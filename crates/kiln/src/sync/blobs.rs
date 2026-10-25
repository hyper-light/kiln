//! Contents, addressed by their SHA-256: a step names a file's new contents by hash, and the copy
//! that applies it reads them from here (fetched from a member that has them, when it doesn't).

use std::collections::BTreeSet;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest as _, Sha256};

use super::fs::{Fs, Kind, Std};

/// Read and hashed at a time.
const CHUNK: usize = 64 * 1024;

/// A content-addressed store: `<dir>/<first two hex digits>/<hash>`, on a filesystem (the
/// machine's own, or a simulated one: `super::fs`).
#[derive(Clone)]
pub struct Blobs {
    dir: PathBuf,
    fs: Arc<dyn Fs>,
    /// Its folders whose names changed (contents renamed in, a folder made for them) since a copy
    /// last took them ([`Blobs::take_touched`]): synced with the copy's own before its state names
    /// what's in them. Clones share it.
    touched: Arc<parking_lot::Mutex<BTreeSet<PathBuf>>>,
}

impl std::fmt::Debug for Blobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blobs").field("dir", &self.dir).finish_non_exhaustive()
    }
}

/// A SHA-256 as a store names it: 64 lowercase hex digits, so a name from elsewhere can never
/// reach outside the store.
pub fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The SHA-256 of a file's contents.
pub fn hash_file(path: &Path) -> io::Result<String> {
    hash_file_on(&Std, path)
}

/// The SHA-256 of the contents of a file on `fs`.
pub fn hash_file_on(fs: &dyn Fs, path: &Path) -> io::Result<String> {
    let mut f = fs.open(path)?;
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
        Self::on(super::fs::std_fs(), dir)
    }

    /// A store at `dir` on `fs`.
    pub fn on(fs: Arc<dyn Fs>, dir: &Path) -> io::Result<Self> {
        fs.create_dir_all(dir)?;
        Ok(Self { dir: dir.to_owned(), fs, touched: Arc::default() })
    }

    /// The folders whose names it changed since this was last asked: what a copy syncs with its
    /// own before its state names what's in them (`Copy::sync_dirs`).
    pub fn take_touched(&self) -> BTreeSet<PathBuf> {
        std::mem::take(&mut *self.touched.lock())
    }

    /// Makes `dest`'s folder, noting the store's own when it's new: its name changed.
    fn make_folder(&self, dest: &Path) -> io::Result<()> {
        if let Some(dir) = dest.parent() {
            let made = self.fs.meta(dir).is_err();
            self.fs.create_dir_all(dir)?;
            if made {
                self.touched.lock().insert(self.dir.clone());
            }
        }
        Ok(())
    }

    /// Notes the folders holding blob `hash` (its own, and the store's, which names that one), to
    /// sync before a state names it: one a process put there before a crash may not be durable yet,
    /// though it's there to see.
    pub fn hold(&self, hash: &str) {
        if let Some(dir) = self.path(hash).as_deref().and_then(Path::parent) {
            let mut touched = self.touched.lock();
            touched.insert(dir.to_owned());
            touched.insert(self.dir.clone());
        }
    }

    /// Notes that a blob was renamed into `dest`'s folder.
    fn renamed_in(&self, dest: &Path) {
        if let Some(dir) = dest.parent() {
            self.touched.lock().insert(dir.to_owned());
        }
    }

    /// The filesystem it's on: what a copy beside it writes its folder on.
    pub fn fs(&self) -> &Arc<dyn Fs> {
        &self.fs
    }

    /// Where a blob is (or would be); `None` for a name that isn't a hash.
    pub fn path(&self, hash: &str) -> Option<PathBuf> {
        valid_hash(hash).then(|| self.dir.join(hash.get(..2).unwrap_or("00")).join(hash))
    }

    pub fn has(&self, hash: &str) -> bool {
        self.path(hash).is_some_and(|p| super::fs::is_file(self.fs.as_ref(), &p))
    }

    /// Keeps a file's contents: its hash, as read (a file changed while it was copied is kept as
    /// it was read, under the hash of that).
    pub fn put_file(&self, src: &Path) -> io::Result<String> {
        let mut f = self.fs.open(src)?;
        self.put_reader(f.as_mut())
    }

    pub fn put_bytes(&self, bytes: &[u8]) -> io::Result<String> {
        self.put_reader(&mut &*bytes)
    }

    fn put_reader(&self, r: &mut dyn io::Read) -> io::Result<String> {
        let tmp = self.dir.join(format!(".in-{}", self.fs.temp_id()));
        let mut out = self.fs.create(&tmp)?;
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
        drop(out);
        if let Err(e) = written {
            let _ = self.fs.remove_file(&tmp);
            return Err(e);
        }
        let hash = hex::encode(h.finalize());
        let Some(dest) = self.path(&hash) else {
            let _ = self.fs.remove_file(&tmp);
            return Err(io::Error::other("a SHA-256 that isn't one"));
        };
        self.make_folder(&dest)?;
        if super::fs::is_file(self.fs.as_ref(), &dest) {
            self.fs.remove_file(&tmp)?;
        } else {
            self.fs.rename(&tmp, &dest)?;
            self.renamed_in(&dest);
        }
        Ok(hash)
    }

    /// A blob's contents.
    pub fn read(&self, hash: &str) -> io::Result<Vec<u8>> {
        let p = self.path(hash).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a SHA-256"))?;
        super::fs::read(self.fs.as_ref(), &p)
    }

    /// A place for a download on its way in: inside the store, so it moves in with a rename.
    pub fn incoming(&self) -> PathBuf {
        self.dir.join(format!(".incoming-{}", self.fs.temp_id()))
    }

    /// Takes a file as blob `hash` if that is what it holds (moved in: true); else drops it.
    pub fn adopt(&self, file: &Path, hash: &str) -> io::Result<bool> {
        let Some(dest) = self.path(hash) else {
            let _ = self.fs.remove_file(file);
            return Ok(false);
        };
        if hash_file_on(self.fs.as_ref(), file)? != hash {
            self.fs.remove_file(file)?;
            return Ok(false);
        }
        self.make_folder(&dest)?;
        self.fs.rename(file, &dest)?;
        self.renamed_in(&dest);
        Ok(true)
    }

    /// Bytes free on the volume the store is on, where the system says.
    pub fn free_bytes(&self) -> Option<u64> {
        crate::os::disk_available(&self.dir)
    }

    /// Every blob held, and when it was last written.
    pub fn all(&self) -> io::Result<Vec<(String, std::time::SystemTime)>> {
        let mut out = Vec::new();
        for (shard, kind) in self.fs.read_dir(&self.dir)? {
            if kind != Kind::Dir {
                continue;
            }
            let shard = self.dir.join(shard);
            for (name, _) in self.fs.read_dir(&shard)? {
                if valid_hash(&name) {
                    let ns = self.fs.meta(&shard.join(&name))?.mtime_ns;
                    let since = std::time::Duration::from_nanos(u64::try_from(ns).unwrap_or(0));
                    out.push((name, std::time::UNIX_EPOCH.checked_add(since).unwrap_or(std::time::UNIX_EPOCH)));
                }
            }
        }
        Ok(out)
    }

    /// Drops a blob nothing needs any more.
    pub fn remove(&self, hash: &str) -> io::Result<()> {
        match self.path(hash) {
            Some(p) if super::fs::is_file(self.fs.as_ref(), &p) => self.fs.remove_file(&p),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_room_left_is_known_where_the_system_says() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let blobs = Blobs::new(dir.path())?;
        assert!(blobs.free_bytes().is_some());
        Ok(())
    }

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
