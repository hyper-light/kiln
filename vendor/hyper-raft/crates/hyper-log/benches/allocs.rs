//! What one append and one fetch cost the allocator and the page tables: `cargo bench -p
//! hyper-log --bench allocs [-- DIR]` (docs/benchmarks.md, "Allocations").
//!
//! A log on a real file in DIR (the target directory's scratch by default) takes closed-loop
//! appends from `R` replicas, each to its own group: one round submits one entry for every
//! replica and waits for all of them, so every frame carries the round. The updates are built
//! before the count begins, so the count is the log's own: the submission, the owner's and the
//! device's work, the answer, and the drop of what the log held. The count is the process's,
//! every thread's together (`hyper_measure::alloc::begin_process`), since the log runs threads of
//! its own. Fetches are counted the same way: a group's entries still in memory, the same
//! entries read back from the file after a reopen, a term and a view. The same appends and reads
//! are counted again through each group's handle (`GroupLog`), which keeps its writes' entries
//! and answers its reads itself, and through handles whose replica's mark keeps what it has yet to
//! apply (`GroupLog::keep_after`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};

use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_block::scratch::Scratch;
use hyper_log::{Config, Entries, Entry, GroupLog, Log, Update, Waits};
use hyper_measure::{alloc, faults};

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// Rounds counted per row, after the warm-up rounds.
const ROUNDS: u64 = 200;
const WARM: u64 = 20;

