//! The land queue's reuse (docs/ci.md W24-R20, W14-R08): what a land candidate, and each step of
//! its bisection, takes from the ledger instead of running. A candidate is the trunk's tip with a
//! batch of diffs applied. A package's inputs on it ([`Inputs`]) are its state on the trunk and the
//! batch's diffs that can affect it (W24-R17's graph):
//!
//! - a package no diff of the batch affects has the trunk's inputs, so a trusted result recorded on
//!   the trunk stands for it;
//! - a package some diffs affect has inputs of the batch's own; a bisection step that keeps every
//!   diff affecting it (a half of the batch) gives it the same inputs, so the steps share what the
//!   batch, or another step, recorded for it;
//! - a diff's own results, made as it was reviewed, are untrusted (W24-R21) and never reused: the
//!   candidate runs those packages itself, as trusted, and its results are the ones kept.
//!
//! Only passes are recorded, so a package that failed on a batch runs again on each step that has
//! the diffs affecting it. Pure: the queue gives each package's state on the trunk (a fingerprint of
//! its files and what it depends on, W24-R01), the diffs' changed files and the ledger's entries,
//! and carries out [`decide`]'s answer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::graph::{self, Ecosystem, Graph};

/// A diff of the batch, and the files it changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub diff: String,
    pub files: Vec<String>,
}

/// A package's inputs on a candidate: its state on the trunk and the candidate's diffs that can
/// affect it, in landing order. Two candidates that agree on both agree on its result.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Inputs {
    /// Its state on the trunk the candidate is built on: a fingerprint of its files and of what it
    /// depends on (W24-R01), which the trunk's own changes since an entry change.
    pub trunk: String,
    pub diffs: Vec<String>,
}

/// A package's result in the ledger: what it was made from, and who made it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub package: String,
    pub inputs: Inputs,
    /// Its trust domain (W24-R21): `trusted`, `protected`, `untrusted` or `person:<id>`.
    pub domain: String,
    /// The run (a trunk's, a candidate's, a diff's) that made it.
    pub source: String,
}

/// Whether a trusted reader (the land queue) may take an entry of `domain`: reads go down only, so
/// the trusted and protected domains' entries, never an untrusted diff's or a person's.
pub fn trusted(domain: &str) -> bool {
    matches!(domain, "trusted" | "protected")
}

/// What the candidate does for a package, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A trusted entry with the package's inputs stands for it.
    Reuse { source: String, why: String },
    Run { why: String },
}

impl Decision {
    pub fn runs(&self) -> bool {
        matches!(self, Decision::Run { .. })
    }
}

/// Each member package's inputs on a candidate of `changes`: its state on the trunk (`trunk`, by
/// package; absent when the trunk doesn't know it) and the diffs that can affect it.
pub fn inputs(g: &Graph, eco: Ecosystem, trunk: &BTreeMap<String, String>, changes: &[Change]) -> BTreeMap<String, Option<Inputs>> {
    let hits: Vec<(&str, std::collections::BTreeSet<String>)> = changes.iter().map(|c| (c.diff.as_str(), graph::affected(g, eco, &c.files))).collect();
    g.of(eco)
        .filter(|p| p.member)
        .map(|p| {
            let diffs: Vec<String> = hits.iter().filter(|(_, hit)| hit.contains(&p.name)).map(|(d, _)| (*d).to_owned()).collect();
            (p.name.clone(), trunk.get(&p.name).map(|t| Inputs { trunk: t.clone(), diffs }))
        })
        .collect()
}

/// What a candidate of `changes` (a batch, or a step of its bisection) does for each member package
/// of `eco` (W24-R20): it reuses a trusted entry with the package's inputs, the newest first in
/// `entries`' order, and runs the rest, saying why.
pub fn decide(g: &Graph, eco: Ecosystem, trunk: &BTreeMap<String, String>, changes: &[Change], entries: &[Entry]) -> BTreeMap<String, Decision> {
    inputs(g, eco, trunk, changes)
        .into_iter()
        .map(|(package, inputs)| {
            let d = match inputs {
                None => Decision::Run { why: "its state on the trunk isn't known".into() },
                Some(i) => {
                    let same: Vec<&Entry> = entries.iter().filter(|e| e.package == package && e.inputs == i).collect();
                    match same.iter().find(|e| trusted(&e.domain)) {
                        Some(e) => {
                            let why = if i.diffs.is_empty() { format!("no diff of the candidate affects it: as on the trunk, in {}", e.source) } else { format!("its inputs are as they were in {}: {} on the same trunk", e.source, affects(&i.diffs)) };
                            Decision::Reuse { source: e.source.clone(), why }
                        }
                        None => {
                            let untrusted = same.iter().find(|e| !trusted(&e.domain));
                            let why = match (untrusted, i.diffs.is_empty()) {
                                (Some(e), _) => format!("only {} has a result for its inputs, in the {} domain, which the queue never takes", e.source, e.domain),
                                (None, true) => "no trusted result is recorded for its state on the trunk".into(),
                                (None, false) => affects(&i.diffs),
                            };
                            Decision::Run { why }
                        }
                    }
                }
            };
            (package, d)
        })
        .collect()
}

