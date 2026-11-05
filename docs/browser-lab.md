# The browser lab

How kiln measures what a browser gets from it over a slow, lossy link, against the best HTTP/2 and
HTTP/3 servers: bytes, server CPU and server-event-to-client time for each operation, at p50, p99
and p99.9, for streams, bursts and requests, on a good link and at 100 and 50 kbit/s, at 0%, 1% and
5% loss. Goal 1 of the browser protocol program built it: goal `a48d593b61a389a8a827b577eb5d5f9f`,
plan `01a1a832e9cc76f79fdd303f52bc658b`, research `01a1a832e9d47513bb663520611a119e`. It is part
of `kiln-lab` (`crates/kiln-lab`, `kiln-lab browser`); [runtime-lab.md](runtime-lab.md) is the
same lab's server-side half, measuring kiln over loopback with Rust clients.

## The machine

The lab runs on Linux, as the person who started it, never root: `kiln-lab browser` runs itself
again inside a user namespace of its own (its uid mapped to root there, and only there), with
network, mount and PID namespaces of its own (`crates/kiln-lab/src/browser/topo.rs`). The PID
namespace takes every process the lab started with it when it ends, so nothing outlives a lab.
Inside:

- the **link** is the lab's own network namespace: it routes between the others and shapes each
  client's link;
- the **server** namespace holds every server a round measures (kiln and its runner, or a reference
  server and its origin), at `10.77.0.1`;
- **K client** namespaces, one browser each, at `10.78.<i>.2`.

Each is joined to the link by a veth pair of MTU 1500, offloads off (netem counts and loses whole
packets, so a 64 KB segment mustn't pass as one), IPv6 off and every neighbour pinned (no router
solicitation or ARP draws on a seeded loss). The link shapes each client's downlink with netem on
its egress toward the client, and its uplink with netem on an ifb its ingress from the client is
redirected to. Nothing is shaped on loopback or on a host's own sending side (netem's own caveat:
research f17). Each client also has an unshaped **control pair** straight to the server namespace
(`10.79.<i>.1`), which carries only the clock's calibration. The links are independent, so K
browsers gather samples at once.

Every server runs in a cgroup scope of its own under the lab's cgroup; its CPU is its scope's
`cpu.stat`, which counts every process in it whether or not the cpu controller is on (f37).

`kiln-lab browser probe` checks each capability in turn and stops at the first one missing, naming
it: Linux, an unprivileged start, `unshare`, `nsenter`, `ip`, `tc`, user namespaces; then, inside,
the namespaces and veths, netem with the ifb and its redirect on a real link, cgroup scopes, node
and Playwright, and Chromium and WebKit (Firefox too, for goals 4, 5 and 7) starting in a client's
namespace. A machine whose `tc` isn't installed can point `KILN_LAB_TC` at one unpacked under
`~/.cache` (its distribution tables in `TC_LIB_DIR`).

## The profiles

The profiles are data (`crates/kiln-lab/src/browser/link.rs`, `profiles()`); `kiln-lab browser
profiles` prints them and, with `--profile P --loss L`, the commands that make one.

| profile | in the bar | each way | base RTT | jitter (σ, each way) | queue | netem limit |
| --- | --- | --- | --- | --- | --- | --- |
| good | yes | 100 Mbit/s | 20 ms | none | 100 ms at the rate | 909 packets |
| 100k | yes | 100 kbit/s | 800 ms | 50 ms, normal | 4 s at the rate (50 KB) | 37 packets |
| 50k | yes | 50 kbit/s | 800 ms | 50 ms, normal | 4 s at the rate (25 KB) | 19 packets |
| 50k-asym | no | 50 down, 30 up | 800 ms | 50 ms | 4 s | 19 packets |
| good-udp-blocked, 50k-udp-blocked | no | as good, 50k | | | | every UDP packet dropped both ways (goal 5) |
| good-udp-dropped | no | as good | | | | UDP dropped from 5 s into the round (goal 5) |