fn config(replicas: usize) -> Config {
    Config {
        segment_bytes: 16 << 20,
        max_segments: 64,
        max_groups: replicas,
        group_entries: 1 << 20,
        group_bytes: 1 << 30,
        group_cache: 1 << 16,
        queue_submissions: replicas * 2,
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

fn updates(replicas: usize, size: usize, from: u64, rounds: u64) -> Vec<Vec<Update>> {
    (from..from + rounds)
        .map(|index| {
            (0..replicas)
                .map(|_| Update {
                    entries: Some(Entries {
                        first: index,
                        entries: vec![Entry {
                            term: 1,
                            bytes: vec![0x5a; size],
                        }],
                    }),
                    ..Update::default()
                })
                .collect()
        })
        .collect()
}

fn rounds(log: &Log<DeviceFile>, batches: Vec<Vec<Update>>) {
    for batch in batches {
        let pending: Vec<_> = batch
            .into_iter()
            .enumerate()
            .map(|(g, u)| log.submit(g as u128, u).unwrap())
            .collect();
        for p in pending {
            p.wait().unwrap();
        }
    }
}

fn handle_rounds(handles: &mut [GroupLog<DeviceFile>], batches: Vec<Vec<Update>>) {
    for batch in batches {
        for (h, u) in handles.iter_mut().zip(batch) {
            h.submit(u).unwrap();
        }
        for h in handles.iter_mut() {
            h.wait().unwrap().unwrap();
        }
    }
}

/// The same appends and reads through each group's handle, on a log of their own.
fn through_handles(dir: &Path, replicas: usize, size: usize) {
    let scratch = Scratch::create(dir, ".hyper-log-allocs-handles").unwrap();
    let log = Log::create(open(scratch.path()), config(replicas), 2).unwrap();
    let mut handles: Vec<GroupLog<DeviceFile>> = (0..replicas)
        .map(|g| log.group(g as u128).unwrap())
        .collect();
    handle_rounds(&mut handles, updates(replicas, size, 1, WARM));
    let batches = updates(replicas, size, WARM + 1, ROUNDS);
    let appends = ROUNDS * replicas as u64;
    let cost = counted(appends, || handle_rounds(&mut handles, batches));
    row("append through the handle", replicas, size, &cost);
    let last = WARM + ROUNDS;
    let mut into = Some(hyper_log::Fetched::new());
    let mut fetch = |h: &GroupLog<DeviceFile>| {
        let got = h
            .fetch(last, last + 1, u64::MAX, into.take().unwrap())
            .unwrap();
        assert_eq!(got.len(), 1);
        into = Some(got);
    };
    fetch(&handles[0]);
    let cost = counted(replicas as u64, || {
        for h in &handles {
            fetch(h);
        }
    });
    row("fetch through the handle, reserved", replicas, size, &cost);
    let cost = counted(replicas as u64, || {
        for h in &handles {
            assert_eq!(h.term(last).unwrap(), 1);
            assert_eq!(h.bounds().unwrap().1, last);
        }
    });
    row("term and bounds through the handle", replicas, size, &cost);
    let asked: u64 = handles.iter().map(GroupLog::asked).sum();
    assert_eq!(asked, 0, "a handle asked the log for what it held");
}

/// The same appends through handles whose replica marks what it applied a round behind its
/// writes (`GroupLog::keep_after`), on a log whose group share is a byte: what the handle keeps
/// is the mark's alone. Appends are counted with the mark raised each round, then a fetch of the
/// entry the replica has yet to apply, which no handle asks the log for.
fn through_marked_handles(dir: &Path, replicas: usize, size: usize) {
    let scratch = Scratch::create(dir, ".hyper-log-allocs-marked").unwrap();
    let log = Log::create(
        open(scratch.path()),
        Config {
            group_cache: 1,
            ..config(replicas)
        },
        3,
    )
    .unwrap();
    let mut handles: Vec<GroupLog<DeviceFile>> = (0..replicas)
        .map(|g| log.group(g as u128).unwrap())
        .collect();
    for h in &mut handles {
        h.keep_after(0);
    }
    let marked_rounds = |handles: &mut [GroupLog<DeviceFile>], batches: Vec<Vec<Update>>| {
        for batch in batches {
            let mut last = 0;
            for (h, u) in handles.iter_mut().zip(batch) {
                last = u.entries.as_ref().map_or(last, |e| e.first);
                h.submit(u).unwrap();
            }
            for h in handles.iter_mut() {
                h.wait().unwrap().unwrap();
                // Applied through the round before this one: this round's entry is kept.
                h.keep_after(last.saturating_sub(1));
            }
        }
    };
    marked_rounds(&mut handles, updates(replicas, size, 1, WARM));
    let batches = updates(replicas, size, WARM + 1, ROUNDS);
    let appends = ROUNDS * replicas as u64;
    let cost = counted(appends, || marked_rounds(&mut handles, batches));
    row("append, marked, a byte's share", replicas, size, &cost);
    let last = WARM + ROUNDS;
    let mut into = Some(hyper_log::Fetched::new());
    let mut fetch = |h: &GroupLog<DeviceFile>| {
        let got = h
            .fetch(last, last + 1, u64::MAX, into.take().unwrap())
            .unwrap();
        assert_eq!(got.len(), 1);
        into = Some(got);
    };
    fetch(&handles[0]);
    let cost = counted(replicas as u64, || {
        for h in &handles {
            fetch(h);
        }
    });
    row("fetch of an unapplied entry, marked", replicas, size, &cost);
    let asked: u64 = handles.iter().map(GroupLog::asked).sum();
    assert_eq!(
        asked, 0,
        "a marked handle asked the log for an entry it keeps"
    );
    let kept: u64 = handles.iter().map(GroupLog::kept_bytes).sum();
    assert_eq!(
        kept,
        (replicas * size) as u64,
        "each keeps its unapplied entry alone"
    );
}

struct Cost {
    allocations: f64,
    reallocations: f64,
    bytes: f64,
    faults: f64,
    /// Allocations the calling thread made: the caller's share of `allocations`.
    caller: f64,
}

fn per(counts: alloc::Counts, caller: alloc::Counts, faults: u64, n: u64) -> Cost {
    let n = n as f64;
    Cost {
        allocations: counts.allocations as f64 / n,
        reallocations: counts.reallocations as f64 / n,
        bytes: counts.bytes as f64 / n,
        faults: faults as f64 / n,
        caller: caller.allocations as f64 / n,
    }
}

fn counted(n: u64, work: impl FnOnce()) -> Cost {
    let before = faults::read().unwrap();
    alloc::begin_process();
    alloc::begin();
    work();
    let caller = alloc::end();
    let counts = alloc::end_process();
    let after = faults::read().unwrap();
    per(counts, caller, after.since(&before).minor, n)
}

fn row(what: &str, replicas: usize, size: usize, cost: &Cost) {
    println!(
        "  {what:<40} {replicas:>8} {size:>7} {:>10.2} {:>10.2} {:>12.0} {:>10.3} {:>10.2}",
        cost.allocations, cost.reallocations, cost.bytes, cost.faults, cost.caller
    );
}

fn point(dir: &Path, replicas: usize, size: usize) {
    let scratch = Scratch::create(dir, ".hyper-log-allocs").unwrap();
    let log = Log::create(open(scratch.path()), config(replicas), 1).unwrap();
    rounds(&log, updates(replicas, size, 1, WARM));
    let batches = updates(replicas, size, WARM + 1, ROUNDS);
    let appends = ROUNDS * replicas as u64;
    let cost = counted(appends, || rounds(&log, batches));
    row("append", replicas, size, &cost);
    let last = WARM + ROUNDS;
    let cost = counted(replicas as u64, || {
        for g in 0..replicas {
            let got = log.entries(g as u128, last, last + 1, u64::MAX).unwrap();
            assert_eq!(got.len(), 1);
        }
    });
    row("fetch, in memory", replicas, size, &cost);
    let mut into = Some(hyper_log::Fetched::new());
    let mut fetch = |log: &Log<DeviceFile>, g: usize| {
        let got = log
            .fetch(g as u128, last, last + 1, u64::MAX, into.take().unwrap())
            .unwrap();
        assert_eq!(got.len(), 1);
        into = Some(got);
    };
    // The reservation grows to an entry once, outside the count.
    fetch(&log, 0);
    let cost = counted(replicas as u64, || {
        for g in 0..replicas {
            fetch(&log, g);
        }
    });
    row("fetch into a reservation", replicas, size, &cost);
    let cost = counted(replicas as u64, || {
        for g in 0..replicas {
            assert_eq!(log.term(g as u128, last).unwrap(), 1);
        }
    });
    row("term", replicas, size, &cost);
    let cost = counted(replicas as u64, || {
        for g in 0..replicas {
            assert!(log.view(g as u128).unwrap().is_some());
        }
    });
    row("view", replicas, size, &cost);
    drop(log);
    let (log, _) = Log::open(open(scratch.path()), config(replicas), 1).unwrap();
    let cost = counted(replicas as u64, || {
        for g in 0..replicas {
            let got = log.entries(g as u128, last, last + 1, u64::MAX).unwrap();
            assert_eq!(got.len(), 1);
        }
    });
    row("fetch, from the file", replicas, size, &cost);
    let mut into = Some(hyper_log::Fetched::new());
    let mut fetch = |log: &Log<DeviceFile>, g: usize| {
        let got = log
            .fetch(g as u128, last, last + 1, u64::MAX, into.take().unwrap())
            .unwrap();
        assert_eq!(got.len(), 1);
        into = Some(got);
    };
    fetch(&log, 0);
    let cost = counted(replicas as u64, || {
        for g in 0..replicas {
            fetch(&log, g);
        }
    });
    row(
        "fetch from the file, into a reservation",
        replicas,
        size,
        &cost,
    );
}

fn main() {
    assert!(alloc::installed(), "the counting allocator is installed");
    let dir = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    println!(
        "hyper-log: allocations, reallocations, bytes asked and minor faults per operation, \
         every thread's ({ROUNDS} rounds after {WARM})"
    );
    println!(
        "  {:<40} {:>8} {:>7} {:>10} {:>10} {:>12} {:>10} {:>10}",
        "", "replicas", "entry", "allocs", "reallocs", "bytes", "faults", "caller"
    );
    for replicas in [1usize, 16] {
        for size in [128usize, 1 << 10, 16 << 10] {
            point(&dir, replicas, size);
            through_handles(&dir, replicas, size);
            through_marked_handles(&dir, replicas, size);
        }
    }
}
