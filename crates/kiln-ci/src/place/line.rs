//! A runner's line (docs/needs.md §5, §11): the runs waiting there, in the order the runner takes
//! them, kept so that every change and every question is O(log n) in the runs waiting, with no
//! allocation once its slab has grown to the line's longest: a treap (Seidel and Aragon's
//! randomized search tree, its priorities a hash of each entry's arrival so that every replay
//! builds the same tree, docs/needs.md §9's "replay is exact"), each node summing its subtree.
//!
//! - **Order**: by [`Key`]: class first (W05-R30), then the run's start tag in its class's fair
//!   share (start-time fair queuing, [`super::runner`]), then arrival.
//! - **Place**: how many come before a key ([`Line::rank`]), and the one at a place
//!   ([`Line::select`]).
//! - **What's ahead**: the sums of what comes before a key ([`Line::ahead`]): how many, the work
//!   each dimension holds (amount times predicted time), how many have no prediction, and their
//!   timeouts; what an estimate of the start is made from.
//! - **The first that fits**: the first in order that fits what's free, predicted to end within a
//!   time, and that the caller accepts ([`Line::find`]), each subtree passed over at once when the
//!   least it asks in each dimension doesn't fit, or its shortest predicted run is too long.

use super::ask::{Amounts, DIMS, fits};
use crate::sched::units::Ms;

/// No node.
const NIL: u32 = u32::MAX;

/// Where an entry stands in a line: its class's place in [`crate::sched::state::Class::ALL`], its
/// start tag, and its arrival (unique, so no two keys are alike).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub class: u8,
    pub tag: u128,
    pub seq: u64,
}

/// What an entry is: the runner's slot for it, what it asks, how long it's predicted to run
/// here, and its timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub slot: u32,
    pub ask: Amounts,
    pub predicted: Option<Ms>,
    pub timeout: Option<Ms>,
}

/// The sums over some entries of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sums {
    /// How many.
    pub count: u32,
    /// The least each asks, dimension by dimension.
    pub least: Amounts,
    /// The shortest predicted run among them (`u64::MAX`: none predicted).
    pub shortest: u64,
    /// Each dimension's amount times predicted milliseconds, over those with a prediction.
    pub work: [u128; DIMS],
    /// How many have no prediction.
    pub unpredicted: u32,
    /// Their timeouts' sum, over those with one.
    pub timeouts: u128,
    /// How many have no timeout.
    pub untimed: u32,
}

impl Sums {
    /// Over nothing.
    pub const NONE: Sums = Sums { count: 0, least: [u64::MAX; DIMS], shortest: u64::MAX, work: [0; DIMS], unpredicted: 0, timeouts: 0, untimed: 0 };

    /// Over one entry.
    pub fn of(i: &Item) -> Sums {
        let mut work = [0u128; DIMS];
        if let Some(p) = i.predicted {
            for (w, a) in work.iter_mut().zip(&i.ask) {
                *w = u128::from(*a).saturating_mul(u128::from(p));
            }
        }
        Sums {
            count: 1,
            least: i.ask,
            shortest: i.predicted.unwrap_or(u64::MAX),
            work,
            unpredicted: u32::from(i.predicted.is_none()),
            timeouts: i.timeout.map_or(0, u128::from),
            untimed: u32::from(i.timeout.is_none()),
        }
    }

