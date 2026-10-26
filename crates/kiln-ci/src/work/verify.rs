//! Verification (docs/ci.md W24-R23), pure: which hits kiln runs again to check them, what the
//! runs again say of the entry they checked, and what that reaches: the entry and everything built
//! on it.
//!
//! - **Sampling** ([`Hermeticity`], [`rate`], [`sampled`]): about 1 % of the hits of hermetic units
//!   (`hermetic: strict`), 5 % of sandboxed ones, 10 % of those on the host (`none`, the default),
//!   or the share the unit's `verify:` gives; each hit drawn from a hash of the seed the caller
//!   passes in (the server's, so nobody outside can tell or steer which), the entry and the hit.
//! - **Verdicts** ([`judge`]): an entry's result against every other run of its key (runs again,
//!   reproductions, results its key refused). The same outcome and outputs: verified. Another
//!   outcome: its lineage is quarantined. Other outputs: the unit is marked nondeterministic (shown,
//!   with early cutoff off for it: its outputs' digests don't stand for what it does, R03), and its
//!   lineage is quarantined too (AC07), since what was built on a result no run can be counted on
//!   to give again can't be told from what a fresh run would build. Runs that agree with each other
//!   and not with the entry mean the entry is what's wrong, corrupted, not the unit.
//! - **Lineage** ([`lineage`]): an entry and every entry that reused it, or reused one that did,
//!   through the results each one says it reused.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use sha2::{Digest as _, Sha256};

use super::key::Key;
use super::trust::{Comparison, Outcome, compare};

/// How hermetic a unit is (`work.hermetic`), which says how often its hits are checked (R23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Hermeticity {
    /// `strict`: in a sandbox with nothing but its declared inputs and no network.
    Strict,
    /// `sandboxed`: in a sandbox, with what its runner's tools reach.
    Sandboxed,
    /// `none`: on the host.
    Host,
}

/// Parts per million: the share of hits run again.
pub type Ppm = u32;

/// All of them.
pub const EVERY: Ppm = 1_000_000;

impl Hermeticity {
    /// `strict`, `sandboxed` or `none`.
    pub fn parse(s: &str) -> Result<Hermeticity, String> {
        match s.trim() {
            "strict" => Ok(Hermeticity::Strict),
            "sandboxed" => Ok(Hermeticity::Sandboxed),
            "none" => Ok(Hermeticity::Host),
            other => Err(format!("hermetic: `{other}` is none, sandboxed or strict")),
        }
    }

    /// R23's share of its hits run again: 1 %, 5 % or 10 %.
    pub fn rate(self) -> Ppm {
        match self {
            Hermeticity::Strict => 10_000,
            Hermeticity::Sandboxed => 50_000,
            Hermeticity::Host => 100_000,
        }
    }
}

/// A `verify:` share: `5%`, `0.5%`, `100%` or `0` (none), to a ten-thousandth of a percent.
pub fn parse_rate(s: &str) -> Result<Ppm, String> {
    let t = s.trim();
    let number = t.strip_suffix('%').unwrap_or(t).trim();
    let bad = || format!("verify: `{t}` is a share of hits, like 5% or 0.5%");
    if t == "0" {
        return Ok(0);
    }
    if !t.ends_with('%') || number.is_empty() {
        return Err(bad());
    }
    let (whole, frac) = number.split_once('.').unwrap_or((number, ""));
    if (whole.is_empty() && frac.is_empty()) || !whole.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) || frac.len() > 4 {
        return Err(bad());
    }
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| bad())? };
    let frac: u64 = format!("{frac:0<4}").parse().map_err(|_| bad())?;
    let ppm = whole.saturating_mul(10_000).saturating_add(frac);
    if ppm > u64::from(EVERY) {
        return Err(format!("verify: `{t}` is more than every hit"));
    }
    u32::try_from(ppm).map_err(|_| bad())
}

/// The share of a unit's hits run again: what its `verify:` says, else its hermeticity's.
pub fn rate(declared: Option<Ppm>, hermetic: Hermeticity) -> Ppm {
    declared.unwrap_or_else(|| hermetic.rate()).min(EVERY)
}

