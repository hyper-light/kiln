//! The content-addressed store (docs/ci.md W08-R01). Objects are cut into content-defined
//! chunks (`kiln_ci::store::cdc`). Each chunk is stored once, under its SHA-256, at
//! `<data>/cas/chunks/<2 hex>/<2 hex>/<digest>`, and an object is its chunk list
//! (`cas_objects`). Two versions of a large object share every chunk the change didn't touch,
//! and the same content saved twice (one cache under two keys, an artifact uploaded again) is
//! stored once.
//!
//! Content arrives in a file, assembled from an upload's parts, and is chunked from there. Reads
//! stream any byte range. [`collect`] removes objects nothing refers to, and chunks no object
//! uses.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kiln_ci::store::cdc::{self, Cdc};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest as _, Sha256};

use crate::hub::Hub;

/// Bytes a read sends at a time.
const READ_PIECE: usize = 1 << 20;
/// Chunks and objects younger than this are never collected: an upload may be about to use them.
const GRACE: Duration = Duration::from_secs(3600);

/// A stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// SHA-256 of the content, hex.
    pub digest: String,
    pub size: u64,
    /// Chunk digests and sizes, in order.
    pub chunks: Vec<(String, u64)>,
}

/// `<data>/cas`.
pub fn root(hub: &Hub) -> PathBuf {
    hub.cfg.data("cas")
}

/// Where uploads are assembled before they are chunked in.
pub fn staging(hub: &Hub) -> PathBuf {
    root(hub).join("staging")
}

fn chunk_path(root: &Path, digest: &str) -> PathBuf {
    root.join("chunks").join(digest.get(0..2).unwrap_or("00")).join(digest.get(2..4).unwrap_or("00")).join(digest)
}

fn hex(d: &[u8]) -> String {
    hex::encode(d)
}

