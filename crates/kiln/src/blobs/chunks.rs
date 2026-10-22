//! The content-addressed store's chunks (docs/ci.md W08-R01): content cut by FastCDC
//! (`kiln_ci::store::cdc`), each chunk kept once, under its SHA-256, at
//! `<root>/chunks/<2 hex>/<2 hex>/<digest>`. Two versions of a large object share every chunk the
//! change didn't touch, and the same content kept twice is stored once. What kiln's local store
//! ([`super::local`]) keeps its objects' bytes in; an object is its list of chunks.
//!
//! A new chunk is flushed to the disk before it's named, and its folder after, so a chunk an
//! acknowledged object names is there after a crash. A chunk already there is touched, so a sweep
//! leaves it alone while the object that uses it is recorded.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use kiln_ci::store::cdc;
use sha2::{Digest as _, Sha256};

/// An object's chunks: each one's digest and size, in order.
pub type Chunks = Vec<(String, u64)>;

/// Chunks younger than this are never swept: an object may be about to name them.
pub const GRACE: Duration = Duration::from_secs(3600);

/// Where chunk `digest` is kept.
pub fn path(root: &Path, digest: &str) -> PathBuf {
    root.join("chunks").join(digest.get(0..2).unwrap_or("00")).join(digest.get(2..4).unwrap_or("00")).join(digest)
}

/// Flushes a folder, so a name made in it lasts (Unix; Windows has no flush of a folder).
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Keeps a chunk unless it's there; the bytes newly written.
pub fn keep(root: &Path, digest: &str, data: &[u8]) -> io::Result<u64> {
    let p = path(root, digest);
    if p.is_file() {
        if let Ok(f) = File::options().write(true).open(&p) {
            let _ = f.set_modified(SystemTime::now());
        }
        return Ok(0);
    }
    let dir = p.parent().ok_or_else(|| io::Error::other("a chunk path with no folder"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = p.with_extension(format!("{}.tmp", crate::util::new_id()));
    let written = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &p)?;
        sync_dir(dir)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(u64::try_from(data.len()).unwrap_or(u64::MAX))
}

/// Cuts `data` into chunks and keeps each: its chunks, and the bytes newly written.
pub fn keep_all(root: &Path, data: &[u8]) -> io::Result<(Chunks, u64)> {
    let (mut chunks, mut fresh, mut at) = (Vec::new(), 0u64, 0usize);
    for cut in cdc::cuts(data) {
        let end = at.saturating_add(cut);
        let chunk = data.get(at..end).ok_or_else(|| io::Error::other("a cut past the data"))?;
        let digest = hex::encode(Sha256::digest(chunk));
        fresh = fresh.saturating_add(keep(root, &digest, chunk)?);
        chunks.push((digest, u64::try_from(cut).unwrap_or(u64::MAX)));
        at = end;
    }
    Ok((chunks, fresh))
}

/// Bytes `first` to `last` (inclusive) of the object whose chunks are `chunks`. A chunk missing or
/// shorter than its list says is an error, never fewer bytes than asked.
pub fn read(root: &Path, chunks: &[(String, u64)], first: u64, last: u64) -> io::Result<Vec<u8>> {
    let want = usize::try_from(last.saturating_sub(first).saturating_add(1)).map_err(|_| io::Error::other("a range past what this machine addresses"))?;
    let mut out = Vec::with_capacity(want);
    let mut at = 0u64;
    for (digest, size) in chunks {
        let (lo, hi) = (at, at.saturating_add(*size));
        at = hi;
        if hi <= first {
            continue;
        }
        if lo > last {
            break;
        }
        let from = first.saturating_sub(lo);
        let to = last.min(hi.saturating_sub(1)).saturating_sub(lo);
        let f = File::open(path(root, digest)).map_err(|e| io::Error::new(e.kind(), format!("chunk {digest}: {e}")))?;
        let len = usize::try_from(to.saturating_sub(from).saturating_add(1)).map_err(|_| io::Error::other("a chunk past what this machine addresses"))?;
        let start = out.len();
        out.resize(start.saturating_add(len), 0);
        let buf = out.get_mut(start..).ok_or_else(|| io::Error::other("a read past its buffer"))?;
        let mut got = 0usize;
        while got < len {
            let rest = buf.get_mut(got..).ok_or_else(|| io::Error::other("a read past its buffer"))?;
            let n = crate::os::read_at(&f, rest, from.saturating_add(u64::try_from(got).unwrap_or(u64::MAX)))?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("chunk {digest} is shorter than recorded")));
            }
            got = got.saturating_add(n);
        }
    }
    if out.len() != want {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("the chunks hold {} of the {want} bytes asked", out.len())));
    }
    Ok(out)
}

