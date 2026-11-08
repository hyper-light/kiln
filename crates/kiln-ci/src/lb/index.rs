//! A signature's members, bucketed by their rank key (docs/lb.md "The index"; docs/ci.md W05-R04:
//! a signature's eligible runners, kept as runners join and leave, so a pick looks only at them).
//!
//! Bucket `b` holds the members whose key is `b`: free whole CPUs, as their owner sees them, until
//! runners measure their speed (then core-equivalents; the owner computes the key). Membership is
//! a bitset per bucket over runner ids, interleaved by word (bucket `b`'s word `w` sits at
//! `w * buckets + b`), so one runner's bits for every bucket share a cache line or two and a move
//! is two bit writes there, touching no other runner. A summary bitset per bucket says which of
//! its words hold any member, and a bitmap of the buckets that hold any gives the highest in two
//! leading-zeros counts. Equals take turns: each bucket has a cursor, a pick takes the first
//! member at or after it (in id order, wrapping) and moves it past the one taken.
//!
//! Every operation is O(1) for fleets up to 4,096 x 64 runners per summary word (a word per 4,096
//! runners), except a walk, which goes member by member and is counted by its caller.

use super::{Amounts, NIL};

/// The most buckets an index has: two levels of 64-bit words, 64 x 64, so the highest non-empty
/// bucket is two leading-zeros counts away. A member whose key is past that is in the last bucket.
pub const BUCKETS: u32 = 64 * 64;

/// A bucket's own state.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Members in it.
    count: u32,
    /// The runner its next pick starts from.
    cursor: u32,
    /// The last member put in, while it is still there (NIL: unknown): the next in turn when it
    /// is the only one, so the commonest pick (the bucket the runner a run just ended on entered)
    /// needs no search.
    last: u32,
}

/// The members of one signature, by rank key.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct Index {
    /// The summary of `words` (below), the word buckets 0 to 63 are in right after it.
    summary: u64,
    /// Buckets, and 64-runner words, it has room for.
    nb: u32,
    nw: u32,
    /// Bucket `b`'s membership word `w`: bit `i` is runner `64 w + i`. At `w * nb + b`.
    bits: Vec<u64>,
    /// Bucket `b`'s summary word `s`: bit `i` is membership word `64 s + i` not 0. At `s * nb + b`.
    sum: Vec<u64>,
    /// Each bucket's count, cursor and last member put in, together: a pick reads them at once.
    slots: Vec<Bucket>,
    /// Bucket `64 w + i` holds a member when bit `i` of `words[w]` is set; bit `w` of `summary`
    /// is set when `words[w]` is not 0. Inline: no pointer to follow for the top.
    words: [u64; 64],
    /// Where cursors start: the owner's spread (docs/lb.md "Several owners").
    start: u32,
    /// Which runners are members, bucketed or not (a member whose link is down isn't bucketed).
    member: Vec<u64>,
    members: u32,
    /// The members' budgets, each distinct one with how many members have it: whether a need
    /// could ever fit here, in O(distinct budgets).
    shapes: Vec<(Amounts, u32)>,
}

impl Default for Index {
    fn default() -> Self {
        Index::new(0)
    }
}

impl Index {
    /// An empty index whose cursors start at `start` (a runner id, taken modulo the fleet).
    pub fn new(start: u32) -> Index {
        Index { nb: 0, nw: 0, bits: Vec::new(), sum: Vec::new(), slots: Vec::new(), start, words: [0; 64], summary: 0, member: Vec::new(), members: 0, shapes: Vec::new() }
    }

