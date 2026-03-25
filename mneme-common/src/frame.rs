// frame.rs — MnemeCache wire protocol implementation.
//
// Wire header (16 bytes):
//   [4B magic][1B ver][1B cmd_id][2B flags][4B payload_len][4B req_id][msgpack payload]
//
// flags bits 15-4 = slot hint  |  bits 3-2 = consistency  |  bits 1-0 = reserved
// consistency: 00=EVENTUAL 01=QUORUM 10=ALL 11=ONE
// req_id: 0 = single-plex (no multiplexing), 1+ = multiplexed request

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::types::{Value, ZSetMember};

pub const MAGIC: u32 = 0x4D4E454D; // "MNEM"
pub const PROTOCOL_VERSION: u8 = 0x01;
pub const NUM_SLOTS: u16 = 16384;
/// Full wire header length: 4B magic + 1B ver + 1B cmd_id + 2B flags + 4B payload_len + 4B req_id.
pub const HEADER_LEN: usize = 16;

// ── CmdId ─────────────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmdId {
    // String / generic ops
    Get     = 0x01,
    Set     = 0x02,
    Del     = 0x03,
    Exists  = 0x04,
    Expire  = 0x05,
    Ttl     = 0x06,
    // Hash ops
    HSet    = 0x10,
    HGet    = 0x11,
    HDel    = 0x12,
    HGetAll = 0x13,
    // List ops
    LPush   = 0x20,
    RPush   = 0x21,
    LPop    = 0x22,
    RPop    = 0x23,
    LRange  = 0x24,
    // Sorted set ops
    ZAdd          = 0x30,
    ZRank         = 0x31,
    ZRange        = 0x32,
    ZRangeByScore = 0x33,
    ZCard         = 0x34,
    ZRem          = 0x35,
    ZScore        = 0x36,
    // Counter ops (0x40–0x45)
    Incr        = 0x40,
    Decr        = 0x41,
    IncrBy      = 0x42,
    DecrBy      = 0x43,
    IncrByFloat = 0x44,
    GetSet      = 0x45,
    // JSON ops (0x50–0x56)
    JsonGet       = 0x50,
    JsonSet       = 0x51,
    JsonDel       = 0x52,
    JsonExists    = 0x53,
    JsonType      = 0x54,
    JsonArrAppend = 0x55,
    JsonNumIncrBy = 0x56,
    // Auth
    Auth        = 0x60,
    RevokeToken = 0x61,
    // User management (admin-only)
    UserCreate  = 0x62,
    UserDelete  = 0x63,
    UserList    = 0x64,
    UserGrant   = 0x65,
    UserRevoke  = 0x66,
    UserInfo    = 0x67,
    UserSetRole = 0x68,
    // Observability
    SlowLog     = 0x70,
    Metrics     = 0x71,
    Stats       = 0x72,
    MemoryUsage = 0x73,
    Monitor     = 0x74,
    // Admin / config
    Config       = 0x80,
    ClusterInfo  = 0x81,
    ClusterSlots = 0x82,
    KeeperList   = 0x83,
    PoolStats    = 0x84,
    Wait         = 0x85,
    // Database namespace ops
    Select   = 0x86,
    DbSize   = 0x87,
    FlushDb  = 0x88,
    Scan     = 0x89,
    Type     = 0x8A,
    MGet     = 0x8B,
    MSet     = 0x8C,
    DbCreate = 0x8D,
    DbList   = 0x8F,
    DbDrop   = 0x90,
    // Cluster management (admin-only)
    GenerateJoinToken = 0x8E,
    // Internal / replication
    SyncStart    = 0xA0,
    PushKey      = 0xA1,
    Heartbeat    = 0xA2,
    Moved        = 0xA3,
    AckWrite     = 0xA4,
    SyncRequest  = 0xA5,
    SyncComplete = 0xA6,
    // Raft consensus (Core-to-Core)
    RaftAppendEntries  = 0xB0,
    RaftVote           = 0xB1,
    RaftInstallSnapshot = 0xB2,
    LeaderRedirect     = 0xB3,
    // Responses
    Ok    = 0xF0,
    Error = 0xF1,
}

