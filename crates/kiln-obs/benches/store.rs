//! The store on recorded data, as live ingest (`docs/observe.md` §12.3).
//!
//! `KOBS_CORPUS=<corpus> cargo bench -p kiln-obs --bench store`: the corpus replayed as the batches
//! an ingest node would build, one a `KOBS_WINDOW_S`-second window (default 60) across every
//! series; the builder's and the replica's cost a sample (time, allocations, page faults); memory a
//! series and a sample; selection and range-read latency.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

#[path = "common/corpus.rs"]
mod corpus;

use std::time::Instant;

use hyper_measure::alloc;
use hyper_measure::faults;
use kiln_obs::batch::{Builder, Kind, SeriesKey};
use kiln_obs::hash::Keyed;
use kiln_obs::index::{Clause, NameMatch, Selector, ValueMatch};
use kiln_obs::manifest::Point;
use kiln_obs::store::{Scratch, Store};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

/// `name{a="1",b="x\"y"}` into the metric and its tags.
fn parse_series(s: &str) -> (String, Vec<(String, String)>) {
    let Some((name, rest)) = s.split_once('{') else { return (s.to_owned(), Vec::new()) };
    let mut tags = Vec::new();
    let body = rest.strip_suffix('}').unwrap_or(rest);
    let mut chars = body.chars().peekable();
    loop {
        let key: String = chars.by_ref().take_while(|&c| c != '=').collect();
        if key.is_empty() {
            break;
        }
        if chars.next() != Some('"') {
            break;
        }
        let mut val = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        val.push(n);
                    }
                }
                '"' => break,
                c => val.push(c),
            }
        }
        tags.push((key.trim_start_matches(',').to_owned(), val));
        if chars.peek() == Some(&',') {
            chars.next();
        }
    }
    (name.to_owned(), tags)
}

fn tails(mut ns: Vec<u64>) -> String {
    ns.sort_unstable();
    let at = |q: f64| ns[((ns.len() as f64 * q).ceil() as usize).saturating_sub(1).min(ns.len() - 1)];
    format!("p50 {:.1} µs, p99 {:.1} µs, max {:.1} µs", at(0.5) as f64 / 1e3, at(0.99) as f64 / 1e3, ns[ns.len() - 1] as f64 / 1e3)
}