Loss is 0%, 1% or 5%, each packet on its own, in each direction (`loss random`), drawn from a seed
each round (netem's `seed`): the downlink takes the round's seed, the uplink its own from it, and
client `i` the round's plus `i`. Every target of a round meets the same draws in order (f16). A
Gilbert-Elliott model with the same mean loss (`ge1`: bursts of 3 packets) is there beside the bar.

Why these: the 50 and 100 kbit/s links are Ada's, below every cellular profile of WebPageTest (its
2G is 280/256 kbit/s at 800 ms: f25), with its slow profiles' ~800 ms RTT; cellular links buffer
deeply (f26), hence a queue of seconds. netem with a rate sends each packet at the later of its own
delay and its predecessor's departure, plus its time on the wire, so nothing reorders, and it holds
`limit` packets in all, delayed or queued: `limit` is the one-way delay plus the queue, in full
packets, plus one (`Way::limit`). Small packets (ACKs) take a slot each, as packet-counting queues
do.

Left out of the bar's cells unless Ada adds them: Gilbert-Elliott bursts, a radio's promotion delay
after idle (netem has no idle-triggered delay: f27), an asymmetric uplink, and BBR (it needs root to
load `tcp_bbr`: f21); TCP is CUBIC everywhere, for kiln and every baseline.

### The profiles' checks

`kiln-lab browser check-profiles --out DIR` measures each profile of the bar on its own client link,
all at once (so it also shows the links don't disturb each other), before any protocol is measured:

- throughput: one TCP bulk transfer from the server to the client for 70 s, the client's received
  frames over 60 s from 5 s in, as a share of the set rate (95% to 101% passes);
- RTT: 1,000 small UDP pings on the idle link (20 ms apart on the good link, 100 ms on the slow
  ones), their median against the base RTT (within 5% passes);
- a full queue: full frames at twice the rate for 60 s, the last third's one-way delay less an idle
  full packet's, against the set queue (within 10% passes);
- loss: 100,000 numbered packets each way on the good link at 1% and 5%, inside the 99% binomial
  band of the set rate; twice with one seed (the same indices lost both times), once with another
  (other indices, at the same rate).

<!-- profile checks: begin -->
On 2026-11-04 (`docs/browser-lab/2026-11-04/profile-checks.md`, seed 1):

| profile | throughput (% of rate) | median RTT (ms) against base | off | full queue (ms) against set | reordered |
| --- | --- | --- | --- | --- | --- |
| good | 100.0 | 20.1 against 20 (1000 pings) | 0.5% | - | 0 |
| 100k | 99.9 | 818.3 against 800 (1000 pings) | 2.3% | 3912 against 4000 | 0 |
| 50k | 99.7 | 827.4 against 800 (1000 pings) | 3.4% | 3873 against 4000 | 0 |

| loss | way | seed | lost of 100,000 | 99% band | same indices as seed's first run | shared with the first seed's | runs again (lab-side drops) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1% | down | 1 | 975 | 918 to 1082 | - | - | 0 |
| 1% | down | 1 | 975 | 918 to 1082 | yes | - | 0 |
| 1% | down | 6364136223846793006 | 1070 | 918 to 1082 | - | 9 | 0 |
| 1% | up | 1 | 982 | 918 to 1082 | - | - | 0 |
| 1% | up | 1 | 982 | 918 to 1082 | yes | - | 0 |
| 1% | up | 6364136223846793006 | 989 | 918 to 1082 | - | 6 | 0 |
| 5% | down | 1 | 4939 | 4822 to 5178 | - | - | 0 |
| 5% | down | 1 | 4939 | 4822 to 5178 | yes | - | 0 |
| 5% | down | 6364136223846793006 | 5031 | 4822 to 5178 | - | 277 | 0 |
| 5% | up | 1 | 5024 | 4822 to 5178 | - | - | 0 |
| 5% | up | 1 | 5024 | 4822 to 5178 | yes | - | 0 |
| 5% | up | 6364136223846793006 | 4942 | 4822 to 5178 | - | 238 | 0 |

Every check passed.
<!-- profile checks: end -->

## The operations

`kiln-lab browser record` starts the lab kiln on a good client link (kiln at origin/main
`01d3ff1c99b`, its own TLS listener with the lab chain, ALPN h2 and http/1.1, no front door; a
runner whose `claude` is the lab's stand-in; its terminal on, the shell a stand-in writing stamped
lines; a lab repository whose one job's step writes stamped lines; three sessions of 200 events
each, two of them at work as agents are, a turn of 40 events 25 ms apart every 3 s). Chromium then
opens kiln's own pages for each journey (Mission control, an agent's page while it works and is sent
a message every 10 s as its composer sends one, a terminal, a job's page and its log, a session's
snapshot), 600 s each, and the browser's own network events (Chrome DevTools Protocol: every request
with its sizes, every socket and each frame) are written down with their payloads
(`crates/kiln-lab/web/record.mjs`). `crates/kiln-lab/web/catalogue.mjs` makes them into the
catalogue, [`docs/browser-lab/catalogue.json`](browser-lab/catalogue.json) (sizes, rates and
digests, small), and the replay the reference servers serve (payloads under `~/.cache`). Where the
UI's pages don't open a stream themselves, the journey opens it as the UI's view does (the
terminal's socket, the log's socket), on the same path with the same parameters.

