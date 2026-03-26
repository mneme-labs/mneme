// response.rs — Typed response structs returned by MnemeConn methods.
// These mirror what the server serialises as msgpack in the response payload.

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ── Cluster / admin ────────────────────────────────────────────────────────────

/// One entry from a KEEPER-LIST response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeeperEntry {
    pub node_id:    u64,
    pub name:       String,
    pub addr:       String,
    pub pool_bytes: u64,
    pub used_bytes: u64,
}

/// POOL-STATS response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolStats {
    pub used_bytes:   u64,
    pub total_bytes:  u64,
    pub keeper_count: usize,
}

/// One entry from a SLOW-LOG response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowLogEntry {
    /// Command name (e.g. "SET").
    pub command:     String,
    /// Key involved (may be empty for keyless commands).
    pub key:         Vec<u8>,
    /// Wall-clock duration in microseconds.
    pub duration_us: u64,
}

// ── User management ────────────────────────────────────────────────────────────

/// USER-INFO response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub username:    String,
    /// "admin", "readwrite", or "readonly".
    pub role:        String,
    /// Numeric database IDs this user is allowed to access.
    /// Empty = all databases allowed.
    pub allowed_dbs: Vec<u16>,
}

// ── Database namespacing ───────────────────────────────────────────────────────

/// SCAN response: cursor + matching keys.
/// A `next_cursor` of 0 means the full scan is complete.
#[derive(Debug, Clone)]
pub struct ScanPage {
    pub next_cursor: u64,
    pub keys:        Vec<Vec<u8>>,
}

// ── Cluster ────────────────────────────────────────────────────────────────────

/// One entry from a CLUSTER-SLOTS response.
///
/// Represents a contiguous range of hash slots [start, end] (inclusive) owned
/// by a single Core node. In a single-Core deployment there will be exactly
/// one entry covering all 16384 slots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotRange {
    /// First slot in this range (0–16383).
    pub start: u16,
    /// Last slot in this range (inclusive).
    pub end:   u16,
    /// Client-facing address of the Core node that owns these slots.
    pub addr:  String,
}

// ── Monitor stream ─────────────────────────────────────────────────────────────

/// Live command stream returned by [`MnemeConn::monitor`].
///
/// Each call to [`next`] returns one event string pushed by the server.
/// Returns `None` when the connection closes.
///
/// [`next`]: MonitorStream::next
pub struct MonitorStream {
    pub(crate) rx: mpsc::Receiver<String>,
}

impl MonitorStream {
    /// Receive the next monitor event.
    ///
    /// Blocks until an event is available or the connection closes.
    pub async fn next(&mut self) -> Option<String> {
        self.rx.recv().await
    }

    /// Try to receive the next monitor event without blocking.
    ///
    /// Returns `None` if no event is currently available.
    pub fn try_next(&mut self) -> Option<String> {
        self.rx.try_recv().ok()
    }
}
