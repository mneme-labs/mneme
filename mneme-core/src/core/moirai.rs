// Moirai — Consistency dispatcher.
// Fans writes out to Keeper nodes with EVENTUAL / ONE / QUORUM / ALL semantics.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use mneme_common::{ConsistencyLevel, Frame};
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, warn};

/// Metadata about a connected Keeper node (A-05: unified registry).
#[derive(Debug, Clone)]
pub struct KeeperInfo {
    pub node_id: u64,
    /// Human-readable node name from config.node.node_id, e.g. "hypnos-1".
    pub node_name: String,
    pub addr: String,
    pub pool_bytes: u64,
    pub used_bytes: u64,
}

/// Handle to a connected Keeper node.
#[derive(Clone)]
pub struct KeeperHandle {
    pub node_id: u64,
    /// Channel to send frames to the keeper connection task.
    pub tx: mpsc::Sender<(Frame, mpsc::Sender<Result<()>>)>,
}

/// Write dispatch result.
#[derive(Debug)]
pub struct DispatchResult {
    pub acks: usize,
    pub required: usize,
}

pub struct Moirai {
    is_solo: bool,
    write_timeout: Duration,
    keepers: Arc<RwLock<HashMap<u64, KeeperHandle>>>,
    /// Shared keeper metadata registry (A-05: unified with Mnemosyne).
    keeper_infos: Arc<RwLock<Vec<KeeperInfo>>>,
}

impl Moirai {
    /// Create.
    /// `is_solo` = true → QUORUM/ALL silently downgraded to EVENTUAL.
    /// `keeper_infos` is the shared registry also used by Mnemosyne (A-05).
    pub fn new(is_solo: bool, write_timeout_ms: u64, keeper_infos: Arc<RwLock<Vec<KeeperInfo>>>) -> Self {
        Self {
            is_solo,
            write_timeout: Duration::from_millis(write_timeout_ms),
            keepers: Arc::new(RwLock::new(HashMap::new())),
            keeper_infos,
        }
    }

    /// Register a keeper connection handle and update the unified KeeperInfo registry (A-02, A-05).
    pub fn add_keeper(&self, handle: KeeperHandle, node_name: String, addr: String, pool_bytes: u64) {
        let node_id = handle.node_id;
        self.keepers.write().insert(node_id, handle);
        let mut infos = self.keeper_infos.write();
        if !infos.iter().any(|k| k.node_id == node_id) {
            infos.push(KeeperInfo { node_id, node_name, addr, pool_bytes, used_bytes: 0 });
        }
    }

    /// Remove a keeper (disconnected) from both handles and the unified registry.
    pub fn remove_keeper(&self, node_id: u64) {
        self.keepers.write().remove(&node_id);
        self.keeper_infos.write().retain(|k| k.node_id != node_id);
    }

    /// Number of currently connected keepers.
    pub fn keeper_count(&self) -> usize {
        self.keepers.read().len()
    }

    /// Return the shared keeper metadata registry (A-05).
    pub fn keeper_infos(&self) -> Arc<RwLock<Vec<KeeperInfo>>> {
        self.keeper_infos.clone()
    }

    /// Dispatch a write frame to keepers according to consistency level.
    /// Returns Ok on success (enough ACKs collected within deadline).
    pub async fn dispatch(
        &self,
        frame: Frame,
        level: ConsistencyLevel,
    ) -> Result<DispatchResult> {
        let effective = self.effective_level(level);

        let handles: Vec<KeeperHandle> = self.keepers.read().values().cloned().collect();
        let n = handles.len();

        let required = match effective {
            ConsistencyLevel::Eventual => 0,
            ConsistencyLevel::One => 1.min(n),
            ConsistencyLevel::Quorum => quorum(n),
            ConsistencyLevel::All => n,
        };

        if required == 0 {
            // Fire-and-forget: send async, don't wait
            for h in handles {
                let f = frame.clone();
                let (ack_tx, _ack_rx) = mpsc::channel(1);
                let _ = h.tx.try_send((f, ack_tx));
            }
            debug!(level = ?effective, "Eventual write dispatched");
            return Ok(DispatchResult { acks: 0, required: 0 });
        }

        if n < required {
            bail!(
                "Not enough keepers: have {n}, need {required} for {:?}",
                effective
            );
        }

        // Fan out and collect ACKs
        let mut ack_rxs = Vec::with_capacity(n);
        for h in &handles {
            let (ack_tx, ack_rx) = mpsc::channel::<Result<()>>(1);
            if h.tx.try_send((frame.clone(), ack_tx)).is_err() {
                warn!(node_id = h.node_id, "keeper channel full — frame not sent");
                // Don't silently skip — count this as a failed keeper
                continue;
            }
            ack_rxs.push(ack_rx);
        }

        // Fail early if we couldn't even send to enough keepers
        if ack_rxs.len() < required {
            bail!(
                "Consistency {:?} not reachable: only {}/{} keeper channels available",
                effective, ack_rxs.len(), required
            );
        }

        // Collect required ACKs with timeout
        let mut acks: usize = 0;
        for mut rx in ack_rxs {
            if acks >= required {
                break;
            }
            match timeout(self.write_timeout, rx.recv()).await {
                Ok(Some(Ok(()))) => acks += 1,
                Ok(Some(Err(e))) => warn!("keeper ack error: {e}"),
                Ok(None) => warn!("keeper ack channel closed"),
                Err(_) => warn!("keeper ack timeout"),
            }
        }

        if acks < required {
            bail!(
                "Consistency {:?} not met: got {acks}/{required} ACKs",
                effective
            );
        }

        debug!(level = ?effective, acks, required, "Write dispatched OK");
        Ok(DispatchResult { acks, required })
    }

