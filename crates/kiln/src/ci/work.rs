//! W24's work ledger (docs/ci.md W24-R02, R04): what units of work produced, recorded once under
//! their strong keys ([`kiln_ci::work::key`]) and found again by two-phase lookup. Rows are shaped
//! as the Records seam's (family, key, value) in `work_records`, so the ledger can move onto
//! Records (Amphora) unchanged. Test binaries' results (W24-R10) go through the same path
//! ([`record_test`], [`found_test`]).

use std::collections::{BTreeMap, BTreeSet};

use kiln_ci::work::key::{self, Key, PathSet};
use kiln_ci::work::tests as test_results;
use kiln_ci::work::tests::{Binary, Choice, Context, Found, Kept, Marks, Ran};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

/// The ledger's families (W24-R02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// A strong key's result.
    Result,
    /// A weak key's path sets.
    Weak,
    /// A path set, by its digest.
    PathSet,
    /// A warm workspace's snapshot (W24-R12).
    Snapshot,
    /// A lane's newest snapshot (W24-R11).
    Lane,
    /// Test keys' flaky marks, and each test binary's newest pass (W24-R10).
    Test,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::Result => "work.result",
            Family::Weak => "work.weak",
            Family::PathSet => "work.pathset",
            Family::Snapshot => "work.snapshot",
            Family::Lane => "work.lane",
            Family::Test => "work.test",
        }
    }
}

/// The most path sets a weak key keeps, newest first (W24-R04).
pub const PATH_SETS: usize = 8;

/// Where an entry belongs: its repository and trust domain (W24-R21).
pub fn scope(repo: &str, domain: &str) -> String {
    format!("{repo}|{domain}|")
}

/// What a unit produced (W24-R02).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recorded {
    pub outcome: String,
    pub exit: Option<i32>,
    /// Outputs by name, as content digests (files and trees in the content store) or values.
    pub outputs: BTreeMap<String, String>,
    /// Its log, by digest.
    pub logs: Option<String>,
    /// Its test reports, by digest.
    pub reports: Vec<String>,
    pub provenance: Provenance,
    /// The trust domain that wrote it, and those its inputs came from (W24-R21).
    pub domain: String,
    pub lineage: Vec<String>,
    /// The runners that reproduced it (W24-R23), and its signatures (W24-R22).
    pub verified_by: Vec<String>,
    pub signatures: Vec<String>,
}

/// Where and how a result was made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Provenance {
    pub repo: String,
    pub run: String,
    pub leg: String,
    pub step: Option<u32>,
    pub attempt: u32,
    pub runner: String,
    pub commit: String,
    pub tree: String,
    pub ms: u64,
    pub cpu_ms: u64,
    /// When it finished, in seconds since the epoch.
    pub finished_at: i64,
}

/// A row, written or replaced.
pub fn put(c: &Connection, family: Family, key: &str, value: &[u8], at: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO work_records (family, key, value, at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (family, key) DO UPDATE SET value = excluded.value, at = excluded.at",
        params![family.name(), key, value, at],
    )?;
    Ok(())
}

/// A row's value.
pub fn get(c: &Connection, family: Family, key: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    c.query_row("SELECT value FROM work_records WHERE family = ?1 AND key = ?2", params![family.name(), key], |r| r.get(0)).optional()
}

/// A row gone.
pub fn delete(c: &Connection, family: Family, key: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM work_records WHERE family = ?1 AND key = ?2", params![family.name(), key])?;
    Ok(())
}

