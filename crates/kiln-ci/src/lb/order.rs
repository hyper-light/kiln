//! An order of a signature's members by one measured rate (docs/lb.md "Orders"): per-core speed
//! under contention, disk, the owner's link, the runner's network. A member is in the class of its
//! rate, classes highest first, members in turn within a class. A class changes when the rate
//! does (a measure, a link's new rate), never on a start or an end: room is checked at the pick,
//! member by member (the narrow order's owner keeps only members with a core free in it).
//!
//! Memory is O(runners + classes): a ring per class through each member's neighbours, and a
//! bitmap of the classes that hold one, two levels of 64-bit words as [`super::Index`]'s.

use super::NIL;
use super::index::BUCKETS;

/// A rate's class: its place on a log scale, the octave (its top bit) and the six bits below
/// it. 64 octaves of a u64 times 64 classes an octave is [`BUCKETS`]: two leading-zeros counts
/// find the highest. Rates in one class are within 1/64 of each other (the six bits' step at
/// the octave's foot); 0 and 1 share class 0.
#[inline]
pub fn class_of(rate: u64) -> u32 {
    if rate == 0 {
        return 0;
    }
    let e = 63 - rate.leading_zeros();
    let m = if e >= 6 { (rate >> (e - 6)) & 63 } else { (rate << (6 - e)) & 63 };
    e * 64 + m as u32
}

/// The least rate in class `c`: (64 + m) << (e - 6) for class (e, m), e >= 6; below 64 (a rate
/// of no real machine), the octave's foot. Members of a class aren't ordered within it, so what
/// reads rates at the kernel's resolution reads this.
#[inline]
pub fn class_lo(c: u32) -> u64 {
    let (e, m) = (c / 64, u64::from(c % 64));
    if e >= 6 { (64 + m) << (e - 6) } else { 1u64 << e }
}

/// The most a rate in class `c` or any below it can be: a bound for the members an order's walk
/// hasn't reached. Class (e, m), e >= 6, holds [(64 + m) << (e - 6), (65 + m) << (e - 6)); below
/// 64 (a rate of no real machine) the octave's top bounds it.
#[inline]
pub fn class_hi(c: u32) -> u64 {
    let (e, m) = (c / 64, u128::from(c % 64));
    if e >= 6 {
        u64::try_from(((65 + m) << (e - 6)) - 1).unwrap_or(u64::MAX)
    } else {
        (2u64 << e) - 1
    }
}

/// The members of one signature by one rate's class.
#[derive(Debug, Clone)]
pub struct Order {
    /// Each class's next member in turn (NIL: empty).
    head: Vec<u32>,
    /// Each runner's class (NIL: not in the order) and its neighbours in its class's ring.
    class: Vec<u32>,
    next: Vec<u32>,
    prev: Vec<u32>,
    /// Class `64 w + i` holds a member when bit `i` of `words[w]` is set; bit `w` of `summary`
    /// when `words[w]` isn't 0.
    words: [u64; 64],
    summary: u64,
}

impl Default for Order {
    fn default() -> Self {
        Order::new()
    }
}

impl Order {
    pub fn new() -> Order {
        Order { head: vec![NIL; BUCKETS as usize], class: Vec::new(), next: Vec::new(), prev: Vec::new(), words: [0; 64], summary: 0 }
    }

    /// Runner `r`'s class (NIL: not in the order).
    #[inline]
    pub fn class(&self, r: u32) -> u32 {
        self.class.get(r as usize).copied().unwrap_or(NIL)
    }

    /// Puts runner `r` in class `c`, out of the one it was in (NIL: out of the order). A member
    /// put in a class is last in its turn.
    pub fn set(&mut self, r: u32, c: u32) {
        let was = self.class(r);
        if was == c {
            return;
        }
        if was != NIL {
            self.unlink(r, was);
        }
        if c != NIL && c < BUCKETS {
            self.link(r, c);
        }
    }

