// Themis — Raft-based leader election + log replication.
// Uses openraft with a real mTLS transport (RaftTransport) for Core-to-Core RPCs.
// Single-core mode: bootstraps as solo leader. Multi-core: Raft cluster with
// automatic leader election and failover.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::Arc;

use anyhow::Result;
use openraft::{
    BasicNode, Entry, LogId, RaftMetrics, RaftTypeConfig,
    Snapshot, SnapshotMeta, StorageError, Vote,
};
use openraft::storage::LogState;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::info;

use super::raft_transport::RaftTransport;
use crate::net::aegis::Aegis;

// ── Type config ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MnemeNodeId(pub u64);

impl Display for MnemeNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node-{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MnemeRequest {
    Noop,
    /// Replicated write command. `frame_bytes` is the encoded Frame.
    Write { frame_bytes: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MnemeResponse {
    pub value: Option<String>,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct TypeConfig;

impl RaftTypeConfig for TypeConfig {
    type D = MnemeRequest;
    type R = MnemeResponse;
    type NodeId = MnemeNodeId;
    type Node = BasicNode;
    type Entry = Entry<Self>;
    type SnapshotData = Cursor<Vec<u8>>;
    type AsyncRuntime = openraft::TokioRuntime;
    type Responder = openraft::impls::OneshotResponder<Self>;
}

// ── In-memory storage ─────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct MemStore {
    last_purged_log_id: RwLock<Option<LogId<MnemeNodeId>>>,
    log: RwLock<BTreeMap<u64, Entry<TypeConfig>>>,
    #[allow(dead_code)]
    committed: RwLock<Option<LogId<MnemeNodeId>>>,
    vote: RwLock<Option<Vote<MnemeNodeId>>>,
    snapshot: RwLock<Option<StoredSnapshot>>,
    state_machine: RwLock<StateMachine>,
    /// Channel to forward committed entries to Mnemosyne for RAM pool application.
    pub apply_tx: Option<mpsc::Sender<MnemeRequest>>,
}

impl Default for MemStore {
    fn default() -> Self {
        Self {
            last_purged_log_id: RwLock::new(None),
            log: RwLock::new(BTreeMap::new()),
            committed: RwLock::new(None),
            vote: RwLock::new(None),
            snapshot: RwLock::new(None),
            state_machine: RwLock::new(StateMachine::default()),
            apply_tx: None,
        }
    }
}

#[derive(Debug, Default)]
struct StateMachine {
    last_applied: Option<LogId<MnemeNodeId>>,
    last_membership: openraft::StoredMembership<MnemeNodeId, BasicNode>,
}

#[derive(Debug)]
struct StoredSnapshot {
    meta: SnapshotMeta<MnemeNodeId, BasicNode>,
    data: Vec<u8>,
}

// ── RaftLogReader impl ────────────────────────────────────────────────────────

impl openraft::RaftLogReader<TypeConfig> for Arc<tokio::sync::Mutex<MemStore>> {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + std::fmt::Debug + openraft::OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        let entries: Vec<_> = store
            .log
            .read()
            .range(range)
            .map(|(_, e)| e.clone())
            .collect();
        Ok(entries)
    }
}

// ── RaftSnapshotBuilder impl ──────────────────────────────────────────────────

impl openraft::RaftSnapshotBuilder<TypeConfig> for Arc<tokio::sync::Mutex<MemStore>> {
    async fn build_snapshot(
        &mut self,
    ) -> Result<Snapshot<TypeConfig>, StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        let data = store
            .snapshot
            .read()
            .as_ref()
            .map(|s| s.data.clone())
            .unwrap_or_default();
        let meta = store
            .snapshot
            .read()
            .as_ref()
            .map(|s| s.meta.clone())
            .unwrap_or_default();
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

// ── RaftStorage impl ──────────────────────────────────────────────────────────

impl openraft::RaftStorage<TypeConfig> for Arc<tokio::sync::Mutex<MemStore>> {
    type LogReader = Self;
    type SnapshotBuilder = Self;

    async fn get_log_state(
        &mut self,
    ) -> Result<LogState<TypeConfig>, StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        let last_purged = store.last_purged_log_id.read().clone();
        let last = store
            .log
            .read()
            .iter()
            .next_back()
            .map(|(_, e)| e.log_id.clone());
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id: last,
        })
    }

