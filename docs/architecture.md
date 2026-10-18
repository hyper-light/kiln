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
| Containerizable, single distribution | Static musl binary; the UI's static files (TanStack Start's client build) embedded with `rust-embed`; pages and React Server Components rendered by the UI server, which the kiln server runs under Bun (in the image; see [The web UI](#the-web-ui)). |
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

### The web UI

The UI (`ui/`) is a TanStack Start app with React Server Components.
- **Static files:** the kiln server serves the client build (`ui/dist/client`, embedded) itself, and
  keeps earlier builds' assets so pages loaded before a deploy keep working (`crate::ui`).
- **Pages, server functions and server components:** these come from the UI server, TanStack
  Start's server build (`ui/dist/server`) run by Bun (`ui/server/main.ts`). The kiln server starts it
  on a loopback port, forwards the UI's requests to it with streaming both ways, and starts it
  again if it exits (`crate::uiserver`). The UI server stops when kiln does.
- **Without Bun or the UI server's files:** every page is the app's shell (`_shell.html`, rendered
  at build time), and the app runs in the browser.
- **The API stays with the kiln server.** Server functions reach it on its own listener, carrying
  the visitor's cookie.
- **Where it looks:** `KILN_UI_DIR` (default `/usr/local/lib/kiln/ui`) and `KILN_BUN` (default:
  `bun` on the PATH).
- **HTTP/1.1 and HTTP/2:** the kiln server's listener speaks both (`crate::http::server`). Over TLS
  (`server.https`) the browser picks by ALPN. In the clear, HTTP/2 opens with its preface, which is
  how the front door passes a browser's HTTP/2 on. Handlers see every request shaped as HTTP/1.1's:
  `Host` from `:authority`, one `Cookie`, the target as a path. WebSockets stay on HTTP/1.1.

Every page is a route of its own (`ui/src/routes/_app.*.tsx`), rendered in the browser for now.
Pages become server components where that keeps JavaScript out of the browser; the shared plan page
(`routes/shared.plans.$token.tsx`) is the first.

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

## Agent transport

Agents keep one QUIC connection to the server (hyper-transport, mutual TLS: the agent checks the
server's certificate under the fixed name `kiln-server`, the server the agent's, both against the
kiln CA). **No ports beyond the HTTP one are required; UDP is used when reachable.** An agent dials
the QUIC address over UDP first. When two direct dials in a row get no answer (or direct dials go
unanswered for 20 s) it opens the HTTP tunnel instead: a WebSocket to `GET /api/agents/tunnel` on
the server's HTTP port (`ws://`, or `wss://` when the server URL is `https://`), with a UDP socket
on loopback at its end that its QUIC endpoint dials. Each binary message is one datagram; the server
relays them to and from its own QUIC listener from a loopback socket per tunnel (a wildcard
`quic_addr` such as `0.0.0.0:7071` is reached at `127.0.0.1:7071`, `[::]` at `[::1]`, a specific
address as itself).

```text
  agent QUIC ─UDP─▶ relay 127.0.0.1:<any> ═WS /api/agents/tunnel═▶ server relay ─UDP─▶ QUIC listener
```

QUIC runs end to end through the tunnel unchanged, so the tunnel adds no trust: the route is open at
the HTTP level, and bounded instead (256 tunnels at once, 60 new ones a minute per source address,
messages of at most one 1500-byte datagram, closed after 10 idle minutes). While tunnelled, the
agent checks every 5 minutes whether UDP answers again (a 1200-byte datagram of a reserved QUIC
version, which the listener answers with Version Negotiation, no handshake) and switches back when
it does, closing the tunnel once the direct connection is up. It logs `connected directly` or
`connected through the HTTP tunnel (UDP <addr> unreachable)` each time the transport changes.
The agent names the TLS exporter value of the connection the tunnel carries in a text message
(`bind <hex>`), which only the two ends of that connection can know; the server matches it against
its own connections' to show the transport of each agent (`transport` in `GET /api/agents`).

The tunnel costs throughput, not correctness: datagrams ride one TCP connection, so a lost segment
holds up every QUIC stream behind it (head-of-line blocking) and QUIC's congestion control sees TCP's
delays rather than the path's; terminals and job traffic are small enough not to notice. Datagrams a
full socket cannot take are dropped, as a congested link would, and QUIC recovers them.

## Home network: discovery and joining

kiln instances on one LAN find each other, and any computer can become an agent of a server with
one command and an admin's click. Every machine that joins appears on the server's Agents page
with its terminal, metrics and jobs: one pane of glass across the computers in the house.

```
 other computer (kiln join)                 kiln server                           admin (UI)
 mDNS query _kiln._tcp.local ────────────▶  answers: PTR, SRV, TXT, A
 POST /api/join-requests ────────────────▶  keeps the request (memory, 10 min)
   {name, os, arch, labels, CSR, H(secret)}
                     ◀──────────────────── {id, nonce, CA}
 shows the verification code                Runners → Add machines: code ─────▶ same code?
 GET /api/join-requests/<id>, every 2 s ─▶                                   ◀──── Approve
                     ◀──────────────────── {approved, cert, CA, QUIC}
                                            (the CSR signed by the kiln CA, as in enrollment)
 stores the credentials, installs the agent, connects over QUIC + mTLS like any enrolled agent
```

### Announcing and browsing (multicast DNS)

`crates/kiln/src/dns.rs` is the slice of the DNS wire format multicast DNS needs (A, AAAA, PTR,
SRV, TXT, name compression), decoded defensively: names ≤ 255 bytes, labels ≤ 63, compression
pointers only backwards and at most 32 per name, at most 256 records per message.
`crates/kiln/src/discovery.rs` is the responder and browser, written on hyper-rt UDP (no mDNS
crate, no extra threads or runtime):

* **Sockets.** One per address family on UDP 5353, created with `socket2` (`SO_REUSEADDR` and
  `SO_REUSEPORT`, because avahi or mDNSResponder usually own the port too), joined to
  224.0.0.251 and ff02::fb on every LAN interface, then adopted by hyper-rt. Interfaces are the ones
  that are up and multicast-capable, minus loopback, point-to-point links (VPNs) and container or
  VM bridges (`docker*`, `br-*`, `virbr*`, `cni*`, …); `[discovery] interfaces` overrides the choice.
  Interfaces are re-read every 30 s.
* **What is announced.** `_kiln._tcp.local.` with the instance name `kiln <role> on <host> (<id8>)`,
  an SRV record pointing at the HTTP port (0 for agents) on the host name `kiln-<id8>.local.`, its A
  records (AAAA too when the HTTP server listens on IPv6) and a TXT record:

  | Key | Value |
  | --- | --- |
  | `id` | stable random instance id, kept in `<data dir>/instance.id` (agents: their state dir) |
  | `name` | host name |
  | `role` | `server` or `agent` |
  | `version` | kiln version |
  | `quic` | QUIC port (0 for agents) |
  | `ca` | servers: first 16 hex digits of the SHA-256 of the CA certificate |
  | `enrolled` | agents: the same fingerprint of the CA of the server they are enrolled with |
  | `os`, `arch` | e.g. `linux`, `x86_64` |

  Names derive from the random id, so kiln needs no probing or conflict resolution and never claims
  the host's own `.local` name.
* **When.** At start, one and two seconds later, then every 30 s, separately on each interface with
  that interface's addresses. Queries are answered: legacy queries (from a port other than 5353)
  and QU questions by unicast, the rest by multicast after a 20–120 ms delay, at most once a second
  per interface, with known-answer suppression. Goodbyes (TTL 0) go out when the server stops.
* **Browsing.** The server queries at start and folds every response it hears into a table of
  instances (its own packets are recognised by id and ignored). Records carry a TTL of 120 s, so an
  instance that misses four announcements expires; a goodbye removes it at once. Addresses heard on
  several interfaces are merged and lapse one by one. `GET /api/discovery` lists the table and marks
  each instance that already is one of this server's agents: an agent announcing `enrolled` = this
  server's CA, or a machine whose host name one of the server's agents has.
* **Agents.** `kiln agent --discover` announces the agent (role `agent`) while it runs. When it is
  not enrolled yet, it picks the server it hears (asking when there are several and a terminal is
  attached) and goes through the join flow below.

A machine where port 5353 cannot be shared, or with no multicast network, works on without
discovery and retries every minute. `[discovery]` in the config has `enabled`, `announce`,
`browse`, `join_requests` and `interfaces`; everything is on by default.

### Joining (`kiln join`)

Joining is never automatic: a machine asks, an admin decides.

1. `kiln join` browses for three seconds (`--server URL` skips that), lists the servers it heard
   (asking which one when there are several; `--yes` takes the only one), generates a key and a
   CSR locally (the private key never leaves the machine) and a random secret, and sends
   `POST /api/join-requests` with its name, host name, OS, architecture, labels (default
   `[baremetal, <os>, <arch>]`), the CSR and the SHA-256 of the secret.
2. The server keeps the request in memory for 10 minutes and answers with an id, a random nonce and
   its CA certificate. At most 20 requests wait at once and each source address may send one every
   5 seconds (HTTP 429 with `Retry-After` otherwise).
3. The machine polls `GET /api/join-requests/<id>` every 2 s with the secret as a bearer token. The
   first poll confirms the request: from then on admins see it under **Runners → Add machines**,
   with its address and verification code, and can approve or deny it.
4. **The verification code** is six digits taken from SHA-256 over the CSR's public key, the
   server's CA certificate, the machine's secret and the server's nonce; both sides compute it.
   The machine commits to its secret (sends only its hash) before it sees the nonce, and the server
   chooses the nonce after the CSR is fixed, so neither another machine nor a man in the middle can
   make the two codes agree except by a one-in-a-million guess per (rate-limited) attempt. The admin
   approves only when the UI shows the code the machine shows. The machine also refuses to continue
   if the server reports a different code, a different CA, or a certificate for another key.
5. **Approval** signs the CSR with the kiln CA exactly as enrollment does (365 days). The agent's
   name is the requested one unless an agent on another host (or the server's local agent) already
   has it; then it becomes `<name>-2`, `-3`, …, and the agent takes its name from the certificate.
   The answer carries the certificate, the CA and the QUIC address (the address the machine reached
   the HTTP API at, when that is an IP, else the advertised one).
6. The machine stores the credentials in the agent state directory like enrollment
   (`agent.key`, `agent.pem`, `ca.pem`, `quic`, plus `server` and `labels`) and runs the
   `kiln install --agent` logic, so the agent starts now and on every boot. `--no-install` runs the
   agent in the foreground instead.

A request that vanished (server restart, expiry) is submitted again, with a new code. On a machine
with no server configuration, `kiln up` offers to join a server it hears instead of starting a new
one. Without a terminal, `curl -fsSL http://<server>:7070/install.sh | sh -s -- --join` downloads
the binary from the server and runs `kiln join --server <server>`.

**Firewalls.** Discovery needs UDP 5353 open to the LAN on every machine involved, e.g.
`sudo ufw allow from 192.168.1.0/24 to any port 5353 proto udp`. Joining and running as an agent
need only the server's HTTP port (TCP 7070) reachable from the machine; its QUIC port (UDP 7071) is
used when it is reachable too, and agents tunnel QUIC through the HTTP port when it is not (see
[Agent transport](#agent-transport)).
