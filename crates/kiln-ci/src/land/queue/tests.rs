//! docs/ci.md W28-R100 to R109: the land queue by component, driven as the server drives it, and
//! Review's gate, pinned revisions, a stack's clear prefix and a host's queue.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::work::graph::Package;

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Two packages side by side, `a` and `b`, in folders of their own.
fn two() -> Units {
    Units::new([Unit { name: "a".into(), ecosystem: "npm".into(), dir: "a".into(), ..Unit::default() }, Unit { name: "b".into(), ecosystem: "npm".into(), dir: "b".into(), ..Unit::default() }])
}

/// The server's part, in memory: entries queued, candidates built and finished as a test says,
/// and the trunk moved by each land.
struct Sim {
    units: Units,
    knobs: Knobs,
    /// Pending entries, in queue order.
    items: Vec<Item>,
    /// Entries whose checks fail on any candidate that holds them.
    bad: BTreeSet<String>,
    /// Candidates under test: each with the number of trunk moves when it was built.
    cands: Vec<(Candidate, usize)>,
    /// The files of each trunk move, in order, and the entries each landed.
    moves: Vec<BTreeSet<String>>,
    landed: Vec<(Vec<String>, bool)>,
    ejected: Vec<String>,
    retested: Vec<String>,
    /// How many candidates held each entry.
    built: BTreeMap<String, usize>,
    log: Vec<String>,
    next: usize,
}

impl Sim {
    fn new(units: Units, batch: u32) -> Sim {
        Sim { units, knobs: Knobs::of_batch(batch), items: Vec::new(), bad: BTreeSet::new(), cands: Vec::new(), moves: Vec::new(), landed: Vec::new(), ejected: Vec::new(), retested: Vec::new(), built: BTreeMap::new(), log: Vec::new(), next: 0 }
    }

    fn queue(&mut self, id: &str, files: &[&str]) {
        self.items.push(Item { id: id.into(), change: Change { id: id.into(), files: set(files), targets: BTreeSet::new() }, ..Item::default() });
    }

    /// The trunk moved by something else: a push of `files`.
    fn push(&mut self, files: &[&str]) {
        self.moves.push(set(files));
    }

    /// The candidate holding `entry` finishes: failed if it holds a bad entry.
    fn finish(&mut self, entry: &str) {
        let bad = self.bad.clone();
        for (c, _) in &mut self.cands {
            if c.entries.iter().any(|e| e == entry) && c.outcome == Outcome::Running {
                c.outcome = if c.entries.iter().any(|e| bad.contains(e)) { Outcome::Failed("`gate` failure on the candidate".into()) } else { Outcome::Passed };
            }
        }
    }

    fn since(&self, at: usize) -> Result<Option<BTreeSet<String>>, String> {
        let moved: BTreeSet<String> = self.moves.iter().skip(at).flatten().cloned().collect();
        Ok((self.moves.len() > at).then_some(moved))
    }

