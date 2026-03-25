# Building MnemeCache Clients

This guide explains how to implement a MnemeCache client library in any language.

---

## Wire Protocol Reference

See **[CLIENT_PROTOCOL.md](../CLIENT_PROTOCOL.md)** for the complete specification:

- 16-byte binary frame format (magic, version, CmdId, flags, payload length, req_id)
- All command IDs with msgpack payload schemas
- Authentication flow (HMAC-SHA256 tokens)
- Request multiplexing (parallel requests on a single connection)
- Consistency levels (EVENTUAL, ONE, QUORUM, ALL)
- Leader redirect protocol for HA failover
- Error codes and their meanings
- Connection lifecycle (TLS 1.3, keepalive, reconnection)
- Payload limits

---

## Reference Implementation

The official Rust client **Pontus** (`mneme-client/`) demonstrates all features:

| File | What it shows |
|------|---------------|
| `pool.rs` | Connection pooling, health checks, multi-address failover |
| `conn.rs` | Request multiplexing with `req_id` dispatch |
| `cmd_string.rs` | String commands (GET, SET, DEL, etc.) |
| `cmd_hash.rs` | Hash commands (HSET, HGET, HGETALL, etc.) |
| `cmd_list.rs` | List commands (LPUSH, RPUSH, LPOP, etc.) |
| `cmd_zset.rs` | Sorted set commands (ZADD, ZRANGE, etc.) |
| `cmd_json.rs` | JSON document commands |

---

## Implementation Checklist

A minimal client needs:

1. **TLS 1.3 connection** to port 6379 (skip cert verify for dev, use CA for production)
2. **Frame encoder/decoder** — 16-byte header + msgpack payload
3. **AUTH command** — send token as the first command after TLS handshake
4. **Basic commands** — GET, SET, DEL at minimum
5. **Error handling** — check CmdId in response (0xF0 = OK, 0xF1 = Error)

A full-featured client adds:

6. **Request multiplexing** — allocate unique `req_id` per request, match responses
7. **Connection pooling** — reuse TLS connections, health checks
8. **Consistency levels** — encode in flags bits 3-2
9. **Leader redirect** — handle CmdId 0xB3, reconnect to leader address
10. **Bulk operations** — MGET, MSET for batch efficiency
11. **All data types** — Hash, List, Sorted Set, JSON, Counters

---

## Dependencies You Will Need

| Capability | Common libraries |
|------------|------------------|
| TLS 1.3 | OpenSSL, BoringSSL, native TLS (Go, Java built-in) |
| MessagePack | msgpack (Python), @msgpack/msgpack (Node), msgpack-go, msgpack-java |
| TCP sockets | Standard library in all languages |

---

## Quick Test

After implementing your client, verify against a running MnemeCache instance:

```
1. Connect with TLS to 127.0.0.1:6379
2. AUTH with a valid token
3. SET "test-key" → "hello"
4. GET "test-key" → expect "hello"
5. DEL "test-key" → expect 1
6. GET "test-key" → expect KeyNotFound error
```

---

## Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 6379 | TLS 1.3 | Client connections (connect here) |
| 7379 | mTLS | Internal replication (do not expose to clients) |
| 9090 | HTTP | Prometheus metrics |
