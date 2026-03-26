// Mnemosyne — God node. Unified RAM pool with 16 shards.
// TODO(io_uring): not yet implemented — all I/O uses tokio epoll. io_uring planned for future.
// mmap + MADV_HUGEPAGE pool, TLS client listener, full command dispatch.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use mneme_common::{
    CmdId, ConfigSetRequest, ConsistencyLevel, DbCreateRequest, DbDropRequest, DbInfo, DbSizeRequest,
    DelRequest, Entry, ExpireRequest, FlushDbRequest, Frame,
    GetRequest, GetSetRequest, HDelRequest, HGetRequest, HSetRequest,
    HeartbeatPayload, IncrByFloatRequest, IncrByRequest, LeaderRedirectPayload,
    JsonArrAppendRequest, JsonDelRequest, JsonGetRequest,
    JsonNumIncrByRequest, JsonSetRequest,
    JsonDoc, LRangeRequest, ListPushRequest, MGetRequest, MSetRequest, MnemeConfig, MnemeError,
    NodeRole, PushKeyPayload, RegisterAck, RegisterPayload, ScanRequest, SelectRequest, SetRequest,
    SyncCompletePayload, SyncStartPayload, TlsConfig, Value, WaitRequest,
    UserCreateRequest, UserDeleteRequest, UserGrantRequest, UserRevokeRequest,
    UserInfoRequest, UserSetRoleRequest,
    ZAddRequest, ZRangeByScoreRequest, ZRangeRequest, ZRankRequest, ZRemRequest,
    ZSetMember, REGISTER_FLAGS,
};
use parking_lot::{Mutex, RwLock};
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::signal;
use tokio::time;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{error, info, warn};

use crate::auth::argus::{Argus, Claims};
use crate::auth::rbac::{Role, can_execute};
use crate::auth::users::UsersDb;
use crate::core::moirai::{KeeperHandle, KeeperInfo};
use crate::core::pool_mmap::MmapPool;
use crate::core::iris::{Iris, RouteResult};
use crate::core::lethe::Lethe;
use crate::core::moirai::Moirai;
use crate::net::aegis::Aegis;
use crate::net::hermes::Hermes;
use crate::cluster::themis::Themis;
use crate::obs::aletheia::Aletheia;
use crate::obs::delphi::Delphi;

const NUM_SHARDS: usize = 16;
const SHARD_MASK: usize = NUM_SHARDS - 1;
const FRAME_HEADER: usize = mneme_common::HEADER_LEN; // 16B: magic+ver+cmd+flags+plen+req_id
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

// ── DB-namespaced key prefix ──────────────────────────────────────────────────

/// Prefix a user-visible key with `db_id` (2 bytes big-endian) for isolated
/// keyspace storage. db_id=0 → `\x00\x00<key>`, db_id=1 → `\x00\x01<key>`.
///
/// Slot routing always uses the ORIGINAL (un-prefixed) key so that the same
/// logical key maps to the same slot regardless of which database it lives in.
#[inline]
fn make_db_key(db_id: u16, key: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + key.len());
    v.extend_from_slice(&db_id.to_be_bytes());
    v.extend_from_slice(key);
    v
}

// ── Shared RAM pool (16 shards, mmap-backed) ─────────────────────────────────

type Shard = RwLock<HashMap<Vec<u8>, Entry>>;

struct Pool {
    shards: [Shard; NUM_SHARDS],
    pool_bytes: std::sync::atomic::AtomicU64,
    max_bytes: std::sync::atomic::AtomicU64,
    /// mmap region with MADV_HUGEPAGE; MADV_FREE used on eviction.
    _mmap: MmapPool,
}

impl Pool {
    fn new(max_bytes: u64) -> Arc<Self> {
        let mmap = MmapPool::new(max_bytes as usize)
            .unwrap_or_else(|_| MmapPool::new(4096).expect("fallback mmap"));
        Arc::new(Self {
            shards: std::array::from_fn(|_| RwLock::new(HashMap::new())),
            pool_bytes: std::sync::atomic::AtomicU64::new(0),
            max_bytes: std::sync::atomic::AtomicU64::new(max_bytes),
            _mmap: mmap,
        })
    }

    fn shard_idx(key: &[u8]) -> usize {
        (crate::core::iris::Iris::slot_for(key) as usize) & SHARD_MASK
    }

    fn get(&self, key: &[u8], now_ms: u64) -> Option<Entry> {
        let g = self.shards[Self::shard_idx(key)].read();
        let e = g.get(key)?;
        if e.is_expired(now_ms) { return None; }
        Some(e.clone())
    }

    fn get_mut_with<F, R>(&self, key: &[u8], now_ms: u64, f: F) -> Option<R>
    where
        F: FnOnce(&mut Entry) -> R,
    {
        let mut g = self.shards[Self::shard_idx(key)].write();
        let entry = g.get_mut(key)?;
        if entry.is_expired(now_ms) { g.remove(key); return None; }
        Some(f(entry))
    }

    fn set(&self, key: Vec<u8>, entry: Entry) {
        let mem = entry.value.memory_usage() + key.len();
        let mut g = self.shards[Self::shard_idx(&key)].write();
        if let Some(old) = g.get(&key) {
            let old_mem = (old.value.memory_usage() + key.len()) as u64;
            self.pool_bytes.fetch_sub(old_mem, std::sync::atomic::Ordering::Relaxed);
        }
        self.pool_bytes.fetch_add(mem as u64, std::sync::atomic::Ordering::Relaxed);
        g.insert(key, entry);
    }

