#!/usr/bin/env bash
# MnemeCache — Core node entrypoint (cluster / HA mode)
#
# Copies the static config to a runtime path, injects environment variable
# overrides (join_token, pool_bytes), then starts mneme-core.
#
# For HA (multi-core Raft): all cores share the same CA and cert material
# from the /certs volume. Core-1 (first to start) generates and publishes;
# Cores 2+ wait for the shared CA before starting.
#
# Environment variables:
#   MNEME_ADMIN_PASSWORD   Admin password (default: "secret")
#   MNEME_CLUSTER_SECRET   Shared join token for Keeper auth — REQUIRED in production
#   MNEME_POOL_BYTES       Hot RAM pool size, e.g. "512mb", "2gb" (default: "512mb")
#   MNEME_LOG_LEVEL        Log verbosity (default: "info")
#   MNEME_CONFIG           Base config path (default: /etc/mneme/core.toml)

set -euo pipefail

BASE_CFG="${MNEME_CONFIG:-/etc/mneme/core.toml}"
CONFIG=/tmp/mneme-core-runtime.toml
DATA_DIR=/var/lib/mneme
ADMIN_PASS="${MNEME_ADMIN_PASSWORD:-secret}"
LOG_LEVEL="${MNEME_LOG_LEVEL:-info}"
CA_DST=/certs/ca.crt

printf '\n  \033[1;36mMnemeCache\033[0m — Core node \033[0;37m(cluster)\033[0m\n\n'

install -d -m 750 "$DATA_DIR" 2>/dev/null || true

# ── Shared CA: wait for it if another Core is generating it ──────────────
# In HA mode all Cores share /certs volume. The first Core to start
# generates the CA and publishes cert material. Others must wait.
if [ -f "$CA_DST" ]; then
    echo "[core] Using existing CA from shared volume"
    cp "$CA_DST"         "${DATA_DIR}/ca.crt"
    cp /certs/node.crt   "${DATA_DIR}/node.crt"  2>/dev/null || true
    cp /certs/node.key   "${DATA_DIR}/node.key"   2>/dev/null || true
elif [ -d /certs ]; then
    # Another Core may be generating certs right now — wait up to 30s
    echo "[core] Waiting for shared CA cert..."
    ELAPSED=0
    while [ ! -f "$CA_DST" ] && [[ $ELAPSED -lt 30 ]]; do
        sleep 1; ELAPSED=$((ELAPSED + 1))
    done
    if [ -f "$CA_DST" ]; then
        echo "[core] Shared CA found after ${ELAPSED}s"
        cp "$CA_DST"         "${DATA_DIR}/ca.crt"
        cp /certs/node.crt   "${DATA_DIR}/node.crt"  2>/dev/null || true
        cp /certs/node.key   "${DATA_DIR}/node.key"   2>/dev/null || true
    else
        echo "[core] No shared CA — will auto-generate (this node is the first)"
    fi
fi

# ── Inject env var overrides into a runtime copy of the config ───────────
cp "$BASE_CFG" "$CONFIG"

if [ -n "${MNEME_CLUSTER_SECRET:-}" ]; then
    sed -i "s|^join_token *=.*|join_token = \"${MNEME_CLUSTER_SECRET}\"|" "$CONFIG"
    echo "[core] Applied MNEME_CLUSTER_SECRET → join_token"
fi

if [ -n "${MNEME_POOL_BYTES:-}" ]; then
    sed -i "s|^pool_bytes *=.*|pool_bytes = \"${MNEME_POOL_BYTES}\"|" "$CONFIG"
    echo "[core] Applied MNEME_POOL_BYTES → pool_bytes (${MNEME_POOL_BYTES})"
fi

# Bootstrap admin user (writes to users.db, then exits — no server needed)
echo "[core] Bootstrapping admin user..."
mneme-core --config "$CONFIG" adduser \
    --username admin \
    --password "$ADMIN_PASS" \
    --role admin 2>/dev/null || true

printf '  \033[1;32m✓ Starting Core node...\033[0m\n'
printf '    Client port : \033[1;37m6379\033[0m  (TLS)\n'
printf '    Rep port    : \033[1;37m7379\033[0m  (mTLS — Keepers/Raft peers connect here)\n'
printf '    Username    : \033[1;33madmin\033[0m\n'
printf "    Password    : \033[1;33m${ADMIN_PASS}\033[0m\n\n"

# Start server in background, publish CA cert, then hand off
RUST_LOG="${LOG_LEVEL}" mneme-core --config "$CONFIG" &
CORE_PID=$!

# Wait for TLS auto-generation and publish CA to shared volume
ELAPSED=0
until [ -f "${DATA_DIR}/ca.crt" ]; do
    if ! kill -0 "$CORE_PID" 2>/dev/null; then
        printf '\033[1;31m[core] ERROR: server exited unexpectedly\033[0m\n' >&2; exit 1
    fi
    if [[ $ELAPSED -ge 30 ]]; then break; fi
    sleep 1; ELAPSED=$((ELAPSED + 1))
done

if [ -f "${DATA_DIR}/ca.crt" ] && [ ! -f "$CA_DST" ]; then
    mkdir -p /certs
    # Publish CA cert + node cert/key to shared volume so all cluster nodes
    # (keepers, other cores) use the same trust anchor for mTLS.
    cp "${DATA_DIR}/ca.crt"   "$CA_DST"
    cp "${DATA_DIR}/node.crt" /certs/node.crt
    cp "${DATA_DIR}/node.key" /certs/node.key
    echo "[core] Published CA cert + node cert/key to shared volume"
fi

# Symlink CA cert to CLI default path so mneme-cli works without --ca-cert
if [ -f "${DATA_DIR}/ca.crt" ]; then
    mkdir -p /etc/mneme
    ln -sf "${DATA_DIR}/ca.crt" /etc/mneme/ca.crt
fi

wait "$CORE_PID"
