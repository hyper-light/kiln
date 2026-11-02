//! Log indexes (`docs/observe.md` §12.35): what routing costs a record where it is taken, and what
//! parts cost a search. Records as a busy node takes them (an access log's lines of five services,
//! a tenth debug, some with a trace id), 100,000 of them, routed: as rows a collector wrote (in
//! segments of a block's text, as `kiln_obs::logs::store::BLOCK_TEXT` cuts them) and as records'
//! runs, by `main` as kiln makes it (every record into it, nothing read), and by four indexes of
//! services, `main` last, excluding every debug record and half of `web`'s. Time (the least of
//! nine, and their median) and allocations, a record. Then a store of 2,000,000 records searched,
//! its records in one part, and the same records in four parts (as four indexes would keep them),
//! written a thousand at a time (a head is sealed whole once it holds a block's text: writes of a
//! size seal blocks alike, one part or four): the newest 100, a service's newest 100, and a count by
//! status of all.
//!
//! `cargo bench -p kiln --bench indexes` (`KOBS_RECORDS`: how many routed, 100,000).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::time::{Duration, Instant};

use hyper_measure::alloc;
use kiln::observe::indexes::{ExclusionDef, ExclusionQuery, Filter, IndexDef, Kept, compile, main_def, route_logs_by};
use kiln::observe::ingest::{LogPart, Logs};
use kiln_obs::hash::Keyed;
use kiln_obs::logs::record::{Record, put_row};
use kiln_obs::logs::store::{Facet, Measure};
use kiln_obs::store::Store;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const SERVICES: [&str; 5] = ["web", "api", "payments", "search", "audit"];

fn record(i: usize) -> Record {
    let service = SERVICES[i % 5];
    Record {
        ts: 1_792_324_800_000_000_000 + i as i64 * 1_000_000,
        severity: if i % 10 == 3 {
            5
        } else if i.is_multiple_of(17) {
            17
        } else {
            9
        },
        service: service.into(),
        host: format!("node-{}", i % 40),
        source: "nginx".into(),
        message: format!("10.0.{}.{} - - \"GET /api/v1/items/{} HTTP/1.1\" {} {} \"-\" \"curl/8.9\"", i % 250, i % 200, i % 9_973, if i.is_multiple_of(17) { 500 } else { 200 }, 512 + i % 4096),
        attrs: vec![("http.status_code".into(), if i.is_multiple_of(17) { "500" } else { "200" }.into()), ("http.method".into(), "GET".into())],
        trace_id: i.is_multiple_of(3).then(|| ((i / 3) as u128).to_le_bytes()),
        span_id: None,
    }
}

/// `n` records as a collector's segments of rows.
fn rows(n: usize) -> Vec<LogPart> {
    let mut out = Vec::new();
    let (mut raw, mut prev, mut count) = (Vec::new(), 0i64, 0u32);
    for i in 0..n {
        let r = record(i);
        put_row(&mut raw, &r, prev);
        prev = r.ts;
        count += 1;
        if raw.len() >= kiln_obs::logs::block::BLOCK_TEXT {
            out.push(LogPart::Rows { raw: std::mem::take(&mut raw), rows: count, last_ts: prev });
            (prev, count) = (0, 0);
        }
    }
    if count > 0 {
        out.push(LogPart::Rows { raw, rows: count, last_ts: prev });
    }
    out
}

/// `n` records as runs of a thousand.
fn runs(n: usize) -> Vec<LogPart> {
    (0..n).collect::<Vec<_>>().chunks(1000).map(|c| LogPart::Records(c.iter().map(|&i| record(i)).collect())).collect()
}

fn index(name: &str, part: u16, query: &str, exclusions: Vec<ExclusionDef>) -> Kept {
    Kept { def: IndexDef { name: name.into(), filter: Filter { query: query.into() }, exclusion_filters: exclusions, ..main_def() }, part, created: 0, modified: 0 }
}

/// The least of nine and their median.
#[derive(Clone, Copy)]
struct Took(Duration, Duration);

