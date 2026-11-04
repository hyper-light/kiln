# The runtime lab

How kiln's server is measured: from inside, on `/metrics` (each shard's health, each route class's
latency), and from outside, by `kiln-lab` (`crates/kiln-lab`), which replays realistic kiln use
against any build, with CPU hogs beside it when asked, and compares two builds round by round. Every
change to how the server schedules its work is decided by these numbers (Ada: A/B test, and let
benchmarking under realistic usage decide). Goal 1 of the API runtime program built them: plan
`01a1a59c2a68718493e26b9415f54824`, research `01a1a59c2a6a73dea4da7a2ba2c36848`.

## What `/metrics` shows

### Each shard's health (`kiln_shard_*`, `crate::shard_health`)

A probe pinned to each of hyper-rt's shards (`crate::rt::spawn_on`) sleeps a period on the shard's
own timers and notes, as it wakes, how late it ran: work that was ready on that shard waited that
long. Node measures its loop the same way (`monitorEventLoopDelay`, sampling every 10 ms by default:
research f8), and the period is the same, `KILN_SHARD_PROBE_MS` (`0`: no probes). Each wake the probe
also copies, on the shard itself, what hyper-rt counts there, and its thread's CPU time, into atomics
of its own; `/metrics` reads only those, so reading never touches a shard.

