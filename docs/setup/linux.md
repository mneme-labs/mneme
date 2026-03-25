# MnemeCache -- Linux Setup Guide

Linux is the **primary and production-supported platform** for MnemeCache.
All high-performance kernel APIs -- `perf_event_open`, `O_DIRECT`, `fallocate`,
`MAP_HUGETLB`, `MADV_HUGEPAGE` -- are available and enabled automatically.

This guide covers four cluster modes, from a single development node to a
fully monitored production deployment.

| Mode | Topology | Use case |
|------|----------|----------|
| **Solo** | 1 node (Core + embedded Keeper) | Development, testing |
| **Core + Keepers** | 1 Mnemosyne + N Hypnos | Durable QUORUM writes |
| **Core + Keepers + Read Replica** | Above + read-only Core replicas | EVENTUAL read scaling |
| **Full Production** | 1 Core + 3 Keepers + 2 Replicas + Prometheus | Production workloads |

---

## 1 -- System Requirements

| Requirement | Minimum | Recommended (production) | Notes |
|-------------|---------|--------------------------|-------|
| Linux kernel | **5.19+** | 6.1 LTS or newer | `perf_event_open` flags, `io_uring` ABI v6 |
| glibc | **2.38+** | matches `rust:latest` / `debian:trixie-slim` | |
| CPU | 2 cores | 8+ cores | one thread per core; `performance` governor |
| RAM | 2 GB free | 16+ GB per Core, 4+ GB per Keeper | Core pool lives entirely in RAM |
| Disk (Keepers) | 20 GB SSD | NVMe, 2x expected dataset | WAL + snapshots + Oneiros cold B-tree |
| Network | 1 Gbps | 10 Gbps, NUMA-aware NICs | replication is bandwidth-sensitive |

Recommended distros: Ubuntu 24.04 LTS, Debian 12/13, Fedora 40+, RHEL 9+.

Check your kernel version:

```bash
uname -r
# Must be >= 5.19
```

---

## 2 -- Prerequisites

### Rust toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup update stable
```

Minimum Rust version: **1.85 stable**.

### System dependencies

```bash
# Debian / Ubuntu
sudo apt-get update && sudo apt-get install -y \
    build-essential pkg-config clang libclang-dev linux-libc-dev

# Fedora / RHEL
sudo dnf install -y gcc pkg-config clang clang-devel kernel-headers
```

---

## 3 -- Optional: Huge Pages

MnemeCache falls back to 4 KB pages gracefully, but huge pages reduce TLB
pressure and improve p99 latency under load.

```bash
# One-time (lost on reboot) -- allocate 512 x 2 MB = 1 GB
echo 512 | sudo tee /proc/sys/vm/nr_hugepages

# Persistent -- add to /etc/sysctl.d/mneme.conf
sudo tee /etc/sysctl.d/mneme-hugepages.conf <<'EOF'
vm.nr_hugepages = 512
EOF
sudo sysctl --system
```

Verify allocation:

```bash
grep HugePages /proc/meminfo
# HugePages_Total:     512
# HugePages_Free:      512
```

---

## 4 -- Build from Source

```bash
git clone https://github.com/mneme-labs/mneme.git
cd mneme

# Release build with native CPU optimizations (excludes benchmarks)
RUSTFLAGS="-C target-cpu=native" \
cargo build --release --workspace --exclude mneme-bench

# Verify binaries
ls -lh target/release/mneme-{core,keeper,cli}
```

---

## 5 -- Install System-wide

```bash
# Install binaries
sudo install -m 755 target/release/mneme-core   /usr/local/bin/
sudo install -m 755 target/release/mneme-keeper  /usr/local/bin/
sudo install -m 755 target/release/mneme-cli     /usr/local/bin/

# Create the mneme system user (no login shell)
sudo useradd -r -s /usr/sbin/nologin -d /var/lib/mneme -m mneme

