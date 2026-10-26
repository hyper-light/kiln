//! A machine's agent sessions (the server's own for `local`, a runner's for its machine): starts
//! them, routes commands to them, enforces limits, and keeps a small record on disk so that after
//! the process holding them restarts they continue where they were (each harness resumes its own
//! session), with kiln's queue of waiting messages intact.
//!
//! Its time is its clock's (kiln-seams' `Clock`, docs/seams.md): its watch of what the harnesses
//! run, and the pause that gathers changes into one write of its record, go by kiln's runtime in
//! production and by a simulated clock in tests.
//!
//! What it's told for a session goes down that session's lane, in the order it came, a command at a
//! time. A message goes through the session's inbox first (CP-T3.2, `super::inbox`): taken once,
//! fenced by the session's epoch, its answer said back as [`Update::Inbox`](super::core::Update).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use kiln_seams::Clock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::core::Update;
use super::inbox::{self, Answer, Inbox, Said};
use super::kinds::{Kind, KindInfo};
use super::model::{Body, Command, QueueItem, Status};
use super::session::{self, Info, Session, Sink, Spec};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Agent sessions at work on this machine at once ([`Info::at_work`]): finished, paused and
    /// ended ones don't count.
    pub per_machine: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { per_machine: super::limit::DEFAULT }
    }
}

/// A machine's room for agent sessions at work (`super::limit`): its limit, who's at work, and
/// who waits for room to go to work, the longest first. Its lock is taken last, nothing else
/// locked while it's held, so a session asks with its own state in hand.
#[derive(Debug, Default)]
struct Room {
    limits: Limits,
    /// At work, as each last said, or let go to work (before it says so).
    working: HashSet<String>,
    /// Waiting for room to go to work, the longest first.
    waiting: VecDeque<String>,
}

impl Room {
    /// Room for `id` to go to work: at work already, or a place free with nobody waiting longer.
    /// Let go, it's at work from now on; not, it waits in line.
    fn take(&mut self, id: &str) -> bool {
        if self.working.contains(id) {
            return true;
        }
        let free = self.limits.per_machine.saturating_sub(self.working.len());
        let ahead = self.waiting.iter().position(|w| w == id).unwrap_or(self.waiting.len());
        if ahead < free {
            self.waiting.retain(|w| w != id);
            self.working.insert(id.to_owned());
            return true;
        }
        if ahead == self.waiting.len() {
            self.waiting.push_back(id.to_owned());
        }
        false
    }

    /// Those told there may be room for them now: as many of the longest waiting as places are
    /// free.
    fn wakeable(&self) -> Vec<String> {
        let free = self.limits.per_machine.saturating_sub(self.working.len());
        self.waiting.iter().take(free).cloned().collect()
    }
}

/// What a machine runs and offers, sent to the server on every (re)connect.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Inventory {
    pub sessions: Vec<String>,
    pub kinds: Vec<KindInfo>,
}

/// A session as kept on disk, and as it goes to another runner process of this machine when its
/// harness process can't (a blue/green upgrade, `crate::bluegreen`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    id: String,
    spec: Spec,
    #[serde(default)]
    queue: Vec<QueueItem>,
    /// A pause for a usage limit, which goes on (with its timer) after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pause: Option<super::model::Pause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pause_at: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    manual_resume: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    move_on_limit: bool,
    #[serde(default, skip_serializing_if = "super::model::MovePlan::is_empty")]
    move_plan: super::model::MovePlan,
    /// A turn of its was under way, and hadn't ended, when this runner last saved it: a restart
    /// (an upgrade, the system stopping the runner, a crash) cut it short, and started again it
    /// is told to go on with it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    cut: bool,
    /// Its harness's process, and what it started, as last seen (`survivors`): after a restart, a
    /// harness that outlived its runner is stopped before another resumes the conversation, and
    /// what it started that still runs is taken back over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    harness: Option<super::survivors::Ran>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ran: Vec<super::survivors::Ran>,
    /// The newest ids its inbox took (its last [`inbox::KEEP`]; the oldest of them first), as it goes
    /// to another runner process of this machine (blue/green): taken there too, so a repeat of one
    /// is answered there, never a second prompt. In its record on disk, the ids it brought that its
    /// inbox hasn't taken in yet ([`Host::recorded`]), taken in as it's started again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inbox: Vec<String>,
    /// Each of `inbox`'s ids' digest, in the same order (`moving::Package::inbox_digests`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inbox_digests: Vec<String>,
}

impl Saved {
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// `s` as it is kept: its spec to resume it by, its queue and pause, and whether a turn of its is
/// under way. Takes the session's locks itself: never with one held.
fn record(s: &Session) -> Saved {
    // resume_spec takes the session's lock itself: before this one, never inside it.
    let spec = s.resume_spec();
    let ran = s.ran.lock().clone();
    let core = s.core.lock();
    let st = &core.state;
    Saved {
        id: s.id.clone(),
        spec,
        queue: st.queue.clone(),
        pause: st.pause.clone(),
        pause_at: st.pause_at,
        manual_resume: st.manual_resume,
        move_on_limit: st.move_on_limit,
        move_plan: st.move_plan.clone(),
        cut: s.turn_open.load(Ordering::Relaxed),
        harness: ran.0,
        ran: ran.1,
        inbox: Vec::new(),
        inbox_digests: Vec::new(),
    }
}

/// What a runner answers a hand-over of a session it runs from before it ([`Host::adopt`]: a copy
/// left behind), after [`crate::reach::node::NOT_TAKEN`]. A runner from before that said it alone,
/// and to a hand-over tried again after one whose answer never came, it meant that one took it over
/// (`crate::reach::node::Handle::adopt`).
pub const RUNS_HERE: &str = "that session runs here already";

/// What a machine's newest runner process answers a local hand-over ([`Host::adopt`] of a package
/// marked `handover`) its machine took over before, where this process doesn't run it: another of
/// its processes does (or did, and it ended there or went on elsewhere). The process that handed it
/// over lets go, and the server doesn't take this one for its holder (CP-T3.2, the nineteenth
/// adversarial check's).
pub const TAKEN_ELSEWHERE: &str = "taken over on this machine before, and not run by this process: ";

/// What a runner process handing everything to a new build of it, or retiring after it, answers a
/// local hand-over, after [`crate::reach::node::NOT_TAKEN`]: it takes none (it would end with it);
/// the older process goes on with it, asked for it again once the new build has taken over.
const LEAVING: &str = "this runner process is handing over to a new build of it, and leaving";

/// Why a hand-over wasn't taken over here ([`Host::adopt`]).
enum Refused {
    /// It isn't here: the runner that handed it over goes on with it.
    NotTaken(String),
    /// Whether it was taken over here before can't be said now: that runner asks again.
    Unsure(String),
    /// A local hand-over its machine took over, that this process doesn't run ([`TAKEN_ELSEWHERE`]).
    Elsewhere(String),
}

/// How a try at handing a session to its machine's newer runner process went
/// ([`super::moving::Package::attempt`]): decided once for the machine, by whichever of its processes
/// came first ([`Host::settle`], [`Host::adopt`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decided {
    /// A newer process took it over: the one that handed it over lets go of it.
    Taken,
    /// Not taken over (refused, or taken back by the process that handed it over), and no process
    /// takes that try over now: that process goes on with it.
    Refused,
}

/// A try's decision as the machine keeps it ([`Handovers`]): its session, whether it was taken over,
/// the epoch the session goes on at there, and when (unix ms).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Decision {
    session: String,
    taken: bool,
    epoch: u64,
    at: i64,
}

/// Hand-overs a machine remembers taking ([`Handovers`]), at most: past it, the oldest go.
pub const HANDOVERS_MAX: usize = 4096;

/// The hand-overs a machine took ([`Host::adopt`]): each session's newest epoch taken over at, and
/// when (unix ms); its newest [`HANDOVERS_MAX`]. Kept in the machine's state folder, where each of
/// its runner processes finds them (blue/green's new build is a process of its own, its folder
/// `<state>/instances/<id>`), written whole and synced (and its folder), one process at a time.
/// Those that couldn't be written are still known to the process that took them, and go with the
/// next written.
///
/// Beside them, how each try at handing a session to the machine's newer runner process went
/// (`decided`, its newest [`HANDOVERS_MAX`]): decided once, under the same lock, by whichever of its
/// processes comes first (one taking it over, or refusing it; or the process that handed it over
/// taking it back), written and synced before it's said or acted on. So whatever word of it reaches
/// the process that handed it over, from whichever try and however late, two of the machine's
/// processes never run it (CP-T3.2, the nineteenth adversarial check's).
struct Handovers {
    path: PathBuf,
    decided: PathBuf,
    unwritten: Mutex<HashMap<String, (u64, i64)>>,
}

/// The machine's hand-overs, this process's alone until it's dropped ([`Handovers::locked`]).
struct Locked<'a> {
    handovers: &'a Handovers,
    _lock: std::fs::File,
}

impl Locked<'_> {
    fn decisions(&self) -> std::io::Result<HashMap<String, Decision>> {
        match std::fs::read(&self.handovers.decided) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(e),
        }
    }

    /// How try `attempt` went: none while it isn't decided.
    fn decided(&self, attempt: &str) -> std::io::Result<Option<Decided>> {
        Ok(self.decisions()?.get(attempt).map(|d| if d.taken { Decided::Taken } else { Decided::Refused }))
    }

    /// Whether a try at handing session `id` over was taken over, to go on above `epoch`.
    fn taken_above(&self, id: &str, epoch: u64) -> std::io::Result<bool> {
        Ok(self.decisions()?.values().any(|d| d.taken && d.session == id && d.epoch > epoch))
    }

    /// Try `attempt` at handing session `id` over went `how` (taken: to go on at `epoch`), at `at`:
    /// written and synced, its folder too.
    fn decide(&self, attempt: &str, id: &str, how: Decided, epoch: u64, at: i64) -> std::io::Result<()> {
        let mut all = self.decisions()?;
        all.insert(attempt.to_owned(), Decision { session: id.to_owned(), taken: how == Decided::Taken, epoch, at });
        if all.len() > HANDOVERS_MAX {
            let mut by_age: Vec<(String, i64)> = all.iter().map(|(k, d)| (k.clone(), d.at)).collect();
            by_age.sort_by_key(|(_, at)| *at);
            for (k, _) in by_age.into_iter().take(all.len().saturating_sub(HANDOVERS_MAX)) {
                all.remove(&k);
            }
        }
        let json = serde_json::to_vec(&all).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::util::write_secret(&self.handovers.decided, &json, 0o600)?;
        self.handovers.decided.parent().map_or(Ok(()), crate::blobs::chunks::sync_dir)
    }

    /// [`Handovers::note`], with this lock held.
    fn note(&self, id: &str, epoch: u64, at: i64) -> std::io::Result<()> {
        self.handovers.note_by(id, epoch, at, |held| self.handovers.write_held(held))
    }
}

impl Handovers {
    /// The machine's, for a runner process whose own folder is `own_dir`.
    fn of(own_dir: &std::path::Path) -> Self {
        let state = match own_dir.parent() {
            Some(instances) if instances.file_name().is_some_and(|n| n == "instances") => instances.parent().unwrap_or(own_dir),
            _ => own_dir,
        };
        Self { path: state.join("handovers-taken.json"), decided: state.join("handovers-decided.json"), unwritten: Mutex::new(HashMap::new()) }
    }

    /// The machine's hand-overs, this process's (and this thread's) alone until what's returned is
    /// dropped.
    fn locked(&self) -> std::io::Result<Locked<'_>> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // One process at a time: blue/green's two may both take one over, and a machine's newer
        // processes may each be handed one.
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.path.with_extension("lock"))?;
        lock.lock()?;
        Ok(Locked { handovers: self, _lock: lock })
    }

    fn read(&self) -> std::io::Result<HashMap<String, (u64, i64)>> {
        match std::fs::read(&self.path) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(e),
        }
    }

    /// The epoch session `id` was last taken over at on this machine: none if never.
    fn taken(&self, id: &str) -> std::io::Result<Option<u64>> {
        let held = self.unwritten.lock().get(id).map(|(epoch, _)| *epoch);
        match self.read() {
            Ok(all) => Ok(all.get(id).map(|(epoch, _)| *epoch).max(held)),
            Err(_) if held.is_some() => Ok(held),
            Err(e) => Err(e),
        }
    }

    /// Session `id` taken over on this machine at `epoch`, at `at`: written with any not written
    /// before. Not written, it's still known here.
    fn note(&self, id: &str, epoch: u64, at: i64) -> std::io::Result<()> {
        // The machine's lock first, then what this process holds, as every path takes them (the
        // twentieth adversarial check's: the other way round, a take holding the lock would wait for
        // this note, and this note for the lock).
        match self.locked() {
            Ok(locked) => locked.note(id, epoch, at),
            Err(e) => self.note_by(id, epoch, at, |_| Err(e)),
        }
    }

    /// [`Handovers::note`], written by `write`.
    fn note_by(&self, id: &str, epoch: u64, at: i64, write: impl FnOnce(&HashMap<String, (u64, i64)>) -> std::io::Result<()>) -> std::io::Result<()> {
        let mut held = self.unwritten.lock();
        let was = held.get(id).map_or(0, |(e, _)| *e);
        held.insert(id.to_owned(), (epoch.max(was), at));
        // While its writes fail, its newest [`HANDOVERS_MAX`] are what's kept here, as in the file
        // (the twelfth adversarial check's): one more at a time, the oldest goes.
        if held.len() > HANDOVERS_MAX
            && let Some(oldest) = held.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone())
        {
            held.remove(&oldest);
        }
        write(&held)?;
        held.clear();
        Ok(())
    }

    /// `new` written into the machine's hand-overs, their lock held.
    fn write_held(&self, new: &HashMap<String, (u64, i64)>) -> std::io::Result<()> {
        let mut all = self.read()?;
        for (id, (epoch, at)) in new {
            let was = all.get(id).map_or(0, |(e, _)| *e);
            all.insert(id.clone(), ((*epoch).max(was), *at));
        }
        if all.len() > HANDOVERS_MAX {
            let mut by_age: Vec<(String, i64)> = all.iter().map(|(k, (_, at))| (k.clone(), *at)).collect();
            by_age.sort_by_key(|(_, at)| *at);
            for (k, _) in by_age.into_iter().take(all.len().saturating_sub(HANDOVERS_MAX)) {
                all.remove(&k);
            }
        }
        let json = serde_json::to_vec(&all).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::util::write_secret(&self.path, &json, 0o600)?;
        // Its new name lasts too: a power loss after it keeps the note.
        self.path.parent().map_or(Ok(()), crate::blobs::chunks::sync_dir)
    }
}

/// What a session's hand-over or move hears when its host has gone (CP-T3.2): it stays, as its
/// inbox's ids can't go with it.
const HOST_GONE: &str = "this runner's host of its sessions is gone, so it stays here";

/// What kiln says first to a session whose turn its restart cut short.
const CUT_BY_RESTART: &str = "kiln restarted on this machine part-way through your last turn, which stopped it. Pick up where you left off.";

pub struct Host {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    sink: Sink,
    /// What its sessions are held to, who's at work and who waits for room ([`Room`]): its most
    /// at work at once can change as it runs (`super::limit`).
    room: Mutex<Room>,
    file: Option<PathBuf>,
    dirty: AtomicBool,
    /// What the sessions' harnesses run is being looked at (`watch`).
    watching: AtomicBool,
    me: Weak<Host>,
    /// Where files attached to messages are kept, one directory per session.
    files_dir: PathBuf,
    /// Files received (whole), by file id.
    received: Mutex<HashMap<String, PathBuf>>,
    clock: Arc<dyn Clock>,
    /// Looks the watch has taken, and records written: told to `changed`'s listeners.
    looks: AtomicU64,
    saves: AtomicU64,
    /// Where its looks and record writes are counted for /metrics (kiln's, `metrics::agents()`).
    metrics: &'static crate::metrics::AgentMetrics,
    changed: event_listener::Event,
    /// Its sessions' inboxes (CP-T3.2): none without their records (`KILN_INBOX` off, or they
    /// couldn't be opened), and messages then go on as they come, unchecked.
    inbox: Option<Arc<Inbox>>,
    /// Each session's lane: what it's told, in order, a command at a time; and what says it's done.
    lanes: Mutex<HashMap<String, (async_channel::Sender<Lane>, async_channel::Receiver<()>)>>,
    /// Lanes on their way out, each done once what it holds is done: a session's next lane waits for
    /// its last (a session taken back right after it went: its fence first, then its opening).
    ending: Mutex<HashMap<String, async_channel::Receiver<()>>>,
    /// It took its sessions over from the runner process it upgrades (blue/green) and isn't
    /// promoted yet: that one takes them back if it never is, so this one takes no messages till
    /// then (each refused `moved`, and sent again).
    held: AtomicBool,
    /// The hand-overs this machine took (CP-T3.2), kept for every runner process of it: none
    /// without a record file.
    handovers: Option<Handovers>,
}

/// Most of what waits in one session's lane: past it, what's sent is refused (a message, `Full`).
pub const LANE: usize = 1024;

/// Longest a move waits for what its sessions' lanes hold to be done before it reads what their
/// inboxes took: a lane can wait on a sink nobody drains while its runner hands its sessions over.
const SETTLE_WAIT: Duration = Duration::from_secs(10);

/// What goes down a session's lane.
enum Lane {
    /// The session is here at `epoch`: the ids of the messages it brought from where it was taken in
    /// ([`Inbox::bring`]), and its inbox opened; `back`: taken back by this host at the epoch it left
    /// at (its inbox's fence lifted).
    Open { epoch: u64, back: bool },
    Cmd { c: Box<Command>, epoch: Option<u64> },
    /// Everything before it is done.
    Mark(futures::channel::oneshot::Sender<()>),
    /// It went on elsewhere from `epoch`, the epoch it was at here: its inbox here takes nothing
    /// more for it.
    Fence { epoch: u64, handover: bool },
    /// It ended for good: its inbox goes.
    Forget,
}

/// Largest file a message can carry.
pub const ATTACHMENT_MAX: u64 = 25 << 20;
/// Largest package of a session going on here from the server's store (its whole transcript).
pub const PACKAGE_MAX: u64 = 1 << 30;

impl Host {
    pub fn new(sink: Sink, file: Option<PathBuf>, limits: Limits) -> Arc<Self> {
        Self::with_clock(sink, file, limits, crate::rt::clock())
    }

    /// A host whose time is `clock`'s, its sessions' inboxes keeping their records beside its own
    /// record of them.
    pub fn with_clock(sink: Sink, file: Option<PathBuf>, limits: Limits, clock: Arc<dyn Clock>) -> Arc<Self> {
        Self::counting_into(sink, file, limits, clock, crate::metrics::agents())
    }

    /// A host whose time is `clock`'s, its looks and record writes counted into `metrics`, its
    /// sessions' inboxes keeping their records beside its own record of them.
    pub fn counting_into(sink: Sink, file: Option<PathBuf>, limits: Limits, clock: Arc<dyn Clock>, metrics: &'static crate::metrics::AgentMetrics) -> Arc<Self> {
        let records = file.as_ref().and_then(|f| f.parent()).and_then(open_records);
        Self::build(sink, file, limits, clock, records, metrics)
    }

    /// A host whose sessions' inboxes keep their records in `records` (none: no inboxes).
    pub fn with_records(sink: Sink, file: Option<PathBuf>, limits: Limits, clock: Arc<dyn Clock>, records: Option<Arc<dyn kiln_seams::Records>>) -> Arc<Self> {
        Self::build(sink, file, limits, clock, records, crate::metrics::agents())
    }

    fn build(sink: Sink, file: Option<PathBuf>, limits: Limits, clock: Arc<dyn Clock>, records: Option<Arc<dyn kiln_seams::Records>>, metrics: &'static crate::metrics::AgentMetrics) -> Arc<Self> {
        // The models its harnesses' sessions list are kept beside its record of them.
        if let Some(dir) = file.as_ref().and_then(|f| f.parent()) {
            super::kinds::keep_models_in(dir.join("harness-models.json"));
        }
        #[cfg(not(test))]
        super::acp::warm_models();
        let files_dir = file.as_ref().and_then(|f| f.parent()).map_or_else(
            || std::env::temp_dir().join(format!("kiln-attachments-{}", crate::os::uid())),
            |d| d.join("attachments"),
        );
        let handovers = file.as_ref().and_then(|f| f.parent()).map(Handovers::of);
        Arc::new_cyclic(|me| Self {
            sessions: Mutex::new(HashMap::new()),
            sink,
            room: Mutex::new(Room { limits, ..Room::default() }),
            file,
            dirty: AtomicBool::new(false),
            watching: AtomicBool::new(false),
            me: me.clone(),
            files_dir,
            received: Mutex::new(HashMap::new()),
            clock,
            looks: AtomicU64::new(0),
            saves: AtomicU64::new(0),
            metrics,
            changed: event_listener::Event::new(),
            inbox: records.map(|r| Arc::new(Inbox::new(r, inbox::Limits::default()))),
            lanes: Mutex::new(HashMap::new()),
            ending: Mutex::new(HashMap::new()),
            held: AtomicBool::new(false),
            handovers,
        })
    }

