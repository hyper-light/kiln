//! The node agent's seeds (decision 5 of docs/design/runtime-architecture.md, goal 5 plan step 3): how many it
//! keeps, which one each create is handed, how long a create waits for one, when a seed is taken for hung and
//! started again, and what it counts. Pure: the agent's shell starts each seed (`hex __oci-seed`, exec'd once),
//! hands it a create over its socket, watches it by its pidfd and kills it; this decides which and when, and refuses
//! by type what can't be done. `now` is the shell's monotonic clock: the time since a fixed point it chose, the same
//! for every call.
//!
//! - **The bound**: at most [`Limits::seeds`] seeds are up or starting at once, one to a slot: the node's, derived
//!   from its memory (`hex_node::sizes`). A seed serves one create at a time, so a node keeps several. A growing
//!   pool ([`Seeds::growing`]) starts with fewer slots, the host's parallelism, and adds one for each create that
//!   finds none free, up to the bound (Ada's rule, 2026-11-06).
//! - **A create** is handed an idle seed that is up, never one starting, busy or gone. With none idle it waits in
//!   line ([`Admit::Wait`]), and each seed that frees goes to the create that has waited longest. While
//!   [`Limits::waiting`] creates wait, the next is refused at once ([`Refused::Busy`]); a create handed no seed
//!   within the line's learned wait is refused ([`Refused::Waited`]). A waiting create claims once more when the
//!   shell wakes it or its wait ends ([`Seeds::claim`]), and counts against the line until it does.
//! - **Death and hangs**: a seed the shell reports dead, or one that has served a create for its learned hang bound
//!   (taken for hung by [`Seeds::hung`], for the shell to kill), leaves its slot, and the create it served is refused
//!   ([`Refused::Died`], [`Refused::Hung`]). The pool asks for the slot to be started again and counts the restart.
//!   A report about a seed that has left is refused by type ([`Stale::Gone`]), never taken for its slot's next
//!   seed's.
//! - **Learned waits** (decision 9; Ada's 2026-11-06 rule, the lead's failure-bound rule): the line's wait and the
//!   hang bound are each twice the slowest answer, recorded ([`CLONE_SLOWEST`], before the node has one) or seen by
//!   this pool on `now`'s clock ([`crate::patience`]), and only rise. A create handed a seed after waiting is a
//!   sample of the line's wait, a seed done with its create one of the hang bound's. A create refused at its wait's
//!   end, and a seed that died or was taken for hung, is no sample (Karn's point).
//! - **Restarts are bounded**: a start that fails, or a seed that dies before it serves a create, stops the starting
//!   until the next create, so a node whose seeds can't start tries each slot once a create, never in a loop
//!   (CLAUDE.md §2).
//!
//! [`Counts`] is what the bench and the node's metrics read, [`Seeds::handed`] the creates each slot's seeds were
//! handed.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::patience::Patience;

/// The slowest answer in a seed's recorded distribution on the bench's host class (x86_64 Linux, adacachy2's AMD Ryzen
/// 9 9950X3D2, the record host in fedora's place): a seed's answer to a create, the `seeds.hang` rows of
/// `bench/competitive/probes/waits_census.md` run 4, the largest max of its release arms, 29.565 ms (29,564,846 ns: its
/// tmpfs arm, measured unarmed, of 47,226). It replaces runs 1 and 3's 187.044 ms (fedora's), which replaced
/// seed_path.md run 3's 1.918 ms, a clone timed alone that under the bar's load would have taken healthy seeds for hung.
/// It seeds a pool's hang bound before the node has an answer of its own, and the line's wait,
/// since a create in line waits for a busy seed's answer (decision 9: twice the slowest, recorded or seen). Another
/// host class has no record yet; it starts from this one's and learns its own.
pub const CLONE_SLOWEST: Duration = Duration::from_nanos(29_564_846);

