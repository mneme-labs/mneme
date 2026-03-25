#!/usr/bin/env bash
# setup-docker.sh — Docker Compose setup for MnemeCache.
# Supports four topologies:
#
#   solo     → single mneme-core container, no Keepers
#              Data lives in RAM only; container restart = data loss
#              Good for: development, integration testing, latency benchmarks
#
#   cluster  → mneme-core + N mneme-keeper containers on this host
#              Full durability (WAL + snapshots); default N = 3
#              Good for: single-server production, staging environments
#
#   keeper   → Keeper-only containers connecting to an external Core
#              Core runs elsewhere (another host / K8s service)
#              Good for: extending an existing distributed cluster
#
#   replica  → read-only Core container syncing from an external primary Core
#              Serves EVENTUAL reads only; no writes; no WAL or snapshots
#              Good for: read scale-out, analytics, DR standby
#
# Normally called from setup.sh (topology already selected).
# Can also be run directly.
#
# Usage (direct):
#   ./scripts/setup-docker.sh
#   TOPOLOGY=solo    ./scripts/setup-docker.sh
#   TOPOLOGY=cluster KEEPER_COUNT=5 ./scripts/setup-docker.sh
#   TOPOLOGY=keeper  CORE_ADDR=10.0.0.1:7379 ./scripts/setup-docker.sh
#   TOPOLOGY=replica CORE_ADDR=10.0.0.1:7379 ./scripts/setup-docker.sh
#
# Environment overrides:
#   MNEME_IMAGE     Full image reference (default: mnemelabs/core:0.1.0)
#   SKIP_BUILD      Set to 1 to skip docker build
#   KEEPER_COUNT    Number of Keeper containers (default: 3 for cluster)
#   CORE_ADDR       External Core address for keeper topology (host:port)
#   DRY_RUN         Set to 1 to print commands without running them

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

# ── Config ────────────────────────────────────────────────────────────────────
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MNEME_IMAGE="${MNEME_IMAGE:-mnemelabs/core:0.1.0}"
SKIP_BUILD="${SKIP_BUILD:-0}"
KEEPER_COUNT="${KEEPER_COUNT:-3}"
CORE_ADDR="${CORE_ADDR:-}"
DRY_RUN="${DRY_RUN:-0}"
ENV_FILE="${REPO_ROOT}/.env"
COMPOSE_FILE="${REPO_ROOT}/docker-compose.yml"

# ── Topology selection (when run directly) ────────────────────────────────────
if [[ -z "${TOPOLOGY:-}" ]]; then
  banner
  step "Deployment topology"
  echo ""
  echo "  ┌──────────────────────────────────────────────────────────────────────┐"
  echo "  │  1) Solo node     Core container only, no Keepers                   │"
  echo "  │                   RAM-only; fast startup; data lost on restart       │"
  echo "  │                                                                      │"
  echo "  │  2) Full cluster  Core + N Keeper containers on this host            │"
  echo "  │                   WAL + snapshots; survives container restarts        │"
  echo "  │                                                                      │"
  echo "  │  3) Keeper only   Keeper containers → external Core host              │"
  echo "  │                   Extend an existing cluster with Docker Keepers      │"
  echo "  │                                                                      │"
  echo "  │  4) Read replica  Read-only Core container syncing from primary      │"
  echo "  │                   EVENTUAL reads only; no WAL; great for read scale  │"
  echo "  └──────────────────────────────────────────────────────────────────────┘"
  echo ""
  while true; do
    read -rp "$(echo -e "${BOLD}Your choice [1/2/3/4]: ${RESET}")" _TC
    case "$_TC" in
      1) TOPOLOGY="solo";    break ;;
      2) TOPOLOGY="cluster"; break ;;
      3) TOPOLOGY="keeper";  break ;;
      4) TOPOLOGY="replica"; break ;;
      *) warn "Please enter 1, 2, 3, or 4." ;;
    esac
  done
  echo ""
fi

