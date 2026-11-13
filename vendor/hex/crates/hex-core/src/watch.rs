//! The rule a macOS `process` box is bounded by (docs/design/runtime.md, decision "each OS runs its own
//! boxes natively"). macOS has no public cap on a process tree's memory or its process count
//! (docs/research/runtime/native-platforms.md §3), so the node's watcher reads the box's members and ends
//! the box once it passes a bound: an eviction by a stated rule (CLAUDE.md §2), where Linux's cgroup
//! refuses the allocation or the fork itself. A box can pass its bound by what it grows between two
//! reads, so each wait is the time its headroom, plus the [`OVERSHOOT`] it may pass its bound by, lasts at
//! the fastest the box can grow ([`Rates`], seeded from its machine class's record, [`seed`], and learned), at least the [`FLOOR`]. A box with no
//! bound to pace by waits for its next change (a fork, an end, a start, a delete). Of a box whose growth could
//! pass its bar before the next read, given how late the node's watchers act ([`Lateness`]), only the members
//! whose own rates fit its headroom run across the wait, the rest held stopped ([`Watch::runs`]). Pure
//! (CLAUDE.md §9): the shell (`hex_runtime::macos`) lists the members, reads their footprints, waits, stops and
//! kills; this says when to read next, whether to end the box, and which members run.

use std::collections::TryReserveError;
use std::num::NonZeroU64;
use std::time::Duration;

/// Shortest wait between two reads of a box. The bound held at it on adas-mac-studio-3: a box kept
/// 256 MiB to within 42 MiB against its members allocating up to 164 GiB/s
/// (bench/native/macos/RESULT.md, commit 9694770).
pub const FLOOR: Duration = Duration::from_micros(250);

/// How far a box may pass its memory bound before the watcher's next read ends it: step 1's bar, a box held
/// to within 64 MiB of its bound (native plan step 1; bench/native/macos/RESULT.md). Each wait is the time
/// the box's headroom plus this lasts at its rate, so no box passes its bound by more between two reads
/// (the lead's call, 2026-11-06).
pub const OVERSHOOT: u64 = 64 << 20;

/// Each machine class's fastest recorded box growth, by its `hw.model`: the worst aggregate a box reached
/// with one, two and four members per core, every member filling incompressible memory, or spawning and
/// reaping, at once (`macos_box fill`), processes rounded up. A node seeds its rates from its own class's
/// (the lead's call, 2026-11-06).
/// - `Mac16,9` (M4 Max, 14 cores, adas-mac-studio-2): 51,052,630,611 bytes and 11,316.5 processes a second,
///   both at four members per core (bench/native/macos/records/box-aggregate-growth-e765a60-20261106T1849Z.txt).
pub const CLASSES: [(&str, Rates); 1] = [(
    "Mac16,9",
    Rates {
        bytes: NonZeroU64::MIN.saturating_add(51_052_630_610),
        processes: NonZeroU64::MIN.saturating_add(11_316),
    },
)];

/// The highest rates on record, each the largest of every class's ([`CLASSES`]): a machine whose model has no
/// record is reckoned at twice these (the learned-wait rule, the lead 2026-11-07), the safe side.
pub const RECORDED: Rates = Rates {
    bytes: NonZeroU64::MIN.saturating_add(51_052_630_610),
    processes: NonZeroU64::MIN.saturating_add(11_316),
};

/// The rates a node of model `model` (its `hw.model`) seeds its boxes' pacing with: its class's record, else
/// twice the highest on record (saturating: a rate past 2^64 a second is past any box).
pub fn seed(model: &str) -> Rates {
    CLASSES.iter().find(|(m, _)| *m == model).map_or(
        Rates {
            bytes: RECORDED
                .bytes
                .saturating_mul(NonZeroU64::MIN.saturating_add(1)),
            processes: RECORDED
                .processes
                .saturating_mul(NonZeroU64::MIN.saturating_add(1)),
        },
        |(_, rates)| *rates,
    )
}

/// Each machine class's latest recorded watcher wake, by its `hw.model`: the widest gap past its armed wait
/// a box's watcher showed, its timeshare thread waiting for a core beside the box's own members and other
/// programs at equal priority (Ada's rule, 2026-11-03: no priority over other programs). What a node's holds
/// reckon its watchers can act late by to start ([`lateness_seed`], [`Watch::runs`]).
/// - `Mac16,9` (M4 Max, 14 cores, adas-mac-studio-3): 79,308 µs, a 28-member box's read armed for the 250 µs
///   floor and made 79,558 µs after the read before, one bound of the gap past the wait
///   (bench/native/macos/records/watcher-wakes-load-0cb6e11-20261108T2114Z.txt).
pub const LATENESS: [(&str, Duration); 1] = [("Mac16,9", Duration::from_micros(79_308))];

/// The wake lateness a node of model `model` seeds its holds with: its class's record, else twice the latest
/// on record (saturating), the safe side, as [`seed`] seeds its rates.
pub fn lateness_seed(model: &str) -> Duration {
    LATENESS.iter().find(|(m, _)| *m == model).map_or_else(
        || {
            LATENESS
                .iter()
                .map(|(_, l)| *l)
                .max()
                .unwrap_or(Duration::ZERO)
                .saturating_mul(2)
        },
        |(_, l)| *l,
    )
}

/// How late a node's watchers act past their armed waits, as it learns it (the learned-waits rule,
/// 2026-11-06; the lead, 2026-11-09: from the reads' own observed delays): a wake past its wait, the read
/// it makes and the stops it then sends, the whole of which a member running across a wait can run on
/// through. Its class's record to start ([`lateness_seed`]), raised to any later act a watcher sees, never
/// lowered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lateness {
    worst: Duration,
}

impl Lateness {
    pub fn new(seed: Duration) -> Self {
        Self { worst: seed }
    }

    /// The latest wake seen.
    pub fn worst(&self) -> Duration {
        self.worst
    }

    /// Notes a watcher acting `late` past its armed wait: the worst rises to it, when later.
    pub fn observe(&mut self, late: Duration) {
        self.worst = self.worst.max(late);
    }
}

/// The rate of `grown` over `over`, a second, rounded down: 0 for no growth, and the most a `u64` holds for
/// growth in no time (or past 2^64 a second), the safe side.
pub fn per_second(grown: u64, over: Duration) -> u64 {
    if grown == 0 {
        return 0;
    }
    u128::from(grown)
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|n| n.checked_div(over.as_nanos()))
        .map_or(u64::MAX, |n| u64::try_from(n).unwrap_or(u64::MAX))
}

/// A node's box growth rates as it learns them (the learned-waits rule, 2026-11-06): its class's record to
/// start ([`seed`]), raised to whatever growth a watcher sees between two reads that is faster, and never
/// lowered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LearnedRates {
    rates: Rates,
}

impl LearnedRates {
    pub fn new(seed: Rates) -> Self {
        Self { rates: seed }
    }

    pub fn rates(&self) -> Rates {
        self.rates
    }

    /// Notes a box that grew by `bytes` and `processes` over `elapsed`: each rate rises to what that
    /// growth was, when faster. Nothing is learned from no time.
    pub fn observe(&mut self, bytes: u64, processes: u64, elapsed: Duration) {
        if elapsed.is_zero() {
            return;
        }
        if let Some(b) = NonZeroU64::new(per_second(bytes, elapsed))
            && b > self.rates.bytes
        {
            self.rates.bytes = b;
        }
        if let Some(p) = NonZeroU64::new(per_second(processes, elapsed))
            && p > self.rates.processes
        {
            self.rates.processes = p;
        }
    }
}

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// The fastest a box grows, as the rule reckons it: what each wait divides the box's headroom by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rates {
    /// Footprint, in bytes a second.
    pub bytes: NonZeroU64,
    /// Processes started, a second.
    pub processes: NonZeroU64,
}

/// What a box may hold; `None` bounds nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limit {
    /// Its members' footprint summed, in bytes.
    pub memory: Option<u64>,
    /// Its processes, as [`Read::processes`] counts them.
    pub processes: Option<u64>,
}