    /// These and `o` together.
    pub fn with(&self, o: &Sums) -> Sums {
        let mut least = self.least;
        for (l, x) in least.iter_mut().zip(&o.least) {
            *l = (*l).min(*x);
        }
        let mut work = self.work;
        for (w, x) in work.iter_mut().zip(&o.work) {
            *w = w.saturating_add(*x);
        }
        Sums {
            count: self.count.saturating_add(o.count),
            least,
            shortest: self.shortest.min(o.shortest),
            work,
            unpredicted: self.unpredicted.saturating_add(o.unpredicted),
            timeouts: self.timeouts.saturating_add(o.timeouts),
            untimed: self.untimed.saturating_add(o.untimed),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Node {
    key: Key,
    slot: u32,
    prio: u64,
    left: u32,
    right: u32,
    own: Sums,
    all: Sums,
}

/// What [`Line::find`] looks for.
#[derive(Debug, Clone, Copy)]
pub struct Query<'a> {
    /// Only entries after this key.
    pub after: Option<&'a Key>,
    /// Only what fits this.
    pub free: &'a Amounts,
    /// Only what's predicted to run at most this long (`u64::MAX`: any, a run with no prediction
    /// among them).
    pub within: u64,
}

/// A runner's line.
#[derive(Debug, Clone)]
pub struct Line {
    nodes: Vec<Node>,
    spare: Vec<u32>,
    root: u32,
}

impl Default for Line {
    fn default() -> Self {
        Line { nodes: Vec::new(), spare: Vec::new(), root: NIL }
    }
}

fn ix(t: u32) -> usize {
    t as usize
}

/// SplitMix64's finalizer (Steele, Lea and Flood, "Fast splittable pseudorandom number
/// generators", OOPSLA 2014): a treap's priority from an entry's arrival, so the same arrivals
/// build the same tree on every machine.
fn mix(seq: u64) -> u64 {
    let mut z = seq.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Line {
    /// How many wait in it.
    pub fn len(&self) -> u32 {
        self.sums(self.root).count
    }

    pub fn is_empty(&self) -> bool {
        self.root == NIL
    }

    fn sums(&self, t: u32) -> Sums {
        self.nodes.get(ix(t)).map_or(Sums::NONE, |n| n.all)
    }

    fn node(&self, t: u32) -> Option<Node> {
        self.nodes.get(ix(t)).copied()
    }

    fn set(&mut self, t: u32, left: u32, right: u32) {
        let (l, r) = (self.sums(left), self.sums(right));
        if let Some(n) = self.nodes.get_mut(ix(t)) {
            n.left = left;
            n.right = right;
            n.all = l.with(&n.own).with(&r);
        }
    }

    /// Splits `t` into the entries before `key` and the rest.
    fn split(&mut self, t: u32, key: &Key) -> (u32, u32) {
        let Some(n) = self.node(t) else { return (NIL, NIL) };
        if n.key < *key {
            let (a, b) = self.split(n.right, key);
            self.set(t, n.left, a);
            (t, b)
        } else {
            let (a, b) = self.split(n.left, key);
            self.set(t, b, n.right);
            (a, t)
        }
    }

    /// Joins `a` and `b`, every key of `a` before every key of `b`.
    fn merge(&mut self, a: u32, b: u32) -> u32 {
        let (Some(na), Some(nb)) = (self.node(a), self.node(b)) else { return if a == NIL { b } else { a } };
        if na.prio >= nb.prio {
            let m = self.merge(na.right, b);
            self.set(a, na.left, m);
            a
        } else {
            let m = self.merge(a, nb.left);
            self.set(b, m, nb.right);
            b
        }
    }

    /// Puts `item` in at `key` (a key already in it is left as it is).
    pub fn insert(&mut self, key: Key, item: Item) {
        if self.contains(&key) {
            return;
        }
        let own = Sums::of(&item);
        let n = Node { key, slot: item.slot, prio: mix(key.seq), left: NIL, right: NIL, own, all: own };
        let t = match self.spare.pop() {
            Some(t) => {
                if let Some(slot) = self.nodes.get_mut(ix(t)) {
                    *slot = n;
                }
                t
            }
            None => {
                self.nodes.push(n);
                u32::try_from(self.nodes.len().saturating_sub(1)).unwrap_or(NIL)
            }
        };
        let root = self.root;
        let (a, b) = self.split(root, &key);
        let left = self.merge(a, t);
        self.root = self.merge(left, b);
    }

    /// Whether `key` is in it.
    pub fn contains(&self, key: &Key) -> bool {
        let mut t = self.root;
        while let Some(n) = self.nodes.get(ix(t)) {
            match key.cmp(&n.key) {
                std::cmp::Ordering::Less => t = n.left,
                std::cmp::Ordering::Greater => t = n.right,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }

    fn erase(&mut self, t: u32, key: &Key, found: &mut bool) -> u32 {
        let Some(n) = self.node(t) else { return NIL };
        match key.cmp(&n.key) {
            std::cmp::Ordering::Less => {
                let l = self.erase(n.left, key, found);
                self.set(t, l, n.right);
                t
            }
            std::cmp::Ordering::Greater => {
                let r = self.erase(n.right, key, found);
                self.set(t, n.left, r);
                t
            }
            std::cmp::Ordering::Equal => {
                *found = true;
                self.spare.push(t);
                self.merge(n.left, n.right)
            }
        }
    }

    /// Takes `key` out: whether it was in.
    pub fn remove(&mut self, key: &Key) -> bool {
        let mut found = false;
        let root = self.root;
        self.root = self.erase(root, key, &mut found);
        found
    }

    /// How many come before `key`.
    pub fn rank(&self, key: &Key) -> u32 {
        self.ahead(key).count
    }

    /// The sums over what comes before `key`.
    pub fn ahead(&self, key: &Key) -> Sums {
        let mut acc = Sums::NONE;
        let mut t = self.root;
        while let Some(n) = self.nodes.get(ix(t)) {
            if n.key < *key {
                acc = acc.with(&self.sums(n.left)).with(&n.own);
                t = n.right;
            } else {
                t = n.left;
            }
        }
        acc
    }

    /// The entry at place `k`, the first 0: its key and slot.
    pub fn select(&self, k: u32) -> Option<(Key, u32)> {
        let (mut t, mut k) = (self.root, k);
        while let Some(n) = self.nodes.get(ix(t)) {
            let before = self.sums(n.left).count;
            match k.cmp(&before) {
                std::cmp::Ordering::Less => t = n.left,
                std::cmp::Ordering::Equal => return Some((n.key, n.slot)),
                std::cmp::Ordering::Greater => {
                    k = k.saturating_sub(before.saturating_add(1));
                    t = n.right;
                }
            }
        }
        None
    }

    /// The first entry: its key and slot.
    pub fn first(&self) -> Option<(Key, u32)> {
        self.select(0)
    }

    /// The first entry in order that `q` describes and `ok` accepts: its key and slot.
    pub fn find(&self, q: &Query<'_>, ok: &mut dyn FnMut(u32) -> bool) -> Option<(Key, u32)> {
        let t = self.find_in(self.root, q, ok)?;
        self.node(t).map(|n| (n.key, n.slot))
    }

    fn find_in(&self, t: u32, q: &Query<'_>, ok: &mut dyn FnMut(u32) -> bool) -> Option<u32> {
        let n = self.nodes.get(ix(t))?;
        if !fits(&n.all.least, q.free) || n.all.shortest > q.within {
            return None;
        }
        if q.after.is_none_or(|a| n.key > *a) {
            if let Some(found) = self.find_in(n.left, q, ok) {
                return Some(found);
            }
            if fits(&n.own.least, q.free) && n.own.shortest <= q.within && ok(n.slot) {
                return Some(t);
            }
        }
        self.find_in(n.right, q, ok)
    }

    /// Every entry in order: its key and slot (for views and tests; O(n)).
    pub fn entries(&self) -> Vec<(Key, u32)> {
        let mut out = Vec::new();
        self.walk(self.root, &mut out);
        out
    }

    fn walk(&self, t: u32, out: &mut Vec<(Key, u32)>) {
        let Some(n) = self.nodes.get(ix(t)) else { return };
        self.walk(n.left, out);
        out.push((n.key, n.slot));
        self.walk(n.right, out);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;

    fn key(class: u8, tag: u128, seq: u64) -> Key {
        Key { class, tag, seq }
    }

    fn item(slot: u32, cpu: u64, predicted: Option<Ms>) -> Item {
        Item { slot, ask: [cpu, 0, 0, 0], predicted, timeout: predicted }
    }

    /// The line's order, places and sums ahead, and its search for the first that fits, as a
    /// sorted map has them.
    #[test]
    fn e5_a_line_keeps_order_places_and_sums() -> Result<(), String> {
        let mut l = Line::default();
        l.insert(key(3, 5, 1), item(1, 4000, Some(10)));
        l.insert(key(3, 2, 2), item(2, 1000, None));
        l.insert(key(2, 9, 3), item(3, 8000, Some(20)));
        assert_eq!(l.entries().iter().map(|(_, s)| *s).collect::<Vec<_>>(), vec![3, 2, 1]);
        assert_eq!(l.rank(&key(3, 5, 1)), 2);
        let a = l.ahead(&key(3, 5, 1));
        let ([work, ..], [least, ..]) = (a.work, a.least);
        assert_eq!((a.count, a.unpredicted, work, least), (2, 1, 160_000, 1000));
        assert_eq!(l.select(1).map(|(_, s)| s), Some(2));
        let free = [4000, 0, 0, 0];
        assert_eq!(l.find(&Query { after: None, free: &free, within: u64::MAX }, &mut |_| true).map(|(_, s)| s), Some(2));
        assert_eq!(l.find(&Query { after: None, free: &free, within: 15 }, &mut |_| true).map(|(_, s)| s), Some(1), "only what ends in time");
        assert_eq!(l.find(&Query { after: None, free: &free, within: u64::MAX }, &mut |s| s != 2).map(|(_, s)| s), Some(1));
        assert!(l.remove(&key(3, 2, 2)) && !l.remove(&key(3, 2, 2)));
        assert_eq!(l.len(), 2);
        // A slot freed is used again.
        l.insert(key(1, 0, 4), item(4, 1, None));
        assert_eq!(l.first().map(|(_, s)| s), Some(4));
        Ok(())
    }

    #[derive(Debug, Clone)]
    enum Op {
        Insert { class: u8, tag: u8, cpu: u8, predicted: Option<u8> },
        Remove { pick: u8 },
        Find { cpu: u8, within: Option<u8>, after: Option<u8> },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..3u8, 0..6u8, 0..8u8, proptest::option::of(0..9u8)).prop_map(|(class, tag, cpu, predicted)| Op::Insert { class, tag, cpu, predicted }),
            any::<u8>().prop_map(|pick| Op::Remove { pick }),
            (0..9u8, proptest::option::of(0..9u8), proptest::option::of(any::<u8>())).prop_map(|(cpu, within, after)| Op::Find { cpu, within, after }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// docs/needs.md §11: the line answers as a sorted map does, after any sequence of
        /// arrivals, departures and searches.
        #[test]
        fn e5_a_line_answers_as_a_sorted_map_does(ops in proptest::collection::vec(op(), 1..120)) {
            let mut l = Line::default();
            let mut m: BTreeMap<Key, Item> = BTreeMap::new();
            let mut seq = 0u64;
            for o in ops {
                match o {
                    Op::Insert { class, tag, cpu, predicted } => {
                        seq += 1;
                        let k = key(class, u128::from(tag), seq);
                        let i = item(u32::try_from(seq).unwrap_or(0), u64::from(cpu), predicted.map(u64::from));
                        l.insert(k, i);
                        m.insert(k, i);
                    }
                    Op::Remove { pick } => {
                        let k = m.keys().nth(usize::from(pick) % m.len().max(1)).copied();
                        if let Some(k) = k {
                            m.remove(&k);
                            prop_assert!(l.remove(&k));
                        }
                    }
                    Op::Find { cpu, within, after } => {
                        let free = [u64::from(cpu), 0, 0, 0];
                        let within = within.map_or(u64::MAX, u64::from);
                        let after = after.and_then(|a| m.keys().nth(usize::from(a) % m.len().max(1)).copied());
                        let want = m.iter().find(|(k, i)| after.is_none_or(|a| **k > a) && fits(&i.ask, &free) && i.predicted.unwrap_or(u64::MAX) <= within).map(|(k, i)| (*k, i.slot));
                        let got = l.find(&Query { after: after.as_ref(), free: &free, within }, &mut |_| true);
                        prop_assert_eq!(got, want);
                    }
                }
                prop_assert_eq!(l.len(), u32::try_from(m.len()).unwrap_or(0));
                let entries: Vec<(Key, u32)> = m.iter().map(|(k, i)| (*k, i.slot)).collect();
                prop_assert_eq!(l.entries(), entries.clone());
                for (n, (k, _)) in entries.iter().enumerate() {
                    prop_assert_eq!(l.rank(k), u32::try_from(n).unwrap_or(0));
                    prop_assert_eq!(l.select(u32::try_from(n).unwrap_or(0)).map(|(k2, _)| k2), Some(*k));
                    let sums = m.range(..*k).fold(Sums::NONE, |acc, (_, i)| acc.with(&Sums::of(i)));
                    prop_assert_eq!(l.ahead(k), sums);
                }
            }
        }
    }
}
