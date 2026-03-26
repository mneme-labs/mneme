# MnemeCache — Operations Guide

Day-to-day operations for MnemeCache clusters: starting topologies, scaling,
HA failover, read replicas, REPL mode, monitoring, and upgrades.

---

## 1. Starting a Cluster

### Solo mode (development)

```bash
# Docker
docker run -d --name mneme -p 6379:6379 -p 9090:9090 \
  -e MNEME_ADMIN_PASSWORD=secret \
  -v mneme-data:/var/lib/mneme \
  mnemelabs/core:1.0.0

# Docker Compose
docker compose --profile solo up -d

# Linux native
mneme-core --config /etc/mneme/solo.toml
```

### Cluster (Core + Keepers)

```bash
# Docker Compose — Core + 3 Keepers
MNEME_ADMIN_PASSWORD=secret docker compose --profile cluster up -d

# Verify
docker exec mneme-core mneme-cli -u admin -p secret keeper-list
docker exec mneme-core mneme-cli -u admin -p secret cluster-info
```

### HA — 3-Core Raft

```bash
# Docker Compose — 3 Cores + 2 Keepers
MNEME_ADMIN_PASSWORD=secret docker compose --profile ha up -d

# Verify leader election
docker exec mneme-core-1 mneme-cli -u admin -p secret cluster-info | grep is_leader
```

### Full topology (Cluster + Replicas + Monitoring)

```bash
MNEME_ADMIN_PASSWORD=secret docker compose --profile full up -d

# Services started:
#   mneme-core          — Core node         :6379
#   mneme-keeper-1/2/3  — Keeper nodes
#   mneme-replica       — Read replica      :6380
#   mneme-replica-2     — Read replica      :6381
#   mneme-prometheus    — Prometheus        :9094
#   mneme-grafana       — Grafana           :3000
```

---

## 2. Adding a Keeper Node

### Via CLI

```bash
# Step 1 — generate a join token (admin role)
TOKEN=$(mneme-cli -u admin -p secret join-token)

# Step 2 — start the Keeper with the token
mneme-keeper --config /etc/mneme/keeper.toml --join-token "$TOKEN"

# Step 3 — verify registration
mneme-cli -u admin -p secret keeper-list
```

### Via Pontus client library

```rust
let token = conn.generate_join_token().await?;
println!("mneme-keeper --join-token {token}");
```

### Via Docker Compose

Add a new service to your compose file or use the `cluster` profile which
includes three Keepers. The Keeper bootstrap loop (Herold) handles mTLS cert
generation and Core registration automatically.

---

## 3. Read Replicas

Read replicas receive a continuous stream of `PushKey` frames from the Core
and serve EVENTUAL reads from their local pool.

```bash
# Start replica
MNEME_ADMIN_PASSWORD=secret docker compose --profile full up -d

# Verify replica health
docker exec mneme-replica mneme-cli \
  --host localhost:6380 \
  -u admin -p secret \
  cluster-info | grep warmup_state

# Read from replica (EVENTUAL consistency)
mneme-cli --host localhost:6380 -u admin -p secret --consistency EVENTUAL get mykey
```

### Replica reconnect behaviour

When a replica restarts or loses its connection to the Core:

1. Replica re-registers with Core via Herold.
2. Core detects `key_count=0` in `SyncComplete` and spawns a full pool replay task.
3. Core pushes all current keys to the replica (without holding any shard locks).
4. The replica reaches `WarmupState::Hot` and begins serving reads.

Pool replay is logged:
```
[INFO] Replica full-pool replay complete node_id=<id> pushed=<n> total=<n>
```

---

## 4. Raft HA — Failover