What the UI does, from the catalogue (sizes in bytes above the transport, rates as recorded):

| case | operation | sizes p50 / p99 | rate |
| --- | --- | --- | --- |
| stream | an agent session's socket (`/api/harness/sessions/:id/ws?z=gzip&patch=state&patch=changes`): events, state and changes patches | 687 / 15,966 a frame | 37 frames a second while two agents work |
| stream | `/api/events`: whole entities (a busy agent's entry on almost any change) | 999 / 1,083 a frame | 31 a second |
| stream | a terminal's socket: PTY output | 97 / 97 a frame | 10 a second (the stand-in's pace) |
| stream | a job log's socket: the step's output | 97 / 97 a frame | 20 a second (the stand-in's pace) |
| burst | an agent's turn, on its session's socket | 60 / 126 frames, 167 KB / 377 KB a burst, over 0.9 / 2.4 s | 0.33 a second |
| burst | agents changing at once, on `/api/events` | 60 / 102 frames, 61 KB / 104 KB a burst | 0.32 a second |
| request | `GET /api/agents` | 2,043 / 2,052 | |
| request | `GET /api/overview` | 6,500 / 6,506 | |
| request | `GET /api/harness/sessions` | 2,911 / 3,014 | |
| request | `GET /api/me` | 1,254 / 1,254 | |
| request | `GET /api/metrics/history` | 114,104 / 171,477 | |
| request | a session's snapshot (the session socket's first, packed frame) | 715 KB unpacked for a session of 200 events | |
| request | `POST /api/harness/sessions/:id/command` (a message to an agent) | 156 / 156 answered, 57 sent | |

Every request rode HTTP/2; every socket was an HTTP/1.1 connection of its own. Left out, with why:
pages, the UI server's server functions and static files (`GET /`, `/agents/:id`, `/runs/...`, 121
files under `/assets`): page loads are goal 8's.

Two recordings on one stand-in seed (`KILN_LAB_SEED=7`) send the same event bodies once their
stamps and ids are taken out (`node catalogue.mjs compare`): of the 2,773 events both 150-second
recordings sent (keyed by their turn and place in it), 2,773 hashed equal. A run uses the same seed,
so the baselines replay exactly what kiln sends.

At the recording's pace, `/api/events` alone carries about 250 kbit/s: five times a 50 kbit/s link
before any page, session or request. The matrix's paces are a link's half at most (below).

## The probe and the three measures

The probe page (`crates/kiln-lab/web/probe/probe.js`) is lab-only. The driver
(`crates/kiln-lab/web/job.mjs`) serves it on the target's own origin through Playwright's routing,
then stops routing, so it runs same-origin as the UI does, and nothing it asks for afterwards passes
through Playwright. It opens what the UI opens for each operation and reads frames as the UI does
(JSON.parse of each text frame, gunzip of a packed snapshot). Its carrier is the target's: WebSocket
and fetch for kiln today; Server-Sent Events, streaming fetch and fetch for the baselines.

