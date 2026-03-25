// mneme-cli — Command-line client for MnemeCache.
// Connects via TLS 1.3, authenticates, then dispatches commands.

use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use clap::{Parser, Subcommand};
use mneme_common::{CmdId, Frame, SelectRequest, Value};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::debug;

const FRAME_HEADER: usize = mneme_common::HEADER_LEN; // 16B: magic+ver+cmd+flags+plen+req_id

// Sentinels used to detect "user did not override this flag".
const DEFAULT_HOST: &str = "127.0.0.1:6379";
const DEFAULT_CA_CERT: &str = "/etc/mneme/ca.crt";
const DEFAULT_CONSISTENCY: &str = "quorum";
const DEFAULT_DB: u16 = 0;

// ── Profile system ─────────────────────────────────────────────────────────────

/// Per-profile connection parameters. All fields are optional; absent fields
/// fall back to CLI defaults. Stored in `~/.mneme/profiles.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProfileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Stored password (plain). Users who want better security should store a
    /// token via `auth-token` and set that instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consistency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,
}

/// Full profiles file: optional default profile name + named profiles map.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProfileFile {
    /// Name of the profile to use when `--profile` is omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,
}

fn profiles_path() -> PathBuf {
    let mut p = dirs_home();
    p.push(".mneme");
    p.push("profiles.toml");
    p
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn load_profiles() -> Result<ProfileFile> {
    let path = profiles_path();
    if !path.exists() {
        return Ok(ProfileFile::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text)
        .with_context(|| format!("parse {}", path.display()))
}

fn save_profiles(pf: &ProfileFile) -> Result<()> {
    let path = profiles_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(pf)
        .context("serialize profiles")?;
    std::fs::write(&path, text)
        .with_context(|| format!("write {}", path.display()))
}

/// Effective configuration built from CLI args with profile as fallback.
struct EffectiveConfig {
    pub host: String,
    pub ca_cert: String,
    pub insecure: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    pub consistency: String,
    pub db: u16,
}

fn merge_config(cli: &Cli, profile: Option<&ProfileConfig>) -> EffectiveConfig {
    let p = profile;

    // For each CLI field: if the CLI value equals the default sentinel, use the
    // profile value (if available); otherwise keep the CLI value.
    let host = if cli.host == DEFAULT_HOST {
        p.and_then(|p| p.host.clone()).unwrap_or_else(|| DEFAULT_HOST.into())
    } else {
        cli.host.clone()
    };
    let ca_cert = if cli.ca_cert == DEFAULT_CA_CERT {
        p.and_then(|p| p.ca_cert.clone()).unwrap_or_else(|| DEFAULT_CA_CERT.into())
    } else {
        cli.ca_cert.clone()
    };
    let insecure = if cli.insecure {
        true
    } else {
        p.and_then(|p| p.insecure).unwrap_or(false)
    };
    let consistency = if cli.consistency == DEFAULT_CONSISTENCY {
        p.and_then(|p| p.consistency.clone()).unwrap_or_else(|| DEFAULT_CONSISTENCY.into())
    } else {
        cli.consistency.clone()
    };
    let db = if cli.db == DEFAULT_DB {
        p.and_then(|p| p.db).unwrap_or(DEFAULT_DB)
    } else {
        cli.db
    };
    // Auth: CLI args take priority; if absent, use profile.
    let token = cli.token.clone()
        .or_else(|| p.and_then(|p| p.token.clone()));
    let username = cli.username.clone()
        .or_else(|| p.and_then(|p| p.username.clone()));
    let password = cli.password.clone()
        .or_else(|| p.and_then(|p| p.password.clone()));

    EffectiveConfig { host, ca_cert, insecure, username, password, token, consistency, db }
}

// ── CLI definition ─────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "mneme-cli",
    about = "MnemeCache command-line client",
    long_about = "MnemeCache command-line client — high-performance distributed in-memory cache.\n\
\n\
QUICK START:\n\
  mneme-cli -u admin -p secret get mykey\n\
  mneme-cli -u admin -p secret set mykey 'hello world'\n\
  mneme-cli --ca-cert /etc/mneme/ca.crt -u admin -p secret stats\n\
\n\
AUTHENTICATION:\n\
  Token:     mneme-cli -t TOKEN get mykey\n\
  Creds:     mneme-cli -u USERNAME -p PASSWORD get mykey\n\
  Get token: mneme-cli -u admin -p secret auth-token\n\
\n\
CONSISTENCY LEVELS (write commands):\n\
  -c eventual   ~150µs — async replication, AP mode\n\
  -c one        ~400µs — first Keeper ACK\n\
  -c quorum     ~800µs — majority ACKs (default)\n\
  -c all        ~2ms   — every Keeper ACK\n\
\n\
KEY TYPES: string, hash, list, zset (sorted set), counter, json\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s set user:1 alice --ttl 3600\n\
  mneme-cli -u admin -p s hset profile:1 name alice age 30\n\
  mneme-cli -u admin -p s hgetall profile:1\n\
  mneme-cli -u admin -p s lpush queue job1 job2 job3\n\
  mneme-cli -u admin -p s zadd leaderboard 1500.0 alice 2300.5 bob\n\
  mneme-cli -u admin -p s incr page:views\n\
  mneme-cli -u admin -p s json-set doc:1 '$' '{\"name\":\"Widget\"}'\n\
  mneme-cli -u admin -p s json-get doc:1 '$.name'\n\
  mneme-cli -u admin -p s cluster-info\n\
  mneme-cli -u admin -p s keeper-list"
)]
struct Cli {
    /// Named connection profile from ~/.mneme/profiles.toml.
    /// All connection flags default to the profile values; explicit CLI flags override.
    /// Omit to use the profile named in `default` key, or "default" if not set.
    #[arg(long)]
    profile: Option<String>,

    /// Server address (host:port).
    #[arg(short = 'H', long, default_value = "127.0.0.1:6379")]
    host: String,

    /// CA certificate path for TLS verification.
    #[arg(long, default_value = "/etc/mneme/ca.crt")]
    ca_cert: String,

    /// Client certificate path (optional, for mTLS).
    #[arg(long)]
    cert: Option<String>,

    /// Client private key path.
    #[arg(long)]
    key: Option<String>,

    /// Skip TLS certificate verification (insecure, for dev only).
    #[arg(long)]
    insecure: bool,

    /// Authenticate with username + password.
    #[arg(short = 'u', long)]
    username: Option<String>,

    /// Password for authentication.
    #[arg(short = 'p', long)]
    password: Option<String>,

    /// Pre-issued token for token-based authentication.
    #[arg(short = 't', long)]
    token: Option<String>,

    /// Consistency level for write commands: eventual, quorum (default), all, one.
    #[arg(short = 'c', long, default_value = "quorum")]
    consistency: String,

