use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, crate::MnemeError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MnemeConfig {
    pub node: NodeConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub cluster: ClusterConfig,
    #[serde(default)]
    pub read_replicas: ReadReplicaConfig,
    #[serde(default)]
    pub persistence: PersistenceConfig,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub connections: ConnectionConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub databases: DatabaseConfig,
}

impl Default for MnemeConfig {
    fn default() -> Self {
        Self {
            node: NodeConfig::default(),
            memory: MemoryConfig::default(),
            cluster: ClusterConfig::default(),
            read_replicas: ReadReplicaConfig::default(),
            persistence: PersistenceConfig::default(),
            tls: TlsConfig::default(),
            auth: AuthConfig::default(),
            logging: LoggingConfig::default(),
            connections: ConnectionConfig::default(),
            limits: LimitsConfig::default(),
            databases: DatabaseConfig::default(),
        }
    }
}

impl MnemeConfig {
    pub fn from_file(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| crate::MnemeError::Config(format!("Cannot read {path}: {e}")))?;
        toml::from_str(&text)
            .map_err(|e| crate::MnemeError::Config(format!("Parse error in {path}: {e}")))
    }

    pub fn is_solo(&self) -> bool {
        self.node.role == NodeRole::Solo
    }

    /// Convenience: client listen address
    pub fn client_addr(&self) -> String {
        format!("{}:{}", self.node.bind, self.node.port)
    }

    /// Convenience: replication listen address.
    ///
    /// When `bind` is the wildcard `"0.0.0.0"` or `"::"`, the socket listens on all
    /// interfaces but that address is not routable by remote nodes.  In that case
    /// we detect the primary outbound IP so that Core can dial back to this address.
    pub fn replication_addr(&self) -> String {
        let port = self.node.rep_port;
        let bind = &self.node.bind;
        if bind == "0.0.0.0" || bind == "::" {
            if let Some(ip) = detect_outbound_ip() {
                return format!("{ip}:{port}");
            }
        }
        format!("{bind}:{port}")
    }

    /// Convenience: metrics listen address
    pub fn metrics_addr(&self) -> String {
        format!("{}:{}", self.node.bind, self.node.metrics_port)
    }
}

// ── Node ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NodeRole {
    Core,
    Keeper,
    Solo,
    ReadReplica,
}

impl Default for NodeRole {
    fn default() -> Self { Self::Solo }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub role: NodeRole,
    /// Human-readable unique ID across the cluster (e.g. "mnemosyne-1")
    pub node_id: String,
    pub bind: String,
    /// Client-facing port. Keepers do not serve clients; use 0 to omit from config.
    #[serde(default)]
    pub port: u16,
    pub rep_port: u16,
    pub metrics_port: u16,
    /// I/O thread count. Set to 0 for auto-detect.
    /// NOTE: io_uring not yet implemented — this field is currently unused (epoll only).
    /// Planned for a future release.
    #[serde(default)]
    pub io_threads: usize,
    /// Join token printed by `mneme-core init`. Empty for first/solo node.
    #[serde(default)]
    pub join_token: String,
    /// For keeper / read-replica: full address of the Core's replication port.
    /// Format: "IP:PORT"  e.g. "10.0.0.1:7379".
    /// Empty string on core and solo nodes.
    #[serde(default)]
    pub core_addr: String,
    /// Ordered list of failover Core addresses for read-replicas.
    /// When `core_addr` is unreachable the replica tries these in order.
    /// Format: ["10.0.0.2:7379", "10.0.0.3:7379"]
    #[serde(default)]
    pub failover_addrs: Vec<String>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            role: NodeRole::Solo,
            node_id: "mneme-solo".into(),
            bind: "0.0.0.0".into(),
            port: 6379,
            rep_port: 7379,
            metrics_port: 9090,
            io_threads: 0,
            join_token: String::new(),
            core_addr: String::new(),
            failover_addrs: Vec::new(),
        }
    }
}

// ── Memory ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Human-readable e.g. "1gb", "512mb". Parsed by parse_bytes().
    pub pool_bytes: String,
    #[serde(default = "default_eviction_threshold")]
    pub eviction_threshold: f64,
    #[serde(default = "default_true")]
    pub huge_pages: bool,
    #[serde(default = "default_promotion_threshold")]
    pub promotion_threshold: u8,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            pool_bytes: "256mb".into(),
            eviction_threshold: 0.90,
            huge_pages: true,
            promotion_threshold: 10,
        }
    }
}

