# Train notes: the API runtime program, trains 1 to 7

The API runtime program (kiln agent 01a1aa52's goal stack, resumed on 2026-11-05 after its first
coordinator was lost) aims to take the locks off kiln's server's hot path, to run its shards and
answer its API within a millisecond, and to cut its memory to a tenth. What its seven trains since
the resumption deliver, what deploying them does and how to watch it, its risks and its rollback,
and what's still open. How the server is measured: docs/runtime-lab.md. How it comes first on bare
metal: docs/priority.md.

**Where it stands (2026-11-05, updated about 18:35Z):**
- **Landed: trains 1 to 7,** each a merge of the integration branch `rt-int` into main, main its
  first parent. Each merge's message says what it holds and what ran.

  | Train | Merge on main | Landed (UTC) | Gated tree | Holds |
  | --- | --- | --- | --- | --- |
  | 1 | `12c5a76a997` | 08:43 | `e5a463e5aea` | goal 8 step 5; goal 5 step 2 (parts 2, 2b); goal 9 steps 1 to 3; goal 10 steps 2 and 3; goal 11 steps 1 to 3 (parts 1 to 3) |
  | 2 | `3e427eae99b` | 10:08 | `d46ceb48d67` | goal 5 step 2 (CI, Ship, relay, job files); goal 6 steps 3 and 4; goal 8 step 7 and goal 9 step 4 (the server's cores, the real-time class); goal 9 steps 3 and 5; goal 10 steps 2 to 4; goal 11 step 3 |
  | 3 | `5c4047a47e5` | 11:26 | `7dd67a5ce0f` | goal 5 step 2 (terminals, the front door's replica locks, the live channel's receivers); goal 11 step 3 (the remaining bounds) |
  | 4 | `6a3f0072091` | 12:37 | `4b429f1a05c` | goal 11 step 3 (the deployments engine's inputs and actions, the bus's per-topic peak); goal 10 step 1's baseline and the server-cores lab, in docs |
  | 5 | `3e8f9f1c9b0` | 13:40 | `f5b2ce35c36` | goal 6 step 4 (the standing rule); goal 9 step 3; goal 5's A/B, goal 6's runs and goal 11's memory by owner, in docs |
  | 6 | `fa6d88b754f` | 16:11 | `58ebfa84134` | goal 9 step 3 (places.json off the hub's shard, learner catch-ups in parts, read costs per kind); goal 10 steps 4 and 5 (the runner wait's legs, deliveries timed, snapshots yielding); goal 11 steps 4 and 5 (gzip on request, the brotli window, 16 KiB agent sockets, the jemalloc feature, spill files); goal 5's checks in its message |
  | 7 | `adefb3e4ab1` | 18:12 | `3ff004c457b` | goal 4 (harness frames on a ring of their own, listeners reading a frame's kind first); goal 6 step 3 (critical first on the run queue); goal 8 step 7 (`[server] nudge`); goal 9 steps 3 to 5 (the derived quantum, custody's work off the shards, parks, wakes and syscalls counted, batched writes, install's grants); goal 10 step 5 (reads); goal 11 steps 3 to 5 (segment and snapshot bounds, streaming snapshots, the allocator switch, startup's pages dropped, build-time brotli) |

- **Landed since: train 7,** `adefb3e4ab1` at 18:12Z (the table above). Its gate on `3ff004c457b`:
  clippy passed on all three targets; 5,409 tests passed, and the two that failed in the run passed
  alone after (w05_r03 3 of 3; w13_r04's Android image in 81.5 s, its push run having outlasted the
  test under the host's load).
- **In the next train (rt-next):** goal 4's lanes (harness frames on a lane per owner shard,
  `Hub::events` on a lock-free lane, `memory::queue` lock-free with every queue moved to it, the owners'
  inboxes and the net thread's among them, and terminal output, job log feeds and CD's change feeds on
  lanes); goal 9's log lines written by a thread of their own, each slow poll's time on the CPU beside
  its hold (the task clock), the one-allocation oneshot and the parks' durations; goal 11's bus bounds
  derived from the bus lab; the shard lab's reports with `--no-inline`.
- **Deployed: none of it.**
  - Production is the bare-metal server on adacachy: `kiln-server.service`, a system unit in
    `kiln.slice`, its process running since 04:45:18Z on 2026-11-05.
  - It runs main at `8234e9b61ed` (2026-11-04 16:58Z, "Merge g3b-finish"). That's the one commit
    its binary carries, and the commit every connected runner reports but one Mac's, which runs
    `1460599d430` (`updates.list`).
  - So production has goals 1 to 3 and goal 8's first part (`82ca3c74d03`). It has none of the seven
    trains, nor the program's landings between its commit and train 1: goal 4, goal 5's part 1,
    goals 6 and 7, goal 8 step 5's first part, and goal 10's steps 1 to 3.
  - KilnCD's `default/kiln-production` still exists (Healthy, OutOfSync), but no kiln pod runs
    under it. Its Deployment names `kiln:6c1eebcd8fb`, and its last history entry is a failed sync
    of `95d00ffbc8e` (2026-11-03 06:20Z). Production left it for bare metal (plan
    `01a18e073f0c74c19a990dba3e766656`, "Production kiln on bare metal"), so this release doesn't
    reach production through KilnCD.
- **Gated:** `ci/gate.sh --cpus 12 --mem 40g` on adacachy, `--mem 24g` for train 2. Each gate passed
  clippy with `-D warnings` for Linux, Windows (`x86_64-pc-windows-gnu`) and macOS
  (`aarch64-apple-darwin`). Every test that failed in `cargo test --workspace` passed alone on the
  same build after:

  | Train | Failed in the run, each passing alone after |
  | --- | --- |
  | 1 | 16: 13 while DNS was down (07:50 to 08:10Z, Docker pulls), 3 under the gate's load |
  | 2 | 2: `k8s_pool` w17_r01, `runner::runtime` w05_r03 |
  | 3 | 2: `harness::socket`'s sweep test, w05_r03 |
  | 4 | 2: `hosting` w03_r09, w05_r03 |
  | 5 | 1, beside 5,303 passed: w05_r03 (3 of 3 alone) |
  | 6 | 1, beside 5,319 passed: w05_r03 (3 of 3 alone) |
  | 7 | 2, beside 5,409 passed: w05_r03 (3 of 3 alone); `hosted_actions` w13_r04 (alone, 81.5 s) |

  `w05_r03_a_host_leg_runs_on_its_cpus` failed beside the full suite in all seven, its leg seeing
  the host's load (W05-R03).

## What it delivers, by goal

Each goal is a kiln plan. The lab's numbers come from docs/runtime-lab.md, the trains' merges and the
hand-overs, all from fedora, mostly in production's layout: the server alone in a scope at
`CPUWeight=1000`, CPU hogs at weight 1.

**Goal 4: each session's copy on a shard of its own** (plan `01a1a59c2a9c`; step 9 of plan
`01a17197`). Landed before the trains (`cba5a019725`, `6b0e578c8a3`).
- Each agent session's copy on the server has one owner shard, and only that shard changes it.
  Readers load an immutable view without a lock.
