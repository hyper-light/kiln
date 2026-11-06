# The persisted tail

CP-T2.2 (kiln plan `01a16c5b7e8c75acbdfafd51315c3885`) fixes the persisted class's tail at its
cause. Step 1 traced the tail to its cause and built the reproduction, in simulation and on a real
disk, that later steps are measured against. Step 2, at the end of this page, judges each node's
process apart from its disk, so that a stall of the leader's disk spends no term and no publish
waits it out. Cheap elections, the three-machine lab and the comparison are steps 3 on.

## What Focal measured

Focal's doc 34 (`docs/34-replicated-part.md`, status 2026-10-18) ran the persisted class, three
nodes on the bare-metal machine (a 9950X3D2, each system's three nodes in Docker, 128-byte
records, open loop): p50 / p99 / p99.9 of 33.2 / 1,404 / 1,674 ms at 1,000 a second, "a tail of
1.4 s at 1,000: ... a shared disk's stall electing, open". Its account: "each node's log flush
took 584 to 587 ms in the same second, the bare-metal machine's one disk stalling under all three
nodes, so no node's heartbeat was proved for 0.6 s and each node suspected both others; the
leader, left no trusted quorum, stepped down, and won the next term. Each detector stood at 165 to
216 ms of detection ... where the step-down cost a second."

Focal's agent added how it happened (2026-10-25). The bare-metal machine is adacachy, kiln's
production host (an AMD Ryzen 9 9950X3D2, its root and `/home` on one btrfs volume: `compress=zstd:1`,
`ssd`, `discard=async`, no `commit=` option). Nothing held the flushes. The volume stalled about 0.6
s every 20 to 30 s, for all three nodes at once, their logs on its one disk. The step-down was traced
natively at 10,000 a second, three `focal start bus` processes with data on that btrfs, with each
member's statistics a second and the stream's suspicions. That the stalls are btrfs's periodic
transaction commit is inferred, not measured: with no `commit=` option the interval is btrfs's
default of 30 s. It matters past this step. Quiver's persisted class is to carry kiln's CI and
deploy events on that host. A 0.6 s stall every 20 to 30 s holds 2 to 3% of all publishes, those
offered during a stall, for up to 0.6 s. Alone, that puts the p99 near 0.3 to 0.4 s there,
whatever the elections do: one such stall in 30 s gave 405 to 416 ms in the real-disk runs below.
AC-T2.2.2's bar is 10 ms.

## The cause

