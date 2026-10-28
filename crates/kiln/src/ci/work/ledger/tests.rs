//! The ledger on each of its homes (docs/ci.md W24-R02): kiln's `work_records` table, kiln's own
//! Records durable in its store, and the simulated Records. The one path decides alike on each.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use anyhow::Context as _;
use futures::future::BoxFuture;
use kiln_ci::work::eclass::{self, Case, Pattern, Rewrite, Rule, Witness};
use kiln_ci::work::key::{self, Key, Kind, PathSet, Platform, Read, Unit};
use kiln_ci::work::trust::{Domain, Quotas};
use kiln_seams::blobs::Put;
use kiln_seams::records::{RecordsFixture, Rev, SimRecords};

use super::super::{Provenance, Written, digest, scope};
use super::*;
use crate::records::StoreRecords;

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

/// `u` as `domain` keys it.
fn unit_in(u: Unit, domain: &str) -> Unit {
    Unit { trust: domain.into(), ..u }
}

fn reads(n: u64, variant: u64) -> PathSet {
    (0..3).map(|i| (format!("src/f{n}_{i}.rs"), Read::File(format!("d{n}_{i}_{variant}")))).collect()
}

/// A success of `domain`, made on `runner`, with `outputs`.
fn made(domain: &str, runner: &str, outputs: &[(&str, &str)]) -> Recorded {
    Recorded {
        outcome: "success".into(),
        exit: Some(0),
        outputs: outputs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
        provenance: Provenance { runner: runner.into(), run: format!("run-{runner}"), leg: "build".into(), attempt: 1, ms: 60_000, ..Provenance::default() },
        domain: domain.into(),
        ..Recorded::default()
    }
}

fn ok(domain: &str, run: &str) -> Recorded {
    Recorded { outcome: "success".into(), exit: Some(0), provenance: Provenance { run: run.into(), ..Provenance::default() }, domain: domain.into(), ..Recorded::default() }
}

fn same(p: &PathSet) -> PathSet {
    p.clone()
}

/// A step that runs `definition` over upstream units, each by its outputs' digest or by key.
fn step(definition: &str, upstream: &[(&str, String)]) -> Unit {
    Unit { definition: definition.to_owned(), inputs: BTreeMap::new(), upstream: upstream.iter().map(|(n, v)| ((*n).to_owned(), v.clone())).collect(), platform: Platform::default(), ..unit(0) }
}

fn lock() -> PathSet {
    PathSet::from([("Cargo.lock".to_owned(), Read::File("l1".into()))])
}

/// The ledger's homes (W24-R02), each with the store its audit log is in: kiln's `work_records`
/// table, kiln's own Records durable in a store, and the simulated Records.
fn homes() -> anyhow::Result<Vec<(&'static str, Ledger, Store)>> {
    let table = Store::in_memory()?;
    let kept = Store::in_memory()?;
    let sim_audit = Store::in_memory()?;
    Ok(vec![
        ("the work_records table", Ledger::Table(table.clone()), table),
        ("kiln's Records", Ledger::Records { records: Arc::new(StoreRecords::open(kept.clone())?), audit: kept.clone() }, kept),
        ("the simulated Records", Ledger::Records { records: Arc::new(SimRecords::new()), audit: sim_audit.clone() }, sim_audit),
    ])
}

