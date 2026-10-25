//! What sealing costs (docs/seal.md §11), on whatever load the host carries, which is printed beside
//! each run: `cargo bench -p hyper-seal --bench seal -- [SAMPLES]` (10,000 by default).
//!
//! - a key made and wrapped, and unwrapped (slates: single-digit µs);
//! - a 4 KiB segment opened warm (its key at hand) and cold (the file's key unwrapped from its
//!   header and checked against its commitment first; slates: ≤ ~1 µs p99 warm);
//! - sealing and opening throughput a core at segments of 4, 16, 64 and 256 KiB;
//! - a 4 KiB overwrite inside a 64 KiB and a 256 KiB chunk: a key per file (the chunk resealed under
//!   a new key) against the version-keyed rule (one segment resealed under the lineage key).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::cognitive_complexity
)]

use std::time::Instant;

use hyper_seal::Secret32;
use hyper_seal::keys::WrappingKey;
use hyper_seal::stream::{FileOpener, FileSealer, VersionKey};

fn percentile(sorted: &[u64], p: f64) -> u64 {
    let at = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[at.min(sorted.len() - 1)]
}

fn nanos(ns: u64) -> String {
    match ns {
        0..1_000 => format!("{ns} ns"),
        1_000..1_000_000 => format!("{:.2} µs", ns as f64 / 1e3),
        _ => format!("{:.2} ms", ns as f64 / 1e6),
    }
}

/// Times `op` `samples` times and prints its p50, p99 and p99.9.
fn latency(name: &str, samples: usize, mut op: impl FnMut()) {
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        op();
        times.push(start.elapsed().as_nanos() as u64);
    }
    times.sort_unstable();
    println!(
        "  {:<44} {:>10} {:>10} {:>10}",
        name,
        nanos(percentile(&times, 0.50)),
        nanos(percentile(&times, 0.99)),
        nanos(percentile(&times, 0.999)),
    );
}

/// Seals and opens `total` bytes in segments of `segment` and prints GB/s each way.
fn throughput(parent: &WrappingKey, segment: u32, total: usize) {
    let seg = segment as usize;
    let count = total / seg;
    let (mut sealer, header) = FileSealer::new(parent, segment).unwrap();
    let mut buf = vec![0x5au8; seg];
    let mut tags = Vec::with_capacity(count);
    let start = Instant::now();
    for i in 0..count {
        tags.push(sealer.seal(&mut buf, i + 1 == count).unwrap());
    }
    let sealing = start.elapsed();
    let opener = FileOpener::new(parent, &header).unwrap();
    // Opening needs each segment's own ciphertext: seal them again into a store first.
    let (mut sealer, header) = FileSealer::new(parent, segment).unwrap();
    let mut store: Vec<(Vec<u8>, [u8; 16])> = (0..count.min(256))
        .map(|i| {
            let mut s = vec![0x5au8; seg];
            let tag = sealer.seal(&mut s, i + 1 == count.min(256)).unwrap();
            (s, tag)
        })
        .collect();
    let opener2 = FileOpener::new(parent, &header).unwrap();
    let rounds = count / store.len();
    let start = Instant::now();
    for _ in 0..rounds {
        for (i, (s, tag)) in store.iter_mut().enumerate() {
            let mut copy = s.clone();
            opener2
                .open(i as u64, i + 1 == count.min(256), &mut copy, tag)
                .unwrap();
        }
    }
    let opening = start.elapsed();
    drop(opener);
    let gb = (count * seg) as f64 / 1e9;
    println!(
        "  {:<44} {:>9.2} GB/s seal {:>7.2} GB/s open",
        format!("segments of {} KiB", seg / 1024),
        gb / sealing.as_secs_f64(),
        (rounds * store.len() * seg) as f64 / 1e9 / opening.as_secs_f64(),
    );
}

fn main() {
    let samples: usize = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map_or(10_000, |s| s.parse().unwrap());
    hyper_seal::lock_keys(1024).unwrap();
    println!(
        "hyper-seal, {samples} samples a row, load {:?}",
        hyper_measure::usage::load()
    );
    println!("  {:<44} {:>10} {:>10} {:>10}", "", "p50", "p99", "p99.9");
    let parent = WrappingKey::generate(0).unwrap();

    latency("a key made and wrapped", samples, || {
        let _ = parent.make_child().unwrap();
    });
    let (_, wrapped) = parent.make_child().unwrap();
    latency("a key unwrapped", samples, || {
        let _ = parent.unwrap(&wrapped).unwrap();
    });

    let (mut sealer, header) = FileSealer::new(&parent, 4096).unwrap();
    let mut seg = vec![0x5au8; 4096];
    let tag = sealer.seal(&mut seg, true).unwrap();
    let opener = FileOpener::new(&parent, &header).unwrap();
    latency("a 4 KiB segment opened, warm", samples, || {
        let mut copy = seg.clone();
        opener.open(0, true, &mut copy, &tag).unwrap();
    });
    latency(
        "a 4 KiB segment opened, cold (unwrap + commit)",
        samples,
        || {
            let opener = FileOpener::new(&parent, &header).unwrap();
            let mut copy = seg.clone();
            opener.open(0, true, &mut copy, &tag).unwrap();
        },
    );

    for chunk in [64 * 1024usize, 256 * 1024] {
        let kib = chunk / 1024;
        latency(
            &format!("4 KiB overwrite, {kib} KiB chunk, key a file"),
            samples / 10,
            || {
                let (mut sealer, _) = FileSealer::new(&parent, 4096).unwrap();
                let mut buf = vec![0x5au8; 4096];
                for i in 0..chunk / 4096 {
                    let _ = sealer.seal(&mut buf, i + 1 == chunk / 4096).unwrap();
                }
            },
        );
        let lineage = Secret32::from_bytes(&[9; 32]).unwrap();
        let key = VersionKey::new(&lineage, [1; 16]).unwrap();
        let mut version = 0u64;
        latency(
            &format!("4 KiB overwrite, {kib} KiB chunk, versioned"),
            samples,
            || {
                version += 1;
                let mut buf = vec![0x5au8; 4096];
                let _ = key.seal(version, 3, false, &mut buf).unwrap();
            },
        );
    }

    println!("throughput a core:");
    for segment in [4096u32, 16 << 10, 64 << 10, 256 << 10] {
        throughput(&parent, segment, 256 << 20);
    }
}
