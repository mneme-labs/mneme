# MnemeCache — Kubernetes Deployment Guide

This guide covers deploying MnemeCache on Kubernetes across **four cluster modes**,
from a single-pod development setup to a full production cluster with read replicas,
monitoring, and automated scaling.

**Cluster modes at a glance:**

| Mode | Pods | Use case | Manifest |
|------|------|----------|----------|
| Solo | 1 | Dev / CI / testing | `k8s/solo.yaml` |
| Core + Keepers | 1 + 3 | Staging / small production | `k8s/core.yaml` + `k8s/keeper.yaml` |
| Core + Keepers + Read Replicas | 1 + 3 + N | Read-heavy workloads | above + `k8s/replica.yaml` |
| Full Production | all-in-one | Production with monitoring + PDB | `k8s/full-cluster.yaml` |

---

## 1 — Prerequisites

| Requirement | Minimum | Notes |
|-------------|---------|-------|
| Kubernetes | 1.27+ | Tested on EKS, GKE, AKS, k3s, Talos |
| `kubectl` | 1.27+ | Configured against your target cluster |
| Container registry | any | `docker buildx` push or a CI pipeline |
| StorageClass | `ReadWriteOnce` | `gp3`, `premium-lrs`, `local-path`, etc. |
| Worker node OS | Linux kernel 5.19+ | Required for MnemeCache memory primitives |

> **Kernel version check**: All major managed Kubernetes offerings (EKS with AL2023,
> GKE with Container-Optimized OS, AKS with Ubuntu 22.04+) satisfy the 5.19+
> requirement. On self-managed clusters, verify with:
>
> ```bash
> kubectl get nodes -o wide
> # Check the KERNEL-VERSION column
> ```

---

## 2 — Build and Push the Image

```bash
# Build for the cluster architecture (usually amd64, or multi-arch)
docker buildx build \
    --platform linux/amd64,linux/arm64 \
    -t your-registry/mnemecache:1.0.0 \
    --push .
```

Update the `image:` field in every manifest you plan to use:

```yaml
image: your-registry/mnemecache:1.0.0
imagePullPolicy: Always
```

> If your registry requires authentication, create an `imagePullSecret` and
> reference it in the StatefulSet pod spec.

---

## 3 — Prepare Secrets

Generate strong values and create the namespace and secret before deploying
any workload:

```bash
CLUSTER_SECRET="$(openssl rand -base64 32)"
ADMIN_PASSWORD="$(openssl rand -base64 16)"

kubectl apply -f k8s/namespace.yaml          # creates 'mnemecache' namespace

kubectl create secret generic mneme-auth \
    --namespace mnemecache \
    --from-literal=cluster_secret="$CLUSTER_SECRET" \
    --from-literal=admin_password="$ADMIN_PASSWORD"
```

Alternatively, apply `k8s/secrets.yaml` after replacing the placeholder base64
values:

```bash
# Encode values
echo -n "$CLUSTER_SECRET" | base64
echo -n "$ADMIN_PASSWORD" | base64
# Paste into k8s/secrets.yaml, then:
kubectl apply -f k8s/secrets.yaml
```

> **Never** commit `secrets.yaml` with real values to version control. Use
> Sealed Secrets, External Secrets Operator, or HashiCorp Vault in production.

---

## 4 — Mode 1: Solo Node

A single pod running MnemeCache in standalone mode. No Keepers, no replication,
no persistence. Ideal for development, CI pipelines, and quick experiments.

### Deploy

```bash
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/secrets.yaml       # or create secret imperatively (see above)
kubectl apply -f k8s/solo.yaml
```

### Architecture

```
┌─────────────────────────────────────────┐
│            mnemecache namespace          │
│                                          │
│  ┌────────────────────────────────────┐  │
│  │        mneme-solo-0 (Pod)          │  │
│  │                                    │  │
│  │  Mnemosyne (God node, RAM-only)    │  │
│  │  :6379  client TLS                 │  │
│  │  :9090  metrics                    │  │
│  └────────────────────────────────────┘  │
│                                          │
│  Service: mneme-solo (ClusterIP :6379)   │
│  Service: mneme-solo-external            │
│           (NodePort :30379)              │
└─────────────────────────────────────────┘
```

### Verify

