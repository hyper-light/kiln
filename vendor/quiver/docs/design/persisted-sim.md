# The persisted class under simulation (kiln's CP-T2.2, step 6)

Status: 2026-10-26, built for CP-T2.2 step 6 (AC-T2.2.5, kiln plan
`01a16c5b7e8c75acbdfafd51315c3885`), on Focal's bus at `b6cf8e3` and hyper-raft at `c6baec2`. How
Quiver's persisted class runs in a deterministic simulation, what the simulation does to it, what
each run checks, and what it found. CLAUDE.md §5: each decision
with its sources, its evidence and what would falsify it.

## What it must show

The step: Quiver's persisted class under kill -9 of any node at every write and await,
partitions, duplicated and lost messages, disk stalls and the storage model's crash states;
every acknowledged publish checked present, in its producer's order, after every recovery; each
failing seed replayable from its trace. Done when no acknowledged publish is lost over 10,000
seeds, and a planted bug (acknowledgement before a quorum's flush, a test build) is found and
replayed from its seed.

The kill drills of step 4 (`tests/persisted_kill.rs`, `tests/persisted_processes.rs`) run in real
time on real disks. They kill at each frame of one run of publishes and cannot replay what they
find. This simulation runs the same code on virtual time, a simulated network and simulated
devices, so a seed decides every event and a failing seed runs again the same.

## The world

- **Runtime and network:** hyper-rt's simulation runtime (`hyper_rt::sim::SimRuntime`,
  hyper-raft `docs/runtime.md` §11). It is one thread with virtual time and a UDP fabric that
  models one-way delay with seeded jitter, reordering, Gilbert–Elliott loss, a path MTU and
  bottleneck links. hyper-rt-transport's `Driver` binds on its ports, as on real sockets
  (hyper-rt-transport `tests/simulation.rs`).
- **Nodes:** three, each a shard of its own running the node's host task (`persisted::host`). This
  is Quiver's code as it ships: the catalog, the members, the publishers, the views. Focal's
  member and client run over hyper-transport's endpoints.
- **Disks:** each member's hyper-log sits on hyper-sim's block device
  (`hyper_block::sim::SimDevice`). It has power-loss semantics: unflushed sectors survive a crash
  per `Crash` (`Random`, `LoseAll`, `KeepAll`), and `Fault::PowerCut` cuts power at a stated
  write or flush. A part's image and the node's small files (its run, its ports, the log's keys)
  are real files in the seed's directory under `~/.cache`. Their writes are synchronous and made
  durable before use, so a kill leaves them as a real kill would.
- **Clients:** each node's API publishes records at virtual times the seed draws. Each record
  names its producer (a node and its run) and its number in the producer's order.

## Determinism

A seed must decide every event. Six sources of variation are closed:

1. **Time.** Every `Instant` in a node comes from one process-wide anchor
   (`hyper_rt::sim::anchor`) and the shard's virtual clock. On a simulated shard
   (`ShardContext::driver_is_sim`) the transport's driver anchors to it, and
   `hyper_rt_transport::shard_instant` gives the same reading to an endpoint made before its
   driver and to the host's own clock. Off a simulated shard nothing changes. Before this, each
   driver anchored to `Instant::now()` when it was made, so a host's drivers disagreed by the real
   time between their making.
2. **Randomness.** Each endpoint's QUIC layer is seeded (`Endpoint::new_seeded`) from the world's
   seed, the node, its start and the endpoint's place in the start's order. TLS draws its own bytes
   from AWS-LC, so a handshake's contents differ from run to run, to the order of a ClientHello's
   extensions; their sizes, and so the flow, are fixed only where every signature is one length.
   An ECDSA certificate verify is a DER document of 70 to 72 bytes, as random as its nonce, so the
   simulation's identities are Ed25519, each signature 64 bytes (focal-bus's `pki::KeyKind`, its
   recorded change 0005, asked for by `Host::open` under the simulation). The cluster's identities
   are made once and kept, so a run replays across processes that share them. hyper-durable's member incarnation is drawn
   from the system, but it is fixed-width on the wire and only ever compared for equality.
