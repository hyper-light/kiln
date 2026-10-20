//! Copying a working tree to another runner before running there (`docs/reach.md` §5).
//!
//! The asking side lists its tree ([`scan`]): a git repository's files as git sees them (tracked,
//! and untracked but not ignored — so `target/` and `node_modules/` stay home) plus `.git` itself,
//! so git works over there too (a worktree's `.git` becomes a repository of its own: the shared
//! objects and refs, with the worktree's HEAD and index); any other folder, everything but the
//! usual build output. The other
//! side keeps a copy per tree and runner (`~/.kiln/reach/<runner>/<key>`), asks for the files whose
//! size or modification time differ ([`need`]), writes them ([`Mirror::write`]) with the asker's
//! times, and on commit makes the copy match the list exactly ([`Mirror::commit`]). A second sync
//! of an unchanged tree sends the list and nothing else.
//!
//! Nothing a list says can reach outside the copy: paths are relative and plain, and no write
//! goes through a symlink.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Component, Path, PathBuf};

use anyhow::Context as _;

use super::wire::{Entry, EntryKind};

/// A tree may hold at most this many entries.
pub const MAX_ENTRIES: usize = 500_000;
/// Bytes of one file piece.
pub const PIECE: usize = 256 * 1024;
/// Folders left out of a tree that is not a git repository (build output, dependencies).
const SKIP_DIRS: &[&str] = &["node_modules", "target", ".venv", "venv", "__pycache__", ".tox", ".gradle", ".next", "dist-newstyle"];

/// The tree a folder belongs to: its git repository's top, else the folder itself; and where the
/// folder is inside it.
pub fn find_root(cwd: &Path) -> (PathBuf, PathBuf) {
    let top = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim_end_matches(['\n', '\r'])))
        .filter(|p| p.is_dir());
    match top {
        Some(top) => {
            // git answers with symlinks resolved; so must the folder be.
            let real = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            let rel = real.strip_prefix(&top).map(Path::to_path_buf).unwrap_or_default();
            (top, rel)
        }
        None => (cwd.to_path_buf(), PathBuf::new()),
    }
}

/// The name a tree's copy has on other runners: its folder's name and a hash of where it is
/// here, so two trees named alike never share a copy.
pub fn mirror_key(me: &str, root: &Path) -> String {
    let base: String = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into()).chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).take(48).collect();
    let hash = crate::util::sha256_hex(format!("{me}\0{}", root.display()).as_bytes());
    format!("{}-{}", base.trim_start_matches('.'), hash.get(..10).unwrap_or("0"))
}

fn mtime_ns(md: &std::fs::Metadata) -> i64 {
    match md.modified() {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
            Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
        },
        Err(_) => 0,
    }
}

fn mode(md: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        md.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        0
    }
}

/// `rel` with `/` separators, or None when it is not a plain relative path.
fn slash(rel: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_owned()),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The entry for `rel` (relative to `root`), or None for what a tree does not carry (sockets,
/// pipes, git's lock files) or what vanished meanwhile.
fn entry(root: &Path, rel: &Path) -> Option<Entry> {
    let path = slash(rel)?;
    if path.ends_with(".lock") && path.starts_with(".git/") {
        return None;
    }
    let md = std::fs::symlink_metadata(root.join(rel)).ok()?;
    let ft = md.file_type();
    let (kind, size, target) = if ft.is_symlink() {
        (EntryKind::Symlink, 0, Some(std::fs::read_link(root.join(rel)).ok()?.to_string_lossy().into_owned()))
    } else if ft.is_dir() {
        (EntryKind::Dir, 0, None)
    } else if ft.is_file() {
        (EntryKind::File, md.len(), None)
    } else {
        return None;
    };
    Some(Entry { path, kind, size, mtime_ns: mtime_ns(&md), mode: mode(&md), target })
}

/// Every file and folder under `dir` (relative to `root`), not following symlinks; `skip` is
/// asked about each folder's path in the tree.
fn walk(root: &Path, dir: &Path, skip: &dyn Fn(&str) -> bool, out: &mut Vec<Entry>) -> anyhow::Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(read) = std::fs::read_dir(root.join(&d)) else { continue };
        for e in read.flatten() {
            let rel = d.join(e.file_name());
            let Some(en) = entry(root, &rel) else { continue };
            if en.kind == EntryKind::Dir {
                if skip(&en.path) {
                    continue;
                }
                stack.push(rel);
            }
            out.push(en);
            anyhow::ensure!(out.len() <= MAX_ENTRIES, "{} holds more than {MAX_ENTRIES} files; sync a smaller folder", root.display());
        }
    }
    Ok(())
}

