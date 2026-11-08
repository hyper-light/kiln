//! Room tokens (docs/lb.md "Room tokens"; LB08, W05-R03, W05-R23): what each owner is told of a
//! runner's room when several owners share it, so that what a runner says grows with the runs it
//! holds, not with the owners that watch it.
//!
//! Under hints, every change of a runner's room is owed to every owner that watches it: 2(W - 1)
//! words a run for W watchers. The least messaging that keeps the queueing delay vanishing is about
//! one message a job, sent when room appears, with memory at the dispatcher (Gamarnik, Tsitsiklis
//! and Zubeldia, Stochastic Systems 8(1), 2018; their lower bound, Annals of Applied Probability
//! 30(2), 2020); Join-Idle-Queue reaches it (Lu et al., Performance Evaluation 68(11), 2011), as
//! does PULL with heterogeneous servers and several routers (Stolyar, Queueing Systems 80(4), 2015
//! and 85(1-2), 2017). Under skew across dispatchers, tokens must arrive in proportion to each one's
//! load, or be exchanged (van der Boor, Borst and van Leeuwaarden, INFOCOM 2017).
//!
//! What this kernel does with that, for runners that hold many runs and four-dimensional room:
//!
//! - **A token is an amount**, `[cpu, mem, disk, gpus]` of a runner's room for new runs, and an
//!   owner holds at most one per runner: what its word from that runner says, which its view files
//!   in the bucket of its CPU part (the same buckets as hints'). Tokens never overlap: what every
//!   owner holds at a runner is at most its room, in each dimension ([`Book::fit`]).
//! - **Room goes back to the owner whose run freed it** (its idle queue: JIQ's I-queue, at the
//!   dispatcher whose job freed the server), less what the runner's line takes first. Room arrives
//!   at each owner in proportion to its own runs by construction, the non-uniform allotment van der
//!   Boor et al. prove necessary under skew, with no rate measured and no destination drawn.
//! - **Room no owner's run freed belongs to the runner's first watcher** (its room when it joined,
//!   after it started again, from a budget that grew, held room withdrawn): that owner's word says
//!   all room the others don't hold, so with one owner a token is the whole room and nothing
//!   differs from hints.
//! - **An owner that takes room pays from its own token first**; room past its token comes from
//!   room no one holds, then from the largest tokens. Tokens aren't reservations: admission is
//!   against the runner's room, as ever, so a token never strands room or lets a runner past its
//!   budget. An owner whose token shrank without its asking is owed a Hint, so its view is exact
//!   once what is in flight arrives.
//! - **An unusable token is forwarded once.** A runner's word says when another owner's run waited
//!   there for room since ([`super::Word::wants`]); an owner holding a token there with no run of its
//!   own waiting for that runner gives it back ([`super::ToRunner::Release`]), and the runner moves
//!   it to the owner whose run last waited there, which then isn't one that wants (so it is moved
//!   once). That is the token exchange (van der Boor et al.), sent only where a want was seen.
//! - **A run no token fits waits at two members drawn from its signature** ([`draw`]), uniformly
//!   (PULL's random server; two, Mitzenmacher, IEEE TPDS 12(10), 2001; late binding, Sparrow, SOSP
//!   2013), after any member whose token covers part of it: under tokens a view is what an owner
//!   holds, not what a runner has, so "nearest to fitting" by view would put every wait on the
//!   same few runners.
//!
//! Pure, as the rest of the kernel: integer amounts, and the only randomness is the generator the
//! owner is given.

use super::index::Index;
use super::{Amounts, NIL, OwnerId};

/// One owner's token at a runner, and what it was last told of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hold {
    owner: OwnerId,
    /// The room it holds here: what its runs freed and it hasn't used (never the first watcher's,
    /// whose room is the rest).
    token: Amounts,
    /// What the last word it was sent said it may use: when the two differ, it is owed a Hint.
    told: Amounts,
}

/// No owner (no run waited here for room).
const NOBODY: OwnerId = OwnerId(NIL);

/// A runner's tokens: what each owner it talks to holds of its room for new runs. Kept only while
/// several owners talk to it and tokens are on; one owner holds everything, as under hints.
#[derive(Debug, Clone)]
pub(crate) struct Book {
    pub(crate) on: bool,
    holds: Vec<Hold>,
    /// The owner whose Wait was last queued here, finding no room: the one a token given back is
    /// forwarded to, once.
    want: OwnerId,
}

impl Default for Book {
    fn default() -> Book {
        Book { on: false, holds: Vec::new(), want: NOBODY }
    }
}

