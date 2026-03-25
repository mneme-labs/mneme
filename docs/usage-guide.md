# MnemeCache Usage Guide

The complete zero-to-hero guide for MnemeCache -- a distributed in-memory cache
built in Rust for Linux. This document takes you from first boot to a
fully-operational production cluster. Read it top-to-bottom once, then use the
section headings as a reference.

---

## Table of Contents

1. [What is MnemeCache?](#1-what-is-mnemecache)
   - [1.1 Architecture Overview](#11-architecture-overview)
   - [1.2 Greek Mythology Naming](#12-greek-mythology-naming)
   - [1.3 Cluster Modes at a Glance](#13-cluster-modes-at-a-glance)
2. [Quick Start](#2-quick-start)
   - [2.1 Docker (30 Seconds)](#21-docker-30-seconds)
   - [2.2 Linux Native](#22-linux-native)
3. [Installation](#3-installation)
   - [3.1 Prerequisites](#31-prerequisites)
   - [3.2 Build from Source](#32-build-from-source)
   - [3.3 Docker Image](#33-docker-image)
4. [Cluster Modes -- Detailed](#4-cluster-modes----detailed)
   - [4.1 Solo Mode](#41-solo-mode)
   - [4.2 Core + Keepers](#42-core--keepers)
   - [4.3 Core + Keepers + Read Replica](#43-core--keepers--read-replica)
   - [4.4 Full Production Cluster](#44-full-production-cluster)
5. [Authentication & Session Tokens](#5-authentication--session-tokens)
6. [User Management & RBAC](#6-user-management--rbac)
7. [CLI Profiles](#7-cli-profiles)
8. [Data Commands](#8-data-commands)
   - [8.1 String Commands](#81-string-commands)
   - [8.2 Counter Commands](#82-counter-commands)
   - [8.3 Hash Commands](#83-hash-commands)
   - [8.4 List Commands](#84-list-commands)
   - [8.5 Sorted Set Commands](#85-sorted-set-commands)
   - [8.6 JSON Commands](#86-json-commands)
9. [Bulk Operations](#9-bulk-operations)
10. [Key Discovery](#10-key-discovery)
11. [Database Namespacing](#11-database-namespacing)
12. [Consistency Levels](#12-consistency-levels)
13. [Admin & Observability](#13-admin--observability)
14. [Memory Management & Eviction](#14-memory-management--eviction)
15. [Deployment Guide Links](#15-deployment-guide-links)
16. [Client Libraries](#16-client-libraries)
    - [16.1 Rust -- Pontus (included)](#161-rust----pontus-included)
    - [16.2 Python](#162-python)
    - [16.3 Node.js / TypeScript](#163-nodejs--typescript)
    - [16.4 Go](#164-go)
    - [16.5 Java](#165-java)
    - [16.6 Wire Protocol Reference](#166-wire-protocol-reference)
17. [Testing & Validation](#17-testing--validation)
    - [17.1 Smoke Tests](#171-smoke-tests)
    - [17.2 Integration Tests](#172-integration-tests)
    - [17.3 Keeper Crash Recovery](#173-keeper-crash-recovery)
    - [17.4 Core Crash Recovery](#174-core-crash-recovery)
    - [17.5 Data Persistence Verification](#175-data-persistence-verification)
    - [17.6 Consistency Level Testing](#176-consistency-level-testing)
    - [17.7 TLS Validation](#177-tls-validation)
    - [17.8 Stress Testing](#178-stress-testing)
18. [CLI Command Reference](#18-cli-command-reference)
19. [Troubleshooting](#19-troubleshooting)
20. [Glossary](#20-glossary)

---

## 1. What is MnemeCache?

MnemeCache is a distributed in-memory cache written in Rust (edition 2021),
designed exclusively for Linux 5.19+. It provides sub-millisecond reads, strong
consistency guarantees, and persistence through a tiered storage architecture:
hot data lives in RAM on the God node, while Keeper nodes provide durable
storage via write-ahead logs, periodic snapshots, and a cold-store B-tree.

Key characteristics:

- **Protocol**: 16-byte binary header + MessagePack payloads over TLS 1.3
- **TLS**: rustls with auto-generated certificates (rcgen) -- no OpenSSL
- **Auth**: HMAC-SHA256 session tokens with CSPRNG-generated JTI
- **Consistency**: four levels from fire-and-forget to full-cluster ACK
- **Key types**: String, Hash, List, Sorted Set, JSON
- **Eviction**: LFU Morris counters with a 3-level TTL timing wheel
- **Replication**: mTLS multiplexed fabric -- one connection per Keeper
- **Leader election**: Raft via openraft

### 1.1 Architecture Overview

```
                        Clients
                          |
                     TLS 1.3 :6379
                          |
                  +-------+-------+
                  |   God Node    |       "Core" / Mnemosyne
                  | (pure RAM)    |       Raft leader, slot router,
                  | mneme-core    |       session auth, eviction
                  +---+---+---+---+
                      |   |   |
                 mTLS :7379 (replication fabric -- Hermes)
                 /    |       \
           +-----+ +-----+ +-----+
           | K-1 | | K-2 | | K-3 |     Keeper nodes (Hypnos)
           +-----+ +-----+ +-----+     WAL + snapshot + cold store
                                        mneme-keeper

           +--------+                   Read Replica (optional)
           | R-1    |                   EVENTUAL reads only
           +--------+                   mneme-core --role read-replica


    Prometheus scrape :9090 (plain HTTP) on every node
```

**God node** (Core): holds all hot data in RAM. Never touches disk. Accepts
all client connections on port 6379 over TLS 1.3. Replicates writes to
Keepers over mTLS on port 7379.

**Keeper nodes**: persist data via WAL (Aoide, O_DIRECT + fallocate),
periodic snapshots (Melete), and a cold-store redb B-tree (Oneiros). On Core
restart, Keepers push all keys back during the warm-up phase.

**Read replicas**: optional God nodes that sync from the primary Core and
serve EVENTUAL-consistency reads. They reduce read load on the primary but
do not participate in write quorum.

### 1.2 Greek Mythology Naming

Every component is named after a figure from Greek mythology. This table maps
each name to its source file and purpose.

| Name | Source File | Purpose |
|------|------------|---------|
| **Mnemosyne** | `mneme-core/src/core/mnemosyne.rs` | God node -- unified RAM pool, main loop |
| **Hypnos** | `mneme-keeper/src/keeper/hypnos.rs` | Keeper node -- persistence coordinator |
| **Aoide** | `mneme-keeper/src/keeper/aoide.rs` | WAL engine (O_DIRECT + fallocate) |
| **Melete** | `mneme-keeper/src/keeper/melete.rs` | Snapshot engine |
| **Oneiros** | `mneme-keeper/src/keeper/oneiros.rs` | Cold store (redb B-tree) |
| **Lethe** | `mneme-core/src/core/lethe.rs` | Eviction -- LFU Morris + TTL wheel |
| **Iris** | `mneme-core/src/core/iris.rs` | Slot router (CRC16 % 16384) |
| **Moirai** | `mneme-core/src/core/moirai.rs` | Consistency dispatcher + KeeperInfo registry |
| **Hermes** | `mneme-core/src/net/hermes.rs` | Replication fabric -- mTLS + multiplex |
| **Themis** | `mneme-core/src/cluster/themis.rs` | Raft leader election (openraft) |
| **Herold** | `mneme-core/src/cluster/herold.rs` | Node registration daemon (Core-side) |
| **Charon** | `mneme-core/src/net/charon.rs` | Connection manager + backpressure |
| **Argus** | `mneme-core/src/auth/argus.rs` | Session tokens (HMAC-SHA256) |
| **Aegis** | `mneme-core/src/net/aegis.rs` | TLS 1.3 (rustls) |
| **Aletheia** | `mneme-core/src/obs/aletheia.rs` | Metrics -- Prometheus + perf_event_open |
| **Delphi** | `mneme-core/src/obs/delphi.rs` | SLOWLOG + MONITOR |
| **Nemesis** | `mneme-core/src/error.rs` | Error types + wire error codes |
| **Pontus** | `mneme-client/src/` | Client library (connection pool) |

### 1.3 Cluster Modes at a Glance

| Mode | Binaries | Persistence | Consistency | Use Case |
|------|----------|-------------|-------------|----------|
| **Solo** | `mneme-core` | RAM only (no Keepers) | EVENTUAL only | Dev, CI, single-server cache |
| **Core + Keepers** | `mneme-core` + N x `mneme-keeper` | WAL + snapshots + cold store | EVENTUAL, ONE, QUORUM, ALL | Production with durability |
| **+ Read Replica** | above + `mneme-core --role read-replica` | Replica syncs from Core | EVENTUAL reads on replica | Read scaling |
| **Full Production** | Multiple of each | Full persistence + replicas | All levels | High availability at scale |

---

## 2. Quick Start

### 2.1 Docker (30 Seconds)

Start a single-node MnemeCache instance with one command:

```bash
docker run -d \
  --name mneme \
  -p 6379:6379 \
  -p 9090:9090 \
  -e MNEME_ADMIN_PASSWORD=secret \
  -v mneme-data:/var/lib/mneme \
  mnemelabs/core:1.0.0
```

Wait about 10 seconds for startup, then connect:

```bash
# From inside the container
docker exec mneme mneme-cli --host $MNEME_HOST -u admin -p secret stats

# From the host (if port 6379 is mapped)
mneme-cli --host 127.0.0.1:6379 --host $MNEME_HOST -u admin -p secret stats
```

Write and read your first key:

```bash
docker exec mneme mneme-cli --host $MNEME_HOST -u admin -p secret set hello world
docker exec mneme mneme-cli --host $MNEME_HOST -u admin -p secret get hello
# -> "world"
```

Run the built-in smoke test (65+ checks):

```bash
docker exec mneme bash /docker/smoke-test.sh
```

### 2.2 Linux Native

```bash
# Clone and build
git clone https://github.com/mneme-labs/mneme && cd mneme
cargo build --release

# Start a solo node (generates TLS certs automatically)
sudo ./target/release/mneme-core --config /etc/mneme/core.toml

# Or use the setup script (interactive -- handles everything)
sudo ./scripts/setup-linux.sh
# Choose: 1) Solo node
# Save the admin password printed at the end

# Connect
mneme-cli --host localhost:6379 -u admin -p <PASSWORD> stats
```

---

## 3. Installation

MnemeCache requires **Linux 5.19+**. It uses `O_DIRECT`, `perf_event_open`, and
Linux-specific kernel APIs. There is no macOS or Windows native build.

### 3.1 Prerequisites

#### Minimum kernel version

```bash
uname -r      # must be >= 5.19
```

Ubuntu 22.04 ships 5.15 -- upgrade the HWE kernel first:

```bash
sudo apt install --install-recommends linux-generic-hwe-22.04
sudo reboot
```

#### System privileges required

| What | Why |
|------|-----|
| `sudo` / root during setup | Installs packages, writes to `/etc`, `/var/lib`, installs systemd units, tunes sysctl |
| `sudo systemctl` | Start / stop / status for `mneme-core`, `mneme-keeper-*`, `mneme-replica-*` |
| `perf_event_paranoid <= 1` | `perf_event_open` for hardware counter metrics (Aletheia) |
| Huge pages enabled | `vm.nr_hugepages` tuned by setup script for hot-set performance |

After installation, daemons run as a dedicated `mneme` system user (no-login,
no shell). Application users authenticate via session tokens -- no special OS
permissions needed.

#### Linux user created by setup

```
User:  mneme
Shell: /usr/sbin/nologin
Home:  /var/lib/mneme
```

All data directories are owned by `mneme:mneme` with mode `0750`.

#### Filesystem paths created

| Path | Owner | Purpose |
|------|-------|---------|
| `/etc/mneme/` | `root:mneme` `0750` | Config files (`core.toml`, `keeper-N.toml`) |
| `/etc/mneme/ca.crt` | `root:mneme` `0640` | CA certificate -- copy to clients |
| `/etc/mneme/node.crt` | `root:mneme` `0640` | Server TLS certificate |
| `/etc/mneme/node.key` | `root:mneme` `0640` | Server TLS private key |
| `/var/lib/mneme/` | `mneme:mneme` `0750` | Data root |
| `/var/lib/mneme/users.db` | `mneme:mneme` `0600` | User credentials (Core only) |
| `/var/lib/mneme/keeper-N/` | `mneme:mneme` `0750` | WAL, snapshot, Oneiros store per Keeper |
| `/var/log/mneme/` | `mneme:mneme` `0750` | Optional structured log output |
| `/run/mneme/` | `mneme:mneme` `0750` | PID files |

> **Tip:** The `mneme-cli` tool reads `~/.mneme/profiles.toml` (owned by
> whichever OS user runs it). No root access is needed to use the CLI after
> setup.

#### Ports

| Port | Protocol | Used by | Reachable from |
|------|----------|---------|----------------|
| **6379** | TLS 1.3 | Client traffic (Core) | Clients / application servers |
| **7379** | mTLS | Replication fabric (Core <-> Keepers) | Internal cluster only -- firewall from outside |
| **9090** | HTTP (plain) | Prometheus metrics scrape | Monitoring network |

Open port 6379 in your firewall/security group. Keep 7379 private.

#### CA certificate -- client side

Every client (application, `mneme-cli`, Pontus pool) must trust the cluster CA.
The setup script prints the path at the end. Copy it to your application
servers:

```bash
# On the MnemeCache server
sudo cat /etc/mneme/ca.crt

# On each application server
scp mneme-server:/etc/mneme/ca.crt /etc/ssl/certs/mneme-ca.crt
# Then point PoolConfig::tls_ca_cert / --ca-cert to that path
```

### 3.2 Build from Source

```bash
# Ubuntu 22.04+ / Debian 12+ / RHEL 9+ / any Linux 5.19+
git clone https://github.com/mneme-labs/mneme
cd mneme
sudo ./scripts/setup-linux.sh          # interactive -- prompts for topology
```

The setup script:
1. Installs build tools and runtime packages via `apt`/`dnf`
2. Asks **all** topology questions up-front (node IDs, RAM allocation, keeper
   count) so you can walk away once compiling starts
3. Builds with `RUSTFLAGS='-C target-cpu=native'` (native CPU tuning)
4. Generates CA and node TLS certificates (rcgen -- no OpenSSL required)
5. Writes TOML configs to `/etc/mneme/`
6. Creates and enables systemd services
7. Tunes `vm.nr_hugepages`, `net.core.somaxconn`, `perf_event_paranoid`
8. Starts all services and prints the admin password **once** -- save it

#### Manual build (without setup script)

```bash
cargo build --release
sudo cp target/release/{mneme-core,mneme-keeper,mneme-cli} /usr/local/bin/
```

Configure and manage services manually following Section 4.

### 3.3 Docker Image

```bash
# Production runtime (default -- ~30 MB)
docker build -t mnemelabs/core:1.0.0 .

# Development image (full Rust toolchain + cargo-watch)
docker build --target dev -t mnemelabs/core:dev .
```

Docker Compose cluster (Core + 3 Keepers):

```bash
docker compose build
ADMIN_PASS=secret docker compose up -d

# Verify
docker exec mneme-core mneme-cli --host $MNEME_HOST -u admin -p secret cluster-info

# Run smoke tests
docker compose --profile test run --rm smoke-test

# Tear down
docker compose down -v
```

Docker Compose services:

| Service | Port (host) | Description |
|---------|-------------|-------------|
| `mneme-core` | 6379 (client), 9090 (metrics) | Core node |
| `mneme-keeper-1` | 9091 (metrics) | Keeper 1 |
| `mneme-keeper-2` | 9092 (metrics) | Keeper 2 |
| `mneme-keeper-3` | 9093 (metrics) | Keeper 3 |

Persistent volumes: `core-data`, `core-etc` (Core), `keeper1-data` through
`keeper3-data` (Keepers).

Override configs via bind mounts:

```yaml
volumes:
  - ./my-core.toml:/etc/mneme/core.toml:ro
```

---

## 4. Cluster Modes -- Detailed

### 4.1 Solo Mode

Solo mode runs Core in a single process with no Keepers. All data lives in RAM
only. Use it for development, single-server deployments, and CI pipelines.

**When to use:** local development, CI/CD pipelines, prototyping, single-server
caches where persistence is not required.

**Limitations:** no persistence (data lost on restart), no QUORUM/ALL
consistency (no Keepers to ACK), no read replicas.

#### Config (`core.toml`)

```toml
[node]
id = "solo-dev"
role = "core"

[network]
client_addr = "0.0.0.0:6379"
rep_addr    = "0.0.0.0:7379"

[memory]
pool_bytes = "512mb"

[tls]
auto_generate = true
server_name   = "mneme.local"

[auth]
admin_password = "changeme"
cluster_secret = "random-secret-here"
token_ttl_h    = 24
```

#### Start command

```bash
# Via setup script
sudo ./scripts/setup-linux.sh
# Choose: 1) Solo node

# Or manually
mneme-core --config /etc/mneme/core.toml

# Check the service
sudo systemctl status mneme-core
```

#### Verify

```bash
mneme-cli --host localhost:6379 -u admin -p <PASSWORD> stats
# -> keys=0 pool_used=0 pool_max=536870912 keepers=0 pool_ratio=0.00

mneme-cli --host localhost:6379 -u admin -p <PASSWORD> set hello world
mneme-cli --host localhost:6379 -u admin -p <PASSWORD> get hello
# -> "world"
```

On first boot, `auto_generate = true` creates `ca.crt`, `node.crt`, and
`node.key` automatically via rcgen (no OpenSSL required).

### 4.2 Core + Keepers

Add Keeper nodes for persistence and QUORUM/ALL consistency. Writes are
replicated from Core to Keepers over mTLS. On Core restart, Keepers push all
keys back during warm-up.

**When to use:** production workloads that need durability and strong consistency.

#### Architecture

```
  Clients --TLS-->  Core (Mnemosyne)   <-- pure RAM, Raft leader
                         |
                    mTLS (replication)
                  +------+------+
                  v      v      v
             Keeper-1  Keeper-2  Keeper-3   <-- WAL + snapshots + cold B-tree
```

#### Step 1 -- Set up the Core node

```bash
# On the Core machine
git clone https://github.com/mneme-labs/mneme && cd mneme
sudo ./scripts/setup-linux.sh
# Choose: 2) Core node
# Enter: node ID (e.g. mneme-core-prod), RAM pool, data directory

# The script:
#   - Generates CA cert + node TLS certificate
#   - Starts mneme-core.service
#   - Prints admin password, cluster secret, join token, and JOIN BUNDLE
```

The script prints a **JOIN BUNDLE** -- one copy-pasteable line that encodes the
CA cert + cluster secret + join token. **Save it.**

```bash
# Verify Core is up
mneme-cli -H 10.0.0.1:6379 -u admin -p <PASSWORD> cluster-info
```

#### Step 2 -- Add Keeper nodes

On each Keeper machine:

```bash
git clone https://github.com/mneme-labs/mneme && cd mneme
sudo TOPOLOGY=keeper KEEPER_COUNT=1 CORE_ADDR=10.0.0.1:7379 \
     ./scripts/setup-linux.sh
# Choose: 1) Join bundle  <-- paste the JOIN BUNDLE from Step 1
# Enter: node ID (e.g. keeper-nyc-0), WAL pool size, data directory
```

The Keeper registers with Core automatically over mTLS via Herold. The
registration flow:

1. Keeper (Hypnos) connects outbound to Core rep_port (7379) over mTLS
2. Hypnos sends SyncStart { node_id, key_count, replication_addr }
3. Core validates join_token and connects back to the Keeper
4. Core adds the Keeper to the Moirai dispatch map
5. Hypnos pushes all existing keys via PushKey frames
6. Hypnos sends SyncComplete; warmup counter decrements

#### Step 3 -- Verify the cluster

```bash
mneme-cli -u admin -p <PASSWORD> cluster-info
# -> leader, live_nodes, raft_term, warmup_state, memory_pressure,
#    replication_lag_ms per Keeper, supported_modes

mneme-cli -u admin -p <PASSWORD> keeper-list
# -> Node Name, Address, Pool (grant), Used
```

### 4.3 Core + Keepers + Read Replica

Read replicas handle EVENTUAL reads, scaling read throughput without adding
write latency.

#### Architecture

```
  Clients --TLS-->  Core (primary)   <-- all writes, QUORUM/ALL reads
                    /     |     \
               mTLS      mTLS    mTLS
               /          |        \
          Keeper-1    Keeper-2    Keeper-3

  Clients --TLS-->  Replica-1   <-- EVENTUAL reads only
                    (syncs from Core)
```

#### Add a read replica

```bash
git clone https://github.com/mneme-labs/mneme && cd mneme
sudo TOPOLOGY=replica CORE_ADDR=10.0.0.1:7379 \
     ./scripts/setup-linux.sh
# Choose: 1) Join bundle  <-- paste the JOIN BUNDLE
# Enter: node ID (e.g. replica-eu-0), RAM pool, data directory
```

#### Route reads to the replica

```bash
# Clients use --consistency eventual to hit the replica
mneme-cli --host replica-eu-0:6380 get leaderboard --consistency eventual

# QUORUM/ALL reads still go to the primary Core
mneme-cli --host 10.0.0.1:6379 get balance:user:7 --consistency quorum
```

Read replicas sync from Core with eventual consistency. They may be a few
milliseconds behind the primary. Data written with EVENTUAL consistency may
not yet be visible on the replica.

### 4.4 Full Production Cluster

A full production deployment combines all components for high availability.

#### Architecture

```
                       Load Balancer / DNS
                       /        |        \
                      /         |         \
              Core (primary)  Replica-1  Replica-2
              (writes + QUORUM reads)    (EVENTUAL reads)
                  |
             mTLS :7379
             / | \ \  \
           K1 K2 K3 K4 K5
           (5 Keepers for QUORUM=3)
```

#### Capacity planning

| Component | Recommendation | Notes |
|-----------|---------------|-------|
| Core RAM | 2x your expected hot-set size | Leaves room for eviction headroom |
| Keeper count | 3 minimum (QUORUM=2) | 5 for stronger durability |
| Keeper disk | 3x expected data size | WAL + snapshots + cold store |
| Read replicas | 1 per read-heavy region | Scale reads horizontally |
| Network | 10 Gbps between Core and Keepers | Replication is bandwidth-sensitive |

#### Complete walkthrough

```bash
# 1. Core node
sudo ./scripts/setup-linux.sh
# Choose: 2) Core node
# Set pool_bytes to your target hot-set size
# Save the JOIN BUNDLE

# 2. Keepers (repeat on 3-5 machines)
sudo TOPOLOGY=keeper CORE_ADDR=10.0.0.1:7379 ./scripts/setup-linux.sh
# Paste the JOIN BUNDLE for each

# 3. Read replicas (optional, repeat per region)
sudo TOPOLOGY=replica CORE_ADDR=10.0.0.1:7379 ./scripts/setup-linux.sh

# 4. Verify everything
mneme-cli -u admin -p <PASSWORD> cluster-info
mneme-cli -u admin -p <PASSWORD> keeper-list

# 5. Create application users (don't share the admin password)
mneme-cli -u admin -p <PASSWORD> user-create app-service hunter2 --role readwrite
mneme-cli -u admin -p <PASSWORD> user-create analytics reader123 --role readonly

# 6. Set up monitoring
curl http://10.0.0.1:9090/metrics | head -20
```

#### Hot-reload configuration

No restart required for most config changes:

```bash
# Increase hot RAM pool on the fly
mneme-cli -u admin -p <PASSWORD> config-set memory.pool_bytes 4gb

# Adjust eviction threshold
mneme-cli -u admin -p <PASSWORD> config-set memory.eviction_threshold 0.85

# Verify
mneme-cli -u admin -p <PASSWORD> config memory.pool_bytes
# -> 4294967296
```

---

## 5. Authentication & Session Tokens

MnemeCache uses short-lived HMAC-SHA256 session tokens (default TTL: 24 hours).
Token JTI values are generated with CSPRNG (`rand::thread_rng`) for security.

### Login and obtain a token

```bash
# Returns a token you pass with -t / --token
mneme-cli --host localhost:6379 -u admin -p mypassword auth-token
# -> eyJ... (token)

# Store it for the session
export MNEME_TOKEN=$(mneme-cli -u admin -p mypassword auth-token)

# All subsequent commands pick it up from the environment
mneme-cli get mykey
```

### Using credentials directly (shorthand)

Every command accepts `-u` / `-p` to authenticate inline (fetches a fresh
token each time):

```bash
mneme-cli -u admin -p mypassword get mykey
mneme-cli -u admin -p mypassword set counter 1
```

### Revoking a token

```bash
# Revoke the current token (immediate -- JTI blocklist)
mneme-cli revoke-token
```

### Token lifetime

Tokens expire after `auth.token_ttl_h` hours (default 24). Re-authenticate to
get a new one. Tokens are revoked immediately when:

- `revoke-token` is called
- The user is deleted
- `cluster_secret` is rotated

### Environment variables

| Variable | Description |
|----------|-------------|
| `MNEME_TOKEN` | Session token (picked up by `mneme-cli` automatically) |
| `MNEME_HOST` | Default server address (overrides 127.0.0.1:6379) |
| `MNEME_CA_CERT` | Default CA certificate path |
| `MNEME_ADMIN_PASSWORD` | Admin password (Docker only) |

---

## 6. User Management & RBAC

Three roles exist: `admin` > `readwrite` > `readonly`.

| Permission | admin | readwrite | readonly |
|------------|-------|-----------|----------|
| Read keys  | yes | yes | yes |
| Write keys | yes | yes | -- |
| Flush DB   | yes | yes | -- |
| User management | yes | -- | -- |
| Config changes  | yes | -- | -- |
| Cluster admin   | yes | -- | -- |

Users can also be restricted to specific databases via per-DB allowlists. By
default, new users have access to all databases.

### user-create

```bash
# Create a readwrite service account (default role)
mneme-cli -u admin -p s user-create app-service hunter2

# Create a read-only analytics user
mneme-cli -u admin -p s user-create analytics secret123 --role readonly

# Create another admin
mneme-cli -u admin -p s user-create alice pass --role admin
```

### user-delete

```bash
mneme-cli -u admin -p s user-delete analytics
```

### user-list

```bash
mneme-cli -u admin -p s user-list
```

### user-grant (per-DB allowlist)

```bash
# Grant access to database ID 3
mneme-cli -u admin -p s user-grant analytics 3
```

### user-revoke

```bash
# Revoke access to database ID 0
mneme-cli -u admin -p s user-revoke analytics 0
```

### user-set-role

```bash
mneme-cli -u admin -p s user-role app-service readonly
mneme-cli -u admin -p s user-role app-service readwrite   # restore
```

### user-info

```bash
# Show a specific user's role and allowed databases
mneme-cli -u admin -p s user-info analytics

# No username argument = info about the authenticated user
mneme-cli -u app-service -p hunter2 user-info
```

---

## 7. CLI Profiles

Profiles save connection settings (host, token, default database, consistency)
so you don't repeat them on every command. Profiles are stored in
`~/.mneme/profiles.toml`. Passwords are saved in plain text -- use filesystem
permissions or a secrets manager for sensitive environments.

### profile-set

```bash
mneme-cli profile-set prod \
  --host 10.0.0.1:6379 \
  --ca-cert /etc/mneme/ca.crt \
  --username admin \
  --password secret

mneme-cli profile-set staging \
  --host staging.internal:6379 \
  --ca-cert /etc/mneme/staging-ca.crt \
  --username dev \
  --password devpass
```

### Use a profile

```bash
# -P / --profile selects the named profile
mneme-cli -P prod set mykey myval
mneme-cli -P staging get mykey

# CLI args override the profile (sentinel-based merge)
mneme-cli -P prod --consistency all set critical-key value
```

### profile-list

```bash
mneme-cli profile-list
```

### profile-show

```bash
mneme-cli profile-show prod
```

### profile-delete

```bash
mneme-cli profile-delete staging
```

### Profile with a default database

```bash
mneme-cli profile-set analytics-team --host 10.0.0.1:6379 --db 1
mneme-cli -P analytics-team dbsize
```

---

## 8. Data Commands

### 8.1 String Commands

The most common data type. Values can be any binary data up to 10 MiB. Stored
internally using a RobinHood hash map with ahash.

#### set

```bash
# Basic
mneme-cli set greeting "hello world"

# With TTL (expires in 60 seconds)
mneme-cli set session:abc123 '{"user":1}' --ttl 60

# Overwrite (no special flag needed)
mneme-cli set greeting "updated"

# With consistency level
mneme-cli set critical-data "important" --consistency all
```

#### get

```bash
mneme-cli get greeting
# -> "hello world"
```

#### del

```bash
# Delete one key
mneme-cli del greeting
# -> (integer) 1   (number of keys deleted)

# Delete multiple keys
mneme-cli del key1 key2 key3
# -> (integer) 3
```

#### getset

```bash
# Returns old value, writes new value atomically
mneme-cli getset session:token "new-token-abc"
# -> (nil)    <- first call, no previous value

mneme-cli getset session:token "another-token"
# -> "new-token-abc"
```

#### exists

```bash
mneme-cli exists greeting
# -> (integer) 0   (0 = not found, 1 = exists)
```

#### expire

```bash
mneme-cli set mykey myval
mneme-cli expire mykey 120      # set expiry: 120 seconds from now
```

#### ttl

```bash
mneme-cli ttl mykey
# -> (integer) 119   (seconds remaining)
# -> (integer) -2    (key does not exist)
```

### 8.2 Counter Commands

Atomic integer operations on string values. Counter keys are created
automatically on first use -- no prior `set` needed.

#### incr / decr

```bash
mneme-cli incr page:views         # -> (integer) 1
mneme-cli incr page:views         # -> (integer) 2
mneme-cli decr page:views         # -> (integer) 1
```

#### incrby / decrby

```bash
mneme-cli incrby page:views 10    # -> (integer) 11
mneme-cli decrby page:views 5     # -> (integer) 6
mneme-cli get page:views          # -> 6
```

#### incrbyfloat

```bash
mneme-cli incrbyfloat price 9.99  # -> 9.99
mneme-cli incrbyfloat price 0.01  # -> 10.0
```

Non-numeric string values return a `WrongType` error.

### 8.3 Hash Commands

Hashes are field-to-value maps stored as `Vec<(field, value)>`. Up to 65,536
fields per hash. Ideal for structured objects.

#### hset

```bash
# Set multiple fields at once (alternating field value pairs)
mneme-cli hset user:1001 name Alice email alice@example.com age 30
# -> (integer) 3   (number of new fields added)
```

#### hget

```bash
mneme-cli hget user:1001 name
# -> "Alice"
```

#### hgetall

```bash
mneme-cli hgetall user:1001
# -> 1) name
#    2) Alice
#    3) email
#    4) alice@example.com
#    5) age
#    6) 30
```

#### hdel

```bash
mneme-cli hdel user:1001 age
# -> (integer) 1
```

#### Practical pattern: session storage

```bash
mneme-cli hset session:abc user_id 42 role admin expires_at 1720000000
mneme-cli expire session:abc 3600

# Later
mneme-cli hget session:abc user_id
# -> 42
```

### 8.4 List Commands

Ordered sequences using an intrusive deque + slab allocator. Efficient push/pop
at both ends.

#### lpush / rpush

```bash
mneme-cli lpush queue task3 task4     # left push: [task4, task3]
mneme-cli rpush queue task1 task2     # right push: [task4, task3, task1, task2]
```

#### lpop / rpop

```bash
mneme-cli lpop queue    # -> task4
mneme-cli rpop queue    # -> task2
```

#### lrange

```bash
# 0-indexed, -1 = last element
mneme-cli lrange queue 0 -- -1   # all elements
mneme-cli lrange queue 0 -- 2    # first three
```

#### Job queue pattern

```bash
# Producer
mneme-cli rpush jobs '{"id":1,"type":"email"}'
mneme-cli rpush jobs '{"id":2,"type":"sms"}'

# Consumer (FIFO: pop from left)
mneme-cli lpop jobs
# -> {"id":1,"type":"email"}
```

### 8.5 Sorted Set Commands

Members with float scores, stored in a skiplist (level 32) with rank index.
Automatically sorted by score. Great for leaderboards, priority queues, and
time-series indices.

#### zadd

```bash
# Scores must be floats
mneme-cli zadd leaderboard 1500.0 alice 1200.0 bob 1800.0 carol
# -> (integer) 3
```

#### zscore

```bash
mneme-cli zscore leaderboard carol   # -> 1800
```

#### zrank

```bash
# 0-based, ascending order
mneme-cli zrank leaderboard alice    # -> (integer) 1 (second lowest score)
mneme-cli zrank leaderboard bob      # -> (integer) 0 (lowest score)
```

#### zrange

```bash
# By rank (ascending); use -- before negative indices
mneme-cli zrange leaderboard 0 -- -1                  # all members
mneme-cli zrange leaderboard 0 -- -1 --withscores     # with scores
```

#### zrangebyscore

```bash
# By score range (inclusive)
mneme-cli zrangebyscore leaderboard 1400.0 2000.0
# -> alice, carol
```

#### zrem

```bash
mneme-cli zrem leaderboard bob
# -> (integer) 1
```

#### zcard

```bash
mneme-cli zcard leaderboard          # -> (integer) 2
```

#### Time-series index pattern

```bash
# Score = unix timestamp
mneme-cli zadd events:2024 1720000000 "login:user:42"
mneme-cli zadd events:2024 1720000100 "purchase:order:99"

# Fetch events in a time window
mneme-cli zrangebyscore events:2024 1720000000 1720000200
```

### 8.6 JSON Commands

Store and manipulate JSON documents with JSONPath. The JSON type supports
nested objects, arrays, numbers, strings, and booleans.

#### json-set

```bash
mneme-cli json-set product:1 '$' '{"name":"Widget","price":9.99,"tags":["sale","new"]}'
```

#### json-get

```bash
# Read the whole document
mneme-cli json-get product:1 '$'
# -> {"name":"Widget","price":9.99,"tags":["sale","new"]}

# Read a nested field
mneme-cli json-get product:1 '$.name'
# -> "Widget"
```

#### json-exists

```bash
mneme-cli json-exists product:1 '$.tags'
# -> true
```

#### json-type

```bash
mneme-cli json-type product:1 '$.price'
# -> number
```

#### json-numincrby

```bash
mneme-cli json-numincrby product:1 '$.price' 0.01
# -> 10.00
```

#### json-arrappend

```bash
mneme-cli json-arrappend product:1 '$.tags' '"clearance"'

mneme-cli json-get product:1 '$.tags'
# -> ["sale","new","clearance"]
```

#### json-del

```bash
# Delete a path within the document
mneme-cli json-del product:1 '$.tags[0]'

# Delete the whole key
mneme-cli del product:1
```

---

## 9. Bulk Operations

Read or write multiple keys in one round-trip. Maximum 1000 keys per request.

### MSET

```bash
mneme-cli mset \
  user:1:name "Alice" \
  user:1:email "alice@example.com" \
  user:2:name "Bob" \
  user:2:email "bob@example.com"
```

### MGET

```bash
# Returns nil for missing keys
mneme-cli mget user:1:name user:1:email user:999:name
# -> Alice
#    alice@example.com
#    (nil)
```

### With consistency

```bash
# MGET / MSET respect the same consistency level as single-key ops
mneme-cli mset --consistency quorum key1 v1 key2 v2
mneme-cli mget --consistency eventual key1 key2
```

---

## 10. Key Discovery

### SCAN -- cursor-based iteration

SCAN is safe to use in production. It never blocks the server. Uses glob
patterns for filtering.

```bash
# Scan all keys (10 per page by default)
mneme-cli scan '*'                # all keys
mneme-cli scan 'user:*'           # keys starting with user:
mneme-cli scan '*:session'        # keys ending with :session
```

`scan` returns a cursor and matching keys. A cursor of 0 means the scan is
complete. Iterate by passing the returned cursor back:

```bash
# Manual iteration (for large key spaces)
mneme-cli scan 'cache:*'                  # returns keys + next_cursor=N
mneme-cli scan 'cache:*' --cursor N       # continue from cursor N
```

### TYPE

Returns the data type of a key.

```bash
mneme-cli type mykey   # -> string | hash | list | zset | json
```

### EXISTS

```bash
mneme-cli exists session:abc
# -> (integer) 1   (exists)
# -> (integer) 0   (not found)
```

### TTL

```bash
mneme-cli ttl session:abc
# -> (integer) 59   (seconds remaining)
# -> (integer) -2   (key does not exist)
```

---

## 11. Database Namespacing

MnemeCache supports up to 65,536 independent databases (default: 16). Database
0 is the default. Each database is isolated -- keys in database 1 are invisible
from database 2. Internally, a 2-byte `db_id` prefix is prepended to all
storage keys.

Databases can be accessed by numeric index **or by a human-readable name** --
names are strongly preferred in scripts and configs.

### Creating named databases

```bash
# Create a named database (server assigns the next free numeric ID)
mneme-cli -u admin -p s db-create analytics

# Pin a name to a specific numeric ID
mneme-cli -u admin -p s db-create cache   --id 2
mneme-cli -u admin -p s db-create staging --id 10
```

### Listing databases

```bash
mneme-cli -u admin -p s db-list
#   ID      Name
#   1       analytics
#   2       cache
#   10      staging
```

### Static names in config

Add names to `core.toml` so they are pre-loaded at startup:

```toml
[databases]
max_databases = 16

[databases.names]
analytics = 1
cache     = 2
staging   = 10
```

Names defined in config are merged with runtime names registered via
`db-create`. If the same ID appears in both, the config value wins.

### SELECT -- switch database

```bash
# Switch by name
mneme-cli -u admin -p s select analytics

# All subsequent commands operate on 'analytics'
mneme-cli -u admin -p s set product:1 widget
mneme-cli -u admin -p s get product:1          # -> "widget"

# Switch by numeric ID
mneme-cli -u admin -p s select 2

# One-shot: global -d flag (numeric only; use 'select' for names)
mneme-cli -u admin -p s -d 2 set session:tok abc123
```

### DBSIZE

```bash
# Count keys in the active (default) database
mneme-cli -u admin -p s dbsize

# Count keys in a named database
mneme-cli -u admin -p s dbsize --db analytics
# -> (integer) 42  (db analytics)

# Count by numeric ID
mneme-cli -u admin -p s dbsize --db 2
```

### FLUSHDB

```bash
# Flush a named database (replicated by default)
mneme-cli -u admin -p s flushdb --db analytics

# Flush by ID, skip replication
mneme-cli -u admin -p s flushdb --db 2 --no-sync
```

### Dropping a database name

```bash
# Deregister the name (data remains accessible by numeric ID)
mneme-cli -u admin -p s flushdb --db staging   # clear data first if desired
mneme-cli -u admin -p s db-drop staging
```

### Access control with named databases

`user-grant` and `user-revoke` use numeric IDs. Resolve with `db-list`:

```bash
mneme-cli -u admin -p s db-list              # -> analytics=1, cache=2
mneme-cli -u admin -p s user-grant alice 1   # grant alice access to 'analytics'
mneme-cli -u admin -p s user-grant alice 2   # grant alice access to 'cache'
```

---

## 12. Consistency Levels

Control the durability guarantee per request with `--consistency`. The default
is QUORUM.

| Level | Flag bits | Guarantee | p99 Latency |
|-------|-----------|-----------|-------------|
| `eventual` | `0x00` | Fire-and-forget to Keepers, AP | < 150 us |
| `one` | `0x0C` | First Keeper ACK | < 300 us |
| `quorum` | `0x04` | floor(N/2)+1 Keeper ACKs **(default)** | < 800 us |
| `all` | `0x08` | Every Keeper ACK | < 1.2 ms |

### When to use each level

**EVENTUAL** -- best for ephemeral data like cache hit-rate counters, session
views, real-time analytics. Writes return immediately without waiting for any
Keeper. Data may be lost if Core crashes before replication completes.

**ONE** -- first Keeper acknowledgment. Provides minimal durability with lower
latency than QUORUM. Suitable for data that can tolerate partial loss.

**QUORUM** -- the safe default for most application data. Ensures a majority of
Keepers have persisted the write. Survives minority node failures.

**ALL** -- strictest guarantee. Every connected Keeper must acknowledge. Use for
financial transactions or data that must never be lost. Highest latency and
blocks if any Keeper is down.

### CLI examples

```bash
# Fast write -- acceptable to lose on crash (cache hit-rate tracking)
mneme-cli set page:views 1000 --consistency eventual

# Default -- safe for most application data
mneme-cli set order:42 '{"status":"paid"}' --consistency quorum

# Critical financial data
mneme-cli set balance:user:7 9999 --consistency all

# Read from a replica (may be stale by a few ms)
mneme-cli get leaderboard:rank1 --consistency eventual

# Read from the authoritative Core (freshest)
mneme-cli get balance:user:7 --consistency quorum
```

### Warmup gate

After Core restart, the warmup state progresses through Cold -> Warming -> Hot.
During Cold and Warming phases, QUORUM and ALL reads are **blocked** until all
Keepers have finished pushing their data back to Core (WarmupState::Hot).
EVENTUAL reads are always served immediately.

---

## 13. Admin & Observability

### stats

```bash
mneme-cli stats
# -> keys, pool_used, pool_max, keepers, pool_ratio
```

### pool-stats

```bash
mneme-cli pool-stats
# -> Pool Used / Max, Keeper count
```

### cluster-info

```bash
mneme-cli cluster-info
# -> leader, live_nodes, raft_term, warmup_state, memory_pressure,
#    replication_lag_ms per Keeper, supported_modes, is_leader, leader_id
```

### keeper-list

```bash
mneme-cli keeper-list
# -> Node Name, Address, Pool (grant), Used
```

### cluster-slots

```bash
mneme-cli cluster-slots
# -> Slot-to-node assignments (CRC16 % 16384)
```

### slowlog

```bash
mneme-cli slowlog
# -> Last N commands that exceeded the threshold (default 10 ms)

mneme-cli slowlog 20    # last 20 slow commands
```

### monitor

```bash
# Stream all commands as they execute (MONITOR mode)
mneme-cli monitor
```

### metrics

```bash
# Fetch raw Prometheus metrics
mneme-cli metrics

# Or directly from the HTTP endpoint (no auth needed)
curl http://10.0.0.1:9090/metrics | grep mneme_
```

Key metrics to watch:

| Metric | Alert threshold |
|--------|----------------|
| `memory_pressure_ratio` | > 0.85 (add Keeper or increase pool) |
| `replication_lag_ms` | > 500 ms (network issue or Keeper overloaded) |
| `evictions_total{reason="oom"}` | > 0 (critical -- data loss possible) |
| `requests_total{cmd="*"}` [rate] | Throughput baseline |
| `request_duration_seconds{p99}` | > 1 s for QUORUM writes |

Full metrics list (Aletheia):

| Category | Metrics |
|----------|---------|
| Charon | connections_active, connections_accepted, connections_rejected, connections_idle |
| Request | requests_total{cmd,consistency}, duration_histogram, in_flight, timeout |
| Memory | pool_bytes_used, pool_bytes_max, pressure_ratio, evictions{lfu,ttl,oom}, cold_fetches |
| Hermes | replication_lag_ms{keeper}, frames, errors, sync_state{keeper} |
| Aoide | wal_bytes, sync_duration, rotations |
| Hardware | l1/l2/l3_cache_misses, tlb_misses, branch_mispredictions, cycles, instructions |
| Themis | cluster_term, leader, live_nodes, elections |
| Iris | slot_distribution{keeper}, migrations |

### memory-usage

```bash
mneme-cli memory-usage mykey
# -> (integer) 128   (approximate in-RAM footprint in bytes)
```

### config / config-set

```bash
# Read a config parameter
mneme-cli config memory.pool_bytes
# -> 2147483648

# Set a config parameter (hot-reload, no restart needed)
mneme-cli config-set memory.pool_bytes 4gb
mneme-cli config-set memory.eviction_threshold 0.85
```

### join-token

```bash
# Print the join token (admin only -- use for adding new Keepers)
mneme-cli join-token
```

---

## 14. Memory Management & Eviction

### Memory model

```
logical pool = Core RAM + sum(Keeper grants)
hot set      = Core RAM only           (~150 us read)
cold set     = Oneiros redb B-tree     (~1.2 ms read)
```

### Eviction thresholds

MnemeCache uses two eviction levels:

1. **Proactive eviction** (>= `eviction_threshold`, default 90%): Lethe evicts
   the coldest 1% of keys using LFU Morris counters. Evicted keys are migrated
   to the nearest Keeper's cold store (Oneiros) -- they are **not deleted** and
   can be promoted back to RAM on access.

2. **OOM eviction** (>= 100% pressure): 5% of keys are dropped immediately.
   OOM evictions appear in metrics as `evictions_total{reason="oom"}`. This
   must be treated as a critical alert.

**Eviction is not the same as TTL expiry.** Eviction is reversible (keys move
to cold store). TTL expiry is a permanent delete.

### LFU Morris counters

Each key maintains a probabilistic access counter using Morris's algorithm. The
counter requires only a few bits per key. Keys with the lowest counter values
are evicted first. Counters decay over time to prevent stale hot keys from
staying in RAM forever.

### TTL wheel (Lethe)

TTL expiry uses a 3-level hierarchical timing wheel:

| Level | Buckets | Resolution |
|-------|---------|------------|
| Level 0 | 256 | 10 ms |
| Level 1 | 64 | 1 second |
| Level 2 | 64 | 60 seconds |
| Level 3 | 64 | 3600 seconds (1 hour) |

TTL is replicated in REPLICATE frames as `expiry_at: u64` (unix milliseconds).
On restart, keys expired during downtime are deleted immediately during the push
phase.

### Scale vertically

```bash
# Increase the hot RAM pool without restart
mneme-cli -u admin -p <PASSWORD> config-set memory.pool_bytes 4gb
```

### Scale horizontally

Add more Keeper nodes (see Section 4.2). Each Keeper extends the logical pool
by its `pool_bytes` grant.

### Eviction policy tuning

In `core.toml`:

```toml
[memory]
pool_bytes          = "2gb"
eviction_threshold  = 0.90   # start LFU eviction at 90% (default)
promotion_threshold = 10     # LFU hits before cold->hot promotion
```

---

## 15. Deployment Guide Links

Detailed deployment guides for each platform:

| Platform | Guide |
|----------|-------|
| Linux (bare metal / VM) | [docs/setup/linux.md](setup/linux.md) |
| Docker | [docs/setup/docker.md](setup/docker.md) |
| Kubernetes | [docs/setup/kubernetes.md](setup/kubernetes.md) |

### Kubernetes quick reference

```bash
# Namespace + RBAC
kubectl apply -f k8s/namespace.yaml

# Secrets (edit before applying)
kubectl apply -f k8s/secrets.yaml

# Core StatefulSet + Service
kubectl apply -f k8s/core.yaml

# Keeper StatefulSet (3 replicas)
kubectl apply -f k8s/keeper.yaml

# Prometheus ServiceMonitor (if using kube-prometheus-stack)
kubectl apply -f k8s/monitoring.yaml
```

The Kubernetes manifests provide:

- **Core**: StatefulSet with 1 replica, PVC for certs, ClusterIP + NodePort
  services for client (6379) and replication (7379) ports
- **Keeper**: StatefulSet with 3 replicas, PVC per pod for WAL + cold store
- **Secrets**: `mneme-auth` Secret holds `admin_password` and `cluster_secret`
- **HPA**: Horizontal scaling for read replicas (not Core or Keeper)
- **PodDisruptionBudget**: Ensures at least 2 of 3 Keepers are available during
  rolling updates

Important notes:

- Core RAM pool is limited by the Pod's memory request/limit. Set these in
  `k8s/core.yaml` to match `memory.pool_bytes`.
- Keeper pods need fast local storage for WAL. Use `storageClassName:
  local-path` or a similar low-latency StorageClass.
- The replication port (7379) must be reachable between Core and Keeper pods.
  A headless Service handles this.

---

## 16. Client Libraries

MnemeCache speaks a binary wire protocol over TLS 1.3. The official client is
the Rust **Pontus** crate (`mneme-client/`) included in this repository.

For other languages, see [docs/CLIENT_PROTOCOL.md](../CLIENT_PROTOCOL.md) — the
full wire protocol specification covers everything needed to build a compatible
client: frame format, command IDs, authentication, multiplexing, consistency
semantics, and error codes.

### 16.1 Rust -- Pontus (included)

The `mneme-client` crate (**Pontus**) is the official, fully-featured Rust
client. It ships inside the MnemeCache repository under `mneme-client/`.

#### Cargo dependency

```toml
[dependencies]
mneme-client = { path = "../mneme-client" }
tokio        = { version = "1", features = ["full"] }
anyhow       = "1"
```

#### Connection pool -- `MnemePool`

For production code, use `MnemePool`. It maintains warm idle connections,
auto-reconnects, health-checks via PING, and enforces an `acquire_timeout`.

```rust
use mneme_client::{MnemePool, PoolConfig, Consistency};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool = MnemePool::new(PoolConfig {
        addr:            "10.0.0.1:6379".into(),
        tls_ca_cert:     Some("/etc/mneme/ca.crt".into()),
        server_name:     "mneme.local".into(),
        token:           "eyJ...".into(),
        min_idle:        4,
        max_size:        32,
        acquire_timeout: Duration::from_millis(200),
        health_interval: Duration::from_secs(30),
        idle_timeout:    Duration::from_secs(300),
    }).await?;

    let conn = pool.acquire().await?;

    // -- Strings --
    conn.set("greeting", b"hello", None, Consistency::Quorum).await?;
    conn.set("session:abc", b"token123", Some(3600), Consistency::Quorum).await?;
    let val = conn.get("greeting").await?;           // Option<Vec<u8>>
    conn.del(&["greeting", "session:abc"]).await?;
    let exists = conn.exists("greeting").await?;      // bool
    conn.expire("session:abc", 120).await?;
    let ttl = conn.ttl("session:abc").await?;          // i64
    let old = conn.getset("counter", b"new").await?;   // Option<Vec<u8>>

    // -- Counters --
    conn.incr("page:views").await?;
    conn.incrby("page:views", 10).await?;
    conn.decr("page:views").await?;
    conn.decrby("page:views", 5).await?;
    conn.incrbyfloat("price", 0.99).await?;

    // -- Hashes --
    conn.hset("user:1", "name", b"alice").await?;
    conn.hset("user:1", "email", b"alice@example.com").await?;
    let name = conn.hget("user:1", "name").await?;      // Option<Vec<u8>>
    let all = conn.hgetall("user:1").await?;             // Vec<(String, Vec<u8>)>
    conn.hdel("user:1", &["email"]).await?;

    // -- Lists --
    conn.lpush("queue", &[b"task1".to_vec(), b"task2".to_vec()]).await?;
    conn.rpush("queue", &[b"task3".to_vec()]).await?;
    let head = conn.lpop("queue").await?;                // Option<Vec<u8>>
    let tail = conn.rpop("queue").await?;
    let range = conn.lrange("queue", 0, -1).await?;     // Vec<Vec<u8>>

    // -- Sorted Sets --
    conn.zadd("lb", 1500.0, b"alice", Consistency::Quorum).await?;
    conn.zadd("lb", 1200.0, b"bob", Consistency::Quorum).await?;
    let score = conn.zscore("lb", "alice").await?;       // Option<f64>
    let rank = conn.zrank("lb", "alice").await?;         // Option<u64>
    let top = conn.zrange("lb", 0, 9, true).await?;     // Vec<(Vec<u8>, f64)>
    let by_score = conn.zrangebyscore("lb", 1000.0, 2000.0).await?;
    conn.zrem("lb", &["bob"]).await?;
    let card = conn.zcard("lb").await?;                  // u64

    // -- JSON --
    conn.json_set("doc:1", "$", r#"{"name":"Widget","price":9.99,"tags":["sale"]}"#).await?;
    let jval = conn.json_get("doc:1", "$.name").await?;
    conn.json_numincrby("doc:1", "$.price", 0.01).await?;
    conn.json_arrappend("doc:1", "$.tags", r#""new""#).await?;
    let jtype = conn.json_type("doc:1", "$.price").await?;
    let jexists = conn.json_exists("doc:1", "$.name").await?;
    conn.json_del("doc:1", "$.tags[0]").await?;

    // -- Bulk Operations --
    conn.mset(&[("k1", b"v1"), ("k2", b"v2"), ("k3", b"v3")], None).await?;
    let vals = conn.mget(&["k1", "k2", "k3"]).await?;   // Vec<Option<Vec<u8>>>

    // -- SCAN --
    let page = conn.scan("user:*", 0, 100).await?;      // ScanPage { next_cursor, keys }
    let all_keys = conn.scan_all("*").await?;            // Vec<String>

    // -- TYPE --
    let key_type = conn.type_of("user:1").await?;        // "hash"

    // -- Database Namespacing --
    conn.select_id(1).await?;
    conn.set("isolated", b"value", None, Consistency::Quorum).await?;
    let size = conn.dbsize_id(1).await?;
    conn.select_id(0).await?;                            // back to default

    // -- Consistency per-request --
    conn.set("fast", b"val", None, Consistency::Eventual).await?;
    conn.set("safe", b"val", None, Consistency::Quorum).await?;
    conn.set("strict", b"val", None, Consistency::All).await?;
    conn.set("one", b"val", None, Consistency::One).await?;

    // -- Admin --
    let stats = conn.stats().await?;
    let pool_st = conn.pool_stats().await?;
    let cluster = conn.cluster_info().await?;
    let keepers = conn.keeper_list().await?;
    let slow = conn.slowlog(10).await?;
    let mem = conn.memory_usage("user:1").await?;
    conn.config_set("memory.pool_bytes", "2gb").await?;

    // -- User Management (admin only) --
    conn.user_create("alice", "pass123", "readwrite").await?;
    conn.user_set_role("alice", "readonly").await?;
    conn.user_grant("alice", 1).await?;
    conn.user_revoke("alice", 0).await?;
    let info = conn.user_info(Some("alice")).await?;
    let users = conn.user_list().await?;
    conn.user_delete("alice").await?;

    Ok(())
}
```

#### Full command surface

All commands are methods on `MnemeConn` (accessed via `PoolGuard` deref):

| Module | Methods |
|--------|---------|
| **cmd_kv** | `set`, `get`, `del`, `exists`, `expire`, `ttl`, `getset`, `incr`, `decr`, `incrby`, `decrby`, `incrbyfloat`, `mget`, `mset` |
| **cmd_hash** | `hset`, `hget`, `hdel`, `hgetall` |
| **cmd_list** | `lpush`, `rpush`, `lpop`, `rpop`, `lrange` |
| **cmd_zset** | `zadd`, `zrank`, `zscore`, `zrange`, `zrangebyscore`, `zrem`, `zcard` |
| **cmd_json** | `json_set`, `json_get`, `json_del`, `json_exists`, `json_type`, `json_arrappend`, `json_numincrby` |
| **cmd_db** | `select_id`, `select_name`, `dbsize_id`, `dbsize_name`, `flushdb_id`, `flushdb_name`, `db_create`, `db_list`, `db_drop`, `scan`, `scan_all`, `type_of` |
| **cmd_admin** | `user_create`, `user_delete`, `user_list`, `user_grant`, `user_revoke`, `user_info`, `user_set_role`, `cluster_info`, `keeper_list`, `pool_stats`, `stats`, `metrics`, `slowlog`, `memory_usage`, `config_set` |
| **conn** | `auth_token`, `revoke_token`, `ping` |

#### Response types

```rust
use mneme_client::{KeeperEntry, PoolStats, ScanPage, SlowLogEntry, UserInfo};

// KeeperEntry  -- from keeper_list()  { node_id, addr, pool_bytes, used_bytes }
// PoolStats    -- from pool_stats()   { used, max, keeper_count }
// ScanPage     -- from scan()         { next_cursor, keys }
// SlowLogEntry -- from slowlog()      { command, key, duration_us }
// UserInfo     -- from user_info()    { username, role, allowed_dbs }
```

### 16.2 Wire Protocol Reference


All clients speak the same 16-byte binary wire protocol. Building a client in
any language is straightforward.

```
Frame layout (16-byte header + msgpack payload):

Offset  Size  Field         Description
------  ----  ----------    -----------
0       4B    magic         0x4D4E454D ("MNEM")
4       1B    version       Protocol version (currently 1)
5       1B    cmd_id        Command identifier (see table below)
6       2B    flags         Bits 15-4: slot hint
                            Bits 3-2: consistency (00=EVENTUAL 01=QUORUM 10=ALL 11=ONE)
                            Bits 1-0: reserved
8       4B    payload_len   Length of msgpack body
12      4B    req_id        Request ID (0=single-plex, 1+=multiplexed)
16      N     payload       MessagePack-encoded request/response body
```

#### Authentication flow

```
1. Client establishes TLS 1.3 connection to port 6379
2. Client sends AUTH frame (cmd_id=0x60):
     payload: { "username": "admin", "password": "secret" }
3. Server responds with:
     payload: { "token": "eyJ..." }
4. Client includes token in subsequent frames:
     payload: { "token": "eyJ...", "key": "mykey" }
5. Optionally multiplex by setting req_id >= 1 -- responses may arrive out of order
```

#### Payload limits

| Limit | Value |
|-------|-------|
| Max key size | 512 bytes |
| Max value size | 10 MB |
| Max fields per hash | 65,536 |
| Max batch keys (MGET/MSET) | 1,000 |

Exceeding any limit returns `ERR_PAYLOAD_TOO_LARGE` -- the server never panics.

#### Command IDs (hex)

| ID | Command | ID | Command | ID | Command |
|----|---------|----|---------|----|---------|
| 0x01 | SET | 0x02 | GET | 0x03 | DEL |
| 0x04 | EXISTS | 0x05 | EXPIRE | 0x06 | TTL |
| 0x07 | GETSET | 0x10 | INCR | 0x11 | DECR |
| 0x12 | INCRBY | 0x13 | DECRBY | 0x14 | INCRBYFLOAT |
| 0x20 | HSET | 0x21 | HGET | 0x22 | HDEL |
| 0x23 | HGETALL | 0x30 | LPUSH | 0x31 | RPUSH |
| 0x32 | LPOP | 0x33 | RPOP | 0x34 | LRANGE |
| 0x40 | ZADD | 0x41 | ZSCORE | 0x42 | ZRANK |
| 0x43 | ZRANGE | 0x44 | ZRANGEBYSCORE | 0x45 | ZREM |
| 0x46 | ZCARD | 0x50 | JSON_SET | 0x51 | JSON_GET |
| 0x52 | JSON_DEL | 0x53 | JSON_EXISTS | 0x54 | JSON_TYPE |
| 0x55 | JSON_ARRAPPEND | 0x56 | JSON_NUMINCRBY | | |
| 0x60 | AUTH | 0x61 | REVOKE_TOKEN | 0x62 | AUTH_TOKEN |
| 0x70 | SELECT | 0x71 | DBSIZE | 0x72 | FLUSHDB |
| 0x73 | DB_CREATE | 0x74 | DB_LIST | 0x75 | DB_DROP |
| 0x80 | SCAN | 0x81 | TYPE | 0x82 | MGET |
| 0x83 | MSET | 0x90 | USER_CREATE | 0x91 | USER_DELETE |
| 0x92 | USER_LIST | 0x93 | USER_GRANT | 0x94 | USER_REVOKE |
| 0x95 | USER_INFO | 0x96 | USER_ROLE | | |
| 0xA0 | STATS | 0xA1 | POOL_STATS | 0xA2 | CLUSTER_INFO |
| 0xA3 | KEEPER_LIST | 0xA4 | CLUSTER_SLOTS | 0xA5 | SLOWLOG |
| 0xA6 | METRICS | 0xA7 | MEMORY_USAGE | 0xA8 | CONFIG |
| 0xA9 | CONFIG_SET | 0xAA | JOIN_TOKEN | 0xAB | WAIT |

See [`docs/API.md`](API.md) for full msgpack payload schemas per command.

---

## 17. Testing & Validation

This section covers how to test MnemeCache cluster behavior under failure
conditions using Docker Compose. Every test below can be run without any
external tools beyond Docker and the built-in `mneme-cli`.

### Prerequisites

```bash
# Build the image and start the cluster
docker compose build
docker compose --profile cluster up -d

# Wait for all services to be healthy (~30s)
docker compose --profile cluster ps

# Set up a shorthand alias for the CLI
MCLI="docker compose --profile cluster exec mneme-core mneme-cli --host $MNEME_HOST -u admin -p secret"
```

### 17.1 Smoke Tests

Quick validation of all basic operations (~30 seconds):

```bash
# Run the built-in smoke test (65+ checks)
docker compose --profile cluster --profile cluster-test run --rm smoke-test
```

The smoke test covers: strings, counters, hashes, lists, sorted sets, JSON,
MGET/MSET, SCAN, TYPE, database namespacing, auth tokens, user management,
observability commands, TTL expiry, and config operations.

Expected output:

```
=== MnemeCache Smoke Test ===
...
RESULT: 65/65 passed, 0 failed
```

### 17.2 Integration Tests

Comprehensive cluster validation including replication, consistency, and RBAC:

```bash
# Run as a standalone container (recommended)
docker compose --profile cluster --profile cluster-test run --rm integration-test

# Or run inside the Core container (enables Core restart test)
docker compose --profile cluster exec mneme-core bash /docker/integration-test.sh
```

The integration test covers 13 test groups:

1. Basic connectivity (PING, SET, GET, DEL)
2. Database namespacing (key isolation across DBs)
3. SELECT guidance
4. QUORUM write + read (Keeper ACK, latency check)
5. Join token format validation
6. Keeper-list statistics (WAL bytes, disk estimates)
7. Data type replication (Hash, List, ZSet survive QUORUM)
8. TTL and expiry
9. MGET/MSET bulk operations
10. SCAN with glob patterns
11. Core restart data survival (exec mode only)
12. CONFIG SET persistence
13. User management / RBAC

### 17.3 Keeper Crash Recovery

Test that data survives Keeper restarts via WAL replay.

#### Test 1: Single Keeper restart -- data survives

```bash
# Write test data with QUORUM consistency
$MCLI set crash-test:1 "important-data" --consistency quorum
$MCLI set crash-test:2 "more-data" --consistency quorum
$MCLI hset crash-test:hash name Alice email alice@example.com
$MCLI zadd crash-test:zset 100.0 member1 200.0 member2

# Verify keeper has the data
$MCLI keeper-list
$MCLI stats

# Kill Keeper-1 hard (simulates crash -- no graceful shutdown)
docker kill mneme-keeper-1

# Verify QUORUM still works (Keeper-2 is alive)
$MCLI set during-outage "still-works" --consistency quorum
$MCLI get during-outage
# -> "still-works"

# Bring Keeper-1 back
docker compose --profile cluster start mneme-keeper-1

# Wait for WAL replay (~5s)
sleep 5

# Verify all data survived
$MCLI get crash-test:1
# -> "important-data"
$MCLI hgetall crash-test:hash
# -> name Alice email alice@example.com
$MCLI zrange crash-test:zset 0 -- -1 --withscores
# -> member1 100 member2 200

# Verify keeper is back in the cluster
$MCLI keeper-list
```

#### Test 2: All Keepers down -- QUORUM must fail

```bash
# Stop all keepers
docker stop mneme-keeper-1 mneme-keeper-2

# QUORUM writes should fail (no keepers to ACK)
$MCLI set quorum-fail "data" --consistency quorum
# -> Error: QuorumNotReached

# EVENTUAL writes still work (fire-and-forget to Core RAM)
$MCLI set eventual-ok "data" --consistency eventual
$MCLI get eventual-ok
# -> "data"

# Restore keepers
docker compose --profile cluster start mneme-keeper-1 mneme-keeper-2
sleep 5

# QUORUM works again
$MCLI set quorum-restored "data" --consistency quorum
```

#### Test 3: Automated keeper crash script

```bash
docker compose --profile cluster exec mneme-core bash /docker/test_keeper_crash.sh
```

### 17.4 Core Crash Recovery

Test that data survives Core restarts via Keeper warm-up push.

#### Test 1: Core restart -- Keepers push data back

```bash
# Write significant test data
for i in $(seq 1 100); do
  $MCLI set "recovery:$i" "value-$i" --consistency quorum
done
$MCLI stats
# -> keys=100

# Record the key count
BEFORE=$($MCLI dbsize | grep -o '[0-9]*')

# Restart the Core (graceful -- drains in-flight, flushes Hermes)
docker restart mneme-core

# Wait for warm-up (Keepers push all keys back)
sleep 10

# Verify warmup completed
$MCLI cluster-info
# -> warmup_state: Hot

# Verify all keys survived
AFTER=$($MCLI dbsize | grep -o '[0-9]*')
echo "Before: $BEFORE, After: $AFTER"
# -> Before: 100, After: 100

# Spot-check individual keys
$MCLI get recovery:1
# -> "value-1"
$MCLI get recovery:100
# -> "value-100"
```

#### Test 2: Warmup gate -- QUORUM blocked until Hot

```bash
# Restart Core
docker restart mneme-core

# Immediately try QUORUM read (should block or error until Hot)
$MCLI get recovery:1 --consistency quorum
# -> May timeout during Cold/Warming state

# Wait for Hot state
sleep 10

# Now QUORUM works
$MCLI get recovery:1 --consistency quorum
# -> "value-1"

# Verify warmup state
$MCLI cluster-info
# -> warmup_state: Hot
```

#### Test 3: Automated core crash script

```bash
docker compose --profile cluster exec mneme-core bash /docker/test_core_crash.sh
```

### 17.5 Data Persistence Verification

Test that data survives full cluster restarts (docker compose down/up).

```bash
# Write test data across all data types
$MCLI set persist:string "hello"
$MCLI hset persist:hash field1 val1 field2 val2
$MCLI rpush persist:list a b c d e
$MCLI zadd persist:zset 1.0 alpha 2.0 beta 3.0 gamma
$MCLI json-set persist:json '$' '{"status":"active","count":42}'
$MCLI set persist:ttl "expires-later" --ttl 3600

# Record state
BEFORE_COUNT=$($MCLI dbsize | grep -o '[0-9]*')

# Stop everything (volumes preserved)
docker compose --profile cluster down

# Bring it all back
docker compose --profile cluster up -d
sleep 15  # wait for full startup + warm-up

# Verify all data survived
$MCLI get persist:string
# -> "hello"

$MCLI hgetall persist:hash
# -> field1 val1 field2 val2

$MCLI lrange persist:list 0 -- -1
# -> a b c d e

$MCLI zrange persist:zset 0 -- -1 --withscores
# -> alpha 1 beta 2 gamma 3

$MCLI json-get persist:json '$'
# -> {"status":"active","count":42}

$MCLI ttl persist:ttl
# -> (integer) 3500+  (still has remaining TTL)

AFTER_COUNT=$($MCLI dbsize | grep -o '[0-9]*')
echo "Before: $BEFORE_COUNT, After: $AFTER_COUNT"

# Clean up (remove volumes for fresh state)
docker compose --profile cluster down -v
```

### 17.6 Consistency Level Testing

Verify each consistency level behaves correctly.

#### EVENTUAL -- fire-and-forget

```bash
$MCLI set eventual:key "fast-write" --consistency eventual
$MCLI get eventual:key --consistency eventual
# -> "fast-write"

# Write 100 keys with EVENTUAL (should be very fast)
time for i in $(seq 1 100); do
  $MCLI set "eventual:$i" "val" --consistency eventual
done
# -> real ~2-3s (no Keeper wait)
```

#### ONE -- first Keeper ACK

```bash
$MCLI set one:key "first-ack" --consistency one
$MCLI get one:key
# -> "first-ack"
```

#### QUORUM -- majority ACK (default)

```bash
$MCLI set quorum:key "majority-safe"
$MCLI get quorum:key
# -> "majority-safe"

# With 2 Keepers: quorum = floor(2/2)+1 = 2 (both must ACK)
docker stop mneme-keeper-1
$MCLI set quorum:during-outage "test" --consistency quorum
# -> May fail if quorum requires both keepers
docker compose --profile cluster start mneme-keeper-1
sleep 5
```

#### ALL -- every Keeper must ACK

```bash
$MCLI set all:key "strictest" --consistency all
$MCLI get all:key --consistency all
# -> "strictest"

# Stop one keeper -- ALL must fail
docker stop mneme-keeper-2
$MCLI set all:fail "data" --consistency all
# -> Error: QuorumNotReached (not all keepers available)

# Restore
docker compose --profile cluster start mneme-keeper-2
sleep 5

$MCLI set all:restored "data" --consistency all
# -> OK
```

### 17.7 TLS Validation

Verify TLS is enforced and misconfigured clients are rejected.

```bash
# Run the built-in TLS test
docker compose --profile cluster exec mneme-core bash /docker/test_tls.sh
```

Manual TLS tests:

```bash
# Test 1: Valid TLS connection succeeds
$MCLI stats
# -> OK (stats output)

# Test 2: Plain TCP to TLS port is rejected
docker compose --profile cluster exec mneme-core \
  bash -c 'echo "PING" | nc -w2 127.0.0.1 6379'
# -> Connection closed (server expects TLS)

# Test 3: Wrong password rejected
docker compose --profile cluster exec mneme-core \
  mneme-cli --host $MNEME_HOST -u admin -p wrongpassword stats
# -> Error: TokenInvalid

# Test 4: No credentials rejected
docker compose --profile cluster exec mneme-core \
  mneme-cli -u admin -p secret stats
# -> Error: authentication required
```

### 17.8 Stress Testing

Load test the cluster for throughput and stability.

```bash
# Run the built-in stress test
docker compose --profile cluster exec mneme-core bash /docker/test_stress.sh
```

#### High-throughput writes

```bash
# 1000 sequential EVENTUAL writes
time for i in $(seq 1 1000); do
  $MCLI set "stress:$i" "payload-$i" --consistency eventual 2>/dev/null
done
echo "1000 EVENTUAL writes completed"

# 1000 sequential QUORUM writes
time for i in $(seq 1 1000); do
  $MCLI set "stress:q:$i" "payload-$i" --consistency quorum 2>/dev/null
done
echo "1000 QUORUM writes completed"
```

#### Large payloads

```bash
# Generate a 100KB payload
PAYLOAD=$(head -c 102400 /dev/urandom | base64 | head -c 100000)

# Write and read large value
$MCLI set stress:large "$PAYLOAD" --consistency quorum
RESULT=$($MCLI get stress:large)
[ ${#RESULT} -gt 99000 ] && echo "PASS: Large payload round-trip" || echo "FAIL"
```

#### Mixed workload

```bash
# Concurrent mixed operations
for i in $(seq 1 200); do
  $MCLI set "mix:$i" "val-$i" 2>/dev/null &
done
wait

for i in $(seq 1 200); do
  $MCLI get "mix:$i" 2>/dev/null &
done
wait

for i in $(seq 1 100); do
  $MCLI del "mix:$i" 2>/dev/null &
done
wait

echo "Mixed workload completed"
$MCLI stats
```

#### Rapid create/delete cycles

```bash
for i in $(seq 1 500); do
  $MCLI set "ephemeral:$i" "temp" 2>/dev/null
  $MCLI del "ephemeral:$i" 2>/dev/null
done
echo "500 create/delete cycles completed"
```

#### Hash with many fields

```bash
ARGS=""
for i in $(seq 1 100); do
  ARGS="$ARGS field$i value$i"
done
$MCLI hset stress:bigHash $ARGS
$MCLI hgetall stress:bigHash | wc -l
# -> 200 (100 fields * 2 lines each)
```

#### Complete CLI command test script

```bash
#!/bin/bash
# Complete CLI command test script
# Run inside: docker compose --profile cluster exec mneme-core bash

MCLI="mneme-cli --host $MNEME_HOST -u admin -p secret"
PASS=0; FAIL=0

check() {
  if [ $? -eq 0 ]; then PASS=$((PASS+1)); echo "  PASS: $1"
  else FAIL=$((FAIL+1)); echo "  FAIL: $1"; fi
}

echo "=== String Commands ==="
$MCLI set test:str "hello" && check "SET"
$MCLI get test:str && check "GET"
$MCLI del test:str && check "DEL"
$MCLI set test:str "val" && $MCLI exists test:str && check "EXISTS"
$MCLI expire test:str 3600 && check "EXPIRE"
$MCLI ttl test:str && check "TTL"
$MCLI getset test:str "new" && check "GETSET"

echo "=== Counter Commands ==="
$MCLI incr test:counter && check "INCR"
$MCLI decr test:counter && check "DECR"
$MCLI incrby test:counter 10 && check "INCRBY"
$MCLI decrby test:counter 5 && check "DECRBY"
$MCLI incrbyfloat test:float 1.5 && check "INCRBYFLOAT"

echo "=== Hash Commands ==="
$MCLI hset test:hash f1 v1 f2 v2 && check "HSET"
$MCLI hget test:hash f1 && check "HGET"
$MCLI hgetall test:hash && check "HGETALL"
$MCLI hdel test:hash f2 && check "HDEL"

echo "=== List Commands ==="
$MCLI lpush test:list a b && check "LPUSH"
$MCLI rpush test:list c d && check "RPUSH"
$MCLI lpop test:list && check "LPOP"
$MCLI rpop test:list && check "RPOP"
$MCLI lrange test:list 0 -- -1 && check "LRANGE"

echo "=== Sorted Set Commands ==="
$MCLI zadd test:zset 1.0 alice 2.0 bob 3.0 carol && check "ZADD"
$MCLI zscore test:zset alice && check "ZSCORE"
$MCLI zrank test:zset alice && check "ZRANK"
$MCLI zrange test:zset 0 -- -1 && check "ZRANGE"
$MCLI zrangebyscore test:zset 1.0 2.5 && check "ZRANGEBYSCORE"
$MCLI zrem test:zset bob && check "ZREM"
$MCLI zcard test:zset && check "ZCARD"

echo "=== JSON Commands ==="
$MCLI json-set test:json '$' '{"a":1,"b":[1,2]}' && check "JSON-SET"
$MCLI json-get test:json '$.a' && check "JSON-GET"
$MCLI json-exists test:json '$.a' && check "JSON-EXISTS"
$MCLI json-type test:json '$.a' && check "JSON-TYPE"
$MCLI json-numincrby test:json '$.a' 5 && check "JSON-NUMINCRBY"
$MCLI json-arrappend test:json '$.b' '3' && check "JSON-ARRAPPEND"
$MCLI json-del test:json '$.b[0]' && check "JSON-DEL"

echo "=== Bulk Commands ==="
$MCLI mset k1 v1 k2 v2 k3 v3 && check "MSET"
$MCLI mget k1 k2 k3 && check "MGET"
$MCLI scan '*' && check "SCAN"
$MCLI type test:hash && check "TYPE"

echo "=== Database Commands ==="
$MCLI dbsize && check "DBSIZE"
$MCLI db-create testdb && check "DB-CREATE"
$MCLI db-list && check "DB-LIST"
$MCLI select testdb && check "SELECT"
$MCLI flushdb --db testdb && check "FLUSHDB"
$MCLI db-drop testdb && check "DB-DROP"

echo "=== Auth Commands ==="
$MCLI auth-token && check "AUTH-TOKEN"

echo "=== Admin Commands ==="
$MCLI stats && check "STATS"
$MCLI pool-stats && check "POOL-STATS"
$MCLI cluster-info && check "CLUSTER-INFO"
$MCLI cluster-slots && check "CLUSTER-SLOTS"
$MCLI keeper-list && check "KEEPER-LIST"
$MCLI slowlog && check "SLOWLOG"
$MCLI metrics && check "METRICS"
$MCLI memory-usage test:str && check "MEMORY-USAGE"
$MCLI config memory.pool_bytes && check "CONFIG"
$MCLI config-set memory.eviction_threshold 0.90 && check "CONFIG-SET"
$MCLI join-token && check "JOIN-TOKEN"

echo "=== User Management ==="
$MCLI user-create testuser pass123 && check "USER-CREATE"
$MCLI user-list && check "USER-LIST"
$MCLI user-info testuser && check "USER-INFO"
$MCLI user-role testuser readonly && check "USER-ROLE"
$MCLI user-grant testuser 1 && check "USER-GRANT"
$MCLI user-revoke testuser 1 && check "USER-REVOKE"
$MCLI user-delete testuser && check "USER-DELETE"

echo "=== Profile Commands (local, no server) ==="
$MCLI profile-set testprofile --host localhost:6379 && check "PROFILE-SET"
$MCLI profile-list && check "PROFILE-LIST"
$MCLI profile-show testprofile && check "PROFILE-SHOW"
$MCLI profile-delete testprofile && check "PROFILE-DELETE"

echo "=== Consistency Levels ==="
$MCLI set c:eventual val --consistency eventual && check "SET EVENTUAL"
$MCLI set c:one val --consistency one && check "SET ONE"
$MCLI set c:quorum val --consistency quorum && check "SET QUORUM"
$MCLI set c:all val --consistency all && check "SET ALL"

echo ""
echo "========================================"
echo "  RESULT: $PASS passed, $FAIL failed"
echo "========================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
```

---

## 18. CLI Command Reference

All 70 commands available in `mneme-cli`, grouped by category.

### Global flags

| Flag | Short | Description |
|------|-------|-------------|
| `--host <addr>` | `-H` | Server address (default: `127.0.0.1:6379`) |
| `--username <user>` | `-u` | Username for auth |
| `--password <pass>` | `-p` | Password for auth |
| `--token <tok>` | `-t` | Session token (alternative to -u/-p) |
| `--ca-cert <path>` | | CA certificate path (e.g. `/etc/mneme/ca.crt`) |
| `--consistency <level>` | `-c` | `eventual`, `one`, `quorum` (default), `all` |
| `--db <id>` | `-d` | Database ID (numeric) |
| `--profile <name>` | `-P` | Use a saved connection profile |

### Command table

| # | Command | Arguments | Category | Description |
|---|---------|-----------|----------|-------------|
| 1 | `set` | `key value [--ttl N]` | Strings | Set a key to a string/bytes value |
| 2 | `get` | `key` | Strings | Get the value of a key |
| 3 | `del` | `key [key ...]` | Strings | Delete one or more keys |
| 4 | `exists` | `key` | Strings | Check if a key exists |
| 5 | `expire` | `key seconds` | Strings | Set a TTL on an existing key |
| 6 | `ttl` | `key` | Strings | Get remaining TTL in seconds |
| 7 | `getset` | `key value` | Strings | Atomically set and return old value |
| 8 | `incr` | `key` | Counters | Increment integer value by 1 |
| 9 | `decr` | `key` | Counters | Decrement integer value by 1 |
| 10 | `incrby` | `key delta` | Counters | Increment integer value by N |
| 11 | `decrby` | `key delta` | Counters | Decrement integer value by N |
| 12 | `incrbyfloat` | `key delta` | Counters | Increment float value by N |
| 13 | `hset` | `key field value [f v ...]` | Hashes | Set one or more hash fields |
| 14 | `hget` | `key field` | Hashes | Get a single hash field |
| 15 | `hdel` | `key field [field ...]` | Hashes | Delete hash fields |
| 16 | `hgetall` | `key` | Hashes | Get all hash fields and values |
| 17 | `lpush` | `key value [value ...]` | Lists | Push to the left (head) |
| 18 | `rpush` | `key value [value ...]` | Lists | Push to the right (tail) |
| 19 | `lpop` | `key` | Lists | Pop from the left |
| 20 | `rpop` | `key` | Lists | Pop from the right |
| 21 | `lrange` | `key start stop` | Lists | Get range of elements |
| 22 | `zadd` | `key score member [s m ...]` | Sorted Sets | Add members with scores |
| 23 | `zscore` | `key member` | Sorted Sets | Get score of a member |
| 24 | `zrank` | `key member` | Sorted Sets | Get rank of a member |
| 25 | `zrange` | `key start stop [--withscores]` | Sorted Sets | Get members by rank |
| 26 | `zrangebyscore` | `key min max` | Sorted Sets | Get members by score range |
| 27 | `zrem` | `key member [member ...]` | Sorted Sets | Remove members |
| 28 | `zcard` | `key` | Sorted Sets | Count members |
| 29 | `json-set` | `key path value` | JSON | Set JSON value at path |
| 30 | `json-get` | `key path` | JSON | Get JSON value at path |
| 31 | `json-del` | `key path` | JSON | Delete JSON path |
| 32 | `json-exists` | `key path` | JSON | Check if path exists |
| 33 | `json-type` | `key path` | JSON | Get JSON type at path |
| 34 | `json-arrappend` | `key path value` | JSON | Append to JSON array |
| 35 | `json-numincrby` | `key path delta` | JSON | Increment JSON number |
| 36 | `mget` | `key [key ...]` | Bulk | Get multiple keys |
| 37 | `mset` | `key val [k v ...] [--ttl N]` | Bulk | Set multiple keys |
| 38 | `scan` | `pattern [--cursor N] [--count N]` | Discovery | Cursor-based key iteration |
| 39 | `type` | `key` | Discovery | Return the type of a key |
| 40 | `select` | `db` | Database | Switch active database |
| 41 | `dbsize` | `[--db name]` | Database | Count keys in a database |
| 42 | `flushdb` | `[--db name] [--no-sync]` | Database | Delete all keys in a database |
| 43 | `db-create` | `name [--id N]` | Database | Create a named database |
| 44 | `db-list` | | Database | List all named databases |
| 45 | `db-drop` | `name` | Database | Remove a named database |
| 46 | `auth-token` | | Auth | Authenticate and print session token |
| 47 | `revoke-token` | `token` | Auth | Revoke a session token |
| 48 | `stats` | | Admin | Show overall stats |
| 49 | `pool-stats` | | Admin | Show RAM pool statistics |
| 50 | `cluster-info` | | Admin | Show cluster topology and state |
| 51 | `cluster-slots` | | Admin | Show slot-to-node assignments |
| 52 | `keeper-list` | | Admin | List connected Keeper nodes |
| 53 | `slowlog` | `[count]` | Admin | Show recent slow commands |
| 54 | `monitor` | | Admin | Stream all commands in real time |
| 55 | `metrics` | | Admin | Show Prometheus metrics |
| 56 | `memory-usage` | `key` | Admin | Show memory usage of a key |
| 57 | `config` | `param` | Admin | Read a live config parameter |
| 58 | `config-set` | `param value` | Admin | Set a live config parameter |
| 59 | `join-token` | | Admin | Print the join token (admin only) |
| 60 | `wait` | `n_keepers timeout_ms` | Admin | Wait for N Keeper ACKs |
| 61 | `user-create` | `username password [--role R]` | Users | Create user (admin only) |
| 62 | `user-delete` | `username` | Users | Delete user (admin only) |
| 63 | `user-list` | | Users | List all users |
| 64 | `user-info` | `[username]` | Users | Show user info |
| 65 | `user-role` | `username role` | Users | Change user role (admin only) |
| 66 | `user-grant` | `username db_id` | Users | Grant DB access (admin only) |
| 67 | `user-revoke` | `username db_id` | Users | Revoke DB access (admin only) |
| 68 | `profile-set` | `name [--host H] [--ca-cert C] ...` | Profiles | Create/update profile |
| 69 | `profile-list` | | Profiles | List all profiles |
| 70 | `profile-show` | `[name]` | Profiles | Show profile details |
| 71 | `profile-delete` | `name` | Profiles | Delete a profile |

---

## 19. Troubleshooting

### Connection refused on port 6379

**Cause:** Core is not running or the port is not open.

```bash
sudo systemctl status mneme-core
sudo ss -tlnp | grep 6379
```

**Fix:** Start the service and check firewall rules.

### Error: TokenInvalid

**Cause:** Wrong password, or the token has been revoked.

```bash
# Re-authenticate
mneme-cli -u admin -p <PASSWORD> auth-token
```

### Error: TokenExpired

**Cause:** Session token has exceeded `auth.token_ttl_h` (default 24 hours).

**Fix:** Re-authenticate to get a fresh token.

### Error: QuorumNotReached

**Cause:** Not enough Keepers are available to form a quorum.

```bash
# Check Keeper status
mneme-cli keeper-list
mneme-cli cluster-info

# Verify Keeper services are running
sudo systemctl status mneme-keeper-*
```

**Fix:** Bring Keepers back online, or temporarily use `--consistency eventual`
for non-critical operations.

### Error: OutOfMemory

**Cause:** Core RAM pool is exhausted.

```bash
mneme-cli pool-stats
mneme-cli config memory.pool_bytes
```

**Fix:**
1. Increase the pool: `mneme-cli config-set memory.pool_bytes 4gb`
2. Add more Keeper nodes to extend the logical pool
3. Lower the eviction threshold: `mneme-cli config-set memory.eviction_threshold 0.85`

### Keepers not appearing in keeper-list

**Cause:** Registration failed -- wrong join token, network issue, or mTLS
misconfiguration.

```bash
# Check Keeper logs
sudo journalctl -u mneme-keeper-0 -f

# Verify the replication port is reachable
nc -zv core-ip 7379
```

**Fix:** Ensure the Keeper has the correct CA cert, join token, and can reach
the Core on port 7379.

### Warmup stuck in "Warming" state

**Cause:** A Keeper has not finished pushing its keys back to Core after
restart.

```bash
mneme-cli cluster-info
# Check warmup_state and pending count
```

**Fix:** Wait for all Keepers to finish the push phase. If a Keeper is down,
bring it back online. QUORUM and ALL reads are blocked until warmup completes.

### High replication_lag_ms

**Cause:** Network congestion between Core and Keepers, or a Keeper is
overloaded with WAL syncs.

```bash
mneme-cli cluster-info
# Check replication_lag_ms per Keeper

curl http://core-ip:9090/metrics | grep replication_lag
```

**Fix:**
1. Check network between Core and Keepers
2. Move Keepers to faster storage (SSD/NVMe for WAL)
3. Increase Keeper I/O throughput

### Slow commands (high p99 latency)

```bash
mneme-cli slowlog 20

# Check if eviction is running (competing for CPU)
mneme-cli pool-stats
```

**Fix:**
1. Increase pool size to reduce eviction pressure
2. Use `--consistency eventual` for latency-sensitive reads
3. Add read replicas for read scaling

### TLS handshake failures

**Cause:** CA cert mismatch, expired certificate, or `server_name` mismatch.

```bash
# Verify the CA cert is correct
openssl x509 -in /etc/mneme/ca.crt -text -noout

# Check server_name matches the config
grep server_name /etc/mneme/core.toml
```

**Fix:** Ensure all nodes and clients use the same CA cert. Regenerate
certificates if expired. The `server_name` in the client config must match
`tls.server_name` in `core.toml`.

### Docker: TLS connection issues

Ensure the CA cert is available. All containers expose `/etc/mneme/ca.crt` via
the entrypoint symlink.

```bash
docker exec mneme-core mneme-cli --host $MNEME_HOST --ca-cert /etc/mneme/ca.crt -u admin -p secret stats
```

### Payload too large error

**Cause:** Key, value, or batch size exceeds limits.

| Limit | Value |
|-------|-------|
| Max key size | 512 bytes |
| Max value size | 10 MB |
| Max fields per hash | 65,536 |
| Max batch keys | 1,000 |

**Fix:** Split large values into smaller chunks or reduce batch size.

---

## 20. Glossary

| Term | Definition |
|------|-----------|
| **God node** | The primary Core node (Mnemosyne) that holds all hot data in RAM. Never touches disk. All client connections go through the God node. |
| **Keeper** | A persistence node (Hypnos) that stores data via WAL, snapshots, and cold store. Provides durability guarantees for QUORUM/ALL writes. |
| **WAL** | Write-Ahead Log (Aoide). Sequential append-only log using O_DIRECT + fallocate for crash recovery. |
| **Cold store** | The Oneiros redb B-tree. Holds evicted keys on disk. Access latency ~1.2 ms vs ~150 us for RAM. |
| **Snapshot** | Periodic full-state capture by Melete. Used for faster recovery than WAL replay alone. |
| **Hot set** | Keys currently residing in Core RAM. Sub-150 us access. |
| **Cold set** | Keys evicted to Oneiros on a Keeper. Accessible but slower (~1.2 ms). |
| **Warmup** | After Core restart, Keepers push all their keys back to Core. Progresses through Cold -> Warming -> Hot states. QUORUM/ALL reads are gated until Hot. |
| **Warmup gate** | The mechanism that blocks QUORUM and ALL reads while warmup is in progress (Cold or Warming state). EVENTUAL reads are always served. |
| **Eviction** | Moving cold keys from RAM to Oneiros when memory pressure exceeds the threshold. Reversible -- keys can be promoted back on access. |
| **TTL expiry** | Permanent deletion of a key when its time-to-live expires. Not reversible (unlike eviction). |
| **LFU Morris counter** | A probabilistic access counter requiring only a few bits per key. Used by Lethe to identify the coldest keys for eviction. |
| **TTL wheel** | A 3-level hierarchical timing wheel used by Lethe for efficient TTL expiry. Levels: 10 ms, 1 s, 60 s, 3600 s buckets. |
| **Slot** | One of 16,384 hash slots (CRC16 % 16384) used by Iris for key routing. |
| **Replication fabric** | The Hermes mTLS multiplexed connection between Core and Keepers. One persistent connection per Keeper, no connection pool needed. |
| **Quorum** | floor(N/2) + 1 Keeper acknowledgments required for a QUORUM write to succeed. The default consistency level. |
| **Join bundle** | A base64-encoded string containing the CA cert, cluster secret, and join token. Used to add new Keepers to a cluster without manual cert distribution. |
| **Join token** | A CSPRNG-generated token that Keepers present during registration. Validated by Core during the Herold handshake. |
| **Raft** | Distributed consensus algorithm (openraft). Used by Themis for leader election among Core nodes. |
| **RBAC** | Role-Based Access Control. Three roles: admin (full access), readwrite (data ops), readonly (read-only). |
| **Per-DB allowlist** | A list of database IDs that a user is allowed to access. Empty means access to all databases. |
| **mTLS** | Mutual TLS. Both client and server present certificates. Used on the replication port (7379) between Core and Keepers. |
| **Profile** | A named set of CLI connection parameters stored in `~/.mneme/profiles.toml`. |
| **Pressure ratio** | `pool_bytes_used / pool_bytes_max`. When >= eviction_threshold (default 0.90), proactive eviction begins. When >= 1.0, OOM eviction drops keys. |
| **Multiplexing** | Sending multiple requests over a single connection with out-of-order responses, identified by `req_id`. Used by both Hermes (Core-Keeper) and Pontus (client). |
| **msgpack** | MessagePack binary serialization format. Used for all wire payloads after the 16-byte binary header. |
| **rcgen** | Rust certificate generator. Used to auto-generate TLS certificates on first boot. No OpenSSL dependency. |