# Create directories
sudo mkdir -p /etc/mneme /var/lib/mneme
sudo chown mneme:mneme /var/lib/mneme
sudo chmod 750 /var/lib/mneme
```

---

## 6 -- Mode 1: Solo Node

A solo node runs Mnemosyne with an embedded Keeper in a single process.
Data is persisted to WAL and snapshots locally. Ideal for development,
testing, and single-machine deployments.

### Configuration

```bash
sudo tee /etc/mneme/solo.toml <<'EOF'
[node]
role         = "solo"
node_id      = "mneme-solo-0"
bind         = "127.0.0.1"
port         = 6379
rep_port     = 7379
metrics_port = 9090

[memory]
pool_bytes         = "1gb"
eviction_threshold = 0.90
huge_pages         = false

[persistence]
wal_dir             = "/var/lib/mneme"
snapshot_interval_s = 60
wal_max_mb          = 256

[tls]
cert          = "/var/lib/mneme/node.crt"
key           = "/var/lib/mneme/node.key"
ca_cert       = "/var/lib/mneme/ca.crt"
auto_generate = true
server_name   = "mneme-solo"

[auth]
users_db       = "/var/lib/mneme/users.db"
cluster_secret = ""            # set via MNEME_CLUSTER_SECRET
token_ttl_h    = 24

[logging]
level  = "info"
format = "json"
EOF
```

### systemd unit

```bash
sudo tee /etc/systemd/system/mneme-solo.service <<'EOF'
[Unit]
Description=MnemeCache Solo node (Core + embedded Keeper)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=mneme
Group=mneme
EnvironmentFile=/etc/mneme/solo.env
ExecStart=/usr/local/bin/mneme-core --config /etc/mneme/solo.toml
Restart=on-failure
RestartSec=5
LimitNOFILE=1048576
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
EOF

# Create the env file with secrets (mode 600, owned by root)
sudo tee /etc/mneme/solo.env <<'EOF'
MNEME_CLUSTER_SECRET=CHANGE_ME_GENERATE_WITH_openssl_rand_base64_32
MNEME_ADMIN_PASSWORD=CHANGE_ME_GENERATE_WITH_openssl_rand_base64_16
EOF
sudo chmod 600 /etc/mneme/solo.env
```

Generate real secrets and update the env file:

```bash
SECRET=$(openssl rand -base64 32)
ADMIN_PW=$(openssl rand -base64 16)
sudo sed -i "s|CHANGE_ME_GENERATE_WITH_openssl_rand_base64_32|${SECRET}|" /etc/mneme/solo.env
sudo sed -i "s|CHANGE_ME_GENERATE_WITH_openssl_rand_base64_16|${ADMIN_PW}|" /etc/mneme/solo.env
echo "Admin password: ${ADMIN_PW}  (save this)"
```

Start the service:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now mneme-solo
```

### Verify

```bash
# Check service status
sudo systemctl status mneme-solo

# Ping the node
mneme-cli --host 127.0.0.1:6379 ping

# Set and get a key
mneme-cli --host 127.0.0.1:6379 set testkey "hello from solo"
mneme-cli --host 127.0.0.1:6379 get testkey

# Check cluster info
mneme-cli --host 127.0.0.1:6379 cluster-info
```

---

## 7 -- Mode 2: Core + Keepers

One Mnemosyne Core holds the hot dataset in RAM. One or more Hypnos Keepers
persist data via WAL, snapshots, and the Oneiros cold B-tree store. Writes
use QUORUM consistency by default (floor(N/2)+1 Keeper ACKs required).

### 7.1 Generate the cluster secret

Every node in the cluster must share the same `cluster_secret`. Generate it
once and distribute securely:

```bash
CLUSTER_SECRET=$(openssl rand -base64 32)
ADMIN_PASSWORD=$(openssl rand -base64 16)
echo "cluster_secret: ${CLUSTER_SECRET}"
echo "admin_password: ${ADMIN_PASSWORD}"
```

Save both values. They are needed on every node.

### 7.2 Core configuration

On the Core host:

```bash
sudo tee /etc/mneme/core.toml <<'EOF'
[node]
role         = "core"
node_id      = "mneme-core-0"
bind         = "0.0.0.0"
port         = 6379
rep_port     = 7379
metrics_port = 9090

[memory]
pool_bytes         = "8gb"
eviction_threshold = 0.90
huge_pages         = true

[tls]
cert          = "/var/lib/mneme/node.crt"
key           = "/var/lib/mneme/node.key"
ca_cert       = "/var/lib/mneme/ca.crt"
auto_generate = true
server_name   = "mneme-core"

[auth]
users_db       = "/var/lib/mneme/users.db"
cluster_secret = ""            # set via MNEME_CLUSTER_SECRET
token_ttl_h    = 24

[logging]
level  = "info"
format = "json"
EOF
```

