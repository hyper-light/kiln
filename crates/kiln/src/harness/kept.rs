//! Files a conversation's finished messages mention, kept by the server ([`super::files`]): read
//! from the session's machine once a message is done, stored by their content, and listed for its
//! viewers, so a screenshot still shows after the file moves, changes or goes (`/tmp` cleared, its
//! machine away).
//!
//! Immutable bytes stay in `files/blobs/<sha256>`. U17 lists and retirement tombstones live in
//! the shared Amphora Records snapshot; `files/<session>.json` is a checked read-only import.
//! Content no canonical list or page names goes at the next sweep. A file is kept whole up to [`MOST_KEPT`], a conversation keeps up to
//! [`MOST_PER_SESSION`], the server up to [`MOST_ALL`]; past those a file is only listed, and read
//! from its machine when someone looks.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::files::{self, Answer, Meta, PIECE};
use super::registry::Location;
use super::host::snapshot::Snapshot;
use kiln_seams::records::{Records, Refusal, Rev};
use crate::hub::Hub;

pub const MOST_KEPT: u64 = 64 << 20;
pub const MOST_PER_SESSION: u64 = 512 << 20;
pub const MOST_ALL: u64 = 4 << 30;
/// Finished messages waiting for their files to be kept; past that, a message's are read when
/// someone looks.
const QUEUE: usize = 4096;
/// How often content no list names is cleared away.
const SWEEP_EVERY: Duration = Duration::from_secs(6 * 3600);

/// A file a message mentioned, as it was when the message was done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "KeptFile"))]
pub struct Kept {
    /// As the message says it, in the session's folder when it was relative.
    pub path: String,
    /// The message (its `seq`). A later message mentioning the path after the file changed gets
    /// an entry of its own, so each shows the file as it was.
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub meta: Option<Meta>,
    /// Its content's SHA-256 when a copy is kept (none: too big or over a budget; it's read from
    /// its machine when someone looks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub sha: Option<String>,
    /// Why it couldn't be read (gone already, a secret, a type kiln doesn't show).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub error: Option<String>,
}

/// A finished message's mentions, to keep.
#[derive(Debug, Clone)]
pub struct Job {
    pub session: String,
    pub seq: u64,
    pub paths: Vec<String>,
    pub location: Location,
}

// U17: bounded metadata, including permanent retirement/import receipts. Reaching a bound
// refuses publication rather than dropping receipts or reviving old sidecars.
const MAX_LISTS: usize = 65_536;
const MAX_ENTRIES: usize = 65_536;
const IMPORT_FILES: usize = 4096;
const IMPORT_FILE_BYTES: u64 = 8 << 20;
const IMPORT_BYTES: u64 = 32 << 20;
const MAX_IMPORTS: usize = 4096;
const PUBLICATION_LOCK: &str = ".publication.lock";

#[derive(Default, Serialize, Deserialize)]
struct State {
    lists: HashMap<String, List>,
    imports: HashMap<String, String>,
}

#[derive(Default, Serialize, Deserialize)]
struct List {
    entries: Vec<Kept>,
    retired: bool,
    changed: i64,
}

#[derive(Debug, thiserror::Error)]
#[error("kept file metadata storage: {0:#}")]
pub struct StorageError(anyhow::Error);
fn storage(e: impl Into<anyhow::Error>) -> anyhow::Error { StorageError(e.into()).into() }

