//! Amphora: the versioned storage's files on Linux through io_uring (Amphora's
//! `docs/design/engine.md`, decision 11): a log's flush is its write and a data flush linked in one
//! submission (`IOSQE_IO_LINK`; the flush runs once the write has ended, and only if it wrote it
//! all), one system call where the portable path ([`Files`]) makes two. Each file has a ring of its
//! own, so lanes flushing side by side never wait on each other's ring (Haas and Leis, "What Modern
//! NVMe Storage Can Do, And How To Exploit It", PVLDB 16(9), 2023: NVMe answers many requests in
//! flight at once, and io_uring is how to put them there). Everything else goes to [`Files`].
//!
//! `unsafe` here (CLAUDE.md §12): a submission entry points at the bytes the kernel writes, and
//! io_uring's push is unsafe because nothing ties those bytes' life to the request's. Each ring
//! writes from a buffer of its own, which lives until both of the submission's completions are
//! reaped; a submission whose end can't be known leaks that buffer and the ring is used no more.

use super::disk::{io, Disk, File, Files};
use crate::error::LimboError;
use crate::sync::Mutex;
use crate::Result;
use io_uring::{opcode, squeue, types, IoUring};
use std::os::fd::AsRawFd;

/// A submission's two entries: the write and the flush linked after it (a format value: what one
/// submission carries).
const ENTRIES: u32 = 2;
const WRITE: u64 = 1;
const FLUSH: u64 = 2;

/// A file's ring and the buffer its writes come from.
struct Ring {
    ring: IoUring,
    buf: Vec<u8>,
    /// A submission's end is unknown: the ring is used no more.
    lost: bool,
}

/// A database's files in a directory, its logs' flushes through io_uring.
pub struct Uring {
    files: Files,
    rings: Vec<Mutex<Ring>>,
}

impl Uring {
    /// The files in `dir` ([`Files::open`]), with a ring for each; refused
    /// (`IoBackendUnavailable`) where the kernel offers no io_uring (before 5.1, or denied it).
    pub fn open(dir: &std::path::Path, lanes: usize) -> Result<Self> {
        let files = Files::open(dir, lanes)?;
        let mut rings = Vec::with_capacity(File::count(files.lanes())?);
        for _ in 0..File::count(files.lanes())? {
            let ring = IoUring::new(ENTRIES)
                .map_err(|e| LimboError::IoBackendUnavailable(format!("io_uring: {e}")))?;
            rings.push(Mutex::new(Ring {
                ring,
                buf: Vec::new(),
                lost: false,
            }));
        }
        Ok(Self { files, rings })
    }

    fn ring(&self, file: File) -> Result<&Mutex<Ring>> {
        self.rings
            .get(file.index(self.files.lanes())?)
            .ok_or_else(|| LimboError::InternalError("versioned storage: a missing ring".into()))
    }
}

/// The results of the write and the flush of one submission, waiting for both: a wait a signal
/// cuts short is waited again, and each wait ends at a completion or a signal.
fn reap(ring: &mut IoUring) -> std::io::Result<(i32, i32)> {
    let (mut wrote, mut flushed) = (None, None);
    while wrote.is_none() || flushed.is_none() {
        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        for cqe in ring.completion() {
            match cqe.user_data() {
                WRITE => wrote = Some(cqe.result()),
                FLUSH => flushed = Some(cqe.result()),
                _ => {}
            }
        }
    }
    Ok((wrote.unwrap_or(0), flushed.unwrap_or(0)))
}

fn errno(what: &str, result: i32) -> LimboError {
    io(
        what,
        std::io::Error::from_raw_os_error(result.checked_neg().unwrap_or(i32::MAX)),
    )
}

impl Disk for Uring {
    fn lanes(&self) -> usize {
        self.files.lanes()
    }

    fn read_at(&self, file: File, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.files.read_at(file, offset, buf)
    }

    fn write_at(&self, file: File, offset: u64, data: &[u8]) -> Result<()> {
        self.files.write_at(file, offset, data)
    }

    fn sync(&self, file: File) -> Result<()> {
        self.files.sync(file)
    }

