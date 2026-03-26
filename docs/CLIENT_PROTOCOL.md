# MnemeCache Wire Protocol

Everything a client implementer needs to build a compatible MnemeCache client
in any language. The Rust client **Pontus** (`mneme-client/`) is the reference
implementation.

---

## 1. Frame Format (16-byte header + payload)

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                      Magic (0x4D4E454D)                       |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|    Version    |    CmdId      |            Flags              |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                        Payload Length                         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                          Request ID                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                       Payload (msgpack)                       |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

| Offset | Size | Field          | Description |
|--------|------|----------------|-------------|
| 0      | 4 B  | Magic          | `0x4D4E454D` ("MNEM") — big-endian |
| 4      | 1 B  | Version        | Protocol version (`0x01`) |
| 5      | 1 B  | CmdId          | Command identifier (see §2) |
| 6      | 2 B  | Flags          | Big-endian; bits 15–4 = slot hint, bits 3–2 = consistency, bits 1–0 = reserved |
| 8      | 4 B  | Payload Length | Big-endian u32 — length of msgpack payload in bytes |
| 12     | 4 B  | Request ID     | Big-endian u32; 0 = single-plex, 1+ = multiplexed |
| 16     | var  | Payload        | MessagePack-encoded request/response body |

---

## 2. Command IDs

### String / Generic KV

| CmdId  | Hex  | Request Payload | Response Payload |
|--------|------|-----------------|------------------|
| GET    | 0x01 | `GetRequest { key: bytes }` | `Value` or Error(KeyNotFound) |
| SET    | 0x02 | `SetRequest { key: bytes, value: Value, ttl_ms: u64 }` | `"OK"` string |
| DEL    | 0x03 | `DelRequest { keys: [bytes] }` | `u64` (count deleted) |
| EXISTS | 0x04 | `bytes` (key) | `bool` |
| EXPIRE | 0x05 | `ExpireRequest { key: bytes, seconds: u64 }` | `u64` (1=applied, 0=not found) |
| TTL    | 0x06 | `bytes` (key) | `i64` ms remaining (−1 = no expiry, −2 = not found) |

### Hash

| CmdId   | Hex  | Request Payload | Response Payload |
|---------|------|-----------------|------------------|
| HSET    | 0x10 | `HSetRequest { key: bytes, pairs: [(field: bytes, value: bytes)] }` | `"OK"` |
| HGET    | 0x11 | `HGetRequest { key: bytes, field: bytes }` | `bytes` (raw value) or Error |
| HDEL    | 0x12 | `HDelRequest { key: bytes, fields: [bytes] }` | `u64` (count removed) |
| HGETALL | 0x13 | `bytes` (key) | `[(field: bytes, value: bytes)]` |

### List

| CmdId  | Hex  | Request Payload | Response Payload |
|--------|------|-----------------|------------------|
| LPUSH  | 0x20 | `ListPushRequest { key: bytes, values: [bytes] }` | `u64` (new length) |
| RPUSH  | 0x21 | `ListPushRequest { key: bytes, values: [bytes] }` | `u64` (new length) |
| LPOP   | 0x22 | `bytes` (key) | `Value` or Error(KeyNotFound) |
| RPOP   | 0x23 | `bytes` (key) | `Value` or Error(KeyNotFound) |
| LRANGE | 0x24 | `LRangeRequest { key: bytes, start: i64, stop: i64 }` | `[Value]` |

### Sorted Set

| CmdId         | Hex  | Request Payload | Response Payload |
|---------------|------|-----------------|------------------|
| ZADD          | 0x30 | `ZAddRequest { key: bytes, members: [ZSetMember] }` | `u64` (added) |
| ZRANK         | 0x31 | `ZRankRequest { key: bytes, member: bytes }` | `u64` or Error |
| ZRANGE        | 0x32 | `ZRangeRequest { key: bytes, start: i64, stop: i64, with_scores: bool }` | `[ZSetMember]` |
| ZRANGEBYSCORE | 0x33 | `ZRangeByScoreRequest { key: bytes, min: f64, max: f64 }` | `[ZSetMember]` |
| ZCARD         | 0x34 | `bytes` (key) | `u64` |
| ZREM          | 0x35 | `ZRemRequest { key: bytes, members: [bytes] }` | `u64` (removed) |
| ZSCORE        | 0x36 | `ZRankRequest { key: bytes, member: bytes }` | `f64` or Error |

`ZSetMember`: `{ member: bytes, score: f64 }`

### Counters

