#!/usr/bin/env bash
# setup-linux.sh — Native Linux setup for MnemeCache.
# Installs system deps, builds from source, configures systemd services,
# tunes the kernel (huge pages, file-descriptor limits), and starts the cluster.
#
# Normally called from setup.sh (which handles topology selection).
# Can also be run directly — will prompt for topology if not already set.
#
# Usage (direct):
#   sudo ./scripts/setup-linux.sh                        # interactive
#   sudo TOPOLOGY=solo ./scripts/setup-linux.sh          # solo Core
#   sudo TOPOLOGY=cluster KEEPER_COUNT=3 \
#        ./scripts/setup-linux.sh                        # Core + 3 Keepers
#   sudo TOPOLOGY=keeper CORE_ADDR=10.0.0.1:7379 \
#        ./scripts/setup-linux.sh                        # Keeper joining Core

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

# ── Static config (env-overridable) ───────────────────────────────────────────
INSTALL_PREFIX="${INSTALL_PREFIX:-/usr/local}"
DATA_DIR="${DATA_DIR:-/var/lib/mneme}"
CONFIG_DIR="${CONFIG_DIR:-/etc/mneme}"
CORE_POOL="${CORE_POOL:-1gb}"
KEEPER_POOL="${KEEPER_POOL:-2gb}"
HUGE_PAGES="${HUGE_PAGES:-512}"   # 2 MB huge pages to reserve; 0 = skip
MIN_KERNEL="5.19"
MNEME_USER="mneme"

# ── Pre-flight ────────────────────────────────────────────────────────────────
banner
step "Pre-flight checks"

# Must run as root
if [[ "$EUID" -ne 0 ]]; then
  fatal "Please re-run as root: sudo $0"
fi

# ── Topology selection (when run directly, not via setup.sh) ──────────────────
# setup.sh exports TOPOLOGY + ROLE + KEEPER_COUNT + CORE_ADDR before exec'ing
# this script. When run directly those vars may be empty — ask interactively.
# ── Uninstall ─────────────────────────────────────────────────────────────────
do_uninstall() {
  step "Uninstalling MnemeCache"

  echo ""
  warn "This will remove all MnemeCache services, binaries, config, and data."
  warn "  Binaries : ${INSTALL_PREFIX}/bin/mneme-{core,keeper,cli}"
  warn "  Config   : ${CONFIG_DIR}/"
  warn "  Data     : ${DATA_DIR}/  (WAL, snapshots, TLS certs)"
  warn "  Systemd  : /etc/systemd/system/mneme-*.service"
  warn "  Sysctl   : /etc/sysctl.d/99-mneme.conf"
  warn "  Limits   : /etc/security/limits.d/99-mneme.conf"
  warn "  User     : ${MNEME_USER}"
  echo ""
  ask_yn _CONFIRM "Are you sure you want to uninstall? This cannot be undone." "n"
  [[ "$_CONFIRM" != "y" ]] && { info "Uninstall cancelled."; exit 0; }

  # Stop and disable all services
  info "Stopping and disabling MnemeCache services…"
  for unit in $(systemctl list-units --plain --no-legend 'mneme-*.service' \
                  2>/dev/null | awk '{print $1}'); do
    systemctl stop    "$unit" 2>/dev/null || true
    systemctl disable "$unit" 2>/dev/null || true
    success "Stopped and disabled: ${unit}"
  done

  # Remove systemd unit files
  for unit_file in /etc/systemd/system/mneme-*.service; do
    [[ -e "$unit_file" ]] || continue
    rm -f "$unit_file"
    success "Removed: ${unit_file}"
  done
  systemctl daemon-reload

  # Remove binaries
  for bin in mneme-core mneme-keeper mneme-cli; do
    local bin_path="${INSTALL_PREFIX}/bin/${bin}"
    if [[ -f "$bin_path" ]]; then
      rm -f "$bin_path"
      success "Removed: ${bin_path}"
    fi
  done

  # Remove config, data, sysctl, limits
  for path in \
    "${CONFIG_DIR}" \
    "${DATA_DIR}" \
    "/etc/sysctl.d/99-mneme.conf" \
    "/etc/sysctl.d/99-mneme-hugepages.conf" \
    "/etc/security/limits.d/99-mneme.conf"
  do
    if [[ -e "$path" ]]; then
      rm -rf "$path"
      success "Removed: ${path}"
    fi
  done

  # Remove system user
  if id "$MNEME_USER" &>/dev/null; then
    userdel "$MNEME_USER" 2>/dev/null || true
    success "Removed system user: ${MNEME_USER}"
  fi

  # Restore sysctl without mneme params
  sysctl --system -q 2>/dev/null || true

  echo ""
  echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════${RESET}"
  echo -e "${BOLD}  MnemeCache uninstalled successfully.${RESET}"
  echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════${RESET}"
  echo ""
  info "Rust toolchain and source tree were NOT removed."
  info "To remove those run:  rm -rf ~/.cargo  ~/path/to/repo/target"
  echo ""
  exit 0
}

if [[ -z "${TOPOLOGY:-}" ]]; then
  step "Deployment topology"
  echo ""
  echo "  1) Solo node    — Core + embedded WAL/snapshots; self-contained, restart-safe"
  echo "  2) Core node    — Distributed primary; Keepers join from other machines"
  echo "  3) Keeper node  — Persistent storage; joins a running Core elsewhere"
  echo "  4) Read replica — EVENTUAL-read-only copy; syncs from a primary Core"
  echo "  5) Uninstall    — Remove all MnemeCache services, data, and config"
  echo ""
  echo -e "  ${DIM}Solo = single machine, no extra processes. Core/Keeper = multi-machine cluster.${RESET}"
  echo ""
  while true; do
    read -rp "$(echo -e "${BOLD}Your choice [1/2/3/4/5]: ${RESET}")" _TC
    case "$_TC" in
      1)
        TOPOLOGY="solo";    ROLE="solo";         KEEPER_COUNT="0"
        CORE_ADDR="${CORE_ADDR:-127.0.0.1:7379}"
        break ;;
      2)
        TOPOLOGY="core";    ROLE="core";          KEEPER_COUNT="0"
        CORE_ADDR="${CORE_ADDR:-127.0.0.1:7379}"
        break ;;
      3)
        TOPOLOGY="keeper";  ROLE="keeper"
        ask KEEPER_COUNT "Number of Keepers on this machine" "1"
        ask CORE_ADDR "Core replication address (host:port)" ""
        break ;;
      4)
        TOPOLOGY="replica"; ROLE="read-replica";  KEEPER_COUNT="0"
        ask CORE_ADDR "Primary Core address to sync from (host:port)" ""
        break ;;
      5)
        do_uninstall ;;
      *) warn "Please enter 1, 2, 3, 4, or 5." ;;
    esac
  done
  echo ""
