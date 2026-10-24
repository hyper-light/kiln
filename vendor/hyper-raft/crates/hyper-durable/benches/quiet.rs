//! How long a volatile state machine's applied state runs ahead of its logged commit under the
//! one-period rule, and what the commit's own write costs a quiet group (`docs/durable.md` §4.1,
//! §14 item 3).
//!
//! ```text
//! cargo bench -p hyper-durable --bench quiet -- DIR [BURSTS] [SIZES] [QUIETS] [DEVICES]
//! ```
//!
//! Three voters on hyper-log at the log's depth, driven as `support` states; every member's state
//! machine is volatile (its durable index stays zero), so its durable commit `C_d` is what its own
//! writes stated. A commit no write has stated while the applied index ran past `C_d` for a whole
//! quiet period is written then (the one-period rule). The quiet periods (`QUIETS`, milliseconds,
//! `10,100` by default) are mantle's (10 ms, as `hyper-durable-compare` states mantle's settings)
//! and focal's (its owner's configured tick, 100 ms, focal `crates/focal-node/src/control_host.rs`;
//! focal's shell writes the commit one period on).
//!
//! The leader takes a burst of `SIZES` proposals at once (`1,8,64` by default), and the group runs
//! until every member has applied the burst, its `C_d` covers what it applied, and no member has a
//! deadline or a write out: quiet. Then the next burst, `BURSTS` of them (50 by default). For each
//! member each burst, the span its applied index stood past its `C_d`, from the first turn it did
//! to the first it did not; and for each burst, from the leader's last answer to quiet, the time,
//! the writes of the commit alone, the frames and flushes over every member's log, and the Raft
//! messages sent.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper_measure::quantile::Level;
use support::{Counts, Device, Group, MEMBERS, Shape, Wakes, now};

/// The log's depth: a group's writes out at once, as the shell runs them.
const DEPTH: usize = 3;
/// Turns a burst may take to go quiet: a counted bound on a run that never would.
const TURNS: u64 = 10_000_000;

/// One burst's account.
#[derive(Default)]
struct Burst {
    /// Each member's span past its durable commit, nanoseconds, the leader's first.
    ahead: [u64; MEMBERS as usize],
    /// From the leader's last answer to quiet.
    tail: u64,
    quiet_writes: u64,
    frames: u64,
    flushes: u64,
    messages: u64,
}

/// Each member's span past its durable commit: opened at the first turn its applied index stands
/// past it, closed (the longest kept) at the first turn it does not.
fn track(
    members: &[support::MemberState],
    since: &mut [Option<u64>; MEMBERS as usize],
    ahead: &mut [u64; MEMBERS as usize],
    t: u64,
) {
    for (k, m) in members.iter().enumerate() {
        match (m.applied > m.durable_commit, since[k]) {
            (true, None) => since[k] = Some(t),
            (false, Some(from)) => {
                ahead[k] = ahead[k].max(t.saturating_sub(from));
                since[k] = None;
            }
            _ => {}
        }
    }
}

fn burst(group: &mut Group<'_>, size: usize) -> Burst {
    let mut account = Burst::default();
    let mut ahead_since: [Option<u64>; MEMBERS as usize] = [None; MEMBERS as usize];
    let target = group.members()[0].applied + size as u64;
    for _ in 0..size {
        assert!(group.propose(), "the leader takes a burst's proposals");
    }
    let mut answered = 0usize;
    let mut last_answer = None;
    let mut at_last: Option<(Counts, u64, u64)> = None;
    for _ in 0..TURNS {
        let answers = group.turn();
        let t = now();
        answered += answers;
        if answers > 0 && answered >= size && last_answer.is_none() {
            last_answer = Some(t);
            let members = group.members();
            at_last = Some((
                group.counts(),
                members.iter().map(|m| m.quiet_writes).sum(),
                group.messages,
            ));
        }
        let members = group.members();
        track(&members, &mut ahead_since, &mut account.ahead, t);
        let caught_up = members
            .iter()
            .all(|m| m.applied >= target && m.durable_commit >= m.applied);
        if let (Some(last), Some((counts, quiet, messages))) = (last_answer, at_last)
            && caught_up
            && !group.busy()
            && !group.writes_out()
            && !group.has_deadline()
        {
            let end = group.counts();
            account.tail = t.saturating_sub(last);
            account.quiet_writes = members.iter().map(|m| m.quiet_writes).sum::<u64>() - quiet;
            account.frames = end.frames - counts.frames;
            account.flushes = end.flushes - counts.flushes;
            account.messages = group.messages - messages;
            return account;
        }
        if answers == 0 {
            // Nothing to drive, no write out and no deadline: no event is coming, so a member
            // still past its durable commit would stay there.
            assert!(
                caught_up || group.busy() || group.writes_out() || group.has_deadline(),
                "a burst of {size}: no member will be driven again, and {members:?} are not \
                 caught up"
            );
            group.wait(None);
        }
    }
    panic!("a burst of {size} never went quiet in {TURNS} turns");
}