Doc 34's run was built on focal 7a79f0d7 (2026-10-18 14:09), which vendors hyper-raft b5ea01ba
(09:12). There a node's heartbeat leaves only with a proof that its log made a write durable since
the previous was due, and a judge held each peer to its freshness whatever its own disk did. A
stall every node shares stops every proof, so every judge suspected both peers once their freshness
points passed, and the leader, suspecting both followers, had no trusted quorum and stepped down.
Three hyper-liveness commits after the run hold such a stall blameless:
- 7959df9 (2026-10-18 16:40): a node whose own log stalls does not blame its peers, Lifeguard's
  local health (Dadgar, Phillips and Currey, "Lifeguard: Local Health Awareness for More Accurate
  Failure Detection", DSN-W 2018, arXiv 1707.00788) applied to the flush proof;
- 1ccd38d (2026-10-20): a judge whose own flush proof is outstanding judges no peer until it comes;
- 2b13b15 (2026-10-20): such a judge shows its waiting pairs.

Focal's agent placed doc 34's run before all three (2026-10-24, `merge-base --is-ancestor`), and
Quiver vendors hyper-raft 25cf889, which has them.

## The trace

hyper-durable's liveness harness (`tests/liveness.rs`) runs three nodes, each an `Owner` of a
`Replica` over a simulated store and a node-pair `Liveness`, under a publish a millisecond, and
stalls a node's device: every write out on it, or given it before the stall's end, is durable at
the end and not sooner, as every file's fsync on a volume waits on its journal's commit. Seed 0 at
b5ea01ba, doc 34's stall (each device held from 0 to 584, 585 and 587 ms), freshness 61 ms, n3
leading term 1, in ms from the stall's start:

| ms | What happened |
|---|---|
| 0 | Every node's device stalls. |
| 0 to 44 | The last proved heartbeats leave, two a node. Each node's next is due unproved, and its stream asks a write for the proof: n3 at 32.3, n2 at 43.8, n1 at 44.3, each held to its device's end. |
| 62.8 to 81.1 | Each judge's freshness points pass with no heartbeat. n1 suspects n3 at 62.8, n3 suspects n2 at 72.0, n2 suspects n1 at 73.3, n3 suspects n1 at 76.0, n1 suspects n2 at 76.8, n2 suspects n3 at 81.1: every node suspects both peers. |
| 76.0 | n3, suspecting both followers, has no trusted quorum and steps down (term 1, following). |
| 76 to 584 | Nothing happens on any node. No leader, and no election can run: a campaign's term and vote must be durable, and the replica takes no Ready while its writes wait on the device. |
| 584 to 587 | The devices answer. Each node's writes are durable, its stream's proof with them. |
| 586.1 | n1 asks for pre-votes for term 2, and n2 at 587.1. |
| 589.1 | n3, the old leader, rejects them. |
| 589.3 to 591.5 | n1 asks for votes for term 2. n2 grants its vote at 591.5, and n1 leads term 2. |

So the 0.6 s without a proved heartbeat is the device's stall itself. The proof is a durable
write, so no thread of the member's own can make one while the device holds every write. No
heartbeat waited on a transport lane or a shard: each waited on its proof. The publishes offered
from a second before the stall to a quiet period past it had p50 3.1, p99 513.6, p99.9 595.1 and
max 596.1 ms, and 69 of them were never applied. n1 led 4.5 ms after the last device answered. What the
step-down cost on doc 34's bare-metal machine, a second more, is inferred, not measured here: it is
the client's way through it. Publishes the old leader took were lost with its uncommitted entries
(the 69), and its owed publishes were redirected (doc 34 §8, "Answers were dropped on a full lane").
The harness's publisher offers again at once until a leader takes one, and does neither.

## The reproduction

The simulation, 8 seeds a shape, each cell the seeds that lost the lead or spent a term
(`survey_of_stalls`, hyper-durable `tests/liveness.rs`):

| Shape of stall | b5ea01ba (doc 34's) | 25cf889 (Quiver's) |
|---|---|---|
| Every node's at once, 584 to 587 ms (doc 34's) | 8 of 8 | 0 of 8 |
| The same, the stream and replica on one log a frame at a time (Focal's member) | 8 of 8 | 0 of 8 |
| A journal's, ending 585, 735 and 885 ms | 8 of 8 | 7 of 8 |
| The leader's alone, 585 ms | 8 of 8 | 8 of 8 |
| The leader's alone, 2 s | 8 of 8 | 8 of 8 |

At b5ea01ba doc 34's shape left no leader for 492 to 556 ms, and 0 to 95 publishes were never
applied. At 25cf889 nobody suspected anybody: publishes p99 571 to 575 ms and p99.9 585 to 588 ms,
the stall itself, and all of them applied.

The test of record is `a_flush_stall_every_node_shares_spends_no_term`: doc 34's stall costs no
lead and no term, over 8 seeds and both log shapes. It fails at b5ea01ba in every seed and passes
at 25cf889 and at hyper-raft main (af01c51 and 698c0da).

On a real disk, with doc 34's own member and client. Three `focal start bus` nodes ran as
processes of one user unit on fedora (an Intel i9-14900HX with 32 threads and a Samsung 9100 PRO;
each node's log in its own directory on that one disk). Doc 34's comparison client,
`focal-compare --system focal-bus --durable --mode append`, sent 128-byte records at 1,000 a
second, 30 s after 10 s of warm-up. Doc 34's run used Docker on the 9950X3D2; these nodes ran
outside it. At 25 s into each run, every node's disk writes were held at once through the unit's
cgroup, and then let go: `io.max` at one write and one byte a second, set and cleared with
`systemctl --user set-property`, so no root was needed. The client was patched locally to print
each leader it learns and each redirect that names none: a diagnostic in a local clone, not in
Focal's tree. Release builds at focal 7a79f0d7 (doc 34's, hyper-raft b5ea01ba) and f8052a5e
(Quiver's pair, hyper-raft 25cf889), run interleaved, 2026-10-25 04:38 to 04:51 PDT:

| Run | Focal | Load at start | Held | Redirects naming no leader | p50 / p99 / p99.9 / max (ms) | Unacknowledged |
|---|---|---|---|---|---|---|
| a1 | 7a79f0d7 | 2.33 | 615 ms | 457, from 458 to 708 ms into the hold | 31.7 / 248.8 / 749.2 / 829.9 | 293 |
| s1 | 7a79f0d7 | 0.28 | 616 ms | 594, from 430 to 689 ms | 31.9 / 269.5 / 746.6 / 817.4 | 282 |
| s1 | f8052a5e | 0.94 | 608 ms | none | 30.5 / 404.5 / 667.9 / 675.3 | 0 |
| s2 | 7a79f0d7 | 2.08 | 616 ms | 936, from 324 to 706 ms | 30.2 / 565.7 / 1,047.0 / 1,097.9 | 322 |
| s2 | f8052a5e | 12.96 | 607 ms | none | 28.1 / 410.5 / 655.4 / 677.9 | 0 |
| s3 | 7a79f0d7 | 15.85 | 610 ms | 1,010, from 316 to 694 ms | 29.4 / 66.2 / 1,069.5 / 1,132.5 | 480 |
| s3 | f8052a5e | 3.82 | 607 ms | none | 30.1 / 416.0 / 663.7 / 688.9 | 0 |

At doc 34's commit the leader stepped down in every run. 316 to 458 ms into the hold, it began
answering publishes with redirects naming no leader, and the client held them until a leader was
named again, after the hold ended. 282 to 480 publishes were never acknowledged ("no acknowledgement
for a serve period"), and the latencies of those acknowledged reached 0.82 to 1.13 s. The figures
count only acknowledged publishes, so s3's p99 of 66.2 ms leaves out its 480 lost. At Quiver's pair,
no leader stepped down in any run: no redirect, every publish acknowledged, the tail the hold itself
(max 675 to 689 ms). Two runs began under others' load (12.96 and 15.85); the outcome was the same
under every load.

hyper-durable-e2e, kept out of the test of record, doesn't reproduce doc 34. It runs three members
as processes, each a replica on hyper-log over a real file flushed with the full flush, and its
`hold_flushes` holds every member's flushes at once. On adas-mac-studio-3 (macOS), with holds of 0.6
to 4 s, at least three times its detectors' freshness of 138 to 365 ms, b5ea01ba's members spent a
term in only 3 of 30 runs: the first three, just after their build, for a reason not known.
25cf889 spent none in 28 runs. A diagnostic print in b5ea01ba's member shows why. Through the hold,
every link stayed trusted with its freshness point passed (572 ms and then 1.38 s before), and its
stream asked no wake, so it judged nothing. When heartbeats came again, the 106 slots their sender
had skipped put the freshness points back. The hold didn't always stop every stream either: in 4 of
36 runs looked at halfway through, a member had sent 4 to 6 heartbeats. The difference is the
owner's, not the stream's, as Focal's agent explained. focal-bus's member polls its stream at the
top of every turn (`Member::turn`), and its serve loop runs a turn after every endpoint event,
deadline and log answer. Under a publish load the judge runs all through a stall, whatever the
stream's wake says. hyper-durable-e2e's member polls only on a datagram or the stream's wake. With
heartbeats held and the wake none, it never judges. Held with no traffic, focal-bus would likely do
the same.

## What step 2 inherits

At 25cf889 and hyper-raft main, three shapes still cost the lead. Each is a test that fails today,
ignored until step 2:
- **Stalls that end apart** (`flush_stalls_that_end_apart_spend_no_term`): 7 of 8 seeds at 25cf889.
  The judge whose own stall ends first holds a peer still stalled to its margin past that end. At
  seed 0, n1 suspects n2 and n3 28 and 30 ms after its own stall ends at 585 ms; 3 terms are spent.
- **The leader's flush alone** (`a_stall_of_the_leaders_flush_alone_spends_no_term`): 8 of 8
  seeds at 25cf889. Both followers suspect it within their freshness (59 to 88 ms) and elect
  another, though its process is alive and its followers' disks are a majority.
- **The end of a stall every node shared** (hyper-liveness `tests/sim.rs`,
  `flushes_stalled_together_draw_no_suspicion_as_they_end`): a judge suspects a peer within a
  freshness of the stall's end (main, macos world, seed 68719476737: node 1 suspects node 3 101 ms
  after a 983 ms stall ends).

Focal's agent traced the first and third to one root: a judge condemns its peers for a span in
which it couldn't take in their proofs. It proposed Lifeguard's local health multiplier: a peer's
freshness is charged to the judge's own progress. The second is step 2's split of the leader's
process from its disk.

## Where it is

The tests are on hyper-raft branch `cp/t2.2-stall-survey` (ef5e98e), on main 698c0da (rebased from
af01c51, checks clean again). It stays local until Ada gives her word to push it to
hyper-light/hyper-raft. The b5ea01ba and 25cf889 simulation runs used the
same tests on those commits, on adas-mac-studio-3, 2026-10-25 03:51 to 04:20 PDT. The simulation
is deterministic: a seed's run is the same on any machine. The real-disk runs' script, the client's
diagnostic patch and each run's report, node logs and client log are in fedora's
`~/.cache/quiver-t2/doc34/`; the scripts are `doc34-run.sh`, `doc34-series.sh` and `focal-diag.py`.

What would falsify this account: a run of doc 34's shape on focal-bus's member at b5ea01ba that
keeps its leader, or a run on a commit with the three rules that loses it. Over the seven real-disk
runs above, neither happened.

## Step 2: each process judged apart from its disk

### What changed

Three changes, on hyper-raft branch `cp/t2.2-disk-apart` on main 698c0da. Each is off by default,
so today's groups run as they did:
- **Heartbeats apart from flushes** (hyper-liveness, `Liveness::set_disk_apart`). A heartbeat
  leaves when it is due, whatever a flush is doing, and still carries its sender's durable count.
  Its receiver takes it as its sender's process alive, with no proof of a fresh flush, and a judge's
  own stalled flush no longer holds its judging back. A peer is suspected for silence alone: a
  process that stopped, or a link that broke.
- **Commits on a majority's disks without the leader's** (`Config::apply_unpersisted`, hyper-raft
  core's R-6, there before this step). A leader applies an entry of its own term, and so answers
  its publish, once two of the three disks hold it, whether its own is one of them or not. Ongaro's
  thesis has it (§10.2.1, "Writing to the leader's disk in parallel"): "The leader may even commit
  an entry before it has been written to its own disk, if a majority of followers have written it
  to their disks; this is still safe." TiKV's RFC 0112 and raft-rs's
  `max_apply_unpersisted_log_limit` apply entries the same way.
- **A leader's patience with its own disk** (hyper-durable, `Replica::set_disk_patience`). A
  leader whose oldest write has been out longer than its patience hands the lead to the voter its
  detectors trust that holds most of its log. It does so by `TimeoutNow`, which leaves without a
  write of its own. A disk that stops for good costs one hand-over, rather than a lead kept with no
  disk under it.

The step names two more things, and each already holds:
- **The leader's lease kept while its own fsync is slow.** Focal's member elects by suspicion
  (`Elections::Suspicion`). A leader keeps the lead while it trusts a quorum, and a follower stands
  by a leader it trusts. Judged apart from the disk, both rest on processes alone, so no fsync can
  take the lease.
- **Flushes scheduled per device.** hyper-log keeps "one log per device, shared by every group on
  it, so that one flush commits every group's writes" (`crates/hyper-log/src/lib.rs`). A bus node
  opens one log (`member.rs`, `open_log`), which both its part's group and its liveness stream's
  group write, and it serves one part (`serve.rs`). Per group is the lab's shape alone: three nodes
  on one machine's file system, where btrfs couples their flushes (below).

### Each change on its own

The simulation, the leader's disk alone stalled 2 s under a publish a millisecond, 8 seeds a row
(`survey_of_stalls`, at 8443cac):

| The group | Seeds that spent a term | Publishes lost | Of the 2,000 offered in the stall, those its leader answered in it | Its leader's slowest answer to them |
|---|---|---|---|---|
| Today's: heartbeats proved by a flush | 8 of 8, 1 or 2 terms each | 37 | none | — (another leader takes them) |
| Heartbeats apart from flushes | 0 of 8 | 0 | none | 2,001 to 2,002 ms |
| And commits on the followers' disks | 0 of 8 | 0 | 1,996 to 1,998, the rest just after its end | 4.2 to 4.5 ms |

In today's group both followers suspect the stalled leader once their freshness (54 to 64 ms)
passes and elect another, and the old leader's uncommitted publishes are lost with it. With heartbeats
apart, nobody suspects the leader and the group applies every publish within 5.3 ms, but the leader
answers only what its own disk holds: the stall's publishes wait for its end. With commits on the
followers' disks too, the leader answers them as they come.

The patience, on its own: the leader's disk stops for 6 s, with a patience of 1 s
(`judged_apart_a_leaders_disk_that_stops_costs_one_hand_over`). The leader answers its first
second's publishes, about 1,000, within 4.2 ms each, then hands over. Over the 8 seeds that's 1 or
2 hand-overs and 1 or 2 terms, at most 7 ms without a leader, and every publish applied, the slowest
in 15.7 ms. A second hand-over is the first one given up by the core (its transfer's timeout) and
begun again. A second term is the heir's. It campaigns without pre-vote on the core's word, and its
ballot's round (the quorum's round trip and one flush, `hyper_timing::Ballot`) can end before a vote
that waited on its voter's flush comes (seed 1: the vote came 0.2 ms after the round ended).

### The bar: 10,000 seeds

`judged_apart_ten_thousand_stalls_of_the_leaders_disk_start_no_election` stalls the leader's disk
alone for 2 s, a publish a millisecond, in the step's group, and counts elections by the
simulator's terms. It ran on adas-mac-studio-3 as twelve processes of the release test binary, each
on its own range of seeds: at 7b4250f (2026-10-25 07:40 PDT), and at 8443cac (09:29 PDT), whose
code the branch's commit keeps (it changes only another test's assertions). Both runs had 0
elections in 10,000 seeds. Of the 20,000,000 publishes offered in the stalls, none went
unanswered and none was answered before two disks held it. The slowest answer took 5.2 ms.

### The tests

Each passes in every one of its seeds:
- **Step 1's stall test in the step's group**
  (`judged_apart_a_flush_stall_every_node_shares_spends_no_term`): no term is spent, and every
  publish is answered, each on a majority's disks when it is answered.
- **A 2 s stall of each node's disk in turn**, the leader's among them, 500 ms apart
  (`judged_apart_a_stall_of_each_node_in_turn_spends_no_term`): no term is spent, and the leader
  answers every publish within 10 ms, each on a majority's disks.
- **The leader's disk alone for 2 s**
  (`judged_apart_a_2_s_stall_of_the_leaders_disk_spends_no_term_and_answers_every_publish`): the
  same.
- **Step 1's other two shapes that cost the lead:**
  - stalls that end apart, at 585, 735 and 885 ms
    (`judged_apart_flush_stalls_that_end_apart_spend_no_term`): no term is spent, and every publish
    is answered on a majority's disks;
  - the end of a stall every node shared (hyper-liveness `tests/sim.rs`,
    `judged_apart_flushes_stalled_together_draw_no_suspicion`): no suspicion that the same run without
    the stall doesn't show.
- **A stalled disk**, in each of hyper-liveness's three worlds, macOS's, a busy machine's and
  Linux's (`judged_apart_a_stalled_disk_is_not_suspected`): no suspicion of its node.
- **kill -9** (hyper-durable's
  `judged_apart_the_shell_elects_sleeps_and_fails_over_on_the_liveness_stream` and hyper-liveness's
  `judged_apart_a_killed_peer_is_suspected_within_the_stated_bound`): a killed leader is suspected
  within the bound its detector states, and the survivors elect and take a proposal, as in today's
  group.

Today's group still fails step 1's tests of the leader's flush alone and of stalls that end apart.
They stay ignored, and now say that the step's group passes them.

hyper-durable-e2e's 23 scenarios and 6 random ones run three members as real processes, each a
replica on hyper-log over a real file. They pass in the step's group (`HYPER_DURABLE_E2E_DISK_APART`
set) and in today's, on adas-mac-studio-3. In the step's group:
- `stall-leader`'s leader hands the lead on after its 5 s patience;
- no member suspects `stall-follower`'s stalled member;
- `stalled-devices`' leader, its disk stopped for good, goes on taking writes through a 7 s hold of
  its survivors' devices, and then hands the lead on;
- the 12 kill -9 scenarios elect as they do in today's group.

### The lab

The build is Focal's member at bus-replicated 10fb5b9, with the branch's liveness and replica
vendored and a switch, `FOCAL_BUS_DISK_APART`, that turns the three changes on. It's a local build
and is never pushed. The setup:
- three `focal start bus` nodes, each a user unit on fedora;
- doc 34's client at 1,000 a second, 30 s after 10 s of warm-up;
- the leader's disk held for 2 s, 25 s in;
- each node's log on a btrfs subvolume of its own, without copy-on-write (`chattr +C`, so its
  direct writes stay direct on a compressed volume).