```bash
kubectl get pods -n mnemecache
# NAME           READY   STATUS    RESTARTS   AGE
# mneme-solo-0   1/1     Running   0          30s

# Quick smoke test
kubectl exec -n mnemecache mneme-solo-0 -- \
    mneme-cli --host 127.0.0.1:6379 set hello world

kubectl exec -n mnemecache mneme-solo-0 -- \
    mneme-cli --host 127.0.0.1:6379 get hello
# "world"
```

> **Limitations**: Solo mode has no persistence, no replication, and no warmup
> gating. Data is lost when the pod restarts.

---

## 5 — Mode 2: Core + Keepers

One Core (Mnemosyne God node) and three Keepers (Hypnos persistence nodes) form
the baseline production topology. Writes are replicated to Keepers via the
Hermes fabric; reads from RAM are served by the Core at sub-millisecond latency.

### Deploy

```bash
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/secrets.yaml

# Deploy Core first — Keepers depend on it
kubectl apply -f k8s/core.yaml
kubectl rollout status statefulset/mneme-core -n mnemecache --timeout=120s

# Deploy Keepers — init containers wait for Core readiness
kubectl apply -f k8s/keeper.yaml
kubectl rollout status statefulset/mneme-keeper -n mnemecache --timeout=180s
```

### Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                      mnemecache namespace                        │
│                                                                  │
│  ┌──────────────────────┐          ┌──────────────────────────┐  │
│  │   mneme-core-0       │  Hermes  │  mneme-keeper-0          │  │
│  │                      │◄────────►│  Hypnos + Aoide (WAL)    │  │
│  │  Mnemosyne           │  mTLS    │  + Melete (snapshots)    │  │
│  │  (God node, RAM)     │  :7379   │  + Oneiros (cold store)  │  │
│  │  :6379 client TLS    │          │  PVC: 20Gi               │  │
│  │  :7379 replication   │          └──────────────────────────┘  │
│  │  :9090 metrics       │          ┌──────────────────────────┐  │
│  │                      │◄────────►│  mneme-keeper-1          │  │
│  │  Themis (Raft)       │          │  (same as above)         │  │
│  │  Herold (registry)   │          │  PVC: 20Gi               │  │
│  │  Moirai (dispatch)   │          └──────────────────────────┘  │
│  │  PVC: 1Gi            │          ┌──────────────────────────┐  │
│  └──────────────────────┘◄────────►│  mneme-keeper-2          │  │
│                                     │  (same as above)         │  │
│  Services:                          │  PVC: 20Gi               │  │
│   mneme-core (ClusterIP :6379)      └──────────────────────────┘  │
│   mneme-core-repl (ClusterIP :7379)                              │
│   mneme-core-external (NodePort :30379)                          │
│   mneme-keeper (headless, pod DNS)                               │
└──────────────────────────────────────────────────────────────────┘
```

### Wait for Warmup

After all three Keepers complete their push phase (Herold registration, full
key replay, SyncComplete), the Core transitions from `Warming` to `Hot`.
QUORUM and ALL consistency reads are blocked until warmup completes.

```bash
# Watch warmup progress
kubectl logs -n mnemecache mneme-core-0 -f | grep -i warmup

# Expected: "warmup_state: Hot" once all Keepers finish sync
```

### Verify

```bash
kubectl get pods -n mnemecache -o wide
# NAME             READY   STATUS    RESTARTS   AGE
# mneme-core-0     1/1     Running   0          2m
# mneme-keeper-0   1/1     Running   0          90s
# mneme-keeper-1   1/1     Running   0          90s
# mneme-keeper-2   1/1     Running   0          90s

# Cluster info
kubectl exec -n mnemecache mneme-core-0 -- \
    mneme-cli --host 127.0.0.1:6379 cluster-info

# Keeper list — should show 3 keepers with pool sizes
kubectl exec -n mnemecache mneme-core-0 -- \
    mneme-cli --host 127.0.0.1:6379 keeper-list

# Write + read test
kubectl exec -n mnemecache mneme-core-0 -- \
    mneme-cli --host 127.0.0.1:6379 set mykey myvalue

kubectl exec -n mnemecache mneme-core-0 -- \
    mneme-cli --host 127.0.0.1:6379 get mykey