fn point(
    wakes: &Wakes,
    dir: &Path,
    device: Device,
    quiet: Duration,
    size: usize,
    bursts: usize,
) -> Vec<Burst> {
    let shape = Shape {
        quiet,
        ..Shape::on(device, DEPTH)
    };
    let mut scratch = Vec::new();
    let seed = 0x0071_7569_6574 ^ size as u64;
    let accounts = {
        let mut group = Group::open(wakes, shape, |kind, id| {
            support::device(kind, id, dir, seed, &mut scratch)
        });
        (0..bursts).map(|_| burst(&mut group, size)).collect()
    };
    drop(scratch);
    accounts
}

fn ms(values: &mut [u64]) -> String {
    values.sort_unstable();
    let show = |ns: u64| format!("{:.2}", ns as f64 / 1e6);
    format!(
        "{} / {}",
        support::cell_with(values, Level::P50, show),
        values
            .last()
            .map_or_else(|| "none".to_owned(), |&v| show(v))
    )
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    let dir = PathBuf::from(args.first().expect("a directory for the logs' files"));
    let bursts: usize = args.get(1).map_or(50, |n| n.parse().unwrap());
    let sizes: Vec<usize> = support::list(args.get(2), &[1, 8, 64]);
    let quiets: Vec<u64> = support::list(args.get(3), &[10, 100]);
    let devices: Vec<Device> = args.get(4).map_or(vec![Device::File, Device::Sim], |d| {
        d.split(',')
            .map(|d| match d.trim() {
                "file" => Device::File,
                "sim" => Device::Sim,
                other => panic!("device {other}: file or sim"),
            })
            .collect()
    });
    let wakes = Wakes::new();
    for device in devices {
        for &q in &quiets {
            let before = support::load();
            println!(
                "\n**{device:?}, quiet period {q} ms, {bursts} bursts a size; load {before}**\n"
            );
            println!(
                "| burst | leader ahead ms, p50 [95%] / max | followers ahead ms, p50 [95%] / max | to quiet ms, p50 [95%] / max | quiet writes/burst | frames/burst | flushes/burst | messages/burst |\n|---|---|---|---|---|---|---|---|"
            );
            for &size in &sizes {
                let accounts = point(&wakes, &dir, device, Duration::from_millis(q), size, bursts);
                let n = accounts.len() as f64;
                let mut leader: Vec<u64> = accounts.iter().map(|a| a.ahead[0]).collect();
                let mut followers: Vec<u64> = accounts
                    .iter()
                    .flat_map(|a| a.ahead[1..].iter().copied())
                    .collect();
                let mut tail: Vec<u64> = accounts.iter().map(|a| a.tail).collect();
                let mean = |f: fn(&Burst) -> u64| accounts.iter().map(f).sum::<u64>() as f64 / n;
                println!(
                    "| {size} | {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.1} |",
                    ms(&mut leader),
                    ms(&mut followers),
                    ms(&mut tail),
                    mean(|a| a.quiet_writes),
                    mean(|a| a.frames),
                    mean(|a| a.flushes),
                    mean(|a| a.messages)
                );
            }
            println!("\nload after: {}", support::load());
        }
    }
}
