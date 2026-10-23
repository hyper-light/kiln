//! A synced repo's Raft group (`docs/sync.md`, "The group"): hyper-raft's core in hyper-durable's
//! shell. The server's member is the group's one voter, and so its leader; the machine of each copy
//! runs a member as a learner, which takes the log and proposes through the server. Each keeps the
//! repo's [`Tree`] as the log makes it: the server's member ([`Index`]) to image it for a copy behind
//! the log's start, and to know which contents the repo still names; a copy's ([`Feed`]) hands the
//! committed steps to the copy, which takes them into its folder once their contents are here, and
//! states durable only what its folder took, so a restart takes the rest from the log again.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::task::Waker;
use std::time::Duration;

use hyper_durable::{EntryRef, Fatal, LogStore, Output, Point, Replica, ReplicaError, Settings, StateMachine};
use hyper_raft::proto::{ConfChangeSingle, ConfChangeTransition, ConfChangeType, ConfChangeV2, ConfState, EntryType, Message};
use hyper_raft::{Config, Elections, Limits, Stated};
use serde::{Deserialize, Serialize};

use super::scan::Limits as CopyLimits;
use super::{Blobs, Change, Copy, Decision, Outcome, State, Step, Tree};

/// The server's member of every group: its one voter.
pub const SERVER: u64 = 1;
/// The largest message a member sends as hyper-raft's wire writes it, but for a snapshot, whose
/// image goes in parts (`wire::split`): it fits one frame of kiln's control lane with room to spare.
pub const MESSAGE: usize = 768 << 10;
/// The entries one append carries, at the most (an entry larger than it goes alone).
const APPEND: u64 = 256 << 10;
/// The most members a group names: the server and its copies.
const MEMBERS: usize = 64;
/// How often an owner ticks its groups. A leader beats every [`BEAT`] ticks, which tells the copies
/// what is committed when nothing else moves.
pub const TICK: Duration = Duration::from_millis(100);
const BEAT: usize = 10;
/// What a copy remembers of what its folder did, newest last.
const OUTCOMES: usize = 200;

/// A member's settings: `id` in the group, on a store that holds `depth` writes out at once.
pub fn settings(id: u64, depth: usize) -> anyhow::Result<Settings> {
    let limits = Limits::derive(Stated { message: MESSAGE, members: MEMBERS, memory: 64 << 20, depth })
        .map_err(|e| anyhow::anyhow!("the group's limits: {e:?}"))?;
    Ok(Settings {
        core: Config { election_tick: BEAT * 3, heartbeat_tick: BEAT, max_size_per_msg: APPEND, seed: id, ..Config::new(id, limits) },
        elections: Elections::Ticks,
        // Woken by events: the commit is written alone at the first moment no write is out.
        quiet: Duration::ZERO,
    })
}

/// The configuration every group starts from, on every member: the server alone. Each copy is a
/// change the log holds (a learner added), so a member that takes the log from its start meets them.
pub fn founded() -> ConfState {
    ConfState { voters: vec![SERVER], ..ConfState::default() }
}

/// A change that adds a copy's member as a learner, or removes it.
pub fn learner(member: u64, add: bool) -> ConfChangeV2 {
    ConfChangeV2 {
        transition: ConfChangeTransition::Auto,
        changes: vec![ConfChangeSingle { change_type: if add { ConfChangeType::AddLearnerNode } else { ConfChangeType::RemoveNode }, node_id: member }],
        context: Vec::new(),
    }
}

/// A step as a log entry holds it.
pub fn entry(step: &Step) -> Vec<u8> {
    serde_json::to_vec(step).unwrap_or_default()
}

/// The step a log entry holds; none for an entry that holds none (a leader's empty entry).
fn step_of(entry: &EntryRef<'_>) -> Option<Step> {
    if entry.kind != EntryType::EntryNormal || entry.data.is_empty() {
        return None;
    }
    serde_json::from_slice(entry.data).ok()
}