# Resolve ROLE from topology
case "${TOPOLOGY:-cluster}" in
  solo)    ROLE="core";         KEEPER_COUNT="0" ;;
  cluster) ROLE="both" ;;
  core)    ROLE="core";         KEEPER_COUNT="0" ;;
  keeper)  ROLE="keeper" ;;
  replica) ROLE="read-replica"; KEEPER_COUNT="0" ;;
  ha)      ROLE="ha";           KEEPER_COUNT="2" ;;
  *)       fatal "Unknown topology '${TOPOLOGY}'. Use: solo | cluster | keeper | replica | ha" ;;
esac

# Topology-specific follow-up questions
if [[ "$TOPOLOGY" == "cluster" && "$KEEPER_COUNT" == "3" ]]; then
  ask KEEPER_COUNT "Number of Keeper containers" "3"
fi

if [[ "$TOPOLOGY" == "keeper" && -z "$CORE_ADDR" ]]; then
  ask CORE_ADDR "External Core replication address (host:port)" ""
fi

if [[ "$TOPOLOGY" == "replica" && -z "$CORE_ADDR" ]]; then
  ask CORE_ADDR "Primary Core address to sync from (host:port)" ""
fi

# ── Pre-flight ────────────────────────────────────────────────────────────────
banner 2>/dev/null || true
step "Pre-flight checks"

require_cmd docker "Install Docker Desktop: https://docs.docker.com/desktop/"

if ! docker info &>/dev/null; then
  if [[ "$(uname)" == "Darwin" ]]; then
    fatal "Docker daemon not running. Start Docker Desktop and try again."
  else
    fatal "Docker daemon not running. Try: sudo systemctl start docker"
  fi
fi

if ! docker compose version &>/dev/null; then
  fatal "Docker Compose v2 not found. Install Docker Desktop or the compose plugin."
fi

DOCKER_VERSION="$(docker version --format '{{.Server.Version}}' 2>/dev/null || echo '0.0')"
COMPOSE_VERSION="$(docker compose version --short 2>/dev/null || echo '0.0')"
info "Docker         : ${DOCKER_VERSION}"
info "Docker Compose : ${COMPOSE_VERSION}"
info "Topology       : ${TOPOLOGY}  (ROLE=${ROLE}, KEEPERS=${KEEPER_COUNT})"
[[ -n "$CORE_ADDR" ]] && info "Primary Core   : ${CORE_ADDR}"

success "Docker environment OK."

# ── Secrets ───────────────────────────────────────────────────────────────────
step "Configuring secrets"

if [[ -f "$ENV_FILE" ]]; then
  info "Found existing ${ENV_FILE} — reusing secrets."
  # shellcheck source=/dev/null
  source "$ENV_FILE"
else
  MNEME_CLUSTER_SECRET="$(gen_secret)"
  MNEME_ADMIN_PASSWORD="$(gen_password)"

  cat > "$ENV_FILE" <<EOF
# MnemeCache Docker secrets — generated $(date -u +%Y-%m-%dT%H:%M:%SZ)
# Read by docker-compose.yml via the env_file key.
# DO NOT commit to version control.
MNEME_CLUSTER_SECRET=${MNEME_CLUSTER_SECRET}
MNEME_ADMIN_PASSWORD=${MNEME_ADMIN_PASSWORD}
EOF
  chmod 600 "$ENV_FILE"

  GITIGNORE="${REPO_ROOT}/.gitignore"
  if [[ -f "$GITIGNORE" ]] && ! grep -q '\.env$' "$GITIGNORE"; then
    echo '.env' >> "$GITIGNORE"
    info "Added .env to .gitignore"
  fi

  success "Secrets written to ${ENV_FILE}"
  echo ""
  warn "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  warn "  Save these — they will NOT be shown again."
  warn "  Cluster secret : ${MNEME_CLUSTER_SECRET}"
  warn "  Admin password : ${MNEME_ADMIN_PASSWORD}"
  warn "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  echo ""
fi

# ── Build image ───────────────────────────────────────────────────────────────
if [[ "$SKIP_BUILD" == "1" ]]; then
  step "Skipping Docker build (SKIP_BUILD=1)"
  if ! docker image inspect "$MNEME_IMAGE" &>/dev/null; then
    fatal "Image '${MNEME_IMAGE}' not found locally. Build it or set SKIP_BUILD=0."
  fi
  info "Using existing image: ${MNEME_IMAGE}"
