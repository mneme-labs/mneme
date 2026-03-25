<div align="center">

# Mneme

**Distributed in-memory cache — built in Rust, Linux 5.19+**

[![Docker Hub](https://img.shields.io/badge/Docker%20Hub-mnemelabs-blue?logo=docker)](https://hub.docker.com/u/mnemelabs)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Linux 5.19+](https://img.shields.io/badge/Linux-5.19%2B-orange.svg)](https://kernel.org)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-orange.svg)](https://rustup.rs)

[Website](https://mnemelabs.io) · [Docs](docs/) · [Docker Hub](https://hub.docker.com/u/mnemelabs)

</div>

---

## What is Mneme?

Mneme is a distributed in-memory cache designed for sub-millisecond reads. It uses a God-node architecture: a Core node holds the full hot set in RAM while Keeper nodes provide WAL persistence, snapshots, and cold storage. Three-node Raft consensus enables automatic leader failover with no manual intervention. Read replicas scale horizontal read throughput for EVENTUAL workloads.

**Key properties:**
- **p99 GET (RAM hit): < 150µs** — EVENTUAL read, no disk
- **p99 SET (QUORUM): < 800µs** — acknowledged by floor(N/2)+1 Keepers
- **Failover: < 5s** — Raft election on leader loss, clients auto-redirect
- **Wire protocol**: binary over TLS 1.3 — any language, see [CLIENT_PROTOCOL.md](docs/CLIENT_PROTOCOL.md)
- **4 data types**: String, Hash, List, Sorted Set
- **RBAC**: admin / readwrite / readonly roles with per-database allowlists
- **Observability**: Prometheus metrics + Grafana, SLOWLOG, per-command histograms

---

## Topologies

| Topology | Command | Use case |
|----------|---------|----------|
| **Solo** | `--profile solo` | Development, CI, single-server |
| **Cluster** | `--profile cluster` | Production: Core + 3 Keepers |
| **HA** | `--profile ha` | High availability: 3-Core Raft + 2 Keepers |
| **HA + Replicas** | `--profile ha-full` | HA + failover-aware read replicas + monitoring |
| **Full** | `--profile full` | Cluster + 2 replicas + Prometheus + Grafana |

---

## Quick Start

### Docker — Solo mode (30 seconds)

```bash
docker pull mnemelabs/core:0.1.0

docker run -d \
  --name mneme \
  -p 6379:6379 -p 9090:9090 \
  -e MNEME_ADMIN_PASSWORD=secret \
  -v mneme-data:/var/lib/mneme \
  mnemelabs/core:0.1.0

# Wait ~10s for TLS bootstrap, then:
docker exec mneme mneme-cli -u admin -p secret ping
# → PONG
```

### Docker Compose — HA cluster

```bash
git clone https://github.com/mneme-labs/mneme && cd mneme

# HA: 3-Core Raft + 2 Keepers (automatic leader election)
MNEME_ADMIN_PASSWORD=secret docker compose --profile ha up -d

# HA + replicas + monitoring
MNEME_ADMIN_PASSWORD=secret docker compose --profile ha-full up -d

# Verify leader
docker exec mneme-core-1 mneme-cli -u admin -p secret cluster-info

# Tear down
docker compose down -v
```

### Docker Compose — All topologies

```bash
# Solo (dev)
docker compose --profile solo up -d

# Cluster: Core + 2 Keepers
docker compose --profile cluster up -d

# Cluster + read replicas
docker compose --profile cluster --profile replica up -d

# Full: Core + 3 Keepers + 2 replicas + Prometheus + Grafana
docker compose --profile full up -d
```

---

## CLI Usage

```bash
# Connect to any running Core or replica
# CA cert is linked automatically — no --insecure needed
mneme-cli -u admin -p secret ping

# Strings
mneme-cli -u admin -p secret set greeting "hello"
mneme-cli -u admin -p secret get greeting

# Hash
mneme-cli -u admin -p secret hset user:1 name Alice age 30
mneme-cli -u admin -p secret hget user:1 name

# List
mneme-cli -u admin -p secret lpush queue task1 task2 task3
mneme-cli -u admin -p secret lrange queue 0 -- -1

# Sorted set
mneme-cli -u admin -p secret zadd leaderboard 100 alice 200 bob
mneme-cli -u admin -p secret zrange leaderboard 0 -- -1 --with-scores

# TTL
mneme-cli -u admin -p secret set session:abc token123 --ttl 3600

# Bulk operations
mneme-cli -u admin -p secret mset k1 v1 k2 v2 k3 v3
mneme-cli -u admin -p secret mget k1 k2 k3

# SCAN with glob pattern
mneme-cli -u admin -p secret scan --pattern "user:*" --count 100

# Multiple databases (16 total, 0-15)
mneme-cli -u admin -p secret --db 1 set isolated-key value

# Cluster info
mneme-cli -u admin -p secret cluster-info
mneme-cli -u admin -p secret stats

# Save a connection profile (no need to type host/password each time)
mneme-cli profile-set prod --host prod.example.com:6379 \
  --ca-cert /etc/mneme/ca.crt --username admin
mneme-cli --profile prod cluster-info
```

### Consistency levels

```bash
# EVENTUAL — fastest, may read stale data (uses replica if available)
mneme-cli -u admin -p secret --consistency EVENTUAL get key

# ONE — first Keeper ACK
mneme-cli -u admin -p secret --consistency ONE set key value

# QUORUM — floor(N/2)+1 Keeper ACKs (default for writes)
mneme-cli -u admin -p secret --consistency QUORUM set key value

# ALL — every Keeper ACK
mneme-cli -u admin -p secret --consistency ALL set key value
```

---

## Docker Images

| Image | Description | Ports |
|-------|-------------|-------|
| `mnemelabs/core:0.1.0` | Core node (solo / cluster / HA) | 6379, 7379, 9090 |
| `mnemelabs/keeper:0.1.0` | Keeper node (persistence) | 7379, 9090 |
| `mnemelabs/cli:0.1.0` | CLI management tool | — |
| `mnemelabs/bench:0.1.0` | Load testing tool | — |

Build locally from source:

```bash
# Build individual images
docker build --target core   -t mnemelabs/core:0.1.0   .
docker build --target keeper -t mnemelabs/keeper:0.1.0 .
docker build --target cli    -t mnemelabs/cli:0.1.0    .
docker build --target bench  -t mnemelabs/bench:0.1.0  .

# Push all (logged in as mnemelabs)
docker push mnemelabs/core:0.1.0
docker push mnemelabs/keeper:0.1.0
docker push mnemelabs/cli:0.1.0
docker push mnemelabs/bench:0.1.0
```

---

## Production Docker Deploy

Use `docker-compose.prod.yml` on top of the base compose for resource limits, restart policies, and log rotation:

```bash
# HA-full topology with production overrides
MNEME_ADMIN_PASSWORD=$(openssl rand -hex 16) \
MNEME_VERSION=0.1.0 \
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

# Apply all resources
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/secrets.yaml      # edit cluster_secret + admin_password first
kubectl apply -f k8s/core.yaml
kubectl apply -f k8s/keeper.yaml
kubectl apply -f k8s/replica.yaml      # optional

# Full cluster (all at once)
kubectl apply -f k8s/full-cluster.yaml
```

### Helm chart

```bash
# Solo (development)
helm install mneme ./k8s/helm/mneme \
  --namespace mneme --create-namespace \
  --set auth.adminPassword=secret \
  --set auth.clusterSecret=$(openssl rand -hex 32)

# Cluster topology
helm install mneme ./k8s/helm/mneme \
  -f k8s/helm/mneme/values.cluster.yaml \
  --namespace mneme --create-namespace \
  --set auth.adminPassword=secret \
  --set auth.clusterSecret=$(openssl rand -hex 32)

# HA topology (3 Cores + 3 Keepers + 2 replicas)
helm install mneme ./k8s/helm/mneme \
  -f k8s/helm/mneme/values.ha.yaml \
  --namespace mneme --create-namespace \
  --set auth.adminPassword=secret \
  --set auth.clusterSecret=$(openssl rand -hex 32)

# Upgrade
helm upgrade mneme ./k8s/helm/mneme --namespace mneme \
  -f k8s/helm/mneme/values.ha.yaml

# Status
helm status mneme --namespace mneme
```

---

## Linux Native (VM / bare metal)

```bash
# Build from source (requires Rust 1.85+)
cargo build --release --workspace

# Install binaries
sudo install -m 755 target/release/mneme-core   /usr/local/bin/
sudo install -m 755 target/release/mneme-keeper /usr/local/bin/
sudo install -m 755 target/release/mneme-cli    /usr/local/bin/
sudo install -m 755 target/release/mneme-bench  /usr/local/bin/

# Create user and directories
sudo useradd -r -s /bin/false mneme
sudo install -d -m 750 -o mneme -g mneme /var/lib/mneme /etc/mneme

# Solo mode — copy config and start
sudo cp docker/configs/solo.toml /etc/mneme/
sudo -u mneme mneme-core --config /etc/mneme/solo.toml

# Bootstrap admin user
sudo -u mneme mneme-core --config /etc/mneme/solo.toml adduser \
  --username admin --password secret --role admin

# See docs/setup/linux.md for systemd service files
```

---

## Configuration

All nodes are configured via TOML files. Key sections:

```toml
[node]
role     = "solo"           # solo | core | keeper | read-replica
node_id  = "mneme-1"
port     = 6379             # client TLS
rep_port = 7379             # replication mTLS

[memory]
pool_bytes          = "2gb"     # hot RAM pool
eviction_threshold  = 0.90      # LFU eviction starts at 90% pressure

[cluster]
raft_id         = 1             # HA only
advertise_addr  = "host:7379"   # HA only — this node's Raft address
peers           = ["core-2:7379", "core-3:7379"]  # HA only

[tls]
auto_generate = true            # rcgen self-signed on first boot
server_name   = "mneme.local"   # SNI name
extra_sans    = ["hostname"]    # additional DNS SANs

[auth]
cluster_secret = "..."          # shared HMAC secret — change in production!
token_ttl_h    = 24
```

Full reference: [docs/setup/linux.md](docs/setup/linux.md)

---

## Performance

| Operation | Consistency | p99 |
|-----------|-------------|-----|
| GET — RAM hit | EVENTUAL | < 150µs |
| GET — cold (Oneiros) | EVENTUAL | < 1.2ms |
| SET | QUORUM | < 800µs |
| Token validate | — | ~75ns |
| Core restart (hot) | — | < 15s |
| Read replica sync | — | < 3s |

Run the built-in benchmark:

```bash
mneme-bench --host localhost:6379 --ca-cert /etc/mneme/ca.crt \
  -u admin -p secret \
  --ops 100000 --concurrency 64 --pipeline 16
```

---

## Wire Protocol

Mneme uses a custom binary protocol over TLS 1.3. Any language can implement a client.

**16-byte frame header:**
```
[4B magic 0x4D4E454D][1B ver][1B cmd_id][2B flags][4B payload_len][4B req_id]
```

Payload is MessagePack. See [docs/CLIENT_PROTOCOL.md](docs/CLIENT_PROTOCOL.md) for the complete specification.

The official Rust client library is **Pontus** (`mneme-client/`).

---

## Monitoring

```bash
# Prometheus metrics (plain HTTP, no auth)
curl http://localhost:9090/metrics

# Key metrics:
#   mneme_pool_bytes_used       — hot RAM in use
#   mneme_requests_total        — by command + consistency
#   mneme_replication_lag_ms    — per-Keeper lag
#   mneme_evictions_total       — LFU, TTL, OOM eviction counts
#   mneme_cluster_term          — Raft term (HA only)

# Grafana (docker compose --profile full)
open http://localhost:3000   # admin/admin
```

---

## Development

```bash
# Run all tests
cargo test --workspace

# Integration tests (bind loopback ports — run serially)
cargo test --test integration_solo -p mneme-core -- --test-threads=1

# Watch mode
cargo watch -x check

# Dev container (includes cargo-watch + cargo-nextest)
docker build --target dev -t mnemelabs/dev:latest .
docker run --rm -it -v $(pwd):/build mnemelabs/dev:latest

# Smoke test against a running cluster
docker compose --profile cluster up -d
docker compose --profile cluster --profile cluster-test run --rm smoke-test
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
Built by <a href="https://mnemelabs.io">Mneme Labs</a> · <a href="https://mnemelabs.io">mnemelabs.io</a>
</div>
