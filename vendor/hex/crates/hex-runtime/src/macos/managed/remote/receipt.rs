//! SAME helper startup receipt, extended in place with its complete native-finalization snapshot.
//! Names locate private metadata; file identity, UID/mode, process birth and native facts grant authority.
use super::*;
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use std::io::{Seek, SeekFrom};

const HEADER: usize = 4 + 2 + 4 + 8 + 32;
const CLOSED: &[u8; 4] = b"DONE";
const FILE_ID_BYTES: usize = 2 * size_of::<u64>();
const CLOSED_FIXED: usize = 4 + 1 + FILE_ID_BYTES + size_of::<u32>();
const CLOSED_REST: usize = CLOSED_FIXED - 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileId {
    device: u64,
    inode: u64,
}
fn identity(stat: &rustix::fs::Stat) -> Result<FileId, Refusal> {
    Ok(FileId {
        device: u64::try_from(stat.st_dev)
            .map_err(|_| super::super::configuration("private metadata device width"))?,
        inode: stat.st_ino,
    })
}
fn owned(stat: &rustix::fs::Stat, kind: FileType) -> Result<(), Refusal> {
    if stat.st_uid != rustix::process::geteuid().as_raw()
        || FileType::from_raw_mode(stat.st_mode) != kind
        || stat.st_mode & (libc::S_IRWXG | libc::S_IRWXO) != 0
    {
        return Err(super::super::configuration(
            "native helper metadata is not private to the same UID",
        ));
    }
    Ok(())
}

struct Parent {
    directory: OwnedFd,
}
impl Parent {
    fn open(path: &Path) -> Result<Self, Refusal> {
        let parent = path
            .parent()
            .ok_or_else(|| super::super::configuration("private metadata parent absent"))?;
        let directory = rustix::fs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| Refusal::os("opening private helper metadata parent", error))?;
        owned(
            &rustix::fs::fstat(&directory)
                .map_err(|error| Refusal::os("checking private helper parent", error))?,
            FileType::Directory,
        )?;
        Ok(Self { directory })
    }
    fn name(path: &Path) -> Result<&std::ffi::OsStr, Refusal> {
        path.file_name()
            .ok_or_else(|| super::super::configuration("private metadata name absent"))
    }
    fn stat(&self, path: &Path) -> Result<Option<rustix::fs::Stat>, Refusal> {
        match rustix::fs::statat(
            &self.directory,
            Self::name(path)?,
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => Ok(Some(stat)),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(Refusal::os("reading private metadata identity", error)),
        }
    }
    fn remove(&self, path: &Path, expected: FileId, kind: FileType) -> Result<(), Refusal> {
        let Some(stat) = self.stat(path)? else {
            return Ok(());
        };
        owned(&stat, kind)?;
        if identity(&stat)? != expected {
            return Err(super::super::configuration(
                "private metadata name was replaced",
            ));
        }
        rustix::fs::unlinkat(&self.directory, Self::name(path)?, AtFlags::empty())
            .map_err(|error| Refusal::os("removing only owned helper metadata", error))?;
        rustix::fs::fsync(&self.directory)
            .map_err(|error| Refusal::os("flushing owned helper metadata removal", error))
    }
}

