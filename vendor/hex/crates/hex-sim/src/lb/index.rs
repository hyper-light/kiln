//! A signature's members, bucketed by their rank key (kiln `crates/kiln-ci/src/lb/index.rs` at
//! f20858c9c): a signature's eligible nodes, kept as nodes join and leave, so a pick looks only at
//! them.
//!
//! Bucket `b` holds the members whose key is `b`: free whole CPUs, as their owner sees them.
//! Membership is a bitset per bucket over node ids, interleaved by word (bucket `b`'s word `w` sits
//! at `w * buckets + b`), so one node's bits for every bucket share a cache line or two and a move
//! is two bit writes there. A summary bitset per bucket says which of its words hold any member,
//! and a bitmap of the buckets that hold any gives the highest in two leading-zeros counts, and
//! (Hex's addition, for the pick that packs) the lowest at or above a bucket in two trailing-zeros
//! counts. Equals take turns: each bucket has a cursor, a pick takes the first member at or after
//! it (in id order, wrapping) and moves it past the one taken.
//!
//! Every operation is O(1) for fleets up to 4,096 x 64 nodes per summary word, except a walk,
//! which goes member by member and is counted by its caller.

use super::{Amounts, NIL, ix, within};

/// The most buckets an index has: two levels of 64-bit words, 64 x 64, so the highest non-empty
/// bucket is two leading-zeros counts away. A member whose key is past that is in the last bucket.
pub const BUCKETS: u32 = 64 * 64;

/// The bit for `i` within its word.
#[inline(always)]
fn bit(i: u32) -> u64 {
    1u64.wrapping_shl(i & 63)
}

/// The 64-bit word `x` is in (x / 64).
#[inline(always)]
fn word_of(x: u32) -> u32 {
    x.wrapping_shr(6)
}

/// `w * 64 + i`: the bucket or node of bit `i` of word `w`.
#[inline(always)]
fn join_bit(w: u32, i: u32) -> Option<u32> {
    w.checked_mul(64)?.checked_add(i)
}

/// The highest set bit of a word that has one.
#[inline(always)]
fn hi(x: u64) -> Option<u32> {
    63u32.checked_sub(x.leading_zeros()).filter(|_| x != 0)
}

/// `w * n + b`, an index into an interleaved table (usize::MAX, which finds nothing, past usize).
#[inline(always)]
fn cell(w: u32, n: u32, b: u32) -> usize {
    u64::from(w)
        .checked_mul(u64::from(n))
        .and_then(|x| x.checked_add(u64::from(b)))
        .and_then(|x| usize::try_from(x).ok())
        .unwrap_or(usize::MAX)
}

/// A bucket's own state.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Members in it.
    count: u32,
    /// The node its next pick starts from.
    cursor: u32,
    /// The last member put in, while it is still there (NIL: unknown): the next in turn when it
    /// is the only one, so the commonest pick needs no search.
    last: u32,
}

/// The members of one signature, by rank key.
#[derive(Debug, Clone)]
pub struct Index {
    /// Buckets, and 64-node words, it has room for.
    nb: u32,
    nw: u32,
    /// Bucket `b`'s membership word `w`: bit `i` is node `64 w + i`. At `w * nb + b`.
    bits: Vec<u64>,
    /// Bucket `b`'s summary word `s`: bit `i` is membership word `64 s + i` not 0. At `s * nb + b`.
    sum: Vec<u64>,
    /// Each bucket's count, cursor and last member put in, together: a pick reads them at once.
    slots: Vec<Bucket>,
    /// Where cursors start: the owner's spread over the fleet.
    start: u32,
    /// Bucket `64 w + i` holds a member when bit `i` of `words[w]` is set; bit `w` of `summary`
    /// is set when `words[w]` is not 0.
    words: [u64; 64],
    summary: u64,
    /// Which nodes are members, bucketed or not (a member whose link is down isn't bucketed).
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
    pub fn try_clone(&self) -> Option<Self> {
        fn copy<T: Copy>(values: &[T]) -> Option<Vec<T>> {
            let mut out = Vec::new();
            out.try_reserve_exact(values.len()).ok()?;
            out.extend_from_slice(values);
            Some(out)
        }
        Some(Self {
            nb: self.nb,
            nw: self.nw,
            bits: copy(&self.bits)?,
            sum: copy(&self.sum)?,
            slots: copy(&self.slots)?,
            start: self.start,
            words: self.words,
            summary: self.summary,
            member: copy(&self.member)?,
            members: self.members,
            shapes: copy(&self.shapes)?,
        })
    }

    /// An empty index whose cursors start at `start` (a node id, taken modulo the fleet).
    pub fn new(start: u32) -> Index {
        Index {
            nb: 0,
            nw: 0,
            bits: Vec::new(),
            sum: Vec::new(),
            slots: Vec::new(),
            start,
            words: [0; 64],
            summary: 0,
            member: Vec::new(),
            members: 0,
            shapes: Vec::new(),
        }
    }

