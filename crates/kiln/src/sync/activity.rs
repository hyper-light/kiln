//! What happened in each synced repo, newest last, as the Repositories page shows it (docs/sync.md,
//! "Watching it"): each step its log committed (the machine that made it, its files and what the
//! log did with each) and what else happened to it: a copy added, going offline and coming back,
//! errors, steps let go, contents asked for again, conflicts resolved, agent sessions moving with
//! its files. The server's owner of the groups writes it; the API reads it. The newest [`KEEP`] of
//! each repo are kept, saved beside its tree, so a restart doesn't blank it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Decision, Step, Version};

/// Events kept for each repo.
pub const KEEP: usize = 300;
/// Files a step's event lists by name; the rest it counts.
pub const FILES: usize = 40;

/// Something that happened in a synced repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Numbered in the order the server saw them, across repos: a reader asks for those after the
    /// last it has.
    pub seq: u64,
    /// The synced repo's id.
    pub repo: String,
    /// Unix milliseconds, the server's clock.
    pub at: i64,
    pub kind: Kind,
    /// The machine it's about: a step's maker (`kiln` for the server's own, a resolution).
    pub machine: String,
    /// What happened, in words. A step's files say what it did.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    pub tone: Tone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<StepEvent>,
    /// A copy's change of state: the state it went into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The log committed a step.
    Step,
    /// A copy was added or removed, or took the repo's files.
    Copy,
    /// A copy went offline or came back, or stopped or recovered from an error.
    State,
    /// The repo's group stopped.
    Error,
    /// The server let a step go.
    Dropped,
    /// Something was asked for or sent again.
    Retry,
    /// A conflict was resolved.
    Resolved,
    /// An agent session moved between machines with the repo's files.
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Info,
    Ok,
    Warn,
    Fail,
}

/// A committed step, as its event shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StepEvent {
    /// Its entry in the repo's log: a copy whose folder has taken the log this far has it.
    pub index: u64,
    /// When its machine looked and saw the changes (unix milliseconds, that machine's clock).
    pub seen: i64,
    /// From its arrival at the server to its commit, in milliseconds (none: the server didn't see
    /// it arrive, as after a restart).
    pub commit_ms: Option<u64>,
    /// The first [`FILES`] of its changes.
    pub files: Vec<FileEvent>,
    /// Its changes, listed or not: by kind, those that met another machine's, those refused.
    pub added: usize,
    pub changed: usize,
    pub deleted: usize,
    pub conflicts: usize,
    pub refused: usize,
    /// Bytes of the contents it brings, as far as the server knows them.
    pub bytes: u64,
}

/// One of a step's changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEvent {
    pub path: String,
    pub kind: Change,
    pub outcome: Outcome,
    /// The new contents' bytes, when the server has them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// A conflict: the name its version went under beside the path (none: a delete, let go).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beside: Option<String>,
    /// It is a symbolic link.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub link: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Added,
    Changed,
    Deleted,
}

impl Change {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Changed => "changed",
            Self::Deleted => "deleted",
        }
    }
}

/// What the log did with a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The path holds it now.
    Taken,
    /// The path held it already.
    Same,
    /// Another machine's change got there first: it went beside the path.
    Conflict,
    /// Not a path kiln writes.
    Refused,
}

/// What a committed step did, as its event shows it: `size` gives a content's bytes, when known.
pub fn step_event(index: u64, step: &Step, decisions: &[Decision], commit_ms: Option<u64>, size: impl Fn(&str) -> Option<u64>) -> StepEvent {
    let mut e = StepEvent { index, seen: step.at, commit_ms, ..StepEvent::default() };
    for (c, d) in step.changes.iter().zip(decisions) {
        let kind = match (&c.base, &c.to) {
            (_, None) => Change::Deleted,
            (None, Some(_)) => Change::Added,
            (Some(_), Some(_)) => Change::Changed,
        };
        match kind {
            Change::Added => e.added += 1,
            Change::Changed => e.changed += 1,
            Change::Deleted => e.deleted += 1,
        }
        let (outcome, beside) = match d {
            Decision::Take => (Outcome::Taken, None),
            Decision::Same => (Outcome::Same, None),
            Decision::Beside(name) => (Outcome::Conflict, name.clone()),
            Decision::Refuse => (Outcome::Refused, None),
        };
        match outcome {
            Outcome::Conflict => e.conflicts += 1,
            Outcome::Refused => e.refused += 1,
            Outcome::Taken | Outcome::Same => {}
        }
        let bytes = match &c.to {
            Some(Version::File { hash, .. }) => size(hash),
            _ => None,
        };
        if outcome != Outcome::Same {
            e.bytes = e.bytes.saturating_add(bytes.unwrap_or(0));
        }
        if e.files.len() < FILES {
            e.files.push(FileEvent { path: c.path.clone(), kind, outcome, size: bytes, beside, link: matches!(c.to, Some(Version::Link { .. })) });
        }
    }
    e
}

