# The placement kernel

`kiln_ci::lb` decides where a run goes: in O(1), at the node that asks for it (its owner), and
admitted by the runner whose room it takes, never past that runner's budget. The place core's
owner picks through it and its runner keeps its ledger in it; the shell (`crates/kiln`,
`reach/place.rs` on lb/wire) carries the messages. It serves docs/ci.md W05-R03 (capacity, never
past it; the first waiting run keeps its room), W05-R04 (signatures and their eligible runners,
kept as runners join and leave, so a decision looks only at them), W05-R05 (no run started
twice), W05-R16 (a refused run goes to another runner that can take it), W05-R23 (least loaded,
then most free) and W05-R26 (speed).

Like the rest of kiln-ci it is pure: no clock, disk, network or randomness; integer amounts;
nothing hashed, and nothing allocated on a decision once its tables have grown to the fleet.

## What it decides

- **Where a run starts.** An ask names a signature (a set of constraints, interned when first
  seen) and a need (`[cpu, mem, disk, gpus]`: CPUs in thousandths, bytes, bytes, GPUs). The owner
  picks the member with the most room by its rank key whose room covers the need in every
  dimension, deducts the need from its view at once and sends the runner a Start. The runner
  admits it against its own ledger (`Started`) or declines it with its room (`Declined`), and the
  owner places a declined run again.
- **Where a run waits** when no member has room: at two runners nearest to fitting it. The first
  to hold its room says Ready, the owner says Go to it and Withdraw to the other, so it starts
  once. A run waits nowhere when no member is reachable, and is sent to wait when one comes back.
- **Never**, when no member's whole budget covers the need.
- **What became of runs** across a dropped link or a runner that started again (below).

It decides nothing about fair share between askers (the runners' lines are first come, first
served), retries, or which runners meet a signature (routing, W05-R22, decides membership; the
owner is told it).

## The fast path

- **Dense ids.** Runners, owners and signatures are numbers the shell interns once. Runs are
  generational slab handles (`{slot, generation}`): a word about a run that is gone, its slot
  since reused, finds nothing.
- **The owner's view** of a runner is its last word less what the owner has sent it and not
  heard back about. It deducts at once, so starts in flight don't herd onto one runner (a test
  holds it: with one owner, a runner never declines one for room).
- **The index** (one per signature): a membership bitset per bucket over runner ids, interleaved
  by word, so a runner's bits for every bucket share a cache line and a move writes two bits and
  touches no other runner; a summary word per 4,096 runners; a bitmap of non-empty buckets (one
  word for buckets under 64, two levels to 4,096). Equals take turns from a cursor per bucket;
  each bucket keeps the last member put in, the next in turn when it's the only one (the bucket
  the runner a run just ended on enters, the commonest pick).
- **The pick:** the highest non-empty bucket, its next in turn, its CPU checked at the view and
  every other dimension exactly; when that member is short of what's asked, a walk member by
  member down to the bucket whose range holds the need, so an exact fit there is found (counted:
  `Counts::walked`).
- **The rank key** is `(view * scale) >> 40`. At the reference speed `scale` is ceil(2^40 / 1000),
  and the key is whole free CPUs, exactly, for every view below 2^32: a multiply and a shift, no
  divide. A rebucket is skipped when a word leaves the runner in its bucket (the Started after
  the owner deducted).
- **The runner** admits when the need fits its room for a new run: what's free less what the
  first run in its line keeps. A run passes that one only in room it can't use (EASY
  backfilling's guarantee to the first in line, Mu'alem and Feitelson, IEEE TPDS 12(6), 2001,
  without run times). Every word it sends carries its room and its epoch.
- **Records:** the owner's record of a runner is 32 bytes, aligned (view, in flight, epoch, ends
  heard, key multiplier, first index, flags, speed): four to a 128-byte line, none straddling
  two. A runner's hot fields are its first 128 bytes. An admitted run is 24 bytes (its memory,
  disk and GPU needs in a side slab when it has any). A run at the owner is 32 bytes.
