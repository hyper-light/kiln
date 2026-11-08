//! The owner's side (docs/lb.md "The owner"): the node that asks for a run owns it until its
//! runner says it started, waits or never can, and decides where it goes.
//!
//! Its view of a runner is that runner's last word less what the owner has sent it since and not
//! heard back about (in flight): it deducts what it sends at once, so starts in flight never herd
//! onto one runner. The pick is the next in turn of the highest bucket of the signature's
//! [`Index`]: the member with the most room by the rank key (free whole CPUs until runners measure
//! their speed; docs/lb.md "Rank"), every dimension checked exactly at the view (W05-R23's "least
//! loaded, then most free"). That is O(1); when that member is short of what's asked, the pick
//! walks on, member by member, and counts it ([`Counts::walked`]). Equals take turns from a cursor
//! each owner starts at its own place, so owners sharing runners don't all pick the same one.
//!
//! Off that path: a run no member has room for waits at runners, not here (fair share lives where
//! the contention is): at the two members nearest to fitting, the first to hold its room wins and
//! the other is withdrawn, so it never starts twice. A runner's decline carries its room, so the
//! next pick never repeats the miss, and the loop ends when no member fits (then the run waits):
//! it is bounded by the members. A link that drops leaves the owner unsure of what it had there
//! until the runner says it all again (Synced, with how many of its runs have ended there); a
//! runner that started again (a newer epoch in its word) has lost every run it had.

use super::index::{BUCKETS, Index};
use super::msg::{self, Brief, NO_FAST, Of, Start, ToOwner, ToRunner, Why, Word};
use super::slab::{Handle, Slab};
use super::weights::{FellBack, Weights};
use super::{Amounts, CPU, NIL, OwnerId, RunnerId, SigId, multi, narrow, rest, rest_fits};

/// How many runners a run that finds no room waits at: two. With two choices the most loaded of
/// n bins holds log log n / ln 2 + O(1) instead of Θ(log n / log log n), and each choice past the
/// second only divides the first term by log d (Azar, Broder, Karlin and Upfal, "Balanced
/// Allocations", SIAM J. Comput. 29(1), 1999); for queues, the second choice is where the
/// exponential gain is (Mitzenmacher, "The Power of Two Choices in Randomized Load Balancing",
/// IEEE TPDS 12(10), 2001); and Sparrow measured two probes per task with late binding, the first
/// worker free taking it, as its best trade of messages for waiting (Ousterhout et al., SOSP 2013).
pub const PLACES: usize = 2;

/// The rank key's multiplier at the reference speed: ceil(2^40 / CPU), so `(view * SCALE) >> 40`
/// is `view / CPU`, whole free CPUs, for every u32 view (the excess, view * 0.224 / 2^40, stays
/// under the 1/1000 a view's fraction keeps below the next whole CPU): a multiply and a shift, no
/// divide. A runner's measured per-core speed scales it (core-equivalents; docs/lb.md "Rank").
pub const SCALE: u32 = 1_099_511_628;

/// A runner's speed in its word is in 1/1024 of the reference core's (0: not measured).
pub const SPEED_ONE: u32 = 1024;

/// Spreads owners' cursors over the fleet: floor(2^32 / φ), Fibonacci hashing (Knuth, TAOCP vol.
/// 3, §6.4: successive multiples of it modulo 2^32 fall as evenly as any sequence can).
const SPREAD: u32 = 2_654_435_769;

// A runner's flags at its owner.
const KNOWN: u32 = 1;
/// Its link is up and synced: it is in its indexes' buckets.
const UP: u32 = 2;
/// Its last word had all of its memory, disk and GPUs free.
const WHOLE: u32 = 4;
/// It is a member of more than one index.
const MORE: u32 = 8;
/// Its rise is in the owner's put-off list ([`Owner::ended_batch`]).
const PUT_OFF: u32 = 16;

// A run's states at its owner.
const STARTING: u32 = 1;
const STARTED: u32 = 2;
const WAITING: u32 = 3;
const GOING: u32 = 4;
const STATE: u32 = 7;
/// Its runner's link dropped while it was starting, started or going there: what became of it
/// waits for the runner's Synced.
const UNSURE: u32 = 8;
/// Its asker gave up while its start or Go was in flight: told when it starts (the shell ends
/// it there), forgotten if it doesn't, never placed again.
const CANCELLED: u32 = 16;

/// What the owner keeps of each runner, all that a decision or a word reads of it: 32 bytes,
/// aligned, so four share one of Apple silicon's 128-byte cache lines (two an x86 one) and none
/// straddles two. (16 would mean narrowing the counts that must not wrap.)
#[derive(Debug, Clone, Copy)]
#[repr(C, align(32))]
struct Peer {
    /// CPU free as this owner sees it, in thousandths: its last word less `inflight`.
    view: u32,
    /// CPU of the starts sent to it and not heard back about.
    inflight: u32,
    /// Its epoch (the low half) and what a word's stamp must say for the fast paths to take it
    /// ([`msg::stamp`]: its speed when it is known, up and whole; [`NO_FAST`] when it isn't).
    stamp: u64,
    /// How many of this owner's runs it has said ended, this epoch.
    heard: u32,
    /// Its rank key's multiplier ([`SCALE`] at the reference speed).
    scale: u32,
    /// The first index it's a member of (NIL: none); the rest are in `more`.
    sig0: u32,
    flags: u32,
}

impl Peer {
    #[inline(always)]
    fn epoch(&self) -> u32 {
        self.stamp as u32
    }
}

const PEER: Peer = Peer { view: 0, inflight: 0, stamp: (NO_FAST as u64) << 32, heard: 0, scale: SCALE, sig0: NIL, flags: 0 };

/// The runner whose room last rose and that hasn't moved in its buckets for it yet: its view is
/// above its bucket by a move the next pick may undo (`NIL`: none). A run that ends and a pick
/// that goes back to the runner it ended on are the commonest pair of steps (W05-R23's least
/// loaded is then that runner), and when the pick is the only thing to read the buckets between
/// them, the move and its opposite are both left undone: the buckets are written when anything
/// else is about to read them ([`Owner::flush`]).
#[derive(Debug, Clone, Copy)]
struct Pend {
    r: u32,
    /// The bucket it's in.
    from: u32,
    /// The bucket its view says.
    to: u32,
}

const NO_PEND: Pend = Pend { r: NIL, from: 0, to: 0 };

/// In [`Pend::r`]: this owner shares its fleet with others and spreads its picks (no move is
/// ever put off then). `NIL - 1`, so the fast paths' test for a runner pending is the one
/// they make anyway (no runner has this number).
const WIDE: u32 = NIL - 1;

/// In [`Pend::r`]: a batch's rises are put off in [`Owner::put_off`] (several runners, so no one
/// of them is "the" move put off). Below `WIDE`, for the same test.
const MANY: u32 = NIL - 2;

/// What the owner keeps of each runner that only a need of memory, disk or GPUs reads.
#[derive(Debug, Clone, Copy, Default)]
struct Cold {
    budget: Amounts,
    /// Memory, disk and GPUs free by its last word (when they weren't all free).
    word_rest: [u64; 3],
    inflight_rest: [u64; 3],
    /// Its per-core speed as its word last said it (0: not measured).
    speed: u32,
    /// Per-core slowness under contention as its word last said it: narrow asks' order (not built).
    slowness: u32,
}

/// A run, 32 bytes. What a run needs beyond CPU, and where it waits, are in its [`Slow`] record.
#[derive(Debug, Clone, Copy)]
struct Run {
    cpu: u64,
    /// Its handle at its runner: an admitted run's there, or the room held for it.
    at: Handle,
    sig: u32,
    runner: u32,
    state: u32,
    /// Its slow record's slot (NIL: none).
    slow: u32,
}

/// A place a run waits at: the runner, its handle in that runner's line once the runner has said
/// it (`Handle::NONE` until then), and the runner's epoch its Wait went under (a Wait the runner
/// fenced was sent under another, so its decline isn't about a place made since).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Place {
    runner: u32,
    at: Handle,
    epoch: u32,
}

const NO_PLACE: Place = Place { runner: NIL, at: Handle::NONE, epoch: 0 };

/// What a run needs of memory, disk and GPUs, and where it waits.
#[derive(Debug, Clone, Copy)]
struct Slow {
    rest: [u64; 3],
    places: [Place; PLACES],
    /// Its neighbours in its signature's waiting runs (run slots), while it is in them.
    prev: u32,
    next: u32,
    listed: bool,
}

/// One signature: its index, and its runs waiting for room, oldest first.
#[derive(Debug, Clone)]
struct Sig {
    ix: Index,
    wait_head: u32,
    wait_tail: u32,
    waiting: u32,
}

/// What an owner tells the askers of its runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Told {
    /// It started on that runner, by that runner's word.
    Started { run: Handle, runner: RunnerId },
    /// No member has room for it now: it waits for room at the runners its owner chose, or for
    /// a member to come back.
    Waiting { run: Handle },
    /// No member could ever take it: it needs more than any member's whole budget.
    Never { run: Handle },
    /// It ended on its runner.
    Ended { run: Handle },
    /// It is gone, and may or may not have run: its runner started again or left while it ran
    /// there, or while what it said of it was lost. Never run twice: the asker decides.
    Lost { run: Handle },
}

/// A run's state at its owner, as tests and explanations read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Its start went to `runner`, unanswered.
    Starting { runner: RunnerId },
    /// It runs on `runner` (`at` there).
    Started { runner: RunnerId, at: Handle },
    /// It waits at these runners (none: no member is reachable).
    Waiting { at: [Option<RunnerId>; PLACES] },
    /// `runner` holds its room, and the owner said Go.
    Going { runner: RunnerId },
}

/// Why an ask wasn't taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// No such signature at this owner.
    NoSig,
    /// No member could ever take it: it needs more than any member's whole budget.
    Never,
    /// The owner holds 2^32 - 1 runs already.
    Full,
}

/// An ask taken: its run's handle, and the start to send when a member has room for it now
/// (`None`: it waits, and what to send for that is in the [`Out`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asked {
    pub run: Handle,
    pub send: Option<(RunnerId, ToRunner)>,
}

/// What an owner has to send and to tell, beyond what a call returns.
#[derive(Debug, Clone, Default)]
pub struct Out {
    pub to: Vec<(RunnerId, ToRunner)>,
    pub told: Vec<Told>,
}

impl Out {
    pub fn clear(&mut self) {
        self.to.clear();
        self.told.clear();
    }
}

/// What happened off the fast path, counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Members a pick passed over: one short of what's asked, or in a bucket whose key didn't
    /// promise the CPU asked.
    pub walked: u64,
    /// Starts and waits runners declined.
    pub declined: u64,
    /// Runs sent to wait.
    pub waited: u64,
    /// Words ignored: from an epoch older than the one known, or about no run of this owner's.
    pub stale: u64,
}

