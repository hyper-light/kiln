//! The files sync reads and writes, as one seam: a copy's folder, its contents store and its saved
//! state. In production it's the machine's own filesystem ([`Std`]); kiln-sim's models give the
//! engine a simulated disk instead (CP-T6.1), whose power losses, fsync errors and full disks
//! show what the engine leaves behind when they strike at each of its writes.
//!
//! Each operation is the one `std::fs` has, by the same name and with the same meaning, so the
//! engine's code reads as it did; what it doesn't fsync, nothing here fsyncs for it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Link,
    /// A socket, a device, a fifo: never synced.
    Other,
}

/// A path's metadata, as `symlink_metadata` gives it: a link is itself, not what it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    pub len: u64,
    /// Its modification time, in nanoseconds since the Unix epoch (0 when the system can't say).
    pub mtime_ns: i128,
    /// Whether anyone may run it (the exec bit; never on Windows).
    pub exec: bool,
}

impl Meta {
    pub fn is_file(&self) -> bool {
        self.kind == Kind::File
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn is_symlink(&self) -> bool {
        self.kind == Kind::Link
    }
}

/// A file being written: what `File::create` gives.
pub trait Written: io::Write + Send {
    /// Its data and metadata durable (`File::sync_all`).
    fn sync_all(&mut self) -> io::Result<()>;
}

/// The filesystem sync works on.
pub trait Fs: Send + Sync + std::fmt::Debug {
    /// `std::fs::symlink_metadata`.
    fn meta(&self, path: &Path) -> io::Result<Meta>;
    /// `File::open`, to read.
    fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read + Send>>;
    /// `File::create`.
    fn create(&self, path: &Path) -> io::Result<Box<dyn Written>>;
    /// `Path::exists`: whether something is at `path`, following a link to it.
    fn exists(&self, path: &Path) -> bool;
    /// `std::fs::read_link`.
    fn read_link(&self, path: &Path) -> io::Result<PathBuf>;
    /// `std::fs::read_dir`: each name in `dir`, and what it is (not following links).
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<(String, Kind)>>;
    /// `std::fs::copy`: `to` holds `from`'s bytes and permissions.
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
    /// Sets or clears the exec bit (nothing on Windows).
    fn set_exec(&self, path: &Path, exec: bool) -> io::Result<()>;
    /// A link at `path` naming `target` (refused where links aren't synced).
    fn symlink(&self, target: &str, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir(&self, path: &Path) -> io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    /// A folder's changes of names durable (`File::open(dir)?.sync_all()`).
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    /// A name for a temporary file no other has.
    fn temp_id(&self) -> String;
}

/// `path`'s bytes.
pub fn read(fs: &dyn Fs, path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    fs.open(path)?.read_to_end(&mut out)?;
    Ok(out)
}

/// Whether `path` is a file (a link to one isn't).
pub fn is_file(fs: &dyn Fs, path: &Path) -> bool {
    fs.meta(path).is_ok_and(|m| m.is_file())
}

/// The machine's own filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct Std;

/// The machine's own filesystem, as sync's types hold it.
pub fn std_fs() -> Arc<dyn Fs> {
    Arc::new(Std)
}

impl Written for std::fs::File {
    fn sync_all(&mut self) -> io::Result<()> {
        std::fs::File::sync_all(self)
    }
}

impl Fs for Std {
    fn meta(&self, path: &Path) -> io::Result<Meta> {
        let m = std::fs::symlink_metadata(path)?;
        let ft = m.file_type();
        let kind = if ft.is_symlink() {
            Kind::Link
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        let mtime_ns = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX));
        Ok(Meta { kind, len: m.len(), mtime_ns, exec: super::scan::executable(&m) })
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read + Send>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }

    fn create(&self, path: &Path) -> io::Result<Box<dyn Written>> {
        Ok(Box::new(std::fs::File::create(path)?))
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(path)
    }

    fn read_dir(&self, dir: &Path) -> io::Result<Vec<(String, Kind)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let kind = if ft.is_symlink() {
                Kind::Link
            } else if ft.is_dir() {
                Kind::Dir
            } else if ft.is_file() {
                Kind::File
            } else {
                Kind::Other
            };
            out.push((entry.file_name().to_string_lossy().into_owned(), kind));
        }
        Ok(out)
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        std::fs::copy(from, to)
    }

    #[cfg(unix)]
    fn set_exec(&self, path: &Path, exec: bool) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let mut p = std::fs::metadata(path)?.permissions();
        let mode = p.mode();
        p.set_mode(if exec { mode | 0o111 } else { mode & !0o111 });
        std::fs::set_permissions(path, p)
    }

    #[cfg(not(unix))]
    fn set_exec(&self, _path: &Path, _exec: bool) -> io::Result<()> {
        Ok(())
    }

    #[cfg(unix)]
    fn symlink(&self, target: &str, path: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, path)
    }

    #[cfg(not(unix))]
    fn symlink(&self, _target: &str, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "links aren't synced on this system yet"))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    #[cfg(unix)]
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        std::fs::File::open(dir)?.sync_all()
    }

    #[cfg(not(unix))]
    fn sync_dir(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }

    fn temp_id(&self) -> String {
        crate::util::new_id()
    }
}