| CmdId       | Hex  | Request Payload | Response Payload |
|-------------|------|-----------------|------------------|
| INCR        | 0x40 | `bytes` (key) | `i64` (new value) |
| DECR        | 0x41 | `bytes` (key) | `i64` |
| INCRBY      | 0x42 | `IncrByRequest { key: bytes, delta: i64 }` | `i64` |
| DECRBY      | 0x43 | `IncrByRequest { key: bytes, delta: i64 }` | `i64` |
| INCRBYFLOAT | 0x44 | `IncrByFloatRequest { key: bytes, delta: f64 }` | `f64` |
| GETSET      | 0x45 | `GetSetRequest { key: bytes, value: Value }` | `Value` (old value) |

### JSON

| CmdId          | Hex  | Request Payload | Response Payload |
|----------------|------|-----------------|------------------|
| JSON.GET       | 0x50 | `JsonGetRequest { key: bytes, path: string }` | JSON string |
| JSON.SET       | 0x51 | `JsonSetRequest { key: bytes, path: string, value: string }` | `"OK"` |
| JSON.DEL       | 0x52 | `JsonDelRequest { key: bytes, path: string }` | `u64` (deleted) |
| JSON.EXISTS    | 0x53 | `JsonGetRequest { key: bytes, path: string }` | `bool` |
| JSON.TYPE      | 0x54 | `JsonGetRequest { key: bytes, path: string }` | `string` (type name) |
| JSON.ARRAPPEND | 0x55 | `JsonArrAppendRequest { key: bytes, path: string, value: string }` | `u64` (new length) |
| JSON.NUMINCRBY | 0x56 | `JsonNumIncrByRequest { key: bytes, path: string, delta: f64 }` | `f64` |

The `path` field uses JSONPath syntax (`$` = root, `$.field`, `$.nested.field`).
`value` in SET/ARRAPPEND is a JSON-encoded string.

### Authentication

| CmdId        | Hex  | Request Payload | Response Payload |
|--------------|------|-----------------|------------------|
| AUTH         | 0x60 | `string` (HMAC session token) | `"OK"` or Error |
| REVOKE_TOKEN | 0x61 | `()` (unit / nil) | `"OK"` |

### User Management (admin-only)

| CmdId        | Hex  | Request Payload | Response Payload |
|--------------|------|-----------------|------------------|
| USER_CREATE  | 0x62 | `UserCreateRequest { username, password, role }` | `string` (session token) |
| USER_DELETE  | 0x63 | `UserDeleteRequest { username }` | `"OK"` |
| USER_LIST    | 0x64 | `()` | `[string]` (usernames) |
| USER_GRANT   | 0x65 | `UserGrantRequest { username, db_id: u16 }` | `"OK"` |
| USER_REVOKE  | 0x66 | `UserRevokeRequest { username, db_id: u16 }` | `"OK"` |
| USER_INFO    | 0x67 | `UserInfoRequest { username: Option<string> }` | `(username, role, [db_id])` |
| USER_SETROLE | 0x68 | `UserSetRoleRequest { username, role }` | `"OK"` |

Roles: `"admin"`, `"readwrite"`, `"readonly"`.
An empty `allowed_dbs` list means access to all databases.

### Observability

| CmdId        | Hex  | Request Payload | Response Payload |
|--------------|------|-----------------|------------------|
| SLOWLOG      | 0x70 | `()` | `[(command: string, key: bytes, duration_us: u64)]` |
| METRICS      | 0x71 | `()` | `(epoch_ms: u64, total_requests: u64)` |
| STATS        | 0x72 | `()` | `string` (INFO-style text block) |
| MEMORY_USAGE | 0x73 | `bytes` (key) | `u64` (approx bytes, 0 = not found) |
| MONITOR      | 0x74 | `()` | `"OK"` ACK, then server pushes event strings with req_id=0 |

`MONITOR` activates a streaming mode: after the initial `OK` ACK, the server
continuously pushes frames with `req_id=0` and `cmd_id=Ok` for every command
executed. Each payload is a msgpack string: `"<timestamp_ms> <cmd> <key>"`.
Stop monitoring by closing the connection.

### Admin / Config

| CmdId          | Hex  | Request Payload | Response Payload |
|----------------|------|-----------------|------------------|
| CONFIG         | 0x80 | `ConfigSetRequest { param: string, value: string }` | `"OK"` |
| CLUSTER_INFO   | 0x81 | `()` | `[(key: string, value: string)]` |
| CLUSTER_SLOTS  | 0x82 | `()` | `[(start: u16, end: u16, addr: string)]` |
| KEEPER_LIST    | 0x83 | `()` | `[(node_id: u64, name: string, addr: string, pool_bytes: u64, used_bytes: u64)]` |
| POOL_STATS     | 0x84 | `()` | `(used_bytes: u64, total_bytes: u64, keeper_count: usize)` |
| WAIT           | 0x85 | `WaitRequest { n_keepers: usize, timeout_ms: u64 }` | `u64` (ACK count) |