/// The owner: its runners, signatures and runs.
#[derive(Debug, Clone)]
pub struct Owner {
    id: OwnerId,
    peers: Vec<Peer>,
    cold: Vec<Cold>,
    /// Each runner's indexes past its first.
    more: Vec<Vec<u32>>,
    sigs: Vec<Sig>,
    runs: Slab<Run>,
    slow: Slab<Slow>,
    /// How many buckets each index has: the most whole CPUs any runner has, plus one, at most
    /// [`BUCKETS`].
    buckets: u32,
    /// The highest bucket: `buckets - 1` (kept: every key on the fast paths reads it).
    last: u32,
    /// Runs waiting, over every signature.
    waiting: u32,
    /// Some runner's key is scaled by a measured speed: bucket lower bounds don't promise CPUs.
    scaled: bool,
    /// A batch's members and the buckets they were taken from, kept to reuse.
    batch: Vec<(u32, u32)>,
    /// The runner whose room rose and whose bucket hasn't moved for it yet.
    pend: Pend,
    /// A batch's runners whose rises are put off and the buckets they're in (`MANY` in `pend`).
    put_off: Vec<(u32, u32)>,
    /// Candidates a pick chooses among when other owners decide over this fleet: 1 is the exact
    /// best (one owner), the rest the expected herd (docs/lb.md "Several owners").
    width: u32,
    /// The generator a wide pick draws from: passed in by [`Owner::set_spread`], advanced one
    /// step a wide pick (SplitMix64: Steele, Lea, Flood, OOPSLA 2014).
    rng: u64,
    pub counts: Counts,
    /// Room tokens' owner side ([`super::tokens`]): waiting places drawn, tokens given back.
    draws: Draws,
    /// Learned wait weights ([`super::weights`]): off until set.
    weights: Weights,
}

/// Room tokens at the owner: whether its waiting places are drawn from the signature and the
/// tokens it can't use given back, and the generator the draws take.
#[derive(Debug, Clone, Copy, Default)]
struct Draws {
    on: bool,
    rng: u64,
}

/// The bucket of a view: its key, at most `last`.
#[inline(always)]
fn key(view: u32, scale: u32, last: u32) -> u32 {
    (((u64::from(view) * u64::from(scale)) >> 40) as u32).min(last)
}

impl Owner {
    pub fn new(id: OwnerId) -> Owner {
        Owner { id, peers: Vec::new(), cold: Vec::new(), more: Vec::new(), sigs: Vec::new(), runs: Slab::new(), slow: Slab::new(), buckets: 1, last: 0, waiting: 0, scaled: false, batch: Vec::new(), pend: NO_PEND, put_off: Vec::new(), width: 1, rng: 0, counts: Counts::default(), draws: Draws::default(), weights: Weights::default() }
    }

    pub fn id(&self) -> OwnerId {
        self.id
    }

    /// Room for `runs` runs before its tables grow (none grows on a decision after that).
    pub fn reserve(&mut self, runs: usize) {
        self.runs = Slab::with_capacity(runs);
    }

    #[inline(always)]
    fn last(&self) -> u32 {
        self.last
    }

    /// Works out what a word's stamp must say to take the fast paths: from its flags and epoch
    /// and, in the [`Cold`] record, its speed. Call it when any of them change.
    fn restamp(&mut self, r: u32) {
        let speed = self.cold.get(r as usize).map_or(0, |c| c.speed);
        if let Some(p) = self.peers.get_mut(r as usize) {
            let tag = if p.flags & (KNOWN | UP | WHOLE) == KNOWN | UP | WHOLE && speed < NO_FAST { speed } else { NO_FAST };
            p.stamp = u64::from(p.epoch()) | u64::from(tag) << 32;
        }
    }

    /// Takes runner `r` into its fleet with its whole budget and its first word. A runner it knew
    /// already leaves first (its runs there are lost).
    pub fn join(&mut self, r: RunnerId, budget: Amounts, word: Word, out: &mut Out) {
        self.flush();
        let i = r.0 as usize;
        if self.peers.get(i).is_some_and(|p| p.flags & KNOWN != 0) {
            self.leave(r, out);
        }
        if self.peers.len() <= i {
            self.peers.resize(i + 1, PEER);
            self.cold.resize(i + 1, Cold::default());
            self.more.resize(i + 1, Vec::new());
        }
        let wanted = u32::try_from((budget[0] / CPU).saturating_add(1)).unwrap_or(BUCKETS).min(BUCKETS);
        self.buckets = self.buckets.max(wanted);
        self.last = self.buckets.saturating_sub(1);
        let (buckets, runners) = (self.buckets, u32::try_from(self.peers.len()).unwrap_or(NIL));
        for s in &mut self.sigs {
            s.ix.grow(buckets, runners);
        }
        let scale = Self::scale_of(word.speed);
        self.scaled |= scale != SCALE;
        if let Some(p) = self.peers.get_mut(i) {
            *p = Peer { view: narrow(word.cpu), inflight: 0, stamp: u64::from(word.epoch), heard: 0, scale, sig0: NIL, flags: KNOWN | UP | if word.whole { WHOLE } else { 0 } };
        }
        if let Some(c) = self.cold.get_mut(i) {
            *c = Cold { budget, word_rest: word.rest, inflight_rest: [0; 3], speed: word.speed, slowness: word.slowness };
        }
        self.restamp(r.0);
    }

    /// A runner's key multiplier at its measured per-core speed: core-equivalents. Speeds past
    /// what a u32 multiplier holds (3.9 times the reference) are taken as that.
    fn scale_of(speed: u32) -> u32 {
        if speed == 0 {
            return SCALE;
        }
        u32::try_from(u64::from(SCALE) * u64::from(speed) / u64::from(SPEED_ONE)).unwrap_or(u32::MAX)
    }

    /// A new signature, with no members yet. Its cursors start at this owner's place in the fleet.
    pub fn sig(&mut self) -> SigId {
        let id = u32::try_from(self.sigs.len()).unwrap_or(NIL);
        #[allow(unused_mut)]
        let mut start = self.id.0.wrapping_mul(SPREAD).wrapping_add(id.wrapping_mul(SPREAD.rotate_left(16)));
        #[cfg(test)]
        if super::planted(super::Mutant::NoSpread) {
            start = 0;
        }
        let mut ix = Index::new(start);
        ix.grow(self.buckets, u32::try_from(self.peers.len()).unwrap_or(NIL));
        self.sigs.push(Sig { ix, wait_head: NIL, wait_tail: NIL, waiting: 0 });
        SigId(id)
    }

    /// Runner `r` meets signature `sig`'s constraints: a member from now on. Runs waiting for it
    /// are tried on it.
    pub fn add(&mut self, sig: SigId, r: RunnerId, out: &mut Out) {
        self.flush();
        let ri = r.0;
        let Some(p) = self.peers.get(ri as usize).copied().filter(|p| p.flags & KNOWN != 0) else { return };
        let budget = self.cold.get(ri as usize).map_or([0; 4], |c| c.budget);
        let last = self.last();
        let Some(s) = self.sigs.get_mut(sig.0 as usize) else { return };
        if !s.ix.join(ri, &budget) {
            return;
        }
        if p.flags & UP != 0 {
            s.ix.push(ri, key(p.view, p.scale, last));
        }
        if p.sig0 == NIL {
            if let Some(p) = self.peers.get_mut(ri as usize) {
                p.sig0 = sig.0;
            }
        } else {
            if let Some(m) = self.more.get_mut(ri as usize) {
                m.push(sig.0);
            }
            if let Some(p) = self.peers.get_mut(ri as usize) {
                p.flags |= MORE;
            }
        }
        if self.waiting != 0 {
            self.retry(sig.0, out);
            self.unpark(ri, out);
        }
    }

    /// Runner `r` no longer meets signature `sig`'s constraints.
    pub fn remove(&mut self, sig: SigId, r: RunnerId) {
        self.flush();
        let ri = r.0;
        let Some(p) = self.peers.get(ri as usize).copied() else { return };
        let budget = self.cold.get(ri as usize).map_or([0; 4], |c| c.budget);
        let last = self.last();
        let Some(s) = self.sigs.get_mut(sig.0 as usize) else { return };
        if !s.ix.contains(ri) {
            return;
        }
        if p.flags & UP != 0 {
            s.ix.unlink(ri, key(p.view, p.scale, last));
        }
        s.ix.part(ri, &budget);
        let more = self.more.get_mut(ri as usize);
        if p.sig0 == sig.0 {
            // Its next index becomes its first.
            let next = more.and_then(|m| (!m.is_empty()).then(|| m.remove(0)));
            if let Some(p) = self.peers.get_mut(ri as usize) {
                p.sig0 = next.unwrap_or(NIL);
            }
        } else if let Some(m) = more {
            m.retain(|s| *s != sig.0);
        }
        let more_left = self.more.get(ri as usize).is_some_and(|m| !m.is_empty());
        if let Some(p) = self.peers.get_mut(ri as usize) {
            if more_left {
                p.flags |= MORE;
            } else {
                p.flags &= !MORE;
            }
        }
    }

    /// Runner `r` is gone: out of every index, its runs lost, and the runs waiting there placed
    /// again.
    pub fn leave(&mut self, r: RunnerId, out: &mut Out) {
        self.flush();
        let ri = r.0;
        let Some(p) = self.peers.get(ri as usize).copied().filter(|p| p.flags & KNOWN != 0) else { return };
        for s in self.sigs_of(ri, &p) {
            self.remove(SigId(s), r);
        }
        if let Some(p) = self.peers.get_mut(ri as usize) {
            *p = PEER;
        }
        let mut again = Vec::new();
        for h in self.runs.handles() {
            let Some(x) = self.runs.get(h).copied() else { continue };
            match x.state & STATE {
                STARTED if x.runner == ri => {
                    self.free_run(h);
                    out.told.push(Told::Lost { run: h });
                }
                STARTING | GOING if x.runner == ri => {
                    self.park(h);
                    again.push(h);
                }
                WAITING if self.drop_place(x.slow, ri) == Some(0) => again.push(h),
                _ => {}
            }
        }
        for h in again {
            self.place(h, out);
        }
    }

    /// The indexes runner `r` is a member of.
    fn sigs_of(&self, r: u32, p: &Peer) -> Vec<u32> {
        std::iter::once(p.sig0).chain(self.more.get(r as usize).cloned().unwrap_or_default()).filter(|s| *s != NIL).collect()
    }

    /// Decides where a run needing `need` of signature `sig`'s members goes: the start to send
    /// when one has room for it now, else it waits (what to send for that is in `out`). Refused
    /// when no member could ever take it.
    #[inline(always)]
    pub fn ask(&mut self, sig: SigId, need: Amounts, out: &mut Out) -> Result<Asked, Refused> {
        if !multi(&need)
            && let Ok(cpu) = u32::try_from(need[0])
            && let Some((r, run, epoch)) = self.ask_fast(sig, cpu)
        {
            return Ok(Asked { run, send: Some((RunnerId(r), ToRunner::Start(Start { owner: self.id, run, epoch, need }))) });
        }
        self.ask_general(sig, need, out)
    }