    /// One tick: the plan, carried out as the server carries it out.
    fn tick(&mut self) -> Plan {
        let cands: Vec<Candidate> = self.cands.iter().map(|(c, at)| Candidate { since: self.since(*at), ..c.clone() }).collect();
        let plan = plan(&self.units, &self.items, &cands, &self.knobs);
        for (id, s) in &plan.standings {
            if let Some(i) = self.items.iter_mut().find(|i| &i.id == id) {
                i.window = Some(s.window);
            }
        }
        for step in &plan.steps {
            self.log.push(format!("{step:?}"));
            match step {
                Step::Land { batch, entries, rebased, .. } => {
                    self.cands.retain(|(c, _)| &c.batch != batch);
                    let files: BTreeSet<String> = self.items.iter().filter(|i| entries.contains(&i.id)).flat_map(|i| i.change.files.iter().cloned()).collect();
                    self.items.retain(|i| !entries.contains(&i.id));
                    self.moves.push(files);
                    self.landed.push((entries.clone(), *rebased));
                }
                Step::Retest { batch, entries, .. } => {
                    self.cands.retain(|(c, _)| &c.batch != batch);
                    self.retested.extend(entries.iter().cloned());
                    for i in self.items.iter_mut().filter(|i| entries.contains(&i.id)) {
                        i.batch = None;
                    }
                }
                Step::Eject { batch, entry, .. } => {
                    self.cands.retain(|(c, _)| &c.batch != batch);
                    self.items.retain(|i| &i.id != entry);
                    self.ejected.push(entry.clone());
                }
                Step::Split { batch, entries, head, next, .. } => {
                    self.cands.retain(|(c, _)| &c.batch != batch);
                    for i in self.items.iter_mut().filter(|i| entries.contains(&i.id)) {
                        i.batch = None;
                        if &i.id == head {
                            i.bisect = Some(*next);
                        }
                    }
                }
                Step::Build { entries, .. } => {
                    self.next += 1;
                    let batch = format!("b{}", self.next);
                    for i in self.items.iter_mut().filter(|i| entries.contains(&i.id)) {
                        i.batch = Some(batch.clone());
                        i.bisect = None;
                        *self.built.entry(i.id.clone()).or_insert(0) += 1;
                    }
                    self.cands.push((Candidate { batch, entries: entries.clone(), outcome: Outcome::Running, since: Ok(None) }, self.moves.len()));
                }
            }
        }
        plan
    }

