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

Where the machine's own libraries can't start Playwright's WebKit (Fedora: its build wants Ubuntu's
ICU 74, libjpeg 8 and others), WebKit runs in Playwright's official image instead:
`KILN_LAB_PODMAN=mcr.microsoft.com/playwright:v1.64.1-noble`. The lab then takes rootless podman's
own user namespace (`podman unshare`) for its namespaces in place of a fresh one, and each WebKit
driver is a container joining its client's network namespace (`--network ns:/proc/<pid>/ns/net`),
running as the image's unprivileged user `pwuser` under podman's default seccomp profile and
SELinux (the driver's folder mounted read-only, the job's folder read-write, both labelled `:z`),
with no cgroup of its own (`--cgroups=disabled`: systemd, outside the lab's PID namespace, can't
place it, and a browser's CPU isn't a measure). No sudo, no `--privileged`, nothing loaded outside
the image. On 2026-11-05 on kiln-a094a12d the probe passed with Chromium 156.0.8078.4 on the
machine and WebKit 27.2 in the image, started as uid 1000, and again on 2026-11-07
(`docs/browser-lab/2026-11-07/probe.txt`).

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
| request | a session's snapshot (the session socket's first, packed frame) | 715 KB unpacked for a session of 200 events (715,426 B: `2026-11-07/snapshot.txt`) | |
| request | `POST /api/harness/sessions/:id/command` (a message to an agent) | 156 / 156 answered, 57 sent | |

Every request rode HTTP/2; every socket was an HTTP/1.1 connection of its own. Left out, with why:
pages, the UI server's server functions and static files (`GET /`, `/agents/:id`, `/runs/...`, 120
files under `/assets`): page loads are goal 8's.

Two recordings on one stand-in seed (`KILN_LAB_SEED=7`) send the same event bodies once their
stamps and ids are taken out (`node catalogue.mjs compare`): of the 2,773 events both 150-second
recordings sent (keyed by their turn and place in it), 2,773 hashed equal (`docs/browser-lab/2026-11-07/recordings.txt`,
the two recordings compared again). A run uses the same seed,
so the baselines replay exactly what kiln sends.

At the recording's pace, `/api/events` alone carries about 162 kbit/s (31.1 frames a second of
652 bytes on average, the catalogue's): three times a 50 kbit/s link
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
  f35). The page's clock (`timeOrigin + now()`, cross-origin isolated) is calibrated against the
  server's each job by 200 WebSocket echoes over the unshaped control pair (to the lab's control
  echo, `crates/kiln-lab/web/control.mjs`), taking the offset of the echo with the least round trip
  (f29). Its residual is the calibration taken again the same way, in 10 groups of 40 more echoes
  interleaved (echo i in group i mod 10, so each group spans the whole check): how far each group's
  offset lies from the one the page uses (each echo's own offset, off by up to half its round trip,
  is kept beside it). Each job records its page's timer grain (`timer_grain_us`: the smallest step
  `performance.now()` takes): 5 µs in Chromium, and 1,000 µs in WebKit 27.2 even cross-origin
  isolated, so **WebKit's time cells resolve to 1 ms**: its calibration, and every time it
  measures, is a whole number of milliseconds on the page's side. The calibration's limit is
  200 µs, or the job's grain where that is coarser (1 ms in WebKit: plan v5's step 4). Chromium runs with Local Network
  Access off (`--disable-features=LocalNetworkAccessChecks,LocalNetworkAccessChecksWebSockets`):
  the control echo is on another private address than the page's origin, which Chromium would
  otherwise hold for a permission; the target's own operations are all same-origin.
- **Bytes.** The client link's frames both ways (the client's own `eth0`, every header, ACK and
  retransmission), read every 25 ms (`--window-ms`) by the driver from its namespace's
  `/proc/net/dev`. A reading's bytes since the last reading an operation completed in go to the
  operations completing in it, shared between them; a reading with none carries its bytes on to
  the next that has one, so no byte is left out and, with readings finer than the operations' pace,
  each operation is a sample of its own. The percentiles are across those samples, and the mean is
  all bytes over all operations. The bytes above the transport (what the page was given) are a mean
  beside it.
- **Server CPU.** The server's scope (and, for the reference servers, their origin's scope, counted
  with it and shown apart) read every 25 ms the same way, less the background's rate, over the
  reading's operations (every client's); percentiles across those samples, and the mean. The background is the
  server with nobody attached: once it is at rest (a server just started is still settling in: two
  5 s windows in a row within a quarter, or 2 ms a second, of each other, 90 s at most), two windows
  before the job, and one after it; its rate is the mean of before and after, and the larger of the
  two windows' difference and the before's and after's is the CPU measure's noise floor, kept with
  every job. kiln's runner (the agents' machine) is in a scope of its own and isn't the server's.
  A per-operation CPU percentile at or under that noise over a reading's length (the noise a
  second times `cpu_us.window_ms.p50`, the median of the cell's rounds) is the background's, not
  the operation's: there the tables give the cell's mean instead, and the cell says so ("mean: the
  p50 is under the noise floor, F µs"; `report::cpu_floor_us`). Step 7's pilot found it in most of
  kiln today's p50s ("Where the run stands").

### The probe's checks

`kiln-lab browser check-probe RUN --catalogue CATALOGUE` reads a run made with `--capture` (a packet
capture on each client link, `crates/kiln-lab/web/capture.py`: a raw packet socket in the client's
own namespace, every frame's length and time, written as pcap with the browser's TLS keys beside it
for goal 7) and `--stall 10` (the server stopped, every process of it, for 2 s, 10 s into each
request job). On 2026-11-04 (`docs/browser-lab/2026-11-04/check4.json`, the good link, kiln today, h2o over
HTTP/2 and Caddy over HTTP/3, 2 links, 30 s jobs):

- calibration: the residual's p99, worst job 20.5 µs, median job 14.2 µs (within 200 µs); in the
  pilot's first jobs at K = 8 (pass 0's good-link pilot, its round 0: `2026-11-05/pass0-pilot-good.json`),
  on the lab machine at a load of 22 to 43 (median 34), the worst job's was 232 µs and the median
  job's 75.5 µs: on a busy machine eight browsers' echoes wait for a
  core, so the matrix's good-link times carry up to a quarter of a millisecond of clock error. With
  groups of 40 echoes taken back to back (2026-11-05, K = 8, `docs/browser-lab/2026-11-05/check4b.json`) Chromium's worst job was
  26 µs and 226 µs in two jobs, WebKit's (in the image) 606 µs and 662 µs. Both have a cause, read
  from the jobs' echoes:
  - WebKit's page timer steps by 1 ms (`timer_grain_us` 1000, cross-origin isolated; Chromium's
    5 µs; a 5 s job on one link, `docs/browser-lab/2026-11-05/grain.json`): its round trips read 0 or 1,000 µs,
    so an offset from them is off by up to a millisecond whatever the link does. WebKit's residuals
    (20 to 662 µs, within its grain) are its timer's, and its limit is its grain (plan v5);
  - Chromium's 226 µs was one group of ten, in one job: every one of its 40 echoes took 745 µs or
    more (the job's least 100 µs), a run of echoes all waiting for a core on the loaded machine, so
    the group's least round trip was a slow one too. The groups are now interleaved across the
    check (echo i in group i mod 10), so a stretch of waiting takes a few echoes from every group
    and no group's least round trip;
- capture against counters: 0.00% apart in all six jobs (2.1 MB to 15.8 MB each);
- the probe's sizes above the transport against the real UI's, where the UI's payloads were the
  same each time: `GET /api/agents` 1.4%, `GET /api/me` 0.0%, `GET /api/overview` 0.1%, the
  terminal's frames 0.1% apart; the message to an agent's answer was 3.7% apart with one id for
  every message, and 8.3% apart (169 bytes against 156) with a new id each time as the UI sends
  them. Then (2026-11-05, K = 8, ids unique to each link, `check4b.json`) 162.5 against 156 (4.2%), read
  apart by browser from the jobs' samples: Chromium's answers were 154, 155 and 156 bytes (the
  digits of `seq`, the session's event count, growing from one digit to three over a job, as the
  recording's 154 to 156), WebKit's 168 to 170, each 14 bytes longer, in every link (the run file's
  means for kiln today: 155.5 and 169.5 bytes, 34,842 and 37,978 over 224 answers each); the 162.5 was
  the two browsers' mean. The answers, kept with their samples since, name the 14 bytes:
  `,"repeat":true`. WebKit's jobs ran after Chromium's against the same kiln with the same ids
  (unique to each link, not to each job), so kiln answered each as a repeat of Chromium's. A write's
  id now carries the job's number in the run too (`l` + link + job in three base-36 digits + the
  request's in five: still ten characters, as the UI's `lab-rec-12`). Then (2026-11-05, K = 8,
  `docs/browser-lab/2026-11-05/check4c.json`, kiln today and h2o over HTTP/2, both browsers) every answer was
  154 to 156 bytes, as the recording's, the `seq` digits apart: 155.8 against 156 (0.1%), within 2%;
  in the same run `GET /api/agents` 1.3%, `GET /api/me` 0.0%, `GET /api/overview` 0.3%. The
  calibration there, each browser held to its own limit: Chromium's worst job 67.2 µs and median
  32.2 µs (within 200 µs), WebKit's worst 602.2 µs and median 588.8 µs (within its 1 ms grain);
  `check-probe` exit 0.
  Operations whose payloads follow the lab kiln's state (the sessions' list, the notices, the
  metrics' history, `/api/events`, a session's frames) aren't compared;
- the stall: the longest request waited 2,156 ms (kiln today), 2,097 ms (h2o) and 2,001 ms (Caddy)
  from its scheduled time: the whole wait counted;
- the CPU measure's noise floor: kiln today 2,887 µs of CPU a second, h2o 8.2, Caddy 17.6.

`kiln-lab browser start-stop --times 20` started and stopped each reference target 20 times (h2o
and Caddy over HTTP/2 and HTTP/3) and kiln today 20 times: each came up on its port (and its UDP
port for HTTP/3) every time, and each stop left none of its recorded processes and nothing in its
scopes. Every static body each reference server served (14 of 14) came back byte for byte as the
catalogue holds it.