    /// Takes no messages (`on`), or takes them again: a new build that takes its sessions over from
    /// the runner process it upgrades takes none until it's promoted, as that one takes them back
    /// (with what they'd taken) if it never is.
    pub fn hold(&self, on: bool) {
        self.held.store(on, Ordering::Release);
    }

    /// It takes no messages now ([`Host::hold`]).
    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }

    /// What sessions new to this machine are held to now.
    pub fn limits(&self) -> Limits {
        self.room.lock().limits
    }

    /// At most `n` of its sessions at work from now on (`super::limit`): those at work go on,
    /// however many are; raised, those waiting longest are told there may be room. Whether that
    /// changed it.
    pub fn set_max_sessions(&self, n: usize) -> bool {
        let (changed, wake) = {
            let mut r = self.room.lock();
            let changed = r.limits.per_machine != n;
            r.limits.per_machine = n;
            (changed, r.wakeable())
        };
        self.wake(wake);
        changed
    }

    /// How many of its sessions are at work, and how many wait for room (`super::limit`).
    pub fn room_counts(&self) -> (usize, usize) {
        let r = self.room.lock();
        (r.working.len(), r.waiting.len())
    }

    /// Whether session `id` may go to work now (`super::limit`): at work already, or room for one
    /// more with nobody waiting longer, it may, and it's at work from now on; else it waits in
    /// line, told when there may be room ([`Session::room`]).
    pub fn room_for(&self, id: &str) -> bool {
        self.room.lock().take(id)
    }

    /// How session `id` stands now, and whether anything waits to go to its harness: one at work
    /// no longer frees its place, one with nothing to go leaves the line, and those waiting longest
    /// are told there may be room.
    pub fn stands(&self, id: &str, work: session::Work, waits: bool) {
        let wake = {
            let mut r = self.room.lock();
            let mut moved = false;
            match work {
                session::Work::Yes => {
                    r.waiting.retain(|w| w != id);
                    r.working.insert(id.to_owned());
                }
                // Let go to work, it keeps its place till it's on a turn or isn't.
                session::Work::Starting => {}
                session::Work::No => moved = r.working.remove(id),
            }
            if work != session::Work::Yes && !waits {
                let before = r.waiting.len();
                r.waiting.retain(|w| w != id);
                moved |= r.waiting.len() != before;
            }
            if moved { r.wakeable() } else { Vec::new() }
        };
        self.wake(wake);
    }

    /// Session `id` is this machine's no longer (closed, gone on elsewhere): its place, and its
    /// place in line, go.
    fn leave(&self, id: &str) {
        let wake = {
            let mut r = self.room.lock();
            let freed = r.working.remove(id);
            let before = r.waiting.len();
            r.waiting.retain(|w| w != id);
            if freed || r.waiting.len() != before { r.wakeable() } else { Vec::new() }
        };
        self.wake(wake);
    }

    /// Tells sessions `ids` there may be room on this machine for them to go to work.
    fn wake(&self, ids: Vec<String>) {
        for id in ids {
            if let Some(s) = self.get(&id) {
                s.room();
            }
        }
    }

    /// Its sessions' inboxes, when it keeps them.
    pub fn inbox(&self) -> Option<&Arc<Inbox>> {
        self.inbox.as_ref()
    }

    /// Looks the watch has taken at what the harnesses run.
    pub fn looks(&self) -> u64 {
        self.looks.load(Ordering::Acquire)
    }

    /// Records of the sessions written (a write that failed isn't one).
    pub fn saves(&self) -> u64 {
        self.saves.load(Ordering::Acquire)
    }

    /// Completes once `holds` holds, looked at again after each look and each record written.
    pub async fn until(&self, holds: impl Fn(&Self) -> bool) {
        loop {
            let listener = self.changed.listen();
            if holds(self) {
                return;
            }
            listener.await;
        }
    }

    /// A piece of a file attached to a message for `session` (blocking; small writes).
    pub fn receive(&self, session: &str, file: &str, offset: u64, data: &[u8], done: bool) -> anyhow::Result<()> {
        use std::io::{Seek as _, Write as _};
        let safe = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && !s.is_empty() && s.len() <= 64;
        if !safe(session) || !safe(file) {
            anyhow::bail!("bad file or session id");
        }
        if offset.saturating_add(data.len() as u64) > ATTACHMENT_MAX {
            anyhow::bail!("attachment too large");
        }
        let dir = self.files_dir.join(session).join("incoming");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(file);
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(offset == 0).open(&path)?;
        f.seek(std::io::SeekFrom::Start(offset))?;
        f.write_all(data)?;
        if done {
            self.received.lock().insert(file.to_owned(), path);
        }
        Ok(())
    }

    /// A piece of the package of a session the server sends to go on here (it went on from the
    /// server's conversation store: its machine is gone). Blocking (small writes). The package,
    /// once its last piece is here.
    pub fn receive_package(&self, session: &str, offset: u64, data: &[u8], done: bool) -> anyhow::Result<Option<PathBuf>> {
        use std::io::{Seek as _, Write as _};
        if session.is_empty() || session.len() > 64 || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            anyhow::bail!("bad session id");
        }
        if offset.saturating_add(data.len() as u64) > PACKAGE_MAX {
            anyhow::bail!("a session package too large");
        }
        let dir = self.files_dir.join(session);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("package.json");
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(offset == 0).open(&path)?;
        f.seek(std::io::SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(done.then_some(path))
    }

    /// Take over the session in the package the server sent (`receive_package`). Blocking. Whether
    /// an older runner process of this machine handed it over (`super::handing`: this one says how
    /// it went), and how it went. `leaving`: this process hands everything to a new build of it,
    /// or retires after it: a hand-over isn't taken, as it would end with this process; the older
    /// one goes on with it, and is asked for it again once the new build has taken over.
    pub fn adopt_package(&self, path: &std::path::Path, leaving: bool) -> (bool, Result<(), String>) {
        let bytes = std::fs::read(path).map_err(|e| format!("the session's package: {e}"));
        let _ = std::fs::remove_file(path);
        let Ok(bytes) = bytes else { return (false, bytes.map(|_| ())) };
        let handover = super::moving::handed_over(&bytes);
        (handover, self.adopt_as(if handover { "an older kiln runner of this machine" } else { "the kiln server" }, &bytes, handover && leaving))
    }

    /// The server's word on a hand-over of session `id` to this machine's newer runner process
    /// (`Command::HandedOver`). The session waiting on it goes on, or lets go (it waits until it's
    /// known: `super::handing`). Heard with nothing waiting for it: taken, it lets go; refused,
    /// the fence its hand-over left lifts, unless the machine's record says a try at handing it over
    /// was taken over (this word an earlier try's: it lets go then); not known, it stays as it is. A
    /// session handing itself over now decides by itself ([`Host::settle`]).
    pub fn handed_over(&self, id: &str, error: Option<String>, unsure: bool, by_newest: bool) {
        if super::handing::answered(id, error.clone(), unsure, by_newest) || self.get(id).is_some_and(|s| s.is_moving()) {
            return;
        }
        match error {
            None => {
                if self.let_go(id) {
                    tracing::info!(session = %id, "this machine's newer runner process took the agent session over after all: let go of it here");
                }
            }
            // Not known still: it stays as it is.
            Some(_) if unsure => {}
            // It didn't, after all: its inbox, fenced while that wasn't known, takes messages here
            // again (CP-T3.2) — unless the machine's record says a newer process took it over.
            Some(_) => {
                if let (Some(h), Some(s)) = (self.me.upgrade(), self.get(id)) {
                    let (id, epoch) = (id.to_owned(), s.epoch());
                    crate::rt::spawn(async move {
                        let looked = Arc::clone(&h);
                        let at = id.clone();
                        match crate::rt::blocking(move || looked.taken_on_machine(&at, epoch)).await.map_err(|e| format!("{e:#}")).and_then(|r| r) {
                            Ok(true) => {
                                if h.let_go(&id) {
                                    tracing::info!(session = %id, "a word that a hand-over was refused came late, while this machine's record says a newer runner process took it over: let go of it here");
                                }
                                return;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                tracing::warn!(target: "kiln.sends", event = "unfence_failed", session = %id, error = %e, "a word that a hand-over was refused came late, and this machine's record of its hand-overs can't be read: its inbox stays fenced until it's handed over");
                                return;
                            }
                        }
                        if let Err(e) = h.moving_fence(&id, false).await {
                            tracing::warn!(target: "kiln.sends", event = "unfence_failed", session = %id, error = %e, "a session whose hand-over failed couldn't take messages again: its inbox stays fenced until it's handed over");
                        }
                    });
                }
            }
        }
    }

    /// The machine's decision on try `attempt` at handing session `id` to its newer runner process
    /// (CP-T3.2, the nineteenth adversarial check's): as whichever of its processes came first
    /// decided it; or, none yet and `take_back`, decided now, not taken over (this process goes on
    /// with it, and no process takes that try over after). None while it isn't decided, or where
    /// this host keeps no record of the machine's hand-overs (the server's word alone says then).
    /// Blocking.
    pub fn settle(&self, attempt: &str, id: &str, take_back: bool) -> Result<Option<Decided>, String> {
        let Some(h) = &self.handovers else { return Ok(None) };
        let locked = h.locked().map_err(|e| format!("this machine's record of its hand-overs: {e}"))?;
        match locked.decided(attempt).map_err(|e| format!("this machine's record of its hand-overs: {e}"))? {
            Some(how) => Ok(Some(how)),
            None if take_back => {
                let epoch = self.get(id).map_or(0, |s| s.epoch());
                locked.decide(attempt, id, Decided::Refused, epoch, self.clock.unix_ms()).map_err(|e| format!("this machine's record of its hand-overs: {e}"))?;
                Ok(Some(Decided::Refused))
            }
            None => Ok(None),
        }
    }

    /// Whether this machine's record says session `id`, here at `epoch`, was taken over by a newer
    /// runner process of it (a try at handing it over taken, or its note of the hand-overs it took).
    /// Blocking.
    fn taken_on_machine(&self, id: &str, epoch: u64) -> Result<bool, String> {
        let Some(h) = &self.handovers else { return Ok(false) };
        let locked = h.locked().map_err(|e| e.to_string())?;
        let decided = locked.taken_above(id, epoch).map_err(|e| e.to_string())?;
        Ok(decided || h.taken(id).map_err(|e| e.to_string())?.is_some_and(|at| at > epoch))
    }

    /// Session `id` went on on another machine since: its harness here stops, and this machine
    /// lets go of it (it isn't closed).
    pub fn let_go(&self, id: &str) -> bool {
        let s = self.get(id);
        if let Some(s) = &s {
            s.let_go();
        }
        s.is_some()
    }

    /// A received file, once (its id is spent).
    fn take_file(&self, file: &str) -> Option<PathBuf> {
        self.received.lock().remove(file)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().get(id).cloned()
    }

    /// Each session and its harness's process, where one runs (which agent a process is: a
    /// `kiln` command run by an agent's tool, `super::socket`). The sessions' own locks are taken
    /// after this host's is let go.
    pub fn harness_pids(&self) -> Vec<(String, Option<u32>)> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        sessions.iter().map(|s| (s.id.clone(), *s.pid.lock())).collect()
    }

    pub fn list(&self) -> Vec<Info> {
        let mut v: Vec<Info> = self.sessions.lock().values().map(|s| s.info()).collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// What each session's harness keeps as its record, for the conversation store to follow.
    /// The sessions' locks are taken after this host's is let go.
    pub fn record_sources(&self) -> Vec<crate::convo::record::Source> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        sessions.iter().filter_map(|s| s.record_source()).collect()
    }

    pub fn inventory(&self) -> Inventory {
        Inventory { sessions: self.sessions.lock().keys().cloned().collect(), kinds: super::kinds::offered() }
    }

    /// Sessions still working on something (an upgrade waits for these).
    pub fn busy(&self) -> usize {
        self.sessions.lock().values().filter(|s| matches!(s.core.lock().state.status, Status::Running | Status::Waiting)).count()
    }

    /// Start a session (`id` keeps a known one's identity, e.g. one the server named). A subagent
    /// opens as its parent runs here ([`Host::like_its_parent`]).
    pub fn open(&self, id: Option<String>, spec: Spec) -> anyhow::Result<Arc<Session>> {
        let spec = self.like_its_parent(spec);
        self.open_seeded(id, spec, None, None, Vec::new(), false, true)
    }

    /// A subagent (`Spec::subagent`) whose parent runs on this machine takes what only the machine
    /// knows of how its parent runs: the environment its parent was given (its shell's, or what a
    /// take-over found: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, an API's address), so it has its parent's
    /// account and configuration, and, in its parent's harness, its parent's program. What its own
    /// spec says stays.
    fn like_its_parent(&self, mut spec: Spec) -> Spec {
        if !spec.subagent {
            return spec;
        }
        let Some(parent) = spec.parent.as_deref().and_then(|p| self.get(p)) else { return spec };
        let theirs = parent.spec.lock();
        if spec.env.is_empty() {
            spec.env.clone_from(&theirs.env);
        }
        if spec.program.is_none() && theirs.kind == spec.kind {
            spec.program.clone_from(&theirs.program);
        }
        drop(theirs);
        spec
    }

    /// Take over a session another runner handed this one (`super::moving`): its record where its
    /// harness here looks, then the session, under its own id, its timeline as it was. Blocking
    /// (writes the record). A hand-over taken over here before (sent again, its answer lost on the
    /// way back) is answered as taken, whatever became of the session since: it ended here, or went
    /// on elsewhere (CP-T3.2). Each refusal says it wasn't taken
    /// ([`crate::reach::node::NOT_TAKEN`]); but whether it was can't be said while this host's
    /// records can't be read, and the runner that handed it over asks again. A try at handing it over
    /// from an older runner process of this machine is decided once for the machine
    /// ([`Decided`]): taken over before where this process doesn't run it, it's answered
    /// [`TAKEN_ELSEWHERE`].
    pub fn adopt(&self, from: &str, data: &[u8]) -> Result<(), String> {
        self.adopt_as(from, data, false)
    }

    /// [`Host::adopt`]; `leaving`: this process hands everything to a new build of it, or retires
    /// after it, so a hand-over isn't taken here (it would end with this process), as the machine's
    /// record says (a try taken here before is answered so).
    fn adopt_as(&self, from: &str, data: &[u8], leaving: bool) -> Result<(), String> {
        self.take_over(from, data, leaving).map_err(|r| match r {
            Refused::NotTaken(why) => format!("{}{why}", crate::reach::node::NOT_TAKEN),
            Refused::Unsure(why) => why,
            Refused::Elsewhere(why) => format!("{TAKEN_ELSEWHERE}{why}"),
        })
    }

    fn take_over(&self, from: &str, data: &[u8], leaving: bool) -> Result<(), Refused> {
        let pkg: super::moving::Package = match serde_json::from_slice(data) {
            Ok(pkg) => pkg,
            Err(_) if leaving => return Err(Refused::NotTaken(LEAVING.into())),
            Err(e) => return Err(Refused::NotTaken(format!("the session from {from} is unreadable: {e}"))),
        };
        // It goes on here an epoch up (`session::start_seeded`).
        let epoch = pkg.state.epoch.saturating_add(1);
        // One take at a time on the machine (CP-T3.2, the nineteenth adversarial check's): two of its
        // processes given one package (a try sent again, the newest gone meanwhile; a move's sent
        // again after a blue/green upgrade) take it over once, the second finding the first's note.
        // A try at handing it over from an older process of this machine is decided once besides,
        // by whichever of its processes comes first: decided, it's answered so; not, it's decided
        // here, written before it's said or acted on. Its record unreadable or unwritable now, it's
        // neither: that process asks again. A package that names no try (a move, or a process from
        // before) is taken over as before where the lock can't be had.
        let attempt = pkg.attempt.clone().filter(|_| pkg.handover);
        let unsure = |e: std::io::Error| Refused::Unsure(format!("how this machine's hand-overs went can't be read or written now: {e}"));
        let locked = match (&self.handovers, &attempt) {
            (None, _) => None,
            (Some(h), Some(_)) => Some(h.locked().map_err(unsure)?),
            (Some(h), None) => match h.locked() {
                Ok(locked) => Some(locked),
                Err(e) => {
                    tracing::warn!(target: "kiln.sends", event = "adopt_unlocked", session = %pkg.id, error = %e, "this machine's lock on its hand-overs couldn't be had: a session is taken over here without it");
                    None
                }
            },
        };
        if let (Some(locked), Some(attempt)) = (&locked, &attempt) {
            match locked.decided(attempt).map_err(unsure)? {
                Some(Decided::Taken) if self.get(&pkg.id).is_some_and(|s| s.epoch() >= epoch) => return Ok(()),
                Some(Decided::Taken) => return Err(Refused::Elsewhere("this try at handing it over was taken over before".into())),
                Some(Decided::Refused) => return Err(Refused::NotTaken("this try at handing it over was decided before: not taken over".into())),
                None => {}
            }
        }
        let id = pkg.id.clone();
        let taken = self.take_over_now(from, pkg, epoch, locked.as_ref(), attempt.as_deref(), leaving);
        // Not taken over, for good: whatever comes of this try after, from any process, says so.
        if let (Err(Refused::NotTaken(why)), Some(locked), Some(attempt)) = (&taken, &locked, &attempt)
            && let Err(e) = locked.decide(attempt, &id, Decided::Refused, epoch, self.clock.unix_ms())
        {
            return Err(Refused::Unsure(format!("{why}; and that it wasn't taken over couldn't be written down: {e}")));
        }
        taken
    }

    /// [`Host::take_over`], its try at handing it over (if it's one) not decided yet, with the
    /// machine's hand-overs `locked` (where they could be): `attempt`, the try, is written as taken
    /// before the session opens here (or it's answered as taken).
    fn take_over_now(&self, from: &str, pkg: super::moving::Package, epoch: u64, locked: Option<&Locked<'_>>, attempt: Option<&str>, leaving: bool) -> Result<(), Refused> {
        let taken = |id: &str| match (locked, attempt) {
            (Some(locked), Some(attempt)) => locked.decide(attempt, id, Decided::Taken, epoch, self.clock.unix_ms()).map_err(|e| Refused::Unsure(format!("that it was taken over couldn't be written down: {e}"))),
            _ => Ok(()),
        };
        // A local hand-over is answered as taken only by the process that runs it: the machine's
        // note of the hand-overs it took is every one of its processes', and another of them may
        // run it (or did, and it ended there), so the one that handed it over lets go without this
        // one becoming its holder.
        if pkg.handover && !self.get(&pkg.id).is_some_and(|s| s.epoch() >= epoch) && self.taken_before(&pkg.id, epoch)? {
            taken(&pkg.id)?;
            return Err(Refused::Elsewhere(format!("the machine's note says it went on at epoch {epoch} or later")));
        }
        if self.taken_before(&pkg.id, epoch)? {
            taken(&pkg.id)?;
            tracing::info!(target: "kiln.sends", event = "adopt_again", session = %pkg.id, from = %from, epoch, "a hand-over taken over here before, sent again: answered as taken");
            return Ok(());
        }
        // Leaving: not taken over here (it would end with this process), as written down for the try.
        if leaving {
            return Err(Refused::NotTaken(LEAVING.into()));
        }
        if !super::moving::takes(pkg.version) {
            return Err(Refused::NotTaken(format!("a session packed as version {} (this runner takes up to {})", pkg.version, super::moving::VERSION)));
        }
        let kind = Kind::get(&pkg.spec.kind).ok_or_else(|| Refused::NotTaken(format!("this runner doesn't know {}", pkg.spec.kind)))?;
        // Its harness: the program the session names (a local hand-over keeps its own), else its
        // kind's.
        if !pkg.spec.program.as_deref().map_or_else(|| kind.available(), |p| crate::os::which(p).is_some()) {
            return Err(Refused::NotTaken(format!("{} is not installed on this machine", kind.name)));
        }
        // A copy of it from before this hand-over (left behind here).
        if self.get(&pkg.id).is_some() {
            return Err(Refused::NotTaken(RUNS_HERE.into()));
        }
        let mut pkg = pkg;
        let mut cwd = session::resolve_cwd(pkg.spec.cwd.as_deref());
        // Its folder isn't here (it wasn't a repository kiln keeps in step): it works in home. A
        // worktree it brings is made once its repository's files are here.
        let mut note = None;
        if !std::path::Path::new(&cwd).is_dir() && pkg.worktree.is_none() {
            note = Some(format!("{cwd} isn't on this machine, so it works in the home folder"));
            cwd = session::resolve_cwd(None);
            pkg.spec.cwd = Some(cwd.clone());
        }
        let items = super::moving::install(&pkg, &cwd).map_err(Refused::NotTaken)?;
        // What its harness keeps for it in its home (its memories, tasks, plans, checkpoints), said
        // with the move.
        let said = super::moving::install_home(&pkg, &cwd, note.is_none());
        let note = note.into_iter().chain(said).reduce(|a, b| format!("{a}. {b}"));
        // Its repository's state, the files git ignores and its worktree, kept on disk until its
        // files are here (a restart keeps them too), readable by this user alone: they may hold
        // secrets.
        if let Some(g) = pkg.git.take() {
            let dir = self.files_dir.join(&pkg.id);
            let kept = super::moving::MovedGit { from: pkg.from.clone(), state: g, ignored: pkg.ignored.take(), worktree: pkg.worktree.take() };
            std::fs::create_dir_all(&dir).and_then(|()| super::moving::keep_private(&super::moving::moved_git(&dir), &serde_json::to_vec(&kept).unwrap_or_default())).map_err(|e| Refused::NotTaken(format!("keeping its repository's state: {e}")))?;
        }
        let id = pkg.id.clone();
        // One this machine ran already (its older runner process handed it over) isn't new here:
        // it's never refused for the machine's limits.
        let new = !pkg.handover;
        let seed = session::Seed { events: pkg.events, state: pkg.state, items, from: pkg.from, from_cwd: pkg.from_cwd, picks_up: pkg.picks_up, note, restored: false, synced: pkg.synced, local: pkg.handover };
        // Taken over, written so before anything of it runs here: no other process of the machine
        // takes this try over, and the one that handed it over doesn't go on with it.
        taken(&id)?;
        self.open_seeded(Some(pkg.id), pkg.spec, Some(seed), None, kept_pairs(pkg.inbox, pkg.inbox_digests), false, new).map_err(|e| Refused::NotTaken(format!("{e:#}")))?;
        // Its record written as it arrives, with the ids it brought until they're taken in: a
        // restart a moment later still has them (CP-T3.2).
        self.save_now();
        // Kept past its end here, and past this runner process (blue/green), so this hand-over
        // sent again is answered as taken.
        let noted = match (locked, &self.handovers) {
            (Some(locked), _) => locked.note(&id, epoch, self.clock.unix_ms()),
            (None, Some(h)) => h.note(&id, epoch, self.clock.unix_ms()),
            (None, None) => Ok(()),
        };
        if let Err(e) = noted {
            tracing::warn!(target: "kiln.sends", event = "adopt_unnoted", session = %id, error = %e, "a hand-over taken over couldn't be written down: this process still knows it, and writes it with the next; restarted first, it wouldn't know it (sent again after the session left, it would be taken over again)");
        }
        Ok(())
    }

    /// Session `id` was taken over here at `epoch` or later: it runs here so, or this machine's
    /// note of the hand-overs it took says it was (whatever became of it since). The note
    /// unreadable now, that can't be said.
    fn taken_before(&self, id: &str, epoch: u64) -> Result<bool, Refused> {
        if self.get(id).is_some_and(|s| s.epoch() >= epoch) {
            return Ok(true);
        }
        let Some(h) = &self.handovers else { return Ok(false) };
        match h.taken(id) {
            Ok(at) => Ok(at.is_some_and(|at| at >= epoch)),
            Err(e) => Err(Refused::Unsure(format!("whether it was taken over here before can't be read now: {e}"))),
        }
    }

    /// Let go of a session that went on on another machine: it is no longer this machine's, and it
    /// isn't closed (its viewers follow it there). Its inbox here takes nothing more for it.
    pub fn release(&self, id: &str) {
        let gone = self.sessions.lock().remove(id);
        self.end_lane(id, Lane::Fence { epoch: gone.as_ref().map_or(0, |s| s.epoch()), handover: false });
        self.leave(id);
        if gone.is_some() {
            self.mark_dirty();
        }
    }

    /// What session `id` reaches its host through: its attached files, branches, letting go, and its
    /// inbox's ids, fence and hand-overs' record. Its host gone, the session can neither read its
    /// inbox's ids nor fence it (CP-T3.2): a hand-over or move of it stays, never going without them;
    /// nor read how a try at handing it over was decided, so it never goes on as if it was refused.
    fn files_for(&self, id: &str) -> session::Files {
        let me = self.me.clone();
        let me2 = self.me.clone();
        let me3 = self.me.clone();
        let me4 = self.me.clone();
        let me5 = self.me.clone();
        let me6 = self.me.clone();
        session::Files {
            dir: self.files_dir.join(id),
            take: Arc::new(move |f: &str| me.upgrade().and_then(|h| h.take_file(f))),
            // A branch: another session on this machine.
            spawn: Arc::new(move |spec: Spec| {
                if let Some(h) = me2.upgrade()
                    && let Err(e) = h.open(None, spec)
                {
                    tracing::warn!(error = %format!("{e:#}"), "could not branch an agent session");
                }
            }),
            release: Arc::new(move |id: &str| {
                if let Some(h) = me3.upgrade() {
                    h.release(id);
                }
            }),
            carried: Arc::new(move |id: String| {
                let h = me4.upgrade();
                Box::pin(async move {
                    match h {
                        Some(h) => h.carried(&id).await.map_err(|e| e.to_string()),
                        None => Err(HOST_GONE.into()),
                    }
                })
            }),
            fence: Arc::new(move |id: String, on: bool| {
                let h = me5.upgrade();
                Box::pin(async move {
                    match h {
                        Some(h) => h.moving_fence(&id, on).await.map_err(|e| e.to_string()),
                        None => Err(HOST_GONE.into()),
                    }
                })
            }),
            settle: Arc::new(move |attempt: String, id: String, take_back: bool| {
                let h = me6.upgrade();
                Box::pin(async move {
                    match h {
                        Some(h) => crate::rt::blocking(move || h.settle(&attempt, &id, take_back)).await.map_err(|e| format!("{e:#}")).and_then(|r| r),
                        None => Err(HOST_GONE.into()),
                    }
                })
            }),
            room: {
                let (me, id) = (self.me.clone(), id.to_owned());
                Arc::new(move || me.upgrade().is_none_or(|h| h.room_for(&id)))
            },
            work: {
                let (me, id) = (self.me.clone(), id.to_owned());
                Arc::new(move |work, waits| {
                    if let Some(h) = me.upgrade() {
                        h.stands(&id, work, waits);
                    }
                })
            },
        }
    }

    /// `brought`: the ids of the messages it had where it was before (its inbox's newest, from a
    /// move or blue/green), taken by its inbox here; a seed brings more (its queue and its
    /// timeline's messages). `back`: this host takes it back at the epoch it left at (blue/green
    /// that never happened), lifting its inbox's fence. `new`: it's new to this machine, so held to
    /// its limits; one it ran already (continued after a restart, taken back, handed over by
    /// another of its runner processes) never is, whatever its limits say now (`super::limit`).
    #[allow(clippy::too_many_arguments, reason = "each says how the session comes to be here")]
    fn open_seeded(&self, id: Option<String>, spec: Spec, seed: Option<session::Seed>, handed: Option<Box<session::Handed>>, brought: Vec<(String, String)>, back: bool, new: bool) -> anyhow::Result<Arc<Session>> {
        if Kind::get(&spec.kind).is_none() {
            anyhow::bail!("unknown harness {:?}", spec.kind);
        }
        let id = id.unwrap_or_else(crate::util::new_id);
        {
            let g = self.sessions.lock();
            if let Some(s) = g.get(&id) {
                return Ok(Arc::clone(s));
            }
            // A new one goes to work at once (its harness starting, its first message): it takes a
            // place, refused while the machine is full. Those at work count, and those waiting for
            // room; finished, ready, paused and ended ones don't.
            if new {
                let mut r = self.room.lock();
                let (working, waiting, max) = (r.working.len(), r.waiting.len(), r.limits.per_machine);
                if working + waiting >= max {
                    let waiting = if waiting == 0 { String::new() } else { format!(" and {waiting} waiting for room") };
                    anyhow::bail!(
                        "this machine already has {working} agent{} at work{waiting} (its limit is {max}): wait for one to finish, or raise the limit on the Runners page or with [agent] max_agent_sessions",
                        if working == 1 { "" } else { "s" },
                    );
                }
                r.working.insert(id.clone());
            }
        }
        let me = self.me.clone();
        let on_change: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(h) = me.upgrade() {
                h.mark_dirty();
            }
        });
        let files = self.files_for(&id);
        // What its timeline and queue show first, what its inbox took last (in its own order, the
        // newest last): past its inbox's bound, the newest are the ones its inbox here keeps.
        let brought = match &seed {
            Some(sd) => {
                let mut all = brought_ids(sd);
                all.extend(brought);
                all
            }
            None => brought,
        };
        // Whatever is sent to it waits until it's in and its lane is open, its first word there.
        let mut lanes = self.lanes.lock();
        // Started meanwhile (by another call, its lock first): that one.
        if let Some(s) = self.sessions.lock().get(&id) {
            return Ok(Arc::clone(s));
        }
        let started = match handed {
            Some(h) => session::start_handed(id.clone(), spec, self.sink.clone(), on_change, files, h),
            None => session::start_seeded(id.clone(), spec, self.sink.clone(), on_change, files, seed),
        };
        let s = match started {
            Ok(s) => s,
            Err(e) => {
                // Not started: the place it took goes.
                if new {
                    self.leave(&id);
                }
                return Err(e);
            }
        };
        let epoch = s.epoch();
        // The ids it brought wait in its inbox until they're taken in (its lane's first word): its
        // record keeps them meanwhile, from the first time it's written, so a restart before then
        // brings them again.
        if let Some(inbox) = &self.inbox {
            inbox.bring(&id, &brought);
        }
        self.sessions.lock().insert(id.clone(), Arc::clone(&s));
        self.open_lane(&mut lanes, &id, epoch, back);
        drop(lanes);
        self.mark_dirty();
        self.watch();
        Ok(s)
    }

    /// Hands `c` to session `id`; see [`Host::command_at`].
    pub fn command(&self, id: &str, c: Command) -> bool {
        self.command_at(id, c, None)
    }

    /// Hands `c` to session `id`, sent for its `epoch` when the sender says so (the server, from what
    /// it last heard of the session): after whatever it was told before, in order. A message goes
    /// through its inbox first, and what became of it is said back ([`Update::Inbox`]). False when
    /// there's no session `id` here, or its lane holds its most (a message is answered `Full`).
    pub fn command_at(&self, id: &str, c: Command, epoch: Option<u64>) -> bool {
        let Some(lane) = self.lanes.lock().get(id).map(|(lane, _)| lane.clone()) else { return false };
        match lane.try_send(Lane::Cmd { c: Box::new(c), epoch }) {
            Ok(()) => true,
            Err(async_channel::TrySendError::Full(Lane::Cmd { c, .. })) => {
                if let Some((message, _)) = message_of(&c) {
                    let said = Said { message, answer: Err(inbox::Refusal::Full { session: id.to_owned(), limit: LANE }) };
                    let (sink, id) = (self.sink.clone(), id.to_owned());
                    crate::rt::spawn(async move {
                        let _ = sink.send((id, Update::Inbox { said: Box::new(said) })).await;
                    });
                }
                false
            }
            Err(_) => false,
        }
    }

    /// Opens session `id`'s lane (with `lanes` held), its first word that the session is here at
    /// `epoch` (`back`: taken back). It goes once its last lane is done.
    fn open_lane(&self, lanes: &mut HashMap<String, (async_channel::Sender<Lane>, async_channel::Receiver<()>)>, id: &str, epoch: u64, back: bool) {
        let (tx, rx) = async_channel::bounded(LANE);
        let _ = tx.try_send(Lane::Open { epoch, back });
        // Dropped as the lane ends: what waits on it goes on.
        let (done, ended) = async_channel::bounded::<()>(1);
        lanes.insert(id.to_owned(), (tx, ended.clone()));
        let before = self.ending.lock().remove(id);
        let (me, id) = (self.me.clone(), id.to_owned());
        crate::rt::spawn(async move {
            if let Some(before) = before {
                let _ = before.recv().await;
            }
            while let Ok(item) = rx.recv().await {
                let Some(h) = me.upgrade() else { return };
                h.step(&id, item).await;
            }
            drop(done);
            if let Some(h) = me.upgrade() {
                let mut ending = h.ending.lock();
                if ending.get(&id).is_some_and(|e| e.same_channel(&ended)) {
                    ending.remove(&id);
                }
            }
        });
    }

    /// Session `id`'s lane ends with `last` (its inbox fenced, or forgotten), once what's ahead of
    /// it is done.
    fn end_lane(&self, id: &str, last: Lane) {
        let Some((lane, ended)) = self.lanes.lock().remove(id) else { return };
        self.ending.lock().insert(id.to_owned(), ended);
        crate::rt::spawn(async move {
            let _ = lane.send(last).await;
        });
    }

    /// Once everything session `id`'s lane holds now is done (a message its inbox took and its
    /// session then refused is taken back by then).
    async fn settled(&self, id: &str) {
        let Some(lane) = self.lanes.lock().get(id).map(|(lane, _)| lane.clone()) else { return };
        let (tx, rx) = futures::channel::oneshot::channel();
        if lane.send(Lane::Mark(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Session `id`'s inbox fenced as it moves to another machine, before its conversation goes
    /// (`on`), or the fence lifted: its move failed, and it goes on here.
    pub async fn moving_fence(&self, id: &str, on: bool) -> Result<(), inbox::Refusal> {
        let (Some(inbox), Some(s)) = (&self.inbox, self.get(id)) else { return Ok(()) };
        let epoch = s.epoch();
        if on { inbox.fence(id, epoch).await } else { inbox.take_back(id, epoch).await.map(|_| ()) }
    }

    /// The ids of the messages session `id` took that go with it when it moves: its inbox's newest,
    /// once what its lane holds is done (a message its inbox took and its session then refused, as
    /// it moves, is taken back by then). None without an inbox. Refused when they can't be read, or
    /// while its lane isn't done within [`SETTLE_WAIT`]: an id read then may be one its inbox takes
    /// back after, which would go without its message, and a retry of that message would be taken
    /// where it went as a repeat and never delivered (the hand-over review's point 2).
    pub async fn carried(&self, id: &str) -> Result<Vec<(String, String)>, inbox::Refusal> {
        let Some(inbox) = &self.inbox else { return Ok(Vec::new()) };
        if crate::rt::timeout(SETTLE_WAIT, self.settled(id)).await.is_err() {
            tracing::warn!(target: "kiln.sends", event = "settle_timed_out", session = %id, "a moving session's lane didn't settle in time: it doesn't go, so no id goes without its message");
            return Err(inbox::Refusal::Unavailable { session: id.to_owned(), why: "a message it was being sent is still being taken".to_owned() });
        }
        carry(inbox, id).await
    }

    /// One thing down session `id`'s lane.
    async fn step(&self, id: &str, item: Lane) {
        match item {
            Lane::Cmd { c, epoch } => self.take(id, *c, epoch).await,
            Lane::Mark(done) => {
                let _ = done.send(());
            }
            Lane::Open { epoch, back } => {
                if let Some(inbox) = &self.inbox {
                    arrive(inbox, id, epoch, &[], back, self.clock.unix_ms()).await;
                    // What it brought may have taken it past its bound.
                    let _ = inbox.tidy(id).await;
                }
            }
            // Only for a session not here (again) by now.
            Lane::Fence { epoch, handover } => {
                if let Some(inbox) = &self.inbox
                    && self.get(id).is_none()
                {
                    if let Err(e) = inbox.fence_for(id, epoch, handover).await {
                        tracing::warn!(target: "kiln.sends", event = "inbox_fence_failed", session = %id, error = %e, "a session's inbox couldn't be fenced after it went on elsewhere");
                    }
                    // What it brought and hadn't taken in went with it.
                    inbox.let_go(id);
                }
            }
            Lane::Forget => {
                if let Some(inbox) = &self.inbox
                    && self.get(id).is_none()
                    && let Err(e) = inbox.forget(id).await
                {
                    tracing::warn!(target: "kiln.sends", event = "inbox_forget_failed", session = %id, error = %e, "an ended session's inbox couldn't be let go of");
                }
            }
        }
    }

    /// Command `c` for session `id`, in its turn: a message through its inbox, and on to the
    /// session if it's new there; anything else straight on.
    async fn take(&self, id: &str, c: Command, epoch: Option<u64>) {
        let s = self.get(id);
        // Held (a new build not promoted yet): a message is refused before anything takes it.
        if self.is_held()
            && let Some(s) = &s
            && let Some((message, _)) = message_of(&c)
        {
            self.say(id, Said { message, answer: Err(inbox::Refusal::Moved { session: id.to_owned(), epoch: s.epoch() }) }).await;
            return;
        }
        let at = self.clock.unix_ms();
        if let Some(said) = take_message(self.inbox.as_deref(), id, s.as_deref(), c, epoch, at).await {
            self.say(id, said).await;
            // Its answer gone: an inbox past its bound lets its oldest ids go now, off the answer's way.
            if let Some(inbox) = &self.inbox {
                let _ = inbox.tidy(id).await;
            }
        }
    }

    /// Tells whoever sent a message to session `id` what became of it.
    async fn say(&self, id: &str, said: Said) {
        let _ = self.sink.send((id.to_owned(), Update::Inbox { said: Box::new(said) })).await;
    }

    pub fn close(&self, id: &str) -> bool {
        let s = self.sessions.lock().remove(id);
        self.end_lane(id, Lane::Forget);
        self.leave(id);
        let found = s.is_some();
        if let Some(s) = s {
            s.close();
            // A workspace of its own ends with it.
            let spec = s.spec.lock().clone();
            crate::workspace::release(&spec);
            // Its attachments go with it.
            let dir = self.files_dir.join(id);
            crate::rt::spawn(async move {
                let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
            });
        }
        self.mark_dirty();
        found
    }

    /// Stop every harness process now (this process is about to end, and a fresh one continues
    /// the sessions from the record saved here: an upgrade by a successor on macOS). Blocks up to
    /// a few seconds.
    pub fn stop_all(&self) {
        self.save_now();
        let pids: Vec<u32> = self.sessions.lock().values().filter_map(|s| *s.pid.lock()).collect();
        for pid in pids {
            stop_process(pid);
            // The process that continues them won't wait for it: reaped now.
            #[cfg(unix)]
            let _ = crate::os::try_reap(pid);
        }
    }

    /// Hand every session's harness process, still running, to this process's next image (it
    /// re-executes itself next: an in-place upgrade), where each goes on as it was, mid-turn
    /// included ([`Host::restore`]). One that can't be handed over is stopped, and starts again
    /// there from its record (its conversation goes on). Returns how many were handed over.
    pub async fn hand_over_all(&self) -> usize {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        let handed = futures::future::join_all(sessions.iter().map(|s| async move { (Arc::clone(s), crate::rt::timeout(Duration::from_secs(15), s.hand_over()).await.ok().flatten()) })).await;
        let mut out: HashMap<String, Box<session::Handed>> = HashMap::new();
        for (s, h) in handed {
            match h {
                Some(h) => {
                    out.insert(s.id.clone(), h);
                }
                None => {
                    if let Some(pid) = s.pid.lock().take() {
                        stop_process(pid);
                        #[cfg(unix)]
                        let _ = crate::os::try_reap(pid);
                    }
                }
            }
        }
        self.save_now();
        let n = out.len();
        if let Some(path) = self.handover_path() {
            let wrote = serde_json::to_vec(&out).map_err(std::io::Error::other).and_then(|json| crate::util::write_secret(&path, &json, 0o600));
            if let Err(e) = wrote {
                // The next image can't take them: stopped, and they start again from their records.
                tracing::warn!(error = %e, file = %path.display(), "couldn't write the agent sessions' handover: they start again after the upgrade");
                for h in out.values() {
                    stop_process(h.process.pid);
                    #[cfg(unix)]
                    let _ = crate::os::try_reap(h.process.pid);
                }
                return 0;
            }
            // Their pipes kept open across the exec that comes next, only now: a process started
            // while they were handed over would hold them for good.
            for (id, h) in &out {
                if let Err(e) = h.process.keep_across_exec() {
                    tracing::warn!(session = %id, error = %e, "an agent session's pipes can't be kept across the upgrade: it starts again after it");
                }
            }
        }
        n
    }

    /// Every session, handed to another runner process of this machine (a blue/green upgrade,
    /// `crate::bluegreen`): each with its spec and, still running, its harness process (`None`:
    /// it couldn't be handed over, was stopped, and starts again there from its spec). They are no
    /// longer this runner's.
    pub async fn move_out(&self) -> Vec<(Saved, Option<Box<session::Handed>>)> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        // They take no messages as they're handed over: sent again, those go where they went.
        for s in &sessions {
            s.stop_taking();
        }
        let handed = futures::future::join_all(sessions.iter().map(|s| async move { (Arc::clone(s), crate::rt::timeout(Duration::from_secs(15), s.hand_over()).await.ok().flatten()) })).await;
        // What their lanes hold is done first, all at once and for a while at most.
        if crate::rt::timeout(SETTLE_WAIT, futures::future::join_all(sessions.iter().map(|s| self.settled(&s.id)))).await.is_err() {
            tracing::warn!(target: "kiln.sends", event = "settle_timed_out", "the sessions' lanes didn't settle in time as they were handed over: the ids their inboxes took go as they stand");
        }
        let mut out = Vec::new();
        for (s, h) in handed {
            if h.is_none()
                && let Some(pid) = s.pid.lock().take()
            {
                stop_process(pid);
                #[cfg(unix)]
                let _ = crate::os::try_reap(pid);
            }
            let mut rec = record(&s);
            // What its inbox took goes with it (its newest ids), and this one takes nothing more for
            // it. Its harness is handed over already, so it goes either way: its inbox unreadable
            // (tried a few times), it takes the ids it holds itself, said.
            if let Some(inbox) = &self.inbox {
                let mut kept = Err(inbox::Refusal::Unavailable { session: s.id.clone(), why: "not read yet".into() });
                for wait in [0u64, 50, 250] {
                    crate::rt::sleep(Duration::from_millis(wait)).await;
                    kept = carry(inbox, &s.id).await;
                    if kept.is_ok() {
                        break;
                    }
                }
                let kept = kept.unwrap_or_else(|_| {
                    tracing::warn!(target: "kiln.sends", event = "carry_own_ids", session = %s.id, "a session handed to the next runner process goes with the ids it holds itself (its queue, its timeline, those it brought and hasn't taken in): its inbox couldn't be read");
                    // Those it brought last: it took nothing since (it refuses messages until
                    // they're in), so they're its newest.
                    let mut ids = own_ids(&s);
                    ids.extend(inbox.waiting(&s.id));
                    ids
                });
                (rec.inbox, rec.inbox_digests) = split_kept(kept);
            }
            self.sessions.lock().remove(&s.id);
            self.leave(&s.id);
            // A handover's fence: started again with its session (the handover never finished),
            // this runner goes on with it.
            self.end_lane(&s.id, Lane::Fence { epoch: s.epoch(), handover: true });
            out.push((rec, h));
        }
        self.mark_dirty();
        out
    }

    /// Take over a session another runner process of this machine handed over (blue/green): with
    /// its process (its descriptors are this process's now), or started again from its record (a
    /// turn its stop cut short is picked up first).
    pub fn move_in(&self, rec: Saved, handed: Option<Box<session::Handed>>) -> anyhow::Result<()> {
        match handed {
            Some(h) => {
                let (harness, ran) = (rec.harness.clone(), rec.ran.clone());
                let session = self.open_seeded(Some(rec.id), rec.spec, None, Some(h), kept_pairs(rec.inbox, rec.inbox_digests), true, false)?;
                // What its harness runs stays under it; the watch finds the rest again.
                *session.ran.lock() = (harness, ran);
                Ok(())
            }
            None => {
                let outlived = super::survivors::alive(&rec.harness.iter().cloned().collect::<Vec<_>>());
                if !outlived.is_empty() {
                    super::survivors::stop(&outlived.iter().map(|r| r.pid).collect::<Vec<_>>());
                }
                let found = super::survivors::scan(&[(rec.id.clone(), None)]);
                let apart = found.get(&rec.id).map(|f| f.apart.clone()).unwrap_or_default();
                let now_s = u64::try_from(self.clock.unix_s()).unwrap_or(0);
                self.open_record(rec, None, apart, now_s, true).map(|_| ())
            }
        }
    }

    /// The image the sessions were handed to never came (the exec failed): this one goes on with
    /// them, their processes included.
    pub fn take_back(&self) {
        self.sessions.lock().clear();
        // Each says how it stands again as it goes on.
        {
            let mut r = self.room.lock();
            r.working.clear();
            r.waiting.clear();
        }
        // Their lanes end as what they hold is done, and each session's next lane waits for its
        // last: what it was sent before goes first.
        let lanes: Vec<(String, (async_channel::Sender<Lane>, async_channel::Receiver<()>))> = self.lanes.lock().drain().collect();
        {
            let mut ending = self.ending.lock();
            for (id, (_, ended)) in lanes {
                ending.insert(id, ended);
            }
        }
        self.restore();
    }

    /// Where the sessions handed to the next image are written: beside their record.
    fn handover_path(&self) -> Option<PathBuf> {
        self.file.as_ref().map(|f| f.with_file_name("agent-handover.json"))
    }

    /// The sessions the previous image of this process handed over ([`Host::hand_over_all`]),
    /// once: the file goes as it's read.
    fn take_handover(&self) -> HashMap<String, Box<session::Handed>> {
        let Some(path) = self.handover_path() else { return HashMap::new() };
        let Ok(raw) = std::fs::read(&path) else { return HashMap::new() };
        let _ = std::fs::remove_file(&path);
        serde_json::from_slice(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, file = %path.display(), "unreadable agent session handover: they start again");
            HashMap::new()
        })
    }

    /// Continue the sessions recorded before a restart (`restore_from`, without the conversation
    /// store).
    pub fn restore(&self) {
        self.restore_from(None);
    }

    /// Continue the sessions recorded before a restart. Those the previous image of this process
    /// handed over go on with their processes, still running. The rest start again, each with its
    /// timeline and state as the conversation store kept them (`crate::convo`), when it kept any;
    /// otherwise as before (a Claude Code session's history from its transcript).
    pub fn restore_from(&self, store: Option<&crate::convo::store::Store>) {
        tracing::info!(how = %crate::os::contain::how(), "agents' processes are held together by");
        let mut handed = self.take_handover();
        let saved: Vec<Saved> = match self.file.as_ref().map(|f| (f, std::fs::read(f))) {
            Some((file, Ok(raw))) => serde_json::from_slice(&raw).unwrap_or_else(|e| {
                tracing::warn!(error = %e, file = %file.display(), "unreadable agent session record; starting without it");
                Vec::new()
            }),
            _ => Vec::new(),
        };
        // A harness that outlived the runner it ran under (a crash, the system stopping only the
        // runner) is stopped first, so no second one resumes its conversation beside it; what it
        // started is kept, and taken back over with the rest.
        let outlived: Vec<super::survivors::Ran> =
            super::survivors::alive(&saved.iter().filter(|s| !handed.contains_key(&s.id)).filter_map(|s| s.harness.clone()).collect::<Vec<_>>());
        if !outlived.is_empty() {
            tracing::info!(processes = ?outlived.iter().map(|r| r.pid).collect::<Vec<_>>(), "agent harnesses outlived their runner: stopped before their sessions resume");
            super::survivors::stop(&outlived.iter().map(|r| r.pid).collect::<Vec<_>>());
        }
        let now_s = u64::try_from(self.clock.unix_s()).unwrap_or(0);
        // What the sessions started that still runs: found by their mark, and as recorded.
        let found = super::survivors::scan(&saved.iter().map(|s| (s.id.clone(), None)).collect::<Vec<_>>());
        for s in saved {
            let id = s.id.clone();
            if let Some(h) = handed.remove(&id) {
                let pid = h.process.pid;
                // With the ids it brought and hasn't taken in, which its record keeps (CP-T3.2).
                match self.open_seeded(Some(s.id.clone()), s.spec.clone(), None, Some(h), kept_pairs(s.inbox.clone(), s.inbox_digests.clone()), false, false) {
                    Ok(session) => {
                        // What its harness runs stays under it; the watch finds the rest again.
                        *session.ran.lock() = (s.harness.clone(), s.ran.clone());
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(session = %id, error = %format!("{e:#}"), "could not go on with an agent session's process: it starts again");
                        stop_process(pid);
                    }
                }
            }
            let seed = store.and_then(|st| kept_seed(st, &id));
            let from_store = seed.is_some();
            let cut = s.cut;
            let apart = found.get(&id).map(|f| f.apart.clone()).unwrap_or_default();
            match self.open_record(s, seed, apart, now_s, false) {
                Ok(left) => tracing::info!(session = %id, from_store, cut, left_running = left, "agent session continued after a restart"),
                Err(e) => tracing::warn!(session = %id, error = %format!("{e:#}"), "could not continue an agent session"),
            }
        }
        // Handed over without a record to go on from: nothing goes on with them.
        for h in handed.into_values() {
            stop_process(h.process.pid);
        }
    }

    /// Start a session again from its record: its queue and pause as they were and, when a turn of
    /// its was cut short, that turn picked up first.
    /// `apart`: what it started that still runs on its own, found by its mark (`survivors::scan`).
    /// What it started that still runs (that, and what its record says) is taken back over as
    /// left running. How many such were found. `back`: this host takes it back (blue/green that
    /// never happened).
    fn open_record(&self, s: Saved, seed: Option<session::Seed>, apart: Vec<super::survivors::Ran>, now_s: u64, back: bool) -> anyhow::Result<usize> {
        let cut = s.cut;
        // What it started that still runs, taken back over (its stopped harness not among it).
        let mut alive = apart;
        for r in super::survivors::alive(&s.ran).into_iter().filter(|r| !super::survivors::login_agent(&r.command)) {
            if !alive.iter().any(|a| a.pid == r.pid) {
                alive.push(r);
            }
        }
        if let Some(h) = &s.harness {
            alive.retain(|r| r.pid != h.pid);
        }
        let trees = super::survivors::trees(&alive);
        let session = self.open_seeded(Some(s.id), s.spec, seed, None, kept_pairs(s.inbox, s.inbox_digests), back, false)?;
        let paused = s.pause.is_some() || s.pause_at.is_some() || s.manual_resume || s.move_on_limit;
        if !trees.is_empty() {
            session.ran.lock().1.clone_from(&alive);
            *session.left.lock() = trees.iter().map(|(root, _)| root.clone()).collect();
        }
        // Shown once each, and what it showed as left running that has ended since, ended.
        let apart = alive.iter().map(|r| r.pid).collect();
        let mut core = session.core.lock();
        if settle_left(&mut core.state.tasks, trees.iter().map(|(root, _)| super::survivors::task(root)).collect(), &apart, now_s) {
            core.touch_state();
        }
        drop(core);
        if !s.queue.is_empty() || paused || cut {
            let mut core = session.core.lock();
            core.state.queue = s.queue;
            // A check under way when it stopped is asked again.
            core.state.pause = s.pause.map(|p| super::model::Pause { checking: None, ..p });
            core.state.pause_at = s.pause_at;
            core.state.manual_resume = s.manual_resume;
            core.state.move_on_limit = s.move_on_limit;
            core.state.move_plan = s.move_plan;
            // The turn the restart cut short goes on first, in kiln's words, with what it started
            // that still runs.
            if cut {
                picks_up(&mut core.state.queue, &super::survivors::said(&trees, now_s));
            }
            core.touch_state();
        }
        Ok(trees.len())
    }

    /// Every [`WATCH_EVERY`] by its clock, what each session's harness runs: noted (and kept in the
    /// record when it changes), and what it started that runs on once no longer under it (a command
    /// that daemonized, a harness started again) taken over as left running, until it ends.
    fn watch(&self) {
        if self.watching.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.me.clone();
        let clock = Arc::clone(&self.clock);
        crate::rt::spawn(async move {
            loop {
                clock.sleep(WATCH_EVERY).await;
                let Some(h) = me.upgrade() else { return };
                let sessions: Vec<Arc<Session>> = h.sessions.lock().values().cloned().collect();
                let which: Vec<(String, Option<u32>)> = sessions.iter().map(|s| (s.id.clone(), *s.pid.lock())).collect();
                let known: Vec<super::survivors::Ran> = sessions.iter().flat_map(|s| s.ran.lock().1.clone()).collect();
                let now_s = u64::try_from(clock.unix_s()).unwrap_or(0);
                // Agents between turns whose harness runs: what their commands left running apart
                // from it (`nohup … &`, a daemon, what a harness before this one started) goes now.
                // Their harness's own background tasks run under it, and end with the agent.
                let settled: std::collections::HashSet<String> = sessions.iter().filter(|s| s.pid.lock().is_some() && super::session::settled(s)).map(|s| s.id.clone()).collect();
                let adopted: Vec<super::survivors::Ran> = sessions.iter().filter(|s| settled.contains(&s.id)).flat_map(|s| s.left.lock().clone()).collect();
                let look = move || {
                    let seen = super::survivors::scan(&which);
                    // Shells' ssh-agents are no session's work: stopped once old enough.
                    let (agents, leftovers) = super::survivors::to_stop(&seen, &settled, now_s);
                    if !agents.is_empty() {
                        tracing::info!(count = agents.len(), "stopping the ssh-agents shells' startup files left in agents' sessions");
                        super::survivors::stop(&agents);
                    }
                    if !leftovers.is_empty() || !adopted.is_empty() {
                        let what: Vec<String> = seen.iter().filter(|(id, _)| settled.contains(*id)).flat_map(|(_, s)| &s.apart).chain(&adopted).take(8).map(|r| format!("{} {}", r.pid, super::survivors::shown(&r.command))).collect();
                        tracing::info!(count = leftovers.len() + adopted.len(), ?what, "stopping what agents' commands left running, now that they're between turns");
                        super::survivors::stop(&leftovers);
                        adopted.iter().for_each(super::survivors::stop_tree);
                    }
                    (seen, super::survivors::alive(&known))
                };
                let Ok((seen, alive)) = crate::rt::blocking(look).await else { continue };
                let alive: HashMap<u32, u64> = alive.into_iter().map(|r| (r.pid, r.started)).collect();
                let mut changed = false;
                for s in &sessions {
                    if h.note(s, seen.get(&s.id).cloned().unwrap_or_default(), &alive, now_s) {
                        changed = true;
                    }
                }
                if changed {
                    h.mark_dirty();
                }
                h.looks.fetch_add(1, Ordering::AcqRel);
                h.metrics.harness_polls.inc();
                h.changed.notify(usize::MAX);
            }
        });
    }

    /// One session's look (`watch`): whether what's kept of it changed. What it started that runs
    /// apart from its harness (marked as its, or as recorded and still running) is left running.
    fn note(&self, s: &Session, seen: super::survivors::Seen, alive: &HashMap<u32, u64>, now_s: u64) -> bool {
        let running = s.pid.lock().is_some();
        let mut ran = s.ran.lock();
        let still = |r: &super::survivors::Ran| alive.get(&r.pid) == Some(&r.started);
        // Everything under it now, not only what's noted (a record keeps so many).
        let under = |pid: u32| seen.below.contains(&pid) || seen.under.iter().any(|r| r.pid == pid);
        let mut apart = seen.apart;
        // As recorded, still running, neither under it nor marked (it cleared its environment).
        for r in ran.1.iter().filter(|r| !under(r.pid) && still(r) && !super::survivors::login_agent(&r.command)) {
            if !apart.iter().any(|a| a.pid == r.pid) {
                apart.push(r.clone());
            }
        }
        let now_apart: std::collections::HashSet<u32> = apart.iter().map(|r| r.pid).collect();
        let mut left = s.left.lock();
        let mut new_left = Vec::new();
        for (root, _) in super::survivors::trees(&apart) {
            if !left.iter().any(|l| l.pid == root.pid) {
                new_left.push(root.clone());
                left.push(root);
            }
        }
        // Left running that ended.
        left.retain(|l| now_apart.contains(&l.pid));
        let mut next = (if running { seen.harness } else { ran.0.clone() }, seen.under);
        next.1.extend(apart);
        let changed = *ran != next;
        *ran = next;
        drop((ran, left));
        let taken = new_left.iter().map(|root| super::model::Task { summary: Some(format!("Left running, apart from the agent (process {})", root.pid)), ..super::survivors::task(root) }).collect();
        let mut core = s.core.lock();
        if settle_left(&mut core.state.tasks, taken, &now_apart, now_s) {
            core.touch_state();
        }
        changed
    }

    fn mark_dirty(&self) {
        if self.file.is_none() || self.dirty.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.me.clone();
        let clock = Arc::clone(&self.clock);
        crate::rt::spawn(async move {
            // Coalesce bursts (a session's id arriving, its queue changing) into one write.
            clock.sleep(SAVE_AFTER).await;
            if let Some(h) = me.upgrade() {
                h.dirty.store(false, Ordering::Release);
                let _ = crate::rt::blocking(move || h.save_now()).await;
            }
        });
    }

    /// `s`'s record ([`record`]), with the ids it brought that its inbox hasn't taken in yet
    /// ([`Inbox::waiting`]): a restart before they're in brings them again, so a message it took
    /// where it was, sent again here, is still its repeat.
    fn recorded(&self, s: &Session) -> Saved {
        let mut rec = record(s);
        if let Some(inbox) = &self.inbox {
            let waiting = inbox.waiting(&s.id);
            if !waiting.is_empty() {
                (rec.inbox, rec.inbox_digests) = split_kept(waiting);
            }
        }
        rec
    }

    fn save_now(&self) {
        let Some(file) = &self.file else { return };
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        let saved: Vec<Saved> = sessions.iter().map(|s| self.recorded(s)).collect();
        let Ok(json) = serde_json::to_vec_pretty(&saved) else { return };
        let tmp = file.with_extension("tmp");
        let wrote = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| crate::util::write_secret(&tmp, &json, 0o600)).and_then(|()| std::fs::rename(&tmp, file));
        match wrote {
            Ok(()) => {
                self.saves.fetch_add(1, Ordering::AcqRel);
                self.metrics.harness_records_written.inc();
            }
            Err(e) => tracing::warn!(error = %e, file = %file.display(), "could not record agent sessions"),
        }
        self.changed.notify(usize::MAX);
    }
}