    /// Makes room for `buckets` buckets (at most [`BUCKETS`]) and nodes numbered below `runners`.
    /// False, and nothing changed, when its tables can't be that large on this target.
    pub fn grow(&mut self, buckets: u32, runners: u32) -> bool {
        let nb = buckets.clamp(1, BUCKETS).max(self.nb);
        let nw = runners.div_ceil(64).max(self.nw).max(1);
        if nb == self.nb && nw == self.nw {
            return true;
        }
        let ns = nw.div_ceil(64);
        let (Some(nbits), Some(nsum)) = (
            usize::try_from(u64::from(nw).saturating_mul(u64::from(nb))).ok(),
            usize::try_from(u64::from(ns).saturating_mul(u64::from(nb))).ok(),
        ) else {
            return false;
        };
        let mut bits = vec![0u64; nbits];
        let mut sum = vec![0u64; nsum];
        for w in 0..self.nw {
            for b in 0..self.nb {
                let x = self.bits.get(cell(w, self.nb, b)).copied().unwrap_or(0);
                if let Some(d) = bits.get_mut(cell(w, nb, b)) {
                    *d = x;
                }
                if x != 0
                    && let Some(s) = sum.get_mut(cell(word_of(w), nb, b))
                {
                    *s |= bit(w);
                }
            }
        }
        self.bits = bits;
        self.sum = sum;
        // A cursor starts at the owner's spread over the fleet it has now: start / 2^32 of it.
        let fleet = u64::from(nw).saturating_mul(64);
        let at = u32::try_from(u64::from(self.start).saturating_mul(fleet).wrapping_shr(32))
            .unwrap_or(0);
        self.slots.resize(
            ix(nb),
            Bucket {
                count: 0,
                cursor: at,
                last: NIL,
            },
        );
        self.member.resize(ix(nw), 0);
        self.nb = nb;
        self.nw = nw;
        true
    }

    /// How many nodes are members.
    pub fn len(&self) -> u32 {
        self.members
    }

    pub fn is_empty(&self) -> bool {
        self.members == 0
    }

    /// Whether node `r` is a member.
    pub fn contains(&self, r: u32) -> bool {
        self.member
            .get(ix(word_of(r)))
            .is_some_and(|w| w & bit(r) != 0)
    }

