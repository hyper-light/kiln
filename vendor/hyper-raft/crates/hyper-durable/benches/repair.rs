//! What repairing a member costs by entries and by an image, on the disk (`docs/durable.md` §14 item 5):
//! whether a leader should choose an image past a crossover measured on a real path.
//!
//! ```text
//! cargo bench -p hyper-durable --bench repair -- DIR [LOST] [ROUNDS] [ENTRIES]
//! ```
//!
//! Three voters on hyper-log, each log and each image a scratch file under `DIR` flushed with the
//! platform's full flush, driven as `support` states. Protocol-Aware Recovery's workload (Alagappan et
//! al., FAST 2018, §5.2), as the core's own repair bench takes it (`hyper-raft/benches/repair.rs`):
//! `ENTRIES` entries of a kibibyte (30,000 by default), applied by a state machine that keeps every
//! entry's bytes, whose image is those bytes, made durable in a file of its own. The third member is
//! then cut off, the detectors on both sides suspecting the other, while the others commit `LOST`
//! entries more (`1,10,100,1000,10000` by default), the entries it lacks, as a member that lost them
//! at rest lacks them (R-5 resends them alike; the mark decides elections, not what is sent).
//! Healed, the detectors trust again and the leader asks it where it is, and it is repaired one of
//! two ways, `ROUNDS` times each (3 by default) in turn: **by entries**, the leader sending what it
//! lacks from its log; **by an image**, the leader having compacted its log past them first, so it
//! sends its machine's image and the member installs it. Each repair is timed from the heal until
//! the member has applied all the leader applied, and its messages' bytes counted. The network is
//! in-process: the disks are real.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_methods
)]

mod support;

use std::path::{Path, PathBuf};

use hyper_block::buf::{AlignedBuf, Alignment};
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_block::scratch::Scratch;
use hyper_durable::{EntryRef, Fatal, Point, StateMachine};
use hyper_raft::proto::{ConfChangeV2, ConfState};
use support::{Device, Group, MEMBERS, Shape, Wakes, now};

/// Each entry's bytes: a kibibyte, PAR's (§5.2).
const ENTRY_BYTES: usize = 1024;
/// The log's depth: a group's writes out at once, as the shell runs them.
const DEPTH: usize = 3;
/// The proposals out at the leader while the group commits its entries: past what one append of the
/// group's settings carries (64 KiB, 64 entries), so commits come a full message at a time.
const OUT: usize = 256;
/// Turns a phase may take: a counted bound on a run that would never finish.
const TURNS: u64 = 100_000_000;
/// The image file's block: its writes are whole blocks, as a direct file's are.
const BLOCK: usize = 4096;

/// A state machine that keeps every entry's bytes, its image those bytes, durable in a file.
struct Keeper {
    configuration: ConfState,
    applied: Point,
    durable: Point,
    state: Vec<u8>,
    file: DeviceFile,
    // The file goes after its handle.
    _scratch: Scratch,
}

impl Keeper {
    fn new(dir: &Path) -> Self {
        let scratch = Scratch::create(dir, ".hyper-durable-image").unwrap();
        let file = DeviceFile::open(
            scratch.path(),
            false,
            CachingRequest::Buffered,
            Alignment::new(BLOCK).unwrap(),
        )
        .unwrap();
        Self {
            configuration: ConfState {
                voters: (1..=MEMBERS).collect(),
                ..ConfState::default()
            },
            applied: Point::default(),
            durable: Point::default(),
            state: Vec::new(),
            file,
            _scratch: scratch,
        }
    }

    /// Writes the state whole, padded to the file's block, and flushes it.
    fn write_image(&mut self) -> Result<(), Fatal> {
        let len = self.state.len().div_ceil(BLOCK).max(1) * BLOCK;
        let align = Alignment::new(BLOCK).map_err(|_| Fatal("the image's block"))?;
        let mut buf = AlignedBuf::zeroed(len, align).map_err(|_| Fatal("an image buffer"))?;
        buf.extend_from_slice(&self.state)
            .map_err(|_| Fatal("an image buffer"))?;
        buf.set_len(len).map_err(|_| Fatal("an image buffer"))?;
        self.file
            .write_all_at(buf.as_slice(), 0)
            .and_then(|()| self.file.sync_data())
            .map_err(|_| Fatal("the image's write"))
    }
}

impl StateMachine for Keeper {
    type Answer = ();

    fn apply(&mut self, entry: &EntryRef<'_>, answers: &mut Vec<()>) -> Result<(), Fatal> {
        if !entry.data.is_empty() {
            self.state.extend_from_slice(entry.data);
            answers.push(());
        }
        self.applied = Point {
            index: entry.index,
            term: entry.term,
        };
        Ok(())
    }
    fn apply_change(
        &mut self,
        at: Point,
        _: &ConfChangeV2,
        configuration: &ConfState,
    ) -> Result<(), Fatal> {
        self.configuration = configuration.clone();
        self.applied = at;
        Ok(())
    }
    fn durable(&self) -> Point {
        self.durable
    }
    fn configuration(&self) -> &ConfState {
        &self.configuration
    }
    fn acts_at_start(&self, _: &EntryRef<'_>) -> bool {
        false
    }
    fn image(&mut self, into: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        into.extend_from_slice(&self.state);
        Ok((self.applied, self.configuration.clone()))
    }
    fn image_bytes(&self) -> Option<u64> {
        u64::try_from(self.state.len()).ok()
    }
    fn install(&mut self, image: &[u8], at: Point, configuration: &ConfState) -> Result<(), Fatal> {
        self.state.clear();
        self.state.extend_from_slice(image);
        self.write_image()?;
        self.configuration = configuration.clone();
        self.applied = at;
        self.durable = at;
        Ok(())
    }
    fn persist(&mut self) -> Result<(), Fatal> {
        self.write_image()?;
        self.durable = self.applied;
        Ok(())
    }
}

