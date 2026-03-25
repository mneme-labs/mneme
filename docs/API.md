# MnemeCache — Wire Protocol and Command Reference

This document describes the binary wire protocol, command set, error codes, and payload limits for MnemeCache. All communication (client-to-God and God-to-Keeper) uses this protocol over TLS 1.3. The client library (Pontus) handles framing automatically; consult this document when implementing a new client or debugging raw connections.

---

## Wire Protocol Header (16 bytes)

Every request and response begins with a fixed 16-byte header followed by a msgpack-encoded payload.

```
Offset  Size  Field         Description
------  ----  -----------   --------------------------------------------------
0       4     magic         0x4D4E454D  ("MNEM" in ASCII)
4       1     version       Protocol version = 0x01
5       1     cmd_id        Command identifier (see Command Reference below)
6       2     flags         bits 15–4: slot hint | bits 3–2: consistency | bits 1–0: reserved
8       4     payload_len   Length of msgpack payload in bytes (0 if no payload)
12      4     req_id        0 = single-plex, ≥1 = multiplexed (responses out-of-order)
```

### Flags field breakdown

```
Bit:  15  14  13  12  11  10  9   8   7   6   5   4  |  3   2  |  1   0
      [         slot hint (12 bits)               ]  | [cons] | [ rsvd ]
```

- **Slot hint (bits 15–4):** Pre-computed `CRC16(key) % 16384`. The router (Iris) validates and may override. Set to 0 if unknown; Iris will compute.
- **Consistency (bits 3–2):**

| Bits | Value | Meaning |
|------|-------|---------|
| `00` | EVENTUAL | Read replica if available; fire-and-forget writes. AP mode. |
| `01` | QUORUM | `floor(N/2)+1` Keeper ACKs required. CP mode. **Default.** |
| `10` | ALL | Every Keeper must ACK. Highest durability, highest latency. |
| `11` | ONE | First Keeper ACK. Lowest latency durable write. |

- **Reserved (bits 1–0):** Must be zero. Reserved for future use.

### Request multiplexing

When `req_id >= 1`, multiple requests may be outstanding on a single connection simultaneously. Responses are returned out-of-order; match them by `req_id`. The Pontus client library manages a `DashMap<req_id, oneshot::Sender<Response>>` internally.

Use `req_id = 0` only for single-request connections or when strict ordering is required.

---

## Command Reference

### String Commands (0x01 – 0x06)

| Hex  | Command | Request Payload | Response Payload | Notes |
|------|---------|-----------------|------------------|-------|
| `0x01` | GET | `key: bytes` | `value: Value` | Returns `KeyNotFound` if key absent or expired |
| `0x02` | SET | `SetRequest` | `"OK"` | Overwrites existing value and TTL |
| `0x03` | DEL | `DelRequest` | `count: u64` | Bulk delete; count = number of keys that existed |
| `0x04` | EXISTS | `key: bytes` | `bool` | True only if key is present and not expired |
| `0x05` | EXPIRE | `ExpireRequest` | `applied: u64` | 1 if TTL was set, 0 if key not found |
| `0x06` | TTL | `key: bytes` | `seconds: i64` | -1 = no expiry, -2 = key missing |

**Payload schemas:**

```rust
// SET
struct SetRequest {
    key:    bytes,
    value:  bytes,
    ttl_ms: Option<u64>,   // None = no expiry
}

// DEL
struct DelRequest {
    keys: Vec<bytes>,      // max 1,000 keys per request
}

// EXPIRE
struct ExpireRequest {
    key:     bytes,
    seconds: u64,
}
```

---

### Hash Commands (0x10 – 0x13)

| Hex  | Command | Request Payload | Response Payload | Notes |
|------|---------|-----------------|------------------|-------|
| `0x10` | HSET | `HSetRequest` | `added: u64` | Upserts fields; count = new fields only |
| `0x11` | HGET | `HGetRequest` | `value: bytes` | Returns `KeyNotFound` if key or field absent |
| `0x12` | HDEL | `HDelRequest` | `deleted: u64` | Count of fields actually removed |
| `0x13` | HGETALL | `key: bytes` | `Vec<(field: bytes, value: bytes)>` | Returns empty vec if key absent |

