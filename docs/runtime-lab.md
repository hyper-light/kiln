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
| `kiln_shard_delay_seconds{shard}` (histogram) | How late the shard ran its ready work: a probe's timer against when the probe ran. Up to a tick of the timer wheel (100 µs) and the thread's timer slack (the tick again) is by design. Its buckets are 25 µs apart to 250 µs and 50 µs apart to 1 ms (`shard_health::DELAY_BUCKETS`, goal 9), so its quantiles read to tens of microseconds below a millisecond. |
| `kiln_shard_stalled_seconds{shard}` | How long since the shard's probe last ran. A shard held in one poll shows here at once; its delay only once it's let go. |
| `kiln_shard_busy_seconds_total{shard}` | CPU time the shard's thread has run: its rate is the share of the time the shard was busy on the CPU. Linux and macOS (Windows keeps a thread's times at its scheduler's tick). Time the host held a ready shard off the CPU isn't busy: it shows as delay, and as `host` slow polls. |
| `kiln_shard_polls_total{shard}` | Task polls. |
| `kiln_shard_slow_polls_total{shard,cause}` | Polls that held the shard past twice its step quantum (266.7 µs, `rt::STEP_BUDGET_NS`, derived in goal 9's step 3: so past 533 µs), by who held it, as hyper-rt attributes them: `task` (it ran on the CPU), `blocking` (it waited inside a call), `host` (runnable, off the CPU: preempted), `unknown` (the OS couldn't tell: off the CPU on macOS, every one on Windows, and a shard's first long poll after a calm spell, since hyper-rt reads the thread's account only once a long poll has armed it). |
| `kiln_shard_longest_poll_seconds{shard}` | The longest single poll the shard has run. |
| `kiln_shard_quantum_seconds{shard}` | The step quantum the shard runs with, as its probe read it: the longest a poll is to hold it (goal 9's step 3), and half the hold its slow polls are counted past. `kiln-lab` holds what held the shards to it. |
| `kiln_shard_scheduler_overrun_seconds{shard}` | How late the shard's steps ran after its waits, as the shard measured it: the host descheduling it, decaying as it recovers. |
| `kiln_shard_held_polls_total{shard,site,label}`, `kiln_shard_held_seconds_total{...}`, `kiln_shard_held_longest_seconds{...}` | What held the shard: every poll past 100 µs (`hyper_rt::slow::SLOW_POLL_NS`, a tenth of goal 9's bar), counted against the spawn site of its task (`#[track_caller]`, `file:line:column`) and the label its code set (`label`: an HTTP request's route, as `read /api/harness/sessions`; `crate::route_latency::label_of`), with their holds added up and the longest. Each shard keeps its own table (`hyper_rt::slow`), which only it writes; `/metrics` reads it without a lock. |
| `kiln_shard_held_host_polls_total{shard,site,label}`, `kiln_shard_held_host_seconds_total{...}` | Of those polls, the ones the host held, and their time: the shard's thread runnable and off the CPU (preempted), as hyper-rt attributes a poll past twice the step quantum (`hyper_rt::attribution`), so a shorter one is never among them. What no code held, and cores of its own would (goal 9, plan 01a1a87b2cec steps 3 and 4); the lab's report gives each site and label's share. |
| `kiln_shard_held_cpu_polls_total{shard,site,label}`, `kiln_shard_held_cpu_held_seconds_total{...}`, `kiln_shard_held_cpu_seconds_total{...}`, `kiln_shard_held_longest_cpu_seconds{...}` | Of those polls, the ones whose shard's thread said its time on the CPU, their holds, that time, and the longest any ran on the CPU (goal 9's step 3): the shard thread's run time from a self-monitoring perf event's page, read with no system call as each poll starts (hyper-rt's `task_clock`; Linux on x86-64 and arm64, where the event can be opened). A hold less its time on the CPU was off it: the host's, or a call that blocked. |
| `kiln_shard_steps_total{shard}`, `kiln_shard_parks_total{shard}`, `kiln_shard_parks_withdrawn_total{shard}` | What each request costs a shard beside its polls (goal 9, plan 01a1a87b2cec step 5; `crate::shard_health::Wakes`): its loop's steps; its parks that waited in its driver, each a system call and a sleep; and those withdrawn, an inbox already holding work, which cost neither. |
| `kiln_shard_wakeups_total{shard,by}` | Its parks by what ended them: `kick` (another thread's wake or message: the driver's kick, one a park however many senders), `io` (a socket's readiness, a completion), `timer` (the deadline it waited for, or a signal). |
| `kiln_shard_wakes_taken_total{shard}`, `kiln_shard_wakes_sent_total{shard,to}` | Wakes from other threads the shard took in (bits drained: a task's wakes before it runs are one), and the wakes its own thread made through tasks' wakers (a channel's send, a broadcast's subscribers, a cell's publisher): `to="own"` to its own tasks, `to="other"` to other shards'. |
| `kiln_shard_kicks_sent_total{shard}`, `kiln_shard_kicks_saved_total{shard}` | The kicks its thread made to parked shards, each a system call on it, counted by the sender as it claims the park's kick; and the wakes and messages sent to it that found it running or its park's kick claimed, which cost their senders none (`hyper_rt::parking`). |
| `kiln_shard_threads_woken_total{shard}` | The threads its thread unparked: compute pool workers woken for the jobs it queued, a thread waiting on what its task published; each a system call on it. |
| `kiln_shard_park_seconds{shard}` (histogram) | How long its parks lasted, each from its driver wait's call to its return, in hyper-rt's bounds (`PARK_BOUNDS_NS`: doubling from 1 µs to 8,192 µs; 16 µs the first past the cheapest wake the lab measured on fedora, a socket's on the waiting thread's own CPU at 12.6 µs, wake-2): the parks a short spin before parking would have served sooner than their wakes, against the rest. |
| `kiln_shard_harvests_total{shard}`, `kiln_shard_arms_total{shard}`, `kiln_shard_socket_calls_total{shard,call}`, `kiln_shard_socket_calls_idle_total{shard,call}` | The rest of its system calls (`crate::shard_health::Calls`): its driver's non-blocking waits while busy (a harvest, once a quantum); its readiness registrations with its driver (epoll_ctl, kevent, an AFD poll), each a task's wait or a handle armed again for the waits left after a fire; its tasks' TCP calls, `call` `accept`, `read` or `write`; and of the reads and writes, those that moved nothing (a read that found nothing ready, a write that found the send buffer full), each with a wait after it. |

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
| `kiln_compute_unparks_total`, `kiln_compute_parks_total` | What handing it work costs (goal 9, plan 01a1a87b2cec step 5): its workers woken from their park, each a system call (a futex wake on Linux) on the thread that queued the job (a shard's, counted there too: `kiln_shard_threads_woken_total`) or on a worker that found more; and the times a worker went to park with nothing to do. |
| `kiln_compute_wait_seconds` (histogram) | How long each job waited for a thread: from its submission to its first piece. |
| `kiln_compute_piece_seconds` (histogram) | How long each piece held its thread. A piece ends once its job has run a shard's step quantum (266.7 µs) and another job waits, at the next unit of its work. |
| `kiln_compute_longest_piece_seconds` | The longest piece yet. |
| `kiln_blocking_wait_seconds{share}`, `kiln_blocking_run_seconds{share}` (histograms) | How long each job of the blocking pool waited for one of its share's places, and ran: `dns` (`getaddrinfo`), `store` (kiln.db's reads, each on a read-only connection of its own), `fs` (files, `/proc`, archives, a child's wait); and `store_write`, kiln.db's writer, a thread of its own that takes every call that may write in turn (goal 5). |
| `kiln_blocking_places{share}` | Each share's places: the most of its jobs that run at once (`crate::rt::Profile`); `store_write`'s one. |

### What overload sheds (`crate::overload`)

Goal 6 of the API runtime program (plan `01a1a59c2abe`): each request is Critical (health, sign-in, a
person's pages and actions), Normal (the API) or Background (agents' and CI's polling, catch-ups, sync,
telemetry). Goal 1's probe on each shard ticks a watch for each of the shard's queues: the shard's delay
for Normal, its Background tasks' waits for Background. The first shard's probe also ticks one for the
compute pool's line, each of the blocking pool's shares' and kiln.db's writer's, on the mean wait of the
jobs whose waits the pool told over the period, since what a request waits on off its shard waits there,
where no shard's delay sees it. Each watch learns its queue's calm (the typical wait of its periods outside
its standing) and sheds only on a queue that *stands*, as CoDel bounds queues (research f14), over a
tolerance (1 s, Kubernetes' default probe timeout), a wait counting past its calm when past the calm and
the shard's quantum. A shard's queue stands while most of the last tolerance's periods were past its calm
and, where the OS keeps a thread's CPU time, the shard ran most of it (a mostly idle shard's late wakes are
the host's). A pool's line stands once every period for a tolerance was past its calm, the line never
emptying, and work that came after it formed kept it standing (it stood a tolerance longer than its jobs'
longest wait), not one request's batch worked off. A queue that stands sheds Background requests; one that
stood another tolerance with Background shed, Normal ones too (a shard's: its API queue's); Critical never;
shedding stops once it no longer stands. A shed request is
answered 503 at once, before its body is read, with `Retry-After` (the delay that shed it, in whole
seconds) and `x-kiln-shed` (its class); kiln's client sends it again after that (a file's upload read
again from its start, a WebSocket's opening too). Shedding is off until `[server] shed = true` or
`KILN_SHED=1`; the watches run either way.

Calm is bursty, and a queue that comes back is not overload: in the lab's calm on fedora (2026-11-05,
research f32) the store share's jobs waited 307 to 620 ms at p99 (one attach's reads queued behind each
other while the share held under one of its 24 places on average) and every round's shards ran a probe
250 ms late; the CUSUM on each period's mean wait that this replaced (headroom's, with nothing it
learned of calm's excursions) read those as shifts and shed 6.5 Background requests a calm round. Its
unit tests replay those waits (`overload::tests`, `g6_s4_*`): the lab's calm store share, fan-outs whose
reads wait up to 2.3 s, the calm's probes with startup holds of 2.9 s and a mostly idle shard's tick of
jitter shed nothing; work outrunning a pool and a closed loop past its places shed Background a tolerance
in and Normal a tolerance later; the lab's flood and a saturated shard, half a tolerance in and a
tolerance later.

| series | what it says |
| --- | --- |
| `kiln_shard_queue_delay_seconds{shard,class}` | Each class's mean wait from ready to run on the shard, over the probe's last period. |
| `kiln_shard_queue_standing_seconds{shard,class}` | How long the class's queue on the shard has stood (most of the last tolerance past its calm, the shard running most of it), 0 once it no longer does. |
| `kiln_shard_shedding{shard,class}` | 1 while the shard's own queue, standing, sheds the class's requests. |
| `kiln_pool_queue_delay_seconds{pool}` | Each pool's mean wait for a place over the probe's last period, as its watch read it: `compute` (a thread), `dns`, `store`, `fs` (the blocking pool's shares), `store_write` (kiln.db's writer). |
| `kiln_pool_queue_standing_seconds{pool}` | How long the pool's line has stood past its calm, never emptying (0 once back). |
| `kiln_pool_shedding{pool,class}` | 1 while work standing in the pool's line sheds the class's requests on every shard. |
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
do, rather than asking again at once. `--profile flood` is the overload profile with 64 readers and 8
agents calling MCP, none thinking: more work than one shard runs, as goal 6 overloads one on purpose.
`--profile many` is production's whole shape rather than its
busy hour's share: its ten runners and all 134 live feeds (the realistic profile's sizes, the rest of
400 events), every one at work at the realistic pace, as goal 4 measures many sessions changing at
once.

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

Every latency is reported to the microsecond (goal 10, plan `01a1a87b2dae`, step 1: its bar is under
a millisecond). Beside the numbers an A/B compares, not among them, each round breaks its classes down
(`seen` in `run.json`): each API read by its route (`api_read.sessions`, `.session`, `.events`,
`.agents`, `.me`, `.overview`, `.runs`, `.plans`, `.goals`), each API write by its kind
(`api_write.command`, the MCP calls'), pages and their files apart, and the frames by how they met
the attaches, as each viewer saw them: `websocket.own_attach`, published before the viewer's own
snapshot came, which they wait for by protocol; `websocket.while_another_attaches`, published while
another viewer of the same session attached, which shouldn't wait for that; and `websocket.live`, the
rest.

The shards' delay is reported to the microsecond (goal 9): all shards together and each shard
(`shards.delay_p50_us`, `_p99_us`, `_p999_us`, `_max_us`, and `shard<N>.delay_*_us`), the max being
the top bucket the window reached; each shard's busy share (`shard<N>.busy_share`) beside their
mean; and the worst shard's p99.9 and busy share (`shards.worst_delay_p999_us`,
`shards.worst_busy_share`). What held the shards past 100 µs (`kiln_shard_held_*`) is each round's
too, in the window and over the fill (`held`, `fill_held` in `run.json`), and the report lists it by
spawn site and label: polls and milliseconds held a second, the mean hold and the longest.

What each request cost the shards beside its CPU (goal 9, plan `01a1a87b2cec` step 5): their loop steps,
polls, parks and wakes over the window, summed over the shards, over its requests (`shards.<what>_per_request`):
`steps`, `polls`, `parks` (each a system call and a sleep) and `parks_withdrawn` (none), the parks by what
ended them (`wakeups_kick`, `wakeups_io`, `wakeups_timer`), the wakes from other threads taken in
(`wakes_taken`), the wakes the shards' threads made (`wakes_sent_own`, `wakes_sent_other`), the kicks they
sent (`kicks_sent`, each a system call on the shard) and were saved (`kicks_saved`), the threads they
unparked (`threads_woken`), their driver's harvests (`harvests`) and readiness registrations (`arms`), and their tasks' TCP calls
(`socket_accepts`, `socket_reads`, `socket_writes`) with those that moved nothing (`socket_reads_empty`,
`socket_writes_full`): with the parks and kicks, most of the system calls a request costs the shards, summed
as `shards.syscalls_per_request` (not counted: a socket's options and shutdown, the probe's clock, the futex
a channel to a thread of kiln's own may wake inside std, and, before goal 9's step 5, the read that drained
the kick's eventfd on each kicked wake). Beside them, what each request handed the pools: the compute pool's jobs, the
workers they woke and the times a worker went to park (`compute.jobs_per_request`, `.unparks_per_request`,
`.parks_per_request`), and each blocking share's jobs, kiln.db's writer's among them
(`blocking.<share>.per_request`). How long the shards' parks lasted, all shards together: `shards.park_p50_us`,
`shards.park_p99_us`, and `shards.parks_under_16us_share`, the parks a spin as long as fedora's cheapest wake
would have served sooner than their wakes.

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

On Linux, `--server-cpus <list>` starts the server and its hogs on those CPUs (`taskset`) and keeps
the CPUs the lab was started on to its clients, runners and stand-ins, so the lab times the server, not
itself; `Nx` hogs then count the server's CPUs. `--layout production` (with systemd's user manager)
starts the server in a scope of its own at `CPUWeight=1000` and the hogs in one at `CPUWeight=1`, as
kiln.slice and CI legs and agents run on bare metal (docs/priority.md); `shared`, the default, keeps
everything in the lab's own cgroup. The two together are production's layout:

```sh
# the lab's clients on CPUs 8-15; the server and as many hogs as its CPUs on 16-31, at production's weights
taskset -c 8-15 target/release/kiln-lab run --kiln target/release/kiln --server-cpus 16-31 --layout production --hogs 1x
```

`KILN_LAB_HOOK` names a command the lab runs as each part of a round begins and ends
(`<hook> <event> <server's pid> <folder>`: `fill-start`, `fill-end`, `window-start`, `window-end`; the
folder `<round>.hook`, kept when the round's kiln is removed): `scripts/shard_lab/hook.sh` has perf
record the server over the window.

`--bus-memory <bytes>` writes `[bus] memory` into the kiln.toml each round's server starts with
(kiln's default otherwise, 171 MiB), so a run measures the bus past its default (goal 11 below).

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

## Goal 4: each agent session's copy on a shard of its own

Goal 4 of the API runtime program (plan `01a1a59c2a9c`, research `01a1a59c2a9d71a2878c4d9beae87c30`)
took the server's copies of agent sessions from under one lock: each copy has an owner shard
(`crate::harness::copies`) that alone changes it, as messages come to it, and publishes an immutable
view of it that every reader loads without waiting (docs/bus.md).

### The A/B against main

On the baseline's machine and its efficient cores (`taskset -c 16-31`; the server sized itself to 4
shards), the `realistic` profile, eight rounds of each, interleaved, on 2026-11-04. A: main at
`01aba3ff882`. B: goal 4 at `7cda56472b8`, which wrote a viewer's snapshot and catch-up on the
compute pool, in pieces, and the agents list there too. The machine was busy with other work (load
26 to 38 before each window).

Calm (15:05 to 15:29 UTC), 1 of 43 numbers differs beyond noise:

| number | A | B | difference | verdict |
| --- | --- | --- | --- | --- |
| server RSS MB | 1086 | 1175 | +8.2% | worse |
| (within noise) websocket p99 ms | 872 | 915 | +4.9% | same |
| (within noise) server: websocket waited p99 ms | 861 | 902 | +4.8% | same |
| (within noise) server: runner waited p99 ms | 192 | 201 | +4.8% | same |
| (within noise) server: runner took p99 ms | 0.240 | 0.237 | -1.4% | same |
| (within noise) attach p50 ms | 1074 | 1092 | +1.7% | same |

With 16 CPU hogs in the lab's own cgroup (15:47 to 16:09 UTC), 13 of 43 differ beyond noise, among
them:

| number | A | B | difference | verdict |
| --- | --- | --- | --- | --- |
| attach p50 ms | 1550 | 3745 | +141.6% | worse |
| attach most ms | 2432 | 9415 | +287.1% | worse |
| websocket p99 ms | 1514 | 6701 | +342.7% | worse |
| server: websocket waited p99 ms | 1811 | 8607 | +375.4% | worse |
| api_read p50 ms | 1.20 | 2.02 | +69.0% | worse |
| server: runner waited p99 ms | 110 | 199 | +81.6% | worse |
| server RSS MB | 1064 | 1158 | +8.8% | worse |
| (within noise) server: runner took p99 ms | 0.676 | 0.430 | -36.4% | same |

### What it says

- **The snapshots' move to the compute pool cost the most.** The pool's threads run below the shards
  (SCHED_BATCH at nice 10), so beside busy processes at the server's own priority in its cgroup a
  snapshot got a small share of a CPU: attaches took 2.4 times as long at the median, and the frames
  their viewers waited on with them 4.7 times at p99. Snapshots, catch-ups and the agents list are
  written where main writes them again (`88f421838fb`: the blocking pool, and the shard that asked).
- **One owner shard per session moves neither of the baseline's two tails: neither is the lock's.**
  The live feed's p99 wait is each viewer's own snapshot: a viewer takes its place on the feed, then
  writes its whole snapshot before it reads a frame, so what's published meanwhile waits for it; that
  wait rises with the attach time in every condition measured (above and in main's baseline). Runner
  messages wait for the hub's loop, not for a lock: applying one took 0.24 ms at p99 on both builds,
  the old lock included; the loop is kept off its shard, the HTTP listener's, by that shard's other
  work.
- **Not measured yet: production's layout.** There the server runs at `CPUWeight=1000` and legs and
  agents at `cpu.weight` 1 (docs/priority.md), so its compute pool competes with kiln's own shards
  alone. Which pool should write snapshots there, and the tails against main, wait for that A/B.

## Goal 6: shards that protect themselves

Goal 6 of the API runtime program (plan `01a1a59c2abe`, research `01a1a59c2abf76c79f9f50edffd98867`):
tasks give up their shard on time, kiln-rt's watchdog moves work off a stuck shard and says why, every
request has a class, and overload sheds Background requests first ("What overload sheds", above).

### Priorities: Critical's p99, calm and with CPU hogs (step 3)

In the lab the Critical class is its pages and their files (`page`): its API calls carry a token, so
they are Normal, and its catch-ups of a conversation (`/events` with a token) Background.
`scripts/overload_lab/driver.sh` measures them in two layouts, the configurations taking turns round by
round as goal 8's driver does (docs/priority.md, "The lab"):

- **production's:** the server alone in a scope of its own at `CPUWeight=1000`, as `kiln.slice` gives
  it on bare metal (`scripts/overload_lab/kiln-scoped`, which the lab starts its server through), the
  lab's runners and clients in the lab's unit at the default weight, and the hogs in a scope of their
  own at `cpu.weight` 1, as CI legs and agents run (configurations `calm` and `hogs`);
- **shared:** the server, the lab's runners and clients and the lab's own hogs in one cgroup
  (`shared-calm`, `shared-hogs`).

```sh
# Linux with systemd's user manager; as many hogs as the lab's CPUs, all of it on those CPUs
ROUNDS=8 N=16 CPUS=16-31 scripts/overload_lab/driver.sh target/release/kiln target/release/kiln-lab ~/.cache/kiln-lab/g6-priorities
kiln-lab table calm=~/.cache/kiln-lab/g6-priorities/runs/calm/run.json hogs=~/.cache/kiln-lab/g6-priorities/runs/hogs/run.json
kiln-lab compare ~/.cache/kiln-lab/g6-priorities/runs/calm/run.json ~/.cache/kiln-lab/g6-priorities/runs/hogs/run.json
```

The step holds when `page.p99_ms` with hogs is within noise of calm in production's layout.

### Shedding by priority, overloaded on purpose (step 4)

What the lab found on fedora (2026-11-05, main `0c11fb03a06`, 6 interleaved rounds each, the server alone
in its scope on 8 CPUs of its own, the lab on 8 others): with the CUSUM shedding replaced, a calm server
(the `realistic` profile, 4 shards) shed 6.5 Background requests a round (research f32), and one
overloaded on purpose (the `overload` profile, one shard) 14.5 (f31), neither moving a latency beyond
noise. Both were calm's own fan-outs in the store share: its jobs waited less at p99 overloaded (97 to
206 ms) than calm (307 to 620 ms). The overload itself was the server's CPUs, 7.8 of its 8 busy, mostly
with the blocking pool's `fs` share's threads (9 places busy, the attaches'), while the one shard stayed
34% busy, its probes late by a scheduler's tick at a time (p99 17 ms): no queue of the server's stood. So
the overloaded run now uses `--profile flood`, the overload profile with 64 readers and 8 agents calling
MCP, none thinking, more work than one shard runs, its readers' event catch-ups Background, its other
reads and MCP calls Normal, its pages and `/api/me` Critical.

The standing rule's first runs (fedora, 2026-11-05 10:44 to 11:17 UTC, d46ceb48d67, 6 interleaved rounds
each): calm, every shed-on round shed nothing (pages p99 0.48 to 0.66 ms). Flooded, the one shard ran 81 to
82% of the time, its probes 4.2 to 6.4 ms late at the median and 24 ms at p99 and pages 16 ms at the median
(25 ms at p99), served throughout, yet one shed-on round of six shed (68 Background requests, 27 Normal):
the rule then asked every period of a tolerance to be past its calm, and the few probes each second that
found the shard idle kept it from standing (3 windows in 12 rounds). A shard's queue now stands while most of
the last tolerance's periods were past its calm and the shard ran most of it; replayed, that flood sheds
Background half a tolerance in and Normal a tolerance later in every round (`overload::tests`).

That rule's runs (fedora, 2026-11-05 12:25 to 12:59 UTC, rt-int `e7cdefced96`, 6 interleaved rounds each,
the server alone in its scope on 8 CPUs of its own, the lab on 8 others): flooded, every shed-on round shed
both classes (Background 568 to 1,693 requests a round, Normal 811 to 1,616), the one shard ran 52 to 55%
of the time against 80 to 81% with shedding off, and pages were served with no error in all 12 rounds, at
4.2 to 5.3 ms at the median against 13 to 15.5 ms with shedding off. Their p99 stayed where it was (20.8 to
23.2 ms, against 18.2 to 24.6 ms), but in one shed-on round (round 7) the machine's load went from 17 to 33
with other work as it ran: the lab sent 60% fewer requests, and 1% of pages took 4.1 s or more (6.6 s at
the most), none failing. Calm (`realistic`, 4 shards), no shard's queue stood in any round. The only sheds,
2 and 6 requests in two of the six shed-on rounds, came from the blocking pool's watch on `kiln.db`'s
writer, whose line stood 2.4 to 3.7 s past its calm of 0.32 ms, its jobs waiting up to 2.5 s: a stall of
seconds in the store's one writer, not a burst (fedora's disk was full and shared with other work then).

`scripts/overload_lab/overload.sh` runs a profile shedding off against on, interleaved (`kiln-lab ab`, the
server in its scope as above): one shard and `PROFILE=flood` overloads it on purpose; with
`PROFILE=realistic SHARDS=default` it asks what shedding costs a calm server, which should be nothing.

```sh
ROUNDS=6 CPUS=16-31 PROFILE=flood scripts/overload_lab/overload.sh target/release/kiln target/release/kiln-lab ~/.cache/kiln-lab/g6-overload
ROUNDS=6 CPUS=16-31 PROFILE=realistic SHARDS=default scripts/overload_lab/overload.sh target/release/kiln target/release/kiln-lab ~/.cache/kiln-lab/g6-calm-shedding
```

Each round's `server.shed.background` and `server.shed.normal` are the sheds by class
(`kiln_shed_total`), and `page.errors` and `page.p99_ms` whether Critical requests were served. Each round's
server log says each queue that stood, when and why (`shard overloaded: its queue stood past its calm`,
`work waiting in a pool stood past its calm`), and its `/metrics` how long each queue stood
(`kiln_*_queue_standing_seconds`).

In process, `crates/kiln/tests/overload.rs` shows both kinds of watch at work on kiln's own HTTP server
with shedding on: busy tasks of each class flooding every shard shed Background requests, then Normal ones,
while Critical ones are served; and a backlog in the blocking pool's `store` share while every shard is
calm (its line kept full so its jobs wait about 200 ms each for a place, then 1.5 s) sheds Background
requests once it has stood a tolerance past those waits, Normal ones a tolerance later, never Critical
ones, and `/readyz` says not ready, naming the share, while the API is shed. Sheds are counted by class in
both.

## Goal 5: no lock on the hot path

Goal 5 of the API runtime program (plan `01a1a59c2aae`, research `01a1a59c2aaf77dfa0fe35f461a3d75f`):
no request, websocket or runner path takes a lock two shards share, since a shard the OS preempts while
it holds one stalls every shard that needs it (research f2), and spinning before parking doesn't change
that (f5, f6).

### Counting the locks the lab's load takes

`scripts/locks.py` counts them. `census-tree` copies the tree and routes kiln's, kiln-http's and Quiver's
`parking_lot` and `std` locks through `scripts/locks/census.rs`, whose wrappers count each acquisition at
its call site (`#[track_caller]`), whether it found the lock held and how long it then waited, and the
first few addresses of the locks taken there; it counts the store's calls at their callers too
(`store_call`), with how long each took. Built in release, that kiln says it all on `/metrics`
(`kiln_lock_*`), which kiln-lab keeps for each round's window and fill (`Round::locks`, apart from the
numbers an A/B compares), and `rank` ranks the locks (the sites that took one lock, by its address,
together) by how often the window took each:

```sh
python3 scripts/locks.py census-tree --to ~/.cache/census --ui ui/dist
(cd ~/.cache/census && CARGO_PROFILE_RELEASE_LTO=off cargo build --release -p kiln -p kiln-lab --bin kiln --bin kiln-lab)
taskset -c 16-31 ~/.cache/census/target/release/kiln-lab run --kiln ~/.cache/census/target/release/kiln --label census --rounds 3 --out census
python3 scripts/locks.py rank census/run.json
```

### What each became

- **The store** (`crate::store`). Its one connection behind a lock took every statement in turn: the
  lab's API reads waited 82 ms at p99 there, its writes 441 ms (goal 3, above). Now what may write goes
  as a message to the one connection that writes, owned by a thread of its own (`kiln-store`), which
  runs the calls in turn (SQLite takes one writer at a time); and what only reads (`Store::read`) takes a
  read-only connection of its own on the blocking pool's `store` share, from a lock-free queue of those
  not in use, in one read transaction, so reads run side by side, with each other and with the writer
  (WAL), and see every write committed before them. The API's reads on the lab's paths (the runners,
  the overview, runs, plans, goals, sign-in and access checks) and the store's own read methods read so.
  The share's places are goal 3's Little's-law measure of the store's calls (24); the writer's waits and
  runs are `kiln_blocking_*{share="store_write"}`.
- **Read-mostly state** is an immutable copy swapped in whole (`crate::rcu::Rcu`, on arc-swap; f7, f9):
  readers take the current copy without waiting on anyone, a writer copies, changes and swaps it in (a
  compare-and-swap, tried again on a race). The hub's runner processes' links, the runners' labels and
  samples, the terminals' outputs, the AI sessions each machine reported, the refusals of what the wires
  couldn't read, the pods it launches and the tokens they enroll with, paused jobs, cancels, declines,
  lost processes, tool fixes and repositories asked to be polled; the metrics' labelled families (their
  series' values atomics); the UI's compressed files; Ship's model (its repositories' rows shared between
  copies); the logs' live jobs.
