//! Queries on recorded data (`docs/observe.md` §12.5): dashboard queries over the corpus's day,
//! each run repeatedly; latency tails, samples scanned, nanoseconds a sample and allocations (a
//! query's second run's: a timezone's first use in a process loads it, once). Before them, what a
//! query's shape costs in allocations to parse and to evaluate, on an empty store.
//!
//! `KOBS_CORPUS=<corpus> cargo bench -p kiln-obs --bench query`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

#[path = "common/corpus.rs"]
mod corpus;
#[path = "common/load.rs"]
mod load;

use std::time::Instant;

use hyper_measure::alloc;
use kiln_obs::query::eval::{Request, eval};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn main() {
    println!("{:<60} {:>7} {:>7}", "a query's shape, on an empty store", "parse", "eval");
    let empty = kiln_obs::store::Store::new(kiln_obs::hash::Keyed::with_keys(1, 2));
    for q in ["avg:x{*}.rollup(avg, 60)", "avg:x{*}.rollup(avg, daily)", "avg:x{*}.rollup(avg, daily, 'America/New_York')", "avg:x{*}.weighted()"] {
        let r = Request::new(q, 0, 86_400_000);
        let _ = eval(&empty, &r);
        alloc::begin();
        let _ = kiln_obs::query::parse::parse(q);
        let p = alloc::end();
        alloc::begin();
        let _ = eval(&empty, &r);
        let e = alloc::end();
        println!("{q:<60} {:>7} {:>7}", p.calls(), e.calls());
    }
    println!();
    distributions();
    let series = match corpus::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    let t_min = series.iter().filter_map(|s| s.ts.first()).min().copied().unwrap_or(0);
    let t_max = series.iter().filter_map(|s| s.ts.last()).max().copied().unwrap_or(0);
    let store = load::store_of(&series);
    println!("corpus in a store: {} series; the queries over its {:.1} h, 1,200 points a series at most\n", store.series_count(), (t_max - t_min) as f64 / 3.6e6);
    let queries = [
        "avg:node_load1{*} by {instance}",
        "p99:node_load1{*}",
        "sum:node_cpu_seconds_total{mode:idle} by {instance}",
        "sum:apiserver_request_total{*} by {verb}",
        "100 * sum:apiserver_request_total{code:5*} / sum:apiserver_request_total{*}",
        "top(avg:container_memory_working_set_bytes{*} by {pod}, 10, 'mean', 'desc')",
        "avg:node_*{*} by {instance}",
        "sum:*{job:apiserver}",
        // The calendar's grid (a local day's periods, their edges from the tz database) and the
        // weights (each series' interval learned, its samples counted a bucket), on the widest read.
        "avg:node_load1{*} by {instance}.rollup(avg, daily)",
        "avg:node_load1{*} by {instance}.rollup(avg, daily, 'America/New_York')",
        "avg:node_*{*} by {instance}.rollup(avg, daily, 'America/New_York')",
        "avg:node_*{*} by {instance}.weighted()",
    ];
    println!("{:<80} {:>7} {:>10} {:>11} {:>11} {:>8} {:>9}", "query", "series", "samples", "p50", "p99", "ns/smp", "allocs");
    for q in queries {
        let req = Request::new(q, t_min, t_max);
        let mut ns = Vec::new();
        let mut answer = None;
        let mut allocs = 0;
        for i in 0..15 {
            let t = Instant::now();
            alloc::begin();
            let a = eval(&store, &req).unwrap();
            let c = alloc::end();
            ns.push(t.elapsed().as_nanos() as u64);
            if i == 1 {
                allocs = c.calls();
            }
            answer = Some(a);
        }
        let a = answer.unwrap();
        ns.sort_unstable();
        let p = |q: f64| ns[((ns.len() as f64 * q).ceil() as usize).saturating_sub(1).min(ns.len() - 1)] as f64 / 1e6;
        println!(
            "{:<80} {:>7} {:>10} {:>9.2}ms {:>9.2}ms {:>8.2} {:>9}",
            q,
            a.series.len(),
            a.scanned_samples,
            p(0.5),
            p(0.99),
            p(0.5) * 1e6 / a.scanned_samples.max(1) as f64,
            allocs
        );
    }
}

