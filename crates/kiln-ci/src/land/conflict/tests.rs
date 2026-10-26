//! docs/ci.md W25-R30 to R34 and W25-AC30: the conflict graph over kiln's own crates, and 10,000
//! random queues over synthetic package graphs.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use sha2::{Digest as _, Sha256};

use super::*;

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn change(id: &str, files: &[&str], targets: &[&str]) -> Change {
    Change { id: id.into(), files: set(files), targets: set(targets) }
}

fn names<V>(m: &BTreeMap<String, V>) -> Vec<&str> {
    m.keys().map(String::as_str).collect()
}

/// kiln's crates and UI as W24-R16 reads them (2026-10-25): every crate reads the workspace's
/// `Cargo.toml` and `Cargo.lock`; `kiln` depends on the other five crates, `kiln-sim` on
/// `kiln-seams`.
fn kiln() -> Units {
    let cargo = |name: &str, deps: &[&str]| Unit { name: name.into(), ecosystem: "cargo".into(), dir: format!("crates/{name}"), reads: set(&["Cargo.toml", "Cargo.lock"]), deps: set(deps) };
    Units::new([
        cargo("kiln", &["kiln-cd", "kiln-ci", "kiln-http", "kiln-seams", "kiln-sim"]),
        cargo("kiln-cd", &[]),
        cargo("kiln-ci", &[]),
        cargo("kiln-http", &[]),
        cargo("kiln-seams", &[]),
        cargo("kiln-sim", &["kiln-seams"]),
        Unit { name: "kiln-ui".into(), ecosystem: "npm".into(), dir: "ui".into(), ..Unit::default() },
    ])
}

#[test]
fn w25_r31_kiln_ci_ui_and_sched() -> Result<(), String> {
    let queue = [
        change("A", &["crates/kiln-ci/src/land.rs"], &[]),
        change("B", &["ui/src/routes/index.tsx"], &[]),
        change("C", &["crates/kiln-ci/src/sched/mod.rs", "docs/ci.md"], &[]),
    ];
    let q = Conflicts::new(&kiln(), &queue);
    let verdict = |id: &str| q.verdict(id).ok_or(format!("no verdict for {id}"));
    let component = |id: &str| q.component_of(id).ok_or(format!("no component for {id}"));

    // B is its own component, and lands at once.
    assert_eq!(component("B")?.changes, ["B"]);
    assert!(verdict("B")?.at_once());
    assert_eq!(verdict("B")?.why(), ["lands beside A: no shared unit, file or target"]);

    // A and C share kiln-ci: one component, whose only interaction unit is kiln-ci. kiln depends
    // on kiln-ci, so both affect it as well: it's a joint unit, built with both.
    let ac = component("A")?;
    assert_eq!(ac.changes, ["A", "C"]);
    assert_eq!(names(&ac.interaction), ["kiln-ci"]);
    assert_eq!(ac.interaction.get("kiln-ci"), Some(&vec!["A".to_owned(), "C".to_owned()]));
    assert_eq!(names(&ac.joint), ["kiln", "kiln-ci"]);
    assert_eq!(q.components.len(), 2);

    assert_eq!(verdict("A")?.why(), ["lands at once: nothing is ahead of it in the queue"]);
    assert_eq!(verdict("C")?.why(), ["waits for A: both affect crates/kiln-ci (unit kiln-ci)", "lands beside B: no shared unit, file or target"]);
    assert_eq!(verdict("C")?.waits_for(), ["A"]);
    assert!(!verdict("C")?.at_once());

    // B lands beside either, in any order, as one class; A and C don't commute.
    let ab = Class(set(&["A", "B"]));
    assert_eq!((q.class(&["A", "B"]), q.class(&["B", "A"])), (Ok(ab.clone()), Ok(ab)));
    assert_eq!(q.class(&["C", "B"]), Ok(Class(set(&["B", "C"]))));
    assert_eq!(q.class(&["C", "B", "A"]), Err("C waits for A: both affect crates/kiln-ci (unit kiln-ci)".to_owned()));
    assert_eq!(q.class(&["A", "D"]), Err("D isn't in the queue".to_owned()));
    assert_eq!(q.class(&["B", "B"]), Err("B is given twice".to_owned()));
    Ok(())
}