### 7.3 Core systemd unit

```bash
sudo tee /etc/systemd/system/mneme-core.service <<'EOF'
[Unit]
Description=MnemeCache Core node (Mnemosyne)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=mneme
Group=mneme
EnvironmentFile=/etc/mneme/core.env
ExecStart=/usr/local/bin/mneme-core --config /etc/mneme/core.toml
Restart=on-failure
RestartSec=5
LimitNOFILE=1048576
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
EOF

sudo tee /etc/mneme/core.env <<'EOF'
MNEME_CLUSTER_SECRET=<paste-cluster-secret-here>
MNEME_ADMIN_PASSWORD=<paste-admin-password-here>
EOF
sudo chmod 600 /etc/mneme/core.env

sudo systemctl daemon-reload
sudo systemctl enable --now mneme-core
```

### 7.4 Distribute the CA certificate

When `auto_generate = true`, Mnemosyne generates a self-signed CA on first
start. All Keepers need this CA cert to establish mTLS.

```bash
# On the Core host -- copy the generated CA cert
sudo cat /var/lib/mneme/ca.crt

# On each Keeper host -- paste it
sudo tee /etc/mneme/ca.crt <<'EOF'
-----BEGIN CERTIFICATE-----
<paste CA cert contents here>
-----END CERTIFICATE-----
EOF
sudo chmod 644 /etc/mneme/ca.crt
```

Alternatively, use `scp`:

```bash
scp core-host:/var/lib/mneme/ca.crt /tmp/mneme-ca.crt
sudo install -m 644 /tmp/mneme-ca.crt /etc/mneme/ca.crt
```

### 7.5 Keeper configuration

Repeat on each Keeper host, changing `node_id` for each:

```bash
sudo tee /etc/mneme/keeper.toml <<'EOF'
[node]
role         = "keeper"
node_id      = "hypnos-0"          # unique per Keeper: hypnos-0, hypnos-1, ...
bind         = "0.0.0.0"
rep_port     = 7379
metrics_port = 9090
core_addr    = "CORE_IP:7379"      # replace CORE_IP with actual Core IP

[memory]
pool_bytes         = "2gb"
eviction_threshold = 0.90
huge_pages         = true

[persistence]
wal_dir             = "/var/lib/mneme"
snapshot_interval_s = 60
wal_max_mb          = 512

[tls]
cert          = "/var/lib/mneme/node.crt"
key           = "/var/lib/mneme/node.key"
ca_cert       = "/etc/mneme/ca.crt"
auto_generate = true
server_name   = "mneme-core"

[auth]
cluster_secret = ""                # set via MNEME_CLUSTER_SECRET

[logging]
level  = "info"
format = "json"
EOF
```

### 7.6 Keeper systemd unit

```bash
sudo tee /etc/systemd/system/mneme-keeper.service <<'EOF'
[Unit]
Description=MnemeCache Keeper node (Hypnos)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=mneme
Group=mneme
EnvironmentFile=/etc/mneme/keeper.env
ExecStart=/usr/local/bin/mneme-keeper --config /etc/mneme/keeper.toml
Restart=on-failure
RestartSec=5
LimitNOFILE=1048576
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
EOF

sudo tee /etc/mneme/keeper.env <<'EOF'
MNEME_CLUSTER_SECRET=<paste-cluster-secret-here>
EOF
sudo chmod 600 /etc/mneme/keeper.env

sudo systemctl daemon-reload
sudo systemctl enable --now mneme-keeper
```

### 7.7 Verify the cluster

```bash
# On the Core host
mneme-cli --host CORE_IP:6379 cluster-info
```

Expected output shows:

- `warmup_state: Hot` (QUORUM and ALL reads are blocked until this is reached)
- Each Keeper listed with its `node_id` and `sync_state: Synced`
- `supported_modes` includes `QUORUM`

