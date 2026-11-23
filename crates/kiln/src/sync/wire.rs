//! What the server and a runner say about synced repos, inside `ServerMsg::Sync` and
//! `AgentMsg::Sync` on a lane of their own: a [`Msg`] as JSON, so either side can add fields, and
//! beside it the bytes of the hyper-raft message it carries, if any. Contents travel over HTTP
//! instead (`GET`/`PUT /api/sync/blobs/:hash`), with the token [`Msg::Copies`] gives a machine.
//!
//! The same lane carries what machines say of the git repositories they hold ([`Msg::Repos`]) and
//! the parts of moving repositories between them ([`Msg::Clone`], [`Msg::Bundle`], [`Msg::Delete`],
//! answered by [`Msg::Task`]): only to runners announcing `CAP_REPOS`. A side that doesn't know a
//! kind skips it.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::Step;
use super::scan::Skipped;

/// A raft message's bytes in one frame, at the most: a bigger one (an image) goes in parts.
pub const PART: usize = 512 << 10;
/// How long the parts of one message wait for the rest.
const PARTS_WAIT: Duration = Duration::from_secs(60);
/// Messages in parts put back together at once, at the most.
const JOINING: usize = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Msg {
    /// A hyper-raft message of `repo`'s group, its bytes beside: whole (`parts` 0), or part `part`
    /// of `parts` of message `id`.
    Raft {
        repo: String,
        #[serde(default)]
        id: u64,
        #[serde(default)]
        part: u32,
        #[serde(default)]
        parts: u32,
    },
    /// Server → runner: every copy this machine holds, whole, and the token its blob requests
    /// carry. Sent when the runner connects and whenever its copies change.
    Copies { copies: Vec<CopySpec>, token: String },
    /// Server → runner: another runner process of this machine syncs now: let go of the copies.
    Release,
    /// Runner → server: a copy's steps for `repo`'s log, in order, their contents on the server.
    Steps { repo: String, steps: Vec<Step> },
    /// Server → runner: contents `repo`'s steps name that the server lacks: the steps are taken
    /// once the contents are there and the steps sent again.
    Want { repo: String, hashes: Vec<String> },
    /// Runner → server: how a copy is doing (boxed: the biggest by far).
    Status(Box<CopyStatus>),
    /// Runner → server: the git repositories on this machine, whole (see [`Repos`]). Sent when it
    /// connects, when they change, and when asked ([`Msg::Look`]).
    Repos(Repos),
    /// Server → runner: look at this machine's git repositories again now.
    Look,
    /// Server → runner: bundle the git repository at `path` (its branches, tags and `HEAD`) and
    /// send it to the server for the batch of tasks `task` (`PUT /api/sync/bundles/:task`).
    Bundle { task: String, path: String },
    /// Server → runner: clone a repository into a folder (see [`CloneOrder`]).
    Clone(CloneOrder),
    /// Server → runner: task `task` moved the repository at `path` to another machine: its folder
    /// goes. `origin`, when given, is the remote it must have (else it is left alone).
    Delete {
        task: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    /// Server → runner: kiln no longer keeps the clone at `path`; its files stay.
    Forget { path: String },
    /// Runner → server: how this machine's part of a task goes.
    Task(TaskNews),
    /// Runner → server: an agent session moving between machines with a synced repo's files, as
    /// this machine saw its part (`crate::harness::session`), for the repo's activity.
    Move(MoveNews),
    /// Server → runner: the repositories this machine holds that others hold too, whole: each with
    /// the batch its work goes to next (`crate::sync::share`).
    Shares { shares: Vec<ShareSlot> },
    /// Runner → server: this machine's work in a repository others hold, sent to the slot's batch.
    Shared(Box<Work>),
    /// Server → runner: another machine's work in a repository this one holds: taken in beside its
    /// own (`refs/remotes/kiln/<machine>/…`), nothing of its own touched.
    Take(Box<Work>),
}

/// An agent session's move, as one of the two machines saw its part.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MoveNews {
    /// The synced repo its folder is a copy of.
    pub repo: String,
    /// The log index its files are in through: the machine it goes to takes the log that far
    /// before the session goes on there.
    pub index: u64,
    pub session: String,
    /// The machines it leaves and goes to.
    pub from: String,
    pub to: String,
    /// `left` (its folder's last changes are in the log; it is being handed over), `failed` (it
    /// stays) or `arrived` (its files are in its folder there; it goes on).
    pub phase: String,
    /// How long that part waited, in milliseconds.
    pub waited_ms: u64,
    /// Why it failed.
    pub error: Option<String>,
}

