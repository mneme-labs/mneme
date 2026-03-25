// Hypnos — Keeper node.
// TODO(io_uring): not yet implemented — all I/O uses tokio epoll. io_uring planned for future.
// Wires Aoide (WAL) + Melete (snapshot) + Oneiros (cold store).
// Accepts replication frames from Mnemosyne, persists writes, serves cold fetches.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use mneme_common::config::TlsConfig;
use mneme_common::{CmdId, Entry, Frame, HeartbeatPayload, MnemeConfig, PushKeyPayload,
                   RegisterPayload, SyncCompletePayload};
use parking_lot::Mutex;
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::signal;
use tokio::time;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{debug, error, info, warn};

use super::aoide::Aoide;
use super::melete::Melete;
use super::oneiros::Oneiros;

const FRAME_HEADER: usize = mneme_common::HEADER_LEN; // 16B: magic+ver+cmd+flags+plen+req_id

/// Sender type for the embedded solo-mode keeper channel.
/// Carries (Frame, ack_tx) pairs identical to KeeperHandle in moirai.rs.
#[allow(dead_code)]
pub type EmbeddedSender = tokio::sync::mpsc::Sender<(Frame, tokio::sync::mpsc::Sender<anyhow::Result<()>>)>;

struct HypnosState {
    aoide: Aoide,
    oneiros: Oneiros,
    /// Highest replicated sequence number seen.
    last_seq: u64,
}

pub struct Hypnos {
    state: Arc<Mutex<HypnosState>>,
    snap_path: PathBuf,
    snap_interval: Duration,
    config: MnemeConfig,
}

