//! Error Tracking (`docs/observe.md` §8.8, §12.24): an error's issue, as it is taken, of each
//! language's stacks as its runtime wrote them (`src/fixtures/errors/`): its message's words, its
//! stack's frame, its id; time and allocations an error. A span's error is read so once, where it
//! is decoded; a log's where it is offered.
//!
//! `cargo bench -p kiln-obs --bench errors`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::time::Instant;

use hyper_measure::alloc;
use kiln_obs::errors::{Error, span_issue};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const FIXTURES: [(&str, &str); 8] = [
    ("python", include_str!("../src/fixtures/errors/python.txt")),
    ("node", include_str!("../src/fixtures/errors/node.txt")),
    ("go", include_str!("../src/fixtures/errors/go.txt")),
    ("java", include_str!("../src/fixtures/errors/java.txt")),
    ("ruby", include_str!("../src/fixtures/errors/ruby.txt")),
    ("php", include_str!("../src/fixtures/errors/php.txt")),
    ("dotnet", include_str!("../src/fixtures/errors/dotnet.txt")),
    ("rust", include_str!("../src/fixtures/errors/rust.txt")),
];

fn cases(text: &str) -> Vec<&str> {
    text.split("---- ").filter(|c| !c.trim().is_empty()).map(|c| c.split_once('\n').map_or("", |(_, s)| s)).collect()
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    println!("{:<8} {:>6} {:>10} {:>12} {:>14}", "stacks", "cases", "bytes", "ns an error", "allocs an error");
    let message = "order 550e8400-e29b-41d4-a716-446655440000 for cart 'c-294' failed after 3 retries (timeout 30s) [shard eu-1]";
    for (lang, text) in FIXTURES {
        let stacks = cases(text);
        let errors: Vec<Error<'_>> = stacks.iter().map(|s| Error { kind: Some("ValueError"), message: Some(message), stack: Some(s), fingerprint: None }).collect();
        let bytes: usize = stacks.iter().map(|s| s.len()).sum::<usize>() / stacks.len();
        let rounds = 2_000;
        let ns = median(
            (0..21)
                .map(|_| {
                    let t = Instant::now();
                    for _ in 0..rounds {
                        for e in &errors {
                            std::hint::black_box(span_issue("shop", true, e));
                        }
                    }
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        let n = (rounds * errors.len()) as f64;
        alloc::begin();
        for e in &errors {
            std::hint::black_box(span_issue("shop", true, e));
        }
        let c = alloc::end();
        println!("{lang:<8} {:>6} {:>10} {:>12.0} {:>14.1}", errors.len(), bytes, ns as f64 / n, c.calls() as f64 / errors.len() as f64);
    }
    seen_bench();
}

/// The metric's read (`kiln_obs::errors::seen`): Error Tracking's bound of issues (`KOBS_ISSUES`,
/// 10,000), each erring in three series (two versions of an env, one of another), in one window of
/// ten seconds in twenty over a day, written a block each two hours but the last two, in the head
/// as the server holds them. Read over the registry's twenty minutes (each ten seconds) and over an
/// hour and a day (monitors' windows), every issue's series and some issues' alone (`seen_of`, a
/// monitor of a few): the time (the median of eleven), the series with errors, and the allocations
/// of a read.
fn seen_bench() {
    use kiln_obs::batch::{Builder, Kind, SeriesKey};
    const WINDOWS: i64 = 8_640;
    const CHUNK: i64 = 720;
    let issues: u64 = std::env::var("KOBS_ISSUES").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let start = 1_800_000_000_000 - 1_800_000_000_000 % 86_400_000;
    let dir = std::env::temp_dir().join(format!("kiln-seen-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut st = kiln_obs::store::Store::open(&dir, kiln_obs::hash::Keyed::with_keys(1, 2)).unwrap();
    let mut keys = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for i in 0..issues {
        let id = format!("{:016x}", i.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let service = format!("svc-{}", i % 100);
        for (env, version) in [("prod", "1.4.2"), ("prod", "1.4.3"), ("staging", "1.5.0")] {
            keys.push(SeriesKey::new(kiln_obs::errors::METRIC, [("env", env), ("issue.id", id.as_str()), ("service", service.as_str()), ("version", version)], Kind::Count).unwrap());
        }
        ids.push(id);
    }
    let started = Instant::now();
    let mut rng = kiln_obs::testkit::Rng::new(7);
    let mut points = 0usize;
    let chunks = WINDOWS / CHUNK;
    for chunk in 0..chunks {
        let mut b = Builder::new();
        for k in &keys {
            b.define(k);
        }
        for k in &keys {
            let mut samples = Vec::new();
            for w in chunk * CHUNK..(chunk + 1) * CHUNK {
                if rng.below(20) == 0 {
                    samples.push((start + w * 10_000, 1.0 + rng.below(3) as f64));
                }
            }
            points += samples.len();
            b.run(k.fp, samples);
        }
        st.apply(&b.finish()).unwrap();
        if chunk + 1 < chunks {
            st.cut(kiln_obs::manifest::Point { index: chunk as u64 + 1, term: 1 }).unwrap();
            st.compact().unwrap();
        }
    }
    println!("\n{} series of a day of {} ({:.1} M points) written in {:.1} s", keys.len(), kiln_obs::errors::METRIC, points as f64 / 1e6, started.elapsed().as_secs_f64());
    let end = start + WINDOWS * 10_000;
    for (name, range) in [("twenty minutes", 1_200_000i64), ("an hour", 3_600_000), ("a day", 86_400_000)] {
        let (from, to) = (end - range, end);
        let all: Vec<&str> = ids.iter().map(String::as_str).collect();
        for k in [0usize, 1, 10, 100, 1_000, all.len()] {
            let some = all.get(..k).unwrap_or(&all);
            let read = |some: &[&str]| if k == 0 { kiln_obs::errors::seen(&st, &[], from, to) } else { kiln_obs::errors::seen_of(&st, some, from, to) };
            let mut found = 0;
            let ns = median(
                (0..11)
                    .map(|_| {
                        let t = Instant::now();
                        found = read(some).unwrap().len();
                        t.elapsed().as_nanos() as u64
                    })
                    .collect(),
            );
            alloc::begin();
            let s = read(some).unwrap();
            let c = alloc::end();
            drop(s);
            let what = if k == 0 { "every issue's".to_owned() } else { format!("{k} issues'") };
            println!("{name:<15} {what:<16} {:>9.3} ms, {found:>6} series with errors, {:.2} allocations a series", ns as f64 / 1e6, c.calls() as f64 / found.max(1) as f64);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