    /// Makes room for `buckets` buckets (at most [`BUCKETS`]) and runners numbered below `runners`.
    pub fn grow(&mut self, buckets: u32, runners: u32) {
        let nb = buckets.clamp(1, BUCKETS).max(self.nb);
        let nw = runners.div_ceil(64).max(self.nw).max(1);
        if nb == self.nb && nw == self.nw {
            return;
        }
        let ns = nw.div_ceil(64);
        let mut bits = vec![0u64; (nw * nb) as usize];
        let mut sum = vec![0u64; (ns * nb) as usize];
        for w in 0..self.nw {
            for b in 0..self.nb {
                let x = self.bits.get((w * self.nb + b) as usize).copied().unwrap_or(0);
                if let Some(d) = bits.get_mut((w * nb + b) as usize) {
                    *d = x;
                }
                if x != 0
                    && let Some(s) = sum.get_mut(((w / 64) * nb + b) as usize)
                {
                    *s |= 1u64 << (w % 64);
                }
            }
        }
        self.bits = bits;
        self.sum = sum;
        // A cursor starts at the owner's spread over the fleet it has now.
        let at = ((u64::from(self.start) * u64::from(nw * 64)) >> 32) as u32;
        self.slots.resize(nb as usize, Bucket { count: 0, cursor: at, last: NIL });
        self.member.resize(nw as usize, 0);
        self.nb = nb;
        self.nw = nw;
    }

    /// How many runners are members.
    pub fn len(&self) -> u32 {
        self.members
    }

    pub fn is_empty(&self) -> bool {
        self.members == 0
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
        self.grow(self.nb, r.saturating_add(1));
        if let Some(w) = self.member.get_mut((r / 64) as usize) {
            *w |= 1u64 << (r % 64);
        }
        self.members = self.members.saturating_add(1);
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
        self.members = self.members.saturating_sub(1);
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

    /// A member's budget changed from `old` to `new`.
    pub fn reshape(&mut self, old: &Amounts, new: &Amounts) {
        if let Some(i) = self.shapes.iter().position(|(b, _)| b == old)
            && let Some((_, n)) = self.shapes.get_mut(i)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.shapes.swap_remove(i);
            }
        }
        match self.shapes.iter_mut().find(|(b, _)| b == new) {
            Some((_, n)) => *n = n.saturating_add(1),
            None => self.shapes.push((*new, 1)),
        }
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

    /// Puts member `r` in bucket `b`.
    #[inline(always)]
    pub fn push(&mut self, r: u32, b: u32) {
        let (w, nb) = (r / 64, self.nb);
        let Some(x) = self.bits.get_mut((w * nb + b) as usize) else { return };
        let was = *x;
        *x = was | (1u64 << (r % 64));
        if was == 0
            && let Some(s) = self.sum.get_mut(((w / 64) * nb + b) as usize)
        {
            *s |= 1u64 << (w % 64);
        }
        if let Some(k) = self.slots.get_mut(b as usize) {
            k.count += 1;
            k.last = r;
            if k.count == 1 {
                self.set(b);
            }
        }
    }

    /// Takes member `r` out of bucket `b`.
    #[inline(always)]
    pub fn unlink(&mut self, r: u32, b: u32) {
        let (w, nb) = (r / 64, self.nb);
        let Some(x) = self.bits.get_mut((w * nb + b) as usize) else { return };
        let left = *x & !(1u64 << (r % 64));
        *x = left;
        if left == 0
            && let Some(s) = self.sum.get_mut(((w / 64) * nb + b) as usize)
        {
            *s &= !(1u64 << (w % 64));
        }
        if let Some(k) = self.slots.get_mut(b as usize) {
            k.count = k.count.saturating_sub(1);
            if k.last == r {
                k.last = NIL;
            }
            if k.count == 0 {
                self.unset(b);
            }
        }
    }

    /// Moves member `r` from bucket `from` to bucket `to` (nothing when they're the same).
    #[inline(always)]
    pub fn relink(&mut self, r: u32, from: u32, to: u32) {
        if from != to {
            self.unlink(r, from);
            self.push(r, to);
        }
    }

