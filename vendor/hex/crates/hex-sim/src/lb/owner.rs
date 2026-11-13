//! The owner's side (kiln `crates/kiln-ci/src/lb/owner.rs` at f20858c9c): the node (or plane) that
//! asks for a box owns it until its node says it started, waits or never can, and decides where it
//! goes.
//!
//! Its view of a node is that node's last word less what the owner has sent it since and not heard
//! back about (in flight): it deducts what it sends at once, so starts in flight never herd onto
//! one node. Kiln picks the member with the most room (least loaded first), which spreads jobs
//! that end soon. A box holds its room until it is deleted, so Hex's pick packs: the member with
//! the least room that holds the need, bucket by bucket from the need's own up, every dimension
//! checked exactly at the view; equals take turns from a cursor each owner starts at its own place.
//!
//! Off that path: a box no member has room for waits at nodes, not here: at the two members nearest
//! to fitting, the first to hold its room wins and the other is withdrawn, so it never starts
//! twice. A node's decline carries its room, so the next pick never repeats the miss, and the loop
//! ends when no member fits (then the box waits): it is bounded by the members. A link that drops
//! leaves the owner unsure of what it had there until the node says it all again (Synced, with how
//! many of its boxes have ended there). A node whose agent started again (a newer epoch in its
//! word) says again the boxes that outlived it (Hex's adaptation: kiln's runner lost every job).

use super::index::{BUCKETS, Index};
use super::msg::{Of, Start, ToOwner, ToRunner, Why, Word};
use super::slab::{Handle, Slab};
use super::{
    Amounts, CPU, NIL, OwnerId, RunnerId, SigId, cpu_of, ix, multi, rest, rest_fits, within,
};

/// How many nodes a box that finds no room waits at: two. With two choices the most loaded of n
/// bins holds log log n / ln 2 + O(1) instead of Θ(log n / log log n) (Azar, Broder, Karlin and
/// Upfal, SIAM J. Comput. 29(1), 1999); for queues the second choice is where the exponential gain
/// is (Mitzenmacher, IEEE TPDS 12(10), 2001); Sparrow measured two probes a task with late binding
/// as its best trade of messages for waiting (Ousterhout et al., SOSP 2013).
pub const PLACES: usize = 2;

/// The rank key's multiplier at the reference speed: ceil(2^40 / CPU), so `(view * SCALE) >> 40`
/// is `view / CPU`, whole free CPUs, for every u32 view: a multiply and a shift, no divide. A
/// node's measured per-core speed scales it (core-equivalents).
pub const SCALE: u32 = 1_099_511_628;

/// A node's speed in its word is in 1/1024 of the reference core's (0: not measured).
pub const SPEED_ONE: u32 = 1024;

/// Spreads owners' cursors over the fleet: floor(2^32 / φ), Fibonacci hashing (Knuth, TAOCP vol.
/// 3, §6.4: successive multiples of it modulo 2^32 fall as evenly as any sequence can).
const SPREAD: u32 = 2_654_435_769;

// A node's flags at its owner.
const KNOWN: u32 = 1;
/// Its link is up and synced: it is in its indexes' buckets.
const UP: u32 = 2;
/// Its last word had all of its memory, disk and GPUs free.
const WHOLE: u32 = 4;
/// It is a member of more than one index.
const MORE: u32 = 8;

// A box's states at its owner.
const STARTING: u32 = 1;
const STARTED: u32 = 2;
const WAITING: u32 = 3;
const GOING: u32 = 4;
const STATE: u32 = 7;
/// Its node's link dropped (or its agent started again) while it was starting, started or going
/// there: what became of it waits for the node's Synced.
const UNSURE: u32 = 8;
/// Its asker deleted it while its start or Go was in flight: told when it starts (the shell
/// deletes it there), forgotten if it doesn't, never placed again.
const CANCELLED: u32 = 16;

/// What the owner keeps of each node, all that a decision or a word reads of it.
#[derive(Debug, Clone, Copy)]
struct Peer {
    /// CPU free as this owner sees it, in thousandths: its last word less `inflight`.
    view: u32,
    /// CPU of the starts sent to it and not heard back about.
    inflight: u32,
    epoch: u32,
    /// How many of this owner's boxes it has said ended.
    heard: u32,
    /// Its rank key's multiplier ([`SCALE`] at the reference speed).
    scale: u32,
    /// The first index it's a member of (NIL: none); the rest are in `more`.
    sig0: u32,
    flags: u32,
    /// Its per-core speed as its word last said it (0: not measured).
    speed: u32,
}

const PEER: Peer = Peer {
    view: 0,
    inflight: 0,
    epoch: 0,
    heard: 0,
    scale: SCALE,
    sig0: NIL,
    flags: 0,
    speed: 0,
};

/// What the owner keeps of each node that only a need of memory, disk or GPUs reads.
#[derive(Debug, Clone, Copy, Default)]
struct Cold {
    budget: Amounts,
    /// Memory, disk and GPUs free by its last word (when they weren't all free).
    word_rest: [u64; 3],
    inflight_rest: [u64; 3],
}

/// A box. What it needs beyond CPU, and where it waits, are in its [`Slow`] record.
#[derive(Debug, Clone, Copy)]
struct Run {
    cpu: u64,
    /// Its handle at its node: an admitted box's there, or the room held for it.
    at: Handle,
    sig: u32,
    runner: u32,
    state: u32,
    /// Its slow record's slot (NIL: none).
    slow: u32,
}

/// A place a box waits at: the node, its handle in that node's line once the node has said it
/// (`Handle::NONE` until then), and the node's epoch its Wait went under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Place {
    runner: u32,
    at: Handle,
    epoch: u32,
}

const NO_PLACE: Place = Place {
    runner: NIL,
    at: Handle::NONE,
    epoch: 0,
};

/// What a box needs of memory, disk and GPUs, and where it waits.
#[derive(Debug, Clone, Copy)]
struct Slow {
    rest: [u64; 3],
    places: [Place; PLACES],
    /// Its neighbours in its signature's waiting boxes (run slots), while it is in them.
    prev: u32,
    next: u32,
    listed: bool,
}

/// One signature: its index, and its boxes waiting for room, oldest first.
#[derive(Debug, Clone)]
struct Sig {
    ix: Index,
    wait_head: u32,
    wait_tail: u32,
    waiting: u32,
}

