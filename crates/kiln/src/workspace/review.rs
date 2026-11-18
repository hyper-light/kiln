//! An agent's work in a workspace of its own with no mount, as git commits it (kiln goal
//! 01a1ec34599974448f6b7663ddb07001; Slates' A-287): an isolated agent's microVM is served its Slates volume
//! (`crate::sandbox::served`), so no working tree on this machine holds what it changed. Its review takes the volume's
//! changes from the full entry list of a landing of a snapshot of the volume over the folder it lies over
//! (`Instance::approval_entries`, presented never writing whatever the volume's mode), each file's bytes read at the
//! volume's head and checked against the BLAKE3 its entry names, and leaves out what git ignores among what it doesn't
//! track ([`super::ignored`], over the volume's own `.gitignore` files), so the branch holds exactly what a commit of
//! the volume's own working tree would (`git add -A`), as a mounted workspace's commit does.
//!
//! A volume reads only at its head: a file the agent wrote after the snapshot reads other bytes than its entry names,
//! and the review is made again from a new snapshot, at most [`TRIES`] times.

use std::collections::HashSet;
use std::path::Path;

use slates_api::{Error, Filter, Instance, LandOutcome, LandingConflict, LandingRequest, Missing, PlannedAction, PlannedEntry, PolicyRefusal, ReadAt, SnapshotId, VolumeId};

use super::git::Change;
use super::ignored::Ignores;
use super::slates::Made;

/// How many snapshots a review is made from before it says the agent is still writing: each one after the first
/// follows a file read whose bytes moved since the snapshot before it, which an agent at rest never causes. Three
/// covers a write landing between a snapshot and its reads twice running; a busy agent is asked to finish first.
const TRIES: u32 = 3;