- **Time.** A stream's or a burst's operation runs from its server stamp (`@lab:<µs>`, written as
  the stand-in agent, terminal or log step wrote it, or as the baselines' origin sent it) to the
  page's handler having the parsed frame. A request runs from its scheduled time to its parsed
  body: requests go on a fixed open-loop schedule, each at its time whether or not those before it
  were answered, so a stall counts in full for every request behind it (no coordinated omission:
  f35). The page's clock (`timeOrigin + now()`, cross-origin isolated, so 5 µs fine) is calibrated
  against the server's each job by 200 WebSocket echoes over the unshaped control pair (to the
  lab's control echo, `crates/kiln-lab/web/control.mjs`), taking the offset of the echo with the
  least round trip (f29). Its residual is the calibration taken again the same way, in 10 groups of
  20 more echoes: how far each group's offset lies from the one the page uses (each echo's own
  offset, off by up to half its round trip, is kept beside it). Chromium runs with Local Network
  Access off (`--disable-features=LocalNetworkAccessChecks,LocalNetworkAccessChecksWebSockets`):
  the control echo is on another private address than the page's origin, which Chromium would
  otherwise hold for a permission; the target's own operations are all same-origin.
- **Bytes.** The client link's frames both ways (the client's own `eth0`, every header, ACK and
  retransmission), read every window (1 s) by the driver from its namespace's `/proc/net/dev`. Each
  window's bytes go to the operations that completed in it; windows with none are left out; the
  percentiles are across windows, and the mean is all bytes over all operations. The bytes above
  the transport (what the page was given) are a mean beside it.
- **Server CPU.** The server's scope (and, for the reference servers, their origin's scope, counted
  with it and shown apart) read every window, less the background's rate, over the window's
  operations (every client's); percentiles across windows, and the mean. The background is the
  server with nobody attached: once it is at rest (a server just started is still settling in: two
  5 s windows in a row within a quarter, or 2 ms a second, of each other, 90 s at most), two windows
  before the job, and one after it; its rate is the mean of before and after, and the larger of the
  two windows' difference and the before's and after's is the CPU measure's noise floor, kept with
  every job. kiln's runner (the agents' machine) is in a scope of its own and isn't the server's.

### The probe's checks

`kiln-lab browser check-probe RUN --catalogue CATALOGUE` reads a run made with `--capture` (a packet
capture on each client link, `crates/kiln-lab/web/capture.py`: a raw packet socket in the client's
own namespace, every frame's length and time, written as pcap with the browser's TLS keys beside it
for goal 7) and `--stall 10` (the server stopped, every process of it, for 2 s, 10 s into each
request job). On 2026-11-04 (`~/.cache/kiln-bp/g1/check4`, the good link, kiln today, h2o over
HTTP/2 and Caddy over HTTP/3, 2 links, 30 s jobs):

- calibration: the residual's p99, worst job 20.5 µs, median job 14.2 µs (within 200 µs); in the
  pilot's first jobs at K = 8, on the lab machine at a load of about 67 with no core idle, the worst
  job's was 232 µs and the median job's 75 µs: on a busy machine eight browsers' echoes wait for a
  core, so the matrix's good-link times carry up to a quarter of a millisecond of clock error;
- capture against counters: 0.00% apart in all six jobs (2.1 MB to 15.8 MB each);
- the probe's sizes above the transport against the real UI's, where the UI's payloads were the
  same each time: `GET /api/agents` 1.4%, `GET /api/me` 0.0%, `GET /api/overview` 0.1%, the
  terminal's frames 0.1% apart; the message to an agent's answer was 3.7% apart with one id for
  every message, and 8.3% apart (169 bytes against 156) with a new id each time as the UI sends
  them. Not yet explained: the answer most likely follows the agent's state (the run's agent gets
  a message every 700 ms, the recording's every 10 s), which would make it a payload the two don't
  share; until that is read from the bodies, this check stands unmet; operations whose payloads follow the lab kiln's state (the sessions' list, the
  notices, the metrics' history, `/api/events`, a session's frames) aren't compared;
- the stall: the longest request waited 2,156 ms (kiln today), 2,097 ms (h2o) and 2,001 ms (Caddy)
  from its scheduled time: the whole wait counted;
- the CPU measure's noise floor: kiln today 2,887 µs of CPU a second, h2o 8.2, Caddy 17.6.

`kiln-lab browser start-stop --times 20` started and stopped each reference target 20 times (h2o
and Caddy over HTTP/2 and HTTP/3) and kiln today 20 times: each came up on its port (and its UDP
port for HTTP/3) every time, and each stop left none of its recorded processes and nothing in its
scopes. Every static body each reference server served (14 of 14) came back byte for byte as the
catalogue holds it.

