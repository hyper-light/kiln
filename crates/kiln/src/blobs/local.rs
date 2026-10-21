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
}

/// What a collection let go.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Collected {
    pub uploads: usize,
    pub objects: usize,
    pub chunks: usize,
    pub bytes: u64,
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
/// at the key, the object recorded, the key pointed at it, and what the key held released (its
/// object kept an hour for reads under way). The object's answer, and a file the key named before
/// (to remove).
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
    tx.execute(
        "INSERT INTO cas_objects (digest, size, chunks, created_at, last_used_at) VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT (digest) DO UPDATE SET last_used_at = excluded.last_used_at",
        params![w.digest, i64_of(w.size), serde_json::to_string(&w.chunks)?, now_s],
    )?;
    if let Some(o) = old.as_ref().filter(|o| o.file.is_none()) {
        tx.execute("UPDATE cas_objects SET last_used_at = ?2 WHERE digest = ?1", params![o.digest, now_s])?;
    }
    tx.execute(
        "INSERT INTO blob_keys (key, digest, size, modified_ms, file) VALUES (?1, ?2, ?3, ?4, NULL)
         ON CONFLICT (key) DO UPDATE SET digest = excluded.digest, size = excluded.size, modified_ms = excluded.modified_ms, file = NULL",
        params![w.key, w.digest, i64_of(w.size), w.now_ms],
    )?;
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
        Self { store, root, clock, limits, presign, following: super::Following::default() }
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
    /// already names something. How many it took.
    pub async fn adopt(&self, files: Vec<(String, PathBuf, Option<String>)>) -> anyhow::Result<usize> {
        let mut taken = 0usize;
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
                    let modified = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
                    out.push((key, digest, md.len(), modified, path.to_string_lossy().into_owned()));
                }
                Ok(out)
            })
            .await??;
            taken = taken.saturating_add(
                self.store
                    .call(move |c| {
                        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                        let mut n = 0usize;
                        for (key, digest, size, modified, path) in &rows {
                            n = n.saturating_add(tx.execute(
                                "INSERT OR IGNORE INTO blob_keys (key, digest, size, modified_ms, file) VALUES (?1, ?2, ?3, ?4, ?5)",
                                params![key, digest, i64_of(*size), modified, path],
                            )?);
                        }
                        tx.commit()?;
                        Ok(n)
                    })
                    .await?,
            );
        }
        Ok(taken)
    }

    /// Lets go of uploads left [`UPLOAD_TTL`], objects no key has held for an hour, and chunks no
    /// object or part uses (once an hour old: a write may be about to name them).
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
                let objects = tx.execute("DELETE FROM cas_objects WHERE last_used_at < ?1 AND digest NOT IN (SELECT digest FROM blob_keys)", [released_before])?;
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
                        tx.execute("UPDATE cas_objects SET last_used_at = ?2 WHERE digest = ?1", params![o.digest, now_s])?;
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
            // The chunk files are as old as the disk says: made older than the grace by hand.
            let old = std::time::SystemTime::now() - chunks::GRACE - Duration::from_secs(60);
            for e in walk(&b.root.join("chunks"))? {
                std::fs::File::options().write(true).open(&e)?.set_modified(old)?;
            }
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
            assert_eq!(f.blobs.adopt(vec![("sync/x".into(), file.clone(), Some(sha.clone()))]).await?, 1);
            assert_eq!(f.blobs.adopt(vec![("sync/x".into(), file.clone(), None)]).await?, 0, "taken once");
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
