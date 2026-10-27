//! Test results as units of work (docs/ci.md W24-R10). A test binary's run is keyed as Go keys its
//! test cache: by the binary's digest, its arguments, the environment, the files it read and the
//! platform ([`unit`], whose weak key the files it read make strong: [`key`], R04). Passes are kept
//! and replayed; failures never are ([`Ran::passed`]). Selection ([`select`]) runs the test binaries
//! of affected packages (W24-R17) whose digest changed, and replays the others' results. Flaky marks
//! (W16-R04) are kept on the same keys ([`Marks`]): a test that both passed and failed under one key
//! is flaky, since nothing it depends on differed between them.
//!
//! Pure: a runner reports the binaries it built and what they read; the server keeps the results
//! in the work ledger (`crates/kiln/src/ci/work.rs`) and answers what [`select`] asks through its
//! `found`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::key::{self, Key, Kind, PathSet, Platform, Unit};

/// One test binary of a run: `cargo test`'s or nextest's test executables, `go test -c`'s.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binary {
    /// Its package, as the package graph names it (`kiln-ci`).
    pub package: String,
    /// Which of the package's binaries: `lib` for its library's tests, an integration test's
    /// name (`plan_props`), a test binary's target name.
    pub name: String,
    /// What it's run with: filters, `--include-ignored`, a shard's partition.
    pub args: Vec<String>,
    /// SHA-256 of the executable; `None` when it wasn't built for this run (its package isn't
    /// affected), so only its last result can say what it would do.
    pub digest: Option<String>,
}

/// What every binary of a run shares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// The environment the binaries run with that a key keeps ([`key::relevant_env`]).
    pub env: BTreeMap<String, String>,
    pub platform: Platform,
    /// Its trust domain (W24-R21).
    pub trust: String,
}

/// A binary's unit (W24-R01): its package, name and arguments as its definition, its digest as
/// its one input, the environment, the platform and the trust domain. Its weak key is
/// [`key::weak`] of it; what it read makes the strong key ([`key`]).
pub fn unit(b: &Binary, cx: &Context) -> Unit {
    Unit {
        kind: Kind::Test,
        definition: definition(b),
        secrets: BTreeSet::new(),
        env: cx.env.clone(),
        platform: cx.platform.clone(),
        inputs: b.digest.iter().map(|d| ("binary".to_owned(), d.clone())).collect(),
        upstream: BTreeMap::new(),
        trust: cx.trust.clone(),
    }
}

/// Its weak key: [`unit`]'s.
pub fn weak(b: &Binary, cx: &Context) -> Key {
    key::weak(&unit(b, cx))
}

/// Its test key (R10): its weak key made strong by the files it read, which a pass is kept under
/// and flaky marks are counted on.
pub fn key(b: &Binary, cx: &Context, read: &PathSet) -> Key {
    key::strong(&weak(b, cx), read)
}

/// Its identity's key: its unit without its digest, which the newest pass of each binary is kept
/// under, so a binary not built (its package unaffected) can still be found.
pub fn identity(b: &Binary, cx: &Context) -> Key {
    key::weak(&Unit { inputs: BTreeMap::new(), ..unit(b, cx) })
}

fn definition(b: &Binary) -> String {
    serde_json::to_string(&(&b.package, &b.name, &b.args)).unwrap_or_default()
}

/// How one test of a binary went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Case {
    Passed,
    Failed,
    Skipped,
}

/// A test binary's run: how it exited, each of its tests, and how long it took.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ran {
    /// Its exit code; `None` when it didn't exit (killed, timed out).
    pub exit: Option<i32>,
    pub cases: BTreeMap<String, Case>,
    pub ms: u64,
}

impl Ran {
    /// Whether it passed: it exited 0 and none of its tests failed. Only passes are kept (R10): a
    /// failure, a crash and a timeout never are.
    pub fn passed(&self) -> bool {
        self.exit == Some(0) && !self.cases.values().any(|c| *c == Case::Failed)
    }
}