fn main() {
    let series = match corpus::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    let samples: usize = series.iter().map(|s| s.ts.len()).sum();
    let window_ms = std::env::var("KOBS_WINDOW_S").ok().and_then(|v| v.parse::<i64>().ok()).unwrap_or(60) * 1000;
    let parsed: Vec<(String, Vec<(String, String)>)> = series.iter().map(|s| parse_series(&s.name)).collect();
    let t_min = series.iter().filter_map(|s| s.ts.first()).min().copied().unwrap_or(0);
    let t_max = series.iter().filter_map(|s| s.ts.last()).max().copied().unwrap_or(0);
    println!("corpus: {} series, {samples} samples, {:.1} h; one batch a {} s window", series.len(), (t_max - t_min) as f64 / 3.6e6, window_ms / 1000);

    // The ingest node: each series' key made once and kept; defined in its first batch, named by
    // fingerprint after; samples sorted into Gorilla runs.
    let mut cursors = vec![0usize; series.len()];
    let mut defined = vec![false; series.len()];
    let mut batches = Vec::new();
    let t0 = Instant::now();
    alloc::begin();
    let keys: Vec<SeriesKey> = series
        .iter()
        .zip(&parsed)
        .map(|(s, (m, tags))| SeriesKey::new(m, tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), if s.kind == 1 { Kind::Counter } else { Kind::Gauge }).unwrap())
        .collect();
    let key_counts = alloc::read();
    let mut b = Builder::new();
    let mut w = t_min;
    while w <= t_max {
        let end = w + window_ms;
        for (i, s) in series.iter().enumerate() {
            let c = cursors[i];
            let n = s.ts[c..].partition_point(|&t| t < end);
            if n == 0 {
                continue;
            }
            if !defined[i] {
                b.define(&keys[i]);
                defined[i] = true;
            }
            b.run(keys[i].fp, s.ts[c..c + n].iter().copied().zip(s.values[c..c + n].iter().copied()));
            cursors[i] = c + n;
        }
        batches.push(b.finish());
        w = end;
    }
    let build_counts = alloc::end().less(&key_counts);
    let build = t0.elapsed();
    let batch_bytes: usize = batches.iter().map(Vec::len).sum();
    println!(
        "build:  {:.1} ns a sample; {:.4} allocations a sample (series keys apart: {} once); batches {:.3} bytes a sample ({} batches)",
        build.as_nanos() as f64 / samples as f64,
        build_counts.calls() as f64 / samples as f64,
        key_counts.calls(),
        batch_bytes as f64 / samples as f64,
        batches.len()
    );

    // A replica: apply every batch.
    let mut store = Store::new(Keyed::new());
    let f0 = faults::read().ok();
    let t1 = Instant::now();
    alloc::begin();
    let mut applied = kiln_obs::store::Counts::default();
    for b in &batches {
        let a = store.apply(b).unwrap();
        assert!(a.unknown.is_empty() && a.refused.is_empty());
        applied += a.counts;
    }
    let apply_counts = alloc::end();
    let apply = t1.elapsed();
    let f1 = faults::read().ok();
    assert_eq!(applied.samples as usize, samples);
    let heap = store.heap();
    println!(
        "apply:  {:.1} ns a sample; {:.4} allocations, {:.4} reallocations a sample; {} late, {} duplicates; {} series",
        apply.as_nanos() as f64 / samples as f64,
        apply_counts.allocations as f64 / samples as f64,
        apply_counts.reallocations as f64 / samples as f64,
        applied.late,
        applied.duplicates,
        store.series_count()
    );
    if let (Some(f0), Some(f1)) = (f0, f1) {
        println!("        page faults {} ({:.5} a sample)", f1.since(&f0).total(), f1.since(&f0).total() as f64 / samples as f64);
    }
    println!(
        "memory: {:.2} MB ({:.2} bytes a sample; {:.0} bytes a series of index and symbols): symbols {:.2} MB, index {:.2} MB, samples {:.2} MB",
        heap.total() as f64 / 1e6,
        heap.samples as f64 / samples as f64,
        (heap.symbols + heap.index) as f64 / store.series_count() as f64,
        heap.symbols as f64 / 1e6,
        heap.index as f64 / 1e6,
        heap.samples as f64 / 1e6
    );

    // Selection: by metric, by metric and tag, by wildcard, with a negation.
    let mut ids = Vec::new();
    let names: Vec<String> = store.metric_names().map(str::to_owned).collect();
    let mut sel_ns = Vec::new();
    for name in &names {
        let s = Instant::now();
        store.select(&Selector::metric(name), &mut ids);
        sel_ns.push(s.elapsed().as_nanos() as u64);
    }
    println!("select a metric ({} metrics):           {}", names.len(), tails(sel_ns));
    let job_sel = Selector { metric: NameMatch::Glob("*".into()), clauses: vec![Clause { key: "job".into(), values: vec![ValueMatch::Exact("apiserver".into())], negate: false }] };
    let mut glob_ns = Vec::new();
    for _ in 0..200 {
        let s = Instant::now();
        store.select(&job_sel, &mut ids);
        glob_ns.push(s.elapsed().as_nanos() as u64);
    }
    println!("select every metric with job:apiserver ({} series): {}", ids.len(), tails(glob_ns));
    let wild = Selector { metric: NameMatch::Glob("node_*".into()), clauses: vec![Clause { key: "instance".into(), values: vec![ValueMatch::Glob("*:9100".into())], negate: false }, Clause { key: "mode".into(), values: vec![ValueMatch::Exact("idle".into())], negate: true }] };
    let mut wild_ns = Vec::new();
    for _ in 0..200 {
        let s = Instant::now();
        store.select(&wild, &mut ids);
        wild_ns.push(s.elapsed().as_nanos() as u64);
    }
    println!("select node_* instance:*:9100 !mode:idle ({} series): {}", ids.len(), tails(wild_ns));

    if std::env::args().any(|a| a == "persist") {
        persist(&batches, samples, t_max, t_min);
    }

    // Range reads: the last hour and the whole day of every series.
    let (mut ts, mut vs) = (Vec::new(), Vec::new());
    let mut scratch = Scratch::default();
    for (label, from) in [("last hour", t_max - 3_600_000), ("whole day", t_min)] {
        let t = Instant::now();
        alloc::begin();
        let mut n = 0usize;
        for id in 0..store.series_count() as u32 {
            ts.clear();
            vs.clear();
            store.read_with(id, from, t_max, &mut scratch, &mut ts, &mut vs).unwrap();
            n += ts.len();
        }
        let c = alloc::end();
        let el = t.elapsed();
        println!("read the {label} of every series: {n} samples in {:.1} ms, {:.2} ns a sample, {} allocations", el.as_secs_f64() * 1e3, el.as_nanos() as f64 / n as f64, c.calls());
    }
}