- The registry's global `Mutex<HashMap>` and its 40 lock sites are gone. A test fails if a lock
  comes back around a copy.
- Lab: calm, only resident memory was worse (+8.2%). Snapshots went back to the blocking pool after
  the compute pool made attaches 2.4 times slower beside hogs.
- **Train 7:** event listeners read a frame's kind off its front and parse only the kinds they
  handle; harness frames go on a ring of their own, read by the events socket, push and agents.wait,
  each event's bytes shared across viewers.
- **Lab** (many sessions, main before goal 4 against train 6's tree, 8 rounds each):
  - calm: API reads 755 to 5.6 ms at p99.9, attaches 234 to 2.4 ms at the median, resident memory
    2,506 to 783 MB, no number worse;
  - with hogs: reads and attaches as much better, but frames worse (p99 140 to 356 ms). Runners'
    messages wait on the hub's one loop (231 ms at p99), which goal 10 step 4's loops per shard take
    away.
- Open: step 5's "better with many sessions" with hogs, waiting on goal 10 step 4.

**Goal 5: no lock on the hot path** (plan `01a1a59c2aae`).
- **Part 1** (landed before the trains, `0fb5b21309e`):
  - kiln.db's reads take read-only connections of their own (WAL), side by side, and its writes go
    in turn to one writer thread;
  - read-mostly state is copied and swapped in whole (`crate::rcu`);
  - questions to runners wait in take-once cells, and the runners' commands have one owner task;
  - `scripts/locks.py check` runs in both CIs.
- **The trains:**
  - train 1: the hub's runners and their processes, the registry's side state, the machines' agent
    limits;
  - train 2: CI's engine and caches, Ship's Deploys, reach's relay, a live job's files;
  - train 3: terminals, the front door's replica locks, the live channel's receivers;
  - `scripts/locks/hot.toml` now lists no lock left to do on the hot path.
- **Lab** (step 4: main before goal 5, `426b02ea011`, against train 3's tree; the program's change
  since then, not goal 5's alone):
  - No lock on the hot path was waited on.
  - API reads' p99 went from 509 to 1.05 ms calm, and from 533 to 1.96 ms with 16 hogs. Frames' p99
    went from 65 to 2.5 ms calm, attaches' from 569 to 6.5 ms, and the store share's wait from 867
    to 0.46 ms.
  - What it cost: the medians of pages and API writes rose 13 to 14%, and the compute pool runs
    seven times the jobs.
- Open: step 4's Windows and macOS test runs (under Risks).

**Goal 6: shards that protect themselves** (plan `01a1a59c2abe`).
- **Before the trains** (`b3a00b70cff`, `a2122e27d28`, `5c3a3a3cbb1`):
  - a task past its share yields at its next await;
  - a watchdog thread names a shard held in one poll and moves the spawns waiting for it;
  - every request has a class: Critical (health, sign-in, a person's pages and actions), Normal (the
    API) or Background (sync, polling, telemetry);
  - shedding, off until `[server] shed = true` (docs/ci.md §0.3, opt-in automation).
- **Trains 2 and 5:**
  - a queue sheds only while it stands: a shard's while most of a tolerance (1 s) was past its calm,
    the shard running most of it; a pool's once every period of a tolerance was;
  - kiln's clients send a shed request again after its `Retry-After`.
- **Lab** (train 5's rule, 6 interleaved rounds each):
  - Flooded: every shed-on round shed both classes, and pages were served with no error in all 12
    rounds, at 4.2 to 5.3 ms at the median against 13 to 15.5 ms with shedding off.
  - Calm: no shard's queue stood. The only sheds, 2 and 6 requests, came from kiln.db's writer
    stalling 2.4 to 3.7 s on fedora's full disk.
- **Train 7:** a request read in Critical's turn gives way to a woken Critical task, and on kiln-rt's
  run queue a more urgent class leads by up to a quantum.
- Open: step 3's Critical p99 with hogs, which fedora's kernel bounds (Ada's decisions).

**Goal 7: placement** (plan `01a1a59c2aea`). In any deploy, though in no train:
- `[server] placement` picks `round-robin`, `steer`, `steal` or `both`;
- the A/B found none better than `round-robin`, which stays the default.

**Goal 8: the server first on bare metal** (plan `01a1a59c2b29`; the CPU part of plan `01a19617`
step 10).
- **Step 5:** headroom reads `/healthz/delay`'s totals (`01d3ff1c99b`) and holds the server's delay
  (train 1).
  - Calm is learned only while calm and unpaced, and a cut for a target that comes first holds until
    the target is back within calm's noise.
  - Lab item2b: 44 of 44 bins within calm's noise under load, and 9 of 9 after it ended.
  - The server's targets are added only on the server's own machine (`6b0e578c8a3`).
- **Step 7** (train 2):
  - with `[server] cores`, kiln keeps its own legs and agents off the server's cores (Linux
    affinity, the CPU ledger, Docker's cpusets);
  - `[server] realtime`, off until declared, puts the shards in `SCHED_RR` at priority 1 where the
    unit grants it (`kiln install` writes `LimitRTPRIO=1` for it);
  - `/healthz/server` says both, under `priority`.
- **Lab s7c** (train 2's tree):
  - hogs on the server's CPUs cost its tail;
  - kept off its cores, they cost little more: API reads' p99 rose 10.7%, just past noise;
  - kiln's own fence did what `taskset` does.
- **Train 7:** `[server] nudge`, off until declared, holds each shard's wait to the timer's tick, so a
  shard woken behind a hog on a lazily preempting kernel (fedora's) runs within about 200 µs, with no
  privilege needed.
- Open: step 7's proof: the nudge's A/B on fedora, or fedora's root (Ada's decisions).

**Goal 9: shards under a millisecond** (plan `01a1a87b2cec`).
- **Measured** (train 1):
  - each shard's delay to the microsecond, with finer buckets;
  - every poll past 100 µs named by its spawn site and label (`kiln_shard_held_*`).
- **Moved off the shards** (trains 1 and 2):
  - custody's signatures (W27-R08: nothing said before it's signed), Error Tracking's save, the
    host sample, and relayed calls' signatures;
  - services, spread over the shards;
  - runners' long frames, read whole and off the hub's shard (byte for byte the same wire).
- **Train 2:** a session's owner takes an update's events in turns.
- **Train 5:**
  - channel items are read as frames are;
  - the trail's checkpoint runs in one compute job;
  - each owner's poll is labelled by the message that held it.
- **Train 6:**
  - places.json is written off the hub's shard, and shares are worked out by key;
  - learner catch-ups go a part at a time;
  - read costs are learned per kind of message.
- **Train 7:**
  - the step quantum derived from the bar and the timer's floor (266.7 µs);
  - custody's take and check off the shards;
  - parks, wakes, kicks and system calls counted per shard, the kick's eventfd edge-triggered;
  - HTTP handler futures boxed where made, live frames fed and flushed once a batch;
  - `kiln install` grants each install what it may: weights, CPUs, a system install's cpuset
    partition, the real-time limit.
- **Lab:**
  - Main's calm baseline: shard 0, which ran every start-up task, was 94 ms late at p99.9.
  - s7c on train 2's tree: the shards ran ready work 1.1 ms late at p99.9 calm, 3.3 ms with hogs on
    their CPUs, and 2.0 ms on cores of their own with the hogs beside them.
  - g9p6 on train 6's tree: 1.42 ms at p99.9 calm and 2.21 ms with hogs; busy share 0.040 and 0.031.
- Open: steps 4 and 6 (p99.9 under 1 ms with hogs; busy share at most a tenth of goal 3's) and
  step 5 (a tenth of the CPU). fedora's kernel bounds the first two (Ada's decisions).

**Goal 10: API reads and frames under a millisecond** (plan `01a1a87b2dae`).
- **Before the trains:**
  - session copies keep their events in chunks, each packed once (`0447ee645b9`);
  - kiln-lab times the server to the microsecond in production's layout (`336a6e87c4a`);
  - sessions' reads come from their views (`a3a9e48aeaa`);
  - plans and goal stacks are read from memory, and packed snapshots go in fragments
    (`29fa3f34d62`).
- **Trains 1 and 2:**
  - one packer, and digests made as chunks pack;
  - `/api/agents`, `/api/overview`, `/api/runs` and `/api/me` answer from memory, encoded ahead;
  - a runner's session update goes straight to its owner shard.
- **Train 6:**
  - the runner's wait split into its legs (`kiln_runner_wait_leg_seconds`);
  - kiln-lab times only frames that bring events;
  - a viewer's snapshot gives up its shard between its 128 KiB pieces.
- **Train 7:** a session's info encoded once for its view and its events frame; its events page
  inflated once a chunk and sent in pieces; the sessions list joined once per change; the per-request
  future from 54,816 to 2,064 bytes.
- **Lab:**
  - Baseline on train 2's tree: API reads 234 / 970 / 1,244 µs at p50 / p99 / p99.9 calm, and
    437 / 1,833 / 3,804 µs with hogs.
  - Frames reach their viewers while another viewer attaches (step 3).
  - On train 5's tree, calm, live frames took 7.4 ms at p99.9. Earlier tails were the lab's own: a
    state frame quoting an old stamp.
  - g9p6 on train 6's tree: API reads 2.6 ms at p99.9 calm and 3.0 with hogs; frames 16 and 23 ms.
- Open: step 4 (runners' messages waited 7.3 ms at p99 calm in g9p6, nearly all of it for the hub's
  one loop; the loops per shard wait on the live channel's owner's review) and step 5 (reads and
  frames under 1 ms at p99.9, calm and with hogs).

**Goal 11: a tenth of the memory** (plan `01a1a87b2e56`).
- **Train 1:**
  - sessions' entries kept as text, and sealed chunks kept compact: a 20,000-event session went from
    39.22 to 3.90 MB, and from 107.91 to 5.23 MB with the store;
  - memory on `/metrics` by kind and by owner;
  - the UI's brotli windows fitted to each file;
  - the conversation store's write-behind queue bounded at what its writer commits in 50 ms.
    Senders wait; nothing is dropped.
- **Trains 2 to 4:** every cache and buffer is bounded in bytes, with its basis and what happens at
  the bound (docs/runtime-lab.md's table). The deployments engine's inputs and actions are bounded at
  256 MiB, a shard's desk is made as first needed, and grok rules are compiled on first use.
- **Train 6:**
  - gzip copies of the UI are made only for a client that takes gzip alone;
  - the feed's brotli window is fitted to its batch;
  - agent pages' sockets read 16 KiB at a time;
  - `--features jemalloc`, off, for the allocator's A/B;
  - a session copy's sealed chunks past its newest 100 events are spilled to a file of its own,
    leaving about 91 KB of segments a session in memory.
- **Train 7:**
  - session segments bounded a copy, and snapshots in flight by the fs share's places;
  - packed snapshots streamed a piece at a time;
  - jemalloc's settings set where the feature is on, glibc's arenas capped at 5, and
    ci/release/build.sh's `KILN_LINUX_ALLOCATOR`;
  - startup's pages and the UI's embedded files leave resident memory once the server is up (Linux;
    `KILN_KEEP_PAGES=1` keeps them), and the UI's brotli copies are packed at build time.
- **Lab** (step 1, production's static musl build, calm): 1,126 MB resident on main before goal 11,
  against 526 MB on train 4's tree. Of that, 127 MB is the binary's pages and 401 MB anonymous. The
  live heap is about 165 MB (jemalloc's profile of the same tree).
- **Lab** (step 4, rt-next's builds, calm, 8 rounds each): musl 523 MB against 333 MB with jemalloc
  (anonymous 398 to 207 MB), and glibc 530 against 338 MB; no latency worse.
- Open: step 4's pick in production's build, and step 5 (the 113 MB bar: Ada's decisions).

**Others' work in the same deploy** (from production's commit to main):
- runners keyed by machine id (`1df4674ca2d`, with notes of its own: docs/trains/machine-ids.md);
- headroom's fixes just before train 1 (`522285567b4`, `d9fc1b0c17e`, `d9ea7383284`, `1460599d430`);
- TLS session resumption (plan `01a1a832ebbf`, "Goal 7: Cheap connects on slow links":
  `426b02ea011`, `04b541b044f`);
- a runner's host sample off its loop (`2a856439c68`);
- smaller fixes: W16-R08's ratio interval, Mission control's offline ×, and the rule that a push
  waits on the tests relevant to its change.

## Deploying it

**What a deploy brings:**
- From production's `8234e9b61ed` to main: 327 commits (30 on main's own line), 312 files, +43,180
  and −4,460 lines. Train 6 adds 20 files, +1,343 and −97.
- The program's areas:

| Area | Production to main | Train 6 adds |
| --- | --- | --- |
| kiln-rt (`crates/kiln-rt`) | 36 files, +5,719 −336 | |
| kiln-lab | 7 files, +920 −133 | 1 file, +93 −6 |
| session copies, registry, spill (`harness/`) | 21 files, +5,130 −1,384 | 4 files, +701 −17 |
| hub, API, reads, store | 7 files, +3,815 −971 | 3 files, +99 −23 |
| runtime, shards' health, overload, placement, memory, `rcu` | 11 files, +4,509 −60 | 1 file, +100 |
| wire and proto | 7 files, +642 −52 | 1 file, +53 −8 |
| CI (`crates/kiln/src/ci`) | 21 files, +567 −144 | |
| kiln-ci (headroom's core, others' fixes too) | 5 files, +1,063 −68 | |

**Config: nothing to set.** Production's kiln.toml (`~/.config/kiln/server.toml`) sets none of the
keys below: its `[server]` holds addresses, `data_dir`, `metrics_public`, `public_url` and
`tls_names`, and it has no `[bus]`, `[cluster]` or `[conversations]`. What's new is off, or today's
behaviour, until declared (docs/ci.md §0.3):

| Key | Default | Overridden by | Goal |
| --- | --- | --- | --- |
| `[server] shed` | `false` | `KILN_SHED` | goal 6 step 4 |
| `[server] placement` | `"round-robin"` (else `steer`, `steal`, `both`) | `KILN_RT_PLACEMENT` | goal 7 |
| `[server] realtime` | `false` | `KILN_RT_REALTIME` | goal 8 step 7, goal 9 step 4 (train 2) |

- `[server] cores` was there already. With it, kiln now also keeps its own legs and agents off those
  cores.
- `KILN_RT_QUANTUM_US` (goal 9) sets the shards' step quantum, for the lab's A/B of quanta.
- The release build leaves jemalloc out (`ci/release/build.sh` doesn't ask for the feature).

**Migrations:** none of the program's. The deploy brings one of machine-ids',
`20261104-1200_machines_names`: a new table, with no epoch line.

**At its first start:**
- Session copies are made again as at any start, now compact. Each spills its sealed chunks past its
  newest 100 events once they're packed.
- The spill folder is made, and emptied of `.seg` files: `<data>/harness/spill`, which is
  `~/.local/share/kiln-server/data/harness/spill` in production.
- The UI's files are compressed with brotli alone. A gzip copy is made on the compute pool the first
  time a gzip-only client asks for its file.
- Shedding stays off, placement stays round-robin, and the shards stay out of the real-time class.
- `/readyz` names any queue that stands and still answers 200.

**Order: the server first, then the runners.** Upgrading runners is Ada's (AGENTS.md: never restart
a runner). Either order works, but:
- Runners from before don't send a shed request again after its `Retry-After`. Turn shedding on
  only once the runners run this release.
- `/healthz/delay` answers a number in production's build and JSON in this one, and neither build
  reads the other's. So until the server and adacachy's runner both run this release, that runner
  reads the server's delay as unavailable, and headroom paces nothing for it. Once both do, it
  paces kiln's own legs and agents there while the server's shards run late, and lets go as they
  come back.
- The feed's smaller brotli windows, which shrink the server's decoding memory, come with each
  runner's upgrade: runners zip the batches.

**Disk:** the spill files hold about what they take out of memory: in the lab, about 27 MB on disk
for 32 sessions. Each file holds at most about twice what's live in it, and goes once no chunk
refers to it. Production's data volume has 2.2 TB free (7.3 TB, 70% used).

## Watching it

None of these series is on kiln's dashboards yet (`deploy/grafana`, `observe/dashboards.rs`): read
`/metrics`.
- **Memory:**
  - `kiln_memory_resident_bytes{kind}`: `anon` is what goal 11 cuts, and `file` is the binary's
    pages, about 127 MB;
  - `kiln_memory_bytes{owner}` against `kiln_memory_bound_bytes{owner}`;
  - swap isn't on `/metrics`: read `VmSwap` in the server's `/proc/<pid>/status`.
  - Before: 5,663,484 kB resident and 5,837,336 kB swapped, about two hours after its 04:45Z start
    on 2026-11-05 (read from `/proc`).
- **The spill:**
  - `kiln_session_spill_bytes{kind="live"|"file"}` (`file` at most about twice `live`);
  - `kiln_session_spill_failures_total{op}` should stay 0;
  - `kiln_session_spill_reads_total{on="shard"}` should stay small beside `on="pool"`;
  - in the log: "a spilled chunk's file didn't give it back", and "the session copies' spill
    folder couldn't be made".
- **Shards:**
  - `kiln_shard_delay_seconds`: a calm shard runs about 195 µs late at the median, by design (the
    timer's tick and its slack);
  - `kiln_shard_held_*{site,label}`: what held a shard past 100 µs;
  - `kiln_shard_stalls_total`, and the watchdog's log line.
- **Requests:**
  - `kiln_route_wait_seconds{class}` and `kiln_route_seconds{class}`;
  - `kiln_runner_wait_leg_seconds{leg}`.
- **Pools:**
  - `kiln_blocking_wait_seconds{share="store_write"}`: kiln.db's one writer, which API writes wait on;
  - `kiln_compute_wait_seconds`.
- **Overload:**
  - `kiln_shard_queue_standing_seconds` and `kiln_pool_queue_standing_seconds`;
  - `kiln_shed_total` stays 0 while shedding is off.
- **Settings:** `kiln_placement_info{placement="round-robin"}`, and `/healthz/server`'s `priority`
  (the real-time class asked for, taken or refused and why, and the work CPUs).

## Metric names

New since production's build. None has been put to Kiln Metrics: no hand-over records an ask.

- **Shards** (`crate::shard_health`, `crate::overload`):
  - goal 9: `kiln_shard_held_polls_total`, `kiln_shard_held_seconds_total` and
    `kiln_shard_held_longest_seconds`, then `kiln_shard_held_host_polls_total` and
    `kiln_shard_held_host_seconds_total`, all `{shard,site,label}`;
  - goal 11: `kiln_shard_tasks{shard}`, `kiln_shard_tasks_most{shard}`;
  - goal 6, the watchdog, each `{shard}`: `kiln_shard_coop_yields_total`, `kiln_shard_stalled`,
    `kiln_shard_stalls_total`, `kiln_shard_stalled_seconds_total`, `kiln_shard_stall_moved_total`,
    `kiln_shard_stall_reports_total`, `kiln_shard_stall_reports_suppressed_total`; and
    `kiln_shard_last_stall_seconds{shard,task,spawned_at,whose}`.
- **Overload** (goal 6): `kiln_shard_queue_delay_seconds{shard,class}`,
  `kiln_shard_queue_standing_seconds{shard,class}`, `kiln_shard_shedding{shard,class}`,
  `kiln_pool_queue_delay_seconds{pool}`, `kiln_pool_queue_standing_seconds{pool}`,
  `kiln_pool_shedding{pool,class}`, `kiln_shed_total{class}`.
- **Placement** (goal 7): `kiln_placement_info{placement}`, `kiln_placement_total{how}`, and
  `kiln_rt_movable_*_total` (`spawned`, `polls`, `stolen`, `shared`, `overflowed`, `handed_off`,
  `notified`).
- **Memory** (goal 11): `kiln_memory_resident_bytes{kind}`, `kiln_memory_bytes{owner}`,
  `kiln_memory_bound_bytes{owner}`, `kiln_bus_topic_bytes_peak`; train 6:
  `kiln_session_spill_bytes{kind}`, `kiln_session_spill_reads_total{on}`,
  `kiln_session_spill_failures_total{op}`.
- **Runners** (goal 10, train 6): `kiln_runner_wait_leg_seconds{leg}` (`to_loop`, `in_loop`,
  `to_owner`), a histogram.
- **Changed:** `kiln_shard_delay_seconds`'s buckets (goal 9): 25 µs apart to 250 µs, then 50 µs
  apart to 1 ms. A dashboard's quantiles across the deploy compare different buckets.
- **Lab builds only:** `kiln_lock_*` (`scripts/locks.py census-tree`).
- **Not this program's:** `kiln_tls_handshakes_total` (TLS resumption).

## Risks

- **The spill's disk writes** (train 6):
  - each spill job syncs its file (`sync_data`) on the harness folder's disk: in production,
    adacachy's `/home` (btrfs), where builds and agents write too. Jobs batch, one in flight a copy;
  - under memory pressure the page cache drops spill pages, and an attach then reads them from disk;
  - a write the disk refuses leaves the chunks in memory, counted and tried again.
- **A spill that doesn't read back** (CRC-32 checked):
  - a snapshot reads those events from the conversation store;
  - with no store (`KILN_CONVERSATIONS` off) the snapshot is refused, never made wrong;
  - a few owner-side reads happen on a shard: changing an older event, letting go of the oldest
    chunk, an out-of-order insert.
- **places.json a moment behind** (train 6): it lags a change by one write on the blocking pool. A
  crash in between loses that change, which the runner says again as it connects. A hung disk holds
  later writes, as one inline write held the shard before.
- **Bounds in bytes** (goal 11 step 3):
  - queues that took counts now take bytes, and their senders wait at the bound: session inboxes,
    the net thread, the CI dispatcher's inbox (the hub's loop waits past 64 MiB), the deployments
    engine's inputs and actions (256 MiB);
  - rings let their oldest go and say so: hub events, log feeds, terminals;
  - webhook deliveries past 1,000 events or 64 MiB a subscriber are logged as dropped, their bodies
    kept to send again. Syslog datagrams past 64 MiB a source are dropped and counted;
  - a queue can hold one item past its bound.
- **kiln.db's one writer** (goal 5): every write waits its turn there. In the lab, API writes' p99
  is about 1.4 s, from that queue: 606 jobs a second, waiting 1.5 s at p99 (train 5's tree). Before
  goal 5, reads waited behind the same connection too.
- **The compute pool works harder:** goals 3, 9 and 10 moved work onto it. Its jobs' wait at p99
  went from 0.25 to 7.4 ms calm, and from 3.2 to 36 ms with hogs. Pages' and API writes' medians
  rose 13 to 14%.
- **The UI's files after a deploy:** only this build's are kept compressed, so a page loaded before
  the deploy that asks for an earlier build's file gets it uncompressed (train 1).
- **Shedding, if turned on:**
  - Windows keeps no fine thread CPU time, so a shard's queue there stands on its waits alone;
  - a shard 50 to 80% busy whose probes run late sheds: the flood's case, never seen calm;
  - shedding stops up to half a tolerance after an overload ends abruptly;
  - runners from before don't retry a shed request after its `Retry-After`.
- **The real-time class, if turned on and granted:** real-time shards keep their CPU from normal
  threads while they run. The kernel's throttling leaves normal threads 5%.
- **`[server] cores`, if declared:**
  - a step can widen its own affinity (`taskset`): only a cpuset partition holds it;
  - every CI step is pinned as it starts, with a `/proc` scan each.
- **Windows runners:** in the coordinator's GitHub trial (runs 42019612537 and 42019616659), five
  `runner::docker` tests failed on main at train 3 and not on main before goal 5: W05-R03 twice,
  W13-R04 twice, W13-R08. Whether goal 8 step 7's work CPUs (`2e6aafd6497`) cause them is being
  checked. On macOS both runs failed the same two tests, and both timed out at 170 minutes.
- **Production's memory today** (under Watching it): most of it is in anonymous maps of 256 KiB to
  4 MiB. Goal 11's compact chunks and the conversation store queue's bound (train 1) bear on it,
  and the spill (train 6) is the biggest lever for it.

## Rollback

- **Going back** to production's build, or any between, goes the way the release went in. That's
  Ada's: the bare-metal unit's binary, not KilnCD.
- **kiln.toml:** every section is `deny_unknown_fields`, production's build's too. So a
  `[server] shed`, `placement` or `realtime` that was set must come out before going back, or the
  older build won't start. None is set today.
- **kiln.db:** the program adds no migration. machine-ids' new table is one an older build never
  names, and its migration runner opens a database with migrations it doesn't know, with a warning
  (`store/migrations.rs`, the same in `8234e9b61ed`).
- **On disk:**
  - The spill files (`<data>/harness/spill/*.seg`) are left: an older build never looks there, and
    the next newer one empties the folder as it starts. They're copies of sealed chunks the
    conversation store keeps, so they can be deleted by hand while no server runs.
  - places.json keeps its form (a map of each machine to what it holds, written whole and renamed),
    and an older build reads it as before.
  - A shard's desk is memory only.
- **The wire:** the program adds no message, variant or capability bit. proto.rs's bytes fields are
  written whole, the same bytes on the wire, so runners and servers of either build talk. Any brotli
  decoder reads the feed's smaller windows.
- **The unit:** a `LimitRTPRIO=1` added for the real-time class is harmless to an older build.
- **What's lost:** the memory and latency gains, and the new series (a dashboard built on them goes
  empty). Session copies are made again from the runners, as at any start.

## Ada's open decisions

- **Deploying it:** when, and how the bare-metal server takes the release, then the runners'
  upgrades.
- **fedora's root, for the lab:** `preempt=full` at boot, or a cpuset partition, plus a real-time
  rlimit (`RLIMIT_RTPRIO` is 0 there).
  - fedora runs Linux 6.19.10 with lazy preemption (`PREEMPT_LAZY`) at HZ=1000. There, any wake
    from another CPU onto a busy one waits up to a tick, 1 ms (docs/priority.md, wake-2: p99
    1.00 ms with every kind of hog, and with none of the lab's).
  - That bounds goal 6 step 3, goal 8 step 7, goal 9 steps 4 and 6, and goal 10 steps 4 and 5 in
    the lab.
  - Production's adacachy preempts fully (`CONFIG_PREEMPT=y`, no `preempt=` at boot).
- **Goal 11's 113 MB bar:** with the binary's 127 MB of resident pages, it can't be met.
  - Goal 11's fourth hand-over put what's reachable at about 200 MB of anonymous memory, maybe
    170 MB with jemalloc's decay tuned: about 300 MB with the binary's pages.
  - 113 MB of anonymous memory alone would need the live heap from about 145 MB to under 90 MB, and
    the allocator's share under 20 MB.
  - Counting file pages out, judging on anonymous memory, or a smaller binary: hers to say.
- **`/api/agents/tunnel` under shedding:** a runner whose UDP gets no answer reaches the server
  through it. It's Background today: with shedding on, a standing queue turns its opening away,
  and the runner is away until it opens it again after the `Retry-After`. Critical is never shed.
- **Turning on what's opt-in:** `[server] shed`, `realtime` and `cores` in production.
  - Production's unit has `LimitRTPRIO=0` and no `AllowedCPUs`, so the real-time class would be
    refused there and said so.
  - Cores without a partition keep only kiln's own work off.

## Open with others

- **Kiln Metrics:** the names above, none agreed yet.
- **Kiln DB:** goal 11 proposes a smaller `cache_size` for kiln.db's read connections and an
  `mmap_size` covering the file, plus a gauge on Turso's caches. No SQLite or Turso setting was
  changed. They wait on Kiln DB's word.
- **Quiver (CP-T2):**
  - its own locks are the only ones the census saw waited on: 0.047 ms a second at the most;
  - `[bus] memory` and `topic_bytes` are to be derived from the bus lab, and a per-topic byte count
    from Quiver would measure them inside the bus.
- **The live channel's acks** (agent 01a16b2c owns acks and resends): goal 5 changed only who reads
  the receivers, with acks, resends and order as they were. Goal 10's next cut of runner messages
  stops short of `wire/link.rs` and `wire/channel.rs`.
- **Plan 01a17197** (T3's session plan): goal 4 is its step 9.
- **Bare metal** (plan `01a19617` step 10, and plan `01a18e073f0c`): production's unit is
  `/etc/systemd/system/kiln-server.service` (`kiln.slice`, `CPUWeight=1000`, `MemoryHigh` 14 GiB,
  `MemoryMax` 16 GiB). The real-time class and a partition need changes to it.
