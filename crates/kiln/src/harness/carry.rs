//! Files a moving session's repository holds that sync doesn't carry, carried in its package
//! (`super::moving`): the ones git ignores, as `.env`, local settings, keys and a local database
//! are, but not what a build makes again (`node_modules`, `target`, `.venv`, `dist`, ...). Each
//! keeps its permissions (a key stays `0600`). On the machine it goes to, one that isn't there is
//! written; one that is there and differs stays, and the one that came goes beside it
//! (`<name>.from-<machine>`), which git is told to ignore there (`info/exclude`), so a secret
//! beside one is never committed nor synced: nothing is overwritten. More than a move carries, and
//! it doesn't move, rather than leave them behind.
//!
//! Blocking: it reads and writes files.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// The most one file, and all of them, may be.
const MAX_FILE: u64 = 16 << 20;
const MAX_TOTAL: u64 = 64 << 20;
/// Folders a build or a tool makes again: never carried, and never a reason not to move.
const REBUILT: &[&str] = &[
    ".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".kiln-sync", "dist", "build", "out", ".next", ".nuxt", ".svelte-kit", ".turbo", ".cache", ".parcel-cache",
    ".gradle", ".tox", ".nox", ".mypy_cache", ".pytest_cache", ".ruff_cache", "coverage", "htmlcov", ".terraform",
];
/// Files that are the machine's own and mean nothing elsewhere.
const LOCAL: &[&str] = &[".DS_Store", "Thumbs.db"];

/// Files carried: each one's path in the repository (`/`-separated), permissions and contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Carried {
    pub files: Vec<CarriedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedFile {
    pub path: String,
    pub mode: u32,
    /// Base64.
    pub data: String,
}

fn rebuilt(path: &str) -> bool {
    path.split('/').any(|part| REBUILT.contains(&part))
}

/// What git ignores in the repository at `root` (the paths it gives; a folder ignored whole ends
/// with `/`).
fn ignored_paths(root: &Path) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git ls-files: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
}

/// Every file under `dir` (relative to `root`), leaving out what a build makes again.
fn walk(root: &Path, dir: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if dir.is_empty() { name.clone() } else { format!("{}/{name}", dir.trim_end_matches('/')) };
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() {
            // A repository of its own, or another worktree, inside an ignored folder: not this
            // one's files.
            if !REBUILT.contains(&name.as_str()) && !e.path().join(".git").exists() {
                walk(root, &rel, out);
            }
        } else if t.is_file() && !LOCAL.contains(&name.as_str()) {
            out.push(rel);
        }
    }
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

/// The files git ignores in the repository at `root`, but those a build makes again. An error
/// names what's too big to carry: it would be left behind.
pub fn ignored(root: &Path) -> Result<Carried, String> {
    let mut paths = Vec::new();
    for p in ignored_paths(root)? {
        if rebuilt(&p) {
            continue;
        }
        if p.ends_with('/') {
            if root.join(&p).join(".git").exists() {
                continue;
            }
            walk(root, p.trim_end_matches('/'), &mut paths);
        } else if !LOCAL.iter().any(|l| p.rsplit('/').next() == Some(*l)) {
            paths.push(p);
        }
    }
    paths.sort();
    paths.dedup();
    let mut sizes: Vec<(String, u64)> = Vec::new();
    for p in &paths {
        let Ok(meta) = std::fs::symlink_metadata(root.join(p)) else { continue };
        if meta.is_file() {
            sizes.push((p.clone(), meta.len()));
        }
    }
    let total: u64 = sizes.iter().map(|(_, s)| *s).sum();
    let mut big: Vec<&(String, u64)> = sizes.iter().filter(|(_, s)| *s > MAX_FILE).collect();
    if !big.is_empty() || total > MAX_TOTAL {
        if big.is_empty() {
            big = sizes.iter().collect();
        }
        big.sort_by_key(|x| std::cmp::Reverse(x.1));
        let names: Vec<String> = big.iter().take(5).map(|(p, s)| format!("{p} ({} MB)", s.div_ceil(1 << 20))).collect();
        return Err(format!(
            "the files git ignores in {} come to {} MB, more than a move carries ({} MB, {} MB a file), so some would be left behind: {}. Move them out of the repository, or delete what can be made again, and it moves",
            root.display(),
            total.div_ceil(1 << 20),
            MAX_TOTAL >> 20,
            MAX_FILE >> 20,
            names.join(", ")
        ));
    }
    let mut files = Vec::new();
    for (p, _) in sizes {
        let full = root.join(&p);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        let mut bytes = Vec::new();
        std::fs::File::open(&full).and_then(|mut f| f.read_to_end(&mut bytes)).map_err(|e| format!("{p}: {e}"))?;
        files.push(CarriedFile { path: p, mode: mode_of(&meta), data: base64::engine::general_purpose::STANDARD.encode(bytes) });
    }
    Ok(Carried { files })
}

