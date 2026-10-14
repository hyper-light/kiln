# Deploying kiln

kiln is one static binary (`kiln`) with the UI embedded. It runs in two modes, with the same
metrics, alert rules and Grafana dashboards in both. Everything here is free and open source.

```
deploy/
├── docker/kiln.toml             default config baked into the image (/etc/kiln/kiln.toml)
├── helm/kiln/                   Helm chart (server, RBAC, PVCs, Services, ServiceMonitor, rules, dashboards)
│   ├── dashboards -> ../../grafana/dashboards   (symlink; shared)
│   └── rules      -> ../../prometheus           (symlink; shared)
├── grafana/dashboards/          "kiln / Overview" + "kiln / Fleet" (uid kiln-overview / kiln-fleet)
├── prometheus/kiln-rules.yaml   alerting + recording rules (single source of truth)
├── argocd/                      app-of-apps: root.yaml, apps/ chart, values/, namespaces/, bootstrap/
├── gitserver/                   in-cluster read-only git server + push.sh
├── cluster/                     minikube bootstrap / teardown / privileged-setup / port-forward / expose-lan
└── baremetal/                   Prometheus + Grafana via docker compose for kiln without Kubernetes
```

The image is built from the top-level `Dockerfile` (`docker build -t kiln:dev .`): Vite UI build
→ static musl binary → Alpine runtime (git, bash, openssh-client) running as uid 10001 with
`ENTRYPOINT ["/kiln","supervise","--"]` and `CMD ["server"]`. Ports: 7070/tcp (UI, API,
`/metrics`, `/healthz`, `/readyz`), 7071/udp (QUIC agents).

---

## Mode 1 — bare metal (no Kubernetes)

```sh
cargo build --release -p kiln              # or: docker build -t kiln:dev .
sudo deploy/cluster/privileged-setup.sh --install-kiln   # optional: /usr/local/bin/kiln, docker group, sysctls
kiln install                               # boot hook (systemd / @reboot / XDG autostart), see docs/architecture.md
deploy/baremetal/up.sh                     # Prometheus :9090 + Grafana :3000 (localhost only)
```

`deploy/baremetal/docker-compose.yml` runs `prom/prometheus:v3.15.0` (7d / 2 GB retention,
512 MiB cap) and `grafana/grafana:13.2.4` (256 MiB cap). Prometheus scrapes
`host.docker.internal:7070` (mapped to the host via `host-gateway`) as `job="kiln"` and loads
`deploy/prometheus/kiln-rules.yaml`; Grafana is provisioned with the Prometheus datasource
(uid `prometheus`) and both kiln dashboards, dark theme. The Grafana password is generated
into `deploy/baremetal/.env` (0600, git-ignored). kiln must listen on `0.0.0.0:7070` (the default)
so the containers can reach it; agents report host telemetry through the server, so the server is
the only scrape target.

Without docker at all: run the stock Prometheus binary with `deploy/baremetal/prometheus.yml`
(change the target to `127.0.0.1:7070`) and import the JSON dashboards in any Grafana.

---

## Mode 2 — Kubernetes

### Quick start: local multi-node cluster

```sh
sudo deploy/cluster/privileged-setup.sh      # ONE time, needs your sudo password
deploy/cluster/bootstrap.sh                  # or --unattended (never prompts)
deploy/cluster/port-forward.sh               # kiln :7070, ArgoCD :8080, Grafana :3000, Prometheus :9090
cat ~/.local/state/kiln/cluster/credentials.env
```

`bootstrap.sh` creates a 3-node minikube cluster (profile `kiln`, docker driver), builds and
loads `kiln:dev`, deploys the in-cluster git server and publishes this repo to it, installs
ArgoCD, applies the root Application and waits until everything is Synced/Healthy. Details,
layout and knobs: [`cluster/README.md`](cluster/README.md).

### Any cluster: Helm only

```sh
helm upgrade --install kiln deploy/helm/kiln -n kiln --create-namespace \
  --set image.repository=<registry>/kiln --set image.tag=<tag>
```

What the chart creates:

| Resource | Notes |
| --- | --- |
| `Deployment/kiln` | 1 replica, `Recreate`, `enableServiceLinks: false`, non-root, read-only root FS (`/tmp` emptyDir, `HOME=/tmp`), probes `/healthz` (startup+liveness) and `/readyz` |
| `ConfigMap/kiln-config` | `kiln.toml` rendered from `values.config` + derived addresses; checksum triggers restarts |
| `Secret/kiln-admin` | admin token → `KILN_ADMIN_TOKEN`; generated once and kept via `lookup`, or `adminToken.existingSecret` (required under ArgoCD) |
| `PVC/kiln-data` | `data_dir` (sqlite, logs, pki, tokens), kept on uninstall |
| `Service/kiln` | ClusterIP 7070/TCP + 7071/UDP — what job pods dial (`kiln.kiln.svc`) |
| `Service/kiln-external` | optional NodePort/LoadBalancer for agents outside the cluster |
| `ServiceAccount/kiln` + `Role/RoleBinding` in `kiln-jobs` | pods (create/get/list/watch/delete), pods/log, secrets (create/get/delete), events, PVC get |
| `ServiceAccount/kiln-job` in `kiln-jobs` | no permissions, token not mounted |
| `Namespace/kiln-jobs` | optional (`namespaces.createJobs`), PSA `baseline`, kept on uninstall |
| `PVC/kiln-cache` in `kiln-jobs` | build cache mounted at `/cache` in job pods (RWX by default) |
| `ServiceMonitor`, `PrometheusRule` | `release: kube-prometheus-stack` label; `job="kiln"` via `jobLabel` |
| dashboards `ConfigMap`s | label `grafana_dashboard: "1"`, annotation `grafana_folder: kiln` |
| `NetworkPolicy`, `Ingress` | optional, off by default |

### Alerts (`deploy/prometheus/kiln-rules.yaml`)

`KilnServerDown`, `KilnServerAbsent`, `KilnQueueBacklog`, `KilnQueueWaitHigh`,
`KilnNoCapacityForQueuedJobs`, `KilnJobFailureRatioHigh`, `KilnRepoFailing`,
`KilnAgentDisconnected`, `KilnAgentsConnectedDropped`, `KilnSupervisorRestarts`,
`KilnServerProcessRestarted`, `KilnCrashLooping`, `KilnPanicsCaught`, `KilnGitopsPollErrors`,
`KilnGithubRateLimitLow`, `KilnK8sPodErrors`, `KilnHighMemory`, `KilnFileDescriptorsHigh`,
`KilnAgentDiskFull`. Validate with `promtool check rules deploy/prometheus/kiln-rules.yaml`.

---

## Bare-metal agents joining the in-cluster server

Benchmark boxes run `kiln agent` on real hardware and connect to the server over HTTP 7070
(enrollment) and QUIC UDP 7071. Inside the cluster, job pods use the ClusterIP Service; from
outside you need a reachable address:

1. **Expose the ports.** The minikube values enable `Service/kiln-external` as NodePort
   **30070/TCP** + **30071/UDP**. From the same host use `http://$(minikube -p kiln ip):30070`.
   For other machines on the LAN, minikube's node IPs are host-only, so run
   `deploy/cluster/expose-lan.sh up`, which forwards `<host>:7070/tcp` and `<host>:7071/udp` to the
   NodePorts with two tiny socat containers (open these ports in the host firewall). On a real
   cluster use `externalService.type: LoadBalancer` (MetalLB is free) or NodePort directly.
2. **Advertise it.** Set the address agents should use in `deploy/argocd/values/kiln.yaml`
   (or your Helm values) and re-publish:
   ```yaml
   publicURL: http://192.168.1.50:7070     # join command / UI links
   quicAdvertise: 192.168.1.50:7071        # QUIC endpoint handed to agents
   tlsNames: [192.168.1.50]                # added to the server certificate (mTLS)
   ```
   Job pods are unaffected: they always use `kubernetes.server_url` / `server_quic`, which the
   chart derives from the in-cluster Service.
3. **Join.** In the UI (Agents → Join) or `GET /api/agents/join`, copy the join command and run it
   on the benchmark machine.