    /// Active database index for this session (0 = default).
    /// Sent as a SELECT command immediately after authentication.
    #[arg(short = 'd', long, default_value_t = 0)]
    db: u16,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Get the value of a key.",
        long_about = "GET key\n\
\n\
Returns the string/bytes value stored at <key>. Errors if the key does not\n\
exist (KeyNotFound) or is of the wrong type (WrongType).\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s get user:1\n\
  mneme-cli -u admin -p s -c eventual get session:abc   # fastest read"
    )]
    Get { key: String },
    #[command(
        about = "Set a key to a string/bytes value.",
        long_about = "SET key value [--ttl SECONDS]\n\
\n\
Stores <value> under <key>. Overwrites any existing value and type.\n\
Use --ttl to set an expiry in seconds (0 = no expiry, default).\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s set user:1 alice\n\
  mneme-cli -u admin -p s set session:tok abc123 --ttl 3600\n\
  mneme-cli -u admin -p s -c quorum set counter 0          # default consistency\n\
  mneme-cli -u admin -p s -c all    set critical_key val   # wait for all keepers"
    )]
    Set {
        key: String,
        value: String,
        /// TTL in seconds (0 = no expiry).
        #[arg(short = 'x', long, default_value_t = 0)]
        ttl: u64,
    },
    #[command(
        about = "Delete one or more keys.",
        long_about = "DEL key [key ...]\n\
\n\
Removes each key. Returns the number of keys that were present and deleted.\n\
Non-existent keys are silently ignored.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s del user:1\n\
  mneme-cli -u admin -p s del user:1 user:2 user:3   # bulk delete"
    )]
    Del { keys: Vec<String> },
    #[command(
        about = "Check if a key exists.",
        long_about = "EXISTS key\n\
\n\
Returns true if <key> exists and has not expired, false otherwise.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s exists user:1"
    )]
    Exists { key: String },
    #[command(
        about = "Set a TTL on an existing key (seconds from now).",
        long_about = "EXPIRE key seconds\n\
\n\
Sets a TTL so the key is automatically deleted after <seconds>.\n\
If the key does not exist, returns KeyNotFound.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s expire session:abc 1800   # expire in 30 min\n\
  mneme-cli -u admin -p s expire user:1 0           # remove expiry (persist)"
    )]
    Expire { key: String, seconds: u64 },
    #[command(
        about = "Get remaining TTL in seconds.",
        long_about = "TTL key\n\
\n\
Returns the remaining time-to-live in seconds.\n\
  -1  = key exists but has no expiry\n\
  -2  = key does not exist\n\
  N>0 = seconds until automatic deletion\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s ttl session:abc"
    )]
    Ttl { key: String },
    #[command(
        about = "Hash: get a single field value.",
        long_about = "HGET key field\n\
\n\
Returns the value of <field> in the hash stored at <key>.\n\
Errors with KeyNotFound if the key or field is absent.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s hget profile:1 name"
    )]
    Hget { key: String, field: String },
    #[command(
        about = "Hash: set one or more field value pairs.",
        long_about = "HSET key field value [field value ...]\n\
\n\
Sets one or more fields in the hash at <key>. Creates the hash if it does\n\
not exist. Fields are provided as alternating positional arguments.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s hset profile:1 name alice\n\
  mneme-cli -u admin -p s hset profile:1 name alice age 30 city London"
    )]
    Hset {
        key: String,
        /// Alternating field value pairs (e.g. name alice age 30).
        pairs: Vec<String>,
    },
    #[command(
        about = "Hash: delete one or more fields.",
        long_about = "HDEL key field [field ...]\n\
\n\
Removes the specified fields from the hash at <key>.\n\
Returns the number of fields actually removed.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s hdel profile:1 age city"
    )]
    Hdel { key: String, fields: Vec<String> },
    #[command(
        about = "Hash: get all fields and values.",
        long_about = "HGETALL key\n\
\n\
Returns all field-value pairs in the hash stored at <key>.\n\
Output is formatted as a two-column table.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s hgetall profile:1"
    )]
    Hgetall { key: String },
    #[command(
        about = "List: push one or more values to the left (head).",
        long_about = "LPUSH key value [value ...]\n\
\n\
Prepends one or more values to the list at <key>. Multiple values are\n\
inserted left-to-right, so the last value ends up at the head.\n\
Creates the list if it does not exist.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s lpush queue job1\n\
  mneme-cli -u admin -p s lpush events ev3 ev2 ev1   # ev1 ends up at head"
    )]
    Lpush { key: String, values: Vec<String> },
    #[command(
        about = "List: push one or more values to the right (tail).",
        long_about = "RPUSH key value [value ...]\n\
\n\
Appends one or more values to the list at <key>. Multiple values are\n\
inserted left-to-right so the last value ends up at the tail.\n\
Creates the list if it does not exist.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s rpush queue job1 job2 job3\n\
  mneme-cli -u admin -p s rpush log 'line one' 'line two'"
    )]
    Rpush { key: String, values: Vec<String> },
    #[command(
        about = "List: pop and return the leftmost value.",
        long_about = "LPOP key\n\
\n\
Removes and returns the first (head) element of the list at <key>.\n\
Returns KeyNotFound if the list is empty or absent.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s lpop queue"
    )]
    Lpop { key: String },
    #[command(
        about = "List: pop and return the rightmost value.",
        long_about = "RPOP key\n\
\n\
Removes and returns the last (tail) element of the list at <key>.\n\
Returns KeyNotFound if the list is empty or absent.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s rpop queue"
    )]
    Rpop { key: String },
    #[command(
        about = "List: get a range of elements by index (0-based, -1 = last).",
        long_about = "LRANGE key start stop\n\
\n\
Returns the list elements between index <start> and <stop> (inclusive).\n\
Negative indices count from the tail: -1 = last element, -2 = second-to-last.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s lrange queue 0 -1    # all elements\n\
  mneme-cli -u admin -p s lrange queue 0 9     # first 10\n\
  mneme-cli -u admin -p s lrange queue -5 -1   # last 5"
    )]
    Lrange { key: String, start: i64, stop: i64 },
    #[command(
        about = "Sorted set: add members with scores.",
        long_about = "ZADD key score member [score member ...]\n\
\n\
Adds one or more members with their scores to the sorted set at <key>.\n\
If a member already exists its score is updated.\n\
Scores are 64-bit floats. Members are ranked in ascending score order.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s zadd leaderboard 1500.0 alice\n\
  mneme-cli -u admin -p s zadd leaderboard 1500.0 alice 2300.5 bob 900.0 carol"
    )]
    Zadd {
        key: String,
        /// Alternating score member pairs (e.g. 1.5 alice 2.3 bob).
        pairs: Vec<String>,
    },
    #[command(
        about = "Sorted set: get the score of a member.",
        long_about = "ZSCORE key member\n\
\n\
Returns the score (float) of <member> in the sorted set at <key>.\n\
Errors with KeyNotFound if the member is absent.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s zscore leaderboard alice"
    )]
    Zscore { key: String, member: String },
    #[command(
        about = "Sorted set: get 0-based rank of a member (ascending score).",
        long_about = "ZRANK key member\n\
\n\
Returns the rank (0 = lowest score) of <member> in the sorted set.\n\
Errors with KeyNotFound if the member is absent.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s zrank leaderboard alice"
    )]
    Zrank { key: String, member: String },
    #[command(
        about = "Sorted set: get members by rank range.",
        long_about = "ZRANGE key start stop [--withscores]\n\
\n\
Returns members between rank <start> and <stop> (inclusive, 0-based ascending).\n\
Negative indices count from the tail (-1 = highest score).\n\
Pass --withscores to include the score alongside each member.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s zrange leaderboard 0 -1              # all members\n\
  mneme-cli -u admin -p s zrange leaderboard 0 2 --withscores  # top 3 with scores\n\
  mneme-cli -u admin -p s zrange leaderboard -3 -1             # bottom 3"
    )]
    Zrange {
        key: String,
        start: i64,
        stop: i64,
        #[arg(long)]
        withscores: bool,
    },
    #[command(
        about = "Sorted set: get members with scores between min and max.",
        long_about = "ZRANGEBYSCORE key min max\n\
\n\
Returns all members whose score falls in [min, max] (inclusive), in\n\
ascending order. Scores are 64-bit floats.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s zrangebyscore leaderboard 1000.0 2000.0\n\
  mneme-cli -u admin -p s zrangebyscore leaderboard 0.0 9999.0   # everyone"
    )]
    Zrangebyscore { key: String, min: f64, max: f64 },
    #[command(
        about = "Sorted set: remove one or more members.",
        long_about = "ZREM key member [member ...]\n\
\n\
Removes the specified members from the sorted set.\n\
Returns the number of members actually removed.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s zrem leaderboard carol dave"
    )]
    Zrem { key: String, members: Vec<String> },
    #[command(
        about = "Sorted set: return the number of members.",
        long_about = "ZCARD key\n\
\n\
Returns the total number of members in the sorted set at <key>.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s zcard leaderboard"
    )]
    Zcard { key: String },

    // ── Counter commands ──────────────────────────────────────────────────────
    #[command(
        about = "Increment a key's integer value by 1.",
        long_about = "INCR key\n\
\n\
Atomically increments the integer stored at <key> by 1. If the key does not\n\
exist it is initialised to 0 before incrementing. Returns an error if the\n\
value is not a valid integer.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s incr page:views\n\
  mneme-cli -u admin -p s incr api:calls"
    )]
    Incr { key: String },
    #[command(
        about = "Decrement a key's integer value by 1.",
        long_about = "DECR key\n\
\n\
Atomically decrements the integer stored at <key> by 1. If the key does not\n\
exist it is initialised to 0 before decrementing. Returns an error if the\n\
value is not a valid integer.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s decr stock:widget"
    )]
    Decr { key: String },
    #[command(
        about = "Increment a key's integer value by N.",
        long_about = "INCRBY key delta\n\
\n\
Atomically adds <delta> (positive or negative i64) to the integer stored at\n\
<key>. Returns the new value. Creates the key (value=0) if it does not exist.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s incrby credits 100\n\
  mneme-cli -u admin -p s incrby balance -50"
    )]
    Incrby { key: String, delta: i64 },
    #[command(
        about = "Decrement a key's integer value by N.",
        long_about = "DECRBY key delta\n\
\n\
Atomically subtracts <delta> from the integer stored at <key>.\n\
Equivalent to INCRBY key -delta.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s decrby credits 10"
    )]
    Decrby { key: String, delta: i64 },
    #[command(
        about = "Increment a key's floating-point value by N.",
        long_about = "INCRBYFLOAT key delta\n\
\n\
Atomically adds the floating-point <delta> to the number stored at <key>.\n\
The value is stored as a UTF-8 decimal string. Returns the new value.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s incrbyfloat price 0.01\n\
  mneme-cli -u admin -p s incrbyfloat temperature -1.5"
    )]
    Incrbyfloat { key: String, delta: f64 },
    #[command(
        about = "Atomically set a key and return its old value.",
        long_about = "GETSET key value\n\
\n\
Sets <key> to <value> and returns the previous value in one atomic operation.\n\
Returns nil if the key did not exist. Useful for resetting counters while\n\
capturing the previous count.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s getset counter 0\n\
  # → previous counter value (now reset to 0)"
    )]
    Getset { key: String, value: String },

    // ── JSON commands ─────────────────────────────────────────────────────────
    #[command(
        about = "Get a value from a JSON document at a JSONPath.",
        long_about = "JSON-GET key path\n\
\n\
Retrieves the value at the JSONPath <path> from the JSON document stored at\n\
<key>. Use '$' or '$.field' for JSONPath notation. Returns the raw JSON string.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s json-get product:1 '$'\n\
  mneme-cli -u admin -p s json-get product:1 '$.price'\n\
  mneme-cli -u admin -p s json-get product:1 '$.tags'"
    )]
    JsonGet { key: String, path: String },
    #[command(
        about = "Set a value in a JSON document at a JSONPath.",
        long_about = "JSON-SET key path value\n\
\n\
Stores a JSON document or updates a field at <path> in the existing document.\n\
Use '$' as the path to store a complete JSON document. The <value> argument\n\
must be valid JSON.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s json-set product:1 '$' '{\"name\":\"Widget\",\"price\":9.99}'\n\
  mneme-cli -u admin -p s json-set product:1 '$.price' '10.99'"
    )]
    JsonSet { key: String, path: String, value: String },
    #[command(
        about = "Delete a path from a JSON document.",
        long_about = "JSON-DEL key path\n\
\n\
Deletes the value at <path> from the JSON document stored at <key>. If <path>\n\
is '$' the entire key is deleted. Deleting an array element shifts remaining\n\
elements left.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s json-del product:1 '$.tags[0]'\n\
  mneme-cli -u admin -p s json-del product:1 '$'    # delete the key"
    )]
    JsonDel { key: String, path: String },
    #[command(
        about = "Check whether a JSONPath exists in a JSON document.",
        long_about = "JSON-EXISTS key path\n\
\n\
Returns true if <path> exists in the JSON document at <key>, false otherwise.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s json-exists product:1 '$.price'\n\
  mneme-cli -u admin -p s json-exists product:1 '$.missing_field'"
    )]
    JsonExists { key: String, path: String },
    #[command(
        about = "Get the JSON type of a value at a JSONPath.",
        long_about = "JSON-TYPE key path\n\
\n\
Returns the JSON type of the value at <path>: object, array, string,\n\
number, boolean, or null.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s json-type product:1 '$'\n\
  mneme-cli -u admin -p s json-type product:1 '$.price'    # → number\n\
  mneme-cli -u admin -p s json-type product:1 '$.tags'     # → array"
    )]
    JsonType { key: String, path: String },
    #[command(
        about = "Append a value to a JSON array at a JSONPath.",
        long_about = "JSON-ARRAPPEND key path value\n\
\n\
Appends the JSON <value> to the array at <path> in the JSON document stored\n\
at <key>. Returns the new array length. The <path> must point to an array.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s json-arrappend product:1 '$.tags' '\"sale\"'\n\
  mneme-cli -u admin -p s json-arrappend list:1 '$' '42'"
    )]
    JsonArrappend { key: String, path: String, value: String },
    #[command(
        about = "Increment a JSON number at a JSONPath.",
        long_about = "JSON-NUMINCRBY key path delta\n\
\n\
Atomically adds the floating-point <delta> to the number at <path> in the\n\
JSON document stored at <key>. Returns the new value. The <path> must point\n\
to a number.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s json-numincrby product:1 '$.price' 0.50\n\
  mneme-cli -u admin -p s json-numincrby stats:1 '$.clicks' 1"
    )]
    JsonNumincrby { key: String, path: String, delta: f64 },

    #[command(
        about = "Revoke a session token immediately.",
        long_about = "REVOKE-TOKEN token\n\
\n\
Immediately invalidates the given session token. Any subsequent request\n\
using that token will receive TokenRevoked.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s revoke-token eyJ..."
    )]
    RevokeToken { token: String },
    #[command(
        about = "Authenticate and print the session token.",
        long_about = "AUTH-TOKEN\n\
\n\
Authenticates with --username / --password and prints the session token.\n\
Use this to cache the token for subsequent commands:\n\
\n\
  TOKEN=$(mneme-cli -u admin -p secret auth-token)\n\
  mneme-cli -t $TOKEN get mykey\n\
  mneme-cli -t $TOKEN set mykey val\n\
\n\
Tokens are valid for the duration configured in token_ttl_h (default 24h)."
    )]
    AuthToken,
    #[command(
        about = "Show the N most recent slow commands.",
        long_about = "SLOWLOG [count]\n\
\n\
Lists the <count> most recent commands that exceeded the slow-log threshold\n\
(configured in mneme.toml). Each entry shows the command, duration, and\n\
timestamp. Default count: 128.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s slowlog        # last 128 slow commands\n\
  mneme-cli -u admin -p s slowlog 20     # last 20 slow commands"
    )]
    Slowlog {
        #[arg(default_value_t = 128)]
        count: usize,
    },
    #[command(
        about = "Show Prometheus-format metrics.",
        long_about = "METRICS\n\
\n\
Returns a Prometheus-format text snapshot of all active metrics:\n\
pool memory usage, request rates, replication lag, eviction counters,\n\
Raft cluster state, and hardware performance counters.\n\
\n\
The same data is also exported on the metrics port (default :9090).\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s metrics"
    )]
    Metrics,
    #[command(
        about = "Show overall stats: keys, memory, keeper count, pool ratio.",
        long_about = "STATS\n\
\n\
Returns a one-line summary of the node state:\n\
  keys=<N>  pool_used=<bytes>  pool_max=<bytes>  keepers=<N>  pool_ratio=<0.00-1.00>\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s stats"
    )]
    Stats,
    #[command(
        about = "Show memory usage of a single key in bytes.",
        long_about = "MEMORY-USAGE key\n\
\n\
Returns the approximate number of bytes consumed by <key> in the RAM pool\n\
(includes value storage, hash table overhead, and TTL metadata).\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s memory-usage user:1"
    )]
    MemoryUsage { key: String },
    #[command(
        about = "Read a live config parameter.",
        long_about = "CONFIG param\n\
\n\
Reads the current value of <param> from the running node.\n\
\n\
COMMON PARAMS:\n\
  memory.pool_bytes      — RAM pool size in bytes\n\
  cluster.heartbeat_ms   — Raft heartbeat interval\n\
  auth.token_ttl_h       — session token lifetime\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s config memory.pool_bytes"
    )]
    Config { param: String },
    #[command(
        about = "Set a live config parameter without restarting.",
        long_about = "CONFIG-SET param value\n\
\n\
Applies a configuration change to the running node immediately — no restart\n\
required. Changes are not persisted to disk; re-apply after restart or update\n\
the config file.\n\
\n\
SUPPORTED PARAMS:\n\
  memory.pool_bytes         — RAM pool size (e.g. 2gb, 536870912)\n\
  memory.eviction_threshold — float 0.0–1.0 (default 0.90)\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s config-set memory.pool_bytes 2gb\n\
  mneme-cli -u admin -p s config-set memory.eviction_threshold 0.85"
    )]
    ConfigSet { param: String, value: String },
    #[command(
        about = "Show cluster topology, consistency modes, and node role.",
        long_about = "CLUSTER-INFO\n\
\n\
Returns a table of cluster metadata for this node:\n\
  role            — core / solo / read-replica\n\
  node_id         — unique node name\n\
  keeper_count    — connected Keeper nodes\n\
  pool_used_bytes — bytes in RAM pool\n\
  pool_max_bytes  — total pool capacity\n\
  warmup_state    — cold / warming / hot\n\
  supported_modes — which consistency levels are currently available\n\
  raft_term       — current Raft term\n\
  is_leader       — true if this node is the Raft leader\n\
  memory_pressure — pool_used / pool_max ratio\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s cluster-info"
    )]
    ClusterInfo,
    #[command(
        about = "Show the slot-to-node assignment table.",
        long_about = "CLUSTER-SLOTS\n\
\n\
Prints the CRC16 slot distribution across connected Keeper nodes.\n\
There are 16384 slots total; each Keeper owns a contiguous range.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s cluster-slots"
    )]
    ClusterSlots,
    #[command(
        about = "List all connected Keeper nodes with memory stats.",
        long_about = "KEEPER-LIST\n\
\n\
Displays a table of every Keeper node that is currently connected and\n\
has completed its warm-up sync:\n\
  node_name — human-readable name from config.node.node_id (e.g. \"hypnos-1\")\n\
  addr      — replication address (IP:port)\n\
  pool      — total RAM grant for this Keeper\n\
  used      — bytes currently in use\n\
\n\
If the table is empty, check that:\n\
  1. The Keeper has core_addr set to this node's IP:7379 in mneme.toml\n\
  2. TLS is configured identically on both nodes (same ca_cert / server_name)\n\
  3. cluster_secret matches on both nodes\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s keeper-list"
    )]
    KeeperList,
    #[command(
        about = "Show RAM pool statistics (used, max, keeper count).",
        long_about = "POOL-STATS\n\
\n\
Returns the three key pool numbers in one call:\n\
  Pool Used  — bytes currently occupied\n\
  Pool Max   — total capacity (local RAM + keeper grants)\n\
  Keepers    — number of connected Keeper nodes\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s pool-stats"
    )]
    PoolStats,
    #[command(
        about = "Wait for N Keeper nodes to acknowledge.",
        long_about = "WAIT n_keepers timeout_ms\n\
\n\
Blocks until at least <n_keepers> Keeper nodes have ACKed, or until\n\
<timeout_ms> milliseconds have elapsed. Returns the actual ack count.\n\
Useful in scripts that need to confirm replication before proceeding.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s wait 1 5000    # wait for at least 1 keeper, 5s timeout\n\
  mneme-cli -u admin -p s wait 2 10000   # wait for quorum of 2 keepers"
    )]
    Wait { n_keepers: usize, timeout_ms: u64 },
    // ── Database namespace commands ────────────────────────────────────────────
    #[command(
        about = "Switch the active database for this connection.",
        long_about = "SELECT db\n\
\n\
Switches the connection's active database. Use a numeric index (0–65535)\n\
or a named database (e.g. 'analytics'). Database 0 is the default.\n\
\n\
Maximum index is configured via databases.max_databases (default 15).\n\
Each database is a fully isolated namespace — keys do not bleed across DBs.\n\
Named databases are created with db-create and resolved server-side.\n\
\n\
Alternatively, use -d on the top-level command to select a DB for a\n\
single-shot command without an explicit SELECT:\n\
  mneme-cli -u admin -p s -d 2 set mykey val\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s select 3\n\
  mneme-cli -u admin -p s select analytics\n\
  mneme-cli -u admin -p s select cache"
    )]
    Select {
        /// Database index (0-65535) or name (e.g. 'analytics').
        db: String,
    },
    #[command(
        about = "Count live (non-expired) keys in a database.",
        long_about = "DBSIZE [--db NAME|ID]\n\
\n\
Returns the number of keys that exist and have not expired in the target\n\
database. Defaults to the connection's active database (set via -d or SELECT).\n\
Accepts a numeric database ID or a named database.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s dbsize                # count keys in default DB\n\
  mneme-cli -u admin -p s dbsize --db 2         # count keys in DB 2\n\
  mneme-cli -u admin -p s dbsize --db analytics # count keys in 'analytics' DB"
    )]
    Dbsize {
        /// Database name or numeric ID. Defaults to the connection's active database.
        #[arg(short = 'd', long)]
        db: Option<String>,
    },
    #[command(
        about = "Delete ALL keys in a database. Irreversible.",
        long_about = "FLUSHDB [--db NAME|ID] [--no-sync]\n\
\n\
Deletes every key in the target database. This operation is irreversible.\n\
By default the flush is replicated to all connected Keeper nodes.\n\
Use --no-sync to flush only the local RAM pool (Keeper data is preserved).\n\
Accepts a numeric database ID or a named database.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s flushdb                 # flush default DB (replicated)\n\
  mneme-cli -u admin -p s flushdb --db 3          # flush DB 3\n\
  mneme-cli -u admin -p s flushdb --db analytics  # flush named DB\n\
  mneme-cli -u admin -p s flushdb --no-sync       # local RAM only, no replication"
    )]
    Flushdb {
        /// Database name or numeric ID. Defaults to the connection's active database.
        #[arg(short = 'd', long)]
        db: Option<String>,
        /// Skip propagating the flush to Keeper nodes (local RAM only).
        #[arg(long)]
        no_sync: bool,
    },
    #[command(
        about = "Create a named database (admin / readwrite).",
        long_about = "DB-CREATE name [--id N]\n\
\n\
Creates a named database and registers it in the server's name registry.\n\
Names are alphanumeric (plus '-' and '_'). Once created, use the name\n\
anywhere a database ID is accepted: select, -d, dbsize, flushdb, user-grant.\n\
\n\
Optionally pin the database to a specific numeric ID with --id. If omitted\n\
the server assigns the next available ID.\n\
\n\
Named databases persist across restarts (stored in databases.json).\n\
Alternatively, static names can be set at config time under [databases.names].\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s db-create analytics\n\
  mneme-cli -u admin -p s db-create cache --id 2\n\
  mneme-cli -u admin -p s db-create staging --id 10"
    )]
    DbCreate {
        /// Name for the new database (alphanumeric, '-', '_').
        name: String,
        /// Pin the database to this numeric ID (0–65535). Server assigns one if omitted.
        #[arg(long)]
        id: Option<u16>,
    },
    #[command(
        about = "List all named databases.",
        long_about = "DB-LIST\n\
\n\
Displays all databases that have a registered name, sorted by ID.\n\
Unnamed databases (accessed only by numeric index) are not shown.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s db-list"
    )]
    DbList,
    #[command(
        about = "Remove a named database registration (admin only).",
        long_about = "DB-DROP name\n\
\n\
Removes the name-to-ID mapping from the server's registry. This does NOT\n\
delete the data in the database — keys remain accessible by numeric ID.\n\
To also remove the data, run `flushdb --db name` before dropping.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s flushdb --db staging  # clear data first\n\
  mneme-cli -u admin -p s db-drop staging       # then remove name"
    )]
    DbDrop {
        /// Name of the database to deregister.
        name: String,
    },
    // ── Bulk / scan commands ──────────────────────────────────────────────────
    #[command(
        about = "Cursor-based iteration over keys in the active database.",
        long_about = "Cursor-based key scan (SCAN). Returns up to --count keys per call.\n\
\n\
Iterate by passing the returned cursor back until the cursor is 0 (full cycle).\n\
Use an optional GLOB pattern to filter keys. The pattern supports:\n\
  *          — all keys\n\
  prefix*    — keys starting with prefix\n\
  *suffix    — keys ending with suffix\n\
  *sub*      — keys containing sub\n\
  exact      — exact match\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s scan                   # all keys, default count\n\
  mneme-cli -u admin -p s scan 'user:*' --count 100\n\
  mneme-cli -u admin -p s scan --cursor 50 'user:*'"
    )]
    Scan {
        /// Glob pattern to filter keys (default: all keys).
        #[arg(default_value = "*")]
        pattern: String,
        /// Max keys to return per call (1–1000, default 10).
        #[arg(long, default_value_t = 10)]
        count: u64,
        /// Cursor from a previous SCAN call. 0 starts a new scan.
        #[arg(long, default_value_t = 0)]
        cursor: u64,
    },
    #[command(
        about = "Return the type of a key.",
        long_about = "TYPE key\n\
\n\
Returns the data type stored under <key>:\n\
  string   — plain bytes / UTF-8 value\n\
  hash     — field-value map (HSET/HGET/HGETALL)\n\
  list     — ordered sequence (LPUSH/RPUSH/LPOP)\n\
  zset     — sorted set with float scores (ZADD/ZRANGE)\n\
  counter  — integer counter\n\
  json     — JSON document\n\
  none     — key does not exist\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s type user:1"
    )]
    Type { key: String },
    #[command(
        about = "Get the values of multiple keys in one round-trip.",
        long_about = "MGET key [key ...]\n\
\n\
Returns one value per key (nil if the key does not exist or is expired).\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s mget user:1 user:2 user:3"
    )]
    Mget {
        /// One or more key names.
        keys: Vec<String>,
    },
    #[command(
        about = "Set multiple keys to values in one round-trip.",
        long_about = "MSET key value [key value ...]\n\
\n\
Accepts interleaved key-value pairs.  Optional --ttl applies to all keys.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s mset user:1 alice user:2 bob user:3 carol\n\
  mneme-cli -u admin -p s mset session:a tok1 session:b tok2 --ttl 3600"
    )]
    Mset {
        /// Interleaved key value pairs (must be even count).
        pairs: Vec<String>,
        /// TTL in seconds applied to all keys (0 = no expiry).
        #[arg(short = 'x', long, default_value_t = 0)]
        ttl: u64,
    },
    // ── User management (admin-only) ──────────────────────────────────────────
    #[command(
        about = "Create a user account (admin only).",
        long_about = "Create a user account with a given role.\n\
\n\
ROLES:\n\
  admin      — full access to all commands and all databases\n\
  readwrite  — read and write data commands on permitted databases (default)\n\
  readonly   — read-only data commands on permitted databases\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s user-create alice s3cr3t\n\
  mneme-cli -u admin -p s user-create reporter reppass --role readonly"
    )]
    UserCreate {
        username: String,
        password: String,
        /// Role: admin, readwrite, readonly. Default readwrite.
        #[arg(long, default_value = "readwrite")]
        role: String,
    },
    #[command(
        about = "Delete a user account (admin only).",
        long_about = "USER-DELETE username\n\
\n\
Permanently removes the user account. The last remaining admin account\n\
cannot be deleted (returns an error).\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s user-delete alice"
    )]
    UserDelete {
        username: String,
    },
    #[command(
        about = "List all user accounts with their roles and database allowlists (admin only).",
        long_about = "USER-LIST\n\
\n\
Prints a table of every user account:\n\
  username  — login name\n\
  role      — admin / readwrite / readonly\n\
  dbs       — allowed database IDs (empty = all databases)\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s user-list"
    )]
    UserList,
    #[command(
        about = "Grant a user access to a specific database (admin only).",
        long_about = "Grant a user access to a specific database.\n\
\n\
After granting at least one database the user is switched to explicit-allowlist\n\
mode: only the listed databases are accessible. An empty allowlist (before any\n\
grants) means all databases are permitted.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s user-grant alice 2   # allow alice to use db 2"
    )]
    UserGrant {
        username: String,
        db_id: u16,
    },
    #[command(
        about = "Revoke a user's access to a specific database (admin only).",
        long_about = "USER-REVOKE username db_id\n\
\n\
Removes <db_id> from the user's database allowlist. If this leaves the\n\
allowlist empty the user regains access to all databases.\n\
\n\
EXAMPLE:\n\
  mneme-cli -u admin -p s user-revoke alice 2"
    )]
    UserRevoke {
        username: String,
        db_id: u16,
    },
    #[command(
        about = "Show information about a user account.",
        long_about = "USER-INFO [username]\n\
\n\
Displays the role and allowed-database list for the specified user.\n\
Omit <username> to show the calling user's own info.\n\
Non-admin users can only inspect themselves.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s user-info          # your own info\n\
  mneme-cli -u admin -p s user-info alice    # inspect alice (admin only)"
    )]
    UserInfo {
        /// Username to inspect. Omit to show the calling user (non-admins may only see themselves).
        username: Option<String>,
    },
    // ── Profile management (local only — no server connection needed) ─────────
    #[command(
        about = "Create or update a named connection profile in ~/.mneme/profiles.toml.",
        long_about = "Create or update a named profile so you can omit connection flags.\n\
\n\
Profile values are used as defaults and are overridden by explicit CLI flags.\n\
\n\
EXAMPLES:\n\
  # Save a token (get one first with auth-token):\n\
  TOKEN=$(mneme-cli -u admin -p s auth-token)\n\
  mneme-cli profile-set dev --host localhost:6379 --token $TOKEN\n\
\n\
  # Mark as default profile:\n\
  mneme-cli profile-set dev --make-default\n\
\n\
  # Then connect without any flags:\n\
  mneme-cli get mykey"
    )]
    ProfileSet {
        /// Profile name (e.g. \"dev\", \"prod\", \"default\").
        name: String,
        #[arg(long)] host: Option<String>,
        #[arg(long)] ca_cert: Option<String>,
        #[arg(short = 'u', long)] username: Option<String>,
        #[arg(short = 'p', long)] password: Option<String>,
        #[arg(short = 't', long)] token: Option<String>,
        #[arg(short = 'c', long)] consistency: Option<String>,
        #[arg(short = 'd', long)] db: Option<u16>,
        #[arg(long)] insecure: bool,
        /// Make this the default profile (used when --profile is omitted).
        #[arg(long)] make_default: bool,
    },
    #[command(
        about = "List all saved profiles (default profile marked with *).",
        long_about = "PROFILE-LIST\n\
\n\
Prints every profile from ~/.mneme/profiles.toml.\n\
The default profile (used when --profile is omitted) is marked with *.\n\
\n\
EXAMPLE:\n\
  mneme-cli profile-list"
    )]
    ProfileList,
    #[command(
        about = "Delete a saved profile.",
        long_about = "PROFILE-DELETE name\n\
\n\
Removes the named profile from ~/.mneme/profiles.toml.\n\
If it was the default profile, no profile will be loaded automatically\n\
until a new default is set with: mneme-cli profile-set <name> --make-default\n\
\n\
EXAMPLE:\n\
  mneme-cli profile-delete old-dev"
    )]
    ProfileDelete { name: String },
    #[command(
        about = "Show details of a saved profile.",
        long_about = "PROFILE-SHOW [name]\n\
\n\
Prints all fields of the named profile (host, ca_cert, username, etc.).\n\
Omit <name> to show the current default profile.\n\
\n\
EXAMPLES:\n\
  mneme-cli profile-show             # show default profile\n\
  mneme-cli profile-show prod        # show the 'prod' profile"
    )]
    ProfileShow {
        /// Profile name to show. Omit to show the default profile.
        name: Option<String>,
    },

    #[command(
        about = "Change a user's role (admin only).",
        long_about = "Change a user's role. Cannot demote the last admin.\n\
\n\
EXAMPLES:\n\
  mneme-cli -u admin -p s user-role alice readonly\n\
  mneme-cli -u admin -p s user-role alice admin"
    )]
    UserRole {
        username: String,
        /// New role: admin, readwrite, readonly.
        role: String,
    },

    // ── Cluster join ──────────────────────────────────────────────────────────
    #[command(
        about = "Print the join token for adding Keeper/replica nodes (admin only).",
        long_about = "JOIN-TOKEN\n\
\n\
Returns the cluster join token — a compact string encoding:\n\
  • Core's CA certificate (PEM, base64-encoded)\n\
  • cluster_secret (base64-encoded)\n\
\n\
The token is identical to what `install.sh --show-join` generates on the Core\n\
machine, but is accessible over the network once the Core is running.\n\
\n\
HOW TO USE:\n\
  1. Print the token on the Core:\n\
       mneme-cli -u admin -p s join-token\n\
\n\
  2. Copy the printed token, then on each new Keeper or read-replica machine:\n\
       sudo bash install.sh --role keeper \\\n\
         --core-addr CORE_IP:7379 \\\n\
         --join-token \"TOKEN\"\n\
\n\
  3. Or use one-liner to pipe it:\n\
       TOKEN=$(mneme-cli -u admin -p s -H CORE_IP:6379 --ca-cert /etc/mneme/ca.crt join-token)\n\
       ssh keeper-1 \"sudo bash install.sh --role keeper \\\n\
         --core-addr CORE_IP:7379 --join-token '$TOKEN'\"\n\
\n\
SECURITY:\n\
  The token contains the cluster_secret — treat it like a password.\n\
  Requires admin role."
    )]
    JoinToken,
}