/// Flaky marks on a test key (W16-R04, on W24-R10's keys): each test's passes and failures under
/// one key, its binary, arguments, environment, platform and the files it read. A test that both
/// passed and failed there is flaky: nothing it depends on changed between them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Marks {
    /// Test → (passes, failures).
    pub cases: BTreeMap<String, (u32, u32)>,
}

impl Marks {
    /// Counts a run's tests.
    pub fn add(&mut self, ran: &Ran) {
        for (name, c) in &ran.cases {
            let e = self.cases.entry(name.clone()).or_default();
            match c {
                Case::Passed => e.0 = e.0.saturating_add(1),
                Case::Failed => e.1 = e.1.saturating_add(1),
                Case::Skipped => {}
            }
        }
    }

    /// The tests that both passed and failed.
    pub fn flaky(&self) -> BTreeSet<String> {
        self.cases.iter().filter(|(_, (p, f))| *p > 0 && *f > 0).map(|(n, _)| n.clone()).collect()
    }
}

/// A recorded pass, as selection replays it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Kept {
    pub ran: Ran,
    /// The binary's digest it passed with.
    pub digest: String,
    /// Where it ran.
    pub run: String,
    pub leg: String,
    /// Its key's flaky tests ([`Marks::flaky`]), said with the replay.
    pub flaky: BTreeSet<String>,
}

/// What the ledger has for a binary, as `found` answers [`select`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// A pass under its key (R04's two-phase lookup: its weak key, and what it read as it is
    /// now): only for a binary with its digest.
    pub by_key: Option<Kept>,
    /// The newest pass of its identity ([`identity`]), at whatever digest.
    pub last: Option<Kept>,
    /// Whether every file `last` read is as it was then: a binary not rebuilt replays `last` only
    /// then, since a test may read files outside its package (a doc it checks, a shared fixture).
    pub last_holds: bool,
}

/// What selection decided for a binary, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Build it if it isn't, and run it.
    Run { why: String },
    /// Its recorded pass stands for this run.
    Replay { from: Kept, why: String },
}

impl Choice {
    pub fn runs(&self) -> bool {
        matches!(self, Choice::Run { .. })
    }
}

/// Which binaries run and which replay (W24-R10), each saying why:
///
/// - a binary whose key has a pass (its digest, and what it read, unchanged) replays it;
/// - one whose key has none runs: its digest or what it read changed, or it never passed;
/// - one not built, of a package the change doesn't affect (`affected`, W24-R17), replays its
///   identity's newest pass while what that read holds; else, or of an affected package, it's
///   built and run.
///
/// A failure is never recorded, so a binary whose last run failed runs again.
pub fn select(binaries: &[Binary], affected: &BTreeSet<String>, found: &dyn Fn(&Binary) -> Found) -> Vec<(Binary, Choice)> {
    binaries
        .iter()
        .map(|b| {
            let f = found(b);
            let hit = affected.contains(&b.package);
            let holds = f.last_holds;
            let choice = match (&b.digest, f.by_key, f.last) {
                (Some(_), Some(k), _) => {
                    let why = if hit { format!("its package is affected, but its binary and what it read are as they were in run {}", k.run) } else { format!("unchanged since run {}", k.run) };
                    Choice::Replay { from: k, why }
                }
                (Some(d), None, Some(last)) if last.digest != *d => {
                    let why = if hit { format!("its package is affected and its binary changed since run {}", last.run) } else { format!("its binary changed since run {}, though its package isn't affected", last.run) };
                    Choice::Run { why }
                }
                (Some(_), None, Some(last)) => Choice::Run { why: format!("what it read changed since run {}", last.run) },
                (Some(_), None, None) => Choice::Run { why: "no pass is recorded for it".into() },
                (None, _, Some(last)) if !hit && holds => Choice::Replay { why: format!("its package isn't affected: as in run {}", last.run), from: last },
                (None, _, Some(last)) if !hit => Choice::Run { why: format!("what it read changed since run {}, though its package isn't affected", last.run) },
                (None, _, _) if hit => Choice::Run { why: "its package is affected".into() },
                (None, _, _) => Choice::Run { why: "no pass is recorded for it".into() },
            };
            (b.clone(), choice)
        })
        .collect()
}

