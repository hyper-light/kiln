//! The persisted class under deterministic simulation (kiln's CP-T2.2 step 6,
//! `docs/design/persisted-sim.md`).
//!
//! Every node's host runs as it ships, on a shard of hyper-rt's simulation runtime, which gives it
//! virtual time and a simulated UDP fabric. Its members' logs sit on hyper-sim's block device under
//! hyper-log's `Waits::Told`: a log writes only when its host cuts it, at the virtual time the
//! node's disk draws for the write, and reads from its device only when its host serves it, at
//! once; the host waits on its shard for each cut and each serve to be answered. So every answer
//! reaches its member at a virtual instant the seed chose, and a run is the same every time it is
//! run from its seed.
//!
//! [`World`] is what `tests/sim.rs` drives: it starts and kills nodes (at an await of a host, or
//! before, during or after a cut of its disk), crashes their devices as a kill or a power cut
//! leaves them, cuts and mends the network between nodes, stalls disks, and publishes and reads
//! through each node.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use focal_memory::MemoryBudget;
use futures::channel::oneshot;
use hyper_block::buf::Alignment;
pub use hyper_block::sim::Crash;
use hyper_block::sim::{Fault, SimDevice, SimHandle};
use hyper_rt::shard::ShardId;
use hyper_rt::sim::SimRuntime;
pub use hyper_rt::sim::{SimFabricStats, SimLoss, SimPath};
use quiver_api::{Message, Offset, Refusal, Topic};

use super::{Command, PersistedConfig, PersistedStats, Shared, lock};
use crate::ephemeral::record_into;
use crate::node::Clock;

/// Bound: the handles one simulated device holds at once: a member's log, and its next one while a
/// fenced member opens again before the old one is dropped, with room to spare.
const HANDLES: usize = 4;
/// Shape: the simulated devices' block and sector, a 512e drive under 4 KiB I/O.
const BLOCK: usize = 4096;
const SECTOR: usize = 512;
/// Bound: the sockets a node's shard holds: a member's and a publisher's for the catalog and each
/// topic, with room for the ones a restart binds before the old ones close.
const SOCKETS_PER_SHARD: u16 = 64;

/// When a node's running start ends in a kill ([`World::start`], [`World::doom`]), counted in its
/// start's awaits and cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kill {
    /// It runs until the run ends.
    #[default]
    Never,
    /// At its host's `n`-th await of the start (from 0), before it looks at what came.
    AtAwait(u64),
    /// At the node's `cut`-th cut of the start (from 0): before it, so what wasn't cut is lost
    /// with the process; during it, power cut after `ops` of its writes and flushes; or once it is
    /// answered, before the members take its answers (the leader killed between its flush and its
    /// acknowledgement).
    AtCut { cut: u64, when: When },
}

/// A kill planned from now on ([`World::doom`]): after so many more awaits of the node's host, or
/// at so many more cuts of its disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doom {
    Awaits(u64),
    Cuts { after: u64, when: When },
}

/// How far a node's running start has come, and the kill planned for it.
#[derive(Debug, Default)]
struct Course {
    awaits: u64,
    cuts: u64,
    kill: Kill,
}

/// Where in a cut a node is killed ([`Kill::AtCut`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Before,
    During { ops: u64 },
    After,
}

/// What a start's kill is, as the host meets it.
pub(crate) enum Fate {
    Live,
    Die,
    /// Cut with power cut after these operations, then die.
    DieDuring(u64),
    /// Cut, then die before the answers are taken.
    DieAfter,
}

/// A node's machine across its starts: the simulated devices its members' logs are on, kept as a
/// kill or a power cut left them, and the windows its disk stalls in.
pub(crate) struct Machine {
    id: u64,
    seed: u64,
    data: PathBuf,
    devices: Mutex<BTreeMap<PathBuf, SimDevice>>,
    stalls: Mutex<Vec<(u64, u64)>>,
    course: Mutex<Course>,
}

impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machine")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Machine {
    /// A handle onto the device of the member whose data is `dir`, the device made empty the first
    /// time, seeded by the world's seed, the node and where the member is under the node's data.
    pub(crate) fn handle(&self, dir: &Path) -> Result<SimHandle, String> {
        let mut devices = lock(&self.devices);
        if !devices.contains_key(dir) {
            let place = dir.strip_prefix(&self.data).unwrap_or(dir);
            let seed = mix(
                mix(self.seed, self.id),
                fnv(place.to_string_lossy().as_bytes()),
            );
            let align = |n| Alignment::new(n).map_err(|e| format!("{e:?}"));
            let device =
                SimDevice::new(align(BLOCK)?, align(SECTOR)?, seed, HANDLES).map_err(said)?;
            devices.insert(dir.to_owned(), device);
        }
        devices
            .get(dir)
            .ok_or_else(|| "a simulated device".to_owned())?
            .handle()
            .map_err(said)
    }

    /// Power cut on every device after `ops` more operations (none at 0): nothing a dying node
    /// writes after it lands.
    fn arm(&self, ops: u64) {
        for device in lock(&self.devices).values() {
            let _ = device.inject(Fault::PowerCut { ops });
        }
    }

    /// Every device crashed as `crash` says and its faults cleared: the machine back on.
    fn crash(&self, crash: Crash) -> Result<(), String> {
        for device in lock(&self.devices).values() {
            device.crash(crash).map_err(said)?;
            device.clear_faults().map_err(said)?;
        }
        Ok(())
    }

    /// The end of the stall that `at_ns` falls in, if it falls in one.
    fn stalled_until(&self, at_ns: u64) -> Option<u64> {
        lock(&self.stalls)
            .iter()
            .filter(|(from, to)| *from <= at_ns && at_ns < *to)
            .map(|(_, to)| *to)
            .max()
    }

    /// The operations every device made so far: (writes, flushes), summed in the devices' order.
    fn operations(&self) -> (u64, u64) {
        lock(&self.devices)
            .values()
            .filter_map(|d| d.stats().ok())
            .fold((0, 0), |(w, s), stats| {
                (w.wrapping_add(stats.writes), s.wrapping_add(stats.syncs))
            })
    }
}

/// What one start of a node's host is given by the world.
pub(crate) struct Start {
    machine: Arc<Machine>,
    /// When the node's disk next takes what its logs hold.
    pub(crate) next_cut: Option<Instant>,
    seeds: u64,
    /// The disk's draws: SplitMix64 from the start's seed.
    draws: u64,
    seed: u64,
}

impl Start {
    pub(crate) fn machine(&self) -> Arc<Machine> {
        Arc::clone(&self.machine)
    }

    /// The seed of the next endpoint the start makes, in the order it makes them.
    pub(crate) fn endpoint_seed(&mut self) -> [u8; 32] {
        self.seeds = self.seeds.wrapping_add(1);
        let mut seed = [0; 32];
        let mut state = mix(self.seed, self.seeds);
        for chunk in seed.chunks_mut(8) {
            state = mix(state, 0x5eed);
            chunk.copy_from_slice(&state.to_le_bytes());
        }
        seed
    }

    /// An await of the host: whether the start is killed here.
    pub(crate) fn awaited(&mut self) -> bool {
        let mut course = lock(&self.machine.course);
        let at = course.awaits;
        course.awaits = course.awaits.wrapping_add(1);
        course.kill == Kill::AtAwait(at)
    }

    /// A cut of the node's disk: what the start's kill makes of it.
    pub(crate) fn cutting(&mut self) -> Fate {
        let mut course = lock(&self.machine.course);
        let at = course.cuts;
        course.cuts = course.cuts.wrapping_add(1);
        match course.kill {
            Kill::AtCut { cut, when } if cut == at => match when {
                When::Before => Fate::Die,
                When::During { ops } => Fate::DieDuring(ops),
                When::After => Fate::DieAfter,
            },
            _ => Fate::Live,
        }
    }

    /// The kill planned for the start.
    pub(crate) fn kill(&self) -> Kill {
        lock(&self.machine.course).kill
    }

    /// Power cut after `ops` more operations on every device of the node.
    pub(crate) fn arm(&self, ops: u64) {
        self.machine.arm(ops);
    }

    /// When the node's disk takes what its logs hold, for writes that came at `now` (`now_ns` on
    /// the virtual clock): its latency drawn, most a flush's, some a slow one's, a few a stall's,
    /// and past any stall the world set that it falls in.
    pub(crate) fn latency(&mut self, now_ns: u64) -> Duration {
        self.draws = mix(self.draws, 1);
        let draw = self.draws;
        let within = |lo: u64, hi: u64| {
            lo.saturating_add(draw.checked_rem(hi.saturating_sub(lo).max(1)).unwrap_or(0))
        };
        let ns = match draw.checked_rem(1000).unwrap_or(0) {
            0..=849 => within(50_000, 1_000_000),
            850..=989 => within(1_000_000, 10_000_000),
            _ => within(10_000_000, 120_000_000),
        };
        let stalled = self
            .machine
            .stalled_until(now_ns)
            .map_or(0, |until| until.saturating_sub(now_ns));
        Duration::from_nanos(ns.saturating_add(stalled))
    }
}