impl TryFrom<u8> for CmdId {
    type Error = crate::MnemeError;

    fn try_from(v: u8) -> crate::Result<Self> {
        Ok(match v {
            0x01 => Self::Get,       0x02 => Self::Set,     0x03 => Self::Del,
            0x04 => Self::Exists,    0x05 => Self::Expire,  0x06 => Self::Ttl,
            0x10 => Self::HSet,      0x11 => Self::HGet,    0x12 => Self::HDel,
            0x13 => Self::HGetAll,
            0x20 => Self::LPush,     0x21 => Self::RPush,   0x22 => Self::LPop,
            0x23 => Self::RPop,      0x24 => Self::LRange,
            0x30 => Self::ZAdd,      0x31 => Self::ZRank,   0x32 => Self::ZRange,
            0x33 => Self::ZRangeByScore, 0x34 => Self::ZCard, 0x35 => Self::ZRem,
            0x36 => Self::ZScore,
            0x40 => Self::Incr,      0x41 => Self::Decr,    0x42 => Self::IncrBy,
            0x43 => Self::DecrBy,    0x44 => Self::IncrByFloat, 0x45 => Self::GetSet,
            0x50 => Self::JsonGet,   0x51 => Self::JsonSet,  0x52 => Self::JsonDel,
            0x53 => Self::JsonExists, 0x54 => Self::JsonType,
            0x55 => Self::JsonArrAppend, 0x56 => Self::JsonNumIncrBy,
            0x60 => Self::Auth,      0x61 => Self::RevokeToken,
            0x62 => Self::UserCreate, 0x63 => Self::UserDelete, 0x64 => Self::UserList,
            0x65 => Self::UserGrant,  0x66 => Self::UserRevoke,  0x67 => Self::UserInfo,
            0x68 => Self::UserSetRole,
            0x70 => Self::SlowLog,   0x71 => Self::Metrics,  0x72 => Self::Stats,
            0x73 => Self::MemoryUsage, 0x74 => Self::Monitor,
            0x80 => Self::Config,    0x81 => Self::ClusterInfo, 0x82 => Self::ClusterSlots,
            0x83 => Self::KeeperList, 0x84 => Self::PoolStats, 0x85 => Self::Wait,
            0x86 => Self::Select,    0x87 => Self::DbSize,      0x88 => Self::FlushDb,
            0x89 => Self::Scan,      0x8A => Self::Type,        0x8B => Self::MGet,
            0x8C => Self::MSet,      0x8D => Self::DbCreate,
            0x8E => Self::GenerateJoinToken,
            0x8F => Self::DbList,    0x90 => Self::DbDrop,
            0xA0 => Self::SyncStart,   0xA1 => Self::PushKey,      0xA2 => Self::Heartbeat,
            0xA3 => Self::Moved,       0xA4 => Self::AckWrite,     0xA5 => Self::SyncRequest,
            0xA6 => Self::SyncComplete,
            0xB0 => Self::RaftAppendEntries, 0xB1 => Self::RaftVote,
            0xB2 => Self::RaftInstallSnapshot, 0xB3 => Self::LeaderRedirect,
            0xF0 => Self::Ok,        0xF1 => Self::Error,
            cmd => return Err(crate::MnemeError::UnknownCommand { cmd }),
        })
    }
}

// ── ConsistencyLevel ──────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsistencyLevel {
    Eventual = 0b00,
    Quorum   = 0b01,
    All      = 0b10,
    One      = 0b11,
}

impl From<u8> for ConsistencyLevel {
    fn from(v: u8) -> Self {
        match v & 0b11 {
            0b00 => Self::Eventual,
            0b01 => Self::Quorum,
            0b10 => Self::All,
            _    => Self::One,
        }
    }
}