fi

# Apply defaults for any vars not set by setup.sh or the menu above
TOPOLOGY="${TOPOLOGY:-solo}"
ROLE="${ROLE:-solo}"
KEEPER_COUNT="${KEEPER_COUNT:-0}"
CORE_ADDR="${CORE_ADDR:-127.0.0.1:7379}"

info "Topology : ${TOPOLOGY}  (ROLE=${ROLE}, KEEPERS=${KEEPER_COUNT}${CORE_ADDR:+, CORE=${CORE_ADDR}})"

# ── Collect all configuration parameters before the slow compile step ─────────
# This lets the operator walk away once compilation starts.
step "Node configuration"
echo ""

case "$ROLE" in
  solo|core)
    ask MNEME_NODE_ID "Node ID (unique cluster name, e.g. mneme-core-prod)" ""
    ask CORE_POOL     "RAM pool size for this Core node" "${CORE_POOL:-1gb}"
    ask DATA_DIR      "Data directory" "${DATA_DIR:-/var/lib/mneme}"
    ;;
  keeper)
    # Collect a node ID for each keeper instance on this machine.
    KEEPER_NODE_IDS=()
    for _ki in $(seq 0 $((KEEPER_COUNT - 1))); do
      ask _KN "Node ID for keeper-${_ki} (e.g. keeper-nyc-${_ki})" ""
      KEEPER_NODE_IDS+=("$_KN")
    done
    ask KEEPER_POOL "RAM pool (WAL buffer) size for each Keeper" "${KEEPER_POOL:-2gb}"
    ask DATA_DIR     "Data directory" "${DATA_DIR:-/var/lib/mneme}"
    ;;
  read-replica)
    ask MNEME_NODE_ID "Node ID (unique cluster name, e.g. replica-eu-0)" ""
    ask CORE_POOL     "RAM pool size for this Replica node" "${CORE_POOL:-1gb}"
    ask DATA_DIR      "Data directory" "${DATA_DIR:-/var/lib/mneme}"
    ;;
esac
echo ""

# ── Kernel version check
KERNEL_VER="$(uname -r | grep -oP '^\d+\.\d+')"
if ! version_gte "$KERNEL_VER" "$MIN_KERNEL"; then
  warn "Kernel ${KERNEL_VER} detected. MnemeCache requires ${MIN_KERNEL}+."
  warn "io_uring, O_DIRECT and perf_event_open may not work correctly."
  ask_yn CONTINUE "Continue anyway?" "n"
  [[ "$CONTINUE" == "n" ]] && fatal "Aborted."
fi
success "Kernel ${KERNEL_VER} OK"

# Detect package manager
if command -v apt-get &>/dev/null; then
  PKG_MGR="apt"
elif command -v dnf &>/dev/null; then
  PKG_MGR="dnf"
elif command -v yum &>/dev/null; then
  PKG_MGR="yum"
else
  fatal "No supported package manager found (apt, dnf, yum)."
fi
info "Package manager: ${PKG_MGR}"

# ── System dependencies ───────────────────────────────────────────────────────
step "Installing system dependencies"
case "$PKG_MGR" in
  apt)
    apt-get update -qq
    apt-get install -y --no-install-recommends \
      build-essential pkg-config clang libclang-dev linux-libc-dev \
      curl openssl ca-certificates netcat-openbsd
    ;;
  dnf|yum)
    "$PKG_MGR" install -y gcc pkg-config clang clang-devel kernel-headers \
      curl openssl ca-certificates nmap-ncat
    ;;
esac
success "System dependencies installed."

# ── Rust toolchain + build ────────────────────────────────────────────────────
step "Rust toolchain"

# Resolve the repo root early — we need it to detect the correct build user.
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Detect the owner of the repo directory.  This is the user that cargo MUST run
# as.  Running cargo as root inside a user-owned repo corrupts ownership of
# Cargo.lock, target/, and the .cargo/ index, causing "Permission denied" on
# every subsequent non-root invocation.  Using SUDO_USER is insufficient because
# it is only set when the caller used sudo — it is empty on a direct root login.
BUILD_USER="$(stat -c '%U' "$REPO_ROOT")"
info "Repo: ${REPO_ROOT}"
info "Repo owner (build user): ${BUILD_USER}"

if [[ "$BUILD_USER" == "root" ]]; then
  # Repo is root-owned — safe to build as root.
  ensure_rust
  CARGO="$(command -v cargo 2>/dev/null || echo "${HOME}/.cargo/bin/cargo")"
  RUSTUP="$(command -v rustup 2>/dev/null || echo "${HOME}/.cargo/bin/rustup")"