/// Drives `r` until it has nothing more to do now, keeping the messages it sends.
pub fn pump<L: LogStore, M: StateMachine>(
    r: &mut Replica<L, M>,
    now: u64,
    waker: &Waker,
    out: &mut Output<M::Answer>,
    messages: &mut Vec<Message>,
) -> Result<(), ReplicaError> {
    for _ in 0..1024 {
        out.clear();
        let driven = r.drive(now, waker, out)?;
        messages.append(&mut out.messages);
        if !driven.more {
            break;
        }
    }
    Ok(())
}

/// A configuration as kiln saves it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conf {
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    #[serde(default)]
    pub voters_outgoing: Vec<u64>,
    #[serde(default)]
    pub learners_next: Vec<u64>,
    #[serde(default)]
    pub auto_leave: bool,
}

impl From<&ConfState> for Conf {
    fn from(c: &ConfState) -> Self {
        Self {
            voters: c.voters.clone(),
            learners: c.learners.clone(),
            voters_outgoing: c.voters_outgoing.clone(),
            learners_next: c.learners_next.clone(),
            auto_leave: c.auto_leave,
        }
    }
}

impl From<&Conf> for ConfState {
    fn from(c: &Conf) -> Self {
        Self {
            voters: c.voters.clone(),
            learners: c.learners.clone(),
            voters_outgoing: c.voters_outgoing.clone(),
            learners_next: c.learners_next.clone(),
            auto_leave: c.auto_leave,
        }
    }
}

/// Writes `bytes` to `path` whole or not at all, durably: a temporary file beside it, flushed,
/// renamed over it, and the folder flushed.
pub fn save_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("tmp-{}", crate::util::new_id()));
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

// ------------------------------------------------------------------------------ the server's member

/// What the server keeps of a repo's tree beside the group's log.
#[derive(Serialize, Deserialize)]
struct SavedIndex {
    tree: Tree,
    index: u64,
    term: u64,
    conf: Conf,
}

/// The server's member's state machine: the repo's tree, saved when the shell asks (before a
/// compaction) and imaged for a copy the log no longer holds enough for.
pub struct Index {
    pub tree: Tree,
    applied: Point,
    configuration: ConfState,
    durable: Point,
    /// Where the tree is saved; none: kept in memory alone.
    file: Option<PathBuf>,
    /// Steps taken since the owner last looked: what the repo's activity shows.
    pub taken: Vec<Taken>,
}

/// A step the server's member took, at `index` of the log: what it decided for each change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taken {
    pub index: u64,
    pub step: Step,
    pub decisions: Vec<Decision>,
}

impl Index {
    /// The tree saved at `file`, or a new repo's.
    pub fn open(file: Option<PathBuf>) -> anyhow::Result<Self> {
        let saved = match &file {
            Some(f) if f.exists() => Some(serde_json::from_slice::<SavedIndex>(&std::fs::read(f)?)?),
            _ => None,
        };
        Ok(match saved {
            Some(s) => {
                let at = Point { index: s.index, term: s.term };
                Self { tree: s.tree, applied: at, configuration: (&s.conf).into(), durable: at, file, taken: Vec::new() }
            }
            None => Self { tree: Tree::default(), applied: Point::default(), configuration: founded(), durable: Point::default(), file, taken: Vec::new() },
        })
    }

    pub fn applied(&self) -> Point {
        self.applied
    }

    fn save(&mut self) -> Result<(), Fatal> {
        if let Some(file) = &self.file {
            let saved = SavedIndex { tree: self.tree.clone(), index: self.applied.index, term: self.applied.term, conf: (&self.configuration).into() };
            let bytes = serde_json::to_vec(&saved).map_err(|_| Fatal("the repo's tree would not encode"))?;
            save_atomic(file, &bytes).map_err(|_| Fatal("the repo's tree could not be saved"))?;
        }
        self.durable = self.applied;
        Ok(())
    }
}

impl StateMachine for Index {
    type Answer = ();

    fn apply(&mut self, entry: &EntryRef<'_>, _answers: &mut Vec<()>) -> Result<(), Fatal> {
        self.applied = Point { index: entry.index, term: entry.term };
        if let Some(step) = step_of(entry)
            && let Some(decisions) = self.tree.take(&step)
        {
            self.taken.push(Taken { index: entry.index, step, decisions });
        }
        Ok(())
    }

