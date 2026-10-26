//! docs/ci.md W28-R50 to R52 and W28-AC50: verdicts on small lanes, and 10,000 random lanes whose
//! verdicts are worked out afresh.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::Range;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::*;

fn only(units: &[&str]) -> Affects {
    Affects::Units(units.iter().map(|u| (*u).to_owned()).collect())
}

fn commit(id: &str, at: i64, affects: Affects) -> Commit {
    Commit { id: id.into(), at, affects }
}

fn test(unit: &str, name: &str) -> Check {
    Check { workflow: "ci".into(), unit: unit.into(), test: name.into() }
}

fn spec(check: &Check, needs: &[&Check]) -> Spec {
    Spec { check: check.clone(), needs: needs.iter().map(|c| (*c).clone()).collect() }
}

fn ran(commit: usize, check: &Check, outcome: Outcome, at: i64) -> Seen {
    Seen { commit, check: check.clone(), outcome, how: How::Ran, at }
}

fn failed(signature: &str) -> Outcome {
    Outcome::Failed { signature: signature.into() }
}

fn check_of<'a>(v: &'a [Verdict], c: usize, check: &Check) -> Result<&'a CheckVerdict, String> {
    v.get(c).and_then(|x| x.checks.iter().find(|y| &y.check == check)).ok_or_else(|| format!("no verdict for {check} at commit {c}"))
}

fn kinds(v: &[Verdict]) -> Vec<Kind> {
    v.iter().map(|x| x.kind).collect()
}

/// W28-R52, the goal's test: the base fails test T; c1..c5 are pushed; c5's run fails only T.
/// Every commit is covered, and none is suspect: T already failed at the base.
#[test]
fn w28_r52_a_failure_already_at_the_base_makes_no_suspect() -> Result<(), String> {
    let (t, u) = (test("p", "t"), test("p", "u"));
    let lane = Lane { commits: (0..6).map(|i| commit(&format!("c{i}"), 10 * i, Affects::All)).collect(), checks: vec![spec(&t, &[]), spec(&u, &[])] };
    let seen = [ran(0, &t, failed("t"), 5), ran(0, &u, Outcome::Passed, 5), ran(5, &t, failed("t"), 60), ran(5, &u, Outcome::Passed, 60)];
    let v = verdicts(&lane, &seen, &BTreeMap::new(), &[]);
    assert_eq!(kinds(&v), [Kind::Direct, Kind::Covered, Kind::Covered, Kind::Covered, Kind::Covered, Kind::Direct]);
    for c in 1..5 {
        let x = check_of(&v, c, &t)?;
        assert!(x.status == Status::Covered && x.failing && x.optimistic && x.by == Some(5) && x.base == Some(0), "{x:?}");
        assert!(x.why.contains("as it already did at the base, c0"), "{}", x.why);
    }
    let at5 = check_of(&v, 5, &t)?;
    assert!(at5.status == Status::Direct && at5.failing && at5.why.contains("as it already did at the base, c0"), "{at5:?}");
    assert!(v.iter().all(|x| x.checks.iter().all(|y| y.status != Status::Suspect)));
    let mut book = Book::new(lane);
    for s in seen {
        book.see(s);
    }
    assert_eq!(book.failures(), []);
    Ok(())
}

/// The lane of W28-R52's second case: c0 the base, where everything passed; c1, c3 and c5 change
/// unit p, c2 and c4 only unit q; c5's run newly fails p's test and passes q's.
fn p_and_q() -> (Lane, Vec<Seen>, Check, Check) {
    let (tp, tq) = (test("p", "tp"), test("q", "tq"));
    let affects = [Affects::All, only(&["p"]), only(&["q"]), only(&["p"]), only(&["q"]), only(&["p"])];
    let commits = affects.into_iter().enumerate().map(|(i, a)| commit(&format!("c{i}"), 10 * i64::try_from(i).unwrap_or(0), a)).collect();
    let lane = Lane { commits, checks: vec![spec(&tp, &[]), spec(&tq, &[])] };
    let seen = vec![ran(0, &tp, Outcome::Passed, 5), ran(0, &tq, Outcome::Passed, 5), ran(5, &tp, failed("tp"), 70), ran(5, &tq, Outcome::Passed, 70)];
    (lane, seen, tp, tq)
}

