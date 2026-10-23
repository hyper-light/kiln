//! The content-addressed store (docs/ci.md W08-R01), behind kiln's `Blobs` (docs/blobs.md,
//! CP-T10.1): an object is named by its content's SHA-256 and kept at `cas/<digest>` in the store
//! `[blobs]` names, so the same content saved twice (one cache under two keys, an artifact uploaded
//! again) is kept once. In the local store it's cut into content-defined chunks
//! (`crate::blobs::chunks`): two versions of a large object share every chunk the change didn't
//! touch.
//!
//! `cas_index` records the objects kept, whichever store holds them, so a lookup asks the store
//! nothing and [`collect`] finds what nothing refers to without listing the store. Content arrives in
//! a file, assembled from an upload's parts in [`staging`], and goes up from there; reads stream any
//! byte range, a piece at a time.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bytes::Bytes;
use kiln_seams::blobs::Refusal;
use parking_lot::Mutex;
use rusqlite::{OptionalExtension as _, TransactionBehavior, params};
use sha2::Digest as _;

use crate::hub::Hub;

/// An object put or found is never collected sooner than this: an upload may be about to refer to it.
const GRACE: Duration = Duration::from_secs(3600);
/// Objects one collection lets go, at most.
const COLLECT_MAX: usize = 100_000;
/// Locks objects are taken and let go under, by digest.
const STRIPES: usize = 64;

/// A stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// SHA-256 of the content, hex.
    pub digest: String,
    pub size: u64,
}

/// `<data>/cas`: where uploads are assembled, and the local store keeps its chunks.
pub fn root(hub: &Hub) -> PathBuf {
    hub.cfg.data("cas")
}

/// Where uploads are assembled before they go into the store.
pub fn staging(hub: &Hub) -> PathBuf {
    root(hub).join("staging")
}

/// The key an object is kept at.
pub fn key(digest: &str) -> String {
    format!("cas/{digest}")
}

/// What taking an object in and letting one go are serialized by, a digest at a time: a collection
/// never lets go of an object a put has just found, nor one whose upload is under way.
struct Guard {
    stripes: Vec<async_lock::Mutex<()>>,
    /// Objects being uploaded now, by digest (as many as uploads under way).
    uploading: Mutex<HashSet<String>>,
}

static GUARD: LazyLock<Guard> = LazyLock::new(|| Guard { stripes: (0..STRIPES).map(|_| async_lock::Mutex::new(())).collect(), uploading: Mutex::default() });

fn stripe(digest: &str) -> &'static async_lock::Mutex<()> {
    let n = digest.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(usize::from(b))) % STRIPES;
    // `n` is below STRIPES, the length.
    GUARD.stripes.get(n).unwrap_or_else(|| first_stripe())
}

fn first_stripe() -> &'static async_lock::Mutex<()> {
    static ONE: LazyLock<async_lock::Mutex<()>> = LazyLock::new(|| async_lock::Mutex::new(()));
    GUARD.stripes.first().unwrap_or(&ONE)
}

/// An upload under way, so a collection leaves its object alone; gone with it.
struct Uploading(String);

impl Uploading {
    fn start(digest: &str) -> Self {
        GUARD.uploading.lock().insert(digest.to_owned());
        Self(digest.to_owned())
    }
}

impl Drop for Uploading {
    fn drop(&mut self) {
        GUARD.uploading.lock().remove(&self.0);
    }
}

fn refused(e: Refusal) -> anyhow::Error {
    anyhow::anyhow!("kiln's store of bytes: {e}")
}

/// The SHA-256 and size of a file.
fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let size = std::fs::metadata(path)?.len();
    Ok((crate::sync::blobs::hash_file(path)?, size))
}

/// Records that `digest` is kept, as of now; whether it was recorded before.
async fn record(hub: &Hub, digest: &str, size: u64) -> anyhow::Result<bool> {
    let (d, now) = (digest.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let was: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM cas_index WHERE digest = ?1)", [&d], |r| r.get(0))?;
            tx.execute(
                "INSERT INTO cas_index (digest, size, created_at, last_used_at) VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT (digest) DO UPDATE SET last_used_at = excluded.last_used_at",
                params![d, i64::try_from(size).unwrap_or(i64::MAX), now],
            )?;
            tx.commit()?;
            Ok(was)
        })
        .await
}

