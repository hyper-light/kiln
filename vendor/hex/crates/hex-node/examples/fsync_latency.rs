//! The data a learned disk-probe timeout rests on (`docs/design/failure-isolation-constants.md`,
//! `probe::MIN_TIMEOUT_NS`): a healthy disk's probe latencies, measured as the prober takes them, a
//! one-byte rewrite and its `fdatasync` every period, for a run's length. Prints the count and the
//! quantiles, and every latency above the 99.9th percentile with its time, so the tail's shape (lone
//! spikes or runs of them) shows.
//!
//! Usage: fsync_latency --dir DIR [--period-ms 10] [--run-s 600] [--dump FILE]
//!
//! `--dump` writes every probe as a line `<ms since start> <latency ns>`: a trace a test replays.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn pause(d: Duration) {
    let (_keep, rx) = std::sync::mpsc::channel::<()>();
    let _ = rx.recv_timeout(d);
}

fn main() {
    let mut args: BTreeMap<String, String> = BTreeMap::new();
    let mut it = std::env::args_os().skip(1).map(|a| {
        a.into_string()
            .unwrap_or_else(|a| panic!("an argument isn't Unicode: {a:?}"))
    });
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        args.insert(k.trim_start_matches("--").to_string(), v);
    }
    let get = |k: &str, d: &str| args.get(k).cloned().unwrap_or_else(|| d.to_string());
    let dir = PathBuf::from(get("dir", "."));
    let period = Duration::from_millis(get("period-ms", "10").parse().unwrap());
    let run = Duration::from_secs(get("run-s", "600").parse().unwrap());
    let path = dir.join(".hex-fsync-latency");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .unwrap();
    let start = Instant::now();
    let mut samples: Vec<(u64, u64)> = Vec::new();
    while start.elapsed() < run {
        let began = Instant::now();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[0u8]).unwrap();
        file.sync_data().unwrap();
        let ns = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let at_ms = u64::try_from(began.duration_since(start).as_millis()).unwrap_or(u64::MAX);
        samples.push((at_ms, ns));
        pause(period.saturating_sub(began.elapsed()));
    }
    std::fs::remove_file(&path).ok();
    if let Some(dump) = args.get("dump") {
        let mut out = String::new();
        for (at_ms, ns) in &samples {
            out.push_str(&format!("{at_ms} {ns}\n"));
        }
        std::fs::write(dump, out).unwrap();
    }
    let mut sorted: Vec<u64> = samples.iter().map(|s| s.1).collect();
    sorted.sort_unstable();
    // The quantile at `per_million` of the sorted latencies.
    let q = |per_million: usize| sorted[(sorted.len() - 1) * per_million / 1_000_000];
    // Microseconds, rounded down: whole microseconds are what the tail's shape needs.
    let us = |ns: u64| ns / 1_000;
    println!(
        "fsync count={} p50_us={} p90_us={} p99_us={} p999_us={} p9999_us={} max_us={}",
        sorted.len(),
        us(q(500_000)),
        us(q(900_000)),
        us(q(990_000)),
        us(q(999_000)),
        us(q(999_900)),
        us(*sorted.last().unwrap()),
    );
    let tail = q(999_000);
    for (at_ms, ns) in samples.iter().filter(|s| s.1 > tail) {
        println!("tail at_ms={at_ms} us={}", us(*ns));
    }
}