    fn apply_change(&mut self, at: Point, _change: &ConfChangeV2, configuration: &ConfState) -> Result<(), Fatal> {
        self.applied = at;
        self.configuration = configuration.clone();
        Ok(())
    }

    fn durable(&self) -> Point {
        self.durable
    }

    fn configuration(&self) -> &ConfState {
        &self.configuration
    }

    fn acts_at_start(&self, _entry: &EntryRef<'_>) -> bool {
        false
    }

    fn image(&mut self, into: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        into.extend(serde_json::to_vec(&self.tree).map_err(|_| Fatal("the repo's tree would not encode"))?);
        Ok((self.applied, self.configuration.clone()))
    }

    fn image_bytes(&self) -> Option<u64> {
        serde_json::to_vec(&self.tree).ok().map(|b| b.len() as u64)
    }

    fn install(&mut self, image: &[u8], at: Point, configuration: &ConfState) -> Result<(), Fatal> {
        self.tree = serde_json::from_slice(image).map_err(|_| Fatal("an image that is no repo's tree"))?;
        self.applied = at;
        self.configuration = configuration.clone();
        self.save()
    }

    fn persist(&mut self) -> Result<(), Fatal> {
        self.save()
    }
}

// --------------------------------------------------------------------------------- a copy's member

/// What a copy keeps on disk, written whole as its folder takes the log: its state and the point
/// of the log it reached are one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Saved {
    pub state: State,
    pub index: u64,
    pub term: u64,
    pub conf: Conf,
    /// A tree the copy is taking into its folder: an image the log sent, its own entries gone.
    #[serde(default)]
    pub install: Option<Tree>,
}

/// What the log committed that the folder hasn't taken, in log order.
#[derive(Debug)]
enum Fed {
    Step(Point, Step),
    /// An entry with no step: a leader's empty entry.
    Mark(Point),
    Change(Point, ConfState),
}

/// A copy's member's state machine: the copy itself, fed the committed steps in order.
pub struct Feed {
    pub copy: Copy,
    file: PathBuf,
    queue: VecDeque<Fed>,
    applied: Point,
    configuration: ConfState,
    /// Through where the folder and the saved state have taken the log.
    durable: Point,
    durable_conf: ConfState,
    /// Where the folder has reached, saved or not.
    reached: Point,
    reached_conf: ConfState,
    install: Option<Tree>,
    /// What the folder did, newest last.
    pub outcomes: VecDeque<Outcome>,
    /// Steps the folder took since the runner last looked (its metrics and status).
    pub took: Vec<Took>,
}

/// A committed step a copy's folder took: whose, when its machine saw it, and what it did to the
/// files here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Took {
    pub by: String,
    /// Unix milliseconds, the clock of the machine that made it.
    pub at: i64,
    /// This copy's own step, back from the log.
    pub ours: bool,
    pub applied: usize,
    pub conflicts: usize,
    pub kept: usize,
    pub refused: usize,
}

impl Took {
    fn of(step: &Step, ours: bool, outcomes: &[Outcome]) -> Self {
        let mut t = Self { by: step.by.clone(), at: step.at, ours, ..Self::default() };
        for o in outcomes {
            match o {
                Outcome::Applied { .. } => t.applied += 1,
                Outcome::Conflict { .. } => t.conflicts += 1,
                Outcome::Kept { .. } => t.kept += 1,
                Outcome::Refused { .. } => t.refused += 1,
            }
        }
        t
    }
}