3. **Threads.** hyper-log runs an owner thread and an I/O thread, and the simulated device a
   thread of its own. When a log answers depends on them, not on the seed, and so does what one
   frame holds. In the simulation each log runs under `Waits::Told`:
   - the owner writes a frame only when its caller cuts (`Log::cut`), and the frame holds
     everything submitted before the cut, in order;
   - the host cuts at the virtual time its disk model draws for the write, then waits on the
     shard's thread until the cut is answered, which is once that frame is durable and confirmed;
   - each confirmation is made durable one way, not the way the device measured cheaper in real
     time;
   - a caller that waits on its own answer from the moment it asks tells the log by waiting.
     That covers the restore an open writes, `Replica::open`'s reconciling write
     (`GroupStore::write_now`), and every blocking fetch: a replica reads its entries through
     `GroupLog::fetch`, which waits. Its thread is blocked until the answer, so the order of what
     the log does is still its own.
   - A fetch made with `fetch_waking` waits for a serve (`Log::serve`). The member never makes
     one.

   So every answer reaches the member at a virtual instant the seed chose. A stall is a cut drawn
   late.
4. **Hash-map order.** Focal's member redirected the acknowledgements it owed in a `HashMap`'s
   order when it stopped leading, which differs from run to run; now they're sorted first.
5. **Ports.** A simulated bind took the next free port of the shard, whatever was asked, from a
   stack. A node started again took its sockets back in another order, so its catalog's member
   listened where its peers never dial. Now a port of the shard's own range asked for by number is
   taken if free and refused if bound, as an OS binds one.