#[test]
fn w25_r34_every_decision_names_what_is_shared() -> Result<(), String> {
    let queue = [
        change("D1", &["docs/ci.md"], &["prod"]),
        change("D2", &["docs/ci.md"], &[]),
        change("D3", &["crates/kiln-cd/src/rollout.rs"], &["prod"]),
        change("D4", &["Cargo.lock"], &[]),
        change("D5", &["crates/kiln-ci/src/lib.rs"], &[]),
    ];
    let q = Conflicts::new(&kiln(), &queue);
    let why = |id: &str| q.verdict(id).map(Verdict::why).ok_or(format!("no verdict for {id}"));
    let beside = |id: &str| format!("lands beside {id}: no shared unit, file or target");
    assert_eq!(why("D2")?, ["waits for D1: both change docs/ci.md"]);
    assert_eq!(why("D3")?, ["waits for D1: both change deploy target prod".to_owned(), beside("D2")]);
    // Every crate reads the lockfile: D4 meets D3 at kiln-cd, not at kiln, which depends on it.
    assert_eq!(why("D4")?, [beside("D1"), beside("D2"), "waits for D3: both affect crates/kiln-cd (unit kiln-cd)".to_owned()]);
    // kiln-cd's and kiln-ci's changes meet at kiln, which depends on both.
    assert_eq!(why("D5")?, [beside("D1"), beside("D2"), "waits for D3: both affect crates/kiln (unit kiln)".to_owned(), "waits for D4: both affect crates/kiln-ci (unit kiln-ci)".to_owned()]);

    let k = q.component_of("D1").ok_or("no component")?;
    assert_eq!(k.changes, ["D1", "D2", "D3", "D4", "D5"]);
    let by = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(k.interaction, BTreeMap::from([("kiln-cd".to_owned(), by(&["D3", "D4"])), ("kiln-ci".to_owned(), by(&["D4", "D5"]))]));
    assert_eq!(names(&k.joint), ["kiln", "kiln-cd", "kiln-ci"]);
    assert_eq!(k.joint.get("kiln"), Some(&by(&["D3", "D4", "D5"])));
    Ok(())
}

#[test]
fn w25_r30_a_dependency_a_change_adds_links_it() {
    let unit = |name: &str, deps: &[&str]| Unit { name: name.into(), ecosystem: "cargo".into(), dir: name.into(), deps: set(deps), ..Unit::default() };
    let queue = [change("X", &["u/Cargo.toml"], &[]), change("Y", &["v/src/lib.rs"], &[])];
    // On the trunk's graph alone u doesn't depend on v, and they'd land side by side.
    let trunk = Units::new([unit("u", &[]), unit("v", &[])]);
    assert_eq!(Conflicts::new(&trunk, &queue).verdict("Y").map(Verdict::why), Some(vec!["lands beside X: no shared unit, file or target".to_owned()]));
    // X's own graph has u depend on v: with it, Y affects u too.
    let union = Units::new([unit("u", &[]), unit("v", &[]), unit("u", &["v"])]);
    assert_eq!(union.affected(["v/src/lib.rs"]), set(&["u", "v"]));
    assert_eq!(Conflicts::new(&union, &queue).verdict("Y").map(Verdict::why), Some(vec!["waits for X: both affect u (unit u)".to_owned()]));
}

#[test]
fn w25_r34_cycles_the_root_and_long_lists() {
    let unit = |name: &str, dir: &str, deps: &[&str]| Unit { name: name.into(), ecosystem: "cargo".into(), dir: dir.into(), deps: set(deps), ..Unit::default() };
    let g = Units::new([unit("p", "p", &["q"]), unit("q", "q", &["p"]), unit("top", "", &[])]);
    let why = |queue: &[Change]| Conflicts::new(&g, queue).verdict("Y").map(Verdict::why);
    // A cycle of dependencies leaves no unit where they first meet: every unit both affect is named.
    assert_eq!(why(&[change("X", &["p/a.rs"], &[]), change("Y", &["q/b.rs"], &[])]), Some(vec!["waits for X: both affect p (unit p); both affect q (unit q)".to_owned()]));
    assert_eq!(why(&[change("X", &["README.md"], &[]), change("Y", &["./x.md"], &[])]), Some(vec!["waits for X: both affect the root (unit top)".to_owned()]));
    let all = change("X", &["README.md", "a", "b", "c"], &["t"]);
    assert_eq!(why(&[all.clone(), Change { id: "Y".into(), ..all }]), Some(vec!["waits for X: both change README.md; both change a; both change b; and 3 more".to_owned()]));
}