/// W28-R52, the goal's test: a new failure names exactly the commits whose diffs could affect its
/// unit. c5 newly fails p's test: c1, c3 and c5 are its suspects; p is unaffected by c2 and c4,
/// which q's pass at c5 covers.
#[test]
fn w28_r52_a_new_failure_names_the_commits_that_could_affect_its_unit() -> Result<(), String> {
    let (lane, seen, tp, tq) = p_and_q();
    let mut book = Book::new(lane.clone());
    for s in &seen {
        book.see(s.clone());
    }
    assert_eq!(book.failures(), [Failure { check: tp.clone(), commit: 5, base: Some(0), suspects: vec![1, 3, 5] }]);
    let v = book.verdicts();
    assert_eq!(kinds(&v), [Kind::Direct, Kind::Suspect, Kind::Covered, Kind::Suspect, Kind::Covered, Kind::Suspect]);
    for c in [1, 3, 5] {
        let x = check_of(&v, c, &tp)?;
        assert!(x.status == Status::Suspect && x.by == Some(5) && x.base == Some(0), "{x:?}");
        assert!(x.why.contains("newly failed at c5 (it passed at the base, c0)"), "{}", x.why);
    }
    for c in [2, 4] {
        assert_eq!(check_of(&v, c, &tp)?.status, Status::Unaffected);
        assert_eq!(check_of(&v, c, &tq)?.status, Status::Covered);
    }
    assert!(v.get(1).is_some_and(|x| x.why == "suspect: ci: tp (p) newly failed at c5, and its diff could have done it"), "{:?}", v.get(1).map(|x| &x.why));
    Ok(())
}

/// W28-R52: a culprit finding's outcome. Naming c3 clears c1 and leaves c5 failing because of c3;
/// no culprit (a flake) clears every suspect; an inconclusive one leaves them suspects, saying why.
#[test]
fn w28_r52_a_culprit_finding_names_one_and_clears_the_rest() -> Result<(), String> {
    let (lane, seen, tp, _) = p_and_q();
    let with = |finding: Finding| verdicts(&lane, &seen, &BTreeMap::new(), &[Found { check: tp.clone(), suspects: vec![1, 3, 5], finding, at: 200 }]);

    let v = with(Finding::Culprit { commit: 3 });
    assert_eq!(kinds(&v), [Kind::Direct, Kind::Covered, Kind::Covered, Kind::Culprit, Kind::Covered, Kind::Direct]);
    let (c1, c3, c5) = (check_of(&v, 1, &tp)?, check_of(&v, 3, &tp)?, check_of(&v, 5, &tp)?);
    assert!(c1.status == Status::Cleared && !c1.failing && c1.optimistic && c1.at == Some(200), "{c1:?}");
    assert!(c3.status == Status::Culprit && c3.failing && c3.at == Some(200) && c3.why.starts_with("culprit:"), "{c3:?}");
    assert!(c5.status == Status::Direct && c5.failing && c5.why.contains("after the culprit c3"), "{c5:?}");
    assert_eq!(v.get(3).map(|x| x.at), Some(Some(200)));

    let v = with(Finding::NoCulprit);
    assert_eq!(kinds(&v), [Kind::Direct, Kind::Covered, Kind::Covered, Kind::Covered, Kind::Covered, Kind::Direct]);
    assert!(check_of(&v, 1, &tp)?.why.contains("a flake"));

    let v = with(Finding::Inconclusive { why: "c3 couldn't be built".into() });
    assert_eq!(kinds(&v), [Kind::Direct, Kind::Suspect, Kind::Covered, Kind::Suspect, Kind::Covered, Kind::Suspect]);
    assert!(check_of(&v, 3, &tp)?.why.ends_with("the culprit finding couldn't tell: c3 couldn't be built"));
    Ok(())
}

/// W28-R51: covered is a kind of its own, optimistic, and never shown as passed: a covered check
/// carries no outcome, as JSON too; a direct one says what its own result was.
#[test]
fn w28_r51_covered_is_optimistic_never_passed() -> Result<(), String> {
    let t = test("p", "t");
    let lane = Lane { commits: vec![commit("c0", 0, Affects::All), commit("c1", 10, Affects::All)], checks: vec![spec(&t, &[])] };
    let v = verdicts(&lane, &[ran(1, &t, Outcome::Passed, 20)], &BTreeMap::new(), &[]);
    let covered = check_of(&v, 0, &t)?;
    assert!(covered.status == Status::Covered && covered.optimistic && covered.outcome.is_none() && !covered.failing, "{covered:?}");
    let json = serde_json::to_value(covered).map_err(|e| e.to_string())?;
    assert_eq!(json.get("status"), Some(&serde_json::json!("covered")));
    assert_eq!(json.get("optimistic"), Some(&serde_json::json!(true)));
    assert_eq!(json.get("outcome"), None, "{json}");
    let direct = serde_json::to_value(check_of(&v, 1, &t)?).map_err(|e| e.to_string())?;
    assert_eq!(direct.pointer("/outcome/outcome"), Some(&serde_json::json!("passed")), "{direct}");
    assert_eq!(v.first().map(|x| (x.kind, x.at)), Some((Kind::Covered, Some(20))));
    Ok(())
}