fn sub(a: &Amounts, b: &Amounts) -> Amounts {
    let mut x = *a;
    for (v, w) in x.iter_mut().zip(b) {
        *v = v.saturating_sub(*w);
    }
    x
}

fn add(a: &Amounts, b: &Amounts) -> Amounts {
    let mut x = *a;
    for (v, w) in x.iter_mut().zip(b) {
        *v = v.saturating_add(*w);
    }
    x
}

fn min(a: &Amounts, b: &Amounts) -> Amounts {
    let mut x = *a;
    for (v, w) in x.iter_mut().zip(b) {
        *v = (*v).min(*w);
    }
    x
}

impl Book {
    fn at(&self, o: OwnerId) -> Option<usize> {
        self.holds.iter().position(|h| h.owner == o)
    }

    /// Owner `o`'s record, made when it is first needed.
    fn hold(&mut self, o: OwnerId) -> Option<&mut Hold> {
        if self.at(o).is_none() {
            self.holds.push(Hold { owner: o, token: [0; 4], told: [0; 4] });
        }
        let i = self.at(o)?;
        self.holds.get_mut(i)
    }

    /// What all tokens but the first watcher's hold.
    fn held(&self, first: OwnerId) -> Amounts {
        self.holds.iter().filter(|h| h.owner != first).fold([0; 4], |s, h| add(&s, &h.token))
    }

    /// What owner `o`'s word says it may use: its token; for the first watcher, all of `room` the
    /// others don't hold.
    pub(crate) fn amount(&self, o: OwnerId, first: OwnerId, room: &Amounts) -> Amounts {
        if o == first {
            return sub(room, &self.held(first));
        }
        self.at(o).and_then(|i| self.holds.get(i)).map_or([0; 4], |h| min(&h.token, room))
    }

    /// Whether `o`'s word says another owner's run waited here for room.
    pub(crate) fn wants(&self, o: OwnerId) -> bool {
        #[cfg(test)]
        if super::planted(super::Mutant::NeverWants) {
            return false;
        }
        self.want != NOBODY && self.want != o
    }

    /// A Wait of `o`'s was queued here: room was short for it.
    pub(crate) fn waited(&mut self, o: OwnerId) {
        self.want = o;
    }

    /// Owner `o` took `used` of the room: out of its own token first (the first watcher's is the
    /// room no one holds, so there is nothing to take from).
    pub(crate) fn pay(&mut self, o: OwnerId, first: OwnerId, used: &Amounts) {
        if o == first {
            return;
        }
        if let Some(h) = self.at(o).and_then(|i| self.holds.get_mut(i)) {
            h.token = sub(&h.token, used);
        }
    }

    /// Tokens never hold more than the room: in each dimension the excess comes out of the
    /// largest tokens first (the first watcher's room is what is left, so it shrinks first of all).
    pub(crate) fn fit(&mut self, first: OwnerId, room: &Amounts) {
        #[cfg(test)]
        if super::planted(super::Mutant::TokensOverlap) {
            return;
        }
        for d in 0..room.len() {
            let cap = room.get(d).copied().unwrap_or(0);
            loop {
                let total: u64 = self.holds.iter().filter(|h| h.owner != first).map(|h| h.token.get(d).copied().unwrap_or(0)).fold(0, u64::saturating_add);
                if total <= cap {
                    break;
                }
                let excess = total - cap;
                let Some(big) = self.holds.iter_mut().filter(|h| h.owner != first).max_by_key(|h| h.token.get(d).copied().unwrap_or(0)) else { break };
                let Some(v) = big.token.get_mut(d) else { break };
                if *v == 0 {
                    break;
                }
                *v -= excess.min(*v);
            }
        }
    }

    /// A run of `o`'s freed `freed`: it goes back to `o`'s token, as far as room no token holds
    /// covers it (the line took its part first). The first watcher's goes nowhere: room no one
    /// holds is its already.
    pub(crate) fn give_back(&mut self, o: OwnerId, first: OwnerId, freed: &Amounts, room: &Amounts) {
        if o == first {
            return;
        }
        #[cfg(test)]
        if super::planted(super::Mutant::FreedToFirst) {
            return;
        }
        let free = sub(room, &self.held(first));
        if let Some(h) = self.hold(o) {
            h.token = add(&h.token, &min(freed, &free));
        }
    }

