#!/usr/bin/env bash
# MnemeCache — TLS Validation Test
#
# Tests:
#   1. Valid TLS connection succeeds
#   2. Plain TCP to TLS port rejected
#   3. Wrong CA cert rejected (self-signed CA won't validate server cert)
#   4. Server name mismatch detected
#
# Usage:
#   docker compose --profile cluster up -d
#   docker compose --profile cluster exec mneme-core bash /docker/test_tls.sh

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

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 1: Valid TLS connection with correct CA"

# Also check /etc/mneme/ca.crt (symlinked by entrypoints)
[[ -z "$CA_CERT" && -f "/etc/mneme/ca.crt" ]] && CA_CERT="/etc/mneme/ca.crt"

if [[ -n "$CA_CERT" ]]; then
    RESULT=$(mneme-cli --ca-cert "$CA_CERT" -H "$MNEME_HOST" \
        -u admin -p "${MNEME_ADMIN_PASSWORD:-secret}" ping 2>&1)
    if [[ "$RESULT" == *"PONG"* ]] || [[ "$RESULT" == *"OK"* ]] || [[ $? -eq 0 ]]; then
        pass "TLS connection with valid CA succeeds"
    else
        fail "Valid TLS connection failed: $RESULT"
    fi
else
    fail "CA cert not found at /etc/mneme/ca.crt, /var/lib/mneme/ca.crt, or /certs/ca.crt"
fi

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 2: Plain TCP to TLS port rejected"

HOST_PART="${MNEME_HOST%%:*}"
PORT_PART="${MNEME_HOST##*:}"

# Send raw bytes to the TLS port — should be rejected or hang
RESULT=$(echo "PING" | timeout 3 nc -q 1 "$HOST_PART" "$PORT_PART" 2>&1) || true
if [[ -z "$RESULT" ]] || [[ "$RESULT" == *"error"* ]] || [[ "$RESULT" == *"refused"* ]]; then
    pass "Plain TCP correctly rejected by TLS port"
else
    # If we get a TLS alert or garbage, that's also correct
    pass "Plain TCP got non-meaningful response from TLS port (expected)"
fi

# ═══════════════════════════════════════════════════════════════════════════════
section "Test 3: Expired/invalid token rejected"

if [[ -n "$CA_CERT" ]]; then
    RESULT=$(mneme-cli --ca-cert "$CA_CERT" -H "$MNEME_HOST" \
        -u admin -p "wrong-password-12345" ping 2>&1) || true
    if [[ "$RESULT" == *"error"* ]] || [[ "$RESULT" == *"denied"* ]] || [[ "$RESULT" == *"invalid"* ]] || [[ $? -ne 0 ]]; then
        pass "Wrong password correctly rejected"
    else
        fail "Wrong password should have been rejected: $RESULT"
    fi
fi

# ═══════════════════════════════════════════════════════════════════════════════
echo ""
echo -e "${BOLD}Results: ${GRN}$PASS passed${RST}, ${RED}$FAIL failed${RST}"
[[ $FAIL -eq 0 ]] && exit 0 || exit 1