    /// The members, in node order.
    pub fn members(&self) -> impl Iterator<Item = u32> + '_ {
        self.member.iter().zip(0u32..).flat_map(|(&w, i)| {
            (0..64u32)
                .filter(move |b| w & bit(*b) != 0)
                .filter_map(move |b| join_bit(i, b))
        })
    }

    /// Makes `r`, whose budget is `budget`, a member (not yet in a bucket). False when it was one,
    /// or when the index can't grow to hold it.
    pub fn join(&mut self, r: u32, budget: &Amounts) -> bool {
        if self.contains(r) || !self.grow(self.nb, r.saturating_add(1)) {
            return false;
        }
        if let Some(w) = self.member.get_mut(ix(word_of(r))) {
            *w |= bit(r);
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
        if let Some(w) = self.member.get_mut(ix(word_of(r))) {
            *w &= !bit(r);
        }
        self.members = self.members.saturating_sub(1);
        self.unshape(budget);
        true
    }

    fn unshape(&mut self, budget: &Amounts) {
        if let Some(i) = self.shapes.iter().position(|(b, _)| b == budget)
            && let Some((_, n)) = self.shapes.get_mut(i)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.shapes.swap_remove(i);
            }
        }
    }

    /// A member's budget changed from `old` to `new`.
    pub fn reshape(&mut self, old: &Amounts, new: &Amounts) {
        self.unshape(old);
        match self.shapes.iter_mut().find(|(b, _)| b == new) {
            Some((_, n)) => *n = n.saturating_add(1),
            None => self.shapes.push((*new, 1)),
        }
    }

    /// Whether some member's whole budget covers `need`: false means it never fits here.
    pub fn could_fit(&self, need: &Amounts) -> bool {
        self.shapes.iter().any(|(b, _)| within(need, b))
    }

    #[inline]
    fn set(&mut self, b: u32) {
        let w = word_of(b);
        if let Some(word) = self.words.get_mut(ix(w)) {
            *word |= bit(b);
            self.summary |= bit(w);
        }
    }

    #[inline]
    fn unset(&mut self, b: u32) {
        let w = word_of(b);
        if let Some(word) = self.words.get_mut(ix(w)) {
            *word &= !bit(b);
            if *word == 0 {
                self.summary &= !bit(w);
            }
        }
    }

    /// Puts member `r` in bucket `b`.
    #[inline(always)]
    pub fn push(&mut self, r: u32, b: u32) {
        let (w, nb) = (word_of(r), self.nb);
        let Some(x) = self.bits.get_mut(cell(w, nb, b)) else {
            return;
        };
        let was = *x;
        *x = was | bit(r);
        if was == 0
            && let Some(s) = self.sum.get_mut(cell(word_of(w), nb, b))
        {
            *s |= bit(w);
        }
        if let Some(k) = self.slots.get_mut(ix(b)) {
            k.count = k.count.saturating_add(1);
            k.last = r;
            if k.count == 1 {
                self.set(b);
            }
        }
    }

    /// Takes member `r` out of bucket `b`.
    #[inline(always)]
    pub fn unlink(&mut self, r: u32, b: u32) {
        let (w, nb) = (word_of(r), self.nb);
        let Some(x) = self.bits.get_mut(cell(w, nb, b)) else {
            return;
        };
        let left = *x & !bit(r);
        *x = left;
        if left == 0
            && let Some(s) = self.sum.get_mut(cell(word_of(w), nb, b))
        {
            *s &= !bit(w);
        }
        if let Some(k) = self.slots.get_mut(ix(b)) {
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
        // Buckets below 64 are the first word alone: read with the summary, not after it.
        let (low, summary) = (self.words.first().copied().unwrap_or(0), self.summary);
        if summary <= 1 {
            return hi(low);
        }
        let w = hi(summary)?;
        let bits = *self.words.get(ix(w))?;
        join_bit(w, hi(bits)?)
    }

    /// The highest bucket below `b` that holds a member.
    pub fn below(&self, b: u32) -> Option<u32> {
        let w = word_of(b);
        let bits = self.words.get(ix(w)).copied().unwrap_or(0) & bit(b).wrapping_sub(1);
        if bits != 0 {
            return join_bit(w, hi(bits)?);
        }
        let lower = self.summary & bit(w).wrapping_sub(1);
        let w = hi(lower)?;
        let bits = *self.words.get(ix(w))?;
        join_bit(w, hi(bits)?)
    }

    /// The lowest bucket at or above `b` that holds a member: where the pick that packs starts
    /// (Hex's adaptation; kiln reads only from the top down).
    pub fn at_or_above(&self, b: u32) -> Option<u32> {
        let w = word_of(b);
        if w >= 64 {
            return None;
        }
        let bits = self.words.get(ix(w)).copied().unwrap_or(0) & !bit(b).wrapping_sub(1);
        if bits != 0 {
            return join_bit(w, bits.trailing_zeros());
        }
        // The words strictly above `w`.
        let higher = self.summary & !bit(w).wrapping_sub(1) & !bit(w);
        if higher == 0 {
            return None;
        }
        let w = higher.trailing_zeros();
        let bits = *self.words.get(ix(w))?;
        join_bit(w, bits.trailing_zeros())
    }

    /// The lowest bucket above `b` that holds a member.
    pub fn above(&self, b: u32) -> Option<u32> {
        self.at_or_above(b.checked_add(1)?)
    }

    /// Bucket `b`'s member at or after node `from`, wrapping (NIL: the bucket is empty).
    #[inline]
    fn first_from(&self, b: u32, from: u32) -> u32 {
        let (nb, nw) = (self.nb, self.nw);
        let (w0, from) = if word_of(from) < nw {
            (word_of(from), from)
        } else {
            (0, 0)
        };
        // The cursor's own word, at and above the cursor.
        let x = self.bits.get(cell(w0, nb, b)).copied().unwrap_or(0) & !bit(from).wrapping_sub(1);
        if x != 0 {
            return join_bit(w0, x.trailing_zeros()).unwrap_or(NIL);
        }
        self.first_after(b, w0)
    }

    /// Bucket `b`'s first member in a word after `w0`, wrapping round to the words before it and
    /// `w0`'s own (by the summary: a word per 4,096 nodes).
    #[inline]
    fn first_after(&self, b: u32, w0: u32) -> u32 {
        let nb = self.nb;
        let ns = self.nw.div_ceil(64);
        let s0 = word_of(w0);
        let found = |s: u32, m: u64| -> u32 {
            let Some(w) = join_bit(s, m.trailing_zeros()) else {
                return NIL;
            };
            let x = self.bits.get(cell(w, nb, b)).copied().unwrap_or(0);
            join_bit(w, x.trailing_zeros()).unwrap_or(NIL)
        };
        let m = self.sum.get(cell(s0, nb, b)).copied().unwrap_or(0);
        // The words after `w0` in its own summary word.
        let above = m & !bit(w0).wrapping_sub(1) & !bit(w0);
        if above != 0 {
            return found(s0, above);
        }
        for s in (s0.saturating_add(1)..ns).chain(0..s0) {
            let m = self.sum.get(cell(s, nb, b)).copied().unwrap_or(0);
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
        let Some(&k) = self.slots.get(ix(b)) else {
            return NIL;
        };
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

    /// A pick took member `r` from bucket `b`: the next in turn is after it.
    #[inline(always)]
    pub fn took(&mut self, b: u32, r: u32) {
        if let Some(k) = self.slots.get_mut(ix(b)) {
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

    /// Whether bucket `b` holds node `r`.
    #[inline(always)]
    pub fn holds(&self, b: u32, r: u32) -> bool {
        self.bits
            .get(cell(word_of(r), self.nb, b))
            .is_some_and(|x| x & bit(r) != 0)
    }
}
