//! The log's appends per second and their latency on a device: mantle's `mantle bench log`
//! (mantle `crates/mantle/src/bench_log.rs` at `147f035`) on hyper-log, the same workload and
//! the same columns, so the two run side by side (docs/benchmarks.md, "Throughput"):
//! `cargo bench -p hyper-log --bench log -- DIR [SECONDS] [SIZES] [REPLICAS] [MODES]`, sizes and
//! replicas comma-separated (128,1024,16384 and 1,4,16,64,256 by default, one second a point),
//! modes `plain`, `sealed` or both (`plain,sealed`, the default): each point runs in each mode,
//! one after the other, so a sealed log's cost is read beside the plain one's under the same load
//! (hyper-raft docs/seal.md §11), with the energy the process was charged an append where the OS
//! estimates it.
//!
//! A log is created in a scratch file on the device and driven by closed-loop replicas: each
//! appends one entry to its own group, waits until the log has made it durable, and appends the
//! next, as a replica does with each `Ready`. A replica is a record, not a thread: driver
//! threads, at most one a granted core, each hold their share of the replicas, submit for each
//! whose answer came, and hear of each answer through the replica's waker. Every replica compacts
//! behind itself, keeping 64 entries, so the log frees and reclaims segments as a running node's
//! does. Each point ends with a restart: the log reopened, timed, and every replica's kept
//! entries read back from the file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::cognitive_complexity,
    clippy::unwrap_in_result
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_block::scratch::Scratch;
use hyper_block::threads;
use hyper_log::{
    Class, Config, Entries, Entry, Log, LogError, Pending, Sealing, Start, Update, Waits,
};
use hyper_seal::Secret32;
use hyper_seal::keys::{KeyId, WrappingKey};

/// Entries a replica keeps behind its last before it compacts, as mantle's bench does.
const KEEP: u64 = 64;

fn config(most: usize) -> Config {
    Config {
        segment_bytes: 16 << 20,
        max_segments: 64,
        max_groups: most,
        group_entries: 1 << 20,
        group_bytes: 1 << 30,
        group_cache: 1 << 16,
        queue_submissions: most * 2,
        waits: Waits::Measured,
    }
}

fn open(path: &Path) -> DeviceFile {
    DeviceFile::open(
        path,
        false,
        CachingRequest::PreferDirect,
        Alignment::new(4096).unwrap(),
    )
    .unwrap()
}

/// One replica: its group, the index it appended last, and the append it has out.
struct Replica {
    group: u128,
    last: u64,
    started: Instant,
    pending: Option<Pending>,
}

/// What one driver measured: each append's latency in nanoseconds, and its CPU time.
struct Driven {
    latencies: Vec<u64>,
    cpu: Duration,
}

fn update(replica: &Replica, size: usize) -> Update {
    let next = replica.last + 1;
    let mut update = Update {
        entries: Some(Entries {
            first: next,
            entries: vec![Entry {
                term: 1,
                bytes: vec![0x5a; size],
            }],
        }),
        ..Update::default()
    };
    if next > KEEP && next.is_multiple_of(KEEP) {
        update.start = Some(Start {
            index: next - KEEP,
            term: 1,
        });
    }
    update
}

/// One driver: the replicas `driver`, `driver + drivers`, ... of `count`.
#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn drive(
    log: &Log<DeviceFile>,
    driver: usize,
    drivers: usize,
    count: usize,
    size: usize,
    deadline: Instant,
    peak: &AtomicUsize,
) -> Result<Driven, LogError> {
    let cpu = threads::thread_cpu().unwrap();
    let mut replicas: Vec<Replica> = (driver..count)
        .step_by(drivers)
        .map(|r| Replica {
            group: r as u128,
            last: 0,
            started: Instant::now(),
            pending: None,
        })
        .collect();
    let (ready, woken) = sync_channel(replicas.len());
    let wakers: Vec<_> = (0..replicas.len())
        .map(|i| hyper_measure::wake::waker(i, ready.clone()).0)
        .collect();
    drop(ready);
    let submit = |replica: &mut Replica, waker: &std::task::Waker| -> Result<(), LogError> {
        let u = update(replica, size);
        replica.started = Instant::now();
        replica.pending =
            Some(log.submit_waking(replica.group, Class::Normal, u, waker.clone())?);
        Ok(())
    };
    let mut latencies = Vec::new();
    let mut out = 0usize;
    for (replica, waker) in replicas.iter_mut().zip(&wakers) {
        submit(replica, waker)?;
        out += 1;
    }
    peak.fetch_max(threads::count().unwrap(), Ordering::Relaxed);
    while out > 0 {
        let Ok(i) = woken.recv() else { break };
        let Some(pending) = replicas[i].pending.take() else {
            continue;
        };
        let Some(answer) = pending.poll() else {
            replicas[i].pending = Some(pending);
            continue;
        };
        out -= 1;
        answer?;
        latencies.push(replicas[i].started.elapsed().as_nanos() as u64);
        replicas[i].last += 1;
        if Instant::now() < deadline {
            submit(&mut replicas[i], &wakers[i])?;
            out += 1;
        }
    }
    Ok(Driven {
        latencies,
        cpu: threads::thread_cpu().unwrap().saturating_sub(cpu),
    })
}