impl MemoryConfig {
    /// Parse pool_bytes string into u64 bytes.
    pub fn pool_bytes_u64(&self) -> u64 {
        parse_bytes(&self.pool_bytes).unwrap_or(256 * 1024 * 1024)
    }
}

// ── Cluster ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    #[serde(default = "default_heartbeat_ms")]
    pub heartbeat_ms: u64,
    #[serde(default = "default_election_min_ms")]
    pub election_min_ms: u64,
    #[serde(default = "default_election_max_ms")]
    pub election_max_ms: u64,
    #[serde(default = "default_quorum_timeout_ms")]
    pub quorum_timeout_ms: u64,
    /// Maximum time (ms) Core waits for a keeper ACK on QUORUM/ALL writes.
    /// Raise this on high-latency or spinning-disk keeper nodes.
    /// Default 800 ms matches the p99 SET QUORUM target in CLAUDE.md.
    #[serde(default = "default_write_timeout_ms")]
    pub write_timeout_ms: u64,
    /// Addresses of other Core nodes for multi-core Raft HA.
    /// Format: ["core-2:7379", "core-3:7379"]
    /// Empty = single-core mode.
    #[serde(default)]
    pub peers: Vec<String>,
    /// Unique numeric Raft node ID for this Core in the Raft cluster.
    /// Must be unique across all Core nodes. Default 1.
    #[serde(default = "default_raft_id")]
    pub raft_id: u64,
    /// This node's own routable Raft address (e.g. "mneme-core-1:7379").
    /// Used so peers can redirect clients to the leader's address.
    /// If empty, leader redirect will show "unknown".
    #[serde(default)]
    pub advertise_addr: String,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            heartbeat_ms: 500,
            election_min_ms: 1500,
            election_max_ms: 3000,
            quorum_timeout_ms: 200,
            write_timeout_ms: 800,
            peers: Vec::new(),
            raft_id: 1,
            advertise_addr: String::new(),
        }
    }
}

// ── Read replicas ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadReplicaConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_lag_alert_ms")]
    pub lag_alert_ms: u64,
}

impl Default for ReadReplicaConfig {
    fn default() -> Self {
        Self { enabled: false, lag_alert_ms: 50 }
    }
}

// ── Persistence ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceConfig {
    pub wal_dir: String,
    #[serde(default = "default_snap_interval")]
    pub snapshot_interval_s: u64,
    #[serde(default = "default_wal_max_mb")]
    pub wal_max_mb: u64,
    #[serde(default = "default_wal_compression")]
    pub wal_compression: String,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            wal_dir: "/var/lib/mneme".into(),
            snapshot_interval_s: 60,
            wal_max_mb: 256,
            wal_compression: "zstd".into(),
        }
    }
}

impl PersistenceConfig {
    pub fn wal_path(&self) -> String { format!("{}/mneme.wal", self.wal_dir) }
    pub fn snap_path(&self) -> String { format!("{}/mneme.snap", self.wal_dir) }
    pub fn cold_db_path(&self) -> String { format!("{}/cold.redb", self.wal_dir) }
    pub fn wal_max_bytes(&self) -> u64 { self.wal_max_mb * 1024 * 1024 }
}