A target is a record (`crates/kiln-lab/src/browser/targets.rs`): its server, its port, whether it
is HTTP/3, its carrier for streams, the protocol the page must have got, and the browsers it runs
in. Each job's record says which protocol each path came over (`nextHopProtocol`).

## The baselines

Two reference servers, each a lab program pinned by version and digest and never a kiln dependency
(f40): their fetch and build are `crates/kiln-lab/web/servers/fetch-caddy.sh` and `build-h2o.sh`,
their configs are written by `crates/kiln-lab/src/browser/servers.rs` (`h2o_conf`, `caddy_conf`)
into each round's folder.

| server | version | digest (sha256 of the binary) | HTTP/3 |
| --- | --- | --- | --- |
| h2o | 2.3.0-DEV at commit `1b5ef77eb` (built against the machine's OpenSSL 3.5, zlib, brotli 1.2.1) | `8b2d8a231345a9d3ad19fb0bd0321acf0c204dffeab454cdcd7983484b775ad8` | quicly, in the same server (f32) |
| Caddy | 2.10.2 (release tarball, its published SHA-512 checked) | `4ef1f68c70219536b2711fd16547a79841a2dec2d6b4e56b1e3e5e9da76028e6` | quic-go (f33) |

Each serves the catalogue's operations with kiln's own bodies:

- **requests** as static files from the catalogue's docroot (`payloads/docroot`, each read's body as
  the lab kiln answered it, each at its own path with no redirect; a session's snapshot, unpacked,
  at `/api/snapshots/lab`), the server's own best path, compressed as each does best (h2o: brotli,
  then gzip; Caddy: zstd, then gzip), `cache-control: no-store` as kiln's API answers;
- **streams and bursts** as Server-Sent Events, or as a streaming fetch (one frame a line), from the
  lab's origin (`crates/kiln-lab/web/origin.mjs`) over a Unix socket: it replays the frames kiln
  sent in step 3's recording, in order, each stamped as it goes out (the stamp keeps its length, so
  every frame is the size kiln's was), on the schedule kiln's stand-ins keep; the stream is
  compressed by the server as its static files are. The origin runs in a scope of its own: its CPU
  counts with the server's and is shown apart;
- **the write** (a message to an agent) passed to the origin, which answers what kiln answered.

The tuning, and why:

- TLS 1.3 only, with the lab kiln's chain (one ECDSA P-256 leaf under a lab CA), so the handshake's
  certificate bytes are the same for every target.
- TCP's congestion control is CUBIC for every target, kiln today's included: BBR needs root on the
  lab host to load `tcp_bbr` (f21), which is Ada's to allow; the default qdisc on the server's veth
  is the kernel's own for every target.
- QUIC's congestion control: h2o's `cc` set to Cubic (h2o defaults to Reno; its choices are Reno,
  Cubic and Pico: f32); Caddy's is quic-go's Cubic (quic-go has no BBR: f33). The pilot (step 7)
  measures h2o's Reno against Cubic at 50 kbit/s with 1% loss and keeps the lower p99.
- Flow-control windows: each server's own defaults, unchanged (h2o's HTTP/2 and quicly's, Go's
  net/http2 and quic-go's): the lab's operations are small and its rates at most half a link, so no
  window is what holds a stream back.
- One thread for h2o (`num-threads: 1`) and Caddy's default `GOMAXPROCS` (the machine's CPUs): each
  server's CPU is its cgroup's whatever its threads.
- h2o runs in a user namespace of its own nested in the lab's, as uid 1000: started as root it would
  try to become `nobody`, which the lab's namespace doesn't map.
- No WebSocket over HTTP/2 (RFC 8441): neither h2o nor Caddy's file and proxy paths here serve it to
  every browser (f34), so the baselines' streams are what every HTTP/2 and HTTP/3 server serves in
  every browser.
- HTTP/3 rows run in Chromium only: WebKit on Linux has no HTTP/3 (f30). Chromium reaches them with
  `--origin-to-force-quic-on=10.77.0.1:<port>` and the leaf's key hash in
  `--ignore-certificate-errors-spki-list`, since it takes no locally added root for QUIC (f31); the
  HTTP/2 rows run with `--disable-quic`, so no Alt-Svc moves them. Each job's record says which
  protocol every path came over (`nextHopProtocol`).
