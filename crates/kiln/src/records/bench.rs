//! A small harness for the engines under Records (CP-T1.5; kiln goal 0d717247): a record's write
//! and read, and the deploy API over them (a release recorded, read back, scheduled, claimed and
//! done), on each store a host or the server keeps records in: kiln's store (`StoreRecords`, the
//! server's), Turso's (the control) and Amphora's. Each operation's latency at p50, p99 and p99.9,
//! nearest rank, in microseconds.
//!
//! Its sizes are what it's given, not picked here: `KILN_RECORDS_BENCH_OPS` operations of each kind
//! and `KILN_RECORDS_BENCH_VALUE` bytes a record; unset, the one tiny instance a lane runs to see it
//! work (`TINY_OPS`, `TINY_VALUE`). The run of record, its sizes measured from kiln's own records, is
//! the coordinator's:
//!
//! ```text
//! KILN_RECORDS_BENCH_OPS=<n> KILN_RECORDS_BENCH_VALUE=<bytes> \
//!   cargo test -p kiln --lib records::bench -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use kiln_seams::records::{Consistency, Records, Tx, Write};

use crate::deploy::model::Release;
use crate::deploy::store as deploy;

/// The tiny instance's operations of each kind: enough to see each quantile is read, no measurement.
const TINY_OPS: usize = 200;
/// The tiny instance's record size.
const TINY_VALUE: usize = 256;

fn input(var: &str, tiny: usize) -> anyhow::Result<usize> {
    match std::env::var(var) {
        Err(std::env::VarError::NotPresent) => Ok(tiny),
        Err(e) => Err(anyhow::anyhow!("{var}: {e}")),
        Ok(v) => v.trim().parse().map_err(|e| anyhow::anyhow!("{var} is `{v}`: {e}")),
    }
}

/// An operation's latencies.
#[derive(Default)]
struct Lat(Vec<Duration>);

impl Lat {
    fn timed<T>(&mut self, start: Instant, out: T) -> T {
        self.0.push(start.elapsed());
        out
    }

    fn line(&self, store: &str, op: &str) -> String {
        let mut sorted = self.0.clone();
        sorted.sort_unstable();
        format!("{store:<8} {op:<16} n={:<7} p50={}us p99={}us p99.9={}us", sorted.len(), at(&sorted, 500), at(&sorted, 990), at(&sorted, 999))
    }
}

/// The `per_mille`th of sorted latencies by nearest rank (500: p50, 990: p99, 999: p99.9).
fn at(sorted: &[Duration], per_mille: usize) -> u128 {
    let rank = sorted.len().saturating_mul(per_mille).div_ceil(1000).saturating_sub(1);
    sorted.get(rank).map_or(0, Duration::as_micros)
}

fn refused(r: kiln_seams::records::Refusal) -> anyhow::Error {
    anyhow::anyhow!("records: {r}")
}

/// Each operation's latencies on one store, a line each.
async fn measure(name: &str, records: Arc<dyn Records>, ops: usize, value: usize) -> anyhow::Result<Vec<String>> {
    let st: deploy::Recs<'_> = &*records;
    let (mut put, mut get) = (Lat::default(), Lat::default());
    let body = vec![b'x'; value];
    for i in 0..ops {
        let key = format!("{i:020}").into_bytes();
        let t = Instant::now();
        put.timed(t, records.commit(Tx { conds: Vec::new(), writes: vec![Write::Put { family: "bench.rows".into(), key, value: body.clone() }] }).await).map_err(refused)?;
    }
    for i in 0..ops {
        let key = format!("{i:020}").into_bytes();
        let t = Instant::now();
        let row = get.timed(t, records.get("bench.rows", &key, Consistency::Linearizable).await).map_err(refused)?;
        anyhow::ensure!(row.is_some_and(|r| r.value.len() == value), "{name}: record {i} read back whole");
    }
    let template: Release = serde_json::from_value(serde_json::json!({"id": 0, "app": "web", "environment": "staging", "kind": "deploy", "state": "waiting", "requested_by": "bench", "created_at": 0}))?;
    let (mut insert, mut read, mut schedule, mut claim, mut done) = (Lat::default(), Lat::default(), Lat::default(), Lat::default(), Lat::default());
    for _ in 0..ops {
        let t = Instant::now();
        let (made, _) = insert.timed(t, deploy::insert_release(st, &template).await)?;
        let t = Instant::now();
        anyhow::ensure!(read.timed(t, deploy::release(st, made.id).await)?.is_some(), "{name}: release {} read back", made.id);
        let t = Instant::now();
        schedule.timed(t, deploy::schedule(st, made.id, 0).await)?;
        let t = Instant::now();
        let taken = claim.timed(t, deploy::claim(st, name, 0, 60, 1).await)?;
        anyhow::ensure!(taken.iter().any(|l| l.release == made.id), "{name}: release {} claimed", made.id);
        let t = Instant::now();
        done.timed(t, deploy::done(st, made.id).await)?;
    }
    Ok(vec![
        put.line(name, "record put"),
        get.line(name, "record get"),
        insert.line(name, "release insert"),
        read.line(name, "release read"),
        schedule.line(name, "release schedule"),
        claim.line(name, "release claim"),
        done.line(name, "release done"),
    ])
}

/// CP-T1.5: the harness itself, run by hand (`--ignored`): kiln's store, Turso's and Amphora's, each
/// operation's p50, p99 and p99.9. It asserts only that every operation did what it should.
#[test]
#[ignore = "a benchmark: run by hand with --ignored --nocapture"]
fn cp_t1_5_records_bench() -> anyhow::Result<()> {
    let ops = input("KILN_RECORDS_BENCH_OPS", TINY_OPS)?;
    let value = input("KILN_RECORDS_BENCH_VALUE", TINY_VALUE)?;
    crate::rt::block_on(async {
        let dir = tempfile::tempdir()?;
        let kept: Arc<dyn Records> = Arc::new(super::StoreRecords::open(crate::store::Store::open(&dir.path().join("kiln.db"))?)?);
        let mut lines = vec![format!("{ops} operations of each kind, {value}-byte records")];
        lines.extend(measure("store", kept, ops, value).await?);
        for engine in super::Engine::ALL {
            let name = format!("{engine:?}").to_ascii_lowercase();
            lines.extend(measure(&name, engine.open(&dir.path().join(format!("{name}.db")))?, ops, value).await?);
        }
        for l in lines {
            println!("{l}");
        }
        Ok(())
    })
}
