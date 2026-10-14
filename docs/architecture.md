# kiln — architecture

kiln is a self-hosted, GitOps CI/CD job server for the hyper-light stack. One static binary carries
the server, the agent, the process supervisor, the installer, the CLI and the embedded web UI. It
runs the same way on bare metal and on Kubernetes.

## Goals

| Requirement | How kiln meets it |
| --- | --- |
| GitOps, pulls from GitHub | Server polls registered repositories (conditional requests, ETag; 304s are free against the rate limit) and optionally accepts webhooks. Pipelines live in the repo at `.kiln/*.yaml`. |
| Rust / Python / Go / TypeScript (+ Erlang) | Toolchain presets pick images and caches; steps are plain shell so anything installed works. |
| Heavy concurrent load | Async (tokio) scheduler, bounded per-agent slots, per-repo concurrency groups, back-pressure. Logs go to disk segment files, never through the database. |
| In-cluster **and** on-hardware jobs | One executor: the `kiln agent`. On bare metal it is a long-lived daemon; on Kubernetes the server launches an ephemeral agent pod per job. Jobs choose with `runs-on` labels (`k8s`, `baremetal`, `bench`, `gpu`, …). |
| hyper-raft QUIC + TLS | Agent⇄server and CLI⇄server traffic runs over hyper-raft's QUIC transport with mutual TLS from hyper-tls. Server state replication across replicas uses hyper-raft consensus when enabled. The browser uses HTTPS + WebSocket (browsers cannot open raw QUIC). |
| Rich, cheap telemetry | `/metrics` Prometheus endpoint; host/job samples read straight from `/proc` and cgroup files at a configurable low rate; agents ship samples as QUIC datagrams. No sidecars, no agents-of-agents. |
| Grafana / Prometheus / ArgoCD | `deploy/` holds a Helm chart for kiln, a ServiceMonitor, Grafana dashboards as ConfigMaps, and ArgoCD Applications for the whole stack. |
| Full machine control from the UI | PTY-backed terminal sessions (xterm.js + WebGL), full TTY semantics: resize, raw mode, alt-screen TUIs (k9s, htop, vim). |
| Starts on boot, self-healing without systemd/K8s | `kiln supervise` is a tiny parent process that restarts the server with exponential backoff and reaps zombies (works as PID 1). Boot hook: systemd unit if present, else `@reboot` crontab, else XDG autostart. Inside the server, every subsystem is a supervised task that is restarted on error (Erlang-style). |
| Never panic | Workspace lints deny `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, unchecked indexing. Task boundaries additionally use `catch_unwind` as a last line of defence. |
| Containerizable, single distribution | Static musl binary; UI built by Vite and embedded with `rust-embed`; `Dockerfile` is `FROM scratch`-style (distroless). |
| Install with or without passwords | `kiln install` is interactive by default; `--unattended` never prompts (uses `sudo -n` or user-level install only, generates secrets itself and writes them `0600`). |
| No paid services | Everything here is open source and self-hosted. |

## Components

```
                   ┌──────────────── kiln server ────────────────┐
 GitHub ◀─poll/ETag─┤ gitops   scheduler   store(SQLite/raft)     │
        ──webhook──▶│ api(HTTP+WS)  terminal(PTY)  metrics  logs  │
                   └───────▲──────────────▲──────────────▲───────┘
                     HTTPS/WS        QUIC + mTLS        QUIC + mTLS
                         │               │                  │
                    browser UI     kiln agent (bare     kiln agent pod
                    (embedded)     metal, bench box)    (per job, k8s)
```

### Pipeline definition (`.kiln/*.yaml`)

```yaml
name: ci
on:
  push: { branches: [main, "release/*"] }
  pull_request: {}
jobs:
  test:
    runs-on: [k8s]
    toolchain: rust            # preset image + cargo cache
    matrix: { profile: [dev, release] }
    steps:
      - run: cargo test --workspace --profile ${{ matrix.profile }}
  bench:
    needs: [test]
    runs-on: [baremetal, bench]
    steps:
      - run: cargo bench -- --save-baseline ${{ git.sha }}
    artifacts: [target/criterion/**]
```

### Run lifecycle

`queued → scheduled → running → (succeeded | failed | cancelled | timed_out)`. The scheduler holds
a ready queue keyed by labels; an agent offers capacity (`slots`) over its QUIC session and the
scheduler assigns the oldest matching job. A lost agent (missed heartbeats) requeues its jobs up to
`retries`.

### Fault tolerance layers

1. **Process**: `kiln supervise` restarts the server with jittered exponential backoff and a crash
   log; a healthy period resets the backoff.
2. **Subsystem**: in-process supervisor restarts failed tasks (poller, scheduler, listeners).
3. **Job**: retries, agent-loss requeue, timeouts, cancellation that kills the whole process group.
4. **State**: SQLite in WAL mode with `synchronous=NORMAL`; recovery on start marks orphaned runs.
