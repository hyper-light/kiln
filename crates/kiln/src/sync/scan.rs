//! What a copy holds: each synced path's version, found by listing the folder (git's own view of
//! it in a git repo, so `.gitignore` means what git says it means) and hashing what changed since
//! the last look (size and modification time unchanged: the file is taken as unchanged).

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Version;
use super::blobs::hash_file_on;
use super::fs::Fs;

/// Never synced, in a folder that isn't a git repo (a git repo's own ignore rules decide there).
pub const DEFAULT_EXCLUDE: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__", ".DS_Store", ".kiln-sync"];

/// A temporary file kiln writes while applying a step: never synced.
pub const TEMP_PREFIX: &str = ".kiln-sync-";

/// How recent a file's time may be, at a look, for a later look to know the file by its size and
/// time: a file changed again within its filesystem's time step keeps both, so one this recent is
/// read again at the next look, as git reads its racily clean entries again. Over two seconds:
/// FAT keeps times to two, HFS+ to one, and Linux stamps them by a clock a tick behind.
pub const RACY_NS: i128 = 3_000_000_000;

/// What a scan keeps of a path between looks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub size: u64,
    pub mtime_ns: i128,
    pub version: Version,
    /// Changed so close to the look that saw it that a change since could have kept its size and
    /// time: read again at the next look, never known by them ([`RACY_NS`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub racy: bool,
}

/// What a copy syncs and how much.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Bigger files are skipped.
    pub max_size: u64,
    /// Names or `/`-separated path prefixes left out, besides the defaults and git's rules.
    pub exclude: Vec<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_size: 256 << 20, exclude: Vec::new() }
    }
}

/// A path a scan left out, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub why: String,
}

/// Each synced path's entry (hashing only what changed since `seen`), and what was left out.
pub fn scan(fs: &dyn Fs, root: &Path, seen: &BTreeMap<String, Entry>, limits: &Limits) -> io::Result<(BTreeMap<String, Entry>, Vec<Skipped>)> {
    let mut found = BTreeMap::new();
    let mut skipped = Vec::new();
    // Before anything is read: a file changed after it is changed after the look.
    let started = fs.now_ns();
    for path in list(fs, root, limits)? {
        let full = root.join(&path);
        let Ok(meta) = fs.meta(&full) else { continue };
        let mtime_ns = meta.mtime_ns;
        if meta.is_symlink() {
            let target = fs.read_link(&full)?.to_string_lossy().replace('\\', "/");
            found.insert(path, Entry { size: 0, mtime_ns, version: Version::Link { target }, racy: false });
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        if meta.len > limits.max_size {
            skipped.push(Skipped { why: format!("{} bytes, over the {} byte limit", meta.len, limits.max_size), path });
            continue;
        }
        let exec = meta.exec;
        let version = match seen.get(&path) {
            Some(e) if !e.racy && e.size == meta.len && e.mtime_ns == mtime_ns && matches!(&e.version, Version::File { exec: x, .. } if *x == exec) => e.version.clone(),
            _ => match hash_file_on(fs, &full) {
                Ok(hash) => Version::File { hash, exec },
                // Gone or unreadable between listing and reading: next look.
                Err(_) => continue,
            },
        };
        let racy = mtime_ns.saturating_add(RACY_NS) > started;
        found.insert(path, Entry { size: meta.len, mtime_ns, version, racy });
    }
    Ok((found, skipped))
}

#[cfg(unix)]
pub(crate) fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(crate) fn executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// The paths to sync: git's list (tracked, and untracked but not ignored) in a git repo, else a
/// walk that leaves out [`DEFAULT_EXCLUDE`]. Never `.git/`, kiln's own temporary files, or
/// `limits.exclude`.
pub fn list(fs: &dyn Fs, root: &Path, limits: &Limits) -> io::Result<Vec<String>> {
    let paths = if fs.exists(&root.join(".git")) { git_list(root)? } else { walk(fs, root)? };
    Ok(paths.into_iter().filter(|p| keep(p, limits)).collect())
}

fn keep(path: &str, limits: &Limits) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    !(path == ".git" || path.starts_with(".git/") || name.starts_with(TEMP_PREFIX))
        && !limits.exclude.iter().any(|x| path == x || path.starts_with(&format!("{x}/")) || path.split('/').any(|part| part == x))
}

