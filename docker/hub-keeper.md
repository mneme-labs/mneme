# mnemelabs/keeper

**Mneme** — distributed in-memory cache built in Rust. Keeper nodes provide WAL persistence, snapshots, and cold storage for the Core (Mnemosyne) server.

🌐 [mnemelabs.io](https://mnemelabs.io) · 📦 [GitHub](https://github.com/mneme-labs/mneme) · 📖 [Docs](https://github.com/mneme-labs/mneme/tree/main/docs)

---

## What is a Keeper?

A **Keeper** (Hypnos) is the persistence tier of a Mneme cluster:

- Receives every write from Core via mTLS replication
- Maintains a **WAL** (write-ahead log) with O_DIRECT + fallocate
- Takes periodic **snapshots** to bound replay time on restart
- Serves a **cold store** (redb B-tree) for keys evicted from Core RAM
- Participates in QUORUM / ALL consistency acknowledgements

One Keeper is sufficient for durability. Three Keepers give QUORUM quorum of 2-of-3.

---

## Quick start — add a Keeper to a running Core

### Step 1 — start the Core first

```bash
docker run -d \
  --name mneme-core \
  -p 6379:6379 -p 7379:7379 \
  -e MNEME_ADMIN_PASSWORD=secret \
  -e MNEME_CLUSTER_SECRET=change-me \
  -v core-data:/var/lib/mneme \
  mnemelabs/core:1.0.0
```

### Step 2 — attach a Keeper

```bash
docker run -d \
  --name mneme-keeper-1 \
  --link mneme-core \
  -e MNEME_CLUSTER_SECRET=change-me \
  -e MNEME_CORE_ADDR=mneme-core:7379 \
  -e MNEME_KEEPER_ID=keeper-1 \
  -v keeper1-data:/var/lib/mneme \
  mnemelabs/keeper:1.0.0
```

The Keeper connects to Core on port 7379 (mTLS), registers via Herold, and immediately begins receiving replication frames.

---

## Docker Compose — Core + Keepers (production cluster)

```yaml
services:
  mneme-core:
    image: mnemelabs/core:1.0.0
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
    image: mnemelabs/keeper:1.0.0
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
    image: mnemelabs/keeper:1.0.0
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
    image: mnemelabs/keeper:1.0.0
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

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MNEME_CLUSTER_SECRET` | — | Shared secret for join authentication — **required** |
| `MNEME_CORE_ADDR` | — | Core replication address (`host:7379`) — **required** |
| `MNEME_KEEPER_ID` | hostname | Unique node identifier within the cluster |
| `MNEME_LOG_LEVEL` | `info` | Log verbosity: `trace` / `debug` / `info` / `warn` / `error` |

---

## Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| `7379` | mTLS | Replication (Core → Keeper) |
| `9090` | HTTP | Prometheus metrics (`/metrics`) |

Keepers do **not** expose a client port — all client traffic goes to Core.

---

## Persistence internals

| Component | Role |
|-----------|------|
| **Aoide** (WAL) | O_DIRECT + fallocate write-ahead log; survives OS crash |
| **Melete** (snapshots) | Periodic full snapshot; bounds WAL replay on restart |
| **Oneiros** (cold store) | redb B-tree; serves keys evicted from Core RAM (~1.2ms) |

---

## Consistency levels — Keeper participation

| Level | Keeper ACKs required |
|-------|---------------------|
| `EVENTUAL` | 0 — fire-and-forget |
| `ONE` | 1 — first Keeper ACK |
| `QUORUM` | ⌊N/2⌋+1 (default) |
| `ALL` | Every connected Keeper |

---

## License

MIT — © 2024 Mneme Labs
