//! Repo sync (`docs/sync.md`): one repository kept on several machines. This is the engine one
//! copy runs: noticing what changed here (steps to propose), and applying committed steps in log
//! order under the rule that makes every copy converge on the same files without losing a change.
//! The repo's Raft group orders the steps ([`group`]).
//!
//! The rule: every copy applies the same steps in the same order. A change made from the version
//! the log has is applied; one made from an older version (another machine's change committed
//! first) is a conflict, and its version goes beside the path, under a name made of the machine and
//! the step's time, on every copy alike. A copy never overwrites a file changed on it that it hasn't
//! proposed yet: that change is proposed from the version it was made from, and meets the rule.

pub mod activity;
pub mod blobs;
pub mod disk;
pub mod fs;
pub mod git;
pub mod group;
pub mod places;
pub mod repos;
pub mod runner;
pub mod scan;
pub mod server;
pub mod tasks;
pub mod wire;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use blobs::Blobs;
use fs::Fs;
use scan::{Entry, Limits};

/// What a path holds.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Version {
    File { hash: String, exec: bool },
    Link { target: String },
}

/// One path's change: what it becomes (none: it is deleted), from what (none: it was new here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Version>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Version>,
}

/// Changes in one step at the most: a look that finds more proposes several, each small enough for
/// one log entry and one message.
pub const STEP_CHANGES: usize = 512;

/// One entry of a repo's log: the changes one machine saw together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The machine, as conflict copies are named.
    pub by: String,
    /// The member of the repo's group that proposed it, and its number there: a member's steps are
    /// taken in order, each once.
    #[serde(default)]
    pub member: u64,
    #[serde(default)]
    pub seq: u64,
    /// Unix milliseconds.
    pub at: i64,
    pub changes: Vec<Change>,
}

/// The repo as the log makes it, alike on every member: each path's version, and the last step
/// taken from each member.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    pub files: BTreeMap<String, Version>,
    #[serde(default)]
    pub seqs: BTreeMap<u64, u64>,
}

/// What a committed change did to the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The path holds the change's version now.
    Take,
    /// The tree held that version already.
    Same,
    /// A conflict: the tree keeps what it holds, and the change's version is beside it under this
    /// name (none: a delete, dropped).
    Beside(Option<String>),
    /// Not a path kiln writes: dropped.
    Refuse,
}

impl Tree {
    /// Takes a committed step: what it decided for each change, or none when the step was taken
    /// before (a member's step proposed twice). A step numbered 0 is taken whatever came before.
    pub fn take(&mut self, step: &Step) -> Option<Vec<Decision>> {
        if step.seq != 0 {
            let last = self.seqs.entry(step.member).or_default();
            if step.seq <= *last {
                return None;
            }
            *last = step.seq;
        }
        let decide = |files: &mut BTreeMap<String, Version>, c: &Change| {
            if !safe_path(&c.path) {
                return Decision::Refuse;
            }
            let now = files.get(&c.path);
            if c.base.as_ref() == now {
                set(files, &c.path, c.to.clone());
                return Decision::Take;
            }
            if c.to.as_ref() == now {
                return Decision::Same;
            }
            let name = c.to.as_ref().map(|_| conflict_name(&c.path, &step.by, step.at)).filter(|n| safe_path(n));
            if let (Some(name), Some(v)) = (&name, &c.to) {
                files.insert(name.clone(), v.clone());
            }
            Decision::Beside(name)
        };
        Some(step.changes.iter().map(|c| decide(&mut self.files, c)).collect())
    }

    /// The step that turns this tree into `to`, each change made from what this tree holds.
    pub fn diff(&self, to: &BTreeMap<String, Version>) -> Vec<Change> {
        let paths: BTreeSet<&String> = self.files.keys().chain(to.keys()).collect();
        paths
            .into_iter()
            .filter(|p| self.files.get(*p) != to.get(*p))
            .map(|p| Change { path: p.clone(), to: to.get(p).cloned(), base: self.files.get(p).cloned() })
            .collect()
    }
}

/// A copy's state, kept between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The repo as the log has made it so far.
    pub tree: Tree,
    /// What this copy's files held when they were last in step with the log: a file that differs
    /// was changed here.
    pub synced: BTreeMap<String, Version>,
    /// What this copy proposed and the log hasn't brought back: each path's version (none: deleted).
    pub pending: BTreeMap<String, Option<Version>>,
    /// The last scan, so unchanged files aren't read again.
    pub seen: BTreeMap<String, Entry>,
    /// Steps proposed here that the log hasn't brought back, oldest first: sent again until it does.
    #[serde(default)]
    pub sent: Vec<Step>,
    /// A folder kiln just cloned for this copy takes the group's files once the log has brought it
    /// through this index ([`Copy::adopt`]); none once it has, or for a copy that joined as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopt: Option<u64>,
    /// Paths whose version a step brought was kept back, changed here then (`Outcome::Kept`): the
    /// log's version comes once the change here is proposed, or gone ([`Copy::catch_up`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub behind: BTreeSet<String>,
}