/// The audit log's rows in `store`: action, result and target, in order.
async fn audited(store: &Store) -> anyhow::Result<Vec<(String, String, String)>> {
    store
        .call(|c| {
            let mut q = c.prepare("SELECT action, result, COALESCE(target, '') FROM audit2 ORDER BY id")?;
            let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

// ----------------------------------------------------------------------------- rows, alike

/// A small, seeded generator: the same operations every run.
struct Gen(u64);

impl Gen {
    fn next(&mut self, below: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % below.max(1)
    }
}

#[derive(Debug, Clone)]
enum Op {
    Write { scope: u64, unit: u64, variant: u64 },
    Lookup { scope: u64, unit: u64, changed: bool },
    PathSets { scope: u64, unit: u64 },
    Put { family: usize, key: u64, value: u64 },
    PutNew { family: usize, key: u64, value: u64 },
    Get { family: usize, key: u64 },
    Delete { family: usize, key: u64 },
    Scan { family: usize, prefix: u64 },
}

fn ops(seed: u64, n: usize) -> Vec<Op> {
    let mut g = Gen(seed);
    let family = |g: &mut Gen| usize::try_from(g.next(Family::ALL.len() as u64)).unwrap_or(0);
    (0..n)
        .map(|_| match g.next(8) {
            0 => Op::Write { scope: g.next(2), unit: g.next(3), variant: g.next(12) },
            1 => Op::Lookup { scope: g.next(2), unit: g.next(3), changed: g.next(4) == 0 },
            2 => Op::PathSets { scope: g.next(2), unit: g.next(3) },
            3 => Op::Put { family: family(&mut g), key: g.next(8), value: g.next(1000) },
            4 => Op::PutNew { family: family(&mut g), key: g.next(8), value: g.next(1000) },
            5 => Op::Get { family: family(&mut g), key: g.next(8) },
            6 => Op::Delete { family: family(&mut g), key: g.next(8) },
            _ => Op::Scan { family: family(&mut g), prefix: g.next(3) },
        })
        .collect()
}

/// What each operation answered, said: the same on every home.
fn run_ops(rows: &dyn Rows, ops: &[Op]) -> anyhow::Result<Vec<String>> {
    let domains = ["trusted", "untrusted"];
    let family = |i: usize| Family::ALL.get(i).copied().unwrap_or(Family::Snapshot);
    let mut said = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let at = i64::try_from(i).unwrap_or(0);
        let d = |s: u64| domains.get(usize::try_from(s).unwrap_or(0)).copied().unwrap_or("trusted");
        let answer = match op {
            Op::Write { scope: s, unit: u, variant } => format!("{:?}", super::super::write(rows, None, &scope("acme/app", d(*s)), &unit_in(unit(*u), d(*s)), &reads(*u, *variant), &ok(d(*s), &format!("r{i}")))?),
            Op::Lookup { scope: s, unit: u, changed } => {
                let changed = *changed;
                let now = move |p: &PathSet| -> PathSet { if changed { p.keys().map(|k| (k.clone(), Read::Absent)).collect() } else { p.clone() } };
                format!("{:?}", super::super::lookup(rows, &scope("acme/app", d(*s)), &unit_in(unit(*u), d(*s)), &now)?.map(|h| (h.key, h.recorded.provenance.run)))
            }
            Op::PathSets { scope: s, unit: u } => format!("{:?}", super::super::path_sets(rows, &scope("acme/app", d(*s)), &key::weak(&unit_in(unit(*u), d(*s))))?),
            Op::Put { family: f, key, value } => format!("{:?}", rows.put(family(*f), &format!("k{key}"), format!("v{value}").as_bytes(), at % 5)?),
            Op::PutNew { family: f, key, value } => format!("{:?}", rows.put_new(family(*f), &format!("k{key}"), format!("n{value}").as_bytes(), at % 5)?),
            Op::Get { family: f, key } => format!("{:?}", rows.get(family(*f), &format!("k{key}"))?),
            Op::Delete { family: f, key } => format!("{:?}", rows.delete(family(*f), &format!("k{key}"))?),
            Op::Scan { family: f, prefix } => format!("{:?}", rows.scan(family(*f), &format!("k{}", if *prefix == 0 { String::new() } else { prefix.to_string() }))?),
        };
        said.push(answer);
    }
    Ok(said)
}

/// W24-R02: the table and Records answer every operation of the ledger alike: 600 seeded writes,
/// two-phase lookups (hits and misses), listings past the newest eight, and rows of every family
/// put, put only where none is, read, listed and deleted.
#[test]
fn w24_r02_the_table_and_records_answer_alike() -> anyhow::Result<()> {
    let homes = homes()?;
    crate::rt::block_on(async move {
        let ops = ops(0x5eed_1e57, 600);
        let mut said = Vec::new();
        for (name, ledger, _) in &homes {
            let theirs = ops.clone();
            said.push((*name, ledger.run(move |rows| run_ops(rows, &theirs)).await.with_context(|| format!("on {name}"))?));
        }
        let (first, answers) = said.first().cloned().context("no homes")?;
        for (name, theirs) in &said {
            for (i, op) in ops.iter().enumerate() {
                anyhow::ensure!(answers.get(i) == theirs.get(i), "operation {i} ({op:?}): {first} said {:?}, {name} {:?}", answers.get(i), theirs.get(i));
            }
        }
        anyhow::ensure!(answers.iter().any(|s| s.starts_with("Some((")), "some lookups hit");
        anyhow::ensure!(answers.iter().any(|s| s.starts_with("Reproduced")), "some writes reproduce an entry");
        Ok(())
    })
}

/// W24-R02: every family is kept alike in both, and in Records a row is its stamp, then the
/// table's value.
#[test]
fn w24_r02_every_family_is_kept_alike_and_records_holds_the_stamp_then_the_value() -> anyhow::Result<()> {
    let table = Store::in_memory()?;
    let sim = Arc::new(SimRecords::new());
    let on_sim = Ledger::Records { records: Arc::clone(&sim) as Arc<dyn Records>, audit: Store::in_memory()? };
    crate::rt::block_on(async move {
        let names: Vec<&str> = Family::ALL.iter().map(|f| f.name()).collect();
        anyhow::ensure!(
            names == ["work.result", "work.weak", "work.pathset", "work.snapshot", "work.lane", "work.test", "work.eclass", "work.object", "work.verify", "work.entry"],
            "{names:?}"
        );
        for f in Family::ALL {
            let both = move |rows: &dyn Rows| -> anyhow::Result<Vec<(String, Vec<u8>, i64)>> {
                rows.put(f, "acme/app|trusted|a", b"one", 7)?;
                rows.put(f, "acme/app|trusted|b", b"two", 9)?;
                rows.scan(f, "acme/app|trusted|")
            };
            let on_table = Ledger::Table(table.clone()).run(both).await?;
            let on_records = on_sim.run(both).await?;
            anyhow::ensure!(on_table == on_records, "{}: {on_table:?} against {on_records:?}", f.name());
            anyhow::ensure!(on_records.first().map(|(k, v, at)| (k.as_str(), v.as_slice(), *at)) == Some(("acme/app|trusted|b", &b"two"[..], 9)), "{on_records:?}");
            let raw = sim.get(f.name(), b"acme/app|trusted|a", Consistency::Linearizable).await?.map(|row| row.value);
            anyhow::ensure!(raw == Some(stamped(7, b"one")), "{}: {raw:?}", f.name());
            anyhow::ensure!(raw.as_ref().and_then(|v| v.get(..8)) == Some(&7i64.to_be_bytes()[..]));
            let gone = on_sim
                .run(move |rows| {
                    rows.delete(f, "acme/app|trusted|a")?;
                    rows.get(f, "acme/app|trusted|a")
                })
                .await?;
            anyhow::ensure!(gone.is_none());
        }
        Ok(())
    })
}

/// W24-R02: what Records can't hold (a path set past a MiB) it refuses, typed, and nothing of the
/// write is kept; the table, which has no bound, takes it.
#[test]
fn w24_r02_records_refuses_what_it_cant_hold_and_writes_nothing() -> anyhow::Result<()> {
    let homes = homes()?;
    crate::rt::block_on(async move {
        let huge: PathSet = (0..20_000).map(|i| (format!("src/a/very/deep/folder/of/sources/file{i:06}.rs"), Read::File(format!("{i:064x}")))).collect();
        anyhow::ensure!(serde_json::to_vec(&huge)?.len() > records::VALUE_MAX);
        for (name, ledger, _) in &homes {
            let theirs = huge.clone();
            let (wrote, listed, found) = ledger
                .run(move |rows| {
                    let sc = scope("acme/app", "trusted");
                    let wrote = super::super::write(rows, None, &sc, &unit(1), &theirs, &ok("trusted", "r1"));
                    let listed = super::super::path_sets(rows, &sc, &key::weak(&unit(1)))?;
                    let found = super::super::lookup(rows, &sc, &unit(1), &same)?.map(|h| h.recorded.provenance.run);
                    Ok((wrote.map_err(|e| e.downcast_ref::<records::Refusal>().map(|r| format!("{r}")).unwrap_or_else(|| format!("{e:#}"))), listed, found))
                })
                .await?;
            if matches!(ledger, Ledger::Table(_)) {
                anyhow::ensure!(wrote.is_ok() && listed.len() == 1 && found.as_deref() == Some("r1"), "{name}: {wrote:?} {listed:?} {found:?}");
            } else {
                anyhow::ensure!(wrote.as_ref().is_err_and(|e| e.contains("over its limit")), "{name}: {wrote:?}");
                anyhow::ensure!(listed.is_empty() && found.is_none(), "{name}: nothing of it kept: {listed:?} {found:?}");
            }
        }
        Ok(())
    })
}

/// W24-R22 on Records: an entry's insert-only write is a condition that its key holds nothing when
/// the transaction commits, so of two writers racing for one key the second's whole transaction is
/// refused, and the first's entry stands.
#[test]
fn w24_r22_on_records_two_writers_racing_for_an_entry_leave_one() -> anyhow::Result<()> {
    let records: Arc<dyn Records> = Arc::new(SimRecords::new());
    let (a, b) = (OnRecords::new(Arc::clone(&records), Store::in_memory()?), OnRecords::new(Arc::clone(&records), Store::in_memory()?));
    let raced = a.atomically(&mut |tx| {
        anyhow::ensure!(tx.put_new(Family::Result, "acme/app|trusted|k", b"first", 1)?, "nothing at the key when it read");
        tx.put(Family::Lane, "acme/app|trusted|lane", b"with it", 1)?;
        // The other writer gets there first.
        anyhow::ensure!(b.put_new(Family::Result, "acme/app|trusted|k", b"second", 2)?);
        Ok(())
    });
    let e = raced.err().context("the later commit went through")?;
    anyhow::ensure!(matches!(e.downcast_ref::<records::Refusal>(), Some(records::Refusal::Conflict { .. })), "{e:#}");
    anyhow::ensure!(a.get(Family::Result, "acme/app|trusted|k")? == Some(b"second".to_vec()), "the first write standing is the one that committed");
    anyhow::ensure!(a.get(Family::Lane, "acme/app|trusted|lane")?.is_none(), "nothing of the refused transaction");
    anyhow::ensure!(!a.put_new(Family::Result, "acme/app|trusted|k", b"third", 3)?, "a write alone, refused the same way");
    Ok(())
}

// --------------------------------------------------------------------------- the one path, alike

const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
const D3: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

/// The one path end to end (W24-R02, R21 to R23, R30, R10), its decisions said: recorded, found
/// while what it read holds, reproduced, refused across domains, read down by an untrusted leg, a
/// conflict judged and quarantined, keys merged by their outputs and a unit found by class, a wrong
/// class hit split, a rule's rewrite found by class, test results kept and replayed, and an
/// untrusted domain over its quota evicted.
fn scenario(rows: &dyn Rows) -> anyhow::Result<Vec<String>> {
    use super::super::{evict, found_test, lookup, lookup_as, member, quarantined, record, record_test, register, verdict, verified, write};
    let mut said = Vec::new();
    let (t, u) = (scope("acme/app", "trusted"), scope("acme/app", "untrusted"));
    let changed = |p: &PathSet| -> PathSet { p.keys().map(|k| (k.clone(), Read::File("changed".into()))).collect() };
    // Recorded, found while what it read holds, missed once it doesn't.
    let a = unit(1);
    let first = write(rows, None, &t, &a, &reads(1, 0), &made("trusted", "r1", &[("bin", D1)]))?;
    let Written::New(strong) = first.clone() else { anyhow::bail!("{first:?}") };
    said.push(format!("{first:?}"));
    said.push(format!("{:?}", lookup(rows, &t, &a, &same)?.map(|h| (h.key, h.recorded.provenance.runner, h.class.is_some()))));
    said.push(format!("{:?}", lookup(rows, &t, &a, &changed)?.map(|h| h.key)));
    // Another runner reproduces it; its own runner again is nothing new.
    said.push(format!("{:?}", write(rows, None, &t, &a, &reads(1, 0), &made("trusted", "r2", &[("bin", D1)]))?));
    said.push(format!("{:?}", write(rows, None, &t, &a, &reads(1, 0), &made("trusted", "r1", &[("bin", D1)]))?));
    said.push(format!("{:?}", lookup(rows, &t, &a, &same)?.map(|h| (h.recorded.verified_by, h.recorded.provenance.runner))));
    // An untrusted result among trusted entries is refused; an untrusted leg's own is kept, and its
    // lookups read the trusted entry first.
    said.push(format!("{:?}", write(rows, None, &t, &unit(2), &reads(2, 0), &made("untrusted", "fork", &[("bin", D2)]))?));
    said.push(format!("{:?}", write(rows, None, &u, &unit_in(a.clone(), "untrusted"), &reads(1, 0), &made("untrusted", "fork", &[("bin", D2)]))?));
    let down = lookup_as(rows, None, "acme/app", &Domain::Untrusted, &a, &same)?;
    said.push(format!("{:?}", down.hit.map(|h| (h.domain, h.recorded.outputs))));
    let up = lookup_as(rows, None, "acme/app", &Domain::Trusted, &unit_in(unit(9), "trusted"), &same)?;
    said.push(format!("{:?}", up.hit.map(|h| h.key)));
    // Another result for the entry's key: refused, kept as evidence, the entry judged again and its
    // lineage quarantined, so lookups pass it over.
    said.push(format!("{:?}", write(rows, None, &t, &a, &reads(1, 0), &made("trusted", "r3", &[("bin", D3)]))?));
    said.push(format!("{:?}", verdict(rows, &t, &strong)?));
    said.push(format!("{:?} {:?}", quarantined(rows, &t, &strong)?.map(|q| q.root), lookup(rows, &t, &a, &same)?.map(|h| h.key)));
    // Keys whose units produced byte-identical outputs are one class, and a unit naming one of them
    // by key finds what ran over the other's outputs.
    let fetch = |src: &str| Unit { inputs: BTreeMap::from([("src".to_owned(), src.to_owned())]), ..step("fetch", &[]) };
    let f1 = record(rows, &t, &fetch("s1"), &lock(), &made("trusted", "r1", &[("out", D1)]))?;
    let f2 = record(rows, &t, &fetch("s2"), &lock(), &made("trusted", "r2", &[("out", D1)]))?;
    said.push(format!("{:?} {:?}", member(rows, &t, &f1)?.map(|m| m.class), member(rows, &t, &f2)?.map(|m| m.class)));
    let d = digest(&made("trusted", "r1", &[("out", D1)]));
    record(rows, &t, &step("make", &[("src", d)]), &lock(), &made("trusted", "r1", &[("out", D2)]))?;
    let by_class = lookup(rows, &t, &step("make", &[("src", eclass::refer(&f2))]), &same)?.context("a congruent result")?;
    said.push(format!("{:?} {:?} {:?}", by_class.key, by_class.class.as_ref().map(|b| (&b.class, &b.took)), by_class.recorded.outputs));
    // Verification sees the class hit wrong: the class splits, and its keys are quarantined.
    let split: Vec<Key> = verified(rows, &t, &by_class, &digest(&made("trusted", "v", &[("out", D3)])))?.into_iter().collect();
    said.push(format!("{split:?}"));
    said.push(format!("{:?}", lookup(rows, &t, &step("make", &[("src", eclass::refer(&f2))]), &same)?.map(|h| h.key)));
    // A rule registered with its witness: a unit it rewrites finds its class's result.
    let case = Case { before: "build\ntrue".into(), after: "build".into(), before_output: "sha256:alike".into(), after_output: "sha256:alike".into() };
    let rule = Rule { name: "drop-true".into(), kinds: BTreeSet::new(), rewrite: Rewrite::Drop { op: Pattern::Is("true".into()) } };
    register(rows, &t, &rule, &Witness { test: "drop-true, both ways".into(), cases: vec![case] })?;
    let built = record(rows, &t, &step("build", &[]), &lock(), &made("trusted", "r1", &[("out", D3)]))?;
    let rewritten = lookup(rows, &t, &step("build\ntrue", &[]), &same)?.context("found by the rule")?;
    said.push(format!("{:?} {} {:?}", rewritten.key, rewritten.key == built, rewritten.class.map(|b| b.why)));
    // Test results: a pass kept and found by its unit; a failure counts, and isn't kept.
    {
        use kiln_ci::work::tests::{Binary, Case as Test, Context, Ran};
        let cx = Context { env: BTreeMap::new(), platform: Platform::default(), trust: "trusted".into() };
        let b = Binary { package: "a".into(), name: "lib".into(), args: Vec::new(), digest: Some("a1".into()) };
        let read = PathSet::from([("crates/a/tests/data.txt".to_owned(), Read::File("a-data".into()))]);
        let ran = |c: Test, exit: i32| Ran { exit: Some(exit), cases: BTreeMap::from([("t1".to_owned(), c)]), ms: 1_000 };
        let at = Provenance { repo: "acme/app".into(), run: "r1".into(), leg: "r1-test".into(), ..Provenance::default() };
        said.push(format!("{:?}", record_test(rows, &t, &b, &cx, &read, &ran(Test::Failed, 101), &at)?));
        said.push(format!("{:?}", record_test(rows, &t, &b, &cx, &read, &ran(Test::Passed, 0), &at)?));
        let found = found_test(rows, &t, &b, &cx, &same)?;
        said.push(format!("{:?} {:?} {}", found.by_key.map(|k| (k.run, k.flaky)), found.last.map(|k| k.digest), found.last_holds));
    }
    // An untrusted domain held to nothing evicts all its entries, and none of another's.
    let mut gone = evict(rows, "acme/app", &Quotas { trusted: 1 << 40, protected: 1 << 40, untrusted: 0, person: 1 << 40 })?;
    gone.sort();
    said.push(format!("{gone:?}"));
    // `a`'s trusted entry is quarantined and its untrusted one gone: nothing for it. A trusted
    // entry in good standing is what an untrusted lookup reads, down.
    said.push(format!("{:?}", lookup_as(rows, None, "acme/app", &Domain::Untrusted, &a, &same)?.hit.map(|h| (h.domain, h.key))));
    said.push(format!("{:?}", lookup_as(rows, None, "acme/app", &Domain::Untrusted, &fetch("s1"), &same)?.hit.map(|h| (h.domain, h.key))));
    Ok(said)
}

/// W24-R02, R30: the one path decides alike on every home: the same scenario, end to end, says the
/// same on the `work_records` table, kiln's Records and the simulated Records, and leaves the same
/// rows in the audit log.
#[test]
fn w24_r02_the_one_path_decides_alike_on_every_home() -> anyhow::Result<()> {
    let homes = homes()?;
    crate::rt::block_on(async move {
        let mut said = Vec::new();
        for (name, ledger, audit) in &homes {
            let answers = ledger.run(scenario).await.with_context(|| format!("on {name}"))?;
            let rows: Vec<(String, String)> = audited(audit).await?.into_iter().map(|(action, result, _)| (action, result)).collect();
            said.push((*name, answers, rows));
        }
        let (first, answers, rows) = said.first().cloned().context("no homes")?;
        for (name, theirs, their_rows) in &said {
            for (i, (a, b)) in answers.iter().zip(theirs).enumerate() {
                anyhow::ensure!(a == b, "step {i}: {first} said {a}, {name} {b}");
            }
            anyhow::ensure!(answers.len() == theirs.len() && rows == *their_rows, "{name}: {theirs:?} {their_rows:?}");
        }
        // What the scenario is to show, on each: a refusal and a quarantine in the audit log, a hit
        // by class, a split, and the untrusted entry evicted.
        anyhow::ensure!(rows.iter().any(|(a, r)| a == "work.record" && r == "refused") && rows.iter().any(|(a, r)| a == "work.verify" && r == "quarantined"), "{rows:?}");
        anyhow::ensure!(answers.iter().any(|s| s.starts_with("Reproduced")) && answers.iter().any(|s| s.starts_with("Refused")) && answers.iter().any(|s| s.starts_with("Conflict")), "{answers:?}");
        let tail: Vec<&String> = answers.iter().rev().take(2).collect();
        anyhow::ensure!(tail.get(1).is_some_and(|s| *s == "None") && tail.first().is_some_and(|s| s.starts_with("Some((Trusted")), "the untrusted lookups at the end: {tail:?}");
        Ok(())
    })
}

// ------------------------------------------------------------------------ the seam's own suite

/// kiln's Records, a store the ledger has written to first.
struct WithLedger(StoreRecords);

impl RecordsFixture for WithLedger {
    fn records(&self) -> &dyn Records {
        &self.0
    }

    fn compact(&self, before: Rev) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Err(e) = self.0.compact(before).await {
                tracing::warn!(error = %format!("{e:#}"), "records: a compaction failed");
            }
        })
    }
}