The leader's disk was held in two ways.

**In its process** (series 5, 2026-10-25 09:11 to 09:18 PDT). The lab build's hold point
(`FOCAL_BUS_LAB_HOLD`) holds each write and flush of the leader's files before it is made, for as
long as a file exists. So the leader holds nothing of the kernel's while it waits, as on a disk of
its own:

| Group | Runs | Terms spent | Publishes offered in the hold | Their slowest acknowledgement |
|---|---|---|---|---|
| Today's | 3 | 3 of 3: both followers suspected the leader 329 to 531 ms into the hold, and another led 0.5 to 0.7 s in | 6,000, all acknowledged | 533 to 713 ms |
| The step's | 6 | 0 of 6, and no suspicion in the hold | 12,000, all acknowledged | 30 to 33 ms |

In the step's group the hold doesn't show. The publishes offered in the seconds just before and
after it were acknowledged within 26 to 34 ms. Each run's slowest acknowledgement outside the hold,
61 to 428 ms, matches its followers' longest commit (84 to 436 ms). In apart-h3, for example, both
followers' longest commit was about 0.44 s, and its slowest acknowledgement came at second 14.

**By its cgroup** (`io.max`, as in step 1; series 3 and 4, 08:05 to 08:15 PDT). The block layer
holds the leader's writes:

| Group | Runs | Terms spent | Publishes offered in the hold | Their slowest acknowledgement |
|---|---|---|---|---|
| Today's | 3 | 3 of 3: another led about 0.5 to 0.7 s in | 6,000, all acknowledged | 570 to 717 ms |
| The step's | 9 | 0 of 9 | 18,000, all acknowledged | 33 to 68 ms in 7 runs; 889 and 2,047 ms in 2 |

In those 2 runs the file system held the followers' flushes with the leader's. In apart8 every
node's longest commit was over 2 s, though only the leader's cgroup was held. A process whose writes
the block layer holds may hold btrfs's own locks or a transaction's commit while it waits. The
processes on the same file system then wait with it. That is inferred, not measured. Either way it's
the lab's one machine, not the group: Quiver's nodes are on machines of their own, and step 7's lab
holds the leader's own disk with dm-delay.

**kill -9 of the leader** (08:15 to 08:36 PDT). Five runs killed the client's leader 25 s in: two
in the step's group, one in today's on the lab build, and two on pristine 10fb5b9:

| Run | Group | Survivors' detection (ms) | Noticed late (ms) | Elected after the slower suspicion (ms) |
|---|---|---|---|---|
| k1 | the step's | 276, 886 | 0.2, 0.2 | 48 |
| k3 | the step's | 158, 217 | 0.1, 0.2 | 61 |
| k2 | today's | 264, 308 | 160, 171 | 39 |
| p2 | today's, pristine | 297, 296 | 95, 135 | 22 |
| p3 | today's, pristine | 399, 416 | 193, 346 | 46 |