/// The same answer's slowest in a debug build, which a debug build's waits are seeded from (decision 9, by build
/// profile): run 5's largest answer of the debug-build arms, the debug arm and the suites' (adacachy2), for it and the
/// waits seeded from it (the line's, the exec seeds'), never under a bound one ran out at: 652.938 ms (652,937,547 ns,
/// the suites' `seeds.hang`, of 493,529).
pub const CLONE_SLOWEST_DEBUG: Duration = Duration::from_nanos(652_937_547);

/// The line's wait's name, as the waits census records it.
pub const WAIT: &str = "seeds.wait";
/// The hang bound's name, as the waits census records it.
pub const HANG: &str = "seeds.hang";

/// The pool's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most seeds up or starting at once: the pool's slots.
    pub seeds: NonZeroUsize,
    /// The most creates waiting for a seed at once, those answered that have yet to claim among them. The node's is
    /// the most calls its socket runs at once (hex-node's `Sizes::workers`), so no create over the socket is refused
    /// for want of room in line; an in-process caller with more creates waiting is refused past it.
    pub waiting: usize,
}

/// One seed: the slot it fills and its number, which no other seed of the pool has, so a report about a seed that
/// has left its slot is never taken for the next one's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeedId {
    slot: usize,
    n: u64,
}

impl SeedId {
    /// The slot it fills, below [`Limits::seeds`]: where the shell keeps its process.
    pub fn slot(self) -> usize {
        self.slot
    }
}

impl fmt::Display for SeedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} in slot {}", self.n, self.slot)
    }
}

/// A create waiting for a seed, by its place in line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket(u64);

impl fmt::Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a create is answered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// An idle seed, now serving this create alone until the shell reports it [`done`](Seeds::done) or
    /// [`died`](Seeds::died).
    Seed(SeedId),
    /// No seed is idle: the create waits in line and claims with its ticket ([`Seeds::claim`]) when the shell wakes
    /// it (a seed handed to it names it: [`Seeds::started`], [`Seeds::done`]) or at `until`, when its wait ends;
    /// `None` while the line's wait is uncalibrated, when it waits for the shell's wake alone.
    Wait {
        ticket: Ticket,
        until: Option<Duration>,
    },
}

/// What a waiting create's claim is answered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// The seed handed to it, now serving it alone.
    Seed(SeedId),
    /// It waits on, until `until` (`None`: until the shell wakes it).
    Wait { until: Option<Duration> },
    /// It waited past its bound, or the seed handed to it died or hung before it claimed it. The ticket is spent.
    Refused(Refused),
}

/// Why a create is handed no seed, or loses the one it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// No seed is idle and the line is full.
    #[error("no seed is free and {waiting} creates already wait for one")]
    Busy { waiting: usize },
    /// No seed was handed to it within the wait bound.
    #[error("no seed came free within {wait:?}")]
    Waited { wait: Duration },
    /// The seed serving it died before it was done.
    #[error("seed {seed} died before it was done with the create")]
    Died { seed: SeedId },
    /// The seed serving it took the hang bound over it, and was taken for hung, to be killed and replaced.
    #[error("seed {seed} took {hang:?} over the create and was taken for hung")]
    Hung { seed: SeedId, hang: Duration },
}

/// A report the pool can't take. A shell that reports only what it was handed meets only [`Stale::Gone`], from a
/// create whose seed left while serving it: the pool refused that create when the seed left, so the shell refuses
/// it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Stale {
    /// The seed isn't the pool's now: it died, was taken for hung or failed to start (or never was one of its).
    #[error("seed {0} has left the pool")]
    Gone(SeedId),
    /// The seed is the pool's but not as the report needs it to be: `expected` says how.
    #[error("seed {seed} isn't {expected}")]
    State {
        seed: SeedId,
        expected: &'static str,
    },
    /// No create holds the ticket: it was never given, or its claim was answered for good.
    #[error("no create holds ticket {0}")]
    Ticket(Ticket),
}