`node crates/kiln-lab/web/check-origin.mjs PAYLOADS CATALOGUE` checks the streams the same way: it
starts the origin on sockets of its own, opens every recorded stream on both carriers (Server-Sent
Events, read as a browser's EventSource reads them, and the streaming fetch, a frame a line), has
the origin replay every frame, and holds that the replay it reads is the recording (each stream's
frames hash to the catalogue's digest) and that every frame it sent, on each carrier, in order and
none missing, is the replay's in normal form (stamps and ids out), a terminal's or a log's text
read back from the JSON string the origin sends it as. Its first run (2026-11-05) found the origin
framing text lossily on both carriers: a terminal's frames end in CR CR LF, which an event's data
can't hold (it ends at any CR or LF), and 10 of a log's frames hold several lines, which the
streaming fetch's one frame a line splits; the origin now sends a terminal's and a log's text as
a JSON string (the escapes are the carrier's cost, in its bytes). Then every frame held: 75,583
`/api/events` frames, 23,019 of a session's, 11,981 of a log's and 6,001 of a terminal's, each on
both carriers, the replay's digests the catalogue's (`node check-origin.mjs`, exit 0; again on
2026-11-07, `docs/browser-lab/2026-11-07/check-origin.txt`). `kiln-lab
browser start-stop` runs the same check through each reference server, the first of its 20 starts
(`check-origin.mjs served`: the server's HTTP/2, gzip asked for, in a client's namespace).
On 2026-11-05, and again for the record on 2026-11-07 (`kiln-lab browser start-stop --times 20`,
which prints what it found: `docs/browser-lab/2026-11-07/start-stop.txt`, the numbers below its;
the final configs, h2o's binary
`8b2d8a23…`, Caddy's `4ef1f68c…`): h2o and Caddy over HTTP/2 and HTTP/3 each came up 20 of 20 and
stopped clean 20 of 20, served 14 of 14 static bodies byte for byte, and sent on every stream frame
whole on both carriers, gzipped (75,583 + 23,019 + 11,981 + 6,001 frames each), each frame from its
stamp to the reader p50 0.1 to 0.2 ms; the p99 was 102 to 145 ms for the events' and a session's
replay (every frame of the recording sent 1 ms apart; alike in all four servers and on both
carriers, so no one server holding frames back) and 0.2 to 1.2 ms for a log's and a terminal's.

A target is a record (`crates/kiln-lab/src/browser/targets.rs`): its server, its port, whether it
is HTTP/3, its carrier for streams, the protocol the page must have got, and the browsers it runs
in. Each job's record says which protocol each path came over (`nextHopProtocol`), and, for a
reference server, which its access log says (`server_protocols`: WebKit gives a stream the page
holds open no timing entry, so a WebKit stream's protocol is the server's word); `check-probe`
holds every path of every reference server's job to its target's protocol. On 2026-11-05
(`docs/browser-lab/2026-11-05/check5c.json`, K = 1, streams and requests): h2o and Caddy over HTTP/2 11 of 11
paths over h2 in Chromium and in WebKit (WebKit 27.2 in Playwright's image under podman; 3 of its
11, the streams, by the servers' logs), h2o and Caddy over HTTP/3 11 of 11 over h3 in Chromium;
`check-probe` exit 0.

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
  every frame is the size kiln's was), on the schedule kiln's stand-ins keep: as many events as
  they write (an event is a stamped frame; the recorded frames between two, a session's state or an
  entity's patch, go with the event after them, as kiln sends them: 0.54 of them an event in a
  session's stream, 0.63 in `/api/events`); the
  stream is compressed as the server does it best (below). The origin runs in a scope of its own:
  its CPU counts with the server's and is shown apart;
- **the write** (a message to an agent) passed to the origin, which answers what kiln answered.

The tuning, and why:

- TLS 1.3 only, with the lab kiln's chain (one ECDSA P-256 leaf under a lab CA), so the handshake's
  certificate bytes are the same for every target.
- JSON compressed as each server does best, since the bar is against each protocol's best
  practice; kiln today sends JSON uncompressed (f8), as it does in production. Static bodies: h2o
  brotli, then gzip (`compress: [ br, gzip ]`), Caddy zstd, then gzip (`encode zstd gzip`).
  Streams, as measured on the lab machine (2026-11-05, and again for the record on 2026-11-07:
  `pass1/compress-probe.sh`, `docs/browser-lab/2026-11-07/compress-probe.txt`; a probe of each
  server on localhost: 51 `/api/events`-like frames 10 ms apart, 3,968 bytes uncompressed, the
  script's own stream): neither server compresses an event
  stream by default (h2o's compressible types leave out `text/event-stream` and
  `application/x-ndjson`; Caddy's encoder waits for 512 bytes, which a stream's frames don't
  reach, and leaves NDJSON out). So h2o marks both types compressible and gzips its streams: its
  brotli starts afresh at each flushed chunk (4,122 bytes, 4% more than none), while its gzip keeps
  its window across them (668 bytes, 83% less); and Caddy compresses its streams from the first
  byte, NDJSON included (zstd: 848 bytes, 79% less). The served-streams check (below) also times
  each frame from its stamp, so a server holding frames back to compress them would show.
- The kernel settings kiln uses, the same for every target: kiln's server sets `TCP_NOTSENT_LOWAT`
  to 128 KiB on each connection (`crates/kiln/src/http/server.rs`), and h2o and Caddy have no
  setting for it, so the server's namespace takes it as every socket's default
  (`net.ipv4.tcp_notsent_lowat=131072`, set by `link::topology`; writable in the lab's own
  namespace without root). kiln sets no qdisc of its own (no `fq`), so neither does any baseline:
  the server's veth keeps the kernel's own.
- TCP's congestion control is CUBIC for every target, kiln today's included: BBR would need root on
  the lab host to load `tcp_bbr` (f21), which is Ada's to allow; it is used nowhere until she does.
- QUIC's congestion control: h2o's `cc` set to Cubic (h2o defaults to Reno; its choices are Reno,
  Cubic and Pico: f32); Caddy's is quic-go's Cubic (quic-go has no BBR: f33). The pilot (step 7)
  measures h2o's Reno against Cubic at 50 kbit/s with 1% loss and keeps the lower median p99
  (`run-all.sh` picks it and logs it). Pass 0's three rounds had Reno's 16.9 s against Cubic's
  17.4 s; pass 0 is superseded, so pass 1's pilot makes the choice.
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
- Each server's whole config is written by `servers.rs` into each round's folder beside its log
  (`h2o.conf`, `Caddyfile`), and `kiln-lab browser start-stop` starts each from it.
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

The matrix runs at **K = 8** (but for the 50 kbit/s streams in WebKit, at K = 2 with 13 rounds:
step 7's K check, "Where the run stands"), in both browsers (WebKit in Playwright's image), on the recording's
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

Bytes and CPU have as many samples as the operations (2026-11-05): the link's counters and the
server's CPU are read every 25 ms (`--window-ms`, finer than any case's pace on a link), and each
reading's amount since the last one an operation completed in goes to the operations completing in
it (a reading with none carries its amount on), so no byte or microsecond is left out and an
operation on a reading of its own is a sample of its own. Pass 0's 1 s windows gave a cell about 60
CPU and 480 bytes samples a round (goal 10's look at its pilot), below the floor at any rounds the
plan gives. The run plan's hours don't change: the readings cost nothing a job doesn't already
spend. Two limits, said where they bind: a reading holds at most one sample of the server's CPU,
so where operations come faster than 40 a second across the links (the good link's streams,
requests and bursts) the CPU's samples are the readings, about 2,400 a round, or 12,000 over 5
rounds; a burst's CPU on the good link (40 events in 1 s every 3 s, each sent to all 8 links)
reaches about 4,000 over its 5 rounds, above 3,688 but under 10,000. And the background's noise
over a reading's length (the job's noise per second times `cpu_us.window_ms.p50`, recorded with
every job) is the floor a per-operation CPU percentile must clear; where it doesn't, the cell gives
the CPU's mean and says so.

The whole run at K = 8: the pilot (good/0 and 50k/1, 5 rounds, kiln today twice, h2o over HTTP/2
and HTTP/3 with Cubic and Reno, both browsers where each runs: about 12 h, scaled from pass 0's
Chromium-only pilot, 1.7 h and about 5.3 h, by 8 rows to 5) and the matrix's 109.6 h above: about
122 h, a little over five days.
<!-- run plan: end -->

## Running it

```sh
cd crates/kiln-lab/web && npm ci        # Playwright, pinned; its browsers from ~/.cache/ms-playwright
npm run check && npm run typecheck && npm run lint && npm test   # the lab's JavaScript: syntax, types (jsconfig.json), ESLint, its tests
kiln-lab browser probe
kiln-lab browser check-profiles --out ~/.cache/kiln-bp/g1/checks
kiln-lab browser record --kiln path/to/kiln --out ~/.cache/kiln-bp/g1/record
kiln-lab browser plan --k 8
kiln-lab browser run --kiln path/to/kiln --h2o path/to/h2o --caddy path/to/caddy \
  --payloads ~/.cache/kiln-bp/g1/catalogue2/payloads --seed 7 --k 8 --rounds 5 --out DIR
  # one cell of it: --targets T --profiles P --losses L --cases C --browsers B
kiln-lab browser report DIR/run.json
kiln-lab browser compare DIR/run.json kiln-today kiln-today-b
kiln-lab browser compare A/run.json kiln-today kiln-today --b-run B/run.json   # two runs' cells
kiln-lab browser merge --out ALL.json [--drop CASE,BROWSER,CONDITION,K] [--drop-jobs FILE] \
  [--drop-target NAME] [--slim] PART/run.json...
kiln-lab browser run ... --first-round R --rounds 1   # one round again, on its own seeds and number
```

A pass in parts (`crates/kiln-lab/web/servers`): `run-pass.sh PARTS WORKERS` runs a list of parts
(a name and the run's arguments a line, `after=` and `$H3` as the list's head says), each part a
lab of its own writing `<out>/<part>/run.json` and `done`; `run-pass-tail.sh` adds workers from the
list's end beside it; on a machine others share, `run-pass-mem.sh` starts a part only with
`KILN_LAB_MIN_AVAIL_GB` (10) available, each in a unit of its own, and `guard-mem.sh` freezes the
newest part's unit under 3 GB available and thaws it over 6, logging both. `pass1/step7.sh` is the
pilot's checks. Step 8's: `pass1/frozen.py` lists the jobs a freeze overlapped and a part for each
round to run again; `pass1/step8.sh KILN_LAB` merges the matrix (what it leaves out, and why, at its
head), writes `coverage.py`'s shortfalls and `report`'s tables; `pass1/fill.py` lists the rounds the
merged matrix still lacks as parts; `pass1/rerun5.py` draws step 8's five cells and compares their
rerun with the matrix; `pass1/records.sh` runs the checks that print and keep no run file again for
the record (`docs/browser-lab/2026-11-07/`).

**A cell again, as written.** The command above with one cell's flags gives that cell again within
noise on a machine as quiet as the matrix's: step 8's five cells, run so on 2026-11-06, were 44 of 45
numbers within noise of the matrix ("Step 8: the matrix"); and on 2026-11-07, a cell run so once the
lab machine had been quiet for five minutes (its one-minute load under 25, CPU pressure under 5%;
the jobs' loads 17.7 to 30.6, against 11 to 32 for the matrix's):
`docs/browser-lab/2026-11-07/quiet-good-1-request.json` (requests on the good link at 1% loss in
Chromium, kiln today and h2o over HTTP/2) against the matrix, 0 of 9 numbers differ beyond noise for
kiln today and 0 of 9 for h2o:

```sh
kiln-lab browser compare docs/browser-lab/2026-11-06/matrix.json kiln-today kiln-today --b-run docs/browser-lab/2026-11-07/quiet-good-1-request.json
kiln-lab browser compare docs/browser-lab/2026-11-06/matrix.json h2o-h2 h2o-h2 --b-run docs/browser-lab/2026-11-07/quiet-good-1-request.json
```

The same cell run so earlier that day
(`docs/browser-lab/2026-11-07/fresh-good-1-request.json`: requests on the good link at 1% loss in
Chromium, kiln today and h2o over HTTP/2, `kiln-lab browser compare` against the matrix) while the lab
machine was busy with other work (its one-minute load 12 to 105 at the jobs' starts, against 11 to 32
for the matrix's jobs of the cell), 2 of each target's 9 numbers were not: kiln today's time p99
(442.7 ms against 3,564) and bytes p99 (139,470 against 143,751 B), h2o's time p50 and p99.9 (21.6
against 37.7 ms, 510 against 1,350). Its round on a quiet machine (load 12 to 19) was the matrix's
for h2o (p50 22.0 ms, p99 376) and nearer it for kiln today (p99 889 ms against 404 to 586). The good
link's times move with the machine's load; each job records it (`machine`), so a rerun's load can be
read against the matrix's before its numbers are.

Adding a target (goals 3, 5, 6): a record in `targets()`, its server's start in `servers.rs`, and,
if its carrier is new, the probe page's reading of it in `probe.js`.

## The numbers

The matrix's tables, `kiln-lab browser report` of `docs/browser-lab/2026-11-06/matrix.json` (step 8's
merge, `pass1/step8.sh`): a table for each case in each browser, a row for each condition and measure
at p50, p99 and p99.9, a column for each target. Each value is the median of its rounds' quantiles,
its 95% interval over the rounds in brackets, and the samples it rests on (`n=`); each time p99.9
carries its within-round band. What a cell can't give, it says: **HTTP/3 in WebKit** is `-` (WebKit on
Linux speaks no HTTP/3: f30; "What the lab leaves out"), and **a CPU percentile at or under the
background's noise** over a reading's length gives the cell's mean instead, with the floor it fell
under ("mean: the p50 is under the noise floor, N µs"; step 4's rule). Which cells have fewer samples
than step 6 planned, and by how much, is in "Step 8: the matrix".

<!-- browser-lab run: docs/browser-lab/2026-11-06/matrix.json -->
<!-- browser-lab tables: begin -->
#### burst in chromium

| condition | measure | kiln-today | caddy-h2 | caddy-h3 | h2o-h2 | h2o-h3 | best |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100k/0 | time p50 (ms) | 12156 (10719–12278) n=14400 | 1118 (1111–1141) n=14054 | 654.2 (649.2–669.6) n=13971 | 1598 (1578–1610) n=13977 | 1221 (1210–1255) n=13935 | caddy-h3 654.2 |
| 100k/0 | time p99 (ms) | 17064 (16811–17883) n=14400 | 2277 (2260–2318) n=14054 | 2729 (1838–2877) n=13971 | 3039 (3033–3064) n=13977 | 3329 (3270–3521) n=13935 | caddy-h2 2277 |
| 100k/0 | time p99.9 (ms) | 17552 (17049–18401) n=14400 | 2381 (2323–2490) n=14054 | 3671 (3023–3753) n=13971 | 3158 (3120–3238) n=13977 | 4452 (3773–4703) n=13935 | caddy-h2 2381 |
| 100k/0 | bytes p50 (B) | 3094 (3094–3160) n=6958 | 504.7 (504.7–527.0) n=5748 | 426.0 (407.0–442.0) n=9972 | 757.0 (757.0–757.0) n=7012 | 674.0 (663.0–680.0) n=9873 | caddy-h3 426.0 |
| 100k/0 | bytes p99 (B) | 64284 (57710–67698) n=6958 | 6254 (4740–6320) n=5748 | 5391 (5305–5443) n=9972 | 5266 (5264–5701) n=7012 | 6053 (5899–6055) n=9873 | h2o-h2 5266 |
| 100k/0 | bytes p99.9 (B) | 72856 (67946–80832) n=6958 | 6386 (6320–6452) n=5748 | 5906 (5897–5975) n=9972 | 7007 (6687–7010) n=7012 | 7015 (7005–7015) n=9873 | caddy-h3 5906 |
| 100k/0 | cpu p50 (µs) | 783.9 (445.4–1164) n=4486 (mean: the p50 is under the noise floor, 206.4 µs) | 181.6 (162.0–240.5) n=2809 (mean: the p50 is under the noise floor, 20.2 µs) | 132.6 (110.5–153.0) n=3091 | 114.9 (87.6–120.3) n=3589 (mean: the p50 is under the noise floor, 12.4 µs) | 36.0 (25.6–38.7) n=3519 | h2o-h3 36.0 |
| 100k/0 | cpu p99 (µs) | 31606 (26769–34982) n=4486 | 8327 (5741–12327) n=2809 | 9734 (9146–14711) n=3091 | 4388 (3067–5825) n=3589 | 5942 (4503–6460) n=3519 | h2o-h2 4388 |
| 100k/0 | cpu p99.9 (µs) | 254797 (191990–435733) n=4486 | 57537 (52823–103937) n=2809 | 60069 (46692–70261) n=3091 | 46719 (27318–71553) n=3589 | 47568 (27548–66236) n=3519 | h2o-h2 46719 |
| 100k/1 | time p50 (ms) | 11436 (11317–11754) n=14080 | 1191 (1171–1215) n=14118 | 785.7 (701.2–808.3) n=13939 | 1743 (1674–1827) n=14119 | 1466 (1423–1540) n=14141 | caddy-h3 785.7 |
| 100k/1 | time p99 (ms) | 18029 (17103–22599) n=14080 | 2877 (2563–2896) n=14118 | 2613 (2385–2746) n=13939 | 3496 (3311–4738) n=14119 | 3471 (3159–3748) n=14141 | caddy-h3 2613 |
| 100k/1 | time p99.9 (ms) | 21148 (18075–28082) n=14080 | 3464 (2734–6152) n=14118 | 2975 (2672–4041) n=13939 | 5099 (3640–6313) n=14119 | 4570 (4340–5549) n=14141 | caddy-h3 2975 |
| 100k/1 | bytes p50 (B) | 3094 (3094–3160) n=6559 | 504.7 (462.0–526.7) n=5329 | 411.0 (403.5–418.0) n=7970 | 757.0 (757.0–757.0) n=6553 | 700.5 (700.0–774.8) n=7011 | caddy-h3 411.0 |
| 100k/1 | bytes p99 (B) | 50935 (36056–57447) n=6559 | 6254 (4740–6320) n=5329 | 5375 (5312–5447) n=7970 | 5332 (5266–5445) n=6553 | 5759 (5525–6053) n=7011 | h2o-h2 5332 |
| 100k/1 | bytes p99.9 (B) | 69436 (68970–81681) n=6559 | 6452 (6386–6452) n=5329 | 5902 (5896–5969) n=7970 | 7006 (6943–7009) n=6553 | 7387 (7010–10012) n=7011 | caddy-h3 5902 |
| 100k/1 | cpu p50 (µs) | 623.9 (468.6–996.8) n=5513 (mean: the p50 is under the noise floor, 356.4 µs) | 198.4 (128.5–234.4) n=2770 (mean: the p50 is under the noise floor, 18.5 µs) | 160.2 (150.6–211.9) n=2976 | 112.7 (98.5–207.3) n=3551 (mean: the p50 is under the noise floor, 10.5 µs) | 64.4 (56.4–73.2) n=3476 | h2o-h3 64.4 |
| 100k/1 | cpu p99 (µs) | 24936 (8246–40059) n=5513 | 8944 (2795–15939) n=2770 | 8166 (6123–14976) n=2976 | 4821 (4245–10728) n=3551 | 5375 (2627–12088) n=3476 | h2o-h2 4821 |
| 100k/1 | cpu p99.9 (µs) | 183113 (159561–258615) n=5513 | 59612 (50034–82847) n=2770 | 68512 (45067–84540) n=2976 | 38865 (23179–60967) n=3551 | 55947 (48368–65366) n=3476 | h2o-h2 38865 |
| 100k/5 | time p50 (ms) | 12056 (11843–12968) n=14400 | 1583 (1529–1675) n=14300 | 1544 (1503–1590) n=14085 | 2258 (2099–2347) n=14178 | 1957 (1908–2129) n=14167 | caddy-h3 1544 |
| 100k/5 | time p99 (ms) | 27276 (23879–29956) n=14400 | 5069 (4013–7666) n=14300 | 3820 (3566–4445) n=14085 | 7175 (6501–7953) n=14178 | 5585 (5073–5885) n=14167 | caddy-h3 3820 |
| 100k/5 | time p99.9 (ms) | 30219 (27483–33865) n=14400 | 8062 (6565–10897) n=14300 | 5617 (4673–5658) n=14085 | 8438 (8105–11341) n=14178 | 6326 (6288–6869) n=14167 | caddy-h3 5617 |
| 100k/5 | bytes p50 (B) | 3160 (3094–3160) n=7749 | 504.7 (465.0–526.7) n=4024 | 296.5 (269.0–453.5) n=3952 | 757.0 (754.5–790.0) n=5002 | 700.5 (700.5–701.5) n=5258 | caddy-h3 296.5 |
| 100k/5 | bytes p99 (B) | 29966 (27192–33650) n=7749 | 5363 (5097–6386) n=4024 | 4989 (4018–5514) n=3952 | 5831 (5658–6434) n=5002 | 5612 (5604–5678) n=5258 | caddy-h3 4989 |
| 100k/5 | bytes p99.9 (B) | 89232 (58096–174396) n=7749 | 10614 (6519–16677) n=4024 | 15840 (5976–20332) n=3952 | 12104 (8916–13646) n=5002 | 14148 (8455–17626) n=5258 | caddy-h2 10614 |
| 100k/5 | cpu p50 (µs) | 957.8 (735.2–1724) n=6675 (mean: the p50 is under the noise floor, 544.7 µs) | 205.2 (176.0–251.0) n=2669 (mean: the p50 is under the noise floor, 4.68 µs) | 210.4 (192.0–313.9) n=2459 | 19.8 (18.9–30.0) n=3705 | 83.5 (79.3–88.5) n=3526 | h2o-h2 19.8 |
| 100k/5 | cpu p99 (µs) | 42231 (28168–116351) n=6675 | 15703 (6971–16671) n=2669 | 14413 (11923–21275) n=2459 | 6704 (4740–9480) n=3705 | 6730 (4700–8411) n=3526 | h2o-h2 6704 |
| 100k/5 | cpu p99.9 (µs) | 231707 (148093–348759) n=6675 | 46008 (22979–92016) n=2669 | 62573 (28897–101209) n=2459 | 53965 (21225–189760) n=3705 | 49127 (28781–64522) n=3526 | caddy-h2 46008 |
| 50k/0 | time p50 (ms) | 24232 (22752–25184) n=6400 | 1642 (1618–1661) n=6060 | 1632 (1258–1803) n=5844 | 2797 (2757–2819) n=6047 | 3611 (3582–3710) n=5994 | caddy-h3 1632 |
| 50k/0 | time p99 (ms) | 33172 (32701–34269) n=6400 | 4183 (4117–4290) n=6060 | 5278 (4176–6308) n=5844 | 6249 (6117–6404) n=6047 | 5476 (5458–5741) n=5994 | caddy-h2 4183 |
| 50k/0 | time p99.9 (ms) | 33671 (33191–34764) n=6400 | 4736 (4231–4809) n=6060 | 5960 (5167–6848) n=5844 | 7932 (6416–14419) n=6047 | 5589 (5537–5797) n=5994 | caddy-h2 4736 |
| 50k/0 | bytes p50 (B) | 3160 (3160–3160) n=3036 | 422.0 (404.0–504.7) n=2998 | 253.6 (249.0–258.0) n=3739 | 757.0 (757.0–758.0) n=3088 | 679.0 (655.0–689.0) n=3156 | caddy-h3 253.6 |
| 50k/0 | bytes p99 (B) | 39145 (36880–41770) n=3036 | 3292 (3226–3508) n=2998 | 10246 (5328–11015) n=3739 | 5290 (5260–5381) n=3088 | 9858 (9156–11124) n=3156 | caddy-h2 3292 |
| 50k/0 | bytes p99.9 (B) | 46416 (40150–63133) n=3036 | 5096 (4841–5543) n=2998 | 14092 (12981–15836) n=3739 | 5952 (5530–6158) n=3088 | 16647 (16636–18142) n=3156 | caddy-h2 5096 |
| 50k/0 | cpu p50 (µs) | 1765 (507.0–1991) n=2147 (mean: the p50 is under the noise floor, 209.2 µs) | 166.4 (145.3–204.6) n=1579 (mean: the p50 is under the noise floor, 23.0 µs) | 134.7 (119.0–157.6) n=1695 | 131.2 (77.6–156.6) n=1854 (mean: the p50 is under the noise floor, 19.2 µs) | 30.4 (28.3–35.8) n=1577 | h2o-h3 30.4 |
| 50k/0 | cpu p99 (µs) | 75210 (21989–251739) n=2147 | 2824 (2618–7371) n=1579 | 6308 (4289–11043) n=1695 | 3308 (2452–4866) n=1854 | 5238 (2227–9931) n=1577 | caddy-h2 2824 |
| 50k/0 | cpu p99.9 (µs) | 264061 (164781–402183) n=2147 | 38298 (29352–73895) n=1579 | 57959 (44993–66602) n=1695 | 40230 (31319–61092) n=1854 | 32570 (31907–56806) n=1577 | h2o-h3 32570 |
| 50k/1 | time p50 (ms) | 25807 (22787–28060) n=6400 | 2013 (1832–2075) n=6231 | 2152 (1886–2174) n=6014 | 3190 (2830–3299) n=6187 | 3666 (3613–3751) n=6138 | caddy-h2 2013 |
| 50k/1 | time p99 (ms) | 49786 (32804–53709) n=6400 | 5078 (4498–5336) n=6231 | 5285 (4886–6388) n=6014 | 6244 (6110–12462) n=6187 | 5578 (5514–7152) n=6138 | caddy-h2 5078 |
| 50k/1 | time p99.9 (ms) | 50522 (33137–54600) n=6400 | 5264 (4585–9836) n=6231 | 6649 (5479–6759) n=6014 | 12863 (6505–20338) n=6187 | 6314 (5748–7431) n=6138 | caddy-h2 5264 |
| 50k/1 | bytes p50 (B) | 3160 (3160–3160) n=3319 | 504.7 (395.0–504.7) n=2698 | 252.7 (241.0–257.0) n=3404 | 757.0 (757.0–758.0) n=2894 | 689.0 (656.5–693.5) n=2991 | caddy-h3 252.7 |
| 50k/1 | bytes p99 (B) | 40634 (35989–49642) n=3319 | 5098 (3616–5131) n=2698 | 6373 (5321–8721) n=3404 | 5507 (5117–5989) n=2894 | 9858 (9119–9872) n=2991 | caddy-h2 5098 |
| 50k/1 | bytes p99.9 (B) | 171234 (40571–185872) n=3319 | 5099 (4841–5221) n=2698 | 14369 (11858–17216) n=3404 | 6444 (5838–21508) n=2894 | 18055 (12405–22179) n=2991 | caddy-h2 5099 |
| 50k/1 | cpu p50 (µs) | 6970 (809.5–12839) n=2825 (mean: the p50 is under the noise floor, 1291 µs) | 231.2 (198.9–244.6) n=1457 (mean: the p50 is under the noise floor, 3.20 µs) | 146.4 (134.4–162.3) n=1680 | 150.5 (84.4–171.8) n=1865 (mean: the p50 is under the noise floor, 6.27 µs) | 43.2 (32.7–57.3) n=1632 | h2o-h3 43.2 |
| 50k/1 | cpu p99 (µs) | 269566 (23038–376894) n=2825 | 12848 (6216–19860) n=1457 | 5333 (4534–24261) n=1680 | 6683 (2713–11929) n=1865 | 8569 (1977–13198) n=1632 | caddy-h3 5333 |
| 50k/1 | cpu p99.9 (µs) | 1674577 (174369–2401862) n=2825 | 55234 (36782–62547) n=1457 | 61292 (32688–66385) n=1680 | 44762 (12676–67181) n=1865 | 40783 (30989–60935) n=1632 | h2o-h3 40783 |
| 50k/5 | time p50 (ms) | 26517 (25075–28301) n=6400 | 2349 (2160–2562) n=6227 | 2332 (2234–2531) n=6168 | 3609 (3434–3934) n=6307 | 3566 (3397–3672) n=6110 | caddy-h3 2332 |
| 50k/5 | time p99 (ms) | 53332 (51691–57020) n=6400 | 5734 (5525–9500) n=6227 | 6966 (6752–9830) n=6168 | 6840 (6306–9564) n=6307 | 6914 (5836–7969) n=6110 | caddy-h2 5734 |
| 50k/5 | time p99.9 (ms) | 57266 (53990–60067) n=6400 | 11932 (5779–12897) n=6227 | 7900 (7297–10505) n=6168 | 10451 (6736–11966) n=6307 | 7739 (6273–8458) n=6110 | h2o-h3 7739 |
| 50k/5 | bytes p50 (B) | 3160 (3160–3192) n=3346 | 526.7 (395.0–526.7) n=1904 | 284.0 (241.0–536.5) n=2303 | 766.0 (757.0–790.0) n=2350 | 700.5 (700.5–700.5) n=2350 | caddy-h3 284.0 |
| 50k/5 | bytes p99 (B) | 38236 (30192–46434) n=3346 | 5097 (4674–5325) n=1904 | 6557 (5130–10800) n=2303 | 5851 (5511–6557) n=2350 | 6948 (5604–9035) n=2350 | caddy-h2 5097 |
| 50k/5 | bytes p99.9 (B) | 176164 (166206–191376) n=3346 | 5862 (5100–11830) n=1904 | 14829 (13211–16014) n=2303 | 9018 (6043–23238) n=2350 | 17032 (13242–20183) n=2350 | caddy-h2 5862 |
| 50k/5 | cpu p50 (µs) | 982.8 (629.6–2135) n=3034 (mean: the p50 is under the noise floor, 760.7 µs) | 244.5 (233.6–254.3) n=1291 (mean: the p50 is under the noise floor, 6.56 µs) | 192.2 (161.2–245.8) n=1593 | 24.6 (17.7–35.6) n=1746 | 60.9 (52.4–71.9) n=1611 | h2o-h2 24.6 |
| 50k/5 | cpu p99 (µs) | 52503 (29265–80003) n=3034 | 17412 (11029–20682) n=1291 | 15843 (9695–17768) n=1593 | 10364 (3857–23334) n=1746 | 6962 (4825–9762) n=1611 | h2o-h3 6962 |
| 50k/5 | cpu p99.9 (µs) | 406404 (285719–644246) n=3034 | 61099 (54201–77091) n=1291 | 59808 (57589–82569) n=1593 | 41179 (23519–60094) n=1746 | 52145 (45769–84960) n=1611 | h2o-h2 41179 |
| good/0 | time p50 (ms) | 10.9 (10.8–11.2) n=32000 | 11.7 (11.7–11.8) n=31996 | 10.6 (10.6–10.8) n=32000 | 11.9 (11.8–12.1) n=32000 | 10.9 (10.7–11.0) n=31994 | caddy-h3 10.6 |
| good/0 | time p99 (ms) | 32.2 (31.9–33.2) n=32000 | 12.7 (12.6–13.1) n=31996 | 11.5 (11.5–12.2) n=32000 | 13.8 (13.1–18.2) n=32000 | 13.5 (12.2–18.3) n=31994 | caddy-h3 11.5 |
| good/0 | time p99.9 (ms) | 94.2 (67.0–119.6) n=32000 band 73.4–131.3 | 15.0 (14.2–15.9) n=31996 band 13.3–16.9 | 15.0 (14.0–19.6) n=32000 band 12.0–16.9 | 14.5 (14.0–26.2) n=32000 band 14.3–15.2 | 15.0 (13.4–30.2) n=31994 band 14.7–15.7 | h2o-h2 14.5 |
| good/0 | bytes p50 (B) | 3181 (3168–3199) n=30705 | 217.0 (215.0–219.0) n=31606 | 206.0 (205.0–207.0) n=31585 | 545.0 (542.0–551.0) n=31265 | 583.0 (576.0–585.0) n=31404 | caddy-h3 206.0 |
| good/0 | bytes p99 (B) | 16409 (16303–17425) n=30705 | 4898 (4285–4964) n=31606 | 4200 (4194–4308) n=31585 | 5906 (5711–5930) n=31265 | 5512 (5444–5551) n=31404 | caddy-h3 4200 |
| good/0 | bytes p99.9 (B) | 35166 (31842–48447) n=30705 | 5781 (5773–5785) n=31606 | 5677 (5674–5756) n=31585 | 6938 (6522–7617) n=31265 | 6187 (6185–6194) n=31404 | caddy-h3 5677 |
| good/0 | cpu p50 (µs) | 1036 (-1052–1327) n=3893 | 145.4 (139.1–162.0) n=4007 | 181.2 (178.3–202.0) n=4003 | 70.7 (57.8–88.8) n=4004 | 87.8 (71.5–104.4) n=4004 | h2o-h2 70.7 |
| good/0 | cpu p99 (µs) | 5450 (2342–7108) n=3893 | 653.4 (444.6–832.0) n=4007 | 515.9 (481.8–847.8) n=4003 | 404.6 (301.4–640.1) n=4004 | 634.5 (339.9–1029) n=4004 | h2o-h2 404.6 |
| good/0 | cpu p99.9 (µs) | 96250 (25795–150144) n=3893 | 3565 (1980–4298) n=4007 | 3974 (2740–7883) n=4003 | 1274 (792.5–2643) n=4004 | 2057 (960.0–2751) n=4004 | h2o-h2 1274 |
| good/1 | time p50 (ms) | 11.0 (10.9–11.2) n=32000 | 11.7 (11.7–11.7) n=32000 | 10.6 (10.6–10.7) n=31997 | 11.9 (11.8–12.2) n=32000 | 10.8 (10.7–10.8) n=32000 | caddy-h3 10.6 |
| good/1 | time p99 (ms) | 72.2 (60.8–96.0) n=32000 | 57.0 (42.4–57.2) n=32000 | 55.1 (34.7–56.0) n=31997 | 53.0 (47.3–56.4) n=32000 | 38.2 (37.6–56.0) n=32000 | h2o-h3 38.2 |
| good/1 | time p99.9 (ms) | 178.4 (117.3–652.8) n=32000 band 151.9–238.5 | 241.9 (58.2–253.7) n=32000 band 145.2–298.7 | 57.4 (56.6–57.7) n=31997 band 56.6–81.5 | 65.2 (58.1–77.5) n=32000 band 57.9–258.0 | 57.5 (57.3–57.8) n=32000 band 56.8–82.4 | caddy-h3 57.4 |
| good/1 | bytes p50 (B) | 3277 (3262–3306) n=28153 | 219.0 (218.0–219.0) n=30814 | 207.0 (206.0–210.0) n=31088 | 543.0 (537.0–549.0) n=30414 | 584.3 (583.0–590.7) n=30811 | caddy-h3 207.0 |
| good/1 | bytes p99 (B) | 20967 (19046–21765) n=28153 | 4121 (3668–4204) n=30814 | 4209 (4195–4352) n=31088 | 4911 (4702–5407) n=30414 | 5046 (4602–5365) n=30811 | caddy-h2 4121 |
| good/1 | bytes p99.9 (B) | 37782 (36003–57693) n=28153 | 5784 (5632–5785) n=30814 | 5673 (5670–6406) n=31088 | 7417 (6631–8258) n=30414 | 6192 (6136–6263) n=30811 | caddy-h3 5673 |
| good/1 | cpu p50 (µs) | 1017 (-258.5–1081) n=3937 | 139.7 (139.0–142.4) n=4027 | 192.5 (181.8–193.9) n=4028 | 63.8 (57.9–89.8) n=4016 | 75.8 (73.5–84.2) n=4022 | h2o-h2 63.8 |
| good/1 | cpu p99 (µs) | 10175 (2085–56511) n=3937 | 663.3 (603.8–836.9) n=4027 | 588.3 (537.8–890.0) n=4028 | 488.9 (369.4–804.2) n=4016 | 457.9 (366.5–574.8) n=4022 | h2o-h3 457.9 |
| good/1 | cpu p99.9 (µs) | 69290 (18731–545035) n=3937 | 3888 (3223–4524) n=4027 | 3374 (1712–3914) n=4028 | 2115 (975.5–2356) n=4016 | 1571 (1376–5544) n=4022 | h2o-h3 1571 |
| good/5 | time p50 (ms) | 42.4 (38.0–46.1) n=32000 | 11.8 (11.7–11.8) n=32000 | 10.6 (10.6–10.6) n=32000 | 11.9 (11.9–12.0) n=31966 | 10.8 (10.7–10.8) n=31998 | caddy-h3 10.6 |
| good/5 | time p99 (ms) | 639.1 (499.3–741.7) n=32000 | 290.7 (278.4–404.5) n=32000 | 56.8 (56.6–58.6) n=32000 | 264.6 (255.7–447.8) n=31966 | 71.9 (58.8–80.4) n=31998 | caddy-h3 56.8 |
| good/5 | time p99.9 (ms) | 1095 (911.3–1702) n=32000 band 925.0–1137 | 678.9 (653.1–1685) n=32000 band 629.0–751.7 | 82.3 (81.7–107.3) n=32000 band 81.8–107.3 | 708.4 (632.9–1067) n=31966 band 676.4–778.4 | 106.3 (104.4–107.4) n=31998 band 85.3–172.6 | caddy-h3 82.3 |
| good/5 | bytes p50 (B) | 3614 (3546–3621) n=20904 | 234.0 (228.0–410.0) n=20017 | 211.0 (210.0–229.0) n=25575 | 576.7 (550.0–645.0) n=19307 | 586.0 (582.0–683.0) n=23691 | caddy-h3 211.0 |
| good/5 | bytes p99 (B) | 23965 (23418–24648) n=20904 | 3678 (3540–4965) n=20017 | 4189 (3702–4202) n=25575 | 4865 (4674–4913) n=19307 | 4454 (4274–5047) n=23691 | caddy-h2 3678 |
| good/5 | bytes p99.9 (B) | 31609 (31093–32970) n=20904 | 6053 (5646–6929) n=20017 | 5751 (5674–5918) n=25575 | 7702 (5408–8662) n=19307 | 7026 (6347–8016) n=23691 | caddy-h3 5751 |
| good/5 | cpu p50 (µs) | 1021 (306.2–1150) n=4670 | 142.8 (136.2–154.8) n=4163 | 196.2 (192.6–201.9) n=4075 | 65.1 (53.9–70.0) n=4158 | 86.7 (69.9–92.4) n=4055 | h2o-h2 65.1 |
| good/5 | cpu p99 (µs) | 12471 (4639–13134) n=4670 | 598.3 (546.7–863.5) n=4163 | 690.6 (660.4–861.8) n=4075 | 571.5 (461.8–933.1) n=4158 | 782.3 (618.3–932.9) n=4055 | h2o-h2 571.5 |
| good/5 | cpu p99.9 (µs) | 129773 (20436–165727) n=4670 | 3851 (3531–6482) n=4163 | 4109 (3574–32561) n=4075 | 2579 (1499–3792) n=4158 | 2450 (2260–3323) n=4055 | h2o-h3 2450 |

#### burst in webkit

| condition | measure | kiln-today | caddy-h2 | caddy-h3 | h2o-h2 | h2o-h3 | best |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100k/0 | time p50 (ms) | 13070 (12175–14449) n=14400 | 1458 (1457–1481) n=14273 | - | 1589 (1580–1610) n=14233 | - | caddy-h2 1458 |
| 100k/0 | time p99 (ms) | 40712 (17273–45215) n=14400 | 2644 (2620–2679) n=14273 | - | 3030 (3017–3039) n=14233 | - | caddy-h2 2644 |
| 100k/0 | time p99.9 (ms) | 41365 (17480–49272) n=14400 | 2811 (2763–2855) n=14273 | - | 3139 (3106–3208) n=14233 | - | caddy-h2 2811 |
| 100k/0 | bytes p50 (B) | 3094 (3094–3094) n=7248 | 757.0 (618.5–757.0) n=6188 | - | 757.0 (731.0–757.0) n=7300 | - | caddy-h2 757.0 |
| 100k/0 | bytes p99 (B) | 63063 (46302–63409) n=7248 | 4814 (4740–4818) n=6188 | - | 5407 (5406–5408) n=7300 | - | caddy-h2 4814 |
| 100k/0 | bytes p99.9 (B) | 77494 (72252–115673) n=7248 | 6320 (6320–6320) n=6188 | - | 6875 (6874–6877) n=7300 | - | caddy-h2 6320 |
| 100k/0 | cpu p50 (µs) | 1160 (689.9–1361) n=5176 (mean: the p50 is under the noise floor, 270.5 µs) | 226.9 (189.3–294.3) n=3359 (mean: the p50 is under the noise floor, 1.80 µs) | - | 115.6 (107.0–158.3) n=3628 (mean: the p50 is under the noise floor, 0.19 µs) | - | h2o-h2 115.6 (mean) |
| 100k/0 | cpu p99 (µs) | 26584 (20753–68662) n=5176 | 14124 (11352–20856) n=3359 | - | 5345 (4543–11160) n=3628 | - | h2o-h2 5345 |
| 100k/0 | cpu p99.9 (µs) | 231451 (124869–270506) n=5176 | 55659 (40430–65977) n=3359 | - | 47075 (19495–63342) n=3628 | - | h2o-h2 47075 |
| 100k/1 | time p50 (ms) | 12794 (9374–14634) n=13687 | 1512 (1486–1526) n=14223 | - | 1676 (1645–1760) n=14278 | - | caddy-h2 1512 |
| 100k/1 | time p99 (ms) | 37942 (19024–44334) n=13687 | 3120 (2957–6711) n=14223 | - | 3530 (3313–5725) n=14278 | - | caddy-h2 3120 |
| 100k/1 | time p99.9 (ms) | 38596 (22833–47415) n=13687 | 3449 (3251–7366) n=14223 | - | 4147 (3782–7368) n=14278 | - | caddy-h2 3449 |
| 100k/1 | bytes p50 (B) | 3094 (3094–3650) n=6261 | 757.0 (730.0–757.0) n=6317 | - | 757.0 (754.5–790.0) n=5545 | - | caddy-h2 757.0 |
| 100k/1 | bytes p99 (B) | 41592 (34166–64797) n=6261 | 4740 (4740–4818) n=6317 | - | 5407 (4984–5613) n=5545 | - | caddy-h2 4740 |
| 100k/1 | bytes p99.9 (B) | 131908 (76661–442178) n=6261 | 6320 (6320–8739) n=6317 | - | 8111 (6875–12752) n=5545 | - | caddy-h2 6320 |
| 100k/1 | cpu p50 (µs) | 1310 (643.1–1481) n=5617 (mean: the p50 is under the noise floor, 334.0 µs) | 207.7 (199.0–247.0) n=3294 (mean: the p50 is under the noise floor, 1.88 µs) | - | 119.2 (110.8–163.7) n=3583 (mean: the p50 is under the noise floor, 0.28 µs) | - | h2o-h2 119.2 (mean) |
| 100k/1 | cpu p99 (µs) | 36809 (31852–44646) n=5617 | 15886 (11093–21078) n=3294 | - | 7519 (6034–9090) n=3583 | - | h2o-h2 7519 |
| 100k/1 | cpu p99.9 (µs) | 272653 (216578–413446) n=5617 | 58657 (44901–74610) n=3294 | - | 46720 (29650–105637) n=3583 | - | h2o-h2 46720 |
| 100k/5 | time p50 (ms) | 23316 (12194–26623) n=14335 | 1926 (1859–2026) n=14279 | - | 2277 (2103–2346) n=14326 | - | caddy-h2 1926 |
| 100k/5 | time p99 (ms) | 48158 (28308–73235) n=14335 | 5575 (4825–5695) n=14279 | - | 7597 (6984–9286) n=14326 | - | caddy-h2 5575 |
| 100k/5 | time p99.9 (ms) | 51509 (30579–77530) n=14335 | 7157 (5365–8557) n=14279 | - | 8983 (8151–11836) n=14326 | - | caddy-h2 7157 |
| 100k/5 | bytes p50 (B) | 3094 (3094–3160) n=7774 | 757.0 (711.8–757.0) n=4826 | - | 757.0 (757.0–759.0) n=5242 | - | caddy-h2 757.0 |
| 100k/5 | bytes p99 (B) | 29380 (26464–38068) n=7774 | 5304 (4816–6254) n=4826 | - | 5828 (5695–5981) n=5242 | - | caddy-h2 5304 |
| 100k/5 | bytes p99.9 (B) | 186990 (67400–427690) n=7774 | 12684 (7547–17452) n=4826 | - | 11192 (6874–15032) n=5242 | - | h2o-h2 11192 |
| 100k/5 | cpu p50 (µs) | 864.7 (608.5–2011) n=6926 (mean: the p50 is under the noise floor, 360.2 µs) | 219.7 (185.4–231.2) n=3192 (mean: the p50 is under the noise floor, 4.03 µs) | - | 19.5 (16.1–27.0) n=3702 | - | h2o-h2 19.5 |
| 100k/5 | cpu p99 (µs) | 26892 (22090–80361) n=6926 | 19749 (17348–21892) n=3192 | - | 7456 (5243–9155) n=3702 | - | h2o-h2 7456 |
| 100k/5 | cpu p99.9 (µs) | 175119 (82271–187082) n=6926 | 49688 (31174–60830) n=3192 | - | 31975 (16452–50073) n=3702 | - | h2o-h2 31975 |
| 50k/0 | time p50 (ms) | 26115 (22231–26718) n=6400 | 2507 (2426–2553) n=6178 | - | 2833 (2793–2858) n=6205 | - | caddy-h2 2507 |
| 50k/0 | time p99 (ms) | 54785 (54107–55797) n=6400 | 13201 (13150–13467) n=6178 | - | 6379 (6365–7188) n=6205 | - | h2o-h2 6379 |
| 50k/0 | time p99.9 (ms) | 55061 (54628–56073) n=6400 | 13878 (13716–14224) n=6178 | - | 12834 (11299–14023) n=6205 | - | h2o-h2 12834 |
| 50k/0 | bytes p50 (B) | 3160 (3160–3192) n=2310 | 724.0 (708.0–757.0) n=3201 | - | 757.0 (757.0–757.0) n=3228 | - | caddy-h2 724.0 |
| 50k/0 | bytes p99 (B) | 33988 (31110–39920) n=2310 | 4806 (4806–4806) n=3201 | - | 4985 (4984–5407) n=3228 | - | caddy-h2 4806 |
| 50k/0 | bytes p99.9 (B) | 41728 (38522–73796) n=2310 | 5288 (4816–5289) n=3201 | - | 5659 (4985–5854) n=3228 | - | caddy-h2 5288 |
| 50k/0 | cpu p50 (µs) | 1432 (1250–27240) n=1880 (mean: the p50 is under the noise floor, 441.7 µs) | 251.0 (217.5–266.9) n=1726 (mean: the p50 is under the noise floor, 3.04 µs) | - | 161.7 (137.2–195.6) n=2029 (mean: the p50 is under the noise floor, 2.24 µs) | - | h2o-h2 161.7 (mean) |
| 50k/0 | cpu p99 (µs) | 26395 (20208–139596) n=1880 | 16957 (9421–28523) n=1726 | - | 5473 (4861–10326) n=2029 | - | h2o-h2 5473 |
| 50k/0 | cpu p99.9 (µs) | 222006 (35699–3257833) n=1880 | 57793 (41993–75161) n=1726 | - | 45214 (30029–68986) n=2029 | - | h2o-h2 45214 |
| 50k/1 | time p50 (ms) | 27375 (26221–40583) n=6400 | 2777 (2670–2834) n=6277 | - | 3112 (2960–3144) n=6244 | - | caddy-h2 2777 |
| 50k/1 | time p99 (ms) | 58554 (52633–76922) n=6400 | 5768 (5582–6410) n=6277 | - | 7088 (6954–12717) n=6244 | - | caddy-h2 5768 |
| 50k/1 | time p99.9 (ms) | 60614 (52859–79241) n=6400 | 13562 (13369–22609) n=6277 | - | 18313 (14199–32677) n=6244 | - | caddy-h2 13562 |
| 50k/1 | bytes p50 (B) | 3160 (3160–3160) n=2858 | 726.0 (724.0–727.0) n=2891 | - | 757.0 (757.0–760.0) n=2957 | - | caddy-h2 726.0 |
| 50k/1 | bytes p99 (B) | 41620 (35300–49576) n=2858 | 4806 (4806–4818) n=2891 | - | 5406 (4986–5407) n=2957 | - | caddy-h2 4806 |
| 50k/1 | bytes p99.9 (B) | 48108 (45281–329592) n=2858 | 4818 (4816–5990) n=2891 | - | 6557 (6556–18041) n=2957 | - | caddy-h2 4818 |
| 50k/1 | cpu p50 (µs) | 2096 (849.8–8750) n=2596 (mean: the p50 is under the noise floor, 1507 µs) | 236.5 (233.3–289.7) n=1700 (mean: the p50 is under the noise floor, 2.94 µs) | - | 152.0 (139.2–181.4) n=1892 (mean: the p50 is under the noise floor, 0.71 µs) | - | h2o-h2 152.0 (mean) |
| 50k/1 | cpu p99 (µs) | 27240 (20439–328714) n=2596 | 18944 (14189–22555) n=1700 | - | 7521 (3022–8582) n=1892 | - | h2o-h2 7521 |
| 50k/1 | cpu p99.9 (µs) | 516598 (201741–712385) n=2596 | 61177 (48302–70734) n=1700 | - | 46695 (14601–63357) n=1892 | - | h2o-h2 46695 |
| 50k/5 | time p50 (ms) | 32078 (26207–41859) n=6400 | 3395 (3125–3444) n=6241 | - | 3769 (3500–3873) n=6195 | - | caddy-h2 3395 |
| 50k/5 | time p99 (ms) | 73119 (51690–77375) n=6400 | 6780 (5835–11522) n=6241 | - | 9600 (6809–15437) n=6195 | - | caddy-h2 6780 |
| 50k/5 | time p99.9 (ms) | 77434 (52448–83150) n=6400 | 11049 (6038–13657) n=6241 | - | 12771 (7439–18997) n=6195 | - | caddy-h2 11049 |
| 50k/5 | bytes p50 (B) | 3160 (3160–3160) n=3077 | 727.0 (690.0–757.0) n=2177 | - | 761.0 (749.0–761.0) n=2262 | - | caddy-h2 727.0 |
| 50k/5 | bytes p99 (B) | 40004 (28904–42308) n=3077 | 4818 (4816–5880) n=2177 | - | 5790 (5408–6441) n=2262 | - | caddy-h2 4818 |
| 50k/5 | bytes p99.9 (B) | 293262 (47984–311688) n=3077 | 7114 (4818–18224) n=2177 | - | 8430 (6978–16101) n=2262 | - | caddy-h2 7114 |
| 50k/5 | cpu p50 (µs) | 1019 (640.3–6322) n=2739 (mean: the p50 is under the noise floor, 919.7 µs) | 238.4 (232.0–286.1) n=1470 (mean: the p50 is under the noise floor, 4.05 µs) | - | 23.7 (16.4–52.7) n=1717 | - | h2o-h2 23.7 |
| 50k/5 | cpu p99 (µs) | 37211 (25725–47258) n=2739 | 20524 (17999–28020) n=1470 | - | 9239 (7026–11262) n=1717 | - | h2o-h2 9239 |
| 50k/5 | cpu p99.9 (µs) | 378299 (239455–562852) n=2739 | 49581 (37725–60940) n=1470 | - | 24465 (18056–79362) n=1717 | - | h2o-h2 24465 |
| good/0 | time p50 (ms) | 11.1 (11.0–11.1) n=32000 | 10.9 (10.7–10.9) n=31999 | - | 11.0 (10.9–11.1) n=31999 | - | caddy-h2 10.9 |
| good/0 | time p99 (ms) | 32.5 (32.2–33.5) n=32000 | 12.2 (12.0–14.8) n=31999 | - | 12.6 (12.5–20.6) n=31999 | - | caddy-h2 12.2 |
| good/0 | time p99.9 (ms) | 223.2 (143.4–267.1) n=32000 band 149.0–307.1 | 14.3 (13.7–23.8) n=31999 band 12.9–15.4 | - | 14.3 (13.9–41.2) n=31999 band 13.7–21.1 | - | caddy-h2 14.3 |
| good/0 | bytes p50 (B) | 3189 (3179–3197) n=30545 | 528.0 (517.0–531.0) n=31378 | - | 599.0 (595.0–603.0) n=31116 | - | caddy-h2 528.0 |
| good/0 | bytes p99 (B) | 17306 (17267–18610) n=30545 | 5310 (5302–5315) n=31378 | - | 5972 (5798–5993) n=31116 | - | caddy-h2 5310 |
| good/0 | bytes p99.9 (B) | 32976 (32647–37370) n=30545 | 5922 (5921–5928) n=31378 | - | 6587 (6569–7131) n=31116 | - | caddy-h2 5922 |
| good/0 | cpu p50 (µs) | 1106 (691.4–1159) n=3921 | 162.9 (159.4–177.6) n=4020 | - | 59.8 (56.0–85.6) n=4017 | - | h2o-h2 59.8 |
| good/0 | cpu p99 (µs) | 7985 (5400–12786) n=3921 | 565.4 (496.5–1025) n=4020 | - | 357.9 (298.2–704.8) n=4017 | - | h2o-h2 357.9 |
| good/0 | cpu p99.9 (µs) | 24711 (11912–85348) n=3921 | 2277 (796.1–4895) n=4020 | - | 1342 (1120–5392) n=4017 | - | h2o-h2 1342 |
| good/1 | time p50 (ms) | 11.2 (11.0–11.2) n=32000 | 10.7 (10.7–10.9) n=32000 | - | 10.9 (10.8–11.0) n=31990 | - | caddy-h2 10.7 |
| good/1 | time p99 (ms) | 129.1 (76.4–417.8) n=32000 | 40.6 (39.2–56.3) n=32000 | - | 55.7 (55.3–56.0) n=31990 | - | caddy-h2 40.6 |
| good/1 | time p99.9 (ms) | 828.3 (459.9–1371) n=32000 band 753.0–965.9 | 124.3 (56.9–254.2) n=32000 band 64.4–255.4 | - | 81.6 (63.8–192.0) n=31990 band 63.6–251.4 | - | h2o-h2 81.6 |
| good/1 | bytes p50 (B) | 3279 (3266–3281) n=27587 | 529.0 (525.0–530.0) n=30659 | - | 595.0 (592.0–595.0) n=30522 | - | caddy-h2 529.0 |
| good/1 | bytes p99 (B) | 23000 (21652–23398) n=27587 | 4209 (4208–4429) n=30659 | - | 4773 (4732–4870) n=30522 | - | caddy-h2 4209 |
| good/1 | bytes p99.9 (B) | 37274 (35821–38710) n=27587 | 5808 (5743–5929) n=30659 | - | 7318 (6464–7460) n=30522 | - | caddy-h2 5808 |
| good/1 | cpu p50 (µs) | 967.4 (508.5–1089) n=3956 | 162.5 (157.2–167.2) n=4040 | - | 66.1 (57.9–70.3) n=4021 | - | h2o-h2 66.1 |
| good/1 | cpu p99 (µs) | 11656 (11275–15536) n=3956 | 816.9 (763.7–1082) n=4040 | - | 478.8 (370.1–543.4) n=4021 | - | h2o-h2 478.8 |
| good/1 | cpu p99.9 (µs) | 37344 (28811–238549) n=3956 | 3398 (2331–4117) n=4040 | - | 2604 (1337–3455) n=4021 | - | h2o-h2 2604 |
| good/5 | time p50 (ms) | 53.4 (51.4–64.3) n=32000 | 10.9 (10.8–11.0) n=29396 | - | 11.1 (10.9–11.2) n=31713 | - | caddy-h2 10.9 |
| good/5 | time p99 (ms) | 1915 (874.0–2491) n=32000 | 331.9 (282.5–511.3) n=29396 | - | 259.7 (252.7–312.8) n=31713 | - | h2o-h2 259.7 |
| good/5 | time p99.9 (ms) | 2943 (2573–3387) n=32000 band 2795–3093 | 696.9 (674.8–1099) n=29396 band 659.1–746.7 | - | 673.2 (565.6–706.6) n=31713 band 603.8–729.7 | - | h2o-h2 673.2 |
| good/5 | bytes p50 (B) | 3677 (3630–4308) n=19963 | 534.0 (531.0–614.0) n=22411 | - | 601.0 (587.0–700.5) n=19923 | - | caddy-h2 534.0 |
| good/5 | bytes p99 (B) | 25977 (25150–26152) n=19963 | 4109 (3585–4208) n=22411 | - | 4743 (4282–6149) n=19923 | - | caddy-h2 4109 |
| good/5 | bytes p99.9 (B) | 82531 (35208–184802) n=19963 | 6470 (4852–7125) n=22411 | - | 7860 (6152–8697) n=19923 | - | caddy-h2 6470 |
| good/5 | cpu p50 (µs) | 989.7 (-1184–1353) n=4931 | 166.5 (156.3–199.3) n=4112 | - | 65.8 (65.0–73.6) n=4166 | - | h2o-h2 65.8 |
| good/5 | cpu p99 (µs) | 20240 (2902–90928) n=4931 | 1184 (705.3–1774) n=4112 | - | 619.2 (561.4–725.0) n=4166 | - | h2o-h2 619.2 |
| good/5 | cpu p99.9 (µs) | 106482 (84006–458482) n=4931 | 3885 (2324–10922) n=4112 | - | 2338 (1436–2690) n=4166 | - | h2o-h2 2338 |

#### request in chromium

| condition | measure | kiln-today | caddy-h2 | caddy-h3 | h2o-h2 | h2o-h3 | best |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100k/0 | time p50 (ms) | 3722 (3704–3773) n=4320 | 7108 (7098–7114) n=4320 | 3960 (3953–4004) n=4320 | 8033 (7794–8189) n=4320 | 4214 (4144–5675) n=4320 | kiln-today 3722 |
| 100k/0 | time p99 (ms) | 15371 (15269–17233) n=4320 | 13339 (13288–13522) n=4320 | 13243 (13232–13285) n=4320 | 22925 (21900–23566) n=4320 | 48941 (24641–90359) n=4320 | caddy-h3 13243 |
| 100k/0 | time p99.9 (ms) | 21772 (15757–22437) n=4320 | 13652 (13525–14110) n=4320 | 14079 (13907–14224) n=4320 | 26715 (24737–29064) n=4320 | 52131 (25949–100725) n=4320 | caddy-h2 13652 |
| 100k/0 | bytes p50 (B) | 23600 (23170–23930) n=3607 | 993.5 (993.0–1000) n=1890 | 25600 (25386–25758) n=4274 | 18324 (18043–18914) n=2887 | 25074 (21199–26254) n=3711 | caddy-h2 993.5 |
| 100k/0 | bytes p99 (B) | 143060 (142694–143123) n=3607 | 157670 (156846–161981) n=1890 | 157544 (157408–157612) n=4274 | 181296 (167350–182897) n=2887 | 134967 (98766–140047) n=3711 | h2o-h3 134967 |
| 100k/0 | bytes p99.9 (B) | 143368 (143098–144368) n=3607 | 162966 (159914–164582) n=1890 | 157938 (157838–157969) n=4274 | 200587 (183794–225180) n=2887 | 147766 (108316–218036) n=3711 | kiln-today 143368 |
| 100k/0 | cpu p50 (µs) | 15079 (14700–18732) n=3022 (mean: the p50 is under the noise floor, 268.6 µs) | 1862 (1228–2194) n=1214 (mean: the p50 is under the noise floor, 6.84 µs) | 589.7 (439.9–702.7) n=3336 | 288.5 (241.9–461.6) n=2545 | 309.3 (36.5–508.1) n=3526 | h2o-h2 288.5 |
| 100k/0 | cpu p99 (µs) | 336552 (313020–457358) n=3022 | 61746 (36159–73717) n=1214 | 144103 (122363–178324) n=3336 | 21900 (15429–23561) n=2545 | 20891 (18846–25258) n=3526 | h2o-h3 20891 |
| 100k/0 | cpu p99.9 (µs) | 522587 (429013–704426) n=3022 | 99385 (55439–120712) n=1214 | 223942 (183436–290541) n=3336 | 59726 (35851–175404) n=2545 | 62717 (53401–72983) n=3526 | h2o-h2 59726 |
| 100k/1 | time p50 (ms) | 3389 (3235–3514) n=4307 | 7114 (7093–7443) n=4320 | 2154 (2046–2838) n=4320 | 7093 (6922–129604) n=4320 | 3939 (3912–4043) n=4310 | caddy-h3 2154 |
| 100k/1 | time p99 (ms) | 18263 (17152–21586) n=4307 | 13873 (13704–16672) n=4320 | 13890 (13759–33413) n=4320 | 24491 (22733–211674) n=4320 | 24753 (24324–28770) n=4310 | caddy-h2 13873 |
| 100k/1 | time p99.9 (ms) | 26172 (24299–31734) n=4307 | 17174 (15002–19964) n=4320 | 16268 (15407–38648) n=4320 | 30724 (28192–220470) n=4320 | 26871 (26285–31370) n=4310 | caddy-h3 16268 |
| 100k/1 | bytes p50 (B) | 22379 (21704–23508) n=3829 | 1084 (1007–1667) n=2001 | 26645 (24043–26866) n=4110 | 18027 (15137–18332) n=2443 | 26837 (26190–26980) n=4227 | caddy-h2 1084 |
| 100k/1 | bytes p99 (B) | 141160 (106950–142926) n=3829 | 159568 (157559–162063) n=2001 | 157368 (139463–157449) n=4110 | 175583 (171556–194728) n=2443 | 121143 (117708–141076) n=4227 | h2o-h3 121143 |
| 100k/1 | bytes p99.9 (B) | 148368 (145179–243614) n=3829 | 163908 (160343–164938) n=2001 | 157743 (157664–161131) n=4110 | 193944 (187812–216790) n=2443 | 156855 (150190–197790) n=4227 | kiln-today 148368 |
| 100k/1 | cpu p50 (µs) | 17500 (14809–21137) n=3388 (mean: the p50 is under the noise floor, 1271 µs) | 1692 (1336–1987) n=1562 (mean: the p50 is under the noise floor, 4.83 µs) | 1803 (992.3–2313) n=3580 | 411.5 (203.9–554.5) n=2300 | 731.6 (677.5–800.8) n=4013 | h2o-h2 411.5 |
| 100k/1 | cpu p99 (µs) | 224235 (189088–268842) n=3388 | 57987 (41855–68616) n=1562 | 77283 (49729–100175) n=3580 | 17732 (12360–436401) n=2300 | 13766 (12765–15717) n=4013 | h2o-h3 13766 |
| 100k/1 | cpu p99.9 (µs) | 423472 (297726–582718) n=3388 | 93346 (75706–102553) n=1562 | 181325 (139034–243806) n=3580 | 90785 (62332–4000819) n=2300 | 50203 (46327–55116) n=4013 | h2o-h3 50203 |
| 100k/5 | time p50 (ms) | 2935 (2861–3327) n=4248 | 11447 (11158–11652) n=4294 | 1564 (1484–1617) n=4307 | 11809 (9908–86952) n=4306 | 1922 (1835–2197) n=4315 | caddy-h3 1564 |
| 100k/5 | time p99 (ms) | 34217 (31095–36692) n=4248 | 27854 (26238–30897) n=4294 | 57735 (52150–62677) n=4307 | 48751 (46188–160480) n=4306 | 52523 (48237–59629) n=4315 | caddy-h2 27854 |
| 100k/5 | time p99.9 (ms) | 52062 (42228–64606) n=4248 | 33661 (30804–43815) n=4294 | 68349 (63059–73810) n=4307 | 61471 (56682–172902) n=4306 | 74263 (63444–77001) n=4315 | caddy-h2 33661 |
| 100k/5 | bytes p50 (B) | 18888 (18105–19153) n=3742 | 1580 (1580–1619) n=2007 | 20714 (19933–21357) n=4125 | 16908 (16189–17318) n=2487 | 22718 (22612–23344) n=4157 | caddy-h2 1580 |
| 100k/5 | bytes p99 (B) | 140440 (122696–143348) n=3742 | 158986 (157187–160966) n=2007 | 116415 (73404–127492) n=4125 | 150486 (125240–152916) n=2487 | 117516 (90372–143994) n=4157 | caddy-h3 116415 |
| 100k/5 | bytes p99.9 (B) | 212142 (169610–236320) n=3742 | 170960 (164770–184492) n=2007 | 172560 (119626–183690) n=4125 | 191184 (168188–218711) n=2487 | 223711 (197604–258759) n=4157 | caddy-h2 170960 |
| 100k/5 | cpu p50 (µs) | 14938 (13873–17885) n=3395 (mean: the p50 is under the noise floor, 1370 µs) | 266.5 (129.8–347.5) n=1925 | 1241 (1111–1953) n=3837 | 623.2 (548.2–869.2) n=2406 | 818.2 (727.7–881.7) n=3951 | caddy-h2 266.5 |
| 100k/5 | cpu p99 (µs) | 164786 (126849–209508) n=3395 | 26240 (24117–46296) n=1925 | 38644 (26590–51204) n=3837 | 13893 (8784–506861) n=2406 | 10095 (8953–12523) n=3951 | h2o-h3 10095 |
| 100k/5 | cpu p99.9 (µs) | 323978 (223250–383865) n=3395 | 52174 (39794–96789) n=1925 | 125614 (92016–180315) n=3837 | 51628 (36426–2611152) n=2406 | 56128 (44892–62791) n=3951 | h2o-h2 51628 |
| 50k/0 | time p50 (ms) | 8616 (8360–8808) n=3959 | 13248 (13193–13258) n=3960 | 4423 (4400–4453) n=3960 | 8499 (8377–8688) n=3960 | 4291 (4275–4299) n=3960 | h2o-h3 4291 |
| 50k/0 | time p99 (ms) | 27476 (27391–29029) n=3959 | 25564 (25524–25633) n=3960 | 26204 (25968–26278) n=3960 | 37720 (37011–38543) n=3960 | 38124 (37843–38479) n=3960 | caddy-h2 25564 |
| 50k/0 | time p99.9 (ms) | 33604 (27766–38045) n=3959 | 25751 (25655–25987) n=3960 | 26518 (26372–26553) n=3960 | 42328 (39930–42924) n=3960 | 40759 (39378–41888) n=3960 | caddy-h2 25751 |
| 50k/0 | bytes p50 (B) | 11168 (9720–14078) n=3141 | 1094 (1094–1095) n=2114 | 26674 (26604–26686) n=3948 | 17853 (17759–17968) n=3403 | 24235 (24169–24324) n=3897 | caddy-h2 1094 |
| 50k/0 | bytes p99 (B) | 127194 (125433–128484) n=3141 | 120295 (81346–142870) n=2114 | 56607 (55531–57915) n=3948 | 98660 (89650–102616) n=3403 | 84606 (83738–86260) n=3897 | caddy-h3 56607 |
| 50k/0 | bytes p99.9 (B) | 131541 (129398–135870) n=3141 | 164006 (162259–164239) n=2114 | 81440 (79940–86674) n=3948 | 107608 (103503–112253) n=3403 | 93411 (87631–99030) n=3897 | caddy-h3 81440 |
| 50k/0 | cpu p50 (µs) | 17890 (14830–22472) n=2865 (mean: the p50 is under the noise floor, 895.5 µs) | 2515 (1760–4658) n=1587 (mean: the p50 is under the noise floor, 231.3 µs) | 6292 (4759–9373) n=3312 (mean: the p50 is under the noise floor, 753.1 µs) | 279.0 (230.3–378.1) n=3216 | 431.4 (383.9–470.4) n=3613 | h2o-h2 279.0 |
| 50k/0 | cpu p99 (µs) | 670663 (617706–896497) n=2865 | 80491 (60385–138715) n=1587 | 124239 (112748–201658) n=3312 | 22752 (11414–28101) n=3216 | 18167 (16764–20134) n=3613 | h2o-h3 18167 |
| 50k/0 | cpu p99.9 (µs) | 826222 (748637–1060230) n=2865 | 278521 (123492–345544) n=1587 | 426566 (274239–474838) n=3312 | 54445 (46531–80620) n=3216 | 49497 (47674–56654) n=3613 | h2o-h3 49497 |
| 50k/1 | time p50 (ms) | 8287 (7967–8454) n=5696 | 13308 (13251–13322) n=3960 | 3838 (3792–3940) n=3960 | 8291 (8104–8540) n=5674 | 4244 (4231–4259) n=5387 | caddy-h3 3838 |
| 50k/1 | time p99 (ms) | 29308 (27949–32652) n=5696 | 26006 (25644–27447) n=3960 | 26301 (25820–26523) n=3960 | 37924 (37500–38465) n=5674 | 39806 (38748–40192) n=5387 | caddy-h2 26006 |
| 50k/1 | time p99.9 (ms) | 34894 (33176–42653) n=5696 | 29758 (26283–34718) n=3960 | 30270 (26583–31747) n=3960 | 41804 (40357–42722) n=5674 | 45036 (41386–47054) n=5387 | caddy-h2 29758 |
| 50k/1 | bytes p50 (B) | 14168 (13552–17783) n=4619 | 1525 (1507–1580) n=2131 | 26064 (25972–26230) n=3913 | 18157 (17847–18603) n=4621 | 24381 (24271–24788) n=5281 | caddy-h2 1525 |
| 50k/1 | bytes p99 (B) | 125932 (122919–128747) n=4619 | 160140 (148319–164260) n=2131 | 56741 (56319–71621) n=3913 | 95738 (91067–101609) n=4621 | 86372 (84836–87466) n=5281 | caddy-h3 56741 |
| 50k/1 | bytes p99.9 (B) | 145796 (127801–164167) n=4619 | 164536 (163924–164753) n=2131 | 81694 (80441–84303) n=3913 | 106143 (102817–111281) n=4621 | 94100 (92400–98309) n=5281 | caddy-h3 81694 |
| 50k/1 | cpu p50 (µs) | 15666 (9732–25816) n=4397 (mean: the p50 is under the noise floor, 1754 µs) | 2026 (1799–2948) n=1740 (mean: the p50 is under the noise floor, 8.67 µs) | 1351 (1183–1580) n=3560 | 607.9 (356.7–744.8) n=4414 | 564.9 (484.9–604.9) n=4957 | h2o-h3 564.9 |
| 50k/1 | cpu p99 (µs) | 501088 (430177–716241) n=4397 | 75416 (59264–92315) n=1740 | 79460 (65371–102928) n=3560 | 37228 (28552–52889) n=4414 | 16132 (13789–19004) n=4957 | h2o-h3 16132 |
| 50k/1 | cpu p99.9 (µs) | 614939 (521970–950154) n=4397 | 116657 (86021–216646) n=1740 | 161663 (149467–253696) n=3560 | 77967 (56989–104614) n=4414 | 59504 (44919–62966) n=4957 | h2o-h3 59504 |
| 50k/5 | time p50 (ms) | 5500 (4624–6096) n=3862 | 13470 (13425–13520) n=3908 | 2357 (2283–2386) n=3947 | 7478 (7176–17477) n=3908 | 3704 (3589–3824) n=3925 | caddy-h3 2357 |
| 50k/5 | time p99 (ms) | 36916 (35463–39629) n=3862 | 29904 (29366–31062) n=3908 | 29189 (28835–30836) n=3947 | 45898 (42304–131477) n=3908 | 45293 (42468–48642) n=3925 | caddy-h3 29189 |
| 50k/5 | time p99.9 (ms) | 48279 (39765–56841) n=3862 | 34328 (31465–37373) n=3908 | 32628 (31660–34524) n=3947 | 53870 (46453–131829) n=3908 | 48265 (44890–55519) n=3925 | caddy-h3 32628 |
| 50k/5 | bytes p50 (B) | 19314 (17984–19841) n=3323 | 1514 (1514–1580) n=2092 | 24424 (24053–25089) n=3887 | 19325 (18566–20606) n=2904 | 25296 (24100–25346) n=3831 | caddy-h2 1514 |
| 50k/5 | bytes p99 (B) | 125396 (120696–130424) n=3323 | 161729 (160446–163259) n=2092 | 56667 (56417–67278) n=3887 | 111195 (105319–131215) n=2904 | 94654 (93202–106637) n=3831 | caddy-h3 56667 |
| 50k/5 | bytes p99.9 (B) | 165551 (145314–183993) n=3323 | 162950 (161021–164304) n=2092 | 80927 (66801–85326) n=3887 | 136525 (135157–163375) n=2904 | 122641 (110131–125943) n=3831 | caddy-h3 80927 |
| 50k/5 | cpu p50 (µs) | 23781 (17320–31404) n=3170 (mean: the p50 is under the noise floor, 2622 µs) | 3101 (2101–3943) n=1962 (mean: the p50 is under the noise floor, 297.4 µs) | 1616 (350.5–2373) n=3718 | 677.3 (516.4–1190) n=2778 | 736.7 (650.1–900.3) n=3730 | h2o-h2 677.3 |
| 50k/5 | cpu p99 (µs) | 347770 (239251–485506) n=3170 | 85627 (58230–117651) n=1962 | 67101 (45899–98556) n=3718 | 41299 (25788–66295) n=2778 | 18421 (14285–21639) n=3730 | h2o-h3 18421 |
| 50k/5 | cpu p99.9 (µs) | 512484 (466609–833357) n=3170 | 121228 (102740–241299) n=1962 | 207319 (135723–260442) n=3718 | 94987 (51378–1963103) n=2778 | 46648 (45502–60040) n=3730 | h2o-h3 46648 |
| good/0 | time p50 (ms) | 21.6 (21.5–105.8) n=24000 | 21.9 (21.6–23.3) n=24000 | 22.0 (21.6–24.4) n=24000 | 21.3 (21.1–354.0) n=23998 | 21.4 (21.2–22.7) n=24000 | h2o-h2 21.3 |
| good/0 | time p99 (ms) | 217.1 (178.9–12751) n=24000 | 80.6 (72.7–118.8) n=24000 | 68.8 (57.6–156.8) n=24000 | 57.1 (46.6–4577) n=23998 | 56.7 (43.6–2527) n=24000 | h2o-h3 56.7 |
| good/0 | time p99.9 (ms) | 277.1 (195.9–14547) n=24000 | 161.2 (110.7–519.5) n=24000 | 131.5 (87.9–253.5) n=24000 | 103.3 (93.2–5083) n=23998 | 94.8 (93.7–3051) n=24000 | h2o-h3 94.8 |
| good/0 | bytes p50 (B) | 3627 (3199–3660) n=21589 | 1454 (1454–1454) n=23933 | 1829 (1828–1829) n=23911 | 1295 (1295–1318) n=21564 | 1568 (1568–1569) n=22835 | h2o-h2 1295 |
| good/0 | bytes p99 (B) | 130716 (121436–146872) n=21589 | 154005 (153675–154071) n=23933 | 151052 (150988–151597) n=23911 | 177132 (177066–457295) n=21564 | 175853 (175778–175988) n=22835 | kiln-today 130716 |
| good/0 | bytes p99.9 (B) | 143874 (143700–257282) n=21589 | 157107 (156975–157107) n=23933 | 153075 (153006–153684) n=23911 | 180914 (180894–566856) n=21564 | 178076 (178075–197043) n=22835 | kiln-today 143874 |
| good/0 | cpu p50 (µs) | 6982 (6666–8722) n=8263 (mean: the p50 is under the noise floor, 3717 µs) | 632.9 (475.6–967.6) n=9566 | 995.2 (696.5–1044) n=9069 | 216.2 (127.7–314.8) n=7931 | 270.1 (215.5–458.7) n=8385 | h2o-h2 216.2 |
| good/0 | cpu p99 (µs) | 199562 (155765–304028) n=8263 | 17200 (11784–26667) n=9566 | 17408 (9912–43069) n=9069 | 12008 (10670–21442) n=7931 | 12877 (10574–15180) n=8385 | h2o-h2 12008 |
| good/0 | cpu p99.9 (µs) | 313888 (197973–395553) n=8263 | 32344 (19817–39064) n=9566 | 30387 (20419–52695) n=9069 | 14720 (13495–28076) n=7931 | 24182 (16657–25719) n=8385 | h2o-h2 14720 |
| good/1 | time p50 (ms) | 21.7 (21.6–26.1) n=23965 | 21.9 (21.6–131.8) n=24000 | 21.7 (21.6–23.2) n=23995 | 21.6 (21.5–26.9) n=24000 | 21.4 (21.3–21.4) n=24000 | h2o-h3 21.4 |
| good/1 | time p99 (ms) | 442.7 (404.2–585.6) n=23965 | 362.0 (333.3–26078) n=24000 | 235.7 (226.5–283.9) n=23995 | 386.9 (371.9–403.9) n=24000 | 349.9 (345.6–405.6) n=24000 | caddy-h3 235.7 |
| good/1 | time p99.9 (ms) | 1252 (1215–2029) n=23965 | 515.8 (429.0–27913) n=24000 | 293.1 (286.8–466.1) n=23995 | 510.0 (493.2–542.1) n=24000 | 457.2 (436.2–791.7) n=24000 | caddy-h3 293.1 |
| good/1 | bytes p50 (B) | 3589 (3523–3721) n=23052 | 1394 (1394–1394) n=17483 | 1833 (1833–1833) n=22777 | 1295 (1295–1295) n=18801 | 1577 (1575–1578) n=22908 | h2o-h2 1295 |
| good/1 | bytes p99 (B) | 139470 (121416–143540) n=23052 | 152807 (152709–154709) n=17483 | 151898 (151830–152015) n=22777 | 178025 (177250–178410) n=18801 | 173871 (162977–175658) n=22908 | kiln-today 139470 |
| good/1 | bytes p99.9 (B) | 145581 (143844–159306) n=23052 | 155919 (155243–315207) n=17483 | 153194 (152938–154022) n=22777 | 181122 (180061–181266) n=18801 | 177000 (176715–178142) n=22908 | kiln-today 145581 |
| good/1 | cpu p50 (µs) | 6961 (6490–7275) n=9267 (mean: the p50 is under the noise floor, 4132 µs) | 635.3 (300.9–882.2) n=8099 | 980.1 (877.0–1976) n=8761 | 114.6 (73.2–123.9) n=8186 | 316.5 (282.6–461.3) n=9004 | h2o-h2 114.6 |
| good/1 | cpu p99 (µs) | 101377 (69488–299122) n=9267 | 22775 (13209–147879) n=8099 | 27792 (18596–53247) n=8761 | 15628 (12740–19145) n=8186 | 9504 (6610–18557) n=9004 | h2o-h3 9504 |
| good/1 | cpu p99.9 (µs) | 138898 (109287–353412) n=9267 | 27121 (18816–1334733) n=8099 | 36857 (24336–66322) n=8761 | 28163 (22198–32889) n=8186 | 12526 (10624–30519) n=9004 | h2o-h3 12526 |
| good/5 | time p50 (ms) | 41.2 (35.3–164.9) n=23768 | 370.7 (342.6–422.1) n=24000 | 39.1 (31.3–414.8) n=23989 | 502.3 (385.6–7393) n=23988 | 34.7 (34.5–36.7) n=24000 | h2o-h3 34.7 |
| good/5 | time p99 (ms) | 1818 (1550–18317) n=23768 | 2358 (1141–10707) n=24000 | 1904 (516.6–11421) n=23989 | 5082 (2158–20809) n=23988 | 783.1 (766.5–4178) n=24000 | h2o-h3 783.1 |
| good/5 | time p99.9 (ms) | 5188 (2939–24794) n=23768 | 2946 (1733–11279) n=24000 | 2286 (618.9–12024) n=23989 | 6446 (3460–25953) n=23988 | 1076 (1022–11246) n=24000 | h2o-h3 1076 |
| good/5 | bytes p50 (B) | 7293 (6799–9054) n=20092 | 8737 (6978–11549) n=9838 | 10905 (9374–13274) n=18546 | 23062 (21724–23360) n=13570 | 26192 (26105–26406) n=22811 | kiln-today 7293 |
| good/5 | bytes p99 (B) | 146222 (143337–204314) n=20092 | 156359 (151144–158045) n=9838 | 119063 (99108–445314) n=18546 | 143264 (127442–188077) n=13570 | 83271 (80834–87199) n=22811 | h2o-h3 83271 |
| good/5 | bytes p99.9 (B) | 264488 (247050–304176) n=20092 | 457710 (159131–623454) n=9838 | 486466 (147532–2361377) n=18546 | 167747 (156766–245649) n=13570 | 121905 (108906–138389) n=22811 | h2o-h3 121905 |
| good/5 | cpu p50 (µs) | 4411 (4199–5299) n=9379 | 734.2 (531.9–936.5) n=6512 | 1508 (1128–1694) n=7305 | 503.6 (204.0–516.2) n=8204 | 733.2 (671.8–742.3) n=9905 | h2o-h2 503.6 |
| good/5 | cpu p99 (µs) | 82203 (63293–123771) n=9379 | 21360 (14807–25693) n=6512 | 21457 (11863–32540) n=7305 | 4586 (4260–12653) n=8204 | 4242 (3680–6272) n=9905 | h2o-h3 4242 |
| good/5 | cpu p99.9 (µs) | 122528 (98230–203215) n=9379 | 40380 (27074–82223) n=6512 | 51735 (18645–2006624) n=7305 | 10833 (7417–45496) n=8204 | 6621 (5691–9758) n=9905 | h2o-h3 6621 |

#### request in webkit

| condition | measure | kiln-today | caddy-h2 | caddy-h3 | h2o-h2 | h2o-h3 | best |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100k/0 | time p50 (ms) | 4268 (4236–4487) n=4317 | 5606 (5514–6081) n=4320 | - | 5458 (5346–5839) n=4319 | - | kiln-today 4268 |
| 100k/0 | time p99 (ms) | 16242 (16142–17234) n=4317 | 14698 (14624–17690) n=4320 | - | 30538 (29677–32871) n=4319 | - | caddy-h2 14698 |
| 100k/0 | time p99.9 (ms) | 16904 (16568–68385) n=4317 | 16638 (15476–25956) n=4320 | - | 42182 (37287–59200) n=4319 | - | caddy-h2 16638 |
| 100k/0 | bytes p50 (B) | 22363 (20392–23610) n=3640 | 1580 (1580–1628) n=3588 | - | 9967 (6592–11352) n=3783 | - | caddy-h2 1580 |
| 100k/0 | bytes p99 (B) | 142778 (142262–142858) n=3640 | 163005 (162805–163280) n=3588 | - | 190122 (185406–197964) n=3783 | - | kiln-today 142778 |
| 100k/0 | bytes p99.9 (B) | 143280 (143143–144239) n=3640 | 163970 (163810–164267) n=3588 | - | 266492 (234824–326558) n=3783 | - | kiln-today 143280 |
| 100k/0 | cpu p50 (µs) | 16820 (13213–41383) n=3284 (mean: the p50 is under the noise floor, 327.1 µs) | 1778 (1527–2377) n=3271 (mean: the p50 is under the noise floor, 3.81 µs) | - | 327.2 (249.8–384.5) n=3518 | - | h2o-h2 327.2 |
| 100k/0 | cpu p99 (µs) | 532647 (458646–705531) n=3284 | 34025 (27370–40414) n=3271 | - | 18785 (16685–25567) n=3518 | - | h2o-h2 18785 |
| 100k/0 | cpu p99.9 (µs) | 709669 (579476–13718748) n=3284 | 92639 (74236–131146) n=3271 | - | 65797 (57409–80240) n=3518 | - | h2o-h2 65797 |
| 100k/1 | time p50 (ms) | 4316 (4216–10274) n=4290 | 5476 (5204–5658) n=4274 | - | 6444 (5972–123775) n=4301 | - | kiln-today 4316 |
| 100k/1 | time p99 (ms) | 21453 (18844–52026) n=4290 | 17401 (15156–18670) n=4274 | - | 35436 (33002–214564) n=4301 | - | caddy-h2 17401 |
| 100k/1 | time p99.9 (ms) | 33070 (26842–74988) n=4290 | 28166 (24752–36942) n=4274 | - | 53460 (40059–228406) n=4301 | - | caddy-h2 28166 |
| 100k/1 | bytes p50 (B) | 19056 (11047–20451) n=3421 | 1638 (1604–1781) n=3663 | - | 7722 (1604–8944) n=3221 | - | caddy-h2 1638 |
| 100k/1 | bytes p99 (B) | 143442 (141507–145770) n=3421 | 162788 (161382–163802) n=3663 | - | 211360 (171432–225228) n=3221 | - | kiln-today 143442 |
| 100k/1 | bytes p99.9 (B) | 153899 (146840–188808) n=3421 | 164778 (164504–173370) n=3663 | - | 294172 (231625–313576) n=3221 | - | kiln-today 153899 |
| 100k/1 | cpu p50 (µs) | 1068 (586.1–8388) n=3193 | 1772 (1516–2262) n=3453 (mean: the p50 is under the noise floor, 27.1 µs) | - | 313.8 (128.7–540.4) n=3002 | - | h2o-h2 313.8 |
| 100k/1 | cpu p99 (µs) | 352559 (241640–860682) n=3193 | 22613 (19535–40084) n=3453 | - | 11782 (9057–21098) n=3002 | - | h2o-h2 11782 |
| 100k/1 | cpu p99.9 (µs) | 775692 (542654–10985439) n=3193 | 51370 (33874–90133) n=3453 | - | 69050 (33835–4508146) n=3002 | - | caddy-h2 51370 |
| 100k/5 | time p50 (ms) | 4526 (4114–5388) n=4250 | 4199 (3982–4412) n=4243 | - | 6978 (6372–34756) n=4274 | - | caddy-h2 4199 |
| 100k/5 | time p99 (ms) | 33062 (29716–38296) n=4250 | 28389 (25548–31616) n=4243 | - | 52444 (49851–107490) n=4274 | - | caddy-h2 28389 |
| 100k/5 | time p99.9 (ms) | 45776 (42846–74816) n=4250 | 46427 (39606–50354) n=4243 | - | 69440 (63936–124626) n=4274 | - | kiln-today 45776 |
| 100k/5 | bytes p50 (B) | 15896 (13780–16886) n=3480 | 12988 (12112–13399) n=3506 | - | 10920 (5853–12218) n=3050 | - | h2o-h2 10920 |
| 100k/5 | bytes p99 (B) | 144461 (141540–150544) n=3480 | 166776 (160222–182604) n=3506 | - | 205268 (189439–209388) n=3050 | - | kiln-today 144461 |
| 100k/5 | bytes p99.9 (B) | 217100 (181450–237050) n=3480 | 218024 (188242–241144) n=3506 | - | 282100 (267410–341386) n=3050 | - | kiln-today 217100 |
| 100k/5 | cpu p50 (µs) | 1748 (869.7–4941) n=3300 | 308.0 (160.1–430.0) n=3386 | - | 412.9 (268.1–543.8) n=2994 | - | caddy-h2 308.0 |
| 100k/5 | cpu p99 (µs) | 175600 (153598–239604) n=3300 | 17388 (12120–30993) n=3386 | - | 12304 (8270–13576) n=2994 | - | h2o-h2 12304 |
| 100k/5 | cpu p99.9 (µs) | 333908 (274233–416787) n=3300 | 25350 (20503–47314) n=3386 | - | 31779 (17645–265639) n=2994 | - | caddy-h2 25350 |
| 50k/0 | time p50 (ms) | 15225 (14128–24567) n=1165 | 11528 (10898–11722) n=3959 | - | 5369 (5022–6755) n=3960 | - | h2o-h2 5369 |
| 50k/0 | time p99 (ms) | 27236 (26226–27729) n=1165 | 33255 (30489–35762) n=3959 | - | 45155 (42756–46100) n=3960 | - | kiln-today 27236 |
| 50k/0 | time p99.9 (ms) | 27729 (26400–30720) n=1165 | 43794 (34576–46345) n=3959 | - | 50398 (47618–62139) n=3960 | - | kiln-today 27729 |
| 50k/0 | bytes p50 (B) | 3521 (3518–4511) n=1617 | 1536 (1452–1646) n=3158 | - | 17226 (14174–18299) n=3382 | - | caddy-h2 1536 |
| 50k/0 | bytes p99 (B) | 150106 (150100–150886) n=1617 | 162790 (162687–162994) n=3158 | - | 191696 (190854–192575) n=3382 | - | kiln-today 150106 |
| 50k/0 | bytes p99.9 (B) | 150332 (150164–154962) n=1617 | 163092 (162922–164047) n=3158 | - | 194943 (192776–195377) n=3382 | - | kiln-today 150332 |
| 50k/0 | cpu p50 (µs) | 6936 (4198–12286) n=1487 (mean: the p50 is under the noise floor, 606.8 µs) | 2354 (2021–2709) n=2963 (mean: the p50 is under the noise floor, 503.9 µs) | - | 291.7 (251.1–426.8) n=3207 | - | h2o-h2 291.7 |
| 50k/0 | cpu p99 (µs) | 249869 (185818–333258) n=1487 | 64612 (46986–69828) n=2963 | - | 14505 (7751–24397) n=3207 | - | h2o-h2 14505 |
| 50k/0 | cpu p99.9 (µs) | 364238 (224593–499895) n=1487 | 124404 (91730–163653) n=2963 | - | 49547 (37822–89962) n=3207 | - | h2o-h2 49547 |
| 50k/1 | time p50 (ms) | 10162 (9222–11696) n=2619 | 10194 (9945–10380) n=3391 | - | 5657 (5050–6921) n=5336 | - | h2o-h2 5657 |
| 50k/1 | time p99 (ms) | 34323 (29454–42490) n=2619 | 33925 (32004–43995) n=3391 | - | 47761 (45514–50462) n=5336 | - | caddy-h2 33925 |
| 50k/1 | time p99.9 (ms) | 44129 (33331–53137) n=2619 | 42947 (38679–56857) n=3391 | - | 72492 (52668–78109) n=5336 | - | caddy-h2 42947 |
| 50k/1 | bytes p50 (B) | 6800 (4740–7771) n=2710 | 1580 (1562–2740) n=2607 | - | 16710 (14937–18503) n=4596 | - | caddy-h2 1580 |
| 50k/1 | bytes p99 (B) | 151008 (150255–153102) n=2710 | 162596 (161501–163488) n=2607 | - | 187868 (187711–190550) n=4596 | - | kiln-today 151008 |
| 50k/1 | bytes p99.9 (B) | 156356 (154564–163339) n=2710 | 163403 (162506–164711) n=2607 | - | 193010 (191778–195045) n=4596 | - | kiln-today 156356 |
| 50k/1 | cpu p50 (µs) | 13576 (9268–28889) n=2578 (mean: the p50 is under the noise floor, 1201 µs) | 2672 (1663–15970) n=2455 (mean: the p50 is under the noise floor, 42.5 µs) | - | 487.4 (275.4–648.1) n=4428 | - | h2o-h2 487.4 |
| 50k/1 | cpu p99 (µs) | 446598 (393086–821132) n=2578 | 50358 (39942–75390) n=2455 | - | 12245 (7860–16916) n=4428 | - | h2o-h2 12245 |
| 50k/1 | cpu p99.9 (µs) | 583552 (462086–1275424) n=2578 | 95574 (51780–644261) n=2455 | - | 37763 (24407–78966) n=4428 | - | h2o-h2 37763 |
| 50k/5 | time p50 (ms) | 7131 (6490–8481) n=2956 | 5731 (5542–6123) n=3835 | - | 6613 (5760–7359) n=3836 | - | caddy-h2 5731 |
| 50k/5 | time p99 (ms) | 37928 (36549–62732) n=2956 | 34868 (31317–36992) n=3835 | - | 53982 (51805–64071) n=3836 | - | caddy-h2 34868 |
| 50k/5 | time p99.9 (ms) | 54483 (43103–71484) n=2956 | 41230 (37532–50645) n=3835 | - | 71992 (63660–81444) n=3836 | - | caddy-h2 41230 |
| 50k/5 | bytes p50 (B) | 14106 (13001–15429) n=2706 | 12386 (11176–13960) n=3242 | - | 18172 (12164–19002) n=3033 | - | caddy-h2 12386 |
| 50k/5 | bytes p99 (B) | 150122 (149462–152517) n=2706 | 161354 (161097–162419) n=3242 | - | 160867 (150734–170261) n=3033 | - | kiln-today 150122 |
| 50k/5 | bytes p99.9 (B) | 154991 (152928–173429) n=2706 | 163303 (162602–164102) n=3242 | - | 193133 (187622–205714) n=3033 | - | kiln-today 154991 |
| 50k/5 | cpu p50 (µs) | 19211 (14236–36079) n=2639 (mean: the p50 is under the noise floor, 3271 µs) | 2614 (2308–4114) n=3164 (mean: the p50 is under the noise floor, 440.4 µs) | - | 507.2 (325.4–727.5) n=2979 | - | h2o-h2 507.2 |
| 50k/5 | cpu p99 (µs) | 304983 (257682–495344) n=2639 | 46873 (39974–84097) n=3164 | - | 13643 (10697–44201) n=2979 | - | h2o-h2 13643 |
| 50k/5 | cpu p99.9 (µs) | 533806 (366541–757719) n=2639 | 135265 (72565–385801) n=3164 | - | 41187 (22361–251681) n=2979 | - | h2o-h2 41187 |
| good/0 | time p50 (ms) | 43.0 (23.0–158.0) n=23290 | 27.0 (23.0–36.0) n=23553 | - | 23.0 (22.0–24.0) n=23230 | - | h2o-h2 23.0 |
| good/0 | time p99 (ms) | 1373 (205.0–24878) n=23290 | 116.0 (106.0–1092) n=23553 | - | 118.0 (104.0–4115) n=23230 | - | caddy-h2 116.0 |
| good/0 | time p99.9 (ms) | 2658 (718.0–28267) n=23290 | 275.0 (152.0–2111) n=23553 | - | 241.0 (124.0–4497) n=23230 | - | h2o-h2 241.0 |
| good/0 | bytes p50 (B) | 3519 (2564–6887) n=17419 | 1363 (1363–1363) n=22635 | - | 1385 (1385–1385) n=22342 | - | caddy-h2 1363 |
| good/0 | bytes p99 (B) | 150388 (146943–257994) n=17419 | 153154 (152933–155524) n=22635 | - | 182101 (182035–182190) n=22342 | - | kiln-today 150388 |
| good/0 | bytes p99.9 (B) | 153278 (150742–562020) n=17419 | 155266 (153287–246901) n=22635 | - | 183541 (183000–580295) n=22342 | - | kiln-today 153278 |
| good/0 | cpu p50 (µs) | 4722 (-541.2–6494) n=8506 | 1177 (686.4–1814) n=9835 | - | 618.3 (461.5–686.7) n=10138 | - | h2o-h2 618.3 |
| good/0 | cpu p99 (µs) | 76316 (57142–108701) n=8506 | 13350 (11180–18036) n=9835 | - | 4382 (4226–6711) n=10138 | - | h2o-h2 4382 |
| good/0 | cpu p99.9 (µs) | 155865 (68458–199264) n=8506 | 25750 (13299–61049) n=9835 | - | 9346 (5631–22223) n=10138 | - | h2o-h2 9346 |
| good/1 | time p50 (ms) | 23.0 (23.0–30.0) n=23987 | 23.0 (23.0–33.0) n=23945 | - | 23.0 (22.0–34.0) n=23970 | - | kiln-today 23.0 |
| good/1 | time p99 (ms) | 652.0 (487.0–845.0) n=23987 | 567.0 (440.0–646.0) n=23945 | - | 541.0 (471.0–617.0) n=23970 | - | h2o-h2 541.0 |
| good/1 | time p99.9 (ms) | 1263 (1226–1377) n=23987 | 1132 (1106–1767) n=23945 | - | 1124 (1112–1431) n=23970 | - | h2o-h2 1124 |
| good/1 | bytes p50 (B) | 3495 (3495–3515) n=21431 | 1363 (1363–1363) n=22636 | - | 1385 (1324–1385) n=22298 | - | caddy-h2 1363 |
| good/1 | bytes p99 (B) | 147083 (146983–148436) n=21431 | 154065 (154002–155241) n=22636 | - | 182659 (182452–183778) n=22298 | - | kiln-today 147083 |
| good/1 | bytes p99.9 (B) | 150832 (150708–165160) n=21431 | 157682 (156597–158329) n=22636 | - | 185290 (185002–186805) n=22298 | - | kiln-today 150832 |
| good/1 | cpu p50 (µs) | 4636 (2508–5102) n=10000 | 1018 (903.2–1360) n=10148 | - | 492.5 (150.2–527.5) n=9291 | - | h2o-h2 492.5 |
| good/1 | cpu p99 (µs) | 79763 (45652–100225) n=10000 | 12584 (7455–13779) n=10148 | - | 6384 (4903–21401) n=9291 | - | h2o-h2 6384 |
| good/1 | cpu p99.9 (µs) | 119685 (73152–187952) n=10000 | 19727 (12527–25323) n=10148 | - | 8551 (7514–29008) n=9291 | - | h2o-h2 8551 |
| good/5 | time p50 (ms) | 53.0 (50.0–445.0) n=23856 | 93.0 (50.0–583.0) n=23210 | - | 47.0 (44.0–67.0) n=23939 | - | h2o-h2 47.0 |
| good/5 | time p99 (ms) | 1535 (1412–28118) n=23856 | 2496 (1138–6008) n=23210 | - | 1368 (1297–5045) n=23939 | - | h2o-h2 1368 |
| good/5 | time p99.9 (ms) | 3772 (2819–31270) n=23856 | 3355 (1968–6486) n=23210 | - | 3324 (2551–8417) n=23939 | - | h2o-h2 3324 |
| good/5 | bytes p50 (B) | 7545 (7479–7677) n=18044 | 5823 (3271–6127) n=15325 | - | 11649 (11136–12002) n=19110 | - | caddy-h2 5823 |
| good/5 | bytes p99 (B) | 149607 (148550–383228) n=18044 | 159504 (156207–476685) n=15325 | - | 184817 (184208–189449) n=19110 | - | kiln-today 149607 |
| good/5 | bytes p99.9 (B) | 240317 (220290–667024) n=18044 | 650174 (192302–1314333) n=15325 | - | 253905 (244240–282410) n=19110 | - | kiln-today 240317 |
| good/5 | cpu p50 (µs) | 4530 (1258–4934) n=9299 | 1051 (1010–1169) n=7699 | - | 535.7 (506.7–607.5) n=9878 | - | h2o-h2 535.7 |
| good/5 | cpu p99 (µs) | 57682 (49973–100796) n=9299 | 11563 (9896–35858) n=7699 | - | 5391 (3809–5473) n=9878 | - | h2o-h2 5391 |
| good/5 | cpu p99.9 (µs) | 97610 (70840–390296) n=9299 | 92982 (14562–152598) n=7699 | - | 8359 (5908–13790) n=9878 | - | h2o-h2 8359 |

#### stream in chromium

| condition | measure | kiln-today | caddy-h2 | caddy-h3 | h2o-h2 | h2o-h3 | best |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100k/0 | time p50 (ms) | 475.7 (470.3–611.3) n=16862 | 429.6 (428.9–429.9) n=21484 | 425.3 (424.1–426.7) n=21489 | 433.2 (432.4–434.6) n=21490 | 428.3 (427.6–429.3) n=21490 | caddy-h3 425.3 |
| 100k/0 | time p99 (ms) | 6684 (6586–127096) n=16862 | 714.2 (687.2–732.1) n=21484 | 709.4 (689.9–717.4) n=21489 | 747.3 (704.6–756.3) n=21490 | 719.4 (709.0–751.4) n=21490 | caddy-h3 709.4 |
| 100k/0 | time p99.9 (ms) | 15197 (14776–130648) n=16862 | 923.9 (905.6–932.0) n=21484 | 927.4 (900.5–944.9) n=21489 | 943.9 (900.9–953.2) n=21490 | 898.7 (869.6–938.7) n=21490 | h2o-h3 898.7 |
| 100k/0 | bytes p50 (B) | 1786 (1567–2301) n=14603 | 249.0 (243.0–249.5) n=19854 | 166.0 (166.0–168.0) n=19484 | 248.0 (223.0–248.0) n=19012 | 168.0 (168.0–168.0) n=19065 | caddy-h3 166.0 |
| 100k/0 | bytes p99 (B) | 14371 (13945–78195) n=14603 | 2748 (2498–2762) n=19854 | 2399 (2160–2460) n=19484 | 2499 (2418–2542) n=19012 | 2232 (2149–2248) n=19065 | h2o-h3 2232 |
| 100k/0 | bytes p99.9 (B) | 119153 (83702–119867) n=14603 | 5416 (5254–5524) n=19854 | 5375 (5372–5446) n=19484 | 5655 (3047–5660) n=19012 | 5354 (4345–5355) n=19065 | h2o-h3 5354 |
| 100k/0 | cpu p50 (µs) | 613.8 (536.0–50595) n=11451 (mean: the p50 is under the noise floor, 232.5 µs) | 135.5 (129.9–169.7) n=11325 (mean: the p50 is under the noise floor, 51.9 µs) | 55.2 (34.5–73.5) n=11277 | 72.0 (64.3–85.1) n=10957 (mean: the p50 is under the noise floor, 25.2 µs) | 89.8 (70.8–102.9) n=11065 (mean: the p50 is under the noise floor, 34.2 µs) | caddy-h3 55.2 |
| 100k/0 | cpu p99 (µs) | 15534 (11296–53634) n=11451 | 2719 (2701–3139) n=11325 | 3416 (3409–3949) n=11277 | 1394 (1180–1658) n=10957 | 1506 (1092–1717) n=11065 | h2o-h2 1394 |
| 100k/0 | cpu p99.9 (µs) | 38764 (16335–8491387) n=11451 | 27069 (6665–48240) n=11325 | 14639 (12069–56070) n=11277 | 5253 (4544–6990) n=10957 | 6899 (4021–17241) n=11065 | h2o-h2 5253 |
| 100k/1 | time p50 (ms) | 475.8 (462.9–487.5) n=19676 | 434.3 (433.5–435.3) n=21516 | 428.4 (426.7–429.1) n=21505 | 437.7 (433.1–440.0) n=21511 | 431.9 (430.5–432.4) n=21488 | caddy-h3 428.4 |
| 100k/1 | time p99 (ms) | 6329 (5596–7239) n=19676 | 1458 (1409–1597) n=21516 | 1411 (1331–1473) n=21505 | 1551 (1467–1631) n=21511 | 1483 (1361–1569) n=21488 | caddy-h3 1411 |
| 100k/1 | time p99.9 (ms) | 14209 (11739–15109) n=19676 | 1846 (1806–2114) n=21516 | 1831 (1750–2042) n=21505 | 1982 (1836–2038) n=21511 | 1805 (1781–1878) n=21488 | h2o-h3 1805 |
| 100k/1 | bytes p50 (B) | 946.0 (910.0–1567) n=18840 | 249.0 (247.0–250.0) n=19318 | 167.0 (167.0–168.0) n=19192 | 248.0 (248.0–249.0) n=18539 | 169.0 (168.0–169.0) n=18774 | caddy-h3 167.0 |
| 100k/1 | bytes p99 (B) | 13626 (13572–13948) n=18840 | 2706 (2565–2742) n=19318 | 2532 (2379–2712) n=19192 | 2508 (2448–2606) n=18539 | 2226 (2080–2249) n=18774 | h2o-h3 2226 |
| 100k/1 | bytes p99.9 (B) | 115815 (83899–119197) n=18840 | 5356 (5231–5454) n=19318 | 5365 (5217–5396) n=19192 | 5657 (4763–5660) n=18539 | 5356 (5354–5358) n=18774 | caddy-h2 5356 |
| 100k/1 | cpu p50 (µs) | 1291 (1038–3738) n=13499 (mean: the p50 is under the noise floor, 245.7 µs) | 140.8 (129.1–170.8) n=11380 (mean: the p50 is under the noise floor, 42.2 µs) | 240.4 (205.9–279.8) n=11228 (mean: the p50 is under the noise floor, 59.9 µs) | 69.2 (46.9–81.8) n=11002 (mean: the p50 is under the noise floor, 13.7 µs) | 87.5 (73.3–130.4) n=11097 (mean: the p50 is under the noise floor, 38.2 µs) | h2o-h2 69.2 (mean) |
| 100k/1 | cpu p99 (µs) | 20498 (14368–65195) n=13499 | 2889 (2560–2998) n=11380 | 3567 (2995–3797) n=11228 | 1156 (1072–1461) n=11002 | 1438 (1218–1964) n=11097 | h2o-h2 1156 |
| 100k/1 | cpu p99.9 (µs) | 50857 (35110–292869) n=13499 | 26503 (6152–38456) n=11380 | 20576 (6685–46992) n=11228 | 4241 (3074–7463) n=11002 | 6804 (3820–10003) n=11097 | h2o-h2 4241 |
| 100k/5 | time p50 (ms) | 519.8 (514.6–529.1) n=19981 | 459.5 (454.6–467.2) n=21573 | 436.3 (434.5–436.8) n=21556 | 467.4 (465.3–471.2) n=21582 | 441.8 (439.6–444.7) n=21562 | caddy-h3 436.3 |
| 100k/5 | time p99 (ms) | 7459 (6189–8770) n=19981 | 2559 (2338–2707) n=21573 | 1849 (1811–2019) n=21556 | 2416 (2291–2837) n=21582 | 2007 (1867–2164) n=21562 | caddy-h3 1849 |
| 100k/5 | time p99.9 (ms) | 17472 (14432–20184) n=19981 | 3790 (3578–6132) n=21573 | 2784 (2595–2898) n=21556 | 3715 (3455–9780) n=21582 | 3215 (3178–3271) n=21562 | caddy-h3 2784 |
| 100k/5 | bytes p50 (B) | 1206 (976.0–1474) n=17511 | 250.0 (249.0–250.0) n=16322 | 171.0 (170.3–171.0) n=18383 | 248.0 (248.0–249.0) n=15614 | 169.0 (169.0–170.0) n=17522 | h2o-h3 169.0 |
| 100k/5 | bytes p99 (B) | 11885 (11375–12975) n=17511 | 2389 (2018–2626) n=16322 | 2800 (2734–2885) n=18383 | 2489 (2139–2502) n=15614 | 2085 (2044–2200) n=17522 | h2o-h3 2085 |
| 100k/5 | bytes p99.9 (B) | 55191 (38738–73992) n=17511 | 5100 (4431–5135) n=16322 | 5309 (5206–5381) n=18383 | 4960 (3076–5510) n=15614 | 5207 (3598–5289) n=17522 | h2o-h2 4960 |
| 100k/5 | cpu p50 (µs) | 1140 (938.7–1998) n=13584 (mean: the p50 is under the noise floor, 195.2 µs) | 176.5 (161.1–201.9) n=11101 (mean: the p50 is under the noise floor, 9.66 µs) | 79.4 (48.4–92.3) n=11241 | 86.0 (73.6–92.0) n=10735 (mean: the p50 is under the noise floor, 7.47 µs) | 20.6 (17.3–22.1) n=11089 | h2o-h3 20.6 |
| 100k/5 | cpu p99 (µs) | 24016 (15490–37594) n=13584 | 2849 (2696–3262) n=11101 | 3622 (3265–3850) n=11241 | 1400 (1136–1526) n=10735 | 1635 (1413–1717) n=11089 | h2o-h2 1400 |
| 100k/5 | cpu p99.9 (µs) | 34166 (29702–66962) n=13584 | 10250 (8559–13112) n=11101 | 22151 (9391–35017) n=11241 | 6283 (5044–7846) n=10735 | 3909 (3230–7459) n=11089 | h2o-h3 3909 |
| 50k/0 | time p50 (ms) | 529.4 (485.2–687.9) n=6648 | 445.6 (444.2–447.2) n=10681 | 438.5 (437.4–443.9) n=10683 | 450.1 (448.7–451.0) n=10684 | 440.3 (439.5–442.4) n=10696 | caddy-h3 438.5 |
| 50k/0 | time p99 (ms) | 23117 (21981–23534) n=6648 | 1012 (1004–1045) n=10681 | 1072 (979.3–1081) n=10683 | 1100 (1060–1224) n=10684 | 1177 (1059–1207) n=10696 | caddy-h2 1012 |
| 50k/0 | time p99.9 (ms) | 28389 (27800–29981) n=6648 | 1419 (1385–1461) n=10681 | 1437 (1397–1457) n=10683 | 1396 (1351–1423) n=10684 | 1355 (1332–1374) n=10696 | h2o-h3 1355 |
| 50k/0 | bytes p50 (B) | 2071 (1567–2674) n=6345 | 261.0 (261.0–285.0) n=9878 | 172.0 (170.0–174.0) n=8362 | 266.0 (252.0–270.0) n=10059 | 169.0 (169.0–170.0) n=9320 | h2o-h3 169.0 |
| 50k/0 | bytes p99 (B) | 22852 (16074–29325) n=6345 | 3316 (3203–3637) n=9878 | 2803 (1492–2962) n=8362 | 2652 (2573–2940) n=10059 | 2694 (2454–2782) n=9320 | h2o-h2 2652 |
| 50k/0 | bytes p99.9 (B) | 151669 (150623–153878) n=6345 | 5518 (5452–5688) n=9878 | 5447 (1612–6927) n=8362 | 5545 (3930–5548) n=10059 | 5353 (4743–5358) n=9320 | h2o-h3 5353 |
| 50k/0 | cpu p50 (µs) | 1005 (875.2–1213) n=5176 (mean: the p50 is under the noise floor, 297.3 µs) | 208.4 (154.0–553.3) n=5893 (mean: the p50 is under the noise floor, 47.9 µs) | 203.8 (187.4–1186) n=5887 (mean: the p50 is under the noise floor, 72.2 µs) | 46.4 (29.9–475.0) n=5637 (mean: the p50 is under the noise floor, 34.1 µs) | 73.9 (73.2–88.7) n=5682 (mean: the p50 is under the noise floor, 33.0 µs) | h2o-h2 46.4 (mean) |
| 50k/0 | cpu p99 (µs) | 14379 (12516–18930) n=5176 | 4212 (3115–6756) n=5893 | 3972 (3701–15830) n=5887 | 1242 (813.5–4838) n=5637 | 1463 (1428–1551) n=5682 | h2o-h2 1242 |
| 50k/0 | cpu p99.9 (µs) | 31714 (17180–422004) n=5176 | 44546 (26571–47173) n=5893 | 51549 (27213–146212) n=5887 | 21449 (7942–24768) n=5637 | 18108 (8381–30548) n=5682 | h2o-h3 18108 |
| 50k/1 | time p50 (ms) | 720.1 (522.7–810.3) n=5784 | 447.9 (445.9–452.1) n=10757 | 442.0 (439.2–443.6) n=10701 | 455.9 (452.8–457.4) n=10744 | 443.4 (441.9–444.7) n=10703 | caddy-h3 442.0 |
| 50k/1 | time p99 (ms) | 21748 (21707–23747) n=5784 | 1869 (1700–2150) n=10757 | 1468 (1369–1630) n=10701 | 2141 (1870–2329) n=10744 | 1499 (1374–1875) n=10703 | caddy-h3 1468 |
| 50k/1 | time p99.9 (ms) | 27177 (26188–28854) n=5784 | 3219 (3130–3627) n=10757 | 2370 (2298–3376) n=10701 | 3101 (2780–4300) n=10744 | 2354 (2318–2491) n=10703 | h2o-h3 2354 |
| 50k/1 | bytes p50 (B) | 2082 (1986–3241) n=5468 | 261.0 (260.0–287.0) n=9049 | 172.0 (171.0–173.0) n=10091 | 260.0 (253.0–283.0) n=8963 | 169.0 (169.0–169.0) n=9240 | h2o-h3 169.0 |
| 50k/1 | bytes p99 (B) | 25585 (22595–32111) n=5468 | 3131 (1964–3256) n=9049 | 3104 (2881–3360) n=10091 | 2571 (2511–2645) n=8963 | 2790 (2687–3827) n=9240 | h2o-h2 2571 |
| 50k/1 | bytes p99.9 (B) | 148228 (139789–186767) n=5468 | 5202 (5134–5646) n=9049 | 5446 (5131–5448) n=10091 | 5479 (5033–5548) n=8963 | 5357 (5208–5357) n=9240 | caddy-h2 5202 |
| 50k/1 | cpu p50 (µs) | 3441 (1086–7510) n=4522 (mean: the p50 is under the noise floor, 1169 µs) | 232.1 (122.6–1184) n=5890 (mean: the p50 is under the noise floor, 7.59 µs) | 256.7 (153.2–694.3) n=5905 (mean: the p50 is under the noise floor, 70.0 µs) | 100.6 (50.2–429.2) n=5708 (mean: the p50 is under the noise floor, 5.84 µs) | 77.4 (49.1–135.1) n=5758 (mean: the p50 is under the noise floor, 35.1 µs) | h2o-h3 77.4 (mean) |
| 50k/1 | cpu p99 (µs) | 100472 (71127–131352) n=4522 | 3728 (2888–13794) n=5890 | 3857 (3526–8060) n=5905 | 1693 (1106–4647) n=5708 | 1171 (1044–1847) n=5758 | h2o-h3 1171 |
| 50k/1 | cpu p99.9 (µs) | 217972 (162422–1132886) n=4522 | 39595 (37281–165052) n=5890 | 26968 (15489–70511) n=5905 | 10900 (7399–25734) n=5708 | 17938 (10137–27527) n=5758 | h2o-h2 10900 |
| 50k/5 | time p50 (ms) | 654.8 (517.5–665.2) n=8522 | 461.8 (455.3–470.2) n=10766 | 448.5 (446.3–452.5) n=10757 | 471.2 (465.8–475.5) n=10740 | 451.4 (448.1–455.8) n=10741 | caddy-h3 448.5 |
| 50k/5 | time p99 (ms) | 20940 (19721–21832) n=8522 | 3164 (2883–3222) n=10766 | 2375 (2357–2643) n=10757 | 3196 (2837–3393) n=10740 | 2379 (2360–2623) n=10741 | caddy-h3 2375 |
| 50k/5 | time p99.9 (ms) | 29223 (27228–33189) n=8522 | 4211 (3409–4430) n=10766 | 3896 (3368–4159) n=10757 | 4088 (3872–5515) n=10740 | 3448 (3353–4229) n=10741 | h2o-h3 3448 |
| 50k/5 | bytes p50 (B) | 2413 (1567–2697) n=7446 | 279.0 (264.0–286.0) n=9405 | 173.0 (172.0–173.0) n=9867 | 270.0 (262.0–285.0) n=8658 | 170.0 (169.0–172.0) n=8971 | h2o-h3 170.0 |
| 50k/5 | bytes p99 (B) | 20497 (17862–21567) n=7446 | 2812 (2588–3092) n=9405 | 3123 (3052–3224) n=9867 | 2624 (2510–2708) n=8658 | 2692 (2593–2789) n=8971 | h2o-h2 2624 |
| 50k/5 | bytes p99.9 (B) | 120100 (83547–136594) n=7446 | 5204 (5134–5700) n=9405 | 5291 (5225–6932) n=9867 | 5403 (4764–5614) n=8658 | 5356 (5214–5361) n=8971 | caddy-h2 5204 |
| 50k/5 | cpu p50 (µs) | 1008 (719.4–1062) n=6643 (mean: the p50 is under the noise floor, 260.3 µs) | 225.2 (204.7–238.6) n=6028 (mean: the p50 is under the noise floor, 7.92 µs) | 315.8 (233.0–1649) n=6085 (mean: the p50 is under the noise floor, 13.3 µs) | 119.3 (61.6–404.0) n=5796 (mean: the p50 is under the noise floor, 12.4 µs) | 122.8 (70.1–514.1) n=5905 (mean: the p50 is under the noise floor, 8.27 µs) | h2o-h2 119.3 (mean) |
| 50k/5 | cpu p99 (µs) | 14348 (11525–16305) n=6643 | 3776 (2826–3983) n=6028 | 4531 (3651–15360) n=6085 | 1542 (1441–3219) n=5796 | 1606 (1441–5817) n=5905 | h2o-h2 1542 |
| 50k/5 | cpu p99.9 (µs) | 28599 (16167–37806) n=6643 | 26342 (21126–49850) n=6028 | 16325 (14433–310652) n=6085 | 10959 (9375–16608) n=5796 | 10380 (5044–176469) n=5905 | h2o-h3 10380 |
| good/0 | time p50 (ms) | 10.4 (10.3–10.5) n=68391 | 11.6 (11.6–12.4) n=72000 | 10.5 (10.5–10.6) n=72000 | 11.6 (11.6–11.7) n=72000 | 10.6 (10.5–10.7) n=72000 | kiln-today 10.4 |
| good/0 | time p99 (ms) | 14.6 (12.3–1025) n=68391 | 12.9 (12.8–28.2) n=72000 | 11.7 (11.4–20.3) n=72000 | 13.5 (13.1–15.2) n=72000 | 13.9 (11.9–19.7) n=72000 | caddy-h3 11.7 |
| good/0 | time p99.9 (ms) | 567.6 (24.5–2207) n=68391 band 400.9–764.8 | 17.8 (15.7–74.3) n=72000 band 16.5–21.5 | 16.5 (16.0–29.1) n=72000 band 15.7–19.7 | 16.5 (13.8–24.8) n=72000 band 16.0–19.4 | 20.6 (13.2–41.5) n=72000 band 19.8–23.1 | h2o-h2 16.5 |
| good/0 | bytes p50 (B) | 472.0 (253.0–472.0) n=47499 | 185.0 (185.0–186.0) n=51762 | 165.0 (164.0–166.0) n=43397 | 183.0 (183.0–183.0) n=39972 | 169.0 (169.0–169.0) n=43911 | caddy-h3 165.0 |
| good/0 | bytes p99 (B) | 13863 (13483–14685) n=47499 | 1592 (1328–2367) n=51762 | 1574 (1501–1707) n=43397 | 1790 (1717–1893) n=39972 | 1662 (1266–1702) n=43911 | caddy-h3 1574 |
| good/0 | bytes p99.9 (B) | 24626 (17669–85533) n=47499 | 3172 (2924–5385) n=51762 | 2872 (2757–2882) n=43397 | 3454 (2049–4016) n=39972 | 3082 (2171–3927) n=43911 | caddy-h3 2872 |
| good/0 | cpu p50 (µs) | 2826 (360.9–3257) n=12820 (mean: the p50 is under the noise floor, 3655 µs) | 108.9 (105.8–111.1) n=7150 | 167.8 (161.7–181.1) n=6918 | 39.7 (38.2–45.3) n=6898 | 55.2 (50.1–63.7) n=6966 | h2o-h2 39.7 |
| good/0 | cpu p99 (µs) | 67888 (8624–96176) n=12820 | 521.1 (469.8–770.9) n=7150 | 582.4 (501.3–1004) n=6918 | 304.0 (233.1–309.8) n=6898 | 266.8 (203.4–403.4) n=6966 | h2o-h3 266.8 |
| good/0 | cpu p99.9 (µs) | 126691 (13402–161436) n=12820 | 2156 (1010–2607) n=7150 | 1614 (913.0–8194) n=6918 | 625.4 (444.4–784.1) n=6898 | 1174 (415.9–4053) n=6966 | h2o-h2 625.4 |
| good/1 | time p50 (ms) | 10.4 (10.3–10.4) n=66690 | 11.6 (11.6–11.8) n=71985 | 10.6 (10.5–10.6) n=71979 | 11.7 (11.6–11.7) n=72000 | 10.5 (10.5–10.6) n=72000 | kiln-today 10.4 |
| good/1 | time p99 (ms) | 51.6 (51.5–71.6) n=66690 | 41.7 (39.5–42.9) n=71985 | 33.4 (30.9–38.4) n=71979 | 39.8 (39.5–57.2) n=72000 | 37.3 (37.1–38.4) n=72000 | caddy-h3 33.4 |
| good/1 | time p99.9 (ms) | 125.4 (112.1–224.6) n=66690 band 84.3–231.2 | 85.3 (82.7–133.0) n=71985 band 82.5–181.6 | 67.3 (65.3–73.5) n=71979 band 64.0–81.1 | 126.0 (83.2–137.7) n=72000 band 82.7–202.0 | 76.1 (68.0–81.0) n=72000 band 66.0–81.2 | caddy-h3 67.3 |
| good/1 | bytes p50 (B) | 253.0 (253.0–472.0) n=55880 | 185.0 (185.0–191.0) n=44615 | 166.0 (165.0–170.0) n=39370 | 183.0 (183.0–183.0) n=51296 | 169.0 (169.0–170.0) n=43775 | caddy-h3 166.0 |
| good/1 | bytes p99 (B) | 12863 (12766–13089) n=55880 | 1569 (1524–2088) n=44615 | 1538 (1284–1705) n=39370 | 1902 (1722–2063) n=51296 | 1812 (1713–1853) n=43775 | caddy-h3 1538 |
| good/1 | bytes p99.9 (B) | 32707 (16725–37706) n=55880 | 3656 (2853–4826) n=44615 | 2947 (2701–3392) n=39370 | 4409 (4002–4592) n=51296 | 3901 (3201–4216) n=43775 | caddy-h3 2947 |
| good/1 | cpu p50 (µs) | 423.6 (336.3–1495) n=12555 (mean: the p50 is under the noise floor, 279.5 µs) | 110.5 (105.0–124.8) n=7455 | 173.4 (165.6–188.6) n=7197 | 41.6 (38.3–48.3) n=7425 | 54.2 (52.1–58.9) n=7156 | h2o-h2 41.6 |
| good/1 | cpu p99 (µs) | 7387 (4658–25393) n=12555 | 629.6 (545.5–799.9) n=7455 | 1213 (799.7–1250) n=7197 | 367.5 (297.9–412.3) n=7425 | 443.5 (327.1–458.7) n=7156 | h2o-h2 367.5 |
| good/1 | cpu p99.9 (µs) | 17394 (12474–48357) n=12555 | 2479 (1599–6176) n=7455 | 2481 (2019–2944) n=7197 | 1047 (611.1–1624) n=7425 | 1516 (1169–1945) n=7156 | h2o-h2 1047 |
| good/5 | time p50 (ms) | 10.3 (10.3–10.5) n=65258 | 11.7 (11.6–12.4) n=72000 | 10.6 (10.5–10.7) n=71998 | 11.7 (11.6–12.8) n=71999 | 10.5 (10.5–13.0) n=72000 | kiln-today 10.3 |
| good/5 | time p99 (ms) | 305.9 (251.0–6440) n=65258 | 287.0 (261.8–307.3) n=72000 | 58.9 (58.4–62.9) n=71998 | 258.6 (234.8–465.7) n=71999 | 64.0 (63.2–82.1) n=72000 | caddy-h3 58.9 |
| good/5 | time p99.9 (ms) | 1189 (801.7–7788) n=65258 band 950.9–1510 | 1186 (683.9–2970) n=72000 band 918.1–1463 | 91.7 (81.8–103.1) n=71998 band 81.5–117.8 | 670.9 (573.7–1296) n=71999 band 626.3–692.4 | 101.5 (93.6–131.6) n=72000 band 88.1–131.0 | caddy-h3 91.7 |
| good/5 | bytes p50 (B) | 429.0 (294.0–472.0) n=45586 | 185.0 (185.0–189.0) n=46810 | 168.0 (168.0–169.0) n=51797 | 183.0 (183.0–183.0) n=47430 | 170.0 (170.0–171.0) n=52167 | caddy-h3 168.0 |
| good/5 | bytes p99 (B) | 9856 (9196–11352) n=45586 | 1776 (1697–1881) n=46810 | 1644 (1560–1707) n=51797 | 1978 (1799–2064) n=47430 | 1853 (1678–2202) n=52167 | caddy-h3 1644 |
| good/5 | bytes p99.9 (B) | 17570 (14782–39172) n=45586 | 3829 (3278–4546) n=46810 | 3660 (3067–4668) n=51797 | 4006 (3806–4180) n=47430 | 3687 (3158–4046) n=52167 | caddy-h3 3660 |
| good/5 | cpu p50 (µs) | 333.0 (249.6–439.9) n=12426 (mean: the p50 is under the noise floor, 363.2 µs) | 116.2 (108.2–121.9) n=8927 | 192.6 (180.3–225.3) n=7763 | 42.8 (40.6–47.9) n=8713 | 57.8 (53.9–74.5) n=7953 | h2o-h2 42.8 |
| good/5 | cpu p99 (µs) | 5466 (3784–8083) n=12426 | 924.4 (865.5–1424) n=8927 | 1576 (1344–1606) n=7763 | 491.0 (404.2–555.6) n=8713 | 500.8 (482.2–810.9) n=7953 | h2o-h2 491.0 |
| good/5 | cpu p99.9 (µs) | 16212 (8718–16735) n=12426 | 3312 (2319–4117) n=8927 | 2331 (1795–3810) n=7763 | 1196 (967.3–2042) n=8713 | 1598 (982.5–3845) n=7953 | h2o-h2 1196 |

#### stream in webkit

| condition | measure | kiln-today | caddy-h2 | caddy-h3 | h2o-h2 | h2o-h3 | best |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100k/0 | time p50 (ms) | 493.4 (451.5–541.8) n=17217 | 430.5 (428.9–435.3) n=21515 | - | 430.8 (430.5–431.6) n=21543 | - | caddy-h2 430.5 |
| 100k/0 | time p99 (ms) | 12234 (5286–15828) n=17217 | 697.3 (688.8–708.1) n=21515 | - | 721.3 (713.4–730.4) n=21543 | - | caddy-h2 697.3 |
| 100k/0 | time p99.9 (ms) | 19744 (6402–23530) n=17217 | 900.7 (866.5–926.5) n=21515 | - | 906.0 (883.6–954.8) n=21543 | - | caddy-h2 900.7 |
| 100k/0 | bytes p50 (B) | 1348 (1348–2081) n=15734 | 239.0 (239.0–239.0) n=20175 | - | 184.0 (184.0–184.0) n=19395 | - | h2o-h2 184.0 |
| 100k/0 | bytes p99 (B) | 16437 (14682–17116) n=15734 | 2701 (2673–2704) n=20175 | - | 1994 (1758–1999) n=19395 | - | h2o-h2 1994 |
| 100k/0 | bytes p99.9 (B) | 27984 (18937–37703) n=15734 | 5332 (5330–5376) n=20175 | - | 4761 (3808–4765) n=19395 | - | h2o-h2 4761 |
| 100k/0 | cpu p50 (µs) | 715.5 (292.3–1405) n=11244 (mean: the p50 is under the noise floor, 201.6 µs) | 163.7 (151.3–165.1) n=11415 (mean: the p50 is under the noise floor, 1.34 µs) | - | 81.4 (70.4–94.3) n=11172 (mean: the p50 is under the noise floor, 0.29 µs) | - | h2o-h2 81.4 (mean) |
| 100k/0 | cpu p99 (µs) | 21416 (10655–43784) n=11244 | 3026 (2801–3307) n=11415 | - | 1356 (1340–1753) n=11172 | - | h2o-h2 1356 |
| 100k/0 | cpu p99.9 (µs) | 81145 (18051–94203) n=11244 | 17273 (5900–20847) n=11415 | - | 5382 (2932–12951) n=11172 | - | h2o-h2 5382 |
| 100k/1 | time p50 (ms) | 486.8 (481.0–493.6) n=20735 | 435.4 (435.0–436.8) n=21564 | - | 436.3 (435.0–437.5) n=21579 | - | caddy-h2 435.4 |
| 100k/1 | time p99 (ms) | 12203 (12144–13001) n=20735 | 1530 (1496–1690) n=21564 | - | 1594 (1446–1634) n=21579 | - | caddy-h2 1530 |
| 100k/1 | time p99.9 (ms) | 20480 (19755–21096) n=20735 | 2086 (1840–2384) n=21564 | - | 1968 (1832–2228) n=21579 | - | h2o-h2 1968 |
| 100k/1 | bytes p50 (B) | 946.0 (910.0–1348) n=19448 | 239.0 (239.0–239.0) n=19400 | - | 184.0 (184.0–184.0) n=18524 | - | h2o-h2 184.0 |
| 100k/1 | bytes p99 (B) | 15471 (15193–15783) n=19448 | 2636 (2418–2702) n=19400 | - | 1999 (1756–2000) n=18524 | - | h2o-h2 1999 |
| 100k/1 | bytes p99.9 (B) | 42720 (24491–53217) n=19448 | 5332 (4820–5381) n=19400 | - | 5409 (3808–5409) n=18524 | - | caddy-h2 5332 |
| 100k/1 | cpu p50 (µs) | 1341 (790.1–1831) n=13892 (mean: the p50 is under the noise floor, 364.3 µs) | 176.1 (147.9–188.7) n=11349 (mean: the p50 is under the noise floor, 7.02 µs) | - | 85.4 (64.2–118.9) n=11188 (mean: the p50 is under the noise floor, 7.53 µs) | - | h2o-h2 85.4 (mean) |
| 100k/1 | cpu p99 (µs) | 22962 (17597–50851) n=13892 | 3200 (2876–3538) n=11349 | - | 1443 (1234–1669) n=11188 | - | h2o-h2 1443 |
| 100k/1 | cpu p99.9 (µs) | 69296 (61879–116096) n=13892 | 16296 (7308–36765) n=11349 | - | 4407 (3273–7840) n=11188 | - | h2o-h2 4407 |
| 100k/5 | time p50 (ms) | 545.1 (533.4–580.8) n=20598 | 459.8 (457.6–465.7) n=21582 | - | 465.4 (462.7–473.1) n=21581 | - | caddy-h2 459.8 |
| 100k/5 | time p99 (ms) | 13793 (13211–14557) n=20598 | 2480 (2292–2549) n=21582 | - | 2595 (2285–2873) n=21581 | - | caddy-h2 2480 |
| 100k/5 | time p99.9 (ms) | 24743 (21479–25233) n=20598 | 3854 (3697–5362) n=21582 | - | 3812 (3195–3954) n=21581 | - | h2o-h2 3812 |
| 100k/5 | bytes p50 (B) | 1179 (997.0–1408) n=17578 | 239.0 (239.0–239.0) n=16417 | - | 184.0 (184.0–184.0) n=15683 | - | h2o-h2 184.0 |
| 100k/5 | bytes p99 (B) | 13322 (12831–13887) n=17578 | 2417 (2334–2662) n=16417 | - | 1921 (1802–1997) n=15683 | - | h2o-h2 1921 |
| 100k/5 | bytes p99.9 (B) | 26385 (25368–38015) n=17578 | 4819 (4732–5066) n=16417 | - | 4760 (3868–5404) n=15683 | - | h2o-h2 4760 |
| 100k/5 | cpu p50 (µs) | 1636 (1057–2609) n=13661 (mean: the p50 is under the noise floor, 827.8 µs) | 154.5 (140.5–163.1) n=11028 (mean: the p50 is under the noise floor, 7.08 µs) | - | 79.0 (65.4–94.7) n=10759 (mean: the p50 is under the noise floor, 9.49 µs) | - | h2o-h2 79.0 (mean) |
| 100k/5 | cpu p99 (µs) | 20820 (14757–59916) n=13661 | 2723 (2710–3297) n=11028 | - | 1535 (1066–1603) n=10759 | - | h2o-h2 1535 |
| 100k/5 | cpu p99.9 (µs) | 47222 (41539–99447) n=13661 | 8419 (7400–9904) n=11028 | - | 4077 (3072–5386) n=10759 | - | h2o-h2 4077 |
| 50k/0 | time p50 (ms) | 546.6 (514.5–719.1) n=6745 | 448.9 (448.3–450.5) n=6942 | - | 447.5 (446.2–449.9) n=6950 | - | h2o-h2 447.5 |
| 50k/0 | time p99 (ms) | 22952 (22067–25002) n=6745 | 1014 (1003–1059) n=6942 | - | 1091 (1048–1117) n=6950 | - | caddy-h2 1014 |
| 50k/0 | time p99.9 (ms) | 32368 (30926–34950) n=6745 | 1374 (1359–1398) n=6942 | - | 1383 (1369–1430) n=6950 | - | caddy-h2 1374 |
| 50k/0 | bytes p50 (B) | 2662 (2224–2881) n=5984 | 239.0 (239.0–239.0) n=6420 | - | 185.0 (185.0–185.0) n=6305 | - | h2o-h2 185.0 |
| 50k/0 | bytes p99 (B) | 14552 (13834–15376) n=5984 | 3140 (2964–3143) n=6420 | - | 2487 (2404–2575) n=6305 | - | h2o-h2 2487 |
| 50k/0 | bytes p99.9 (B) | 29184 (29110–29251) n=5984 | 5162 (5161–5332) n=6420 | - | 4624 (3871–5556) n=6305 | - | h2o-h2 4624 |
| 50k/0 | cpu p50 (µs) | 7653 (-6826–12063) n=5805 (mean: the p50 is under the noise floor, 11265 µs) | 323.9 (250.4–427.5) n=6251 (mean: the p50 is under the noise floor, 41.7 µs) | - | 286.4 (220.9–304.0) n=5852 (mean: the p50 is under the noise floor, 24.1 µs) | - | h2o-h2 286.4 (mean) |
| 50k/0 | cpu p99 (µs) | 81977 (29859–102819) n=5805 | 2641 (2350–2829) n=6251 | - | 1519 (1407–1614) n=5852 | - | h2o-h2 1519 |
| 50k/0 | cpu p99.9 (µs) | 424407 (84053–1450304) n=5805 | 50042 (36861–54649) n=6251 | - | 40264 (36116–53572) n=5852 | - | h2o-h2 40264 |
| 50k/1 | time p50 (ms) | 656.6 (545.8–729.2) n=6758 | 454.2 (449.2–455.1) n=6957 | - | 450.2 (448.1–451.9) n=6949 | - | h2o-h2 450.2 |
| 50k/1 | time p99 (ms) | 22662 (22089–24287) n=6758 | 1903 (1862–2245) n=6957 | - | 1926 (1465–2162) n=6949 | - | caddy-h2 1903 |
| 50k/1 | time p99.9 (ms) | 31813 (30974–34694) n=6758 | 3257 (2474–3310) n=6957 | - | 3060 (2620–3347) n=6949 | - | h2o-h2 3060 |
| 50k/1 | bytes p50 (B) | 2445 (2233–3013) n=5906 | 239.0 (239.0–239.0) n=6668 | - | 185.0 (185.0–185.0) n=6065 | - | h2o-h2 185.0 |
| 50k/1 | bytes p99 (B) | 14145 (13655–15800) n=5906 | 3142 (2965–3144) n=6668 | - | 2424 (2391–2493) n=6065 | - | h2o-h2 2424 |
| 50k/1 | bytes p99.9 (B) | 29051 (29049–29266) n=5906 | 5161 (5159–5162) n=6668 | - | 4619 (3866–4763) n=6065 | - | h2o-h2 4619 |
| 50k/1 | cpu p50 (µs) | 6961 (1642–16769) n=5933 (mean: the p50 is under the noise floor, 9914 µs) | 364.0 (295.0–387.0) n=6090 (mean: the p50 is under the noise floor, 49.6 µs) | - | 291.9 (242.7–361.1) n=5725 (mean: the p50 is under the noise floor, 35.6 µs) | - | h2o-h2 291.9 (mean) |
| 50k/1 | cpu p99 (µs) | 81218 (36065–150577) n=5933 | 2711 (2600–4364) n=6090 | - | 1537 (1361–1691) n=5725 | - | h2o-h2 1537 |
| 50k/1 | cpu p99.9 (µs) | 229207 (152584–1032476) n=5933 | 47233 (39416–59039) n=6090 | - | 50935 (37959–65363) n=5725 | - | caddy-h2 47233 |
| 50k/5 | time p50 (ms) | 771.3 (706.1–808.6) n=6945 | 460.0 (458.8–464.8) n=6960 | - | 467.3 (463.7–471.4) n=6975 | - | caddy-h2 460.0 |
| 50k/5 | time p99 (ms) | 23481 (21079–25547) n=6945 | 3054 (2755–3273) n=6960 | - | 3223 (2952–3398) n=6975 | - | caddy-h2 3054 |
| 50k/5 | time p99.9 (ms) | 33072 (30844–35555) n=6945 | 4258 (3662–4512) n=6960 | - | 4252 (3753–5432) n=6975 | - | h2o-h2 4252 |
| 50k/5 | bytes p50 (B) | 2333 (2169–2700) n=5725 | 239.0 (239.0–239.0) n=6192 | - | 185.0 (185.0–185.0) n=5548 | - | h2o-h2 185.0 |
| 50k/5 | bytes p99 (B) | 13441 (12544–13755) n=5725 | 2960 (2680–3142) n=6192 | - | 2136 (1834–2265) n=5548 | - | h2o-h2 2136 |
| 50k/5 | bytes p99.9 (B) | 26059 (20905–29116) n=5725 | 5159 (4740–5162) n=6192 | - | 4762 (3930–4764) n=5548 | - | h2o-h2 4762 |
| 50k/5 | cpu p50 (µs) | 6035 (531.0–13788) n=5664 (mean: the p50 is under the noise floor, 14347 µs) | 335.5 (312.7–478.0) n=5728 (mean: the p50 is under the noise floor, 75.7 µs) | - | 286.4 (260.4–363.9) n=5285 (mean: the p50 is under the noise floor, 57.1 µs) | - | h2o-h2 286.4 (mean) |
| 50k/5 | cpu p99 (µs) | 106586 (73226–220611) n=5664 | 2659 (2146–3488) n=5728 | - | 1328 (1270–1611) n=5285 | - | h2o-h2 1328 |
| 50k/5 | cpu p99.9 (µs) | 198448 (148560–1355882) n=5664 | 41587 (36114–57275) n=5728 | - | 54589 (43288–58314) n=5285 | - | caddy-h2 41587 |
| good/0 | time p50 (ms) | 10.6 (10.4–11.4) n=66403 | 10.6 (10.5–13.4) n=70047 | - | 10.8 (10.7–10.8) n=71961 | - | kiln-today 10.6 |
| good/0 | time p99 (ms) | 45.9 (13.7–7506) n=66403 | 12.2 (11.8–34.0) n=70047 | - | 13.2 (12.2–23.8) n=71961 | - | caddy-h2 12.2 |
| good/0 | time p99.9 (ms) | 163.2 (26.6–8747) n=66403 band 144.5–174.1 | 22.3 (14.8–157.4) n=70047 band 18.9–34.7 | - | 21.4 (13.4–66.2) n=71961 band 17.5–23.9 | - | h2o-h2 21.4 |
| good/0 | bytes p50 (B) | 472.0 (472.0–472.0) n=42720 | 239.0 (239.0–239.0) n=34543 | - | 183.0 (183.0–183.0) n=43329 | - | h2o-h2 183.0 |
| good/0 | bytes p99 (B) | 14262 (14045–15413) n=42720 | 1544 (1516–1901) n=34543 | - | 1869 (1559–1936) n=43329 | - | caddy-h2 1544 |
| good/0 | bytes p99.9 (B) | 19283 (18130–72680) n=42720 | 3020 (2152–5745) n=34543 | - | 3328 (3220–5712) n=43329 | - | caddy-h2 3020 |
| good/0 | cpu p50 (µs) | 2907 (1564–3140) n=12735 (mean: the p50 is under the noise floor, 2093 µs) | 104.0 (74.0–115.2) n=7282 | - | 42.3 (37.2–45.0) n=7102 | - | h2o-h2 42.3 |
| good/0 | cpu p99 (µs) | 28265 (16074–61845) n=12735 | 803.5 (440.7–1229) n=7282 | - | 326.5 (252.5–376.8) n=7102 | - | h2o-h2 326.5 |
| good/0 | cpu p99.9 (µs) | 45489 (33667–152851) n=12735 | 1981 (1768–5533) n=7282 | - | 608.7 (510.5–1127) n=7102 | - | h2o-h2 608.7 |
| good/1 | time p50 (ms) | 10.7 (10.5–11.2) n=64959 | 10.7 (10.6–10.8) n=70742 | - | 10.8 (10.6–13.3) n=71518 | - | kiln-today 10.7 |
| good/1 | time p99 (ms) | 79.9 (39.9–96.9) n=64959 | 40.9 (38.7–60.3) n=70742 | - | 41.4 (39.1–53.0) n=71518 | - | caddy-h2 40.9 |
| good/1 | time p99.9 (ms) | 216.2 (162.5–231.9) n=64959 band 161.2–273.0 | 147.4 (81.9–224.4) n=70742 band 92.1–225.0 | - | 86.7 (82.2–127.2) n=71518 band 81.9–197.6 | - | h2o-h2 86.7 |
| good/1 | bytes p50 (B) | 472.0 (253.0–472.0) n=54401 | 239.0 (239.0–239.0) n=30129 | - | 183.0 (183.0–183.0) n=39648 | - | h2o-h2 183.0 |
| good/1 | bytes p99 (B) | 13800 (12576–14263) n=54401 | 1457 (1143–1575) n=30129 | - | 1796 (1634–2201) n=39648 | - | caddy-h2 1457 |
| good/1 | bytes p99.9 (B) | 62286 (18301–85194) n=54401 | 3676 (1971–5151) n=30129 | - | 4227 (3649–5604) n=39648 | - | caddy-h2 3676 |
| good/1 | cpu p50 (µs) | 481.7 (-670.5–654.8) n=12344 (mean: the p50 is under the noise floor, 401.2 µs) | 112.3 (73.1–127.1) n=7676 | - | 42.8 (41.9–52.8) n=7770 | - | h2o-h2 42.8 |
| good/1 | cpu p99 (µs) | 11906 (9612–25705) n=12344 | 894.0 (847.5–979.8) n=7676 | - | 457.5 (354.7–935.8) n=7770 | - | h2o-h2 457.5 |
| good/1 | cpu p99.9 (µs) | 38073 (27783–55800) n=12344 | 1891 (1406–2999) n=7676 | - | 1600 (1224–5077) n=7770 | - | h2o-h2 1600 |
| good/5 | time p50 (ms) | 10.6 (10.4–10.8) n=62739 | 10.7 (10.6–10.7) n=71934 | - | 10.9 (10.7–12.7) n=71941 | - | kiln-today 10.6 |
| good/5 | time p99 (ms) | 323.9 (262.1–2960) n=62739 | 261.1 (208.5–583.9) n=71934 | - | 259.5 (231.5–371.7) n=71941 | - | h2o-h2 259.5 |
| good/5 | time p99.9 (ms) | 1195 (1068–4558) n=62739 band 833.1–1585 | 670.5 (516.3–2981) n=71934 band 635.4–685.9 | - | 637.0 (521.5–761.6) n=71941 band 599.3–706.7 | - | h2o-h2 637.0 |
| good/5 | bytes p50 (B) | 472.0 (259.0–625.0) n=41581 | 239.0 (239.0–239.0) n=51090 | - | 183.0 (183.0–183.0) n=50852 | - | h2o-h2 183.0 |
| good/5 | bytes p99 (B) | 11132 (10098–12761) n=41581 | 1721 (1659–1764) n=51090 | - | 1959 (1796–2038) n=50852 | - | caddy-h2 1721 |
| good/5 | bytes p99.9 (B) | 23189 (20397–33247) n=41581 | 3391 (3301–3967) n=51090 | - | 3995 (3779–4135) n=50852 | - | caddy-h2 3391 |
| good/5 | cpu p50 (µs) | 370.8 (49.8–428.9) n=11363 (mean: the p50 is under the noise floor, 187.4 µs) | 117.2 (112.7–121.3) n=9016 | - | 42.2 (39.4–45.2) n=8961 | - | h2o-h2 42.2 |
| good/5 | cpu p99 (µs) | 6214 (4639–10967) n=11363 | 926.3 (709.8–1021) n=9016 | - | 447.3 (409.3–615.3) n=8961 | - | h2o-h2 447.3 |
| good/5 | cpu p99.9 (µs) | 14998 (10985–35561) n=11363 | 2378 (1728–2939) n=9016 | - | 1243 (1097–2793) n=8961 | - | h2o-h2 1243 |
<!-- browser-lab tables: end -->

### What the numbers say

Every number here is `pass1/say.py`'s, over the tables above (`kiln-lab browser report
docs/browser-lab/2026-11-06/matrix.json`): a ratio is kiln today's median over the best baseline's in
the same cell, its median over the cell's losses and browsers, with the least and the most.

- **Kiln today is the best target in 38 of the 486 numbers**: 32 of a request (most of them its
  bytes at p99 or p99.9, the largest answers) and a stream's
  p50 time on the good link in all 6 of its cells. h2o over HTTP/2 is the best in 177, Caddy over
  HTTP/2 in 133, Caddy over HTTP/3 in 79 and h2o over HTTP/3 in 59.
- **Bytes.** Kiln today sends JSON uncompressed (f8) and each baseline compresses it; a stream's
  event costs kiln today 2.6 times the best baseline's bytes at p50 on the good link, 6.8 at
  100 kbit/s and 12.9 at 50 kbit/s (12.3 to 14.4); a burst's message 11.2, 5.7 and 7.8; a request,
  2.6, 13.1 and 6.8 at p50, while at p99 and p99.9 a request's bytes are about the same (0.9 to 1.6).
- **Time.** On the good link kiln today's p50 is the best baseline's (stream 0.99, burst 1.04,
  request 1.07) and its tail is not: p99 1.75, 2.99 and 2.10. On the slow links the bytes become
  time: a burst's message reaches the page 10.5 (100 kbit/s) and 10.9 times (50 kbit/s) later at
  p50 and 8.1 and 9.6 times at p99; a stream's event 6.8 and 13.4 times later at p99, though its p50
  is near (1.13, 1.46). Requests stay within about 2 (p50 2.08 at 50 kbit/s, p99 1.08 to 1.20). At
  50 and 100 kbit/s kiln today's stream times are lower bounds and its WebKit requests at 50 kbit/s
  are of the ones that completed ("Step 8: the matrix").
- **Server CPU.** Where the lab resolves it, kiln today spends 5 to 53 times the best baseline's
  CPU per operation at p99: a burst's message 5.1, 6.2 and 22.3 times (100 kbit/s, 50 kbit/s, the
  good link), a stream's event 15.9, 53.4 and 23.1, a request 16.3, 26.7 and 14.6. Many of its p50s,
  and some of the baselines', are under the background's noise and give the mean (step 4's rule).
- **HTTP/3 against HTTP/2** (the better of h2o and Caddy on each, Chromium): the same on a clean
  link (time p99 0.99 at 0% loss) and better as loss grows: time p99 0.91 at 1% and 0.75 at 5%
  (0.21 to 1.89 over the cells), p99.9 0.74 at 5%. Bytes are about the same (p50 0.90 to 0.91 at
  each loss).
- **For goal 10's bar** (a cell meets 1/10 when the whole interval of kiln's value over the best
  baseline's is at or under 0.1): the best baseline in nearly every cell is an HTTP/2 or HTTP/3
  server that compresses, and kiln today is near it or several times it, so the new protocol has
  to be ten times better than that server, not than kiln today.

### Where the run stands

**Pass 0, superseded.** The first pilot ran from 2026-11-05 03:37Z to 08:31Z (pilot-good done,
pilot-50k three rounds in), from a snapshot of the lab as it stood on 2026-11-04 (`lab-snap`), and
was stopped by its unit's name; its two parts are `docs/browser-lab/2026-11-05/pass0-pilot-good.json` and `pass0-pilot-50k.json`
(slim). None of its
numbers are used, for five reasons found since: it ran Chromium only; a write's id was the same in
every job, so kiln answered later jobs' writes as repeats (14 bytes longer); its baselines' terminal
frames crossed SSE and the streaming fetch without their line ends (1 to 3 bytes a frame short); its
baselines ran without kiln's `TCP_NOTSENT_LOWAT` (the host's default, 4 GiB, against kiln's
128 KiB), which would make them slower on the slow links and flatter kiln; and its bytes and CPU
had a sample a second, too few for the tails. Its kiln-today request job of pilot-50k round 0 had
no samples: none of its eight drivers wrote anything (not even its empty samples file), a failure
of the lab's, which a job now records as a named failure ("the driver left no result"), never as a
cell with nothing in it; requests a page scheduled and had no answer to by the job's end are now
counted as misses with their wait (`missed`, open loop), so a starved link shows as misses.

**Pass 1** runs the pilot and the matrix on two lab machines at once, kiln-a094a12d (Fedora, WebKit
in Playwright's image) and adacachy (CachyOS, WebKit 27.2 on the machine itself; production's
host, on Ada's word), each machine three parts at a time, each part a lab of its own (its own
namespaces, servers and browsers: the labs share nothing but the CPU), from a snapshot of the lab
(`~/.cache/kiln-bp/g1/lab-snap1`: `kiln-lab`, the lab kiln, the web driver, `run-pass.sh` and the
machine's parts) that nothing edits while it runs:

```sh
systemd-run --user --unit=kiln-bp-g1-lab -E PATH=$PATH -E HOME=$HOME -p Delegate=yes \
  -p Nice=19 -p CPUWeight=10 -p IOWeight=10 \
  -p StandardOutput=append:$HOME/.cache/kiln-bp/g1/lab-run.log \
  -p StandardError=append:$HOME/.cache/kiln-bp/g1/lab-run.log \
  --collect bash $HOME/.cache/kiln-bp/g1/lab-snap1/run-pass.sh $HOME/.cache/kiln-bp/g1/lab-snap1/parts 3
```

The parts (`crates/kiln-lab/web/servers/pass1/{fedora,adacachy}.parts`, made by `genparts.py`)
split the run plan's cells between the machines by their hours (38.5 and 38.3 job-hours: about
13 hours of wall time with three parts at once), each condition's rounds as the plan gives them at
a 70 h budget: the step-6 rule's budget clause puts the slow links' requests (11 rounds at 50k, 6 at
100k) and 50k/5's bursts at the 3,688 floor, and every other cell at the 5-round minimum, which, with
each job's fixed length (60 s on the good link, 180 s on the slow ones), is what the hours come
from. Each part writes `~/.cache/kiln-bp/g1/lab/<part>/run.json` and `done` when it ends well
(`failed` otherwise); started again the same way, it skips what is done. `kiln-lab browser merge`
puts the parts' run files together (each job keeps its machine's name), for the report and the
comparisons.

- **The pilot** comes first on each machine: adacachy runs 50k/1 (stream, burst and request, each a
  part) and fedora good/0, each for kiln today twice (the A/A, interleaved on the same seeds), h2o
  over HTTP/2, and h2o over HTTP/3 with Cubic and with Reno, in both browsers where each runs. Its
  cells stand for the matrix's good/0 and 50k/1 cells (except 50k/1's requests, which the plan gives
  11 rounds). Each machine also runs kiln today's 50k/1 stream and good/0 request cells with one link
  (the K-links-against-one check, on each machine, against its own K = 8 cell), and the two machines
  share two cells for kiln today and h2o over HTTP/2 (50k/1 stream on fedora, good/0 request on
  adacachy, each against the other's pilot): a cross-machine A/A. The h2o builds differ by machine
  (the same commit, `1b5ef77eb`, against each machine's OpenSSL: fedora's 3.5 `8b2d8a23…`,
  adacachy's 3.6.5 `8b1ecfc5…`), so h2o's cross-machine cells show whether the baselines agree.
- **h2o's QUIC controller**: h2o-h3's matrix rows run on adacachy after its pilot (`$H3` in a part:
  the lower median p99 at 50k/1 of Cubic and Reno).
- **Production's guard** (`pass1/guard-prod.sh`): on adacachy, production's `/readyz` every minute;
  slower than 1 s or failing, the lab's unit is frozen and that is logged, and thawed after five
  good minutes. The jobs a freeze overlapped are left out of the report; the freezes' count and
  minutes are in the run's notes. If adacachy's share falls far behind, its parts move to fedora.

**Where pass 1 ran, in the end: fedora alone.** adacachy's share never ran there: production's
host went deep into swap on 2026-11-05 and each thaw of the guarded unit slowed production's
`/readyz` again, so its parts moved to fedora at 20:13Z (`pass1/fedora-moved.parts`, taken from the
list's end by `run-pass-tail.sh` in the unit `kiln-bp-g1-lab-moved`, beside `kiln-bp-g1-lab` and
`kiln-bp-g1-lab-tail` on `fedora.parts`, two parts at a time each). Every pilot cell (good/0 and
50k/1), the K-links check and the cross-machine cell's fedora half (`xm-50k-stream`) ran on fedora;
the cross-machine A/A has no adacachy half. fedora's load (one-minute) during the pilot's jobs:
median 33, at most 114, CPU pressure (some, avg10) median 9%; every job records its own.

### Step 7: the pilot's checks

`pass1/step7.sh` (on the lab machine, after the pilot's parts and `pass1/fedora-kcheck.parts`). Its
run files are in the repository (each `kiln-lab browser merge --slim` of the part's own): the
pilot, `docs/browser-lab/2026-11-05/pilot.json` (240 jobs, the six pilot parts), and the K check's
cells, `docs/browser-lab/2026-11-05/{k1,k1b,k2,k4,xm}-50k-stream.json` and `k1-good-request.json`;
each number below comes again from them as written:

```sh
D=docs/browser-lab/2026-11-05
kiln-lab browser compare $D/pilot.json kiln-today kiln-today-b                          # the A/A: 0 of 108
kiln-lab browser compare $D/pilot.json kiln-today kiln-today --b-run $D/k1-good-request.json   # K = 8 against 1: 0 of 18
kiln-lab browser compare $D/pilot.json kiln-today kiln-today --b-run $D/k1-50k-stream.json     # 0 of 18
kiln-lab browser compare $D/xm-50k-stream.json kiln-today kiln-today --b-run $D/k1-50k-stream.json   # 2 of 18
kiln-lab browser compare $D/k4-50k-stream.json kiln-today kiln-today --b-run $D/k1-50k-stream.json   # 3 of 18
kiln-lab browser compare $D/k2-50k-stream.json kiln-today kiln-today --b-run $D/k1-50k-stream.json   # 0 of 18
kiln-lab browser compare $D/k2-50k-stream.json kiln-today kiln-today --b-run $D/k1b-50k-stream.json  # 0 of 18
kiln-lab browser compare $D/k1-50k-stream.json kiln-today kiln-today --b-run $D/k1b-50k-stream.json  # 0 of 18
```

- **The A/A**: kiln today against itself (`kiln-today-b`), interleaved on the same seeds, in both
  browsers, good/0 and 50k/1, each case: **0 of 108** numbers differ beyond noise (each held so all
  108 hold together at 95%, Bonferroni). The bar is 5%.
- **Every number has its interval and sample count**: the pilot's tables (108 rows,
  `kiln-lab browser report` of the merged pilot) give each target's median of its rounds, its 95%
  interval and n, and each time p99.9 its within-round band.
- **Report and compare read the run files back**: for each pilot part, the tables `report` prints
  from its `run.json` are the ones the part printed as it ran (the script compares part folders
  only: `<part>.take` and `<part>.skip` beside them are the parts' claim markers, which an earlier
  glob took for parts).
- **h2o's QUIC controller**: at 50k/1, the median p99 of every h2o-h3 job is 3,841 ms with Cubic
  (15 jobs) against 5,041 ms with Reno (15 jobs), so h2o's HTTP/3 rows run with **Cubic**
  (`h2o-h3`).
- **CPU under the background's noise.** kiln today's server at rest is noisy (both copies, `kiln-today` and `kiln-today-b`, cell by
  cell, from `pilot.json`): two background
  windows before a job differ by 5,318 to 246,124 µs of CPU a second (h2o's by 19 to 7,827; each
  cell's median), so over a reading (25 ms on the good link; 75 to 327 ms at 50 kbit/s, where a
  reading with no operation carries on to the next) its floor is 180 to 10,648 µs. kiln today's per-operation CPU
  p50 is at or under it in most pilot cells: at 50k/1 it even comes out negative (requests in
  Chromium -1,852 µs, bursts -136 µs; the background's rate taken from readings that held little
  else), and on the good link its streams' and requests' p50 in Chromium (1,213 and 2,343 µs under
  floors of 3,655 and 3,717). Some of h2o's 50k/1 p50s are under theirs too (bursts -4.2 µs,
  requests 297 µs under 457). Those cells give the CPU's mean and say so; the p99 and p99.9 clear
  the floor in every pilot cell. Where even the mean is negative (kiln-today-b's 50k/1 burst in
  Chromium, -746 µs), the operation's CPU is below what the lab resolves under that background.
- **K links against one** (kiln today, the same cell with K links and with one, each set of rounds
  against the other, Bonferroni over its 18 numbers): good/0 requests at K = 8 (the pilot) against
  K = 1, 0 of 18 differ, so **K = 8 stands** there and for every cell but one kind. 50k/1 streams:
  the pilot's K = 8 cell against K = 1, 0 of 18; but fedora's second K = 8 cell (`xm-50k-stream`)
  against K = 1, 2 of 18, both WebKit (bytes p99.9 46,910 against 29,173 B; CPU p99.9 56,998 against
  295,419 µs). Lowered per the step (`pass1/fedora-kcheck.parts`, beside the pass's six parts):
  K = 4 against K = 1, 3 of 18, all WebKit (time p99 30,254 against 35,870 ms, p99.9 37,153 against
  42,333 ms, bytes p50 1,786 against 2,921 B); **K = 2 against K = 1, 0 of 18**, and against K = 1
  measured again beside it (`k1b`), 0 of 18; K = 1 against K = 1 again, 0 of 18 (the cell itself is
  steady). Chromium's numbers differ at no K. So **the 50 kbit/s stream cells in WebKit run at
  K = 2** (each link runs its own WebKit, in a container of its own; what makes 4 or 8 of them
  interfere, and Chromium's not, the lab doesn't show), with
  13 rounds (810 operations a round, 10,530 in all), every other cell at K = 8. fedora's load during
  the K = 4, 2 and 1-again parts was lower than the pilot's (one-minute median 7.7, at most 12.5,
  against the pilot's 33 and K = 1's first cell's 25): the K = 2 cell agreed with K = 1 under both.
  The parts this changed (`pass1/fedora-k2.parts`, unit `kiln-bp-g1-lab-k2`, from 2026-11-06
  01:31Z): `m2-50k-{0,1,5}-stream-wk` (kiln today, h2o and Caddy over HTTP/2 in WebKit at K = 2);
  `m-50k-0-stream` and `m-50k-5-stream`, not yet started, were marked `.skip` and run as their
  Chromium half at K = 8 (`m-50k-{0,5}-stream-cr`). **Superseded**: the K = 8 WebKit stream jobs
  at 50k/1 of `pilot-50k-stream` (kiln today, kiln-today-b and h2o-h2), `xm-50k-stream` and
  `m-50k-1-stream` (Caddy): the merge leaves them out (`kiln-lab browser merge --drop
  stream,webkit,50k/1,8`). The K check covered the pilot's two cells only; 100 kbit/s streams in
  WebKit run at K = 8, unchecked against K = 1.

### Step 8: the matrix

The rest of pass 1 ran on fedora alone behind its memory gate (`run-pass-mem.sh`, each part a unit of
its own, `guard-mem.sh` beside them), and what went wrong on the way is part of the record:

- **Memory freezes.** Between 02:06Z and 04:46Z on 2026-11-06 the guard froze parts' units 31 times
  (fedora is shared), 13 parts for 49 to 83 minutes each in all (the guard's log:
  `docs/browser-lab/2026-11-06/freeze.log`). A job a freeze overlapped is left out (`merge --drop-jobs`,
  `pass1/frozen.py`'s list: 30 jobs) and its round runs again on its own seeds under its own number
  (`--first-round`, the `fa-` parts).
- **A full disk.** At 06:29Z fedora's `/home` filled (and the kernel killed a part for memory): 14
  parts failed and ran again (`<part>.failed-0629` kept).
- **No node.** The unit that ran them again from 09:17Z had a `PATH` without node: each part failed in
  20 s ("the control echo didn't come up"; its `control.log`: `exec: node: not found`) and ran again
  from 09:48Z (`<part>.failed-node` kept). Nothing was measured in those 30 minutes.
- **The baselines' events.** The merge of what had run by 09:55Z showed the baselines' bursts with
  about 0.6 of kiln's operations (at 50k/1 in Chromium, 786 a job against kiln today's 1,280, none
  missed: the baselines' jobs of that cell from before, `docs/browser-lab/2026-11-06/before-s2-50k-1-burst.json`). The origin replayed `events` recorded *frames*, and 35% of a session's recorded frames
  (39% of `/api/events`') are state and patches with no stamp, so each baseline burst carried 26
  events to kiln's 40, and its streams' sessions and entities 0.61 to 0.65 of kiln's rate: a lighter load,
  which flattered the baselines' times under bursts. The origin now replays events (each with the
  frames before it, "The baselines"), and every baseline stream and burst cell ran again on it
  (`pass1/s2.parts`, the `s2-` parts, from a snapshot that differs from pass 1's by the origin and the
  burst's replay cursor alone, `lab-snap2`); the merge leaves out the baselines' streams and bursts
  from before (requests and kiln today's rows were never affected). Measured again, a baseline burst
  job at 50k/1 has 1,169 to 1,268 operations (the last burst can be cut by the job's end), and h2o
  over HTTP/2's in Chromium, its first round against its first round before, went from a p50
  of 2.0 s and a p99 of 3.95 s to 2.83 s and 6.12 s. The origin's check (`check-origin.mjs`) still finds every frame equal on both carriers
  and the replay the catalogue's.
- **Named failures.** Some jobs failed by name (the lab kiln not up within 120 s, an origin not up
  within 20 s, drivers that left no result, a turn refused while the lab session's machine
  reconnected). Their rounds ran again on their seeds (`pass1/fill.py`, the `g?-` parts).

<!-- step 8 coverage: begin -->
**What the matrix holds** (`pass1/step8.sh`, 2026-11-06 22:56Z): 166 parts, 1,343 jobs on fedora (its
host field says `fedora` or `kiln-a094a12d.attlocal.net`, the same machine), every one of the 216
target cells (27 conditions and cases, kiln today and both HTTP/2 rows in both browsers, both HTTP/3
rows in Chromium) with every round step 6's plan gives it: no round is missing. Left out of the
merge (`drop-jobs.txt` beside the run file): 653 jobs, of them 580 baseline stream and burst jobs from
before the origin's fix, 44 whose page clock was past step 4's limit (each round ran again), and the
29 a memory freeze overlapped; with them the K = 8 WebKit streams at 50k/1 (step 7) and the pilot's
extra rows (`kiln-today-b`, `h2o-h3-reno`).

**Samples against step 6's plan.** Two shortfalls hold in every cell of a kind, from the plan's count
rather than the run:

- **Streams have two thirds of the operations the plan counted.** The plan's stream rate is a
  session's events and twice the lines a second (a terminal's and a job log's), but a job log's
  stream runs with `--logs` only, which the matrix doesn't use ("The statistics, the run plan and
  the decision rule"); a link carries a session, a terminal and `/api/events`: 30 operations a
  second on the good link to the plan's 45, and so on down. On the good and 100 kbit/s links every
  stream cell still has more than 10,000 samples; at 50 kbit/s, the cells below.
- **Bursts have 0.81 to 0.99 of theirs.** The plan counts a job's bursts as its length over the gap
  (4.5 at 50 kbit/s: 180 s over 40 s); a job holds 4 whole turns, and a baseline's last turn can be
  cut by the job's end.

Pass 1's 70 h budget gave every other cell 5 rounds (the slow links' requests 6 and 11), so these
cells, all at 50 kbit/s, are under the floor step 6 set (10,000, or 3,688 where its budget clause
put it), each by the amount shown. Bytes and server CPU have fewer samples than time in every cell:
a reading (25 ms, or longer where no operation completed in it) is a sample of each, shared by the
operations it holds ("The probe and the three measures").

| case | condition | browser | target | time's samples | floor | short by | bytes' samples | CPU's samples |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| burst | 50k/0 | chromium | caddy-h2 | 6,060 | 10,000 | 39% | 2,998 | 1,579 |
| burst | 50k/0 | chromium | caddy-h3 | 5,844 | 10,000 | 42% | 3,739 | 1,695 |
| burst | 50k/0 | chromium | h2o-h2 | 6,047 | 10,000 | 40% | 3,088 | 1,854 |
| burst | 50k/0 | chromium | h2o-h3 | 5,994 | 10,000 | 40% | 3,156 | 1,577 |
| burst | 50k/0 | chromium | kiln-today | 6,400 | 10,000 | 36% | 3,036 | 2,147 |
| burst | 50k/0 | webkit | caddy-h2 | 6,178 | 10,000 | 38% | 3,201 | 1,726 |
| burst | 50k/0 | webkit | h2o-h2 | 6,205 | 10,000 | 38% | 3,228 | 2,029 |
| burst | 50k/0 | webkit | kiln-today | 6,400 | 10,000 | 36% | 2,310 | 1,880 |
| burst | 50k/1 | chromium | caddy-h2 | 6,231 | 10,000 | 38% | 2,698 | 1,457 |
| burst | 50k/1 | chromium | caddy-h3 | 6,014 | 10,000 | 40% | 3,404 | 1,680 |
| burst | 50k/1 | chromium | h2o-h2 | 6,187 | 10,000 | 38% | 2,894 | 1,865 |
| burst | 50k/1 | chromium | h2o-h3 | 6,138 | 10,000 | 39% | 2,991 | 1,632 |
| burst | 50k/1 | chromium | kiln-today | 6,400 | 10,000 | 36% | 3,319 | 2,825 |
| burst | 50k/1 | webkit | caddy-h2 | 6,277 | 10,000 | 37% | 2,891 | 1,700 |
| burst | 50k/1 | webkit | h2o-h2 | 6,244 | 10,000 | 38% | 2,957 | 1,892 |
| burst | 50k/1 | webkit | kiln-today | 6,400 | 10,000 | 36% | 2,858 | 2,596 |
| request | 50k/0 | webkit | kiln-today | 1,165 | 3,688 | 68% | 1,617 | 1,487 |
| request | 50k/1 | webkit | caddy-h2 | 3,391 | 3,688 | 8% | 2,607 | 2,455 |
| request | 50k/1 | webkit | kiln-today | 2,619 | 3,688 | 29% | 2,710 | 2,578 |
| request | 50k/5 | webkit | kiln-today | 2,956 | 3,688 | 20% | 2,706 | 2,639 |
| stream | 50k/0 | chromium | kiln-today | 6,648 | 10,000 | 34% | 6,345 | 5,176 |
| stream | 50k/0 | webkit | caddy-h2 | 6,942 | 10,000 | 31% | 6,420 | 6,251 |
| stream | 50k/0 | webkit | h2o-h2 | 6,950 | 10,000 | 30% | 6,305 | 5,852 |
| stream | 50k/0 | webkit | kiln-today | 6,745 | 10,000 | 33% | 5,984 | 5,805 |
| stream | 50k/1 | chromium | kiln-today | 5,784 | 10,000 | 42% | 5,468 | 4,522 |
| stream | 50k/1 | webkit | caddy-h2 | 6,957 | 10,000 | 30% | 6,668 | 6,090 |
| stream | 50k/1 | webkit | h2o-h2 | 6,949 | 10,000 | 31% | 6,065 | 5,725 |
| stream | 50k/1 | webkit | kiln-today | 6,758 | 10,000 | 32% | 5,906 | 5,933 |
| stream | 50k/5 | chromium | kiln-today | 8,522 | 10,000 | 15% | 7,446 | 6,643 |
| stream | 50k/5 | webkit | caddy-h2 | 6,960 | 10,000 | 30% | 6,192 | 5,728 |
| stream | 50k/5 | webkit | h2o-h2 | 6,975 | 10,000 | 30% | 5,548 | 5,285 |
| stream | 50k/5 | webkit | kiln-today | 6,945 | 10,000 | 31% | 5,725 | 5,664 |

**Operations that didn't complete.** A request that failed outright (its fetch errored) is counted
apart, not as a time: kiln today's requests in WebKit at 50 kbit/s fail by the thousand, so its time cells there are
of the requests that completed; a request still waiting at the job's end is a miss, counted with its wait (open loop).
Streams and bursts have no misses: an event still queued when the job ends has no time, so where a
target holds far fewer events than the baselines did on the same schedule, its time percentiles are
of what arrived, **a lower bound** on its real ones. That is kiln today's streams in these cells
(kiln today sends its JSON uncompressed: f8):

| case | condition | browser | target | failed | of operations |
| --- | --- | --- | --- | --- | --- |
| request | 50k/0 | webkit | kiln-today | 2,795 | 3,960 (71%) |
| request | 50k/1 | webkit | caddy-h2 | 52 | 3,397 (2%) |
| request | 50k/1 | webkit | kiln-today | 3,105 | 5,523 (56%) |
| request | 50k/5 | webkit | caddy-h2 | 65 | 3,899 (2%) |
| request | 50k/5 | webkit | kiln-today | 928 | 3,839 (24%) |

Events left behind at the job's end (a round's events against the baselines' median in the cell; under 90%):

| case | condition | browser | target | events a round | baselines' | delivered |
| --- | --- | --- | --- | --- | --- | --- |
| stream | 100k/0 | chromium | kiln-today | 3,372 | 4,298 | 78% |
| stream | 100k/0 | webkit | kiln-today | 3,443 | 4,309 | 80% |
| stream | 50k/0 | chromium | kiln-today | 1,330 | 2,137 | 62% |
| stream | 50k/1 | chromium | kiln-today | 1,157 | 2,149 | 54% |
| stream | 50k/5 | chromium | kiln-today | 1,704 | 2,151 | 79% |
| stream | good/5 | webkit | kiln-today | 12,548 | 14,388 | 87% |

**The machine while the matrix ran** (each job's own record: the one-minute load and CPU pressure as
it started, and its page clock's residual; `pass1/machine.py`). fedora is shared: its load reached
965 and 1,429 at moments when other work thrashed it, and on 2026-11-06 it hung from 14:50Z to
20:28Z (the parts it was running ran again from the start). Every job kept is within step 4's clock
limits (200 µs in Chromium, WebKit's 1 ms grain).

| browser | jobs | hosts | load (1 min): median, p90, max | CPU pressure (avg10, %): median, p90, max | clock residual p99 (µs): median, max |
| --- | --- | --- | --- | --- | --- |
| chromium | 794 | fedora, kiln-a094a12d.attlocal.net | 14.2, 50.4, 965.3 | 0.2, 17.8, 64.1 | 16, 185 |
| webkit | 549 | fedora, kiln-a094a12d.attlocal.net | 15.3, 69.8, 1429.2 | 0.2, 18.7, 67.8 | 694, 948 |


**Step 8's test: five cells measured again.** After the matrix ended, five of its 216 cells were
drawn (`pass1/rerun5.py parts`, `random.Random(20261106)`) and measured again in a session of their
own (the unit `kiln-bp-g1-lab-r5`, 22:56Z to 23:38Z, each cell's rounds on its own seeds and K;
its run files `docs/browser-lab/2026-11-06/rerun5/`, slim, and the draw `rerun5/parts`), then
each against the matrix's (`kiln-lab browser compare MATRIX T T --b-run RERUN`, Bonferroni over the
cell's nine numbers):

| cell | rerun's rounds | numbers that differ beyond noise |
| --- | --- | --- |
| stream, 100k/5, WebKit, h2o over HTTP/2 | 5 | 1 of 9: CPU p50, -2.36 against -0.37 µs |
| burst, 100k/1, Chromium, kiln today | 5 | 0 of 9 |
| burst, 100k/5, WebKit, h2o over HTTP/2 | 5 | 0 of 9 |
| burst, 50k/1, Chromium, h2o over HTTP/2 | 5 | 0 of 9 |
| burst, good/1, Chromium, kiln today | 5 | 0 of 9 |

44 of 45 numbers are within noise of the matrix's. The one that isn't is a CPU p50 under the
background's noise floor in both runs (both negative: the operation's CPU is below what the lab
resolves there), which the tables don't give: that cell shows the CPU's mean instead, and the means
agree (the median of the rounds' means 79.0 µs in the matrix, 87.7 µs again; the rounds' 65 to 95
and 78 to 94 µs).
<!-- step 8 coverage: end -->

## What the lab leaves out

- The front door's hop: the lab measures kiln's own TLS listener; production phones reach kiln
  through the front door, which passes bytes through (f7).
- A radio's promotion delay after idle, unless Ada adds it.
- HTTP/3 in WebKit: WebKit on Linux speaks no HTTP/3 (libsoup: f30). Safari on a Mac would, with its
  traffic shaped on the lab host.
- BBR: TCP is CUBIC for every target (loading `tcp_bbr` needs root on the lab host: Ada's).
- Page loads and static files: goal 8's.
