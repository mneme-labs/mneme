# MnemeCache — Command Reference

Complete command reference for the MnemeCache wire protocol. For the
framing format, consistency levels, auth flow, and multiplexing specification
see [CLIENT_PROTOCOL.md](CLIENT_PROTOCOL.md). For the Rust client library
usage guide see [../mneme-client/README.md](../mneme-client/README.md).

---

## Wire Header (16 bytes)

```
[4B magic 0x4D4E454D][1B ver=0x01][1B cmd_id][2B flags][4B payload_len][4B req_id][msgpack payload]
```

Flags bits 15–4 = slot hint (CRC16(key) % 16384)
Flags bits 3–2  = consistency (00=EVENTUAL 01=QUORUM 10=ALL 11=ONE)
req_id = 0 → single-plex; ≥1 → multiplexed (responses may arrive out-of-order)

---

## String / KV Commands

### GET — 0x01

Fetch the value stored at `key`.

**Request**
```rust
struct GetRequest { key: bytes }
```

**Response** — `Value` payload on success, `Error(KeyNotFound)` if absent or expired.

**Notes:** Works with all value types. Returns the raw `Value` enum variant.

---

### SET — 0x02

Store a value with an optional TTL.

**Request**
```rust
struct SetRequest {
    key:    bytes,
    value:  Value,
    ttl_ms: u64,   // 0 = no expiry; milliseconds
}
```

**Response** — `"OK"` string.

**Notes:** Overwrites any existing value and TTL. The TTL is replicated to Keepers
as an absolute `expiry_at` timestamp, so keys expired during a Keeper restart are
deleted immediately on reconnect rather than serving stale data.

---

### DEL — 0x03

Delete one or more keys.

**Request**
```rust
struct DelRequest { keys: Vec<bytes> }   // max 1 000 keys
```

**Response** — `u64` count of keys that existed.

---

### EXISTS — 0x04

Check whether a key exists (and has not expired).

**Request** — `bytes` (raw key)

**Response** — `bool`

---

### EXPIRE — 0x05

Set a TTL on an existing key.

**Request**
```rust
struct ExpireRequest { key: bytes, seconds: u64 }
```

**Response** — `u64` (1 = TTL applied, 0 = key not found)

---

### TTL — 0x06

Return the remaining TTL of a key.

**Request** — `bytes` (raw key)

**Response** — `i64` milliseconds remaining.
- `−1` = key exists but has no TTL (permanent)
- `−2` = key does not exist

---

## Hash Commands

### HSET — 0x10

Set one or more fields on a hash key.

**Request**
```rust
struct HSetRequest {
    key:   bytes,
    pairs: Vec<(field: bytes, value: bytes)>,  // max 65 536 pairs
}
```

**Response** — `"OK"` string.

---

### HGET — 0x11

Get one field from a hash key.

**Request**
```rust
struct HGetRequest { key: bytes, field: bytes }
```

**Response** — `bytes` (raw field value) or `Error(KeyNotFound)`.

---

### HDEL — 0x12

Delete one or more fields from a hash key.

**Request**
```rust
struct HDelRequest { key: bytes, fields: Vec<bytes> }
```

**Response** — `u64` count of fields removed.

---

### HGETALL — 0x13

Return all fields and values in a hash key.

**Request** — `bytes` (raw key)

**Response** — `Vec<(field: bytes, value: bytes)>`. Returns empty vec if key absent.

---

## List Commands

### LPUSH — 0x20

Prepend values to the head of a list.

**Request**
```rust
struct ListPushRequest { key: bytes, values: Vec<bytes> }
```

**Response** — `u64` new list length.

---

### RPUSH — 0x21

Append values to the tail of a list.

**Request** — same as `LPUSH`

**Response** — `u64` new list length.

---

### LPOP — 0x22

Remove and return the first element of a list.

**Request** — `bytes` (raw key)

**Response** — `Value` or `Error(KeyNotFound)`.

---

### RPOP — 0x23

Remove and return the last element.

**Request** — `bytes` (raw key)

**Response** — `Value` or `Error(KeyNotFound)`.

---

### LRANGE — 0x24

Return a slice of the list.

**Request**
```rust
struct LRangeRequest { key: bytes, start: i64, stop: i64 }
```

