# kiln dev cluster (minikube + ArgoCD)

A local, multi-node Kubernetes cluster that runs kiln exactly the way production would: GitOps
via ArgoCD, Prometheus/Grafana/Alertmanager via kube-prometheus-stack, and job pods scheduled
across several nodes. Everything is free and self-hosted.

## Scripts

| Script | Root? | What it does |
| --- | --- | --- |
| `privileged-setup.sh` | **yes, once** | docker group for your user, docker on boot, inotify/fd/socket-buffer sysctls, optional `/usr/local/bin/kiln` |
| `bootstrap.sh` | no | creates/starts the cluster, builds + loads images, deploys the git server, installs ArgoCD, applies the root app, waits for health, prints URLs + credential locations. Idempotent. |
| `port-forward.sh` | no | forwards kiln :7070, ArgoCD :8080, Grafana :3000, Prometheus :9090 to localhost |
| `expose-lan.sh up\|down\|status` | no | forwards this host's 7070/tcp + 7071/udp to the kiln NodePorts so bare-metal agents on other machines can join |
| `teardown.sh [--stop\|--yes\|--purge]` | no | stop or delete the cluster |

```sh
sudo deploy/cluster/privileged-setup.sh          # once; asks for your sudo password
deploy/cluster/bootstrap.sh                       # interactive
deploy/cluster/bootstrap.sh --unattended          # CI / scripted: never prompts
deploy/cluster/port-forward.sh                    # open the UIs
```

If the docker socket is not accessible, `bootstrap.sh` tells you exactly why and what to run.
When your user is already in the `docker` group but the current login session predates it,
`bootstrap.sh` re-executes itself under `sg docker` / `newgrp docker` automatically, so no re-login
is needed.

Fish users: the scripts are bash and run fine from fish (`./deploy/cluster/bootstrap.sh`).

## Layout

```
 minikube profile "kiln" — docker driver, 3 nodes (kiln, kiln-m02, kiln-m03)
 sizes derived from the host (3/4 of CPUs, ~80% RAM ÷ nodes; ≤16 CPU / 16 GiB per node)
 addons: metrics-server, storage-provisioner-rancher (local-path)
 ┌──────────────────────────────────────────────────────────────────────────────┐
 │ argocd       ArgoCD (argo-cd chart 10.10.2 / ArgoCD v3.5.4), app-of-apps root │
 │ kiln-git     read-only git smart-HTTP server (nginx+fcgiwrap+git-http-backend)│
 │ kiln         kiln server Deployment (Recreate) + PVC + Services               │
 │ kiln-jobs    ephemeral job pods (one per job) + kiln-cache PVC                │
 │ monitoring   kube-prometheus-stack 92.2.0 (Prometheus, Alertmanager, Grafana) │
 └──────────────────────────────────────────────────────────────────────────────┘
```

ArgoCD applications (sync waves keep the order):

| Wave | Application | Source |
| --- | --- | --- |
| – | `kiln-root` | `deploy/argocd/apps` (Helm chart rendering the children) |
| -2 | `kiln-namespaces` | `deploy/argocd/namespaces` (no pruning) |
| -1 | `kube-prometheus-stack` | prometheus-community chart 92.2.0 + `$values/deploy/argocd/values/kube-prometheus-stack.yaml`, ServerSideApply |
| 0 | `kiln` | `deploy/helm/kiln` + `deploy/argocd/values/kiln.yaml` |

`bootstrap.sh` adds an `argoproj.io/Application` health check to `argocd-cm`, so a wave only
starts once the previous child Application is Healthy (CRDs before ServiceMonitors).

### Storage

* `local-path` (rancher provisioner) for state that must stay with one node: kiln data,
  git server repo, Prometheus TSDB. The PV carries node affinity, so the pod follows its data.
* `standard` (minikube hostpath, default class) for the `kiln-cache` build cache. It accepts
  `ReadWriteMany`, so job pods on every node can mount it; physically each node keeps its own
  directory, which is what a build cache wants (no cross-node locking).

### Images

`bootstrap.sh` builds `kiln:dev` (top-level `Dockerfile`) and `kiln-git:dev`
(`deploy/gitserver/Dockerfile`) with your local docker and runs `minikube image load` (every
node). Pods use `imagePullPolicy: IfNotPresent`. Re-running `bootstrap.sh` rebuilds, reloads and
restarts the kiln Deployment.

## The git source: in-cluster git server

The kiln repository has no public remote yet, but ArgoCD needs a git URL. `deploy/gitserver/`
is a ~40 MB Alpine image serving bare repositories read-only over **smart HTTP**
(`git-http-backend` behind nginx + fcgiwrap; protocol v0/v1/v2, shallow clones) — the transport
ArgoCD supports natively for `http(s)://` URLs, with no credentials:

    http://kiln-git.kiln-git.svc.cluster.local/kiln.git

`deploy/gitserver/push.sh` publishes the local repo by streaming a `git bundle` through
`kubectl exec` (so publishing requires cluster access, not a git password; HTTP pushes are
refused). By default it publishes a **snapshot of the working tree** — committed and uncommitted
changes, honoring `.gitignore` and always excluding `target/`, `node_modules/`, `ui/dist/` —
built with a temporary index, so your branch, index and stash are untouched. Use
`KILN_GIT_PUSH_MODE=head` to publish only committed `HEAD`. After publishing it asks ArgoCD to
refresh immediately.

```sh
KUBE_CONTEXT=kiln deploy/gitserver/push.sh     # re-publish after editing manifests/values
```

### Switching to GitHub later

The repo URL is one value. Re-run bootstrap with it — the root Application passes it to every
child Application:

```sh
KILN_REPO_URL=https://github.com/hyper-light/kiln KILN_REPO_REVISION=main deploy/cluster/bootstrap.sh --unattended
# or by hand:
sed -e 's|__REPO_URL__|https://github.com/hyper-light/kiln|' -e 's|__TARGET_REVISION__|main|' \
  deploy/argocd/root.yaml | kubectl --context kiln apply -f -
```

With a non-default URL, bootstrap skips the in-cluster git server (you may then delete the
`kiln-git` namespace). For a private repo add `username`/`password` (a read-only fine-grained
PAT) to `deploy/argocd/bootstrap/repo-secret.yaml` before applying.

## Credentials

All generated, never prompted for:

| What | Where |
| --- | --- |
| kiln admin token | secret `kiln/kiln-admin` key `token` |
| Grafana `admin` password | secret `monitoring/grafana-admin` key `admin-password` |
| ArgoCD `admin` password | secret `argocd/argocd-initial-admin-secret` key `password` |
| all of the above | `${XDG_STATE_HOME:-~/.local/state}/kiln/cluster/credentials.env` (mode 0600) |

## Configuration knobs (env)

`KILN_MINIKUBE_PROFILE` (kiln), `KILN_NODES` (3), `KILN_NODE_CPUS`, `KILN_NODE_MEMORY_MB`,
`KILN_NODE_DISK` (80g), `KILN_K8S_VERSION` (stable), `KILN_REPO_URL`, `KILN_REPO_REVISION`
(main), `KILN_WAIT_TIMEOUT` (1200 s), `KILN_IMAGE` (kiln:dev). Node count/size only apply when
the cluster is created; `bootstrap.sh` adds worker nodes if you raise `KILN_NODES` later.
