//! The owner's side (docs/lb.md "The owner"): the node that asks for a run owns it until its
//! runner says it started, waits or never can, and decides where it goes.
//!
//! Its view of a runner is that runner's last word less what the owner has sent it since and not
//! heard back about (in flight): it deducts what it sends at once, so starts in flight never herd
//! onto one runner. The pick is the head of the highest bucket of the signature's [`Index`] whose
//! lower bound covers the CPU asked: the least loaded member, first in first out among equals,
//! with every other dimension checked exactly at the view (W05-R23's "least loaded, then most
//! free"). That is O(1); when the head is short of memory, disk or GPUs the pick walks on, member
//! by member, and counts it ([`Counts::walked`]).
//!
//! Off that path: a run no member has room for waits at runners, not here (fair share lives where
//! the contention is): at the two members nearest to fitting, the first to hold its room wins and
//! the other is withdrawn, so it never starts twice. A runner's decline carries its room, so the
//! next pick never repeats the miss, and the loop ends when no member fits (then the run waits):
//! it is bounded by the members. A link that drops leaves the owner unsure of what it had there
//! until the runner says it all again (Synced); a runner that started again (a newer epoch in its
//! word) has lost every run it had.

use super::index::{BUCKETS, Index};
use super::msg::{Of, Start, ToOwner, ToRunner, Why, Word};
use super::slab::{Handle, Slab};
use super::{Amounts, CPU, NIL, OwnerId, RunnerId, SigId, multi, rest, rest_fits};

/// How many runners a run that finds no room waits at: two. With two choices the most loaded of
/// n bins holds log log n / ln 2 + O(1) instead of Θ(log n / log log n), and each choice past the
/// second only divides the first term by log d (Azar, Broder, Karlin and Upfal, "Balanced
/// Allocations", SIAM J. Comput. 29(1), 1999); for queues, the second choice is where the
/// exponential gain is (Mitzenmacher, "The Power of Two Choices in Randomized Load Balancing",
/// IEEE TPDS 12(10), 2001); and Sparrow measured two probes per task with late binding, the first
/// worker free taking it, as its best trade of messages for waiting (Ousterhout et al., SOSP 2013).
pub const PLACES: usize = 2;

// A runner's flags at its owner.
const KNOWN: u32 = 1;
/// Its link is up and synced: it is in its indexes' buckets.
const UP: u32 = 2;
/// Its last word had all of its memory, disk and GPUs free.
const WHOLE: u32 = 4;
/// It is a member of more than one index.
const MORE: u32 = 8;

// A run's states at its owner.
const STARTING: u32 = 1;
const STARTED: u32 = 2;
const WAITING: u32 = 3;
const GOING: u32 = 4;
const STATE: u32 = 7;
/// Its runner's link dropped while it was starting, started or going there: what became of it
/// waits for the runner's Synced.
const UNSURE: u32 = 8;

/// What the owner keeps of each runner, on every decision's path: 32 bytes, four to a cache line
/// of Apple silicon's (128 bytes), two to an x86 one.
#[derive(Debug, Clone, Copy)]
struct Peer {
    /// CPU free as this owner sees it: its last word less `inflight`.
    view: u64,
    /// CPU of the starts sent to it and not heard back about.
    inflight: u64,
    epoch: u32,
    /// The last of its Ended this owner has heard (their `seq`), acknowledged with each start.
    heard: u32,
    /// Its bucket in every index it is in while it's up (NIL: down).
    bucket: u32,
    /// The first index it's a member of (NIL: none); the rest are in `more`.
    sig0: u32,
    flags: u32,
}

const PEER: Peer = Peer { view: 0, inflight: 0, epoch: 0, heard: 0, bucket: NIL, sig0: NIL, flags: 0 };

/// What the owner keeps of each runner that only a need of memory, disk or GPUs reads.
#[derive(Debug, Clone, Copy, Default)]
struct Cold {
    budget: Amounts,
    /// Memory, disk and GPUs free by its last word (when they weren't all free).
    word_rest: [u64; 3],
    inflight_rest: [u64; 3],
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
    /// Its runner started again, or left, while it ran there: it is gone.
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
    /// Members a pick passed over: a head short of memory, disk or GPUs, or a bucket whose lower
    /// bound didn't cover the CPU asked.
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
    /// Runs waiting, over every signature.
    waiting: u32,
    pub counts: Counts,
}

