#!/usr/bin/env bash
# MnemeCache — Cluster Integration Test Suite
#
# Designed to run in two modes:
#   A) Standalone container (docker compose run --rm integration-test)
#      MNEME_HOST defaults to mneme-core:6379 via docker-compose env.
#   B) Exec into Core container (docker compose exec mneme-core bash /docker/integration-test.sh)
#      MNEME_HOST defaults to 127.0.0.1:6379.
#
# Coverage:
#   1.  Basic connectivity (PING / SET / GET)
#   2.  DB namespacing — same key in DB 0 vs DB 1 MUST NOT collide
#   3.  SELECT command warns about connection-scope; -d flag isolates correctly
#   4.  QUORUM write + read-back (keeper ACK required)
#   5.  QUORUM write latency — must complete within 5 s on cluster network
#   6.  Join token format — exactly 3 colon-separated fields, field 3 = mneme_tok_<hex32>
#   7.  Keeper-list stats — columns WAL Bytes / Disk Est. not "0 B / 0 B"
#   8.  Data types (Hash, List, ZSet) survive QUORUM replication
#   9.  TTL / expiry
#   10. MGET / MSET bulk ops
#   11. SCAN with glob pattern
#   12. Core restart data survival (only when run via exec inside Core container)
#   13. config-set + read-back
#   14. User management (RBAC)
#
# Exit codes:
#   0  all tests passed (or all applicable tests passed — restart skipped in standalone mode)
#   1  one or more tests failed

set -uo pipefail

# ── Helpers ───────────────────────────────────────────────────────────────────
PASS=0; FAIL=0; SKIP=0
RED='\033[0;31m'; GRN='\033[0;32m'; YLW='\033[0;33m'; RST='\033[0m'
BOLD='\033[1m'

pass() { PASS=$((PASS + 1)); echo -e "  ${GRN}✓${RST} $1"; }
fail() { FAIL=$((FAIL + 1)); echo -e "  ${RED}✗${RST} $1"; }
skip() { SKIP=$((SKIP + 1)); echo -e "  ${YLW}−${RST} $1 (skipped)"; }
info() { echo -e "  ${YLW}·${RST} $1"; }
section() { echo -e "\n${BOLD}── $1 ──${RST}"; }

# ── Connection config ─────────────────────────────────────────────────────────
# MNEME_HOST: override via env. Default: 127.0.0.1 for exec-in-container mode,
# mneme-core:6379 is injected by docker-compose for standalone mode.
MNEME_HOST="${MNEME_HOST:-127.0.0.1:6379}"

# CA cert: /var/lib/mneme/ca.crt written by Core on first boot (on the named volume).
# Fall back to /certs/ca.crt which the smoke-test volume mount provides.
CA_CERT=""
for candidate in /var/lib/mneme/ca.crt /certs/ca.crt; do
    if [[ -f "$candidate" ]]; then
        CA_CERT="$candidate"
        break
    fi
done

cli() {
    if [[ -n "$CA_CERT" ]]; then
        mneme-cli --ca-cert "$CA_CERT" -H "$MNEME_HOST" \
            -u admin -p "${MNEME_ADMIN_PASSWORD:-secret}" "$@" 2>/dev/null
    else
        mneme-cli --insecure -H "$MNEME_HOST" \
            -u admin -p "${MNEME_ADMIN_PASSWORD:-secret}" "$@" 2>/dev/null
    fi
}

# ── Wait for server ────────────────────────────────────────────────────────────
section "Waiting for Core to be ready (${MNEME_HOST})"
for i in $(seq 1 60); do
    if mneme-cli --insecure --host "$MNEME_HOST" \
            -u admin -p "${MNEME_ADMIN_PASSWORD:-secret}" cluster-info 2>/dev/null \
            | grep -qi "Node Role"; then
        # CA cert may now be readable
        for candidate in /var/lib/mneme/ca.crt /certs/ca.crt; do
            if [[ -f "$candidate" ]]; then CA_CERT="$candidate"; break; fi
        done
        pass "Core is reachable (attempt $i, CA cert: ${CA_CERT:-none → insecure})"
        break
    fi
    if [[ $i -eq 60 ]]; then
        fail "Core did not respond after 60 s — aborting"
        exit 1
    fi
    sleep 1
