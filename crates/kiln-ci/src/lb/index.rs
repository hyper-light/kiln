//! A signature's members, bucketed by free whole CPUs (docs/lb.md "The index"; docs/ci.md W05-R04:
//! a signature's eligible runners, kept as runners join and leave, so a pick looks only at them).
//!
//! Bucket `b` holds the members with at least `b` and less than `b + 1` CPUs free, as their owner
//! sees them. Within a bucket members are first in, first out: one that moves (it took work, or a
//! run of it ended) goes to the tail of its new bucket, so equals take turns. A bitmap says which
//! buckets hold any member, so the most free member is the head of the highest set bit: one
//! leading-zeros count per level. Every operation is O(1) but a walk, which goes member by member
//! and is counted by its caller.
//!
//! Members are runners' dense ids; the links are arrays indexed by them, compact (`u32`), so an
//! index over 10,000 runners is 80 KB of links.

use super::{Amounts, NIL};

/// The most buckets an index has: two levels of 64-bit words, 64 × 64, so the highest non-empty
/// bucket is two leading-zeros counts away. A member with more whole CPUs free than that is in the
/// last bucket, where the pick checks each member's CPUs exactly.
pub const BUCKETS: u32 = 64 * 64;

#[derive(Debug, Clone, Copy)]
struct Link {
    prev: u32,
    next: u32,
}

const UNLINKED: Link = Link { prev: NIL, next: NIL };

/// The members of one signature, by free whole CPUs.
#[derive(Debug, Clone, Default)]
pub struct Index {
    /// Each bucket's first and last member (NIL: none).
    head: Vec<u32>,
    tail: Vec<u32>,
    /// Each runner's neighbours in its bucket, by runner id.
    link: Vec<Link>,
    /// Bucket `64 w + i` holds a member when bit `i` of `words[w]` is set; bit `w` of `summary`
    /// is set when `words[w]` is not 0.
    words: Vec<u64>,
    summary: u64,
    /// Which runners are members, bucketed or not (a member whose link is down isn't bucketed).
    member: Vec<u64>,
    count: u32,
    /// The members' budgets, each distinct one with how many members have it: whether a need
    /// could ever fit here, in O(distinct budgets).
    shapes: Vec<(Amounts, u32)>,
}

impl Index {
    pub fn new() -> Index {
        Index::default()
    }

    /// Makes room for `buckets` buckets (at most [`BUCKETS`]) and runners numbered below `runners`.
    pub fn grow(&mut self, buckets: u32, runners: u32) {
        let b = buckets.min(BUCKETS) as usize;
        if self.head.len() < b {
            self.head.resize(b, NIL);
            self.tail.resize(b, NIL);
        }
        let w = b.div_ceil(64);
        if self.words.len() < w {
            self.words.resize(w, 0);
        }
        let r = runners as usize;
        if self.link.len() < r {
            self.link.resize(r, UNLINKED);
            self.member.resize(r.div_ceil(64), 0);
        }
    }

    /// How many runners are members.
    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether runner `r` is a member.
    pub fn contains(&self, r: u32) -> bool {
        self.member.get((r / 64) as usize).is_some_and(|w| w & (1u64 << (r % 64)) != 0)
    }