**Response** — `Vec<Value>`. Negative indexes count from the tail (−1 = last element).

---

## Sorted Set Commands

`ZSetMember` = `{ member: bytes, score: f64 }`

### ZADD — 0x30

Add or update members.

**Request**
```rust
struct ZAddRequest { key: bytes, members: Vec<ZSetMember> }
```

**Response** — `u64` count of newly added members (updates not counted).

---

### ZRANK — 0x31

Return the 0-based rank of a member (ascending order).

**Request**
```rust
struct ZRankRequest { key: bytes, member: bytes }
```

**Response** — `u64` rank or `Error(KeyNotFound)`.

---

### ZRANGE — 0x32

Return members by rank range.

**Request**
```rust
struct ZRangeRequest { key: bytes, start: i64, stop: i64, with_scores: bool }
```

**Response** — `Vec<ZSetMember>`. When `with_scores=false`, `score` fields are 0.0.

---

### ZRANGEBYSCORE — 0x33

Return members in a score range [min, max].

**Request**
```rust
struct ZRangeByScoreRequest { key: bytes, min: f64, max: f64 }
```

**Response** — `Vec<ZSetMember>` in ascending score order.

---

### ZCARD — 0x34

Return the number of members.

**Request** — `bytes` (raw key)

**Response** — `u64`

---

### ZREM — 0x35

Remove one or more members.

**Request**
```rust
struct ZRemRequest { key: bytes, members: Vec<bytes> }
```

**Response** — `u64` count removed.

---

### ZSCORE — 0x36

Return the score of a member.

**Request**
```rust
struct ZRankRequest { key: bytes, member: bytes }
```

**Response** — `f64` score or `Error(KeyNotFound)`.

---

## Counter Commands

Counter commands operate on `Value::Counter(i64)` keys. Using them on a key
that holds another type returns `WrongType`. A missing key is initialised to 0
before the operation.

### INCR — 0x40

Increment by 1.

**Request** — `bytes` (raw key)

**Response** — `i64` new value.

---

### DECR — 0x41

Decrement by 1.

**Request** — `bytes` (raw key)

**Response** — `i64`

---

### INCRBY — 0x42

Increment by a signed delta.

**Request**
```rust
struct IncrByRequest { key: bytes, delta: i64 }
```

**Response** — `i64`

---

### DECRBY — 0x43

Decrement by `delta`.

**Request** — same as `IncrByRequest`

**Response** — `i64`

---

### INCRBYFLOAT — 0x44

Increment by a floating-point delta. Operates on `Counter` keys; result is
stored back as `Counter` with the float value truncated if needed.

**Request**
```rust
struct IncrByFloatRequest { key: bytes, delta: f64 }
```

**Response** — `f64`

---

### GETSET — 0x45

Atomically swap a value, returning the old value.

**Request**
```rust
struct GetSetRequest { key: bytes, value: Value }
```

**Response** — `Value` (old value) or `Error(KeyNotFound)` if key was absent.

---

## JSON Commands

JSON values are stored internally as UTF-8 strings. Path selectors use
**JSONPath** syntax: `$` = root, `$.field`, `$.nested.field`, `$.arr[0]`.

### JSON.GET — 0x50

Fetch a value at a path.

**Request**
```rust
struct JsonGetRequest { key: bytes, path: String }
```

**Response** — JSON string or `Error(KeyNotFound)`.

---

### JSON.SET — 0x51

Set a value at a path.

**Request**
```rust
struct JsonSetRequest { key: bytes, path: String, value: String }
```

**Response** — `"OK"`.

Set `path = "$"` to replace the entire document.

---

### JSON.DEL — 0x52

Delete a node at a path.

**Request**
```rust
struct JsonDelRequest { key: bytes, path: String }
```

**Response** — `u64` count of nodes deleted (0 if path does not exist).

---

### JSON.EXISTS — 0x53

Check whether a path exists in a JSON document.

**Request**
```rust
struct JsonGetRequest { key: bytes, path: String }
```

**Response** — `bool`

---

### JSON.TYPE — 0x54

Return the JSON type of the value at a path.

**Request**
```rust
struct JsonGetRequest { key: bytes, path: String }
```

**Response** — `string`: `"object"`, `"array"`, `"string"`, `"number"`, `"boolean"`, `"null"`.