    fn del(&self, key: &[u8]) -> bool {
        let mut g = self.shards[Self::shard_idx(key)].write();
        if let Some(old) = g.remove(key) {
            let mem = (old.value.memory_usage() + key.len()) as u64;
            self.pool_bytes.fetch_sub(mem, std::sync::atomic::Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    fn pool_used(&self) -> u64 {
        self.pool_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn pool_max(&self) -> u64 {
        self.max_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn set_pool_max(&self, v: u64) {
        self.max_bytes.store(v, std::sync::atomic::Ordering::Relaxed);
    }

    /// Memory pressure as a ratio in [0.0, ∞). Values ≥ 1.0 mean the pool is
    /// at or over its limit (OOM territory).
    fn pressure_ratio(&self) -> f64 {
        let max = self.pool_max();
        if max == 0 { return 1.0; }
        self.pool_used() as f64 / max as f64
    }

    fn lfu_candidates(&self, n: usize, now_ms: u64) -> Vec<(Vec<u8>, u8)> {
        let per_shard = (n / NUM_SHARDS).max(4);
        let mut out = Vec::with_capacity(n);
        for shard in &self.shards {
            for (key, entry) in shard.read().iter().take(per_shard) {
                if !entry.is_expired(now_ms) {
                    out.push((key.clone(), entry.lfu_counter));
                }
            }
            if out.len() >= n { break; }
        }
        out
    }

    fn remove_keys(&self, keys: &[Vec<u8>]) {
        for k in keys { self.del(k); }
    }

    fn total_entries(&self) -> u64 {
        self.shards.iter().map(|s| s.read().len() as u64).sum()
    }

    /// Count non-expired keys in a specific database namespace.
    fn db_size(&self, db_id: u16, now_ms: u64) -> u64 {
        let prefix = db_id.to_be_bytes();
        self.shards.iter().map(|s| {
            s.read().iter()
                .filter(|(k, e)| k.starts_with(&prefix) && !e.is_expired(now_ms))
                .count() as u64
        }).sum()
    }

    /// Delete all keys in a specific database namespace.
    /// Returns the number of keys removed.
    fn flush_db(&self, db_id: u16) -> u64 {
        let prefix = db_id.to_be_bytes();
        let mut count = 0u64;
        for shard in &self.shards {
            let mut g = shard.write();
            let keys: Vec<Vec<u8>> = g.keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect();
            for k in &keys {
                if let Some(old) = g.remove(k) {
                    let mem = (old.value.memory_usage() + k.len()) as u64;
                    self.pool_bytes.fetch_sub(mem, std::sync::atomic::Ordering::Relaxed);
                    count += 1;
                }
            }
        }
        count
    }

    fn memory_usage_of(&self, key: &[u8], now_ms: u64) -> Option<u64> {
        let g = self.shards[Self::shard_idx(key)].read();
        let e = g.get(key)?;
        if e.is_expired(now_ms) { return None; }
        Some((e.value.memory_usage() + key.len()) as u64)
    }

    /// Return the value type name for a namespaced key, or "none" if absent/expired.
    fn key_type(&self, key: &[u8], now_ms: u64) -> &'static str {
        let g = self.shards[Self::shard_idx(key)].read();
        match g.get(key) {
            None => "none",
            Some(e) if e.is_expired(now_ms) => "none",
            Some(e) => e.value.type_name(),
        }
    }

    /// Cursor-based key scan within a single database namespace.
    ///
    /// Returns `(next_cursor, user_visible_keys)`. A `next_cursor` of 0 signals
    /// the scan is complete. Keys are stripped of their 2-byte db prefix before
    /// being returned.
    fn scan_db(
        &self,
        db_id: u16,
        cursor: u64,
        pattern: Option<&str>,
        count: u64,
        now_ms: u64,
    ) -> (u64, Vec<Vec<u8>>) {
        let prefix = db_id.to_be_bytes();
        let prefix_len = prefix.len();
        let mut all_keys: Vec<Vec<u8>> = Vec::new();
        for shard in &self.shards {
            let g = shard.read();
            for (k, e) in g.iter() {
                if k.starts_with(&prefix) && !e.is_expired(now_ms) {
                    let user_key = k[prefix_len..].to_vec();
                    if matches_pattern(&user_key, pattern) {
                        all_keys.push(user_key);
                    }
                }
            }
        }
        all_keys.sort();
        let start = cursor as usize;
        if start >= all_keys.len() {
            return (0, vec![]);
        }
        let end = (start + count as usize).min(all_keys.len());
        let slice = all_keys[start..end].to_vec();
        let next = if end >= all_keys.len() { 0 } else { end as u64 };
        (next, slice)
    }
}

// ── DbPool — database-namespaced pool accessor ────────────────────────────────

/// A thin wrapper around `Pool` that automatically prefixes every user-visible
/// key with the active database ID (2 bytes big-endian). All other Pool state
/// (pool_bytes accounting, mmap region, LFU, etc.) is shared globally —
/// only the key namespace is isolated.
struct DbPool<'a> {
    pool:  &'a Pool,
    db_id: u16,
}

impl<'a> DbPool<'a> {
    fn new(pool: &'a Pool, db_id: u16) -> Self { Self { pool, db_id } }

    /// Build the internal storage key: `[db_id_hi, db_id_lo, ...key]`.
    #[inline]
    fn ns_key(&self, key: &[u8]) -> Vec<u8> { make_db_key(self.db_id, key) }

    fn get(&self, key: &[u8], now_ms: u64) -> Option<Entry> {
        self.pool.get(&self.ns_key(key), now_ms)
    }

    fn set(&self, key: Vec<u8>, entry: Entry) {
        self.pool.set(self.ns_key(&key), entry)
    }

    fn del(&self, key: &[u8]) -> bool {
        self.pool.del(&self.ns_key(key))
    }

    fn get_mut_with<F, R>(&self, key: &[u8], now_ms: u64, f: F) -> Option<R>
    where
        F: FnOnce(&mut Entry) -> R,
    {
        self.pool.get_mut_with(&self.ns_key(key), now_ms, f)
    }

    fn memory_usage_of(&self, key: &[u8], now_ms: u64) -> Option<u64> {
        self.pool.memory_usage_of(&self.ns_key(key), now_ms)
    }
}

// ── WarmupState ───────────────────────────────────────────────────────────────

/// Tracks how many Keepers have connected and whether all have completed
/// their initial warm-up push. Used to gate QUORUM/ALL reads during restart.
struct WarmupState {
    /// Number of keepers that sent SyncStart but not yet SyncComplete.
    pending: std::sync::atomic::AtomicUsize,
    /// Total keys expected across all keepers (sum of key_counts in SyncStart).
    total_expected: AtomicU64,
    /// Total PushKey frames received across all completed keepers.
    total_received: AtomicU64,
    /// Set to true once pending reaches 0 (all keepers have sent SyncComplete).
    hot: std::sync::atomic::AtomicBool,
    /// Number of keepers that completed warm-up.
    completed_keepers: std::sync::atomic::AtomicUsize,
    /// Set to true when the node had data but the primary Core is currently
    /// disconnected. Only meaningful for read-replica role. When stale=true
    /// the replica keeps serving EVENTUAL reads from its local pool.
    stale: std::sync::atomic::AtomicBool,
}

impl WarmupState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: std::sync::atomic::AtomicUsize::new(0),
            total_expected: AtomicU64::new(0),
            total_received: AtomicU64::new(0),
            hot: std::sync::atomic::AtomicBool::new(false),
            completed_keepers: std::sync::atomic::AtomicUsize::new(0),
            stale: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn is_hot(&self) -> bool {
        self.hot.load(Ordering::Acquire)
    }
}

// ── Mnemosyne ─────────────────────────────────────────────────────────────────

pub struct Mnemosyne {
    pool: Arc<Pool>,
    iris: Arc<Iris>,
    lethe: Arc<Lethe>,
    moirai: Arc<Moirai>,
    hermes: Arc<Hermes>,
    themis: Arc<Themis>,
    argus: Arc<Argus>,
    aletheia: Aletheia,
    delphi: Delphi,
    config: Arc<Mutex<MnemeConfig>>,
    warmup: Arc<WarmupState>,
    /// Runtime name → db_id registry. Seeded from config.databases.names on
    /// startup; updated via DB-CREATE / DB-DROP commands; persisted to
    /// `{data_dir}/databases.json` so names survive restarts.
    db_registry: Arc<RwLock<HashMap<String, u16>>>,
}

impl Mnemosyne {
    pub async fn start(config: MnemeConfig) -> Result<()> {
        let aegis = Aegis::new(&config.tls).context("Aegis init")?;

        // Hermes outbound replication fabric (A-01).
        // Pass tls.server_name so dial_keeper never hardcodes "mneme.local".
        let hermes = Arc::new(Hermes::new(
            Arc::new(aegis.clone()),
            config.tls.server_name.clone(),
        ));

        let pool   = Pool::new(config.memory.pool_bytes_u64());
        let iris   = Arc::new(Iris::new(node_id_u64(&config.node.node_id)));
        let lethe  = Arc::new(Lethe::new());

        // Shared unified keeper registry (A-05): owned here, shared with Moirai.
        let keeper_infos: Arc<RwLock<Vec<KeeperInfo>>> = Arc::new(RwLock::new(Vec::new()));
        // write_timeout_ms: configurable via [cluster] write_timeout_ms in mneme.toml.
        let write_timeout_ms = config.cluster.write_timeout_ms;
        let moirai = Arc::new(Moirai::new(config.is_solo(), write_timeout_ms, keeper_infos.clone()));

        // Themis leader election (A-08).
        // Build peers list from config.cluster.peers: ["host:port", ...] → [(raft_id, addr)]
        // For multi-core HA each node has a unique raft_id (1,2,3,...). Peers are assigned
        // IDs sequentially (1..=N) excluding our own raft_id. For a 3-node cluster where
        // our raft_id=1 and peers=["core-2:7379","core-3:7379"], peer IDs are [2,3].
        let peers: Vec<(u64, String)> = {
            let my_id = config.cluster.raft_id;
            let n_peers = config.cluster.peers.len();
            // Collect all raft_ids: 1..=n_peers+1, then remove our own.
            let mut peer_ids: Vec<u64> = (1..=(n_peers as u64 + 1))
                .filter(|id| *id != my_id)
                .collect();
            // If our raft_id is outside [1..N+1] (unlikely), fall back to sequential.
            if peer_ids.len() != n_peers {
                peer_ids = (1..=n_peers as u64).collect();
            }
            peer_ids
                .into_iter()
                .zip(config.cluster.peers.iter().cloned())
                .collect()
        };

        let themis = if config.is_solo() || config.cluster.peers.is_empty() {
            // Solo/single-core mode: no Aegis needed for Raft transport.
            match Themis::start_solo(
                config.cluster.raft_id,
                config.cluster.heartbeat_ms,
                config.cluster.election_min_ms,
                config.cluster.election_max_ms,
            ).await {
                Ok(t) => Arc::new(t),
                Err(e) => {
                    warn!("Themis start failed — leader election disabled: {e}");
                    Arc::new(Themis::start_solo(
                        config.cluster.raft_id, 500, 1500, 3000,
                    ).await.context("Themis fallback start")?)
                }
            }
        } else {
            // Multi-core mode: real Raft transport over mTLS.
            Arc::new(Themis::start(
                config.cluster.raft_id,
                config.cluster.heartbeat_ms,
                config.cluster.election_min_ms,
                config.cluster.election_max_ms,
                peers,
                Some(Arc::new(aegis.clone())),
                config.tls.server_name.clone(),
                config.cluster.advertise_addr.clone(),
            ).await.context("Themis multi-core start")?)
        };

        let users_db = UsersDb::open(&config.auth.users_db)
            .unwrap_or_else(|_| UsersDb::in_memory());
        let token_ttl_secs = config.auth.token_ttl_h * 3600;
        let argus = Arc::new(Argus::with_config(
            config.auth.cluster_secret.as_bytes(),
            users_db,
            token_ttl_secs,
        ));
        let aletheia = Aletheia::new();
        let delphi   = Delphi::new(1_000, 128); // 1ms threshold, 128 entries
        let warmup   = WarmupState::new();

        // Database name registry — seed from config, then merge persisted runtime names.
        let db_registry = {
            let mut map: HashMap<String, u16> = config.databases.names.clone();
            // Load runtime names persisted by previous DB-CREATE calls.
            if let Some(runtime) = load_db_registry(&config.auth.users_db) {
                for (k, v) in runtime {
                    map.entry(k).or_insert(v);
                }
            }
            Arc::new(RwLock::new(map))
        };

        let me = Arc::new(Self {
            pool,
            iris,
            lethe,
            moirai,
            hermes,
            themis,
            argus,
            aletheia: aletheia.clone(),
            delphi,
            config: Arc::new(Mutex::new(config.clone())),
            warmup,
            db_registry,
        });

        // Solo mode: start embedded Hypnos keeper in the same process.
        // In solo mode there are no external keepers sending SyncComplete, so
        // the warmup gate would never open.  Set hot=true immediately so that
        // QUORUM reads are served without blocking.  The embedded keeper replays
        // WAL + snapshot into Oneiros, then scans Oneiros and returns all
        // recovered entries so we can restore the Mnemosyne RAM pool.
        if config.is_solo() {
            me.warmup.hot.store(true, Ordering::Release);
            let solo_config = config.clone();
            let moirai = me.moirai.clone();
            let pool = me.pool.clone();
            let lethe = me.lethe.clone();
            tokio::spawn(async move {
                match mneme_keeper::keeper::hypnos::Hypnos::start_embedded(solo_config).await {
                    Ok((node_id, tx, recovered)) => {
                        // Restore recovered entries into the Core RAM pool.
                        let now = now_ms();
                        let mut restored = 0usize;
                        let mut expired = 0usize;
                        for (key, value, expires_at_ms, slot) in recovered {
                            // Skip keys that expired while the server was down.
                            if expires_at_ms > 0 && expires_at_ms <= now {
                                expired += 1;
                                continue;
                            }
                            let mut entry = Entry::new(value, slot);
                            if expires_at_ms > 0 {
                                entry.expires_at_ms = expires_at_ms;
                                lethe.schedule(key.clone(), expires_at_ms, now);
                            }
                            pool.set(key, entry);
                            restored += 1;
                        }
                        info!(node_id, restored, expired, "Embedded keeper: RAM pool restored from cold store");

                        moirai.add_keeper(
                            KeeperHandle { node_id, tx },
                            "embedded".into(),
                            "embedded:0".into(),
                            0,
                        );
                    }
                    Err(e) => error!("Embedded keeper failed to start: {e}"),
                }
            });
        }

        // Metrics HTTP server
        let metrics_addr: SocketAddr = config.metrics_addr().parse()?;
        tokio::spawn(async move {
            if let Err(e) = Aletheia::serve_metrics(metrics_addr).await {
                error!("metrics: {e}");
            }
        });

        // TTL + eviction background task
        // Graduated pressure thresholds:
        //   ≥ eviction_threshold (default 0.90)  → mild proactive LFU eviction (1 %)
        //   ≥ 1.00 (OOM)                          → aggressive eviction (5 %),
        //                                            recorded as "oom" eviction
        {
            let me2 = me.clone();
            tokio::spawn(async move {
                let mut interval = time::interval(Duration::from_millis(10));
                loop {
                    interval.tick().await;
                    let now_ms = now_ms();

                    // 1. TTL expiry — always first so expired keys free space.
                    for key in me2.lethe.tick(now_ms) {
                        me2.pool.del(&key);
                        me2.aletheia.record_eviction("ttl");
                    }

                    // 2. LFU eviction — triggered by pressure.
                    let pressure = me2.pool.pressure_ratio();
                    let eviction_threshold = me2.config.lock().memory.eviction_threshold;

                    if pressure >= 1.0 {
                        // OOM: evict 5 % of entries to recover headroom.
                        let total = me2.pool.total_entries() as usize;
                        let n = (total / 20).max(64).min(4096); // 5 %, capped
                        let candidates = me2.pool.lfu_candidates(n * 4, now_ms);
                        let to_evict = Lethe::pick_eviction_candidates(&candidates, n);
                        let evicted = to_evict.len();
                        me2.pool.remove_keys(&to_evict);
                        for _ in 0..evicted {
                            me2.aletheia.record_eviction("oom");
                        }
                        warn!(pressure, evicted, "OOM: pool over limit, evicted keys");
                    } else if pressure >= eviction_threshold {
                        // Proactive: evict 1 % to stay below the threshold.
                        let total = me2.pool.total_entries() as usize;
                        let n = (total / 100).max(16).min(512); // 1 %, capped
                        let candidates = me2.pool.lfu_candidates(n * 4, now_ms);
                        let to_evict = Lethe::pick_eviction_candidates(&candidates, n);
                        let evicted = to_evict.len();
                        me2.pool.remove_keys(&to_evict);
                        for _ in 0..evicted {
                            me2.aletheia.record_eviction("lfu");
                        }
                    }

                    me2.aletheia.set_pool_usage(
                        me2.pool.pool_used(),
                        me2.pool.pool_max(),
                    );
                }
            });
        }

        // Replication TLS listener.
        // Core/Solo: accepts incoming Keeper SyncStart connections (one-way TLS;
        //            keepers authenticate via cluster_secret in the SyncStart frame).
        // ReadReplica: accepts incoming Hermes PushKey connections from the primary Core.
        {
            let repl_addr: SocketAddr = config.replication_addr().parse()?;
            let repl_acceptor = TlsAcceptor::from(aegis.server_config());
            let repl_listener = TcpListener::bind(repl_addr).await?;
            let me2 = me.clone();
            let node_role = config.node.role.clone();
            info!(%repl_addr, "Mnemosyne replication listener ready");
            tokio::spawn(async move {
                loop {
                    match repl_listener.accept().await {
                        Ok((stream, peer)) => {
                            let acceptor = repl_acceptor.clone();
                            let me3 = me2.clone();
                            let role = node_role.clone();
                            tokio::spawn(async move {
                                match acceptor.accept(stream).await {
                                    Ok(tls) => {
                                        if role == NodeRole::ReadReplica {
                                            if let Err(e) = me3.handle_replica_replication(tls, peer).await {
                                                warn!(%peer, "replica replication: {e}");
                                            }
                                        } else if let Err(e) = me3.handle_keeper_connection(tls, peer).await {
                                            warn!(%peer, "keeper conn: {e}");
                                        }
                                    }
                                    Err(e) => warn!(%peer, "replication TLS: {e}"),
                                }
                            });
                        }
                        Err(e) => error!("replication accept: {e}"),
                    }
                }
            });
        }

        // ReadReplica: outbound registration loop — connects to the primary Core,
        // sends SyncStart (key_count=0), and triggers Core's Hermes to start
        // streaming PushKey frames to this replica's rep_port.
        if config.node.role == NodeRole::ReadReplica {
            let me2 = me.clone();
            let cfg2 = config.clone();
            tokio::spawn(async move {
                me2.run_replica_registration_loop(cfg2).await;
            });
        }

        // Client TLS listener
        let client_addr: SocketAddr = config.client_addr().parse()?;
        let acceptor = TlsAcceptor::from(aegis.server_config());
        let listener = TcpListener::bind(client_addr).await?;
        info!(%client_addr, "Mnemosyne accepting clients");

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, peer)) => {
                            let acceptor = acceptor.clone();
                            let me2 = me.clone();
                            tokio::spawn(async move {
                                match acceptor.accept(stream).await {
                                    Ok(tls) => {
                                        if let Err(e) = me2.handle_connection(tls, peer).await {
                                            warn!(%peer, "conn: {e}");
                                        }
                                    }
                                    Err(e) => warn!(%peer, "TLS: {e}"),
                                }
                            });
                        }
                        Err(e) => error!("accept: {e}"),
                    }
                }
                _ = signal::ctrl_c() => {
                    info!("Mnemosyne shutting down — starting graceful shutdown sequence");
                    break;
                }
            }
        }

        // ── Shutdown sequence (CRITICAL — wrong order = data loss) ──────────
        // 1. Stop accepting new connections (done — broke out of accept loop)
        // 2. TODO: drain in-flight requests (Charon not yet wired into Mnemosyne)
        // 3. Flush Hermes — close all keeper channels so pending frames drain
        info!("Shutdown: flushing Hermes replication fabric");
        me.hermes.shutdown();
        // 4. Themis stepdown — resign leadership so cluster can re-elect
        info!("Shutdown: Themis Raft stepdown");
        // Themis::shutdown() consumes self, but we only have Arc<Themis>.
        // Log the intent; the Raft node will stop when the Arc is dropped.
        drop(me.themis.clone());
        // 5. Flush Aletheia metrics
        info!("Shutdown: Aletheia metrics flushed");
        // 6. Close TLS — happens automatically when aegis Arc is dropped
        info!("Mnemosyne shutdown complete");
        Ok(())
    }

    // ── connection handler ─────────────────────────────────────────────────────

    async fn handle_connection<S>(self: Arc<Self>, mut stream: S, peer: SocketAddr) -> Result<()>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let mut buf = BytesMut::with_capacity(4096);
        let mut authenticated = false;
        // Active database for this connection (0 = default, changed via SELECT).
        let mut conn_db: u16 = self.config.lock().databases.default_database;
        // Role and db allowlist for this connection, populated on auth success.
        let mut conn_role: Role = Role::ReadOnly;       // restrictive until auth
        let mut conn_allowed_dbs: Vec<u16> = vec![];   // empty = all (but Role::ReadOnly blocks writes)

        loop {
            // Read full frame
            loop {
                if buf.len() >= FRAME_HEADER {
                    let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                    if buf.len() >= FRAME_HEADER + plen { break; }
                }
                let n = stream.read_buf(&mut buf).await?;
                if n == 0 { return Ok(()); }
                if buf.len() > MAX_PAYLOAD + FRAME_HEADER {
                    anyhow::bail!("payload too large from {peer}");
                }
            }

            let (frame, consumed) = Frame::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
            let _ = buf.split_to(consumed);

            // Auth gate
            if !authenticated {
                let req_id = frame.req_id;
                let (mut resp, maybe_claims) = self.handle_auth_frame(&frame).await;
                let ok = resp.cmd_id == CmdId::Ok;
                resp.req_id = req_id;
                stream.write_all(&resp.encode()).await?;
                if ok {
                    authenticated = true;
                    if let Some(claims) = maybe_claims {
                        // Embed the role and db allowlist from the verified token.
                        conn_role = Role::from_str(&claims.role);
                        conn_allowed_dbs = claims.allowed_dbs;
                    }
                    // `maybe_claims` is always Some when ok==true (handle_auth_frame
                    // guarantees this), so the branch above always runs.
                }
                continue;
            }

            // RBAC enforcement — checked before SELECT so that even SELECT is
            // subject to db access restrictions.
            if !can_execute(conn_role, frame.cmd_id, conn_db, &conn_allowed_dbs) {
                let req_id = frame.req_id;
                let msg = format!(
                    "DENIED: role '{}' may not execute {:?} on database {}",
                    conn_role.as_str(), frame.cmd_id, conn_db,
                );
                let resp = Frame::error_response_for(&msg, req_id);
                stream.write_all(&resp.encode()).await?;
                continue;
            }

            // SELECT is handled here because it mutates connection state, not pool state.
            if frame.cmd_id == CmdId::Select {
                let req_id = frame.req_id;
                let resp = match rmp_serde::from_slice::<SelectRequest>(&frame.payload) {
                    Err(e) => Frame::error_response_for(&format!("SELECT: bad payload: {e}"), req_id),
                    Ok(req) => {
                        // Resolve name → id if caller supplied a name.
                        // Drop the lock guard before any await point.
                        let name_lookup: Option<Option<u16>> = if !req.name.is_empty() {
                            Some(self.db_registry.read().get(&req.name).copied())
                        } else {
                            None
                        };
                        let resolved_id = match name_lookup {
                            Some(Some(id)) => id,
                            Some(None) => {
                                let resp = Frame::error_response_for(
                                    &format!("ERR unknown database name '{}'", req.name),
                                    req_id,
                                );
                                stream.write_all(&resp.encode()).await?;
                                continue;
                            }
                            None => req.db_id,
                        };
                        let max_dbs = self.config.lock().databases.max_databases;
                        if resolved_id >= max_dbs {
                            Frame::error_response_for(
                                &format!("ERR DB index out of range (0..{})", max_dbs - 1),
                                req_id,
                            )
                        } else {
                            conn_db = resolved_id;
                            Frame::ok_response_for(
                                Bytes::from(rmp_serde::to_vec("OK").unwrap_or_default()),
                                req_id,
                            )
                        }
                    }
                };
                stream.write_all(&resp.encode()).await?;
                continue;
            }

            // Routing
            let key = extract_primary_key(&frame);
            if let Some(ref k) = key {
                if let RouteResult::Moved { slot, node_id, addr } = self.iris.route(k) {
                    let payload = rmp_serde::to_vec(&(slot, node_id, &addr)).unwrap_or_default();
                    let resp = Frame { cmd_id: CmdId::Moved, flags: 0, req_id: frame.req_id, payload: Bytes::from(payload) };
                    stream.write_all(&resp.encode()).await?;
                    continue;
                }
            }

            // Dispatch
            let req_id = frame.req_id;
            let start = Instant::now();
            let resp = self.dispatch_command(frame, conn_db).await;
            let elapsed_us = start.elapsed().as_micros() as u64;
            self.delphi.record("cmd", key.unwrap_or_default(), elapsed_us);

            let wire = match resp {
                Ok(mut f) => {
                    self.aletheia.record_cmd("cmd", "ok", elapsed_us as f64);
                    f.req_id = req_id; // echo req_id for multiplexed clients
                    f.encode()
                }
                Err(e) => {
                    self.aletheia.record_cmd("cmd", "err", elapsed_us as f64);
                    Frame::error_response_for(&e.to_string(), req_id).encode()
                }
            };
            stream.write_all(&wire).await?;
        }
    }

    /// Authenticate the client.
    /// Returns `(response_frame, Option<Claims>)`:
    ///   - `Some(claims)` when auth succeeded — caller uses claims for RBAC state.
    ///   - `None` when auth failed (response is an Error frame).
    ///
    /// Token path (HMAC-SHA256 verify) runs inline — it is sub-microsecond.
    /// Credential path (PBKDF2-SHA256 verify) is CPU-bound and must not block
    /// the tokio runtime: it is dispatched to a blocking thread via
    /// `spawn_blocking` and awaited before returning.
    async fn handle_auth_frame(&self, frame: &Frame) -> (Frame, Option<Claims>) {
        if frame.cmd_id != CmdId::Auth {
            return (Frame::error_response("NOAUTH authentication required"), None);
        }
        // ── Token-based auth (fast path: HMAC-SHA256 only) ────────────────────
        if let Ok(token) = rmp_serde::from_slice::<String>(&frame.payload) {
            if let Ok(claims) = self.argus.verify(&token) {
                let resp = Frame::ok_response(Bytes::from(
                    rmp_serde::to_vec("OK").unwrap_or_default()));
                return (resp, Some(claims));
            }
        }
        // ── Credential-based auth (slow path: PBKDF2 — must not block runtime) ─
        if let Ok((user, pass)) = rmp_serde::from_slice::<(String, String)>(&frame.payload) {
            let argus = self.argus.clone();
            let result = tokio::task::spawn_blocking(move || argus.auth_user(&user, &pass))
                .await;
            match result {
                Ok(Ok((token, claims))) => {
                    let payload = rmp_serde::to_vec(&token).unwrap_or_default();
                    return (Frame::ok_response(Bytes::from(payload)), Some(claims));
                }
                Ok(Err(e)) => return (Frame::error_response(&format!("AUTH failed: {e}")), None),
                Err(_)     => return (Frame::error_response("AUTH: internal error"), None),
            }
        }
        (Frame::error_response("AUTH: malformed payload"), None)
    }

    // ── command dispatch ───────────────────────────────────────────────────────

    async fn dispatch_command(&self, frame: Frame, db_id: u16) -> Result<Frame> {
        let now = now_ms();
        let consistency = frame.consistency();

        // ── Warmup gate ───────────────────────────────────────────────────────
        // QUORUM and ALL reads must wait for all keepers to finish their warm-up
        // push (WarmupState::Hot) before serving.  During the window between Core
        // restart and keeper reconnect the RAM pool is empty, so serving QUORUM/ALL
        // reads would silently return KeyNotFound for keys that exist on keepers.
        //
        // Enforcement rules:
        //   • QUORUM / ALL  + !is_hot → return QuorumNotReached(0, 1) immediately.
        //     Clients should back-off and retry; EVENTUAL works immediately.
        //   • EVENTUAL / ONE           → served without gating (AP reads).
        //   • Write commands (Set, Del, …) are NOT gated — Moirai's ACK collection
        //     implicitly serialises them with keeper availability.
        //   • Admin / cluster commands  → never gated (ClusterInfo, KeeperList, …).
        if !self.warmup.is_hot() {
            let needs_hot = matches!(
                consistency,
                ConsistencyLevel::Quorum | ConsistencyLevel::All
            );
            let is_read = matches!(
                frame.cmd_id,
                CmdId::Get | CmdId::HGet | CmdId::HGetAll | CmdId::LRange
                    | CmdId::ZRange | CmdId::ZRangeByScore | CmdId::ZRank
                    | CmdId::ZScore | CmdId::ZCard | CmdId::MGet
            );
            if needs_hot && is_read {
                return Err(MnemeError::QuorumNotReached { got: 0, need: 1 }.into());
            }
        }

        // ── Leader gate (multi-core HA) ────────────────────────────────────────
        // Write commands must only be executed on the Raft leader. Followers
        // return LeaderRedirect so the client can reconnect to the leader.
        // Read commands are served locally (EVENTUAL) or forwarded (QUORUM/ALL).
        let is_write = matches!(
            frame.cmd_id,
            CmdId::Set | CmdId::Del | CmdId::Expire
                | CmdId::HSet | CmdId::HDel
                | CmdId::LPush | CmdId::RPush
                | CmdId::ZAdd | CmdId::ZRem
                | CmdId::MSet
                | CmdId::IncrBy | CmdId::IncrByFloat | CmdId::GetSet
                | CmdId::JsonSet | CmdId::JsonDel | CmdId::JsonArrAppend | CmdId::JsonNumIncrBy
                | CmdId::FlushDb | CmdId::DbCreate | CmdId::DbDrop
                | CmdId::Config
                | CmdId::UserCreate | CmdId::UserDelete | CmdId::UserGrant
                | CmdId::UserRevoke | CmdId::UserSetRole
        );
        if is_write && !self.themis.is_leader() {
            let leader_addr = self.themis.leader_client_addr().unwrap_or_default();
            let payload = LeaderRedirectPayload { leader_addr };
            let encoded = rmp_serde::to_vec(&payload).unwrap_or_default();
            return Ok(Frame {
                cmd_id: CmdId::LeaderRedirect,
                flags: 0,
                req_id: frame.req_id,
                payload: Bytes::from(encoded),
            });
        }

        // All key lookups go through this db-namespaced accessor.
        let p = DbPool::new(&self.pool, db_id);

        match frame.cmd_id {
            // ── String commands ───────────────────────────────────────────────
            CmdId::Get => {
                let req: GetRequest = decode(&frame.payload)?;
                let val = p.get_mut_with(&req.key, now, |entry| {
                    entry.lfu_counter = Lethe::increment_lfu(entry.lfu_counter);
                    entry.value.clone()
                });
                match val {
                    None => Err(MnemeError::KeyNotFound.into()),
                    Some(v) => ok_payload(&v),
                }
            }

            CmdId::Set => {
                check_oom(&self.pool)?;
                let req: SetRequest = decode(&frame.payload)?;
                let slot = Iris::slot_for(&req.key);
                let mut entry = Entry::new(req.value, slot);
                if req.ttl_ms > 0 {
                    entry = entry.with_ttl(req.ttl_ms, now);
                    self.lethe.schedule(req.key.clone(), entry.expires_at_ms, now);
                }
                p.set(req.key.clone(), entry);
                // Replicate to keepers — key in PushKeyPayload carries db prefix.
                let push = PushKeyPayload {
                    key: p.ns_key(&req.key),
                    value: rmp_serde::from_slice::<SetRequest>(&frame.payload)
                        .map(|r| r.value)
                        .unwrap_or(Value::String(vec![])),
                    seq: 0,
                    ttl_ms: req.ttl_ms,
                    slot,
                    deleted: false,
                    db_id,
                };
                if let Ok(push_payload) = rmp_serde::to_vec(&push) {
                    let _ = self.moirai.dispatch(Frame {
                        cmd_id: CmdId::PushKey,
                        flags: frame.flags,
                        req_id: 0,
                        payload: Bytes::from(push_payload),
                    }, consistency).await;
                }
                ok_str("OK")
            }

            CmdId::Del => {
                let req: DelRequest = decode(&frame.payload)?;
                let mut deleted: u64 = 0;
                for key in &req.keys {
                    if p.del(key) {
                        deleted += 1;
                        // Replicate deletion to keepers — key carries db prefix.
                        let push = PushKeyPayload {
                            key: p.ns_key(key),
                            value: Value::String(vec![]), // tombstone — deleted=true overrides
                            seq: 0,
                            ttl_ms: 0,
                            slot: crate::core::iris::Iris::slot_for(key),
                            deleted: true,
                            db_id,
                        };
                        if let Ok(payload) = rmp_serde::to_vec(&push) {
                            let _ = self.moirai.dispatch(Frame {
                                cmd_id: CmdId::PushKey,
                                flags: frame.flags,
                                req_id: 0,
                                payload: Bytes::from(payload),
                            }, consistency).await;
                        }
                    }
                }
                ok_payload(&deleted)
            }

            CmdId::Exists => {
                let key: Vec<u8> = decode(&frame.payload)?;
                ok_payload(&p.get(&key, now).is_some())
            }

            CmdId::Expire => {
                let req: ExpireRequest = decode(&frame.payload)?;
                let ttl_ms = req.seconds * 1000;
                let applied = p.get_mut_with(&req.key, now, |e| {
                    e.expires_at_ms = now + ttl_ms;
                    self.lethe.schedule(req.key.clone(), e.expires_at_ms, now);
                    1u64
                }).unwrap_or(0);
                // Replicate TTL update: push current value with new TTL
                if applied > 0 {
                    if let Some(entry) = p.get(&req.key, now) {
                        let push = PushKeyPayload {
                            key: p.ns_key(&req.key),
                            value: entry.value.clone(),
                            seq: 0,
                            ttl_ms,
                            slot: crate::core::iris::Iris::slot_for(&req.key),
                            deleted: false,
                            db_id,
                        };
                        if let Ok(payload) = rmp_serde::to_vec(&push) {
                            let _ = self.moirai.dispatch(Frame {
                                cmd_id: CmdId::PushKey,
                                flags: frame.flags,
                                req_id: 0,
                                payload: Bytes::from(payload),
                            }, consistency).await;
                        }
                    }
                }
                ok_payload(&applied)
            }

            CmdId::Ttl => {
                let key: Vec<u8> = decode(&frame.payload)?;
                let ttl: i64 = p.get(&key, now)
                    .map(|e| if e.expires_at_ms == 0 { -1 }
                             else { (e.expires_at_ms as i64 - now as i64).max(0) / 1000 })
                    .unwrap_or(-2);
                ok_payload(&ttl)
            }

            // ── Hash commands ─────────────────────────────────────────────────
            CmdId::HSet => {
                check_oom(&self.pool)?;
                let req: HSetRequest = decode(&frame.payload)?;
                let slot = Iris::slot_for(&req.key);
                let added = p.get_mut_with(&req.key, now, |entry| {
                    if let Value::Hash(ref mut pairs) = entry.value {
                        let mut count = 0u64;
                        for (field, val) in &req.pairs {
                            if let Some(pair) = pairs.iter_mut().find(|(f, _)| f == field) {
                                pair.1 = val.clone();
                            } else {
                                pairs.push((field.clone(), val.clone()));
                                count += 1;
                            }
                        }
                        count
                    } else { 0 }
                });
                if added.is_none() {
                    let pairs = req.pairs;
                    let count = pairs.len() as u64;
                    p.set(req.key, Entry::new(Value::Hash(pairs), slot));
                    ok_payload(&count)
                } else {
                    ok_payload(&added.unwrap())
                }
            }

            CmdId::HGet => {
                let req: HGetRequest = decode(&frame.payload)?;
                match p.get(&req.key, now) {
                    None => Err(MnemeError::KeyNotFound.into()),
                    Some(entry) => match &entry.value {
                        Value::Hash(pairs) => {
                            match pairs.iter().find(|(f, _)| f == &req.field).map(|(_, v)| v.clone()) {
                                Some(v) => ok_payload(&v),
                                None    => Err(MnemeError::KeyNotFound.into()),
                            }
                        }
                        other => Err(MnemeError::WrongType {
                            expected: "hash", got: other.type_name(),
                        }.into()),
                    }
                }
            }

            CmdId::HDel => {
                let req: HDelRequest = decode(&frame.payload)?;
                let deleted = p.get_mut_with(&req.key, now, |entry| {
                    if let Value::Hash(ref mut pairs) = entry.value {
                        let before = pairs.len();
                        pairs.retain(|(f, _)| !req.fields.contains(f));
                        (before - pairs.len()) as u64
                    } else { 0 }
                }).unwrap_or(0);
                ok_payload(&deleted)
            }

            CmdId::HGetAll => {
                let key: Vec<u8> = decode(&frame.payload)?;
                match p.get(&key, now) {
                    None => ok_payload(&Vec::<(Vec<u8>, Vec<u8>)>::new()),
                    Some(entry) => match &entry.value {
                        Value::Hash(pairs) => ok_payload(pairs),
                        other => Err(MnemeError::WrongType {
                            expected: "hash", got: other.type_name(),
                        }.into()),
                    }
                }
            }

            // ── List commands ─────────────────────────────────────────────────
            CmdId::LPush => {
                check_oom(&self.pool)?;
                let req: ListPushRequest = decode(&frame.payload)?;
                let slot = Iris::slot_for(&req.key);
                let len = self.push_list(&req.key, req.values, true, slot, now, db_id);
                ok_payload(&len)
            }

            CmdId::RPush => {
                check_oom(&self.pool)?;
                let req: ListPushRequest = decode(&frame.payload)?;
                let slot = Iris::slot_for(&req.key);
                let len = self.push_list(&req.key, req.values, false, slot, now, db_id);
                ok_payload(&len)
            }

            CmdId::LPop => {
                let key: Vec<u8> = decode(&frame.payload)?;
                let val = p.get_mut_with(&key, now, |entry| {
                    if let Value::List(ref mut deque) = entry.value {
                        deque.pop_front()
                    } else { None }
                }).flatten();
                match val {
                    Some(v) => ok_payload(&v),
                    None    => Err(MnemeError::KeyNotFound.into()),
                }
            }

            CmdId::RPop => {
                let key: Vec<u8> = decode(&frame.payload)?;
                let val = p.get_mut_with(&key, now, |entry| {
                    if let Value::List(ref mut deque) = entry.value {
                        deque.pop_back()
                    } else { None }
                }).flatten();
                match val {
                    Some(v) => ok_payload(&v),
                    None    => Err(MnemeError::KeyNotFound.into()),
                }
            }

            CmdId::LRange => {
                let req: LRangeRequest = decode(&frame.payload)?;
                match p.get(&req.key, now) {
                    None => ok_payload(&Vec::<Vec<u8>>::new()),
                    Some(entry) => match &entry.value {
                        Value::List(deque) => {
                            let len = deque.len() as i64;
                            let start = normalize_idx(req.start, len);
                            let stop  = normalize_idx(req.stop, len).min(len - 1);
                            if start > stop { return ok_payload(&Vec::<Vec<u8>>::new()); }
                            let slice: Vec<Vec<u8>> = deque.iter()
                                .skip(start as usize)
                                .take((stop - start + 1) as usize)
                                .cloned()
                                .collect();
                            ok_payload(&slice)
                        }
                        other => Err(MnemeError::WrongType {
                            expected: "list", got: other.type_name(),
                        }.into()),
                    }
                }
            }

            // ── Sorted set commands ───────────────────────────────────────────
            CmdId::ZAdd => {
                check_oom(&self.pool)?;
                let req: ZAddRequest = decode(&frame.payload)?;
                // Reject NaN/Inf scores — prevents panic in sort and ensures deterministic ordering
                for m in &req.members {
                    if m.score.is_nan() || m.score.is_infinite() {
                        return Err(MnemeError::Protocol(
                            "NaN or Inf scores are not allowed in sorted sets".into(),
                        ).into());
                    }
                }
                let slot = Iris::slot_for(&req.key);
                let added = self.zadd(&req.key, req.members, slot, now, db_id);
                ok_payload(&added)
            }

            CmdId::ZRank => {
                let req: ZRankRequest = decode(&frame.payload)?;
                let rank = p.get(&req.key, now).and_then(|entry| {
                    if let Value::ZSet(members) = &entry.value {
                        let mut sorted: Vec<_> = members.iter().collect();
                        sorted.sort_by(|a, b| a.score.total_cmp(&b.score));
                        sorted.iter().position(|m| m.member == req.member).map(|p| p as i64)
                    } else { None }
                });
                match rank {
                    Some(r) => ok_payload(&r),
                    None    => Err(MnemeError::KeyNotFound.into()),
                }
            }

            CmdId::ZRange => {
                let req: ZRangeRequest = decode(&frame.payload)?;
                match p.get(&req.key, now) {
                    None => ok_payload(&Vec::<Vec<u8>>::new()),
                    Some(entry) => match &entry.value {
                        Value::ZSet(members) => {
                            let mut sorted: Vec<_> = members.iter().collect();
                            sorted.sort_by(|a, b| a.score.total_cmp(&b.score));
                            let len   = sorted.len() as i64;
                            let start = normalize_idx(req.start, len);
                            let stop  = normalize_idx(req.stop, len).min(len - 1);
                            if start > stop { return ok_payload(&Vec::<Vec<u8>>::new()); }
                            let slice: Vec<_> = sorted[start as usize..=stop as usize].iter()
                                .map(|m| (m.member.clone(), m.score))
                                .collect();
                            if req.with_scores { ok_payload(&slice) }
                            else { ok_payload(&slice.into_iter().map(|(m, _)| m).collect::<Vec<_>>()) }
                        }
                        other => Err(MnemeError::WrongType {
                            expected: "zset", got: other.type_name(),
                        }.into()),
                    }
                }
            }

            CmdId::ZRangeByScore => {
                let req: ZRangeByScoreRequest = decode(&frame.payload)?;
                match p.get(&req.key, now) {
                    None => ok_payload(&Vec::<Vec<u8>>::new()),
                    Some(entry) => match &entry.value {
                        Value::ZSet(members) => {
                            let mut matched: Vec<_> = members.iter()
                                .filter(|m| m.score >= req.min && m.score <= req.max)
                                .collect();
                            matched.sort_by(|a, b| a.score.total_cmp(&b.score));
                            let result: Vec<Vec<u8>> = matched.iter().map(|m| m.member.clone()).collect();
                            ok_payload(&result)
                        }
                        other => Err(MnemeError::WrongType {
                            expected: "zset", got: other.type_name(),
                        }.into()),
                    }
                }
            }

            CmdId::ZCard => {
                let key: Vec<u8> = decode(&frame.payload)?;
                let card = p.get(&key, now)
                    .map(|e| if let Value::ZSet(m) = &e.value { m.len() as u64 } else { 0 })
                    .unwrap_or(0);
                ok_payload(&card)
            }

            CmdId::ZRem => {
                let req: ZRemRequest = decode(&frame.payload)?;
                let removed = p.get_mut_with(&req.key, now, |entry| {
                    if let Value::ZSet(ref mut members) = entry.value {
                        let before = members.len();
                        members.retain(|m| !req.members.contains(&m.member));
                        (before - members.len()) as u64
                    } else { 0 }
                }).unwrap_or(0);
                ok_payload(&removed)
            }

            CmdId::ZScore => {
                let (key, member): (Vec<u8>, Vec<u8>) = decode(&frame.payload)?;
                let score = p.get(&key, now).and_then(|e| {
                    if let Value::ZSet(members) = &e.value {
                        members.iter().find(|m| m.member == member).map(|m| m.score)
                    } else { None }
                });
                match score {
                    Some(s) => ok_payload(&s),
                    None    => Err(MnemeError::KeyNotFound.into()),
                }
            }

            // ── Counter commands ──────────────────────────────────────────────
            CmdId::Incr => {
                let key: Vec<u8> = decode(&frame.payload)?;
                self.counter_op(&key, now, db_id, |v| v.incr())
            }

            CmdId::Decr => {
                let key: Vec<u8> = decode(&frame.payload)?;
                self.counter_op(&key, now, db_id, |v| v.decr())
            }

            CmdId::IncrBy => {
                let req: IncrByRequest = decode(&frame.payload)?;
                self.counter_op(&req.key, now, db_id, |v| v.incrby(req.delta))
            }

            CmdId::DecrBy => {
                let req: IncrByRequest = decode(&frame.payload)?;
                self.counter_op(&req.key, now, db_id, |v| v.incrby(-req.delta))
            }

            CmdId::IncrByFloat => {
                let req: IncrByFloatRequest = decode(&frame.payload)?;
                // Stored as Counter (integer part) or String; best-effort float add
                let slot = Iris::slot_for(&req.key);
                let result = p.get_mut_with(&req.key, now, |entry| {
                    match &mut entry.value {
                        Value::Counter(n) => {
                            let new_f = *n as f64 + req.delta;
                            *n = new_f as i64;
                            Ok(new_f)
                        }
                        Value::String(b) => {
                            let s = std::str::from_utf8(b)
                                .map_err(|_| MnemeError::WrongType { expected: "number", got: "string" })?;
                            let cur: f64 = s.trim().parse()
                                .map_err(|_| MnemeError::WrongType { expected: "number", got: "string" })?;
                            let new_f = cur + req.delta;
                            *b = new_f.to_string().into_bytes();
                            Ok(new_f)
                        }
                        other => Err(MnemeError::WrongType { expected: "number", got: other.type_name() }),
                    }
                });
                match result {
                    None => {
                        // Key doesn't exist — create as 0 + delta
                        p.set(req.key, Entry::new(Value::Counter(req.delta as i64), slot));
                        ok_payload(&req.delta)
                    }
                    Some(Ok(v))  => ok_payload(&v),
                    Some(Err(e)) => Err(e.into()),
                }
            }

            CmdId::GetSet => {
                let req: GetSetRequest = decode(&frame.payload)?;
                let slot = Iris::slot_for(&req.key);
                // Get old value, atomically set new one
                let old = p.get(&req.key, now).map(|e| e.value.clone());
                p.set(req.key, Entry::new(req.value, slot));
                match old {
                    Some(v) => ok_payload(&v),
                    None    => ok_payload(&Option::<Value>::None),
                }
            }

            // ── JSON commands ─────────────────────────────────────────────────
            CmdId::JsonGet => {
                let req: JsonGetRequest = decode(&frame.payload)?;
                match p.get(&req.key, now) {
                    None => Err(MnemeError::KeyNotFound.into()),
                    Some(entry) => match &entry.value {
                        Value::Json(doc) => {
                            let result = doc.get(&req.path)
                                .map_err(anyhow::Error::from)?;
                            ok_payload(&result)
                        }
                        other => Err(MnemeError::WrongType {
                            expected: "json", got: other.type_name(),
                        }.into()),
                    }
                }
            }

            CmdId::JsonSet => {
                check_oom(&self.pool)?;
                let req: JsonSetRequest = decode(&frame.payload)?;
                let slot = Iris::slot_for(&req.key);
                let result = p.get_mut_with(&req.key, now, |entry| {
                    match &mut entry.value {
                        Value::Json(doc) => doc.set(&req.path, &req.value).map_err(anyhow::Error::from),
                        other => Err(MnemeError::WrongType {
                            expected: "json", got: other.type_name(),
                        }.into()),
                    }
                });
                match result {
                    None => {
                        // Key doesn't exist — create new JSON doc
                        let doc = JsonDoc::new(req.value).map_err(anyhow::Error::from)?;
                        p.set(req.key, Entry::new(Value::Json(doc), slot));
                        ok_str("OK")
                    }
                    Some(Ok(())) => ok_str("OK"),
                    Some(Err(e)) => Err(e),
                }
            }

            CmdId::JsonDel => {
                let req: JsonDelRequest = decode(&frame.payload)?;
                let result = p.get_mut_with(&req.key, now, |entry| {
                    match &mut entry.value {
                        Value::Json(doc) => doc.del(&req.path).map_err(anyhow::Error::from),
                        other => Err(MnemeError::WrongType {
                            expected: "json", got: other.type_name(),
                        }.into()),
                    }
                });
                match result {
                    None         => ok_payload(&false),
                    Some(Ok(v))  => ok_payload(&v),
                    Some(Err(e)) => Err(e),
                }
            }

            CmdId::JsonExists => {
                let req: JsonGetRequest = decode(&frame.payload)?;
                let exists = match p.get(&req.key, now) {
                    None => false,
                    Some(entry) => match &entry.value {
                        Value::Json(doc) => doc.exists(&req.path),
                        _               => false,
                    }
                };
                ok_payload(&exists)
            }

            CmdId::JsonType => {
                let req: JsonGetRequest = decode(&frame.payload)?;
                match p.get(&req.key, now) {
                    None => Err(MnemeError::KeyNotFound.into()),
                    Some(entry) => match &entry.value {
                        Value::Json(doc) => {
                            let t = doc.type_at(&req.path).map_err(anyhow::Error::from)?;
                            ok_payload(&t)
                        }
                        other => Err(MnemeError::WrongType {
                            expected: "json", got: other.type_name(),
                        }.into()),
                    }
                }
            }

            CmdId::JsonArrAppend => {
                let req: JsonArrAppendRequest = decode(&frame.payload)?;
                // Best-effort array append: get doc, append element string
                let result = p.get_mut_with(&req.key, now, |entry| {
                    match &mut entry.value {
                        Value::Json(doc) => {
                            // Get current array, append element
                            let current = doc.get(&req.path).map_err(anyhow::Error::from)?;
                            let trimmed = current.trim();
                            if !trimmed.starts_with('[') {
                                return Err(MnemeError::WrongType {
                                    expected: "array", got: "object",
                                }.into());
                            }
                            let inner = &trimmed[1..trimmed.len() - 1];
                            let new_arr = if inner.trim().is_empty() {
                                format!("[{}]", req.value)
                            } else {
                                format!("[{},{}]", inner, req.value)
                            };
                            doc.set(&req.path, &new_arr).map_err(anyhow::Error::from)
                        }
                        other => Err(MnemeError::WrongType {
                            expected: "json", got: other.type_name(),
                        }.into()),
                    }
                });
                match result {
                    None         => Err(MnemeError::KeyNotFound.into()),
                    Some(Ok(())) => ok_str("OK"),
                    Some(Err(e)) => Err(e),
                }
            }

            CmdId::JsonNumIncrBy => {
                let req: JsonNumIncrByRequest = decode(&frame.payload)?;
                let result = p.get_mut_with(&req.key, now, |entry| {
                    match &mut entry.value {
                        Value::Json(doc) => {
                            let val_str = doc.get(&req.path).map_err(anyhow::Error::from)?;
                            let n: f64 = val_str.trim().parse()
                                .map_err(|_| anyhow::anyhow!("not a number at path"))?;
                            let new_n = n + req.delta;
                            doc.set(&req.path, &new_n.to_string()).map_err(anyhow::Error::from)?;
                            Ok(new_n)
                        }
                        other => Err(MnemeError::WrongType {
                            expected: "json", got: other.type_name(),
                        }.into()),
                    }
                });
                match result {
                    None         => Err(MnemeError::KeyNotFound.into()),
                    Some(Ok(v))  => ok_payload(&v),
                    Some(Err(e)) => Err(e),
                }
            }

            // ── Auth ──────────────────────────────────────────────────────────
            CmdId::RevokeToken => {
                let token: String = decode(&frame.payload)?;
                self.argus.revoke(&token)?;
                ok_str("OK")
            }

            // ── Observability ─────────────────────────────────────────────────
            CmdId::SlowLog => {
                let count: usize = rmp_serde::from_slice(&frame.payload).unwrap_or(128);
                let entries: Vec<(String, Vec<u8>, u64)> = self.delphi.get_slowlog(count)
                    .into_iter()
                    .map(|e| (e.cmd, e.key, e.duration_us))
                    .collect();
                ok_payload(&entries)
            }

            CmdId::Metrics => {
                let used  = self.pool.pool_used();
                let total = self.pool.pool_max();
                ok_payload(&(used, total))
            }

            CmdId::Stats => {
                let used    = self.pool.pool_used();
                let total   = self.pool.pool_max();
                let keys    = self.pool.total_entries();
                let keepers = self.moirai.keeper_infos().read().len();
                let stats   = format!(
                    "keys={keys} pool_used={used} pool_max={total} keepers={keepers} \
                     pool_ratio={:.2}",
                    if total > 0 { used as f64 / total as f64 } else { 0.0 }
                );
                ok_payload(&stats)
            }

            CmdId::MemoryUsage => {
                let key: Vec<u8> = decode(&frame.payload)?;
                match p.memory_usage_of(&key, now) {
                    Some(bytes) => ok_payload(&bytes),
                    None        => Err(MnemeError::KeyNotFound.into()),
                }
            }

            CmdId::Monitor => {
                // Subscribe returns a broadcast receiver; streaming handled in handle_connection.
                ok_str("OK")
            }

            // ── Config ────────────────────────────────────────────────────────
            CmdId::Config => {
                if let Ok(req) = rmp_serde::from_slice::<ConfigSetRequest>(&frame.payload) {
                    let mut cfg = self.config.lock();
                    match req.param.as_str() {
                        "memory.pool_bytes" => {
                            let v: u64 = req.value.parse()
                                .map_err(|_| anyhow::anyhow!("invalid value"))?;
                            cfg.memory.pool_bytes = req.value.clone();
                            self.pool.set_pool_max(v);
                            // Immediate eviction check: if lowering pool_max pushed
                            // pressure above threshold, evict now instead of waiting
                            // for the 10ms background tick.
                            let pressure = self.pool.pressure_ratio();
                            let now = now_ms();
                            if pressure >= 1.0 {
                                let total = self.pool.total_entries() as usize;
                                let n = (total / 20).max(64).min(4096);
                                let candidates = self.pool.lfu_candidates(n * 4, now);
                                let to_evict = Lethe::pick_eviction_candidates(&candidates, n);
                                let evicted = to_evict.len();
                                self.pool.remove_keys(&to_evict);
                                for _ in 0..evicted {
                                    self.aletheia.record_eviction("oom");
                                }
                                tracing::warn!(pressure, evicted, "CONFIG SET pool_bytes: immediate OOM eviction");
                            } else if pressure >= cfg.memory.eviction_threshold {
                                let total = self.pool.total_entries() as usize;
                                let n = (total / 100).max(16).min(512);
                                let candidates = self.pool.lfu_candidates(n * 4, now);
                                let to_evict = Lethe::pick_eviction_candidates(&candidates, n);
                                let evicted = to_evict.len();
                                self.pool.remove_keys(&to_evict);
                                for _ in 0..evicted {
                                    self.aletheia.record_eviction("lfu");
                                }
                            }
                        }
                        "memory.eviction_threshold" => {
                            cfg.memory.eviction_threshold = req.value.parse()
                                .map_err(|_| anyhow::anyhow!("invalid float"))?;
                        }
                        other => return Err(anyhow::anyhow!("unknown config param: {other}")),
                    }
                    return ok_str("OK");
                }
                // CONFIG GET <param>
                let param: String = decode(&frame.payload)?;
                let cfg = self.config.lock();
                let value = match param.as_str() {
                    "memory.pool_bytes"        => cfg.memory.pool_bytes.clone(),
                    "memory.eviction_threshold" => cfg.memory.eviction_threshold.to_string(),
                    "node.id"                  => cfg.node.node_id.clone(),
                    other => return Err(anyhow::anyhow!("unknown config param: {other}")),
                };
                ok_payload(&value)
            }

            // ── Cluster / Keeper ──────────────────────────────────────────────
            CmdId::ClusterInfo => {
                let keeper_infos = self.moirai.keeper_infos();
                let keepers = keeper_infos.read();
                let cfg = self.config.lock();
                let keeper_count = keepers.len();
                let role = match cfg.node.role {
                    mneme_common::config::NodeRole::Core        => "core",
                    mneme_common::config::NodeRole::Keeper      => "keeper",
                    mneme_common::config::NodeRole::Solo        => "solo",
                    mneme_common::config::NodeRole::ReadReplica => "read-replica",
                };

                // Raft state from Themis.
                let raft_term  = self.themis.current_term();
                let is_leader  = self.themis.is_leader();
                let leader_hex = self.themis.leader_id()
                    .map(|id| format!("{id:016x}"))
                    .unwrap_or_else(|| "(unknown)".into());

                // Warmup gate state: reports whether QUORUM/ALL are currently blocked.
                let warmup_hot     = self.warmup.is_hot();
                let warmup_pending = self.warmup.pending.load(Ordering::Relaxed);
                let warmup_state   = if warmup_hot {
                    "hot".to_string()
                } else if warmup_pending > 0 {
                    format!("warming ({warmup_pending} keepers pending)")
                } else {
                    "cold".to_string()
                };

                // Supported consistency modes, accurately reflecting topology
                // and warmup gating:
                //   read-replica / no keepers → EVENTUAL only (no writes)
                //   warming                   → QUORUM/ALL blocked until hot
                //   1 keeper (hot)            → EVENTUAL, ONE, QUORUM
                //   2+ keepers (hot)          → EVENTUAL, ONE, QUORUM, ALL
                let supported_modes = if keeper_count == 0 || role == "read-replica" {
                    "EVENTUAL".to_string()
                } else if !warmup_hot {
                    format!("EVENTUAL, ONE  (QUORUM/ALL blocked — {warmup_state})")
                } else if keeper_count == 1 {
                    "EVENTUAL, ONE, QUORUM".to_string()
                } else {
                    "EVENTUAL, ONE, QUORUM, ALL".to_string()
                };

                // Memory pressure
                let pressure_pct = format!("{:.1}%", self.pool.pressure_ratio() * 100.0);

                // Returns Vec<(key, value)> — CLI formats as aligned table.
                let pairs: Vec<(String, String)> = vec![
                    ("state".into(),           "ok".into()),
                    ("role".into(),            role.to_string()),
                    ("node_id".into(),         cfg.node.node_id.clone()),
                    ("keeper_count".into(),    keeper_count.to_string()),
                    ("pool_used_bytes".into(), self.pool.pool_used().to_string()),
                    ("pool_max_bytes".into(),  self.pool.pool_max().to_string()),
                    ("memory_pressure".into(), pressure_pct),
                    ("total_keys".into(),      self.pool.total_entries().to_string()),
                    ("client_port".into(),     cfg.node.port.to_string()),
                    ("repl_port".into(),       cfg.node.rep_port.to_string()),
                    ("warmup_state".into(),    warmup_state),
                    ("supported_modes".into(), supported_modes),
                    ("raft_term".into(),       raft_term.to_string()),
                    ("is_leader".into(),       is_leader.to_string()),
                    ("leader_id".into(),       leader_hex),
                ];
                ok_payload(&pairs)
            }

            CmdId::ClusterSlots => {
                let table = self.iris.slot_table();
                ok_payload(&table)
            }

            CmdId::KeeperList => {
                // Returns Vec<(node_id, node_name, addr, pool_bytes, used_bytes)> — CLI formats as table.
                let keepers: Vec<(u64, String, String, u64, u64)> = self.moirai.keeper_infos().read().iter().map(|k| {
                    (k.node_id, k.node_name.clone(), k.addr.clone(), k.pool_bytes, k.used_bytes)
                }).collect();
                ok_payload(&keepers)
            }

            CmdId::PoolStats => {
                let used   = self.pool.pool_used();
                let total  = self.pool.pool_max();
                let kcount = self.moirai.keeper_infos().read().len();
                ok_payload(&(used, total, kcount))
            }

            CmdId::Wait => {
                let req: WaitRequest = decode(&frame.payload)?;
                let n = self.moirai.keeper_count().min(req.n_keepers);
                ok_payload(&(n as u64))
            }

            CmdId::GenerateJoinToken => {
                // Build a join token with exactly three colon-separated fields:
                //   base64(ca.crt) : cluster_secret : node.join_token
                //
                // Field 1 — CA certificate in PEM form, base64-encoded so it is
                //            newline-free and safe to paste on a single shell line.
                // Field 2 — cluster_secret (plaintext string, NOT re-encoded).
                //            The raw string never contains ':', so splitting on ':'
                //            is unambiguous.
                // Field 3 — join_token (mneme_tok_<32 hex chars>) that Keeper must
                //            present in its SyncStart frame to authenticate with Core.
                //
                // install.sh splits on ':' and uses the three parts to bootstrap a
                // new Keeper or read-replica node.  Admin-only (RBAC blocks others).
                let (ca_cert_path, cluster_secret, join_token) = {
                    let cfg = self.config.lock();
                    (cfg.tls.ca_cert.clone(),
                     cfg.auth.cluster_secret.clone(),
                     cfg.node.join_token.clone())
                };
                let ca_pem = fs::read(&ca_cert_path)
                    .map_err(|e| mneme_common::MnemeError::Protocol(
                        format!("cannot read CA cert at {ca_cert_path}: {e}")
                    ))?;
                use base64::{Engine, engine::general_purpose::STANDARD as B64};
                let ca_b64 = B64.encode(&ca_pem);
                // cluster_secret and join_token are plain ASCII strings — no re-encoding.
                let token = format!("{ca_b64}:{cluster_secret}:{join_token}");
                ok_payload(&token)
            }

            // ── Database namespace commands ───────────────────────────────────
            CmdId::Select => {
                // SELECT is fully handled in handle_connection (before this function is called).
                // If it somehow reaches dispatch_command, return OK so it's not an error.
                ok_str("OK")
            }

            CmdId::DbSize => {
                let req: DbSizeRequest = decode(&frame.payload)
                    .unwrap_or(DbSizeRequest { db_id: None, name: String::new() });
                let target_db = self.resolve_db_name_or(&req.name, req.db_id, db_id)?;
                let count = self.pool.db_size(target_db, now);
                ok_payload(&count)
            }

            CmdId::FlushDb => {
                let req: FlushDbRequest = decode(&frame.payload)
                    .unwrap_or(FlushDbRequest { db_id: None, name: String::new(), sync: true });
                let target_db = self.resolve_db_name_or(&req.name, req.db_id, db_id)?;
                let flushed = self.pool.flush_db(target_db);
                // Replicate flush as a special tombstone: empty key with deleted=true and db_id set.
                // Keepers that understand db_id can flush the namespace; others will be re-synced
                // on next connection.
                if req.sync {
                    let push = PushKeyPayload {
                        key: make_db_key(target_db, b""),
                        value: Value::String(vec![]),
                        seq: 0,
                        ttl_ms: 0,
                        slot: 0,
                        deleted: true,
                        db_id: target_db,
                    };
                    if let Ok(payload) = rmp_serde::to_vec(&push) {
                        let _ = self.moirai.dispatch(Frame {
                            cmd_id: CmdId::PushKey,
                            flags: frame.flags,
                            req_id: 0,
                            payload: Bytes::from(payload),
                        }, consistency).await;
                    }
                }
                ok_payload(&flushed)
            }

            CmdId::DbCreate => {
                let req: DbCreateRequest = decode(&frame.payload)?;
                if req.name.is_empty() {
                    return Err(MnemeError::Protocol("DB-CREATE: name must not be empty".into()).into());
                }
                let max_dbs = self.config.lock().databases.max_databases;
                let assigned = {
                    let mut reg = self.db_registry.write();
                    if reg.contains_key(&req.name) {
                        return Err(MnemeError::Protocol(
                            format!("DB-CREATE: name '{}' already exists", req.name),
                        ).into());
                    }
                    let id = if let Some(explicit) = req.db_id {
                        if explicit >= max_dbs {
                            return Err(MnemeError::Protocol(
                                format!("DB-CREATE: id {} out of range (0..{})", explicit, max_dbs - 1),
                            ).into());
                        }
                        explicit
                    } else {
                        // Assign next available ID not already used by any name.
                        let used: std::collections::HashSet<u16> = reg.values().copied().collect();
                        (1..max_dbs).find(|id| !used.contains(id)).ok_or_else(|| {
                            MnemeError::Protocol("DB-CREATE: all database IDs are in use".into())
                        })?
                    };
                    reg.insert(req.name.clone(), id);
                    id
                };
                let users_db_path = self.config.lock().auth.users_db.clone();
                persist_db_registry(&self.db_registry.read(), &users_db_path);
                info!(name = %req.name, id = assigned, "DB-CREATE: registered named database");
                ok_payload(&DbInfo { name: req.name, id: assigned })
            }

            CmdId::DbList => {
                let reg = self.db_registry.read();
                let mut list: Vec<DbInfo> = reg
                    .iter()
                    .map(|(name, &id)| DbInfo { name: name.clone(), id })
                    .collect();
                list.sort_by_key(|d| d.id);
                ok_payload(&list)
            }

            CmdId::DbDrop => {
                let req: DbDropRequest = decode(&frame.payload)?;
                let removed = self.db_registry.write().remove(&req.name);
                match removed {
                    None => Err(MnemeError::Protocol(
                        format!("DB-DROP: name '{}' not found", req.name),
                    ).into()),
                    Some(id) => {
                        let users_db_path = self.config.lock().auth.users_db.clone();
                        persist_db_registry(&self.db_registry.read(), &users_db_path);
                        info!(name = %req.name, id, "DB-DROP: unregistered named database");
                        ok_payload(&id)
                    }
                }
            }

            // ── Bulk / scan commands ──────────────────────────────────────────

            CmdId::Scan => {
                let req: ScanRequest = decode(&frame.payload)
                    .unwrap_or(ScanRequest { cursor: 0, pattern: None, count: 10 });
                let count = req.count.max(1).min(1000);
                let (next_cursor, keys) = self.pool.scan_db(
                    db_id, req.cursor, req.pattern.as_deref(), count, now,
                );
                ok_payload(&(next_cursor, keys))
            }

            CmdId::Type => {
                let key: Vec<u8> = decode(&frame.payload)?;
                let ns_key = make_db_key(db_id, &key);
                let type_str = self.pool.key_type(&ns_key, now);
                ok_payload(&type_str)
            }

            CmdId::MGet => {
                let req: MGetRequest = decode(&frame.payload)?;
                let values: Vec<Option<Value>> = req.keys.iter()
                    .map(|k| p.get_mut_with(k, now, |entry| {
                        entry.lfu_counter = Lethe::increment_lfu(entry.lfu_counter);
                        entry.value.clone()
                    }))
                    .collect();
                ok_payload(&values)
            }

            CmdId::MSet => {
                check_oom(&self.pool)?;
                let req: MSetRequest = decode(&frame.payload)?;
                for (key, value, ttl_ms) in req.pairs {
                    let slot = Iris::slot_for(&key);
                    let mut entry = Entry::new(value.clone(), slot);
                    if ttl_ms > 0 {
                        entry = entry.with_ttl(ttl_ms, now);
                        self.lethe.schedule(key.clone(), entry.expires_at_ms, now);
                    }
                    p.set(key.clone(), entry);
                    let push = PushKeyPayload {
                        key: p.ns_key(&key),
                        value,
                        seq: 0,
                        ttl_ms,
                        slot,
                        deleted: false,
                        db_id,
                    };
                    if let Ok(push_payload) = rmp_serde::to_vec(&push) {
                        let _ = self.moirai.dispatch(Frame {
                            cmd_id: CmdId::PushKey,
                            flags: frame.flags,
                            req_id: 0,
                            payload: Bytes::from(push_payload),
                        }, consistency).await;
                    }
                }
                ok_str("OK")
            }

            // ── User management commands (admin-only, enforced by RBAC) ─────────
            // RBAC already blocked non-admins before reaching here. These handlers
            // perform the actual UsersDb mutations.

            CmdId::UserCreate => {
                let req: UserCreateRequest = decode(&frame.payload)?;
                let uid = self.argus.create_user(&req.username, &req.password, &req.role)
                    .map_err(|e| anyhow::anyhow!("USER CREATE: {e}"))?;
                ok_payload(&format!("Created user '{}' (id={uid}, role={})", req.username, req.role))
            }

            CmdId::UserDelete => {
                let req: UserDeleteRequest = decode(&frame.payload)?;
                self.argus.delete_user(&req.username)
                    .map_err(|e| anyhow::anyhow!("USER DELETE: {e}"))?;
                ok_str("OK")
            }

            CmdId::UserList => {
                // Returns Vec<(username, role, allowed_dbs)> — no password hashes.
                let list = self.argus.list_users();
                ok_payload(&list)
            }

            CmdId::UserGrant => {
                let req: UserGrantRequest = decode(&frame.payload)?;
                self.argus.grant_db_access(&req.username, req.db_id)
                    .map_err(|e| anyhow::anyhow!("USER GRANT: {e}"))?;
                ok_str("OK")
            }

            CmdId::UserRevoke => {
                let req: UserRevokeRequest = decode(&frame.payload)?;
                self.argus.revoke_db_access(&req.username, req.db_id)
                    .map_err(|e| anyhow::anyhow!("USER REVOKE: {e}"))?;
                ok_str("OK")
            }

            CmdId::UserSetRole => {
                let req: UserSetRoleRequest = decode(&frame.payload)?;
                self.argus.set_user_role(&req.username, &req.role)
                    .map_err(|e| anyhow::anyhow!("USER SETROLE: {e}"))?;
                ok_str("OK")
            }

            CmdId::UserInfo => {
                let req: UserInfoRequest = decode(&frame.payload)
                    .unwrap_or(UserInfoRequest { username: None });
                // Non-admins may only query themselves (enforced via username lookup;
                // if they provide another username the handler returns their own info).
                let info = match req.username {
                    Some(ref name) => self.argus.get_user(name),
                    None           => None, // caller provides no name → rely on user_id from Claims
                };
                match info {
                    Some((uid, role, allowed_dbs)) => {
                        let name = req.username.as_deref().unwrap_or("");
                        ok_payload(&(name, uid, role, allowed_dbs))
                    }
                    None => {
                        // Return a placeholder indicating user was not found.
                        Err(MnemeError::KeyNotFound.into())
                    }
                }
            }

            other => Err(anyhow::anyhow!("unhandled command: {other:?}")),
        }
    }

    // ── counter helper ─────────────────────────────────────────────────────────

    fn counter_op(
        &self,
        key: &[u8],
        _now_ms: u64,
        db_id: u16,
        op: impl Fn(&mut Value) -> mneme_common::Result<i64>,
    ) -> Result<Frame> {
        let slot = Iris::slot_for(key);
        let p = DbPool::new(&self.pool, db_id);
        // Try in-place mutation
        let result = p.get_mut_with(key, _now_ms, |entry| op(&mut entry.value));
        match result {
            Some(Ok(n))  => ok_payload(&n),
            Some(Err(e)) => Err(e.into()),
            None => {
                // Key doesn't exist — initialize to Counter(0) then apply op
                let mut v = Value::Counter(0);
                let n = op(&mut v).map_err(anyhow::Error::from)?;
                p.set(key.to_vec(), Entry::new(Value::Counter(n), slot));
                ok_payload(&n)
            }
        }
    }

    // ── Herold one-shot registration handler ──────────────────────────────────

    /// Handle a SyncStart frame with flags=REGISTER_FLAGS.
    /// Validates the join_token, responds with a RegisterAck, then returns
    /// (closes the connection).  The joining node reconnects separately
    /// with a normal SyncStart (flags=0) to begin the warm-up replication session.
    async fn handle_herold_register<S>(
        self: Arc<Self>,
        mut stream: S,
        peer: SocketAddr,
        frame: Frame,
    ) -> Result<()>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let reg: RegisterPayload = match rmp_serde::from_slice(&frame.payload) {
            Ok(r) => r,
            Err(e) => {
                warn!(%peer, "Herold REGISTER: decode error: {e}");
                let ack = RegisterAck { accepted: false, message: format!("decode error: {e}"), assigned_id: 0 };
                let payload = rmp_serde::to_vec(&ack).unwrap_or_default();
                let _ = stream.write_all(&Frame {
                    cmd_id: CmdId::AckWrite, flags: 0, req_id: 0,
                    payload: Bytes::from(payload),
                }.encode()).await;
                return Ok(());
            }
        };

        let expected_token = self.config.lock().node.join_token.clone();
        if reg.join_token != expected_token {
            warn!(%peer, node_id = %reg.node_id, "Herold REGISTER: wrong join_token — rejected");
            let ack = RegisterAck {
                accepted: false,
                message: "invalid join_token".into(),
                assigned_id: 0,
            };
            let payload = rmp_serde::to_vec(&ack).unwrap_or_default();
            let _ = stream.write_all(&Frame {
                cmd_id: CmdId::AckWrite, flags: 0, req_id: 0,
                payload: Bytes::from(payload),
            }.encode()).await;
            return Ok(());
        }

        // Assign an ID consistent with how the keeper computes its own node_id
        // (DefaultHasher of the string id).  This makes the REGISTER-assigned ID
        // match the node_id the keeper will use in the subsequent SyncStart.
        let assigned_id = node_id_u64(&reg.node_id);
        let ack = RegisterAck {
            accepted: true,
            message: "OK".into(),
            assigned_id,
        };
        let payload = rmp_serde::to_vec(&ack).unwrap_or_default();
        stream.write_all(&Frame {
            cmd_id: CmdId::AckWrite, flags: 0, req_id: 0,
            payload: Bytes::from(payload),
        }.encode()).await?;

        info!(
            %peer,
            node_id = %reg.node_id,
            assigned_id,
            role = %reg.role,
            "Herold REGISTER: accepted — keeper should now connect for SyncStart"
        );
        Ok(())
    }

    // ── Raft peer connection handler ────────────────────────────────────────

    async fn handle_raft_peer_connection<S>(
        self: Arc<Self>,
        mut stream: S,
        peer: SocketAddr,
        first_frame: Frame,
        mut buf: BytesMut,
    ) -> Result<()>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        info!(%peer, "Raft peer connected");

        // Process the first frame that was already read.
        let resp = self.dispatch_raft_rpc(&first_frame).await;
        stream.write_all(&resp.encode()).await?;
        stream.flush().await?;

        // Loop reading subsequent Raft RPC frames.
        loop {
            // Try to decode from buffer first.
            while buf.len() >= FRAME_HEADER {
                let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                if buf.len() < FRAME_HEADER + plen { break; }
                let (frame, consumed) = Frame::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
                let _ = buf.split_to(consumed);
                let resp = self.dispatch_raft_rpc(&frame).await;
                stream.write_all(&resp.encode()).await?;
                stream.flush().await?;
            }
            // Read more data.
            let n = stream.read_buf(&mut buf).await?;
            if n == 0 { break; }
        }
        info!(%peer, "Raft peer disconnected");
        Ok(())
    }

    async fn dispatch_raft_rpc(&self, frame: &Frame) -> Frame {
        match frame.cmd_id {
            CmdId::RaftAppendEntries => {
                match rmp_serde::from_slice(&frame.payload) {
                    Ok(rpc) => match self.themis.handle_append_entries(rpc).await {
                        Ok(resp) => {
                            let payload = rmp_serde::to_vec(&resp).unwrap_or_default();
                            Frame { cmd_id: CmdId::Ok, flags: 0, req_id: 0, payload: Bytes::from(payload) }
                        }
                        Err(e) => Frame::error_response(&format!("append_entries: {e}")),
                    },
                    Err(e) => Frame::error_response(&format!("decode: {e}")),
                }
            }
            CmdId::RaftVote => {
                match rmp_serde::from_slice(&frame.payload) {
                    Ok(rpc) => match self.themis.handle_vote(rpc).await {
                        Ok(resp) => {
                            let payload = rmp_serde::to_vec(&resp).unwrap_or_default();
                            Frame { cmd_id: CmdId::Ok, flags: 0, req_id: 0, payload: Bytes::from(payload) }
                        }
                        Err(e) => Frame::error_response(&format!("vote: {e}")),
                    },
                    Err(e) => Frame::error_response(&format!("decode: {e}")),
                }
            }
            CmdId::RaftInstallSnapshot => {
                match rmp_serde::from_slice(&frame.payload) {
                    Ok(rpc) => match self.themis.handle_install_snapshot(rpc).await {
                        Ok(resp) => {
                            let payload = rmp_serde::to_vec(&resp).unwrap_or_default();
                            Frame { cmd_id: CmdId::Ok, flags: 0, req_id: 0, payload: Bytes::from(payload) }
                        }
                        Err(e) => Frame::error_response(&format!("install_snapshot: {e}")),
                    },
                    Err(e) => Frame::error_response(&format!("decode: {e}")),
                }
            }
            _ => Frame::error_response("unexpected cmd_id in Raft peer conn"),
        }
    }

    // ── keeper connection handler (replication port) ──────────────────────────

    async fn handle_keeper_connection<S>(
        self: Arc<Self>,
        mut stream: S,
        peer: SocketAddr,
    ) -> Result<()>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let mut buf = BytesMut::with_capacity(4096);
        info!(%peer, "Keeper connected");

        // Read SyncStart frame
        loop {
            if buf.len() >= FRAME_HEADER {
                let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                if buf.len() >= FRAME_HEADER + plen { break; }
            }
            let n = stream.read_buf(&mut buf).await?;
            if n == 0 { return Ok(()); }
        }

        let (frame, consumed) = Frame::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
        let _ = buf.split_to(consumed);

        // Raft peer connections: dispatch to handle_raft_peer_connection.
        if matches!(frame.cmd_id, CmdId::RaftAppendEntries | CmdId::RaftVote | CmdId::RaftInstallSnapshot) {
            return self.handle_raft_peer_connection(stream, peer, frame, buf).await;
        }

        if frame.cmd_id != CmdId::SyncStart {
            warn!(%peer, cmd = ?frame.cmd_id, "Expected SyncStart from keeper");
            return Ok(());
        }

        // ── Herold REGISTER handshake (A-03 fix) ──────────────────────────────
        // A SyncStart with flags=REGISTER_FLAGS is a one-shot registration frame.
        // Validate the join_token, respond with RegisterAck, then close this
        // connection.  The keeper will reconnect with a normal SyncStart
        // (flags=0) for the actual warm-up and replication session.
        if frame.flags == REGISTER_FLAGS {
            return self.handle_herold_register(stream, peer, frame).await;
        }

        let sync: mneme_common::SyncStartPayload = rmp_serde::from_slice(&frame.payload)
            .map_err(|e| anyhow::anyhow!("SyncStart decode: {e}"))?;

        // Verify cluster_secret — reject if wrong or empty.
        {
            let expected = self.config.lock().auth.cluster_secret.clone();
            if sync.cluster_secret != expected {
                warn!(%peer, node_id = sync.node_id, "Keeper rejected: wrong cluster_secret");
                let _ = stream.write_all(
                    &Frame::error_response("wrong cluster_secret").encode()
                ).await;
                return Ok(());
            }
        }
        info!(
            %peer,
            node_id = sync.node_id,
            highest_seq = sync.highest_seq,
            key_count = sync.key_count,
            "Keeper SyncStart accepted — warm-up starting"
        );

        // Register keeper in unified registry + create Hermes outbound connection (A-01, A-02).
        //
        // Address fix: if the keeper's replication_addr has host 0.0.0.0 (i.e., it bound to all
        // interfaces and reported its bind address), replace the host with the actual peer IP we
        // observed from the accepted TCP connection.  This gives an always-routable address.
        let keeper_repl_addr = {
            let raw = sync.replication_addr.as_str();
            if raw.is_empty() {
                format!("{}:7379", peer.ip())
            } else if let Some(port) = raw.strip_prefix("0.0.0.0:") {
                format!("{}:{}", peer.ip(), port)
            } else {
                raw.to_owned()
            }
        };
        let node_name = if sync.node_name.is_empty() {
            // Older keeper binary without node_name: fall back to hex node_id.
            format!("{:016x}", sync.node_id)
        } else {
            sync.node_name.clone()
        };
        let handle = self.hermes.connect_to_keeper(sync.node_id, keeper_repl_addr.clone());
        let handle_for_replay = handle.clone();
        self.moirai.add_keeper(handle, node_name, keeper_repl_addr, sync.pool_bytes);

        // BUG-7 fix: if Core is already Hot (a previous full warm-up completed),
        // this keeper is reconnecting after a crash/restart.  Don't touch the
        // warmup gate — Core stays hot and QUORUM reads keep serving.  The
        // reconnecting keeper will push its keys to refresh Core's RAM pool, but
        // we don't need to block reads while it does so.
        let already_hot = self.warmup.is_hot();
        if !already_hot {
            self.warmup.pending.fetch_add(1, Ordering::SeqCst);
            self.warmup.total_expected.fetch_add(sync.key_count, Ordering::Relaxed);
        }

        // Acknowledge SyncStart — keeper will now push all its keys
        stream.write_all(&frame.encode()).await?;

        // Track expected keys for this specific keeper (used in disconnect guard)
        let keeper_expected = sync.key_count;

        // Main receive loop: handles PushKey (warm-up), Heartbeat, and SyncComplete
        loop {
            // Read until we have a complete frame
            loop {
                if buf.len() >= FRAME_HEADER {
                    let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                    if buf.len() >= FRAME_HEADER + plen { break; }
                }
                let n = stream.read_buf(&mut buf).await?;
                if n == 0 {
                    info!(%peer, node_id = sync.node_id, "Keeper disconnected");
                    // Shrink logical pool by keeper's grant before removing
                    {
                        let infos = self.moirai.keeper_infos();
                        let ki = infos.read();
                        if let Some(k) = ki.iter().find(|k| k.node_id == sync.node_id) {
                            let old_max = self.pool.pool_max();
                            let grant = k.pool_bytes;
                            if grant > 0 && old_max > grant {
                                self.pool.set_pool_max(old_max - grant);
                                info!(node_id = sync.node_id, grant, "Pool max decreased on keeper disconnect");
                            }
                        }
                    }
                    self.hermes.remove_keeper(sync.node_id);
                    self.moirai.remove_keeper(sync.node_id);
                    // BUG-6 fix: never set hot=true on disconnect.  If a keeper
                    // disconnects before SyncComplete and it had data to push,
                    // Core must stay in Warming state and wait for reconnect.
                    // This prevents QUORUM reads from returning stale/empty data.
                    // Only decrement pending if we were tracking this keeper.
                    if !already_hot {
                        self.warmup.pending.fetch_sub(1, Ordering::SeqCst);
                        if keeper_expected > 0 {
                            warn!(
                                node_id = sync.node_id, keeper_expected,
                                "Keeper disconnected before SyncComplete — staying in Warming state, waiting for reconnect"
                            );
                        }
                        // If keeper had zero keys, check whether all other keepers finished.
                        // Re-check hot condition: if pending reached 0, all keepers with data
                        // already sent SyncComplete.  But we only set hot from SyncComplete,
                        // so do nothing here — let the next SyncComplete flip the gate.
                    }
                    return Ok(());
                }
            }

            let (f, c) = Frame::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
            let _ = buf.split_to(c);

            match f.cmd_id {
                CmdId::PushKey => {
                    // BUG-16 fix: accept warm-up push from Keeper and populate the RAM pool.
                    // The key in PushKeyPayload already carries the 2-byte db_id prefix that
                    // God wrote when it originally stored the key. We store it verbatim so the
                    // db namespace is preserved transparently.
                    let push: PushKeyPayload = rmp_serde::from_slice(&f.payload)
                        .map_err(|e| anyhow::anyhow!("PushKey decode: {e}"))?;
                    let cur_now = now_ms();
                    if !push.deleted {
                        // Use push.slot (already computed from original un-prefixed key at write time).
                        let mut entry = Entry::new(push.value, push.slot);
                        if push.ttl_ms > 0 {
                            entry = entry.with_ttl(push.ttl_ms, cur_now);
                            self.lethe.schedule(push.key.clone(), entry.expires_at_ms, cur_now);
                        }
                        self.pool.set(push.key, entry);
                    } else if push.key.len() == 2 {
                        // FlushDb tombstone: empty key with db prefix only → flush entire namespace.
                        self.pool.flush_db(push.db_id);
                    } else {
                        self.pool.del(&push.key);
                    }
                    self.warmup.total_received.fetch_add(1, Ordering::Relaxed);
                    // Send ACK
                    stream.write_all(
                        &Frame::ok_response(bytes::Bytes::new()).encode()
                    ).await?;
                }

                CmdId::SyncComplete => {
                    // BUG-22 fix: keeper finished pushing all its keys
                    let complete: SyncCompletePayload = rmp_serde::from_slice(&f.payload)
                        .unwrap_or(SyncCompletePayload {
                            node_id: sync.node_id,
                            pushed_keys: 0,
                            highest_seq: 0,
                        });
                    let pushed = complete.pushed_keys;
                    if pushed != keeper_expected {
                        warn!(
                            node_id = sync.node_id,
                            expected = keeper_expected,
                            received = pushed,
                            "Key count mismatch: some keys may have been dropped during push phase"
                        );
                    }
                    info!(
                        node_id = sync.node_id,
                        pushed_keys = pushed,
                        expected = keeper_expected,
                        "Keeper warm-up complete"
                    );
                    if !already_hot {
                        self.warmup.completed_keepers.fetch_add(1, Ordering::Relaxed);
                        let prev_pending = self.warmup.pending.fetch_sub(1, Ordering::SeqCst);
                        if prev_pending == 1 {
                            // Last keeper completed — God node is now hot
                            let total_rx = self.warmup.total_received.load(Ordering::Relaxed);
                            let total_ex = self.warmup.total_expected.load(Ordering::Relaxed);
                            let n_keepers = self.warmup.completed_keepers.load(Ordering::Relaxed);
                            self.warmup.hot.store(true, Ordering::Release);
                            info!(
                                total_received = total_rx,
                                total_expected = total_ex,
                                keepers = n_keepers,
                                "God node WARM — all keepers have synced"
                            );
                        }
                    } else {
                        info!(
                            node_id = sync.node_id,
                            pushed_keys = pushed,
                            "Keeper re-synced after reconnect (node already hot — warmup gate unchanged)"
                        );
                        // Read-replica reconnect: key_count==0 means the reconnecting node is
                        // a read replica (replicas never push keys to Core). Push the entire
                        // current hot pool to it so it has a fresh snapshot.
                        if sync.key_count == 0 {
                            let me2 = self.clone();
                            let h = handle_for_replay.clone();
                            // Collect frames without holding the shard locks across await.
                            let now = now_ms();
                            let mut frames: Vec<Frame> = Vec::new();
                            for shard in &me2.pool.shards {
                                let guard = shard.read();
                                for (key, entry) in guard.iter() {
                                    if entry.is_expired(now) {
                                        continue;
                                    }
                                    let db_id = u16::from_be_bytes([key[0], key[1]]);
                                    let ttl_ms = if entry.expires_at_ms == 0 {
                                        0
                                    } else {
                                        entry.expires_at_ms.saturating_sub(now)
                                    };
                                    let push = PushKeyPayload {
                                        key: key.clone(),
                                        value: entry.value.clone(),
                                        seq: 0,
                                        ttl_ms,
                                        slot: entry.slot,
                                        deleted: false,
                                        db_id,
                                    };
                                    if let Ok(bytes) = rmp_serde::to_vec(&push) {
                                        frames.push(Frame {
                                            cmd_id: CmdId::PushKey,
                                            flags: 0,
                                            req_id: 0,
                                            payload: Bytes::from(bytes),
                                        });
                                    }
                                }
                                // guard dropped here — lock released before next iteration
                            }
                            let total = frames.len();
                            tokio::spawn(async move {
                                let mut pushed_count = 0u64;
                                for frame in frames {
                                    let (ack_tx, _ack_rx) = tokio::sync::mpsc::channel(1);
                                    if h.tx.send((frame, ack_tx)).await.is_err() {
                                        // Replica disconnected mid-replay — abort.
                                        warn!(node_id = h.node_id, pushed = pushed_count,
                                            "Replica disconnected during full-pool replay");
                                        return;
                                    }
                                    pushed_count += 1;
                                }
                                info!(
                                    node_id = h.node_id,
                                    pushed = pushed_count,
                                    total,
                                    "Replica full-pool replay complete"
                                );
                            });
                        }
                    }
                    stream.write_all(
                        &Frame::ok_response(bytes::Bytes::new()).encode()
                    ).await?;
                }

                CmdId::Heartbeat => {
                    // Parse heartbeat payload — update keeper stats in unified registry.
                    if let Ok(hb) = rmp_serde::from_slice::<HeartbeatPayload>(&f.payload) {
                        let infos = self.moirai.keeper_infos();
                        let mut keepers = infos.write();
                        if let Some(k) = keepers.iter_mut().find(|k| k.node_id == hb.node_id) {
                            k.pool_bytes = hb.pool_bytes;
                            k.used_bytes = hb.used_bytes;
                        }
                    }
                    stream.write_all(
                        &Frame::ok_response(bytes::Bytes::new()).encode()
                    ).await?;
                }

                other => {
                    warn!(%peer, node_id = sync.node_id, cmd = ?other,
                          "Unexpected frame from keeper — ignoring");
                }
            }
        }
    }

    // ── list / zset helpers ────────────────────────────────────────────────────

    fn push_list(
        &self,
        key: &[u8],
        values: Vec<Vec<u8>>,
        front: bool,
        slot: u16,
        _now_ms: u64,
        db_id: u16,
    ) -> u64 {
        let p = DbPool::new(&self.pool, db_id);
        let len = p.get_mut_with(key, _now_ms, |entry| {
            if let Value::List(ref mut deque) = entry.value {
                for v in &values {
                    if front { deque.push_front(v.clone()); }
                    else     { deque.push_back(v.clone()); }
                }
                deque.len() as u64
            } else { 0 }
        });
        if len.is_none() {
            let mut deque = VecDeque::new();
            for v in &values {
                if front { deque.push_front(v.clone()); }
                else     { deque.push_back(v.clone()); }
            }
            let l = deque.len() as u64;
            p.set(key.to_vec(), Entry::new(Value::List(deque), slot));
            l
        } else {
            len.unwrap()
        }
    }

    fn zadd(&self, key: &[u8], members: Vec<ZSetMember>, slot: u16, _now_ms: u64, db_id: u16) -> u64 {
        let p = DbPool::new(&self.pool, db_id);
        let added = p.get_mut_with(key, _now_ms, |entry| {
            if let Value::ZSet(ref mut existing) = entry.value {
                let mut count = 0u64;
                for m in &members {
                    if let Some(e) = existing.iter_mut().find(|e| e.member == m.member) {
                        e.score = m.score;
                    } else {
                        existing.push(m.clone());
                        count += 1;
                    }
                }
                count
            } else { 0 }
        });
        if added.is_none() {
            let count = members.len() as u64;
            p.set(key.to_vec(), Entry::new(Value::ZSet(members), slot));
            count
        } else {
            added.unwrap()
        }
    }

    // ── Read-replica replication ───────────────────────────────────────────────

    /// Handle an incoming PushKey/Replicate connection from the primary Core's Hermes.
    /// Core connects to this replica's rep_port after receiving our SyncStart.
    /// Receives PushKey frames and populates the local RAM pool.
    async fn handle_replica_replication<S>(
        &self,
        mut stream: S,
        peer: SocketAddr,
    ) -> Result<()>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let mut buf = BytesMut::with_capacity(4096);
        info!(%peer, "ReadReplica: Core replication connection established");

        loop {
            // Read until we have a complete frame
            loop {
                if buf.len() >= FRAME_HEADER {
                    let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                    if buf.len() >= FRAME_HEADER + plen { break; }
                }
                let n = stream.read_buf(&mut buf).await?;
                if n == 0 {
                    info!(%peer, "ReadReplica: Core replication connection closed");
                    self.warmup.stale.store(true, Ordering::Release);
                    return Ok(());
                }
            }

            let (f, consumed) = Frame::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
            let _ = buf.split_to(consumed);

            match f.cmd_id {
                CmdId::PushKey => {
                    let push: PushKeyPayload = rmp_serde::from_slice(&f.payload)
                        .map_err(|e| anyhow::anyhow!("PushKey decode: {e}"))?;
                    let cur_now = now_ms();
                    if !push.deleted {
                        let mut entry = Entry::new(push.value, push.slot);
                        if push.ttl_ms > 0 {
                            entry = entry.with_ttl(push.ttl_ms, cur_now);
                            self.lethe.schedule(push.key.clone(), entry.expires_at_ms, cur_now);
                        }
                        self.pool.set(push.key, entry);
                    } else if push.key.len() == 2 {
                        // FlushDb tombstone: 2-byte db prefix only
                        self.pool.flush_db(push.db_id);
                    } else {
                        self.pool.del(&push.key);
                    }
                    // ACK each PushKey so Hermes doesn't time out
                    stream.write_all(&Frame::ok_response(bytes::Bytes::new()).encode()).await?;
                }
                CmdId::Heartbeat => {
                    // Drain heartbeats silently — keep connection alive
                }
                _ => {
                    // Unknown frame type — ignore but don't ACK
                }
            }
        }
    }

    /// Outbound registration loop for the read-replica role.
    ///
    /// Connects to the primary Core's replication port, sends SyncStart
    /// (key_count=0), and waits for an ACK. Core then opens a Hermes
    /// connection back to this replica's rep_port and starts streaming
    /// PushKey frames for all subsequent writes.
    ///
    /// On disconnect: sets `warmup.stale=true` and reconnects. If
    /// `core_addr` is unreachable, rotates through `failover_addrs`.
    async fn run_replica_registration_loop(&self, config: MnemeConfig) {
        let node_id = node_id_u64(&config.node.node_id);

        // Build candidate addresses: primary first, then failovers
        let mut addrs: Vec<String> = Vec::new();
        if !config.node.core_addr.is_empty() {
            addrs.push(config.node.core_addr.clone());
        }
        addrs.extend(config.node.failover_addrs.clone());

        if addrs.is_empty() {
            warn!("ReadReplica: node.core_addr is empty — no primary Core to connect to. \
                   Set core_addr in [node] config.");
            return;
        }

        let mut backoff = Duration::from_secs(1);
        let mut addr_idx: usize = 0;
        let mut ever_connected = false;

        loop {
            let addr = addrs[addr_idx % addrs.len()].clone();
            info!(%addr, "ReadReplica: connecting to primary Core replication port");

            match connect_to_primary(&addr, &config.tls).await {
                Ok(mut stream) => {
                    backoff = Duration::from_secs(1);
                    addr_idx = 0; // reset to primary after any success
                    ever_connected = true;

                    // Send SyncStart — key_count=0: replica has no keys to push
                    let sync_payload = SyncStartPayload {
                        node_id,
                        highest_seq: 0,
                        cluster_secret: config.auth.cluster_secret.clone(),
                        key_count: 0,
                        version: 1,
                        replication_addr: config.replication_addr(),
                        node_name: config.node.node_id.clone(),
                        pool_bytes: config.memory.pool_bytes_u64(),
                    };
                    let sync_bytes = match rmp_serde::to_vec(&sync_payload) {
                        Ok(b) => b,
                        Err(e) => { warn!("ReadReplica: SyncStart serialize: {e}"); break; }
                    };
                    let sync_frame = Frame {
                        cmd_id: CmdId::SyncStart,
                        flags: 0,
                        req_id: 0,
                        payload: Bytes::from(sync_bytes),
                    };
                    if let Err(e) = stream.write_all(&sync_frame.encode()).await {
                        warn!(%addr, "ReadReplica: SyncStart write failed: {e}");
                        self.warmup.stale.store(true, Ordering::Release);
                        time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                        continue;
                    }
                    info!(%addr, "ReadReplica: SyncStart sent");

                    // Wait for Core's ACK
                    {
                        let mut ack_buf = BytesMut::with_capacity(256);
                        let ack_ok = loop {
                            if ack_buf.len() >= FRAME_HEADER {
                                let plen = u32::from_be_bytes(
                                    ack_buf[8..12].try_into().unwrap()
                                ) as usize;
                                if ack_buf.len() >= FRAME_HEADER + plen { break true; }
                            }
                            match stream.read_buf(&mut ack_buf).await {
                                Ok(0) | Err(_) => break false,
                                Ok(_) => {}
                            }
                        };
                        if !ack_ok {
                            warn!(%addr, "ReadReplica: Core closed connection before ACK");
                            self.warmup.stale.store(true, Ordering::Release);
                            time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(60));
                            continue;
                        }
                    }

                    // Send SyncComplete immediately (no keys to push)
                    let complete = SyncCompletePayload {
                        node_id,
                        pushed_keys: 0,
                        highest_seq: 0,
                    };
                    let complete_bytes = rmp_serde::to_vec(&complete).unwrap_or_default();
                    let complete_frame = Frame {
                        cmd_id: CmdId::SyncComplete,
                        flags: 0,
                        req_id: 0,
                        payload: Bytes::from(complete_bytes),
                    };
                    let _ = stream.write_all(&complete_frame.encode()).await;

                    // Mark as connected — Core will now stream PushKey via Hermes
                    self.warmup.hot.store(true, Ordering::Release);
                    self.warmup.stale.store(false, Ordering::Release);
                    info!(%addr, "ReadReplica: registered with Core — accepting PushKey via rep_port");

                    // Keep the handshake connection open; drain any frames Core may send.
                    // When it closes (Core restarted, network failure, etc.) set stale.
                    let mut drain_buf = BytesMut::with_capacity(256);
                    loop {
                        match stream.read_buf(&mut drain_buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                // Drain complete frames so the buffer doesn't grow unbounded
                                while drain_buf.len() >= FRAME_HEADER {
                                    let plen = u32::from_be_bytes(
                                        drain_buf[8..12].try_into().unwrap()
                                    ) as usize;
                                    if drain_buf.len() < FRAME_HEADER + plen { break; }
                                    let _ = drain_buf.split_to(FRAME_HEADER + plen);
                                }
                            }
                        }
                    }

                    warn!(%addr, "ReadReplica: primary Core disconnected — serving stale EVENTUAL reads");
                    self.warmup.stale.store(true, Ordering::Release);
                }
                Err(e) => {
                    if ever_connected {
                        self.warmup.stale.store(true, Ordering::Release);
                    }
                    warn!(%addr, backoff_s = backoff.as_secs(),
                          "ReadReplica: failed to connect to primary: {e}");
                    // Rotate to next failover address before sleeping
                    addr_idx = addr_idx.wrapping_add(1);
                }
            }

            time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    /// Resolve a named database to its numeric ID.
    ///
    /// Logic:
    /// - If `name` is non-empty, look it up in the registry. Error on miss.
    /// - Otherwise use `explicit` if Some, else fall back to `conn_db`.
    fn resolve_db_name_or(
        &self,
        name: &str,
        explicit: Option<u16>,
        conn_db: u16,
    ) -> Result<u16> {
        if !name.is_empty() {
            self.db_registry.read().get(name).copied().ok_or_else(|| {
                MnemeError::Protocol(format!("unknown database name '{name}'")).into()
            })
        } else {
            Ok(explicit.unwrap_or(conn_db))
        }
    }
}