```

---

## 6 — Mode 3: Core + Keepers + Read Replicas

Add read replicas to offload EVENTUAL-consistency reads from the Core. Read
replicas are God nodes running in `read-replica` role: they receive replication
frames from the Core and serve reads, but never accept writes.

### Deploy

Start from a running Core + Keepers cluster (Mode 2), then add replicas:

```bash
kubectl apply -f k8s/replica.yaml
kubectl rollout status statefulset/mneme-replica -n mnemecache --timeout=120s
```

### Architecture

```
                        Clients (EVENTUAL reads)
                            │           │
                  ┌─────────┘           └──────────┐
                  ▼                                 ▼
┌──────────────────────────────────────────────────────────────────┐
│                      mnemecache namespace                        │
│                                                                  │
│  ┌─────────────────┐                    ┌──────────────────────┐ │
│  │  mneme-replica-0 │◄──repl (mTLS)───┐ │  mneme-keeper-0..2  │ │
│  │  God read-replica│                 │ │  (Hypnos, 20Gi PVC) │ │
│  │  :6379 :9090     │                 │ └──────────────────────┘ │
│  └─────────────────┘                 │                          │
│  ┌─────────────────┐    ┌────────────┴───────┐                  │
│  │  mneme-replica-1 │◄──│   mneme-core-0      │                  │
│  │  God read-replica│   │   Mnemosyne (write) │                  │
│  │  :6379 :9090     │   │   :6379 :7379 :9090 │                  │
│  └─────────────────┘    └────────────────────┘                  │
│                                                                  │
│  Service: mneme-replica (ClusterIP :6379, EVENTUAL reads only)   │
└──────────────────────────────────────────────────────────────────┘
```

### Verify EVENTUAL Reads

```bash
# Write via Core
kubectl exec -n mnemecache mneme-core-0 -- \
    mneme-cli --host 127.0.0.1:6379 set counter 42

# Read via read replica (EVENTUAL consistency)
kubectl exec -n mnemecache mneme-replica-0 -- \
    mneme-cli --host 127.0.0.1:6379 --consistency eventual get counter
# "42"
```

> **Important**: Read replicas only serve EVENTUAL consistency reads. QUORUM,
> ALL, and ONE reads must go through the Core. Writes always go through the Core.

---

## 7 — Mode 4: Full Production Cluster

The all-in-one manifest deploys the complete stack: Core, Keepers, read
replicas, monitoring (ServiceMonitor + alerting rules), PodDisruptionBudgets,
and NetworkPolicies.

### Deploy

**Option A** — Single manifest:

```bash
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/secrets.yaml
kubectl apply -f k8s/full-cluster.yaml
```

**Option B** — Individual manifests (equivalent, more granular control):

```bash
kubectl apply -f k8s/namespace.yaml
kubectl apply -f k8s/secrets.yaml
kubectl apply -f k8s/core.yaml
kubectl rollout status statefulset/mneme-core -n mnemecache --timeout=120s
kubectl apply -f k8s/keeper.yaml
kubectl apply -f k8s/replica.yaml
kubectl apply -f k8s/monitoring.yaml
```

### Architecture

```
                  ┌──────────────────────────────────────┐
                  │         Prometheus / Grafana          │
                  │  ServiceMonitor scrapes :9090/metrics │
                  └─────────┬──────────┬─────────────────┘
                            │          │