// ── Frame ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Frame {
    pub cmd_id:  CmdId,
    pub flags:   u16,
    /// 0 = single-plex; ≥1 = multiplexed (response carries same req_id).
    pub req_id:  u32,
    pub payload: Bytes,
}

impl Frame {
    /// Encode to wire bytes: 16-byte header + msgpack payload.
    pub fn encode(&self) -> Vec<u8> {
        let plen = self.payload.len() as u32;
        let mut buf = Vec::with_capacity(HEADER_LEN + self.payload.len());
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.push(PROTOCOL_VERSION);
        buf.push(self.cmd_id as u8);
        buf.extend_from_slice(&self.flags.to_be_bytes());
        buf.extend_from_slice(&plen.to_be_bytes());
        buf.extend_from_slice(&self.req_id.to_be_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode one frame from `buf`. Returns `(frame, bytes_consumed)`.
    /// The caller must ensure `buf.len() >= HEADER_LEN + payload_len` before calling.
    pub fn decode(buf: &[u8]) -> crate::Result<(Self, usize)> {
        if buf.len() < HEADER_LEN {
            return Err(crate::MnemeError::Protocol("incomplete header".into()));
        }
        let magic = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(crate::MnemeError::Protocol(
                format!("bad magic: 0x{magic:08X}")));
        }
        let cmd_byte = buf[5];
        let flags    = u16::from_be_bytes(buf[6..8].try_into().unwrap());
        let plen     = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
        let req_id   = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        let total    = HEADER_LEN + plen;
        if buf.len() < total {
            return Err(crate::MnemeError::Protocol("incomplete payload".into()));
        }
        let cmd_id  = CmdId::try_from(cmd_byte)?;
        let payload = Bytes::copy_from_slice(&buf[HEADER_LEN..total]);
        Ok((Frame { cmd_id, flags, req_id, payload }, total))
    }

    /// Consistency level extracted from flags bits 3-2.
    pub fn consistency(&self) -> ConsistencyLevel {
        ConsistencyLevel::from(((self.flags >> 2) & 0b11) as u8)
    }

    /// Slot hint from flags bits 15-4 (0 = no hint).
    pub fn slot_hint(&self) -> u16 {
        self.flags >> 4
    }

    /// Build a success response frame (req_id=0 for single-plex).
    pub fn ok_response(payload: Bytes) -> Self {
        Self { cmd_id: CmdId::Ok, flags: 0, req_id: 0, payload }
    }

    /// Build a success response that echoes the request's req_id (for multiplexed connections).
    pub fn ok_response_for(payload: Bytes, req_id: u32) -> Self {
        Self { cmd_id: CmdId::Ok, flags: 0, req_id, payload }
    }

    /// Build an error response frame (msgpack-encoded string message).
    pub fn error_response(msg: &str) -> Self {
        let payload = rmp_serde::to_vec(msg).unwrap_or_default();
        Self { cmd_id: CmdId::Error, flags: 0, req_id: 0, payload: Bytes::from(payload) }
    }

    /// Build an error response that echoes the request's req_id.
    pub fn error_response_for(msg: &str, req_id: u32) -> Self {
        let payload = rmp_serde::to_vec(msg).unwrap_or_default();
        Self { cmd_id: CmdId::Error, flags: 0, req_id, payload: Bytes::from(payload) }
    }
}

// ── slot_from_key ─────────────────────────────────────────────────────────────

/// CRC16-CCITT (xmodem) % 16384 with hash-tag support.
pub fn slot_from_key(key: &[u8]) -> u16 {
    crc16_xmodem(extract_hash_tag(key)) % NUM_SLOTS
}

fn extract_hash_tag(key: &[u8]) -> &[u8] {
    if let Some(open) = key.iter().position(|&b| b == b'{') {
        if let Some(rel) = key[open + 1..].iter().position(|&b| b == b'}') {
            let tag = &key[open + 1..open + 1 + rel];
            if !tag.is_empty() { return tag; }
        }
    }
    key
}

fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 { crc = (crc << 1) ^ 0x1021; } else { crc <<= 1; }
        }
    }
    crc
}

