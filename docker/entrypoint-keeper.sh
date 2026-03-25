#!/usr/bin/env bash
# MnemeCache — Keeper node entrypoint (cluster mode)
#
# Copies the static config to a runtime path, injects environment variable
# overrides (join_token, core_addr, node_id, pool_bytes), waits for Core to
# publish the CA cert, then starts mneme-keeper.
#
# Environment variables:
#   MNEME_CLUSTER_SECRET   Shared join token — must match Core's MNEME_CLUSTER_SECRET
#   MNEME_KEEPER_ID        Node ID for this Keeper (default: "keeper-1")
#   MNEME_CORE_ADDR        Core replication address (default: "mneme-core:7379")
#   MNEME_POOL_BYTES       RAM grant size, e.g. "2gb" (default: "2gb")
#   MNEME_LOG_LEVEL        Log verbosity (default: "info")
#   MNEME_CONFIG           Base config path (auto-detected from MNEME_KEEPER_ID)

set -euo pipefail

NODE_ID="${MNEME_KEEPER_ID:-keeper-1}"
CORE_ADDR="${MNEME_CORE_ADDR:-mneme-core:7379}"

# Pick keeper-specific base config; fall back to keeper-1.toml
DEFAULT_CFG="/etc/mneme/${NODE_ID}.toml"
if [ ! -f "$DEFAULT_CFG" ]; then DEFAULT_CFG="/etc/mneme/keeper-1.toml"; fi
BASE_CFG="${MNEME_CONFIG:-$DEFAULT_CFG}"
CONFIG=/tmp/mneme-keeper-runtime.toml
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
# Copy Core's node cert+key so Hermes mTLS handshakes succeed.
[ -f /certs/node.crt ] && cp /certs/node.crt "${DATA_DIR}/node.crt"
[ -f /certs/node.key ] && cp /certs/node.key "${DATA_DIR}/node.key"
echo "[keeper] CA cert + node cert/key installed"

# Symlink CA cert to CLI default path so mneme-cli works without --ca-cert
mkdir -p /etc/mneme
ln -sf "$CA_DST" /etc/mneme/ca.crt

# ── Inject env var overrides into a runtime copy of the config ───────────
cp "$BASE_CFG" "$CONFIG"

if [ -n "${MNEME_CLUSTER_SECRET:-}" ]; then
    sed -i "s|^join_token *=.*|join_token = \"${MNEME_CLUSTER_SECRET}\"|" "$CONFIG"
    echo "[keeper] Applied MNEME_CLUSTER_SECRET → join_token"
fi

# Apply MNEME_CORE_ADDR → core_addr (where this Keeper dials Core for registration)
sed -i "s|^core_addr *=.*|core_addr = \"${CORE_ADDR}\"|" "$CONFIG"
echo "[keeper] Applied MNEME_CORE_ADDR → core_addr (${CORE_ADDR})"

# Apply MNEME_KEEPER_ID → node_id
sed -i "s|^node_id *=.*|node_id = \"${NODE_ID}\"|" "$CONFIG"

# Apply MNEME_POOL_BYTES → pool_bytes (RAM grant size)
POOL_BYTES="${MNEME_POOL_BYTES:-2gb}"
sed -i "s|^pool_bytes *=.*|pool_bytes = \"${POOL_BYTES}\"|" "$CONFIG"
echo "[keeper] Pool size: ${POOL_BYTES}"

echo "[keeper] Starting mneme-keeper (node_id=${NODE_ID}, core=${CORE_ADDR})..."
RUST_LOG="${LOG_LEVEL}" exec mneme-keeper --config "$CONFIG"
