//! kiln's local store: kiln-seams' `Blobs` over its content-addressed store (CP-T10.1,
//! docs/blobs.md). A key names an object of the content-addressed store (`cas_objects`, its chunks
//! under `<data>/cas/chunks`, [`super::chunks`]) by its content's SHA-256: two keys with the same
//! content share it, and a new version of a large object shares the chunks it didn't change. Keys
//! and uploads are rows in kiln's store (`blob_keys`, `blob_uploads`, `blob_parts`), changed in
//! transactions, so a condition is judged and its write made in one step.
//!
//! What the content-addressed store held before keys are its objects at `cas/<digest>`, their data
//! where it was (the migration that made the tables keyed them). Files kept another way before
//! (sync's contents, the upgrade binaries) are taken where they lie ([`LocalBlobs::adopt`]): their
//! keys name the files, unchanged, until they're replaced or deleted.
//!
//! An object no key holds any more stays an hour ([`chunks::GRACE`]), so a read already under way
//! finishes; [`LocalBlobs::collect`] then lets it go, its chunks with it once no object uses them.
//! A part is cut into chunks as it arrives; its upload's SHA-256 is taken as parts come in order
//! (most uploads' way), else read back once at completion.
//!
//! An older kiln run on this store again (a rollback) keeps its objects in the same tables, and
//! lets go of those nothing of CI refers to. So an object a key other than `cas/<digest>` holds is
//! pinned there ([`PINNED`]): the older kiln never lets it go. When this release starts again,
//! [`LocalBlobs::reconcile`] takes in what the older kiln kept, and drops the keys whose bytes it
//! let go (docs/blobs.md, "Rolling back").

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kiln_seams::Clock;
use kiln_seams::blobs::{self as seam, Access, Blobs, Cond, Listed, Listing, Meta, Method, Put, Range, Refusal};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};

use super::chunks::{self, Chunks};
use crate::store::Store;

/// What the local store holds.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The largest object, and part.
    pub object: u64,
    /// Uploads under way at once.
    pub uploads: usize,
    /// The smallest a part but the last may be: S3's, so an upload works alike in every store.
    pub part_min: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { object: super::OBJECT_MAX, uploads: 1000, part_min: seam::PART_MIN }
    }
}

/// Where the local store's presigned URLs lead (`<base>/<key>`: kiln's `/api/blobs`), and the key
/// they're signed with.
#[derive(Clone)]
pub struct Presign {
    pub base: String,
    pub key: [u8; 32],
}

impl std::fmt::Debug for Presign {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presign").field("base", &self.base).field("key", &"<redacted>").finish()
    }
}

/// An upload left this long is let go by [`LocalBlobs::collect`].
pub const UPLOAD_TTL: Duration = Duration::from_secs(2 * 86_400);
/// Chunk files a collection removes at most.
const SWEEP_BUDGET: usize = 1_000_000;
/// Rows a taking-in of files where they lie writes a transaction.
const ADOPT_BATCH: usize = 1000;
/// An object's `last_used_at` while a key other than the CAS's holds it: later than any time an
/// older kiln lets objects go before, so it never does.
pub const PINNED: i64 = i64::MAX;
/// Rows [`LocalBlobs::reconcile`] reads a page.
const RECONCILE_PAGE: usize = 10_000;
/// Pages [`LocalBlobs::reconcile`] reads of each kind, at most.
const RECONCILE_PAGES: usize = 1000;

/// A key's object, as its row says.
struct Row {
    digest: String,
    size: u64,
    modified_ms: i64,
    file: Option<String>,
}

pub struct LocalBlobs {
    store: Store,
    root: PathBuf,
    clock: Arc<dyn Clock>,
    limits: Limits,
    presign: Presign,
    following: super::Following,
    /// [`LocalBlobs::reconcile`] has run this start.
    ready: async_lock::OnceCell<()>,
}

/// What a collection let go.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Collected {
    pub uploads: usize,
    pub objects: usize,
    pub chunks: usize,
    pub bytes: u64,
}

/// What a taking-in of files where they lie did ([`LocalBlobs::adopt`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Adopted {
    pub taken: usize,
    /// Files whose content their key holds already another way (in chunks, or another file): their
    /// caller's to remove.
    pub redundant: Vec<PathBuf>,
}

/// What an older kiln run on this store did, as [`LocalBlobs::reconcile`] found it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reconciled {
    /// Objects it kept, or used after this store let them go: keys at `cas/<digest>` now.
    pub taken: usize,
    /// Keys whose object it let go, or whose file it removed or replaced: gone.
    pub dropped: usize,
}

fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn i64_of(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn unavailable(e: impl std::fmt::Display) -> Refusal {
    Refusal::Unavailable(e.to_string())
}

fn row(c: &Connection, key: &str) -> rusqlite::Result<Option<Row>> {
    c.query_row("SELECT digest, size, modified_ms, file FROM blob_keys WHERE key = ?1", [key], |r| {
        Ok(Row { digest: r.get(0)?, size: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0), modified_ms: r.get(2)?, file: r.get(3)? })
    })
    .optional()
}

fn meta_of(key: &str, r: &Row) -> Meta {
    Meta { key: key.to_owned(), size: r.size, etag: r.digest.clone(), modified_ms: r.modified_ms }
}

/// The key kiln's CAS keeps an object at (`crate::ci::services::cas::key`): the only objects an
/// older kiln knows.
fn cas_key(key: &str) -> bool {
    key.starts_with("cas/")
}

