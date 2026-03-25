use std::net::SocketAddr;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, MnemeError>;

#[derive(Debug, Error)]
pub enum MnemeError {
    // ── Client errors (4xx) ───────────────────────────────────────────────────
    #[error("key not found")]
    KeyNotFound,

    #[error("wrong type: expected {expected}, got {got}")]
    WrongType { expected: &'static str, got: &'static str },

    #[error("token expired")]
    TokenExpired,

    #[error("token invalid")]
    TokenInvalid,

    #[error("token revoked")]
    TokenRevoked,

    #[error("max connections reached")]
    MaxConnectionsReached,

    #[error("request timeout after {ms}ms")]
    RequestTimeout { ms: u64 },

    /// Client must retry at the given address.
    #[error("MOVED slot={slot} addr={addr}")]
    SlotMoved { slot: u16, addr: String },

    #[error("payload too large: max={max} got={got}")]
    PayloadTooLarge { max: usize, got: usize },

    #[error("key too large: max={max} got={got}")]
    KeyTooLarge { max: usize, got: usize },

    // ── Consistency errors ────────────────────────────────────────────────────
    #[error("quorum not reached: got {got}/{need} ACKs")]
    QuorumNotReached { got: usize, need: usize },

    #[error("replication timeout")]
    ReplicationTimeout,

    #[error("shutting down — rejecting new writes")]
    ShuttingDown,

    // ── Server errors (5xx) ───────────────────────────────────────────────────
    #[error("out of memory: pool exhausted")]
    OutOfMemory,

    #[error("keeper unreachable: {id}")]
    KeeperUnreachable { id: String },

    #[error("WAL write failed: {0}")]
    WalWriteFailed(String),

    #[error("snapshot failed: {0}")]
    SnapshotFailed(String),

    // ── Protocol errors ───────────────────────────────────────────────────────
    #[error("protocol violation: {0}")]
    Protocol(String),

    #[error("unknown command: 0x{cmd:02X}")]
    UnknownCommand { cmd: u8 },

    // ── Infrastructure ────────────────────────────────────────────────────────
    #[error("config error: {0}")]
    Config(String),

    #[error("auth error: {0}")]
    Auth(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("cluster error: {0}")]
    Cluster(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl MnemeError {
    /// Wire error code for the 2-byte code field in error frames.
    pub fn wire_code(&self) -> u16 {
        match self {
            Self::KeyNotFound => 404,
            Self::WrongType { .. } => 400,
            Self::TokenExpired => 401,
            Self::TokenInvalid => 401,
            Self::TokenRevoked => 401,
            Self::MaxConnectionsReached => 429,
            Self::RequestTimeout { .. } => 408,
            Self::SlotMoved { .. } => 301,
            Self::PayloadTooLarge { .. } => 413,
            Self::KeyTooLarge { .. } => 413,
            Self::QuorumNotReached { .. } => 503,
            Self::ReplicationTimeout => 503,
            Self::ShuttingDown => 503,
            Self::OutOfMemory => 507,
            Self::KeeperUnreachable { .. } => 502,
            Self::WalWriteFailed(_) => 500,
            Self::SnapshotFailed(_) => 500,
            Self::Protocol(_) => 400,
            Self::UnknownCommand { .. } => 400,
            Self::Config(_) => 500,
            Self::Auth(_) => 401,
            Self::Serialization(_) => 400,
            Self::Storage(_) => 500,
            Self::Network(_) => 502,
            Self::Cluster(_) => 503,
            Self::Io(_) => 500,
            Self::Other(_) => 500,
        }
    }

    /// Build a SlotMoved from a SocketAddr.
    pub fn moved(slot: u16, addr: SocketAddr) -> Self {
        Self::SlotMoved { slot, addr: addr.to_string() }
    }
}

// Allow ? from rmp_serde decode errors
impl From<rmp_serde::decode::Error> for MnemeError {
    fn from(e: rmp_serde::decode::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

impl From<rmp_serde::encode::Error> for MnemeError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_codes_are_sensible() {
        assert_eq!(MnemeError::KeyNotFound.wire_code(), 404);
        assert_eq!(MnemeError::TokenExpired.wire_code(), 401);
        assert_eq!(MnemeError::SlotMoved { slot: 1, addr: "x".into() }.wire_code(), 301);
        assert_eq!(MnemeError::QuorumNotReached { got: 1, need: 2 }.wire_code(), 503);
        assert_eq!(MnemeError::PayloadTooLarge { max: 10, got: 20 }.wire_code(), 413);
    }

    #[test]
    fn display_messages() {
        assert!(MnemeError::KeyNotFound.to_string().contains("not found"));
        assert!(MnemeError::QuorumNotReached { got: 1, need: 3 }.to_string().contains("1/3"));
        assert!(MnemeError::RequestTimeout { ms: 5000 }.to_string().contains("5000"));
    }
}