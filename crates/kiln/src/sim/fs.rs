//! Sync's filesystem seam (`crate::sync::fs`) on a kiln-sim disk: what a copy's folder, its
//! contents store and its saved state are on in the sync and moves models. Each write a file
//! takes is a write of the disk's (a crash can come between two), and temporary names are counted,
//! not random, so a seed's run is the same every time.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kiln_sim::disk::{self, Disk};

use crate::sync::fs::{Fs, Kind, Meta, Written};

/// Sync's filesystem, on a simulated disk.
#[derive(Debug, Clone)]
pub struct SimFs {
    pub disk: Disk,
    temps: Arc<AtomicU64>,
}

impl SimFs {
    pub fn new(disk: Disk) -> Self {
        Self { disk, temps: Arc::new(AtomicU64::new(0)) }
    }
}

/// A path as the simulated disk names it: `/`-separated, whatever the platform joined it with.
fn s(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn kind(k: disk::Kind) -> Kind {
    match k {
        disk::Kind::File => Kind::File,
        disk::Kind::Dir => Kind::Dir,
        disk::Kind::Link => Kind::Link,
    }
}

/// A file being written: each write the disk's own (the whole file so far), its fsync the disk's.
struct SimWritten {
    disk: Disk,
    path: String,
    buf: Vec<u8>,
}

impl io::Write for SimWritten {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let mut next = self.buf.clone();
        next.extend_from_slice(b);
        self.disk.write(&self.path, &next)?;
        self.buf = next;
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Written for SimWritten {
    fn sync_all(&mut self) -> io::Result<()> {
        self.disk.fsync(&self.path)
    }
}

impl Fs for SimFs {
    fn meta(&self, path: &Path) -> io::Result<Meta> {
        let m = self.disk.meta(&s(path))?;
        Ok(Meta { kind: kind(m.kind), len: m.len, mtime_ns: m.mtime_ns, exec: m.exec })
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read + Send>> {
        Ok(Box::new(io::Cursor::new(self.disk.read(&s(path))?)))
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn Written>> {
        let path = s(path);
        self.disk.write(&path, b"")?;
        Ok(Box::new(SimWritten { disk: self.disk.clone(), path, buf: Vec::new() }))
    }

    fn exists(&self, path: &Path) -> bool {
        self.disk.exists(&s(path))
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.disk.read_link(&s(path)).map(PathBuf::from)
    }

    fn read_dir(&self, dir: &Path) -> io::Result<Vec<(String, Kind)>> {
        Ok(self.disk.list(&s(dir))?.into_iter().map(|(n, k)| (n, kind(k))).collect())
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        self.disk.copy(&s(from), &s(to))
    }

    fn set_exec(&self, path: &Path, exec: bool) -> io::Result<()> {
        self.disk.set_exec(&s(path), exec)
    }

    fn symlink(&self, target: &str, path: &Path) -> io::Result<()> {
        self.disk.symlink(target, &s(path))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.disk.rename(&s(from), &s(to))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.disk.remove_file(&s(path))
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.disk.remove_dir(&s(path))
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.disk.create_dir_all(&s(path))
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.disk.fsync(&s(path))
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.disk.fsync_dir(&s(dir))
    }

    fn temp_id(&self) -> String {
        format!("t{:08x}", self.temps.fetch_add(1, Ordering::Relaxed))
    }

    fn now_ns(&self) -> i128 {
        self.disk.now_ns()
    }
}