- **The runners and their processes** (the hub's `agents` and `instances`), read on every runner's
  message, every agent session's update and every command sent to one, are copies too. A runner's record
  is its own `Arc` in the copy, so a change copies the map's pointers and that one record
  (`Hub::change_agent`); its last word's time is written once a second at most, not once a message. The
  processes' copy changes only as processes come and go and as what they hold changes: a session's
  update from the process holding it, and a message about what one holds, read it without writing
  (`Instances::routes`).
- **What agent sessions' updates read beside their copies** (the registry's names given here, epochs,
  which runner process holds each, keep-offs, changesets waiting for the conversation store, what each
  machine offers, uploads, starts under way) and **each machine's agent limit** are copies too: a
  session's state read on its copy's shard takes no lock the shards share, and a copy is swapped in only
  when something changes (a name taken up, a holder moved, an epoch raised). Two writers that hand work to
  a writer of their own on the blocking pool (holders.json, changesets) read its flag past a SeqCst fence
  that pairs with the writer's, so a change made as the writer lets go is written by it or starts the next.
  Three registry locks are off the hot path, `hot.toml` saying why: the server's own transcripts' scanner
  and holders.json's one writer, both on the blocking pool, and messages waiting for their inbox's answer
  (CP-T3.2), taken twice a message.
