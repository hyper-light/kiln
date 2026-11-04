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
`--profile small` is a minute's try.

### What it reports

Each round reports, for each class the lab times (`page`, `api_read`, `api_write`, `websocket`: an
event from its harness writing it to a viewer's socket bringing it, `attach`: a viewer opening a
whole conversation), its median, p99, p99.9 and most (the stall tail) and its rate; the server's CPU
for each request and its share of a CPU; and, where the build has them, from `/metrics`: the shards'
delay (p99, p99.9 and the top bucket reached, all shards together), their busy share and slow polls
a second, and each route class's server-side p99 took and waited.

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

# one build, two settings of its server
kiln-lab ab --a kiln --b kiln --label-a 2-shards --label-b 4-shards --env-a KILN_RT_SHARDS=2 --env-b KILN_RT_SHARDS=4

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

## What the lab doesn't do yet

- **CI.** A lab kiln has no repositories, so its "CI pages" are the run lists and overview, empty;
  no job writes logs and no one tails them.
- **One machine.** The runners, their stand-ins and the lab's own clients share the server's
  machine; production's runners are mostly elsewhere. The server-side numbers (`server.*`,
  `shards.*`) are the server's alone; the lab's own (`page`, `api_*`, `websocket`, `attach`) are what
  a person on that machine would see.
- **HTTP waits.** A request's wait before its handler can't include time its bytes sat in the socket
  while its shard was busy (see above): that's in the shard's delay.
