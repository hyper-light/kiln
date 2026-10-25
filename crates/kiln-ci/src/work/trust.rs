//! Trust domains and lineage (docs/ci.md W24-R21, R22), pure: which domains' entries a unit may
//! read, where it may write, whether an entry a lookup finds is taken, how two results of one key
//! compare, and what quota pressure may evict.
//!
//! - **Domains** ([`Domain`]): `trusted` (default-branch pushes and the land queue), `protected`
//!   (release and deploy legs), `untrusted` (forks and outside contributors, and by default `diff`
//!   and `agent` events, as W08-R14 has them) and `person:<id>` (a person's agents' own builds).
//! - **Reads go down only** ([`Domain::may_read`]): what a domain writes flows to the domains below
//!   it, protected to trusted to untrusted and each person. Trusted never reads untrusted or person
//!   entries (W18-R07), and people don't read each other's. The one way up is decision 4's: a
//!   protected unit takes only results two independent trusted runners produced identically
//!   ([`Domain::needs_two_runners`]), trusted ones among them, else it runs cold.
//! - **Writes stay home** ([`Domain::may_write`], [`lineage_fits`]): a unit records only into its
//!   own domain, and only a result whose lineage (the domains its inputs came from: results it
//!   reused, the warm workspace it ran in) its domain may read. So promotion is by reproduction
//!   alone: a trusted run that reproduces a person's result writes an entry of its own, and nothing
//!   is ever copied up.
//! - **Entries are never overwritten** ([`compare`]): a strong key recorded again with the same
//!   outcome and outputs is a reproduction, its runner one more producer (R23); a different result
//!   is a conflict, refused and kept apart as evidence for verification to judge.
//! - **Quotas per domain** ([`evict`]): a domain over its quota loses its own least valuable
//!   entries, never another domain's.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// A trust domain (W24-R21): whose entries a unit writes, and which it may read.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Domain {
    /// Default-branch pushes and the land queue.
    Trusted,
    /// Release and deploy legs.
    Protected,
    /// Forks and outside contributors; by default `diff` and `agent` events.
    Untrusted,
    /// One person's agents' own builds.
    Person(String),
}

impl Domain {
    /// `trusted`, `protected`, `untrusted` or `person:<id>` (an id with no spaces, controls or `|`,
    /// which separates a ledger key's parts).
    pub fn parse(s: &str) -> Result<Domain, String> {
        match s {
            "trusted" => Ok(Domain::Trusted),
            "protected" => Ok(Domain::Protected),
            "untrusted" => Ok(Domain::Untrusted),
            _ => match s.strip_prefix("person:") {
                Some(id) if !id.is_empty() && id.chars().all(|c| !c.is_control() && !c.is_whitespace() && c != '|') => Ok(Domain::Person(id.to_owned())),
                Some(_) => Err(format!("`{s}`: a person's domain is `person:<id>`, the id without spaces or `|`")),
                None => Err(format!("`{s}`: a trust domain is trusted, protected, untrusted or person:<id>")),
            },
        }
    }

    /// A run's legs' domain: protected for release and deploy legs, else trusted or untrusted as
    /// the run is (W18-R05).
    pub fn of_run(trusted: bool, protected: bool) -> Domain {
        match (trusted, protected) {
            (true, true) => Domain::Protected,
            (true, false) => Domain::Trusted,
            (false, _) => Domain::Untrusted,
        }
    }

    /// How far up it is: protected, then trusted, then untrusted and each person, side by side.
    fn rank(&self) -> u8 {
        match self {
            Domain::Protected => 2,
            Domain::Trusted => 1,
            Domain::Untrusted | Domain::Person(_) => 0,
        }
    }

    /// Whether what `writer` wrote flows down to a unit of this domain (R21): its own, and every
    /// domain above it. Nothing flows up or sideways; a protected unit's trusted reads are
    /// [`Domain::needs_two_runners`]'s.
    pub fn may_read(&self, writer: &Domain) -> bool {
        self == writer || writer.rank() > self.rank()
    }

    /// Whether this domain takes only results two independent trusted runners produced
    /// identically, and with that reads trusted entries as well as its own: a protected unit's
    /// (decision 4). Without two, it runs cold.
    pub fn needs_two_runners(&self) -> bool {
        *self == Domain::Protected
    }