    fn len(&self, file: File) -> Result<u64> {
        self.files.len(file)
    }

    fn truncate(&self, file: File, len: u64) -> Result<()> {
        self.files.truncate(file, len)
    }

    /// The write and its flush in one submission; a write the kernel ends short (which cancels the
    /// flush) is submitted again from where it ended, so each pass writes a byte or more, or fails.
    fn write_sync(&self, file: File, offset: u64, data: &[u8]) -> Result<()> {
        let fd = types::Fd(self.files.file(file)?.as_raw_fd());
        let mut guard = self.ring(file)?.lock();
        let r = &mut *guard;
        if r.lost {
            return Err(LimboError::InternalError(
                "versioned storage: an io_uring submission never ended; the ring is used no more"
                    .into(),
            ));
        }
        r.buf.clear();
        r.buf.extend_from_slice(data);
        let mut done = 0usize;
        while done < r.buf.len() {
            let rest = r.buf.get(done..).unwrap_or_default();
            let len = u32::try_from(rest.len()).unwrap_or(u32::MAX);
            let at = offset
                .checked_add(u64::try_from(done).map_err(|_| LimboError::IntegerOverflow)?)
                .ok_or(LimboError::IntegerOverflow)?;
            let entries = [
                opcode::Write::new(fd, rest.as_ptr(), len)
                    .offset(at)
                    .build()
                    .flags(squeue::Flags::IO_LINK)
                    .user_data(WRITE),
                opcode::Fsync::new(fd)
                    .flags(types::FsyncFlags::DATASYNC)
                    .build()
                    .user_data(FLUSH),
            ];
            // SAFETY: the write's entry points into `r.buf`, which is neither changed nor freed
            // until both entries' completions are reaped below; if that can't be known, `r.buf` is
            // leaked and the ring marked lost, so the bytes outlive the request whatever it does.
            unsafe { r.ring.submission().push_multiple(&entries) }.map_err(|_| {
                LimboError::InternalError("versioned storage: an io_uring queue was full".into())
            })?;
            let (wrote, flushed) = match reap(&mut r.ring) {
                Ok(results) => results,
                Err(e) => {
                    r.lost = true;
                    // `Vec::leak` keeps the allocation where it is (`into_boxed_slice` would
                    // shrink it to fit, moving the bytes and freeing the ones the kernel reads).
                    let _kept: &mut [u8] = std::mem::take(&mut r.buf).leak();
                    return Err(io("an io_uring submission", e));
                }
            };
            if wrote < 0 {
                return Err(errno("a write", wrote));
            }
            if wrote == 0 {
                return Err(io(
                    "a write",
                    std::io::Error::from(std::io::ErrorKind::WriteZero),
                ));
            }
            done = done
                .checked_add(usize::try_from(wrote).map_err(|_| LimboError::IntegerOverflow)?)
                .ok_or(LimboError::IntegerOverflow)?;
            if done >= r.buf.len() && flushed < 0 {
                return Err(errno("a flush", flushed));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Do: write and flush a lane's record through the ring, then through the portable path at
    /// its end, and read the file back. Expect: both writes, in place (decision 11).
    #[test]
    fn a_write_and_its_flush_in_one_submission_land_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let d = match Uring::open(dir.path(), 2) {
            Ok(d) => d,
            // A kernel that denies io_uring (a container's seccomp) refuses it, typed.
            Err(LimboError::IoBackendUnavailable(_)) => return,
            Err(e) => panic!("{e}"),
        };
        let lane = File::Redo { lane: 1, turn: 0 };
        d.write_sync(lane, 0, b"first").unwrap();
        d.files.write_sync(lane, 5, b"second").unwrap();
        d.write_sync(lane, 11, &vec![7u8; 70_000]).unwrap();
        let mut buf = vec![0u8; 11 + 70_000];
        assert_eq!(d.read_at(lane, 0, &mut buf).unwrap(), buf.len());
        assert_eq!(&buf[..11], b"firstsecond");
        assert!(buf[11..].iter().all(|&b| b == 7));
    }
}
