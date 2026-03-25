#!/usr/bin/env bash
# setup-k8s.sh — Kubernetes deployment for MnemeCache.
# Supports three topologies:
#
#   solo    → Core StatefulSet only (1 replica, no Keeper StatefulSet)
#             RAM-only; pod restart = data loss
#             Good for: K8s dev/staging, latency benchmarks, sidecar caches
#
#   cluster → Core StatefulSet + Keeper StatefulSet (default: 3 replicas)
#             Full durability (WAL + snapshots on PVCs)
#             Good for: production, multi-region caching, compliance workloads
#
#   replica → Read-replica Deployment — EVENTUAL reads only, no PVCs
#             Syncs from an existing Core service in the cluster
#             Good for: read scale-out, analytics, DR standby
#
# Normally called from setup.sh (topology already selected).
# Can also be run directly.
#
# Usage (direct):
#   ./scripts/setup-k8s.sh
#   TOPOLOGY=solo    ./scripts/setup-k8s.sh
#   TOPOLOGY=cluster KEEPER_REPLICAS=5 ./scripts/setup-k8s.sh
#   TOPOLOGY=replica CORE_ADDR=mneme-core.mnemecache.svc.cluster.local:7379 \
#     ./scripts/setup-k8s.sh
#
# Environment overrides:
#   REGISTRY         Docker registry prefix, e.g. "docker.io/myorg"
#   IMAGE_TAG        Image tag (default: latest)
#   NAMESPACE        K8s namespace (default: mnemecache)
#   KEEPER_REPLICAS  Number of Keeper pods (default: 3 for cluster)
#   STORAGE_CLASS    StorageClass for PVCs (default: cluster default)
#   SKIP_BUILD       Set to 1 to skip docker build/push
#   SKIP_PUSH        Set to 1 to skip push (local registry / kind / k3d)
#   DRY_RUN          Set to 1 to print kubectl commands without applying

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

# ── Config ────────────────────────────────────────────────────────────────────
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
REGISTRY="${REGISTRY:-}"
IMAGE_TAG="${IMAGE_TAG:-latest}"
IMAGE_NAME="${IMAGE_NAME:-mnemecache}"
NAMESPACE="${NAMESPACE:-mnemecache}"
SKIP_BUILD="${SKIP_BUILD:-0}"
SKIP_PUSH="${SKIP_PUSH:-0}"
KEEPER_REPLICAS="${KEEPER_REPLICAS:-${KEEPER_COUNT:-3}}"
STORAGE_CLASS="${STORAGE_CLASS:-}"
DRY_RUN="${DRY_RUN:-0}"
K8S_DIR="${REPO_ROOT}/k8s"

# kubectl wrapper that respects DRY_RUN
kube() {
  if [[ "$DRY_RUN" == "1" ]]; then
    echo "[DRY-RUN] kubectl $*"
  else
    kubectl "$@"
  fi
}

# ── Topology selection (when run directly) ────────────────────────────────────
if [[ -z "${TOPOLOGY:-}" ]]; then
  banner
  step "Deployment topology"
  echo ""
  echo "  ┌──────────────────────────────────────────────────────────────────────┐"
  echo "  │  1) Solo node     Core StatefulSet only (1 pod, no Keepers)         │"
  echo "  │                   RAM-only; pod restart = data loss                  │"
  echo "  │                   Good for: dev, staging, sidecar caches             │"
  echo "  │                                                                      │"
  echo "  │  2) Full cluster  Core StatefulSet + Keeper StatefulSet              │"
  echo "  │                   WAL + snapshots on PVCs; survives pod restarts     │"
  echo "  │                   Good for: production, compliance, durability       │"
  echo "  │                                                                      │"
  echo "  │  3) Read replica  Deployment syncing from existing Core service      │"
  echo "  │                   EVENTUAL reads only; no PVCs; horizontal scaling   │"
  echo "  │                   Good for: read scale-out, analytics, DR standby   │"
  echo "  └──────────────────────────────────────────────────────────────────────┘"
  echo ""
  while true; do
    read -rp "$(echo -e "${BOLD}Your choice [1/2/3]: ${RESET}")" _TC
    case "$_TC" in
      1) TOPOLOGY="solo";    break ;;
      2) TOPOLOGY="cluster"; break ;;
      3) TOPOLOGY="replica"; break ;;
      *) warn "Please enter 1, 2, or 3." ;;
    esac
  done
  echo ""