    fn link(&mut self, r: u32, c: u32) {
        let n = r as usize + 1;
        if self.class.len() < n {
            self.class.resize(n, NIL);
            self.next.resize(n, NIL);
            self.prev.resize(n, NIL);
        }
        let h = self.head.get(c as usize).copied().unwrap_or(NIL);
        let (p, nx) = if h == NIL {
            if let Some(x) = self.head.get_mut(c as usize) {
                *x = r;
            }
            if let Some(w) = self.words.get_mut((c / 64) as usize) {
                *w |= 1u64 << (c % 64);
            }
            self.summary |= 1u64 << (c / 64);
            (r, r)
        } else {
            let t = self.prev.get(h as usize).copied().unwrap_or(h);
            if let Some(x) = self.next.get_mut(t as usize) {
                *x = r;
            }
            if let Some(x) = self.prev.get_mut(h as usize) {
                *x = r;
            }
            (t, h)
        };
        if let Some(x) = self.prev.get_mut(r as usize) {
            *x = p;
        }
        if let Some(x) = self.next.get_mut(r as usize) {
            *x = nx;
        }
        if let Some(x) = self.class.get_mut(r as usize) {
            *x = c;
        }
    }

    fn unlink(&mut self, r: u32, c: u32) {
        let (p, n) = (self.prev.get(r as usize).copied().unwrap_or(NIL), self.next.get(r as usize).copied().unwrap_or(NIL));
        if n == r || n == NIL {
            if let Some(x) = self.head.get_mut(c as usize) {
                *x = NIL;
            }
            if let Some(w) = self.words.get_mut((c / 64) as usize) {
                *w &= !(1u64 << (c % 64));
                if *w == 0 {
                    self.summary &= !(1u64 << (c / 64));
                }
            }
        } else {
            if let Some(x) = self.next.get_mut(p as usize) {
                *x = n;
            }
            if let Some(x) = self.prev.get_mut(n as usize) {
                *x = p;
            }
            if let Some(x) = self.head.get_mut(c as usize)
                && *x == r
            {
                *x = n;
            }
        }
        for v in [self.class.get_mut(r as usize), self.next.get_mut(r as usize), self.prev.get_mut(r as usize)].into_iter().flatten() {
            *v = NIL;
        }
    }

    /// Class `c`'s next member in turn (NIL: empty).
    #[inline]
    pub fn head(&self, c: u32) -> u32 {
        self.head.get(c as usize).copied().unwrap_or(NIL)
    }

    /// The member after `r` in its class's ring (NIL: `r` isn't in the order).
    #[inline]
    pub fn after(&self, r: u32) -> u32 {
        self.next.get(r as usize).copied().unwrap_or(NIL)
    }

    /// Whether class `c` holds a member.
    #[inline]
    pub fn has(&self, c: u32) -> bool {
        self.words.get((c / 64) as usize).is_some_and(|w| w & (1u64 << (c % 64)) != 0)
    }

    /// The highest class that holds a member.
    #[inline]
    pub fn top(&self) -> Option<u32> {
        if self.summary == 0 {
            return None;
        }
        let w = 63 - self.summary.leading_zeros();
        let bits = *self.words.get(w as usize)?;
        Some(w * 64 + 63u32.checked_sub(bits.leading_zeros())?)
    }

    /// The highest class below `c` that holds a member.
    pub fn below(&self, c: u32) -> Option<u32> {
        let (w, i) = (c / 64, c % 64);
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

    /// Class `c`'s members from the next in turn, once round.
    pub fn ring(&self, c: u32) -> impl Iterator<Item = u32> + '_ {
        let first = self.head.get(c as usize).copied().unwrap_or(NIL);
        let mut at = first;
        std::iter::from_fn(move || {
            if at == NIL {
                return None;
            }
            let r = at;
            let n = self.next.get(r as usize).copied().unwrap_or(NIL);
            at = if n == first { NIL } else { n };
            Some(r)
        })
    }

    /// A pick took member `r`: the next in turn in its class is after it.
    pub fn took(&mut self, r: u32) {
        let c = self.class(r);
        let n = self.next.get(r as usize).copied().unwrap_or(NIL);
        if c != NIL
            && n != NIL
            && let Some(h) = self.head.get_mut(c as usize)
        {
            *h = n;
        }
    }
}