/// Why a review couldn't be made from a volume's changes.
#[derive(Debug)]
pub enum Fault {
    /// Slates refused a call, typed.
    Slates(Error),
    /// The folder changed under paths the agent changed too: each conflict, as Slates found it.
    Conflicted(Vec<LandingConflict>),
    /// A write policy refused presenting the landing.
    Refused(PolicyRefusal),
    /// A file kept changing between a snapshot and its read [`TRIES`] times: the agent is still writing.
    Moving { path: String },
    /// A file Slates writes with no bytes named, or a rule or a read that failed.
    Other(String),
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::Slates(e) => write!(f, "Slates: {e}"),
            Fault::Conflicted(c) => write!(f, "the folder changed under {} path{} the agent changed too: take the folder's side or the agent's before a review", c.len(), if c.len() == 1 { "" } else { "s" }),
            Fault::Refused(r) => write!(f, "a write policy refused presenting its landing: {r:?}"),
            Fault::Moving { path } => write!(f, "{path} changed while its review was read, {TRIES} times: the agent is still writing; ask again once it's done"),
            Fault::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Fault {}

/// The changes workspace `m` holds over `over`, the repository's top, as git would commit them in the volume's own
/// working tree (`git add -A`): files written (each read at the volume's head and checked against its entry), links,
/// removals, directories moved whole or cleared; none git ignores among what it doesn't track. Empty where nothing
/// changed.
pub async fn changes(m: &Made, over: &str) -> anyhow::Result<Vec<Change>> {
    let (tracked, ignores) = git_rules(Path::new(over)).await?;
    let volume = super::slates::volume_id(&m.volume)?;
    let over = over.to_owned();
    let read = super::slates::call("review", move |i| Ok(changes_of(i, volume, &over, &tracked, ignores))).await?;
    read.map_err(|e| anyhow::anyhow!("its review: {e}"))
}

/// How many of the files a landing of workspace `m` into `over` writes git ignores (none tracked): what a working
/// tree's `git status` would leave out, for a workspace with no working tree on this machine.
pub async fn ignored_writes(m: &Made, over: &str) -> anyhow::Result<usize> {
    let (tracked, mut ignores) = git_rules(Path::new(over)).await?;
    let volume = super::slates::volume_id(&m.volume)?;
    let over = over.to_owned();
    let counted = super::slates::call("review", move |i| {
        Ok(match present(i, volume, None, &over) {
            Ok(entries) => entries
                .iter()
                .try_fold(0usize, |n, entry| Ok::<usize, Fault>(if writes_untracked(entry, &tracked) && ignored(i, volume, &mut ignores, entry)? { n.saturating_add(1) } else { n })),
            Err(fault) => Err(fault),
        })
    })
    .await?;
    counted.map_err(|e| anyhow::anyhow!("what its landing writes: {e}"))
}

/// The paths the repository at `top` tracks (its index), and git's ignore rules beneath a volume's own `.gitignore`
/// files there: its `info/exclude`, the user's excludes file (`core.excludesFile`, else git's own default) and whether
/// names match whatever their case (`core.ignoreCase`).
async fn git_rules(top: &Path) -> anyhow::Result<(HashSet<String>, Ignores)> {
    let tracked = super::git::tracked(top).await?;
    let (info, user, case_insensitive) = super::git::excludes(top).await?;
    Ok((tracked, Ignores::new(info.as_deref(), user.as_deref(), case_insensitive)?))
}

/// [`changes`] on one client, its review made again from a new snapshot where a file moved since the last one.
fn changes_of(i: &mut Instance, volume: VolumeId, over: &str, tracked: &HashSet<String>, mut ignores: Ignores) -> Result<Vec<Change>, Fault> {
    let mut moving = String::new();
    for _ in 0..TRIES {
        match once(i, volume, over, tracked, &mut ignores)? {
            Read::Changes(changes) => return Ok(changes),
            Read::Moved(path) => moving = path,
        }
    }
    Err(Fault::Moving { path: moving })
}

/// One snapshot's review: its changes, or the path whose bytes moved since it.
enum Read {
    Changes(Vec<Change>),
    Moved(String),
}

fn once(i: &mut Instance, volume: VolumeId, over: &str, tracked: &HashSet<String>, ignores: &mut Ignores) -> Result<Read, Fault> {
    let snapshot = i.snapshot(volume).map_err(Fault::Slates)?;
    let read = present(i, volume, Some(snapshot), over).and_then(|entries| read_entries(i, volume, over, entries, tracked, ignores));
    // The snapshot is the review's alone: let go of it whatever came of it.
    let gone = i.destroy_snapshot(volume, snapshot).map_err(Fault::Slates);
    let read = read?;
    gone?;
    Ok(read)
}

/// Every entry a landing of `volume` (at `snapshot`, else its head) into `over` writes, `.git` left out, presented
/// never writing whatever the volume's mode: none where nothing changed.
fn present(i: &mut Instance, volume: VolumeId, snapshot: Option<SnapshotId>, over: &str) -> Result<Vec<PlannedEntry>, Fault> {
    let request = LandingRequest { volume, snapshot, target: over.to_owned(), filter: Filter { include: Vec::new(), exclude: super::slates::EXCLUDES.iter().map(|p| (*p).to_owned()).collect() } };
    match i.plan_landing(&request).map_err(Fault::Slates)? {
        LandOutcome::ApprovalRequired(approval) => i.approval_entries(&approval).map_err(Fault::Slates),
        // Nothing to write: nothing changed.
        LandOutcome::Written(_) => Ok(Vec::new()),
        LandOutcome::Conflicted(conflicts) => Err(Fault::Conflicted(conflicts)),
        LandOutcome::Refused(refusal) => Err(Fault::Refused(refusal)),
    }
}

/// `entries` as git commits them, each written file read at the volume's head and checked against its entry, and each
/// file whose executable bit alone changed read there and checked against the folder's copy at `over`, whose bytes its
/// landing keeps; what git ignores among what it doesn't track left out; the path whose bytes moved since the entries
/// were presented, where one did.
fn read_entries(i: &mut Instance, volume: VolumeId, over: &str, entries: Vec<PlannedEntry>, tracked: &HashSet<String>, ignores: &mut Ignores) -> Result<Read, Fault> {
    let mut changes = Vec::with_capacity(entries.len());
    for entry in entries {
        if writes_untracked(&entry, tracked) && ignored(i, volume, ignores, &entry)? {
            continue;
        }
        let path = relative(&entry.path);
        let executable_now = executable_change(&entry);
        let change = match entry.action {
            PlannedAction::Create | PlannedAction::Replace => {
                let content = entry.content.ok_or_else(|| Fault::Other(format!("{path}: Slates writes it with no bytes named")))?;
                let bytes = i.read(volume, &entry.path, ReadAt::Head).map_err(Fault::Slates)?;
                if blake3::hash(&bytes).as_bytes() != &content.hash {
                    return Ok(Read::Moved(path));
                }
                Change::File { path, bytes, executable: entry.mode.is_some_and(|m| m & 0o111 != 0) }
            }
            PlannedAction::Symlink { target } => Change::Link { path, target },
            PlannedAction::Delete => Change::Gone { path },
            PlannedAction::Rmdir | PlannedAction::Clear => Change::GoneUnder { path },
            PlannedAction::Rename { from } => Change::Moved { from: relative(&from), to: path },
            PlannedAction::Mkdir => continue,
            // Its mode or Windows attribute bits alone (Slates' contract 41): a commit holds it only where git keeps
            // the change, its executable bit. Its bytes are the folder's, which its landing keeps, so the volume's are
            // held to the folder's copy as a written file's are held to its entry: an agent's write since the
            // snapshot moves them, and the review is made again.
            PlannedAction::Chmod { .. } => {
                let Some(executable) = executable_now else { continue };
                let bytes = i.read(volume, &entry.path, ReadAt::Head).map_err(Fault::Slates)?;
                let kept = std::fs::read(Path::new(over).join(&path)).map_err(|e| Fault::Other(format!("{path}: reading the folder's copy, whose bytes its landing keeps: {e}")))?;
                if bytes != kept {
                    return Ok(Read::Moved(path));
                }
                Change::File { path, bytes, executable }
            }
        };
        changes.push(change);
    }
    Ok(Read::Changes(changes))
}

/// What git commits for an entry whose mode or Windows attribute bits alone change (Slates' contract 41,
/// `PlannedAction::Chmod`): the file executable or not as it lands, where that changed, the one part of a mode git keeps
/// (its two file modes); none where only another permission bit or a Windows attribute changed, of which a commit of
/// the volume's working tree holds nothing, and none for any other entry.
fn executable_change(entry: &PlannedEntry) -> Option<bool> {
    let PlannedAction::Chmod { permissions_was, .. } = entry.action else { return None };
    let executable = entry.mode.is_some_and(|mode| mode & 0o111 != 0);
    (executable != (permissions_was & 0o111 != 0)).then_some(executable)
}

/// A path from the target's root (`/src/a.rs`) as git names it within the repository (`src/a.rs`).
fn relative(path: &str) -> String {
    path.trim_start_matches('/').to_owned()
}

/// Whether `entry` writes a file or a link git doesn't track, or sets such a file's mode, which git's ignore rules apply
/// to.
fn writes_untracked(entry: &PlannedEntry, tracked: &HashSet<String>) -> bool {
    matches!(entry.action, PlannedAction::Create | PlannedAction::Replace | PlannedAction::Symlink { .. } | PlannedAction::Chmod { .. }) && !tracked.contains(&relative(&entry.path))
}

/// Whether git ignores what `entry` writes in `volume`, by its `.gitignore` files read at the volume's head.
fn ignored(i: &mut Instance, volume: VolumeId, ignores: &mut Ignores, entry: &PlannedEntry) -> Result<bool, Fault> {
    let mut read = |file: &str| -> anyhow::Result<Option<Vec<u8>>> {
        match i.read(volume, &format!("/{file}"), ReadAt::Head) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(Error::NotFound(Missing::Entry { .. })) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("reading {file}: {e}")),
        }
    };
    ignores.ignored(&relative(&entry.path), false, &mut read).map_err(|e| Fault::Other(format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry of `action` at `/bin/run.sh`, landing with `mode`.
    fn entry(action: PlannedAction, mode: Option<u32>) -> PlannedEntry {
        PlannedEntry { path: "/bin/run.sh".to_owned(), action, content: None, mode, witnessed: true, conflict: None }
    }

    /// A file's mode or Windows attribute bits alone set: the permission bits it had, the attribute bits it had and
    /// those it lands with.
    fn chmod(permissions_was: u32, windows_was: u16, windows: u16) -> PlannedAction {
        PlannedAction::Chmod { permissions_was, windows_was, windows }
    }

    /// W8 (Slates' contract 41, A-458): a file whose executable bit alone changes is a change its review commits,
    /// executable or not as it lands; one whose other permission bits or Windows attribute bits alone change is none,
    /// since git keeps a file's executable bit alone of its mode; and no other entry is judged a mode change.
    #[test]
    fn w8_a_files_mode_alone_changed_is_committed_where_git_keeps_it() {
        assert_eq!(executable_change(&entry(chmod(0o644, 0, 0), Some(0o100_755))), Some(true), "made executable");
        assert_eq!(executable_change(&entry(chmod(0o755, 0, 0), Some(0o100_644))), Some(false), "made not executable");
        assert_eq!(executable_change(&entry(chmod(0o750, 0, 0), Some(0o100_700))), None, "its group's bits alone: git keeps none of them");
        assert_eq!(executable_change(&entry(chmod(0o644, 0x20, 0x01), Some(0o100_644))), None, "a Windows attribute alone");
        assert_eq!(executable_change(&entry(chmod(0o644, 0, 0), None)), None, "no mode named lands as it was: not executable");
        assert_eq!(executable_change(&entry(PlannedAction::Replace, Some(0o100_755))), None, "a written file is no mode change");
    }

    /// W8 (Slates' contract 41): git's ignore rules judge an untracked file whose mode alone changes as they judge one
    /// written, so a mode set on what git ignores is left out of a review and counted as a write git ignores; a tracked
    /// file's mode change is git's to commit whatever its ignore rules say.
    #[test]
    fn w8_an_untracked_files_mode_change_is_judged_by_gits_ignore_rules() {
        let changed = entry(chmod(0o644, 0, 0), Some(0o100_755));
        let tracked: HashSet<String> = ["bin/run.sh".to_owned()].into_iter().collect();
        assert!(writes_untracked(&changed, &HashSet::new()), "untracked: git's ignore rules apply");
        assert!(!writes_untracked(&changed, &tracked), "tracked: git commits it");
        assert!(!writes_untracked(&entry(PlannedAction::Delete, None), &HashSet::new()), "a removal writes nothing");
    }
}