fn git_list(root: &Path) -> io::Result<Vec<String>> {
    let out = std::process::Command::new("git").arg("-C").arg(root).args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!("git ls-files: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let mut paths: Vec<String> = out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Every file under `root`, breadth first, leaving out [`DEFAULT_EXCLUDE`] at any depth.
fn walk(fs: &dyn Fs, root: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut dirs = VecDeque::from([String::new()]);
    while let Some(rel) = dirs.pop_front() {
        let dir = if rel.is_empty() { root.to_owned() } else { root.join(&rel) };
        for (name, kind) in fs.read_dir(&dir)? {
            if DEFAULT_EXCLUDE.contains(&name.as_str()) {
                continue;
            }
            let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if kind == super::fs::Kind::Dir {
                dirs.push_back(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::fs::Std;
    use super::*;

    fn write(root: &Path, path: &str, body: &str) -> io::Result<()> {
        let p = root.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, body)
    }

    #[test]
    fn a_folder_is_listed_without_what_never_syncs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        for (p, b) in [("a.txt", "a"), ("src/lib.rs", "fn x() {}"), ("node_modules/m/i.js", "x"), ("target/debug/k", "bin"), (".kiln-sync-1.tmp", "t"), ("docs/notes.md", "n")] {
            write(root, p, b)?;
        }
        let limits = Limits { exclude: vec!["docs".into()], ..Limits::default() };
        assert_eq!(list(&Std, root, &limits)?, ["a.txt", "src/lib.rs"]);
        let (found, _) = scan(&Std, root, &BTreeMap::new(), &Limits { max_size: 3, ..Limits::default() })?;
        assert!(matches!(found.get("a.txt").map(|e| &e.version), Some(Version::File { exec: false, .. })));
        Ok(())
    }

    #[test]
    fn a_git_repo_is_listed_as_git_sees_it() -> anyhow::Result<()> {
        if crate::os::which("git").is_none() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(root).args(args).output();
        git(&["init", "-q"])?;
        for (p, b) in [(".gitignore", "*.log\nbuild/\n"), ("main.rs", "fn main() {}"), ("run.log", "x"), ("build/out", "o"), ("new.txt", "untracked")] {
            write(root, p, b)?;
        }
        git(&["add", "main.rs", ".gitignore"])?;
        assert_eq!(list(&Std, root, &Limits::default())?, [".gitignore", "main.rs", "new.txt"], "tracked and untracked, never ignored or .git");
        Ok(())
    }

    /// Sets `path`'s modification time to `secs_ago` seconds before now.
    fn aged(path: &Path, secs_ago: u64) -> io::Result<()> {
        let t = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(secs_ago)).unwrap_or(std::time::UNIX_EPOCH);
        std::fs::File::options().write(true).open(path)?.set_modified(t)
    }

    #[test]
    fn only_what_changed_is_hashed_again() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        write(dir.path(), "f", "one")?;
        // Changed long before the look: it's known by its size and time after.
        aged(&dir.path().join("f"), 3600)?;
        let (first, _) = scan(&Std, dir.path(), &BTreeMap::new(), &Limits::default())?;
        // An entry that says otherwise for the same size and time is believed: nothing was read.
        let mut seen = first.clone();
        if let Some(e) = seen.get_mut("f") {
            e.version = Version::File { hash: "cached".into(), exec: false };
        }
        let (again, _) = scan(&Std, dir.path(), &seen, &Limits::default())?;
        assert!(matches!(again.get("f").map(|e| &e.version), Some(Version::File { hash, .. }) if hash == "cached"));
        let (big, skipped) = scan(&Std, dir.path(), &BTreeMap::new(), &Limits { max_size: 2, ..Limits::default() })?;
        assert!(big.is_empty() && skipped.first().is_some_and(|s| s.path == "f"));
        Ok(())
    }

    /// CP-T6.1, the third bug kiln-sim found: a file changed again just after a look, keeping its
    /// size and (within its filesystem's time step) its time, is read again at the next look, not
    /// taken for what the look saw; once its time is long past, it's known by them.
    #[test]
    fn a_change_that_keeps_a_file_s_size_and_time_just_after_a_look_is_still_seen() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let f = dir.path().join("f");
        write(dir.path(), "f", "one")?;
        let t = std::fs::metadata(&f)?.modified()?;
        let (first, _) = scan(&Std, dir.path(), &BTreeMap::new(), &Limits::default())?;
        assert!(first.get("f").is_some_and(|e| e.racy), "changed just now: read again next time");
        // Changed again within the same time step: the same size and time.
        write(dir.path(), "f", "two")?;
        std::fs::File::options().write(true).open(&f)?.set_modified(t)?;
        let (again, _) = scan(&Std, dir.path(), &first, &Limits::default())?;
        let (now, before) = (Version::File { hash: hash_file_on(&Std, &f)?, exec: false }, first.get("f").map(|e| e.version.clone()));
        assert!(again.get("f").is_some_and(|e| e.version == now) && Some(now) != before, "the second change was seen");
        // Long since: known by its size and time.
        aged(&f, 3600)?;
        let (old, _) = scan(&Std, dir.path(), &again, &Limits::default())?;
        assert!(old.get("f").is_some_and(|e| !e.racy));
        Ok(())
    }
}
