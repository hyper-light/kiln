//! What the shell's benchmarks on hyper-log share: a group of three voters, each on a log of its
//! own, one owner driving them on the bench's thread, and the closed and open loops that measure
//! a group's commits.
//!
//! Each member's log is a scratch file under the run's directory (`File`: direct I/O where the
//! file system takes it, flushed with the platform's full flush) or hyper-block's simulated device
//! (`Sim`, whose flush costs nothing). The owner is woken by the logs' answers and by the replicas'
//! own deadlines; a message is delivered in the turn it was sent. Every entry is 64 bytes, as in
//! the core's pipeline bench (`hyper-raft/benches/pipeline.rs`); the settings are mantle's range
//! group's, electing by suspicion, so no election starts during a run.
//!
//! A run elects member 1, commits [`WARM`] entries, then measures its entries: each latency from
//! its proposal (closed loop) or its scheduled arrival (open loop, `hyper_measure::open_loop`) to
//! the leader's answer; entries a second; an entry's frames, flushes and confirmations made
//! durable without a flush, over every member's log (`hyper_log::LogStats`); and the process's CPU
//! an entry, every thread's.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::cognitive_complexity,
    clippy::disallowed_methods
)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{Receiver, sync_channel};
use std::task::Waker;
use std::time::{Duration, Instant};

use hyper_block::DiskError;
use hyper_block::block::{BlockFile, Durable};
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_block::scratch::Scratch;
use hyper_block::sim::SimFile;
use hyper_durable::{
    EntryRef, Fatal, Fault, GroupStore, Handle, LogStore, Output, Owner, Point, Replica,
    ReplicaError, Settings, StateMachine, StoreView, Unbounded, Write,
};
use hyper_log::{Config as LogConfig, Log, Waits};
use hyper_measure::open_loop::Arrivals;
use hyper_measure::quantile::{self, Level};
use hyper_raft::proto::{ConfChangeV2, ConfState, Message};
use hyper_raft::{Config, StorageError};

/// The group's voters: mantle's range group, and the pipeline bench's.
pub const MEMBERS: u64 = 3;
/// Each entry's bytes: the pipeline bench's.
pub const ENTRY_BYTES: usize = 64;
/// Entries committed before a run measures: the group's first appends and the logs' first frames
/// out of the measurement.
pub const WARM: u64 = 100;
/// The coverage every quantile's interval states (`docs/tails.md` §3.3).
pub const COVERAGE: Level = match Level::new(95, 100) {
    Ok(level) => level,
    Err(_) => panic!("95/100 is a probability"),
};
/// The group every member's log holds.
const GROUP: u128 = 0x0064_6570_7468;

/// A run's clock: nanoseconds since the process's first reading.
pub fn now() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    u64::try_from(epoch.elapsed().as_nanos()).unwrap()
}

/// What the group applies: entries counted, one answer each; no image, so a run compacts nothing
/// and each opens on fresh logs.
pub struct Counter {
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

/// Where a member's log lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    /// A scratch file on the run's directory's disk.
    File,
    /// hyper-block's simulated device.
    Sim,
}

/// A member's device, either kind, so one group may hold both.
pub enum Member {
    File(Box<DeviceFile>),
    Sim(Box<SimFile>),
}

macro_rules! each {
    ($self:ident, $f:ident => $e:expr) => {
        match $self {
            Member::File($f) => $e,
            Member::Sim($f) => $e,
        }
    };
}

impl BlockFile for Member {
    fn alignment(&self) -> Alignment {
        each!(self, f => f.alignment())
    }
    fn layout_block(&self) -> Alignment {
        each!(self, f => f.layout_block())
    }
    fn len(&self) -> Result<u64, DiskError> {
        each!(self, f => f.len())
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), DiskError> {
        each!(self, f => f.read_exact_at(buf, offset))
    }
    fn read_resident_at(&mut self, buf: &mut [u8], offset: u64) -> Result<bool, DiskError> {
        each!(self, f => f.read_resident_at(buf, offset))
    }
    fn reads_resident(&self) -> bool {
        each!(self, f => f.reads_resident())
    }
    fn residency(&self) -> hyper_block::Residency {
        each!(self, f => f.residency())
    }
    fn set_residency_shadow(&mut self, on: bool) {
        each!(self, f => f.set_residency_shadow(on));
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<(), DiskError> {
        each!(self, f => f.write_all_at(buf, offset))
    }
    fn sync_data(&self) -> Result<(), DiskError> {
        each!(self, f => f.sync_data())
    }
    fn fills_new_space(&self) -> bool {
        each!(self, f => f.fills_new_space())
    }
    fn grows_by_segments(&self) -> bool {
        each!(self, f => f.grows_by_segments())
    }
    fn grow(&self, len: u64) -> Result<Option<u64>, DiskError> {
        each!(self, f => f.grow(len))
    }
    fn write_whole(&self, offset: u64, copies: u64, fill: &[u8]) -> Result<(), DiskError> {
        each!(self, f => f.write_whole(offset, copies, fill))
    }
    fn segment_end(&self, offset: u64) -> Result<Option<u64>, DiskError> {
        each!(self, f => f.segment_end(offset))
    }
    fn write_durable_at(&self, buf: &[u8], offset: u64) -> Result<Durable, DiskError> {
        each!(self, f => f.write_durable_at(buf, offset))
    }
    fn try_clone(&self) -> Result<Self, DiskError> {
        match self {
            Self::File(f) => f.try_clone().map(|f| Self::File(Box::new(f))),
            Self::Sim(f) => f.try_clone().map(|f| Self::Sim(Box::new(f))),
        }
    }
}

/// hyper-log's group handle at the depth the run gives it.
struct Depth(GroupStore<Member>, usize);

impl LogStore for Depth {
    type Hold = std::convert::Infallible;

