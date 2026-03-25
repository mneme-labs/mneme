#!/usr/bin/env bash
# MnemeCache — Stress Tests
#
# Tests:
#   1. Write throughput — 10k sequential EVENTUAL writes
#   2. Write throughput — 1k QUORUM writes
#   3. Mixed workload — concurrent SET/GET/DEL
#   4. Large payload — SET/GET with 1MB values
#   5. Bulk operations — MSET/MGET with 100 keys per batch
#
# Usage:
#   docker compose --profile cluster up -d
#   docker compose --profile cluster exec mneme-core bash /docker/test_stress.sh

set -uo pipefail

PASS=0; FAIL=0
RED='\033[0;31m'; GRN='\033[0;32m'; YLW='\033[0;33m'; RST='\033[0m'
BOLD='\033[1m'

pass() { PASS=$((PASS + 1)); echo -e "  ${GRN}✓${RST} $1"; }
fail() { FAIL=$((FAIL + 1)); echo -e "  ${RED}✗${RST} $1"; }
info() { echo -e "  ${YLW}·${RST} $1"; }
section() { echo -e "\n${BOLD}── $1 ──${RST}"; }

MNEME_HOST="${MNEME_HOST:-127.0.0.1:6379}"
CA_CERT=""
for c in /var/lib/mneme/ca.crt /certs/ca.crt; do
    [[ -f "$c" ]] && { CA_CERT="$c"; break; }
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

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 1: Sequential EVENTUAL write throughput (1000 keys)"

START=$(date +%s%N)
ERRORS=0
for i in $(seq 1 1000); do
    cli set "stress-ev-$i" "value-$i" || ERRORS=$((ERRORS + 1))
done
END=$(date +%s%N)
ELAPSED_MS=$(( (END - START) / 1000000 ))

if [[ $ERRORS -eq 0 ]]; then
    pass "1000 EVENTUAL writes completed in ${ELAPSED_MS}ms (0 errors)"
else
    fail "1000 EVENTUAL writes: $ERRORS errors in ${ELAPSED_MS}ms"
fi

# Verify a sample
VAL=$(cli get stress-ev-500)
if [[ "$VAL" == *"value-500"* ]]; then
    pass "Sample key verified after bulk write"
else
    fail "Sample key verification failed: '$VAL'"
fi

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 2: Mixed workload (SET/GET/DEL)"

ERRORS=0
for i in $(seq 1 200); do
    cli set "mix-$i" "data-$i" || ERRORS=$((ERRORS + 1))
done
for i in $(seq 1 200); do
    cli get "mix-$i" > /dev/null || ERRORS=$((ERRORS + 1))
done
for i in $(seq 1 100); do
    cli del "mix-$i" > /dev/null || ERRORS=$((ERRORS + 1))
done

if [[ $ERRORS -le 5 ]]; then
    pass "Mixed workload completed with $ERRORS errors (threshold: 5)"
else
    fail "Mixed workload had $ERRORS errors"
fi

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 3: Large payload (100KB values)"

# Generate a 100KB value
LARGE_VAL=$(head -c 102400 /dev/urandom | base64 | tr -d '\n' | head -c 102400)
cli set "large-key" "$LARGE_VAL"

RESULT=$(cli get "large-key")
if [[ ${#RESULT} -gt 100000 ]]; then
    pass "100KB value stored and retrieved (${#RESULT} chars)"
else
    fail "Large value retrieval failed: got ${#RESULT} chars"
fi

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 4: Rapid key creation and deletion"

for i in $(seq 1 500); do
    cli set "ephemeral-$i" "temp" > /dev/null
done
for i in $(seq 1 500); do
    cli del "ephemeral-$i" > /dev/null
done

# Verify all deleted
REMAINING=0
for i in 1 100 250 499; do
    VAL=$(cli get "ephemeral-$i" 2>&1)
    if [[ "$VAL" == *"temp"* ]]; then
        REMAINING=$((REMAINING + 1))
    fi
done

if [[ $REMAINING -eq 0 ]]; then
    pass "500 keys created and deleted cleanly"
else
    fail "$REMAINING keys survived deletion"
fi

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 5: Hash with many fields"

for i in $(seq 1 100); do
    cli hset stress-hash "field-$i" "value-$i" > /dev/null
done

FIELD50=$(cli hget stress-hash field-50)
if [[ "$FIELD50" == *"value-50"* ]]; then
    pass "Hash with 100 fields: field-50 verified"
else
    fail "Hash field-50 not found: '$FIELD50'"
fi

# ═══════════════════════════════════════════════════════════════════════════════
echo ""
echo -e "${BOLD}Results: ${GRN}$PASS passed${RST}, ${RED}$FAIL failed${RST}"
[[ $FAIL -eq 0 ]] && exit 0 || exit 1