/// U17: publication contention and filesystem failures are retryable, never accepted writes.
#[derive(Debug, thiserror::Error)]
pub enum PublicationError {
    #[error("file byte root is busy; publication is unavailable, retry later")]
    Busy,
    #[error("file byte root is unavailable; retry publication: {0:#}")]
    Unavailable(#[source] anyhow::Error),
}
fn publication_unavailable(e: impl Into<anyhow::Error>) -> PublicationError { PublicationError::Unavailable(e.into()) }

/// The physical lock and in-process guard share ownership. Every dispatched byte writer and
/// metadata commit retains a clone, including after cancellation of its async caller. This is
/// cooperative exclusion: every frontend publishing/sweeping this byte root must use it.
#[derive(Clone)]
pub struct Publication { _held: Arc<PublicationHeld> }
struct PublicationHeld {
    _file: std::fs::File,
    _local: futures::lock::OwnedMutexGuard<()>,
}
fn sha_valid(sha: &str) -> bool { sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()) }
fn stem_valid(stem: &str) -> bool { !stem.is_empty() && stem.len() <= 128 && stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') }

/// Only the keeper's current/legacy session staging and Visuals' content staging formats.
fn staging_name(name: &str) -> bool {
    fn number(s: &str) -> bool { !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u64>().is_ok() }
    let Some(stem) = name.strip_suffix(".part") else { return false };
    if let Some((sha, at)) = stem.rsplit_once('.') { return sha_valid(sha) && number(at); }
    let Some((prefix, at)) = stem.rsplit_once('-') else { return false };
    if !number(at) { return false; }
    stem_valid(prefix) || prefix.rsplit_once('-').is_some_and(|(session, tag)| stem_valid(session) && (tag == "page" || number(tag)))
}
fn conflict(e: &anyhow::Error) -> bool { e.downcast_ref::<Refusal>().is_some_and(|e| matches!(e, Refusal::Conflict { .. })) }

fn validate_entries(entries: &[Kept]) -> anyhow::Result<()> {
    anyhow::ensure!(entries.len() <= MAX_ENTRIES, "U17 kept list exceeds {MAX_ENTRIES} entries");
    let mut pairs = HashSet::new();
    for k in entries {
        anyhow::ensure!(k.path.len() <= 16_384 && k.error.as_ref().is_none_or(|e| e.len() <= 16_384), "U17 kept entry text exceeds bounds");
        anyhow::ensure!(k.sha.as_deref().is_none_or(sha_valid), "U17 invalid kept content SHA");
        anyhow::ensure!(pairs.insert((k.seq, &k.path)), "U17 duplicate kept path/message entry");
    }
    Ok(())
}

impl State {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.lists.len() <= MAX_LISTS && self.imports.len() <= MAX_IMPORTS, "U17 kept metadata exceeds receipt/list bounds");
        for (id, list) in &self.lists {
            anyhow::ensure!(stem_valid(id) && (!list.retired || list.entries.is_empty()), "U17 invalid kept list/tombstone");
            validate_entries(&list.entries)?;
        }
        anyhow::ensure!(self.imports.iter().all(|(origin, digest)| sha_valid(origin) && sha_valid(digest)), "U17 invalid kept import receipt");
        Ok(())
    }
}

pub struct Keeper {
    dir: PathBuf,
    snapshot: Snapshot,
    origin: String,
    /// Only an eviction hint, never a metadata read cache.
    touched: Mutex<HashSet<String>>,
    gone: Mutex<HashSet<String>>,
    /// U17: this async guard alone is process-local. `publication` also takes the physical
    /// byte root's stable file lock, shared by every frontend's publishers and sweep.
    publication: Arc<futures::lock::Mutex<()>>,
    tx: async_channel::Sender<Job>,
    rx: async_channel::Receiver<Job>,
    used: AtomicU64,
}

impl Keeper {
    pub fn new(harness_dir: &Path, records: Arc<dyn Records>, source_host: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(!source_host.is_empty(), "U17 empty kept import host identity");
        let dir = if harness_dir.is_absolute() { harness_dir.join("files") } else { std::env::current_dir()?.join(harness_dir).join("files") };
        let origin = hex::encode(Sha256::digest(serde_json::to_vec(&(source_host, &dir))?));
        let (snapshot, _) = Snapshot::open::<State>(records, "files/lists", None).map_err(storage)?;
        let (tx, rx) = async_channel::bounded(QUEUE);
        let this = Self { dir, snapshot, origin, touched: Mutex::default(), gone: Mutex::default(), publication: Arc::new(futures::lock::Mutex::new(())), tx, rx, used: AtomicU64::new(u64::MAX) };
        this.import().map_err(storage)?;
        Ok(this)
    }

    pub async fn publication(&self) -> Result<Publication, PublicationError> {
        let local = self.publication.try_lock_owned().ok_or(PublicationError::Busy)?;
        let root = self.dir.join("blobs");
        crate::rt::blocking(move || {
            std::fs::create_dir_all(&root).map_err(publication_unavailable)?;
            // Never unlink/replace this file: all SDK/Hub frontends using the same physical
            // byte directory must contend on this inode, even when their Keeper differs.
            let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
                .open(root.join(PUBLICATION_LOCK)).map_err(publication_unavailable)?;
            if !std::fs::symlink_metadata(root.join(PUBLICATION_LOCK)).map_err(publication_unavailable)?.is_file() {
                return Err(publication_unavailable(anyhow::anyhow!("byte-root publication lock is not a regular file")));
            }
            match file.try_lock() {
                Ok(()) => Ok(Publication { _held: Arc::new(PublicationHeld { _file: file, _local: local }) }),
                Err(std::fs::TryLockError::WouldBlock) => Err(PublicationError::Busy),
                Err(std::fs::TryLockError::Error(error)) => Err(publication_unavailable(error)),
            }
        }).await.map_err(publication_unavailable)?
    }

    fn state(&self, imported: bool) -> anyhow::Result<(Rev, State)> {
        let (rev, state) = self.snapshot.read_versioned::<State>()?.ok_or_else(|| anyhow::anyhow!("U17 canonical kept metadata disappeared"))?;
        state.validate()?;
        anyhow::ensure!(!imported || state.imports.contains_key(&self.origin), "U17 kept source import is incomplete");
        Ok((rev, state))
    }