/// A step's `tests:` (W24-R10): the test binaries one build makes, each run or replayed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    /// `cargo test`'s arguments for the build that makes them (`--workspace`, `-p kiln-ci`).
    pub cargo: Vec<String>,
}

impl Spec {
    /// The step's `tests:`, or why it can't be read: `{ cargo: --workspace }`, or a list of them.
    pub fn from_yval(v: &crate::wf::ir::YVal) -> Result<Spec, String> {
        use crate::wf::ir::YVal;
        let YVal::Map(m) = v else { return Err("`tests` is a mapping: `{ cargo: [--workspace] }`".into()) };
        let mut cargo = None;
        for (k, v) in m {
            match k.as_str() {
                "cargo" => {
                    let args = match v {
                        YVal::Null => Vec::new(),
                        YVal::Seq(items) => items.iter().map(|x| x.as_str().map(str::to_owned).ok_or("`tests.cargo`'s arguments are strings")).collect::<Result<Vec<String>, _>>()?,
                        other => other.as_str().map(|s| s.split_whitespace().map(str::to_owned).collect()).ok_or("`tests.cargo` is `cargo test`'s arguments: a string or a list")?,
                    };
                    cargo = Some(args);
                }
                other => return Err(format!("`tests.{other}` isn't one of `tests`'s keys (`cargo`)")),
            }
        }
        let cargo = cargo.ok_or("`tests` names the tool its binaries are built with: `{ cargo: [--workspace] }`")?;
        Ok(Spec { cargo })
    }
}

/// A test binary a build made: from cargo's `compiler-artifact` message of a test target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Built {
    /// Its package and name, as [`Binary`] names them.
    pub package: String,
    pub name: String,
    /// The executable, and its package's folder, where it runs.
    pub executable: String,
    pub dir: String,
}

/// The test binaries in `cargo test --no-run --message-format=json`'s messages, once each.
pub fn built(messages: &str) -> Vec<Built> {
    use serde_json::Value;
    let mut out: Vec<Built> = Vec::new();
    for line in messages.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("reason").and_then(Value::as_str) != Some("compiler-artifact") || !v.pointer("/profile/test").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let Some(executable) = v.get("executable").and_then(Value::as_str) else { continue };
        let target = v.get("target").cloned().unwrap_or_default();
        let kinds: Vec<&str> = target.get("kind").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let target_name = target.get("name").and_then(Value::as_str).unwrap_or_default();
        let name = if kinds.iter().any(|k| k.ends_with("lib") || *k == "proc-macro") { "lib".to_owned() } else { target_name.to_owned() };
        let manifest = v.get("manifest_path").and_then(Value::as_str).unwrap_or_default();
        let dir = manifest.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
        let package = package_of(v.get("package_id").and_then(Value::as_str).unwrap_or_default());
        let b = Built { package, name, executable: executable.to_owned(), dir };
        if !out.iter().any(|x| x.package == b.package && x.name == b.name) {
            out.push(b);
        }
    }
    out
}

/// A package's name from cargo's package id: `path+file:///…/kiln#kiln-ci@0.2.0`, or
/// `path+file:///…/kiln-ci#0.2.0` (its folder's name), or the older `kiln-ci 0.2.0 (path+…)`.
fn package_of(id: &str) -> String {
    match id.split_once('#') {
        Some((url, rest)) => match rest.split_once('@') {
            Some((name, _)) => name.to_owned(),
            None => url.trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_owned(),
        },
        None => id.split_whitespace().next().unwrap_or_default().to_owned(),
    }
}

/// Each test's outcome in libtest's output: `test name ... ok`, `FAILED`, `ignored`.
pub fn libtest(out: &str) -> BTreeMap<String, Case> {
    out.lines()
        .filter_map(|l| {
            let rest = l.trim_end().strip_prefix("test ")?;
            let (name, outcome) = rest.rsplit_once(" ... ")?;
            let case = match outcome.split([' ', ',']).next()? {
                "ok" => Case::Passed,
                "FAILED" => Case::Failed,
                "ignored" => Case::Skipped,
                _ => return None,
            };
            Some((name.to_owned(), case))
        })
        .collect()
}