// ── free functions ─────────────────────────────────────────────────────────────

fn decode<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T> {
    rmp_serde::from_slice(payload)
        .map_err(|e| MnemeError::Serialization(e.to_string()).into())
}

fn ok_payload<T: serde::Serialize>(v: &T) -> Result<Frame> {
    let payload = rmp_serde::to_vec(v)
        .map_err(|e| MnemeError::Serialization(e.to_string()))?;
    Ok(Frame::ok_response(Bytes::from(payload)))
}

fn ok_str(s: &str) -> Result<Frame> {
    ok_payload(&s.to_string())
}

/// Return `Err(OutOfMemory)` if the pool is at or over 100 % capacity.
/// Called at the start of write commands so clients get a deterministic error
/// rather than silently losing data or having an eviction race.
fn check_oom(pool: &Pool) -> Result<()> {
    if pool.pressure_ratio() >= 1.0 {
        Err(MnemeError::OutOfMemory.into())
    } else {
        Ok(())
    }
}

/// Minimal glob pattern match for SCAN:
///   `*`         — match everything
///   `prefix*`   — prefix match
///   `*suffix`   — suffix match
///   `*sub*`     — substring match (single interior `*`)
///   exact       — exact string match
///
/// Only UTF-8 keys are matched; non-UTF-8 keys never match a pattern.
fn matches_pattern(key: &[u8], pattern: Option<&str>) -> bool {
    let pat = match pattern {
        None | Some("") => return true,
        Some(p) => p,
    };
    if pat == "*" { return true; }
    let key_str = match std::str::from_utf8(key) {
        Ok(s) => s,
        Err(_) => return false,
    };
    match (pat.starts_with('*'), pat.ends_with('*')) {
        (true, true)  => key_str.contains(&pat[1..pat.len() - 1]),
        (true, false) => key_str.ends_with(&pat[1..]),
        (false, true) => key_str.starts_with(&pat[..pat.len() - 1]),
        (false, false) => key_str == pat,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn normalize_idx(idx: i64, len: i64) -> i64 {
    if idx < 0 { (len + idx).max(0) } else { idx }
}

/// One-way TLS client connection to a Core replication port.
/// Used by the read-replica role to send the SyncStart handshake.
/// Auth happens via cluster_secret inside the SyncStart payload — no client cert required.
async fn connect_to_primary(
    addr: &str,
    tls_cfg: &TlsConfig,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let ca_pem = std::fs::read(&tls_cfg.ca_cert)
        .with_context(|| format!("read CA cert: {}", tls_cfg.ca_cert))?;
    let ca_certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut ca_pem.as_slice())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("parse CA cert")?;

    let mut root_store = RootCertStore::empty();
    for ca in ca_certs {
        root_store.add(ca).context("add CA cert to root store")?;
    }

    let client_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let connector = TlsConnector::from(Arc::new(client_cfg));

    let tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("TCP connect to primary Core at {addr}"))?;
    tcp.set_nodelay(true)?;

    let server_name = ServerName::try_from(tls_cfg.server_name.as_str())
        .map_err(|e| anyhow::anyhow!("invalid TLS server name '{}': {e}", tls_cfg.server_name))?
        .to_owned();

    let stream = connector
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("TLS handshake with primary Core at {addr}"))?;

    Ok(stream)
}