/// A tree as listed: its entries, and where on this machine each file's bytes are (its place in
/// the tree, except for a worktree's git data).
pub struct Tree {
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    sources: HashMap<String, PathBuf>,
}

impl Tree {
    /// Where the bytes of `path` are read from.
    pub fn source(&self, path: &str) -> PathBuf {
        self.sources.get(path).cloned().unwrap_or_else(|| self.root.join(path))
    }
}

/// `git rev-parse <what>` in `root`, as an absolute path.
fn git_path(root: &Path, what: &str) -> Option<PathBuf> {
    let o = std::process::Command::new("git").arg("-C").arg(root).args(["rev-parse", what]).stderr(std::process::Stdio::null()).output().ok().filter(|o| o.status.success())?;
    let p = PathBuf::from(String::from_utf8(o.stdout).ok()?.trim_end_matches(['\n', '\r']));
    let p = if p.is_relative() { root.join(p) } else { p };
    p.is_dir().then_some(p)
}

/// A worktree's git data as one repository's `.git`: the shared part (objects, refs, config; not
/// the other worktrees), then the worktree's own (HEAD, index, logs) over it.
fn worktree_git(root: &Path, out: &mut Vec<Entry>, sources: &mut HashMap<String, PathBuf>) -> anyhow::Result<bool> {
    let (Some(own), Some(common)) = (git_path(root, "--absolute-git-dir"), git_path(root, "--git-common-dir")) else { return Ok(false) };
    if own == common {
        return Ok(false);
    }
    let mut layer = |dir: &Path, skip: &dyn Fn(&str) -> bool| -> anyhow::Result<()> {
        let mut found = Vec::new();
        walk(dir, Path::new(""), &|p| p == "worktrees", &mut found)?;
        for mut e in found {
            if skip(&e.path) {
                continue;
            }
            let src = dir.join(&e.path);
            e.path = format!(".git/{}", e.path);
            sources.insert(e.path.clone(), src);
            out.retain(|x| x.path != e.path);
            out.push(e);
        }
        Ok(())
    };
    layer(&common, &|p| p.ends_with(".lock"))?;
    layer(&own, &|p| matches!(p, "gitdir" | "commondir" | "locked") || p.ends_with(".lock"))?;
    if let Some(d) = entry(root, Path::new(".git")) {
        // `.git` is a folder over there.
        out.retain(|x| x.path != ".git");
        out.push(Entry { kind: EntryKind::Dir, size: 0, target: None, ..d });
    }
    Ok(true)
}

/// The tree at `root`, as the other side should have it.
pub fn scan(root: &Path) -> anyhow::Result<Tree> {
    let mut out = Vec::new();
    let mut sources = HashMap::new();
    let git = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success() && root.join(".git").exists());
    match git {
        Some(o) => {
            let mut dirs: HashSet<String> = HashSet::new();
            for name in o.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
                let rel = PathBuf::from(String::from_utf8_lossy(name).into_owned());
                // Listed but deleted (or a submodule's folder): carried as it is on disk, if at all.
                let Some(en) = entry(root, &rel) else { continue };
                let mut parent = rel.parent();
                while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
                    let Some(s) = slash(p) else { break };
                    if !dirs.insert(s) {
                        break;
                    }
                    if let Some(d) = entry(root, p) {
                        out.push(d);
                    }
                    parent = p.parent();
                }
                out.push(en);
                anyhow::ensure!(out.len() <= MAX_ENTRIES, "{} holds more than {MAX_ENTRIES} files; sync a smaller folder", root.display());
            }
            // git itself, so `git status`, `git log` and `git diff` work over there.
            if let Some(d) = entry(root, Path::new(".git")) {
                if d.kind == EntryKind::Dir {
                    out.push(d);
                    walk(root, Path::new(".git"), &|_| false, &mut out)?;
                } else if !worktree_git(root, &mut out, &mut sources)? {
                    // A `.git` file git can't resolve (a submodule's elsewhere): carried as is.
                    out.push(d);
                }
            }
        }
        None => walk(root, Path::new(""), &|p| SKIP_DIRS.contains(&p.rsplit('/').next().unwrap_or(p)), &mut out)?,
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(Tree { root: root.to_path_buf(), entries: out, sources })
}