/// The records a host's sessions' inboxes keep, in its folder `dir`: none when `KILN_INBOX` is off,
/// or they can't be opened (said, and messages then go on unchecked).
fn open_records(dir: &std::path::Path) -> Option<Arc<dyn kiln_seams::Records>> {
    if std::env::var("KILN_INBOX").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no")) {
        tracing::info!(target: "kiln.sends", event = "inbox_off", "sessions' inboxes are off (KILN_INBOX): messages aren't checked for repeats");
        return None;
    }
    let path = dir.join(crate::records::FILE);
    match crate::records::open(&path) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(target: "kiln.sends", event = "inbox_unavailable", file = %path.display(), error = %format!("{e:#}"), "sessions' inboxes couldn't open their records: messages aren't checked for repeats");
            None
        }
    }
}

/// A message's id and digest ([`inbox::digest`]): what a session's inbox takes once. None for a
/// command that isn't a message, or a message without an id.
/// A session as the lane hands it what it's sent: `Session` here, a stand-in in the sends
/// simulation, which runs this code (`inbox::sim`).
pub(crate) trait Taker {
    /// It's on its way elsewhere: it takes no messages.
    fn is_moving(&self) -> bool;
    /// The epoch it's at.
    fn epoch(&self) -> u64;
    /// Hands `c` to it: false when it can't take it (it's on its way elsewhere, or it ended).
    fn hand(&self, c: Command) -> bool;
}