    /// The members, in runner order.
    pub fn members(&self) -> impl Iterator<Item = u32> + '_ {
        self.member.iter().zip(0u32..).flat_map(|(&w, i)| (0..64u32).filter(move |b| w & (1u64 << b) != 0).map(move |b| i * 64 + b))
    }

    /// Makes `r`, whose budget is `budget`, a member (not yet in a bucket). False when it was one.
    pub fn join(&mut self, r: u32, budget: &Amounts) -> bool {
        if self.contains(r) {
            return false;
        }
        self.grow(0, r.saturating_add(1));
        if let Some(w) = self.member.get_mut((r / 64) as usize) {
            *w |= 1u64 << (r % 64);
        }
        self.count = self.count.saturating_add(1);
        match self.shapes.iter_mut().find(|(b, _)| b == budget) {
            Some((_, n)) => *n = n.saturating_add(1),
            None => self.shapes.push((*budget, 1)),
        }
        true
    }

    /// Takes `r` out of the members (its caller takes it out of its bucket first). False when it
    /// wasn't one.
    pub fn part(&mut self, r: u32, budget: &Amounts) -> bool {
        if !self.contains(r) {
            return false;
        }
        if let Some(w) = self.member.get_mut((r / 64) as usize) {
            *w &= !(1u64 << (r % 64));
        }
        self.count = self.count.saturating_sub(1);
        if let Some(i) = self.shapes.iter().position(|(b, _)| b == budget)
            && let Some((_, n)) = self.shapes.get_mut(i)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.shapes.swap_remove(i);
            }
        }
        true
    }

    /// Whether some member's whole budget covers `need`: false means it never fits here.
    pub fn could_fit(&self, need: &Amounts) -> bool {
        self.shapes.iter().any(|(b, _)| need.iter().zip(b).all(|(n, b)| n <= b))
    }

    #[inline]
    fn set(&mut self, b: u32) {
        let w = b / 64;
        if let Some(word) = self.words.get_mut(w as usize) {
            *word |= 1u64 << (b % 64);
            self.summary |= 1u64 << (w % 64);
        }
    }

    #[inline]
    fn unset(&mut self, b: u32) {
        let w = b / 64;
        if let Some(word) = self.words.get_mut(w as usize) {
            *word &= !(1u64 << (b % 64));
            if *word == 0 {
                self.summary &= !(1u64 << (w % 64));
            }
        }
    }

    /// Puts member `r` at the tail of bucket `b`.
    #[inline]
    pub fn push(&mut self, r: u32, b: u32) {
        #[cfg(test)]
        if super::planted(super::Mutant::Lifo) {
            return self.push_front(r, b);
        }
        let Some(&t) = self.tail.get(b as usize) else { return };
        if let Some(l) = self.link.get_mut(r as usize) {
            *l = Link { prev: t, next: NIL };
        }
        // An empty bucket's tail is NIL, past every link: then `r` is its head too.
        match self.link.get_mut(t as usize) {
            Some(tl) => tl.next = r,
            None => {
                if let Some(h) = self.head.get_mut(b as usize) {
                    *h = r;
                }
                self.set(b);
            }
        }
        if let Some(tl) = self.tail.get_mut(b as usize) {
            *tl = r;
        }
    }

    /// The Lifo mutant's push: at the head.
    #[cfg(test)]
    fn push_front(&mut self, r: u32, b: u32) {
        let Some(&h) = self.head.get(b as usize) else { return };
        if let Some(l) = self.link.get_mut(r as usize) {
            *l = Link { prev: NIL, next: h };
        }
        match self.link.get_mut(h as usize) {
            Some(hl) => hl.prev = r,
            None => {
                if let Some(t) = self.tail.get_mut(b as usize) {
                    *t = r;
                }
                self.set(b);
            }
        }
        if let Some(hd) = self.head.get_mut(b as usize) {
            *hd = r;
        }
    }

    /// Takes member `r` out of bucket `b`, wherever it is in it.
    #[inline]
    pub fn unlink(&mut self, r: u32, b: u32) {
        let Some(&Link { prev, next }) = self.link.get(r as usize) else { return };
        match self.link.get_mut(prev as usize) {
            Some(p) => p.next = next,
            None => {
                if let Some(h) = self.head.get_mut(b as usize) {
                    *h = next;
                }
            }
        }
        match self.link.get_mut(next as usize) {
            Some(n) => n.prev = prev,
            None => {
                if let Some(t) = self.tail.get_mut(b as usize) {
                    *t = prev;
                }
            }
        }
        if prev == NIL && next == NIL {
            self.unset(b);
        }
    }

    /// Moves member `r` from bucket `from` to the tail of bucket `to` (its own, if they're the
    /// same: it took its turn).
    #[inline]
    pub fn relink(&mut self, r: u32, from: u32, to: u32) {
        self.unlink(r, from);
        self.push(r, to);
    }

    /// Moves bucket `from`'s first members, from its head `first` to `last`, to the tail of bucket
    /// `to` in their order: what that many moves one by one would leave, in O(1).
    pub fn splice(&mut self, from: u32, first: u32, last: u32, to: u32) {
        let after = self.next(last);
        if let Some(h) = self.head.get_mut(from as usize) {
            *h = after;
        }
        match self.link.get_mut(after as usize) {
            Some(a) => a.prev = NIL,
            None => {
                if let Some(t) = self.tail.get_mut(from as usize) {
                    *t = NIL;
                }
                self.unset(from);
            }
        }
        let Some(&t) = self.tail.get(to as usize) else { return };
        if let Some(l) = self.link.get_mut(first as usize) {
            l.prev = t;
        }
        if let Some(l) = self.link.get_mut(last as usize) {
            l.next = NIL;
        }
        match self.link.get_mut(t as usize) {
            Some(tl) => tl.next = first,
            None => {
                if let Some(h) = self.head.get_mut(to as usize) {
                    *h = first;
                }
                self.set(to);
            }
        }
        if let Some(tl) = self.tail.get_mut(to as usize) {
            *tl = last;
        }
    }

    /// The highest bucket that holds a member.
    #[inline]
    pub fn top(&self) -> Option<u32> {
        if self.summary == 0 {
            return None;
        }
        let w = 63 - self.summary.leading_zeros();
        let bits = *self.words.get(w as usize)?;
        Some(w * 64 + 63u32.checked_sub(bits.leading_zeros())?)
    }

    /// The highest bucket below `b` that holds a member.
    pub fn below(&self, b: u32) -> Option<u32> {
        let (w, i) = (b / 64, b % 64);
        let bits = self.words.get(w as usize).copied().unwrap_or(0) & ((1u64 << i) - 1);
        if bits != 0 {
            return Some(w * 64 + 63 - bits.leading_zeros());
        }
        let lower = self.summary & ((1u64 << (w % 64)) - 1);
        if lower == 0 || w == 0 {
            return None;
        }
        let w = 63 - lower.leading_zeros();
        let bits = *self.words.get(w as usize)?;
        Some(w * 64 + 63u32.checked_sub(bits.leading_zeros())?)
    }

    /// Bucket `b`'s first member (NIL: none).
    #[inline]
    pub fn head(&self, b: u32) -> u32 {
        self.head.get(b as usize).copied().unwrap_or(NIL)
    }

    /// The member after `r` in its bucket (NIL: none).
    #[inline]
    pub fn next(&self, r: u32) -> u32 {
        self.link.get(r as usize).map_or(NIL, |l| l.next)
    }

    /// Bucket `b`'s members, first to last.
    pub fn bucket(&self, b: u32) -> impl Iterator<Item = u32> + '_ {
        std::iter::successors(Some(self.head(b)).filter(|r| *r != NIL), move |r| Some(self.next(*r)).filter(|n| *n != NIL))
    }
}
