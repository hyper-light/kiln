//! The fleet's waiting work, grouped by what it needs and the room it asks (docs/reach.md §2.2),
//! kept as it changes (AUDIT O3, P1): each run or leg that starts to wait, waits for another
//! reason, or stops waiting changes its one group, in O(log n). Nothing is rebuilt or sorted per
//! message; a view reads the groups as they stand.

use std::collections::{BTreeMap, HashMap, HashSet};

use kiln_ci::sched::state::Class;

use super::hint::Amounts;

/// Whose word a waiting item is: the CI engine's (a leg), or a runner's as the owner of its runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Leg,
    Owner(String),
}

/// One waiting run or leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub need: String,
    pub ask: Amounts,
    pub class: Class,
    /// When it began to wait (the server's clock, unix ms).
    pub since: i64,
    pub reason: String,
}

/// A group's key: what it needs, then the room it asks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub need: String,
    pub ask: Amounts,
}

/// What waits of one need and size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Group {
    pub runs: u32,
    /// By class, in [`Class::ALL`]'s order.
    pub by_class: [u32; 5],
    /// How many wait for each reason.
    pub reasons: BTreeMap<String, u32>,
    /// When each began to wait, counted: the first is the longest waiting.
    since: BTreeMap<i64, u32>,
}

impl Group {
    /// When the one waiting longest began to.
    pub fn oldest(&self) -> Option<i64> {
        self.since.first_key_value().map(|(t, _)| *t)
    }
}

fn up<K: Ord + Clone>(m: &mut BTreeMap<K, u32>, k: &K) {
    if let Some(n) = m.get_mut(k) {
        *n = n.saturating_add(1);
    } else {
        m.insert(k.clone(), 1);
    }
}

fn down<K: Ord>(m: &mut BTreeMap<K, u32>, k: &K) {
    if let Some(n) = m.get_mut(k) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            m.remove(k);
        }
    }
}

/// The waiting work.
#[derive(Debug, Default)]
pub struct Demand {
    items: HashMap<(Source, String), Item>,
    /// Each owner's items' ids, so one gone or saying them whole costs its own, not everyone's.
    by_owner: HashMap<String, HashSet<String>>,
    groups: BTreeMap<Key, Group>,
    /// How many groups ask each size: the sizes known (the offers view answers them per runner).
    sizes: BTreeMap<Amounts, u32>,
    /// How many items in all.
    total: u32,
    /// Changes to a group made, in all: what the work costs (the tests hold it to O(changes)).
    pub touched: u64,
}

/// What a change did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Moved {
    /// Some group's counts changed.
    pub counts: bool,
    /// A size was asked that no group asked before.
    pub new_size: bool,
}

impl Moved {
    fn or(self, o: Moved) -> Moved {
        Moved { counts: self.counts || o.counts, new_size: self.new_size || o.new_size }
    }
}

impl Demand {
    fn add(&mut self, it: &Item) -> bool {
        self.touched += 1;
        self.total = self.total.saturating_add(1);
        let key = Key { need: it.need.clone(), ask: it.ask };
        let fresh = !self.groups.contains_key(&key);
        let g = self.groups.entry(key).or_default();
        g.runs = g.runs.saturating_add(1);
        if let Some(c) = g.by_class.get_mut(it.class.index()) {
            *c = c.saturating_add(1);
        }
        up(&mut g.reasons, &it.reason);
        up(&mut g.since, &it.since);
        let mut new_size = false;
        if fresh {
            new_size = !self.sizes.contains_key(&it.ask);
            up(&mut self.sizes, &it.ask);
        }
        new_size
    }

    fn sub(&mut self, it: &Item) {
        self.touched += 1;
        self.total = self.total.saturating_sub(1);
        let key = Key { need: it.need.clone(), ask: it.ask };
        let Some(g) = self.groups.get_mut(&key) else { return };
        g.runs = g.runs.saturating_sub(1);
        if let Some(c) = g.by_class.get_mut(it.class.index()) {
            *c = c.saturating_sub(1);
        }
        down(&mut g.reasons, &it.reason);
        down(&mut g.since, &it.since);
        if g.runs == 0 {
            self.groups.remove(&key);
            down(&mut self.sizes, &it.ask);
        }
    }