/// Refuses, by name, the first bound in `bounds` macOS can't hold of a process box, never dropping it (CLAUDE.md §3),
/// as Windows' job refuses its own (`crate::job::limits`). macOS caps no group of processes' CPU: its CPU monitor only
/// reports or ends a process past a rate, and Hex's CPU control holds its managed VMs alone. It throttles no memory at a
/// level: a footprint limit ends a process past it. And it holds no disk share, latency target or throughput cap. The
/// bounds it holds are a box's memory and processes, the watcher's ([`Limit`]), and its CPU weight, as its QoS clamp.
pub fn check(bounds: &hex_api::Bounds) -> Result<(), hex_api::Refusal> {
    let unheld = [
        (
            bounds.cpu_max_milli.is_some(),
            "cpu_max_milli",
            "macOS caps no group of processes' CPU: its CPU monitor only reports or ends a process past a rate, and \
             Hex's CPU control holds its managed VMs alone",
        ),
        (
            bounds.memory_high.is_some(),
            "memory_high",
            "macOS throttles no process's memory at a level: a footprint limit ends a process past it",
        ),
        (
            bounds.io_weight.is_some(),
            "io_weight",
            "the process class holds no disk share on macOS",
        ),
        (
            bounds.io_latency_us.is_some(),
            "io_latency_us",
            "the process class holds no disk latency target on macOS",
        ),
        (
            bounds.io_max.is_some(),
            "io_max",
            "the process class holds no disk throughput cap on macOS",
        ),
    ];
    match unheld.into_iter().find(|(named, _, _)| *named) {
        Some((_, what, why)) => Err(hex_api::Refusal::Platform {
            what,
            platform: "macos",
            why: why.to_owned(),
        }),
        None => Ok(()),
    }
}

/// Where a member is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Life {
    Running,
    /// It has ended and waits for its parent to reap it. The process bound counts it, as Linux's
    /// `pids.max` counts a task until it is reaped.
    Ended,
    /// It has ended and the node holds it unreaped: the program leading the box's process group, kept so
    /// the group's id can't be given to another process. The process bound leaves it out, since the node
    /// keeps it, not the box.
    Held,
}

/// One process of the box, as a read found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub pid: u32,
    /// When it started, on a clock the shell keeps: with its pid, which process it is.
    pub started: u64,
    /// Its footprint in bytes, 0 once it has ended (XNU zeroes an ended process's,
    /// `bsd/kern/kern_exit.c:2079`).
    pub footprint: u64,
    pub life: Life,
    /// Whether this member carries a fatal kernel memory cap at the box's whole bound, armed by the shell
    /// when it spawned the member (macOS's `posix_spawnattr_setjetsam_ext`; native plan step 1's
    /// `_ext` backstop). A member the box did not spawn (a fork) carries none. The kernel bounds an armed
    /// member with no overshoot, so a box whose every running member is armed, and that holds at most one,
    /// waits for its next change ([`Verdict::Events`]); any fork or any unarmed member reverts it to
    /// headroom pacing.
    pub armed: bool,
}

/// Why a box is ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Over {
    /// Its footprint passed its memory bound.
    Memory { footprint: u64, bound: u64 },
    /// It held more processes than its process bound.
    Processes { count: u64, bound: u64 },
}

/// What the shell does after a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Read again after this long.
    Wait(Duration),
    /// End the box: signal every member KILL, then read again after [`FLOOR`].
    End(Over),
    /// The box is being ended and a member still lives (one that started as the kill went out): signal
    /// every member KILL again and read again after [`FLOOR`], until none lives.
    Kill,
    /// No member lives, so the box can't grow: nothing to read until a program is started in it.
    Idle,
    /// No bound needs a timer: read again when the box changes (a member forks or ends, a program is
    /// started, the box is deleted), each of which wakes the watcher.
    Events,
}

/// What a read found and decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Read {
    pub verdict: Verdict,
    /// The box's footprint: its members' summed.
    pub footprint: u64,
    /// Its processes, as its process bound counts them: every member but one [`Life::Held`].
    pub processes: u64,
    /// Its members still running.
    pub running: u64,
}

/// A member's run since its box's last read, for the rate the hold reckons it at ([`Watch::ran`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub pid: u32,
    /// How long it could run: since the last read, for a member that ran across the wait; its run for this
    /// read, for one held across it.
    pub over: Duration,
    /// It ran across the wait, so its rate is what it grew over that time. A held member runs for a read
    /// alone, which it may not run whole, so its rate is the fastest such runs show.
    pub across: bool,
    /// It had a core in its run, or it was waiting on its own (asleep), so what it grew in the run is what it
    /// does. A member left runnable with no core in its run shows nothing: its rate stays as it was.
    pub shown: bool,
}

/// One running member as the hold reckons it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Grows {
    /// Its footprint at the latest read.
    footprint: u64,
    /// Its footprint at the read before, `None` at its first.
    before: Option<u64>,
    /// The rate it grows at, in bytes a second, as its runs show it; `None` until one has.
    rate: Option<u64>,
    /// Input order makes duplicate native identities retain their original last-update semantics.
    order: usize,
}

/// One box under the rule: its bounds, the rates it is read at, and what its reads have found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    limit: Limit,
    rates: Rates,
    /// Why the box is being ended, from the read that decided it until a read finds none of its members
    /// running.
    ending: Option<Over>,
    /// The highest footprint a read found.
    peak: u64,
    reads: u64,
    /// The running-member count the last read found: while it rises, the box is still taking on members
    /// (a node starts a box's processes one after another), each beginning to allocate before the next is
    /// there, so the read paces at the floor rather than by a headroom the next new member would blow past.
    last_running: u64,
    /// Each running member's footprints and rate, by pid and start: the hold reckons each member by its own
    /// ([`Watch::runs`]). Bounded by the box's running members, the last read's.
    grows: Vec<((u32, u64), Grows)>,
    /// The next flat snapshot, reserved before any read state changes and swapped after preparation.
    scratch: Vec<((u32, u64), Grows)>,
}