QUIC is UDP: NAT/firewalls must pass UDP 7071 both ways; `externalTrafficPolicy: Local` keeps
client source addresses if you need them.

---

## GitOps source and switching to GitHub

ArgoCD reads this repository. Until it lives on GitHub, the cluster runs its own read-only git
server (`http://kiln-git.kiln-git.svc.cluster.local/kiln.git`) that `deploy/gitserver/push.sh`
publishes the local working tree to (see [`cluster/README.md`](cluster/README.md#the-git-source-in-cluster-git-server)).

The URL is a single value. To move to GitHub:

```sh
KILN_REPO_URL=https://github.com/hyper-light/kiln deploy/cluster/bootstrap.sh --unattended
```

That re-renders `deploy/argocd/root.yaml`, whose `repoURL`/`targetRevision` Helm parameters
flow into every child Application (`deploy/argocd/apps/values.yaml` holds the defaults). For the
in-repo default, edit `repoURL` in `deploy/argocd/apps/values.yaml` too.

## Versions

| Component | Version |
| --- | --- |
| argo-cd Helm chart | 10.10.2 (ArgoCD v3.5.4) |
| kube-prometheus-stack | 92.2.0 (operator v0.94.1, Prometheus v3.15.0, Grafana 13.2.3) |
| bare-metal Prometheus / Grafana | prom/prometheus:v3.15.0 / grafana/grafana:13.2.4 |
| base images | node:24-alpine, rust:1-alpine, alpine:3.24 |
| LAN forwarder | alpine/socat:1.8.0.3 |

## This machine (current setup)

| Piece | How it starts at boot | Restarted on failure by |
| --- | --- | --- |
| minikube cluster `kiln` | `~/.config/systemd/user/kiln-cluster.service` (`deploy/cluster/autostart.sh enable`) | Kubernetes (pods), Docker (nodes) |
| kiln server (in-cluster, ArgoCD app `kiln`) | the cluster | `kiln supervise` (PID 1 in the pod), then Kubernetes |
| bare-metal/bench agent on this host | `~/.config/systemd/user/kiln-agent.service`, user lingering on | `kiln supervise` (restarts, hot-swaps a new `~/.local/bin/kiln`) |
| LAN forwarders `:7070/tcp`, `:7071/udp` → NodePorts | Docker `--restart unless-stopped` | Docker |

Nothing in this setup needs a password, with one exception. `ufw` is active and blocks *other* machines from
reaching 7070/7071. Opening those ports takes root once:

```sh
sudo deploy/cluster/privileged-setup.sh      # opens 7070/tcp + 7071/udp in ufw/firewalld (plus limits, docker on boot)
```

The UI is at <http://localhost:7070>. Sign in with `kubectl --context kiln -n kiln get secret kiln-admin -o jsonpath='{.data.token}' | base64 -d`.
All generated credentials are also in `~/.local/state/kiln/cluster/credentials.env` (mode 0600).

## Cluster capacity protections

* **Honest node sizes.** Under the docker driver every minikube node reports the whole host. The
  bootstrap reserves the difference in each kubelet (`kubelet.system-reserved`), so each node
  advertises its real 8 CPU / 16 GiB and Kubernetes never overcommits builds into OOM kills.
* **The control plane outranks CI.** The `kiln-control` PriorityClass covers the server and
  `kiln-job` covers job pods. `kiln-job` is low, preemptible and never preempts anything. When the
  cluster is full, the scheduler evicts job pods, and kiln requeues those jobs.
* **Job quota.** A `ResourceQuota` in `kiln-jobs` caps CI at 18 CPU / 36 GiB, which leaves headroom
  for ArgoCD, Prometheus and Grafana. kiln retries quota-rejected launches every ~15 s.
* **Pre-pulled toolchains.** The `kiln-prepull` DaemonSet keeps the rust, go, python (uv), node,
  erlang, debian and alpine images on every node, including new ones.
* **Restart-safe.** If the kiln server restarts, running jobs keep going. Their agents reconnect
  and reclaim them, and a job is requeued only if its agent never comes back.