```bash
# Write a key (uses QUORUM by default)
mneme-cli --host CORE_IP:6379 set hello "world"

# Read it back
mneme-cli --host CORE_IP:6379 get hello
```

---

## 8 -- Mode 3: Core + Keepers + Read Replica

A read replica is a second Mnemosyne Core running in `read-replica` role.
It receives the full dataset from the primary Core via Hermes replication
and serves EVENTUAL-consistency reads. It does not accept writes.

This mode is Mode 2 plus one or more read replicas.

### 8.1 Read replica configuration

On the replica host, after distributing the CA cert (see section 7.4):

```bash
sudo tee /etc/mneme/replica.toml <<'EOF'
[node]
role         = "read-replica"
node_id      = "mneme-replica-0"       # unique per replica
bind         = "0.0.0.0"
port         = 6379
rep_port     = 7379
metrics_port = 9090
core_addr    = "CORE_IP:7379"          # primary Core address

[memory]
pool_bytes         = "8gb"             # should match or exceed primary Core
eviction_threshold = 0.90
huge_pages         = true

[tls]
cert          = "/var/lib/mneme/node.crt"
key           = "/var/lib/mneme/node.key"
ca_cert       = "/etc/mneme/ca.crt"
auto_generate = true
server_name   = "mneme-core"

[auth]
cluster_secret = ""                    # set via MNEME_CLUSTER_SECRET

[logging]
level  = "info"
format = "json"
EOF
```

### 8.2 Read replica systemd unit

```bash
sudo tee /etc/systemd/system/mneme-replica.service <<'EOF'
[Unit]
Description=MnemeCache Read Replica (Mnemosyne EVENTUAL)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=mneme
Group=mneme
EnvironmentFile=/etc/mneme/replica.env
ExecStart=/usr/local/bin/mneme-core --config /etc/mneme/replica.toml
Restart=on-failure
RestartSec=5
LimitNOFILE=1048576
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
EOF

sudo tee /etc/mneme/replica.env <<'EOF'
MNEME_CLUSTER_SECRET=<paste-cluster-secret-here>
EOF
sudo chmod 600 /etc/mneme/replica.env

sudo systemctl daemon-reload
sudo systemctl enable --now mneme-replica
```

### 8.3 Verify replica lag

```bash
# Check cluster info on primary -- replicas appear in node list
mneme-cli --host CORE_IP:6379 cluster-info

# Read from replica (EVENTUAL consistency)
mneme-cli --host REPLICA_IP:6379 get hello
```

Monitor replication lag via Prometheus:

```
mneme_replication_lag_ms{keeper="mneme-replica-0"}
```

Read replica sync target: **< 3 seconds** under normal load.

---

## 9 -- Mode 4: Full Production Cluster

A production-grade deployment with durability, read scaling, and monitoring.

### 9.1 Capacity planning

| Component | Count | RAM | Disk | CPU | Purpose |
|-----------|-------|-----|------|-----|---------|
| Core (Mnemosyne) | 1 | 16--64 GB | minimal (no persistence) | 8+ cores | Hot dataset, routing |
| Keeper (Hypnos) | 3 | 4--8 GB each | NVMe, 2x dataset | 4+ cores | WAL, snapshots, cold store |
| Read Replica | 2 | 16--64 GB (match Core) | minimal | 8+ cores | EVENTUAL read offload |
| Prometheus | 1 | 2--4 GB | 50+ GB SSD | 2 cores | Metrics collection |

QUORUM with 3 Keepers requires 2 ACKs (floor(3/2)+1 = 2).

### 9.2 Network topology

```
                                       Clients
                                         |
                              +----------+----------+
                              |                     |
                         Core:6379           Replica-0:6379
                         Core:7379           Replica-1:6379
                         Core:9090              |
                              |            (replication
                    +---------+---------+    from Core)
                    |         |         |
               Keeper-0  Keeper-1  Keeper-2
                :7379      :7379      :7379
                :9090      :9090      :9090
```

### 9.3 Complete walkthrough

Assume the following hosts:

