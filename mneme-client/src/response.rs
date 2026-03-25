// response.rs — Typed response structs returned by MnemeConn methods.
// These mirror what the server serialises as msgpack in the response payload.

use serde::{Deserialize, Serialize};

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