else
  # Repo belongs to a non-root user.
  # 1. Restore ownership of any files that a prior root-run of cargo may have
  #    created (Cargo.lock, target/, .cargo/) so the build user can write them.
  info "Restoring file ownership in repo to '${BUILD_USER}'…"
  chown -R "${BUILD_USER}:${BUILD_USER}" "$REPO_ROOT"

  BUILD_HOME="$(getent passwd "$BUILD_USER" | cut -d: -f6)"
  CARGO="${BUILD_HOME}/.cargo/bin/cargo"
  RUSTUP="${BUILD_HOME}/.cargo/bin/rustup"

  # 2. Install or update Rust toolchain as the repo owner.
  if [[ ! -x "$CARGO" ]]; then
    info "Installing Rust toolchain for user '${BUILD_USER}'…"
    sudo -u "$BUILD_USER" \
      sh -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
             | sh -s -- -y --default-toolchain stable --no-modify-path'
  else
    info "Updating Rust toolchain for user '${BUILD_USER}'…"
    sudo -u "$BUILD_USER" "$RUSTUP" update stable --no-self-update
  fi
fi
info "cargo: ${CARGO}"

# ── Build ─────────────────────────────────────────────────────────────────────
step "Building MnemeCache from source"

if [[ "$BUILD_USER" == "root" ]]; then
  RUSTFLAGS='-C target-cpu=native' \
    "$CARGO" build --release \
      --workspace --exclude mneme-bench \
      --manifest-path "${REPO_ROOT}/Cargo.toml"
else
  # Use sudo -u so the build runs under the repo owner's identity.
  # Pass --manifest-path so cargo does not rely on a shell cd.
  sudo -u "$BUILD_USER" \
    env RUSTFLAGS='-C target-cpu=native' \
    "$CARGO" build --release \
      --workspace --exclude mneme-bench \
      --manifest-path "${REPO_ROOT}/Cargo.toml"
fi
success "Build complete."

# ── Install binaries ──────────────────────────────────────────────────────────
step "Installing binaries to ${INSTALL_PREFIX}/bin"
install -m 755 "${REPO_ROOT}/target/release/mneme-core"   "${INSTALL_PREFIX}/bin/"
install -m 755 "${REPO_ROOT}/target/release/mneme-keeper" "${INSTALL_PREFIX}/bin/"
install -m 755 "${REPO_ROOT}/target/release/mneme-cli"    "${INSTALL_PREFIX}/bin/"
success "Binaries installed."

# ── System user ───────────────────────────────────────────────────────────────
step "Creating system user '${MNEME_USER}'"
if ! id "$MNEME_USER" &>/dev/null; then
  useradd -r -s /bin/false -d "$DATA_DIR" -c "MnemeCache daemon" "$MNEME_USER"
  success "User '${MNEME_USER}' created."
else
  info "User '${MNEME_USER}' already exists."
fi

# ── Directories ───────────────────────────────────────────────────────────────
step "Creating directories"
ensure_dir "$CONFIG_DIR" "root:root" "755"
ensure_dir "$DATA_DIR"   "${MNEME_USER}:${MNEME_USER}" "750"
# Recursively fix ownership in case a previous root-run created files inside DATA_DIR.
chown -R "${MNEME_USER}:${MNEME_USER}" "$DATA_DIR"

# ── Secrets ───────────────────────────────────────────────────────────────────
step "Configuring secrets"
ENV_FILE="${CONFIG_DIR}/mneme.env"

# ── Helper: decode a join bundle produced by a Core node ──────────────────────
# Join bundle format (single line, no spaces):
#   <base64_ca_pem_no_newlines>:<cluster_secret>:<join_token>
# Populates: MNEME_CLUSTER_SECRET, MNEME_JOIN_TOKEN
# Writes ca.crt to the given directory ($1).
decode_join_bundle() {
  local ca_dir="$1"
  local bundle="$2"
  # Split on the LAST two colons: ca may have none; secret and token have none.
  # Format guarantees no colon in base64 (uses standard alphabet) or hex strings.
  local b64_ca secret token
  b64_ca="$(echo "$bundle" | awk -F: '{OFS=":"; NF-=2; print}')"
  secret="$(echo "$bundle" | awk -F: '{print $(NF-1)}')"
  token="$(echo  "$bundle" | awk -F: '{print $NF}')"
  if [[ -z "$b64_ca" || -z "$secret" || -z "$token" ]]; then
    fatal "Join bundle appears malformed. Expected format: <base64-ca>:<cluster-secret>:<join-token>"
  fi
  mkdir -p "$ca_dir"
  echo "$b64_ca" | base64 -d > "${ca_dir}/ca.crt" \
    || fatal "Failed to decode CA certificate from join bundle."
  chown "${MNEME_USER}:${MNEME_USER}" "${ca_dir}/ca.crt"
  chmod 640 "${ca_dir}/ca.crt"
  MNEME_CLUSTER_SECRET="$secret"
  MNEME_JOIN_TOKEN="$token"
  success "Join bundle decoded — CA cert written to ${ca_dir}/ca.crt"
}

# ── Core / Solo: generate secrets if not already present ─────────────────────
if [[ "$ROLE" == "core" || "$ROLE" == "solo" ]]; then
  if [[ -f "$ENV_FILE" ]]; then
    info "Found existing ${ENV_FILE} — skipping secret generation."
    # shellcheck source=/dev/null
    source "$ENV_FILE"
  else
    info "Generating cryptographically random secrets…"
    MNEME_CLUSTER_SECRET="$(gen_secret)"
    MNEME_ADMIN_PASSWORD="$(gen_password)"
    # join_token: "mneme_tok_" prefix + 32 hex chars = 42 chars total
    MNEME_JOIN_TOKEN="mneme_tok_$(openssl rand -hex 16)"
    cat > "$ENV_FILE" <<EOF
# MnemeCache secrets — generated by setup-linux.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)
# Mode 600 — readable only by root. Do NOT share this file.
MNEME_CLUSTER_SECRET=${MNEME_CLUSTER_SECRET}
MNEME_ADMIN_PASSWORD=${MNEME_ADMIN_PASSWORD}
MNEME_JOIN_TOKEN=${MNEME_JOIN_TOKEN}
EOF
    chmod 600 "$ENV_FILE"
    success "Secrets written to ${ENV_FILE}"
  fi
  # Ensure join_token is loaded (may have been sourced above or generated)
  MNEME_JOIN_TOKEN="${MNEME_JOIN_TOKEN:-}"
  if [[ -z "$MNEME_JOIN_TOKEN" ]]; then
    MNEME_JOIN_TOKEN="$(grep -E '^MNEME_JOIN_TOKEN=' "$ENV_FILE" | cut -d= -f2- || echo '')"
  fi