- **Terminals** (`termsess.rs`): every session, the names given them and where those are kept are
  copies, and what the list shows of a session (how many are attached, when it was last active, its
  size, its title, how it ended) are atomics and copies beside it, so the list, a keystroke and a detach
  take no lock. A session's screen keeps a lock of its own, taken by its relay for its output and by its
  own viewers as they attach or resize it, so a replay and the live output meet with no byte twice or
  lost; `hot.toml` says so.
- **Questions put to runners** waiting for their answers (`crate::rcu::Pending`): each in a take-once
  cell of its own, the cells' map an `Rcu`, so an answer and its asker's giving up take it at most once.
- **The runners' commands** (`crate::wire::commands::Owner`): one task owns their state machine and takes
  every change and question as a message, in order; a sync path's command (`Hub::command_soon`) is queued
  in its turn and its answer comes on its one wait.
- **CI's engine's legs, runs and kept legs** (`ci::dispatch::Handle`), which the hub reads on every message
  a runner sends about a job (is it one of the engine's legs, does this runner keep it) and the API on its
  calls, are copies: the dispatcher, the legs' and runs' one writer, swaps a new one in as they come and go,
  and a kept leg's (the dispatcher's or the API's) is a compare-and-swap. A reader sees every change made
  before it looked, as under the lock; a finished run's legs go in one swap, so it sees all of them or
  none. What the dispatcher rebuilt as it started is set once, and its messages' end is taken once.
- **CI's caches** that the API's calls and the runs being planned read are copies too: each repository's
  trunk files (`.kiln/config.yaml`, `routing.yaml`, `environments.yaml`, read again at most every 20 s),
  its tests' timings (shards balanced by time), the package graphs of the newest commits, the patch-ids
  of commits signed over, and how far each runner's checkouts are behind. A value read again goes in with
  the others' pointers (each value its own `Arc` where it's more than a few words); readers that found one
  stale both read it again, as under the lock.
- **Ship's Deploys** (`ship::deploys::Model`): the facts as last gathered, which every list reads, are a
  copy a new gathering replaces whole, and so are each source's newest commits.

### What's left in the bus, Ship and CI, and why it's off the hot path