    async fn save_vote(
        &mut self,
        vote: &Vote<MnemeNodeId>,
    ) -> Result<(), StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        *store.vote.write() = Some(vote.clone());
        Ok(())
    }

    async fn read_vote(
        &mut self,
    ) -> Result<Option<Vote<MnemeNodeId>>, StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        let v = store.vote.read().clone();
        Ok(v)
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn append_to_log<I>(
        &mut self,
        entries: I,
    ) -> Result<(), StorageError<MnemeNodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
    {
        let store = self.lock().await;
        let mut log = store.log.write();
        for e in entries {
            log.insert(e.log_id.index, e);
        }
        Ok(())
    }

    async fn delete_conflict_logs_since(
        &mut self,
        log_id: LogId<MnemeNodeId>,
    ) -> Result<(), StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        store.log.write().retain(|&k, _| k < log_id.index);
        Ok(())
    }

    async fn purge_logs_upto(
        &mut self,
        log_id: LogId<MnemeNodeId>,
    ) -> Result<(), StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        *store.last_purged_log_id.write() = Some(log_id.clone());
        store.log.write().retain(|&k, _| k > log_id.index);
        Ok(())
    }

    async fn last_applied_state(
        &mut self,
    ) -> Result<
        (
            Option<LogId<MnemeNodeId>>,
            openraft::StoredMembership<MnemeNodeId, BasicNode>,
        ),
        StorageError<MnemeNodeId>,
    > {
        let store = self.lock().await;
        let sm = store.state_machine.read();
        Ok((sm.last_applied.clone(), sm.last_membership.clone()))
    }

    async fn apply_to_state_machine(
        &mut self,
        entries: &[Entry<TypeConfig>],
    ) -> Result<Vec<MnemeResponse>, StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        let mut sm = store.state_machine.write();
        let mut replies = Vec::new();
        for e in entries {
            sm.last_applied = Some(e.log_id.clone());
            // Committed writes are applied via the apply_tx channel in Mnemosyne.
            if let Some(apply_tx) = store.apply_tx.as_ref() {
                if let openraft::EntryPayload::Normal(req) = &e.payload {
                    let _ = apply_tx.try_send(req.clone());
                }
            }
            replies.push(MnemeResponse { value: None });
        }
        Ok(replies)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<MnemeNodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<MnemeNodeId, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        *store.snapshot.write() = Some(StoredSnapshot {
            meta: meta.clone(),
            data: snapshot.into_inner(),
        });
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<MnemeNodeId>> {
        let store = self.lock().await;
        let snap = store.snapshot.read().as_ref().map(|s| (s.meta.clone(), s.data.clone()));
        Ok(snap.map(|(meta, data)| Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        }))
    }
}

// ── Themis ────────────────────────────────────────────────────────────────────

pub struct Themis {
    raft: openraft::Raft<TypeConfig>,
    node_id: MnemeNodeId,
    /// Channel to receive committed write entries for application to the RAM pool.
    pub apply_rx: tokio::sync::Mutex<mpsc::Receiver<MnemeRequest>>,
}

impl Themis {
    pub async fn start(
        node_id: u64,
        heartbeat_ms: u64,
        election_min_ms: u64,
        election_max_ms: u64,
        peers: Vec<(u64, String)>,
        aegis: Option<Arc<Aegis>>,
        tls_server_name: String,
        self_addr: String,
    ) -> Result<Self> {
        let node_id = MnemeNodeId(node_id);

        let mut config = openraft::Config::default();
        config.heartbeat_interval = heartbeat_ms;
        config.election_timeout_min = election_min_ms;
        config.election_timeout_max = election_max_ms;
        let config = Arc::new(config.validate()?);

        let (apply_tx, apply_rx) = mpsc::channel::<MnemeRequest>(4096);

        let store = Arc::new(tokio::sync::Mutex::new(MemStore {
            apply_tx: Some(apply_tx),
            ..Default::default()
        }));
        let (log_store, state_machine) = openraft::storage::Adaptor::new(store);

        let network: Arc<RaftTransport> = Arc::new(RaftTransport::new(
            aegis.unwrap_or_else(|| {
                // Fallback for solo mode: create a dummy Aegis-less transport.
                // In solo mode, peers is empty so the network is never used.
                panic!("Aegis required for multi-core Raft transport");
            }),
            tls_server_name,
        ));

        let raft = openraft::Raft::new(
            node_id,
            config,
            network,
            log_store,
            state_machine,
        ).await?;

        if peers.is_empty() {
            // Single-core mode: bootstrap as solo leader.
            let mut members = BTreeMap::new();
            members.insert(node_id, BasicNode::default());
            raft.initialize(members).await?;
            info!(?node_id, "Themis: bootstrapped single-node cluster");
        } else {
            // Multi-core mode: build membership from all peers + self.
            let mut members = BTreeMap::new();
            members.insert(node_id, BasicNode { addr: self_addr });
            for (peer_id, addr) in &peers {
                members.insert(
                    MnemeNodeId(*peer_id),
                    BasicNode { addr: addr.clone() },
                );
            }
            // Node with lowest ID bootstraps the cluster.
            let min_id = members.keys().min().copied().unwrap();
            if node_id == min_id {
                raft.initialize(members).await?;
                info!(?node_id, "Themis: bootstrapped multi-core cluster as initializer");
            } else {
                info!(?node_id, "Themis: waiting for leader election from initializer");
            }
        }

        Ok(Self {
            raft,
            node_id,
            apply_rx: tokio::sync::Mutex::new(apply_rx),
        })
    }