// ── main ───────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Must be called before any rustls usage when multiple providers are compiled in.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let cli = Cli::parse();

    // ── Profile management (local, no server connection needed) ────────────────
    if let Some(local_result) = handle_profile_cmd(&cli.cmd)? {
        println!("{local_result}");
        return Ok(());
    }

    // ── Resolve effective config: CLI args + profile fallback ─────────────────
    let profiles = load_profiles().unwrap_or_default();
    let profile_name = cli.profile.as_deref()
        .or_else(|| profiles.default.as_deref())
        .unwrap_or("default");
    let profile = profiles.profiles.get(profile_name);
    let eff = merge_config(&cli, profile);

    let mut conn = connect(&eff.host, &eff.ca_cert, eff.insecure).await
        .context("connect")?;

    // AuthToken: authenticate and print the token, then exit.
    if matches!(cli.cmd, Command::AuthToken) {
        let token = authenticate_for_token(&mut conn, &eff).await.context("auth-token")?;
        println!("{token}");
        return Ok(());
    }

    // Authenticate
    authenticate(&mut conn, &eff).await.context("auth")?;

    // SELECT active database if non-default (or if the command overrides it).
    // This mirrors how Redis clients behave: SELECT once per connection.
    // db_str: the raw string/name; effective_db: the numeric fallback (0 when named).
    let db_str_for_select: Option<String> = match &cli.cmd {
        Command::Select { db } => Some(db.clone()),
        Command::Dbsize { db } => db.clone().or_else(|| Some(eff.db.to_string())),
        Command::Flushdb { db, .. } => db.clone().or_else(|| Some(eff.db.to_string())),
        _ => Some(eff.db.to_string()),
    };
    // Numeric active_db is used for display purposes only.
    let active_db: u16 = db_str_for_select.as_deref()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(eff.db);
    // Send SELECT unless we're staying on db 0 (default) and it's numeric.
    let need_select = db_str_for_select.as_deref()
        .map(|s| s.parse::<u16>().map(|n| n != 0).unwrap_or(true))  // name → always select
        .unwrap_or(false);
    if need_select {
        let raw = db_str_for_select.as_deref().unwrap_or("0");
        let req = if let Ok(n) = raw.parse::<u16>() {
            SelectRequest { db_id: n, name: String::new() }
        } else {
            SelectRequest { db_id: 0, name: raw.to_string() }
        };
        let payload = rmp_serde::to_vec(&req)?;
        let resp = send_raw(&mut conn, CmdId::Select, payload, 0).await
            .context("SELECT")?;
        debug!("SELECT {raw}: {:?}", resp.cmd_id);
    }

    // Parse consistency
    let consistency_flags: u16 = parse_consistency(&eff.consistency);

    // Execute command
    let result = run_command(&mut conn, &cli.cmd, consistency_flags, active_db).await?;
    println!("{result}");

    Ok(())
}

