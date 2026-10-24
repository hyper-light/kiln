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
//! What an idle group costs its owner, on ticks and by suspicion (timing
//! step L-2, `docs/timing.md` §2.9). Many groups of three voters, all in this
//! process, each elected and holding one committed entry, every member caught
//! up; then the owner's periods pass with nothing proposed.
//!
//! - **ticks**: each period the owner ticks every member of every group, as
//!   raft-rs's owners do, takes what each member gives, and delivers every
//!   message within the group until it is quiet: the leader's heartbeats
//!   every `heartbeat_tick` periods and their answers.
//! - **suspicion**: each period the owner asks every member of every group
//!   when it is next to be woken (`RawNode::deadline`), the most an owner
//!   without a timer queue would do, and wakes none, for none is due.
//!   An owner that keeps its deadlines in a timer queue does nothing at all.
//!
//! `idle <ticks|suspicion> <groups> <periods>` runs one, for a process
//! measurement (`/usr/bin/time -l`); with no arguments both run at 10,000
//! groups and 200 periods. It prints the time a period took, a group, and
//! the messages a period sent.
#[path = "../tests/support/mod.rs"]
mod support;

use std::time::Instant;

use hyper_raft::proto::{ConfState, Message};
use support::{New, Replica, Settings, Store};

struct Group {
    members: [New; 3],
}

fn voters() -> ConfState {
    ConfState {
        voters: vec![1, 2, 3],
        ..ConfState::default()
    }
}

impl Group {
    fn open(settings: &Settings, seed: u64) -> Self {
        let open = |id: u64| New::open(id, Store::new(voters()), settings, seed * 3 + id, 3);
        Self {
            members: [open(1), open(2), open(3)],
        }
    }

    /// Takes what each member gives and delivers every message until the
    /// group is quiet; the messages delivered.
    fn settle(&mut self, now: u64, wake: bool) -> u64 {
        let mut net: Vec<Message> = Vec::new();
        let mut delivered = 0;
        for member in &mut self.members {
            if wake {
                member.wake(now);
            }
            net.extend(member.drain().messages);
        }
        while let Some(message) = net.pop() {
            delivered += 1;
            let to = &mut self.members[(message.to - 1) as usize];
            to.step(message);
            if wake {
                to.wake(now);
            }
            net.extend(to.drain().messages);
        }
        delivered
    }

    /// Member 1 campaigns (by suspicion another may draw a delay of nothing
    /// and win first), and the leader commits an entry on every member.
    fn elect(&mut self, suspicion: bool) {
        self.members[0].campaign();
        self.settle(0, suspicion);
        let leader = self
            .members
            .iter()
            .position(|m| m.view().role == 2)
            .expect("a leader");
        self.members[leader].propose(b"x".to_vec());
        self.settle(0, suspicion);
        if !suspicion {
            // A heartbeat round carries the last commit to every member.
            for _ in 0..4 {
                for member in &mut self.members {
                    member.tick();
                }
                self.settle(0, false);
            }
        }
        for member in &self.members {
            assert_eq!(member.app().index, 2, "every member applied the entry");
        }
        if suspicion {
            for member in &self.members {
                assert_eq!(member.deadline(), None, "an idle member is due for nothing");
            }
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn run(suspicion: bool, groups: u64, periods: u64) {
    let settings = if suspicion {
        Settings::focal().by_suspicion()
    } else {
        Settings::focal()
    };
    let mut all: Vec<Group> = (0..groups)
        .map(|seed| {
            let mut group = Group::open(&settings, seed);
            group.elect(suspicion);
            group
        })
        .collect();
    let started = Instant::now();
    let mut messages = 0u64;
    let mut due = 0u64;
    for _ in 0..periods {
        for group in &mut all {
            if suspicion {
                for member in &group.members {
                    due += u64::from(member.deadline().is_some());
                }
            } else {
                for member in &mut group.members {
                    member.tick();
                }
                messages += group.settle(0, false);
            }
        }
    }
    let took = started.elapsed();
    let per = took.as_nanos() as f64 / (groups * periods.max(1)) as f64;
    println!(
        "{}: {groups} groups, {periods} periods: {:.1} ns a group a period, {:.3} messages a group a period, {due} members due",
        if suspicion { "suspicion" } else { "ticks" },
        per,
        messages as f64 / (groups * periods.max(1)) as f64,
    );
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    match args.as_slice() {
        [mode, groups, periods] => run(
            mode == "suspicion",
            groups.parse().unwrap(),
            periods.parse().unwrap(),
        ),
        _ => {
            for suspicion in [false, true] {
                run(suspicion, 10_000, 200);
            }
        }
    }
}