// ── Request / payload structs ─────────────────────────────────────────────────
// String ops

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub value: Value,
    pub ttl_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelRequest {
    pub keys: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpireRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub seconds: u64,
}

// Hash ops

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HGetRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub field: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HSetRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HDelRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub fields: Vec<Vec<u8>>,
}

// List ops

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPushRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub values: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LRangeRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub start: i64,
    pub stop:  i64,
}

// ZSet ops

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZAddRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub members: Vec<ZSetMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZRankRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub member: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZRangeRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub start:      i64,
    pub stop:       i64,
    pub with_scores: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZRangeByScoreRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZRemRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub members: Vec<Vec<u8>>,
}

// Counter ops

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrByRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub delta: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrByFloatRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub delta: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetSetRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub value: Value,
}

// JSON ops

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonGetRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    /// JSONPath e.g. "$.name" — empty or "$" = root
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonSetRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub path:  String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonDelRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonArrAppendRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub path:  String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonNumIncrByRequest {
    #[serde(with = "serde_bytes")]
    pub key: Vec<u8>,
    pub path:  String,
    pub delta: f64,
}

// User management payloads

/// Create a user with a given role. Caller must be admin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCreateRequest {
    pub username: String,
    pub password: String,
    /// Role: "admin", "readwrite", "readonly". Default "readwrite".
    #[serde(default = "default_readwrite_role")]
    pub role: String,
}

/// Delete a user by username. Caller must be admin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserDeleteRequest {
    pub username: String,
}

/// Grant access to a specific database. Caller must be admin.
/// If a user has no db grants their `allowed_dbs` is empty = all databases allowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserGrantRequest {
    pub username: String,
    pub db_id:    u16,
}

/// Revoke access to a specific database. Caller must be admin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRevokeRequest {
    pub username: String,
    pub db_id:    u16,
}

/// Query user information. `username = None` means the calling user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfoRequest {
    #[serde(default)]
    pub username: Option<String>,
}

/// Change a user's role (admin only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSetRoleRequest {
    pub username: String,
    /// New role: "admin", "readwrite", "readonly".
    pub role: String,
}

fn default_readwrite_role() -> String { "readwrite".into() }

// Admin / cluster

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitRequest {
    pub n_keepers:  usize,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSetRequest {
    pub param: String,
    pub value: String,
}

// Replication payloads

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStartPayload {
    pub node_id:        u64,
    pub highest_seq:    u64,
    /// Cluster secret — Core verifies this before registering the keeper.
    /// Must match [auth] cluster_secret on the Core node.
    #[serde(default)]
    pub cluster_secret: String,
    /// How many live (non-expired) keys this keeper holds.
    /// God uses this to know when warm-up is complete.
    #[serde(default)]
    pub key_count:      u64,
    /// Keeper software version for compatibility checks.
    #[serde(default)]
    pub version:        u8,
    /// The keeper's own replication listen address (e.g. "10.0.0.2:7379").
    /// Core uses this to dial back for outbound write replication (Hermes).
    #[serde(default)]
    pub replication_addr: String,
    /// Human-readable node name from config.node.node_id (e.g. "hypnos-1").
    /// Used for display in keeper-list; distinct from the numeric node_id.
    #[serde(default)]
    pub node_name: String,
    /// RAM / cold-store grant in bytes this keeper is offering to the cluster.
    /// Core stores this in KeeperInfo and displays it in keeper-list.
    #[serde(default)]
    pub pool_bytes: u64,
}

/// Sent by Keeper after all PushKey frames for the warm-up phase are sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncCompletePayload {
    pub node_id:     u64,
    /// How many PushKey frames were sent during warm-up.
    pub pushed_keys: u64,
    pub highest_seq: u64,
}