    /// The highest bucket that holds a member.
    #[inline(always)]
    pub fn top(&self) -> Option<u32> {
        // Buckets below 64 (runners under 64 cores) are the first word alone: read with the
        // summary, not after it.
        let (low, summary) = (self.words.first().copied().unwrap_or(0), self.summary);
        if summary <= 1 {
            return if low == 0 { None } else { Some(63 - low.leading_zeros()) };
        }
        let w = 63 - summary.leading_zeros();
        let bits = *self.words.get(w as usize)?;
        Some(w * 64 + 63u32.checked_sub(bits.leading_zeros())?)
    }

    /// Whether bucket `b` and every one above it are empty.
    #[inline(always)]
    pub fn none_from(&self, b: u32) -> bool {
        if b < 64 {
            let low = self.words.first().copied().unwrap_or(0);
            return self.summary <= 1 && low >> b == 0;
        }
        self.top().is_none_or(|t| b > t)
    }

    /// The highest bucket below `b` that holds a member.
    pub fn below(&self, b: u32) -> Option<u32> {
        let (w, i) = (b / 64, b % 64);
        let bits = self.words.get(w as usize).copied().unwrap_or(0) & ((1u64 << i) - 1);
        if bits != 0 {
            return Some(w * 64 + 63 - bits.leading_zeros());
        }
        let lower = self.summary & ((1u64 << (w % 64)) - 1);
        if lower == 0 {
            return None;
        }
        let w = 63 - lower.leading_zeros();
        let bits = *self.words.get(w as usize)?;
        Some(w * 64 + 63u32.checked_sub(bits.leading_zeros())?)
    }

    /// Bucket `b`'s member at or after runner `from`, wrapping (NIL: the bucket is empty).
    #[inline]
    fn first_from(&self, b: u32, from: u32) -> u32 {
        let (nb, nw) = (self.nb, self.nw);
        let (w0, from) = if from / 64 < nw { (from / 64, from) } else { (0, 0) };
        // The cursor's own word, at and above the cursor.
        let x = self.bits.get((w0 * nb + b) as usize).copied().unwrap_or(0) & (u64::MAX << (from % 64));
        if x != 0 {
            return w0 * 64 + x.trailing_zeros();
        }
        self.first_after(b, w0)
    }

    /// Bucket `b`'s first member in a word after `w0`, wrapping round to the words before it and
    /// `w0`'s own (by the summary: a word per 4,096 runners).
    #[inline]
    fn first_after(&self, b: u32, w0: u32) -> u32 {
        let nb = self.nb;
        let ns = self.nw.div_ceil(64);
        let s0 = w0 / 64;
        let found = |s: u32, m: u64| -> u32 {
            let w = s * 64 + m.trailing_zeros();
            w * 64 + self.bits.get((w * nb + b) as usize).copied().unwrap_or(0).trailing_zeros()
        };
        let m = self.sum.get((s0 * nb + b) as usize).copied().unwrap_or(0);
        let above = m & u64::MAX.checked_shl(w0 % 64 + 1).unwrap_or(0);
        if above != 0 {
            return found(s0, above);
        }
        for s in (s0 + 1..ns).chain(0..s0) {
            let m = self.sum.get((s * nb + b) as usize).copied().unwrap_or(0);
            if m != 0 {
                return found(s, m);
            }
        }
        // Round again to `w0`'s own summary word: its words up to and with `w0`.
        if m != 0 { found(s0, m) } else { NIL }
    }

    /// Bucket `b`'s next member in turn (NIL: none). It stays next until [`Index::took`] moves the
    /// cursor past it.
    #[inline(always)]
    pub fn next_in(&self, b: u32) -> u32 {
        let Some(&k) = self.slots.get(b as usize) else { return NIL };
        #[cfg(test)]
        if super::planted(super::Mutant::NoTurn) {
            return self.first_from(b, 0);
        }
        // The only member: the last put in, if it hasn't left.
        if k.count == 1 && k.last != NIL {
            return k.last;
        }
        self.first_from(b, k.cursor)
    }

    /// How many members bucket `b` holds.
    #[inline]
    pub fn count(&self, b: u32) -> u32 {
        self.slots.get(b as usize).map_or(0, |k| k.count)
    }