fi

CORE_ADDR="${CORE_ADDR:-}"

case "${TOPOLOGY:-cluster}" in
  solo)    KEEPER_REPLICAS="0" ;;
  cluster)
    if [[ "$KEEPER_REPLICAS" == "3" ]]; then
      ask KEEPER_REPLICAS "Number of Keeper pods" "3"
    fi
    ;;
  replica)
    KEEPER_REPLICAS="0"
    if [[ -z "$CORE_ADDR" ]]; then
      ask CORE_ADDR \
        "Primary Core service address (e.g. mneme-core.mnemecache.svc.cluster.local:7379)" \
        "mneme-core.${NAMESPACE}.svc.cluster.local:7379"
    fi
    ;;
  *)
    fatal "K8s topology must be 'solo', 'cluster', or 'replica'. Got: ${TOPOLOGY}"
    ;;
esac

# ── Pre-flight ────────────────────────────────────────────────────────────────
banner 2>/dev/null || true
step "Pre-flight checks"

require_cmd kubectl "Install kubectl: https://kubernetes.io/docs/tasks/tools/"
require_cmd docker  "Install Docker:  https://docs.docker.com/engine/install/"

if ! kubectl cluster-info &>/dev/null; then
  fatal "kubectl cannot reach a cluster. Check your KUBECONFIG / current context."
fi

CONTEXT="$(kubectl config current-context 2>/dev/null || echo 'unknown')"
CLUSTER="$(kubectl config view --minify \
  -o jsonpath='{.clusters[0].name}' 2>/dev/null || echo 'unknown')"
info "kubectl context : ${CONTEXT}"
info "Cluster name    : ${CLUSTER}"
info "Topology        : ${TOPOLOGY}  (Keeper replicas: ${KEEPER_REPLICAS})"
[[ -n "$CORE_ADDR" ]] && info "Primary Core    : ${CORE_ADDR}"

# Warn on production-looking context names
if echo "$CLUSTER $CONTEXT" | grep -qi -E 'prod|production|live'; then
  warn "This looks like a PRODUCTION cluster!"
  ask_yn CONFIRM "Are you sure you want to deploy here?" "n"
  [[ "$CONFIRM" == "n" ]] && fatal "Aborted."
fi

# Node OS check
NODES_OS="$(kubectl get nodes \
  -o jsonpath='{.items[*].status.nodeInfo.operatingSystem}' 2>/dev/null || echo '')"
if echo "$NODES_OS" | grep -qv linux; then
  warn "Non-Linux nodes detected. MnemeCache requires Linux 5.19+ on worker nodes."
fi

success "Pre-flight checks passed."

# ── Image ─────────────────────────────────────────────────────────────────────
step "Docker image"

if [[ -n "$REGISTRY" ]]; then
  FULL_IMAGE="${REGISTRY}/${IMAGE_NAME}:${IMAGE_TAG}"
else
  ask REGISTRY \
    "Docker registry (e.g. docker.io/myorg, ghcr.io/myorg — empty = no push)" ""
  if [[ -n "$REGISTRY" ]]; then
    FULL_IMAGE="${REGISTRY}/${IMAGE_NAME}:${IMAGE_TAG}"
  else
    FULL_IMAGE="${IMAGE_NAME}:${IMAGE_TAG}"
    warn "No registry — image will not be pushed."
    warn "Nodes must have '${FULL_IMAGE}' available locally (e.g. kind load or k3d)."
    SKIP_PUSH="1"
  fi
fi
info "Image: ${FULL_IMAGE}"

if [[ "$SKIP_BUILD" == "1" ]]; then
  info "SKIP_BUILD=1 — skipping docker build."
