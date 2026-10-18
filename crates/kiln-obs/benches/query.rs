//! Queries on recorded data (`docs/observe.md` §12.5): dashboard queries over the corpus's day,
//! each run repeatedly; latency tails, samples scanned, nanoseconds a sample and allocations.
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
            if i == 0 {
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