┌───────────────────────────┼──────────┼──────────────────────────────┐
│  mnemecache namespace     │          │                              │
│                           ▼          ▼                              │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                    mneme-core-0                              │   │
│  │  Mnemosyne (God)  Themis (Raft)  Herold (registry)          │   │
│  │  Moirai (dispatch)  Lethe (eviction)  Iris (slot router)    │   │
│  │  :6379 (client TLS)  :7379 (replication mTLS)  :9090        │   │
│  │  PVC: 1Gi (certs + users.db)                                │   │
│  └──────┬──────────┬──────────┬──────────┬─────────────────────┘   │
│         │ Hermes   │          │          │ replication              │
│         ▼          ▼          ▼          ▼                          │
│  ┌──────────┐┌──────────┐┌──────────┐┌──────────────────────────┐  │
│  │ keeper-0 ││ keeper-1 ││ keeper-2 ││ replica-0  replica-1     │  │
│  │ Hypnos   ││ Hypnos   ││ Hypnos   ││ God (read-replica role) │  │
│  │ Aoide    ││ Aoide    ││ Aoide    ││ :6379  :9090             │  │
│  │ Melete   ││ Melete   ││ Melete   ││                          │  │
│  │ Oneiros  ││ Oneiros  ││ Oneiros  ││ EVENTUAL reads only      │  │
│  │ 20Gi PVC ││ 20Gi PVC ││ 20Gi PVC ││                          │  │
│  └──────────┘└──────────┘└──────────┘└──────────────────────────┘  │
│                                                                     │
│  PodDisruptionBudget: mneme-keeper-pdb (minAvailable: 2)           │
│                                                                     │
│  Services:                                                          │
│   mneme-core          ClusterIP  :6379  (writes + QUORUM/ALL reads)│
│   mneme-core-repl     ClusterIP  :7379  (replication)              │
│   mneme-core-external NodePort   :30379 (external client access)   │
│   mneme-keeper        Headless          (pod DNS for Hermes)       │
│   mneme-replica       ClusterIP  :6379  (EVENTUAL reads)           │
│   mneme-metrics       ClusterIP  :9090  (Prometheus scrape target) │
└─────────────────────────────────────────────────────────────────────┘
```

### Monitoring Setup

Apply the monitoring manifest (included in `full-cluster.yaml` or standalone):

```bash
kubectl apply -f k8s/monitoring.yaml
```

This creates a `ServiceMonitor` for the Prometheus Operator and recommended
alerting rules. See [Section 9 — Prometheus Monitoring](#9--prometheus-monitoring)
for details.

### PodDisruptionBudget

The Keeper PDB ensures at least 2 of 3 Keepers remain available during
voluntary disruptions (node drains, cluster upgrades):

```yaml
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: mneme-keeper-pdb
  namespace: mnemecache
spec:
  minAvailable: 2
  selector:
    matchLabels:
      app.kubernetes.io/name: mnemecache
      app.kubernetes.io/component: keeper
```

This guarantees QUORUM writes succeed even during rolling maintenance.

---

## 8 — Connecting to the Cluster

### In-Cluster (Service DNS)

Applications running inside the same Kubernetes cluster connect via Service DNS:

| Purpose | Address |
|---------|---------|
| Writes + strong reads | `mneme-core.mnemecache.svc.cluster.local:6379` |
| EVENTUAL reads (replicas) | `mneme-replica.mnemecache.svc.cluster.local:6379` |
| Replication (internal) | `mneme-core-repl.mnemecache.svc.cluster.local:7379` |
| Individual Keeper pod | `mneme-keeper-N.mneme-keeper.mnemecache.svc.cluster.local:7379` |

Example Pontus (mneme-client) configuration:

```toml
[client]
host = "mneme-core.mnemecache.svc.cluster.local"
port = 6379
tls  = true
ca_cert = "/etc/mneme/ca.crt"
```

> Mount the MnemeCache CA certificate as a ConfigMap or Secret volume in your
> application pods.

### External Access

**NodePort** (default in manifests):

The `mneme-core-external` service exposes port **30379** on every worker node:

```bash
# Connect from outside the cluster
mneme-cli --host <any-node-ip>:30379 --tls ping
```

**LoadBalancer** (cloud environments):

```yaml
apiVersion: v1
kind: Service
metadata:
  name: mneme-core-lb
  namespace: mnemecache
  annotations:
    # AWS NLB example:
    service.beta.kubernetes.io/aws-load-balancer-type: "nlb"
spec:
  type: LoadBalancer
  selector:
    app.kubernetes.io/name: mnemecache
    app.kubernetes.io/component: core
  ports:
    - name: client
      port: 6379
      targetPort: 6379
```

**Ingress** (TCP passthrough via nginx-ingress or similar):

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: tcp-services
  namespace: ingress-nginx
data:
  "6379": "mnemecache/mneme-core:6379"
```

> MnemeCache uses a binary protocol over TLS, not HTTP. Standard HTTP Ingress
> rules do not apply. Use TCP passthrough or a dedicated L4 load balancer.

---

## 9 — Prometheus Monitoring

All MnemeCache pods expose Aletheia metrics on port **9090** at `/metrics`.

### Pod Annotations (auto-discovery)

Every pod template includes:

```yaml
annotations:
  prometheus.io/scrape: "true"
  prometheus.io/port:   "9090"
  prometheus.io/path:   "/metrics"
```

### ServiceMonitor (Prometheus Operator)

Apply `k8s/monitoring.yaml` or create manually:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: mnemecache
  namespace: mnemecache
  labels:
    release: prometheus     # match your Prometheus Operator selector