---

### JSON.ARRAPPEND — 0x55

Append a JSON-encoded value to an array at a path.

**Request**
```rust
struct JsonArrAppendRequest { key: bytes, path: String, value: String }
```

**Response** — `u64` new array length.

---

### JSON.NUMINCRBY — 0x56

Increment a numeric value at a path.

**Request**
```rust
struct JsonNumIncrByRequest { key: bytes, path: String, delta: f64 }
```

**Response** — `f64` new value.

---

## Authentication Commands

### AUTH — 0x60

Authenticate the current connection with a session token.

**Request** — `string` (HMAC-SHA256 session token)

**Response** — `"OK"` on success, `Error(TokenInvalid | TokenExpired | TokenRevoked)` on failure.

Must be the **first command** on every new connection.
Validation time: ~75 ns (HMAC verify + JTI blocklist lookup).

---

### REVOKE_TOKEN — 0x61

Immediately invalidate the current session token.

**Request** — `()` (unit / nil payload)

**Response** — `"OK"`.

Inserts the token's JTI into the in-memory blocklist. Subsequent AUTH attempts
with the same token return `TokenRevoked`.

---

## User Management Commands (admin role required)

### USER_CREATE — 0x62

Create a new user.

**Request**
```rust
struct UserCreateRequest {
    username: String,
    password: String,
    role:     String,   // "admin" | "readwrite" | "readonly"
}
```

**Response** — `string` (newly issued session token for the new user).

---

### USER_DELETE — 0x63

Delete a user.

**Request**
```rust
struct UserDeleteRequest { username: String }
```

**Response** — `"OK"`.

---

### USER_LIST — 0x64

List all registered usernames.

**Request** — `()` (unit)

**Response** — `Vec<string>` (usernames).

---

### USER_GRANT — 0x65

Grant a user access to a database by numeric ID.

**Request**
```rust
struct UserGrantRequest { username: String, db_id: u16 }
```

**Response** — `"OK"`.

A user with an empty `allowed_dbs` list has access to all databases.

---

### USER_REVOKE — 0x66

Revoke a user's access to a specific database.

**Request**
```rust
struct UserRevokeRequest { username: String, db_id: u16 }
```

**Response** — `"OK"`.

---

### USER_INFO — 0x67

Return information about a user.

**Request**
```rust
struct UserInfoRequest { username: Option<String> }   // None = calling user
```

**Response** — `(username: String, role: String, allowed_dbs: Vec<u16>)`.

---

### USER_SETROLE — 0x68

Change a user's role.

**Request**
```rust
struct UserSetRoleRequest { username: String, role: String }
```

**Response** — `"OK"`.

Valid roles: `"admin"`, `"readwrite"`, `"readonly"`.

---

## Observability Commands

### SLOWLOG — 0x70

Return the most recent slow commands.

**Request** — `()` (unit)

**Response** — `Vec<(command: String, key: bytes, duration_us: u64)>` sorted descending by duration.

The server retains the last 128 entries (configurable).

---

### METRICS — 0x71

Return a Prometheus metrics summary.

**Request** — `()` (unit)

**Response** — `(epoch_ms: u64, total_requests: u64)`.

For the full Prometheus scrape endpoint, use HTTP `GET :9090/metrics`.

---

### STATS — 0x72

Return a human-readable INFO-style server statistics block.

**Request** — `()` (unit)

**Response** — `string` (multi-line text, one `key: value` per line).

Fields include: `version`, `uptime_s`, `connected_clients`, `pool_bytes_used`,
`pool_bytes_max`, `memory_pressure`, `evictions_lfu`, `evictions_oom`,
`replication_keeper_count`, `raft_term`, `is_leader`, `warmup_state`.

---

### MEMORY_USAGE — 0x73

Estimate the memory footprint of a single key.

**Request** — `bytes` (raw key)

**Response** — `u64` approximate bytes. `0` if key does not exist.

---

### MONITOR — 0x74

Subscribe to the real-time command stream.

**Request** — `()` (unit)

**Initial response** — `"OK"` ACK.

After the ACK, the server continuously pushes frames with `req_id=0` and
`cmd_id=Ok` for every command executed. Each payload is a msgpack string:

```
"<timestamp_ms> <cmd_name> <key_hex>"
```