pub(super) struct Snapshot {
    pub process: ProcId,
    pub completed: Option<protocol::completed::Snapshot>,
    endpoint: Option<FileId>,
    file: File,
    parent: Parent,
    identity: FileId,
}
impl Snapshot {
    pub fn completion(
        &self,
        key: &protocol::Key,
        controller: Option<ProcId>,
    ) -> Result<Option<&protocol::completed::Completed>, Refusal> {
        let Some(completed) = &self.completed else {
            return Ok(None);
        };
        let at = match completed
            .owners
            .binary_search_by(|entry| entry.key.owner.cmp(&key.owner))
        {
            Ok(at) => at,
            Err(_) => return Ok(None),
        };
        let entry = completed
            .owners
            .get(at)
            .ok_or_else(|| native_allocation(completed.owners.len()))?;
        if entry.key != *key
            || controller.is_some_and(|controller| Some(controller) != entry.controller)
        {
            return Err(super::super::configuration(
                "completed native scope/controller differs from canonical owner",
            ));
        }
        Ok(Some(entry))
    }
    pub fn check_live(&self) -> Result<(), Refusal> {
        if self.completed.is_some() {
            return Err(super::super::configuration(
                "native helper is closing or completed",
            ));
        }
        check_process(self.process)
    }
    /// Only a completed native snapshot plus positive absence of that exact helper may retire these
    /// files. Live/unknown receipt, endpoint absence or PID-name alone never grants permission.
    pub fn retire(self, helper: &Helper) -> Result<(), Refusal> {
        if self.completed.is_none() {
            return Err(super::super::configuration(
                "live helper metadata cannot be retired",
            ));
        }
        let pid = i32::try_from(self.process.pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| super::super::configuration("completed helper PID width"))?;
        match sys::rusage(pid)
            .map_err(|error| Refusal::os("proving completed helper absence", error))?
        {
            None => {}
            Some(actual) if actual.started != 0 && actual.started != self.process.start_ticks => {}
            Some(_) => {
                return Err(super::super::configuration(
                    "completed helper is not yet absent/reaped",
                ));
            }
        }
        self.file
            .try_lock()
            .map_err(|_| super::super::configuration("completed helper receipt is still locked"))?;
        let result = (|| {
            if let Some(endpoint) = self.endpoint {
                Parent::open(&helper.config.endpoint)?.remove(
                    &helper.config.endpoint,
                    endpoint,
                    FileType::Socket,
                )?;
            }
            self.parent
                .remove(&helper.config.receipt, self.identity, FileType::RegularFile)
        })();
        unlock_result(&self.file, result)
    }
}

pub(super) fn read(helper: &Helper) -> Result<Option<Snapshot>, Refusal> {
    let parent = Parent::open(&helper.config.receipt)?;
    let fd = match rustix::fs::openat(
        &parent.directory,
        Parent::name(&helper.config.receipt)?,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(Refusal::os("reading private helper receipt", error)),
    };
    let stat = rustix::fs::fstat(&fd)
        .map_err(|error| Refusal::os("checking private helper receipt", error))?;
    owned(&stat, FileType::RegularFile)?;
    let mut file = File::from(fd);
    let mut header = [0u8; HEADER];
    file.read_exact(&mut header)
        .map_err(|error| Refusal::os("reading exact helper startup header", error))?;
    let process = decode_header(&header, helper.fingerprint()?)?;
    let mut flag = [0u8; 4];
    let (completed, endpoint) = match file
        .read(&mut flag)
        .map_err(|error| Refusal::os("reading helper terminal marker", error))?
    {
        0 => (None, None),
        4 if &flag == CLOSED => {
            let mut fixed = [0u8; CLOSED_REST];
            file.read_exact(&mut fixed)
                .map_err(|error| Refusal::os("reading helper terminal boundary", error))?;
            let (flag, rest) = fixed
                .split_first()
                .ok_or_else(|| super::super::configuration("terminal endpoint flag absent"))?;
            let (identity_bytes, length_bytes) =
                rest.split_at_checked(FILE_ID_BYTES).ok_or_else(|| {
                    super::super::configuration("terminal endpoint identity boundary")
                })?;
            let (identity_parts, remainder) = identity_bytes.as_chunks::<8>();
            let [device, inode] = identity_parts else {
                return Err(super::super::configuration(
                    "terminal endpoint identity width",
                ));
            };
            if !remainder.is_empty() {
                return Err(super::super::configuration(
                    "terminal endpoint identity remainder",
                ));
            }
            let endpoint = match *flag {
                0 if *device == [0; 8] && *inode == [0; 8] => None,
                1 => Some(FileId {
                    device: u64::from_le_bytes(*device),
                    inode: u64::from_le_bytes(*inode),
                }),
                _ => {
                    return Err(super::super::configuration(
                        "terminal endpoint identity flag invalid",
                    ));
                }
            };
            let length =
                usize::try_from(u32::from_le_bytes(length_bytes.try_into().map_err(
                    |_| super::super::configuration("terminal helper frame width"),
                )?))
                .map_err(|_| native_allocation(usize::MAX))?;
            let bound = protocol::completed::message_bound(crate::MAX_BOXES)
                .map_err(|_| native_allocation(crate::MAX_BOXES))?;
            if length > bound {
                return Err(native_allocation(bound));
            }
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(length)
                .map_err(|_| native_allocation(length))?;
            bytes.resize(length, 0);
            file.read_exact(&mut bytes)
                .map_err(|error| Refusal::os("reading bounded helper terminal receipts", error))?;
            let (version, completed) = protocol::completed::read(&bytes, 0, crate::MAX_BOXES)
                .map_err(|_| {
                    super::super::configuration("invalid completed native helper snapshot")
                })?;
            if version != VERSION || completed.helper != process {
                return Err(super::super::configuration(
                    "completed helper incarnation changed",
                ));
            }
            let mut extra = [0u8; 1];
            if file
                .read(&mut extra)
                .map_err(|error| Refusal::os("reading completed receipt boundary", error))?
                != 0
            {
                return Err(super::super::configuration("trailing helper receipt bytes"));
            }
            (Some(completed), endpoint)
        }
        _ => {
            return Err(super::super::configuration(
                "partial or invalid helper completion publication",
            ));
        }
    };
    Ok(Some(Snapshot {
        process,
        completed,
        endpoint,
        identity: identity(&stat)?,
        file,
        parent,
    }))
}