/// Every synced repo's events.
#[derive(Debug, Default)]
pub struct Activity {
    repos: HashMap<String, VecDeque<Event>>,
    next: u64,
    /// Repos with events not saved yet.
    dirty: HashSet<String>,
}

fn file(dir: &Path, repo: &str) -> PathBuf {
    let safe: String = repo.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    dir.join(format!("{safe}.activity.json"))
}

impl Activity {
    /// The events saved under `dir` for the repos `ids`.
    pub fn load<'a>(dir: &Path, ids: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = Self::default();
        for id in ids {
            let Ok(bytes) = std::fs::read(file(dir, id)) else { continue };
            let Ok(events) = serde_json::from_slice::<VecDeque<Event>>(&bytes) else { continue };
            out.next = out.next.max(events.iter().map(|e| e.seq.saturating_add(1)).max().unwrap_or(0));
            out.repos.insert(id.to_owned(), events);
        }
        out
    }

    /// Keeps `e`, numbered after the last: the oldest of its repo's go past [`KEEP`].
    pub fn push(&mut self, mut e: Event) -> u64 {
        e.seq = self.next;
        self.next = self.next.saturating_add(1);
        let seq = e.seq;
        self.dirty.insert(e.repo.clone());
        let ring = self.repos.entry(e.repo.clone()).or_default();
        ring.push_back(e);
        while ring.len() > KEEP {
            ring.pop_front();
        }
        seq
    }

    /// Saves the repos with new events.
    pub fn save(&mut self, dir: &Path) -> io::Result<()> {
        for repo in std::mem::take(&mut self.dirty) {
            let Some(ring) = self.repos.get(&repo) else { continue };
            let bytes = serde_json::to_vec(ring).map_err(io::Error::other)?;
            if let Err(e) = super::group::save_atomic(&file(dir, &repo), &bytes) {
                self.dirty.insert(repo);
                return Err(e);
            }
        }
        Ok(())
    }

    /// A repo no longer synced: its events go, saved or not.
    pub fn forget(&mut self, dir: &Path, repo: &str) {
        self.repos.remove(repo);
        self.dirty.remove(repo);
        let _ = std::fs::remove_file(file(dir, repo));
    }

    /// Events numbered `since` on (all: 0), of `repo` alone if given: the newest `limit` of them,
    /// newest first.
    pub fn since(&self, since: u64, repo: Option<&str>, limit: usize) -> Vec<Event> {
        let mut out: Vec<&Event> = self.repos.iter().filter(|(id, _)| repo.is_none_or(|r| r == id.as_str())).flat_map(|(_, ring)| ring.iter().rev().take_while(|e| e.seq >= since)).collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.seq));
        out.into_iter().take(limit).cloned().collect()
    }

    /// The number the next event gets: a reader that has everything asks from here.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// The log index of the newest step each repo's record holds.
    pub fn recorded(&self) -> HashMap<String, u64> {
        self.repos.iter().map(|(id, ring)| (id.clone(), ring.iter().filter_map(|e| e.step.as_ref().map(|s| s.index)).max().unwrap_or(0))).collect()
    }

    /// The copies, by repo and machine, whose last change of state on record is going offline:
    /// their coming back is news after a restart too.
    pub fn away(&self) -> HashSet<(String, String)> {
        let mut out = HashSet::new();
        for (repo, ring) in &self.repos {
            for e in ring.iter().filter(|e| e.kind == Kind::State) {
                let key = (repo.clone(), e.machine.clone());
                match e.state.as_deref() {
                    Some("offline") => {
                        out.insert(key);
                    }
                    Some(_) => {
                        out.remove(&key);
                    }
                    None => {}
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::Change as StepChange;
    use super::*;

    fn file_v(hash: &str) -> Option<Version> {
        Some(Version::File { hash: hash.into(), exec: false })
    }

    fn event(repo: &str, at: i64) -> Event {
        Event { seq: 0, repo: repo.into(), at, kind: Kind::State, machine: "mac".into(), text: "went offline".into(), tone: Tone::Warn, step: None, state: Some("offline".into()) }
    }

    #[test]
    fn a_step_says_what_each_change_did() {
        let step = Step {
            by: "mac".into(),
            member: 2,
            seq: 7,
            at: 1_000,
            changes: vec![
                StepChange { path: "new.rs".into(), to: file_v("a"), base: None, bundle: None },
                StepChange { path: "old.rs".into(), to: file_v("b"), base: file_v("c"), bundle: None },
                StepChange { path: "gone.rs".into(), to: None, base: file_v("d"), bundle: None },
                StepChange { path: "link".into(), to: Some(Version::Link { target: "x".into() }), base: None, bundle: None },
            ],
        };
        let decisions = vec![Decision::Take, Decision::Beside(Some("old.conflict-mac-20261019-120000.rs".into())), Decision::Same, Decision::Refuse];
        let e = step_event(42, &step, &decisions, Some(3), |h| (h == "a").then_some(10).or((h == "b").then_some(5)));
        assert_eq!((e.index, e.seen, e.commit_ms), (42, 1_000, Some(3)));
        assert_eq!((e.added, e.changed, e.deleted, e.conflicts, e.refused), (2, 1, 1, 1, 1));
        assert_eq!(e.bytes, 15, "the new file's and the conflict's contents");
        let kinds: Vec<(Change, Outcome)> = e.files.iter().map(|f| (f.kind, f.outcome)).collect();
        assert_eq!(kinds, vec![(Change::Added, Outcome::Taken), (Change::Changed, Outcome::Conflict), (Change::Deleted, Outcome::Same), (Change::Added, Outcome::Refused)]);
        assert_eq!(e.files.get(1).and_then(|f| f.beside.as_deref()), Some("old.conflict-mac-20261019-120000.rs"));
        assert!(e.files.get(3).is_some_and(|f| f.link));
    }

    #[test]
    fn a_big_step_lists_some_files_and_counts_them_all() {
        let changes: Vec<StepChange> = (0..FILES + 5).map(|i| StepChange { path: format!("f{i}"), to: file_v("h"), base: None, bundle: None }).collect();
        let decisions = vec![Decision::Take; changes.len()];
        let e = step_event(1, &Step { by: "mac".into(), member: 2, seq: 1, at: 0, changes }, &decisions, None, |_| Some(1));
        assert_eq!(e.files.len(), FILES);
        assert_eq!(e.added, FILES + 5);
        assert_eq!(e.bytes, u64::try_from(FILES + 5).unwrap_or(0));
    }

    #[test]
    fn the_newest_are_kept_saved_and_read_back() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut a = Activity::default();
        for i in 0..KEEP + 10 {
            a.push(event("r1", i64::try_from(i).unwrap_or(0)));
        }
        a.push(event("r2", 0));
        assert_eq!(a.since(0, Some("r1"), usize::MAX).len(), KEEP, "the oldest went");
        let newest = a.since(0, None, 3);
        assert_eq!(newest.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![KEEP as u64 + 10, KEEP as u64 + 9, KEEP as u64 + 8], "newest first, across repos");
        assert_eq!(a.since(KEEP as u64 + 9, None, 10).len(), 2, "after a point");
        a.save(dir.path())?;
        let b = Activity::load(dir.path(), ["r1", "r2", "r3"]);
        assert_eq!(b.since(0, None, usize::MAX), a.since(0, None, usize::MAX));
        assert_eq!(b.next(), a.next(), "numbering goes on after a restart");
        assert_eq!(b.recorded().get("r1"), Some(&0), "no step on record");
        let away = b.away();
        assert!(away.contains(&("r1".to_owned(), "mac".to_owned())), "mac's copies went offline, last it said");
        let mut c = Activity::default();
        c.push(event("r1", 0));
        c.push(Event { state: Some("catching up".into()), text: "mac is back".into(), ..event("r1", 1) });
        assert!(c.away().is_empty(), "and came back");
        let mut b = b;
        b.forget(dir.path(), "r1");
        assert!(Activity::load(dir.path(), ["r1"]).since(0, None, 10).is_empty(), "a repo no longer synced leaves nothing");
        Ok(())
    }
}