/// W28-R50: a check whose need fails at a commit is settled with that need, there and where a
/// failure covers it; a commit whose diff affects none of the lane's checks is direct at once.
#[test]
fn w28_r50_needs_settle_and_unaffected_checks_never_count() -> Result<(), String> {
    let (rust, binary) = (Check::unit("ci", "rust"), Check::unit("ci", "binary"));
    let lane = Lane {
        commits: vec![commit("c0", 0, Affects::All), commit("c1", 10, Affects::All), commit("c2", 20, Affects::All), commit("c3", 30, Affects::All), commit("docs", 40, only(&[]))],
        checks: vec![spec(&binary, &[&rust]), spec(&rust, &[])],
    };
    let seen = [
        ran(0, &rust, Outcome::Passed, 5),
        ran(0, &binary, Outcome::Passed, 6),
        ran(1, &rust, failed("test"), 15),
        ran(1, &binary, Outcome::Skipped, 15),
        ran(3, &rust, failed("test"), 35),
        ran(3, &binary, Outcome::Skipped, 35),
    ];
    let v = verdicts(&lane, &seen, &BTreeMap::new(), &[]);
    // c1 newly failed rust: its only suspect; binary, skipped there, is settled with it.
    assert_eq!(check_of(&v, 1, &rust)?.status, Status::Suspect);
    let b1 = check_of(&v, 1, &binary)?;
    assert!(b1.status == Status::Suspect && b1.why.starts_with("settled with ci: rust, which it needs"), "{b1:?}");
    // c2: rust failing as at its base c1, by c3's result; binary settled with it.
    assert_eq!(check_of(&v, 2, &rust)?.status, Status::Covered);
    assert_eq!(check_of(&v, 2, &binary)?.status, Status::Covered);
    // c3: rust failed as at the base; binary, skipped, settled: direct.
    let b3 = check_of(&v, 3, &binary)?;
    assert!(b3.status == Status::Direct && b3.outcome == Some(Outcome::Skipped), "{b3:?}");
    // A commit whose diff affects none of the checks: direct at once, when it was pushed.
    let docs = v.get(4).ok_or("no verdict for the docs commit")?;
    assert_eq!((docs.kind, docs.at, docs.why.as_str()), (Kind::Direct, Some(40), "direct: its diff affects none of the lane's checks"));
    assert!(docs.checks.iter().all(|x| x.status == Status::Unaffected));
    Ok(())
}

/// W28-R52: W16's marks: a failure of a test marked flaky or quarantined doesn't count. It covers
/// as a failure already at the base does, and makes no suspect.
#[test]
fn w28_r52_marked_tests_failures_dont_count() -> Result<(), String> {
    let t = test("p", "t");
    let lane = Lane { commits: (0..3).map(|i| commit(&format!("c{i}"), i, Affects::All)).collect(), checks: vec![spec(&t, &[])] };
    let seen = [ran(0, &t, Outcome::Passed, 1), ran(2, &t, failed("t"), 9)];
    let marks = BTreeMap::from([(t.clone(), "probably flaky (it flipped in 7 of its last 30 runs)".to_owned())]);
    let v = verdicts(&lane, &seen, &marks, &[]);
    assert_eq!(kinds(&v), [Kind::Direct, Kind::Covered, Kind::Direct]);
    assert!(check_of(&v, 1, &t)?.why.contains("W16 marks it probably flaky"));
    let mut book = Book::new(lane);
    book.mark(t.clone(), "flaky".into());
    for s in seen {
        book.see(s);
    }
    assert_eq!(book.failure(&t, 2), None);
    Ok(())
}

// --------------------------------------------------------------- W28-AC50: random lanes

