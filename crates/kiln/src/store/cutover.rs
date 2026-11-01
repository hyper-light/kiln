//! U05/U09: the format transition from a SQLite kiln.db, made by the build that finds one as it
//! starts. The directory at the old pathname fences old SQLite opens AND replica replacement. A
//! kiln.db is taken only once no other process has it open ([`supported`]'s `let_go`); its files
//! are kept read-only in the fence and imported.
use std::path::{Path, PathBuf};

pub(super) struct Fence {
    pub legacy: PathBuf,
    pub source: Option<PathBuf>,
    pub identity: Option<String>,
}

pub(super) fn ensure(path: &Path) -> anyhow::Result<Fence> {
    supported::ensure(path)
}
#[cfg(test)]
pub(super) fn prepare(path: &Path) -> anyhow::Result<Fence> {
    supported::prepare(path)
}

#[cfg(test)]
pub(super) fn with_sync(path: &Path, offline: bool, flush: &mut dyn FnMut(&Path) -> anyhow::Result<()>) -> anyhow::Result<Fence> {
    supported::with_sync(path, offline, flush)
}

mod supported {
    use super::*;
    use std::io::{Read as _, Write as _};
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use anyhow::Context as _;
    use serde::{Deserialize, Serialize};

    const FORMAT: &str = "amphora-format.json";
    const LIMIT: u64 = 16 << 10;
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Format { version: u32, source_identity: Option<String>, files: Vec<Entry> }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry { name: String, device: u64, inode: u64, bytes: u64, modified: (i64, i64) }
    struct Layout { legacy: PathBuf, pending: PathBuf, lock: PathBuf }
    fn layout(path: &Path) -> anyhow::Result<Layout> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let parent = parent.canonicalize()?;
        let name = path.file_name().context("U05 legacy database must have a filename")?;
        anyhow::ensure!(name != super::super::FILE, "U05 a legacy fence cannot replace the canonical Amphora database");
        let legacy = parent.join(name);
        let mut pending = name.to_os_string(); pending.push(".amphora-cutover");
        Ok(Layout { legacy, pending: parent.join(pending), lock: parent.join(".amphora-cutover.lock") })
    }
    // create_dir_all/existence do not prove the ancestor entries survived an earlier failed
    // sync. Establish the whole canonical chain bottom-up on every attempt, without guessing
    // which preexisting ancestor was durable. The path component count is finite.
    fn persist_ancestry(layout: &Layout, flush: &mut dyn FnMut(&Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
        for parent in layout.legacy.parent().context("U05 data directory")?.ancestors() {
            flush(parent).with_context(|| format!("U05 persist data-directory ancestor {}", parent.display()))?;
        }
        Ok(())
    }
    fn exists(path: &Path) -> anyhow::Result<bool> {
        match std::fs::symlink_metadata(path) { Ok(_) => Ok(true), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false), Err(e) => Err(e.into()) }
    }
    // Relocating a rollback-mode database without its journal can expose uncommitted
    // main-file pages as committed. Do not guess hotness or recover through a writable importer.
    fn refuse_journal(layout: &Layout) -> anyhow::Result<()> {
        let mut original = layout.legacy.as_os_str().to_os_string(); original.push("-journal");
        let mut journals = vec![PathBuf::from(original)];
        for dir in [&layout.pending, &layout.legacy] {
            if exists(dir)? && std::fs::symlink_metadata(dir)?.is_dir() { journals.push(dir.join("source.sqlite-journal")); }
        }
        for journal in journals {
            anyhow::ensure!(!exists(&journal)?, "U05 rollback journal {} requires the existing old OFFLINE recovery/checkpoint owner before cutover; no hotness is guessed and all inputs are kept unchanged", journal.display());
        }
        Ok(())
    }
    #[cfg(unix)]
    fn sync(path: &Path) -> anyhow::Result<()> { std::fs::File::open(path)?.sync_all()?; Ok(()) }
    /// Windows flushes no folder (NTFS journals what's in them), and a file only through a handle
    /// that may write it: one made read-only here was a link to a file its own writer had flushed.
    #[cfg(not(unix))]
    fn sync(path: &Path) -> anyhow::Result<()> {
        let meta = std::fs::metadata(path)?;
        if meta.is_dir() || meta.permissions().readonly() { return Ok(()); }
        std::fs::OpenOptions::new().write(true).open(path)?.sync_all()?;
        Ok(())
    }
    /// A SQLite kiln.db is taken only once no other process has it open: its journal can't leave
    /// WAL while another connection holds it (SQLite answers "wal"), and an exclusive transaction
    /// can't begin while one reads or writes. Leaving WAL folds the WAL into the file, and opening it
    /// to write rolls back a hot journal, so the file alone is then the source. A process that still
    /// has it open (an older build not yet stopped) keeps it: this start fails, and the next takes it.
    fn let_go(legacy: &Path) -> anyhow::Result<()> {
        let c = rusqlite::Connection::open_with_flags(legacy, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX).with_context(|| format!("U05 open legacy SQL {}", legacy.display()))?;
        c.busy_timeout(std::time::Duration::ZERO)?;
        let busy = || format!("U05 another process has {} open; it's cut over to Amphora once that process has stopped", legacy.display());
        let mode: String = c.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0)).with_context(busy)?;
        anyhow::ensure!(mode.eq_ignore_ascii_case("delete"), busy());
        c.execute_batch("BEGIN EXCLUSIVE; COMMIT").with_context(busy)?;
        Ok(())
    }
    fn roles(legacy: &Path) -> anyhow::Result<Vec<(&'static str, PathBuf)>> {
        let parent = legacy.parent().context("U05 legacy parent")?;
        let append = |suffix: &str| { let mut name = legacy.as_os_str().to_os_string(); name.push(suffix); PathBuf::from(name) };
        Ok(vec![("source.sqlite", legacy.to_path_buf()), ("source.sqlite-wal", append("-wal")), ("source.sqlite-shm", append("-shm")), ("cluster/cluster.log", parent.join("cluster/cluster.log"))])
    }
    fn entry(path: &Path, name: &str) -> anyhow::Result<Entry> {
        let meta = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(meta.is_file(), "U05 retired source is not a regular file: {}", path.display());
        Ok(Entry { name: name.into(), device: device(&meta), inode: inode(&meta), bytes: meta.len(), modified: modified(&meta)? })
    }
    #[cfg(unix)]
    fn device(meta: &std::fs::Metadata) -> u64 { meta.dev() }
    #[cfg(unix)]
    fn inode(meta: &std::fs::Metadata) -> u64 { meta.ino() }
    #[cfg(unix)]
    fn modified(meta: &std::fs::Metadata) -> anyhow::Result<(i64, i64)> { Ok((meta.mtime(), meta.mtime_nsec())) }
    // Stable Rust has no file identity on Windows: its size and last write say it's unchanged.
    #[cfg(not(unix))]
    fn device(_meta: &std::fs::Metadata) -> u64 { 0 }
    #[cfg(not(unix))]
    fn inode(_meta: &std::fs::Metadata) -> u64 { 0 }
    #[cfg(not(unix))]
    fn modified(meta: &std::fs::Metadata) -> anyhow::Result<(i64, i64)> {
        let since = meta.modified()?.duration_since(std::time::UNIX_EPOCH)?;
        Ok((i64::try_from(since.as_secs())?, i64::from(since.subsec_nanos())))
    }
    #[cfg(unix)]
    fn writable_bits(meta: &std::fs::Metadata) -> bool { meta.permissions().mode() & 0o222 != 0 }
    #[cfg(not(unix))]
    fn writable_bits(meta: &std::fs::Metadata) -> bool { !meta.permissions().readonly() }
    fn matches(path: &Path, expected: &Entry) -> anyhow::Result<()> {
        let actual = entry(path, &expected.name)?;
        anyhow::ensure!((actual.device, actual.inode, actual.bytes, actual.modified) == (expected.device, expected.inode, expected.bytes, expected.modified), "U05 preserved source changed: {}; retain all files and reconcile offline", path.display());
        Ok(())
    }
    // Mode bits alone do not protect against elevated credentials/ACLs. This nontruncating
    // open performs no write: if this process can obtain a writer, SQLite's WAL/SHM fallback
    // cannot be called immutable. Refuse before opening SQLite, rather than guess privileges.
    fn readonly(path: &Path) -> anyhow::Result<()> {
        anyhow::ensure!(!writable_bits(&std::fs::metadata(path)?), "U05 preserved source has writable mode bits; retain it and reconcile it by hand");
        match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(_) => anyhow::bail!("U05 this process can still write the preserved input {}; use its nonprivileged readonly owner/snapshot before migration; elevated/ACL write authority is unsupported", path.display()),
            Err(error) if matches!(error.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem) => Ok(()),
            Err(error) => Err(error).with_context(|| format!("U05 cannot establish readonly input {}", path.display())),
        }
    }
    fn read(dir: &Path) -> anyhow::Result<Format> {
        anyhow::ensure!(std::fs::symlink_metadata(dir)?.is_dir(), "U05 legacy pathname is not its directory fence");
        anyhow::ensure!(std::fs::symlink_metadata(dir.join(FORMAT))?.is_file(), "U05 format metadata is not a regular file");
        let mut bytes = Vec::new();
        std::fs::File::open(dir.join(FORMAT))?.take(LIMIT + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(u64::try_from(bytes.len())? <= LIMIT, "U05 format fence metadata exceeds 16KiB");
        let format: Format = serde_json::from_slice(&bytes).context("U05 invalid format fence; retain it and reconcile it by hand")?;
        anyhow::ensure!(format.version == 1 && format.files.len() <= 4, "U05 unsupported format fence");
        let roles = ["source.sqlite", "source.sqlite-wal", "source.sqlite-shm", "cluster/cluster.log"];
        let mut names = std::collections::HashSet::new();
        for file in &format.files {
            anyhow::ensure!(roles.contains(&file.name.as_str()) && names.insert(file.name.clone()), "U05 invalid or duplicate retired source role");
            matches(&dir.join(&file.name), file)?;
            readonly(&dir.join(&file.name))?;
        }
        anyhow::ensure!(!names.contains("source.sqlite-wal") || names.contains("source.sqlite-shm"), "U05 fenced readonly WAL has no SHM input; reconcile the stopped source offline");
        anyhow::ensure!(names.contains("source.sqlite") == format.source_identity.is_some() && (format.source_identity.is_some() || format.files.is_empty()), "U05 source identity/fence inventory disagrees");
        if let Some(id) = &format.source_identity { anyhow::ensure!(id.strip_prefix("main-sql-v1:").is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))), "U05 invalid source identity in fence"); }
        // Unknown files may be operator data; never discard them or call them a fresh source.
        for item in std::fs::read_dir(dir)? {
            let item = item?; let name = item.file_name();
            anyhow::ensure!(name == FORMAT || name == "cluster" || format.files.iter().any(|file| Path::new(&file.name).as_os_str() == name), "U05 unknown entry in format fence: {}", item.path().display());
        }
        let cluster = dir.join("cluster");
        if exists(&cluster)? {
            anyhow::ensure!(std::fs::symlink_metadata(&cluster)?.is_dir() && names.contains("cluster/cluster.log"), "U05 unknown archived cluster source");
            for child in std::fs::read_dir(&cluster)? { anyhow::ensure!(child?.file_name() == "cluster.log", "U05 unknown entry in archived cluster source"); }
        }
        Ok(format)
    }
    // Readability/existence after an unknown fsync outcome is not durable publication.
    // Repeat the SAME barriers on every resumed or existing fence before unlink/activation.
    fn durable(dir: &Path, format: &Format, flush: &mut dyn FnMut(&Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
        for file in &format.files { flush(&dir.join(&file.name))?; }
        flush(&dir.join(FORMAT))?;
        if dir.join("cluster").exists() { flush(&dir.join("cluster"))?; }
        flush(dir)?;
        flush(dir.parent().context("U05 fence parent")?)?;
        Ok(())
    }
    fn fence(legacy: PathBuf, format: Format) -> Fence {
        Fence { source: format.source_identity.as_ref().map(|_| legacy.join("source.sqlite")), identity: format.source_identity, legacy }
    }
    pub(super) fn ensure(path: &Path) -> anyhow::Result<Fence> { ensure_using(path, &mut sync) }
    fn ensure_using(path: &Path, flush: &mut dyn FnMut(&Path) -> anyhow::Result<()>) -> anyhow::Result<Fence> {
        let layout = layout(path)?;
        // A cutover an earlier start began goes on from where it stopped.
        if exists(&layout.pending)? {
            refuse_journal(&layout)?;
            return prepare_checked(path, true, flush);
        }
        // A SQLite kiln.db: cut over now, once nothing else has it open.
        if exists(&layout.legacy)? && !std::fs::symlink_metadata(&layout.legacy)?.is_dir() {
            let_go(&layout.legacy)?;
            refuse_journal(&layout)?;
            return prepare_checked(path, true, flush);
        }
        refuse_journal(&layout)?;
        if exists(&layout.legacy)? {
            persist_ancestry(&layout, flush)?;
            let format = read(&layout.legacy)?;
            durable(&layout.legacy, &format, flush)?;
            return Ok(fence(layout.legacy, format));
        }
        // Fresh activation also fences old CREATE-on-open. The prepared directory is renamed
        // only if the old pathname stays absent; a competing legacy creator causes refusal.
        prepare_checked(path, false, flush)
    }
    #[cfg(test)]
    pub(super) fn prepare(path: &Path) -> anyhow::Result<Fence> { prepare_checked(path, true, &mut sync) }
    fn prepare_checked(path: &Path, offline: bool, flush: &mut dyn FnMut(&Path) -> anyhow::Result<()>) -> anyhow::Result<Fence> {
        let layout = layout(path)?;
        refuse_journal(&layout)?;
        persist_ancestry(&layout, flush)?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&layout.lock)?;
        let _owned = crate::util::FileLock::try_lock(lock).map_err(|e| anyhow::anyhow!("U05 another cutover owns this directory: {e}"))?;
        refuse_journal(&layout)?;
        if exists(&layout.legacy)? && std::fs::symlink_metadata(&layout.legacy)?.is_dir() {
            anyhow::ensure!(!exists(&layout.pending)?, "U05 both completed and pending cutover exist; reconcile offline without deleting sources");
            let format = read(&layout.legacy)?;
            durable(&layout.legacy, &format, flush)?;
            return Ok(fence(layout.legacy, format));
        }
        if !offline {
            anyhow::ensure!(!exists(&layout.pending)?, "U05 pending cutover appeared during fresh activation; it goes on at the next start");
            for (_, original) in roles(&layout.legacy)? {
                anyhow::ensure!(!exists(&original)?, "U05 legacy input {} appeared during fresh activation; it is cut over at the next start", original.display());
            }
        }
        if !exists(&layout.pending)? { std::fs::create_dir(&layout.pending)?; flush(layout.pending.parent().context("U05 pending parent")?)?; }
        anyhow::ensure!(std::fs::symlink_metadata(&layout.pending)?.is_dir(), "U05 pending cutover is not a directory");
        let metadata = layout.pending.join(FORMAT);
        let temporary = layout.pending.join("amphora-format.next");
        if exists(&metadata)? && exists(&temporary)? {
            anyhow::ensure!(std::fs::symlink_metadata(&temporary)?.is_file(), "U05 invalid pending format file");
            std::fs::remove_file(&temporary)?; flush(&layout.pending)?;
        }
        let format = if exists(&metadata)? { read(&layout.pending)? } else {
            let source = exists(&layout.legacy)?;
            let roles = roles(&layout.legacy)?;
            let mut preserved = Vec::new();
            for (name, original) in &roles {
                let target = layout.pending.join(name);
                if exists(original)? {
                    let old = entry(original, name)?;
                    if let Some(parent) = target.parent() { std::fs::create_dir_all(parent)?; }
                    if !exists(&target)? { std::fs::hard_link(original, &target).with_context(|| format!("U05 preserve {} on the same filesystem; no copying fallback", original.display()))?; }
                    matches(&target, &old)?;
                    let mut permissions = std::fs::metadata(&target)?.permissions(); permissions.set_readonly(true);
                    std::fs::set_permissions(&target, permissions)?;
                    flush(&target)?;
                    readonly(&target)?;
                    preserved.push(name.to_string());
                } else {
                    anyhow::ensure!(!exists(&target)?, "U05 missing original for incomplete preserved source {}; reconcile offline", target.display());
                }
            }
            anyhow::ensure!(source || preserved.is_empty(), "U05 source-less legacy sidecars/log cannot be treated as an empty host");
            anyhow::ensure!(!preserved.iter().any(|name| name == "source.sqlite-wal") || preserved.iter().any(|name| name == "source.sqlite-shm"), "U05 readonly WAL source lacks its SHM coordination file; supply a complete stopped readonly WAL/SHM snapshot or checkpoint it with the old offline recovery owner before cutover; no SQLite source writes attempted");
            let identity = if source { Some(super::super::legacy::identity(&layout.pending.join("source.sqlite"))?) } else { None };
            let files = preserved.iter().map(|name| entry(&layout.pending.join(name), name)).collect::<anyhow::Result<Vec<_>>>()?;
            let format = Format { version: 1, source_identity: identity, files };
            let temporary = layout.pending.join("amphora-format.next");
            if exists(&temporary)? { anyhow::ensure!(std::fs::symlink_metadata(&temporary)?.is_file(), "U05 invalid pending format file"); std::fs::remove_file(&temporary)?; }
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
            file.write_all(&serde_json::to_vec(&format)?)?; file.sync_all()?;
            std::fs::rename(&temporary, &metadata)?;
            if layout.pending.join("cluster").exists() { flush(&layout.pending.join("cluster"))?; }
            flush(&layout.pending)?;
            read(&layout.pending)?
        };
        durable(&layout.pending, &format, flush)?;
        // This is intentionally NOT called an atomic exchange: no other process had the source
        // open when it was taken (`let_go`), and once the fence is published no older build can
        // open it again.
        for (name, original) in roles(&layout.legacy)? {
            if !exists(&original)? { continue }
            let saved = format.files.iter().find(|file| file.name == name).context("U05 unexpected old source appeared during cutover; keep all files")?;
            matches(&original, saved)?;
            remove_link(&original, &layout.pending.join(name))?;
            flush(original.parent().context("U05 source parent")?)?;
        }
        anyhow::ensure!(!exists(&layout.legacy)?, "U05 old pathname reappeared; old producer/restart exclusion was violated");
        std::fs::rename(&layout.pending, &layout.legacy)?;
        flush(layout.legacy.parent().context("U05 fence parent")?)?;
        Ok(fence(layout.legacy, format))
    }
    /// The source's old name goes; the link kept in the fence stays read-only. On Windows a file's
    /// read-only flag is the file's, every link's, and keeps a link from being removed: it's lifted
    /// for the removal and set again.
    #[cfg(unix)]
    fn remove_link(original: &Path, _kept: &Path) -> anyhow::Result<()> { std::fs::remove_file(original)?; Ok(()) }
    #[cfg(not(unix))]
    #[allow(clippy::permissions_set_readonly_false, reason = "Windows: clears the read-only attribute, nothing else")]
    fn remove_link(original: &Path, kept: &Path) -> anyhow::Result<()> {
        let mut permissions = std::fs::metadata(original)?.permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(original, permissions.clone())?;
        let removed = std::fs::remove_file(original);
        permissions.set_readonly(true);
        std::fs::set_permissions(kept, permissions)?;
        Ok(removed?)
    }
    #[cfg(test)]
    pub(super) fn with_sync(path: &Path, offline: bool, flush: &mut dyn FnMut(&Path) -> anyhow::Result<()>) -> anyhow::Result<Fence> {
        if offline { prepare_checked(path, true, flush) } else { ensure_using(path, flush) }
    }

}