/// Puts a file's content into the store and records it; the file is left as it is. Content kept
/// already is found and touched, not sent again.
pub async fn put_file(hub: &Hub, path: &Path) -> anyhow::Result<Object> {
    let p = path.to_path_buf();
    let (digest, size) = crate::rt::blocking(move || hash_file(&p)).await??;
    let object = Object { digest: digest.clone(), size };
    let blobs = Arc::clone(&hub.blobs.blobs);
    let uploading = {
        let _held = stripe(&digest).lock().await;
        if record(hub, &digest, size).await? {
            match blobs.head(&key(&digest)).await {
                Ok(m) if m.size == size => return Ok(object),
                Ok(_) | Err(Refusal::NotFound(_)) => {}
                Err(e) => return Err(refused(e)),
            }
        }
        Uploading::start(&digest)
    };
    let kept = crate::blobs::io::put_file(blobs.as_ref(), &key(&digest), path, Some(&digest)).await;
    {
        let _held = stripe(&digest).lock().await;
        kept.map_err(refused)?;
        record(hub, &digest, size).await?;
        drop(uploading);
    }
    tracing::debug!(target: "kiln.blobs", event = "put", object = %digest, size, "kept");
    Ok(object)
}

/// An object, and a touch of its last use.
pub async fn object(hub: &Hub, digest: &str) -> anyhow::Result<Option<Object>> {
    let (d, now) = (digest.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            let Some(size) = c.query_row("SELECT size FROM cas_index WHERE digest = ?1", [&d], |r| r.get::<_, i64>(0)).optional()? else { return Ok(None) };
            c.execute("UPDATE cas_index SET last_used_at = ?2 WHERE digest = ?1", params![d, now])?;
            Ok(Some(Object { digest: d, size: u64::try_from(size).unwrap_or(0) }))
        })
        .await
}

/// Streams bytes `[start, end)` of an object. A piece that can't be read ends the stream early (the
/// reader sees fewer bytes than it was promised), with a warning.
pub fn read(hub: &Hub, obj: &Object, start: u64, end: u64) -> async_channel::Receiver<Bytes> {
    let end = end.min(obj.size);
    if start >= end {
        let (_, rx) = async_channel::bounded(1);
        return rx;
    }
    crate::blobs::io::read_stream(Arc::clone(&hub.blobs.blobs), key(&obj.digest), Some(obj.digest.clone()), start, end.saturating_sub(1))
}

// ---------------------------------------------------------------------------------------- chunks

/// The local store, when it's the one: kiln's chunked cache protocol (`super::chunks`, W08-R09)
/// moves an object chunk by chunk, which only a store that keeps chunks can serve.
pub fn chunked(hub: &Hub) -> Option<&Arc<crate::blobs::local::LocalBlobs>> {
    hub.blobs.local.as_ref()
}

fn is_digest(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())
}

/// An object's chunks (each one's digest and size, in order), as the local store keeps it.
pub async fn chunks_of(hub: &Hub, digest: &str) -> anyhow::Result<Option<crate::blobs::chunks::Chunks>> {
    let d = digest.to_owned();
    hub.store
        .call(move |c| {
            let row: Option<String> = c.query_row("SELECT chunks FROM cas_objects WHERE digest = ?1", [&d], |r| r.get(0)).optional()?;
            Ok(match row {
                Some(list) => Some(serde_json::from_str(&list)?),
                None => None,
            })
        })
        .await
}

/// Where the local store keeps chunk `digest`, when it does.
pub fn chunk_file(hub: &Hub, digest: &str) -> Option<PathBuf> {
    let local = chunked(hub)?;
    is_digest(digest).then(|| crate::blobs::chunks::path(local.root(), digest)).filter(|p| p.is_file())
}

/// Keeps a chunk in the local store, its digest checked first: the bytes newly written.
pub fn put_chunk(hub: &Hub, digest: &str, data: &[u8]) -> anyhow::Result<u64> {
    let local = chunked(hub).ok_or_else(|| anyhow::anyhow!("kiln's store of bytes keeps no chunks here ([blobs] store isn't local)"))?;
    anyhow::ensure!(is_digest(digest) && hex::encode(sha2::Sha256::digest(data)) == digest, "the chunk doesn't hash to {digest}");
    Ok(crate::blobs::chunks::keep(local.root(), digest, data)?)
}