spec:
  selector:
    matchLabels:
      app.kubernetes.io/name: mnemecache
  namespaceSelector:
    matchNames:
      - mnemecache
  endpoints:
    - port: metrics
      path: /metrics
      interval: 15s
```

### Scrape Config (standalone Prometheus)

If not using the Prometheus Operator, add to `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: mnemecache
    kubernetes_sd_configs:
      - role: pod
        namespaces:
          names: [mnemecache]
    relabel_configs:
      - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_scrape]
        action: keep
        regex: "true"
      - source_labels: [__meta_kubernetes_pod_annotation_prometheus_io_port]
        action: replace
        target_label: __address__
        regex: (.+)
        replacement: ${1}:9090
      - source_labels: [__meta_kubernetes_pod_label_app_kubernetes_io_component]
        target_label: component
```

### Alerting Rules

Recommended alerts for production (included in `k8s/monitoring.yaml`):

```yaml
apiVersion: monitoring.coreos.com/v1
kind: PrometheusRule
metadata:
  name: mnemecache-alerts
  namespace: mnemecache
spec:
  groups:
    - name: mnemecache.rules
      rules:
        - alert: MnemeCacheHighMemoryPressure
          expr: mneme_pool_pressure_ratio > 0.85
          for: 5m
          labels:
            severity: warning
          annotations:
            summary: "Memory pressure above 85% on {{ $labels.pod }}"

        - alert: MnemeCacheOOM
          expr: mneme_pool_pressure_ratio >= 1.0
          for: 1m
          labels:
            severity: critical
          annotations:
            summary: "OOM condition on {{ $labels.pod }}"

        - alert: MnemeCacheKeeperDown
          expr: up{job="mnemecache", component="keeper"} == 0
          for: 2m
          labels:
            severity: critical
          annotations:
            summary: "Keeper {{ $labels.pod }} unreachable"

        - alert: MnemeCacheReplicationLag
          expr: mneme_replication_lag_ms > 5000
          for: 3m
          labels:
            severity: warning
          annotations:
            summary: "Replication lag >5s to {{ $labels.keeper }}"

        - alert: MnemeCacheWarmupStuck
          expr: mneme_warmup_state != 2   # 2 = Hot
          for: 10m
          labels:
            severity: warning
          annotations:
            summary: "Core stuck in warmup for >10 minutes"
```

### Key Metrics to Watch

| Metric | Component | Description |
|--------|-----------|-------------|
| `mneme_pool_bytes_used` | Core | Current RAM usage |
| `mneme_pool_pressure_ratio` | Core | Memory pressure (0.0 - 1.0+) |
| `mneme_requests_total` | Core | Requests by cmd and consistency |
| `mneme_request_duration_seconds` | Core | Latency histogram |
| `mneme_replication_lag_ms` | Hermes | Per-Keeper replication lag |
| `mneme_connections_active` | Charon | Active client connections |
| `mneme_evictions_total` | Lethe | Evictions by type (lfu, ttl, oom) |
| `mneme_wal_bytes` | Aoide | WAL size per Keeper |
| `mneme_cluster_term` | Themis | Current Raft term |

---

## 10 — Scaling

### Scale Keepers

```bash
# Scale from 3 to 5 Keepers
kubectl scale statefulset mneme-keeper -n mnemecache --replicas=5
```

Each new Keeper pod will:
1. Wait for Core readiness (init container)
2. Register with Core via the Herold protocol (SyncStart with node_id, key_count, replication_addr)
3. Core dials back to the Keeper over mTLS (Hermes connect_to_keeper)
4. Receive all existing keys via the push phase (PushKey frames)
5. Send SyncComplete — Core decrements the warmup pending counter
6. Begin receiving live replication frames

> **Scaling down**: Scale the StatefulSet replicas. The Core detects disconnected
> Keepers and removes them from the Moirai dispatch map. Ensure at least
> `floor(N/2)+1` Keepers remain for QUORUM writes to succeed.

### Scale Read Replicas

```bash
# Scale from 2 to 4 read replicas
kubectl scale statefulset mneme-replica -n mnemecache --replicas=4
```

Read replicas are stateless (no PVC). They connect to the Core, receive the
full key set, and then serve EVENTUAL reads. Scaling is fast (~3 seconds for
sync on typical datasets).

### Auto-Scaling Considerations

MnemeCache is a stateful system. Horizontal Pod Autoscaler (HPA) can be used
for read replicas but requires care for Keepers:

**Read replicas** (safe to auto-scale):

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: mneme-replica-hpa
  namespace: mnemecache
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: StatefulSet
    name: mneme-replica
  minReplicas: 2
  maxReplicas: 8
  metrics:
    - type: Pods
      pods:
        metric:
          name: mneme_connections_active
        target:
          type: AverageValue
          averageValue: "500"
```

