//! The node agent's seeds (decision 5 of docs/design/runtime-architecture.md, goal 5 plan step 3): how many it
//! keeps, which one each create is handed, how long a create waits for one, when a seed that died is started again,
//! and what it counts. Pure: the agent's shell starts each seed (`hex __oci-seed`, exec'd once),
//! hands it a create over its socket, watches it by its pidfd and kills it; this decides which and when, and refuses
//! by type what can't be done. `now` is the shell's monotonic clock: the time since a fixed point it chose, the same
//! for every call.
//!
//! - **The bound**: at most [`Limits::seeds`] seeds are up or starting at once, one to a slot: the node's, derived
//!   from its memory (`hex_node::sizes`). A seed serves one create at a time, so a node keeps several. A growing
//!   pool ([`Seeds::growing`]) starts with fewer slots, the host's parallelism, and adds one for each create that
//!   finds none free, up to the bound (Ada's rule, 2026-11-06).
//! - **A create** is handed an idle seed that is up, never one starting, busy or gone. With none idle it waits in
//!   line ([`Admit::Wait`]), and each seed that frees goes to the create that has waited longest, however long that
//!   is. While [`Limits::waiting`] creates wait, the next is refused at once ([`Refused::Busy`]). Past the line's
//!   learned wait a create waits on, following its seeds by their life alone, and is refused ([`Refused::Waited`])
//!   only when no seed can come. A waiting create claims once more when the shell wakes it or the time its claim gave
//!   it comes ([`Seeds::claim`]), and counts against the line until it does.
//! - **Death**: a seed the shell reports dead leaves its slot, and the create it served is refused
//!   ([`Refused::Died`]). The pool asks for the slot to be started again and counts the restart. A report about a seed
//!   that has left is refused by type ([`Stale::Gone`]), never taken for its slot's next seed's. A seed past its
//!   learned hang bound is never taken for hung ([`Seeds::past_hang`]): it is followed by its life, its death the only
//!   end of its create but its answer.
//! - **Learned waits** (decision 9; Ada's rules of 2026-11-06 and 2026-11-13, no picked constant and no record): the
//!   line's wait and the hang bound are each the slowest answer this pool has seen on `now`'s clock
//!   ([`crate::patience`]), and only rise. Each is a bound only while answers come within it: a wait past it is a
//!   run-out, told to the census and no sample (Karn's point), and goes on, following its party by its life alone, as
//!   at every local site (docs/research/runtime/learned-waits.md: a stall judged at the slowest gap seen refuses
//!   healthy parties): the line's, the busy seeds, until one is handed to it or none can come; the hang bound's, the
//!   seed serving the create, until it answers or dies, its death the pool's [`Seeds::died`]. Before each's first
//!   answer, and from a run-out to the next, it follows its party from its start. A create handed a seed after
//!   waiting, however long, is a sample of the line's wait, a seed done with its create one of the hang bound's; a
//!   create refused in line, and a seed that died, is none.
//! - **Restarts are bounded**: a start that fails, or a seed that dies before it serves a create, stops the starting
//!   until the next create, so a node whose seeds can't start tries each slot once a create, never in a loop
//!   (CLAUDE.md §2). With the starting stopped and no seed up or starting, none can come for the creates in line, and
//!   each is refused then ([`Refused::Waited`]).
//!
//! [`Counts`] is what the bench and the node's metrics read, [`Seeds::handed`] the creates each slot's seeds were
//! handed.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::patience::Patience;

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
    /// it (a seed handed to it names it: [`Seeds::started`], [`Seeds::done`]) or at `until`, the end of its wait
    /// within the line's W*; `None` while it follows its seeds by their life, when it waits for the shell's wake
    /// alone.
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
    /// No seed can come, or the seed handed to it died before it claimed it. The ticket is spent.
    Refused(Refused),
}

/// Why a create is handed no seed, or loses the one it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// No seed is idle and the line is full.
    #[error("no seed is free and {waiting} creates already wait for one")]
    Busy { waiting: usize },
    /// No seed came for it in the `wait` it waited, and none can come: no seed is up or starting, and none is asked
    /// for.
    #[error("no seed came for it in {wait:?}, and none can come")]
    Waited { wait: Duration },
    /// The seed serving it died before it was done.
    #[error("seed {seed} died before it was done with the create")]
    Died { seed: SeedId },
}

