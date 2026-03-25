# MnemeCache — Component Reference

Every internal subsystem is named after a figure from Greek mythology. This document explains what each component does, why it was named that way, and how it fits into the architecture.

---

## Core layer — mneme-core

### Mnemosyne (Μνημοσύνη) — `src/core/mnemosyne.rs`

**What:** The primary God node. Holds the entire working data set in RAM via a memory-mapped pool. Serves every client request directly — no disk I/O ever touches this path.

**Why the name:** Mnemosyne is the Titaness of memory and the mother of the nine Muses in Greek mythology. She literally *is* memory.

**Key facts:**
- Owns the `RobinHoodMap` key → Entry store, sharded by slot
- Dispatches all 40+ command types (String, Hash, List, ZSet, Counter, JSON, Auth, Admin, Cluster)
- Coordinates with Moirai for consistency fan-out to Keepers
- Delegates eviction to Lethe and slot routing to Iris
- Never calls `fsync`, never writes to disk

---

### Lethe (Λήθη) — `src/core/lethe.rs`

**What:** The eviction engine. Runs a 3-level TTL wheel for time-based expiry and a Morris probabilistic LFU counter for memory-pressure eviction.

**Why the name:** In Greek mythology, Lethe is the river of forgetfulness in the underworld. Souls who drank from it forgot their earthly lives — the cache forgets cold keys.

**Key facts:**
- TTL wheel: level 0 = 256 buckets × 10 ms, level 1 = 64 × 1 s, level 2 = 64 × 60 s, level 3 = 64 × 3600 s
- LFU counter: Morris 8-bit probabilistic — O(1) update, 1 byte per key, ~95% accuracy vs exact LFU
- Eviction ≠ expiry: eviction pushes cold keys to Oneiros (reversible); expiry is a permanent delete
- Fires automatically when pool pressure exceeds `eviction_threshold` (default: 90%)

---

### Iris (Ἶρις) — `src/core/iris.rs`

**What:** The slot router. Maps every key to one of 16,384 hash slots using CRC16-CCITT and distributes slots across Keepers.

**Why the name:** Iris is the goddess of the rainbow and the messenger between the gods — she bridges disparate places, just as Iris routes requests to the right node.

**Key facts:**
- `slot = crc16(key) % 16384` (same algorithm as Redis Cluster)
- Hash tags: `{user:1}.score` uses only `user:1` for slot calculation, co-locating related keys
- Slot → Keeper mapping is stored in the Iris table and updated by Herold on join/leave
- Generates `SlotMoved` errors when a key has been migrated (client must retry at new address)

---

### Moirai (Μοῖραι) — `src/core/moirai.rs`

**What:** The consistency dispatcher. Intercepts every write and fans it out to the required number of Keepers based on the request's consistency level.

**Why the name:** The Moirai (Fates) are the three goddesses who controlled the destiny of every being — they decide what must happen. Moirai decides how many confirmations are required.

**Key facts:**
- Consistency levels: EVENTUAL (fire-and-forget), ONE (first Keeper ACK), QUORUM (⌊N/2⌋+1 ACKs, default), ALL (every Keeper)
- In solo mode: QUORUM and ALL are silently downgraded to EVENTUAL
- Uses `tokio::sync::Semaphore` to bound in-flight replication frames
- Returns `QuorumNotReached { got, need }` if not enough Keepers respond in time

---

## Keeper layer — mneme-keeper

### Hypnos (Ὕπνος) — `src/keeper/hypnos.rs`

**What:** The Keeper node. Wires together all persistence subsystems (Aoide, Melete, Oneiros) and serves as the replication endpoint for Mnemosyne.

**Why the name:** Hypnos is the god of sleep — he holds the world in a state of suspended animation, ready to be recalled. The Keeper holds data in "cold sleep" on disk, ready to revive the Core after a crash.

**Key facts:**
- Accepts replication frames from Mnemosyne over mTLS port 7379
- On startup: replays Melete snapshot then replays Aoide WAL, sends SYNC_START to Core
- Responds to PushKey frames with AckWrite acknowledgements
- Two modes: standalone TCP listener (cluster) or embedded in-process (solo mode)

---

### Aoide (Ἀοιδή) — `src/keeper/aoide.rs`

