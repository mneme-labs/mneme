# MnemeCache — Database Design Decisions

This document captures the architectural decisions behind MnemeCache's data model, security design, consistency model, and recovery strategy. Each section explains the chosen approach and the tradeoffs that motivated it.

---

## 1. Architecture Overview

MnemeCache is built around a strict two-tier separation between hot data and durable data.

### The God/Keeper split

**God node (Mnemosyne)** holds the entire working set in RAM using a `RobinHoodMap` with ahash. It never touches disk on the hot path. Every read from the hot tier completes within the p99 < 150µs target because there is no I/O in the critical path: the only work is a hash lookup, an optional TTL check via the Lethe wheel, and a response frame.

**Keeper nodes (Hypnos + Aoide + Melete + Oneiros)** hold durable copies. Writes are replicated asynchronously from God to Keepers over Hermes (mTLS, io_uring fixed buffers). The Keeper's WAL (Aoide) uses `O_DIRECT + fallocate` to bypass page cache and achieve predictable fsync latency. Snapshots (Melete) compress with zstd and write atomically via rename.

### Why disk I/O cannot be on the hot path

The performance budget for a `GET QUORUM` is p99 < 800µs. A single `fsync` on a modern NVMe drive is 100–400µs. A synchronous disk write on the God node would consume more than half the entire budget before network RTT is even counted, making the target unreachable on any hardware. The God/Keeper separation is not an optimization; it is a prerequisite for the latency targets.

Cold data that has been evicted from RAM to Oneiros (redb B-tree) is served at p99 < 1.2ms, which is a deliberately degraded path. Lethe's LFU Morris counter tracks access frequency so that genuinely cold keys are the ones pushed to disk, not recently accessed ones.

---

## 2. Transparent Key Prefixing for Database Namespacing (D-01)

### The design

Database namespacing is implemented by prepending a database name to every key before it reaches the storage layer:

```
db_key(db, raw_key)  =  db_name + '\x00' + raw_key
```

For example, a key `"session:abc123"` in database `"app_prod"` is stored internally as `"app_prod\x00session:abc123"`.

### Why not a separate namespace field in the frame header

An alternative would be to add a `db_id` or `db_name` field to the 16-byte protocol header. This was considered and rejected for the following reasons:

1. **Zero protocol overhead in storage.** The storage layers — Mnemosyne's RobinHoodMap, Lethe's eviction counters, Iris's slot router, and Oneiros's redb B-tree — are completely namespace-unaware. They receive a single opaque `bytes` key and treat it uniformly. No code path in the hot tier branches on a database identifier. Adding a `db_id` field to the header would require every storage operation to carry and inspect a second key component, adding conditional logic on the hot path.

2. **The `'\x00'` separator is never valid in database names.** Database names are validated as UTF-8 strings at the protocol boundary. UTF-8 multi-byte sequences never contain `0x00` bytes, so `'\x00'` is unambiguous as a separator: it cannot appear in the database name component and therefore the original `raw_key` can always be recovered by splitting on the first `'\x00'`.

3. **SCAN scopes naturally by prefix match.** A `SCAN db_name\x00pattern*` in the storage layer returns only keys belonging to the named database without any additional filter layer. This is a natural consequence of sorted key ordering in redb and prefix iteration in the hot-tier map.

4. **Simplicity of the implementation.** The prefix is applied once in a thin `db_key()` function at the Charon boundary before the key enters any storage structure. There is no "database context" that needs to be threaded through the call stack.

### Tradeoff

The only cost is slightly longer keys in storage. A 20-byte database name adds 21 bytes (name + separator) to every key. At 512 bytes maximum key size, the effective raw key budget becomes `512 - len(db_name) - 1` bytes. This is documented in the client library (Pontus) so that users can account for it when choosing database names.

---

## 3. RBAC with Glob Patterns (D-02)

### The design

Access control is expressed as a set of glob patterns on database names, not a per-key ACL:

```toml
[[user]]
username = "analytics"
password_hash = "..."
databases = ["analytics_*", "reporting"]
is_admin = false
```

A user with `databases = ["analytics_*"]` can read and write any database whose name matches that glob pattern. Admins (`is_admin = true`) bypass all database checks.

### Why glob patterns on database names rather than per-key ACLs

1. **Tenancy model alignment.** In practice, isolation boundaries are at the database level, not the key level. A microservice owns a database; it does not own individual keys within a shared database. Per-key ACLs would require every key to carry an owner annotation, making multi-tenant setups complex for operators and application developers.

2. **Wildcards match tenant prefix schemes.** A common deployment pattern is `tenant_{id}_cache`, `tenant_{id}_sessions`. A single grant `"tenant_*"` covers all databases for all tenants without requiring per-tenant config changes. Per-key ACLs cannot express this naturally.