Use a dedicated connection for monitoring. Stop by closing the connection.

---

## Admin / Config Commands

### CONFIG — 0x80

Set a live configuration parameter.

**Request**
```rust
struct ConfigSetRequest { param: String, value: String }
```

**Response** — `"OK"`.

Hot-reloadable parameters: `memory.pool_bytes`, `memory.eviction_threshold`.
Parameters requiring restart return an error.

---

### CLUSTER_INFO — 0x81

Return a key-value summary of cluster state.

**Request** — `()` (unit)

**Response** — `Vec<(key: String, value: String)>`.

Fields: `raft_term`, `is_leader`, `leader_id`, `leader_addr`, `warmup_state`,
`supported_modes`, `memory_pressure`, `keeper_count`, `uptime_s`.

---

### CLUSTER_SLOTS — 0x82

Return the slot-to-node assignment table.

**Request** — `()` (unit)

**Response** — `Vec<(start: u16, end: u16, addr: String)>`.

Each tuple represents a contiguous slot range [start, end] (inclusive) owned
by the Core node at `addr`. In a single-Core deployment there is one entry
covering all 16 384 slots.

---

### KEEPER_LIST — 0x83

Return one entry per connected Keeper node.

**Request** — `()` (unit)

**Response** — `Vec<(node_id: u64, name: String, addr: String, pool_bytes: u64, used_bytes: u64)>`.

---

### POOL_STATS — 0x84

Return aggregate memory pool statistics.

**Request** — `()` (unit)

**Response** — `(used_bytes: u64, total_bytes: u64, keeper_count: usize)`.

`total_bytes` is the sum of Core RAM pool + all Keeper cold-store grants.

---

### WAIT — 0x85

Block until Keepers acknowledge all pending writes.

**Request**
```rust
struct WaitRequest { n_keepers: usize, timeout_ms: u64 }
```

**Response** — `u64` count of Keepers that ACKed within the timeout.

Returns immediately if there are no pending writes. Use this to ensure
durability before reading from a replica or signalling to a caller that a
write is safe.

---

## Database Namespace Commands

MnemeCache supports up to 65 536 logical databases (default 16). Database 0
is the default; all connections start on database 0. Named databases map a
human-readable name to a numeric ID.

### SELECT — 0x86

Switch the active database for the current connection.

**Request**
```rust
struct SelectRequest { db_id: u16, name: String }
```

Supply either `db_id` or `name` (non-empty `name` takes priority).

**Response** — `"OK"`.

---

### DBSIZE — 0x87

Count live (non-expired) keys in a database.

**Request**
```rust
struct DbSizeRequest { db_id: Option<u16>, name: String }
```

`None` db_id and empty name → use connection's active database.

**Response** — `u64`

---

### FLUSHDB — 0x88

Delete all keys in a database.

**Request**
```rust
struct FlushDbRequest { db_id: Option<u16>, name: String, sync: bool }
```

`sync=true` (default) replicates delete tombstones to Keepers.

**Response** — `"OK"`.

---

### SCAN — 0x89

Cursor-based iteration over keys.

**Request**
```rust
struct ScanRequest {
    cursor:  u64,            // 0 = begin new scan
    pattern: Option<String>, // glob: "prefix*", "*suffix", "*sub*", exact
    count:   u64,            // hint; default 10, max 1 000
}
```

**Response** — `(next_cursor: u64, keys: Vec<bytes>)`.
`next_cursor=0` signals scan completion.

---

### TYPE — 0x8A

Return the type string of a key's value.

**Request** — `bytes` (raw key)

**Response** — `string`: `"string"`, `"hash"`, `"list"`, `"zset"`, `"counter"`, `"json"`.
Returns `Error(KeyNotFound)` if key absent.

---

### MGET — 0x8B

Bulk fetch up to 1 000 keys.

**Request**
```rust
struct MGetRequest { keys: Vec<bytes> }
```

**Response** — `Vec<Option<Value>>`. Each element is `Some(value)` or `None` if not found.

---

### MSET — 0x8C

Bulk set up to 1 000 key-value pairs.

**Request**
```rust
struct MSetRequest { pairs: Vec<(key: bytes, value: Value, ttl_ms: u64)> }
```

`ttl_ms=0` = no expiry.