impl Feed {
    /// The copy at `root`, its state saved at `file` (a new copy's when there is none).
    pub fn open(root: &Path, me: &str, member: u64, limits: CopyLimits, file: PathBuf) -> anyhow::Result<Self> {
        let saved: Saved = if file.exists() { serde_json::from_slice(&std::fs::read(&file)?)? } else { Saved { conf: (&founded()).into(), ..Saved::default() } };
        let at = Point { index: saved.index, term: saved.term };
        let conf: ConfState = (&saved.conf).into();
        Ok(Self {
            copy: Copy::new(root, me, member, limits, saved.state),
            file,
            queue: VecDeque::new(),
            applied: at,
            configuration: conf.clone(),
            durable: at,
            durable_conf: conf.clone(),
            reached: at,
            reached_conf: conf,
            install: saved.install,
            outcomes: VecDeque::new(),
            took: Vec::new(),
        })
    }

    /// Through where the folder has taken the log, and how much it hasn't taken yet.
    pub fn progress(&self) -> (u64, usize) {
        (self.reached.index, self.queue.len() + usize::from(self.install.is_some()))
    }

    /// Whether the folder has taken everything the log committed so far.
    pub fn idle(&self) -> bool {
        self.queue.is_empty() && self.install.is_none()
    }

    /// Takes what the log committed into the folder, as far as the contents here allow: empty when
    /// it is through, else the blobs it waits for.
    pub fn work(&mut self, blobs: &Blobs) -> io::Result<Vec<String>> {
        let mut moved = false;
        let lacking = |needs: Vec<String>| -> Vec<String> { needs.into_iter().filter(|h| !blobs.has(h)).collect() };
        if let Some(tree) = self.install.clone() {
            let step = Step { by: "kiln".into(), member: 0, seq: 0, at: 0, changes: self.copy.state.tree.diff(&tree) };
            let missing = lacking(self.copy.needs(&step));
            if !missing.is_empty() {
                return Ok(missing);
            }
            let outcomes = self.copy.apply(&step, blobs)?;
            self.note(outcomes);
            let state = &mut self.copy.state;
            state.tree.seqs = tree.seqs.clone();
            state.tree.bundles = tree.bundles.clone();
            // What the image holds of this copy's steps needn't be sent again.
            let taken = tree.seqs.get(&self.copy.member).copied().unwrap_or(0);
            state.sent.retain(|s| s.seq > taken);
            state.pending = state.sent.iter().flat_map(|s| s.changes.iter()).map(|c: &Change| (c.path.clone(), c.to.clone())).collect();
            self.install = None;
            moved = true;
        }
        while let Some(front) = self.queue.front() {
            match front {
                Fed::Step(at, step) => {
                    let missing = lacking(self.copy.needs(step));
                    if !missing.is_empty() {
                        if moved {
                            self.save()?;
                        }
                        return Ok(missing);
                    }
                    let at = *at;
                    let outcomes = self.copy.apply(step, blobs)?;
                    let ours = step.member != 0 && step.member == self.copy.member;
                    self.took.push(Took::of(step, ours, &outcomes));
                    self.note(outcomes);
                    self.reached = at;
                }
                Fed::Mark(at) => self.reached = *at,
                Fed::Change(at, conf) => {
                    self.reached = *at;
                    self.reached_conf = conf.clone();
                }
            }
            self.queue.pop_front();
            moved = true;
        }
        if moved {
            self.save()?;
        }
        Ok(Vec::new())
    }

    /// Saves the copy's state as the folder has it; then that point is durable.
    pub fn save(&mut self) -> io::Result<()> {
        let saved = Saved {
            state: self.copy.state.clone(),
            index: self.reached.index,
            term: self.reached.term,
            conf: (&self.reached_conf).into(),
            install: self.install.clone(),
        };
        save_atomic(&self.file, &serde_json::to_vec(&saved)?)?;
        self.durable = self.reached;
        self.durable_conf = self.reached_conf.clone();
        Ok(())
    }

    fn note(&mut self, outcomes: Vec<Outcome>) {
        for o in outcomes {
            if self.outcomes.len() >= OUTCOMES {
                self.outcomes.pop_front();
            }
            self.outcomes.push_back(o);
        }
    }