impl Watch {
    pub fn new(limit: Limit, rates: Rates) -> Self {
        Self {
            limit,
            rates,
            ending: None,
            peak: 0,
            reads: 0,
            last_running: 0,
            grows: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Paces the box at `rates` from its next read on (the shell's [`LearnedRates`]).
    pub fn rated(&mut self, rates: Rates) {
        self.rates = rates;
    }

    /// One read of the box: its members as listed now.
    pub fn read(&mut self, members: &[Member]) -> Result<Read, TryReserveError> {
        self.reserve_members(members.iter().filter(|m| m.life == Life::Running).count())?;
        // A count, so saturating: 2^64 reads are never made.
        let reads = self.reads.saturating_add(1);
        for (order, m) in members
            .iter()
            .enumerate()
            .filter(|(_, m)| m.life == Life::Running)
        {
            let old = self
                .grows
                .binary_search_by_key(&(m.pid, m.started), |(key, _)| *key)
                .ok()
                .and_then(|at| self.grows.get(at))
                .map(|(_, grows)| grows);
            self.scratch.push((
                (m.pid, m.started),
                Grows {
                    footprint: m.footprint,
                    before: old.map(|grows| grows.footprint),
                    rate: old.and_then(|grows| grows.rate),
                    order,
                },
            ));
        }
        // One bulk sort, without allocating or shifting a prefix for every first-snapshot member.
        self.scratch
            .sort_unstable_by_key(|(key, grows)| (*key, grows.order));
        self.scratch.dedup_by(|later, earlier| {
            if later.0 != earlier.0 {
                return false;
            }
            earlier.1.before = Some(earlier.1.footprint);
            earlier.1.footprint = later.1.footprint;
            earlier.1.order = later.1.order;
            true
        });
        std::mem::swap(&mut self.grows, &mut self.scratch);
        self.scratch.clear();
        self.reads = reads;
        // Saturating, as a footprint summed past 2^64 bytes is past any bound.
        let footprint = members
            .iter()
            .fold(0u64, |sum, m| sum.saturating_add(m.footprint));
        let processes = count(members, |m| m.life != Life::Held);
        let running = count(members, |m| m.life == Life::Running);
        // Every running member carries its own fatal kernel cap at the whole bound. `.all` is true of no
        // members, but running == 0 is the idle branch, so this is read only with a member running.
        let all_running_armed = members
            .iter()
            .filter(|m| m.life == Life::Running)
            .all(|m| m.armed);
        let taking_on = running > self.last_running;
        self.last_running = running;
        self.peak = self.peak.max(footprint);
        let verdict = if running == 0 {
            self.ending = None;
            Verdict::Idle
        } else if self.ending.is_some() {
            Verdict::Kill
        } else if let Some(over) = self.over(footprint, processes) {
            self.ending = Some(over);
            Verdict::End(over)
        } else if running <= 1 && all_running_armed {
            // The one member carries a fatal kernel cap at the whole bound (native plan step 1's `_ext`
            // backstop), so the kernel bounds it with no overshoot, and its fork or end wakes the watcher.
            // A second member, or any member the box didn't arm (a fork), drops this and the headroom rule
            // below paces the reads.
            Verdict::Events
        } else if taking_on {
            // A member just joined and begins allocating at once; the headroom the formula would pace by
            // doesn't account for it, so read again at the floor until the membership settles.
            Verdict::Wait(FLOOR)
        } else {
            next(&self.limit, &self.rates, footprint, processes)
                .map_or(Verdict::Events, Verdict::Wait)
        };
        Ok(Read {
            verdict,
            footprint,
            processes,
            running,
        })
    }
    /// Reserve both bounded snapshots before a shell starts native effects. Failure changes no
    /// verdict, learned rate or membership; successfully reserved spare capacity may be retained.
    pub fn reserve_members(&mut self, count: usize) -> Result<(), TryReserveError> {
        self.scratch
            .try_reserve(count.saturating_sub(self.scratch.len()))?;
        self.grows
            .try_reserve(count.saturating_sub(self.grows.len()))
    }

    /// The bound the box is past, memory's first.
    fn over(&self, footprint: u64, processes: u64) -> Option<Over> {
        if let Some(bound) = self.limit.memory
            && footprint > bound
        {
            return Some(Over::Memory { footprint, bound });
        }
        match self.limit.processes {
            Some(bound) if processes > bound => Some(Over::Processes {
                count: processes,
                bound,
            }),
            _ => None,
        }
    }

    pub fn limit(&self) -> Limit {
        self.limit
    }

    /// Why the box is being ended, while it is.
    pub fn ending(&self) -> Option<Over> {
        self.ending
    }

    /// The highest footprint a read found.
    pub fn peak(&self) -> u64 {
        self.peak
    }

    /// How many reads the box has had.
    pub fn reads(&self) -> u64 {
        self.reads
    }

    /// The box's rate as the hold reckons it: its running members' rates summed, one no run has shown yet at
    /// the recorded rate, in bytes a second.
    pub fn rate(&self) -> u64 {
        let recorded = self.rates.bytes.get();
        self.grows.iter().fold(0u64, |sum, (_, g)| {
            sum.saturating_add(g.rate.unwrap_or(recorded))
        })
    }

    /// Notes what each member's run since the last read showed of its rate (the lead, 2026-11-09: each
    /// member's rate from its own runs). A run across the wait: what it grew over that time. A held member's
    /// run for a read: the fastest such runs have shown while they show it growing at all (a member just
    /// continued may not run its whole run), and 0 once one shows it not growing. A run that shows nothing (a
    /// member left runnable with no core) leaves its rate as it was; so does a member's first read, which has
    /// no read before it to grow from.
    pub fn ran(&mut self, runs: &[Run]) {
        for run in runs.iter().filter(|run| run.shown) {
            let at = self.grows.partition_point(|(key, _)| key.0 < run.pid);
            for g in self
                .grows
                .iter_mut()
                .skip(at)
                .take_while(|(key, _)| key.0 == run.pid)
                .map(|(_, g)| g)
            {
                let Some(before) = g.before else {
                    continue;
                };
                let grown = g.footprint.saturating_sub(before);
                let rate = per_second(grown, run.over);
                g.rate = Some(if run.across || grown == 0 {
                    rate
                } else {
                    rate.max(g.rate.unwrap_or(0))
                });
            }
        }
    }

    /// Which of the box's running members run across the wait `read` decided, the rest held stopped (the
    /// graded hold: the lead, 2026-11-08, the box's own bound enforced as Linux's `memory.high` throttles a
    /// cgroup near its limit, slowing no other program; 2026-11-09, as many members running as the bound
    /// allows). The watcher's next read, and its stop of a member that must not run on, can come `lateness`
    /// past the wait (learned from its own reads' delays), so a member running across the wait can grow over
    /// the wait and that before it is stopped. So `order`'s members are taken in turn, each whose rate
    /// ([`Watch::ran`]; one no run has shown yet at the recorded rate, as a member just started allocates at
    /// once) over the wait and the lateness still fits what is left of the box's headroom to its bar (its
    /// bound plus the [`OVERSHOOT`], less the read's footprint), written to `run`. Every member runs when the
    /// read's wait isn't a memory bound's (an end, an idle box, one left to events, no memory bound).
    ///
    /// Returns the wait to hold the box across when any of its members is held: the [`FLOOR`], as a held member
    /// runs only at the reads (each runs one, the shell's), so a longer wait only slows it, while those let run
    /// were reckoned over the read's own wait, no shorter. `None` leaves the read's own wait.
    pub fn runs(
        &self,
        read: &Read,
        order: &[u32],
        lateness: Duration,
        run: &mut Vec<u32>,
    ) -> Result<Option<Duration>, TryReserveError> {
        run.try_reserve(order.len().saturating_sub(run.len()))?;
        run.clear();
        let recorded = self.rates.bytes.get();
        let (Verdict::Wait(wait), Some(bound)) = (read.verdict, self.limit.memory) else {
            run.extend_from_slice(order);
            return Ok(None);
        };
        let span = wait.saturating_add(lateness).as_nanos();
        let mut left = u128::from(
            bound
                .saturating_add(OVERSHOOT)
                .saturating_sub(read.footprint),
        );
        for &pid in order {
            let at = self.grows.partition_point(|(key, _)| key.0 < pid);
            let rate = self
                .grows
                .iter()
                .skip(at)
                .take_while(|(key, _)| key.0 == pid)
                .fold(0u64, |sum, (_, g)| {
                    sum.saturating_add(g.rate.unwrap_or(recorded))
                });
            let grows = u128::from(rate)
                .saturating_mul(span)
                .checked_div(NANOS_PER_SECOND)
                .unwrap_or(u128::MAX);
            if grows < left {
                left = left.saturating_sub(grows);
                run.push(pid);
            }
        }
        Ok(if run.len() < order.len() {
            Some(FLOOR)
        } else {
            None
        })
    }
}

/// Members for which `which` holds.
fn count(members: &[Member], which: impl Fn(&Member) -> bool) -> u64 {
    // A slice holds at most isize::MAX members, so its count fits a u64.
    u64::try_from(members.iter().filter(|m| which(m)).count()).unwrap_or(u64::MAX)
}

/// The wait after a read of a box under its bounds: the shorter of the times its memory headroom plus the
/// [`OVERSHOOT`] and its process headroom last, at least the [`FLOOR`]; `None` when it has neither bound, so
/// no timer is needed.
pub fn next(limit: &Limit, rates: &Rates, footprint: u64, processes: u64) -> Option<Duration> {
    // Saturating: the sum past 2^64 bytes is past any footprint, and a box at its bound has no headroom.
    let memory = limit.memory.map(|bound| {
        wait(
            bound.saturating_add(OVERSHOOT).saturating_sub(footprint),
            rates.bytes,
        )
    });
    let processes = limit
        .processes
        .map(|bound| wait(bound.saturating_sub(processes), rates.processes));
    match (memory, processes) {
        (Some(m), Some(p)) => Some(m.min(p)),
        (one, None) | (None, one) => one,
    }
}

/// How long `headroom` lasts at `rate` a second, in whole nanoseconds rounded down, at least [`FLOOR`].
pub fn wait(headroom: u64, rate: NonZeroU64) -> Duration {
    // headroom × 10^9 fits a u128 (2^64 × 10^9 < 2^94), and the rate isn't 0, so neither step fails.
    let nanos = u128::from(headroom)
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|n| n.checked_div(u128::from(rate.get())))
        .map_or(u64::MAX, |n| u64::try_from(n).unwrap_or(u64::MAX));
    Duration::from_nanos(nanos).max(FLOOR)
}

/// How a box's members changed between two listings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Members the new listing adds, by pid: each to watch for its forks and its end.
    pub added: Vec<u32>,
    /// Members that were running at the listing before and the new one no longer holds: each ended and
    /// was reaped, or left the box's process group (setsid(2), as a member that daemonizes does), which
    /// only a look at the process tells apart.
    pub gone: Vec<Member>,
}

/// What changed from listing `before` to listing `now`, a member being its pid and start time.
pub fn changes(before: &[Member], now: &[Member]) -> Result<Changes, TryReserveError> {
    let mut was = Vec::new();
    let mut is = Vec::new();
    let mut changed = Changes::default();
    was.try_reserve_exact(before.len())?;
    is.try_reserve_exact(now.len())?;
    changed.added.try_reserve_exact(now.len())?;
    changed.gone.try_reserve_exact(before.len())?;
    was.extend(before.iter().map(|m| (m.pid, m.started)));
    is.extend(now.iter().map(|m| (m.pid, m.started)));
    was.sort_unstable();
    is.sort_unstable();
    for member in now {
        if was.binary_search(&(member.pid, member.started)).is_err() {
            changed.added.push(member.pid);
        }
    }
    for member in before {
        if member.life == Life::Running && is.binary_search(&(member.pid, member.started)).is_err()
        {
            changed.gone.push(*member);
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use proptest::prelude::*;
    use proptest::test_runner::{TestRng, TestRunner};

    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    /// AC-T7.4.1 (CLAUDE.md §3; ROOT, 2026-11-13): the bounds macOS holds of a process box pass, and each it can't is
    /// refused by name. Do check a box bounded at its memory, processes and CPU weight, then that box at each of a CPU
    /// cap, a memory level and the three disk bounds; expect the first held and each other refused as macOS's, naming
    /// its bound.
    #[test]
    fn a_bound_macos_cant_hold_is_refused_by_name() {
        let held = hex_api::Bounds {
            memory_max: Some(GIB),
            pids_max: Some(64),
            cpu_weight: Some(50),
            ..hex_api::Bounds::default()
        };
        assert_eq!(check(&held), Ok(()));
        let unheld = [
            (
                "cpu_max_milli",
                hex_api::Bounds {
                    cpu_max_milli: Some(363),
                    ..held.clone()
                },
            ),
            (
                "memory_high",
                hex_api::Bounds {
                    memory_high: Some(512 * MIB),
                    ..held.clone()
                },
            ),
            (
                "io_weight",
                hex_api::Bounds {
                    io_weight: Some(100),
                    ..held.clone()
                },
            ),
            (
                "io_latency_us",
                hex_api::Bounds {
                    io_latency_us: Some(1_000),
                    ..held.clone()
                },
            ),
            (
                "io_max",
                hex_api::Bounds {
                    io_max: Some(hex_api::IoRates {
                        read_bps: Some(MIB),
                        ..hex_api::IoRates::default()
                    }),
                    ..held.clone()
                },
            ),
        ];
        for (bound, bounds) in unheld {
            assert!(
                matches!(
                    check(&bounds),
                    Err(hex_api::Refusal::Platform { what, platform: "macos", .. }) if what == bound
                ),
                "{bound}"
            );
        }
    }

    /// AC-T7.4.1/R01: a deterministic real capacity-overflow refusal cannot change lifecycle facts.
    /// This exercises Vec's checked allocation boundary, not host OOM or a native failure campaign.
    #[test]
    fn allocation_refusal_preserves_the_previous_read_and_rates() {
        let mut watch = Watch::new(
            Limit {
                memory: Some(MIB),
                processes: None,
            },
            rates(),
        );
        watch.read(&[member(1, 2 * MIB, Life::Running)]).unwrap();
        let before = watch.clone();
        assert!(watch.reserve_members(usize::MAX).is_err());
        assert_eq!(watch, before);
        assert!(watch.ending().is_some());
        assert_eq!(watch.reads(), 1);
    }

    /// AC-T7.4.1: bulk sorting preserves native incarnation and original duplicate-update order.
    #[test]
    fn bulk_growth_keeps_incarnations_and_input_order() {
        let mut watch = Watch::new(Limit::default(), rates());
        let mut first = member(7, 4, Life::Running);
        first.started = 1;
        let mut last = first;
        last.footprint = 10;
        let mut other = member(7, 20, Life::Running);
        other.started = 2;
        watch.read(&[other, first, last]).unwrap();
        watch.ran(&[Run {
            pid: 7,
            over: Duration::from_secs(1),
            across: true,
            shown: true,
        }]);
        assert_eq!(watch.rate(), rates().bytes.get() + 6);
        watch.read(&[other]).unwrap();
        assert_eq!(watch.rate(), rates().bytes.get());
        assert_eq!(watch.grows.len(), 1);
    }

    /// RESULT.md's: 256 GiB/s, and a process rate high enough not to matter here.
    fn rates() -> Rates {
        Rates {
            bytes: NonZeroU64::new(256 * GIB).unwrap(),
            processes: NonZeroU64::new(1_000_000).unwrap(),
        }
    }

    fn member(pid: u32, footprint: u64, life: Life) -> Member {
        Member {
            pid,
            started: u64::from(pid) * 7,
            footprint,
            life,
            armed: false,
        }
    }

    /// A member carrying the fatal kernel cap (one the box spawned), for the ceiling-pacing cases.
    fn armed(pid: u32, footprint: u64, life: Life) -> Member {
        Member {
            armed: true,
            ..member(pid, footprint, life)
        }
    }

    /// HEX-MAC-4, the lead's call (2026-11-06): the rates the rule is reckoned at on record are the worst
    /// aggregate a box reached, one member per core filling memory and spawning at once, and the bar a box may
    /// pass its bound by between reads is step 1's 64 MiB. Do read them; expect the record's figures, its
    /// spawn rate rounded up (bench/native/macos/records/box-aggregate-growth-f83189c-20261106T1718Z.txt).
    #[test]
    fn the_recorded_rates_are_the_worst_aggregate_on_record() {
        assert_eq!(seed("Mac16,9").bytes.get(), 51_052_630_611);
        assert_eq!(seed("Mac16,9").processes.get(), 11_317);
        assert_eq!(OVERSHOOT, 64 * MIB);
    }

    /// HEX-MAC-4, the learned-wait rule (the lead, 2026-11-07): a node seeds its rates from its own machine
    /// class's record, and a model with no record from twice the highest record of any class, the safe side.
    /// Do seed an unrecorded model; expect twice the highest bytes and twice the highest processes on record.
    #[test]
    fn an_unrecorded_model_seeds_twice_the_highest_on_record() {
        let highest_bytes = CLASSES.iter().map(|(_, r)| r.bytes.get()).max().unwrap();
        let highest_processes = CLASSES
            .iter()
            .map(|(_, r)| r.processes.get())
            .max()
            .unwrap();
        let unknown = seed("Mac99,1");
        assert_eq!(unknown.bytes.get(), 2 * highest_bytes);
        assert_eq!(unknown.processes.get(), 2 * highest_processes);
        assert_eq!(RECORDED.bytes.get(), highest_bytes);
        for (model, rates) in CLASSES {
            assert_eq!(seed(model), rates, "{model}");
        }
    }

    fn limit(bound: u64) -> Limit {
        Limit {
            memory: Some(bound),
            processes: None,
        }
    }

    /// A box of `bound` whose members (pids 1 on) were read at `before`, then at `after`, each having run
    /// across the wait between them for `over`: the box and its second read.
    fn across(bound: u64, before: &[u64], after: &[u64], over: Duration) -> (Watch, Read) {
        let mut w = Watch::new(limit(bound), RECORDED);
        let at = |footprints: &[u64]| -> Vec<Member> {
            footprints
                .iter()
                .zip(1u32..)
                .map(|(f, pid)| member(pid, *f, Life::Running))
                .collect()
        };
        let _ = w.read(&at(before)).unwrap();
        let read = w.read(&at(after)).unwrap();
        let runs: Vec<Run> = (1u32..)
            .take(after.len())
            .map(|pid| Run {
                pid,
                over,
                across: true,
                shown: true,
            })
            .collect();
        w.ran(&runs);
        (w, read)
    }

    /// Member `pid`'s run for one read of a held box, showing what it does.
    fn read_run(pid: u32, over: Duration) -> [Run; 1] {
        [Run {
            pid,
            over,
            across: false,
            shown: true,
        }]
    }

    /// HEX-MAC-4, the graded hold (the lead, 2026-11-08 and 2026-11-09): a member growing at the recorded rate
    /// reaches the box's bar by the read its wait asks for, so any lateness of that read would carry it past,
    /// while a member growing at none can run across any wait. Do read a box whose member 1 grew at the
    /// recorded rate (51,052,631 bytes in 1 ms, rounded up to a byte) and member 2 nothing, and ask with a wake
    /// 1 µs late; expect member 2 run and member 1 held, at the floor. Do ask of a box that grew nothing with a
    /// wake a day late; expect both run, at the read's own wait.
    #[test]
    fn a_member_growing_at_the_recorded_rate_is_held_and_one_growing_at_none_runs() {
        let ms = Duration::from_millis(1);
        let (w, read) = across(256 * MIB, &[39 * MIB, 0], &[39 * MIB + 51_052_631, 0], ms);
        assert!(matches!(read.verdict, Verdict::Wait(_)));
        let mut run = Vec::new();
        assert_eq!(
            w.runs(&read, &[1, 2], Duration::from_micros(1), &mut run)
                .unwrap(),
            Some(FLOOR)
        );
        assert_eq!(run, [2]);
        assert_eq!(w.rate(), 51_052_631_000);
        let (still, read) = across(256 * MIB, &[39 * MIB, 0], &[39 * MIB, 0], ms);
        assert_eq!(
            still
                .runs(&read, &[1, 2], Duration::from_secs(86_400), &mut run)
                .unwrap(),
            None
        );
        assert_eq!(run, [1, 2]);
        assert_eq!(still.rate(), 0);
    }

    /// HEX-MAC-4, the graded hold (the lead, 2026-11-09: each member's rate from its own runs): the box's rate
    /// is its members' summed, each from what its own runs showed. Do read a box of three whose member 1 grew
    /// 10 MiB in 1 ms across the wait, then run member 2 alone for a read of 100 µs while it grows 1 MiB, then
    /// member 1 alone growing nothing; expect the box at 10 MiB/ms, then twice that, then member 2's alone.
    #[test]
    fn a_box_is_reckoned_at_its_members_rates_summed_each_from_its_own_runs() {
        let ms = Duration::from_millis(1);
        let fast = per_second(10 * MIB, ms);
        let (mut w, _) = across(GIB, &[0, 0, 0], &[10 * MIB, 0, 0], ms);
        assert_eq!(w.rate(), fast);
        let three = |a, b| {
            [
                member(1, a, Life::Running),
                member(2, b, Life::Running),
                member(3, 0, Life::Running),
            ]
        };
        let _ = w.read(&three(10 * MIB, MIB)).unwrap();
        w.ran(&read_run(2, Duration::from_micros(100)));
        assert_eq!(w.rate(), 2 * fast);
        let _ = w.read(&three(10 * MIB, MIB)).unwrap();
        w.ran(&read_run(1, Duration::from_micros(100)));
        assert_eq!(w.rate(), fast);
    }

    /// HEX-MAC-4, the graded hold (the lead, 2026-11-09): a member run for a read but left runnable with no core
    /// grew nothing because it didn't run, which says nothing of its rate. Do read a box whose member 1 grew
    /// 10 MiB in 1 ms, so that with the class's lateness it is held; run it for a read with no core, then with
    /// one, growing nothing both times; expect it held at its rate, still held, then run at 0.
    #[test]
    fn a_held_members_run_with_no_core_shows_nothing() {
        let ms = Duration::from_millis(1);
        let late = lateness_seed("Mac16,9");
        let (mut w, read) = across(256 * MIB, &[100 * MIB, 0], &[110 * MIB, 0], ms);
        let mut run = Vec::new();
        let _ = w.runs(&read, &[1, 2], late, &mut run).unwrap();
        assert_eq!(run, [2]);
        let two = [
            member(1, 110 * MIB, Life::Running),
            member(2, 0, Life::Running),
        ];
        let read = w.read(&two).unwrap();
        w.ran(&[Run {
            pid: 1,
            over: ms,
            across: false,
            shown: false,
        }]);
        assert_eq!(w.rate(), per_second(10 * MIB, ms));
        let _ = w.runs(&read, &[1, 2], late, &mut run).unwrap();
        assert_eq!(run, [2]);
        let read = w.read(&two).unwrap();
        w.ran(&read_run(1, ms));
        assert_eq!(w.rate(), 0);
        let _ = w.runs(&read, &[1, 2], late, &mut run).unwrap();
        assert_eq!(run, [1, 2]);
    }

    /// HEX-MAC-4, the graded hold: a held member runs for a read, which it may not run whole, so a slower run
    /// alone isn't it slowing: its rate is the fastest such runs show while they show it growing at all, and 0
    /// once one shows it not growing. Do run member 1 for four reads of 1 ms, growing 10 MiB, 1 MiB, nothing,
    /// then 1 MiB; expect 10 MiB/ms, 10 MiB/ms, 0, then 1 MiB/ms.
    #[test]
    fn a_held_members_rate_is_the_fastest_its_runs_show_until_one_shows_none() {
        let ms = Duration::from_millis(1);
        let (mut w, _) = across(GIB, &[0, 0], &[0, 0], ms);
        for (footprint, rate) in [
            (10 * MIB, per_second(10 * MIB, ms)),
            (11 * MIB, per_second(10 * MIB, ms)),
            (11 * MIB, 0),
            (12 * MIB, per_second(MIB, ms)),
        ] {
            let _ = w
                .read(&[
                    member(1, footprint, Life::Running),
                    member(2, 0, Life::Running),
                ])
                .unwrap();
            w.ran(&read_run(1, ms));
            assert_eq!(w.rate(), rate, "at {footprint}");
        }
    }

    /// HEX-MAC-4, the graded hold: a member no run has shown yet (one just started, which begins allocating at
    /// once) is reckoned at the recorded rate until a run of it shows its own. Do read one member, then two
    /// (taking on, so the floor); expect the new one held when the floor and the lateness at the recorded rate
    /// would carry it past the bar, and run when they wouldn't; then run it for a read, growing nothing; expect
    /// the box at 0.
    #[test]
    fn a_member_no_run_has_shown_is_reckoned_at_the_recorded_rate() {
        let ms = Duration::from_millis(1);
        let mut w = Watch::new(limit(256 * MIB), RECORDED);
        let _ = w.read(&[member(1, MIB, Life::Running)]).unwrap();
        let both = [member(1, MIB, Life::Running), member(2, 0, Life::Running)];
        let read = w.read(&both).unwrap();
        assert_eq!(read.verdict, Verdict::Wait(FLOOR));
        w.ran(&[Run {
            pid: 1,
            over: ms,
            across: true,
            shown: true,
        }]);
        assert_eq!(w.rate(), RECORDED.bytes.get());
        let mut run = Vec::new();
        // 319 MiB of headroom to the bar lasts 6.55 ms at the recorded rate; the floor is 250 µs of it.
        let _ = w
            .runs(&read, &[1, 2], Duration::from_millis(7), &mut run)
            .unwrap();
        assert_eq!(run, [1]);
        let _ = w
            .runs(&read, &[1, 2], Duration::from_millis(6), &mut run)
            .unwrap();
        assert_eq!(run, [1, 2]);
        let _ = w.read(&both).unwrap();
        w.ran(&read_run(2, ms));
        assert_eq!(w.rate(), 0);
    }

    /// HEX-MAC-4, the graded hold (the lead, 2026-11-09: as many members running as the bound allows): members
    /// are taken in turn, each whose growth over the wait and the lateness fits what the ones before it left of
    /// the headroom, so a slow member after a fast one that doesn't fit still runs. Do read a 960 MiB box whose
    /// members 1 and 3 grow 600 KiB a millisecond and 2 and 4 100 KiB, with a second's lateness (about 598 MiB
    /// and 100 MiB each, against 1,023 MiB of headroom), in turn from member 1 and from member 3; expect 1, 2
    /// and 4 run, then 3, 4 and 2, the one held at the floor.
    #[test]
    fn the_graded_hold_runs_each_member_whose_growth_fits_what_is_left_in_turn() {
        let ms = Duration::from_millis(1);
        let (fast, slow) = (600 << 10, 100 << 10);
        let (w, read) = across(960 * MIB, &[0, 0, 0, 0], &[fast, slow, fast, slow], ms);
        let mut run = Vec::new();
        let late = Duration::from_secs(1);
        assert_eq!(
            w.runs(&read, &[1, 2, 3, 4], late, &mut run).unwrap(),
            Some(FLOOR)
        );
        assert_eq!(run, [1, 2, 4]);
        assert_eq!(
            w.runs(&read, &[3, 4, 1, 2], late, &mut run).unwrap(),
            Some(FLOOR)
        );
        assert_eq!(run, [3, 4, 2]);
    }

    /// HEX-MAC-4, the graded hold: a box none of whose members fits grows only at its reads, each running one
    /// held member, so it is held across the floor, the shortest wait. Do read a 256 MiB box of two members
    /// each growing at the recorded rate, with the class's lateness; expect none run, at the floor.
    #[test]
    fn a_box_none_of_whose_members_fits_is_held_across_the_floor() {
        let (w, read) = across(
            256 * MIB,
            &[0, 0],
            &[51_052_631, 51_052_631],
            Duration::from_millis(1),
        );
        let mut run = Vec::new();
        assert_eq!(
            w.runs(&read, &[1, 2], lateness_seed("Mac16,9"), &mut run)
                .unwrap(),
            Some(FLOOR)
        );
        assert!(run.is_empty());
    }

    /// HEX-MAC-4, the graded hold: only a memory bound's wait holds members. Do ask of a box past its bound
    /// (end), an idle one and one left to events, each with any lateness; expect every member run, at the
    /// read's own wait.
    #[test]
    fn only_a_memory_wait_holds_members() {
        let mut run = Vec::new();
        let mut w = Watch::new(limit(256 * MIB), RECORDED);
        let over = w.read(&[member(1, 300 * MIB, Life::Running)]).unwrap();
        assert!(matches!(over.verdict, Verdict::End(_)));
        assert_eq!(w.runs(&over, &[1], Duration::MAX, &mut run).unwrap(), None);
        assert_eq!(run, [1]);
        let mut idle = Watch::new(limit(256 * MIB), RECORDED);
        let none = idle.read(&[]).unwrap();
        assert_eq!(none.verdict, Verdict::Idle);
        assert_eq!(
            idle.runs(&none, &[], Duration::MAX, &mut run).unwrap(),
            None
        );
        assert!(run.is_empty());
        let mut one = Watch::new(limit(256 * MIB), RECORDED);
        let events = one.read(&[armed(1, MIB, Life::Running)]).unwrap();
        assert_eq!(events.verdict, Verdict::Events);
        assert_eq!(
            one.runs(&events, &[1], Duration::MAX, &mut run).unwrap(),
            None
        );
        assert_eq!(run, [1]);
    }

    proptest! {
        /// HEX-MAC-4, the graded hold (model-based, the lead 2026-11-09): of a box whose members all ran across
        /// the wait, each grown by any amount over any time, the members run across the next wait are, in turn,
        /// each whose growth at its own rate over the wait and any lateness fits what the ones before it left
        /// of the headroom; so together they grow less than the headroom before the watcher can stop them, and
        /// a box holding any waits at the floor. Do read up to six members twice and ask, from any turn; expect
        /// the oracle's members, their growth within the headroom, and the oracle's wait.
        #[test]
        fn the_members_run_across_a_wait_fit_its_headroom(
            grown in proptest::collection::vec(0u64..(1u64 << 30), 1..6),
            over_us in 1u64..100_000,
            late_us in 0u64..200_000,
            turn in 0usize..6,
        ) {
            let bound = 4 * GIB;
            let n = grown.len();
            let over = Duration::from_micros(over_us);
            let (w, read) = across(bound, &vec![0; n], &grown, over);
            let order: Vec<u32> = (0..n)
                .map(|i| u32::try_from((turn + i) % n + 1).unwrap())
                .collect();
            let late = Duration::from_micros(late_us);
            let mut run = Vec::new();
            let wait = w.runs(&read, &order, late, &mut run).unwrap();
            let Verdict::Wait(read_wait) = read.verdict else {
                prop_assert_eq!(&run, &order);
                prop_assert_eq!(wait, None);
                return Ok(());
            };
            let span = read_wait.saturating_add(late).as_nanos();
            let headroom = u128::from((bound + OVERSHOOT).saturating_sub(read.footprint));
            let growth = |pid: u32| {
                let g = grown[usize::try_from(pid).unwrap() - 1];
                u128::from(per_second(g, over)) * span / 1_000_000_000
            };
            let mut left = headroom;
            let mut oracle = Vec::new();
            for &pid in &order {
                if growth(pid) < left {
                    left -= growth(pid);
                    oracle.push(pid);
                }
            }
            prop_assert_eq!(&run, &oracle);
            prop_assert!(
                run.is_empty() || run.iter().map(|&pid| growth(pid)).sum::<u128>() < headroom
            );
            let held_wait = if run.len() < order.len() { Some(FLOOR) } else { None };
            prop_assert_eq!(wait, held_wait);
        }
    }

    /// HEX-MAC-4, the learned-waits rule: a node reckons its holds with its class's recorded wake lateness
    /// to start, raised to any later wake its watchers see, never lowered; a model with no record starts
    /// at twice the highest on record. Do seed Mac16,9 and observe a shorter, a longer, then a shorter wake;
    /// expect the record, then the longer kept; and an unrecorded model at twice the record.
    #[test]
    fn the_lateness_seeds_from_the_class_record_and_only_rises() {
        assert_eq!(lateness_seed("Mac16,9"), Duration::from_micros(79_308));
        let highest = LATENESS.iter().map(|(_, l)| *l).max().unwrap();
        assert_eq!(lateness_seed("Mac99,1"), highest * 2);
        let mut l = Lateness::new(lateness_seed("Mac16,9"));
        l.observe(Duration::from_millis(1));
        assert_eq!(l.worst(), Duration::from_micros(79_308));
        l.observe(Duration::from_millis(100));
        assert_eq!(l.worst(), Duration::from_millis(100));
        l.observe(Duration::from_millis(1));
        assert_eq!(l.worst(), Duration::from_millis(100));
    }

    /// HEX-MAC-4: a box's growth rate from what it grew over how long. Do ask 47 MiB over 1 ms, nothing over
    /// no time, and something over no time; expect 49,283,072,000 B/s, 0 and the most a u64 holds.
    #[test]
    fn a_growth_rate_is_its_growth_over_its_time() {
        assert_eq!(
            per_second(47 * MIB, Duration::from_millis(1)),
            49_283_072_000
        );
        assert_eq!(per_second(0, Duration::ZERO), 0);
        assert_eq!(per_second(1, Duration::ZERO), u64::MAX);
    }

    /// HEX-MAC-4, the learned-waits rule: a box seen filling or spawning faster than the rates raises them
    /// to what it was seen doing, and nothing lowers them. Do observe 100 MiB in 1 ms and 50 processes in
    /// 1 ms, then slower growth; expect the rates raised to 104,857,600,000 B/s and 50,000/s, and kept.
    #[test]
    fn an_observed_faster_fill_raises_the_rate_and_only_raises() {
        let mut l = LearnedRates::new(seed("Mac16,9"));
        l.observe(100 * MIB, 50, Duration::from_millis(1));
        assert_eq!(l.rates().bytes.get(), 104_857_600_000);
        assert_eq!(l.rates().processes.get(), 50_000);
        l.observe(MIB, 1, Duration::from_millis(1));
        assert_eq!(l.rates().bytes.get(), 104_857_600_000);
        assert_eq!(l.rates().processes.get(), 50_000);
        l.observe(MIB, 1, Duration::ZERO);
        assert_eq!(l.rates().bytes.get(), 104_857_600_000);
    }

    /// HEX-MAC-4: at the recorded rates no box can pass its bound by more than the bar before its next
    /// read, every member filling at once: a box at its bound is read again within the bar's time,
    /// 64 MiB / 51,052,630,611 B/s = 1,314,503 ns. Do ask the wait at the bound; expect that time.
    #[test]
    fn a_box_at_its_bound_is_read_within_the_bars_time() {
        let limit = Limit {
            memory: Some(256 * MIB),
            processes: None,
        };
        assert_eq!(
            next(&limit, &RECORDED, 256 * MIB, 1),
            Some(Duration::from_nanos(1_314_503))
        );
    }

    proptest! {
        /// HEX-MAC-4 (model-based): for any bound and any footprint at or under it, the box grows at most
        /// the bar past its bound before its next read at the recorded rates: wait x rate <= bound + bar -
        /// footprint. Do ask the wait; expect that, with no ceiling and no floor needed to hold it.
        #[test]
        fn no_box_passes_its_bound_by_more_than_the_bar_between_reads(
            bound in MIB..(1u64 << 40),
            at in 0u64..=1000,
        ) {
            let footprint = bound / 1000 * at;
            let limit = Limit { memory: Some(bound), processes: None };
            let d = next(&limit, &RECORDED, footprint, 1).unwrap();
            prop_assert!(
                d.as_nanos() * u128::from(RECORDED.bytes.get())
                    <= u128::from(bound + OVERSHOOT - footprint) * 1_000_000_000
            );
        }
    }

    /// HEX-MAC-4: a box waits as long as its headroom plus the bar lasts at its rate: at 95% of a 256 MiB
    /// bound, 12.8 MiB + 64 MiB at 256 GiB/s; at 1 MiB, 255 MiB + 64 MiB; at its bound, the bar's time, held
    /// at the floor; one with no bound needs no timer.
    #[test]
    fn a_box_is_read_as_often_as_its_headroom_asks() {
        let limit = Limit {
            memory: Some(256 * MIB),
            processes: None,
        };
        assert_eq!(
            next(&limit, &rates(), 256 * MIB / 100 * 95, 1),
            Some(Duration::from_nanos(292_968))
        );
        // 319 MiB at 256 GiB/s: 319 / (256 × 1024) s, 1,216,888 ns rounded down.
        assert_eq!(
            next(&limit, &rates(), MIB, 1),
            Some(Duration::from_nanos(1_216_888))
        );
        assert_eq!(next(&Limit::default(), &rates(), 64 * GIB, 900), None);
        // At the bound exactly the box isn't past it: the bar's 64 MiB lasts 244 µs, under the floor.
        assert_eq!(next(&limit, &rates(), 256 * MIB, 1), Some(FLOOR));
    }

    /// HEX-MAC-4: the process bound's headroom paces reads too, the shorter wait winning.
    #[test]
    fn the_shorter_of_the_two_headrooms_sets_the_wait() {
        let limit = Limit {
            memory: Some(256 * MIB),
            processes: Some(64),
        };
        let slow_forks = Rates {
            processes: NonZeroU64::new(1_000).unwrap(),
            ..rates()
        };
        // 1 MiB used (1.217 ms with the bar), 63 processes left at 1,000/s (63 ms): memory's wins.
        assert_eq!(
            next(&limit, &slow_forks, MIB, 1),
            Some(Duration::from_nanos(1_216_888))
        );
        // 1 process left at 10,000/s lasts 100 µs, under memory's 1.217 ms: held at the floor.
        let fast_forks = Rates {
            processes: NonZeroU64::new(10_000).unwrap(),
            ..rates()
        };
        assert_eq!(next(&limit, &fast_forks, MIB, 63), Some(FLOOR));
    }

    /// HEX-MAC-4 (native plan step 1's `_ext` backstop): a box holding at most one running member, every
    /// running member armed with the kernel's fatal cap, needs no timer: the kernel bounds the member with
    /// no overshoot, and a fork or an end wakes the watcher. Unarmed, the same read is paced by its headroom.
    #[test]
    fn a_single_armed_member_waits_for_events() {
        let limit = Limit {
            memory: Some(256 * MIB),
            processes: None,
        };
        let mut w = Watch::new(limit, rates());
        assert_eq!(
            w.read(&[armed(1, MIB, Life::Running)]).unwrap().verdict,
            Verdict::Events
        );
        assert_eq!(
            w.read(&[armed(1, MIB, Life::Running)]).unwrap().verdict,
            Verdict::Events
        );
        let mut u = Watch::new(limit, rates());
        let _ = u.read(&[member(1, MIB, Life::Running)]).unwrap(); // taking on -> floor
        let unarmed = u.read(&[member(1, MIB, Life::Running)]).unwrap().verdict;
        assert_eq!(
            unarmed,
            Verdict::Wait(next(&limit, &rates(), MIB, 1).unwrap())
        );
    }

    /// HEX-MAC-4: a two-member box is paced by headroom, not left to events, even when both members are
    /// armed — the kernel caps each at the whole bound, which doesn't bound their sum, so the watcher's
    /// headroom rule holds the box.
    #[test]
    fn a_two_member_box_is_paced_by_headroom() {
        let limit = Limit {
            memory: Some(256 * MIB),
            processes: None,
        };
        let mut w = Watch::new(limit, rates());
        let two = [armed(1, MIB, Life::Running), armed(2, MIB, Life::Running)];
        let _ = w.read(&two).unwrap(); // taking on 0 -> 2: floor
        let steady = w.read(&two).unwrap().verdict;
        assert_eq!(
            steady,
            Verdict::Wait(next(&limit, &rates(), 2 * MIB, 2).unwrap())
        );
    }

    /// HEX-MAC-4: a fork (an unarmed member the box didn't spawn) reverts the box to headroom pacing,
    /// whether it joins an armed member or is all that's left of one — any unarmed running member puts the
    /// box back on a timer.
    #[test]
    fn a_fork_or_an_unarmed_member_reverts_to_headroom() {
        let limit = Limit {
            memory: Some(256 * MIB),
            processes: None,
        };
        // An armed member and its forked (unarmed) child: two running members, so headroom.
        let mut w = Watch::new(limit, rates());
        let pair = [armed(1, MIB, Life::Running), member(2, MIB, Life::Running)];
        let _ = w.read(&pair).unwrap();
        let with_fork = w.read(&pair).unwrap().verdict;
        assert_eq!(
            with_fork,
            Verdict::Wait(next(&limit, &rates(), 2 * MIB, 2).unwrap())
        );
        // Only the unarmed fork remains (the armed member exited): one running member, unarmed -> headroom.
        let mut u = Watch::new(limit, rates());
        let _ = u.read(&[member(2, MIB, Life::Running)]).unwrap();
        let lone_fork = u.read(&[member(2, MIB, Life::Running)]).unwrap().verdict;
        assert_eq!(
            lone_fork,
            Verdict::Wait(next(&limit, &rates(), MIB, 1).unwrap())
        );
    }

    /// HEX-MAC-4: a box with no bound needs no timer: nothing it does can pass a bound, so after taking
    /// its members on it is read when it changes (a fork, an end, a start, a delete).
    #[test]
    fn an_unbounded_box_waits_for_events() {
        let none = Limit::default();
        let mut w = Watch::new(none, rates());
        assert_eq!(
            w.read(&[member(1, MIB, Life::Running)]).unwrap().verdict,
            Verdict::Wait(FLOOR)
        );
        assert_eq!(
            w.read(&[member(1, MIB, Life::Running)]).unwrap().verdict,
            Verdict::Events
        );
        let mut t = Watch::new(none, rates());
        let two = [member(1, MIB, Life::Running), member(2, MIB, Life::Running)];
        let _ = t.read(&two).unwrap();
        assert_eq!(t.read(&two).unwrap().verdict, Verdict::Events);
    }

    /// HEX-MAC-1: a read past the memory bound ends the box with the footprint it reached; a read while
    /// a member still runs kills again; a read with none running leaves the box idle, and the box is
    /// watched afresh after.
    #[test]
    fn a_box_past_its_bound_is_ended_once_and_killed_until_empty() {
        let mut w = Watch::new(
            Limit {
                memory: Some(64 * MIB),
                processes: None,
            },
            rates(),
        );
        let under = w.read(&[member(10, 60 * MIB, Life::Running)]).unwrap();
        assert_eq!(under.verdict, Verdict::Wait(FLOOR));
        let past = w
            .read(&[
                member(10, 50 * MIB, Life::Running),
                member(11, 20 * MIB, Life::Running),
            ])
            .unwrap();
        assert_eq!(
            past.verdict,
            Verdict::End(Over::Memory {
                footprint: 70 * MIB,
                bound: 64 * MIB
            })
        );
        assert_eq!(w.peak(), 70 * MIB);
        // One started as the kill went out: it is killed too, and the end isn't decided twice.
        let late = w
            .read(&[member(10, 0, Life::Held), member(12, MIB, Life::Running)])
            .unwrap();
        assert_eq!(late.verdict, Verdict::Kill);
        let empty = w.read(&[member(10, 0, Life::Held)]).unwrap();
        assert_eq!(empty.verdict, Verdict::Idle);
        assert_eq!(w.ending(), None);
        let again = w
            .read(&[member(10, 0, Life::Held), member(13, MIB, Life::Running)])
            .unwrap();
        assert!(matches!(again.verdict, Verdict::Wait(_)), "{again:?}");
        assert_eq!(w.reads(), 5);
    }

    /// HEX-MAC-1: the process bound counts a running member and one ended but unreaped, not the node's
    /// held leader; past it the box is ended with the count.
    #[test]
    fn the_process_bound_counts_every_member_but_the_held_leader() {
        let mut w = Watch::new(
            Limit {
                memory: None,
                processes: Some(2),
            },
            rates(),
        );
        let held = w
            .read(&[
                member(20, 0, Life::Held),
                member(21, MIB, Life::Running),
                member(22, 0, Life::Ended),
            ])
            .unwrap();
        assert_eq!((held.processes, held.running), (2, 1));
        assert!(matches!(held.verdict, Verdict::Wait(_)), "{held:?}");
        let past = w
            .read(&[
                member(20, 0, Life::Held),
                member(21, MIB, Life::Running),
                member(22, 0, Life::Ended),
                member(23, MIB, Life::Running),
            ])
            .unwrap();
        assert_eq!(
            past.verdict,
            Verdict::End(Over::Processes { count: 3, bound: 2 })
        );
    }

    /// HEX-MAC-6: a running member the next listing lacks is gone, to be looked at; one already ended
    /// isn't; a pid back with another start time is a new process.
    #[test]
    fn a_listing_says_which_members_are_new_and_which_went() {
        let before = [
            member(30, MIB, Life::Running),
            member(31, MIB, Life::Running),
            member(32, 0, Life::Ended),
        ];
        let reused = Member {
            started: 999,
            ..member(31, MIB, Life::Running)
        };
        let now = [member(30, MIB, Life::Running), reused];
        let c = changes(&before, &now).unwrap();
        assert_eq!(c.added, vec![31]);
        assert_eq!(c.gone, vec![member(31, MIB, Life::Running)]);
    }

    /// The rule as its decision states it, written apart from the core: the longest whole number of
    /// nanoseconds whose growth at `rate` stays within `headroom`, found by bisection, held at the floor at
    /// least and at u64::MAX nanoseconds at most.
    fn oracle_wait(headroom: u64, rate: u64) -> Duration {
        let top = u128::from(u64::MAX);
        let fits = |ns: u128| ns * u128::from(rate) <= u128::from(headroom) * 1_000_000_000;
        // Invariant: `lo` fits; `hi` doesn't, or is past the top.
        let (mut lo, mut hi) = (0u128, top + 1);
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if fits(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Duration::from_nanos(u64::try_from(lo.clamp(FLOOR.as_nanos(), top)).unwrap())
    }

    /// What the oracle says of read `i` of a series: the box is idle with none of its members running;
    /// being ended if a read since it was last idle was past a bound; ended if this read is past one; else
    /// it waits the shorter of its headrooms.
    fn oracle(limit: Limit, rates: Rates, series: &[Vec<Member>], i: usize) -> Verdict {
        let sum = |read: &[Member]| read.iter().map(|m| u128::from(m.footprint)).sum::<u128>();
        let counted = |read: &[Member]| {
            u64::try_from(read.iter().filter(|m| m.life != Life::Held).count()).unwrap()
        };
        let running = |read: &[Member]| read.iter().any(|m| m.life == Life::Running);
        let past = |read: &[Member]| -> Option<Over> {
            let footprint = u64::try_from(sum(read).min(u128::from(u64::MAX))).unwrap();
            if limit
                .memory
                .is_some_and(|b| u128::from(footprint) > u128::from(b))
            {
                return Some(Over::Memory {
                    footprint,
                    bound: limit.memory.unwrap(),
                });
            }
            limit
                .processes
                .filter(|b| counted(read) > *b)
                .map(|bound| Over::Processes {
                    count: counted(read),
                    bound,
                })
        };
        if !running(&series[i]) {
            return Verdict::Idle;
        }
        let since_idle = series[..i]
            .iter()
            .rposition(|read| !running(read))
            .map_or(0, |idle| idle + 1);
        if series[since_idle..i]
            .iter()
            .any(|read| past(read).is_some())
        {
            return Verdict::Kill;
        }
        if let Some(over) = past(&series[i]) {
            return Verdict::End(over);
        }
        let running_n = |read: &[Member]| {
            u64::try_from(read.iter().filter(|m| m.life == Life::Running).count()).unwrap()
        };
        // Every running member armed and at most one running: the kernel caps the member, so the box waits
        // for events regardless of headroom or taking-on (native plan step 1's `_ext` backstop).
        let all_armed = |read: &[Member]| {
            read.iter()
                .filter(|m| m.life == Life::Running)
                .all(|m| m.armed)
        };
        if running_n(&series[i]) <= 1 && all_armed(&series[i]) {
            return Verdict::Events;
        }
        // A read that finds more running members than the one before it (the box is still taking members
        // on) paces at the floor, regardless of headroom.
        let prev_running = if i == 0 { 0 } else { running_n(&series[i - 1]) };
        if running_n(&series[i]) > prev_running {
            return Verdict::Wait(FLOOR);
        }
        let footprint = u64::try_from(sum(&series[i]).min(u128::from(u64::MAX))).unwrap();
        let memory = limit
            .memory
            .map(|b| oracle_wait((b + OVERSHOOT).saturating_sub(footprint), rates.bytes.get()));
        let processes = limit
            .processes
            .map(|b| oracle_wait(b.saturating_sub(counted(&series[i])), rates.processes.get()));
        match (memory, processes) {
            (Some(m), Some(p)) => Verdict::Wait(m.min(p)),
            (Some(d), None) | (None, Some(d)) => Verdict::Wait(d),
            (None, None) => Verdict::Events,
        }
    }

    fn arb_life() -> impl Strategy<Value = Life> {
        prop_oneof![Just(Life::Running), Just(Life::Ended), Just(Life::Held)]
    }

    fn arb_series() -> impl Strategy<Value = Vec<Vec<Member>>> {
        // `armed` weighted toward true so single-armed reads (the events path) are met often.
        let member = (
            1u32..64,
            0u64..(64 * GIB),
            arb_life(),
            proptest::bool::weighted(0.6),
        )
            .prop_map(|(pid, footprint, life, armed)| Member {
                pid,
                started: u64::from(pid),
                footprint: if life == Life::Running { footprint } else { 0 },
                life,
                armed,
            });
        prop::collection::vec(prop::collection::vec(member, 0..8), 1..120)
    }

    fn arb_limit() -> impl Strategy<Value = Limit> {
        (
            prop::option::of(MIB..(64 * GIB)),
            prop::option::of(1u64..64),
        )
            .prop_map(|(memory, processes)| Limit { memory, processes })
    }

    fn arb_rates() -> impl Strategy<Value = Rates> {
        (1u64..(1 << 41), 1u64..10_000_000).prop_map(|(bytes, processes)| Rates {
            bytes: NonZeroU64::new(bytes).unwrap(),
            processes: NonZeroU64::new(processes).unwrap(),
        })
    }

    /// The verdicts of each kind the model-based test met, so it can't pass by never meeting one
    /// (CLAUDE.md §6's non-vacuity counters).
    #[derive(Default)]
    struct Met {
        floor: Cell<u64>,
        above: Cell<u64>,
        events: Cell<u64>,
        /// A wait for events the arming rule decided: one running member, armed (native plan step 1).
        armed_events: Cell<u64>,
        memory: Cell<u64>,
        processes: Cell<u64>,
        kill: Cell<u64>,
        idle: Cell<u64>,
    }

    /// HEX-MAC-4 (model-based, CLAUDE.md §6): for random series of reads, each of the core's verdicts is
    /// the oracle's, its peak is the series' highest footprint, and no wait is longer than the headroom plus
    /// the bar lasts at the stated rate, unless it is the floor. Every kind of verdict is met: a wait at the
    /// floor and above it, a wait for events and one the arming rule decided; an end for memory and for
    /// processes; a kill; a box gone idle. Drawn from proptest's deterministic RNG, so the check that every
    /// kind was met is a fact of the code, not a draw (proptest's note on `TestRng::deterministic_rng`).
    #[test]
    fn the_core_decides_as_the_oracle() {
        let met = Met::default();
        let bump = |c: &Cell<u64>| c.set(c.get() + 1);
        let config = ProptestConfig::with_cases(512);
        let rng = TestRng::deterministic_rng(config.rng_algorithm);
        let mut runner = TestRunner::new_with_rng(config, rng);
        let cases = (arb_limit(), arb_rates(), arb_series());
        runner
            .run(&cases, |(limit, rates, series)| {
                let mut w = Watch::new(limit, rates);
                let mut peak = 0u64;
                for i in 0..series.len() {
                    let read = w.read(&series[i]).unwrap();
                    prop_assert_eq!(read.verdict, oracle(limit, rates, &series, i), "read {}", i);
                    peak = peak.max(read.footprint);
                    let armed_rule = read.running == 1
                        && series[i]
                            .iter()
                            .filter(|m| m.life == Life::Running)
                            .all(|m| m.armed);
                    match read.verdict {
                        Verdict::Wait(d) => {
                            prop_assert!(d >= FLOOR);
                            bump(if d == FLOOR { &met.floor } else { &met.above });
                            // The headroom bound, bar included, holds for every paced wait but the floor.
                            if d > FLOOR {
                                let ns = d.as_nanos();
                                if let Some(b) = limit.memory {
                                    let room = u128::from(b + OVERSHOOT - read.footprint);
                                    prop_assert!(
                                        ns * u128::from(rates.bytes.get()) <= room * 1_000_000_000
                                    );
                                }
                                if let Some(b) = limit.processes {
                                    let room = u128::from(b - read.processes);
                                    prop_assert!(
                                        ns * u128::from(rates.processes.get())
                                            <= room * 1_000_000_000
                                    );
                                }
                            }
                        }
                        Verdict::Events => {
                            bump(&met.events);
                            if armed_rule {
                                bump(&met.armed_events);
                            }
                        }
                        Verdict::End(Over::Memory { .. }) => bump(&met.memory),
                        Verdict::End(Over::Processes { .. }) => bump(&met.processes),
                        Verdict::Kill => bump(&met.kill),
                        Verdict::Idle => bump(&met.idle),
                    }
                }
                prop_assert_eq!(w.peak(), peak);
                prop_assert_eq!(w.reads(), u64::try_from(series.len()).unwrap());
                Ok(())
            })
            .unwrap();
        for (kind, n) in [
            ("a wait at the floor", &met.floor),
            ("a wait above the floor", &met.above),
            ("a wait for events", &met.events),
            (
                "a wait for events the arming rule decided",
                &met.armed_events,
            ),
            ("an end for memory", &met.memory),
            ("an end for processes", &met.processes),
            ("a kill", &met.kill),
            ("an idle box", &met.idle),
        ] {
            assert!(n.get() > 0, "the series met no {kind}");
        }
    }

    proptest! {
        /// HEX-MAC-4: the wait is the oracle's for any headroom and rate.
        #[test]
        fn the_wait_is_the_oracles(headroom in any::<u64>(), rate in 1u64..u64::MAX) {
            prop_assert_eq!(wait(headroom, NonZeroU64::new(rate).unwrap()), oracle_wait(headroom, rate));
        }
    }
}
