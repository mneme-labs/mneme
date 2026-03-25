# MnemeCache Wire Protocol

Everything a client implementer needs to build a compatible MnemeCache client
in any language.  The Rust client **Pontus** (`mneme-client/`) is the reference
implementation.

---

## 1. Frame Format (16 bytes header + payload)

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                      Magic (0x4D4E454D)                       |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|    Version    |    CmdId      |            Flags              |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                        Payload Length                          |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                          Request ID                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                       Payload (msgpack)                       |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

| Offset | Size | Field          | Description |
|--------|------|----------------|-------------|
| 0      | 4B   | Magic          | `0x4D4E454D` ("MNEM") — big-endian |
| 4      | 1B   | Version        | Protocol version (`0x01`) |
| 5      | 1B   | CmdId          | Command identifier (see table below) |
| 6      | 2B   | Flags          | Big-endian; bits 15-4 = slot hint, bits 3-2 = consistency, bits 1-0 = reserved |
| 8      | 4B   | Payload Length | Big-endian u32, length of msgpack payload |
| 12     | 4B   | Request ID     | Big-endian u32; 0 = single-plex, 1+ = multiplexed |
| 16     | var  | Payload        | MessagePack-encoded request/response body |

---

## 2. Command IDs

### String / Generic

| CmdId | Hex  | Request Payload | Response Payload |
|-------|------|-----------------|------------------|
| GET   | 0x01 | `{ key: bytes }` | Value (msgpack) or Error(KeyNotFound) |
| SET   | 0x02 | `{ key: bytes, value: Value, ttl_ms: u64 }` | "OK" (string) |
| DEL   | 0x03 | `{ keys: [bytes] }` | u64 (count deleted) |
| EXISTS| 0x04 | `bytes` (key) | bool |
| EXPIRE| 0x05 | `{ key: bytes, ttl_ms: u64 }` | bool |
| TTL   | 0x06 | `bytes` (key) | i64 (ms remaining, -1 = no expiry, -2 = not found) |

### Hash

| CmdId  | Hex  | Request Payload | Response Payload |
|--------|------|-----------------|------------------|
| HSET   | 0x10 | `{ key: bytes, field: bytes, value: Value }` | "OK" |
| HGET   | 0x11 | `{ key: bytes, field: bytes }` | Value or Error |
| HDEL   | 0x12 | `{ key: bytes, fields: [bytes] }` | u64 (count) |
| HGETALL| 0x13 | `bytes` (key) | `[(field, value), ...]` |

### List

| CmdId  | Hex  | Request Payload | Response Payload |
|--------|------|-----------------|------------------|
| LPUSH  | 0x20 | `{ key: bytes, value: Value }` | u64 (new length) |
| RPUSH  | 0x21 | `{ key: bytes, value: Value }` | u64 (new length) |
| LPOP   | 0x22 | `bytes` (key) | Value or Error |
| RPOP   | 0x23 | `bytes` (key) | Value or Error |
| LRANGE | 0x24 | `{ key: bytes, start: i64, stop: i64 }` | `[Value, ...]` |

### Sorted Set

| CmdId        | Hex  | Request Payload | Response Payload |
|--------------|------|-----------------|------------------|
| ZADD         | 0x30 | `{ key: bytes, members: [{ member: bytes, score: f64 }] }` | u64 (added) |
| ZRANK        | 0x31 | `{ key: bytes, member: bytes }` | u64 or Error |
| ZRANGE       | 0x32 | `{ key: bytes, start: i64, stop: i64, with_scores: bool }` | `[ZSetMember, ...]` |
| ZRANGEBYSCORE| 0x33 | `{ key: bytes, min: f64, max: f64, with_scores: bool, offset: u64, count: u64 }` | `[ZSetMember, ...]` |
| ZCARD        | 0x34 | `bytes` (key) | u64 |
| ZREM         | 0x35 | `{ key: bytes, members: [bytes] }` | u64 (removed) |
| ZSCORE       | 0x36 | `{ key: bytes, member: bytes }` | f64 or Error |

### Counters

| CmdId      | Hex  | Request Payload | Response Payload |
|------------|------|-----------------|------------------|
| INCR       | 0x40 | `bytes` (key) | i64 (new value) |
| DECR       | 0x41 | `bytes` (key) | i64 |
| INCRBY     | 0x42 | `{ key: bytes, delta: i64 }` | i64 |
| DECRBY     | 0x43 | `{ key: bytes, delta: i64 }` | i64 |
| INCRBYFLOAT| 0x44 | `{ key: bytes, delta: f64 }` | f64 |
| GETSET     | 0x45 | `{ key: bytes, value: Value }` | Value (old) |

### JSON