/// Whether a hit is run again to check it (R23): a draw below `rate` from the SHA-256 of `seed`,
/// the entry's strong key and `hit` (what took it: a run, leg and attempt), so the same hit always
/// draws the same and one who doesn't know the seed can't tell which hits are checked.
pub fn sampled(seed: &[u8], strong: &Key, hit: &str, rate: Ppm) -> bool {
    let mut h = Sha256::new();
    for part in [seed, strong.0.as_slice(), hit.as_bytes()] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part);
    }
    let d: [u8; 32] = h.finalize().into();
    let first: [u8; 8] = d.get(..8).and_then(|b| b.try_into().ok()).unwrap_or_default();
    // A draw in [0, 10⁶), unbiased to well within a part per million.
    u64::from_be_bytes(first) % u64::from(EVERY) < u64::from(rate)
}

/// What the other runs of an entry's key say of it (R23).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every run gave its outcome and outputs (or there's no other run).
    Verified,
    /// A run gave another outcome or exit code: its lineage is quarantined.
    Outcome { kept: String, other: String },
    /// Runs gave other outputs (these): the unit is marked nondeterministic, and its lineage is
    /// quarantined. One run that differs could also mean the entry is corrupted: a second that
    /// agrees with the first says so ([`Verdict::Corrupted`]).
    Nondeterministic { names: Vec<String> },
    /// Runs again agree with each other on other outputs (these), and none gave the entry's: the
    /// entry is corrupted (by its runner, or since), not the unit nondeterministic; its lineage is
    /// quarantined.
    Corrupted { names: Vec<String>, runs: usize },
}

impl Verdict {
    /// Whether the entry's lineage is quarantined.
    pub fn quarantines(&self) -> bool {
        !matches!(self, Verdict::Verified)
    }

    /// Whether the unit is marked nondeterministic.
    pub fn nondeterministic(&self) -> bool {
        matches!(self, Verdict::Nondeterministic { .. })
    }

    /// What it says, for the entry's verdict and the audit log.
    pub fn why(&self) -> String {
        match self {
            Verdict::Verified => "every run gave the same outcome and outputs".into(),
            Verdict::Outcome { kept, other } => format!("a run gave another outcome: {other}, where it gave {kept}"),
            Verdict::Nondeterministic { names } => format!("its runs give different outputs ({}): the unit is nondeterministic", names.join(", ")),
            Verdict::Corrupted { names, runs } => format!("{runs} runs agree on other outputs ({}) and none on its own: it's corrupted", names.join(", ")),
        }
    }
}