# ── Keeper / Read-replica: obtain secrets via join bundle or manual entry ─────
else
  if [[ -f "$ENV_FILE" ]]; then
    info "Found existing ${ENV_FILE} — loading secrets."
    # shellcheck source=/dev/null
    source "$ENV_FILE"
  else
    echo ""
    echo -e "${BOLD}${CYAN}  How do you want to provide the cluster credentials?${RESET}"
    echo ""
    echo "  1) Join bundle  — paste one line printed by the Core setup (recommended)"
    echo "  2) Manual entry — paste CA cert path, cluster secret, and join token separately"
    echo ""
    read -rp "$(echo -e "${BOLD}  Your choice [1/2]: ${RESET}")" _CRED_MODE
    echo ""

    if [[ "$_CRED_MODE" == "1" ]]; then
      echo -e "  ${DIM}Paste the join bundle line from the Core machine (then press Enter):${RESET}"
      read -rp "  Bundle: " _RAW_BUNDLE
      # Determine where to put the CA cert (first keeper or replica sub-dir)
      if [[ "$ROLE" == "keeper" ]]; then
        _CA_DEST="${DATA_DIR}/keeper-0"
      else
        _CA_DEST="${DATA_DIR}/replica-0"
      fi
      decode_join_bundle "$_CA_DEST" "$_RAW_BUNDLE"
    else
      echo -e "  ${DIM}You can also run: scp root@<core-ip>:${DATA_DIR}/ca.crt /tmp/ca.crt${RESET}"
      echo ""
      read -rp "  Path to Core's ca.crt file [/tmp/ca.crt]: " _CA_SRC
      _CA_SRC="${_CA_SRC:-/tmp/ca.crt}"
      [[ -f "$_CA_SRC" ]] || fatal "CA cert not found at '${_CA_SRC}'"

      read -rp "  Cluster secret (from Core's mneme.env MNEME_CLUSTER_SECRET): " MNEME_CLUSTER_SECRET
      read -rp "  Join token    (from Core's mneme.env MNEME_JOIN_TOKEN):       " MNEME_JOIN_TOKEN
      [[ -n "$MNEME_CLUSTER_SECRET" ]] || fatal "Cluster secret must not be empty."
      [[ -n "$MNEME_JOIN_TOKEN"     ]] || fatal "Join token must not be empty."

      # Copy the CA cert to the first keeper/replica dir (will be copied to others below)
      if [[ "$ROLE" == "keeper" ]]; then
        _CA_DEST="${DATA_DIR}/keeper-0"
      else
        _CA_DEST="${DATA_DIR}/replica-0"
      fi
      mkdir -p "$_CA_DEST"
      cp "$_CA_SRC" "${_CA_DEST}/ca.crt"
      chown "${MNEME_USER}:${MNEME_USER}" "${_CA_DEST}/ca.crt"
      chmod 640 "${_CA_DEST}/ca.crt"
      success "CA cert installed to ${_CA_DEST}/ca.crt"
    fi

    # If there are multiple keepers/replicas on this machine, copy ca.crt to each dir
    for _i in $(seq 1 $((KEEPER_COUNT - 1))); do
      _dst="${DATA_DIR}/keeper-${_i}"
      mkdir -p "$_dst"
      cp "${DATA_DIR}/keeper-0/ca.crt" "${_dst}/ca.crt" 2>/dev/null || true
      chown "${MNEME_USER}:${MNEME_USER}" "${_dst}/ca.crt" 2>/dev/null || true
    done

    MNEME_ADMIN_PASSWORD=""   # not used on keeper/replica
    cat > "$ENV_FILE" <<EOF
# MnemeCache secrets — written by setup-linux.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)
# Mode 600 — readable only by root.
MNEME_CLUSTER_SECRET=${MNEME_CLUSTER_SECRET}
MNEME_JOIN_TOKEN=${MNEME_JOIN_TOKEN}
EOF
    chmod 600 "$ENV_FILE"
    success "Credentials saved to ${ENV_FILE}"
  fi
fi

# ── Core configuration ────────────────────────────────────────────────────────
write_core_config() {
  # Solo mode embeds an Hypnos keeper inside the Core process — no separate keeper
  # service needed. Persistence (WAL + snapshots + cold store) runs in-process.
  local node_role
  [[ "$TOPOLOGY" == "solo" ]] && node_role="solo" || node_role="core"

  cat > "${CONFIG_DIR}/core.toml" <<EOF
[node]
role         = "${node_role}"
node_id      = "${MNEME_NODE_ID}"
bind         = "0.0.0.0"
port         = 6379
rep_port     = 7379
metrics_port = 9090
join_token   = "${MNEME_JOIN_TOKEN}"

[memory]
pool_bytes         = "${CORE_POOL}"
eviction_threshold = 0.90
huge_pages         = $([[ "$HUGE_PAGES" -gt 0 ]] && echo true || echo false)

[cluster]
heartbeat_ms      = 500
election_min_ms   = 1500
election_max_ms   = 3000
quorum_timeout_ms = 200

[persistence]
wal_dir             = "${DATA_DIR}"
snapshot_interval_s = 60
wal_max_mb          = 256

[tls]
cert          = "${DATA_DIR}/node.crt"
key           = "${DATA_DIR}/node.key"
# ca_cert MUST live in DATA_DIR — systemd ProtectSystem=strict blocks writes to /etc/
ca_cert       = "${DATA_DIR}/ca.crt"
auto_generate = true
server_name   = "mneme.local"
extra_sans    = ["$(hostname -f)"]

[auth]
users_db       = "${DATA_DIR}/users.db"
cluster_secret = "${MNEME_CLUSTER_SECRET}"
token_ttl_h    = 24

[connections]
max_total          = 100000
max_per_ip         = 1000
idle_timeout_s     = 30
tcp_keepalive_s    = 10
max_in_flight      = 200000
request_timeout_ms = 5000

[logging]
level  = "info"
format = "json"
EOF
  info "Wrote ${CONFIG_DIR}/core.toml  (role=${node_role})"
}