/// A piece of a file of the tree (read from `source`), for the other side.
pub fn read_piece(source: &Path, offset: u64) -> anyhow::Result<(Vec<u8>, bool)> {
    let full = source.to_path_buf();
    let mut f = std::fs::File::open(&full).with_context(|| format!("open {}", full.display()))?;
    let len = f.metadata()?.len();
    f.seek(std::io::SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; PIECE];
    let mut n = 0;
    while n < buf.len() {
        match f.read(buf.get_mut(n..).unwrap_or_default())? {
            0 => break,
            k => n += k,
        }
    }
    buf.truncate(n);
    let last = offset + n as u64 >= len || n == 0;
    Ok((buf, last))
}

/// A list's path, checked: relative, plain (no `..`, no root, no prefix), `/`-separated.
pub fn safe_rel(path: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(!path.is_empty() && !path.contains('\0') && !path.contains('\\'), "bad path {path:?}");
    let p = Path::new(path);
    anyhow::ensure!(p.components().all(|c| matches!(c, Component::Normal(_))), "path {path:?} leaves the tree");
    Ok(p.to_path_buf())
}

/// Every folder between `root` and `rel`'s parent exists as a real folder (made if missing);
/// none is a symlink, so a write can't leave the copy.
fn real_parents(root: &Path, rel: &Path) -> anyhow::Result<()> {
    let mut at = root.to_path_buf();
    let comps: Vec<Component<'_>> = rel.components().collect();
    let parents = comps.len().saturating_sub(1);
    for c in comps.into_iter().take(parents) {
        at.push(c);
        match std::fs::symlink_metadata(&at) {
            Ok(md) if md.file_type().is_dir() => {}
            Ok(md) if md.file_type().is_symlink() => anyhow::bail!("{} is a symlink; nothing is written through it", at.display()),
            Ok(_) => {
                std::fs::remove_file(&at)?;
                std::fs::create_dir(&at)?;
            }
            Err(_) => std::fs::create_dir(&at).with_context(|| format!("create {}", at.display()))?,
        }
    }
    Ok(())
}

/// The files of `entries` the copy at `root` lacks or has otherwise.
pub fn need(root: &Path, entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .filter(|e| {
            let Ok(rel) = safe_rel(&e.path) else { return false };
            match std::fs::symlink_metadata(root.join(rel)) {
                Ok(md) => !(md.file_type().is_file() && md.len() == e.size && mtime_ns(&md) == e.mtime_ns),
                Err(_) => true,
            }
        })
        .map(|e| e.path.clone())
        .collect()
}

fn mtime_of(e: &Entry) -> std::time::SystemTime {
    if e.mtime_ns >= 0 {
        std::time::UNIX_EPOCH + std::time::Duration::from_nanos(u64::try_from(e.mtime_ns).unwrap_or(0))
    } else {
        std::time::UNIX_EPOCH
    }
}

/// The entry's permission bits (after its time is set: git's objects are read-only).
fn set_mode(path: &Path, e: &Entry) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if e.mode != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(e.mode & 0o7777))?;
        }
    }
    #[cfg(not(unix))]
    let _ = (path, e);
    Ok(())
}

/// One copy being brought up to date on this side.
pub struct Mirror {
    pub root: PathBuf,
    plan: Vec<Entry>,
    /// Files being written: their temporary file, where it is, and how many bytes arrived.
    open: HashMap<String, (std::fs::File, PathBuf, u64)>,
    pub written: u32,
    pub bytes: u64,
}