const UNITS: [&str; 3] = ["u0", "u1", "u2"];
const TESTS: [&str; 3] = ["", "t0", "t1"];

#[derive(Debug, Clone)]
struct RawCheck {
    unit: usize,
    test: usize,
    /// A check before it that it needs.
    need: Option<usize>,
}

#[derive(Debug, Clone)]
struct RawSeen {
    commit: usize,
    check: usize,
    /// 0-4 passed, 5-6 failed with signature a, 7 with b, 8-9 skipped.
    outcome: u8,
    how: u8,
    at: u8,
}

#[derive(Debug, Clone)]
struct RawFound {
    check: usize,
    suspects: u8,
    /// 0-1 a culprit, 2 no culprit, 3 inconclusive.
    finding: u8,
    culprit: usize,
    at: u8,
}

#[derive(Debug, Clone)]
struct Raw {
    /// Each commit's units, as a mask; none: every unit.
    commits: Vec<Option<u8>>,
    checks: Vec<RawCheck>,
    seen: Vec<RawSeen>,
    marks: u8,
    found: Vec<RawFound>,
}

fn raw() -> impl Strategy<Value = Raw> {
    let commits = prop::collection::vec(prop::option::weighted(0.85, 0u8..8), 1..=8);
    let check = (0..UNITS.len(), 0..TESTS.len(), prop::option::weighted(0.3, any::<usize>())).prop_map(|(unit, test, need)| RawCheck { unit, test, need });
    let seen = (any::<usize>(), any::<usize>(), 0u8..10, 0u8..3, 0u8..60).prop_map(|(commit, check, outcome, how, at)| RawSeen { commit, check, outcome, how, at });
    let found = (any::<usize>(), any::<u8>(), 0u8..4, any::<usize>(), 0u8..90).prop_map(|(check, suspects, finding, culprit, at)| RawFound { check, suspects, finding, culprit, at });
    let marks = prop_oneof![3 => Just(0u8), 1 => any::<u8>()];
    (commits, prop::collection::vec(check, 1..=4), prop::collection::vec(seen, 0..=14), marks, prop::collection::vec(found, 0..=2)).prop_map(|(commits, checks, seen, marks, found)| Raw { commits, checks, seen, marks, found })
}

/// A random lane, its results, marks and findings.
struct Case {
    lane: Lane,
    seen: Vec<Seen>,
    marks: BTreeMap<Check, String>,
    found: Vec<Found>,
}

fn build(raw: &Raw) -> Case {
    let commits: Vec<Commit> = raw
        .commits
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let affects = m.map_or(Affects::All, |m| Affects::Units(UNITS.iter().enumerate().filter(|(k, _)| (m >> k) & 1 == 1).map(|(_, u)| (*u).to_owned()).collect()));
            commit(&format!("c{i}"), 10 * i64::try_from(i).unwrap_or(0), affects)
        })
        .collect();
    let mut checks: Vec<Spec> = Vec::new();
    for r in &raw.checks {
        let check = test(UNITS.get(r.unit).copied().unwrap_or("u0"), TESTS.get(r.test).copied().unwrap_or(""));
        if checks.iter().any(|s| s.check == check) {
            continue;
        }
        let needs = r.need.filter(|_| !checks.is_empty()).and_then(|n| checks.get(n % checks.len())).map(|s| vec![s.check.clone()]).unwrap_or_default();
        checks.push(Spec { check, needs });
    }
    let n = commits.len();
    let pick = |i: usize| checks.get(i % checks.len().max(1)).map(|s| s.check.clone()).unwrap_or_default();
    let seen = raw
        .seen
        .iter()
        .map(|r| {
            let outcome = match r.outcome {
                0..=4 => Outcome::Passed,
                5 | 6 => failed("a"),
                7 => failed("b"),
                _ => Outcome::Skipped,
            };
            let how = match r.how {
                0 => How::Ran,
                1 => How::Reused,
                _ => How::Adopted,
            };
            Seen { commit: r.commit % n, check: pick(r.check), outcome, how, at: i64::from(r.at) }
        })
        .collect();
    let marks = checks.iter().enumerate().filter(|(k, _)| (raw.marks >> k) & 1 == 1).map(|(_, s)| (s.check.clone(), "flaky".to_owned())).collect();
    let found = raw
        .found
        .iter()
        .map(|r| {
            let suspects: Vec<usize> = (0..n).filter(|c| (r.suspects >> c) & 1 == 1).collect();
            let finding = match r.finding {
                0 | 1 => Finding::Culprit { commit: suspects.get(r.culprit % suspects.len().max(1)).copied().unwrap_or(0) },
                2 => Finding::NoCulprit,
                _ => Finding::Inconclusive { why: "untestable".into() },
            };
            Found { check: pick(r.check), suspects, finding, at: 100 + i64::from(r.at) }
        })
        .collect();
    Case { lane: Lane { commits, checks }, seen, marks, found }
}

