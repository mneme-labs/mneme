#!/usr/bin/env bash
# setup.sh — MnemeCache universal installer.
#
# Guides you through two decisions:
#   1. Topology  — solo node, full cluster, or distributed (core / keeper)
#   2. Platform  — native binaries, Docker Compose, or Kubernetes
# Then delegates to the right platform-specific script with all config exported.
#
# ─── Quick start ──────────────────────────────────────────────────────────────
#   ./scripts/setup.sh                    # interactive wizard
#   curl -fsSL <url>/scripts/setup.sh | bash
#
# ─── Non-interactive / CI flags ───────────────────────────────────────────────
#   Topology  : --solo | --cluster | --core | --keeper
#   Platform  : --native | --docker | --k8s
#   Options   : --keepers N            number of Keeper nodes (default: 3)
#               --core-addr HOST:PORT   Core address (required for --keeper)
#               --dry-run              print commands, make no changes
#
# ─── Examples ─────────────────────────────────────────────────────────────────
#   ./scripts/setup.sh --solo --docker
#   ./scripts/setup.sh --cluster --native --keepers 5
#   ./scripts/setup.sh --cluster --k8s
#   ./scripts/setup.sh --keeper --native --core-addr 10.0.0.1:7379
#   TOPOLOGY=cluster TARGET=docker KEEPER_COUNT=3 ./scripts/setup.sh

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

# ── Flag parsing ───────────────────────────────────────────────────────────────
TOPOLOGY="${TOPOLOGY:-}"
TARGET="${TARGET:-}"
KEEPER_COUNT="${KEEPER_COUNT:-}"
CORE_ADDR="${CORE_ADDR:-}"
DRY_RUN="${DRY_RUN:-0}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --solo)              TOPOLOGY="solo"    ;;
    --cluster)           TOPOLOGY="cluster" ;;
    --core)              TOPOLOGY="core"    ;;
    --keeper)            TOPOLOGY="keeper"  ;;
    --replica)           TOPOLOGY="replica" ;;
    --ha)                TOPOLOGY="ha"      ;;
    --native)            TARGET="native"    ;;
    --docker)            TARGET="docker"    ;;
    --k8s|--kubernetes)  TARGET="k8s"       ;;
    --keepers)           KEEPER_COUNT="$2"; shift ;;
    --core-addr)         CORE_ADDR="$2";    shift ;;
    --dry-run)           DRY_RUN="1"        ;;
    --help|-h)
      grep '^#' "$0" | sed 's/^# \?//' | head -30
      exit 0
      ;;
    *)
      warn "Unknown flag: $1 (ignored)"
      ;;
  esac
  shift
done

# ── OS detection ───────────────────────────────────────────────────────────────
detect_os() {
  case "$(uname -s)" in
    Linux*)               echo "linux"   ;;
    Darwin*)              echo "macos"   ;;
    CYGWIN*|MINGW*|MSYS*) echo "windows" ;;
    *)                    echo "unknown" ;;
  esac
}
OS="$(detect_os)"

# ── Windows / macOS guard ─────────────────────────────────────────────────────
# Native build requires Linux 5.19+ (io_uring, O_DIRECT, perf_event_open).
# On Windows and macOS, Docker is the only supported runtime.
if [[ "$OS" == "windows" || "$OS" == "macos" ]]; then
  banner
  if [[ "$OS" == "windows" ]]; then
    echo -e "  ${BOLD}Windows detected.${RESET}"
  else
    echo -e "  ${BOLD}macOS detected.${RESET}"
  fi
  echo ""
  echo -e "  ${YELLOW}Native build is not supported on ${OS}.${RESET}"
  echo "  MnemeCache requires Linux 5.19+ for io_uring, O_DIRECT, and"
  echo "  perf_event_open.  Use Docker instead — it works on any OS."
  echo ""
  echo -e "  ${BOLD}Docker is the supported path on ${OS}:${RESET}"
  echo "    1. Install Docker Desktop → https://docs.docker.com/desktop/"
  echo "    2. Start Docker Desktop"
  echo "    3. Run:  ./scripts/setup.sh --docker"
  echo ""
  echo "  If you are on Windows and want full Linux features (io_uring, huge"
  echo "  pages, perf counters), use WSL2 and run setup.sh inside it."
  echo ""
  # If --docker was already passed on the command line, fall through instead
  # of exiting — the dispatch block below will pick it up.
  if [[ "$TARGET" != "docker" && "$TARGET" != "k8s" ]]; then
    exit 0
  fi