| CmdId         | Hex  | Request Payload | Response Payload |
|---------------|------|-----------------|------------------|
| JSON.GET      | 0x50 | `{ key: bytes, path: string }` | JsonDoc |
| JSON.SET      | 0x51 | `{ key: bytes, path: string, value: JsonDoc }` | "OK" |
| JSON.DEL      | 0x52 | `{ key: bytes, path: string }` | u64 (deleted) |
| JSON.EXISTS   | 0x53 | `{ key: bytes, path: string }` | bool |
| JSON.TYPE     | 0x54 | `{ key: bytes, path: string }` | string |
| JSON.ARRAPPEND| 0x55 | `{ key: bytes, path: string, values: [JsonDoc] }` | u64 (new len) |
| JSON.NUMINCRBY| 0x56 | `{ key: bytes, path: string, delta: f64 }` | f64 |

### Authentication

| CmdId       | Hex  | Request Payload | Response Payload |
|-------------|------|-----------------|------------------|
| AUTH        | 0x60 | string (token)  | "OK" or Error |
| REVOKE_TOKEN| 0x61 | `()` (unit)     | "OK" |

### User Management (admin-only)

| CmdId       | Hex  | Request Payload | Response Payload |
|-------------|------|-----------------|------------------|
| USER_CREATE | 0x62 | `{ username: string, password: string, role: string }` | string (token) |
| USER_DELETE | 0x63 | `{ username: string }` | "OK" |
| USER_LIST   | 0x64 | `()` | `[UserInfo, ...]` |
| USER_GRANT  | 0x65 | `{ username: string, db_name: string }` | "OK" |
| USER_REVOKE | 0x66 | `{ username: string, db_name: string }` | "OK" |
| USER_INFO   | 0x67 | `{ username: string }` | UserInfo |
| USER_SETROLE| 0x68 | `{ username: string, role: string }` | "OK" |

Roles: `"admin"`, `"readwrite"`, `"readonly"`

### Observability

| CmdId       | Hex  | Description |
|-------------|------|-------------|
| SLOWLOG     | 0x70 | Get slow query log entries |
| METRICS     | 0x71 | Get Prometheus metrics text |
| STATS       | 0x72 | Get server statistics |
| MEMORY_USAGE| 0x73 | Get memory usage info |
| MONITOR     | 0x74 | Stream all commands (debug) |

### Admin / Config

| CmdId          | Hex  | Description |
|----------------|------|-------------|
| CONFIG         | 0x80 | `{ key: string, value: string }` — set runtime config |
| CLUSTER_INFO   | 0x81 | Get cluster status (leader, term, members, warmup) |
| CLUSTER_SLOTS  | 0x82 | Get slot-to-node mapping |
| KEEPER_LIST    | 0x83 | List connected keeper nodes |
| POOL_STATS     | 0x84 | Connection pool statistics |
| WAIT           | 0x85 | `{ replicas: u64, timeout_ms: u64 }` — wait for replication |

### Database Namespace

| CmdId    | Hex  | Description |
|----------|------|-------------|
| SELECT   | 0x86 | `{ db_name: string }` — switch to named database |
| DBSIZE   | 0x87 | `{ db_id: u16 }` — key count in database |
| FLUSHDB  | 0x88 | `{ db_id: u16 }` — delete all keys in database |
| SCAN     | 0x89 | `{ cursor: u64, pattern: string, count: u64, db_id: u16 }` |
| TYPE     | 0x8A | `bytes` (key) — return value type string |
| MGET     | 0x8B | `{ keys: [bytes] }` — bulk get |
| MSET     | 0x8C | `{ entries: [{ key: bytes, value: Value }] }` — bulk set |
| DB_CREATE| 0x8D | `{ name: string }` — create named database |
| DB_LIST  | 0x8F | List all named databases |
| DB_DROP  | 0x90 | `{ name: string }` — drop named database |
| GEN_JOIN_TOKEN | 0x8E | Generate a keeper join token (admin) |

### Responses

| CmdId  | Hex  | Description |
|--------|------|-------------|
| OK     | 0xF0 | Success — payload is the result |
| ERROR  | 0xF1 | Failure — payload is msgpack string with error message |
| LEADER_REDIRECT | 0xB3 | `{ leader_addr: string }` — retry at the leader address |

---

## 3. Authentication Flow

1. Obtain a session token via the admin API or `USER_CREATE`.
2. Send `AUTH` frame with the token as the first command after TLS handshake.
3. Server validates the HMAC-SHA256 token and extracts claims (username, role, allowed databases).
4. All subsequent commands are authorized against the token's role and database allowlist.
5. Tokens have a configurable TTL (default: 24h). On expiry, re-authenticate.
6. `REVOKE_TOKEN` immediately invalidates the current token (JTI blocklist).

---

## 4. Request Multiplexing

A single TLS connection supports parallel in-flight requests using `req_id`:

