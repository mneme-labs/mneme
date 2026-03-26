<div align="center">

# MnemeCache

**Distributed in-memory cache — built in Rust, Linux 5.19+**

[![Docker Hub](https://img.shields.io/badge/Docker%20Hub-mnemelabs-blue?logo=docker)](https://hub.docker.com/u/mnemelabs)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Linux 5.19+](https://img.shields.io/badge/Linux-5.19%2B-orange.svg)](https://kernel.org)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-orange.svg)](https://rustup.rs)

[Docs](docs/) · [Client Protocol](docs/CLIENT_PROTOCOL.md) · [API Reference](docs/API.md) · [Docker Hub](https://hub.docker.com/u/mnemelabs)

</div>

---

## What is MnemeCache?

MnemeCache is a distributed in-memory cache designed for sub-millisecond reads. A **Core** node holds the full hot set in RAM; **Keeper** nodes provide WAL persistence, snapshots, and cold storage. Three-node Raft consensus enables automatic leader failover with no manual intervention. **Read replicas** scale horizontal read throughput for EVENTUAL workloads.

**Key properties:**

| Property | Value |
|----------|-------|
| p99 GET (RAM hit) | < 150 µs |
| p99 SET (QUORUM) | < 800 µs |
| Leader failover | < 5 s (Raft election) |
| Core restart (hot) | < 15 s |
| Read replica sync | < 3 s |
| Wire protocol | Binary over TLS 1.3 |
| Data types | String, Hash, List, Sorted Set, Counter, JSON |
| Auth | HMAC-SHA256 tokens · RBAC (admin/readwrite/readonly) · per-database allowlists |
| Observability | Prometheus metrics · Grafana · SLOWLOG · per-command histograms |

---

## Topologies

| Topology | Profile | Use case |
|----------|---------|----------|
| **Solo** | `--profile solo` | Development, CI, single-server |
| **Cluster** | `--profile cluster` | Production: 1 Core + 3 Keepers |
| **HA** | `--profile ha` | High availability: 3-Core Raft + 2 Keepers |
| **HA + Replicas** | `--profile ha-full` | HA + read replicas + Prometheus + Grafana |
| **Full** | `--profile full` | 1 Core + 3 Keepers + 2 replicas + monitoring |

---

## Quick Start

### Docker — Solo mode (30 seconds)

```bash
docker pull mnemelabs/core:1.0.0

docker run -d \
  --name mneme \
  -p 6379:6379 -p 9090:9090 \
  -e MNEME_ADMIN_PASSWORD=secret \
  -v mneme-data:/var/lib/mneme \
  mnemelabs/core:1.0.0

# Wait ~5s for TLS bootstrap, then:
docker exec mneme mneme-cli -u admin -p secret ping
# → PONG
```

### Docker Compose — HA cluster

```bash
git clone https://github.com/vusalrahimov/mnemecache && cd mnemecache

# HA: 3-Core Raft + 2 Keepers (automatic leader election)
MNEME_ADMIN_PASSWORD=secret docker compose --profile ha up -d

# HA + replicas + monitoring
MNEME_ADMIN_PASSWORD=secret docker compose --profile ha-full up -d

# Verify leader
docker exec mneme-core-1 mneme-cli -u admin -p secret cluster-info

# Tear down
docker compose down -v
```

### All topologies

```bash
docker compose --profile solo     up -d   # Dev
docker compose --profile cluster  up -d   # Core + 3 Keepers
docker compose --profile full     up -d   # Cluster + 2 replicas + monitoring
docker compose --profile ha       up -d   # 3-Core Raft HA
docker compose --profile ha-full  up -d   # HA + replicas + monitoring
```

---

## CLI Usage

```bash
# Authenticate (token is cached at ~/.mneme/tokens/)
TOKEN=$(mneme-cli -u admin -p secret auth-token)

# String
mneme-cli -u admin -p secret set greeting "hello"
mneme-cli -u admin -p secret get greeting

# Hash
mneme-cli -u admin -p secret hset user:1 name Alice age 30
mneme-cli -u admin -p secret hget user:1 name
mneme-cli -u admin -p secret hgetall user:1

# List
mneme-cli -u admin -p secret lpush queue task1 task2 task3
mneme-cli -u admin -p secret lrange queue 0 -- -1

# Sorted set
mneme-cli -u admin -p secret zadd leaderboard 100 alice 200 bob
mneme-cli -u admin -p secret zrange leaderboard 0 -- -1 --with-scores

# Counter
mneme-cli -u admin -p secret set visits 0
mneme-cli -u admin -p secret incr visits
mneme-cli -u admin -p secret incrby visits 10

# JSON
mneme-cli -u admin -p secret json-set config '$.timeout' '30'
mneme-cli -u admin -p secret json-get config '$.timeout'

# TTL
mneme-cli -u admin -p secret set session:abc token123 --ttl 3600
mneme-cli -u admin -p secret ttl session:abc

# Bulk operations
mneme-cli -u admin -p secret mset k1 v1 k2 v2 k3 v3
mneme-cli -u admin -p secret mget k1 k2 k3

# SCAN with glob pattern
mneme-cli -u admin -p secret scan --pattern "user:*" --count 100

# Multiple databases
mneme-cli -u admin -p secret db-create analytics
mneme-cli -u admin -p secret --db 1 set isolated-key value

# Cluster info
mneme-cli -u admin -p secret cluster-info
mneme-cli -u admin -p secret keeper-list
mneme-cli -u admin -p secret stats
```

### REPL mode (< 2ms per command)

```bash
# Interactive shell with persistent TLS connection
mneme-cli -u admin -p secret repl

# Pipe mode (stdin → commands, stdout → results)
echo -e "set k v\nget k" | mneme-cli -u admin -p secret repl

# Benchmark with saved profile
mneme-cli profile-set prod --host prod.example.com:6379 --ca-cert /etc/mneme/ca.crt
mneme-cli --profile prod repl
```

> **Latency tip:** Each standalone CLI invocation incurs a TLS handshake (~35 ms on Docker Desktop). Use `repl` mode for interactive sessions or scripting — it keeps the connection open and achieves < 2 ms per command.

### Consistency levels

```bash
mneme-cli -u admin -p secret --consistency EVENTUAL get key    # fastest, may read stale
mneme-cli -u admin -p secret --consistency ONE      set key v  # first Keeper ACK
mneme-cli -u admin -p secret --consistency QUORUM   set key v  # majority (default for writes)
mneme-cli -u admin -p secret --consistency ALL      set key v  # every Keeper ACK
```

### Connection profiles

```bash
# Save host/cert/credentials — no need to repeat flags
mneme-cli profile-set prod \
  --host prod.example.com:6379 \
  --ca-cert /etc/mneme/ca.crt \
  --username admin

mneme-cli --profile prod cluster-info
mneme-cli --profile prod repl
```

---

## Rust Client Library — Pontus

```toml
# Cargo.toml
[dependencies]
mneme-client = { path = "mneme-client" }      # from source
mneme-common = { path = "mneme-common" }      # required for Value type
```

### Basic usage

```rust
use mneme_client::{MnemePool, PoolConfig, Consistency};
use mneme_common::types::Value;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool = MnemePool::new(PoolConfig {
        addr:        "127.0.0.1:6379".into(),
        tls_ca_cert: Some("/etc/mneme/ca.crt".into()),
        server_name: "mneme.local".into(),
        token:       std::env::var("MNEME_TOKEN")?,
        ..Default::default()
    }).await?;

    let conn = pool.acquire().await?;

    // String
    conn.set(b"greeting", Value::String(b"hello".to_vec()), 0, Consistency::Quorum).await?;
    let val = conn.get(b"greeting").await?;           // Option<Value>

    // Hash
    conn.hset(b"user:1", vec![
        (b"name".to_vec(), b"Alice".to_vec()),
        (b"age".to_vec(),  b"30".to_vec()),
    ]).await?;
    let name = conn.hget(b"user:1", b"name").await?; // Option<Vec<u8>>

    // Sorted set
    conn.zadd(b"scores", vec![
        mneme_common::types::ZSetMember { member: b"alice".to_vec(), score: 100.0 },
    ]).await?;
    let rank = conn.zrank(b"scores", b"alice").await?; // Option<u64>

    // Counter
    conn.set(b"visits", Value::Counter(0), 0, Consistency::Quorum).await?;
    let n = conn.incr(b"visits").await?;               // i64

    // JSON
    conn.json_set(b"config", "$", r#"{"timeout":30}"#).await?;
    let doc = conn.json_get(b"config", "$.timeout").await?; // Option<String>

    Ok(())
}
```

### Pipeline (batched writes)

```rust
use mneme_client::{Pipeline, Consistency};
use mneme_common::{CmdId, types::Value};

let mut p = Pipeline::new();
p.set(b"key1", Value::String(b"v1".to_vec()), 0, Consistency::Quorum)
 .set(b"key2", Value::Counter(42), 3600_000, Consistency::Quorum)
 .incr(b"counter")
 .get(b"key1");

let results = conn.execute_pipeline(p).await?;
for frame in &results {
    match frame.cmd_id {
        CmdId::Ok    => { /* decode frame.payload per command */ }
        CmdId::Error => {
            let msg: String = rmp_serde::from_slice(&frame.payload)?;
            eprintln!("error: {msg}");
        }
        _ => {}
    }
}
```

### HA failover

```rust
let pool = MnemePool::new(PoolConfig {
    addr:  "core-1:6379".into(),
    addrs: vec![                        // tried in order if primary fails
        "core-2:6379".into(),
        "core-3:6379".into(),
    ],
    tls_ca_cert: Some("/etc/mneme/ca.crt".into()),
    server_name: "mneme.local".into(),
    token: token.clone(),
    ..Default::default()
}).await?;
```

### Monitor stream

```rust
// Use a dedicated connection for monitoring
let mon_conn = /* open connection */;
let mut stream = mon_conn.monitor().await?;
while let Some(event) = stream.next().await {
    println!("[MONITOR] {event}");
}
```

### Admin operations

```rust
// User management
conn.user_create("alice", "pass", "readwrite").await?;
conn.user_grant("alice", 1).await?;      // grant access to db 1
conn.user_set_role("alice", "readonly").await?;

// Keeper join token (for adding new Keeper nodes)
let token = conn.generate_join_token().await?;
println!("Join token: {token}");

// Wait for replication
let acked = conn.wait(2, 5000).await?;   // wait up to 5s for 2 keepers

// Slot mapping
let slots = conn.cluster_slots().await?;
for slot in &slots { println!("{}-{} → {}", slot.start, slot.end, slot.addr); }

// Cluster info
let info = conn.cluster_info().await?;
for (k, v) in &info { println!("{k}: {v}"); }

// Config hot-reload
conn.config_set("memory.pool_bytes", "4gb").await?;
```

See [mneme-client/README.md](mneme-client/README.md) for the full API reference.

---

## Docker Images

| Image | Description | Ports |
|-------|-------------|-------|
| `mnemelabs/core:1.0.0` | Core node (solo / cluster / HA) | 6379, 7379, 9090 |
| `mnemelabs/keeper:1.0.0` | Keeper node (WAL + snapshots + cold store) | 7379, 9090 |
| `mnemelabs/cli:1.0.0` | CLI management tool | — |

Build locally:

```bash
docker build --target core   -t mnemelabs/core:1.0.0   .
docker build --target keeper -t mnemelabs/keeper:1.0.0 .
docker build --target cli    -t mnemelabs/cli:1.0.0    .
```

---

## Production Docker Deploy

```bash
MNEME_ADMIN_PASSWORD=$(openssl rand -hex 16) \
MNEME_VERSION=1.0.0 \
CORE_MEM_LIMIT=16g \
KEEPER_MEM_LIMIT=8g \
  docker compose \
    -f docker-compose.yml \
    -f docker-compose.prod.yml \
    --profile ha-full up -d
```

---

## Kubernetes

### Raw manifests

```bash
kubectl create namespace mneme
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/secrets.yaml      # edit cluster_secret + admin_password first
kubectl apply -f k8s/core.yaml
kubectl apply -f k8s/keeper.yaml
kubectl apply -f k8s/replica.yaml      # optional
```

### Helm chart

```bash
# Solo (development)
helm install mneme ./k8s/helm/mneme \
  --namespace mneme --create-namespace \
  --set auth.adminPassword=secret \
  --set auth.clusterSecret=$(openssl rand -hex 32)

# HA topology (3 Cores + 3 Keepers + 2 replicas)
helm install mneme ./k8s/helm/mneme \
  -f k8s/helm/mneme/values.ha.yaml \
  --namespace mneme --create-namespace \
  --set auth.adminPassword=secret \
  --set auth.clusterSecret=$(openssl rand -hex 32)
```

---

## Linux Native

```bash
# Build from source (requires Rust 1.85+)
cargo build --release --workspace

sudo install -m 755 target/release/mneme-core   /usr/local/bin/
sudo install -m 755 target/release/mneme-keeper /usr/local/bin/
sudo install -m 755 target/release/mneme-cli    /usr/local/bin/
sudo install -m 755 target/release/mneme-bench  /usr/local/bin/

sudo useradd -r -s /bin/false mneme
sudo install -d -m 750 -o mneme -g mneme /var/lib/mneme /etc/mneme

sudo cp docker/configs/solo.toml /etc/mneme/
sudo -u mneme mneme-core --config /etc/mneme/solo.toml
```

See [docs/setup/linux.md](docs/setup/linux.md) for systemd service files.

---

## Configuration

```toml
[node]
role     = "solo"           # solo | core | keeper | read-replica
node_id  = "mneme-1"
port     = 6379             # client TLS
rep_port = 7379             # replication mTLS

[memory]
pool_bytes         = "2gb"
eviction_threshold = 0.90   # LFU eviction starts at 90% pressure

[cluster]
raft_id        = 1                              # HA only
advertise_addr = "host:7379"                    # HA only
peers          = ["core-2:7379", "core-3:7379"] # HA only

[tls]
auto_generate = true
server_name   = "mneme.local"
extra_sans    = ["hostname"]   # additional DNS/IP SANs

[auth]
cluster_secret = "..."   # shared HMAC secret — change in production!
token_ttl_h    = 24
```

Full reference: [docs/setup/linux.md](docs/setup/linux.md)

---

## Performance

| Operation | Consistency | p99 |
|-----------|-------------|-----|
| GET — RAM hit | EVENTUAL | < 150 µs |
| GET — cold (Oneiros) | EVENTUAL | < 1.2 ms |
| SET | QUORUM | < 800 µs |
| Token validate | — | ~75 ns |
| Core restart (hot) | — | < 15 s |
| Read replica sync | — | < 3 s |

```bash
# Built-in benchmark
mneme-bench --host localhost:6379 --ca-cert /etc/mneme/ca.crt \
  -u admin -p secret \
  --ops 100000 --concurrency 64 --pipeline 16
```

---

## Wire Protocol

Custom binary protocol over TLS 1.3. Any language can implement a client.

```
[4B magic 0x4D4E454D][1B ver][1B cmd_id][2B flags][4B payload_len][4B req_id][msgpack payload]
```

- Flags bits 15–4: slot hint (CRC16(key) % 16384)
- Flags bits 3–2: consistency level (00=EVENTUAL, 01=QUORUM, 10=ALL, 11=ONE)
- req_id ≥ 1: multiplexed — responses may arrive out of order

Full specification: [docs/CLIENT_PROTOCOL.md](docs/CLIENT_PROTOCOL.md)
Command reference: [docs/API.md](docs/API.md)
Rust client library: [mneme-client/README.md](mneme-client/README.md)

---

## Monitoring

```bash
# Prometheus metrics (plain HTTP, no auth)
curl http://localhost:9090/metrics

# Key metrics:
#   mneme_pool_bytes_used            — hot RAM in use
#   mneme_requests_total{cmd,cons}   — by command + consistency level
#   mneme_replication_lag_ms{keeper} — per-Keeper replication lag
#   mneme_evictions_total{reason}    — lfu / ttl / oom eviction counts
#   mneme_cluster_term               — Raft term (HA only)
#   mneme_live_nodes                 — Core nodes in quorum

# Grafana (docker compose --profile full or ha-full)
open http://localhost:3000   # admin/admin
```

---

## Development

```bash
# Tests
cargo test --workspace

# Integration tests (bind loopback ports — run serially)
cargo test --test integration_solo -p mneme-core -- --test-threads=1

# Watch mode
cargo watch -x check

# Dev container
docker build --target dev -t mnemelabs/dev:latest .
docker run --rm -it -v $(pwd):/build mnemelabs/dev:latest
```

---

## Ports

| Port | Protocol | Description |
|------|----------|-------------|
| 6379 | TLS 1.3 | Client connections |
| 7379 | mTLS | Replication (Core↔Keeper, Raft peers) |
| 9090 | HTTP | Prometheus metrics |

---

## License

MIT — see [LICENSE](LICENSE).

---

<div align="center">
Built by <a href="https://github.com/vusalrahimov">Vusal Rahimov</a>
</div>