// -------------------------------------------------------------------- W25-AC30: random queues

/// The synthetic repositories' folders: each unit's is one of them.
const FOLDERS: [&str; 7] = ["", "a", "a/b", "a/b/c", "a/d", "e", "e/f"];
/// The workspace files a unit may read.
const WORKSPACE: [&str; 3] = ["Cargo.toml", "Cargo.lock", "package.json"];
const ELSEWHERE: [&str; 2] = ["docs/ci.md", "README.md"];
const ECOSYSTEMS: [&str; 2] = ["cargo", "npm"];
const TARGETS: [&str; 3] = ["t0", "t1", "t2"];

/// The files a change may edit besides manifests: two in each folder, the workspace's, and two
/// more.
fn plain() -> Vec<String> {
    let mut out: Vec<String> = FOLDERS.iter().flat_map(|d| ["x", "y"].map(|leaf| if d.is_empty() { leaf.to_owned() } else { format!("{d}/{leaf}") })).collect();
    out.extend(WORKSPACE.iter().chain(ELSEWHERE.iter()).map(|f| (*f).to_owned()));
    out
}

/// A unit's manifest: the units it depends on, separated by commas.
fn manifest(u: &Unit) -> String {
    if u.dir.is_empty() { format!("{}.deps", u.name) } else { format!("{}/{}.deps", u.dir, u.name) }
}

fn joined(deps: &BTreeSet<String>) -> String {
    deps.iter().map(String::as_str).collect::<Vec<_>>().join(",")
}

#[derive(Debug, Clone)]
struct RawUnit {
    folder: usize,
    eco: usize,
    reads: u8,
    deps: u8,
}

#[derive(Debug, Clone)]
struct RawChange {
    /// Files (by place in [`plain`]) and their new contents, or none to remove them.
    edits: Vec<(usize, Option<u8>)>,
    targets: u8,
    /// A unit (by place) whose manifest it rewrites, and which of the units before it it then
    /// depends on.
    deps: Option<(usize, u8)>,
}

#[derive(Debug, Clone)]
struct Raw {
    units: Vec<RawUnit>,
    trunk: Vec<Option<u8>>,
    changes: Vec<RawChange>,
}

fn raw() -> impl Strategy<Value = Raw> {
    let sparse = || (any::<u8>(), any::<u8>()).prop_map(|(a, b)| a & b);
    let unit = (0..FOLDERS.len(), 0..ECOSYSTEMS.len(), prop_oneof![2 => Just(0u8), 1 => any::<u8>()], sparse()).prop_map(|(folder, eco, reads, deps)| RawUnit { folder, eco, reads, deps });
    let edit = (any::<usize>(), prop::option::weighted(0.8, any::<u8>()));
    let targets = prop_oneof![3 => Just(0u8), 1 => any::<u8>()];
    let change = (prop::collection::vec(edit, 0..=3), targets, prop::option::weighted(0.25, (any::<usize>(), sparse()))).prop_map(|(edits, targets, deps)| RawChange { edits, targets, deps });
    (prop::collection::vec(unit, 1..=8), prop::collection::vec(prop::option::weighted(0.8, any::<u8>()), plain().len()), prop::collection::vec(change, 1..=6)).prop_map(|(units, trunk, changes)| Raw { units, trunk, changes })
}

/// A synthetic repository and its queue.
struct Case {
    /// The trunk's units, in order: each depends only on units before it, as Cargo's do.
    units: Vec<Unit>,
    trunk: BTreeMap<String, String>,
    queue: Vec<Change>,
    /// Each change's edits: a file's new contents, or none when it's removed.
    edits: Vec<BTreeMap<String, Option<String>>>,
    /// Each unit's dependencies on the trunk or in any change: the union (W25-R30).
    deps: BTreeMap<String, BTreeSet<String>>,
    /// The graph the conflict graph is given: the trunk's units, then each change's own.
    graph: Units,
}