// ── TLS ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
    /// CA certificate used to sign node certs and validate mTLS peers.
    /// Written here by `auto_generate`; read from here for mTLS validation.
    #[serde(default = "default_ca_cert")]
    pub ca_cert: String,
    #[serde(default = "default_true")]
    pub auto_generate: bool,
    /// Additional Subject Alternative Names to embed in the auto-generated
    /// node certificate.  Accepts IP addresses and DNS names as strings.
    ///
    /// Use this when the node is reachable via a public/EIP address that
    /// differs from its private LAN IP, so that `mneme-cli --host PUBLIC_IP`
    /// works without `--insecure`.
    ///
    /// Example: extra_sans = ["34.30.37.135", "cache.example.com"]
    #[serde(default)]
    pub extra_sans: Vec<String>,
    /// TLS server name used for SNI when connecting to another node.
    /// Must match a DNS SAN in the server's certificate.
    /// Default "mneme.local" matches the auto-generated cert.
    #[serde(default = "default_server_name")]
    pub server_name: String,
    /// Optional separate cert/key for the client-facing port (6379).
    /// When set, the client port uses this cert (e.g. a publicly-trusted cert)
    /// so clients don't need `--ca-cert`. The internal CA/mTLS is only for
    /// cluster communication (Core↔Keeper, Core↔Core, Core↔ReadReplica).
    #[serde(default)]
    pub client_cert: String,
    #[serde(default)]
    pub client_key: String,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            cert: "/var/lib/mneme/node.crt".into(),
            key: "/var/lib/mneme/node.key".into(),
            ca_cert: "/etc/mneme/ca.crt".into(),
            auto_generate: true,
            extra_sans: Vec::new(),
            server_name: "mneme.local".into(),
            client_cert: String::new(),
            client_key: String::new(),
        }
    }
}

// ── Auth ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Path to the SQLite users database (Core / Solo only).
    /// Keeper and read-replica nodes do not serve client authentication and
    /// should omit this field from their config — it defaults to an empty
    /// string so that the TOML parser does not reject their config file.
    #[serde(default = "default_users_db")]
    pub users_db: String,
    /// HMAC-SHA256 signing secret shared across all cluster nodes.
    #[serde(default = "default_cluster_secret")]
    pub cluster_secret: String,
    #[serde(default = "default_token_ttl_h")]
    pub token_ttl_h: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            users_db: default_users_db(),
            cluster_secret: "change-me-in-production".into(),
            token_ttl_h: 24,
        }
    }
}

// ── Connections (Charon) ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    #[serde(default = "default_max_total")]
    pub max_total: usize,
    #[serde(default = "default_max_per_ip")]
    pub max_per_ip: u32,
    #[serde(default = "default_idle_timeout_s")]
    pub idle_timeout_s: u64,
    #[serde(default = "default_tcp_keepalive_s")]
    pub tcp_keepalive_s: u64,
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            max_total: 100_000,
            max_per_ip: 1_000,
            idle_timeout_s: 30,
            tcp_keepalive_s: 10,
            max_in_flight: 200_000,
            request_timeout_ms: 5_000,
        }
    }
}

// ── Limits (Nemesis) ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_key_bytes")]
    pub max_key_bytes: usize,
    #[serde(default = "default_max_value_bytes")]
    pub max_value_bytes: usize,
    #[serde(default = "default_max_field_count")]
    pub max_field_count: usize,
    #[serde(default = "default_max_batch_keys")]
    pub max_batch_keys: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_key_bytes: 512,
            max_value_bytes: 10 * 1024 * 1024,
            max_field_count: 65_536,
            max_batch_keys: 1_000,
        }
    }
}

// ── Databases ─────────────────────────────────────────────────────────────────

/// Configuration for isolated keyspace namespaces.
///
/// Databases work like Redis SELECT: each connection tracks an active db_id (0-based).
/// The active database is changed with SELECT. Keys in different databases are
/// completely isolated — db_id 0 is the default and is equivalent to the classic
/// single-namespace behaviour.
///
/// Internally, shard keys are prefixed with 2 bytes (db_id big-endian) so isolation
/// is enforced at storage time without any per-database locking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Maximum number of isolated keyspace databases.
    /// Valid db_ids are 0 .. max_databases-1.
    /// Default 16 (same as Redis).
    #[serde(default = "default_max_databases")]
    pub max_databases: u16,
    /// Default database index for new connections.
    #[serde(default)]
    pub default_database: u16,
    /// Static name → ID aliases loaded at startup.
    /// Used to seed the runtime name registry.  Names created via DB-CREATE
    /// at runtime are stored in `{data_dir}/databases.json` and merged on top.
    ///
    /// Example (in mneme.toml):
    /// ```toml
    /// [databases.names]
    /// analytics = 1
    /// cache     = 2
    /// staging   = 3
    /// ```
    #[serde(default)]
    pub names: std::collections::HashMap<String, u16>,
}

