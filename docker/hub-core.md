# mnemelabs/core

**Mneme** — distributed in-memory cache built in Rust. Sub-millisecond reads, QUORUM writes, TLS 1.3 everywhere.

🌐 [mnemelabs.io](https://mnemelabs.io) · 📦 [GitHub](https://github.com/mneme-labs/mneme) · 📖 [Docs](https://github.com/mneme-labs/mneme/tree/main/docs)

---

## What's in this image

The Core node (**Mnemosyne**) — pure RAM hot pool + CLI:

- `mneme-core` — Core server (client port 6379, replication port 7379, metrics 9090)
- `mneme-cli` — Management CLI (included for exec access)
- Auto-generates TLS certificates on first start (CA + node cert via rcgen)
- CA cert symlinked to `/etc/mneme/ca.crt` for zero-config CLI access

---

## Quick start

### Solo node (dev / single-server)

```bash
docker run -d \
  --name mneme \
  -p 6379:6379 \
  -e MNEME_ADMIN_PASSWORD=secret \
  -v mneme-data:/var/lib/mneme \
  mnemelabs/core:0.1.0
```

Connect:

```bash
docker exec mneme mneme-cli -u admin -p secret ping
# PONG
docker exec mneme mneme-cli -u admin -p secret set hello world
docker exec mneme mneme-cli -u admin -p secret get hello
# world
```

---

## Docker Compose

### Solo (development)

```yaml
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

### Core + Keepers (production cluster)

```yaml
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
      - core-certs:/var/lib/mneme
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

volumes:
  core-certs:
  keeper1-data:
  keeper2-data:
```

---

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MNEME_ADMIN_PASSWORD` | `secret` | Admin user password — **change in production** |
| `MNEME_CLUSTER_SECRET` | — | Shared secret for Keeper join auth (cluster mode) |
| `MNEME_POOL_BYTES` | `512mb` | Hot RAM pool size (`512mb`, `2gb`, `8gb`, …) |
| `MNEME_LOG_LEVEL` | `info` | Log verbosity: `trace` / `debug` / `info` / `warn` / `error` |
| `MNEME_NODE_ID` | hostname | Raft / cluster node identifier |

---

## Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| `6379` | TLS 1.3 | Client connections |
| `7379` | mTLS | Keeper + Raft replication |
| `9090` | HTTP | Prometheus metrics (`/metrics`) |

---

## Topologies

| Mode | Images needed | Use case |
|------|--------------|----------|
| Solo | `core` only | Dev, CI, latency benchmarks |
| Cluster | `core` + `keeper` × N | Single-server production, full durability |
| HA | `core` × 3 + `keeper` × N | Multi-node, automatic failover (Raft) |
| Read replicas | any + `core` (replica mode) | Read scale-out, analytics |

See the [full docker-compose.yml](https://github.com/mneme-labs/mneme/blob/main/docker-compose.yml) for ready-to-use profiles.

---

## Consistency levels

| Level | Behaviour | Latency |
|-------|-----------|---------|
| `EVENTUAL` | Fire-and-forget write, any replica for reads | ~150 µs |
| `ONE` | First Keeper ACK | fastest durable |
| `QUORUM` | ⌊N/2⌋+1 Keeper ACKs (default) | ~800 µs |
| `ALL` | Every Keeper ACK | maximum durability |

---

## License

MIT — © 2024 Mneme Labs