    /// Whether a unit of this domain may take what `writer` wrote, once [`admit`]'s other checks
    /// pass: what [`Domain::may_read`] lets down, and trusted entries for a protected unit.
    fn reads(&self, writer: &Domain) -> bool {
        self.may_read(writer) || (self.needs_two_runners() && *writer == Domain::Trusted)
    }

    /// The domains a lookup tries, the most trusted first: when two hold a result for the unit,
    /// it takes the one with the better provenance.
    pub fn lookups(&self) -> Vec<Domain> {
        let mut out = vec![Domain::Protected, Domain::Trusted];
        match self {
            Domain::Protected | Domain::Trusted => {}
            other => out.push(other.clone()),
        }
        out
    }

    /// Whether a unit of this domain may record into `scope`'s entries: its own only (R21: what
    /// another domain holds gets there by reproduction, never by copying).
    pub fn may_write(&self, scope: &Domain) -> bool {
        self == scope
    }
}

impl std::fmt::Display for Domain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Domain::Trusted => f.write_str("trusted"),
            Domain::Protected => f.write_str("protected"),
            Domain::Untrusted => f.write_str("untrusted"),
            Domain::Person(id) => write!(f, "person:{id}"),
        }
    }
}

impl TryFrom<String> for Domain {
    type Error = String;
    fn try_from(s: String) -> Result<Domain, String> {
        Domain::parse(&s)
    }
}

impl From<Domain> for String {
    fn from(d: Domain) -> String {
        d.to_string()
    }
}

/// Whether a result whose inputs came from `lineage`'s domains may be written into `domain` (R21):
/// each is one `domain` reads, so nothing reaches a domain from below through what was built on it
/// either. A protected result may build on trusted ones, which it read only verified.
pub fn lineage_fits(domain: &Domain, lineage: &[String]) -> Result<(), String> {
    for l in lineage {
        let from = Domain::parse(l).map_err(|e| format!("its lineage: {e}"))?;
        if !domain.reads(&from) {
            return Err(format!("a {domain} result can't build on a {from} one: reads go down only (W24-R21)"));
        }
    }
    Ok(())
}

/// What a lookup found, as a reader judges it: everything comes from the entry's own record, so
/// a forged or misplaced row is judged by what it says of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found<'a> {
    /// The domain whose entries hold it.
    pub scope: &'a Domain,
    /// The domain its record says wrote it.
    pub writer: &'a str,
    /// The domains its inputs came from.
    pub lineage: &'a [String],
    /// The runner that produced it and each that reproduced it since, all of its domain.
    pub runners: Vec<&'a str>,
    /// Why it's quarantined (R23), when it is.
    pub quarantined: Option<&'a str>,
}

/// The independent runners among `runners`: each named once, unnamed ones not counted.
pub fn independent<'a>(runners: &[&'a str]) -> BTreeSet<&'a str> {
    runners.iter().copied().filter(|r| !r.is_empty()).collect()
}

/// Why a reader passes an entry over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Passed {
    /// It breaks the trust rules (R21, R22): another domain's row among a scope's entries, a record
    /// that says it was built on what's below its domain, one that isn't read. Never ordinary: the
    /// ledger audits it (AC06).
    Breach(String),
    /// Sound, but not for this reader now: quarantined (R23), or verified by too few runners for a
    /// protected unit, which then runs cold.
    Unfit(String),
}

impl Passed {
    pub fn is_breach(&self) -> bool {
        matches!(self, Passed::Breach(_))
    }
}

impl std::fmt::Display for Passed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Passed::Breach(why) | Passed::Unfit(why) => f.write_str(why),
        }
    }
}