# ── Keeper configuration ──────────────────────────────────────────────────────
write_keeper_config() {
  local idx="$1"
  # Use the user-supplied node ID collected in the "Node configuration" step.
  # KEEPER_NODE_IDS is the global array populated there.
  local node_id="${KEEPER_NODE_IDS[$idx]}"
  local metrics_port=$((9090 + idx))
  local rep_port=$((7379 + idx))

  cat > "${CONFIG_DIR}/keeper-${idx}.toml" <<EOF
[node]
role         = "keeper"
node_id      = "${node_id}"
bind         = "0.0.0.0"
rep_port     = ${rep_port}
metrics_port = ${metrics_port}
core_addr    = "${CORE_ADDR}"
join_token   = "${MNEME_JOIN_TOKEN}"

[memory]
pool_bytes         = "${KEEPER_POOL}"
eviction_threshold = 0.90
huge_pages         = $([[ "$HUGE_PAGES" -gt 0 ]] && echo true || echo false)

[persistence]
wal_dir             = "${DATA_DIR}/keeper-${idx}"
snapshot_interval_s = 60
wal_max_mb          = 512

[tls]
cert          = "${DATA_DIR}/keeper-${idx}/node.crt"
key           = "${DATA_DIR}/keeper-${idx}/node.key"
# ca_cert is the Core's CA cert — installed from join bundle or scp
ca_cert       = "${DATA_DIR}/keeper-${idx}/ca.crt"
auto_generate = true
server_name   = "mneme.local"

[auth]
cluster_secret = "${MNEME_CLUSTER_SECRET}"

[logging]
level  = "info"
format = "json"
EOF
  ensure_dir "${DATA_DIR}/keeper-${idx}" "${MNEME_USER}:${MNEME_USER}" "750"
  chown -R "${MNEME_USER}:${MNEME_USER}" "${DATA_DIR}/keeper-${idx}" 2>/dev/null || true
  info "Wrote ${CONFIG_DIR}/keeper-${idx}.toml"
}

# ── Read replica configuration ────────────────────────────────────────────────
write_replica_config() {
  local idx="${1:-0}"
  local client_port=$((6380 + idx))
  local rep_port=$((7380 + idx))
  local metrics_port=$((9095 + idx))

  cat > "${CONFIG_DIR}/replica-${idx}.toml" <<EOF
[node]
role         = "read-replica"
node_id      = "${MNEME_NODE_ID}"
bind         = "0.0.0.0"
port         = ${client_port}
rep_port     = ${rep_port}
metrics_port = ${metrics_port}
core_addr    = "${CORE_ADDR}"
join_token   = "${MNEME_JOIN_TOKEN}"

[memory]
pool_bytes         = "${CORE_POOL}"
eviction_threshold = 0.90
huge_pages         = $([[ "$HUGE_PAGES" -gt 0 ]] && echo true || echo false)

[cluster]
# Read replicas do not participate in Raft; they only receive replicated state.
heartbeat_ms = 500

[tls]
cert          = "${DATA_DIR}/replica-${idx}/node.crt"
key           = "${DATA_DIR}/replica-${idx}/node.key"
# ca_cert is the Core's CA cert — installed from join bundle or scp
ca_cert       = "${DATA_DIR}/replica-${idx}/ca.crt"
auto_generate = true
server_name   = "mneme.local"

[auth]
cluster_secret = "${MNEME_CLUSTER_SECRET}"
token_ttl_h    = 24

[connections]
max_total          = 100000
max_per_ip         = 1000
idle_timeout_s     = 30
tcp_keepalive_s    = 10
max_in_flight      = 200000
request_timeout_ms = 5000

[logging]
level  = "info"
format = "json"
EOF
  ensure_dir "${DATA_DIR}/replica-${idx}" "${MNEME_USER}:${MNEME_USER}" "750"
  chown -R "${MNEME_USER}:${MNEME_USER}" "${DATA_DIR}/replica-${idx}" 2>/dev/null || true
  info "Wrote ${CONFIG_DIR}/replica-${idx}.toml  (client port: ${client_port})"
}

# ── Write systemd unit — Core ─────────────────────────────────────────────────
write_core_systemd() {
  cat > /etc/systemd/system/mneme-core.service <<EOF
[Unit]
Description=MnemeCache Core node (Mnemosyne)
Documentation=https://github.com/mneme-labs/mneme
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${MNEME_USER}
Group=${MNEME_USER}
EnvironmentFile=${CONFIG_DIR}/mneme.env
ExecStart=${INSTALL_PREFIX}/bin/mneme-core --config ${CONFIG_DIR}/core.toml
Restart=on-failure
RestartSec=5
TimeoutStartSec=30
TimeoutStopSec=60

# Security hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=${DATA_DIR}
PrivateTmp=yes

# Resource limits
LimitNOFILE=1048576
LimitNPROC=65536
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
EOF
  info "Wrote /etc/systemd/system/mneme-core.service"
}

# ── Write systemd unit — Keeper ───────────────────────────────────────────────
write_keeper_systemd() {
  local idx="$1"
  local rep_port=$((7379 + idx))
  # Precompute conditional values — ${[[ ... ]]} is not valid bash inside heredocs.
  local after_core=""
  local requires_core=""
  if [[ "$ROLE" == "both" ]]; then
    after_core=" mneme-core.service"
    requires_core="Requires=mneme-core.service"
  fi
  cat > "/etc/systemd/system/mneme-keeper-${idx}.service" <<EOF
[Unit]
Description=MnemeCache Keeper node (Hypnos-${idx})
Documentation=https://github.com/mneme-labs/mneme
After=network-online.target${after_core}
Wants=network-online.target
${requires_core}

[Service]
Type=simple
User=${MNEME_USER}
Group=${MNEME_USER}
EnvironmentFile=${CONFIG_DIR}/mneme.env
ExecStart=${INSTALL_PREFIX}/bin/mneme-keeper --config ${CONFIG_DIR}/keeper-${idx}.toml
Restart=on-failure
RestartSec=5
TimeoutStartSec=60
TimeoutStopSec=120

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=${DATA_DIR}/keeper-${idx}
PrivateTmp=yes

LimitNOFILE=1048576
LimitNPROC=65536
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
EOF
  info "Wrote /etc/systemd/system/mneme-keeper-${idx}.service"
}

