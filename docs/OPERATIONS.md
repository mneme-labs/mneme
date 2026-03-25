# MnemeCache — Cluster Operations Runbook

This runbook covers day-to-day operations for MnemeCache clusters: starting fresh, scaling, recovering from failures, monitoring, backup, and upgrades. Follow each section in order. Skipping steps, especially around shutdown order, causes data loss.

---

## 1. Starting a Cluster From Scratch

### Step 1 — Initialize the first God node

```bash
mneme-core init \
  --id 1 \
  --bind 0.0.0.0 \
  --config /etc/mneme/core.toml
```

On success the process prints a join token to stdout:

```
[mneme-core] node_id=1 bound=0.0.0.0:6379 repl=0.0.0.0:7379
[mneme-core] join_token=eyJhbGciOiJIUzI1NiJ9...
```

Copy the join token. It is required for every Keeper and read-replica that joins this cluster. The token is HMAC-SHA256 signed with `cluster_secret` from the config file and does not expire by default.

### Step 2 — Join the first Keeper

```bash
mneme-keeper join \
  --core 10.0.0.1:7379 \
  --join-token eyJhbGciOiJIUzI1NiJ9... \
  --grant 250mb \
  --id 1 \
  --config /etc/mneme/keeper.toml
```

Herold generates an mTLS certificate via rcgen, sends a `REGISTER` frame to Themis, and the cluster pool grows. Watch logs for:

```
[herold] cert_generated san=keeper-1.mneme.internal
[herold] registered keeper_id=1 grant_bytes=262144000
[hypnos] warmup_state=Synced
```

### Step 3 — Verify with keeper-list

```bash
mneme-cli keeper-list
```

Expected output:

```
ID   ADDR             GRANT     STATE    CONNECTED
1    10.0.0.2:7379   250 MiB   Synced   yes
```

The cluster is ready once at least one Keeper shows `STATE=Synced`. For QUORUM writes you need `floor(N/2)+1` Keepers. A single-Keeper cluster satisfies QUORUM for N=1 (need 1 ACK). Add a second Keeper before serving production traffic.

---

## 2. Adding a Keeper to a Running Cluster (Zero Downtime)

Adding a Keeper requires no downtime. Herold handles registration and warm-up entirely in the background. The cluster continues serving requests throughout.

### Step 1 — Prepare the Keeper config

```toml
# /etc/mneme/keeper.toml
[keeper]
id         = 2
data_dir   = "/var/lib/mneme"
grant_bytes = 536870912   # 512 MiB

[cluster]
core_addr   = "10.0.0.1:7379"
join_token  = "eyJhbGciOiJIUzI1NiJ9..."

[tls]
# rcgen generates certs automatically on first start
cert_dir = "/etc/mneme/tls"
```

Set `core_addr` to the God node's replication address (port 7379). The `join_token` must match the token printed by `mneme-core init`.

### Step 2 — Start the new Keeper

```bash
systemctl start mneme-keeper@2
# or directly:
mneme-keeper start --config /etc/mneme/keeper.toml
```

Herold immediately begins the registration handshake. Mnemosyne starts migrating slots to the new Keeper according to Iris slot distribution. This migration runs at low priority and does not affect hot-path latency.

### Step 3 — Watch warm-up logs

```bash
journalctl -u mneme-keeper@2 -f
```

Look for the state progression:

```
[hypnos] warmup_state=Warming  received=0  expected=1048576
[hypnos] warmup_state=Warming  received=524288  expected=1048576
[hypnos] warmup_state=Synced   received=1048576  expected=1048576
```

Migration is complete when `warmup_state=Synced`. Duration depends on data volume; at 1 GiB expect roughly 3-10 seconds on a 10 GbE link.

### Step 4 — Verify with keeper-list

```bash
mneme-cli keeper-list
```

```
ID   ADDR             GRANT     STATE    CONNECTED
1    10.0.0.2:7379   250 MiB   Synced   yes
2    10.0.0.3:7379   512 MiB   Synced   yes
```

Both Keepers must show `Synced` before you adjust QUORUM-dependent SLOs.

---

## 3. Draining and Removing a Keeper Gracefully

Never kill a Keeper with SIGKILL unless the node has already failed. Use the drain procedure to avoid data loss.