/// §12.4: the same batches into a store on disk, cut every `KOBS_CUT_BATCHES` (default 120: two
/// hours of 60 s batches, Gorilla's and Prometheus's block span) and compacted after each cut.
fn persist(batches: &[Vec<u8>], samples: usize, t_max: i64, t_min: i64) {
    let every = std::env::var("KOBS_CUT_BATCHES").ok().and_then(|v| v.parse().ok()).unwrap_or(120usize);
    let tmp = std::env::temp_dir().join(format!("kobs-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut store = Store::open(&tmp, Keyed::new()).unwrap();
    let mut cut_ns = Vec::new();
    let mut compact_ns = Vec::new();
    let mut written = 0u64;
    let t0 = Instant::now();
    for (i, b) in batches.iter().enumerate() {
        store.apply(b).unwrap();
        if (i + 1) % every == 0 || i + 1 == batches.len() {
            let c = Instant::now();
            let done = store.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
            cut_ns.push(c.elapsed().as_nanos() as u64);
            written += done.bytes_written;
            let c = Instant::now();
            let done = store.compact().unwrap();
            compact_ns.push(c.elapsed().as_nanos() as u64);
            written += done.bytes_written;
        }
    }
    let total = t0.elapsed();
    let disk = store.disk_bytes();
    println!("\npersist: a cut every {every} batches, compaction after each");
    println!("  apply, cut and compact in all: {:.1} ns a sample", total.as_nanos() as f64 / samples as f64);
    println!("  cut {} (head of {} batches)", tails(cut_ns), every);
    println!("  compact {}", tails(compact_ns));
    println!("  on disk: {:.2} MB in {} blocks, {:.3} bytes a sample; written {:.2} MB, write amplification {:.2}", disk as f64 / 1e6, store.block_count(), disk as f64 / samples as f64, written as f64 / 1e6, written as f64 / disk as f64);
    drop(store);
    let f0 = faults::read().ok();
    let t1 = Instant::now();
    alloc::begin();
    let store = Store::open(&tmp, Keyed::new()).unwrap();
    let c = alloc::end();
    let open = t1.elapsed();
    let f1 = faults::read().ok();
    println!("  reopen: {:.1} ms, {} series, {} allocations, {} page faults", open.as_secs_f64() * 1e3, store.series_count(), c.calls(), f0.zip(f1).map_or(0, |(a, b)| b.since(&a).total()));
    let (mut ts, mut vs) = (Vec::new(), Vec::new());
    let mut scratch = Scratch::default();
    for (label, from) in [("last hour", t_max - 3_600_000), ("whole day", t_min)] {
        let t = Instant::now();
        alloc::begin();
        let mut n = 0usize;
        for id in 0..store.series_count() as u32 {
            ts.clear();
            vs.clear();
            store.read_with(id, from, t_max, &mut scratch, &mut ts, &mut vs).unwrap();
            n += ts.len();
        }
        let c = alloc::end();
        let el = t.elapsed();
        println!("  read the {label} of every series from blocks: {n} samples in {:.1} ms, {:.2} ns a sample, {:.2} allocations a series", el.as_secs_f64() * 1e3, el.as_nanos() as f64 / n as f64, c.calls() as f64 / store.series_count() as f64);
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
