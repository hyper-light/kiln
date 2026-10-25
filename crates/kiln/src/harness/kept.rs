//! Files a conversation's finished messages mention, kept by the server ([`super::files`]): read
//! from the session's machine once a message is done, stored by their content, and listed for its
//! viewers, so a screenshot still shows after the file moves, changes or goes (`/tmp` cleared, its
//! machine away).
//!
//! In the harness folder: `files/blobs/<sha256>`, each content once whatever mentions it, and
//! `files/<session>.json`, what a conversation's messages mentioned (the path, the message, the
//! file as it was then). A closed conversation's list goes; content no list names goes at the next
//! sweep. A file is kept whole up to [`MOST_KEPT`], a conversation keeps up to
//! [`MOST_PER_SESSION`], the server up to [`MOST_ALL`]; past those a file is only listed, and read
//! from its machine when someone looks.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::files::{self, Answer, Meta, PIECE};
use super::registry::Location;
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
pub struct Kept {
    /// As the message says it, in the session's folder when it was relative.
    pub path: String,
    /// The message (its `seq`). A later message mentioning the path after the file changed gets
    /// an entry of its own, so each shows the file as it was.
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
    /// Its content's SHA-256 when a copy is kept (none: too big or over a budget; it's read from
    /// its machine when someone looks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// Why it couldn't be read (gone already, a secret, a type kiln doesn't show).
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

pub struct Keeper {
    dir: PathBuf,
    lists: Mutex<HashMap<String, Vec<Kept>>>,
    /// Conversations closed since start: nothing more is kept for them.
    gone: Mutex<HashSet<String>>,
    tx: async_channel::Sender<Job>,
    rx: async_channel::Receiver<Job>,
    /// What the kept content takes, as of the last sweep and since (`u64::MAX` before the first).
    used: AtomicU64,
}

impl Keeper {
    pub fn new(harness_dir: &Path) -> Self {
        let (tx, rx) = async_channel::bounded(QUEUE);
        Self { dir: harness_dir.join("files"), lists: Mutex::default(), gone: Mutex::default(), tx, rx, used: AtomicU64::new(u64::MAX) }
    }

    fn list_file(&self, session: &str) -> PathBuf {
        self.dir.join(format!("{}.json", file_name(session)))
    }

    /// Where content `sha` is kept (`None`: not a SHA-256).
    pub fn blob(&self, sha: &str) -> Option<PathBuf> {
        (sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())).then(|| self.dir.join("blobs").join(sha.to_ascii_lowercase()))
    }

    /// What a conversation's finished messages mentioned, in the order they did.
    pub fn list(&self, session: &str) -> Vec<Kept> {
        if let Some(l) = self.lists.lock().get(session) {
            return l.clone();
        }
        let l: Vec<Kept> = std::fs::read(self.list_file(session)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        self.lists.lock().insert(session.to_owned(), l.clone());
        l
    }

    /// Kept content `sha`, when one of the conversation's messages mentioned it.
    pub fn find(&self, session: &str, sha: &str) -> Option<Kept> {
        self.list(session).into_iter().rev().find(|k| k.sha.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(sha)))
    }

    /// Whether one of the conversation's finished messages mentioned `path`.
    pub fn lists_path(&self, session: &str, path: &str) -> bool {
        self.list(session).iter().any(|k| k.path == path)
    }

    /// The frame that tells a viewer what's kept (`None`: nothing yet).
    pub fn frame(&self, session: &str) -> Option<String> {
        let l = self.list(session);
        (!l.is_empty()).then(|| serde_json::json!({"t": "files", "files": l}).to_string())
    }

    pub fn enqueue(&self, job: Job) {
        if job.paths.is_empty() || self.gone.lock().contains(&job.session) {
            return;
        }
        if self.tx.try_send(job).is_err() {
            tracing::debug!("too many messages wait for their files to be kept; this one's are read when someone looks");
        }
    }

    /// A conversation that's closed: its list goes, its content at the next sweep.
    pub fn forget(&self, session: &str) {
        self.gone.lock().insert(session.to_owned());
        self.lists.lock().remove(session);
        let _ = std::fs::remove_file(self.list_file(session));
    }