### Step 1 — Wait for in-flight requests to drain

Check the Keeper's in-flight count via Prometheus or CLI before proceeding:

```bash
mneme-cli pool-stats
```

```
keeper_id   in_flight   lag_ms   state
1           0           0        Synced
2           3           12       Synced
```

Wait until `in_flight=0` on the target Keeper. Under normal load this takes under one second.

### Step 2 — Send graceful shutdown signal

```bash
# systemd managed:
systemctl stop mneme-keeper@2

# or send SIGTERM directly:
kill -TERM $(pidof mneme-keeper)
```

Hypnos shutdown order: stop accepting REPLICATE frames → fsync WAL → final snapshot → close Hermes connection to God → close Oneiros → exit. This takes 1-5 seconds under normal conditions.

### Step 3 — Verify cluster still has quorum

After the Keeper exits, confirm remaining Keepers satisfy QUORUM:

```bash
mneme-cli keeper-list
```

```
ID   ADDR             GRANT     STATE    CONNECTED
1    10.0.0.2:7379   250 MiB   Synced   yes
2    10.0.0.3:7379   512 MiB   Synced   no     ← removed
```

With N-1 Keepers, verify `floor((N-1)/2)+1` still covers your QUORUM requirement. If you drop below quorum, QUORUM writes will block until a new Keeper joins.

### Step 4 — Update logical pool size

The logical pool shrinks by the removed Keeper's grant. Check `pool_bytes_used` vs the new `pool_bytes_max`:

```bash
mneme-cli pool-stats
```

If `pressure_ratio > 0.90`, add another Keeper immediately or reduce the working set before completing the removal.

---

## 4. God Node Crash Recovery

**Operator action required: none.**

Keepers auto-reconnect to God when it restarts. The reconnect loop in Hermes retries with exponential backoff (initial 100ms, max 10s). The full warm-up cycle (Cold → Warming → Hot) completes in under 15 seconds for typical working sets.

Monitor warm-up state via logs:

```bash
journalctl -u mneme-core -f | grep warmup_state
```

```
[mnemosyne] warmup_state=Cold
[mnemosyne] warmup_state=Warming  synced_keepers=0/2
[mnemosyne] warmup_state=Warming  synced_keepers=1/2
[mnemosyne] warmup_state=Hot      synced_keepers=2/2
```

### Prometheus alert for stuck warm-up

```yaml
- alert: GodNodeStuckInWarmup
  expr: mneme_warmup_hot == 0
  for: 30s
  annotations:
    summary: "God node has not reached Hot state for >30s"
    description: "Check Hermes connectivity to Keepers. Keeper logs may show SyncComplete not sent."
```

QUORUM reads are not served until `warmup_state=Hot`. EVENTUAL reads may be served from read replicas during warm-up if read replicas are available.

---

## 5. Keeper Crash Recovery

**QUORUM-written data is safe.** While a Keeper is down, the remaining Keepers continue satisfying QUORUM for `floor(N/2)+1 <= N-1`. Reads and writes proceed normally as long as quorum is met.

When the crashed Keeper restarts, Hypnos automatically:

1. Replays the WAL from the last clean checkpoint.
2. Loads the most recent snapshot from Melete.
3. Requests a delta sync from God via `SyncRequest` frame for any keys written during downtime.
4. Sends `SyncComplete` once fully caught up.

```bash
# Watch recovery on the restarted Keeper:
journalctl -u mneme-keeper@1 -f
```

```
[aoide] wal_replay_start segments=3
[aoide] wal_replay_done  applied=48291
[melete] snapshot_loaded  keys=1048576
[hypnos] sync_request_sent  from_lsn=29182847
[hypnos] warmup_state=Synced
```

No operator action is required beyond restarting the process. No data is lost for QUORUM writes.

---

## 6. All Keepers Crash Simultaneously

This is a partial outage event. QUORUM and ALL writes are lost from the in-flight window (at most `request_timeout=5000ms` of writes). EVENTUAL-only writes in the crash window may be lost.

**QUORUM-acknowledged writes are safe** — they were written to WAL on disk before ACK was sent.

### Recovery procedure

1. Restart all Keepers (order does not matter):

```bash
systemctl start mneme-keeper@1
systemctl start mneme-keeper@2
systemctl start mneme-keeper@3
```