`CLUSTER_INFO` returns key-value pairs including:
`raft_term`, `is_leader`, `leader_id`, `warmup_state`, `supported_modes`,
`memory_pressure`, `keeper_count`, `uptime_s`.

`WAIT` blocks until `n_keepers` Keeper nodes have acknowledged all outstanding
writes, or until `timeout_ms` elapses. Returns the count that ACKed in time.

### Database Namespace

| CmdId            | Hex  | Request Payload | Response Payload |
|------------------|------|-----------------|------------------|
| SELECT           | 0x86 | `SelectRequest { db_id: u16, name: string }` | `"OK"` |
| DBSIZE           | 0x87 | `DbSizeRequest { db_id: Option<u16>, name: string }` | `u64` |
| FLUSHDB          | 0x88 | `FlushDbRequest { db_id: Option<u16>, name: string, sync: bool }` | `"OK"` |
| SCAN             | 0x89 | `ScanRequest { cursor: u64, pattern: Option<string>, count: u64 }` | `(next_cursor: u64, [key: bytes])` |
| TYPE             | 0x8A | `bytes` (key) | `string` ("string", "hash", "list", "zset", "counter", "json") |
| MGET             | 0x8B | `MGetRequest { keys: [bytes] }` | `[Option<Value>]` |
| MSET             | 0x8C | `MSetRequest { pairs: [(key: bytes, value: Value, ttl_ms: u64)] }` | `"OK"` |
| DB_CREATE        | 0x8D | `DbCreateRequest { name: string, db_id: Option<u16> }` | `u16` (assigned ID) |
| GEN_JOIN_TOKEN   | 0x8E | `()` | `string` (one-time join token) |
| DB_LIST          | 0x8F | `()` | `[(name: string, id: u16)]` |
| DB_DROP          | 0x90 | `DbDropRequest { name: string }` | `"OK"` |

`SCAN` cursor: 0 = begin new scan; `next_cursor=0` in response = scan complete.
`FLUSHDB sync=true` replicates delete tombstones to Keepers (default).

### Response Frames

| CmdId           | Hex  | Description |
|-----------------|------|-------------|
| OK              | 0xF0 | Success — payload is the result |
| ERROR           | 0xF1 | Failure — payload is msgpack string with error message |
| LEADER_REDIRECT | 0xB3 | Payload: `LeaderRedirectPayload { leader_addr: string }` |

---

## 3. Authentication Flow

1. Obtain a session token via `USER_CREATE` or the admin API.
2. Establish TLS connection to Core port 6379.
3. Send `AUTH` frame with the token as the **first command** after TLS handshake.
4. The server validates the HMAC-SHA256 signature, extracts claims (username, role, allowed_dbs).
5. All subsequent commands on this connection are authorized against those claims.
6. Tokens expire after `auth.token_ttl_h` hours (default 24). Re-authenticate on `TokenExpired`.
7. `REVOKE_TOKEN` immediately adds the JTI to the blocklist.

---

## 4. Request Multiplexing

A single TLS connection supports parallel in-flight requests using `req_id`:

- **req_id = 0**: single-plex — one request at a time, responses in order.
- **req_id ≥ 1**: multiplexed — multiple requests outstanding simultaneously.
  Responses may arrive **out of order**. Match them to requests by `req_id`.
  The server echoes `req_id` in the response frame unchanged.

Allocate `req_id` values sequentially (1, 2, 3, …), skipping 0. After u32
wrap-around, skip any ID still present in your pending map. The Pontus library
manages this automatically with a `DashMap<u32, oneshot::Sender<Frame>>`.

---

## 5. Consistency Levels

Encoded in flags bits 3–2:

| Bits | Level    | Behavior |
|------|----------|----------|
| `00` | EVENTUAL | Fire-and-forget writes; reads from any available replica. AP mode. |
| `01` | QUORUM   | Wait for `floor(N/2)+1` Keeper ACKs. **Default for writes.** |
| `10` | ALL      | Wait for every Keeper ACK. Highest durability, highest latency. |
| `11` | ONE      | Wait for the first Keeper ACK. Lowest-latency durable write. |

Set per-request:
```
flags = (flags & 0xFFF3) | (consistency_bits << 2)
```

Reads use EVENTUAL by default. The warmup gate enforces: QUORUM/ALL reads are
blocked until the Core node's warmup state reaches `Hot` after restart.

---

## 6. Leader Redirect (HA Clusters)

In a multi-Core Raft cluster, **write commands must go to the leader**. If a
write reaches a follower, the server returns:

```
CmdId: 0xB3 (LeaderRedirect)
Payload: { leader_addr: "10.0.0.1:6379" }
```

