// Herold — Node registration daemon.
// Zero-friction: one command to join any node to the cluster.
// Server-side registry: validates join tokens, assigns node IDs, registers keepers.
//
// Client-side registration (`register_with_core`) has been moved to
// `mneme_common::herold_client` (A-03 fix) so that mneme-keeper can call it
// without depending on mneme-core.

use std::sync::Arc;

use bytes::Bytes;
use mneme_common::{CmdId, Frame, RegisterAck, RegisterPayload};
use parking_lot::RwLock;
use tracing::info;

use crate::core::moirai::KeeperInfo;

// ── Herold registry (server side — lives inside Mnemosyne) ────────────────────

pub struct HeroldRegistry {
    join_token: String,
    keepers: Arc<RwLock<Vec<KeeperInfo>>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl HeroldRegistry {
    pub fn new(join_token: String, keepers: Arc<RwLock<Vec<KeeperInfo>>>) -> Self {
        Self {
            join_token,
            keepers,
            next_id: std::sync::atomic::AtomicU64::new(2), // 1 = God node
        }
    }

    /// Process a REGISTER frame from a joining node.
    /// Returns an ack frame to send back.
    pub fn handle_register(&self, payload: &RegisterPayload, pool_max: &std::sync::atomic::AtomicU64) -> Frame {
        // Validate join token
        if payload.join_token != self.join_token {
            let ack = RegisterAck {
                accepted: false,
                message: "invalid join token".into(),
                assigned_id: 0,
            };
            return Frame {
                cmd_id: CmdId::AckWrite,
                flags: 0,
                req_id: 0, payload: Bytes::from(rmp_serde::to_vec(&ack).unwrap_or_default()),
            };
        }

        let assigned_id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // Register keeper
        if payload.role == "keeper" {
            let mut keepers = self.keepers.write();
            if !keepers.iter().any(|k| k.node_id == assigned_id) {
                keepers.push(KeeperInfo {
                    node_id:    assigned_id,
                    node_name:  payload.node_id.clone(),
                    addr:       payload.replication_addr.clone(),
                    pool_bytes: payload.grant_bytes,
                    used_bytes: 0,
                });
                // Grow the logical pool
                pool_max.fetch_add(payload.grant_bytes, std::sync::atomic::Ordering::Relaxed);
            }
            info!(
                node_id = assigned_id,
                addr = %payload.replication_addr,
                grant_mb = payload.grant_bytes / 1024 / 1024,
                "Herold: Keeper registered"
            );
        } else if payload.role == "read-replica" {
            info!(
                node_id = assigned_id,
                addr = %payload.replication_addr,
                "Herold: Read replica registered"
            );
        }

        let ack = RegisterAck {
            accepted: true,
            message: "OK".into(),
            assigned_id,
        };
        Frame {
            cmd_id: CmdId::AckWrite,
            flags: 0,
            req_id: 0, payload: Bytes::from(rmp_serde::to_vec(&ack).unwrap_or_default()),
        }
    }
}

/// Generate a cryptographically random join token for cluster bootstrap.
pub fn generate_join_token() -> String {
    use rand::Rng;
    use std::fmt::Write;
    let mut rng = rand::thread_rng();
    let bytes: [u8; 16] = rng.gen();
    let mut s = String::with_capacity(42);
    s.push_str("mneme_tok_");
    for b in &bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_format() {
        let t = generate_join_token();
        assert!(t.starts_with("mneme_tok_"));
        assert_eq!(t.len(), 42); // "mneme_tok_" + 32 hex chars
    }

    #[test]
    fn registry_rejects_bad_token() {
        let keepers = Arc::new(RwLock::new(Vec::new()));
        let registry = HeroldRegistry::new("correct_token".into(), keepers);
        let pool_max = std::sync::atomic::AtomicU64::new(0);

        let payload = RegisterPayload {
            node_id: "hypnos-1".into(),
            role: "keeper".into(),
            grant_bytes: 256 * 1024 * 1024,
            replication_addr: "1.2.3.4:7379".into(),
            join_token: "wrong_token".into(),
        };

        let ack_frame = registry.handle_register(&payload, &pool_max);
        let ack: RegisterAck = rmp_serde::from_slice(&ack_frame.payload).unwrap();
        assert!(!ack.accepted);
        assert_eq!(pool_max.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn registry_accepts_valid_keeper() {
        let keepers = Arc::new(RwLock::new(Vec::new()));
        let registry = HeroldRegistry::new("secret".into(), keepers.clone());
        let pool_max = std::sync::atomic::AtomicU64::new(0);

        let grant = 256u64 * 1024 * 1024;
        let payload = RegisterPayload {
            node_id: "hypnos-1".into(),
            role: "keeper".into(),
            grant_bytes: grant,
            replication_addr: "1.2.3.4:7379".into(),
            join_token: "secret".into(),
        };

        let ack_frame = registry.handle_register(&payload, &pool_max);
        let ack: RegisterAck = rmp_serde::from_slice(&ack_frame.payload).unwrap();
        assert!(ack.accepted);
        assert_eq!(pool_max.load(std::sync::atomic::Ordering::Relaxed), grant);
        assert_eq!(keepers.read().len(), 1);
    }

    #[test]
    fn registry_assigns_unique_ids() {
        let keepers = Arc::new(RwLock::new(Vec::new()));
        let registry = HeroldRegistry::new("tok".into(), keepers.clone());
        let pool_max = std::sync::atomic::AtomicU64::new(0);

        let make_payload = |id: &str| RegisterPayload {
            node_id: id.into(),
            role: "keeper".into(),
            grant_bytes: 1,
            replication_addr: format!("{id}:7379"),
            join_token: "tok".into(),
        };

        let f1 = registry.handle_register(&make_payload("h1"), &pool_max);
        let f2 = registry.handle_register(&make_payload("h2"), &pool_max);
        let a1: RegisterAck = rmp_serde::from_slice(&f1.payload).unwrap();
        let a2: RegisterAck = rmp_serde::from_slice(&f2.payload).unwrap();
        assert_ne!(a1.assigned_id, a2.assigned_id);
    }
}