#[cfg(test)]
mod cases {
    use super::*;
    use crate::work::graph;
    use crate::work::key::Read;

    /// A ledger in memory, as the server keeps it: passes under their test keys with each weak
    /// key's path sets, each identity's newest pass with what it read, and marks under test keys.
    #[derive(Default)]
    struct Ledger {
        sets: BTreeMap<Key, Vec<PathSet>>,
        results: BTreeMap<Key, Kept>,
        last: BTreeMap<Key, (Kept, PathSet)>,
        marks: BTreeMap<Key, Marks>,
    }

    impl Ledger {
        fn record(&mut self, b: &Binary, cx: &Context, read: &PathSet, ran: &Ran, run: &str) {
            let k = key(b, cx, read);
            let marks = self.marks.entry(k).or_default();
            marks.add(ran);
            if !ran.passed() {
                return;
            }
            let kept = Kept { ran: ran.clone(), digest: b.digest.clone().unwrap_or_default(), run: run.into(), leg: format!("{run}/{}", b.package), flaky: marks.flaky() };
            self.sets.entry(weak(b, cx)).or_default().push(read.clone());
            self.results.insert(k, kept.clone());
            self.last.insert(identity(b, cx), (kept, read.clone()));
        }

        /// `now` gives what each path holds now.
        fn found(&self, b: &Binary, cx: &Context, now: &dyn Fn(&str) -> Read) -> Found {
            let current = |set: &PathSet| -> PathSet { set.keys().map(|p| (p.clone(), now(p))).collect() };
            let by_key = b.digest.as_ref().and_then(|_| {
                self.sets.get(&weak(b, cx)).into_iter().flatten().find_map(|set| {
                    let k = key(b, cx, &current(set));
                    let flaky = self.marks.get(&k).map(Marks::flaky).unwrap_or_default();
                    self.results.get(&k).map(|kept| Kept { flaky, ..kept.clone() })
                })
            });
            let last = self.last.get(&identity(b, cx));
            Found { by_key, last_holds: last.is_some_and(|(_, read)| current(read) == *read), last: last.map(|(k, _)| k.clone()) }
        }
    }

    fn cx() -> Context {
        Context { env: BTreeMap::from([("RUST_TEST_THREADS".to_owned(), "4".to_owned())]), platform: Platform { os: "linux".into(), arch: "x86_64".into(), ..Platform::default() }, trust: "trusted".into() }
    }

    fn bin(package: &str, digest: &str) -> Binary {
        Binary { package: package.into(), name: "lib".into(), args: Vec::new(), digest: Some(digest.into()) }
    }

    fn passed(tests: &[&str]) -> Ran {
        Ran { exit: Some(0), cases: tests.iter().map(|t| ((*t).to_owned(), Case::Passed)).collect(), ms: 1_000 }
    }

    fn reads(package: &str) -> PathSet {
        PathSet::from([(format!("crates/{package}/tests/fixture.txt"), Read::File(format!("{package}-fixture")))])
    }

    /// What each fixture holds, as [`reads`] recorded it.
    fn same(p: &str) -> Read {
        Read::File(format!("{}-fixture", p.split('/').nth(1).unwrap_or_default()))
    }

    /// Three crates: `b` depends on `a`; `c` stands alone.
    fn workspace() -> graph::Graph {
        let files = BTreeMap::from([
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
            ("crates/b/Cargo.toml", "[package]\nname = \"b\"\n\n[dependencies]\na = { path = \"../a\" }\n"),
            ("crates/c/Cargo.toml", "[package]\nname = \"c\"\n"),
        ]);
        let paths: Vec<String> = files.keys().map(|p| (*p).to_owned()).collect();
        graph::read(&|p| files.get(p).map(|t| (*t).to_owned()), &paths)
    }