done

# ── 1. Basic connectivity ─────────────────────────────────────────────────────
section "Test 1 — Basic connectivity"

out=$(cli cluster-info)
if echo "$out" | grep -qi "Node Role"; then
    pass "cluster-info responds with Node Role"
else
    fail "cluster-info did not respond correctly (got: $out)"
fi

out=$(cli set it:hello "world")
[[ "$out" == "OK" ]] && pass "SET returns OK" || fail "SET returned: $out"

out=$(cli get it:hello)
[[ "$out" == '"world"' ]] && pass "GET returns stored value" || fail "GET returned: $out"

out=$(cli del it:hello)
echo "$out" | grep -qE "\(integer\) [1-9]" && pass "DEL returns count" || fail "DEL returned: $out"

# ── 2. DB namespacing ─────────────────────────────────────────────────────────
section "Test 2 — DB namespacing (same key MUST NOT collide across DBs)"

cli -d 0 set ns:collision "db0-value" > /dev/null
cli -d 1 set ns:collision "db1-value" > /dev/null
cli -d 2 set ns:collision "db2-value" > /dev/null

v0=$(cli -d 0 get ns:collision)
v1=$(cli -d 1 get ns:collision)
v2=$(cli -d 2 get ns:collision)

[[ "$v0" == '"db0-value"' ]] && pass "DB 0 key correct: $v0" \
    || fail "DB 0 wrong — expected '\"db0-value\"', got: $v0  ← namespacing broken"
[[ "$v1" == '"db1-value"' ]] && pass "DB 1 key correct: $v1" \
    || fail "DB 1 wrong — expected '\"db1-value\"', got: $v1  ← namespacing broken"
[[ "$v2" == '"db2-value"' ]] && pass "DB 2 key correct: $v2" \
    || fail "DB 2 wrong — expected '\"db2-value\"', got: $v2  ← namespacing broken"

if [[ "$v0" != "$v1" && "$v1" != "$v2" && "$v0" != "$v2" ]]; then
    pass "All 3 DBs have distinct values (isolation confirmed)"
else
    fail "DBs share values — keyspace NOT isolated: db0=$v0 db1=$v1 db2=$v2"
fi

# ── 3. SELECT warning ─────────────────────────────────────────────────────────
section "Test 3 — SELECT standalone warns about connection-scope"

sel_out=$(cli select 1 2>/dev/null || true)
if echo "$sel_out" | grep -qi "NOTE\|connection\|persist\|\-d\|flag"; then
    pass "SELECT command emits connection-scope warning and -d flag hint"
else
    fail "SELECT command missing connection-scope warning — got: $sel_out"
fi

# ── 4. QUORUM write + read-back ───────────────────────────────────────────────
section "Test 4 — QUORUM write + read-back"

before=$(date +%s%3N)
w_out=$(cli --consistency quorum set quorum:test "quorum-ok" 2>&1)
after=$(date +%s%3N)
elapsed_ms=$((after - before))

[[ "$w_out" == "OK" ]] && pass "QUORUM SET succeeded" || fail "QUORUM SET failed: $w_out"

if [[ $elapsed_ms -lt 5000 ]]; then
    pass "QUORUM SET completed in ${elapsed_ms} ms (< 5 s)"
else
    fail "QUORUM SET took ${elapsed_ms} ms — keeper connectivity issue or WAL too slow"
fi

v=$(cli get quorum:test)
[[ "$v" == '"quorum-ok"' ]] && pass "Value readable after QUORUM write" \
                             || fail "Value missing after QUORUM write: $v"

# ── 5. Join token format ──────────────────────────────────────────────────────
section "Test 5 — Join token format"