**What:** The Write-Ahead Log (WAL). Appends every replicated write to a binary log with O_DIRECT + fallocate for maximum durability without the fsync jitter of the Core.

**Why the name:** Aoide is one of the three original Muses — the Muse of song. A WAL is a sequential song of writes, each entry following the last.

**Key facts:**
- O_DIRECT writes bypass the page cache — data is durable on NVMe the moment `write()` returns
- fallocate pre-allocates disk space to avoid fragmentation and metadata updates mid-write
- Each entry: `seq_u64 | key_len_u32 | key | msgpack(Value)`
- WAL rotation: triggered when size exceeds `wal_max_mb` (default 256 MB); old segments compressed with zstd
- `Aoide::replay()` is used during startup to rebuild cold store state

---

### Melete (Μελέτη) — `src/keeper/melete.rs`

**What:** The snapshot engine. Periodically serializes the entire cold store to an atomic msgpack file.

**Why the name:** Melete is one of the three original Muses — the Muse of practice and meditation. Taking a snapshot is a periodic, disciplined practice that prepares for recovery.

**Key facts:**
- Snapshots are written atomically: write to `.snap.tmp`, then rename — no partial snapshots
- Format: msgpack array of `(key_bytes, Value, expires_at_ms, slot)` tuples
- Default interval: every 60 seconds (`snapshot_interval_s`)
- On Keeper restart: snapshot is loaded first, then WAL is replayed on top — WAL records newer than the snapshot take precedence

---

### Oneiros (Ὄνειρος) — `src/keeper/oneiros.rs`

**What:** The cold data store. Persists key → value pairs in a redb B-tree. Serves cold fetches when Mnemosyne needs to retrieve data that has been evicted from RAM.

**Why the name:** Oneiros is the personification of dreams — the realm between sleep (Hypnos) and waking. Cold data is dormant but can be retrieved, like a memory surfacing from a dream.

**Key facts:**
- Backed by `redb` — pure Rust, ACID-compliant, B-tree on NVMe
- Stores `(key, msgpack(Value), expires_at_ms, slot)` per row
- `scan()` is used by snapshot tasks to iterate all live keys
- Expired keys (where `now_ms >= expires_at_ms`) are filtered out on read and lazily deleted on scan
- Cold fetch latency: ~1.2 ms p99 (Hermes round-trip + redb read)

---

## Network layer — mneme-core

### Hermes (Ἑρμῆς) — `src/net/hermes.rs`

**What:** The replication fabric. Manages one persistent mTLS connection per Keeper, multiplexes all replication frames over it, and uses io_uring fixed buffers for zero-copy I/O.

**Why the name:** Hermes is the messenger of the gods — he carries information between realms at divine speed. Hermes carries replication frames from Mnemosyne to Keepers.

**Key facts:**
- One connection per Keeper (not a pool) — multiplexing via `req_id` replaces connection pooling
- `pending: DashMap<req_id, oneshot::Sender<Response>>` for out-of-order response dispatch
- io_uring fixed buffers: registered once, reused for every frame — avoids per-frame kernel transitions
- Reconnects automatically on disconnect with exponential back-off
- Tracks `replication_lag_ms` per Keeper for Aletheia metrics

---

### Charon (Χάρων) — `src/net/charon.rs`

**What:** The connection manager. Accepts client connections, enforces per-IP and global limits, and manages idle timeouts and backpressure.

**Why the name:** Charon is the ferryman of the dead who transports souls across the River Styx — he controls who may cross. Charon decides which clients may enter.

**Key facts:**
- Rejects connections *before TLS* if `max_total` (100,000) is reached — saves TLS handshake cost
- Per-IP limit: 1,000 connections (configurable via `connections.max_per_ip`)
- Idle timeout: 30 seconds, TCP keepalive: 10 seconds
- Backpressure: `tokio::sync::Semaphore` with `max_in_flight = 200,000` requests
- Request timeout: 5,000 ms per request

---

### Aegis (Αἰγίς) — `src/net/aegis.rs`

**What:** The TLS layer. Provides TLS 1.3 for client connections and mTLS for replication/inter-node communication. Built on rustls — no OpenSSL.

**Why the name:** The Aegis is the divine shield of Zeus and Athena — impenetrable protection. Aegis protects all network communication.