In the step's group each survivor suspected the killed leader within 0.2 ms of the point its
detector stated, and the group elected within 61 ms after that. In today's group survivors noticed
95 to 346 ms late. That is inferred to be 1ccd38d's rule, a judge with its own flush proof out
judging no peer, and was not traced. In k1 the second survivor's detector held the killed leader to
886 ms, so the first survivor's pre-vote waited for it, as pre-vote should, and the lead changed
699 ms after the first suspicion.

In none of these runs did doc 34's client follow the new leader. It kept the killed node as its
leader to the end and failed its window of 4,096 publishes ("no acknowledgement for a serve
period"). That is the client's failover, in Focal's tree, and Focal's agent has it. An earlier
report to Focal's agent said the survivors didn't elect. That was wrong, and it has been corrected.

### Where it is

- **hyper-raft branch `cp/t2.2-disk-apart`**: one commit, 435908a, on step 1's ef5e98e on main
  698c0da. It holds the three changes, the tests, hyper-durable-e2e's switch, and `docs/timing.md`
  §2.8 and `docs/durable.md` §4.2 on what they do. It's local until Ada gives her word to push it to
  hyper-light/hyper-raft. Its checks on adas-mac-studio-3:
  - At 8443cac: hyper-raft's gate (`scripts/gates.sh`). The contracts, fmt and clippy on every
    target are clean, and 1,745 of the workspace's 1,746 tests pass; CI's 10,000 seeds of
    hyper-durable's sim pass too.
  - The one failure is a flake that main has too: hyper-rt-transport's
    `each_connection_receives_on_a_socket_of_its_own_and_lets_it_go_with_the_connection`. Run
    interleaved 20 times each, it failed 2 times at 698c0da and 0 on the branch. hyper-rt-transport
    depends on none of the crates the branch changes.
  - At 435908a: the contracts, fmt and clippy are clean again. The changed crates' tests pass
    (hyper-liveness 47, hyper-durable 83), and so do hyper-durable-e2e's scenarios in the step's
    group.
- **Focal's lab build** (`lab/disk-apart`: 0f1d68e, and 3549be9 for the hold point): local, and
  never to be pushed. The fix goes to Focal as a branch of its own in step 10.
- **The records:**
  - the lab's scripts are `doc34-lab3.sh` (the cgroup hold), `doc34-lab4.sh` (the hold in the
    process), and the series' scripts;
  - the runs' reports, node logs and client logs are in fedora's `~/.cache/quiver-t2/doc34-lab/`;
  - the 10,000 seeds' logs are in adas-mac-studio-3's `~/.cache/quiver-t2/10k-0740/` and
    `final4-0912/`.