// ── profile command handler (no server connection) ─────────────────────────────

/// Handle profile management commands that operate only on the local
/// `~/.mneme/profiles.toml` file. Returns `Some(output)` if `cmd` is a profile
/// command; `None` for all other commands.
fn handle_profile_cmd(cmd: &Command) -> Result<Option<String>> {
    match cmd {
        Command::ProfileSet {
            name, host, ca_cert, username, password, token,
            consistency, db, insecure, make_default,
        } => {
            let mut pf = load_profiles().unwrap_or_default();
            let entry = pf.profiles.entry(name.clone()).or_default();
            if let Some(v) = host        { entry.host        = Some(v.clone()); }
            if let Some(v) = ca_cert     { entry.ca_cert     = Some(v.clone()); }
            if let Some(v) = username    { entry.username    = Some(v.clone()); }
            if let Some(v) = password    {
                eprintln!("Warning: password stored in plaintext in {}. Consider using auth-token instead.",
                    profiles_path().display());
                entry.password = Some(v.clone());
            }
            if let Some(v) = token       { entry.token       = Some(v.clone()); }
            if let Some(v) = consistency { entry.consistency = Some(v.clone()); }
            if let Some(v) = db          { entry.db          = Some(*v); }
            if *insecure                 { entry.insecure    = Some(true); }
            if *make_default             { pf.default        = Some(name.clone()); }
            save_profiles(&pf)?;
            let default_note = if *make_default { " (set as default)" } else { "" };
            Ok(Some(format!("Profile '{}' saved to {}{default_note}",
                name, profiles_path().display())))
        }

        Command::ProfileList => {
            let pf = load_profiles().unwrap_or_default();
            if pf.profiles.is_empty() {
                return Ok(Some(format!(
                    "(no profiles — create one with: mneme-cli profile-set default --host HOST -u USER -p PASS)"
                )));
            }
            let default_name = pf.default.as_deref().unwrap_or("default");
            let hr = "─".repeat(52);
            let mut out = format!("  {hr}\n");
            out.push_str(&format!("  {:<24} {}\n", "Profile", "Host"));
            out.push_str(&format!("  {hr}\n"));
            let mut names: Vec<&String> = pf.profiles.keys().collect();
            names.sort();
            for n in names {
                let p = &pf.profiles[n];
                let host_str = p.host.as_deref().unwrap_or(DEFAULT_HOST);
                let marker = if n == default_name { "*" } else { " " };
                out.push_str(&format!("  {marker} {:<22} {}\n", n, host_str));
            }
            out.push_str(&format!("  {hr}\n  (* = default profile)"));
            Ok(Some(out))
        }

        Command::ProfileDelete { name } => {
            let mut pf = load_profiles().unwrap_or_default();
            if pf.profiles.remove(name).is_none() {
                bail!("Profile '{}' not found", name);
            }
            if pf.default.as_deref() == Some(name.as_str()) {
                pf.default = None;
            }
            save_profiles(&pf)?;
            Ok(Some(format!("Profile '{}' deleted", name)))
        }

        Command::ProfileShow { name } => {
            let pf = load_profiles().unwrap_or_default();
            let default_name = pf.default.as_deref().unwrap_or("default");
            let target = name.as_deref().unwrap_or(default_name);
            match pf.profiles.get(target) {
                None => bail!("Profile '{}' not found (use 'profile-list' to see available profiles)", target),
                Some(p) => {
                    let is_default = pf.default.as_deref() == Some(target);
                    let mut out = format!("  Profile: {target}");
                    if is_default { out.push_str(" (default)"); }
                    out.push('\n');
                    out.push_str(&format!("  Host      : {}\n", p.host.as_deref().unwrap_or(DEFAULT_HOST)));
                    out.push_str(&format!("  CA cert   : {}\n", p.ca_cert.as_deref().unwrap_or(DEFAULT_CA_CERT)));
                    out.push_str(&format!("  Username  : {}\n", p.username.as_deref().unwrap_or("(none)")));
                    out.push_str(&format!("  Password  : {}\n",
                        if p.password.is_some() { "(stored)" } else { "(none)" }));
                    out.push_str(&format!("  Token     : {}\n",
                        if p.token.is_some() { "(stored)" } else { "(none)" }));
                    out.push_str(&format!("  Consistency: {}\n", p.consistency.as_deref().unwrap_or(DEFAULT_CONSISTENCY)));
                    out.push_str(&format!("  Database  : {}\n", p.db.unwrap_or(DEFAULT_DB)));
                    out.push_str(&format!("  Insecure  : {}", p.insecure.unwrap_or(false)));
                    Ok(Some(out))
                }
            }
        }

        _ => Ok(None),
    }
}

