// mneme-client/src/lib.rs — Pontus: Mneme client library.
//
// Public surface:
//   MnemePool / PoolConfig  — connection pool with health-check + auto-reconnect
//   MnemeConn / Consistency — raw multiplexed connection (impl split across cmd_*.rs)
//   Pipeline                — batch multiple commands in one write
//   ClientError             — error type
//   response::*             — typed response structs (KeeperEntry, PoolStats, …)

// ── Internal modules ─────────────────────────────────────────────────────────
pub(crate) mod conn;

mod cmd_admin;
mod cmd_db;
mod cmd_hash;
mod cmd_json;
mod cmd_kv;
mod cmd_list;
mod cmd_zset;

// ── Public modules ────────────────────────────────────────────────────────────
pub mod cmd_pipeline;
pub mod error;
pub mod pool;
pub mod response;

// ── Re-exports ────────────────────────────────────────────────────────────────
pub use cmd_pipeline::Pipeline;
pub use conn::{Consistency, MnemeConn};
pub use error::ClientError;
pub use pool::{MnemePool, PoolConfig, PoolGuard};
pub use response::{
    KeeperEntry, MonitorStream, PoolStats, ScanPage, SlotRange, SlowLogEntry, UserInfo,
};
