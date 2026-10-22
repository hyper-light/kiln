//! Files and streams through a store: a file put whole or in parts, an object read back as a
//! stream of ranges, all of one version.

use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use kiln_seams::blobs::{Blobs, Cond, Meta, PART_MIN, PARTS_MAX, Put, Range, Refusal};
use sha2::{Digest as _, Sha256};

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

/// Puts the file at `path` as `key`'s object, in place of what it held: whole when it's
/// [`SINGLE_MAX`] or less, else in parts read from it in turn and hashed as they go. `sha256`, when
/// the caller knows the content's, is checked before anything is replaced: bytes that aren't what
/// it says (sent wrong, or changed under the upload) are refused (`BadChecksum`), and the key keeps
/// what it held.
pub async fn put_file(b: &dyn Blobs, key: &str, path: &Path, sha256: Option<&str>) -> Result<Meta, Refusal> {
    let p: PathBuf = path.to_owned();
    let size = blocking(move || std::fs::metadata(&p).map(|m| m.len())).await?;
    if size <= SINGLE_MAX {
        let p = path.to_owned();
        let bytes = blocking(move || std::fs::read(&p)).await?;
        return b.put(key, bytes, Put { cond: Cond::Always, sha256: sha256.map(str::to_owned) }).await;
    }
    let upload = b.start_upload(key).await?;
    let parts = match send_parts(b, &upload, path, size).await {
        Ok((parts, got)) => match sha256.filter(|want| !want.eq_ignore_ascii_case(&got)) {
            Some(want) => {
                let _ = b.abort_upload(&upload).await;
                return Err(Refusal::BadChecksum { want: want.to_owned(), got });
            }
            None => parts,
        },
        Err(e) => {
            let _ = b.abort_upload(&upload).await;
            return Err(e);
        }
    };
    let meta = match b.complete_upload(&upload, parts).await {
        Ok(m) => m,
        Err(e) => {
            let _ = b.abort_upload(&upload).await;
            return Err(e);
        }
    };
    // Bytes that hashed right, kept as something else: the store's fault, and not kept.
    if let Some(want) = sha256.filter(|w| !w.eq_ignore_ascii_case(&meta.etag)) {
        let _ = b.delete(key).await;
        return Err(Refusal::BadChecksum { want: want.to_owned(), got: meta.etag });
    }
    Ok(meta)
}

/// Sends the file's parts in turn: the parts, and the SHA-256 of what was sent.
async fn send_parts(b: &dyn Blobs, upload: &str, path: &Path, size: u64) -> Result<(Vec<(u32, String)>, String), Refusal> {
    let each = part_size(size);
    let mut parts = Vec::new();
    let mut sha = Sha256::new();
    let (mut at, mut n) = (0u64, 1u32);
    while at < size {
        let (p, len) = (path.to_owned(), each.min(size.saturating_sub(at)));
        let (bytes, s) = blocking(move || {
            let bytes = read_part(&p, at, len)?;
            sha.update(&bytes);
            Ok((bytes, sha))
        })
        .await?;
        sha = s;
        if u64::try_from(bytes.len()).ok() != Some(len) {
            return Err(Refusal::Unavailable(format!("{} changed while it was read", path.display())));
        }
        parts.push((n, b.upload_part(upload, n, bytes).await?));
        at = at.saturating_add(len);
        n = n.saturating_add(1);
    }
    Ok((parts, hex::encode(sha.finalize())))
}

/// Bytes `first` to `last` of `key`'s object as a stream of [`PIECE`]s, each got by its range, all
/// of one version: the one whose etag is `etag` when it's given (the one its reader was told of),
/// else the one the first piece is of. A piece of another, or a refusal, ends the stream early (the
/// reader sees fewer bytes than it was promised), with a warning. A read of the whole object is
/// checked against its SHA-256 (its etag): its last piece goes only once all of it hashes to it, so
/// damaged bytes end the stream short too.
pub fn read_stream(b: Arc<dyn Blobs>, key: String, etag: Option<String>, first: u64, last: u64) -> async_channel::Receiver<Bytes> {
    let (tx, rx) = async_channel::bounded::<Bytes>(4);
    crate::rt::spawn(async move {
        let mut etag = etag;
        let mut at = first;
        // The whole object's hash so far, when it's the whole that's read; the piece held back.
        let mut whole: Option<Sha256> = None;
        let mut held: Option<Bytes> = None;
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
            if at == 0 && last.checked_add(1) == Some(meta.size) {
                whole = Some(Sha256::new());
            }
            let n = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if n == 0 {
                return;
            }
            let bytes = Bytes::from(bytes);
            if let Some(h) = whole.take() {
                let piece = bytes.clone();
                match crate::rt::blocking(move || {
                    let mut h = h;
                    h.update(&piece);
                    h
                })
                .await
                {
                    Ok(h) => whole = Some(h),
                    Err(_) => return,
                }
            }
            if let Some(before) = held.replace(bytes)
                && tx.send(before).await.is_err()
            {
                return;
            }
            at = at.saturating_add(n);
        }
        if let (Some(h), Some(e)) = (whole, etag.as_deref())
            && hex::encode(h.finalize()) != e
        {
            tracing::warn!(target: "kiln.blobs", event = "get", key = %key, "damaged: the object's bytes don't hash to its etag; the read ends short");
            return;
        }
        if let Some(last_piece) = held {
            let _ = tx.send(last_piece).await;
        }
    });
    rx
}