    /// Owner `o` gives back `amount` it can't use: forwarded to the owner whose run last waited
    /// here for room, once (that owner is no longer the one that wants), else left to the first
    /// watcher. What moved.
    pub(crate) fn release(&mut self, o: OwnerId, first: OwnerId, amount: &Amounts, room: &Amounts) -> Amounts {
        let have = self.amount(o, first, room);
        let moved = min(amount, &have);
        self.pay(o, first, &moved);
        let to = self.want;
        if to != NOBODY && to != o {
            #[cfg(test)]
            let again = super::planted(super::Mutant::ForwardAgain);
            #[cfg(not(test))]
            let again = false;
            if !again {
                self.want = NOBODY;
            }
            if to != first
                && let Some(h) = self.hold(to)
            {
                h.token = add(&h.token, &moved);
            }
        }
        moved
    }

    /// Owner `o` is gone from this runner (unwatched, or its link dropped): what it held is no
    /// one's.
    pub(crate) fn forget(&mut self, o: OwnerId) {
        self.holds.retain(|h| h.owner != o);
        if self.want == o {
            self.want = NOBODY;
        }
    }

    /// The runner started again: nothing of before is held.
    pub(crate) fn reset(&mut self) {
        for h in &mut self.holds {
            h.token = [0; 4];
        }
        self.want = NOBODY;
    }

    /// Owner `o` is sent a word now: what it says `o` may use, written down as told.
    pub(crate) fn tell(&mut self, o: OwnerId, first: OwnerId, room: &Amounts) -> Amounts {
        let a = self.amount(o, first, room);
        if let Some(h) = self.hold(o) {
            h.told = a;
        }
        a
    }

    /// Owner `o` was told the whole room (under hints, or as a runner's one owner).
    pub(crate) fn told_room(&mut self, o: OwnerId, first: OwnerId, room: &Amounts) {
        let _ = first;
        if let Some(h) = self.hold(o) {
            h.told = *room;
        }
    }

    /// Whether owner `o` would be told something other than its last word said.
    pub(crate) fn owed(&self, o: OwnerId, first: OwnerId, room: &Amounts) -> bool {
        #[cfg(test)]
        if super::planted(super::Mutant::NoHint) {
            return false;
        }
        let told = self.at(o).and_then(|i| self.holds.get(i)).map_or([0; 4], |h| h.told);
        self.amount(o, first, room) != told
    }

    /// What every owner but the first holds, for the model's oracle.
    #[cfg(test)]
    pub(crate) fn tokens(&self, first: OwnerId) -> Vec<(OwnerId, Amounts)> {
        self.holds.iter().filter(|h| h.owner != first).map(|h| (h.owner, h.token)).collect()
    }
}

/// SplitMix64, one step (Steele, Lea, Flood, OOPSLA 2014), as the owner's spread uses.
pub(crate) fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One of `n` drawn uniformly, by multiplication (Lemire, TOMACS 2019, less its rejection).
fn below(state: &mut u64, n: u32) -> u32 {
    ((u128::from(next(state)) * u128::from(n)) >> 64) as u32
}

/// A member of `ix` drawn uniformly: the bucket that holds the `u`-th, then the `u`-th there.
fn member(ix: &Index, state: &mut u64) -> Option<u32> {
    let n = ix.len();
    if n == 0 {
        return None;
    }
    let mut u = below(state, n);
    let mut b = ix.top()?;
    loop {
        let c = ix.count(b);
        if u < c {
            let r = ix.nth_in(b, u);
            return (r != NIL).then_some(r);
        }
        u -= c;
        b = ix.below(b)?;
    }
}

/// The places a run waits at under tokens: `first`, the members whose view covers part of it
/// (nearest to fitting first, as the caller found them), then members drawn uniformly from the
/// signature's, each one that `accept` takes (one that could ever take the run, and, with learned
/// weights, kept in proportion to its weight by a draw of its own) and none twice. Each draw is
/// tried at most as many times as there are members, so it ends; when draws find too few, the
/// caller's own order fills the rest.
pub(crate) fn draw<const P: usize>(ix: &Index, first: &[u32], state: &mut u64, mut accept: impl FnMut(u32, &mut u64) -> bool) -> [u32; P] {
    let mut places = [NIL; P];
    let mut n = 0usize;
    for &r in first {
        if n < P
            && !places.contains(&r)
            && let Some(p) = places.get_mut(n)
        {
            *p = r;
            n += 1;
        }
    }
    let tries = ix.len();
    while n < P {
        let mut got = None;
        for _ in 0..tries {
            #[cfg(test)]
            if super::planted(super::Mutant::NearestPlaces) {
                break;
            }
            match member(ix, state) {
                Some(r) if !places.contains(&r) && accept(r, state) => {
                    got = Some(r);
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        let Some(r) = got else { break };
        if let Some(p) = places.get_mut(n) {
            *p = r;
        }
        n += 1;
    }
    places
}