/// `rel` under `root`, when it is a plain relative path that stays inside it.
fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    p.components().all(|c| matches!(c, Component::Normal(_))).then(|| root.join(p))
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(".{}.kiln-move", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())));
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path)
}

/// Has git ignore `rel` in the repository at `root`, there alone: its `info/exclude`.
fn exclude(root: &Path, rel: &str) -> std::io::Result<()> {
    let out = Command::new("git").arg("-C").arg(root).args(["rev-parse", "--git-path", "info/exclude"]).output()?;
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let path = if path.is_absolute() { path } else { root.join(path) };
    let line = format!("/{rel}");
    let had = std::fs::read_to_string(&path).unwrap_or_default();
    if had.lines().any(|l| l == line) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let sep = if had.is_empty() || had.ends_with('\n') { "" } else { "\n" };
    std::fs::write(&path, format!("{had}{sep}{line}\n"))
}

/// Puts `c` (from machine `from`) into the repository at `root`, overwriting nothing. What it
/// did, in words, for the session's timeline.
pub fn place(root: &Path, c: &Carried, from: &str) -> Vec<String> {
    let mut came = Vec::new();
    let mut beside = Vec::new();
    let mut failed = Vec::new();
    for f in &c.files {
        let Some(path) = inside(root, &f.path) else {
            failed.push(format!("{} (not a path inside the repository)", f.path));
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&f.data) else {
            failed.push(format!("{} (unreadable)", f.path));
            continue;
        };
        match std::fs::read(&path) {
            Ok(here) if here == bytes => {}
            Ok(_) => {
                // This machine's stays; the one that came goes beside it, never over another.
                let mut side = PathBuf::from(format!("{}.from-{from}", path.display()));
                let mut n = 1;
                while std::fs::read(&side).is_ok_and(|b| b != bytes) {
                    n += 1;
                    side = PathBuf::from(format!("{}.from-{from}-{n}", path.display()));
                }
                let rel = side.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                match exclude(root, &rel).and_then(|()| write_file(&side, &bytes, f.mode)) {
                    Ok(()) => beside.push(format!("{} (beside it: {})", f.path, side.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()))),
                    Err(e) => failed.push(format!("{} ({e})", f.path)),
                }
            }
            Err(_) => match write_file(&path, &bytes, f.mode) {
                Ok(()) => came.push(f.path.clone()),
                Err(e) => failed.push(format!("{} ({e})", f.path)),
            },
        }
    }
    let list = |v: &[String]| {
        let shown: Vec<&str> = v.iter().take(6).map(String::as_str).collect();
        format!("{}{}", shown.join(", "), if v.len() > 6 { format!(" and {} more", v.len() - 6) } else { String::new() })
    };
    let mut said = Vec::new();
    if !came.is_empty() {
        said.push(format!("{} file{} git ignores came from {from}: {}", came.len(), if came.len() == 1 { "" } else { "s" }, list(&came)));
    }
    if !beside.is_empty() {
        said.push(format!("{} differ{} here, so this machine's stay{}: {}", beside.len(), if beside.len() == 1 { "s" } else { "" }, if beside.len() == 1 { "s" } else { "" }, list(&beside)));
    }
    if !failed.is_empty() {
        said.push(format!("These couldn't be written here, and stay on {from}: {}", list(&failed)));
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
        let out = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output()?;
        anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(())
    }

    fn put(dir: &Path, rel: &str, text: &str) -> anyhow::Result<()> {
        let p = dir.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, text)?;
        Ok(())
    }

    /// A repository's ignored files: its `.env`, a key, a log, a local database come; what a
    /// build makes again doesn't.
    #[test]
    fn what_git_ignores_comes_but_what_builds_again() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let a = t.path().join("a");
        std::fs::create_dir_all(&a)?;
        git(&a, &["init", "-q"])?;
        put(&a, ".gitignore", ".env\nsecrets/\n*.log\nnode_modules/\ntarget/\nlocal.db\n.DS_Store\n")?;
        put(&a, "src/main.rs", "fn main() {}")?;
        git(&a, &["add", "."])?;
        git(&a, &["commit", "-qm", "first"])?;
        put(&a, ".env", "TOKEN=secret\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(a.join(".env"), std::fs::Permissions::from_mode(0o600))?;
        }
        put(&a, "secrets/key.pem", "KEY")?;
        put(&a, "app.log", "log")?;
        put(&a, "local.db", "db")?;
        put(&a, ".DS_Store", "x")?;
        put(&a, "node_modules/x/index.js", "x")?;
        put(&a, "target/debug/kiln", "bin")?;
        // Another worktree, in an ignored folder: its files are its own.
        put(&a, ".gitignore", ".env\nsecrets/\n*.log\nnode_modules/\ntarget/\nlocal.db\n.DS_Store\n.trees/\n")?;
        git(&a, &["add", ".gitignore"])?;
        git(&a, &["commit", "-qm", "trees"])?;
        git(&a, &["worktree", "add", "-q", ".trees/other", "-b", "other"])?;
        put(&a, ".trees/other/theirs.txt", "not ours")?;
        let c = ignored(&a).map_err(anyhow::Error::msg)?;
        let paths: Vec<&str> = c.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, [".env", "app.log", "local.db", "secrets/key.pem"]);
        // On another machine: written where missing, its permissions kept.
        let b = t.path().join("b");
        std::fs::create_dir_all(&b)?;
        git(&b, &["init", "-q"])?;
        put(&b, ".gitignore", ".env\nsecrets/\n*.log\nnode_modules/\ntarget/\nlocal.db\n.DS_Store\n")?;
        put(&b, "local.db", "b's own")?;
        let said = place(&b, &c, "lab-a");
        assert_eq!(std::fs::read_to_string(b.join(".env"))?, "TOKEN=secret\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(b.join(".env"))?.permissions().mode() & 0o777, 0o600, "a secret stays private");
        }
        assert_eq!(std::fs::read_to_string(b.join("secrets/key.pem"))?, "KEY");
        // One that differs there: b's stays, a's beside it.
        assert_eq!(std::fs::read_to_string(b.join("local.db"))?, "b's own");
        assert_eq!(std::fs::read_to_string(b.join("local.db.from-lab-a"))?, "db");
        assert!(said.iter().any(|s| s.starts_with("3 files git ignores came from lab-a")), "{said:?}");
        assert!(said.iter().any(|s| s.contains("local.db.from-lab-a")), "{said:?}");
        // The one beside is b's to keep: git there ignores it, so it's never committed nor synced.
        let st = Command::new("git").arg("-C").arg(&b).args(["status", "--porcelain", "--ignored"]).output()?;
        let st = String::from_utf8_lossy(&st.stdout).into_owned();
        assert!(st.contains("!! local.db.from-lab-a"), "{st}");
        // Placed again: nothing new, nothing overwritten.
        let again = place(&b, &c, "lab-a");
        assert!(again.iter().all(|s| !s.contains("came from")), "{again:?}");
        Ok(())
    }

    /// More than a move carries holds it, naming what would be left behind.
    #[test]
    fn too_much_holds_it() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let a = t.path().join("a");
        std::fs::create_dir_all(&a)?;
        git(&a, &["init", "-q"])?;
        put(&a, ".gitignore", "*.bin\n")?;
        let big = vec![0u8; usize::try_from(MAX_FILE + 1)?];
        std::fs::write(a.join("model.bin"), big)?;
        assert!(ignored(&a).is_err_and(|e| e.contains("model.bin (17 MB)") && e.contains("left behind")));
        Ok(())
    }

    /// A package can't write outside the repository.
    #[test]
    fn nothing_goes_outside() {
        let root = Path::new("/r");
        assert!(inside(root, "../etc/passwd").is_none());
        assert!(inside(root, "/etc/passwd").is_none());
        assert!(inside(root, "a/./b").is_some_and(|p| p == Path::new("/r/a/b")));
    }
}