else
  step "Building Docker images"
  info "Building mnemelabs/core:0.1.0 …"
  docker build --target core   -t mnemelabs/core:0.1.0   "${REPO_ROOT}"
  info "Building mnemelabs/keeper:0.1.0 …"
  docker build --target keeper -t mnemelabs/keeper:0.1.0 "${REPO_ROOT}"
  info "Building mnemelabs/cli:0.1.0 …"
  docker build --target cli    -t mnemelabs/cli:0.1.0    "${REPO_ROOT}"
  success "All images built."
fi

export MNEME_IMAGE KEEPER_COUNT CORE_ADDR

# ── Stop any existing cluster ──────────────────────────────────────────────────
step "Stopping any existing MnemeCache containers"
cd "${REPO_ROOT}"
docker compose down --remove-orphans 2>/dev/null || true
info "Previous containers cleared."

# ── Generate topology-specific compose override ────────────────────────────────
# When topology=keeper, disable the core service and wire keepers to CORE_ADDR.
# When topology=replica, disable the core service and add a read-replica service.
OVERRIDE_FILE=""
if [[ "$TOPOLOGY" == "keeper" ]]; then
  OVERRIDE_FILE="$(mktemp /tmp/mneme-compose-keeper.XXXXXX.yml)"
  trap 'rm -f "$OVERRIDE_FILE"' EXIT

  {
    echo "# Auto-generated keeper-only overlay — external Core: ${CORE_ADDR}"
    echo "services:"
    echo "  mneme-core:"
    echo "    profiles: [disabled]"
    for i in $(seq 1 "$KEEPER_COUNT"); do
      REP_PORT=$((7379 + i))
      MET_PORT=$((9090 + i))
      echo "  mneme-keeper-${i}:"
      echo "    image: \${MNEME_IMAGE:-mnemelabs/core:0.1.0}"
      echo "    restart: unless-stopped"
      echo "    environment:"
      echo "      MNEME_CLUSTER_SECRET: \${MNEME_CLUSTER_SECRET}"
      echo "      MNEME_CORE_ADDR: ${CORE_ADDR}"
      echo "      MNEME_KEEPER_ID: hypnos-docker-${i}"
      echo "      MNEME_REP_PORT: \"${REP_PORT}\""
      echo "      MNEME_METRICS_PORT: \"${MET_PORT}\""
      echo "    ports:"
      echo "      - \"${MET_PORT}:${MET_PORT}\""
      echo "    volumes:"
      echo "      - mneme-keeper-${i}-data:/var/lib/mneme/keeper-${i}"
    done
    echo "volumes:"
    for i in $(seq 1 "$KEEPER_COUNT"); do
      echo "  mneme-keeper-${i}-data:"
    done
  } > "$OVERRIDE_FILE"

  info "Keeper-only override written."

elif [[ "$TOPOLOGY" == "replica" ]]; then
  OVERRIDE_FILE="$(mktemp /tmp/mneme-compose-replica.XXXXXX.yml)"
  trap 'rm -f "$OVERRIDE_FILE"' EXIT

  {
    echo "# Auto-generated read-replica overlay — primary Core: ${CORE_ADDR}"
    echo "services:"
    echo "  mneme-core:"
    echo "    profiles: [disabled]"
    echo "  mneme-replica-0:"
    echo "    image: \${MNEME_IMAGE:-mnemelabs/core:0.1.0}"
    echo "    restart: unless-stopped"
    echo "    environment:"
    echo "      MNEME_CLUSTER_SECRET: \${MNEME_CLUSTER_SECRET}"
    echo "      MNEME_ROLE: read-replica"
    echo "      MNEME_PRIMARY_ADDR: ${CORE_ADDR}"
    echo "      MNEME_NODE_ID: mneme-replica-docker-0"
    echo "      MNEME_PORT: \"6380\""
    echo "      MNEME_METRICS_PORT: \"9095\""
    echo "    ports:"
    echo "      - \"6380:6380\""
    echo "      - \"9095:9095\""
  } > "$OVERRIDE_FILE"

  info "Read-replica override written."
