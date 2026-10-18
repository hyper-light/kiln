//! LZ4 against deflate on kiln's logs (`docs/observe.md` §12.6): ratio and throughput by block
//! size, to choose the log block's text size.
//!
//! `KOBS_LOGS=<file of log lines>[:<file>...] cargo bench -p kiln-obs --bench lz4`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::io::{Read, Write};
use std::time::Instant;

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use hyper_measure::alloc;
use kiln_obs::lz4;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn main() {
    let Ok(paths) = std::env::var("KOBS_LOGS") else {
        eprintln!("set KOBS_LOGS to files of log lines (docs/observe.md §12.6)");
        return;
    };
    let mut data = Vec::new();
    for p in paths.split(':') {
        std::fs::File::open(p).unwrap().read_to_end(&mut data).unwrap();
    }
    println!("logs: {:.1} MB", data.len() as f64 / 1e6);
    println!("{:>9} {:>10} {:>8} {:>12} {:>12}", "block", "codec", "ratio", "compress", "decompress");
    for block in [64usize << 10, 256 << 10, 1 << 20, 4 << 20] {
        let blocks: Vec<&[u8]> = data.chunks(block).collect();
        // LZ4.
        let t = Instant::now();
        let mut comp: Vec<Vec<u8>> = Vec::with_capacity(blocks.len());
        for b in &blocks {
            let mut out = Vec::with_capacity(b.len() / 2);
            lz4::compress(b, &mut out);
            comp.push(out);
        }
        let ct = t.elapsed();
        let csize: usize = comp.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(block);
        let t = Instant::now();
        alloc::begin();
        for (c, b) in comp.iter().zip(&blocks) {
            out.clear();
            lz4::decompress(c, &mut out, b.len()).unwrap();
            assert_eq!(out.len(), b.len());
        }
        let calls = alloc::end().calls();
        let dt = t.elapsed();
        println!("{:>8}K {:>10} {:>8.2} {:>9.0}MB/s {:>9.0}MB/s  ({} allocations decoding)", block >> 10, "lz4", data.len() as f64 / csize as f64, data.len() as f64 / ct.as_secs_f64() / 1e6, data.len() as f64 / dt.as_secs_f64() / 1e6, calls);
        for level in [1u32, 6] {
            let t = Instant::now();
            let comp: Vec<Vec<u8>> = blocks
                .iter()
                .map(|b| {
                    let mut e = DeflateEncoder::new(Vec::with_capacity(b.len() / 2), Compression::new(level));
                    e.write_all(b).unwrap();
                    e.finish().unwrap()
                })
                .collect();
            let ct = t.elapsed();
            let csize: usize = comp.iter().map(Vec::len).sum();
            let t = Instant::now();
            for c in &comp {
                out.clear();
                DeflateDecoder::new(c.as_slice()).read_to_end(&mut out).unwrap();
            }
            let dt = t.elapsed();
            println!("{:>8}K {:>10} {:>8.2} {:>9.0}MB/s {:>9.0}MB/s", block >> 10, format!("deflate-{level}"), data.len() as f64 / csize as f64, data.len() as f64 / ct.as_secs_f64() / 1e6, data.len() as f64 / dt.as_secs_f64() / 1e6);
        }
    }
}
