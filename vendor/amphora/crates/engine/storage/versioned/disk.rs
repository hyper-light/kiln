//! Amphora: the versioned storage's files (Amphora's `docs/design/engine.md`, decisions 6 to 8),
//! behind a seam: positional reads and writes, flushes, lengths and cuts of a database's three
//! files ([`File`]). [`Files`] keeps them in a directory; [`Simulated`] keeps them in memory, its
//! writes durable only once flushed, and can crash after any number of operations, keeping of each
//! write since its file's last flush all of it, none of it, or a prefix of its sectors, as a power
//! cut may (CLAUDE.md §9: a simulator beside the real thing).

use crate::error::LimboError;
use crate::sync::Mutex;
use crate::Result;

/// A database's files.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum File {
    /// The pages, and the header ahead of them.
    Data,
    /// The redo log's two files, taking turns (decision 6).
    Redo,
    RedoNext,
    /// The checkpoint journals, taking turns: each a checkpoint's pages, its list of rows to
    /// correct and its timestamp (decision 7).
    Journal,
    JournalNext,
}

/// The files there are.
const FILES: usize = 5;

impl File {
    fn index(self) -> usize {
        match self {
            File::Data => 0,
            File::Redo => 1,
            File::RedoNext => 2,
            File::Journal => 3,
            File::JournalNext => 4,
        }
    }
}

/// A database's files, as the storage reads and writes them.
pub trait Disk: Send + Sync {
    /// Reads into `buf` from `offset`: the bytes read, fewer past the file's end.
    fn read_at(&self, file: File, offset: u64, buf: &mut [u8]) -> Result<usize>;
    /// Writes `data` at `offset`, growing the file if it ends before.
    fn write_at(&self, file: File, offset: u64, data: &[u8]) -> Result<()>;
    /// Makes every write to `file` so far durable.
    fn sync(&self, file: File) -> Result<()>;
    fn len(&self, file: File) -> Result<u64>;
    /// Cuts `file` to `len` bytes, durably once synced.
    fn truncate(&self, file: File, len: u64) -> Result<()>;
}

fn io(what: &str, e: std::io::Error) -> LimboError {
    LimboError::InternalError(format!("versioned storage: {what}: {e}"))
}

/// The files in a directory: `data`, `redo`, `redo-next`, `journal` and `journal-next`.
pub struct Files {
    files: [std::fs::File; FILES],
}

impl Files {
    /// Opens (creating) the files in `dir`.
    pub fn open(dir: &std::path::Path) -> Result<Self> {
        let open = |name: &str| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(name))
                .map_err(|e| io(name, e))
        };
        Ok(Self {
            files: [
                open("data")?,
                open("redo")?,
                open("redo-next")?,
                open("journal")?,
                open("journal-next")?,
            ],
        })
    }

    fn file(&self, file: File) -> Result<&std::fs::File> {
        self.files
            .get(file.index())
            .ok_or_else(|| LimboError::InternalError("versioned storage: a missing file".into()))
    }
}

#[cfg(unix)]
fn pread(f: &std::fs::File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let at = offset.saturating_add(u64::try_from(done).unwrap_or(u64::MAX));
        let rest = buf.get_mut(done..).unwrap_or_default();
        match f.read_at(rest, at)? {
            0 => break,
            n => done = done.saturating_add(n),
        }
    }
    Ok(done)
}

#[cfg(unix)]
fn pwrite(f: &std::fs::File, offset: u64, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(data, offset)
}

#[cfg(windows)]
fn pread(f: &std::fs::File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let at = offset.saturating_add(u64::try_from(done).unwrap_or(u64::MAX));
        let rest = buf.get_mut(done..).unwrap_or_default();
        match f.seek_read(rest, at)? {
            0 => break,
            n => done = done.saturating_add(n),
        }
    }
    Ok(done)
}

#[cfg(windows)]
fn pwrite(f: &std::fs::File, offset: u64, data: &[u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < data.len() {
        let at = offset.saturating_add(u64::try_from(done).unwrap_or(u64::MAX));
        let rest = data.get(done..).unwrap_or_default();
        match f.seek_write(rest, at)? {
            0 => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero)),
            n => done = done.saturating_add(n),
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn pread(_: &std::fs::File, _: u64, _: &mut [u8]) -> std::io::Result<usize> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "positional reads on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn pwrite(_: &std::fs::File, _: u64, _: &[u8]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "positional writes on this platform",
    ))
}

