# The placement kernel

`kiln_ci::lb` decides where a run goes: in O(1), at the node that asks for it (its owner), and
admitted by the runner whose room it takes, never past that runner's budget. The place core's
owner picks through it and its runner keeps its ledger in it; the shell (`crates/kiln`,
`reach/place.rs` on lb/wire) carries the messages. It serves docs/ci.md W05-R03 (capacity, never
past it; the first waiting run keeps its room), W05-R04 (signatures and their eligible runners,
kept as runners join and leave, so a decision looks only at them), W05-R05 (no run started
twice), W05-R16 (a refused run goes to another runner that can take it), W05-R23 (least loaded,
then most free; a run's expected finish), W05-R26 (speed) and W24-R18 (warm state and what must
move, in the expected finish).

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
  divide. A runner's measured speed and slowness are folded into its `scale` when its word says
  them (below), so its key is its core-equivalents under contention; it is 0 exactly when no whole
  core is free, and at least 1 otherwise. A rebucket is skipped when a word leaves the runner in
  its bucket (the Started after the owner deducted).
- **The runner** admits when the need fits its room for a new run: what's free less what the
  first run in its line keeps. A run passes that one only in room it can't use (EASY
  backfilling's guarantee to the first in line, Mu'alem and Feitelson, IEEE TPDS 12(6), 2001,
  without run times). Every word it sends carries its room and its epoch.
- **Records:** the owner's record of a runner is 32 bytes, aligned (view, in flight, epoch, ends
  heard, key multiplier, first index, flags, measure count): four to a 128-byte line, none straddling
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
bookkeeping, not a CPU quota, so a run's finish follows the cores it gets times their speed under
the contention there, plus what it reads and writes, what it fetches, what must move to it first,
and warm state. The objective is the run's expected finish.

Each runner's word carries what it measures of itself (`Runner::measure`,
`Runner::measure_rates`), 0 until measured: `speed` (per-core, 1/1024 of a reference core's),
`slowness` (per-core under the contention it measures now, 1/1024 of uncontended), `disk` (its
disk's effective rate under its load, bytes a second) and `net` (its own network's). The owner
keeps its link to each runner (`Owner::set_link`, the shell's measure of its own transfers: rate,
latency, mean time between drops, mean outage). A word's stamp carries a count of the runner's
measures (`Word::measured`), so the fast paths take a word only while nothing measured changed;
the first word after a measure goes the general path once.

**Wide and narrow.** A run with work W (CPU time on a reference core) and parallelism p (the
most cores it can use) on a runner with f cores free and per-core rate s (speed over slowness)
finishes in W / (min(p, f) s). Every runner with room for its CPU need c has f >= c. So when p <= c
min(p, f) is p on every candidate and the order is s alone: the run is **narrow**, and it goes to
the runner whose cores are fastest under contention, the most free of those. When p > c it gains
from free cores past its ask: it is **wide**, and the order is core-equivalents under contention, f
times s (the index's key), exact when p is at least every candidate's f. p is the ask's learned
parallelism (`Terms::parallel`); not learned, its CPU need is the best word on it, so an ask
without one is narrow.

**Folded at word time.** Speed and slowness are a runner's own integers, known when its word
arrives, so the owner folds them then into one multiplier, `scale` = SCALE x speed / slowness
(each 1/1024; past a u32, 3.9 times the reference, it is that): the index's key, the narrow
order's class and the CPU time all read it. A plain ask is then a lookup again. A narrow one takes
the narrow order's highest class, its next in turn (or the index's next in turn at its top when
that one is in the same class), its CPU checked at the view: every member of the order has a whole
core free, so a need of one core never misses, and a bigger one walks as the index's pick does. A
wide one takes the index's top, its next in turn, checked exactly. Disk, link and network rates
bear only on asks that carry those terms, so they don't take plain asks off the fast path. With
nothing measured, or every multiplier the reference's, both orders are free cores and the index
decides as before. The buckets cover the most core-equivalents any runner's whole budget has.

## Orders

Beside its index each signature keeps, once something is measured, an order by each rate a run's
finish divides by: the key multiplier, per-core speed over slowness (narrow asks), disk, this
owner's link, the runner's network (`lb::Order`). A member's class is its rate's place on a log scale: the octave (its top bit) and
the six bits below it, 64 classes an octave, rates in one class within 1/64 of each other; 64
octaves of a u64 times 64 is BUCKETS, so the highest class is two leading-zeros counts away, as
the index's top bucket is. Members of a class take turns in a ring through their neighbours:
memory O(runners + classes), where the index's bitset per bucket would be O(runners x classes).

A class changes only when its rate does (a measure, a link measured again), never on a start or
an end. An order keeps only members with a whole core free (their key not 0): a runner moves in or
out when its key goes to or from 0, a function of the key, so the fast paths' "same bucket, no
move" holds for the orders too. The narrow order, which plain asks read, follows at once; the disk,
link and network orders, read only by asks with those terms, follow when one of those reads them:
a key that goes to or from 0 marks its runner (one flag, once in a list), and `ask_with` applies
the list first, O(runners marked). Room is checked at the pick, member by member from the highest
class down, members passed counted (`Counts::walked`); without the gate, at N = 10,000 a fleet of
seven disk classes filled its top class and each pick walked its 1,430 full members (2.3 µs a
decision, below). The narrow pick is the first member with room from the top class, or the
index's next in turn at its top when that one is in the same class (as fast a core, the most
free); none in it, the index's pick.

