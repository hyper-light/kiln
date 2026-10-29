//! Equivalence classes (docs/ci.md W24-R30): keys known to stand for the same outputs, kept as an
//! e-graph keeps e-classes (egg, Willsey et al., POPL 2021, arXiv 2004.03082: "an e-graph
//! efficiently represents a congruence relation over many expressions"), the structure Dagger's
//! cache is built on. Pure: what units produced, the rules, and what verification saw are passed in.
//!
//! - **Classes.** [`EGraph`] is a union-find over keys ([`Key`]). Keys whose units produced
//!   byte-identical outputs are one class, whose outputs' digest is its e-class analysis and its id
//!   ([`ClassId::Output`]). A class none of whose keys' units has produced anything yet is known by
//!   its least key ([`ClassId::Pending`]), so a class's id depends on its keys alone. Classes with
//!   different outputs never merge: that is a mismatch, for verification to settle
//!   ([`Refused::Conflict`]).
//! - **Congruence.** A unit names an upstream unit whose outputs it doesn't have yet by its key
//!   ([`refer`]). [`canonical`] takes each such unit by class and puts the definition in the rules'
//!   normal form, so congruent units (the same operation over equivalent inputs) have one canonical
//!   key and share a result; over a class with outputs, a unit keys as it would over those outputs
//!   (W24-R03). [`EGraph::rebuild`] closes the classes under congruence, as egg's rebuild does.
//! - **Rewrite rules** are data ([`Rule`]): an operation that does nothing dropped, two that
//!   commute in either order. [`Rules`] takes a rule only with a differential test's witness that
//!   it keeps outputs ([`Witness`]), and only the rules it holds rewrite anything.
//! - **Trust.** An e-graph belongs to one trust domain, and its classes take only entries that
//!   domain may read ([`reads`], W24-R21): untrusted evidence never enters a class trusted lookups
//!   use.
//! - **Splits.** Verification that sees a key's unit produce other outputs than its class's refutes
//!   the claims that put it there ([`separate`]): every key they reached keeps only what its own
//!   outputs show and is quarantined, as is everything that took one of them by class (its
//!   lineage), so none of them serves a class-based hit again.
//! - **Where classes are kept.** [`EGraph`] keeps them in memory; the work ledger keeps them as rows
//!   (W24-R02's `work.eclass`). Both are [`Classes`], and [`split`] runs on either alike, so what a
//!   mismatch quarantines is decided here whichever keeps them; [`producing`] decides what a unit's
//!   outputs make of its key for both.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::key::{self, Key, Kind, Unit};

/// Whether lookups in `reader`'s trust domain may use what `writer`'s wrote (W24-R21): a domain's
/// own entries, and for untrusted and person scopes trusted ones too. Trusted reads only trusted.
/// Protected reads only protected: the trusted results a protected unit may take are those two
/// runners produced alike (Decision 4), which a class doesn't show.
pub fn reads(reader: &str, writer: &str) -> bool {
    reader == writer || (writer == "trusted" && (reader == "untrusted" || reader.starts_with("person:")))
}

/// What an upstream value that names a unit by its key starts with.
const REFER: &str = "key:";

/// An upstream unit named by its key, its outputs not materialized yet: [`Unit::upstream`]'s value
/// for it, which [`canonical`] takes by class.
pub fn refer(k: &Key) -> String {
    format!("{REFER}{}", k.hex())
}

/// The key an upstream value names, when it names one ([`refer`]).
pub fn reference(value: &str) -> Option<Key> {
    parse_key(value.strip_prefix(REFER)?)
}

/// A key from its hex digits, lowercase, as [`Key::hex`] writes them.
pub fn parse_key(hex: &str) -> Option<Key> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(Key(out))
}

/// Whether `d` can be an output digest: not empty, without the `|` the ledger's rows divide keys
/// with, and not a key named ([`refer`]).
fn digest_ok(d: &str) -> bool {
    !d.is_empty() && !d.contains('|') && !d.starts_with(REFER)
}

/// What a unit produced, as one digest (W24-R30): its outcome, exit code, outputs and test reports,
/// canonically encoded. Its log, which carries times, isn't in it. Units with one digest produced
/// byte-identical outputs.
pub fn output_digest(outcome: &str, exit: Option<i32>, outputs: &BTreeMap<String, String>, reports: &[String]) -> String {
    fn put(h: &mut Sha256, s: &str) {
        h.update((s.len() as u64).to_be_bytes());
        h.update(s.as_bytes());
    }
    let mut h = Sha256::new();
    put(&mut h, key::VERSION);
    put(&mut h, "output");
    put(&mut h, outcome);
    match exit {
        None => h.update([0]),
        Some(e) => {
            h.update([1]);
            h.update(e.to_be_bytes());
        }
    }
    h.update((outputs.len() as u64).to_be_bytes());
    for (name, value) in outputs {
        put(&mut h, name);
        put(&mut h, value);
    }
    h.update((reports.len() as u64).to_be_bytes());
    for r in reports {
        put(&mut h, r);
    }
    format!("sha256:{}", Key(h.finalize().into()).hex())
}

/// A class's id: its canonical representative.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassId {
    /// A class whose keys' units produced these outputs: its e-class analysis.
    Output(String),
    /// A class none of whose keys' units has produced anything yet, by its least key.
    Pending(#[serde(with = "hex_key")] Key),
}

impl ClassId {
    /// Its outputs' digest, when it has outputs.
    pub fn output(&self) -> Option<&str> {
        match self {
            ClassId::Output(d) => Some(d),
            ClassId::Pending(_) => None,
        }
    }

    /// How a unit names it as an upstream unit's class in canonical form: by its outputs, as an
    /// upstream unit with those outputs is named (W24-R03), else by its least key.
    pub fn rep(&self) -> String {
        match self {
            ClassId::Output(d) => d.clone(),
            ClassId::Pending(k) => refer(k),
        }
    }
}

mod hex_key {
    use super::{Key, parse_key};
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(k: &Key, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&k.hex())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Key, D::Error> {
        let s = String::deserialize(d)?;
        parse_key(&s).ok_or_else(|| serde::de::Error::custom(format!("{s:?} isn't a key's 64 hex digits")))
    }
}

/// A key in an e-graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub class: ClassId,
    /// The trust domain that wrote its entry (W24-R21).
    pub writer: String,
    /// What its unit was seen to produce: the evidence it brings its class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced: Option<String>,
    /// A split reached it: it serves no class-based hit, and joins no class by a claim.
    #[serde(default)]
    pub quarantined: bool,
    /// Verification saw its unit produce different outputs from one run to the next (W24-R23):
    /// it's in no class with others.
    #[serde(default)]
    pub nondeterministic: bool,
}

impl Member {
    /// Whether its own outputs put it in its class.
    pub fn anchored(&self) -> bool {
        self.produced.is_some() && self.produced.as_deref() == self.class.output()
    }

    /// Whether a claim may join it to others: it isn't quarantined or nondeterministic.
    pub fn claimable(&self) -> bool {
        !self.quarantined && !self.nondeterministic
    }

    /// How a unit that names it as an upstream unit takes it by class ([`ClassId::rep`]): not at
    /// all once a split has quarantined it.
    pub fn rep(&self) -> Option<String> {
        (!self.quarantined).then(|| self.class.rep())
    }

    /// Whether a lookup may take a result through it by class (W24-R30): it takes claims, and its
    /// class has outputs.
    pub fn serves(&self) -> bool {
        self.claimable() && self.class.output().is_some()
    }
}

/// What `key`'s member is once its unit is seen producing `produced` (W24-R30): a key with none
/// yet becomes `writer`'s, in the class of those outputs; one with a member keeps it, now with those
/// outputs, in their class. Refused when its unit produced other outputs before, or its class holds
/// others: verification settles which ([`split`]).
pub fn producing(key: &Key, m: Option<&Member>, writer: &str, produced: &str) -> Result<Member, Refused> {
    if !digest_ok(produced) {
        return Err(Refused::Digest(produced.to_owned()));
    }
    let class = ClassId::Output(produced.to_owned());
    let Some(m) = m else {
        return Ok(Member { class, writer: writer.to_owned(), produced: Some(produced.to_owned()), quarantined: false, nondeterministic: false });
    };
    if m.nondeterministic {
        return Err(Refused::Nondeterministic(*key));
    }
    if let Some(held) = m.produced.as_deref().or(m.class.output()).filter(|held| *held != produced) {
        return Err(Refused::Conflict { held: held.to_owned(), seen: produced.to_owned() });
    }
    Ok(Member { class, produced: Some(produced.to_owned()), ..m.clone() })
}

/// Why two keys are one class with no outputs of both to show it: a claim.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Why {
    /// One canonical unit (congruence): the rules named rewrote the definitions alike, and the
    /// upstream units named were taken by class.
    Canonical { rules: Vec<String>, by_class: Vec<String> },
    /// The caller's own claim, as it puts it.
    Claim(String),
}

