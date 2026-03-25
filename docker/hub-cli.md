# mnemelabs/cli

**Mneme** — distributed in-memory cache built in Rust. The CLI image provides `mneme-cli` for managing any Mneme node or cluster.

🌐 [mnemelabs.io](https://mnemelabs.io) · 📦 [GitHub](https://github.com/mneme-labs/mneme) · 📖 [Docs](https://github.com/mneme-labs/mneme/tree/main/docs)

---

## What's in this image

- `mneme-cli` — management CLI for all Mneme operations
- Communicates over TLS 1.3 — no plaintext fallback
- CA cert auto-discovery when connecting to a co-located Core container

---

## Quick start

```bash
# Connect to a running Core container
docker run --rm \
  --link mneme-core \
  -e MNEME_HOST=mneme-core:6379 \
  mnemelabs/cli:1.0.0 \
  mneme-cli -u admin -p secret ping
# PONG
```

Or exec into the Core container (CLI is bundled there too):

```bash
docker exec mneme-core mneme-cli -u admin -p secret ping
```

---

## Common commands

```bash
# Ping
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

# Multiple databases (0-15)
mneme-cli -u admin -p secret --db 1 set isolated-key value

# Cluster info
mneme-cli -u admin -p secret cluster-info
mneme-cli -u admin -p secret stats
```

---

## Consistency levels

```bash
# EVENTUAL — fastest, may read stale data
mneme-cli -u admin -p secret --consistency EVENTUAL get key

# ONE — first Keeper ACK
mneme-cli -u admin -p secret --consistency ONE set key value

# QUORUM — floor(N/2)+1 Keeper ACKs (default for writes)
mneme-cli -u admin -p secret --consistency QUORUM set key value

# ALL — every Keeper ACK
mneme-cli -u admin -p secret --consistency ALL set key value
```

---

## Connection profiles

Save connection details to avoid repeating `--host`, `--ca-cert`, and `--username` every time:

```bash
# Save a profile
mneme-cli profile-set prod \
  --host prod.example.com:6379 \
  --ca-cert /etc/mneme/ca.crt \
  --username admin

# Use the profile
mneme-cli --profile prod cluster-info
mneme-cli --profile prod stats
```

Profiles are stored in `~/.mneme/profiles.toml`.

---

## TLS

`mneme-cli` requires TLS — there is no plaintext mode. When connecting to a Mneme node that auto-generated its certificate (the default), pass the CA cert:

```bash
mneme-cli --ca-cert /path/to/ca.crt -u admin -p secret ping
```

When running CLI **inside** a Core container, the CA cert is symlinked to `/etc/mneme/ca.crt` automatically — no `--ca-cert` needed.

---

## User management (admin only)

```bash
# Create user
mneme-cli -u admin -p secret user-create --username alice --password pass --role readwrite

# List users
mneme-cli -u admin -p secret user-list

# Grant database access
mneme-cli -u admin -p secret user-grant --username alice --db 2

# Revoke database access
mneme-cli -u admin -p secret user-revoke --username alice --db 2

# Delete user
mneme-cli -u admin -p secret user-delete --username alice
```

---

## Roles

| Role | Permissions |
|------|-------------|
| `admin` | All commands, user management, cluster operations |
| `readwrite` | Read + write on allowed databases |
| `readonly` | Read-only on allowed databases |

---

## License

MIT — © 2024 Mneme Labs