- kiln today is measured on its own TLS listener (`server.https`, ALPN h2 and http/1.1, the same
  chain), without the front door (f7): its requests ride HTTP/2, and each of its sockets is an
  HTTP/1.1 connection of its own (kiln has no extended CONNECT).

## The statistics, the run plan and the decision rule

Written before the first measured round of the matrix (step 6), and held to by every number below.

- **A round** is one draw: each target started fresh, every target on the same netem seeds, the
  targets' order turning round by round. Each condition gets at least 5 rounds.
- **A number** is the median of its rounds' quantiles, with the interval that median moves in when
  the rounds are drawn again (kiln-lab's bootstrap, 95%: f1), and the count of operations it rests
  on. Each p99.9 of time also carries its within-round binomial band (the samples at the ranks of a
  95% interval for the 99.9th percentile of that round's n: f23, f36).
- **Samples.** Each operation class has a floor of 10,000 samples per condition. Where the planned
  hours don't fit the budget, the costliest cells fall to 3,688, the least that bounds a p99.9 at
  all, and the plan lists them.
- **K and the hours** come from `kiln-lab browser plan --k K`: each case's pace on each profile
  (`runfile::workload`), the samples a round gathers on K links, the rounds the floor takes, and
  the hours each job and each server's start cost.
- **A cell** is one case × link × loss × browser × measure, judged at each of p50, p99 and p99.9.
- **The best baseline** in a cell is the lowest median among the HTTP/2 and HTTP/3 rows and kiln
  today.
- **Goal 10's rule.** A cell meets 1/10 when the whole 95% interval of kiln's value over the best
  baseline's lies at or below 0.1; every cell is reported either way. Goal 10's step 1 adds the
  allowed causes for a miss and freezes the rule.
- **Two sets of rounds compared** (the A/A, the K-links check, a rerun) differ in a number only
  where that number's interval, held at a level that keeps every number compared together at 95%
  (Bonferroni), leaves out none (`kiln-lab browser compare`).

<!-- run plan: begin -->
`kiln-lab browser plan --k K` (2026-11-04), for the five targets of the bar in both browsers (8
target rows: the HTTP/3 rows run in Chromium only), a 5 s background window twice before each job,
a budget of 120 h:

| K | hours in all | cells at the 3,688 floor |
| --- | --- | --- |
| 1 | 277.4 (over the budget) | every cell |
| 8 | 109.6 | none |
| 16 | 73.4 | none |

The matrix runs at **K = 8**, Chromium only (see "What the lab leaves out"), on the recording's
seed (`--seed 7`, so the baselines' replay is what the run's kiln sends). Each case's pace on each
profile is `runfile::workload`: a stream case opens an agent's session (its events at 5 a second
on the good link, 0.5 at 100 kbit/s and 0.25 at 50), a terminal (its lines at 20, 2 and 1 a second)
and `/api/events`, as the UI does; a job log's stream is there with `--logs` only, since kiln
today's log needs its CI to take the lab job, which kiln's runner declines while the lab machine's
load is past its limit (it did in some rounds and not others on the lab machine), and a cell's
stream mix must be the same for every target; a burst is an agent's turn of 40 events 25 ms
apart, every 3, 20 and 40 s; requests go every 100 ms, 2 s and 4 s, open-loop, the UI's reads (the
agents, the sessions, the overview, `me`, the notices, a session's snapshot) and its write (a
message to an agent) in turn. Each job runs 60 s on the good link and 180 s on the slow ones. At K =
8 (`kiln-lab browser plan --k 8`):

| condition | case | operations a round (8 links) | rounds | hours |
| --- | --- | --- | --- | --- |
| good/0, good/1, good/5 | stream | 21,600 | 5 | 1.02 each |
| good/0, good/1, good/5 | burst | 6,400 | 5 | 1.02 each |
| good/0, good/1, good/5 | request | 4,800 | 5 | 1.02 each |
| 100k/0, 100k/1, 100k/5 | stream | 6,480 | 5 | 2.54 each |
| 100k/0, 100k/1, 100k/5 | burst | 2,880 | 5 | 2.54 each |
| 100k/0, 100k/1, 100k/5 | request | 720 | 14 | 7.12 each |
| 50k/0, 50k/1, 50k/5 | stream | 3,240 | 5 | 2.66 each |
| 50k/0, 50k/1, 50k/5 | burst | 1,440 | 7 | 3.72 each |
| 50k/0, 50k/1, 50k/5 | request | 360 | 28 | 14.87 each |