/// What the pool has done, as the bench and the node's metrics read it. Every create asked is handed a seed, refused
/// at once, refused after waiting, or waits still: `asked` is `handed + refused_busy + refused_waited` plus
/// [`Seeds::waiting`]. Every create handed one is done, refused for its seed's death or hang, or served still:
/// `handed` is `done + refused_died + refused_hung` plus [`Seeds::busy`]. A count stops at `u64::MAX`, which at one
/// a nanosecond takes 584 years to reach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Creates that asked for a seed.
    pub asked: u64,
    /// Creates handed a seed, at once or after waiting.
    pub handed: u64,
    /// Creates that found no seed idle and waited.
    pub waited: u64,
    /// Creates their seed was done with.
    pub done: u64,
    /// Creates refused at once: no seed idle and the line full.
    pub refused_busy: u64,
    /// Creates handed no seed within the wait bound.
    pub refused_waited: u64,
    /// Creates whose seed died before it was done.
    pub refused_died: u64,
    /// Creates whose seed was taken for hung.
    pub refused_hung: u64,
    /// Seeds started and up, the first in their slot or again.
    pub started: u64,
    /// Of those, seeds started in place of one that died or hung.
    pub restarted: u64,
    /// Starts that failed.
    pub start_failed: u64,
    /// Seeds that died.
    pub died: u64,
    /// Seeds taken for hung.
    pub hung: u64,
    /// The most seeds serving creates at once.
    pub most_busy: u64,
}

/// What a slot holds.
#[derive(Debug, Clone, Copy)]
enum Holds {
    /// No seed: none yet, or the last one left.
    Nothing,
    /// Seed `n`, which the shell is starting.
    Starting { n: u64 },
    /// Seed `n`, up and serving no create; `served` says whether it has served one.
    Idle { n: u64, served: bool },
    /// Seed `n`, serving a create since `since` until `until`, when it is taken for hung (`None` while the hang
    /// bound is uncalibrated: the shell's wait for this answer is the calibration read). `unclaimed` is the waiting
    /// create it was handed to, while that create has yet to claim it.
    Busy {
        n: u64,
        since: Duration,
        until: Option<Duration>,
        unclaimed: Option<Ticket>,
    },
}