| series | what it says |
| --- | --- |
| `kiln_shard_delay_seconds{shard}` (histogram) | How late the shard ran its ready work: a probe's timer against when the probe ran. Up to a tick of the timer wheel (100 µs) is by design. |
| `kiln_shard_stalled_seconds{shard}` | How long since the shard's probe last ran. A shard held in one poll shows here at once; its delay only once it's let go. |
| `kiln_shard_busy_seconds_total{shard}` | CPU time the shard's thread has run: its rate is the share of the time the shard was busy on the CPU. Linux and macOS (Windows keeps a thread's times at its scheduler's tick). Time the host held a ready shard off the CPU isn't busy: it shows as delay, and as `host` slow polls. |
| `kiln_shard_polls_total{shard}` | Task polls. |
| `kiln_shard_slow_polls_total{shard,cause}` | Polls that held the shard past twice its step quantum (250 µs here, so past 500 µs), by who held it, as hyper-rt attributes them: `task` (it ran on the CPU), `blocking` (it waited inside a call), `host` (runnable, off the CPU: preempted), `unknown` (the OS couldn't tell: off the CPU on macOS, every one on Windows, and a shard's first long poll after a calm spell, since hyper-rt reads the thread's account only once a long poll has armed it). |
| `kiln_shard_longest_poll_seconds{shard}` | The longest single poll the shard has run. |
| `kiln_shard_scheduler_overrun_seconds{shard}` | How late the shard's steps ran after its waits, as the shard measured it: the host descheduling it, decaying as it recovers. |

### Each route class's latency (`kiln_route_*`, `crate::route_latency`)

`kiln_route_wait_seconds{class}` is how long the server's work waited before its handler started,
`kiln_route_seconds{class}` how long the handler then took. Both are histograms of atomics, from
50 µs to 60 s (`metrics::STALL_BUCKETS`), so a stall of seconds lands in a bucket of its own.

| class | what it is | waited from | took until |
| --- | --- | --- | --- |
| `page` | a GET or HEAD outside `/api`: the UI's pages and files | the read that brought the request's head (`kiln_http::Received`) | its response's head |
| `api_read` | the API's GETs and HEADs, and kiln's own reads at the root (`/metrics`, `/healthz`, git) | as `page` | as `page` |
| `api_write` | every other request: messages to agents, agents' MCP calls, every change | as `page` | as `page` |
| `websocket` | an agent session's live frames going out to a viewer's socket | the frame's publish on the bus | its write on the socket, from the viewer's task reading it |
| `runner` | a runner's message off its link; runners' and their jobs' own HTTP routes (enrolling, sync's contents, artifacts, CI's runtime services) | the network thread handing it to the hub (a request: as `page`) | the hub having applied it (a request: as `page`) |

A WebSocket's opening isn't timed as a request: its frames are. An HTTP request's handler starts in
the same poll that read it, so its wait is mostly parsing; time a request's bytes sat in its socket
while its shard was busy is the shard's delay, since the runtime doesn't say when a task was woken.

### The pools' work (`kiln_compute_*`, `kiln_blocking_*`, `crate::pool_health`)

kiln-rt tells kiln of each job its two pools run (goal 3 of the API runtime program, plan
`01a1a59c2a8e`; `hyper_rt::compute::observe`, `hyper_rt::blocking::observe`): the compute pool, where
CPU-heavy work goes off the shards (snapshots, serializing, compressing, hashing), each job's wait for a
thread and each piece's time; the blocking pool, each job's share, its wait for one of the share's
places and its run. The histograms are atomics the pools' threads write as they go.

| series | what it says |
| --- | --- |
| `kiln_compute_jobs_total`, `kiln_compute_refused_total` | Jobs the compute pool took, and those it refused at its bound (each asked again). |
| `kiln_compute_pieces_total`, `kiln_compute_yielded_total` | Pieces its jobs ran, and those after which their job gave its thread to a waiting one. |
| `kiln_compute_wait_seconds` (histogram) | How long each job waited for a thread: from its submission to its first piece. |
| `kiln_compute_piece_seconds` (histogram) | How long each piece held its thread. A piece ends once its job has run a shard's step (250 µs) and another job waits, at the next unit of its work. |
| `kiln_compute_longest_piece_seconds` | The longest piece yet. |
| `kiln_blocking_wait_seconds{share}`, `kiln_blocking_run_seconds{share}` (histograms) | How long each job of the blocking pool waited for one of its share's places, and ran: `dns` (`getaddrinfo`), `store` (kiln.db's statements), `fs` (files, `/proc`, archives, a child's wait). |
| `kiln_blocking_places{share}` | Each share's places: the most of its jobs that run at once (`crate::rt::Profile`). |

### What overload sheds (`crate::overload`)

Goal 6 of the API runtime program (plan `01a1a59c2abe`): each request is Critical (health, sign-in, a
person's pages and actions), Normal (the API) or Background (agents' and CI's polling, catch-ups, sync,
telemetry). Goal 1's probe on each shard ticks detectors that learn the shard's calm (headroom's CUSUM,
`kiln_ci::headroom::detect`): the shard's delay for Normal, its Background tasks' waits for Background.
The first shard's probe also ticks one for the compute pool and one for each of the blocking pool's
shares, on the mean wait of the jobs whose waits the pool told over the period, since what a request
waits on off its shard waits there, where no shard's delay sees it. A pool whose waits leave their calm
sheds Background requests on every shard, and once its waits reach the tolerance (1 s), Normal ones
too, while any of its jobs waits; Critical never. A shed request is answered 503 at once, before its
body is read, with `Retry-After` (the delay that shed it, in whole seconds) and `x-kiln-shed` (its
class). Shedding is off until `[server] shed = true` or `KILN_SHED=1`; the detectors run either way.

| series | what it says |
| --- | --- |
| `kiln_shard_queue_delay_seconds{shard,class}` | Each class's mean wait from ready to run on the shard, over the probe's last period. |
| `kiln_shard_shedding{shard,class}` | 1 while the shard's own queueing delay sheds the class's requests. |
| `kiln_pool_queue_delay_seconds{pool}` | Each pool's mean wait for a place over the probe's last period, as its detector read it: `compute` (a thread), `dns`, `store`, `fs` (the blocking pool's shares). |
| `kiln_pool_shedding{pool,class}` | 1 while work waiting in the pool past its calm sheds the class's requests on every shard. |
| `kiln_shed_total{class}` | Requests answered 503 at once, by class (`background`, `normal`; Critical is never shed). |
| `kiln_shard_stalled`, `kiln_shard_stalls_total`, `kiln_shard_stall_moved_total` and the others | kiln-rt's watchdog: a shard held in one poll past its limit, and the spawns it moved to other shards. |

## The lab (`kiln-lab`)

### A round

Each round is a fresh kiln of the build under test, so rounds are independent draws:

1. **The fleet.** A server with a data directory of its own, configured as production runs (the new
   CI engine off, nothing found on the network; terminals and KilnCD off, which a lab can't have
   on), and its runners, each a `kiln agent` of the same build whose `claude` is the lab itself as a
   stand-in (`crates/kiln-lab/src/standin.rs`): it speaks Claude Code's stream-json, so every
   update goes the way a real session's does, from the runner's harness to its store, over its link
   to the server's copy and the bus, and out to every viewer. The runners' headroom is off: a busy
   lab machine would otherwise pace the very sessions it measures.
2. **The fill.** Each session is opened on its runner, and its first turn writes its conversation as
   fast as the runner takes it, as an agent's that has run for days: a fifth of its events tool
   calls with their output (reads, commands, edits, searches), the rest thoughts and replies of a
   few hundred words (the mix of `crates/kiln/benches/session_feed.rs`).
3. **The window.** After a warm-up, the lab reads `/metrics`, measures for the window, and reads it
   again, while all of this runs at once:
   - **people viewing agents:** each viewer opens an agent's conversation as the UI does
     (`?z=gzip&patch=state&patch=changes`: the whole of it, packed), watches its live frames, then
     opens another; three in four watch an agent at work;
   - **agents at work:** the largest sessions are sent a message (an `api_write`) and answer with a
     turn of events written a few tens of milliseconds apart, each stamped with when it was
     written, so a viewer can say how long it took to reach it;
   - **the UI's and agents' API reads:** the agents list, an agent, its history, its goals and
     plans, the runners, the overview, runs;
   - **agents' MCP calls** (`agents_list`, `runners`);
   - **pages and their files,** asked for as a browser asks (`Accept: text/html`, brotli);
   - with `--hogs`, **CPU hogs:** busy-loop processes beside the server, a count or a multiple of the
     lab's CPUs (`1x`, `2x`).

The profile (`--profile realistic`, `lab::Profile::realistic`) is production's shape at a busy hour:
its `/metrics` on 2026-11-03 showed ten runners, 134 live feeds, 4 to 8 people watching, and
conversations to about 20,000 events. The lab runs 4 runners and 32 live sessions (2 of 20,000
events, 6 of 4,000, 24 of 400), 8 of them at work (a 40-event turn, 25 ms apart, then about 2 s
before the next message), 8 viewers (20 s on each agent), 4 API readers (50 ms apart), 2 agents
calling MCP (250 ms apart) and a page loader (100 ms apart), with a 5 s warm-up and a 60 s window.
`--profile small` is a minute's try. `--profile overload` is the realistic one with no think time
for its viewers and readers (each asks again as soon as it is answered): with one shard
(`--env KILN_RT_SHARDS=1`) it overloads the server on purpose, as goal 6's shedding is measured
(`--env-b KILN_SHED=1` against the same build without). A request the server sheds counts as an
error, and its worker waits out the answer's `Retry-After` before asking again, as kiln's own clients
do, rather than asking again at once.

### What it reports

Each round reports, for each class the lab times (`page`, `api_read`, `api_write`, `websocket`: an
event from its harness writing it to a viewer's socket bringing it, `attach`: a viewer opening a
whole conversation), its median, p99, p99.9 and most (the stall tail), its rate and its errors (a
status of 400 or more, or no answer); the server's CPU for each request and its share of a CPU; and,
where the build has them, from `/metrics`: the shards' delay (p99, p99.9 and the top bucket reached,
all shards together), their busy share and slow polls a second, all and by cause
(`shards.slow_polls_task_per_s` and the others), each route class's server-side p99 took and waited,
and the requests the server shed by class (`kiln_shed_total`: goal 6's shedding, `crate::overload`,
on with `KILN_SHED=1`). Where the build has the pools' series: the compute pool's jobs and
pieces a second, its jobs' wait for a thread at p99, its pieces' p99 and the top bucket one reached,
and its threads busy on average (`compute.*`); and for each of the blocking pool's shares, its jobs a
second, their run and wait at p99, the places they held on average (`busy`: their runs' time over the
window's) and the places Little's law asks of their rate and p99 run (`little_p99`, as hyper-rt sizes
a shard's tasks: `admission_limit`) (`blocking.<share>.*`).

The fill is reported too (`fill.*`): how long it took (`fill.s`) and the server's own numbers over it
(its CPU share, the shards', the pools'). It is the conversation feed at its busiest: every session's
conversation written at once and sent by each runner's shipper to the server's intake, a batch of up to
2 MiB at a time unzipped, hashed and parsed.

A run's number is the median of its rounds'. Its noise is the interval that median moves in when the
rounds are drawn again (a bootstrap of the rounds, 95%). `ab` runs two builds interleaved and in
turn (A B, then B A, ...), and for each number reports B's median against A's, the difference, and
its noise: the interval the difference moves in when both builds' rounds are drawn again, held to a
level at which every number compared holds together at 95% (Bonferroni). A difference is called
better or worse only when its interval leaves out none; `compare` does the same for two runs made
apart. Each round also records the machine's load averages and, on Linux, its CPU pressure and the
lab's CPUs' clock, before and after its window, and a comparison says each side's machine.

### Running it

Build the server to measure in release, as production's is, and the lab beside it (the lab drives
any build: it speaks to kiln only over HTTP, WebSockets and the runners' own link):

```sh
(cd ui && npm ci && KILN_UI_SERVER=off npm run build)   # the UI's client, so pages are the app's
KILN_UI_SERVER=off cargo build --release -p kiln -p kiln-lab --bin kiln --bin kiln-lab
```

Then, on Linux, hold the lab and everything it starts to a set of CPUs, so hogs crowd only the lab's
CPUs (`taskset`), and ask kiln for a machine quiet enough to measure:

```sh
# one build, 8 rounds, calm and with as many hogs as the lab's CPUs
taskset -c 16-31 target/release/kiln-lab run --kiln target/release/kiln --label main --rounds 8
taskset -c 16-31 target/release/kiln-lab run --kiln target/release/kiln --label main --rounds 8 --hogs 1x

# two builds, interleaved: is B better than A beyond noise?
taskset -c 16-31 target/release/kiln-lab ab --a main/kiln --b branch/kiln --label-a main --label-b branch --rounds 8

# one build, two settings of its server: its shards, or its blocking pool's places (dns,store,fs)
kiln-lab ab --a kiln --b kiln --label-a 2-shards --label-b 4-shards --env-a KILN_RT_SHARDS=2 --env-b KILN_RT_SHARDS=4
kiln-lab ab --a kiln --b kiln --label-a fs-8 --label-b fs-26 --env-a KILN_RT_BLOCKING=4,2,8 --env-b KILN_RT_BLOCKING=4,2,26 --hogs 1x

# a second run against a first; a run's report again; runs side by side
kiln-lab compare first/run.json second/run.json
kiln-lab report run/run.json
kiln-lab table calm=calm/run.json "16 hogs=hogs/run.json"
```

A new build against main's baseline is an `ab` of the two on one machine, not a `compare` with the
files below: they hold the baseline's numbers and machine, which another machine (or this one under
other work) doesn't share.

On one of kiln's machines, ask for it by what the lab needs (MCP `run` with `needs:
"linux/amd64,cpus=16,mem=16g,exclusive"`, or `"macos"`), and start the lab in a unit of its own
(`systemd-run --user --unit=<name> ...`) so it outlives the run that started it. Each run writes
`run.json` (every round, as it finishes) and `report.md` to `--out` (`~/.cache/kiln-lab/<when>`
otherwise). Each round's kiln (a few GB of stores) is removed after it a file at a time, each
waiting while the machine's IO is held back (`full avg10` 25% or more), unless `--keep`. A round
takes about the fill plus 70 s. Stop a run by the unit or run it's in: the lab's server and runners
lead process groups of their own (so each one's harnesses go with it), which a kill of the lab's
process alone leaves running.

## Main's baseline

Measured on 2026-11-04 (10:00 to 11:56 UTC) on a Linux machine with an Intel Core i9-14900HX (32
threads: 8 performance cores of two threads each, then 16 efficient cores), the lab and everything it
started held to the 16 efficient cores (`taskset -c 16-31`), where the server's own sizing for 16
CPUs gave it 4 shards. The build: main at `2a2d93f6df5` (before kiln's own runtime, `crates/kiln-rt`,
and its compute pool landed) with this goal's metrics, in release, the UI's client built in
(`KILN_UI_SERVER=off`; this branch's `d3c2cb7b599`). The `realistic` profile, eight rounds each:
calm, with 16 CPU hogs (as many as the lab's CPUs) and with 32. Each cell is the median of the
rounds and, after it, its noise (the 95% interval of that median). In each 60 s window the lab timed
about 5,200 live deliveries, 4,000 API reads, 630 API writes, 600 page loads and 20 attaches, so a
p99.9 is the window's most for pages, API writes and attaches, and so is an attach's p99. The rounds
themselves, as `kiln-lab` writes them (`kiln-lab report` and `compare` read each):
`docs/runtime-lab/2026-11-04/calm.json`, `hogs-1x.json`, `hogs-2x.json`.

| number | calm | 16 hogs (1x) | 32 hogs (2x) |
| --- | --- | --- | --- |
| page p50 ms | 0.197 (0.191 to 0.204) | 0.183 (0.169 to 0.887) | 0.145 (0.126 to 0.177) |
| page p99 ms | 0.423 (0.408 to 0.450) | 4.12 (2.73 to 132) | 3.13 (2.73 to 4.02) |
| page most ms | 1.23 (0.628 to 1.65) | 7.19 (5.54 to 243) | 5.16 (4.34 to 9.93) |
| api_read p50 ms | 0.672 (0.592 to 0.682) | 1.60 (1.28 to 2.02) | 1.69 (1.28 to 2.43) |
| api_read p99 ms | 81.0 (55.2 to 101) | 128 (95.0 to 151) | 133 (113 to 146) |
| api_read p99.9 ms | 665 (329 to 847) | 727 (604 to 864) | 675 (494 to 1060) |
| api_read most ms | 1188 (476 to 1810) | 1080 (846 to 1210) | 955 (735 to 1258) |
| api_write p50 ms | 1.50 (1.46 to 1.55) | 1.29 (1.11 to 1.80) | 1.05 (0.967 to 1.10) |
| api_write p99 ms | 339 (136 to 733) | 645 (515 to 726) | 586 (511 to 653) |
| api_write most ms | 991 (513 to 1603) | 1208 (799 to 1268) | 1003 (755 to 1464) |
| websocket p50 ms | 0.986 (0.960 to 1.01) | 4.15 (3.29 to 11.4) | 4.00 (3.48 to 9.22) |
| websocket p99 ms | 702 (619 to 748) | 1734 (1481 to 2403) | 2236 (1859 to 2522) |
| websocket p99.9 ms | 896 (885 to 929) | 2250 (1962 to 2930) | 2948 (2601 to 3355) |
| websocket most ms | 2486 (2222 to 3127) | 3718 (3331 to 4137) | 4374 (3384 to 4682) |
| attach p50 ms | 866 (856 to 873) | 1897 (1594 to 2076) | 2283 (2108 to 2573) |
| attach most ms | 1064 (960 to 1603) | 2622 (2398 to 3302) | 3345 (3299 to 3696) |
| shards' delay p99 ms | 2.03 (1.98 to 2.07) | 10.0 (9.21 to 16.7) | 9.23 (7.89 to 16.7) |
| shards' delay p99.9 ms | 19.2 (18.6 to 20.1) | 89.6 (85.2 to 95.0) | 92.9 (89.3 to 95.8) |
| shards' delay, top bucket reached ms | 250 | 375 (250 to 500) | 500 |
| shards' busy share | 0.107 (0.105 to 0.108) | 0.085 (0.080 to 0.088) | 0.073 (0.070 to 0.077) |
| shards' slow polls a second | 214 (206 to 217) | 182 (151 to 282) | 179 (173 to 230) |
| server: api_read took p99 ms | 87.3 (47.9 to 93.4) | 100 (91.7 to 127) | 91.7 (89.4 to 99.8) |
| server: api_write took p99 ms | 363 (200 to 622) | 624 (591 to 731) | 678 (481 to 759) |
| server: websocket waited p99 ms | 735 (614 to 775) | 2113 (2039 to 2348) | 2275 (2072 to 2653) |
| server: websocket took p99 ms | 0.098 (0.098 to 0.099) | 1.85 (0.625 to 2.09) | 1.48 (0.958 to 2.19) |
| server: runner waited p99 ms | 195 (163 to 206) | 306 (255 to 393) | 403 (347 to 437) |
| server: runner took p99 ms | 0.400 (0.397 to 0.410) | 1.16 (0.947 to 2.13) | 1.38 (1.12 to 2.29) |
| server CPU ms a request | 14.4 (13.4 to 14.6) | 11.9 (11.1 to 12.2) | 10.1 (9.72 to 10.6) |
| server CPU share (cores) | 1.28 (1.26 to 1.29) | 1.01 (0.970 to 1.07) | 0.876 (0.839 to 0.927) |
| throughput: API reads a second | 67.9 (67.2 to 73.6) | 65.4 (64.2 to 68.3) | 65.5 (64.9 to 67.3) |
| throughput: live deliveries a second | 86.5 (85.2 to 87.7) | 87.7 (87.0 to 89.8) | 87.1 (85.9 to 87.4) |

What it says:

- **The live feed's tail is the viewer's wait.** An agent's event reached a viewer in about 1 ms at
  the median, but in 0.7 s at p99 calm and 1.7 to 2.2 s with hogs (2.5 s at most calm, 4.4 s with
  32 hogs). The server places it: a frame waited 0.74 s at p99 between its publish and its viewer's
  task reading it (2.1 to 2.3 s with hogs) and took 0.1 ms to write, while the shards ran their
  ready work 2 ms late at p99. Production's own live feed showed the same tail (deliveries of 2.5 to
  5 s: research f7).
- **Runners' messages queue for the hub's one loop:** 195 ms at p99 calm, 306 and 403 ms with hogs,
  then 0.4 to 1.4 ms to apply.
- **API reads and writes are slow at the tail inside their handlers,** not before them: reads took
  81 ms at p99 to the lab and 87 ms at the server, against a median of 0.7 ms; writes (messages to
  agents, MCP calls) took 0.34 s at p99 calm and 0.6 s with hogs.
- **Opening a long conversation** (up to 20,000 events, packed) took 0.87 s at the median calm, 1.9
  and 2.3 s with hogs.
- **Hogs cost the shards' delay five times over** at p99 (2 ms to 9 or 10 ms) and at p99.9 (19 to 90
  ms); the longest any probe waited went from the 0.25 s bucket to the 0.5 s one. Their busy share
  fell (11% to 7%): they're on the CPU less, held off it more.
- **CPU per request seems to follow the clock,** not only the work: 14.4 ms calm, 11.9 and 10.1 ms
  with hogs, whose load raised the efficient cores' shared clocks (2.1 to 3.0 GHz calm, 3.0 to 3.5 GHz
  with hogs, as each round's machine line read them before its window).

### Its noise

- **Two runs of one build agree.** Two calm runs back to back (`calm.json`, then `calm-again.json`)
  differ beyond noise in none of the 43 numbers, and neither does either from a third run an hour
  before them (`calm-earlier.json`), the machine at the same load (8.8 to 9.3) each time.
- **A machine that changed shows.** A calm run made while other agents' CI builds loaded the machine
  (load 18.0 against 8.8; `calm-busy-machine.json`) differs from each of the others in 13 or 14 of
  the 43 numbers: its medians faster, its live tail slower. `compare` says each side's machine
  beside the verdicts; a comparison of builds across such a change is the interleaved A/B's to make.
- **A/B of main against itself** (`kiln-lab ab --a kiln --b kiln`, 16 rounds interleaved;
  `main-against-main.json`): no difference beyond noise in any of the 43 numbers.

### On macOS

The lab ran on a Mac Studio (14 cores) against that machine's installed kiln, a build without this
goal's series (so no `server.*` or `shards.*` numbers): the `small` profile in 2 rounds of 2, the
`realistic` one in 1 of 2 (the other couldn't open a session in 120 s). Its runner's headroom held
the run, and so the lab, to 0.1 to 0.8 cores while production's server answered it slowly, so those
numbers are the pacing's, not kiln's.

## Goal 3: heavy work off the shards

Goal 3 of the API runtime program (plan `01a1a59c2a8e`, research `01a1a59c2a8e7225ace75b13eaae5c6e`)
moved CPU-heavy work off the shards to kiln-rt's compute pool, below them in the OS's priority and in
pieces: Ship's rows; the conversation feed's batches on runners (serialized, hashed, compressed) and its
intake on the server (each whole batch unzipped, hashed and parsed, which a thread of its own did at the
shards' priority: now a buffer or a row at a time on the pool, the intake's thread waiting for each so
batches are taken in order); the UI's compression, password hashing and an S3 part's digest. The
terminal relay gives up its shard by the step quantum. The blocking pool's shares are sized from this
lab's measurement (below), and both pools show their work on `/metrics` (above).

### The A/B against main before goal 3

Measured on 2026-11-04 from 15:02 to 15:46 UTC on the baseline's machine, but on its performance cores
(`taskset -c 0-15`: 8 cores of two threads each), as two other labs held its efficient cores: the
machine's load averages were 25 to 33 before each window. So these numbers are this A/B's own, not
comparable with the baseline's table above. A: goal 1's baseline build (main at `2a2d93f6df5` with goal
1's metrics; goal 2's kiln-rt is hyper-rt copied, two calls renamed, so this is main before goal 3). B:
main at `01aba3ff882` with this change (`g3b-9d599a5892`), its server's blocking pool at the sizes
derived below (`KILN_RT_BLOCKING=4,2,26`, its defaults since). The `realistic` profile, eight rounds of
each, interleaved, calm and with 16 CPU hogs; the server sized itself to 4 shards and a compute pool of
12 threads (16 CPUs less the shards). The rounds: `docs/runtime-lab/2026-11-04/goal3-ab-calm.json` and
`goal3-ab-hogs-1x.json`.

Calm, 4 of 58 numbers differ beyond noise:

| number | A | B | difference | verdict |
| --- | --- | --- | --- | --- |
| fill: shards' delay p99 ms | 21.8 | 3.66 | -83.2% | better |
| fill: shards' delay p99.9 ms | 41.4 | 10.6 | -74.3% | better |
| fill: shards' busy share | 0.191 | 0.110 | -42.3% | better |
| server RSS MB | 977 | 1132 | +15.9% | worse |
| (within noise) shards' delay p99 ms | 0.888 | 0.525 | -40.8% | same |
| (within noise) shards' task slow polls a second | 9.62 | 2.17 | -77.4% | same |
| (within noise) websocket p99 ms | 575 | 488 | -15.1% | same |
| (within noise) fill seconds | 14.8 | 13.9 | -6.0% | same |

With 16 hogs, 11 of 58 differ beyond noise:

| number | A | B | difference | verdict |
| --- | --- | --- | --- | --- |
| api_read p99 ms | 123 | 83.3 | -32.4% | better |
| api_read p99.9 ms | 795 | 533 | -32.9% | better |
| api_read a second | 65.6 | 69.5 | +5.9% | better |
| server: api_read took p99 ms | 98.5 | 86.3 | -12.4% | better |
| websocket p50 ms | 2.98 | 2.30 | -22.8% | better |
| shards' delay p99.9 ms | 83.0 | 22.3 | -73.1% | better |
| shards' busy share | 0.069 | 0.056 | -19.1% | better |
| fill: shards' delay p99 ms | 23.8 | 2.70 | -88.7% | better |
| fill: shards' delay p99.9 ms | 47.0 | 9.72 | -79.3% | better |
| fill: shards' busy share | 0.201 | 0.104 | -48.1% | better |
| server RSS MB | 989 | 1149 | +16.2% | worse |
| (within noise) shards' delay p99 ms | 5.65 | 2.08 | -63.3% | same |
| (within noise) shards' task slow polls a second | 15.0 | 5.18 | -65.5% | same |
| (within noise) api_write p99 ms | 605 | 379 | -37.4% | same |

What B's pools did (B alone has their series): in the calm window the compute pool took 142 jobs a
second, its pieces 2.3 ms at p99 and its jobs' wait for a thread 0.09 ms at p99, 0.06 of a thread busy on
average; over the fill, 40 jobs a second, pieces 48 ms at p99, waits 0.2 ms, 0.9 of a thread busy. The
fill's longest piece reached the 10 s bucket: the UI's files compressed at brotli's quality 11 as the
server starts, a file a unit, which goes on in one piece while no other job waits for a thread (none
did: 0.2 ms at p99).

### The blocking pool's shares, sized

B alone at the old sizes (4, 2, 8), four rounds calm and four with 16 hogs, from 14:31 to 14:43 UTC
(`blocking.<share>.*`, the median of the rounds; `goal3-measure-calm.json`,
`goal3-measure-hogs-1x.json`); "at once" is Little's law, each round's jobs a second times its p99 run:

| share | calm: jobs/s | run p99 | wait p99 | at once | hogs: jobs/s | run p99 | wait p99 | at once |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `fs` | 47.8 | 81.5 ms | 0.25 ms | 3.8 | 24.9 | 1,056 ms | 6.3 ms | 25.5 |
| `store` | 982 | 24.9 ms | 441 ms | 23.9 | 532 | 20.1 ms | 294 ms | 10.8 |
| `dns` | 0 | | | | 0 | | | |

- **`fs`: 26**, its jobs at once where it held the most (with hogs, `crate::rt::SERVER_FS`). An A/B of
  26 against 8 with 16 hogs (six rounds each, `goal3-fs-sizes-hogs-1x.json`) found no difference beyond
  noise in any of 94 numbers: its jobs waited 4.7 and 4.8 ms at p99 for a place, the pool's threads'
  own wait for a CPU beside the hogs, not a full share.
- **`store`: 2**: its one connection takes a statement at a time (`crate::store`), so places past the
  connection's and one at its door would only wait on its lock. What its statements wait for, 441 ms at
  p99 calm, is that connection: Little's law asks 24 places of it. A follow-up, outside this goal: the
  API's tail (reads 82 ms and writes 441 ms at p99 calm, above) waits there.
- **`dns`: 4**, kiln's count from its move onto hyper-rt: the lab's kiln resolves no name (every
  address it dials is a literal), so the lab can't size it. Production's `kiln_blocking_*{share="dns"}`
  can.

### What it says

- **No shard is held up by goal 3's heavy paths in the lab.** None of them runs on a server shard now:
  their work is the compute pool's jobs above. The shards' task slow polls in the window (B 2.2 a second
  calm and 5.2 with hogs, A 9.6 and 15.0) are within noise of each other. Over the fill, when the
  conversation feed is at its busiest, the shards ran their ready work 3.7 ms (calm) and 2.7 ms (hogs)
  late at p99 where main before goal 3 ran it 21.8 and 23.8 ms late, their busy share 42% and 48%
  lower; with hogs, in the window too, 22.3 ms late at p99.9 against 83.0, and API reads 33% faster at
  p99 and p99.9.
- **The pool's rank costs its jobs a wait beside the hogs, as step 1 chose:** with 16 hogs in its cgroup
  the compute pool's jobs waited 9.0 ms at p99 for a thread (calm, 0.09 ms), its pieces 55 ms at p99.
- **What the lab can't say** is which code held a slow poll: kiln-rt counts them by cause, not by task.
  The fill's task slow polls (B 92 a second, A 106: within noise) are the shards' own work on what the
  runners send, which goal 3 didn't move: the hub's loop and the registry's ingest of each event (the
  registry is goal 4's).
- **The intake's move costs the feed nothing the lab sees:** the fill took 13.9 s against 14.8 s calm,
  13.3 against 13.7 with hogs.
- **The server's RSS is 16% higher** (977 to 1132 MB calm, 989 to 1149 with hogs): goal 3's, not the
  share sizes' (26 against 8 places: 1146 against 1130 MB).

## What the lab doesn't do yet

- **CI.** A lab kiln has no repositories, so its "CI pages" are the run lists and overview, empty;
  no job writes logs and no one tails them.
- **One machine.** The runners, their stand-ins and the lab's own clients share the server's
  machine; production's runners are mostly elsewhere. The server-side numbers (`server.*`,
  `shards.*`) are the server's alone; the lab's own (`page`, `api_*`, `websocket`, `attach`) are what
  a person on that machine would see.
- **HTTP waits.** A request's wait before its handler can't include time its bytes sat in the socket
  while its shard was busy (see above): that's in the shard's delay.