What would falsify this: a seed, or a lab run with the leader's disk held apart from the
followers', in which the step's group spends a term on a stall, or answers a publish before two
disks hold it.

## Focal's conditions on the disk mode

Focal's agent set six conditions on the disk mode before it goes anywhere. Where each stands on
2026-10-25:

1. **Mode agreement:** not yet begun.
2. **A model of the Persisted acknowledgement:** met. hyper-check's model of the persisted class's
   commits (`tests/models/unpersisted.rs`) found no fault at any of three scopes (192,592 to
   4,133,285 classes). The rule taken out (the leader's memory counted) was refused in 7 steps. Both
   searches run in hyper-raft CI's explore job (d69f309).
3. **Images and compaction:** met (below).
4. **Votes:** met (fb44834): a stalled disk gives no vote it doesn't hold.
5. **The hand-over:** the flush tail, the disk verdicts and the heir rule are met. The patience's law
   waits on Focal's agent.
6. **All disks slow:** met (2060014). Meeting it took a fix to the patience: it counts from when a
   voter could take the lead.

### Condition 3: images and compaction

Focal's agent asked for three things:

- A part's image covers only committed entries.
- It is durable before the log is compacted past the local durable index.
- A crash is checked at every point between apply, image write and compaction.

Under `apply_unpersisted` the window can let go of entries the leader applied but doesn't hold
durably yet, so the image's point can pass the log's durable index.

**The test.** It is `a_member_killed_between_apply_image_and_compaction_keeps_every_acknowledged_batch`
in focal-bus's `tests/images.rs`, on Focal's lab branch.

- Three members replicate one part over loopback in the disk mode.
- Each segment holds one batch of the largest size (1 MiB), so the window lets go, and the part
  makes an image and compacts, at about every batch.
- The lab's hooks hold one member's log writes, or its image install at a chosen step, and then
  kill it. Every later write and flush of its files fails and reaches no file, as after a kill -9.
- The killed member is then opened again on its directory.

**The points.** The leader's:

- P1: it applied past its log's durable index.
- P2: its window let go past that index.
- P3a, P3b, P3c: it handed an image past that index, the install held at its create, at its rename
  and at its directory's sync.
- P4: that image is durable.
- P5: a compaction's start is out, held.
- P6: that start is durable.

A follower applies only what its own disk holds, so its points are P3a to P6.

**The results.** On adas-mac-studio-3, in release, 3 runs of each point, 42 runs in 100 s:

- Every point was reached. The leader was killed up to 15 entries ahead of its own durable log.
- In all 18 leader runs from P3a to P6, its durable image was past its log's durable end. So each
  of its reopens took `docs/durable.md` §4.3's third case: the log starts at the image's point.
- 2,261 batches were acknowledged, each once and at its offset. Every member's part, the reopened
  one's among them, came to hold every batch.
- At every step, each image a member had out or made durable was checked against the members'
  durable logs: 510,681 checks, and none was past what two members' disks hold.

**What it found.** Two faults, both in rejoining, not in images, both fixed on the lab branch:

- **A node that restarted was never dialed again.** A node dials only the nodes above it, once, at
  its start. Its peers' connections to the process that stopped stay open until the transport's
  idle timeout, 120 s. The leader, killed and reopened as n3, sat at term 2 for 30 s while the
  group was at term 4. Now a node opened on a run its data directory held before dials every node,
  and a node whose connection closed or whose dial failed is dialed again (a82c1f7).
- **A restarted follower's Raft traffic stayed queued for the old process.** A follower killed
  while its log was held left its leader's outbox holding frames for the old connection's lane.
  Every later Raft frame for it queued behind those, the leader's probe on seeing it restart among
  them, waiting for lane room that no event would bring until the idle timeout. The leader's
  progress for it stayed Probe paused, and it never caught up, in 3 runs of 10. Now a peer seen
  restarting has its outbox let go, and a node's new connection flushes what waited (87eca91).
  After it: 12 of 12, then the full 42.

Both fixes are Focal's to take. One patch on Focal main 10fb5b9 (`rejoin-fixes.patch`, 7 hunks in
`member.rs`) applies, is clean under clippy, and passes focal-bus's 56 tests.

What would falsify this: a run that loses or moves an acknowledged batch, that opens on a log that
doesn't fit its image, or that finds an image past what two members' disks hold.

**Where it is:**

- Focal's lab branch `lab/disk-apart`: 5f4510c to 87eca91 on 3549be9. Local, and never to be pushed.
- The drop for Focal's agent: fedora's `~/quiver-review/focal-condition-3/`.
- The runs' logs: adas-mac-studio-3's `~/.cache/quiver-t2/f3-143538/`.