2. Each Keeper replays its WAL and snapshot independently.

3. Each Keeper reconnects to God and sends `SyncComplete`.

4. God transitions to `Hot` once all expected Keepers send `SyncComplete`.

5. Verify:

```bash
mneme-cli keeper-list
mneme-cli pool-stats
```

All Keepers should reach `Synced` within 15 seconds per TB of data stored.

### What to tell users

- Any write that received a QUORUM or ALL acknowledgment before the crash is intact.
- Any write that was in-flight (no ACK received by the client) is lost and must be retried.
- EVENTUAL reads may return stale data during the warm-up window until `warmup_state=Hot`.

---

## 7. Monitoring — Key Prometheus Alerts

All metrics are exposed on port 9090 (`/metrics`, plain HTTP).

| Alert | Expression | Severity | Action |
|---|---|---|---|
| Connection saturation | `mneme_connections_active > 90000` | warning | Add more God nodes or reduce client connection counts |
| OOM risk | `mneme_pool_pressure_ratio > 0.90` | critical | Add a Keeper or increase `pool_bytes` immediately |
| Keeper falling behind | `mneme_replication_lag_ms{keeper} > 500` | warning | Check Keeper disk I/O, WAL flush rate, and network |
| Latency spike | `mneme_request_duration_p99 > 2000` | warning | Check `slowlog`, hot-key distribution via Iris metrics |
| God stuck in warm-up | `mneme_warmup_hot == 0 for > 30s` | critical | Check Hermes connectivity to all Keepers on port 7379 |

### Full recommended alert config

```yaml
groups:
  - name: mneme_cluster
    rules:
      - alert: ConnectionSaturation
        expr: mneme_connections_active > 90000
        for: 1m
        labels:
          severity: warning

      - alert: MemoryPressureCritical
        expr: mneme_pool_pressure_ratio > 0.90
        for: 30s
        labels:
          severity: critical

      - alert: KeeperReplicationLag
        expr: mneme_replication_lag_ms > 500
        for: 2m
        labels:
          severity: warning

      - alert: HighP99Latency
        expr: mneme_request_duration_p99 > 2000
        for: 5m
        labels:
          severity: warning

      - alert: GodNodeStuckInWarmup
        expr: mneme_warmup_hot == 0
        for: 30s
        labels:
          severity: critical
```

### Useful metric queries

```promql
# Request rate by command
rate(mneme_requests_total[1m])

# Eviction breakdown
rate(mneme_evictions_total{reason=~"lfu|ttl|oom"}[5m])

# Keeper sync health
mneme_replication_lag_ms

# Cache hit rate (cold fetches = misses from RAM, served from Oneiros)
rate(mneme_cold_fetches_total[5m]) / rate(mneme_requests_total{cmd="GET"}[5m])
```

---

## 8. Backup

MnemeCache durability lives in two files per Keeper: the snapshot (`mneme.snap`) and the cold store (`cold.redb`). WAL segments are transient and do not need to be backed up separately if a recent snapshot exists.

### Trigger a manual snapshot

Send `SIGUSR1` to the Hypnos process to force an immediate snapshot outside the normal schedule:

```bash
# Manual snapshot trigger (send SIGUSR1 to Hypnos process)
kill -USR1 $(pidof mneme-keeper)
```

Watch logs for confirmation:

```
[melete] snapshot_start  keys=1048576
[melete] snapshot_done   bytes=284367192  duration_ms=1240
```

### Copy backup files

```bash
# Files to copy:
cp /var/lib/mneme/mneme.snap /backup/
cp /var/lib/mneme/cold.redb /backup/
```

Both files can be copied while the Keeper is running. `mneme.snap` is written atomically (rename-on-complete). `cold.redb` is a redb B-tree with its own page-level consistency guarantees.

### Restore from backup

1. Stop the Keeper: `systemctl stop mneme-keeper@N`
2. Replace data files: `cp /backup/mneme.snap /var/lib/mneme/` and `cp /backup/cold.redb /var/lib/mneme/`
3. Start the Keeper: `systemctl start mneme-keeper@N`
4. Hypnos loads the snapshot, replays any WAL segments on top, and reconnects to God.

### Backup schedule recommendation