impl Disk for Files {
    fn read_at(&self, file: File, offset: u64, buf: &mut [u8]) -> Result<usize> {
        pread(self.file(file)?, offset, buf).map_err(|e| io("a read", e))
    }

    fn write_at(&self, file: File, offset: u64, data: &[u8]) -> Result<()> {
        pwrite(self.file(file)?, offset, data).map_err(|e| io("a write", e))
    }

    fn sync(&self, file: File) -> Result<()> {
        self.file(file)?.sync_data().map_err(|e| io("a flush", e))
    }

    fn len(&self, file: File) -> Result<u64> {
        Ok(self
            .file(file)?
            .metadata()
            .map_err(|e| io("a length", e))?
            .len())
    }

    fn truncate(&self, file: File, len: u64) -> Result<()> {
        self.file(file)?.set_len(len).map_err(|e| io("a cut", e))
    }
}

/// Bytes a simulated power cut keeps or loses together.
const SECTOR: usize = 512;

/// A simulated file: what is durable, what reads see now, and the writes since the last flush.
#[derive(Clone, Default)]
struct SimFile {
    durable: Vec<u8>,
    now: Vec<u8>,
    pending: Vec<Pending>,
}

/// A write or cut since a file's last flush.
#[derive(Clone)]
enum Pending {
    Write(u64, Vec<u8>),
    Cut(u64),
}

fn put(bytes: &mut Vec<u8>, offset: u64, data: &[u8]) -> Result<()> {
    let start = usize::try_from(offset).map_err(|_| LimboError::IntegerOverflow)?;
    let end = start
        .checked_add(data.len())
        .ok_or(LimboError::IntegerOverflow)?;
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
    bytes
        .get_mut(start..end)
        .ok_or(LimboError::IntegerOverflow)?
        .copy_from_slice(data);
    Ok(())
}

struct SimState {
    files: [SimFile; FILES],
    /// Operations left before the simulated crash (None: never).
    budget: Option<u64>,
    crashed: bool,
    seed: u64,
}

/// The files in memory, for tests: durable once flushed, crashing on demand.
pub struct Simulated {
    state: Mutex<SimState>,
}

impl Simulated {
    pub fn new(seed: u64) -> Self {
        Self {
            state: Mutex::new(SimState {
                files: Default::default(),
                budget: None,
                crashed: false,
                seed: seed | 1,
            }),
        }
    }

    /// Fails every operation after `ops` more, as a process that stopped there would.
    pub fn crash_after(&self, ops: u64) {
        self.state.lock().budget = Some(ops);
    }

    /// The power cut: each file back to what was flushed, plus, of each write since, all, none or
    /// a prefix of its sectors (cuts since are kept or not); operations work again.
    pub fn power_cut(&self) -> Result<()> {
        let mut s = self.state.lock();
        let mut seed = s.seed;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for f in &mut s.files {
            let mut image = f.durable.clone();
            for p in &f.pending {
                match p {
                    Pending::Write(offset, data) => {
                        let sectors = data.len().div_ceil(SECTOR);
                        let kept = match next() % 3 {
                            0 => 0,
                            1 => sectors,
                            _ => usize::try_from(
                                next()
                                    .checked_rem(
                                        u64::try_from(sectors.saturating_add(1)).unwrap_or(1),
                                    )
                                    .unwrap_or(0),
                            )
                            .unwrap_or(0),
                        };
                        let bytes = kept.saturating_mul(SECTOR).min(data.len());
                        put(&mut image, *offset, data.get(..bytes).unwrap_or_default())?;
                    }
                    Pending::Cut(len) => {
                        if next().is_multiple_of(2) {
                            image.truncate(
                                usize::try_from(*len).map_err(|_| LimboError::IntegerOverflow)?,
                            );
                        }
                    }
                }
            }
            f.durable = image.clone();
            f.now = image;
            f.pending.clear();
        }
        s.seed = seed;
        s.budget = None;
        s.crashed = false;
        Ok(())
    }

    fn spend(s: &mut SimState) -> Result<()> {
        if s.crashed {
            return Err(LimboError::InternalError(
                "versioned storage: the simulated disk crashed".into(),
            ));
        }
        if let Some(b) = s.budget.as_mut() {
            if *b == 0 {
                s.crashed = true;
                return Err(LimboError::InternalError(
                    "versioned storage: the simulated disk crashed".into(),
                ));
            }
            *b = b.saturating_sub(1);
        }
        Ok(())
    }