/// What an owner tells the askers of its boxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Told {
    /// It was created and started on that node, by that node's word.
    Started { run: Handle, runner: RunnerId },
    /// No member has room for it now: it waits for room at the nodes its owner chose, or for a
    /// member to come back.
    Waiting { run: Handle },
    /// No member could ever take it: it needs more than any member's whole budget.
    Never { run: Handle },
    /// It was deleted (or exited) on its node.
    Ended { run: Handle },
    /// It is gone, and may or may not have run: its node left while it ran there, or while what
    /// it said of it was lost. Never started twice: the asker decides.
    Lost { run: Handle },
}

/// A box's state at its owner, as tests and explanations read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Its start went to `runner`, unanswered.
    Starting { runner: RunnerId },
    /// It runs on `runner` (`at` there).
    Started { runner: RunnerId, at: Handle },
    /// It waits at these nodes (none: no member is reachable).
    Waiting { at: [Option<RunnerId>; PLACES] },
    /// `runner` holds its room, and the owner said Go.
    Going { runner: RunnerId },
}

/// Why an ask, or a node or signature, wasn't taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// No such signature at this owner.
    NoSig,
    /// No member could ever take it: it needs more than any member's whole budget.
    Never,
    /// The owner holds 2^32 - 1 boxes, signatures or nodes already, or its tables can't grow to
    /// hold more on this target.
    Full,
}

/// An ask taken: its box's handle, and the start to send when a member has room for it now
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

/// What happened off the first pick, counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Members a pick passed over: short of what's asked.
    pub walked: u64,
    /// Starts and waits nodes declined.
    pub declined: u64,
    /// Boxes sent to wait.
    pub waited: u64,
    /// Words ignored: from an epoch older than the one known, or about no box of this owner's.
    pub stale: u64,
}

/// The owner: its nodes, signatures and boxes.
#[derive(Debug, Clone)]
pub struct Owner {
    id: OwnerId,
    peers: Vec<Peer>,
    cold: Vec<Cold>,
    /// Each node's indexes past its first.
    more: Vec<Vec<u32>>,
    sigs: Vec<Sig>,
    runs: Slab<Run>,
    slow: Slab<Slow>,
    /// How many buckets each index has: the most whole CPUs any node has, plus one, at most
    /// [`BUCKETS`].
    buckets: u32,
    /// Boxes waiting, over every signature.
    waiting: u32,
    /// Some node's key is scaled by a measured speed: bucket lower bounds don't promise CPUs.
    scaled: bool,
    pub counts: Counts,
}

/// The bucket of a view: its key, at most `last`.
#[inline(always)]
fn key(view: u32, scale: u32, last: u32) -> u32 {
    let k = u64::from(view)
        .saturating_mul(u64::from(scale))
        .wrapping_shr(40);
    u32::try_from(k).unwrap_or(u32::MAX).min(last)
}

/// A u64 amount as a view (saturating: views top out at 4,294,967 thousandths, 4,294 CPUs; a
/// bigger node admits exactly, its owner just sees less of it).
#[inline(always)]
fn narrow(x: u64) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

/// The buckets a node of `budget` needs: its whole CPUs and one, at most [`BUCKETS`].
fn buckets_for(budget: &Amounts) -> u32 {
    let whole = cpu_of(budget)
        .checked_div(CPU)
        .unwrap_or(0)
        .saturating_add(1);
    u32::try_from(whole).unwrap_or(BUCKETS).min(BUCKETS)
}

/// The lowest bucket a need of `cpu` can be in at the reference speed: its whole CPUs.
fn floor_of(cpu: u64) -> u32 {
    u32::try_from(cpu.checked_div(CPU).unwrap_or(0)).unwrap_or(u32::MAX)
}

impl Owner {
    pub fn new(id: OwnerId) -> Owner {
        Owner {
            id,
            peers: Vec::new(),
            cold: Vec::new(),
            more: Vec::new(),
            sigs: Vec::new(),
            runs: Slab::new(),
            slow: Slab::new(),
            buckets: 1,
            waiting: 0,
            scaled: false,
            counts: Counts::default(),
        }
    }

    pub fn id(&self) -> OwnerId {
        self.id
    }

    /// Room for `runs` boxes before its tables grow.
    pub fn reserve(&mut self, runs: usize) {
        self.runs = Slab::with_capacity(runs);
    }

    #[inline(always)]
    fn last(&self) -> u32 {
        self.buckets.saturating_sub(1)
    }

    /// Grows every index to `buckets` buckets and the fleet it has; refused when one can't.
    fn grow_all(&mut self) -> Result<(), Refused> {
        let (buckets, runners) = (self.buckets, u32::try_from(self.peers.len()).unwrap_or(NIL));
        for s in &mut self.sigs {
            if !s.ix.grow(buckets, runners) {
                return Err(Refused::Full);
            }
        }
        Ok(())
    }

    /// Takes node `r` into its fleet with its whole budget and its first word. A node it knew
    /// already leaves first (its boxes there are lost).
    pub fn join(
        &mut self,
        r: RunnerId,
        budget: Amounts,
        word: Word,
        out: &mut Out,
    ) -> Result<(), Refused> {
        let i = ix(r.0);
        if self.peers.get(i).is_some_and(|p| p.flags & KNOWN != 0) {
            self.leave(r, out);
        }
        if self.peers.len() <= i {
            let n = i.checked_add(1).ok_or(Refused::Full)?;
            self.peers.resize(n, PEER);
            self.cold.resize(n, Cold::default());
            self.more.resize(n, Vec::new());
        }
        self.buckets = self.buckets.max(buckets_for(&budget));
        self.grow_all()?;
        let scale = scale_of(word.speed);
        self.scaled |= scale != SCALE;
        if let Some(p) = self.peers.get_mut(i) {
            *p = Peer {
                view: narrow(word.cpu),
                inflight: 0,
                epoch: word.epoch,
                heard: 0,
                scale,
                sig0: NIL,
                flags: KNOWN | UP | if word.whole { WHOLE } else { 0 },
                speed: word.speed,
            };
        }
        if let Some(c) = self.cold.get_mut(i) {
            *c = Cold {
                budget,
                word_rest: word.rest,
                inflight_rest: [0; 3],
            };
        }
        Ok(())
    }

