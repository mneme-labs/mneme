#!/usr/bin/env bash
# common.sh — Shared utilities sourced by every MnemeCache setup script.
#
# Source at the top of every setup script:
#   source "$(dirname "$0")/common.sh"

set -euo pipefail

MNEME_VERSION="${MNEME_VERSION:-1.0.0-dev}"

# ── Terminal colour detection ──────────────────────────────────────────────────
# Only enable colours when:
#   • stdout is a real terminal (not piped/redirected)
#   • tput is available
#   • the terminal supports at least 8 colours
if [[ -t 1 ]] && command -v tput &>/dev/null \
              && tput colors &>/dev/null \
              && [[ "$(tput colors)" -ge 8 ]]; then
  COLOUR=1
  RED='\033[0;31m'
  GREEN='\033[0;32m'
  YELLOW='\033[1;33m'
  BLUE='\033[0;34m'
  CYAN='\033[0;36m'
  MAGENTA='\033[0;35m'
  DIM='\033[2m'
  BOLD='\033[1m'
  RESET='\033[0m'
else
  COLOUR=0
  RED='' GREEN='' YELLOW='' BLUE='' CYAN='' MAGENTA='' DIM='' BOLD='' RESET=''
fi

# ── Timestamp helper ───────────────────────────────────────────────────────────
_ts() { date '+%H:%M:%S'; }

# ── Logging ───────────────────────────────────────────────────────────────────
# Each function prints:  HH:MM:SS [LEVEL]  message
#
#   info    → blue    [INFO]   general progress
#   success → green   [OK]     step completed successfully
#   warn    → yellow  [WARN]   non-fatal issue; setup continues
#   error   → red     [ERROR]  printed to stderr; caller decides whether to fatal
#   fatal   → red     [FATAL]  prints to stderr and exits 1
#   step    → bold    ━━━ …    section header (printed to stdout)
#   detail  → dim             indented supplementary info (no level tag)

info()    { echo -e "${BLUE}$(_ts) [INFO]${RESET}   $*"; }
success() { echo -e "${GREEN}$(_ts) [OK]${RESET}     $*"; }
warn()    { echo -e "${YELLOW}$(_ts) [WARN]${RESET}   $*"; }
error()   { echo -e "${RED}$(_ts) [ERROR]${RESET}  $*" >&2; }
fatal()   { echo -e "${RED}$(_ts) [FATAL]${RESET}  $*" >&2; exit 1; }
detail()  { echo -e "${DIM}         ↳  $*${RESET}"; }

step() {
  echo ""
  echo -e "${BOLD}${BLUE}  ┌─────────────────────────────────────────────────────┐${RESET}"
  printf "${BOLD}${BLUE}  │  %-51s│${RESET}\n" "$*"
  echo -e "${BOLD}${BLUE}  └─────────────────────────────────────────────────────┘${RESET}"
}

# ── Banner ────────────────────────────────────────────────────────────────────
# ASCII-art banner shown at the start of every setup script.
# Degrades to plain text when colours are unavailable.
banner() {
  echo -e "${BOLD}${BLUE}"
  echo "  ╔════════════════════════════════════════════════════════╗"
  echo "  ║                                                        ║"
  echo "  ║   ███╗   ███╗███╗   ██╗███████╗███╗   ███╗███████╗     ║"
  echo "  ║   ████╗ ████║████╗  ██║██╔════╝████╗ ████║██╔════╝     ║"
  echo "  ║   ██╔████╔██║██╔██╗ ██║█████╗  ██╔████╔██║█████╗       ║"
  echo "  ║   ██║╚██╔╝██║██║╚██╗██║██╔══╝  ██║╚██╔╝██║██╔══╝       ║"
  echo "  ║   ██║ ╚═╝ ██║██║ ╚████║███████╗██║ ╚═╝ ██║███████╗     ║"
  echo "  ║   ╚═╝     ╚═╝╚═╝  ╚═══╝╚══════╝╚═╝     ╚═╝╚══════╝     ║"
  echo "  ║               ·  C  A  C  H  E  ·                      ║"
  echo "  ╠════════════════════════════════════════════════════════╣"
  echo "  ║  Distributed in-memory cache  ·  Rust 2021             ║"
  printf "║  Version  : %-43s║\n" "${MNEME_VERSION}"
  printf "║  Platform : %-43s║\n" "$(uname -s)/$(uname -m)"
  echo "  ║  Docs     : https://github.com/mneme-labs/mneme     ║"
  echo "  ╚════════════════════════════════════════════════════════╝"
  echo -e "${RESET}"
}

# ── Requirement checks ────────────────────────────────────────────────────────
require_cmd() {
  local cmd="$1"
  local hint="${2:-}"
  if ! command -v "$cmd" &>/dev/null; then
    if [[ -n "$hint" ]]; then
      fatal "'${cmd}' not found.  ${hint}"
    else
      fatal "'${cmd}' not found.  Please install it and re-run this script."
    fi
  fi
  detail "Found: ${cmd} → $(command -v "$cmd")"
}

require_root() {
  if [[ "$EUID" -ne 0 ]]; then
    fatal "This step requires root.  Re-run with: sudo $0"
  fi
}

# ── Interactive prompts ────────────────────────────────────────────────────────
# ask VAR "prompt text" [default]
ask() {
  local varname="$1"
  local prompt="$2"
  local default="${3:-}"
  local answer
  if [[ -n "$default" ]]; then
    read -rp "$(echo -e "${BOLD}  ${prompt}${RESET} [${DIM}${default}${RESET}]: ")" answer
    answer="${answer:-$default}"
  else
    read -rp "$(echo -e "${BOLD}  ${prompt}${RESET}: ")" answer
    while [[ -z "$answer" ]]; do
      warn "  Value cannot be empty."
      read -rp "$(echo -e "${BOLD}  ${prompt}${RESET}: ")" answer
    done
  fi
  printf -v "$varname" '%s' "$answer"
}