    fn file(s: &mut SimState, file: File) -> Result<&mut SimFile> {
        s.files
            .get_mut(file.index())
            .ok_or_else(|| LimboError::InternalError("versioned storage: a missing file".into()))
    }
}

impl Disk for Simulated {
    fn read_at(&self, file: File, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut s = self.state.lock();
        Self::spend(&mut s)?;
        let f = Self::file(&mut s, file)?;
        let start = usize::try_from(offset).map_err(|_| LimboError::IntegerOverflow)?;
        let have = f.now.get(start..).unwrap_or_default();
        let n = have.len().min(buf.len());
        buf.get_mut(..n)
            .ok_or(LimboError::IntegerOverflow)?
            .copy_from_slice(have.get(..n).unwrap_or_default());
        Ok(n)
    }

    fn write_at(&self, file: File, offset: u64, data: &[u8]) -> Result<()> {
        let mut s = self.state.lock();
        Self::spend(&mut s)?;
        let f = Self::file(&mut s, file)?;
        put(&mut f.now, offset, data)?;
        f.pending.push(Pending::Write(offset, data.to_vec()));
        Ok(())
    }

    fn sync(&self, file: File) -> Result<()> {
        let mut s = self.state.lock();
        Self::spend(&mut s)?;
        let f = Self::file(&mut s, file)?;
        f.durable = f.now.clone();
        f.pending.clear();
        Ok(())
    }

    fn len(&self, file: File) -> Result<u64> {
        let mut s = self.state.lock();
        Self::spend(&mut s)?;
        let f = Self::file(&mut s, file)?;
        u64::try_from(f.now.len()).map_err(|_| LimboError::IntegerOverflow)
    }

    fn truncate(&self, file: File, len: u64) -> Result<()> {
        let mut s = self.state.lock();
        Self::spend(&mut s)?;
        let f = Self::file(&mut s, file)?;
        f.now
            .truncate(usize::try_from(len).map_err(|_| LimboError::IntegerOverflow)?);
        f.pending.push(Pending::Cut(len));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Do: write two files, flush one, cut the power. Expect: the flushed file as written; the
    /// other back to what was flushed before (nothing), or a prefix of its sectors.
    #[test]
    fn a_power_cut_keeps_what_was_flushed() {
        for seed in 0..20 {
            let d = Simulated::new(seed);
            d.write_at(File::Data, 0, &[1; 1024]).unwrap();
            d.sync(File::Data).unwrap();
            d.write_at(File::Redo, 0, &[2; 2048]).unwrap();
            d.power_cut().unwrap();
            let mut buf = vec![0; 4096];
            assert_eq!(d.read_at(File::Data, 0, &mut buf).unwrap(), 1024);
            assert!(buf[..1024].iter().all(|&b| b == 1));
            let n = d.read_at(File::Redo, 0, &mut buf).unwrap();
            assert!(n.is_multiple_of(SECTOR) && n <= 2048, "{n} bytes kept");
            assert!(buf[..n].iter().all(|&b| b == 2));
        }
    }

    /// Do: a budget of three operations. Expect: the fourth fails, and every one after, until the
    /// power cut.
    #[test]
    fn a_crash_stops_every_operation_until_the_power_cut() {
        let d = Simulated::new(1);
        d.crash_after(3);
        for _ in 0..3 {
            d.write_at(File::Data, 0, b"x").unwrap();
        }
        assert!(d.sync(File::Data).is_err());
        assert!(d.len(File::Data).is_err());
        d.power_cut().unwrap();
        assert!(d.len(File::Data).is_ok());
    }

    /// Do: write, read, cut and flush a real file. Expect: what was written, then what was kept.
    #[test]
    fn files_in_a_directory_read_back_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let d = Files::open(dir.path()).unwrap();
        d.write_at(File::Journal, 10, b"hello").unwrap();
        d.sync(File::Journal).unwrap();
        let mut buf = [0u8; 15];
        assert_eq!(d.read_at(File::Journal, 0, &mut buf).unwrap(), 15);
        assert_eq!(&buf[10..], b"hello");
        d.truncate(File::Journal, 12).unwrap();
        assert_eq!(d.len(File::Journal).unwrap(), 12);
    }
}