/// Sent by a follower Core when a write is received but this node is not the
/// Raft leader. Contains the leader's client-facing address so the client can
/// reconnect and retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderRedirectPayload {
    /// Client-facing address of the current leader (e.g. "10.0.0.1:6379").
    /// Empty if the leader is unknown (election in progress).
    pub leader_addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckPayload {
    pub seq:     u64,
    pub node_id: u64,
    pub ok:      bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushKeyPayload {
    #[serde(with = "serde_bytes")]
    pub key:     Vec<u8>,
    pub value:   Value,
    /// WAL sequence number for this write.
    pub seq:     u64,
    /// TTL in milliseconds (0 = no expiry).
    pub ttl_ms:  u64,
    pub slot:    u16,
    /// If true, delete this key from cold store instead of writing it.
    #[serde(default)]
    pub deleted: bool,
    /// Database namespace index (0 = default database).
    /// The `key` field already embeds this as a 2-byte big-endian prefix;
    /// `db_id` is carried separately for Keeper-side observability.
    #[serde(default)]
    pub db_id: u16,
}

// ── Database namespace payloads ───────────────────────────────────────────────

/// Payload for SELECT — switch active database on the current connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectRequest {
    /// Database index to select (0 = default). Must be < config.databases.max_databases.
    pub db_id: u16,
    /// Named database — if non-empty, the server resolves the name to an ID.
    /// Takes priority over `db_id` when the server finds a matching name.
    #[serde(default)]
    pub name: String,
}

/// Payload for DBSIZE — count live (non-expired) keys in a database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbSizeRequest {
    /// Database to count. `None` = use the connection's active database.
    #[serde(default)]
    pub db_id: Option<u16>,
    /// Named database — resolved server-side to an ID.
    #[serde(default)]
    pub name: String,
}

/// Payload for FLUSHDB — delete all keys in a database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlushDbRequest {
    /// Database to flush. `None` = use the connection's active database.
    #[serde(default)]
    pub db_id: Option<u16>,
    /// Named database — resolved server-side to an ID.
    #[serde(default)]
    pub name: String,
    /// If true, also send delete tombstones to Keepers (default true).
    #[serde(default = "default_true")]
    pub sync: bool,
}

/// Payload for DB-CREATE — register a named database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbCreateRequest {
    /// Logical name for the database (e.g. "analytics", "cache", "staging").
    /// Must be unique. Letters, digits, hyphens, and underscores only.
    pub name: String,
    /// Explicit numeric ID to assign. If None, the server picks the next
    /// available ID (starting from 1; 0 is always the default database).
    #[serde(default)]
    pub db_id: Option<u16>,
}

/// One entry in a DB-LIST response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbInfo {
    pub name: String,
    pub id: u16,
}

/// Payload for DB-DROP — unregister a named database.
/// Does NOT delete data — keys remain under the numeric ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbDropRequest {
    pub name: String,
}

fn default_true() -> bool { true }

/// Payload for SCAN — cursor-based iteration over keys in a database.
///
/// Cursor is an opaque offset into the sorted key list. A cursor of 0 starts
/// a new scan. Returns `(next_cursor, Vec<key>)` — `next_cursor == 0` signals
/// the scan is complete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRequest {
    /// Cursor position. 0 = begin new scan.
    #[serde(default)]
    pub cursor: u64,
    /// Optional glob pattern (supports `*`, `prefix*`, `*suffix`, `*sub*`, exact).
    #[serde(default)]
    pub pattern: Option<String>,
    /// Max keys to return per call. Actual count may be lower. Default 10, max 1000.
    #[serde(default = "default_scan_count")]
    pub count: u64,
}

fn default_scan_count() -> u64 { 10 }

/// Payload for MGET — bulk key fetch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MGetRequest {
    pub keys: Vec<Vec<u8>>,
}

/// Payload for MSET — bulk key-value set. Each tuple is `(key, value, ttl_ms)`.
/// `ttl_ms = 0` means no expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MSetRequest {
    pub pairs: Vec<(Vec<u8>, Value, u64)>,
}

/// Heartbeat payload carrying keeper stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    pub node_id:    u64,
    /// Total bytes granted to this keeper's cold store (0 if untracked).
    pub pool_bytes: u64,
    /// Bytes actually in use in cold store.
    pub used_bytes: u64,
    /// Number of keys in cold store.
    pub key_count:  u64,
}