The census (`python3 scripts/locks.py list`; every Mutex and RwLock outside a test build's code in
`bus.rs`, `ship.rs`, `ship/`, `ci/` and `store.rs`, and kiln-seams' `bus.rs`) found none on the bus's
side in kiln: its node is Quiver's (vendored; its locks are its own change), and `SimBus`'s is the
simulator's, built for tests only. In Ship, only Deploys' (above). In CI, besides those above, these,
none taken on a runner's message, a page's read or a websocket's frame:

- **Async locks**, which park the task that waits and never a shard (a holder whose shard the OS
  preempts holds up only that lock's waiters): the table of steps in flight (`services/memo.rs`, its
  leases changed in order, each kept in SQLite before the next); a digest's put against its collection
  (`services/cas.rs`'s stripes); one artifact finishing at a time (`services/pipelines.rs`); the OIDC
  keys' rotation (`oidc.rs` `ROTATE`, so two requests never both make a key); a push to a repository kiln
  hosts (`hosting.rs`, one at a time a repository, its `refs/for/*` the way in); the search's meta file and
  a tree's index while it's built (`search.rs`); an incident's bookkeeping (`attach.rs` `WAKING`).
- **One loop's own, or taken as something rare happens:** the scheduler's sweep every 5 s (`notify.rs`
  `OFFLINE`, `QUARANTINED`, `UNUSED_LOOKED`), a release's move (`MOVING`), a cache's eviction
  (`NEAR_QUOTA`); a push (`QUARANTINED`, and `fresh.rs`'s queues of trunk moves and of changes with a new
  revision, which its replayer's loop takes); kiln's events, by the attachments' listener (`attach.rs`
  `FROM_REPOS`, read again at most once a minute, `UPGRADES_SEEN`, `UPGRADES_THROUGH`) and by review's
  (`cd_events.rs`, CD's apps moving); the replies mailbox's loop (`replies.rs`); the land queue's and the conflict
  ladder's loops, and a review or landing as it's given (`land.rs`, `ladder.rs`); a preview's leg ending,
  or a runner that takes previews down connecting or saying it did (`previews.rs`); an attest step's
  signature (`attest.rs`); a credential minted for a brokered secret, a deployment or a tracker, each
  around a token exchange over the network (`integrations/aws.rs`, `azure.rs`, `gcp.rs`, `oci.rs`,
  `trackers.rs`), a cloud service's endpoint, read as each call to it is made (`integrations/mod.rs`,
  where a test puts its fakes), and the KMS key's public half, fetched once (`signing.rs`); a person's
  search of a change (`search.rs` `TREES`, `CHANGED`, `BUILDING`, around git and an index's build); a push
  to a repository kiln hosts (`hosting.rs`'s map of its locks); an artifact kept in CAS, and the collector
  (`services/cas.rs` `uploading`); the issuer's own listener (`[ci.oidc] listen`), whose documents' rate
  limit by client (`oidc.rs` `asked`) clouds meet as they fetch kiln's keys, never the API's listener.
- **One request's own:** GraphQL's errors and missing fields (`rest/graphql.rs`), made and dropped with
  its request, so no other request, and no other shard, takes them.

`scripts/locks/hot.toml` names the files above that became lock-free, and the CI modules the API's runs,
legs, pools and insights are read through, so a lock there fails the check.

### Keeping them out

`python3 scripts/locks.py check` fails when a Mutex, RwLock or Condvar appears in the modules
`scripts/locks/hot.toml` names (the HTTP server and the API, sign-in, the hub, the runners' link and
processes, the websockets' feeds, the bus, the logs, Ship, the store) outside a test build's code, but
those the file lists, each with why it's off the hot path, or (`todo`) what replaces it. Both CIs run it
(`.kiln/ci.yaml`'s `locks` job, `.github/workflows/ci.yml`'s), with `scripts/test_locks.py`, which plants
a lock in a copy of the hub and holds the check to failing on it.

Reach's relay between runners has one owner, a task of its own (`Hub::relay_loop`): the hub's loop
hands it each relayed frame without waiting, it forwards them in order, and the reach directory reads
each runner's announcement from a copy. A live job's files have one writer each, a task of its own that
writes what it's sent (its runner's chunks, which the hub's loop waits for as it waited for the write; its
steps; its end) in order, a batch at a time on the blocking pool; a read of the job, and a retry's mark,
wait for what was sent before, and a retried job's next writer for the last one. A runner's front
door answering reads from its replica (the peer-to-peer unification's P2, plan 01a1a283e14c) takes
its learner from an atomic pointer on each connection, the owners it knows from a copy, whether its
replica is behind from an atomic, its kept connections to the server from a lock-free queue, and its
learner's questions of the server from take-once cells; the server's side reads its learners'
machines from a copy. The live channel's receivers (`wire/link.rs`) are the hub's loop's alone: it
takes them for each item a runner sends and then its answer, as before, and publishes what they have
of the stream it changed into a copy per stream, which the commands loop's have-again (every few
seconds) and a link coming up read: what they say, and when and where acks go, are as they were.
Inside Quiver (vendored, the bus's node): its topics' map and each topic's tail, for Quiver's own change.

### The A/B against main before goal 5 (step 4)

fedora, 2026-11-05, production's layout (each build's server alone in its scope at `CPUWeight=1000` on
CPUs 16-31, the lab on the others), the `realistic` profile, 8 interleaved rounds each, calm and with 16
hogs on the server's CPUs at weight 1: main before goal 5's first part (`426b02ea011`) against train 3's tree
(`7dd67a5ce0f`, on main as `5c4047a47e5`). That tree holds goal 5's parts and also what goals 9, 10 and 11
landed in trains 1 to 3, so this is the program's change, not goal 5's alone.

First the locks (`scripts/locks.py census-tree`, then `rank`, train 3's tree, 3 rounds calm and 3 with the
hogs): 180 call sites, ranked by how often the window took each. The most taken, the store's capture (2,746
a second), its tables (902), its learner's joiner (603), its groups' point (556), Quiver's node (487) and
the live channel's receivers (459, one taker), each waited 0.000 ms a second; only Quiver's own locks
(vendored) waited, 0.047 ms a second at the most. No lock on the hot path is waited on.

Then the A/B, the medians of the rounds (ms, but the busy share):

| number | calm, before | calm, after | hogs, before | hogs, after |
| --- | --- | --- | --- | --- |
| `api_read.p99_ms` | 508.9 | 1.05 | 532.8 | 1.96 |
| `api_read.p999_ms` | 1,429.7 | 1.97 | 1,596.7 | 5.33 |
| `websocket.p99_ms` | 64.7 | 2.54 | 41.2 | 5.73 |
| `attach.p99_ms` | 569.3 | 6.50 | 1,318.1 | 12.5 |
| `server.runner.wait_p99_ms` | 57.2 | 3.00 | 39.2 | 4.90 |
| `blocking.store.wait_p99_ms` | 866.8 | 0.46 | 1,015.1 | 1.73 |
| `page.p99_ms` | 0.483 | 0.521 (same) | 1.677 | 1.680 (same) |
| `shards.busy_share` | 0.057 | 0.036 | 0.050 | 0.030 |

No tail got worse, calm or with hogs. What did, past noise: the medians of pages calm (0.200 to 0.226 ms,
+13%) and of API writes (1.02 to 1.16 ms calm, 1.39 to 1.58 with hogs, +14%), the runners' messages' p99
time to be taken with hogs (0.47 to 0.71 ms), and the compute pool, which now runs seven times the jobs (69
to 475 a second calm) and whose jobs wait longer for it at p99 (0.25 to 7.4 ms calm, 3.2 to 36 ms with
hogs): the work goals 3, 9 and 10 moved off the shards (shard 0, which ran 18.5% of the time calm, runs
2.2%). Goal 9's step 5 (a tenth of the CPU) and goal 10's step 4 (runner messages) take those up.

## Goal 7: placement

Goal 7 of the API runtime program (plan `01a1a59c2aea73e487fb6fcfe90c5fcc`, research
`01a1a59c2aeb70e9824606865d6762ca`) built three more ways for the server to place its work on kiln-rt's
shards beside today's. `[server] placement` picks one, and `KILN_RT_PLACEMENT` overrides it, so each runs
from the setting alone, with no rebuild (`crate::placement`):

| placement | a new connection | a request on a connection | a held shard's queued work |
| --- | --- | --- | --- |
| `round-robin` (main before goal 7) | the next shard in turn, past one the watchdog sees held | in its connection's task | spawns not yet admitted go to another shard (goal 6) |
| `steer` | the less delayed of two shards drawn at random, held ones passed over | on its connection's shard, unless one other drawn at random is less delayed by more than a step quantum (250 µs), or its own shard is held | as `round-robin` |
| `steal` | as `round-robin` | a movable task (`hyper_rt::movable`) queued on its connection's shard, which an idle shard takes half of a busy one's queue of | the watchdog hands it to the shared queues and kicks a parked shard |
| `both` | as `steer` | as `steal` | as `steal` |

- **A shard's delay** is goal 1's probe's: how late its latest probe ran, or how overdue its next one is
  (`crate::shard_health::delay_now`), so a shard held right now shows before its probe runs again.
- **A request is weighed against its own shard** and one other drawn at random, not two drawn afresh: moving
  it costs a hop each way between threads, worth paying only for a wait longer than the step quantum, the
  least wait goal 6 takes as a queue (`crate::overload`). Drawing at random, rather than taking the least
  delayed of all, keeps placers acting on the same stale reading from piling onto one shard (research f10).
- **Movable tasks** (kiln-rt's `movable.rs`, ORIGIN.md) are reference-counted headers beside the arena's
  pinned tasks, their waker the header itself. Each shard keeps a first-in, first-out queue of its own per
  class (crossbeam-deque), bounded at its poll batch, the older half moving to a shared queue when full; an
  idle shard takes from the shared queues and then half a sibling's queue, at most half the shards looking at
  once; a push behind another task kicks a parked sibling while none looks (Tokio's layout, f14 to f16). A
  carrier task per class polls a shard's movable tasks, each once a turn until its share is used, so a
  movable task's yield is one turn of a round, as hyper-rt's is (f13). Sockets and timers stay on their
  shard: a movable task's waits go through a helper task on the shard each began on (`crate::rt::Bridge`,
  `crate::rt::native`).
- **WebSockets** are placed with their upgrade request: a socket's task is spawned where that request's
  handler ran. A viewer's heavy messages, its catch-ups, are made off the shards (the blocking pool, goal 3)
  from state its session's owner keeps (goal 4), so none is heavy on the socket's shard.
- `/metrics` says what was placed how (`kiln_placement_total{how}`: `steered`, `local`, `moved`, `movable`,
  `refused`) and what the movable tasks did (`kiln_rt_movable_*`: spawned, polls, stolen, taken from the
  shared queues, overflowed, handed off by the watchdog, parked shards kicked).

### The rule, written before the runs (2026-11-04)

The default is the placement that gives people's requests the best p99 under CPU hogs in the production-like
layout, without losing throughput or CPU per request beyond noise:

1. **The deciding run** is the production-like layout with hogs: the server alone in a cgroup of its own at
   `cpu.weight` 1000 (`kiln.slice`'s, docs/priority.md), as many busy loops as the lab's CPUs in another at
   `cpu.weight` 1, the lab and its runners in a third at the default weight, all on the lab's CPUs.
2. **People's requests** are the lab's `page` (a person's pages: kiln's Critical class) and `api_write`
   (messages to agents and agents' MCP calls: what stands in for a person's actions; the lab sends them with
   a token, so kiln serves them as Normal).
3. **A placement qualifies** when, against `round-robin` in the deciding run (`kiln-lab compare` of the two
   placements' rounds, every number held together at 95%, Bonferroni):
   - `page.p99_ms` or `api_write.p99_ms` is better beyond noise, and neither is worse beyond noise;
   - no throughput (`page`, `api_read`, `api_write`, `websocket` and `attach` `.per_s`) and not
     `server.cpu_ms_per_request` is worse beyond noise.
4. **The winner** is the qualifying placement whose two p99s improved most against `round-robin`, by the
   mean of their relative changes. When none qualifies, `round-robin` stays the default.
5. The shared layout and calm are run and reported beside it; a placement worse beyond noise there in
   people's p99s is named, but they don't change the pick: production runs the production-like layout.

### Running it

On a Linux machine with systemd (fedora, as goal 8's lab), release builds of one commit beside the scripts:

```sh
mkdir -p ~/.cache/g7-lab/bin && cp scripts/placement_lab/*.sh scripts/placement_lab/merge.py ~/.cache/g7-lab/
cp target/release/kiln target/release/kiln-lab ~/.cache/g7-lab/bin/
systemd-run --user --unit=g7-lab --collect -E HOME="$HOME" -E PATH="$PATH" \
  env ROUNDS=6 N=16 CPUS=16-31 ~/.cache/g7-lab/driver.sh
~/.cache/g7-lab/report.sh      # once runs/DONE is there: the tables and the comparisons
```

`driver.sh` runs every condition (`prod`, `shared`, `calm`) and placement in turn, one `kiln-lab run` round
each, each round starting the turn one further on, so a machine's drift falls on every placement alike.
Run again, it measures only the rounds not measured yet; with `HOLD_FILE` set, each round waits while another
lab keeps that file fresh, so two labs never measure at once.

### The A/B (2026-11-04)

On fedora (an Intel Core i9-14900HX, 32 threads; Fedora's Linux 6.19 with lazy preemption), the lab and
everything it started held to the 16 efficient cores (`taskset -c 16-31`), where the server sized itself to 4
shards. One build, this goal at `8cbb752a586` (main at `b3a00b70cff` with goal 7), in release with the UI's
client built in; each placement set by `KILN_RT_PLACEMENT` alone. The `realistic` profile, six rounds of each
placement, the placements taking turns round by round (`scripts/placement_lab/driver.sh`). Other agents'
builds loaded the machine throughout (a load of 42 to 48 before each window, its CPU pressure 18% to 23%), so
the noise is wide; the turns spread it over every placement alike. The rounds, as `kiln-lab` wrote them:
`docs/runtime-lab/2026-11-04/goal7-prod-<placement>.json`.

**The deciding run: the production-like layout, 16 hogs at `cpu.weight` 1** (21:28 to 22:09 UTC). Each cell
is the median of the six rounds and its noise (the 95% interval of that median):

| number | round-robin | steer | steal | both |
| --- | --- | --- | --- | --- |
| page p50 ms | 0.217 (0.189 to 0.303) | 0.211 (0.156 to 0.349) | 0.224 (0.191 to 0.285) | 0.231 (0.200 to 0.396) |
| page p99 ms | 3.42 (2.75 to 7.15) | 2.75 (2.16 to 8.64) | 2.65 (1.95 to 4.14) | 2.27 (1.97 to 11.5) |
| page p99.9 ms | 32.2 (7.79 to 57.8) | 13.1 (3.47 to 42.2) | 19.7 (4.37 to 49.8) | 14.0 (3.15 to 53.9) |
| api_read p50 ms | 1.94 (1.79 to 2.02) | 2.14 (1.79 to 2.23) | 2.05 (1.97 to 2.33) | 2.16 (2.04 to 2.27) |
| api_read p99 ms | 165 (119 to 194) | 168 (160 to 195) | 179 (148 to 207) | 188 (186 to 196) |
| api_read p99.9 ms | 823 (723 to 1147) | 750 (623 to 1398) | 984 (748 to 1498) | 1112 (839 to 1365) |
| api_write p50 ms | 1.66 (1.50 to 1.89) | 1.56 (1.38 to 1.95) | 1.57 (1.31 to 1.74) | 1.58 (1.20 to 1.72) |
| api_write p99 ms | 593 (529 to 890) | 618 (526 to 925) | 626 (479 to 825) | 631 (550 to 878) |
| api_write p99.9 ms | 1025 (917 to 1463) | 800 (755 to 1387) | 1040 (774 to 1963) | 1359 (917 to 1695) |
| websocket p50 ms | 2.71 (2.58 to 2.77) | 2.59 (2.50 to 2.69) | 2.62 (2.58 to 2.75) | 2.63 (2.57 to 2.73) |
| websocket p99 ms | 1118 (1068 to 1300) | 1068 (952 to 1181) | 1080 (1035 to 1202) | 1021 (921 to 1140) |
| websocket p99.9 ms | 1442 (1331 to 1639) | 1304 (1205 to 1463) | 1352 (1288 to 1672) | 1314 (1235 to 1502) |
| attach p50 ms | 1334 (1250 to 1384) | 1254 (1156 to 1296) | 1281 (1203 to 1354) | 1312 (1235 to 1336) |
| attach p99 ms | 1940 (1465 to 2224) | 1841 (1416 to 2355) | 1543 (1466 to 2196) | 1743 (1677 to 1917) |
| throughput: API reads a second | 65.2 (64.2 to 66.5) | 65.7 (63.1 to 67.5) | 65.3 (64.6 to 67.2) | 65.7 (64.7 to 66.9) |
| throughput: live deliveries a second | 86.8 (85.7 to 87.5) | 87.1 (86.1 to 88.4) | 86.8 (85.4 to 87.7) | 86.3 (85.4 to 87.7) |
| server CPU ms a request | 13.1 (12.1 to 14.6) | 12.7 (11.2 to 13.5) | 13.0 (12.3 to 14.1) | 13.1 (12.2 to 13.4) |
| shards' delay p99 ms | 2.37 (2.08 to 2.57) | 2.23 (1.49 to 2.48) | 2.17 (1.89 to 2.74) | 2.18 (1.86 to 2.22) |
| server: page waited p99 ms | 0.058 (0.050 to 0.091) | 0.795 (0.504 to 1.28) | 0.166 (0.111 to 0.587) | 0.166 (0.114 to 0.959) |
| server: api_read waited p99 ms | 0.055 (0.050 to 0.091) | 0.823 (0.482 to 0.890) | 0.779 (0.512 to 0.838) | 0.517 (0.230 to 1.04) |
| server: api_write waited p99 ms | 0.060 (0.050 to 0.091) | 0.921 (0.761 to 0.995) | 0.717 (0.269 to 1.19) | 0.660 (0.220 to 0.986) |
| server: websocket waited p99 ms | 1514 (1342 to 1752) | 1297 (927 to 1738) | 1361 (1111 to 1562) | 1093 (875 to 1554) |
| server: runner waited p99 ms | 159 (84.0 to 214) | 97.6 (53.3 to 141) | 131 (69.4 to 163) | 128 (70.9 to 151) |

Pages (10 a second) and API writes (10.4 a second) ran at the same rate under every placement. Against
`round-robin` (`kiln-lab compare`, 101 numbers held together at 95%), each placement differs beyond noise in 3
numbers, the same three: the server's wait from a request's head to its handler's start at p99, for pages,
API reads and writes, worse, from 0.06 ms to between 0.17 and 0.92 ms: the hop each placement adds (a task on
another shard, or a movable task and its carrier). Nothing the lab's clients timed, no throughput and no CPU
per request differs beyond noise.

**By the rule, `round-robin` stays the default.** No placement's people's p99 is better beyond noise: `page`
p99 and `api_write` p99 went -19.7% and +4.2% under `steer`, -22.3% and +5.6% under `steal`, -33.7% and
+6.3% under `both`, each inside its noise (±91% to ±281%). The other placements stay behind the setting for
later A/B tests.

**The shared layout and calm, partial** (22:10 to 23:50 UTC; the same build, CPUs and profile). In `shared`
the server, its runners, the lab and 16 of kiln-lab's own hogs share the lab's cgroup at one weight; in `calm`
the server has its scope at `cpu.weight` 1000 and there are no hogs. The block waited from 22:28 to 23:11 while
another lab held the machine, and was stopped at 23:50, short of its six rounds, to give the machine back;
another agent's A/B of runner restarts ran beside it, unpinned, from 22:49. Fedora thrashed from 23:04 to 23:23
(a kiln CI job's unbounded release build: a load of 330 to 400, swap full), so the six rounds measured in it are
dropped, not run again: round 2 of calm under `round-robin` and of shared under every placement, and round 3 of
shared under `steal`. What's left: in shared, four rounds of `round-robin` and `steer` and three of `steal` and
`both`; in calm, three of `round-robin` and four of the others. Their rounds:
`docs/runtime-lab/2026-11-04/goal7-shared-<placement>.json` and `goal7-calm-<placement>.json`. Medians and
their noise, as above:

| number | round-robin | steer | steal | both |
| --- | --- | --- | --- | --- |
| shared: page p99 ms | 2.42 (1.90 to 3.34) | 2.48 (1.96 to 5.42) | 2.02 (1.96 to 10.2) | 1.94 (1.93 to 7.94) |
| shared: api_write p99 ms | 193 (65.1 to 348) | 366 (194 to 530) | 413 (364 to 3411) | 338 (188 to 543) |
| shared: api_read p99 ms | 63.6 (39.0 to 74.7) | 111 (93.4 to 147) | 74.0 (68.8 to 1279) | 85.6 (82.1 to 138) |
| shared: API reads a second | 72.1 (71.6 to 74.6) | 69.8 (67.7 to 70.5) | 69.9 (40.9 to 70.5) | 67.7 (67.0 to 71.2) |
| shared: live deliveries a second | 88.8 (88.0 to 90.4) | 88.6 (87.1 to 89.1) | 87.8 (75.6 to 88.4) | 87.3 (87.3 to 88.3) |
| shared: server CPU ms a request | 9.55 (9.10 to 12.0) | 9.49 (9.15 to 10.1) | 9.81 (9.27 to 10.6) | 9.60 (8.99 to 9.99) |
| shared: shards' delay p99 ms | 2.27 (1.77 to 7.15) | 2.29 (1.94 to 8.10) | 2.19 (2.18 to 6.33) | 2.26 (2.19 to 8.50) |
| calm: page p99 ms | 0.448 (0.354 to 1.30) | 0.413 (0.354 to 1.17) | 0.384 (0.350 to 8.37) | 1.09 (0.334 to 2.18) |
| calm: api_write p99 ms | 496 (492 to 574) | 600 (376 to 754) | 638 (334 to 1184) | 369 (335 to 2228) |
| calm: api_read p99 ms | 82.3 (80.7 to 176) | 85.5 (83.2 to 161) | 105 (82.1 to 135) | 85.3 (60.9 to 114) |
| calm: API reads a second | 68.4 (66.2 to 70.3) | 68.3 (66.9 to 69.1) | 67.4 (67.0 to 71.7) | 69.6 (64.2 to 71.8) |
| calm: live deliveries a second | 86.2 (86.1 to 86.6) | 86.2 (85.5 to 87.0) | 86.6 (83.7 to 88.0) | 86.5 (83.0 to 87.9) |
| calm: server CPU ms a request | 14.9 (12.0 to 15.3) | 12.4 (9.54 to 15.5) | 10.8 (9.54 to 14.0) | 11.7 (10.7 to 14.8) |
| calm: shards' delay p99 ms | 0.992 (0.646 to 1.41) | 0.721 (0.490 to 1.01) | 0.561 (0.491 to 2.30) | 0.671 (0.569 to 1.11) |

Against `round-robin` (`kiln-lab compare`, as above):

- **Shared:** no people's p99 is better beyond noise under any placement, and `steal`'s API writes' p99 is
  worse beyond it (413 ms against 193, +114%). The API reads' throughput is worse beyond noise under all three
  (-3.1% under `steer` and `steal`, -6.1% under `both`), and so is their p99 under `steer` (+75%) and `both`
  (+35%); with the server's wait before handlers, 5, 13 and 8 numbers differ in all. `both`'s rounds began at
  a load of 63 before each window against `round-robin`'s 27 (one as the thrash ended), so its differences are
  the least sure.
- **Calm:** no people's p99, no throughput and no CPU per request differs beyond noise under any placement.
  What does is the server's wait before handlers (`steal` one number, `both` three) and numbers of the fill
  (six under `steer`, one under `both`).

**The pick holds in both.** No placement qualifies by rule 3 in either, and in the shared layout `round-robin`
leads; rule 5 names `steal` there, worse beyond noise in API writes' p99. With fewer than six rounds, these say
less than the deciding run.

What it says:

- **On this load no placement beat today's.** The tails people see (pages' p99 of 2 to 3 ms, API writes'
  p99 of about 0.6 s) are not a shard's queue that placement can shorten: the shards ran their ready work
  2.2 to 2.4 ms late at p99 under every placement, and an API write's p99 is its handler's own time at the
  server (0.67 to 0.72 s), which waits on kiln.db's one connection (goal 3's `store` share, 0.5 s at p99
  here). The live feed's and runners' waits moved the most (a viewer's frames 1.51 s at p99 under
  `round-robin` against 1.09 to 1.36 s, runners' messages 159 ms against 98 to 131 ms), within noise.
- **What placing costs is a sub-millisecond hop** before each handler, the one difference beyond noise; it
  bought nothing measurable here.
- **Production's load may differ.** These rounds, as goal 8's, ran with the machine shared with other agents'
  builds; a quieter machine narrows the noise, and the setting lets the A/B be run again on one build
  (`scripts/placement_lab/driver.sh`).
- **Not built: per-shard `SO_REUSEPORT` listeners with a BPF picker** (step 1's option). Per-worker accept
  queues leave a clogged worker's connections waiting for it (research f3, f21), the picker would need the
  shards' delays in a BPF map, and the portable acceptor's two choices, the option's own aim, didn't win.

## Goal 9: shards in under a millisecond

Goal 9 of the API runtime program (plan `01a1a87b2cec70b8be87932f1cd55fc3`, research
`01a1a87b2cee763282534206fdb0c990`): every shard runs its ready work within a millisecond at p99.9,
calm and with CPU hogs in production's layout, at a tenth of goal 3's busy share (Ada: "sub
millisecond p99.9 on shard delay, at least an order of magnitude on busy share").

### Its lab (`scripts/shard_lab`)

`driver.sh` runs `kiln-lab`'s rounds in production's layout, the configurations taking turns round by
round, the lab's clients and runners on CPUs of their own (`LAB_CPUS`) so it measures the server, not
itself, and every server at 4 shards (`KILN_RT_SHARDS`, the count the server sizes itself to on 16
CPUs):

- `calm`: the server alone in a scope of its own at `CPUWeight=1000` (`kiln.slice`'s weight on bare
  metal) on `SERVER_CPUS`;
- `hogs`: that, and 16 busy loops in a scope of their own at `cpu.weight` 1 (CI legs' and agents'
  weight) on the server's CPUs;
- `cores-calm`, `cores`: the server on cores of its own (`[server] cores`, and its scope's
  `AllowedCPUs=`), calm and with the hogs on the CPUs past the server's;
- `q<µs>-<configuration>`: a configuration with the shards' step quantum at `<µs>`
  (`KILN_RT_QUANTUM_US`).

With `PROF=1` each configuration gets one round more with perf recording the server over its window
(`hook.sh`), kept apart from the measured rounds; every round's hook writes which CPUs each of the
server's threads and children may run on as its window ends (`affinity.txt`). `PERF_FROM=fill` starts
perf at the fill's start (sessions' Open), `PERF_FROM=server` at the server's, before its runners
connect (their Build and ReachHello); either way it stops as the window ends.

```sh
# the calm baseline below: the lab on 0-7 and the server on 16-23, beside another lab on the rest
ROUNDS=8 LAB_CPUS=0-7 SERVER_CPUS=16-23 CONFIGS=calm SHARDS=4 PROF=1 PROF_KILN=<an unstripped kiln> \
  PERF=<perf> scripts/shard_lab/driver.sh <kiln> <kiln-lab> ~/.cache/kiln-lab/g9-calm
```

### Main's baseline, calm (step 1)

Measured on 2026-11-05 from 01:12 to 01:26 UTC on fedora (Intel Core i9-14900HX, Fedora's Linux
6.19.10 with lazy preemption at 1000 Hz: research f17): the server on 8 efficient cores (16-23), the
lab on 8 performance threads (0-7), another lab on the rest. The build: main at `0ed7bc79afa` with this
goal's instruments (`5036242688e`, built with zig against glibc 2.43). Eight rounds; each cell the
median of the rounds and, after it, their range. The rounds: `docs/runtime-lab/2026-11-05/g9-calm.json`.

| number | all shards | shard 0 | shard 1 | shard 2 | shard 3 |
| --- | --- | --- | --- | --- | --- |
| delay p50 µs | 194.5 (193 to 196) | | | | |
| delay p99 µs | 615 (480 to 1087) | 1303 (847 to 3917) | 382 (324 to 864) | 337 (304 to 789) | 340 (323 to 824) |
| delay p99.9 µs | 20,110 (12,770 to 24,320) | 93,830 (89,230 to 127,100) | 1026 (544 to 2271) | 924 (527 to 2543) | 984 (727 to 2386) |
| delay, top bucket reached µs | 250,000 | 250,000 | 3500 (1250 to 10,000) | 2000 (1000 to 10,000) | 2500 (1250 to 10,000) |
| busy share | 0.071 (0.068 to 0.079) | 0.239 (0.232 to 0.259) | 0.0168 | 0.0157 | 0.0166 |

Over the fill the shards' delay was 21.6 ms at p99.9 and their busy share 0.166; the server used 1.09
cores, 12.4 ms of CPU a request.

What it says:

- **One shard made the tail.** Shard 0 ran every task the server's start-up spawned (a task spawned on a
  shard stays on it, and the start-up runs on shard 0): busy 0.24 of the time, against 0.016 on each of
  the others, and 94 ms late at p99.9, where the other three ran their ready work about 1 ms late.
- **A calm shard waits about 195 µs:** the probe's timer tick (100 µs) and its thread's timer slack (the
  tick again), by design.

### What held the shards (step 2)

Every poll past 100 µs, by its task's spawn site and the label its code set (`kiln_shard_held_*`), in
the calm window: the mean of the eight rounds a second, the mean hold and the longest seen.

| site | label | ms held a second | polls a second | mean hold ms | longest ms |
| --- | --- | --- | --- | --- | --- |
| `custody/feeds.rs:28` (custody's receipts of the feeds' batches) | | 105.9 | 313.9 | 0.337 | 14.1 |
| `hub.rs:2588` (runners' custody records, `custody::on_runner`) | | 32.6 | 98.6 | 0.331 | 7.0 |
| `convo/feed.rs:242` (the server's feed to its mirrors: its hand-ons) | | 30.9 | 101.9 | 0.304 | 4.9 |
| `sup.rs:136` (the hub's net loop, `spawn_guarded("net-events")`) | | 18.6 | 93.5 | 0.199 | 76.0 |
| `server.rs:471` (HTTP) | read /api/harness/sessions | 16.1 | 45.3 | 0.355 | 13.2 |
| `server.rs:471` | write /api/mcp | 6.2 | 8.0 | 0.774 | 18.1 |
| `harness/copies.rs:745` (sessions' owners, goal 4) | | 5.7 | 32.6 | 0.176 | 51.3 |
| `sup.rs:87` (the server's supervised children) | | 2.9 | 7.0 | 0.417 | 14.8 |
| `api.rs:5901` (a viewer's socket) | | 1.6 | 5.0 | 0.315 | 6.2 |
| `server.rs:471` | read /api/agents | 1.4 | 7.6 | 0.186 | 4.0 |
| `server.rs:471` | read /api/overview | 1.1 | 3.7 | 0.295 | 3.8 |
| `server.rs:286` (Error Tracking's issues, saved each minute) | | 0.35 | 0.02 | 21.0 | 29.3 |

Over the fill: the sessions' owners 588 ms a second (297 polls a second, 2.1 ms each, the longest 51
ms), the net loop 120 ms (283, 0.42 ms, 76 ms), custody's receipts 21 ms.

perf over a ninth round's window (an unstripped build of the same commit; its DWARF unwinding stopped
at the leaf on most samples, so by the function holding the IP): half of the shards' CPU was P-256
ECDSA (aws-lc's `nistz256` arithmetic and its modular inverse: custody's signatures, verified and made
on the shards), 15% glibc's malloc and free, 4% memmove and memset, 3.5% serde_json, 2% postcard
reading and writing `Vec<u8>` a byte at a time. Of the whole server's CPU, a third was deflate
(`miniz_oxide`) on the blocking pool: viewers' gzipped attaches.

### What moved (step 3)

- **Custody's signatures off the shards** (`custody::verified`, `load`, `server_signed_off`): a record
  is verified, and the feeds' receipts and hand-ons signed, on the compute pool; each in the order it
  was, nothing said before it's signed (W27-R08).
- **Services spread over the shards:** `sup::spawn` and `sup::spawn_guarded` place their tasks round-robin
  (`rt::spawn_spread_joined`), as do custody's receipts task and a feed's tasks.
- **Error Tracking's save on the blocking pool** (`observe::errors`).
- **Named by their callers:** `sup::spawn` and `sup::spawn_guarded` are `#[track_caller]`, so the table
  names the hub's net loop and each supervised child by where it was started.
- **Runners' frames read whole, and off the hub's shard when long:** each `Vec<u8>` of the wire's
  messages is written and read as bytes (`proto::wire_bytes`: the same bytes on the wire), and
  `wire::payload::read_frame` reads a frame and what it carries on the compute pool when that's expected
  to take past a slow poll (100 µs), by the moving averages of the frames read before; the hub's loop
  waits either way, so its messages keep their order.
- **The server's host sample on the blocking pool** (`Hub::sampler_loop`), as a runner's already was: it
  held its shard about 2 s every minute where sysinfo samples (found by the Bare Metal Migration agent).
- **The step quantum, from the measurements** (`rt::STEP_BUDGET_NS`: 266,666 ns, where it was a configured
  250 µs): the longest a poll may hold a shard, so that every shard runs its ready work within 1 ms at
  p99.9. A ready task waits:
  - its floor, 200 µs: its timer falls due on the wheel's next tick (100 µs), and the OS wakes the
    shard's thread within its timer slack (the tick again); the calm shards' median, 192 µs (g6c2 calm on
    rt5, 191 to 194 over six rounds), is that floor;
  - the polls it finds in hand or ahead of it: on a shard busy a share ρ of the time it finds n or more
    with a chance of about ρⁿ (a single server's queue, as what arrives finds it), and on the lab's
    busiest shard (ρ = 0.065, shard 1; the mean 0.052) ρ² is 0.42% and ρ³ 0.027%, so the 1-in-1000 wake
    finds two, each a quantum at most where no poll holds a shard past it (step 3's done-when);
  - its CPU where another thread holds it, about a quantum again: the slice each shard asks the OS for
    is its quantum (`hyper_rt::compute::prefer_current_thread`; beside four pool threads on one core a
    shard's p99 wake took 0.21 ms with a 250 µs slice).

  So the floor and three quanta come to the bar: (1 ms − 200 µs) / 3 = 266.7 µs, the largest quantum
  within it. It holds ordinary work's long polls: of a site the list doesn't name, the largest mean hold
  past 100 µs was 0.246 ms (`reads.rs:338`); the listed paths' held 0.15 to 0.26 ms on average, but for
  what's left to cut: `write /api/mcp` 0.92 ms, a supervised child (`server.rs:245`) 0.57, the owners'
  `Update::Meta` 0.40, custody's receipts 0.38 and records 0.37. With 250 µs the two shards ran 12.1 polls
  a second past twice the quantum (500 µs): six a shard, each 0.5 ms or more, so a shard was held so 0.3%
  of the time or more, past the 0.1% its p99.9 can spare, and its p99.9 was 1295 µs. Where the kernel's
  preemption is lazy (fedora's), a wake onto a busy CPU waits for its next tick instead, up to 1 ms
  (docs/priority.md, wake-2): step 4's to keep off, not the quantum's. As step 5 cuts the busy share the
  same rule gives a longer quantum: at 0.011 a wake finds one poll at p99.9, and 400 µs would do. The
  quantum also sets a cooperative task's share of its shard and its loops' turns (`rt::Turns`), how often
  a busy shard reads its inputs, the slow polls' threshold (twice it: 533 µs), the watchdog's stall (64
  quanta: 17.1 ms), the compute pool's pieces, the slice each shard asks for, and placement's margin.
  `KILN_RT_QUANTUM_US` still sets another for the lab's A/B of quanta. The derivation is
  `rt::tests::g9_s3_the_step_quantum_is_derived_from_the_lab`.
- **A loop's turn ends before the piece that would pass the quantum** (`rt::Turns`): by the longest piece
  it has done that turn, where it looked only for the quantum to have passed, so held its shard a piece
  past it.
- **A runner's Build in pieces, its mirrors' file off the shards** (`hub::build`): the hub's loop took a
  runner process's Build in one poll of ten things (its process, its legs' receipt, its upgrades, its
  front door, sync's, observe's and the cluster's announcements, the conversation store's mirrors and a
  restore, CI's word, its previews), and the mirrors' choice was written and synced there
  (`mirrors.json`): g9p6's fill held the HTTP listener's shard 5 to 34 ms with it, calm and with hogs
  alike (the disk's). Now each is a piece of its own, in the order they were, its process first (so its
  Build still comes before its first job), the shard given up between them once its turn would pass the
  quantum; and the kept files' writer writes the mirrors' file, synced, on the blocking pool
  (`keep::synced`). A ReachHello is an announcement (a copy swapped in whole, and a message to the relay's
  owner not waited for) and a line, whose write to stdout under its lock was its only wait: the log's
  writer's now (`log_lines`).
- **An Open going on from its copy keeps its chunks as they are** (`Mirror::keep_through`): a runner that
  connects again sends each session it holds an Open going on from the copy's newest, and the owner kept
  the events through it by reading every chunk of the copy again (inflating each, reading spilled ones
  back from disk, parsing each event's JSON) and making them anew, on its shard, to keep them all. Now the
  chunks before the cut stay as they are, the one it falls in is made again (read again only when it's
  compact), and the index lets go of the rest alone. g9p6's fill doesn't have it (its server is fresh);
  its Opens held up to 10.6 ms with hogs and 0.22 ms calm, a line written to stdout under its lock among
  their work, now the log's writer's.
- **g9p7's paths (train 7's tree), each moved or cut:**
  - `write /api/mcp` (6.9 ms the median round's longest, calm): `agents.list` copies every session's info and
    writes each one's brief on the compute pool, then `lineage::seen` keeps what its caller sees by id; a
    tool's text and the answer's JSON are written there too (`mcp::tool_ok`, `json_answer`).
  - The net loop's `Identity` (about 50 a fill, 0.2 ms each calm, to 3 ms with hogs): its certificate is made
    on the compute pool (the CSR's check, the CA's signature, its fingerprints: `identities::certify`), and
    a leg's delegation signed on the blocking pool (the server's key is read from its files for each).
  - The owners' `Update::Events` (6 to 20 ms the round's longest calm, where `Update::State` on the same
    shards held 1.3 to 4.8): a long copy letting go of its oldest (every thousand events) read the chunk
    its cut fell in back from its spill file, inflated and parsed it on the owner's shard. The chunks
    before the cut go at once now, and that one is made again on the blocking pool and taken in by its
    owner after (`Events::drain_whole`, `trim_first`).
  - The mirrors' feed (2.3 ms median calm): what the mirror has is left out of a batch in the compute job
    that writes it, not row by row on the shard; the feed takes its turns between what waits in its queue.
  - Custody: a waiting record's two loads share kiln's root and the revocations with their compute jobs,
    not copies made on the shard (a list growing by one with each certificate kiln supersedes).
  - The server's own tasks: push parses a `harness` frame's info on the compute pool (1 to 6 ms calm);
    the fleet's `builds.json` is written by the kept files' writer and read first on the blocking pool;
    observe's sender takes turns (a replica catching up was sent in one poll).

### Step 3's check

`kiln-lab`'s report says of each build whether step 3's done-when holds, "no poll past the new quantum
from the listed paths". It finds each path of step 2's list in the rounds' `kiln_shard_held_*`: by the
file of its task's spawn site (custody's receipts `custody/feeds.rs`, the hub's tasks `hub.rs`, the feed
to the mirrors `convo/feed.rs`, the owners `harness/copies.rs`, the server's own tasks `server.rs` and
`sup.rs`, a viewer's socket `api.rs`), the hub's net loop by the site its `runner …` labels name, and the
four requests by their routes at the site the requests' routes name. It holds the longest poll of each,
the window's and the fill's (`kiln_shard_held_longest_seconds` is the longest since the server started),
to the quantum the rounds' shards ran with (`kiln_shard_quantum_seconds`), round by round. Its first line
says met or not, and how many paths passed the shards' probes' longest poll too: a probe's own work is
tens of microseconds (`shard_health`), so that much of its hold was the host's, and on a shared machine
the host may hold any poll as long (g9p6 on fedora, at a load of 23 to 45: the probes' polls held up to
5.3 ms calm and 18.9 ms with hogs). Then a row a path, with where its longest poll was, the rounds past
(and past the probe's), its polls past 100 µs a second and the share of their time the host was found to
hold (preempted: step 4's, not the code's); then what else passed the quantum. Where the shards say their
time on the CPU (`kiln_shard_held_cpu_*`), the first line also says whether any listed path's poll ran
past the quantum on the CPU, kiln's code alone with the host's time left out, and each row its longest on
the CPU; and the tables of what held the shards say what share of their holds was on the CPU (kiln's) and
what share off it (the host's, or a call that blocked). Rounds of a build from before the gauge, or held
to another quantum:

```sh
kiln-lab report --quantum-us 266.666 <run>/run.json
```

## Goal 11: a tenth of the memory

Goal 11 of the API runtime program (plan `01a1a87b2e5673758106c92602a8b04d`, research
`01a1a87b2e577517ab4c81874f8700a2`): in the lab's realistic profile the server's resident memory is at
most a tenth of goal 3's (113 MB, from 1,132 MB), with no latency number worse.

### Where the memory goes (step 1)

fedora, 2026-11-05 12:59 to 13:24 UTC, the realistic profile, calm, 4 shards, the server on 8 CPUs of its
own (16-23) and the lab on 0-7: main before goal 11 (`1460599d430`) against train 4's tree (`4b429f1a05c`),
both the static musl build production runs, 8 interleaved rounds each; then one round each of the same
tree's glibc build, under jemalloc's heap profile and on glibc's own malloc with `malloc_info`
(`g11load.sh`, with step 3's scripts). Resident memory at the window's end, MB, the median of 8 rounds:

| | main | train 4 |
| --- | --- | --- |
| all (`server.rss_mb`) | 1,126 (1,028 to 1,141) | 526 (454 to 570) |
| the binary's pages (file) | | 127 (104 to 127) |
| anonymous | | 401 (328 to 443) |
| of it, named on `/metrics` | | 51: `session_segments` 30.0, `bus` 7.2, `session_entries` 6.3, `sqlite` 3.9, `ui_assets` 3.0, `custody_receipts` 0.4, `ui_events` 0.2 |
| of it, not named | | 351 (277 to 395) |

What's live, by jemalloc's heap profile at the window's end: 165 MB (its estimate from samples), by owner:

| owner | MB | the most of it |
| --- | --- | --- |
| sessions' copies | 45.5 | packed segments 29.7 (`session_segments`), the registry's entries and mirrors 12.6, compaction 3.2 |
| the runtime | 27.7 | each shard's context 16.4, and its interests, timer wheel and queues |
| other | 18.5 | hyper_log's device and its buffers 16.4 |
| the network (QUIC, the net thread) | 15.2 | the send budget's reserve 8.3, QUIC's transmits 5.2 |
| observe | 11.9 | its index 4.0, its owner 3.2, its store 2.1 |
| kiln.db (SQLite and its group) | 11.7 | SQLite's page cache 4.7, the group's driver 2.6, the capture's tables 2.1 |
| the bus (live feeds' frames) | 10.1 | Quiver's frames 6.8 |
| the conversation store | 9.0 | its feed's decoding 5.2, custody's intake 2.6, turso 1.1 |
| snapshots in flight | 6.3 | packed for viewers 3.3, being sent 2.4 |
| UI assets | 5.4 | gzip 3.8, brotli 1.6 |
| HTTP | 2.3 | websockets' buffers 1.8 |
| queues made up front; runners, CI and CD | 1.0 | |

The rest of the anonymous memory is the allocator's: on glibc's malloc, 88 arenas (one for nearly each of
the server's 88 threads) had taken 438 MB from the system with 297 MB of it free; under jemalloc, 261 MB
resident held the 165 MB live. On musl's malloc, production's until step 4, the same code's 401 MB holds about
the same live heap, so about 235 MB is what the allocator keeps. Against the bar: the live heap alone (165
MB) is past 113 MB, and the binary's own resident pages (127 MB) are past it too, so step 5 needs the
allocator's (step 4), less held live, and a word on whether file pages count.

### Every cache and buffer, bounded (step 3)

Each holds at most a stated number of bytes, from a measurement or a stated derivation, and says what
happens at its bound: a sender waits, or what's kept is let go where it can be had again. On `/metrics`:
`kiln_memory_bytes{owner}` (what it holds now) and `kiln_memory_bound_bytes{owner}` (its bound, where it's
a number; an owner of many queues or rings shows their sum). Queues are `crate::memory::queue`'s (made as
they fill, items counted by what they carry); fan-out rings are `crate::memory::ring`'s (async-broadcast's
queue, each message counted by its bytes until the ring lets it go, the oldest let go past the bytes as
past the count).

| Owner | What it holds | Bound | Basis | At the bound |
| --- | --- | --- | --- | --- |
| `net_out` | commands to the net thread (frames to runners) | 64 MiB | a connection's receive window (`net::WINDOW`, RFC 9000 §4.1): the most a peer takes unread | the sender waits (`try_send` is told it's full) |
| `net_events` | runners' frames waiting for the hub | 64 MiB | the same window, at which the net thread holds its peers' lanes | the net thread holds the rest, then its lanes: QUIC holds the runners |
| `session_inboxes` | messages to each session copy's owner, an inbox a shard | 64 MiB each | the hub's queue they're handed from | the hub's loop waits, then the net thread and QUIC |
| `local_updates` | the server's own sessions' updates | 64 MiB | the inbox they go to next | the session waits |
| `records_queue` | requests to a records file's thread (Turso) | 16 MiB a file | what the thread writes in one commit (`BATCH_BYTES`): the next batch gathers while one commits | the caller waits |
| `feed_intake` | runners' conversation-feed parts waiting for the intake | 64 MiB | the hub's queue it's handed from | dropped as before: the runner holds the batch until acknowledged and sends it again |
| `mirrors_out` | the conversation store's parts going to its mirrors | 64 MiB | the net thread's queue it feeds | the mirror's feed waits |
| `wire_checks` | runners' payloads waiting for the unknown-fields check | 64 MiB | the hub's queue of runner events | let go unlooked at, counted and said (the message was taken whole) |
| `custody_receipts` | receipts of runners' feed batches waiting to be signed | 64 MiB | the intake's queue the hand-ons came in | let go and logged; its hand-on goes unanswered |
| `kept_files` | finished messages' files waiting to be kept | 64 MiB | an owner's inbox, where the messages came from | not kept now: read when someone looks |
| `sqlite` | kiln.db's page caches (writer and readers) | the sum of their `cache_size`s with each page's header, a temp database's again: about 4.3 MB a connection | SQLite's default `cache_size` (-2000 KiB, kiln sets none); a page's header measured at 256 B | SQLite lets the group's least recently used page go, read again from the file |
| `convo_queue` | rows the conversation store's writer hasn't committed | what the writer commits in `COMMIT_MS` at its measured rate | its "durable within `COMMIT_MS`" | senders wait |
| `ui_assets` | the UI's files compressed (brotli, gzip) | what this build's files take compressed both ways | known as the server makes them at start | none: a fixed set |
| `session_entries` | session copies' events as their JSON's text | each copy's `MAX_EVENTS`, its newest chunk alone as text, for each session | the copy's own bound | older chunks are kept packed |
| `snapshots` | viewers' snapshots in flight: made, and not yet written to their sockets (each taken by its estimate before it's made, then counted as it was made, given back as it's written) | the blocking pool's `fs` places of whole snapshots: 26 x 3.2 MB, 84 MB on the server | the places from the lab's measure of the share's jobs at once (`rt::SERVER_FS`, 25.5); a whole snapshot's events, as many as a copy keeps before it lets its oldest go (`MAX_EVENTS` and a quarter, 5,000) at 647 B packed each (research f20) | an attach waits its turn (a task's wait) until one in flight is written out; one larger than the room goes alone |
| `ui_events` | the hub's events (`GET /api/events`, kiln's listeners) on one lane, and agent sessions' own on each owner shard's lane, to each listener's own channels (goal 4, `memory::lanes`) | each listener's places (4,096 events in all its lanes; push's sessions' 4,096 a lane) and 64 MiB of the texts (each counted once, as held) | the hub's queue of runners' messages most of them are made from | the oldest go: a page resyncs, a listener is told how many it missed |
| `log_feeds` | each live job's log on its way to its viewers | 1,024 pieces and 32 MiB a job | one batch of its writer (`FEED` pieces) of the largest chunks a runner sends (`proto::LOG_CHUNK`, 32 KiB) | the oldest go: the viewer is told what it skipped; the log file has it |
| `terminal_feeds` | each terminal's output on its way to its viewers | 1,024 frames and 256 KiB a terminal | `BACKLOG_BYTES`, the backlog a viewer that falls behind is repainted from | the oldest go: the viewer is repainted from the backlog and screen |
| `stream_viewers` | each viewer's queue of a browser stream | 12.5 MB a viewer | 240 pictures at 60 a second (4 s) at the encoder's top rate (25 Mb/s, `bitrateFor`) | the chunk isn't sent to that viewer: its next key frame mends it |
| `webhook_queues` | outgoing webhooks' deliveries waiting for their subscriber | 1,000 events and 64 MiB a subscriber | the hub's events ring they're taken from | logged as dropped, the body kept in its row to send again |
| `syslog_waiting` | syslog records not yet offered to the observe node | 64 MiB a source (was 16,384 records of up to 256 KiB) | the node's waiting log bytes (`WAITING_LOG_BYTES`), where they go next | a datagram is dropped and counted; a TCP sender waits |
| `ci_inbox` | messages waiting for the CI engine's dispatcher | 64 MiB | the hub's queue of runners' messages they're made from, one a message | the hub's loop waits (then the net thread and QUIC); a person's request never waits, counted all the same |
| `keep_waiting` | small files kept whole, waiting for their writer | each file's newest bytes, one copy | a file's earlier bytes are let go as newer come | - |
| `ship_cache` | Ship's rows: each repository's newest commits drawn | 5,000 rows a repository, each as large as its largest held (measured as drawn) | `CAP`; older pages come from the store | the oldest rows go: the store answers their pages |
| `ci_graphs` | package graphs kept for `affected()` | 64 graphs and 64 MiB | `MAX_BYTES`, the most one commit's manifests are read from | the oldest go: read again from the mirror |
| `pull_lists` | hosts' open pull requests for the inbox | a minute each, and 32 MiB | one host response as kiln reads it (GitHub's, up to 32 MiB) | the lists read longest ago go: read from the host again |
| `search_indexes` | kiln-search's open indexes' tables | 8 indexes, each as large as the largest opened (measured as opened) | `OPEN_MAX` | the one searched least recently goes: opened again from its files |
| `grok_dfa` | observe's grok rules compiled | each rule's lazy-DFA caches at regex's capacity (2 x 2 MiB) for each thread searching it at once | regex's default `dfa_size_limit`; it reports no figure of use, so this counts the rules compiled at one thread's capacity | regex resets a full cache |
| `cd_inputs` | what the deployments engine is told and hasn't taken: clusters' lists, watch events and apply answers, renders, people's and the source poll's word, each object as serde_json keeps it | 256 MiB | one answer of a cluster's API server as kiln reads it at the most (`cd::kube::BODY_LIMIT`), which most of its inputs are | the sender waits as a task: a watch stops reading its stream (the API server holds what follows), a render or answer waits to be handed in, a request waits its turn; the poll's and a rollback's inputs wait for room before the gate that keeps them in order, so none is past it |
| `cd_actions` | what the deployments engine asked for and its dispatcher hasn't taken: applies (each its object), deletes, renders, watches, statuses, records | 256 MiB | as much as its inputs, which what it asks for is made of (an apply's object is a render's) | the engine's thread waits (its own, not the runtime's), and its inputs behind it |
| `log_lines` | kiln's log lines waiting for the thread that writes them (`log_lines`; goal 9 step 3: a server's and a runner's) | 10,000 lines and 64 MiB | journald's burst (`RateLimitBurst`, journald.conf(5): past it journald drops a service's lines itself), and the bytes observe keeps waiting of a source's log (`WAITING_LOG_BYTES`) | let go, counted (`kiln_log_lines_dropped_total`), and said as the thread next writes: a shard that logs never waits for the log's reader |
| `bus` | the bus's retained messages (`kiln_bus_memory_bytes`), each topic's as its retention asks (`kiln_bus_topic_bytes_peak`, the largest since start) | what its topics and subscriptions can hold (`QuiverBus::bound`: about 6.6 MB a feed), at most `[bus] memory`, 171 MiB; a topic `[bus] topic_bytes`, 5 MiB | the bus lab (below): its most for each topic held, 1,327,754 B, for production's 134 sessions every one watched; its largest topic, 3,672,418 B; each with room for one more of the largest message | a topic lets its oldest go; one it can't make room for is refused and its viewers start over |
| `session_segments` | sealed chunks' segments kept in memory: each copy's among its newest 100 events (`copies::EVENTS_PAGE`, the events API's page without a `limit`), and those on their way to its spill | `copies::PAGE_BYTES` for each copy: 2 x 70 x 647 B, 90,580 B (~91 KB) | two chunks (`CHUNK`, 70 events) at the most hold the newest page beside the open chunk; 647 B an event packed (research f20) | the rest are spilled to the copy's spill file (`harness::spill`, below): a snapshot reads them back on the blocking pool, the events API a spilled range from the store; past the bytes, the newest page's chunks spill too |
| (on disk) `kiln_session_spill_bytes` | the copies' spilled segments: `live` (still referred to) and `file` (the spill files whole) | `file` at most `live` and as much again | a copy starts a new file once its file's dead bytes (chunks let go of) pass its live ones; a file goes once no chunk refers to it | a write the disk refuses leaves the chunks in memory (`kiln_session_spill_failures_total{op="write"}`); one that doesn't read back as written is refused (`op="read"`), a snapshot then reading those events from the conversation store |

Channels bounded by their senders (each sender waits for its answer, so a channel holds at most one call
for each waiting task, which would hold it anyway): the store's writer (`Store::write`, `Store::now`), the
wire commands' owner (`Owner::ask`; `tell`'s are a command each for a session told at once, each at most
`ITEM_MOST`). Sync's two channels carry its groups' raft messages, each group in flight within the limits
hyper-raft derives from the 64 MiB it states (`sync::group::settings`), each message at most
`sync::group::MESSAGE`. The deploys' change ring (`Deploy::changes`) holds 256 numbers, a fixed 6 KiB.
The deployments engine's queries (`Cd::query`) are bounded by their senders too, each waiting for its
answer, and wait for room in its inbox besides. Its other channels carry a person's or a check's word:
a rollout's driver's inbox (an approval, a retry), a self-deploy guard's measurements (one a check as it
comes due), and the store's changes (an app's or setting's name each).

The deployments engine's inbox (`cd_inputs`) is std's channel, made as it fills, which the engine's
thread waits on with a timeout for its timers, with a `crate::memory::queue::Room` for its bytes; its
about forty senders are all tasks now and each waits for room (`Cd::input`, `Cd::register`,
`Cd::unregister`, `Cd::query`). The source poll's inputs and a rollback's, which are sent under the gate
that keeps them in order (`inputs_gate`, a lock no one holds across a wait), take their room first
(`Cd::reserve_input`) and send under the gate without waiting, giving the room back if they don't send.
Its actions queue (`cd_actions`) is a `crate::memory::queue`, which the engine's thread waits on when it's
full; the dispatcher that takes from it never waits on the inbox (what answers an action is a task of its
own) and starts before the apps kept are registered, so the engine always goes on.

### Measuring the bus's bound

The bus's `[bus] memory` (256 MiB until goal 11) and `topic_bytes` (16 MiB) had no measurement behind them.

**How it's measured.**
- A lab run samples the bus each second of every round through a `KILN_LAB_HOOK`: its bytes, limit, topics,
  frames, refusals and lag notices, its largest topic, every memory owner, and the resident memory by kind.
- It runs 8 rounds calm and 8 with hogs, in production's layout. Each round's server starts with
  `--bus-memory` 1 GiB: twice what the realistic profile's 32 sessions' topics could keep at 16 MiB each,
  the rest for Quiver's framing and the segments readers still hold. So no round reaches the limit, and the
  peaks are what the load needs.
- Each topic's bytes come from its feed (`crate::harness::live`). As it publishes, the feed counts its topic's
  newest 2048 frames by their keys and payloads, as `topic_bytes` counts them, whether or not `topic_bytes`
  lets the bus keep them all, and raises `kiln_bus_topic_bytes_peak`. Kiln can't ask Quiver for its own count
  of a topic.

**The run.** bus6 ran on fedora on 2026-11-05, of rt6's tree (`705fb481bfb`), with the realistic profile.
No round refused a frame or sent a lag notice.

| | calm | hogs |
| --- | --- | --- |
| the bus's peak, MB (window) | 9.29 to 11.94 | 9.26 to 12.16 |
| its largest topic, MB | 2.61 to 3.66 | 2.55 to 3.67 |
| topics at the most | 10 to 11 | 10 to 11 |
| frames a round | 9,781 to 10,201 | 9,988 to 11,238 |

- The busiest second held 12,157,694 B for 11 topics.
- The most it held for each topic it held, at any second, was 1,327,754 B: 10,622,030 B for 8 topics, in the
  hogs round 2.
- The largest topic asked for 3,672,418 B.

**What it gives** (`crate::bus`, derived there from these figures):
- `[bus] topic_bytes` is 5 MiB. That's the largest topic, with room for one more of the largest message a
  topic takes (1 MiB of payload), never below that message (Quiver refuses a message larger than
  `topic_bytes`), and rounded up to a MiB.
- `[bus] memory` is 171 MiB (179,306,496 B). That's the most for each topic held, times production's 134
  sessions every one watched, with room for one more of Quiver's largest messages (1,083,392 B), never below
  the floor (2,166,784 B), and rounded up to a MiB.
  - The lab's own peak with that message, 13 MiB, holds its 11 topics. Those are the realistic profile's 32
    sessions at a busy hour, not production's 134.
  - Quiver fixes a node's memory when it's made, so the limit can't follow the topics a server holds. Rather,
    what the bus can hold follows them: each topic is held to `topic_bytes`.
  - `/metrics` gives that as `kiln_memory_bound_bytes{owner="bus"}`, capped at the limit. For each topic it's
    the topic's kept messages and their framing, its oldest segment let go, and a message being taken: about
    6.6 MB at the defaults. For each subscription it's a segment and a message. A server watching few sessions
    is bound by those.

The scripts are under `scripts/memory_lab/`: `bus-lab.sh` runs the rounds, `bus-hook.sh` samples them, and
`bus-summary.py` gives the table and these bounds. The summary's `TOPICS` (134 by default) is the topics
`[bus] memory` is reckoned for.

### The allocator (step 4)

fedora, 2026-11-05, the realistic profile, calm, 4 shards, the server on CPUs 16-23 and the lab on 0-7,
in A/Bs of interleaved rounds (`g11alloc.sh`):
- 13:24 to 14:45 UTC: train 4's tree (`4b429f1a05c`), its glibc build and its static musl build
  (production's then), and glibc's arenas.
- 17:18 to 18:01 UTC (g11alloc2): jemalloc with the settings below, in two ways:
  - preloaded into that glibc build: Fedora's jemalloc 5.3.0 (`/usr/lib64/libjemalloc.so.2`), the
    settings as `MALLOC_CONF`;
  - compiled into rt-next `313b1774496`'s static musl build (kiln's `jemalloc` feature), against the same
    build without it. That tree predates `os::alloc`, so the lab passed the settings as `_RJEM_MALLOC_CONF`,
    the string `os::alloc` now compiles in.

Resident memory at the window's end, MB, the median of rounds; and what moved beyond noise, B against A, of
the numbers kiln-lab compares (203; 222 on rt-next's tree):

| A against B | rounds | resident, A to B | anonymous | the binary's pages | latency beyond noise |
| --- | --- | --- | --- | --- | --- |
| glibc against musl | 8, 8 | 576 to 541 (within noise) | 449 to 415 | 128 to 127 | worse on musl: attach p50 3.5 to 4.7 ms; the server's wait p99 for API reads, writes and pages 0.10 to 0.25-0.28 ms |
| glibc against `MALLOC_ARENA_MAX=2` | 8, 8 | 545 to 420 (-23%) | 419 to 293 | 128 to 129 | worse: bus delivery p99 0.25 to 0.50 ms (p99.9 0.74 to 1.69); websocket frames' server wait p99 0.25 to 0.50 ms; runners' messages took p99 0.21 to 0.33 ms (+56%); slow polls +42% |
| glibc against `MALLOC_ARENA_MAX=5` | 7, 8 | 523 to 459 (-12%) | 396 to 333 | 129 to 128 | none |
| glibc against jemalloc preloaded | 8, 8 | 530 to 338 (-36%) | 403 to 212 (-47%) | 128 to 129 | worse: attach p50 3.3 to 4.4 ms (+33%, noise ±24%). Better: API writes' p50 1.0 to 0.7 ms |
| musl against musl-jemalloc (kiln's `jemalloc` feature, `os::alloc`'s settings) | 8, 8 | 523 to 333 (-36%) | 398 to 207 (-48%) | 126 to 128 (+1.6%, worse beyond noise: jemalloc's own code) | none worse. Better: the server's wait p99.9 for API reads 1.16 to 0.25 ms and for pages 1.22 to 0.25 ms; compute's slowest piece 100 to 50 ms |

glibc's side of the `MALLOC_ARENA_MAX=5` A/B lost its last round to a full disk.

In the musl-jemalloc A/B, these medians moved within noise, musl to musl-jemalloc:
- attach p50 8.1 to 5.4 ms;
- API writes' p50 1.4 to 1.0 ms;
- API reads' p99.9 2.3 to 2.1 ms;
- pages' p99.9 1.8 to 1.3 ms;
- the bus delivery's p99 0.25 to 0.27 ms, and runners' messages took p99 0.16 to 0.20 ms (glibc's 2 arenas
  moved both beyond noise);
- the shards' delays, and slow polls 11.5 to 10.0 a second.

Each jemalloc server ran 92 threads to the other side's 88. The extra 4 are jemalloc's background threads,
which start only when `background_thread:true` is set, so the settings reached jemalloc. The preloaded A/B
ran train 4's tree. In rt-next's tree, the musl pair's, a packed snapshot goes out a piece at a time, each
made on the blocking pool (step 5).

Before these:
- One round of the glibc build under jemalloc preloaded, its heap profile on, held 261 MB anonymous for
  165 MB live (step 1).
- mimalloc, tried before, held 22.7% more resident memory, and idle 3.4 times glibc's anonymous memory
  (183 against 54 MB, research f18).

glibc's default is eight arenas for every core: in the lab, 88, one for nearly each of the server's
threads, each keeping what its threads freed (438 MB taken, 297 MB of it free). musl's malloc held about
the same as glibc's. So:

- **glibc builds** (`*-linux-gnu`, built from source): at most 5 arenas, the shards' 4 and glibc's main
  one, by `mallopt(M_ARENA_MAX)` as the process starts (`kiln::os::alloc::init`, `main`'s first call),
  unless `MALLOC_ARENA_MAX` or `GLIBC_TUNABLES` says how many (so a lab can still A/B them). 5 held
  every latency number with 12% less resident memory. 2 took 23% off but doubled the bus delivery's p99,
  so it isn't 2. jemalloc preloaded held 36% less than glibc's default arenas, but its attach p50 was worse
  beyond noise, so a glibc build keeps glibc's malloc.
- **Production's static musl build: jemalloc**, the pick (below). `ci/release/build.sh`'s one switch,
  `KILN_LINUX_ALLOCATOR` (`jemalloc` or `musl`), defaults to `jemalloc`. So the image and releases
  (`.kiln/image.yaml`, `.kiln/release.yaml`, `.github/workflows/release.yml`) build with kiln's `jemalloc`
  feature, and `KILN_LINUX_ALLOCATOR=musl` builds one with musl's malloc. Both Linux targets build with it
  under zig's cc (zig 0.17.0 and cargo-zigbuild 0.23.4, as `build.sh` pins them): jemalloc's configure
  finds background threads, `MADV_DONTNEED` and `MADV_FREE`, and the binaries stay static. A cross build can't ask its target's page size and takes
  4 KiB, and jemalloc refuses to start under a larger one ("Unsupported system page size"). arm64 Linux
  runs 4, 16 or 64 KiB pages (16 on Apple silicon and the Raspberry Pi 5's kernel), so its jemalloc takes
  64 KiB (tikv-jemalloc-sys's `JEMALLOC_SYS_WITH_LG_PAGE=16`, for that target alone).

jemalloc's settings live in `kiln::os::alloc`, its `malloc_conf`. `_RJEM_MALLOC_CONF` in the environment
overrides them a setting at a time, so a lab can A/B them on one build. Unit tests hold the string to the
settings and to what the lab ran. With the feature, jemalloc reads each back, and takes a variant from the
environment over them:

| Setting | Value | Basis |
| --- | --- | --- |
| `background_thread` | true | Without them jemalloc purges an arena only as that arena allocates and frees, so what a quiet server holds after a burst stays. A purge's `madvise` then runs on jemalloc's own thread, never inline on a shard that freed (jemalloc's TUNING.md). On the static musl build (adacachy, a check program), a burst of 64 MiB freed and left alone went from 54 MB resident to 0.9 MB within 9.5 s; with jemalloc's defaults it stayed at 54 MB. A quiet server's burst goes within about twice the decay: having found nothing to purge, the background thread sleeps the decay's length before it looks again |
| `narenas` | 5 | The shards' 4 and one more: the count glibc held every latency number with (above). jemalloc's default is four for every CPU (32 in the lab, 128 on a 32-CPU machine), each keeping what its threads freed. jemalloc takes an arena's locks only past a thread's own cache (up to 200 of a small size, against glibc's 7) |
| `dirty_decay_ms` | 5000 | jemalloc keeps about what was freed in the last half of it (its decay curve is symmetric about the middle): 2.5 s, as often as the lab's work recurs (an attach every 2.7 s, an agent's turn about every 3 s), so that work reuses its pages rather than faulting new ones in. What's idle longer goes within the 5 s of the lab's warm-up after the fill. Half jemalloc's default (10 s), as its TUNING.md suggests where memory comes first |
| `muzzy_decay_ms` | 0 | A page given back with `MADV_FREE` stays resident until the kernel wants memory (smaps' `LazyFree`), so resident memory doesn't count it as given back. At 0, what decays goes with `MADV_DONTNEED` at once. jemalloc 5.3's code has 0 too, though its manual says 10 s |
| `thp` | never | fedora gives transparent huge pages only where asked (`madvise`), so the lab measures jemalloc without them. adacachy gives them to every anonymous mapping (`always`), and its khugepaged makes a 2 MiB range with 103 of its 512 pages in use one huge page again (`max_ptes_none` 409), filling in what purges gave back. With `never` (`MADV_NOHUGEPAGE` on jemalloc's mappings), production's heap runs as the lab measured it. The check program's glibc build on adacachy started at 6.3 MB anonymous with jemalloc's default (6 MB of it huge pages) and 0.5 MB with `never`. Its static musl build got huge pages only some runs: a fault takes one only when a free one is at hand |

**The pick: jemalloc, with these settings, for production's static musl build.** The musl-jemalloc A/B
decided it. On the build production runs, against musl's malloc:
- resident memory fell from 523 to 333 MB (-36%), and anonymous memory from 398 to 207 MB (-48%), both
  beyond noise;
- no latency number was worse beyond noise, and three were better.

So the switch's default is `jemalloc`. The one number worse beyond noise is the binary's own resident pages,
126 to 128 MB (+1.6%). That's jemalloc's code: the binary is 3.1 MB larger. Variants are an environment
away on the same build, for example `--env-b _RJEM_MALLOC_CONF=narenas:8` or `dirty_decay_ms:1000`.

## Goal 10: API reads and frames in under a millisecond

Goal 10 of the API runtime program (plan `01a1a87b2dae`): every read the lab makes answers from memory,
encoded ahead; frames never wait behind another viewer's snapshot; a runner's session update goes
straight to its session's owner shard.

### Main's baseline (step 1)

Measured on 2026-11-05 from 09:26 to 10:43 UTC on fedora, as the calm and hogs configurations of goal 8
step 7's lab (`scripts/priority_lab/s7c.sh`, below): production's layout, the server on 16-31 at
`CPUWeight=1000`, 16 spinning hogs at `cpu.weight` 1 beside it in `hogs`, the lab on 0-15, 4 shards, 8
rounds each. The build: rt-int `d46ceb48d67`, whose tree is main's since `3e427eae99b` (zig, glibc 2.43).
Each cell the median of the rounds, in microseconds.

| number | calm | hogs |
| --- | --- | --- |
| API read p50 / p99 / p99.9 | 234 / 970 / 1244 | 437 / 1833 / 3804 |
| frame p50 / p99 / p99.9 (all) | 703 / 2025 / 16,443 | 1713 / 5657 / 39,909 |
| frame p99, live (not a viewer's own attach) | 1891 to 2169 | 4680 to 5625 |
| page p50 / p99 | 207 / 407 | 392 / 1453 |
| runner's wait for its owner p99 | 6595 | 14,526 |

A frame published while another viewer attaches reached its viewer in 0.39 to 4.7 ms in every round
that had one (29 of them, across all of the lab's configurations), at or under that round's live p99,
while the attaches took 6 ms to 1.7 s at the median: frames don't wait behind a snapshot (step 3).
The frames' p99.9 is a viewer's own attach (its snapshot), counted with the rest.

## Goal 8 step 7 and goal 9 step 4: the server's cores

`[server] cores` gives the server cores of its own; kiln then keeps its own legs and agents off them
(Linux affinity, the CPU ledger, Docker's cpusets), and `[server] realtime` gives the shards Linux's
round-robin class at its lowest priority where the server's unit grants it (docs/priority.md).
`scripts/priority_lab/s7c.sh` ran 8 rounds of each configuration, interleaved, on fedora on
2026-11-05 (09:26 to 10:43 UTC; the build as above):

- `calm`: the server on 16-31, nothing else of the lab's there;
- `hogs`: and 16 spinning hogs on 16-31 at weight 1;
- `cores-calm`: the server on its cores, 16-23;
- `cores`: and the hogs on 24-31;
- `fence`: the hogs started as a kiln runner's agent, which kiln itself put on 24-31.

| number (µs) | calm | hogs | cores-calm | cores | fence |
| --- | --- | --- | --- | --- | --- |
| page p99 | 407 | 1453 | 1028 | 1157 | 1108 |
| API read p99 | 970 | 1833 | 1151 | 1274 | 1312 |
| API write p99 | 446,456 | 406,874 | 434,740 | 434,803 | 425,894 |
| frame p99 | 2025 | 5657 | 2798 | 3665 | 3500 |
| shards' delay p99 | 338 | 1139 | 1056 | 1188 | 1178 |
| shards' delay p99.9 | 1115 | 3284 | 1558 | 1964 | 1971 |
| shards' busy share | 0.037 | 0.037 | 0.035 | 0.037 | 0.037 |

What it says:

- **Hogs on the server's CPUs cost its tail** (calm against hogs, beyond noise): pages 0.41 to 1.45 ms
  at p99, API reads 0.97 to 1.83 ms, frames 2.0 to 5.7 ms, the shards' delay 1.1 to 3.3 ms at p99.9.
- **Kept off its cores, they cost little more** (cores-calm against cores): pages, API writes and
  frames the same within noise; API reads 1.15 to 1.27 ms at p99, 10.7% worse, just past the noise.
  kiln's own placement does what `taskset` does (cores against fence: every number the same), and
  the fence's verdict each round was that none of the hog agent's threads may run on 16-23.
- **The rest is fedora's**: with 8 cores instead of 16, and other sessions' work on those 8 (fedora
  has no cpuset partition, and its kernel preempts lazily at 1000 Hz, so a wake onto a busy CPU waits
  up to a tick: docs/priority.md, wakelab wake-2), every configuration with the server on its cores
  ran its shards about a millisecond late at p99, and none made goal 9 step 4's bar (p99.9 under 1
  ms with hogs). Production's machine preempts fully and its installer can make the cores a
  partition; on fedora that needs root, as does the real-time class (`RLIMIT_RTPRIO` 0 there), so
  neither ran here.

### The bare-metal default (goal 9 step 4)

What `kiln install` gives the server on Linux with systemd, and why (docs/priority.md, "What kiln install
grants" and "The bare-metal default"):

- **The shards in the real-time class wherever the server's unit grants it**, declared in a new server's
  config (`[server] realtime = true`, with a line saying why) and given room by its unit (`LimitRTPRIO=1`):
  every system install (the server runs as root), and a user's install whose service manager allows
  `RLIMIT_RTPRIO` 1 or more. Elsewhere it stays off, and the install says what it would take. Once its shards
  have asked, the server gives up the room, so nothing it starts inherits it.
- **Cores of its own stay declared** (`[server] cores`); declared on a system install, they're a cpuset
  partition as the server starts (given back without them, and by `kiln uninstall`), the server says whether
  the partition took (its log; `/healthz/server`'s `priority.cores`), and its own runner's legs and agents run
  off the shards' cores.
- **macOS and Windows keep what they have** (launchd's `Interactive` job and background policy; above and below
  normal priority classes), and say so.
- **Its basis:** the table above (weights alone 3.28 ms at p99.9 with hogs, cores by affinity 1.96, neither
  under 1 ms on fedora's lazy kernel), the kernel's rules (a real-time wake preempts at once under every
  preemption mode; a normal one waits for a lazy kernel's tick, wake-2's 1.00 ms), and what the installer can
  grant. Not yet a lab A/B of the class itself: the one below decides it, and if the class doesn't beat weights
  alone there, the default goes back to off (`realtime_default`, `crates/kiln/src/install.rs`).

What the lab's machines grant (read 2026-11-05 ~15:20 UTC):

| | fedora (the lab) | adacachy (production) |
| --- | --- | --- |
| kernel, preemption | 6.19.10, lazy at 1000 Hz | 7.2.9, full at 1000 Hz |
| real-time group scheduling | none | none |
| the user's service manager's `RLIMIT_RTPRIO` | 0, soft and hard | 99, soft and hard (`limits.d`, group `audio`) |
| cpuset delegated to that manager | yes (`DelegateControllers=cpu cpuset io memory pids`) | yes (a `delegate.conf` drop-in) |
| the production server's own `RLIMIT_RTPRIO` | | 0, soft and hard: a system unit with `User=`, which `limits.d` doesn't reach |

So no lab machine grants the class today, and production's server wouldn't take it as deployed.

**Ada's call:**

- **The real-time A/B on fedora** needs one root command there, which restarts nothing and lasts until the
  user's service manager next starts (a reboot or a re-login): it raises that manager's own limit, so a unit
  that asks for `LimitRTPRIO=1`, as the lab's rt-* rounds do, gets it, and no other unit changes (each gets
  the limits the manager started with unless it asks):
  `sudo prlimit --pid "$(systemctl show -p MainPID --value user@1000.service)" --rtprio=1:1`. The lasting
  alternative, `adalundhe - rtprio 1` in `/etc/security/limits.d`, takes a new service manager there, which
  restarts fedora's runner.
- **Production:** `LimitRTPRIO=1` in `kiln-server.service` and `[server] realtime = true` in its config, a
  deploy; cores of its own there would also want a partition its hand-written unit doesn't make.
- **A partition in the lab,** `preempt=full` at boot there, and `isolated` partitions anywhere: root's, and
  outside what the lab's driver can do as a user.

### The A/B to run (goal 9 step 4)

Weights alone against cores of its own against the real-time class, beside the same hogs in production's
layout, eight rounds each, interleaved, with `scripts/shard_lab/driver.sh` on fedora (the coordinator's):

- `hogs`: weights alone (the server at `CPUWeight=1000` on 16-31, 16 spinning hogs at `cpu.weight` 1 there);
- `cores`: the server on cores of its own (16-23, its shards on 17-20), the hogs on 24-31;
- `rt-hogs`: `hogs` with the shards in the real-time class: the bare-metal default where it's granted;
- `rt-cores`: `cores` with the class;
- `calm` and `rt-calm`: no hogs, so the class is seen to cost calm nothing.

Once Ada has given the room (above), check it, then run (this branch's `kiln` and `kiln-lab`, built as for
step 7's lab):

```sh
# 1, where a unit asking for LimitRTPRIO=1 gets it; 0 and every rt-* configuration is skipped, said in driver.log
systemd-run --user --quiet --wait --pipe --collect -p LimitRTPRIO=1 bash -c 'ulimit -Hr'
systemd-run --user --unit=g9s4-ab --collect -E HOME=$HOME -E PATH=$PATH -E XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR \
  env ROUNDS=8 N=16 LAB_CPUS=0-15 SERVER_CPUS=16-31 CORES=16-23 CORES_HOGS=24-31 SHARDS=4 \
  CONFIGS="calm hogs cores rt-calm rt-hogs rt-cores" \
  bash <worktree>/scripts/shard_lab/driver.sh <kiln> <kiln-lab> ~/.cache/kiln-lab-pending/g9s4
```

About 2.5 to 3 minutes a round, 48 rounds: two to two and a half hours. Without the room the same command runs
`calm hogs cores` and logs each rt-* configuration skipped. `COMPARE` (driver.sh) sets the report's pairs; by
default `calm:hogs hogs:cores hogs:rt-hogs hogs:rt-cores cores:rt-cores calm:rt-calm cores-calm:cores`.

What to read:

- `driver.log`: each rt-* round's line `4 of 4 shard threads in the real-time class` (from the hook's
  `affinity.txt`: policy 2 is `SCHED_RR`); fewer, and that round measured something else.
- `report.md`: the shards' delay p99.9 of `hogs`, `cores`, `rt-hogs` and `rt-cores` (goal 9 step 4's bar: under
  1 ms with hogs), and `kiln-lab compare` of each pair, over every number at once.
- **The class stays the default** where `rt-hogs` makes the bar or beats `hogs` beyond noise, with no number of
  `rt-calm` worse than `calm` beyond noise (API reads, pages and frames among them: a shard in the class also
  preempts the server's own pools on its CPU). **It goes back to off** where `rt-hogs` doesn't beat `hogs`.
  **Cores need Ada's call** where only `rt-cores` (or `cores`) makes the bar: then cores of its own, which take a
  choice of how many and which, would have to become a default the installer derives.

### Why: fedora's kernel, read (goal 8 step 7)

fedora runs Linux 6.19.10 (`6.19.10-300.fc44`, read 2026-11-05): `CONFIG_PREEMPT_DYNAMIC` booted lazy
(`CONFIG_PREEMPT_LAZY=y`, no `preempt=` on its command line), `CONFIG_HZ=1000`, `CONFIG_SCHED_CLASS_EXT=y`
with no scheduler loaded, on an i9-14900HX whose 32 CPUs share one L3. Its scheduler, read in the stable
tree's v6.19.10 (`kernel/sched/core.c`, `fair.c`, `syscalls.c`, `kernel/entry/common.c`; lines as there):

- **Every wakeup preemption of the fair class is lazy.** `check_preempt_wakeup_fair` ends in
  `resched_curr_lazy` (fair.c:8981) whatever led there: a shorter slice (`PREEMPT_SHORT`, fair.c:8927), an
  idle entity beside it (fair.c:8903), the pick. Under lazy preemption that sets `TIF_NEED_RESCHED_LAZY`
  on the running thread and sends no interrupt: `__resched_curr` sends one only for the full mark
  (core.c:1131-1132), and makes the lazy one full only for the idle task (core.c:1114).
- **The busy CPU switches at its next return to user space, or its tick.** `exit_to_user_mode_loop` takes
  either mark (common.c:31), so a system call's end, a fault's or any interrupt's runs the woken thread;
  `sched_tick` makes the lazy mark full (core.c:5539). A return to kernel code takes only the full one
  (common.c:172). Behind a thread spinning in user space, as the lab's hogs do, that is the tick: up to
  1 ms at 1000 Hz, half of it on average. Real legs enter the kernel often (wake-look1 in docs/priority.md:
  hogs entering it every 20 µs held a wake 31.6 µs at p99.9), so the lab's hogs are the worst case.
- **What preempts at once:** a wake of a higher class (`wakeup_preempt` calls `resched_curr`, core.c:2183):
  a real-time or deadline thread over a normal one, a normal one over a sched_ext one; a wake onto an idle
  CPU; and a wake that goes through the target's wakelist and its interrupt, which a wake from another
  last-level cache does (core.c:3854), as does one onto an idle CPU (core.c:3871) or one that finds the task
  still switching out (core.c:4196).

So for each means a thread has without root:

| means | needs | a wake behind a spinning hog | its cost |
| --- | --- | --- | --- |
| a slice of its own (`sched_attr.sched_runtime`, 0.1 to 100 ms, fair.c:5198) | nothing: `user_check_sched_setscheduler` doesn't check it | still lazy; but the wake decides at once, past the hog's slice protection (`RUN_TO_PARITY`, fair.c:1037; the kernel's slice here is 2.8 ms, 0.7 ms × (1 + log2 8), fair.c:79, :206) | turns taken sooner beside threads of its own weight; kiln's shards ask for their step, 266.7 µs, already |
| the hogs at the idle policy (`SCHED_IDLE`, or `cpu.idle` on their cgroup) | nothing, for kiln's own legs and agents | still lazy: an idle entity is preempted without the pick (fair.c:8903), lazily | the hogs at a weight of 3 |
| sched_ext | root: loading a scheduler takes `CAP_BPF` and `CAP_SYS_ADMIN` (fedora: `unprivileged_bpf_disabled` 2); without one a `SCHED_EXT` task is fair (ext.c:3734) | at once, with the hogs in the loaded class (core.c:2183) | a BPF scheduler to keep |
| the wakelist across caches | two last-level caches | at once | not on fedora: one L3 for all 32 CPUs; adacachy has two and preempts fully anyway |
| spinning before it parks (hyper-rt's `spin_ns`) | nothing | none for a wake within the spin; one after it waits as before | the spin's CPU at every idle gap, taken from the hogs |
| the real-time class | `RLIMIT_RTPRIO` room (fedora: 0) | at once | "What needs Ada" |
| a nudge: a timer of its own on its CPU while it waits | nothing | at most the nudge's period: the timer's interrupt returns to the hog's user space and takes the mark | a wake of the waiting shard every period |

### What kiln does: a nudge for the shards (goal 8 step 7)

`[server] nudge = true` (off until declared; `KILN_RT_NUDGE` overrides it, `KILN_RT_NUDGE_US` sets its
period for a lab's A/B) holds each of a shard's waits in its driver to the timers' tick, 100 µs
(`hyper_rt::compute::nudge_current_thread`: a thread-local the epoll driver's `wait` reads). With the
shard's timer slack, which is that tick too (hyper-rt's `Shard::build`), its CPU takes an interrupt of the
shard's own at least every 200 µs while it waits. The wait's timer is armed on the CPU the shard parks on,
busy with the shard as it arms (`get_nohz_timer_target`, core.c:1202), and its interrupt's return to the
hog's user space takes the lazy mark, so a shard woken behind a hog waits at most two ticks, no longer than
a timer of its own may be late, instead of the kernel's tick.

- **In the real-time class's place.** A shard that took the real-time class (`[server] realtime`, granted)
  asks for nothing more: its wake preempts at once. Refused, or not asked for, it asks for its slice and,
  with `[server] nudge`, nudges (`crate::rt::shard_asks`). macOS and Windows refuse the nudge: they have no
  lazy preemption to bound.
- **It says so**, in the log once every shard has answered and in `/healthz/server` (`priority.nudge`:
  asked, period, shards, nudged, those in the real-time class, refused, why).
- **What it costs.** A waiting shard wakes once a period, up to 10,000 times a second at the tick: a park and
  a turn of its loop each, CPU the shard spends (its busy share rises with it) and the threads beside it
  lose; and a calm machine's CPUs can't reach their deeper idle states while a shard waits (fedora's C3
  takes up to 1,048 µs to leave, which a calm wake pays too). That cost is measurable, so the nudge is a
  setting, not the default. The microbench below measures it.

### The wake microbench (goal 8 step 7)

`crates/kiln-rt/benches/lazy_wake.rs` times a cross-thread wake of a hyper-rt shard (a task sent from off the
runtime, `submit_to_holder`: the control queue and the kick) at exponential gaps, 1 ms on average, with
spinning hogs at `cpu.weight` 1 on the shard's CPUs and the bench at `CPUWeight=1000`, for each means above:
`calm`, `calm-tick:100`, `plain` (the kernel's slice), `slice` (kiln's), `slice-min`, `idle-hogs`, `rr`
(where granted), `spin:10`, `spin:100`, `tick:100`, `tick:50`, `unpinned` (the lab's `hogs` layout) and `llc`
(where there are two caches). Each arm gives its wakes' p50 to max, the shard's CPU, its driver waits a
second, its run-queue wait, the hogs' CPU, and the CPU other work took on the shard's CPUs. On fedora, the
lab's queue between its runs:

```text
cargo bench -p hyper-rt --bench lazy_wake --no-run
systemd-run --user --scope --quiet --collect -p CPUWeight=1000 -- \
  cargo bench -p hyper-rt --bench lazy_wake -- --cpus 16-19 --waker 24 --secs 10 --rounds 3
```

Its results go here when it has run.

### What needs Ada (goal 8 step 7)

- **The real-time class, `preempt=full` or a partition on fedora**, each root's: "Ada's call" above. With any
  of them the shards need no nudge there: a real-time wake, a full kernel's and a core of its own each run a
  woken shard at once.
- **Whether kiln should ask rtkit.** fedora runs `rtkit-daemon`, which grants `SCHED_RR` up to priority 20,
  without root, to a thread of a local session's process whose `RLIMIT_RTTIME` hard limit is at most 200 ms;
  kiln would ask over D-Bus. But a shard that ran 200 ms without sleeping would get the server killed (the
  kernel's `RLIMIT_RTTIME`; goal 9 step 2 found polls of 76 ms), and whenever rtkit's watchdog thread, a
  normal one, starves for 10 s it demotes every thread it granted and refuses for 5 minutes: its journal shows
  it did on 2026-11-04 under load. Not built; hers to weigh.


## What the lab doesn't do yet

- **CI.** A lab kiln has no repositories, so its "CI pages" are the run lists and overview, empty;
  no job writes logs and no one tails them.
- **One machine.** The runners, their stand-ins and the lab's own clients share the server's
  machine; production's runners are mostly elsewhere. The server-side numbers (`server.*`,
  `shards.*`) are the server's alone; the lab's own (`page`, `api_*`, `websocket`, `attach`) are what
  a person on that machine would see.
- **HTTP waits.** A request's wait before its handler can't include time its bytes sat in the socket
  while its shard was busy (see above): that's in the shard's delay.