fi

banner
echo -e "  ${BOLD}OS detected : ${BLUE}${OS}${RESET}"
[[ "$DRY_RUN" == "1" ]] && echo -e "  ${YELLOW}[DRY-RUN] No changes will be made${RESET}"
echo ""

# ── Step 1: Topology ──────────────────────────────────────────────────────────
select_topology() {
  echo -e "  ${BOLD}Step 1 of 2 — What are you setting up?${RESET}"
  echo ""
  echo "  ┌──────────────────────────────────────────────────────────────────────┐"
  echo "  │  1) Solo node     Core only, no Keepers                             │"
  echo "  │                   Best for: dev, testing, servers under 32 GB RAM   │"
  echo "  │                                                                      │"
  echo "  │  2) Full cluster  Core + N Keeper nodes on THIS machine              │"
  echo "  │                   Best for: single-server production, durability     │"
  echo "  │                                                                      │"
  echo "  │  3) Core node     Distributed — this machine is the primary Core    │"
  echo "  │                   Best for: dedicated Core, Keepers on other hosts   │"
  echo "  │                                                                      │"
  echo "  │  4) Keeper node   Distributed — join an existing Core               │"
  echo "  │                   Best for: adding persistence to a cluster          │"
  echo "  │                                                                      │"
  echo "  │  5) Read replica  Distributed — EVENTUAL-read-only Core copy        │"
  echo "  │                   Best for: read scale-out, analytics, DR standby   │"
  echo "  │                                                                      │"
  echo "  │  6) HA cluster    3 Core Raft cluster + Keepers — automatic failover│"
  echo "  │                   Best for: production HA, leader election           │"
  echo "  └──────────────────────────────────────────────────────────────────────┘"
  echo ""
  while true; do
    read -rp "$(echo -e "  ${BOLD}Your choice [1/2/3/4/5/6]: ${RESET}")" CHOICE
    case "$CHOICE" in
      1) TOPOLOGY="solo";    break ;;
      2) TOPOLOGY="cluster"; break ;;
      3) TOPOLOGY="core";    break ;;
      4) TOPOLOGY="keeper";  break ;;
      5) TOPOLOGY="replica"; break ;;
      6) TOPOLOGY="ha";      break ;;
      *) warn "  Please enter 1, 2, 3, 4, 5, or 6." ;;
    esac
  done
  echo ""
}

[[ -z "$TOPOLOGY" ]] && select_topology

# ── Topology → ROLE / KEEPER_COUNT / CORE_ADDR ────────────────────────────────
case "$TOPOLOGY" in
  solo)
    ROLE="core"
    KEEPER_COUNT="0"
    info "Solo: Core only. No Keepers — data lives in RAM only."
    ;;

  cluster)
    ROLE="both"
    if [[ -z "$KEEPER_COUNT" ]]; then
      ask KEEPER_COUNT "Number of Keeper nodes on this machine" "3"
    fi
    info "Full cluster: Core + ${KEEPER_COUNT} Keeper(s) on this machine."
    ;;

  core)
    ROLE="core"
    KEEPER_COUNT="0"
    info "Core node: starts Core only."
    info "Add Keepers later: ./scripts/setup.sh --keeper --core-addr <this-ip>:7379"
    ;;

  keeper)
    ROLE="keeper"
    if [[ -z "$KEEPER_COUNT" ]]; then
      ask KEEPER_COUNT "Number of Keeper instances on this machine" "1"
    fi
    if [[ -z "$CORE_ADDR" ]]; then
      ask CORE_ADDR "Core replication address (host:port)" ""
    fi
    info "Keeper: ${KEEPER_COUNT} Keeper(s) connecting to Core at ${CORE_ADDR}."
    ;;

  replica)
    ROLE="read-replica"
    KEEPER_COUNT="0"
    if [[ -z "$CORE_ADDR" ]]; then
      ask CORE_ADDR "Primary Core address to sync from (host:port)" ""
    fi
    info "Read replica: EVENTUAL reads only, syncing from Core at ${CORE_ADDR}."
    info "Clients connecting here get read-only EVENTUAL consistency."
    ;;

  ha)
    ROLE="ha"
    KEEPER_COUNT="2"
    info "HA cluster: 3 Raft Core nodes + ${KEEPER_COUNT} Keeper(s)."
    info "Automatic leader election and failover if a Core goes down."
    ;;

  *)
    fatal "Unknown topology '${TOPOLOGY}'. Use: solo | cluster | core | keeper | replica | ha"
    ;;