fn decode_header(bytes: &[u8; HEADER], fingerprint: [u8; 32]) -> Result<ProcId, Refusal> {
    // Fixed shell envelope; the structured native completion body reuses the pure helper codec.
    let mut at = 0usize;
    let mut take = |length: usize| -> Result<&[u8], Refusal> {
        let end = at
            .checked_add(length)
            .ok_or_else(|| native_allocation(usize::MAX))?;
        let value = bytes
            .get(at..end)
            .ok_or_else(|| super::super::configuration("truncated helper receipt header"))?;
        at = end;
        Ok(value)
    };
    if take(4)? != b"HEXH" || take(2)? != VERSION.to_le_bytes() {
        return Err(super::super::configuration(
            "helper startup receipt format changed",
        ));
    }
    let process = ProcId {
        pid: u32::from_le_bytes(
            take(4)?
                .try_into()
                .map_err(|_| super::super::configuration("helper PID width"))?,
        ),
        start_ticks: u64::from_le_bytes(
            take(8)?
                .try_into()
                .map_err(|_| super::super::configuration("helper birth width"))?,
        ),
    };
    if process.pid == 0 || process.start_ticks == 0 || take(32)? != fingerprint {
        return Err(super::super::configuration(
            "helper startup commitment/incarnation changed",
        ));
    }
    Ok(process)
}

