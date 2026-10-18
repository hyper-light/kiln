//! The git repositories on a runner's machine (`docs/sync.md`, "Repositories on machines"): the
//! ones in its usual folders, found by looking ([`look`]); the ones kiln clones there, from a
//! bundle the server holds or from their remote ([`clone_into`]); bundles of them, for clones on
//! other machines ([`bundle`]); and the folder of one moved away, deleted ([`delete`]) only when it
//! is that repository and nowhere kiln or its user lives.
//!
//! Git does the work and never prompts (`GIT_TERMINAL_PROMPT=0`); looking takes no lock a person's
//! git would trip over (`--no-optional-locks`). Nothing here reaches past the folder it is given.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::wire::{CloneOrder, Found};

/// Folders under home where people keep repositories, looked in two levels deep (`~/src/kiln`,
/// `~/src/hyper-light/kiln`). Home itself is looked in one level deep.
const ROOTS: &[&str] = &["Projects", "projects", "src", "code", "Code", "dev", "Developer", "work", "repos", "git", "workspace", "go/src"];
/// Folders never looked in: dependencies, build output, and a Mac's own.
const SKIP: &[&str] = &["node_modules", "target", "vendor", "dist", "build", "venv", "__pycache__", "Library", "Applications", "Pictures", "Music", "Movies"];
/// Repositories one look reports at the most.
pub const MOST: usize = 400;
/// Folder entries one look reads at the most: a folder of millions must not hold a runner.
const ENTRIES: usize = 50_000;
/// How long git may take over one repository when looking, and over a clone or bundle.
const STATUS_TIME: Duration = Duration::from_secs(20);
const LONG: Duration = Duration::from_secs(4 * 3600);

/// A git command that never prompts, in `dir`.
fn git(dir: Option<&Path>) -> Command {
    let mut c = Command::new("git");
    crate::os::git_no_prompts(&mut c);
    c.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes").env("LC_ALL", "C");
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    c
}

/// Runs git, its standard output on success; a failure says what git said.
async fn run<S: AsRef<OsStr>>(dir: Option<&Path>, args: &[S], timeout: Duration) -> anyhow::Result<String> {
    let mut c = git(dir);
    c.args(args);
    let what = args.first().map(|a| a.as_ref().to_string_lossy().into_owned()).unwrap_or_default();
    let out = crate::proc::output(c, timeout).await.with_context(|| format!("git {what}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {what}: {}", crate::util::truncate(err.trim(), 600));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether git is there to run.
pub fn have_git() -> bool {
    crate::os::have("git")
}

/// `url` without the credentials an `http(s)` URL may carry (`https://token@host/…`): never sent
/// on, never shown. An ssh user (`git@host:…`) is no secret and stays.
pub fn strip_credentials(url: &str) -> String {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_owned() };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return url.to_owned();
    }
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if path.is_empty() && !rest.contains('/') { format!("{scheme}://{host}") } else { format!("{scheme}://{host}/{path}") }
}

// ------------------------------------------------------------------------------ looking

/// What a look found, and why it found nothing, when it couldn't look.
pub struct Looked {
    pub repos: Vec<Found>,
    pub error: Option<String>,
}

/// The git repositories on this machine: in `extra` (the clones kiln made here and the synced
/// copies), at home one level deep, and two levels deep under its usual folders ([`ROOTS`]). Hidden
/// folders, symlinks and [`SKIP`] are not looked in, nor is a repository's own inside.
pub async fn look(home: PathBuf, extra: Vec<PathBuf>, kiln: BTreeSet<String>) -> Looked {
    if !have_git() {
        return Looked { repos: Vec::new(), error: Some("git isn't installed on this machine".to_owned()) };
    }
    let found = crate::rt::blocking(move || candidates(&home, &extra)).await.unwrap_or_default();
    let mut repos = Vec::new();
    for path in found {
        if let Some(mut f) = found_at(&path).await {
            f.kiln = kiln.contains(&f.path);
            repos.push(f);
        }
    }
    repos.sort_by(|a, b| a.path.cmp(&b.path));
    Looked { repos, error: None }
}

