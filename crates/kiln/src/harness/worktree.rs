//! A git worktree an agent works in, moved with it (`super::moving`). Sync keeps a repository's
//! main working tree in step on every machine, but a worktree inside it (`.claude/worktrees/x`)
//! is a repository of its own to git, which sync leaves alone. So a session working in one carries
//! it in its package: where it is in the repository, the commit its HEAD is at, and what differs
//! from that commit (changed, staged and new files, and the ones deleted), with what git ignores
//! there ([`super::carry`]); its branches, stash and index go as any repository's do
//! ([`super::gitstate`]).
//!
//! On the machine it goes to, the repository's copy is there (sync made it), so the worktree is
//! made from it at that commit and the differences written over it, then its HEAD and index put as
//! they were. A worktree there already (the session coming back) keeps what it had: anything
//! changed in it is stashed first, never thrown away.
//!
//! Only a worktree of the synced repository moves this way; a repository of its own nested inside
//! (or a submodule) doesn't move, and says why.
//!
//! Blocking: git does the work.

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::carry::{Carried, CarriedFile};

/// A worktree carried: where it is in the repository (relative to its top folder), the commit its
/// HEAD is at, and what differs from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub rel: String,
    /// The session's folder in the worktree.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sub: String,
    pub commit: String,
    /// Files that differ from the commit (changed, staged, new).
    pub changed: Carried,
    /// Files the commit has that the worktree no longer does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<String>,
    /// What git ignores there.
    #[serde(default)]
    pub ignored: Carried,
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "kiln")
        .env("GIT_AUTHOR_EMAIL", "kiln@localhost")
        .env("GIT_COMMITTER_NAME", "kiln")
        .env("GIT_COMMITTER_EMAIL", "kiln@localhost")
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    } else {
        Err(format!("git {}: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// What a folder nested in a synced repository is to git.
#[derive(Debug, PartialEq, Eq)]
pub enum Nested {
    /// A worktree of the repository itself: it moves (`rel`: where, in the repository).
    Worktree { root: PathBuf, rel: String },
    /// A repository of its own, or a submodule: it doesn't.
    Own(PathBuf),
}

/// The repository `cwd` is in, when it's nested in the synced folder `copy`: a worktree of the
/// same repository, or one of its own. None when `cwd` is in `copy`'s own working tree.
pub fn nested(cwd: &Path, copy: &Path) -> Option<Nested> {
    let root = canon(Path::new(&git(cwd, &["rev-parse", "--show-toplevel"]).ok()?));
    let copy = canon(copy);
    if root == copy || !root.starts_with(&copy) {
        return None;
    }
    let common = |d: &Path| git(d, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).ok().map(|c| canon(Path::new(&c)));
    let rel = root.strip_prefix(&copy).ok()?.to_string_lossy().replace('\\', "/");
    match (common(&root), common(&copy)) {
        (Some(a), Some(b)) if a == b => Some(Nested::Worktree { root, rel }),
        _ => Some(Nested::Own(root)),
    }
}

fn read(root: &Path, rel: &str) -> Option<CarriedFile> {
    let full = root.join(rel);
    let meta = std::fs::symlink_metadata(&full).ok()?;
    if !meta.is_file() {
        return None;
    }
    let bytes = std::fs::read(&full).ok()?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o7777
    };
    #[cfg(not(unix))]
    let mode = 0o644;
    Some(CarriedFile { path: rel.to_owned(), mode, data: base64::engine::general_purpose::STANDARD.encode(bytes) })
}

/// The worktree at `root` (`rel` in its repository): its HEAD's commit, what differs from it, and
/// what git ignores there. An error says what would be left behind.
pub fn capture(root: &Path, rel: &str) -> Result<Worktree, String> {
    let commit = git(root, &["rev-parse", "HEAD"]).map_err(|_| format!("the worktree at {} has no commit yet", root.display()))?;
    let z = |args: &[&str]| -> Result<Vec<String>, String> { Ok(git(root, args)?.split('\0').filter(|p| !p.is_empty()).map(str::to_owned).collect()) };
    // Against HEAD: what's staged and what isn't, deleted or not; and what's new.
    let mut differ = z(&["diff", "--name-only", "-z", "--no-renames", "HEAD"])?;
    differ.extend(z(&["ls-files", "-z", "--others", "--exclude-standard"])?);
    differ.sort();
    differ.dedup();
    let mut changed = Vec::new();
    let mut deleted = Vec::new();
    for p in differ {
        match read(root, &p) {
            Some(f) => changed.push(f),
            None if !root.join(&p).exists() => deleted.push(p),
            None => {}
        }
    }
    let size: usize = changed.iter().map(|f| f.data.len() / 4 * 3).sum();
    if size > 64 << 20 {
        return Err(format!("what differs from its last commit in the worktree at {} comes to {} MB, more than a move carries: commit some of it, and it moves", root.display(), size >> 20));
    }
    let ignored = super::carry::ignored(root)?;
    Ok(Worktree { rel: rel.to_owned(), sub: String::new(), commit, changed: Carried { files: changed }, deleted, ignored })
}

fn write(path: &Path, f: &CarriedFile) -> Result<(), String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(&f.data).map_err(|e| format!("{}: {e}", f.path))?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", f.path))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", f.path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(f.mode)).map_err(|e| format!("{}: {e}", f.path))?;
    }
    Ok(())
}