fi

# ── Start services ────────────────────────────────────────────────────────────
step "Starting MnemeCache (${TOPOLOGY})"

BASE_COMPOSE_ARGS=(--env-file "$ENV_FILE")
[[ -n "$OVERRIDE_FILE" ]] && BASE_COMPOSE_ARGS+=(-f "$COMPOSE_FILE" -f "$OVERRIDE_FILE")

case "$TOPOLOGY" in
  solo)
    echo ""
    echo "  Architecture:"
    echo "  ┌───────────────────────────────────────────────────┐"
    echo "  │  mneme-core                                        │"
    echo "  │    :6379 → client connections (TLS)               │"
    echo "  │    :9090 → Prometheus metrics                      │"
    echo "  │  RAM-only — no WAL, no snapshots.                  │"
    echo "  │  Data is lost when the container stops.            │"
    echo "  └───────────────────────────────────────────────────┘"
    echo ""
    docker compose "${BASE_COMPOSE_ARGS[@]}" up -d mneme-core
    ;;

  cluster)
    echo ""
    echo "  Architecture:"
    echo "  ┌──────────────────────────────────────────────────────────────────┐"
    echo "  │  mneme-core      :6379 (client TLS)     :9090 (metrics)         │"
    echo "  │  mneme-keeper-1  :7380 (replication)    :9091 (metrics) + WAL   │"
    [[ "$KEEPER_COUNT" -ge 2 ]] && \
    echo "  │  mneme-keeper-2  :7381 (replication)    :9092 (metrics) + WAL   │"
    [[ "$KEEPER_COUNT" -ge 3 ]] && \
    echo "  │  mneme-keeper-3  :7382 (replication)    :9093 (metrics) + WAL   │"
    echo "  │                                                                  │"
    echo "  │  Keepers persist to named Docker volumes (WAL + snapshots).      │"
    echo "  │  Quorum reads require ⌊N/2⌋+1 Keeper ACKs.                      │"
    echo "  └──────────────────────────────────────────────────────────────────┘"
    echo ""
    docker compose "${BASE_COMPOSE_ARGS[@]}" up -d
    ;;

  keeper)
    echo ""
    echo "  Architecture:"
    echo "  ┌────────────────────────────────────────────────────────────────┐"
    echo "  │  External Core : ${CORE_ADDR}"
    for i in $(seq 1 "$KEEPER_COUNT"); do
      MET_PORT=$((9090 + i))
      printf "  │  mneme-keeper-%-2s → Core   :%-5s (metrics)\n" \
        "${i}" "${MET_PORT}"
    done
    echo "  └────────────────────────────────────────────────────────────────┘"
    echo ""
    docker compose "${BASE_COMPOSE_ARGS[@]}" up -d
    ;;

  replica)
    echo ""
    echo "  Architecture:"
    echo "  ┌────────────────────────────────────────────────────────────────┐"
    echo "  │  Primary Core  : ${CORE_ADDR}  (read from)"
    echo "  │  mneme-replica-0                                               │"
    echo "  │    :6380 → client connections (TLS, EVENTUAL reads only)       │"
    echo "  │    :9095 → Prometheus metrics                                  │"
    echo "  │  No WAL, no snapshots.  Restart = re-sync from primary.        │"
    echo "  └────────────────────────────────────────────────────────────────┘"
    echo ""
    docker compose "${BASE_COMPOSE_ARGS[@]}" up -d
    ;;

  ha)
    echo ""
    echo "  Architecture:"
    echo "  ┌──────────────────────────────────────────────────────────────────┐"
    echo "  │  3-node Raft Core cluster with automatic leader election:       │"
    echo "  │  mneme-core-1  :6379 (client)  :7379 (Raft mTLS)  :9090 (met) │"
    echo "  │  mneme-core-2  :6382 (client)  :7379 (Raft mTLS)  :9097 (met) │"
    echo "  │  mneme-core-3  :6383 (client)  :7379 (Raft mTLS)  :9098 (met) │"
    echo "  │  mneme-ha-keeper-1  WAL + snapshots                            │"
    echo "  │  mneme-ha-keeper-2  WAL + snapshots                            │"
    echo "  │                                                                  │"
    echo "  │  If a Core dies, Raft re-elects a new leader within ~3s.       │"
    echo "  │  Writes go to leader; reads served from any Core.              │"
    echo "  └──────────────────────────────────────────────────────────────────┘"
    echo ""
    docker compose "${BASE_COMPOSE_ARGS[@]}" --profile ha up -d
    ;;
