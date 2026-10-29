//! Metrics from logs on recorded logs (`docs/observe.md` §12.18): what noting each batch's windows
//! costs the replica that applies, and what counting a window costs, a metric and five at once,
//! counting every window of the records once and as the server does it, each a little after it ends.
//!
//! `KOBS_LOGS=<journal>:<pods> cargo bench -p kiln-obs --bench logmetrics` (`KOBS_CASE=dense` or
//! `sparse`: that case alone). `KOBS_CASE=fleet`, no corpus: a fleet's records made at its rate
//! (`KOBS_RATE` records a second, 100,000; for `KOBS_SECS`, 30), counted at three fleets' sizes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::sync::Arc;
use std::time::Instant;

use hyper_measure::alloc;
use kiln_obs::batch::Builder;
use kiln_obs::command;
use kiln_obs::logs::metrics::{Compute, LogMetric, Touched, WINDOW, window_of};
use kiln_obs::logs::search::parse;
use kiln_obs::logs::store::Facet;
use kiln_obs::store::Store;

#[path = "common/loglines.rs"]
mod loglines;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

/// Records an entry carries: an OTLP export's worth.
const PER_ENTRY: usize = 1_000;

fn entries(records: &[kiln_obs::logs::record::Record]) -> Vec<Vec<u8>> {
    let at = records.last().map_or(0, |r| r.ts / 1_000_000) + 60_000;
    let mut b = Builder::new();
    records
        .chunks(PER_ENTRY)
        .enumerate()
        .map(|(i, chunk)| {
            b.logs(chunk);
            command::write(7, i as u64 + 1, at, &b.finish())
        })
        .collect()
}

fn apply_all(entries: &[Vec<u8>], touched: Option<Arc<Touched>>) -> (f64, u64, Store) {
    let mut store = Store::default();
    store.set_touched(touched);
    alloc::begin();
    let t = Instant::now();
    for (i, e) in entries.iter().enumerate() {
        store.apply_entry(i as u64 + 1, e).unwrap();
    }
    let el = t.elapsed().as_secs_f64();
    (el, alloc::end().calls(), store)
}

fn metric(name: &str, query: &str, by: &[&str], compute: Compute) -> LogMetric {
    LogMetric { name: name.into(), search: parse(query).unwrap(), by: by.iter().map(|b| (Facet::parse(b), b.trim_start_matches('@').to_owned())).collect(), compute, since: 0 }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    if std::env::var("KOBS_CASE").is_ok_and(|c| c == "fleet") {
        return fleet();
    }
    let (mut records, raw_bytes) = match loglines::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    // The records moved to end now, so the replica notes the windows of their last twenty minutes,
    // as it would of records coming now.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64;
    let shift = now - records.last().map_or(now, |r| r.ts);
    for r in &mut records {
        r.ts += shift;
    }
    let n = records.len();
    let entries = entries(&records);
    let (first, last) = (records.first().unwrap().ts, records.last().unwrap().ts);
    println!("records: {n} ({:.1} MB of lines), {} entries of {PER_ENTRY}, {:.1} hours", raw_bytes as f64 / 1e6, entries.len(), (last - first) as f64 / 3.6e12);

    // Applying, with the windows noted and without, taken in turn in each of nine rounds.
    println!("\n{:<34} {:>14} {:>16}", "applying (median of 9)", "records/s", "allocations/rec");
    let (mut plain, mut noted) = (Vec::new(), Vec::new());
    let mut store = None;
    for _ in 0..9 {
        let (s, a, _) = apply_all(&entries, None);
        plain.push((s, a));
        let t = Arc::new(Touched::new());
        let (s, a, st) = apply_all(&entries, Some(t.clone()));
        noted.push((s, a));
        let spans = t.take();
        assert!(!spans.windows.is_empty() && spans.batches > 0);
        store = Some(st);
    }
    let store = store.unwrap();
    for (what, runs) in [("windows not noted", plain), ("each batch's windows noted", noted)] {
        let secs = median(runs.iter().map(|r| r.0).collect());
        let allocs = runs.iter().map(|r| r.1).min().unwrap_or(0);
        println!("{:<34} {:>14.0} {:>16.3}", what, n as f64 / secs, allocs as f64 / n as f64);
    }

    let one = vec![metric("corpus.errors", "status:error", &["service", "host"], Compute::Count)];
    let five = vec![
        metric("corpus.records", "*", &["service"], Compute::Count),
        metric("corpus.errors", "status:error", &["service", "host"], Compute::Count),
        metric("kube.warnings", "service:kube status:warn", &[], Compute::Count),
        metric("refused", "\"connection refused\"", &["service"], Compute::Count),
        metric("argocd.grpc", "service:argocd @grpc.time_ms:*", &["@grpc.service"], Compute::Distribution { path: "@grpc.time_ms".into(), percentiles: true }),
    ];
    // `KOBS_CASE=dense` (or `sparse`): that case alone, for a profile of it.
    let case = std::env::var("KOBS_CASE").unwrap_or_default();
    if case != "dense" {
        count_all("every window of the 48 hours", &store, &one, &five, first, last);
    }
    if case == "sparse" {
        return;
    }

    // The same records in 100 s (19,500 a window, 1,950 a second): what a record costs.
    let span = last - first;
    for r in &mut records {
        r.ts = last - 100_000_000_000 + ((r.ts - first) as i128 * 100_000_000_000 / span.max(1) as i128) as i64;
    }
    let (_, _, dense) = apply_all(&entries_of(&records), None);
    count_all("the records in 100 s", &dense, &one, &five, records.first().unwrap().ts, last);
}