**Payload schemas:**

```rust
// HSET
struct HSetRequest {
    key:   bytes,
    pairs: Vec<(field: bytes, value: bytes)>,  // max 65,536 pairs per call
}

// HGET
struct HGetRequest {
    key:   bytes,
    field: bytes,
}

// HDEL
struct HDelRequest {
    key:    bytes,
    fields: Vec<bytes>,
}
```

---

### List Commands (0x20 – 0x24)

| Hex  | Command | Request Payload | Response Payload | Notes |
|------|---------|-----------------|------------------|-------|
| `0x20` | LPUSH | `LPushRequest{key, values: Vec<bytes>}` | `length: u64` | Prepend to list head |
| `0x21` | RPUSH | `RPushRequest{key, values: Vec<bytes>}` | `length: u64` | Append to list tail |
| `0x22` | LPOP | `LPopRequest{key, count: u64}` | `Vec<bytes>` | Pops up to count items from head |
| `0x23` | RPOP | `RPopRequest{key, count: u64}` | `Vec<bytes>` | Pops up to count items from tail |
| `0x24` | LRANGE | `LRangeRequest{key, start: i64, stop: i64}` | `Vec<bytes>` | Negative indexes count from end |

---

### Sorted Set Commands (0x30 – 0x36)

| Hex  | Command | Request Payload | Response Payload | Notes |
|------|---------|-----------------|------------------|-------|
| `0x30` | ZADD | `ZAddRequest{key, members: Vec<(score: f64, member: bytes)>}` | `added: u64` | Upserts; count = new members only |
| `0x31` | ZREM | `ZRemRequest{key, members: Vec<bytes>}` | `removed: u64` | |
| `0x32` | ZSCORE | `ZScoreRequest{key, member: bytes}` | `score: f64` | `KeyNotFound` if member absent |
| `0x33` | ZRANK | `ZRankRequest{key, member: bytes}` | `rank: u64` | 0-indexed ascending rank |
| `0x34` | ZRANGE | `ZRangeRequest{key, start: i64, stop: i64, withscores: bool}` | `Vec<bytes>` or `Vec<(bytes, f64)>` | |
| `0x35` | ZRANGEBYSCORE | `ZRangeByScoreRequest{key, min: f64, max: f64, limit: Option<u64>}` | `Vec<bytes>` | |
| `0x36` | ZCARD | `key: bytes` | `count: u64` | |

---

### Counter Commands (0x40 – 0x45)

| Hex  | Command | Request Payload | Response Payload | Notes |
|------|---------|-----------------|------------------|-------|
| `0x40` | INCR | `key: bytes` | `value: i64` | Atomically increment by 1; creates key at 0 if absent |
| `0x41` | INCRBY | `IncrByRequest{key, delta: i64}` | `value: i64` | |
| `0x42` | DECR | `key: bytes` | `value: i64` | |
| `0x43` | DECRBY | `DecrByRequest{key, delta: i64}` | `value: i64` | |
| `0x44` | GETSET | `GetSetRequest{key, value: bytes}` | `old_value: bytes \| null` | Atomic swap |
| `0x45` | SETNX | `SetNxRequest{key, value: bytes, ttl_ms: Option<u64>}` | `set: bool` | Set only if key absent |

---

### JSON Commands (0x50 – 0x56)

JSON values are stored as opaque bytes internally. Path selectors use JSONPath syntax.

| Hex  | Command | Request Payload | Response Payload | Notes |
|------|---------|-----------------|------------------|-------|
| `0x50` | JSON.SET | `JsonSetRequest{key, path: String, value: bytes}` | `"OK"` | |
| `0x51` | JSON.GET | `JsonGetRequest{key, paths: Vec<String>}` | `bytes` | |
| `0x52` | JSON.DEL | `JsonDelRequest{key, path: String}` | `deleted: u64` | |
| `0x53` | JSON.ARRAPPEND | `JsonArrAppendRequest{key, path: String, values: Vec<bytes>}` | `length: u64` | |
| `0x54` | JSON.NUMINCRBY | `JsonNumIncrRequest{key, path: String, delta: f64}` | `value: f64` | |
| `0x55` | JSON.TYPE | `JsonTypeRequest{key, path: String}` | `type_name: String` | "string", "number", "object", "array", "boolean", "null" |
| `0x56` | JSON.MGET | `JsonMGetRequest{keys: Vec<bytes>, path: String}` | `Vec<bytes \| null>` | Bulk get same path from multiple keys |