**Keepers** (auto-scale with caution): Scaling Keepers triggers a full data
sync per new pod. Use the Aletheia `mneme_pool_pressure_ratio` metric and
set conservative thresholds:

```yaml
metrics:
  - type: Pods
    pods:
      metric:
        name: mneme_pool_pressure_ratio
      target:
        type: AverageValue
        averageValue: "0.7"
```

> Monitor `mneme_pool_pressure_ratio`. At 0.70+ consider adding Keepers.
> At 0.90+ Lethe begins proactive LFU eviction. At 1.0 OOM eviction kicks in.

---

## 11 — Storage

### PVC Layout

| Component | PVC Size | Contents | Access Mode |
|-----------|----------|----------|-------------|
| Core | 1Gi | TLS certificates, `users.db` | `ReadWriteOnce` |
| Keeper | 20Gi | WAL (Aoide), snapshots (Melete), cold store (Oneiros/redb) | `ReadWriteOnce` |
| Read Replica | none | Stateless, RAM only | N/A |
| Solo | none | Stateless, RAM only | N/A |

### StorageClass Selection

Choose the StorageClass based on your environment:

| Environment | StorageClass | Notes |
|-------------|-------------|-------|
| AWS EKS | `gp3` | General purpose SSD, good balance of cost and performance |
| Azure AKS | `managed-premium` or `premium-lrs` | Premium SSD for low-latency WAL writes |
| GCP GKE | `premium-rwo` | SSD persistent disk |
| Local dev (k3s, kind) | `local-path` | Host-path storage, not suitable for production |
| Bare metal | `local-storage` | Node-local NVMe, best WAL performance |

Configure in the StatefulSet `volumeClaimTemplates`:

```yaml
volumeClaimTemplates:
  - metadata:
      name: data
    spec:
      accessModes: ["ReadWriteOnce"]
      storageClassName: gp3            # change to your StorageClass
      resources:
        requests:
          storage: 20Gi
```

> **Performance note**: WAL writes (Aoide) use `O_DIRECT` + `fallocate`. For
> best performance, use SSD-backed storage classes. Avoid network-attached HDD
> volumes for Keeper PVCs.

### Resizing PVCs

If your StorageClass supports volume expansion (`allowVolumeExpansion: true`):

```bash
kubectl patch pvc data-mneme-keeper-0 -n mnemecache \
    -p '{"spec":{"resources":{"requests":{"storage":"50Gi"}}}}'
```

---

## 12 — Rolling Upgrades

### Image Update

```bash
# Update Core image
kubectl set image statefulset/mneme-core \
    mneme-core=your-registry/mnemecache:1.1.0 \
    -n mnemecache

# Update Keeper image
kubectl set image statefulset/mneme-keeper \
    mneme-keeper=your-registry/mnemecache:1.1.0 \
    -n mnemecache

# Update read replicas
kubectl set image statefulset/mneme-replica \
    mneme-replica=your-registry/mnemecache:1.1.0 \
    -n mnemecache

# Monitor rollout
kubectl rollout status statefulset/mneme-keeper -n mnemecache
kubectl rollout status statefulset/mneme-replica -n mnemecache
```

### Upgrade Order

For zero-downtime upgrades, follow this order:

1. **Read replicas first** — stateless, fast restart, no data risk
2. **Keepers next** — PDB ensures at least 2 remain available; each Keeper
   performs `fsync WAL` and a final snapshot before shutting down
3. **Core last** — follows the shutdown drain sequence (stop accept, drain
   in-flight, flush Hermes, Themis stepdown)

### PDB Guarantees

The PodDisruptionBudget (`minAvailable: 2`) ensures:
- At most 1 Keeper pod is unavailable at any time during voluntary disruptions
- QUORUM writes (floor(3/2)+1 = 2 ACKs needed) continue to succeed
- `kubectl drain` on a node will wait if evicting the pod would violate the PDB

```bash
# Verify PDB status
kubectl get pdb -n mnemecache
# NAME               MIN AVAILABLE   MAX UNAVAILABLE   ALLOWED DISRUPTIONS
# mneme-keeper-pdb   2               N/A               1
```