fn build(raw: &Raw) -> Case {
    let files = plain();
    let units: Vec<Unit> = raw
        .units
        .iter()
        .enumerate()
        .map(|(i, r)| Unit {
            name: format!("u{i}"),
            ecosystem: ECOSYSTEMS.get(r.eco).copied().unwrap_or("cargo").to_owned(),
            dir: FOLDERS.get(r.folder).copied().unwrap_or("").to_owned(),
            reads: WORKSPACE.iter().enumerate().filter(|(k, _)| (r.reads >> k) & 1 == 1).map(|(_, f)| (*f).to_owned()).collect(),
            deps: (0..i).filter(|k| (r.deps >> k) & 1 == 1).map(|k| format!("u{k}")).collect(),
        })
        .collect();
    let mut trunk: BTreeMap<String, String> = files.iter().zip(&raw.trunk).filter_map(|(f, v)| v.map(|v| (f.clone(), format!("trunk {v}")))).collect();
    trunk.extend(units.iter().map(|u| (manifest(u), joined(&u.deps))));

    let mut deps: BTreeMap<String, BTreeSet<String>> = units.iter().map(|u| (u.name.clone(), u.deps.clone())).collect();
    let mut graph = units.clone();
    let (mut queue, mut edits) = (Vec::new(), Vec::new());
    for (n, r) in raw.changes.iter().enumerate() {
        let id = format!("D{}", n + 1);
        let mut e: BTreeMap<String, Option<String>> = BTreeMap::new();
        for (f, v) in &r.edits {
            if let Some(path) = files.get(f % files.len()) {
                e.insert(path.clone(), v.map(|v| format!("{id} {v}")));
            }
        }
        if let Some((k, mask)) = r.deps {
            let i = k % units.len();
            if let Some(u) = units.get(i) {
                let new: BTreeSet<String> = (0..i).filter(|j| (mask >> j) & 1 == 1).map(|j| format!("u{j}")).collect();
                e.insert(manifest(u), Some(joined(&new)));
                deps.entry(u.name.clone()).or_default().extend(new.iter().cloned());
                graph.push(Unit { deps: new, ..u.clone() });
            }
        }
        let targets = TARGETS.iter().enumerate().filter(|(k, _)| (r.targets >> k) & 1 == 1).map(|(_, t)| (*t).to_owned()).collect();
        queue.push(Change { id, files: e.keys().cloned().collect(), targets });
        edits.push(e);
    }
    Case { graph: Units::new(graph), units, trunk, queue, edits, deps }
}

/// Whether `u` takes `file`, as W24-R17 has it, worked out afresh: it reads it, or its folder holds
/// it and no folder of a unit of its ecosystem holds it more closely.
fn takes(units: &[Unit], u: &Unit, file: &str) -> bool {
    let holds = |dir: &str| dir.is_empty() || file.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'));
    u.reads.contains(file) || (holds(&u.dir) && !units.iter().any(|v| v.ecosystem == u.ecosystem && v.dir.len() > u.dir.len() && holds(&v.dir)))
}

/// The units a change to `files` affects, worked out afresh: what takes them, then a fixpoint over
/// `deps`.
fn affected(units: &[Unit], deps: &BTreeMap<String, BTreeSet<String>>, files: &BTreeSet<String>) -> BTreeSet<String> {
    let mut hit: BTreeSet<String> = units.iter().filter(|u| files.iter().any(|f| takes(units, u, f))).map(|u| u.name.clone()).collect();
    loop {
        let more: Vec<String> = units.iter().filter(|u| !hit.contains(&u.name) && deps.get(&u.name).is_some_and(|d| d.iter().any(|x| hit.contains(x)))).map(|u| u.name.clone()).collect();
        if more.is_empty() {
            return hit;
        }
        hit.extend(more);
    }
}

/// `edits` (by place) applied to `trunk` in `order`.
fn land(trunk: &BTreeMap<String, String>, edits: &[BTreeMap<String, Option<String>>], order: &[usize]) -> BTreeMap<String, String> {
    let mut tree = trunk.clone();
    for e in order.iter().filter_map(|&i| edits.get(i)) {
        for (path, text) in e {
            match text {
                Some(t) => {
                    tree.insert(path.clone(), t.clone());
                }
                None => {
                    tree.remove(path);
                }
            }
        }
    }
    tree
}