esac

# ── Wait for service health ────────────────────────────────────────────────────
if [[ "$TOPOLOGY" != "keeper" ]]; then
  if [[ "$TOPOLOGY" == "replica" ]]; then
    WAIT_SVC="mneme-replica-0"
  else
    WAIT_SVC="mneme-core"
  fi
  step "Waiting for ${WAIT_SVC} to become healthy"
  TIMEOUT=120
  ELAPSED=0
  until docker compose ps "$WAIT_SVC" 2>/dev/null | grep -q "healthy"; do
    ELAPSED=$((ELAPSED + 2))
    if [[ "$ELAPSED" -ge "$TIMEOUT" ]]; then
      error "${WAIT_SVC} did not become healthy within ${TIMEOUT}s."
      echo ""
      error "Recent logs:"
      docker compose logs --tail=40 "$WAIT_SVC"
      fatal "Aborting."
    fi
    sleep 2
    printf '.'
  done
  echo ""
  success "${WAIT_SVC} is healthy."

  if [[ "$TOPOLOGY" == "cluster" ]]; then
    info "Waiting for Keepers to register with Core…"
    sleep 6
  fi
fi

# ── Smoke tests ───────────────────────────────────────────────────────────────
if [[ "$TOPOLOGY" != "keeper" && "$TOPOLOGY" != "replica" ]]; then
  step "Smoke tests"
  ask_yn RUN_SMOKE "Run smoke test suite?" "y"
  if [[ "$RUN_SMOKE" == "y" ]]; then
    docker compose --env-file "$ENV_FILE" --profile test run --rm smoke-test \
      && success "Smoke tests passed." \
      || warn "Smoke tests failed — see output above."
  else
    info "Skipping smoke tests."
  fi
fi

# ── Verification ───────────────────────────────────────────────────────────────
if [[ "$TOPOLOGY" != "keeper" ]]; then
  step "Verifying service"

  if [[ "$TOPOLOGY" == "replica" ]]; then
    docker compose exec mneme-replica-0 \
      mneme-cli --host 127.0.0.1:6380 ping 2>/dev/null \
      && success "ping → PONG  (replica)" \
      || warn "ping failed — check: docker compose logs mneme-replica-0"
  else
    docker compose exec mneme-core \
      mneme-cli --host 127.0.0.1:6379 ping 2>/dev/null \
      && success "ping → PONG" \
      || warn "ping failed — check: docker compose logs mneme-core"

    if [[ "$TOPOLOGY" == "cluster" ]]; then
      docker compose exec mneme-core \
        mneme-cli --host 127.0.0.1:6379 keeper-list 2>/dev/null \
        || warn "keeper-list — Keepers may still be syncing."
      docker compose exec mneme-core \
        mneme-cli --host 127.0.0.1:6379 cluster-info 2>/dev/null \
        || true
    fi
  fi
fi

# ── Host IP ────────────────────────────────────────────────────────────────────
HOST_IP="127.0.0.1"
if [[ "$(uname)" == "Linux" ]] && ! command -v docker-desktop &>/dev/null; then
  HOST_IP="$(hostname -I | awk '{print $1}')"
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════════${RESET}"