impl Holds {
    /// The seed held, if any.
    fn seed(self) -> Option<u64> {
        match self {
            Self::Nothing => None,
            Self::Starting { n } | Self::Idle { n, .. } | Self::Busy { n, .. } => Some(n),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    holds: Holds,
    /// Whether a seed of this slot has been up, so the next one started replaces one that died or hung.
    up_before: bool,
}

/// The node's seeds: what each slot holds, the creates waiting, and what the pool has done. The shell keeps it behind
/// its lock; no call waits.
#[derive(Debug)]
pub struct Seeds {
    limits: Limits,
    slots: Vec<Slot>,
    /// Creates waiting for a seed, by ticket so in the order they came, each with when it came and the end of its
    /// wait (`None` while the line's wait is uncalibrated).
    line: BTreeMap<Ticket, (Duration, Option<Duration>)>,
    /// Creates out of line, handed a seed or refused, whose claim is yet to come.
    answered: BTreeMap<Ticket, Result<SeedId, Refused>>,
    /// False from a failed start, or a seed that died before it served a create, to the next create: the bound on
    /// restarts.
    asking: bool,
    next_seed: u64,
    next_ticket: u64,
    counts: Counts,
    /// Creates handed to each slot's seeds.
    handed: Vec<u64>,
    /// The line's wait, learned from the waits that ended in a seed.
    wait: Patience,
    /// The hang bound, learned from the creates seeds were done with.
    hang: Patience,
}

impl Seeds {
    /// A pool with every slot vacant, asking for all of them.
    pub fn new(limits: Limits) -> Self {
        Self::growing(limits, limits.seeds.get())
    }

    /// The pool with both its waits seeded from a record whose slowest answer is `slowest` in place of
    /// [`CLONE_SLOWEST`]: for a host class with a record of its own, or a test whose seeds answer as it chooses.
    pub fn seeded(mut self, slowest: Duration) -> Self {
        self.wait = Patience::seeded(WAIT, slowest);
        self.hang = Patience::seeded(HANG, slowest);
        self
    }

    /// A pool with `start` slots vacant (one at least, [`Limits::seeds`] at most), asking for all of them, that adds
    /// a vacant slot for each create that finds no seed idle and no slot vacant, up to [`Limits::seeds`]: its slots
    /// are the most creates it has seen at once, never fewer than it started with, and never removed, so a burst it
    /// has seen finds a slot for each create again (Ada's rule, 2026-11-06). Its scans walk only the slots it has.
    pub fn growing(limits: Limits, start: usize) -> Self {
        let slots = start.clamp(1, limits.seeds.get());
        Self {
            limits,
            slots: vec![
                Slot {
                    holds: Holds::Nothing,
                    up_before: false,
                };
                slots
            ],
            line: BTreeMap::new(),
            answered: BTreeMap::new(),
            asking: true,
            next_seed: 0,
            next_ticket: 0,
            counts: Counts::default(),
            handed: vec![0; slots],
            wait: Patience::by_profile(WAIT, CLONE_SLOWEST, Some(CLONE_SLOWEST_DEBUG)),
            hang: Patience::by_profile(HANG, CLONE_SLOWEST, Some(CLONE_SLOWEST_DEBUG)),
        }
    }

    /// How many seeds the pool asks its shell to start now: its vacant slots, unless a failed start or a seed that
    /// died before serving a create stopped the asking until the next create.
    pub fn wanted(&self) -> usize {
        if !self.asking {
            return 0;
        }
        self.slots
            .iter()
            .filter(|s| matches!(s.holds, Holds::Nothing))
            .count()
    }

    /// A seed for the shell to start, in a vacant slot, now starting: the shell reports it
    /// [`started`](Self::started) or [`start_failed`](Self::start_failed). `None` while the pool asks for none.
    /// After `u64::MAX` seeds it starts no more: at one a microsecond, 584,000 years.
    #[must_use = "the seed is the shell's to start, or its slot stays starting"]
    pub fn start(&mut self) -> Option<SeedId> {
        if !self.asking {
            return None;
        }
        let slot = self
            .slots
            .iter()
            .position(|s| matches!(s.holds, Holds::Nothing))?;
        let n = self.next_seed;
        self.next_seed = n.checked_add(1)?;
        self.slots.get_mut(slot)?.holds = Holds::Starting { n };
        Some(SeedId { slot, n })
    }

    /// Seed `seed` is up. It goes to the create that has waited longest, whose ticket is returned for the shell to
    /// wake, or idles. Counted, and as a restart if it replaces one that died or hung. [`Stale`] if the pool isn't
    /// starting it (it was reported dead meanwhile): the shell ends it.
    pub fn started(&mut self, seed: SeedId, now: Duration) -> Result<Option<Ticket>, Stale> {
        let slot = self.slot(seed)?;
        if !matches!(slot.holds, Holds::Starting { .. }) {
            return Err(Stale::State {
                seed,
                expected: "starting",
            });
        }
        let restart = slot.up_before;
        slot.up_before = true;
        bump(&mut self.counts.started);
        if restart {
            bump(&mut self.counts.restarted);
        }
        Ok(self.free(seed, now, false))
    }

    /// Seed `seed` failed to start at `now` (the shell says why): its slot is vacant again, counted, and the pool asks
    /// for no start until the next create. With no seed up or starting then, none can come for the creates in line,
    /// so each is refused as its wait ended now, rather than left to wait; says whether any was, for the shell to
    /// wake them.
    pub fn start_failed(&mut self, seed: SeedId, now: Duration) -> Result<bool, Stale> {
        let slot = self.slot(seed)?;
        if !matches!(slot.holds, Holds::Starting { .. }) {
            return Err(Stale::State {
                seed,
                expected: "starting",
            });
        }
        slot.holds = Holds::Nothing;
        bump(&mut self.counts.start_failed);
        self.asking = false;
        if self.idle() > 0 || self.busy() > 0 || self.starting() > 0 || self.line.is_empty() {
            return Ok(false);
        }
        while let Some((ticket, (since, _))) = self.line.pop_first() {
            // No seed was slow: none came, and none will. The line's wait learns nothing from it.
            let refused = self.refused_waiting(since, now);
            self.answered.insert(ticket, Err(refused));
        }
        Ok(true)
    }

    /// A create asks for a seed at `now`: handed an idle one at once, put in line, or, with the line full, refused.
    /// Either way the pool asks again for the starts a failure stopped.
    pub fn create(&mut self, now: Duration) -> Result<Admit, Refused> {
        bump(&mut self.counts.asked);
        self.asking = true;
        let idle = self
            .slots
            .iter()
            .enumerate()
            .find_map(|(slot, s)| match s.holds {
                Holds::Idle { n, .. } => Some(SeedId { slot, n }),
                _ => None,
            });
        if let Some(seed) = idle {
            self.hand(seed, now, None);
            return Ok(Admit::Seed(seed));
        }
        self.grow();
        let room = self.line.len().saturating_add(self.answered.len()) < self.limits.waiting;
        // Past `u64::MAX` waits no ticket is left, and a create is refused as if the line were full: at one a
        // nanosecond, 584 years.
        match (room, self.next_ticket.checked_add(1)) {
            (true, Some(next)) => {
                let ticket = Ticket(self.next_ticket);
                self.next_ticket = next;
                let until = self.wait.deadline().map(|wait| now.saturating_add(wait));
                self.line.insert(ticket, (now, until));
                bump(&mut self.counts.waited);
                Ok(Admit::Wait { ticket, until })
            }
            _ => {
                bump(&mut self.counts.refused_busy);
                Err(Refused::Busy {
                    waiting: self.limits.waiting,
                })
            }
        }
    }

    /// A waiting create claims at `now`, when the shell wakes it or its wait ends: the seed handed to it, now serving
    /// it; word to wait on; or its refusal, which spends the ticket. [`Stale::Ticket`] for a ticket no create holds.
    pub fn claim(&mut self, ticket: Ticket, now: Duration) -> Result<Claim, Stale> {
        if let Some(answer) = self.answered.remove(&ticket) {
            return Ok(match answer {
                Ok(seed) => {
                    // The create holds its seed now: no longer one the seed's death or hang must answer at a claim.
                    if let Some(Slot {
                        holds: Holds::Busy { n, unclaimed, .. },
                        ..
                    }) = self.slots.get_mut(seed.slot)
                        && *n == seed.n
                        && *unclaimed == Some(ticket)
                    {
                        *unclaimed = None;
                    }
                    Claim::Seed(seed)
                }
                Err(refused) => Claim::Refused(refused),
            });
        }
        let (since, until) = *self.line.get(&ticket).ok_or(Stale::Ticket(ticket))?;
        let Some(ended) = until.filter(|&u| u <= now) else {
            return Ok(Claim::Wait { until });
        };
        self.line.remove(&ticket);
        Ok(Claim::Refused(self.waited_out(since, ended)))
    }

    /// Seed `seed` is done with the create it served (it started the container, or refused it and is whole): it
    /// goes to the create that has waited longest, whose ticket is returned for the shell to wake, or idles.
    /// [`Stale::Gone`] if it left meanwhile, dead or taken for hung: the pool refused that create then.
    pub fn done(&mut self, seed: SeedId, now: Duration) -> Result<Option<Ticket>, Stale> {
        let slot = self.slot(seed)?;
        let Holds::Busy {
            since,
            unclaimed: None,
            ..
        } = slot.holds
        else {
            return Err(Stale::State {
                seed,
                expected: "serving a create that holds it",
            });
        };
        bump(&mut self.counts.done);
        // An answer the shell waited for and got: a sample of the hang bound.
        self.hang.answered(now.saturating_sub(since));
        Ok(self.free(seed, now, true))
    }

    /// Seed `seed` died: its pidfd says so, or its socket broke. It leaves its slot, counted, and the pool asks for
    /// the slot to be started again. The create it served, if any, is refused and the refusal returned; a waiting
    /// create it was handed to meets the refusal at its claim. A seed that dies before serving a create (starting,
    /// or idle since it started) stops the starting until the next create. [`Stale::Gone`] if it had left already.
    pub fn died(&mut self, seed: SeedId) -> Result<Option<Refused>, Stale> {
        let slot = self.slot(seed)?;
        let held = std::mem::replace(&mut slot.holds, Holds::Nothing);
        bump(&mut self.counts.died);
        Ok(match held {
            Holds::Busy { unclaimed, .. } => {
                let refused = Refused::Died { seed };
                bump(&mut self.counts.refused_died);
                if let Some(ticket) = unclaimed {
                    self.answered.insert(ticket, Err(refused));
                }
                Some(refused)
            }
            Holds::Starting { .. } | Holds::Idle { served: false, .. } => {
                self.asking = false;
                None
            }
            Holds::Idle { served: true, .. } | Holds::Nothing => None,
        })
    }

    /// The next seed that has served its create for the hang bound by `now`, taken for hung: it leaves its slot,
    /// counted, and the pool asks for the slot to be started again. Returned with its create's refusal, for the shell
    /// to kill it; a waiting create it was handed to meets the refusal at its claim, and a later report about the
    /// seed is [`Stale::Gone`]. `None` once no seed is past the bound, so a shell asks at most once a slot and once
    /// more.
    #[must_use = "a seed taken for hung is the shell's to kill"]
    pub fn hung(&mut self, now: Duration) -> Option<(SeedId, Refused)> {
        let (seed, since, until, unclaimed) =
            self.slots
                .iter()
                .enumerate()
                .find_map(|(slot, s)| match s.holds {
                    Holds::Busy {
                        n,
                        since,
                        until,
                        unclaimed,
                    } => until
                        .filter(|&u| u <= now)
                        .map(|u| (SeedId { slot, n }, since, u, unclaimed)),
                    _ => None,
                })?;
        self.slots.get_mut(seed.slot)?.holds = Holds::Nothing;
        // No sample: the answer never came (Karn's point).
        let refused = Refused::Hung {
            seed,
            hang: until.saturating_sub(since),
        };
        bump(&mut self.counts.hung);
        bump(&mut self.counts.refused_hung);
        if let Some(ticket) = unclaimed {
            self.answered.insert(ticket, Err(refused));
        }
        Some((seed, refused))
    }

    /// A create found no seed idle: a vacant slot is added for a seed to start, unless one is vacant already or the
    /// slots are at the bound.
    fn grow(&mut self) {
        let vacant = self.slots.iter().any(|s| matches!(s.holds, Holds::Nothing));
        if !vacant && self.slots.len() < self.limits.seeds.get() {
            self.slots.push(Slot {
                holds: Holds::Nothing,
                up_before: false,
            });
            self.handed.push(0);
        }
    }

    /// How many slots it has: the most creates it has seen at once, for a growing pool.
    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// How many seeds are up and serve no create.
    pub fn idle(&self) -> usize {
        self.count(|h| matches!(h, Holds::Idle { .. }))
    }

    /// How many seeds serve a create, those handed to a waiting create that has yet to claim among them.
    pub fn busy(&self) -> usize {
        self.count(|h| matches!(h, Holds::Busy { .. }))
    }

    /// How many seeds the shell is starting.
    pub fn starting(&self) -> usize {
        self.count(|h| matches!(h, Holds::Starting { .. }))
    }

    /// How many creates wait in line.
    pub fn waiting(&self) -> usize {
        self.line.len()
    }

    /// What the pool has done.
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The creates each slot's seeds were handed, by slot.
    pub fn handed(&self) -> &[u64] {
        &self.handed
    }

    /// When seed `seed`, serving a create, is taken for hung: the end of the shell's wait for its answer, `None`
    /// while the hang bound is uncalibrated (that wait is its calibration read). [`Stale`] for a seed serving none.
    pub fn until(&self, seed: SeedId) -> Result<Option<Duration>, Stale> {
        match self.slots.get(seed.slot).map(|s| s.holds) {
            Some(Holds::Busy { n, until, .. }) if n == seed.n => Ok(until),
            _ => Err(Stale::State {
                seed,
                expected: "serving a create",
            }),
        }
    }

    /// The line's wait and the hang bound, as learned now (`None` before each's calibration): what the node's
    /// metrics read.
    pub fn deadlines(&self) -> (Option<Duration>, Option<Duration>) {
        (self.wait.deadline(), self.hang.deadline())
    }

    /// The slot `seed` fills, or [`Stale::Gone`] if it has left it (or never was the pool's).
    fn slot(&mut self, seed: SeedId) -> Result<&mut Slot, Stale> {
        self.slots
            .get_mut(seed.slot)
            .filter(|s| s.holds.seed() == Some(seed.n))
            .ok_or(Stale::Gone(seed))
    }

    /// Seed `seed`, up and free, goes to the create that has waited longest, refusing on the way those whose wait
    /// ended by `now`, and returns its ticket; with none left waiting, it idles. Each turn takes a create out of line,
    /// so this ends within the line's length.
    fn free(&mut self, seed: SeedId, now: Duration, served: bool) -> Option<Ticket> {
        while let Some((ticket, (since, until))) = self.line.pop_first() {
            if until.is_none_or(|u| now < u) {
                self.wait.answered(now.saturating_sub(since));
                self.hand(seed, now, Some(ticket));
                self.answered.insert(ticket, Ok(seed));
                return Some(ticket);
            }
            let refused = self.waited_out(since, until.unwrap_or(now));
            self.answered.insert(ticket, Err(refused));
        }
        if let Some(slot) = self.slots.get_mut(seed.slot) {
            slot.holds = Holds::Idle { n: seed.n, served };
        }
        None
    }

    /// Seed `seed` serves a create from `now`, the waiting create `unclaimed` if it was one: counted.
    fn hand(&mut self, seed: SeedId, now: Duration, unclaimed: Option<Ticket>) {
        if let Some(slot) = self.slots.get_mut(seed.slot) {
            slot.holds = Holds::Busy {
                n: seed.n,
                since: now,
                until: self.hang.deadline().map(|hang| now.saturating_add(hang)),
                unclaimed,
            };
        }
        bump(&mut self.counts.handed);
        if let Some(handed) = self.handed.get_mut(seed.slot) {
            bump(handed);
        }
        let busy = u64::try_from(self.busy()).unwrap_or(u64::MAX);
        self.counts.most_busy = self.counts.most_busy.max(busy);
    }

    /// A create in line since `since` whose wait ran out at its bound, `until`, with no seed: refused, and no sample
    /// (Karn's point), but the line's wait backs off past that bound and the census is told (busy seeds, not hung or
    /// failed ones: a seed past its hang bound is [`Seeds::hung`]'s and one that failed to start
    /// [`Seeds::start_failed`]'s, neither of which says anything of an answer's time).
    fn waited_out(&mut self, since: Duration, until: Duration) -> Refused {
        self.wait.ran_out(until.saturating_sub(since));
        self.refused_waiting(since, until)
    }

    /// A create in line since `since`, refused at `until` with no seed: counted, its wait the time it waited.
    fn refused_waiting(&mut self, since: Duration, until: Duration) -> Refused {
        bump(&mut self.counts.refused_waited);
        Refused::Waited {
            wait: until.saturating_sub(since),
        }
    }

    fn count(&self, holds: impl Fn(Holds) -> bool) -> usize {
        self.slots.iter().filter(|s| holds(s.holds)).count()
    }
}

/// Adds one to a count, which stops at `u64::MAX`.
fn bump(count: &mut u64) {
    *count = count.saturating_add(1);
}

#[cfg(test)]
mod tests;
