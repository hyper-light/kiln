//! The sync model (CP-T6.1): one synced repo, its copies on two or three machines and the
//! server's member, run on kiln's own sync engine and driven as kiln drives it:
//!
//! - each copy is a hyper-durable `Replica` of kiln's `Feed` (its `Copy` applying what the log
//!   commits, proposing what changed), its folder, contents and saved state on a kiln-sim disk,
//!   its group log kept as a runner's is (across its crashes);
//! - its runner's shell is `sync::runner`'s: each drive pumps the replica, takes what's committed
//!   into the folder, fetches what that lacks, looks at the folder every 2 s, sends the contents
//!   its unsent steps name and then the steps (again every 15 s), retrying a transfer that failed
//!   after 10 s;
//! - the server's member is a `Replica` of kiln's `Index`, the sole voter, taking each machine's
//!   steps through `sync::server::take_steps` (in order, once, once their contents are there).
//!
//! People edit every folder all the while (files written whole and saved durably, deleted, made
//! executable, linked). The nemesis, until it stops: frames lost and cut off (the sync lane keeps
//! its order, as a QUIC stream does; transfers are requests of their own);
//! runners crashing, with and without the power going (what kiln didn't fsync torn, zeroed, lost
//! or kept); a crash at a chosen write of kiln's; the server crashing; fsync and write errors; a
//! full disk.
//!
//! The moves model ([`Mode::Moves`]) runs the same, with an agent session working in the repo on
//! one machine at a time and moving between them as `harness::session` moves one: on the machine
//! it leaves, kiln's look-now and `left_at` (its last changes in the log); on the one it goes to,
//! `arrived` (the folder has taken the log that far). It checks, at each move, that the session
//! leaves no write of its own behind and goes on with every one of them in its folder.
//!
//! The checker, once the nemesis has stopped and everything settled:
//! - nothing the log took holds contents nobody wrote (torn, zeroed or lost bytes never spread);
//! - every copy converged: its folder holds what the log made the repo, its state agrees, and it
//!   has nothing left unsent or untaken;
//! - each person's last write to a path that nobody touched after it is in the repo, at its name
//!   or beside it as a conflict copy.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use hyper_durable::{LogStore as _, Output, RamStore, Replica, Unbounded};
use hyper_raft::proto::{Message, MessageType};
use kiln_seams::clock::{SIM_EPOCH_MS, SimClock};
use kiln_sim::disk::{Disk, DiskCounts, DiskFaults};
use kiln_sim::net::{Faults, Net, NetCounts, Node};
use kiln_sim::{Queue, Rng, Trace};

use super::fs::SimFs;
use crate::sync::blobs::Blobs;
use crate::sync::fs::Fs;
use crate::sync::group::{Feed, Index, SERVER, TICK, learner, pump, settings};
use crate::sync::runner::{Looked, Mark, Standing, arrived, left_at, look_now_at, stand, to_send};
use crate::sync::scan::{Limits, TEMP_PREFIX};
use crate::sync::server::{Taking, take_steps};
use crate::sync::{Decision, Step, Version};

/// How often a runner looks at its folders.
const LOOK: Duration = Duration::from_secs(2);
/// How often a runner sends its unsent steps again.
const RESEND: Duration = Duration::from_secs(15);
/// How long after a failed transfer a runner tries it again.
const RETRY: Duration = Duration::from_secs(10);
/// How long a runner waits for a transfer's answer before it counts it failed.
const TRANSFER: Duration = Duration::from_secs(30);
/// Transfers a runner has under way at once.
const TRANSFERS: usize = 6;
/// How long the model runs after the nemesis stops, for everything to settle.
const SETTLE: Duration = Duration::from_secs(90);
/// Most events one seed runs: past them, the run fails (something woke itself forever).
const EVENTS_MAX: u64 = 2_000_000;

/// The paths people edit, and whether each is a script (made executable).
const PATHS: [(&str, bool); 7] =
    [("a.txt", false), ("b.md", false), ("src/lib.rs", false), ("src/util/x.rs", false), ("docs/guide/intro.md", false), ("run.sh", true), ("data.bin", false)];
/// The link people make, and what it names.
const LINK: (&str, &str) = ("latest", "a.txt");
/// The paths the moving agent session writes (nobody else does), and how long each always is
/// (none: any length): a write of the same length within the filesystem's time step is the
/// change a look by size and time can't see.
const SESSION_PATHS: [(&str, Option<usize>); 3] = [("agent/notes.md", Some(120)), ("agent/src/main.rs", None), ("agent/data.bin", Some(5000))];
/// How often a session leaving asks whether its folder's last changes are in the log
/// (`harness::session`'s `FILES_EVERY`), and how often one arriving asks. kiln gives up leaving
/// after its `FILES_OUT_FOR`, 600 s: no run lasts that long, so a move still leaving when a run
/// ends fails the check.
const FILES_EVERY: Duration = Duration::from_millis(250);
const FILES_IN_EVERY: Duration = Duration::from_secs(1);
/// The synced repo's id, as marks and standings name it.
const REPO: &str = "repo";
/// The lane sync's messages ride on kiln's links.
const LANE_SYNC: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Who {
    Server,
    M(usize),
}

#[derive(Debug, Clone)]
enum Frame {
    Raft(Box<Message>),
    /// A runner's steps.
    Steps(Vec<Step>),
    /// The server asks again for contents steps named.
    Want(Vec<String>),
    /// A runner sends contents (`PUT`), and the server answers.
    Put { hash: String, bytes: Vec<u8>, attempt: u64 },
    Putted { hash: String, ok: bool, attempt: u64 },
    /// A runner fetches contents (`GET`), and the server answers (none: it doesn't have them).
    Get { hash: String, attempt: u64 },
    Got { hash: String, bytes: Option<Vec<u8>>, attempt: u64 },
}

#[derive(Debug, Clone)]
enum Ev {
    /// Every member's tick, then a drive of each.
    Tick,
    Arrive { to: Who, from: Who, frame: Frame },
    Edit(usize),
    Crash { m: usize, power: bool },
    Up(usize),
    Freeze { m: usize, after: u64 },
    ServerCrash,
    ServerUp,
    /// The nemesis stops: a crash chosen for a write still to come is called off.
    Calm,
    Cut { m: usize, span: Duration },
    Full(usize),
    Room(usize),
    PutLate { m: usize, hash: String, attempt: u64 },
    GetLate { m: usize, hash: String, attempt: u64 },
    /// The moving session writes a file where it is; sets off for another machine; asks how its
    /// copy stands (leaving or arriving); and its package arrives where it goes.
    SessionEdit,
    SessionMove,
    SessionPoll,
    SessionArrive,
}

/// Which of kiln's parts a run drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Sync alone: people editing every copy.
    Sync,
    /// An agent session working in the repo and moving between its machines, too.
    Moves,
}

/// Where the moving session is in a move.
#[derive(Debug, Clone)]
enum Leg {
    Working,
    Leaving { to: usize, since: Duration },
    Moving { to: usize, mark: Mark },
    Arriving { mark: Mark },
}

/// The agent session the moves model moves.
struct Session {
    /// The machine it's on (or leaves).
    at: usize,
    leg: Leg,
    /// Its last write to each path: when, and what.
    wrote: BTreeMap<String, (Duration, Vec<u8>)>,
    edits: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Upload {
    Moving(u64),
    Done,
    Failed(Duration),
}

/// A runner's process, holding its copy: what a crash loses.
struct Runner {
    r: Replica<RamStore, Feed>,
    out: Output<()>,
    uploads: BTreeMap<String, Upload>,
    /// The newest step sent.
    sent: u64,
    fetching: BTreeMap<String, u64>,
    retry: BTreeMap<String, Duration>,
    gone: BTreeSet<String>,
    looked: Duration,
    resent: Duration,
    attempts: u64,
}

struct Machine {
    name: String,
    member: u64,
    disk: Disk,
    fs: Arc<dyn Fs>,
    root: PathBuf,
    state: PathBuf,
    blobs_dir: PathBuf,
    /// The group's log on this machine's disk: kept across its runner's crashes.
    log: RamStore,
    runner: Option<Runner>,
    /// Its runner's contents store, as the runner holds it (once, its clones sharing it).
    blobs: Option<Blobs>,
    /// The person's last act on each path: when, and what it wrote (none: deleted).
    last: BTreeMap<String, (Duration, Option<Vec<u8>>)>,
    edits: u64,
}

struct Server {
    r: Option<Replica<RamStore, Index>>,
    /// How it last stood, as the trace said it: leader, term, applied, learners.
    said: String,
    log: RamStore,
    out: Output<()>,
    blobs: BTreeMap<String, Vec<u8>>,
    next: HashMap<u64, u64>,
}

/// What a run of the model counted: across seeds, a fault that never happened tested nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub edits: u64,
    pub steps_committed: u64,
    pub conflicts: u64,
    pub crashes: u64,
    pub power_losses: u64,
    pub frozen: u64,
    pub server_crashes: u64,
    pub full: u64,
    pub fetched: u64,
    /// Steps the server got again that it had taken already (a runner's resends, after a lost
    /// answer or a reconnect).
    pub again: u64,
    /// Moves made, and moves that waited for their changes to reach the log or the folder.
    pub moves: u64,
    pub moves_waited: u64,
    pub net: NetCounts,
    pub disk: DiskCounts,
}

impl Counts {
    pub fn add(&mut self, o: &Self) {
        self.edits += o.edits;
        self.steps_committed += o.steps_committed;
        self.conflicts += o.conflicts;
        self.crashes += o.crashes;
        self.power_losses += o.power_losses;
        self.frozen += o.frozen;
        self.server_crashes += o.server_crashes;
        self.full += o.full;
        self.fetched += o.fetched;
        self.again += o.again;
        self.moves += o.moves;
        self.moves_waited += o.moves_waited;
        self.net.add(&o.net);
        self.disk.add(&o.disk);
    }
}