- **req_id = 0**: single-plex mode (one request at a time, responses in order).
- **req_id >= 1**: multiplexed mode. Each request gets a unique `req_id`; responses
  may arrive out of order. The client must match responses to requests by `req_id`.

Allocate `req_id` values sequentially (1, 2, 3, ...), skipping 0. After u32 wrap,
skip any ID still pending. The server echoes `req_id` in the response frame.

---

## 5. Consistency Levels

Encoded in flags bits 3-2:

| Bits | Level    | Behavior |
|------|----------|----------|
| 00   | EVENTUAL | Fire-and-forget writes; read from any replica |
| 01   | QUORUM   | Wait for floor(N/2)+1 keeper ACKs (default) |
| 10   | ALL      | Wait for all keeper ACKs |
| 11   | ONE      | Wait for first keeper ACK |

Set consistency per-request in the flags field:
```
flags = (flags & 0xFFF3) | (consistency << 2)
```

---

## 6. Leader Redirect (HA)

In a multi-Core Raft cluster, write commands must go to the leader.  If a write
is sent to a follower, it returns:

```
CmdId: 0xB3 (LeaderRedirect)
Payload: { leader_addr: "10.0.0.1:6379" }
```

Client behavior:
1. On receiving `LeaderRedirect`, reconnect to `leader_addr` and retry the write.
2. If `leader_addr` is empty, the leader is unknown (election in progress) — back off and retry.
3. Read commands (GET, HGET, LRANGE, etc.) are served locally on any Core node.
4. Cache the leader address for subsequent writes to avoid unnecessary redirects.

---

## 7. Error Codes

Error responses (`CmdId::Error = 0xF1`) carry a msgpack string.  Known error types:

| Error | Description |
|-------|-------------|
| `KeyNotFound` | Key does not exist |
| `WrongType` | Operation against wrong value type |
| `TokenExpired` | Session token has expired |
| `TokenInvalid` | Token signature verification failed |
| `TokenRevoked` | Token was explicitly revoked |
| `MaxConnectionsReached` | Server at connection limit |
| `RequestTimeout` | Request exceeded deadline |
| `SlotMoved { slot, addr }` | Slot migrated to another node |
| `QuorumNotReached { got, need }` | Insufficient keeper ACKs |
| `OutOfMemory` | Memory pool exhausted |
| `PayloadTooLarge` | Key or value exceeds limits |
| `UnknownCommand` | Unrecognized CmdId byte |
| `ProtocolViolation` | Malformed frame |

---

## 8. Connection Lifecycle

1. **TCP connect** to the Core node's client port (default: 6379).
2. **TLS 1.3 handshake**. For self-signed certs, skip CA verification
   (`--insecure` / disable cert verification in your TLS library).
   For production, use a publicly-trusted cert on the Core.
3. **Authenticate** — send `AUTH` frame with your session token.
4. **Send commands** — each command is a Frame with the appropriate CmdId.
5. **Read responses** — match by `req_id` if multiplexing.
6. **Keepalive** — server closes idle connections after `idle_timeout_s` (default: 30s).
   Send periodic PINGs or real commands to keep alive.
7. **Reconnect** on disconnect — re-authenticate after reconnecting.

TCP_NODELAY is recommended for low-latency operation.

---

## 9. Payload Limits

| Limit | Default |
|-------|---------|
| Max key size | 512 bytes |
| Max value size | 10 MB |
| Max field count (hash) | 65,536 |
| Max batch keys (MGET/MSET) | 1,000 |

Exceeding these returns `PayloadTooLarge`.

---

## 10. Value Types

The `Value` enum is msgpack-encoded with tagged variants:

| Variant | Msgpack | Description |
|---------|---------|-------------|
| String  | bytes   | Raw byte string |
| Integer | i64     | 64-bit signed integer |
| Float   | f64     | 64-bit float |
| List    | array   | Ordered list of Values |
| Hash    | map     | Field-value map |
| ZSet    | array   | Sorted set members `[{ member, score }]` |
| Json    | map     | JSON document (nested maps/arrays/scalars) |
| Null    | nil     | Null/missing value |

---

## 11. Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 6379 | TLS 1.3  | Client connections |
| 7379 | mTLS     | Internal replication + Raft (Core-to-Core, Core-to-Keeper) |
| 9090 | HTTP     | Prometheus metrics |

Clients only connect to port 6379.  The mTLS ports (7379) are internal to the
cluster and require the auto-generated CA certificate for authentication.

---

## 12. Reference Implementation

The Rust client library **Pontus** (`mneme-client/`) demonstrates:
- Connection pooling with health checks (`pool.rs`)
- TLS session resumption across pool connections
- Request multiplexing with `req_id` dispatch (`conn.rs`)
- All command methods (`cmd_string.rs`, `cmd_hash.rs`, etc.)
- Multi-address failover for HA clusters