enum Publication {
    Active,
    Pending,
    Published,
}
pub(super) struct Publisher {
    file: File,
    parent: Parent,
    identity: FileId,
    endpoint: Option<FileId>,
    phase: Publication,
}
impl Publisher {
    pub fn new(helper: &Helper, process: ProcId) -> Result<Self, Refusal> {
        let fingerprint = helper.fingerprint()?;
        let parent = Parent::open(&helper.config.receipt)?;
        let fd = rustix::fs::openat(
            &parent.directory,
            Parent::name(&helper.config.receipt)?,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| Refusal::os("claiming private helper startup receipt", error))?;
        let stat = rustix::fs::fstat(&fd)
            .map_err(|error| Refusal::os("checking new helper receipt", error))?;
        owned(&stat, FileType::RegularFile)?;
        let identity = identity(&stat)?;
        let file = File::from(fd);
        file.try_lock()
            .map_err(|_| super::super::configuration("helper receipt ownership collision"))?;
        let publication = Self {
            file,
            parent,
            identity,
            endpoint: None,
            phase: Publication::Active,
        };
        let result = (|| {
            for part in [
                b"HEXH".as_slice(),
                VERSION.to_le_bytes().as_slice(),
                process.pid.to_le_bytes().as_slice(),
                process.start_ticks.to_le_bytes().as_slice(),
                fingerprint.as_slice(),
            ] {
                (&publication.file)
                    .write_all(part)
                    .map_err(|error| Refusal::os("publishing helper startup identity", error))?;
            }
            publication
                .file
                .sync_all()
                .map_err(|error| Refusal::os("flushing helper startup identity", error))?;
            rustix::fs::fsync(&publication.parent.directory)
                .map_err(|error| Refusal::os("flushing helper receipt directory", error))
        })();
        match result {
            Ok(()) => Ok(publication),
            Err(error) => unlock_result(&publication.file, Err(error)),
        }
    }
    pub fn pending(&self) -> bool {
        !matches!(self.phase, Publication::Active)
    }
    pub fn published(&self) -> bool {
        matches!(self.phase, Publication::Published)
    }
    pub fn close_result<T>(&self, result: Result<T, Refusal>) -> Result<T, Refusal> {
        unlock_result(&self.file, result)
    }
    pub fn bound(&mut self, helper: &Helper) -> Result<(), Refusal> {
        let stat = Parent::open(&helper.config.endpoint)?
            .stat(&helper.config.endpoint)?
            .ok_or_else(|| super::super::configuration("bound helper endpoint absent"))?;
        owned(&stat, FileType::Socket)?;
        self.endpoint = Some(identity(&stat)?);
        Ok(())
    }
    pub fn finish(
        &mut self,
        helper: &Helper,
        completed: &protocol::completed::Snapshot,
    ) -> Result<(), Refusal> {
        let endpoint = self.endpoint;
        self.phase = Publication::Pending;
        let bytes =
            protocol::completed::encode(VERSION, completed, crate::MAX_BOXES).map_err(|_| {
                super::super::configuration("encoding completed native helper receipts")
            })?;
        let length = u32::try_from(bytes.len()).map_err(|_| native_allocation(bytes.len()))?;
        let current = self
            .parent
            .stat(&helper.config.receipt)?
            .ok_or_else(|| super::super::configuration("helper receipt disappeared"))?;
        if identity(&current)? != self.identity {
            return Err(super::super::configuration("helper receipt was replaced"));
        }
        self.file
            .set_len(u64::try_from(HEADER).map_err(|_| native_allocation(HEADER))?)
            .map_err(|error| Refusal::os("resetting owned terminal publication", error))?;
        self.file
            .seek(SeekFrom::Start(
                u64::try_from(HEADER).map_err(|_| native_allocation(HEADER))?,
            ))
            .map_err(|error| Refusal::os("locating owned terminal publication", error))?;
        let (flag, device, inode) = match endpoint {
            Some(endpoint) => (1u8, endpoint.device, endpoint.inode),
            None => (0u8, 0u64, 0u64),
        };
        for part in [
            CLOSED.as_slice(),
            [flag].as_slice(),
            device.to_le_bytes().as_slice(),
            inode.to_le_bytes().as_slice(),
            length.to_le_bytes().as_slice(),
            bytes.as_slice(),
        ] {
            self.file.write_all(part).map_err(|error| {
                Refusal::os("publishing completed native helper receipts", error)
            })?;
        }
        self.file
            .sync_all()
            .map_err(|error| Refusal::os("flushing completed native helper receipts", error))?;
        self.phase = Publication::Published;
        if let Some(endpoint) = endpoint {
            Parent::open(&helper.config.endpoint)?.remove(
                &helper.config.endpoint,
                endpoint,
                FileType::Socket,
            )?;
        }
        Ok(())
    }
}