/// What `f` takes (nine runs, each on what `make` makes, made apart) and allocates (its second run);
/// what it gives back let go after it is timed (100,000 records freed are not routing's time).
fn measure<T, R>(mut make: impl FnMut() -> T, mut f: impl FnMut(T) -> R) -> (Took, alloc::Counts) {
    let mut times = Vec::new();
    let mut counted = alloc::Counts::default();
    for n in 0..9 {
        let input = make();
        let t = Instant::now();
        if n == 1 {
            alloc::begin();
        }
        let out = std::hint::black_box(f(input));
        if n == 1 {
            counted = alloc::end();
        }
        times.push(t.elapsed());
        drop(out);
    }
    times.sort();
    (Took(times[0], times[times.len() / 2]), counted)
}

fn main() {
    let n: usize = std::env::var("KOBS_RECORDS").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000);
    let main_alone = compile(&[Kept { def: main_def(), part: 0, created: 0, modified: 0 }], Vec::new());
    let exclusions = vec![
        ExclusionDef { name: "debug".into(), is_enabled: true, filter: ExclusionQuery { query: "status:debug".into(), sample_rate: 1.0, sample_attribute: None } },
        ExclusionDef { name: "half of web".into(), is_enabled: true, filter: ExclusionQuery { query: "service:web".into(), sample_rate: 0.5, sample_attribute: None } },
    ];
    let four = compile(
        &[index("payments", 1, "service:payments", Vec::new()), index("audit", 2, "service:audit", Vec::new()), index("errors", 3, "status:error", Vec::new()), index("main", 0, "*", exclusions)],
        Vec::new(),
    );
    println!("{n} records routed, a record:");
    for (what, a) in [("main alone, as kiln makes it", &main_alone), ("four indexes, main's two exclusion filters", &four)] {
        for (form, make) in [("rows", rows as fn(usize) -> Vec<LogPart>), ("records", runs)] {
            let (t, c) = measure(
                || Logs::from(make(n)),
                |mut logs| {
                    let u = route_logs_by(a, &mut logs);
                    (u, logs)
                },
            );
            println!("  {what:<44} {form:<8} {:>8.2} / {:>8.2} ns  {:>6.3} allocations", t.0.as_nanos() as f64 / n as f64, t.1.as_nanos() as f64 / n as f64, c.calls() as f64 / n as f64);
        }
    }

    // A store's search: its records in one part, and in four.
    let total = 2_000_000;
    let fill = |parts: bool| {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = kiln_obs::batch::Builder::new();
        for chunk in (0..total).collect::<Vec<_>>().chunks(1_000) {
            let mut by: Vec<(u16, Vec<u8>, u32, i64)> = (0..4).map(|p| (p, Vec::new(), 0, 0)).collect();
            for &i in chunk {
                let r = record(i);
                let part = if parts { (i % 4) as u16 } else { 0 };
                let (_, raw, count, prev) = &mut by[part as usize];
                put_row(raw, &r, *prev);
                *prev = r.ts;
                *count += 1;
            }
            for (part, raw, count, _) in by {
                b.log_rows_in(part, &raw, count);
            }
            s.apply(&b.finish()).unwrap();
        }
        s
    };
    println!("a store of {total} records searched:");
    for (what, parts) in [("one part", false), ("four parts", true)] {
        let s = fill(parts);
        let at = s.logs_every();
        for (q, label) in [("*", "the newest 100"), ("service:payments", "a service's newest 100")] {
            let search = kiln_obs::logs::search::parse(q).unwrap();
            let (t, c) = measure(|| (), |()| at.search(&search, i64::MIN, i64::MAX, 100).unwrap());
            println!("  {what:<12} {label:<26} {:>9.3} / {:>9.3} ms  {:>8} allocations", t.0.as_secs_f64() * 1e3, t.1.as_secs_f64() * 1e3, c.calls());
        }
        let all = kiln_obs::logs::search::parse("*").unwrap();
        let (t, c) = measure(|| (), |()| at.aggregate(&all, i64::MIN, i64::MAX, &[Facet::Status], &Measure::Count).unwrap());
        println!("  {what:<12} {:<26} {:>9.3} / {:>9.3} ms  {:>8} allocations", "a count by status of all", t.0.as_secs_f64() * 1e3, t.1.as_secs_f64() * 1e3, c.calls());
    }
}
