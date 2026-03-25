# MnemeCache -- Docker Setup Guide

Complete guide to running MnemeCache in Docker across all five cluster modes:
Solo, Core + Keepers, Core + Keepers + Read Replica, HA (3-Core Raft), and Full Production.

---

## Table of Contents

1. [Prerequisites](#1-prerequisites)
2. [Quick Start](#2-quick-start)
3. [Build Options](#3-build-options)
4. [Mode 1: Solo Node](#4-mode-1-solo-node)
5. [Mode 2: Core + Keepers Cluster](#5-mode-2-core--keepers-cluster)
6. [Mode 3: Core + Keepers + Read Replica](#6-mode-3-core--keepers--read-replica)
7. [Mode 4: Full Production Cluster](#7-mode-4-full-production-cluster)
8. [Mode 5: HA Cluster (3-Core Raft)](#8-mode-5-ha-cluster-3-core-raft)
9. [Configuration](#9-configuration)
10. [Testing](#10-testing)
10. [Resource Limits](#10-resource-limits)
11. [Networking](#11-networking)
12. [Tear Down](#12-tear-down)
13. [Troubleshooting](#13-troubleshooting)

---

## 1. Prerequisites

| Requirement | Minimum Version | Install |
|---|---|---|
| Docker Engine | 24+ | [docs.docker.com/engine/install](https://docs.docker.com/engine/install/) |
| Docker Compose | v2 (plugin) | Bundled with Docker Desktop; `apt install docker-compose-plugin` on Linux |
| macOS | Docker Desktop for Mac | [Install](https://docs.docker.com/desktop/install/mac-install/) |
| Windows | Docker Desktop (WSL2 backend) | [Install](https://docs.docker.com/desktop/install/windows-install/) |

**OS support.** MnemeCache containers run Linux internally. The host OS can be
Linux, macOS, or Windows. All kernel-level features (O_DIRECT, fallocate,
perf_event_open) work inside the container regardless of the host.

Verify your installation:

```bash
docker --version          # Docker Engine 24.x+
docker compose version    # Docker Compose v2.x+
```

---

## 2. Quick Start

Build the image and launch a solo node in under 30 seconds:

```bash
git clone https://github.com/mneme-labs/mneme && cd mneme

# Build and start a single-node instance
docker compose --profile solo up --build -d

# Verify the node is healthy
docker compose --profile solo ps

# Connect with the CLI
docker compose --profile solo exec mneme-solo \
    mneme-cli --host $MNEME_HOST -u admin -p "${MNEME_ADMIN_PASSWORD:-secret}" ping
```

The node is ready when the health check passes and `ping` returns `PONG`.

---

## 3. Build Options

### Single-platform build (fastest)

```bash
docker build -t mnemelabs/core:1.0.0 .
```

The multi-stage Dockerfile uses cargo-chef for dependency caching. Subsequent
builds after a source-only change take seconds, not minutes.

### Build all images

```bash
# Build each component image separately
docker build --target core   -t mnemelabs/core:1.0.0   .
docker build --target keeper -t mnemelabs/keeper:1.0.0 .
docker build --target cli    -t mnemelabs/cli:1.0.0    .

# Push to registry
docker push mnemelabs/core:1.0.0
docker push mnemelabs/keeper:1.0.0
docker push mnemelabs/cli:1.0.0
```

### Development image

The `dev` target includes the full Rust toolchain, cargo-watch, and
cargo-nextest:

```bash
docker build --target dev -t mnemelabs/core:dev .

# Interactive dev shell with source mounted
docker run --rm -it -v "$(pwd)":/build mnemelabs/core:dev bash
```

### Dockerfile stages

| Stage | Purpose |
|---|---|
| `chef` | Base toolchain with cargo-chef and Linux headers |
| `planner` | Computes dependency recipe from Cargo.lock |
| `builder` | Compiles workspace (deps cached, source rebuilt) |
| `tester` | Runs unit and integration tests inside the build |
| `dev` | Full toolchain for interactive development |
| `runtime` | Minimal Debian slim image with three binaries only |

---

## 4. Mode 1: Solo Node

A single process running Mnemosyne (Core) with an embedded Hypnos (Keeper).
WAL, snapshots, and cold store (Oneiros) all run in-process. Data survives
restarts via WAL replay and snapshot restore.

**Best for:** development, CI pipelines, single-server deployments.

### Start

```bash
docker compose --profile solo up -d
```

### Architecture

```
┌───────────────────────────────────────────────┐
│  Docker network: mneme-net                    │
│                                               │
│  ┌─────────────────────────────────────────┐  │
│  │  mneme-solo                             │  │
│  │  ┌───────────┐  ┌──────────────────┐    │  │
│  │  │ Mnemosyne │  │ Embedded Hypnos  │    │  │
│  │  │  (Core)   │──│ WAL + Snapshots  │    │  │
│  │  │           │  │ + Oneiros (cold) │    │  │
│  │  └───────────┘  └──────────────────┘    │  │
│  │  :6379 client   :9090 metrics           │  │
│  └─────────────────────────────────────────┘  │
└───────────────────────────────────────────────┘
```

### Ports

| Host Port | Container Port | Purpose |
|---|---|---|
| 6379 | 6379 | Client TLS |
| 9090 | 9090 | Prometheus metrics |

### Volumes

| Volume | Mount Point | Contents |
|---|---|---|
| `solo-data` | `/var/lib/mneme` | WAL, snapshots, cold store, TLS certs, users DB |

### Environment Variables

| Variable | Default | Description |
|---|---|---|
| `MNEME_ADMIN_PASSWORD` | `secret` | Admin user password |
| `MNEME_POOL_BYTES` | `512mb` | Hot RAM pool size |
| `MNEME_LOG_LEVEL` | `info` | Log verbosity: trace, debug, info, warn, error |

### Verify

```bash
# Health check
docker compose --profile solo ps

# Ping
docker compose --profile solo exec mneme-solo \
    mneme-cli --host $MNEME_HOST -u admin -p secret ping

# Write and read
docker compose --profile solo exec mneme-solo \
    mneme-cli --host $MNEME_HOST -u admin -p secret set hello "world"
docker compose --profile solo exec mneme-solo \
    mneme-cli --host $MNEME_HOST -u admin -p secret get hello

# Check stats
docker compose --profile solo exec mneme-solo \
    mneme-cli --host $MNEME_HOST -u admin -p secret stats

# Metrics endpoint
curl -s http://localhost:9090/metrics | head -20
```

---

## 5. Mode 2: Core + Keepers Cluster

One Mnemosyne Core node (pure RAM, God node) with two Hypnos Keeper nodes
providing WAL persistence, snapshots, and cold storage. Keepers register with
the Core via the Herold protocol over mTLS on port 7379.

**Best for:** staging environments, durability testing, learning cluster
behavior.

### Start

```bash
docker compose --profile cluster up -d
```

### Architecture

```
┌───────────────────────────────────────────────────────────────┐
│  Docker network: mneme-net                                    │
│                                                               │
│  ┌─────────────────────────┐                                  │
│  │      mneme-core         │                                  │
│  │    (Mnemosyne God)      │                                  │
│  │  :6379 client TLS       │                                  │
│  │  :7379 replication mTLS │                                  │
│  │  :9090 metrics          │                                  │
│  └──────────┬──────────────┘                                  │
│             │  mTLS replication (Hermes)                       │
│      ┌──────┴──────┐                                          │
│      │             │                                          │
│  ┌───▼──────┐  ┌───▼──────┐                                   │
│  │ keeper-1 │  │ keeper-2 │                                   │
│  │ (Hypnos) │  │ (Hypnos) │                                   │
│  │  Aoide   │  │  Aoide   │  ← WAL (O_DIRECT + fallocate)    │
│  │  Melete  │  │  Melete  │  ← Snapshots                     │
│  │  Oneiros │  │  Oneiros │  ← Cold store (redb)             │
│  │  :9091   │  │  :9092   │  ← Prometheus metrics             │
│  └──────────┘  └──────────┘                                   │
└───────────────────────────────────────────────────────────────┘
```

### Topology

| Service | Container | Host Ports | Role |
|---|---|---|---|
| `mneme-core` | mneme-core | 6379 (client), 9090 (metrics) | God node -- pure RAM |
| `mneme-keeper-1` | mneme-keeper-1 | 9091 (metrics) | Keeper -- WAL + snapshots + cold |
| `mneme-keeper-2` | mneme-keeper-2 | 9092 (metrics) | Keeper -- WAL + snapshots + cold |

Port 7379 (replication mTLS) is **not** published to the host. All replication
traffic stays inside the `mneme-net` bridge network.

### Verify

```bash
# Check all three containers are healthy
docker compose --profile cluster ps

# Cluster info -- shows Core role, Raft term, warmup state
docker compose --profile cluster exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret cluster-info

# Keeper list -- shows connected keepers with WAL/disk stats
docker compose --profile cluster exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret keeper-list

# QUORUM write (default) -- requires floor(N/2)+1 keeper ACKs
docker compose --profile cluster exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret set test:key "hello"

# Read back
docker compose --profile cluster exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret get test:key
```

The cluster is fully operational when `cluster-info` shows
`warmup_state: Hot`. During startup, the warmup gate blocks QUORUM and ALL
reads until all keepers have completed their push phase.

---

## 6. Mode 3: Core + Keepers + Read Replica

Adds a read-replica Core node to Mode 2. The replica syncs state from the
primary Core via Hermes replication and serves EVENTUAL consistency reads only.
It does not participate in Raft elections.

**Best for:** read-heavy workloads, geographic read scaling, separating read
and write traffic.

### Start

```bash
docker compose --profile cluster --profile replica up -d
```

### Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│  Docker network: mneme-net                                           │
│                                                                      │
│  ┌────────────────────────┐        ┌────────────────────────┐        │
│  │     mneme-core         │        │    mneme-replica        │        │
│  │   (Primary God)        │───────▶│  (Read Replica God)     │        │
│  │  :6379 client TLS      │ sync   │  :6380 client TLS       │        │
│  │  :7379 rep mTLS        │        │  (EVENTUAL reads only)  │        │
│  │  :9090 metrics         │        │  :9095 metrics          │        │
│  └──────────┬─────────────┘        └────────────────────────┘        │
│             │  mTLS replication                                       │
│      ┌──────┴──────┐                                                  │
│  ┌───▼──────┐  ┌───▼──────┐                                          │
│  │ keeper-1 │  │ keeper-2 │                                          │
│  │  :9091   │  │  :9092   │                                          │
│  └──────────┘  └──────────┘                                          │
└──────────────────────────────────────────────────────────────────────┘
```

### Replica Configuration

The replica runs as a separate `mneme-core` process with `role = "read-replica"`
in its config (`docker/configs/replica.toml`). Key settings:

| Setting | Value | Description |
|---|---|---|
| `node.role` | `read-replica` | Accepts EVENTUAL reads only |
| `node.port` | `6380` | Client port (distinct from primary 6379) |
| `node.core_addr` | `mneme-core:7379` | Primary Core to sync from |
| `read_replicas.enabled` | `true` | Enables replica sync |
| `read_replicas.lag_alert_ms` | `50` | Alert threshold for replication lag |

### Ports

| Service | Host Port | Purpose |
|---|---|---|
| mneme-core | 6379 | Client TLS (read/write) |
| mneme-replica | 6380 | Client TLS (EVENTUAL reads only) |
| mneme-core | 9090 | Prometheus metrics |
| mneme-keeper-1 | 9091 | Prometheus metrics |
| mneme-keeper-2 | 9092 | Prometheus metrics |
| mneme-replica | 9095 | Prometheus metrics |

### Verify

```bash
# All four services should be healthy
docker compose --profile cluster --profile replica ps

# Write to primary
docker compose --profile cluster --profile replica exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret set replica:test "replicated"

# Read from replica (EVENTUAL consistency)
docker compose --profile cluster --profile replica exec mneme-replica \
    mneme-cli --host 127.0.0.1:6380 --host $MNEME_HOST -u admin -p secret get replica:test

# Check replication lag on the replica
curl -s http://localhost:9095/metrics | grep replication_lag

# Cluster info on replica shows role = read-replica
docker compose --profile cluster --profile replica exec mneme-replica \
    mneme-cli --host 127.0.0.1:6380 --host $MNEME_HOST -u admin -p secret cluster-info
```

**Important:** The replica only serves EVENTUAL consistency reads. Attempting
QUORUM or ALL reads against the replica will return an error. Route all writes
to the primary Core on port 6379.

---

## 7. Mode 4: Full Production Cluster

The complete production topology: one primary Core, three Keepers, two read
replicas, plus Prometheus and Grafana for observability. This mode activates
all Compose profiles.

**Best for:** production deployments, performance testing, full observability.

### Start

```bash
docker compose --profile full up -d
```

### Architecture

```
┌────────────────────────────────────────────────────────────────────────────┐
│  Docker network: mneme-net                                                 │
│                                                                            │
│  ┌───────────────────┐   sync   ┌──────────────┐  ┌──────────────┐        │
│  │    mneme-core      │────────▶│ mneme-replica │  │mneme-replica-2│        │
│  │  (Primary God)     │────────▶│ (Replica #1)  │  │ (Replica #2)  │        │
│  │  :6379 client      │         │ :6380 client  │  │ :6381 client  │        │
│  │  :7379 rep mTLS    │         │ :9095 metrics │  │ :9096 metrics │        │
│  │  :9090 metrics     │         └──────────────┘  └──────────────┘        │
│  └─────────┬──────────┘                                                    │
│            │ mTLS replication (Hermes)                                      │
│    ┌───────┼───────┐                                                       │
│    │       │       │                                                       │
│  ┌─▼────┐ ┌▼─────┐ ┌▼─────┐                                               │
│  │ k-1  │ │ k-2  │ │ k-3  │  ← 3 Keeper nodes (Hypnos)                   │
│  │:9091 │ │:9092 │ │:9093 │  ← WAL + Snapshots + Cold Store              │
│  └──────┘ └──────┘ └──────┘                                               │
│                                                                            │
│  ┌──────────────┐  ┌──────────────┐                                        │
│  │  Prometheus   │  │   Grafana    │                                        │
│  │  :9094        │  │   :3000      │                                        │
│  └──────────────┘  └──────────────┘                                        │
└────────────────────────────────────────────────────────────────────────────┘
```

### All Services and Ports

| Service | Container | Host Port | Container Port | Profile | Purpose |
|---|---|---|---|---|---|
| `mneme-core` | mneme-core | 6379 | 6379 | cluster | Primary Core (God node) |
| `mneme-keeper-1` | mneme-keeper-1 | 9091 | 9090 | cluster | Keeper #1 |
| `mneme-keeper-2` | mneme-keeper-2 | 9092 | 9090 | cluster | Keeper #2 |
| `mneme-keeper-3` | mneme-keeper-3 | 9093 | 9090 | cluster | Keeper #3 |
| `mneme-replica` | mneme-replica | 6380 | 6380 | replica | Read replica #1 |
| `mneme-replica-2` | mneme-replica-2 | 6381 | 6381 | replica | Read replica #2 |
| `prometheus` | prometheus | 9094 | 9090 | monitoring | Metrics aggregation |
| `grafana` | grafana | 3000 | 3000 | monitoring | Dashboards |
| `mneme-core` | mneme-core | 9090 | 9090 | cluster | Core metrics |

### Prometheus Setup

Prometheus scrapes all MnemeCache nodes automatically. The configuration is
at `docker/prometheus.yml`:

```yaml
global:
  scrape_interval: 15s

scrape_configs:
  - job_name: mnemecache
    static_configs:
      - targets:
          - mneme-core:9090
          - mneme-keeper-1:9090
          - mneme-keeper-2:9090
          - mneme-keeper-3:9090
          - mneme-replica:9090
          - mneme-replica-2:9090
```

Access Prometheus at `http://localhost:9094`.

Key metrics to monitor:

| Metric | Description |
|---|---|
| `pool_bytes_used` / `pool_bytes_max` | Memory pressure |
| `pressure_ratio` | Current memory utilization (alert at >0.70) |
| `replication_lag_ms` | Per-keeper replication lag |
| `requests_total` | Request throughput by command and consistency |
| `duration_histogram` | Latency percentiles |
| `connections_active` | Current client connections (Charon) |
| `wal_bytes` | WAL size per keeper (Aoide) |
| `cluster_term` | Current Raft term (Themis) |
| `evictions` | Eviction counts by type (lfu, ttl, oom) |

### Grafana Setup

Access Grafana at `http://localhost:3000` (default credentials: admin/admin).

1. Add Prometheus as a data source: URL = `http://prometheus:9090`
2. Import the MnemeCache dashboard (if provided), or create panels for the
   metrics listed above.

### Verify

```bash
# All services running
docker compose --profile full ps

# Cluster topology
docker compose --profile full exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret cluster-info

# All 3 keepers registered
docker compose --profile full exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret keeper-list

# QUORUM write (requires floor(3/2)+1 = 2 keeper ACKs)
docker compose --profile full exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret set prod:key "production"

# Read from each replica
docker compose --profile full exec mneme-replica \
    mneme-cli --host 127.0.0.1:6380 --host $MNEME_HOST -u admin -p secret get prod:key
docker compose --profile full exec mneme-replica-2 \
    mneme-cli --host 127.0.0.1:6381 --host $MNEME_HOST -u admin -p secret get prod:key

# Prometheus is scraping
curl -s http://localhost:9094/api/v1/targets | grep -c "up"

# Grafana is accessible
curl -s -o /dev/null -w "%{http_code}" http://localhost:3000/api/health
```

---

## 8. Mode 5: HA Cluster (3-Core Raft)

**Best for:** production environments requiring automatic failover and high availability.

Three Core nodes form a Raft consensus cluster. One is elected leader; the others
are followers. If the leader dies, Raft re-elects a new leader within ~3 seconds.
Writes go to the leader; reads can be served from any Core.

### Start

```bash
docker compose --profile ha up -d
```

### Architecture

```
┌──────────────────────────────────────────────────────────┐
│  mneme-core-1  :6379 (client)  :7379 (Raft mTLS)        │
│  mneme-core-2  :6382 (client)  :7379 (Raft mTLS)        │
│  mneme-core-3  :6383 (client)  :7379 (Raft mTLS)        │
│  mneme-ha-keeper-1  WAL + snapshots                      │
│  mneme-ha-keeper-2  WAL + snapshots                      │
│                                                          │
│  Leader election: Raft consensus (openraft)              │
│  Failover time: ~3 seconds                               │
│  Write path: client → leader Core → Keeper replication   │
│  Read path: client → any Core (EVENTUAL consistency)     │
└──────────────────────────────────────────────────────────┘
```

### Connect

```bash
# Connect to Core 1 (may or may not be the leader)
mneme-cli --host 127.0.0.1:6379 -u admin -p secret

# If Core 1 is a follower, writes return a LeaderRedirect error
# with the leader's address. Use that address instead.

# Connect to Core 2 or Core 3
mneme-cli --host 127.0.0.1:6382 -u admin -p secret
mneme-cli --host 127.0.0.1:6383 -u admin -p secret
```

### HA Test

```bash
# Run the HA test suite from inside Core 1
docker compose --profile ha exec mneme-core-1 bash /docker/test_ha.sh
```

### Exposed ports (HA profile)

| Service | Client Port | Metrics Port |
|---------|-------------|--------------|
| mneme-core-1 | 6379 | 9090 |
| mneme-core-2 | 6382 | 9097 |
| mneme-core-3 | 6383 | 9098 |

---

## 9. Configuration

### Environment Variables Reference

| Variable | Default | Applies To | Description |
|---|---|---|---|
| `MNEME_ADMIN_PASSWORD` | `secret` | All | Admin user password. **Change in production.** |
| `MNEME_CLUSTER_SECRET` | (from config) | All | Cluster join secret for node authentication |
| `MNEME_POOL_BYTES` | `512mb` | Core, Solo | Hot RAM pool size. Accepts: `256mb`, `1gb`, `2gb` |
| `KEEPER_POOL_BYTES` | `2gb` | Keepers | Per-keeper pool size |
| `MNEME_LOG_LEVEL` | `info` | All | Log verbosity: `trace`, `debug`, `info`, `warn`, `error` |
| `MNEME_CONFIG` | `/etc/mneme/solo.toml` | All | Path to config file inside the container |
| `MNEME_HOST_PORT` | `6379` | Core | Override the host-side client port |
| `MNEME_HOST_METRICS_PORT` | `9090` | Core | Override the host-side metrics port |
| `CORE_ADDR` | `mneme-core:7379` | Keepers, Replicas | Core replication address |
| `KEEPER_NODE_ID` | `keeper-N` | Keepers | Unique node identifier |

### Override Secrets (.env file)

Create a `.env` file in the repository root (**never commit this file**):

```bash
# .env
MNEME_ADMIN_PASSWORD=your-strong-password-here
MNEME_CLUSTER_SECRET=$(openssl rand -base64 32)
MNEME_POOL_BYTES=2gb
KEEPER_POOL_BYTES=4gb
MNEME_LOG_LEVEL=info
```

Or pass secrets inline:

```bash
MNEME_ADMIN_PASSWORD="$(openssl rand -base64 16)" \
MNEME_CLUSTER_SECRET="$(openssl rand -base64 32)" \
docker compose --profile cluster up -d
```

### Persistent Data

Named Docker volumes are created automatically:

| Volume | Service | Contents |
|---|---|---|
| `solo-data` | mneme-solo | WAL, snapshots, cold store, certs, users DB |
| `core-data` | mneme-core | Certs, users DB |
| `keeper1-data` | mneme-keeper-1 | WAL, snapshots, cold store (Oneiros) |
| `keeper2-data` | mneme-keeper-2 | WAL, snapshots, cold store (Oneiros) |
| `keeper3-data` | mneme-keeper-3 | WAL, snapshots, cold store (Oneiros) |
| `replica-data` | mneme-replica | Replica sync state |
| `certs` | All (cluster) | Shared CA certificate for mTLS |

To use bind-mounts instead of named volumes for easier inspection:

```yaml
# docker-compose.override.yml
services:
  mneme-core:
    volumes:
      - ./data/core:/var/lib/mneme
  mneme-keeper-1:
    volumes:
      - ./data/keeper1:/var/lib/mneme
      - certs:/certs:ro
```

### Custom Configs

Override any config by mounting your own TOML file:

```yaml
# docker-compose.override.yml
services:
  mneme-core:
    volumes:
      - ./my-core.toml:/etc/mneme/core.toml:ro
    environment:
      MNEME_CONFIG: /etc/mneme/core.toml
```

Default configs shipped in the image are located at `/etc/mneme/`:

| File | Used By |
|---|---|
| `solo.toml` | Solo node |
| `core.toml` | Primary Core |
| `keeper-1.toml` | Keeper 1 |
| `keeper-2.toml` | Keeper 2 |
| `keeper-3.toml` | Keeper 3 |
| `replica.toml` | Read replica |

---

## 9. Testing

### Smoke Tests (65 checks)

The smoke test covers connectivity, string/KV operations, counters, hashes,
lists, sorted sets, MGET/MSET, SCAN, DB namespacing, auth tokens, user
management, observability commands, JSON operations, and TTL expiry.

**Solo mode:**

```bash
docker compose --profile solo up -d
docker compose --profile solo exec mneme-solo \
    bash /docker/smoke-test.sh
```

**Cluster mode:**

```bash
docker compose --profile cluster up -d

# Wait for warmup to complete, then run smoke tests
docker compose --profile cluster --profile cluster-test \
    run --rm smoke-test
```

The smoke-test container includes an internal 60-second wait loop, so the
cluster does not need to be fully healthy before the test starts.

### Integration Tests

Covers DB namespacing, QUORUM writes, join-token format, keeper-list stats,
data type replication, TTL/expiry, MGET/MSET bulk ops, SCAN patterns, Core
restart survival, config-set, and user management (RBAC).

```bash
# As a standalone container
docker compose --profile cluster up -d
docker compose --profile cluster --profile cluster-test \
    run --rm integration-test

# Or exec into the running Core
docker compose --profile cluster exec mneme-core \
    bash /docker/integration-test.sh
```

### Crash Recovery Tests

**Keeper crash and recovery:**

Tests WAL replay after restart, kill-9 mid-write corruption handling, all
keepers down (QUORUM fails), and partial keeper loss (QUORUM still works with
floor(N/2)+1 keepers).

```bash
docker compose --profile cluster up -d
docker compose --profile cluster exec mneme-core \
    bash /docker/test_keeper_crash.sh
```

**Core crash and recovery:**

Tests warmup gate transitions (Cold to Warming to Hot), data survival after
Core restart (keepers push data back), and client reconnection.

```bash
docker compose --profile cluster up -d
docker compose --profile cluster exec mneme-core \
    bash /docker/test_core_crash.sh
```

### TLS Tests

Verifies TLS 1.3 configuration, certificate validation, and mTLS between
Core and Keepers.

```bash
docker compose --profile cluster exec mneme-core \
    bash /docker/test_tls.sh
```

### Stress Tests

Covers write throughput (10k EVENTUAL writes, 1k QUORUM writes), mixed
concurrent workloads (SET/GET/DEL), large payload handling (1MB values), and
bulk operations (MSET/MGET with 100 keys per batch).

```bash
docker compose --profile cluster up -d
docker compose --profile cluster exec mneme-core \
    bash /docker/test_stress.sh
```

---

## 10. Resource Limits

For production deployments, set explicit resource constraints in
`docker-compose.override.yml`:

```yaml
services:
  mneme-core:
    deploy:
      resources:
        limits:
          cpus: "4.0"
          memory: "4G"
        reservations:
          cpus: "1.0"
          memory: "2G"

  mneme-keeper-1:
    deploy:
      resources:
        limits:
          cpus: "2.0"
          memory: "6G"
        reservations:
          cpus: "0.5"
          memory: "3G"

  mneme-keeper-2:
    deploy:
      resources:
        limits:
          cpus: "2.0"
          memory: "6G"
        reservations:
          cpus: "0.5"
          memory: "3G"

  mneme-keeper-3:
    deploy:
      resources:
        limits:
          cpus: "2.0"
          memory: "6G"
        reservations:
          cpus: "0.5"
          memory: "3G"

  mneme-replica:
    deploy:
      resources:
        limits:
          cpus: "2.0"
          memory: "2G"
        reservations:
          cpus: "0.5"
          memory: "1G"

  mneme-replica-2:
    deploy:
      resources:
        limits:
          cpus: "2.0"
          memory: "2G"
        reservations:
          cpus: "0.5"
          memory: "1G"
```

**Sizing guidelines:**

- **Core (Mnemosyne):** All hot data lives in RAM. Set `memory` limit to at
  least `MNEME_POOL_BYTES` + 512MB overhead. CPU scales with request rate.
- **Keepers (Hypnos):** Need RAM for their local pool plus disk I/O buffers.
  WAL writes use O_DIRECT, so kernel page cache overhead is minimal.
- **Replicas:** Mirror the Core's RAM requirements for the hot set, but with
  lower CPU since they only serve reads.

---

## 11. Networking

### Bridge Network

All services run on a single Docker bridge network named `mneme-net`. Services
resolve each other by container hostname (e.g., `mneme-core`, `mneme-keeper-1`).

```yaml
networks:
  mneme-net:
    driver: bridge
```

### Port Mapping

| Port | Protocol | Exposure | Description |
|---|---|---|---|
| 6379 | TLS 1.3 | Published to host | Client connections (primary Core) |
| 6380 | TLS 1.3 | Published to host | Client connections (replica #1) |
| 6381 | TLS 1.3 | Published to host | Client connections (replica #2) |
| 7379 | mTLS | Internal only | Replication between Core and Keepers (Hermes) |
| 7380 | mTLS | Internal only | Replica replication port |
| 9090 | HTTP | Published to host | Core Prometheus metrics (Aletheia) |
| 9091-9093 | HTTP | Published to host | Keeper Prometheus metrics |
| 9094 | HTTP | Published to host | Prometheus UI (monitoring profile) |
| 9095-9096 | HTTP | Published to host | Replica Prometheus metrics |
| 3000 | HTTP | Published to host | Grafana UI (monitoring profile) |

### TLS

MnemeCache uses rustls (TLS 1.3 only, no OpenSSL).

- **Client connections (6379/6380/6381):** TLS 1.3. The Core auto-generates a
  self-signed CA and node certificate on first boot (`auto_generate = true` in
  `tls` config). The CA cert is written to the shared `certs` volume so Keepers
  and replicas can trust it.
- **Replication (7379):** mTLS. Both sides present certificates signed by the
  same CA. Managed automatically by the Aegis TLS layer.
- **Metrics (9090):** Plain HTTP. Intended for internal scraping by Prometheus.

For production, mount your own certificates:

```yaml
# docker-compose.override.yml
services:
  mneme-core:
    volumes:
      - ./certs/ca.crt:/var/lib/mneme/ca.crt:ro
      - ./certs/core.crt:/var/lib/mneme/node.crt:ro
      - ./certs/core.key:/var/lib/mneme/node.key:ro
    environment:
      MNEME_CONFIG: /etc/mneme/core.toml
```

Update the corresponding `[tls]` section in your config to set
`auto_generate = false`.

---

## 12. Tear Down

### Stop containers (preserves volumes and data)

```bash
# Solo
docker compose --profile solo down

# Cluster
docker compose --profile cluster down

# Cluster + replica
docker compose --profile cluster --profile replica down

# Full production
docker compose --profile full down
```

### Stop and remove all volumes (destroys all data)

```bash
docker compose --profile solo down -v
docker compose --profile cluster down -v
docker compose --profile full down -v
```

### Remove the built image

```bash
docker rmi mnemelabs/core:1.0.0
```

### Full cleanup (containers, volumes, networks, images)

```bash
docker compose --profile full down -v --rmi local --remove-orphans
```

---

## 13. Troubleshooting

### Container fails to start

```bash
# Check logs for the failing service
docker compose --profile cluster logs mneme-core
docker compose --profile cluster logs mneme-keeper-1

# Common causes:
# - Port conflict: another service is using 6379 or 9090
# - Permission denied: entrypoint scripts need to create /var/lib/mneme dirs
# - Missing CA cert: keepers wait up to 120s for the Core to generate certs
```

### Keepers not connecting

```bash
# Verify Core is listening on replication port
docker compose --profile cluster exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret cluster-info

# Check keeper logs for connection errors
docker compose --profile cluster logs mneme-keeper-1 | grep -i "error\|connect"

# Common causes:
# - Core not yet healthy (keepers wait and retry)
# - Join token mismatch between Core and Keeper configs
# - TLS cert not yet available on the shared certs volume
```

### Warmup state stuck on Cold or Warming

```bash
# Check warmup state
docker compose --profile cluster exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret cluster-info

# The warmup gate blocks QUORUM/ALL reads until all registered keepers
# complete their push phase. If a keeper is down, warmup cannot complete.
# Verify all keepers are running:
docker compose --profile cluster ps
```

### QUORUM writes timing out

```bash
# Check keeper connectivity
docker compose --profile cluster exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret keeper-list

# QUORUM requires floor(N/2)+1 keeper ACKs.
# With 2 keepers: both must ACK. With 3 keepers: 2 must ACK.
# If a keeper is unreachable, writes may fail or timeout.

# Check replication lag
curl -s http://localhost:9090/metrics | grep replication_lag
```

### Read replica not syncing

```bash
# Check replica logs
docker compose --profile cluster --profile replica logs mneme-replica

# Verify replica can reach Core on port 7379
docker compose --profile cluster --profile replica exec mneme-replica \
    mneme-cli --host 127.0.0.1:6380 --host $MNEME_HOST -u admin -p secret cluster-info

# Common causes:
# - Core not healthy yet (replica waits for CA cert, then connects)
# - Incorrect core_addr in replica config
# - Network partition inside Docker bridge
```

### Health check failing

The health check probes `http://127.0.0.1:9090/metrics` using curl. If it
fails:

```bash
# Check if the metrics endpoint is responding inside the container
docker compose --profile cluster exec mneme-core \
    curl -sf http://127.0.0.1:9090/metrics

# Check health status
docker inspect --format='{{.State.Health.Status}}' mneme-core
docker inspect --format='{{json .State.Health.Log}}' mneme-core | python3 -m json.tool
```

### Out of memory

If a container is OOM-killed:

```bash
# Check memory pressure
docker compose --profile cluster exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret pool-stats

# Increase pool size
docker compose --profile cluster down
MNEME_POOL_BYTES=2gb docker compose --profile cluster up -d

# Or scale hot pool at runtime (no restart needed)
docker compose --profile cluster exec mneme-core \
    mneme-cli --host $MNEME_HOST -u admin -p secret config-set memory.pool_bytes 2147483648
```

Lethe (eviction engine) triggers proactive LFU eviction at the configured
`eviction_threshold` (default 0.90 = 90% pressure) and emergency OOM eviction
at 100% pressure.

### Logs and debugging

```bash
# Tail all logs
docker compose --profile cluster logs -f

# Tail a specific service
docker compose --profile cluster logs -f mneme-keeper-2

# Enable debug logging
docker compose --profile cluster down
MNEME_LOG_LEVEL=debug docker compose --profile cluster up -d

# Enable trace logging (very verbose)
MNEME_LOG_LEVEL=trace docker compose --profile cluster up -d
```
