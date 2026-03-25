#!/usr/bin/env bash
# MnemeCache — Solo node entrypoint
#
# Copies the static config to a runtime path, injects environment variable
# overrides, bootstraps the admin user, then starts mneme-core in solo mode.
# WAL, snapshots, and cold store all run in-process — no separate keeper.
# Data survives restarts via WAL replay + snapshot.
#
# Environment variables:
#   MNEME_ADMIN_PASSWORD   Admin password (default: "secret")
#   MNEME_POOL_BYTES       Hot RAM pool size, e.g. "512mb", "2gb" (default: "512mb")
#   MNEME_LOG_LEVEL        Log verbosity (default: "info")
#   MNEME_CONFIG           Base config path (default: /etc/mneme/solo.toml)

set -euo pipefail

BASE_CFG="${MNEME_CONFIG:-/etc/mneme/solo.toml}"
CONFIG=/tmp/mneme-solo-runtime.toml
DATA_DIR=/var/lib/mneme
ADMIN_PASS="${MNEME_ADMIN_PASSWORD:-secret}"
LOG_LEVEL="${MNEME_LOG_LEVEL:-info}"

# ── Banner ────────────────────────────────────────────────────────────────────
printf '\n  \033[1;36mMnemeCache\033[0m — Distributed In-Memory Cache \033[0;37m(solo)\033[0m\n\n'

# ── Ensure data directory ─────────────────────────────────────────────────────
install -d -m 750 "$DATA_DIR" 2>/dev/null || true

# ── Inject env var overrides into a runtime copy of the config ───────────────
cp "$BASE_CFG" "$CONFIG"

if [ -n "${MNEME_POOL_BYTES:-}" ]; then
    sed -i "s|^pool_bytes *=.*|pool_bytes = \"${MNEME_POOL_BYTES}\"|" "$CONFIG"
    echo "[solo] Pool size: ${MNEME_POOL_BYTES}"
fi

# ── Bootstrap admin user directly into users.db (no server required) ──────────
# mneme-core adduser writes to the users.db file and exits immediately.
# Upsert semantics: safe to call on every restart.
echo "[mneme] Bootstrapping admin user..."
mneme-core --config "$CONFIG" adduser \
    --username admin \
    --password "$ADMIN_PASS" \
    --role admin 2>/dev/null || true

# ── Print connection info BEFORE starting (so it appears at the top of logs) ──
printf '\n'
printf '  \033[1;32m✓ Starting MnemeCache server...\033[0m\n'
printf '    Port     : \033[1;37m6379\033[0m  (TLS)\n'
printf '    Metrics  : \033[1;37mhttp://localhost:9090/metrics\033[0m\n'
printf '    Username : \033[1;33madmin\033[0m\n'
printf "    Password : \033[1;33m${ADMIN_PASS}\033[0m\n"
printf '\n'
printf "  \033[0;37mmneme-cli -u admin -p %s ping\033[0m\n\n" "$ADMIN_PASS"

# ── Start server (foreground — this is the container's main process) ──────────
# Solo mode: auto-generated CA lands at /var/lib/mneme/ca.crt.
# Start server, wait for CA, then symlink to the CLI's default lookup path so
# mneme-cli works without --ca-cert flags.
RUST_LOG="${LOG_LEVEL}" mneme-core --config "$CONFIG" &
SOLO_PID=$!

ELAPSED=0
until [ -f "${DATA_DIR}/ca.crt" ]; do
    if ! kill -0 "$SOLO_PID" 2>/dev/null; then
        printf '\033[1;31m[solo] ERROR: server exited unexpectedly\033[0m\n' >&2; exit 1
    fi
    if [[ $ELAPSED -ge 30 ]]; then break; fi
    sleep 1; ELAPSED=$((ELAPSED + 1))
done

if [ -f "${DATA_DIR}/ca.crt" ]; then
    mkdir -p /etc/mneme
    ln -sf "${DATA_DIR}/ca.crt" /etc/mneme/ca.crt
fi

wait "$SOLO_PID"