fn entries_of(records: &[kiln_obs::logs::record::Record]) -> Vec<Vec<u8>> {
    entries(records)
}

/// Every window from `first` to `last` counted, once each, for one metric and for five.
fn count_all(what: &str, store: &Store, one: &[LogMetric], five: &[LogMetric], first: i64, last: i64) {
    let windows: Vec<i64> = (window_of(first)..=window_of(last)).step_by(WINDOW as usize).collect();
    println!("\n{:<34} {:>10} {:>12} {:>12} {:>10} {:>12} {:>14}", format!("{what} (median of 5)"), "windows", "ms/window", "rows/s", "ns/row", "points", "allocs/window");
    for (what, set) in [("one metric", one), ("five metrics", five)] {
        let mut runs = Vec::new();
        let (mut points, mut allocs, mut rows) = (0usize, u64::MAX, 0u64);
        for _ in 0..5 {
            let (mut p, mut r) = (0usize, 0u64);
            alloc::begin();
            let t = Instant::now();
            for &w in &windows {
                let c = store.count_log_metrics(set, w).unwrap().unwrap();
                p += c.series.len();
                r += c.rows_scanned;
            }
            runs.push(t.elapsed().as_secs_f64());
            allocs = allocs.min(alloc::end().calls());
            (points, rows) = (p, r);
        }
        let secs = median(runs);
        println!(
            "{:<34} {:>10} {:>12.3} {:>12.0} {:>10.0} {:>12} {:>14.1}",
            what,
            windows.len(),
            secs * 1e3 / windows.len() as f64,
            rows as f64 / secs,
            secs * 1e9 / rows.max(1) as f64,
            points,
            allocs as f64 / windows.len() as f64
        );
    }
}

/// The services of a fleet's hosts: Argo CD's records a gRPC call's time each, kube's warnings among
/// what the five metrics count.
const FLEET_SERVICES: [&str; 10] = ["web", "api", "auth", "db", "cache", "queue", "argocd", "kube", "billing", "search"];