impl Why {
    /// Both reasons: each one's rules and upstream units, once each.
    fn with(&self, other: &Why) -> Why {
        let union = |a: &[String], b: &[String]| a.iter().chain(b).cloned().collect::<BTreeSet<_>>().into_iter().collect();
        match (self, other) {
            (Why::Canonical { rules: r1, by_class: b1 }, Why::Canonical { rules: r2, by_class: b2 }) => Why::Canonical { rules: union(r1, r2), by_class: union(b1, b2) },
            (Why::Claim(_), _) => self.clone(),
            (_, Why::Claim(_)) => other.clone(),
        }
    }
}

/// Why an e-graph refused a step; a refused step changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("{reader} lookups may not use what {writer} wrote (W24-R21)")]
    Domain { reader: String, writer: String },
    #[error("outputs differ: {held} is held, {seen} was seen; verification settles which (W24-R23)")]
    Conflict { held: String, seen: String },
    #[error("{0} is quarantined (W24-R30): it joins no class by a claim")]
    Quarantined(Key),
    #[error("{0} produced different outputs from one run to the next (W24-R23): it joins no class")]
    Nondeterministic(Key),
    #[error("{0} isn't in the e-graph")]
    Unknown(Key),
    #[error("{0:?} isn't an output digest: one is not empty, has no `|` and doesn't start `key:`")]
    Digest(String),
    #[error("rule {name:?}: {why}")]
    Rule { name: String, why: String },
}

/// The class two classes make, or why they can't be one: outputs win over pending, two pending
/// classes take the lesser id (a class's least key), and two classes with outputs are one only if
/// their outputs are the same.
pub fn joined(a: &ClassId, b: &ClassId) -> Result<ClassId, Refused> {
    match (a, b) {
        (ClassId::Output(x), ClassId::Output(y)) if x != y => Err(Refused::Conflict { held: x.clone(), seen: y.clone() }),
        (ClassId::Output(_), _) => Ok(a.clone()),
        (_, ClassId::Output(_)) => Ok(b.clone()),
        (ClassId::Pending(x), ClassId::Pending(y)) => Ok(ClassId::Pending(*x.min(y))),
    }
}

/// What a split decides (W24-R30, R23).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Split {
    /// Every key the claims reached, as it stands after: quarantined; still in its class if its own
    /// outputs put it there, else on its own; the key verification ran, with what it produced (or,
    /// produced differently before, nondeterministic and on its own).
    pub members: BTreeMap<Key, Member>,
    /// The claims dropped: every one among those keys.
    pub dropped: BTreeSet<(Key, Key)>,
}

/// Splits `at`'s class where verification saw its unit produce `observed` instead of the class's
/// outputs, or (`None`) where its lineage reaches it. `tree` holds the keys the class's claims join
/// to `at`, `at` among them, and `claims` the claims among them, each pair once.
pub fn separate(tree: &BTreeMap<Key, Member>, claims: &BTreeSet<(Key, Key)>, at: &Key, observed: Option<&str>) -> Split {
    let mut members = BTreeMap::new();
    for (k, m) in tree {
        let mut now = m.clone();
        now.quarantined = true;
        match observed.filter(|_| k == at) {
            Some(o) if m.produced.as_deref().is_some_and(|p| p != o) => {
                now.nondeterministic = true;
                now.produced = None;
                now.class = ClassId::Pending(*k);
            }
            Some(o) => {
                now.produced = Some(o.to_owned());
                now.class = ClassId::Output(o.to_owned());
            }
            None if m.anchored() => {}
            None => now.class = ClassId::Pending(*k),
        }
        members.insert(*k, now);
    }
    Split { members, dropped: claims.clone() }
}

/// Where an e-graph's classes are kept: [`EGraph`] in memory, or the work ledger's rows (W24-R02's
/// `work.eclass`). [`tree`] and [`split`] run on either alike.
pub trait Classes {
    type Error: From<Refused>;
    /// A key's member, if it has one.
    fn entry(&self, k: &Key) -> Result<Option<Member>, Self::Error>;
    /// The keys claimed equal to `k`.
    fn claimed(&self, k: &Key) -> Result<BTreeSet<Key>, Self::Error>;
    /// The keys of the units that took `k` by class: its lineage.
    fn lineage(&self, k: &Key) -> Result<BTreeSet<Key>, Self::Error>;
    /// Makes a split so: its members as it has them, its claims dropped.
    fn apply(&mut self, split: &Split) -> Result<(), Self::Error>;
}

/// The keys the claims join to `at` within its class, `at` among them, and the claims among them,
/// each pair once.
pub fn tree<G: Classes + ?Sized>(g: &G, at: &Key) -> Result<(BTreeMap<Key, Member>, BTreeSet<(Key, Key)>), G::Error> {
    let (mut tree, mut edges) = (BTreeMap::new(), BTreeSet::new());
    let mut queue = VecDeque::from([*at]);
    while let Some(k) = queue.pop_front() {
        if tree.contains_key(&k) {
            continue;
        }
        let Some(m) = g.entry(&k)? else { continue };
        tree.insert(k, m);
        for other in g.claimed(&k)? {
            edges.insert(if k < other { (k, other) } else { (other, k) });
            queue.push_back(other);
        }
    }
    Ok((tree, edges))
}

/// Splits `at`'s class where verification saw its unit produce `observed` instead of the class's
/// outputs, or (`None`) where a lineage reaches it ([`separate`]), then every class whose units
/// took one of the keys it quarantined by class, in turn. Returns every key quarantined; nothing
/// when `observed` is the class's own outputs.
pub fn split<G: Classes + ?Sized>(g: &mut G, at: &Key, observed: Option<&str>) -> Result<BTreeSet<Key>, G::Error> {
    let m = g.entry(at)?.ok_or(Refused::Unknown(*at))?;
    if let Some(o) = observed {
        if !digest_ok(o) {
            return Err(Refused::Digest(o.to_owned()).into());
        }
        if m.class.output() == Some(o) {
            return Ok(BTreeSet::new());
        }
    }
    let mut quarantined = BTreeSet::new();
    let mut queue = VecDeque::from([(*at, observed.map(str::to_owned))]);
    while let Some((k, seen)) = queue.pop_front() {
        if seen.is_none() && quarantined.contains(&k) {
            continue;
        }
        let (reached, claims) = tree(g, &k)?;
        let parts = separate(&reached, &claims, &k, seen.as_deref());
        g.apply(&parts)?;
        for m in parts.members.keys() {
            quarantined.insert(*m);
            for p in g.lineage(m)? {
                if !quarantined.contains(&p) {
                    queue.push_back((p, None));
                }
            }
        }
    }
    Ok(quarantined)
}

/// An e-graph of one trust domain: a union-find over keys with each class's analysis, the claims
/// that joined keys without outputs, and units as terms for congruence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EGraph {
    domain: String,
    members: BTreeMap<Key, Member>,
    classes: BTreeMap<ClassId, BTreeSet<Key>>,
    /// Claims, both ways: each key's to the keys it was claimed equal to.
    claims: BTreeMap<Key, BTreeMap<Key, Why>>,
    /// The units added as terms, by their own keys.
    terms: BTreeMap<Key, Unit>,
    /// The terms that name each key as an upstream unit.
    parents: BTreeMap<Key, BTreeSet<Key>>,
}

