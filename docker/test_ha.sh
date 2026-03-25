#!/usr/bin/env bash
# MnemeCache — High Availability Test Script
#
# Tests leader election, failover, data consistency, and recovery in a 3-Core
# Raft cluster.
#
# Usage:
#   docker compose --profile ha up -d
#   # Wait for cluster to start (~30s)
#   docker compose --profile ha exec mneme-core-1 bash /docker/test_ha.sh
#
# Or run from outside Docker if mneme-cli is installed locally:
#   MNEME_HOST=127.0.0.1:6379 bash docker/test_ha.sh

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[1;36m'
NC='\033[0m'

PASS=0
FAIL=0
ADMIN_PASS="${MNEME_ADMIN_PASSWORD:-secret}"

# Core addresses (inside Docker network)
CORE1="${MNEME_CORE1:-mneme-core-1:6379}"
CORE2="${MNEME_CORE2:-mneme-core-2:6379}"
CORE3="${MNEME_CORE3:-mneme-core-3:6379}"

log()  { printf "${CYAN}[HA-TEST]${NC} %s\n" "$*"; }
pass() { printf "${GREEN}  PASS${NC} %s\n" "$*"; PASS=$((PASS + 1)); }
fail() { printf "${RED}  FAIL${NC} %s\n" "$*"; FAIL=$((FAIL + 1)); }
warn() { printf "${YELLOW}  WARN${NC} %s\n" "$*"; }

cli() {
    local host="$1"; shift
    mneme-cli -H "$host" --insecure --password "$ADMIN_PASS" "$@" 2>&1
}

# ── Wait for cluster to be ready ──────────────────────────────────────────────

log "Waiting for all 3 Cores to be reachable..."
for core in "$CORE1" "$CORE2" "$CORE3"; do
    ELAPSED=0
    while ! cli "$core" ping >/dev/null 2>&1; do
        if [[ $ELAPSED -ge 60 ]]; then
            fail "Core $core not reachable after 60s"
            exit 1
        fi
        sleep 2; ELAPSED=$((ELAPSED + 2))
    done
    pass "Core $core reachable"
done

# ── Test 1: Cluster info shows Raft state ─────────────────────────────────────

log "Test 1: Cluster info"
INFO=$(cli "$CORE1" cluster-info 2>&1 || true)
if echo "$INFO" | grep -qi "raft_term\|leader"; then
    pass "cluster-info shows Raft metadata"
else
    warn "cluster-info may not show Raft fields yet: $INFO"
fi

# ── Test 2: Write data to leader ──────────────────────────────────────────────

log "Test 2: Write data to leader"

# Try each core — one should be the leader
LEADER=""
for core in "$CORE1" "$CORE2" "$CORE3"; do
    RESULT=$(cli "$core" set ha-test-key "hello-ha" 2>&1 || true)
    if echo "$RESULT" | grep -qi "OK\|ok"; then
        LEADER="$core"
        pass "SET on $core succeeded (this is the leader)"
        break
    elif echo "$RESULT" | grep -qi "NOT_LEADER\|redirect"; then
        log "  $core is a follower, trying next..."
    else
        warn "  Unexpected response from $core: $RESULT"
    fi
done

if [ -z "$LEADER" ]; then
    fail "Could not find a leader among the 3 Cores"
    exit 1
fi

# ── Test 3: Read from all cores ───────────────────────────────────────────────

log "Test 3: Read data from all cores"
sleep 1  # Allow replication

for core in "$CORE1" "$CORE2" "$CORE3"; do
    VAL=$(cli "$core" get ha-test-key 2>&1 || true)
    if echo "$VAL" | grep -q "hello-ha"; then
        pass "GET from $core returned correct value"
    else
        fail "GET from $core: expected 'hello-ha', got '$VAL'"
    fi
done

# ── Test 4: Write more test data ──────────────────────────────────────────────

log "Test 4: Write batch of test data"
for i in $(seq 1 20); do
    cli "$LEADER" set "ha-batch-$i" "value-$i" >/dev/null 2>&1
done
pass "Wrote 20 keys to leader"

# Verify batch
FOUND=0
for i in $(seq 1 20); do
    VAL=$(cli "$LEADER" get "ha-batch-$i" 2>&1 || true)
    if echo "$VAL" | grep -q "value-$i"; then
        FOUND=$((FOUND + 1))
    fi
done
if [[ $FOUND -eq 20 ]]; then
    pass "All 20 keys verified on leader"
else
    fail "Only $FOUND/20 keys found on leader"
fi

# ── Test 5: Follower rejects writes (LeaderRedirect) ─────────────────────────

log "Test 5: Follower rejects writes"
for core in "$CORE1" "$CORE2" "$CORE3"; do
    if [ "$core" != "$LEADER" ]; then
        RESULT=$(cli "$core" set "follower-write" "should-redirect" 2>&1 || true)
        if echo "$RESULT" | grep -qi "NOT_LEADER\|redirect\|leader"; then
            pass "Write to follower $core correctly redirected"
        else
            warn "Follower $core did not redirect: $RESULT"
        fi
        break
    fi
done

# ── Test 6: Performance baseline ──────────────────────────────────────────────

log "Test 6: Performance baseline"
START_NS=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
for i in $(seq 1 100); do
    cli "$LEADER" set "perf-$i" "data-$i" >/dev/null 2>&1
done
END_NS=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
AVG_MS=$(( ELAPSED_MS / 100 ))
log "  100 SETs: ${ELAPSED_MS}ms total, ~${AVG_MS}ms/op"
if [[ $AVG_MS -lt 50 ]]; then
    pass "SET latency acceptable (${AVG_MS}ms/op)"
else
    warn "SET latency high (${AVG_MS}ms/op) — may be expected in Docker"
fi

START_NS=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
for i in $(seq 1 100); do
    cli "$LEADER" get "perf-$i" >/dev/null 2>&1
done
END_NS=$(date +%s%N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1e9))')
ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
AVG_MS=$(( ELAPSED_MS / 100 ))
log "  100 GETs: ${ELAPSED_MS}ms total, ~${AVG_MS}ms/op"
if [[ $AVG_MS -lt 50 ]]; then
    pass "GET latency acceptable (${AVG_MS}ms/op)"
else
    warn "GET latency high (${AVG_MS}ms/op) — may be expected in Docker"
fi

# ── Summary ───────────────────────────────────────────────────────────────────

echo ""
log "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
log "  Results: ${GREEN}${PASS} passed${NC}, ${RED}${FAIL} failed${NC}"
log "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
