# kiln

Self-hosted GitOps CI/CD for the [hyper-light](https://github.com/hyper-light) stack. One static
binary contains the server, the agent, a process supervisor, the installer, the CLI and the web
UI. It runs the same way on bare metal and on Kubernetes.

* **GitOps:** kiln polls GitHub with `git ls-remote` (no token, no rate limit) and builds what
  `.kiln/*.yaml` at each commit declares: branches, tags, PR heads, cron schedules, manual runs.
  Pipelines are multi-step DAGs with matrices, conditions, retries, timeouts, artifacts and
  concurrency groups. See [docs/pipelines.md](docs/pipelines.md).
* **Runs anywhere:** `runs-on: [k8s]` gets a fresh pod per job, with build caches on a PVC.
  `runs-on: [baremetal, bench]` runs on real hardware; `exclusive: true` keeps benchmarks quiet.
* **hyper-raft transport:** agents talk to the server over hyper-transport QUIC with mutual TLS
  from kiln's own CA. Agents enroll by CSR, so private keys never travel. Control, terminal and
  log traffic use separate prioritised lanes.
* **Full machine control:** real PTY terminals in the browser on the server and on every agent.
  Resize, alt-screen and TUIs (k9s, htop, vim) all work.
* **Observability:** Prometheus `/metrics` (host stats read straight from `/proc`, sampled every
  5 s), Grafana dashboards, alert rules, live UI charts. No sidecars, nothing paid.
* **Never down for long:** no `unwrap`/`expect`/`panic`/indexing (enforced by clippy).
  Recovery happens at several levels:
  * subsystems restart in-process, Erlang style;
  * `kiln supervise` restarts the process with backoff, reaps zombies (works as PID 1) and
    hot-swaps a new binary;
  * jobs are requeued when an agent or the server dies;
  * orphaned build processes are reaped on restart.

## Quick start (bare metal)

```sh
cargo build --release              # after building the UI: npm --prefix ui ci && npm --prefix ui run build
./target/release/kiln install      # ~/.local/bin, ~/.config/kiln/kiln.toml, start on boot, start now
kiln login                         # opens the UI with a one-time link; no password to type
```

`kiln install --unattended` never prompts. `kiln install --system` installs system-wide (asks for
sudo; add `--unattended` to use `sudo -n` only). The server runs a local agent by default, so
`runs-on: [baremetal]` jobs work immediately.

To add another machine, such as a benchmark box, copy the command from **Agents → Add agent** in
the UI:

```sh
curl -fsSL http://<server>:7070/install.sh | sh -s -- --token <join-token> --labels baremetal,bench
```

## Kubernetes (multi-node, ArgoCD, Prometheus, Grafana)

```sh
deploy/cluster/bootstrap.sh --unattended   # 3-node minikube + ArgoCD + kube-prometheus-stack + kiln
deploy/cluster/port-forward.sh             # kiln :7070 · ArgoCD :8080 · Grafana :3000 · Prometheus :9090
```

See [deploy/README.md](deploy/README.md). Bare-metal agents join the in-cluster server through
the `kiln-external` service (UDP 7071 + TCP 7070).

## Layout

| Path | What |
| --- | --- |
| `crates/kiln` | The binary: `server`, `agent`, `supervise`, `install`, `login`, `validate`, … |
| `ui/` | React + Tailwind dashboard in the hyperlight-site design language, embedded at build time |
| `deploy/` | Dockerfile support, Helm chart, ArgoCD apps, Grafana dashboards, alert rules, cluster scripts |
| `docs/` | [architecture](docs/architecture.md), [API](docs/api.md), [pipelines](docs/pipelines.md) |
| `.kiln/ci.yaml` | kiln's own pipeline |
