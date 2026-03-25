# Mneme — Docker Compose Examples

All examples use the official images from Docker Hub. Clone the repo to get the full `docker-compose.yml` with all profiles, or copy the snippets below for a standalone setup.

```bash
git clone https://github.com/mneme-labs/mneme && cd mneme
```

---

## Topologies

| Profile | Command | Use case |
|---------|---------|----------|
| `solo` | `--profile solo` | Development, CI, single-server |
| `cluster` | `--profile cluster` | Production: Core + 3 Keepers |
| `cluster` + `replica` | `--profile cluster --profile replica` | Cluster + read scale-out |
| `ha` | `--profile ha` | HA: 3-Core Raft + 2 Keepers |
| `ha-full` | `--profile ha-full` | HA + HA-aware replicas + monitoring |
| `full` | `--profile full` | Core + 3 Keepers + 2 replicas + Prometheus + Grafana |

---

## Solo (development)

One container — Core with embedded persistence. Best for development, CI, and single-server setups.

```bash
docker compose --profile solo up -d
```

Or without the repo:

```yaml
# docker-compose.yml
services:
  mneme:
    image: mnemelabs/core:0.1.0
    container_name: mneme-solo
    entrypoint: ["/bin/bash", "/docker/entrypoint-solo.sh"]
    user: root
    environment:
      MNEME_ADMIN_PASSWORD: secret
      MNEME_POOL_BYTES: 512mb
    ports:
      - "6379:6379"
      - "9090:9090"
    volumes:
      - mneme-data:/var/lib/mneme
    restart: unless-stopped

volumes:
  mneme-data:
```

Connect:

```bash
docker exec mneme-solo mneme-cli -u admin -p secret ping
# PONG
docker exec mneme-solo mneme-cli -u admin -p secret set hello world
docker exec mneme-solo mneme-cli -u admin -p secret get hello
# world
```

---

## Cluster — Core + 3 Keepers

Single Core node with 3 Keeper nodes for WAL persistence and QUORUM writes.

```bash
MNEME_ADMIN_PASSWORD=secret docker compose --profile cluster up -d
```

Standalone compose:

```yaml
# docker-compose.yml
services:
  mneme-core:
    image: mnemelabs/core:0.1.0
    container_name: mneme-core
    entrypoint: ["/bin/bash", "/docker/entrypoint-core.sh"]
    user: root
    environment:
      MNEME_ADMIN_PASSWORD: secret
      MNEME_CLUSTER_SECRET: change-me-in-production
      MNEME_POOL_BYTES: 2gb
    ports:
      - "6379:6379"
      - "7379:7379"
      - "9090:9090"
    volumes:
      - core-data:/var/lib/mneme
    restart: unless-stopped

  mneme-keeper-1:
    image: mnemelabs/keeper:0.1.0
    container_name: mneme-keeper-1
    entrypoint: ["/bin/bash", "/docker/entrypoint-keeper.sh"]
    user: root
    environment:
      MNEME_CLUSTER_SECRET: change-me-in-production
      MNEME_CORE_ADDR: mneme-core:7379
      MNEME_KEEPER_ID: keeper-1
    volumes:
      - keeper1-data:/var/lib/mneme
    depends_on:
      - mneme-core
    restart: unless-stopped

  mneme-keeper-2:
    image: mnemelabs/keeper:0.1.0
    container_name: mneme-keeper-2
    entrypoint: ["/bin/bash", "/docker/entrypoint-keeper.sh"]
    user: root
    environment:
      MNEME_CLUSTER_SECRET: change-me-in-production
      MNEME_CORE_ADDR: mneme-core:7379
      MNEME_KEEPER_ID: keeper-2
    volumes:
      - keeper2-data:/var/lib/mneme
    depends_on:
      - mneme-core
    restart: unless-stopped

  mneme-keeper-3:
    image: mnemelabs/keeper:0.1.0
    container_name: mneme-keeper-3
    entrypoint: ["/bin/bash", "/docker/entrypoint-keeper.sh"]
    user: root
    environment:
      MNEME_CLUSTER_SECRET: change-me-in-production
      MNEME_CORE_ADDR: mneme-core:7379
      MNEME_KEEPER_ID: keeper-3
    volumes:
      - keeper3-data:/var/lib/mneme
    depends_on:
      - mneme-core
    restart: unless-stopped

volumes:
  core-data:
  keeper1-data:
  keeper2-data:
  keeper3-data:
```