fn default_max_databases() -> u16 { 16 }

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            max_databases: 16,
            default_database: 0,
            names: std::collections::HashMap::new(),
        }
    }
}

// ── Logging ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self { level: "info".into(), format: "json".into() }
    }
}

// ── defaults ──────────────────────────────────────────────────────────────────

fn default_users_db() -> String { "/var/lib/mneme/users.db".into() }
fn default_true() -> bool { true }
fn default_eviction_threshold() -> f64 { 0.90 }
fn default_promotion_threshold() -> u8 { 10 }
fn default_heartbeat_ms() -> u64 { 500 }
fn default_election_min_ms() -> u64 { 1500 }
fn default_election_max_ms() -> u64 { 3000 }
fn default_quorum_timeout_ms() -> u64 { 200 }
fn default_write_timeout_ms() -> u64 { 800 }
fn default_raft_id() -> u64 { 1 }
fn default_lag_alert_ms() -> u64 { 50 }
fn default_snap_interval() -> u64 { 60 }
fn default_wal_max_mb() -> u64 { 256 }
fn default_wal_compression() -> String { "zstd".into() }
fn default_ca_cert() -> String { "/etc/mneme/ca.crt".into() }
fn default_cluster_secret() -> String { "change-me-in-production".into() }
fn default_token_ttl_h() -> u64 { 24 }
fn default_max_total() -> usize { 100_000 }
fn default_max_per_ip() -> u32 { 1_000 }
fn default_idle_timeout_s() -> u64 { 30 }
fn default_tcp_keepalive_s() -> u64 { 10 }
fn default_max_in_flight() -> usize { 200_000 }
fn default_request_timeout_ms() -> u64 { 5_000 }
fn default_max_key_bytes() -> usize { 512 }
fn default_max_value_bytes() -> usize { 10 * 1024 * 1024 }
fn default_max_field_count() -> usize { 65_536 }
fn default_max_batch_keys() -> usize { 1_000 }
fn default_server_name() -> String { "mneme.local".into() }

// ── IP detection ──────────────────────────────────────────────────────────────

/// Return the primary outbound non-loopback IP of this machine.
///
/// Uses the UDP socket trick: bind to 0.0.0.0:0, "connect" (no packets sent)
/// to an external address, then read the OS-chosen source IP.  Falls back to
/// None if the machine has no default route.
fn detect_outbound_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

// ── byte parser ───────────────────────────────────────────────────────────────

pub fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if let Some(n) = s.strip_suffix("gb") {
        n.trim().parse::<u64>().ok().map(|v| v * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("mb") {
        n.trim().parse::<u64>().ok().map(|v| v * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("kb") {
        n.trim().parse::<u64>().ok().map(|v| v * 1024)
    } else {
        s.parse::<u64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bytes_variants() {
        assert_eq!(parse_bytes("1gb"), Some(1024*1024*1024));
        assert_eq!(parse_bytes("256mb"), Some(256*1024*1024));
        assert_eq!(parse_bytes("512kb"), Some(512*1024));
        assert_eq!(parse_bytes("1000"), Some(1000));
        assert_eq!(parse_bytes("bad"), None);
    }

    #[test]
    fn default_config_is_solo() {
        let cfg = MnemeConfig::default();
        assert!(cfg.is_solo());
        assert_eq!(cfg.node.role, NodeRole::Solo);
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = MnemeConfig::default();
        let s = toml::to_string(&cfg).unwrap();
        let back: MnemeConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.node.node_id, cfg.node.node_id);
        assert_eq!(back.node.port, cfg.node.port);
    }

    #[test]
    fn persistence_paths() {
        let p = PersistenceConfig::default();
        assert!(p.wal_path().ends_with("/mneme.wal"));
        assert!(p.snap_path().ends_with("/mneme.snap"));
        assert!(p.cold_db_path().ends_with("/cold.redb"));
        assert_eq!(p.wal_max_bytes(), 256 * 1024 * 1024);
    }

    #[test]
    fn read_replica_variant_parses() {
        let toml_str = r#"
[node]
role = "read-replica"
node_id = "god-2"
bind = "0.0.0.0"
port = 6379
rep_port = 7379
metrics_port = 9090
"#;
        let cfg: MnemeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.node.role, NodeRole::ReadReplica);
    }
}