impl Hypnos {
    pub async fn start(config: MnemeConfig) -> Result<()> {
        let wal_path  = config.persistence.wal_path();
        let snap_path = PathBuf::from(config.persistence.snap_path());
        let cold_path = config.persistence.cold_db_path();

        // Ensure WAL directory exists
        std::fs::create_dir_all(&config.persistence.wal_dir)
            .context("create WAL dir")?;

        // Open subsystems
        let aoide   = Aoide::open(&wal_path, config.persistence.wal_max_bytes())
            .context("Aoide open")?;
        let oneiros = Oneiros::open(&cold_path).context("Oneiros open")?;

        // Replay snapshot → cold store (initial warm-up)
        if Melete::exists(&snap_path) {
            info!("Replaying snapshot");
            let (entries, _ts) = Melete::load(&snap_path).context("Melete load")?;
            // B-07: use set_batch for single-transaction replay (much faster for large snapshots)
            let batch: Vec<(Vec<u8>, mneme_common::Value, u64, u16)> = entries
                .into_iter()
                .map(|se| (se.key, se.value, se.expires_at_ms, se.slot))
                .collect();
            oneiros.set_batch(&batch).context("snapshot replay batch")?;
        }

        // Replay WAL on top of snapshot.
        // expires_at_ms is now stored in each WAL record (v2 format) so TTLs
        // survive Keeper restarts.  Keys that expired during downtime are
        // replayed with their original expires_at_ms; they will be skipped
        // during the next warm-up push to Core and purged from disk then.
        let mut last_seq: u64 = 0;
        {
            Aoide::replay(&wal_path, |seq, expires_at_ms, key, value| {
                let slot = mneme_common::frame::slot_from_key(&key);
                oneiros.set(&key, &value, expires_at_ms, slot)?;
                last_seq = last_seq.max(seq);
                Ok(())
            })
            .context("WAL replay")?;
        }
        info!(last_seq, "WAL replay complete");

        let state = Arc::new(Mutex::new(HypnosState {
            aoide,
            oneiros,
            last_seq,
        }));

        let node = Arc::new(Self {
            state,
            snap_path: snap_path.clone(),
            snap_interval: Duration::from_secs(config.persistence.snapshot_interval_s),
            config: config.clone(),
        });

        // Snapshot background task
        {
            let node2 = node.clone();
            tokio::spawn(async move {
                let mut interval = time::interval(node2.snap_interval);
                loop {
                    interval.tick().await;
                    if let Err(e) = node2.take_snapshot().await {
                        error!("snapshot error: {e}");
                    }
                }
            });
        }

        // WAL rotation watcher
        {
            let node2 = node.clone();
            tokio::spawn(async move {
                let mut interval = time::interval(Duration::from_secs(10));
                loop {
                    interval.tick().await;
                    let needs_rotation = node2.state.lock().aoide.needs_rotation(0.90);
                    if needs_rotation {
                        if let Err(e) = node2.state.lock().aoide.rotate() {
                            error!("WAL rotation error: {e}");
                        }
                    }
                }
            });
        }

        // ── Outbound connection to Core ────────────────────────────────────────
        // The Keeper connects TO the Core's replication port, sends SyncStart
        // (with cluster_secret), then receives PushKey / Heartbeat frames.
        // Reconnects automatically with exponential back-off on disconnect.
        if !config.node.core_addr.is_empty() {
            let node2          = node.clone();
            let core_addr      = config.node.core_addr.clone();
            let cluster_secret = config.auth.cluster_secret.clone();
            let tls_cfg        = config.tls.clone();

            // A-03 sub-problem 1 fix: call register_with_core() from mneme_common::herold_client
            // before the SyncStart reconnect loop.  This announces the keeper's replication
            // address and validates the join_token with Core in a one-shot handshake.
            // Core will then accept the subsequent SyncStart connection for warm-up/replication.
            // Errors here are non-fatal: the SyncStart path (which also carries replication_addr)
            // will still register the keeper correctly even if REGISTER is skipped.
            {
                let reg_payload = RegisterPayload {
                    node_id:          config.node.node_id.clone(),
                    role:             "keeper".into(),
                    grant_bytes:      0,   // keepers do not advertise a RAM grant
                    replication_addr: config.replication_addr(),
                    join_token:       config.node.join_token.clone(),
                };
                let reg_addr = core_addr.clone();
                let reg_tls  = tls_cfg.clone();
                tokio::spawn(async move {
                    match mneme_common::herold_client::register_with_core(
                        &reg_addr, reg_payload, &reg_tls,
                    ).await {
                        Ok(ack) => info!(
                            assigned_id = ack.assigned_id,
                            "Herold REGISTER: accepted by Core"
                        ),
                        Err(e) => warn!(
                            "Herold REGISTER: failed (SyncStart will still establish replication): {e}"
                        ),
                    }
                });
            }

            tokio::spawn(async move {
                let mut backoff = Duration::from_secs(1);
                let mut consecutive_failures: u32 = 0;
                loop {
                    info!(%core_addr, "Hypnos: connecting to Core replication port");
                    match connect_to_core(&core_addr, &tls_cfg).await {
                        Ok(stream) => {
                            backoff = Duration::from_secs(1); // reset on success
                            consecutive_failures = 0;
                            let peer: SocketAddr = core_addr
                                .parse()
                                .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
                            if let Err(e) = node2.clone()
                                .run_replication_client(stream, peer, cluster_secret.clone())
                                .await
                            {
                                warn!(%core_addr, "Replication disconnected: {e}");
                            }
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            if consecutive_failures >= 10 {
                                error!(%core_addr, backoff_s = backoff.as_secs(),
                                       failures = consecutive_failures,
                                       "Hypnos: persistent failure connecting to Core: {e}");
                            } else {
                                warn!(%core_addr, backoff_s = backoff.as_secs(),
                                      "Hypnos: failed to connect to Core: {e}");
                            }
                        }
                    }
                    time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            });
        } else {
            warn!("node.core_addr is empty — this keeper will NOT connect to any Core. \
                   Set core_addr in [node] section of /etc/mneme/mneme.toml.");
        }

        // Replication TLS listener (inbound mTLS connections from Core's Hermes)
        let rep_addr: SocketAddr = config.replication_addr().parse()?;
        let listener = TcpListener::bind(rep_addr).await?;

        // Build TLS server config from the node cert/key (copied from Core's shared volume).
        // Core connects with mTLS client auth; we accept with server-side TLS only —
        // no_client_auth means we don't require the client cert, but Hermes still sends it.
        let tls_acceptor: Option<TlsAcceptor> = {
            let cert_path = &config.tls.cert;
            let key_path  = &config.tls.key;
            if std::path::Path::new(cert_path).exists() && std::path::Path::new(key_path).exists() {
                let cert_pem = std::fs::read(cert_path)
                    .with_context(|| format!("read keeper cert: {cert_path}"))?;
                let key_pem = std::fs::read(key_path)
                    .with_context(|| format!("read keeper key: {key_path}"))?;
                let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
                    rustls_pemfile::certs(&mut cert_pem.as_slice())
                        .collect::<std::result::Result<Vec<_>, _>>()
                        .context("parse keeper cert")?;
                let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
                    .context("read keeper key")?
                    .context("no private key in keeper key file")?;
                let server_cfg = rustls::ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(certs, key)
                    .context("build keeper TLS server config")?;
                info!(%rep_addr, "Hypnos listening for inbound replication (TLS)");
                Some(TlsAcceptor::from(Arc::new(server_cfg)))
            } else {
                info!(%rep_addr, "Hypnos listening for inbound replication (plain TCP — no cert)");
                None
            }
        };

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, peer)) => {
                            let node2 = node.clone();
                            let acceptor = tls_acceptor.clone();
                            tokio::spawn(async move {
                                if let Some(acc) = acceptor {
                                    match acc.accept(stream).await {
                                        Ok(tls_stream) => {
                                            if let Err(e) = node2.handle_replication(tls_stream, peer).await {
                                                warn!(%peer, "replication conn error: {e}");
                                            }
                                        }
                                        Err(e) => warn!(%peer, "TLS handshake failed: {e}"),
                                    }
                                } else if let Err(e) = node2.handle_replication(stream, peer).await {
                                    warn!(%peer, "replication conn error: {e}");
                                }
                            });
                        }
                        Err(e) => error!("replication accept: {e}"),
                    }
                }
                _ = signal::ctrl_c() => {
                    info!("Hypnos shutting down — taking final snapshot");
                    let _ = node.take_snapshot().await;
                    break;
                }
            }
        }
        Ok(())
    }

    /// Start an in-process keeper (solo mode).
    /// Returns (node_id, sender). The sender is compatible with moirai::KeeperHandle.
    /// QUORUM/ALL → EVENTUAL in Moirai, so frames arrive fire-and-forget.
    #[allow(dead_code)]
    /// Start an embedded keeper for solo mode.
    /// Returns (node_id, channel_sender, recovered_entries).
    /// `recovered_entries`: Vec<(key_bytes, Value, expires_at_ms, slot)> read from
    /// the cold store after WAL replay.  Mnemosyne uses these to restore its RAM pool.
    pub async fn start_embedded(
        config: MnemeConfig,
    ) -> Result<(u64, EmbeddedSender, Vec<(Vec<u8>, mneme_common::Value, u64, u16)>)> {
        let node_id  = node_id_u64(&config.node.node_id);
        let wal_path  = config.persistence.wal_path();
        let snap_path = PathBuf::from(config.persistence.snap_path());
        let cold_path = config.persistence.cold_db_path();

        std::fs::create_dir_all(&config.persistence.wal_dir)
            .context("create WAL dir (embedded)")?;

        // Diagnostic: log file existence and sizes before opening
        for (label, p) in [("WAL", wal_path.as_str()), ("snap", snap_path.to_str().unwrap_or("")), ("cold", cold_path.as_str())] {
            match std::fs::metadata(p) {
                Ok(m) => info!(file = p, size = m.len(), "{label} file exists"),
                Err(_) => info!(file = p, "{label} file does not exist"),
            }
        }

        let aoide   = Aoide::open(&wal_path, config.persistence.wal_max_bytes())
            .context("Aoide open (embedded)")?;
        let oneiros = Oneiros::open(&cold_path).context("Oneiros open (embedded)")?;

        if Melete::exists(&snap_path) {
            let (entries, _) = Melete::load(&snap_path).context("Melete load (embedded)")?;
            // B-07: use set_batch for single-transaction replay (much faster for large snapshots)
            let batch: Vec<(Vec<u8>, mneme_common::Value, u64, u16)> = entries
                .into_iter()
                .map(|se| (se.key, se.value, se.expires_at_ms, se.slot))
                .collect();
            oneiros.set_batch(&batch).context("snapshot replay batch (embedded)")?;
        }

        let mut last_seq: u64 = 0;
        {
            Aoide::replay(&wal_path, |seq, expires_at_ms, key, value| {
                let slot = mneme_common::frame::slot_from_key(&key);
                oneiros.set(&key, &value, expires_at_ms, slot)?;
                last_seq = last_seq.max(seq);
                Ok(())
            })
            .context("WAL replay (embedded)")?;
        }
        // Scan Oneiros to collect all persisted entries for Mnemosyne pool restore.
        let mut recovered: Vec<(Vec<u8>, mneme_common::Value, u64, u16)> = Vec::new();
        oneiros.scan(|key, value, expires_at_ms, slot| {
            recovered.push((key, value, expires_at_ms, slot));
            Ok(())
        }).context("Oneiros scan for pool restore")?;

        info!(node_id, last_seq, recovered_keys = recovered.len(), "Embedded keeper initialized");

        let state = Arc::new(Mutex::new(HypnosState { aoide, oneiros, last_seq }));
        let snap_interval = Duration::from_secs(config.persistence.snapshot_interval_s);

        // Snapshot task
        // NOTE (B-06): The embedded keeper uses a single scan under the lock here because it
        // runs in solo mode (no separate replication goroutine competing for the lock).
        // If the embedded keeper is ever extended to run concurrent replication, replace this
        // with the same two-phase list_keys/get_with_meta approach used in take_snapshot().
        {
            let st = state.clone();
            let sp = snap_path.clone();
            tokio::spawn(async move {
                let mut interval = time::interval(snap_interval);
                loop {
                    interval.tick().await;
                    let entries: anyhow::Result<Vec<_>> = {
                        let g = st.lock();
                        let mut out = Vec::new();
                        let r = g.oneiros.scan(|key, value, expires_at_ms, slot| {
                            let mut e = Entry::new(value, slot);
                            e.expires_at_ms = expires_at_ms;
                            out.push((key, e));
                            Ok(())
                        });
                        r.map(|_| out)
                    };
                    match entries {
                        Ok(v) => {
                            if let Err(e) = Melete::save(v, &sp) {
                                error!("embedded snapshot error: {e}");
                            }
                        }
                        Err(e) => error!("embedded scan error: {e}"),
                    }
                }
            });
        }

        // Frame processing channel
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(Frame, tokio::sync::mpsc::Sender<anyhow::Result<()>>)>(256);
        {
            let st = state.clone();
            tokio::spawn(async move {
                while let Some((frame, ack_tx)) = rx.recv().await {
                    let result = process_embedded_frame(&st, frame);
                    if let Err(ref e) = result {
                        error!("Embedded frame processing error: {e:#}");
                    }
                    let _ = ack_tx.send(result).await;
                }
            });
        }

        Ok((node_id, tx, recovered))
    }

    // ── replication handler ───────────────────────────────────────────────────

    /// Handle an inbound connection FROM Core's Hermes.
    /// Core connects here to send REPLICATE/PushKey frames.
    /// We do NOT send SyncStart — Core is the initiator on this channel.
    async fn handle_replication<S>(
        self: Arc<Self>,
        mut stream: S,
        peer: SocketAddr,
    ) -> Result<()>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let mut buf = BytesMut::with_capacity(4096);
        info!(%peer, "Replication connection established");

        loop {
            loop {
                if buf.len() >= FRAME_HEADER {
                    let payload_len =
                        u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                    if buf.len() >= FRAME_HEADER + payload_len {
                        break;
                    }
                }
                let n = stream.read_buf(&mut buf).await?;
                if n == 0 {
                    info!(%peer, "Replication connection closed");
                    return Ok(());
                }
            }

            let (frame, consumed) =
                Frame::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
            buf_advance(&mut buf, consumed);

            let resp = self.handle_frame(frame).await;
            let ack = match resp {
                Ok(seq) => {
                    let payload =
                        rmp_serde::to_vec(&mneme_common::AckPayload {
                            seq,
                            node_id: node_id_u64(&self.config.node.node_id),
                            ok: true,
                        })
                        .unwrap_or_default();
                    Frame {
                        cmd_id: CmdId::AckWrite,
                        flags: 0,
                        req_id: 0,
                        payload: Bytes::from(payload),
                    }
                }
                Err(e) => {
                    warn!(%peer, "frame error: {e}");
                    Frame::error_response(&e.to_string())
                }
            };
            stream.write_all(&ack.encode()).await?;
        }
    }

    async fn handle_frame(&self, frame: Frame) -> Result<u64> {
        match frame.cmd_id {
            CmdId::PushKey => {
                let push: PushKeyPayload = rmp_serde::from_slice(&frame.payload)
                    .map_err(|e| anyhow::anyhow!("PushKey deserialize: {e}"))?;

                let mut g = self.state.lock();
                if push.deleted {
                    // Tombstone: remove from cold store
                    g.oneiros.del(&push.key)?;
                    // Seq still advances
                    g.last_seq = g.last_seq.max(push.seq);
                } else {
                    let slot = mneme_common::frame::slot_from_key(&push.key);
                    let expires_at_ms = if push.ttl_ms > 0 {
                        now_ms() + push.ttl_ms
                    } else {
                        0
                    };
                    // Write to WAL first — include expires_at_ms so TTL survives
                    // Keeper restarts (v2 WAL format stores absolute expiry).
                    g.aoide.append(push.seq, expires_at_ms, &push.key, &push.value)?;
                    g.aoide.flush()?;
                    // Write to cold store
                    g.oneiros.set(&push.key, &push.value, expires_at_ms, slot)?;
                    g.last_seq = g.last_seq.max(push.seq);
                }
                Ok(push.seq)
            }

            CmdId::Heartbeat => {
                // ACK and send back our stats
                Ok(0)
            }

            CmdId::SyncRequest => {
                // Mnemosyne requests a full sync — stream all cold store keys back
                // This is a one-shot bulk push; actual streaming is done via PushKey frames.
                // For now, we ACK and rely on the keeper to emit PushKey per key.
                Ok(0)
            }

            CmdId::SyncStart => {
                // Mnemosyne echoing back our SyncStart — just ACK
                Ok(0)
            }

            other => {
                anyhow::bail!("unexpected replication cmd: {:?}", other);
            }
        }
    }

    // ── outbound replication client ───────────────────────────────────────────

    /// Drive an outbound replication session with Core.
    /// Sends SyncStart (with cluster_secret + key_count), then pushes all cold
    /// store keys to Core (warm-up phase), then enters the normal receive loop.
    async fn run_replication_client<S>(
        self: Arc<Self>,
        mut stream: S,
        peer: SocketAddr,
        cluster_secret: String,
    ) -> Result<()>
    where
        S: AsyncReadExt + AsyncWriteExt + Unpin,
    {
        let node_id = node_id_u64(&self.config.node.node_id);

        // Count live keys in Oneiros so Core knows how many PushKey frames to expect.
        let key_count = {
            let g = self.state.lock();
            let mut count = 0u64;
            let _ = g.oneiros.scan(|_, _, _, _| { count += 1; Ok(()) });
            count
        };

        // Send SyncStart with cluster_secret, highest_seq, and key_count.
        {
            let last_seq = self.state.lock().last_seq;
            let sync_payload = mneme_common::SyncStartPayload {
                node_id,
                highest_seq:    last_seq,
                cluster_secret,
                key_count,
                version:        1,
                replication_addr: self.config.replication_addr(),
                node_name:      self.config.node.node_id.clone(),
                pool_bytes:     self.config.memory.pool_bytes_u64(),
            };
            let payload_bytes = rmp_serde::to_vec(&sync_payload)
                .map_err(|e| anyhow::anyhow!("SyncStart serialize: {e}"))?;
            let sync_frame = Frame {
                cmd_id: CmdId::SyncStart,
                flags:  0,
                req_id: 0,
                payload: Bytes::from(payload_bytes),
            };
            stream.write_all(&sync_frame.encode()).await?;
            info!(%peer, key_count, "Hypnos: SyncStart sent to Core");
        }

        // Wait for Core to ACK the SyncStart before pushing keys.
        {
            let mut buf = BytesMut::with_capacity(256);
            loop {
                if buf.len() >= FRAME_HEADER {
                    let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                    if buf.len() >= FRAME_HEADER + plen { break; }
                }
                let n = stream.read_buf(&mut buf).await?;
                if n == 0 { return Ok(()); }
            }
            // Consume the ACK frame (we don't need to parse it)
        }

        // ── Warm-up push phase ──────────────────────────────────────────────
        // Push all cold store keys to Core so it can warm its RAM pool.
        // Single O(N) scan: collect all (key, value, expires_at_ms, slot) tuples
        // under one lock acquisition, then release the lock before network I/O.
        // The previous O(N²) approach (list_keys then scan-per-key) was replaced
        // because it held the mutex across the full cold store for each key.
        // Purge keys that expired during Core downtime before counting / pushing.
        // This ensures the key_count in SyncStart matches what we actually push,
        // and frees disk space from keys that will never be read again.
        {
            let g = self.state.lock();
            match g.oneiros.purge_expired() {
                Ok(n) if n > 0 => info!(%peer, purged = n, "Hypnos: purged expired keys before warm-up push"),
                Err(e) => warn!(%peer, "Hypnos: purge_expired failed (non-fatal): {e}"),
                _ => {}
            }
        }

        info!(%peer, key_count, "Hypnos: starting warm-up push to Core");
        let mut pushed = 0u64;
        {
            type Entry = (Vec<u8>, mneme_common::Value, u64, u16);
            let entries: Vec<Entry> = {
                let g = self.state.lock();
                let mut es: Vec<Entry> = Vec::new();
                let _ = g.oneiros.scan(|key, value, expires_at_ms, slot| {
                    es.push((key, value, expires_at_ms, slot));
                    Ok(())
                });
                es
            };
            // Lock is released — network I/O proceeds without holding the mutex.

            let now = now_ms();
            for (key, value, expires_at_ms, slot) in entries {
                // Skip any key that slipped through purge_expired (race with clock).
                // Also delete it from disk so it doesn't accumulate across restarts.
                if expires_at_ms > 0 && now >= expires_at_ms {
                    let g = self.state.lock();
                    let _ = g.oneiros.del(&key);
                    continue;
                }
                let ttl_ms = if expires_at_ms > 0 {
                    expires_at_ms.saturating_sub(now)
                } else {
                    0
                };

                let push = PushKeyPayload {
                    key,
                    value,
                    seq: 0,
                    ttl_ms,
                    slot,
                    deleted: false,
                    db_id: 0,  // db prefix is already embedded in the raw key bytes
                };
                let payload = rmp_serde::to_vec(&push)
                    .map_err(|e| anyhow::anyhow!("PushKey serialize: {e}"))?;
                let push_frame = Frame {
                    cmd_id: CmdId::PushKey,
                    flags:  0,
                    req_id: 0,
                    payload: Bytes::from(payload),
                };
                stream.write_all(&push_frame.encode()).await?;

                // Read ACK from Core
                let mut ack_buf = BytesMut::with_capacity(64);
                loop {
                    if ack_buf.len() >= FRAME_HEADER {
                        let plen = u32::from_be_bytes(ack_buf[8..12].try_into().unwrap()) as usize;
                        if ack_buf.len() >= FRAME_HEADER + plen { break; }
                    }
                    let n = stream.read_buf(&mut ack_buf).await?;
                    if n == 0 { return Ok(()); }
                }
                pushed += 1;
            }
        }
        info!(%peer, pushed, "Hypnos: warm-up push complete — sending SyncComplete");

        // Send SyncComplete so Core knows this keeper's warm-up is done.
        {
            let highest_seq = self.state.lock().last_seq;
            let complete = SyncCompletePayload {
                node_id,
                pushed_keys: pushed,
                highest_seq,
            };
            let payload = rmp_serde::to_vec(&complete)
                .map_err(|e| anyhow::anyhow!("SyncComplete serialize: {e}"))?;
            let complete_frame = Frame {
                cmd_id: CmdId::SyncComplete,
                flags:  0,
                req_id: 0,
                payload: Bytes::from(payload),
            };
            stream.write_all(&complete_frame.encode()).await?;

            // Read Core's ACK to SyncComplete
            let mut ack_buf = BytesMut::with_capacity(64);
            loop {
                if ack_buf.len() >= FRAME_HEADER {
                    let plen = u32::from_be_bytes(ack_buf[8..12].try_into().unwrap()) as usize;
                    if ack_buf.len() >= FRAME_HEADER + plen { break; }
                }
                let n = stream.read_buf(&mut ack_buf).await?;
                if n == 0 { return Ok(()); }
            }
        }

        // ── Normal heartbeat loop ───────────────────────────────────────────
        // Receive frames from Core; send periodic heartbeats with keeper stats.
        let mut buf = BytesMut::with_capacity(4096);
        const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
        let mut next_heartbeat = std::time::Instant::now() + HEARTBEAT_INTERVAL;

        loop {
            // How long until next heartbeat?
            let timeout_dur = next_heartbeat
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or(Duration::ZERO);

            // Read one byte at a time with a deadline, then check for full frame
            let read_timed = time::timeout(timeout_dur, stream.read_buf(&mut buf)).await;

            match read_timed {
                Err(_elapsed) => {
                    // Heartbeat due: send keeper stats to Core
                    let (key_count, wal_bytes) = {
                        let g = self.state.lock();
                        let mut count = 0u64;
                        let _ = g.oneiros.scan(|_, _, _, _| { count += 1; Ok(()) });
                        let wal_offset = g.aoide.offset();
                        (count, wal_offset)
                    };
                    let hb = HeartbeatPayload {
                        node_id,
                        // Keepers have no RAM pool.  Re-purpose pool_bytes to report
                        // WAL bytes written (disk usage) so keeper-list shows useful
                        // information instead of "0 B / 0 B".
                        pool_bytes: wal_bytes,
                        // used_bytes = approximate cold-store footprint (128 B/key avg).
                        used_bytes: key_count * 128,
                        key_count,
                    };
                    if let Ok(payload) = rmp_serde::to_vec(&hb) {
                        let hb_frame = Frame {
                            cmd_id: CmdId::Heartbeat,
                            flags:  0,
                            req_id: 0,
                            payload: Bytes::from(payload),
                        };
                        stream.write_all(&hb_frame.encode()).await?;
                    }
                    next_heartbeat = std::time::Instant::now() + HEARTBEAT_INTERVAL;
                    continue;
                }
                Ok(Ok(0)) => {
                    info!(%peer, "Core closed replication connection");
                    return Ok(());
                }
                Ok(Err(e)) => {
                    return Err(e.into());
                }
                Ok(Ok(_)) => {} // data appended to buf; check for complete frame below
            }

            // Try to decode complete frames from buf (may have accumulated multiple)
            loop {
                if buf.len() < FRAME_HEADER { break; }
                let payload_len = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
                if buf.len() < FRAME_HEADER + payload_len { break; }

                let (frame, consumed) =
                    Frame::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
                buf_advance(&mut buf, consumed);

                // Ignore ACKs to our heartbeats (CmdId::Ok)
                if frame.cmd_id == CmdId::Ok {
                    continue;
                }

                let resp = self.handle_frame(frame).await;
                let ack = match resp {
                    Ok(seq) => {
                        let payload =
                            rmp_serde::to_vec(&mneme_common::AckPayload {
                                seq,
                                node_id,
                                ok: true,
                            })
                            .unwrap_or_default();
                        Frame {
                            cmd_id: CmdId::AckWrite,
                            flags:  0,
                            req_id: 0,
                            payload: Bytes::from(payload),
                        }
                    }
                    Err(e) => {
                        warn!(%peer, "client frame error: {e}");
                        Frame::error_response(&e.to_string())
                    }
                };
                stream.write_all(&ack.encode()).await?;
            }
        }
    }

    // ── snapshot ──────────────────────────────────────────────────────────────

    /// B-06: two-phase snapshot to avoid holding the global lock across a full Oneiros scan.
    /// Phase 1 — collect the key list under a short lock, then release immediately.
    /// Phase 2 — read each value with a per-key short lock so replication can proceed between reads.
    async fn take_snapshot(&self) -> Result<()> {
        // Phase 1: collect key list under a short lock, then release.
        let keys: Vec<Vec<u8>> = {
            let g = self.state.lock();
            g.oneiros.list_keys()?
        };

        // Phase 2: read each value without holding the global lock.
        let mut entries = Vec::with_capacity(keys.len());
        for key in &keys {
            // Short lock per key — allows replication to proceed between reads.
            let meta_opt = {
                let g = self.state.lock();
                g.oneiros.get_with_meta(key)?
            };
            if let Some((value, expires_at_ms, slot)) = meta_opt {
                let mut entry = Entry::new(value, slot);
                entry.expires_at_ms = expires_at_ms;
                entries.push((key.clone(), entry));
            }
        }

        let count = entries.len();
        super::melete::Melete::save(entries, &self.snap_path).context("Melete save")?;
        info!(count, "Snapshot taken");
        Ok(())
    }
}

