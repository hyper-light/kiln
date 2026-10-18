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
    if std::env::args().any(|a| a == "long") {
        long_ranges(&series);
        return;
    }
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

/// Long ranges (`docs/observe.md` §4.7, §12.16): the corpus's node and apiserver series replayed for
/// 30 days into a store on disk (`KOBS_DIR`, else the system's temporary folder), cut every six
/// hours of data and merged as a replica's maintenance merges; queries over the last day, week and
/// month at 240 points, rollups read and not.
fn long_ranges(series: &[corpus::Series]) {
    use kiln_obs::batch::{Builder, Kind, SeriesKey};
    use kiln_obs::manifest::Point;
    let keep: Vec<&corpus::Series> = series.iter().filter(|s| s.name.starts_with("node_") || s.name.starts_with("apiserver_request_total")).collect();
    let keys: Vec<SeriesKey> = keep
        .iter()
        .map(|s| {
            let (m, tags) = load::parse_series(&s.name);
            SeriesKey::new(&m, tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), if s.kind == 1 { Kind::Counter } else { Kind::Gauge }).unwrap()
        })
        .collect();
    let t0 = keep.iter().filter_map(|s| s.ts.first()).min().copied().unwrap_or(0);
    let day: i64 = 86_400_000;
    let days = 30i64;
    let base = std::env::var_os("KOBS_DIR").map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let dir = tempfile::tempdir_in(base).unwrap();
    let mut store = kiln_obs::store::Store::open(dir.path(), kiln_obs::hash::Keyed::with_keys(1, 2)).unwrap();
    let (mut samples, mut cuts) = (0u64, 0u64);
    let six = 6 * 3_600_000;
    let mut cursors = vec![0usize; keep.len()];
    let started = Instant::now();
    let mut merging = std::time::Duration::ZERO;
    for d in 0..days {
        cursors.iter_mut().for_each(|c| *c = 0);
        for part in 0..4 {
            let (lo, hi) = (t0 + part * six, t0 + (part + 1) * six);
            let mut b = Builder::new();
            for ((s, k), c) in keep.iter().zip(&keys).zip(cursors.iter_mut()) {
                b.define(k);
                let start = *c;
                while s.ts.get(*c).is_some_and(|&t| t < hi) {
                    *c += 1;
                }
                let run: Vec<(i64, f64)> = s.ts[start..*c].iter().zip(&s.values[start..*c]).filter(|(t, _)| **t >= lo).map(|(&t, &v)| (t + d * day, v)).collect();
                samples += run.len() as u64;
                b.run(k.fp, run);
            }
            store.apply(&b.finish()).unwrap();
            cuts += 1;
            store.cut(Point { index: cuts, term: 1 }).unwrap();
            let m = Instant::now();
            store.compact().unwrap();
            merging += m.elapsed();
        }
    }
    let (bytes, rolled) = store.block_bytes();
    println!(
        "long ranges: {} series, {samples} samples over {days} days, {cuts} cuts; {} blocks, {:.1} MB ({:.2} bytes a sample), rollups {:.1}% of them; written in {:.1} s, merges {:.1} s",
        keep.len(),
        store.block_count(),
        bytes as f64 / 1e6,
        bytes as f64 / samples as f64,
        100.0 * rolled as f64 / bytes as f64,
        started.elapsed().as_secs_f64(),
        merging.as_secs_f64()
    );
    let end = t0 + days * day;
    // `KOBS_PROFILE=<query>`: that query alone, over the month at 240 points, rollups read, again
    // and again (what a sampling profile is taken of).
    if let Ok(q) = std::env::var("KOBS_PROFILE") {
        let mut req = Request::new(&q, end - 30 * day, end);
        req.points = 240;
        let n: u32 = std::env::var("KOBS_PROFILE_N").ok().and_then(|n| n.parse().ok()).unwrap_or(40);
        let t = Instant::now();
        for _ in 0..n {
            eval(&store, &req).unwrap();
        }
        println!("{q}: {:.2} ms a query", t.elapsed().as_secs_f64() * 1e3 / f64::from(n));
        return;
    }
    println!("{:<54} {:>5} {:>6} {:>7} {:>11} {:>10} {:>10} {:>8} {:>8}", "query", "range", "points", "series", "samples", "raw p50", "rolled p50", "allocs", "(rolled)");
    for q in ["avg:node_load1{*} by {instance}", "max:node_memory_MemAvailable_bytes{*}", "avg:node_*{*} by {instance}", "sum:node_cpu_seconds_total{mode:idle} by {instance}", "sum:apiserver_request_total{*} by {verb}"] {
        for ((label, range), points) in [("1 d", day), ("7 d", 7 * day), ("30 d", 30 * day)].into_iter().flat_map(|r| [(r, 240usize), (r, 1_200)]) {
            let mut req = Request::new(q, end - range, end);
            req.points = points;
            let mut p50 = [0.0f64; 2];
            let mut allocs = [0u64; 2];
            let mut answers = Vec::new();
            for (k, on) in [false, true].into_iter().enumerate() {
                store.set_read_rollups(on);
                let mut ns = Vec::new();
                for i in 0..5 {
                    let t = Instant::now();
                    alloc::begin();
                    let a = eval(&store, &req).unwrap();
                    let c = alloc::end();
                    ns.push(t.elapsed().as_nanos() as u64);
                    if i == 1 {
                        allocs[k] = c.calls();
                        answers.push(a);
                    }
                }
                ns.sort_unstable();
                p50[k] = ns[ns.len() / 2] as f64 / 1e6;
            }
            let same = answers[0].series.len() == answers[1].series.len() && answers[0].series.iter().zip(&answers[1].series).all(|(a, b)| a.values.iter().zip(&b.values).all(|(x, y)| x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan())));
            assert!(same, "{q} over {label}: the answers differ");
            println!("{q:<54} {label:>5} {points:>6} {:>7} {:>11} {:>8.2}ms {:>8.2}ms {:>8} {:>8}", answers[1].series.len(), answers[1].scanned_samples, p50[0], p50[1], allocs[0], allocs[1]);
        }
    }
    store.set_read_rollups(true);
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
