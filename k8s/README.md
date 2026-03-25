# MnemeCache — Kubernetes Manifests

Raw YAML manifests for deploying MnemeCache on any Kubernetes cluster.

## Files

| File | Purpose |
|------|---------|
| `namespace.yaml` | `mnemecache` namespace |
| `secrets.yaml` | `mneme-auth` Secret (cluster_secret, admin_password) |
| `core.yaml` | Core node StatefulSet + Services + ConfigMap |
| `keeper.yaml` | Keeper StatefulSet (3 replicas) + PDB + headless Service |
| `replica.yaml` | Read replica StatefulSet (2 replicas) + Services |
| `solo.yaml` | Solo mode — single pod with embedded keeper + persistence |
| `monitoring.yaml` | Prometheus ServiceMonitor + PrometheusRule alerts |
| `full-cluster.yaml` | All-in-one: Core + 3 Keepers + 2 Replicas + Monitoring |

## Deployment Modes

### 1. Solo Mode (development / single-node)

A single pod running mneme-core with an embedded keeper. Persistence is enabled
with WAL and snapshots stored on a 10 Gi PVC. Good for development, CI, or
small single-node deployments.

```bash
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/secrets.yaml
kubectl apply -f k8s/solo.yaml

kubectl rollout status statefulset/mneme-solo -n mnemecache

# Connect via NodePort
mneme-cli --host <NODE_IP>:30379 --ca-cert /etc/mneme/ca.crt -u admin -p <PASSWORD> stats
```

### 2. Core + Keepers (standard production)

One Core node (Raft leader, pure RAM) and three Keeper nodes (WAL + cold store).
QUORUM writes require at least 2 Keeper ACKs.

```bash
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/secrets.yaml
kubectl apply -f k8s/core.yaml
kubectl apply -f k8s/keeper.yaml
kubectl apply -f k8s/monitoring.yaml   # only if kube-prometheus-stack is installed

kubectl rollout status statefulset/mneme-core -n mnemecache
kubectl rollout status statefulset/mneme-keeper -n mnemecache

# Connect
kubectl port-forward svc/mneme-core 6379:6379 -n mnemecache
mneme-cli --host localhost:6379 --ca-cert /etc/mneme/ca.crt -u admin -p <PASSWORD> stats
```

### 3. Core + Keepers + Read Replicas (read-heavy workloads)

Same as mode 2, plus two read replicas that serve EVENTUAL-consistency reads on
port 6380. Clients send writes to `mneme-core:6379` and reads to
`mneme-replica-read:6380`.

```bash
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/secrets.yaml
kubectl apply -f k8s/core.yaml
kubectl apply -f k8s/keeper.yaml
kubectl apply -f k8s/replica.yaml
kubectl apply -f k8s/monitoring.yaml

kubectl rollout status statefulset/mneme-core -n mnemecache
kubectl rollout status statefulset/mneme-keeper -n mnemecache
kubectl rollout status statefulset/mneme-replica -n mnemecache

# Writes
kubectl port-forward svc/mneme-core 6379:6379 -n mnemecache
# Reads (EVENTUAL)
kubectl port-forward svc/mneme-replica-read 6380:6380 -n mnemecache
```

### 4. Full Cluster (all-in-one)

Single file deploying everything: namespace, secrets, Core (x1), Keepers (x3),
Replicas (x2), and monitoring. Convenient for bootstrapping a complete cluster.

```bash
# Edit secrets in full-cluster.yaml first!
kubectl apply -f k8s/full-cluster.yaml

kubectl rollout status statefulset/mneme-core -n mnemecache
kubectl rollout status statefulset/mneme-keeper -n mnemecache
kubectl rollout status statefulset/mneme-replica -n mnemecache

# Connect
kubectl port-forward svc/mneme-core 6379:6379 -n mnemecache
mneme-cli --host localhost:6379 --ca-cert /etc/mneme/ca.crt -u admin -p <PASSWORD> stats
```

## Configuration

### Tuning memory

Match `memory.pool_bytes` in the ConfigMap to the Pod's `resources.limits.memory`:

```yaml
# ConfigMap
pool_bytes = "4gb"

# StatefulSet
resources:
  limits:
    memory: "5Gi"    # pool + OS overhead
```

### Storage class

Keeper pods need fast local storage for WAL (`O_DIRECT` writes). Set
`storageClassName` in the volumeClaimTemplates:

```yaml
volumeClaimTemplates:
  - spec:
      storageClassName: local-path    # or gp3, premium-ssd, etc.
```

### Image

Replace `mnemelabs/core:0.1.0` with your registry tag:

```bash
docker build --target core -t registry.example.com/mnemelabs/core:1.0.0 .
docker push registry.example.com/mnemelabs/core:1.0.0

sed -i 's|mnemelabs/core:0.1.0|registry.example.com/mnemelabs/core:1.0.0|g' k8s/*.yaml
```

## Architecture in Kubernetes

```
                    ┌─────────────────────────────┐
  External traffic  │  NodePort 30379              │
  ──────────────▶   │  mneme-core-external Service  │
                    └─────────────────────────────┘
                                 │
                    ┌─────────────────────────────┐
                    │  mneme-core StatefulSet (x1) │
                    │  ClusterIP Service :6379     │
                    │  ClusterIP Service :7379     │ ◀── Keepers + Replicas connect here
                    └─────────────────────────────┘
                    TLS replication │              │ replication
          ┌────────────────────────┼──────────────┼──────────────────┐
          ▼                        ▼              ▼                   ▼
┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐
│ mneme-keeper-0  │ │ mneme-keeper-1  │ │ mneme-keeper-2  │ │ mneme-replica-*  │
│ PVC 20 Gi       │ │ PVC 20 Gi       │ │ PVC 20 Gi       │ │ EVENTUAL reads   │
└─────────────────┘ └─────────────────┘ └─────────────────┘ └─────────────────┘
                                                             ▲
                                                             │
                                                     mneme-replica-read
                                                     ClusterIP :6380
```

## Notes

- Core is a single Raft leader -- do not scale the Core StatefulSet beyond 1.
  For read scaling, add read-replica pods via `replica.yaml`.
- Keeper pods use a PodDisruptionBudget (`minAvailable: 2`) so rolling updates
  never leave fewer than 2 Keepers running (needed for QUORUM writes).
- Read replicas serve EVENTUAL-consistency reads only. QUORUM/ALL reads still
  go through the Core node.
- Solo mode is not intended for production clusters. It runs everything in a
  single pod with no replication.
- `huge_pages` is disabled in ConfigMaps because it requires host-level
  configuration (`vm.nr_hugepages`). Enable it if your nodes are pre-configured.
- The init containers in keeper and replica StatefulSets patch `node_id` from
  the pod hostname ordinal index and wait for Core readiness before starting.