3. **Per-key ACLs are incompatible with the 150µs budget.** A per-key ACL check would require a lookup against an ACL store on every GET and SET. Even a DashMap lookup at ~200ns would add 0.1µs per operation and, more critically, would require the ACL store to be kept in sync with key mutations. At high key churn this becomes a maintenance and consistency burden.

4. **Glob matching is O(1) per token.** A token encodes the user's database grants as a list of glob patterns in the HMAC-signed payload. At request time, Argus matches the incoming `db_name` against the token's pattern list. This is a small bounded loop (typically 1–3 patterns) with no external lookup.

5. **Admin tokens are unconditionally trusted.** A token with `is_admin = true` skips the glob check entirely. The admin flag is embedded in the token payload and covered by the HMAC signature, so it cannot be forged.

---

## 4. Token Design

### Structure

Tokens are HMAC-SHA256 signed blobs containing:

```rust
struct TokenClaims {
    jti:        String,   // JWT ID — CSPRNG bytes, base64url encoded
    username:   String,
    is_admin:   bool,
    databases:  Vec<String>,   // glob patterns
    issued_at:  u64,           // unix seconds
    expires_at: u64,           // unix seconds
}
```

The token is serialized with msgpack, then signed: `token = base64url(claims) + "." + base64url(HMAC-SHA256(cluster_secret, claims))`.

### Why embed claims in the token

Embedding `is_admin`, `username`, and `databases` in the token payload means Argus can validate a token and resolve all permissions in a single HMAC verification (~75ns). There is no per-request lookup against a users database. The only state Argus needs at runtime is `cluster_secret` (a static config value) and the revocation list.

### JTI and unguessability

The `jti` field is generated from `getrandom` (CSPRNG). Even if an attacker can observe token traffic, they cannot predict or enumerate valid tokens. The `jti` is also the revocation key: REVOKE_TOKEN stores the `jti` in Argus's revocation set.

### Revocation

Argus maintains a revocation set (bloom filter for fast negatives + explicit list for confirmed revocations). A `REVOKE_TOKEN` command adds the `jti` to this set. Every subsequent validation of a token with that `jti` returns `TokenRevoked`. The revocation set is replicated to Keepers via the replication channel so that Keepers can enforce revocations independently.

### Tradeoff: admin promotion requires token re-issue

Because permissions are embedded in the token, promoting a user to admin requires them to re-authenticate to receive a new token. The old non-admin token remains valid until it expires (or is revoked). This is acceptable: admin promotion is rare, and operators can explicitly revoke the old token with `REVOKE_TOKEN` when promoting a user.

---

## 5. Why Passwords Are Never Stored

### Storage: PBKDF2 hashes only

`users.db` stores `PBKDF2-HMAC-SHA256(password, salt, 600_000_iterations)`. The plaintext password is never written anywhere. If `users.db` is stolen, an attacker obtains only the PBKDF2 hash, which requires 600,000 SHA-256 operations per guess — computationally expensive for offline brute-force.

### Auth tokens are not password equivalents

An issued token differs from a password in three important ways:

1. **Tokens expire.** The default TTL is configurable (default 24 hours). A leaked token becomes useless after expiry without any operator action.
2. **Tokens are revocable.** A single `REVOKE_TOKEN` call immediately invalidates a token cluster-wide. A leaked password requires a password change flow.
3. **Tokens carry bounded permissions.** A token for user `analytics` can only access databases matching `analytics_*`. Even if the token is leaked, the attacker's blast radius is limited to those databases.

### CLI never stores passwords

The `mneme-cli` tool never writes a password to disk. The `profile refresh-token` command prompts interactively using `rpassword` (which disables terminal echo) and discards the password from memory after the auth request completes. The CLI stores the resulting token in the profile config file (`~/.config/mneme/profiles.toml`), not the password.

---

## 6. Consistency Model

MnemeCache offers four consistency levels selectable per-request via the flags field.

### EVENTUAL (AP mode)

Writes are fire-and-forget: God updates its in-memory state and sends replication frames to Keepers without waiting for ACKs. Reads may be served from a read-replica God node, which may be slightly behind the master.

Use EVENTUAL for: cache reads, session lookups, rate-limit counters where approximate values are acceptable.

### QUORUM (CP mode, default)

`floor(N/2)+1` Keepers must ACK the write before God responds to the client. This is the default consistency level. It guarantees that data survives any single-Keeper failure.

### ALL

Every Keeper in the cluster must ACK. Highest durability, highest latency. Use for writes that must survive simultaneous loss of all-but-one Keepers.

### ONE

First Keeper ACK. Minimum durable write latency. Data survives the God node restarting but could be lost if the single acknowledging Keeper also fails before the other Keepers replicate.

### WarmupState and consistency during restart