**Key facts:**
- rustls only — no C FFI, no OpenSSL, no legacy cipher suites
- TLS 1.3 exclusively for clients (port 6379)
- mTLS (mutual authentication) for replication (port 7379) — both sides present certificates
- Certificates auto-generated by `rcgen` on first boot; CA cert shared across cluster
- Custom CA supported: set `auto_generate = false` and provide `cert`, `key`, `ca_cert`

---

## Cluster layer — mneme-core

### Themis (Θέμις) — `src/cluster/themis.rs`

**What:** The Raft consensus engine. Manages leader election across Core nodes and coordinates failover when a Core becomes unreachable.

**Why the name:** Themis is the Titaness of law, order, and divine justice. She embodies the orderly rules that govern society — just as Raft enforces the rules of distributed consensus.

**Key facts:**
- Raft implementation via `openraft`
- Election timeout: randomized 1,500–3,000 ms (configurable via `cluster.election_min_ms` / `election_max_ms`)
- Heartbeat interval: 500 ms
- On Core failure: Keepers elect a new leader within one election timeout period (~1.5–3 s)
- Tiebreak: node with the highest WAL sequence number wins

---

### Herold (Ἥρωλδ) — `src/cluster/herold.rs`

**What:** The node registration daemon. Handles zero-friction joining of new Keepers and read replicas without manual certificate distribution.

**Why the name:** A herald is the official announcer who introduces new arrivals to the court — Herold introduces new nodes to the cluster.

**Key facts:**
- New node generates its own mTLS cert via `rcgen`
- Sends `REGISTER { node_id, role, grant_bytes }` frame to Core with the cluster join token
- Core verifies token, registers node in Themis, and logical pool grows immediately
- State progression: `CONNECTING → SYNCING → SYNCED`
- Read replicas send a `SYNC_REQUEST` to stream the full slot table from Core

---

## Auth layer — mneme-core

### Argus (Ἄργος) — `src/auth/argus.rs`

**What:** The session token manager. Issues HMAC-SHA256 signed tokens, validates them in ~75 ns, and maintains an in-memory revocation blacklist.

**Why the name:** Argus Panoptes was a hundred-eyed giant — he saw everything and never slept. Argus watches every request and verifies the identity of every caller.

**Key facts:**
- Token format: `base64url(claims_msgpack) + "." + base64url(hmac_sha256_sig)`
- Claims: `{ user_id, exp, iat, jti }` — jti (JWT ID) is a random u64 used as blacklist key
- Validation: ~75 ns (HMAC-SHA256 in hardware via AES-NI)
- Password hashing: PBKDF2-SHA256 with 100,000 iterations (NIST SP 800-132 compliant)
- Blacklist is bounded — oldest entries pruned when `max_blacklist` is exceeded
- Token TTL: configurable via `auth.token_ttl_h` (default: 24 hours)

---

## Observability layer — mneme-core

### Aletheia (Ἀλήθεια) — `src/obs/aletheia.rs`

**What:** The metrics engine. Exposes Prometheus-format metrics over HTTP (port 9090), including both software metrics and hardware performance counters via `perf_event_open`.

**Why the name:** Aletheia is the goddess of truth and sincerity — she reveals what is real. Aletheia reveals the true state of the system without distortion.

**Key facts:**
- Prometheus metrics at `http://<node>:9090/metrics`
- Software metrics: pool usage, command latency histograms, eviction counts, replication lag, connection counts
- Hardware metrics (via `perf_event_open`): L1/L2/L3 cache misses, TLB misses, branch mispredictions, CPU cycles, instructions
- Alert thresholds: pool pressure > 0.85, keeper disconnected, replica lag > 50 ms

---

### Delphi (Δελφοί) — `src/obs/delphi.rs`

**What:** The slow-query log and command monitor. Records commands that exceed the slowlog threshold and optionally streams all commands in real time.

**Why the name:** The Oracle of Delphi saw all things past and present — Delphi observes all commands, surfacing the slow ones for inspection.

**Key facts:**
- SLOWLOG: ring buffer of configurable size (default 512 entries); threshold default 1,000 µs
- Each entry: timestamp, command type, key, duration, client address
- MONITOR mode: streams every command to connected observers in real time (similar to Redis MONITOR)
- Runtime configuration: `config observability.slowlog_threshold_us=500`

---