/// What the model is asked to run besides its seed: the bugs planted in a test build (none for
/// kiln's own), and how it runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct Plant {
    /// What sync writes into a folder renamed in with its bytes unsynced, its folder never synced
    /// (`Copy::planted_unsynced`, T-T6.1.1's planted bug): a power loss tears, zeroes or loses it.
    pub writes_unsynced: bool,
    /// Each contents request sent once, never tried again by its client, as the model ran when the
    /// first bugs were found: their seeds replay the runs that found them. Else a seed draws whether
    /// its requests may be sent twice, and overtake one another.
    pub requests_once: bool,
    /// Disks' names as the model kept them then too (`DiskFaults::names_in_order`), and kiln's
    /// state folder made by kiln, not there before it ran. Else a disk keeps POSIX's names (or one
    /// journal's), and the machine had kiln's state folder already.
    pub names_in_order: bool,
}

struct Sim {
    clock: SimClock,
    rng: Rng,
    trace: Trace,
    queue: Queue<Ev>,
    net: Net,
    machines: Vec<Machine>,
    server: Server,
    until: Duration,
    end: Duration,
    /// Every content a person wrote, by hash, and every link target.
    written: BTreeSet<String>,
    /// Each committed step's changes that hold contents nobody wrote: what spread.
    invented: Vec<String>,
    counts: Counts,
    plant: Plant,
    /// What the server's member took, by log index (taken again as its log replays: the same).
    history: BTreeMap<u64, (Step, Vec<Decision>)>,
    session: Option<Session>,
    /// Contents requests may be sent twice (a client trying again) and overtake one another: on a
    /// network of their own, so the rest of a seed's draws are as they were without it.
    retried: Option<Net>,
    /// An event the queue refused, full: the run fails at once.
    overflowed: bool,
}

fn node(w: Who) -> Node {
    match w {
        Who::Server => Node(0),
        Who::M(i) => Node(u32::try_from(i).unwrap_or(u32::MAX).saturating_add(1)),
    }
}

fn ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn e(err: impl std::fmt::Debug) -> String {
    format!("{err:?}")
}

/// What a member's log holds once its process has gone: everything written (a `RamStore` makes
/// each write durable as it's submitted), without the answer to a write still out, which was the
/// gone process's to take.
fn after_crash(log: &RamStore) -> RamStore {
    let mut kept = log.clone();
    for _ in 0..16 {
        if kept.poll().is_none() {
            break;
        }
    }
    kept
}

impl Sim {
    fn new(seed: u64, keep: usize, plant: Plant, mode: Mode) -> Result<Self, String> {
        let mut rng = Rng::new(seed);
        let clock = SimClock::new();
        let until = rng.span(Duration::from_secs(20), Duration::from_secs(60));
        let mut netr = rng.stream("net");
        let faults = Faults::draw(&mut netr, Duration::from_millis(5), Duration::from_millis(400), until);
        let net = Net::new(faults, netr);
        let mut retries = rng.stream("retries");
        let retried = (!plant.requests_once && retries.chance(500)).then(|| Net::new(faults, retries));
        let n = usize::try_from(rng.range(2, 3)).unwrap_or(2);
        let mut machines = Vec::new();
        for i in 0..n {
            let mut dr = rng.stream(&format!("disk{i}"));
            let df = DiskFaults { names_in_order: plant.names_in_order, ..DiskFaults::draw(&mut dr, until) };
            let disk = Disk::new(clock.clone(), dr, df);
            let fs: Arc<dyn Fs> = Arc::new(SimFs::new(disk.clone()));
            let name = format!("m{}", i.saturating_add(2));
            let root = PathBuf::from(format!("/home/{name}/repo"));
            let (state, blobs_dir) = (PathBuf::from(format!("/state/{name}/copy.json")), PathBuf::from(format!("/state/{name}/blobs")));
            // The machine's folders before kiln runs: the repo's, and kiln's own.
            fs.create_dir_all(&root).map_err(e)?;
            disk.make_durable(&root.to_string_lossy());
            if !plant.names_in_order {
                let kiln = format!("/state/{name}");
                fs.create_dir_all(&PathBuf::from(&kiln)).map_err(e)?;
                disk.make_durable(&kiln);
            }
            machines.push(Machine {
                name,
                member: u64::try_from(i).unwrap_or(0).saturating_add(2),
                disk,
                fs,
                root,
                state,
                blobs_dir,
                log: RamStore::new(),
                runner: None,
                blobs: None,
                last: BTreeMap::new(),
                edits: 0,
            });
        }
        let mut leader = Replica::open(&settings(SERVER, 1).map_err(e)?, RamStore::new(), Index::open(None).map_err(e)?, Unbounded).map_err(e)?;
        leader.campaign().map_err(e)?;
        let server = Server { r: Some(leader), said: String::new(), log: RamStore::new(), out: Output::default(), blobs: BTreeMap::new(), next: HashMap::new() };
        let mut sim = Self {
            clock,
            trace: Trace::new(seed, keep),
            queue: Queue::new(),
            net,
            machines,
            server,
            until,
            end: until.saturating_add(SETTLE),
            written: BTreeSet::new(),
            invented: Vec::new(),
            counts: Counts::default(),
            plant,
            history: BTreeMap::new(),
            session: (mode == Mode::Moves).then(|| Session { at: 0, leg: Leg::Working, wrote: BTreeMap::new(), edits: 0 }),
            retried,
            overflowed: false,
            rng,
        };
        // Some folders hold files before they sync; then every copy starts.
        for i in 0..n {
            if sim.rng.chance(500) {
                for _ in 0..sim.rng.range(1, 3) {
                    sim.edit(i);
                }
            }
            sim.start(i)?;
        }
        sim.plan();
        Ok(sim)
    }

    fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    fn unix_ms(&self) -> i64 {
        SIM_EPOCH_MS.saturating_add(i64::try_from(self.now().as_millis()).unwrap_or(i64::MAX))
    }

    fn at(&mut self, at: Duration, ev: Ev) {
        if !self.queue.push(at, ev) {
            self.overflowed = true;
        }
    }

    fn after(&mut self, d: Duration, ev: Ev) {
        let at = self.now().saturating_add(d);
        self.at(at, ev);
    }

    /// The nemesis's moments and the people's edits, while the nemesis lasts.
    fn plan(&mut self) {
        let until = self.until;
        let n = self.machines.len();
        self.at(TICK, Ev::Tick);
        self.at(until, Ev::Calm);
        for i in 0..n {
            for _ in 0..self.rng.range(4, 20) {
                let t = self.rng.span(Duration::ZERO, until);
                self.at(t, Ev::Edit(i));
            }
        }
        for _ in 0..self.rng.range(0, 3) {
            let (m, t, power) = (self.rng.index(n), self.rng.span(Duration::ZERO, until), self.rng.chance(500));
            self.at(t, Ev::Crash { m, power });
        }
        for _ in 0..self.rng.range(0, 2) {
            let (m, t, after) = (self.rng.index(n), self.rng.span(Duration::ZERO, until), self.rng.range(0, 60));
            self.at(t, Ev::Freeze { m, after });
        }
        for _ in 0..self.rng.range(0, 1) {
            let t = self.rng.span(Duration::ZERO, until);
            self.at(t, Ev::ServerCrash);
        }
        for _ in 0..self.rng.range(0, 3) {
            let (m, t, span) = (self.rng.index(n), self.rng.span(Duration::ZERO, until), self.rng.span(Duration::from_secs(1), Duration::from_secs(20)));
            self.at(t, Ev::Cut { m, span });
        }
        for _ in 0..self.rng.range(0, 1) {
            let (m, t) = (self.rng.index(n), self.rng.span(Duration::ZERO, until));
            let back = t.saturating_add(self.rng.span(Duration::from_secs(1), Duration::from_secs(15))).min(until);
            self.at(t, Ev::Full(m));
            self.at(back, Ev::Room(m));
        }
        if self.session.is_some() {
            for _ in 0..self.rng.range(5, 30) {
                let t = self.rng.span(Duration::ZERO, until);
                self.at(t, Ev::SessionEdit);
            }
            for _ in 0..self.rng.range(1, 4) {
                let t = self.rng.span(Duration::ZERO, until);
                self.at(t, Ev::SessionMove);
            }
        }
    }

    // ------------------------------------------------------------------------------ the people