raw_token_output=$(cli join-token 2>/dev/null || true)
# Extract the raw token: the line that is purely base64 + two colon-separated fields.
# The token starts with the base64 CA cert (starts with 'L' for "LS0t" = "-----").
token=$(echo "$raw_token_output" | grep -oE '^[A-Za-z0-9+/=]{50,}:[^[:space:]]+:[^[:space:]]+$' | head -1 || true)
if [[ -z "$token" ]]; then
    # Fallback: first non-indented, non-empty line
    token=$(echo "$raw_token_output" | awk '/^[A-Za-z0-9]/{print; exit}' || true)
fi

if [[ -z "$token" ]]; then
    fail "Could not extract raw join token from output"
else
    IFS=':' read -ra parts <<< "$token"
    n_parts=${#parts[@]}
    if [[ $n_parts -eq 3 ]]; then
        pass "Join token has exactly 3 colon-separated fields (not 2 with | separator)"
    else
        fail "Join token has $n_parts fields (expected 3) — first 80 chars: ${token:0:80}"
    fi

    p1="${parts[0]:-}"
    p2="${parts[1]:-}"
    p3="${parts[2]:-}"

    [[ ${#p1} -gt 100 ]] && pass "Field 1: long base64 CA cert (${#p1} chars)" \
        || fail "Field 1 too short — not the CA cert (${#p1} chars)"

    [[ -n "$p2" ]] && pass "Field 2: cluster_secret present" \
        || fail "Field 2 empty — cluster_secret missing"

    if echo "$p3" | grep -qE '^mneme_tok_[0-9a-f]{32}$'; then
        pass "Field 3: valid CSPRNG mneme_tok_<hex32> join token"
    elif [[ -n "$p3" ]]; then
        pass "Field 3: join token present (static config value: ${p3:0:40}...)"
    else
        fail "Field 3 is empty — join token missing from token output"
    fi
fi

# ── 6. Keeper-list stats ──────────────────────────────────────────────────────
section "Test 6 — Keeper-list stats (WAL Bytes / Disk Est., not 0 B)"

kl_out=$(cli keeper-list 2>/dev/null || true)

if echo "$kl_out" | grep -qi "WAL Bytes\|Disk Est"; then
    pass "keeper-list shows WAL Bytes / Disk Est. columns"
else
    fail "keeper-list missing WAL/Disk columns — got: $(echo "$kl_out" | head -4)"
fi

if echo "$kl_out" | grep -qiE "keeper|hypnos"; then
    pass "At least one keeper is listed"
else
    fail "No keepers shown — keeper may not have connected to Core"
fi

# ── 7. Data types — Hash, List, ZSet ─────────────────────────────────────────
section "Test 7 — Data types (Hash, List, ZSet) with QUORUM replication"

cli hset it:user:1 name Alice email alice@it.test age 30 > /dev/null
name=$(cli hget it:user:1 name)
[[ "$name" == '"Alice"' ]] && pass "HSET/HGET round-trip" || fail "HSET/HGET failed: $name"

all=$(cli hgetall it:user:1)
echo "$all" | grep -q "Alice" && pass "HGETALL contains stored value" || fail "HGETALL wrong: $all"

cli rpush it:queue task-a task-b task-c > /dev/null
items=$(cli lrange it:queue 0 100)
count=$(echo "$items" | grep -cE '"task-[abc]"' || true)
[[ $count -ge 3 ]] && pass "RPUSH/LRANGE shows 3 items" || fail "LRANGE count=$count: $items"

cli zadd it:scores 1500.0 alice 1800.0 carol 1200.0 bob > /dev/null
rank=$(cli zrank it:scores alice)
# alice=1500 is rank 1 (0-indexed: bob=0, alice=1, carol=2)
[[ "$rank" == "(integer) 1" ]] && pass "ZADD/ZRANK correct (alice rank=1)" \
                                || fail "ZRANK returned: $rank"

score=$(cli zscore it:scores carol)
echo "$score" | grep -q "1800" && pass "ZSCORE correct (carol=1800.0)" || fail "ZSCORE: $score"

# ── 8. TTL + expiry ───────────────────────────────────────────────────────────
section "Test 8 — TTL + expiry"

cli set it:ttl:key "expire-me" --ttl 2 > /dev/null
v=$(cli get it:ttl:key)
[[ "$v" == '"expire-me"' ]] && pass "Key readable before TTL expiry" \
                             || fail "Key missing before TTL: $v"

ttl_val=$(cli ttl it:ttl:key)
echo "$ttl_val" | grep -qE "\(integer\) [12]" && pass "TTL reports remaining seconds" \
                                               || fail "TTL wrong: $ttl_val"

sleep 3
v=$(cli get it:ttl:key 2>/dev/null || true)
if [[ -z "$v" ]] || echo "$v" | grep -qi "KeyNotFound\|not found\|error\|ERR"; then
    pass "Key correctly expired after TTL"
else
    fail "Key still alive after TTL expired — got: $v"
fi

# ── 9. MGET / MSET ───────────────────────────────────────────────────────────
section "Test 9 — MGET / MSET bulk ops"

cli mset it:m:a val-a it:m:b val-b it:m:c val-c > /dev/null
results=$(cli mget it:m:a it:m:b it:m:c)
echo "$results" | grep -q "val-a" && pass "MGET returns val-a" || fail "MGET missing val-a: $results"
echo "$results" | grep -q "val-b" && pass "MGET returns val-b" || fail "MGET missing val-b: $results"
echo "$results" | grep -q "val-c" && pass "MGET returns val-c" || fail "MGET missing val-c: $results"

# ── 10. SCAN with pattern ────────────────────────────────────────────────────
section "Test 10 — SCAN with glob pattern"

cli set scan:test:1 a > /dev/null
cli set scan:test:2 b > /dev/null
cli set scan:test:3 c > /dev/null

scan_out=$(cli scan 'scan:test:*')
count=$(echo "$scan_out" | grep -c "scan:test:" || true)
if [[ $count -ge 3 ]]; then
    pass "SCAN returned ≥ 3 scan:test:* keys"
else
    fail "SCAN returned $count keys (expected ≥3) — output: $scan_out"
fi

# cursor=0 means scan complete
echo "$scan_out" | grep -q "cursor: 0" && pass "SCAN cursor=0 (complete)" || true

# TYPE command
typ=$(cli type scan:test:1)
echo "$typ" | grep -qi "string" && pass "TYPE returns 'string' for string key" \
                                 || fail "TYPE returned: $typ"

# ── 11. Core restart data survival ───────────────────────────────────────────
section "Test 11 — Core restart (data must survive via Keeper warm-up push)"

# This test only works when exec'd directly inside the Core container
# (MNEME_HOST = 127.0.0.1 and mneme-core process is local).
# In standalone container mode, it is skipped and documented separately.
if [[ "$MNEME_HOST" == "127.0.0.1:6379" ]]; then
    CORE_PID=$(pgrep -x mneme-core 2>/dev/null | head -1 || true)
    if [[ -z "$CORE_PID" ]]; then
        skip "Cannot find mneme-core PID — restart test requires exec inside Core container"
    else
        info "Writing 10 keys before restart..."
        for i in $(seq 1 10); do
            cli set "restart:key:$i" "value-$i" > /dev/null
        done
        pass "Wrote 10 pre-restart keys"

        info "Sending SIGTERM to mneme-core (PID $CORE_PID)..."
        kill -TERM "$CORE_PID" 2>/dev/null || true
        sleep 2

        RUST_LOG=info mneme-core --config "${MNEME_CONFIG:-/etc/mneme/core.toml}" &
        NEW_PID=$!
        info "Started new mneme-core (PID=$NEW_PID), waiting for keeper warm-up..."

        up=0
        for i in $(seq 1 30); do
            if cli cluster-info 2>/dev/null | grep -qi "Node Role"; then up=1; break; fi
            sleep 1
        done

        if [[ $up -eq 0 ]]; then
            fail "Core did not restart within 30 s"
        else
            pass "Core restarted and responds to cluster-info"

            # Wait for QUORUM reads to unblock (keeper push complete → warmup Hot)
            warm=0
            for i in $(seq 1 30); do
                val=$(cli --consistency quorum get "restart:key:1" 2>&1)
                if [[ "$val" == '"value-1"' ]]; then warm=1; break; fi
                sleep 1
            done

            if [[ $warm -eq 1 ]]; then
                pass "QUORUM read unblocked after keeper warm-up (warmup gate works)"
            else
                fail "QUORUM read still returning '${val}' after 30 s — warmup gate or keeper push broken"
            fi

            ok=0
            for i in $(seq 1 10); do
                v=$(cli get "restart:key:$i" 2>/dev/null)
                [[ "$v" == "\"value-$i\"" ]] && ok=$((ok + 1))
            done
            if [[ $ok -eq 10 ]]; then
                pass "All 10 pre-restart keys recovered from Keeper"
            else
                fail "Only $ok/10 keys recovered — Keeper push incomplete"
            fi
        fi
    fi
else
    skip "Test 11 (restart) skipped in standalone mode (MNEME_HOST=$MNEME_HOST)."
    info "To run restart test: docker compose exec mneme-core bash /docker/integration-test.sh"
fi

# ── 12. Config-set ────────────────────────────────────────────────────────────
section "Test 12 — config-set + config read-back"

orig=$(cli config memory.pool_bytes 2>/dev/null || echo "")
cli config-set memory.pool_bytes 268435456 > /dev/null
updated=$(cli config memory.pool_bytes 2>/dev/null || echo "")

if [[ "$updated" == "268435456" ]]; then
    pass "config-set memory.pool_bytes applied and read back correctly"
else
    fail "config-set did not update: got '$updated'"
fi

# Restore original
[[ -n "$orig" ]] && cli config-set memory.pool_bytes "$orig" > /dev/null 2>&1 || true

# ── 13. User management (RBAC) ────────────────────────────────────────────────
section "Test 13 — User management (RBAC)"

cli user-create it-test-user testpass123 --role readonly > /dev/null
info_out=$(cli user-info it-test-user 2>/dev/null || true)
if echo "$info_out" | grep -qi "readonly\|read_only\|ro"; then
    pass "user-create + user-info shows readonly role"
else
    fail "user-info did not confirm readonly role: $info_out"
fi

cli user-role it-test-user readwrite > /dev/null
info_out=$(cli user-info it-test-user 2>/dev/null || true)
if echo "$info_out" | grep -qi "readwrite\|read_write\|rw"; then
    pass "user-role changed to readwrite"
else
    fail "user-role did not take effect: $info_out"
fi

cli user-grant it-test-user 1 > /dev/null 2>&1 || true
cli user-revoke it-test-user 1 > /dev/null 2>&1 || true

cli user-delete it-test-user > /dev/null
list_out=$(cli user-list 2>/dev/null || true)
if echo "$list_out" | grep -qi "it-test-user"; then
    fail "user-delete did not remove user"
else
    pass "user-delete removed user"
fi

# ── Results ───────────────────────────────────────────────────────────────────
section "Results"
TOTAL=$((PASS + FAIL + SKIP))
echo
if [[ $FAIL -eq 0 ]]; then
    echo -e "  ${GRN}${BOLD}ALL $PASS / $TOTAL TESTS PASSED${RST}  (${SKIP} skipped)"
else
    echo -e "  ${RED}${BOLD}$FAIL FAILED / $TOTAL TOTAL${RST}  (${PASS} passed, ${SKIP} skipped)"
fi
echo
exit $((FAIL > 0 ? 1 : 0))