    /// A new signature, with no members yet. Its cursors start at this owner's place in the fleet.
    pub fn sig(&mut self) -> Result<SigId, Refused> {
        let id = u32::try_from(self.sigs.len())
            .ok()
            .filter(|i| *i != NIL)
            .ok_or(Refused::Full)?;
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut start = self
            .id
            .0
            .wrapping_mul(SPREAD)
            .wrapping_add(id.wrapping_mul(SPREAD.rotate_left(16)));
        #[cfg(test)]
        if super::planted(super::Mutant::NoSpread) {
            start = 0;
        }
        let mut ix = Index::new(start);
        if !ix.grow(self.buckets, u32::try_from(self.peers.len()).unwrap_or(NIL)) {
            return Err(Refused::Full);
        }
        self.sigs.push(Sig {
            ix,
            wait_head: NIL,
            wait_tail: NIL,
            waiting: 0,
        });
        Ok(SigId(id))
    }

    /// Node `r` can run signature `sig`'s boxes: a member from now on. Boxes waiting for it are
    /// tried on it.
    pub fn add(&mut self, sig: SigId, r: RunnerId, out: &mut Out) -> Result<(), Refused> {
        let ri = r.0;
        let Some(p) = self
            .peers
            .get(ix(ri))
            .copied()
            .filter(|p| p.flags & KNOWN != 0)
        else {
            return Ok(());
        };
        let budget = self.cold.get(ix(ri)).map_or([0; 4], |c| c.budget);
        let last = self.last();
        let s = self.sigs.get_mut(ix(sig.0)).ok_or(Refused::NoSig)?;
        if s.ix.contains(ri) {
            return Ok(());
        }
        if !s.ix.join(ri, &budget) {
            return Err(Refused::Full);
        }
        if p.flags & UP != 0 {
            s.ix.push(ri, key(p.view, p.scale, last));
        }
        if p.sig0 == NIL {
            if let Some(p) = self.peers.get_mut(ix(ri)) {
                p.sig0 = sig.0;
            }
        } else {
            if let Some(m) = self.more.get_mut(ix(ri)) {
                m.push(sig.0);
            }
            if let Some(p) = self.peers.get_mut(ix(ri)) {
                p.flags |= MORE;
            }
        }
        if self.waiting != 0 {
            self.retry(sig.0, out);
            self.unpark(ri, out);
        }
        Ok(())
    }

    /// Node `r` can no longer run signature `sig`'s boxes.
    pub fn remove(&mut self, sig: SigId, r: RunnerId) {
        let ri = r.0;
        let Some(p) = self.peers.get(ix(ri)).copied() else {
            return;
        };
        let budget = self.cold.get(ix(ri)).map_or([0; 4], |c| c.budget);
        let last = self.last();
        let Some(s) = self.sigs.get_mut(ix(sig.0)) else {
            return;
        };
        if !s.ix.contains(ri) {
            return;
        }
        if p.flags & UP != 0 {
            s.ix.unlink(ri, key(p.view, p.scale, last));
        }
        s.ix.part(ri, &budget);
        let more = self.more.get_mut(ix(ri));
        if p.sig0 == sig.0 {
            // Its next index becomes its first.
            let next = more.and_then(|m| {
                if m.is_empty() {
                    None
                } else {
                    Some(m.remove(0))
                }
            });
            if let Some(p) = self.peers.get_mut(ix(ri)) {
                p.sig0 = next.unwrap_or(NIL);
            }
        } else if let Some(m) = more {
            m.retain(|s| *s != sig.0);
        }
        let more_left = self.more.get(ix(ri)).is_some_and(|m| !m.is_empty());
        if let Some(p) = self.peers.get_mut(ix(ri)) {
            if more_left {
                p.flags |= MORE;
            } else {
                p.flags &= !MORE;
            }
        }
    }