/// The bucket of a view: its whole CPUs, at most `last`.
#[inline]
fn bucket(view: u64, last: u32) -> u32 {
    u32::try_from(view / CPU).unwrap_or(u32::MAX).min(last)
}

impl Owner {
    pub fn new(id: OwnerId) -> Owner {
        Owner { id, peers: Vec::new(), cold: Vec::new(), more: Vec::new(), sigs: Vec::new(), runs: Slab::new(), slow: Slab::new(), buckets: 1, waiting: 0, counts: Counts::default() }
    }

    pub fn id(&self) -> OwnerId {
        self.id
    }

    /// Room for `runs` runs before its tables grow (none grows on a decision after that).
    pub fn reserve(&mut self, runs: usize) {
        self.runs = Slab::with_capacity(runs);
    }

    #[inline]
    fn last(&self) -> u32 {
        self.buckets.saturating_sub(1)
    }

    /// Takes runner `r` into its fleet with its whole budget and its first word. A runner it knew
    /// already leaves first (its runs there are lost).
    pub fn join(&mut self, r: RunnerId, budget: Amounts, word: Word, out: &mut Out) {
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
        let (buckets, runners) = (self.buckets, u32::try_from(self.peers.len()).unwrap_or(NIL));
        for s in &mut self.sigs {
            s.ix.grow(buckets, runners);
        }
        let last = self.last();
        if let Some(p) = self.peers.get_mut(i) {
            *p = Peer { view: word.cpu, inflight: 0, epoch: word.epoch, heard: 0, bucket: bucket(word.cpu, last), sig0: NIL, flags: KNOWN | UP | if word.whole { WHOLE } else { 0 } };
        }
        if let Some(c) = self.cold.get_mut(i) {
            *c = Cold { budget, word_rest: word.rest, inflight_rest: [0; 3] };
        }
    }

    /// A new signature, with no members yet.
    pub fn sig(&mut self) -> SigId {
        let mut ix = Index::new();
        ix.grow(self.buckets, u32::try_from(self.peers.len()).unwrap_or(NIL));
        let id = u32::try_from(self.sigs.len()).unwrap_or(NIL);
        self.sigs.push(Sig { ix, wait_head: NIL, wait_tail: NIL, waiting: 0 });
        SigId(id)
    }