#[allow(dead_code)]
fn process_embedded_frame(state: &Arc<Mutex<HypnosState>>, frame: Frame) -> anyhow::Result<()> {
    match frame.cmd_id {
        CmdId::PushKey => {
            let push: PushKeyPayload = rmp_serde::from_slice(&frame.payload)
                .map_err(|e| anyhow::anyhow!("PushKey deserialize: {e}"))?;
            let mut g = state.lock();
            if push.deleted {
                debug!(key = %String::from_utf8_lossy(&push.key), "Embedded: PushKey delete");
                g.oneiros.del(&push.key)?;
            } else {
                let slot = mneme_common::frame::slot_from_key(&push.key);
                let expires_at_ms = if push.ttl_ms > 0 { now_ms() + push.ttl_ms } else { 0 };
                debug!(
                    key = %String::from_utf8_lossy(&push.key),
                    seq = push.seq,
                    expires_at_ms,
                    "Embedded: PushKey write to WAL + Oneiros"
                );
                g.aoide.append(push.seq, expires_at_ms, &push.key, &push.value)?;
                g.aoide.flush()?;
                g.oneiros.set(&push.key, &push.value, expires_at_ms, slot)?;
            }
            g.last_seq = g.last_seq.max(push.seq);
            Ok(())
        }
        CmdId::Heartbeat => Ok(()),
        _ => Ok(()),
    }
}

fn node_id_u64(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn buf_advance(buf: &mut BytesMut, n: usize) {
    let _ = buf.split_to(n);
}

/// Open a TLS client connection to the Core's replication port.
///
/// Connects TCP to `addr`, then performs a TLS handshake.  The server name
/// sent in the SNI extension (and used for cert verification) is taken from
/// `tls_cfg.server_name` so it can be overridden per-deployment without
/// recompiling.  The TCP connection itself is made to whatever IP:port is in
/// `addr`.  (A-03 fix: was previously hardcoded to "mneme.local".)
async fn connect_to_core(
    addr:    &str,
    tls_cfg: &TlsConfig,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    // Build a one-way TLS client config (no client cert — Core uses plain TLS
    // on the replication port, auth is via cluster_secret in SyncStart).
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

    // TCP connection goes to the real IP:port; TLS SNI/verification uses
    // tls_cfg.server_name which must match the SAN in the Core's certificate.
    let tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("TCP connect to Core at {addr}"))?;

    let server_name = ServerName::try_from(tls_cfg.server_name.as_str())
        .map_err(|e| anyhow::anyhow!("invalid server name: {e}"))?
        .to_owned();

    let stream = connector
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("TLS handshake with Core at {addr}"))?;

    Ok(stream)
}
