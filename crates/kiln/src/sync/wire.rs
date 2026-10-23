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

/// A repository this machine holds that others hold too: where its work goes next (`PUT
/// /api/sync/bundles/:batch`), and whether the server has none of it yet (it goes whatever it is).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareSlot {
    pub key: String,
    pub batch: String,
    pub fresh: bool,
}

/// A machine's work in a repository: its branches, the work in progress of each folder of it (a
/// commit of its files as they are, on the branch checked out), and the bundle of the commits
/// those name that its remote lacks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Work {
    pub key: String,
    /// The machine it's from (the server fills it in).
    pub from: String,
    pub batch: String,
    /// Branch → commit.
    pub branches: BTreeMap<String, String>,
    /// Branch checked out → its work in progress.
    pub wip: BTreeMap<String, String>,
    /// The bundle, when there's one: its SHA-256 and size.
    pub hash: Option<String>,
    pub size: u64,
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

/// Parts of messages, put back together.
#[derive(Default)]
pub struct Joiner {
    open: HashMap<(String, u64), Joining>,
}

struct Joining {
    parts: Vec<Option<Vec<u8>>>,
    have: usize,
    since: Instant,
}

impl Joiner {
    /// A message's bytes once its last part came (`parts` 0: it came whole). The parts of a message
    /// whose rest never came are dropped after a while: Raft sends it again.
    pub fn add(&mut self, from: &str, id: u64, part: u32, parts: u32, bytes: Vec<u8>) -> Option<Vec<u8>> {
        if parts <= 1 {
            return Some(bytes);
        }
        self.open.retain(|_, j| j.since.elapsed() < PARTS_WAIT);
        let key = (from.to_owned(), id);
        if !self.open.contains_key(&key) && self.open.len() >= JOINING {
            return None;
        }
        let total = usize::try_from(parts).ok()?;
        let j = self.open.entry(key.clone()).or_insert_with(|| Joining { parts: vec![None; total], have: 0, since: Instant::now() });
        let slot = j.parts.get_mut(usize::try_from(part).ok()?)?;
        if slot.is_none() {
            *slot = Some(bytes);
            j.have += 1;
        }
        if j.have < j.parts.len() {
            return None;
        }
        let j = self.open.remove(&key)?;
        Some(j.parts.into_iter().flatten().flatten().collect())
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