// Explicit unlock precedes File::drop on every owned lock path, including failed publication.
// Keep both causes when publication and unlock fail; neither result grants release authority.
fn unlock_result<T>(file: &File, result: Result<T, Refusal>) -> Result<T, Refusal> {
    match (result, file.unlock()) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(Refusal::os(
            "unlocking owned helper receipt before close",
            error,
        )),
        (Err(original), Err(error)) => Err(Refusal::Os {
            op: "helper publication and unlock failed",
            why: diagnostic(format_args!("original: {original}; unlock: {error}"))?,
        }),
    }
}

#[cfg(test)]
mod controls {
    use super::super::controls::{completed, helper, pool};
    use super::*;

    /// AC-T7.4.1: a partial terminal write never becomes release authority. The SAME publication
    /// owner can retry its exact retained result without starting a helper or redispatching a VMM.
    #[test]
    fn partial_completion_stays_uncertain_until_exact_same_receipt_is_published() {
        let directory = tempfile::tempdir().unwrap();
        let helper = helper(directory.path());
        let expected = completed(&pool());
        let mut publisher = Publisher::new(&helper, expected.helper).unwrap();
        let before = std::fs::read(helper.config.receipt()).unwrap();
        assert_eq!(before.len(), HEADER);
        assert!(read(&helper).unwrap().unwrap().completed.is_none());
        publisher.file.write_all(CLOSED).unwrap();
        publisher.file.write_all(&[1]).unwrap();
        publisher.file.sync_all().unwrap();
        let interrupted = std::fs::read(helper.config.receipt()).unwrap();
        assert!(read(&helper).is_err());
        assert_eq!(std::fs::read(helper.config.receipt()).unwrap(), interrupted);
        publisher.finish(&helper, &expected).unwrap();
        assert!(publisher.published());
        let actual = read(&helper).unwrap().unwrap();
        assert_eq!(actual.completed.as_ref(), Some(&expected));
        let entry = &expected.owners[0];
        assert!(
            actual
                .completion(&entry.key, entry.controller)
                .unwrap()
                .is_some()
        );
        let mut stale = entry.key.try_clone().unwrap();
        stale.scope = hex_core::record::NativeScope::from_bytes([37; 16]);
        assert!(actual.completion(&stale, entry.controller).is_err());
        let reused = entry.controller.map(|controller| ProcId {
            start_ticks: controller.start_ticks + 1,
            ..controller
        });
        assert!(actual.completion(&entry.key, reused).is_err());
        // No native process observation or reap is invoked by this private-file control.
        publisher.close_result(Ok(())).unwrap();
    }

    /// AC-T7.4.1: replaced metadata and changed frozen profiles cannot authorize overwriting or
    /// adopting the original receipt; refusal preserves the replacement's exact bytes.
    #[test]
    fn completion_publication_refuses_replaced_file_and_frozen_profile() {
        let directory = tempfile::tempdir().unwrap();
        let helper = helper(directory.path());
        let expected = completed(&pool());
        let mut publisher = Publisher::new(&helper, expected.helper).unwrap();
        let mut header = std::fs::read(helper.config.receipt()).unwrap();
        header[HEADER - 1] ^= 1;
        let changed: &[u8; HEADER] = header.as_slice().try_into().unwrap();
        assert!(decode_header(changed, helper.fingerprint().unwrap()).is_err());
        std::fs::rename(helper.config.receipt(), directory.path().join("old")).unwrap();
        std::fs::write(helper.config.receipt(), b"foreign replacement").unwrap();
        assert!(publisher.finish(&helper, &expected).is_err());
        assert!(publisher.pending());
        assert!(!publisher.published());
        assert_eq!(
            std::fs::read(helper.config.receipt()).unwrap(),
            b"foreign replacement"
        );
        publisher.close_result(Ok(())).unwrap();
    }
}
