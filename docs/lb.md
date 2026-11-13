# The placement kernel

`kiln_ci::lb` decides where a run goes: in O(1) for plain CPU asks, at the node that asks for it (its owner), and
admitted by the runner whose room it takes, never past that runner's budget. The place core's
owner picks through it and its runner keeps its ledger in it; the shell (`crates/kiln`,
`reach/place.rs` on lb/wire) carries the messages. It serves docs/ci.md W05-R03 (capacity, never
past it; the first waiting run keeps its room), W05-R04 (signatures and their eligible runners,
kept as runners join and leave, so a decision looks only at them), W05-R05 (no run started
twice), W05-R16 (a refused run goes to another runner that can take it), W05-R23 (least loaded,
then most free; a run's expected finish), W05-R26 (speed) and W24-R18 (warm state and what must
move, in the expected finish).

Like the rest of kiln-ci it is pure: no clock, disk, network or randomness; integer amounts;
nothing hashed on a plain CPU decision, and no allocation there once its tables have grown to the fleet. GPU requests use the bounded device-aware walk described below.

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
- **A run that ends, and the pick after it.** The rise of the runner a run just ended on is put
  off until something reads the buckets. When it is then alone as the most free, the pick goes
  back to it and neither move is made. When it only ties the most free (the top bucket holds
  others: the usual pick on a fleet with room to spare), the pick is what the top bucket gives
  with it counted in: the good place of the kind asked when that is there ("Good places",
  below), else the bucket's next in turn. That is the runner itself, and again nothing moves
  (when it is its kind's good place, with nothing looked up either: the move put off carries
  the kind); or another, which the run leaves where the first was, and the two change places in
  one write (`Index::swap`), each bucket's count and the bitmap of non-empty buckets untouched.
  Anything else (a run of another size, a runner in several indexes, orders kept) makes the two
  moves one after the other. A twin test holds the put-off form to every move made as it comes,
  pick by pick (W05-R04, W05-R26).
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
  two. A runner's fast fields and eight inline 16-byte admitted records occupy its first 160
  bytes; overflow records are 24 bytes. Memory, disk and GPU needs use a side slab when needed.
  An owner's whole run slot is 24 bytes, including its full generation. CPU and signature share
  one word with the runner handle: CPU/signature remain live through waiting and Go refusal;
  only Started replaces them with the admitted handle. Wider CPU demand stays exact in Slow
  until admission. A freed slot reuses its runner field for the free-list link (W05-R23).
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
- **Batches.** `ask_many` decides `n` runs of one need and one kind in the order one at a time
  would. For whole CPUs alone on runners whose keys are whole free CPUs, a member of bucket b
  goes to b - c, so the index alone decides the batch and the members' records are then updated
  in a pass of independent steps. After a batch of ends (`ended_batch`), its runs go first, among
  equals, to the runners that kind's work just went well on, in the order their ends came; every
  other rise is made first and takes its turn.

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

Beside its index each signature can keep an order by each rate a run's finish divides by: the key
multiplier, per-core speed over slowness (narrow asks), disk, this owner's link, the runner's
network (`lb::Order`). The first non-reference CPU measurement activates the CPU order once.
Each byte-rate order is populated from current membership and facts when an ask first needs it;
that first ask pays its construction cost. Subsequent facts, joins, departures and room changes
maintain active orders. An unused order has no heap storage, and CPU-only updates do not load the
separate byte-rate facts. No TTL, eviction threshold or guessed capacity controls these indexes
(W05-R26). Cold construction and first-use timings are reported separately from warmed throughput
by the benchmark's `--cold` mode. A member's class is its rate's place on a log scale: the octave (its top bit) and
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

## A machine's keeper