/// A report the pool can't take. A shell that reports only what it was handed meets only [`Stale::Gone`], from a
/// create whose seed left while serving it: the pool refused that create when the seed left, so the shell refuses
/// it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Stale {
    /// The seed isn't the pool's now: it died or failed to start (or never was one of its).
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
/// [`Seeds::waiting`]. Every create handed one is done, refused for its seed's death, or served still: `handed` is
/// `done + refused_died` plus [`Seeds::busy`]. A count stops at `u64::MAX`, which at one a nanosecond takes 584 years
/// to reach.
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
    /// Creates refused in line: none could come.
    pub refused_waited: u64,
    /// Creates whose seed died before it was done.
    pub refused_died: u64,
    /// Seeds started and up, the first in their slot or again.
    pub started: u64,
    /// Of those, seeds started in place of one that died.
    pub restarted: u64,
    /// Starts that failed.
    pub start_failed: u64,
    /// Seeds that died.
    pub died: u64,
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
    /// Seed `n`, serving a create since `since`, within the hang bound until `until`: `None` while it is followed by
    /// its life (before the hang bound's first answer, from a run-out to the next, and once past it). `unclaimed` is
    /// the waiting create it was handed to, while that create has yet to claim it.
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
    /// Whether a seed of this slot has been up, so the next one started replaces one that died.
    up_before: bool,
}

/// A create waiting in line for a seed: when it came, and how its wait goes on.
#[derive(Debug, Clone, Copy)]
enum Waiting {
    /// Within the line's W* as it was when the create came: until `until`.
    Within { since: Duration, until: Duration },
    /// Following its seeds by their life alone: past its W*, its run-out told, or from its start where the line had
    /// none.
    Following { since: Duration },
}

impl Waiting {
    fn since(self) -> Duration {
        match self {
            Self::Within { since, .. } | Self::Following { since } => since,
        }
    }

    /// When its shell claims again: the end of its W*; `None` while it follows, when its shell's wake alone does.
    fn wakes(self) -> Option<Duration> {
        match self {
            Self::Within { until, .. } => Some(until),
            Self::Following { .. } => None,
        }
    }
}

/// The node's seeds: what each slot holds, the creates waiting, and what the pool has done. The shell keeps it behind
/// its lock; no call waits.
#[derive(Debug)]
pub struct Seeds {
    limits: Limits,
    slots: Vec<Slot>,
    /// Creates waiting for a seed, by ticket so in the order they came.
    line: BTreeMap<Ticket, Waiting>,
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