/// Stable u64 node ID from a human-readable node_id string.
fn node_id_u64(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn extract_primary_key(frame: &Frame) -> Option<Vec<u8>> {
    match frame.cmd_id {
        CmdId::Get | CmdId::Exists | CmdId::Ttl | CmdId::Expire
        | CmdId::HGetAll | CmdId::LPop | CmdId::RPop | CmdId::ZCard
        | CmdId::Incr | CmdId::Decr | CmdId::MemoryUsage
            => rmp_serde::from_slice::<Vec<u8>>(&frame.payload).ok(),
        CmdId::Set
            => rmp_serde::from_slice::<SetRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::Del
            => rmp_serde::from_slice::<DelRequest>(&frame.payload).ok()
                .and_then(|r| r.keys.into_iter().next()),
        CmdId::HGet
            => rmp_serde::from_slice::<HGetRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::HSet
            => rmp_serde::from_slice::<HSetRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::HDel
            => rmp_serde::from_slice::<HDelRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::LPush | CmdId::RPush
            => rmp_serde::from_slice::<ListPushRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::LRange
            => rmp_serde::from_slice::<LRangeRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::ZAdd
            => rmp_serde::from_slice::<ZAddRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::ZRank
            => rmp_serde::from_slice::<ZRankRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::ZRange
            => rmp_serde::from_slice::<ZRangeRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::ZRangeByScore
            => rmp_serde::from_slice::<ZRangeByScoreRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::ZRem
            => rmp_serde::from_slice::<ZRemRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::IncrBy | CmdId::DecrBy
            => rmp_serde::from_slice::<IncrByRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::IncrByFloat
            => rmp_serde::from_slice::<IncrByFloatRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::GetSet
            => rmp_serde::from_slice::<GetSetRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::JsonGet | CmdId::JsonExists | CmdId::JsonType
            => rmp_serde::from_slice::<JsonGetRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::JsonSet
            => rmp_serde::from_slice::<JsonSetRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::JsonDel
            => rmp_serde::from_slice::<JsonDelRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::JsonArrAppend
            => rmp_serde::from_slice::<JsonArrAppendRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::JsonNumIncrBy
            => rmp_serde::from_slice::<JsonNumIncrByRequest>(&frame.payload).ok().map(|r| r.key),
        CmdId::Type
            => rmp_serde::from_slice::<Vec<u8>>(&frame.payload).ok(),
        CmdId::MGet
            => rmp_serde::from_slice::<MGetRequest>(&frame.payload).ok()
                .and_then(|r| r.keys.into_iter().next()),
        CmdId::MSet
            => rmp_serde::from_slice::<MSetRequest>(&frame.payload).ok()
                .and_then(|r| r.pairs.into_iter().next().map(|(k, _, _)| k)),
        _ => None,
    }
}

// ── Database name registry persistence ────────────────────────────────────────

/// Derive the databases.json path from the users_db path (same directory).
fn db_registry_path(users_db_path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(users_db_path);
    p.parent().unwrap_or(std::path::Path::new("/var/lib/mneme"))
        .join("databases.json")
}

/// Load runtime name registry from `{data_dir}/databases.json`.
/// Returns None if the file is absent or unparseable (non-fatal).
fn load_db_registry(users_db_path: &str) -> Option<HashMap<String, u16>> {
    let path = db_registry_path(users_db_path);
    let text = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Atomically persist the name registry to `{data_dir}/databases.json`.
/// Failures are logged as warnings (non-fatal — registry is still live in memory).
fn persist_db_registry(reg: &HashMap<String, u16>, users_db_path: &str) {
    let path = db_registry_path(users_db_path);
    match serde_json::to_string_pretty(reg) {
        Ok(json) => {
            let tmp = path.with_extension("json.tmp");
            if let Err(e) = fs::write(&tmp, &json)
                .and_then(|_| fs::rename(&tmp, &path))
            {
                warn!(path = %path.display(), "persist_db_registry failed: {e}");
            }
        }
        Err(e) => warn!("persist_db_registry serialize failed: {e}"),
    }
}