/// Every unit's key in `tree`, standing in for W24-R01's: SHA-256 over its name, the files it takes
/// (`inputs`) with their contents, and the keys of the units the tree's manifest says it depends on.
fn keys(units: &[Unit], inputs: &BTreeMap<&str, Vec<&str>>, tree: &BTreeMap<String, String>) -> BTreeMap<String, [u8; 32]> {
    let mut out: BTreeMap<String, [u8; 32]> = BTreeMap::new();
    for u in units {
        let mut h = Sha256::new();
        h.update(u.name.as_bytes());
        for path in inputs.get(u.name.as_str()).into_iter().flatten() {
            if let Some(text) = tree.get(*path) {
                h.update([0u8]);
                h.update(path.as_bytes());
                h.update([1u8]);
                h.update(text.as_bytes());
            }
        }
        for d in tree.get(&manifest(u)).into_iter().flat_map(|m| m.split(',')).filter(|d| !d.is_empty()) {
            h.update([2u8]);
            h.update(d.as_bytes());
            h.update(out.get(d).map(|k| k.as_slice()).unwrap_or_default());
        }
        out.insert(u.name.clone(), h.finalize().into());
    }
    out
}

fn permutations(items: &[usize]) -> Vec<Vec<usize>> {
    if items.len() < 2 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for (i, &x) in items.iter().enumerate() {
        let rest: Vec<usize> = items.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, &y)| y).collect();
        for mut p in permutations(&rest) {
            p.insert(0, x);
            out.push(p);
        }
    }
    out
}

/// The orders a set of changes is landed in: all of them for up to four, else every rotation and
/// its reverse.
fn orders(items: &[usize]) -> Vec<Vec<usize>> {
    if items.len() <= 4 {
        return permutations(items);
    }
    let mut out = Vec::new();
    for r in 0..items.len() {
        let mut o = items.to_vec();
        o.rotate_left(r);
        out.push(o.clone());
        o.reverse();
        out.push(o);
    }
    out
}

/// What the property test saw, across its cases.
#[derive(Default)]
struct Seen {
    cases: Cell<usize>,
    /// Sets of two or more changes no two of which conflict, landed in every order checked.
    commuting: Cell<usize>,
    /// Components of two or more changes landed whole.
    whole: Cell<usize>,
}

fn bump(c: &Cell<usize>) {
    c.set(c.get() + 1);
}