impl EGraph {
    /// An empty e-graph for `domain`'s lookups.
    pub fn new(domain: &str) -> EGraph {
        EGraph { domain: domain.to_owned(), ..EGraph::default() }
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn member(&self, k: &Key) -> Option<&Member> {
        self.members.get(k)
    }

    /// A key's class: its union-find root.
    pub fn find(&self, k: &Key) -> Option<&ClassId> {
        self.members.get(k).map(|m| &m.class)
    }

    pub fn same(&self, a: &Key, b: &Key) -> bool {
        matches!((self.find(a), self.find(b)), (Some(x), Some(y)) if x == y)
    }

    /// Every class with its keys.
    pub fn classes(&self) -> &BTreeMap<ClassId, BTreeSet<Key>> {
        &self.classes
    }

    /// How a unit that names `k` as an upstream unit takes it by class ([`Member::rep`]): not at
    /// all when it's quarantined or not here.
    pub fn rep(&self, k: &Key) -> Option<String> {
        self.members.get(k).and_then(Member::rep)
    }

    fn admit(&self, writer: &str) -> Result<(), Refused> {
        if reads(&self.domain, writer) { Ok(()) } else { Err(Refused::Domain { reader: self.domain.clone(), writer: writer.to_owned() }) }
    }

    /// A key whose entry `writer`'s domain wrote, with no outputs yet: a class of its own, unless
    /// it's here already.
    pub fn add(&mut self, key: Key, writer: &str) -> Result<ClassId, Refused> {
        self.admit(writer)?;
        if let Some(m) = self.members.get(&key) {
            return Ok(m.class.clone());
        }
        let class = ClassId::Pending(key);
        self.members.insert(key, Member { class: class.clone(), writer: writer.to_owned(), produced: None, quarantined: false, nondeterministic: false });
        self.classes.entry(class.clone()).or_default().insert(key);
        Ok(class)
    }

    /// What `key`'s unit produced: it, and what's claimed equal to it, join the class of those
    /// outputs, so keys whose units produced byte-identical outputs are one class. Refused when its
    /// class holds other outputs, or it produced others before: verification settles which.
    pub fn record(&mut self, key: Key, writer: &str, produced: &str) -> Result<ClassId, Refused> {
        if !digest_ok(produced) {
            return Err(Refused::Digest(produced.to_owned()));
        }
        self.admit(writer)?;
        let now = producing(&key, self.members.get(&key), writer, produced)?;
        let (from, to) = (self.add(key, writer)?, now.class.clone());
        self.members.insert(key, now);
        self.relabel(&from, &to);
        Ok(to)
    }

    /// A claim that two keys here are equivalent: their classes merge, unless one of them is
    /// quarantined or nondeterministic, or their outputs differ.
    pub fn merge(&mut self, a: &Key, b: &Key, why: Why) -> Result<ClassId, Refused> {
        let (ca, cb) = (self.claimable(a)?.class.clone(), self.claimable(b)?.class.clone());
        if ca == cb {
            return Ok(ca);
        }
        let to = joined(&ca, &cb)?;
        self.claims.entry(*a).or_default().insert(*b, why.clone());
        self.claims.entry(*b).or_default().insert(*a, why);
        self.relabel(&ca, &to);
        self.relabel(&cb, &to);
        Ok(to)
    }

    fn claimable(&self, k: &Key) -> Result<&Member, Refused> {
        let m = self.members.get(k).ok_or(Refused::Unknown(*k))?;
        if m.quarantined {
            Err(Refused::Quarantined(*k))
        } else if m.nondeterministic {
            Err(Refused::Nondeterministic(*k))
        } else {
            Ok(m)
        }
    }

    /// Moves every key of class `from` into `to`.
    fn relabel(&mut self, from: &ClassId, to: &ClassId) {
        if from == to {
            return;
        }
        let moved = self.classes.remove(from).unwrap_or_default();
        for k in &moved {
            if let Some(m) = self.members.get_mut(k) {
                m.class = to.clone();
            }
        }
        self.classes.entry(to.clone()).or_default().extend(moved);
    }

    /// A unit as a term (egg's e-node): its own key here, and its upstream units, named by key,
    /// its children. Returns its key.
    pub fn add_term(&mut self, writer: &str, unit: &Unit) -> Result<Key, Refused> {
        let own = key::weak(unit);
        self.add(own, writer)?;
        for value in unit.upstream.values() {
            if let Some(up) = reference(value) {
                self.parents.entry(up).or_default().insert(own);
            }
        }
        self.terms.insert(own, unit.clone());
        Ok(own)
    }

    /// Congruence closure, as egg's rebuild: each term keyed in canonical form over its upstream
    /// units' classes, and terms with one canonical key merged, until nothing more merges.
    /// Quarantined and nondeterministic terms take no part. Returns the congruences refused
    /// because their classes' outputs differ, for verification to settle.
    pub fn rebuild(&mut self, rules: &Rules) -> Vec<Refused> {
        let mut problems = Vec::new();
        loop {
            let mut first: BTreeMap<Key, (Key, Why)> = BTreeMap::new();
            let mut pairs = Vec::new();
            for (own, unit) in &self.terms {
                if self.members.get(own).is_none_or(|m| m.quarantined || m.nondeterministic) {
                    continue;
                }
                let c = canonical(unit, rules, &|k| self.rep(k));
                match first.get(&c.key) {
                    Some((other, why)) if !self.same(other, own) => pairs.push((*other, *own, why.with(&c.why))),
                    Some(_) => {}
                    None => {
                        first.insert(c.key, (*own, c.why));
                    }
                }
            }
            let mut merged = false;
            for (a, b, why) in pairs {
                match self.merge(&a, &b, why) {
                    Ok(_) => merged = true,
                    Err(e @ Refused::Conflict { .. }) if !problems.contains(&e) => problems.push(e),
                    Err(_) => {}
                }
            }
            if !merged {
                return problems;
            }
        }
    }

    /// The keys the claims join to `at` within its class, `at` among them, and the claims among
    /// them, each pair once ([`tree`]).
    pub fn tree(&self, at: &Key) -> (BTreeMap<Key, Member>, BTreeSet<(Key, Key)>) {
        tree(self, at).unwrap_or_default()
    }

    /// Splits `at`'s class where verification saw its unit produce `observed`, or (`None`) where a
    /// lineage reaches it ([`split`]): its lineage is the terms that name a key by key.
    pub fn split(&mut self, at: &Key, observed: Option<&str>) -> Result<BTreeSet<Key>, Refused> {
        split(self, at, observed)
    }
}

impl Classes for EGraph {
    type Error = Refused;

    fn entry(&self, k: &Key) -> Result<Option<Member>, Refused> {
        Ok(self.members.get(k).cloned())
    }

    fn claimed(&self, k: &Key) -> Result<BTreeSet<Key>, Refused> {
        Ok(self.claims.get(k).map(|c| c.keys().copied().collect()).unwrap_or_default())
    }

    fn lineage(&self, k: &Key) -> Result<BTreeSet<Key>, Refused> {
        Ok(self.parents.get(k).cloned().unwrap_or_default())
    }

    fn apply(&mut self, split: &Split) -> Result<(), Refused> {
        for (a, b) in &split.dropped {
            for (x, y) in [(a, b), (b, a)] {
                let empty = match self.claims.get_mut(x) {
                    Some(c) => {
                        c.remove(y);
                        c.is_empty()
                    }
                    None => false,
                };
                if empty {
                    self.claims.remove(x);
                }
            }
        }
        for (k, m) in &split.members {
            if let Some(old) = self.members.get(k).map(|o| o.class.clone()).filter(|old| *old != m.class) {
                let empty = match self.classes.get_mut(&old) {
                    Some(set) => {
                        set.remove(k);
                        set.is_empty()
                    }
                    None => false,
                };
                if empty {
                    self.classes.remove(&old);
                }
                self.classes.entry(m.class.clone()).or_default().insert(*k);
            }
            self.members.insert(*k, m.clone());
        }
        Ok(())
    }
}

/// A unit in canonical form (W24-R30), and its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canonical {
    pub unit: Unit,
    pub key: Key,
    /// What made it: the rules that rewrote its definition and the upstream units taken by class.
    pub why: Why,
}

/// A unit's canonical form: each upstream unit it names by key taken by class (`rep`, the class's
/// [`ClassId::rep`]; one with no class stays as it's named), and its definition in the normal form
/// of the rules that apply to its kind. A unit whose upstream units all have outputs, and that the
/// rules don't rewrite, is its own canonical form. Recording and lookup both key units here, so a
/// key is computed and looked up on one path, as in Dagger's cache.
pub fn canonical(u: &Unit, rules: &Rules, rep: &dyn Fn(&Key) -> Option<String>) -> Canonical {
    let mut unit = u.clone();
    let mut by_class = Vec::new();
    for (name, value) in &mut unit.upstream {
        if let Some(r) = reference(value).and_then(|k| rep(&k)).filter(|r| r != value) {
            *value = r;
            by_class.push(name.clone());
        }
    }
    let (definition, applied) = rules.normalize(u.kind, &u.definition);
    unit.definition = definition;
    Canonical { key: key::weak(&unit), unit, why: Why::Canonical { rules: applied, by_class } }
}

/// Which operations a rule's rewrite names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pattern {
    /// An operation that is exactly this, spaces around it aside.
    Is(String),
    /// An operation that starts with this, spaces before it aside.
    Starts(String),
}

impl Pattern {
    pub fn matches(&self, op: &str) -> bool {
        let op = op.trim();
        match self {
            Pattern::Is(s) => op == s.trim(),
            Pattern::Starts(p) => op.starts_with(p.trim_start()),
        }
    }

    /// Whether it names every operation there is.
    fn everything(&self) -> bool {
        matches!(self, Pattern::Starts(p) if p.trim().is_empty())
    }
}

/// What a rule rewrites. A definition's operations are its lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rewrite {
    /// Each operation `op` names does nothing: it's dropped.
    Drop { op: Pattern },
    /// An operation `a` names and one `b` names, different ones, commute: either order is one.
    Commute { a: Pattern, b: Pattern },
    /// Operations that start with `prefix` and assign different names (what comes before their
    /// first `=`) commute: environment variables set in either order are one.
    Assign { prefix: String },
    /// In an operation `op` names, the words among `flags` (flags that take no value) may come in
    /// any order: one order stands for them all.
    Flags { op: Pattern, flags: BTreeSet<String> },
}