    /// An ask of CPU alone on the fast path: the top bucket's next in turn has the room, in that
    /// one index. It is in that bucket by the index's invariant, so its move starts there. The
    /// runner, the run's handle and the epoch to send under; `None`: nothing done,
    /// [`Owner::ask`]'s general path decides.
    #[inline(always)]
    pub fn ask_fast(&mut self, sig: SigId, cpu: u32) -> Option<(u32, Handle, u32)> {
        #[cfg(test)]
        if super::any_planted() {
            return None;
        }
        let si = sig.0;
        let last = self.last();
        let Pend { r: pr, from, to: kp } = self.pend;
        if pr < MANY {
            let s = self.sigs.get_mut(si as usize)?;
            let p = self.peers.get_mut(pr as usize)?;
            if p.view >= cpu && s.ix.none_from(kp) && (p.sig0 == si || s.ix.contains(pr)) {
                // It alone is the most free: the pick goes back to it, and it needn't have moved.
                let run = self.runs.insert(Run { cpu: u64::from(cpu), at: Handle::NONE, sig: si, runner: pr, state: STARTING, slow: NIL })?;
                s.ix.took(kp, pr);
                p.view -= cpu;
                p.inflight = p.inflight.saturating_add(cpu);
                let (to, epoch) = (key(p.view, p.scale, last), p.epoch());
                self.pend.r = NIL;
                if to != from {
                    self.resettle(pr, from, to);
                }
                return Some((pr, run, epoch));
            }
        }
        self.ask_other(si, cpu)
    }

    /// Every pick the put-off move didn't decide, the fast entry's one call off its path (so its
    /// path holds only what it uses): an owner that spreads draws among the top buckets; the
    /// exact best is the top bucket's next in turn.
    #[inline(never)]
    fn ask_other(&mut self, si: u32, cpu: u32) -> Option<(u32, Handle, u32)> {
        if self.pend.r == WIDE {
            return self.ask_wide(si, cpu);
        }
        self.flush();
        self.ask_top(si, cpu)
    }

    /// Other owners decide over this fleet from views a word's delay old (Mitzenmacher, "How
    /// Useful Is Old Information?", IEEE TPDS 11(1), 2000: picking the apparent best herds; a
    /// small random choice doesn't). `others_asks_per_s` is the rate other owners decide at (its
    /// measure, passed in) and `rtt_ns` a word's delay; the expected herd is the decisions they
    /// make in that delay, and a pick then chooses uniformly among that many candidates more
    /// than the exact best, from the top buckets down. `seed` starts its generator. Rates of 0
    /// restore the exact best (and the put-off move, for the next ask).
    pub fn set_spread(&mut self, others_asks_per_s: u64, rtt_ns: u64, seed: u64) {
        self.flush();
        let herd = u128::from(others_asks_per_s).saturating_mul(u128::from(rtt_ns)) / 1_000_000_000;
        self.width = u32::try_from(herd).unwrap_or(u32::MAX).saturating_add(1);
        self.rng = seed;
        self.pend.r = if self.width > 1 { WIDE } else { NIL };
    }

    /// A pick spread over the top buckets (only with [`Owner::set_spread`]): one candidate of
    /// `width`, drawn uniformly, in the order the buckets and the turns within them give.
    #[inline(never)]
    fn ask_wide(&mut self, si: u32, cpu: u32) -> Option<(u32, Handle, u32)> {
        // SplitMix64, one step (Steele, Lea, Flood, OOPSLA 2014); `width` by multiplication,
        // not division (Lemire, "Fast Random Integer Generation in an Interval", TOMACS 2019,
        // less its rejection: a few parts in 2^64 of bias).
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let z = z ^ (z >> 31);
        let s = self.sigs.get_mut(si as usize)?;
        let w = self.width.min(s.ix.len().max(1));
        let mut u = ((u128::from(z) * u128::from(w)) >> 64) as u32;
        let mut b = s.ix.top()?;
        let (r, bucket) = loop {
            let c = s.ix.count(b);
            if u < c {
                break (s.ix.nth_in(b, u), b);
            }
            u -= c;
            let Some(x) = s.ix.below(b) else { return self.ask_top(si, cpu) };
            b = x;
        };
        let Some(p) = self.peers.get_mut(r as usize) else { return self.ask_top(si, cpu) };
        if p.view < cpu || p.flags & MORE != 0 {
            return self.ask_top(si, cpu);
        }
        let run = self.runs.insert(Run { cpu: u64::from(cpu), at: Handle::NONE, sig: si, runner: r, state: STARTING, slow: NIL })?;
        s.ix.took(bucket, r);
        p.view -= cpu;
        p.inflight = p.inflight.saturating_add(cpu);
        let to = key(p.view, p.scale, self.last);
        s.ix.relink(r, bucket, to);
        Some((r, run, p.epoch()))
    }

    /// The pick the put-off move didn't decide: the top bucket's next in turn. Out of line: the
    /// release it follows paid for the rest, and the pick that goes back to its runner is the
    /// hot one.
    #[inline(never)]
    fn ask_top(&mut self, si: u32, cpu: u32) -> Option<(u32, Handle, u32)> {
        let last = self.last();
        let s = self.sigs.get_mut(si as usize)?;
        let top = s.ix.top()?;
        let r = s.ix.next_in(top);
        let p = self.peers.get_mut(r as usize)?;
        if p.view < cpu || p.flags & MORE != 0 {
            return None;
        }
        let run = self.runs.insert(Run { cpu: u64::from(cpu), at: Handle::NONE, sig: si, runner: r, state: STARTING, slow: NIL })?;
        s.ix.took(top, r);
        p.view -= cpu;
        p.inflight = p.inflight.saturating_add(cpu);
        s.ix.relink(r, top, key(p.view, p.scale, last));
        Some((r, run, p.epoch()))
    }

    /// The pick that went back to the runner whose move was put off left it where it wasn't: a
    /// run of another size. Out of line.
    #[cold]
    #[inline(never)]
    fn resettle(&mut self, r: u32, from: u32, to: u32) {
        let Some(p) = self.peers.get(r as usize) else { return };
        let (sig0, more) = (p.sig0, p.flags & MORE != 0);
        self.relink_all(r, sig0, more, from, to);
    }

    /// Moves the runner whose move was put off to the bucket its view says. Everything that
    /// reads or moves buckets, other than the fast paths that know of it, comes here first.
    #[inline(never)]
    fn flush(&mut self) {
        let Pend { r, from, .. } = self.pend;
        if r >= MANY {
            if r == MANY {
                self.apply_put_off();
            }
            return;
        }
        self.pend.r = NIL;
        let last = self.last();
        let Some(p) = self.peers.get(r as usize) else { return };
        let (to, sig0, more) = (key(p.view, p.scale, last), p.sig0, p.flags & MORE != 0);
        if to != from {
            self.relink_all(r, sig0, more, from, to);
        }
    }

    /// A word moved runner `r`'s view from key `from` to key `to`, and neither fast case held:
    /// no runner pending and the view risen, or `r` pending already.
    #[cold]
    #[inline(never)]
    fn moved_slow(&mut self, r: u32, from: u32, to: u32) {
        let Some(p) = self.peers.get(r as usize) else { return };
        let (sig0, more, listed) = (p.sig0, p.flags & MORE != 0, p.flags & PUT_OFF != 0);
        if self.pend.r == WIDE {
            self.relink_all(r, sig0, more, from, to);
            return;
        }
        if self.pend.r == MANY {
            // A batch's put-off rises: applied now, one of them this runner's own if it's there.
            self.apply_put_off();
            if listed {
                return;
            }
        }
        if self.pend.r == r {
            let at = self.pend.from;
            if to > at {
                self.pend.to = to;
                return;
            }
            self.pend.r = NIL;
            if to != at {
                self.relink_all(r, sig0, more, at, to);
            }
        } else if to > from {
            self.flush();
            self.pend = Pend { r, from, to };
        } else {
            self.relink_all(r, sig0, more, from, to);
        }
    }

    /// Every other ask: memory, disk or GPUs, a walk, a runner in several indexes, a wait.
    #[cold]
    #[inline(never)]
    fn ask_general(&mut self, sig: SigId, need: Amounts, out: &mut Out) -> Result<Asked, Refused> {
        self.flush();
        let si = sig.0;
        let cpu_only = !multi(&need);
        let r = self.pick(si, &need, cpu_only);
        if r == NIL {
            return self.ask_waits(si, need, out);
        }
        let slow = if cpu_only { NIL } else { self.slow_for(&need)? };
        let Some(run) = self.runs.insert(Run { cpu: need[0], at: Handle::NONE, sig: si, runner: r, state: STARTING, slow }) else {
            self.free_slow(slow);
            return Err(Refused::Full);
        };
        let epoch = self.deduct(r, &need, cpu_only);
        Ok(Asked { run, send: Some((RunnerId(r), ToRunner::Start(Start { owner: self.id, run, epoch, need }))) })
    }

    #[cold]
    #[inline(never)]
    fn slow_for(&mut self, need: &Amounts) -> Result<u32, Refused> {
        Ok(self.slow.insert(Slow::of(need)).ok_or(Refused::Full)?.slot)
    }

    /// The ask found no member with room now: it waits, or never fits.
    #[cold]
    #[inline(never)]
    fn ask_waits(&mut self, si: u32, need: Amounts, out: &mut Out) -> Result<Asked, Refused> {
        let Some(s) = self.sigs.get(si as usize) else { return Err(Refused::NoSig) };
        if !s.ix.could_fit(&need) {
            return Err(Refused::Never);
        }
        let slow = self.slow_for(&need)?;
        let Some(run) = self.runs.insert(Run { cpu: need[0], at: Handle::NONE, sig: si, runner: NIL, state: WAITING, slow }) else {
            self.free_slow(slow);
            return Err(Refused::Full);
        };
        self.wait(run, out, false);
        Ok(Asked { run, send: None })
    }

    /// Decides `n` runs of the same need at once (a CI push's legs): the same choices as `n` asks
    /// one at a time. For whole CPUs alone, on runners whose keys are whole free CPUs, a member of
    /// bucket b has b CPUs free and goes to bucket b - c, so the index alone decides the batch, in
    /// the order one at a time would; then each member's record is updated in a pass whose steps
    /// touch different runners, so their loads overlap. Anything else: one at a time. The starts
    /// go in `starts`; runs that wait, in `waits` (what to send for them is in `out`).
    pub fn ask_many(&mut self, sig: SigId, need: Amounts, n: usize, starts: &mut Vec<(RunnerId, Start)>, waits: &mut Vec<Handle>, out: &mut Out) -> Result<(), Refused> {
        starts.reserve(n);
        let mut left = n;
        #[cfg(test)]
        let planted = super::any_planted();
        #[cfg(not(test))]
        let planted = false;
        let c = need[0] / CPU;
        // A batch of whole CPUs after a batch of ends: the rises put off go first.
        if self.pend.r == MANY && !multi(&need) && c > 0 && need[0].is_multiple_of(CPU) && !self.scaled && self.buckets < BUCKETS && !planted {
            self.overlay(sig.0, need, &mut left, starts)?;
        }
        self.flush();
        if !multi(&need) && c > 0 && need[0].is_multiple_of(CPU) && !self.scaled && self.buckets < BUCKETS && !planted {
            let step = u32::try_from(c).unwrap_or(u32::MAX);
            let mut batch = std::mem::take(&mut self.batch);
            batch.clear();
            if let Some(s) = self.sigs.get_mut(sig.0 as usize) {
                while left > 0 {
                    let Some(top) = s.ix.top().filter(|t| *t >= step) else { break };
                    let r = s.ix.next_in(top);
                    s.ix.took(top, r);
                    s.ix.relink(r, top, top - step);
                    batch.push((r, top));
                    left -= 1;
                }
            }
            let cpu = narrow(need[0]);
            for &(r, b) in &batch {
                let Some(run) = self.runs.insert(Run { cpu: need[0], at: Handle::NONE, sig: sig.0, runner: r, state: STARTING, slow: NIL }) else { return Err(Refused::Full) };
                let Some(p) = self.peers.get_mut(r as usize) else { continue };
                p.view = p.view.saturating_sub(cpu);
                p.inflight = p.inflight.saturating_add(cpu);
                let (epoch, more) = (p.epoch(), p.flags & MORE != 0);
                if more {
                    self.relink_others(r, sig.0, b, b - step);
                }
                starts.push((RunnerId(r), Start { owner: self.id, run, epoch, need }));
            }
            self.batch = batch;
        }
        for _ in 0..left {
            let a = self.ask(sig, need, out)?;
            match a.send {
                Some((r, ToRunner::Start(s))) => starts.push((r, s)),
                _ => waits.push(a.run),
            }
        }
        Ok(())
    }