/// The entries a candidate's run leaves for the steps after it: each package it ran and passed,
/// under its inputs on the candidate, trusted (the land queue's domain).
pub fn recorded(g: &Graph, eco: Ecosystem, trunk: &BTreeMap<String, String>, changes: &[Change], passed: &[String], source: &str) -> Vec<Entry> {
    inputs(g, eco, trunk, changes)
        .into_iter()
        .filter(|(p, _)| passed.contains(p))
        .filter_map(|(package, inputs)| Some(Entry { package, inputs: inputs?, domain: "trusted".into(), source: source.to_owned() }))
        .collect()
}

/// `D1 affects it`, `D1 and D2 affect it`.
fn affects(diffs: &[String]) -> String {
    match diffs {
        [one] => format!("{one} affects it"),
        [init @ .., last] => format!("{} and {last} affect it", init.join(", ")),
        [] => "nothing affects it".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Four crates: `b` depends on `a`; `c` and `d` stand alone.
    fn workspace() -> Graph {
        let files = BTreeMap::from([
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
            ("crates/b/Cargo.toml", "[package]\nname = \"b\"\n\n[dependencies]\na = { path = \"../a\" }\n"),
            ("crates/c/Cargo.toml", "[package]\nname = \"c\"\n"),
            ("crates/d/Cargo.toml", "[package]\nname = \"d\"\n"),
        ]);
        let paths: Vec<String> = files.keys().map(|p| (*p).to_owned()).collect();
        graph::read(&|p| files.get(p).map(|t| (*t).to_owned()), &paths)
    }

    fn trunk() -> BTreeMap<String, String> {
        ["a", "b", "c", "d"].iter().map(|p| ((*p).to_owned(), format!("{p}@t1"))).collect()
    }

    fn change(diff: &str, files: &[&str]) -> Change {
        Change { diff: diff.into(), files: files.iter().map(|f| (*f).to_owned()).collect() }
    }

    /// The trunk's trusted results, as its last run (or warm-up) recorded them.
    fn on_trunk() -> Vec<Entry> {
        trunk().into_iter().map(|(package, t)| Entry { package, inputs: Inputs { trunk: t, diffs: Vec::new() }, domain: "trusted".into(), source: "trunk-run".into() }).collect()
    }

    fn running(d: &BTreeMap<String, Decision>) -> Vec<&str> {
        d.iter().filter(|(_, x)| x.runs()).map(|(p, _)| p.as_str()).collect()
    }

    fn source<'d>(d: &'d BTreeMap<String, Decision>, p: &str) -> Option<&'d str> {
        match d.get(p) {
            Some(Decision::Reuse { source, .. }) => Some(source),
            _ => None,
        }
    }

    /// W24-R20: a candidate reuses the trunk's trusted entries for the packages its diff didn't
    /// touch, and runs those it did (and their dependents).
    #[test]
    fn w24_r20_a_candidate_reuses_trusted_entries_for_packages_its_diff_did_not_touch() {
        let g = workspace();
        let d = decide(&g, Ecosystem::Cargo, &trunk(), &[change("D1", &["crates/a/src/lib.rs"])], &on_trunk());
        assert_eq!(running(&d), ["a", "b"], "a, and b, which depends on it");
        assert_eq!((source(&d, "c"), source(&d, "d")), (Some("trunk-run"), Some("trunk-run")));
        assert_eq!(d.get("a"), Some(&Decision::Run { why: "D1 affects it".into() }));
        // The trunk moved under `d` since its entry: `d` runs too.
        let mut moved = trunk();
        moved.insert("d".into(), "d@t2".into());
        let d2 = decide(&g, Ecosystem::Cargo, &moved, &[change("D1", &["crates/a/src/lib.rs"])], &on_trunk());
        assert_eq!(running(&d2), ["a", "b", "d"]);
        assert_eq!(d2.get("d"), Some(&Decision::Run { why: "no trusted result is recorded for its state on the trunk".into() }));
    }

    /// W24-R20: a failing batch's bisection steps share results: the batch's passes stand for the
    /// steps that keep the diffs affecting them, and the trunk's for what no diff of a step affects.
    #[test]
    fn w24_r20_bisection_steps_share_results() {
        let (g, t) = (workspace(), trunk());
        let (d1, d2, d3) = (change("D1", &["crates/a/src/lib.rs"]), change("D2", &["crates/c/src/lib.rs"]), change("D3", &["crates/d/src/lib.rs"]));
        let batch = [d1.clone(), d2.clone(), d3.clone()];
        let mut ledger = on_trunk();
        let first = decide(&g, Ecosystem::Cargo, &t, &batch, &ledger);
        assert_eq!(running(&first), ["a", "b", "c", "d"]);
        // The batch fails on `c`: a, b and d passed, and are kept under the batch's inputs.
        let mut kept = recorded(&g, Ecosystem::Cargo, &t, &batch, &["a".into(), "b".into(), "d".into()], "batch");
        kept.extend(ledger);
        ledger = kept;
        // Its first half: D1 and D2. a and b have the batch's inputs (D1 alone affects them); d,
        // which only D3 touched, is the trunk's; c runs, its failure never kept.
        let half = [d1, d2.clone()];
        let step = decide(&g, Ecosystem::Cargo, &t, &half, &ledger);
        assert_eq!(running(&step), ["c"]);
        assert_eq!((source(&step, "a"), source(&step, "b"), source(&step, "d")), (Some("batch"), Some("batch"), Some("trunk-run")));
        // The step fails on c again; its quarter, D2 alone: only c runs.
        let quarter = [d2];
        let step = decide(&g, Ecosystem::Cargo, &t, &quarter, &ledger);
        assert_eq!(running(&step), ["c"]);
        assert!(["a", "b", "d"].iter().all(|p| source(&step, p) == Some("trunk-run")), "{step:?}");
        // The other half, D3 alone: d has the batch's inputs, and nothing runs.
        let other = [d3];
        let step = decide(&g, Ecosystem::Cargo, &t, &other, &ledger);
        assert_eq!(running(&step), Vec::<&str>::new());
        assert_eq!(source(&step, "d"), Some("batch"));
    }

    /// W24-R20, R21: a diff's untrusted results are never reused, though they have the
    /// candidate's very inputs; nor a person's.
    #[test]
    fn w24_r20_a_diffs_untrusted_results_are_never_reused() {
        let (g, t) = (workspace(), trunk());
        let batch = [change("D1", &["crates/c/src/lib.rs"])];
        let mut ledger = on_trunk();
        for (domain, source) in [("untrusted", "D1's run"), ("person:ada", "ada's build")] {
            ledger.insert(0, Entry { package: "c".into(), inputs: Inputs { trunk: "c@t1".into(), diffs: vec!["D1".into()] }, domain: domain.into(), source: source.into() });
        }
        let d = decide(&g, Ecosystem::Cargo, &t, &batch, &ledger);
        assert_eq!(d.get("c"), Some(&Decision::Run { why: "only ada's build has a result for its inputs, in the person:ada domain, which the queue never takes".into() }));
        // Once the candidate ran it, as trusted, its own result is kept and taken.
        ledger.extend(recorded(&g, Ecosystem::Cargo, &t, &batch, &["c".into()], "candidate"));
        assert_eq!(source(&decide(&g, Ecosystem::Cargo, &t, &batch, &ledger), "c"), Some("candidate"));
        assert!(trusted("protected") && !trusted("untrusted") && !trusted("person:ada"));
    }

    fn arb_entry() -> impl Strategy<Value = Entry> {
        let package = prop_oneof![Just("a"), Just("b"), Just("c"), Just("d")];
        let domain = prop_oneof![Just("trusted"), Just("protected"), Just("untrusted"), Just("person:ada")];
        let diffs = proptest::sample::subsequence(vec!["D1", "D2", "D3"], 0..=3);
        (package, domain, diffs, any::<bool>(), 0u8..4).prop_map(|(p, dom, diffs, moved, n)| Entry {
            package: p.into(),
            inputs: Inputs { trunk: if moved { format!("{p}@t0") } else { format!("{p}@t1") }, diffs: diffs.into_iter().map(str::to_owned).collect() },
            domain: dom.into(),
            source: format!("run{n}"),
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// Whatever the ledger holds, a candidate reuses only trusted entries with exactly the
        /// package's inputs, and runs every package with none.
        #[test]
        fn w24_r20_reuse_is_only_ever_of_trusted_entries_with_the_same_inputs(entries in proptest::collection::vec(arb_entry(), 0..24), which in proptest::sample::subsequence(vec![0usize, 1, 2], 0..=3)) {
            let g = workspace();
            let all = [change("D1", &["crates/a/src/lib.rs"]), change("D2", &["crates/c/x.rs"]), change("D3", &["crates/d/y.rs", "crates/c/z.rs"])];
            let changes: Vec<Change> = which.iter().filter_map(|i| all.get(*i).cloned()).collect();
            let wanted = inputs(&g, Ecosystem::Cargo, &trunk(), &changes);
            for (p, d) in decide(&g, Ecosystem::Cargo, &trunk(), &changes, &entries) {
                let i = wanted.get(&p).cloned().flatten();
                let usable = entries.iter().any(|e| e.package == p && Some(&e.inputs) == i.as_ref() && trusted(&e.domain));
                match d {
                    Decision::Reuse { source, .. } => prop_assert!(entries.iter().any(|e| e.package == p && e.source == source && Some(&e.inputs) == i.as_ref() && trusted(&e.domain))),
                    Decision::Run { .. } => prop_assert!(!usable, "{p} ran though a trusted entry had its inputs"),
                }
            }
        }
    }
}
