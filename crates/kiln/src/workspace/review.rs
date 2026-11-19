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
    let (tracked, ignores, modes) = git_rules(Path::new(over)).await?;
    let volume = super::slates::volume_id(&m.volume)?;
    let over = over.to_owned();
    let read = super::slates::call("review", move |i| Ok(changes_of(i, volume, &over, (&tracked, &modes), ignores))).await?;
    read.map_err(|e| anyhow::anyhow!("its review: {e}"))
}

/// How many of the files a landing of workspace `m` into `over`, leaving out `excludes`, writes git doesn't see changed:
/// a write git ignores (none tracked), or a mode git keeps no part of set on a file it tracks ([`unseen_mode_change`]):
/// what a working tree's `git status` would leave out, for a workspace with no working tree on this machine.
pub async fn unseen_writes(m: &Made, over: &str, excludes: &[String]) -> anyhow::Result<usize> {
    let (tracked, mut ignores, modes) = git_rules(Path::new(over)).await?;
    let volume = super::slates::volume_id(&m.volume)?;
    let (over, excludes) = (over.to_owned(), excludes.to_vec());
    let counted = super::slates::call("review", move |i| {
        Ok(match present(i, volume, None, &over, &excludes) {
            Ok(entries) => entries
                .iter()
                .try_fold(0usize, |n, entry| {
                    let unseen = unseen_mode_change(entry, &tracked, &modes) || (writes_untracked(entry, &tracked) && ignored(i, volume, &mut ignores, entry)?);
                    Ok::<usize, Fault>(if unseen { n.saturating_add(1) } else { n })
                }),
            Err(fault) => Err(fault),
        })
    })
    .await?;
    counted.map_err(|e| anyhow::anyhow!("what its landing writes: {e}"))
}

/// What a landing of workspace `m` into `over` writes that git ignores (none tracked), each a prefix of the volume as a
/// landing's filter leaves it out: the shallowest directory git ignores above it where there is one (`/target`), else
/// the file itself (`/secret.env`). For a workspace with no working tree on this machine, what `git status --ignored`
/// would name in one (`super::slates::excludes`).
pub async fn ignored_paths(m: &Made, over: &str) -> anyhow::Result<Vec<String>> {
    let (tracked, mut ignores, _modes) = git_rules(Path::new(over)).await?;
    let volume = super::slates::volume_id(&m.volume)?;
    let over = over.to_owned();
    let excludes: Vec<String> = super::slates::EXCLUDES.iter().map(|p| (*p).to_owned()).collect();
    let found = super::slates::call("review", move |i| {
        Ok(match present(i, volume, None, &over, &excludes) {
            Ok(entries) => ignored_prefixes(i, volume, &mut ignores, &tracked, &entries),
            Err(fault) => Err(fault),
        })
    })
    .await?;
    found.map_err(|e| anyhow::anyhow!("what its landing writes that git ignores: {e}"))
}

/// The prefixes of `entries` git ignores ([`ignored_paths`]): each untracked write's shallowest ignored ancestor
/// directory, else itself where git ignores the file; one a prefix found already covers adds none.
fn ignored_prefixes(i: &mut Instance, volume: VolumeId, ignores: &mut Ignores, tracked: &HashSet<String>, entries: &[PlannedEntry]) -> Result<Vec<String>, Fault> {
    let mut out: Vec<String> = Vec::new();
    for entry in entries.iter().filter(|e| writes_untracked(e, tracked)) {
        let path = relative(&entry.path);
        if out.iter().any(|p| p.strip_prefix('/').is_some_and(|p| path == p || path.starts_with(&format!("{p}/")))) {
            continue;
        }
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let mut found = None;
        for depth in 1..parts.len() {
            let dir = parts.get(..depth).unwrap_or_default().join("/");
            if ignored_at(i, volume, ignores, &dir, true)? {
                found = Some(dir);
                break;
            }
        }
        if found.is_none() && ignored_at(i, volume, ignores, &path, false)? {
            found = Some(path.clone());
        }
        if let Some(prefix) = found {
            out.push(format!("/{prefix}"));
        }
    }
    Ok(out)
}

