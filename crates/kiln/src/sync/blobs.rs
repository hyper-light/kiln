//! Contents, addressed by their SHA-256: a step names a file's new contents by hash, and the copy
//! that applies it reads them from here (fetched from a member that has them, when it doesn't).

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

/// Read and hashed at a time.
const CHUNK: usize = 64 * 1024;

/// A content-addressed store: `<dir>/<first two hex digits>/<hash>`.
#[derive(Debug, Clone)]
pub struct Blobs {
    dir: PathBuf,
}

/// A SHA-256 as a store names it: 64 lowercase hex digits, so a name from elsewhere can never
/// reach outside the store.
pub fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The SHA-256 of a file's contents.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(buf.get(..n).unwrap_or_default());
    }
    Ok(hex::encode(h.finalize()))
}

impl Blobs {
    pub fn new(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self { dir: dir.to_owned() })
    }

    /// Where a blob is (or would be); `None` for a name that isn't a hash.
    pub fn path(&self, hash: &str) -> Option<PathBuf> {
        valid_hash(hash).then(|| self.dir.join(hash.get(..2).unwrap_or("00")).join(hash))
    }

    pub fn has(&self, hash: &str) -> bool {
        self.path(hash).is_some_and(|p| p.is_file())
    }

    /// Keeps a file's contents: its hash, as read (a file changed while it was copied is kept as
    /// it was read, under the hash of that).
    pub fn put_file(&self, src: &Path) -> io::Result<String> {
        let mut f = File::open(src)?;
        self.put_reader(&mut f)
    }

    pub fn put_bytes(&self, bytes: &[u8]) -> io::Result<String> {
        self.put_reader(&mut &*bytes)
    }

    fn put_reader(&self, r: &mut dyn io::Read) -> io::Result<String> {
        let tmp = self.dir.join(format!(".in-{}", crate::util::new_id()));
        let mut out = File::create(&tmp)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        let written = (|| -> io::Result<()> {
            loop {
                let n = r.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                let part = buf.get(..n).unwrap_or_default();
                h.update(part);
                out.write_all(part)?;
            }
            out.sync_all()
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let hash = hex::encode(h.finalize());
        let Some(dest) = self.path(&hash) else {
            let _ = std::fs::remove_file(&tmp);
            return Err(io::Error::other("a SHA-256 that isn't one"));
        };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dest.is_file() {
            std::fs::remove_file(&tmp)?;
        } else {
            std::fs::rename(&tmp, &dest)?;
        }
        Ok(hash)
    }

    /// A blob's contents.
    pub fn read(&self, hash: &str) -> io::Result<Vec<u8>> {
        let p = self.path(hash).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a SHA-256"))?;
        std::fs::read(p)
    }

    /// A place for a download on its way in: inside the store, so it moves in with a rename.
    pub fn incoming(&self) -> PathBuf {
        self.dir.join(format!(".incoming-{}", crate::util::new_id()))
    }

    /// Takes a file as blob `hash` if that is what it holds (moved in: true); else drops it.
    pub fn adopt(&self, file: &Path, hash: &str) -> io::Result<bool> {
        let Some(dest) = self.path(hash) else {
            let _ = std::fs::remove_file(file);
            return Ok(false);
        };
        if hash_file(file)? != hash {
            std::fs::remove_file(file)?;
            return Ok(false);
        }
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::rename(file, &dest)?;
        Ok(true)
    }

    /// Every blob held, and when it was last written.
    pub fn all(&self) -> io::Result<Vec<(String, std::time::SystemTime)>> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(&self.dir)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if valid_hash(&name) {
                    out.push((name, entry.metadata()?.modified()?));
                }
            }
        }
        Ok(out)
    }

    /// Drops a blob nothing needs any more.
    pub fn remove(&self, hash: &str) -> io::Result<()> {
        match self.path(hash) {
            Some(p) if p.is_file() => std::fs::remove_file(p),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contents_are_kept_by_their_hash() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let b = Blobs::new(&dir.path().join("blobs"))?;
        let h = b.put_bytes(b"hello")?;
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert!(b.has(&h) && b.read(&h)? == b"hello");
        let f = dir.path().join("f.txt");
        std::fs::write(&f, b"hello")?;
        assert_eq!((b.put_file(&f)?, hash_file(&f)?), (h.clone(), h.clone()), "the same contents, the same blob");
        // A name from elsewhere never reaches outside the store.
        assert!(b.path("../../etc/passwd").is_none() && !b.has("..") && b.read("x").is_err());
        b.remove(&h)?;
        assert!(!b.has(&h));
        Ok(())
    }
}
