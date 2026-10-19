//! Zstandard decompressed (`docs/observe.md` §8.6, §12.22): the fixtures the reference CLI wrote
//! (text over two blocks at --fast=5, 3 and 19; binary at 3 and 19), unzipped into a buffer kept
//! from one to the next. Its speed (MB/s of output), allocations and page faults.
//!
//! `cargo bench -p kiln-obs --bench zstd`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::time::Instant;

use hyper_measure::{alloc, faults};
use kiln_obs::zstd::decompress;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fixtures/zstd");
    println!("{:<20} {:>10} {:>10} {:>10} {:>10} {:>8}", "frame", "bytes", "content", "MB/s", "allocs", "faults");
    for file in ["logs.fast5.zst", "logs.3_C.zst", "logs.19.zst", "mixed.3.zst", "mixed.19_C.zst"] {
        let frame = std::fs::read(dir.join(file)).unwrap();
        let mut out = Vec::new();
        decompress(&frame, &mut out, 1 << 26).unwrap();
        let ns = median(
            (0..101)
                .map(|_| {
                    let t = Instant::now();
                    decompress(&frame, &mut out, 1 << 26).unwrap();
                    std::hint::black_box(&out);
                    t.elapsed().as_nanos() as u64
                })
                .collect(),
        );
        let f0 = faults::read().unwrap();
        alloc::begin();
        decompress(&frame, &mut out, 1 << 26).unwrap();
        let c = alloc::end();
        let f1 = faults::read().unwrap();
        println!("{:<20} {:>10} {:>10} {:>10.0} {:>10} {:>8}", file, frame.len(), out.len(), out.len() as f64 / (ns as f64 / 1e9) / 1e6, c.calls(), f1.since(&f0).total());
    }
}