/// What the verdict rules give, worked out afresh with plain scans: each commit's checks' status
/// and whether each is taken to fail there.
fn afresh(case: &Case) -> Vec<BTreeMap<Check, (Status, bool)>> {
    let n = case.lane.commits.len();
    // The latest result for each check at each commit.
    let mut latest: BTreeMap<(Check, usize), &Seen> = BTreeMap::new();
    for s in &case.seen {
        let key = (s.check.clone(), s.commit);
        if latest.get(&key).is_none_or(|o| (s.at, &s.outcome, s.how) > (o.at, &o.outcome, o.how)) {
            latest.insert(key, s);
        }
    }
    let tested = |k: &Check, c: usize| latest.get(&(k.clone(), c)).filter(|s| s.outcome != Outcome::Skipped).map(|s| s.outcome.clone());
    let affects = |c: usize, k: &Check| case.lane.commits.get(c).is_some_and(|x| match &x.affects {
        Affects::All => true,
        Affects::Units(u) => u.contains(&k.unit),
    });
    let mut checks: Vec<Spec> = case.lane.checks.clone();
    for s in &case.seen {
        if !checks.iter().any(|x| x.check == s.check) {
            checks.push(Spec { check: s.check.clone(), needs: Vec::new() });
        }
    }
    let mut out = Vec::with_capacity(n);
    for c in 0..n {
        let mut at: BTreeMap<Check, (Status, bool)> = BTreeMap::new();
        // Needs come earlier in the list, so one pass in order decides each check after its needs.
        for spec in &checks {
            let k = &spec.check;
            let affected = affects(c, k);
            let marked = case.marks.contains_key(k);
            let base = (0..c).rev().find(|&b| tested(k, b).is_some());
            let same_as_base = |sig: &str| base.and_then(|b| tested(k, b)).is_some_and(|o| o == failed(sig));
            // A suspect of the failure at `d`, as the latest finding whose suspects include it says.
            let suspect = |own: bool| -> (Status, bool) {
                let f = case.found.iter().filter(|f| &f.check == k && f.suspects.contains(&c)).max_by(|a, b| (a.at, &a.finding, &a.suspects).cmp(&(b.at, &b.finding, &b.suspects)));
                let resolved = if own { Status::Direct } else { Status::Cleared };
                match f.map(|f| &f.finding) {
                    None | Some(Finding::Inconclusive { .. }) => (Status::Suspect, true),
                    Some(Finding::Culprit { commit }) if *commit == c => (Status::Culprit, true),
                    Some(Finding::Culprit { commit }) => (resolved, c > *commit),
                    Some(Finding::NoCulprit) => (resolved, own),
                }
            };
            let need_failing = spec.needs.iter().filter_map(|nk| at.get(nk)).find(|(_, failing)| *failing).copied();
            let mine = if affected { Status::Direct } else { Status::Unaffected };
            // A failure is new only where it passed, or failed otherwise, at a base there is.
            let new = |sig: &str| !(marked || same_as_base(sig) || base.is_none());
            let next = (c + 1..n).find_map(|d| tested(k, d));
            let newly = |o: &Option<Outcome>| matches!(o, Some(Outcome::Failed { signature }) if new(signature));
            let v = if let Some(o) = tested(k, c) {
                match o {
                    Outcome::Failed { signature } if new(&signature) && affected => suspect(true),
                    Outcome::Failed { .. } => (mine, true),
                    _ => (mine, false),
                }
            } else if affected && newly(&next) {
                suspect(false)
            } else if let Some((st, _)) = need_failing {
                (if !affected { Status::Unaffected } else if st == Status::Unaffected { Status::Direct } else { st }, true)
            } else if !affected {
                (Status::Unaffected, false)
            } else {
                match next {
                    None | Some(Outcome::Skipped) => (Status::Pending, false),
                    Some(Outcome::Passed) => (Status::Covered, false),
                    Some(Outcome::Failed { .. }) => (Status::Covered, true),
                }
            };
            at.insert(k.clone(), v);
        }
        out.push(at);
    }
    out
}