// ── connection ─────────────────────────────────────────────────────────────────

struct Conn {
    stream: tokio_rustls::client::TlsStream<TcpStream>,
}

async fn connect(host: &str, ca_cert_path: &str, insecure: bool) -> Result<Conn> {
    use rustls::ClientConfig;
    use std::sync::Arc;

    let mut config = if insecure {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth()
    } else {
        let ca_data = std::fs::read(ca_cert_path)
            .with_context(|| format!("read CA cert: {ca_cert_path}"))?;
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut ca_data.as_slice()) {
            root_store.add(cert?)?;
        }
        ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };

    // Enable file-backed TLS session resumption so repeat CLI invocations
    // skip the full 1-RTT handshake (~5-10ms savings per command).
    config.resumption = rustls::client::Resumption::store(Arc::new(
        FileSessionCache::new(),
    ));

    let (hostname, port) = parse_host_port(host)?;
    let addr = format!("{hostname}:{port}")
        .to_socket_addrs()?
        .next()
        .context("DNS resolution failed")?;

    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let connector = TlsConnector::from(Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(hostname.as_str())
        .map_err(|_| anyhow::anyhow!("invalid hostname: {hostname}"))?
        .to_owned();
    let tls = connector.connect(server_name, tcp).await?;
    Ok(Conn { stream: tls })
}

fn parse_host_port(addr: &str) -> Result<(String, u16)> {
    if let Some((h, p)) = addr.rsplit_once(':') {
        let port: u16 = p.parse().context("invalid port")?;
        Ok((h.to_string(), port))
    } else {
        Ok((addr.to_string(), 6379))
    }
}

// ── auth ───────────────────────────────────────────────────────────────────────

/// Authenticate and return; used for all commands except auth-token.
async fn authenticate(conn: &mut Conn, eff: &EffectiveConfig) -> Result<()> {
    let payload = if let Some(token) = &eff.token {
        rmp_serde::to_vec(token)?
    } else if let (Some(user), Some(pass)) = (&eff.username, &eff.password) {
        rmp_serde::to_vec(&(user.as_str(), pass.as_str()))?
    } else {
        bail!("Authentication required: provide --token (-t) or --username (-u) + --password (-p), or configure a profile with `mneme-cli profile-set`");
    };

    let frame = Frame {
        cmd_id: CmdId::Auth,
        flags: 0,
        req_id: 0,
        payload: Bytes::from(payload),
    };
    let resp = send_recv(conn, frame).await?;
    if resp.cmd_id != CmdId::Ok {
        let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        bail!("AUTH failed: {msg}");
    }
    debug!("Authenticated");
    Ok(())
}

/// Authenticate with username+password and return the session token string.
/// Used by the `auth-token` subcommand so users can cache the token.
async fn authenticate_for_token(conn: &mut Conn, eff: &EffectiveConfig) -> Result<String> {
    let (user, pass) = match (&eff.username, &eff.password) {
        (Some(u), Some(p)) => (u.as_str(), p.as_str()),
        _ => bail!("auth-token requires --username (-u) and --password (-p)"),
    };
    let payload = rmp_serde::to_vec(&(user, pass))?;
    let frame = Frame {
        cmd_id: CmdId::Auth,
        flags: 0,
        req_id: 0,
        payload: Bytes::from(payload),
    };
    let resp = send_recv(conn, frame).await?;
    if resp.cmd_id != CmdId::Ok {
        let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        bail!("AUTH failed: {msg}");
    }
    // Server returns the token string as the Ok payload for credential-based auth.
    let token: String = rmp_serde::from_slice(&resp.payload)
        .unwrap_or_else(|_| "OK".to_string());
    Ok(token)
}

/// Parse --consistency flag into wire flags bits 3-2.
/// eventual=00, quorum=01 (default), all=10, one=11.
fn parse_consistency(s: &str) -> u16 {
    let bits: u16 = match s.to_lowercase().as_str() {
        "eventual" | "e"  => 0b00,
        "quorum"   | "q"  => 0b01,
        "all"      | "a"  => 0b10,
        "one"      | "o"  => 0b11,
        _                 => 0b01, // default: quorum
    };
    bits << 2
}

// ── command runner ─────────────────────────────────────────────────────────────