/// Writes a chunk unless it is stored; returns the bytes newly written. A stored chunk is
/// touched, so collection leaves it alone while this upload records its object.
fn keep_chunk(root: &Path, digest: &str, data: &[u8]) -> std::io::Result<u64> {
    let p = chunk_path(root, digest);
    if p.is_file() {
        if let Ok(f) = File::options().write(true).open(&p) {
            let _ = f.set_modified(SystemTime::now());
        }
        return Ok(0);
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension(format!("{}.tmp", crate::util::new_id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, &p)?;
    Ok(u64::try_from(data.len()).unwrap_or(u64::MAX))
}

/// Chunks a file into the store, on this thread: returns the object and the bytes of chunks
/// that were new.
pub fn put_file_blocking(root: &Path, path: &Path) -> std::io::Result<(Object, u64)> {
    let mut f = File::open(path)?;
    let mut whole = Sha256::new();
    let mut cdc = Cdc::new();
    let mut pending: Vec<u8> = Vec::with_capacity(cdc::MAX * 2);
    let mut start = 0usize;
    let mut buf = vec![0u8; 4 << 20];
    let (mut chunks, mut fresh, mut size) = (Vec::new(), 0u64, 0u64);
    let mut eof = false;
    loop {
        // Keep at least a maximal chunk ahead of the scan, unless the file has ended.
        while !eof && pending.len() - start < cdc::MAX {
            if start > 0 {
                pending.drain(..start);
                start = 0;
            }
            let n = f.read(&mut buf)?;
            if n == 0 {
                eof = true;
            } else {
                let got = buf.get(..n).unwrap_or_default();
                whole.update(got);
                pending.extend_from_slice(got);
                size += u64::try_from(n).unwrap_or(0);
            }
        }
        let rest = pending.get(start..).unwrap_or_default();
        match cdc.next(rest, eof) {
            Some(cut) if cut > 0 => {
                let chunk = rest.get(..cut).unwrap_or_default();
                let digest = hex(&Sha256::digest(chunk));
                fresh += keep_chunk(root, &digest, chunk)?;
                chunks.push((digest, u64::try_from(cut).unwrap_or(0)));
                start += cut;
            }
            _ => break,
        }
    }
    Ok((Object { digest: hex(&whole.finalize()), size, chunks }, fresh))
}

/// Chunks a file into the store and records the object; the file is left as it is.
pub async fn put_file(hub: &Hub, path: &Path) -> anyhow::Result<Object> {
    let (r, p) = (root(hub), path.to_path_buf());
    let (obj, fresh) = crate::rt::blocking(move || put_file_blocking(&r, &p)).await??;
    let o = obj.clone();
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            for (d, n) in &o.chunks {
                tx.execute("INSERT OR IGNORE INTO cas_chunks (digest, size) VALUES (?1, ?2)", params![d, i64::try_from(*n).unwrap_or(i64::MAX)])?;
            }
            let chunks = serde_json::to_string(&o.chunks)?;
            tx.execute(
                "INSERT INTO cas_objects (digest, size, chunks, created_at, last_used_at) VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT (digest) DO UPDATE SET last_used_at = excluded.last_used_at",
                params![o.digest, i64::try_from(o.size).unwrap_or(i64::MAX), chunks, now],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    tracing::debug!(object = %obj.digest, size = obj.size, chunks = obj.chunks.len(), new_bytes = fresh, "stored");
    Ok(obj)
}

/// An object's chunk list, and a touch of its last use.
pub async fn object(hub: &Hub, digest: &str) -> anyhow::Result<Option<Object>> {
    let d = digest.to_owned();
    hub.store
        .call(move |c| {
            let row: Option<(i64, String)> = c.query_row("SELECT size, chunks FROM cas_objects WHERE digest = ?1", [&d], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((size, chunks)) = row else { return Ok(None) };
            c.execute("UPDATE cas_objects SET last_used_at = ?2 WHERE digest = ?1", params![d, crate::util::now_unix()])?;
            Ok(Some(Object { digest: d, size: u64::try_from(size).unwrap_or(0), chunks: serde_json::from_str(&chunks)? }))
        })
        .await
}

/// Streams bytes `[start, end)` of an object. A chunk that can't be read ends the stream early
/// (the reader sees fewer bytes than it was promised), with a warning.
pub fn read(hub: &Hub, obj: &Object, start: u64, end: u64) -> async_channel::Receiver<Bytes> {
    let (tx, rx) = async_channel::bounded::<Bytes>(4);
    let root = root(hub);
    let chunks = obj.chunks.clone();
    let digest = obj.digest.clone();
    crate::rt::spawn(async move {
        let mut at = 0u64;
        for (d, n) in chunks {
            let (lo, hi) = (at, at.saturating_add(n));
            at = hi;
            if hi <= start {
                continue;
            }
            if lo >= end {
                break;
            }
            let path = chunk_path(&root, &d);
            let file = match File::open(&path) {
                Ok(f) => Arc::new(f),
                Err(e) => {
                    tracing::warn!(object = %digest, chunk = %d, error = %e, "a chunk is missing; the read ends short");
                    return;
                }
            };
            let (mut off, to) = (start.saturating_sub(lo), end.min(hi) - lo);
            while off < to {
                let len = usize::try_from((to - off).min(READ_PIECE as u64)).unwrap_or(READ_PIECE);
                let f = Arc::clone(&file);
                let piece = crate::rt::blocking(move || {
                    let mut buf = vec![0u8; len];
                    let mut got = 0usize;
                    while got < len {
                        let n = crate::os::read_at(&f, buf.get_mut(got..).unwrap_or_default(), off + got as u64)?;
                        if n == 0 {
                            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "chunk shorter than recorded"));
                        }
                        got += n;
                    }
                    Ok::<_, std::io::Error>(buf)
                })
                .await;
                match piece {
                    Ok(Ok(buf)) => {
                        if tx.send(Bytes::from(buf)).await.is_err() {
                            return;
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(object = %digest, chunk = %d, error = %e, "a chunk can't be read; the read ends short");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(object = %digest, error = %format!("{e:#}"), "read stopped");
                        return;
                    }
                }
                off += len as u64;
            }
        }
    });
    rx
}

/// What a collection removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Collected {
    pub objects: usize,
    pub chunks: usize,
    pub bytes: u64,
}

/// Removes objects nothing refers to (no cache entry, artifact or release asset), and then chunks that
/// no remaining object uses. Anything younger than an hour stays, since an upload in progress
/// may be about to refer to it.
pub async fn collect(hub: &Hub) -> anyhow::Result<Collected> {
    let now = crate::util::now_unix();
    let grace = i64::try_from(GRACE.as_secs()).unwrap_or(3600);
    let (objects, used): (usize, HashSet<String>) = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let gone = tx.execute(
                "DELETE FROM cas_objects WHERE last_used_at < ?1
                   AND digest NOT IN (SELECT object FROM ci_caches WHERE object IS NOT NULL)
                   AND digest NOT IN (SELECT object FROM ci_artifacts WHERE object IS NOT NULL)
                   AND digest NOT IN (SELECT object FROM ci_release_assets)
                   AND digest NOT IN (SELECT object FROM test_reports)",
                params![now - grace],
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
            Ok((gone, used))
        })
        .await?;
    let r = root(hub);
    let used = Arc::new(used);
    let u = Arc::clone(&used);
    let (chunks, bytes, removed) = crate::rt::blocking(move || sweep(&r, &u)).await?;
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            for d in &removed {
                tx.execute("DELETE FROM cas_chunks WHERE digest = ?1", [d])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(Collected { objects, chunks, bytes })
}

/// Deletes chunk files not in `used` and older than [`GRACE`]; returns their count, bytes and
/// digests.
fn sweep(root: &Path, used: &HashSet<String>) -> (usize, u64, Vec<String>) {
    let mut out = (0usize, 0u64, Vec::new());
    let Ok(level1) = std::fs::read_dir(root.join("chunks")) else { return out };
    for a in level1.flatten() {
        let Ok(level2) = std::fs::read_dir(a.path()) else { continue };
        for b in level2.flatten() {
            let Ok(files) = std::fs::read_dir(b.path()) else { continue };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                let Ok(meta) = f.metadata() else { continue };
                let old = meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age > GRACE);
                // Leftover temporary files of a crashed write go too.
                if !old || (used.contains(&name) && !name.ends_with(".tmp")) {
                    continue;
                }
                if std::fs::remove_file(f.path()).is_ok() {
                    out.0 += 1;
                    out.1 += meta.len();
                    out.2.push(name);
                }
            }
        }
    }
    out
}