- **Messages** are `Copy` values. A start or an end of CPU alone, for a runner's one owner, takes
  a short path on each side; anything else (memory, disk or GPUs asked, several owners, a line,
  another epoch) takes the general one.

## Off the fast path

- **Declines** say which word they answer (`Of::Start`, `Wait`, `Go`): a run can have an old Wait
  and a newer Start in flight to the same runner when it was placed again as that runner started
  over. A decline carries the runner's room, so the next pick never repeats the miss; the loop
  ends when no member fits (then the run waits) and is bounded by the members.
- **Waiting** at two runners: d = 2. With two choices the most loaded of n bins holds
  log log n / ln 2 + O(1) instead of Θ(log n / log log n), and each choice past the second only
  divides the first term by log d (Azar, Broder, Karlin and Upfal, SIAM J. Comput. 29(1), 1999);
  for queues the second choice is where the exponential gain is (Mitzenmacher, IEEE TPDS 12(10),
  2001); Sparrow measured two probes a task with late binding as its best trade of messages for
  waiting (Ousterhout et al., SOSP 2013). The places are the two members nearest to fitting by
  room. A waiting run is started directly when a member's room rises enough for it.
- **Fences.** A start, wait or Go under an epoch that isn't the runner's is declined: handles
  mean nothing to a runner that started again.
- **A dropped link** loses what was on it; both ends are told. The owner takes the runner out of
  its buckets and is unsure of what was starting, started or going there (again on a drop before
  the last one's Synced arrived, so a run said again and then ended isn't held for ever). When the link is back
  the runner says each live run of the owner's again (Started), then Synced with how many of
  that owner's runs have ended there this epoch. The owner's missed ends (that count less the
  ends it heard) tell a run that ran and ended while the link was down from one whose start never
  arrived; where they can't (some of several did), each is told Lost and never run again: at most
  once. No per-run log is kept.
- **A runner that started again** (a newer epoch in any word) has lost every run it had: those
  are Lost; what waited there waits elsewhere; starts sent under the old epoch come back declined.
- **Hints.** A runner's room changes on every start and end. The owner it is answering hears it
  in the answer; other owners that watch it are owed a Hint, one each however many changes, sent
  when the shell drains them (backpressure, no timer).
- **Giving up.** `Owner::cancel`: a waiting run is forgotten at once, its places withdrawn; one
  whose start or Go is in flight is marked, its Started still told (so the shell ends it at its
  runner), a decline never placed again.
- **Room the ledger doesn't hold.** `Runner::outside` (the shell's measured use beyond what's
  admitted) is kept off the room for new runs, with what the first in line keeps; a waiting run is
  held only when what's left covers it. `Runner::set_budget` and `Owner::set_budget` follow a budget
  that changes: what's free is worked out again from what's held, and runs past a smaller budget run
  on, nothing new admitted until they fit.
- **Batches.** `ask_many` decides `n` runs of one need in the order one at a time would. For whole
  CPUs alone on runners whose keys are whole free CPUs, a member of bucket b goes to b - c, so the
  index alone decides the batch and the members' records are then updated in a pass of
  independent steps.

## Rank: expected finish

Not load share. Proxies normalize by capacity because a request uses a sliver of one server.
kiln's runs are mostly parallel (builds and tests run `-j` wide) and a reservation is admission
bookkeeping, not a CPU quota, so a run's finish follows the idle capacity it would get times the
machine's per-core speed, less contention for what isn't reserved, plus warm state. The objective
on a mixed fleet is the run's expected finish there. Each runner's word carries two measured
integers for it: `speed` (per-core speed in 1/1024 of a reference core's) and `slowness`
(per-core slowness under the contention it measures now, in 1/1024 of uncontended), 0 until
measured (`Runner::measure`). Wide asks rank by core-equivalents, free cores times speed (the key's
`scale`), built; narrow asks are to rank by slowness in a second bucketed order, not built. With
nothing measured the order is free cores, the same as on a uniform fleet. Speed and contention are
the sense and history work's to measure; the kernel only orders by them.

## Several owners

Each owner deducts only its own sends, so owners with the same view choose alike. Each index's
cursors start at the owner's place in the fleet (its id times floor(2^32 / φ), Fibonacci hashing:
Knuth, TAOCP vol. 3, §6.4), so that owners would pick different runners among equals.