/// Whether a unit of `reader` takes an entry a lookup found (R21, R22, R23), or why not: the
/// domain whose entries hold it wrote it, by its own record, and the reader may read that domain;
/// its lineage fits that domain; it isn't quarantined; and for a protected reader, two independent
/// runners of its trusted or protected domain produced it identically.
pub fn admit(reader: &Domain, f: &Found) -> Result<(), Passed> {
    let writer = Domain::parse(f.writer).map_err(|e| Passed::Breach(format!("its writer: {e}")))?;
    if writer != *f.scope {
        return Err(Passed::Breach(format!("written by {writer} but among {}'s entries: never taken (W24-R21)", f.scope)));
    }
    if !reader.reads(f.scope) {
        return Err(Passed::Breach(format!("a {reader} unit doesn't read {} entries: reads go down only (W24-R21)", f.scope)));
    }
    lineage_fits(f.scope, f.lineage).map_err(Passed::Breach)?;
    if let Some(why) = f.quarantined {
        return Err(Passed::Unfit(format!("quarantined (W24-R23): {why}")));
    }
    if reader.needs_two_runners() {
        let n = independent(&f.runners).len();
        if n < 2 {
            return Err(Passed::Unfit(format!("a protected unit takes only what two independent trusted runners produced identically; this has {n}")));
        }
    }
    Ok(())
}

/// A result as two records of one strong key are compared: its outcome, exit code and outputs.
/// Logs and test reports carry times and aren't compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome<'a> {
    pub outcome: &'a str,
    pub exit: Option<i32>,
    /// Outputs by name: content digests or values.
    pub outputs: &'a BTreeMap<String, String>,
}

/// How a result recorded again under a strong key compares with the entry that holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "is")]
pub enum Comparison {
    /// The same outcome, exit code and outputs: a reproduction.
    Same,
    /// The same outcome and exit code with these outputs different (or present in one only):
    /// nondeterministic, or one of them corrupted (R23).
    Outputs { names: Vec<String> },
    /// Another outcome or exit code (R23: its lineage is quarantined).
    Outcome { kept: String, new: String },
}

/// How `new` compares with `kept`, the entry a strong key holds.
pub fn compare(kept: &Outcome, new: &Outcome) -> Comparison {
    if kept.outcome != new.outcome || kept.exit != new.exit {
        let show = |o: &Outcome| match o.exit {
            Some(code) => format!("{} (exit {code})", o.outcome),
            None => o.outcome.to_owned(),
        };
        return Comparison::Outcome { kept: show(kept), new: show(new) };
    }
    let names: Vec<String> = kept.outputs.keys().chain(new.outputs.keys()).collect::<BTreeSet<_>>().into_iter().filter(|k| kept.outputs.get(*k) != new.outputs.get(*k)).cloned().collect();
    if names.is_empty() { Comparison::Same } else { Comparison::Outputs { names } }
}

/// What an entry holds and saves, for eviction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub domain: Domain,
    /// Its bytes: the record's and its outputs' in the content store.
    pub bytes: u64,
    /// The time a hit saves: how long its unit took.
    pub saved_ms: u64,
    /// The hits it has had.
    pub hits: u64,
    /// When it was last used or written, in seconds.
    pub last_used: i64,
}

/// Each domain's quota in bytes; `person` is each person's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quotas {
    pub trusted: u64,
    pub protected: u64,
    pub untrusted: u64,
    pub person: u64,
}

impl Quotas {
    /// `domain`'s quota.
    pub fn of(&self, domain: &Domain) -> u64 {
        match domain {
            Domain::Trusted => self.trusted,
            Domain::Protected => self.protected,
            Domain::Untrusted => self.untrusted,
            Domain::Person(_) => self.person,
        }
    }
}

