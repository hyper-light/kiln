//! A synced repo's git refs (`docs/sync.md`, "Modes"): its branches and tags, and with its files
//! its `HEAD`, kept the same on every copy as its files are. A ref is a path of the tree under
//! `.git/` (`.git/refs/heads/main`, `.git/HEAD`) whose version is what it names: a commit (a tag's
//! object, for an annotated tag), or for `HEAD` the branch checked out (`ref: refs/heads/main`).
//! The commits a change brings travel as a git bundle, a blob like a file's contents, made of what
//! the copies don't hold yet: a copy that has taken the log so far holds every commit it builds on.
//!
//! The conflict rule is the files': a branch moved from a commit the log no longer has (another
//! copy moved it first) goes beside it, `main.conflict-<machine>-<time>`, on every copy, so no
//! commit is lost; on the copy that lost, its branch takes the log's commit, its own commits being
//! on the branch beside. A `HEAD` that lost goes nowhere: its copy checks out the log's.
//!
//! Blocking: it runs git.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::Version;

/// The tree's path of a repo's `HEAD`.
pub const HEAD: &str = ".git/HEAD";
/// Where the tree's branches and tags are.
const REFS: &str = ".git/refs/";
/// What a `HEAD` on a branch names, before the branch.
const SYMBOLIC: &str = "ref: ";
/// Where a bundle's tips are named while it is made.
const TIPS: &str = "refs/kiln-sync/tip-";
/// How long the repository's remote has to send commits a bundle builds on.
const FETCH_TIME: Duration = Duration::from_secs(60);

/// Whether `path` is one of the tree's refs rather than a file.
pub fn is_ref(path: &str) -> bool {
    path == HEAD || path.starts_with(REFS)
}

/// Whether `path` is a ref kiln syncs: `HEAD`, or a branch or tag whose name git takes.
pub fn safe_ref(path: &str) -> bool {
    path == HEAD || path.strip_prefix(".git/").is_some_and(|name| (name.starts_with("refs/heads/") || name.starts_with("refs/tags/")) && valid_name(name))
}