Client behaviour:
1. On `LeaderRedirect`, reconnect to `leader_addr` and retry the write.
2. If `leader_addr` is empty, an election is in progress — back off and retry.
3. Read commands are served locally on any Core node (including followers).
4. Cache the leader address to avoid unnecessary redirects on subsequent writes.

---

## 7. Error Codes

Error responses (`CmdId::Error = 0xF1`) carry a msgpack-encoded string.

| Error | Description |
|-------|-------------|
| `KeyNotFound` | Key does not exist or has expired |
| `WrongType` | Operation against a mismatched value type |
| `TokenExpired` | Session token TTL has elapsed |
| `TokenInvalid` | HMAC signature verification failed |
| `TokenRevoked` | Token was explicitly revoked via REVOKE_TOKEN |
| `MaxConnectionsReached` | Server at connection limit (`charon.max_total`) |
| `RequestTimeout` | Request exceeded per-request deadline (5 s default) |
| `SlotMoved { slot, addr }` | Slot migrated to another node — retry at `addr` |
| `QuorumNotReached { got, need }` | Insufficient Keeper ACKs for consistency level |
| `OutOfMemory` | RAM pool exhausted, eviction failed |
| `KeeperUnreachable` | Required Keeper is offline |
| `PayloadTooLarge` | Key or value exceeds payload limits |
| `UnknownCommand` | Unrecognised CmdId byte |
| `ProtocolViolation` | Malformed frame header or payload |

---

## 8. Connection Lifecycle

1. **TCP connect** to Core client port (default 6379). Set `TCP_NODELAY`.
2. **TLS 1.3 handshake**. Provide the CA cert to validate the server certificate.
   For production, use a publicly-trusted cert (`tls.client_cert`/`client_key`).
3. **AUTH** — send `AUTH 0x60` frame with session token as first command.
4. **Send commands** — each command is a Frame with the appropriate CmdId.
5. **Receive responses** — match by `req_id` if multiplexing.
6. **Keepalive** — server closes idle connections after `idle_timeout_s` (default 30 s).
   Send periodic commands or implement your own ping.
7. **Reconnect** — re-send `AUTH` after reconnecting.

---

## 9. Payload Limits

| Limit | Default |
|-------|---------|
| Max key size | 512 bytes |
| Max value size | 10 MB |
| Max hash field count | 65 536 |
| Max batch keys (MGET/MSET) | 1 000 |

Exceeding any limit returns `PayloadTooLarge` and the command is rejected.

---

## 10. Value Types

The `Value` enum is msgpack-encoded in request and response payloads:

| Variant | Rust type | msgpack encoding | Description |
|---------|-----------|-----------------|-------------|
| `String(Vec<u8>)` | bytes | bin | Raw byte string |
| `Counter(i64)` | i64 | int | Signed 64-bit integer |
| `Hash(...)` | map | map | Field-value map |
| `List(...)` | vec | array | Ordered list of values |
| `ZSet(...)` | vec | array | Sorted set — array of `[member, score]` pairs |
| `Json(...)` | string | str | JSON document (stored as UTF-8 string) |

Use `Value::String` for arbitrary byte data. Use `Value::Counter` for integer
counters. The `INCR`/`DECR` family operates exclusively on `Counter` values and
returns an error for keys that hold other types.

---

## 11. Hash Tag Routing

Keys support hash tags for co-location: `{tag}` — only the content inside the
first `{...}` pair is hashed. Keys with the same tag are guaranteed to map to
the same slot:

```
slot("user:{alice}:profile")  == slot("user:{alice}:settings")
slot("{order:1}:items")       == slot("{order:1}:total")
```

Slot = `CRC16-CCITT(hash_tag) % 16384`.

---

## 12. Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 6379 | TLS 1.3  | Client connections |
| 7379 | mTLS     | Internal replication (Core↔Keeper, Raft peer-to-peer) |
| 9090 | HTTP     | Prometheus metrics (no auth) |

Clients connect only to port 6379. The mTLS replication port (7379) is
cluster-internal and requires the auto-generated CA certificate.

---

## 13. Reference Implementation

The Rust client library **Pontus** (`mneme-client/`) demonstrates:

- Connection pooling with health checks, idle timeout, and min-idle warm-up (`pool.rs`)
- TLS session resumption across pool connections
- Request multiplexing with req_id dispatch (`conn.rs`)
- Full command surface (`cmd_kv.rs`, `cmd_hash.rs`, `cmd_list.rs`, `cmd_zset.rs`, `cmd_json.rs`, `cmd_db.rs`, `cmd_admin.rs`)
- Pipeline batching — multiple frames in one `write_all()` (`cmd_pipeline.rs`)
- MONITOR streaming subscription
- Multi-address HA failover (pool tries next address on `LeaderRedirect` or connection failure)

See [mneme-client/README.md](../mneme-client/README.md) for usage examples.