impl Taker for Session {
    fn is_moving(&self) -> bool {
        Session::is_moving(self)
    }

    fn epoch(&self) -> u64 {
        Session::epoch(self)
    }

    fn hand(&self, c: Command) -> bool {
        self.command(c)
    }
}

/// Command `c` for session `id` (held here as `s`; none: it isn't), in its turn down its lane, sent
/// for `epoch` when the sender says, at `at`: a message through its `inbox` (none: unchecked), and
/// on to the session if it's new there; anything else straight on. What to tell the message's
/// sender (none for a command that isn't a message).
pub(crate) async fn take_message<T: Taker + ?Sized>(inbox: Option<&Inbox>, id: &str, s: Option<&T>, c: Command, epoch: Option<u64>, at: i64) -> Option<Said> {
    let message = message_of(&c);
    let Some(s) = s else {
        return message.map(|(message, _)| Said { message, answer: Err(inbox::Refusal::NoSession { session: id.to_owned() }) });
    };
    let Some((message, digest)) = message else {
        s.hand(c);
        return None;
    };
    // On its way elsewhere: refused before its inbox takes it, so what goes with it stays as it
    // was when it began to move.
    if s.is_moving() {
        return Some(Said { message, answer: Err(gone(s, id)) });
    }
    let answer = match inbox {
        Some(inbox) => {
            let took = std::time::Instant::now();
            let mut answer = inbox.accept(id, &message, &digest, epoch, at).await;
            // Its inbox isn't at the epoch it's at (opening it failed): behind it, or fenced at one
            // it has left (it came back here higher). Opened now, and the message taken again.
            let lagging = match &answer {
                Err(inbox::Refusal::Behind { here, .. }) => *here < s.epoch(),
                Err(inbox::Refusal::Moved { epoch: at, .. }) => *at < s.epoch(),
                _ => false,
            };
            if lagging {
                match inbox.open(id, s.epoch()).await {
                    Ok(_) => answer = inbox.accept(id, &message, &digest, epoch, at).await,
                    Err(e) => tracing::warn!(target: "kiln.sends", event = "inbox_open_failed", session = %id, error = %e, "a session's inbox couldn't be opened: its messages are refused until it can"),
                }
            }
            crate::metrics::sends().take_seconds.observe(took.elapsed().as_secs_f64());
            if let Ok(a) = &answer
                && !a.repeat
                && !s.hand(c)
            {
                // It couldn't take it after all: it's on its way elsewhere, or it ended. Taken
                // back, so it's taken where it goes when it's sent again.
                if let Err(e) = inbox.unaccept(id, &message).await {
                    tracing::warn!(target: "kiln.sends", event = "unaccept_failed", session = %id, %message, error = %e, "a message a session couldn't take stays taken: its repeat would be answered, not delivered");
                }
                answer = Err(gone(s, id));
            }
            answer
        }
        // No inbox: on as it comes, unchecked (place 0).
        None if s.hand(c) => Ok(Answer { session: id.to_owned(), message: message.clone(), epoch: s.epoch(), at, seq: 0, repeat: false }),
        None => Err(gone(s, id)),
    };
    Some(Said { message, answer })
}