/// What the ledger writes before a case: an entry, and a row of every other family.
fn fill(rows: &dyn Rows) -> anyhow::Result<Key> {
    let sc = scope("acme/app", "trusted");
    let strong = super::super::record(rows, &sc, &unit(9), &reads(9, 0), &ok("trusted", "before"))?;
    for family in [Family::Snapshot, Family::Lane, Family::Entry] {
        rows.put(family, &format!("{sc}row"), b"kept", 2)?;
    }
    Ok(strong)
}

/// W24-R02: the Records the ledger is kept in (kiln's own, durable in kiln's store) passes the
/// seam's conformance suite as it is, every clause, with the ledger's rows in it: each case gets a
/// store the ledger wrote to first, and the ledger finds its rows after, as they were.
#[test]
fn w24_r02_the_records_the_ledger_is_kept_in_passes_the_seams_suite_with_its_rows_in_it() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let cases = kiln_seams::records::cases::<WithLedger>();
        let mut stores = VecDeque::new();
        for _ in 0..cases.len() {
            let audit = Store::in_memory()?;
            let r = StoreRecords::open(Store::in_memory()?)?;
            let strong = Ledger::Records { records: Arc::new(r.clone()), audit: audit.clone() }.run(fill).await?;
            stores.push_back((r, audit, strong));
        }
        let after: Vec<(StoreRecords, Store, Key)> = stores.iter().cloned().collect();
        let last = stores.back().map(|(r, _, _)| r.clone()).context("no cases")?;
        let make = || {
            let r = stores.pop_front().map_or_else(|| last.clone(), |(r, _, _)| r);
            async move { WithLedger(r) }
        };
        let clock = crate::rt::clock();
        let n = kiln_seams::conformance::run_within("Records (kiln's store, the work ledger in it)", &cases, make, || clock.sleep(Duration::from_secs(30))).await.into_result().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(n == cases.len(), "{n} of {} clauses held", cases.len());
        for (r, audit, strong) in after {
            let found = Ledger::Records { records: Arc::new(r), audit }
                .run(move |rows| {
                    let sc = scope("acme/app", "trusted");
                    let found = super::super::lookup(rows, &sc, &unit(9), &same)?.map(|h| (h.key, h.recorded.provenance.run));
                    let kept: Vec<Option<Vec<u8>>> = [Family::Snapshot, Family::Lane, Family::Entry].into_iter().map(|f| rows.get(f, &format!("{sc}row"))).collect::<anyhow::Result<_>>()?;
                    Ok((found, kept))
                })
                .await?;
            anyhow::ensure!(found.0 == Some((strong, "before".to_owned())), "{found:?}");
            anyhow::ensure!(found.1.iter().all(|v| v.as_deref() == Some(&b"kept"[..])), "{found:?}");
        }
        Ok(())
    })
}