/// The store's size on disk: every chunk once.
pub async fn stored_bytes(hub: &Hub) -> anyhow::Result<u64> {
    hub.store.call(|c| Ok(c.query_row("SELECT COALESCE(SUM(size), 0) FROM cas_chunks", [], |r| r.get::<_, i64>(0)).map(|n| u64::try_from(n).unwrap_or(0))?)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed.max(1);
        let mut out = Vec::with_capacity(n + 8);
        while out.len() < n {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            out.extend_from_slice(&s.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
        }
        out.truncate(n);
        out
    }

    #[test]
    fn w08_ac05_a_changed_object_stores_only_its_new_chunks() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("cas");
        let data = noise(48 << 20, 5);
        let one = dir.path().join("one");
        std::fs::write(&one, &data)?;
        let (a, fresh_a) = put_file_blocking(&root, &one)?;
        assert_eq!(a.size, data.len() as u64);
        assert_eq!(fresh_a, a.size, "everything is new the first time");
        assert_eq!(a.digest, hex(&Sha256::digest(&data)));
        // The same content again: nothing new.
        let (again, fresh) = put_file_blocking(&root, &one)?;
        assert_eq!((again.digest.clone(), fresh), (a.digest.clone(), 0));
        // 1 % of the bytes changed in place, in one place, and an insertion elsewhere.
        let mut edited = data.clone();
        for b in edited.iter_mut().skip(20 << 20).take(data.len() / 100) {
            *b ^= 0x5a;
        }
        edited.splice(40 << 20..40 << 20, b"inserted".iter().copied());
        let two = dir.path().join("two");
        std::fs::write(&two, &edited)?;
        let (b, fresh_b) = put_file_blocking(&root, &two)?;
        assert_eq!(b.size, edited.len() as u64);
        assert!(fresh_b * 100 <= b.size * 30, "{fresh_b} new bytes of {}", b.size);
        Ok(())
    }
}