/// The paths the repository at `top` tracks (its index), git's ignore rules beneath a volume's own `.gitignore` files
/// there (its `info/exclude`, the user's excludes file, `core.excludesFile` else git's own default, and whether names
/// match whatever their case, `core.ignoreCase`), and how it takes a file's execute bit (`core.fileMode`, [`FileModes`]).
async fn git_rules(top: &Path) -> anyhow::Result<(HashSet<String>, Ignores, FileModes)> {
    let tracked = super::git::tracked(top).await?;
    let (info, user, case_insensitive) = super::git::excludes(top).await?;
    let (trusted, executable) = super::git::file_modes(top).await?;
    Ok((tracked, Ignores::new(info.as_deref(), user.as_deref(), case_insensitive)?, FileModes { trusted, executable }))
}

/// How the repository a review lands in takes a file's execute bit (`core.fileMode`, [`super::git::file_modes`]):
/// trusted, a file is executable as its owner's execute bit says ([`git_executable`]); not, git keeps a tracked file's
/// mode as its index holds it and records a new file as not executable (`ce_mode_from_stat`, read-cache.h at git
/// e1ec09b23), so a commit holds no execute bit taken from the workspace, and a mode changed alone is none to git.
#[derive(Debug, Clone)]
struct FileModes {
    /// Whether the working tree's execute bit is honoured (`core.fileMode` not false).
    trusted: bool,
    /// The tracked files the index records executable (`100755`): read only where the bit is not trusted.
    executable: HashSet<String>,
}