    fn held(&self) -> Option<&Self::Hold> {
        None
    }
    fn release(&mut self, met: &Self::Hold) {
        match *met {}
    }
    fn depth(&self) -> usize {
        self.1
    }
    fn view(&self) -> Result<StoreView, Fault> {
        self.0.view()
    }
    fn bounds(&self) -> Result<(Point, u64), StorageError> {
        self.0.bounds()
    }
    fn term(&self, index: u64) -> Result<u64, StorageError> {
        self.0.term(index)
    }
    fn entries(
        &self,
        low: u64,
        high: u64,
        max: u64,
        into: &mut Vec<hyper_raft::proto::Entry>,
    ) -> Result<(), StorageError> {
        self.0.entries(low, high, max, into)
    }
    fn visit(
        &self,
        low: u64,
        high: u64,
        page: u64,
        visit: &mut dyn FnMut(EntryRef<'_>) -> bool,
    ) -> Result<(), StorageError> {
        self.0.visit(low, high, page, visit)
    }
    fn proposals(&self, into: &mut Vec<hyper_raft::proto::Entry>) -> Result<(), StorageError> {
        self.0.proposals(into)
    }
    fn released(&self) -> Result<u64, StorageError> {
        self.0.released()
    }
    fn room(&self) -> bool {
        self.0.room()
    }
    fn submit(&mut self, write: &Write<'_>, waker: &Waker) -> Result<(), Fault> {
        self.0.submit(write, waker)
    }
    fn poll(&mut self) -> Option<Result<(), Fault>> {
        self.0.poll()
    }
    fn write_now(&mut self, write: &Write<'_>) -> Result<(), Fault> {
        self.0.write_now(write)
    }
    fn keep_after(&mut self, index: u64) {
        self.0.keep_after(index);
    }
    fn kept_bytes(&self) -> u64 {
        self.0.kept_bytes()
    }
}

/// A log large enough for a run's frames, never compacted: one group, mantle's waits.
fn log_config() -> LogConfig {
    log_config_for(1)
}

/// The log of one group's, holding `groups` groups: one group's segments for each (a run's groups
/// commit as many entries each as one group's run, and nothing is compacted), and a queue that
/// admits every write their members may have out at the log's depth, three
/// (`GroupStore::depth`), as hyper-log's own benches size theirs by the replicas they drive, so no
/// write waits for room and stalls its group.
fn log_config_for(groups: usize) -> LogConfig {
    let segments = u32::try_from(groups).unwrap() * 64;
    LogConfig {
        segment_bytes: 1 << 20,
        max_segments: segments,
        max_groups: groups,
        group_entries: 1 << 18,
        group_bytes: 1 << 26,
        group_cache: 1 << 16,
        queue_submissions: (groups * 3).max(64),
        waits: Waits::Measured,
    }
}

/// mantle's range group's settings (as `hyper-durable-compare` states them from mantle
/// `1c179e8`), electing by suspicion; `apply_ahead` lets the leader apply its own term's committed
/// entries before its own write of them is durable (`docs/durable.md` §4.2).
fn settings(id: u64, apply_ahead: bool, quiet: Duration) -> Settings {
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
            apply_unpersisted: apply_ahead,
            seed: id,
            ..Config::new(id, limits)
        },
        quiet,
        elections: hyper_raft::Elections::Suspicion,
    }
}

/// The process's wakers, one a member, made once: each leaks its slot.
pub struct Wakes {
    wakers: Vec<Waker>,
    woken: Receiver<usize>,
}

impl Wakes {
    pub fn new() -> Self {
        Self::for_groups(1)
    }

    /// Wakers for `groups` groups' members, one a member. A waker that finds the channel full
    /// drops its wake, so the channel holds every wake that can be pending at once: one a write a
    /// member has out, at most the log's three pipeline frames (`GroupStore::depth`).
    pub fn for_groups(groups: usize) -> Self {
        let slots = groups * MEMBERS as usize;
        let (tell, woken) = sync_channel((slots * 3).max(4096));
        let wakers = (0..slots)
            .map(|slot| hyper_measure::wake::waker(slot, tell.clone()).0)
            .collect();
        Self { wakers, woken }
    }
}

/// What a group is made of: its writes' depth, whether its leader applies ahead of its own
/// write, each member's device (the leader's first), and the owner's quiet period, after which a
/// commit no write has stated is written (`docs/durable.md` §4.1).
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub depth: usize,
    pub apply_ahead: bool,
    pub devices: [Device; MEMBERS as usize],
    pub quiet: Duration,
}

impl Shape {
    /// `depth` on every member's `device`, the leader applying after its own write, mantle's quiet
    /// period (10 ms, as `hyper-durable-compare` states mantle's settings).
    pub fn on(device: Device, depth: usize) -> Self {
        Self {
            depth,
            apply_ahead: false,
            devices: [device; MEMBERS as usize],
            quiet: Duration::from_millis(10),
        }
    }
}

/// How proposals come.
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    /// Proposals out at once, each proposed again at its answer.
    Closed(usize),
    /// Arrivals a second, a Poisson process.
    Open(f64),
}

impl Mode {
    pub fn label(self) -> String {
        match self {
            Self::Closed(clients) => format!("closed loop, {clients} client(s)"),
            Self::Open(rate) => format!("open loop, {rate:.0} arrivals a second"),
        }
    }
}

/// What one run measured.
pub struct Run {
    pub latencies: Vec<u64>,
    /// Each span the leader held a `Ready` it could not take, its writes at the depth, nanoseconds.
    pub held: Vec<u64>,
    /// The leader's writes with something to make durable, and those with nothing, an entry.
    pub readies_per_entry: f64,
    pub empty_per_entry: f64,
    /// Each measured entry's phases, in the order answered: from its arrival to its first leaving
    /// the leader, from then to its answer, and the leader's writes out when it was proposed.
    pub phases: Vec<(u64, u64, usize)>,
    pub entries_per_second: f64,
    pub frames_per_entry: f64,
    /// Flushes of the device's cache an entry: each frame's, and each confirmation's written on its
    /// own with a flush (`LogStats::flushes`).
    pub flushes_per_entry: f64,
    /// Confirmations the device made durable on their own, without a flush (`RWF_DSYNC` on a
    /// direct file on Linux), an entry.
    pub durable_writes_per_entry: f64,
    /// The process's CPU (user and system, every thread: the owner's and the logs') an entry, µs.
    pub cpu_per_entry: f64,
}

