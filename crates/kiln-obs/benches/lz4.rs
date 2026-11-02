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
use kiln_obs::logs::bloom::{Bloom, tokens};
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
    if std::env::args().any(|a| a == "tokens") {
        token_stats(&data);
        return;
    }
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

/// Distinct tokens a 256 KiB block, and what a Bloom filter of them costs against the block's LZ4
/// text, at several false-positive rates (`docs/observe.md` §12.7).
fn token_stats(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let block = 256 << 10;
    let mut distinct: Vec<usize> = Vec::new();
    let mut comp_sizes: Vec<usize> = Vec::new();
    let mut buf = String::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    while start < bytes.len() {
        let mut end = (start + block).min(bytes.len());
        while end < bytes.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        let chunk = &text[start..end];
        let mut set = std::collections::HashSet::new();
        tokens(chunk, &mut buf, |t| {
            set.insert(t.to_owned());
        });
        distinct.push(set.len());
        let mut c = Vec::new();
        lz4::compress(chunk.as_bytes(), &mut c);
        comp_sizes.push(c.len());
        start = end;
    }
    distinct.sort_unstable();
    let mean = distinct.iter().sum::<usize>() as f64 / distinct.len() as f64;
    let comp_mean = comp_sizes.iter().sum::<usize>() as f64 / comp_sizes.len() as f64;
    println!("{} blocks of 256 KiB: distinct tokens mean {:.0}, p50 {}, p99 {}, max {}; LZ4 text mean {:.0} bytes", distinct.len(), mean, distinct[distinct.len() / 2], distinct[distinct.len() * 99 / 100], distinct[distinct.len() - 1], comp_mean);
    for p in [0.001, 0.005, 0.01, 0.02, 0.05, 0.1] {
        let b = Bloom::with_rate(mean as usize, p);
        // A false positive decompresses the block's text (at 3.8 GB/s, §12.6) for nothing.
        let wasted_us = p * (block as f64) / 3.8e9 * 1e6;
        println!("  p {:>5}: {:>6} bytes a block ({:>5.1}% of its LZ4 text), k {:>2}; a missing word costs {:.2} µs a block in wasted decompression", p, b.byte_len(), 100.0 * b.byte_len() as f64 / comp_mean, b.k, wasted_us);
    }
}