6. **The wall clock.** QUIC's address-validation tokens carry their issue time in whole seconds of
   the wall clock, and a server's token log refused a valid token issued before the first one it
   checked: its first period began at that token's expiry. Whether a run's tokens straddled a
   second decided whether a server refused one, and a client refused went on unvalidated. Now the
   first period begins where that token's own began (hyper-quic's `BloomTokenLog`, hyper-raft's
   recorded change 0006, a fix offered upstream), so a valid token is taken once whatever order
   the clock issued it in. Its expiry is read on the wall clock too, but no run comes near it (two
   weeks; a Retry's, 15 s).

**What would falsify it:** a seed whose run differs from itself. Each failing seed is run again,
and its digest must match. `a_seeds_run_is_the_same_run_twice` compares two runs' traces,
outcomes, views and digests. The digest covers the trace (every fault and kill, with when), every
publish's outcome, every node's view, the fabric's counts and every device's writes and flushes.
The sweep runs every seed twice, in two partitions of the seeds over processes, and compares the
digests. A probe runs one seed again and again in one process and names the first mark two runs
part at (`a_seed_run_again_and_again`, `QUIVER_SIM_REPEAT`, `QUIVER_SIM_MARKS_NS`).

What it showed (2026-10-26): seeds 2 and 3, each run five times in a process, ran alike on
adacachy and on fedora, to the same digests on both, fedora having made its identities afresh. One
first run differed: the first at the bus's `b6cf8e3` against an identities directory the older
binary had left in the clear, which the newer seals as it loads it (a run once, then never). At the
older pins a run in a resend storm ran apart once, late (the planted build's seed 6 at 63.7 s, one
run of four), and one seed had two outcomes in separate processes; neither came again at the newer
pins. The identities are made once, by one process, before a sweep's processes start: made by
several at once, they raced, and each one's first seed found no topic held everywhere.

The first full sweep (both passes of the 10,000 seeds at `bf54d1e`) ran 1,067 seeds apart, 10.7%:
every acknowledged publish present in both, but not alike. A log of every datagram the fabric
carried and every packet the QUIC layers built (a diagnosis kept out of the tree) traced each one
it was asked about to the two causes above: a certificate verify a byte longer in one run than in
the other, and a token one run's server refused and the other's took. Seed 8 ran its first
handshakes apart in all twelve runs of a process with ECDSA identities, and alike in twelve with
Ed25519. With both closed, seeds 1 to 400 ran alike in the sweep's two partitions, 400 of 400;
with ECDSA identities and the token log fixed, one ran apart (seed 317). The logs' own threads wake
the host as they answer, a turn more at an instant real time chose: with the logs given a waker
that wakes nothing, 400 of 400 ran alike, as with it, so it stays as it was.

## What the simulation found

- **A moved peer was unreachable for two minutes.** A topic's member started again binds the port
  it kept, unless another socket took it while the node was down. Then it binds another and
  records it in the catalog. Its peers took the new address, but went on with what they held or
  were dialing at the old one. A dial in flight holds off every new dial to that peer until its
  handshake times out (`Endpoint::connect`), which is two minutes. In seed 3, node 3 started
  again while node 2 was down, and the two dialed each other's old ports. Node 3 never caught up
  within the run's 60 seconds once the network mended. Now a member and a publisher close what
  they hold to a peer that moved and dial it at its new address
  (`Seat::peers_changed`, `Publisher::set_addresses`). A real network moves ports rarely, as an
  OS's ephemeral ports rarely collide. The simulation's ports collide often, so it found this.

- **A node back from a kill could not publish for two minutes.** A transport keeps a peer's
  connections until they have carried nothing for its serve period, two minutes. A process back
  from a kill dials with the same identity, and the dead process's connections hold both of its
  places, so it is refused Busy. Quiver's members closed a suspected peer's connections for this
  (step 4's kill drills), but only its member's. The node's publisher, a client identity of its
  own, was refused by every member until the old connections aged out: in seed 8, 4,576 times in
  a minute. Now a member that suspects a node closes that node's publisher's connections too
  (`Seat::turn`, `Plan::publishers`).
- **A node's own publisher kept its own node's old port.** A topic's publisher took the catalog's
  new addresses only when its member's peers changed, and a member's peers leave its own node
  out. When only this node's member moved, its publisher went on dialing the old port, and a node
  that led never reached itself: in seeds 9, 13 and 14 the leader's own publishes were redirected
  thousands of times, never sent to the leader. Now the publisher takes every address the catalog
  changes, its own node's included (`Host::reconcile`).

Each of these runs again as its seed, and seeds 1 to 20 pass with all three fixed.

- **Followers that never caught up again (Focal's bus, hyper-raft).** Over about 6,000 seeds of
  the generator before this one's (the bus at `f8052a5`, hyper-raft at `25cf889`), 77 runs (1.3%)
  left a node behind for the whole minute after everything mended. In none was an acknowledged
  publish lost, or another put at its offset. The member's diagnosis (focal-bus's `sim` feature:
  the core's state, each member's progress, the outbox, the Raft traffic per peer, the liveness
  stream's measures) traced three causes:
  - A leader's Raft frames to a node that waited for its lane waited for good once the node's
    next connection took the old one's place: a lane says it has room only once it refused a
    frame, and the new one never had. One leader queued 16,032 heartbeats and 320 appends for a
    node that, hearing its leader's liveness, never campaigned. Focal's bus fixed it in `425dcda`
    (a member's new connection flushes what waited).
  - A restarted node's liveness stream sent no heartbeat and refused every peer's as Unmeasured,
    so its leader suspected it for good and left it out of its beats. hyper-raft's newer
    liveness and QUIC fixes (`3e54e50` among them) cleared 12 of the 14 seeds the outbox's fix
    left; with Focal's bus at `b6cf8e3` and hyper-raft at `c6baec2`, 71 of the 77 passed.
  - The six left: the stream's first flush was answered at the instant it was asked (a cut due
    then), its timer not yet measured, so its floor and first interval were nothing, and the
    pair kept a schedule's spacing of nothing. Every heartbeat came due at the instant it was
    asked, no flush fell after the one before, and the stream proved none in 121,752 tries, sent
    none, asked no wake, and so never measured its timer. Fixed here (hyper-raft's recorded change
    `0004`, its test failing without it, offered upstream): all 77 pass.

- **A node back at its member's port was refused for two minutes.** The sweep's first pass at
  `d4ad260` broke 8 of its first 1,982 seeds, none losing an acknowledged publish. A node killed
  and started again at its member's port left its peers holding the dead run's connection (a
  transport keeps a peer's connections until they have carried nothing for its serve period), and
  they refused the new run's dials as Busy: in seed 3763 a catalog member dialed every 56 ms for
  the rest of the run, each connection closed within 3 ms, and the two members suspected each other
  for good. Focal's node keys its endpoint for restarts (hyper-raft's `RestartKey`,
  `docs/transport.md` §4ah, through Focal's `serve::restart_key`); Quiver's members now do too, at
  the address their socket holds (`Identities::member_endpoint`, its socket bound first): the new
  run answers the dead run's connection with a reset its peer verifies, and the peer dials again.
- **A node back within its peers' detection bound could not publish.** Killed and back in 33 or
  44 ms (seeds 1293 and 2608), a node was never suspected, so no member closed its dead run's
  publisher connections, which held both of its client identity's places: every member refused the
  new run's publisher Busy (3,718 connections lost in a minute, no acknowledgement). A member told
  the node restarted (its liveness stream's later run, `GroupEvent::Restarted`) now closes that
  node's publisher's connections too (`Seat::turn`). A publisher's own endpoint takes a port of the
  system's choosing at each start, where no peer reaches its last run, so it is not keyed.

  With both, the eight seeds pass.

- **A handshake met one loss burst for 30 seconds (the simulation's own model).** The sweep's two
  passes at `bf54d1e` broke the same 3 of their 10,000 seeds, none losing an acknowledged publish:
  in seeds 3053, 6302 and 7452 the nodes never caught up once mended. In each a publisher's dial
  to the node that led its group met a burst on the path that lost most of what it carried, and
  the burst never ended: hyper-rt's Gilbert–Elliott chain stepped once per datagram, and the
  dial's probes, backing off to 16 s apart, sent a datagram a few seconds. In 6302 node 3, back
  from a kill, dialed node 1: 16 datagrams in 31 s, every one in the burst, the ClientHello's
  second half alone ever arriving, held by node 1 and let go. In 7452 node 1's answers to node 2's
  publisher were lost the same way, then the publisher's own datagrams; in 3053, with no kill,
  node 1's catalog publisher redialed node 2 after a suspicion, 12 datagrams in 15 s. A real
  path's burst ends in time whether anything is sent or not, so a bursty path's chain now steps
  once per millisecond its flow is quiet as well (`SimLoss::in_time`, hyper-raft's recorded change
  0005), and the three seeds pass. Quiver's publisher waits out the probes' backoff as a TCP client
  waits out its SYN's: a dial that heard nothing is held until the transport's idle timeout, two
  minutes, and the next waits for it. A setup timeout, as Kafka's producer has
  (`socket.connection.setup.timeout.ms`, 10 s), would bound the wait after a path heals, at the
  cost of a pending place at the server for each dial given up until its own idle timeout; it is
  not made here.

## What the simulation does

Each seed draws a run (`tests/sim.rs`): 6 s of virtual time with a publish every 2 to 30 ms
through a node drawn at random, and a fault every 0.2 to 2 s, a kill 40% of the time, a partition
25%, a disk stall 20%, nothing 15%.

- **kill -9 of a node:** of the topic's leader half the time, else of any node. Where it lands,
  in the node's own course: 40% of the time at its host's n-th await from then (n below 400), else
  at its disk's c-th cut from then (c below 12), before the cut (what it hadn't cut is lost, as
  hyper-log's queue is with a process), during it (power cut after up to seven of the cut's writes
  and flushes: a torn frame, a frame without its confirmation) or after it, before the members
  take its answers (the leader killed between its flush and its acknowledgement), a third each.
  The node's devices are then crashed as a kill leaves them (`Crash::KeepAll`, the OS kept what
  was written) or as a power cut does (`Random`, `LoseAll`), a third each, and the node starts
  again 20 ms to 2.5 s later from what its devices and files kept.
- **Partitions:** a pair of nodes cut both ways (70%) or one way, for 50 ms to 2 s.
- **The network**, drawn for the seed: one way 0.1 to 2 ms, up to 0.4 ms either side; each flow in
  order or, on half the seeds, reordered by its jitter; up to 2% of datagrams lost one by one or,
  on half the seeds, in bursts (entered by up to 1% of datagrams, 2 to 10 long on average, losing
  30 to 100% of what they hold, the chain stepping once per datagram and once per millisecond a
  flow is quiet, hyper-rt's `SimLoss::in_time`, so a burst ends in time); up to 2% delivered twice, each copy after a delay of its own
  (hyper-rt's `SimPath::with_duplicates`). QUIC discards a packet it already processed (RFC 9000
  §12.3), so the duplicates Quiver's code meets are resends: a publisher's after a redirect, a
  silent leader or a lost connection, and Raft's own. The leaders must recognise each, and a seed
  counts what they did.
- **The disk:** each cut's latency drawn, 85% 0.05 to 1 ms, 14% 1 to 10 ms, 1% 10 to 120 ms; a
  stall of one disk (70%) or every disk at once (30%, doc 34's shared disk) for 0.2 to 2.5 s holds
  each cut that falls in it until it ends.

After the 6 s every partition mends and every node is up, and the run goes on until every node
has applied the same offsets and every publish is answered, at most 60 s.

## What each run checks

While the run goes on, every 100 ms of virtual time, each running node's view: what it holds at
each offset is what every view before showed there (an applied publish never changes, across
nodes, kills and recoveries), every publish acknowledged so far within the offsets it holds is
there, and no payload is there twice. So each recovery is checked as it catches up. At the end:

- every acknowledged publish is at the offset it was acknowledged with, in every node's view;
- each producer's acknowledged publishes are in its order;
- no record is there twice;
- the nodes' views agree;
- every node caught up, and every publish was answered.

Lost acknowledged publishes, AC-T2.2.5's measure, are those some node's view lacks at their offset
at the end; a run breaks on any check. A run's trace is its seed and the schedule it drew, each
fault, kill, crash and restart with its virtual time; its digest folds the trace, every publish's
outcome, every view, the devices' operations and the fabric's counts. Every 100 ms the run marks
where it stands (each node's awaits and cuts, its devices' writes and flushes, its groups' figures,
the fabric's counts), so a replay that runs apart says where. A failing seed's trace is kept with
what it broke, its nodes' own account and its marks, and the seed runs again to the same digest.
Each seed's line counts its kills by where they fell and whether they took the leader, the crash
states, partitions and stalls, the resends and the leaders' recognitions, the views checked, and
the datagrams the fabric duplicated and lost.

## The planted bug

A test build (`--features plant-own-log-commit`) plants hyper-raft's `Mutant::OwnLogCommit` in
every member's replica. The group then commits what its leader's own durable log holds, before a
quorum's flush. Without faults no acknowledged publish is lost. A leader killed after its own flush,
before the followers have the entry, loses it once they elect a leader without it. The simulation
must find this and replay it from its seed
(`the_planted_acknowledgement_before_a_quorums_flush_is_found_and_replayed`): a correct build
breaks no check, so any check the planted build breaks is the bug found. At `c6baec2` it is found
at seed 3, applied publishes changed (nodes 1 and 2 differ at offsets 173 and 174), and replayed to
the same digest. hyper-raft's own oracles catch it too (`tests/check.rs`, seed 0).

## The changes to vendored code

Each is recorded in its project's `UPSTREAM.md` and offered upstream (hyper-raft's seams on the
local branch `cp/t2.2-told`, its fixes on `cp/t2.2-liveness` at `a91f1e0` and
`cp/t2.2-token-log` at `20cdaf9`). The seams change nothing a production build does: the told and
serve paths need `Waits::Told`, the anchor and binds a simulated shard, Ed25519 identities and
bursts stepped in time to be asked for, and the rest a feature. The liveness and token-log fixes do: they are
fixes.

| Project | Change |
|---|---|
| hyper-raft `hyper-log` | `Waits::Told` and `Log::cut`: a frame only when told, its confirmation made one way. `Log::serve` for waking fetches. A restore at open, and a caller that waits on its write or fetch, tell the log by waiting |
| hyper-raft `hyper-rt`, `hyper-rt-transport` | `sim::anchor` and `shard_instant`: one clock for every driver on a simulated shard. A simulated bind takes the port asked for in its shard. A simulated path may duplicate datagrams (`SimPath::with_duplicates`). A loss burst ends in time on a quiet flow (`SimLoss::in_time`) |
| hyper-raft `hyper-raft`, `hyper-durable` | `Mutant::OwnLogCommit`, and `Replica::plant` to reach it (`mutants` features) |
| hyper-raft `hyper-liveness` | A stream whose floor is nothing still sends once a flush proves a heartbeat (above) |
| hyper-raft `hyper-quic` | A token log takes a valid token issued before the first it checked (above) |
| Focal `focal-bus` | `NodeFile` and `Member::open_simulated` (`sim` feature): a member's log on a simulated device, its data directory sealed as `Member::open` seals it. `Member::cut`, `serve`, `in_flight` and `plant` (`mutants` feature). The owed acknowledgements redirected in order. A member's diagnosis, `Member::core_state` (`sim` feature). A directory's identities made as Ed25519 when asked (`pki::KeyKind`) |

## Runs

Each seed takes three nodes through 6 s of faults and up to a minute of catching up, in virtual
time, in a few seconds of a core. The sweep (`bench/sim-sweep.sh`) runs the 10,000 seeds twice, in
two partitions over processes, each pass in a directory of its own holding one copy of the
cluster's identities. Its record is `bench/records/2026-10-26-adacachy-sim-sweep/`: at `a52fb28`,
on adacachy under production's caps (fedora had stopped answering), 0 of 3,181,568 acknowledged
publishes lost in each pass, no seed broken, and every seed's digest alike in both passes.

The sweeps before it, and what each found, are above: at the older pins 77 runs left a node behind
(the bus's outbox, hyper-raft's liveness); at `d4ad260` eight nodes back from kills were refused
Busy; at `bf54d1e` three handshakes met a burst that never ended, and 1,067 seeds ran apart from one
process to another. None lost an acknowledged publish.
