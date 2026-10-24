//! What the memory budget costs a committed entry (`docs/durable.md` §14, item 7): an owner that
//! passes `Unbounded` must pay nothing for the reservations, which the replica's paths test against
//! `Budget::BOUNDED`, a constant.
//!
//! ```text
//! cargo bench -p hyper-durable --bench budget -- [ENTRIES] [ROUNDS] [CLIENTS]
//! ```
//!
//! A group of three voters on `RamStore` (each write durable as it is submitted, so no thread and no
//! device enters the count), one owner driving them on this thread, a message delivered in the turn
//! it was sent, `CLIENTS` proposals of 64 bytes out at the leader at once (`1,64` by default), each
//! proposed again when the leader applies it. Each round runs the group with `Unbounded` and with a
//! `Bytes` budget past anything the run holds, in turn, the order turned each round, on fresh
//! replicas; each run elects member 1, commits a thousand entries, then counts `ENTRIES` (100,000 by
//! default): the process's instructions, cycles and CPU an entry, where the OS counts them
//! (`hyper_measure::usage`). `ROUNDS` is 9 by default; the table gives each variant's median and its
//! least and most.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, sync_channel};
use std::task::Waker;
use std::time::Duration;

use hyper_durable::{
    Budget, Bytes, EntryRef, Fatal, Handle, Output, Owner, Point, RamStore, Replica, ReplicaError,
    Settings, StateMachine, Unbounded,
};
use hyper_measure::usage::{self, Usage};
use hyper_raft::Config;
use hyper_raft::proto::{ConfChangeV2, ConfState, Message};

/// The group's voters.
const MEMBERS: u64 = 3;
/// Each entry's bytes: the depth bench's and the core's pipeline bench's.
const ENTRY_BYTES: usize = 64;
/// Entries committed before a run counts.
const WARM: u64 = 1_000;

/// What the group applies: one answer an entry.
struct Counter {
    configuration: ConfState,
}

impl StateMachine for Counter {
    type Answer = ();

    fn apply(&mut self, entry: &EntryRef<'_>, answers: &mut Vec<()>) -> Result<(), Fatal> {
        if !entry.data.is_empty() {
            answers.push(());
        }
        Ok(())
    }
    fn apply_change(
        &mut self,
        _at: Point,
        _change: &ConfChangeV2,
        configuration: &ConfState,
    ) -> Result<(), Fatal> {
        self.configuration = configuration.clone();
        Ok(())
    }
    fn durable(&self) -> Point {
        Point::default()
    }
    fn configuration(&self) -> &ConfState {
        &self.configuration
    }
    fn acts_at_start(&self, _: &EntryRef<'_>) -> bool {
        false
    }
    fn image(&mut self, _: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        Err(Fatal("the run compacts nothing"))
    }
    fn image_bytes(&self) -> Option<u64> {
        None
    }
    fn install(&mut self, _: &[u8], _: Point, _: &ConfState) -> Result<(), Fatal> {
        Err(Fatal("the run compacts nothing"))
    }
    fn persist(&mut self) -> Result<(), Fatal> {
        Ok(())
    }
}

/// The depth bench's settings: mantle's range group's, elections by suspicion.
fn settings(id: u64) -> Settings {
    let max_size_per_msg: u64 = 1 << 16;
    let limits = hyper_raft::Limits::derive(hyper_raft::Stated {
        message: usize::try_from(max_size_per_msg).unwrap()
            + hyper_raft::wire::MESSAGE_RECORD_FIXED_BYTES,
        members: MEMBERS as usize,
        memory: 16 << 20,
        depth: 1,
    })
    .expect("the run's statement gives its bounds");
    Settings {
        core: Config {
            election_tick: 10,
            heartbeat_tick: 2,
            max_size_per_msg,
            max_inflight_msgs: 16,
            max_uncommitted_size: 1 << 20,
            max_committed_size_per_ready: 1 << 22,
            check_quorum: true,
            pre_vote: true,
            seed: id,
            ..Config::new(id, limits)
        },
        quiet: Duration::from_millis(10),
        elections: hyper_raft::Elections::Suspicion,
    }
}

/// One group on `RamStore`s with each replica's budget from `budget`.
struct Group<'w, B: Budget> {
    owner: Owner<RamStore, Counter, B>,
    handles: Vec<Handle>,
    woken: &'w Receiver<usize>,
    out: Output<()>,
    wire: VecDeque<Message>,
    clock: u64,
}

impl<'w, B: Budget> Group<'w, B> {
    fn open(wakers: &[Waker], woken: &'w Receiver<usize>, budget: impl Fn() -> B) -> Self {
        while woken.try_recv().is_ok() {}
        let mut owner = Owner::new(wakers.to_vec());
        let handles = (1..=MEMBERS)
            .map(|id| {
                let machine = Counter {
                    configuration: ConfState {
                        voters: (1..=MEMBERS).collect(),
                        ..ConfState::default()
                    },
                };
                let replica = Replica::open(&settings(id), RamStore::new(), machine, budget())
                    .expect("a member opens");
                owner.insert(replica).map_err(|_| ()).unwrap()
            })
            .collect();
        let mut group = Self {
            owner,
            handles,
            woken,
            out: Output::default(),
            wire: VecDeque::new(),
            clock: 0,
        };
        let first = group.handles[0];
        group.owner.get_mut(first).unwrap().campaign().unwrap();
        group.owner.schedule(first);
        // A counted budget of turns: an election on three members takes a handful.
        for _ in 0..100_000 {
            group.turn();
            if group.owner.get(first).unwrap().is_leader() && !group.busy() {
                return group;
            }
        }
        panic!("the group never elected member 1");
    }

