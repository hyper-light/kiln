//! kiln's goal 11 (plan 01a1a87b2e56): a shard's desk is made as it is used. Its task, timer and wait cells
//! (`src/slab.rs`), the loop's task records and the timer wheel's entries are made as their slots are first
//! taken, so a runtime's memory follows the most tasks it has held, not its bound, and the bound stays a
//! bound. Before, a shard of kiln's server (8,192 tasks, 16,384 timers and waits) wrote about 7 MB of cells
//! as it started, whatever it held. The resource is resident memory, so that is what is measured: the
//! process's own resident size through `ps` (a read of the kernel's accounting). One test per binary, so no
//! other test's allocations move the numbers.

// Test harness code: a panic here is a failed test (CLAUDE.md §1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::cognitive_complexity,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::string_slice,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn,
    clippy::missing_panics_doc
)]
// The whole file measures resident size through `ps` (Unix).
#![cfg(unix)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hyper_rt::runtime::{Runtime, RuntimeConfig};

/// Shape: a bound far past what the test holds, so cells made for the whole bound would be many megabytes.
const BOUND: usize = 262_144;
/// Shape: the tasks the burst holds at once, a sixteenth of the bound.
const BURST: u64 = 16_384;
/// Shape: how long the burst is given before the test calls it lost.
const WAIT: Duration = Duration::from_secs(60);

/// How many of the burst's tasks have ended.
static ENDED: AtomicU64 = AtomicU64::new(0);

fn config(tasks: usize) -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: tasks,
        timers_per_shard: tasks,
        interests_per_shard: 64,
        ring_entries: 64,
        step_budget_ns: 1_000_000,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// The process's resident size in KiB, as the kernel accounts it (`ps`, Unix).
fn resident_kib() -> u64 {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("ps prints the resident size in KiB")
}

/// `BURST` tasks spawned at once from one task on the shard, each sleeping a tick so all are held together,
/// then ending; waits for every one to end.
fn burst(rt: &Runtime) {
    let before = ENDED.load(Ordering::Relaxed);
    let shard = rt.shard_ids()[0];
    rt.spawn_on(shard, async {
        for _ in 0..BURST {
            hyper_rt::futures::spawn_detached(async {
                let _ = hyper_rt::futures::sleep(5_000_000).await;
                ENDED.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        }
    })
    .unwrap();
    let until = Instant::now() + WAIT;
    while ENDED.load(Ordering::Relaxed) < before + BURST {
        assert!(Instant::now() < until, "the burst ended within {WAIT:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Do: start a runtime of `BOUND` task slots (and as many timers), read the process's resident size, run a
/// burst of `BURST` tasks to their end, read it again, run the same burst again, and read it a third time.
/// Expect: the runtime starts in less than a quarter of what the cells of its whole bound would take, at what
/// the burst's cells took (a sixteenth of the bound: the burst's cells are made, the rest aren't; what still
/// goes with the bound as the shard starts is its rings and free stacks, a few bytes a slot); the second
/// burst, of slots given back and taken again, grows it by less than a quarter of the first.
#[test]
fn a_desk_is_made_as_its_slots_are_first_taken() {
    let before = resident_kib();
    let rt = Runtime::start(&config(BOUND)).unwrap();
    let started = resident_kib();
    burst(&rt);
    let first = resident_kib();
    burst(&rt);
    let second = resident_kib();
    rt.shutdown().unwrap();
    let (start, grew, again) = (started.saturating_sub(before), first.saturating_sub(started), second.saturating_sub(first));
    eprintln!(
        "resident KiB: before {before}, the runtime of {BOUND} slots started {started} (+{start}), after a burst of \
         {BURST} tasks {first} (+{grew}), after the same again {second} (+{again})"
    );
    assert!(grew > 0, "a burst of {BURST} tasks makes their cells (non-vacuity)");
    // Cells for the whole bound are BOUND / BURST times what the burst made: the runtime started in far less
    // than that, its cells not made.
    let whole = grew * (BOUND as u64 / BURST);
    assert!(start < whole / 4, "the runtime started in {start} KiB, against {whole} KiB for cells of its whole bound");
    assert!(again < grew / 4, "a second burst takes the slots the first gave back: +{again} KiB, against +{grew} KiB");
}