| Host | Role | IP |
|------|------|----|
| core-01 | Core | 10.0.1.10 |
| keeper-01 | Keeper | 10.0.1.11 |
| keeper-02 | Keeper | 10.0.1.12 |
| keeper-03 | Keeper | 10.0.1.13 |
| replica-01 | Read Replica | 10.0.1.21 |
| replica-02 | Read Replica | 10.0.1.22 |
| prom-01 | Prometheus | 10.0.1.30 |

**Step 1: Generate secrets (once, on any host)**

```bash
CLUSTER_SECRET=$(openssl rand -base64 32)
ADMIN_PASSWORD=$(openssl rand -base64 16)
echo "MNEME_CLUSTER_SECRET=${CLUSTER_SECRET}"
echo "MNEME_ADMIN_PASSWORD=${ADMIN_PASSWORD}"
```

Store these in a secrets manager. They are needed on every MnemeCache node.

**Step 2: Deploy Core (core-01)**

Install binaries and create the system user as in sections 4--5.

```bash
# /etc/mneme/core.toml
sudo tee /etc/mneme/core.toml <<'EOF'
[node]
role         = "core"
node_id      = "mneme-core-0"
bind         = "0.0.0.0"
port         = 6379
rep_port     = 7379
metrics_port = 9090

[memory]
pool_bytes         = "32gb"
eviction_threshold = 0.90
huge_pages         = true

[tls]
cert          = "/var/lib/mneme/node.crt"
key           = "/var/lib/mneme/node.key"
ca_cert       = "/var/lib/mneme/ca.crt"
auto_generate = true
server_name   = "mneme-core"

[auth]
users_db       = "/var/lib/mneme/users.db"
cluster_secret = ""
token_ttl_h    = 24

[logging]
level  = "info"
format = "json"
EOF

# /etc/mneme/core.env
sudo tee /etc/mneme/core.env <<EOF
MNEME_CLUSTER_SECRET=${CLUSTER_SECRET}
MNEME_ADMIN_PASSWORD=${ADMIN_PASSWORD}
EOF
sudo chmod 600 /etc/mneme/core.env
```

Install the Core systemd unit from section 7.3 and start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now mneme-core
```

**Step 3: Distribute the CA certificate**

```bash
# From core-01, copy to all other nodes:
for HOST in 10.0.1.11 10.0.1.12 10.0.1.13 10.0.1.21 10.0.1.22; do
    scp /var/lib/mneme/ca.crt ${HOST}:/tmp/mneme-ca.crt
    ssh ${HOST} "sudo install -m 644 /tmp/mneme-ca.crt /etc/mneme/ca.crt"
done
```

**Step 4: Deploy Keepers (keeper-01, keeper-02, keeper-03)**

On each Keeper host, install binaries and create the system user as in
sections 4--5. Then configure with a unique `node_id`:

```bash
# Replace KEEPER_ID and KEEPER_NUM for each host:
#   keeper-01: node_id = "hypnos-0"
#   keeper-02: node_id = "hypnos-1"
#   keeper-03: node_id = "hypnos-2"

sudo tee /etc/mneme/keeper.toml <<'EOF'
[node]
role         = "keeper"
node_id      = "hypnos-0"             # CHANGE per Keeper
bind         = "0.0.0.0"
rep_port     = 7379
metrics_port = 9090
core_addr    = "10.0.1.10:7379"

[memory]
pool_bytes         = "4gb"
eviction_threshold = 0.90
huge_pages         = true

[persistence]
wal_dir             = "/var/lib/mneme"
snapshot_interval_s = 60
wal_max_mb          = 512

[tls]
cert          = "/var/lib/mneme/node.crt"
key           = "/var/lib/mneme/node.key"
ca_cert       = "/etc/mneme/ca.crt"
auto_generate = true
server_name   = "mneme-core"

[auth]
cluster_secret = ""

[logging]
level  = "info"
format = "json"
EOF

sudo tee /etc/mneme/keeper.env <<EOF
MNEME_CLUSTER_SECRET=${CLUSTER_SECRET}
EOF
sudo chmod 600 /etc/mneme/keeper.env
```

Install the Keeper systemd unit from section 7.6 and start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now mneme-keeper
```

**Step 5: Deploy Read Replicas (replica-01, replica-02)**

