//! Files and streams through a store: a file put whole or in parts, an object read back as a
//! stream of ranges, all of one version.

use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use kiln_seams::blobs::{Blobs, Cond, Meta, PART_MIN, PARTS_MAX, Put, Range, Refusal};

/// Past this, a file goes up in parts.
pub const SINGLE_MAX: u64 = 16 << 20;
/// What a part holds, at least.
pub const PART: u64 = 16 << 20;
/// What a read asks the store for at a time.
pub const PIECE: u64 = 4 << 20;

/// The size of a file's parts: [`PART`], or larger where it needs more than 10,000.
pub fn part_size(size: u64) -> u64 {
    size.div_ceil(u64::from(PARTS_MAX)).max(PART).max(u64::try_from(PART_MIN).unwrap_or(u64::MAX))
}

fn disk(e: impl std::fmt::Display) -> Refusal {
    Refusal::Unavailable(format!("the file: {e}"))
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> std::io::Result<R> + Send + 'static) -> Result<R, Refusal> {
    match crate::rt::blocking(f).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(disk(e)),
        Err(e) => Err(disk(format!("{e:#}"))),
    }
}

/// Bytes `at` to `at + len` of a file.
fn read_part(path: &Path, at: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(std::io::SeekFrom::Start(at))?;
    let mut out = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    f.take(len).read_to_end(&mut out)?;
    Ok(out)
}

/// Puts the file at `path` as `key`'s object: whole when it's [`SINGLE_MAX`] or less, else in
/// parts read from it in turn (its condition judged before the first). `sha256`, when the caller
/// knows the content's, is checked: a file that changed under the upload isn't kept.
pub async fn put_file(b: &dyn Blobs, key: &str, path: &Path, cond: Cond, sha256: Option<&str>) -> Result<Meta, Refusal> {
    let p: PathBuf = path.to_owned();
    let size = blocking(move || std::fs::metadata(&p).map(|m| m.len())).await?;
    if size <= SINGLE_MAX {
        let p = path.to_owned();
        let bytes = blocking(move || std::fs::read(&p)).await?;
        return b.put(key, bytes, Put { cond, sha256: sha256.map(str::to_owned) }).await;
    }
    match &cond {
        Cond::Always => {}
        Cond::IfNoneMatch => match b.head(key).await {
            Ok(_) => return Err(Refusal::PreconditionFailed(key.to_owned())),
            Err(Refusal::NotFound(_)) => {}
            Err(e) => return Err(e),
        },
        Cond::IfMatch(want) => match b.head(key).await {
            Ok(m) if &m.etag == want => {}
            Ok(_) | Err(Refusal::NotFound(_)) => return Err(Refusal::PreconditionFailed(key.to_owned())),
            Err(e) => return Err(e),
        },
    }
    let upload = b.start_upload(key).await?;
    let sent = send_parts(b, &upload, path, size).await;
    let meta = match sent {
        Ok(parts) => b.complete_upload(&upload, parts).await,
        Err(e) => Err(e),
    };
    let meta = match meta {
        Ok(m) => m,
        Err(e) => {
            let _ = b.abort_upload(&upload).await;
            return Err(e);
        }
    };
    if let Some(want) = sha256.filter(|w| !w.eq_ignore_ascii_case(&meta.etag)) {
        let _ = b.delete(key).await;
        return Err(Refusal::BadChecksum { want: want.to_owned(), got: meta.etag });
    }
    Ok(meta)
}

async fn send_parts(b: &dyn Blobs, upload: &str, path: &Path, size: u64) -> Result<Vec<(u32, String)>, Refusal> {
    let each = part_size(size);
    let mut parts = Vec::new();
    let (mut at, mut n) = (0u64, 1u32);
    while at < size {
        let (p, len) = (path.to_owned(), each.min(size.saturating_sub(at)));
        let bytes = blocking(move || read_part(&p, at, len)).await?;
        if u64::try_from(bytes.len()).ok() != Some(len) {
            return Err(Refusal::Unavailable(format!("{} changed while it was read", path.display())));
        }
        parts.push((n, b.upload_part(upload, n, bytes).await?));
        at = at.saturating_add(len);
        n = n.saturating_add(1);
    }
    Ok(parts)
}

/// Bytes `first` to `last` of `key`'s object as a stream of [`PIECE`]s, each got by its range, all
/// of the version the first was: a piece of another, or a refusal, ends the stream early (the
/// reader sees fewer bytes than it was promised), with a warning.
pub fn read_stream(b: Arc<dyn Blobs>, key: String, first: u64, last: u64) -> async_channel::Receiver<Bytes> {
    let (tx, rx) = async_channel::bounded::<Bytes>(4);
    crate::rt::spawn(async move {
        let mut etag: Option<String> = None;
        let mut at = first;
        while at <= last {
            let end = at.saturating_add(PIECE.saturating_sub(1)).min(last);
            let (meta, bytes) = match b.get(&key, Some(Range { start: at, end: Some(end) })).await {
                Ok(got) => got,
                Err(e) => {
                    tracing::warn!(target: "kiln.blobs", event = "get", key = %key, error = %e, "a read ends short");
                    return;
                }
            };
            if etag.get_or_insert_with(|| meta.etag.clone()) != &meta.etag {
                tracing::warn!(target: "kiln.blobs", event = "get", key = %key, "the object changed while it was read; the read ends short");
                return;
            }
            let n = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if n == 0 || tx.send(Bytes::from(bytes)).await.is_err() {
                return;
            }
            at = at.saturating_add(n);
        }
    });
    rx
}

/// `key`'s whole object, refused `TooLarge` past `limit`.
pub async fn get_all(b: &dyn Blobs, key: &str, limit: u64) -> Result<(Meta, Vec<u8>), Refusal> {
    let meta = b.head(key).await?;
    if meta.size > limit {
        return Err(Refusal::TooLarge { what: "an object", size: meta.size, limit });
    }
    b.get(key, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::blobs::{SimBlobs, SimLimits};

    /// CP-T10.1: a file larger than one put goes up in parts and reads back whole as a stream; a
    /// file that isn't what its SHA-256 says isn't kept.
    #[test]
    fn a_large_file_goes_up_in_parts_and_streams_back_whole() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let data = kiln_seams::blobs::noise((40 << 20) + 123, 3);
            let path = dir.path().join("big");
            std::fs::write(&path, &data)?;
            let sha = crate::util::sha256_hex(&data);
            let b: Arc<dyn Blobs> = Arc::new(SimBlobs::new(crate::rt::clock(), SimLimits { object: 1 << 30, ..SimLimits::default() }));
            let meta = put_file(b.as_ref(), "big", &path, Cond::Always, Some(&sha)).await?;
            assert_eq!((meta.size, meta.etag), (data.len() as u64, sha.clone()));
            let rx = read_stream(Arc::clone(&b), "big".into(), 0, data.len() as u64 - 1);
            let mut got = Vec::new();
            while let Ok(piece) = rx.recv().await {
                got.extend_from_slice(&piece);
            }
            assert!(got == data, "the stream gave {} of {} bytes", got.len(), data.len());
            let wrong = put_file(b.as_ref(), "wrong", &path, Cond::Always, Some(&crate::util::sha256_hex(b"other"))).await;
            assert!(matches!(wrong, Err(Refusal::BadChecksum { .. })), "{wrong:?}");
            assert!(matches!(b.head("wrong").await, Err(Refusal::NotFound(_))));
            assert!(matches!(put_file(b.as_ref(), "big", &path, Cond::IfNoneMatch, None).await, Err(Refusal::PreconditionFailed(_))));
            Ok(())
        })
    }
}