    /// Node `r` is gone from the fleet: out of every index, its boxes lost, and the boxes waiting
    /// there placed again.
    pub fn leave(&mut self, r: RunnerId, out: &mut Out) {
        let ri = r.0;
        let Some(p) = self
            .peers
            .get(ix(ri))
            .copied()
            .filter(|p| p.flags & KNOWN != 0)
        else {
            return;
        };
        for s in self.sigs_of(ri, &p) {
            self.remove(SigId(s), r);
        }
        if let Some(p) = self.peers.get_mut(ix(ri)) {
            *p = PEER;
        }
        let mut again = Vec::new();
        for h in self.runs.handles() {
            let Some(x) = self.runs.get(h).copied() else {
                continue;
            };
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

    /// The indexes node `r` is a member of.
    fn sigs_of(&self, r: u32, p: &Peer) -> Vec<u32> {
        std::iter::once(p.sig0)
            .chain(self.more.get(ix(r)).cloned().unwrap_or_default())
            .filter(|s| *s != NIL)
            .collect()
    }

    /// Decides where a box needing `need` of signature `sig`'s members goes: the start to send when
    /// one has room for it now, else it waits (what to send for that is in `out`). Refused when no
    /// member could ever take it.
    pub fn ask(&mut self, sig: SigId, need: Amounts, out: &mut Out) -> Result<Asked, Refused> {
        let si = sig.0;
        if self.sigs.get(ix(si)).is_none() {
            return Err(Refused::NoSig);
        }
        let cpu_only = !multi(&need);
        let r = self.pick(si, &need, cpu_only);
        if r == NIL {
            return self.ask_waits(si, need, out);
        }
        let slow = if cpu_only { NIL } else { self.slow_for(&need)? };
        let Some(run) = self.runs.insert(Run {
            cpu: cpu_of(&need),
            at: Handle::NONE,
            sig: si,
            runner: r,
            state: STARTING,
            slow,
        }) else {
            self.free_slow(slow);
            return Err(Refused::Full);
        };
        let epoch = self.deduct(r, &need, cpu_only);
        Ok(Asked {
            run,
            send: Some((
                RunnerId(r),
                ToRunner::Start(Start {
                    owner: self.id,
                    run,
                    epoch,
                    need,
                }),
            )),
        })
    }

    fn slow_for(&mut self, need: &Amounts) -> Result<u32, Refused> {
        Ok(self.slow.insert(Slow::of(need)).ok_or(Refused::Full)?.slot)
    }

    /// The ask found no member with room now: it waits, or never fits.
    fn ask_waits(&mut self, si: u32, need: Amounts, out: &mut Out) -> Result<Asked, Refused> {
        let Some(s) = self.sigs.get(ix(si)) else {
            return Err(Refused::NoSig);
        };
        if !s.ix.could_fit(&need) {
            return Err(Refused::Never);
        }
        let slow = self.slow_for(&need)?;
        let Some(run) = self.runs.insert(Run {
            cpu: cpu_of(&need),
            at: Handle::NONE,
            sig: si,
            runner: NIL,
            state: WAITING,
            slow,
        }) else {
            self.free_slow(slow);
            return Err(Refused::Full);
        };
        self.wait(run, out, false);
        Ok(Asked { run, send: None })
    }

    /// The member a need goes to (NIL: none fits): Hex's pick packs, so it is the next in turn of
    /// the lowest bucket that could hold the need, if it fits (CPU at the view, every other
    /// dimension exactly), else a walk upward. Kiln's took the highest bucket's (its `MostLoaded`
    /// mutant is Hex's planted `MostFree`).
    fn pick(&mut self, sig: u32, need: &Amounts, cpu_only: bool) -> u32 {
        #[cfg(test)]
        if super::planted(super::Mutant::MostFree) || super::planted(super::Mutant::Unordered) {
            return self.pick_mutant(sig, need);
        }
        let floor = if self.scaled {
            0
        } else {
            floor_of(cpu_of(need))
        };
        let Some(s) = self.sigs.get(ix(sig)) else {
            return NIL;
        };
        let Some(low) = s.ix.at_or_above(floor) else {
            return NIL;
        };
        let r = s.ix.next_in(low);
        let fits = self
            .peers
            .get(ix(r))
            .is_some_and(|p| u64::from(p.view) >= cpu_of(need));
        if fits && (cpu_only || self.rest_fits_at(r, need)) {
            if let Some(s) = self.sigs.get_mut(ix(sig)) {
                s.ix.took(low, r);
            }
            return r;
        }
        self.walk(sig, need, low)
    }

    /// The pick's planted bugs: the member with the most room that fits (kiln's least loaded), or a
    /// tie broken by a counter outside the owner.
    #[cfg(test)]
    fn pick_mutant(&mut self, sig: u32, need: &Amounts) -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static TURN: AtomicU32 = AtomicU32::new(0);
        let Some(s) = self.sigs.get(ix(sig)) else {
            return NIL;
        };
        let mut fits: Vec<(u32, u32)> = Vec::new();
        let mut b = s.ix.at_or_above(0);
        while let Some(bb) = b {
            for r in s.ix.bucket(bb) {
                if self
                    .peers
                    .get(ix(r))
                    .is_some_and(|p| u64::from(p.view) >= cpu_of(need))
                    && self.rest_fits_at(r, need)
                {
                    fits.push((bb, r));
                }
            }
            b = s.ix.above(bb);
        }
        if super::planted(super::Mutant::MostFree) {
            return fits.last().map_or(NIL, |f| f.1);
        }
        let Some(&(low, _)) = fits.first() else {
            return NIL;
        };
        let ties: Vec<u32> = fits.iter().filter(|f| f.0 == low).map(|f| f.1).collect();
        let turn = TURN.fetch_add(1, Ordering::Relaxed) as usize;
        ties.get(turn % ties.len().max(1)).copied().unwrap_or(NIL)
    }

    /// The pick's walk: bucket by bucket from `low` up, member by member in turn, each checked
    /// exactly; the first that fits is the one with the least room that holds the need.
    fn walk(&mut self, sig: u32, need: &Amounts, low: u32) -> u32 {
        let mut walked = 0u64;
        let mut found = (NIL, 0u32);
        if let Some(s) = self.sigs.get(ix(sig)) {
            let mut b = Some(low);
            'buckets: while let Some(bb) = b {
                for r in s.ix.bucket(bb) {
                    if self
                        .peers
                        .get(ix(r))
                        .is_some_and(|p| u64::from(p.view) >= cpu_of(need))
                        && (!multi(need) || self.rest_fits_at(r, need))
                    {
                        found = (r, bb);
                        break 'buckets;
                    }
                    walked = walked.saturating_add(1);
                }
                b = s.ix.above(bb);
            }
        }
        self.counts.walked = self.counts.walked.saturating_add(walked);
        if found.0 != NIL
            && let Some(s) = self.sigs.get_mut(ix(sig))
        {
            s.ix.took(found.1, found.0);
        }
        found.0
    }

    /// Whether `need`'s memory, disk and GPUs fit node `r` at this owner's view.
    #[inline]
    fn rest_fits_at(&self, r: u32, need: &Amounts) -> bool {
        let (Some(p), Some(c)) = (self.peers.get(ix(r)), self.cold.get(ix(r))) else {
            return false;
        };
        let free = if p.flags & WHOLE != 0 {
            rest(&c.budget)
        } else {
            c.word_rest
        };
        let [a, b, d] = free;
        let [ia, ib, id] = c.inflight_rest;
        rest_fits(
            &rest(need),
            &[
                a.saturating_sub(ia),
                b.saturating_sub(ib),
                d.saturating_sub(id),
            ],
        )
    }

    /// Takes what it sends node `r` out of its view at once, and moves `r` to its new bucket. The
    /// epoch to send under.
    fn deduct(&mut self, r: u32, need: &Amounts, cpu_only: bool) -> u32 {
        let last = self.last();
        let Some(p) = self.peers.get_mut(ix(r)) else {
            return 0;
        };
        #[cfg(test)]
        if super::planted(super::Mutant::NoDeduct) {
            return p.epoch;
        }
        let cpu = narrow(cpu_of(need));
        let from = key(p.view, p.scale, last);
        p.view = p.view.saturating_sub(cpu);
        p.inflight = p.inflight.saturating_add(cpu);
        let (epoch, to, sig0, more) = (
            p.epoch,
            key(p.view, p.scale, last),
            p.sig0,
            p.flags & MORE != 0,
        );
        if !cpu_only && let Some(c) = self.cold.get_mut(ix(r)) {
            for (f, n) in c.inflight_rest.iter_mut().zip(rest(need)) {
                *f = f.saturating_add(n);
            }
        }
        if from != to {
            self.relink_all(r, sig0, more, from, to);
        }
        epoch
    }