/// A rewrite rule over unit definitions (W24-R30), as data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    /// The kinds of unit it rewrites; every kind when empty.
    #[serde(default)]
    pub kinds: BTreeSet<Kind>,
    pub rewrite: Rewrite,
}

impl Rule {
    fn applies(&self, kind: Kind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }

    fn drops(&self, op: &str) -> bool {
        matches!(&self.rewrite, Rewrite::Drop { op: p } if p.matches(op))
    }

    fn commutes(&self, x: &str, y: &str) -> bool {
        match &self.rewrite {
            Rewrite::Commute { a, b } => x != y && ((a.matches(x) && b.matches(y)) || (a.matches(y) && b.matches(x))),
            Rewrite::Assign { prefix } => {
                let name = |op: &str| op.trim().strip_prefix(prefix.trim_start()).and_then(|r| r.split_once('=')).map(|(n, _)| n.trim().to_owned());
                matches!((name(x), name(y)), (Some(m), Some(n)) if !m.is_empty() && !n.is_empty() && m != n)
            }
            Rewrite::Drop { .. } | Rewrite::Flags { .. } => false,
        }
    }

    /// `op` with the flags this rule puts in order sorted in their places, if it names `op`.
    fn ordered(&self, op: &str) -> Option<String> {
        let Rewrite::Flags { op: p, flags } = &self.rewrite else { return None };
        if !p.matches(op) {
            return None;
        }
        let words: Vec<&str> = op.split(' ').collect();
        let mut listed: Vec<&str> = words.iter().copied().filter(|w| flags.contains(*w)).collect();
        listed.sort_unstable();
        let mut next = listed.into_iter();
        let out: Vec<&str> = words.iter().map(|w| if flags.contains(*w) { next.next().unwrap_or(w) } else { w }).collect();
        Some(out.join(" "))
    }
}

/// A differential test's evidence that a rule keeps outputs: definitions the rule makes one, each
/// run both ways, with what each run produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    /// The differential test that ran them.
    pub test: String,
    pub cases: Vec<Case>,
}

/// Two definitions a rule makes one, and the output digests running each gave.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    pub before: String,
    pub after: String,
    pub before_output: String,
    pub after_output: String,
}

/// The most operations a definition is put in commuting order for. Ordering is quadratic in them;
/// past this, only drops apply.
pub const ORDER_MAX: usize = 1024;

/// A definition's normal form under `rules`, and the names of those that changed it: every
/// operation a drop names gone, then the rest in Foata normal form under the commutations, which
/// is one for every order they allow (each operation at one more than the deepest earlier one it
/// doesn't commute with, and each level in text order).
fn normal(rules: &[&Rule], definition: &str) -> (String, BTreeSet<String>) {
    let mut used = BTreeSet::new();
    let mut owned: Vec<String> = Vec::new();
    for op in definition.split('\n') {
        match rules.iter().find(|r| r.drops(op)) {
            Some(r) => {
                used.insert(r.name.clone());
            }
            None => {
                let mut op = op.to_owned();
                for r in rules {
                    if let Some(o) = r.ordered(&op).filter(|o| *o != op) {
                        used.insert(r.name.clone());
                        op = o;
                    }
                }
                owned.push(op);
            }
        }
    }
    let mut ops: Vec<&str> = owned.iter().map(String::as_str).collect();
    let commuting: Vec<&&Rule> = rules.iter().filter(|r| matches!(r.rewrite, Rewrite::Commute { .. } | Rewrite::Assign { .. })).collect();
    if !commuting.is_empty() && ops.len() <= ORDER_MAX {
        let by = |x: &str, y: &str| commuting.iter().find(|r| r.commutes(x, y)).map(|r| r.name.clone());
        let mut levels: Vec<usize> = Vec::with_capacity(ops.len());
        for op in &ops {
            let level = ops.iter().zip(&levels).filter(|(earlier, _)| by(earlier, op).is_none()).map(|(_, l)| l + 1).max().unwrap_or(0);
            levels.push(level);
        }
        let mut order: Vec<(usize, &str, usize)> = levels.iter().zip(&ops).enumerate().map(|(i, (l, op))| (*l, *op, i)).collect();
        order.sort_unstable();
        // The rules that put two operations in another order, each pair out of order once.
        let place: BTreeMap<usize, usize> = order.iter().enumerate().map(|(at, (_, _, i))| (*i, at)).collect();
        for (i, x) in ops.iter().enumerate() {
            for (j, y) in ops.iter().enumerate().skip(i + 1) {
                if place.get(&i) > place.get(&j) {
                    used.extend(by(x, y));
                }
            }
        }
        ops = order.into_iter().map(|(_, op, _)| op).collect();
    }
    (ops.join("\n"), used)
}

/// The rewrite rules registered (W24-R30), each with its witness. Only these rewrite anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    held: Vec<(Rule, Witness)>,
}