/// The git repositories on a machine, as it last looked: under its usual folders (`~/Projects`,
/// `~/src`, …), the clones kiln made there, and its synced copies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Repos {
    /// Its user's home: where `~` is on it.
    pub home: String,
    pub repos: Vec<Found>,
    /// Why it couldn't look (no git), if it couldn't.
    pub error: Option<String>,
    /// Unix seconds.
    pub at: i64,
}

/// One git repository on a machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Found {
    /// Its folder, whole.
    pub path: String,
    /// `remote.origin.url`, without credentials.
    pub origin: Option<String>,
    /// The branch checked out; none: a detached `HEAD`.
    pub branch: Option<String>,
    /// The commit checked out; none: no commit yet.
    pub head: Option<String>,
    /// The branch it tracks, and how far ahead of and behind it the branch is.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Tracked files changed and not committed.
    pub changed: u32,
    /// When git last did something there (unix seconds).
    pub active: i64,
    /// kiln cloned it there.
    pub kiln: bool,
    /// Its branch checked out against the same branch on the other machines that hold it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<Peer>,
}

/// A repository this machine holds that others hold too: where its branches' bundle and its work
/// in progress's go next (`PUT /api/sync/bundles/:batch`), and whether the server has none of its
/// work yet (it goes whatever it is).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareSlot {
    pub key: String,
    pub batch: String,
    pub wip_batch: String,
    pub fresh: bool,
}

/// A machine's work in a repository: its branches, the work in progress of its folders of it (a
/// commit of its files as they are, on the branch checked out), and two bundles: the commits its
/// branches name that its remote lacks, and the work in progress beyond them. Each goes again only
/// when it changes: a batch left empty is the one before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Work {
    pub key: String,
    /// The machine it's from (the server fills it in).
    pub from: String,
    /// Branch → commit.
    pub branches: BTreeMap<String, String>,
    /// Branch checked out → its work in progress.
    pub wip: BTreeMap<String, String>,
    /// The branches' bundle: its batch, and its SHA-256 and size when there's one.
    pub batch: String,
    pub hash: Option<String>,
    pub size: u64,
    /// The work in progress's bundle, the same way.
    pub wip_batch: String,
    pub wip_hash: Option<String>,
    pub wip_size: u64,
}

/// How a folder's branch stands against the same branch on another machine (its work, taken in).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Peer {
    pub machine: String,
    /// Commits of this branch the other's lacks, and of the other's this one lacks.
    pub ahead: u32,
    pub behind: u32,
    /// It has work in progress on the branch.
    pub wip: bool,
}

/// Clone a repository into `path`, which must be absent or an empty folder: from a bundle the
/// server holds for the task's batch (`GET /api/sync/bundles/:bundle`), or from `url` itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloneOrder {
    pub task: String,
    /// The repository's key (`crate::sync::places::key_of`), kept with the clone.
    pub key: String,
    pub path: String,
    /// The bundle to fetch (its batch), its SHA-256 and size.
    pub bundle: Option<String>,
    pub hash: Option<String>,
    pub size: u64,
    pub url: Option<String>,
    /// The branch to check out; none: the one the source has.
    pub branch: Option<String>,
    /// What `origin` is set to once cloned; none: the clone keeps no remote.
    pub origin: Option<String>,
    /// Every branch of the source becomes a branch here (a move), not only the one checked out.
    pub all_branches: bool,
}