Measured, it doesn't hold the decline rate down. `cargo test -p kiln-ci lb::tests::herding --
--nocapture`: 64 runners of 8 CPUs, 400 rounds, each round K runs end, every owner hears, then
each owner decides one run from that same view before hearing of the others'. Starts declined per
decision:

| K | rho 0.5, unspread | spread | runners in the top bucket | rho 0.9, unspread | spread | runners in the top bucket |
|---|---|---|---|---|---|---|
| 1 | 0.000 | 0.000 | 1.0 | 0.000 | 0.000 | 10.8 |
| 2 | 0.000 | 0.000 | 9.1 | 0.001 | 0.000 | 7.9 |
| 4 | 0.000 | 0.000 | 5.1 | 0.266 | 0.274 | 6.5 |
| 8 | 0.006 | 0.005 | 8.3 | 0.692 | 0.698 | 3.9 |
| 16 | 0.440 | 0.442 | 9.4 | 1.502 | 1.526 | 2.8 |
| 32 | 0.999 | 0.881 | 12.2 | 3.097 | 3.102 | 2.6 |

One owner is never declined. Past that the spread changes nothing but the K = 32, rho 0.5 row.
Cursors only order a bucket, and the highest bucket holds few runners when owners decide (2.6 at
K = 32, rho 0.9), so every owner goes to those. Admission keeps this correct, since a runner
declines what doesn't fit; each decline is a round trip and a pick again.

This is the known result on stale load information (Mitzenmacher, "How Useful Is Old
Information?", IEEE TPDS 11(1), 2000): picking the apparent best herds, a small random choice
doesn't. An owner told it shares its fleet (`Owner::set_spread`: the rate the others decide at,
measured, and a word's delay, both passed in) widens its pick by the herd it should expect, the
decisions others make in that delay: `others_rate * delay + 1` candidates, from the top buckets
down in the order the buckets and turns give them, one drawn uniformly by a generator passed in
(SplitMix64, multiply-reduced: no division on a decision). A pick then needs one load to draw
and a walk over the top buckets, and only a shared owner pays it; a sole owner keeps the exact
best through the fast path with no instruction more. The rate and the delay are the owner's
to measure, so the width has no constant in it.

Starts declined per decision, and the CV of in-flight counts across the fleet it costs:

| K | rho 0.5, exact | widened | CV exact | CV widened | rho 0.9, exact | widened | CV exact | CV widened |
|---|---|---|---|---|---|---|---|---|
| 1 | 0.000 | 0.000 | 0.000 | 0.000 | 0.000 | 0.000 | 0.054 | 0.054 |
| 2 | 0.000 | 0.000 | 0.176 | 0.134 | 0.000 | 0.000 | 0.095 | 0.069 |
| 4 | 0.000 | 0.000 | 0.302 | 0.186 | 0.274 | 0.039 | 0.097 | 0.072 |
| 8 | 0.005 | 0.000 | 0.638 | 0.193 | 0.698 | 0.094 | 0.093 | 0.081 |
| 16 | 0.442 | 0.000 | 0.649 | 0.225 | 1.526 | 0.097 | 0.094 | 0.090 |
| 32 | 0.881 | 0.000 | 0.667 | 0.320 | 3.102 | 0.115 | 0.101 | 0.131 |

Widened picks decline 7 to 27 times less at rho 0.9 and not at all at rho 0.5, and the balance
is better for any K up to 16 (the exact best's herds overload the few runners they pick: at
rho 0.5, K = 32 they are in CV 0.667 from runs pushed through declines). Only at K = 32, rho
0.9 does widening cost, 0.101 to 0.131, because the expected herd (31) is then a good part of
the fleet (64).

## Room tokens

Hints cost 2(W - 1) words a run for W owners watching a runner. The least messaging that keeps
queueing delay vanishing is about one message a job, sent when room appears, with memory at the
dispatcher (Gamarnik, Tsitsiklis and Zubeldia, Stochastic Systems 8(1), 2018, and their lower
bound, Annals of Applied Probability 30(2), 2020). Join-Idle-Queue reaches it (Lu et al.,
Performance Evaluation 68(11), 2011), as does PULL with mixed servers and several routers
(Stolyar, Queueing Systems 80(4), 2015 and 85(1-2), 2017); under skew across dispatchers, tokens
must arrive in proportion to each one's load, or be exchanged (van der Boor, Borst and van
Leeuwaarden, INFOCOM 2017). `Runner::set_tokens` and `Owner::set_tokens` turn tokens on; they are
off until set (`src/lb/tokens.rs`). Under them, for runners that hold many runs and four-dimensional
room:

- **A token is an amount**, `[cpu, mem, disk, gpus]` of a runner's room for new runs; an owner
  holds at most one per runner, which its word from that runner says (`Runner::word_for`; an
  owner joins with it) and its view files in the bucket of its CPU part, as under hints. What all
  owners hold at a runner is at most its room, in each dimension.
- **Room goes back to the owner whose run freed it**, its idle queue, less what the runner's line
  takes first: room reaches each owner in proportion to its own runs by construction, the
  non-uniform allotment van der Boor et al. need, with no rate measured.
- **Partial room splits by who freed it.** Room no owner's run freed (at the start, after a
  restart, from a budget that grew, held room withdrawn, an owner gone) is the runner's first
  watcher's: its word says all room the others don't hold. With one owner a token is the whole
  room, and every word is the one hints send (a test holds it).
- **Taking room** comes out of the taker's own token first, then out of room no one holds, then
  out of the largest tokens. Tokens aren't reservations: admission is against the runner's room
  as ever, so a token never strands room or lets a runner past its budget. An owner whose token
  shrank without its asking is owed a Hint, so views are exact once what's in flight arrives: the
  only Hints tokens send.
- **An unusable token is forwarded once.** A word says `wants` when another owner's run waited
  there for room since; an owner holding a token there with no run of its own waiting for that
  runner gives it back (`ToRunner::Release`, appended), and the runner moves it to the owner whose
  run last waited, which is then no longer the one that wants. A forward is three words: the
  release and a Hint to each of the two owners.
- **Waits are drawn.** A view under tokens is what an owner holds, not what a runner has, so a run
  no token fits waits at the members whose view covers part of it, then at members drawn uniformly
  from its signature (PULL's random server; two of them, late-bound, as before).

The model checks every property with tokens on everywhere, and adds two: each owner's view of a
runner is what that runner says it holds there, at quiescence; what the owners hold is at most the
room, at every step. Which design kiln uses where, hints or tokens, is for comparisons at scale
that aren't in this kernel's tests.

## Complexity

| Operation | Cost |
|---|---|
| Pick, start, Started, end, Hint | O(1): a few loads and stores, two leading-zeros counts, two bit moves |
| A walk (head short of memory, disk or GPUs; a fractional need at the boundary) | O(members passed), counted |
| Never | O(distinct budgets among the members) |
| Choosing waiting places | O(members passed until two that could fit) |
| A runner's line on a change of its room | O(line) |
| Link down, Synced, an epoch change | O(the owner's runs) |
| Membership (a runner joins or leaves a signature) | O(1) amortized; growing the index O(buckets × words) |

## Where each number comes from

| Number | Where from |
|---|---|
| CPU = 1000 (bucket width) | the unit amounts count CPUs in; the core is what the hardware schedules |
| BUCKETS = 4,096 | two levels of 64-bit words: two leading-zeros counts |
| SCALE = 1,099,511,628 | ceil(2^40 / 1000): the key is exact whole CPUs for every u32 view (proof in the source) |
| SPEED_ONE = 1024 | the reference core's speed, the unit of `speed` (a shift) |
| PLACES = 2 | Azar et al. 1999; Mitzenmacher 2001; Sparrow, SOSP 2013 (above) |
| SPREAD = 2,654,435,769 | floor(2^32 / φ), Knuth §6.4 |
| Tokens' two places, drawn uniformly | PULL's random server (Stolyar 2015, 2017); two, as PLACES |
| Record sizes (32, 24 bytes) | Apple silicon's 128-byte line (`sysctl hw.cachelinesize`) |
| Views to 4,294,967 thousandths | u32; a bigger runner admits exactly, its owner sees less of it |

The bench's workload numbers (C = 8, the seed, 1,000 ticks a block, three blocks at least) are
bench-spec.md's and ci/bench/common.h's.

## Correctness

`cargo test -p kiln-ci lb::`. The model (`src/lb/model.rs`) drives three owners and five runners
of mixed shapes, every pair linked, through random interleavings of asks, deliveries (each link in
order, links in any order), ends, Hints, links that drop and come back, and runners that start
again without their links dropping. It checks at every step no oversubscription (by its own count
of the starts it saw), no run started twice and the fences; at quiescence no ask lost (each runs
where its owner says, waits in a line its owner names, or is gone with its asker told), nothing
running that its owner doesn't know, and every view equal to its runner's room; and at the end
that every ask ended or never fits and that the same seed gives the same trace. 48 seeds of 2,000
steps.

Each property has a planted bug that must be caught: the model catches AdmitPastRoom, GoTwice,
DropDeclined, NoFence, NoGeneration, NoHint and Unordered; unit tests catch NoDeduct (one owner's
starts herding), NoKeep (the first in line starves), NoTurn (equals don't take turns) and
MostLoaded. The model also drives askers giving up and room taken outside the ledger.

With tokens on (`lb::model`, its tokens run), the model also catches NoHint (a shrunk token not
told) and TokensOverlap (owners holding more than the room). `lb::scale_tests` pins each rule of
tokens on a small case, each with its planted mistake caught:
FreedToFirst, NeverWants and ForwardAgain, NearestPlaces.

## Measured

`cargo bench -p kiln-ci --bench lb -- [--json] --n N --rho R --d D [--runs K] [--batch K]
[--impl kiln|floor|owner|runner]`. The bench follows ci/bench/common.h phase for phase, so it
compares with every competitor's driver: SplitMix64 from seed 0 with Lemire's reduction, the null
implementation measured the same way, the fill, W warm-up steps, timed blocks of B steps (the
smallest power of two whose null block lasts 1,000 ticks), the median block, quality every N
steps, and the release and pick called through pointers the compiler can't see. Release-only is
measured on copies of the state (common.h tops the state up past capacity, which an
implementation that admits exactly can't take) with the clock's own cost taken off, and isn't
reported below 64 requests in flight.

Apple M3 Ultra (adas-mac-studio.local-2), 2026-11-07, load 12 to 14 of 32 cores; ns per step
net of the null; runs until the median's 95% interval is within 5% or 30 runs:

| N | rho | kiln d = 1 | 95% interval | d = 2 | d = 4 | batched, K = 64 | floor | cv |
|---|---|---|---|---|---|---|---|---|
| 100 | 0 | 38.0 | 36.9-38.2 | 63.1 | 64.5 | 35.4* | 0.34 | (one in flight) |
| 100 | 0.5 | 33.4 | 33.1-33.9 | 63.7 | 67.2 | 37.1 | 0.83 | 0.000 |
| 100 | 0.9 | 38.2 | 37.6-38.4 | 70.8 | 69.9 | 39.1 | 0.79 | 0.056 |
| 500 | 0 | 40.2 | 39.5-41.1 | 67.0 | 68.7 | 37.0* | 0.34 | (one in flight) |
| 500 | 0.5 | 46.1 | 44.1-46.8 | 85.5 | 86.8 | 44.2 | 0.73 | 0.000 |
| 500 | 0.9 | 50.2 | 50.1-51.4 | 92.1 | 91.8 | 46.5 | 0.73 | 0.056 |
| 4,095 | 0 | 41.1 | 39.9-42.6 | 67.7 | 69.2 | 37.0* | 0.34 | (one in flight) |
| 4,095 | 0.5 | 59.2 | 57.7-61.8 | 118.8 | 108.8 | 61.6 | 0.79 | 0.000 |
| 4,095 | 0.9 | 76.2 | 65.9-90.3 | 135.6 | 139.0 | 71.6 | 0.96 | 0.056 |
| 10,000 | 0 | 42.9 | 41.1-44.1 | 68.8 | 68.1 | 46.9* | 0.34 | (one in flight) |
| 10,000 | 0.5 | 99.4 | 83.6-108.7 | 154.9 | 162.5 | 74.6 | 1.22 | 0.000 |
| 10,000 | 0.9 | 133.5 | 111.6-144.2 | 233.9 | 238.2 | 96.9 | 1.18 | 0.056 |

\* At rho = 0 the batch needs K in flight: a burst of 64 into an idle fleet.

Quality in every cell: no oversubscription, no refusals, and the cv of in-flight counts the least
the integers allow (0 at rho = 0.5, where every runner holds 4; 0.056 at 0.9, where they hold 7 or
8). The floor is a count per backend decremented on release and round robin on pick, no load read
and no admission: what keeping per-backend state costs on this machine at all.

Where the step goes, measured apart at N = 100, rho = 0.5: the owner's side 17 ns (its release
7.8), the runners' side 11 ns, the bench's carrying of messages the rest. Each part is a short chain
of dependent loads through a different structure (the owner's record of the runner, its run slab,
the index, the runner's ledger, its run slab): it is latency, not instructions, at every N, and at
N = 10,000 those structures (about 10 MB) are in L2.

The competitors on the same machine, one run each of the bench's drivers at rho = 0.5 (the
interleaved rounds of `ci/bench/run.py` are the comparison that decides):

| Algorithm | N = 100 | kiln faster by | N = 10,000 | kiln faster by |
|---|---|---|---|---|
| Katran maglev | 10.2 | 0.31x | 10.4 | 0.10x |
| HAProxy static-rr | 12.7 | 0.38x | 28.3 | 0.28x |
| HAProxy roundrobin | 25.0 | 0.75x | 96.6 | 0.97x |
| HAProxy leastconn | 43.6 | 1.3x | 160.6 | 1.6x |
| nginx random | 45.7 | 1.4x | 320.6 | 3.2x |
| nginx random two least_conn | 64.4 | 1.9x | 383.0 | 3.9x |
| nginx least_conn | 171.7 | 5.1x | 29,574 | 298x |

The bar (10 times every algorithm, the load-unaware ones included) is met against nginx
least_conn at N = 10,000 alone. Against Katran's Maglev lookup it would take about 1 ns a step,
which is the floor itself (0.8 ns at N = 100, 1.2 at 10,000): no implementation that releases and
picks with per-backend state can be ten times faster than a stateless hash at that size.
Against the load-aware algorithms it would take 4.4 ns at N = 100 (HAProxy leastconn) and 16 ns at
10,000; kiln's step, at 33 and 99, is the five chains above one after the other. The competitors
without admission oversubscribe (static-rr 372 picks onto full backends in this run, Maglev
5,471, nginx random 5,000); kiln never does.

## What remains

- The step's chains: messages kept out of memory between owner and runner, fewer structures a
  step touches (the run slabs above all), and a batched path that restructures per-item work so
  items' chains overlap. Today's batches loop over the same per-item code and gain at N = 10,000
  alone.
- Narrow asks' order by per-core slowness, the second bucketed order.
- The contended path isn't this kernel's: fair share, classes, floors, promotion, predicted ends and
  answers with reasons are `kiln_ci::place`'s, and this kernel becomes its hot path.