/// What applying a change did here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Applied { path: String },
    /// The log kept what it had; the step's version went beside it (`copy`; none for a delete).
    Conflict { path: String, copy: Option<String> },
    /// Changed here and not proposed yet, or proposed and on its way: left as it is.
    Kept { path: String },
    Refused { path: String, why: String },
}

/// One copy of a synced repo, on this machine.
#[derive(Debug, Clone)]
pub struct Copy {
    pub root: PathBuf,
    /// This machine, as steps name it.
    pub me: String,
    /// This copy's member of the repo's group.
    pub member: u64,
    pub limits: Limits,
    pub state: State,
    /// What the last look left out.
    pub skipped: Vec<scan::Skipped>,
    /// Contents the server no longer has: versions later steps replaced, for a copy far behind.
    /// Such a version is never written here; the later one is.
    pub gone: HashSet<String>,
    /// The filesystem its folder is on (the machine's own; a simulated one in kiln-sim's models).
    pub fs: Arc<dyn Fs>,
    /// Folders whose names changed since its state was last saved: its own (a file renamed in or
    /// removed, a folder made or removed) and the contents store's its contents went into. Synced
    /// before the state is ([`Copy::sync_dirs`]), so a power loss can't undo what the saved state
    /// says the folder holds, or take the contents its steps carry. One entry a folder.
    pub touched: BTreeSet<PathBuf>,
    /// A test build's planted bug (kiln-sim's T-T6.1.1): what sync writes into the folder renamed
    /// in with its bytes unsynced and its folder never synced, as before the first bug kiln-sim
    /// found was fixed. A power loss then tears, zeroes or loses it.
    #[cfg(test)]
    pub planted_unsynced: bool,
}

/// Whether a path from a step may be written here: relative, `/`-separated, without `.`, `..` or
/// empty parts, outside `.git/` and kiln's temporary files.
pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && !(cfg!(windows) && path.contains(':'))
        && path.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
        && path != ".git"
        && !path.starts_with(".git/")
        && !path.rsplit('/').next().unwrap_or(path).starts_with(scan::TEMP_PREFIX)
}

/// Where a step's version of `path` goes when the log kept another: beside it, named for the
/// machine and the step's time (UTC), so every copy picks the same name.
pub fn conflict_name(path: &str, by: &str, at_ms: i64) -> String {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    let when = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000)
        .map(|t| format!("{:04}{:02}{:02}-{:02}{:02}{:02}", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second()))
        .unwrap_or_else(|_| at_ms.to_string());
    let machine: String = by.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' }).collect();
    let file = format!("{stem}.conflict-{machine}-{when}{ext}");
    if dir.is_empty() { file } else { format!("{dir}/{file}") }
}

fn set(map: &mut BTreeMap<String, Version>, path: &str, v: Option<Version>) {
    match v {
        Some(v) => {
            map.insert(path.to_owned(), v);
        }
        None => {
            map.remove(path);
        }
    }
}

impl Copy {
    pub fn new(root: &Path, me: &str, member: u64, limits: Limits, state: State) -> Self {
        Self::on(fs::std_fs(), root, me, member, limits, state)
    }

    /// A copy whose folder is on `fs`.
    pub fn on(fs: Arc<dyn Fs>, root: &Path, me: &str, member: u64, limits: Limits, state: State) -> Self {
        Self {
            root: root.to_owned(),
            me: me.to_owned(),
            member,
            limits,
            state,
            skipped: Vec::new(),
            gone: HashSet::new(),
            fs,
            touched: BTreeSet::new(),
            #[cfg(test)]
            planted_unsynced: false,
        }
    }

    /// Whether T-T6.1.1's bug is planted.
    #[cfg(test)]
    fn planted(&self) -> bool {
        self.planted_unsynced
    }

    /// Whether T-T6.1.1's bug is planted: never, outside a test build.
    #[cfg(not(test))]
    fn planted(&self) -> bool {
        false
    }