## Per-ask terms

`Owner::ask_with(sig, need, &Terms, out)` decides an ask with what bears on its finish, each an
integer the shell computes from history and its measures (W24-R18's queue wait + cold duration x
(1 - warmth) + missing bytes / bandwidth, with disk, network and the link's stability added):

| Term | From the ask | From the runner | Time |
|---|---|---|---|
| queue wait | a listed runner's `wait_ns` | (no room now) | as given |
| transfer | `bytes`, or a listed runner's `missing` | the owner's link rate | bytes / rate |
| latency (interactive) | `interactive` | the link's round trip | as given |
| CPU | `cpu_ns` less a listed runner's `saved_ns`, `parallel` | speed, slowness, cores free | W / (cores) x slowness / speed |
| I/O | `io_bytes` | disk rate | bytes / rate |
| network | `net_bytes` | network rate | bytes / rate |
| stability (interactive) | `interactive` | the link's drops and outages | P(drop during the run) x mean outage |

**Candidates: the threshold.** The finish is a sum of terms, each an ask's volume over one of a
runner's rates (its CPU time over its cores and key multiplier), and each order lists the members
by one of those rates. No fixed count from each order bounds how far the best of them is from the
soonest: a member second in two orders and first in neither beats both firsts by any factor (two
terms, firsts at (0, M) and (M, 0), it at (1, 1)); each order's first alone was optimal in 560 of
582 asks with several terms, 1.53 times the soonest at p99 and 2.19 at worst. So the owner runs
Fagin, Lotem and Naor's threshold algorithm ("Optimal Aggregation Algorithms for Middleware", JCSS
66(4), 2003; arXiv cs/0204046, §4 and §6.2). It reads the orders the ask's terms make relevant
(by CPU, the narrow order and for a wide ask the index; by disk for I/O; by the owner's link when
bytes must move; by the runner's network when it fetches) one member each in turn, from the top,
and works out each one's finish. The threshold is the least finish a member no order has reached
yet can have: each term at its order's place (its class's highest rate; for the CPU time, the
parallelism times the narrow class's highest multiplier and, for a wide ask, the core-equivalents
the index's bucket allows), latency, stability and waits at 0. It stops once the soonest seen is
no later than the threshold times 1 + 1/64. The choice is then within 1/64 of the soonest finish
of every member with a whole core free and every listed runner (their Theorem 6.6), and no
algorithm that reads the orders without guessing reads fewer than a constant times as many
members on any fleet (Theorem 6.7, instance optimality). 1/64 is the orders' own resolution:
members of a class aren't ordered within it, so a finer bound would read classes whole. When one
member is in the current class of every order it reads (one runner first at everything) it stops
after the first round, O(orders + listed) as before; it reads further only as far as the fleet
makes it. The shell's short list (`Terms::warm`: runners with a known saving, their own missing
bytes, and a wait when busy) is read first. No candidate has room: the run is placed as a plain
ask is (a need below a core may fit a runner with less), or waits as any does.