/// Session `id` is here at `epoch` (`back`: taken back by this host at the epoch it left at), with
/// the ids of the messages it `brought`: its inbox opened (or taken back), and those taken as its own.
pub(crate) async fn arrive(inbox: &Inbox, id: &str, epoch: u64, brought: &[(String, String)], back: bool, at: i64) {
    if let Err(e) = inbox.arrive(id, epoch, brought, back, at).await {
        tracing::warn!(target: "kiln.sends", event = "inbox_open_failed", session = %id, error = %e, "a session's inbox couldn't take in what it brought, or be opened: its messages are refused until it can");
    }
}

/// The ids of the messages session `id` took that go with it when it leaves, each with its digest:
/// its inbox's newest ([`inbox::KEEP`]), the oldest of them first. Refused when they can't be read:
/// a session doesn't leave without them (CP-T3.2), or a message sent again where it went would be
/// taken there as new.
pub(crate) async fn carry(inbox: &Inbox, id: &str) -> Result<Vec<(String, String)>, inbox::Refusal> {
    inbox.carried(id, usize::try_from(inbox::KEEP).unwrap_or(usize::MAX)).await.inspect_err(|e| {
        tracing::warn!(target: "kiln.sends", event = "carry_failed", session = %id, error = %e, "a leaving session's inbox couldn't be read: it doesn't leave without the ids it took");
    })
}

/// The ids of the messages session `s` holds itself (each without a digest): those in its queue,
/// and those its timeline shows it was sent. What its inbox handed it: what goes with it when its
/// inbox can't be read and it must go (handed to this machine's next runner process).
fn own_ids(s: &Session) -> Vec<(String, String)> {
    let core = s.core.lock();
    let mut ids: Vec<String> = core.events().filter_map(|e| match &e.body {
        Body::User { client_id: Some(c), .. } if !c.is_empty() => Some(c.clone()),
        _ => None,
    }).collect();
    ids.extend(core.state.queue.iter().map(|q| q.id.clone()).filter(|q| !q.is_empty()));
    ids.into_iter().map(|id| (id, String::new())).collect()
}

pub(crate) fn message_of(c: &Command) -> Option<(String, String)> {
    match c {
        Command::Send { id, text, attachments, .. } if !id.is_empty() => {
            let names: Vec<&str> = attachments.iter().map(|a| a.name.as_str()).collect();
            // A message from another agent is its words: kiln's note of who sent it changes as its
            // sender does, and a retry under its key is the same message.
            Some((id.clone(), inbox::digest(super::ops::peer_words(text), &names)))
        }
        Command::SkillRun { id: Some(id), name, args, .. } if !id.is_empty() => Some((id.clone(), inbox::digest(&format!("/{name} {}", args.as_deref().unwrap_or_default()), &[]))),
        _ => None,
    }
}

/// Why session `s` couldn't take a message its inbox took: it's on its way elsewhere, or ended.
fn gone<T: Taker + ?Sized>(s: &T, id: &str) -> inbox::Refusal {
    if s.is_moving() { inbox::Refusal::Moved { session: id.to_owned(), epoch: s.epoch() } } else { inbox::Refusal::NoSession { session: id.to_owned() } }
}

/// Carried ids with their digests, from the lists a package or a record keeps them in (`ids`, and
/// `digests` in the same order): one from a runner before digests has none, and each is empty.
fn kept_pairs(ids: Vec<String>, digests: Vec<String>) -> Vec<(String, String)> {
    let mut d = digests.into_iter();
    ids.into_iter().map(|id| (id, d.next().unwrap_or_default())).collect()
}

/// `kept` (ids with their digests) as the lists a package or a record keeps them in: no digests
/// when none is known.
pub(crate) fn split_kept(kept: Vec<(String, String)>) -> (Vec<String>, Vec<String>) {
    let digests = if kept.iter().all(|(_, d)| d.is_empty()) { Vec::new() } else { kept.iter().map(|(_, d)| d.clone()).collect() };
    (kept.into_iter().map(|(id, _)| id).collect(), digests)
}

/// The ids of the messages a session brings from where it was: those in its queue, and those its
/// timeline shows it was sent (their words as its timeline keeps them may differ from how they
/// came: no digest).
fn brought_ids(seed: &session::Seed) -> Vec<(String, String)> {
    let mut ids: Vec<String> = seed.events.iter().filter_map(|e| match &e.body {
        Body::User { client_id: Some(c), .. } if !c.is_empty() => Some(c.clone()),
        _ => None,
    }).collect();
    ids.extend(seed.state.queue.iter().map(|q| q.id.clone()).filter(|q| !q.is_empty()));
    ids.into_iter().map(|id| (id, String::new())).collect()
}

/// What a session shows as left running, against what runs apart from it now (`apart`, by pid):
/// the tasks of the roots taken over now (`taken`) shown, once each (one that came back with its
/// timeline after a restart or an upgrade is shown already), and every one shown that no longer
/// runs ended, those it came back with included, which may have ended while no runner looked.
/// Whether any changed.
fn settle_left(tasks: &mut Vec<super::model::Task>, taken: Vec<super::model::Task>, apart: &std::collections::HashSet<u32>, now_s: u64) -> bool {
    use super::survivors::LEFT;
    // A second copy of one shown goes (older images showed it again on each restart).
    let mut shown = std::collections::HashSet::new();
    let before = tasks.len();
    tasks.retain(|t| !(t.live() && t.id.starts_with(LEFT)) || shown.insert(t.id.clone()));
    let mut changed = tasks.len() != before;
    for t in taken {
        if !shown.contains(&t.id) {
            tasks.push(t);
            changed = true;
        }
    }
    let ended = i64::try_from(now_s).unwrap_or(0).saturating_mul(1000);
    for t in tasks.iter_mut().filter(|t| t.live() && t.id.strip_prefix(LEFT).and_then(|p| p.parse::<u32>().ok()).is_some_and(|p| !apart.contains(&p))) {
        t.status = "completed".to_owned();
        t.ended = Some(ended);
        changed = true;
    }
    changed
}

/// The turn a restart cut short goes on first: kiln's word (shown as kiln's), ahead of what was
/// already queued, with what it started that still runs (`also`), unless it's waiting there
/// already (a restart before it went out).
fn picks_up(queue: &mut Vec<QueueItem>, also: &str) {
    if queue.first().is_some_and(|q| q.text.starts_with(CUT_BY_RESTART)) {
        return;
    }
    let now = super::core::Core::now();
    queue.insert(0, QueueItem { id: format!("{}restart-{now}", super::core::KEEP_ID), text: format!("{CUT_BY_RESTART}{also}"), at: now, attachments: Vec::new(), overrides: None, reply_to: None });
}

/// How often what the harnesses run is looked at.
pub const WATCH_EVERY: Duration = Duration::from_secs(10);
/// How long changes are gathered before the record is written.
pub const SAVE_AFTER: Duration = Duration::from_millis(500);

/// A session's timeline and state as the conversation store kept them, for its restart: None when
/// the store kept nothing of it (or can't say in time), and the session starts as it always did.
fn kept_seed(store: &crate::convo::store::Store, id: &str) -> Option<session::Seed> {
    let kept = match store.kept(id, super::core::MAX_EVENTS) {
        Ok(Some(k)) => k,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(session = %id, error = %format!("{e:#}"), "the conversation store couldn't say what it kept of an agent session");
            return None;
        }
    };
    let state: super::model::State = kept.state.as_deref().and_then(|s| serde_json::from_str(s).ok())?;
    let mut events: Vec<super::model::Event> = kept.events.iter().filter_map(|e| serde_json::from_str(&e.body).ok()).collect();
    // Cut at the bound: from where a run of the server's digests begins, as the timeline lets go
    // of its oldest (`crate::wire::sums::run_after`).
    if kept.events.len() >= super::core::MAX_EVENTS
        && let Some(first) = events.first().map(|e| e.seq)
    {
        let from = crate::wire::sums::run_after(first);
        events.retain(|e| e.seq >= from);
    }
    if events.is_empty() {
        return None;
    }
    Some(session::Seed { events, state, items: None, from: String::new(), from_cwd: String::new(), picks_up: false, note: None, restored: true, synced: None, local: false })
}

/// SIGTERM a harness's process group, then SIGKILL it if it is still there after 3 s.
#[cfg(unix)]
pub(crate) fn stop_process(pid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let Ok(p) = i32::try_from(pid) else { return };
    if p <= 0 {
        return;
    }
    let _ = killpg(Pid::from_raw(p), Signal::SIGTERM);
    let end = std::time::Instant::now() + Duration::from_secs(3);
    let mut pause = Duration::from_millis(1);
    while std::time::Instant::now() < end {
        if !crate::os::pid_alive(pid) {
            return;
        }
        std::thread::sleep(pause);
        pause = super::survivors::next_look(pause);
    }
    let _ = killpg(Pid::from_raw(p), Signal::SIGKILL);
}

#[cfg(windows)]
pub(crate) fn stop_process(pid: u32) {
    crate::os::proc::taskkill_tree(pid);
}

/// The working directory and environment of a shell (by pid), to start an agent in.
pub fn shell_context(pid: u32) -> (Option<String>, Vec<(String, String)>) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    let pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always).with_environ(UpdateKind::Always).without_tasks(),
    );
    let Some(p) = sys.process(pid) else { return (None, Vec::new()) };
    let cwd = p.cwd().map(|c| c.to_string_lossy().into_owned()).filter(|c| !c.is_empty());
    let env = p
        .environ()
        .iter()
        .filter_map(|kv| {
            let kv = kv.to_string_lossy();
            let (k, v) = kv.split_once('=')?;
            (!k.is_empty()).then(|| (k.to_owned(), v.to_owned()))
        })
        .collect();
    (cwd, env)
}

