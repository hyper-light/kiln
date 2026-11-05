# Needs and capabilities: one engine places kiln's work, and agents don't negotiate

> **Design, not built** (2026-10-26). Two parts, built in order. **Part I** is the engine: one
> assignment engine for everything kiln places (CI legs, deployments' work, commands, agent
> sessions, holds on shared things), made correct, robust, efficient and fast first. **Part II** is
> the agentic layer on it: agents say what they want, kiln decides, and agents stop coordinating,
> asking and watching load. It grows W05's scheduler (docs/ci.md) and takes in two designs:
> CLM, "Coordination in kiln" (plan `01a174c54e0076a3b4e31e02848afa6c`, approved 2026-10-24,
> stopped at its first step when the machine its agents ran on went offline), and Headroom
> (branch `crash-guard`, on one runner from an unmerged build). IDs: `E-…` for the engine, `A-…`
> for the agentic layer, `G1`–`G10` for the guarantees. Decided 2026-10-26: E-D1, E-D2 and E-D5
> (§24).

Work says what it needs; machines, resources and agents say what they can do; one engine matches
them. It grants what fits now, holds a place in line for what doesn't, and says why for everything.
Every grant is a lease the engine can account for, fence and take back. Agents talk to it the way CI
does, through needs, and what it decides reaches them as news, so they never check a machine,
arrange a turn, or ask a person for what kiln can decide.

One gate, as it goes today and as it would:

| Today | With this design |
| --- | --- |
| `uptime`, `free`, the I/O pressure, `systemctl --user list-units 'kiln-gate-*'` | (nothing) |
| `kiln run --needs cpus=12,mem=40g,linux/amd64 -- ci/gate.sh` → `no runner suits` | `ci/gate.sh` → `queued: 2nd for 12 CPUs, 40 GiB, linux/amd64 · about 8 min · kiln tells you when it ends (run r12)` |
| a launcher script that tries again every five minutes, while 1-CPU work keeps the machine full | a reservation: it starts by a bound (G6) |
| messages with another agent about whose gate goes first | (nothing) |
| `output` read until the run ends | news: `run r12 (ci/gate.sh, tree 3e4a…) passed: 7 min in line, 31 min on a Linux machine with 32 cores` |
| "Ready to push: may I?", and the person's yes relayed to whoever else waits | `agents_change`, then `ci.diffs.land`: it lands with the next train the person approves |

## 1. Why

**What agents spend turns on.** In the 24 hours to about 03:00Z on 2026-10-27, counted as events
of kiln's conversation records (`conversations.search`) that hold the words, this design's session
left out:

| What | Events | Agents |
| --- | --- | --- |
| A command probing a machine's load (`uptime`) | 520 | 46 |
| Reading I/O pressure (`io.pressure`) | 164 | 22 |
| Looking for other agents' gates (`list-units kiln-gate`) | 84 | 19 |
| A run refused with `no runner suits` | 251 | 50 |
| A message from one agent to another, delivered | 314 | 29 |

CLM counted the messages of 2026-10-20 to 25: 1,130, half to three quarters of them about shared
things. What it looks like:

- **Turns arranged by message.** Two agents sent each other fifteen messages in three minutes about
  whose gate runs first on the production machine ("Final, and I won't change it again: mine goes
  first"), crossing, each with a waiter script watching the other's systemd units (agent:01a14bf7,
  agent:01a14a67).
- **Machines booked by message.** "I need adas-mac-studio-2 for about 90 more minutes … Tell me if
  you need it sooner" (agent:01a14d4a). An agent asked another, "fedora's queue", for about two
  days on that machine with no other heavy gates (agent:01a17514).
- **Schedulers built by hand.** A shared `heavy.lock`, `guarded.sh`, waiter units, and gate
  launchers that try again every five minutes and log each try: `try 3: no runner suits 12 CPUs,
  40 GiB of memory, os == linux, …` (agent:01a15d9e).
- **Leftovers nobody owns.** A stopped gate's 23 containers and 23 networks filled Docker's address
  pools for six hours, and every other gate's Docker tests failed. Agents spent turns finding whose
  they were, and removing them took the person's word: the harness's own classifier refused it.
- **The person's word kept in conversation.** "Ada's standing word for my goal stack is to land
  each goal myself once it's gated, one per push" lives in one agent's context; others relay theirs
  ("with Ada's go-ahead"). kiln can check neither.
- **Asking first.** "I'd rather not run a privileged Docker bench on an unverified machine without
  your go-ahead."
- **Waiting by timer.** A goal paused with "when this resumes in an hour, try once", for a runner
  that was connected but answered no commands.

**What the system does that makes them.** kiln places work in six ways, and they don't count each
other's work:

| Work | Placed by | Holds |
| --- | --- | --- |
| CI legs in production | v1's `Hub::schedule_once`: events and a 1 s tick, a compare-and-set claim, the runner with the most room | a slot or a share of the runner, counted by the hub |
| CI legs with `[ci] enabled` | W05's engine, `kiln_ci::sched`: queues, priorities, fair share, resource mode, reservations, admission by load, warm placement. Built, and off in production; with it on, v1 and the engine split the legs and each counts only its own on a shared runner | the same, counted apart |
| Commands (`kiln run`, MCP's `run`, `kiln.machines.run`) | the asking runner's `reach::directory::pick`, from a directory cached 30 s | nothing: "the room is what's idle then" (docs/reach.md §2); none suits now, refused |
| Agent sessions | `harness::lineage`, by reach's ranking (the idlest) | nothing |
| Deployments' syncs | kiln-cd's own queue: each cluster's request budget and concurrent syncs | its own budgets, apart from CI's |
| An agent's shell work | Headroom, on one runner from a branch: pacing | nothing |

And what decides for CI legs has gaps a design has to close, not tune:

- **A leg can run twice, and both copies are taken.** A runner cut off for 45 s keeps running its
  leg while the retry runs elsewhere. Nothing fences the stale copy: its result is taken by the
  runner's name, its logs while the leg is in that runner's list, its uploads by a token with no
  attempt in it that can't be revoked and lives until the leg's timeout plus an hour; its
  `GITHUB_TOKEN` is revoked only when a result comes. A reconnecting runner's resent events land on
  the new attempt.
- **A big leg can wait for ever.** A runner that frees goes to the best queued leg that fits, so
  1-CPU legs keep overtaking a 12-CPU one, and nothing holds room for it. No preemption, and no
  starting several legs together.
- **Fair share only orders work that arrived in the same minute.** The engine's first key is
  `priority − aging × minutes`, with minutes since the epoch (`sched::state`, `set_key`); fair share
  is the second. A repository that queues 1,000 legs a minute before another's 10 runs all 1,000
  first. W05-AC04 doesn't see it: its test queues both repositories' legs at the same instant. And
  cost is runner-seconds, so a leg asking 32 CPUs costs its repository what one asking 1 does.
- **Losing a runner reaches the engine late or not at all.** It hears only through v1's sweep, and
  only for a runner that held engine legs; it can keep choosing a runner that's gone, and a leg whose
  only eligible runner refused it waits `other-runner` for good.
- **Deciding waits, on old news.** Assigning waits for a GitHub token inside the dispatcher's one
  task; admission by load decides on I/O pressure up to 10 minutes old; a runner admits against a
  size it probed once, at its start; and each assignment is a transaction of its own on one disk, at
  `synchronous=NORMAL`, which a power loss can undo.
- **Claims by compare-and-set can't see the fleet.** docs/p2p.md has nodes claim jobs by
  compare-and-set: a claim sees one runner and one queue, so none can keep fair share, packing or
  reservations across the fleet.

Part I fixes the system; Part II changes what agents do with it.

---

# Part I. The engine

## 2. What it places, and on what

An **item** is work to place: a CI leg's attempt, a pool's executor, a command, an agent session, a
deployment's sync or apply, a hold on something shared (a claim), and, in Part II, a goal for an
agent. A **holder** is what it's placed on: a runner (and its executors), a named resource (a count:
minikube, a cluster's concurrent syncs, a KVM lab), or a rate (tokens with a rate and a burst: a
cluster's API budget). The engine decides where and when; what the work is stays with its owner
(the planner for runs, kiln-cd for deployments, the harness for sessions).

## 3. Needs and capabilities

An item carries:

| | |
| --- | --- |
| `kind` | leg, executor, command, session, sync, apply, claim, goal |
| `tenant` | where its share comes from: the person, then their program and track, or the repository and workflow (§5) |
| `class` | `critical`, `land`, `interactive`, `batch` or `background` (§7) |
| `needs` | in the spelling below |
| `estimate` | its duration (p50, p90) and peak use of each dimension (p95), learned (§5) |
| `preemptible`, `restartable` | whether it may be paused, and whether it may be stopped and run again |
| `on_lease_loss` | `stop` or `finish` (§8) |
| `key` | its inputs' fingerprint (W24-R01), for running the same work once |

The needs' spelling is docs/runners.md's (`kiln_ci::sched::needs`), grown:

| Spelling | | |
| --- | --- | --- |
| `cpus=8`, `mem=32g`, `disk=50g`, `gpus=1`, `io=100m` | requests | held for the lease's life |
| `heavy` | one of the runner's heavy units (§6) | for work that writes hard (a workspace's tests linking) |
| `linux/amd64`, `+gpu`, `tool:cargo`, `disk.free>=50Gi` | constraints | checked, never held |
| `exclusive` | the runner whole | as today |
| `quiet`, `quiet:load1<2/2m` | nothing else kiln places runs there while it holds; it starts once the condition holds | benchmarks |
| `hold:minikube`, `hold:kvm-lab=2` | units of a named resource | |
| `after:land:C42`, `after:run:r12`, `after:gate:<tree>`, `after:agent:01a1…`, `after:machine:+gpu`, `after:time:21:00Z` | it starts once that holds | conditions (§5) |
| `together:g/3` | one of three that start together or not at all | gangs (§5) |
| `for=45m` | its expected duration, when history has none | |