/// One group on its logs, driven on this thread, applying to `M`.
pub struct Group<'w, M: StateMachine<Answer = ()> = Counter> {
    // Dropped before the logs, whose owners the handles reach.
    owner: Owner<Depth, M, Unbounded>,
    logs: Vec<Log<Member>>,
    handles: Vec<Handle>,
    woken: &'w Receiver<usize>,
    out: Output<()>,
    wire: VecDeque<Message>,
    /// Each member's own deadline, as its last drive gave it.
    deadlines: Vec<Option<u64>>,
    /// Raft messages the members sent.
    pub messages: u64,
    /// The store's depth: writes a member may have out.
    pub depth: usize,
    /// When the leader first sent each entry from `first_sent` on to a follower, nanoseconds: the
    /// phase between an entry's proposal and its leaving the leader.
    sent: VecDeque<u64>,
    /// The index `sent` starts at.
    first_sent: u64,
    /// A member whose messages, to it and from it, the network drops while it is cut.
    cut: Option<u64>,
    /// The bytes of the messages delivered to each member, the first's first.
    pub bytes_to: [u64; MEMBERS as usize],
    /// The leader's waker, for the calls an owner makes with one.
    leader_waker: Waker,
}

/// What a member holds now: the last entry it applied, its durable commit `C_d`, and the writes of
/// the commit alone it made after a quiet period.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemberState {
    pub applied: u64,
    pub durable_commit: u64,
    pub quiet_writes: u64,
}

impl<'w> Group<'w> {
    pub fn open(wakes: &'w Wakes, shape: Shape, device: impl FnMut(Device, u64) -> Member) -> Self {
        Self::open_with(wakes, shape, device, |_| Counter {
            configuration: ConfState {
                voters: (1..=MEMBERS).collect(),
                ..ConfState::default()
            },
        })
    }
}

impl<'w, M: StateMachine<Answer = ()>> Group<'w, M> {
    /// A group whose member `id` applies to `machine(id)`.
    pub fn open_with(
        wakes: &'w Wakes,
        shape: Shape,
        mut device: impl FnMut(Device, u64) -> Member,
        mut machine: impl FnMut(u64) -> M,
    ) -> Self {
        while wakes.woken.try_recv().is_ok() {}
        let mut owner = Owner::new(wakes.wakers[..MEMBERS as usize].to_vec());
        let mut logs = Vec::new();
        let mut handles = Vec::new();
        for (id, kind) in (1..=MEMBERS).zip(shape.devices) {
            let log =
                Log::create(device(kind, id), log_config(), 0x6c6f67 + u128::from(id)).unwrap();
            let store = GroupStore::claim(&log, GROUP).unwrap();
            assert!(
                shape.depth <= store.depth(),
                "depth {} past the log's",
                shape.depth
            );
            let replica = Replica::open(
                &settings(id, shape.apply_ahead, shape.quiet),
                Depth(store, shape.depth),
                machine(id),
                Unbounded,
            )
            .expect("a member opens");
            handles.push(owner.insert(replica).map_err(|_| ()).unwrap());
            logs.push(log);
        }
        let mut group = Self {
            owner,
            logs,
            handles,
            woken: &wakes.woken,
            out: Output::default(),
            wire: VecDeque::new(),
            deadlines: vec![None; MEMBERS as usize],
            messages: 0,
            depth: shape.depth,
            sent: VecDeque::new(),
            first_sent: 0,
            cut: None,
            bytes_to: [0; MEMBERS as usize],
            leader_waker: wakes
                .wakers
                .first()
                .cloned()
                .unwrap_or_else(|| Waker::noop().clone()),
        };
        let first = group.handles[0];
        group.owner.get_mut(first).unwrap().campaign().unwrap();
        group.owner.schedule(first);
        // A counted budget of turns: an election on three members on this thread takes a handful.
        for _ in 0..100_000 {
            group.turn();
            let leads = group.owner.get(first).unwrap().is_leader();
            if leads && !group.busy() {
                return group;
            }
            group.wait(None);
        }
        panic!("the group never elected member 1");
    }

    /// Drives every queued member once and delivers what they sent; returns the leader's answers.
    pub fn turn(&mut self) -> usize {
        let leader = self.handles[0];
        let (wire, deadlines, handles) = (&mut self.wire, &mut self.deadlines, &self.handles);
        let mut answers = 0;
        let mut sent = 0u64;
        self.owner.turn(now(), &mut self.out, |h, driven, out| {
            let driven = driven.expect("a member drives");
            sent += out.messages.len() as u64;
            wire.extend(out.messages.drain(..));
            if let Some(at) = handles.iter().position(|&x| x == h) {
                deadlines[at] = driven.wake;
            }
            if h == leader {
                answers += out.answers.len();
            }
        });
        self.messages += sent;
        let at = now();
        for m in &self.wire {
            if m.from != 1 || m.msg_type != hyper_raft::proto::MessageType::MsgAppend {
                continue;
            }
            for e in &m.entries {
                // Each index once, in order: an entry sent again is not its first leaving.
                if e.index >= self.first_sent + self.sent.len() as u64 {
                    if self.sent.is_empty() {
                        self.first_sent = e.index;
                    }
                    while self.first_sent + (self.sent.len() as u64) < e.index {
                        self.sent.push_back(at);
                    }
                    self.sent.push_back(at);
                }
            }
        }
        while let Some(m) = self.wire.pop_front() {
            if self.cut.is_some_and(|cut| cut == m.to || cut == m.from) {
                continue;
            }
            let Some(&to) = self.handles.get(usize::try_from(m.to).unwrap() - 1) else {
                continue;
            };
            if let Some(b) = self.bytes_to.get_mut(usize::try_from(m.to).unwrap() - 1) {
                *b += hyper_raft::proto::message_bytes(&m) as u64;
            }
            match self.owner.get_mut(to).unwrap().step(m) {
                Ok(()) | Err(ReplicaError::Refused(_) | ReplicaError::Stalled) => {}
                Err(e) => panic!("step: {e}"),
            }
            self.owner.schedule(to);
        }
        answers
    }

    pub fn busy(&self) -> bool {
        self.owner.has_work() || !self.wire.is_empty()
    }

    /// Whether a member asked to be driven at a time of its own.
    pub fn has_deadline(&self) -> bool {
        self.deadlines.iter().any(Option::is_some)
    }

    /// Whether a member has a write out, whose answer will wake the owner.
    pub fn writes_out(&self) -> bool {
        self.handles
            .iter()
            .any(|&h| self.owner.get(h).is_some_and(|r| r.in_flight() > 0))
    }