async fn run_command(conn: &mut Conn, cmd: &Command, cons_flags: u16, active_db: u16) -> Result<String> {
    use mneme_common::*;

    match cmd {
        Command::Get { key } => {
            let req = GetRequest { key: key.as_bytes().to_vec() };
            let resp = send_cmd(conn, CmdId::Get, &req, 0).await?;
            let val: Value = rmp_serde::from_slice(&resp.payload)?;
            Ok(format_value(&val))
        }

        Command::Set { key, value, ttl } => {
            let req = SetRequest {
                key: key.as_bytes().to_vec(),
                value: Value::String(value.as_bytes().to_vec()),
                ttl_ms: ttl * 1000,
            };
            let resp = send_cmd(conn, CmdId::Set, &req, cons_flags).await?;
            let s: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(s)
        }

        Command::Del { keys } => {
            let req = DelRequest {
                keys: keys.iter().map(|k| k.as_bytes().to_vec()).collect(),
            };
            let resp = send_cmd(conn, CmdId::Del, &req, cons_flags).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Exists { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::Exists, payload, 0).await?;
            let exists: bool = rmp_serde::from_slice(&resp.payload)?;
            Ok(if exists { "(integer) 1" } else { "(integer) 0" }.to_string())
        }

        Command::Expire { key, seconds } => {
            let req = ExpireRequest {
                key: key.as_bytes().to_vec(),
                seconds: *seconds,
            };
            let resp = send_cmd(conn, CmdId::Expire, &req, cons_flags).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Ttl { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::Ttl, payload, 0).await?;
            let ttl: i64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {ttl}"))
        }

        Command::Hget { key, field } => {
            let req = HGetRequest {
                key: key.as_bytes().to_vec(),
                field: field.as_bytes().to_vec(),
            };
            let resp = send_cmd(conn, CmdId::HGet, &req, 0).await?;
            let val: Vec<u8> = rmp_serde::from_slice(&resp.payload)?;
            Ok(format_bytes(&val))
        }

        Command::Hset { key, pairs } => {
            let parsed = parse_pairs(pairs)?;
            let req = HSetRequest {
                key: key.as_bytes().to_vec(),
                pairs: parsed,
            };
            let resp = send_cmd(conn, CmdId::HSet, &req, cons_flags).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Hdel { key, fields } => {
            let req = HDelRequest {
                key: key.as_bytes().to_vec(),
                fields: fields.iter().map(|f| f.as_bytes().to_vec()).collect(),
            };
            let resp = send_cmd(conn, CmdId::HDel, &req, cons_flags).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Hgetall { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::HGetAll, payload, 0).await?;
            let pairs: Vec<(Vec<u8>, Vec<u8>)> = rmp_serde::from_slice(&resp.payload)?;
            let mut out = String::new();
            for (i, (f, v)) in pairs.iter().enumerate() {
                out.push_str(&format!(
                    "{}) {}\n{}) {}",
                    i * 2 + 1,
                    String::from_utf8_lossy(f),
                    i * 2 + 2,
                    String::from_utf8_lossy(v)
                ));
                if i + 1 < pairs.len() { out.push('\n'); }
            }
            Ok(if out.is_empty() { "(empty)".to_string() } else { out })
        }

        Command::Lpush { key, values } => {
            let req = ListPushRequest {
                key: key.as_bytes().to_vec(),
                values: values.iter().map(|v| v.as_bytes().to_vec()).collect(),
            };
            let resp = send_cmd(conn, CmdId::LPush, &req, cons_flags).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Rpush { key, values } => {
            let req = ListPushRequest {
                key: key.as_bytes().to_vec(),
                values: values.iter().map(|v| v.as_bytes().to_vec()).collect(),
            };
            let resp = send_cmd(conn, CmdId::RPush, &req, cons_flags).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Lpop { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::LPop, payload, cons_flags).await?;
            let val: Vec<u8> = rmp_serde::from_slice(&resp.payload)?;
            Ok(format_bytes(&val))
        }

        Command::Rpop { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::RPop, payload, cons_flags).await?;
            let val: Vec<u8> = rmp_serde::from_slice(&resp.payload)?;
            Ok(format_bytes(&val))
        }

        Command::Lrange { key, start, stop } => {
            let req = LRangeRequest {
                key: key.as_bytes().to_vec(),
                start: *start,
                stop: *stop,
            };
            let resp = send_cmd(conn, CmdId::LRange, &req, 0).await?;
            let items: Vec<Vec<u8>> = rmp_serde::from_slice(&resp.payload)?;
            Ok(format_list(&items))
        }

        Command::Zadd { key, pairs } => {
            let members = parse_zscore_pairs(pairs)?;
            let req = ZAddRequest {
                key: key.as_bytes().to_vec(),
                members,
            };
            let resp = send_cmd(conn, CmdId::ZAdd, &req, cons_flags).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Zscore { key, member } => {
            let payload = rmp_serde::to_vec(&(key.as_bytes().to_vec(), member.as_bytes().to_vec()))?;
            let resp = send_raw(conn, CmdId::ZScore, payload, 0).await?;
            let score: f64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("{score}"))
        }

        Command::Zrank { key, member } => {
            let req = ZRankRequest {
                key: key.as_bytes().to_vec(),
                member: member.as_bytes().to_vec(),
            };
            let resp = send_cmd(conn, CmdId::ZRank, &req, 0).await?;
            let rank: i64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {rank}"))
        }

        Command::Zrange { key, start, stop, withscores } => {
            let req = ZRangeRequest {
                key: key.as_bytes().to_vec(),
                start: *start,
                stop: *stop,
                with_scores: *withscores,
            };
            let resp = send_cmd(conn, CmdId::ZRange, &req, 0).await?;
            let items: Vec<Vec<u8>> = rmp_serde::from_slice(&resp.payload)?;
            Ok(format_list(&items))
        }

        Command::Zrangebyscore { key, min, max } => {
            let req = ZRangeByScoreRequest {
                key: key.as_bytes().to_vec(),
                min: *min,
                max: *max,
            };
            let resp = send_cmd(conn, CmdId::ZRangeByScore, &req, 0).await?;
            let items: Vec<Vec<u8>> = rmp_serde::from_slice(&resp.payload)?;
            Ok(format_list(&items))
        }

        Command::Zrem { key, members } => {
            let req = ZRemRequest {
                key: key.as_bytes().to_vec(),
                members: members.iter().map(|m| m.as_bytes().to_vec()).collect(),
            };
            let resp = send_cmd(conn, CmdId::ZRem, &req, cons_flags).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Zcard { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::ZCard, payload, 0).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        // ── Counter commands ──────────────────────────────────────────────────
        Command::Incr { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::Incr, payload, cons_flags).await?;
            let n: i64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Decr { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::Decr, payload, cons_flags).await?;
            let n: i64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Incrby { key, delta } => {
            let req = IncrByRequest {
                key: key.as_bytes().to_vec(),
                delta: *delta,
            };
            let resp = send_cmd(conn, CmdId::IncrBy, &req, cons_flags).await?;
            let n: i64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Decrby { key, delta } => {
            let req = IncrByRequest {
                key: key.as_bytes().to_vec(),
                delta: *delta,   // server's DecrBy handler negates: incrby(-delta)
            };
            let resp = send_cmd(conn, CmdId::DecrBy, &req, cons_flags).await?;
            let n: i64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Incrbyfloat { key, delta } => {
            let req = IncrByFloatRequest {
                key: key.as_bytes().to_vec(),
                delta: *delta,
            };
            let resp = send_cmd(conn, CmdId::IncrByFloat, &req, cons_flags).await?;
            let v: f64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("{v}"))
        }

        Command::Getset { key, value } => {
            let req = GetSetRequest {
                key: key.as_bytes().to_vec(),
                value: mneme_common::Value::String(value.as_bytes().to_vec()),
            };
            let resp = send_cmd(conn, CmdId::GetSet, &req, cons_flags).await?;
            let val: Option<mneme_common::Value> = rmp_serde::from_slice(&resp.payload)?;
            Ok(match val {
                Some(v) => format_value(&v),
                None => "(nil)".to_string(),
            })
        }

        // ── JSON commands ─────────────────────────────────────────────────────
        Command::JsonGet { key, path } => {
            let req = mneme_common::JsonGetRequest {
                key: key.as_bytes().to_vec(),
                path: path.clone(),
            };
            let resp = send_cmd(conn, CmdId::JsonGet, &req, 0).await?;
            let s: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(s)
        }

        Command::JsonSet { key, path, value } => {
            let req = mneme_common::JsonSetRequest {
                key:   key.as_bytes().to_vec(),
                path:  path.clone(),
                value: value.clone(),
            };
            let resp = send_cmd(conn, CmdId::JsonSet, &req, cons_flags).await?;
            let s: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(s)
        }

        Command::JsonDel { key, path } => {
            let req = mneme_common::JsonDelRequest {
                key:  key.as_bytes().to_vec(),
                path: path.clone(),
            };
            let resp = send_cmd(conn, CmdId::JsonDel, &req, cons_flags).await?;
            let s: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(s)
        }

        Command::JsonExists { key, path } => {
            let req = mneme_common::JsonGetRequest {
                key:  key.as_bytes().to_vec(),
                path: path.clone(),
            };
            let resp = send_cmd(conn, CmdId::JsonExists, &req, 0).await?;
            let b: bool = rmp_serde::from_slice(&resp.payload)?;
            Ok(if b { "true".into() } else { "false".into() })
        }

        Command::JsonType { key, path } => {
            let req = mneme_common::JsonGetRequest {
                key:  key.as_bytes().to_vec(),
                path: path.clone(),
            };
            let resp = send_cmd(conn, CmdId::JsonType, &req, 0).await?;
            let s: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(s)
        }

        Command::JsonArrappend { key, path, value } => {
            let req = mneme_common::JsonArrAppendRequest {
                key:   key.as_bytes().to_vec(),
                path:  path.clone(),
                value: value.clone(),
            };
            let resp = send_cmd(conn, CmdId::JsonArrAppend, &req, cons_flags).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::JsonNumincrby { key, path, delta } => {
            let req = mneme_common::JsonNumIncrByRequest {
                key:   key.as_bytes().to_vec(),
                path:  path.clone(),
                delta: *delta,
            };
            let resp = send_cmd(conn, CmdId::JsonNumIncrBy, &req, cons_flags).await?;
            let s: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(s)
        }

        Command::RevokeToken { token } => {
            let payload = rmp_serde::to_vec(token)?;
            let resp = send_raw(conn, CmdId::RevokeToken, payload, 0).await?;
            let s: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(s)
        }

        Command::AuthToken => {
            // Handled before authenticate() in main() — should not reach here.
            bail!("auth-token must be called before authentication")
        }

        Command::Slowlog { count } => {
            let payload = rmp_serde::to_vec(count)?;
            let resp = send_raw(conn, CmdId::SlowLog, payload, 0).await?;
            let entries: Vec<(String, Vec<u8>, u64)> =
                rmp_serde::from_slice(&resp.payload).unwrap_or_default();
            if entries.is_empty() {
                return Ok("(empty list)".to_string());
            }
            let out: Vec<String> = entries.iter().enumerate().map(|(i, (cmd, key, us))| {
                format!("{}) cmd={cmd} key={} latency_us={us}",
                    i + 1, String::from_utf8_lossy(key))
            }).collect();
            Ok(out.join("\n"))
        }

        Command::Metrics => {
            let resp = send_raw(conn, CmdId::Metrics, rmp_serde::to_vec(&())?, 0).await?;
            let (used, total): (u64, u64) = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!(
                "  Pool Used  : {}\n  Pool Total : {}",
                fmt_bytes(used), fmt_bytes(total)
            ))
        }

        Command::Stats => {
            let resp = send_raw(conn, CmdId::Stats, rmp_serde::to_vec(&())?, 0).await?;
            let s: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(s)
        }

        Command::MemoryUsage { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::MemoryUsage, payload, 0).await?;
            let bytes: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {bytes}"))
        }

        Command::Config { param } => {
            let payload = rmp_serde::to_vec(param)?;
            let resp = send_raw(conn, CmdId::Config, payload, 0).await?;
            let val: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(val)
        }

        Command::ConfigSet { param, value } => {
            use mneme_common::ConfigSetRequest;
            let req = ConfigSetRequest { param: param.clone(), value: value.clone() };
            let resp = send_cmd(conn, CmdId::Config, &req, 0).await?;
            let msg: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(msg)
        }

        Command::ClusterInfo => {
            let resp = send_raw(conn, CmdId::ClusterInfo, rmp_serde::to_vec(&())?, 0).await?;
            let pairs: Vec<(String, String)> = rmp_serde::from_slice(&resp.payload)?;
            Ok(fmt_cluster_info(&pairs))
        }

        Command::ClusterSlots => {
            let resp = send_raw(conn, CmdId::ClusterSlots, rmp_serde::to_vec(&())?, 0).await?;
            let table: std::collections::HashMap<u16, u64> = rmp_serde::from_slice(&resp.payload)?;
            if table.is_empty() {
                return Ok("  (no slots assigned)".to_string());
            }
            let mut entries: Vec<(u16, u64)> = table.into_iter().collect();
            entries.sort_by_key(|(s, _)| *s);
            let out: Vec<String> = entries.iter()
                .map(|(slot, node)| format!("  slot {slot:5}  →  node {node}"))
                .collect();
            Ok(out.join("\n"))
        }

        Command::KeeperList => {
            let resp = send_raw(conn, CmdId::KeeperList, rmp_serde::to_vec(&())?, 0).await?;
            let list: Vec<(u64, String, String, u64, u64)> = rmp_serde::from_slice(&resp.payload)?;
            Ok(fmt_keeper_list(&list))
        }

        Command::PoolStats => {
            let resp = send_raw(conn, CmdId::PoolStats, rmp_serde::to_vec(&())?, 0).await?;
            let (used, total, keepers): (u64, u64, usize) = rmp_serde::from_slice(&resp.payload)?;
            let pct = if total > 0 { used * 100 / total } else { 0 };
            Ok(format!(
                "  Pool Used    : {} / {} ({}%)\n  Keeper Count : {}",
                fmt_bytes(used), fmt_bytes(total), pct, keepers
            ))
        }

        Command::Wait { n_keepers, timeout_ms } => {
            let req = WaitRequest {
                n_keepers: *n_keepers,
                timeout_ms: *timeout_ms,
            };
            let resp = send_cmd(conn, CmdId::Wait, &req, 0).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}"))
        }

        Command::Select { db } => {
            // SELECT was already sent in main() before run_command.
            // Important: mneme-cli creates a NEW TCP connection per invocation, so
            // this SELECT only applied to the current command.  The next invocation
            // starts at database 0 again.  Use the -d/--db flag on every command
            // to target a specific database in a stateless CLI session.
            Ok(format!(
                "OK  (active database: '{db}')\n\
                 \n\
                 NOTE: This selection is connection-scoped and does NOT persist.\n\
                 Use the -d flag on every subsequent command to stay in this database:\n\
                 \n\
                 \x20 mneme-cli [flags] -d {db} set mykey myvalue\n\
                 \x20 mneme-cli [flags] -d {db} get mykey"
            ))
        }

        Command::Dbsize { db } => {
            let active_str = active_db.to_string();
            let label = db.as_deref().unwrap_or(active_str.as_str());
            let req = if let Some(s) = db.as_deref() {
                if let Ok(n) = s.parse::<u16>() {
                    DbSizeRequest { db_id: Some(n), name: String::new() }
                } else {
                    DbSizeRequest { db_id: None, name: s.to_string() }
                }
            } else {
                DbSizeRequest { db_id: None, name: String::new() }
            };
            let resp = send_cmd(conn, CmdId::DbSize, &req, 0).await?;
            let n: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("(integer) {n}  (db {label})"))
        }

        Command::Flushdb { db, no_sync } => {
            let active_str = active_db.to_string();
            let label = db.as_deref().unwrap_or(active_str.as_str());
            let req = if let Some(s) = db.as_deref() {
                if let Ok(n) = s.parse::<u16>() {
                    FlushDbRequest { db_id: Some(n), name: String::new(), sync: !no_sync }
                } else {
                    FlushDbRequest { db_id: None, name: s.to_string(), sync: !no_sync }
                }
            } else {
                FlushDbRequest { db_id: None, name: String::new(), sync: !no_sync }
            };
            let resp = send_cmd(conn, CmdId::FlushDb, &req, cons_flags).await?;
            let flushed: u64 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("OK  ({flushed} keys flushed from db {label})"))
        }

        Command::DbCreate { name, id } => {
            let req = DbCreateRequest { name: name.clone(), db_id: *id };
            let resp = send_cmd(conn, CmdId::DbCreate, &req, 0).await?;
            let info: DbInfo = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("OK  (database '{}' created with id {})", info.name, info.id))
        }

        Command::DbList => {
            let resp = send_raw(conn, CmdId::DbList, rmp_serde::to_vec(&())?, 0).await?;
            let list: Vec<DbInfo> = rmp_serde::from_slice(&resp.payload)?;
            if list.is_empty() {
                return Ok("  (no named databases — create one with: mneme-cli db-create NAME)".into());
            }
            let hr = "─".repeat(40);
            let mut out = format!("  {hr}\n");
            out.push_str(&format!("  {:<6}  {}\n", "ID", "Name"));
            out.push_str(&format!("  {hr}\n"));
            for db in &list {
                out.push_str(&format!("  {:<6}  {}\n", db.id, db.name));
            }
            out.push_str(&format!("  {hr}"));
            Ok(out)
        }

        Command::DbDrop { name } => {
            let req = DbDropRequest { name: name.clone() };
            let resp = send_cmd(conn, CmdId::DbDrop, &req, 0).await?;
            let id: u16 = rmp_serde::from_slice(&resp.payload)?;
            Ok(format!("OK  (database '{}' (id {}) deregistered — data still accessible by id)", name, id))
        }

        // ── Bulk / scan commands ──────────────────────────────────────────────

        Command::Scan { pattern, count, cursor } => {
            let pat = if pattern == "*" { None } else { Some(pattern.clone()) };
            let req = ScanRequest { cursor: *cursor, pattern: pat, count: *count };
            let resp = send_cmd(conn, CmdId::Scan, &req, 0).await?;
            let (next_cursor, keys): (u64, Vec<Vec<u8>>) = rmp_serde::from_slice(&resp.payload)?;
            if keys.is_empty() {
                return Ok(format!("(empty)  next_cursor={next_cursor}"));
            }
            let mut out = String::new();
            for (i, k) in keys.iter().enumerate() {
                out.push_str(&format!("{}) \"{}\"\n", i + 1, String::from_utf8_lossy(k)));
            }
            out.push_str(&format!("next_cursor={next_cursor}"));
            Ok(out)
        }

        Command::Type { key } => {
            let payload = rmp_serde::to_vec(&key.as_bytes().to_vec())?;
            let resp = send_raw(conn, CmdId::Type, payload, 0).await?;
            let type_str: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(type_str)
        }

        Command::Mget { keys } => {
            let req = MGetRequest {
                keys: keys.iter().map(|k| k.as_bytes().to_vec()).collect(),
            };
            let resp = send_cmd(conn, CmdId::MGet, &req, 0).await?;
            let values: Vec<Option<Value>> = rmp_serde::from_slice(&resp.payload)?;
            let out: Vec<String> = values.iter().enumerate().map(|(i, v)| {
                match v {
                    None    => format!("{}) (nil)", i + 1),
                    Some(v) => format!("{}) {}", i + 1, format_value(v)),
                }
            }).collect();
            Ok(out.join("\n"))
        }

        Command::Mset { pairs, ttl } => {
            if pairs.len() % 2 != 0 {
                bail!("MSET expects interleaved key value pairs (even number of args)");
            }
            let mset_pairs: Vec<(Vec<u8>, Value, u64)> = pairs.chunks(2).map(|c| {
                (c[0].as_bytes().to_vec(), Value::String(c[1].as_bytes().to_vec()), ttl * 1000)
            }).collect();
            let req = MSetRequest { pairs: mset_pairs };
            send_cmd(conn, CmdId::MSet, &req, cons_flags).await?;
            Ok("OK".to_string())
        }

        // ── User management ───────────────────────────────────────────────────

        Command::UserCreate { username, password, role } => {
            let req = UserCreateRequest {
                username: username.clone(),
                password: password.clone(),
                role: role.clone(),
            };
            let resp = send_cmd(conn, CmdId::UserCreate, &req, 0).await?;
            let msg: String = rmp_serde::from_slice(&resp.payload)?;
            Ok(msg)
        }

        Command::UserDelete { username } => {
            let req = UserDeleteRequest { username: username.clone() };
            send_cmd(conn, CmdId::UserDelete, &req, 0).await?;
            Ok(format!("OK  (user '{}' deleted)", username))
        }

        Command::UserList => {
            let resp = send_raw(conn, CmdId::UserList, vec![], 0).await?;
            let list: Vec<(String, String, Vec<u16>)> = rmp_serde::from_slice(&resp.payload)?;
            if list.is_empty() {
                return Ok("(no users)".into());
            }
            let hr = "─".repeat(56);
            let mut out = format!("  {hr}\n");
            out.push_str(&format!("  {:<20} {:<12} {}\n", "Username", "Role", "Allowed DBs"));
            out.push_str(&format!("  {hr}\n"));
            for (name, role, dbs) in &list {
                let dbs_str = if dbs.is_empty() {
                    "all".to_string()
                } else {
                    dbs.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(",")
                };
                out.push_str(&format!("  {:<20} {:<12} {}\n", name, role, dbs_str));
            }
            out.push_str(&format!("  {hr}"));
            Ok(out)
        }

        Command::UserGrant { username, db_id } => {
            let req = UserGrantRequest { username: username.clone(), db_id: *db_id };
            send_cmd(conn, CmdId::UserGrant, &req, 0).await?;
            Ok(format!("OK  (granted db {} to '{}')", db_id, username))
        }

        Command::UserRevoke { username, db_id } => {
            let req = UserRevokeRequest { username: username.clone(), db_id: *db_id };
            send_cmd(conn, CmdId::UserRevoke, &req, 0).await?;
            Ok(format!("OK  (revoked db {} from '{}')", db_id, username))
        }

        Command::UserInfo { username } => {
            let req = UserInfoRequest { username: username.clone() };
            let resp = send_cmd(conn, CmdId::UserInfo, &req, 0).await?;
            let (name, uid, role, dbs): (String, u64, String, Vec<u16>) =
                rmp_serde::from_slice(&resp.payload)?;
            let dbs_str = if dbs.is_empty() {
                "all databases".to_string()
            } else {
                dbs.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(", ")
            };
            Ok(format!(
                "  Username  : {name}\n  User ID   : {uid}\n  Role      : {role}\n  DBs       : {dbs_str}"
            ))
        }

        Command::UserRole { username, role } => {
            let req = UserSetRoleRequest { username: username.clone(), role: role.clone() };
            send_cmd(conn, CmdId::UserSetRole, &req, 0).await?;
            Ok(format!("OK  (role of '{}' set to '{}')", username, role))
        }

        Command::JoinToken => {
            let resp = send_raw(conn, CmdId::GenerateJoinToken, rmp_serde::to_vec(&())?, 0).await?;
            let token: String = rmp_serde::from_slice(&resp.payload)?;
            // Print a ready-to-paste install command followed by the raw token on its own line
            // so the token can easily be captured with TOKEN=$(mneme-cli ... join-token).
            let out = format!(
                "  Join token (paste into install.sh --join-token on each Keeper / replica):\n\n\
                 {token}\n\n\
                 Quick-add a Keeper:\n\
                   sudo bash install.sh --role keeper --core-addr <CORE_IP>:7379 --join-token \"{token}\"",
            );
            Ok(out)
        }

        // Profile commands are handled before connect() in main(); these
        // branches are unreachable at runtime but required for match exhaustion.
        Command::ProfileSet { .. } | Command::ProfileList
        | Command::ProfileDelete { .. } | Command::ProfileShow { .. } => {
            Ok(String::new()) // unreachable
        }
    }
}