    /// Moves `r` from bucket `from` to bucket `to` in every index it's in.
    fn relink_all(&mut self, r: u32, sig0: u32, more: bool, from: u32, to: u32) {
        if let Some(s) = self.sigs.get_mut(ix(sig0)) {
            s.ix.relink(r, from, to);
        }
        if more {
            let list = self.more.get(ix(r)).map_or(&[][..], |v| v.as_slice());
            for &o in list {
                if let Some(s) = self.sigs.get_mut(ix(o)) {
                    s.ix.relink(r, from, to);
                }
            }
        }
    }

    /// Takes node `r`'s word into its view: its room less what is in flight there; and moves it if
    /// its bucket changed (a rise tries the boxes waiting for its signatures).
    fn apply(&mut self, r: u32, word: &Word, out: &mut Out) {
        let last = self.last();
        let Some(p) = self.peers.get_mut(ix(r)) else {
            return;
        };
        let from = key(p.view, p.scale, last);
        p.view = narrow(word.cpu).saturating_sub(p.inflight);
        if word.whole {
            p.flags |= WHOLE;
        } else {
            p.flags &= !WHOLE;
        }
        if word.speed != p.speed {
            // A new multiplier: it moves from its old bucket to the one it gives.
            let scale = scale_of(word.speed);
            p.speed = word.speed;
            p.scale = scale;
            self.scaled |= scale != SCALE;
        }
        let pc = *p;
        if let Some(c) = self.cold.get_mut(ix(r)) {
            c.word_rest = word.rest;
        }
        if pc.flags & UP == 0 {
            return;
        }
        let to = key(pc.view, pc.scale, last);
        if from == to {
            return;
        }
        self.relink_all(r, pc.sig0, pc.flags & MORE != 0, from, to);
        if to > from && self.waiting != 0 {
            self.rise(r, out);
        }
    }

    /// A word from node `r`: false when it is to be ignored (an unknown node, or an epoch older than
    /// the one known). A newer epoch means its agent started again.
    fn fresh(&mut self, r: u32, word: &Word, out: &mut Out) -> bool {
        let Some(p) = self.peers.get(ix(r)) else {
            return self.stale();
        };
        if p.flags & KNOWN == 0 || word.epoch < p.epoch {
            return self.stale();
        }
        if word.epoch > p.epoch {
            self.restarted(r, word.epoch, out);
        }
        true
    }

    fn stale(&mut self) -> bool {
        self.counts.stale = self.counts.stale.saturating_add(1);
        false
    }

    /// Node `r`'s agent started again under `epoch`. Hex's adaptation: its boxes outlived it, and
    /// it says each again before its Synced, so what was starting, started or going there is unsure
    /// until then (as after a dropped link), and its count of ends goes on; what waited there (its
    /// line went with the agent) waits elsewhere. Starts and Gos sent under the old epoch come back
    /// declined. Kiln's owner took every job there for lost.
    fn restarted(&mut self, r: u32, epoch: u32, out: &mut Out) {
        if let Some(p) = self.peers.get_mut(ix(r)) {
            p.epoch = epoch;
        }
        let mut again = Vec::new();
        for h in self.runs.handles() {
            let Some(x) = self.runs.get(h).copied() else {
                continue;
            };
            match x.state & STATE {
                STARTING | STARTED | GOING if x.runner == r => {
                    if let Some(y) = self.runs.get_mut(h) {
                        y.state |= UNSURE;
                    }
                }
                WAITING if self.drop_place(x.slow, r) == Some(0) => again.push(h),
                _ => {}
            }
        }
        for h in again {
            self.place(h, out);
        }
    }

    /// Hears a node's word. What it says to the asker of the box it is about, if anything; news of
    /// other boxes, and what to send, go in `out`.
    pub fn hear(&mut self, msg: ToOwner, out: &mut Out) -> Option<Told> {
        match msg {
            ToOwner::Started {
                runner,
                run,
                at,
                word,
            } => return self.started(runner.0, run, at, &word, out),
            ToOwner::Ended { runner, run, word } => return self.ended(runner.0, run, &word, out),
            ToOwner::Declined {
                runner,
                run,
                word,
                why,
                of,
            } => self.declined(runner.0, run, &word, why, of, out),
            ToOwner::Queued {
                runner,
                run,
                at,
                word,
            } => self.queued(runner.0, run, at, &word, out),
            ToOwner::Ready {
                runner,
                run,
                at,
                word,
            } => self.ready(runner.0, run, at, &word, out),
            ToOwner::Hint { runner, word } => {
                if self.fresh(runner.0, &word, out) {
                    self.apply(runner.0, &word, out);
                }
            }
            ToOwner::Synced {
                runner,
                word,
                ended,
            } => self.synced(runner.0, &word, ended, out),
        }
        None
    }

    fn started(
        &mut self,
        r: u32,
        run: Handle,
        at: Handle,
        word: &Word,
        out: &mut Out,
    ) -> Option<Told> {
        if !self.fresh(r, word, out) {
            return None;
        }
        let told = self.started_run(r, run, at);
        self.apply(r, word, out);
        told
    }