/// How a machine's part of a task goes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskNews {
    pub task: String,
    /// `bundling`, `sending`, `fetching`, `cloning`, `deleting`, `done` or `failed`.
    pub state: String,
    /// What it is doing, in words.
    pub detail: Option<String>,
    pub error: Option<String>,
    /// A bundle sent: its SHA-256 and size.
    pub hash: Option<String>,
    pub size: Option<u64>,
    /// A clone made: the branch and commit it checked out.
    pub branch: Option<String>,
    pub head: Option<String>,
}

impl TaskNews {
    pub fn new(task: &str, state: &str, detail: impl Into<String>) -> Self {
        Self { task: task.to_owned(), state: state.to_owned(), detail: Some(detail.into()), ..Self::default() }
    }

    pub fn failed(task: &str, error: impl Into<String>) -> Self {
        Self { task: task.to_owned(), state: "failed".to_owned(), error: Some(error.into()), ..Self::default() }
    }

    /// The task's part here is over.
    pub fn over(&self) -> bool {
        self.state == "done" || self.state == "failed"
    }
}

/// A copy, as the server tells its machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySpec {
    pub repo: String,
    pub name: String,
    /// The copy's member of the repo's group.
    pub member: u64,
    pub path: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
    /// Its files, its git refs, or both (`docs/sync.md`, "Modes").
    #[serde(default, skip_serializing_if = "super::Mode::is_files")]
    pub mode: super::Mode,
    /// A folder kiln has just cloned for this copy: it takes the group's files once it has the
    /// log through this index (what it holds that the group doesn't goes), proposing nothing till
    /// then. Only for a copy whose machine has no state for it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopt: Option<u64>,
}

/// How a copy is doing, as its machine says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CopyStatus {
    pub repo: String,
    pub member: u64,
    /// Through where the folder has taken the log.
    pub applied: u64,
    /// Committed steps the folder hasn't taken yet.
    pub behind: usize,
    /// Steps made here the log hasn't brought back.
    pub unsent: usize,
    /// Contents the folder waits for.
    pub waiting: usize,
    /// Conflict copies in the folder.
    pub conflicts: usize,
    /// Files the last look left out (a few of them).
    pub skipped: Vec<Skipped>,
    /// What went wrong last, if nothing has gone right since.
    pub error: Option<String>,
    /// Unix seconds.
    pub at: i64,
    /// It has looked at its folder since it opened: what the folder held then is in the log, or
    /// on its way (`unsent`).
    pub looked: bool,
    /// It waits to take the group's files (`CopySpec::adopt`).
    pub adopting: bool,
    /// The contents it moved with the server, since its runner started.
    pub traffic: Traffic,
    /// How long the last change another machine made took to get to its folder, in milliseconds:
    /// from that machine's look to the file here (the two clocks apart add to it).
    pub lag_ms: Option<u64>,
}

/// What a copy moved with the server, since its runner started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Traffic {
    /// Contents sent to the server, and their bytes (those it had already count, not their bytes).
    pub sent: u64,
    pub sent_bytes: u64,
    /// Contents fetched from it, and their bytes.
    pub fetched: u64,
    pub fetched_bytes: u64,
    /// Transfers that failed, and those tried again after.
    pub failed: u64,
    pub retried: u64,
    /// Contents the server no longer had: versions later steps replaced.
    pub gone: u64,
    /// Steps sent again, the log not having brought them back.
    pub resent: u64,
    /// The last failure, and when (unix seconds).
    pub last_failure: Option<String>,
    pub failed_at: i64,
}

/// A message as it goes on the wire: the JSON and the raft bytes beside it.
pub fn encode(msg: &Msg) -> String {
    serde_json::to_string(msg).unwrap_or_default()
}