    /// Exact historical layout only. Hash-named legacy lists retain that lookup key: their
    /// JSON has no original session ID to reconstruct. No source is modified or globbed elsewhere.
    fn import(&self) -> anyhow::Result<()> {
        if self.state(false)?.1.imports.contains_key(&self.origin) { return Ok(()); }
        let mut inputs = Vec::new();
        let mut total = 0u64;
        match std::fs::read_dir(&self.dir) {
            Ok(entries) => for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
                anyhow::ensure!(inputs.len() < IMPORT_FILES, "U17 legacy kept lists exceed {IMPORT_FILES} files");
                anyhow::ensure!(entry.file_type()?.is_file(), "U17 legacy kept list is not a regular file: {}", path.display());
                let stem = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| anyhow::anyhow!("U17 invalid legacy kept filename"))?.to_owned();
                anyhow::ensure!(stem_valid(&stem), "U17 unsafe legacy kept filename: {}", path.display());
                let md = entry.metadata()?;
                anyhow::ensure!(md.len() <= IMPORT_FILE_BYTES && md.len() <= IMPORT_BYTES.saturating_sub(total), "U17 legacy kept list import exceeds byte bounds: {}", path.display());
                total += md.len();
                inputs.push((stem, path, md));
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        }
        inputs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut digest = Sha256::new();
        digest.update(b"U17-kept-import-v1");
        digest.update((inputs.len() as u64).to_be_bytes());
        let mut lists = Vec::new();
        for (stem, path, md) in inputs {
            let mut raw = Vec::new();
            std::fs::File::open(&path)?.take(IMPORT_FILE_BYTES + 1).read_to_end(&mut raw)?;
            let after = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(after.is_file() && raw.len() as u64 == md.len() && after.len() == md.len() && after.modified()? == md.modified()?, "U17 legacy kept source changed during import: {}", path.display());
            let mut entries: Vec<Kept> = serde_json::from_slice(&raw)?;
            validate_entries(&entries)?;
            for k in &mut entries { if let Some(sha) = &mut k.sha { sha.make_ascii_lowercase(); } }
            digest.update((stem.len() as u64).to_be_bytes());
            digest.update(stem.as_bytes());
            digest.update(Sha256::digest(&raw));
            lists.push((stem, entries));
        }
        let fingerprint = hex::encode(digest.finalize());
        for _ in 0..3 {
            let (rev, mut state) = self.state(false)?;
            if state.imports.contains_key(&self.origin) { return Ok(()); }
            for (stem, entries) in &lists {
                let list = state.lists.entry(stem.clone()).or_default();
                if list.retired { continue; }
                let mut pairs: HashMap<_, _> = list.entries.iter().enumerate().map(|(n, k)| ((k.seq, k.path.clone()), n)).collect();
                for k in entries {
                    let pair = (k.seq, k.path.clone());
                    if let Some(index) = pairs.get(&pair) {
                        anyhow::ensure!(list.entries.get(*index) == Some(k), "U17 divergent kept source for {stem} at message {}", k.seq);
                    } else {
                        pairs.insert(pair, list.entries.len());
                        list.entries.push(k.clone());
                    }
                }
            }
            state.imports.insert(self.origin.clone(), fingerprint.clone());
            state.validate()?;
            match self.snapshot.save_at(&state, rev) {
                Ok(()) => return Ok(()),
                Err(e) if conflict(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        anyhow::bail!("U17 kept import kept changing; import was not committed")
    }

    pub fn blob(&self, sha: &str) -> Option<PathBuf> { sha_valid(sha).then(|| self.dir.join("blobs").join(sha.to_ascii_lowercase())) }

    pub fn list(&self, session: &str) -> anyhow::Result<Vec<Kept>> {
        let (_, mut state) = self.state(true).map_err(storage)?;
        let key = file_name(session);
        self.touched.lock().insert(key.clone());
        Ok(state.lists.remove(&key).map_or_else(Vec::new, |l| l.entries))
    }

    pub fn find(&self, session: &str, sha: &str) -> anyhow::Result<Option<Kept>> {
        Ok(self.list(session)?.into_iter().rev().find(|k| k.sha.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(sha))))
    }
    pub fn lists_path(&self, session: &str, path: &str) -> anyhow::Result<bool> { Ok(self.list(session)?.iter().any(|k| k.path == path)) }
    pub fn frame(&self, session: &str) -> anyhow::Result<Option<String>> {
        let l = self.list(session)?;
        Ok((!l.is_empty()).then(|| crate::wire::ui::SessionDown::from(crate::wire::ui::SessionFrame::Files { files: l }).text()))
    }

    pub fn enqueue(&self, job: Job) {
        if job.paths.is_empty() || self.gone.lock().contains(&job.session) { return; }
        if self.tx.try_send(job).is_err() { tracing::debug!("too many messages wait for their files to be kept; this one's are read when someone looks"); }
    }

    /// Commit retirement before publication or GC. Late jobs and other hosts' legacy imports
    /// cannot resurrect a retired session, including after every process has restarted.
    pub fn forget(&self, session: &str) -> anyhow::Result<()> {
        self.edit(session, &[], true)?;
        self.gone.lock().insert(session.to_owned());
        self.touched.lock().remove(&file_name(session));
        Ok(())
    }

    /// Fresh CAS applies only this message's additions, never a stale complete list.
    fn save(&self, session: &str, additions: &[Kept]) -> anyhow::Result<Vec<Kept>> { self.edit(session, additions, false) }
    fn edit(&self, session: &str, additions: &[Kept], retire: bool) -> anyhow::Result<Vec<Kept>> {
        let key = file_name(session);
        validate_entries(additions).map_err(storage)?;
        for _ in 0..3 {
            let (rev, mut state) = self.state(true).map_err(storage)?;
            let list = state.lists.entry(key.clone()).or_default();
            if list.retired { return Ok(Vec::new()); }
            let mut added = Vec::new();
            if retire { list.retired = true; list.entries.clear(); }
            else {
                let mut pairs: HashSet<_> = list.entries.iter().map(|k| (k.seq, k.path.clone())).collect();
                for k in additions {
                    if pairs.insert((k.seq, k.path.clone())) { list.entries.push(k.clone()); added.push(k.clone()); }
                }
                if added.is_empty() { return Ok(added); }
            }
            list.changed = crate::util::now_ms();
            state.validate().map_err(storage)?;
            match self.snapshot.save_at(&state, rev) {
                Ok(()) => return Ok(added),
                Err(e) if conflict(&e) => continue,
                Err(e) => return Err(storage(e)),
            }
        }
        Err(storage(anyhow::anyhow!("U17 kept list kept changing; update was not committed")))
    }

    /// Caller holds publication guard through page/list reads, eviction commit and byte deletion.
    /// Read/parse/import/CAS errors return before deletion, never an empty keep set.
    fn sweep(&self, also: &HashSet<String>) -> anyhow::Result<u64> { self.sweep_bounded(also, MOST_ALL / 4 * 3) }

    fn sweep_bounded(&self, also: &HashSet<String>, budget: u64) -> anyhow::Result<u64> {
        let (rev, mut state) = self.state(true).map_err(storage)?;
        anyhow::ensure!(also.iter().all(|s| sha_valid(s)), "U17 invalid page content SHA; sweep skipped");
        let blobs = self.dir.join("blobs");
        let mut sizes = HashMap::new();
        let mut parts = Vec::new();
        match std::fs::read_dir(&blobs) {
            Ok(entries) => for entry in entries {
                let e = entry?;
                let md = e.metadata()?;
                anyhow::ensure!(e.file_type()?.is_file(), "U17 blob store contains a non-regular file");
                let name = e.file_name().into_string().map_err(|_| anyhow::anyhow!("U17 invalid blob filename"))?;
                if name == PUBLICATION_LOCK { continue; } // The stable physical owner lock is never garbage.
                if name.ends_with(".part") {
                    if staging_name(&name) && md.modified()?.elapsed().is_ok_and(|age| age > Duration::from_secs(3600)) { parts.push(e.path()); }
                } else {
                    anyhow::ensure!(sha_valid(&name) && name == name.to_ascii_lowercase(), "U17 invalid blob filename {name:?}; sweep skipped");
                    sizes.insert(name, md.len());
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        }
        let named = |s: &State| -> HashSet<String> { s.lists.values().flat_map(|l| l.entries.iter().filter_map(|k| k.sha.as_ref().map(|s| s.to_ascii_lowercase()))).chain(also.iter().map(|s| s.to_ascii_lowercase())).collect() };
        let size_of = |keep: &HashSet<String>| -> u64 { sizes.iter().filter(|(s, _)| keep.contains(*s)).fold(0u64, |n, (_, size)| n.saturating_add(*size)) };
        let mut keep = named(&state);
        let mut used = size_of(&keep);
        let live = self.touched.lock().clone();
        let mut oldest: Vec<_> = state.lists.iter().map(|(id, l)| (l.changed, id.clone())).collect();
        oldest.sort_unstable();
        let mut evicted = false;
        for (_, id) in oldest {
            if used <= budget { break; }
            if !live.contains(&id) && let Some(list) = state.lists.get_mut(&id) && list.entries.iter().any(|k| k.sha.is_some()) {
                for k in &mut list.entries { k.sha = None; }
                evicted = true;
                keep = named(&state);
                used = size_of(&keep);
            }
        }
        // U17: reference removal is durable before destructive byte cleanup. A concurrent
        // Records writer invalidates this eviction plan; it is not retried over newer metadata.
        if evicted { self.snapshot.save_at(&state, rev).map_err(storage)?; }
        for name in sizes.keys().filter(|s| !keep.contains(*s)) { std::fs::remove_file(blobs.join(name))?; }
        for path in parts { std::fs::remove_file(path)?; }
        Ok(used)
    }

    pub async fn run(hub: Arc<Hub>) {
        let mut next_sweep = Instant::now();
        loop {
            if Instant::now() >= next_sweep {
                let result = match hub.harness.keeper.publication().await {
                    Ok(publication) => {
                        let h = hub.clone();
                        crate::rt::blocking(move || {
                            // Cancellation of the async waiter cannot release either lock while GC runs.
                            let _publication = publication;
                            h.harness.visuals.shas().and_then(|shas| h.harness.keeper.sweep(&shas))
                        }).await
                    }
                    Err(error) => Ok(Err(error.into())),
                };
                match result {
                    Ok(Ok(used)) => hub.harness.keeper.used.store(used, Ordering::Relaxed),
                    result => tracing::warn!(?result, "U17 canonical metadata unavailable; skipped blob sweep"),
                }
                next_sweep = Instant::now() + SWEEP_EVERY;
            }
            match crate::rt::timeout(Duration::from_secs(600), hub.harness.keeper.rx.recv()).await {
                Ok(Ok(job)) => if let Err(error) = keep(&hub, job).await { tracing::warn!(error = %format!("{error:#}"), "U17 kept list was not published"); },
                Ok(Err(_)) => return,
                Err(_) => {}
            }
        }
    }
}

/// A conversation's id as a file name: as it is when it's plain, else its hash.
fn file_name(session: &str) -> String {
    if !session.is_empty() && session.len() <= 128 && session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        session.to_owned()
    } else {
        hex::encode(Sha256::digest(session.as_bytes()))
    }
}

/// A piece of a file a session's message mentions, read where the session runs. `Ok(None)`: its
/// runner doesn't read files (it's older than [`crate::proto::CAP_FILES`]).
pub async fn piece(hub: &Hub, location: &Location, session: &str, path: &str, offset: u64, len: u32) -> anyhow::Result<Option<(Answer, Vec<u8>)>> {
    match location {
        Location::Local => {
            let path = path.to_owned();
            let r = crate::rt::blocking(move || files::read(&path, offset, len)).await?;
            let a = Answer::of(&r);
            Ok(Some((a, r.map(|(_, d)| d).unwrap_or_default())))
        }
        Location::Machine(m) => {
            hub.harness_file_read(m, session, path, offset, len).await
        }
    }
}

/// A file of a session's machine into the store (a page's picture, style or script,
/// `super::visuals`), up to `most` bytes: its content's SHA-256, and what it is.
pub async fn store_file(hub: &Arc<Hub>, location: &Location, session: &str, path: &str, most: u64, publication: &Publication) -> anyhow::Result<(String, Meta)> {
    let Some((answer, first)) = piece(hub, location, session, path, 0, PIECE).await? else {
        anyhow::bail!("its machine's runner doesn't read files yet: upgrade it")
    };
    let meta = answer.meta.ok_or_else(|| anyhow::anyhow!(answer.error.unwrap_or_else(|| "it couldn't be read".to_owned())))?;
    anyhow::ensure!(meta.size <= most, "it's {} MB; a page's file may be up to {} MB", meta.size >> 20, most >> 20);
    let sha = copy(hub, location, session, publication, path, &meta, first).await?;
    Ok((sha, meta))
}

async fn keep(hub: &Arc<Hub>, job: Job) -> anyhow::Result<()> {
    let keeper = &hub.harness.keeper;
    let publication = keeper.publication().await?;
    let (h, session) = (hub.clone(), job.session.clone());
    let mut list = crate::rt::blocking(move || h.harness.keeper.list(&session)).await??;
    let mut added = Vec::new();
    for path in &job.paths {
        if list.iter().any(|k| k.path == *path && k.seq == job.seq) { continue; }
        if let Some(k) = keep_one(hub, &job, path, &list, &publication).await { list.push(k.clone()); added.push(k); }
    }
    if added.is_empty() { return Ok(()); }
    let (h, session) = (hub.clone(), job.session.clone());
    let committed = crate::rt::blocking(move || {
        let _publication = publication; // Keep exclusion even if the async waiter is cancelled.
        h.harness.keeper.save(&session, &added)
    }).await??;
    // No mirror/event/cache publication precedes the durable Records commit.
    if !committed.is_empty() { hub.harness.files_added(&job.session, &committed); }
    Ok(())
}

/// One mention: the file as it is now, copied when it fits. `None` when there's nothing to say
/// yet (its runner doesn't read files, or its machine is away: it's read when someone looks).
async fn keep_one(hub: &Arc<Hub>, job: &Job, path: &str, list: &[Kept], publication: &Publication) -> Option<Kept> {
    let (answer, first) = match piece(hub, &job.location, &job.session, path, 0, PIECE).await {
        Ok(Some(p)) => p,
        Ok(None) => return None,
        Err(e) => {
            tracing::debug!(session = %job.session, path, error = %format!("{e:#}"), "a file a message mentions couldn't be read to keep");
            return None;
        }
    };
    let entry = |meta: Option<Meta>, sha: Option<String>, error: Option<String>| Kept { path: path.to_owned(), seq: job.seq, meta, sha, error };
    let Some(meta) = answer.meta else { return Some(entry(None, None, answer.error.or_else(|| Some("it couldn't be read".to_owned())))) };
    // Unchanged since an earlier message mentioned it: the same copy.
    if let Some(prev) = list.iter().rev().find(|k| k.path == path)
        && let Some(pm) = &prev.meta
        && pm.size == meta.size
        && pm.mtime == meta.mtime
        && prev.sha.is_some()
    {
        return Some(entry(Some(meta), prev.sha.clone(), None));
    }
    let keeper = &hub.harness.keeper;
    let conversation: u64 = list.iter().filter(|k| k.sha.is_some()).filter_map(|k| k.meta.as_ref().map(|m| m.size)).fold(0u64, u64::saturating_add);
    let fits = meta.size <= MOST_KEPT && conversation.saturating_add(meta.size) <= MOST_PER_SESSION && keeper.used.load(Ordering::Relaxed).saturating_add(meta.size) <= MOST_ALL;
    if !fits {
        return Some(entry(Some(meta), None, None));
    }
    match copy(hub, &job.location, &job.session, publication, path, &meta, first).await {
        Ok(sha) => Some(entry(Some(meta), Some(sha), None)),
        Err(e) => {
            tracing::debug!(session = %job.session, path, error = %format!("{e:#}"), "a file a message mentions couldn't be kept; it's read when someone looks");
            Some(entry(Some(meta), None, None))
        }
    }
}

/// The whole file into the store, a piece at a time (never all of it in memory), checking it
/// doesn't change on the way; its SHA-256.
async fn copy(hub: &Arc<Hub>, location: &Location, session: &str, publication: &Publication, path: &str, meta: &Meta, first: Vec<u8>) -> anyhow::Result<String> {
    let blobs = hub.harness.keeper.dir.join("blobs");
    let tmp = blobs.join(format!("{}-{}.part", file_name(session), crate::util::now_ms()));
    let (b, t, held) = (blobs.clone(), tmp.clone(), publication.clone());
    let file = crate::rt::blocking(move || -> anyhow::Result<std::fs::File> {
        let _publication = held;
        std::fs::create_dir_all(&b)?;
        Ok(std::fs::File::create(&t)?)
    })
    .await??;
    // Piece by piece: each written off the async loop, the file handed back for the next.
    let written: anyhow::Result<(std::fs::File, Sha256)> = async move {
        let (mut file, mut hasher, mut offset, mut data) = (file, Sha256::new(), 0u64, first);
        loop {
            anyhow::ensure!(!data.is_empty() || offset == meta.size, "{path} ended early");
            hasher.update(&data);
            offset += data.len() as u64;
            let held = publication.clone();
            file = crate::rt::blocking(move || -> anyhow::Result<std::fs::File> {
                let _publication = held;
                file.write_all(&data)?;
                Ok(file)
            })
            .await??;
            if offset >= meta.size {
                return Ok((file, hasher));
            }
            let Some((a, d)) = piece(hub, location, session, path, offset, PIECE).await? else { anyhow::bail!("its runner stopped reading files") };
            let m = a.meta.ok_or_else(|| anyhow::anyhow!(a.error.unwrap_or_else(|| "it couldn't be read".to_owned())))?;
            anyhow::ensure!(m.size == meta.size && m.mtime == meta.mtime, "{path} changed while kiln copied it");
            data = d;
        }
    }
    .await;
    let (file, hasher) = match written {
        Ok(w) => w,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    let sha = hex::encode(hasher.finalize());
    let dest = blobs.join(&sha);
    let size = meta.size;
    let held = publication.clone();
    let fresh = crate::rt::blocking(move || -> anyhow::Result<bool> {
        let _publication = held;
        drop(file);
        if dest.exists() {
            std::fs::remove_file(&tmp)?;
            Ok(false)
        } else {
            std::fs::rename(&tmp, &dest)?;
            Ok(true)
        }
    })
    .await??;
    if fresh {
        let used = &hub.harness.keeper.used;
        let now = used.load(Ordering::Relaxed);
        if now != u64::MAX {
            used.store(now.saturating_add(size), Ordering::Relaxed);
        }
    }
    Ok(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversation_is_named_plainly_on_disk_or_by_its_hash() {
        assert_eq!(file_name("agent-01HZX_9"), "agent-01HZX_9");
        assert_eq!(file_name("../../etc").len(), 64);
        assert_eq!(file_name("").len(), 64);
    }

    #[test]
    fn u17_lists_import_reopen_merge_retire_and_refuse_writes() -> anyhow::Result<()> {
        use kiln_seams::records::SimRecords;
        let dir = tempfile::tempdir()?;
        let files = dir.path().join("files");
        std::fs::create_dir_all(&files)?;
        let sha = "a".repeat(64);
        let list = vec![
            Kept { path: "/tmp/a.png".into(), seq: 3, meta: Some(Meta { size: 10, mtime: 1, mime: "image/png".into(), width: Some(2), height: Some(1) }), sha: Some(sha.clone()), error: None },
            Kept { path: "/tmp/gone.png".into(), seq: 4, meta: None, sha: None, error: Some("/tmp/gone.png isn't there any more".into()) },
        ];
        let source = files.join("s1.json");
        let raw = serde_json::to_vec(&list)?;
        std::fs::write(&source, &raw)?;
        std::fs::write(files.join(format!("{}.json", file_name("../../old-session"))), &raw)?;
        let records = Arc::new(kiln_sim::records::Crashing::new(SimRecords::default()));
        let k = Keeper::new(dir.path(), records.clone(), "host-a")?;
        let k2 = Keeper::new(dir.path(), records.clone(), "host-a")?;
        assert_eq!(k2.list("s1")?, list);
        assert_eq!(k2.list("../../old-session")?, list, "hashed legacy IDs retain their exact lookup encoding");
        assert_eq!(k2.find("s1", &sha.to_uppercase())?.map(|f| f.seq), Some(3));
        assert!(k2.lists_path("s1", "/tmp/gone.png")? && !k2.lists_path("s1", "/tmp/other.png")?);
        assert!(k2.frame("s1")?.is_some_and(|f| f.contains("\"t\":\"files\"")));
        assert_eq!(k2.blob("not-a-sha"), None);
        let extra = Kept { path: "/tmp/extra.png".into(), seq: 5, meta: None, sha: None, error: None };
        k.save("s1", std::slice::from_ref(&extra))?;
        let unrelated = Kept { path: "/tmp/other.png".into(), seq: 6, ..extra.clone() };
        k2.save("s1", std::slice::from_ref(&unrelated))?;
        assert_eq!(k.list("s1")?.len(), 4, "another instance's additions survive fresh CAS deltas");
        records.refuse_at(0, Refusal::Unavailable("fixture outage".into()));
        let refused = k.forget("s1").err().ok_or_else(|| anyhow::anyhow!("retirement was acknowledged without a commit"))?;
        assert!(refused.downcast_ref::<StorageError>().is_some());
        assert_eq!(k2.list("s1")?.len(), 4);
        assert!(!k.gone.lock().contains("s1"), "a refused retirement cannot publish a local tombstone");
        k2.forget("s1")?;
        assert!(k.save("s1", std::slice::from_ref(&extra))?.is_empty(), "late jobs cannot revive retirement");
        // A different host's first import of the same stale list also cannot revive it.
        let late = Keeper::new(dir.path(), records.clone(), "host-b")?;
        assert!(late.list("s1")?.is_empty());
        assert_eq!(std::fs::read(&source)?, raw, "import and retirement never write/delete source JSON");
        std::fs::write(&source, b"obsolete corrupt sidecar")?;
        assert!(Keeper::new(dir.path(), records.clone(), "host-a")?.list("s1")?.is_empty());
        // First import errors are visible, and a repaired exact source can be retried.
        let fresh = Arc::new(SimRecords::default());
        assert!(Keeper::new(dir.path(), fresh.clone(), "host-a").is_err());
        std::fs::write(&source, &raw)?;
        let repaired = Keeper::new(dir.path(), fresh.clone(), "host-a")?;
        assert_eq!(repaired.list("s1")?, list);
        // Canonical snapshots cross the Records value limit without creating feature files.
        let large: Vec<_> = (10..710).map(|seq| Kept { path: format!("{seq}/{}", "x".repeat(1800)), seq, meta: None, sha: None, error: None }).collect();
        repaired.save("large", &large)?;
        assert_eq!(Keeper::new(dir.path(), fresh, "host-a")?.list("large")?, large);
        // The only legacy filenames are plain session IDs or their old SHA encoding.
        let unsafe_dir = tempfile::tempdir()?;
        std::fs::create_dir_all(unsafe_dir.path().join("files"))?;
        std::fs::write(unsafe_dir.path().join("files/bad.name.json"), b"[]")?;
        assert!(Keeper::new(unsafe_dir.path(), Arc::new(SimRecords::default()), "host-a").is_err());
        Ok(())
    }

    #[test]
    fn u17_sweep_preserves_live_refs_and_fails_closed_before_deletion() -> anyhow::Result<()> {
        use kiln_seams::records::SimRecords;
        let dir = tempfile::tempdir()?;
        let records = Arc::new(kiln_sim::records::Crashing::new(SimRecords::default()));
        let k = Keeper::new(dir.path(), records.clone(), "host-a")?;
        // Distinct frontends have distinct async mutexes. The stable physical lock must still
        // refuse overlap, and a dispatched closure's clone must retain it after caller drop.
        let other = Keeper::new(dir.path(), records.clone(), "host-a")?;
        let held = futures::executor::block_on(k.publication())?;
        let dispatched = held.clone();
        drop(held);
        let busy = futures::executor::block_on(other.publication()).err().ok_or_else(|| anyhow::anyhow!("another frontend bypassed the shared byte-root lock"))?;
        assert!(matches!(busy, PublicationError::Busy));
        drop(dispatched);
        drop(futures::executor::block_on(other.publication())?);
        let _publication = futures::executor::block_on(k.publication())?;
        let blobs = dir.path().join("files/blobs");
        std::fs::create_dir_all(&blobs)?;
        let (named, unnamed, page) = ("b".repeat(64), "c".repeat(64), "d".repeat(64));
        std::fs::write(blobs.join(&named), b"0123456789")?;
        std::fs::write(blobs.join(&unnamed), b"01234")?;
        std::fs::write(blobs.join(&page), b"0123")?;
        let staging = blobs.join("s2-page-123.part");
        let unrelated = blobs.join("unrelated.part");
        for path in [&staging, &unrelated] {
            std::fs::write(path, b"stale")?;
            std::fs::File::options().write(true).open(path)?.set_modified(std::time::SystemTime::UNIX_EPOCH)?;
        }
        assert!(staging_name(&format!("{page}.123.part")) && !staging_name("unrelated.part"));
        k.save("s2", &[Kept { path: "/tmp/b.png".into(), seq: 1, meta: None, sha: Some(named.clone()), error: None }])?;
        let also = HashSet::from([page.clone()]);
        // Use the existing crash/refusal fixture: no new testing controller.
        records.refuse_at(0, Refusal::Unavailable("fixture outage".into()));
        assert!(k.sweep_bounded(&also, 0).is_err(), "eviction must commit before deleting any bytes");
        assert!(blobs.join(&named).exists() && blobs.join(&unnamed).exists());
        assert!(k.find("s2", &named)?.is_some());
        records.crash_at(0, false);
        assert!(k.forget("s2").is_err());
        assert!(k.sweep(&also).is_err(), "an unavailable read is never an empty keep set");
        assert!(blobs.join(&unnamed).exists());
        records.restart();
        assert_eq!(k.sweep(&also)?, 14);
        assert!(blobs.join(&named).exists() && blobs.join(&page).exists() && !blobs.join(&unnamed).exists());
        assert!(blobs.join(PUBLICATION_LOCK).is_file(), "sweep must never unlink the lock's stable inode");
        assert!(!staging.exists() && unrelated.exists(), "stale cleanup only removes real blob staging names");
        let busy = futures::executor::block_on(other.publication()).err().ok_or_else(|| anyhow::anyhow!("sweep replaced/unlinked the held lock inode"))?;
        assert!(matches!(busy, PublicationError::Busy), "another frontend still contends on the same held inode after GC");
        std::fs::write(blobs.join(&unnamed), b"01234")?;
        let (rev, mut state) = k.state(true)?;
        state.imports.clear();
        k.snapshot.save_at(&state, rev)?;
        assert!(k.sweep(&also).is_err(), "an incomplete local import prevents sweep");
        assert!(blobs.join(&unnamed).exists());
        // A malformed canonical root is also an error, not a fallback to old files.
        futures::executor::block_on(records.commit(kiln_seams::records::Tx { conds: Vec::new(), writes: vec![kiln_seams::records::Write::Put { family: "session-control".into(), key: b"files/lists".to_vec(), value: b"corrupt".to_vec() }] }))?;
        assert!(k.list("s2").is_err() && k.sweep(&also).is_err());
        assert!(blobs.join(&named).exists() && blobs.join(&unnamed).exists());
        Ok(())
    }
}