    fn save(&self, session: &str, list: &[Kept]) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.list_file(session);
        let tmp = path.with_extension("json.part");
        std::fs::write(&tmp, serde_json::to_vec(list)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Content no list names goes (and copies left half-made); returns what's left. Past the
    /// server's budget, the lists of conversations that changed least recently go first.
    fn sweep(&self, also: &HashSet<String>) -> u64 {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return 0 };
        let mut lists: Vec<(std::time::SystemTime, PathBuf, Vec<Kept>)> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let when = e.metadata().and_then(|m| m.modified()).ok()?;
                let list: Vec<Kept> = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
                Some((when, e.path(), list))
            })
            .collect();
        lists.sort_by_key(|(when, _, _)| *when);
        let blobs = self.dir.join("blobs");
        let mut sizes: HashMap<String, u64> = HashMap::new();
        for e in std::fs::read_dir(&blobs).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let md = e.metadata().ok();
            if name.ends_with(".part") {
                // A copy half-made by a server that stopped mid-way.
                if md.and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(3600)) {
                    let _ = std::fs::remove_file(e.path());
                }
                continue;
            }
            sizes.insert(name, md.map_or(0, |m| m.len()));
        }
        // What lists name, and what pages use (`super::visuals`: theirs stays while their
        // conversation does).
        let named = |lists: &[(std::time::SystemTime, PathBuf, Vec<Kept>)]| -> HashSet<String> { lists.iter().flat_map(|(_, _, l)| l.iter().filter_map(|k| k.sha.clone())).chain(also.iter().cloned()).collect() };
        let mut keep = named(&lists);
        let mut used: u64 = sizes.iter().filter(|(s, _)| keep.contains(*s)).map(|(_, n)| n).sum();
        // Over budget: the oldest conversations' copies go (their lists stay, read live).
        let live: HashSet<String> = self.lists.lock().keys().cloned().collect();
        let mut i = 0;
        while used > MOST_ALL / 4 * 3 && i < lists.len() {
            if let Some((_, path, list)) = lists.get_mut(i)
                && !path.file_stem().is_some_and(|s| live.contains(&s.to_string_lossy().into_owned()))
                && list.iter().any(|k| k.sha.is_some())
            {
                for k in list.iter_mut() {
                    k.sha = None;
                }
                if let Ok(b) = serde_json::to_vec(list) {
                    let _ = std::fs::write(&*path, b);
                }
                keep = named(&lists);
                used = sizes.iter().filter(|(s, _)| keep.contains(*s)).map(|(_, n)| n).sum();
            }
            i += 1;
        }
        for name in sizes.keys().filter(|s| !keep.contains(*s)) {
            let _ = std::fs::remove_file(blobs.join(name));
        }
        used
    }

    /// Keep what finished messages mention as they come, and sweep now and then.
    pub async fn run(hub: Arc<Hub>) {
        let mut next_sweep = Instant::now();
        loop {
            if Instant::now() >= next_sweep {
                let h = hub.clone();
                let used = crate::rt::blocking(move || h.harness.keeper.sweep(&h.harness.visuals.shas())).await.unwrap_or(0);
                hub.harness.keeper.used.store(used, Ordering::Relaxed);
                next_sweep = Instant::now() + SWEEP_EVERY;
            }
            match crate::rt::timeout(Duration::from_secs(600), hub.harness.keeper.rx.recv()).await {
                Ok(Ok(job)) => keep(&hub, job).await,
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
            let Some((json, data)) = hub.harness_file_read(m, session, path, offset, len).await? else { return Ok(None) };
            Ok(Some((serde_json::from_str(&json)?, data)))
        }
    }
}

/// A file of a session's machine into the store (a page's picture, style or script,
/// `super::visuals`), up to `most` bytes: its content's SHA-256, and what it is.
pub async fn store_file(hub: &Arc<Hub>, location: &Location, session: &str, path: &str, most: u64) -> anyhow::Result<(String, Meta)> {
    let Some((answer, first)) = piece(hub, location, session, path, 0, PIECE).await? else {
        anyhow::bail!("its machine's runner doesn't read files yet: upgrade it")
    };
    let meta = answer.meta.ok_or_else(|| anyhow::anyhow!(answer.error.unwrap_or_else(|| "it couldn't be read".to_owned())))?;
    anyhow::ensure!(meta.size <= most, "it's {} MB; a page's file may be up to {} MB", meta.size >> 20, most >> 20);
    let sha = copy(hub, location, session, "page", path, &meta, first).await?;
    Ok((sha, meta))
}

async fn keep(hub: &Arc<Hub>, job: Job) {
    let keeper = &hub.harness.keeper;
    let mut list = keeper.list(&job.session);
    let mut added = Vec::new();
    for path in &job.paths {
        if list.iter().any(|k| k.path == *path && k.seq == job.seq) {
            continue;
        }
        if let Some(k) = keep_one(hub, &job, path, &list).await {
            list.push(k.clone());
            added.push(k);
        }
    }
    if added.is_empty() || keeper.gone.lock().contains(&job.session) {
        return;
    }
    keeper.lists.lock().insert(job.session.clone(), list.clone());
    let (h, session) = (hub.clone(), job.session.clone());
    match crate::rt::blocking(move || h.harness.keeper.save(&session, &list)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) | Err(e) => tracing::warn!(error = %format!("{e:#}"), "couldn't keep a conversation's list of files"),
    }
    hub.harness.files_added(&job.session, &added);
}