    fn busy(&self) -> bool {
        self.owner.has_work() || !self.wire.is_empty()
    }

    /// One turn on the run's own clock, a microsecond a turn: the store answers at once, so no
    /// wait is ever real.
    fn turn(&mut self) -> usize {
        self.clock += 1_000;
        while let Ok(slot) = self.woken.try_recv() {
            self.owner.woken(slot);
        }
        let leader = self.handles[0];
        let wire = &mut self.wire;
        let mut answers = 0;
        self.owner
            .turn(self.clock, &mut self.out, |h, driven, out| {
                driven.expect("a member drives");
                wire.extend(out.messages.drain(..));
                if h == leader {
                    answers += out.answers.len();
                }
            });
        while let Some(m) = self.wire.pop_front() {
            let to = self.handles[usize::try_from(m.to).unwrap() - 1];
            match self.owner.get_mut(to).unwrap().step(m) {
                Ok(()) | Err(ReplicaError::Refused(_)) => {}
                Err(e) => panic!("step: {e}"),
            }
            self.owner.schedule(to);
        }
        answers
    }

    fn propose(&mut self) {
        let leader = self.handles[0];
        self.owner
            .get_mut(leader)
            .unwrap()
            .propose(Vec::new(), vec![0x5a; ENTRY_BYTES])
            .expect("the leader takes a proposal");
        self.owner.schedule(leader);
    }
}

/// Commits `WARM` then `entries` entries with `clients` out at once; the process's usage over the
/// counted ones.
fn run<B: Budget>(
    wakers: &[Waker],
    woken: &Receiver<usize>,
    budget: impl Fn() -> B,
    entries: u64,
    clients: u64,
) -> Usage {
    let mut group = Group::open(wakers, woken, budget);
    let total = WARM + entries;
    let mut proposed = 0u64;
    let mut answered = 0u64;
    let mut before = None;
    while proposed < clients.min(total) {
        group.propose();
        proposed += 1;
    }
    let mut idle = 0u32;
    while answered < total {
        let answers = group.turn() as u64;
        for _ in 0..answers {
            answered += 1;
            if answered == WARM {
                before = Some(usage::this().unwrap());
            }
            if proposed < total {
                group.propose();
                proposed += 1;
            }
        }
        // The store answers at once, so a turn that answers nothing is followed by one that
        // does; a counted bound stops a run that would never commit.
        idle = if answers == 0 { idle + 1 } else { 0 };
        assert!(idle < 100_000, "the group stopped committing");
    }
    usage::this().unwrap().since(&before.unwrap())
}

fn spread(values: &mut [f64]) -> String {
    let s = hyper_measure::stats::Summary::of(values).unwrap();
    format!("{:.0} [{:.0}–{:.0}]", s.median, s.min, s.max)
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    let entries: u64 = args.first().map_or(100_000, |n| n.parse().unwrap());
    let rounds: usize = args.get(1).map_or(9, |n| n.parse().unwrap());
    let clients: Vec<u64> = args.get(2).map_or(vec![1, 64], |c| {
        c.split(',').map(|n| n.trim().parse().unwrap()).collect()
    });
    let (tell, woken) = sync_channel(1 << 16);
    let wakers: Vec<Waker> = (0..MEMBERS as usize)
        .map(|slot| hyper_measure::wake::waker(slot, tell.clone()).0)
        .collect();
    println!(
        "| clients | budget | instructions/entry | cycles/entry | CPU ns/entry |\n|---|---|---|---|---|"
    );
    for &c in &clients {
        let mut by: [Vec<Usage>; 2] = [Vec::new(), Vec::new()];
        for round in 0..rounds {
            for k in 0..2 {
                let at = (k + round) % 2;
                let usage = if at == 0 {
                    run(&wakers, &woken, || Unbounded, entries, c)
                } else {
                    run(&wakers, &woken, || Bytes::new(u64::MAX), entries, c)
                };
                by[at].push(usage);
            }
        }
        for (at, name) in ["Unbounded", "Bytes"].iter().enumerate() {
            let per = |f: fn(&Usage) -> Option<u64>| -> Vec<f64> {
                by[at]
                    .iter()
                    .map(|u| f(u).map_or(f64::NAN, |v| v as f64 / entries as f64))
                    .collect()
            };
            println!(
                "| {c} | {name} | {} | {} | {} |",
                spread(&mut per(|u| u.instructions)),
                spread(&mut per(|u| u.cycles)),
                spread(&mut per(|u| Some(u.user_ns + u.system_ns)))
            );
        }
    }
}