    /// The member `n` places after the next in turn in bucket `b` (0: the next in turn), going
    /// round in id order; NIL when it holds no more than `n`. A walk by words, counted by its
    /// caller (the pick that samples among the top buckets, docs/lb.md "Several owners").
    pub fn nth_in(&self, b: u32, n: u32) -> u32 {
        let Some(&k) = self.slots.get(b as usize) else { return NIL };
        if n >= k.count {
            return NIL;
        }
        if n == 0 {
            return self.next_in(b);
        }
        let (nb, nw) = (self.nb, self.nw);
        let c = if k.cursor / 64 < nw { k.cursor } else { 0 };
        let w0 = c / 64;
        let high = u64::MAX << (c % 64);
        let word = |w: u32| self.bits.get((w * nb + b) as usize).copied().unwrap_or(0);
        let mut left = n;
        let mut scan = |w: u32, mut x: u64| -> Option<u32> {
            let ones = x.count_ones();
            if left < ones {
                for _ in 0..left {
                    x &= x - 1;
                }
                return Some(w * 64 + x.trailing_zeros());
            }
            left -= ones;
            None
        };
        let first = word(w0);
        if let Some(r) = scan(w0, first & high) {
            return r;
        }
        for w in (w0 + 1..nw).chain(0..w0) {
            let x = word(w);
            if x != 0
                && let Some(r) = scan(w, x)
            {
                return r;
            }
        }
        scan(w0, first & !high).unwrap_or(NIL)
    }

    /// A pick took member `r` from bucket `b`: the next in turn is after it.
    #[inline(always)]
    pub fn took(&mut self, b: u32, r: u32) {
        if let Some(k) = self.slots.get_mut(b as usize) {
            k.cursor = r.wrapping_add(1);
        }
    }

    /// Bucket `b`'s members from the next in turn on, once round (in id order, wrapping).
    pub fn bucket(&self, b: u32) -> impl Iterator<Item = u32> + '_ {
        let first = self.next_in(b);
        let mut at = first;
        let mut done = first == NIL;
        std::iter::from_fn(move || {
            if done {
                return None;
            }
            let r = at;
            let next = self.first_from(b, r.wrapping_add(1));
            if next == first || next == NIL {
                done = true;
            }
            at = next;
            Some(r)
        })
    }

    /// Bucket `b`'s members from the next in turn on, once round, as if member `moved.0` had left
    /// bucket `moved.1` for bucket `moved.2`: what a read sees of a move put off (by its owner, so
    /// no pick, order or peek depends on whether it has happened yet).
    pub fn bucket_as(&self, b: u32, moved: Option<(u32, u32, u32)>) -> Vec<u32> {
        let mut v: Vec<u32> = self.bucket(b).filter(|r| !moved.is_some_and(|(m, from, _)| b == from && *r == m)).collect();
        if let Some((m, _, to)) = moved
            && to == b
        {
            // Its place among the bucket's members, in turn order from the cursor.
            let c = self.slots.get(b as usize).map_or(0, |k| k.cursor);
            let c = if c / 64 < self.nw { c } else { 0 };
            let i = v.partition_point(|x| x.wrapping_sub(c) < m.wrapping_sub(c));
            v.insert(i, m);
        }
        v
    }

    /// The highest bucket below `b` that holds a member, with a move put off (as `bucket_as`).
    pub fn below_as(&self, b: u32, moved: Option<(u32, u32, u32)>) -> Option<u32> {
        let n = self.below(b);
        match moved {
            Some((_, _, to)) if to < b && n.is_none_or(|x| to > x) => Some(to),
            _ => n,
        }
    }

    /// Whether bucket `b` holds runner `r`.
    #[inline(always)]
    pub fn holds(&self, b: u32, r: u32) -> bool {
        self.bits.get(((r / 64) * self.nb + b) as usize).is_some_and(|x| x & (1u64 << (r % 64)) != 0)
    }
}