fn check(raw: &Raw, seen: &Seen) -> Result<(), TestCaseError> {
    bump(&seen.cases);
    let case = build(raw);
    let q = Conflicts::new(&case.graph, &case.queue);
    let n = case.queue.len();
    prop_assert_eq!(q.changes.len(), n);
    let id = |i: usize| case.queue.get(i).map(|c| c.id.clone()).unwrap_or_default();

    // R30: what each change's files are inputs of, and what it affects, worked out afresh.
    let universe: BTreeSet<String> = plain().into_iter().chain(case.units.iter().map(manifest)).collect();
    let inputs: BTreeMap<&str, Vec<&str>> = case.units.iter().map(|u| (u.name.as_str(), universe.iter().filter(|f| takes(&case.units, u, f)).map(String::as_str).collect())).collect();
    let (mut took, mut hit): (Vec<BTreeSet<String>>, Vec<BTreeSet<String>>) = (Vec::new(), Vec::new());
    for (p, c) in q.changes.iter().zip(&case.queue) {
        let t: BTreeSet<String> = case.units.iter().filter(|u| c.files.iter().any(|f| takes(&case.units, u, f))).map(|u| u.name.clone()).collect();
        let a = affected(&case.units, &case.deps, &c.files);
        prop_assert_eq!(&p.takes, &t);
        prop_assert_eq!(&p.units, &a);
        took.push(t);
        hit.push(a);
    }
    let meets = |i: usize, j: usize| match (case.queue.get(i), case.queue.get(j), hit.get(i), hit.get(j)) {
        (Some(a), Some(b), Some(ha), Some(hb)) => !a.files.is_disjoint(&b.files) || !ha.is_disjoint(hb) || !a.targets.is_disjoint(&b.targets),
        _ => false,
    };

    // R31: two changes share a component exactly when a chain of conflicts links them.
    let mut label: Vec<usize> = (0..n).collect();
    let mut moved = true;
    while moved {
        moved = false;
        for i in 0..n {
            for j in 0..n {
                let (li, lj) = (label.get(i).copied().unwrap_or(i), label.get(j).copied().unwrap_or(j));
                if li != lj && meets(i, j) {
                    for x in [i, j] {
                        if let Some(l) = label.get_mut(x) {
                            *l = li.min(lj);
                        }
                    }
                    moved = true;
                }
            }
        }
    }
    for i in 0..n {
        for j in 0..n {
            let same = q.changes.get(i).map(|c| c.component) == q.changes.get(j).map(|c| c.component);
            prop_assert_eq!(same, label.get(i) == label.get(j), "{} and {}", id(i), id(j));
        }
    }

    // R32: interaction units are exactly those whose inputs take files from two or more of a
    // component's changes; joint units, those two or more of them affect.
    for (k, comp) in q.components.iter().enumerate() {
        let members: Vec<usize> = (0..n).filter(|&i| q.changes.get(i).is_some_and(|c| c.component == k)).collect();
        prop_assert_eq!(&comp.changes, &members.iter().map(|&i| id(i)).collect::<Vec<_>>());
        let by = |sets: &[BTreeSet<String>]| -> BTreeMap<String, Vec<String>> {
            case.units
                .iter()
                .filter_map(|u| {
                    let ids: Vec<String> = members.iter().filter(|&&i| sets.get(i).is_some_and(|s| s.contains(&u.name))).map(|&i| id(i)).collect();
                    (ids.len() > 1).then(|| (u.name.clone(), ids))
                })
                .collect()
        };
        prop_assert_eq!(&comp.interaction, &by(took.as_slice()));
        prop_assert_eq!(&comp.joint, &by(hit.as_slice()));
    }

    // R34: each change is decided against each change ahead of it, and a wait names only what the
    // two really share: every file and target, and units both affect.
    for (j, v) in q.verdicts.iter().enumerate() {
        prop_assert_eq!(v.decisions.len(), j);
        prop_assert_eq!(v.at_once(), (0..j).all(|i| !meets(i, j)));
        let (Some(b), Some(hb)) = (case.queue.get(j), hit.get(j)) else { continue };
        for (i, d) in v.decisions.iter().enumerate() {
            let (Some(a), Some(ha)) = (case.queue.get(i), hit.get(i)) else { continue };
            match d {
                Decision::Beside { other } => {
                    prop_assert_eq!(other, &a.id);
                    prop_assert!(!meets(i, j), "{} lands beside {} though they conflict", b.id, a.id);
                    prop_assert_eq!(d.to_string(), format!("lands beside {}: no shared unit, file or target", a.id));
                }
                Decision::Waits { other, conflicts } => {
                    prop_assert_eq!(other, &a.id);
                    prop_assert!(meets(i, j) && !conflicts.is_empty());
                    let files: BTreeSet<&String> = conflicts.iter().filter_map(|c| if let Conflict::File { path } = c { Some(path) } else { None }).collect();
                    let targets: BTreeSet<&String> = conflicts.iter().filter_map(|c| if let Conflict::Target { name } = c { Some(name) } else { None }).collect();
                    prop_assert_eq!(files, a.files.intersection(&b.files).collect::<BTreeSet<_>>());
                    prop_assert_eq!(targets, a.targets.intersection(&b.targets).collect::<BTreeSet<_>>());
                    let mut units = 0;
                    for c in conflicts {
                        if let Conflict::Unit { name, dir } = c {
                            units += 1;
                            prop_assert!(ha.contains(name) && hb.contains(name), "{} isn't affected by both {} and {}", name, a.id, b.id);
                            prop_assert_eq!(Some(dir), case.units.iter().find(|u| &u.name == name).map(|u| &u.dir));
                        }
                    }
                    prop_assert_eq!(units > 0, !ha.is_disjoint(hb));
                    prop_assert!(d.to_string().starts_with(&format!("waits for {}: both ", a.id)), "{}", d);
                }
            }
        }
    }

    // R33: changes no two of which conflict land in any order as one class, with one tree, and
    // every unit the key it has with its own change alone (or the trunk's, when none affects it).
    let trunk_keys = keys(&case.units, &inputs, &case.trunk);
    let alone: Vec<BTreeMap<String, [u8; 32]>> = (0..n).map(|i| keys(&case.units, &inputs, &land(&case.trunk, &case.edits, &[i]))).collect();
    let own = |members: &[usize], u: &str| -> Result<Option<[u8; 32]>, TestCaseError> {
        let by: Vec<usize> = members.iter().copied().filter(|&i| hit.get(i).is_some_and(|h| h.contains(u))).collect();
        prop_assert!(by.len() < 2, "{} is affected by {:?}", u, by);
        Ok(match by.first() {
            Some(&i) => alone.get(i).and_then(|k| k.get(u)).copied(),
            None => trunk_keys.get(u).copied(),
        })
    };
    for mask in 1u32..(1 << n) {
        let members: Vec<usize> = (0..n).filter(|i| (mask >> i) & 1 == 1).collect();
        let commute = members.iter().all(|&i| members.iter().all(|&j| i == j || !meets(i, j)));
        let ids: Vec<String> = members.iter().map(|&i| id(i)).collect();
        if !commute {
            prop_assert!(q.class(&ids.iter().map(String::as_str).collect::<Vec<_>>()).is_err());
            continue;
        }
        let class = Class(ids.iter().cloned().collect());
        let tree = land(&case.trunk, &case.edits, &members);
        for order in orders(&members) {
            let named: Vec<String> = order.iter().map(|&i| id(i)).collect();
            prop_assert_eq!(q.class(&named.iter().map(String::as_str).collect::<Vec<_>>()), Ok(class.clone()));
            prop_assert_eq!(&land(&case.trunk, &case.edits, &order), &tree, "{:?} landed in another order", named);
        }
        let got = keys(&case.units, &inputs, &tree);
        for u in &case.units {
            prop_assert_eq!(got.get(&u.name).copied(), own(members.as_slice(), u.name.as_str())?, "{}'s key with {:?}", u.name, ids);
        }
        if members.len() > 1 {
            bump(&seen.commuting);
        }
    }

    // R32: a component landed whole, its changes touching no file in common, leaves every unit
    // but its joint units the key it has with its own change alone.
    for comp in &q.components {
        let members: Vec<usize> = (0..n).filter(|&i| q.changes.get(i).is_some_and(|c| comp.changes.contains(&c.id))).collect();
        let apart = members.iter().all(|&i| members.iter().all(|&j| i == j || case.queue.get(i).zip(case.queue.get(j)).is_some_and(|(a, b)| a.files.is_disjoint(&b.files))));
        if members.len() < 2 || !apart {
            continue;
        }
        let tree = land(&case.trunk, &case.edits, &members);
        let back: Vec<usize> = members.iter().rev().copied().collect();
        prop_assert_eq!(&land(&case.trunk, &case.edits, &back), &tree);
        let got = keys(&case.units, &inputs, &tree);
        for u in case.units.iter().filter(|u| !comp.joint.contains_key(&u.name)) {
            prop_assert_eq!(got.get(&u.name).copied(), own(members.as_slice(), u.name.as_str())?, "{}'s key with {:?}", u.name, comp.changes);
        }
        bump(&seen.whole);
    }
    Ok(())
}

#[test]
fn w25_ac30_commuting_changes() -> Result<(), String> {
    let seen = Seen::default();
    let mut runner = TestRunner::new(Config { cases: 10_000, failure_persistence: None, ..Config::default() });
    runner.run(&raw(), |r| check(&r, &seen)).map_err(|e| e.to_string())?;
    let (cases, commuting, whole) = (seen.cases.get(), seen.commuting.get(), seen.whole.get());
    println!("W25-AC30: {cases} cases; {commuting} sets of two or more commuting changes landed in every order checked; {whole} components landed whole");
    assert!(cases >= 10_000, "{cases} cases run");
    assert!(commuting >= 10_000, "only {commuting} sets of two or more commuting changes");
    assert!(whole >= 1_000, "only {whole} components landed whole");
    Ok(())
}