    /// Start in solo mode without Aegis (for Solo/single-core deployments).
    pub async fn start_solo(
        node_id: u64,
        heartbeat_ms: u64,
        election_min_ms: u64,
        election_max_ms: u64,
    ) -> Result<Self> {
        let nid = MnemeNodeId(node_id);

        let mut config = openraft::Config::default();
        config.heartbeat_interval = heartbeat_ms;
        config.election_timeout_min = election_min_ms;
        config.election_timeout_max = election_max_ms;
        let config = Arc::new(config.validate()?);

        let (apply_tx, apply_rx) = mpsc::channel::<MnemeRequest>(4096);

        let store = Arc::new(tokio::sync::Mutex::new(MemStore {
            apply_tx: Some(apply_tx),
            ..Default::default()
        }));
        let (log_store, state_machine) = openraft::storage::Adaptor::new(store);

        // Stub network for solo — never used since peers is empty.
        let network: Arc<RaftTransport> = Arc::new(RaftTransport::new(
            Arc::new(crate::net::aegis::Aegis::dummy()),
            "mneme.local".into(),
        ));

        let raft = openraft::Raft::new(nid, config, network, log_store, state_machine).await?;

        let mut members = BTreeMap::new();
        members.insert(nid, BasicNode::default());
        raft.initialize(members).await?;
        info!(?nid, "Themis: bootstrapped single-node cluster (solo)");

        Ok(Self {
            raft,
            node_id: nid,
            apply_rx: tokio::sync::Mutex::new(apply_rx),
        })
    }

    pub fn is_leader(&self) -> bool {
        self.raft
            .metrics()
            .borrow()
            .current_leader
            .as_ref()
            .map(|l| l == &self.node_id)
            .unwrap_or(false)
    }

    pub fn leader_id(&self) -> Option<u64> {
        self.raft
            .metrics()
            .borrow()
            .current_leader
            .as_ref()
            .map(|id| id.0)
    }

    pub fn node_id(&self) -> u64 {
        self.node_id.0
    }

    /// Return the leader's client-facing address by looking up the leader node
    /// in the Raft membership and converting the replication port to the client
    /// port (rep_port − 1000).  Returns None when the leader is unknown or when
    /// this node is the leader itself.
    pub fn leader_client_addr(&self) -> Option<String> {
        let metrics = self.raft.metrics().borrow().clone();
        let leader = metrics.current_leader?;
        if leader == self.node_id {
            return None; // We are the leader — no redirect needed.
        }
        // Look up the leader's BasicNode from the membership config.
        let membership = &metrics.membership_config;
        let node = membership.membership().get_node(&leader)?;
        // node.addr is the replication address (e.g. "10.0.0.1:7379").
        // Derive client address by subtracting 1000 from the port.
        rep_addr_to_client_addr(&node.addr)
    }

    pub fn current_term(&self) -> u64 {
        self.raft.metrics().borrow().current_term
    }

    pub fn metrics(&self) -> tokio::sync::watch::Receiver<RaftMetrics<MnemeNodeId, BasicNode>> {
        self.raft.metrics()
    }

    /// Submit a write to the Raft log. Only succeeds on the leader.
    pub async fn client_write(&self, request: MnemeRequest) -> Result<MnemeResponse> {
        let resp = self.raft.client_write(request).await?;
        Ok(resp.data)
    }

    /// Get the Raft handle for directly calling RPCs from incoming peer connections.
    pub fn raft_handle(&self) -> &openraft::Raft<TypeConfig> {
        &self.raft
    }

    /// Handle incoming AppendEntries RPC from a peer.
    pub async fn handle_append_entries(
        &self,
        rpc: openraft::raft::AppendEntriesRequest<TypeConfig>,
    ) -> Result<openraft::raft::AppendEntriesResponse<MnemeNodeId>> {
        Ok(self.raft.append_entries(rpc).await?)
    }

    /// Handle incoming Vote RPC from a peer.
    pub async fn handle_vote(
        &self,
        rpc: openraft::raft::VoteRequest<MnemeNodeId>,
    ) -> Result<openraft::raft::VoteResponse<MnemeNodeId>> {
        Ok(self.raft.vote(rpc).await?)
    }

    /// Handle incoming InstallSnapshot RPC from a peer.
    pub async fn handle_install_snapshot(
        &self,
        rpc: openraft::raft::InstallSnapshotRequest<TypeConfig>,
    ) -> Result<openraft::raft::InstallSnapshotResponse<MnemeNodeId>> {
        Ok(self.raft.install_snapshot(rpc).await?)
    }

    pub async fn shutdown(self) -> Result<()> {
        self.raft.shutdown().await?;
        Ok(())
    }
}

/// Convert a replication address (host:rep_port) to a client address
/// (host:client_port) by subtracting 1000 from the port.
fn rep_addr_to_client_addr(rep_addr: &str) -> Option<String> {
    let (host, port_str) = rep_addr.rsplit_once(':')?;
    let rep_port: u16 = port_str.parse().ok()?;
    let client_port = rep_port.checked_sub(1000)?;
    Some(format!("{host}:{client_port}"))
}