/// A file's modification time, Unix milliseconds: a file taken where it lies is the one taken while
/// its time and size are.
pub(crate) fn modified_ms(md: &std::fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Whether the file at `path` is still the one taken there.
fn same_file(path: &str, size: u64, modified: i64) -> bool {
    std::fs::metadata(path).is_ok_and(|md| md.is_file() && md.len() == size && modified_ms(&md) == modified)
}

/// What a key let go of. Held still by a key other than the CAS's, it stays pinned; else an older
/// kiln may let it go as its own (used now); and once no key holds it, it's released now, for
/// [`LocalBlobs::collect`] an hour later.
fn released(c: &Connection, digest: &str, now_s: i64) -> rusqlite::Result<usize> {
    c.execute(
        "UPDATE cas_objects SET last_used_at = ?2,
           released_at = CASE WHEN EXISTS (SELECT 1 FROM blob_keys WHERE digest = ?1) THEN NULL ELSE ?2 END
         WHERE digest = ?1 AND NOT EXISTS (SELECT 1 FROM blob_keys WHERE digest = ?1 AND substr(key, 1, 4) <> 'cas/')",
        params![digest, now_s],
    )
}

/// A new object for a key.
struct Write {
    key: String,
    digest: String,
    size: u64,
    chunks: Chunks,
    cond: Cond,
    now_ms: i64,
    /// The upload it completes, gone with it.
    upload: Option<String>,
}

/// In one transaction: the upload it completes still there, the condition judged against what's
/// at the key, the object recorded (pinned, unless the key is the CAS's), the key pointed at it,
/// and what the key held released (its object kept an hour for reads under way). The object's
/// answer, and a file the key named before (to remove).
fn commit(c: &mut Connection, w: &Write) -> anyhow::Result<Result<(Meta, Option<String>), Refusal>> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(up) = &w.upload {
        let there: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM blob_uploads WHERE id = ?1)", [up], |r| r.get(0))?;
        if !there {
            return Ok(Err(Refusal::NoSuchUpload(up.clone())));
        }
    }
    let old = row(&tx, &w.key)?;
    if let Err(r) = seam::holds(&w.cond, &w.key, old.as_ref().map(|o| o.digest.as_str())) {
        return Ok(Err(r));
    }
    let now_s = w.now_ms.div_euclid(1000);
    for (d, n) in &w.chunks {
        tx.execute("INSERT OR IGNORE INTO cas_chunks (digest, size) VALUES (?1, ?2)", params![d, i64_of(*n)])?;
    }
    let used = if cas_key(&w.key) { now_s } else { PINNED };
    tx.execute(
        "INSERT INTO cas_objects (digest, size, chunks, created_at, last_used_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (digest) DO UPDATE SET last_used_at = MAX(last_used_at, excluded.last_used_at), released_at = NULL",
        params![w.digest, i64_of(w.size), serde_json::to_string(&w.chunks)?, now_s, used],
    )?;
    tx.execute(
        "INSERT INTO blob_keys (key, digest, size, modified_ms, file) VALUES (?1, ?2, ?3, ?4, NULL)
         ON CONFLICT (key) DO UPDATE SET digest = excluded.digest, size = excluded.size, modified_ms = excluded.modified_ms, file = NULL",
        params![w.key, w.digest, i64_of(w.size), w.now_ms],
    )?;
    if let Some(o) = old.as_ref().filter(|o| o.file.is_none() && o.digest != w.digest) {
        released(&tx, &o.digest, now_s)?;
    }
    if let Some(up) = &w.upload {
        tx.execute("DELETE FROM blob_parts WHERE upload = ?1", [up])?;
        tx.execute("DELETE FROM blob_uploads WHERE id = ?1", [up])?;
    }
    tx.commit()?;
    let meta = Meta { key: w.key.clone(), size: w.size, etag: w.digest.clone(), modified_ms: w.now_ms };
    Ok(Ok((meta, old.and_then(|o| o.file))))
}

/// Bytes `first` to `last` of a whole file kept where it lay.
fn read_file(path: &str, first: u64, last: u64) -> io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(io::SeekFrom::Start(first))?;
    let want = usize::try_from(last.saturating_sub(first).saturating_add(1)).map_err(|_| io::Error::other("a range past what this machine addresses"))?;
    let mut out = Vec::with_capacity(want);
    f.take(len64(want)).read_to_end(&mut out)?;
    if out.len() != want {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{path} holds {} of the {want} bytes asked", out.len())));
    }
    Ok(out)
}

fn remove_if_there(path: &str) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A presigned URL's signature: HMAC-SHA256 under `key` of what it covers (its method, its exact
/// key, its expiry and, for a put, its body's length: `seam::signed`).
fn signature(key: &[u8; 32], access: Access, object: &str, expires_ms: i64) -> String {
    crate::util::hmac_hex(key, seam::signed(access, object, expires_ms).as_bytes())
}

impl LocalBlobs {
    pub fn new(store: Store, root: PathBuf, clock: Arc<dyn Clock>, limits: Limits, presign: Presign) -> Self {
        Self { store, root, clock, limits, presign, following: super::Following::default(), ready: async_lock::OnceCell::new() }
    }