/// What the property test saw, across its cases.
#[derive(Default)]
struct Saw {
    cases: Cell<usize>,
    /// New failures, and their suspects.
    failures: Cell<usize>,
    suspects: Cell<usize>,
    covered: Cell<usize>,
    culprits: Cell<usize>,
    settled: Cell<usize>,
}

fn bump(c: &Cell<usize>, by: usize) {
    c.set(c.get() + by);
}

fn check(raw: &Raw, seen: &Saw) -> Result<(), TestCaseError> {
    bump(&seen.cases, 1);
    let case = build(raw);
    let n = case.lane.commits.len();
    let v = verdicts(&case.lane, &case.seen, &case.marks, &case.found);
    prop_assert_eq!(v.len(), n);

    // R50 to R52: each check's status and failing, as worked out afresh.
    let want = afresh(&case);
    for (c, (got, w)) in v.iter().zip(&want).enumerate() {
        let have: BTreeMap<Check, (Status, bool)> = got.checks.iter().map(|x| (x.check.clone(), (x.status, x.failing))).collect();
        prop_assert_eq!(&have, w, "commit {}: {:?}", c, got);
        // A commit's verdict is the weakest of its affected checks'.
        let kind = got.checks.iter().filter(|x| x.status != Status::Unaffected).map(|x| x.status.kind()).max().unwrap_or(Kind::Direct);
        prop_assert_eq!(got.kind, kind);
        prop_assert!(!got.why.is_empty() && got.checks.iter().all(|x| !x.why.is_empty()));
        for x in &got.checks {
            // R51: covered and cleared are optimistic and carry no outcome: never shown as passed.
            if matches!(x.status, Status::Covered | Status::Cleared) {
                prop_assert!(x.optimistic && x.outcome.is_none(), "{:?}", x);
                bump(&seen.covered, 1);
            }
            if x.need.is_some() {
                bump(&seen.settled, 1);
            }
            // R50: a suspect or culprit check is one its commit's diff could affect.
            if matches!(x.status, Status::Suspect | Status::Culprit) {
                prop_assert!(case.lane.commits.get(c).is_some_and(|k| k.affects.unit(&x.check.unit)), "{:?}", x);
            }
            if x.status == Status::Culprit {
                bump(&seen.culprits, 1);
            }
        }
    }

    // R52: each new failure's base didn't fail the same way, its check isn't marked, and its
    // suspects are exactly the commits from just after the base to it whose diffs could affect the
    // check's unit; without findings, they are the commits suspect for it.
    let mut book = Book::new(case.lane.clone());
    for (k, why) in &case.marks {
        book.mark(k.clone(), why.clone());
    }
    for s in &case.seen {
        book.see(s.clone());
    }
    for f in book.failures() {
        bump(&seen.failures, 1);
        bump(&seen.suspects, f.suspects.len());
        prop_assert!(!case.marks.contains_key(&f.check));
        let tested_at = |c: usize| case.seen.iter().filter(|s| s.check == f.check && s.commit == c).max_by(|a, b| (a.at, &a.outcome, a.how).cmp(&(b.at, &b.outcome, b.how))).filter(|s| s.outcome != Outcome::Skipped);
        let mine = tested_at(f.commit).map(|s| s.outcome.clone());
        let failed_there = matches!(mine, Some(Outcome::Failed { .. }));
        prop_assert!(failed_there, "the failure's commit has no failing result: {:?}", mine);
        let base = (0..f.commit).rev().find(|&b| tested_at(b).is_some());
        prop_assert_eq!(f.base, base);
        prop_assert!(base.and_then(tested_at).map(|s| s.outcome.clone()) != mine, "the base failed the same way");
        let could: Vec<usize> = (base.map_or(0, |b| b + 1)..=f.commit).filter(|&c| case.lane.commits.get(c).is_some_and(|k| k.affects.unit(&f.check.unit))).collect();
        prop_assert_eq!(&f.suspects, &could);
        if case.found.is_empty() {
            // Its own suspects: not those settled with a need's.
            let suspect: Vec<usize> = (0..n).filter(|&c| v.get(c).and_then(|x| x.checks.iter().find(|y| y.check == f.check)).is_some_and(|y| y.status == Status::Suspect && y.by == Some(f.commit) && y.need.is_none())).collect();
            prop_assert_eq!(&suspect, &could);
        }
    }

    // R52: a culprit finding over a new failure's suspects names one of them: the culprit. Those
    // before it are cleared; those after it fail because of it, the failing commit itself directly.
    for (i, f) in book.failures().into_iter().enumerate() {
        let Some(&x) = f.suspects.get((usize::from(raw.marks) + i) % f.suspects.len().max(1)) else { continue };
        let mut found = case.found.clone();
        found.push(Found { check: f.check.clone(), suspects: f.suspects.clone(), finding: Finding::Culprit { commit: x }, at: 1_000 });
        let named = Case { lane: case.lane.clone(), seen: case.seen.clone(), marks: case.marks.clone(), found };
        let got = verdicts(&named.lane, &named.seen, &named.marks, &named.found);
        let want = afresh(&named);
        for &c in &f.suspects {
            let expect = if c == x {
                (Status::Culprit, true)
            } else if c < x {
                (Status::Cleared, false)
            } else if c == f.commit {
                (Status::Direct, true)
            } else {
                (Status::Cleared, true)
            };
            let have = got.get(c).and_then(|v| v.checks.iter().find(|y| y.check == f.check)).map(|y| (y.status, y.failing));
            prop_assert_eq!(have, Some(expect), "suspect {} of {:?}, the culprit named {}", c, f, x);
            prop_assert_eq!(want.get(c).and_then(|w| w.get(&f.check)).copied(), Some(expect));
        }
        prop_assert!(got.get(x).is_some_and(|v| v.kind == Kind::Culprit));
        bump(&seen.culprits, 1);
    }

    // §0.3: the verdicts don't depend on the order the results and findings come in.
    let mut seen_back = case.seen.clone();
    seen_back.reverse();
    let mut found_back = case.found.clone();
    found_back.reverse();
    prop_assert_eq!(&verdicts(&case.lane, &seen_back, &case.marks, &found_back), &v);

    // A book's additions say every commit whose verdict they change: added one at a time, the
    // commits outside each range keep their verdicts.
    let mut book = Book::new(case.lane.clone());
    for (k, why) in &case.marks {
        book.mark(k.clone(), why.clone());
    }
    let mut kept: Vec<Option<Verdict>> = (0..n).map(|c| book.verdict(c)).collect();
    let mut step = |range: Range<usize>, book: &Book| -> Result<(), TestCaseError> {
        for c in 0..n {
            let now = book.verdict(c);
            if range.contains(&c) {
                if let Some(slot) = kept.get_mut(c) {
                    *slot = now;
                }
            } else {
                prop_assert_eq!(kept.get(c).cloned().flatten(), now, "commit {} changed outside {:?}", c, range);
            }
        }
        Ok(())
    };
    for s in &case.seen {
        let r = book.see(s.clone());
        step(r, &book)?;
    }
    for f in &case.found {
        let r = book.found(f.clone());
        step(r, &book)?;
    }
    prop_assert_eq!(&book.verdicts(), &v);
    Ok(())
}