    fn running(choices: &[(Binary, Choice)]) -> Vec<&str> {
        choices.iter().filter(|(_, c)| c.runs()).map(|(b, _)| b.package.as_str()).collect()
    }

    fn replaying(choices: &[(Binary, Choice)]) -> Vec<&str> {
        choices.iter().filter(|(_, c)| !c.runs()).map(|(b, _)| b.package.as_str()).collect()
    }

    fn why(c: &Choice) -> &str {
        match c {
            Choice::Run { why } | Choice::Replay { why, .. } => why,
        }
    }

    /// W24-R10: changing one crate runs only the test binaries it affects; the rest replay. A
    /// change to `c` changes only `c`'s binary; a change to `a` changes `a`'s and `b`'s, which
    /// links `a`; the untouched replay their passes, naming the run they came from.
    #[test]
    fn w24_r10_changing_one_crate_runs_only_its_affected_test_binaries() {
        let (cx, g) = (cx(), workspace());
        let mut ledger = Ledger::default();
        let first = [bin("a", "a1"), bin("b", "b1"), bin("c", "c1")];
        let all: BTreeSet<String> = ["a", "b", "c"].map(str::to_owned).into();
        let choices = select(&first, &all, &|b| ledger.found(b, &cx, &same));
        assert_eq!(running(&choices), ["a", "b", "c"], "nothing recorded yet");
        for b in &first {
            ledger.record(b, &cx, &reads(&b.package), &passed(&["t1", "t2"]), "r1");
        }
        // `c` changes: only its binary is rebuilt differently.
        let affected = graph::affected(&g, graph::Ecosystem::Cargo, &["crates/c/src/lib.rs".to_owned()]);
        assert_eq!(affected.iter().map(String::as_str).collect::<Vec<_>>(), ["c"]);
        let second = [bin("a", "a1"), bin("b", "b1"), bin("c", "c2")];
        let choices = select(&second, &affected, &|b| ledger.found(b, &cx, &same));
        assert_eq!(running(&choices), ["c"]);
        assert_eq!(replaying(&choices), ["a", "b"]);
        let said: Vec<&str> = choices.iter().map(|(_, c)| why(c)).collect();
        assert_eq!(said, ["unchanged since run r1", "unchanged since run r1", "its package is affected and its binary changed since run r1"]);
        for b in second.iter().filter(|b| b.package == "c") {
            ledger.record(b, &cx, &reads("c"), &passed(&["t1", "t2"]), "r2");
        }
        // `a` changes: `a` and `b` (which links it) run; `c` replays run 2's pass.
        let affected = graph::affected(&g, graph::Ecosystem::Cargo, &["crates/a/src/lib.rs".to_owned()]);
        let third = [bin("a", "a2"), bin("b", "b2"), bin("c", "c2")];
        let choices = select(&third, &affected, &|b| ledger.found(b, &cx, &same));
        assert_eq!(running(&choices), ["a", "b"]);
        assert!(matches!(choices.get(2), Some((_, Choice::Replay { from, .. })) if from.run == "r2"), "{choices:?}");
        // Only the affected packages' binaries built: the others replay their last passes.
        let fourth = [Binary { digest: None, ..bin("a", "") }, Binary { digest: None, ..bin("b", "") }, bin("c", "c3")];
        let affected = graph::affected(&g, graph::Ecosystem::Cargo, &["crates/c/src/lib.rs".to_owned()]);
        let choices = select(&fourth, &affected, &|b| ledger.found(b, &cx, &same));
        assert_eq!((running(&choices), replaying(&choices)), (vec!["c"], vec!["a", "b"]));
        // A file a binary not rebuilt read changed (outside its package, which the graph can't
        // see): it's built and run, not replayed.
        let moved = |p: &str| if p.starts_with("crates/a/") { Read::File("edited".into()) } else { same(p) };
        let choices = select(&fourth, &affected, &|b| ledger.found(b, &cx, &moved));
        assert_eq!(running(&choices), ["a", "c"]);
        assert_eq!(choices.first().map(|(_, c)| why(c)), Some("what it read changed since run r1, though its package isn't affected"));
    }

