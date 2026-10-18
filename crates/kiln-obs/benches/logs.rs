//! Logs on recorded data (`docs/observe.md` §12.8): kiln's own log lines as records, ingested
//! through batches into a store on disk, then searched.
//!
//! `KOBS_LOGS=<file of log lines>[:<file>...] cargo bench -p kiln-obs --bench logs`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::time::Instant;

use hyper_measure::alloc;
use kiln_obs::batch::Builder;
use kiln_obs::hash::Keyed;
use kiln_obs::logs::search::parse;
use kiln_obs::manifest::Point;
use kiln_obs::store::Store;

#[path = "common/loglines.rs"]
mod loglines;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn main() {
    let (records, raw_bytes) = match loglines::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    println!("records: {} ({:.1} MB of lines)", records.len(), raw_bytes as f64 / 1e6);
    let dir = std::env::temp_dir().join(format!("kobs-logs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut store = Store::open(&dir, Keyed::new()).unwrap();
    // Ingest: batches of 1,000 records (an OTLP export's worth), a cut every 50 batches.
    let mut b = Builder::new();
    let t = Instant::now();
    alloc::begin();
    let mut batch_bytes = 0usize;
    for (i, chunk) in records.chunks(1000).enumerate() {
        b.logs(chunk);
        let batch = b.finish();
        batch_bytes += batch.len();
        store.apply(&batch).unwrap();
        if i % 50 == 49 {
            store.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
        }
    }
    store.cut(Point { index: u64::MAX, term: 1 }).unwrap();
    let c = alloc::end();
    let el = t.elapsed();
    println!(
        "ingest (build, apply, seal, cut): {:.0} records/s, {:.1} MB/s of lines; {:.2} allocations a record; batches {:.2} bytes a line byte; on disk {:.2} MB ({:.3} of the lines)",
        records.len() as f64 / el.as_secs_f64(),
        raw_bytes as f64 / el.as_secs_f64() / 1e6,
        c.calls() as f64 / records.len() as f64,
        batch_bytes as f64 / raw_bytes as f64,
        store.log_bytes() as f64 / 1e6,
        store.log_bytes() as f64 / raw_bytes as f64
    );
    drop(store);
    let t = Instant::now();
    let store = Store::open(&dir, Keyed::new()).unwrap();
    println!("reopen: {:.1} ms; {:.2} MB of summaries in memory", t.elapsed().as_secs_f64() * 1e3, store.heap().index as f64 / 1e6);
    let (t0, t1) = (records.first().unwrap().ts, records.last().unwrap().ts);
    let rare = records[records.len() / 20].message.split_whitespace().find(|w| w.len() > 12 && w.chars().all(char::is_alphanumeric)).unwrap_or("rare").to_owned();
    let queries = [
        "*".to_owned(),
        "status:error".to_owned(),
        "service:kube status:warn".to_owned(),
        "\"connection refused\"".to_owned(),
        "@application:kiln-namespaces".to_owned(),
        "nosuchtokenanywhere".to_owned(),
        rare.clone(),
        "service:argocd -status:info".to_owned(),
    ];
    println!("\n{:<44} {:>8} {:>10} {:>10} {:>9} {:>10} {:>10}", "search (newest 100)", "found", "blocks", "pruned", "rows", "p50", "p99");
    for q in &queries {
        let s = parse(q).unwrap();
        let mut ns = Vec::new();
        let mut last = None;
        for _ in 0..9 {
            let t = Instant::now();
            let f = store.search_logs(&s, t0, t1, 100).unwrap();
            ns.push(t.elapsed().as_nanos() as u64);
            last = Some(f);
        }
        ns.sort_unstable();
        let f = last.unwrap();
        println!("{:<44} {:>8} {:>10} {:>10} {:>9} {:>8.2}ms {:>8.2}ms", q, f.records.len(), f.blocks_total, f.blocks_pruned, f.rows_scanned, ns[ns.len() / 2] as f64 / 1e6, ns[ns.len() - 1] as f64 / 1e6);
    }
    // Log analytics: every matching record grouped and measured, as the explorer's facets and a log
    // monitor read them.
    use kiln_obs::logs::store::{Facet, Measure};
    let aggregations: [(&str, &str, Vec<Facet>, Measure); 5] = [
        ("count by {service}", "*", vec![Facet::Service], Measure::Count),
        ("count by {status}", "*", vec![Facet::Status], Measure::Count),
        ("count by {service,status}", "*", vec![Facet::Service, Facet::Status], Measure::Count),
        ("count by {@application}", "*", vec![Facet::Attr("application".into())], Measure::Count),
        ("cardinality(host) by {service}", "status:error", vec![Facet::Service], Measure::Cardinality(Facet::Host)),
    ];
    println!("\n{:<44} {:>8} {:>10} {:>12} {:>10} {:>10}", "aggregate (whole range)", "groups", "rows", "allocs/row", "p50", "p99");
    for (name, q, by, m) in &aggregations {
        let s = parse(q).unwrap();
        let mut ns = Vec::new();
        let mut last = None;
        let mut allocs = 0u64;
        for _ in 0..9 {
            alloc::begin();
            let t = Instant::now();
            let a = store.aggregate_logs(&s, t0, t1, by, m).unwrap();
            ns.push(t.elapsed().as_nanos() as u64);
            allocs = alloc::end().calls();
            last = Some(a);
        }
        ns.sort_unstable();
        let a = last.unwrap();
        println!("{:<44} {:>8} {:>10} {:>12.4} {:>8.2}ms {:>8.2}ms", name, a.groups.len(), a.rows_scanned, allocs as f64 / a.rows_scanned.max(1) as f64, ns[ns.len() / 2] as f64 / 1e6, ns[ns.len() - 1] as f64 / 1e6);
    }
    // The explorer's facet list (four facets, one pass) and its histogram (count by status, 60 buckets).
    let all = parse("*").unwrap();
    let four = [Facet::Service, Facet::Host, Facet::Source, Facet::Status];
    let step = ((t1 - t0) / 60).max(1);
    for (name, run) in [
        ("facets: service, host, source, status (1 pass)", Box::new(|| store.log_facets(&all, t0, t1, &four, 10).map(|f| (f.facets.len(), f.rows_scanned))) as Box<dyn Fn() -> Result<(usize, u64), kiln_obs::logs::store::AggregateError>>),
        ("histogram: count by {status}, 60 buckets", Box::new(|| store.aggregate_logs_buckets(&all, t0, t1, step, &[Facet::Status], &Measure::Count).map(|b| (b.groups.len(), b.rows_scanned)))),
    ] {
        let mut ns = Vec::new();
        let mut allocs = 0u64;
        let mut last = (0, 0);
        for _ in 0..9 {
            alloc::begin();
            let t = Instant::now();
            last = run().unwrap();
            ns.push(t.elapsed().as_nanos() as u64);
            allocs = alloc::end().calls();
        }
        ns.sort_unstable();
        println!("{:<44} {:>8} {:>10} {:>12.4} {:>8.2}ms {:>8.2}ms", name, last.0, last.1, allocs as f64 / last.1.max(1) as f64, ns[ns.len() / 2] as f64 / 1e6, ns[ns.len() - 1] as f64 / 1e6);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