// ── Herold registration frames ────────────────────────────────────────────────

/// Special flags value in a SyncStart frame that marks it as a Herold REGISTER.
/// A joining node sends SyncStart with flags=REGISTER_FLAGS and a msgpack-encoded
/// RegisterPayload instead of SyncStartPayload. Core responds with a RegisterAck.
pub const REGISTER_FLAGS: u16 = 0xFF00;

/// Sent by a joining keeper or read-replica to announce itself to the Core.
/// Carried as the payload of a SyncStart frame with flags=REGISTER_FLAGS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterPayload {
    /// Human-readable node identifier (config.node.node_id).
    pub node_id: String,
    /// "keeper" or "read-replica".
    pub role: String,
    /// RAM/cold-store grant in bytes the keeper is offering. 0 = unspecified.
    pub grant_bytes: u64,
    /// The keeper's own replication listen address, e.g. "10.0.0.2:7379".
    /// Core uses this to dial back for Hermes outbound replication.
    pub replication_addr: String,
    /// Must match the Core's config.auth.join_token.
    pub join_token: String,
}

/// Sent by Core in response to a RegisterPayload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterAck {
    pub accepted: bool,
    pub message: String,
    /// Numeric node ID assigned by Core for Raft / routing. 0 on rejection.
    pub assigned_id: u64,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_encode_decode_roundtrip() {
        let payload = rmp_serde::to_vec("hello").unwrap();
        let f = Frame {
            cmd_id: CmdId::Get,
            flags: 0x0004, // QUORUM consistency
            req_id: 42,
            payload: Bytes::from(payload),
        };
        let wire = f.encode();
        assert_eq!(wire.len(), HEADER_LEN + f.payload.len());
        let (decoded, consumed) = Frame::decode(&wire).unwrap();
        assert_eq!(consumed, wire.len());
        assert_eq!(decoded.cmd_id, CmdId::Get);
        assert_eq!(decoded.flags, 0x0004);
        assert_eq!(decoded.req_id, 42);
    }

    #[test]
    fn header_is_16_bytes() {
        let wire = Frame::ok_response(Bytes::new()).encode();
        assert_eq!(wire.len(), HEADER_LEN);
    }

    #[test]
    fn magic_bad_rejected() {
        let mut wire = Frame::ok_response(Bytes::new()).encode();
        wire[0] = 0xFF; // corrupt magic
        assert!(Frame::decode(&wire).is_err());
    }

    #[test]
    fn consistency_decoding() {
        let f = Frame { cmd_id: CmdId::Set, flags: 0b0000_0000_0000_1100, req_id: 0, payload: Bytes::new() };
        // flags=0b1100 → bits 3-2 = 0b11 = 3 → One (protocol: 11=ONE)
        assert_eq!(f.consistency(), ConsistencyLevel::One);
        // Verify full mapping:
        assert_eq!(ConsistencyLevel::from(3u8), ConsistencyLevel::One);
        assert_eq!(ConsistencyLevel::from(0u8), ConsistencyLevel::Eventual);
        assert_eq!(ConsistencyLevel::from(1u8), ConsistencyLevel::Quorum);
        assert_eq!(ConsistencyLevel::from(2u8), ConsistencyLevel::All);
    }

    #[test]
    fn slot_from_key_range() {
        for key in [b"foo".as_ref(), b"bar", b"baz", b"hello", b"world"] {
            assert!(slot_from_key(key) < NUM_SLOTS);
        }
    }

    #[test]
    fn hash_tag_routing() {
        let s1 = slot_from_key(b"user:{alice}:profile");
        let s2 = slot_from_key(b"user:{alice}:settings");
        assert_eq!(s1, s2, "same hash tag must map to same slot");
    }

    #[test]
    fn cmd_id_roundtrip() {
        for &cmd in &[
            CmdId::Get, CmdId::Set, CmdId::Del, CmdId::ZAdd, CmdId::Incr,
            CmdId::JsonGet, CmdId::JsonSet, CmdId::Auth, CmdId::Ok, CmdId::Error,
            CmdId::SyncStart, CmdId::PushKey, CmdId::Select, CmdId::DbSize, CmdId::FlushDb,
            CmdId::GenerateJoinToken,
        ] {
            assert_eq!(CmdId::try_from(cmd as u8).unwrap(), cmd);
        }
    }

    #[test]
    fn unknown_cmd_rejected() {
        assert!(CmdId::try_from(0xFFu8).is_err());
    }

    #[test]
    fn encode_decode_round_trip() {
        let payload_data = rmp_serde::to_vec(&"test payload").unwrap();
        let original = Frame {
            cmd_id: CmdId::Set,
            flags: 0x0034, // slot hint in upper bits + QUORUM (bits 3-2 = 01)
            req_id: 99,
            payload: Bytes::from(payload_data.clone()),
        };
        let wire = original.encode();

        // Header must start with magic
        assert_eq!(&wire[0..4], &MAGIC.to_be_bytes());
        // Version byte
        assert_eq!(wire[4], PROTOCOL_VERSION);
        // Total length = HEADER_LEN + payload
        assert_eq!(wire.len(), HEADER_LEN + payload_data.len());

        let (decoded, consumed) = Frame::decode(&wire).unwrap();
        assert_eq!(consumed, wire.len());
        assert_eq!(decoded.cmd_id, original.cmd_id);
        assert_eq!(decoded.flags, original.flags);
        assert_eq!(decoded.req_id, original.req_id);
        assert_eq!(decoded.payload, original.payload);
    }

    #[test]
    fn decode_malformed_magic() {
        // Build a valid frame then corrupt every byte of the magic
        let wire = Frame {
            cmd_id: CmdId::Get,
            flags: 0,
            req_id: 0,
            payload: Bytes::new(),
        }
        .encode();

        // Wrong first byte
        let mut bad = wire.clone();
        bad[0] = 0x00;
        assert!(Frame::decode(&bad).is_err());

        // All-zero magic
        let mut bad = wire.clone();
        bad[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        assert!(Frame::decode(&bad).is_err());

        // Off-by-one in last magic byte
        let mut bad = wire.clone();
        bad[3] ^= 0x01;
        assert!(Frame::decode(&bad).is_err());
    }

    #[test]
    fn decode_truncated_header() {
        // Empty buffer
        assert!(Frame::decode(&[]).is_err());
        // 1 byte
        assert!(Frame::decode(&[0x4D]).is_err());
        // HEADER_LEN - 1 bytes
        let wire = Frame::ok_response(Bytes::new()).encode();
        assert!(Frame::decode(&wire[..HEADER_LEN - 1]).is_err());
    }

    #[test]
    fn decode_payload_too_large() {
        // Construct a header that claims payload_len > 10 MB.
        // Frame::decode does not have an explicit max-payload guard, but it
        // will fail with "incomplete payload" because the buffer doesn't
        // actually contain 10 MB+ of data.
        let huge_len: u32 = 10 * 1024 * 1024 + 1; // 10 MB + 1
        let mut buf = Vec::with_capacity(HEADER_LEN);
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.push(PROTOCOL_VERSION);
        buf.push(CmdId::Set as u8);
        buf.extend_from_slice(&0u16.to_be_bytes()); // flags
        buf.extend_from_slice(&huge_len.to_be_bytes()); // payload_len
        buf.extend_from_slice(&0u32.to_be_bytes()); // req_id
        assert_eq!(buf.len(), HEADER_LEN);

        // Buffer only has the header — far less than the claimed payload.
        let err = Frame::decode(&buf);
        assert!(err.is_err(), "frame with payload_len > 10 MB and insufficient data must fail");
    }

    #[test]
    fn decode_zero_payload() {
        let frame = Frame {
            cmd_id: CmdId::Ok,
            flags: 0,
            req_id: 0,
            payload: Bytes::new(),
        };
        let wire = frame.encode();
        assert_eq!(wire.len(), HEADER_LEN);

        let (decoded, consumed) = Frame::decode(&wire).unwrap();
        assert_eq!(consumed, HEADER_LEN);
        assert!(decoded.payload.is_empty());
        assert_eq!(decoded.cmd_id, CmdId::Ok);
    }

    #[test]
    fn consistency_flag_extraction() {
        // EVENTUAL = 0b00 in bits 3-2
        let f = Frame { cmd_id: CmdId::Get, flags: 0b0000_0000_0000_0000, req_id: 0, payload: Bytes::new() };
        assert_eq!(f.consistency(), ConsistencyLevel::Eventual);

        // QUORUM = 0b01 in bits 3-2 → flags = 0b0100
        let f = Frame { cmd_id: CmdId::Get, flags: 0b0000_0000_0000_0100, req_id: 0, payload: Bytes::new() };
        assert_eq!(f.consistency(), ConsistencyLevel::Quorum);

        // ALL = 0b10 in bits 3-2 → flags = 0b1000
        let f = Frame { cmd_id: CmdId::Get, flags: 0b0000_0000_0000_1000, req_id: 0, payload: Bytes::new() };
        assert_eq!(f.consistency(), ConsistencyLevel::All);

        // ONE = 0b11 in bits 3-2 → flags = 0b1100
        let f = Frame { cmd_id: CmdId::Get, flags: 0b0000_0000_0000_1100, req_id: 0, payload: Bytes::new() };
        assert_eq!(f.consistency(), ConsistencyLevel::One);

        // Verify other bits don't interfere: set slot hint bits + reserved bits
        let f = Frame { cmd_id: CmdId::Get, flags: 0b1111_1111_1111_0111, req_id: 0, payload: Bytes::new() };
        // bits 3-2 = 0b01 → Quorum, even though all other bits are set
        assert_eq!(f.consistency(), ConsistencyLevel::Quorum);

        // Round-trip through encode/decode preserves consistency
        let f = Frame { cmd_id: CmdId::Set, flags: 0b0000_0000_0000_1000, req_id: 1, payload: Bytes::new() };
        let wire = f.encode();
        let (decoded, _) = Frame::decode(&wire).unwrap();
        assert_eq!(decoded.consistency(), ConsistencyLevel::All);
    }

    #[test]
    fn req_id_zero_vs_nonzero() {
        // req_id = 0 → single-plex
        let single = Frame {
            cmd_id: CmdId::Get,
            flags: 0,
            req_id: 0,
            payload: Bytes::from(rmp_serde::to_vec(&"key1").unwrap()),
        };
        let wire = single.encode();
        let (decoded, _) = Frame::decode(&wire).unwrap();
        assert_eq!(decoded.req_id, 0, "req_id=0 means single-plex");

        // req_id = 1 → multiplexed
        let mux1 = Frame {
            cmd_id: CmdId::Get,
            flags: 0,
            req_id: 1,
            payload: Bytes::from(rmp_serde::to_vec(&"key2").unwrap()),
        };
        let wire = mux1.encode();
        let (decoded, _) = Frame::decode(&wire).unwrap();
        assert_eq!(decoded.req_id, 1, "req_id=1 means multiplexed");

        // req_id = u32::MAX → still valid multiplexed
        let mux_max = Frame {
            cmd_id: CmdId::Set,
            flags: 0,
            req_id: u32::MAX,
            payload: Bytes::new(),
        };
        let wire = mux_max.encode();
        let (decoded, _) = Frame::decode(&wire).unwrap();
        assert_eq!(decoded.req_id, u32::MAX, "max req_id should survive round-trip");

        // Verify ok_response_for echoes req_id correctly
        let resp = Frame::ok_response_for(Bytes::new(), 42);
        assert_eq!(resp.req_id, 42);
        let resp0 = Frame::ok_response(Bytes::new());
        assert_eq!(resp0.req_id, 0);
    }
}