On each replica host, install binaries and create the system user. Then:

```bash
# replica-01: node_id = "mneme-replica-0"
# replica-02: node_id = "mneme-replica-1"

sudo tee /etc/mneme/replica.toml <<'EOF'
[node]
role         = "read-replica"
node_id      = "mneme-replica-0"       # CHANGE per replica
bind         = "0.0.0.0"
port         = 6379
rep_port     = 7379
metrics_port = 9090
core_addr    = "10.0.1.10:7379"

[memory]
pool_bytes         = "32gb"
eviction_threshold = 0.90
huge_pages         = true

[tls]
cert          = "/var/lib/mneme/node.crt"
key           = "/var/lib/mneme/node.key"
ca_cert       = "/etc/mneme/ca.crt"
auto_generate = true
server_name   = "mneme-core"

[auth]
cluster_secret = ""

[logging]
level  = "info"
format = "json"
EOF

sudo tee /etc/mneme/replica.env <<EOF
MNEME_CLUSTER_SECRET=${CLUSTER_SECRET}
EOF
sudo chmod 600 /etc/mneme/replica.env
```

Install the replica systemd unit from section 8.2 and start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now mneme-replica
```

**Step 6: Verify the full cluster**

```bash
# From any host that can reach the Core
mneme-cli --host 10.0.1.10:6379 cluster-info
```

Expected output:

- `warmup_state: Hot`
- 3 Keepers listed, all `sync_state: Synced`
- 2 read replicas listed
- `raft_term` and `is_leader` populated
- `supported_modes: EVENTUAL, ONE, QUORUM, ALL`

```bash
# Write through Core (QUORUM -- needs 2 of 3 Keeper ACKs)
mneme-cli --host 10.0.1.10:6379 set prod:key "production-value"

# Read from a replica (EVENTUAL)
mneme-cli --host 10.0.1.21:6379 get prod:key
```

### 9.4 Firewall rules

Open only the required ports between nodes:

| Port | Protocol | Direction | Purpose |
|------|----------|-----------|---------|
| **6379/tcp** | TLS 1.3 | Clients -> Core, Clients -> Replicas | Client connections |
| **7379/tcp** | mTLS | Core <-> Keepers, Core <-> Replicas | Replication (Hermes) |
| **9090/tcp** | HTTP | Prometheus -> all nodes | Metrics scraping |

Example using `ufw` on Core:

```bash
sudo ufw allow from any to any port 6379 proto tcp comment "MnemeCache client"
sudo ufw allow from 10.0.1.0/24 to any port 7379 proto tcp comment "MnemeCache replication"
sudo ufw allow from 10.0.1.30 to any port 9090 proto tcp comment "Prometheus scrape"
```

Example using `firewall-cmd` (RHEL/Fedora):

```bash
sudo firewall-cmd --permanent --add-port=6379/tcp
sudo firewall-cmd --permanent --add-rich-rule='rule family="ipv4" source address="10.0.1.0/24" port port="7379" protocol="tcp" accept'
sudo firewall-cmd --permanent --add-rich-rule='rule family="ipv4" source address="10.0.1.30" port port="9090" protocol="tcp" accept'
sudo firewall-cmd --reload
```

### 9.5 Prometheus scrape configuration

On the Prometheus host (`prom-01`), add to `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: "mneme-core"
    scrape_interval: 10s
    static_configs:
      - targets:
          - "10.0.1.10:9090"
        labels:
          role: "core"

  - job_name: "mneme-keepers"
    scrape_interval: 10s
    static_configs:
      - targets:
          - "10.0.1.11:9090"
          - "10.0.1.12:9090"
          - "10.0.1.13:9090"
        labels:
          role: "keeper"

  - job_name: "mneme-replicas"
    scrape_interval: 10s
    static_configs:
      - targets:
          - "10.0.1.21:9090"
          - "10.0.1.22:9090"
        labels:
          role: "read-replica"