Where Hex's node keeps a machine's room (Hex's `docs/design/capacity-view.md`; kiln plan
`01a1cd8e862e7169a074db3d5fa33f3d`, step 13), Hex's boxes and kiln's runs admit against one ledger
there, the keeper's: kiln's runner is a holder, which reserves a run's room through the keeper and
starts the run only on the keeper's grant (CV1). Two admitters on one room each checking only their
own books oversubscribe it by construction ("multiple schedulers may attempt to claim the same
resource simultaneously": Omega, Schwarzkopf et al., EuroSys 2013, §3); one commit point that
validates is what keeps deciders working on copies safe (§3.4). The runner's ledger
(`place::runner::Admission`, `place::keeper`) takes where it stands with its keeper as an input,
`Admission::keeper_said`:

- **None known** (`Keeper::None`): it admits on its own measured room, as ever. A keeper is known
  only where the runner's configuration names one: off until declared.
- **Unsynced** (`Keeper::Unsynced`: joining, not served, superseded by a newer process of the
  runner, or closed for a word it couldn't take): nothing new is admitted there (CV11). Its kernel
  is cordoned as for a battery or strain (above), so its words say no room and owners' kernels pick
  elsewhere; its waiting runs are told `Unsynced`, why in words, which owners widen past as past a
  strained runner, so probing places the work elsewhere too; what runs goes on, held in the keeper's
  books. A run starts here only on its reserve's grant, and no reserve is said while unsynced, so a
  start under a claim offered before lapses and the run waits here again, saying why. A keeper that
  stops answering is never taken for none: that would double-book what it still holds (CV10).
- **Synced** (`Keeper::Synced`: the keeper's view as its last answer or hint said it, and what of it
  is the runner's own): the room it admits against, overcommitted and not, is held to what the view
  leaves it, the machine's room less what everyone else holds, the keeper's own rule (CV3: a charge
  fits iff `c <= room - local - foreign`). Its CPUs are never overcommitted past that room, which the
  keeper grants nothing past whatever `overcommit_cpu` says; a run asking GPUs never runs where a
  keeper is known, as a keeper grants no holder GPUs by count (CV3, until device grants by identity).
  The kernel's budget stays the runner's own (`cap`): what others hold is room the ledger doesn't
  hold, so it reaches the kernel as its `outside`, through `sync_fast`. A view over its room (the
  room measured below what is held) admits nothing new until it fits, and what runs keeps its room.
  It holds no more claims asking something than its holder may reserve (`View::most_claims`): the
  most its holder may hold, Hex's `most_held` (the wire's message bound over the widest
  reservation, so that one sync lists them all: CV8, CV9), less those it holds for no live claim
  here (an earlier process's runs that outlived it, releases not yet answered). A claim more fits
  nowhere, by probing or by its kernel, and its waiting runs say they wait for room. A run that asks
  nothing reserves nothing, as Hex's holder and keeper take no empty amount: it starts without a
  reservation and isn't counted, what it uses being in the keeper's measured room (CV12) and CPU
  shared. That is its cold start; once kiln has learned what it asks, it reserves as any run does
  (agreed with Hex's lead, 2026-11-13).

**A run's start.** The shell reserves a claim's room (`Admission::live_claim`: the room the claim
holds) before the ledger starts its run, and starts it only on the keeper's grant, as Hex's stand-in
holder writes a run down on its grant, before its work starts: `Admission::start` on the grant, its
record then written before the process starts, as ever. A reserve refused, or unanswered once its
wait ends (**Waits**, below: `NotGranted`), lapses the claim (`Admission::not_granted`): the run
waits here again on the room there is (the shell says the refusal's view first), and its owner is
told why, the one word on which it may place the run elsewhere. A kernel start refused is declined
for room (`Admission::fast_not_granted`), in the runner's word as it stands, so the owner's kernel
picks again. A grant that comes for a claim that lapsed meanwhile starts nothing, and is given back.

**Waits.** Every wait the holder makes on its keeper (a hello's, a sync's, a reserve's, a release's,
and so the sync again after a lost link) follows the keeper's liveness and progress alone
(`place::keeper::Waits`): no time is seeded from records and none is multiplied (no constants:
Shards' R03, as the user restated it on 2026-11-13; Hex's own holders seed theirs from its waits
census, CV6, which kiln's doesn't). Its words go on one link and are answered in the order said
(CV9), so the answer due is the oldest word's. A wait ends, typed (`WaitEnded`), when the link dies,
by the transport's own liveness rule (QUIC's idle timeout, three probe timeouts of the measured
round trip: RFC 9000 §10.1, RFC 9002 §6.2); when the keeper starts again or resets the link
(`Reset`: a stateless reset, RFC 9000 §10.3, or `Closed { Session }`); or when the keeper, its link
alive, has answered nothing for longer than the slowest answer it has given in this process
(`Stalled`), each answer timed from its word, or from the answer before it where that came later.
Until its first answer there is no such bound: a cold start waits on liveness alone, the runner
admitting nothing new there meanwhile (CV11). A reserve whose wait ended is not granted
(`NotGranted::Unanswered`); a stall leaves the runner `Unsynced` (`Stalled`), taking nothing new
there until the keeper answers again; and a late answer comes to its own word and makes the slowest
answer what it took.

The view is the decider's copy (CV7): the keeper validates every reserve, so a stale view costs a
refusal and a placement again, never a double booking. Its `own` is the reservations granted the
starts of the runs its ledger holds; one being released, and an earlier process's runs that outlived
it, count as another's until the keeper's word drops them, so the view never says more room than the
keeper has. A release goes on the runner's one ordered link before any later reserve, so the room of
a run that ended here is the keeper's again by the time the next run's reserve reaches it. No number
here is the ledger's own: each is the keeper's measured room and books, or the runner's.

**The holding** (`place::holder::Holding`). Hex's holder core, `hex_api::capacity::Holder`, keeps
the order of words, kiln driving it rather than a copy of it (Shards' R12: one implementation); the
holding drives it for the ledger with the waits above and the starts its reservations are for. Its
shell says when a link opens (`linked`: the hello) and ends (`ended`, died or reset), carries each
answer and hint (`heard`), wakes it at `due` (`woke`), and asks it to reserve a start's room
(`reserve`: none for a start that asks nothing, which starts at once) and to give back a
reservation whose work can no longer run (`release`). Each input says where the runner stands first
(`Event::Keeper`, for `Admission::keeper_said`: the refusal's view before the refusal), then each
start's reserve granted or not (`Event::Granted`, `Event::NotGranted`) and each sync's counts
(`Event::Synced`). It stands joining until its sync is answered, the core's link going down,
joining, syncing and up; a close it says hello again after (`Session`, `NotSynced`, a refused
`Scope`) it says hello at once, on the same link; `OlderEpoch` leaves it superseded for good, never
saying hello again; `HoldersFull` and `NotAuthorized` leave it not served, and `Invalid` malformed,
until a new link. The most it may hold is the most reservations one sync lists within the wire's
message bound (`hex_core::capacity::most_held`, CV8, CV9), less its survivors (the reservations its
records list from earlier processes, whose work still runs) and its releases not yet answered: the
ledger's `most_claims`. kiln-ci's simulator (`place::holder_tests`) drives one to three holdings
through Hex's own keeper over records in memory (`capacity::sim::SimView`), beside Hex's boxes,
under a nemesis that restarts the keeper, crashes runners (their work dying or outliving them),
cuts links with what was on them, drops sessions behind live links, delays words until the keeper
stalls and gives up starts: on each of 24 seeds the work running fits the machine and is in the
keeper's books after every step, and once healed and settled the keeper's books of each runner, its
own, its records and what runs are the same, no sync disagreeing.

Its words to owners from before it: `Unsynced` is appended to the waiting reasons under the link
feature `place.keeper` (docs/wire.md); to an owner whose link didn't name it a runner says `Strained`,
its why kept, or `Draining` where the owner doesn't read health's words either. The offers hint says
it as its `Strained` state, with its why, which every server reads.

## Good places

An assignment succeeds when the work lands on the node that lets it complete as quickly, as
efficiently, as completely and as correctly as it can. What the kernel holds down is the time
and work of deciding. One rule serves both: **work goes back to where its kind last went well,
when the rank can't tell that node from the best.** What that kind left there is warm and the
node has shown it can do the work; and when it is the runner a run just ended on, going back is
also the decision that takes least: nothing looked up, nothing moved. It is on by default, the
user's exception to opt-in automation (2026-11-11, docs/ci.md §0.3): `[reach] good_places =
false` turns it off, and a run it sends back says so.

- **Kinds.** The shell numbers kinds of work (`KindId`: work whose runs leave a node equipped
  for the next of them: the same job, the same tree), as it numbers signatures. `Owner::ask_kind`
  names one. `Owner::ask` names none and is decided as before, through entries compiled apart
  that carry none of a kind's checks: it takes what it took before there were kinds.
- **The shell judges, as a run ends and never while deciding.** It has what the four tests
  need (how long the run took against its history, what it used, whether it ran to its end, a
  cause it knows when it didn't); the kernel knows nothing of how runs end. Of an end it says one
  of three things, or nothing:
  - `Owner::won`: a successful assignment (`Owner::ended_won` hears the end and says it in one).
    A run that failed for its own sake, a test that fails anywhere, was assigned well.
  - `Owner::failed`: the node let the work down, by a cause the shell knows (lost, killed for
    memory, out of room, a tool missing).
  - `Owner::unsure`: it failed and no cause is known.
- **What the owner keeps** of a kind is twelve bytes: the runner its work last went well on (its
  good place), the runner that last let it down, and whether a doubt is open.
- **The pick** of a kind's run: its good place, when that runner is among the best by rank (in
  the top bucket, with the room; on measured runners, in the narrow order's highest class with a
  core free), and no turn moves for it; else the next in turn, passing over the runner that let
  it down when another is as free. The rank decides first. A good place that is less free isn't
  gone back to, nor one whose cores have measured slower since (heat, a power limit: its word
  says a lower speed, above), though its last run went well.
- **A failure no one can explain is settled by comparison,** with no rate, count or timer. The
  kind's next run goes elsewhere once. If it goes well there, the first runner was at fault and
  stays passed over. If it fails there too, the work was: nothing is held against either runner,
  the kind stays where it now is, and its further failures are its own until a run of it goes
  well. So work that fails for its own sake moves once and then keeps its place (and its warm
  state), and a node that fails work that passes elsewhere loses it at its first failure.
- **It moves no load.** The pick goes to one of the most free either way; only which of equals
  differs. With every run said to have gone well and with nothing said, a fleet is as full step
  by step (as many runners hold each number of runs). What differs is where: 64 runners of eight
  cores, one kind, a run ending and another asked at each step (`cargo test -p kiln-ci
  lb::tests::w05_r04_a_good_place_moves_no_load -- --nocapture`):

  | Runs in flight | Runners given runs, taking turns | Going back to where it went well |
  |---|---|---|
  | 1 | 64 | 1 |
  | 4 | 64 | 4 |
  | 16 | 64 | 16 |
  | 32 | 64 | 32 |
  | 64 and more | 64 | 64 |

  A kind's work stays on as many runners as it has runs in flight; taking turns takes it round
  every runner, each cold for it.
- **A runner that lets a kind down doesn't keep it.** One that went well and then goes bad
  loses the kind at its first let-down and isn't given it again while another is as free. One
  that never went well gets the kind's work only while the kind is looking for a place: once,
  when the shell can tell a node's fault from the work's own. When it can't, and says of a share
  q of the other runners' runs that they were let down, the kind stays 1 / q runs on each of them
  (the mean of a geometric count) and the failing runner gets one run a round: q / (q + n - 1) of
  the runs on n runners, against 1 / n taking turns. That is staying on a winner (Robbins, "Some
  aspects of the sequential design of experiments", Bull. AMS 58(5), 1952) as Zelen's play the
  winner rule counts it ("Play the winner rule and the controlled clinical trial", JASA 64(325),
  1969: for two, q_B / (q_A + q_B)); a test holds the share to it within 5% over 200,000 runs,
  for 2, 3, 10 and 64 runners. The comparison above is why the shell seldom has to say it of a
  healthy runner.

What the rule doesn't reach: an ask with terms (`ask_with`) ranks by its own finish, where the
shell's warm list already says what a runner saves; a shared owner's draw (`set_spread`) spreads
first; and a node that is the best by rank and fails every run is still picked when no other is
as free or as fast, since the rank knows speed and room, not failure.

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
  slowness); hints carry it too (`offers`' `fast`), and the server passes them on. The speed and
  slowness are the runner's pace as the shell's sense measured it (`Admission::paced`, and a
  sample's pace), said through its kernel's `Runner::measure`: owners' kernels fold them into its
  key multiplier (W05-R26).
- **Learned weights** (`[reach] learned_weights`, "Learned wait weights" above): a ticket probed
  afresh (`probe_new`) has its two runners drawn in proportion to the weights of the runners its
  kind of ask suits, and its run, when history knows its time, is Algorithm 1's job over them.
- **The auction** (`[reach] auction`, "The auction" above): `Owner::ask_many` takes asks that
  arrive in one step; `Owner::configure` reads both settings, and the reach `Placer` calls it as
  it's made.
- **The server** owns v1 jobs (`crates/kiln/src/hub/legs.rs`): an ordinary job's runner is the
  kernel's pick among the runners that named `place.fast` and hint their words, by the same rules
  the v1 dispatcher's pick filters by; the job's room is taken from the view until its runner is
  heard of (started, refused, released). What the kernel can't place, the v1 dispatcher's own pick
  does. A run's ordinary jobs waiting together in one look at the queue go to `Legs::pick_batch`
  first. Runners' words and the jobs heard of at them come by mail (`legs::Mail`: each runner's
  newest word, behind a lock held for a map insert), which the scheduling loop takes in as it looks,
  so the kernel's records are written by that loop alone. The CI engine's legs go through a kernel
  of the engine's own (below).
- **Room tokens** (`[reach] tokens`, off until declared; below): a runner that declares them tells
  each owner whose link named `place.tokens` what it holds there, its own word (`tokens` in its
  answers' words, and `FastHint` for what's owed besides), and that owner takes no hint of the
  runner's kernel epoch while it does. The server, the runner's own node and owners that didn't
  name it hear its hints, as before.

Measured (`cargo bench -p kiln-ci --bench place_fast`): see below.

## The CI engine

Every leg of a `.kiln` or GitHub workflow is placed by this kernel (`kiln_ci::sched::balance`;
docs/ci.md W05-R04, W05-R23): the engine keeps one, as the one owner of its legs, and its own
pick, the order it kept of its candidates (preference, headroom, then the least loaded and the
most free slots), is gone. The queue still decides which leg goes next (classes, fair share,
priorities, W05-R09 to R10, R30, R31); routing, which runners may take it (W05-R19 to R22); the
engine's rules, whether one takes it now (its room, a slot or its request within capacity ×
overcommit, W05-R03; its target use by its live sample; room kept for another, W05-R39; a runner a
retry stays off or is pinned to, W05-R16, R28); and the kernel, which of those it goes to. What a
winner is decides it: the runner where the leg completes as quickly, efficiently, completely and
correctly as it can.

- **What the kernel is told.** Each runner joins it at its first word, with its whole budget (its
  CPUs, memory, disk and GPUs as it offers them or its facts say, else its CPUs as its latest
  sample says, else one for each slot; CPUs overcommitted in resource mode), and its room for an
  ordinary leg now, whenever that changes (a leg granted or ended there, an executor started or
  stopped, a sample, the runner up or changed): the least of what each of the engine's measures
  leaves, its free slots' share of the CPUs no exclusive leg holds (slot mode) or what its capacity
  leaves (resource mode), and, by its live sample, what it can still take within its target use,
  less its CPUs' share of the time its tasks all stalled on memory and on I/O (PSI's `full`: time
  lost, not used). None while a leg holds it whole. Its pace is what its own kernel's words say
  (the fleet's fitted per-core speed, its slowness under contention now, its disk's and network's
  rates): the server passes it on when it changes (`Input::RunnerPace`; one said before the runner
  is up is kept for it). A signature's members are its eligible runners that aren't draining,
  synced when which runners those are may have changed. A runner leaves the kernel with its link,
  and joins it again, under its next epoch, when it's back.
- **Each leg by when it will finish there.** A leg comes with what its kind's runs taught
  (`Profile`): its CPU time on the reference core (the fleet's speed fit, `pace::speed`, every run
  of it kept, each machine's speed taken out: `Fleet::work`), the most cores a run of it kept busy
  (CPU time over wall time, the most of its newest runs: a lower bound of what it can use, that only
  rises as runs find more cores free), and its bytes. The kernel ranks each runner by when the leg
  is expected to finish there ("Per-ask terms", `Owner::ask_with_kind`): its CPU time over the
  cores it would get, no more than it can use, at that runner's speed under contention, and its
  bytes at the runner's disk rate. So a leg that keeps one core busy goes to the fastest cores,
  and a build that uses every core it finds to the most of them at their speed. A kind no run has
  taught is expected to be like the kinds so far, the median of their profiles, each term apart
  (an empirical prior: Robbins, 1956); before any kind has taught one, a leg ranks runners by
  their free cores at their speed (`Owner::ask_kind_wide`). Legs rebuilt after a restart take the
  prior until their kind's next leg is queued.
- **The pick.** The kernel proposes the signature's members in its order: the soonest finish
  first, and between runners it can't tell apart (within 1/64, the orders' resolution), the leg's
  kind's good place, else the next in turn, passing over the runner that last let its kind down
  when another is that close ("Good places"). The first that takes the leg now by the engine's
  rules gets it; one that doesn't is set aside for that pick alone. The pick takes its turn; nothing
  of the kernel's view is left changed by it, since the engine tells the kernel the runner's room
  once the leg is granted.
- **Where it's asked.** A runner with room takes its best legs in queue order (W05-R04), and each
  goes where the kernel puts it: that runner, or one where it finishes as soon or sooner, or its
  kind's good place among equals. A leg queued is offered to the runners with room for it in the
  kernel's rank (the most whole cores free first, by id among equals), each taking its best legs,
  as before. A runner's lost legs, coming back together, each go straight where the kernel puts it.
- **A run's critical path first.** What people wait on is the run. Legs that become ready
  together go to the scheduler in order of their jobs' upward ranks (HEFT's `rank_u`: Topcuoglu,
  Hariri & Wu, IEEE TPDS 13(3), 2002): a job's median duration and the longest chain of medians
  among the jobs that need it, from the workflow's `needs` and its jobs' history
  (`ci::dispatch::critical_first`). The leg the run waits on longest takes the best runner first;
  in the queue, legs queued together keep that order among those of one priority.
- **Preferences and GPUs.** A leg whose job or a rule declares preferences (W05-R23's terms over
  facts and labels, W24-R18's `warm` and `affinity`), or that asks for GPUs (W05-R22's device fit),
  goes to the runners those rank first, of those that take it now, and between them where the
  kernel puts it: where they rank none above another (a cold fleet), exactly as a leg without.
- **Kinds and ends.** A leg's kind is the key its runner keys its runs of it by
  (`sense::run::leg_key`: its repository, its job's workflow, its job and its matrix), so the
  engine, the runner and the run-time model name the same work alike. As an attempt ends on its
  runner, the kernel is told how it went: success, it went well (its kind's good place from now);
  lost with its runner, its infrastructure, its image, killed for memory or its disk full, the
  runner let it down; failed in its steps or timed out, no one knows whose fault it was, which the
  kernel settles by comparison; cancelled, nothing.
- **Quickly.** A winner completes quickly as well as correctly. A leg that succeeded but took
  longer from its grant than every earlier run of its kind that went well on that runner is a
  record: something there may have slowed it (a throttled or contended machine, a failing disk) or
  its work grew, and no one can say which, so it's judged as a failure no one can explain, and the
  comparison settles it: its kind's next leg goes elsewhere once; going well there, the runner was
  at fault and is passed over; a record there too, the work grew, and it keeps its place. No
  threshold is picked: of n exchangeable runs the last is the longest with probability 1/n
  (Foster & Stuart, JRSS B 16(1), 1954), so a runner that hasn't slowed sees about ln N such moves
  over N runs of a kind there (H_N - 1: Rényi, 1962; Glick, Amer. Math. Monthly 85(1), 1978),
  fewer the longer the kind has run there. `[reach] good_places = false` turns kinds off for CI
  legs too: equals take turns alone, and no end is judged.
- **Pools.** A pool's leg (W13-R09) runs in an idle executor of its pool: of the hosts with one,
  where the kernel puts it, as any leg (its GPUs' fit first when it asks for GPUs). Where a pool
  starts its next executor is the kernel's pick as well (`host_for`): of the hosts that may run
  them, the most free by the rank, and between hosts as free, back to the one where the pool's
  executors last started well (an executor ready is its good place's win), past one where a start
  last failed; one at its `per_host_max`, backing off, without the GPUs or the room beside its
  exclusive legs is set aside for that pick.
- **Gangs.** A gang's members (W05-R40) start together or not at all, placed by the engine's
  all-or-nothing planner, which tries the runners for each member in the kernel's rank (the most
  whole cores free with the members put there before it, then by id).

The reference scheduler (`sched::reference`, docs/needs.md E1.5) holds the engine to this: a leg
goes to a runner with the most whole cores free of those that take it now, and of runners as free,
the one the kernel took (its turn and good places are the kernel's own tests'); its replay digest
(`sched::replay`) moved with it.
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
restricted assignment (a run may go only to its kind's members: a signature's, a suit's live
runners), one weight per machine, each job split over its eligible machines in proportion to their
weights (their (3.1)). Predicted weights within a factor η of good ones give an O(min{log η, log
m})-competitive fractional makespan with their Algorithm 1 (Theorem 3.1). `src/lb/weights.rs`
follows it step for step. The kernel turns it on with `Owner::set_weights`, the place core with
`[reach] learned_weights` (docs/needs.md §21): off until declared, as before them otherwise.

- **Weights.** A runner's weight starts at its CPU budget (in proportion to capacity: what
  predicts with no history) or at what the shell predicts (`Owner::set_weight`). A runner of C
  CPUs is C unit machines weighted alike, Algorithm 1's identical machines: its load counts per
  CPU, its units pass a threshold together, and halving the runner's weight halves theirs.
- **The draw.** A run that finds no room waits (the kernel) or is probed (the place core,
  `probe_new`) at places drawn in proportion to the weights of its kind's members, two without
  replacement: the fractional assignment rounded by sampling, late-bound as before (the first
  place with room starts it), as Algorithm 1's weights bias where runs wait.
- **Algorithm 1's step.** A run's first wait (the kernel) or first probes (the place core) is a
  job. Its size p_j (the kernel's: its CPU need; the place core's: its CPUs times its median from
  history, no job when history knows no time) adds p_j x_ij(ŵ) to the phase load of every member
  i, and each member whose phase load passed 2T has its weight halved and starts a new phase: if
  1/2 ≤ ŵ_i/w_i ≤ 1 for every i then x_ij(ŵ) ≤ 2 x_ij(w), so each halving closes a factor two of
  the error. Every member, as the algorithm's loop does: O(members) a job.
- **T.** Algorithm 1 is given the optimal fractional makespan. Their §2 and Lemma D.1 remove that
  by a standard online doubling argument, the one of Aspnes, Azar, Fiat, Plotkin and Waarts
  ("On-line routing of virtual circuits with applications to load balancing and machine
  scheduling", J. ACM 44(3), 1997, §2.1 and §3.2): T starts at what the first job shows of the
  optimum; an epoch (their phase) runs the algorithm on its own jobs, ignoring those before it;
  once its jobs show the optimum is past T, T doubles and a new epoch begins with the job that
  showed it, every load at 0, nothing fallen back, the weights as learned (Theorem 3.1 holds for
  any predictions, η theirs). Each doubling is shown needed, so T stays below twice the optimum,
  and the epochs together cost at most four times what the algorithm guarantees given the
  optimum. What jobs show of the optimum: each kind's sizes over its members' CPUs (every job of a
  kind runs on its members, so one of them carries at least that) and the fleet's over its CPUs.
- **The switch.** Once a job makes a member's load in the epoch, all its phases, pass 2⌈log2 m⌉T,
  the predictions aren't helping (their switch to a robust algorithm, after Theorem 3.1): the owner
  draws as it did before the weights until T next doubles, and says which runner and loads
  (`fell_back`). The kernel's draw is then the two members nearest to fitting (drawn uniformly
  under tokens), the place core's two at random, and its answers say "not probed (two drawn at
  random: the learned weights weren't helping)". m counts runners, the machines the draw chooses
  among.

What it does: a runner's weight halves once its load in its phase passes 2T, and an epoch begins
every phase afresh. An epoch's work of a kind is at most T times its members' CPUs (more would show
the optimum past T), so a runner halves when it takes more than twice its share of that capacity,
and each epoch holds about twice the work of the one before. It bounds the makespan; it doesn't
balance below that, and it learns from runs' sizes and where they may run, not from how long they
waited (the paper's model).

Where kiln departs, each for a reason: the rounding is two places drawn and late-bound, not their
§5 rounding to one machine a job, since a run here waits at two places and starts at the first
with room (above); what learns, the fractional algorithm, is theirs exactly. The robust draw is the
one each core made before the weights, not their [3, 4]'s O(log m)-competitive algorithms; with
tokens off the kernel's, the members nearest to fitting, is greedy's least-loaded choice (Azar, Naor
and Rom, J. Algorithms 18(2), 1995, their [4]). What the jobs show is a lower bound on the optimum,
so an epoch whose T is below the optimum without its jobs showing it can switch early: back to the
draw before the weights, the safe side.

The rule before this one held T to the fleet's balanced load and each phase to the T gained in it.
A runner that alone runs a kind carries that kind's whole work, which no assignment avoids; under
that T it looked overloaded, its weight collapsed and the owner fell back though nothing did better
(`KindBlind` plants it, and the test of that case catches it).

## The auction

`Owner::ask_batch` places runs that arrive in one step together, each with its own cost at each
member (its expected finish) over the slots members' views hold now. `Owner::set_auction` decides
them by an integer auction (off: one at a time, each to its cheapest member, the rule it is
measured against). Bertsekas, "Auction Algorithms for Network Flow Problems: A Tutorial
Introduction", Computational Optimization and Applications 1:7-66, 1992 (`src/lb/auction.rs`):

- The forward auction, prices from zero (§9: more slots than runs).
- Costs scaled by min(n, m) + 1 and ε = 1, so ε < 1/n (Prop. 1 and 2) and ε < 1/m for m runners
  (§5.1, similar objects): the assignment is optimal, ε derived, not chosen.
- A runner's slots are one similarity class (§5.1): a bid's increment is against the second-best
  runner, so runs don't bid identical slots up one ε at a time.
- Feasibility first, by augmenting paths, so the auction ends.

**Costs.** A run's expected finish at a runner (`auction::finish_at`): its time at the reference
core over the runner's key multiplier as the kernel keeps it (`Owner::multiplier`: its measured
per-core speed over its slowness under contention, folded in as its word says them), at the
kernel's resolution, the least rate of the multiplier's class (rates within 1/64 are one class,
and the orders don't rank within one), plus its inputs' transfer where it's given. Runners'
measures reach owners' kernels in their own words: the place core's admission says its pace
(`Admission::paced`, what the shell's sense measured, W05-R26) in its kernel's word, so on a
measured fleet the multipliers differ and the auction has something to decide by.

**When it decides.** Where the runs' times and the members' rates (their classes) both differ.
Otherwise every assignment of them costs the same, and the kernel's own pick, one at a time, is as
cheap and spreads them by room (W05-R23), where the auction's slots, alike within a runner, would
stack them on one. What the views don't hold goes one at a time: probed in the place core, the
engine's pick at the server.

**Where batches arrive.**

- The place core (`place::Owner::ask_many`, `[reach] auction`): the asks a tenant's budget lets
  through together (the reach `Placer`'s budget releases). The ordinary asks of a kind the kernel
  starts go together; every other ask goes as one at a time would. A run it placed says so ("the
  auction put it there with 2 others asked with it, where their expected finishes total least").
- The server's kernel for CI legs (`hub/legs.rs`, `Legs::pick_batch`, the same setting): a run's
  ordinary legs of one kind and need that wait in one look at the queue; a pick the job can't take
  (refused there, no slot by the engine's rule) goes back, and the engine's pick decides. v1 legs
  carry no time from history (none is kept by job), so their costs can't tell them apart and they
  are picked one at a time, as before; the auction acts there once a leg's median is passed.

**Batches, not gangs.** A gang (`together:`) starts whole or not at all. The place core has
answered every gang never since it was built, as reach's directory pick refused one before it
("commands can't start together yet": commands can't be one of a gang until the engine places
them, W05-R40, docs/needs.md E4 and E5), and CI's gangs are the engine's, granted whole in one
input (docs/needs.md E4). So the auction's integration point is the batch: runs that arrive in one
step, each started on its own. `Owner::place_jointly` decides a gang's places all or none for a
gang protocol to come; holding every member's room and starting them together is that
protocol's.

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
| A wait with learned weights on (the kernel's first wait; the place core's first probes) | the draw O(members); Algorithm 1's step O(members), as its loop tests every eligible machine; the fleet's CPUs O(runners) in the kernel; a doubling O(runners) |
| A batch by the auction (n runs, m members with room) | feasibility O(n × edges) by augmenting paths, then the auction's bids |

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
| Weights' 2⌈log2 m⌉T | Lattanzi et al. 2020, the switch to a robust algorithm (after Theorem 3.1) |
| T's first value, and doubling it | Aspnes et al. 1997 §2.1 and §3.2, the doubling Lattanzi et al.'s Lemma D.1 takes: the first job's bound on the optimum, doubled when an epoch's jobs show the optimum past it |
| A weight's start, its CPU budget | allocation in proportion to capacity, what predicts with no history (good weights are scale-invariant: Remark 3.1) |
| The auction's ε = 1 on costs × (min(n, m) + 1) | Bertsekas 1992, Prop. 1 and 2, §5.1 |
| The auction's rate, its class's least | the orders' resolution (64 classes an octave): the kernel doesn't rank within a class |
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
their links; its asks are of three kinds of work and of none, and of each end an owner hears its
shell says that it went well, that its runner let it down, that no one can say, or nothing.

Good places (`lb::tests`, each with its planted bug): a kind's work goes back to where it went
well, another kind's and work of no kind don't take that place (NoStay, NoTurn); a runner that
lets a kind down doesn't keep it, and a failure no one can explain is settled by comparison
(StaysOnALoser: what the shell says against a runner is dropped); a good place holds only among
cores as fast; it moves no load. The twin test runs two owners over twin fleets through the same
ends, the same words of their shell and the same picks, of three kinds and of none, one putting
moves off and one making every move as it comes, and holds them to the same pick, the same
picks to come and the same views at each of 32,000 steps on fleets of 2 to 257 runners.

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
token and tells no one). `lb::scale_tests` pins each rule of tokens, the weights' draw and the
auction on a small case, each with its planted mistake caught: FreedToFirst, NeverWants and
ForwardAgain, HintsHold and FirstHearsHints (a mixed fleet), CordonQuiet, NearestPlaces; Unweighted
(places in proportion to the weights); EpsilonTooLarge (against the optimum found by trying every
assignment, 400 cases) and
SecondInClass (a price war).

Learned weights (`src/lb/weights_tests.rs`): Algorithm 1 written as the paper states it, with
Aspnes et al.'s doubling for T and the switch, in floats, beside the kernel's integers on 400
random instances (two to nine runners of a quarter to 16 CPUs, one to four kinds of random
members, predictions up to 4,096 times a runner's CPUs, up to 80 jobs of one to 16 CPUs): every
weight, the switch and T's doublings agree after every job. One instance stopped where a
comparison came within 10^-5 of its threshold; of the rest, 271 halved a weight, 392 doubled T
and 85 fell back. Each rule's planted mistake is caught there: NoLearn, NoFallback, DrawnOnly
(only the places drawn tested), KindBlind (T blind to kinds: the earlier rule), NoDoubling and
NoRestart (T doubled without beginning an epoch). Cases small enough to follow: a runner that
alone runs a kind keeps its weight and the predictions (KindBlind); a weight predicted four times
its CPU halves once and holds (NoLearn); predictions 2^20 off fall back with the first job and
say why (NoFallback); once fallen back, the places are the nearest to fitting again.

In the place core (`place::weights_tests`, `place::auction_tests`), through its owner and, where
words matter, its runners' admissions paced as the shell paces them: probes in proportion to the
weights, a runner of 3 CPUs among three of 2 one of the two 13/21 of the time (UnweightedProbes);
the runs probed are Algorithm 1's jobs, the owner's weights and T equal to the kernel's learner
fed the same jobs by hand (NoProbeLearn, SizeByCpu); predictions that don't help fall back to
random probes and say so (KeepWeights); the auction off until declared; asks in one step where
it puts them (AuctionOneByOne); its costs read slowness (SpeedAloneCost) and a runner's measured
pace through its own kernel's word (UnpacedKernel); where costs tie, the kernel's own pick
(AuctionTies); what it can't start probed (DropUnplaced); gangs answered never; and, with either
on or off, every run started once.
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
N = 10,000 those structures (about 10 MB) are in the M3 Ultra's L2 (16 MB for each cluster of six
cores); on a core with 1 MB of L2 they're in L3 already (below, "At scale").

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

## At scale

Where a step leaves each level of cache, whether owners on several cores slow each other, and what
a lock other cores write through costs (Ada's questions, 2026-11-12). `--profile` runs (one
stretch of 2^23 steps after the fill and 25 N warm-up steps), ns a step with the harness's own
bookkeeping in it (null); `--impl owner` is the owner's side alone, what a node that places holds
(the whole bench also keeps every runner's ledger, which lives on the runner). Commit `1a5a8da8b`
(main `efb40ea0f` and the bench's two modes below); `~/.cache/kiln-lb/sync-scale` runs it.

**One owner, 10,000 to 1,000,000 runners.** adacachy2 (AMD Ryzen 9 9950X3D2: 48 KB L1d and 1 MB L2
a core, 96 MB L3 for eight; load 4 to 7) and adas-mac-studio.local-2 (Apple M3 Ultra: 16 MB L2 for
each cluster of six performance cores, no L3 but the system cache; load about 8, a CI job beside):

| N | rho | x86 owner | x86 whole | M3 owner | M3 whole |
|---|---|---|---|---|---|
| 10,000 | 0.5 | 19.2 | 25.3 | 14.8 | 27.1 |
| 30,000 | 0.5 | 22.7 | 29.6 | 19.8 | 49.8 |
| 100,000 | 0.5 | 23.8 | 33.6 | 46.3 | 74.6 |
| 300,000 | 0.5 | 31.7 | 105.3 | 84.0 | 125.2 |
| 1,000,000 | 0.5 | 112.9 | 171.0 | 125.0 | 156.3 |
| 10,000 | 0.9 | 25.4 | 32.6 | 22.2 | 33.7 |
| 100,000 | 0.9 | 29.3 | 47.2 | 64.7 | 113.5 |
| 300,000 | 0.9 | 88.4 | 145.4 | 113.0 | 156.3 |
| 1,000,000 | 0.9 | 132.5 | 210.2 | 134.5 | 172.3 |

On a core with 1 MB of L2 a step left it below 10,000 runners (the 10k audit: 15 of its 25 ns
wait on L3); the 96 MB L3 then holds the owner's side nearly flat to 300,000 runners at half load
(to 100,000 at 90%), the whole bench to 100,000; past that each miss is memory's, and the owner's
step costs four to five times what it did on the plateau. On the M3 Ultra the owner's side leaves the 16 MB L2 between 30,000 and 100,000 runners,
the whole bench between 10,000 and 30,000. At a million runners a decision takes 113 to 135 ns of
the owner's on either machine.

**Owners on several cores** (`--owners K`): K owners at once, one a thread, each with its own
fleet and requests; every owner's steps a second together, over K times one owner's alone:

| N | rho | K = 2 | 4 | 8 | 16 | machine |
|---|---|---|---|---|---|---|
| 10,000 | 0.5 | 0.98 | 0.98 | 0.94 | 0.87 | x86 |
| 10,000 | 0.9 | 1.00 | 0.96 | 0.93 | 0.90 | x86 |
| 100,000 | 0.5 | 0.64 | 0.29 | 0.26 | 0.14 | x86 |
| 100,000 | 0.9 | 0.43 | 0.31 | 0.22 | 0.15 | x86 |
| 10,000 | 0.5 | 1.01 | 0.92 | 0.82 | 0.75 | M3 |
| 100,000 | 0.5 | 0.80 | 0.73 | 0.61 | 0.51 | M3 |

Owners share no memory, and two owners on two cores lose nothing at 10,000 runners: no line goes
back and forth between them. What they share is the machine's: at 100,000 runners each owner of the
whole bench holds about 100 MB (ten times the 10 MB at 10,000 above), so two of them no longer fit the 96 MB L3 they share, and from four
on they're held to memory's bandwidth (x86: 35 to 66 million steps a second together). The
M3 Ultra's memory carries several times more, and its owners keep half their
pace at 16. Most of the whole bench's footprint is the runners' ledgers (640 bytes each, the 10k
audit's sizes), which a node that places doesn't hold.

**One owner behind a lock** (`--hinters M`, the server's v1 kernel's shape, `crates/kiln/src/hub/legs.rs`):
its steps a lock each while M threads hint it as fast as they can, against the same owner without
the lock:

| M | x86, rho 0.5 | x86, rho 0.9 | M3, rho 0.5 | M3, rho 0.9 |
|---|---|---|---|---|
| 0 (the lock alone) | 1.09x | 1.08x | 1.38x | 1.43x |
| 1 | 2.7x | 2.2x | 5.2x | 5.1x |
| 2 | 4.5x | 3.6x | 25x | 27x |
| 4 | 13x | 11x | 49x | 41x |
| 8 | 41x | 35x | 58x | 41x |

Hints came at 3 to 17 million a second, as fast as the threads could; a fleet's runners say
their room when it changes, a dozen runners a few times a second each. Against a mailbox the
owner drains itself (`--mailbox M`: each runner's newest word put in a set behind a lock of its
own, held for the insert, and a flag raised; before each step the owner reads the flag and takes
the set when it's up, so the owner's records have one writer), run that way the mailbox did
worse: its owner applied every word its posters made, up to 1,000 a step, where the lock let
fewer in. At the same words a second (`--rate`, spread over the threads and paced by the clock;
commit `20318e8b3`), a step's cost over the bare kernel's, for 1 to 8 threads at 10,000 runners,
rho 0.5 and 0.9, three runs on each machine:

| words a second | x86, lock | x86, mailbox | M3, lock | M3, mailbox |
|---|---|---|---|---|
| 10,000 | 1.00-1.42x | 1.02-1.33x | 2.35-5.7x | 1.04-1.30x |
| 100,000 | 1.03-1.28x | 1.03-1.34x | 2.6-8.8x | 1.06-1.39x |
| 1,000,000 | 1.29-2.40x | 1.11-1.43x | 2.5-101x | 1.18-21x |
| 10,000,000 | 1.99-44x | 4.9-47x | 2.5-72x | 17-457x |

With no thread at all the owners cost 1.0x to 1.2x on x86 (a step taken one at a time, the lock's
with a lock each) and 1.0x to 2.1x on the M3 Ultra. On x86 the two designs cost the same to
100,000 words a second and the mailbox less at a million. On the M3 Ultra the lock costs 2.4x and
more from 10,000 a second (why isn't measured here; a contended lock there puts its waiter to sleep
in the kernel), and its hinters can't get their words in: at a million a second one hinter got 2%
to 6% of them in. The mailbox there stays within 1.4x to 100,000 words a second; at a million,
eight posters contending for its own lock cost its owner 2.4x to 21x. At ten million a second (a
thousand a runner a second, no fleet's) both fail, the lock's hinters shut out and the mailbox's
owner applying every word that came; unpaced, the M3's mailbox with one poster ran for hours
without finishing its steps (x86: under a minute). The server's v1 kernel takes its words by mail
(Adopted, above), its scheduling loop the one writer, and a look takes at most one word a runner
however many came.

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
- Learned weights and the auction are off until declared, and whether either beats what's there
  (two choices; one at a time) is for comparisons at scale, not these tests. Learning with many
  owners at once is unproven: each owner learns from its own runs alone.
- The server's auction has no times to read for v1 legs (no history of v1 jobs is kept by job), so
  their batches are picked one at a time; a leg's median passed to `Legs::pick_batch` lets it act.
- What the jobs show of the optimum is each kind's bound and the fleet's, not the optimum: kinds
  nested in one another (a kind's members inside another's) can leave it below, and an epoch
  switch early. The optimum itself is a max-flow over kinds and runners.
- Room tokens through the place core change what owners hear, not yet how many words: the server
  still passes each runner's word on to every owner (owners that hear hints, and its own kernel,
  need it), and an owner that hears tokens drops it. Cutting that fan-out needs the server to know
  which owners hear tokens from which runners.
- A machine's keeper ("A machine's keeper"): built are the ledger's side and the holding (where it
  stands with the keeper, the room and claims the view leaves it, a start's reserve not granted, a
  run that asks nothing, the holder's waits, Hex's holder core driven, kiln-ci's simulator against
  Hex's `SimView`), kiln-seams' `Capacity` seam (its simulator Hex's own keeper, CAPACITY-1 to 7,
  and Hex's keeper suite on it), the holder's identity (`crate::capacity`: kiln's CA as Hex's
  `enroll::Ca`, passing HEX-ENROLL-1 to 5; `POST /api/agents/capacity`, the machine's proof naming
  the holder), and its link (`capacity::Quic`: `TLS13_AES_256_GCM_SHA384` asserted, framed by
  `hex_core::capacity`, passing CAPACITY-1 to 7 against Hex's keeper over loopback QUIC). Not yet:
  the runner's node driving the holding over that link, so nothing names a keeper yet: `[capacity]`
  naming the keeper (its address and its certificate's name), the holding's epoch the runner's
  durable one and its survivors from its records (each run's reservation kept with its record
  before its process starts), every start path (a placed run's spec, a kernel start, local runs, CI
  legs, sessions, hand-offs) waiting on its reserve's grant, and each run's reservation released as
  it ends.

## GPU fit and load (W05-R03, W05-R22, W05-R23, W13-R01)

GPU asks use Hex's per-device fit and dominant-load rule, ported from the fixed source
`37021c35e64b2a8bab1d35f20f92a0042d874a44`: `hex-sim/src/model.rs`'s `matches`/`bind`
and `hex-sim/src/index.rs`'s `gpu_busy`/`gpu_idle`. The kiln adaptation is
`sched/gpu.rs`, used by CI's scheduler, the place owner's kernel start and offer comparison,
and the receiving runner's admission, plus directory ranking and the pre-placement fallback. The same facts serve commands, MCP runs and sessions.

For `gpus=2,gpu.model==A100,gpu.memory.free>=16Gi`, two cards must each meet both predicates.
A model on one card and enough memory on a different card do not suffice. Boolean constraints
retain their meaning and host facts (OS, labels, GPU count) retain their host scope. `gpu.memory` and `gpu.memory.free` constrain each card's free memory for GPU reservations,
as Hex does; `gpu.memory.total` constrains its hardware total. Without `gpus=N`, the existing
`gpu.memory` fact still describes the largest hardware total. GPU model
names also accept Hex's whitespace-joined spelling, e.g. `NVIDIA-A100`.

Hex's `bind` first chooses compatible cards by utilization, then used memory, then inventory
position. It then scores those selected cards by the sum of their idle dominant shares: the
greater of utilization and used-memory share, in millionths (DRF, Ghodsi et al., NSDI 2011).
Sorting the cards directly by dominant pressure would be a different policy. GPU rank precedes
CPU headroom; explicit CI preferences still apply first. Counters stay independently optional
in the facts. Hex's later quality fix (`b5b67b383e3cb2746efa330687fdd911275b5d6c`) is retained:
a card with a missing memory or compute reading, or an invalid utilization, cannot satisfy a
fresh GPU reservation and earns no idle credit. Known zero remains idle; unread free memory
is not fabricated. A later valid observation can make a queued ask fit, and CPU-only work still
proceeds. Count-only peers without a device inventory retain their declared-count compatibility, but earn
no measured idle credit and cannot satisfy per-card predicates or supply concrete binding IDs.
There is no separate kiln GPU prior. All four reservation dimensions
are still checked and deducted by the existing ledger. Equal GPU ranks first compare measured per-core speed under contention, then retain CPU room
and the bucket cursor. Only GPU asks pay for the full bounded candidate walk. GPU bids in an
optional auction take this same individual path.

Reservations remain node-level GPU counts. In a mixed-device node, availability is conservative:
already-held count is removed from the compatible pool, since no physical assignment is known.
This may wait despite spare cards. Kiln does not set CUDA visibility, create CDI attachments,
pass through VFIO devices or recover per-device runtime assignments. Hex's generic live Runtime
path still refused device-bearing asks at the source pin; its ongoing device recovery/attachment
work is not imported here.

For a local executor pool, declare its GPU count in the pool's resources. Each warm executor
reserves that count when it starts, in slot mode as well as resource mode. A GPU leg must fit
the selected executor's reservation; its original GPU request and per-card predicates are
retained for that check. The grant credits its own executor's reservation once, leaving the
other warm executors' GPUs held. Ordinary gang siblings share the remaining host capacity.
A hosted pool's pod instead needs its own runner to report enough GPU capacity before a grant.
These admission checks do not establish physical attachment or device isolation.

`crates/kiln-gpu` is Hex's retained provider, updated at fixed source
`c2da38cc363e7d53f6df8fb82ab7bbda5e2f1efe`; `ORIGIN.md` and `HEX-SOURCE.json` retain provenance.
It opens NVIDIA NVML, Linux AMD amdgpu counters and Windows AMD ADL once, then samples through
retained handles, file descriptors and contexts. Apple Silicon uses native Metal identity and
working-set budgets, exact-entry IOKit mapped allocations and IOReport GPU residency. The full
Metal registry ID survives in the optional facts ID; it is scoped to the host boot. Apple's memory
budget is not dedicated VRAM or host RAM, and observed allocations may exceed it. Missing or
ambiguous counters remain unknown. Sampling starts no subprocess or periodic worker. Linux/Windows
samples retain their allocation-free contract; Apple's native snapshots may allocate.
No installed driver is redistributed or replaced.

The same vendored libraries and recorded changes are built from kiln's tree:

- NVIDIA `cuda-rust` `a0a4dc78a20e06ec90827f597e950bdc49f723e2`: adapted runtime loader.
- `all-smi` `fe479ae675e39475a2eab1cfbb3f5432f705e12b`: adapted amdgpu counter reader and AMD ADL types/sensors.

Their licenses, `UPSTREAM.md` files and patches travel with them. `scripts/vendor.py check
cuda-rust all-smi` verifies both. Native FFI remains in the retained NVML/ADL `sys.rs` modules and the ported
Apple `sys.rs`; scheduling contains none. Hex's original rationale and primary sources are retained in
[gpu-telemetry-origin.md](gpu-telemetry-origin.md).

One retained facts value and one coalescing publisher handle full inventory probes and GPU-only
refreshes. GPU hosts refresh on workload start/end through the existing recorder, without a
periodic timer. Local placement receives facts directly; remote owners receive the existing
optional JSON facts fields through the directory. Host CPU/memory samples do not erase GPU
inventory. A full probe requested during GPU I/O is retained for the same publisher's next read.
On known GPU hosts, external load changes are observed at the next facts/workload event; a newly installed GPU or driver is discovered by the next full inventory probe.

Two other Hex fixes are ported: interleaved bucket offsets and the cursor's rounded fleet width
use widened arithmetic; reservation-size parsing uses checked integer bytes, refusing overflow
and fractional bytes instead of rounding through `f64`. Sources are Hex's `lb/index.rs` and
`needs.rs` at the pin above. Packing and box-runtime recovery changes are specific to Hex's
long-lived boxes; kiln retains its expected-finish placement and existing session recovery.

The follow-up is explicitly gated on Hex's completed **durable GPU bindings, actual runtime
attachment and validation of that combined path** (Ada, 2026-11-10). Bring over the accepted
pinned changes and their recovery/attachment evidence together, including the load-swap/reboot
case, exact selected-identity persistence before launch, lost/duplicate attachment controls and
native platform validation. The current port carries neither an independent attachment design
nor a claim that this unfinished path is proven. Hex has been asked to provide the ready SHA and
combined validation; until then the existing count ledger remains authoritative and conservative.