# ── Write systemd unit — Read replica ─────────────────────────────────────────
write_replica_systemd() {
  local idx="${1:-0}"
  local client_port=$((6380 + idx))
  cat > "/etc/systemd/system/mneme-replica-${idx}.service" <<EOF
[Unit]
Description=MnemeCache read replica ${idx} — EVENTUAL reads, syncs from ${CORE_ADDR}
Documentation=https://github.com/mneme-labs/mneme
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${MNEME_USER}
Group=${MNEME_USER}
EnvironmentFile=${CONFIG_DIR}/mneme.env
ExecStart=${INSTALL_PREFIX}/bin/mneme-core --config ${CONFIG_DIR}/replica-${idx}.toml
Restart=on-failure
RestartSec=5
TimeoutStartSec=60
TimeoutStopSec=60

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=${DATA_DIR}/replica-${idx}
PrivateTmp=yes

LimitNOFILE=1048576
LimitNPROC=65536
LimitMEMLOCK=infinity

[Install]
WantedBy=multi-user.target
EOF
  info "Wrote /etc/systemd/system/mneme-replica-${idx}.service  (client :${client_port})"
}

# ── Kernel tuning ─────────────────────────────────────────────────────────────
tune_kernel() {
  step "Kernel tuning"

  # Huge pages — apply immediately to the running kernel
  if [[ "$HUGE_PAGES" -gt 0 ]]; then
    echo "$HUGE_PAGES" > /proc/sys/vm/nr_hugepages
    success "Huge pages: ${HUGE_PAGES} × 2 MB reserved."
  fi

  # Transparent hugepage defragmentation — reduce latency spikes
  if [[ -f /sys/kernel/mm/transparent_hugepage/defrag ]]; then
    echo madvise > /sys/kernel/mm/transparent_hugepage/defrag
    echo 'echo madvise > /sys/kernel/mm/transparent_hugepage/defrag' \
      >> /etc/rc.local 2>/dev/null || true
  fi

  # File-descriptor limits
  cat > /etc/security/limits.d/99-mneme.conf <<EOF
# MnemeCache — open file limits (100k connections × 2 + WAL fds)
${MNEME_USER} soft nofile 1048576
${MNEME_USER} hard nofile 1048576
EOF

  # Write the complete sysctl file in one pass (overwrite, never append).
  # Appending would duplicate entries on every re-run of setup.
  {
    echo "# MnemeCache — generated by setup-linux.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "# Do not edit by hand — re-run setup-linux.sh to regenerate."
    echo ""
    echo "# Network"
    echo "net.core.somaxconn           = 65535"
    echo "net.ipv4.tcp_max_syn_backlog = 65535"
    echo "net.ipv4.tcp_tw_reuse        = 1"
    echo "net.core.rmem_max            = 16777216"
    echo "net.core.wmem_max            = 16777216"
    if [[ "$HUGE_PAGES" -gt 0 ]]; then
      echo ""
      echo "# Huge pages"
      echo "vm.nr_hugepages = ${HUGE_PAGES}"
    fi
  } > /etc/sysctl.d/99-mneme.conf

  sysctl --system -q
  success "Kernel parameters applied."

  # perf_event_paranoid
  if [[ -f /proc/sys/kernel/perf_event_paranoid ]]; then
    local current
    current="$(cat /proc/sys/kernel/perf_event_paranoid)"
    if [[ "$current" -gt 1 ]]; then
      ask_yn SET_PERF "Enable hardware perf counters (perf_event_paranoid=1)?" "y"
      if [[ "$SET_PERF" == "y" ]]; then
        echo 1 > /proc/sys/kernel/perf_event_paranoid
        # Append only the perf line — the file was freshly written just above.
        echo "kernel.perf_event_paranoid = 1" >> /etc/sysctl.d/99-mneme.conf
        success "Hardware perf counters enabled."
      fi
    fi
  fi
}

# ── Main ──────────────────────────────────────────────────────────────────────
step "Writing configuration files"

# Replica count comes from KEEPER_COUNT for keeper topology,
# or implicitly 1 for replica topology (can be overridden with REPLICA_COUNT).
REPLICA_COUNT="${REPLICA_COUNT:-1}"

if [[ "$ROLE" == "solo" || "$ROLE" == "core" ]]; then
  write_core_config
fi
for i in $(seq 0 $((KEEPER_COUNT - 1))); do
  if [[ "$ROLE" == "keeper" ]]; then
    write_keeper_config "$i"
  fi
done
for i in $(seq 0 $((REPLICA_COUNT - 1))); do
  if [[ "$ROLE" == "read-replica" ]]; then
    write_replica_config "$i"
  fi
done

tune_kernel

step "Installing systemd services"
if [[ "$ROLE" == "solo" || "$ROLE" == "core" ]]; then
  write_core_systemd
fi
for i in $(seq 0 $((KEEPER_COUNT - 1))); do
  if [[ "$ROLE" == "keeper" ]]; then
    write_keeper_systemd "$i"
  fi
done
for i in $(seq 0 $((REPLICA_COUNT - 1))); do
  if [[ "$ROLE" == "read-replica" ]]; then
    write_replica_systemd "$i"
  fi
done

systemctl daemon-reload

step "Enabling and starting services"