/// SplitMix64's step from `state` and `input`: the draws and seeds of the simulation.
const fn mix(state: u64, input: u64) -> u64 {
    let mut z = state
        .wrapping_add(input)
        .wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// FNV-1a of `bytes`.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn said(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

/// What a world is made of ([`World::new`]).
#[derive(Debug, Clone)]
pub struct Settings {
    /// What every draw of the world comes from: the fabric's, each endpoint's, each disk's.
    pub seed: u64,
    /// The cluster's nodes, numbered 1, 2, 3… each on a shard of its own.
    pub nodes: usize,
    /// The run's directory: each node's data under it, `node-<id>`.
    pub dir: PathBuf,
    /// The cluster's identities, made there the first time and kept: the same certificates make
    /// the same handshakes, so a run replays across processes that share them.
    pub identities: PathBuf,
    /// Every path between two nodes, until cut ([`World::cut`]).
    pub path: SimPath,
    /// How long a publish waits for a leader before it's refused for no quorum.
    pub quorum_wait: Duration,
}

/// A node of the world: its shard, its machine, its configuration and its current start.
struct Slot {
    shard: ShardId,
    machine: Arc<Machine>,
    config: PersistedConfig,
    shared: Option<Arc<Shared>>,
    starts: u64,
}

/// An answer the world waits for: a publish's offset, a create's word.
pub struct Answer<T>(oneshot::Receiver<Result<T, Refusal>>);

impl<T> Answer<T> {
    /// The answer, once it has come; refused as the host ended where it ended first.
    pub fn take(&mut self) -> Option<Result<T, Refusal>> {
        match self.0.try_recv() {
            Ok(Some(answer)) => Some(answer),
            Ok(None) => None,
            Err(_) => Some(Err(Refusal::Unavailable(
                "the node's host ended".to_owned(),
            ))),
        }
    }
}

/// The world of a simulated cluster ([`Settings`]): its runtime, its nodes and their machines.
pub struct World {
    rt: SimRuntime,
    nodes: Vec<Slot>,
    settings: Settings,
}

impl World {
    /// A world of `settings.nodes` nodes, none started yet.
    pub fn new(settings: Settings) -> Result<Self, String> {
        let shards = u16::try_from(settings.nodes).map_err(said)?;
        let runtime = hyper_rt::runtime::RuntimeConfig {
            shards,
            ..super::runtime()
        };
        let sim = hyper_rt::sim::SimConfig {
            sockets_per_shard: SOCKETS_PER_SHARD,
        };
        let mut rt = SimRuntime::with(&runtime, settings.seed, sim).map_err(said)?;
        rt.set_path(settings.path);
        let addresses: Vec<(u64, SocketAddr)> = (1..=settings.nodes)
            .map(|n| {
                let id = u64::try_from(n).map_err(said)?;
                Ok((id, SocketAddr::from(([127, 0, 0, 1], first_port(n)?))))
            })
            .collect::<Result<_, String>>()?;
        let shard_ids = rt.shard_ids();
        let mut nodes = Vec::new();
        for ((id, _), shard) in addresses.iter().zip(shard_ids) {
            let data = settings.dir.join(format!("node-{id}"));
            let config = PersistedConfig {
                id: *id,
                nodes: addresses.clone(),
                data: data.clone(),
                identities: settings.identities.clone(),
                // Kept beside the identities, as they are kept across runs.
                identities_key: Some(settings.identities.with_extension("key")),
                part_memory: PersistedConfig::PART_MEMORY_MIN,
                topics: 4,
                publishes: 4096,
                quorum_wait: settings.quorum_wait,
            };
            nodes.push(Slot {
                shard,
                machine: Arc::new(Machine {
                    id: *id,
                    seed: settings.seed,
                    data,
                    devices: Mutex::new(BTreeMap::new()),
                    stalls: Mutex::new(Vec::new()),
                    course: Mutex::new(Course::default()),
                }),
                config,
                shared: None,
                starts: 0,
            });
        }
        Ok(Self {
            rt,
            nodes,
            settings,
        })
    }

    fn slot(&self, node: u64) -> Result<&Slot, String> {
        let at = usize::try_from(node.checked_sub(1).ok_or("node 0")?).map_err(said)?;
        self.nodes.get(at).ok_or_else(|| format!("no node {node}"))
    }

    fn slot_mut(&mut self, node: u64) -> Result<&mut Slot, String> {
        let at = usize::try_from(node.checked_sub(1).ok_or("node 0")?).map_err(said)?;
        self.nodes
            .get_mut(at)
            .ok_or_else(|| format!("no node {node}"))
    }

    /// Starts `node`'s host on its shard, from what its machine and its files kept, to end as
    /// `kill` says; refused while its last start runs.
    pub fn start(&mut self, node: u64, kill: Kill) -> Result<(), String> {
        let seed = self.settings.seed;
        let slot = self.slot_mut(node)?;
        if slot.shared.as_ref().is_some_and(|s| !s.ended()) {
            return Err(format!("node {node} is running"));
        }
        slot.starts = slot.starts.wrapping_add(1);
        let start_seed = mix(mix(seed, node), slot.starts);
        *lock(&slot.machine.course) = Course {
            awaits: 0,
            cuts: 0,
            kill,
        };
        let start = Start {
            machine: Arc::clone(&slot.machine),
            next_cut: None,
            seeds: 0,
            draws: start_seed,
            seed: start_seed,
        };
        let shared = Arc::new(Shared::new(&slot.config));
        slot.shared = Some(Arc::clone(&shared));
        let (config, shard) = (slot.config.clone(), slot.shard);
        let clock: Clock = Arc::new(|| hyper_rt::futures::now_ns() / 1_000);
        let budget = MemoryBudget::new(64 << 20, 0).map_err(said)?;
        self.rt
            .spawn_on(shard, async move {
                let ended =
                    super::host_body(config, &shared, |_| {}, clock, budget, Some(start)).await;
                shared.end(ended.unwrap_or_else(|why| why));
            })
            .map_err(said)?;
        Ok(())
    }

    /// Plans `node`'s running start's kill from now: after so many more awaits, or at a cut so
    /// many cuts on.
    pub fn doom(&self, node: u64, doom: Doom) -> Result<(), String> {
        let mut course = lock(&self.slot(node)?.machine.course);
        course.kill = match doom {
            Doom::Awaits(n) => Kill::AtAwait(course.awaits.saturating_add(n)),
            Doom::Cuts { after, when } => Kill::AtCut {
                cut: course.cuts.saturating_add(after),
                when,
            },
        };
        Ok(())
    }

    /// The awaits and cuts `node`'s running start has made.
    pub fn course(&self, node: u64) -> Result<(u64, u64), String> {
        let course = lock(&self.slot(node)?.machine.course);
        Ok((course.awaits, course.cuts))
    }

    /// Whether `node`'s host runs.
    #[must_use]
    pub fn alive(&self, node: u64) -> bool {
        self.slot(node)
            .ok()
            .and_then(|s| s.shared.as_ref())
            .is_some_and(|s| !s.ended())
    }

    /// Why `node`'s last start ended, if it did.
    #[must_use]
    pub fn ended(&self, node: u64) -> Option<String> {
        self.slot(node)
            .ok()
            .and_then(|s| s.shared.as_ref())
            .and_then(|s| lock(&s.stats).ended.clone())
    }

    /// `node`'s devices crashed as `crash` says once its host has ended: `Crash::KeepAll` as a
    /// kill -9 leaves them (the OS kept what was written), `Random` or `LoseAll` as a power cut.
    pub fn crash(&mut self, node: u64, crash: Crash) -> Result<(), String> {
        if self.alive(node) {
            return Err(format!("node {node} is running"));
        }
        self.slot(node)?.machine.crash(crash)
    }

    /// Runs the world to `at_ns` on its virtual clock.
    pub fn run_until(&mut self, at_ns: u64) {
        self.rt.run_until(at_ns);
    }

    /// The virtual clock.
    #[must_use]
    pub fn now_ns(&self) -> u64 {
        self.rt.now_ns()
    }

    fn send<T>(
        &self,
        node: u64,
        command: impl FnOnce(oneshot::Sender<Result<T, Refusal>>) -> Command,
    ) -> Answer<T> {
        let (reply, answer) = oneshot::channel();
        let command = command(reply);
        if let Some(shared) = self.slot(node).ok().and_then(|s| s.shared.as_ref()) {
            // A refused send drops its reply, which the answer reads as the host gone.
            let _ = shared.send(command);
        }
        Answer(answer)
    }

    /// Creates the persisted topic `name` through `node`.
    pub fn create(&self, node: u64, name: &str, topic: Topic) -> Answer<()> {
        let name = name.to_owned();
        self.send(node, |reply| Command::Create { name, topic, reply })
    }

    /// Publishes `payload` to `name` through `node`, stamped with the virtual clock.
    pub fn publish(&self, node: u64, name: &str, payload: &[u8]) -> Answer<Offset> {
        let mut record = Vec::new();
        record_into(&mut record, None, payload);
        let name = name.to_owned();
        let at_us = self.rt.now_ns() / 1_000;
        self.send(node, |reply| Command::Publish {
            name,
            record,
            at_us,
            reply,
        })
    }

    /// Whether `node` holds the topic `name`: its catalog has applied the topic's create.
    #[must_use]
    pub fn holds(&self, node: u64, name: &str) -> bool {
        self.slot(node)
            .ok()
            .and_then(|s| s.shared.as_ref())
            .is_some_and(|shared| {
                shared
                    .views
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains_key(name)
            })
    }

    /// What `node`'s view of `name` holds, from the first it keeps; none where it holds no view.
    pub fn read(&self, node: u64, name: &str) -> Option<Vec<Message>> {
        let shared = self.slot(node).ok()?.shared.as_ref()?;
        let view = shared
            .views
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()?;
        let (first, end) = view.bounds();
        let mut messages = Vec::new();
        let mut at = first;
        while at < end {
            let batch = view.read(at, 1024).ok()?;
            if batch.messages.is_empty() {
                break;
            }
            at = batch.next;
            messages.extend(batch.messages);
        }
        Some(messages)
    }

    /// Everything `node`'s members and publishers say of themselves, as its host's next turn finds
    /// them: asked for, the world run a millisecond on, and read.
    pub fn diagnose(&mut self, node: u64) -> Option<String> {
        let shared = Arc::clone(self.slot(node).ok()?.shared.as_ref()?);
        *lock(&shared.diagnosis) = Some(String::new());
        shared.wake();
        let next = self.rt.now_ns().saturating_add(1_000_000);
        self.rt.run_until(next);
        lock(&shared.diagnosis).take().filter(|s| !s.is_empty())
    }

    /// `node`'s figures as its host last set them.
    #[must_use]
    pub fn stats(&self, node: u64) -> Option<PersistedStats> {
        let shared = self.slot(node).ok()?.shared.as_ref()?;
        Some(lock(&shared.stats).clone())
    }

    /// The network between `a` and `b` cut, both ways or from `a` to `b` alone.
    pub fn cut(&mut self, a: u64, b: u64, both: bool) -> Result<(), String> {
        let lost = self.settings.path.with_loss(SimLoss::random(1_000_000));
        self.paths(a, b, lost)?;
        if both {
            self.paths(b, a, lost)?;
        }
        Ok(())
    }

    /// The network between `a` and `b` as it was, both ways.
    pub fn mend(&mut self, a: u64, b: u64) -> Result<(), String> {
        let path = self.settings.path;
        self.paths(a, b, path)?;
        self.paths(b, a, path)
    }

    /// Every port of `from`'s shard to every port of `to`'s on `path`.
    fn paths(&mut self, from: u64, to: u64, path: SimPath) -> Result<(), String> {
        let from = ports(usize::try_from(from).map_err(said)?)?;
        let to = ports(usize::try_from(to).map_err(said)?)?;
        for f in from.clone() {
            for t in to.clone() {
                self.rt.set_pair_path(f, t, path);
            }
        }
        Ok(())
    }

    /// `node`'s disk stalled from `from_ns` to `to_ns`: a write that comes in between is taken
    /// once it ends.
    pub fn stall(&self, node: u64, from_ns: u64, to_ns: u64) -> Result<(), String> {
        lock(&self.slot(node)?.machine.stalls).push((from_ns, to_ns));
        Ok(())
    }

    /// The writes and flushes `node`'s devices made so far.
    pub fn operations(&self, node: u64) -> Result<(u64, u64), String> {
        Ok(self.slot(node)?.machine.operations())
    }

    /// The fabric's counts: datagrams sent, delivered and dropped.
    #[must_use]
    pub fn fabric(&self) -> SimFabricStats {
        self.rt.stats()
    }

    /// Stops every running host, as the run ends: each ends at its next wake.
    pub fn stop(&mut self) {
        for slot in &self.nodes {
            if let Some(shared) = &slot.shared {
                shared.stop();
            }
        }
        let end = self.rt.now_ns().saturating_add(1_000_000_000);
        self.rt.run_until(end);
    }
}

/// The first port of node `n`'s shard (numbered from 1): where its catalog's member listens.
fn first_port(n: usize) -> Result<u16, String> {
    let index = n.checked_sub(1).ok_or("node 0")?;
    u16::try_from(
        index
            .saturating_mul(usize::from(SOCKETS_PER_SHARD))
            .saturating_add(1),
    )
    .map_err(said)
}

/// Every port of node `n`'s shard.
fn ports(n: usize) -> Result<std::ops::Range<u16>, String> {
    let first = first_port(n)?;
    Ok(first..first.saturating_add(SOCKETS_PER_SHARD))
}