```

Key metrics to monitor:

| Metric | Description | Alert threshold |
|--------|-------------|-----------------|
| `mneme_memory_pressure_ratio` | Used / max pool bytes | > 0.70 add Keeper, > 0.90 critical |
| `mneme_replication_lag_ms` | Per-Keeper replication delay | > 1000ms warning, > 5000ms critical |
| `mneme_connections_active` | Current client connections | > 80000 warning |
| `mneme_requests_total` | Request throughput by command | monitor trends |
| `mneme_request_duration_seconds` | Latency histogram | p99 > 1ms warning |
| `mneme_evictions_total{reason}` | Evictions by reason (lfu, ttl, oom) | oom > 0 critical |
| `mneme_wal_sync_duration_seconds` | WAL fsync latency | p99 > 10ms warning |
| `mneme_cluster_term` | Raft term (Themis) | unexpected increments |

---

## 10 -- Performance Tuning

### File descriptor limits

MnemeCache supports up to 100,000 concurrent connections. The systemd units
set `LimitNOFILE=1048576`, but the system-wide limits must also be raised:

```bash
sudo tee /etc/security/limits.d/mneme.conf <<'EOF'
mneme    soft    nofile    1048576
mneme    hard    nofile    1048576
EOF
```

### Transparent Huge Pages (THP)

Disable THP defragmentation to avoid latency spikes from compaction:

```bash
echo madvise | sudo tee /sys/kernel/mm/transparent_hugepage/defrag

# Persistent via systemd tmpfile
sudo tee /etc/tmpfiles.d/mneme-thp.conf <<'EOF'
w /sys/kernel/mm/transparent_hugepage/defrag - - - - madvise
EOF
```

### CPU governor

Lock the CPU to maximum frequency to avoid frequency scaling jitter:

```bash
sudo cpupower frequency-set -g performance
```

To persist across reboots, add a systemd service or use `tuned`:

```bash
sudo tuned-adm profile latency-performance
```

### NUMA pinning

On multi-socket systems, pin each MnemeCache process to a single NUMA node
to avoid cross-socket memory access:

```bash
# Check NUMA topology
numactl --hardware

# Pin Core to NUMA node 0
sudo tee /etc/systemd/system/mneme-core.service.d/numa.conf <<'EOF'
[Service]
ExecStart=
ExecStart=/usr/bin/numactl --cpunodebind=0 --membind=0 /usr/local/bin/mneme-core --config /etc/mneme/core.toml
EOF
sudo systemctl daemon-reload
sudo systemctl restart mneme-core
```

### Network tuning

```bash
sudo tee /etc/sysctl.d/mneme-net.conf <<'EOF'
# Increase socket buffer sizes for replication traffic
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
net.ipv4.tcp_rmem = 4096 87380 16777216
net.ipv4.tcp_wmem = 4096 65536 16777216

# Enable SO_REUSEPORT for multi-queue NICs
net.core.somaxconn = 65535
net.core.netdev_max_backlog = 65535

# Reduce TIME_WAIT sockets
net.ipv4.tcp_tw_reuse = 1
net.ipv4.tcp_fin_timeout = 15
EOF
sudo sysctl --system
```

---

## 11 -- Hardware Counters (Aletheia)

MnemeCache uses `perf_event_open` to expose hardware performance counters
through Aletheia metrics: L1/L2/L3 cache misses, TLB misses, branch
mispredictions, CPU cycles, and instructions retired.

### Enable perf_event_open for non-root users

```bash
# Allow the mneme user to access performance counters
echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid

# Persistent
sudo tee -a /etc/sysctl.d/mneme.conf <<'EOF'
kernel.perf_event_paranoid = 1
EOF
sudo sysctl --system
```

Values for `perf_event_paranoid`:

| Value | Access level |
|-------|-------------|
| -1 | No restrictions |
| 0 | Allow all non-sampling events |
| 1 | Allow non-sampling events for non-root (recommended) |
| 2 | Disallow all for non-root (default on many distros) |

### Exposed hardware metrics

All metrics are available at `http://<node>:9090/metrics`:

| Metric | Description |
|--------|-------------|
| `mneme_l1_cache_misses_total` | L1 data cache misses |
| `mneme_l2_cache_misses_total` | L2 cache misses |
| `mneme_l3_cache_misses_total` | Last-level cache misses |
| `mneme_tlb_misses_total` | Translation lookaside buffer misses |
| `mneme_branch_mispredictions_total` | Branch prediction failures |
| `mneme_cpu_cycles_total` | CPU cycles consumed |
| `mneme_instructions_total` | Instructions retired |

