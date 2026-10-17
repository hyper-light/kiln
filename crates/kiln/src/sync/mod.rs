//! Repo sync (`docs/sync.md`): one repository kept on several machines. This is the engine one
//! copy runs: noticing what changed here (a step to propose), and applying committed steps in log
//! order under the rule that makes every copy converge on the same files without losing a change.
//! Ordering the steps (the repo's Raft group) and moving blobs between machines are elsewhere.
//!
//! The rule: every copy applies the same steps in the same order. A change made from the version
//! the log has is applied; one made from an older version (another machine's change committed
//! first) is a conflict, and its version goes beside the path, under a name made of the machine and
//! the step's time, on every copy alike. A copy never overwrites a file changed on it that it hasn't
//! proposed yet: that change is proposed from the version it was made from, and meets the rule.

pub mod blobs;
pub mod scan;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use blobs::Blobs;
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

/// One entry of a repo's log: the changes one machine saw together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub by: String,
    /// Unix milliseconds.
    pub at: i64,
    pub changes: Vec<Change>,
}

/// A copy's state, kept between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The repo as the log has made it so far: each path's version.
    pub log: BTreeMap<String, Version>,
    /// What this copy's files held when they were last in step with the log: a file that differs
    /// was changed here.
    pub synced: BTreeMap<String, Version>,
    /// What this copy proposed and the log hasn't brought back: each path's version (none: deleted).
    pub pending: BTreeMap<String, Option<Version>>,
    /// The last scan, so unchanged files aren't read again.
    pub seen: BTreeMap<String, Entry>,
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
    pub limits: Limits,
    pub state: State,
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
    pub fn new(root: &Path, me: &str, limits: Limits, state: State) -> Self {
        Self { root: root.to_owned(), me: me.to_owned(), limits, state }
    }

    /// What this copy's file at `path` holds now (none: no file).
    fn now(&self, path: &str) -> io::Result<Option<Version>> {
        let full = self.root.join(path);
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if meta.file_type().is_symlink() {
            return Ok(Some(Version::Link { target: std::fs::read_link(&full)?.to_string_lossy().replace('\\', "/") }));
        }
        if !meta.is_file() {
            return Ok(None);
        }
        Ok(Some(Version::File { hash: blobs::hash_file(&full)?, exec: scan::executable(&meta) }))
    }

    /// Whether every folder on the way to `path` is a real folder under the root: a link among them
    /// could carry a write outside the repo.
    fn parents_real(&self, path: &str) -> bool {
        let mut at = self.root.clone();
        let parts: Vec<&str> = path.split('/').collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            at.push(part);
            match std::fs::symlink_metadata(&at) {
                Ok(m) if m.file_type().is_symlink() => return false,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return true,
                Err(_) => return false,
            }
        }
        true
    }

    /// What changed here since the log last brought this copy in step (or since it last proposed):
    /// a step for the log, with the new contents put in `blobs`. None: nothing changed.
    pub fn propose(&mut self, blobs: &Blobs, at: i64) -> io::Result<Option<Step>> {
        let (found, _skipped) = scan::scan(&self.root, &self.state.seen, &self.limits)?;
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
            if here.is_none() && std::fs::symlink_metadata(self.root.join(&path)).is_ok() {
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
        self.state.seen = found;
        if changes.is_empty() {
            return Ok(None);
        }
        for c in &changes {
            self.state.pending.insert(c.path.clone(), c.to.clone());
        }
        Ok(Some(Step { by: self.me.clone(), at, changes }))
    }

    /// The blobs applying `step` here needs: its new contents, and the log's version of any change
    /// of ours it turns down (written back under the name).
    pub fn needs(&self, step: &Step) -> Vec<String> {
        let mut out = Vec::new();
        for c in &step.changes {
            if let Some(Version::File { hash, .. }) = &c.to {
                out.push(hash.clone());
            }
            let log_now = self.state.log.get(&c.path);
            if step.by == self.me
                && c.base.as_ref() != log_now
                && let Some(Version::File { hash, .. }) = log_now
            {
                out.push(hash.clone());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Applies a committed step, in log order (`needs` says which blobs it reads).
    pub fn apply(&mut self, step: &Step, blobs: &Blobs) -> io::Result<Vec<Outcome>> {
        let ours = step.by == self.me;
        let mut out = Vec::new();
        for c in &step.changes {
            if !safe_path(&c.path) || !self.parents_real(&c.path) {
                out.push(Outcome::Refused { path: c.path.clone(), why: "not a path kiln writes".to_owned() });
                continue;
            }
            let log_now = self.state.log.get(&c.path).cloned();
            let latest_ours = ours && self.state.pending.get(&c.path) == Some(&c.to);
            if latest_ours {
                self.state.pending.remove(&c.path);
            }
            if c.base == log_now {
                set(&mut self.state.log, &c.path, c.to.clone());
                if ours {
                    // This copy made it: its file holds it, or has moved on from it.
                    set(&mut self.state.synced, &c.path, c.to.clone());
                    out.push(Outcome::Applied { path: c.path.clone() });
                } else if self.state.pending.contains_key(&c.path) || self.now(&c.path)? != self.state.synced.get(&c.path).cloned() {
                    // Changed here: its proposal (made or to come) meets this version in the log.
                    out.push(Outcome::Kept { path: c.path.clone() });
                } else {
                    self.write(&c.path, c.to.as_ref(), blobs)?;
                    set(&mut self.state.synced, &c.path, c.to.clone());
                    out.push(Outcome::Applied { path: c.path.clone() });
                }
                continue;
            }
            // A conflict: the log keeps what it has; the step's version goes beside it, on every copy.
            let copy = c.to.as_ref().map(|_| conflict_name(&c.path, &step.by, step.at));
            if let (Some(name), Some(v)) = (&copy, &c.to) {
                set(&mut self.state.log, name, Some(v.clone()));
                if safe_path(name) && self.parents_real(name) {
                    self.write(name, Some(v), blobs)?;
                    set(&mut self.state.synced, name, Some(v.clone()));
                }
            }
            if ours {
                if self.now(&c.path)? == c.to {
                    // Ours, unchanged since: it is beside the name now, and the name takes the log's.
                    self.write(&c.path, log_now.as_ref(), blobs)?;
                    set(&mut self.state.synced, &c.path, log_now);
                } else {
                    // Changed again since: kept, and proposed as a change of what lost, so it can't
                    // silently undo what won.
                    set(&mut self.state.synced, &c.path, c.to.clone());
                }
            }
            out.push(Outcome::Conflict { path: c.path.clone(), copy });
        }
        Ok(out)
    }

    /// Makes this copy's `path` hold `v` (none: removes it), atomically for a file.
    fn write(&self, path: &str, v: Option<&Version>, blobs: &Blobs) -> io::Result<()> {
        let full = self.root.join(path);
        let gone = |r: io::Result<()>| match r {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
        match v {
            None => gone(std::fs::remove_file(&full)),
            Some(Version::File { hash, exec }) => {
                let src = blobs.path(hash).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a SHA-256"))?;
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp = full.with_file_name(format!("{}{}.tmp", scan::TEMP_PREFIX, crate::util::new_id()));
                std::fs::copy(&src, &tmp)?;
                let made = set_exec(&tmp, *exec).and_then(|()| std::fs::rename(&tmp, &full));
                if made.is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
                made
            }
            Some(Version::Link { target }) => {
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                gone(std::fs::remove_file(&full))?;
                link(target, &full)
            }
        }
    }
}

#[cfg(unix)]
fn set_exec(path: &Path, exec: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut p = std::fs::metadata(path)?.permissions();
    let mode = p.mode();
    p.set_mode(if exec { mode | 0o111 } else { mode & !0o111 });
    std::fs::set_permissions(path, p)
}

#[cfg(not(unix))]
fn set_exec(_path: &Path, _exec: bool) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn link(target: &str, at: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(not(unix))]
fn link(_target: &str, _at: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "links aren't synced on this system yet"))
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
            let a = Copy::new(&ra, "a", Limits::default(), State::default());
            let b = Copy::new(&rb, "b", Limits::default(), State::default());
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
            if let Some(step) = c.propose(&self.blobs, at)? {
                self.log.push(step);
            }
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
            for p in scan::list(&c.root, &Limits::default())? {
                out.insert(p.clone(), Self::read(c, &p).unwrap_or_default());
            }
            Ok(out)
        }
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
        let out = t.a.apply(&Step { by: "b".into(), at: 1, changes: evil }, &t.blobs)?;
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