/// Reads back from the file every entry the replicas' groups keep; returns how many.
fn read_back(log: &Log<DeviceFile>, count: usize) -> u64 {
    let mut read = 0u64;
    for replica in 0..count {
        let Some(view) = log.view(replica as u128).unwrap() else {
            continue;
        };
        let (first, high) = (view.start.index + 1, view.last + 1);
        if first < high {
            read += log
                .entries(replica as u128, first, high, u64::MAX)
                .unwrap()
                .len() as u64;
        }
    }
    read
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let at = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[at.min(sorted.len() - 1)]
}

fn nanos(ns: u64) -> String {
    match ns {
        0..1_000 => format!("{ns} ns"),
        1_000..1_000_000 => format!("{:.1} µs", ns as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.2} ms", ns as f64 / 1e6),
        _ => format!("{:.2} s", ns as f64 / 1e9),
    }
}

/// A sealed log's keys: the same each time, so the reopen opens what the run wrote.
fn keys() -> Sealing {
    Sealing {
        parent: WrappingKey::new(KeyId([1; 16]), 0, Secret32::from_bytes(&[1; 32]).unwrap()),
        auth: Secret32::from_bytes(&[2; 32]).unwrap(),
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn point(
    dir: &Path,
    step: Duration,
    size: usize,
    count: usize,
    most: usize,
    id: u128,
    sealed: bool,
) {
    let scratch = Scratch::create(dir, ".hyper-bench-log").unwrap();
    let before = hyper_measure::usage::this().ok();
    let device_before = device_flushes(scratch.path());
    let log = if sealed {
        Log::create_sealed(open(scratch.path()), config(most), id, keys()).unwrap()
    } else {
        Log::create(open(scratch.path()), config(most), id).unwrap()
    };
    let drivers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(count)
        .max(1);
    let peak = AtomicUsize::new(0);
    let started = Instant::now();
    let deadline = started + step;
    let results: Vec<Result<Driven, LogError>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..drivers)
            .map(|d| {
                let (log, peak) = (&log, &peak);
                s.spawn(move || drive(log, d, drivers, count, size, deadline, peak))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let elapsed = started.elapsed();
    let energy = before
        .and_then(|b| hyper_measure::usage::this().ok().map(|a| a.since(&b)))
        .and_then(|u| u.energy_nj);
    let mut latencies = Vec::new();
    let mut cpu = Duration::ZERO;
    for r in results {
        let d = r.unwrap();
        latencies.extend(d.latencies);
        cpu += d.cpu;
    }
    latencies.sort_unstable();
    let appends = latencies.len() as f64;
    let (frames, updates) = log.flushed();
    // Device flushes an append, confirmations among them: an entry nothing follows (one replica,
    // a node at rest) is confirmed by a flush of its own (docs/benchmarks.md, "hyper-log: the
    // flushes an append costs").
    let stats = log.stats(None).unwrap();
    let flushes_per_append = stats.flushes as f64 / appends.max(1.0);
    let durable_per_append = stats.durable_writes as f64 / appends.max(1.0);
    let fallback_per_append = stats.durable_fallbacks as f64 / appends.max(1.0);
    // The flushes the device itself completed, the log's and anything else on it, an append.
    let device = match (device_before, device_flushes(scratch.path())) {
        (Some((before, _)), Some((after, fua))) => format!(
            "{:.2}{}",
            after.saturating_sub(before) as f64 / appends.max(1.0),
            if fua { " fua" } else { "" }
        ),
        _ => "-".to_string(),
    };
    let per_flush = if frames == 0 {
        0.0
    } else {
        updates as f64 / frames as f64
    };
    let file = log.close().unwrap();
    drop(file);
    let reopened = Instant::now();
    let (log, _) = if sealed {
        Log::open_sealed(open(scratch.path()), config(most), id, keys()).unwrap()
    } else {
        Log::open(open(scratch.path()), config(most), id).unwrap()
    };
    let reopen = reopened.elapsed();
    let reading = Instant::now();
    let read = read_back(&log, count);
    let read_time = reading.elapsed();
    drop(log);
    let rate = appends / elapsed.as_secs_f64();
    let per_append = energy.map_or_else(
        || "-".to_string(),
        |nj| format!("{:.2} µJ", nj as f64 / 1e3 / appends.max(1.0)),
    );
    println!(
        "  {:<19} {:>9} {:>8} {:>9} {:>11.0} {:>9.1} MiB/s {:>10} {:>10} {:>10} {:>11.1} {:>11.2} {:>11.2} {:>11.2} {:>12} {:>12} {:>10} {:>8} in {}",
        format!("{} {size}", if sealed { "sealed" } else { "plain" }),
        count,
        peak.load(Ordering::Relaxed),
        nanos(cpu.as_nanos() as u64),
        rate,
        rate * size as f64 / (1 << 20) as f64,
        nanos(percentile(&latencies, 0.50)),
        nanos(percentile(&latencies, 0.99)),
        nanos(percentile(&latencies, 0.999)),
        per_flush,
        flushes_per_append,
        durable_per_append,
        fallback_per_append,
        device,
        per_append,
        nanos(reopen.as_nanos() as u64),
        read,
        nanos(read_time.as_nanos() as u64),
    );
}

/// The block device a file lives on, as Linux names it under `/sys/dev/block`: the whole disk's
/// directory where the file is on a partition, whose counters are the disk's.
#[cfg(target_os = "linux")]
fn device_dir(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(path).ok()?.dev();
    // glibc's encoding of a dev_t's major and minor (makedev(3)).
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    let dir = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()?;
    if dir.join("partition").exists() {
        dir.parent().map(Path::to_path_buf)
    } else {
        Some(dir)
    }
}

/// What the device the file lives on did: the flush requests it completed, from its counters
/// (`/sys/block/<dev>/stat`, field 16, Linux 5.5 on), and whether its queue advertises FUA.
#[cfg(target_os = "linux")]
fn device_flushes(path: &Path) -> Option<(u64, bool)> {
    let dir = device_dir(path)?;
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    let flushes = stat.split_whitespace().nth(15)?.parse().ok()?;
    let fua = std::fs::read_to_string(dir.join("queue/fua"))
        .ok()
        .is_some_and(|v| v.trim() == "1");
    Some((flushes, fua))
}

#[cfg(not(target_os = "linux"))]
fn device_flushes(_path: &Path) -> Option<(u64, bool)> {
    None
}

fn list(arg: Option<&String>, default: &[usize]) -> Vec<usize> {
    arg.map_or_else(
        || default.to_vec(),
        |s| s.split(',').map(|n| n.parse().unwrap()).collect(),
    )
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let dir = args
        .first()
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let seconds: f64 = args.get(1).map_or(1.0, |s| s.parse().unwrap());
    let sizes = list(args.get(2), &[128, 1 << 10, 16 << 10]);
    let replicas = list(args.get(3), &[1, 4, 16, 64, 256]);
    let modes: Vec<bool> = args.get(4).map_or_else(
        || vec![false, true],
        |s| s.split(',').map(|m| m == "sealed").collect(),
    );
    let most = replicas.iter().copied().max().unwrap_or(1);
    hyper_seal::lock_keys(64).unwrap();
    println!("{}", dir.display());
    println!("hyper-log in a scratch file (removed afterwards), one entry an append");
    println!(
        "  {:<19} {:>9} {:>8} {:>9} {:>11} {:>15} {:>10} {:>10} {:>10} {:>11} {:>11} {:>11} {:>11} {:>12} {:>12} {:>10} {:>20}",
        "",
        "replicas",
        "threads",
        "cpu",
        "appends/s",
        "throughput",
        "p50",
        "p99",
        "p99.9",
        "per flush",
        "flush/app",
        "durable/app",
        "fallbk/app",
        "dev flush/app",
        "energy",
        "reopen",
        "read back"
    );
    let step = Duration::from_secs_f64(seconds);
    let mut id = 0u128;
    for &size in &sizes {
        for &count in &replicas {
            for &sealed in &modes {
                id += 1;
                point(&dir, step, size, count, most, id, sealed);
            }
        }
    }
}
