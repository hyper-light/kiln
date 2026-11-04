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

(The catalogue: step 3.)

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
  against the server's each job by WebSocket echoes over the unshaped control pair, taking the
  offset of the echo with the least round trip (f29); the residuals of 200 more echoes are kept.
- **Bytes.** The client link's frames both ways (the client's own `eth0`, every header, ACK and
  retransmission), read every window (1 s) by the driver from its namespace's `/proc/net/dev`. Each
  window's bytes go to the operations that completed in it; windows with none are left out; the
  percentiles are across windows, and the mean is all bytes over all operations. The bytes above
  the transport (what the page was given) are a mean beside it.
- **Server CPU.** The server's scope (and, for the reference servers, their origin's scope, counted
  with it and shown apart) read every window, less the background's rate measured just before the
  job with nobody attached, over the window's operations (every client's); percentiles across
  windows, and the mean. Two background windows' difference is the CPU measure's noise floor,
  reported beside every CPU number.

A target is a record (`crates/kiln-lab/src/browser/targets.rs`): its server, its port, whether it
is HTTP/3, its carrier for streams, the protocol the page must have got, and the browsers it runs
in. Each job's record says which protocol each path came over (`nextHopProtocol`).

## The baselines

(The reference servers and their tuning: step 5.)

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
(The run plan's computation goes here: step 6.)
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
(The matrix's tables go here: step 8.)
<!-- browser-lab tables: end -->

## What the lab leaves out

- The front door's hop: the lab measures kiln's own TLS listener; production phones reach kiln
  through the front door, which passes bytes through (f7).
- A radio's promotion delay after idle, unless Ada adds it.
- HTTP/3 in WebKit: WebKit on Linux speaks no HTTP/3 (libsoup: f30). Safari on a Mac would, with its
  traffic shaped on the lab host.
- Page loads and static files: goal 8's.