    /// With nothing to drive, waits for a log's answer, a member's deadline or `until`.
    pub fn wait(&mut self, until: Option<u64>) {
        if !self.busy() {
            let deadline = self.deadlines.iter().flatten().copied().chain(until).min();
            let at = now();
            match deadline {
                Some(d) if d <= at => {}
                Some(d) => {
                    if let Ok(slot) = self.woken.recv_timeout(Duration::from_nanos(d - at)) {
                        self.owner.woken(slot);
                    }
                }
                None => {
                    if let Ok(slot) = self.woken.try_recv() {
                        self.owner.woken(slot);
                    } else {
                        // Only a log's answer can wake the group now: with no write out, none
                        // will come.
                        assert!(
                            self.writes_out(),
                            "the group waits with nothing to wake it: {:#?}",
                            self.stuck()
                        );
                        if let Ok(slot) = self.woken.recv() {
                            self.owner.woken(slot);
                        }
                    }
                }
            }
        }
        while let Ok(slot) = self.woken.try_recv() {
            self.owner.woken(slot);
        }
        let at = now();
        for (i, d) in self.deadlines.iter_mut().enumerate() {
            if d.is_some_and(|d| d <= at) {
                *d = None;
                self.owner.schedule(self.handles[i]);
            }
        }
    }