// ── transport helpers ──────────────────────────────────────────────────────────

async fn send_cmd<T: serde::Serialize>(
    conn: &mut Conn,
    cmd: CmdId,
    req: &T,
    flags: u16,
) -> Result<Frame> {
    let payload = rmp_serde::to_vec(req)?;
    send_raw(conn, cmd, payload, flags).await
}

async fn send_raw(conn: &mut Conn, cmd: CmdId, payload: Vec<u8>, flags: u16) -> Result<Frame> {
    let frame = Frame { cmd_id: cmd, flags, req_id: 0, payload: Bytes::from(payload) };
    send_recv(conn, frame).await
}

async fn send_recv(conn: &mut Conn, frame: Frame) -> Result<Frame> {
    conn.stream.write_all(&frame.encode()).await?;

    let mut buf = BytesMut::with_capacity(4096);
    loop {
        if buf.len() >= FRAME_HEADER {
            // payload_len is at header bytes [8..12]; req_id at [12..16]
            let plen = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
            if buf.len() >= FRAME_HEADER + plen {
                break;
            }
        }
        let n = conn.stream.read_buf(&mut buf).await?;
        if n == 0 { bail!("connection closed"); }
    }

    let (resp, _) = Frame::decode(&buf).map_err(|e| anyhow::anyhow!("{e}"))?;
    if resp.cmd_id == CmdId::LeaderRedirect {
        let payload: mneme_common::LeaderRedirectPayload =
            rmp_serde::from_slice(&resp.payload).unwrap_or(mneme_common::LeaderRedirectPayload {
                leader_addr: String::new(),
            });
        if payload.leader_addr.is_empty() {
            bail!("NOT_LEADER: this node is not the Raft leader and the leader is unknown (election in progress)");
        }
        bail!("NOT_LEADER: redirect to leader at {}", payload.leader_addr);
    }
    if resp.cmd_id == CmdId::Error {
        let msg: String = rmp_serde::from_slice(&resp.payload).unwrap_or_default();
        bail!("{msg}");
    }
    Ok(resp)
}

