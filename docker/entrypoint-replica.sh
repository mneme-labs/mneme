#!/usr/bin/env bash
# MnemeCache — Read Replica entrypoint (cluster mode)
#
# Copies the static config to a runtime path, injects environment variable
# overrides (join_token, core_addr, node_id), waits for Core to publish the CA
# cert, then starts mneme-core in read-replica mode.
#
# Replicas accept EVENTUAL consistency reads only. They receive PushKey frames
# from Core and maintain a local RAM pool.
#
# Environment variables:
#   MNEME_CLUSTER_SECRET   Shared join token — must match Core's MNEME_CLUSTER_SECRET
#   MNEME_CORE_ADDR        Core replication address (default: "mneme-core:7379")
#   MNEME_NODE_ID          Node ID for this replica (default: "replica-1")
#   MNEME_POOL_BYTES       Hot RAM pool size (default: "512mb")
#   MNEME_ADMIN_PASSWORD   Admin password bootstrapped into users.db (default: "secret")
#   MNEME_LOG_LEVEL        Log verbosity (default: "info")
#   MNEME_CONFIG           Base config path (default: /etc/mneme/replica.toml)

set -euo pipefail

BASE_CFG="${MNEME_CONFIG:-/etc/mneme/replica.toml}"
CONFIG=/tmp/mneme-replica-runtime.toml
DATA_DIR=/var/lib/mneme
LOG_LEVEL="${MNEME_LOG_LEVEL:-info}"
CA_SRC=/certs/ca.crt
CA_DST="${DATA_DIR}/ca.crt"
CORE_ADDR="${MNEME_CORE_ADDR:-mneme-core:7379}"
NODE_ID="${MNEME_NODE_ID:-replica-1}"

printf '\n  \033[1;36mMnemeCache\033[0m — Read Replica \033[1;37m%s\033[0m\n\n' "$NODE_ID"

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

# Symlink CA cert to CLI default path so mneme-cli works without --ca-cert
mkdir -p /etc/mneme
ln -sf "$CA_DST" /etc/mneme/ca.crt

# ── Inject env var overrides into a runtime copy of the config ───────────
cp "$BASE_CFG" "$CONFIG"

if [ -n "${MNEME_CLUSTER_SECRET:-}" ]; then
    sed -i "s|^join_token *=.*|join_token = \"${MNEME_CLUSTER_SECRET}\"|" "$CONFIG"
    echo "[replica] Applied MNEME_CLUSTER_SECRET → join_token"
fi

sed -i "s|^core_addr *=.*|core_addr = \"${CORE_ADDR}\"|" "$CONFIG"
echo "[replica] Applied MNEME_CORE_ADDR → core_addr (${CORE_ADDR})"

sed -i "s|^node_id *=.*|node_id = \"${NODE_ID}\"|" "$CONFIG"

if [ -n "${MNEME_POOL_BYTES:-}" ]; then
    sed -i "s|^pool_bytes *=.*|pool_bytes = \"${MNEME_POOL_BYTES}\"|" "$CONFIG"
fi

# Bootstrap admin user so clients can authenticate to the replica
ADMIN_PASS="${MNEME_ADMIN_PASSWORD:-secret}"
echo "[replica] Bootstrapping admin user..."
mneme-core --config "$CONFIG" adduser \
    --username admin \
    --password "$ADMIN_PASS" \
    --role admin 2>/dev/null || true

echo "[replica] Starting read-replica (node=${NODE_ID}, core=${CORE_ADDR})..."
RUST_LOG="${LOG_LEVEL}" exec mneme-core --config "$CONFIG"