// ----------------------------------------------------------------------------------------- bytes

/// The bytes a result names, in a store: a log, a test report and an output file, each at its
/// digest; and the result naming them (one output a value, not a digest).
async fn bytes_named(b: &dyn Blobs) -> anyhow::Result<(Recorded, BTreeMap<String, Vec<u8>>)> {
    let mut kept = BTreeMap::new();
    for (what, data) in [("log", b"compiling crate1\nfinished\n".to_vec()), ("report", br#"<testsuite tests="3"/>"#.to_vec()), ("file", kiln_seams::blobs::noise(3 << 20, 5))] {
        let digest = crate::util::sha256_hex(&data);
        b.put(&crate::ci::services::cas::key(&digest), data.clone(), Put { sha256: Some(digest.clone()), ..Put::default() }).await?;
        kept.insert(what.to_owned(), data);
    }
    let d = |what: &str| kept.get(what).map(|v| crate::util::sha256_hex(v)).unwrap_or_default();
    let result = Recorded {
        logs: Some(d("log")),
        reports: vec![format!("sha256:{}", d("report"))],
        outputs: BTreeMap::from([("target/release/app".to_owned(), d("file")), ("version".to_owned(), "1.2.3".to_owned())]),
        ..ok("trusted", "r1")
    };
    Ok((result, kept))
}

/// W24-R02: the bytes a result names stay in the content store (`cas/<sha256>`), reachable through
/// `Blobs` from the ledger on every home, on the simulated store and on kiln's local one; one gone,
/// or holding other bytes, is said to be unreachable.
#[test]
fn w24_r02_a_results_bytes_are_reachable_through_blobs() -> anyhow::Result<()> {
    let local = crate::blobs::local::tests::Fixture::new()?;
    let homes = homes()?;
    crate::rt::block_on(async move {
        let sim = kiln_seams::blobs::SimBlobs::new(Arc::new(kiln_seams::clock::SimClock::new()), kiln_seams::blobs::SimLimits::default());
        for (bname, b) in [("the simulated store", &sim as &dyn Blobs), ("kiln's local store", &local.blobs as &dyn Blobs)] {
            let (result, kept) = bytes_named(b).await?;
            for (name, ledger, _) in &homes {
                let theirs = result.clone();
                let found = ledger
                    .run(move |rows| {
                        let sc = scope("acme/app", "trusted");
                        super::super::write(rows, None, &sc, &unit(2), &reads(2, 0), &theirs)?;
                        Ok(super::super::lookup(rows, &sc, &unit(2), &same)?.map(|h| h.recorded))
                    })
                    .await?
                    .with_context(|| format!("{bname}, the ledger on {name}: recorded, then not found"))?;
                let keys = blob_keys(&found);
                anyhow::ensure!(keys.len() == 3, "the log, the report and the file, not the version: {keys:?}");
                anyhow::ensure!(unreachable(b, &found).await?.is_empty(), "{bname}, the ledger on {name}");
                for data in kept.values() {
                    let key = crate::ci::services::cas::key(&crate::util::sha256_hex(data));
                    anyhow::ensure!(keys.contains(&key), "{key} is named");
                    let (meta, got) = b.get(&key, None).await?;
                    anyhow::ensure!(got == *data && Some(meta.etag.as_str()) == key.strip_prefix("cas/"), "{key} read back whole");
                }
            }
            // Gone, or other bytes: unreachable.
            let file = crate::ci::services::cas::key(&crate::util::sha256_hex(kept.get("file").map(Vec::as_slice).unwrap_or_default()));
            b.delete(&file).await?;
            let log = crate::ci::services::cas::key(result.logs.as_deref().unwrap_or_default());
            b.put(&log, b"other bytes".to_vec(), Put::default()).await?;
            let mut gone = unreachable(b, &result).await?;
            gone.sort();
            let mut want = vec![file, log];
            want.sort();
            anyhow::ensure!(gone == want, "{bname}: {gone:?}");
        }
        Ok(())
    })
}

/// W24-R02: which values are content digests.
#[test]
fn w24_r02_digests_bare_or_named_and_nothing_else() {
    let hex = "ab".repeat(32);
    assert_eq!(digest_of(&hex), Some(hex.as_str()));
    assert_eq!(digest_of(&format!("sha256:{hex}")), Some(hex.as_str()));
    assert_eq!(digest_of(&"AB".repeat(32)), None, "lowercase, as the store names them");
    assert_eq!(digest_of(&"ab".repeat(31)), None);
    assert_eq!(digest_of("1.2.3"), None);
    assert_eq!(digest_of(&format!("sha512:{hex}")), None);
}

/// W24-AC10 with the ledger in Records (W24-R02): 20,000 results under 5,000 weak keys, recorded
/// on the one path into the simulated Records, copied row for row (a thousand a commit) into kiln's
/// own Records durable in a store on disk, then looked up 5,000 times on the one path there.
///
/// AC10's 5 ms is held on the table (`ci::work`'s `w24_ac10_lookup_p99_under_5ms`), the ledger's
/// system of record. A lookup on Records is its seam's calls one after another (the weak key's
/// listing, a path set, a result, what's kept beside it, and its hit counted, a commit), each a
/// trip to the store's thread. This holds it under twice AC10's bound, against regressions, until
/// the ledger moves onto Records and Amphora's (CP-T1.5) is measured against AC10 itself.
#[test]
fn w24_ac10_lookup_on_records_p99_under_10ms() -> anyhow::Result<()> {
    use kiln_seams::records::Scan as Page;
    let dir = tempfile::tempdir()?;
    let kept_store = Store::open(&dir.path().join("kiln.db"))?;
    let kept: Arc<dyn Records> = Arc::new(StoreRecords::open(kept_store.clone())?);
    let sim: Arc<dyn Records> = Arc::new(SimRecords::new());
    let units: u64 = 5_000;
    let sc = scope("acme/app", "trusted");
    let filling = OnRecords::new(Arc::clone(&sim), Store::in_memory()?);
    for n in 0..units {
        for v in 0..4 {
            super::super::record(&filling, &sc, &unit(n), &reads(n, v), &ok("trusted", &format!("r{n}_{v}")))?;
        }
    }
    let (from, to) = (Arc::clone(&sim), Arc::clone(&kept));
    let copied = crate::rt::block_on(async move {
        let mut copied = 0usize;
        for family in Family::ALL {
            let mut after = None;
            loop {
                let page = from.scan(family.name(), Page { prefix: Vec::new(), after: after.take(), limit: records::TX_MAX, at: Consistency::Linearizable }).await?;
                after = page.rows.last().map(|r| r.key.clone());
                copied += page.rows.len();
                let writes = page.rows.into_iter().map(|r| Write::Put { family: family.name().to_owned(), key: r.key, value: r.value }).collect();
                to.commit(Tx { conds: Vec::new(), writes }).await?;
                if !page.more || after.is_none() {
                    break;
                }
            }
        }
        Ok(copied)
    })?;
    anyhow::ensure!(copied >= 60_000, "a path set, a listing and a result for each of 20,000 records, at least: {copied} rows");
    let rows = OnRecords::new(kept, kept_store);
    let mut took = Vec::new();
    for i in 0..units {
        let n = (i * 7_919) % units;
        let started = std::time::Instant::now();
        let hit = super::super::lookup(&rows, &sc, &unit(n), &same)?;
        took.push(started.elapsed());
        anyhow::ensure!(hit.is_some(), "unit {n} has results");
    }
    took.sort();
    let at = |q: f64| took.get(((took.len() as f64 * q).ceil() as usize).saturating_sub(1)).copied().unwrap_or_default();
    let (p50, p99) = (at(0.50), at(0.99));
    println!("w24_ac10 on Records: {} lookups, p50 {:?}, p99 {:?} ({copied} rows)", took.len(), p50, p99);
    anyhow::ensure!(p99 < Duration::from_millis(10), "p99 {p99:?} is 10 ms or more");
    Ok(())
}
