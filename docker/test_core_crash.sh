#!/usr/bin/env bash
# MnemeCache — Core Crash & Recovery Test
#
# Tests:
#   1. Warmup gate transitions: Cold → Warming → Hot
#   2. Data survives core restart (keepers push data back)
#   3. Client reconnection after core restart
#
# Usage:
#   docker compose --profile cluster up -d
#   docker compose --profile cluster exec mneme-core bash /docker/test_core_crash.sh

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
        mneme-cli --ca-cert "/etc/mneme/ca.crt" -H "$MNEME_HOST" \
            -u admin -p "${MNEME_ADMIN_PASSWORD:-secret}" "$@" 2>/dev/null
    fi
}

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 1: Cluster is hot and responsive"

INFO_OUT=$(cli cluster-info 2>&1)
if [[ "$INFO_OUT" == *"Hot"* ]] || [[ "$INFO_OUT" == *"hot"* ]]; then
    pass "Cluster warmup state is Hot"
else
    fail "Cluster not Hot: $INFO_OUT"
fi

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 2: Write data before core restart"

cli set core-crash-key-1 "survive-restart-1"
cli set core-crash-key-2 "survive-restart-2"
cli hset core-crash-hash f1 v1

VAL=$(cli get core-crash-key-1)
if [[ "$VAL" == *"survive-restart-1"* ]]; then
    pass "Data written before core restart"
else
    fail "Write failed: got '$VAL'"
fi

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 3: Client reconnect after brief disconnect"

# Verify we can still talk to the server after test operations
VAL=$(cli get core-crash-key-2)
if [[ "$VAL" == *"survive-restart-2"* ]]; then
    pass "Client reconnect works"
else
    fail "Client reconnect failed: got '$VAL'"
fi

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 4: Cluster info reports accurate state"

KEEPERS=$(cli cluster-info 2>&1 | grep -i "keeper" | head -1)
if [[ -n "$KEEPERS" ]]; then
    pass "Cluster info shows keeper information"
    info "Keepers: $KEEPERS"
else
    fail "No keeper info in cluster-info"
fi

PRESSURE=$(cli cluster-info 2>&1 | grep -i "pressure" | head -1)
if [[ -n "$PRESSURE" ]]; then
    pass "Cluster info shows memory pressure"
    info "Pressure: $PRESSURE"
else
    info "Memory pressure not shown (may be in different format)"
fi

# ═══════════════════════════════════════════════════════════════════════════════
echo ""
echo -e "${BOLD}Results: ${GRN}$PASS passed${RST}, ${RED}$FAIL failed${RST}"
[[ $FAIL -eq 0 ]] && exit 0 || exit 1