/// Commits `count` entries at the leader with [`OUT`] proposals out; the members `live` apply them.
fn commit(group: &mut Group<'_, Keeper>, count: u64, live: &[u64]) {
    let start = group.applied(1);
    let target = start + count;
    let mut proposed = 0u64;
    for _ in 0..TURNS {
        // The leader applies what it was proposed in order: what it applied since is answered.
        while proposed < count && proposed - (group.applied(1) - start) < OUT as u64 {
            if !group.propose_data(vec![0x5a; ENTRY_BYTES]) {
                break;
            }
            proposed += 1;
        }
        let answers = group.turn();
        if live.iter().all(|&m| group.applied(m) >= target) {
            return;
        }
        if answers == 0 && !group.busy() {
            group.wait(None);
        }
    }
    panic!("the group stopped committing");
}

/// Drives the group until `until` holds: asked again after each turn, before waiting, since the
/// turn that makes it hold may leave nothing to wait for.
fn settle(group: &mut Group<'_, Keeper>, mut until: impl FnMut(&Group<'_, Keeper>) -> bool) {
    if until(group) {
        return;
    }
    for _ in 0..TURNS {
        let answers = group.turn();
        if until(group) {
            return;
        }
        if answers == 0 && !group.busy() {
            group.wait(None);
        }
    }
    panic!("the group never settled");
}

/// One repair of `lost` entries: its time and the bytes the member was sent.
fn repair(
    wakes: &Wakes,
    dir: &Path,
    entries: u64,
    lost: u64,
    image: bool,
    seed: u64,
) -> (u64, u64) {
    let mut scratch = Vec::new();
    let shape = Shape::on(Device::File, DEPTH);
    let mut group = Group::open_with(
        wakes,
        shape,
        |kind, id| support::device(kind, id, dir, seed, &mut scratch),
        |_| Keeper::new(dir),
    );
    commit(&mut group, entries, &[1, 2, 3]);
    group.set_cut(Some(3));
    commit(&mut group, lost, &[1, 2]);
    if image {
        let behind = group.applied(3);
        assert!(group.compact_leader(0), "the leader compacts");
        settle(&mut group, |g| g.leader_start() > behind + 1);
    }
    let sent = group.bytes_to[2];
    let started = now();
    group.set_cut(None);
    let target = group.applied(1);
    settle(&mut group, |g| g.applied(3) >= target);
    let took = now() - started;
    let bytes = group.bytes_to[2] - sent;
    assert_eq!(
        group.machine(3).state.len(),
        group.machine(1).state.len(),
        "the repaired member holds the leader's state"
    );
    drop(group);
    drop(scratch);
    (took, bytes)
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    let dir = PathBuf::from(
        args.first()
            .expect("a directory for the logs' and images' files"),
    );
    let losts: Vec<u64> = support::list(args.get(1), &[1, 10, 100, 1000, 10_000]);
    let rounds: usize = args.get(2).map_or(3, |r| r.parse().unwrap());
    let entries: u64 = args.get(3).map_or(30_000, |n| n.parse().unwrap());
    let wakes = Wakes::new();
    println!(
        "\n{entries} entries of {ENTRY_BYTES} bytes, {rounds} rounds a point; load {}\n",
        support::load()
    );
    println!(
        "| lost | by entries: ms, median [least–most] | bytes sent | by an image: ms, median [least–most] | bytes sent |\n|---|---|---|---|---|"
    );
    for &lost in &losts {
        let mut by: [Vec<(u64, u64)>; 2] = [Vec::new(), Vec::new()];
        for round in 0..rounds {
            for k in 0..2 {
                let at = (k + round) % 2;
                by[at].push(repair(
                    &wakes,
                    &dir,
                    entries,
                    lost,
                    at == 1,
                    0x7265_7061 ^ ((round as u64) << 8) ^ lost,
                ));
            }
        }
        let cell = |runs: &[(u64, u64)]| {
            let mut ms: Vec<f64> = runs.iter().map(|r| r.0 as f64 / 1e6).collect();
            ms.sort_by(f64::total_cmp);
            let mut bytes: Vec<u64> = runs.iter().map(|r| r.1).collect();
            bytes.sort_unstable();
            (
                format!(
                    "{:.1} [{:.1}–{:.1}]",
                    ms[ms.len() / 2],
                    ms[0],
                    ms[ms.len() - 1]
                ),
                bytes[bytes.len() / 2],
            )
        };
        let (entries_ms, entries_bytes) = cell(&by[0]);
        let (image_ms, image_bytes) = cell(&by[1]);
        println!("| {lost} | {entries_ms} | {entries_bytes} | {image_ms} | {image_bytes} |");
    }
    println!("\nload after: {}", support::load());
}
