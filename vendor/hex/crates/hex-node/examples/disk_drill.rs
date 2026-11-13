//! `AC-T7.10.3`'s drill (`bench/health/dm_delay_drill.sh` runs it in a root lab): work units write and
//! flush on a node's state disk, each write due within `--deadline-ms`; the disk is stalled
//! (`--stall-cmd`); with the probe on, the node is marked (`Suspect`), cordoned and drained at `Drain`,
//! its units moved to a healthy disk (`--spare`) before their writes are due. A unit fails when a write
//! it began isn't done, and its unit not moved, by the deadline. Prints one `key=value` record line.
//!
//! Usage: disk_drill --state DIR --spare DIR --probe on|off --stall-cmd CMD --resume-cmd CMD
//!        --replace-ms N (the measured time to re-place one unit elsewhere: what a false drain costs)
//!        [--record FILE: the state disk's recorded probe answers, `fsync_latency --dump`'s lines;
//!        default the host class's] [--units 8] [--deadline-ms 10000] [--period-ms 100] [--warm-s 10] [--after-s 30]

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
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hex_health::Level;
use hex_node::disk_probe::{DiskProber, recorded};

/// One unit of work: where it runs now, and when its write in flight began.
struct Unit {
    /// Bumped when the unit moves; a thread whose generation is old has been moved off and ends.
    generation: AtomicU64,
    in_flight: Mutex<Option<Instant>>,
    writes: AtomicU64,
}

fn spawn_worker(id: usize, unit: Arc<Unit>, dir: PathBuf, generation: u64) {
    std::thread::spawn(move || {
        let path = dir.join(format!("unit-{id}"));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        let block = [7u8; 4096];
        while unit.generation.load(Ordering::Acquire) == generation {
            *unit.in_flight.lock().unwrap() = Some(Instant::now());
            file.write_all(&block).unwrap();
            file.sync_data().unwrap();
            if unit.generation.load(Ordering::Acquire) != generation {
                return;
            }
            *unit.in_flight.lock().unwrap() = None;
            unit.writes.fetch_add(1, Ordering::Relaxed);
            pause(Duration::from_millis(50));
        }
    });
}

/// Wait `d`: on a channel nothing sends on, as `thread::sleep` is disallowed.
fn pause(d: Duration) {
    let (_keep, rx) = std::sync::mpsc::channel::<()>();
    let _ = rx.recv_timeout(d);
}

fn sh(cmd: &str) {
    let status = Command::new("sh").arg("-c").arg(cmd).status().unwrap();
    assert!(status.success(), "{cmd}: {status}");
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
    let state = PathBuf::from(get("state", ""));
    let spare = PathBuf::from(get("spare", ""));
    let probe_on = get("probe", "on") == "on";
    let units: usize = get("units", "8").parse().unwrap();
    let deadline = Duration::from_millis(get("deadline-ms", "10000").parse().unwrap());
    let period = Duration::from_millis(get("period-ms", "100").parse().unwrap());
    let warm = Duration::from_secs(get("warm-s", "10").parse().unwrap());
    let after = Duration::from_secs(get("after-s", "30").parse().unwrap());
    let (stall_cmd, resume_cmd) = (get("stall-cmd", ""), get("resume-cmd", ""));

    let prober = if probe_on {
        let replace = Duration::from_millis(args["replace-ms"].parse().unwrap());
        let record: Vec<(u64, u32)> = match args.get("record") {
            Some(file) => std::fs::read_to_string(file)
                .unwrap()
                .lines()
                .filter_map(|l| l.split_whitespace().nth(1))
                .map(|ns| (ns.parse().unwrap(), 1))
                .collect(),
            None => recorded().unwrap().to_vec(),
        };
        Some(DiskProber::start(&state, period, &record, replace).unwrap())
    } else {
        None
    };
    let all: Vec<Arc<Unit>> = (0..units)
        .map(|id| {
            let u = Arc::new(Unit {
                generation: AtomicU64::new(0),
                in_flight: Mutex::new(None),
                writes: AtomicU64::new(0),
            });
            spawn_worker(id, Arc::clone(&u), state.clone(), 0);
            u
        })
        .collect();

    let start = Instant::now();
    let mut stalled_at: Option<Instant> = None;
    let mut timeout_at_stall = Duration::ZERO;
    // A cordon the probe made before the stall: a false drain of a healthy disk.
    let mut cordoned_before_stall = false;
    let (mut marked, mut drained): (Option<Duration>, Option<Duration>) = (None, None);
    let mut failed = vec![false; units];
    let mut moved = 0usize;
    loop {
        let now = Instant::now();
        if stalled_at.is_none() && now - start >= warm {
            if let Some(p) = &prober {
                timeout_at_stall = p.drain_age().unwrap();
                cordoned_before_stall = !p.schedulable();
            }
            stalled_at = Some(Instant::now());
            sh(&stall_cmd);
        }
        if let Some(p) = &prober {
            let level = p.level().unwrap();
            if let Some(s) = stalled_at {
                if level >= Level::Suspect && marked.is_none() {
                    marked = Some(now - s);
                }
                if !p.schedulable() && drained.is_none() {
                    // The drain: every unit on the cordoned node moves to the healthy disk.
                    for (id, u) in all.iter().enumerate() {
                        let g = u.generation.fetch_add(1, Ordering::AcqRel) + 1;
                        *u.in_flight.lock().unwrap() = None;
                        spawn_worker(id, Arc::clone(u), spare.clone(), g);
                        moved += 1;
                    }
                    drained = Some(Instant::now() - s);
                }
            }
        }
        for (id, u) in all.iter().enumerate() {
            if let Some(began) = *u.in_flight.lock().unwrap()
                && now.duration_since(began) > deadline
            {
                failed[id] = true;
            }
        }
        if stalled_at.is_some_and(|s| now - s >= after) {
            break;
        }
        pause(Duration::from_millis(10));
    }
    sh(&resume_cmd);
    let ms = |d: Option<Duration>| d.map_or("none".to_string(), |d| d.as_millis().to_string());
    let writes: u64 = all.iter().map(|u| u.writes.load(Ordering::Relaxed)).sum();
    println!(
        "drill probe={} units={units} deadline_ms={} period_ms={} drain_age_at_stall_ms={} cordoned_before_stall={cordoned_before_stall} marked_ms={} drained_ms={} moved={moved} writes={writes} failed={}",
        if probe_on { "on" } else { "off" },
        deadline.as_millis(),
        period.as_millis(),
        timeout_at_stall.as_millis(),
        ms(marked),
        ms(drained),
        failed.iter().filter(|f| **f).count(),
    );
    drop(prober);
}