    /// The member a need goes to: the next in turn of the highest bucket if it fits (CPU at the
    /// view, every other dimension exactly), else a walk (NIL: none fits).
    #[inline(always)]
    fn pick(&mut self, sig: u32, need: &Amounts, cpu_only: bool) -> u32 {
        let Some(s) = self.sigs.get(sig as usize) else { return NIL };
        let Some(top) = s.ix.top() else { return NIL };
        #[cfg(test)]
        if super::planted(super::Mutant::MostLoaded) || super::planted(super::Mutant::Unordered) {
            return self.pick_mutant(sig, need);
        }
        let r = s.ix.next_in(top);
        let fits = self.peers.get(r as usize).is_some_and(|p| u64::from(p.view) >= need[0]);
        if fits && (cpu_only || self.rest_fits_at(r, need)) {
            if let Some(s) = self.sigs.get_mut(sig as usize) {
                s.ix.took(top, r);
            }
            return r;
        }
        self.walk(sig, need, top)
    }

    /// The pick's planted bugs: the most loaded member that fits, or a tie broken by a counter
    /// outside the owner.
    #[cfg(test)]
    fn pick_mutant(&mut self, sig: u32, need: &Amounts) -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static TURN: AtomicU32 = AtomicU32::new(0);
        let Some(s) = self.sigs.get(sig as usize) else { return NIL };
        let mut fits: Vec<(u32, u32)> = Vec::new();
        let mut b = s.ix.top();
        while let Some(bb) = b {
            for r in s.ix.bucket(bb) {
                if self.peers.get(r as usize).is_some_and(|p| u64::from(p.view) >= need[0]) && self.rest_fits_at(r, need) {
                    fits.push((bb, r));
                }
            }
            b = s.ix.below(bb);
        }
        if super::planted(super::Mutant::MostLoaded) {
            return fits.last().map_or(NIL, |f| f.1);
        }
        let Some(&(top, _)) = fits.first() else { return NIL };
        let ties: Vec<u32> = fits.iter().filter(|f| f.0 == top).map(|f| f.1).collect();
        let turn = TURN.fetch_add(1, Ordering::Relaxed) as usize;
        ties.get(turn % ties.len().max(1)).copied().unwrap_or(NIL)
    }

    /// The pick's walk: bucket by bucket from `top` down to the lowest that could hold a fit (the
    /// one whose range holds the need: an exact fit there counts), member by member in turn, each
    /// checked exactly.
    #[cold]
    #[inline(never)]
    fn walk(&mut self, sig: u32, need: &Amounts, top: u32) -> u32 {
        let floor = if self.scaled { 0 } else { u32::try_from(need[0] / CPU).unwrap_or(u32::MAX) };
        let mut walked = 0u64;
        let mut found = (NIL, 0u32);
        if let Some(s) = self.sigs.get(sig as usize) {
            let mut b = top;
            'buckets: while b >= floor {
                for r in s.ix.bucket(b) {
                    if self.peers.get(r as usize).is_some_and(|p| u64::from(p.view) >= need[0]) && (!multi(need) || self.rest_fits_at(r, need)) {
                        found = (r, b);
                        break 'buckets;
                    }
                    walked += 1;
                }
                match s.ix.below(b) {
                    Some(n) => b = n,
                    None => break,
                }
            }
        }
        self.counts.walked = self.counts.walked.saturating_add(walked);
        if found.0 != NIL
            && let Some(s) = self.sigs.get_mut(sig as usize)
        {
            s.ix.took(found.1, found.0);
        }
        found.0
    }

    /// Whether `need`'s memory, disk and GPUs fit runner `r` at this owner's view.
    #[inline]
    fn rest_fits_at(&self, r: u32, need: &Amounts) -> bool {
        let (Some(p), Some(c)) = (self.peers.get(r as usize), self.cold.get(r as usize)) else { return false };
        let free = if p.flags & WHOLE != 0 { rest(&c.budget) } else { c.word_rest };
        let [a, b, d] = free;
        let [ia, ib, id] = c.inflight_rest;
        rest_fits(&rest(need), &[a.saturating_sub(ia), b.saturating_sub(ib), d.saturating_sub(id)])
    }

    /// Takes what it sends runner `r` out of its view at once, and moves `r` to its new bucket.
    /// The epoch to send under.
    #[inline(always)]
    fn deduct(&mut self, r: u32, need: &Amounts, cpu_only: bool) -> u32 {
        let last = self.last();
        let Some(p) = self.peers.get_mut(r as usize) else { return 0 };
        #[cfg(test)]
        if super::planted(super::Mutant::NoDeduct) {
            return p.epoch();
        }
        let cpu = narrow(need[0]);
        let from = key(p.view, p.scale, last);
        p.view = p.view.saturating_sub(cpu);
        p.inflight = p.inflight.saturating_add(cpu);
        let (epoch, to, sig0, more) = (p.epoch(), key(p.view, p.scale, last), p.sig0, p.flags & MORE != 0);
        if !cpu_only {
            self.deduct_rest(r, need);
        }
        if from != to {
            self.relink_all(r, sig0, more, from, to);
        }
        epoch
    }

    #[cold]
    #[inline(never)]
    fn deduct_rest(&mut self, r: u32, need: &Amounts) {
        if let Some(c) = self.cold.get_mut(r as usize) {
            for (f, n) in c.inflight_rest.iter_mut().zip(rest(need)) {
                *f = f.saturating_add(n);
            }
        }
    }

    /// Moves `r` from bucket `from` to bucket `to` in every index it's in.
    #[inline(always)]
    fn relink_all(&mut self, r: u32, sig0: u32, more: bool, from: u32, to: u32) {
        if let Some(s) = self.sigs.get_mut(sig0 as usize) {
            s.ix.relink(r, from, to);
        }
        if more {
            self.relink_more(r, from, to);
        }
    }

    /// Moves `r` from `from` to `to` in every index it's in but `except` (moved already).
    #[cold]
    #[inline(never)]
    fn relink_others(&mut self, r: u32, except: u32, from: u32, to: u32) {
        let Some(p) = self.peers.get(r as usize).copied() else { return };
        for o in self.sigs_of(r, &p) {
            if o != except
                && let Some(s) = self.sigs.get_mut(o as usize)
            {
                s.ix.relink(r, from, to);
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn relink_more(&mut self, r: u32, from: u32, to: u32) {
        let list = self.more.get(r as usize).map_or(&[][..], |v| v.as_slice());
        for &o in list {
            if let Some(s) = self.sigs.get_mut(o as usize) {
                s.ix.relink(r, from, to);
            }
        }
    }

    /// Takes runner `r`'s word into its view: its room less what is in flight there; and moves
    /// it if its bucket changed (a rise tries the runs waiting for its signatures).
    #[inline(always)]
    fn apply(&mut self, r: u32, word: &Word, out: &mut Out) {
        let last = self.last();
        let Some(p) = self.peers.get_mut(r as usize) else { return };
        let from = key(p.view, p.scale, last);
        p.view = narrow(word.cpu).saturating_sub(p.inflight);
        // Memory, disk, GPUs and speed are read only when they aren't as they were.
        if (p.stamp >> 32) as u32 != msg::tag(word.whole, word.speed) {
            self.apply_rest(r, word);
        }
        let Some(p) = self.peers.get(r as usize) else { return };
        if p.flags & UP == 0 {
            return;
        }
        let to = key(p.view, p.scale, last);
        if from == to {
            return;
        }
        let (sig0, more) = (p.sig0, p.flags & MORE != 0);
        self.relink_all(r, sig0, more, from, to);
        if to > from && self.waiting != 0 {
            self.rise(r, out);
        }
    }

    /// The part of a word that only measured or mixed fleets change: memory, disk and GPUs not
    /// all free, a new speed.
    #[cold]
    #[inline(never)]
    fn apply_rest(&mut self, r: u32, word: &Word) {
        let Some(p) = self.peers.get_mut(r as usize) else { return };
        if word.whole {
            p.flags |= WHOLE;
        } else {
            p.flags &= !WHOLE;
        }
        let Some(c) = self.cold.get_mut(r as usize) else { return };
        if word.speed != c.speed {
            // A new multiplier: the caller moves it from its old bucket to the one it gives.
            let scale = Self::scale_of(word.speed);
            c.speed = word.speed;
            p.scale = scale;
            self.scaled |= scale != SCALE;
        }
        c.word_rest = word.rest;
        c.slowness = word.slowness;
        self.restamp(r);
    }

    /// A word from runner `r`: false when it is to be ignored (an unknown runner, or an epoch
    /// older than the one known). A newer epoch means the runner started again, and lost every
    /// run it had.
    #[inline(always)]
    fn fresh(&mut self, r: u32, word: &Word, out: &mut Out) -> bool {
        let Some(p) = self.peers.get(r as usize) else { return self.stale() };
        if p.flags & KNOWN == 0 || word.epoch < p.epoch() {
            return self.stale();
        }
        if word.epoch > p.epoch() {
            self.restarted(r, word.epoch, out);
        }
        true
    }

    #[cold]
    #[inline(never)]
    fn stale(&mut self) -> bool {
        self.counts.stale = self.counts.stale.saturating_add(1);
        false
    }

    /// Runner `r` started again under `epoch`: what ran there is lost, and what waited there
    /// waits elsewhere. Starts and Gos sent under the old epoch come back declined.
    #[cold]
    #[inline(never)]
    fn restarted(&mut self, r: u32, epoch: u32, out: &mut Out) {
        if let Some(p) = self.peers.get_mut(r as usize) {
            p.stamp = (p.stamp & !u64::from(u32::MAX)) | u64::from(epoch);
            // Its ends are counted again from the first.
            p.heard = 0;
        }
        let mut again = Vec::new();
        for h in self.runs.handles() {
            let Some(x) = self.runs.get(h).copied() else { continue };
            // Unsure of a start or a Go there (its answer went with the link): the runner before
            // may have run it, and nothing is left to say. At most once: lost, never run again.
            let unsure = x.state & UNSURE != 0 && matches!(x.state & STATE, STARTING | GOING);
            if x.runner == r && unsure {
                if x.state & STATE == STARTING {
                    self.answered(r, x.cpu, x.slow);
                }
                self.free_run(h);
                if x.state & CANCELLED == 0 {
                    out.told.push(Told::Lost { run: h });
                }
                continue;
            }
            match x.state & STATE {
                STARTED if x.runner == r => {
                    self.free_run(h);
                    out.told.push(Told::Lost { run: h });
                }
                WAITING if self.drop_place(x.slow, r) == Some(0) => again.push(h),
                _ => {}
            }
        }
        for h in again {
            self.place(h, out);
        }
    }

    /// Hears a runner's word. What it says to the asker of the run it is about, if anything;
    /// news of other runs, and what to send, go in `out`.
    #[inline(always)]
    pub fn hear(&mut self, msg: ToOwner, out: &mut Out) -> Option<Told> {
        match msg {
            ToOwner::Started { runner, run, at, word } => {
                if self.started_fast(runner.0, run, at, word.brief()) {
                    return Some(Told::Started { run, runner });
                }
                if word.wants {
                    return self.hear_wanted(msg, out);
                }
                self.started(runner.0, run, at, &word, out)
            }
            ToOwner::Ended { runner, run, word } => {
                if self.ended_fast(runner.0, run, word.brief()) {
                    return Some(Told::Ended { run });
                }
                if word.wants {
                    return self.hear_wanted(msg, out);
                }
                self.ended(runner.0, run, &word, out)
            }
            other if other.word().wants => self.hear_wanted(other, out),
            other => self.hear_other(other, out),
        }
    }

    /// A start answered on the fast path: its epoch, the run starting there with CPU alone, and
    /// the word leaving the runner in the bucket it is in (as it does when this owner deducted
    /// what it sent). False: nothing done, [`Owner::hear`]'s general path decides.
    #[inline(always)]
    pub fn started_fast(&mut self, r: u32, run: Handle, at: Handle, b: Brief) -> bool {
        #[cfg(test)]
        if super::any_planted() {
            return false;
        }
        let Some(p) = self.peers.get_mut(r as usize) else { return false };
        if p.stamp != b.stamp {
            return false;
        }
        let Some(x) = self.runs.get_mut(run) else { return false };
        if x.runner != r || x.state != STARTING || x.slow != NIL {
            return false;
        }
        let inflight = p.inflight.saturating_sub(narrow(x.cpu));
        let view = b.cpu.saturating_sub(inflight);
        if view != p.view {
            // Commonest by far when the owner alone changes this runner's room: the view is what
            // it deducted, and no key need be worked out for it. Otherwise it may move within its
            // bucket alone.
            let last = self.last;
            if key(view, p.scale, last) != key(p.view, p.scale, last) {
                return false;
            }
            p.view = view;
        }
        x.state = STARTED;
        x.at = at;
        p.inflight = inflight;
        true
    }

    /// An end on the fast path: its epoch, a run of CPU alone started there, the runner in one
    /// index, nothing waiting to try on it. False: nothing done, [`Owner::hear`]'s general path
    /// decides.
    #[inline(always)]
    pub fn ended_fast(&mut self, r: u32, run: Handle, b: Brief) -> bool {
        #[cfg(test)]
        if super::any_planted() {
            return false;
        }
        if self.waiting != 0 {
            return false;
        }
        let last = self.last();
        let Some(p) = self.peers.get_mut(r as usize) else { return false };
        if p.stamp != b.stamp {
            return false;
        }
        if self.runs.take_if(run, |x| x.runner == r && x.state == STARTED && x.slow == NIL).is_none() {
            return false;
        }
        p.heard = p.heard.wrapping_add(1);
        let from = key(p.view, p.scale, last);
        p.view = b.cpu.saturating_sub(p.inflight);
        let to = key(p.view, p.scale, last);
        if to != from {
            // Its move is put off: nothing reads its bucket before the next pick looks at it.
            if self.pend.r == NIL && to > from {
                self.pend = Pend { r, from, to };
            } else {
                self.moved_slow(r, from, to);
            }
        }
        true
    }

    /// [`Owner::ended_fast`] for a burst of ends: the rise a word brings is put off to the
    /// batch's end: a pick of the batch that goes back to that runner leaves it where it is, and
    /// each runner's net change moves its bucket once (the pair of moves a single step cancels by
    /// [`Pend`], done for a whole batch). End the burst with [`Owner::ask_many`] or any other
    /// call that reads the buckets; [`Owner::settle`] applies what's put off without deciding.
    pub fn ended_batch(&mut self, r: u32, run: Handle, b: Brief) -> bool {
        #[cfg(test)]
        if super::any_planted() {
            return false;
        }
        if self.pend.r != MANY && self.pend.r != NIL {
            return self.ended_fast(r, run, b);
        }
        if self.waiting != 0 {
            return false;
        }
        let last = self.last();
        let Some(p) = self.peers.get_mut(r as usize) else { return false };
        if p.stamp != b.stamp {
            return false;
        }
        if self.runs.take_if(run, |x| x.runner == r && x.state == STARTED && x.slow == NIL).is_none() {
            return false;
        }
        p.heard = p.heard.wrapping_add(1);
        let from = key(p.view, p.scale, last);
        p.view = b.cpu.saturating_sub(p.inflight);
        let to = key(p.view, p.scale, last);
        if to > from && p.flags & PUT_OFF == 0 {
            p.flags |= PUT_OFF;
            self.put_off.push((r, from));
            self.pend.r = MANY;
        } else if to < from && p.flags & PUT_OFF == 0 {
            self.moved_slow(r, from, to);
        }
        true
    }

    /// Applies what [`Owner::ended_batch`] has put off.
    pub fn settle(&mut self) {
        self.flush();
    }

    /// Moves every runner on the put-off list to the bucket its view says.
    #[cold]
    #[inline(never)]
    fn apply_put_off(&mut self) {
        self.pend.r = NIL;
        let mut list = std::mem::take(&mut self.put_off);
        let last = self.last;
        for &(r, from) in &list {
            let Some(p) = self.peers.get_mut(r as usize) else { continue };
            p.flags &= !PUT_OFF;
            let (to, sig0, more) = (key(p.view, p.scale, last), p.sig0, p.flags & MORE != 0);
            if to != from {
                self.relink_all(r, sig0, more, from, to);
            }
        }
        list.clear();
        self.put_off = list;
    }

    /// A batch's picks of the runners whose rises are put off: while one of them is at least as
    /// free as anything in the index, it takes the next run, and its bucket is written once, with
    /// its net change. Each pick goes to the highest bucket, as one at a time; between equals (one
    /// bucket) they go to the runners put off first, in the order their ends came, and the
    /// bucket's turn doesn't move: that is the only way the choices differ from one at a time.
    fn overlay(&mut self, sig: u32, need: Amounts, left: &mut usize, starts: &mut Vec<(RunnerId, Start)>) -> Result<(), Refused> {
        let last = self.last;
        let mut list = std::mem::take(&mut self.put_off);
        let peers = &self.peers;
        let key_of = |r: u32| peers.get(r as usize).map_or(0, |p| key(p.view, p.scale, last));
        list.sort_by_key(|&(r, _)| std::cmp::Reverse(key_of(r)));
        let keys: Vec<u32> = list.iter().map(|&(r, _)| key_of(r)).collect();
        let Some(s) = self.sigs.get_mut(sig as usize) else {
            self.put_off = list;
            return Ok(());
        };
        let top = s.ix.top();
        let cpu = narrow(need[0]);
        let mut i = 0usize;
        while i < list.len() && *left > 0 {
            let Some(&(r, from)) = list.get(i) else { break };
            let Some(p) = self.peers.get_mut(r as usize) else { break };
            let k = key(p.view, p.scale, last);
            if top.is_some_and(|t| k < t) || p.view < cpu || p.sig0 != sig || p.flags & MORE != 0 {
                break;
            }
            let Some(run) = self.runs.insert(Run { cpu: need[0], at: Handle::NONE, sig, runner: r, state: STARTING, slow: NIL }) else {
                self.put_off = list.split_off(i);
                return Err(Refused::Full);
            };
            p.view -= cpu;
            p.inflight = p.inflight.saturating_add(cpu);
            starts.push((RunnerId(r), Start { owner: self.id, run, epoch: p.epoch(), need }));
            *left -= 1;
            let now = key(p.view, p.scale, last);
            let next = keys.get(i + 1).copied().unwrap_or(0);
            if *left == 0 || now < next || top.is_some_and(|t| now < t) || p.view < cpu {
                // Done with this runner: its bucket moves once, by its net change.
                p.flags &= !PUT_OFF;
                if now != from {
                    s.ix.relink(r, from, now);
                }
                i += 1;
            }
        }
        // Those it didn't reach stay put off: the caller applies them.
        self.put_off = list.split_off(i);
        if self.put_off.is_empty() {
            self.pend.r = NIL;
        }
        Ok(())
    }

    /// Every word but Started and Ended: off the fast path.
    #[cold]
    #[inline(never)]
    fn hear_other(&mut self, msg: ToOwner, out: &mut Out) -> Option<Told> {
        self.flush();
        match msg {
            ToOwner::Declined { runner, run, word, why, of } => self.declined(runner.0, run, &word, why, of, out),
            ToOwner::Queued { runner, run, at, word } => self.queued(runner.0, run, at, &word, out),
            ToOwner::Ready { runner, run, at, word } => self.ready(runner.0, run, at, &word, out),
            ToOwner::Hint { runner, word } => {
                if self.fresh(runner.0, &word, out) {
                    self.apply(runner.0, &word, out);
                }
            }
            ToOwner::Synced { runner, word, ended } => self.synced(runner.0, &word, ended, out),
            ToOwner::Started { runner, run, at, word } => return self.started(runner.0, run, at, &word, out),
            ToOwner::Ended { runner, run, word } => return self.ended(runner.0, run, &word, out),
        }
        None
    }

    #[cold]
    #[inline(never)]
    fn started(&mut self, r: u32, run: Handle, at: Handle, word: &Word, out: &mut Out) -> Option<Told> {
        self.flush();
        if !self.fresh(r, word, out) {
            return None;
        }
        let told = match self.runs.get_mut(run) {
            Some(x) if x.runner == r && x.state & STATE == STARTING && x.slow == NIL => {
                // The fast path: a start answered, CPU alone.
                let cpu = narrow(x.cpu);
                *x = Run { state: STARTED, at, ..*x };
                if let Some(p) = self.peers.get_mut(r as usize) {
                    p.inflight = p.inflight.saturating_sub(cpu);
                }
                Some(Told::Started { run, runner: RunnerId(r) })
            }
            _ => self.started_slow(r, run, at),
        };
        self.apply(r, word, out);
        told
    }

    /// Started for a run with a slow record, a Go, a run said again, or a stale one.
    #[cold]
    #[inline(never)]
    fn started_slow(&mut self, r: u32, run: Handle, at: Handle) -> Option<Told> {
        let x = self.runs.get(run).copied().filter(|x| x.runner == r)?;
        match x.state & STATE {
            STARTING | GOING => {
                if x.state & STATE == STARTING {
                    self.answered(r, x.cpu, x.slow);
                }
                if let Some(y) = self.runs.get_mut(run) {
                    *y = Run { state: STARTED, at, slow: NIL, ..*y };
                }
                self.free_slow(x.slow);
                Some(Told::Started { run, runner: RunnerId(r) })
            }
            // Said again as its link came back: no longer unsure.
            STARTED if x.at == at => {
                if let Some(y) = self.runs.get_mut(run) {
                    y.state = STARTED;
                }
                None
            }
            _ => {
                self.stale();
                None
            }
        }
    }

    /// A start sent to `r` is answered: what it asked is no longer in flight there.
    #[inline]
    fn answered(&mut self, r: u32, cpu: u64, slow: u32) {
        if let Some(p) = self.peers.get_mut(r as usize) {
            p.inflight = p.inflight.saturating_sub(narrow(cpu));
        }
        if let Some(rest) = self.slow.at(slow).map(|s| s.rest)
            && let Some(c) = self.cold.get_mut(r as usize)
        {
            for (f, n) in c.inflight_rest.iter_mut().zip(rest) {
                *f = f.saturating_sub(n);
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn ended(&mut self, r: u32, run: Handle, word: &Word, out: &mut Out) -> Option<Told> {
        self.flush();
        if !self.fresh(r, word, out) {
            return None;
        }
        if let Some(p) = self.peers.get_mut(r as usize) {
            p.heard = p.heard.wrapping_add(1);
        }
        let told = match self.runs.get(run) {
            Some(x) if x.runner == r && x.state == STARTED && x.slow == NIL => {
                let _ = self.runs.remove(run);
                Some(Told::Ended { run })
            }
            _ => self.ended_slow(r, run),
        };
        self.apply(r, word, out);
        told
    }

    #[cold]
    #[inline(never)]
    fn ended_slow(&mut self, r: u32, run: Handle) -> Option<Told> {
        let x = self.runs.get(run).copied().filter(|x| x.runner == r);
        match x {
            Some(x) if x.state & STATE == STARTED => {
                self.free_run(run);
                Some(Told::Ended { run })
            }
            _ => {
                self.stale();
                None
            }
        }
    }

    fn declined(&mut self, r: u32, run: Handle, word: &Word, why: Why, of: Of, out: &mut Out) {
        if !self.fresh(r, word, out) {
            return;
        }
        self.counts.declined = self.counts.declined.saturating_add(1);
        let Some(x) = self.runs.get(run).copied() else {
            self.stale();
            self.apply(r, word, out);
            return;
        };
        match (x.state & STATE, of) {
            (STARTING, Of::Start) if x.runner == r => {
                self.answered(r, x.cpu, x.slow);
                self.park(run);
                self.apply(r, word, out);
                #[cfg(test)]
                if why == Why::Room && super::planted(super::Mutant::DropDeclined) {
                    self.free_run(run);
                    return;
                }
                self.place(run, out);
            }
            (GOING, Of::Go) if x.runner == r => {
                self.park(run);
                self.apply(r, word, out);
                self.place(run, out);
            }
            (WAITING, Of::Wait) => {
                let left = if why == Why::Epoch { self.drop_place_before(x.slow, r, word.epoch) } else { self.drop_place(x.slow, r) };
                self.apply(r, word, out);
                if left == Some(0) {
                    self.place(run, out);
                }
            }
            _ => {
                self.stale();
                self.apply(r, word, out);
            }
        }
    }

    fn queued(&mut self, r: u32, run: Handle, at: Handle, word: &Word, out: &mut Out) {
        if !self.fresh(r, word, out) {
            return;
        }
        let wanted = match self.runs.get(run) {
            Some(x) if x.state & STATE == WAITING => self.set_place(x.slow, r, at),
            _ => false,
        };
        if !wanted {
            out.to.push((RunnerId(r), ToRunner::Withdraw { owner: self.id, run, at }));
        }
        self.apply(r, word, out);
    }

    fn ready(&mut self, r: u32, run: Handle, at: Handle, word: &Word, out: &mut Out) {
        if !self.fresh(r, word, out) {
            return;
        }
        let x = self.runs.get(run).copied();
        #[allow(unused_mut)]
        let mut take = x.is_some_and(|x| x.state & STATE == WAITING && self.set_place(x.slow, r, at));
        #[cfg(test)]
        if x.is_some_and(|x| x.state & STATE == GOING && x.runner != r) && super::planted(super::Mutant::GoTwice) {
            take = true;
        }
        let epoch = self.peers.get(r as usize).map_or(0, |p| p.epoch());
        if take {
            self.unwait(run, out, r);
            if let Some(y) = self.runs.get_mut(run) {
                y.state = GOING;
                y.runner = r;
                y.at = at;
            }
            out.to.push((RunnerId(r), ToRunner::Go { owner: self.id, run, at, epoch }));
        } else {
            out.to.push((RunnerId(r), ToRunner::Withdraw { owner: self.id, run, at }));
        }
        self.apply(r, word, out);
    }

    /// Runner `r`'s link is back: it has said every live run of this owner's there again, and how
    /// many of this owner's runs have ended there (`ended`). What this owner was unsure of there
    /// is settled by that count, and the runner is in its buckets again.
    fn synced(&mut self, r: u32, word: &Word, ended: u32, out: &mut Out) {
        if !self.fresh(r, word, out) {
            return;
        }
        // Unsure now: runs that started there (it ended: it wasn't said again), and starts or Gos
        // whose answers went with the link (each never reached it, or ran and ended there).
        let (mut gone, mut unknown) = (Vec::new(), Vec::new());
        for h in self.runs.handles() {
            let Some(x) = self.runs.get(h).copied() else { continue };
            if x.runner != r || x.state & UNSURE == 0 {
                continue;
            }
            if x.state & STATE == STARTED { gone.push(h) } else { unknown.push(h) }
        }
        let heard = self.peers.get(r as usize).map_or(0, |p| p.heard);
        // Ends it said that never reached this owner: each a run of `gone`, or one of `unknown`
        // that ran and ended.
        let missed = ended.wrapping_sub(heard) as usize;
        let ran = missed.saturating_sub(gone.len());
        for h in gone {
            self.free_run(h);
            out.told.push(Told::Ended { run: h });
        }
        let mut again = Vec::new();
        let all_ran = ran == unknown.len();
        for h in unknown {
            let Some(x) = self.runs.get(h).copied() else { continue };
            if x.state & STATE == STARTING {
                self.answered(r, x.cpu, x.slow);
            }
            if ran == 0 {
                // None of them ran: placed again.
                self.park(h);
                again.push(h);
            } else {
                // Every one ran and ended; or some did, and which isn't known: at most once, each
                // is lost, never run again.
                self.free_run(h);
                out.told.push(if all_ran { Told::Ended { run: h } } else { Told::Lost { run: h } });
            }
        }
        let last = self.last();
        let Some(p) = self.peers.get_mut(r as usize) else { return };
        let was_up = p.flags & UP != 0;
        let from = key(p.view, p.scale, last);
        p.heard = ended;
        p.flags |= UP;
        p.view = narrow(word.cpu).saturating_sub(p.inflight);
        if word.whole {
            p.flags |= WHOLE;
        } else {
            p.flags &= !WHOLE;
        }
        let pc = *p;
        self.restamp(r);
        let to = key(pc.view, pc.scale, last);
        if let Some(c) = self.cold.get_mut(r as usize) {
            c.word_rest = word.rest;
        }
        for s in self.sigs_of(r, &pc) {
            if let Some(s) = self.sigs.get_mut(s as usize) {
                if was_up {
                    s.ix.relink(r, from, to);
                } else {
                    s.ix.push(r, to);
                }
            }
        }
        for h in again {
            self.place(h, out);
        }
        if self.waiting != 0 {
            self.rise(r, out);
            self.unpark(r, out);
        }
    }

    /// Runner `r`'s link dropped: what was on it is lost. It leaves its buckets; what was
    /// starting, started or going there is unsure until it syncs; what waited there waits
    /// elsewhere.
    pub fn link_down(&mut self, r: RunnerId, out: &mut Out) {
        self.flush();
        let ri = r.0;
        let last = self.last();
        let Some(p) = self.peers.get_mut(ri as usize) else { return };
        let was_up = p.flags & UP != 0;
        p.flags &= !UP;
        let pc = *p;
        self.restamp(ri);
        // Out of its buckets if it was in them. A drop while it was still syncing (its Synced
        // lost with the link) still makes what it had unsure: a run said again before the drop,
        // whose end the drop then lost, would otherwise stay started for ever.
        if was_up {
            let b = key(pc.view, pc.scale, last);
            for s in self.sigs_of(ri, &pc) {
                if let Some(s) = self.sigs.get_mut(s as usize) {
                    s.ix.unlink(ri, b);
                }
            }
        }
        let mut again = Vec::new();
        for h in self.runs.handles() {
            let Some(x) = self.runs.get(h).copied() else { continue };
            match x.state & STATE {
                STARTING | STARTED | GOING if x.runner == ri => {
                    if let Some(y) = self.runs.get_mut(h) {
                        y.state |= UNSURE;
                    }
                }
                WAITING if self.drop_place(x.slow, ri) == Some(0) => again.push(h),
                _ => {}
            }
        }
        for h in again {
            self.place(h, out);
        }
    }

    /// Places a run again (its start was declined, or its runner's link came back without it):
    /// a member with room now, else it waits.
    fn place(&mut self, run: Handle, out: &mut Out) {
        let Some(x) = self.runs.get(run).copied() else { return };
        // Its asker gave up: it isn't placed again.
        if x.state & CANCELLED != 0 {
            self.free_run(run);
            return;
        }
        // Only a run parked here moves: one starting, going, running or waiting at runners is in
        // hand already, so a second call (its state changed under the first) does nothing.
        if !self.is_parked(&x) {
            return;
        }
        let need = self.need_of(&x);
        let cpu_only = !multi(&need);
        let r = self.pick(x.sig, &need, cpu_only);
        if r == NIL {
            self.wait(run, out, true);
            return;
        }
        self.unlist(x.sig, run.slot, x.slow);
        let epoch = self.deduct(r, &need, cpu_only);
        if let Some(y) = self.runs.get_mut(run) {
            y.state = STARTING;
            y.runner = r;
            y.at = Handle::NONE;
        }
        out.to.push((RunnerId(r), ToRunner::Start(Start { owner: self.id, run, epoch, need })));
    }

    /// Sends a run to wait at the [`PLACES`] members nearest to fitting it (the most free, first
    /// in first out among equals) whose budget covers it; none reachable, it waits here until a
    /// member comes back. Never, when no member could ever take it.
    fn wait(&mut self, run: Handle, out: &mut Out, tell: bool) {
        let Some(x) = self.runs.get(run).copied() else { return };
        let need = self.need_of(&x);
        let Some(s) = self.sigs.get(x.sig as usize) else { return };
        if !s.ix.could_fit(&need) {
            self.free_run(run);
            out.told.push(Told::Never { run });
            return;
        }
        let mut places = [NO_PLACE; PLACES];
        let mut n = 0usize;
        let mut b = s.ix.top();
        'find: while let Some(bb) = b {
            for r in s.ix.bucket(bb) {
                if self.cold.get(r as usize).is_some_and(|c| need.iter().zip(&c.budget).all(|(n, b)| n <= b)) {
                    if let Some(p) = places.get_mut(n) {
                        *p = Place { runner: r, at: Handle::NONE, epoch: self.peers.get(r as usize).map_or(0, |q| q.epoch()) };
                    }
                    n += 1;
                    if n == PLACES {
                        break 'find;
                    }
                }
            }
            b = s.ix.below(bb);
        }
        if self.draws.on || self.weights.on {
            places = self.drawn(x.sig, &need, places);
        }
        let slow = if x.slow == NIL {
            match self.slow.insert(Slow::of(&need)) {
                Some(h) => h.slot,
                None => return,
            }
        } else {
            x.slow
        };
        if let Some(sl) = self.slow.at_mut(slow) {
            // Places it had are withdrawn (those whose answer hasn't come are withdrawn as it
            // comes, finding no place that wants it), so none is left behind in a line.
            for p in sl.places {
                if p.runner != NIL && p.at != Handle::NONE {
                    out.to.push((RunnerId(p.runner), ToRunner::Withdraw { owner: self.id, run, at: p.at }));
                }
            }
            sl.places = places;
        }
        if let Some(y) = self.runs.get_mut(run) {
            y.state = WAITING;
            y.runner = NIL;
            y.at = Handle::NONE;
            y.slow = slow;
        }
        self.list(x.sig, run.slot, slow);
        for p in places.iter().filter(|p| p.runner != NIL) {
            let epoch = self.peers.get(p.runner as usize).map_or(0, |q| q.epoch());
            out.to.push((RunnerId(p.runner), ToRunner::Wait(Start { owner: self.id, run, epoch, need })));
        }
        self.counts.waited = self.counts.waited.saturating_add(1);
        if tell {
            out.told.push(Told::Waiting { run });
        }
    }

    /// Takes a run out of waiting (it goes to `keep`, or elsewhere): its places are withdrawn,
    /// those whose line handle is known now, the rest as their runners answer.
    fn unwait(&mut self, run: Handle, out: &mut Out, keep: u32) {
        let Some(x) = self.runs.get(run).copied() else { return };
        if let Some(sl) = self.slow.at_mut(x.slow) {
            for p in &mut sl.places {
                if p.runner != NIL && p.runner != keep && p.at != Handle::NONE {
                    out.to.push((RunnerId(p.runner), ToRunner::Withdraw { owner: self.id, run, at: p.at }));
                }
                *p = NO_PLACE;
            }
        }
        self.unlist(x.sig, run.slot, x.slow);
    }

    /// Tries the oldest runs waiting for signature `sig`'s members while one has room for them.
    fn retry(&mut self, sig: u32, out: &mut Out) {
        loop {
            let Some(head) = self.sigs.get(sig as usize).map(|s| s.wait_head) else { return };
            let Some(run) = self.runs.handle(head) else { return };
            let Some(x) = self.runs.get(run).copied() else { return };
            let need = self.need_of(&x);
            let cpu_only = !multi(&need);
            let r = self.pick(sig, &need, cpu_only);
            if r == NIL {
                return;
            }
            self.unwait(run, out, NIL);
            let epoch = self.deduct(r, &need, cpu_only);
            if let Some(y) = self.runs.get_mut(run) {
                y.state = STARTING;
                y.runner = r;
            }
            out.to.push((RunnerId(r), ToRunner::Start(Start { owner: self.id, run, epoch, need })));
        }
    }

    /// Runner `r`'s room rose: the runs waiting for its signatures are tried.
    fn rise(&mut self, r: u32, out: &mut Out) {
        let Some(p) = self.peers.get(r as usize).copied() else { return };
        let list: Vec<u32> = std::iter::once(p.sig0).chain(self.more.get(r as usize).cloned().unwrap_or_default()).filter(|s| *s != NIL).collect();
        for s in list {
            if self.sigs.get(s as usize).is_some_and(|s| s.waiting != 0) {
                self.retry(s, out);
            }
        }
    }

    /// Runner `r` is reachable again (its link came back, or it joined a signature): runs of its
    /// signatures that wait nowhere (no member was reachable) are sent to wait.
    fn unpark(&mut self, r: u32, out: &mut Out) {
        let Some(p) = self.peers.get(r as usize).copied() else { return };
        let list: Vec<u32> = std::iter::once(p.sig0).chain(self.more.get(r as usize).cloned().unwrap_or_default()).filter(|s| *s != NIL).collect();
        for s in list {
            let mut slot = self.sigs.get(s as usize).map_or(NIL, |s| s.wait_head);
            let mut parked = Vec::new();
            while let Some(x) = self.runs.at(slot).copied() {
                let sl = self.slow.at(x.slow).copied();
                if sl.is_some_and(|sl| sl.places.iter().all(|p| p.runner == NIL)) {
                    parked.extend(self.runs.handle(slot));
                }
                slot = sl.map_or(NIL, |sl| sl.next);
            }
            for h in parked {
                self.wait(h, out, false);
            }
        }
    }

    /// Whether a run waits nowhere: parked here, to be placed.
    fn is_parked(&self, x: &Run) -> bool {
        x.state & STATE == WAITING && self.slow.at(x.slow).is_none_or(|sl| sl.places.iter().all(|p| p.runner == NIL))
    }

    /// Parks a run here (its start, Go or places came to nothing): waiting nowhere, to be placed.
    fn park(&mut self, run: Handle) {
        if let Some(x) = self.runs.get_mut(run) {
            x.state = WAITING | (x.state & CANCELLED);
            x.runner = NIL;
            x.at = Handle::NONE;
        }
    }

    /// Drops a run's place at `r` if its Wait went under an epoch other than `epoch` (the runner
    /// fenced that one); how many places it has left (`None`: none dropped).
    fn drop_place_before(&mut self, slow: u32, r: u32, epoch: u32) -> Option<usize> {
        let sl = self.slow.at_mut(slow)?;
        let p = sl.places.iter_mut().find(|p| p.runner == r && p.epoch != epoch)?;
        *p = NO_PLACE;
        Some(sl.places.iter().filter(|p| p.runner != NIL).count())
    }

    /// Sets the line handle of a run's place at `r` (false: it has none there, or one already).
    fn set_place(&mut self, slow: u32, r: u32, at: Handle) -> bool {
        let Some(sl) = self.slow.at_mut(slow) else { return false };
        match sl.places.iter_mut().find(|p| p.runner == r) {
            Some(p) if p.at == Handle::NONE || p.at == at => {
                p.at = at;
                true
            }
            _ => false,
        }
    }

    /// Drops a run's place at `r`; how many places it has left (`None`: it had none there).
    fn drop_place(&mut self, slow: u32, r: u32) -> Option<usize> {
        let sl = self.slow.at_mut(slow)?;
        let p = sl.places.iter_mut().find(|p| p.runner == r)?;
        *p = NO_PLACE;
        Some(sl.places.iter().filter(|p| p.runner != NIL).count())
    }

    /// Puts a run (by slot) at the tail of its signature's waiting runs, unless it's there.
    fn list(&mut self, sig: u32, run: u32, slow: u32) {
        let Some(sl) = self.slow.at(slow).copied() else { return };
        if sl.listed {
            return;
        }
        let Some(s) = self.sigs.get_mut(sig as usize) else { return };
        let tail = s.wait_tail;
        s.wait_tail = run;
        if tail == NIL {
            s.wait_head = run;
        }
        s.waiting = s.waiting.saturating_add(1);
        self.waiting = self.waiting.saturating_add(1);
        if let Some(t) = self.runs.at(tail).map(|t| t.slow)
            && let Some(ts) = self.slow.at_mut(t)
        {
            ts.next = run;
        }
        if let Some(sl) = self.slow.at_mut(slow) {
            sl.prev = tail;
            sl.next = NIL;
            sl.listed = true;
        }
    }

    /// Takes a run out of its signature's waiting runs, if it's there.
    fn unlist(&mut self, sig: u32, run: u32, slow: u32) {
        let Some(sl) = self.slow.at(slow).copied() else { return };
        if !sl.listed {
            return;
        }
        let _ = run;
        let Some(s) = self.sigs.get_mut(sig as usize) else { return };
        if sl.prev == NIL {
            s.wait_head = sl.next;
        }
        if sl.next == NIL {
            s.wait_tail = sl.prev;
        }
        s.waiting = s.waiting.saturating_sub(1);
        self.waiting = self.waiting.saturating_sub(1);
        if let Some(p) = self.runs.at(sl.prev).map(|p| p.slow)
            && let Some(ps) = self.slow.at_mut(p)
        {
            ps.next = sl.next;
        }
        if let Some(n) = self.runs.at(sl.next).map(|n| n.slow)
            && let Some(ns) = self.slow.at_mut(n)
        {
            ns.prev = sl.prev;
        }
        if let Some(sl) = self.slow.at_mut(slow) {
            sl.listed = false;
            sl.prev = NIL;
            sl.next = NIL;
        }
    }

    /// A run's whole need.
    fn need_of(&self, x: &Run) -> Amounts {
        let [m, d, g] = self.slow.at(x.slow).map_or([0; 3], |s| s.rest);
        [x.cpu, m, d, g]
    }

    /// Forgets a run.
    fn free_run(&mut self, run: Handle) {
        let Some(x) = self.runs.get(run).copied() else { return };
        if x.slow != NIL {
            self.unlist(x.sig, run.slot, x.slow);
            self.free_slow(x.slow);
        }
        let _ = self.runs.remove(run);
    }

    fn free_slow(&mut self, slow: u32) {
        if let Some(h) = self.slow.handle(slow) {
            let _ = self.slow.remove(h);
        }
    }

    /// The asker gave up on a run (lb/wire's ask 1). Waiting, it is forgotten at once: its places
    /// are withdrawn (those whose line handle isn't known yet, as their answers come) and nothing
    /// is told; true. Starting or going, it is marked: its Started is still told, so the shell can
    /// end it at its runner, but a decline is never placed again; false. Started or gone: false
    /// (the shell ends a started run at its runner).
    pub fn cancel(&mut self, run: Handle, out: &mut Out) -> bool {
        let Some(x) = self.runs.get(run).copied() else { return false };
        match x.state & STATE {
            WAITING => {
                self.unwait(run, out, NIL);
                self.free_run(run);
                true
            }
            STARTING | GOING => {
                if let Some(y) = self.runs.get_mut(run) {
                    y.state |= CANCELLED;
                }
                false
            }
            _ => false,
        }
    }

    /// Runner `r`'s whole budget changed (lb/wire's ask 2: GPUs and disk probed after it started,
    /// memory on a VM): what could ever fit its signatures follows. The shell tells the runner too.
    pub fn set_budget(&mut self, r: RunnerId, budget: Amounts) {
        self.flush();
        let ri = r.0;
        let Some(c) = self.cold.get_mut(ri as usize) else { return };
        let old = std::mem::replace(&mut c.budget, budget);
        let wanted = u32::try_from((budget[0] / CPU).saturating_add(1)).unwrap_or(BUCKETS).min(BUCKETS);
        if wanted > self.buckets {
            self.buckets = wanted;
            self.last = wanted.saturating_sub(1);
            let (buckets, runners) = (self.buckets, u32::try_from(self.peers.len()).unwrap_or(NIL));
            for s in &mut self.sigs {
                s.ix.grow(buckets, runners);
            }
        }
        let Some(p) = self.peers.get(ri as usize).copied() else { return };
        for s in self.sigs_of(ri, &p) {
            if let Some(s) = self.sigs.get_mut(s as usize) {
                s.ix.reshape(&old, &budget);
            }
        }
    }

    /// Where an ask would go now, without deciding it (nothing deducted, no turn taken): the
    /// member the pick would choose, in the pick's order with its exact fit (lb/wire's ask 4,
    /// for `kiln run --reuse`). `None`: no member has room now.
    pub fn peek(&self, sig: SigId, need: &Amounts) -> Option<RunnerId> {
        let s = self.sigs.get(sig.0 as usize)?;
        let floor = if self.scaled { 0 } else { u32::try_from(need[0] / CPU).unwrap_or(u32::MAX) };
        let moved = self.put_off(sig.0);
        let mut b = Self::top_of(&s.ix, moved);
        while let Some(bb) = b.filter(|bb| *bb >= floor) {
            for r in s.ix.bucket_as(bb, moved) {
                if self.peers.get(r as usize).is_some_and(|p| u64::from(p.view) >= need[0]) && (!multi(need) || self.rest_fits_at(r, need)) {
                    return Some(RunnerId(r));
                }
            }
            b = s.ix.below_as(bb, moved);
        }
        None
    }

    /// The move put off in signature `sig`'s index, if any: its runner, the bucket it's in and
    /// the bucket its view says.
    fn put_off(&self, sig: u32) -> Option<(u32, u32, u32)> {
        let Pend { r, from, .. } = self.pend;
        if r >= MANY || !self.sigs.get(sig as usize).is_some_and(|s| s.ix.contains(r)) {
            return None;
        }
        let p = self.peers.get(r as usize)?;
        Some((r, from, key(p.view, p.scale, self.last())))
    }

    /// The highest bucket that holds a member, with the move put off.
    fn top_of(ix: &Index, moved: Option<(u32, u32, u32)>) -> Option<u32> {
        match (ix.top(), moved) {
            (Some(t), Some((_, _, to))) => Some(t.max(to)),
            (t, None) => t,
            (None, Some((_, _, to))) => Some(to),
        }
    }

    /// Its view of runner `r`: what it takes `r` to have free for a new run (`None`: not known,
    /// or its link is down).
    pub fn view(&self, r: RunnerId) -> Option<Amounts> {
        let p = self.peers.get(r.0 as usize).filter(|p| p.flags & UP != 0)?;
        let c = self.cold.get(r.0 as usize)?;
        let free = if p.flags & WHOLE != 0 { rest(&c.budget) } else { c.word_rest };
        let [a, b, d] = free;
        let [ia, ib, id] = c.inflight_rest;
        Some([u64::from(p.view), a.saturating_sub(ia), b.saturating_sub(ib), d.saturating_sub(id)])
    }

    /// A run's state (`None`: it's gone, or never was).
    pub fn state(&self, run: Handle) -> Option<State> {
        let x = self.runs.get(run)?;
        Some(match x.state & STATE {
            STARTING => State::Starting { runner: RunnerId(x.runner) },
            STARTED => State::Started { runner: RunnerId(x.runner), at: x.at },
            GOING => State::Going { runner: RunnerId(x.runner) },
            _ => {
                let places = self.slow.at(x.slow).map_or([NO_PLACE; PLACES], |s| s.places);
                State::Waiting { at: places.map(|p| (p.runner != NIL).then_some(RunnerId(p.runner))) }
            }
        })
    }

    /// Every run it holds, with its state.
    pub fn runs(&self) -> Vec<(Handle, State)> {
        self.runs.iter().filter_map(|(h, _)| Some((h, self.state(h)?))).collect()
    }

    /// Whether runner `r` is in signature `sig`'s buckets (a member whose link is up).
    pub fn bucketed(&self, sig: SigId, r: RunnerId) -> bool {
        self.sigs.get(sig.0 as usize).is_some_and(|s| s.ix.contains(r.0)) && self.peers.get(r.0 as usize).is_some_and(|p| p.flags & UP != 0)
    }

    /// Signature `sig`'s members, each bucket's first to last, highest bucket first: the order its
    /// picks take them in.
    pub fn order(&self, sig: SigId) -> Vec<RunnerId> {
        let Some(s) = self.sigs.get(sig.0 as usize) else { return Vec::new() };
        let moved = self.put_off(sig.0);
        let mut v = Vec::new();
        let mut b = Self::top_of(&s.ix, moved);
        while let Some(bb) = b {
            v.extend(s.ix.bucket_as(bb, moved).into_iter().map(RunnerId));
            b = s.ix.below_as(bb, moved);
        }
        v
    }
}

impl Slow {
    fn of(need: &Amounts) -> Slow {
        Slow { rest: rest(need), places: [NO_PLACE; PLACES], prev: NIL, next: NIL, listed: false }
    }
}

// Room tokens at the owner (docs/lb.md "Room tokens", src/lb/tokens.rs): off the fast paths,
// which a word that wants never takes.
impl Owner {
    /// Room tokens on or off at this owner: its waiting places drawn from each signature's
    /// members (after those whose view covers part of the need), uniformly, from `seed`'s
    /// generator; and a token it can't use given back when its runner says another owner's run
    /// waited there. Its runners say tokens with [`super::Runner::set_tokens`].
    pub fn set_tokens(&mut self, on: bool, seed: u64) {
        self.draws = Draws { on, rng: seed };
    }

    /// The places a run of signature `sig` needing `need` waits at with tokens or learned weights
    /// on: under tokens, members whose view covers part of it (of `walked`, the nearest to
    /// fitting) first; then members drawn, uniformly or in proportion to their learned weights;
    /// then the rest of `walked` if draws found too few. With weights on, the wait is then learned
    /// from (Algorithm 1's test at each place drawn).
    fn drawn(&mut self, sig: u32, need: &Amounts, walked: [Place; PLACES]) -> [Place; PLACES] {
        let Some(s) = self.sigs.get(sig as usize) else { return walked };
        let first: Vec<u32> = if self.draws.on { walked.iter().filter(|p| p.runner != NIL && self.peers.get(p.runner as usize).is_some_and(|q| q.view > 0)).map(|p| p.runner).collect() } else { Vec::new() };
        let (cold, peers, weights) = (&self.cold, &self.peers, &mut self.weights);
        let weighted = weights.drawing();
        let cpu_of = |r: u32| cold.get(r as usize).map_or(0, |c| c.budget[0]);
        let max = if weighted {
            let whole = || peers.iter().zip(cold).filter(|(p, _)| p.flags & KNOWN != 0).fold(0u64, |t, (_, c)| t.saturating_add(c.budget[0]));
            weights.sums(sig, s.ix.len(), s.ix.members().map(|r| (r, cpu_of(r))), whole).1
        } else {
            0
        };
        #[cfg(test)]
        let unweighted = super::planted(super::Mutant::Unweighted);
        #[cfg(not(test))]
        let unweighted = false;
        let mut rng = self.draws.rng;
        let got: [u32; PLACES] = super::tokens::draw(&s.ix, &first, &mut rng, |r, st| {
            let fits = cold.get(r as usize).is_some_and(|c| need.iter().zip(&c.budget).all(|(n, b)| n <= b));
            fits && (!weighted || unweighted || super::tokens::below64(st, max.max(1)) < weights.weight(r, cpu_of(r)))
        });
        self.draws.rng = rng;
        let mut places = [NO_PLACE; PLACES];
        let mut n = 0usize;
        for r in got.into_iter().chain(walked.iter().map(|p| p.runner)).filter(|r| *r != NIL) {
            if places.iter().any(|p| p.runner == r) {
                continue;
            }
            let epoch = self.peers.get(r as usize).map_or(0, |q| q.epoch());
            if let Some(p) = places.get_mut(n) {
                *p = Place { runner: r, at: Handle::NONE, epoch };
                n += 1;
            }
        }
        if weighted {
            self.learn(sig, need, &places);
        }
        places
    }

    /// A word saying another owner's run waited at its runner: heard as any other, then a token
    /// this owner can't use there is given back.
    #[cold]
    #[inline(never)]
    fn hear_wanted(&mut self, msg: ToOwner, out: &mut Out) -> Option<Told> {
        let r = msg.runner().0;
        let told = self.hear_other(msg, out);
        self.offer_back(r, out);
        told
    }

    /// What this owner's view of runner `r` holds goes back to it, to be forwarded once, when no
    /// run of this owner's waits for any of `r`'s signatures: its view of `r` is then what is in
    /// flight there, nothing more.
    fn offer_back(&mut self, r: u32, out: &mut Out) {
        if !self.draws.on {
            return;
        }
        let Some(p) = self.peers.get(r as usize).copied() else { return };
        if p.flags & UP == 0 || self.sigs_of(r, &p).iter().any(|s| self.sigs.get(*s as usize).is_some_and(|x| x.waiting != 0)) {
            return;
        }
        let Some(view) = self.view(RunnerId(r)) else { return };
        if view.iter().all(|v| *v == 0) {
            return;
        }
        let c = self.cold.get(r as usize).copied().unwrap_or_default();
        let word = Word { epoch: p.epoch(), whole: false, cpu: u64::from(p.inflight), rest: c.inflight_rest, speed: c.speed, slowness: c.slowness, wants: false };
        self.apply(r, &word, out);
        out.to.push((RunnerId(r), ToRunner::Release { owner: self.id, epoch: p.epoch(), amount: view }));
    }
}

// Learned wait weights at the owner (docs/lb.md "Learned wait weights", src/lb/weights.rs).
impl Owner {
    /// Learned wait weights on or off (off until set): a run that finds no room waits at members
    /// drawn in proportion to one weight per runner, learned as runs wait (Lattanzi et al.,
    /// SODA 2020, Algorithm 1), from `seed`'s generator; learning starts again from the weights
    /// as they are.
    pub fn set_weights(&mut self, on: bool, seed: u64) {
        self.weights.on = on;
        self.weights.begin();
        if !self.draws.on {
            self.draws.rng = seed;
        }
    }

    /// Runner `r`'s weight is predicted to be `w` (the shell's, from history): its phase starts
    /// again.
    pub fn set_weight(&mut self, r: RunnerId, w: u64) {
        self.weights.set(r.0, w);
    }

    /// Runner `r`'s weight now (its CPU budget until one is set or learned).
    pub fn weight(&self, r: RunnerId) -> u64 {
        self.weights.weight(r.0, self.cold.get(r.0 as usize).map_or(0, |c| c.budget[0]))
    }

    /// Why this owner stopped drawing by its weights, if it did: the predictions weren't helping,
    /// and its places are drawn uniformly again.
    pub fn fell_back(&self) -> Option<FellBack> {
        self.weights.fell_back
    }

    /// A run needing `need` of signature `sig` waits at `places`: the fractional assignment's
    /// loads grow, and each place drawn gets Algorithm 1's test.
    fn learn(&mut self, sig: u32, need: &Amounts, places: &[Place; PLACES]) {
        let m = u32::try_from(self.peers.iter().filter(|p| p.flags & KNOWN != 0).count()).unwrap_or(u32::MAX);
        self.weights.waited(sig, need[0]);
        for p in places.iter().filter(|p| p.runner != NIL) {
            let Some(q) = self.peers.get(p.runner as usize).copied() else { continue };
            let sigs = self.sigs_of(p.runner, &q);
            let cpu = self.cold.get(p.runner as usize).map_or(0, |c| c.budget[0]);
            self.weights.check(p.runner, cpu, &sigs, m);
        }
    }
}