impl Rules {
    /// Registers a rule with a differential test's witness: refused without a name, under a name
    /// held already, naming every operation, or with a witness that names no test, has no case, or
    /// has a case the rule doesn't relate, that rewrites nothing, or whose runs' outputs differ.
    pub fn register(&mut self, rule: Rule, witness: Witness) -> Result<(), Refused> {
        let fail = |why: String| Err(Refused::Rule { name: rule.name.clone(), why });
        if rule.name.trim().is_empty() {
            return fail("it has no name".into());
        }
        if self.held.iter().any(|(r, _)| r.name == rule.name) {
            return fail("a rule of that name is registered".into());
        }
        let everything = match &rule.rewrite {
            Rewrite::Drop { op } => op.everything(),
            Rewrite::Commute { a, b } => a.everything() || b.everything(),
            Rewrite::Assign { prefix } => prefix.trim().is_empty(),
            Rewrite::Flags { op, flags } => op.everything() || flags.is_empty(),
        };
        if everything {
            return fail("it names every operation".into());
        }
        if witness.test.trim().is_empty() {
            return fail("its witness names no differential test".into());
        }
        if witness.cases.is_empty() {
            return fail("its witness has no case".into());
        }
        for (i, case) in witness.cases.iter().enumerate() {
            if case.before == case.after {
                return fail(format!("case {i} rewrites nothing"));
            }
            if normal(&[&rule], &case.before).0 != normal(&[&rule], &case.after).0 {
                return fail(format!("case {i}: the rule doesn't make its two definitions one"));
            }
            if !digest_ok(&case.before_output) || case.before_output != case.after_output {
                return fail(format!("case {i}: the differential test saw different outputs"));
            }
        }
        self.held.push((rule, witness));
        Ok(())
    }

    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.held.iter().map(|(r, _)| r)
    }

    /// The witness a rule was registered with.
    pub fn witness(&self, name: &str) -> Option<&Witness> {
        self.held.iter().find(|(r, _)| r.name == name).map(|(_, w)| w)
    }

    /// A definition's normal form under the rules that apply to `kind`, and the names of those
    /// that changed it.
    pub fn normalize(&self, kind: Kind, definition: &str) -> (String, Vec<String>) {
        let applying: Vec<&Rule> = self.rules().filter(|r| r.applies(kind)).collect();
        if applying.is_empty() {
            return (definition.to_owned(), Vec::new());
        }
        let (out, used) = normal(&applying, definition);
        (out, used.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::key::Platform;
    use proptest::prelude::*;
    use proptest::sample::Index;

    fn k(i: u8) -> Key {
        Key([i; 32])
    }

    fn out(i: u8) -> String {
        format!("sha256:out{i}")
    }

    fn unit(definition: &str, upstream: &[(&str, String)]) -> Unit {
        Unit {
            kind: Kind::Step,
            definition: definition.to_owned(),
            secrets: BTreeSet::new(),
            env: BTreeMap::new(),
            platform: Platform::default(),
            inputs: BTreeMap::new(),
            upstream: upstream.iter().map(|(n, v)| ((*n).to_owned(), v.clone())).collect(),
            trust: "trusted".into(),
        }
    }

    fn witness(before: &str, after: &str) -> Witness {
        Witness { test: "differential".into(), cases: vec![Case { before: before.into(), after: after.into(), before_output: out(9), after_output: out(9) }] }
    }

    fn drop_rule(name: &str, op: Pattern) -> Rule {
        Rule { name: name.into(), kinds: BTreeSet::new(), rewrite: Rewrite::Drop { op } }
    }

    fn commute_rule(name: &str, a: Pattern, b: Pattern) -> Rule {
        Rule { name: name.into(), kinds: BTreeSet::new(), rewrite: Rewrite::Commute { a, b } }
    }

    /// Sound rules for the scripts here: `true` does nothing, and adding features commutes.
    fn sound() -> Result<Rules, Refused> {
        let mut rules = Rules::default();
        rules.register(drop_rule("drop-true", Pattern::Is("true".into())), witness("make\ntrue", "make"))?;
        rules.register(commute_rule("adds-commute", Pattern::Starts("add ".into()), Pattern::Starts("add ".into())), witness("add a\nadd b", "add b\nadd a"))?;
        Ok(rules)
    }

    /// A partition by brute force, for reference.
    struct Naive<T: Ord + Clone>(Vec<BTreeSet<T>>);

    impl<T: Ord + Clone> Naive<T> {
        fn new() -> Self {
            Naive(Vec::new())
        }

        fn add(&mut self, x: T) {
            if !self.0.iter().any(|s| s.contains(&x)) {
                self.0.push(BTreeSet::from([x]));
            }
        }

        fn same(&self, a: &T, b: &T) -> bool {
            self.0.iter().any(|s| s.contains(a) && s.contains(b))
        }

        /// Whether `a`'s and `b`'s sets were two, now one.
        fn union(&mut self, a: &T, b: &T) -> bool {
            let (Some(i), Some(j)) = (self.0.iter().position(|s| s.contains(a)), self.0.iter().position(|s| s.contains(b))) else { return false };
            if i == j {
                return false;
            }
            let moved = self.0.get_mut(j).map(std::mem::take).unwrap_or_default();
            if let Some(into) = self.0.get_mut(i) {
                into.extend(moved);
            }
            self.0.retain(|s| !s.is_empty());
            true
        }

        fn partition(&self) -> BTreeSet<BTreeSet<T>> {
            self.0.iter().cloned().collect()
        }
    }

    fn partition(g: &EGraph) -> BTreeSet<BTreeSet<Key>> {
        g.classes().values().cloned().collect()
    }

    #[derive(Debug, Clone)]
    enum Step {
        Add(u8),
        Record(u8, u8),
        Claim(u8, u8),
    }

    fn steps() -> impl Strategy<Value = Vec<Step>> {
        proptest::collection::vec(prop_oneof![(0..10u8).prop_map(Step::Add), (0..10u8, 0..3u8).prop_map(|(k, o)| Step::Record(k, o)), (0..10u8, 0..10u8).prop_map(|(a, b)| Step::Claim(a, b))], 0..40)
    }

    fn take(g: &mut EGraph, step: &Step) -> Result<ClassId, Refused> {
        match step {
            Step::Add(i) => g.add(k(*i), "trusted"),
            Step::Record(i, o) => g.record(k(*i), "trusted", &out(*o)),
            Step::Claim(a, b) => g.merge(&k(*a), &k(*b), Why::Claim("test".into())),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// W24-R30: the classes are a union-find's. A step is taken whole or refused for cause and
        /// changes nothing; a claim taken leaves its keys one class, and taken again changes
        /// nothing; the classes are the components of what was taken (the claims, and keys whose
        /// units produced the same outputs); each class has one root, its outputs when it has
        /// them, else its least key.
        #[test]
        fn w24_r30_union_find_laws(history in steps()) {
            let mut g = EGraph::new("trusted");
            let mut claimed = Vec::new();
            for step in &history {
                let before = g.clone();
                match take(&mut g, step) {
                    Err(e) => {
                        prop_assert_eq!(&g, &before, "a refused step changes nothing");
                        match (&e, step) {
                            (Refused::Unknown(x), Step::Claim(..)) => prop_assert!(before.member(x).is_none()),
                            (Refused::Conflict { held, seen }, Step::Claim(a, b)) => {
                                prop_assert_eq!(before.find(&k(*a)).and_then(ClassId::output), Some(held.as_str()));
                                prop_assert_eq!(before.find(&k(*b)).and_then(ClassId::output), Some(seen.as_str()));
                            }
                            (Refused::Conflict { held, seen }, Step::Record(i, o)) => {
                                prop_assert_eq!(seen, &out(*o));
                                prop_assert!(before.member(&k(*i)).is_some_and(|m| m.produced.as_ref() == Some(held) || m.class.output() == Some(held.as_str())));
                            }
                            _ => return Err(TestCaseError::fail(format!("refused for no cause: {e:?} on {step:?}"))),
                        }
                    }
                    Ok(id) => match step {
                        Step::Claim(a, b) => {
                            prop_assert!(g.same(&k(*a), &k(*b)));
                            prop_assert_eq!(g.find(&k(*a)), Some(&id));
                            claimed.push((k(*a), k(*b)));
                            let again = g.clone();
                            prop_assert!(g.merge(&k(*a), &k(*b), Why::Claim("again".into())).is_ok());
                            prop_assert_eq!(&g, &again, "idempotent");
                        }
                        Step::Record(i, o) => prop_assert_eq!(g.find(&k(*i)), Some(&ClassId::Output(out(*o)))),
                        Step::Add(i) => prop_assert_eq!(g.find(&k(*i)), Some(&id)),
                    },
                }
            }
            let mut naive = Naive::new();
            for x in g.members.keys() {
                naive.add(*x);
            }
            for (a, b) in &claimed {
                naive.union(a, b);
            }
            for (a, x) in &g.members {
                for (b, y) in &g.members {
                    if x.produced.is_some() && x.produced == y.produced {
                        naive.union(a, b);
                    }
                }
            }
            prop_assert_eq!(partition(&g), naive.partition());
            for (id, keys) in g.classes() {
                let outputs: BTreeSet<&str> = keys.iter().filter_map(|x| g.member(x).and_then(|m| m.produced.as_deref())).collect();
                match id {
                    ClassId::Output(d) => prop_assert_eq!(outputs, BTreeSet::from([d.as_str()])),
                    ClassId::Pending(least) => {
                        prop_assert!(outputs.is_empty());
                        prop_assert_eq!(keys.iter().next(), Some(least));
                    }
                }
                for x in keys {
                    prop_assert_eq!(g.find(x), Some(id));
                    prop_assert!(g.same(x, x));
                }
            }
        }

        /// W24-R30: claims commute: two taken in either order leave the same classes, ids and all.
        #[test]
        fn w24_r30_claims_commute(history in steps(), a in (0..10u8, 0..10u8), b in (0..10u8, 0..10u8)) {
            let mut g = EGraph::new("trusted");
            for step in &history {
                let _ = take(&mut g, step);
            }
            let (mut ab, mut ba) = (g.clone(), g);
            let taken = (take(&mut ab, &Step::Claim(a.0, a.1)).is_ok(), take(&mut ab, &Step::Claim(b.0, b.1)).is_ok(), take(&mut ba, &Step::Claim(b.0, b.1)).is_ok(), take(&mut ba, &Step::Claim(a.0, a.1)).is_ok());
            if taken == (true, true, true, true) {
                prop_assert_eq!(ab.classes(), ba.classes());
            }
        }
    }

    /// A random DAG: each node a definition, up to two inputs among the nodes before it (one with
    /// none is a leaf), and whether it ran.
    fn dag() -> impl Strategy<Value = Vec<(u8, Vec<Index>, bool)>> {
        proptest::collection::vec((0..4u8, proptest::collection::vec(any::<Index>(), 0..3), any::<bool>()), 1..14)
    }

    /// What the DAGs' inner nodes run.
    const DEFS: [&str; 4] = ["make", "test", "make\ntrue", "true\ntest"];

    /// A script without its no-ops, worked out without the rules.
    fn meaning(def: &str) -> String {
        def.split('\n').filter(|op| op.trim() != "true").collect::<Vec<_>>().join("\n")
    }

    /// A DAG's nodes as terms of `g`, each over its inputs by key, and those that ran recorded with
    /// what they produce: the leaves (`leaf0` and `leaf2` alike, as `leaf1` and `leaf3` are) and
    /// the inner nodes whose unit ran, their outputs worked out from their inputs' without the rules
    /// (which keep what a script does). Each node: its key, definition, inputs, what it produces,
    /// and whether that was seen.
    fn grow(g: &mut EGraph, nodes: &[(u8, Vec<Index>, bool)], with_rules: bool) -> Result<Vec<(Key, String, Vec<usize>, String, bool)>, Refused> {
        let mut built: Vec<(Key, String, Vec<usize>, String, bool)> = Vec::new();
        for (i, (def, inputs, ran)) in nodes.iter().enumerate() {
            let inputs: Vec<usize> = if i == 0 { Vec::new() } else { inputs.iter().map(|x| x.index(i)).collect() };
            let (definition, truth) = if inputs.is_empty() {
                (format!("leaf{def}"), format!("sha256:leaf{}", def % 2))
            } else {
                let definition = DEFS.get(usize::from(*def)).copied().unwrap_or_default().to_owned();
                let mut h = Sha256::new();
                h.update(if with_rules { meaning(&definition) } else { definition.clone() });
                for (n, j) in inputs.iter().enumerate() {
                    h.update(format!("/in{n}={}", built.get(*j).map(|b| b.3.as_str()).unwrap_or_default()));
                }
                (definition, format!("sha256:{}", Key(h.finalize().into()).hex()))
            };
            let upstream: Vec<(String, String)> = inputs.iter().enumerate().filter_map(|(n, j)| built.get(*j).map(|b| (format!("in{n}"), refer(&b.0)))).collect();
            let u = unit(&definition, &upstream.iter().map(|(n, v)| (n.as_str(), v.clone())).collect::<Vec<_>>());
            let own = g.add_term("trusted", &u)?;
            let seen = inputs.is_empty() || *ran;
            if seen {
                g.record(own, "trusted", &truth)?;
            }
            built.push((own, definition, inputs, truth, seen));
        }
        Ok(built)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(5_000))]

        /// W24-R30: congruence closure on random DAGs. After rebuilding, two nodes are one class
        /// exactly when the smallest congruence holding the outputs seen says so, found here by
        /// brute force (nodes alike, or that produced the same outputs, or the same operation over
        /// equivalent inputs, to a fixed point), with the rules' rewrites or without them. And it's
        /// sound: a class's nodes would all produce one output, and a class with outputs has the
        /// ones each of its nodes would produce, those that never ran included.
        #[test]
        fn w24_r30_congruence_closure_on_random_dags(nodes in dag(), with_rules in any::<bool>()) {
            let rules = if with_rules { sound()? } else { Rules::default() };
            let same_op = |a: &str, b: &str| if with_rules { meaning(a) == meaning(b) } else { a == b };
            let mut g = EGraph::new("trusted");
            let built = grow(&mut g, &nodes, with_rules)?;
            prop_assert!(g.rebuild(&rules).is_empty());
            let mut naive = Naive::new();
            for i in 0..built.len() {
                naive.add(i);
            }
            for (i, a) in built.iter().enumerate() {
                for (j, b) in built.iter().enumerate() {
                    if a.0 == b.0 || (a.4 && b.4 && a.3 == b.3) {
                        naive.union(&i, &j);
                    }
                }
            }
            loop {
                let mut changed = false;
                for (i, a) in built.iter().enumerate() {
                    for (j, b) in built.iter().enumerate() {
                        if !a.2.is_empty() && a.2.len() == b.2.len() && same_op(&a.1, &b.1) && a.2.iter().zip(&b.2).all(|(x, y)| naive.same(x, y)) && naive.union(&i, &j) {
                            changed = true;
                        }
                    }
                }
                if !changed {
                    break;
                }
            }
            for (i, a) in built.iter().enumerate() {
                for (j, b) in built.iter().enumerate() {
                    prop_assert_eq!(g.same(&a.0, &b.0), naive.same(&i, &j), "nodes {} and {}", i, j);
                    if g.same(&a.0, &b.0) {
                        prop_assert_eq!(&a.3, &b.3, "one class, one output");
                    }
                }
                if let Some(d) = g.find(&a.0).and_then(ClassId::output) {
                    prop_assert_eq!(d, a.3.as_str());
                }
            }
        }
    }

    fn domain() -> impl Strategy<Value = String> {
        prop_oneof![Just("trusted"), Just("protected"), Just("untrusted"), Just("person:ada"), Just("person:bo")].prop_map(str::to_owned)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// W24-R30, R21: no merge crosses a trust domain. Whatever is added, recorded and claimed,
        /// by whichever domains, an e-graph's classes hold only entries its domain may read; what
        /// it may not read is refused and changes nothing; a trusted e-graph holds trusted entries
        /// only, so untrusted evidence never enters a class trusted lookups use.
        #[test]
        fn w24_r30_no_cross_domain_merge(reader in domain(), steps in proptest::collection::vec((0..8u8, domain(), 0..3u8, 0..8u8, 0..3u8), 0..40)) {
            let mut g = EGraph::new(&reader);
            for (i, writer, o, j, what) in &steps {
                let before = g.clone();
                let r = match what {
                    0 => g.add(k(*i), writer),
                    1 => g.record(k(*i), writer, &out(*o)),
                    _ => g.merge(&k(*i), &k(*j), Why::Claim("test".into())),
                };
                if *what < 2 && !reads(&reader, writer) {
                    let refused = matches!(r, Err(Refused::Domain { .. }));
                    prop_assert!(refused, "{:?}", r);
                }
                if r.is_err() {
                    prop_assert_eq!(&g, &before);
                }
            }
            for keys in g.classes().values() {
                for x in keys {
                    let writer = g.member(x).map(|m| m.writer.as_str()).unwrap_or_default();
                    prop_assert!(reads(&reader, writer), "{} holds what {} wrote", reader, writer);
                    if reader == "trusted" {
                        prop_assert_eq!(writer, "trusted");
                    }
                }
            }
        }

        /// W24-R30: a split restores separation. Verification sees a key's unit produce other
        /// outputs than its class's: every key the class's claims joined it to is quarantined and
        /// takes no claim; the key goes to the class of what it produced (or, having produced
        /// otherwise before, is nondeterministic and alone); the others keep their class only by
        /// their own outputs, else stand alone; nothing the claims didn't reach changes; and the
        /// classes are those what's left makes from scratch.
        #[test]
        fn w24_r30_a_split_restores_separation(history in steps(), at in 0..10u8, seen in 0..4u8) {
            let mut g = EGraph::new("trusted");
            for step in &history {
                let _ = take(&mut g, step);
            }
            let (at, observed) = (k(at), out(seen));
            let Some(was) = g.member(&at).cloned() else { return Ok(()) };
            let before = g.clone();
            let (tree, _) = before.tree(&at);
            let quarantined = g.split(&at, Some(&observed))?;
            if was.class.output() == Some(observed.as_str()) {
                prop_assert!(quarantined.is_empty());
                prop_assert_eq!(&g, &before, "verification agreed: nothing splits");
                return Ok(());
            }
            prop_assert_eq!(&quarantined, &tree.keys().copied().collect::<BTreeSet<_>>(), "the claims' reach, no more: no terms, so no lineage");
            for (x, m) in &tree {
                let now = g.member(x).cloned().ok_or_else(|| TestCaseError::fail("a key went"))?;
                prop_assert!(now.quarantined);
                if *x == at {
                    if was.produced.is_some() {
                        prop_assert!(now.nondeterministic);
                        prop_assert_eq!(&now.class, &ClassId::Pending(at));
                    } else {
                        prop_assert_eq!(&now.class, &ClassId::Output(observed.clone()));
                    }
                } else {
                    prop_assert!(!g.same(x, &at), "apart from what its claims joined it to");
                    if m.anchored() {
                        prop_assert_eq!(&now.class, &m.class, "its own outputs keep it");
                    } else {
                        prop_assert_eq!(&now.class, &ClassId::Pending(*x), "alone");
                    }
                }
                for y in before.members.keys() {
                    prop_assert_eq!(g.clone().merge(x, y, Why::Claim("again".into())), Err(Refused::Quarantined(*x)));
                }
            }
            for (x, m) in &before.members {
                if !tree.contains_key(x) {
                    prop_assert_eq!(g.member(x), Some(m), "what the claims didn't reach stands");
                }
            }
            let mut fresh = EGraph::new("trusted");
            for (x, m) in &g.members {
                fresh.add(*x, &m.writer)?;
                if let Some(p) = &m.produced {
                    fresh.record(*x, &m.writer, p)?;
                }
            }
            for (a, others) in &g.claims {
                for (b, why) in others {
                    if a < b {
                        fresh.merge(a, b, why.clone())?;
                    }
                }
            }
            prop_assert_eq!(fresh.classes(), g.classes());
        }
    }

    #[test]
    fn w24_r30_untrusted_evidence_never_enters_a_trusted_class() -> Result<(), Refused> {
        let mut trusted = EGraph::new("trusted");
        trusted.record(k(1), "trusted", &out(1))?;
        assert!(matches!(trusted.record(k(2), "untrusted", &out(1)), Err(Refused::Domain { .. })));
        assert!(matches!(trusted.add(k(3), "person:ada"), Err(Refused::Domain { .. })));
        assert_eq!(trusted.classes().get(&ClassId::Output(out(1))), Some(&BTreeSet::from([k(1)])));
        // Reads go down only: untrusted lookups read trusted entries, so their classes hold both.
        let mut untrusted = EGraph::new("untrusted");
        untrusted.record(k(1), "trusted", &out(1))?;
        untrusted.record(k(2), "untrusted", &out(1))?;
        assert!(untrusted.same(&k(1), &k(2)));
        // A person's builds read trusted entries, not another person's; protected reads protected only.
        let mut ada = EGraph::new("person:ada");
        ada.record(k(1), "trusted", &out(1))?;
        assert!(matches!(ada.record(k(3), "person:bo", &out(1)), Err(Refused::Domain { .. })));
        let mut protected = EGraph::new("protected");
        assert!(matches!(protected.record(k(1), "trusted", &out(1)), Err(Refused::Domain { .. })));
        Ok(())
    }

    /// A unit over an upstream unit's key, before that unit's outputs are materialized, takes it by
    /// class: over a class with outputs it keys as the unit over those outputs, which ran; over one
    /// without, as every unit over an equivalent input does.
    #[test]
    fn w24_r30_upstream_units_are_taken_by_class() -> Result<(), Refused> {
        let mut g = EGraph::new("trusted");
        let rules = Rules::default();
        let build = g.add_term("trusted", &unit("make", &[]))?;
        g.record(build, "trusted", &out(1))?;
        let planned = unit("test", &[("build", refer(&build))]);
        let ran = unit("test", &[("build", out(1))]);
        let c = canonical(&planned, &rules, &|x| g.rep(x));
        assert_eq!(c.key, key::weak(&ran));
        assert_eq!(c.why, Why::Canonical { rules: Vec::new(), by_class: vec!["build".into()] });
        assert_eq!(canonical(&ran, &rules, &|x| g.rep(x)).key, key::weak(&ran), "a unit over its inputs' outputs is its own canonical form");
        let unknown = unit("test", &[("build", refer(&k(42)))]);
        assert_eq!(canonical(&unknown, &rules, &|x| g.rep(x)).key, key::weak(&unknown), "one with no class stays as it's named");
        let (a, b) = (g.add_term("trusted", &unit("fetch a", &[]))?, g.add_term("trusted", &unit("fetch b", &[]))?);
        g.merge(&a, &b, Why::Claim("one tarball".into()))?;
        let over = |x: &Key| canonical(&unit("test", &[("build", refer(x))]), &rules, &|y| g.rep(y)).key;
        assert_eq!(over(&a), over(&b), "congruent: the same operation over equivalent inputs");
        assert_ne!(over(&a), over(&build));
        Ok(())
    }

    /// A rewritten unit that never ran joins the class of the one that did, and what runs over it
    /// finds the result congruent to it before its outputs exist. Then verification runs it and
    /// sees other outputs: the claim splits, and its lineage (what took it by class) is
    /// quarantined, and stays apart as the e-graph rebuilds.
    #[test]
    fn w24_r30_a_split_quarantines_its_lineage() -> Result<(), Refused> {
        let rules = sound()?;
        let mut g = EGraph::new("trusted");
        let x = g.add_term("trusted", &unit("make", &[]))?;
        g.record(x, "trusted", &out(1))?;
        let x2 = g.add_term("trusted", &unit("make\ntrue", &[]))?;
        let y = g.add_term("trusted", &unit("test", &[("in", refer(&x))]))?;
        g.record(y, "trusted", &out(5))?;
        let y2 = g.add_term("trusted", &unit("test", &[("in", refer(&x2))]))?;
        let other = g.add_term("trusted", &unit("lint", &[]))?;
        assert!(g.rebuild(&rules).is_empty());
        assert!(g.same(&x, &x2) && g.same(&y, &y2));
        assert_eq!(g.find(&y2), Some(&ClassId::Output(out(5))), "found before x2's outputs are");
        let q = g.split(&x2, Some(&out(2)))?;
        assert_eq!(q, BTreeSet::from([x, x2, y, y2]));
        assert!(!g.same(&x, &x2) && !g.same(&y, &y2));
        assert_eq!(g.find(&x2), Some(&ClassId::Output(out(2))));
        assert_eq!((g.find(&x), g.find(&y)), (Some(&ClassId::Output(out(1))), Some(&ClassId::Output(out(5)))), "their own outputs keep them");
        assert_eq!(g.find(&y2), Some(&ClassId::Pending(y2)));
        assert!(g.member(&other).is_some_and(|m| !m.quarantined));
        assert!(g.rebuild(&rules).is_empty());
        assert!(!g.same(&y, &y2), "quarantined, it isn't merged again");
        assert_eq!(g.rep(&x), None, "nor taken by class");
        // Its unit, run again with other outputs still, is nondeterministic.
        let again = g.split(&x2, Some(&out(3)))?;
        assert!(again.contains(&x2) && g.member(&x2).is_some_and(|m| m.nondeterministic));
        assert!(matches!(g.record(x2, "trusted", &out(3)), Err(Refused::Nondeterministic(_))));
        Ok(())
    }

    /// Classes kept as the work ledger keeps them: a row per member, claims and lineage as pairs,
    /// and no index of classes.
    #[derive(Debug, Default, PartialEq)]
    struct Rows {
        members: BTreeMap<Key, Member>,
        claims: BTreeSet<(Key, Key)>,
        lineage: BTreeSet<(Key, Key)>,
    }

    impl Rows {
        fn of(g: &EGraph) -> Rows {
            Rows {
                members: g.members.clone(),
                claims: g.claims.iter().flat_map(|(a, others)| others.keys().map(move |b| (*a, *b))).collect(),
                lineage: g.parents.iter().flat_map(|(up, took)| took.iter().map(move |t| (*up, *t))).collect(),
            }
        }
    }

    impl Classes for Rows {
        type Error = Refused;

        fn entry(&self, k: &Key) -> Result<Option<Member>, Refused> {
            Ok(self.members.get(k).cloned())
        }

        fn claimed(&self, k: &Key) -> Result<BTreeSet<Key>, Refused> {
            Ok(self.claims.iter().filter(|(a, _)| a == k).map(|(_, b)| *b).collect())
        }

        fn lineage(&self, k: &Key) -> Result<BTreeSet<Key>, Refused> {
            Ok(self.lineage.iter().filter(|(up, _)| up == k).map(|(_, t)| *t).collect())
        }

        fn apply(&mut self, split: &Split) -> Result<(), Refused> {
            for (a, b) in &split.dropped {
                self.claims.remove(&(*a, *b));
                self.claims.remove(&(*b, *a));
            }
            self.members.extend(split.members.iter().map(|(k, m)| (*k, m.clone())));
            Ok(())
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// W24-R30: a split decides alike wherever classes are kept. On a random DAG's e-graph,
        /// rebuilt, and on the same classes kept as the ledger keeps them (rows, no index of
        /// classes), splitting one node with whatever verification saw (other outputs, its own, or
        /// nothing as a lineage reaches it) quarantines the same keys and leaves the same members,
        /// claims and lineage.
        #[test]
        fn w24_r30_a_split_decides_alike_wherever_classes_are_kept(nodes in dag(), with_rules in any::<bool>(), at in any::<Index>(), seen in proptest::option::of(0..4u8)) {
            let (mut g, rules) = (EGraph::new("trusted"), if with_rules { sound()? } else { Rules::default() });
            let built = grow(&mut g, &nodes, with_rules)?;
            prop_assert!(g.rebuild(&rules).is_empty());
            let Some((at, truth)) = built.get(at.index(built.len())).map(|b| (b.0, b.3.clone())) else { return Ok(()) };
            let observed = seen.map(|s| if s == 3 { truth } else { out(s) });
            let mut rows = Rows::of(&g);
            let by_rows = split(&mut rows, &at, observed.as_deref());
            let by_graph = g.split(&at, observed.as_deref());
            prop_assert_eq!(&by_rows, &by_graph);
            prop_assert_eq!(rows, Rows::of(&g));
        }
    }

    /// What a unit's outputs make of its key ([`producing`]), and what a member serves.
    #[test]
    fn w24_r30_what_outputs_make_of_a_key() -> Result<(), Refused> {
        let first = producing(&k(1), None, "trusted", &out(1))?;
        assert_eq!((first.class.output(), first.writer.as_str()), (Some(out(1).as_str()), "trusted"));
        assert!(first.anchored() && first.claimable() && first.serves());
        assert_eq!(first.rep(), Some(out(1)), "taken by class, it's its outputs");
        assert_eq!(producing(&k(1), Some(&first), "untrusted", &out(1))?, first, "the same outputs again: as it was, its writer kept");
        assert_eq!(producing(&k(1), Some(&first), "trusted", &out(2)), Err(Refused::Conflict { held: out(1), seen: out(2) }));
        let claimed = Member { produced: None, ..first.clone() };
        assert!(!claimed.anchored() && claimed.serves(), "claimed into a class with outputs, it serves by class");
        assert_eq!(producing(&k(1), Some(&claimed), "trusted", &out(2)), Err(Refused::Conflict { held: out(1), seen: out(2) }), "its class holds other outputs");
        let alone = Member { class: ClassId::Pending(k(1)), quarantined: true, ..claimed };
        assert!(!alone.serves() && !alone.claimable() && alone.rep().is_none());
        let back = producing(&k(1), Some(&alone), "trusted", &out(2))?;
        assert!(back.anchored() && back.quarantined, "its own outputs put it in their class; quarantined still, it serves no class-based hit");
        assert!(!back.serves());
        let odd = Member { nondeterministic: true, ..first };
        assert_eq!(producing(&k(1), Some(&odd), "trusted", &out(1)), Err(Refused::Nondeterministic(k(1))));
        assert!(!odd.claimable() && !odd.serves());
        assert_eq!(producing(&k(1), None, "trusted", "a|b"), Err(Refused::Digest("a|b".into())));
        Ok(())
    }

    #[test]
    fn w24_r30_rules_apply_only_registered_with_a_witness() -> Result<(), Refused> {
        let mut rules = Rules::default();
        assert_eq!(rules.normalize(Kind::Step, "make\ntrue"), ("make\ntrue".to_owned(), Vec::new()), "nothing registered: nothing rewrites");
        let drop_true = || drop_rule("drop-true", Pattern::Is("true".into()));
        let refused = |r: Result<(), Refused>| matches!(r, Err(Refused::Rule { .. }));
        assert!(refused(rules.register(drop_true(), Witness { test: "differential".into(), cases: Vec::new() })), "no case");
        assert!(refused(rules.register(drop_true(), Witness { test: " ".into(), ..witness("make\ntrue", "make") })), "no test named");
        assert!(refused(rules.register(drop_true(), witness("make\ntrue", "test"))), "a case it doesn't make one");
        assert!(refused(rules.register(drop_true(), witness("make", "make"))), "a case that rewrites nothing");
        let mut differs = witness("make\ntrue", "make");
        if let Some(c) = differs.cases.first_mut() {
            c.after_output = out(8);
        }
        assert!(refused(rules.register(drop_true(), differs)), "the runs' outputs differ");
        assert!(refused(rules.register(drop_rule("drop-all", Pattern::Starts(" ".into())), witness("make\ntrue", ""))), "every operation");
        assert!(refused(rules.register(drop_rule("", Pattern::Is("true".into())), witness("make\ntrue", "make"))), "no name");
        assert_eq!(rules.rules().count(), 0);
        rules.register(drop_true(), witness("make\ntrue", "make"))?;
        assert!(refused(rules.register(drop_true(), witness("make\ntrue", "make"))), "a name held");
        assert_eq!(rules.normalize(Kind::Step, "true\nmake\n  true"), ("make".to_owned(), vec!["drop-true".to_owned()]));
        assert_eq!(rules.witness("drop-true").map(|w| w.test.as_str()), Some("differential"));
        // A rule for actions leaves steps as they are.
        let mut actions = Rules::default();
        actions.register(Rule { kinds: BTreeSet::from([Kind::Action]), ..drop_true() }, witness("make\ntrue", "make"))?;
        assert_eq!(actions.normalize(Kind::Step, "make\ntrue").0, "make\ntrue");
        assert_eq!(actions.normalize(Kind::Action, "make\ntrue").0, "make");
        // Commuting operations: in one order, the rule that reordered them named.
        let rules = sound()?;
        assert_eq!(rules.normalize(Kind::Step, "add b\nadd a\nmake\nadd c"), ("add a\nadd b\nmake\nadd c".to_owned(), vec!["adds-commute".to_owned()]));
        assert_eq!(rules.normalize(Kind::Step, "add a\nadd b\nmake"), ("add a\nadd b\nmake".to_owned(), Vec::new()), "already in order");
        // As data, a rule reads back.
        let json = serde_json::to_string(&commute_rule("c", Pattern::Starts("add ".into()), Pattern::Is("true".into()))).map_err(|e| Refused::Digest(e.to_string()))?;
        let back: Rule = serde_json::from_str(&json).map_err(|e| Refused::Digest(e.to_string()))?;
        assert_eq!(back, commute_rule("c", Pattern::Starts("add ".into()), Pattern::Is("true".into())));
        Ok(())
    }

    /// W24-R30: assignments of different names commute and those of one name never do; listed
    /// flags come in one order, in their places; a rule of either kind that names everything is
    /// refused.
    #[test]
    fn w24_r30_assignments_and_flags() -> Result<(), Refused> {
        let mut rules = Rules::default();
        let assign = Rule { name: "env-order".into(), kinds: BTreeSet::new(), rewrite: Rewrite::Assign { prefix: "export ".into() } };
        let flags = Rule { name: "cargo-flags".into(), kinds: BTreeSet::new(), rewrite: Rewrite::Flags { op: Pattern::Starts("cargo ".into()), flags: BTreeSet::from(["--locked".to_owned(), "--release".to_owned()]) } };
        let refused = |r: Result<(), Refused>| matches!(r, Err(Refused::Rule { .. }));
        assert!(refused(rules.register(Rule { rewrite: Rewrite::Assign { prefix: " ".into() }, ..assign.clone() }, witness("export B=2\nexport A=1", "export A=1\nexport B=2"))), "every assignment");
        assert!(refused(rules.register(Rule { rewrite: Rewrite::Flags { op: Pattern::Starts("cargo ".into()), flags: BTreeSet::new() }, ..flags.clone() }, witness("cargo b", "cargo c"))), "no flags");
        rules.register(assign, witness("export B=2\nexport A=1", "export A=1\nexport B=2"))?;
        rules.register(flags, witness("cargo build --release --locked", "cargo build --locked --release"))?;
        let n = |d: &str| rules.normalize(Kind::Step, d).0;
        assert_eq!(n("export B=2\nexport A=1\nmake"), n("export A=1\nexport B=2\nmake"));
        assert_ne!(n("export A=1\nexport A=2"), n("export A=2\nexport A=1"), "one name set twice: the last stands");
        assert_eq!(n("cargo build --release -p x --locked"), "cargo build --locked -p x --release");
        assert_eq!(n("cargo build --locked -p x --release"), "cargo build --locked -p x --release");
        assert_eq!(n("make --release --locked"), "make --release --locked", "only the tool it names");
        Ok(())
    }

    fn script() -> impl Strategy<Value = Vec<&'static str>> {
        proptest::collection::vec(prop_oneof![Just("add a"), Just("add b"), Just("add c"), Just("true"), Just("make"), Just("test"), Just("")], 0..10)
    }

    /// What a script does, worked out without the rules: the features it adds (a set), and each
    /// other step with the features it saw. `true` and blank lines do nothing.
    fn effect(def: &str) -> (BTreeSet<String>, Vec<(String, BTreeSet<String>)>) {
        let (mut features, mut steps) = (BTreeSet::new(), Vec::new());
        for op in def.split('\n').filter(|op| !op.trim().is_empty() && op.trim() != "true") {
            match op.strip_prefix("add ") {
                Some(f) => {
                    features.insert(f.to_owned());
                }
                None => steps.push((op.to_owned(), features.clone())),
            }
        }
        (features, steps)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(5_000))]

        /// W24-R30: a normal form is one for every order and padding the rules allow: a no-op put
        /// anywhere, or two adjacent operations that commute swapped, normalizes alike; normalizing
        /// again changes nothing; and sound rules keep what a script does.
        #[test]
        fn w24_r30_normal_forms(ops in script(), at in any::<Index>(), swap in any::<Index>()) {
            let rules = sound()?;
            let norm = |ops: &[&str]| rules.normalize(Kind::Step, &ops.join("\n")).0;
            let n = norm(&ops);
            prop_assert_eq!(rules.normalize(Kind::Step, &n).0, n.clone(), "idempotent");
            prop_assert_eq!(effect(&n), effect(&ops.join("\n")), "sound");
            let mut padded = ops.clone();
            padded.insert(at.index(ops.len() + 1), "true");
            prop_assert_eq!(norm(&padded), n.clone());
            if ops.len() >= 2 {
                let i = swap.index(ops.len() - 1);
                let (x, y) = (ops.get(i).copied().unwrap_or_default(), ops.get(i + 1).copied().unwrap_or_default());
                if x.starts_with("add ") && y.starts_with("add ") {
                    let mut swapped = ops.clone();
                    swapped.swap(i, i + 1);
                    prop_assert_eq!(norm(&swapped), n.clone());
                }
            }
        }
    }

    #[test]
    fn w24_r30_keys_and_digests_read_back() -> Result<(), serde_json::Error> {
        let x = key::weak(&unit("make", &[]));
        assert_eq!(parse_key(&x.hex()), Some(x));
        assert_eq!(reference(&refer(&x)), Some(x));
        assert_eq!(parse_key(&x.hex().to_uppercase()), None);
        assert_eq!(parse_key(&format!("+{}", x.hex().get(1..).unwrap_or_default())), None);
        assert_eq!(reference("sha256:abc"), None);
        for id in [ClassId::Pending(x), ClassId::Output(out(1))] {
            let back: ClassId = serde_json::from_str(&serde_json::to_string(&id)?)?;
            assert_eq!(back, id);
        }
        let outputs = BTreeMap::from([("bin".to_owned(), "sha256:b".to_owned())]);
        let d = output_digest("success", Some(0), &outputs, &[]);
        assert!(digest_ok(&d));
        assert_eq!(d, output_digest("success", Some(0), &outputs.clone(), &[]));
        assert_ne!(d, output_digest("success", Some(1), &outputs, &[]));
        assert_ne!(d, output_digest("success", None, &outputs, &[]));
        assert_ne!(d, output_digest("success", Some(0), &BTreeMap::new(), &[]));
        assert_ne!(d, output_digest("success", Some(0), &outputs, &["sha256:r".to_owned()]));
        assert!(!digest_ok("") && !digest_ok("a|b") && !digest_ok(&refer(&x)));
        Ok(())
    }
}