---

### Auth Commands (0x60 – 0x61)

Auth commands must be sent before any other command on a new connection when authentication is enabled.

| Hex  | Command | Request Payload | Response Payload | Notes |
|------|---------|-----------------|------------------|-------|
| `0x60` | AUTH | `token: String` OR `(username: String, password: String)` | `"OK"` (token auth) or `token: String` (credential auth) | Credential auth returns a fresh session token |
| `0x61` | REVOKE_TOKEN | `token: String` | `"OK"` | Admin-only. Immediately invalidates the token. |

**Token auth flow:** Pass a previously issued token string. Argus validates the HMAC-SHA256 signature against `cluster_secret` and checks the revocation list. Validation is ~75ns.

**Credential auth flow:** Pass username and password. Argus checks PBKDF2 hash in `users.db`. On success, a new token is issued and returned. Store this token; do not store the password.

---

### Admin Commands (0x80 – 0x85)

Admin commands require a token with `is_admin = true`.

| Hex  | Command | Request Payload | Response Payload | Notes |
|------|---------|-----------------|------------------|-------|
| `0x80` | CONFIG | `ConfigRequest{action: "GET"\|"SET", key: String, value: Option<String>}` | `value: String` or `"OK"` | Live config: `memory.pool_bytes`, `lethe.eviction_threshold`, etc. |
| `0x81` | CLUSTER_INFO | `(empty)` | `ClusterInfo` struct | Cluster term, leader, live node count, pool sizes |
| `0x82` | CLUSTER_SLOTS | `(empty)` | `Vec<SlotRange{start, end, keeper_id, addr}>` | Current Iris slot assignment |
| `0x83` | KEEPER_LIST | `(empty)` | `Vec<KeeperInfo{id, addr, grant, state, connected, lag_ms}>` | |
| `0x84` | POOL_STATS | `(empty)` | `PoolStats{used, max, pressure_ratio, in_flight, keeper_stats}` | |
| `0x85` | WAIT | `WaitRequest{numreplicas: u64, timeout_ms: u64}` | `acked: u64` | Block until N Keepers ACK last write or timeout |

---

### Internal / Replication Commands (0xA0 – 0xA6)

These commands are used exclusively on the replication port (7379) between God and Keepers. Clients must not send these; a `ProtocolViolation` error will be returned if they appear on the client port.

| Hex  | Command | Direction | Description |
|------|---------|-----------|-------------|
| `0xA0` | SYNC_START | Keeper → God | Initiate sync session after Keeper restart or first join |
| `0xA1` | PUSH_KEY | God → Keeper | Replicate a single key-value pair with TTL |
| `0xA2` | HEARTBEAT | God ↔ Keeper | Bidirectional keepalive; carries lag measurement |
| `0xA3` | MOVED | God → Client | Redirect client to correct node for a slot |
| `0xA4` | ACK_WRITE | Keeper → God | Acknowledge a replicated write for QUORUM/ALL counting |
| `0xA5` | SYNC_REQUEST | Keeper → God | Request delta sync from a given LSN |
| `0xA6` | SYNC_COMPLETE | Keeper → God | Signal that warm-up is done; carries `key_count: u64` |

**PUSH_KEY payload:**

```rust
struct PushKey {
    key:       bytes,
    value:     bytes,
    expiry_at: u64,   // unix ms; 0 = no expiry
    slot:      u16,
}
```

TTL is replicated as an absolute `expiry_at` timestamp so that keys expired during Keeper downtime are deleted immediately on push rather than serving stale data.

**SYNC_COMPLETE payload:**

```rust
struct SyncComplete {
    keeper_id: u32,
    key_count: u64,   // total keys pushed; God uses this to detect sync completion
}
```

---