/// An object written a piece at a time, from wherever its bytes come: held up to a part in memory,
/// sent in parts of [`PART`] past it, put whole when it's no bigger.
pub struct Writer<'a> {
    b: &'a dyn Blobs,
    key: String,
    buf: Vec<u8>,
    upload: Option<String>,
    parts: Vec<(u32, String)>,
}

impl<'a> Writer<'a> {
    pub fn new(b: &'a dyn Blobs, key: &str) -> Self {
        Self { b, key: key.to_owned(), buf: Vec::new(), upload: None, parts: Vec::new() }
    }

    /// The next bytes.
    pub async fn write(&mut self, data: &[u8]) -> Result<(), Refusal> {
        self.buf.extend_from_slice(data);
        let part = usize::try_from(PART).unwrap_or(usize::MAX);
        while self.buf.len() >= part {
            let rest = self.buf.split_off(part);
            let full = std::mem::replace(&mut self.buf, rest);
            self.send(full).await?;
        }
        Ok(())
    }

    async fn send(&mut self, bytes: Vec<u8>) -> Result<(), Refusal> {
        let upload = match &self.upload {
            Some(u) => u.clone(),
            None => {
                let u = self.b.start_upload(&self.key).await?;
                self.upload = Some(u.clone());
                u
            }
        };
        let n = u32::try_from(self.parts.len()).unwrap_or(u32::MAX).saturating_add(1);
        let etag = self.b.upload_part(&upload, n, bytes).await?;
        self.parts.push((n, etag));
        Ok(())
    }

    /// The object, whole: put, or its upload's last part sent and the upload completed.
    pub async fn finish(mut self) -> Result<Meta, Refusal> {
        let Some(upload) = self.upload.clone() else {
            return self.b.put(&self.key, std::mem::take(&mut self.buf), Put::default()).await;
        };
        if !self.buf.is_empty() {
            let last = std::mem::take(&mut self.buf);
            self.send(last).await?;
        }
        self.b.complete_upload(&upload, std::mem::take(&mut self.parts)).await
    }

    /// Gives it up: an upload under way is aborted.
    pub async fn abort(self) {
        if let Some(u) = &self.upload {
            let _ = self.b.abort_upload(u).await;
        }
    }
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
    /// file that isn't what its SHA-256 says isn't kept, and the key keeps what it held.
    #[test]
    fn a_large_file_goes_up_in_parts_and_streams_back_whole() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let data = kiln_seams::blobs::noise((40 << 20) + 123, 3);
            let path = dir.path().join("big");
            std::fs::write(&path, &data)?;
            let sha = crate::util::sha256_hex(&data);
            let b: Arc<dyn Blobs> = Arc::new(SimBlobs::new(crate::rt::clock(), SimLimits { object: 1 << 30, ..SimLimits::default() }));
            let meta = put_file(b.as_ref(), "big", &path, Some(&sha)).await?;
            assert_eq!((meta.size, meta.etag), (data.len() as u64, sha.clone()));
            let rx = read_stream(Arc::clone(&b), "big".into(), None, 0, data.len() as u64 - 1);
            let mut got = Vec::new();
            while let Ok(piece) = rx.recv().await {
                got.extend_from_slice(&piece);
            }
            assert!(got == data, "the stream gave {} of {} bytes", got.len(), data.len());
            // Told of one version, a stream gives nothing of another put since.
            b.put("big", b"replaced".to_vec(), Put::default()).await?;
            let rx = read_stream(Arc::clone(&b), "big".into(), Some(sha.clone()), 0, 7);
            assert!(rx.recv().await.is_err(), "the version its reader was told of is gone");
            let rx = read_stream(Arc::clone(&b), "big".into(), Some(crate::util::sha256_hex(b"replaced")), 0, 7);
            assert_eq!(rx.recv().await.ok().as_deref(), Some(&b"replaced"[..]));
            let wrong = put_file(b.as_ref(), "wrong", &path, Some(&crate::util::sha256_hex(b"other"))).await;
            assert!(matches!(wrong, Err(Refusal::BadChecksum { .. })), "{wrong:?}");
            assert!(matches!(b.head("wrong").await, Err(Refusal::NotFound(_))));
            // A large file sent under another's name is refused before anything is replaced: the
            // key still holds the object that is what its name says.
            b.put("big", data.clone(), Put::default()).await?;
            let other = dir.path().join("other");
            std::fs::write(&other, kiln_seams::blobs::noise((20 << 20) + 7, 4))?;
            let refused = put_file(b.as_ref(), "big", &other, Some(&sha)).await;
            assert!(matches!(refused, Err(Refusal::BadChecksum { .. })), "{refused:?}");
            assert_eq!(b.head("big").await?.etag, sha, "what it held, it holds still");
            Ok(())
        })
    }
}