/// git's rules for a ref's name (`git check-ref-format`).
fn valid_name(name: &str) -> bool {
    !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && name.split('/').all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
        && !name.chars().any(|c| c.is_ascii_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

/// Where a branch's or tag's change goes when the log kept another commit: beside it, named for the
/// machine and the step's time (`main.conflict-fedora-20261022-183000`). A `HEAD` goes nowhere.
pub fn conflict_name(path: &str, by: &str, at_ms: i64) -> Option<String> {
    (path != HEAD).then(|| format!("{path}.conflict-{}", super::stamp(by, at_ms))).filter(|n| safe_ref(n))
}

/// A ref's name as git says it: `refs/heads/main`, `HEAD`.
fn name_of(path: &str) -> &str {
    path.strip_prefix(".git/").unwrap_or(path)
}

/// The branch a `HEAD` version names (`refs/heads/main`), if it names one.
fn branch_of(v: &Version) -> Option<&str> {
    match v {
        Version::Ref { commit } => commit.strip_prefix(SYMBOLIC),
        _ => None,
    }
}

fn git(root: &Path) -> Command {
    let mut c = Command::new("git");
    crate::os::git_no_prompts(&mut c);
    c.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes").env("LC_ALL", "C").arg("-C").arg(root);
    c
}

/// Runs git in `root`: what it printed, or what it said when it failed.
fn run(root: &Path, args: &[&str]) -> io::Result<String> {
    let out = git(root).args(args).stdin(Stdio::null()).output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(io::Error::other(format!("git {}: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim())))
    }
}

/// Whether the folder is a git repository's working tree (a worktree's too).
pub fn is_repo(root: &Path) -> bool {
    root.join(".git").exists()
}

/// The repo's branches and tags as the tree has them, and its `HEAD` too with `head`.
pub fn read(root: &Path, head: bool) -> io::Result<BTreeMap<String, Version>> {
    let mut out = BTreeMap::new();
    for line in run(root, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/heads", "refs/tags"])?.lines() {
        let Some((name, commit)) = line.split_once(' ') else { continue };
        let path = format!(".git/{name}");
        if safe_ref(&path) {
            out.insert(path, Version::Ref { commit: commit.trim().to_owned() });
        }
    }
    if head && let Some(v) = now(root, HEAD)? {
        out.insert(HEAD.to_owned(), v);
    }
    Ok(out)
}

/// What the ref at `path` names now (none: there's no such ref).
pub fn now(root: &Path, path: &str) -> io::Result<Option<Version>> {
    if path == HEAD {
        // On a branch (one with no commit yet too): the branch; else the commit checked out.
        if let Ok(branch) = run(root, &["symbolic-ref", "-q", "HEAD"]) {
            return Ok(Some(Version::Ref { commit: format!("{SYMBOLIC}{}", branch.trim()) }));
        }
        return Ok(run(root, &["rev-parse", "-q", "--verify", "HEAD"]).ok().map(|c| Version::Ref { commit: c.trim().to_owned() }));
    }
    Ok(run(root, &["rev-parse", "-q", "--verify", name_of(path)]).ok().map(|c| Version::Ref { commit: c.trim().to_owned() }))
}

/// Whether the repository holds the object `id` (a commit, a tag).
pub fn holds(root: &Path, id: &str) -> bool {
    id.len() >= 40 && id.chars().all(|c| c.is_ascii_hexdigit()) && run(root, &["cat-file", "-e", id]).is_ok()
}

/// Whether commit `later` is `earlier` or has it among its own (`git merge-base --is-ancestor`).
pub fn descends(root: &Path, later: &str, earlier: &str) -> bool {
    later == earlier || run(root, &["merge-base", "--is-ancestor", earlier, later]).is_ok()
}

/// Whether git is in the middle of something here (a rebase, a merge, a cherry-pick, a revert, a
/// bisect): its refs and `HEAD` move through states that aren't for the other copies meanwhile.
pub fn busy(root: &Path) -> bool {
    let Ok(dir) = run(root, &["rev-parse", "--git-dir"]) else { return false };
    let dir = root.join(dir.trim());
    ["rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "BISECT_LOG"].iter().any(|f| dir.join(f).exists())
}

/// The branch checked out (`refs/heads/main`), if one is.
fn checked_out(root: &Path) -> Option<String> {
    run(root, &["symbolic-ref", "-q", "HEAD"]).ok().map(|b| b.trim().to_owned())
}

/// A bundle in `out` of what the objects `tips` reach that those of `have` don't (what the other
/// copies hold already): false, and no bundle, when they hold all of it.
pub fn bundle(root: &Path, tips: &[&str], have: &[&str], out: &Path) -> io::Result<bool> {
    let tips: Vec<&str> = tips.iter().copied().filter(|t| holds(root, t)).collect();
    if tips.is_empty() {
        return Ok(false);
    }
    // A bundle names refs, not commits: each tip is one for as long as it takes.
    let names: Vec<String> = (0..tips.len()).map(|n| format!("{TIPS}{n}")).collect();
    let made = (|| {
        for (name, tip) in names.iter().zip(&tips) {
            run(root, &["update-ref", name, tip])?;
        }
        let file = out.to_string_lossy().into_owned();
        let mut args: Vec<String> = vec!["bundle".into(), "create".into(), "--quiet".into(), file];
        args.extend(names.iter().cloned());
        args.extend(have.iter().filter(|h| holds(root, h)).map(|h| format!("^{h}")));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match run(root, &args) {
            Ok(_) => Ok(true),
            // Nothing the others lack.
            Err(e) if e.to_string().contains("empty bundle") => Ok(false),
            Err(e) => Err(e),
        }
    })();
    for name in &names {
        let _ = run(root, &["update-ref", "-d", name]);
    }
    made
}

/// Takes a bundle's objects into the repository, its refs left as they are. One that builds on
/// commits this copy lacks (it missed a bundle no longer kept) asks the repository's remote for
/// them first.
pub fn take(root: &Path, bundle: &Path) -> io::Result<()> {
    let file = bundle.to_string_lossy().into_owned();
    if run(root, &["bundle", "unbundle", &file]).is_ok() {
        return Ok(());
    }
    fetch_remote(root);
    run(root, &["bundle", "unbundle", &file]).map(|_| ())
}

/// `git fetch origin`, for no longer than [`FETCH_TIME`].
fn fetch_remote(root: &Path) {
    let Ok(mut child) = git(root).args(["fetch", "--quiet", "origin"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return };
    let start = Instant::now();
    while start.elapsed() < FETCH_TIME {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// What setting a ref did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Set {
    Done,
    /// Left as it is, and why: the copy's own work is in the way.
    Kept(String),
}

/// Makes the ref at `path` name `to` (none: deletes it), the objects it names here already. With
/// `files` the copy's working tree is synced too, so it holds what the commit checked out has (or
/// will, as the log brings it): moving the branch checked out moves the index with it and leaves
/// the files, and `git status` agrees with the copy it came from. Without, the branch checked out
/// moves only forward and only as `git merge --ff-only` would, taking the files with it; else it
/// stays, and says so.
pub fn set(root: &Path, path: &str, to: Option<&Version>, files: bool) -> io::Result<Set> {
    if path == HEAD {
        return set_head(root, to, files);
    }
    let name = name_of(path);
    let commit = match to {
        Some(Version::Ref { commit }) => Some(commit.as_str()),
        Some(_) => return Err(io::Error::other(format!("{name}: not a commit"))),
        None => None,
    };
    if checked_out(root).as_deref() != Some(name) {
        match commit {
            Some(c) => run(root, &["update-ref", name, c])?,
            None => run(root, &["update-ref", "-d", name])?,
        };
        return Ok(Set::Done);
    }
    let Some(commit) = commit else { return Ok(Set::Kept(format!("{name} is checked out here: it stays"))) };
    if files {
        run(root, &["reset", "--quiet", "--mixed", commit])?;
        return Ok(Set::Done);
    }
    match run(root, &["merge", "--ff-only", "--quiet", commit]) {
        Ok(_) => Ok(Set::Done),
        Err(e) => Ok(Set::Kept(format!("{name} is checked out here and can't move as a fast-forward ({e}): it stays"))),
    }
}

/// Checks out the branch (or commit) `to` names, its files being synced: the index follows, the
/// files are the log's.
fn set_head(root: &Path, to: Option<&Version>, files: bool) -> io::Result<Set> {
    let Some(to) = to.filter(|_| files) else { return Ok(Set::Kept("HEAD is this copy's own".into())) };
    match (branch_of(to), to) {
        (Some(branch), _) => run(root, &["symbolic-ref", "HEAD", branch])?,
        (None, Version::Ref { commit }) => run(root, &["update-ref", "--no-deref", "HEAD", commit])?,
        (None, _) => return Err(io::Error::other("HEAD: not a commit")),
    };
    // A branch with no commit yet has nothing to take into the index.
    if run(root, &["rev-parse", "-q", "--verify", "HEAD"]).is_ok() {
        run(root, &["reset", "--quiet", "--mixed"])?;
    }
    Ok(Set::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_kiln_syncs_are_branches_tags_and_head_with_names_git_takes() {
        for ok in [HEAD, ".git/refs/heads/main", ".git/refs/heads/feature/x", ".git/refs/tags/v1.2", ".git/refs/heads/main.conflict-fedora-20261022-183000"] {
            assert!(safe_ref(ok), "{ok}");
        }
        for bad in [
            ".git/config",
            ".git/refs/remotes/origin/main",
            ".git/refs/heads/a..b",
            ".git/refs/heads/x.lock",
            ".git/refs/heads/.hidden",
            ".git/refs/heads/a b",
            ".git/refs/heads/a/",
            ".git/refs/kiln-sync/tip-0",
            "refs/heads/main",
        ] {
            assert!(!safe_ref(bad), "{bad}");
        }
        assert!(is_ref(HEAD) && is_ref(".git/refs/heads/main") && !is_ref("src/main.rs"));
        let name = conflict_name(".git/refs/heads/main", "fedora.attlocal.net", 1_792_717_800_000);
        assert_eq!(name.as_deref(), Some(".git/refs/heads/main.conflict-fedora.attlocal.net-20261023-011000"));
        assert_eq!(conflict_name(HEAD, "fedora", 0), None);
    }
}