/// [`changes`] on one client, its review made again from a new snapshot where a file moved since the last one.
fn changes_of(i: &mut Instance, volume: VolumeId, over: &str, (tracked, modes): (&HashSet<String>, &FileModes), mut ignores: Ignores) -> Result<Vec<Change>, Fault> {
    let mut moving = String::new();
    for _ in 0..TRIES {
        match once(i, volume, over, (tracked, modes), &mut ignores)? {
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

fn once(i: &mut Instance, volume: VolumeId, over: &str, (tracked, modes): (&HashSet<String>, &FileModes), ignores: &mut Ignores) -> Result<Read, Fault> {
    let snapshot = i.snapshot(volume).map_err(Fault::Slates)?;
    let read = present(i, volume, Some(snapshot), over, &super::slates::EXCLUDES.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>()).and_then(|entries| read_entries(i, volume, over, entries, (tracked, modes), ignores));
    // The snapshot is the review's alone: let go of it whatever came of it.
    let gone = i.destroy_snapshot(volume, snapshot).map_err(Fault::Slates);
    let read = read?;
    gone?;
    Ok(read)
}

/// Every entry a landing of `volume` (at `snapshot`, else its head) into `over` writes, `excludes` left out, presented
/// never writing whatever the volume's mode: none where nothing changed.
fn present(i: &mut Instance, volume: VolumeId, snapshot: Option<SnapshotId>, over: &str, excludes: &[String]) -> Result<Vec<PlannedEntry>, Fault> {
    let request = LandingRequest { volume, snapshot, target: over.to_owned(), filter: Filter { include: Vec::new(), exclude: excludes.to_vec() } };
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
fn read_entries(i: &mut Instance, volume: VolumeId, over: &str, entries: Vec<PlannedEntry>, (tracked, modes): (&HashSet<String>, &FileModes), ignores: &mut Ignores) -> Result<Read, Fault> {
    let mut changes = Vec::with_capacity(entries.len());
    for entry in entries {
        if writes_untracked(&entry, tracked) && ignored(i, volume, ignores, &entry)? {
            continue;
        }
        let path = relative(&entry.path);
        let executable_now = executable_change(&entry, modes);
        let change = match entry.action {
            PlannedAction::Create | PlannedAction::Replace => {
                let content = entry.content.ok_or_else(|| Fault::Other(format!("{path}: Slates writes it with no bytes named")))?;
                let bytes = i.read(volume, &entry.path, ReadAt::Head).map_err(Fault::Slates)?;
                if blake3::hash(&bytes).as_bytes() != &content.hash {
                    return Ok(Read::Moved(path));
                }
                let executable = written_executable(&entry, &path, modes);
                Change::File { path, bytes, executable }
            }
            PlannedAction::Symlink { target } => Change::Link { path, target },
            PlannedAction::Delete => Change::Gone { path },
            PlannedAction::Rmdir | PlannedAction::Clear => Change::GoneUnder { path },
            PlannedAction::Rename { from } => Change::Moved { from: relative(&from), to: path },
            PlannedAction::Mkdir => continue,
            // Its mode or Windows attribute bits alone (Slates' contract 41): a commit holds it only where git keeps
            // the change, its owner's execute bit ([`git_executable`]). Its bytes are the folder's, which its landing keeps, so the volume's are
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

/// Whether git records a file of `mode` as executable: its owner's execute bit alone, all git keeps of a regular file's
/// mode. Git's `ce_permissions` (object.h line 134 at git e1ec09b23, `(((mode) & 0100) ? 0755 : 0644)`), which
/// `create_ce_mode` and `canon_mode` apply to every regular file, so a file only its group or others may execute is
/// `100644` to git, and its commit holds it so.
fn git_executable(mode: u32) -> bool {
    mode & 0o100 != 0
}

/// What git commits for an entry whose mode or Windows attribute bits alone change (Slates' contract 41,
/// `PlannedAction::Chmod`): the file executable or not as it lands ([`git_executable`]), where that changed, the one part
/// of a mode git keeps (its two file modes); none where only another permission bit or a Windows attribute changed, of
/// which a commit of the volume's working tree holds nothing, none in a repository that trusts no execute bit
/// (`core.fileMode` false, [`FileModes`]), and none for any other entry.
fn executable_change(entry: &PlannedEntry, modes: &FileModes) -> Option<bool> {
    if !modes.trusted {
        return None;
    }
    let PlannedAction::Chmod { permissions_was, .. } = entry.action else { return None };
    let executable = entry.mode.is_some_and(git_executable);
    (executable != git_executable(permissions_was)).then_some(executable)
}

/// Whether `entry` sets a mode git keeps no part of on a file git tracks (any permission bit but its owner's execute, a
/// Windows attribute): a change git doesn't see, which holds a landing back for review as a mounted workspace's is held
/// by git's own count (A-496's rule, `super::slates::writable`). A file git doesn't track is listed by git whatever its
/// mode, and its ignore rules judge it ([`writes_untracked`]).
fn unseen_mode_change(entry: &PlannedEntry, tracked: &HashSet<String>, modes: &FileModes) -> bool {
    matches!(entry.action, PlannedAction::Chmod { .. }) && tracked.contains(&relative(&entry.path)) && executable_change(entry, modes).is_none()
}

/// Whether git commits the file `entry` writes at `path` executable: as its owner's execute bit lands
/// ([`git_executable`]) where the repository trusts the bit; where it doesn't (`core.fileMode` false, [`FileModes`]), as
/// the index records the file it tracks, and not executable when it tracks none (`ce_mode_from_stat`).
fn written_executable(entry: &PlannedEntry, path: &str, modes: &FileModes) -> bool {
    if modes.trusted { entry.mode.is_some_and(git_executable) } else { modes.executable.contains(path) }
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
    ignored_at(i, volume, ignores, &relative(&entry.path), false)
}

/// Whether git ignores `path` in `volume` (relative to its top; a directory where `is_dir`), by its `.gitignore` files
/// read at the volume's head.
fn ignored_at(i: &mut Instance, volume: VolumeId, ignores: &mut Ignores, path: &str, is_dir: bool) -> Result<bool, Fault> {
    let mut read = |file: &str| -> anyhow::Result<Option<Vec<u8>>> {
        match i.read(volume, &format!("/{file}"), ReadAt::Head) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(Error::NotFound(Missing::Entry { .. })) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("reading {file}: {e}")),
        }
    };
    ignores.ignored(path, is_dir, &mut read).map_err(|e| Fault::Other(format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry of `action` at `/bin/run.sh`, landing with `mode`.
    fn entry(action: PlannedAction, mode: Option<u32>) -> PlannedEntry {
        PlannedEntry { path: "/bin/run.sh".to_owned(), action, content: None, mode, witnessed: true, conflict: None }
    }

    /// A repository that takes a file's execute bit from its working tree (`core.fileMode` true, git's default).
    fn trusted() -> FileModes {
        FileModes { trusted: true, executable: HashSet::new() }
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
        assert_eq!(executable_change(&entry(chmod(0o644, 0, 0), Some(0o100_755)), &trusted()), Some(true), "made executable");
        assert_eq!(executable_change(&entry(chmod(0o755, 0, 0), Some(0o100_644)), &trusted()), Some(false), "made not executable");
        assert_eq!(executable_change(&entry(chmod(0o750, 0, 0), Some(0o100_700)), &trusted()), None, "its group's bits alone: git keeps none of them");
        assert_eq!(executable_change(&entry(chmod(0o644, 0x20, 0x01), Some(0o100_644)), &trusted()), None, "a Windows attribute alone");
        assert_eq!(executable_change(&entry(chmod(0o644, 0, 0), None), &trusted()), None, "no mode named lands as it was: not executable");
        assert_eq!(executable_change(&entry(PlannedAction::Replace, Some(0o100_755)), &trusted()), None, "a written file is no mode change");
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

    /// W8 (git's `ce_permissions`, object.h line 134 at git e1ec09b23: `(((mode) & 0100) ? 0755 : 0644)`, which
    /// `create_ce_mode` and `canon_mode` apply to every regular file): git judges a file executable by its owner's
    /// execute bit alone. So a mode change that sets only its group's or others' execute is none to git, one that clears
    /// the owner's makes it not executable whatever the others' are, and one that sets the owner's makes it executable
    /// whatever the others' were.
    #[test]
    fn w8_git_judges_a_file_executable_by_its_owners_execute_bit_alone() {
        assert_eq!(executable_change(&entry(chmod(0o644, 0, 0), Some(0o100_655)), &trusted()), None, "the group's and others' execute set, the owner's not");
        assert_eq!(executable_change(&entry(chmod(0o755, 0, 0), Some(0o100_655)), &trusted()), Some(false), "the owner's execute cleared, the others' kept");
        assert_eq!(executable_change(&entry(chmod(0o611, 0, 0), Some(0o100_711)), &trusted()), Some(true), "the owner's execute set beside the others' already set");
        assert!(!git_executable(0o100_655) && !git_executable(0o100_011), "the group's or others' execute alone: 100644 to git");
        assert!(git_executable(0o100_744) && git_executable(0o100_700), "the owner's execute: 100755 to git");
    }

    /// W6 on a workspace with no mount (A-496's rule): a mode change git keeps no part of, on a file git tracks, counts as
    /// one git doesn't see, so its landing is held back for review as a mounted workspace's is by git's own count; one git
    /// sees (the owner's execute bit) doesn't, nor one on a file git doesn't track (git lists that file whatever its mode,
    /// and its ignore rules judge it), nor a write.
    #[test]
    fn w6_a_mode_change_git_cant_see_on_a_tracked_file_holds_its_landing_back() {
        let tracked: HashSet<String> = ["bin/run.sh".to_owned()].into_iter().collect();
        assert!(unseen_mode_change(&entry(chmod(0o644, 0, 0), Some(0o100_654)), &tracked, &trusted()), "the group's execute alone");
        assert!(unseen_mode_change(&entry(chmod(0o644, 0x20, 0x01), Some(0o100_644)), &tracked, &trusted()), "a Windows attribute alone");
        assert!(!unseen_mode_change(&entry(chmod(0o644, 0, 0), Some(0o100_744)), &tracked, &trusted()), "the owner's execute: git sees it");
        assert!(!unseen_mode_change(&entry(chmod(0o644, 0, 0), Some(0o100_654)), &HashSet::new(), &trusted()), "untracked: git lists the file itself");
        assert!(!unseen_mode_change(&entry(PlannedAction::Replace, Some(0o100_644)), &tracked, &trusted()), "a write is no mode change");
    }

    /// W8, git's `core.fileMode` (git-config(1): "Tells Git if the executable bit of files in the working tree is to be
    /// honored"; `ce_mode_from_stat`, read-cache.h at git e1ec09b23). Do: in a repository tracking `bin/run.sh` not
    /// executable and `bin/tool` executable, with `core.fileMode` false, read how it takes a file's execute bit and judge
    /// `bin/run.sh` made executable alone, a rewrite of each file landing with the other mode and a new file landing
    /// executable; then the same with `core.fileMode` true. Expect false: no mode change committed (and one git can't see,
    /// which holds its landing back), each rewrite keeping its index's mode, the new file not executable. Expect true: the
    /// execute bit's change committed, and each file as its owner's execute bit lands.
    #[test]
    fn w8_a_repository_that_trusts_no_execute_bit_commits_no_mode_change() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let top = d.path().to_path_buf();
        let git = |args: &[&str]| -> anyhow::Result<()> {
            let out = std::process::Command::new("git").args(args).current_dir(&top).output()?;
            anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            Ok(())
        };
        git(&["init", "-q", "-b", "main"])?;
        std::fs::create_dir_all(top.join("bin"))?;
        std::fs::write(top.join("bin/run.sh"), "echo run\n")?;
        std::fs::write(top.join("bin/tool"), "echo tool\n")?;
        git(&["add", "."])?;
        git(&["update-index", "--chmod=+x", "bin/tool"])?;
        let tracked: HashSet<String> = ["bin/run.sh".to_owned(), "bin/tool".to_owned()].into_iter().collect();
        let modes_of = |top: &std::path::Path| -> anyhow::Result<FileModes> {
            let (trusted, executable) = crate::rt::block_on(super::super::git::file_modes(top))?;
            Ok(FileModes { trusted, executable })
        };
        let rewrite = |path: &str, mode: u32| PlannedEntry { path: format!("/{path}"), action: PlannedAction::Replace, content: None, mode: Some(mode), witnessed: true, conflict: None };
        let made_executable = entry(chmod(0o644, 0, 0), Some(0o100_755));

        git(&["config", "core.fileMode", "false"])?;
        let untrusted = modes_of(&top)?;
        assert!(!untrusted.trusted, "core.fileMode false: the working tree's execute bit is not honoured");
        assert_eq!(executable_change(&made_executable, &untrusted), None, "an execute bit changed alone is no change to commit");
        assert!(unseen_mode_change(&made_executable, &tracked, &untrusted), "nor one git sees: its landing is held back for review");
        assert!(!written_executable(&rewrite("bin/run.sh", 0o100_755), "bin/run.sh", &untrusted), "a rewrite keeps the index's 100644");
        assert!(written_executable(&rewrite("bin/tool", 0o100_644), "bin/tool", &untrusted), "a rewrite keeps the index's 100755");
        assert!(!written_executable(&rewrite("bin/new", 0o100_755), "bin/new", &untrusted), "a new file is 100644");

        git(&["config", "core.fileMode", "true"])?;
        let honoured = modes_of(&top)?;
        assert!(honoured.trusted, "core.fileMode true: the working tree's execute bit is honoured");
        assert_eq!(executable_change(&made_executable, &honoured), Some(true), "the execute bit's change is committed");
        assert!(!unseen_mode_change(&made_executable, &tracked, &honoured), "and git sees it");
        assert!(written_executable(&rewrite("bin/run.sh", 0o100_755), "bin/run.sh", &honoured), "a rewrite as its owner's bit lands");
        assert!(!written_executable(&rewrite("bin/tool", 0o100_644), "bin/tool", &honoured), "a rewrite as its owner's bit lands");
        Ok(())
    }
}