    /// The contents this copy still needs: what its queue and an install name, and what its unsent
    /// steps carry to the server.
    pub fn named(&self) -> std::collections::HashSet<String> {
        let file = |v: &Option<super::Version>| match v {
            Some(super::Version::File { hash, .. }) => Some(hash.clone()),
            _ => None,
        };
        let queued = self.queue.iter().filter_map(|f| match f {
            Fed::Step(_, s) => Some(s),
            _ => None,
        });
        let mut out: std::collections::HashSet<String> =
            queued.chain(self.copy.state.sent.iter()).flat_map(|s| s.changes.iter()).flat_map(|c| file(&c.to).into_iter().chain(c.bundle.clone())).collect();
        if let Some(t) = &self.install {
            out.extend(t.files.values().filter_map(|v| file(&Some(v.clone()))));
            out.extend(t.bundles.values().cloned());
        }
        out
    }

    /// Conflict copies in the tree, each with the path it stands beside.
    pub fn conflicts(&self) -> BTreeMap<String, String> {
        conflicts(&self.copy.state.tree)
    }
}

/// The conflict copies a tree holds (`<name>.conflict-<machine>-<when><.ext>`), each with the path
/// it stands beside.
pub fn conflicts(tree: &Tree) -> BTreeMap<String, String> {
    tree.files
        .keys()
        .filter_map(|p| {
            let (dir, name) = p.rsplit_once('/').map_or(("", p.as_str()), |(d, n)| (d, n));
            let (stem, rest) = name.split_once(".conflict-")?;
            // `<machine>-<YYYYMMDD>-<HHMMSS><.ext>`, a machine's name may hold dots: the extension
            // is what follows the time.
            let ext = rest.get(stamp_end(rest)?..)?;
            let original = format!("{stem}{ext}");
            let original = if dir.is_empty() { original } else { format!("{dir}/{original}") };
            Some((p.clone(), original))
        })
        .collect()
}

/// Where the last `-YYYYMMDD-HHMMSS` in `s` ends.
fn stamp_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    (0..b.len()).rev().find(|&i| b.get(i) == Some(&b'-') && b.get(i + 9) == Some(&b'-') && digits(i + 1..i + 9) && digits(i + 10..i + 16)).map(|i| i + 16)
}

impl StateMachine for Feed {
    type Answer = ();

    fn apply(&mut self, entry: &EntryRef<'_>, _answers: &mut Vec<()>) -> Result<(), Fatal> {
        let at = Point { index: entry.index, term: entry.term };
        self.applied = at;
        self.queue.push_back(match step_of(entry) {
            Some(step) => Fed::Step(at, step),
            None => Fed::Mark(at),
        });
        Ok(())
    }

    fn apply_change(&mut self, at: Point, _change: &ConfChangeV2, configuration: &ConfState) -> Result<(), Fatal> {
        self.applied = at;
        self.configuration = configuration.clone();
        self.queue.push_back(Fed::Change(at, configuration.clone()));
        Ok(())
    }

    fn durable(&self) -> Point {
        self.durable
    }

    fn configuration(&self) -> &ConfState {
        &self.configuration
    }

    fn acts_at_start(&self, _entry: &EntryRef<'_>) -> bool {
        false
    }

    /// The tree as the saved state has it: behind what was applied, never behind the log's start.
    fn image(&mut self, into: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        let tree = self.install.as_ref().unwrap_or(&self.copy.state.tree);
        into.extend(serde_json::to_vec(tree).map_err(|_| Fatal("the copy's tree would not encode"))?);
        Ok((self.durable, self.durable_conf.clone()))
    }

    fn image_bytes(&self) -> Option<u64> {
        serde_json::to_vec(self.install.as_ref().unwrap_or(&self.copy.state.tree)).ok().map(|b| b.len() as u64)
    }

    /// The leader's tree, for a copy behind its log's start: saved before this returns, and taken
    /// into the folder as its contents come ([`Feed::work`]).
    fn install(&mut self, image: &[u8], at: Point, configuration: &ConfState) -> Result<(), Fatal> {
        let tree: Tree = serde_json::from_slice(image).map_err(|_| Fatal("an image that is no repo's tree"))?;
        self.queue.clear();
        self.install = Some(tree);
        self.applied = at;
        self.configuration = configuration.clone();
        self.reached = at;
        self.reached_conf = configuration.clone();
        self.save().map_err(|_| Fatal("the copy's state could not be saved"))
    }