    /// W24-R10: failures are never kept, so a binary whose run failed runs again even unchanged;
    /// a file it read that changed runs it too, though its binary didn't.
    #[test]
    fn w24_r10_failures_are_never_replayed_and_what_it_read_counts() {
        let cx = cx();
        let mut ledger = Ledger::default();
        let c = bin("c", "c1");
        let failed = Ran { exit: Some(101), cases: BTreeMap::from([("t1".to_owned(), Case::Passed), ("t2".to_owned(), Case::Failed)]), ms: 500 };
        assert!(!failed.passed() && !Ran { exit: None, ..passed(&["t1"]) }.passed(), "a failure, and a run that never exited");
        ledger.record(&c, &cx, &reads("c"), &failed, "r1");
        let none = BTreeSet::new();
        let choices = select(std::slice::from_ref(&c), &none, &|b| ledger.found(b, &cx, &same));
        assert_eq!(choices.first().map(|(_, ch)| ch.clone()), Some(Choice::Run { why: "no pass is recorded for it".into() }));
        ledger.record(&c, &cx, &reads("c"), &passed(&["t1", "t2"]), "r2");
        let choices = select(std::slice::from_ref(&c), &none, &|b| ledger.found(b, &cx, &same));
        assert!(matches!(choices.first(), Some((_, Choice::Replay { from, .. })) if from.run == "r2"));
        // Its fixture changes: what it read differs, so it runs.
        let changed = |_: &str| Read::File("edited".into());
        let choices = select(std::slice::from_ref(&c), &none, &|b| ledger.found(b, &cx, &changed));
        assert_eq!(choices.first().map(|(_, ch)| ch.clone()), Some(Choice::Run { why: "what it read changed since run r2".into() }));
    }

    /// W16-R04 on W24-R10's keys: a test that failed and then passed with nothing it depends on
    /// changed is flaky, and its replay says so; the same test on another binary, or having read
    /// other files, isn't.
    #[test]
    fn w16_r04_flaky_marks_on_test_keys() {
        let cx = cx();
        let mut ledger = Ledger::default();
        let c = bin("c", "c1");
        let failed = Ran { exit: Some(101), cases: BTreeMap::from([("t1".to_owned(), Case::Passed), ("t2".to_owned(), Case::Failed)]), ms: 500 };
        ledger.record(&c, &cx, &reads("c"), &failed, "r1");
        ledger.record(&c, &cx, &reads("c"), &passed(&["t1", "t2"]), "r2");
        let found = ledger.found(&c, &cx, &same);
        assert_eq!(found.by_key.map(|k| k.flaky), Some(BTreeSet::from(["t2".to_owned()])));
        let rebuilt = bin("c", "c2");
        ledger.record(&rebuilt, &cx, &reads("c"), &passed(&["t1", "t2"]), "r3");
        assert!(ledger.found(&rebuilt, &cx, &same).by_key.is_some_and(|k| k.flaky.is_empty()), "another binary is another key");
        // It failed having read another fixture, then passed with its own: not flaky, the fixture
        // differed.
        let d = bin("d", "d1");
        let other = PathSet::from([("crates/d/tests/fixture.txt".to_owned(), Read::File("broken".into()))]);
        ledger.record(&d, &cx, &other, &failed, "r4");
        ledger.record(&d, &cx, &reads("d"), &passed(&["t1", "t2"]), "r5");
        assert!(ledger.found(&d, &cx, &same).by_key.is_some_and(|k| k.flaky.is_empty() && k.run == "r5"));
    }