    /// Makes the changes of names in the folders this copy changed durable: what the copy's state
    /// may then say the folder holds survives a power loss. A folder gone since needs nothing (the
    /// one it was in is among them); one that failed is tried again next time.
    pub fn sync_dirs(&mut self) -> io::Result<()> {
        let dirs = std::mem::take(&mut self.touched);
        let mut rest = dirs.into_iter();
        while let Some(dir) = rest.next() {
            match self.fs.sync_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    self.touched.insert(dir);
                    self.touched.extend(rest);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Notes that the names in the folder holding `path` changed (and, for a folder made just now,
    /// in each folder above it, up to the copy's root).
    fn touch(&mut self, path: &Path, made: bool) {
        let mut dir = path.parent();
        while let Some(d) = dir.filter(|d| d.starts_with(&self.root)) {
            self.touched.insert(d.to_owned());
            if !made || *d == self.root {
                break;
            }
            dir = d.parent();
        }
    }

    /// What this copy's file at `path` holds now (none: no file).
    fn now(&self, path: &str) -> io::Result<Option<Version>> {
        let full = self.root.join(path);
        let meta = match self.fs.meta(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if meta.is_symlink() {
            return Ok(Some(Version::Link { target: self.fs.read_link(&full)?.to_string_lossy().replace('\\', "/") }));
        }
        if !meta.is_file() {
            return Ok(None);
        }
        Ok(Some(Version::File { hash: blobs::hash_file_on(self.fs.as_ref(), &full)?, exec: meta.exec }))
    }

    /// Whether every folder on the way to `path` is a real folder under the root: a link among them
    /// could carry a write outside the repo.
    fn parents_real(&self, path: &str) -> bool {
        let mut at = self.root.clone();
        let parts: Vec<&str> = path.split('/').collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            at.push(part);
            match self.fs.meta(&at) {
                Ok(m) if m.is_symlink() => return false,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return true,
                Err(_) => return false,
            }
        }
        true
    }

    /// What changed here since the log last brought this copy in step (or since it last proposed):
    /// steps for the log, numbered on from the last this member sent, each of at most
    /// [`STEP_CHANGES`] changes, with the new contents put in `blobs`. Empty: nothing changed.
    pub fn propose(&mut self, blobs: &Blobs, at: i64) -> io::Result<Vec<Step>> {
        if self.adopting() {
            return Ok(Vec::new());
        }
        let (mut found, skipped) = scan::scan(self.fs.as_ref(), &self.root, &self.state.seen, &self.limits)?;
        self.skipped = skipped;
        // What the log holds of paths the folder fell behind on, brought now that nothing here
        // stands in its way; read again at the next look.
        for path in self.catch_up(&found, blobs)? {
            found.remove(&path);
        }
        let mut paths: BTreeSet<String> = found.keys().cloned().collect();
        paths.extend(self.state.synced.keys().cloned());
        paths.extend(self.state.pending.keys().cloned());
        let mut changes = Vec::new();
        for path in paths {
            let here = found.get(&path).map(|e| e.version.clone());
            let expected = match self.state.pending.get(&path) {
                Some(p) => p.clone(),
                None => self.state.synced.get(&path).cloned(),
            };
            if here == expected {
                continue;
            }
            // Not listed but still there (left out: too big, excluded, unreadable): not deleted.
            if here.is_none() && self.fs.meta(&self.root.join(&path)).is_ok() {
                continue;
            }
            if let Some(Version::File { hash, .. }) = &here
                && !blobs.has(hash)
                && blobs.put_file(&self.root.join(&path))? != *hash
            {
                // Changed again while it was read: the next look takes it.
                continue;
            }
            changes.push(Change { path, to: here, base: expected });
        }
        // The store's folders holding what the new steps carry, whoever put it there (a look
        // before a crash may have, its folders never synced): synced with this copy's before the
        // state that names them is saved, or a power loss could keep the steps and lose what they
        // carry.
        for c in &changes {
            if let Some(Version::File { hash, .. }) = &c.to {
                blobs.hold(hash);
            }
        }
        self.touched.extend(blobs.take_touched());
        self.state.seen = found;
        for c in &changes {
            self.state.pending.insert(c.path.clone(), c.to.clone());
        }
        let mut steps = Vec::new();
        let mut rest = changes.into_iter().peekable();
        while rest.peek().is_some() {
            let step = Step { by: self.me.clone(), member: self.member, seq: self.next_seq(), at, changes: rest.by_ref().take(STEP_CHANGES).collect() };
            self.state.sent.push(step.clone());
            steps.push(step);
        }
        Ok(steps)
    }

    /// Starts this copy as a folder kiln just cloned to join the group, rather than one whose files
    /// meet the others': what it holds now counts as in step, so the log's version of each file
    /// replaces it as the log brings it, and it proposes nothing until [`Copy::adopted`] (`at`: the
    /// log index it takes the group's files through). A clone of the source's commit then becomes
    /// the source's working tree, uncommitted work and all, with no conflict copies.
    pub fn adopt(&mut self, at: u64) -> io::Result<()> {
        let (found, skipped) = scan::scan(self.fs.as_ref(), &self.root, &BTreeMap::new(), &self.limits)?;
        self.skipped = skipped;
        self.state.synced = found.iter().map(|(p, e)| (p.clone(), e.version.clone())).collect();
        self.state.seen = found;
        self.state.adopt = Some(at);
        Ok(())
    }

    /// Whether this copy waits to take the group's files and proposes nothing meanwhile.
    pub fn adopting(&self) -> bool {
        self.state.adopt.is_some()
    }

    /// The log has brought this adopting copy through its index: each file it had that the log
    /// doesn't goes, unless it changed here since (then it is proposed like any change), and from
    /// now on it syncs as any copy does. A log holding no file at all deletes nothing: that is no
    /// source a clone was made from.
    pub fn adopted(&mut self, blobs: &Blobs) -> io::Result<Vec<Outcome>> {
        let mut out = Vec::new();
        if !self.state.tree.files.is_empty() {
            let extra: Vec<(String, Version)> = self.state.synced.iter().filter(|(p, _)| !self.state.tree.files.contains_key(*p)).map(|(p, v)| (p.clone(), v.clone())).collect();
            for (path, v) in extra {
                if self.parents_real(&path) && self.now(&path)? == Some(v) {
                    self.write(&path, None, blobs)?;
                    out.push(Outcome::Applied { path: path.clone() });
                }
                self.state.synced.remove(&path);
                self.state.seen.remove(&path);
            }
        }
        self.state.adopt = None;
        Ok(out)
    }

    /// Brings the log's version of each path a step's version was kept back from (changed here then)
    /// once the change here is gone (undone, or a file made here deleted) without being proposed:
    /// the folder is then as this copy last synced it, so no look would ever propose it, and the
    /// step that changed it was taken, so none would bring it either. `found` is what the look found
    /// here. The paths brought; one whose contents aren't here yet waits for them
    /// ([`Copy::behind_needs`]).
    fn catch_up(&mut self, found: &BTreeMap<String, Entry>, blobs: &Blobs) -> io::Result<Vec<String>> {
        let mut brought = Vec::new();
        let behind: Vec<String> = self.state.behind.iter().cloned().collect();
        for path in behind {
            // Proposed: its step meets the log's version, and the rule decides.
            if self.state.pending.contains_key(&path) {
                continue;
            }
            let log = self.state.tree.files.get(&path).cloned();
            let synced = self.state.synced.get(&path).cloned();
            let here = found.get(&path).map(|e| e.version.clone());
            if log == synced || here == log {
                set(&mut self.state.synced, &path, log);
                self.state.behind.remove(&path);
                continue;
            }
            // Still changed here: the look proposes it.
            if here != synced || !self.parents_real(&path) {
                continue;
            }
            if let Some(Version::File { hash, .. }) = &log
                && (!blobs.has(hash) || self.gone.contains(hash))
            {
                continue;
            }
            self.write(&path, log.as_ref(), blobs)?;
            set(&mut self.state.synced, &path, log);
            self.state.behind.remove(&path);
            brought.push(path);
        }
        Ok(brought)
    }

    /// The contents the paths the folder fell behind on wait for ([`Copy::catch_up`]).
    pub fn behind_needs(&self, blobs: &Blobs) -> Vec<String> {
        let mut out: Vec<String> = self
            .state
            .behind
            .iter()
            .filter_map(|p| match self.state.tree.files.get(p) {
                Some(Version::File { hash, .. }) if !blobs.has(hash) && !self.gone.contains(hash) => Some(hash.clone()),
                _ => None,
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The number of this member's next step: one past what it sent, or past what the log took
    /// from it (a copy whose state was started again).
    fn next_seq(&self) -> u64 {
        let taken = self.state.tree.seqs.get(&self.member).copied().unwrap_or(0);
        self.state.sent.last().map_or(taken, |s| s.seq.max(taken)).saturating_add(1)
    }

    /// The blobs applying `step` here needs: its new contents, and the log's version of any change
    /// of ours it turns down (written back under the name).
    pub fn needs(&self, step: &Step) -> Vec<String> {
        let mut out = Vec::new();
        for c in &step.changes {
            if let Some(Version::File { hash, .. }) = &c.to {
                out.push(hash.clone());
            }
            let log_now = self.state.tree.files.get(&c.path);
            if self.ours(step)
                && c.base.as_ref() != log_now
                && let Some(Version::File { hash, .. }) = log_now
            {
                out.push(hash.clone());
            }
        }
        out.retain(|h| !self.gone.contains(h));
        out.sort();
        out.dedup();
        out
    }

    /// Whether `v` is contents the server no longer has.
    fn is_gone(&self, v: Option<&Version>) -> bool {
        matches!(v, Some(Version::File { hash, .. }) if self.gone.contains(hash))
    }

    /// Whether this copy proposed `step`.
    fn ours(&self, step: &Step) -> bool {
        step.member != 0 && step.member == self.member
    }

    /// Applies a committed step, in log order (`needs` says which blobs it reads). A step taken
    /// before changes nothing. All or nothing to the copy's state: when writing its files fails
    /// part-way (a full disk, an I/O error), the state is put back as it was before it, so the step
    /// is applied again, whole, next time, its files already written found in place; taken by the
    /// tree with the rest unwritten, they'd never be written at all.
    pub fn apply(&mut self, step: &Step, blobs: &Blobs) -> io::Result<Vec<Outcome>> {
        let undo = Undo::of(&self.state, step, self.ours(step));
        let applied = self.apply_whole(step, blobs);
        if applied.is_err() {
            undo.restore(&mut self.state);
        }
        applied
    }

    fn apply_whole(&mut self, step: &Step, blobs: &Blobs) -> io::Result<Vec<Outcome>> {
        let ours = self.ours(step);
        if ours {
            // The log has it: it isn't sent again.
            self.state.sent.retain(|s| s.seq > step.seq);
        }
        let before: Vec<Option<Version>> = step.changes.iter().map(|c| self.state.tree.files.get(&c.path).cloned()).collect();
        let Some(decisions) = self.state.tree.take(step) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for ((c, log_now), decision) in step.changes.iter().zip(before).zip(decisions) {
            if ours && self.state.pending.get(&c.path) == Some(&c.to) {
                self.state.pending.remove(&c.path);
            }
            match decision {
                Decision::Refuse => out.push(Outcome::Refused { path: c.path.clone(), why: "not a path kiln writes".to_owned() }),
                // This copy made it: its file holds it, or has moved on from it.
                Decision::Take | Decision::Same if ours => {
                    set(&mut self.state.synced, &c.path, c.to.clone());
                    out.push(Outcome::Applied { path: c.path.clone() });
                }
                Decision::Take | Decision::Same => out.push(self.bring(&c.path, c.to.as_ref(), blobs)?),
                // A conflict: the log keeps what it has; the step's version goes beside it, on every copy.
                Decision::Beside(copy) => {
                    if let (Some(name), Some(v)) = (&copy, &c.to)
                        && self.parents_real(name)
                        && !self.is_gone(Some(v))
                    {
                        self.write(name, Some(v), blobs)?;
                        set(&mut self.state.synced, name, Some(v.clone()));
                    }
                    if ours {
                        if self.now(&c.path)? == c.to {
                            // Ours, unchanged since: it is beside the name now, and the name takes the log's
                            // (unless that is gone too: a later step brings the name its version).
                            if self.is_gone(log_now.as_ref()) {
                                set(&mut self.state.synced, &c.path, c.to.clone());
                            } else {
                                if self.parents_real(&c.path) {
                                    self.write(&c.path, log_now.as_ref(), blobs)?;
                                }
                                set(&mut self.state.synced, &c.path, log_now);
                            }
                        } else {
                            // Changed again since: kept, and proposed as a change of what lost, so it can't
                            // silently undo what won; undone instead, the log's version comes after all.
                            set(&mut self.state.synced, &c.path, c.to.clone());
                            self.state.behind.insert(c.path.clone());
                        }
                    }
                    out.push(Outcome::Conflict { path: c.path.clone(), copy });
                }
            }
        }
        Ok(out)
    }

    /// Brings another member's version of `path` here, unless the file changed here (its own
    /// proposal, made or to come, meets that version in the log).
    fn bring(&mut self, path: &str, to: Option<&Version>, blobs: &Blobs) -> io::Result<Outcome> {
        if !self.parents_real(path) {
            return Ok(Outcome::Refused { path: path.to_owned(), why: "a folder on its way is a link".to_owned() });
        }
        if self.is_gone(to) {
            // Replaced since: the step that replaced it comes next and brings what it holds now.
            return Ok(Outcome::Kept { path: path.to_owned() });
        }
        let here = self.now(path)?;
        if here.as_ref() == to {
            // Here already (the same file on both machines).
            set(&mut self.state.synced, path, to.cloned());
            return Ok(Outcome::Applied { path: path.to_owned() });
        }
        if self.state.pending.contains_key(path) || here != self.state.synced.get(path).cloned() {
            // Its version comes once the change here is proposed (it meets it in the log), or gone.
            self.state.behind.insert(path.to_owned());
            return Ok(Outcome::Kept { path: path.to_owned() });
        }
        self.write(path, to, blobs)?;
        set(&mut self.state.synced, path, to.cloned());
        Ok(Outcome::Applied { path: path.to_owned() })
    }

    /// Makes the folders `full` goes in: whether any was missing.
    fn make_parents(&self, full: &Path) -> io::Result<bool> {
        let Some(parent) = full.parent() else { return Ok(false) };
        let missing = self.fs.meta(parent).is_err();
        self.fs.create_dir_all(parent)?;
        Ok(missing)
    }

    /// Makes this copy's `path` hold `v` (none: removes it), atomically for a file: its bytes
    /// durable before its name, and the folders whose names changed noted to be synced before the
    /// copy's state says the folder holds it ([`Copy::sync_dirs`]).
    fn write(&mut self, path: &str, v: Option<&Version>, blobs: &Blobs) -> io::Result<()> {
        let full = self.root.join(path);
        let gone = |r: io::Result<()>| match r {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
        match v {
            None => {
                gone(self.fs.remove_file(&full))?;
                self.touch(&full, false);
                // Folders left empty go too, as git leaves none behind.
                let mut dir = full.parent().map(Path::to_path_buf);
                while let Some(d) = dir.filter(|d| *d != self.root && d.starts_with(&self.root)) {
                    if self.fs.remove_dir(&d).is_err() {
                        break;
                    }
                    self.touch(&d, false);
                    dir = d.parent().map(Path::to_path_buf);
                }
                Ok(())
            }
            Some(Version::File { hash, exec }) => {
                let src = blobs.path(hash).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a SHA-256"))?;
                let made_dirs = self.make_parents(&full)?;
                let tmp = full.with_file_name(format!("{}{}.tmp", scan::TEMP_PREFIX, self.fs.temp_id()));
                self.fs.copy(&src, &tmp)?;
                // Its bytes durable before its name: renamed in unsynced, a power loss can leave the
                // name holding nothing, or a torn page, which the next look would take for an edit
                // made here and spread to every copy.
                let planted = self.planted();
                let made = self
                    .fs
                    .set_exec(&tmp, *exec)
                    .and_then(|()| if planted { Ok(()) } else { self.fs.sync_file(&tmp) })
                    .and_then(|()| self.fs.rename(&tmp, &full));
                if made.is_err() {
                    let _ = self.fs.remove_file(&tmp);
                } else if !planted {
                    self.touch(&full, made_dirs);
                }
                made
            }
            Some(Version::Link { target }) => {
                let made_dirs = self.make_parents(&full)?;
                gone(self.fs.remove_file(&full))?;
                self.fs.symlink(target, &full)?;
                self.touch(&full, made_dirs);
                Ok(())
            }
        }
    }
}

/// What applying a step may change of a copy's state, as it was before ([`Copy::apply`]): the tree's,
/// synced and pending versions of its paths and of the names their conflict copies would take,
/// its member's last number, and, for a step of the copy's own, its unsent steps.
struct Undo {
    seq: Option<(u64, Option<u64>)>,
    files: Vec<(String, Option<Version>)>,
    synced: Vec<(String, Option<Version>)>,
    pending: Vec<(String, Option<Option<Version>>)>,
    behind: Vec<(String, bool)>,
    sent: Option<Vec<Step>>,
}

impl Undo {
    fn of(st: &State, step: &Step, ours: bool) -> Self {
        let mut paths: Vec<String> = Vec::new();
        for c in &step.changes {
            paths.push(c.path.clone());
            if c.to.is_some() {
                paths.push(conflict_name(&c.path, &step.by, step.at));
            }
        }
        paths.sort();
        paths.dedup();
        Self {
            seq: (step.seq != 0).then(|| (step.member, st.tree.seqs.get(&step.member).copied())),
            files: paths.iter().map(|p| (p.clone(), st.tree.files.get(p).cloned())).collect(),
            synced: paths.iter().map(|p| (p.clone(), st.synced.get(p).cloned())).collect(),
            pending: paths.iter().map(|p| (p.clone(), st.pending.get(p).cloned())).collect(),
            behind: paths.iter().map(|p| (p.clone(), st.behind.contains(p))).collect(),
            sent: ours.then(|| st.sent.clone()),
        }
    }

    fn restore(self, st: &mut State) {
        if let Some((member, seq)) = self.seq {
            match seq {
                Some(s) => {
                    st.tree.seqs.insert(member, s);
                }
                None => {
                    st.tree.seqs.remove(&member);
                }
            }
        }
        for (p, v) in self.files {
            set(&mut st.tree.files, &p, v);
        }
        for (p, v) in self.synced {
            set(&mut st.synced, &p, v);
        }
        for (p, v) in self.pending {
            match v {
                Some(v) => {
                    st.pending.insert(p, v);
                }
                None => {
                    st.pending.remove(&p);
                }
            }
        }
        for (p, was) in self.behind {
            if was {
                st.behind.insert(p);
            } else {
                st.behind.remove(&p);
            }
        }
        if let Some(sent) = self.sent {
            st.sent = sent;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two copies of one repo and a log both apply in order; blobs fetched means one shared store.
    struct Two {
        _dir: tempfile::TempDir,
        a: Copy,
        b: Copy,
        blobs: Blobs,
        log: Vec<Step>,
        applied: (usize, usize),
    }

    impl Two {
        fn new() -> anyhow::Result<Self> {
            let dir = tempfile::tempdir()?;
            let (ra, rb) = (dir.path().join("a"), dir.path().join("b"));
            std::fs::create_dir_all(&ra)?;
            std::fs::create_dir_all(&rb)?;
            let blobs = Blobs::new(&dir.path().join("blobs"))?;
            let a = Copy::new(&ra, "a", 2, Limits::default(), State::default());
            let b = Copy::new(&rb, "b", 3, Limits::default(), State::default());
            Ok(Self { _dir: dir, a, b, blobs, log: Vec::new(), applied: (0, 0) })
        }
        fn write(c: &Copy, path: &str, body: &str) -> io::Result<()> {
            let p = c.root.join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(p, body)
        }
        fn read(c: &Copy, path: &str) -> Option<String> {
            std::fs::read_to_string(c.root.join(path)).ok()
        }
        /// `who` proposes what changed on it; the log takes it.
        fn propose(&mut self, who: char, at: i64) -> anyhow::Result<()> {
            let c = if who == 'a' { &mut self.a } else { &mut self.b };
            self.log.extend(c.propose(&self.blobs, at)?);
            Ok(())
        }
        /// Both copies apply what they haven't.
        fn sync(&mut self) -> anyhow::Result<Vec<Outcome>> {
            let mut all = Vec::new();
            for step in self.log.get(self.applied.0..).unwrap_or_default() {
                all.extend(self.a.apply(step, &self.blobs)?);
            }
            for step in self.log.get(self.applied.1..).unwrap_or_default() {
                all.extend(self.b.apply(step, &self.blobs)?);
            }
            self.applied = (self.log.len(), self.log.len());
            Ok(all)
        }
        /// Every synced file of a copy, path → contents.
        fn files(c: &Copy) -> anyhow::Result<BTreeMap<String, String>> {
            let mut out = BTreeMap::new();
            for p in scan::list(&fs::Std, &c.root, &Limits::default())? {
                out.insert(p.clone(), Self::read(c, &p).unwrap_or_default());
            }
            Ok(out)
        }
    }

    /// CP-T6.1, AC-T6.1.3 (the first bug's fix): a file sync writes is made durable through a
    /// handle to read: a blob a strict umask made read-only still goes into the other copy (its
    /// bytes synced before its name).
    #[cfg(unix)]
    #[test]
    fn a_read_only_blob_still_goes_into_the_folder() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let mut t = Two::new()?;
        Two::write(&t.a, "notes.txt", "kept read-only")?;
        t.propose('a', 1_000)?;
        for step in &t.log {
            for c in &step.changes {
                if let Some(Version::File { hash, .. }) = &c.to
                    && let Some(blob) = t.blobs.path(hash)
                {
                    std::fs::set_permissions(blob, std::fs::Permissions::from_mode(0o400))?;
                }
            }
        }
        t.sync()?;
        assert_eq!(Two::read(&t.b, "notes.txt").as_deref(), Some("kept read-only"));
        Ok(())
    }

    #[test]
    fn a_change_on_either_machine_reaches_the_other() -> anyhow::Result<()> {
        let mut t = Two::new()?;
        Two::write(&t.a, "src/x.txt", "one")?;
        t.propose('a', 1_000)?;
        t.sync()?;
        assert_eq!(Two::read(&t.b, "src/x.txt").as_deref(), Some("one"));
        Two::write(&t.b, "src/x.txt", "two")?;
        t.propose('b', 2_000)?;
        t.sync()?;
        assert_eq!(Two::read(&t.a, "src/x.txt").as_deref(), Some("two"));
        std::fs::remove_file(t.a.root.join("src/x.txt"))?;
        t.propose('a', 3_000)?;
        t.sync()?;
        assert_eq!(Two::read(&t.b, "src/x.txt"), None, "deleted everywhere");
        assert!(!t.b.root.join("src").exists(), "and the folder it left empty");
        assert!(t.b.root.exists(), "never the copy's own folder");
        assert!(t.a.state.pending.is_empty() && t.b.state.pending.is_empty());
        Ok(())
    }

    #[test]
    fn both_changing_a_file_loses_neither_and_converges() -> anyhow::Result<()> {
        let mut t = Two::new()?;
        Two::write(&t.a, "x.txt", "one")?;
        t.propose('a', 1_000)?;
        t.sync()?;
        // Both edit it, from the same version; a's step commits first.
        Two::write(&t.a, "x.txt", "from a")?;
        Two::write(&t.b, "x.txt", "from b")?;
        t.propose('a', 2_000)?;
        t.propose('b', 2_500)?;
        t.sync()?;
        let copy = conflict_name("x.txt", "b", 2_500);
        assert_eq!(copy, "x.conflict-b-19700101-000002.txt");
        for c in [&t.a, &t.b] {
            assert_eq!(Two::read(c, "x.txt").as_deref(), Some("from a"), "the first to commit keeps the name");
            assert_eq!(Two::read(c, &copy).as_deref(), Some("from b"), "the other is beside it");
        }
        assert_eq!(Two::files(&t.a)?, Two::files(&t.b)?, "the copies are the same");
        // Nothing left to propose on either.
        t.propose('a', 3_000)?;
        t.propose('b', 3_000)?;
        assert_eq!(t.log.len(), 3);
        Ok(())
    }

    #[test]
    fn a_delete_meeting_an_edit_keeps_the_edit_beside() -> anyhow::Result<()> {
        let mut t = Two::new()?;
        Two::write(&t.a, "y.md", "v1")?;
        t.propose('a', 1_000)?;
        t.sync()?;
        std::fs::remove_file(t.a.root.join("y.md"))?;
        Two::write(&t.b, "y.md", "edited on b")?;
        t.propose('a', 2_000)?;
        t.propose('b', 2_100)?;
        t.sync()?;
        let copy = conflict_name("y.md", "b", 2_100);
        for c in [&t.a, &t.b] {
            assert_eq!(Two::read(c, "y.md"), None, "the delete committed first");
            assert_eq!(Two::read(c, &copy).as_deref(), Some("edited on b"));
        }
        Ok(())
    }

    #[test]
    fn a_change_not_yet_proposed_is_never_overwritten() -> anyhow::Result<()> {
        let mut t = Two::new()?;
        Two::write(&t.a, "z.rs", "one")?;
        t.propose('a', 1_000)?;
        t.sync()?;
        // a edits but hasn't proposed when b's change arrives.
        Two::write(&t.a, "z.rs", "a, unsent")?;
        Two::write(&t.b, "z.rs", "from b")?;
        t.propose('b', 2_000)?;
        let outcomes = t.sync()?;
        assert!(outcomes.contains(&Outcome::Kept { path: "z.rs".into() }));
        assert_eq!(Two::read(&t.a, "z.rs").as_deref(), Some("a, unsent"), "kept");
        t.propose('a', 3_000)?;
        t.sync()?;
        let copy = conflict_name("z.rs", "a", 3_000);
        for c in [&t.a, &t.b] {
            assert_eq!(Two::read(c, "z.rs").as_deref(), Some("from b"));
            assert_eq!(Two::read(c, &copy).as_deref(), Some("a, unsent"));
        }
        Ok(())
    }

    #[test]
    fn a_step_cannot_write_outside_the_repo() -> anyhow::Result<()> {
        let mut t = Two::new()?;
        let outside = t.a.root.parent().map(|p| p.join("outside")).unwrap_or_default();
        std::fs::create_dir_all(&outside)?;
        let hash = t.blobs.put_bytes(b"pwned")?;
        let file = |path: &str| Change { path: path.into(), to: Some(Version::File { hash: hash.clone(), exec: false }), base: None };
        let mut evil = vec![file("../outside/x"), file("/etc/x"), file(".git/config"), file("a/./b"), file(".kiln-sync-1.tmp")];
        // A link to outside, then a write through it.
        #[cfg(unix)]
        evil.push(Change { path: "door".into(), to: Some(Version::Link { target: outside.to_string_lossy().into_owned() }), base: None });
        evil.push(file("door/x"));
        let out = t.a.apply(&Step { by: "b".into(), member: 3, seq: 0, at: 1, changes: evil }, &t.blobs)?;
        let refused = out.iter().filter(|o| matches!(o, Outcome::Refused { .. })).count();
        assert!(refused >= 6, "{out:?}");
        assert!(std::fs::read_dir(&outside)?.next().is_none(), "nothing written outside");
        assert!(!t.a.root.join(".git").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn the_exec_bit_and_links_travel() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let mut t = Two::new()?;
        Two::write(&t.a, "run.sh", "#!/bin/sh\n")?;
        std::fs::set_permissions(t.a.root.join("run.sh"), std::fs::Permissions::from_mode(0o755))?;
        std::os::unix::fs::symlink("run.sh", t.a.root.join("go"))?;
        t.propose('a', 1_000)?;
        t.sync()?;
        let mode = std::fs::metadata(t.b.root.join("run.sh"))?.permissions().mode();
        assert!(mode & 0o111 != 0, "{mode:o}");
        assert_eq!(std::fs::read_link(t.b.root.join("go"))?, PathBuf::from("run.sh"));
        Ok(())
    }

    #[test]
    fn a_fresh_clone_takes_the_source_s_working_tree_without_conflict_copies() -> anyhow::Result<()> {
        let mut t = Two::new()?;
        // The commit both have; the source has since edited one file, added one and deleted one,
        // none of it committed.
        for c in [&t.a, &t.b] {
            for (p, body) in [("a.txt", "committed"), ("gone.txt", "committed"), ("same.txt", "same")] {
                Two::write(c, p, body)?;
            }
        }
        Two::write(&t.a, "a.txt", "an edit not committed")?;
        Two::write(&t.a, "src/new.txt", "untracked")?;
        std::fs::remove_file(t.a.root.join("gone.txt"))?;
        t.propose('a', 1_000)?;
        // The clone joins, to take the group's files through the log as it is now.
        t.b.adopt(u64::try_from(t.log.len())?)?;
        assert!(t.b.adopting());
        t.propose('b', 1_500)?;
        assert_eq!(t.log.len(), 1, "an adopting copy proposes nothing");
        let outcomes = t.sync()?;
        assert!(!outcomes.iter().any(|o| matches!(o, Outcome::Conflict { .. } | Outcome::Kept { .. })), "{outcomes:?}");
        t.b.adopted(&t.blobs)?;
        assert!(!t.b.adopting());
        assert_eq!(Two::files(&t.a)?, Two::files(&t.b)?, "the clone is the source's working tree");
        assert_eq!(Two::read(&t.b, "a.txt").as_deref(), Some("an edit not committed"));
        assert_eq!(Two::read(&t.b, "gone.txt"), None, "what the source deleted goes from the clone");
        assert!(group::conflicts(&t.b.state.tree).is_empty());
        // In step: neither has anything to say, and a change on the clone reaches the source.
        t.propose('a', 2_000)?;
        t.propose('b', 2_000)?;
        assert_eq!(t.log.len(), 1);
        Two::write(&t.b, "a.txt", "edited on the clone")?;
        t.propose('b', 3_000)?;
        t.sync()?;
        assert_eq!(Two::read(&t.a, "a.txt").as_deref(), Some("edited on the clone"));
        Ok(())
    }

    #[test]
    fn an_adopting_copy_keeps_what_changed_on_it_meanwhile() -> anyhow::Result<()> {
        let mut t = Two::new()?;
        for c in [&t.a, &t.b] {
            Two::write(c, "old.txt", "committed")?;
        }
        std::fs::remove_file(t.a.root.join("old.txt"))?;
        Two::write(&t.a, "kept.txt", "source")?;
        t.propose('a', 1_000)?;
        t.b.adopt(1)?;
        // Someone edits the clone's old.txt before it has caught up: not the clone's file any more.
        Two::write(&t.b, "old.txt", "edited while adopting")?;
        t.sync()?;
        t.b.adopted(&t.blobs)?;
        assert_eq!(Two::read(&t.b, "old.txt").as_deref(), Some("edited while adopting"), "an edit is never deleted");
        t.propose('b', 2_000)?;
        t.sync()?;
        assert_eq!(Two::read(&t.a, "old.txt").as_deref(), Some("edited while adopting"), "it is proposed like any change");
        Ok(())
    }

    #[test]
    fn a_copy_s_state_round_trips() -> anyhow::Result<()> {
        let mut t = Two::new()?;
        Two::write(&t.a, "f", "x")?;
        t.propose('a', 1)?;
        t.sync()?;
        let json = serde_json::to_string(&t.a.state)?;
        assert_eq!(serde_json::from_str::<State>(&json)?, t.a.state);
        Ok(())
    }
}