### Rollback

```bash
# Undo the last rollout
kubectl rollout undo statefulset/mneme-keeper -n mnemecache
kubectl rollout undo statefulset/mneme-core -n mnemecache
```

---

## 13 — Backup and Restore

### Snapshot on Demand

Trigger a Melete snapshot on a specific Keeper:

```bash
kubectl exec -n mnemecache mneme-keeper-0 -- \
    mneme-cli --host 127.0.0.1:6379 snapshot
```

Trigger snapshots on all Keepers:

```bash
for i in 0 1 2; do
  kubectl exec -n mnemecache mneme-keeper-$i -- \
      mneme-cli --host 127.0.0.1:6379 snapshot &
done
wait
```

### PVC Snapshots (Velero)

Use Velero for cluster-wide, crash-consistent backups of all Keeper PVCs:

```bash
# Install Velero (one-time)
velero install --provider aws --bucket mneme-backups \
    --secret-file ./cloud-credentials

# Create a backup
velero backup create mneme-backup-$(date +%Y%m%d) \
    --include-namespaces mnemecache \
    --include-resources persistentvolumeclaims,persistentvolumes

# Schedule daily backups
velero schedule create mneme-daily \
    --schedule="0 3 * * *" \
    --include-namespaces mnemecache \
    --ttl 168h    # retain for 7 days
```

For cloud-native volume snapshots (without Velero):

```bash
# AWS EBS snapshot example
kubectl apply -f - <<EOF
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshot
metadata:
  name: mneme-keeper-0-snap
  namespace: mnemecache
spec:
  volumeSnapshotClassName: csi-aws-vsc
  source:
    persistentVolumeClaimName: data-mneme-keeper-0
EOF
```

### Restore

1. Stop the StatefulSets (scale to 0)
2. Restore PVCs from snapshots (Velero restore or manual PVC recreation)
3. Scale StatefulSets back up
4. Core restarts; each Keeper replays its WAL (Aoide) and cold store (Oneiros)
   automatically on startup
5. Wait for warmup to reach `Hot`

```bash
# Velero restore
velero restore create --from-backup mneme-backup-20260321

# Redeploy
kubectl apply -f k8s/core.yaml
kubectl rollout status statefulset/mneme-core -n mnemecache --timeout=120s
kubectl apply -f k8s/keeper.yaml
kubectl rollout status statefulset/mneme-keeper -n mnemecache --timeout=300s

# Verify warmup completes
kubectl logs -n mnemecache mneme-core-0 -f | grep warmup
```

---

## 14 — Manifest Reference Table

| File | Resources Created |
|------|-------------------|
| `k8s/namespace.yaml` | `Namespace: mnemecache` |
| `k8s/secrets.yaml` | `Secret: mneme-auth` (cluster_secret, admin_password) |
| `k8s/solo.yaml` | `StatefulSet: mneme-solo`, `Service: mneme-solo` (ClusterIP), `Service: mneme-solo-external` (NodePort), `ConfigMap`, `ServiceAccount` |
| `k8s/core.yaml` | `StatefulSet: mneme-core`, `Service: mneme-core` (ClusterIP :6379), `Service: mneme-core-repl` (ClusterIP :7379), `Service: mneme-core-external` (NodePort :30379), `ConfigMap`, `ServiceAccount`, `PVC: 1Gi` |
| `k8s/keeper.yaml` | `StatefulSet: mneme-keeper` (3 replicas), `Service: mneme-keeper` (headless), `ConfigMap`, `ServiceAccount`, `PodDisruptionBudget` (minAvailable: 2), `PVC: 20Gi per pod` |
| `k8s/replica.yaml` | `StatefulSet: mneme-replica` (2 replicas), `Service: mneme-replica` (ClusterIP :6379), `ConfigMap`, `ServiceAccount` |
| `k8s/monitoring.yaml` | `ServiceMonitor`, `PrometheusRule` (alerting rules), `Service: mneme-metrics` |
| `k8s/full-cluster.yaml` | All of the above in a single multi-document YAML |

### Labels

All resources use the standard Kubernetes recommended labels:

```yaml
labels:
  app.kubernetes.io/name: mnemecache
  app.kubernetes.io/component: core|keeper|replica|solo
  app.kubernetes.io/part-of: mnemecache
  app.kubernetes.io/managed-by: kubectl
```