/// One mention: the file as it is now, copied when it fits. `None` when there's nothing to say
/// yet (its runner doesn't read files, or its machine is away: it's read when someone looks).
async fn keep_one(hub: &Arc<Hub>, job: &Job, path: &str, list: &[Kept]) -> Option<Kept> {
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
    let conversation: u64 = list.iter().filter(|k| k.sha.is_some()).filter_map(|k| k.meta.as_ref().map(|m| m.size)).sum();
    let fits = meta.size <= MOST_KEPT && conversation.saturating_add(meta.size) <= MOST_PER_SESSION && keeper.used.load(Ordering::Relaxed).saturating_add(meta.size) <= MOST_ALL;
    if !fits {
        return Some(entry(Some(meta), None, None));
    }
    match copy(hub, &job.location, &job.session, &job.seq.to_string(), path, &meta, first).await {
        Ok(sha) => Some(entry(Some(meta), Some(sha), None)),
        Err(e) => {
            tracing::debug!(session = %job.session, path, error = %format!("{e:#}"), "a file a message mentions couldn't be kept; it's read when someone looks");
            Some(entry(Some(meta), None, None))
        }
    }
}

/// The whole file into the store, a piece at a time (never all of it in memory), checking it
/// doesn't change on the way; its SHA-256.
async fn copy(hub: &Arc<Hub>, location: &Location, session: &str, tag: &str, path: &str, meta: &Meta, first: Vec<u8>) -> anyhow::Result<String> {
    let blobs = hub.harness.keeper.dir.join("blobs");
    let tmp = blobs.join(format!("{}-{tag}-{}.part", file_name(session), crate::util::now_ms()));
    let (b, t) = (blobs.clone(), tmp.clone());
    let file = crate::rt::blocking(move || -> anyhow::Result<std::fs::File> {
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
            file = crate::rt::blocking(move || -> anyhow::Result<std::fs::File> {
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
    let fresh = crate::rt::blocking(move || -> anyhow::Result<bool> {
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
    fn lists_are_kept_found_and_forgotten() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-kept-{}", std::process::id()));
        let k = Keeper::new(&dir);
        let sha = "a".repeat(64);
        let list = vec![
            Kept { path: "/tmp/a.png".into(), seq: 3, meta: Some(Meta { size: 10, mtime: 1, mime: "image/png".into(), width: Some(2), height: Some(1) }), sha: Some(sha.clone()), error: None },
            Kept { path: "/tmp/gone.png".into(), seq: 4, meta: None, sha: None, error: Some("/tmp/gone.png isn't there any more".into()) },
        ];
        k.save("s1", &list)?;
        // A fresh keeper (a restarted server) reads it back.
        let k2 = Keeper::new(&dir);
        assert_eq!(k2.list("s1"), list);
        assert_eq!(k2.find("s1", &sha.to_uppercase()).map(|f| f.seq), Some(3));
        assert!(k2.lists_path("s1", "/tmp/gone.png") && !k2.lists_path("s1", "/tmp/other.png"));
        assert!(k2.frame("s1").is_some_and(|f| f.contains("\"t\":\"files\"")));
        assert_eq!(k2.blob("not-a-sha"), None);
        k2.forget("s1");
        assert!(k2.list("s1").is_empty() && !dir.join("files/s1.json").exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn a_sweep_clears_what_no_list_names() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-sweep-{}", std::process::id()));
        let k = Keeper::new(&dir);
        let blobs = dir.join("files/blobs");
        std::fs::create_dir_all(&blobs)?;
        let (named, unnamed) = ("b".repeat(64), "c".repeat(64));
        std::fs::write(blobs.join(&named), b"0123456789")?;
        std::fs::write(blobs.join(&unnamed), b"01234")?;
        k.save("s2", &[Kept { path: "/tmp/b.png".into(), seq: 1, meta: None, sha: Some(named.clone()), error: None }])?;
        let page = "d".repeat(64);
        std::fs::write(blobs.join(&page), b"0123")?;
        assert_eq!(k.sweep(&HashSet::from([page.clone()])), 14);
        assert!(blobs.join(&named).exists() && blobs.join(&page).exists() && !blobs.join(&unnamed).exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