    fn builds(plan: &Plan) -> Vec<Vec<String>> {
        plan.steps.iter().filter_map(|s| if let Step::Build { entries, .. } = s { Some(entries.clone()) } else { None }).collect()
    }
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// W28-R100 to R103 (W14-AC03 extended): four entries in two components, one culprit. Each
/// component gets a candidate of its own; the clean one lands at once, in one move; the culprit's
/// is bisected inside it, ejecting only the culprit; nothing is tested again.
#[test]
fn w28_r100_four_entries_two_components_one_culprit() {
    let mut s = Sim::new(two(), 4);
    for (id, f) in [("D1", "a/one.txt"), ("D2", "b/BAD"), ("D3", "a/three.txt"), ("D4", "b/four.txt")] {
        s.queue(id, &[f]);
    }
    s.bad.insert("D2".into());
    let p = s.tick();
    assert_eq!(Sim::builds(&p), [ids(&["D1", "D3"]), ids(&["D2", "D4"])], "a candidate each, side by side");
    let why: Vec<String> = p.steps.iter().filter_map(|st| if let Step::Build { why, .. } = st { Some(why.clone()) } else { None }).collect();
    assert_eq!(why.first().map(String::as_str), Some("D1, D3: one candidate on the trunk (window 4), where they meet: D3 waits for D1: both affect a (unit a)"));
    assert_eq!(p.standings.get("D3").map(|st| (st.component.as_str(), st.position, st.window)), Some(("D1", 2, 4)));
    assert_eq!(p.standings.get("D4").map(|st| st.why.clone()), Some(vec!["lands beside D1: no shared unit, file or target".to_owned(), "waits for D2: both affect b (unit b)".to_owned(), "lands beside D3: no shared unit, file or target".to_owned()]));

    s.finish("D2");
    s.finish("D1");
    let p = s.tick();
    // The clean component lands at once, both in one move; the culprit's splits.
    assert_eq!(s.landed, [(ids(&["D1", "D3"]), false)]);
    assert!(p.steps.iter().any(|st| matches!(st, Step::Split { next: 1, window: 2, head, .. } if head == "D2")), "{:?}", p.steps);
    let p = s.tick();
    assert_eq!(Sim::builds(&p), [ids(&["D2"])], "bisected: the first half alone");
    s.finish("D2");
    let p = s.tick();
    assert!(p.steps.iter().any(|st| matches!(st, Step::Eject { entry, window: 1, .. } if entry == "D2")), "{:?}", p.steps);
    let p = s.tick();
    assert_eq!(Sim::builds(&p), [ids(&["D4"])]);
    s.finish("D4");
    s.tick();
    assert_eq!(s.landed, [(ids(&["D1", "D3"]), false), (ids(&["D4"]), false)]);
    assert_eq!(s.ejected, ["D2"], "only the culprit");
    assert!(s.retested.is_empty(), "nothing tested again: {:?}", s.log);
    assert_eq!((s.built.get("D1"), s.built.get("D3"), s.built.get("D4")), (Some(&1), Some(&1), Some(&2)), "the clean component's one candidate");
}

/// W28-R102: two entries that share nothing, each its own candidate; the trunk moves for one, and
/// the other's result stands: it lands as tested, without being tested again.
#[test]
fn w28_r102_independent_entries_land_without_being_tested_again() {
    let mut s = Sim::new(two(), 1);
    s.queue("D1", &["a/x.txt"]);
    s.queue("D2", &["b/y.txt"]);
    let p = s.tick();
    assert_eq!(Sim::builds(&p), [ids(&["D1"]), ids(&["D2"])]);
    assert!(p.steps.iter().all(|st| matches!(st, Step::Build { why, .. } if why.ends_with("a candidate of its own on the trunk: it meets no other change in the queue"))), "{:?}", p.steps);
    s.finish("D1");
    s.tick();
    s.finish("D2");
    let p = s.tick();
    assert!(p.steps.iter().any(|st| matches!(st, Step::Land { rebased: true, why, .. } if why.contains("its result stands"))), "{:?}", p.steps);
    assert_eq!(s.landed, [(ids(&["D1"]), false), (ids(&["D2"]), true)]);
    assert!(s.retested.is_empty());
    assert_eq!((s.built.get("D1"), s.built.get("D2")), (Some(&1), Some(&1)), "each tested once");
}

/// W28-R102: a candidate whose trunk moved for what it meets, or whose base the trunk dropped, is
/// tested again, saying why.
#[test]
fn w28_r102_a_result_that_doesnt_stand_is_tested_again() {
    let mut s = Sim::new(two(), 1);
    s.queue("D2", &["b/y.txt"]);
    s.tick();
    s.push(&["b/other.txt"]);
    s.finish("D2");
    let p = s.tick();
    let why = p.steps.iter().find_map(|st| if let Step::Retest { why, .. } = st { Some(why.clone()) } else { None }).unwrap_or_default();
    assert_eq!(why, "D2: passed, but the trunk moved since it was built, and D2 waits for what landed since: both affect b (unit b): tested again on the trunk as it is");
    assert_eq!(Sim::builds(&s.tick()), [ids(&["D2"])], "built again on the trunk as it is");

    let items = [Item { id: "e1".into(), change: Change { id: "D9".into(), files: set(&["a/z"]), targets: BTreeSet::new() }, batch: Some("b".into()), ..Item::default() }];
    let rewritten = Candidate { batch: "b".into(), entries: ids(&["e1"]), outcome: Outcome::Passed, since: Err("the trunk was rewritten since it was built".into()) };
    let p = plan(&two(), &items, &[rewritten], &Knobs::of_batch(1));
    assert!(matches!(p.steps.as_slice(), [Step::Retest { why, .. }] if why == "D9: passed, but the trunk was rewritten since it was built: tested again on the trunk as it is"), "{:?}", p.steps);
    // An entry that left a candidate leaves the rest of it to be tested again.
    let left = Candidate { batch: "b".into(), entries: ids(&["e1", "gone"]), outcome: Outcome::Passed, since: Ok(None) };
    let p = plan(&two(), &items, &[left], &Knobs::of_batch(2));
    assert!(matches!(p.steps.as_slice(), [Step::Retest { entries, .. }] if entries == &ids(&["e1"])), "{:?}", p.steps);
}

/// W28-R31, R100: a change that meets nothing ahead of it lands at once, though a later change
/// joins its component to another's.
#[test]
fn w28_r100_components_joined_later_land_what_meets_nothing_ahead() {
    let mut s = Sim::new(two(), 1);
    s.queue("X", &["a/x"]);
    s.queue("Y", &["b/y"]);
    s.tick();
    s.queue("Z", &["a/z", "b/z"]);
    let p = s.tick();
    assert!(Sim::builds(&p).is_empty(), "Z waits for its component's candidates: {:?}", p.steps);
    assert_eq!(p.standings.get("Z").map(|st| st.why.clone()), Some(vec!["waits for X: both affect a (unit a)".to_owned(), "waits for Y: both affect b (unit b)".to_owned()]));
    s.finish("Y");
    s.tick();
    assert_eq!(s.landed, [(ids(&["Y"]), false)], "Y lands beside X");
    s.finish("X");
    s.tick();
    assert_eq!(Sim::builds(&s.tick()), [ids(&["Z"])]);
}

/// W28-R36 in the server's queue: each component's window starts at `queue.batch`, halves on a
/// failure, and opens by one for each change landed, never past the cap.
#[test]
fn w28_r101_windows_by_component() {
    let mut s = Sim::new(two(), 4);
    for (id, f) in [("A1", "a/1"), ("A2", "a/2"), ("A3", "a/3"), ("A4", "a/4"), ("A5", "a/5")] {
        s.queue(id, &[f]);
    }
    s.bad.insert("A4".into());
    assert_eq!(Sim::builds(&s.tick()), [ids(&["A1", "A2", "A3", "A4"])], "its window, four");
    s.finish("A1");
    s.tick();
    assert_eq!(Sim::builds(&s.tick()), [ids(&["A1", "A2"])], "halved, and bisected to the first two");
    s.finish("A1");
    s.tick();
    assert_eq!(s.landed, [(ids(&["A1", "A2"]), false)]);
    assert_eq!(s.items.first().and_then(|i| i.window), Some(4), "two landed: 2 + 2");
    assert_eq!(Sim::builds(&s.tick()), [ids(&["A3", "A4", "A5"])]);
}

/// W28-R35 in the server's queue: new candidates go by R35's order, the shortest expected service
/// first, then the longest wait.
#[test]
fn w28_r103_new_candidates_in_the_order_of_r35() {
    let item = |id: &str, file: &str, service_s: i64, waited_s: i64| Item { id: id.into(), change: Change { id: id.into(), files: set(&[file]), targets: BTreeSet::new() }, service_s, waited_s, ..Item::default() };
    let items = [item("slow", "a/x", 1800, 0), item("quick", "b/y", 60, 0)];
    let p = plan(&two(), &items, &[], &Knobs::of_batch(1));
    assert_eq!(Sim::builds(&p), [ids(&["quick"]), ids(&["slow"])]);
    let aged = [item("slow", "a/x", 1800, 3600), item("quick", "b/y", 60, 0)];
    assert_eq!(Sim::builds(&plan(&two(), &aged, &[], &Knobs::of_batch(1))), [ids(&["slow"]), ids(&["quick"])], "past the aging bound nothing passes it");
}

/// W28-R100: the units of a package graph: each package with its folder, dependencies and its
/// workspace's files, a name two ecosystems share qualified.
#[test]
fn w28_r100_units_of_package_graphs() {
    let pkg = |name: &str, eco: Ecosystem, dir: &str, deps: &[&str]| Package { name: name.into(), ecosystem: eco, dir: dir.into(), member: true, workspace: Some(String::new()), deps: set(deps) };
    let trunk = Graph { packages: vec![pkg("core", Ecosystem::Cargo, "crates/core", &[]), pkg("ui", Ecosystem::Cargo, "crates/ui", &["core"]), pkg("ui", Ecosystem::Npm, "ui", &[])], problems: Vec::new() };
    let u = units([&trunk]);
    assert_eq!(u.unit("cargo:ui").map(|x| (x.dir.as_str(), x.deps.clone())), Some(("crates/ui", set(&["core"]))));
    assert!(u.unit("npm:ui").is_some() && u.unit("ui").is_none(), "qualified where two ecosystems share it");
    assert_eq!(u.unit("core").map(|x| x.reads.clone()), Some(set(&["Cargo.toml", "Cargo.lock"])));
    assert_eq!(u.affected(["Cargo.lock"]), set(&["cargo:ui", "core"]), "a workspace's lockfile affects every member");
    assert_eq!(u.affected(["crates/core/src/lib.rs"]), set(&["cargo:ui", "core"]));
    // An entry's own graph adds what it adds.
    let mine = Graph { packages: vec![pkg("core", Ecosystem::Cargo, "crates/core", &["extra"]), pkg("extra", Ecosystem::Cargo, "crates/extra", &[])], problems: Vec::new() };
    assert_eq!(units([&trunk, &mine]).affected(["crates/extra/a.rs"]), set(&["cargo:ui", "core", "extra"]));
}

/// W28-R104: Review's gate, of a diff and of an entry.
#[test]
fn w28_r104_the_gate() {
    assert_eq!(Gate::of(Vec::new(), Vec::new()), Gate::Ready);
    assert_eq!(Gate::of(vec!["0 of 1 accepts".into()], Vec::new()), Gate::Hold { why: vec!["0 of 1 accepts".into()] });
    assert_eq!(Gate::of(vec!["0 of 1 accepts".into()], vec!["README.md".into()]), Gate::Conflict { files: vec!["README.md".into()], why: vec!["0 of 1 accepts".into()] }, "a conflict goes back to Review");
    // Review's own reasons stay beside the conflict, each said (W26's conflicts where they fall).
    let with = Gate::of(vec!["expand.rs:3: resolve this conflict first".into()], vec!["expand.rs".into()]);
    assert_eq!(with.reason().as_deref(), Some("needs rebase: conflicts with the trunk in expand.rs; expand.rs:3: resolve this conflict first"));
    assert_eq!(prefix(&[("D1".to_owned(), with)]).held, [("D1".to_owned(), vec!["needs rebase: conflicts with the trunk in expand.rs".to_owned(), "expand.rs:3: resolve this conflict first".to_owned()])]);
    let entry = [("D1".to_owned(), Gate::Ready), ("D2".to_owned(), Gate::Hold { why: vec!["user:rev asked for changes".into()] })];
    let g = Gate::of_entry(&entry);
    assert_eq!(g.reason().as_deref(), Some("D2: user:rev asked for changes"));
    assert!(!g.ready() && Gate::of_entry(&[("D1".to_owned(), Gate::Ready)]).ready());
    let many: Vec<String> = (1..=7).map(|n| format!("f{n}")).collect();
    assert_eq!(Gate::Conflict { files: many, why: Vec::new() }.reason().as_deref(), Some("needs rebase: conflicts with the trunk in f1, f2, f3, f4, f5 and 2 more"));
}

/// W28-R105: a pinned revision holds through rebase-only revisions with its patch-id, and a newer
/// one that changes the patch invalidates it.
#[test]
fn w28_r105_pinned_revisions() {
    let pin = Pin { diff: "D2".into(), version: 1, commit: "c1".into(), parent: "p".into(), patch_id: "P".into() };
    let rev = |version: u32, patch_id: &str, rebase_only: bool| Revision { version, commit: format!("c{version}"), parent: "p".into(), patch_id: patch_id.into(), rebase_only };
    assert_eq!(pin_holds(&pin, &[rev(1, "P", false)]), Ok(()));
    assert_eq!(pin_holds(&pin, &[rev(1, "P", false), rev(2, "P", true)]), Ok(()), "rebase-only, the same patch");
    assert_eq!(pin_holds(&pin, &[rev(1, "P", false), rev(2, "Q", false)]), Err("it has v2 since v1 was approved, which changes its patch: v2 lands once Review approves it".to_owned()));
    assert!(pin_holds(&pin, &[rev(1, "P", false), rev(2, "Q", true)]).is_err(), "rebase-only with another patch-id");
    assert!(pin_holds(&pin, &[rev(1, "P", false), rev(2, "P", true), rev(3, "R", false)]).is_err());
    assert_eq!(pin_holds(&pin, &[rev(2, "P", false)]), Err("its v1, approved, is gone".to_owned()));
}

/// W28-R106: a stack lands its clear prefix; the rest holds, each saying why.
#[test]
fn w28_r106_a_stack_lands_its_clear_prefix() {
    let hold = |w: &str| Gate::Hold { why: vec![w.to_owned()] };
    let stack = [("D1".to_owned(), Gate::Ready), ("D2".to_owned(), Gate::Ready), ("D3".to_owned(), hold("0 of 1 accepts")), ("D4".to_owned(), Gate::Ready), ("D5".to_owned(), hold("1 open thread"))];
    let p = prefix(&stack);
    assert_eq!(p.clear, ["D1", "D2"]);
    assert_eq!(
        p.held,
        [
            ("D3".to_owned(), vec!["0 of 1 accepts".to_owned()]),
            ("D4".to_owned(), vec!["D3, below it, holds".to_owned()]),
            ("D5".to_owned(), vec!["D3, below it, holds".to_owned(), "1 open thread".to_owned()])
        ]
    );
    let blocked = prefix(&[("D1".to_owned(), Gate::Conflict { files: vec!["a".into()], why: Vec::new() }), ("D2".to_owned(), Gate::Ready)]);
    assert!(blocked.clear.is_empty() && blocked.held.len() == 2, "{blocked:?}");
    // A diff's reasons stay apart, so each is said with its name (W26-R11's blocking thread among them).
    let two = prefix(&[("D1".to_owned(), Gate::Hold { why: vec!["0 of 1 accepts".into(), "expand.rs:2: kim resolves this blocking comment first".into()] })]);
    assert_eq!(two.held, [("D1".to_owned(), vec!["0 of 1 accepts".to_owned(), "expand.rs:2: kim resolves this blocking comment first".to_owned()])]);
}

/// W28-R109: which queue lands a repository's changes, and how an entry stands on its host's.
#[test]
fn w28_r109_a_hosts_queue() {
    assert_eq!(Route::of(None), Route::Kiln);
    assert_eq!(Route::of(Some("merge queue")).name(), "merge queue");
    let on = |number: u64, queued: bool, state: HostState| OnHost { number, entered: true, queued, state };
    let merged = |c: &str| HostState::Merged { commit: c.into() };
    assert_eq!(on_host(&[on(1, false, merged("m1")), on(2, false, merged("m2"))]), HostVerdict::Landed { commits: vec![(1, "m1".into()), (2, "m2".into())] });
    assert_eq!(on_host(&[on(1, false, merged("m1")), on(2, true, HostState::Open)]), HostVerdict::Waiting, "partly merged, the rest queued");
    assert!(matches!(on_host(&[on(1, true, HostState::Open), on(2, false, HostState::Open)]), HostVerdict::Out { number: 2, .. }), "open and not queued: ejected or dequeued");
    let waits = OnHost { entered: false, ..on(2, false, HostState::Open) };
    assert_eq!(on_host(&[on(1, true, HostState::Open), waits.clone()]), HostVerdict::Waiting, "one not put in yet waits (a train takes a stack's next once the one below merged)");
    assert!(matches!(on_host(&[on(7, false, HostState::Closed)]), HostVerdict::Out { number: 7, why } if why.contains("closed")));
    assert_eq!(on_host(&[]), HostVerdict::Waiting);
    assert_eq!(next_to_enqueue(&[on(1, true, HostState::Open), waits.clone(), OnHost { number: 3, ..waits.clone() }]), Some(2));
    assert_eq!(next_to_enqueue(&[on(1, true, HostState::Open)]), None);
}

/// W28-R108: a land record reads back as written, and fields it lacks are their defaults.
#[test]
fn w28_r108_the_land_record() -> Result<(), serde_json::Error> {
    let r = Record {
        changes: ids(&["D1", "D2"]),
        kind: "stack".into(),
        strategy: "stack".into(),
        revisions: vec![Pin { diff: "D1".into(), version: 2, commit: "c".into(), parent: "p".into(), patch_id: "P".into() }],
        candidate: Some("cand".into()),
        runs: vec![CheckRun { run: "r1".into(), workflow: ".github/workflows/gate.yml".into(), conclusion: Some("success".into()) }],
        landed: BTreeMap::from([("D1".to_owned(), "l1".to_owned())]),
        queue: "kiln".into(),
        ..Record::default()
    };
    let back: Record = serde_json::from_str(&serde_json::to_string(&r)?)?;
    assert_eq!(back, r);
    let sparse: Record = serde_json::from_str(r#"{"changes":["D9"]}"#)?;
    assert_eq!((sparse.changes, sparse.policy.queue.batch), (ids(&["D9"]), 1));
    Ok(())
}