# ── First-boot admin user creation ────────────────────────────────────────────
# Must be done BEFORE starting the service: mneme-core loads users.db at startup
# and does NOT hot-reload it. The adduser subcommand writes directly to users.db
# without starting the server, so we run it here while the service is still down.
if [[ "$ROLE" == "solo" || "$ROLE" == "core" ]]; then
  USERS_DB="${DATA_DIR}/users.db"
  if [[ ! -f "$USERS_DB" ]]; then
    info "Creating initial admin user (first-boot only)…"
    sudo -u "$MNEME_USER" \
      "${INSTALL_PREFIX}/bin/mneme-core" \
        --config "${CONFIG_DIR}/core.toml" adduser \
        --username admin \
        --password "${MNEME_ADMIN_PASSWORD}" \
        --role admin
    success "Admin user 'admin' created in ${USERS_DB}"
  else
    info "users.db already exists — skipping admin user creation."
  fi
fi

if [[ "$ROLE" == "solo" || "$ROLE" == "core" ]]; then
  systemctl enable --now mneme-core
  wait_for_port "127.0.0.1" "6379" "60" "mneme-core"
fi

for i in $(seq 0 $((KEEPER_COUNT - 1))); do
  if [[ "$ROLE" == "keeper" ]]; then
    systemctl enable --now "mneme-keeper-${i}"
    # Keeper's rep_port listener only opens AFTER the full Herold registration
    # handshake with Core (Core dials back to keeper). nc -z will time out.
    # Poll systemctl is-active instead — it becomes "active" once the process
    # is running and has not exited, which is sufficient to confirm a clean start.
    info "Waiting for mneme-keeper-${i} to become active (max 90s)…"
    _t=0
    until systemctl is-active --quiet "mneme-keeper-${i}"; do
      _t=$((_t + 1))
      if [[ "$_t" -ge 90 ]]; then
        fatal "mneme-keeper-${i} did not become active within 90s. Check: journalctl -u mneme-keeper-${i} -n 50"
      fi
      sleep 1
    done
    success "mneme-keeper-${i} is active."
  fi
done

for i in $(seq 0 $((REPLICA_COUNT - 1))); do
  if [[ "$ROLE" == "read-replica" ]]; then
    systemctl enable --now "mneme-replica-${i}"
    # Replica's client port opens only after sync with Core completes.
    # Poll systemctl is-active to confirm a clean start, then fall through.
    info "Waiting for mneme-replica-${i} to become active (max 90s)…"
    _t=0
    until systemctl is-active --quiet "mneme-replica-${i}"; do
      _t=$((_t + 1))
      if [[ "$_t" -ge 90 ]]; then
        fatal "mneme-replica-${i} did not become active within 90s. Check: journalctl -u mneme-replica-${i} -n 50"
      fi
      sleep 1
    done
    success "mneme-replica-${i} is active."
  fi
done

# ── Health check ──────────────────────────────────────────────────────────────
step "Health check"
sleep 2

if [[ "$ROLE" == "solo" || "$ROLE" == "core" ]]; then
  # Core listens on 6379 immediately — nc is reliable here.
  if nc -z 127.0.0.1 6379 2>/dev/null; then
    success "Port 6379 reachable — mneme-core is up."
  else
    warn "Port 6379 not yet reachable — check: journalctl -u mneme-core -n 50"
  fi
fi

if [[ "$ROLE" == "keeper" ]]; then
  for i in $(seq 0 $((KEEPER_COUNT - 1))); do
    if systemctl is-active --quiet "mneme-keeper-${i}"; then
      success "mneme-keeper-${i} is running (registered with Core via Herold)."
    else
      warn "mneme-keeper-${i} is not active — check: journalctl -u mneme-keeper-${i} -n 50"
    fi
  done
fi

if [[ "$ROLE" == "read-replica" ]]; then
  for i in $(seq 0 $((REPLICA_COUNT - 1))); do
    local_replica_port=$((6380 + i))
    if systemctl is-active --quiet "mneme-replica-${i}"; then
      success "mneme-replica-${i} is running (client port :${local_replica_port})."
    else
      warn "mneme-replica-${i} is not active — check: journalctl -u mneme-replica-${i} -n 50"
    fi
  done
fi

# ── Summary ───────────────────────────────────────────────────────────────────
# Detect best external IP for keeper join instructions (prefer non-loopback).
_PUBLIC_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
[[ -z "$_PUBLIC_IP" ]] && _PUBLIC_IP="$(hostname -f 2>/dev/null || echo '127.0.0.1')"

# Read cluster secret from env file (sourced earlier, but re-read defensively).
_CLUSTER_SECRET=""
if [[ -f "${CONFIG_DIR}/mneme.env" ]]; then
  _CLUSTER_SECRET="$(grep -E '^MNEME_CLUSTER_SECRET=' "${CONFIG_DIR}/mneme.env" \
                     | cut -d= -f2- | tr -d '"' || echo '')"
fi

echo ""
echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════${RESET}"
echo -e "${BOLD}  MnemeCache setup complete!${RESET}"
echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════${RESET}"
echo ""

if [[ "$ROLE" == "solo" || "$ROLE" == "core" ]]; then
  echo "  Client endpoint : ${_PUBLIC_IP}:6379 (TLS)"
  echo "  Metrics         : http://${_PUBLIC_IP}:9090/metrics"
  echo "  Secrets file    : ${CONFIG_DIR}/mneme.env  (chmod 600)"
  [[ "$ROLE" == "solo" ]] && echo "  Persistence     : WAL + snapshots in ${DATA_DIR}  (restart-safe)"
  echo ""
  echo "  Useful commands:"
  echo "    nc -z 127.0.0.1 6379 && echo OK   # port reachability check"
  echo "    mneme-cli --host localhost:6379 cluster-info"
  echo "    mneme-cli --host localhost:6379 keeper-list"
  echo "    journalctl -u mneme-core -f"
  echo "    systemctl status mneme-core"
fi

if [[ "$ROLE" == "keeper" ]]; then
  echo "  Keeper joined   : ${CORE_ADDR}"
  echo "  Metrics         : http://${_PUBLIC_IP}:9091/metrics"
  echo ""
  echo "  Useful commands:"
  echo "    journalctl -u mneme-keeper-0 -f"
  echo "    systemctl status mneme-keeper-0"