// ── formatters ─────────────────────────────────────────────────────────────────

fn format_value(v: &Value) -> String {
    match v {
        Value::String(b) => format!("\"{}\"", String::from_utf8_lossy(b)),
        Value::Hash(pairs) => {
            let s: Vec<String> = pairs.iter()
                .map(|(f, v)| format!("{}={}", String::from_utf8_lossy(f), String::from_utf8_lossy(v)))
                .collect();
            format!("{{{}}}", s.join(", "))
        }
        Value::List(items) => format_list(&items.iter().cloned().collect::<Vec<_>>()),
        Value::ZSet(members) => {
            let s: Vec<String> = members.iter()
                .map(|m| format!("{}:{}", m.score, String::from_utf8_lossy(&m.member)))
                .collect();
            format!("[{}]", s.join(", "))
        }
        Value::Counter(n) => n.to_string(),
        Value::Json(doc) => doc.raw.clone(),
    }
}

fn format_bytes(b: &[u8]) -> String {
    format!("\"{}\"", String::from_utf8_lossy(b))
}

fn format_list(items: &[Vec<u8>]) -> String {
    if items.is_empty() {
        return "(empty)".to_string();
    }
    items.iter().enumerate()
        .map(|(i, v)| format!("{}) \"{}\"", i + 1, String::from_utf8_lossy(v)))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── input parsers ──────────────────────────────────────────────────────────────

fn parse_pairs(args: &[String]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    if args.len() % 2 != 0 {
        bail!("expected alternating field value pairs (even number of args), got {}", args.len());
    }
    let mut out = Vec::new();
    for chunk in args.chunks(2) {
        out.push((chunk[0].as_bytes().to_vec(), chunk[1].as_bytes().to_vec()));
    }
    Ok(out)
}

fn parse_zscore_pairs(args: &[String]) -> Result<Vec<mneme_common::ZSetMember>> {
    if args.len() % 2 != 0 {
        bail!("ZADD expects score member pairs (even number of args)");
    }
    let mut out = Vec::new();
    for chunk in args.chunks(2) {
        let score: f64 = chunk[0].parse().context("score must be a float")?;
        let member = chunk[1].as_bytes().to_vec();
        out.push(mneme_common::ZSetMember { score, member });
    }
    Ok(out)
}

// ── pretty-print helpers ───────────────────────────────────────────────────────

/// Format a byte count as human-readable: "1.23 GB", "456.7 MB", "789 KB", "123 B".
fn fmt_bytes(n: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if n >= GB {
        format!("{:.2} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.0} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

/// Map a ClusterInfo field key to a human-readable label.
fn cluster_label(k: &str) -> String {
    match k {
        "state"             => "Cluster State",
        "role"              => "Node Role",
        "node_id"           => "Node ID",
        "keeper_count"      => "Keeper Count",
        "pool_used_bytes"   => "Pool Used",
        "pool_max_bytes"    => "Pool Capacity",
        "memory_pressure"   => "Memory Pressure",
        "total_keys"        => "Total Keys",
        "client_port"       => "Client Port",
        "repl_port"         => "Replication Port",
        "warmup_state"      => "Warmup State",
        "supported_modes"   => "Supported Consistency",
        "raft_term"         => "Raft Term",
        "is_leader"         => "Is Leader",
        "leader_id"         => "Raft Leader ID",
        other               => other,
    }.to_string()
}

/// Format cluster-info key-value pairs as an aligned table.
fn fmt_cluster_info(pairs: &[(String, String)]) -> String {
    let col_w = pairs.iter().map(|(k, _)| cluster_label(k).len()).max().unwrap_or(0) + 2;
    let hr = "─".repeat(col_w + 20);
    let mut out = format!("  {hr}\n");
    for (k, v) in pairs {
        let lbl = cluster_label(k);
        let val = match k.as_str() {
            "pool_used_bytes" | "pool_max_bytes" => {
                let n: u64 = v.parse().unwrap_or(0);
                fmt_bytes(n)
            }
            _ => v.clone(),
        };
        out.push_str(&format!("  {:<col_w$}: {val}\n", lbl));
    }
    out.push_str(&format!("  {hr}"));
    out
}

/// Format keeper list as an aligned table.
fn fmt_keeper_list(list: &[(u64, String, String, u64, u64)]) -> String {
    if list.is_empty() {
        return concat!(
            "  (no keepers connected)\n",
            "  Tip: check that keeper nodes have core_addr set in their config.\n",
            "  Run: sudo /bin/bash install.sh --fix-core-addr CORE_IP:7379"
        ).to_string();
    }
    let hr = "─".repeat(70);
    let mut out = format!("  {hr}\n");
    out.push_str(&format!("  {:<20} {:<26} {:>10} {:>10}\n",
        "Node Name", "Address", "WAL Bytes", "Disk Est."));
    out.push_str(&format!("  {hr}\n"));
    for (_node_id, node_name, addr, pool, used) in list {
        out.push_str(&format!("  {:<20} {:<26} {:>10} {:>10}\n",
            node_name, addr, fmt_bytes(*pool), fmt_bytes(*used)));
    }
    out.push_str(&format!("  {hr}"));
    out
}

// ── TLS verifier (insecure dev mode) ──────────────────────────────────────────

// ── File-backed TLS key-exchange hint cache ──────────────────────────────────
//
// Persists the server's preferred NamedGroup (key exchange hint) to
// ~/.mneme/tls_sessions/ so repeat CLI invocations skip the key-exchange
// negotiation probe. Session tickets (Tls12/Tls13) use the in-memory
// delegate — rustls doesn't expose serialization for those types, so full
// session resumption across process lifetimes requires a persistent daemon.
//
// The kx_hint alone saves ~1 RTT by avoiding the HelloRetryRequest path
// when the client already knows the server's preferred group.

use rustls::client::{ClientSessionMemoryCache, ClientSessionStore};

const KX_HINT_DIR: &str = ".mneme/tls_sessions";
const KX_HINT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(86400);

#[derive(Debug)]
struct FileSessionCache {
    /// In-memory delegate for session ticket storage (within process lifetime).
    mem: ClientSessionMemoryCache,
    /// Directory for persisting kx_hint across process invocations.
    dir: PathBuf,
}

impl FileSessionCache {
    fn new() -> Self {
        let dir = dirs_or_home().join(KX_HINT_DIR);
        let _ = std::fs::create_dir_all(&dir);
        Self {
            mem: ClientSessionMemoryCache::new(32),
            dir,
        }
    }

    fn kx_path(&self, server_name: &rustls::pki_types::ServerName<'_>) -> PathBuf {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        format!("{server_name:?}").hash(&mut h);
        self.dir.join(format!("{:016x}.kx", h.finish()))
    }

    fn is_fresh(path: &std::path::Path) -> bool {
        path.metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or(KX_HINT_MAX_AGE) < KX_HINT_MAX_AGE)
            .unwrap_or(false)
    }
}

impl ClientSessionStore for FileSessionCache {
    fn set_kx_hint(
        &self,
        server_name: rustls::pki_types::ServerName<'static>,
        group: rustls::NamedGroup,
    ) {
        // Persist to disk so next CLI invocation knows the server's preferred group.
        let path = self.kx_path(&server_name);
        let val: u16 = group.into();
        let _ = std::fs::write(path, val.to_le_bytes());
        self.mem.set_kx_hint(server_name, group);
    }

    fn kx_hint(
        &self,
        server_name: &rustls::pki_types::ServerName<'_>,
    ) -> Option<rustls::NamedGroup> {
        // Try in-memory first, then fall back to disk.
        if let Some(g) = self.mem.kx_hint(server_name) {
            return Some(g);
        }
        let path = self.kx_path(server_name);
        if !Self::is_fresh(&path) {
            return None;
        }
        let data = std::fs::read(&path).ok()?;
        if data.len() != 2 {
            return None;
        }
        let val = u16::from_le_bytes([data[0], data[1]]);
        Some(rustls::NamedGroup::from(val))
    }

    fn set_tls12_session(
        &self,
        server_name: rustls::pki_types::ServerName<'static>,
        value: rustls::client::Tls12ClientSessionValue,
    ) {
        self.mem.set_tls12_session(server_name, value);
    }

    fn tls12_session(
        &self,
        server_name: &rustls::pki_types::ServerName<'_>,
    ) -> Option<rustls::client::Tls12ClientSessionValue> {
        self.mem.tls12_session(server_name)
    }

    fn remove_tls12_session(&self, server_name: &rustls::pki_types::ServerName<'static>) {
        self.mem.remove_tls12_session(server_name);
    }

    fn insert_tls13_ticket(
        &self,
        server_name: rustls::pki_types::ServerName<'static>,
        value: rustls::client::Tls13ClientSessionValue,
    ) {
        self.mem.insert_tls13_ticket(server_name, value);
    }

    fn take_tls13_ticket(
        &self,
        server_name: &rustls::pki_types::ServerName<'static>,
    ) -> Option<rustls::client::Tls13ClientSessionValue> {
        self.mem.take_tls13_ticket(server_name)
    }
}

/// Return the user's home directory for config/session storage.
fn dirs_or_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dh_params: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dh_params: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