else
  step "Building Docker image"
  info "Building ${FULL_IMAGE} …"
  docker build --target core -t "$FULL_IMAGE" "${REPO_ROOT}"
  success "Image built: ${FULL_IMAGE}"
fi

if [[ "$SKIP_PUSH" != "1" ]]; then
  step "Pushing image to registry"
  docker push "$FULL_IMAGE"
  success "Image pushed: ${FULL_IMAGE}"
fi

# ── Secrets ───────────────────────────────────────────────────────────────────
step "K8s secrets"

if kubectl get secret mneme-auth -n "$NAMESPACE" &>/dev/null 2>&1; then
  info "Secret 'mneme-auth' already exists in '${NAMESPACE}' — skipping."
  info "To rotate: kubectl delete secret mneme-auth -n ${NAMESPACE}"
else
  CLUSTER_SECRET="$(gen_secret)"
  ADMIN_PASSWORD="$(gen_password)"

  info "Creating namespace (if needed)…"
  kubectl apply -f "${K8S_DIR}/namespace.yaml" || true

  info "Creating secret 'mneme-auth'…"
  kube create secret generic mneme-auth \
    --namespace "$NAMESPACE" \
    --from-literal=cluster_secret="${CLUSTER_SECRET}" \
    --from-literal=admin_password="${ADMIN_PASSWORD}"

  success "Secret 'mneme-auth' created."
  echo ""
  warn "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  warn "  Save these — they cannot be recovered from K8s."
  warn "  Cluster secret : ${CLUSTER_SECRET}"
  warn "  Admin password : ${ADMIN_PASSWORD}"
  warn "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  echo ""
fi

# ── Patch manifests ────────────────────────────────────────────────────────────
step "Preparing manifests"

PATCH_DIR="$(mktemp -d)"
trap 'rm -rf "$PATCH_DIR"' EXIT

# Determine which manifests to apply for this topology
case "$TOPOLOGY" in
  solo)
    MANIFESTS_TO_APPLY=("${K8S_DIR}/namespace.yaml" "${K8S_DIR}/core.yaml")
    ;;
  cluster)
    MANIFESTS_TO_APPLY=("${K8S_DIR}/namespace.yaml" "${K8S_DIR}/core.yaml" "${K8S_DIR}/keeper.yaml")
    ;;
  replica)
    MANIFESTS_TO_APPLY=("${K8S_DIR}/namespace.yaml" "${K8S_DIR}/replica.yaml")
    ;;
esac

for f in "${MANIFESTS_TO_APPLY[@]}"; do
  [[ ! -f "$f" ]] && fatal "Manifest not found: ${f}"
  name="$(basename "$f")"
  dest="${PATCH_DIR}/${name}"
  cp "$f" "$dest"

  if [[ "$name" == "core.yaml" || "$name" == "keeper.yaml" || "$name" == "replica.yaml" ]]; then
    sed -i.bak "s|image: mnemelabs/core:1.0.0|image: ${FULL_IMAGE}|g" "$dest"
    rm -f "${dest}.bak"
    info "Patched image in ${name}: ${FULL_IMAGE}"
  fi

  if [[ "$name" == "keeper.yaml" ]]; then
    sed -i.bak "s|replicas: 3|replicas: ${KEEPER_REPLICAS}|g" "$dest"
    rm -f "${dest}.bak"
    info "Patched keeper replicas: ${KEEPER_REPLICAS}"
  fi

  if [[ "$name" == "replica.yaml" && -n "$CORE_ADDR" ]]; then
    sed -i.bak "s|primary_addr: .*|primary_addr: \"${CORE_ADDR}\"|g" "$dest"
    rm -f "${dest}.bak"
    info "Patched replica primary_addr: ${CORE_ADDR}"
  fi

  if [[ -n "$STORAGE_CLASS" ]]; then
    sed -i.bak \
      "s|# storageClassName: local-path|storageClassName: ${STORAGE_CLASS}|g" \
      "$dest"
    rm -f "${dest}.bak"
    info "Patched storageClassName: ${STORAGE_CLASS}"
  fi
done