/// Puts the worktree `w` (from machine `from`) in the repository whose copy is at `copy`, its
/// commits already fetched there ([`super::gitstate`] brings them). Where it is, and what it did
/// in words; an error when it couldn't be made.
pub fn place(copy: &Path, w: &Worktree, from: &str) -> Result<(PathBuf, Vec<String>), String> {
    let at = copy.join(&w.rel);
    let mut said = Vec::new();
    if !git(copy, &["cat-file", "-e", &format!("{}^{{commit}}", w.commit)]).is_ok() {
        return Err(format!("its worktree's commit {} isn't in the repository here", w.commit.get(..10).unwrap_or(&w.commit)));
    }
    if at.join(".git").exists() {
        // There already (the session coming back): what changed in it is kept, stashed.
        let dirty = !git(&at, &["status", "--porcelain"]).unwrap_or_default().is_empty();
        if dirty {
            git(&at, &["stash", "push", "-u", "-q", "-m", &format!("kiln: what was here when the session came back from {from}")])?;
            said.push("What had changed in its worktree here is kept in the stash".to_owned());
        }
        git(&at, &["checkout", "-q", "--detach", "-f", &w.commit])?;
    } else {
        if at.exists() && std::fs::read_dir(&at).map(|mut d| d.next().is_some()).unwrap_or(false) {
            return Err(format!("{} is here already, and isn't a worktree of the repository", at.display()));
        }
        git(copy, &["worktree", "add", "-q", "--detach", &at.to_string_lossy(), &w.commit])?;
    }
    for f in &w.changed.files {
        let Some(path) = inside(&at, &f.path) else { continue };
        write(&path, f)?;
    }
    for d in &w.deleted {
        if let Some(path) = inside(&at, d) {
            let _ = std::fs::remove_file(path);
        }
    }
    said.extend(super::carry::place(&at, &w.ignored, from));
    said.insert(0, format!("Its worktree is here, {}, as it was on {from}", w.rel));
    Ok((at, said))
}

fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    p.components().all(|c| matches!(c, std::path::Component::Normal(_))).then(|| root.join(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Git for the tests' own repositories, free of this machine's config (one that signs
    /// commits would ask for a key).
    fn sh(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "kiln")
            .env("GIT_AUTHOR_EMAIL", "kiln@localhost")
            .env("GIT_COMMITTER_NAME", "kiln")
            .env("GIT_COMMITTER_EMAIL", "kiln@localhost")
            .output()?;
        anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    }

    fn put(dir: &Path, rel: &str, text: &str) -> anyhow::Result<()> {
        let p = dir.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, text)?;
        Ok(())
    }

    /// A repository with a worktree inside it on machine a, and a's copy on machine b (a clone,
    /// as sync makes one): the worktree, its branch and its work arrive as they were.
    #[test]
    fn a_worktree_moves_as_it_was() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let a = t.path().join("a");
        std::fs::create_dir_all(&a)?;
        sh(&a, &["init", "-q", "-b", "main"])?;
        put(&a, ".gitignore", ".env\n.trees/\n")?;
        put(&a, "README", "one\n")?;
        put(&a, "old.txt", "old\n")?;
        sh(&a, &["add", "."])?;
        sh(&a, &["commit", "-qm", "first"])?;
        let b = t.path().join("b");
        let out = Command::new("git").args(["clone", "-q"]).arg(&a).arg(&b).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output()?;
        anyhow::ensure!(out.status.success(), "clone");
        // On a: a worktree on its own branch, with a commit, a staged file, an edit, a deletion,
        // a new file and a .env.
        sh(&a, &["worktree", "add", "-q", ".trees/wt", "-b", "wt"])?;
        let w = a.join(".trees/wt");
        put(&w, "README", "two\n")?;
        sh(&w, &["commit", "-qam", "on wt"])?;
        put(&w, "staged.txt", "staged\n")?;
        sh(&w, &["add", "staged.txt"])?;
        put(&w, "README", "unstaged\n")?;
        std::fs::remove_file(w.join("old.txt"))?;
        put(&w, "new.txt", "new\n")?;
        put(&w, ".env", "TOKEN=wt\n")?;
        let status_a = sh(&w, &["status", "--porcelain"])?;
        assert!(matches!(nested(&w, &a), Some(Nested::Worktree { ref rel, .. }) if rel == ".trees/wt"));
        let st = super::super::gitstate::capture(&w, &super::super::gitstate::tips(&b)).map_err(anyhow::Error::msg)?;
        let carried = capture(&w, ".trees/wt").map_err(anyhow::Error::msg)?;
        assert_eq!(carried.deleted, ["old.txt"]);
        // On b: its commits, then the worktree, then its HEAD and index.
        super::super::gitstate::fetch(&b, &st, "lab-a").map_err(anyhow::Error::msg)?;
        let (wt, mut notes) = place(&b, &carried, "lab-a").map_err(anyhow::Error::msg)?;
        notes.extend(super::super::gitstate::restore_at(&b, &wt, &st, "lab-a"));
        assert_eq!(wt, b.join(".trees/wt"));
        assert_eq!(sh(&wt, &["symbolic-ref", "HEAD"])?, "refs/heads/wt", "{notes:?}");
        assert_eq!(sh(&wt, &["log", "--format=%s"])?, "on wt\nfirst");
        assert_eq!(sh(&wt, &["status", "--porcelain"])?, status_a, "{notes:?}");
        assert_eq!(std::fs::read_to_string(wt.join(".env"))?, "TOKEN=wt\n");
        assert_eq!(sh(&b, &["symbolic-ref", "HEAD"])?, "refs/heads/main", "the copy's own HEAD stays");
        Ok(())
    }

    /// Coming back to a worktree that's there already: what changed in it is stashed, not lost.
    #[test]
    fn a_worktree_there_already_keeps_what_it_had() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let a = t.path().join("a");
        std::fs::create_dir_all(&a)?;
        sh(&a, &["init", "-q", "-b", "main"])?;
        put(&a, ".gitignore", ".trees/\n")?;
        put(&a, "README", "one\n")?;
        sh(&a, &["add", "."])?;
        sh(&a, &["commit", "-qm", "first"])?;
        sh(&a, &["worktree", "add", "-q", ".trees/wt", "-b", "wt"])?;
        let w = a.join(".trees/wt");
        let carried = capture(&w, ".trees/wt").map_err(anyhow::Error::msg)?;
        // Someone changed it here meanwhile.
        put(&w, "README", "changed here\n")?;
        let (_, said) = place(&a, &carried, "lab-b").map_err(anyhow::Error::msg)?;
        assert!(said.iter().any(|s| s.contains("kept in the stash")), "{said:?}");
        assert_eq!(std::fs::read_to_string(w.join("README"))?, "one\n");
        assert!(sh(&w, &["stash", "list"])?.contains("came back from lab-b"));
        Ok(())
    }
}