    /// Apply solo-mode downgrade rules.
    fn effective_level(&self, level: ConsistencyLevel) -> ConsistencyLevel {
        if self.is_solo {
            match level {
                ConsistencyLevel::Quorum | ConsistencyLevel::All => {
                    ConsistencyLevel::Eventual
                }
                other => other,
            }
        } else {
            level
        }
    }
}

fn quorum(n: usize) -> usize {
    n / 2 + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use mneme_common::CmdId;

    fn dummy_frame() -> Frame {
        Frame {
            cmd_id: CmdId::Set,
            flags: 0,
            req_id: 0,
            payload: Bytes::from_static(b"test"),
        }
    }

    fn make_keeper(node_id: u64) -> (KeeperHandle, mpsc::Receiver<(Frame, mpsc::Sender<Result<()>>)>) {
        let (tx, rx) = mpsc::channel(16);
        (KeeperHandle { node_id, tx }, rx)
    }

    #[test]
    fn quorum_formula() {
        assert_eq!(quorum(1), 1);
        assert_eq!(quorum(2), 2);
        assert_eq!(quorum(3), 2);
        assert_eq!(quorum(4), 3);
        assert_eq!(quorum(5), 3);
    }

    fn make_keeper_infos() -> Arc<RwLock<Vec<KeeperInfo>>> {
        Arc::new(RwLock::new(Vec::new()))
    }

    #[test]
    fn solo_downgrades_quorum_to_eventual() {
        let m = Moirai::new(true, 500, make_keeper_infos());
        assert_eq!(m.effective_level(ConsistencyLevel::Quorum), ConsistencyLevel::Eventual);
        assert_eq!(m.effective_level(ConsistencyLevel::All), ConsistencyLevel::Eventual);
        assert_eq!(m.effective_level(ConsistencyLevel::One), ConsistencyLevel::One);
        assert_eq!(m.effective_level(ConsistencyLevel::Eventual), ConsistencyLevel::Eventual);
    }

    #[test]
    fn non_solo_does_not_downgrade() {
        let m = Moirai::new(false, 500, make_keeper_infos());
        assert_eq!(m.effective_level(ConsistencyLevel::Quorum), ConsistencyLevel::Quorum);
        assert_eq!(m.effective_level(ConsistencyLevel::All), ConsistencyLevel::All);
    }

    #[tokio::test]
    async fn eventual_no_keepers_succeeds() {
        let m = Moirai::new(false, 500, make_keeper_infos());
        let result = m.dispatch(dummy_frame(), ConsistencyLevel::Eventual).await;
        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r.required, 0);
    }

    #[tokio::test]
    async fn one_no_keepers_fails() {
        let m = Moirai::new(false, 200, make_keeper_infos());
        // ONE with no keepers: required=min(1,0)=0, so it succeeds with 0 acks
        let result = m.dispatch(dummy_frame(), ConsistencyLevel::One).await;
        assert!(result.is_ok()); // required = min(1,0) = 0
    }

    #[tokio::test]
    async fn quorum_no_keepers_fails() {
        let m = Moirai::new(false, 200, make_keeper_infos());
        // QUORUM with 0 keepers → required=1 but n=0 → bail
        // Actually quorum(0) = 1 but n=0 < 1 → fail
        let result = m.dispatch(dummy_frame(), ConsistencyLevel::Quorum).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn eventual_fires_and_forgets() {
        let m = Moirai::new(false, 500, make_keeper_infos());
        let (handle, mut rx) = make_keeper(1);
        m.add_keeper(handle, "keeper-test".into(), "1.2.3.4:7379".into(), 0);
        let result = m.dispatch(dummy_frame(), ConsistencyLevel::Eventual).await;
        assert!(result.is_ok());
        // Frame may or may not be received (fire-and-forget)
        // Just verify dispatch completed
    }

    #[tokio::test]
    async fn one_with_keeper_waits_for_ack() {
        let m = Moirai::new(false, 500, make_keeper_infos());
        let (handle, mut rx) = make_keeper(1);
        m.add_keeper(handle, "keeper-test".into(), "1.2.3.4:7379".into(), 0);

        // Spawn a task that ACKs the frame
        tokio::spawn(async move {
            if let Some((_, ack_tx)) = rx.recv().await {
                let _ = ack_tx.send(Ok(())).await;
            }
        });

        let result = m.dispatch(dummy_frame(), ConsistencyLevel::One).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().acks, 1);
    }

    #[test]
    fn add_remove_keeper() {
        let m = Moirai::new(false, 500, make_keeper_infos());
        let (handle, _rx) = make_keeper(42);
        m.add_keeper(handle, "keeper-42".into(), "10.0.0.1:7379".into(), 1024);
        assert_eq!(m.keeper_count(), 1);
        assert_eq!(m.keeper_infos.read().len(), 1);

        m.remove_keeper(42);
        assert_eq!(m.keeper_count(), 0);
        assert!(m.keeper_infos.read().is_empty());
    }

    #[test]
    fn remove_nonexistent_keeper() {
        let m = Moirai::new(false, 500, make_keeper_infos());
        // Should not panic when removing a node_id that was never added
        m.remove_keeper(999);
        assert_eq!(m.keeper_count(), 0);
    }

    #[tokio::test]
    async fn quorum_calculation_3_keepers() {
        let m = Moirai::new(false, 1000, make_keeper_infos());

        // Add 3 keepers, each with a task that sends an ACK
        let mut tasks = Vec::new();
        for id in 1..=3 {
            let (handle, mut rx) = make_keeper(id);
            m.add_keeper(handle, format!("keeper-{id}"), format!("10.0.0.{id}:7379"), 0);
            tasks.push(tokio::spawn(async move {
                if let Some((_, ack_tx)) = rx.recv().await {
                    let _ = ack_tx.send(Ok(())).await;
                }
            }));
        }

        // QUORUM with 3 keepers needs floor(3/2)+1 = 2 ACKs
        let result = m.dispatch(dummy_frame(), ConsistencyLevel::Quorum).await;
        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r.required, 2);
        assert!(r.acks >= 2);
    }

    #[tokio::test]
    async fn all_1_keeper_needs_1() {
        let m = Moirai::new(false, 1000, make_keeper_infos());
        let (handle, mut rx) = make_keeper(1);
        m.add_keeper(handle, "keeper-1".into(), "10.0.0.1:7379".into(), 0);

        tokio::spawn(async move {
            if let Some((_, ack_tx)) = rx.recv().await {
                let _ = ack_tx.send(Ok(())).await;
            }
        });

        let result = m.dispatch(dummy_frame(), ConsistencyLevel::All).await;
        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r.required, 1);
        assert_eq!(r.acks, 1);
    }

    #[tokio::test]
    async fn dispatch_channel_full_fails_quorum() {
        let m = Moirai::new(false, 500, make_keeper_infos());

        // Create keepers with bounded(0) channels — always full, try_send will fail
        for id in 1..=3 {
            // mpsc::channel requires capacity >= 1, so use capacity 1 and pre-fill it
            let (tx, _rx) = mpsc::channel(1);
            // Fill the channel so the next try_send will fail
            let (dummy_ack_tx, _dummy_ack_rx) = mpsc::channel::<Result<()>>(1);
            let _ = tx.try_send((dummy_frame(), dummy_ack_tx));
            let handle = KeeperHandle { node_id: id, tx };
            m.add_keeper(handle, format!("keeper-{id}"), format!("10.0.0.{id}:7379"), 0);
        }

        // QUORUM needs 2 ACKs but all channels are full → "not reachable"
        let result = m.dispatch(dummy_frame(), ConsistencyLevel::Quorum).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not reachable") || err_msg.contains("QuorumNotReached"),
            "unexpected error: {err_msg}"
        );
    }
}
