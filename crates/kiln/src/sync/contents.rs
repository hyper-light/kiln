//! The server's copy of the contents synced repos' steps name (docs/sync.md), in kiln's store of
//! bytes (`crate::blobs`, docs/blobs.md) at `sync/<sha256>`: a runner fetches from it what its
//! copy lacks and sends it what its own steps name. Runners keep theirs as files
//! ([`super::blobs::Blobs`]): working copies, refilled from here.
//!
//! The sync thread asks whether the server holds a content, and how big it is, many times a step,
//! without waiting on the store: an index in memory answers, listed from the store when the server
//! starts ([`Contents::start`]) and kept as contents come and go. It holds at most [`INDEX_MAX`]
//! contents; past that a content sent is refused, typed, as a full disk refuses it. Contents kept as
//! files before (`<data>/sync/blobs/<2 hex>/<hash>`) are taken where they lie by the local store,
//! the first time it starts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kiln_seams::blobs::{Blobs, Cond, LIST_MAX, Refusal};
use parking_lot::{Condvar, Mutex};

use super::blobs::valid_hash;

/// The most contents the server holds.
pub const INDEX_MAX: usize = 2_000_000;
/// What a listing at start reads, at most (pages of 1000).
const PAGES_MAX: usize = INDEX_MAX / LIST_MAX + 1;
/// How long the sync thread waits for the index before it judges what's held.
pub const LOAD_WAIT: Duration = Duration::from_secs(300);
/// Marks the files kept before as taken in by the local store.
const TAKEN: &str = ".taken";

/// The key a content is kept at.
pub fn key(hash: &str) -> String {
    format!("sync/{hash}")
}

fn id(hash: &str) -> Option<[u8; 32]> {
    if !valid_hash(hash) {
        return None;
    }
    hex::decode(hash).ok()?.try_into().ok()
}

/// What the server holds, by content: its size and when it was kept (Unix milliseconds).
#[derive(Default)]
struct Index {
    held: HashMap<[u8; 32], (u64, i64)>,
    loaded: bool,
}

pub struct Contents {
    blobs: Arc<dyn Blobs>,
    index: Mutex<Index>,
    loaded_cv: Condvar,
    /// The listing at start ended short (the store unavailable): what's held is known only as it
    /// comes.
    partial: AtomicBool,
}

impl Contents {
    fn new(blobs: Arc<dyn Blobs>) -> Self {
        Self { blobs, index: Mutex::default(), loaded_cv: Condvar::new(), partial: AtomicBool::new(false) }
    }

    /// The contents in `blobs`, its index made in the background: files kept before under `legacy`
    /// taken in first, where `local` is the store.
    pub fn start(opened: &crate::blobs::Opened, legacy: PathBuf) -> Arc<Self> {
        let me = Arc::new(Self::new(Arc::clone(&opened.blobs)));
        let (c, local) = (Arc::clone(&me), opened.local.clone());
        crate::rt::spawn(async move {
            if let Some(local) = local
                && let Err(e) = take_in(&local, &legacy).await
            {
                tracing::warn!(target: "kiln.blobs", event = "adopt", error = %format!("{e:#}"), "sync's contents from before couldn't be taken in; runners send again what's wanted");
            }
            c.load().await;
        });
        me
    }

    /// Contents with nothing held and nothing to list: a test's.
    pub fn empty(blobs: Arc<dyn Blobs>) -> Arc<Self> {
        let me = Self::new(blobs);
        me.index.lock().loaded = true;
        Arc::new(me)
    }