    /// For tests alone (see [`Patience::seeded`]): the pool with both its waits as they would be once each had seen
    /// an answer of `slowest`.
    #[cfg(any(test, feature = "test-seeds"))]
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
            wait: Patience::measuring(WAIT),
            hang: Patience::measuring(HANG),
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
    /// wake, or idles. Counted, and as a restart if it replaces one that died. [`Stale`] if the pool isn't
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
        Ok(self.none_can_come(now))
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
                let waiting = match self.wait.deadline() {
                    Some(wait) => Waiting::Within {
                        since: now,
                        until: now.saturating_add(wait),
                    },
                    None => Waiting::Following { since: now },
                };
                self.line.insert(ticket, waiting);
                bump(&mut self.counts.waited);
                Ok(Admit::Wait {
                    ticket,
                    until: waiting.wakes(),
                })
            }
            _ => {
                bump(&mut self.counts.refused_busy);
                Err(Refused::Busy {
                    waiting: self.limits.waiting,
                })
            }
        }
    }

    /// A waiting create claims at `now`, when the shell wakes it or the time its last answer gave it comes: the seed
    /// handed to it, now serving it; word to wait on; or its refusal, which spends the ticket. Within its W* it waits
    /// on. Past it, its run-out is told to the census, once and with no sample, and it waits on, following its seeds by
    /// their life alone: the shell's wake, a seed handed to it or none able to come, ends it. [`Stale::Ticket`] for a
    /// ticket no create holds.
    pub fn claim(&mut self, ticket: Ticket, now: Duration) -> Result<Claim, Stale> {
        if let Some(answer) = self.answered.remove(&ticket) {
            return Ok(match answer {
                Ok(seed) => {
                    // The create holds its seed now: no longer one the seed's death must answer at a claim.
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
        let waiting = self.line.get_mut(&ticket).ok_or(Stale::Ticket(ticket))?;
        match *waiting {
            Waiting::Within { until, .. } if now < until => Ok(Claim::Wait { until: Some(until) }),
            Waiting::Within { since, until } => {
                // Past W*: no sample (Karn's point), but the census is told, once, and the create waits on.
                *waiting = Waiting::Following { since };
                self.wait.ran_out(until.saturating_sub(since));
                Ok(Claim::Wait { until: None })
            }
            Waiting::Following { .. } => Ok(Claim::Wait { until: None }),
        }
    }

    /// Seed `seed` is done with the create it served (it started the container, or refused it and is whole): it
    /// goes to the create that has waited longest, whose ticket is returned for the shell to wake, or idles.
    /// [`Stale::Gone`] if it left meanwhile, dead: the pool refused that create then.
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

    /// Seed `seed` died at `now`: its pidfd says so, or its socket broke. It leaves its slot, counted, and the pool
    /// asks for the slot to be started again. The create it served, if any, is refused and the refusal returned; a
    /// waiting create it was handed to meets the refusal at its claim. A seed that dies before serving a create
    /// (starting, or idle since it started) stops the starting until the next create; with the starting stopped and no
    /// seed left, none can come for the creates in line, and each is refused then, to meet the refusal at its claim.
    /// [`Stale::Gone`] if it had left already.
    pub fn died(&mut self, seed: SeedId, now: Duration) -> Result<Option<Refused>, Stale> {
        let slot = self.slot(seed)?;
        let held = std::mem::replace(&mut slot.holds, Holds::Nothing);
        bump(&mut self.counts.died);
        let refused = match held {
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
        };
        self.none_can_come(now);
        Ok(refused)
    }

    /// The seeds that have served their creates past the hang bound by `now` (decision 9, the person's ruling of
    /// 2026-11-13: W* is a bound only while answers come within it): each one's run-out told to the census, once and
    /// with no sample, and from then followed by its life alone, waited on until it answers or dies, never taken for
    /// hung. How many were told now.
    pub fn past_hang(&mut self, now: Duration) -> usize {
        let mut told = 0usize;
        for slot in &mut self.slots {
            if let Holds::Busy { since, until, .. } = &mut slot.holds
                && let Some(at) = *until
                && at <= now
            {
                self.hang.ran_out(at.saturating_sub(*since));
                *until = None;
                told = told.saturating_add(1);
            }
        }
        told
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

    /// When seed `seed`, serving a create, passes the hang bound: the end of the shell's wait for its answer within
    /// it, `None` while it follows the seed by its life (before the hang bound's first answer, from a run-out to the
    /// next, and past it: [`Seeds::past_hang`]). [`Stale`] for a seed serving none.
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

    /// Seed `seed`, up and free, goes to the create that has waited longest, however long that is, and returns its
    /// ticket; with none waiting, it idles. Its wait is a sample, the line's W* rising to it (decision 9): a create
    /// past its W* that has yet to claim has its run-out told first. A seed freed the instant a wait ends serves it
    /// within W*: its wait is no longer than the slowest answer seen (a create served the instant it joined the line
    /// teaches the line a bound of none, which an answer as quick still meets).
    fn free(&mut self, seed: SeedId, now: Duration, served: bool) -> Option<Ticket> {
        if let Some((ticket, waiting)) = self.line.pop_first() {
            if let Waiting::Within { since, until } = waiting
                && until < now
            {
                self.wait.ran_out(until.saturating_sub(since));
            }
            self.wait.answered(now.saturating_sub(waiting.since()));
            self.hand(seed, now, Some(ticket));
            self.answered.insert(ticket, Ok(seed));
            return Some(ticket);
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

    /// With the starting stopped (a failed start, or a seed dead before it served a create) and no seed up or starting,
    /// none can come for the creates in line: each is refused as its wait ended `now`, rather than left to follow seeds
    /// that are gone (decision 9: a wait that follows its party is refused at the party's end). No seed was slow: none
    /// came, and none will, so the line's wait learns nothing from it. Says whether any create was refused, for the
    /// shell to wake them.
    fn none_can_come(&mut self, now: Duration) -> bool {
        if self.asking
            || self.line.is_empty()
            || self.idle() > 0
            || self.busy() > 0
            || self.starting() > 0
        {
            return false;
        }
        while let Some((ticket, waiting)) = self.line.pop_first() {
            let refused = self.refused_waiting(waiting.since(), now);
            self.answered.insert(ticket, Err(refused));
        }
        true
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