**Dimensions**, and how each is held to its grant on the runner:

| Dimension | Unit | Over its capacity | Held by |
| --- | --- | --- | --- |
| CPU | milli-CPUs | up to `overcommit_cpu` (shares) | `cpu.weight`; a job object's CPU rate; nice |
| memory | bytes | never | `memory.max`; a job object's memory limit; on macOS, watched and stopped past its request |
| disk | bytes | never | its scratch folder, measured |
| GPUs | devices | never | the devices it's given (`CUDA_VISIBLE_DEVICES`) |
| I/O | bytes a second | up to `overcommit_io` | `io.max` and `io.weight` where the runner can |
| heavy | units | never | Headroom's pacing |

A **runner's capabilities** are its facts (W13-R01), its budget (what kiln may place there, §6), its
room (budget, less what's held, less what it measures kiln didn't place), its heavy units, and its
state (connected, draining, offline, wedged). A **named resource's** are its count; a **rate's**,
its tokens.

## 4. Guarantees

| | The engine guarantees | Checked by |
| --- | --- | --- |
| G1 | No runner's grants exceed its budget in a dimension that can't overcommit, or budget × overcommit in one that can | an assertion after every input; the simulator |
| G2 | An attempt runs under at most one grant, and what a stale attempt writes (status, results, events, logs, artifacts, a deployment's apply) is refused | fencing (§8); the scheduler model (docs/sim.md), a million seeds; W05-AC16, live |
| G3 | A whole or quiet grant shares its runner with nothing else kiln placed; a resource's holders never exceed its count; a rate is never overdrawn | assertion; simulator |
| G4 | Nothing starts before its `after` conditions hold; a gang starts whole or not at all | assertion; simulator |
| G5 | Concurrency groups' and quotas' limits hold | W05-AC01; simulator |
| G6 | An item some runner could hold starts within a bound: its reservation's time (§5), plus what its runner's earlier grants overran within their timeouts | simulator: each wait against its bound |
| G7 | Under saturation, each tenant gets its weighted share of dominant-resource time within 10 % over any 10-minute window, whatever the order work arrived in and whatever its shape | W05-AC04, grown: staggered arrivals, mixed shapes |
| G8 | Every waiting item has exactly one reason, a place and an estimate; every grant, refusal and preemption says why | W05-AC10; a property test |
| G9 | Decisions are a pure function of the inputs: replayed on any node, of any architecture, the same commands come out | replay on x86-64 and arm64 |
| G10 | A grant is in the store before anything acts on it; through any failover nothing granted is lost or granted twice | W05-AC09, grown to running legs; the scheduler model (docs/sim.md), a million seeds |

## 5. How it decides

**Which item next.** By class (§7), then hierarchical weighted fair share across tenants, then each
tenant's own order.

- **Tenants form a tree:** a person, then their program and track, or the repository and workflow
  the work is for. Each node has a weight: a program's track weights (docs/programs.md), `[ci.fairness]
  weights`, the person's settings. Start-time fair queuing runs over the tree, as W05-R10 runs it over
  repositories.
- **Cost is dominant share × duration:** an item's largest request as a share of the fleet's
  capacity in that dimension, times its expected duration. 32 CPUs for an hour costs 32 times one CPU
  for an hour, and a memory-heavy test costs by its memory.
- **Tags are kept for every set of dimensions a runner may offer:** an item charges each set its
  largest share among them × its time (DRFQ's dominant charge), and a runner ranks tenants by its
  own set's tags. A GPU counts on the runners with GPUs, not against the same tenant's work on the
  others (H-DRF), and each tenant is ranked by its dominant resource among what the runner has. One
  tag for each dimension, ranked by the largest, doesn't hold: each dimension's virtual time moves
  on its own, and a tenant coming back after another ran alone is ranked by whichever dimension's
  virtual time is furthest ahead, not by its dominant one (E2.4 found it: 61 % where 75 % was due).
- **Charged at dispatch, settled at the end:** an item is charged its expected time when it's
  dispatched, so a burst registers at once (Kueue's entry penalty), and settled to the time it ran
  when it ends, as start-time fair queuing needs no time beforehand (DRFQ). A tenant that was idle
  comes back at the virtual time, with no credit or debt from before.
- **Priority, aged, orders a tenant's own work,** never one tenant's against another's: W05-R09's
  priorities pick which of a tenant's items goes when its turn comes, and an item's priority rises
  as it waits (W05-R09's aging), so a tenant's own stream of high-priority work never starves its
  low-priority work (W05-AC03). Between tenants, only weights count.
- **A class passed over is promoted.** An item that has waited its class's bound while its class
  started nothing where it would run is promoted a class, at most to `interactive`, and a promoted
  item never preempts (§7). A class being served isn't: there the order stays fair share's and its
  tenants' own, never the longest wait's (promoting every item that waited lifted a saturated
  class's own backlog past other tenants). With the classes' floor shares, that bounds every
  class's wait twice over. *Integration call (merging into main, to confirm with the person):* an
  `interactive` item also goes up into `land`, never `critical`, once the `land` and `critical` work
  queued or running when it came has started and is due done by its expected ends (p90 from history,
  else median, else its timeout), so it has a bounded wait with no picked bound, history or none.
- **Aging never orders tenants against each other.** There it would only measure who came first:
  today's key does that, which is why its fair share holds only within a minute.

**Where.** Of the eligible holders with room (less what reservations keep), the one where it's
expected to finish first: its wait there plus its run time there, given what's warm (W24-R18's
model, grown to reach's synced copies and their build output). Ties go to packing (`batch` and
`background`: the best fit on the dominant dimension, leaving big holes for big items;
`interactive`: the most room, for latency), then the item's preferences (W05-R23), then the asking
machine for light work.

**Big items don't starve.** With `[sched] backfill` declared (§21), the first item of each class
that waits only for room keeps room on the runner where its room frees soonest, from the expected
ends of the grants there: each one's p90, else its median, else its timeout, and once that has
passed, its start plus twice what it has run, never past its timeout (docs/ci.md W05-R38). That
runner grants only what will end before then, or fits beside the item then (EASY backfilling,
W05-R39). A 12-CPU gate on a machine that 1-CPU work keeps refilling starts by its bound: its
room's time, plus how far what it waited for ran past its expected ends (G6, W05-AC17). A gang
keeps room the same way, every one of its runners at once from the latest of their times, and
starts whole or not at all (G4, W05-R40, W05-AC18). An item waits for room by room alone, the room
kept for others aside: one that has room but is held off by room kept for another waits behind it.
Until runners can pause (E7), what's preemptible takes no kept room.

**The same work once.** Items with the same `key` share a grant: one asked while another runs joins
it, and one asked after another passed takes its result from the ledger while that is valid
(W24-R02, as W24-R06 reuses legs).

**Conditions.** `after:` needs, deploy windows, holds (CLM-7) and approvals are conditions the
engine is told about as inputs (a change landed, a run passed, a machine came back, a time came);
an item waiting on one has it as its reason.

**Estimates** come in as inputs, learned per work key: legs' from their history (W19), commands' and
sessions' from what runners measured them using. They give work that asks for nothing its request
(the p95 of what it used), give costs and reservation times, and give each answer its estimate
("about 8 min").

## 6. Capacity

- **Budget.** What kiln may place on a machine, per runner, set by the person (`[runner] budget`:
  CPUs, memory, I/O, heavy units), by default the machine less a reserve. A machine that runs
  production gets a budget that leaves production its room (CLM-4).
- **Room** is the budget, less what's held, less what the runner measures kiln didn't place (an
  agent's own shell build, a person's programs). The runner reports it when it moves by more than a
  step, and at least every 10 s.
- **Heavy units** are the runner's Headroom window: how many heavy items may run at full speed. The
  runner shrinks it when what it protects (an fsync in its data folder, production's health) misses
  its goal, and grows it back when that's healthy (docs/headroom.md on its branch); the engine sees
  this as capacity changing. Headroom's controller moves into kiln-ci (its author found it in the
  wrong crate).
- **The runner admits too.** On `Assign` it checks the grant against what it measures now (today it
  checks the engine's rule, `state::room`, against a size it probed at its start) and refuses what
  doesn't fit, saying why (`Rejected`); the item goes back to the queue (`OtherRunner{refused}`).
- **A slice for local work.** The engine leases each runner a slice of its own budget, renewed with
  its heartbeat, for work that can't wait on the network: commands that stay on the agent's machine,
  agent sessions and their shells. The runner admits that work from its slice first, and the engine
  counts the slice as held; past it, the work waits in the engine's line like any other, and the
  engine may grow the slice. Cut off from the engine, a runner still admits local work from it.
- **Reclaiming** (opt-in): `background` items may use what's held but unused (requests above
  measured use), and are paused or stopped when the holder needs it back.

## 7. Classes and preemption

| Class | What | May preempt | Preempted |
| --- | --- | --- | --- |
| `critical` | deployments and rollbacks of production, kiln's own upkeep | `background`, then `batch` | never |
| `land` | land-queue candidates and trains | `background` | never |
| `interactive` | a person waiting at a terminal, a manual run, an agent's turn waiting on it | `background` | by pausing, if it says it may |
| `batch` | CI for pushes and pull requests, agents' runs | no | if it says it may |
| `background` | schedules, simulations, prewarming, reclaimed room | no | always |

Classes go strictly in order, except that each has a **floor share** of capacity it gets whenever it
has work (by default 5 % for `background`, 15 % for `batch`), and a class passed over for its bound
has its items promoted (§5), so no class starves another. A class short of its floor goes ahead
with an item that keeps it within its floor, and with one that would take it over (a floor smaller
than an item) as often as its floor of the fleet's time allows: on one slot, 15 % is the slot 15 %
of the time. *Integration call (merging into main, to confirm with the person):* `interactive`,
which has no floor or bound by default, is promoted into `land` once the `land` work ahead of it is
due done (§5), in place of the aging between priorities (W28-R65) main had; it preempts nothing.
W05-R09's default priorities map onto classes (a manual run `interactive`, a land `land`, pushes,
pull requests and agents' runs `batch`, schedules `background`) and keep ordering, with their aging,
a tenant's own work within its class.

**Preempting** is **pausing** the grant's group (its CPU and I/O come back at once; its memory stays
held) when that's enough, else **stopping** a restartable item (its attempt ends `preempted`, it's
queued again and keeps its place in fair share). Preemption takes from a class only what it holds
above its floor. An item is preempted at most twice, never in its first minute, and never if it's
`critical` or says it isn't preemptible. Preemption is opt-in per class (§21).

## 8. Grants, leases and fencing

As E3 built it (docs/ci.md W05-R05, R25, R34 to R37 and W10-R03), and as docs/sim.md's scheduler
model checks it, G2 and G10 at every step over a million seeds:

1. **The grant.** The engine makes each grant with a fence: the epoch it's made in (the cluster
   group's term, or one past the last epoch taken if that's greater, so every start and every new
   leader raises it), then its number among that epoch's grants. A later grant's fence is greater,
   and no two share one, not even an attempt granted again after a refusal, which keeps its
   attempt: so a grant is named by its fence, not by its attempt and epoch. The dispatcher writes
   the grant to the store (its runner, attempt and fence, on the leg and on the attempt) before
   anything acts on it (G10); one it couldn't write sends no `Assign`, and its leg goes back to its
   queue. Each grant is a transaction of its own on one disk; written to the cluster group's quorum,
   batched within 5 ms, it is E9's (W05-R05).
2. **The lease.** `Assign` carries the grant, its lease (`[sched] lease`, 60 s by default, at least
   3 s) and what its runner does once it loses it (`on_lease_loss`: `finish`, a CI leg's, or
   `stop`). The runner admits it (§6) or refuses it naming the grant (`Refused`), and a refused
   grant ends in the store as in the engine. The runner renews what it holds three times a lease,
   at most every 10 s (`Renew`, each grant by its fence); the server answers with those that aren't
   its own any more (`Renewed`) and tells it once to stop each (`Stop`). A grant not renewed by its
   expiry is lost (`Lost`), as is every grant of a runner lost (its connection gone past its grace,
   no process of it left) and of a machine's runner process gone; a draining runner takes no new
   grant and keeps its own. A `stop` item's next attempt waits a lock-delay (15 s) past the lost
   grant's expiry.
3. **Fencing, one rule.** What a runner writes about an item (its steps, events, result, refusals,
   and its output, on a lane of its own) is taken only under the item's live grant, that runner's.
   From the grant's loss on, what it writes is refused, counted, and its runner told once to stop
   that grant: nothing it writes after the next attempt starts is taken. The tokens the grant hands
   out (its runtime token, for uploads and ID tokens; its `GITHUB_TOKEN`) are taken by the same
   rule, one lookup in memory, and die with the grant, ended or lost. What kiln can't take back
   lives out its own TTL: an ID token minted while the grant was live, and a registry token a job
   token was exchanged for. A runner from before leases is fenced by runner: what it writes about a
   leg is taken while the leg is its own.
4. **Self-fencing.** The runner keeps each grant's lease by its own clock, which counts sleep
   (Linux `CLOCK_BOOTTIME`, macOS's continuous time, Windows' interrupt time): from when it sent the
   last `Renew` naming it that the server answered, less a tenth (Chubby's local lease). Checking
   every half second, it kills a `stop` item whose lease ended (no cancel's grace, nothing more of
   it run); a `finish` item runs to its end, its writes refused. A runner that is itself paused
   stops nothing: the guarantee is fencing's (G2).
5. **Failover.** A start takes the next epoch and rebuilds from the store (W05-AC09: 10,000 queued
   legs in about 1.2 s, 10,000 running in about 0.5 s). Each grant the store has running comes back
   as it was made, with a lease from then; each queued item comes back at the attempt the store has,
   and a `stop` item queued at a later attempt waits a whole lease and the lock-delay first, since
   the store doesn't keep when its retry was due. A runner back says what it runs (`Hello.running`)
   and renews what it holds: the grants the store has are adopted; anything else it's told to stop,
   and what it sends under it is refused.

## 9. Where it runs: inside p2p's kiln

docs/p2p.md's kiln has no server: every node is a whole kiln, records live in the cluster group,
work that happens once is held by a lease any node may take, and a node cut off from the rest keeps
working on its own. All of that stays (E-D1). Only one thing moves: choosing a leg's runner, from
nodes' claims by compare-and-set to the engine.

- **What a repository decides stays with its lease.** p2p's per-repository `schedule` lease, on any
  node, keeps a repository's runs: their plans, which legs are ready, outputs, retries, cancelling.
  That is most of the work, and it stays spread over the nodes, each repository's beside its records
  (P7).
- **Where work goes is decided where every write already goes.** A ready item is a write to the
  cluster group, and every write passes its leader (docs/p2p.md, "State, by kind"). The leader runs
  the engine on it and writes its grants in the same commit. So placement has no node, lease or
  election of its own: whichever node the group elects places work, decides for the whole fleet,
  and a new leader picks up from the group's rows (§8). The bare engine makes 127,000 decisions a
  second in today's simulator (W05-AC08), so one leader has room to spare once nothing on its path
  waits (§11).
- **Nodes start their own work.** Grants are rows of the cluster group, so a runner finds its own in
  its replica and starts them, as it would have claimed them; what the engine decided reaches every
  node as any record does.
- **Not by compare-and-set.** A node claiming the next job that fits it sees one runner and one
  queue, so it can't keep fair share between tenants, hold room for a big item, start a gang or
  pack. One rule at the one place every write passes can, for the whole fleet.
- **Cut off.** A node cut off from the group keeps what it was granted until it expires (§8) and
  admits local work from its slice (§6), so a laptop on a plane goes on with its own sessions and
  folders; grants for it wait until it's back.
- **Replay is exact.** A new leader rebuilding from the rows, a test replaying a day's inputs and the
  simulator must all reach the same state, on any machine: decisions use integers only (milli-CPUs,
  bytes, milliseconds, fixed-point virtual time), never depend on a hash map's iteration order, and
  never use a function whose result differs between platforms. Decisions travel as rows, so
  replicas never compute them again, and nodes on different versions of kiln agree on them.

## 10. Deployments on the engine

- A sync is an item of kind `sync`. It holds one of its cluster's concurrent-sync units and draws on
  the cluster's request budget, a rate (tokens with a rate and a burst). kiln-cd's order among syncs
  (people, then rollbacks, promotions, automatic syncs, self-heal; docs/cd.md "Fairness") maps onto
  classes and priorities within them; one app never holding more than its share is fair share over a
  cluster's apps.
- Windows, approvals and freezes are conditions.
- kiln-cd still decides what to sync and in what order within an app; the engine decides when,
  against budgets it shares with everything else.
- Applies through jobs (`terraform`, `job`), `script` checks, and host, Compose and microVM
  deployments are legs or runner work as today, now `critical` when they're production's, and
  `on_lease_loss: stop`.

## 11. Fast

| Measure | Target |
| --- | --- |
| Ready to granted, with room | p99 < 50 ms, mean < 5 ms (docs/ci.md §1.5) |
| Grants a second | ≥ 2,000 sustained, mixed kinds |
| Items waiting; runners | 100,000; 1,000 |
| Rebuilt after a failover | < 2 s at 10,000 items; < 10 s at 100,000 |
| An answer's place and estimate | p99 < 1 ms |
| A grant committed | within 5 ms |

Signatures with their eligible runners (W05-R04), each runner's free amounts, a fair-share heap per
class, reservations per runner and a timer wheel for expiries: an input touches only the signatures
and runners it changes. **Nothing on the deciding path waits on the network:** a grant's tokens
(GitHub's, the store's, ID tokens) are made after it's committed, by workers apart from the engine,
which today's dispatcher does inside its one task. Runners report room and pressure as they change,
not every 10 minutes. `kiln ci bench --scheduler` grows to mixed kinds (legs,
commands, sessions, syncs, claims, gangs) and gates regressions as W16's benchmark gate does.

## 12. Correct

- **Invariants as assertions:** G1, G3, G4, G5 and G8, after every input, in every test and
  simulator run.
- **Property tests** over random input sequences against the pure engine.
- **A reference engine:** a slow one, plainly correct (it tries every assignment), for small fleets;
  the real one must decide the same under the same order.
- **A kiln-sim model** (docs/sim.md): the engine, runners, the store's group and the network under the
  nemesis (crashes, failovers, partitions, frames lost, doubled and late, clock jumps). G2, G6 and G10
  hold at every step, and every item has settled once the nemesis ends; a failure replays from its
  seed.
- **Replay:** the same inputs give the same commands on x86-64 and arm64 (G9).
- **Live:** once it's enabled for production (E-D5), W05-AC11: 500 legs use every free slot.

---

# Part II. The agentic layer

Agents use the engine as CI does. What follows is how their requests reach it, what they hear back,
what they may do, and what kiln now does that they did by hand.

## 13. Answers and news

| Answer | When | What the agent reads |
| --- | --- | --- |
| **go** | its needs are met now, and it may | nothing more: the command runs, the push goes, the prompt is answered yes |
| **queued** | it could be met, not yet | `queued: 2nd for 12 CPUs, 40 GiB, linux/amd64 · about 8 min · kiln tells you when it ends (run r12)` |
| **asked** | only a person can say | `waiting for the person: landing on main (card "Train: 3 changes")` |
| **refused** | it can never be met, or it isn't allowed | `refused: agents don't restart runners (a restart ends the sessions on it). Instead: say what the upgrade is; kiln upgrades runners itself (updates.retry).` |

Queued and asked answer at once, and the agent ends its turn. A refusal says what to do instead.
`needs.explain` gives the answer without asking.

**News, not polling.** kiln tells the agent through `harness::told`'s one waiting message of news, as
it tells a parent of its children (docs/agents.md, "Told, not made to wait"): its request granted or
started; a run ended, with its exit code, where it ran, how long it waited and its failure excerpt; a
card answered; a lease about to end; a condition met. An idle agent's next turn starts with it. A goal
waits on a condition, not a timer (`goals_control {pause, until: "machine:+gpu"}`, CLM-3's
`waits_on`); a turn that can't go on without a result calls `requests.wait`.

## 14. Needs agents don't have to say

An agent's request reaches the engine through one of four doors:

| Door | What | Harnesses |
| --- | --- | --- |
| 1. Asked | `run`, `kiln run`, `code_run`'s `kiln.machines.*`, `claims_*`, `agents_change`, `ci.diffs.land`, `grants.request` | all |
| 2. Declared | a shell command that `.kiln/needs.yaml` names goes through kiln: a `PreToolUse` hook rewrites it (`updatedInput`) to `kiln run --needs <needs> --where <where> -- <command>`, so its answer, queued or not, is the command's output | Claude Code; the others where they ask before running |
| 3. A person's | a harness's approval request, decided by grants first (§15) | all that ask |
| 4. Contained | everything else, in its session's group of processes, admitted from the runner's slice and paced by Headroom | all (Linux first) |

The repository declares what its commands need, read from its trunk (docs/ci.md §1.2, principle 4):

```yaml
# .kiln/needs.yaml
commands:
  - match: ci/gate.sh
    needs: cpus=12,mem=40g,linux/amd64,heavy,for=45m
    where: anywhere    # only its result matters, so it may run on another machine
    same: tree         # one per tree: a second ask joins it, or takes its result
  - match: cargo test --workspace
    needs: cpus=8,mem=24g,heavy
    where: anywhere
  - match: bench/**
    needs: quiet,for=30m
```

`where: here` (the default) waits for room on the agent's own machine, for a command whose output
the agent goes on to use there. A command nothing declares gets its request from what it used before
(§5); one with no history is light and runs where the agent is, through door 4. An agent's `kiln run`
that has to wait answers at once with its place (its processes carry `KILN_AGENT`); a person's waits
in their terminal, showing its place. Door 2 is opt-in per repository and per agent; a compound
command goes through whole (`bash -c`), with the largest needs of its parts.

## 15. Permission: grants and policy

**Agents' capabilities are grants:** W10's capabilities and scopes (`diffs.land@repo:kiln`), grown
for what agents do:

- **new capabilities** for acts outside kiln's API: `git.push` (a repository and a ref pattern),
  `claims.quiet`, `claims.hold:<resource>`, `class:<class>`;
- **for whom:** a person (W10), or an agent, its goal stack, its plan run, its program track
  (`program:critical-path/T3`), or every agent of a person on a repository;
- **when:** conditions checked as it acts: `gated` (the tree passed the checks the trunk's review
  policy requires, wherever they ran), `through: land-queue`, `max: 1 change per land`, hours;
- **until:** a time, or the stack, plan run or track ending;
- **by** and **why:** the person who granted it, in their words.

What an agent may do is its person's grants ∩ its own ∩ the trunk's policy; an explicit deny wins
(W10-R02). The person's standing word becomes a grant, which every agent it covers acts on, none
relays, and the person sees and revokes in one place:

```json
{"for": "agent:01a16b2c…/goals", "may": "diffs.land", "on": "repo:kiln@main",
 "when": ["gated", "max: 1 change per land"], "until": "the stack ends",
 "by": "<the person>", "why": "Land each goal yourself once it's gated, one per push."}
```

**One place decides.** Every harness's approval request reaches `Core::add_approval` (Claude Code's
`can_use_tool`, Codex's approval requests, ACP's `session/request_permission`). Before a person sees
one, `kiln_ci::grants::decide(action, grants, policy, facts)` answers it: **allow** when a grant
covers it and its conditions hold (the timeline says which grant); **refuse** when the trunk's policy
forbids it, saying why and what to do instead; **ask** otherwise, on a card. So an agent can stay in a
mode that asks, safer than bypass, and still never wait on a person for what it's been allowed. An
action is classified from its tool call: a shell command by its words (in the syntax of Claude Code's
rules, `Bash(git push origin agents/*)`), an edit by its path, an MCP call by its operation; kiln's
own operations already declare their capability (W10-R05).