**Response** — `"OK"`.

---

### DB_CREATE — 0x8D

Register a named database.

**Request**
```rust
struct DbCreateRequest { name: String, db_id: Option<u16> }
```

`db_id=None` → server assigns the next available ID.

**Response** — `u16` (assigned database ID).

---

### GEN_JOIN_TOKEN — 0x8E

Generate a one-time Keeper join token.

**Request** — `()` (unit) — admin role required.

**Response** — `string` (token). Valid for `auth.join_token_ttl_s` seconds (default 300).

Pass this token to `mneme-keeper --join-token <TOKEN>` when adding a new node.

---

### DB_LIST — 0x8F

List all registered named databases.

**Request** — `()` (unit)

**Response** — `Vec<(name: String, id: u16)>`.

---

### DB_DROP — 0x90

Unregister a named database. **Does not delete data** — keys remain stored
under the numeric ID and are still accessible by ID.

**Request**
```rust
struct DbDropRequest { name: String }
```

**Response** — `"OK"`.

---

## Response Frames

| CmdId           | Hex  | Payload |
|-----------------|------|---------|
| OK              | 0xF0 | Command-specific result (see above) |
| ERROR           | 0xF1 | `string` — error name and optional detail |
| LEADER_REDIRECT | 0xB3 | `{ leader_addr: String }` |

---

## Error Reference

| Error | When |
|-------|------|
| `KeyNotFound` | Key absent or expired |
| `WrongType` | Operation type mismatch (e.g. HGET on a Counter key) |
| `TokenExpired` | Session token TTL elapsed |
| `TokenInvalid` | HMAC signature mismatch |
| `TokenRevoked` | Token in JTI blocklist |
| `MaxConnectionsReached` | Server at `charon.max_total` (100 000) or `max_per_ip` (1 000) |
| `RequestTimeout` | Exceeded 5 000 ms per-request deadline |
| `SlotMoved { slot, addr }` | Slot migrated — retry at `addr` |
| `QuorumNotReached { got, need }` | Insufficient Keeper ACKs |
| `OutOfMemory` | RAM pool full, eviction failed |
| `KeeperUnreachable` | Required Keeper offline |
| `WalWriteFailed` | Aoide WAL write / fsync failure |
| `SnapshotFailed` | Melete snapshot write failure |
| `ProtocolViolation` | Bad frame header or internal command on client port |
| `UnknownCommand` | Unrecognised CmdId |
| `PayloadTooLarge` | Key > 512 B, value > 10 MB, or batch > 1 000 keys |

---

## Payload Limits

| Limit | Default |
|-------|---------|
| Max key size | 512 bytes |
| Max value size | 10 MB |
| Max hash field count | 65 536 |
| Max batch keys (MGET / MSET / DEL) | 1 000 |

---

## Internal / Replication Commands

These command IDs appear only on the mTLS replication port (7379).
Sending them on the client port (6379) returns `ProtocolViolation`.

| CmdId         | Hex  | Direction | Description |
|---------------|------|-----------|-------------|
| SYNC_START    | 0xA0 | Keeper→Core | Begin sync session; carries `SyncStartPayload` |
| PUSH_KEY      | 0xA1 | Core→Keeper | Replicate one key-value pair with TTL |
| HEARTBEAT     | 0xA2 | Core↔Keeper | Keepalive + Keeper stats |
| MOVED         | 0xA3 | Core→Client | Slot migration redirect |
| ACK_WRITE     | 0xA4 | Keeper→Core | Acknowledge a replicated write |
| SYNC_REQUEST  | 0xA5 | Keeper→Core | Request delta replay from a sequence number |
| SYNC_COMPLETE | 0xA6 | Keeper→Core | Warmup complete; carries pushed key count |

### Raft Commands (Core-to-Core, mTLS port)

| CmdId                | Hex  |
|----------------------|------|
| RAFT_APPEND_ENTRIES  | 0xB0 |
| RAFT_VOTE            | 0xB1 |
| RAFT_INSTALL_SNAPSHOT| 0xB2 |
| LEADER_REDIRECT      | 0xB3 |

`LEADER_REDIRECT (0xB3)` is both a Raft-layer frame and a client-facing
response. When sent to a client it carries `{ leader_addr: String }`.