## Response Envelope

Every response carries a one-byte response code prefix followed by the msgpack payload.

| Code | Value | Meaning |
|------|-------|---------|
| `0xF0` | OK | Command succeeded; payload follows |
| `0xF1` | Error | Command failed; payload is `MnemeError` (see below) |

The `req_id` in the response header matches the `req_id` of the originating request. The `cmd_id` in the response echoes the request command.

---

## Error Codes

All errors are returned as msgpack-encoded `MnemeError` variants with `0xF1` response code. Errors never cause a panic; the connection remains open and can accept further requests.

| Error Variant | Description |
|---|---|
| `KeyNotFound` | The requested key does not exist or has expired |
| `WrongType` | Operation not valid for the key's type (e.g., HGET on a String key) |
| `TokenExpired` | Session token has passed its TTL |
| `TokenInvalid` | Token HMAC signature verification failed |
| `TokenRevoked` | Token has been explicitly revoked via REVOKE_TOKEN |
| `MaxConnectionsReached` | `max_total` (100,000) or `max_per_ip` (1,000) limit hit; connection rejected before TLS |
| `RequestTimeout` | Request exceeded `request_timeout` (5,000ms) deadline |
| `SlotMoved { slot, addr }` | Key's slot has migrated; retry the request at the given address |
| `QuorumNotReached { got, need }` | Fewer Keepers ACKed than required; write not committed |
| `OutOfMemory` | Logical pool is full; Lethe eviction could not free sufficient space |
| `KeeperUnreachable` | Hermes could not deliver a frame to one or more required Keepers |
| `WalWriteFailed` | Aoide failed to write or fsync the WAL segment |
| `SnapshotFailed` | Melete snapshot write failed (e.g., disk full) |
| `ProtocolViolation` | Malformed header, wrong magic, or internal command on client port |
| `UnknownCommand` | `cmd_id` not recognized |
| `PayloadTooLarge` | Payload exceeds a size limit (see Payload Limits) |

### SlotMoved handling

When a client receives `SlotMoved { slot, addr }`, it must:

1. Update its local slot map for the affected slot.
2. Retry the exact same request at the returned `addr`.
3. Not retry more than once for the same request; if the second attempt also returns `SlotMoved`, there is a cluster reconfiguration in progress — back off and retry with exponential delay.

The Pontus client library handles this automatically.

---

## Payload Limits

These limits are enforced at the Charon connection layer, before the request reaches Mnemosyne. Requests that exceed any limit receive `PayloadTooLarge` immediately.

| Parameter | Limit |
|---|---|
| Max key size | 512 bytes |
| Max value size | 10 MiB (10,485,760 bytes) |
| Max hash fields per key | 65,536 |
| Max keys per batch request (DEL, JSON.MGET, etc.) | 1,000 |
| Max header `payload_len` | 10,485,760 (enforced before read) |

Keys exceeding 512 bytes are rejected regardless of the value size. The `payload_len` field in the header is checked before the payload is read from the socket; an oversized frame does not consume memory.

---

## Connection Lifecycle

```
Client                           Charon (God)
  |                                  |
  |-- TCP SYN ---------------------->|  (rejected here if max_total reached)
  |<- TCP SYN-ACK ------------------|
  |-- TLS ClientHello -------------->|
  |<- TLS ServerHello + cert --------|  (TLS 1.3, rustls, cert from Aegis)
  |-- [TLS established] ------------>|
  |-- AUTH (0x60, req_id=1) -------->|
  |<- 0xF0 OK / token (req_id=1) ---|
  |                                  |
  |-- GET  (0x01, req_id=2) -------->|  ← multiplexed requests start here
  |-- SET  (0x02, req_id=3) -------->|
  |<- 0xF0 value   (req_id=3) ------|  ← responses may arrive out of order
  |<- 0xF0 "OK"    (req_id=2) ------|
  |                                  |
  |-- [idle > 30s] ------------------|  (Charon closes with idle_timeout)
```

TCP keepalive is set to 10 seconds (`tcp_keepalive_s`) to detect half-open connections. Idle connections without traffic are closed after 30 seconds (`idle_timeout_s`).