---

## Cluster + Read Replicas

Core + 3 Keepers + 2 read replicas. Replicas serve EVENTUAL reads directly from their local RAM pool.

```bash
MNEME_ADMIN_PASSWORD=secret \
  docker compose --profile cluster --profile replica up -d
```

Add these services to the cluster compose above:

```yaml
  mneme-replica-1:
    image: mnemelabs/core:0.1.0
    container_name: mneme-replica-1
    entrypoint: ["/bin/bash", "/docker/entrypoint-replica.sh"]
    user: root
    environment:
      MNEME_CLUSTER_SECRET: change-me-in-production
      MNEME_CORE_ADDR: mneme-core:7379
      MNEME_NODE_ID: replica-1
      MNEME_POOL_BYTES: 1gb
    ports:
      - "6380:6379"
    volumes:
      - replica1-data:/var/lib/mneme
    depends_on:
      - mneme-core
    restart: unless-stopped

  mneme-replica-2:
    image: mnemelabs/core:0.1.0
    container_name: mneme-replica-2
    entrypoint: ["/bin/bash", "/docker/entrypoint-replica.sh"]
    user: root
    environment:
      MNEME_CLUSTER_SECRET: change-me-in-production
      MNEME_CORE_ADDR: mneme-core:7379
      MNEME_NODE_ID: replica-2
      MNEME_POOL_BYTES: 1gb
    ports:
      - "6381:6379"
    volumes:
      - replica2-data:/var/lib/mneme
    depends_on:
      - mneme-core
    restart: unless-stopped
```

Read from a replica:

```bash
docker exec mneme-replica-1 mneme-cli -u admin -p secret \
  --consistency EVENTUAL get mykey
```

---

## HA — 3-Core Raft + 2 Keepers

Three Core nodes form a Raft cluster. Leader election is automatic — failover in under 5 seconds.

```bash
MNEME_ADMIN_PASSWORD=secret docker compose --profile ha up -d

# Check which node is leader
docker exec mneme-core-1 mneme-cli -u admin -p secret cluster-info
```

---

## HA-Full — HA + Replicas + Monitoring

Three Core nodes (Raft) + 2 Keepers + 2 HA-aware read replicas + Prometheus + Grafana.

```bash
MNEME_ADMIN_PASSWORD=secret docker compose --profile ha-full up -d

# Grafana dashboard
open http://localhost:3000   # admin / admin
```

---

## Full — Cluster + Replicas + Monitoring

Single Core + 3 Keepers + 2 read replicas + Prometheus + Grafana.

```bash
MNEME_ADMIN_PASSWORD=secret docker compose --profile full up -d

# Prometheus metrics
curl http://localhost:9090/metrics

# Grafana
open http://localhost:3000   # admin / admin
```

---

## Production overrides

Use `docker-compose.prod.yml` for resource limits, memory caps, and log rotation:

```bash
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

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MNEME_ADMIN_PASSWORD` | `secret` | Admin password — **change in production** |
| `MNEME_CLUSTER_SECRET` | — | Shared secret for Keeper join auth (cluster / HA) |
| `MNEME_POOL_BYTES` | `512mb` | Hot RAM pool size (`512mb`, `2gb`, `8gb`, …) |
| `MNEME_LOG_LEVEL` | `info` | Log verbosity: `trace` / `debug` / `info` / `warn` / `error` |
| `MNEME_VERSION` | `0.1.0` | Image tag |
| `MNEME_NODE_ID` | hostname | Raft / cluster node identifier |

---

## Tear down

```bash
# Stop and remove containers
docker compose down

# Stop and remove containers + all volumes (data loss!)
docker compose down -v
```

---

## Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| `6379` | TLS 1.3 | Client connections (Core / replica) |
| `7379` | mTLS | Replication (Core ↔ Keeper, Raft peers) |
| `9090` | HTTP | Prometheus metrics |
| `3000` | HTTP | Grafana dashboard (monitoring profile) |

---

## Images

| Image | Description |
|-------|-------------|
| `mnemelabs/core:0.1.0` | Core node — solo / cluster / HA / replica mode |
| `mnemelabs/keeper:0.1.0` | Keeper node — WAL + snapshots + cold store |
| `mnemelabs/cli:0.1.0` | CLI tool — standalone management client |