MnemeCache uses [openraft](https://github.com/datafuselabs/openraft) for
consensus. With 3 Core nodes, the cluster tolerates one node failure.

### Checking leader status

```bash
mneme-cli -u admin -p secret cluster-info
# raft_term: 3
# is_leader: true
# leader_id: 1
# leader_addr: 10.0.0.1:6379
```

### Simulating leader failure

```bash
# Stop leader
docker stop mneme-core-1

# Watch election (should complete in < 5s)
sleep 1
docker exec mneme-core-2 mneme-cli -u admin -p secret cluster-info | grep is_leader
```

### Client failover behaviour

The Pontus pool handles `LeaderRedirect` automatically:
1. Write sent to a follower → server returns `LeaderRedirect { leader_addr }`.
2. Pool reconnects to `leader_addr` and retries the write.
3. If the leader is unknown (election in progress), the pool backs off and retries.

For the CLI:
```bash
# Set all Core addresses so CLI tries failover on redirect
mneme-cli --host core-1:6379,core-2:6379,core-3:6379 -u admin -p secret set k v
```

---

## 5. CLI — REPL and Pipe Mode

### REPL mode

REPL mode maintains a persistent TLS connection, achieving < 2 ms per command
(versus ~35 ms per invocation with standalone CLI calls on Docker Desktop).

```bash
# Interactive REPL shell
mneme-cli -u admin -p secret repl

mneme> set greeting hello
OK (0.19ms)
mneme> get greeting
"hello" (0.09ms)
mneme> incr counter
1 (0.21ms)
mneme> cluster-info
...
mneme> exit
```

### Pipe mode (non-interactive)

```bash
# Execute a script via stdin
echo -e "set k1 v1\nset k2 v2\nget k1" | mneme-cli -u admin -p secret repl

# Pipe from a file
mneme-cli --profile prod repl < commands.txt

# In CI/scripting
mneme-cli -u admin -p secret repl <<'EOF'
mset k1 v1 k2 v2 k3 v3
mget k1 k2 k3
dbsize
EOF
```

### Connection profiles

```bash
# Save a profile (stored in ~/.mneme/profiles.toml)
mneme-cli profile-set prod \
  --host prod.example.com:6379 \
  --ca-cert /etc/mneme/ca.crt \
  --username admin

# Use the profile
mneme-cli --profile prod repl
mneme-cli --profile prod cluster-info

# List profiles
mneme-cli profile-list

# Show profile details
mneme-cli profile-show prod
```

---

## 6. User Management

### Creating users

```bash
# Create a readwrite user
mneme-cli -u admin -p secret user-create alice password123

# Create an admin user
mneme-cli -u admin -p secret user-create bob password456 --role admin

# Create a readonly user
mneme-cli -u admin -p secret user-create carol password789 --role readonly
```

### RBAC — database allowlists

```bash
# Grant access to specific database IDs
mneme-cli -u admin -p secret user-grant alice 1
mneme-cli -u admin -p secret user-grant alice 2

# Revoke access
mneme-cli -u admin -p secret user-revoke alice 2

# Users with empty allowlist can access all databases
```

### Roles

| Role | Permissions |
|------|-------------|
| `admin` | Full access — user management, cluster operations, config |
| `readwrite` | Read and write keys; no user/cluster management |
| `readonly` | Read-only; EVENTUAL reads only on allowed databases |

---

## 7. Monitoring

### Prometheus + Grafana

```bash
# Start monitoring stack
docker compose --profile full up -d

# Prometheus
open http://localhost:9094/metrics

# Grafana
open http://localhost:3000   # admin/admin
```

### Key metrics

| Metric | Description |
|--------|-------------|
| `mneme_pool_bytes_used` | Hot RAM in use |
| `mneme_pool_bytes_max` | RAM pool capacity |
| `mneme_memory_pressure_ratio` | Usage ratio (trigger eviction at > 0.9) |
| `mneme_requests_total{cmd,consistency}` | Requests by command + consistency level |
| `mneme_request_duration_seconds` | Latency histogram |
| `mneme_replication_lag_ms{keeper}` | Per-Keeper write lag |
| `mneme_evictions_total{reason}` | `lfu`, `ttl`, `oom` eviction counts |
| `mneme_cluster_term` | Raft term (HA only) |
| `mneme_live_nodes` | Core nodes currently in quorum |
| `mneme_connections_active` | Active client connections |

### CLI observability

```bash
# Server stats (INFO-style block)
mneme-cli -u admin -p secret stats

# Slow query log
mneme-cli -u admin -p secret slowlog

# Memory usage of a specific key
mneme-cli -u admin -p secret memory-usage user:1

# Pool statistics
mneme-cli -u admin -p secret pool-stats

# Cluster summary
mneme-cli -u admin -p secret cluster-info

# Keeper list
mneme-cli -u admin -p secret keeper-list
```

### MONITOR stream

```bash
# Stream all commands through the CLI
mneme-cli -u admin -p secret repl
mneme> monitor
```

Or via the client library:

```rust
let mut stream = conn.monitor().await?;
while let Some(event) = stream.next().await {
    println!("[MONITOR] {event}");
}
```

---

## 8. Memory Management

### Pressure levels

| Pressure | Threshold | Action |
|----------|-----------|--------|
| Low | < 0.70 | No action |
| Moderate | ≥ 0.70 | Alert via metrics |
| High | ≥ 0.90 | LFU eviction — coldest 1% of keys migrated to Keeper cold store |
| Critical | ≥ 1.00 | OOM eviction — 5% of LFU-coldest keys evicted immediately |

### Hot-reload pool size

```bash
# Increase pool without restart
mneme-cli -u admin -p secret config-set memory.pool_bytes 4gb
```

Or via client library:

```rust
conn.config_set("memory.pool_bytes", "4gb").await?;
```

---

## 9. Shutdown Order

**Incorrect shutdown order causes data loss.** Always follow this sequence:

### Core node

1. Stop accepting new connections (SIGTERM or `docker stop`)
2. Drain in-flight requests
3. Flush all pending Hermes replication frames to Keepers
4. Themis (Raft) steps down as leader
5. Drop memory pool shards
6. Flush Aletheia (Prometheus metrics) to disk
7. Close TLS sessions
8. Exit

### Keeper node

1. Stop accepting new REPLICATE frames
2. fsync WAL (Aoide)
3. Write final snapshot (Melete)
4. Close Hermes replication connection
5. Close Oneiros (redb cold store)
6. Exit

Docker's default graceful shutdown (`docker stop`) gives 10 seconds. For large
pools, increase the stop timeout:

```bash
docker stop --time 30 mneme-core
```

---

## 10. Backup and Recovery

### Keeper WAL + snapshots

Keepers write all data to `$data_dir/wal/` (O_DIRECT WAL segments) and
periodic snapshots to `$data_dir/snapshots/`. Back up both directories.

```bash
# Snapshot directory on keeper-1
ls /var/lib/mneme/snapshots/

# Trigger a manual snapshot (via config reload or restart)
# Snapshots are taken automatically on Keeper shutdown
```

### Cold store (Oneiros)

Cold data lives in `$data_dir/oneiros.redb` (redb B-tree). Back up this file
while the Keeper is stopped or during a hot backup using redb's copy API.

### Recovery

1. Start Core in solo mode with an empty pool.
2. Start Keepers with existing WAL/snapshot/cold-store directories.
3. Keepers push all keys to Core during warmup (SyncStart → PushKey × N → SyncComplete).
4. Core warmup state transitions: `Cold → Warming → Hot`.
5. QUORUM/ALL writes unblock once all registered Keepers reach `Hot`.

---

## 11. Upgrades

### Rolling upgrade (HA cluster)

```bash
# Upgrade one Core at a time — cluster remains available

# Step 1: upgrade follower cores first
docker pull mnemelabs/core:1.1.0
docker stop mneme-core-3
docker run -d --name mneme-core-3 ... mnemelabs/core:1.1.0

# Step 2: trigger leader re-election, then upgrade old leader
docker stop mneme-core-1  # triggers election — cluster elects new leader
sleep 5
docker run -d --name mneme-core-1 ... mnemelabs/core:1.1.0

# Verify all nodes are on new version
docker exec mneme-core-1 mneme-cli -u admin -p secret stats | grep version
```

### Single-node upgrade

```bash
docker stop mneme-core
docker pull mnemelabs/core:1.1.0
docker start mneme-core   # or re-run with new image tag
```
