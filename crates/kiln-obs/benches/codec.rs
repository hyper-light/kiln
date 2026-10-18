//! The Gorilla codec on recorded data (`docs/observe.md` §12.1, §12.2).
//!
//! `KOBS_CORPUS=<corpus> cargo bench -p kiln-obs --bench codec`: bytes a sample; encode and decode
//! time a sample; allocations, reallocations, bytes and page faults a chunk; then the sweep of
//! chunk lengths and the delta-of-delta bucket widths the corpus makes cheapest.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

#[path = "common/corpus.rs"]
mod corpus;

use std::time::Instant;

use hyper_measure::alloc;
use hyper_measure::faults;
use kiln_obs::gorilla::{Chunk, DOD_BITS, Encoder};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn encode_all(series: &[corpus::Series], chunk_len: usize) -> Vec<Vec<Chunk>> {
    series
        .iter()
        .map(|s| {
            let mut chunks = Vec::with_capacity(s.ts.len().div_ceil(chunk_len));
            for (ts, vs) in s.ts.chunks(chunk_len).zip(s.values.chunks(chunk_len)) {
                let mut e = Encoder::with_capacity(chunk_len * 2);
                for (&t, &v) in ts.iter().zip(vs) {
                    e.push(t, v).unwrap();
                }
                chunks.push(e.finish());
            }
            chunks
        })
        .collect()
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
    let chunk_len = std::env::var("KOBS_CHUNK").ok().and_then(|v| v.parse().ok()).unwrap_or(120usize);
    println!("corpus: {} series, {samples} samples; chunks of {chunk_len}; DOD_BITS {DOD_BITS:?}", series.len());

    // Encode: time, allocations, faults.
    let f0 = faults::read().ok();
    let t0 = Instant::now();
    alloc::begin();
    let chunks = encode_all(&series, chunk_len);
    let enc_counts = alloc::end();
    let enc = t0.elapsed();
    let f1 = faults::read().ok();
    let n_chunks: usize = chunks.iter().map(Vec::len).sum();
    let bytes: usize = chunks.iter().flatten().map(|c| c.bytes.len()).sum();

    // Decode into reused columns: what a query does.
    let mut ts = Vec::with_capacity(chunk_len);
    let mut vs = Vec::with_capacity(chunk_len);
    let mut check = 0u64;
    let t1 = Instant::now();
    alloc::begin();
    for c in chunks.iter().flatten() {
        ts.clear();
        vs.clear();
        c.as_ref().decode_into(&mut ts, &mut vs).unwrap();
        check = check.wrapping_add(ts.len() as u64);
    }
    let dec_counts = alloc::end();
    let dec = t1.elapsed();
    let f2 = faults::read().ok();
    assert_eq!(check as usize, samples);

    println!("bytes a sample:       {:.3} (raw 16; Gorilla's paper 1.37 on Facebook's data)", bytes as f64 / samples as f64);
    println!("encode ns a sample:   {:.2}", enc.as_nanos() as f64 / samples as f64);
    println!("decode ns a sample:   {:.2}", dec.as_nanos() as f64 / samples as f64);
    println!(
        "encode a chunk:       {:.2} allocations, {:.2} reallocations ({:.2} moved), {:.0} bytes",
        enc_counts.allocations as f64 / n_chunks as f64,
        enc_counts.reallocations as f64 / n_chunks as f64,
        enc_counts.moved as f64 / n_chunks as f64,
        enc_counts.bytes as f64 / n_chunks as f64
    );
    println!("decode a chunk:       {:.3} allocations, {:.3} reallocations", dec_counts.allocations as f64 / n_chunks as f64, dec_counts.reallocations as f64 / n_chunks as f64);
    if let (Some(f0), Some(f1), Some(f2)) = (f0, f1, f2) {
        println!("page faults:          encode {} ({:.4} a chunk), decode {}", f1.since(&f0).total(), f1.since(&f0).total() as f64 / n_chunks as f64, f2.since(&f1).total());
    }

    if std::env::args().any(|a| a == "sweep") {
        sweep(&series, samples);
    }
}

/// §12.2: bytes a sample against chunk length, and the timestamp buckets the corpus makes cheapest.
fn sweep(series: &[corpus::Series], samples: usize) {
    println!("\nchunk length sweep: bytes a sample, and samples decoded to reach one at random (half a chunk)");
    for len in [15usize, 30, 60, 90, 120, 180, 240, 360, 480, 960, 1920] {
        let chunks = encode_all(series, len);
        let bytes: usize = chunks.iter().flatten().map(|c| c.bytes.len()).sum();
        println!("  {len:>5}: {:.3} bytes a sample, {:>4} decoded to reach a sample", bytes as f64 / samples as f64, len / 2);
    }
    // The distribution of the signed width each delta-of-delta needs, chunks of 120.
    let mut hist = [0u64; 65];
    let mut zeros = 0u64;
    let mut timed = 0u64;
    for s in series {
        for ts in s.ts.chunks(120) {
            let mut prev_delta: u64 = 0;
            for w in ts.windows(2) {
                let (a, b) = (w[0], w[1]);
                let delta = (b as u64).wrapping_sub(a as u64);
                let dod = delta.wrapping_sub(prev_delta) as i64;
                prev_delta = delta;
                timed += 1;
                if dod == 0 {
                    zeros += 1;
                    continue;
                }
                let width = 65 - if dod < 0 { (!dod).leading_zeros() } else { dod.leading_zeros() };
                hist[width.min(64) as usize] += 1;
            }
        }
    }
    let cost = |b: [u32; 3]| -> f64 {
        let mut bits = zeros as f64;
        for (w, &n) in hist.iter().enumerate() {
            let w = w as u32;
            let c = if w <= b[0] { 2 + b[0] } else if w <= b[1] { 3 + b[1] } else if w <= b[2] { 4 + b[2] } else { 4 + 64 };
            bits += (n as f64) * f64::from(c);
        }
        bits / timed as f64
    };
    println!("\ndelta-of-delta: {:.1}% zero; widths (bits: share):", 100.0 * zeros as f64 / timed as f64);
    for (w, &n) in hist.iter().enumerate().filter(|(_, n)| **n > 0) {
        println!("  {w:>2}: {:.3}%", 100.0 * n as f64 / timed as f64);
    }
    let mut best = ([0u32; 3], f64::MAX);
    for a in 1..=40u32 {
        for b in a + 1..=50 {
            for c in b + 1..=63 {
                let v = cost([a, b, c]);
                if v < best.1 {
                    best = ([a, b, c], v);
                }
            }
        }
    }
    println!("timestamp bits a sample: Prometheus's [14, 17, 20] {:.3}; Gorilla's [7, 9, 12] {:.3}; cheapest {:?} {:.3}", cost([14, 17, 20]), cost([7, 9, 12]), best.0, best.1);
    // The outer bucket kept at Prometheus's 20 bits (±524 s): a missed 60 s export or a restart
    // stays out of the 68-bit fallback on workloads this corpus lacks.
    let mut kept = ([0u32; 3], f64::MAX);
    for a in 1..=18u32 {
        for b in a + 1..=19 {
            let v = cost([a, b, 20]);
            if v < kept.1 {
                kept = ([a, b, 20], v);
            }
        }
    }
    println!("cheapest with the outer bucket at 20: {:?} {:.3}", kept.0, kept.1);
}