# ask_secret VAR "prompt"  — hides input (for passwords/tokens)
ask_secret() {
  local varname="$1"
  local prompt="$2"
  local answer
  read -rsp "$(echo -e "${BOLD}  ${prompt}${RESET}: ")" answer
  echo
  while [[ -z "$answer" ]]; do
    warn "  Value cannot be empty."
    read -rsp "$(echo -e "${BOLD}  ${prompt}${RESET}: ")" answer
    echo
  done
  printf -v "$varname" '%s' "$answer"
}

# ask_yn VAR "prompt" [y|n]  — sets VAR to "y" or "n"
ask_yn() {
  local varname="$1"
  local prompt="$2"
  local default="${3:-n}"
  local answer
  local yn_hint="[y/N]"
  [[ "$default" == "y" ]] && yn_hint="[Y/n]"
  read -rp "$(echo -e "${BOLD}  ${prompt}${RESET} ${DIM}${yn_hint}${RESET}: ")" answer
  answer="${answer:-$default}"
  answer="${answer,,}"
  if [[ "$answer" == "y" || "$answer" == "yes" ]]; then
    printf -v "$varname" 'y'
  else
    printf -v "$varname" 'n'
  fi
}

# ── Secret generation ──────────────────────────────────────────────────────────
# gen_secret  → 32 bytes of CSPRNG, base64-encoded (no padding, no newlines)
gen_secret() {
  openssl rand -base64 32 | tr -d '\n='
}

# gen_password → 16 bytes of CSPRNG, base64-encoded
gen_password() {
  openssl rand -base64 16 | tr -d '\n='
}

# ── Version comparison ─────────────────────────────────────────────────────────
# version_gte "1.2.3" "1.1.0"  → returns 0 (true) if first ≥ second
version_gte() {
  local a="$1" b="$2"
  printf '%s\n%s\n' "$b" "$a" | sort -V -C
}

# ── Network helpers ────────────────────────────────────────────────────────────
# wait_for_port HOST PORT TIMEOUT_SECONDS LABEL
wait_for_port() {
  local host="$1" port="$2" timeout="${3:-30}" label="${4:-service}"
  info "Waiting for ${label} at ${host}:${port}  (timeout ${timeout}s)…"
  local i=0
  while ! nc -z "$host" "$port" 2>/dev/null; do
    i=$((i + 1))
    if [[ "$i" -ge "$timeout" ]]; then
      fatal "${label} did not become reachable within ${timeout}s."
    fi
    sleep 1
    printf '.'
  done
  echo ""
  success "${label} is reachable at ${host}:${port}"
}

# wait_for_cmd TIMEOUT_SECONDS LABEL CMD [ARGS...]
wait_for_cmd() {
  local timeout="$1"; shift
  local label="$1";   shift
  info "Waiting for: ${label}  (timeout ${timeout}s)…"
  local i=0
  until "$@" &>/dev/null; do
    i=$((i + 1))
    if [[ "$i" -ge "$timeout" ]]; then
      fatal "'${label}' did not succeed within ${timeout}s."
    fi
    sleep 1
  done
  success "${label} succeeded."
}

# ── Rust toolchain ─────────────────────────────────────────────────────────────
ensure_rust() {
  if command -v rustup &>/dev/null; then
    local ver
    ver="$(rustc --version 2>/dev/null || echo 'unknown')"
    info "Rust toolchain found: ${ver}"
    rustup update stable --no-self-update &>/dev/null || true
    detail "Toolchain updated to latest stable."
    return 0
  fi
  step "Installing Rust via rustup"
  info "Downloading rustup installer from https://sh.rustup.rs …"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --no-modify-path
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
  success "Rust installed: $(rustc --version)"
}

# ── Directory helpers ──────────────────────────────────────────────────────────
# ensure_dir PATH [OWNER:GROUP] [MODE]
ensure_dir() {
  local dir="$1" owner="${2:-}" mode="${3:-755}"
  if [[ ! -d "$dir" ]]; then
    mkdir -p "$dir"
    info "Created directory: ${dir}"
  fi
  chmod "$mode" "$dir"
  if [[ -n "$owner" ]]; then
    chown "$owner" "$dir"
    detail "Ownership: ${owner}  mode: ${mode}"
  fi
}

# write_file PATH MODE [OWNER]  — reads content from stdin
write_file() {
  local path="$1" mode="${2:-644}" owner="${3:-}"
  cat > "$path"
  chmod "$mode" "$path"
  [[ -n "$owner" ]] && chown "$owner" "$path"
  detail "Wrote: ${path}  (mode ${mode})"
}

# ── Health check ───────────────────────────────────────────────────────────────
mneme_ping() {
  local host="${1:-localhost}" port="${2:-6379}"
  if command -v mneme-cli &>/dev/null; then
    mneme-cli --host "${host}:${port}" ping &>/dev/null
  else
    nc -z "$host" "$port" 2>/dev/null
  fi
}

# ── Build helper ───────────────────────────────────────────────────────────────
# build_mneme REPO_ROOT [EXTRA_RUSTFLAGS]
build_mneme() {
  local repo_root="$1"
  local extra="${2:-}"
  step "Building MnemeCache from source"
  info "This takes a few minutes on first run (compiling all dependencies)."
  detail "Repo: ${repo_root}"
  pushd "$repo_root" >/dev/null
  RUSTFLAGS="-C target-cpu=native ${extra}" \
    cargo build --release --workspace --exclude mneme-bench
  popd >/dev/null
  success "Build complete."
}