    /// Each part of a binary's key counts: its digest, its arguments, its environment, its
    /// platform, its trust domain and the files it read; its identity leaves out only the digest.
    #[test]
    fn w24_r10_what_keys_a_test_binary() {
        let cx = cx();
        let b = bin("c", "c1");
        let base = weak(&b, &cx);
        assert_ne!(base, weak(&bin("c", "c2"), &cx), "its digest");
        assert_ne!(base, weak(&Binary { args: vec!["--include-ignored".into()], ..b.clone() }, &cx), "its arguments");
        assert_ne!(base, weak(&b, &Context { env: BTreeMap::new(), ..cx.clone() }), "its environment");
        assert_ne!(base, weak(&b, &Context { platform: Platform { arch: "aarch64".into(), ..cx.platform.clone() }, ..cx.clone() }), "its platform");
        assert_ne!(base, weak(&b, &Context { trust: "untrusted".into(), ..cx.clone() }), "its trust domain");
        assert_eq!(identity(&b, &cx), identity(&bin("c", "c2"), &cx), "an identity is the same at any digest");
        assert_ne!(identity(&b, &cx), base);
        let read = PathSet::from([("crates/c/data.txt".to_owned(), Read::File("d1".into()))]);
        let other = PathSet::from([("crates/c/data.txt".to_owned(), Read::File("d2".into()))]);
        assert_ne!(key(&b, &cx, &read), key(&b, &cx, &other), "the files it read");
        assert_eq!(key(&b, &cx, &read), key::strong(&base, &read));
    }

    /// W24-R10: a step's `tests:`, as written and refused.
    #[test]
    fn w24_r10_a_steps_tests() {
        use crate::wf::ir::YVal;
        let s = |x: &str| YVal::Str(crate::wf::ir::Tpl::new(x, Default::default()));
        let one = YVal::Map(vec![("cargo".into(), s("--workspace --locked"))]);
        assert_eq!(Spec::from_yval(&one), Ok(Spec { cargo: vec!["--workspace".into(), "--locked".into()] }));
        let list = YVal::Map(vec![("cargo".into(), YVal::Seq(vec![s("-p"), s("kiln-ci")]))]);
        assert_eq!(Spec::from_yval(&list).map(|x| x.cargo), Ok(vec!["-p".to_owned(), "kiln-ci".to_owned()]));
        assert!(Spec::from_yval(&YVal::Map(vec![("go".into(), s("./..."))])).is_err());
        assert!(Spec::from_yval(&YVal::Map(Vec::new())).is_err());
        assert!(Spec::from_yval(&s("cargo")).is_err());
    }

    /// W24-R10: the test binaries a cargo build reports, and libtest's outcomes.
    #[test]
    fn w24_r10_binaries_built_and_outcomes() {
        let msgs = [
            r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/a#0.1.0","manifest_path":"/w/crates/a/Cargo.toml","target":{"kind":["lib"],"name":"a"},"profile":{"test":true},"executable":"/w/target/debug/deps/a-1"}"#,
            r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/b#b-pkg@0.1.0","manifest_path":"/w/crates/b/Cargo.toml","target":{"kind":["test"],"name":"props"},"profile":{"test":true},"executable":"/w/target/debug/deps/props-2"}"#,
            r#"{"reason":"compiler-artifact","package_id":"c 0.1.0 (path+file:///w/c)","manifest_path":"/w/c/Cargo.toml","target":{"kind":["lib"],"name":"c"},"profile":{"test":false},"executable":null}"#,
            r#"{"reason":"build-finished","success":true}"#,
            "not json",
        ]
        .join("\n");
        let got = built(&msgs);
        assert_eq!(got.iter().map(|b| (b.package.as_str(), b.name.as_str(), b.dir.as_str())).collect::<Vec<_>>(), vec![("a", "lib", "/w/crates/a"), ("b-pkg", "props", "/w/crates/b")]);
        assert_eq!(package_of("kiln-ci 0.2.0 (path+file:///w/kiln-ci)"), "kiln-ci");
        let out = "running 3 tests\ntest a::one ... ok\ntest a::two ... FAILED\ntest slow ... ignored, needs a GPU\n\ntest result: FAILED. 1 passed; 1 failed";
        let cases = libtest(out);
        assert_eq!(cases.get("a::one"), Some(&Case::Passed));
        assert_eq!(cases.get("a::two"), Some(&Case::Failed));
        assert_eq!(cases.get("slow"), Some(&Case::Skipped));
        assert_eq!(cases.len(), 3);
    }
}
