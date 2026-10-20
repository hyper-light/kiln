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
}