pub fn decode(json: &str) -> Option<Msg> {
    serde_json::from_str(json).ok()
}

/// A raft message's frames for `repo`: one, or the parts of a bigger one, numbered `id`.
pub fn frames(repo: &str, id: u64, bytes: Vec<u8>) -> Vec<(Msg, Vec<u8>)> {
    if bytes.len() <= PART {
        return vec![(Msg::Raft { repo: repo.to_owned(), id: 0, part: 0, parts: 0 }, bytes)];
    }
    let parts = bytes.chunks(PART).count();
    let parts = u32::try_from(parts).unwrap_or(u32::MAX);
    bytes
        .chunks(PART)
        .zip(0..)
        .map(|(chunk, part)| (Msg::Raft { repo: repo.to_owned(), id, part, parts }, chunk.to_vec()))
        .collect()
}

/// Sparse out-of-order transfer state. A conversation stages its image in an anonymous WAL-side
/// file. Other lanes retain their small transfers in memory. Only received bytes occupy space.
#[derive(Default)]
pub struct Joiner {
    held: usize,
    idle_floor: Option<Duration>,
    next_expiry: Option<Instant>,
    open: HashMap<(String, u64), Joining>,
    dir: Option<PathBuf>,
    /// No clock (a conversation's, CP-T3.2): a partial message goes only as it completes, as its sender is forgotten
    /// ([`Joiner::forget_prefix`]), or as room is needed for another, the one that progressed least recently first;
    /// none goes for time alone.
    untimed: bool,
    /// Parts taken so far: the order partial messages last progressed in, which an untimed joiner lets go of by.
    taken: u64,
}

struct Joining {
    total: u32,
    parts: BTreeMap<u32, Part>,
    file: Option<std::fs::File>,
    bytes: usize,
    room: usize,
    since: Instant,
    window: Duration,
    /// When it last took a part, in the joiner's count of parts taken.
    last: u64,
}

enum Part {
    Memory(Vec<u8>),
    File { offset: u64, len: usize, checksum: u32 },
}

fn available_bytes() -> usize {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let host = sys.available_memory();
    let free = crate::os::cgroup::memory().and_then(|m| m.limit.map(|limit| limit.saturating_sub(m.used))).map_or(host, |limit| host.min(limit));
    usize::try_from(free).unwrap_or(usize::MAX)
}

impl Joining {
    fn charge(&self) -> usize { self.bytes.saturating_add(self.parts.len().saturating_mul(size_of::<(u32, Part)>() )) }
}

impl Joiner {
    pub fn in_dir(dir: PathBuf, idle_floor: Duration) -> Self { Self { dir: Some(dir), idle_floor: Some(idle_floor), ..Self::default() } }

    /// A conversation's joiner (CP-T3.2): its parts staged in `dir`, and no clock ([`Joiner::untimed`]).
    pub fn staged_untimed(dir: PathBuf) -> Self { Self { dir: Some(dir), untimed: true, ..Self::default() } }

    /// An untimed joiner's room: the partial message that progressed least recently, other than `keep`, let go of.
    /// Whether there was one.
    fn evict_least_recent(&mut self, keep: &(String, u64)) -> bool {
        let Some(oldest) = self.open.iter().filter(|(key, _)| *key != keep).min_by_key(|(_, j)| j.last).map(|(key, _)| key.clone()) else { return false };
        if let Some(j) = self.open.remove(&oldest) {
            self.held = self.held.saturating_sub(j.charge());
        }
        true
    }

    pub fn forget_prefix(&mut self, prefix: &str) {
        self.open.retain(|(source, _), j| {
            if !source.starts_with(prefix) { return true; }
            self.held = self.held.saturating_sub(j.charge());
            false
        });
    }