/// An object made of chunks the local store keeps already (each there, the whole hashing to
/// `digest`: the caller checked), kept at its key and recorded.
pub async fn put_chunked(hub: &Hub, digest: &str, chunks: crate::blobs::chunks::Chunks) -> anyhow::Result<Object> {
    let local = Arc::clone(chunked(hub).ok_or_else(|| anyhow::anyhow!("kiln's store of bytes keeps no chunks here ([blobs] store isn't local)"))?);
    let size = chunks.iter().map(|(_, n)| *n).sum::<u64>();
    let _held = stripe(digest).lock().await;
    local.put_chunks(&key(digest), digest, chunks).await.map_err(refused)?;
    record(hub, digest, size).await?;
    tracing::debug!(target: "kiln.blobs", event = "put", object = %digest, size, "kept from chunks");
    Ok(Object { digest: digest.to_owned(), size })
}

/// What a verification found (`kiln store verify`, W08-R01).
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Verified {
    pub chunks: usize,
    pub bytes: u64,
    /// Chunks whose contents aren't their digest: each moved to `<store>/corrupt/`, so the next
    /// write of it stores it again and a read of it fails rather than returning wrong bytes.
    pub corrupt: Vec<String>,
}

/// Re-hashes every chunk the local store keeps on this machine's disk against its name (W08-R01).
/// An S3 store's objects are checked by the store itself (each put's SHA-256).
pub async fn verify(hub: &Hub) -> anyhow::Result<Verified> {
    let Some(local) = chunked(hub) else { anyhow::bail!("kiln's store of bytes is S3 ([blobs] store), which checks each object's SHA-256 as it's put: no chunks to re-hash here") };
    let root = local.root().to_path_buf();
    crate::rt::blocking(move || verify_blocking(&root)).await?
}

fn verify_blocking(root: &Path) -> anyhow::Result<Verified> {
    use std::io::Read as _;
    let mut out = Verified::default();
    let mut buf = vec![0u8; 1 << 20];
    let Ok(level1) = std::fs::read_dir(root.join("chunks")) else { return Ok(out) };
    for a in level1.flatten() {
        let Ok(level2) = std::fs::read_dir(a.path()) else { continue };
        for b in level2.flatten() {
            let Ok(files) = std::fs::read_dir(b.path()) else { continue };
            for f in files.flatten() {
                let digest = f.file_name().to_string_lossy().to_string();
                if !is_digest(&digest) {
                    continue;
                }
                let Ok(mut file) = std::fs::File::open(f.path()) else { continue };
                let mut h = sha2::Sha256::new();
                let mut size = 0u64;
                loop {
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    size += n as u64;
                    h.update(buf.get(..n).unwrap_or_default());
                }
                out.chunks += 1;
                out.bytes += size;
                if hex::encode(h.finalize()) != digest {
                    let aside = root.join("corrupt");
                    std::fs::create_dir_all(&aside)?;
                    std::fs::rename(f.path(), aside.join(&digest))?;
                    out.corrupt.push(digest);
                }
            }
        }
    }
    Ok(out)
}

/// What a collection let go.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Collected {
    pub objects: usize,
    pub bytes: u64,
}

/// What refers to an object: a cache entry, an artifact, a release asset, a test report, a registry
/// blob or a proxied one.
const UNREFERENCED: &str = "digest NOT IN (SELECT object FROM ci_caches WHERE object IS NOT NULL)
   AND digest NOT IN (SELECT object FROM ci_artifacts WHERE object IS NOT NULL)
   AND digest NOT IN (SELECT object FROM ci_release_assets)
   AND digest NOT IN (SELECT object FROM test_reports)
   AND 'sha256:' || digest NOT IN (SELECT digest FROM registry_blobs)
   AND digest NOT IN (SELECT digest FROM proxy_cache)";

