// Dependency-free bench: a plain `harness = false` binary, no criterion (the
// workspace's deny.toml forbids unmaintained/unvetted deps). A measurement
// tool, not a pass/fail test.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::disallowed_macros,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cognitive_complexity,
    unreachable_pub
)]
//! What repairing a marked member costs (`docs/durable.md` §5, §14.5): by its
//! lost entries (core step R-5) against by a snapshot (mantle's repair before
//! it), at several sizes of what was lost.
//!
//! Three voters hold `ENTRIES` entries of `ENTRY_BYTES` each, Protocol-Aware
//! Recovery's workload (Alagappan et al., FAST 2018, §5.2: one corrupted
//! entry of 30,000 fixed in 1.2 ms and 7 KB by CTRL against 1.24 s and 32 MB
//! by LogCabin's snapshot; 32 MB over 30,000 entries is about a kibibyte
//! each). The third member then loses its last `lag` entries at rest and
//! reopens marked. Repaired by entries, the leader sends them again; by a
//! snapshot, the leader has compacted its log into an image of the state
//! (every entry's bytes, as the state machine would hold them) and sends that.
//!
//! Every message of the repair is written in the wire format and read back
//! (`hyper_raft::wire`), as a transport carries it, and counted in bytes. The
//! time is the repair's on this machine's one thread, from the member's
//! reopening until its log holds the leader's again: the core's work, the
//! copies, the encoding and decoding. No device and no network: the bytes
//! are what they would carry.
#[path = "../tests/support/mod.rs"]
mod support;

use std::time::Instant;

use hyper_raft::{proto::Message, wire::Record};
use support::{Cluster, Fault, New, Op, Replica, Settings};

/// Protocol-Aware Recovery's log (§5.2).
const ENTRIES: usize = 30_000;
/// 32 MB over its 30,000 entries, to the kibibyte.
const ENTRY_BYTES: usize = 1024;
/// Runs a cell, of which the median is stated.
const RUNS: usize = 5;

/// A group whose three voters hold `ENTRIES` entries, led by 1.
fn loaded() -> Cluster<New> {
    let mut group: Cluster<New> = Cluster::new(3, &[1, 2, 3], Settings::shell(), 1);
    group.act(&Op::Campaign(1));
    carry(&mut group, &mut 0);
    let mut proposed = 0;
    while proposed < ENTRIES {
        // A batch at a time, within what the leader holds uncommitted.
        for _ in 0..1_000.min(ENTRIES - proposed) {
            let mut data = vec![0u8; ENTRY_BYTES];
            data[..8].copy_from_slice(&(proposed as u64).to_le_bytes());
            group.act(&Op::Propose(1, data));
            proposed += 1;
        }
        carry(&mut group, &mut 0);
    }
    group
}

/// Delivers everything, each message written out and read back; adds the
/// bytes written to `bytes`.
fn carry(group: &mut Cluster<New>, bytes: &mut usize) {
    while !group.net.is_empty() {
        let written = group.net[0].encode_to_vec();
        *bytes += written.len();
        group.net[0] = Message::decode(&written).expect("a message reads back");
        group.act(&Op::Deliver {
            at: 0,
            keep: false,
            lose: false,
        });
    }
}

/// Repairs member 3 after it lost its last `lag` entries; by a snapshot when
/// the leader compacted its log first. The time and the bytes.
#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn repair(lag: u64, snapshot: bool) -> (f64, usize) {
    let mut group = loaded();
    if snapshot {
        let disk = &group.disk(1).clone();
        let last = disk.last_index();
        let image: Vec<u8> = disk.entries.iter().flat_map(|e| e.data.clone()).collect();
        group.node(1).unwrap().store_mut().0.compact(last, image);
    }
    // It reopens marked: the repair runs from here.
    group.act(&Op::Corrupt(3, Fault::Lose(lag)));
    let started = Instant::now();
    let mut bytes = 0;
    for _ in 0..64 {
        if group.node(3).unwrap().raw.raft.lost().is_none()
            && group.disk(3).last_index() == group.disk(1).last_index()
        {
            let elapsed = started.elapsed().as_nanos() as f64;
            return (elapsed, bytes);
        }
        group.act(&Op::Tick(1));
        carry(&mut group, &mut bytes);
    }
    panic!("member 3 was not repaired");
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() {
    println!(
        "{:>8} {:>16} {:>16} {:>18} {:>18}",
        "lost", "entries µs", "entries bytes", "snapshot µs", "snapshot bytes"
    );
    for lag in [1u64, 10, 100, 1_000, 10_000] {
        let mut cells = Vec::new();
        for snapshot in [false, true] {
            let mut times = Vec::new();
            let mut bytes = 0;
            for _ in 0..RUNS {
                let (time, sent) = repair(lag, snapshot);
                times.push(time / 1_000.0);
                bytes = sent;
            }
            cells.push((median(times), bytes));
        }
        println!(
            "{:>8} {:>16.1} {:>16} {:>18.1} {:>18}",
            lag, cells[0].0, cells[0].1, cells[1].0, cells[1].1
        );
    }
}
