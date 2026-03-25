#!/usr/bin/env bash
# MnemeCache — Keeper node entrypoint (cluster mode)
#
# Waits for Core to publish the CA cert to /certs, installs it, then starts
# mneme-keeper which connects back to Core for registration (Herold flow).
#
# Environment variables:
#   KEEPER_NODE_ID     Node ID (default: "keeper-1")
#   KEEPER_POOL_BYTES  RAM grant to Core pool (default: "2gb")
#   CORE_ADDR          Core replication address (default: "mneme-core:7379")
#   MNEME_LOG_LEVEL    Log verbosity (default: "info")

set -euo pipefail

NODE_ID="${KEEPER_NODE_ID:-keeper-1}"
# Pick keeper-specific config if it exists; fall back to keeper-1.toml
DEFAULT_CFG="/etc/mneme/${NODE_ID}.toml"
if [ ! -f "$DEFAULT_CFG" ]; then DEFAULT_CFG="/etc/mneme/keeper-1.toml"; fi
CONFIG="${MNEME_CONFIG:-$DEFAULT_CFG}"
DATA_DIR=/var/lib/mneme
LOG_LEVEL="${MNEME_LOG_LEVEL:-info}"
CA_SRC=/certs/ca.crt
CA_DST="${DATA_DIR}/ca.crt"

printf '\n  \033[1;36mMnemeCache\033[0m — Keeper node \033[1;37m%s\033[0m\n\n' "$NODE_ID"

install -d -m 750 "$DATA_DIR" 2>/dev/null || true

# Wait for Core to publish CA cert (up to 120s — Core must start first)
echo "[keeper] Waiting for CA cert from Core (shared volume)..."
ELAPSED=0
until [ -f "$CA_SRC" ]; do
    if [[ $ELAPSED -ge 120 ]]; then
        printf '\033[1;31m[keeper] ERROR: CA cert not available after 120s\033[0m\n' >&2; exit 1
    fi
    sleep 1; ELAPSED=$((ELAPSED + 1))
done

cp "$CA_SRC" "$CA_DST"
# Copy Core's node cert+key so Hermes mTLS handshakes succeed:
# Core's CA trusts Core's node cert; sharing it means keepers present a
# cert that Core can verify. Required for mTLS outbound (Hermes dial_keeper).
[ -f /certs/node.crt ] && cp /certs/node.crt "${DATA_DIR}/node.crt"
[ -f /certs/node.key ] && cp /certs/node.key "${DATA_DIR}/node.key"
echo "[keeper] CA cert + node cert/key installed"

# Symlink CA cert to CLI default path so mneme-cli works without --insecure
mkdir -p /etc/mneme
ln -sf "$CA_DST" /etc/mneme/ca.crt

echo "[keeper] Starting mneme-keeper (node_id=${NODE_ID})..."
RUST_LOG="${LOG_LEVEL}" exec mneme-keeper --config "$CONFIG"