/// Lets go of objects nothing refers to, unused for an hour (an upload in progress may be about to
/// refer to one): each one's record, then its bytes, under its digest's lock.
pub async fn collect(hub: &Hub) -> anyhow::Result<Collected> {
    let cutoff = crate::util::now_unix().saturating_sub(i64::try_from(GRACE.as_secs()).unwrap_or(3600));
    let candidates: Vec<String> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare(&format!("SELECT digest FROM cas_index WHERE last_used_at < ?1 AND {UNREFERENCED} LIMIT ?2"))?;
            let rows = q.query_map(params![cutoff, i64::try_from(COLLECT_MAX).unwrap_or(i64::MAX)], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await?;
    let mut out = Collected::default();
    for digest in candidates {
        let _held = stripe(&digest).lock().await;
        if GUARD.uploading.lock().contains(&digest) {
            continue;
        }
        let d = digest.clone();
        let gone: Option<i64> = hub
            .store
            .call(move |c| {
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let size = tx.query_row(&format!("SELECT size FROM cas_index WHERE digest = ?1 AND last_used_at < ?2 AND {UNREFERENCED}"), params![d, cutoff], |r| r.get::<_, i64>(0)).optional()?;
                if size.is_some() {
                    tx.execute("DELETE FROM cas_index WHERE digest = ?1", [&d])?;
                }
                tx.commit()?;
                Ok(size)
            })
            .await?;
        let Some(size) = gone else { continue };
        if let Err(e) = hub.blobs.blobs.delete(&key(&digest)).await {
            tracing::warn!(target: "kiln.blobs", event = "collect", object = %digest, error = %e, "an object nothing refers to couldn't be deleted");
            continue;
        }
        out.objects = out.objects.saturating_add(1);
        out.bytes = out.bytes.saturating_add(u64::try_from(size).unwrap_or(0));
    }
    if out.objects > 0 {
        crate::metrics::blobs().collected.add(&["cas"], u64::try_from(out.objects).unwrap_or(u64::MAX));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP-T10.1: content goes into the store once, at `cas/<digest>`, reads back in any range, and
    /// goes only once nothing refers to it and it's been unused an hour.
    #[test]
    fn content_is_kept_once_read_in_ranges_and_collected_when_nothing_refers_to_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let dir = tempfile::tempdir()?;
            let data = kiln_seams::blobs::noise(20 << 20, 11);
            let path = dir.path().join("f");
            std::fs::write(&path, &data)?;
            let a = put_file(&hub, &path).await?;
            let again = put_file(&hub, &path).await?;
            assert_eq!((a.digest.clone(), a.size), (crate::util::sha256_hex(&data), data.len() as u64));
            assert_eq!(again, a, "the same content is the same object");
            assert_eq!(hub.blobs.blobs.head(&key(&a.digest)).await?.etag, a.digest);
            let obj = object(&hub, &a.digest).await?.ok_or_else(|| anyhow::anyhow!("not found"))?;
            let rx = read(&hub, &obj, 1_000_000, 9_000_000);
            let mut got = Vec::new();
            while let Ok(b) = rx.recv().await {
                got.extend_from_slice(&b);
            }
            assert!(got.as_slice() == data.get(1_000_000..9_000_000).unwrap_or_default(), "{} bytes", got.len());
            assert_eq!(collect(&hub).await?.objects, 0, "used this hour, it stays");
            let d = a.digest.clone();
            hub.store.call(move |c| Ok(c.execute("UPDATE cas_index SET last_used_at = 0 WHERE digest = ?1", [&d])?)).await?;
            assert_eq!(collect(&hub).await?, Collected { objects: 1, bytes: data.len() as u64 });
            assert!(matches!(hub.blobs.blobs.head(&key(&a.digest)).await, Err(Refusal::NotFound(_))));
            assert_eq!(object(&hub, &a.digest).await?, None);
            Ok(())
        })
    }

    /// CP-T10.1: what the content-addressed store held before (its objects in `cas_objects`, its
    /// chunks on disk) is the local store's at `cas/<digest>`, read as it was.
    #[test]
    fn objects_from_before_are_read_where_they_lie() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let data = kiln_seams::blobs::noise(3 << 20, 12);
            let digest = crate::util::sha256_hex(&data);
            // As the store kept it before: chunks on disk, an object row, nothing else.
            let (chunks, _) = crate::blobs::chunks::keep_all(&root(&hub), &data)?;
            let (d, list) = (digest.clone(), serde_json::to_string(&chunks)?);
            hub.store
                .call(move |c| {
                    c.execute("INSERT INTO cas_objects (digest, size, chunks, created_at, last_used_at) VALUES (?1, ?2, ?3, 1, 1)", params![d, 3 << 20, list])?;
                    // What the migration does for what was there when it ran.
                    c.execute_batch(
                        "INSERT OR IGNORE INTO blob_keys (key, digest, size, modified_ms, file) SELECT 'cas/' || digest, digest, size, created_at * 1000, NULL FROM cas_objects;
                         INSERT OR IGNORE INTO cas_index (digest, size, created_at, last_used_at) SELECT digest, size, created_at, last_used_at FROM cas_objects;",
                    )?;
                    Ok(())
                })
                .await?;
            let obj = object(&hub, &digest).await?.ok_or_else(|| anyhow::anyhow!("not found"))?;
            let rx = read(&hub, &obj, 0, obj.size);
            let mut got = Vec::new();
            while let Ok(b) = rx.recv().await {
                got.extend_from_slice(&b);
            }
            assert!(got == data, "{} bytes", got.len());
            Ok(())
        })
    }
}