The CPU measure's percentiles are across 1 s windows, so a cell's CPU p99.9 rests on its rounds'
seconds, not its operations: under 3,688 windows (most cells) the CPU p99.9 is reported with its
count and marked as not bounded, and its mean carries the cell.
<!-- run plan: end -->

## Running it

```sh
cd crates/kiln-lab/web && npm ci        # Playwright, pinned; its browsers from ~/.cache/ms-playwright
kiln-lab browser probe
kiln-lab browser check-profiles --out ~/.cache/kiln-bp/g1/checks
kiln-lab browser record --kiln path/to/kiln --out ~/.cache/kiln-bp/g1/record
kiln-lab browser plan --k 8
kiln-lab browser run --kiln path/to/kiln --h2o path/to/h2o --caddy path/to/caddy \
  --payloads ~/.cache/kiln-bp/g1/catalogue/payloads --k 8 --rounds 5 --out DIR
kiln-lab browser report DIR/run.json
kiln-lab browser compare DIR/run.json kiln-today kiln-today-b
```

Adding a target (goals 3, 5, 6): a record in `targets()`, its server's start in `servers.rs`, and,
if its carrier is new, the probe page's reading of it in `probe.js`.

## The numbers

<!-- browser-lab tables: begin -->
(The matrix's tables go here: step 8. As of 2026-11-05 03:37Z the pilot is running on the lab
machine, in the unit `kiln-bp-g1-lab` (`crates/kiln-lab/web/servers/run-all.sh`), and the matrix
follows it there; see "Where the run stands".)
<!-- browser-lab tables: end -->

### Where the run stands

The pilot and the matrix run in one systemd user unit on the lab machine, started by the agent that
built the lab, from a snapshot of the lab (`~/.cache/kiln-bp/g1/lab-snap`: the `kiln-lab` binary,
the web driver and `run-all.sh`) that nothing edits while it runs:

```sh
systemd-run --user --unit=kiln-bp-g1-lab -E PATH=$PATH -E HOME=$HOME -p Delegate=yes \
  -p StandardOutput=append:$HOME/.cache/kiln-bp/g1/lab-run-all.log \
  -p StandardError=append:$HOME/.cache/kiln-bp/g1/lab-run-all.log \
  --collect bash $HOME/.cache/kiln-bp/g1/lab-snap/run-all.sh
```

Its parts (the pilot's three, then the matrix's 21: each condition and case with the rounds the run
plan gives it) each write `~/.cache/kiln-bp/g1/lab/<part>/run.json` as their jobs end, and `done`
when they end well (`failed` otherwise). Started again the same way, it skips what is done and
runs the rest: that is how it resumes after a stop or a reset. `systemctl --user stop
kiln-bp-g1-lab` stops it (the lab's PID namespace takes every process it started with it). The
pilot picks h2o's QUIC controller for the matrix (the lower median p99 at 50k/1 of `h2o-h3`, Cubic,
and `h2o-h3-reno`, Reno).

## What the lab leaves out

- The front door's hop: the lab measures kiln's own TLS listener; production phones reach kiln
  through the front door, which passes bytes through (f7).
- A radio's promotion delay after idle, unless Ada adds it.
- HTTP/3 in WebKit: WebKit on Linux speaks no HTTP/3 (libsoup: f30). Safari on a Mac would, with its
  traffic shaped on the lab host.
- WebKit at all, for now: on the lab machine (kiln-a094a12d, Fedora 44) Playwright's WebKit doesn't
  start ("Host system is missing dependencies": ICU 74, libjpeg 8 and others built for Ubuntu), and
  the machine where it does (adacachy) is production's host, where long labs don't run. The probe
  names it (`kiln-lab browser probe` stops at "webkit starts"); `--browsers chromium` runs without
  it. Installing WebKit's libraries on the lab machine is Ada's (it needs root).
- BBR: TCP is CUBIC for every target (loading `tcp_bbr` needs root on the lab host: Ada's).
- Page loads and static files: goal 8's.