fi

if [[ "$ROLE" == "read-replica" ]]; then
  echo "  Replica endpoint: ${_PUBLIC_IP}:6380 (TLS, EVENTUAL reads only)"
  echo "  Primary sync    : ${CORE_ADDR}"
  echo "  Metrics         : http://${_PUBLIC_IP}:9095/metrics"
  echo ""
  echo "  Useful commands:"
  echo "    journalctl -u mneme-replica-0 -f"
  echo "    systemctl status mneme-replica-0"
fi

# ── Core/Solo: print all secrets and join bundle ──────────────────────────────
if [[ "$ROLE" == "core" || "$ROLE" == "solo" ]] && [[ -n "$_CLUSTER_SECRET" ]]; then

  # Re-read join token in case it was set on a different code path
  _JOIN_TOKEN="${MNEME_JOIN_TOKEN:-}"
  if [[ -z "$_JOIN_TOKEN" ]] && [[ -f "${CONFIG_DIR}/mneme.env" ]]; then
    _JOIN_TOKEN="$(grep -E '^MNEME_JOIN_TOKEN=' "${CONFIG_DIR}/mneme.env" \
                   | cut -d= -f2- | tr -d '"' || echo '')"
  fi

  # Read admin password
  _ADMIN_PASS="${MNEME_ADMIN_PASSWORD:-}"
  if [[ -z "$_ADMIN_PASS" ]] && [[ -f "${CONFIG_DIR}/mneme.env" ]]; then
    _ADMIN_PASS="$(grep -E '^MNEME_ADMIN_PASSWORD=' "${CONFIG_DIR}/mneme.env" \
                   | cut -d= -f2- | tr -d '"' || echo '')"
  fi

  # Build the join bundle: <base64-ca-pem-no-newlines>:<cluster-secret>:<join-token>
  _JOIN_BUNDLE=""
  if [[ -f "${DATA_DIR}/ca.crt" ]]; then
    _B64_CA="$(base64 -w 0 "${DATA_DIR}/ca.crt" 2>/dev/null || base64 "${DATA_DIR}/ca.crt" | tr -d '\n')"
    _JOIN_BUNDLE="${_B64_CA}:${_CLUSTER_SECRET}:${_JOIN_TOKEN}"
  fi

  echo ""
  echo -e "${BOLD}${YELLOW}╔══════════════════════════════════════════════════════════╗${RESET}"
  echo -e "${BOLD}${YELLOW}║  ⚠  SAVE THIS INFORMATION — SHOWN ONLY ONCE  ⚠           ║${RESET}"
  echo -e "${BOLD}${YELLOW}╚══════════════════════════════════════════════════════════╝${RESET}"
  echo ""
  echo -e "  ${BOLD}Admin username${RESET}   : admin"
  echo -e "  ${BOLD}Admin password${RESET}   : ${YELLOW}${_ADMIN_PASS}${RESET}"
  echo -e "  ${BOLD}Cluster secret${RESET}   : ${YELLOW}${_CLUSTER_SECRET}${RESET}"
  echo -e "  ${BOLD}Join token${RESET}       : ${YELLOW}${_JOIN_TOKEN}${RESET}"
  echo -e "  ${BOLD}Secrets file${RESET}     : ${CONFIG_DIR}/mneme.env  (chmod 600, root-only)"
  echo ""
  echo -e "  ${DIM}To see these again:  grep -v '^#' ${CONFIG_DIR}/mneme.env${RESET}"
  echo ""

  if [[ -n "$_JOIN_BUNDLE" && "$ROLE" == "core" ]]; then
    echo -e "${BOLD}${CYAN}╔══════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}${CYAN}║  How to add Keeper or Read-Replica nodes                 ║${RESET}"
    echo -e "${BOLD}${CYAN}╚══════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo -e "  ${BOLD}Core replication address${RESET}: ${_PUBLIC_IP}:7379"
    echo ""
    echo -e "  ${BOLD}JOIN BUNDLE${RESET} (copy the entire line below):"
    echo ""
    echo -e "  ${GREEN}${_JOIN_BUNDLE}${RESET}"
    echo ""
    echo -e "  ${DIM}The join bundle encodes the CA certificate + cluster secret + join token${RESET}"
    echo -e "  ${DIM}in a single copy-pasteable line. Keep it secret — it authorises nodes.${RESET}"
    echo ""
    echo -e "  ${BOLD}On each Keeper machine:${RESET}"
    echo ""
    echo -e "    ${CYAN}git clone <repo> mneme && cd mneme"
    echo -e "    sudo TOPOLOGY=keeper \\"
    echo -e "         KEEPER_COUNT=1 \\"
    echo -e "         CORE_ADDR=${_PUBLIC_IP}:7379 \\"
    echo -e "         ./scripts/setup-linux.sh${RESET}"
    echo ""
    echo -e "    ${DIM}The script will prompt: 'How do you want to provide credentials?'"
    echo -e "    Choose option 1 and paste the JOIN BUNDLE above.${RESET}"
    echo ""
    echo -e "  ${BOLD}On each Read-Replica machine:${RESET}"
    echo ""
    echo -e "    ${CYAN}sudo TOPOLOGY=replica \\"
    echo -e "         CORE_ADDR=${_PUBLIC_IP}:7379 \\"
    echo -e "         ./scripts/setup-linux.sh${RESET}"
    echo ""
    echo -e "    ${DIM}Same join bundle prompt — choose option 1.${RESET}"
    echo ""
    echo -e "  ${BOLD}Alternative (manual):${RESET} scp ${DATA_DIR}/ca.crt root@<joiner>:/tmp/ca.crt"
    echo -e "  ${DIM}and enter secret + token individually when the script prompts.${RESET}"
    echo -e "${BOLD}${CYAN}══════════════════════════════════════════════════════════${RESET}"
  fi
fi

echo ""