# ── Print topology diagram ─────────────────────────────────────────────────────
echo ""
case "$TOPOLOGY" in
  solo)
    echo "  Deploying:"
    echo "  ┌──────────────────────────────────────────────────────────────────┐"
    printf "  │  namespace: %-54s│\n" "${NAMESPACE}"
    echo "  │  mneme-core  (StatefulSet, 1 pod)                               │"
    echo "  │    :6379 → NodePort 30379  (client TLS)                         │"
    echo "  │    :9090 → ClusterIP       (Prometheus metrics)                  │"
    echo "  │  No PVCs — data lives in pod RAM only.                           │"
    echo "  └──────────────────────────────────────────────────────────────────┘"
    ;;
  cluster)
    echo "  Deploying:"
    echo "  ┌──────────────────────────────────────────────────────────────────┐"
    printf "  │  namespace: %-54s│\n" "${NAMESPACE}"
    echo "  │  mneme-core    (StatefulSet, 1 pod)                             │"
    echo "  │    :6379 → NodePort 30379  (client TLS)                         │"
    echo "  │    :7379 → ClusterIP       (replication mTLS)                   │"
    echo "  │    :9090 → ClusterIP       (metrics)                            │"
    printf "  │  mneme-keeper  (StatefulSet, %-3s pods)                         │\n" \
      "${KEEPER_REPLICAS}"
    echo "  │    WAL + snapshots on PVCs                                      │"
    echo "  │    :7380+ → ClusterIP      (replication mTLS)                   │"
    echo "  │    :9091+ → ClusterIP      (metrics)                            │"
    echo "  └──────────────────────────────────────────────────────────────────┘"
    ;;
  replica)
    echo "  Deploying:"
    echo "  ┌──────────────────────────────────────────────────────────────────┐"
    printf "  │  namespace: %-54s│\n" "${NAMESPACE}"
    echo "  │  mneme-replica (Deployment, 1+ pods)                            │"
    echo "  │    :6380 → NodePort 30380  (client TLS, EVENTUAL reads only)    │"
    echo "  │    :9095 → ClusterIP       (metrics)                            │"
    echo "  │  No PVCs — syncs from primary Core on pod start.                │"
    printf "  │  Primary Core: %-52s│\n" "${CORE_ADDR}"
    echo "  └──────────────────────────────────────────────────────────────────┘"
    ;;
esac
echo ""

# ── Apply manifests ────────────────────────────────────────────────────────────
step "Applying manifests"

info "Applying namespace…"
kube apply -f "${PATCH_DIR}/namespace.yaml"

info "Skipping secrets.yaml (secret already created via kubectl create secret)."

if [[ "$TOPOLOGY" == "replica" ]]; then
  info "Applying read-replica Deployment…"
  kube apply -f "${PATCH_DIR}/replica.yaml"

  if [[ "$DRY_RUN" != "1" ]]; then
    step "Waiting for replica rollout"
    kubectl rollout status deployment/mneme-replica \
      -n "$NAMESPACE" --timeout=180s
    success "Read replica is running."
  fi
else
  info "Applying Core StatefulSet…"
  kube apply -f "${PATCH_DIR}/core.yaml"

  if [[ "$DRY_RUN" != "1" ]]; then
    step "Waiting for Core rollout"
    kubectl rollout status statefulset/mneme-core \
      -n "$NAMESPACE" --timeout=180s
    success "Core is running."
  fi

  if [[ "$TOPOLOGY" == "cluster" ]]; then
    info "Applying Keeper StatefulSet (${KEEPER_REPLICAS} replicas)…"
    kube apply -f "${PATCH_DIR}/keeper.yaml"

    if [[ "$DRY_RUN" != "1" ]]; then
      step "Waiting for Keeper rollout"
      kubectl rollout status statefulset/mneme-keeper \
        -n "$NAMESPACE" --timeout=300s
      success "Keepers are running."
    fi
  fi
fi