**Policy, from trunk,** turns AGENTS.md's hazards into rules kiln holds agents to:

```yaml
# .kiln/policy.yaml
forbid:
  - match: ["kiln install *", "kiln join *"]
    why: agents run through runners, and a restart ends their sessions
    instead: say what the upgrade is; kiln upgrades runners itself (updates.retry), or the person does
  - match: ["pkill -f *", "pgrep -f *"]
    why: other sessions run the same commands
    instead: stop what you started by its PID, or let your lease end and kiln reaps it
  - match: ["git push * main", "git push * HEAD:main"]
    why: main moves only through the land queue
    instead: agents_change, then ci.diffs.land
```

A forbidden act is refused in every mode, bypass included: in Claude Code by a `PreToolUse` hook on
`Bash` (kiln already holds subagents to their limit this way), in the others where they ask; kiln's
own CLI also refuses `install`, `join` and replacing itself inside an agent's group of processes. A
harness that doesn't ask in its mode can't be stopped before it acts: for it, the policy is in its
instructions and what it did is in the audit.

**Cards ask once.** A card is one decision however many agents wait on it ("Train: 3 changes,
gated"), answered once or as a standing grant with the scope the person picks; an agent that wants a
standing answer proposes one (`grants.request`), on the same card. Every merge to main, push, deploy
and new repository still needs the person's word (AGENTS.md), given as a train's approval or a grant,
so no agent carries it to another.

## 16. Coordination

- **Machines and resources** held across several commands are claims (CLM-2, CLM-3), items of kind
  `claim` on the engine: `claims_request {needs: "quiet,for=60m"}`.
- **main** moves through the land queue (CLM-5, CLM-6): agents enqueue; kiln builds candidates on
  main's tip, gates them, batches, bisects and lands; the person approves a train once (CLM-8).
  Freezes are holds (CLM-7). Candidates are `land` items.
- **Order between agents' work** is a need, not a message: `after:land:C42`. A change request on
  another's branch is a stack (W14): it lands after its base, kiln restacks it when the base moves,
  and its author is told.
- **Leftovers belong to a lease.** A lease owns what its work makes: its processes (a cgroup, job
  object or process tree of its own); its containers and networks, made through a Docker socket of
  its own (the proxy a `docker` leg already gets in its daemon's place, `runner::docker_sock`, taught
  to label what it creates `kiln.lease=<id>`); its systemd user units (`kiln-lease-<id>-…`, by a
  `systemd-run` on its PATH); and its scratch folder, `$KILN_SCRATCH` under
  `~/.cache/kiln/leases/<id>` (`TMPDIR` too, so big files never land in `/tmp`). When the lease ends,
  kiln reaps them (opt-in); what no lease owns is listed on its runner's page.
- **An agent that went away** lets go of its leases when its session ends. A plan step whose agent
  ended with its goal unmet goes back to its run, retried by the run's policy (opt-in) or put to the
  person. A goal handed to an agent carries what its stack's owner may do with that goal's objects
  (its plan's `plans.ready`, its research's `research.add`), so finishing another's step isn't
  refused with a 403.
- **What stays conversation:** interfaces, names, reviews, questions about each other's code (CLM's
  point). Opt-in, `agents_send` answers a message about allocation with where kiln keeps that now
  (CLM-10).