---

## 12 -- Troubleshooting

### Node fails to start

**Symptom:** `Address already in use`

```bash
# Check what is using the port
sudo ss -tlnp | grep -E '6379|7379|9090'
# Stop conflicting services or change the port in the config
```

**Symptom:** `Permission denied` on `/var/lib/mneme`

```bash
sudo chown -R mneme:mneme /var/lib/mneme
sudo chmod 750 /var/lib/mneme
```

**Symptom:** `LimitMEMLOCK` errors with huge pages

```bash
# Verify the systemd unit has LimitMEMLOCK=infinity
systemctl show mneme-core | grep LimitMEMLOCK
# Must show LimitMEMLOCKSoft=infinity
```

### Keeper cannot connect to Core

**Symptom:** `TLS handshake failed` or `certificate verify failed`

```bash
# Verify CA cert matches on both sides
openssl x509 -in /etc/mneme/ca.crt -noout -fingerprint -sha256
# Run this on both Core and Keeper -- fingerprints must match
```

**Symptom:** `connection refused` on port 7379

```bash
# On the Core host, verify the replication port is listening
sudo ss -tlnp | grep 7379

# Check firewall rules
sudo iptables -L -n | grep 7379
```

**Symptom:** `cluster_secret mismatch`

Verify that `MNEME_CLUSTER_SECRET` in the env file is identical across all
nodes. A trailing newline or whitespace difference will cause rejection.

### Warmup stuck (QUORUM/ALL reads blocked)

MnemeCache gates QUORUM and ALL reads until `warmup_state` reaches `Hot`.
The warmup counter decrements as each Keeper completes its SyncComplete
handshake.

```bash
# Check warmup state
mneme-cli --host CORE_IP:6379 cluster-info
# Look for warmup_state field

# Check individual Keeper sync status
# Each Keeper should show sync_state: Synced
```

If a Keeper is stuck in sync:
1. Check Keeper logs: `journalctl -u mneme-keeper -f`
2. Verify network connectivity to Core on port 7379
3. Check Keeper disk I/O -- slow WAL replay delays SyncComplete
4. Restart the stuck Keeper: `sudo systemctl restart mneme-keeper`

### High memory pressure / OOM evictions

```bash
# Check current pressure
curl -s http://CORE_IP:9090/metrics | grep mneme_memory_pressure_ratio

# If pressure_ratio > 0.90:
#   - Graduated eviction kicks in (1% LFU at threshold, 5% at OOM)
#   - Evictions logged as "lfu" or "oom" in Aletheia metrics
#   - Add more Keepers or increase pool_bytes
```

### Replication lag on read replicas

```bash
curl -s http://CORE_IP:9090/metrics | grep mneme_replication_lag_ms
```

If lag exceeds 3 seconds:
1. Check network bandwidth between Core and replica
2. Verify replica has enough CPU and memory
3. Look for packet loss: `ping -c 100 REPLICA_IP`
4. Check if replica is under heavy read load

### Viewing logs

```bash
# Core logs
sudo journalctl -u mneme-core -f

# Keeper logs
sudo journalctl -u mneme-keeper -f

# Read replica logs
sudo journalctl -u mneme-replica -f

# Solo node logs
sudo journalctl -u mneme-solo -f

# Filter for errors only
sudo journalctl -u mneme-core -p err --since "1 hour ago"
```

### Slow queries

MnemeCache includes a SLOWLOG (Delphi) for diagnosing latency issues:

```bash
# View recent slow queries
mneme-cli --host CORE_IP:6379 slowlog

# Monitor all commands in real time
mneme-cli --host CORE_IP:6379 monitor
```

### Performance targets reference

| Operation | Target p99 |
|-----------|-----------|
| GET EVENTUAL (RAM hit) | < 150 us |
| GET (Oneiros cold fetch) | < 1.2 ms |
| SET QUORUM | < 800 us |
| Token validation | ~75 ns |
| Core restart (hot) | < 15 s |
| Read replica sync | < 3 s |