    /// A person edits machine `m`'s folder: writes a file whole (saved durably), deletes one,
    /// changes a script's exec bit, or makes the link.
    fn edit(&mut self, m: usize) {
        let roll = self.rng.below(100);
        let (path, script) = self.rng.pick(&PATHS).copied().unwrap_or(("a.txt", false));
        // Lengths that repeat, as rewrites of a file often keep its size, and any.
        let len = match self.rng.below(3) {
            0 => usize::try_from(self.rng.range(0, 9000)).unwrap_or(0),
            _ => self.rng.pick(&[64usize, 4096, 5000]).copied().unwrap_or(64),
        };
        let now = self.now();
        let Some(mc) = self.machines.get_mut(m) else { return };
        let full = mc.root.join(path);
        let full_s = full.to_string_lossy().into_owned();
        mc.edits = mc.edits.saturating_add(1);
        let did: Option<Option<Vec<u8>>> = match roll {
            0..15 => {
                // A delete, of a file that's there.
                if mc.disk.is_file(&full_s) && mc.disk.remove_file(&full_s).is_ok() {
                    mc.disk.make_durable(&full_s);
                    Some(None)
                } else {
                    None
                }
            }
            15..20 => {
                // The link, once: what it names is a person's too.
                let (link, target) = LINK;
                let at = mc.root.join(link).to_string_lossy().into_owned();
                if !mc.disk.exists(&at) && mc.disk.symlink(target, &at).is_ok() {
                    mc.disk.make_durable(&at);
                    self.written.insert(format!("->{target}"));
                }
                None
            }
            _ => {
                let bytes = filled(&format!("{} edit #{} of {path}\n", mc.name, mc.edits), len);
                let made = full.parent().map(|p| mc.disk.create_dir_all(&p.to_string_lossy())).unwrap_or(Ok(()));
                match made.and_then(|()| mc.disk.write(&full_s, &bytes)) {
                    Ok(()) => {
                        // Its bytes are there, whatever comes of its exec bit.
                        self.written.insert(kiln_seams::sha256_hex(&bytes));
                        if script {
                            let _ = mc.disk.set_exec(&full_s, true);
                        }
                        mc.disk.make_durable(&full_s);
                        Some(Some(bytes))
                    }
                    Err(_) => None,
                }
            }
        };
        if let Some(what) = did {
            self.trace.fold(u64::try_from(m).unwrap_or(0) ^ kiln_sim::rng::fnv(path.as_bytes()));
            self.trace.note(|| format!("{:?} {} edits {path}: {}", now, mc.name, what.as_ref().map_or("deleted".to_owned(), |b| format!("{} bytes", b.len()))));
            mc.last.insert(path.to_owned(), (now, what));
            self.counts.edits += 1;
        }
    }

    // ------------------------------------------------------------------------------ the session

    /// The session writes one of its files where it works, whole (an agent's edit).
    fn session_edit(&mut self) {
        let now = self.now();
        let (path, fixed) = self.rng.pick(&SESSION_PATHS).copied().unwrap_or(("agent/notes.md", None));
        let len = fixed.unwrap_or_else(|| usize::try_from(self.rng.range(0, 9000)).unwrap_or(0));
        let Some(se) = self.session.as_mut() else { return };
        if !matches!(se.leg, Leg::Working) {
            return;
        }
        se.edits = se.edits.saturating_add(1);
        let bytes = filled(&format!("the session's edit #{} of {path}\n", se.edits), len);
        let Some(mc) = self.machines.get(se.at) else { return };
        let full = mc.root.join(path);
        let full_s = full.to_string_lossy().into_owned();
        let made = full.parent().map(|p| mc.disk.create_dir_all(&p.to_string_lossy())).unwrap_or(Ok(()));
        if made.and_then(|()| mc.disk.write(&full_s, &bytes)).is_ok() {
            mc.disk.make_durable(&full_s);
            self.written.insert(kiln_seams::sha256_hex(&bytes));
            self.trace.note(|| format!("{now:?} the session on {} writes {path}: {} bytes", mc.name, bytes.len()));
            se.wrote.insert(path.to_owned(), (now, bytes));
        }
    }

    /// The session sets off for another machine once its turn is over.
    fn session_move(&mut self) {
        let now = self.now();
        let n = self.machines.len();
        let pick = self.rng.index(n.saturating_sub(1));
        let Some(se) = self.session.as_mut() else { return };
        if !matches!(se.leg, Leg::Working) || n < 2 {
            return;
        }
        let to = if pick >= se.at { pick.saturating_add(1) } else { pick };
        se.leg = Leg::Leaving { to, since: now };
        self.at(now, Ev::SessionPoll);
    }

    /// The session asks how the copy it leaves, or arrives at, stands, as `harness::session`'s
    /// `files_out` and `files_in` do.
    fn session_poll(&mut self) -> Result<(), String> {
        let now = self.now();
        let at_ms = self.unix_ms();
        let Some((at, leg)) = self.session.as_ref().map(|se| (se.at, se.leg.clone())) else { return Ok(()) };
        match leg {
            Leg::Leaving { to, since } => {
                let ready = !self.net.is_cut(now, node(Who::Server), node(Who::M(at)));
                let Some(blobs) = self.blobs(at) else {
                    // Its runner is down: it asks again.
                    self.after(FILES_EVERY, Ev::SessionPoll);
                    return Ok(());
                };
                let mut mark = None;
                if let Some(rn) = self.machines.get_mut(at).and_then(|mc| mc.runner.as_mut()) {
                    let feed = rn.r.machine_mut();
                    let mut looked = Looked { repo: REPO.to_owned(), ..Looked::default() };
                    if let Some(Ok((proposed, _))) = look_now_at(feed, &blobs, ready, at_ms) {
                        looked.looked = true;
                        looked.proposed = proposed;
                    }
                    let st = Standing { repo: REPO.to_owned(), ..stand(feed) };
                    mark = left_at(&looked, &st);
                    if self.trace.keeps() {
                        let short = |v: Option<&Version>| match v {
                            Some(Version::File { hash, .. }) => hash.get(..8).unwrap_or_default().to_owned(),
                            Some(Version::Link { target }) => format!("->{target}"),
                            None => "-".to_owned(),
                        };
                        let state = &feed.copy.state;
                        let files: Vec<String> = SESSION_PATHS
                            .iter()
                            .map(|(p, _)| {
                                let here = feed.copy.fs.meta(&feed.copy.root.join(p)).ok().and_then(|_| crate::sync::blobs::hash_file_on(feed.copy.fs.as_ref(), &feed.copy.root.join(p)).ok());
                                let seen = state.seen.get(*p).map(|e| format!("{}{}", short(Some(&e.version)), if e.racy { " racy" } else { "" }));
                                format!(
                                    "{p}: here {} synced {} pending {} tree {} seen {}",
                                    here.as_deref().and_then(|h| h.get(..8)).unwrap_or("-"),
                                    short(state.synced.get(*p)),
                                    state.pending.get(*p).map_or("-".to_owned(), |v| short(v.as_ref())),
                                    short(state.tree.files.get(*p)),
                                    seen.unwrap_or_else(|| "-".to_owned())
                                )
                            })
                            .collect();
                        self.trace.note(|| format!("{now:?} leaving m{}: looked {looked:?}, standing reached {} behind {} unsent {}; {files:?}", at.saturating_add(2), st.reached, st.behind, st.unsent));
                    }
                }
                match mark {
                    Some(mark) => {
                        self.left(at, &mark)?;
                        if now > since {
                            self.counts.moves_waited += 1;
                        }
                        self.trace.note(|| format!("{now:?} the session leaves m{} through log index {}", at.saturating_add(2), mark.index));
                        if let Some(se) = self.session.as_mut() {
                            se.leg = Leg::Moving { to, mark };
                        }
                        let transfer = self.rng.span(Duration::from_millis(100), Duration::from_secs(5));
                        self.after(transfer, Ev::SessionArrive);
                    }
                    None => self.after(FILES_EVERY, Ev::SessionPoll),
                }
            }
            Leg::Arriving { mark } => {
                let st = self.machines.get(at).and_then(|mc| mc.runner.as_ref()).map(|rn| Standing { repo: REPO.to_owned(), ..stand(rn.r.machine()) });
                if st.is_some_and(|st| arrived(&st, &mark)) {
                    self.went_on(at)?;
                    self.trace.note(|| format!("{now:?} the session goes on on m{}", at.saturating_add(2)));
                    if let Some(se) = self.session.as_mut() {
                        se.leg = Leg::Working;
                    }
                    self.counts.moves += 1;
                } else {
                    self.after(FILES_IN_EVERY, Ev::SessionPoll);
                }
            }
            Leg::Working | Leg::Moving { .. } => {}
        }
        Ok(())
    }

    /// The session's package arrives where it goes: it waits there for its files.
    fn session_arrive(&mut self) {
        let Some(se) = self.session.as_mut() else { return };
        if let Leg::Moving { to, mark } = se.leg.clone() {
            se.at = to;
            se.leg = Leg::Arriving { mark };
            let now = self.now();
            self.at(now, Ev::SessionPoll);
        }
    }

    /// The session leaves machine `from` through log index `mark`: each of its last writes is in
    /// the log by then.
    fn left(&self, from: usize, mark: &Mark) -> Result<(), String> {
        let Some(se) = self.session.as_ref() else { return Ok(()) };
        for (path, (t, bytes)) in &se.wrote {
            let hash = kiln_seams::sha256_hex(bytes);
            let logged = self.history.range(..=mark.index).any(|(_, (step, _))| step.changes.iter().any(|c| c.path == *path && matches!(&c.to, Some(Version::File { hash: h, .. }) if *h == hash)));
            if !logged {
                return Err(format!(
                    "{:?}: the session left m{} with its write to {path} ({} bytes, at {t:?}) not in the log through index {}: left behind",
                    self.now(),
                    from.saturating_add(2),
                    bytes.len(),
                    mark.index
                ));
            }
        }
        Ok(())
    }