    /// Runner `r` meets signature `sig`'s constraints: a member from now on. Runs waiting for it
    /// are tried on it.
    pub fn add(&mut self, sig: SigId, r: RunnerId, out: &mut Out) {
        let ri = r.0;
        let Some(p) = self.peers.get(ri as usize).copied().filter(|p| p.flags & KNOWN != 0) else { return };
        let budget = self.cold.get(ri as usize).map_or([0; 4], |c| c.budget);
        let Some(s) = self.sigs.get_mut(sig.0 as usize) else { return };
        if !s.ix.join(ri, &budget) {
            return;
        }
        if p.flags & UP != 0 {
            s.ix.push(ri, p.bucket);
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
        let ri = r.0;
        let Some(p) = self.peers.get(ri as usize).copied() else { return };
        let budget = self.cold.get(ri as usize).map_or([0; 4], |c| c.budget);
        let Some(s) = self.sigs.get_mut(sig.0 as usize) else { return };
        if !s.ix.contains(ri) {
            return;
        }
        if p.flags & UP != 0 {
            s.ix.unlink(ri, p.bucket);
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
        let ri = r.0;
        let Some(p) = self.peers.get(ri as usize).copied().filter(|p| p.flags & KNOWN != 0) else { return };
        let mut sigs: Vec<u32> = self.more.get(ri as usize).cloned().unwrap_or_default();
        if p.sig0 != NIL {
            sigs.insert(0, p.sig0);
        }
        for s in sigs {
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

    /// Decides where a run needing `need` of signature `sig`'s members goes: the start to send
    /// when one has room for it now, else it waits (what to send for that is in `out`). Refused
    /// when no member could ever take it.
    #[inline]
    pub fn ask(&mut self, sig: SigId, need: Amounts, out: &mut Out) -> Result<Asked, Refused> {
        let si = sig.0;
        let cpu_only = !multi(&need);
        let r = self.pick(si, &need, cpu_only);
        if r != NIL {
            let slow = if cpu_only { NIL } else { self.slow.insert(Slow::of(&need)).ok_or(Refused::Full)?.slot };
            let Some(run) = self.runs.insert(Run { cpu: need[0], at: Handle::NONE, sig: si, runner: r, state: STARTING, slow }) else {
                self.free_slow(slow);
                return Err(Refused::Full);
            };
            let (epoch, ack) = self.deduct(r, &need, cpu_only);
            return Ok(Asked { run, send: Some((RunnerId(r), ToRunner::Start(Start { owner: self.id, run, epoch, need, ack }))) });
        }
        let Some(s) = self.sigs.get(si as usize) else { return Err(Refused::NoSig) };
        if !s.ix.could_fit(&need) {
            return Err(Refused::Never);
        }
        let slow = self.slow.insert(Slow::of(&need)).ok_or(Refused::Full)?.slot;
        let Some(run) = self.runs.insert(Run { cpu: need[0], at: Handle::NONE, sig: si, runner: NIL, state: WAITING, slow }) else {
            self.free_slow(slow);
            return Err(Refused::Full);
        };
        self.wait(run, out, false);
        Ok(Asked { run, send: None })
    }

    /// Decides `n` runs of the same need at once, the same choices as `n` asks one at a time (a
    /// CI push's legs): each member chosen is moved once, and a bucket the batch empties is cut
    /// from its list whole. Their starts go in `starts` (runs that wait: their sends are in `out`).
    pub fn ask_many(&mut self, sig: SigId, need: Amounts, n: usize, starts: &mut Vec<(RunnerId, Start)>, waits: &mut Vec<Handle>, out: &mut Out) -> Result<(), Refused> {
        let whole = need[0] / CPU;
        // The batched path is for whole CPUs alone: each pick then moves its member down by
        // exactly `whole` buckets, never within one, so the order one at a time gives is each
        // bucket's members in turn, those moved in during the batch after its own.
        // A runner with more whole CPUs than there are buckets would make the last bucket hold
        // more than one count of CPUs: one at a time there too.
        if multi(&need) || whole == 0 || !need[0].is_multiple_of(CPU) || self.buckets >= BUCKETS {
            for _ in 0..n {
                let a = self.ask(sig, need, out)?;
                match a.send {
                    Some((r, ToRunner::Start(s))) => starts.push((r, s)),
                    _ => waits.push(a.run),
                }
            }
            return Ok(());
        }
        let step = u32::try_from(whole).unwrap_or(u32::MAX);
        let mut left = n;
        while left > 0 {
            let Some(s) = self.sigs.get(sig.0 as usize) else { return Err(Refused::NoSig) };
            let top = match s.ix.top() {
                Some(t) if t >= step => t,
                _ => {
                    // No member has room: the rest one at a time (they wait or never fit).
                    for _ in 0..left {
                        let a = self.ask(sig, need, out)?;
                        match a.send {
                            Some((r, ToRunner::Start(s))) => starts.push((r, s)),
                            _ => waits.push(a.run),
                        }
                    }
                    return Ok(());
                }
            };
            // Take the first `left` (at most all) of the top bucket, in order.
            let mut taken = 0usize;
            let mut r = s.ix.head(top);
            let first = r;
            let mut last = NIL;
            while r != NIL && taken < left {
                last = r;
                r = s.ix.next(r);
                taken += 1;
            }
            self.cut(sig.0, top, first, last, top - step);
            // Each member taken: its view and in flight, its run, its start.
            let mut m = first;
            for _ in 0..taken {
                let next = self.sigs.get(sig.0 as usize).map_or(NIL, |s| s.ix.next(m));
                let Some(run) = self.runs.insert(Run { cpu: need[0], at: Handle::NONE, sig: sig.0, runner: m, state: STARTING, slow: NIL }) else { return Err(Refused::Full) };
                let (epoch, ack) = match self.peers.get_mut(m as usize) {
                    Some(p) => {
                        p.view = p.view.saturating_sub(need[0]);
                        p.inflight = p.inflight.saturating_add(need[0]);
                        p.bucket = top - step;
                        (p.epoch, p.heard)
                    }
                    None => (0, 0),
                };
                starts.push((RunnerId(m), Start { owner: self.id, run, epoch, need, ack }));
                m = next;
            }
            left -= taken;
        }
        Ok(())
    }

    /// Moves the members `first`..=`last` of bucket `from` (its head on) to the tail of bucket
    /// `to`, in every index they are in: the one being picked from in O(1), the others member by
    /// member.
    fn cut(&mut self, sig: u32, from: u32, first: u32, last: u32, to: u32) {
        // Members in other indexes too move there one by one (their places in those lists aren't
        // contiguous); in the index picked from, the run of them moves whole.
        let mut m = first;
        while m != NIL {
            let next = self.sigs.get(sig as usize).map_or(NIL, |s| s.ix.next(m));
            let p = self.peers.get(m as usize).copied().unwrap_or(PEER);
            if p.flags & MORE != 0 || p.sig0 != sig {
                let list: Vec<u32> = std::iter::once(p.sig0).chain(self.more.get(m as usize).cloned().unwrap_or_default()).filter(|s| *s != sig && *s != NIL).collect();
                for o in list {
                    if let Some(s) = self.sigs.get_mut(o as usize) {
                        s.ix.relink(m, from, to);
                    }
                }
            }
            if m == last {
                break;
            }
            m = next;
        }
        if let Some(s) = self.sigs.get_mut(sig as usize) {
            s.ix.splice(from, first, last, to);
        }
    }

    /// The member a need goes to: the head of the highest bucket whose lower bound covers its CPU
    /// if the rest fits it, else a walk (NIL: none fits).
    #[inline]
    fn pick(&mut self, sig: u32, need: &Amounts, cpu_only: bool) -> u32 {
        let Some(s) = self.sigs.get(sig as usize) else { return NIL };
        let Some(top) = s.ix.top() else { return NIL };
        #[cfg(test)]
        if super::planted(super::Mutant::MostLoaded) || super::planted(super::Mutant::Unordered) {
            return self.pick_mutant(sig, need);
        }
        if u64::from(top) * CPU >= need[0] {
            let r = s.ix.head(top);
            if cpu_only || self.rest_fits_at(r, need) {
                return r;
            }
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
                if self.peers.get(r as usize).is_some_and(|p| p.view >= need[0]) && self.rest_fits_at(r, need) {
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

    /// The pick's walk: bucket by bucket from `top` down to the lowest that could hold a fit,
    /// member by member, each checked exactly.
    fn walk(&mut self, sig: u32, need: &Amounts, top: u32) -> u32 {
        let floor = u32::try_from(need[0] / CPU).unwrap_or(u32::MAX);
        let mut walked = 0u64;
        let mut found = NIL;
        if let Some(s) = self.sigs.get(sig as usize) {
            let mut b = top;
            'buckets: while b >= floor {
                let mut r = s.ix.head(b);
                while r != NIL {
                    if self.peers.get(r as usize).is_some_and(|p| p.view >= need[0]) && (!multi(need) || self.rest_fits_at(r, need)) {
                        found = r;
                        break 'buckets;
                    }
                    walked += 1;
                    r = s.ix.next(r);
                }
                match s.ix.below(b) {
                    Some(n) => b = n,
                    None => break,
                }
            }
        }
        self.counts.walked = self.counts.walked.saturating_add(walked);
        found
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

    /// Takes what it sends runner `r` out of its view at once, and moves `r` to the tail of its
    /// new bucket (it took its turn). The epoch to send under, and the ends to acknowledge.
    #[inline]
    fn deduct(&mut self, r: u32, need: &Amounts, cpu_only: bool) -> (u32, u32) {
        let last = self.last();
        let Some(p) = self.peers.get_mut(r as usize) else { return (0, 0) };
        #[cfg(test)]
        if super::planted(super::Mutant::NoDeduct) {
            return (p.epoch, p.heard);
        }
        p.view = p.view.saturating_sub(need[0]);
        p.inflight = p.inflight.saturating_add(need[0]);
        let (epoch, ack, from, to, sig0, more) = (p.epoch, p.heard, p.bucket, bucket(p.view, last), p.sig0, p.flags & MORE != 0);
        p.bucket = to;
        if !cpu_only && let Some(c) = self.cold.get_mut(r as usize) {
            for (f, n) in c.inflight_rest.iter_mut().zip(rest(need)) {
                *f = f.saturating_add(n);
            }
        }
        self.relink_all(r, sig0, more, from, to);
        (epoch, ack)
    }

    /// Moves `r` from bucket `from` to the tail of `to` in every index it's in.
    #[inline]
    fn relink_all(&mut self, r: u32, sig0: u32, more: bool, from: u32, to: u32) {
        if let Some(s) = self.sigs.get_mut(sig0 as usize) {
            s.ix.relink(r, from, to);
        }
        if more {
            let list = self.more.get(r as usize).map_or(&[][..], |v| v.as_slice());
            for &o in list {
                if let Some(s) = self.sigs.get_mut(o as usize) {
                    s.ix.relink(r, from, to);
                }
            }
        }
    }

    /// Takes runner `r`'s word into its view: its room less what is in flight there; and moves
    /// it if its bucket changed (a rise tries the runs waiting for its signatures).
    #[inline]
    fn apply(&mut self, r: u32, word: &Word, out: &mut Out) {
        let last = self.last();
        let Some(p) = self.peers.get_mut(r as usize) else { return };
        p.view = word.cpu.saturating_sub(p.inflight);
        // Memory, disk and GPUs are read only when they aren't all free.
        if !(word.whole && p.flags & WHOLE != 0) {
            if word.whole {
                p.flags |= WHOLE;
            } else {
                p.flags &= !WHOLE;
            }
            if let Some(c) = self.cold.get_mut(r as usize) {
                c.word_rest = word.rest;
            }
        }
        if p.flags & UP == 0 {
            return;
        }
        let (from, to) = (p.bucket, bucket(p.view, last));
        if from == to {
            return;
        }
        p.bucket = to;
        let (sig0, more) = (p.sig0, p.flags & MORE != 0);
        self.relink_all(r, sig0, more, from, to);
        if to > from && self.waiting != 0 {
            self.rise(r, out);
        }
    }

    /// A word from runner `r`: false when it is to be ignored (an unknown runner, or an epoch
    /// older than the one known). A newer epoch means the runner started again, and lost every
    /// run it had.
    #[inline]
    fn fresh(&mut self, r: u32, word: &Word, out: &mut Out) -> bool {
        let Some(p) = self.peers.get(r as usize) else {
            self.counts.stale = self.counts.stale.saturating_add(1);
            return false;
        };
        if p.flags & KNOWN == 0 || word.epoch < p.epoch {
            self.counts.stale = self.counts.stale.saturating_add(1);
            return false;
        }
        if word.epoch > p.epoch {
            self.restarted(r, word.epoch, out);
        }
        true
    }

    /// Runner `r` started again under `epoch`: what ran there is lost, and what waited there
    /// waits elsewhere. Starts and Gos sent under the old epoch come back declined.
    fn restarted(&mut self, r: u32, epoch: u32, out: &mut Out) {
        if let Some(p) = self.peers.get_mut(r as usize) {
            p.epoch = epoch;
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
                out.told.push(Told::Lost { run: h });
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
    #[inline]
    pub fn hear(&mut self, msg: ToOwner, out: &mut Out) -> Option<Told> {
        match msg {
            ToOwner::Started { runner, run, at, word } => self.started(runner.0, run, at, &word, out),
            ToOwner::Ended { runner, run, word, seq } => self.ended(runner.0, run, seq, &word, out),
            ToOwner::Declined { runner, run, word, why, of } => {
                self.declined(runner.0, run, &word, why, of, out);
                None
            }
            ToOwner::Queued { runner, run, at, word } => {
                self.queued(runner.0, run, at, &word, out);
                None
            }
            ToOwner::Ready { runner, run, at, word } => {
                self.ready(runner.0, run, at, &word, out);
                None
            }
            ToOwner::Hint { runner, word } => {
                if self.fresh(runner.0, &word, out) {
                    self.apply(runner.0, &word, out);
                }
                None
            }
            ToOwner::Synced { runner, word } => {
                self.synced(runner.0, &word, out);
                None
            }
        }
    }

    #[inline]
    fn started(&mut self, r: u32, run: Handle, at: Handle, word: &Word, out: &mut Out) -> Option<Told> {
        if !self.fresh(r, word, out) {
            return None;
        }
        let mut told = None;
        // A start in flight there, answered (its CPU, its slow record); a slow record to free.
        let mut answered = None;
        let mut slow = NIL;
        match self.runs.get_mut(run) {
            Some(x) if x.runner == r => match x.state & STATE {
                STARTING => {
                    answered = Some((x.cpu, x.slow));
                    slow = x.slow;
                    *x = Run { state: STARTED, at, slow: NIL, ..*x };
                    told = Some(Told::Started { run, runner: RunnerId(r) });
                }
                GOING => {
                    // Its room was held, never in flight: nothing to answer.
                    slow = x.slow;
                    *x = Run { state: STARTED, at, slow: NIL, ..*x };
                    told = Some(Told::Started { run, runner: RunnerId(r) });
                }
                // Said again as its link came back: no longer unsure.
                STARTED if x.at == at => x.state = STARTED,
                _ => self.counts.stale = self.counts.stale.saturating_add(1),
            },
            _ => self.counts.stale = self.counts.stale.saturating_add(1),
        }
        if let Some((cpu, s)) = answered {
            self.answered(r, cpu, s);
        }
        if slow != NIL {
            self.free_slow(slow);
        }
        self.apply(r, word, out);
        told
    }

    /// A start sent to `r` is answered: what it asked is no longer in flight there.
    #[inline]
    fn answered(&mut self, r: u32, cpu: u64, slow: u32) {
        if let Some(p) = self.peers.get_mut(r as usize) {
            p.inflight = p.inflight.saturating_sub(cpu);
        }
        if let Some(rest) = self.slow.at(slow).map(|s| s.rest)
            && let Some(c) = self.cold.get_mut(r as usize)
        {
            for (f, n) in c.inflight_rest.iter_mut().zip(rest) {
                *f = f.saturating_sub(n);
            }
        }
    }

    #[inline]
    fn ended(&mut self, r: u32, run: Handle, seq: u32, word: &Word, out: &mut Out) -> Option<Told> {
        if !self.fresh(r, word, out) {
            return None;
        }
        if let Some(p) = self.peers.get_mut(r as usize)
            && (seq.wrapping_sub(p.heard) as i32) > 0
        {
            p.heard = seq;
        }
        let told = match self.runs.get(run).copied() {
            Some(x) if x.runner == r && x.state & STATE == STARTED => {
                self.free_run(run);
                Some(Told::Ended { run })
            }
            // Its start's answer was lost with the link, and it ran and ended meanwhile.
            Some(x) if x.runner == r && x.state & UNSURE != 0 && matches!(x.state & STATE, STARTING | GOING) => {
                if x.state & STATE == STARTING {
                    self.answered(r, x.cpu, x.slow);
                }
                self.free_run(run);
                Some(Told::Ended { run })
            }
            _ => {
                self.counts.stale = self.counts.stale.saturating_add(1);
                None
            }
        };
        self.apply(r, word, out);
        told
    }

    fn declined(&mut self, r: u32, run: Handle, word: &Word, why: Why, of: Of, out: &mut Out) {
        if !self.fresh(r, word, out) {
            return;
        }
        self.counts.declined = self.counts.declined.saturating_add(1);
        let Some(x) = self.runs.get(run).copied() else {
            self.counts.stale = self.counts.stale.saturating_add(1);
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
                let _ = why;
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
                self.counts.stale = self.counts.stale.saturating_add(1);
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
        let epoch = self.peers.get(r as usize).map_or(0, |p| p.epoch);
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

    /// Runner `r`'s link is back and it has said every live run of this owner's there again:
    /// what this owner was unsure of there is settled, and the runner is in its buckets again.
    fn synced(&mut self, r: u32, word: &Word, out: &mut Out) {
        if !self.fresh(r, word, out) {
            return;
        }
        let mut again = Vec::new();
        for h in self.runs.handles() {
            let Some(x) = self.runs.get(h).copied() else { continue };
            if x.runner != r || x.state & UNSURE == 0 {
                continue;
            }
            match x.state & STATE {
                STARTING => {
                    self.answered(r, x.cpu, x.slow);
                    self.park(h);
                    again.push(h);
                }
                GOING => {
                    self.park(h);
                    again.push(h);
                }
                STARTED => {
                    // It wasn't said again: it ended while the link was down.
                    self.free_run(h);
                    out.told.push(Told::Ended { run: h });
                }
                _ => {}
            }
        }
        let last = self.last();
        let Some(p) = self.peers.get_mut(r as usize) else { return };
        let was_up = p.flags & UP != 0;
        p.flags |= UP;
        p.view = word.cpu.saturating_sub(p.inflight);
        if word.whole {
            p.flags |= WHOLE;
        } else {
            p.flags &= !WHOLE;
        }
        let (from, to, sig0) = (p.bucket, bucket(p.view, last), p.sig0);
        p.bucket = to;
        if let Some(c) = self.cold.get_mut(r as usize) {
            c.word_rest = word.rest;
        }
        let list: Vec<u32> = std::iter::once(sig0).chain(self.more.get(r as usize).cloned().unwrap_or_default()).filter(|s| *s != NIL).collect();
        for s in list {
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
        let ri = r.0;
        let Some(p) = self.peers.get_mut(ri as usize) else { return };
        if p.flags & UP == 0 {
            return;
        }
        p.flags &= !UP;
        let (b, sig0) = (p.bucket, p.sig0);
        p.bucket = NIL;
        let list: Vec<u32> = std::iter::once(sig0).chain(self.more.get(ri as usize).cloned().unwrap_or_default()).filter(|s| *s != NIL).collect();
        for s in list {
            if let Some(s) = self.sigs.get_mut(s as usize) {
                s.ix.unlink(ri, b);
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
        let (epoch, ack) = self.deduct(r, &need, cpu_only);
        if let Some(y) = self.runs.get_mut(run) {
            y.state = STARTING;
            y.runner = r;
            y.at = Handle::NONE;
        }
        out.to.push((RunnerId(r), ToRunner::Start(Start { owner: self.id, run, epoch, need, ack })));
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
                        *p = Place { runner: r, at: Handle::NONE, epoch: self.peers.get(r as usize).map_or(0, |q| q.epoch) };
                    }
                    n += 1;
                    if n == PLACES {
                        break 'find;
                    }
                }
            }
            b = s.ix.below(bb);
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
            let (epoch, ack) = self.peers.get(p.runner as usize).map_or((0, 0), |q| (q.epoch, q.heard));
            out.to.push((RunnerId(p.runner), ToRunner::Wait(Start { owner: self.id, run, epoch, need, ack })));
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
            let (epoch, ack) = self.deduct(r, &need, cpu_only);
            if let Some(y) = self.runs.get_mut(run) {
                y.state = STARTING;
                y.runner = r;
            }
            out.to.push((RunnerId(r), ToRunner::Start(Start { owner: self.id, run, epoch, need, ack })));
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
            x.state = WAITING;
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

    /// Its view of runner `r`: what it takes `r` to have free for a new run (`None`: not known,
    /// or its link is down).
    pub fn view(&self, r: RunnerId) -> Option<Amounts> {
        let p = self.peers.get(r.0 as usize).filter(|p| p.flags & UP != 0)?;
        let c = self.cold.get(r.0 as usize)?;
        let free = if p.flags & WHOLE != 0 { rest(&c.budget) } else { c.word_rest };
        let [a, b, d] = free;
        let [ia, ib, id] = c.inflight_rest;
        Some([p.view, a.saturating_sub(ia), b.saturating_sub(ib), d.saturating_sub(id)])
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
        let mut v = Vec::new();
        let mut b = s.ix.top();
        while let Some(bb) = b {
            v.extend(s.ix.bucket(bb).map(RunnerId));
            b = s.ix.below(bb);
        }
        v
    }
}

impl Slow {
    fn of(need: &Amounts) -> Slow {
        Slow { rest: rest(need), places: [NO_PLACE; PLACES], prev: NIL, next: NIL, listed: false }
    }
}