    pub fn expire(&mut self) {
        let now = Instant::now();
        if self.next_expiry.is_none_or(|until| now < until) { return; }
        self.next_expiry = None;
        self.open.retain(|_, j| {
            let until = j.since + j.window;
            if now >= until { self.held = self.held.saturating_sub(j.charge()); return false; }
            self.next_expiry = Some(self.next_expiry.map_or(until, |next| next.min(until)));
            true
        });
    }

    pub fn add(&mut self, from: &str, id: u64, part: u32, parts: u32, bytes: Vec<u8>) -> Option<Vec<u8>> {
        self.add_frame(from, id, part, parts, bytes, 0)?.finish()
    }

    pub fn add_frame(&mut self, from: &str, id: u64, part: u32, parts: u32, mut bytes: Vec<u8>, start: usize) -> Option<Joined> {
        let len = bytes.len().checked_sub(start)?;
        if parts <= 1 {
            if part != 0 || len > PART { return None; }
            if start != 0 { bytes.drain(..start); }
            return Some(Joined(Completed::Frame(bytes)));
        }
        if part >= parts || len == 0 || len > PART { return None; }
        if !self.untimed { self.expire(); }
        let key = (from.to_owned(), id);
        if !self.open.contains_key(&key) {
            let max_open = if self.dir.is_some() {
                let frames = (crate::memory::room::share() / PART).max(1);
                #[cfg(unix)]
                let frames = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
                    .ok().and_then(|(soft, _)| usize::try_from(soft).ok())
                    .map_or(frames, |limit| {
                        // Leave a descriptor for the checkpoint writer, using the process's
                        // actual open count alongside the kernel's final tempfile allocation.
                        let open = usize::try_from(crate::host::process_self().open_fds).unwrap_or(usize::MAX);
                        frames.min(self.open.len().saturating_add(limit.saturating_sub(open).saturating_sub(1)))
                    });
                frames
            } else { JOINING };
            // An untimed joiner makes room by letting go of the partial message that progressed least recently.
            while self.open.len() >= max_open {
                if !self.untimed || !self.evict_least_recent(&key) { return None; }
            }
            // The contiguous wire buffer and its decoded Raft message coexist at completion.
            let mut room = available_bytes() / 2;
            let file = if let Some(dir) = &self.dir {
                std::fs::create_dir_all(dir).ok()?;
                room = room.min(usize::try_from(crate::observe::disk_free(dir)?).unwrap_or(usize::MAX));
                Some(tempfile::tempfile_in(dir).ok()?)
            } else { None };
            let since = Instant::now();
            let window = self.idle_floor.unwrap_or(PARTS_WAIT);
            if !self.untimed {
                self.next_expiry = Some(self.next_expiry.map_or(since + window, |next| next.min(since + window)));
            }
            self.open.insert(key.clone(), Joining { total: parts, parts: BTreeMap::new(), file, bytes: 0, room, since, window, last: 0 });
        }
        let charge = len.checked_add(size_of::<(u32, Part)>())?;
        if self.untimed {
            let room = self.open.get(&key)?.room;
            while self.held.checked_add(charge)? > room {
                if !self.evict_least_recent(&key) { break; }
            }
        }
        let j = self.open.get_mut(&key)?;
        if j.total != parts || j.parts.contains_key(&part) || self.held.checked_add(charge)? > j.room { return None; }
        let stored = if let Some(file) = &mut j.file {
            let offset = u64::try_from(j.bytes).ok()?;
            let payload = bytes.get(start..)?;
            let checksum = crc32fast::hash(payload);
            if file.seek(SeekFrom::Start(offset)).and_then(|_| file.write_all(payload)).is_err() {
                self.held = self.held.saturating_sub(j.charge());
                self.open.remove(&key);
                return None;
            }
            Part::File { offset, len, checksum }
        } else {
            if start != 0 { bytes.drain(..start); }
            Part::Memory(bytes)
        };
        self.held += charge;
        j.parts.insert(part, stored);
        j.bytes += len;
        self.taken = self.taken.wrapping_add(1);
        j.last = self.taken;
        if !self.untimed {
            if let Some(floor) = self.idle_floor { j.window = j.window.max(j.since.elapsed().saturating_add(floor)); }
            j.since = Instant::now();
        }
        if j.parts.len() != usize::try_from(parts).ok()? { return None; }
        let j = self.open.remove(&key)?;
        self.held = self.held.saturating_sub(j.charge());
        Some(Joined(Completed::Parts(j)))
    }
}