    /// `id` of `src` waits as `it` says: new, or what it was replaced. Its `since` is kept from
    /// when it first began to wait. A change of `since` alone changes no count.
    pub fn put(&mut self, src: Source, id: &str, mut it: Item) -> Moved {
        let k = (src, id.to_owned());
        if let Some(was) = self.items.get(&k) {
            it.since = was.since;
            if *was == it {
                return Moved::default();
            }
            let was = was.clone();
            self.sub(&was);
        } else if let Source::Owner(o) = &k.0 {
            self.by_owner.entry(o.clone()).or_default().insert(id.to_owned());
        }
        let new_size = self.add(&it);
        self.items.insert(k, it);
        Moved { counts: true, new_size }
    }

    /// `id` of `src` waits no more: whether it did.
    pub fn remove(&mut self, src: &Source, id: &str) -> bool {
        let k = (src.clone(), id.to_owned());
        let Some(was) = self.items.remove(&k) else { return false };
        if let Source::Owner(o) = src
            && let Some(ids) = self.by_owner.get_mut(o)
        {
            ids.remove(id);
            if ids.is_empty() {
                self.by_owner.remove(o);
            }
        }
        self.sub(&was);
        true
    }

    /// Everything `owner` said waits, gone (it went away, or says them whole again): O(its own).
    pub fn forget_owner(&mut self, owner: &str) -> bool {
        let Some(ids) = self.by_owner.remove(owner) else { return false };
        let src = Source::Owner(owner.to_owned());
        for id in &ids {
            if let Some(was) = self.items.remove(&(src.clone(), id.clone())) {
                self.sub(&was);
            }
        }
        !ids.is_empty()
    }

    /// Owner `owner`'s word: `whole` replaces what it said before; `waits` wait (each with its
    /// id); `gone` wait no more.
    pub fn owner_says(&mut self, owner: &str, whole: bool, waits: impl IntoIterator<Item = (String, Item)>, gone: &[String]) -> Moved {
        let src = Source::Owner(owner.to_owned());
        let mut m = Moved::default();
        if whole {
            // What it says now against what it said: only the difference moves a count.
            let had: HashSet<String> = self.by_owner.get(owner).cloned().unwrap_or_default();
            let mut kept = HashSet::new();
            for (id, it) in waits {
                m = m.or(self.put(src.clone(), &id, it));
                kept.insert(id);
            }
            for id in had.difference(&kept) {
                m.counts |= self.remove(&src, id);
            }
        } else {
            for (id, it) in waits {
                m = m.or(self.put(src.clone(), &id, it));
            }
        }
        for id in gone {
            m.counts |= self.remove(&src, id);
        }
        m
    }

    /// The groups, by need then size.
    pub fn groups(&self) -> &BTreeMap<Key, Group> {
        &self.groups
    }

    /// The sizes asked, each once.
    pub fn sizes(&self) -> impl Iterator<Item = &Amounts> {
        self.sizes.keys()
    }

    /// How many runs and legs wait in all.
    pub fn total(&self) -> u32 {
        self.total
    }

    /// Whether `id` of `src` waits.
    pub fn has(&self, src: &Source, id: &str) -> bool {
        self.items.contains_key(&(src.clone(), id.to_owned()))
    }

    /// Its books against its items, O(n): every group counts exactly its items, and every size
    /// counts its groups. For the tests.
    #[cfg(test)]
    pub fn check(&self) -> Result<(), String> {
        let mut groups: BTreeMap<Key, Group> = BTreeMap::new();
        for it in self.items.values() {
            let g = groups.entry(Key { need: it.need.clone(), ask: it.ask }).or_default();
            g.runs += 1;
            if let Some(c) = g.by_class.get_mut(it.class.index()) {
                *c += 1;
            }
            up(&mut g.reasons, &it.reason);
            up(&mut g.since, &it.since);
        }
        if groups != self.groups {
            return Err(format!("groups {:?} against items' {:?}", self.groups, groups));
        }
        let mut sizes = BTreeMap::new();
        for k in groups.keys() {
            up(&mut sizes, &k.ask);
        }
        if sizes != self.sizes {
            return Err(format!("sizes {:?} against groups' {:?}", self.sizes, sizes));
        }
        if usize::try_from(self.total).unwrap_or(usize::MAX) != self.items.len() {
            return Err(format!("total {} against {} items", self.total, self.items.len()));
        }
        Ok(())
    }
}