# ── Wait for WarmupState::Hot (cluster only) ───────────────────────────────────
if [[ "$DRY_RUN" != "1" && "$TOPOLOGY" == "cluster" ]]; then
  step "Waiting for WarmupState::Hot"
  TIMEOUT=180
  ELAPSED=0
  info "Polling Core logs for '\"warmup_state\":\"Hot\"'…"
  while true; do
    if kubectl logs statefulset/mneme-core -n "$NAMESPACE" --tail=50 2>/dev/null \
        | grep -q '"warmup_state":"Hot"'; then
      success "Cluster is Hot — all Keepers synced."
      break
    fi
    ELAPSED=$((ELAPSED + 3))
    if [[ "$ELAPSED" -ge "$TIMEOUT" ]]; then
      warn "Did not see WarmupState::Hot within ${TIMEOUT}s."
      warn "Cluster may still be Warming — check:"
      warn "  kubectl logs -n ${NAMESPACE} mneme-core-0 -f"
      break
    fi
    sleep 3
    printf '.'
  done
  echo ""
fi

# ── Health verification ────────────────────────────────────────────────────────
if [[ "$DRY_RUN" != "1" ]]; then
  step "Health verification"

  info "Pod status:"
  kubectl get pods -n "$NAMESPACE" -o wide
  echo ""

  if [[ "$TOPOLOGY" == "replica" ]]; then
    REPLICA_POD="$(kubectl get pods -n "$NAMESPACE" \
      -l app=mneme-replica -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo '')"
    if [[ -n "$REPLICA_POD" ]]; then
      info "Ping via mneme-cli inside replica pod (${REPLICA_POD})…"
      kubectl exec -n "$NAMESPACE" "$REPLICA_POD" -- \
        mneme-cli --host 127.0.0.1:6380 ping 2>/dev/null \
        && success "ping → PONG  (replica)" \
        || warn "ping failed — replica may still be syncing."
    else
      warn "No replica pods found yet."
    fi
  else
    info "Ping via mneme-cli inside Core pod…"
    kubectl exec -n "$NAMESPACE" mneme-core-0 -- \
      mneme-cli --host 127.0.0.1:6379 ping 2>/dev/null \
      && success "ping → PONG" \
      || warn "ping failed — Core may still be starting."

    if [[ "$TOPOLOGY" == "cluster" ]]; then
      info "Keeper list:"
      kubectl exec -n "$NAMESPACE" mneme-core-0 -- \
        mneme-cli --host 127.0.0.1:6379 keeper-list 2>/dev/null || true
      info "Cluster info:"
      kubectl exec -n "$NAMESPACE" mneme-core-0 -- \
        mneme-cli --host 127.0.0.1:6379 cluster-info 2>/dev/null || true
    fi
  fi
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════════${RESET}"