    /// Each member's role, term, applied, committed, first and last index, writes out, whether it
    /// holds a `Ready` and the writes it made, and on the leader each member's progress: what a
    /// group that waits with nothing to wake it was left in.
    fn stuck(&self) -> Vec<String> {
        self.handles
            .iter()
            .map(|&h| {
                let r = self.owner.get(h).expect("a member is held");
                let raft = &r.core().raft;
                let progress: Vec<String> = if r.is_leader() {
                    raft.tracker()
                        .iter()
                        .map(|(id, p)| {
                            format!(
                                "{id}: {:?} matched {} next {} full {}",
                                p.state,
                                p.matched,
                                p.next_index,
                                p.inflights.full()
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                format!(
                    "{}: {:?} term {} applied {} committed {} first {} last {} out {} ready {} \
                     stalled {} cut {:?} {:?} {progress:?}",
                    r.id(),
                    raft.state(),
                    raft.term(),
                    r.applied().index,
                    raft.log().committed(),
                    raft.log().first_index().unwrap_or(0),
                    raft.log().last_index().unwrap_or(0),
                    r.in_flight(),
                    r.core().has_ready(),
                    r.is_stalled(),
                    self.cut,
                    r.writes()
                )
            })
            .collect()
    }

    /// Whether the leader holds a `Ready` it may not take: its writes out at the store's depth.
    fn leader_held(&self, depth: usize) -> bool {
        self.owner
            .get(self.handles[0])
            .is_some_and(|r| r.core().has_ready() && r.in_flight() >= depth)
    }

    /// The leader's writes so far, by what they were for.
    fn leader_writes(&self) -> hyper_durable::Writes {
        self.owner
            .get(self.handles[0])
            .map(Replica::writes)
            .unwrap_or_default()
    }

    /// The leader's last index and its writes out.
    fn leader_state(&self) -> (u64, usize) {
        let r = self.owner.get(self.handles[0]).expect("the leader is held");
        (r.core().raft.log().last_index().unwrap_or(0), r.in_flight())
    }

    /// When the leader first sent entry `index`, if it has.
    fn sent_at(&self, index: u64) -> Option<u64> {
        index
            .checked_sub(self.first_sent)
            .and_then(|at| self.sent.get(usize::try_from(at).ok()?))
            .copied()
    }

    /// Proposes an entry at the leader; false when the core refused it for now.
    pub fn propose(&mut self) -> bool {
        self.propose_data(vec![0x5a; ENTRY_BYTES])
    }

    /// Proposes `data` at the leader; false when the core refused it for now.
    pub fn propose_data(&mut self, data: Vec<u8>) -> bool {
        let leader = self.handles[0];
        let taken = self
            .owner
            .get_mut(leader)
            .unwrap()
            .propose(Vec::new(), data)
            .is_ok();
        self.owner.schedule(leader);
        taken
    }

    /// Cuts `member` off the network, or heals the cut with `None`: what it sends and what it is
    /// sent is dropped meanwhile. The detectors on both sides of a cut say what the node-pair
    /// liveness stream says of one (`docs/timing.md` §2.1): each side suspects the other while it
    /// lasts and trusts it again once healed, and the leader, trusting the member again, tells it
    /// who leads, so a member cut off while its group was idle catches up (`hyper_raft::watch`).
    /// Without the words, a group with nothing in flight sends the healed member nothing: a leader
    /// beats only on its owner's timing, which a bench in one process does not measure.
    pub fn set_cut(&mut self, member: Option<u64>) {
        let (cut, trusted) = match (member, self.cut) {
            (Some(cut), _) => (cut, false),
            (None, Some(cut)) => (cut, true),
            (None, None) => return,
        };
        self.cut = member;
        let handles = self.handles.clone();
        for (at, &h) in handles.iter().enumerate() {
            let id = at as u64 + 1;
            let peers: Vec<u64> = if id == cut {
                (1..=MEMBERS).filter(|&p| p != cut).collect()
            } else {
                vec![cut]
            };
            let r = self.owner.get_mut(h).expect("a member is held");
            for peer in peers {
                let said = if trusted {
                    r.trust(peer)
                } else {
                    r.suspect(peer)
                };
                said.expect("a member takes its detectors' word");
            }
            self.owner.schedule(h);
        }
    }

    /// Member `id`'s last index applied.
    pub fn applied(&self, id: u64) -> u64 {
        let h = self.handles[usize::try_from(id).unwrap() - 1];
        self.owner.get(h).map_or(0, |r| r.applied().index)
    }

    /// The member `id`'s state machine.
    pub fn machine(&self, id: u64) -> &M {
        let h = self.handles[usize::try_from(id).unwrap() - 1];
        self.owner.get(h).expect("a member is held").machine()
    }

    /// Has the leader compact its log, keeping `keep` entries behind its machine's durable point;
    /// true when a compaction's start went out.
    pub fn compact_leader(&mut self, keep: u64) -> bool {
        let leader = self.handles[0];
        let waker = self.leader_waker.clone();
        let started = self
            .owner
            .get_mut(leader)
            .unwrap()
            .compact(keep, now(), &waker)
            .expect("the leader compacts");
        self.owner.schedule(leader);
        started
    }

    /// The leader's log's first index held.
    pub fn leader_start(&self) -> u64 {
        self.owner
            .get(self.handles[0])
            .and_then(|r| r.core().raft.log().first_index().ok())
            .unwrap_or(0)
    }

    /// Each member's state, the leader's first.
    pub fn members(&self) -> [MemberState; MEMBERS as usize] {
        let mut states = [MemberState::default(); MEMBERS as usize];
        for (state, &h) in states.iter_mut().zip(&self.handles) {
            let r = self.owner.get(h).expect("a member is held");
            *state = MemberState {
                applied: r.applied().index,
                durable_commit: r.durable_commit(),
                quiet_writes: r.writes().quiet,
            };
        }
        states
    }

    /// What every member's log has written and flushed since it opened.
    pub fn counts(&self) -> Counts {
        self.logs.iter().fold(Counts::default(), |c, log| {
            let s = log.stats(None).expect("a log answers its statistics");
            Counts {
                frames: c.frames + s.frames,
                flushes: c.flushes + s.flushes,
                durable_writes: c.durable_writes + s.durable_writes,
            }
        })
    }
}

/// Frames written, flushes made and confirmations made durable without one, over the logs.
#[derive(Clone, Copy, Default)]
pub struct Counts {
    pub frames: u64,
    pub flushes: u64,
    pub durable_writes: u64,
}

/// When a run's measured entries began, and what the logs had done then.
struct Mark {
    at: u64,
    counts: Counts,
    usage: Option<hyper_measure::usage::Usage>,
}

/// One run's loop: arrivals from `arrive` (none left when it gives none), each proposed as the
/// leader takes it and timed from its arrival; `again` proposes a closed loop's next entry at
/// each answer.
fn drive(
    group: &mut Group<'_, Counter>,
    entries: u64,
    mut arrive: impl FnMut(u64) -> Option<u64>,
    again: bool,
) -> Run {
    let total = WARM + entries;
    let mut waiting: VecDeque<u64> = VecDeque::new();
    // Each proposal taken: its arrival, its index, the leader's writes out as it was proposed.
    let mut taken: VecDeque<(u64, u64, usize)> = VecDeque::new();
    let mut latencies = Vec::with_capacity(usize::try_from(entries).unwrap());
    let mut phases = Vec::with_capacity(usize::try_from(entries).unwrap());
    let mut held: Vec<u64> = Vec::new();
    let mut held_since: Option<u64> = None;
    let mut writes_at_mark = group.leader_writes();
    let mut arrived = 0u64;
    let mut answered = 0u64;
    let mut next = arrive(now());
    let mut mark = Mark {
        at: now(),
        counts: group.counts(),
        usage: hyper_measure::usage::this().ok(),
    };
    while answered < total {
        let at = now();
        while let Some(t) = next.filter(|&t| t <= at && arrived < total) {
            waiting.push_back(t);
            arrived += 1;
            next = arrive(at).filter(|_| arrived < total);
        }
        let mut refused = false;
        while let Some(&t) = waiting.front() {
            let (_, out) = group.leader_state();
            if !group.propose() {
                refused = true;
                break;
            }
            let (index, _) = group.leader_state();
            taken.push_back((t, index, out));
            waiting.pop_front();
        }
        let answers = group.turn();
        let at = now();
        for _ in 0..answers {
            let (t, index, out) = taken
                .pop_front()
                .expect("an answer for each proposal taken");
            answered += 1;
            if answered > WARM {
                latencies.push(at.saturating_sub(t));
                let sent = group.sent_at(index).unwrap_or(at);
                phases.push((sent.saturating_sub(t), at.saturating_sub(sent), out));
            }
            if answered == WARM {
                mark = Mark {
                    at,
                    counts: group.counts(),
                    usage: hyper_measure::usage::this().ok(),
                };
                writes_at_mark = group.leader_writes();
                held.clear();
            }
            if again && arrived < total {
                waiting.push_back(at);
                arrived += 1;
            }
        }
        match (group.leader_held(group.depth), held_since) {
            (true, None) => held_since = Some(at),
            (false, Some(since)) => {
                held.push(at.saturating_sub(since));
                held_since = None;
            }
            _ => {}
        }
        // With nothing answered and nothing it could propose, the loop waits for the group.
        if answers == 0 && (waiting.is_empty() || refused) {
            group.wait(next);
        }
    }
    let elapsed = now().saturating_sub(mark.at).max(1) as f64 / 1e9;
    let cpu = mark
        .usage
        .and_then(|b| hyper_measure::usage::this().ok().map(|a| a.since(&b)))
        .map_or(f64::NAN, |u| (u.user_ns + u.system_ns) as f64 / 1e3);
    let end = group.counts();
    let per_entry = |n: u64, at: u64| n.saturating_sub(at) as f64 / entries as f64;
    let writes = group.leader_writes();
    Run {
        latencies,
        phases,
        held,
        readies_per_entry: per_entry(writes.readies, writes_at_mark.readies),
        empty_per_entry: per_entry(writes.empty, writes_at_mark.empty),
        entries_per_second: entries as f64 / elapsed,
        frames_per_entry: per_entry(end.frames, mark.counts.frames),
        flushes_per_entry: per_entry(end.flushes, mark.counts.flushes),
        durable_writes_per_entry: per_entry(end.durable_writes, mark.counts.durable_writes),
        cpu_per_entry: cpu / entries as f64,
    }
}

/// Many groups of three voters on three logs, member `id` of every group on log `id`: a node's log
/// holding every group the node hosts, as mantle's ranges share their node's (`docs/durable.md`
/// §14 item 1). Every group's member 1 leads; one owner drives them all on this thread.
pub struct Groups<'w> {
    // Dropped before the logs, whose owners the handles reach.
    owner: Owner<Depth, Counter, Unbounded>,
    logs: Vec<Log<Member>>,
    /// Each group's members' handles, member 1, its leader, first.
    handles: Vec<[Handle; MEMBERS as usize]>,
    /// Each slot's handle and group.
    slots: Vec<(Handle, usize)>,
    woken: &'w Receiver<usize>,
    out: Output<()>,
    /// Messages sent and not yet delivered, each with its group.
    wire: VecDeque<(usize, Message)>,
    /// Each slot's own deadline, as its last drive gave it.
    deadlines: Vec<Option<u64>>,
    /// The store's depth: writes a member may have out.
    pub depth: usize,
}

impl<'w> Groups<'w> {
    /// `groups` groups of `shape`, each member's log on its device.
    pub fn open(
        wakes: &'w Wakes,
        groups: usize,
        shape: Shape,
        mut device: impl FnMut(Device, u64) -> Member,
    ) -> Self {
        while wakes.woken.try_recv().is_ok() {}
        let count = groups * MEMBERS as usize;
        assert!(
            count <= wakes.wakers.len(),
            "{groups} groups past the wakers"
        );
        let mut owner = Owner::new(wakes.wakers[..count].to_vec());
        let logs: Vec<Log<Member>> = (1..=MEMBERS)
            .zip(shape.devices)
            .map(|(id, kind)| {
                Log::create(
                    device(kind, id),
                    log_config_for(groups),
                    0x6c6f67 + u128::from(id),
                )
                .unwrap()
            })
            .collect();
        let mut handles = Vec::with_capacity(groups);
        let mut slots = vec![None; count];
        for g in 0..groups {
            let mut members = Vec::with_capacity(MEMBERS as usize);
            for (at, log) in logs.iter().enumerate() {
                let id = at as u64 + 1;
                let store = GroupStore::claim(log, GROUP + g as u128).unwrap();
                assert!(
                    shape.depth <= store.depth(),
                    "depth {} past the log's",
                    shape.depth
                );
                let replica = Replica::open(
                    &settings(id, shape.apply_ahead, shape.quiet),
                    Depth(store, shape.depth),
                    Counter {
                        configuration: ConfState {
                            voters: (1..=MEMBERS).collect(),
                            ..ConfState::default()
                        },
                    },
                    Unbounded,
                )
                .expect("a member opens");
                let h = owner.insert(replica).map_err(|_| ()).unwrap();
                slots[h.slot()] = Some((h, g));
                members.push(h);
            }
            handles.push(<[Handle; MEMBERS as usize]>::try_from(members).unwrap());
        }
        let mut set = Self {
            owner,
            logs,
            handles,
            slots: slots.into_iter().map(Option::unwrap).collect(),
            woken: &wakes.woken,
            out: Output::default(),
            wire: VecDeque::new(),
            deadlines: vec![None; count],
            depth: shape.depth,
        };
        for g in 0..groups {
            let first = set.handles[g][0];
            set.owner.get_mut(first).unwrap().campaign().unwrap();
            set.owner.schedule(first);
        }
        let mut answers = vec![0; groups];
        // A counted budget of turns: each group's election on this thread takes a handful.
        for _ in 0..100_000 * groups {
            set.turn(&mut answers);
            let lead = set
                .handles
                .iter()
                .all(|m| set.owner.get(m[0]).unwrap().is_leader());
            if lead && !set.busy() {
                return set;
            }
            set.wait(None);
        }
        panic!("the groups never elected their members 1");
    }

    /// How many groups the logs hold.
    pub fn count(&self) -> usize {
        self.handles.len()
    }

    /// Drives every queued member once and delivers what they sent; each group's leader's answers
    /// this turn in `answers`, a slot a group.
    pub fn turn(&mut self, answers: &mut [usize]) {
        answers.fill(0);
        let (wire, deadlines, handles, slots) = (
            &mut self.wire,
            &mut self.deadlines,
            &self.handles,
            &self.slots,
        );
        self.owner.turn(now(), &mut self.out, |h, driven, out| {
            let driven = driven.expect("a member drives");
            let g = slots[h.slot()].1;
            wire.extend(out.messages.drain(..).map(|m| (g, m)));
            deadlines[h.slot()] = driven.wake;
            if handles[g][0] == h {
                answers[g] += out.answers.len();
            }
        });
        while let Some((g, m)) = self.wire.pop_front() {
            let Some(&to) = self.handles[g].get(usize::try_from(m.to).unwrap() - 1) else {
                continue;
            };
            match self.owner.get_mut(to).unwrap().step(m) {
                Ok(()) | Err(ReplicaError::Refused(_) | ReplicaError::Stalled) => {}
                Err(e) => panic!("step: {e}"),
            }
            self.owner.schedule(to);
        }
    }

    pub fn busy(&self) -> bool {
        self.owner.has_work() || !self.wire.is_empty()
    }

    /// Whether a member has a write out, whose answer will wake the owner.
    fn writes_out(&self) -> bool {
        self.slots
            .iter()
            .any(|&(h, _)| self.owner.get(h).is_some_and(|r| r.in_flight() > 0))
    }

    /// With nothing to drive, waits for a log's answer, a member's deadline or `until`.
    pub fn wait(&mut self, until: Option<u64>) {
        if !self.busy() {
            let deadline = self.deadlines.iter().flatten().copied().chain(until).min();
            let at = now();
            match deadline {
                Some(d) if d <= at => {}
                Some(d) => {
                    if let Ok(slot) = self.woken.recv_timeout(Duration::from_nanos(d - at)) {
                        self.owner.woken(slot);
                    }
                }
                None => {
                    if let Ok(slot) = self.woken.try_recv() {
                        self.owner.woken(slot);
                    } else {
                        // Only a log's answer can wake the groups now: with no write out, none
                        // will come.
                        assert!(
                            self.writes_out(),
                            "the groups wait with nothing to wake them: {:#?}",
                            self.stuck()
                        );
                        if let Ok(slot) = self.woken.recv() {
                            self.owner.woken(slot);
                        }
                    }
                }
            }
        }
        while let Ok(slot) = self.woken.try_recv() {
            self.owner.woken(slot);
        }
        let at = now();
        for (slot, d) in self.deadlines.iter_mut().enumerate() {
            if d.is_some_and(|d| d <= at) {
                *d = None;
                self.owner.schedule(self.slots[slot].0);
            }
        }
    }

    /// The members of every group whose last index passes what they applied, or that hold a
    /// `Ready` or are stalled: what groups that wait with nothing to wake them were left in.
    fn stuck(&self) -> Vec<String> {
        self.handles
            .iter()
            .enumerate()
            .flat_map(|(g, members)| members.iter().map(move |&h| (g, h)))
            .filter_map(|(g, h)| {
                let r = self.owner.get(h)?;
                let raft = &r.core().raft;
                let last = raft.log().last_index().unwrap_or(0);
                (last > r.applied().index || r.core().has_ready() || r.is_stalled()).then(|| {
                    format!(
                        "group {g} member {}: {:?} term {} applied {} committed {} last {} out {} \
                         ready {} stalled {}",
                        r.id(),
                        raft.state(),
                        raft.term(),
                        r.applied().index,
                        raft.log().committed(),
                        last,
                        r.in_flight(),
                        r.core().has_ready(),
                        r.is_stalled()
                    )
                })
            })
            .take(24)
            .collect()
    }

    /// Proposes an entry at group `g`'s leader; false when its core refused it for now.
    pub fn propose(&mut self, g: usize) -> bool {
        let leader = self.handles[g][0];
        let taken = self
            .owner
            .get_mut(leader)
            .unwrap()
            .propose(Vec::new(), vec![0x5a; ENTRY_BYTES])
            .is_ok();
        self.owner.schedule(leader);
        taken
    }

    /// What every member's log has written and flushed since it opened.
    pub fn counts(&self) -> Counts {
        self.logs.iter().fold(Counts::default(), |c, log| {
            let s = log.stats(None).expect("a log answers its statistics");
            Counts {
                frames: c.frames + s.frames,
                flushes: c.flushes + s.flushes,
                durable_writes: c.durable_writes + s.durable_writes,
            }
        })
    }
}

/// One run's loop over many groups: arrivals from `arrive`, the `k`th to group `k mod groups`,
/// each proposed as its group's leader takes it and timed from its arrival; `again` proposes a
/// closed loop's next entry into the group that answered. [`WARM`] entries a group come before
/// the measured `entries`.
fn drive_groups(
    set: &mut Groups<'_>,
    entries: u64,
    mut arrive: impl FnMut(u64) -> Option<u64>,
    again: bool,
) -> Run {
    let groups = set.count();
    let warm = WARM * groups as u64;
    let total = warm + entries;
    let mut waiting: Vec<VecDeque<u64>> = vec![VecDeque::new(); groups];
    let mut taken: Vec<VecDeque<u64>> = vec![VecDeque::new(); groups];
    let mut answers = vec![0usize; groups];
    let mut latencies = Vec::with_capacity(usize::try_from(entries).unwrap());
    let mut arrived = 0u64;
    let mut answered = 0u64;
    let mut next = arrive(now());
    let mut mark = Mark {
        at: now(),
        counts: set.counts(),
        usage: hyper_measure::usage::this().ok(),
    };
    while answered < total {
        let at = now();
        while let Some(t) = next.filter(|&t| t <= at && arrived < total) {
            waiting[usize::try_from(arrived % groups as u64).unwrap()].push_back(t);
            arrived += 1;
            next = arrive(at).filter(|_| arrived < total);
        }
        let mut refused = false;
        let mut queued = false;
        for g in 0..groups {
            while let Some(&t) = waiting[g].front() {
                if !set.propose(g) {
                    refused = true;
                    break;
                }
                taken[g].push_back(t);
                waiting[g].pop_front();
            }
            queued |= !waiting[g].is_empty();
        }
        set.turn(&mut answers);
        let at = now();
        let mut any = false;
        for (g, &n) in answers.iter().enumerate() {
            for _ in 0..n {
                let t = taken[g]
                    .pop_front()
                    .expect("an answer for each proposal taken");
                any = true;
                answered += 1;
                if answered > warm {
                    latencies.push(at.saturating_sub(t));
                }
                if answered == warm {
                    mark = Mark {
                        at,
                        counts: set.counts(),
                        usage: hyper_measure::usage::this().ok(),
                    };
                }
                if again && arrived < total {
                    waiting[g].push_back(at);
                    arrived += 1;
                }
            }
        }
        // With nothing answered and nothing it could propose, the loop waits for the groups.
        if !any && (!queued || refused) {
            set.wait(next);
        }
    }
    let elapsed = now().saturating_sub(mark.at).max(1) as f64 / 1e9;
    let cpu = mark
        .usage
        .and_then(|b| hyper_measure::usage::this().ok().map(|a| a.since(&b)))
        .map_or(f64::NAN, |u| (u.user_ns + u.system_ns) as f64 / 1e3);
    let end = set.counts();
    let per_entry = |n: u64, at: u64| n.saturating_sub(at) as f64 / entries as f64;
    Run {
        latencies,
        phases: Vec::new(),
        held: Vec::new(),
        readies_per_entry: f64::NAN,
        empty_per_entry: f64::NAN,
        entries_per_second: entries as f64 / elapsed,
        frames_per_entry: per_entry(end.frames, mark.counts.frames),
        flushes_per_entry: per_entry(end.flushes, mark.counts.flushes),
        durable_writes_per_entry: per_entry(end.durable_writes, mark.counts.durable_writes),
        cpu_per_entry: cpu / entries as f64,
    }
}

/// One run of `groups` groups of `shape` under `mode` on fresh logs, its files under `dir`; a
/// closed loop's clients are `clients` a group.
pub fn run_groups(
    wakes: &Wakes,
    dir: &Path,
    groups: usize,
    shape: Shape,
    mode: Mode,
    entries: u64,
    seed: u64,
) -> Run {
    let mut scratch: Vec<Scratch> = Vec::new();
    let run = {
        let mut set = Groups::open(wakes, groups, shape, |kind, id| {
            device(kind, id, dir, seed, &mut scratch)
        });
        match mode {
            Mode::Closed(clients) => {
                let mut first = clients * groups;
                drive_groups(
                    &mut set,
                    entries,
                    |at| {
                        (first > 0).then(|| {
                            first -= 1;
                            at
                        })
                    },
                    true,
                )
            }
            Mode::Open(rate) => {
                let mut arrivals = Arrivals::poisson(rate, seed, now()).unwrap();
                drive_groups(
                    &mut set,
                    entries,
                    |_| Some(arrivals.next_arrival().unwrap()),
                    false,
                )
            }
        }
    };
    // The groups, their owner and their logs are gone; the files go after them.
    drop(scratch);
    run
}

/// A member's device of `kind`: a scratch file under `dir`, kept in `scratch` until the group is
/// gone, or a simulated device seeded from `seed`.
pub fn device(kind: Device, id: u64, dir: &Path, seed: u64, scratch: &mut Vec<Scratch>) -> Member {
    match kind {
        Device::File => {
            let s = Scratch::create(dir, ".hyper-durable-bench").unwrap();
            let file = DeviceFile::open(
                s.path(),
                false,
                CachingRequest::PreferDirect,
                Alignment::new(4096).unwrap(),
            )
            .unwrap();
            scratch.push(s);
            Member::File(Box::new(file))
        }
        Device::Sim => Member::Sim(Box::new(
            SimFile::new(
                Alignment::new(4096).unwrap(),
                Alignment::new(512).unwrap(),
                seed ^ id,
            )
            .unwrap(),
        )),
    }
}

/// One run of a group of `shape` under `mode` on fresh logs, its files under `dir`.
pub fn run(wakes: &Wakes, dir: &Path, shape: Shape, mode: Mode, entries: u64, seed: u64) -> Run {
    let mut scratch: Vec<Scratch> = Vec::new();
    let run = {
        let mut group = Group::open(wakes, shape, |kind, id| {
            device(kind, id, dir, seed, &mut scratch)
        });
        match mode {
            Mode::Closed(clients) => {
                let mut first = clients;
                drive(
                    &mut group,
                    entries,
                    |at| {
                        (first > 0).then(|| {
                            first -= 1;
                            at
                        })
                    },
                    true,
                )
            }
            Mode::Open(rate) => {
                let mut arrivals = Arrivals::poisson(rate, seed, now()).unwrap();
                drive(
                    &mut group,
                    entries,
                    |_| Some(arrivals.next_arrival().unwrap()),
                    false,
                )
            }
        }
    };
    // The group, its owner and its logs are gone; the files go after them.
    drop(scratch);
    run
}

/// The machine's load averages, as `uptime` prints them.
pub fn load() -> String {
    let out = std::process::Command::new("uptime").output();
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.split("load average")
                .nth(1)
                .map(|l| l.trim_start_matches(['s', ':', ' ']).trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The median of `values`, NaN for none.
pub fn median(values: &mut [f64]) -> f64 {
    hyper_measure::stats::median(values).unwrap_or(f64::NAN)
}

/// A quantile in microseconds with its interval.
pub fn cell(sorted: &[u64], q: Level) -> String {
    cell_with(sorted, q, |ns| format!("{:.0}", ns as f64 / 1e3))
}

/// A quantile with its interval, each sample written by `show`.
pub fn cell_with(sorted: &[u64], q: Level, show: impl Fn(u64) -> String) -> String {
    quantile::cell(sorted, q, COVERAGE, show)
}

/// The table's head, for rows of [`row`].
pub const HEAD: &str = "| p50 µs | p99 µs | p99.9 µs | entries/s | frames/entry | flushes/entry | durable writes/entry | CPU µs/entry |";

/// One row of runs pooled: the latencies' quantiles and each rate's median over the runs. Returns
/// the row and the median entries a second.
pub fn row(runs: &[Run]) -> (String, f64) {
    let mut all: Vec<u64> = runs
        .iter()
        .flat_map(|r| r.latencies.iter().copied())
        .collect();
    all.sort_unstable();
    let of = |f: fn(&Run) -> f64| median(&mut runs.iter().map(f).collect::<Vec<f64>>());
    let rate = of(|r| r.entries_per_second);
    (
        format!(
            "| {} | {} | {} | {:.0} | {:.3} | {:.3} | {:.3} | {:.1} |",
            cell(&all, Level::P50),
            cell(&all, Level::P99),
            cell(&all, Level::P999),
            rate,
            of(|r| r.frames_per_entry),
            of(|r| r.flushes_per_entry),
            of(|r| r.durable_writes_per_entry),
            of(|r| r.cpu_per_entry)
        ),
        rate,
    )
}

/// A run's own p50 latency, nanoseconds: its nearest-rank median.
pub fn run_p50(run: &Run) -> f64 {
    let mut sorted = run.latencies.clone();
    sorted.sort_unstable();
    quantile::quantile(&sorted, Level::P50, COVERAGE).map_or(f64::NAN, |q| q.estimate as f64)
}

/// `b`'s runs against `a`'s: the ratio of their medians, and whether it lies outside both runs'
/// noise bands (`hyper_measure::stats::band`, every halving's ratio of medians), the repository's
/// rule for a difference (`docs/benchmarks.md`, "The noise band"); none for fewer than four runs
/// a side.
pub fn against(a: &[f64], b: &[f64]) -> Option<(f64, bool)> {
    let (a_low, a_high) = hyper_measure::stats::band(a)?;
    let (b_low, b_high) = hyper_measure::stats::band(b)?;
    let ratio = median(&mut b.to_vec()) / median(&mut a.to_vec());
    let outside = |low: f64, high: f64| ratio < low || ratio > high;
    Some((ratio, outside(a_low, a_high) && outside(b_low, b_high)))
}

/// [`against`] as words for a table's note.
pub fn verdict(a: &[f64], b: &[f64]) -> String {
    match against(a, b) {
        Some((ratio, true)) => format!("×{ratio:.3}, outside both bands"),
        Some((ratio, false)) => format!("×{ratio:.3}, within the noise"),
        None => "fewer than four runs a side: no band".to_owned(),
    }
}

/// A comma-separated list from the command line, or `default`.
pub fn list<T>(arg: Option<&String>, default: &[T]) -> Vec<T>
where
    T: std::str::FromStr + Clone,
    T::Err: std::fmt::Debug,
{
    arg.map_or_else(
        || default.to_vec(),
        |s| s.split(',').map(|n| n.trim().parse().unwrap()).collect(),
    )
}

/// The measured entries a run takes by default: the run length a p99.9 needs for an upper end at
/// [`COVERAGE`] (`hyper_measure::quantile::samples_for`), shared among `rounds`.
pub fn default_entries(rounds: usize) -> u64 {
    quantile::samples_for(Level::P999, COVERAGE)
        .unwrap()
        .div_ceil(rounds as u64)
}