esac

export TOPOLOGY ROLE KEEPER_COUNT CORE_ADDR DRY_RUN

# ── Step 2: Deployment target ─────────────────────────────────────────────────
select_target() {
  echo ""
  echo -e "  ${BOLD}Step 2 of 2 — How do you want to deploy?${RESET}"
  echo ""
  echo "  ┌──────────────────────────────────────────────────────────────────────┐"
  echo "  │  1) Native binary   Build and run on ${OS} directly                │"
  echo "  │                     Full Linux features (io_uring, huge pages, perf) │"
  echo "  │                                                                      │"
  echo "  │  2) Docker Compose  Containerised setup (any OS, needs Docker ≥ 24)  │"
  echo "  │                                                                      │"
  echo "  │  3) Kubernetes      Production K8s deployment (needs kubectl)        │"
  echo "  │                                                                      │"
  echo "  │  q) Quit                                                             │"
  echo "  └──────────────────────────────────────────────────────────────────────┘"
  echo ""
  while true; do
    read -rp "$(echo -e "  ${BOLD}Your choice [1/2/3/q]: ${RESET}")" CHOICE
    case "$CHOICE" in
      1)
        TARGET="native"
        break
        ;;
      2)
        if ! command -v docker &>/dev/null; then
          error "Docker not found."
          echo "  Install Docker Desktop : https://docs.docker.com/desktop/"
          [[ "$OS" == "linux" ]] && \
            echo "  Or engine only        : curl -fsSL https://get.docker.com | sh"
          continue
        fi
        TARGET="docker"
        break
        ;;
      3)
        if ! command -v kubectl &>/dev/null; then
          error "kubectl not found."
          echo "  Install : https://kubernetes.io/docs/tasks/tools/"
          continue
        fi
        TARGET="k8s"
        break
        ;;
      q|Q|quit|exit)
        info "Goodbye."
        exit 0
        ;;
      *)
        warn "  Please enter 1, 2, 3, or q."
        ;;
    esac
  done
}

[[ -z "$TARGET" ]] && select_target

# ── Summary ────────────────────────────────────────────────────────────────────
echo ""
echo -e "  ${BOLD}Configuration${RESET}"
echo "  ─────────────────────────────────────────────────"
printf "  %-10s %s\n" "Topology:"  "$TOPOLOGY"
printf "  %-10s %s\n" "Role:"      "$ROLE"
printf "  %-10s %s\n" "Keepers:"   "$KEEPER_COUNT"
[[ -n "$CORE_ADDR" ]] && printf "  %-10s %s\n" "Core addr:" "$CORE_ADDR"
printf "  %-10s %s\n" "Platform:"  "$TARGET"
printf "  %-10s %s\n" "OS:"        "$OS"
[[ "$DRY_RUN" == "1" ]] && printf "  %-10s %s\n" "Dry-run:" "yes"
echo "  ─────────────────────────────────────────────────"
echo ""

# ── Dispatch ──────────────────────────────────────────────────────────────────
case "$TARGET" in
  native)
    if [[ "$OS" != "linux" ]]; then
      warn "Native build is only supported on Linux."
      info "Switching to Docker (the supported path on ${OS})…"
      echo ""
      exec bash "${SCRIPT_DIR}/setup-docker.sh"
    fi
    exec bash "${SCRIPT_DIR}/setup-linux.sh"
    ;;
  docker)
    exec bash "${SCRIPT_DIR}/setup-docker.sh"
    ;;
  k8s)
    exec bash "${SCRIPT_DIR}/setup-k8s.sh"
    ;;
  *)
    fatal "Unknown target '${TARGET}'. Use: native | docker | k8s"
    ;;
esac
