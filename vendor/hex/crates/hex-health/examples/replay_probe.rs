//! The disk probe's cost rule replayed on a recorded trace (`docs/design/failure-isolation.md` F3),
//! beside `bench/health/replay_probe.py`'s exact form: the probe seeded with a record's answers and a
//! re-placement time, every probe of the trace answered in turn. Prints the false drains (a healthy
//! answer slower than the drain age it met) and the drain age a stall at the end would meet.
//!
//! Usage: replay_probe <trace> <record trace> <re-placement ms>... (each `<ms> <latency ns>` lines)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use hex_health::Level;
use hex_health::probe::DiskProbe;

fn latencies(path: &str) -> Vec<u64> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(|ns| ns.parse().unwrap())
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().unwrap())
        .collect();
    let trace = latencies(&args[0]);
    let record: Vec<(u64, u32)> = latencies(&args[1]).into_iter().map(|ns| (ns, 1)).collect();
    for ms in &args[2..] {
        let replace: u64 = ms.parse::<u64>().unwrap() * 1_000_000;
        let mut p = DiskProbe::new(&record, replace).unwrap();
        let (mut t, mut false_drains) = (0u64, 0u64);
        for lat in &trace {
            p.start(t).unwrap();
            if p.level(t + lat).unwrap() == Level::Drain {
                false_drains += 1;
            }
            t += lat;
            p.finish(t).unwrap();
            t += 1_000_000;
        }
        println!(
            "cost rule in hex-health (re-placement {ms} ms/workload, record {} answers): false drains {false_drains:4}  a stall at the end drained at {:8.1} ms",
            record.len(),
            p.drain_age_ns().unwrap() as f64 / 1e6
        );
    }
}