| Data criticality | Snapshot frequency |
|---|---|
| Development | Manual on demand |
| Staging | Daily via cron + SIGUSR1 |
| Production | Every 15 minutes via cron + SIGUSR1 |

---

## 9. Upgrade Procedure

MnemeCache uses a rolling upgrade strategy. Keepers are upgraded first, one at a time. The God node is upgraded last. The reconnect window during God restart is under 15 seconds; clients with retry logic will see no errors.

### Step 1 — Upgrade Keepers (one at a time)

For each Keeper in turn:

```bash
# 1. Drain in-flight (wait for in_flight=0 in pool-stats)
mneme-cli pool-stats

# 2. Stop the old binary gracefully
systemctl stop mneme-keeper@N

# 3. Install the new binary
cp mneme-keeper-vX.Y.Z /usr/local/bin/mneme-keeper

# 4. Start with the new binary
systemctl start mneme-keeper@N

# 5. Verify reconnect
mneme-cli keeper-list   # wait for STATE=Synced

# 6. Proceed to next Keeper
```

Do not stop the next Keeper until the current one shows `STATE=Synced`. Leaving two Keepers down simultaneously in a three-Keeper cluster breaks QUORUM.

### Step 2 — Upgrade the God node

```bash
# 1. Install the new binary (do not start yet)
cp mneme-core-vX.Y.Z /usr/local/bin/mneme-core

# 2. Graceful restart (systemd handles drain automatically via ExecStop)
systemctl restart mneme-core

# 3. Watch warm-up — should complete in <15s
journalctl -u mneme-core -f | grep warmup_state

# 4. Verify
mneme-cli keeper-list
```

Clients will receive `KeeperUnreachable` or connection-reset errors during the <15s restart window. Configure client retry with backoff (Pontus does this automatically).

---

## 10. Troubleshooting

### Keeper not connecting

**Symptom:** `keeper-list` shows `CONNECTED=no` or Keeper logs show repeated `REGISTER` failures.

**Checks:**
1. TLS certificate SAN must match the `server_name` in the Keeper config. Herold generates certs automatically, but if `cert_dir` already contains stale certs from a different hostname, delete them and restart to force regeneration.
2. `cluster_secret` in `keeper.toml` must match `cluster_secret` in `core.toml`. A mismatch causes the HMAC verification of the `join_token` to fail.
3. Firewall: replication traffic uses port 7379. Confirm: `nc -zv <core_addr> 7379`.

```bash
# Check cert SAN
openssl x509 -in /etc/mneme/tls/keeper.crt -text -noout | grep -A1 "Subject Alternative"

# Test replication port
nc -zv 10.0.0.1 7379
```

### Token expired

**Symptom:** Client receives `TokenExpired` error.

**Fix:** Re-authenticate to obtain a fresh token:

```bash
mneme-cli -u user -p pass auth-token
```

Store the new token in the client config or environment variable. Tokens have a configurable TTL (default 24h). If tokens expire too quickly for your use case, increase `token_ttl_seconds` in `core.toml`.

### OOM errors

**Symptom:** `OutOfMemory` errors returned to clients. `pressure_ratio` metric above 0.90.

**Immediate actions:**
1. Check `pressure_ratio`: `mneme-cli pool-stats`
2. Add a Keeper: `mneme-keeper join --core ... --grant 1gb --id N`
3. Or increase pool size on existing Keepers: `mneme-cli config set memory.pool_bytes 2gb`
4. Check for hot keys causing disproportionate memory use: `mneme-cli slowlog 50`

### Slow queries

**Symptom:** p99 latency above 2ms. Clients reporting timeouts.

```bash
# Show top 50 slowest recent commands
mneme-cli slowlog 50
```

SLOWLOG output includes: timestamp, duration_µs, command, key, consistency level. Common causes:

| Cause | Indicator | Fix |
|---|---|---|
| Cold fetch from Oneiros | `cold_fetches_total` rising | Increase RAM grant, working set too large for hot tier |
| Hot key contention | One key dominates SLOWLOG | Shard the key at the application layer |
| QUORUM with slow Keeper | `replication_lag_ms` high | Check Keeper disk I/O (WAL fsync rate) |
| Connection backpressure | `in_flight` near 200000 | Scale horizontally or reduce client parallelism |