    /// Lists what the store holds into the index.
    async fn load(&self) {
        let began = Instant::now();
        let mut after: Option<String> = None;
        let mut seen = 0usize;
        for _ in 0..PAGES_MAX {
            let page = match self.blobs.list("sync/", after.as_deref(), LIST_MAX).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(target: "kiln.blobs", event = "list", error = %e, "sync's contents couldn't all be listed; the rest are known as they come");
                    self.partial.store(true, Ordering::Relaxed);
                    break;
                }
            };
            {
                let mut g = self.index.lock();
                for o in &page.objects {
                    if let Some(i) = o.key.strip_prefix("sync/").and_then(id)
                        && g.held.len() < INDEX_MAX
                    {
                        g.held.insert(i, (o.size, o.modified_ms));
                    }
                }
            }
            seen = seen.saturating_add(page.objects.len());
            after = page.objects.last().map(|o| o.key.clone());
            if !page.truncated || after.is_none() {
                break;
            }
        }
        self.index.lock().loaded = true;
        self.loaded_cv.notify_all();
        tracing::info!(target: "kiln.blobs", event = "list", contents = seen, ms = began.elapsed().as_millis(), "sync's contents listed");
    }

    /// Waits (blocking: the sync thread's) until the index is made, or `d` has passed.
    pub fn wait_loaded(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        let mut g = self.index.lock();
        while !g.loaded {
            if self.loaded_cv.wait_until(&mut g, deadline).timed_out() {
                return g.loaded;
            }
        }
        true
    }

    pub fn has(&self, hash: &str) -> bool {
        id(hash).is_some_and(|i| self.index.lock().held.contains_key(&i))
    }

    pub fn size(&self, hash: &str) -> Option<u64> {
        id(hash).and_then(|i| self.index.lock().held.get(&i).map(|(s, _)| *s))
    }

    /// Every content held and when it was kept (Unix milliseconds).
    pub fn all(&self) -> Vec<(String, i64)> {
        self.index.lock().held.iter().map(|(i, (_, at))| (hex::encode(i), *at)).collect()
    }

    fn note(&self, hash: &str, size: u64, at_ms: i64) -> Result<(), Refusal> {
        let i = id(hash).ok_or_else(|| Refusal::Invalid { what: "a content's name", why: "not a SHA-256".into() })?;
        let mut g = self.index.lock();
        if !g.held.contains_key(&i) && g.held.len() >= INDEX_MAX {
            return Err(Refusal::Full { what: "sync's contents", limit: u64::try_from(INDEX_MAX).unwrap_or(u64::MAX) });
        }
        g.held.insert(i, (size, at_ms));
        Ok(())
    }

    /// Keeps the file at `path` as content `hash`, once it hashes to it (else `BadChecksum`).
    pub async fn put_file(&self, hash: &str, path: &Path) -> Result<(), Refusal> {
        if id(hash).is_none() {
            return Err(Refusal::Invalid { what: "a content's name", why: "not a SHA-256".into() });
        }
        if !self.has(hash) && self.index.lock().held.len() >= INDEX_MAX {
            return Err(Refusal::Full { what: "sync's contents", limit: u64::try_from(INDEX_MAX).unwrap_or(u64::MAX) });
        }
        let meta = crate::blobs::io::put_file(self.blobs.as_ref(), &key(hash), path, Cond::Always, Some(hash)).await?;
        self.note(hash, meta.size, meta.modified_ms)
    }

    /// Keeps `bytes` as content: its hash.
    pub async fn put_bytes(&self, bytes: Vec<u8>) -> Result<String, Refusal> {
        let hash = crate::util::sha256_hex(&bytes);
        let put = kiln_seams::blobs::Put { cond: Cond::Always, sha256: Some(hash.clone()) };
        let meta = self.blobs.put(&key(&hash), bytes, put).await?;
        self.note(&hash, meta.size, meta.modified_ms)?;
        Ok(hash)
    }

    /// Drops a content nothing needs any more: from the index now, from the store as soon as it
    /// answers.
    pub fn remove(&self, hash: &str) {
        let Some(i) = id(hash) else { return };
        if self.index.lock().held.remove(&i).is_none() {
            return;
        }
        let (blobs, k) = (Arc::clone(&self.blobs), key(hash));
        crate::rt::spawn(async move {
            match blobs.delete(&k).await {
                Ok(()) => crate::metrics::blobs().collected.inc(&["sync"]),
                Err(e) => tracing::warn!(target: "kiln.blobs", event = "collect", key = %k, error = %e, "a content nothing needs couldn't be deleted"),
            }
        });
    }

    /// Content `hash` as a stream, with its size (`None`: not held).
    pub fn stream(&self, hash: &str) -> Option<(u64, async_channel::Receiver<Bytes>)> {
        let size = self.size(hash)?;
        if size == 0 {
            let (_, rx) = async_channel::bounded(1);
            return Some((0, rx));
        }
        Some((size, crate::blobs::io::read_stream(Arc::clone(&self.blobs), key(hash), 0, size.saturating_sub(1))))
    }
}

/// Takes the contents kept as files before (`<dir>/<2 hex>/<hash>`) into the local store where they
/// lie, once: a `.taken` mark says it's done.
async fn take_in(local: &crate::blobs::local::LocalBlobs, dir: &Path) -> anyhow::Result<()> {
    let mark = dir.join(TAKEN);
    if mark.exists() || !dir.is_dir() {
        return Ok(());
    }
    let d = dir.to_path_buf();
    let files = crate::rt::blocking(move || -> std::io::Result<Vec<(String, PathBuf, Option<String>)>> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(&d)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for e in std::fs::read_dir(shard.path())? {
                let e = e?;
                let name = e.file_name().to_string_lossy().into_owned();
                if valid_hash(&name) {
                    out.push((key(&name), e.path(), Some(name)));
                }
                if out.len() >= INDEX_MAX {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    })
    .await??;
    let n = files.len();
    let taken = local.adopt(files).await?;
    let m = mark.clone();
    crate::rt::blocking(move || std::fs::write(&m, format!("{taken} of {n} taken\n"))).await??;
    tracing::info!(target: "kiln.blobs", event = "adopt", taken, files = n, "sync's contents from before taken in where they lie");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP-T10.1: contents kept as files before are the store's where they lie; what's put comes
    /// into the index, and goes from it and the store when removed.
    #[test]
    fn contents_from_before_are_taken_in_and_new_ones_kept_in_the_store() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let local = hub.blobs.local.clone().ok_or_else(|| anyhow::anyhow!("not the local store"))?;
            let dir = tempfile::tempdir()?;
            let old = super::super::blobs::Blobs::new(&dir.path().join("blobs"))?;
            let hash = old.put_bytes(b"from before")?;
            let path = old.path(&hash).ok_or_else(|| anyhow::anyhow!("no path"))?;
            take_in(&local, &dir.path().join("blobs")).await?;
            assert!(dir.path().join("blobs").join(TAKEN).exists());
            let c = Contents::empty(Arc::clone(&hub.blobs.blobs));
            c.load().await;
            assert_eq!(c.size(&hash), Some(11), "listed from the store");
            assert!(path.exists(), "where it lay");
            let (size, rx) = c.stream(&hash).ok_or_else(|| anyhow::anyhow!("not held"))?;
            let mut got = Vec::new();
            while let Ok(b) = rx.recv().await {
                got.extend_from_slice(&b);
            }
            assert_eq!((size, got), (11, b"from before".to_vec()));
            let new = c.put_bytes(b"new".to_vec()).await?;
            assert!(c.has(&new));
            let f = dir.path().join("wrong");
            std::fs::write(&f, b"not what it says")?;
            assert!(matches!(c.put_file(&new, &f).await, Err(Refusal::BadChecksum { .. })));
            c.remove(&hash);
            assert!(!c.has(&hash));
            for _ in 0..200 {
                if !path.exists() {
                    break;
                }
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            assert!(!path.exists(), "a content let go goes from where it lay");
            Ok(())
        })
    }
}