### Ports

| Port | Protocol | Service | Purpose |
|------|----------|---------|---------|
| 6379 | TLS 1.3 | mneme-core, mneme-replica, mneme-solo | Client connections |
| 7379 | mTLS | mneme-core-repl | Hermes replication fabric |
| 9090 | HTTP | mneme-metrics | Prometheus / Aletheia metrics |
| 30379 | TLS 1.3 | mneme-core-external (NodePort) | External client access |

---

## 15 — Troubleshooting

### Keepers Stuck in `Init:0/1`

The init container waits for Core to be ready before allowing the Keeper to
start. Common causes:

```bash
# Check init container logs
kubectl logs -n mnemecache mneme-keeper-0 -c wait-for-core

# Common causes:
# - Core pod not yet running (check: kubectl get pods -n mnemecache)
# - Core service not resolving (check: kubectl get svc -n mnemecache)
# - Network policy blocking traffic
```

### `warmup_state` Stays `Warming`

All Keepers must complete their push phase for warmup to finish. Check each
Keeper's sync status:

```bash
kubectl logs -n mnemecache mneme-core-0 | grep -iE "sync|warmup|keeper"

# Verify all Keepers registered
kubectl exec -n mnemecache mneme-core-0 -- \
    mneme-cli --host 127.0.0.1:6379 keeper-list
```

If a Keeper is stuck during push, check its logs:

```bash
kubectl logs -n mnemecache mneme-keeper-0 | grep -iE "sync|push|herold"
```

### PVC Not Bound

```bash
kubectl get pvc -n mnemecache
kubectl describe pvc data-mneme-keeper-0 -n mnemecache

# Verify StorageClass exists
kubectl get storageclass

# Common causes:
# - StorageClass does not exist or is misspelled
# - No available PVs (local-storage provisioner)
# - Insufficient disk quota (cloud)
```

### TLS Handshake Errors

Core auto-generates a CA and node certificates on first start. Certificates
are stored on the Core PVC at `/var/lib/mneme/`.

```bash
# Check certificate generation
kubectl logs -n mnemecache mneme-core-0 | grep -i tls

# Verify certificate files exist
kubectl exec -n mnemecache mneme-core-0 -- ls -la /var/lib/mneme/*.crt

# Common causes:
# - Core PVC lost (certificates regenerated, Keepers have old CA)
# - server_name mismatch (must match config.tls.server_name)
# - Clock skew between nodes (certificates appear expired)
```

### Connection Refused from Application Pods

```bash
# Verify service endpoints
kubectl get endpoints mneme-core -n mnemecache

# Test connectivity from a debug pod
kubectl run -n mnemecache debug --rm -it --image=busybox -- \
    nc -zv mneme-core.mnemecache.svc.cluster.local 6379

# Common causes:
# - Application not in the right namespace (use FQDN)
# - NetworkPolicy blocking ingress to mnemecache namespace
# - Client not configured for TLS
```

### High Memory Pressure / OOM Evictions

```bash
# Check current pressure
kubectl exec -n mnemecache mneme-core-0 -- \
    mneme-cli --host 127.0.0.1:6379 cluster-info
# Look for memory_pressure field

# Check eviction metrics
kubectl exec -n mnemecache mneme-core-0 -- \
    curl -s localhost:9090/metrics | grep eviction

# Resolution:
# - Scale Keepers to increase pool: kubectl scale sts mneme-keeper --replicas=5
# - Increase Core pod memory limits
# - Adjust eviction_threshold in config
```

### Keeper Replication Lag

```bash
# Check lag per Keeper
kubectl exec -n mnemecache mneme-core-0 -- \
    curl -s localhost:9090/metrics | grep replication_lag

# Common causes:
# - Keeper disk I/O bottleneck (check WAL sync duration)
# - Network congestion between Core and Keeper pods
# - Keeper pod resource limits too low (CPU throttling)
```

### Pod CrashLoopBackOff

```bash
# Check recent logs
kubectl logs -n mnemecache <pod-name> --previous

# Check events
kubectl describe pod -n mnemecache <pod-name>

# Common causes:
# - Missing secret (mneme-auth not created)
# - Invalid configuration in ConfigMap
# - Insufficient memory (OOMKilled by kubelet — different from MnemeCache OOM)
# - Kernel version too old (check node kernel with kubectl get nodes -o wide)
```