/// A completed sparse transfer. Its read and verification can run independently of Raft ticks.
pub struct Joined(Completed);

enum Completed {
    Frame(Vec<u8>),
    Parts(Joining),
}

impl Joined {
    pub fn encoded_bytes(&self) -> usize {
        match &self.0 { Completed::Frame(frame) => frame.len(), Completed::Parts(j) => j.bytes }
    }

    pub fn finish(self) -> Option<Vec<u8>> {
        let mut j = match self.0 { Completed::Frame(frame) => return Some(frame), Completed::Parts(j) => j };
        if j.file.is_some() && j.bytes > available_bytes() / 2 { return None; }
        let mut out = Vec::new();
        out.try_reserve_exact(j.bytes).ok()?;
        for chunk in j.parts.into_values() {
            match chunk {
                Part::Memory(bytes) => out.extend_from_slice(&bytes),
                Part::File { offset, len, checksum } => {
                    let file = j.file.as_mut()?;
                    file.seek(SeekFrom::Start(offset)).ok()?;
                    let start = out.len();
                    let end = start.checked_add(len)?;
                    if end > j.bytes { return None; }
                    out.resize(end, 0);
                    let payload = out.get_mut(start..end)?;
                    file.read_exact(payload).ok()?;
                    if crc32fast::hash(payload) != checksum { return None; }
                }
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_big_message_goes_in_parts_and_comes_back_whole() {
        let bytes: Vec<u8> = (0..(PART * 2 + 17)).map(|i| (i % 251) as u8).collect();
        let sent = frames("r", 9, bytes.clone());
        assert_eq!(sent.len(), 3);
        let mut j = Joiner::default();
        let mut whole = None;
        // Out of order, one twice.
        for i in [2, 0, 0, 1] {
            let Some((Msg::Raft { id, part, parts, .. }, b)) = sent.get(i).cloned() else { continue };
            whole = whole.or(j.add("m", id, part, parts, b));
        }
        assert_eq!(whole, Some(bytes));
        let small = frames("r", 1, vec![1, 2, 3]);
        assert!(matches!(small.first(), Some((Msg::Raft { parts: 0, .. }, _))));
    }

    /// CP-T3.2, a conversation's joiner with no clock: a partial message arms no expiry and isn't let go for time;
    /// room for another lets go of the one that progressed least recently, never the one taking the part; a sender
    /// forgotten takes its partial messages with it; what completes comes back whole.
    #[test]
    fn cp_t32_an_untimed_joiner_lets_a_partial_message_go_only_for_room_or_its_sender() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut join = Joiner::staged_untimed(dir.path().to_owned());
        anyhow::ensure!(join.add("1:a", 1, 0, 2, br#"{"a":"#.to_vec()).is_none() && join.next_expiry.is_none(), "a partial message armed an expiry");
        anyhow::ensure!(join.add("1:b", 2, 0, 2, br#"{"b":"#.to_vec()).is_none(), "the second partial message wasn't kept");
        anyhow::ensure!(join.add("2:c", 3, 0, 2, br#"{"c":"#.to_vec()).is_none(), "the third partial message wasn't kept");
        anyhow::ensure!(join.add("1:b", 2, 1, 3, br#"1}"#.to_vec()).is_none(), "a part of another message's count was taken");
        let keep = ("2:c".to_owned(), 3);
        anyhow::ensure!(join.evict_least_recent(&keep), "nothing was let go for room");
        anyhow::ensure!(!join.open.contains_key(&("1:a".to_owned(), 1)) && join.open.contains_key(&("1:b".to_owned(), 2)) && join.open.contains_key(&keep), "room let go of another than the least recent: {:?}", join.open.keys().collect::<Vec<_>>());
        join.forget_prefix("2:");
        anyhow::ensure!(!join.open.contains_key(&keep), "a forgotten sender's partial message stayed");
        let whole = join.add("1:b", 2, 1, 2, br#"1}"#.to_vec());
        anyhow::ensure!(whole.as_deref() == Some(br#"{"b":1}"#.as_slice()), "the completed message came back as {whole:?}");
        anyhow::ensure!(join.open.is_empty() && join.held == 0, "held {} after completing", join.held);
        Ok(())
    }

    #[test]
    fn cp_t32_receiver_spool_refuses_same_length_valid_json_corruption() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut join = Joiner::in_dir(dir.path().to_owned(), PARTS_WAIT);
        assert!(join.add("sender", 1, 0, 2, br#"{"value":"A"#.to_vec()).is_none());
        let staged = join.open.values_mut().next().and_then(|j| j.file.as_mut()).ok_or_else(|| anyhow::anyhow!("missing staged frame"))?;
        staged.seek(SeekFrom::End(-1))?;
        staged.write_all(b"B")?;
        assert!(serde_json::from_slice::<serde_json::Value>(br#"{"value":"B"}"#).is_ok());
        assert!(join.add("sender", 1, 1, 2, br#""}"#.to_vec()).is_none());
        assert!(join.open.is_empty() && join.held == 0);
        Ok(())
    }

    #[test]
    fn cp_t32_snapshot_spool_joins_out_of_order_without_retaining_payloads() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut join = Joiner::in_dir(dir.path().to_owned(), PARTS_WAIT);
        assert!(join.add("me", 1, 1, 3, vec![2]).is_none());
        let staged = join.open.values().next().ok_or_else(|| anyhow::anyhow!("missing sparse snapshot"))?;
        assert!(staged.file.is_some() && staged.parts.values().all(|p| matches!(p, Part::File { .. })));
        assert!(join.add("me", 1, 1, 3, vec![9]).is_none());
        assert!(join.add("me", 1, 0, 3, vec![1]).is_none());
        assert_eq!(join.add("me", 1, 2, 3, vec![3]), Some(vec![1, 2, 3]));
        assert!(join.open.is_empty());
        Ok(())
    }

    #[test]
    fn messages_round_trip_as_json() -> anyhow::Result<()> {
        let m = Msg::Want { repo: "r".into(), hashes: vec!["ab".into()] };
        assert_eq!(decode(&encode(&m)), Some(m));
        // A field this side doesn't know is ignored.
        assert!(decode(r#"{"kind":"release","later":1}"#).is_some());
        let order = Msg::Clone(CloneOrder { task: "t".into(), key: "github.com/a/b".into(), path: "/x".into(), bundle: Some("b".into()), hash: Some("ab".repeat(32)), size: 9, ..CloneOrder::default() });
        assert_eq!(decode(&encode(&order)), Some(order));
        let news = Msg::Task(TaskNews::new("t", "fetching", "9 bytes"));
        assert_eq!(decode(&encode(&news)), Some(news));
        // A kind this side doesn't know is skipped, as an older runner skips these.
        assert_eq!(decode(r#"{"kind":"teleport","to":"mars"}"#), None);
        // A copy's spec and status from before adopting read as they did.
        let spec: CopySpec = serde_json::from_str(r#"{"repo":"r","name":"n","member":2,"path":"/p"}"#)?;
        assert_eq!(spec.adopt, None);
        assert!(!encode(&Msg::Copies { copies: vec![spec], token: String::new() }).contains("adopt"));
        let status: CopyStatus = serde_json::from_str(r#"{"repo":"r","member":2,"applied":4}"#)?;
        assert!(!status.looked && !status.adopting);
        Ok(())
    }
}