/// Distributions (`docs/observe.md` §4.4): 50 hosts' latencies, 100 values every 10 s for an hour
/// (log-normal, each host its own level), as their buckets' series; a percentile across them and by
/// host, the average, and a heatmap, beside a plain gauge read of as many samples.
fn distributions() {
    use kiln_obs::batch::Builder;
    let mut store = kiln_obs::store::Store::new(kiln_obs::hash::Keyed::with_keys(1, 2));
    let mut seed = 5u64;
    let mut uniform = move || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 11) as f64 / (1u64 << 53) as f64).max(1e-300)
    };
    let (hosts, steps, per) = (50usize, 360i64, 100usize);
    let mut b = Builder::new();
    let mut bucket_series = 0usize;
    let mut samples = 0u64;
    for h in 0..hosts {
        let host = format!("h{h}");
        let level = -4.0 + 2.0 * h as f64 / hosts as f64;
        let points: Vec<(i64, Vec<f64>)> = (0..steps).map(|k| (k * 10_000, (0..per).map(|_| ((-2.0 * uniform().ln()).sqrt() * (std::f64::consts::TAU * uniform()).cos() * 0.5 + level).exp()).collect())).collect();
        for (key, pts) in kiln_obs::dist::series_of("lat", &[("host", host.as_str())], &points).unwrap() {
            bucket_series += usize::from(kiln_obs::batch::parse_key(&key.key).is_ok_and(|p| p.metric == "lat.dist"));
            samples += pts.len() as u64;
            b.define(&key);
            b.run(key.fp, pts);
        }
        // A gauge beside it, read as often.
        let g = kiln_obs::batch::SeriesKey::new("plain", [("host", host.as_str())], kiln_obs::batch::Kind::Gauge).unwrap();
        b.define(&g);
        b.run(g.fp, (0..steps * 100).map(|k| (k * 100, k as f64)));
        let batch = b.finish();
        store.apply(&batch).unwrap();
    }
    println!("distributions: {hosts} hosts, {bucket_series} bucket series, {samples} samples; over the hour, 120 points");
    println!("{:<44} {:>7} {:>10} {:>11} {:>11} {:>9}", "query", "series", "samples", "p50", "p99", "allocs");
    for (q, heat) in [("p99:lat{*}", false), ("p99:lat{*} by {host}", false), ("avg:lat{*}", false), ("p99:lat{*} (heatmap)", true), ("avg:plain{*}", false)] {
        let query = q.trim_end_matches(" (heatmap)");
        let mut r = Request::new(query, 0, steps * 10_000);
        r.points = 120;
        let mut ns = Vec::new();
        let (mut allocs, mut read, mut series) = (0, 0, 0);
        for i in 0..15 {
            let t = Instant::now();
            alloc::begin();
            if heat {
                let h = kiln_obs::query::eval::heatmap(&store, &r, 40).unwrap();
                series = h.rows.len();
            } else {
                let a = eval(&store, &r).unwrap();
                series = a.series.len();
                read = a.scanned_samples;
            }
            let c = alloc::end();
            ns.push(t.elapsed().as_nanos() as u64);
            if i == 1 {
                allocs = c.calls();
            }
        }
        ns.sort_unstable();
        let p = |q: f64| ns[((ns.len() as f64 * q).ceil() as usize).saturating_sub(1).min(ns.len() - 1)] as f64 / 1e6;
        println!("{q:<44} {series:>7} {read:>10} {:>9.2}ms {:>9.2}ms {allocs:>9}", p(0.5), p(0.99));
    }
    println!();
}