/// W28-AC50: over 10,000 random lanes, each check's verdict is as worked out afresh; a failure
/// already at the base, or of a marked test, makes no suspect; a new failure names exactly the
/// commits whose diffs could affect its unit; covered is never shown as passed; the verdicts don't
/// depend on the order results come in; and a book's additions name every commit they change.
#[test]
fn w28_ac50_random_lanes() -> Result<(), String> {
    let seen = Saw::default();
    let mut runner = TestRunner::new(Config { cases: 10_000, failure_persistence: None, ..Config::default() });
    runner.run(&raw(), |r| check(&r, &seen)).map_err(|e| e.to_string())?;
    let (cases, failures, suspects, covered, culprits, settled) = (seen.cases.get(), seen.failures.get(), seen.suspects.get(), seen.covered.get(), seen.culprits.get(), seen.settled.get());
    println!("W28-AC50: {cases} cases; {failures} new failures with {suspects} suspects; {covered} checks covered or cleared; {culprits} culprits; {settled} settled with a need");
    assert!(cases >= 10_000, "{cases} cases run");
    // Every rule was exercised, many times over.
    for (what, count) in [("new failures", failures), ("suspects", suspects), ("covered or cleared checks", covered), ("culprits", culprits), ("checks settled with a need", settled)] {
        assert!(count >= 200, "only {count} {what}");
    }
    Ok(())
}