/// Which entries go under quota pressure (R21, AC12), as indices into `entries`, ascending. Each
/// domain is weighed alone: one over its quota loses the entries that save the least time per
/// byte first (the least recently used first among equals) until what's left fits, and one within
/// its quota loses nothing, whatever the others hold.
pub fn evict(entries: &[Usage], quotas: &Quotas) -> Vec<usize> {
    let mut by_domain: BTreeMap<&Domain, Vec<usize>> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        by_domain.entry(&e.domain).or_default().push(i);
    }
    let mut out = Vec::new();
    for (domain, mut idx) in by_domain {
        let quota = quotas.of(domain);
        let mut held: u128 = idx.iter().filter_map(|i| entries.get(*i)).map(|e| u128::from(e.bytes)).sum();
        if held <= u128::from(quota) {
            continue;
        }
        // Time saved per byte, as a fraction compared crosswise: saved × (hits + 1) over bytes.
        let worth = |i: &usize| entries.get(*i).map_or((0, 1), |e| (u128::from(e.saved_ms) * (u128::from(e.hits) + 1), u128::from(e.bytes.max(1))));
        idx.sort_by(|a, b| {
            let ((wa, ba), (wb, bb)) = (worth(a), worth(b));
            (wa * bb).cmp(&(wb * ba)).then_with(|| entries.get(*a).map(|e| e.last_used).cmp(&entries.get(*b).map(|e| e.last_used))).then(a.cmp(b))
        });
        for i in idx {
            if held <= u128::from(quota) {
                break;
            }
            held -= entries.get(i).map_or(0, |e| u128::from(e.bytes));
            out.push(i);
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn all() -> Vec<Domain> {
        vec![Domain::Protected, Domain::Trusted, Domain::Untrusted, Domain::Person("ada".into()), Domain::Person("bo".into())]
    }

    #[test]
    fn w24_r21_domains_read_and_print() -> Result<(), String> {
        for d in all() {
            assert_eq!(Domain::parse(&d.to_string())?, d);
            let json = serde_json::to_string(&d).map_err(|e| e.to_string())?;
            assert_eq!(serde_json::from_str::<Domain>(&json).map_err(|e| e.to_string())?, d);
        }
        assert_eq!(serde_json::to_string(&Domain::Person("ada".into())).map_err(|e| e.to_string())?, "\"person:ada\"");
        for bad in ["", "Trusted", "person:", "person:a b", "person:a|b", "root"] {
            assert!(Domain::parse(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<Domain>("\"forks\"").is_err());
        assert_eq!((Domain::of_run(true, true), Domain::of_run(true, false), Domain::of_run(false, true)), (Domain::Protected, Domain::Trusted, Domain::Untrusted), "an untrusted run is never protected");
        Ok(())
    }

    /// W24-R21: reads go down only, and a lookup tries the most trusted domains first.
    #[test]
    fn w24_r21_reads_go_down_only() {
        let (p, t, u, ada, bo) = (Domain::Protected, Domain::Trusted, Domain::Untrusted, Domain::Person("ada".into()), Domain::Person("bo".into()));
        for reader in [&t, &u, &ada] {
            assert!(reader.may_read(&p) && reader.may_read(reader));
        }
        assert!(u.may_read(&t) && ada.may_read(&t), "untrusted and person scopes read trusted entries");
        assert!(!t.may_read(&u) && !t.may_read(&ada), "trusted never reads either");
        assert!(!p.may_read(&t) && p.reads(&t), "protected reads trusted only verified by two runners");
        assert!(!ada.may_read(&bo) && !ada.may_read(&u) && !u.may_read(&ada), "people and forks don't read each other's");
        assert_eq!(t.lookups(), [p.clone(), t.clone()]);
        assert_eq!(p.lookups(), [p.clone(), t.clone()]);
        assert_eq!(u.lookups(), [p.clone(), t.clone(), u.clone()]);
        assert_eq!(ada.lookups(), [p.clone(), t.clone(), ada.clone()]);
        for reader in all() {
            for d in reader.lookups() {
                assert!(reader.reads(&d), "{reader} looks up only what it reads: {d}");
            }
            assert!(!reader.lookups().contains(&u) || reader == u);
        }
    }

    /// W24-R21: a unit writes only its own domain's entries, and only results whose lineage its
    /// domain reads: nothing is promoted by copying or by building on what's below.
    #[test]
    fn w24_r21_writes_stay_home() {
        for a in all() {
            for b in all() {
                assert_eq!(a.may_write(&b), a == b, "{a} into {b}");
            }
        }
        let l = |ds: &[&str]| ds.iter().map(|d| (*d).to_owned()).collect::<Vec<_>>();
        assert!(lineage_fits(&Domain::Trusted, &l(&["trusted", "protected"])).is_ok());
        assert!(lineage_fits(&Domain::Trusted, &l(&["trusted", "untrusted"])).is_err_and(|e| e.contains("untrusted")));
        assert!(lineage_fits(&Domain::Trusted, &l(&["person:ada"])).is_err());
        assert!(lineage_fits(&Domain::Protected, &l(&["trusted"])).is_ok(), "read verified");
        assert!(lineage_fits(&Domain::Person("ada".into()), &l(&["trusted", "person:ada"])).is_ok());
        assert!(lineage_fits(&Domain::Person("ada".into()), &l(&["person:bo"])).is_err());
        assert!(lineage_fits(&Domain::Untrusted, &l(&["nonsense"])).is_err());
    }

    fn found<'a>(scope: &'a Domain, writer: &'a str, lineage: &'a [String], runners: Vec<&'a str>) -> Found<'a> {
        Found { scope, writer, lineage, runners, quarantined: None }
    }

    /// W24-R21, R22, R23: what a reader takes, and why it passes the rest over: breaches of the
    /// trust rules apart from entries merely unfit for it.
    #[test]
    fn w24_r22_admit() {
        let (p, t, u, ada) = (Domain::Protected, Domain::Trusted, Domain::Untrusted, Domain::Person("ada".into()));
        let none: Vec<String> = Vec::new();
        let breach = |r: Result<(), Passed>, what: &str| r.is_err_and(|e| e.is_breach() && e.to_string().contains(what));
        let unfit = |r: Result<(), Passed>, what: &str| r.is_err_and(|e| !e.is_breach() && e.to_string().contains(what));
        assert!(admit(&t, &found(&t, "trusted", &none, vec!["r1"])).is_ok());
        assert!(admit(&u, &found(&t, "trusted", &none, vec!["r1"])).is_ok());
        assert!(admit(&ada, &found(&ada, "person:ada", &none, vec!["laptop"])).is_ok());
        assert!(breach(admit(&t, &found(&u, "untrusted", &none, vec!["r1"])), "doesn't read"));
        assert!(breach(admit(&t, &found(&t, "untrusted", &none, vec!["r1"])), "never taken"), "an untrusted row among trusted entries");
        assert!(breach(admit(&t, &found(&t, "garbage", &none, vec!["r1"])), "its writer"));
        let tainted = vec!["untrusted".to_owned()];
        assert!(breach(admit(&t, &found(&t, "trusted", &tainted, vec!["r1"])), "reads go down"));
        let q = Found { quarantined: Some("its outcome differed on r2"), ..found(&t, "trusted", &none, vec!["r1"]) };
        assert!(unfit(admit(&t, &q), "quarantined"));
        // Protected: two independent runners, or it runs cold.
        assert!(unfit(admit(&p, &found(&t, "trusted", &none, vec!["r1"])), "has 1"));
        assert!(admit(&p, &found(&t, "trusted", &none, vec!["r1", "r1", ""])).is_err(), "the same runner twice is one");
        assert!(admit(&p, &found(&t, "trusted", &none, vec!["r1", "r2"])).is_ok());
        assert!(admit(&p, &found(&p, "protected", &none, vec!["r1", "r2"])).is_ok());
        assert!(breach(admit(&p, &found(&u, "untrusted", &none, vec!["r1", "r2", "r3"])), "reads go down"), "never from below, however many");
    }

    #[test]
    fn w24_r22_compare() {
        fn o<'a>(outcome: &'a str, exit: Option<i32>, outputs: &'a BTreeMap<String, String>) -> Outcome<'a> {
            Outcome { outcome, exit, outputs }
        }
        let a = BTreeMap::from([("bin".to_owned(), "sha256:1".to_owned()), ("report".to_owned(), "ok".to_owned())]);
        let mut b = a.clone();
        assert_eq!(compare(&o("success", Some(0), &a), &o("success", Some(0), &b)), Comparison::Same);
        b.insert("bin".into(), "sha256:2".into());
        b.insert("extra".into(), "x".into());
        assert_eq!(compare(&o("success", Some(0), &a), &o("success", Some(0), &b)), Comparison::Outputs { names: vec!["bin".into(), "extra".into()] });
        assert_eq!(compare(&o("success", Some(0), &a), &o("failure", Some(1), &a)), Comparison::Outcome { kept: "success (exit 0)".into(), new: "failure (exit 1)".into() });
        assert!(matches!(compare(&o("success", Some(0), &a), &o("success", None, &a)), Comparison::Outcome { .. }));
    }

    /// W24-AC06, at the level of decisions: a fork's entry with a poisoned output never satisfies
    /// a trusted or protected lookup, however it's placed or what it claims.
    #[test]
    fn w24_ac06_a_forks_poisoned_entry_never_satisfies_a_trusted_lookup() {
        let (p, t, u) = (Domain::Protected, Domain::Trusted, Domain::Untrusted);
        let none: Vec<String> = Vec::new();
        for reader in [&t, &p] {
            // Its own entries: never looked up.
            assert!(!reader.lookups().contains(&u));
            assert!(admit(reader, &found(&u, "untrusted", &none, vec!["fork-runner", "r2"])).is_err());
            // Copied among trusted entries, saying who wrote it.
            assert!(admit(reader, &found(&t, "untrusted", &none, vec!["fork-runner", "r2"])).is_err());
            // Claiming to be trusted, but built on the fork's output.
            assert!(admit(reader, &found(&t, "trusted", &["untrusted".to_owned()], vec!["r1", "r2"])).is_err());
        }
        // Recorded by the fork into trusted entries, or as a trusted result built on its own.
        assert!(!u.may_write(&t) && !u.may_write(&p));
        assert!(lineage_fits(&t, &["untrusted".to_owned()]).is_err());
    }

    fn usage() -> impl Strategy<Value = Usage> {
        let domain = prop_oneof![Just(Domain::Trusted), Just(Domain::Protected), Just(Domain::Untrusted), Just(Domain::Person("ada".into())), Just(Domain::Person("bo".into()))];
        (domain, 0u64..10_000, 0u64..100_000, 0u64..20, 0i64..1_000).prop_map(|(domain, bytes, saved_ms, hits, last_used)| Usage { domain, bytes, saved_ms, hits, last_used })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// W24-R21, AC12: eviction weighs each domain alone. A domain within its quota loses
        /// nothing however full the others are; one over it ends within it, losing only its own
        /// entries, and never keeps one that saves less per byte than one it lost.
        #[test]
        fn w24_r21_quotas_keep_each_domain_to_itself(entries in proptest::collection::vec(usage(), 0..60), q in (0u64..60_000, 0u64..60_000, 0u64..60_000, 0u64..60_000)) {
            let quotas = Quotas { trusted: q.0, protected: q.1, untrusted: q.2, person: q.3 };
            let gone = evict(&entries, &quotas);
            let gone_set: BTreeSet<usize> = gone.iter().copied().collect();
            prop_assert_eq!(gone_set.len(), gone.len());
            let domains: BTreeSet<&Domain> = entries.iter().map(|e| &e.domain).collect();
            for d in domains {
                let mine: Vec<usize> = (0..entries.len()).filter(|i| entries.get(*i).is_some_and(|e| &e.domain == d)).collect();
                let bytes = |set: &mut dyn Iterator<Item = &usize>| -> u64 { set.filter_map(|i| entries.get(*i)).map(|e| e.bytes).sum() };
                let held = bytes(&mut mine.iter());
                let kept = bytes(&mut mine.iter().filter(|i| !gone_set.contains(i)));
                if held <= quotas.of(d) {
                    prop_assert!(mine.iter().all(|i| !gone_set.contains(i)), "{d} within its quota lost an entry");
                } else {
                    prop_assert!(kept <= quotas.of(d), "{d} still over its quota");
                }
                let worth = |i: usize| entries.get(i).map_or(0.0, |e| e.saved_ms as f64 * (e.hits as f64 + 1.0) / e.bytes.max(1) as f64);
                let lost_best = mine.iter().filter(|i| gone_set.contains(i)).map(|i| worth(*i)).fold(f64::MIN, f64::max);
                let kept_worst = mine.iter().filter(|i| !gone_set.contains(i)).map(|i| worth(*i)).fold(f64::MAX, f64::min);
                prop_assert!(lost_best <= kept_worst + 1e-9, "{d}: lost one worth {lost_best} but kept one worth {kept_worst}");
            }
        }
    }
}