/// The SHA-256 of the object whose chunks are `chunks`, read in order a chunk at a time.
pub fn digest(root: &Path, chunks: &[(String, u64)]) -> io::Result<String> {
    let mut h = Sha256::new();
    for (d, size) in chunks {
        let bytes = std::fs::read(path(root, d)).map_err(|e| io::Error::new(e.kind(), format!("chunk {d}: {e}")))?;
        if u64::try_from(bytes.len()).ok() != Some(*size) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("chunk {d} is {} bytes, not {size}", bytes.len())));
        }
        h.update(&bytes);
    }
    Ok(hex::encode(h.finalize()))
}

/// What a sweep removed: chunks, their bytes, and their digests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Swept {
    pub chunks: usize,
    pub bytes: u64,
    pub digests: Vec<String>,
}

/// Deletes chunk files not in `used` and older than [`GRACE`], and leftovers of writes a crash cut
/// short; at most `budget` of them a sweep.
pub fn sweep(root: &Path, used: &HashSet<String>, budget: usize) -> Swept {
    let mut out = Swept::default();
    let Ok(level1) = std::fs::read_dir(root.join("chunks")) else { return out };
    for a in level1.flatten() {
        let Ok(level2) = std::fs::read_dir(a.path()) else { continue };
        for b in level2.flatten() {
            let Ok(files) = std::fs::read_dir(b.path()) else { continue };
            for f in files.flatten() {
                if out.chunks >= budget {
                    return out;
                }
                let name = f.file_name().to_string_lossy().to_string();
                let Ok(meta) = f.metadata() else { continue };
                let old = meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age > GRACE);
                if !old || (used.contains(&name) && !name.ends_with(".tmp")) {
                    continue;
                }
                if std::fs::remove_file(f.path()).is_ok() {
                    out.chunks = out.chunks.saturating_add(1);
                    out.bytes = out.bytes.saturating_add(meta.len());
                    out.digests.push(name);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        kiln_seams::blobs::noise(n, seed)
    }

    /// docs/ci.md W08-AC05: a changed object stores only its new chunks.
    #[test]
    fn w08_ac05_a_changed_object_stores_only_its_new_chunks() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("cas");
        let data = noise(48 << 20, 5);
        let (a, fresh_a) = keep_all(&root, &data)?;
        let size: u64 = a.iter().map(|(_, n)| n).sum();
        assert_eq!(size, data.len() as u64);
        assert_eq!(fresh_a, size, "everything is new the first time");
        assert_eq!(digest(&root, &a)?, hex::encode(Sha256::digest(&data)));
        // The same content again: nothing new.
        let (again, fresh) = keep_all(&root, &data)?;
        assert_eq!((again.clone(), fresh), (a.clone(), 0));
        // 1 % of the bytes changed in place, in one place, and an insertion elsewhere.
        let mut edited = data.clone();
        for b in edited.iter_mut().skip(20 << 20).take(data.len() / 100) {
            *b ^= 0x5a;
        }
        edited.splice(40 << 20..40 << 20, b"inserted".iter().copied());
        let (_, fresh_b) = keep_all(&root, &edited)?;
        assert!(fresh_b * 100 <= edited.len() as u64 * 30, "{fresh_b} new bytes of {}", edited.len());
        Ok(())
    }

    /// CP-T10.1: any range of an object reads back exactly its bytes, across chunks; a chunk gone
    /// is an error, never a short read.
    #[test]
    fn ranges_read_across_chunks_and_a_missing_chunk_is_an_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let data = noise(12 << 20, 9);
        let (chunks, _) = keep_all(dir.path(), &data)?;
        assert!(chunks.len() > 2, "{} chunks", chunks.len());
        for (first, last) in [(0u64, 0u64), (0, data.len() as u64 - 1), (1_000_000, 9_000_000), (data.len() as u64 - 1, data.len() as u64 - 1)] {
            let got = read(dir.path(), &chunks, first, last)?;
            assert_eq!(Some(got.as_slice()), data.get(first as usize..=last as usize), "bytes {first}-{last}");
        }
        let (gone, _) = chunks.get(1).cloned().ok_or_else(|| anyhow::anyhow!("no second chunk"))?;
        std::fs::remove_file(path(dir.path(), &gone))?;
        assert!(read(dir.path(), &chunks, 0, data.len() as u64 - 1).is_err());
        assert!(digest(dir.path(), &chunks).is_err());
        Ok(())
    }
}