## Error types — mneme-common

### Nemesis (Νέμεσις) — `src/error.rs`

**What:** The canonical error type for the entire codebase. Every subsystem returns `MnemeError`; wire error codes are mapped from it.

**Why the name:** Nemesis is the goddess of retribution and righteous anger — she delivers consequences for transgressions. Nemesis surfaces the consequences of bad requests.

**Key variants:**

| Variant | Wire code | When raised |
|---|---|---|
| `KeyNotFound` | 0x01 | GET on a missing key |
| `WrongType { expected, got }` | 0x02 | Type mismatch (HGET on a String, etc.) |
| `TokenExpired` | 0x10 | Auth token past its `exp` claim |
| `TokenInvalid` | 0x11 | HMAC signature mismatch |
| `TokenRevoked` | 0x12 | Token jti is in the blacklist |
| `MaxConnectionsReached` | 0x20 | Charon global or per-IP limit hit |
| `RequestTimeout` | 0x21 | Request exceeded 5,000 ms deadline |
| `SlotMoved { slot, addr }` | 0x30 | Key's slot has been migrated; retry at `addr` |
| `QuorumNotReached { got, need }` | 0x31 | Fewer Keeper ACKs than required |
| `OutOfMemory` | 0x40 | Pool full and eviction cannot free enough space |
| `KeeperUnreachable` | 0x41 | No route to Keeper node |
| `WalWriteFailed` | 0x50 | Aoide fsync or write error |
| `SnapshotFailed` | 0x51 | Melete write error |
| `PayloadTooLarge` | 0x60 | Key > 512 B or value > 10 MB |
| `ProtocolViolation` | 0x61 | Malformed frame header or payload |
| `UnknownCommand` | 0x62 | Unrecognized `cmd_id` byte |

---

## Client library — mneme-client (Pontus)

### Pontus (Πόντος) — `mneme-client/src/`

**What:** The official Rust client library. Provides a connection pool and a multiplexed connection type for issuing parallel requests over a single TLS connection.

**Why the name:** Pontus is the ancient god of the sea — a vast, traversable medium that connects all shores. Pontus connects application code to the MnemeCache cluster.

**Key types:**
- `MnemePool` — manages a pool of idle `MnemeConn` instances with health-check background task
- `MnemeConn` — single multiplexed TLS connection; parallel requests, out-of-order responses via `req_id`
- `Consistency` — re-export of `ConsistencyLevel` for setting per-request consistency level
- `ClientError` — error type for pool and connection failures

---

## Summary table

| Name | Myth | Layer | Role |
|---|---|---|---|
| **Mnemosyne** | Titaness of memory | Core | Pure RAM data store, all client commands |
| **Lethe** | River of forgetfulness | Core | TTL wheel + LFU Morris eviction |
| **Iris** | Goddess of rainbows / messenger | Core | CRC16 slot routing, key → Keeper mapping |
| **Moirai** | The three Fates | Core | Consistency dispatcher (EVENTUAL/ONE/QUORUM/ALL) |
| **Hermes** | Messenger god | Net | mTLS replication fabric, io_uring multiplexed |
| **Charon** | Ferryman of the dead | Net | Connection manager, backpressure, limits |
| **Aegis** | Divine shield | Net | TLS 1.3 + mTLS, rustls only |
| **Themis** | Titaness of law | Cluster | Raft leader election, failover |
| **Herold** | Herald / announcer | Cluster | Zero-friction node registration |
| **Argus** | Hundred-eyed giant | Auth | HMAC-SHA256 tokens, PBKDF2 passwords |
| **Aletheia** | Goddess of truth | Obs | Prometheus metrics + hardware counters |
| **Delphi** | Oracle of Delphi | Obs | SLOWLOG + MONITOR |
| **Nemesis** | Goddess of retribution | Common | Canonical error types + wire codes |
| **Hypnos** | God of sleep | Keeper | Keeper node, wires persistence subsystems |
| **Aoide** | Muse of song | Keeper | Write-Ahead Log, O_DIRECT + fallocate |
| **Melete** | Muse of practice | Keeper | Atomic msgpack snapshots |
| **Oneiros** | God of dreams | Keeper | Cold store, redb B-tree |
| **Pontus** | Ancient god of the sea | Client | Rust client library, connection pool |