case "$TOPOLOGY" in
  solo)
    echo -e "${BOLD}  MnemeCache solo node is running! (Docker)${RESET}"
    echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════════${RESET}"
    echo ""
    echo "  Client   : ${HOST_IP}:6379  (TLS)"
    echo "  Metrics  : http://${HOST_IP}:9090/metrics"
    echo "  Secrets  : ${ENV_FILE}  (chmod 600)"
    echo ""
    echo "  Quick commands:"
    echo "    docker compose exec mneme-core mneme-cli ping"
    echo "    docker compose exec mneme-core mneme-cli set mykey hello"
    echo "    docker compose exec mneme-core mneme-cli get mykey"
    echo "    docker compose logs -f mneme-core"
    echo "    docker compose down          # stop (keeps volumes)"
    echo "    docker compose down -v       # stop + DELETE all data"
    echo ""
    echo "  Upgrade to full cluster:"
    echo "    TOPOLOGY=cluster SKIP_BUILD=1 ./scripts/setup-docker.sh"
    ;;

  cluster)
    echo -e "${BOLD}  MnemeCache cluster is running! (Docker)${RESET}"
    echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════════${RESET}"
    echo ""
    echo "  Containers:"
    docker compose ps --format "table {{.Name}}\t{{.Status}}\t{{.Ports}}" \
      2>/dev/null || docker compose ps
    echo ""
    echo "  Endpoints (from host):"
    echo "    Client     : ${HOST_IP}:6379  (TLS)"
    echo "    Metrics C  : http://${HOST_IP}:9090/metrics"
    echo "    Metrics K1 : http://${HOST_IP}:9091/metrics"
    [[ "$KEEPER_COUNT" -ge 2 ]] && \
    echo "    Metrics K2 : http://${HOST_IP}:9092/metrics"
    [[ "$KEEPER_COUNT" -ge 3 ]] && \
    echo "    Metrics K3 : http://${HOST_IP}:9093/metrics"
    echo ""
    echo "  Secrets  : ${ENV_FILE}  (chmod 600)"
    echo ""
    echo "  Quick commands:"
    echo "    docker compose exec mneme-core mneme-cli ping"
    echo "    docker compose exec mneme-core mneme-cli cluster-info"
    echo "    docker compose exec mneme-core mneme-cli keeper-list"
    echo "    docker compose logs -f mneme-core"
    echo "    docker compose logs -f mneme-keeper-1"
    echo "    docker compose down          # stop (keeps volumes)"
    echo "    docker compose down -v       # stop + DELETE all data"
    echo ""
    echo "  Scale Keepers:"
    echo "    KEEPER_COUNT=5 SKIP_BUILD=1 TOPOLOGY=cluster ./scripts/setup-docker.sh"
    ;;

  keeper)
    echo -e "${BOLD}  MnemeCache Keeper(s) running! (Docker)${RESET}"
    echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════════${RESET}"
    echo ""
    echo "  Registered with Core : ${CORE_ADDR}"
    echo "  Containers:"
    docker compose ps --format "table {{.Name}}\t{{.Status}}\t{{.Ports}}" \
      2>/dev/null || docker compose ps
    echo ""
    echo "  Quick commands:"
    echo "    docker compose logs -f mneme-keeper-1"
    echo "    docker compose down"
    ;;

  replica)
    echo -e "${BOLD}  MnemeCache read replica is running! (Docker)${RESET}"
    echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════════${RESET}"
    echo ""
    echo "  Mode            : Read replica (EVENTUAL reads only)"
    echo "  Syncing from    : ${CORE_ADDR}"
    echo "  Client          : ${HOST_IP}:6380  (TLS)"
    echo "  Metrics         : http://${HOST_IP}:9095/metrics"
    echo "  Secrets         : ${ENV_FILE}  (chmod 600)"
    echo ""
    echo "  Quick commands:"
    echo "    docker compose exec mneme-replica-0 mneme-cli --host 127.0.0.1:6380 ping"
    echo "    docker compose exec mneme-replica-0 mneme-cli --host 127.0.0.1:6380 get mykey"
    echo "    docker compose logs -f mneme-replica-0"
    echo "    docker compose down"
    echo ""
    echo "  Note: This replica serves EVENTUAL reads. Writes must go to:"
    echo "    ${CORE_ADDR}"
    ;;
esac

echo ""