    fn persist(&mut self) -> Result<(), Fatal> {
        self.save().map_err(|_| Fatal("the copy's state could not be saved"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_durable::{RamStore, Unbounded};

    type Leader = Replica<RamStore, Index>;
    type Learner = Replica<RamStore, Feed>;

    /// The server's member and copies, each message delivered at once (unless its copy is away),
    /// every copy's folder taking what is committed; blobs fetched means one shared store.
    struct Group {
        dir: tempfile::TempDir,
        blobs: Blobs,
        leader: Leader,
        copies: BTreeMap<u64, Learner>,
        away: Vec<u64>,
        clock: u64,
    }

    fn e(err: impl std::fmt::Debug) -> anyhow::Error {
        anyhow::anyhow!("{err:?}")
    }

    impl Group {
        fn new() -> anyhow::Result<Self> {
            let dir = tempfile::tempdir()?;
            let blobs = Blobs::new(&dir.path().join("blobs"))?;
            let mut leader = Replica::open(&settings(SERVER, 1)?, RamStore::new(), Index::open(None)?, Unbounded).map_err(e)?;
            leader.campaign().map_err(e)?;
            let mut g = Self { dir, blobs, leader, copies: BTreeMap::new(), away: Vec::new(), clock: 0 };
            g.settle()?;
            anyhow::ensure!(g.leader.is_leader(), "the sole voter leads");
            Ok(g)
        }

        fn feed(&self, member: u64) -> anyhow::Result<Feed> {
            let root = self.dir.path().join(format!("copy-{member}"));
            std::fs::create_dir_all(&root)?;
            Feed::open(&root, &format!("m{member}"), member, CopyLimits::default(), self.dir.path().join(format!("state-{member}.json")))
        }

        fn add(&mut self, member: u64) -> anyhow::Result<()> {
            let r = Replica::open(&settings(member, 1)?, RamStore::new(), self.feed(member)?, Unbounded).map_err(e)?;
            self.copies.insert(member, r);
            self.leader.change(Vec::new(), &learner(member, true)).map_err(e)?;
            self.settle()
        }

        /// A folder kiln just cloned joins, to take the group's files through the log index `at`.
        fn add_adopting(&mut self, member: u64, at: u64) -> anyhow::Result<()> {
            let mut feed = self.feed(member)?;
            feed.copy.adopt(at)?;
            let r = Replica::open(&settings(member, 1)?, RamStore::new(), feed, Unbounded).map_err(e)?;
            self.copies.insert(member, r);
            self.leader.change(Vec::new(), &learner(member, true)).map_err(e)?;
            self.settle()
        }

        /// The copy's process starts again: its member reopens on what its log and its saved
        /// state hold.
        fn restart(&mut self, member: u64) -> anyhow::Result<()> {
            let mut old = self.copies.remove(&member).ok_or_else(|| anyhow::anyhow!("no copy {member}"))?;
            let log = old.log_mut().clone();
            drop(old);
            let r = Replica::open(&settings(member, 1)?, log, self.feed(member)?, Unbounded).map_err(e)?;
            self.copies.insert(member, r);
            // The leader's next beat finds what the copy lacks, as its ticks would.
            self.leader.beat().map_err(e)?;
            self.settle()
        }

        fn settle(&mut self) -> anyhow::Result<()> {
            let waker = Waker::noop();
            for _ in 0..500 {
                self.clock += 1_000_000;
                let mut messages = Vec::new();
                pump(&mut self.leader, self.clock, waker, &mut Output::default(), &mut messages).map_err(e)?;
                self.leader.machine_mut().taken.clear();
                for (member, r) in &mut self.copies {
                    if self.away.contains(member) {
                        continue;
                    }
                    pump(r, self.clock, waker, &mut Output::default(), &mut messages).map_err(e)?;
                    let missing = r.machine_mut().work(&self.blobs)?;
                    anyhow::ensure!(missing.is_empty(), "blobs missing from the shared store: {missing:?}");
                }
                let mut moved = false;
                for m in messages {
                    if m.to == SERVER {
                        self.leader.step(m).map_err(e)?;
                        moved = true;
                    } else if !self.away.contains(&m.to)
                        && let Some(r) = self.copies.get_mut(&m.to)
                    {
                        r.step(m).map_err(e)?;
                        moved = true;
                    }
                }
                let out = self.leader.in_flight() + self.copies.values().map(Replica::in_flight).sum::<usize>();
                if !moved && out == 0 {
                    return Ok(());
                }
            }
            anyhow::bail!("the group never settled")
        }

        fn write(&self, member: u64, path: &str, body: &str) -> io::Result<()> {
            let p = self.dir.path().join(format!("copy-{member}")).join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(p, body)
        }

        fn read(&self, member: u64, path: &str) -> Option<String> {
            std::fs::read_to_string(self.dir.path().join(format!("copy-{member}")).join(path)).ok()
        }

        /// What changed on a copy is proposed, through the server.
        fn propose(&mut self, member: u64, at: i64) -> anyhow::Result<Vec<Step>> {
            let r = self.copies.get_mut(&member).ok_or_else(|| anyhow::anyhow!("no copy {member}"))?;
            let steps = r.machine_mut().copy.propose(&self.blobs, at)?;
            for s in &steps {
                self.leader.propose(Vec::new(), entry(s)).map_err(e)?;
            }
            self.settle()?;
            Ok(steps)
        }

        fn tree(&self, member: u64) -> Option<&Tree> {
            self.copies.get(&member).map(|r| &r.machine().copy.state.tree)
        }
    }

    #[test]
    fn a_change_on_one_copy_reaches_the_others_through_the_group() -> anyhow::Result<()> {
        let mut g = Group::new()?;
        g.add(2)?;
        g.add(3)?;
        assert_eq!(g.leader.configuration().learners, vec![2, 3]);
        g.write(2, "src/a.txt", "hello")?;
        assert_eq!(g.propose(2, 1_000)?.len(), 1);
        assert_eq!(g.read(3, "src/a.txt").as_deref(), Some("hello"));
        g.write(3, "src/a.txt", "hello from 3")?;
        g.propose(3, 2_000)?;
        assert_eq!(g.read(2, "src/a.txt").as_deref(), Some("hello from 3"));
        assert_eq!(g.tree(2), Some(&g.leader.machine().tree), "every member's tree is the leader's");
        assert_eq!(g.tree(3), Some(&g.leader.machine().tree));
        assert!(g.propose(2, 3_000)?.is_empty() && g.propose(3, 3_000)?.is_empty(), "nothing left to say");
        Ok(())
    }

    #[test]
    fn a_step_proposed_twice_is_taken_once() -> anyhow::Result<()> {
        let mut g = Group::new()?;
        g.add(2)?;
        g.add(3)?;
        g.write(2, "x.txt", "one")?;
        let first = g.propose(2, 1_000)?;
        g.write(3, "x.txt", "two")?;
        g.propose(3, 2_000)?;
        // The first step again (sent twice, say): taken before, it changes nothing.
        for s in &first {
            g.leader.propose(Vec::new(), entry(s)).map_err(e)?;
        }
        g.settle()?;
        for m in [2, 3] {
            assert_eq!(g.read(m, "x.txt").as_deref(), Some("two"));
            assert!(g.tree(m).is_some_and(|t| conflicts(t).is_empty()), "no conflict from a step taken twice");
        }
        Ok(())
    }

    #[test]
    fn a_copy_added_after_the_log_was_compacted_takes_an_image() -> anyhow::Result<()> {
        let mut g = Group::new()?;
        g.add(2)?;
        g.write(2, "a.md", "kept")?;
        g.write(2, "dir/b.rs", "fn b() {}")?;
        g.propose(2, 1_000)?;
        let clock = g.clock;
        assert!(g.leader.compact(0, clock, Waker::noop()).map_err(e)?, "a compaction's start went out");
        g.settle()?;
        // A machine that already holds one of the files, the same, and one of its own.
        g.write(4, "a.md", "kept")?;
        g.write(4, "local.txt", "mine")?;
        g.add(4)?;
        assert_eq!(g.read(4, "dir/b.rs").as_deref(), Some("fn b() {}"));
        assert_eq!(g.tree(4), Some(&g.leader.machine().tree));
        // What it had of its own is proposed like any change, and reaches the others.
        g.propose(4, 2_000)?;
        assert_eq!(g.read(2, "local.txt").as_deref(), Some("mine"));
        assert!(g.tree(2).is_some_and(|t| conflicts(t).is_empty()), "the same file on both is no conflict");
        Ok(())
    }

    #[test]
    fn a_copy_that_was_away_catches_up_after_a_restart() -> anyhow::Result<()> {
        let mut g = Group::new()?;
        g.add(2)?;
        g.add(3)?;
        g.away.push(3);
        g.write(2, "late.txt", "while 3 was away")?;
        g.propose(2, 1_000)?;
        assert_eq!(g.read(3, "late.txt"), None);
        g.away.clear();
        g.restart(3)?;
        assert_eq!(g.read(3, "late.txt").as_deref(), Some("while 3 was away"));
        // And a restart with nothing new changes nothing.
        g.restart(2)?;
        assert_eq!(g.tree(2), g.tree(3));
        Ok(())
    }

    #[test]
    fn a_clone_that_joins_takes_the_source_s_working_tree_through_the_log() -> anyhow::Result<()> {
        let mut g = Group::new()?;
        g.add(2)?;
        // The source: the commit both have, then an edit, a new file and a deletion not committed.
        g.write(2, "a.txt", "an edit")?;
        g.write(2, "src/new.rs", "fn new() {}")?;
        g.write(2, "same.md", "same")?;
        g.propose(2, 1_000)?;
        // The clone of that commit, on another machine.
        g.write(3, "a.txt", "committed")?;
        g.write(3, "gone.txt", "deleted on the source")?;
        g.write(3, "same.md", "same")?;
        let at = g.leader.applied().index;
        g.add_adopting(3, at)?;
        let blobs = g.blobs.clone();
        let feed = g.copies.get_mut(&3).map(Replica::machine_mut).ok_or_else(|| anyhow::anyhow!("no copy 3"))?;
        assert!(feed.idle() && feed.progress().0 >= at, "the log brought it through the index it adopts at");
        assert!(feed.copy.adopting());
        feed.copy.adopted(&blobs)?;
        assert_eq!(g.read(3, "a.txt").as_deref(), Some("an edit"));
        assert_eq!(g.read(3, "src/new.rs").as_deref(), Some("fn new() {}"));
        assert_eq!(g.read(3, "gone.txt"), None);
        assert!(g.propose(3, 2_000)?.is_empty(), "in step: nothing to say");
        assert_eq!(g.tree(3), Some(&g.leader.machine().tree));
        assert!(conflicts(&g.leader.machine().tree).is_empty(), "no conflict copy anywhere");
        // From now on it is a copy like any other.
        g.write(3, "a.txt", "from the new machine")?;
        g.propose(3, 3_000)?;
        assert_eq!(g.read(2, "a.txt").as_deref(), Some("from the new machine"));
        Ok(())
    }

    #[test]
    fn conflict_copies_are_found_beside_their_names() {
        let mut t = Tree::default();
        let v = crate::sync::Version::File { hash: "h".into(), exec: false };
        for p in ["src/x.rs", "src/x.conflict-mac.local-20261017-151200.rs", "notes.conflict-b-19700101-000002", "plain.txt"] {
            t.files.insert(p.into(), v.clone());
        }
        let found = conflicts(&t);
        assert_eq!(found.get("src/x.conflict-mac.local-20261017-151200.rs").map(String::as_str), Some("src/x.rs"));
        assert_eq!(found.get("notes.conflict-b-19700101-000002").map(String::as_str), Some("notes"));
        assert_eq!(found.len(), 2);
    }
}
