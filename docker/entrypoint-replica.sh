#!/usr/bin/env bash
# MnemeCache — Read Replica entrypoint (cluster mode)
#
# Waits for Core to publish the CA cert, then starts mneme-core in
# read-replica mode. Accepts EVENTUAL consistency reads only.
#
# Environment variables:
#   CORE_ADDR       Core replication address (default: "mneme-core:7379")
#   MNEME_LOG_LEVEL Log verbosity (default: "info")

set -euo pipefail

CONFIG="${MNEME_CONFIG:-/etc/mneme/mneme.toml}"
DATA_DIR=/var/lib/mneme
LOG_LEVEL="${MNEME_LOG_LEVEL:-info}"
CA_SRC=/certs/ca.crt
CA_DST="${DATA_DIR}/ca.crt"

printf '\n  \033[1;36mMnemeCache\033[0m — Read Replica\n\n'

install -d -m 750 "$DATA_DIR" 2>/dev/null || true

echo "[replica] Waiting for CA cert from Core..."
ELAPSED=0
until [ -f "$CA_SRC" ]; do
    if [[ $ELAPSED -ge 120 ]]; then
        printf '\033[1;31m[replica] ERROR: CA cert not available after 120s\033[0m\n' >&2; exit 1
    fi
    sleep 1; ELAPSED=$((ELAPSED + 1))
done

cp "$CA_SRC" "$CA_DST"
[ -f /certs/node.crt ] && cp /certs/node.crt "${DATA_DIR}/node.crt"
[ -f /certs/node.key ] && cp /certs/node.key "${DATA_DIR}/node.key"
echo "[replica] CA cert + node cert/key installed"

# Symlink CA cert to CLI default path so mneme-cli works without --insecure
mkdir -p /etc/mneme
ln -sf "$CA_DST" /etc/mneme/ca.crt

# Bootstrap admin user so clients can authenticate to the replica
ADMIN_PASS="${MNEME_ADMIN_PASSWORD:-secret}"
echo "[replica] Bootstrapping admin user..."
mneme-core --config "$CONFIG" adduser \
    --username admin \
    --password "$ADMIN_PASS" \
    --role admin 2>/dev/null || true

echo "[replica] Starting read-replica..."
RUST_LOG="${LOG_LEVEL}" exec mneme-core --config "$CONFIG"