/// Updates that show a session this machine could not start (and why), so it can be dismissed.
pub fn refusal(id: &str, spec_json: &str, error: &str) -> Vec<super::core::Update> {
    let spec: Spec = serde_json::from_str(spec_json).unwrap_or_default();
    let now = crate::util::now_ms();
    let kind = Kind::get(&spec.kind);
    let exit = super::model::ExitInfo::now(None, Some(error.to_owned()));
    let info = Info {
        id: id.to_owned(),
        kind: spec.kind.clone(),
        kind_name: kind.map_or_else(|| spec.kind.clone(), |k| k.name.to_owned()),
        target: String::new(),
        name: spec.name.clone().unwrap_or_else(|| kind.map_or_else(|| spec.kind.clone(), |k| k.name.to_owned())),
        shell: spec.shell.clone(),
        cwd: spec.cwd.clone().unwrap_or_default(),
        tree: None,
        created_at: now,
        status: Status::Exited,
        title: String::new(),
        model: String::new(),
        mode: String::new(),
        harness_session: None,
        approvals: Vec::new(),
        queued: 0,
        queue: Vec::new(),
        said: Vec::new(),
        tasks_running: 0,
        subagents: Vec::new(),
        cost_usd: 0.0,
        activity: None,
        last_activity: now,
        exit: Some(exit.clone()),
        goal: spec.prompt.clone().map(|p| super::model::clip_middle(&p, 200).0).unwrap_or_default(),
        objective: None,
        summary: String::new(),
        doing: None,
        epoch: 0,
        todos_done: 0,
        todos_total: 0,
        files_changed: 0,
        lines_added: 0,
        lines_removed: 0,
        context_pct: None,
        turn_started: None,
        limits: Vec::new(),
        account: None,
        plan: None,
        left_behind: 0,
        reach: spec.reach.clone(),
        pause: None,
        room_wait: None,
        last_turn: None,
        keep_off: false,
        held: None,
        refused: None,
        move_on_limit: false,
        move_plan: Default::default(),
        pause_at: None,
        max_subagents: None,
        parent: spec.parent.clone(),
        plan_run: spec.plan_run.clone(),
        helper: spec.helper.clone(),
        subagent: spec.subagent,
        no_bypass: !spec.allow_bypass,
        workspace: spec.workspace.clone(),
    };
    let state = super::model::State { status: Status::Exited, exit: Some(exit), reach: spec.reach.clone(), ..Default::default() };
    vec![super::core::Update::Open { info: Box::new(info), since: None }, super::core::Update::State { state: Box::new(state) }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::clock::SimClock;

    /// A host on a simulated clock of its own, never moved: its watch never looks and its record
    /// is written only when a test says (`save_now`).
    fn sim(tx: Sink, file: Option<PathBuf>, limits: Limits) -> Arc<Host> {
        Host::with_clock(tx, file, limits, Arc::new(SimClock::new()))
    }

    /// `Host::until`, failing the test rather than waiting forever when what it waits for never
    /// comes (a write that failed): 30 s, which no passing run comes near.
    async fn until(host: &Host, holds: impl Fn(&Host) -> bool) -> anyhow::Result<()> {
        crate::rt::timeout(Duration::from_secs(30), host.until(holds)).await.map_err(|_| anyhow::anyhow!("what the test waited for didn't come in 30 s"))
    }

    fn no_harness(dir: &std::path::Path) -> Spec {
        Spec {
            kind: "claude".into(),
            cwd: Some(dir.to_string_lossy().into_owned()),
            program: Some(dir.join("no-such-claude").to_string_lossy().into_owned()),
            ..Spec::default()
        }
    }

    /// Waits until `holds`: 30 s at most, which no passing run comes near.
    async fn soon(what: &str, holds: impl Fn() -> bool) -> anyhow::Result<()> {
        crate::rt::timeout(Duration::from_secs(30), async {
            while !holds() {
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("{what} never came"))
    }

    /// The watch looks at what the harnesses run every 10 s by the host's clock: not before
    /// (CP-M2's AC-M2.1: the harness host's watch on the simulated clock in tests). Each look is
    /// counted for /metrics, once (`kiln_harness_polls_total`, a non-vacuity check).
    #[test]
    fn the_watch_looks_every_10_s_by_the_host_s_clock() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let clock = SimClock::new();
            let start = clock.now();
            let (tx, _rx) = async_channel::unbounded();
            let metrics: &'static crate::metrics::AgentMetrics = Box::leak(Box::default());
            let host = Host::counting_into(tx, None, Limits::default(), Arc::new(clock.clone()), metrics);
            // A session starts the watch; closed again, a look has nothing to scan.
            host.open(Some("watched".into()), no_harness(dir.path()))?;
            assert!(host.close("watched"));
            let mut slept = 0;
            for n in 1..=2u32 {
                // The watch sleeps till its next look: 10 s on from the last, and it's the only
                // way to a look, so one that hasn't woken is one that hasn't looked.
                clock.sleeping_since(slept, 1).await;
                slept = clock.registrations();
                assert_eq!((clock.sleepers(), clock.next_deadline()), (1, Some(start + WATCH_EVERY * n)), "the watch's sleep");
                clock.advance(WATCH_EVERY.saturating_sub(Duration::from_millis(1)));
                assert_eq!((clock.sleepers(), host.looks()), (1, u64::from(n - 1)), "awake, or a look, before 10 s");
                clock.advance(Duration::from_millis(1));
                until(&host, |h| h.looks() == u64::from(n)).await?;
                assert_eq!(metrics.harness_polls.get(), u64::from(n), "each look counted");
            }
            Ok(())
        })
    }

    /// Changes in a burst are written once, half a second by the host's clock after the first
    /// (CP-M2, AC-M2.1), and the write counted for /metrics, once
    /// (`kiln_harness_records_written_total`, a non-vacuity check).
    #[test]
    fn changes_are_written_once_half_a_second_after_them_by_the_host_s_clock() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let clock = SimClock::new();
            let start = clock.now();
            let (tx, _rx) = async_channel::unbounded();
            let metrics: &'static crate::metrics::AgentMetrics = Box::leak(Box::default());
            let host = Host::counting_into(tx, Some(file.clone()), Limits::default(), Arc::new(clock.clone()), metrics);
            host.open(Some("first".into()), no_harness(dir.path()))?;
            // The watch's sleep, and the write's: the write sleeps half a second, and it's the
            // only way to the write, so one that hasn't woken is one that hasn't written.
            clock.sleeping_since(0, 2).await;
            assert_eq!((clock.sleepers(), clock.next_deadline()), (2, Some(start + SAVE_AFTER)), "the write's sleep");
            host.open(Some("second".into()), no_harness(dir.path()))?;
            clock.advance(SAVE_AFTER.saturating_sub(Duration::from_millis(1)));
            assert_eq!((clock.sleepers(), clock.next_deadline()), (2, Some(start + SAVE_AFTER)), "awake before half a second");
            assert!(host.saves() == 0 && !file.exists());
            clock.advance(Duration::from_millis(1));
            until(&host, |h| h.saves() == 1).await?;
            let saved: Vec<Saved> = serde_json::from_slice(&std::fs::read(&file)?)?;
            let mut ids: Vec<&str> = saved.iter().map(Saved::id).collect();
            ids.sort_unstable();
            assert_eq!(ids, ["first", "second"], "both changes in the one write");
            // (What the sessions do next, their harnesses failing to start, is the next burst's.)
            assert_eq!(host.saves(), 1);
            assert_eq!(metrics.harness_records_written.get(), 1, "the write counted");
            Ok(())
        })
    }

    /// A process handing everything to a new build of it takes no hand-over (it would end with
    /// it), and says so; what the server sends of another machine's sessions it still takes.
    #[test]
    fn a_process_leaving_for_its_new_build_takes_no_hand_over() -> anyhow::Result<()> {
        let (tx, _rx) = async_channel::unbounded();
        let host = sim(tx, None, Limits::default());
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("package.json");
        std::fs::write(&path, br#"{"handover": true}"#)?;
        let (handover, taken) = host.adopt_package(&path, true);
        assert!(handover && taken.as_ref().is_err_and(|e| e.contains("leaving")), "{taken:?}");
        assert!(!path.exists(), "its package goes, as any does");
        // Not a hand-over: adopted as before (this one, unreadable, isn't for its own reasons).
        std::fs::write(&path, br#"{"handover": false}"#)?;
        let (handover, taken) = host.adopt_package(&path, true);
        assert!(!handover && taken.as_ref().is_err_and(|e| !e.contains("leaving")), "{taken:?}");
        Ok(())
    }

    /// On a simulated clock of its own, as every host in these tests is (CP-M2, AC-M2.1).
    #[test]
    fn limits_and_unknown_kinds_are_refused() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (tx, _rx) = async_channel::unbounded();
            let host = sim(tx, None, Limits { per_machine: 1 });
            assert!(host.open(None, Spec { kind: "nope".into(), ..Spec::default() }).is_err());
            let dir = tempfile::tempdir()?;
            // A program that does not exist: the session reports why it is down, nothing runs.
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            let s = host.open(Some("limits".into()), spec.clone())?;
            assert_eq!(s.id, "limits");
            // The same id again is the same session. Down, it's at work no longer; another at work,
            // a new one is over the limit.
            assert_eq!(host.open(Some("limits".into()), spec.clone())?.id, "limits");
            soon("its place free once it's down", || host.room_counts() == (0, 0)).await?;
            assert!(host.room_for("elsewhere"));
            assert!(host.open(None, spec).is_err());
            assert_eq!(host.inventory().sessions, vec!["limits".to_owned()]);
            assert!(host.close("limits"));
            assert!(host.list().is_empty());
            Ok(())
        })
    }

    /// A machine's room (docs/agents.md, "How many agents a machine runs"): so many at work at
    /// once, one at work already going on; the next wait in line, the longest first, and one with
    /// nothing to go leaves it. Starting, one keeps its place; at work without asking (after a
    /// restart), it counts. Raised, there's room for the one waiting longest.
    #[test]
    fn a_machines_room_lets_so_many_at_work_and_the_rest_wait_their_turn() {
        let (tx, _rx) = async_channel::unbounded();
        let host = sim(tx, None, Limits { per_machine: 2 });
        assert!(host.room_for("a") && host.room_for("b"), "two places");
        assert!(host.room_for("a"), "at work already, it goes on");
        assert!(!host.room_for("c") && !host.room_for("d"), "full: they wait, c first");
        assert!(!host.room_for("c"), "asking again, it keeps its place in line");
        assert_eq!(host.room_counts(), (2, 2));
        host.stands("a", session::Work::No, false);
        assert!(!host.room_for("d"), "a's place is c's, which waits longer");
        assert!(host.room_for("c"));
        host.stands("d", session::Work::No, false);
        assert_eq!(host.room_counts(), (2, 0), "nothing waits to go for d: it left the line");
        assert!(!host.room_for("e"));
        host.stands("b", session::Work::Starting, true);
        assert_eq!(host.room_counts(), (2, 1), "starting, b keeps its place");
        assert!(host.set_max_sessions(3), "raised");
        assert!(host.room_for("e"), "room for e, waiting longest");
        host.stands("x", session::Work::Yes, false);
        assert_eq!(host.room_counts(), (4, 0), "at work without asking, x counts");
        host.stands("b", session::Work::No, false);
        assert!(!host.room_for("f"), "three at work: no room yet");
        host.stands("f", session::Work::No, false);
        host.stands("c", session::Work::No, false);
        assert!(host.room_for("f"), "two at work: room");
    }

    /// Only sessions at work count toward a machine's limit, and those waiting for room (docs/
    /// agents.md, "How many agents a machine runs"): while it's full a new one is refused, saying
    /// how to raise it; one that went down frees its place. Lowered under what's at work, those go
    /// on. Started again, every session it ran comes back, however few its limit says.
    #[test]
    fn only_sessions_at_work_count_toward_a_machines_limit_and_a_restart_brings_back_all_it_ran() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            {
                let (tx, _rx) = async_channel::unbounded();
                let host = sim(tx, Some(file.clone()), Limits { per_machine: 1 });
                let refused = |host: &Host, id: &str| host.open(Some(id.into()), no_harness(dir.path())).err().map(|e| format!("{e:#}")).unwrap_or_default();
                // One at work, as a session on a turn says it.
                host.stands("busy", session::Work::Yes, false);
                let why = refused(&host, "one");
                assert!(why.contains("already has 1 agent at work (its limit is 1)") && why.contains("[agent] max_agent_sessions"), "{why}");
                // Raised, one waiting for room has the place free first.
                assert!(!host.room_for("next"));
                assert!(host.set_max_sessions(2));
                assert!(!host.set_max_sessions(2), "the same again changes nothing");
                let why = refused(&host, "one");
                assert!(why.contains("already has 1 agent at work and 1 waiting for room (its limit is 2)"), "{why}");
                host.stands("next", session::Work::No, false);
                // A new one takes its place at once; down (its harness won't start), it frees it.
                for id in ["one", "two", "three"] {
                    host.open(Some(id.into()), no_harness(dir.path()))?;
                    soon(&format!("{id}'s place free once it's down"), || host.room_counts() == (1, 0)).await?;
                }
                host.stands("more", session::Work::Yes, false);
                host.set_max_sessions(1);
                assert_eq!(host.limits().per_machine, 1);
                assert!(refused(&host, "four").contains("already has 2 agents at work (its limit is 1)"));
                assert_eq!(host.list().len(), 3, "those it has go on");
                host.save_now();
            }
            let (tx, _rx) = async_channel::unbounded();
            let again = sim(tx, Some(file), Limits { per_machine: 1 });
            again.stands("busy", session::Work::Yes, false);
            again.restore();
            let mut ids: Vec<String> = again.list().into_iter().map(|i| i.id).collect();
            ids.sort();
            assert_eq!(ids, ["one", "three", "two"], "every session it ran came back");
            assert!(again.open(None, no_harness(dir.path())).is_err(), "a new one is held to its limit");
            Ok(())
        })
    }

    /// On a simulated clock of its own, as every host in these tests is (CP-M2, AC-M2.1).
    #[test]
    fn a_restarted_session_takes_its_timeline_from_the_conversation_store() -> anyhow::Result<()> {
        use super::super::core::Update;
        use super::super::model::{Body, Event, State};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            // Before the restart: the session, saved; its conversation, in the store.
            {
                let (tx, _rx) = async_channel::unbounded();
                let host = sim(tx, Some(file.clone()), Limits::default());
                host.open(Some("timeline".into()), spec)?;
                host.save_now();
            }
            let (_dir, store) = crate::convo::store::scratch()?;
            let said = |id: &str, seq: u64, text: &str| Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] };
            store.send("timeline", &Update::State { state: Box::new(State { title: "the parser".into(), ..State::default() }) });
            store.send("timeline", &said("m1", 1, "first"));
            store.send("timeline", &said("n1", 2, "a notice only kiln has"));
            store.send("timeline", &said("m2", 3, "second"));
            store.sync()?;
            // After it: the session comes back with that timeline, in order, by id.
            let (tx, rx) = async_channel::unbounded();
            let again = sim(tx, Some(file.clone()), Limits::default());
            again.restore_from(Some(&store));
            let back = again.sessions.lock().get("timeline").cloned().ok_or_else(|| anyhow::anyhow!("not restored"))?;
            {
                let core = back.core.lock();
                assert_eq!(core.events().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["m1", "n1", "m2"]);
                assert_eq!(core.state.title, "the parser");
            }
            // It announces itself without it: the store has it as it was, and its feed to the
            // server carries only what's new (docs/wire.md, "Restarts").
            crate::rt::sleep(Duration::from_millis(500)).await;
            let said: Vec<Update> = std::iter::from_fn(|| rx.try_recv().ok()).filter(|(s, _)| s == "timeline").map(|(_, u)| u).collect();
            assert!(said.iter().any(|u| matches!(u, Update::Open { .. })), "{said:?}");
            let again_said = |u: &Update| match u {
                Update::Reset { .. } => true,
                Update::Events { events } => events.iter().any(|e| ["m1", "n1", "m2"].contains(&e.id.as_str())),
                _ => false,
            };
            assert!(!said.iter().any(again_said), "{said:?}");
            Ok(())
        })
    }

    /// On a simulated clock of its own, as every host in these tests is (CP-M2, AC-M2.1).
    #[test]
    fn a_turn_a_restart_cut_short_goes_on_first_thing() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            let cut = |file: &std::path::Path| -> anyhow::Result<Vec<String>> {
                let saved: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(file)?)?;
                Ok(saved.iter().filter(|s| s.get("cut") == Some(&serde_json::json!(true))).filter_map(|s| s.get("id").and_then(serde_json::Value::as_str).map(str::to_owned)).collect())
            };
            {
                let (tx, _rx) = async_channel::unbounded();
                let host = sim(tx, Some(file.clone()), Limits::default());
                // A turn under way in one, none in the other, as the runner is stopped.
                host.open(Some("busy".into()), spec.clone())?.turn_open.store(true, Ordering::Relaxed);
                host.open(Some("idle".into()), spec.clone())?;
                host.save_now();
            }
            assert_eq!(cut(&file)?, ["busy"]);
            let restore = || -> anyhow::Result<(Arc<Host>, Vec<QueueItem>, Vec<QueueItem>)> {
                let (tx, _rx) = async_channel::unbounded();
                let again = sim(tx, Some(file.clone()), Limits::default());
                again.restore();
                let queue = |id: &str| again.sessions.lock().get(id).map(|s| s.core.lock().state.queue.clone()).unwrap_or_default();
                let (busy, idle) = (queue("busy"), queue("idle"));
                Ok((again, busy, idle))
            };
            let (again, busy, idle) = restore()?;
            assert_eq!(busy.len(), 1);
            assert!(busy.first().is_some_and(|q| q.id.starts_with(super::super::core::KEEP_ID) && q.text.ends_with("Pick up where you left off.")), "{busy:?}");
            assert!(idle.is_empty());
            // Restarted again before it went out: it's there once, not twice.
            if let Some(s) = again.sessions.lock().get("busy") {
                s.turn_open.store(true, Ordering::Relaxed);
            }
            again.save_now();
            drop(again);
            let (again, busy, _) = restore()?;
            assert_eq!(busy.len(), 1, "{busy:?}");
            // Once its turn has ended, a restart says nothing.
            if let Some(s) = again.sessions.lock().get("busy") {
                s.turn_open.store(false, Ordering::Relaxed);
            }
            again.save_now();
            assert!(cut(&file)?.is_empty());
            Ok(())
        })
    }

    /// What a harness left running is taken back over. The test waits for its stop on the
    /// session's updates, each a look; it doesn't sleep (CP-M2, AC-M2.3; the stop it asks for
    /// polls the process, as stopping one does).
    #[cfg(unix)]
    #[test]
    fn what_a_session_left_running_is_taken_back_over_after_a_restart() -> anyhow::Result<()> {
        use super::super::survivors::{self, Ran};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            // A harness that outlived its runner (a sleep stands in for it), what it left running
            // that the record has, and one only its mark says is its.
            let mut harness = std::process::Command::new("sleep").arg("60").spawn()?;
            let mut recorded = std::process::Command::new("sleep").arg("61").spawn()?;
            let Some(sleep_62) = survivors::sleeper(62) else {
                eprintln!("skipped: nothing here sleeps as a process whose environment can be read");
                let _ = harness.kill();
                let _ = recorded.kill();
                return Ok(());
            };
            // Started as an agent's processes start: in the session's group of processes where
            // this machine has them (found by it), by its mark elsewhere.
            let group = crate::os::contain::Group::of("left-running");
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg(format!("{sleep_62} >/dev/null 2>&1 & echo $!")).env(survivors::MARK, "left-running");
            if let Some(g) = &group {
                g.enter(&mut cmd);
            }
            let out = cmd.output()?;
            let marked: u32 = String::from_utf8_lossy(&out.stdout).trim().parse()?;
            let mine = survivors::scan(&[("me".to_owned(), Some(std::process::id()))]).remove("me").unwrap_or_default().under;
            let ran = |pid: u32| -> anyhow::Result<Ran> { mine.iter().find(|r| r.pid == pid).cloned().ok_or_else(|| anyhow::anyhow!("{pid} not seen")) };
            let (h, r) = (ran(harness.id())?, ran(recorded.id())?);
            {
                let (tx, _rx) = async_channel::unbounded();
                let host = sim(tx, Some(file.clone()), Limits::default());
                let s = host.open(Some("left-running".into()), spec.clone())?;
                s.turn_open.store(true, Ordering::Relaxed);
                *s.ran.lock() = (Some(h.clone()), vec![r.clone()]);
                host.save_now();
            }
            let (tx, rx) = async_channel::unbounded();
            let again = sim(tx, Some(file.clone()), Limits::default());
            again.restore();
            // The harness was stopped; what it started wasn't.
            let _ = harness.wait();
            assert!(survivors::alive(std::slice::from_ref(&h)).is_empty(), "the harness that outlived its runner is stopped");
            assert_eq!(survivors::alive(std::slice::from_ref(&r)).len(), 1);
            let s = again.sessions.lock().get("left-running").cloned().ok_or_else(|| anyhow::anyhow!("not restored"))?;
            let (tasks, queue) = {
                let core = s.core.lock();
                (core.state.tasks.clone(), core.state.queue.clone())
            };
            for pid in [recorded.id(), marked] {
                assert!(tasks.iter().any(|t| t.id == format!("left-{pid}") && t.status == "running" && t.left_running()), "{pid} taken back over: {tasks:?}");
            }
            let said = queue.first().map(|q| q.text.clone()).unwrap_or_default();
            assert!(said.contains("Pick up where you left off") && said.contains("sleep 61") && said.contains("62"), "{said}");
            // Stopped from kiln, it ends (waited for without holding up the session doing it): each
            // update the session sends is a look at whether its task says so yet.
            let task = format!("left-{}", recorded.id());
            s.command(Command::StopTask { task: task.clone() });
            let stopped = || s.core.lock().state.tasks.iter().any(|t| t.id == task && t.status == "stopped");
            let waited = crate::rt::timeout(Duration::from_secs(30), async {
                while !stopped() {
                    if rx.recv().await.is_err() {
                        return;
                    }
                }
            })
            .await;
            assert!(waited.is_ok() && stopped(), "its task says it stopped");
            assert!(survivors::alive(std::slice::from_ref(&r)).is_empty(), "and it ended");
            let _ = recorded.wait();
            survivors::stop(&[marked]);
            if let Some(g) = group {
                g.kill();
                g.remove();
            }
            Ok(())
        })
    }

    /// What a session shows as left running is set right: one that has ended is ended though no look
    /// took it over, one shown twice is shown once, and one taken over again isn't shown again.
    #[test]
    fn left_running_is_shown_once_and_ended_once_it_no_longer_runs() {
        use super::super::survivors::{self, Ran};
        let shown = |pid: u32| survivors::task(&Ran { pid, ppid: 1, started: 1_700_000_000, command: format!("sleep {pid}") });
        // 11 ended while no runner looked; 22 runs, shown twice by an older image; 33 is new.
        let mut tasks = vec![shown(11), shown(22), shown(22)];
        let apart = [22, 33].into_iter().collect();
        assert!(settle_left(&mut tasks, vec![shown(22), shown(33)], &apart, 1_800_000_000));
        let live: Vec<&str> = tasks.iter().filter(|t| t.live()).map(|t| t.id.as_str()).collect();
        assert_eq!(live, ["left-22", "left-33"]);
        let ended = tasks.iter().find(|t| t.id == "left-11").map(|t| (t.status.as_str(), t.ended));
        assert_eq!(ended, Some(("completed", Some(1_800_000_000_000))), "kept, as ended");
        // Nothing to set right, nothing changes.
        assert!(!settle_left(&mut tasks, vec![shown(22)], &apart, 1_800_000_010));
    }

    /// A session an upgrade handed over comes with its timeline, whose left running the watch never
    /// took over: its next look ends those that have ended since and shows the rest once, taken
    /// back over.
    #[test]
    fn a_look_sets_right_the_left_running_a_handed_over_session_came_with() -> anyhow::Result<()> {
        use super::super::survivors::{self, Ran, Seen};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, _rx) = async_channel::unbounded();
            let host = sim(tx, None, Limits::default());
            let s = host.open(Some("handed".into()), no_harness(dir.path()))?;
            let ran = |pid: u32| Ran { pid, ppid: 1, started: 1_700_000_000, command: format!("sleep {pid}") };
            // Its timeline shows 11 (ended during the upgrade) and 22 (twice); its record has 22.
            s.core.lock().state.tasks.extend([survivors::task(&ran(11)), survivors::task(&ran(22)), survivors::task(&ran(22))]);
            *s.ran.lock() = (None, vec![ran(22)]);
            host.note(&s, Seen::default(), &HashMap::from([(22, 1_700_000_000)]), 1_800_000_000);
            let tasks = s.core.lock().state.tasks.clone();
            let live: Vec<&str> = tasks.iter().filter(|t| t.live()).map(|t| t.id.as_str()).collect();
            assert_eq!(live, ["left-22"], "{tasks:?}");
            assert!(tasks.iter().any(|t| t.id == "left-11" && t.status == "completed"), "{tasks:?}");
            assert_eq!(s.left.lock().iter().map(|r| r.pid).collect::<Vec<_>>(), [22], "taken back over");
            // Once 22 ends, the next look ends it.
            host.note(&s, Seen::default(), &HashMap::new(), 1_800_000_010);
            assert!(s.core.lock().state.tasks.iter().all(|t| !t.live()), "nothing left running");
            Ok(())
        })
    }

    /// A harness running more than a look notes (two browsers, a server: a record keeps so many):
    /// whichever of them one look notes and the next doesn't, all still run under it, and none is
    /// taken for left running (and stopped once the agent is between turns).
    #[test]
    fn what_runs_under_a_harness_past_what_a_look_notes_is_never_left_running() -> anyhow::Result<()> {
        use super::super::survivors::{Ran, Seen};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, _rx) = async_channel::unbounded();
            let host = sim(tx, None, Limits::default());
            let s = host.open(Some("busy".into()), no_harness(dir.path()))?;
            let all: Vec<Ran> = (100..180).map(|pid| Ran { pid, ppid: 99, started: 1_700_000_000, command: format!("chrome --type=renderer {pid}") }).collect();
            let alive: HashMap<u32, u64> = all.iter().map(|r| (r.pid, r.started)).collect();
            // Each look notes 64 of the 80, not the same 64.
            let look = |from: usize| Seen { under: all.iter().skip(from).take(64).cloned().collect(), below: all.iter().map(|r| r.pid).collect(), ..Seen::default() };
            host.note(&s, look(0), &alive, 1_800_000_000);
            host.note(&s, look(16), &alive, 1_800_000_010);
            let left = s.left.lock().clone();
            assert!(left.is_empty(), "{left:?}");
            assert!(s.core.lock().state.tasks.iter().all(|t| !t.live()), "nothing shown as left running");
            Ok(())
        })
    }

    /// On a simulated clock of its own, as every host in these tests is (CP-M2, AC-M2.1).
    #[test]
    fn a_session_is_saved_and_restored_with_its_pause_without_deadlocking() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (tx, _rx) = async_channel::unbounded();
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let host = sim(tx, Some(file.clone()), Limits::default());
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            let s = host.open(Some("paused".into()), spec)?;
            {
                let mut core = s.core.lock();
                core.state.harness_session = Some("h1".into());
                core.state.pause = Some(super::super::model::Pause { reason: "limit".into(), since: 7, auto: true, by: "harness".into(), checking: Some(9), ..Default::default() });
                core.state.pause_at = Some(90);
            }
            // Saving takes each session's lock: it must never take one twice.
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let saver = host.clone();
            std::thread::spawn(move || {
                saver.save_now();
                let _ = done_tx.send(());
            });
            anyhow::ensure!(done_rx.recv_timeout(Duration::from_secs(5)).is_ok(), "saving the sessions deadlocked");
            let saved: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&file)?)?;
            let a = saved.first().ok_or_else(|| anyhow::anyhow!("nothing saved"))?;
            assert_eq!(a.pointer("/spec/resume").and_then(serde_json::Value::as_str), Some("h1"));
            assert_eq!(a.pointer("/pause/since").and_then(serde_json::Value::as_i64), Some(7));
            assert_eq!(a.get("pause_at").and_then(serde_json::Value::as_u64), Some(90));
            // Another start of the machine picks the pause up, its check under way asked again.
            let (tx2, _rx2) = async_channel::unbounded();
            let again = sim(tx2, Some(file.clone()), Limits::default());
            again.restore();
            let back = again.sessions.lock().get("paused").cloned().ok_or_else(|| anyhow::anyhow!("not restored"))?;
            let p = back.core.lock().state.pause.clone().ok_or_else(|| anyhow::anyhow!("pause not restored"))?;
            assert_eq!((p.since, p.checking), (7, None));
            assert_eq!(back.core.lock().state.pause_at, Some(90));
            Ok(())
        })
    }

    /// What a host says of the messages sent its sessions (`Update::Inbox`), the next `n` of them,
    /// whatever else it says between.
    async fn answers(rx: &async_channel::Receiver<(String, super::super::core::Update)>, n: usize) -> anyhow::Result<Vec<Said>> {
        let mut out = Vec::new();
        let got = crate::rt::timeout(Duration::from_secs(30), async {
            while out.len() < n {
                match rx.recv().await {
                    Ok((_, super::super::core::Update::Inbox { said })) => out.push(*said),
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        })
        .await;
        anyhow::ensure!(got.is_ok() && out.len() == n, "{} of {n} answers came: {out:?}", out.len());
        Ok(out)
    }

    fn message(id: &str, text: &str) -> Command {
        Command::Send { id: id.into(), text: text.into(), mode: Default::default(), attachments: Vec::new(), overrides: None, reply_to: None, task: None }
    }

    /// A host whose sessions' inboxes keep their records in the simulator's.
    fn with_inbox(tx: Sink) -> Arc<Host> {
        Host::with_records(tx, None, Limits::default(), Arc::new(SimClock::new()), Some(Arc::new(kiln_seams::records::SimRecords::new())))
    }

    /// CP-T3.2 (seen as a failed test's session, left behind its host, was handed over later with
    /// no ids): a session whose host is gone can neither read its inbox's ids nor fence it, so a
    /// hand-over or move of it stays here. It never goes without the ids its inbox took, which the
    /// process it went to would need to answer one sent again as a repeat, not a new prompt.
    #[test]
    fn a_session_whose_host_is_gone_goes_nowhere_without_its_inbox_s_ids() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (tx, _rx) = async_channel::unbounded();
            let host = with_inbox(tx);
            let files = host.files_for("s1");
            assert!((files.carried)("s1".into()).await.is_ok(), "its host there, its inbox's ids are read");
            drop(host);
            let read = (files.carried)("s1".into()).await;
            assert!(read.as_ref().is_err_and(|e| e.contains("stays here")), "{read:?}");
            let fenced = (files.fence)("s1".into(), true).await;
            assert!(fenced.as_ref().is_err_and(|e| e.contains("stays here")), "{fenced:?}");
            // Nor is a try at handing it over said undecided: a refusal then would let it go on
            // here though the machine's record may say it was taken (the twenty-second check's).
            let decided = (files.settle)("try-1".into(), "s1".into(), true).await;
            assert!(decided.as_ref().is_err_and(|e| e.contains("stays here")), "{decided:?}");
            Ok(())
        })
    }

    /// CP-T3.2: a message sent to a session twice under its id is handed to the session once, and
    /// both sends are answered: the first taken, the second its repeat, with the first's answer. The
    /// harness is sent one prompt.
    #[cfg(unix)]
    #[test]
    fn a_message_sent_twice_reaches_its_harness_once_and_both_are_answered() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let heard = dir.path().join("heard.jsonl");
            let program = dir.path().join("recording-claude");
            std::fs::write(&program, format!("#!/bin/sh\nexec cat >> '{}'\n", heard.display()))?;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
            let (tx, rx) = async_channel::unbounded();
            let host = with_inbox(tx);
            let spec = Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Spec::default() };
            let s = host.open(Some("twice".into()), spec)?;
            let started = crate::rt::timeout(Duration::from_secs(10), async {
                while s.pid.lock().is_none() || s.core.lock().meta.features.is_empty() {
                    crate::rt::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            anyhow::ensure!(started.is_ok(), "the recording harness never started");
            assert!(host.command("twice", message("m1", "build it")));
            assert!(host.command("twice", message("m1", "build it")));
            let said = answers(&rx, 2).await?;
            let first = said.first().and_then(|s| s.answer.clone().ok()).ok_or_else(|| anyhow::anyhow!("{said:?}"))?;
            assert!(!first.repeat && first.seq > 0 && first.at == kiln_seams::clock::SIM_EPOCH_MS, "{first:?}");
            assert_eq!(said.get(1).map(|s| s.answer.clone()), Some(Ok(Answer { repeat: true, ..first })));
            let prompts = || -> Vec<String> {
                std::fs::read_to_string(&heard).unwrap_or_default().lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).filter(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("user")).filter_map(|v| v.get("uuid").and_then(serde_json::Value::as_str).map(str::to_owned)).collect()
            };
            let reached = crate::rt::timeout(Duration::from_secs(10), async {
                while prompts().is_empty() {
                    crate::rt::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            anyhow::ensure!(reached.is_ok(), "no prompt reached the harness");
            assert_eq!(prompts(), ["m1"]);
            assert!(s.core.lock().state.queue.is_empty(), "no copy waits behind it");
            Ok(())
        })
    }

    /// CP-T3.2, AC-T3.2.4: a session's host refuses a message sent for an epoch the session has
    /// left, typed and naming both epochs, and one for an epoch it hasn't reached; one for its
    /// epoch is taken.
    #[test]
    fn a_host_refuses_a_message_for_another_epoch_naming_both() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, rx) = async_channel::unbounded();
            let host = with_inbox(tx);
            host.open(Some("fenced".into()), no_harness(dir.path()))?;
            // Gone on elsewhere and back twice: epoch 2.
            let inbox = host.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            // (Its lane's first word, the epoch it opened at, goes first: what follows comes after.)
            assert!(host.command_at("fenced", message("m0", "x"), Some(0)));
            answers(&rx, 1).await?;
            inbox.open("fenced", 2).await?;
            assert!(host.command_at("fenced", message("m1", "hello"), Some(1)));
            assert!(host.command_at("fenced", message("m1", "hello"), Some(3)));
            assert!(host.command_at("fenced", message("m1", "hello"), Some(2)));
            let said: Vec<Result<Answer, inbox::Refusal>> = answers(&rx, 3).await?.into_iter().map(|s| s.answer).collect();
            assert_eq!(said.first(), Some(&Err(inbox::Refusal::StaleEpoch { session: "fenced".into(), sent: 1, current: 2 })));
            assert_eq!(said.get(1), Some(&Err(inbox::Refusal::Behind { session: "fenced".into(), here: 2, sent: 3 })));
            assert!(said.get(2).is_some_and(|a| a.as_ref().is_ok_and(|a| a.epoch == 2 && !a.repeat)), "{said:?}");
            // A session that isn't here: refused, saying so.
            assert!(!host.command_at("nowhere", message("m2", "x"), None));
            Ok(())
        })
    }

    /// CP-T3.2: what a host's inboxes took is still taken after the host starts again: the same
    /// message sent after a restart is answered as a repeat, never handed to the session again.
    #[test]
    fn what_a_host_took_is_still_taken_after_it_restarts() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let first = {
                let (tx, rx) = async_channel::unbounded();
                let host = sim(tx, Some(file.clone()), Limits::default());
                anyhow::ensure!(host.inbox().is_some(), "a host with a record keeps inboxes beside it");
                host.open(Some("kept".into()), no_harness(dir.path()))?;
                host.save_now();
                assert!(host.command("kept", message("m1", "ship it")));
                let said = answers(&rx, 1).await?;
                said.into_iter().next().and_then(|s| s.answer.ok()).ok_or_else(|| anyhow::anyhow!("not taken"))?
            };
            let (tx, rx) = async_channel::unbounded();
            let again = sim(tx, Some(file.clone()), Limits::default());
            again.restore();
            assert!(again.command("kept", message("m1", "ship it")));
            let said = answers(&rx, 1).await?;
            assert_eq!(said.first().map(|s| s.answer.clone()), Some(Ok(Answer { repeat: true, ..first })));
            Ok(())
        })
    }

    /// CP-T3.2: a blue/green handover carries the newest ids its session's inbox took (the likeliest
    /// to be sent again), however many it keeps: on the new runner process a message taken before
    /// the handover is a repeat, never a second prompt.
    #[test]
    fn a_blue_green_handover_carries_the_newest_ids() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, _rx) = async_channel::unbounded();
            let blue = with_inbox(tx);
            blue.open(Some("bg".into()), no_harness(dir.path()))?;
            let inbox = blue.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            let d = inbox::digest("x", &[]);
            let most = inbox::KEEP.saturating_add(10);
            for n in 0..most {
                inbox.accept("bg", &format!("m{n}"), &d, None, 1).await?;
            }
            let newest = format!("m{}", most.saturating_sub(1));
            let moved = blue.move_out().await;
            anyhow::ensure!(moved.len() == 1, "one session handed over");
            let (tx2, rx2) = async_channel::unbounded();
            let green = with_inbox(tx2);
            for (rec, handed) in moved {
                green.move_in(rec, handed)?;
            }
            assert!(green.command("bg", message(&newest, "x")));
            let said = answers(&rx2, 1).await?;
            assert!(matches!(said.first().map(|s| &s.answer), Some(Ok(a)) if a.repeat), "{newest}, taken before the handover, was taken again after it: {said:?}");
            Ok(())
        })
    }

    /// CP-T3.2: a blue/green handover that's rolled back (the new build never took over, and this
    /// host takes its sessions back at the epoch they left at) leaves them taking messages again,
    /// and what they took before still counts.
    #[test]
    fn a_rolled_back_handover_takes_messages_again() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, rx) = async_channel::unbounded();
            let blue = with_inbox(tx);
            blue.open(Some("rb".into()), no_harness(dir.path()))?;
            assert!(blue.command("rb", message("m0", "before")));
            anyhow::ensure!(matches!(answers(&rx, 1).await?.first().map(|s| &s.answer), Some(Ok(_))), "taken before");
            let moved = blue.move_out().await;
            anyhow::ensure!(moved.len() == 1, "one session handed over");
            // Its inbox here fenced, as it is by the time a rollback comes.
            let inbox = blue.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            let mut fenced = false;
            for _ in 0..500 {
                if inbox.epoch("rb").await?.is_some_and(|(_, moved)| moved) {
                    fenced = true;
                    break;
                }
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            anyhow::ensure!(fenced, "the session's old lane never fenced its inbox");
            for (rec, handed) in moved {
                blue.move_in(rec, handed)?;
            }
            assert!(blue.command("rb", message("m1", "after the rollback")));
            assert!(blue.command("rb", message("m0", "before")));
            let said = answers(&rx, 2).await?;
            assert!(matches!(said.first().map(|s| &s.answer), Some(Ok(a)) if !a.repeat), "a new message after the rollback is taken: {said:?}");
            assert!(matches!(said.get(1).map(|s| &s.answer), Some(Ok(a)) if a.repeat), "one taken before still counts: {said:?}");
            Ok(())
        })
    }

    /// CP-T3.2: a new build that took its sessions over takes no messages till it's promoted (if it
    /// never is, its sessions go back to the runner it upgrades, which never heard of what this one
    /// took): each is refused `moved`, its inbox taking nothing; promoted, it takes them.
    #[test]
    fn a_new_build_takes_no_messages_till_it_is_promoted() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, rx) = async_channel::unbounded();
            let green = with_inbox(tx);
            green.hold(true);
            green.open(Some("g".into()), no_harness(dir.path()))?;
            assert!(green.command("g", message("m1", "x")));
            let said = answers(&rx, 1).await?;
            assert!(matches!(said.first().map(|s| &s.answer), Some(Err(inbox::Refusal::Moved { .. }))), "{said:?}");
            assert!(green.carried("g").await.is_ok_and(|v| v.is_empty()), "its inbox took nothing");
            green.hold(false);
            assert!(green.command("g", message("m1", "x")));
            let said = answers(&rx, 1).await?;
            assert!(matches!(said.first().map(|s| &s.answer), Some(Ok(a)) if !a.repeat), "{said:?}");
            Ok(())
        })
    }

    /// CP-T3.2 (the merge with main's one runner a machine): a session whose hand-over to its
    /// machine's newer runner process got no word stays fenced, taking no messages (the newer one may
    /// run it); the server's word coming late, a refusal, it takes messages here again.
    #[test]
    fn a_hand_over_s_late_refusal_lets_its_session_take_messages_again() -> anyhow::Result<()> {
        let _link = super::super::handing::hold_link();
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, rx) = async_channel::unbounded();
            let host = with_inbox(tx);
            host.open(Some("late".into()), no_harness(dir.path()))?;
            host.moving_fence("late", true).await?;
            assert!(host.command("late", message("m1", "x")));
            let said = answers(&rx, 1).await?;
            assert!(matches!(said.first().map(|s| &s.answer), Some(Err(inbox::Refusal::Moved { .. }))), "fenced: {said:?}");
            // The server's word, late (no hand-over waits for it now): the newer process refused it.
            host.handed_over("late", Some("no room".into()), false, true);
            let mut taken = false;
            for _ in 0..100 {
                assert!(host.command("late", message("m1", "x")));
                let said = answers(&rx, 1).await?;
                if matches!(said.first().map(|s| &s.answer), Some(Ok(a)) if !a.repeat) {
                    taken = true;
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(taken, "it takes messages here again");
            Ok(())
        })
    }

    /// CP-T3.2: a session back on a host at a higher epoch, its inbox there still fenced at the one it
    /// left at (opening it failed as it came back), has it opened as a message comes: it takes the
    /// message rather than refuse every one.
    #[test]
    fn a_session_back_higher_whose_inbox_is_still_fenced_takes_messages() -> anyhow::Result<()> {
        struct Back(u64, Mutex<Vec<String>>);
        impl Taker for Back {
            fn is_moving(&self) -> bool {
                false
            }
            fn epoch(&self) -> u64 {
                self.0
            }
            fn hand(&self, c: Command) -> bool {
                if let Command::Send { id, .. } = c {
                    self.1.lock().push(id);
                }
                true
            }
        }
        crate::rt::block_on(async move {
            let inbox = Inbox::new(Arc::new(kiln_seams::records::SimRecords::new()), inbox::Limits::default());
            inbox.open("s1", 1).await?;
            inbox.fence("s1", 1).await?;
            let back = Back(3, Mutex::new(Vec::new()));
            let said = take_message(Some(&inbox), "s1", Some(&back), message("m1", "x"), Some(3), 1).await;
            assert!(matches!(said.map(|s| s.answer), Some(Ok(a)) if !a.repeat && a.epoch == 3), "taken at the epoch it's at");
            assert_eq!(*back.1.lock(), ["m1"]);
            // A copy left behind at the epoch it left at isn't opened again: it takes nothing.
            inbox.fence("s1", 3).await?;
            let left = Back(3, Mutex::new(Vec::new()));
            let said = take_message(Some(&inbox), "s1", Some(&left), message("m2", "x"), Some(3), 2).await;
            assert!(matches!(said.map(|s| s.answer), Some(Err(inbox::Refusal::Moved { epoch: 3, .. }))));
            assert!(left.1.lock().is_empty());
            Ok(())
        })
    }

    /// CP-T3.2: handing sessions over ends even when their lanes wait on a sink nobody drains (the
    /// runner's loop that drains it is the one handing them over): it waits for them a while at most.
    #[test]
    fn a_handover_ends_when_its_sessions_lanes_wait_on_a_full_sink() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, rx) = async_channel::bounded(4);
            let host = Host::with_records(tx, None, Limits::default(), Arc::new(SimClock::new()), Some(Arc::new(kiln_seams::records::SimRecords::new())));
            host.open(Some("full".into()), no_harness(dir.path()))?;
            for n in 0..16 {
                let _ = host.command("full", message(&format!("m{n}"), "x"));
            }
            crate::rt::sleep(Duration::from_secs(1)).await;
            let out = crate::rt::timeout(Duration::from_secs(45), host.move_out()).await;
            drop(rx);
            anyhow::ensure!(out.is_ok_and(|m| m.len() == 1), "the handover hung on a full sink");
            Ok(())
        })
    }

    /// CP-T3.2: a session on its way elsewhere refuses a message before its inbox takes it (`moved`),
    /// so what goes with it is what it took before it began to move, and the message, sent again,
    /// is taken where it went.
    #[test]
    fn a_moving_session_refuses_a_message_before_its_inbox_takes_it() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, rx) = async_channel::unbounded();
            let host = with_inbox(tx);
            let s = host.open(Some("mv".into()), no_harness(dir.path()))?;
            s.stop_taking();
            assert!(host.command("mv", message("m1", "now")));
            let said = answers(&rx, 1).await?;
            assert!(matches!(said.first().map(|s| &s.answer), Some(Err(inbox::Refusal::Moved { .. }))), "{said:?}");
            assert!(host.carried("mv").await.is_ok_and(|v| v.is_empty()), "its inbox took nothing");
            Ok(())
        })
    }

    /// Ending an agent while its harness runs (and while it's down): the last state it sends
    /// before it closes, what the ended agent shows, has nothing still running.
    #[cfg(unix)]
    #[test]
    fn an_ended_agents_last_state_has_its_tasks_stopped() -> anyhow::Result<()> {
        use super::super::core::Update;
        use super::super::model::Task;
        use std::os::unix::fs::PermissionsExt;
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            // A harness that runs and says nothing.
            let quiet = dir.path().join("quiet-claude");
            std::fs::write(&quiet, "#!/bin/sh\nexec sleep 30\n")?;
            std::fs::set_permissions(&quiet, std::fs::Permissions::from_mode(0o755))?;
            for (id, program) in [("running", quiet.clone()), ("down", dir.path().join("no-such-claude"))] {
                let (tx, rx) = async_channel::unbounded();
                let host = Host::new(tx, None, Limits::default());
                let spec = Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Spec::default() };
                let s = host.open(Some(id.into()), spec)?;
                // Its harness running (and its adapter started), or down (it could not start). One
                // lock at a time.
                let ready = |s: &Session| {
                    if id == "down" {
                        return s.core.lock().state.status == Status::Exited;
                    }
                    let spawned = s.pid.lock().is_some();
                    spawned && !s.core.lock().meta.features.is_empty()
                };
                let started = crate::rt::timeout(Duration::from_secs(10), async {
                    while !ready(&s) {
                        crate::rt::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await;
                anyhow::ensure!(started.is_ok(), "{id}: the session never got going");
                {
                    let mut core = s.core.lock();
                    core.state.tasks.push(Task { id: "b7".into(), class: Some("shell".into()), status: "running".into(), background: true, stoppable: true, ..Task::default() });
                    core.touch_state();
                }
                assert!(host.close(id));
                let last = crate::rt::timeout(Duration::from_secs(20), async {
                    let mut last = None;
                    while let Ok((_, u)) = rx.recv().await {
                        match u {
                            Update::State { state } => last = Some(state),
                            Update::Closed => return Some(last),
                            _ => {}
                        }
                    }
                    None
                })
                .await;
                let state = last.ok().flatten().flatten().ok_or_else(|| anyhow::anyhow!("{id}: never closed, or no state before it"))?;
                assert!(state.tasks.iter().any(|t| t.id == "b7") && state.tasks.iter().all(|t| !t.live()), "{id}: {:?}", state.tasks);
            }
            Ok(())
        })
    }

    /// CP-T3.2 (the twelfth adversarial check's): while a machine's note of the hand-overs it took
    /// can't be written, the process that took them keeps its newest [`HANDOVERS_MAX`], as the file
    /// would, and no more: one more noted, the oldest goes.
    #[test]
    fn hand_overs_not_written_down_are_kept_to_the_newest_bound() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Its lock can't be opened: every write fails.
        std::fs::create_dir_all(dir.path().join("handovers-taken.lock"))?;
        let h = Handovers::of(dir.path());
        for n in 0..HANDOVERS_MAX + 10 {
            anyhow::ensure!(h.note(&format!("s{n}"), 1, i64::try_from(n)?).is_err(), "a write that can't happen didn't fail");
        }
        let kept = h.unwritten.lock().len();
        anyhow::ensure!(kept == HANDOVERS_MAX, "{kept} kept");
        let newest = format!("s{}", HANDOVERS_MAX + 9);
        anyhow::ensure!(h.taken("s9")?.is_none() && h.taken("s10")? == Some(1) && h.taken(&newest)? == Some(1), "the oldest 10 went, the newest are kept");
        Ok(())
    }

    /// CP-T3.2 (the nineteenth adversarial check's): a try at handing a session over is decided once
    /// for its machine, by whichever of its processes comes first: two deciding at once (one taking
    /// it over, the other taking it back) both find the one decision. Its record keeps the newest
    /// [`HANDOVERS_MAX`]; unreadable, it decides nothing.
    #[test]
    fn a_try_at_a_hand_over_is_decided_once_for_its_machine() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = dir.path().to_path_buf();
        for round in 0..4 {
            let attempt = format!("a{round}");
            let decide = |name: &str, how: Decided| -> anyhow::Result<Decided> {
                let h = Handovers::of(&state.join("instances").join(name));
                let locked = h.locked()?;
                if let Some(was) = locked.decided(&attempt)? {
                    return Ok(was);
                }
                // A moment between looking and deciding, for the other to come.
                std::thread::sleep(Duration::from_millis(30));
                locked.decide(&attempt, "s1", how, 2, 1)?;
                Ok(how)
            };
            let (one, two) = std::thread::scope(|sc| {
                let one = sc.spawn(|| decide("one", Decided::Taken));
                let two = sc.spawn(|| decide("two", Decided::Refused));
                (one.join(), two.join())
            });
            let (one, two) = (one.map_err(|_| anyhow::anyhow!("a decider panicked"))??, two.map_err(|_| anyhow::anyhow!("a decider panicked"))??);
            anyhow::ensure!(one == two, "round {round}: decided {one:?} and {two:?}");
        }
        let h = Handovers::of(dir.path());
        h.locked()?.decide("b1", "s3", Decided::Taken, 5, 2)?;
        let locked = h.locked()?;
        anyhow::ensure!(locked.taken_above("s3", 4)? && !locked.taken_above("s3", 5)? && !locked.taken_above("s4", 0)?, "taken above an epoch it wasn't");
        drop(locked);
        // Full: one more decided, the oldest goes.
        let full: HashMap<String, Decision> = (0..HANDOVERS_MAX).map(|n| (format!("old{n}"), Decision { session: "s2".into(), taken: false, epoch: 1, at: i64::try_from(n).unwrap_or(0) })).collect();
        std::fs::write(&h.decided, serde_json::to_vec(&full)?)?;
        h.locked()?.decide("new", "s2", Decided::Taken, 2, i64::MAX)?;
        let locked = h.locked()?;
        anyhow::ensure!(locked.decided("old0")?.is_none() && locked.decided("old1")? == Some(Decided::Refused) && locked.decided("new")? == Some(Decided::Taken), "the oldest didn't go");
        drop(locked);
        // Unreadable: nothing decided, and nothing said of it.
        std::fs::write(&h.decided, b"{")?;
        anyhow::ensure!(h.locked()?.decided("new").is_err(), "an unreadable record read as decided");
        Ok(())
    }

    /// CP-T3.2 (the nineteenth adversarial check's): a word that a hand-over was refused, come with
    /// nothing waiting for it (late), while the machine's record says a newer process took a try of
    /// it over: the session lets go here, rather than take messages again.
    #[cfg(unix)]
    #[test]
    fn a_late_refusal_of_a_hand_over_its_machine_took_lets_go() -> anyhow::Result<()> {
        let _link = super::super::handing::hold_link();
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, _rx) = async_channel::unbounded();
            let host = Host::with_records(tx, Some(dir.path().join("harness-sessions.json")), Limits::default(), Arc::new(SimClock::new()), Some(Arc::new(kiln_seams::records::SimRecords::new())));
            host.open(Some("late".into()), no_harness(dir.path()))?;
            host.moving_fence("late", true).await?;
            let epoch = host.get("late").map_or(0, |s| s.epoch());
            let h = host.handovers.as_ref().ok_or_else(|| anyhow::anyhow!("no record of the machine's hand-overs"))?;
            h.locked()?.decide("a1", "late", Decided::Taken, epoch + 1, 1)?;
            host.handed_over("late", Some("no room".into()), false, true);
            until_it("it let go", 30, || host.get("late").is_none()).await
        })
    }

    /// CP-T3.2 (the twentieth adversarial check's): the machine's lock on its hand-overs is taken
    /// before what a process holds of them, on every path. A note made while a take holds the lock
    /// waits for it without holding up that take, which reads and notes what the process holds.
    #[test]
    fn a_note_waits_for_the_machines_lock_without_holding_up_a_take() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let h = Arc::new(Handovers::of(dir.path()));
        let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        // A take holding the machine's lock reads what this process holds, and notes its own.
        let take = {
            let (h, tx) = (Arc::clone(&h), tx.clone());
            std::thread::spawn(move || {
                let took = (|| -> std::io::Result<()> {
                    let locked = h.locked()?;
                    let _ = held_tx.send(());
                    std::thread::sleep(Duration::from_millis(300));
                    let _ = h.taken("s1")?;
                    locked.note("s2", 3, 2)
                })();
                let _ = tx.send(if took.is_ok() { "take" } else { "the take failed" });
            })
        };
        held_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| anyhow::anyhow!("the take never held the lock"))?;
        // Meanwhile a note made without it in hand.
        let note = {
            let h = Arc::clone(&h);
            std::thread::spawn(move || {
                let noted = h.note("s1", 2, 1);
                let _ = tx.send(if noted.is_ok() { "note" } else { "the note failed" });
            })
        };
        let mut done = Vec::new();
        for _ in 0..2 {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(what) => done.push(what),
                Err(_) => anyhow::bail!("a deadlock: in 10 s only {done:?} finished"),
            }
        }
        take.join().map_err(|_| anyhow::anyhow!("the take panicked"))?;
        note.join().map_err(|_| anyhow::anyhow!("the note panicked"))?;
        done.sort_unstable();
        anyhow::ensure!(done == ["note", "take"], "{done:?}");
        anyhow::ensure!(h.taken("s1")? == Some(2) && h.taken("s2")? == Some(3), "both noted");
        Ok(())
    }

    /// What a host said of message `id` (`Update::Inbox`), whatever else it says before it.
    #[cfg(unix)]
    async fn answer_to(rx: &async_channel::Receiver<(String, super::super::core::Update)>, id: &str) -> anyhow::Result<Said> {
        let got = crate::rt::timeout(Duration::from_secs(30), async {
            while let Ok((_, u)) = rx.recv().await {
                if let super::super::core::Update::Inbox { said } = u
                    && said.message == id
                {
                    return Some(*said);
                }
            }
            None
        })
        .await;
        got.ok().flatten().ok_or_else(|| anyhow::anyhow!("no answer to {id} came"))
    }

    #[cfg(unix)]
    async fn until_it(what: &str, secs: u64, holds: impl Fn() -> bool) -> anyhow::Result<()> {
        let ok = crate::rt::timeout(Duration::from_secs(secs), async {
            while !holds() {
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        anyhow::ensure!(ok.is_ok(), "{what} didn't come in {secs} s");
        Ok(())
    }

    /// CP-T3.2, AC-T3.2.1 (the eighteenth adversarial check's; the merge with main's one runner a
    /// machine). An older runner process hands a session to its machine's newer one, m2 waiting in
    /// its queue behind m1's turn: its package goes by way of the server, and the newer process
    /// takes it over (`Host::adopt`), the queue with it, and runs it. Then the older process hears
    /// no word of it (`went_away` false: its own link to the server dropped, `handing::link_lost`,
    /// `NotHanded::Unknown`), or hears a refusal that isn't one (`went_away`: the newer process's
    /// link dropped after it took the session over, before its word reached the server, and the
    /// server tells the older one it went away: `crate::handovers::Handovers::lost`). m2 must reach
    /// one harness. And (`went_away`) m3, taken by the older process then, sent again once the newer
    /// one is back and holds the session (the older told to let go of it, as `Hub::let_go_of`
    /// does), must be answered there as its repeat, never taken again. Each case runs alone
    /// (`handing`'s link is one a process).
    #[cfg(unix)]
    fn local_hand_over(mode: u8) -> anyhow::Result<()> {
        let _link = super::super::handing::hold_link();
        let went_away = mode == 2;
        use std::os::unix::fs::PermissionsExt;
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let heard = dir.path().join("heard.jsonl");
            let fake = dir.path().join("fake-claude.py");
            std::fs::write(&fake, r##"import json, os, sys, time, uuid
HEARD = sys.argv[1]
args = sys.argv[2:]
def out(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()
if "--version" in args:
    print("2.1.296 (Claude Code)")
    sys.exit(0)
session = str(uuid.uuid4())
for flag in ("--session-id", "--resume"):
    if flag in args and args.index(flag) + 1 < len(args):
        session = args[args.index(flag) + 1]
turn = 0
while True:
    line = sys.stdin.readline()
    if not line:
        break
    with open(HEARD, "a") as f:
        f.write(json.dumps({"pid": os.getpid(), "line": line.strip()}) + "\n")
    try:
        m = json.loads(line)
    except ValueError:
        continue
    kind = m.get("type")
    if kind == "control_request":
        req = m.get("request") or {}
        resp = {}
        if req.get("subtype") == "initialize":
            resp = {"commands": [], "models": [], "agents": [], "current_permission_mode": "default", "claude_code_version": "2.1.296"}
        out({"type": "control_response", "response": {"subtype": "success", "request_id": m.get("request_id"), "response": resp}})
    elif kind == "user":
        turn += 1
        uid = m.get("uuid")
        out({"type": "command_lifecycle", "command_uuid": uid, "state": "started", "session_id": session})
        out({"type": "system", "subtype": "init", "cwd": ".", "session_id": session, "tools": [], "mcp_servers": [], "model": "claude-opus-5-5", "permissionMode": "default", "claude_code_version": "2.1.296"})
        for n in range(20):
            out({"type": "assistant", "message": {"id": "msg_%d_%d_%d" % (id(m), turn, n), "type": "message", "role": "assistant", "model": "claude-opus-5-5", "content": [{"type": "text", "text": "line %d" % n}], "stop_reason": None, "usage": {"input_tokens": 1, "output_tokens": 1}}, "parent_tool_use_id": None, "session_id": session})
            time.sleep(0.1)
        out({"type": "result", "subtype": "success", "is_error": False, "duration_ms": 1, "num_turns": 1, "result": "done", "session_id": session, "total_cost_usd": 0, "usage": {"input_tokens": 1, "output_tokens": 1}})
        out({"type": "command_lifecycle", "command_uuid": uid, "state": "completed", "session_id": session})
"##)?;
            let program = dir.path().join("claude-stand-in");
            std::fs::write(&program, format!("#!/bin/sh\nexec python3 '{}' '{}' \"$@\"\n", fake.display(), heard.display()))?;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
            // The prompts the stand-in harnesses (both processes') were sent under id `id`.
            let handed = |id: &str| -> Vec<u64> {
                std::fs::read_to_string(&heard)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                    .filter_map(|w| {
                        let v: serde_json::Value = serde_json::from_str(w.get("line")?.as_str()?).ok()?;
                        (v.get("type").and_then(serde_json::Value::as_str) == Some("user") && v.get("uuid").and_then(serde_json::Value::as_str) == Some(id)).then(|| w.get("pid").and_then(serde_json::Value::as_u64).unwrap_or(0))
                    })
                    .collect()
            };
            let prompts = |id: &str| handed(id).len();
            let (up, pieces) = async_channel::unbounded();
            super::super::handing::set_held_link(up);
            let (tx_old, rx_old) = async_channel::unbounded();
            let (tx_new, rx_new) = async_channel::unbounded();
            // Modes 3 on (the nineteenth adversarial check's): both keep the machine's record of its
            // hand-overs, in its state folder, as a runner's processes do: the older one there, the
            // newer one in a folder of its own under it.
            let state = dir.path().join("state");
            let (older, newer) = if mode >= 3 {
                let own = state.join("instances").join("newer");
                std::fs::create_dir_all(&own)?;
                let at = |file: PathBuf, tx| Host::with_records(tx, Some(file), Limits::default(), Arc::new(SimClock::new()), Some(Arc::new(kiln_seams::records::SimRecords::new())));
                (at(state.join("harness-sessions.json"), tx_old), at(own.join("harness-sessions.json"), tx_new))
            } else {
                (with_inbox(tx_old), with_inbox(tx_new))
            };
            let spec = Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Spec::default() };
            let s = older.open(Some("h".into()), spec)?;
            until_it("the stand-in harness's start", 30, || s.pid.lock().is_some() && !s.core.lock().meta.features.is_empty()).await?;
            // The stand-in harnesses that read a line (their first, kiln's initialize), in the order they
            // did. Each is waited for to be reading before what's timed after it: a loaded Mac can hold
            // a fresh script at its exec for minutes (its policy's checks), and the turns below are timed
            // from here (the twenty-second adversarial check's: these tests timed out with their stand-in
            // never run, not with kiln slow).
            let readers = || -> Vec<u64> {
                let mut out = Vec::new();
                for l in std::fs::read_to_string(&heard).unwrap_or_default().lines() {
                    if let Some(pid) = serde_json::from_str::<serde_json::Value>(l).ok().and_then(|w| w.get("pid").and_then(serde_json::Value::as_u64))
                        && !out.contains(&pid)
                    {
                        out.push(pid);
                    }
                }
                out
            };
            until_it("the stand-in harness reading", 300, || !readers().is_empty()).await?;
            assert!(older.command("h", message("m1", "build it")));
            until_it("m1's turn", 30, || s.core.lock().state.status == Status::Running).await?;
            assert!(older.command("h", message("m2", "then test it")));
            until_it("m2 waiting behind m1's turn", 30, || s.core.lock().state.queue.iter().any(|q| q.id == "m2")).await?;
            // The server asks the older process to hand it to the newest (`crate::handovers`): it
            // goes once m1's turn is over.
            assert!(older.command("h", Command::MoveTo { to: String::new(), cwd: String::new(), note: None, now: false, kind: None, model: None, local: true }));
            // Its package, in pieces, as the server gets them, passed on to the newer process.
            let mut bytes = Vec::new();
            let got = crate::rt::timeout(Duration::from_secs(60), async {
                while let Ok(up) = pieces.recv().await {
                    if let crate::exec::Up::Convo(crate::proto::AgentMsg::HarnessHandOver { data, done, .. }) = up {
                        bytes.extend(data);
                        if done {
                            return true;
                        }
                    }
                }
                false
            })
            .await;
            if !matches!(got, Ok(true)) {
                let core = s.core.lock();
                let notes: Vec<String> = core.events().map(|e| format!("{}: {}", e.id, format!("{:?}", e.body).chars().take(120).collect::<String>())).collect();
                let heard_lines = std::fs::read_to_string(&heard).unwrap_or_default();
                anyhow::bail!("no package came: status {:?}, turn_started {:?}, moving {}, hold {}, queue {:?}, notes {notes:?}, heard {} lines: {}", core.state.status, core.state.turn_started, s.is_moving(), core.hold, core.state.queue.iter().map(|q| q.id.clone()).collect::<Vec<_>>(), heard_lines.lines().count(), heard_lines.lines().map(|l| l.chars().take(160).collect::<String>()).collect::<Vec<_>>().join("\n"));
            }
            let pkg: serde_json::Value = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(
                pkg.pointer("/state/queue/0/id") == Some(&serde_json::json!("m2")) && pkg.get("inbox").is_some_and(|i| i.to_string().contains("m2")),
                "m2 goes in its queue, its id with its inbox's: {:?} {:?}",
                pkg.pointer("/state/queue"),
                pkg.get("inbox")
            );
            newer.adopt("an older kiln runner of this machine", &bytes).map_err(anyhow::Error::msg)?;
            until_it("the newer process's stand-in reading", 300, || readers().len() >= 2).await?;
            if went_away {
                // The newer process's link drops before its word that it took it reaches the
                // server, which tells the older one it went away (`Hub::handovers_lost`).
                let mut server = crate::handovers::Handovers::default();
                let _ = server.due("box", 1, vec!["h".into()], std::time::Instant::now());
                anyhow::ensure!(server.piece("box", "h", 1, Some(2), false) == crate::handovers::Piece::To(2), "its package goes to the newest");
                server.went_whole("h");
                let told = server.lost(2);
                anyhow::ensure!(told.iter().any(|(id, x)| id == "h" && x.from == 1 && x.whole), "the server tells the older process it can't say: {told:?}");
                older.handed_over("h", Some("this machine's newest runner process went away".into()), true, false);
            } else if mode == 1 {
                // The older process's link drops before the server's word comes.
                super::super::handing::link_lost();
            } else if mode == 3 {
                // The machine's record can't be read now (a disk fault): the newer process, sent the
                // package again, can't say whether it took it over, and the server passes that on as
                // not known (`Word::from_newest`); the older process, looking, can't read it either.
                // It waits, fenced, and asks again.
                let decided = state.join("handovers-decided.json");
                let kept = std::fs::read(&decided)?;
                std::fs::write(&decided, b"{")?;
                let why = match newer.adopt("an older kiln runner of this machine", &bytes) {
                    Ok(()) => anyhow::bail!("answered as taken with the machine's record unreadable"),
                    Err(why) => why,
                };
                let Command::HandedOver { error, unsure, by_newest, .. } = crate::handovers::Word::from_newest(Some(why.clone())).command() else { anyhow::bail!("not a word on a hand-over") };
                anyhow::ensure!(unsure && !by_newest, "the newest's {why:?} passed on as more than not known");
                older.handed_over("h", error, unsure, by_newest);
                // A while for the older process to go on with it, which it mustn't.
                crate::rt::sleep(Duration::from_secs(8)).await;
                std::fs::write(&decided, kept)?;
                // Readable again: its link drops, it looks, and lets go.
                super::super::handing::link_lost();
            } else if mode == 4 {
                // The session goes on in the newer process (m2 reaches its harness), then ends there
                // (the person ends it), and the older process's link drops: it looks, finds it taken
                // over, and lets go.
                until_it("m2 handed to the newer process's harness", 30, || prompts("m2") >= 1).await?;
                assert!(newer.close("h"));
                super::super::handing::link_lost();
            } else if mode == 5 {
                // A refusal comes: an earlier try's, late (a hand-over waits for one word across its
                // tries). The machine's record says this try was taken over: it lets go.
                older.handed_over("h", Some(format!("{}no room", crate::reach::node::NOT_TAKEN)), false, true);
            } else {
                // The control: the server's word comes, the newer process took it.
                older.handed_over("h", None, false, false);
            }
            until_it("m2 handed to a harness", 30, || prompts("m2") >= 1).await?;
            // A while for a second harness to be handed it too.
            let _ = crate::rt::timeout(Duration::from_secs(15), async {
                while prompts("m2") < 2 {
                    crate::rt::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            let m2 = prompts("m2");
            let mut m3 = None;
            if went_away {
                // m3 comes to the older process, which the server still has holding the session: it
                // waits, fenced, so m3 is refused there (`moved`) and goes where the session is.
                assert!(older.command("h", message("m3", "and deploy it")));
                let there = answer_to(&rx_old, "m3").await?;
                anyhow::ensure!(matches!(there.answer, Err(inbox::Refusal::Moved { .. })), "the older process, not knowing, takes nothing: {:?}", there.answer);
                // The newer process is back and says it holds the session: the older lets go.
                older.handed_over("h", None, false, false);
                assert!(newer.command("h", message("m3", "and deploy it")));
                let first = answer_to(&rx_new, "m3").await?;
                // m3's sender, its answer lost, sends it again.
                assert!(newer.command("h", message("m3", "and deploy it")));
                let again = answer_to(&rx_new, "m3").await?;
                anyhow::ensure!(first.answer.as_ref().is_ok_and(|a| !a.repeat), "taken where the session is: {:?}", first.answer);
                until_it("m3 handed to the newer process's harness", 30, || prompts("m3") >= 1).await?;
                let _ = crate::rt::timeout(Duration::from_secs(15), async {
                    while prompts("m3") < 2 {
                        crate::rt::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await;
                m3 = Some((again, prompts("m3")));
            } else if mode == 1 {
                // The word comes once the link is back: the newer process took it.
                older.handed_over("h", None, false, false);
            }
            if mode >= 3 {
                until_it("the older process letting go of it", 60, || older.get("h").is_none()).await?;
            }
            let m3_once = m3.as_ref().is_none_or(|(again, n)| again.answer.as_ref().is_ok_and(|a| a.repeat) && *n == 1);
            anyhow::ensure!(m2 == 1 && m3_once, "m2 reached {m2} harnesses (handed by the processes {:?}; the newer one's harness, as its package says, and the older one's); m3 sent again to the newer process (answer, prompts): {m3:?}", handed("m2"));
            Ok(())
        })
    }

    /// The control: the server's word comes (the newer process took it), and m2 reaches one harness.
    #[cfg(unix)]
    #[test]
    fn a_local_hand_over_answered_hands_its_queue_to_one_harness() -> anyhow::Result<()> {
        local_hand_over(0)
    }

    #[cfg(unix)]
    #[test]
    /// With no word back (its link to the server dropped), the older process waits, fenced, its
    /// harness down: m2 reaches the newer process's harness alone.
    fn a_local_hand_over_with_no_word_hands_its_queue_to_one_harness() -> anyhow::Result<()> {
        local_hand_over(1)
    }

    #[cfg(unix)]
    #[test]
    /// The newest went away once its whole package went to it: the server can't say, and the older
    /// process waits, fenced; m2 reaches one harness, and m3 is taken once, where the session is.
    fn a_local_hand_over_whose_newest_went_away_after_taking_it_is_one_prompt() -> anyhow::Result<()> {
        local_hand_over(2)
    }

    #[cfg(unix)]
    #[test]
    /// The nineteenth adversarial check's (AC-T3.2.1): the machine's record of its hand-overs can't
    /// be read now, so the newer process, sent the package again, can't say whether it took it over.
    /// That isn't a refusal: the older process keeps waiting (it can't read the record either), and
    /// m2 reaches one harness; readable again, it lets go.
    fn a_local_hand_over_the_newest_cannot_say_it_took_keeps_waiting() -> anyhow::Result<()> {
        local_hand_over(3)
    }

    #[cfg(unix)]
    #[test]
    /// Taken over, and ended there since: the older process, its link dropped, looks at the machine's
    /// record and lets go; m2 reaches one harness.
    fn a_local_hand_over_taken_and_ended_there_is_let_go_of_by_the_older_process() -> anyhow::Result<()> {
        local_hand_over(4)
    }

    #[cfg(unix)]
    #[test]
    /// The nineteenth adversarial check's (AC-T3.2.1): a refusal that's an earlier try's, come late
    /// while the newer process took this one over, doesn't make the older process go on with it: the
    /// machine's record says it was taken over, and m2 reaches one harness.
    fn a_late_refusal_of_a_local_hand_over_taken_over_is_one_prompt() -> anyhow::Result<()> {
        local_hand_over(5)
    }

    /// A subagent opened beside its parent runs as its parent does here: its parent's environment
    /// (the account and configuration it was given) and, in its harness, its program; what its own
    /// spec says stays. An agent its parent started that isn't its subagent, and a subagent whose
    /// parent runs elsewhere, open as they're asked.
    #[test]
    fn a_subagent_opens_with_its_parent_s_environment_and_program() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, _rx) = async_channel::unbounded();
            let host = sim(tx, None, Limits::default());
            let env = vec![("CLAUDE_CONFIG_DIR".to_owned(), dir.path().join("work-account").to_string_lossy().into_owned())];
            let parent = Spec { env: env.clone(), ..no_harness(dir.path()) };
            let program = parent.program.clone();
            host.open(Some("main".into()), parent)?;
            let child = |subagent: bool| Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), parent: Some("main".into()), subagent, ..Spec::default() };
            // Opened: it runs as its parent does (its parent's program, which can't start here).
            host.open(Some("sub".into()), child(true))?;
            let sub = host.get("sub").map(|s| s.spec.lock().clone()).ok_or_else(|| anyhow::anyhow!("the subagent didn't open"))?;
            assert_eq!((&sub.env, &sub.program), (&env, &program), "its parent's account and program");
            // The rest as each would open (none started: a spec without a program runs the machine's own harness).
            let own = host.like_its_parent(Spec { env: vec![("CLAUDE_CONFIG_DIR".into(), "/its/own".into())], program: Some("/its/own/claude".into()), ..child(true) });
            assert_eq!((own.env.first().map(|(_, v)| v.as_str()), own.program.as_deref()), (Some("/its/own"), Some("/its/own/claude")), "what it was given stays");
            let other = host.like_its_parent(Spec { kind: "codex".into(), ..child(true) });
            assert_eq!((&other.env, other.program.as_deref()), (&env, None), "in another harness, its parent's account but not its program");
            let plain = host.like_its_parent(child(false));
            assert!(plain.env.is_empty() && plain.program.is_none(), "an agent of its own opens as asked");
            let far = host.like_its_parent(Spec { parent: Some("elsewhere".into()), ..child(true) });
            assert!(far.env.is_empty() && far.program.is_none(), "its parent runs on another machine: nothing to copy");
            for id in ["main", "sub"] {
                let _ = host.close(id);
            }
            Ok(())
        })
    }
}