case "$TOPOLOGY" in
  solo)
    echo -e "${BOLD}  MnemeCache solo node deployed on Kubernetes!${RESET}"
    echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════════${RESET}"
    echo ""
    echo "  Namespace  : ${NAMESPACE}"
    echo "  Image      : ${FULL_IMAGE}"
    echo "  Mode       : Solo (Core only, RAM)"
    echo ""
    echo "  Internal  : mneme-core.${NAMESPACE}.svc.cluster.local:6379"
    echo "  External  : <any-node-ip>:30379  (NodePort)"
    echo ""
    echo "  Quick commands:"
    echo "    kubectl get pods -n ${NAMESPACE}"
    echo "    kubectl logs -n ${NAMESPACE} mneme-core-0 -f"
    echo "    kubectl exec -n ${NAMESPACE} mneme-core-0 -- mneme-cli ping"
    echo "    kubectl exec -n ${NAMESPACE} mneme-core-0 -- mneme-cli set foo bar"
    echo "    kubectl exec -n ${NAMESPACE} mneme-core-0 -- mneme-cli get foo"
    echo ""
    echo "  Upgrade to cluster topology:"
    echo "    TOPOLOGY=cluster SKIP_BUILD=1 ./scripts/setup-k8s.sh"
    echo ""
    echo "  Tear down:"
    echo "    kubectl delete -f ${K8S_DIR}/core.yaml"
    echo "    kubectl delete namespace ${NAMESPACE}"
    ;;

  cluster)
    echo -e "${BOLD}  MnemeCache cluster deployed on Kubernetes!${RESET}"
    echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════════${RESET}"
    echo ""
    echo "  Namespace  : ${NAMESPACE}"
    echo "  Image      : ${FULL_IMAGE}"
    echo "  Keepers    : ${KEEPER_REPLICAS} pods (WAL + snapshots on PVCs)"
    echo ""
    echo "  Internal  : mneme-core.${NAMESPACE}.svc.cluster.local:6379"
    echo "  External  : <any-node-ip>:30379  (NodePort)"
    echo ""
    echo "  Quick commands:"
    echo "    kubectl get pods -n ${NAMESPACE}"
    echo "    kubectl logs -n ${NAMESPACE} mneme-core-0 -f"
    echo "    kubectl exec -n ${NAMESPACE} mneme-core-0 -- mneme-cli ping"
    echo "    kubectl exec -n ${NAMESPACE} mneme-core-0 -- mneme-cli cluster-info"
    echo "    kubectl exec -n ${NAMESPACE} mneme-core-0 -- mneme-cli keeper-list"
    echo ""
    echo "  Scale Keepers:"
    echo "    kubectl scale statefulset/mneme-keeper -n ${NAMESPACE} --replicas=5"
    echo ""
    echo "  Add a read replica:"
    echo "    TOPOLOGY=replica CORE_ADDR=mneme-core.${NAMESPACE}.svc.cluster.local:7379 \\"
    echo "      SKIP_BUILD=1 SKIP_PUSH=1 ./scripts/setup-k8s.sh"
    echo ""
    echo "  Rolling image upgrade:"
    echo "    kubectl set image statefulset/mneme-core \\"
    echo "      mneme-core=${FULL_IMAGE%:*}:NEW_TAG -n ${NAMESPACE}"
    echo "    kubectl set image statefulset/mneme-keeper \\"
    echo "      mneme-keeper=${FULL_IMAGE%:*}:NEW_TAG -n ${NAMESPACE}"
    echo ""
    echo "  Tear down:"
    echo "    kubectl delete -f ${K8S_DIR}/keeper.yaml"
    echo "    kubectl delete -f ${K8S_DIR}/core.yaml"
    echo "    kubectl delete namespace ${NAMESPACE}   # also removes PVCs!"
    ;;

  replica)
    echo -e "${BOLD}  MnemeCache read replica deployed on Kubernetes!${RESET}"
    echo -e "${BOLD}${GREEN}══════════════════════════════════════════════════════${RESET}"
    echo ""
    echo "  Namespace   : ${NAMESPACE}"
    echo "  Image       : ${FULL_IMAGE}"
    echo "  Mode        : Read replica (EVENTUAL reads only)"
    echo "  Primary     : ${CORE_ADDR}"
    echo ""
    echo "  Internal  : mneme-replica.${NAMESPACE}.svc.cluster.local:6380"
    echo "  External  : <any-node-ip>:30380  (NodePort)"
    echo ""
    echo "  Quick commands:"
    echo "    kubectl get pods -n ${NAMESPACE} -l app=mneme-replica"
    echo "    kubectl logs -n ${NAMESPACE} -l app=mneme-replica -f"
    echo "    kubectl exec -n ${NAMESPACE} deploy/mneme-replica -- \\"
    echo "      mneme-cli --host 127.0.0.1:6380 ping"
    echo "    kubectl exec -n ${NAMESPACE} deploy/mneme-replica -- \\"
    echo "      mneme-cli --host 127.0.0.1:6380 get mykey"
    echo ""
    echo "  Scale replicas (horizontal read scale-out):"
    echo "    kubectl scale deployment/mneme-replica -n ${NAMESPACE} --replicas=3"
    echo ""
    echo "  Tear down:"
    echo "    kubectl delete -f ${K8S_DIR}/replica.yaml"
    echo ""
    echo "  Note: Writes must always go to the primary Core:"
    echo "    ${CORE_ADDR%:*}:6379"
    ;;
esac

echo ""