    /// Waits for [`LocalBlobs::reconcile`] to have run this start; the first to ask runs it.
    pub async fn ready(&self) {
        self.ready
            .get_or_init(|| async {
                match self.reconcile().await {
                    Ok(r) if r.taken + r.dropped > 0 => {
                        tracing::warn!(target: "kiln.blobs", event = "adopt", taken = r.taken, dropped = r.dropped, "an older kiln ran on the local store: what it kept is taken in, and keys whose bytes it let go are gone (sync's runners send those again; an upgrade binary is fetched or uploaded again)");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(target: "kiln.blobs", event = "adopt", error = %format!("{e:#}"), "what an older kiln did to the local store couldn't all be taken in"),
                }
            })
            .await;
    }

    /// Takes in what an older kiln did to this store while it ran on it again (a rollback, then
    /// this release again): objects it kept, or used after this store let them go, become keys at
    /// `cas/<digest>` (and records of kiln's CAS, `cas_index`, as the migration that made the
    /// tables did); keys whose object it let go, or whose file it removed or replaced, go. Run once
    /// a start, before anything reads the store ([`LocalBlobs::ready`]).
    pub async fn reconcile(&self) -> anyhow::Result<Reconciled> {
        let mut out = Reconciled::default();
        let page = i64::try_from(RECONCILE_PAGE).unwrap_or(i64::MAX);
        for _ in 0..RECONCILE_PAGES {
            let (found, taken, dropped) = self
                .store
                .call(move |c| {
                    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let gone: Vec<(String, String)> = {
                        let mut q = tx.prepare("SELECT key, digest FROM blob_keys WHERE file IS NULL AND digest NOT IN (SELECT digest FROM cas_objects) LIMIT ?1")?;
                        q.query_map([page], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
                    };
                    for (key, digest) in &gone {
                        tx.execute("DELETE FROM blob_keys WHERE key = ?1", [key])?;
                        if *key == format!("cas/{digest}") {
                            tx.execute("DELETE FROM cas_index WHERE digest = ?1", [digest])?;
                        }
                    }
                    let found: Vec<(String, i64, i64, i64)> = {
                        let mut q = tx.prepare(
                            "SELECT digest, size, created_at, last_used_at FROM cas_objects
                             WHERE digest NOT IN (SELECT digest FROM blob_keys) AND (released_at IS NULL OR last_used_at > released_at) LIMIT ?1",
                        )?;
                        q.query_map([page], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
                    };
                    let mut taken = 0usize;
                    for (digest, size, created, used) in &found {
                        let n = tx.execute(
                            "INSERT OR IGNORE INTO blob_keys (key, digest, size, modified_ms, file) VALUES ('cas/' || ?1, ?1, ?2, ?3, NULL)",
                            params![digest, size, created.saturating_mul(1000)],
                        )?;
                        if n == 0 {
                            continue;
                        }
                        taken = taken.saturating_add(n);
                        tx.execute(
                            "INSERT INTO cas_index (digest, size, created_at, last_used_at) VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT (digest) DO UPDATE SET last_used_at = MAX(last_used_at, excluded.last_used_at)",
                            params![digest, size, created, used],
                        )?;
                        tx.execute("UPDATE cas_objects SET released_at = NULL WHERE digest = ?1", [digest])?;
                    }
                    tx.commit()?;
                    Ok((found.len(), taken, gone.len()))
                })
                .await?;
            out.taken = out.taken.saturating_add(taken);
            out.dropped = out.dropped.saturating_add(dropped);
            if (found < RECONCILE_PAGE && dropped < RECONCILE_PAGE) || taken.saturating_add(dropped) == 0 {
                break;
            }
        }
        // Keys naming files: each file still the one taken, a page at a time.
        let mut after = String::new();
        for _ in 0..RECONCILE_PAGES {
            let a = after.clone();
            let files: Vec<(String, String, i64, i64)> = self
                .store
                .call(move |c| {
                    let mut q = c.prepare("SELECT key, file, size, modified_ms FROM blob_keys WHERE file IS NOT NULL AND key > ?1 ORDER BY key LIMIT ?2")?;
                    Ok(q.query_map(params![a, page], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?)
                })
                .await?;
            let Some((last, ..)) = files.last() else { break };
            after = last.clone();
            let full = files.len() >= RECONCILE_PAGE;
            let changed: Vec<(String, String)> = crate::rt::blocking(move || {
                files.into_iter().filter(|(_, file, size, modified)| !same_file(file, u64::try_from(*size).unwrap_or(0), *modified)).map(|(key, file, ..)| (key, file)).collect()
            })
            .await?;
            if !changed.is_empty() {
                let dropped = self
                    .store
                    .call(move |c| {
                        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                        let mut n = 0usize;
                        for (key, file) in &changed {
                            n = n.saturating_add(tx.execute("DELETE FROM blob_keys WHERE key = ?1 AND file = ?2", params![key, file])?);
                        }
                        tx.commit()?;
                        Ok(n)
                    })
                    .await?;
                out.dropped = out.dropped.saturating_add(dropped);
            }
            if !full {
                break;
            }
        }
        Ok(out)
    }

    /// `<data>/cas`: where the chunks are.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    async fn sql<R, F>(&self, f: F) -> Result<R, Refusal>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<Result<R, Refusal>> + Send + 'static,
        R: Send + 'static,
    {
        match self.store.call(f).await {
            Ok(r) => r,
            Err(e) => Err(Refusal::Unavailable(format!("kiln's store: {e:#}"))),
        }
    }

    async fn disk<R, F>(f: F) -> Result<R, Refusal>
    where
        F: FnOnce() -> io::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        match crate::rt::blocking(f).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(Refusal::Unavailable(format!("the disk: {e}"))),
            Err(e) => Err(unavailable(format!("{e:#}"))),
        }
    }

    /// Removes a file a replaced or deleted key named, where it lay.
    async fn release(file: Option<String>) {
        if let Some(f) = file
            && let Err(e) = Self::disk(move || remove_if_there(&f)).await
        {
            tracing::warn!(target: "kiln.blobs", event = "refused", error = %e, "a file a key no longer names couldn't be removed");
        }
    }

    /// Checks a presigned URL used with `method`: its key, what it grants (a put, the length of
    /// its body) and its signature, in constant time, before anything is read or written; then its
    /// expiry. Its key, and what it grants.
    pub fn check_presigned(&self, encoded_key: &str, query: &str, method: Method) -> Result<(String, Access), Refusal> {
        let field = |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(name).and_then(|v| v.strip_prefix('='))).map(str::to_owned);
        let key = seam::decode_key(encoded_key).ok_or_else(|| Refusal::Forbidden("a bad key".into()))?;
        seam::check_key(&key).map_err(|_| Refusal::Forbidden("a bad key".into()))?;
        let expires_ms: i64 = field("expires").and_then(|e| e.parse().ok()).ok_or_else(|| Refusal::Forbidden("no expiry".into()))?;
        let access = match method {
            Method::Get => Access::Get,
            Method::Put => Access::Put { length: field("length").and_then(|l| l.parse().ok()).ok_or_else(|| Refusal::Forbidden("no length".into()))? },
        };
        let sig = field("sig").ok_or_else(|| Refusal::Forbidden("no signature".into()))?;
        if !crate::util::ct_eq(sig.as_bytes(), signature(&self.presign.key, access, &key, expires_ms).as_bytes()) {
            return Err(Refusal::Forbidden("the signature doesn't match".into()));
        }
        // A URL lasts an hour at the most, however its expiry was made.
        let longest = i64::try_from(seam::PRESIGN_MAX.as_millis()).unwrap_or(i64::MAX);
        let now = self.clock.unix_ms();
        if now >= expires_ms || expires_ms.saturating_sub(now) > longest {
            return Err(Refusal::Expired);
        }
        Ok((key, access))
    }

    /// Uses one of this store's presigned URLs as its holder would (a get's bytes; a put stores
    /// `body`): what `/api/blobs` does, without the HTTP.
    pub async fn redeem(&self, url: &str, method: Method, body: Option<Vec<u8>>) -> Result<Option<Vec<u8>>, Refusal> {
        let rest = url.strip_prefix(&format!("{}/", self.presign.base)).ok_or_else(|| Refusal::Forbidden("not one of this store's URLs".into()))?;
        let (path, query) = rest.split_once('?').ok_or_else(|| Refusal::Forbidden("no signature".into()))?;
        let (key, access) = self.check_presigned(path, query, method)?;
        match access {
            Access::Get => self.get(&key, None).await.map(|(_, b)| Some(b)),
            Access::Put { length } => {
                let body = body.unwrap_or_default();
                if len64(body.len()) != length {
                    return Err(Refusal::Forbidden(format!("the body is {} bytes; its URL takes {length}", body.len())));
                }
                self.put(&key, body, Put::default()).await.map(|_| None)
            }
        }
    }

    /// Takes files kept before the store had keys as its objects, where they lie: each of
    /// `files` (its key, its path, and its content's SHA-256, `None` to read it) unless its key
    /// already names something.
    pub async fn adopt(&self, files: Vec<(String, PathBuf, Option<String>)>) -> anyhow::Result<Adopted> {
        let mut out = Adopted::default();
        for batch in files.chunks(ADOPT_BATCH) {
            let batch = batch.to_vec();
            let rows = crate::rt::blocking(move || -> anyhow::Result<Vec<(String, String, u64, i64, String)>> {
                let mut out = Vec::with_capacity(batch.len());
                for (key, path, digest) in batch {
                    seam::check_key(&key).map_err(|e| anyhow::anyhow!("{key}: {e}"))?;
                    let Ok(md) = std::fs::metadata(&path) else { continue };
                    if !md.is_file() {
                        continue;
                    }
                    let digest = match digest {
                        Some(d) => d.to_ascii_lowercase(),
                        None => crate::sync::blobs::hash_file(&path)?,
                    };
                    out.push((key, digest, md.len(), modified_ms(&md), path.to_string_lossy().into_owned()));
                }
                Ok(out)
            })
            .await??;
            let (taken, redundant) = self
                .store
                .call(move |c| {
                    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let (mut n, mut redundant) = (0usize, Vec::new());
                    for (key, digest, size, modified, path) in &rows {
                        let took = tx.execute(
                            "INSERT OR IGNORE INTO blob_keys (key, digest, size, modified_ms, file) VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![key, digest, i64_of(*size), modified, path],
                        )?;
                        n = n.saturating_add(took);
                        if took == 0 && row(&tx, key)?.is_some_and(|r| r.digest == *digest && r.file.as_deref() != Some(path.as_str())) {
                            redundant.push(PathBuf::from(path));
                        }
                    }
                    tx.commit()?;
                    Ok((n, redundant))
                })
                .await?;
            out.taken = out.taken.saturating_add(taken);
            out.redundant.extend(redundant);
        }
        Ok(out)
    }

    /// Lets go of uploads left [`UPLOAD_TTL`], objects this store released over an hour ago that
    /// nothing has used since, and chunks no object or part uses (once an hour old: a write may be
    /// about to name them). What an older kiln kept is [`LocalBlobs::reconcile`]'s.
    pub async fn collect(&self) -> anyhow::Result<Collected> {
        let now_ms = self.clock.unix_ms();
        let stale_before = now_ms.saturating_sub(i64::try_from(UPLOAD_TTL.as_millis()).unwrap_or(i64::MAX));
        let released_before = now_ms.div_euclid(1000).saturating_sub(i64::try_from(chunks::GRACE.as_secs()).unwrap_or(3600));
        let (stale, objects, used) = self
            .store
            .call(move |c| {
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let stale: Vec<String> = {
                    let mut q = tx.prepare("SELECT id FROM blob_uploads WHERE started_ms < ?1")?;
                    q.query_map([stale_before], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?
                };
                for id in &stale {
                    tx.execute("DELETE FROM blob_parts WHERE upload = ?1", [id])?;
                    tx.execute("DELETE FROM blob_uploads WHERE id = ?1", [id])?;
                }
                let objects = tx.execute(
                    "DELETE FROM cas_objects WHERE released_at < ?1 AND last_used_at <= released_at AND digest NOT IN (SELECT digest FROM blob_keys)",
                    [released_before],
                )?;
                let mut used = HashSet::new();
                for sql in ["SELECT chunks FROM cas_objects", "SELECT chunks FROM blob_parts"] {
                    let mut q = tx.prepare(sql)?;
                    for list in q.query_map([], |r| r.get::<_, String>(0))? {
                        let list: Chunks = serde_json::from_str(&list?).unwrap_or_default();
                        used.extend(list.into_iter().map(|(d, _)| d));
                    }
                }
                tx.commit()?;
                Ok((stale, objects, used))
            })
            .await?;
        for id in &stale {
            self.following.forget(id);
        }
        let root = self.root.clone();
        let swept = crate::rt::blocking(move || chunks::sweep(&root, &used, SWEEP_BUDGET)).await?;
        let gone = swept.digests.clone();
        self.store
            .call(move |c| {
                let tx = c.transaction()?;
                for d in &gone {
                    tx.execute("DELETE FROM cas_chunks WHERE digest = ?1", [d])?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        Ok(Collected { uploads: stale.len(), objects, chunks: swept.chunks, bytes: swept.bytes })
    }

    /// The bytes the store keeps on its disk: every chunk once.
    pub async fn stored_bytes(&self) -> anyhow::Result<u64> {
        self.store.call(|c| Ok(c.query_row("SELECT COALESCE(SUM(size), 0) FROM cas_chunks", [], |r| r.get::<_, i64>(0)).map(|n| u64::try_from(n).unwrap_or(0))?)).await
    }

}

impl Blobs for LocalBlobs {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let size = len64(bytes.len());
            if size > self.limits.object {
                return Err(Refusal::TooLarge { what: "an object", size, limit: self.limits.object });
            }
            if let Some(want) = &put.sha256 {
                seam::check_sha256(want)?;
            }
            let (root, want) = (self.root.clone(), put.sha256.clone());
            let kept = Self::disk(move || {
                let digest = hex::encode(Sha256::digest(&bytes));
                if let Some(w) = want.filter(|w| !w.eq_ignore_ascii_case(&digest)) {
                    return Ok(Err(Refusal::BadChecksum { want: w, got: digest }));
                }
                let (chunks, _) = chunks::keep_all(&root, &bytes)?;
                Ok(Ok((digest, chunks)))
            })
            .await??;
            let w = Write { key: key.to_owned(), digest: kept.0, size, chunks: kept.1, cond: put.cond, now_ms: self.clock.unix_ms(), upload: None };
            let (meta, released) = self.sql(move |c| commit(c, &w)).await?;
            Self::release(released).await;
            Ok(meta)
        })
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let k = key.to_owned();
            let (r, chunks) = self
                .sql(move |c| {
                    let Some(r) = row(c, &k)? else { return Ok(Err(Refusal::NotFound(k))) };
                    let chunks: Option<String> = match r.file {
                        Some(_) => None,
                        None => c.query_row("SELECT chunks FROM cas_objects WHERE digest = ?1", [&r.digest], |x| x.get(0)).optional()?,
                    };
                    Ok(Ok((r, chunks)))
                })
                .await?;
            let meta = meta_of(key, &r);
            let (first, last) = match range {
                Some(range) => seam::byte_range(range, meta.size)?,
                None if meta.size == 0 => return Ok((meta, Vec::new())),
                None => (0, meta.size.saturating_sub(1)),
            };
            let (root, whole, etag) = (self.root.clone(), range.is_none(), meta.etag.clone());
            let bytes = Self::disk(move || {
                let data = match (r.file, chunks) {
                    (Some(f), _) => read_file(&f, first, last)?,
                    (None, Some(list)) => chunks::read(&root, &serde_json::from_str::<Chunks>(&list).map_err(io::Error::other)?, first, last)?,
                    (None, None) => return Err(io::Error::other("its object isn't in the content-addressed store")),
                };
                // Read whole, it's checked against its name.
                if whole && hex::encode(Sha256::digest(&data)) != etag {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "damaged: its bytes don't hash to its etag"));
                }
                Ok(data)
            })
            .await?;
            Ok((meta, bytes))
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let k = key.to_owned();
            self.sql(move |c| Ok(row(c, &k)?.map(|r| meta_of(&k, &r)).ok_or_else(|| Refusal::NotFound(k.clone())))).await
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let (k, now_s) = (key.to_owned(), self.clock.unix_ms().div_euclid(1000));
            let released = self
                .sql(move |c| {
                    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let old = row(&tx, &k)?;
                    tx.execute("DELETE FROM blob_keys WHERE key = ?1", [&k])?;
                    if let Some(o) = old.as_ref().filter(|o| o.file.is_none()) {
                        released(&tx, &o.digest, now_s)?;
                    }
                    tx.commit()?;
                    Ok(Ok(old.and_then(|o| o.file)))
                })
                .await?;
            Self::release(released).await;
            Ok(())
        })
    }

    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Listing, Refusal>> {
        Box::pin(async move {
            let limit = seam::list_limit(limit)?;
            let (sql, from) = match seam::list_from(prefix, after) {
                std::ops::Bound::Excluded(a) => ("SELECT key, size, modified_ms FROM blob_keys WHERE key > ?1 ORDER BY key LIMIT ?2", a.to_owned()),
                std::ops::Bound::Included(p) => ("SELECT key, size, modified_ms FROM blob_keys WHERE key >= ?1 ORDER BY key LIMIT ?2", p.to_owned()),
                std::ops::Bound::Unbounded => ("SELECT key, size, modified_ms FROM blob_keys WHERE key >= ?1 ORDER BY key LIMIT ?2", String::new()),
            };
            let p = prefix.to_owned();
            self.sql(move |c| {
                let mut q = c.prepare(sql)?;
                let rows = q.query_map(params![from, i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
                let (mut objects, mut truncated) = (Vec::new(), false);
                for r in rows {
                    let (key, size, modified_ms) = r?;
                    if !key.starts_with(&p) {
                        break;
                    }
                    if objects.len() >= limit {
                        truncated = true;
                        break;
                    }
                    objects.push(Listed { key, size: u64::try_from(size).unwrap_or(0), modified_ms });
                }
                Ok(Ok(Listing { objects, truncated }))
            })
            .await
        })
    }

    fn start_upload<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let id = format!("up-{}", hex::encode(rand::random::<[u8; 16]>()));
            let (k, i, now, most) = (key.to_owned(), id.clone(), self.clock.unix_ms(), self.limits.uploads);
            self.sql(move |c| {
                let n: i64 = c.query_row("SELECT COUNT(*) FROM blob_uploads", [], |r| r.get(0))?;
                if usize::try_from(n).unwrap_or(usize::MAX) >= most {
                    return Ok(Err(Refusal::Full { what: "uploads", limit: len64(most) }));
                }
                c.execute("INSERT INTO blob_uploads (id, key, started_ms) VALUES (?1, ?2, ?3)", params![i, k, now])?;
                Ok(Ok(()))
            })
            .await?;
            self.following.start(&id);
            Ok(id)
        })
    }

    fn upload_part<'a>(&'a self, upload: &'a str, part: u32, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            seam::check_part(part)?;
            let size = len64(bytes.len());
            if size > self.limits.object {
                return Err(Refusal::TooLarge { what: "a part", size, limit: self.limits.object });
            }
            let up = upload.to_owned();
            self.sql(move |c| {
                let there: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM blob_uploads WHERE id = ?1)", [&up], |r| r.get(0))?;
                Ok(if there { Ok(()) } else { Err(Refusal::NoSuchUpload(up)) })
            })
            .await?;
            let bytes = Arc::new(bytes);
            self.following.part(upload, part, Arc::clone(&bytes)).await;
            let root = self.root.clone();
            let (etag, chunks) = Self::disk(move || {
                let etag = hex::encode(Sha256::digest(bytes.as_slice()));
                let (chunks, _) = chunks::keep_all(&root, &bytes)?;
                Ok((etag, chunks))
            })
            .await?;
            let (up, e) = (upload.to_owned(), etag.clone());
            self.sql(move |c| {
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let there: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM blob_uploads WHERE id = ?1)", [&up], |r| r.get(0))?;
                if !there {
                    return Ok(Err(Refusal::NoSuchUpload(up)));
                }
                tx.execute(
                    "INSERT INTO blob_parts (upload, part, etag, size, chunks) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (upload, part) DO UPDATE SET etag = excluded.etag, size = excluded.size, chunks = excluded.chunks",
                    params![up, part, e, i64_of(size), serde_json::to_string(&chunks)?],
                )?;
                tx.commit()?;
                Ok(Ok(()))
            })
            .await?;
            Ok(etag)
        })
    }

    fn complete_upload<'a>(&'a self, upload: &'a str, parts: Vec<(u32, String)>) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            let up = upload.to_owned();
            let (key, have) = self
                .sql(move |c| {
                    let Some(key) = c.query_row("SELECT key FROM blob_uploads WHERE id = ?1", [&up], |r| r.get::<_, String>(0)).optional()? else {
                        return Ok(Err(Refusal::NoSuchUpload(up)));
                    };
                    let mut q = c.prepare("SELECT part, etag, size, chunks FROM blob_parts WHERE upload = ?1")?;
                    let mut have: BTreeMap<u32, (String, u64, Chunks)> = BTreeMap::new();
                    for r in q.query_map([&up], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?)))? {
                        let (n, etag, size, chunks) = r?;
                        have.insert(n, (etag, u64::try_from(size).unwrap_or(0), serde_json::from_str(&chunks)?));
                    }
                    Ok(Ok((key, have)))
                })
                .await?;
            seam::check_parts(&parts)?;
            let last = parts.len().saturating_sub(1);
            let (mut size, mut all): (u64, Chunks) = (0, Vec::new());
            for (i, (n, etag)) in parts.iter().enumerate() {
                let (have_etag, part_size, chunks) = have.get(n).ok_or_else(|| Refusal::BadParts(format!("part {n} wasn't uploaded")))?;
                if have_etag != etag {
                    return Err(Refusal::BadParts(format!("part {n}'s etag doesn't match")));
                }
                if i < last && *part_size < len64(self.limits.part_min) {
                    return Err(Refusal::BadParts(format!("part {n} is {part_size} bytes: every part but the last is at least {}", self.limits.part_min)));
                }
                size = size.saturating_add(*part_size);
                all.extend(chunks.iter().cloned());
            }
            if size > self.limits.object {
                return Err(Refusal::TooLarge { what: "an object", size, limit: self.limits.object });
            }
            let digest = match self.following.finish(upload, &parts) {
                Some(d) => d,
                None => {
                    let (root, list) = (self.root.clone(), all.clone());
                    Self::disk(move || chunks::digest(&root, &list)).await?
                }
            };
            let w = Write { key, digest, size, chunks: all, cond: Cond::Always, now_ms: self.clock.unix_ms(), upload: Some(upload.to_owned()) };
            let (meta, released) = self.sql(move |c| commit(c, &w)).await?;
            Self::release(released).await;
            Ok(meta)
        })
    }

    fn abort_upload<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            let up = upload.to_owned();
            self.sql(move |c| {
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute("DELETE FROM blob_parts WHERE upload = ?1", [&up])?;
                let gone = tx.execute("DELETE FROM blob_uploads WHERE id = ?1", [&up])?;
                tx.commit()?;
                Ok(if gone == 0 { Err(Refusal::NoSuchUpload(up)) } else { Ok(()) })
            })
            .await?;
            self.following.forget(upload);
            Ok(())
        })
    }

    fn presign<'a>(&'a self, key: &'a str, access: Access, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            let secs = seam::lifetime_secs(expires)?;
            let expires_ms = self.clock.unix_ms().saturating_add(i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX));
            let length = match access {
                Access::Get => String::new(),
                Access::Put { length } => format!("&length={length}"),
            };
            Ok(format!("{}/{}?expires={expires_ms}{length}&sig={}", self.presign.base, seam::encode_key(key), signature(&self.presign.key, access, key, expires_ms)))
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kiln_seams::blobs::BlobsFixture;
    use kiln_seams::clock::SimClock;

    /// The largest object the fixture's store takes: small, so the suite can pass it.
    const FIXTURE_OBJECT: u64 = 24 << 20;

    pub(crate) struct Fixture {
        pub blobs: LocalBlobs,
        pub clock: SimClock,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        pub(crate) fn new() -> anyhow::Result<Self> {
            let dir = tempfile::tempdir()?;
            let clock = SimClock::new();
            let presign = Presign { base: "http://kiln.test/api/blobs".into(), key: [7u8; 32] };
            let limits = Limits { object: FIXTURE_OBJECT, ..Limits::default() };
            let blobs = LocalBlobs::new(Store::in_memory()?, dir.path().join("cas"), Arc::new(clock.clone()), limits, presign);
            Ok(Self { blobs, clock, _dir: dir })
        }
    }

    impl BlobsFixture for Fixture {
        fn blobs(&self) -> &dyn Blobs {
            &self.blobs
        }
        fn part_min(&self) -> usize {
            self.blobs.limits.part_min
        }
        fn object_max(&self) -> u64 {
            FIXTURE_OBJECT
        }
        fn resolution(&self) -> Duration {
            Duration::from_millis(1)
        }
        fn pass(&self, d: Duration) -> BoxFuture<'_, ()> {
            self.clock.advance(d);
            Box::pin(async {})
        }
        fn redeem<'a>(&'a self, url: &'a str, method: Method, body: Option<Vec<u8>>) -> BoxFuture<'a, Result<Option<Vec<u8>>, Refusal>> {
            Box::pin(self.blobs.redeem(url, method, body))
        }
    }

    /// CP-T10.1, AC-T10.1.2: the local store passes every clause of the Blobs suite, each case on a
    /// store of its own.
    #[test]
    fn the_local_store_passes_the_blobs_suite() -> anyhow::Result<()> {
        let report = crate::rt::block_on(crate::blobs::tests::run_suite(|| async { Fixture::new() }, Duration::from_secs(120)))?;
        let passed = report.into_result().map_err(|e| anyhow::anyhow!(e))?;
        assert!(passed >= 150, "{passed} cases");
        Ok(())
    }

    /// CP-T10.1: an object no key holds stays an hour, so a read under way finishes; then it goes,
    /// its chunks with it, and what another key holds stays.
    #[test]
    fn what_no_key_holds_goes_an_hour_later_and_nothing_else_does() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = Fixture::new()?;
            let b = &f.blobs;
            let (kept, dropped) = (kiln_seams::blobs::noise(3 << 20, 1), kiln_seams::blobs::noise(3 << 20, 2));
            b.put("kept", kept.clone(), Put::default()).await?;
            b.put("dropped", dropped, Put::default()).await?;
            b.delete("dropped").await?;
            let before = b.stored_bytes().await?;
            assert_eq!(b.collect().await?.objects, 0, "released within the hour, it stays");
            f.clock.advance(chunks::GRACE + Duration::from_secs(1));
            age_chunks(&b.root)?;
            let c = b.collect().await?;
            assert_eq!(c.objects, 1, "{c:?}");
            assert!(c.chunks > 0 && b.stored_bytes().await? < before, "{c:?}");
            assert_eq!(b.get("kept", None).await?.1, kept);
            Ok(())
        })
    }

    fn walk(dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d)? {
                let p = e?.path();
                if p.is_dir() { todo.push(p) } else { out.push(p) }
            }
        }
        Ok(out)
    }

    /// The chunk files are as old as the disk says: made older than the grace by hand.
    fn age_chunks(root: &Path) -> io::Result<()> {
        let old = std::time::SystemTime::now() - chunks::GRACE - Duration::from_secs(60);
        for e in walk(&root.join("chunks"))? {
            std::fs::File::options().write(true).open(&e)?.set_modified(old)?;
        }
        Ok(())
    }

    /// What kiln's CAS upkeep did before its objects were behind Blobs, as an older kiln run on
    /// this store again (a rollback) does it at `now`: `collect`'s SQL in ci/services/cas.rs at main
    /// 64b69db word for word, then its sweep of every chunk file no remaining object uses (made old
    /// enough by hand). How many objects it let go.
    fn older_kiln_upkeep(c: &mut Connection, root: &Path, now: i64) -> anyhow::Result<usize> {
        let grace = 3600;
        let tx = c.transaction()?;
        let gone = tx.execute(
            "DELETE FROM cas_objects WHERE last_used_at < ?1
               AND digest NOT IN (SELECT object FROM ci_caches WHERE object IS NOT NULL)
               AND digest NOT IN (SELECT object FROM ci_artifacts WHERE object IS NOT NULL)
               AND digest NOT IN (SELECT object FROM ci_release_assets)
               AND digest NOT IN (SELECT object FROM test_reports)
               AND 'sha256:' || digest NOT IN (SELECT digest FROM registry_blobs)
               AND digest NOT IN (SELECT digest FROM proxy_cache)",
            params![now.saturating_sub(grace)],
        )?;
        let mut used = HashSet::new();
        {
            let mut q = tx.prepare("SELECT chunks FROM cas_objects")?;
            let rows = q.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                let list: Vec<(String, u64)> = serde_json::from_str(&row?).unwrap_or_default();
                used.extend(list.into_iter().map(|(d, _)| d));
            }
        }
        tx.commit()?;
        for f in walk(&root.join("chunks"))? {
            let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !used.contains(&name) || name.ends_with(".tmp") {
                std::fs::remove_file(&f)?;
            }
        }
        Ok(gone)
    }

    /// CP-T10.1: an older kiln run on this store again (a rollback) lets go of no object a key
    /// holds, but for the CAS's own nothing of CI refers to, as it always did; and this release,
    /// started again, drops the keys of those.
    #[test]
    fn an_older_kiln_run_on_the_store_lets_go_of_nothing_a_key_holds() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = Fixture::new()?;
            let b = &f.blobs;
            let noise = kiln_seams::blobs::noise;
            let held = [("sync/a", noise(2 << 20, 21)), ("binaries/kiln-x", noise(3 << 20, 22)), ("recordings/s/r/video", noise(1 << 20, 23)), ("transcripts/s/claude.jsonl.br", noise(1 << 20, 24))];
            for (k, v) in &held {
                b.put(k, v.clone(), Put::default()).await?;
            }
            // The CAS's own: one CI refers to, one nothing does, one a sync key holds too.
            let (used, unused, shared) = (noise(2 << 20, 25), noise(2 << 20, 26), noise(2 << 20, 27));
            let cas = |v: &[u8]| format!("cas/{}", crate::util::sha256_hex(v));
            for v in [&used, &unused, &shared] {
                b.put(&cas(v), v.clone(), Put::default()).await?;
            }
            b.put("sync/shared", shared.clone(), Put::default()).await?;
            let d = crate::util::sha256_hex(&used);
            b.store.call(move |c| Ok(c.execute("INSERT INTO proxy_cache (proxy, key, digest, size, content_type, fetched_at) VALUES ('p', 'k', ?1, 1, 'x', 0)", [d])?)).await?;
            // Two hours on, the older kiln's upkeep.
            age_chunks(&b.root)?;
            let (root, now) = (b.root.clone(), f.clock.unix_ms().div_euclid(1000).saturating_add(7200));
            assert_eq!(b.store.call(move |c| older_kiln_upkeep(c, &root, now)).await?, 1, "the CAS's object nothing refers to, alone");
            for (k, v) in held.iter().chain([("sync/shared", shared.clone())].iter()) {
                assert!(b.get(k, None).await?.1 == *v, "{k}");
            }
            assert!(b.get(&cas(&used), None).await?.1 == used && b.get(&cas(&shared), None).await?.1 == shared);
            assert!(b.get(&cas(&unused), None).await.is_err());
            assert_eq!(b.reconcile().await?, Reconciled { taken: 0, dropped: 1 });
            assert!(matches!(b.head(&cas(&unused)).await, Err(Refusal::NotFound(_))));
            Ok(())
        })
    }

    /// CP-T10.1: started again after an older kiln ran on this store (a rollback), the store takes
    /// in the objects it kept, and those it used after this store let them go, at `cas/<digest>`
    /// with records of kiln's CAS; keys whose files it removed or replaced go; once. Its upkeep
    /// then lets go of what it let go itself, and of nothing taken in.
    #[test]
    fn what_an_older_kiln_kept_is_taken_in_and_what_it_removed_is_dropped() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = Fixture::new()?;
            let b = &f.blobs;
            let noise = kiln_seams::blobs::noise;
            let cas = |v: &[u8]| format!("cas/{}", crate::util::sha256_hex(v));
            // Let go of by this store: one then used by the older kiln (a CI read, as it does),
            // one not.
            let (reused, dropped) = (noise(1 << 20, 31), noise(1 << 20, 32));
            for (k, v) in [("tmp/reused", &reused), ("tmp/dropped", &dropped)] {
                b.put(k, v.clone(), Put::default()).await?;
                b.delete(k).await?;
            }
            // Kept by the older kiln as it keeps objects: its chunks, and a row.
            let kept = noise(3 << 20, 33);
            let (list, _) = chunks::keep_all(&b.root, &kept)?;
            let (kd, rd, list) = (crate::util::sha256_hex(&kept), crate::util::sha256_hex(&reused), serde_json::to_string(&list)?);
            let now = f.clock.unix_ms().div_euclid(1000);
            b.store
                .call(move |c| {
                    c.execute("INSERT INTO cas_objects (digest, size, chunks, created_at, last_used_at) VALUES (?1, ?2, ?3, ?4, ?4)", params![kd, 3 << 20, list, now.saturating_add(60)])?;
                    c.execute("UPDATE cas_objects SET last_used_at = ?2 WHERE digest = ?1", params![rd, now.saturating_add(120)])?;
                    Ok(())
                })
                .await?;
            // Files taken where they lay: the older kiln removed one, and replaced another.
            let dir = tempfile::tempdir()?;
            let (gone, replaced) = (dir.path().join("gone"), dir.path().join("replaced"));
            std::fs::write(&gone, b"removed")?;
            std::fs::write(&replaced, b"before")?;
            assert_eq!(b.adopt(vec![("sync/gone".into(), gone.clone(), None), ("binaries/kiln-y".into(), replaced.clone(), None)]).await?.taken, 2);
            std::fs::remove_file(&gone)?;
            std::fs::write(&replaced, b"after, and longer")?;
            assert_eq!(b.reconcile().await?, Reconciled { taken: 2, dropped: 2 });
            assert!(b.get(&cas(&kept), None).await?.1 == kept && b.get(&cas(&reused), None).await?.1 == reused);
            for k in ["sync/gone", "binaries/kiln-y", &cas(&dropped)] {
                assert!(matches!(b.head(k).await, Err(Refusal::NotFound(_))), "{k}");
            }
            let index: i64 = b.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM cas_index", [], |r| r.get(0))?)).await?;
            assert_eq!(index, 2, "kiln's CAS knows what was taken in");
            assert_eq!(b.reconcile().await?, Reconciled::default(), "taken in once");
            f.clock.advance(chunks::GRACE + Duration::from_secs(1));
            age_chunks(&b.root)?;
            assert_eq!(b.collect().await?.objects, 1);
            assert!(b.get(&cas(&kept), None).await?.1 == kept && b.get(&cas(&reused), None).await?.1 == reused);
            Ok(())
        })
    }

    /// CP-T10.1: files kept before the store had keys are its objects where they lie, read as they
    /// are; replaced, the file goes.
    #[test]
    fn files_kept_before_are_taken_where_they_lie() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = Fixture::new()?;
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("old");
            std::fs::write(&file, b"from before")?;
            let sha = crate::util::sha256_hex(b"from before");
            assert_eq!(f.blobs.adopt(vec![("sync/x".into(), file.clone(), Some(sha.clone()))]).await?.taken, 1);
            assert_eq!(f.blobs.adopt(vec![("sync/x".into(), file.clone(), None)]).await?, Adopted::default(), "taken once");
            let (meta, bytes) = f.blobs.get("sync/x", None).await?;
            assert_eq!((meta.etag, bytes), (sha, b"from before".to_vec()));
            assert_eq!(f.blobs.get("sync/x", Some(Range { start: 5, end: None })).await?.1, b"before".to_vec());
            f.blobs.put("sync/x", b"new".to_vec(), Put::default()).await?;
            assert!(!file.exists(), "the file it named is removed once the key is replaced");
            assert_eq!(f.blobs.get("sync/x", None).await?.1, b"new".to_vec());
            Ok(())
        })
    }
}