impl Mirror {
    /// The copy of `key` from runner `from`, under `base` (`~/.kiln/reach` on a runner).
    pub fn root_for(base: &Path, from: &str, key: &str) -> anyhow::Result<PathBuf> {
        let from = safe_rel(from)?;
        let key = safe_rel(key)?;
        anyhow::ensure!(from.components().count() == 1 && key.components().count() == 1, "bad copy name");
        Ok(base.join(from).join(key))
    }

    pub fn new(root: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
        Ok(Self { root, plan: Vec::new(), open: HashMap::new(), written: 0, bytes: 0 })
    }

    /// More of the list.
    pub fn plan(&mut self, entries: Vec<Entry>) -> anyhow::Result<()> {
        anyhow::ensure!(self.plan.len() + entries.len() <= MAX_ENTRIES, "the tree holds more than {MAX_ENTRIES} files");
        for e in &entries {
            safe_rel(&e.path)?;
        }
        self.plan.extend(entries);
        Ok(())
    }

    pub fn entries(&self) -> &[Entry] {
        &self.plan
    }

    /// A piece of a listed file.
    pub fn write(&mut self, path: &str, offset: u64, data: &[u8], last: bool) -> anyhow::Result<()> {
        let rel = safe_rel(path)?;
        let entry = self.plan.iter().find(|e| e.path == path && e.kind == EntryKind::File).cloned().ok_or_else(|| anyhow::anyhow!("{path} is not a file of the list"))?;
        if !self.open.contains_key(path) {
            real_parents(&self.root, &rel)?;
            let tmp = self.root.join(format!("{}.kiln-part", rel.display()));
            let f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            self.open.insert(path.to_owned(), (f, tmp, 0));
        }
        let Some((f, _, got)) = self.open.get_mut(path) else { anyhow::bail!("{path} is not open") };
        f.seek(std::io::SeekFrom::Start(offset))?;
        f.write_all(data)?;
        *got = (*got).max(offset + data.len() as u64);
        self.bytes += data.len() as u64;
        if last {
            let Some((f, tmp, got)) = self.open.remove(path) else { anyhow::bail!("{path} is not open") };
            f.set_len(got)?;
            // A file that changed or vanished while it was sent is kept as it arrived, dated so the
            // next sync sends it again.
            f.set_modified(if got == entry.size { mtime_of(&entry) } else { std::time::UNIX_EPOCH })?;
            drop(f);
            let target = self.root.join(&rel);
            if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_dir()) {
                std::fs::remove_dir_all(&target)?;
            }
            std::fs::rename(&tmp, &target).with_context(|| format!("place {}", target.display()))?;
            set_mode(&target, &entry)?;
            self.written += 1;
        }
        Ok(())
    }

    /// Make the copy match the list: folders and symlinks as listed, and whatever is not listed
    /// removed. Returns how many entries were removed.
    pub fn commit(&mut self) -> anyhow::Result<u32> {
        for (_, (_, tmp, _)) in self.open.drain() {
            let _ = std::fs::remove_file(tmp);
        }
        let listed: HashSet<&str> = self.plan.iter().map(|e| e.path.as_str()).collect();
        // What is not listed goes first, deepest first, never following a symlink.
        let mut found = Vec::new();
        walk(&self.root, Path::new(""), &|_| false, &mut found)?;
        found.sort_by(|a, b| b.path.len().cmp(&a.path.len()).then_with(|| b.path.cmp(&a.path)));
        let mut removed = 0;
        for f in found {
            let keep = listed.contains(f.path.as_str()) && self.plan.iter().any(|e| e.path == f.path && e.kind == f.kind);
            if keep {
                continue;
            }
            let full = self.root.join(&f.path);
            let gone = match f.kind {
                EntryKind::Dir => std::fs::remove_dir_all(&full),
                _ => std::fs::remove_file(&full),
            };
            if gone.is_ok() {
                removed += 1;
            }
        }
        let mut dirs: Vec<&Entry> = self.plan.iter().filter(|e| e.kind == EntryKind::Dir).collect();
        dirs.sort_by_key(|e| e.path.len());
        for d in dirs {
            let rel = safe_rel(&d.path)?;
            real_parents(&self.root, &rel)?;
            let full = self.root.join(&rel);
            if !full.is_dir() {
                std::fs::create_dir(&full).with_context(|| format!("create {}", full.display()))?;
            }
        }
        for l in self.plan.iter().filter(|e| e.kind == EntryKind::Symlink) {
            let rel = safe_rel(&l.path)?;
            real_parents(&self.root, &rel)?;
            let full = self.root.join(&rel);
            let target = l.target.clone().unwrap_or_default();
            if std::fs::read_link(&full).is_ok_and(|t| t.to_string_lossy() == target) {
                continue;
            }
            let _ = std::fs::remove_file(&full);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &full).with_context(|| format!("link {}", full.display()))?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
        // Free of this machine's git config: one that signs commits would ask for a key.
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()?
            .status
            .success();
        anyhow::ensure!(ok, "git {args:?} failed");
        Ok(())
    }

    /// Run a whole sync from `src` into a copy at `dst` as the two sides would.
    fn sync(src: &Path, dst: &Path) -> anyhow::Result<(Vec<String>, u32, u32)> {
        let tree = scan(src)?;
        let mut m = Mirror::new(dst.to_path_buf())?;
        m.plan(tree.entries.clone())?;
        let needed = need(dst, m.entries());
        for p in &needed {
            let mut off = 0;
            loop {
                let (data, last) = read_piece(&tree.source(p), off)?;
                m.write(p, off, &data, last)?;
                off += data.len() as u64;
                if last {
                    break;
                }
            }
        }
        let removed = m.commit()?;
        Ok((needed, m.written, removed))
    }

    #[test]
    fn a_repo_syncs_as_git_sees_it_then_only_what_changed() -> anyhow::Result<()> {
        let (src, dst) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let s = src.path();
        git(s, &["init", "-q"])?;
        std::fs::write(s.join(".gitignore"), "target/\n")?;
        std::fs::create_dir_all(s.join("src/deep"))?;
        std::fs::write(s.join("src/main.rs"), "fn main() {}\n")?;
        std::fs::write(s.join("src/deep/big.bin"), vec![7u8; PIECE * 2 + 10])?;
        std::fs::create_dir_all(s.join("target/debug"))?;
        std::fs::write(s.join("target/debug/huge"), vec![0u8; 1000])?;
        git(s, &["add", "."])?;
        git(s, &["commit", "-qm", "one"])?;
        std::fs::write(s.join("untracked.txt"), "new\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::write(s.join("run.sh"), "#!/bin/sh\necho hi\n")?;
            std::fs::set_permissions(s.join("run.sh"), std::fs::Permissions::from_mode(0o755))?;
            std::os::unix::fs::symlink("src/main.rs", s.join("link"))?;
        }
        let d = dst.path().join("copy");
        let (needed, written, _) = sync(s, &d)?;
        assert!(needed.len() >= 4 && written as usize == needed.len());
        assert_eq!(std::fs::read(d.join("src/deep/big.bin"))?, vec![7u8; PIECE * 2 + 10]);
        assert_eq!(std::fs::read_to_string(d.join("untracked.txt"))?, "new\n");
        assert!(!d.join("target").exists(), "ignored build output stays home");
        assert!(d.join(".git/HEAD").is_file(), "git comes along");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(d.join("run.sh"))?.permissions().mode() & 0o777, 0o755);
            assert_eq!(std::fs::read_link(d.join("link"))?, PathBuf::from("src/main.rs"));
        }
        // git works in the copy and agrees nothing changed but the untracked files.
        let status = std::process::Command::new("git").arg("-C").arg(&d).args(["status", "--porcelain"]).output()?;
        let status = String::from_utf8_lossy(&status.stdout).into_owned();
        assert!(status.contains("?? untracked.txt") && !status.contains("main.rs"), "{status}");
        // Again, unchanged: only the index `git status` refreshed over there comes back.
        let again = sync(s, &d)?.0;
        assert!(again.iter().all(|p| p == ".git/index"), "{again:?}");
        assert!(sync(s, &d)?.0.is_empty());
        // A change, a new file, a deletion: just those.
        std::fs::write(s.join("src/main.rs"), "fn main() { println!(\"x\"); }\n")?;
        std::fs::remove_file(s.join("untracked.txt"))?;
        std::fs::write(s.join("src/new.rs"), "\n")?;
        let (needed, _, removed) = sync(s, &d)?;
        assert_eq!(needed, vec!["src/main.rs".to_owned(), "src/new.rs".to_owned()]);
        assert!(removed >= 1);
        assert!(!d.join("untracked.txt").exists());
        assert_eq!(std::fs::read_to_string(d.join("src/main.rs"))?, "fn main() { println!(\"x\"); }\n");
        Ok(())
    }

    #[test]
    fn a_worktree_becomes_a_repository_of_its_own_over_there() -> anyhow::Result<()> {
        let (main, wt_parent, dst) = (tempfile::tempdir()?, tempfile::tempdir()?, tempfile::tempdir()?);
        let m = main.path();
        git(m, &["init", "-q", "-b", "main"])?;
        std::fs::write(m.join("a.txt"), "on main\n")?;
        git(m, &["add", "."])?;
        git(m, &["commit", "-qm", "main"])?;
        let wt = wt_parent.path().join("feature");
        git(m, &["worktree", "add", "-q", "-b", "feature", &wt.to_string_lossy()])?;
        std::fs::write(wt.join("a.txt"), "on feature\n")?;
        git(&wt, &["commit", "-qam", "feature work"])?;
        std::fs::write(wt.join("a.txt"), "uncommitted\n")?;
        let d = dst.path().join("copy");
        sync(&wt, &d)?;
        assert!(d.join(".git").is_dir(), "a real .git over there");
        let run = |args: &[&str]| -> anyhow::Result<String> {
            let o = std::process::Command::new("git").arg("-C").arg(&d).args(args).output()?;
            Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
        };
        assert_eq!(run(&["rev-parse", "--abbrev-ref", "HEAD"])?, "feature");
        assert_eq!(run(&["log", "--format=%s", "-1"])?, "feature work");
        assert_eq!(run(&["status", "--porcelain"])?, "M a.txt");
        assert!(!d.join(".git/worktrees").exists(), "the other worktrees stay home");
        Ok(())
    }

    #[test]
    fn a_plan_cannot_reach_outside_the_copy() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("copy");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside)?;
        let mut m = Mirror::new(root.clone())?;
        let file = |p: &str| Entry { path: p.into(), kind: EntryKind::File, size: 1, mtime_ns: 0, mode: 0o644, target: None };
        assert!(m.plan(vec![file("../escape")]).is_err());
        assert!(m.plan(vec![file("/etc/passwd")]).is_err());
        assert!(m.plan(vec![file("a\\b")]).is_err());
        assert!(Mirror::root_for(dir.path(), "../x", "k").is_err());
        assert!(Mirror::root_for(dir.path(), "mac", "a/b").is_err());
        // A symlink in the copy pointing outside: nothing is written through it.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("out"))?;
            m.plan(vec![file("out/pwned")])?;
            assert!(m.write("out/pwned", 0, b"x", true).is_err());
            assert!(!outside.join("pwned").exists());
        }
        // A file that is not on the list is refused.
        assert!(m.write("unlisted", 0, b"x", true).is_err());
        Ok(())
    }

    #[test]
    fn keys_differ_by_place_and_runner() {
        let a = mirror_key("mac", Path::new("/Users/a/Projects/kiln"));
        assert!(a.starts_with("kiln-"));
        assert_ne!(a, mirror_key("mac", Path::new("/Users/a/old/kiln")));
        assert_ne!(a, mirror_key("mac2", Path::new("/Users/a/Projects/kiln")));
        assert!(mirror_key("m", Path::new("/x/.hidden")).starts_with("hidden-"));
    }
}