## 17. Load, as agents see it

- Agents see places and estimates, not load: answers and `runners` say what suits and where they'd
  stand; load, pressure and fingerprints come with `detail: true`, for a person looking into a
  machine.
- A run whose machine went past its budget while it ran is marked `contended`, a failure class
  W05-R14's `on:` can name, so a retry policy (opt-in) runs it again and the agent sees one result.
- A `quiet` result carries what else ran on the machine (by `KILN_AGENT` mark) and the load while it
  ran, for a benchmark's record.

### 17.1 What a runner measures, and when

A runner owns its capacity: it decides admission from its own measurements, read when it decides
(`crate::sense::Sensor::measure`), never from a timer. The runner's 5-second sample feeds its page
and the server's charts; placement doesn't use it (legacy placement reads what a runner last said,
however old, until the new protocol replaces it).

- **What one read gives** (each share exact over the interval since the previous read, any
  interval; each level as read): CPU idle share (`/proc/stat`, Mach's `HOST_CPU_LOAD_INFO`,
  `GetSystemTimes`), memory available (no more than the runner's cgroup leaves), the work
  directory's free space (APFS's important-use figure too), pressure (Linux PSI `some`/`full` for
  CPU, memory and I/O, the machine's and the runner's own cgroup's, from their cumulative stall
  times; macOS's memory pressure level) and the work device's throughput, latency, queue and
  errors from the OS's own counters. A read costs microseconds and starts nothing.
- **What the work volume is** (`RunnerFacts.storage`, probed with the other facts, unprivileged):
  file system, mount, source, network or not, and each device under it with its kind by
  interconnect before medium flag (NVMe, SSD, HDD, network, virtual, RAM), its model, size and,
  where an unprivileged reader can have it, NVMe health.
- **What each run did** (`RunnerFacts.runs`, sent as the run ends): wall time, CPU time (its cgroup,
  job object, or `wait4`'s tree), I/O bytes and operations, its share stalled on I/O, the device's
  latency and throughput over it, the idle CPUs it started with, and the probability its
  placement had. The server folds each into its key's sufficient statistics and keeps the newest
  `ROWS_KEPT` rows per key (the model's terms and one): no age bounds it.
- **The learned run time** (`kiln_ci::runtime`): per key, `wall ≈ θ0 + θ1·cpu_s/idle_cpus +
  θ2·bytes/throughput + θ3·ops·latency`, θ ≥ 0 by non-negative least squares (Ernest, NSDI'16),
  rows weighted by their placement's inverse probability (Bottou et al. 2013), with a posterior a
  placer draws from (Thompson sampling) instead of a fixed exploration rate.
  `Model::predict(key, facts, now)` is the place core's prediction input.

## 18. Machines away

- An item only an offline machine could hold waits (`no-runner`, naming it). The person gets one card
  per machine, listing what waits; agents end their turns and are told when their work starts.
- A runner connected but answering no run (a reach probe that times out) is **wedged**: the engine
  stops placing work there and says so. Opt-in, kiln repairs it (`runners.repair`) when no agent
  session runs there, or an agent attached to runners does within its grants (docs/agentic.md, R4);
  otherwise it's on the card.
- A full disk is a fact (`disk.free`) and a budget: work that needs disk goes elsewhere, and the
  machine's card says why.

## 19. Agents as holders, and as operators

- **Goals go to agents by the same engine.** A goal or a plan step is an item of kind `goal`; an
  agent is a holder with capabilities (its harness, model, machine and grants) and capacity (one goal
  at a time, its account's usage window as a rate that refills). Programs' slots and track weights
  become the tenant tree's weights, and their usage rules (no new goal past 80 %, pause past 95 %:
  docs/programs.md) become the window's budget. The goal driver hands out what the engine grants.
- **Agents operate kiln within grants.** Attached agents (docs/agentic.md, K and R4) wake on the
  engine's events (a wait past its bound, a wedged runner, a full disk, a tenant short of its share)
  and act within their capabilities.

## 20. What agents are told

The machines paragraph of kiln's MCP instructions (`reach::mcp::instructions`) becomes:

> Say what work needs; kiln decides where and when. Run builds, tests, gates and benchmarks with
> `run` (or `kiln run` in your shell), with `needs` when the repository doesn't declare them. Don't
> pick a machine for its room, check a machine's load, or wait for one: kiln holds the room, runs the
> work where it fits, and when it has to wait, answers at once with its place and tells you when it
> ends; end your turn, and don't report waits to the person (kiln shows them). To hold something
> across commands (a machine quiet for a benchmark, a named resource), `claims_request`. Don't
> message other agents to arrange machines, gates, main or deploys, and don't ask the person in prose
> for something kiln decides: do it, and kiln lets it through, asks the person on a card, or refuses
> and says why. What your work makes (processes, containers, units, files in `$KILN_SCRATCH`) is your
> lease's, and kiln clears it when the lease ends.

The list of machines stays, for steps that have to happen on a particular one (the person's Mac),
without "this machine first". The `kiln-agents` skill and a new `kiln-claims` skill say the same at
length. AGENTS.md's hazards move into `.kiln/policy.yaml` and `.kiln/needs.yaml`, and AGENTS.md
keeps why; changing it is the person's call.

---

# Part III. Building it

## 21. Opt-in

Each decision kiln makes on its own is off until declared, and says why it did what it did
(docs/ci.md §0.3):

| Decision | Declared by | Proposed |
| --- | --- | --- |
| reservations for big items (backfilling) | `[sched] backfill` | on: without it G6 doesn't hold |
| preemption | `[sched.classes.<class>] preempt` | pausing `background` on; stopping off |
| reclaiming held, unused room | `[sched] reclaim` | off |
| an agent's command waits in line instead of failing | `[reach] queue` | on: it's what the agent asked for |
| declared commands go through kiln (door 2) | `.kiln/needs.yaml`'s `intercept: true`, and the agent's setting | on for kiln's repository |
| an approval answered from a grant | the grant | each grant is a person's declaration |
| a forbidden act refused | `.kiln/policy.yaml` on trunk | on for kiln's repository |
| what a lease made, reaped as it ends | `[leases] reap` | on |
| a contended run tried again | a retry policy's `on: [contended]` | off |
| a plan step whose agent ended, tried again | the plan run's `retry` | off: put to the person |
| a wedged runner repaired | `[runners] repair` | off: on the card |
| a message about allocation answered by kiln | `[agents] deflect` | off (CLM's D7) |

## 22. In code

- **kiln-ci, pure** (`handle(Input, now) -> Vec<Command>`; no clock, disk, network or randomness of
  its own):
  - `kiln_ci::sched`: the engine, every kind of item; integer amounts; classes, the tenant tree,
    reservations, gangs, conditions, keys;
  - `kiln_ci::sched::needs`: the grammar of §3;
  - `kiln_ci::claims`: named and rate resources, fencing tokens, lock-delay (CLM-2);
  - `kiln_ci::headroom`: Headroom's window and pacing plans, moved from `crates/kiln/src/headroom`;
  - `kiln_ci::grants`: `decide`, and matching actions (§15);
  - `kiln_ci::coord`: the meter's classifier (CLM-1, with `load` and `permission` classes).
- **The kiln crate:** the driver feeding the engine and carrying out its commands, for every kind
  (`ci::dispatch` grown); reach asking the engine before `reach::directory::pick`, which becomes the
  fallback when the engine can't be reached; agent sessions placed through it rather than
  `harness::lineage`'s ranking; kiln-cd's syncs submitted to it; news through `harness::told`; grants
  in `Core::add_approval`; the `PreToolUse` hook beside `SUBAGENTS_HOOK`.
- **The runner:** admission (§6), its slice, holding grants to their requests (§3's table), pausing
  for preemption, renewing and self-fencing (§8), labelling and reaping what leases make.
- **SQLite,** numbered migrations: grants as leases (epoch, attempt, expiry, amounts), reservations,
  `claims`, `lease_owned`, agents' grants (W10's, with `for`, `when`, `until` and `why`), `cards`,
  `conditions`; all through the cluster group (docs/p2p.md).
- **The protocol** grows at its end: `Assign` carries the epoch and expiry (in its JSON, or a new
  variant after every existing one), renewal rides the heartbeat, and runners announce `CAP_LEASES`
  (they hold grants to their requests, self-fence, label and reap). An older runner's grants are
  still counted and fenced at the store; its answers say nothing was capped.
- **Operations** (one registry: HTTP, MCP, code mode, CLI): `needs.explain`, `requests.wait`,
  `claims.request|get|list|release|extend` (people's: `claims.reserve|revoke`,
  `claims.resources.put`), `grants.request`, `grants.mine`; `run`, `kiln run` and `machines.run` take
  `queue`, `where`, `for` and `same`.
- **The UI** (CLM-9; hyperlight-site style, one surface per page, no popups): an item's page and an
  agent's row say what it waits for or holds, with its place and estimate; a runner's page, its
  budget, room, grants, reservations and queue; Settings, every standing grant, each revocable.

## 23. What this changes elsewhere

| | |
| --- | --- |
| W05 | grows: every kind of item, integer amounts, classes with floors, W05-R09's aging kept within a tenant and across classes but never between tenants, a tenant tree with dominant-share costs (W05-R10), reservations and gangs, grants as fenced leases |
| v1's dispatcher | `Hub::schedule_once` retired, as W05 already plans, when the engine is enabled for production (E10) |
| docs/p2p.md | per-repository `schedule` leases kept for what a repository decides; legs placed by the engine at the cluster group's leader, where every write goes, in place of claims by compare-and-set |
| docs/reach.md | commands placed and held by the engine; the asking runner's own pick only when the engine can't be reached |
| docs/cd.md | syncs on the engine, budgets shared with everything else |
| CLM | its claims (CLM-2, 3) are items of kind `claim`; budgets (CLM-4) are §6; the gate workflow, land queue, holds and trains (CLM-5 to CLM-8) are kept as written; its views and conventions (CLM-9, 10) take in Part II |
| Headroom | its window is the heavy dimension (§6), its pacing door 4 (§14), its core in kiln-ci |

## 24. Decisions for the person

CLM's D1 to D7 stand as that plan recommends unless the person says otherwise. New:

- **E-D1, decided: p2p's kiln kept.** Per-repository leases keep what a repository decides; the
  engine places work at the cluster group's leader, where every write goes; nodes start the grants
  they find in their own replicas; no claims by compare-and-set (§9).
- **E-D2, decided: both.** The five classes with their floors (5 % `background`, 15 % `batch`), and
  W05-R09's aging, each where it can't undo the other: aging orders a tenant's own work and promotes
  an item that waited past its class's bound; between tenants in a class, fair share alone (§5).
- **E-D3, preemption.** Pausing `background` for `critical`, `land` and `interactive` (recommended);
  stopping restartable `batch` items to free memory, off until asked for.
- **E-D4, budgets.** A budget for every runner, and production's machine's (CLM's D4).
- **E-D5, decided: enabled directly.** No shadow week: once E1 to E9 pass, the engine is turned on
  for production's CI and v1 is retired (E10).
- **A-D1, wait in line by default.** An agent's command waits in kiln rather than failing
  (recommended), or only when it asks to (`queue: true`).
- **A-D2, intercept.** Claude Code agents' declared commands go through kiln without the agent saying
  so (recommended for kiln's repository), or only what agents send through `run`.
- **A-D3, standing grants.** The scopes a card offers (once, this agent, its goal stack, its program
  track, this repository's agents) and the default end (when the stack or plan run ends, else 7 days);
  only a person grants (recommended).
- **A-D4, policy held.** AGENTS.md's hazards as `.kiln/policy.yaml`, refused in every mode
  (recommended), or advice the agent is given but not held to.
- **A-D5, reaping.** What a lease made goes when it ends (recommended); leftovers no lease owns are
  only listed.
- **A-D6, repair.** kiln, or an attached agent, repairs a wedged runner when no session runs there,
  or it only goes on a card (recommended to start).

## 25. Order of work

Part I first, each step leaving the engine shippable and checked; Part II on it. E1 and A1 can start
at once; A1 only measures.

| Step | | Done when |
| --- | --- | --- |
| E1 (built) | The ground: integer amounts, no decision on hash-map order, the replay test, the reference engine, the kiln-sim model's skeleton | the same inputs give the same commands on x86-64 and arm64; the reference and real engines agree on 10,000 random small cases |
| E2 (built) | Fair share that holds: classes with floors and promotion, W05-R09's aging within a tenant, the tenant tree, dominant-share costs | G7 with staggered arrivals and mixed shapes; W05-AC03 within a tenant; a `background` item under a steady stream of `batch` starts within its bound |
| E3 (built) | Grants as fenced leases end to end: epoch and expiry, renewal, runners' loss and draining told to the engine, self-fencing, the store refusing stale writes, tokens that die with their grant, reconciling after failover | G2 and G10 in the simulator under the nemesis, a million seeds; live, a runner cut off past its lease: nothing it writes after the next attempt starts is taken, and its tokens no longer work |
| E4 (built) | Reservations and gangs | G6: a 12-CPU item among a stream of 1-CPU ones starts by its bound; a gang of 3 never half-starts |
| E5 | Every kind on the engine: commands and sessions held, slices for local work, budgets, rooms measured, heavy units from Headroom | an agent's command with no room is queued, not refused; CI legs, commands and sessions on one runner never exceed its budget together (G1) |
| E6 | Estimates and keys: requests from history, the same work once, places and estimates in answers | a second gate for the same tree runs nothing; estimates within 25 % at p50 over a day's work |
| E7 | Preemption by pausing, and stopping restartable items | `critical` work waiting on `background` starts within 10 s; no item preempted more than twice |
| E8 | Named and rate resources, claims (CLM-2), deployments' syncs on the engine | CLM-2's invariants; a cluster's budget never overdrawn by syncs and applies together |
| E9 | Placement at the cluster group's leader (§9): ready items and grants as the group's rows, runners starting their own grants from their replicas, a new leader picking up from the rows; nothing on the deciding path waiting on the network | the leader stopped mid-stream: the next places work within 2 s at 10,000 items over 1,000 runs, nothing granted twice; ready to granted p99 under 50 ms with GitHub slow |
| E10 | The engine enabled for production's CI, v1 retired (as W05 already plans), W05-AC11 live | production's legs assigned by the engine; `Hub::schedule_once` gone; 500 legs use every free slot |
| A1 | The meter: CLM-1, with `load` and `permission` | §1's counts given again, within 5 % |
| A2 | Answers and news: queued answers, news for runs, claims, cards and conditions; goals waiting on conditions | an agent's gate with no room answers `queued` at once, and the agent is told when it ends with no call between |
| A3 | Needs agents don't say: `.kiln/needs.yaml`, door 2, learned requests | `ci/gate.sh` typed in a Claude Code agent's shell waits in kiln's line with its declared needs |
| A4 | Grants and policy | an act under a grant is answered in under 1 s with no card; a forbidden one is refused for a Claude Code agent in bypass mode, with its `instead` |
| A5 | Coordination: claims for agents, leases owning what they make, the land queue, holds and trains (CLM-3, CLM-5 to CLM-8) | a lease's containers gone within 30 s of its end; three agents' changes land as one train after one approval, with no message between them about it |
| A6 | Machines away and agents gone | work for an offline machine starts within 10 s of its return, with one card while it waits; a wedged runner gets no new work within 30 s |
| A7 | Goals on the engine; agents operating within grants | programs' tracks keep their weighted shares with no coordinator deciding slots |
| A8 | What agents are told, the policy files, the views | the views pass in Chromium and WebKit at desktop and phone widths; `cargo test -p kiln --test agents_md` passes |
| A9 | A week, measured against §1 | load probes by agents down 90 %, coordination messages down 80 % (CLM-10), go-aheads asked in prose down 90 %, and no hand-made lock, waiter or retry loop |

**E1 is built** (branch `needs-engine`, 2026-10-26; docs/ci.md W05-R29 and W05-AC13). The engine
decides in integers (`kiln_ci::sched::units`) and walks what it keeps in one order; its replay gives
one digest on arm64 and x86-64; a reference scheduler written from W05 agrees with it on 10,000
cases; kiln-sim's scheduler model (docs/sim.md) is E3's skeleton. On the way, the reference and the
model found three flaws in the engine, each fixed with its test: the legs a runner lost together
were given back one at a time, out of queue order; a leg lost with its runner waited for good for
another runner too small to hold it (docs/sim.md's bug 10); and a leg refused by the only runner big
enough to hold it said it waited for capacity.

**E2 is built** (branch `needs-engine`, 2026-10-28; docs/ci.md W05-R30 to R33, W05-AC14 and AC15,
W05-R09, R10 and AC03 amended). Every leg has a class, set from its event or intent; classes go in
order with floors and promotion; fair share runs over a tree of tenants for each class, costed by
dominant share × time, charged at dispatch and settled at the end; W05-R09's aging orders only a
tenant's own work. Building it changed three of its rules, each for a test it failed:

- **Promotion is for a class passed over:** promoting every item that waited its bound lifted a
  saturated class's own backlog past other tenants (a late actor got 15 % of its repository, not
  50 %).
- **A floor smaller than an item is paced:** on one slot a 15 % floor took the slot every time it
  freed; pacing every item a floor lets through starved it when items ended together (5 % of 20
  slots, not 15 %), so only an item that takes its class over its floor is paced.
- **Fair share's tags are kept per set of dimensions**, not per dimension (§5).

Its limits are named in W05-AC14: small items refilling the room a big one needs (1-GiB legs
against 8-GiB ones where memory runs out first) are E4's, measured at a third rather than half.

**E3 is built** (branch `needs-engine`, 2026-10-30; docs/ci.md W05-R34 to R37 and W05-AC16,
W05-R05, R25 and W10-R03 amended; §8 says how it works). Grants are fenced leases end to end: the
engine makes each with a fence and a lease, the epoch the store group's term, raised by every start
(E3.1, E3.7); leases go on the wire, the runner renewing what it holds and naming a leg's grant in
what it sends (E3.2); the server refuses what a grant writes from its loss on, and its tokens die
with it (E3.3, E3.4); the runner keeps its own leases by a clock that counts sleep, and kills a
`stop` item at its lease's end (E3.5, E3.9); every runner's loss and drain reach the engine, by the
process that held what (E3.6); and a restart rebuilds the grants it finds running and reconciles
what runners hold (E3.7). What it measured:

- **G2 and G10 over a million seeds:** docs/sim.md's scheduler model, with the server crashing,
  runners paused past their lease and asleep, and frames lost, doubled and late, holds both at every
  step and settles every leg, seeds 0 to 999,999 in release: 0 failed. On the way it found three
  bugs (docs/sim.md's 11 to 13). Four planted ones are each caught within the 200 seeds `cargo test`
  runs.
- **Live (W05-AC16):** `w05_ac16_a_runner_cut_off_past_its_lease_writes_nothing_after_the_next_attempt_starts`
  and `w05_ac16_a_stop_grant_cut_off_stops_itself` (`crates/kiln/tests/leases.rs`), on Linux and
  macOS: a runner paused past a 5 s lease has nothing it writes taken once the next attempt starts
  on another, its tokens refused; a `stop` item's process is gone 315 to 335 ms after its runner's
  lease end.
- **W05-AC09, grown to running legs:** 10,000 running legs rebuilt through a restart, each as the
  grant it ran under and none granted again, in 516 to 550 ms.
- **W05-AC08 against E2:** E3's bench (its grants leased, 60 s) and E2's at 17710407e, run by
  turns on one Mac beside its other work, 160 rounds each, three runs a round: 46,946 assignments/s
  at the median against E2's 47,494, 1.2% fewer (a bootstrap's 95% interval: 4.0% fewer to 1.8%
  more). Each build's runs spread from about 41,500 to 52,000 between their quartiles, so a few
  rounds can show a gap of 6% either way; E3 retires 1.6% more instructions.
- **The gate:** `ci/gate.sh` through `kiln run --needs cpus=12,mem=40g,linux/amd64`, at E3.9's
  commit with these docs: clippy for Linux, Windows and macOS with `-D warnings`, and `cargo test
  --workspace`, 3,147 tests passed in 110 binaries, 0 failed, 34 ignored. `GATE: PASS`.

Building it changed five of its rules, each for a test or a seed it failed:

- **A grant is named by its fence, not by its attempt and epoch:** a refused grant keeps its
  attempt (W05-R02), so a refusal that came late could have released the leg's next grant of the
  same attempt.
- **A runner keeps a lease end per grant, not per connection:** a reconnect's hello renews nothing
  granted before it.
- **Renewals follow the lease** (three times a lease, at most every 10 s), not the heartbeat's
  10 s, which held a lease to 20 s at least: the live test's 5 s lease is renewed every 1.7 s.
- **A `stop` item past its lease is killed, not cancelled:** GitHub's cancel gave its step up to
  10 s more to act.
- **A restart keeps a queued leg's attempt, and a `stop` item's wait** (docs/sim.md's bugs 11 and
  13): the store held the attempt, the engine began each leg at its first; and a lock-delay kept in
  memory went with the process.

Its limits: grants aren't batched within 5 ms (W05-R05), nor written to the cluster group's quorum
(E9's). An ID token minted while its grant was live, and a registry token a job token was exchanged
for, live out their TTL. A leg in a microVM is stopped as a cancel stops it. A runner refuses a
`Stop` naming a grant of an older epoch than it has seen, though a newer server sent it: such a
`finish` leg runs to its end, its writes refused, and a `stop` one still ends at its lease's end. A
restart forgets a leg's retry backoff, which the store doesn't keep.

**E4 is built** (branch `needs-engine`, 2026-11-01; docs/ci.md W05-R38 to R40, W05-AC17 and AC18,
and W05-R13's `reserved`, `behind` and `gang`; §5 says how it works). A grant keeps two ends beside
its median one: its expected end at its p90, moved on once it passes, and its hard end at its
timeout (E4.1). With `[sched] backfill` declared, the first of each class that fits no runner keeps
room where it frees first, and only what ends before then, or fits beside it, is granted there
(E4.2). `together:<name>/<n>`, and a job's `together: true`, make items a gang that starts whole or
not at all, granted in one input and, through the dispatcher, written in one transaction before any
of its Assigns goes (E4.3, E4.4). The reference engine keeps room and gangs plainly (E4.5), and
kiln-sim's model draws big items and gangs under its nemesis (E4.6). What it measured:

- **W05-AC17 (G6):** in kiln-ci's simulator, a 12-CPU leg among a stream of 1-CPU legs on one
  16-CPU runner starts by its bound in each of 1,000 seeds; without backfilling, it doesn't start
  within three hours.
- **W05-AC18 (G4):** a gang of three among competing work, a member refused in a third of the seeds
  and a runner lost in another third: no grant takes part of it in any of 1,000 seeds. Room was kept
  for its members 5,211 times, 3,387 of them standing until it started, each start by its bound.
  Through the dispatcher, a matrix of three with `together: true` starts within a second of itself;
  with kiln's `chaos` faults, a refused member has its gang stopped and run whole after, and a gang
  whose grants the store can't write is heard of by no runner.
- **G4 and G6 over a million seeds:** docs/sim.md's scheduler model with big items and gangs,
  backfilling on, under its nemesis, seeds 0 to 999,999 in release on Linux at 3edf8394e: 0 failed.
  Room was kept 4,389,232 times, 2,640,214 of them until their item started, and 1,522,310 gangs
  started whole. On the way, a release probe of its first 125,000 seeds and a first million found
  five bugs (docs/sim.md's 14 to 18), which the 200 seeds `cargo test` runs had missed. The two bugs
  planted (a backfill past the room kept for another; a gang started without its last member) are
  each caught within those 200.
- **The reference engine:** 0 of 10,000 cases part from the real one, room made, moved and worked
  out again, backfills granted and refused, and gangs granted, kept for and taken back whole among
  them. On the way it found five bugs (E4.5).
- **W05-AC08 against E3:** E4's bench and E3's at 3e176d5b4, each build's binary run by turns in
  ABBA order on one Mac, 160 rounds, three runs a process: 74,098 assignments/s at the median
  against E3's 76,314, 2.9% fewer (a bootstrap's 95% interval: 2.1% to 3.7% fewer); E4 retires 1.2%
  more instructions. Backfilling is off there, and every leg fits every runner, so no room is kept.
- **What room kept costs:** the mixed bench (`--bench sched_mixed`), 250 runners of 16 CPUs and
  10,000 legs, one in twenty asking 12 CPUs, backfilling on: 64,475 to 80,524 assignments/s over
  nine runs, room kept 162 times a run. W05-R26 asks 2,000.
- **The views:** a leg waiting `reserved`, `behind` or `gang` says so in a sentence on its run, and
  the queue's breakdown names each, in Chromium and WebKit at desktop and phone widths
  (`ui/e2e/wait-reasons.spec.ts`).
- **The gate:** `ci/gate.sh` through `kiln run --needs cpus=12,mem=40g,linux/amd64`, at
  dc020b20f (the commit before the one adding this line, which changes nothing else): clippy for
  Linux, Windows and macOS with `-D warnings`, and `cargo test --workspace`, 3,185 tests passed in
  111 binaries, 0 failed, 34 ignored. `GATE: PASS`. A run before it on the same tree failed four
  tests that its machine, loaded and with Docker's address pools full, failed and this run passed:
  a timing test's p99 at 2.57 ms against its 2 ms, two that make Docker networks, and an account
  action's dry run (`w22_ac02_…`).

Building it changed nine of its rules, each for a test or a seed it failed:

- **Room worked out again as it stood changes nothing:** a runner picked forever once a grant ran
  past its hard end (E4.5).
- **A runner whose kept room ends takes what fits it before the input is done** (W05-R04): room
  dropped as its item went elsewhere left its runner idle beside the legs it had kept out (E4.5).
- **A gang's members share a class,** promoted together: promoted one by one, they split across
  classes, and the gang's room was dropped and made again for good (E4.5).
- **Room is kept only on a runner its item may be routed to** (E4.5).
- **A gang waits for room by room alone,** the room kept for others aside: two whole gangs of a
  class, each with room only beside the room kept for it, took that room from each other for good,
  the engine never done with one input (docs/sim.md's bug 14).
- **A start that comes back from a restart with fewer members than make its gang has had one
  end:** a member lost after was tried again alone, and waited for good (bug 15).
- **Room goes once its runner wouldn't take its item now:** a leg lost on a runner stayed off it
  once another was back, but its room there held others off (bug 16).
- **A gang kept off the runners that refused its members has those refusals forgiven** when it has
  no place without them (bug 17).
- **A member refused or not granted once its start has had an end ends failed, `infra`,** rather
  than waiting for good for a gang that can't be whole (bug 18).

Its limits: preemptible items take no kept room until runners can pause them (E7), and commands
can't be one of a gang until the engine places commands (E5). Room kept isn't stored: a restart
works it out again from the grants it rebuilt. A gang bigger than every runner's room together waits
saying `capacity`, with no reason of its own. A lone leg that a runner refused stays off that runner
for its attempt, and waits for another if that was the only one that could take it
(`other-runner`): only a gang's refusals are forgiven. The end-to-end tests of a refused member and
of a store that can't write a gang's grants need kiln's `chaos` feature, which the gate doesn't
build. W05-AC14's limit wasn't measured again with backfilling on.

**E5, begun (branch `lb/core`, 2026-11-04): one engine, decided where the room is.** A runner's own
admission (`kiln_ci::place::runner`) decides by the engine's policy, with the engine's own pure
functions (`sched::state`'s `own_order`, `shares`, `charges`, `held_share`, `pace_ms`, `leaf_of`,
`nodes`, `Due`), over its own line (`place::line`, a treap O(log n) for every change and question,
now keyed as the engine's queue is: class, the tenant's standing level by level, its own aged order,
arrival). Classes go in order with their floors and pace; a run waited past its bound while its class
started nothing here goes up a class; an `interactive` run goes up into `land` once the `land` and
`critical` work that came before it no longer waits, at that work's latest expected end (§5's
integration call); fair share is DRF over the tenant tree, charged as a run is offered and settled to
what it ran. Room is the engine's rule (W05-R03: `exclusive` whole or a share), against what the
runner measures, never past its budget. Each look at a promotion is a timer the runner asks for. Held
to the reference as the engine is: `sched::reference::one_runner`, 10,000 one-runner cases (every
class, floors and bounds drawn, aging, two and three levels of fair share, whole runs and shares,
draining), zero disagreements. What a runner's admission doesn't do of the engine's: room kept is for
its first in line (the engine keeps it for each class's first across the fleet), gangs, leases, and
where a run goes (the owner's, `place::owner`). A tenant's standing moving re-keys its waiting runs,
O(k log n) for the k of the class charged whose tenants moved, with fair share deeper than repositories
re-keying a repository's leaves whose parent moved: the line's one cost above O(log n).