/// The outputs whose values differ between `a` and `b`, or that one of them lacks.
fn differing(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> BTreeSet<String> {
    a.keys().chain(b.keys()).filter(|k| a.get(*k) != b.get(*k)).cloned().collect()
}

/// What `runs` (every other run of an entry's key: runs again, reproductions and results its key
/// refused) say of `entry` (R23, AC07): see [`Verdict`].
pub fn judge(entry: &Outcome, runs: &[Outcome]) -> Verdict {
    let mut agree = 0usize;
    let mut other: Vec<&Outcome> = Vec::new();
    for run in runs {
        match compare(entry, run) {
            Comparison::Same => agree += 1,
            Comparison::Outcome { kept, new } => return Verdict::Outcome { kept, other: new },
            Comparison::Outputs { .. } => other.push(run),
        }
    }
    let Some(first) = other.first() else { return Verdict::Verified };
    let names = |set: BTreeSet<String>| set.into_iter().collect::<Vec<_>>();
    let against_entry: BTreeSet<String> = other.iter().flat_map(|r| differing(entry.outputs, r.outputs)).collect();
    let among_runs: BTreeSet<String> = other.iter().flat_map(|r| differing(first.outputs, r.outputs)).collect();
    if other.len() >= 2 && agree == 0 && among_runs.is_empty() {
        return Verdict::Corrupted { names: names(against_entry), runs: other.len() };
    }
    // Runs that differ: one from the entry, from each other, or one giving its outputs and one not.
    Verdict::Nondeterministic { names: names(against_entry.union(&among_runs).cloned().collect()) }
}

/// An entry's lineage (R23): `root` and every entry that reused it, or reused one that did, in the
/// order they're reached (`root` first, then nearer before farther, each by name). `reused` maps
/// each entry to the entries it says it reused.
pub fn lineage(root: &str, reused: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut users: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (entry, used) in reused {
        for u in used {
            users.entry(u.as_str()).or_default().insert(entry.as_str());
        }
    }
    let mut seen: BTreeSet<&str> = BTreeSet::from([root]);
    let mut out = vec![root.to_owned()];
    let mut queue: VecDeque<&str> = VecDeque::from([root]);
    while let Some(at) = queue.pop_front() {
        for user in users.get(at).into_iter().flatten() {
            if seen.insert(user) {
                out.push((*user).to_owned());
                queue.push_back(user);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn w24_r23_rates() -> Result<(), String> {
        assert_eq!([Hermeticity::Strict, Hermeticity::Sandboxed, Hermeticity::Host].map(Hermeticity::rate), [10_000, 50_000, 100_000], "1 %, 5 %, 10 %");
        assert_eq!(Hermeticity::parse("none")?, Hermeticity::Host);
        assert_eq!(Hermeticity::parse(" strict ")?, Hermeticity::Strict);
        assert!(Hermeticity::parse("airtight").is_err());
        assert_eq!(parse_rate("5%")?, 50_000);
        assert_eq!(parse_rate("0.5%")?, 5_000);
        assert_eq!(parse_rate(".25%")?, 2_500);
        assert_eq!(parse_rate("100%")?, EVERY);
        assert_eq!(parse_rate("0.0001%")?, 1);
        assert_eq!(parse_rate("0")?, 0);
        for bad in ["5", "%", "abc%", "101%", "1.23456%", "-1%", "1.2.3%", ""] {
            assert!(parse_rate(bad).is_err(), "{bad}");
        }
        assert_eq!(rate(None, Hermeticity::Sandboxed), 50_000);
        assert_eq!(rate(Some(2_000), Hermeticity::Host), 2_000, "the unit's own `verify:`");
        Ok(())
    }

    /// W24-R23: each hit draws the same every time, and about the rate of hits are drawn.
    #[test]
    fn w24_r23_sampling_draws_about_the_rate() {
        let strong = Key([5; 32]);
        for (rate, lo, hi) in [(10_000u32, 150, 250), (50_000, 900, 1_100), (100_000, 1_850, 2_150)] {
            let n = (0..20_000).filter(|i| sampled(b"seed", &strong, &format!("run{i}/leg/1"), rate)).count();
            assert!((lo..=hi).contains(&n), "{rate} ppm drew {n} of 20,000");
        }
        assert!(!sampled(b"seed", &strong, "run1/leg/1", 0) && sampled(b"seed", &strong, "run1/leg/1", EVERY));
        let a: Vec<bool> = (0..200).map(|i| sampled(b"seed", &strong, &format!("h{i}"), 100_000)).collect();
        let b: Vec<bool> = (0..200).map(|i| sampled(b"seed", &strong, &format!("h{i}"), 100_000)).collect();
        let other: Vec<bool> = (0..200).map(|i| sampled(b"another seed", &strong, &format!("h{i}"), 100_000)).collect();
        assert_eq!(a, b, "the same hit draws the same");
        assert_ne!(a, other, "another seed draws others");
    }

    fn o<'a>(outcome: &'a str, exit: Option<i32>, outputs: &'a BTreeMap<String, String>) -> Outcome<'a> {
        Outcome { outcome, exit, outputs }
    }

    fn out(v: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("bin".to_owned(), v.to_owned()), ("lib".to_owned(), "sha256:lib".to_owned())])
    }

    /// W24-R23, AC07: what runs again say of an entry.
    #[test]
    fn w24_r23_verdicts() {
        let (x, y, z) = (out("sha256:x"), out("sha256:y"), out("sha256:z"));
        let entry = o("success", Some(0), &x);
        assert_eq!(judge(&entry, &[]), Verdict::Verified);
        assert_eq!(judge(&entry, &[o("success", Some(0), &x), o("success", Some(0), &x)]), Verdict::Verified);
        let failed = judge(&entry, &[o("success", Some(0), &x), o("failure", Some(1), &x)]);
        assert_eq!(failed, Verdict::Outcome { kept: "success (exit 0)".into(), other: "failure (exit 1)".into() });
        assert!(failed.quarantines() && !failed.nondeterministic());
        // One run with other outputs: nondeterministic, as R23 has it, and quarantined.
        let one = judge(&entry, &[o("success", Some(0), &y)]);
        assert_eq!(one, Verdict::Nondeterministic { names: vec!["bin".into()] });
        assert!(one.quarantines() && one.nondeterministic());
        // A second that agrees with it, not with the entry: the entry is corrupted, the unit isn't
        // nondeterministic.
        let corrupted = judge(&entry, &[o("success", Some(0), &y), o("success", Some(0), &y)]);
        assert_eq!(corrupted, Verdict::Corrupted { names: vec!["bin".into()], runs: 2 });
        assert!(corrupted.quarantines() && !corrupted.nondeterministic() && corrupted.why().contains("corrupted"));
        // Two that disagree with each other too, or one that gives its outputs and one that doesn't: nondeterministic.
        let nd = judge(&entry, &[o("success", Some(0), &y), o("success", Some(0), &z)]);
        assert_eq!(nd, Verdict::Nondeterministic { names: vec!["bin".into()] });
        assert!(judge(&entry, &[o("success", Some(0), &x), o("success", Some(0), &y)]).nondeterministic());
        assert!(judge(&entry, &[o("success", Some(0), &x), o("success", Some(0), &y), o("success", Some(0), &y)]).nondeterministic(), "one run gave its outputs");
        // Outputs one run lacks count as differing.
        let fewer = BTreeMap::from([("bin".to_owned(), "sha256:x".to_owned())]);
        assert_eq!(judge(&entry, &[o("success", Some(0), &fewer)]), Verdict::Nondeterministic { names: vec!["lib".into()] });
    }

    #[test]
    fn w24_r23_lineage_reaches_what_was_built_on_it() {
        let reused = BTreeMap::from([
            ("b".to_owned(), vec!["a".to_owned()]),
            ("c".to_owned(), vec!["b".to_owned(), "x".to_owned()]),
            ("d".to_owned(), vec!["c".to_owned(), "a".to_owned()]),
            ("e".to_owned(), vec!["x".to_owned()]),
            ("a".to_owned(), vec!["d".to_owned()]),
        ]);
        assert_eq!(lineage("a", &reused), ["a", "b", "d", "c"], "a cycle is walked once");
        assert_eq!(lineage("x", &reused), ["x", "c", "e", "d", "a", "b"]);
        assert_eq!(lineage("e", &reused), ["e"]);
        assert_eq!(lineage("lone", &BTreeMap::new()), ["lone"]);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        /// W24-R23: a lineage holds its root, every entry that reused one in it, and nothing else.
        #[test]
        fn w24_r23_lineage_is_closed_and_least(edges in proptest::collection::btree_map(0u8..30, proptest::collection::vec(0u8..30, 0..4), 0..30), root in 0u8..30) {
            let name = |n: &u8| format!("e{n}");
            let reused: BTreeMap<String, Vec<String>> = edges.iter().map(|(k, v)| (name(k), v.iter().map(name).collect())).collect();
            let got = lineage(&name(&root), &reused);
            let set: BTreeSet<&String> = got.iter().collect();
            prop_assert_eq!(set.len(), got.len(), "each once");
            prop_assert_eq!(got.first(), Some(&name(&root)));
            for (entry, used) in &reused {
                if used.iter().any(|u| set.contains(u)) {
                    prop_assert!(set.contains(entry), "{} reused one in the lineage", entry);
                }
            }
            // Least: each but the root reused one reached before it.
            for (i, e) in got.iter().enumerate().skip(1) {
                let before: BTreeSet<&String> = got.iter().take(i).collect();
                prop_assert!(reused.get(e).is_some_and(|u| u.iter().any(|x| before.contains(x))), "{} isn't built on the lineage", e);
            }
        }
    }
}