    /// The session goes on on machine `m`: each of its last writes is in its folder.
    fn went_on(&self, m: usize) -> Result<(), String> {
        let (Some(se), Some(mc)) = (self.session.as_ref(), self.machines.get(m)) else { return Ok(()) };
        for (path, (t, bytes)) in &se.wrote {
            let here = mc.disk.read(&mc.root.join(path).to_string_lossy()).ok();
            if here.as_deref() != Some(bytes.as_slice()) {
                let holds = here.map_or_else(|| "nothing".to_owned(), |h| format!("{} bytes of other contents", h.len()));
                return Err(format!("{:?}: the session went on on {} without its write to {path} ({} bytes, at {t:?}): the folder holds {holds}", self.now(), mc.name, bytes.len()));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------ runners

    /// Starts machine `m`'s runner on what its disk and its log hold.
    fn start(&mut self, m: usize) -> Result<(), String> {
        let Some(mc) = self.machines.get_mut(m) else { return Ok(()) };
        mc.disk.thaw();
        let blobs = Blobs::on(Arc::clone(&mc.fs), &mc.blobs_dir).map_err(|err| format!("{}: its contents store wouldn't open after a restart: {err}", mc.name))?;
        mc.blobs = Some(blobs);
        let feed = Feed::open_on(Arc::clone(&mc.fs), &mc.root, &mc.name, mc.member, Limits::default(), mc.state.clone())
            .map_err(|err| format!("{}'s copy wouldn't open after a restart: {err:#}", mc.name))?;
        let mut feed = feed;
        feed.copy.planted_unsynced = self.plant.writes_unsynced;
        let (reached, sent, pending) = (feed.progress().0, feed.copy.state.sent.iter().map(|s| s.seq).collect::<Vec<_>>(), feed.copy.state.pending.len());
        let now = self.clock.elapsed();
        self.trace.note(|| format!("{now:?} {} starts: its state through log index {reached}, unsent steps {sent:?}, {pending} changes pending", mc.name));
        let r = Replica::open(&settings(mc.member, 1).map_err(e)?, mc.log.clone(), feed, Unbounded).map_err(|err| format!("{}'s member wouldn't open: {err:?}", mc.name))?;
        let now = self.clock.elapsed();
        mc.runner = Some(Runner {
            r,
            out: Output::default(),
            uploads: BTreeMap::new(),
            sent: 0,
            fetching: BTreeMap::new(),
            retry: BTreeMap::new(),
            gone: BTreeSet::new(),
            looked: now.checked_sub(LOOK).unwrap_or_default(),
            resent: now.checked_sub(RESEND).unwrap_or_default(),
            attempts: 0,
        });
        Ok(())
    }

    /// Machine `m`'s runner ends where it is (and with it the power, when `power`).
    fn crash(&mut self, m: usize, power: bool) {
        let back = self.rng.span(Duration::from_secs(1), Duration::from_secs(20));
        let Some(mc) = self.machines.get_mut(m) else { return };
        let Some(mut rn) = mc.runner.take() else { return };
        mc.blobs = None;
        mc.log = after_crash(rn.r.log_mut());
        drop(rn);
        if power {
            mc.disk.power_loss();
            self.counts.power_losses += 1;
        }
        self.counts.crashes += 1;
        self.trace.note(|| format!("{:?} {} crashes{}", self.clock.elapsed(), mc.name, if power { ", the power gone" } else { "" }));
        let at = self.clock.elapsed().saturating_add(back);
        if !self.queue.push(at, Ev::Up(m)) {
            self.overflowed = true;
        }
    }

    /// Machine `m`'s runner's contents store: none while it's down.
    fn blobs(&self, m: usize) -> Option<Blobs> {
        self.machines.get(m)?.blobs.clone()
    }

    /// One drive of machine `m`'s runner, as `sync::runner`'s owner drives it.
    fn drive(&mut self, m: usize) -> Result<(), String> {
        let now = self.now();
        let at_ms = self.unix_ms();
        let Some(blobs) = self.blobs(m) else { return Ok(()) };
        let Some(mc) = self.machines.get_mut(m) else { return Ok(()) };
        let Some(rn) = mc.runner.as_mut() else { return Ok(()) };
        let mut messages = Vec::new();
        // An error (its saved state couldn't be written) the runner says, and drives it again next
        // time.
        let _ = pump(&mut rn.r, ns(now), Waker::noop(), &mut rn.out, &mut messages);
        // Each peer's messages in the order made; the peers in a fixed order (a map's isn't).
        messages.sort_by_key(|msg| msg.to);
        let mut frames: Vec<(Who, Frame)> = messages.into_iter().filter(|msg| msg.to == SERVER).map(|msg| (Who::Server, Frame::Raft(Box::new(msg)))).collect();
        let wanted = match rn.r.machine_mut().work(&blobs) {
            Ok(wanted) => wanted,
            Err(err) => {
                // As the runner does: the folder's error is said, and the next drive takes it again.
                self.trace.note(|| format!("{now:?} {}'s folder: {err}", mc.name));
                Vec::new()
            }
        };
        if self.trace.keeps() {
            // What the folder did, as the runner's status reads it (and clears it).
            let feed = rn.r.machine_mut();
            let (took, outcomes): (Vec<_>, Vec<_>) = (std::mem::take(&mut feed.took), feed.outcomes.drain(..).collect());
            if !took.is_empty() || !outcomes.is_empty() {
                let reached = feed.progress().0;
                self.trace.note(|| format!("{now:?} {}'s folder took {:?} through log index {reached}: {outcomes:?}", mc.name, took.iter().map(|t| format!("{}{}", t.by, if t.ours { " (ours)" } else { "" })).collect::<Vec<_>>()));
            }
        }
        // Its contents, fetched a few at a time.
        rn.retry.retain(|_, t| *t > now);
        for hash in wanted {
            if rn.fetching.len() >= TRANSFERS {
                break;
            }
            if rn.fetching.contains_key(&hash) || rn.retry.contains_key(&hash) || rn.gone.contains(&hash) || blobs.has(&hash) {
                continue;
            }
            rn.attempts = rn.attempts.saturating_add(1);
            rn.fetching.insert(hash.clone(), rn.attempts);
            frames.push((Who::Server, Frame::Get { hash: hash.clone(), attempt: rn.attempts }));
            if !self.queue.push(now.saturating_add(TRANSFER), Ev::GetLate { m, hash, attempt: rn.attempts }) {
                self.overflowed = true;
            }
        }
        // A look at the folder, once it has taken everything committed: the runner's own
        // (`look_now_at`, an error said and the next look saving again).
        if now.saturating_sub(rn.looked) >= LOOK {
            rn.looked = now;
            let feed = rn.r.machine_mut();
            if let Some(Ok((proposed, unsaved))) = look_now_at(feed, &blobs, true, at_ms)
                && (proposed > 0 || unsaved.is_some())
            {
                self.trace.note(|| {
                    let sent = &feed.copy.state.sent;
                    let what: Vec<String> = sent.iter().skip(sent.len().saturating_sub(proposed)).map(|s| format!("#{} {:?}", s.seq, s.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>())).collect();
                    format!("{now:?} {} proposes {what:?} (not saved: {unsaved:?})", mc.name)
                });
            }
        }
        // The contents its unsent steps name, a few at a time.
        let mut moving = rn.uploads.values().filter(|u| matches!(u, Upload::Moving(_))).count();
        let need: Vec<String> = rn.r.machine().copy.state.sent.iter().flat_map(|s| s.changes.iter()).filter_map(|c| match &c.to {
            Some(Version::File { hash, .. }) => Some(hash.clone()),
            _ => None,
        }).collect();
        for hash in need {
            if moving >= TRANSFERS {
                break;
            }
            let ready = match rn.uploads.get(&hash) {
                None => true,
                Some(Upload::Failed(t)) => *t <= now,
                Some(_) => false,
            };
            if !ready {
                continue;
            }
            let Ok(bytes) = blobs.read(&hash) else {
                rn.uploads.insert(hash, Upload::Failed(now.saturating_add(RETRY)));
                continue;
            };
            rn.attempts = rn.attempts.saturating_add(1);
            rn.uploads.insert(hash.clone(), Upload::Moving(rn.attempts));
            frames.push((Who::Server, Frame::Put { hash: hash.clone(), bytes, attempt: rn.attempts }));
            if !self.queue.push(now.saturating_add(TRANSFER), Ev::PutLate { m, hash, attempt: rn.attempts }) {
                self.overflowed = true;
            }
            moving += 1;
        }
        // Its unsent steps whose contents are on the server, in order: new ones now, all again
        // every so often (the runner's own `to_send`).
        let again = now.saturating_sub(rn.resent) >= RESEND;
        if again {
            rn.resent = now;
        }
        let uploads = &rn.uploads;
        if let Some(go) = to_send(rn.r.machine(), |h| uploads.get(h) == Some(&Upload::Done), rn.sent, again) {
            let went: Vec<u64> = go.steps.iter().map(|s| s.seq).collect();
            frames.extend(go.steps.iter().map(|s| (Who::Server, Frame::Steps(vec![(*s).clone()]))));
            rn.sent = go.newest;
            self.trace.note(|| format!("{now:?} {} sends steps {went:?}{}", mc.name, if again { " (again)" } else { "" }));
        }
        for (to, frame) in frames {
            self.send(Who::M(m), to, frame);
        }
        if self.machines.get(m).is_some_and(|mc| mc.disk.frozen()) {
            self.counts.frozen += 1;
            let power = self.rng.chance(500);
            self.crash(m, power);
        }
        Ok(())
    }

    fn heard_by_machine(&mut self, m: usize, frame: Frame) -> Result<(), String> {
        let now = self.now();
        let Some(blobs) = self.blobs(m) else { return Ok(()) };
        let Some(mc) = self.machines.get_mut(m) else { return Ok(()) };
        let Some(rn) = mc.runner.as_mut() else { return Ok(()) };
        match frame {
            Frame::Raft(msg) => {
                if msg.from == SERVER {
                    let _ = rn.r.step(*msg);
                }
            }
            Frame::Want(hashes) => {
                for h in hashes {
                    rn.uploads.remove(&h);
                }
                rn.resent = now.checked_sub(RESEND).unwrap_or_default();
            }
            Frame::Putted { hash, ok, attempt } => {
                if rn.uploads.get(&hash) == Some(&Upload::Moving(attempt)) {
                    rn.uploads.insert(hash, if ok { Upload::Done } else { Upload::Failed(now.saturating_add(RETRY)) });
                }
            }
            Frame::Got { hash, bytes, attempt } => {
                if rn.fetching.get(&hash) != Some(&attempt) {
                    return Ok(());
                }
                rn.fetching.remove(&hash);
                match bytes {
                    // As the runner's fetch does: written to a file in the store (synced), then
                    // taken in under its name if it is what it's named.
                    Some(bytes) => {
                        let tmp = blobs.incoming();
                        let written = (|| -> std::io::Result<()> {
                            let mut f = mc.fs.create(&tmp)?;
                            std::io::Write::write_all(&mut f, &bytes)?;
                            f.sync_all()
                        })();
                        let kept = written.and_then(|()| blobs.adopt(&tmp, &hash));
                        match kept {
                            Ok(true) => self.counts.fetched += 1,
                            _ => {
                                let _ = mc.fs.remove_file(&tmp);
                                rn.retry.insert(hash, now.saturating_add(RETRY));
                            }
                        }
                    }
                    None => {
                        rn.r.machine_mut().copy.gone.insert(hash.clone());
                        rn.gone.insert(hash);
                    }
                }
            }
            Frame::Steps(_) | Frame::Put { .. } | Frame::Get { .. } => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------ the server

    fn drive_server(&mut self) {
        let now = self.now();
        let Some(r) = self.server.r.as_mut() else { return };
        let mut messages = Vec::new();
        let pumped = pump(r, ns(now), Waker::noop(), &mut self.server.out, &mut messages);
        messages.sort_by_key(|msg| msg.to);
        if self.trace.keeps() {
            let stands = format!("leader {} term {} applied {} learners {:?} in flight {} stalled {} pump {:?}", r.is_leader(), r.term(), r.applied().index, r.configuration().learners, r.in_flight(), r.is_stalled(), pumped.err());
            if stands != self.server.said {
                self.trace.note(|| format!("{now:?} the server stands: {stands}"));
                self.server.said = stands;
            }
        }
        // Each copy a learner, one change at a time.
        if r.is_leader() {
            let have = r.configuration().learners.clone();
            if let Some(mc) = self.machines.iter().find(|mc| !have.contains(&mc.member)) {
                let _ = r.change(Vec::new(), &learner(mc.member, true));
            }
        }
        for t in std::mem::take(&mut r.machine_mut().taken) {
            if self.history.contains_key(&t.index) {
                // The log replayed as the server came back: taken before.
                continue;
            }
            self.history.insert(t.index, (t.step.clone(), t.decisions.clone()));
            self.counts.steps_committed += 1;
            self.trace.fold(t.index);
            for (c, d) in t.step.changes.iter().zip(&t.decisions) {
                if matches!(d, crate::sync::Decision::Beside(_)) {
                    self.counts.conflicts += 1;
                }
                let known = match &c.to {
                    Some(Version::File { hash, .. }) => self.written.contains(hash),
                    Some(Version::Link { target }) => self.written.contains(&format!("->{target}")),
                    None => true,
                };
                if !known && self.invented.len() < 10 {
                    self.invented.push(format!("{}'s step #{} (log index {}) holds contents nobody wrote at {} ({:?})", t.step.by, t.step.seq, t.index, c.path, c.to));
                }
            }
        }
        let mut out = Vec::new();
        for msg in messages {
            let Some(m) = self.machines.iter().position(|mc| mc.member == msg.to) else { continue };
            let up = self.machines.get(m).is_some_and(|mc| mc.runner.is_some());
            if let Some(r) = self.server.r.as_mut() {
                if !up {
                    let _ = r.report_unreachable(msg.to);
                    continue;
                }
                if msg.msg_type == MessageType::MsgSnapshot {
                    let _ = r.report_snapshot(msg.to, true);
                }
            }
            out.push((Who::M(m), Frame::Raft(Box::new(msg))));
        }
        for (to, frame) in out {
            self.send(Who::Server, to, frame);
        }
    }

    fn heard_by_server(&mut self, from: Who, frame: Frame) {
        let Who::M(m) = from else { return };
        let Some(r) = self.server.r.as_mut() else { return };
        let Some(mc) = self.machines.get(m) else { return };
        let (name, member) = (mc.name.clone(), mc.member);
        let reply = match frame {
            Frame::Raft(msg) => {
                if msg.to == SERVER {
                    let _ = r.step(*msg);
                }
                None
            }
            Frame::Steps(steps) => {
                let blobs = &self.server.blobs;
                let seqs: Vec<u64> = steps.iter().map(|s| s.seq).collect();
                let Taking { want, again, dropped, proposed } = take_steps(r, &mut self.server.next, |h| blobs.contains_key(h), &name, &[member], steps);
                let next = self.server.next.get(&member).copied();
                self.counts.again += again;
                let now = self.clock.elapsed();
                self.trace.note(|| format!("{now:?} the server gets {name}'s steps {seqs:?}: proposed {proposed:?}, again {again}, dropped {dropped:?}, wants {} (next: {next:?})", want.len()));
                (!want.is_empty()).then_some(Frame::Want(want))
            }
            Frame::Put { hash, bytes, attempt } => {
                // Hashed as it comes: what doesn't hash to its name is refused.
                let ok = kiln_seams::sha256_hex(&bytes) == hash;
                if ok {
                    self.server.blobs.insert(hash.clone(), bytes);
                }
                Some(Frame::Putted { hash, ok, attempt })
            }
            Frame::Get { hash, attempt } => {
                let bytes = self.server.blobs.get(&hash).cloned();
                Some(Frame::Got { hash, bytes, attempt })
            }
            Frame::Want(_) | Frame::Putted { .. } | Frame::Got { .. } => None,
        };
        if let Some(f) = reply {
            self.send(Who::Server, from, f);
        }
    }

    fn server_crash(&mut self) {
        let Some(mut r) = self.server.r.take() else { return };
        self.server.log = after_crash(r.log_mut());
        drop(r);
        self.server.next.clear();
        self.counts.server_crashes += 1;
        self.trace.note(|| format!("{:?} the server crashes", self.clock.elapsed()));
        let back = self.rng.span(Duration::from_secs(1), Duration::from_secs(10));
        self.after(back, Ev::ServerUp);
    }

    fn server_up(&mut self) -> Result<(), String> {
        if self.server.r.is_some() {
            return Ok(());
        }
        let mut r = Replica::open(&settings(SERVER, 1).map_err(e)?, self.server.log.clone(), Index::open(None).map_err(e)?, Unbounded).map_err(|err| format!("the server's member wouldn't open: {err:?}"))?;
        r.campaign().map_err(e)?;
        self.server.r = Some(r);
        Ok(())
    }

    // ------------------------------------------------------------------------------ the run

    /// Sends `frame` as kiln does: on the sync lane of the runner's QUIC link (in order, or lost
    /// when the lane sheds frames or the link drops), or as an HTTP request or its answer (contents
    /// moving): lost or late, never twice.
    fn send(&mut self, from: Who, to: Who, frame: Frame) {
        let now = self.now();
        let fate = match (&frame, self.retried.as_mut()) {
            (Frame::Raft(_) | Frame::Steps(_) | Frame::Want(_), _) => self.net.send_in_order(now, node(from), node(to), LANE_SYNC),
            // A request a client may send again, overtaking others; each copy answered once.
            (Frame::Put { .. } | Frame::Get { .. }, Some(retried)) => retried.send(now, node(from), node(to)),
            (Frame::Put { .. } | Frame::Putted { .. } | Frame::Get { .. } | Frame::Got { .. }, _) => self.net.send_once(now, node(from), node(to)),
        };
        for at in fate.arrivals() {
            if !self.queue.push(at, Ev::Arrive { to, from, frame: frame.clone() }) {
                self.overflowed = true;
            }
        }
    }

    fn step(&mut self, ev: Ev) -> Result<(), String> {
        let now = self.now();
        let faulty = now < self.until;
        match ev {
            Ev::Tick => {
                if let Some(r) = self.server.r.as_mut() {
                    let _ = r.tick();
                }
                for mc in &mut self.machines {
                    if let Some(rn) = mc.runner.as_mut() {
                        let _ = rn.r.tick();
                    }
                }
                self.drive_server();
                let n = self.machines.len();
                let first = self.rng.index(n);
                for k in 0..n {
                    self.drive(first.saturating_add(k).checked_rem(n).unwrap_or(0))?;
                }
                if now.saturating_add(TICK) <= self.end {
                    self.after(TICK, Ev::Tick);
                }
            }
            Ev::Arrive { to: Who::Server, from, frame } => {
                self.heard_by_server(from, frame);
                self.drive_server();
            }
            Ev::Arrive { to: Who::M(m), frame, .. } => {
                self.heard_by_machine(m, frame)?;
                self.drive(m)?;
            }
            Ev::Edit(m) => {
                if faulty {
                    self.edit(m);
                }
            }
            Ev::Crash { m, power } => {
                if faulty {
                    self.crash(m, power);
                }
            }
            Ev::Up(m) => self.start(m)?,
            Ev::Freeze { m, after } => {
                if faulty && let Some(mc) = self.machines.get(m) {
                    mc.disk.freeze_after(after);
                }
            }
            Ev::ServerCrash => {
                if faulty {
                    self.server_crash();
                }
            }
            Ev::ServerUp => self.server_up()?,
            Ev::Calm => {
                for mc in &self.machines {
                    if !mc.disk.frozen() {
                        mc.disk.thaw();
                    }
                }
            }
            Ev::Cut { m, span } => {
                let until = now.saturating_add(span);
                self.net.cut(node(Who::Server), node(Who::M(m)), until);
            }
            Ev::Full(m) => {
                if let Some(mc) = self.machines.get(m) {
                    mc.disk.set_capacity(Some(mc.disk.used().saturating_add(2048)));
                    self.counts.full += 1;
                }
            }
            Ev::Room(m) => {
                if let Some(mc) = self.machines.get(m) {
                    mc.disk.set_capacity(None);
                }
            }
            Ev::PutLate { m, hash, attempt } => {
                if let Some(rn) = self.machines.get_mut(m).and_then(|mc| mc.runner.as_mut())
                    && rn.uploads.get(&hash) == Some(&Upload::Moving(attempt))
                {
                    rn.uploads.insert(hash, Upload::Failed(now.saturating_add(RETRY)));
                }
            }
            Ev::SessionEdit => {
                if faulty {
                    self.session_edit();
                }
            }
            Ev::SessionMove => {
                if faulty {
                    self.session_move();
                }
            }
            Ev::SessionPoll => self.session_poll()?,
            Ev::SessionArrive => self.session_arrive(),
            Ev::GetLate { m, hash, attempt } => {
                if let Some(rn) = self.machines.get_mut(m).and_then(|mc| mc.runner.as_mut())
                    && rn.fetching.get(&hash) == Some(&attempt)
                {
                    rn.fetching.remove(&hash);
                    rn.retry.insert(hash, now.saturating_add(RETRY));
                }
            }
        }
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        for _ in 0..EVENTS_MAX {
            let Some((at, ev)) = self.queue.pop() else { return self.check() };
            if at > self.end {
                return self.check();
            }
            let now = self.now();
            if at > now {
                self.clock.advance(at.saturating_sub(now));
            }
            self.trace.fold(ns(at));
            self.step(ev)?;
            if !self.invented.is_empty() {
                return Err(self.invented.join("; "));
            }
            if self.overflowed {
                return Err(format!("{:?}: the model's event queue was full: an event was dropped", self.now()));
            }
        }
        Err(format!("{:?}: {EVENTS_MAX} events and the run hadn't ended: something woke itself forever", self.now()))
    }

    /// Once the nemesis has stopped and everything settled: every copy converged on what the log
    /// made the repo, nothing spread that nobody wrote, and nobody's last write was lost.
    fn check(&mut self) -> Result<(), String> {
        let Some(r) = self.server.r.as_ref() else { return Err("the server's member never came back".into()) };
        let tree = r.machine().tree.files.clone();
        for mc in &self.machines {
            let Some(rn) = mc.runner.as_ref() else { return Err(format!("{} never came back", mc.name)) };
            let feed = rn.r.machine();
            let st = &feed.copy.state;
            if !feed.idle() || !st.sent.is_empty() || !st.pending.is_empty() {
                // Contents its unsent steps carry that its store no longer holds: they can't be sent.
                let gone = st
                    .sent
                    .iter()
                    .flat_map(|s| s.changes.iter())
                    .filter_map(|c| match &c.to {
                        Some(Version::File { hash, .. }) => Some(hash),
                        _ => None,
                    })
                    .filter(|h| !mc.disk.is_file(&mc.blobs_dir.join(h.get(..2).unwrap_or("00")).join(h.as_str()).to_string_lossy()))
                    .count();
                return Err(format!(
                    "{} never settled: {} entries untaken, {} steps unsent ({gone} of the contents they carry gone from its store), {} changes pending ({:?})",
                    mc.name,
                    feed.progress().1,
                    st.sent.len(),
                    st.pending.len(),
                    st.pending.keys().collect::<Vec<_>>()
                ));
            }
            if st.tree.files != tree {
                return Err(format!("{}'s tree isn't the log's: {:?} against {:?}", mc.name, differ(&st.tree.files, &tree), differ(&tree, &st.tree.files)));
            }
            let folder = folder_versions(mc);
            if folder != tree {
                return Err(format!("{}'s folder isn't the log's repo: it has {:?}, the log {:?}", mc.name, differ(&folder, &tree), differ(&tree, &folder)));
            }
        }
        if let Some(se) = &self.session
            && !matches!(se.leg, Leg::Working)
        {
            return Err(format!("the session never finished its move: {:?}", se.leg));
        }
        // Each person's last write to a path nobody touched after it is in the repo, at its name
        // or beside it.
        for mc in &self.machines {
            for (path, (at, what)) in &mc.last {
                let Some(bytes) = what else { continue };
                let later = self.machines.iter().any(|o| o.last.get(path).is_some_and(|(t, _)| t > at || (t == at && o.name != mc.name)));
                if later {
                    continue;
                }
                let hash = kiln_seams::sha256_hex(bytes);
                let kept = tree.iter().any(|(p, v)| matches!(v, Version::File { hash: h, .. } if *h == hash) && (p == path || beside(path, p)));
                if !kept {
                    return Err(format!("{}'s last write to {path} ({} bytes, at {at:?}) is lost: the repo holds {:?}", mc.name, bytes.len(), tree.get(path)));
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self) {
        self.counts.net = self.net.counts();
        if let Some(retried) = &self.retried {
            self.counts.net.add(&retried.counts());
        }
        for mc in &self.machines {
            self.counts.disk.add(&mc.disk.counts());
        }
        for mc in &self.machines {
            self.trace.fold(mc.disk.digest());
        }
    }
}

/// `head`, repeated to `len` bytes (or `head` alone, when it's longer).
fn filled(head: &str, len: usize) -> Vec<u8> {
    let mut bytes = head.as_bytes().to_vec();
    while bytes.len() < len {
        bytes.extend_from_slice(head.as_bytes());
    }
    bytes.truncate(len.max(head.len()));
    bytes
}

/// What a machine's folder holds, as versions: every file and link but kiln's temporary files.
fn folder_versions(mc: &Machine) -> BTreeMap<String, Version> {
    let mut out = BTreeMap::new();
    for (path, (bytes, exec)) in mc.disk.files(&mc.root.to_string_lossy()) {
        if path.rsplit('/').next().is_some_and(|n| n.starts_with(TEMP_PREFIX)) {
            continue;
        }
        let v = match bytes.strip_prefix(b"->") {
            Some(target) if mc.disk.read_link(&mc.root.join(&path).to_string_lossy()).is_ok() => Version::Link { target: String::from_utf8_lossy(target).into_owned() },
            _ => Version::File { hash: kiln_seams::sha256_hex(&bytes), exec },
        };
        out.insert(path, v);
    }
    out
}

/// What `a` has that `b` hasn't, or has otherwise (a few, for a failure's words).
fn differ(a: &BTreeMap<String, Version>, b: &BTreeMap<String, Version>) -> Vec<String> {
    a.iter().filter(|(p, v)| b.get(*p) != Some(*v)).take(5).map(|(p, v)| format!("{p}={v:?}")).collect()
}

/// Whether `copy` is a conflict copy of `path` (`<stem>.conflict-<machine>-<when><.ext>`).
fn beside(path: &str, copy: &str) -> bool {
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    let prefix = if dir.is_empty() { format!("{stem}.conflict-") } else { format!("{dir}/{stem}.conflict-") };
    copy.starts_with(&prefix) && copy.ends_with(&ext)
}

/// One seed's run: what it counted and its digest, or how it failed (with its last steps when
/// `keep` asked for them).
pub fn run(seed: u64, keep: usize, plant: Plant, mode: Mode) -> Result<(Counts, u64), String> {
    let mut sim = Sim::new(seed, keep, plant, mode).map_err(|why| format!("seed {seed}: {why}"))?;
    let ran = sim.run();
    sim.finish();
    match ran {
        Ok(()) => Ok((sim.counts.clone(), sim.trace.digest())),
        Err(why) => {
            let tail = if sim.trace.keeps() { format!("\n--- its last steps:\n{}", sim.trace.tail_text()) } else { String::new() };
            Err(format!("{why}{tail}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use kiln_sim::seeds;

    use super::*;

    /// The model as the first bugs' seeds found it (each request sent once, disks' names kept in
    /// order): they replay those runs.
    const ONCE: Plant = Plant { writes_unsynced: false, requests_once: true, names_in_order: true };

    /// CP-T6.1, AC-T6.1.1: the sync model over the seed budget (200 seeds on every change
    /// request; nightly sets `KILN_SIM_SEEDS` and `KILN_SIM_FROM`): every copy converges, nothing
    /// spreads that nobody wrote, no one's last write is lost; and, over a budget that size, every
    /// fault struck.
    #[test]
    fn sync_converges_and_loses_nothing_under_the_nemesis() -> Result<(), String> {
        let mut all = Counts::default();
        let report = seeds::run_budget("sync", 200, |seed| {
            let keep = if seeds::replay().is_some() { 400 } else { 0 };
            let (counts, digest) = run(seed, keep, Plant::default(), Mode::Sync)?;
            all.add(&counts);
            Ok(digest)
        });
        eprintln!("{}\n  counted: {all:?}", report.line());
        let ran = report.seeds;
        report.into_result()?;
        if seeds::replay().is_none() && ran >= 200 {
            let none = [
                ("edits", all.edits),
                ("steps committed", all.steps_committed),
                ("conflicts", all.conflicts),
                ("runner crashes", all.crashes),
                ("power losses", all.power_losses),
                ("crashes at a chosen write", all.frozen),
                ("server crashes", all.server_crashes),
                ("writes refused for room (ENOSPC)", all.disk.full),
                ("contents fetched", all.fetched),
                ("frames lost", all.net.dropped),
                ("steps sent again", all.again),
                ("requests doubled", all.net.duplicated),
                ("frames cut off", all.net.cut),
                ("write errors", all.disk.write_errors),
                ("fsync errors", all.disk.fsync_errors),
                ("files torn", all.disk.torn),
                ("files zeroed", all.disk.zeroed),
                ("files lost", all.disk.lost),
                ("names lost", all.disk.names_lost),
            ];
            if let Some((what, _)) = none.iter().find(|(_, n)| *n == 0) {
                return Err(format!("no {what} over the budget: the model didn't test what it says"));
            }
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.4: a seed runs the same every time.
    #[test]
    fn a_sync_seed_replays_exactly() -> Result<(), String> {
        for seed in [1, 77, 4242] {
            seeds::twice(seed, |s| run(s, 0, Plant::default(), Mode::Sync).map(|(_, d)| d))?;
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the first bug kiln-sim found, by its seeds: sync wrote another machine's
    /// change into a folder through a temporary file renamed over its name unsynced, and saved
    /// state saying the folder held it. A power loss then left the name holding nothing, zeroes or
    /// a torn page. The next look took that for an edit made here and the log spread it to every
    /// copy (sync seeds 3 and 163); a moving session went on with its file empty (moves seed 57);
    /// or, its size and time unchanged, no look saw it, and the copy kept it for good while the log
    /// held another version (moves seed 90).
    #[test]
    fn the_seeds_where_a_power_loss_tore_a_file_sync_had_written_converge() -> Result<(), String> {
        for seed in [3, 163] {
            run(seed, 0, ONCE, Mode::Sync)?;
        }
        for seed in [57, 90] {
            run(seed, 0, ONCE, Mode::Moves)?;
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the first bug's contract: once a copy's folders are synced (as
    /// `Feed::save` does before it writes the state that says what the folder holds), a file sync
    /// wrote holds what it wrote, whatever a power loss does after; on either way of journaling
    /// names.
    #[test]
    fn a_file_sync_wrote_holds_it_after_a_power_loss_once_its_folders_are_synced() -> Result<(), String> {
        use crate::sync::{Change, Copy, State};
        for seed in 0..200u64 {
            let disk = Disk::new(SimClock::new(), Rng::new(seed), DiskFaults { fsync_commits_names: seed % 2 == 0, ..DiskFaults::none() });
            let fs: Arc<dyn Fs> = Arc::new(SimFs::new(disk.clone()));
            let root = PathBuf::from("/home/m2/repo");
            fs.create_dir_all(&root).map_err(e)?;
            disk.make_durable("/home/m2/repo");
            let blobs = Blobs::on(Arc::clone(&fs), &PathBuf::from("/state/m2/blobs")).map_err(e)?;
            let bytes = filled("another machine's change\n", 6000);
            let hash = blobs.put_bytes(&bytes).map_err(e)?;
            let mut copy = Copy::on(Arc::clone(&fs), &root, "m2", 2, Limits::default(), State::default());
            let change = Change { path: "docs/a.txt".into(), to: Some(Version::File { hash, exec: false }), base: None };
            copy.apply(&Step { by: "m3".into(), member: 3, seq: 1, at: SIM_EPOCH_MS, changes: vec![change] }, &blobs).map_err(e)?;
            copy.sync_dirs().map_err(e)?;
            disk.power_loss();
            let got = disk.read("/home/m2/repo/docs/a.txt").map_err(e)?;
            if got != bytes {
                return Err(format!("seed {seed}: after a power loss docs/a.txt holds {} bytes, not the {} sync wrote there", got.len(), bytes.len()));
            }
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the second bug kiln-sim found, by its seeds: a write failing part-way
    /// through applying a step (an I/O error, a full disk) left the step taken by the copy's tree
    /// with the rest of its files unwritten; tried again, the tree had it already, so they never
    /// were. A conflict copy missing from a folder for good (sync seed 184, moves 136 and 170), a
    /// file left at an older version (sync seed 125).
    #[test]
    fn the_seeds_where_a_write_failed_part_way_through_a_step_converge() -> Result<(), String> {
        for seed in [184, 125] {
            run(seed, 0, ONCE, Mode::Sync)?;
        }
        for seed in [136, 170] {
            run(seed, 0, ONCE, Mode::Moves)?;
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the second bug's contract: a step whose writes fail part-way, at any of
    /// them, is applied whole when it's tried again: every file it brings is in the folder.
    #[test]
    fn a_step_a_write_failed_part_way_through_is_applied_whole_when_tried_again() -> Result<(), String> {
        use crate::sync::{Change, Copy, State};
        for fail_at in 0..40u64 {
            let disk = Disk::new(SimClock::new(), Rng::new(fail_at), DiskFaults::none());
            let fs: Arc<dyn Fs> = Arc::new(SimFs::new(disk.clone()));
            let root = PathBuf::from("/home/m2/repo");
            fs.create_dir_all(&root).map_err(e)?;
            let blobs = Blobs::on(Arc::clone(&fs), &PathBuf::from("/state/m2/blobs")).map_err(e)?;
            let mut changes = Vec::new();
            let mut want = BTreeMap::new();
            for (path, body) in [("a.txt", "one"), ("src/b.rs", "two"), ("docs/c/d.md", "three")] {
                let hash = blobs.put_bytes(body.as_bytes()).map_err(e)?;
                want.insert(path.to_owned(), body.as_bytes().to_vec());
                changes.push(Change { path: path.into(), to: Some(Version::File { hash, exec: false }), base: None });
            }
            let step = Step { by: "m3".into(), member: 3, seq: 1, at: SIM_EPOCH_MS, changes };
            let mut copy = Copy::on(Arc::clone(&fs), &root, "m2", 2, Limits::default(), State::default());
            disk.freeze_after(fail_at);
            let first = copy.apply(&step, &blobs);
            disk.thaw();
            if first.is_err() {
                copy.apply(&step, &blobs).map_err(|err| format!("failing at write {fail_at}, then again: {err}"))?;
            }
            let have: BTreeMap<String, Vec<u8>> =
                disk.files("/home/m2/repo").into_iter().filter(|(p, _)| !p.rsplit('/').next().is_some_and(|n| n.starts_with(TEMP_PREFIX))).map(|(p, (b, _))| (p, b)).collect();
            if have != want {
                return Err(format!("failing at write {fail_at}: the folder holds {:?}, the step brings {:?}", have.keys().collect::<Vec<_>>(), want.keys().collect::<Vec<_>>()));
            }
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the third bug kiln-sim found, by its seeds: a file written again just
    /// after a look, keeping its size, kept its time too on a filesystem that stamps times by the
    /// second (or the tick), and no look after saw the change: it took the file for what it had
    /// seen. A moving session left its machine with its last write not in the log (moves seed
    /// 2000); a copy kept bytes the log never had, and proposed stale ones over a conflict's (sync
    /// seed 1848).
    #[test]
    fn the_seeds_where_a_change_kept_its_file_s_size_and_time_converge() -> Result<(), String> {
        run(1848, 0, ONCE, Mode::Sync)?;
        run(2000, 0, ONCE, Mode::Moves)?;
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the fourth bug kiln-sim found, by its seeds: a copy's save failing after
    /// it proposed a step (a full disk, an I/O error, a crash at that write), the runner sent the
    /// step anyway. A crash then lost the copy's record of it, and its next step took the same
    /// number: the server, which takes a number once, let that one go as sent again, and the copy
    /// dropped it too once the first came back through the log, its changes pending for good. A
    /// moving session left with its last write never in the log (moves seed 3174); a copy never
    /// settled (3083).
    #[test]
    fn the_seeds_where_a_step_went_out_before_it_was_saved_converge() -> Result<(), String> {
        for seed in [3174, 3083] {
            run(seed, 0, ONCE, Mode::Moves)?;
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the fourth bug's contract: a step a copy proposed goes to the server
    /// only once the copy's saved state holds it; while a save fails, the copy says it has steps
    /// unsaved, and the next save that works lets them go.
    #[test]
    fn a_step_goes_to_the_server_only_once_the_copy_s_saved_state_holds_it() -> Result<(), String> {
        let disk = Disk::new(SimClock::new(), Rng::new(1), DiskFaults::none());
        let fs: Arc<dyn Fs> = Arc::new(SimFs::new(disk.clone()));
        let root = PathBuf::from("/home/m2/repo");
        fs.create_dir_all(&root).map_err(e)?;
        let blobs = Blobs::on(Arc::clone(&fs), &PathBuf::from("/state/m2/blobs")).map_err(e)?;
        let mut feed = Feed::open_on(Arc::clone(&fs), &root, "m2", 2, Limits::default(), PathBuf::from("/state/m2/copy.json")).map_err(e)?;
        disk.write("/home/m2/repo/a.txt", b"one").map_err(e)?;
        let steps: Vec<u64> = feed.copy.propose(&blobs, SIM_EPOCH_MS).map_err(e)?.iter().map(|s| s.seq).collect();
        if steps.is_empty() {
            return Err("the change wasn't proposed".into());
        }
        // The save fails (the disk crashes at its first write): nothing may go out.
        disk.freeze_after(0);
        if feed.save().is_ok() {
            return Err("the save was to fail".into());
        }
        disk.thaw();
        if feed.sendable().next().is_some() || !feed.unsaved() {
            return Err("a step its saved state doesn't hold may go to the server".into());
        }
        // Saved, it goes.
        feed.save().map_err(e)?;
        let sendable: Vec<u64> = feed.sendable().map(|s| s.seq).collect();
        if sendable != steps || feed.unsaved() {
            return Err(format!("saved, the steps that may go are {sendable:?}, not {steps:?}"));
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the fifth bug kiln-sim found, by its seed: a step's version of a file
    /// was kept back from a copy whose file was changed there, unproposed; the change was then
    /// deleted (or undone) before a look proposed it. The folder was as the copy last synced it, so
    /// no look proposed a thing, and the step was taken, so nothing brought its version: that copy
    /// lacked the file the log held, for good (sync seed 1331).
    #[test]
    fn the_seed_where_a_kept_back_version_never_came_converges() -> Result<(), String> {
        run(1331, 0, ONCE, Mode::Sync).map(|_| ())
    }

    /// CP-T6.1, AC-T6.1.3: the fifth bug's contract: a version a step brought, kept back while the
    /// file was changed here, comes at the next look once the change here is undone, or a file made
    /// here is deleted, without being proposed; and nothing is proposed for it.
    #[test]
    fn a_kept_back_version_comes_once_the_change_here_is_undone_or_deleted() -> Result<(), String> {
        use crate::sync::{Change, Copy, Outcome, State};
        for undone in [true, false] {
            let disk = Disk::new(SimClock::new(), Rng::new(7), DiskFaults::none());
            let fs: Arc<dyn Fs> = Arc::new(SimFs::new(disk.clone()));
            let root = PathBuf::from("/home/m2/repo");
            fs.create_dir_all(&root).map_err(e)?;
            let blobs = Blobs::on(Arc::clone(&fs), &PathBuf::from("/state/m2/blobs")).map_err(e)?;
            let file = "/home/m2/repo/a.txt";
            let version = |hash: &String| Some(Version::File { hash: hash.clone(), exec: false });
            let (zero, one) = (blobs.put_bytes(b"zero").map_err(e)?, blobs.put_bytes(b"one!").map_err(e)?);
            let step = |seq: u64, change: Change| Step { by: "m3".into(), member: 3, seq, at: SIM_EPOCH_MS, changes: vec![change] };
            let mut copy = Copy::on(Arc::clone(&fs), &root, "m2", 2, Limits::default(), State::default());
            let base = if undone {
                // a.txt as the log has it, then changed here.
                copy.apply(&step(1, Change { path: "a.txt".into(), to: version(&zero), base: None }), &blobs).map_err(e)?;
                version(&zero)
            } else {
                // A file made here.
                None
            };
            disk.write(file, b"mine").map_err(e)?;
            // Another machine's version: kept back, the file changed here.
            let kept = copy.apply(&step(2, Change { path: "a.txt".into(), to: version(&one), base }), &blobs).map_err(e)?;
            if !matches!(kept.as_slice(), [Outcome::Kept { .. }]) {
                return Err(format!("the version wasn't kept back: {kept:?}"));
            }
            // The change here goes before any look proposed it.
            if undone {
                disk.write(file, b"zero").map_err(e)?;
            } else {
                disk.remove_file(file).map_err(e)?;
            }
            let proposed = copy.propose(&blobs, SIM_EPOCH_MS).map_err(e)?;
            let got = disk.read(file).ok();
            if !proposed.is_empty() || got.as_deref() != Some(&b"one!"[..]) {
                return Err(format!("undone: {undone}: the look proposed {proposed:?} and a.txt holds {got:?}, not the log's version"));
            }
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3 (the fifth bug's fix, as the runner's sweep sees it): a version kept
    /// back is among the contents the copy still needs (`Feed::named`), so the sweep, which drops
    /// the contents no copy names, keeps it for the look that brings it.
    #[test]
    fn a_kept_back_version_is_named_till_it_comes() -> Result<(), String> {
        use crate::sync::{Change, Outcome};
        let disk = Disk::new(SimClock::new(), Rng::new(7), DiskFaults::none());
        let fs: Arc<dyn Fs> = Arc::new(SimFs::new(disk.clone()));
        let root = PathBuf::from("/home/m2/repo");
        fs.create_dir_all(&root).map_err(e)?;
        let blobs = Blobs::on(Arc::clone(&fs), &PathBuf::from("/state/m2/blobs")).map_err(e)?;
        let mut feed = Feed::open_on(Arc::clone(&fs), &root, "m2", 2, Limits::default(), PathBuf::from("/state/m2/copy.json")).map_err(e)?;
        let one = blobs.put_bytes(b"one!").map_err(e)?;
        // A file made here, unproposed; another machine's version of it comes.
        disk.write("/home/m2/repo/a.txt", b"mine").map_err(e)?;
        let change = Change { path: "a.txt".into(), to: Some(Version::File { hash: one.clone(), exec: false }), base: None };
        let kept = feed.copy.apply(&Step { by: "m3".into(), member: 3, seq: 1, at: SIM_EPOCH_MS, changes: vec![change] }, &blobs).map_err(e)?;
        if !matches!(kept.as_slice(), [Outcome::Kept { .. }]) {
            return Err(format!("the version wasn't kept back: {kept:?}"));
        }
        if !feed.named().contains(&one) {
            return Err("a version kept back isn't among the contents the copy names: the sweep would drop it before it came".into());
        }
        disk.remove_file("/home/m2/repo/a.txt").map_err(e)?;
        feed.copy.propose(&blobs, SIM_EPOCH_MS).map_err(e)?;
        let got = disk.read("/home/m2/repo/a.txt").ok();
        if got.as_deref() != Some(&b"one!"[..]) {
            return Err(format!("the kept-back version never came: a.txt holds {got:?}"));
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the sixth bug kiln-sim found, by its seeds: the contents store renamed
    /// what a look put in it into its folders and never synced them, so a power loss on a disk
    /// that keeps each folder's names apart, as POSIX allows, could lose them while the state
    /// naming them, saved and synced, survived. The copy's steps carrying them could never be sent,
    /// and its changes stayed pending for good (sync seeds 12 and 14, moves 12 and 14).
    #[test]
    fn the_seeds_where_contents_a_look_kept_were_lost_converge() -> Result<(), String> {
        for seed in [12, 14] {
            run(seed, 0, Plant::default(), Mode::Sync)?;
            run(seed, 0, Plant::default(), Mode::Moves)?;
        }
        Ok(())
    }

    /// CP-T6.1, AC-T6.1.3: the sixth bug's contract: once a copy has saved the state that names
    /// the contents a look put in its store, a power loss leaves the store holding every one its
    /// unsent steps carry; on either way of journaling names.
    #[test]
    fn contents_a_look_kept_outlive_a_power_loss_once_the_state_naming_them_is_saved() -> Result<(), String> {
        for seed in 0..200u64 {
            let disk = Disk::new(SimClock::new(), Rng::new(seed), DiskFaults { fsync_commits_names: seed % 2 == 0, ..DiskFaults::none() });
            let fs: Arc<dyn Fs> = Arc::new(SimFs::new(disk.clone()));
            let (root, state, store) = (PathBuf::from("/home/m2/repo"), PathBuf::from("/state/m2/copy.json"), PathBuf::from("/state/m2/blobs"));
            for dir in ["/home/m2/repo/docs/c", "/state/m2"] {
                disk.create_dir_all(dir).map_err(e)?;
                disk.make_durable(dir);
            }
            // A person's files, saved durably.
            for (path, len) in [("a.txt", 10), ("src.rs", 5000), ("docs/c/d.md", 300)] {
                let full = format!("/home/m2/repo/{path}");
                disk.write(&full, &filled(path, len)).map_err(e)?;
                disk.make_durable(&full);
            }
            let blobs = Blobs::on(Arc::clone(&fs), &store).map_err(e)?;
            let mut feed = Feed::open_on(Arc::clone(&fs), &root, "m2", 2, Limits::default(), state.clone()).map_err(e)?;
            if feed.copy.propose(&blobs, SIM_EPOCH_MS).map_err(e)?.is_empty() {
                return Err(format!("seed {seed}: the look proposed nothing"));
            }
            feed.save().map_err(e)?;
            disk.power_loss();
            let blobs = Blobs::on(Arc::clone(&fs), &store).map_err(e)?;
            let feed = Feed::open_on(Arc::clone(&fs), &root, "m2", 2, Limits::default(), state).map_err(e)?;
            let carried: Vec<&String> = feed
                .copy
                .state
                .sent
                .iter()
                .flat_map(|s| s.changes.iter())
                .filter_map(|c| match &c.to {
                    Some(Version::File { hash, .. }) => Some(hash),
                    _ => None,
                })
                .collect();
            if carried.len() != 3 {
                return Err(format!("seed {seed}: the saved state's unsent steps carry {} contents, not 3", carried.len()));
            }
            if let Some(gone) = carried.iter().find(|h| !blobs.has(h)) {
                return Err(format!("seed {seed}: after a power loss the store lacks {gone}, which the saved state's unsent steps carry"));
            }
        }
        Ok(())
    }

    /// CP-T6.1, T-T6.1.1: a planted bug, a lost write after a torn page, is found within the CI
    /// seed budget. What sync writes into a folder is renamed in unsynced (as before the first
    /// bug's fix); a power loss tears, zeroes or loses it; and the checker finds what comes of it
    /// (contents nobody wrote spreading, a last write lost, a copy apart from the log), by a seed
    /// that passes without the plant.
    #[test]
    fn the_planted_lost_write_after_a_torn_page_is_found() -> Result<(), String> {
        let planted = Plant { writes_unsynced: true, ..Plant::default() };
        for seed in 0..200 {
            let Err(why) = run(seed, 0, planted, Mode::Sync) else { continue };
            if !["nobody wrote", "is lost", "isn't the log's", "never settled"].iter().any(|c| why.contains(c)) {
                return Err(format!("seed {seed} failed with the plant, but not by the checker: {why}"));
            }
            run(seed, 0, Plant::default(), Mode::Sync).map_err(|why| format!("seed {seed} fails without the plant too: {why}"))?;
            return Ok(());
        }
        Err("200 seeds didn't find the planted lost write: the nemesis is too weak".into())
    }
}