/// A fleet's records, written as entries as they are made (none kept): `hosts` hosts each running
/// the ten services, `rate` a second for `secs` seconds ending at `end` (ns), each host's services in
/// turn; a fiftieth errors, a twentieth warnings; messages of six shapes, numbers in them.
fn fleet_entries(hosts: usize, rate: u64, secs: u64, end: i64) -> Vec<Vec<u8>> {
    let host_names: Vec<String> = (0..hosts).map(|h| format!("node-{h:05}")).collect();
    let total = rate * secs;
    let step = 1_000_000_000 / rate.max(1) as i64;
    let start = end - secs as i64 * 1_000_000_000;
    let at = end / 1_000_000 + 60_000;
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut b = Builder::new();
    let mut chunk = Vec::with_capacity(PER_ENTRY);
    let mut out = Vec::new();
    for i in 0..total {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let service = FLEET_SERVICES[(i % FLEET_SERVICES.len() as u64) as usize];
        let host = &host_names[((i / FLEET_SERVICES.len() as u64) % hosts as u64) as usize];
        let severity = if rng.is_multiple_of(50) {
            17
        } else if rng.is_multiple_of(20) {
            13
        } else {
            9
        };
        let n = rng >> 20;
        let message = match (rng >> 8) % 6 {
            0 => format!("GET /api/v1/orders/{} 200 {} ms", n % 100_000, n % 500),
            1 => format!("connection refused to db-{}:5432", n % 8),
            2 => format!("job {} done in {} ms", n % 1_000_000, n % 9_000),
            3 => format!("cache miss for key user:{}", n % 50_000),
            4 => format!("POST /v1/charge {} {} ms", [200, 201, 402, 500][(n % 4) as usize], n % 800),
            _ => format!("reconciled {} objects in {} ms", n % 300, n % 2_000),
        };
        let attrs = if service == "argocd" {
            vec![("grpc.service".into(), ["application", "repository", "cluster"][(n % 3) as usize].into()), ("grpc.time_ms".into(), (n % 2_000).to_string())]
        } else {
            Vec::new()
        };
        chunk.push(kiln_obs::logs::record::Record {
            ts: start + i as i64 * step,
            severity,
            service: service.into(),
            host: host.clone(),
            source: "app".into(),
            message,
            attrs,
            trace_id: None,
            span_id: None,
        });
        if chunk.len() == PER_ENTRY {
            b.logs(&chunk);
            out.push(command::write(7, out.len() as u64 + 1, at, &b.finish()));
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        b.logs(&chunk);
        out.push(command::write(7, out.len() as u64 + 1, at, &b.finish()));
    }
    out
}

/// Metrics from logs at a fleet's rates: its records applied, then every window counted (one metric
/// and five), at 20, 200 and 2,000 hosts of ten services each (`status:error` by service and host
/// 200, 2,000 and 20,000 groups: the last past OpenTelemetry's limit, its rest in the overflow).
fn fleet() {
    let rate: u64 = std::env::var("KOBS_RATE").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000);
    let secs: u64 = std::env::var("KOBS_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
    let one = vec![metric("fleet.errors", "status:error", &["service", "host"], Compute::Count)];
    let five = vec![
        metric("fleet.records", "*", &["service"], Compute::Count),
        metric("fleet.errors", "status:error", &["service", "host"], Compute::Count),
        metric("kube.warnings", "service:kube status:warn", &[], Compute::Count),
        metric("refused", "\"connection refused\"", &["service"], Compute::Count),
        metric("argocd.grpc", "service:argocd @grpc.time_ms:*", &["@grpc.service"], Compute::Distribution { path: "@grpc.time_ms".into(), percentiles: true }),
    ];
    // The windows whole: the records end where one does.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64;
    let end = window_of(now);
    println!("a fleet's records: {rate} a second for {secs} s ({} a window of {} s)", rate * (WINDOW as u64 / 1_000_000_000), WINDOW / 1_000_000_000);
    for hosts in [20usize, 200, 2_000] {
        let entries = fleet_entries(hosts, rate, secs, end);
        let (applied, allocs, store) = apply_all(&entries, None);
        let n = rate * secs;
        println!("\n{hosts} hosts of {} services: applied {:.0} records/s, {:.3} allocations a record", FLEET_SERVICES.len(), n as f64 / applied, allocs as f64 / n as f64);
        let first = end - secs as i64 * 1_000_000_000;
        count_all(&format!("{hosts} hosts, every window"), &store, &one, &five, first, end - 1);
        // The groups and the overflow of the last window, one metric: what the limit kept.
        if let Ok(Some(c)) = store.count_log_metrics(&one, window_of(end - 1)) {
            let t = c.tallies.first().cloned().unwrap_or_default();
            println!("{:<34} {} records, {} groups, {} in the overflow", "  its last window", t.records, t.groups, t.overflowed);
        }
    }
}