/// The rows whose keys start with `prefix`, highest `at` first.
pub fn scan(c: &Connection, family: Family, prefix: &str) -> rusqlite::Result<Vec<(String, Vec<u8>, i64)>> {
    let mut q = c.prepare_cached("SELECT key, value, at FROM work_records WHERE family = ?1 AND key >= ?2 AND key < ?3 ORDER BY at DESC, key")?;
    let rows = q.query_map(params![family.name(), prefix, format!("{prefix}\u{10FFFF}")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

fn weak_prefix(scope: &str, weak: &Key) -> String {
    format!("{scope}{}|", weak.hex())
}

/// A weak key's path sets, by digest, newest first.
pub fn path_sets(c: &Connection, scope: &str, weak: &Key) -> rusqlite::Result<Vec<String>> {
    Ok(scan(c, Family::Weak, &weak_prefix(scope, weak))?.into_iter().map(|(_, id, _)| String::from_utf8_lossy(&id).into_owned()).collect())
}

/// Records what a unit produced (W24-R02): its path set kept by digest and listed under its weak
/// key (the newest [`PATH_SETS`] kept), its result under its strong key. Only successes are to be
/// recorded (W24-R07). Returns the strong key.
pub fn record(c: &Connection, scope: &str, weak: &Key, read: &PathSet, result: &Recorded) -> anyhow::Result<Key> {
    let now = crate::util::now_unix();
    let strong = key::strong(weak, read);
    let set = serde_json::to_vec(read)?;
    let id = crate::util::sha256_hex(&set);
    put(c, Family::PathSet, &id, &set, now)?;
    let prefix = weak_prefix(scope, weak);
    let listed = scan(c, Family::Weak, &prefix)?;
    let next = listed.first().map_or(1, |(_, _, at)| at + 1);
    put(c, Family::Weak, &format!("{prefix}{id}"), id.as_bytes(), next)?;
    for (old, _, _) in scan(c, Family::Weak, &prefix)?.iter().skip(PATH_SETS) {
        delete(c, Family::Weak, old)?;
    }
    put(c, Family::Result, &format!("{scope}{}", strong.hex()), &serde_json::to_vec(result)?, now)?;
    Ok(strong)
}

/// Two-phase lookup (W24-R04): the weak key's path sets, newest first, each made a strong key from
/// what is at its paths now (`now` reads them), and the first with a recorded result.
pub fn lookup(c: &Connection, scope: &str, weak: &Key, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<Option<(Key, Recorded)>> {
    for id in path_sets(c, scope, weak)? {
        let Some(set) = get(c, Family::PathSet, &id)? else { continue };
        let recorded: PathSet = serde_json::from_slice(&set)?;
        let strong = key::strong(weak, &now(&recorded));
        if let Some(found) = get(c, Family::Result, &format!("{scope}{}", strong.hex()))? {
            return Ok(Some((strong, serde_json::from_slice(&found)?)));
        }
    }
    Ok(None)
}

// ------------------------------------------------------------------ test results (W24-R10)

fn marks_at(scope: &str, key: &Key) -> String {
    format!("{scope}marks|{}", key.hex())
}

fn last_at(scope: &str, b: &Binary, cx: &Context) -> String {
    format!("{scope}last|{}", test_results::identity(b, cx).hex())
}

fn marks(c: &Connection, scope: &str, key: &Key) -> anyhow::Result<Marks> {
    Ok(get(c, Family::Test, &marks_at(scope, key))?.map(|v| serde_json::from_slice(&v)).transpose()?.unwrap_or_default())
}

/// Records a test binary's run (W24-R10) through the ledger's one path: a pass goes in by
/// [`record`], under the binary's weak key and what it read (its test key,
/// [`kiln_ci::work::tests::key`]), so [`lookup`] finds it only while its digest, arguments,
/// environment, platform and every file it read are as they were; a failure, a crash or a timeout
/// never does. Every run counts towards its test key's flaky marks (W16-R04), and a pass is kept as
/// its binary's identity's newest, with what it read, for a run that doesn't rebuild it. Returns the
/// test key of a pass.
pub fn record_test(c: &Connection, scope: &str, b: &Binary, cx: &Context, read: &PathSet, ran: &Ran, provenance: &Provenance) -> anyhow::Result<Option<Key>> {
    let now = crate::util::now_unix();
    let at = test_results::key(b, cx, read);
    let mut counted = marks(c, scope, &at)?;
    counted.add(ran);
    put(c, Family::Test, &marks_at(scope, &at), &serde_json::to_vec(&counted)?, now)?;
    if !ran.passed() {
        return Ok(None);
    }
    let kept = Kept { ran: ran.clone(), digest: b.digest.clone().unwrap_or_default(), run: provenance.run.clone(), leg: provenance.leg.clone(), flaky: counted.flaky() };
    let result = Recorded {
        outcome: "success".into(),
        exit: ran.exit,
        outputs: BTreeMap::from([("test".to_owned(), serde_json::to_string(&kept)?)]),
        provenance: Provenance { ms: ran.ms, ..provenance.clone() },
        domain: cx.trust.clone(),
        ..Recorded::default()
    };
    let strong = record(c, scope, &test_results::weak(b, cx), read, &result)?;
    put(c, Family::Test, &last_at(scope, b, cx), &serde_json::to_vec(&(&kept, read))?, now)?;
    Ok(Some(strong))
}

/// What the ledger has for a test binary (W24-R10), as [`kiln_ci::work::tests::select`] asks: a
/// pass under its test key by two-phase lookup (only for a binary with its digest), with that key's
/// flaky tests as they are now; and its identity's newest pass, with whether what that read holds.
/// `now` gives a path set as it is now.
pub fn found_test(c: &Connection, scope: &str, b: &Binary, cx: &Context, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<Found> {
    let mut by_key = None;
    if b.digest.is_some()
        && let Some((strong, found)) = lookup(c, scope, &test_results::weak(b, cx), now)?
        && let Some(kept) = found.outputs.get("test").map(|t| serde_json::from_str::<Kept>(t)).transpose()?
    {
        by_key = Some(Kept { flaky: marks(c, scope, &strong)?.flaky(), ..kept });
    }
    let last: Option<(Kept, PathSet)> = get(c, Family::Test, &last_at(scope, b, cx))?.map(|v| serde_json::from_slice(&v)).transpose()?;
    let last_holds = last.as_ref().is_some_and(|(_, read)| now(read) == *read);
    Ok(Found { by_key, last: last.map(|(k, _)| k), last_holds })
}

/// Which of a run's test binaries run and which replay their passes (W24-R10), each saying why:
/// [`kiln_ci::work::tests::select`] over what the ledger has for each ([`found_test`]).
pub fn select_tests(c: &Connection, scope: &str, binaries: &[Binary], cx: &Context, affected: &BTreeSet<String>, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<Vec<(Binary, Choice)>> {
    let found: Vec<Found> = binaries.iter().map(|b| found_test(c, scope, b, cx, now)).collect::<anyhow::Result<_>>()?;
    let answer = |b: &Binary| binaries.iter().position(|x| x == b).and_then(|i| found.get(i)).cloned().unwrap_or_default();
    Ok(test_results::select(binaries, affected, &answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::work::key::{Kind, Platform, Read, Unit};
    use std::collections::BTreeSet;

    fn unit(n: u64) -> Unit {
        Unit {
            kind: Kind::Step,
            definition: format!("cargo test -p crate{n}"),
            secrets: BTreeSet::new(),
            env: BTreeMap::new(),
            platform: Platform { os: "linux".into(), arch: "x86_64".into(), ..Platform::default() },
            inputs: BTreeMap::from([("crates/**".to_owned(), format!("tree{n}"))]),
            upstream: BTreeMap::new(),
            trust: "trusted".into(),
        }
    }

    fn reads(n: u64, variant: u64) -> PathSet {
        (0..6).map(|i| (format!("src/f{n}_{i}.rs"), Read::File(format!("d{n}_{i}_{variant}")))).collect()
    }

    fn ok(run: &str) -> Recorded {
        Recorded { outcome: "success".into(), exit: Some(0), provenance: Provenance { run: run.into(), ..Provenance::default() }, domain: "trusted".into(), ..Recorded::default() }
    }

    #[test]
    fn w24_r02_a_result_is_found_while_what_it_read_is_unchanged() -> anyhow::Result<()> {
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let (trusted, untrusted) = (scope("acme/app", "trusted"), scope("acme/app", "untrusted"));
                let weak = key::weak(&unit(1));
                let strong = record(c, &trusted, &weak, &reads(1, 0), &ok("r1"))?;
                let same = |p: &PathSet| p.clone();
                let hit = lookup(c, &trusted, &weak, &same)?;
                anyhow::ensure!(hit.as_ref().map(|(k, r)| (*k, r.provenance.run.as_str())) == Some((strong, "r1")), "{hit:?}");
                // A file it read changed: no result for that.
                let changed = |p: &PathSet| -> PathSet { p.keys().map(|k| (k.clone(), Read::File("changed".into()))).collect() };
                anyhow::ensure!(lookup(c, &trusted, &weak, &changed)?.is_none());
                // Another domain's ledger holds nothing of it (W24-R21).
                anyhow::ensure!(lookup(c, &untrusted, &weak, &same)?.is_none());
                // Another unit: nothing.
                anyhow::ensure!(lookup(c, &trusted, &key::weak(&unit(2)), &same)?.is_none());
                Ok(())
            })
            .await
        })
    }

    #[test]
    fn w24_r04_a_weak_key_keeps_its_newest_eight_path_sets() -> anyhow::Result<()> {
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let sc = scope("acme/app", "trusted");
                let weak = key::weak(&unit(1));
                let ids: Vec<String> = (0..10).map(|v| crate::util::sha256_hex(&serde_json::to_vec(&reads(1, v)).unwrap_or_default())).collect();
                for v in 0..10 {
                    record(c, &sc, &weak, &reads(1, v), &ok(&format!("r{v}")))?;
                }
                let kept = path_sets(c, &sc, &weak)?;
                anyhow::ensure!(kept.len() == PATH_SETS, "{kept:?}");
                let newest: Vec<String> = ids.iter().rev().take(PATH_SETS).cloned().collect();
                anyhow::ensure!(kept == newest, "newest first: {kept:?}");
                // Recorded again, an older path set is newest again, without a second listing.
                record(c, &sc, &weak, &reads(1, 3), &ok("again"))?;
                let again = path_sets(c, &sc, &weak)?;
                anyhow::ensure!(again.len() == PATH_SETS && again.first() == ids.get(3), "{again:?}");
                Ok(())
            })
            .await
        })
    }

    /// W24-AC10: ledger lookups take p99 under 5 ms on the server: 20,000 results under 5,000
    /// weak keys (4 path sets each) in a store on disk, looked up 5,000 times through the store's
    /// own calls, as dispatch would.
    #[test]
    fn w24_ac10_lookup_p99_under_5ms() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let s = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        let units: u64 = 5_000;
        crate::rt::block_on(async move {
            s.call(move |c| {
                let tx = c.transaction()?;
                let sc = scope("acme/app", "trusted");
                for n in 0..units {
                    let weak = key::weak(&unit(n));
                    for v in 0..4 {
                        record(&tx, &sc, &weak, &reads(n, v), &ok(&format!("r{n}_{v}")))?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
            let mut took = Vec::new();
            for i in 0..units {
                let n = (i * 7_919) % units;
                let started = std::time::Instant::now();
                let hit = s
                    .call(move |c| {
                        let same = |p: &PathSet| p.clone();
                        lookup(c, &scope("acme/app", "trusted"), &key::weak(&unit(n)), &same)
                    })
                    .await?;
                took.push(started.elapsed());
                anyhow::ensure!(hit.is_some(), "unit {n} has results");
            }
            took.sort();
            let at = |q: f64| took.get(((took.len() as f64 * q).ceil() as usize).saturating_sub(1)).copied().unwrap_or_default();
            let (p50, p99) = (at(0.50), at(0.99));
            println!("w24_ac10: {} lookups, p50 {:?}, p99 {:?}", took.len(), p50, p99);
            anyhow::ensure!(p99 < std::time::Duration::from_millis(5), "p99 {p99:?} is 5 ms or more");
            Ok(())
        })
    }

    /// W24-R10 through the ledger's one path: a test binary's pass is found again under its test
    /// key while its digest and what it read hold, and replayed; a failure never is, though it
    /// counts towards the key's flaky marks; a binary its change didn't rebuild replays its newest
    /// pass while what that read holds; another domain's ledger has none of it.
    #[test]
    fn w24_r10_test_results_through_the_ledger() -> anyhow::Result<()> {
        use kiln_ci::work::tests::Case;
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let sc = scope("acme/app", "trusted");
                let cx = Context { env: BTreeMap::from([("RUST_TEST_THREADS".to_owned(), "4".to_owned())]), platform: Platform { os: "linux".into(), arch: "x86_64".into(), ..Platform::default() }, trust: "trusted".into() };
                let bin = |p: &str, d: Option<&str>| Binary { package: p.into(), name: "lib".into(), args: Vec::new(), digest: d.map(str::to_owned) };
                let read = |p: &str| PathSet::from([(format!("crates/{p}/tests/data.txt"), Read::File(format!("{p}-data")))]);
                let ran = |t2: Case, exit: i32| Ran { exit: Some(exit), cases: BTreeMap::from([("t1".to_owned(), Case::Passed), ("t2".to_owned(), t2)]), ms: 1_500 };
                let at = |run: &str| Provenance { repo: "acme/app".into(), run: run.into(), leg: format!("{run}-test"), ..Provenance::default() };
                let same = |p: &PathSet| p.clone();
                let (a, b) = (bin("a", Some("a1")), bin("b", Some("b1")));
                // A failure isn't kept, but counts: t2 failed, then passed with nothing changed.
                anyhow::ensure!(record_test(c, &sc, &a, &cx, &read("a"), &ran(Case::Failed, 101), &at("r1"))?.is_none());
                let key = record_test(c, &sc, &a, &cx, &read("a"), &ran(Case::Passed, 0), &at("r2"))?;
                anyhow::ensure!(key == Some(test_results::key(&a, &cx, &read("a"))), "kept under its test key: {key:?}");
                record_test(c, &sc, &b, &cx, &read("b"), &ran(Case::Passed, 0), &at("r2"))?;
                let found = found_test(c, &sc, &a, &cx, &same)?;
                let kept = found.by_key.clone().ok_or_else(|| anyhow::anyhow!("no pass under its key"))?;
                anyhow::ensure!(kept.run == "r2" && kept.leg == "r2-test" && kept.digest == "a1" && kept.ran.ms == 1_500, "{kept:?}");
                anyhow::ensure!(kept.flaky == BTreeSet::from(["t2".to_owned()]), "flaky on its key: {:?}", kept.flaky);
                anyhow::ensure!(found.last.is_some() && found.last_holds);
                // b's data changes, and a isn't rebuilt (its package isn't affected): b runs, a
                // replays its newest pass.
                let edited = |p: &PathSet| -> PathSet { p.iter().map(|(k, v)| (k.clone(), if k.starts_with("crates/b/") { Read::File("edited".into()) } else { v.clone() })).collect() };
                let affected = BTreeSet::from(["b".to_owned()]);
                let choices = select_tests(c, &sc, &[bin("a", None), b.clone()], &cx, &affected, &edited)?;
                let said: Vec<(String, bool, String)> = choices.iter().map(|(x, ch)| (x.package.clone(), ch.runs(), match ch { Choice::Run { why } | Choice::Replay { why, .. } => why.clone() })).collect();
                anyhow::ensure!(
                    said == [("a".to_owned(), false, "its package isn't affected: as in run r2".to_owned()), ("b".to_owned(), true, "what it read changed since run r2".to_owned())],
                    "{said:?}"
                );
                // A failed run of b is never kept: b runs again next time too.
                anyhow::ensure!(record_test(c, &sc, &b, &cx, &edited(&read("b")), &ran(Case::Failed, 101), &at("r3"))?.is_none());
                anyhow::ensure!(found_test(c, &sc, &b, &cx, &edited)?.by_key.is_none());
                // Another domain's ledger holds none of it (W24-R21).
                let theirs = found_test(c, &scope("acme/app", "untrusted"), &a, &cx, &same)?;
                anyhow::ensure!(theirs.by_key.is_none() && theirs.last.is_none(), "{theirs:?}");
                Ok(())
            })
            .await
        })
    }
}