fn is_repo(p: &Path) -> bool {
    std::fs::symlink_metadata(p.join(".git")).is_ok()
}

/// A folder's own folders worth looking in: real folders (no symlinks), not hidden, not [`SKIP`].
fn folders(dir: &Path, budget: &mut usize) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in rd {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

/// What makes two paths one folder: its device and inode (so `~/Projects` and `~/projects` on a
/// Mac's disk, or a folder reached twice, are looked in once).
#[cfg(unix)]
fn identity(p: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(p).ok().map(|m| format!("{}:{}", m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn identity(p: &Path) -> Option<String> {
    let real = std::fs::canonicalize(p).ok()?;
    Some(real.to_string_lossy().to_lowercase())
}

/// The folders that are git repositories, in the order a person would list them.
fn candidates(home: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut budget = ENTRIES;
    let mut add = |p: PathBuf, out: &mut Vec<PathBuf>| {
        if out.len() < MOST && identity(&p).is_some_and(|id| seen.insert(id)) {
            out.push(p);
        }
    };
    for p in extra {
        if p.is_absolute() && is_repo(p) {
            add(p.clone(), &mut out);
        }
    }
    for child in folders(home, &mut budget) {
        if is_repo(&child) {
            add(child, &mut out);
        }
    }
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for root in ROOTS {
        let root = home.join(root);
        if !root.is_dir() || !identity(&root).is_some_and(|id| roots.insert(id)) {
            continue;
        }
        if is_repo(&root) {
            add(root, &mut out);
            continue;
        }
        for child in folders(&root, &mut budget) {
            if is_repo(&child) {
                add(child, &mut out);
                continue;
            }
            for grandchild in folders(&child, &mut budget) {
                if is_repo(&grandchild) {
                    add(grandchild, &mut out);
                }
            }
        }
    }
    out
}

/// What `git status --porcelain=v2 --branch` says of a repository.
#[derive(Debug, Default, PartialEq, Eq)]
struct Status {
    head: Option<String>,
    branch: Option<String>,
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
    changed: u32,
}

fn parse_status(out: &str) -> Status {
    let mut s = Status::default();
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("# branch.oid ") {
            s.head = (v != "(initial)").then(|| v.to_owned());
        } else if let Some(v) = line.strip_prefix("# branch.head ") {
            s.branch = (v != "(detached)").then(|| v.to_owned());
        } else if let Some(v) = line.strip_prefix("# branch.upstream ") {
            s.upstream = Some(v.to_owned());
        } else if let Some(v) = line.strip_prefix("# branch.ab ") {
            let mut parts = v.split_whitespace();
            s.ahead = parts.next().and_then(|a| a.trim_start_matches('+').parse().ok()).unwrap_or(0);
            s.behind = parts.next().and_then(|b| b.trim_start_matches('-').parse().ok()).unwrap_or(0);
        } else if !line.starts_with('#') && !line.is_empty() {
            s.changed = s.changed.saturating_add(1);
        }
    }
    s
}

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// One repository as it is now; none when git won't say (not a repository after all, or one this
/// user may not read).
pub async fn found_at(path: &Path) -> Option<Found> {
    let status = run(Some(path), &["--no-optional-locks", "status", "--porcelain=v2", "--branch", "--untracked-files=no"], STATUS_TIME).await.ok()?;
    let s = parse_status(&status);
    let origin = run(Some(path), &["config", "--get", "remote.origin.url"], STATUS_TIME).await.ok().map(|o| strip_credentials(&o)).filter(|o| !o.is_empty());
    let dot = path.join(".git");
    let active = [dot.join("index"), dot.join("HEAD"), dot.join("FETCH_HEAD"), dot].iter().map(|p| mtime(p)).max().unwrap_or(0);
    Some(Found {
        path: path.to_string_lossy().into_owned(),
        origin,
        branch: s.branch,
        head: s.head,
        upstream: s.upstream,
        ahead: s.ahead,
        behind: s.behind,
        changed: s.changed,
        active,
        kiln: false,
    })
}

// ------------------------------------------------------------------------------ the clones kiln made here

/// The clones kiln made on this machine (`<state>/sync/clones.json`): looked at wherever they
/// are, and deleted only by kiln when asked.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub clones: BTreeMap<String, Kept>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    /// The repository's key.
    pub key: String,
    /// The task that made it.
    pub task: String,
    /// Unix seconds.
    pub at: i64,
}

impl Registry {
    pub fn load(file: &Path) -> Self {
        std::fs::read(file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, file: &Path) -> anyhow::Result<()> {
        super::group::save_atomic(file, &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn paths(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.clones.keys().map(PathBuf::from)
    }
}

// ------------------------------------------------------------------------------ cloning

/// `~` and `~/…` on this machine (`home`); any other path as it is.
pub fn expand(path: &str, home: &Path) -> PathBuf {
    let p = path.trim();
    match p {
        "~" => home.to_path_buf(),
        _ => match p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
            Some(rest) => home.join(rest),
            None => PathBuf::from(p),
        },
    }
}

/// Whether a path names a place by name alone: absolute, without `.` or `..`.
fn plain(p: &Path) -> bool {
    p.is_absolute() && p.components().all(|c| !matches!(c, Component::CurDir | Component::ParentDir))
}

/// Where a clone goes: absent, or an empty folder (nothing is ever cloned over files). True when
/// it is there already (empty).
fn room_for(path: &Path) -> anyhow::Result<bool> {
    anyhow::ensure!(plain(path), "a clone's folder is a whole path, not {}", path.display());
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => anyhow::bail!("{} is there already, and isn't a folder", path.display()),
        Ok(_) => {
            let empty = std::fs::read_dir(path).with_context(|| path.display().to_string())?.next().is_none();
            anyhow::ensure!(empty, "{} holds files already: kiln clones only into a new or empty folder", path.display());
            Ok(true)
        }
    }
}

/// Clones into `order.path` from `source` (a bundle's file, or the order's URL): the branch it
/// names checked out (else the source's), `origin` set as it says, and with `all_branches` a branch
/// here for every branch there. What it checked out: the branch (none: detached) and commit. A
/// clone that fails leaves the folder as it found it.
pub async fn clone_into(order: &CloneOrder, source: &OsStr, home: &Path) -> anyhow::Result<(Option<String>, Option<String>)> {
    let path = expand(&order.path, home);
    let existed = {
        let p = path.clone();
        crate::rt::blocking(move || room_for(&p)).await??
    };
    if let Some(parent) = path.parent() {
        let parent = parent.to_path_buf();
        crate::rt::blocking(move || std::fs::create_dir_all(parent)).await??;
    }
    let made = clone_steps(order, source, &path).await;
    if made.is_err() {
        let p = path.clone();
        let _ = crate::rt::blocking(move || {
            let _ = std::fs::remove_dir_all(&p);
            if existed {
                let _ = std::fs::create_dir(&p);
            }
        })
        .await;
    }
    made
}

async fn clone_steps(order: &CloneOrder, source: &OsStr, path: &Path) -> anyhow::Result<(Option<String>, Option<String>)> {
    let mut args: Vec<&OsStr> = vec![OsStr::new("clone"), OsStr::new("--quiet")];
    if let Some(b) = order.branch.as_deref().filter(|b| !b.is_empty()) {
        args.extend([OsStr::new("--branch"), OsStr::new(b)]);
    }
    args.extend([source, path.as_os_str()]);
    run(None, &args, LONG).await?;
    match order.origin.as_deref().filter(|o| !o.trim().is_empty()) {
        Some(o) => {
            run(Some(path), &["remote", "set-url", "origin", o.trim()], STATUS_TIME).await?;
        }
        None => {
            run(Some(path), &["remote", "remove", "origin"], STATUS_TIME).await?;
        }
    }
    if order.all_branches && order.origin.is_some() {
        let current = run(Some(path), &["symbolic-ref", "--short", "-q", "HEAD"], STATUS_TIME).await.unwrap_or_default();
        let refs = run(Some(path), &["for-each-ref", "--format=%(refname:short)", "refs/remotes/origin"], STATUS_TIME).await?;
        for r in refs.lines() {
            let Some(name) = r.strip_prefix("origin/") else { continue };
            if name == "HEAD" || name == current.trim() || name.is_empty() {
                continue;
            }
            run(Some(path), &["branch", "--quiet", "--track", name, r], STATUS_TIME).await?;
        }
    }
    let branch = run(Some(path), &["symbolic-ref", "--short", "-q", "HEAD"], STATUS_TIME).await.ok().map(|b| b.trim().to_owned()).filter(|b| !b.is_empty());
    let head = run(Some(path), &["rev-parse", "-q", "--verify", "HEAD"], STATUS_TIME).await.ok().map(|h| h.trim().to_owned()).filter(|h| !h.is_empty());
    Ok((branch, head))
}

/// A bundle of the repository at `path` in `out`: its branches, tags and `HEAD`, everything a
/// clone needs and no more (not its remote-tracking branches, not a mirror's pull requests).
pub async fn bundle(path: &Path, out: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(plain(path) && is_repo(path), "{} is no git repository", path.display());
    let made = run(Some(path), &[OsStr::new("bundle"), OsStr::new("create"), out.as_os_str(), OsStr::new("--branches"), OsStr::new("--tags"), OsStr::new("HEAD")], LONG).await;
    match made {
        Err(e) if format!("{e:#}").contains("empty bundle") => anyhow::bail!("{} has no commits to clone yet", path.display()),
        other => other.map(|_| ()),
    }
}

// ------------------------------------------------------------------------------ deleting

/// Whether `path` is somewhere a moved repository's folder can be deleted from: a real folder (no
/// symlink) at least two levels below the root, that is neither home nor holds it, nor holds or is
/// any of `keep` (kiln's own folders).
pub fn deletable(path: &Path, home: &Path, keep: &[PathBuf]) -> anyhow::Result<()> {
    anyhow::ensure!(plain(path), "{} is no whole path", path.display());
    let deep = path.components().filter(|c| matches!(c, Component::Normal(_))).count();
    anyhow::ensure!(deep >= 2, "{} is too near the root to delete", path.display());
    let m = std::fs::symlink_metadata(path).with_context(|| path.display().to_string())?;
    anyhow::ensure!(m.is_dir() && !m.file_type().is_symlink(), "{} isn't a folder", path.display());
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for k in std::iter::once(home).chain(keep.iter().map(PathBuf::as_path)) {
        let k = std::fs::canonicalize(k).unwrap_or_else(|_| k.to_path_buf());
        anyhow::ensure!(!k.starts_with(&real), "{} holds {}, which kiln keeps", path.display(), k.display());
    }
    Ok(())
}

/// Deletes the folder of a repository moved to another machine, once [`deletable`] allows it and,
/// given `origin`, the repository there has that remote: anything else is left alone.
pub async fn delete(path: &Path, origin: Option<&str>, home: &Path, keep: &[PathBuf]) -> anyhow::Result<()> {
    deletable(path, home, keep)?;
    if let Some(want) = origin.filter(|o| !o.is_empty()) {
        let have = run(Some(path), &["config", "--get", "remote.origin.url"], STATUS_TIME).await.map(|o| strip_credentials(&o)).unwrap_or_default();
        anyhow::ensure!(
            super::places::key_of(&have).is_some() && super::places::key_of(&have) == super::places::key_of(want),
            "{} is no longer the repository that moved (its remote is {})",
            path.display(),
            if have.is_empty() { "none" } else { have.as_str() }
        );
    }
    let p = path.to_path_buf();
    crate::rt::blocking(move || std::fs::remove_dir_all(&p)).await?.with_context(|| format!("deleting {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.email=k@k", "-c", "user.name=k", "-c", "init.defaultBranch=main"]).args(args).output()?;
        anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// A repository with two branches, a tag, a remote, an edit not committed.
    fn source(dir: &Path) -> anyhow::Result<PathBuf> {
        let src = dir.join("src");
        std::fs::create_dir_all(&src)?;
        sh(&src, &["init", "-q"])?;
        std::fs::write(src.join("a.txt"), "one\n")?;
        sh(&src, &["add", "a.txt"])?;
        sh(&src, &["commit", "-qm", "one"])?;
        sh(&src, &["tag", "v1"])?;
        sh(&src, &["checkout", "-qb", "feature"])?;
        std::fs::write(src.join("b.txt"), "two\n")?;
        sh(&src, &["add", "b.txt"])?;
        sh(&src, &["commit", "-qm", "two"])?;
        sh(&src, &["remote", "add", "origin", "https://user:secret@github.com/hyper-light/demo.git"])?;
        std::fs::write(src.join("a.txt"), "edited\n")?;
        Ok(src)
    }

    #[test]
    fn credentials_never_leave_a_url() {
        assert_eq!(strip_credentials("https://x-access-token:abc@github.com/a/b.git"), "https://github.com/a/b.git");
        assert_eq!(strip_credentials("https://tok@github.com/a/b"), "https://github.com/a/b");
        assert_eq!(strip_credentials("HTTP://u:p@h:8080/x"), "HTTP://h:8080/x");
        assert_eq!(strip_credentials("git@github.com:a/b.git"), "git@github.com:a/b.git");
        assert_eq!(strip_credentials("ssh://git@github.com/a/b"), "ssh://git@github.com/a/b");
        assert_eq!(strip_credentials("/srv/git/x.git"), "/srv/git/x.git");
        assert_eq!(strip_credentials("https://github.com"), "https://github.com");
    }

    #[test]
    fn a_status_says_where_a_repository_stands() {
        let out = "# branch.oid 0123abcd\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -3\n1 .M N... 100644 100644 100644 a a src/x.rs\n1 M. N... 100644 100644 100644 b b y.md\n";
        assert_eq!(
            parse_status(out),
            Status { head: Some("0123abcd".into()), branch: Some("main".into()), upstream: Some("origin/main".into()), ahead: 2, behind: 3, changed: 2 }
        );
        let fresh = parse_status("# branch.oid (initial)\n# branch.head (detached)\n");
        assert_eq!((fresh.head, fresh.branch, fresh.changed), (None, None, 0));
    }

    #[test]
    fn paths_and_the_room_a_clone_needs() -> anyhow::Result<()> {
        let home = Path::new("/home/ada");
        assert_eq!(expand("~/Projects/kiln", home), PathBuf::from("/home/ada/Projects/kiln"));
        assert_eq!(expand("~", home), PathBuf::from("/home/ada"));
        assert_eq!(expand("/opt/x", home), PathBuf::from("/opt/x"));
        let dir = tempfile::tempdir()?;
        assert!(!room_for(&dir.path().join("new"))?, "a new folder");
        std::fs::create_dir(dir.path().join("empty"))?;
        assert!(room_for(&dir.path().join("empty"))?, "an empty one");
        std::fs::create_dir(dir.path().join("full"))?;
        std::fs::write(dir.path().join("full/x"), "x")?;
        assert!(room_for(&dir.path().join("full")).is_err(), "never over files");
        assert!(room_for(Path::new("relative/x")).is_err());
        assert!(room_for(&dir.path().join("a/../b")).is_err());
        Ok(())
    }

    #[test]
    fn a_folder_is_deleted_only_where_that_is_safe() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path().join("home");
        let state = home.join(".local/share/kiln");
        let repo = home.join("Projects/kiln");
        std::fs::create_dir_all(&state)?;
        std::fs::create_dir_all(&repo)?;
        assert!(deletable(&repo, &home, std::slice::from_ref(&state)).is_ok());
        assert!(deletable(&home, &home, &[]).is_err(), "never home");
        assert!(deletable(&home.join(".local"), &home, std::slice::from_ref(&state)).is_err(), "never what holds kiln's own");
        assert!(deletable(Path::new("/"), &home, &[]).is_err());
        assert!(deletable(&home.join("Projects/none"), &home, &[]).is_err(), "nothing there");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&repo, home.join("link"))?;
            assert!(deletable(&home.join("link"), &home, &[]).is_err(), "never through a link");
        }
        Ok(())
    }

    #[test]
    fn repositories_are_found_where_people_keep_them() -> anyhow::Result<()> {
        if !have_git() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let home = dir.path().to_path_buf();
        for p in ["Projects/kiln", "Projects/hyper-light/raft", "dotfiles", "src/deep/er/x", "Projects/node_modules/pkg", "Projects/.hidden"] {
            std::fs::create_dir_all(home.join(p))?;
            sh(&home.join(p), &["init", "-q"])?;
        }
        std::fs::create_dir_all(home.join("Projects/kiln/sub"))?;
        sh(&home.join("Projects/kiln/sub"), &["init", "-q"])?;
        let elsewhere = dir.path().join("opt-build");
        std::fs::create_dir_all(&elsewhere)?;
        sh(&elsewhere, &["init", "-q"])?;
        let found = candidates(&home, std::slice::from_ref(&elsewhere));
        let names: Vec<String> = found.iter().map(|p| p.strip_prefix(dir.path()).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default()).collect();
        assert_eq!(names, ["opt-build", "dotfiles", "Projects/hyper-light/raft", "Projects/kiln"], "not too deep, not hidden, not dependencies, not inside another");
        Ok(())
    }

    #[test]
    fn a_clone_from_a_bundle_is_the_source_s_repository() -> anyhow::Result<()> {
        if !have_git() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let src = source(&root)?;
        crate::rt::block_on(async move {
            let file = root.join("b.bundle");
            bundle(&src, &file).await?;
            let order = CloneOrder {
                task: "t".into(),
                key: "github.com/hyper-light/demo".into(),
                path: root.join("dst").to_string_lossy().into_owned(),
                origin: Some("https://github.com/hyper-light/demo.git".into()),
                all_branches: true,
                ..CloneOrder::default()
            };
            let (branch, head) = clone_into(&order, file.as_os_str(), &root).await?;
            assert_eq!(branch.as_deref(), Some("feature"), "the branch the source has checked out");
            assert_eq!(head.as_deref(), Some(sh(&src, &["rev-parse", "HEAD"])?.as_str()));
            let dst = root.join("dst");
            assert_eq!(sh(&dst, &["remote", "get-url", "origin"])?, "https://github.com/hyper-light/demo.git", "its remote, never the bundle");
            assert!(sh(&dst, &["branch", "--format=%(refname:short)"])?.lines().any(|b| b == "main"), "every branch, for a move");
            assert_eq!(sh(&dst, &["tag"])?, "v1");
            assert_eq!(std::fs::read_to_string(dst.join("a.txt"))?, "one\n", "commits travel; an edit not committed does not");
            let f = found_at(&dst).await.ok_or_else(|| anyhow::anyhow!("not found"))?;
            assert_eq!((f.branch.as_deref(), f.origin.as_deref(), f.changed), (Some("feature"), Some("https://github.com/hyper-light/demo.git"), 0));
            let s = found_at(&src).await.ok_or_else(|| anyhow::anyhow!("not found"))?;
            assert_eq!(s.origin.as_deref(), Some("https://github.com/hyper-light/demo.git"), "no credentials");
            assert_eq!(s.changed, 1);
            // Never over files, and a failed clone leaves nothing behind.
            assert!(clone_into(&order, file.as_os_str(), &root).await.is_err());
            let bad = CloneOrder { path: root.join("bad").to_string_lossy().into_owned(), branch: Some("no-such".into()), ..order.clone() };
            assert!(clone_into(&bad, file.as_os_str(), &root).await.is_err());
            assert!(!root.join("bad").exists());
            // Deleting checks the remote first.
            let keep: Vec<PathBuf> = Vec::new();
            assert!(delete(&dst, Some("https://github.com/someone/else.git"), &root.join("home"), &keep).await.is_err());
            assert!(dst.exists());
            delete(&dst, Some("git@github.com:hyper-light/demo.git"), &root.join("home"), &keep).await?;
            assert!(!dst.exists());
            Ok(())
        })
    }
}