    /// A Started for a box: a start or Go answered, a box said again, or a stale one.
    fn started_run(&mut self, r: u32, run: Handle, at: Handle) -> Option<Told> {
        let x = self.runs.get(run).copied().filter(|x| x.runner == r)?;
        match x.state & STATE {
            STARTING | GOING => {
                if x.state & STATE == STARTING {
                    self.answered(r, x.cpu, x.slow);
                }
                if let Some(y) = self.runs.get_mut(run) {
                    *y = Run {
                        state: STARTED,
                        at,
                        slow: NIL,
                        ..*y
                    };
                }
                self.free_slow(x.slow);
                Some(Told::Started {
                    run,
                    runner: RunnerId(r),
                })
            }
            // Said again as its link came back, or (Hex's adaptation) as its node's agent recovered
            // it under a new handle there: no longer unsure.
            STARTED if x.at == at || x.state & UNSURE != 0 => {
                if let Some(y) = self.runs.get_mut(run) {
                    y.state = STARTED;
                    y.at = at;
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
    fn answered(&mut self, r: u32, cpu: u64, slow: u32) {
        if let Some(p) = self.peers.get_mut(ix(r)) {
            p.inflight = p.inflight.saturating_sub(narrow(cpu));
        }
        if let Some(rest) = self.slow.at(slow).map(|s| s.rest)
            && let Some(c) = self.cold.get_mut(ix(r))
        {
            for (f, n) in c.inflight_rest.iter_mut().zip(rest) {
                *f = f.saturating_sub(n);
            }
        }
    }

    fn ended(&mut self, r: u32, run: Handle, word: &Word, out: &mut Out) -> Option<Told> {
        if !self.fresh(r, word, out) {
            return None;
        }
        if let Some(p) = self.peers.get_mut(ix(r)) {
            p.heard = p.heard.wrapping_add(1);
        }
        let x = self.runs.get(run).copied().filter(|x| x.runner == r);
        let told = match x {
            Some(x) if x.state & STATE == STARTED => {
                self.free_run(run);
                Some(Told::Ended { run })
            }
            _ => {
                self.stale();
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
                let left = if why == Why::Epoch {
                    self.drop_place_before(x.slow, r, word.epoch)
                } else {
                    self.drop_place(x.slow, r)
                };
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
            out.to.push((
                RunnerId(r),
                ToRunner::Withdraw {
                    owner: self.id,
                    run,
                    at,
                },
            ));
        }
        self.apply(r, word, out);
    }

    fn ready(&mut self, r: u32, run: Handle, at: Handle, word: &Word, out: &mut Out) {
        if !self.fresh(r, word, out) {
            return;
        }
        let x = self.runs.get(run).copied();
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut take =
            x.is_some_and(|x| x.state & STATE == WAITING && self.set_place(x.slow, r, at));
        #[cfg(test)]
        if x.is_some_and(|x| x.state & STATE == GOING && x.runner != r)
            && super::planted(super::Mutant::GoTwice)
        {
            take = true;
        }
        let epoch = self.peers.get(ix(r)).map_or(0, |p| p.epoch);
        if take {
            self.unwait(run, out, r);
            if let Some(y) = self.runs.get_mut(run) {
                y.state = GOING;
                y.runner = r;
                y.at = at;
            }
            out.to.push((
                RunnerId(r),
                ToRunner::Go {
                    owner: self.id,
                    run,
                    at,
                    epoch,
                },
            ));
        } else {
            out.to.push((
                RunnerId(r),
                ToRunner::Withdraw {
                    owner: self.id,
                    run,
                    at,
                },
            ));
        }
        self.apply(r, word, out);
    }

    /// Node `r`'s link is back: it has said every live box of this owner's there again, and how
    /// many of this owner's boxes have ended there (`ended`). What this owner was unsure of there
    /// is settled by that count, and the node is in its buckets again.
    fn synced(&mut self, r: u32, word: &Word, ended: u32, out: &mut Out) {
        if !self.fresh(r, word, out) {
            return;
        }
        // Unsure now: boxes that started there (it ended: it wasn't said again), and starts or Gos
        // whose answers went with the link (each never reached it, or ran and ended there).
        let (mut gone, mut unknown) = (Vec::new(), Vec::new());
        for h in self.runs.handles() {
            let Some(x) = self.runs.get(h).copied() else {
                continue;
            };
            if x.runner != r || x.state & UNSURE == 0 {
                continue;
            }
            if x.state & STATE == STARTED {
                gone.push(h);
            } else {
                unknown.push(h);
            }
        }
        let heard = self.peers.get(ix(r)).map_or(0, |p| p.heard);
        // Ends it said that never reached this owner: each a box of `gone`, or one of `unknown`
        // that ran and ended.
        let missed = ix(ended.wrapping_sub(heard));
        let ran = missed.saturating_sub(gone.len());
        for h in gone {
            self.free_run(h);
            out.told.push(Told::Ended { run: h });
        }
        let mut again = Vec::new();
        let all_ran = ran == unknown.len();
        for h in unknown {
            let Some(x) = self.runs.get(h).copied() else {
                continue;
            };
            if x.state & STATE == STARTING {
                self.answered(r, x.cpu, x.slow);
            }
            if ran == 0 {
                // None of them ran: placed again.
                self.park(h);
                again.push(h);
            } else {
                // Every one ran and ended; or some did, and which isn't known: at most once, each is
                // lost, never started again.
                self.free_run(h);
                out.told.push(if all_ran {
                    Told::Ended { run: h }
                } else {
                    Told::Lost { run: h }
                });
            }
        }
        let last = self.last();
        let Some(p) = self.peers.get_mut(ix(r)) else {
            return;
        };
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
        let to = key(pc.view, pc.scale, last);
        if let Some(c) = self.cold.get_mut(ix(r)) {
            c.word_rest = word.rest;
        }
        for s in self.sigs_of(r, &pc) {
            if let Some(s) = self.sigs.get_mut(ix(s)) {
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

    /// Node `r`'s link dropped: what was on it is lost. It leaves its buckets; what was starting,
    /// started or going there is unsure until it syncs; what waited there waits elsewhere.
    pub fn link_down(&mut self, r: RunnerId, out: &mut Out) {
        let ri = r.0;
        let last = self.last();
        let Some(p) = self.peers.get_mut(ix(ri)) else {
            return;
        };
        let was_up = p.flags & UP != 0;
        p.flags &= !UP;
        let pc = *p;
        // Out of its buckets if it was in them. A drop while it was still syncing still makes what
        // it had unsure.
        if was_up {
            let b = key(pc.view, pc.scale, last);
            for s in self.sigs_of(ri, &pc) {
                if let Some(s) = self.sigs.get_mut(ix(s)) {
                    s.ix.unlink(ri, b);
                }
            }
        }
        let mut again = Vec::new();
        for h in self.runs.handles() {
            let Some(x) = self.runs.get(h).copied() else {
                continue;
            };
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

    /// Places a box again (its start was declined, or its node's link came back without it): a
    /// member with room now, else it waits.
    fn place(&mut self, run: Handle, out: &mut Out) {
        let Some(x) = self.runs.get(run).copied() else {
            return;
        };
        // Its asker deleted it: it isn't placed again.
        if x.state & CANCELLED != 0 {
            self.free_run(run);
            return;
        }
        // Only a box parked here moves: one starting, going, running or waiting at nodes is in hand
        // already.
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
        self.unlist(x.sig, x.slow);
        let epoch = self.deduct(r, &need, cpu_only);
        if let Some(y) = self.runs.get_mut(run) {
            y.state = STARTING;
            y.runner = r;
            y.at = Handle::NONE;
        }
        out.to.push((
            RunnerId(r),
            ToRunner::Start(Start {
                owner: self.id,
                run,
                epoch,
                need,
            }),
        ));
    }

    /// Sends a box to wait at the [`PLACES`] members nearest to fitting it (the most free, first in
    /// first out among equals) whose budget covers it; none reachable, it waits here until a member
    /// comes back. Never, when no member could ever take it.
    fn wait(&mut self, run: Handle, out: &mut Out, tell: bool) {
        let Some(x) = self.runs.get(run).copied() else {
            return;
        };
        let need = self.need_of(&x);
        let Some(s) = self.sigs.get(ix(x.sig)) else {
            return;
        };
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
                if self
                    .cold
                    .get(ix(r))
                    .is_some_and(|c| within(&need, &c.budget))
                {
                    if let Some(p) = places.get_mut(n) {
                        *p = Place {
                            runner: r,
                            at: Handle::NONE,
                            epoch: self.peers.get(ix(r)).map_or(0, |q| q.epoch),
                        };
                    }
                    n = n.saturating_add(1);
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
            // Places it had are withdrawn (those whose answer hasn't come are withdrawn as it comes,
            // finding no place that wants it), so none is left behind in a line.
            for p in sl.places {
                if p.runner != NIL && p.at != Handle::NONE {
                    out.to.push((
                        RunnerId(p.runner),
                        ToRunner::Withdraw {
                            owner: self.id,
                            run,
                            at: p.at,
                        },
                    ));
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
            let epoch = self.peers.get(ix(p.runner)).map_or(0, |q| q.epoch);
            out.to.push((
                RunnerId(p.runner),
                ToRunner::Wait(Start {
                    owner: self.id,
                    run,
                    epoch,
                    need,
                }),
            ));
        }
        self.counts.waited = self.counts.waited.saturating_add(1);
        if tell {
            out.told.push(Told::Waiting { run });
        }
    }

    /// Takes a box out of waiting (it goes to `keep`, or elsewhere): its places are withdrawn,
    /// those whose line handle is known now, the rest as their nodes answer.
    fn unwait(&mut self, run: Handle, out: &mut Out, keep: u32) {
        let Some(x) = self.runs.get(run).copied() else {
            return;
        };
        if let Some(sl) = self.slow.at_mut(x.slow) {
            for p in &mut sl.places {
                if p.runner != NIL && p.runner != keep && p.at != Handle::NONE {
                    out.to.push((
                        RunnerId(p.runner),
                        ToRunner::Withdraw {
                            owner: self.id,
                            run,
                            at: p.at,
                        },
                    ));
                }
                *p = NO_PLACE;
            }
        }
        self.unlist(x.sig, x.slow);
    }

    /// Tries the oldest boxes waiting for signature `sig`'s members while one has room for them.
    /// Each try starts one box or ends the loop, so the boxes waiting bound it.
    fn retry(&mut self, sig: u32, out: &mut Out) {
        loop {
            let Some(head) = self.sigs.get(ix(sig)).map(|s| s.wait_head) else {
                return;
            };
            let Some(run) = self.runs.handle(head) else {
                return;
            };
            let Some(x) = self.runs.get(run).copied() else {
                return;
            };
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
            out.to.push((
                RunnerId(r),
                ToRunner::Start(Start {
                    owner: self.id,
                    run,
                    epoch,
                    need,
                }),
            ));
        }
    }

    /// Node `r`'s room rose: the boxes waiting for its signatures are tried.
    fn rise(&mut self, r: u32, out: &mut Out) {
        let Some(p) = self.peers.get(ix(r)).copied() else {
            return;
        };
        for s in self.sigs_of(r, &p) {
            if self.sigs.get(ix(s)).is_some_and(|s| s.waiting != 0) {
                self.retry(s, out);
            }
        }
    }

    /// Node `r` is reachable again (its link came back, or it joined a signature): boxes of its
    /// signatures that wait nowhere (no member was reachable) are sent to wait.
    fn unpark(&mut self, r: u32, out: &mut Out) {
        let Some(p) = self.peers.get(ix(r)).copied() else {
            return;
        };
        for s in self.sigs_of(r, &p) {
            let mut slot = self.sigs.get(ix(s)).map_or(NIL, |s| s.wait_head);
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

    /// Whether a box waits nowhere: parked here, to be placed.
    fn is_parked(&self, x: &Run) -> bool {
        x.state & STATE == WAITING
            && self
                .slow
                .at(x.slow)
                .is_none_or(|sl| sl.places.iter().all(|p| p.runner == NIL))
    }

    /// Parks a box here (its start, Go or places came to nothing): waiting nowhere, to be placed.
    fn park(&mut self, run: Handle) {
        if let Some(x) = self.runs.get_mut(run) {
            x.state = WAITING | (x.state & CANCELLED);
            x.runner = NIL;
            x.at = Handle::NONE;
        }
    }

    /// Drops a box's place at `r` if its Wait went under an epoch other than `epoch` (the node
    /// fenced that one); how many places it has left (`None`: none dropped).
    fn drop_place_before(&mut self, slow: u32, r: u32, epoch: u32) -> Option<usize> {
        let sl = self.slow.at_mut(slow)?;
        let p = sl
            .places
            .iter_mut()
            .find(|p| p.runner == r && p.epoch != epoch)?;
        *p = NO_PLACE;
        Some(sl.places.iter().filter(|p| p.runner != NIL).count())
    }

    /// Sets the line handle of a box's place at `r` (false: it has none there, or one already).
    fn set_place(&mut self, slow: u32, r: u32, at: Handle) -> bool {
        let Some(sl) = self.slow.at_mut(slow) else {
            return false;
        };
        match sl.places.iter_mut().find(|p| p.runner == r) {
            Some(p) if p.at == Handle::NONE || p.at == at => {
                p.at = at;
                true
            }
            _ => false,
        }
    }

    /// Drops a box's place at `r`; how many places it has left (`None`: it had none there).
    fn drop_place(&mut self, slow: u32, r: u32) -> Option<usize> {
        let sl = self.slow.at_mut(slow)?;
        let p = sl.places.iter_mut().find(|p| p.runner == r)?;
        *p = NO_PLACE;
        Some(sl.places.iter().filter(|p| p.runner != NIL).count())
    }

    /// Puts a box (by slot) at the tail of its signature's waiting boxes, unless it's there.
    fn list(&mut self, sig: u32, run: u32, slow: u32) {
        let Some(sl) = self.slow.at(slow).copied() else {
            return;
        };
        if sl.listed {
            return;
        }
        let Some(s) = self.sigs.get_mut(ix(sig)) else {
            return;
        };
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

    /// Takes a box out of its signature's waiting boxes, if it's there.
    fn unlist(&mut self, sig: u32, slow: u32) {
        let Some(sl) = self.slow.at(slow).copied() else {
            return;
        };
        if !sl.listed {
            return;
        }
        let Some(s) = self.sigs.get_mut(ix(sig)) else {
            return;
        };
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

    /// A box's whole need.
    fn need_of(&self, x: &Run) -> Amounts {
        let [m, d, g] = self.slow.at(x.slow).map_or([0; 3], |s| s.rest);
        [x.cpu, m, d, g]
    }

    /// Forgets a box.
    fn free_run(&mut self, run: Handle) {
        let Some(x) = self.runs.get(run).copied() else {
            return;
        };
        if x.slow != NIL {
            self.unlist(x.sig, x.slow);
            self.free_slow(x.slow);
        }
        let _ = self.runs.remove(run);
    }

    fn free_slow(&mut self, slow: u32) {
        if let Some(h) = self.slow.handle(slow) {
            let _ = self.slow.remove(h);
        }
    }

    /// The asker deleted a box before it was known to run. Waiting, it is forgotten at once: its
    /// places are withdrawn (those whose line handle isn't known yet, as their answers come) and
    /// nothing is told; true. Starting or going, it is marked: its Started is still told, so the
    /// shell can delete it at its node, but a decline is never placed again; false. Started or
    /// gone: false (the shell deletes a started box at its node).
    pub fn cancel(&mut self, run: Handle, out: &mut Out) -> bool {
        let Some(x) = self.runs.get(run).copied() else {
            return false;
        };
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

    /// Node `r`'s whole budget changed (devices or disk found after its agent started, memory on a
    /// VM): what could ever fit its signatures follows. The shell tells the node too.
    pub fn set_budget(&mut self, r: RunnerId, budget: Amounts) -> Result<(), Refused> {
        let ri = r.0;
        let Some(c) = self.cold.get_mut(ix(ri)) else {
            return Ok(());
        };
        let old = std::mem::replace(&mut c.budget, budget);
        let wanted = buckets_for(&budget);
        if wanted > self.buckets {
            self.buckets = wanted;
            self.grow_all()?;
        }
        let Some(p) = self.peers.get(ix(ri)).copied() else {
            return Ok(());
        };
        for s in self.sigs_of(ri, &p) {
            if let Some(s) = self.sigs.get_mut(ix(s)) {
                s.ix.reshape(&old, &budget);
            }
        }
        Ok(())
    }

    /// Its view of node `r`: what it takes `r` to have free for a new box (`None`: not known, or
    /// its link is down).
    pub fn view(&self, r: RunnerId) -> Option<Amounts> {
        let p = self.peers.get(ix(r.0)).filter(|p| p.flags & UP != 0)?;
        let c = self.cold.get(ix(r.0))?;
        let free = if p.flags & WHOLE != 0 {
            rest(&c.budget)
        } else {
            c.word_rest
        };
        let [a, b, d] = free;
        let [ia, ib, id] = c.inflight_rest;
        Some([
            u64::from(p.view),
            a.saturating_sub(ia),
            b.saturating_sub(ib),
            d.saturating_sub(id),
        ])
    }

    /// A box's state (`None`: it's gone, or never was).
    pub fn state(&self, run: Handle) -> Option<State> {
        let x = self.runs.get(run)?;
        Some(match x.state & STATE {
            STARTING => State::Starting {
                runner: RunnerId(x.runner),
            },
            STARTED => State::Started {
                runner: RunnerId(x.runner),
                at: x.at,
            },
            GOING => State::Going {
                runner: RunnerId(x.runner),
            },
            _ => {
                let places = self
                    .slow
                    .at(x.slow)
                    .map_or([NO_PLACE; PLACES], |s| s.places);
                State::Waiting {
                    at: places.map(|p| {
                        if p.runner == NIL {
                            None
                        } else {
                            Some(RunnerId(p.runner))
                        }
                    }),
                }
            }
        })
    }

    /// Every box it holds, with its state.
    pub fn runs(&self) -> Vec<(Handle, State)> {
        self.runs
            .iter()
            .filter_map(|(h, _)| Some((h, self.state(h)?)))
            .collect()
    }

    /// Whether node `r` is in signature `sig`'s buckets (a member whose link is up).
    pub fn bucketed(&self, sig: SigId, r: RunnerId) -> bool {
        self.sigs.get(ix(sig.0)).is_some_and(|s| s.ix.contains(r.0))
            && self.peers.get(ix(r.0)).is_some_and(|p| p.flags & UP != 0)
    }

    /// Signature `sig`'s members, each bucket's first to last, lowest bucket first: the order its
    /// picks take them in.
    pub fn order(&self, sig: SigId) -> Vec<RunnerId> {
        let Some(s) = self.sigs.get(ix(sig.0)) else {
            return Vec::new();
        };
        let mut v = Vec::new();
        let mut b = s.ix.at_or_above(0);
        while let Some(bb) = b {
            v.extend(s.ix.bucket(bb).map(RunnerId));
            b = s.ix.above(bb);
        }
        v
    }
}

/// A node's key multiplier at its measured per-core speed: core-equivalents. Speeds past what a u32
/// multiplier holds (3.9 times the reference) are taken as that.
fn scale_of(speed: u32) -> u32 {
    if speed == 0 {
        return SCALE;
    }
    let s = u64::from(SCALE)
        .saturating_mul(u64::from(speed))
        .checked_div(u64::from(SPEED_ONE))
        .unwrap_or(u64::MAX);
    u32::try_from(s).unwrap_or(u32::MAX)
}

impl Slow {
    fn of(need: &Amounts) -> Slow {
        Slow {
            rest: rest(need),
            places: [NO_PLACE; PLACES],
            prev: NIL,
            next: NIL,
            listed: false,
        }
    }
}