**What is compared.** A term counts once any runner has measured its rate (disk, network, link).
A runner not yet measured takes the prior: the mean, over the runners that did, of ns a byte (the
harmonic mean of their rates, since expected time is bytes times E[1/rate]), the posterior mean of
a unit with no observation of its own (empirical Bayes: Robbins 1956; Efron and Morris, JASA 1973),
as an unmeasured machine's speed is its fleet's in `crate::pace`. It is filed in the rate's order at
the prior's class, so one new runner doesn't switch a term off for the fleet, and is neither
favoured by a missing term nor shut out by it. What is compared doesn't depend on how far the walk
read, and with nothing measured the choice is the plain one. Each byte rate is a multiplier in
2^-32 ns per byte, worked out when it is measured (`Rates`, as the key's `scale` is); the CPU time
is W x SCALE / (cores x `scale`), one divide by the cores a run gets times its key multiplier.

**Waiting for a warm runner.** A listed runner without room is a candidate with its wait. When it
is the soonest, the run waits there alone (one place, not its signature's waiting runs, so room
elsewhere doesn't take it); declined there, it is placed again as any run.

**Interactive and batch.** Latency and stability count only for a run that streams to a person (an
agent session, a live log: `Terms::interactive`): the person waits on every exchange and through
every outage of the owner's link to it. A batch or background run doesn't: once started it runs on
while the link is down (the runner keeps its ledger and says it all again on Synced), so neither
delays its finish. Drops are a Poisson process at the measured rate (Ross, Introduction to
Probability Models, section 5.3: the count in a span T is Poisson with mean T / mean time between
drops), so P(one or more in the run's predicted length T) = 1 - e^(-T / mean time between drops),
in integers: e^-x to within 2^-25 (`exp_neg_q32`, no divide), the rate a multiplier in 2^-96 ns.

The policy over these is the shell's: whether a laptop on battery takes others' work, how long a
run may wait for a warm runner (W24-R18's share of what it saves) are a person's settings; the
kernel only orders and compares.

## Thermals and battery

A machine that throttles (heat, a battery's power limit) measures a lower speed, and its owners
hear it through `Runner::measure`: no rule of its own. On battery the shell may lower the runner's
budget (`Runner::set_budget`) or cordon it (`Runner::cordon`): it takes no new run, its words say
no room so owners pick elsewhere, its runs run on, runs waiting in its line stay (each waits at
another runner too) and are held when it opens again. The place core cordons its kernel while its
runner takes no work: draining (an upgrade pending, or `[runner] on_battery`) or under strain.
Under room tokens a cordon leaves no room to hold, so every token is cut to none and each owner
that held some is told; the room that comes back is no owner's run's, so the first watcher's.

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
to measure, so the width has no constant in it. On measured runners a plain ask's general path
draws the same way (the drawn one of the narrow order's members with room, from the top), and
`ask_with` draws among the candidates the threshold has seen within its bound of the soonest, so
every draw keeps the bound.

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

## Adopted: the place core's hot path

`kiln_ci::place` asks the kernel first, and keeps everything contended (docs/reach.md §2.3).

- **The owner** (`place::owner`): an ordinary ask (not `exclusive`, no gang) goes to the kernel's
  `ask` on its kind of ask's signature (the suit's live runners that named `place.fast` and said
  their word, synced when the suit or those runners change). A member with room gets the run at
  once: its ticket is held there as a taken offer is, its tree copied, then the kernel's start
  sent with the spec (`ToRunner::Fast`). No member with room: the kernel's run is cancelled and
  the ticket is probed as before. A decline (`FastDeclined`) goes through `hear`: placed again
  on another member, or, when none has room (`Told::Waiting`) or could (`Told::Never`), cancelled,
  its waits dropped unsent, and probed with the same ask. A start given up before it was sent is
  `Owner::unsent`: its room back in the view at once.
- **The runner** (`place::runner::Admission`) keeps one ledger. A run the kernel admits is a claim
  in the admission from its start: its room, its tenant's charge, its predicted and hard ends (kept
  room and expected starts count it), its record. The kernel's ledger is the admission's index:
  its budget the admission's cap, its room for a new run what's free for an ordinary run while
  nothing waits in the line and no run holds it whole, else none (`sync_fast`, at the close of
  every input; everything else is its `outside`), and it's cordoned while the runner takes no
  work (draining, on battery, under strain). So a run takes its room once in each whichever path
  admitted it, and nothing passes a line: while runs wait, owners' kernels see no room there and
  probe. Runs probing admits don't enter the kernel's
  ledger (they are `outside`), so its end counts are the kernel's runs' alone.
- **Words**: `FastStarted` once the process has started (the asker hears it started then),
  `FastDeclined` with the room there (a decline not about room says none, so it isn't picked
  again before the runner's next word), `FastEnded`, then the owner's `Ack` drops the record. Each
  carries the kernel's word as the input left it (`FastWord`: epoch, room, budget, speed,
  slowness); hints carry it too (`offers`' `fast`), and the server passes them on.
- **The server** owns CI legs (`crates/kiln/src/hub/legs.rs`): an ordinary leg's runner is the
  kernel's pick among the runners that named `place.fast` and hint their words, by the same rules
  the engine's pick filters by; the leg's room is taken from the view until its runner is heard
  of (started, refused, released). What the kernel can't place, the engine's own pick does.
- **Room tokens** (`[reach] tokens`, off until declared; below): a runner that declares them tells
  each owner whose link named `place.tokens` what it holds there, its own word (`tokens` in its
  answers' words, and `FastHint` for what's owed besides), and that owner takes no hint of the
  runner's kernel epoch while it does. The server, the runner's own node and owners that didn't
  name it hear its hints, as before.

Measured (`cargo bench -p kiln-ci --bench place_fast`): see below.
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
- **A cordon holds nothing** (`Runner::cordon`): it leaves no room, so every token is cut to none
  and each owner that held some is owed its word; the room that comes back is no owner's run's, so
  the first watcher's.
- **A mixed fleet keeps hints with the owners that hear them** (`Runner::hints_for`): an owner whose
  link can't carry words of its own is told the whole room, as under hints, holds no token, never
  hears that another wants, and the room its runs free is no one's. The first watcher is the first
  owner the runner talks to that hears tokens, so that room reaches them; were it the hints owner's
  token, no owner would ever see it (it sees all the room anyway).

The model checks every property with tokens on everywhere (and again with one owner of three
hearing hints at every runner, a mixed fleet), runners cordoned and opened among its events, and
adds two: each owner's view of a runner is what that runner says it holds there, at quiescence;
what the owners hold is at most the room, at every step. Which design kiln uses where, hints or
tokens, is for comparisons at scale that aren't in this kernel's tests.

**Through the place core** (docs/reach.md §2.3; `place::runner::Settings::tokens`, `[reach] tokens`
of the runner's own config, off until declared). The runner's admission keeps them in its kernel
ledger:

- **Who hears tokens:** an owner whose link named `place.tokens` (the shell reads it off the link
  each of the owner's words comes on, `Admission::tokens_with`), its name's current process, its
  link up and it in the directory. Any other hears hints and holds nothing: an owner from before,
  the server's CI engine, the runner's own node (in its process, every word is free), an owner away,
  one that gave way to a new process, one gone from the directory. What one of those held is no
  one's, so the first watcher's.
- **What it hears:** its answers (`FastStarted`, `FastDeclined`, `FastEnded`) carry its own word,
  saying `tokens`; what it's owed besides (another took room under its token, a token forwarded to
  it, room come back to the first watcher, another's run waiting) goes at the close of each input
  as `FastHint`, one however many changes the input made, on placement's channel. Its owner hears
  that as it hears a hint, and takes no hint of that runner's kernel epoch from then (one of a
  newer epoch, the runner started again, it takes until the new process says it its own; with its
  link down it takes them again).
- **Wanting:** a run that waits in the runner's line for room (probing's: the kernel's runs never
  wait here) makes its owner the one that wants there. An owner whose word says another wants,
  with none of its own waiting in its kernel, gives back what it holds there (`FastRelease`), and
  the runner forwards it to that owner, once.
- **The kernel's room is the line's:** none while runs wait in the runner's line (they're probing's,
  its line's order, fair share and kept room deciding), so tokens carry the uncontended path, where
  the kernel decides, and the line keeps the contended one, as under hints.

A token owner hears, besides the answers about its own runs it hears anyway, a word only when what
it holds there changed without its asking; under hints every owner watching a runner hears each
change of its room, through the server. `place::tokens_tests` pins each rule through the place
core, each with its mistake caught: what an owner hears (OneWord, TakesHints), who hears tokens
(TokensToAll, AwayHolds, RetiredHolds, TokensUndeclared), wanting and forwarding (NoWaited,
KeepsTokens, and the kernel's NeverWants and ForwardAgain), a mixed fleet (the kernel's HintsHold
and FirstHearsHints) and a runner that takes no work (CordonQuiet).

## Learned wait weights

Lattanzi, Lavastida, Moseley and Vassilvitskii, "Online Scheduling via Learned Weights", SODA 2020:
restricted assignment (a run may go only to its signature's members), one weight per machine,
each job split over its eligible machines in proportion to their weights. Predicted weights within
a factor η of good ones give an O(min{log η, log m})-competitive fractional makespan with their
Algorithm 1. `Owner::set_weights` turns them on (off until set; `src/lb/weights.rs`):

- A runner's weight starts at its CPU budget (in proportion to capacity: what predicts with no
  history) or at what the shell predicts (`Owner::set_weight`).
- A run that finds no room waits at members drawn in proportion to their weights: the fractional
  assignment rounded by sampling, by rejection against the signature's largest weight.
- The fractional loads are kept lazily: a signature's waits add need / Σ weights to its
  accumulator; a runner's phase load is its weight times what its signatures' accumulators gained,
  over its CPU. T is the balanced load, each wait's need over the owner's whole CPU.
- A drawn runner whose phase load passed 2T has its weight halved and starts a phase (Algorithm 1;
  each halving closes a factor two of the error). One whose load since the weights were set passed
  2⌈log2 m⌉T means the predictions aren't helping: the owner draws uniformly again and says which
  runner and loads (`Owner::fell_back`), the switch of their Theorem 3.1.

Makespan isn't kiln's objective, and learning online with many owners is unproven (the research
says so); comparing these draws with two choices is the lead's, at scale.

## Batches and gangs

`Owner::ask_batch` places runs together, each with its own cost at each member (its expected
finish: `auction::expected_finish_ms`, the run time at the runner's measured speed and its inputs'
transfer there from the topology's `Link::transfer_us`), over the slots members' views hold now.
`Owner::set_auction` decides them by an integer auction (off: one at a time, each to its cheapest
member, the rule it is measured against). Bertsekas, "Auction Algorithms for Network Flow
Problems: A Tutorial Introduction", Computational Optimization and Applications 1:7-66, 1992
(`src/lb/auction.rs`):

- The forward auction, prices from zero (§9: more slots than runs).
- Costs scaled by min(n, m) + 1 and ε = 1, so ε < 1/n (Prop. 1 and 2) and ε < 1/m for m runners
  (§5.1, similar objects): the assignment is optimal, ε derived, not chosen.
- A runner's slots are one similarity class (§5.1): a bid's increment is against the second-best
  runner, so runs don't bid identical slots up one ε at a time.
- Feasibility first, by augmenting paths, so the auction ends.

`Owner::place_jointly` is the same decision all or none: a gang's members go where it says or
nowhere (None when the views can't hold them all). Holding every member's room and starting them
together is the caller's; the kernel has no gang protocol of its own.

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
| A measure or a link's new rate | O(signatures it is in × orders) |
| A key to or from 0, once something is measured | the narrow order: O(signatures it is in); the others: a flag, applied by the next `ask_with` |
| A pick by an order | O(members passed), counted |
| `ask_with` | O(listed + members the threshold reads): O(orders + listed) when one member leads every order it reads; a multiply a term each, one divide for the CPU time |

## Where each number comes from

| Number | Where from |
|---|---|
| CPU = 1000 (bucket width) | the unit amounts count CPUs in; the core is what the hardware schedules |
| BUCKETS = 4,096 | two levels of 64-bit words: two leading-zeros counts |
| Buckets in an index | the most core-equivalents any member's whole budget has at its multiplier, plus one |
| SCALE = 1,099,511,628 | ceil(2^40 / 1000): the key is exact whole CPUs for every u32 view (proof in the source) |
| SPEED_ONE = 1024 | the reference core's speed, the unit of `speed` and `slowness` |
| PLACES = 2 | Azar et al. 1999; Mitzenmacher 2001; Sparrow, SOSP 2013 (above) |
| 64 classes an octave | BUCKETS / 64: the 64 octaves of a u64 rate, the six bits below its top one |
| The orders' gate, a key not 0 | a whole core free: the unit the hardware schedules (CPU), and the key is 0 exactly then, so it moves with the key |
| Wide when parallelism > CPU need | W / (min(p, f) s) with f >= c on every candidate (above) |
| Multipliers in 2^-32, drops in 2^-96 | the key's precision; 41 bits or more for a mean time between drops below a year |
| e^-1 = 1,580,030,169 / 2^32; 2^32 / k for k = 2 to 6 | rounded; the series to g^6 for g < 1/16 is off by less than g^7/7! < 2^-39 |
| The threshold's 1 + 1/64 | the orders' resolution: a class is a rate's top bit and the six below it, so its rates are within (65 + m) / (64 + m) <= 65/64 of each other |
| SPREAD = 2,654,435,769 | floor(2^32 / φ), Knuth §6.4 |
| Tokens' two places, drawn uniformly | PULL's random server (Stolyar 2015, 2017); two, as PLACES |
| Weights' 2T and halving | Lattanzi et al. 2020, Algorithm 1 (x(ŵ) ≤ 2x(w) when 1/2 ≤ ŵ/w ≤ 1) |
| Weights' 2⌈log2 m⌉T | Lattanzi et al. 2020, the switch to a robust algorithm (Theorem 3.1) |
| The auction's ε = 1 on costs × (min(n, m) + 1) | Bertsekas 1992, Prop. 1 and 2, §5.1 |
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
MostLoaded. The model also drives askers giving up and room taken outside the ledger, and in half
its seeds runners that measure their speed, slowness, disk and network and owners that measure
their links.

The plain path on measured runners (`w05_r26_plain_asks_on_measured_runners_take_the_fast_path`):
4,000 random fleets of 2 to 8 runners, speed and slowness measured, then measured again (every key
folded anew), disk, network and link rates measured or not, each a run of up to 16 plain asks,
narrow or wide, of half a core to three cores, with runs ending between (keys going to and from
0). Each choice has the rank the brute force says (the fastest class with a whole core free and
room for a narrow ask, else the most core-equivalents with room), and an ask of at most a core
while a member has one free is decided by the fast entry with nothing walked: of 28,599 asks,
19,077 narrow, 15,496 decided there. Each has its planted bug, caught there: a key that keeps
stale rates (StaleKey), a narrow order that doesn't follow a key to or from 0 (NoGate).

With tokens on (`lb::model`, its tokens run), the model also catches NoHint (a shrunk token not
told), TokensOverlap (owners holding more than the room) and CordonQuiet (a cordon that cuts no
token and tells no one). `lb::scale_tests` pins each rule of tokens, weights and the auction on a
small case, each with its planted mistake caught: FreedToFirst, NeverWants and ForwardAgain,
HintsHold and FirstHearsHints (a mixed fleet), CordonQuiet, NearestPlaces; NoLearn, Unweighted,
NoFallback; EpsilonTooLarge (against the optimum found by trying every assignment, 400 cases) and
SecondInClass (a price war).
Expected finish (`src/lb/finish_tests.rs`): 20,000 random fleets of 2 to 6 runners, each order's
classes and keys their own, and 20,000 of 2 to 12 whose classes and buckets hold several runners,
measured or not term by term, one ask each with every term drawn on or off and a list of up to
four. A brute force works out every option's expected finish from the terms as written above
(each runner with a whole core free and room, each listed runner with room or with its wait): the
kernel's choice is an option, within 1 + 1/64 of the soonest
(`w24_r18_expected_finish_is_within_1_64_of_the_soonest`). Of the asks with several terms it was
the soonest of all in 13,945 of 14,089 (classes a runner's own: p99 1.0000, worst 1.0150) and in
13,039 of 13,251 (classes shared: p99 1.0013, worst 1.0155); the orders' tops alone were the
soonest in 588 of 615, 2.06 times it at p99 and 9.7 at worst. Where one term alone bears, every
rate measured and each class a runner's own, it is the soonest. A shared owner's draws keep the
bound, and two seeds choose differently (65 of 1,997 asks; most have one candidate within it).
Each term has its planted bug, each caught there: NoSlowness, NoWarm, NoTransfer, NoDisk, NoNet,
NoLatency, NoStability, NoQueueWait, OneCandidate. NoNarrow is caught by the narrow ranking's unit
test: where both have room a narrow ask's CPU time is the same counted wide, so the threshold's
choice doesn't turn on it. A class bounds its rates, as the threshold assumes. Unit tests hold
narrow and wide ranking, a run waiting for its warm runner alone, cordon, and a new slowness alone
reaching the owner.

The threshold's reads (`w24_r18_threshold_reads_on_the_bench_fleet`, `Counts::read`), on the
bench's `--terms` fleet kept half full, with the bench's ask and every term: with its four listed
runners a median of 0 members read (p99 12 at 100 runners, 0 at 1,000); with no list, a median
of 30 at 100 runners (p99 45) and 228 at 1,000 (p99 429). There disk rates fall in 7 classes and
link rates in 5 that don't go together, and the best runners are full, so the best free one is
first in no order and the walk reads classes whole before the threshold passes it: by Theorem 6.7
any walk of the orders that doesn't guess pays as much, to within a constant factor (Theorem 6.1's
m + m(m - 1) c_R / c_S for m orders). One term alone stops at the first member with room.

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
- The server's kernel (`crates/kiln/src/hub/legs.rs`) syncs a kind of leg's members by looking at
  each agent record's address, O(agents) pointer compares a pick after any record changed; members
  kept by the records' own change events would make it O(changed).
- Words in flight between owners: a hint that left the runner before an answer reaches the owner
  after it and sets the view back (no sequence orders the two channels); the runner's admission
  declines what doesn't fit, so it costs a round trip, never room.
- Asks with several terms whose orders disagree read members by the hundred at 1,000 runners
  (above). The bound is the orders' resolution, not the inputs': the shell's learned volumes and
  measured rates are surely coarser than 1/64, and a bound from their measured error, passed in
  as `set_spread`'s rate is, would stop the walk sooner. Reading first the order whose current
  class is cheapest to finish for what it raises the threshold (Güntzer, Balke and Kießling's
  Quick-Combine, VLDB 2000) would read less on such fleets without changing the bound. Latency and
  stability have no order: the threshold counts them as 0, so an interactive ask whose link terms
  pass 1/64 of its finish reads on until its ordered terms' bound passes the best (counted).
- A decided run placed again (declined) is placed by its order alone: the owner doesn't keep an
  ask's terms.
- Room tokens through the place core change what owners hear, not yet how many words: the server
  still passes each runner's word on to every owner (owners that hear hints, and its own kernel,
  need it), and an owner that hears tokens drops it. Cutting that fan-out needs the server to know
  which owners hear tokens from which runners.