A restarted God node must not serve QUORUM reads until its in-memory state is fully synchronized with Keepers. Mnemosyne maintains a `WarmupState` enum:

```
Cold  →  Warming  →  Hot
```

- **Cold:** God has just started. No data is in RAM. All writes are forwarded to Keepers directly (write path still works). Reads return `KeeperUnreachable` for QUORUM/ALL.
- **Warming:** At least one Keeper is pushing data. Eventual reads can be served from Keepers but not from God's RAM.
- **Hot:** `SyncComplete` received from all expected Keepers with matching `key_count`. QUORUM reads now served from God's RAM at full speed.

The transition to Hot is triggered by `SyncComplete` frames, not by a timeout. This is deliberate: a timeout-based approach would either leave a warm-up window open (serving stale data) or fail prematurely on slow Keepers. Knowing the exact `key_count` means God can verify that all data has arrived before declaring itself Hot.

### Why not Raft for writes

Raft-based write consensus is used by Themis for leader election, but not for data writes. The reason is latency: Raft requires two round trips (leader → followers propose, followers → leader commit), which at a 10 GbE RTT of ~100µs means 200µs in network time alone before the Keeper has even acknowledged. The p99 < 800µs target for `SET QUORUM` leaves ~600µs for disk I/O and processing after network, which is tight. Hermes's direct-to-Keeper write with ACK counting achieves the same durability guarantee as Raft QUORUM without the two-round-trip overhead.

---

## 7. Recovery Design

### Cold → Warming → Hot state machine

```
                  ┌──────────────────────────────────────────────────────┐
                  │                   God (Mnemosyne)                     │
                  │                                                        │
  restart ──────► │  Cold ──────────────► Warming ─────────────► Hot     │
                  │         first Keeper       SyncComplete from          │
                  │         connects           all Keepers                │
                  └──────────────────────────────────────────────────────┘
```

**Cold state:** God has just restarted (or started fresh). RAM is empty. The cluster is still available for writes because Hermes routes QUORUM writes directly to Keepers. Reads that require God's RAM (EVENTUAL from master) are unavailable.

**Warming state:** One or more Keepers have connected and begun pushing data via `PUSH_KEY` frames. God accumulates keys. A progress counter tracks `received / expected`. The `expected` count is derived from the first `SYNC_START` frame sent by each Keeper.

**Hot state:** God has received `SyncComplete` from every expected Keeper, and the sum of `key_count` values in those frames matches the expected total. God now serves all consistency levels from RAM at full speed.

### Why SyncComplete + key_count instead of a timeout

A naive warm-up design would transition to Hot after a fixed timeout (e.g., "wait 15 seconds, then serve reads"). This has two failure modes:

1. **Premature Hot:** The timeout fires before Keepers finish pushing. God serves reads with incomplete data — effectively serving stale cache misses as if they were authoritative.
2. **Indefinite Warming:** A Keeper that is slow (high disk I/O, network congestion) causes the timeout to be too short. Either the timeout is set conservatively long (delaying availability) or data integrity is at risk.

`SyncComplete` with an explicit `key_count` solves both: God transitions to Hot exactly when it has received every key that every Keeper intends to push. There is no race and no timeout. The `key_count` field is the Keeper's snapshot count at the moment it sent `SYNC_START`, so God can detect if a Keeper crashes mid-sync (connection drops before `SyncComplete` arrives) and remain in Warming until the Keeper reconnects and resumes.

### WAL replay and snapshot recovery

On Keeper restart, Hypnos loads state in this order:

1. **Load snapshot (Melete):** The most recent complete snapshot is loaded from `mneme.snap`. This establishes the baseline key set.
2. **Replay WAL (Aoide):** WAL segments written after the snapshot are replayed in order. Each segment is checksummed; a torn write (partial segment from a crash) is detected and discarded. Only complete segments are applied.
3. **Delta sync (Hermes):** The Keeper sends `SYNC_REQUEST` to God with the LSN of its last applied WAL entry. God pushes any writes that occurred during the Keeper's downtime.
4. **SyncComplete:** When the delta sync finishes, the Keeper sends `SyncComplete` and resumes normal replication.

This design means a Keeper that was down for a short time (seconds to minutes) only needs to replay a small delta. A Keeper that was down for hours may need to receive a large delta from God or, if God has also been restarted, from another Keeper's snapshot.

### Keys expired during downtime

When God pushes keys to a Keeper via `PUSH_KEY`, it includes `expiry_at: u64` (unix ms). On receiving a key, the Keeper checks:

```
if expiry_at > 0 && expiry_at <= now_ms() {
    // Key has already expired; do not store
}
```

This ensures that a Keeper coming back online after a long outage does not resurface keys that expired while it was down. The check is performed in constant time at the Hermes receive path, before the key reaches Aoide or Oneiros.
