// kiln: its shared-WAL locks are unused in kiln's build, which would warn on Windows.
#![allow(dead_code)]

use crate::error::io_error;
use crate::io::{FileId, SharedWalMappedRegion};
use crate::sync::{Arc, Mutex};
use crate::{LimboError, Result};
use std::collections::HashMap;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::OnceLock;
#[cfg(test)]
use std::{cell::RefCell, collections::VecDeque};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_LOCK_VIOLATION, ERROR_NOT_LOCKED, FALSE, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, TRUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_END_OF_FILE_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileEndOfFileInfo,
    GetFileInformationByHandle, GetFinalPathNameByHandleW, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, OPEN_EXISTING, ReOpenFile, SetFileInformationByHandle,
    UnlockFileEx,
};
use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

/// CP-T1.4: one mapping owner for both Windows backends. The borrowed file handle is never
/// closed here; only the mapping object and its view belong to this region.
struct WindowsSharedWalMapping {
    _mapping_handle: OwnedHandle,
    view_ptr: NonNull<u8>,
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping remains valid until this owner drops, and shared-WAL callers coordinate
// accesses through the same byte locks and atomic protocol as the other platform backends.
unsafe impl Send for WindowsSharedWalMapping {}
// SAFETY: the shared-WAL lock/atomic protocol also coordinates access through shared references.
unsafe impl Sync for WindowsSharedWalMapping {}

impl SharedWalMappedRegion for WindowsSharedWalMapping {
    fn ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for WindowsSharedWalMapping {
    fn drop(&mut self) {
        // SAFETY: view_ptr is the exact successful MapViewOfFile result and this owner unmaps
        // it once. OwnedHandle closes the mapping object after the view is released.
        if unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view_ptr.as_ptr().cast(),
            })
        } == FALSE
        {
            tracing::error!(
                "UnmapViewOfFile failed for shared WAL coordination region: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

pub(crate) fn shared_wal_set_len(handle: HANDLE, len: u64) -> Result<()> {
    let file_info = FILE_END_OF_FILE_INFO {
        EndOfFile: i64::try_from(len).map_err(|_| {
            LimboError::InvalidArgument(
                "shared WAL file length exceeds the Windows signed file limit".into(),
            )
        })?,
    };
    let size = u32::try_from(std::mem::size_of::<FILE_END_OF_FILE_INFO>()).map_err(|_| {
        LimboError::InternalError("shared WAL file information size exceeds DWORD".into())
    })?;
    // SAFETY: the caller retains its open file handle for this call, and file_info is a live
    // FILE_END_OF_FILE_INFO with the exact structure size Windows expects.
    if unsafe {
        SetFileInformationByHandle(
            handle,
            FileEndOfFileInfo,
            (&raw const file_info).cast(),
            size,
        )
    } == FALSE
    {
        return Err(io_error(
            std::io::Error::last_os_error(),
            "resize shared WAL coordination file",
        ));
    }
    Ok(())
}

pub(crate) fn shared_wal_map(
    handle: HANDLE,
    offset: u64,
    len: usize,
) -> Result<Box<dyn SharedWalMappedRegion>> {
    if len == 0 {
        return Err(LimboError::InvalidArgument(
            "cannot map shared WAL coordination region with zero length".into(),
        ));
    }
    let len64 = u64::try_from(len)
        .map_err(|_| LimboError::InvalidArgument("shared WAL map length exceeds u64".into()))?;
    let end = offset
        .checked_add(len64)
        .ok_or_else(|| LimboError::InvalidArgument("shared WAL map end overflow".into()))?;
    i64::try_from(end).map_err(|_| {
        LimboError::InvalidArgument("shared WAL map exceeds the Windows signed file limit".into())
    })?;

    // SAFETY: SYSTEM_INFO is an output-only plain Windows structure, fully initialized by
    // GetSystemInfo before its allocation granularity is read.
    let mut system_info = unsafe { std::mem::zeroed::<SYSTEM_INFO>() };
    // SAFETY: system_info points to writable storage of the required size.
    unsafe { GetSystemInfo(&raw mut system_info) };
    let granularity = u64::from(system_info.dwAllocationGranularity);
    let prefix64 = offset.checked_rem(granularity).ok_or_else(|| {
        LimboError::LockingError(
            "failed to determine shared WAL mapping allocation granularity".into(),
        )
    })?;
    let aligned_offset = offset
        .checked_sub(prefix64)
        .ok_or_else(|| LimboError::InternalError("shared WAL aligned offset underflow".into()))?;
    let prefix_len = usize::try_from(prefix64).map_err(|_| {
        LimboError::InvalidArgument("shared WAL map prefix exceeds address space".into())
    })?;
    let view_len = prefix_len
        .checked_add(len)
        .filter(|n| isize::try_from(*n).is_ok())
        .ok_or_else(|| {
            LimboError::InvalidArgument("shared WAL map view exceeds pointer bounds".into())
        })?;
    let offset_high = u32::try_from(aligned_offset >> 32).map_err(|_| {
        LimboError::InvalidArgument("shared WAL map offset high exceeds DWORD".into())
    })?;
    let offset_low = u32::try_from(aligned_offset & u64::from(u32::MAX)).map_err(|_| {
        LimboError::InvalidArgument("shared WAL map offset low exceeds DWORD".into())
    })?;

    // SAFETY: handle is borrowed from the caller's live file. A zero maximum size maps its
    // existing size; MapViewOfFile validates that the requested interval is inside that size.
    let mapping_handle = unsafe {
        CreateFileMappingW(
            handle,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            0,
            std::ptr::null(),
        )
    };
    if mapping_handle.is_null() {
        return Err(io_error(
            std::io::Error::last_os_error(),
            "create shared WAL file mapping",
        ));
    }
    // SAFETY: CreateFileMappingW returned a new non-null owned mapping handle. The borrowed
    // file handle is distinct and is never passed to OwnedHandle.
    let mapping_handle = unsafe { OwnedHandle::from_raw_handle(mapping_handle) };
    // SAFETY: the mapping handle is live, the offset is allocation-granularity aligned, and
    // view_len and its requested suffix are checked against usize/isize and file limits.
    let mapped = unsafe {
        MapViewOfFile(
            mapping_handle.as_raw_handle(),
            FILE_MAP_READ | FILE_MAP_WRITE,
            offset_high,
            offset_low,
            view_len,
        )
    };
    let view_ptr = NonNull::new(mapped.Value.cast::<u8>()).ok_or_else(|| {
        io_error(
            std::io::Error::last_os_error(),
            "map shared WAL coordination file",
        )
    })?;
    // Create the view owner before deriving its suffix so every later error unmaps the view.
    let mut region = WindowsSharedWalMapping {
        _mapping_handle: mapping_handle,
        view_ptr,
        ptr: view_ptr,
        len,
    };
    // SAFETY: len is nonzero, prefix_len < view_len, and the complete mapped view is at most
    // isize::MAX bytes, so this pointer remains inside the successful mapping.
    region.ptr = NonNull::new(unsafe { view_ptr.as_ptr().add(prefix_len) }).ok_or_else(|| {
        LimboError::InternalError("shared WAL mapped suffix pointer is null".into())
    })?;
    Ok(Box::new(region))
}

/// Keep the open-time database lock outside the useful range of every Turso
/// file. Locking the data range itself makes Windows reject I/O through sibling
/// handles in this process.
const PROCESS_LOCK_OFFSET: u64 = 0x4000_0000_0000_0000;

/// Resolve a path once so registry identity is stable after a caller changes
/// its working directory or the file is later removed.
pub(crate) fn stable_lock_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

/// Resolve an identity from the file already opened by either backend so a concurrent
/// process-wide current-directory change cannot redirect later lock operations.
pub(crate) fn stable_lock_path_for_handle(handle: HANDLE, fallback: &Path) -> PathBuf {
    let mut buffer = vec![0_u16; 260];
    let mut length =
        unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0) }
            as usize;
    if length == 0 {
        return stable_lock_path(fallback);
    }
    if length >= buffer.len() {
        buffer.resize(length + 1, 0);
        length = unsafe {
            GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0)
        } as usize;
        if length == 0 || length >= buffer.len() {
            return stable_lock_path(fallback);
        }
    }
    PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]))
}

fn overlapped_at(offset: u64) -> OVERLAPPED {
    OVERLAPPED {
        Internal: 0,
        InternalHigh: 0,
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: offset as u32,
                OffsetHigh: (offset >> 32) as u32,
            },
        },
        hEvent: std::ptr::null_mut(),
    }
}

fn open_lock_handle(path: &Path) -> Result<HANDLE> {
    let path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(LimboError::LockingError(format!(
            "Failed opening lock handle: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(handle)
}

fn lock_range(
    handle: HANDLE,
    offset: u64,
    exclusive: bool,
    fail_immediately: bool,
) -> Result<bool> {
    #[cfg(test)]
    if take_injected_lock_failure(offset, exclusive, fail_immediately) {
        return Err(LimboError::LockingError(
            "injected Windows file lock acquisition failure".into(),
        ));
    }

    let flags = (if exclusive {
        LOCKFILE_EXCLUSIVE_LOCK
    } else {
        0
    }) | if fail_immediately {
        LOCKFILE_FAIL_IMMEDIATELY
    } else {
        0
    };
    let mut overlapped = overlapped_at(offset);
    let result = unsafe { LockFileEx(handle, flags, 0, 1, 0, &mut overlapped) };
    if result == TRUE {
        return Ok(true);
    }
    if unsafe { GetLastError() } == ERROR_LOCK_VIOLATION {
        return Ok(false);
    }
    Err(LimboError::LockingError(format!(
        "Failed acquiring Windows file lock: {}",
        std::io::Error::last_os_error()
    )))
}

fn unlock_range(handle: HANDLE, offset: u64) -> Result<()> {
    #[cfg(test)]
    if take_injected_unlock_failure(offset) {
        return Err(LimboError::LockingError(
            "injected Windows file lock release failure".into(),
        ));
    }

    let mut overlapped = overlapped_at(offset);
    let result = unsafe { UnlockFileEx(handle, 0, 1, 0, &mut overlapped) };
    if result == TRUE || unsafe { GetLastError() } == ERROR_NOT_LOCKED {
        return Ok(());
    }
    Err(LimboError::LockingError(format!(
        "Failed releasing Windows file lock: {}",
        std::io::Error::last_os_error()
    )))
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum InjectedLockFailure {
    Lock {
        offset: u64,
        exclusive: bool,
        fail_immediately: bool,
    },
    Unlock {
        offset: u64,
    },
}

#[cfg(test)]
thread_local! {
    static LOCK_FAILURES: RefCell<VecDeque<InjectedLockFailure>> = const { RefCell::new(VecDeque::new()) };
}

#[cfg(test)]
fn inject_lock_failures(failures: impl IntoIterator<Item = InjectedLockFailure>) {
    LOCK_FAILURES.with(|injected| {
        let mut injected = injected.borrow_mut();
        injected.clear();
        injected.extend(failures);
    });
}

#[cfg(test)]
fn take_injected_lock_failure(offset: u64, exclusive: bool, fail_immediately: bool) -> bool {
    LOCK_FAILURES.with(|injected| {
        let mut injected = injected.borrow_mut();
        if matches!(
            injected.front(),
            Some(InjectedLockFailure::Lock {
                offset: expected_offset,
                exclusive: expected_exclusive,
                fail_immediately: expected_fail_immediately,
            }) if *expected_offset == offset
                && *expected_exclusive == exclusive
                && *expected_fail_immediately == fail_immediately
        ) {
            injected.pop_front();
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
fn take_injected_unlock_failure(offset: u64) -> bool {
    LOCK_FAILURES.with(|injected| {
        let mut injected = injected.borrow_mut();
        if matches!(
            injected.front(),
            Some(InjectedLockFailure::Unlock {
                offset: expected_offset,
            }) if *expected_offset == offset
        ) {
            injected.pop_front();
            true
        } else {
            false
        }
    })
}

struct ProcessFileLockEntry {
    handle: HANDLE,
    refcount: usize,
}

unsafe impl Send for ProcessFileLockEntry {}

type ProcessFileLockRegistry = Mutex<HashMap<PathBuf, ProcessFileLockEntry>>;

fn process_file_lock_registry() -> &'static ProcessFileLockRegistry {
    static REGISTRY: OnceLock<ProcessFileLockRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A reference to this process's single open-time lock for one database file.
pub(crate) struct ProcessFileLockGuard {
    key: PathBuf,
}

impl Drop for ProcessFileLockGuard {
    fn drop(&mut self) {
        let mut registry = process_file_lock_registry().lock();
        let Some(entry) = registry.get_mut(&self.key) else {
            tracing::error!(
                path = %self.key.display(),
                "Windows process lock registry entry missing during release"
            );
            return;
        };
        if entry.refcount == 0 {
            tracing::error!(
                path = %self.key.display(),
                "Windows process lock registry reference count underflow"
            );
            return;
        }
        entry.refcount -= 1;
        if entry.refcount != 0 {
            return;
        }

        let handle = registry
            .remove(&self.key)
            .expect("process lock entry disappeared while releasing")
            .handle;
        if let Err(err) = unlock_range(handle, PROCESS_LOCK_OFFSET) {
            tracing::error!(path = %self.key.display(), ?err, "failed releasing Windows process lock");
        }
        if unsafe { CloseHandle(handle) } == FALSE {
            tracing::error!(
                path = %self.key.display(),
                error = %std::io::Error::last_os_error(),
                "failed closing Windows process lock handle"
            );
        }
    }
}

/// Acquire the per-process open-time lock for `path`.
///
/// Windows byte-range locks are per-handle, so all same-process opens share a
/// single dedicated handle. The lock lives beyond the data range and therefore
/// does not interfere with normal database I/O.
pub(crate) fn acquire_process_file_lock(path: &str) -> Result<ProcessFileLockGuard> {
    let key = stable_lock_path(Path::new(path));
    let mut registry = process_file_lock_registry().lock();
    if let Some(entry) = registry.get_mut(&key) {
        entry.refcount += 1;
        return Ok(ProcessFileLockGuard { key });
    }

    let handle = open_lock_handle(&key)?;
    match lock_range(handle, PROCESS_LOCK_OFFSET, true, true) {
        Err(err) => {
            unsafe {
                CloseHandle(handle);
            }
            Err(err)
        }
        Ok(true) => {
            registry.insert(
                key.clone(),
                ProcessFileLockEntry {
                    handle,
                    refcount: 1,
                },
            );
            Ok(ProcessFileLockGuard { key })
        }
        Ok(false) => {
            unsafe {
                CloseHandle(handle);
            }
            Err(LimboError::LockingError(
                "Failed locking file. File is locked by another process".into(),
            ))
        }
    }
}

#[derive(Default)]
pub(crate) struct SharedWalLockState {
    held: HashMap<u64, HeldSharedWalLock>,
    entry: Option<SharedWalRegistryEntry>,
}

#[derive(Default)]
struct HeldSharedWalLock {
    shared: usize,
    exclusive: usize,
}

enum SharedWalGlobalLock {
    Shared { holders: usize },
    Exclusive,
}

struct SharedWalFileLockEntry {
    handle: HANDLE,
    locks: HashMap<u64, SharedWalGlobalLock>,
    poisoned: bool,
}

unsafe impl Send for SharedWalFileLockEntry {}

#[derive(Clone)]
struct SharedWalRegistryEntry {
    key: FileId,
    entry: Arc<Mutex<SharedWalFileLockEntry>>,
}

type SharedWalLockRegistry = Mutex<HashMap<FileId, Arc<Mutex<SharedWalFileLockEntry>>>>;

fn shared_wal_lock_registry() -> &'static SharedWalLockRegistry {
    static REGISTRY: OnceLock<SharedWalLockRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn shared_wal_file_id(handle: HANDLE) -> Result<FileId> {
    // SAFETY: this Win32 output structure consists entirely of integer fields, for which zero
    // is valid; the caller keeps handle open throughout the identity query.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: handle is borrowed from a live Windows file and info is a correctly sized output.
    if unsafe { GetFileInformationByHandle(handle, &raw mut info) } == FALSE {
        return Err(io_error(
            std::io::Error::last_os_error(),
            "identify shared WAL coordination file",
        ));
    }
    Ok(FileId {
        dev: u64::from(info.dwVolumeSerialNumber),
        ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

fn reopen_shared_wal_handle(original: HANDLE) -> Result<HANDLE> {
    // SAFETY: the caller retains original for this call. ReOpenFile binds the new handle to the
    // same file object even after rename/replacement. Flags zero requests synchronous I/O; file
    // attributes such as FILE_ATTRIBUTE_NORMAL are not valid ReOpenFile flags.
    let handle = unsafe {
        ReOpenFile(
            original,
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io_error(
            std::io::Error::last_os_error(),
            "reopen shared WAL coordination file for byte locks",
        ));
    }
    Ok(handle)
}

fn acquire_shared_wal_entry(
    original: HANDLE,
    state: &mut SharedWalLockState,
) -> Result<SharedWalRegistryEntry> {
    if let Some(entry) = &state.entry {
        return Ok(entry.clone());
    }

    let key = shared_wal_file_id(original)?;
    let entry = {
        let mut registry = shared_wal_lock_registry().lock();
        match registry.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.get().clone(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let handle = reopen_shared_wal_handle(original)?;
                let shared_entry = Arc::new(Mutex::new(SharedWalFileLockEntry {
                    handle,
                    locks: HashMap::new(),
                    poisoned: false,
                }));
                entry.insert(shared_entry.clone());
                shared_entry
            }
        }
    };
    let entry = SharedWalRegistryEntry { key, entry };
    state.entry = Some(entry.clone());
    Ok(entry)
}

fn release_shared_wal_entry(entry: SharedWalRegistryEntry) {
    let removed = {
        let mut registry = shared_wal_lock_registry().lock();
        let Some(current) = registry.get(&entry.key) else {
            tracing::error!(
                file_id = ?entry.key,
                "shared WAL lock registry entry missing during release"
            );
            return;
        };
        if !Arc::ptr_eq(current, &entry.entry) {
            tracing::error!(
                file_id = ?entry.key,
                "shared WAL lock registry entry changed during release"
            );
            return;
        }
        if Arc::strong_count(&entry.entry) != 2 {
            return;
        }
        registry.remove(&entry.key)
    };

    let Some(removed) = removed else {
        return;
    };
    let entry_state = removed.lock();
    if !entry_state.locks.is_empty() {
        tracing::error!(
            file_id = ?entry.key,
            "closing shared WAL lock handle with unreleased byte locks"
        );
    }
    if unsafe { CloseHandle(entry_state.handle) } == FALSE {
        tracing::error!(
            file_id = ?entry.key,
            error = %std::io::Error::last_os_error(),
            "failed closing shared WAL lock handle"
        );
    }
}

fn lock_shared_wal_byte(
    original: HANDLE,
    state: &Mutex<SharedWalLockState>,
    offset: u64,
    exclusive: bool,
    fail_immediately: bool,
) -> Result<bool> {
    let mut local = state.lock();
    let entry = acquire_shared_wal_entry(original, &mut local)?;
    let mut entry_state = entry.entry.lock();
    if entry_state.poisoned {
        return Err(LimboError::LockingError(
            "shared WAL lock state is unavailable after a failed lifetime lock restoration".into(),
        ));
    }

    match entry_state.locks.get_mut(&offset) {
        Some(SharedWalGlobalLock::Shared { holders }) if !exclusive => {
            *holders += 1;
            local.held.entry(offset).or_default().shared += 1;
            Ok(true)
        }
        Some(_) => Ok(false),
        None => match lock_range(entry_state.handle, offset, exclusive, fail_immediately) {
            Err(err) => Err(err),
            Ok(true) => {
                entry_state.locks.insert(
                    offset,
                    if exclusive {
                        SharedWalGlobalLock::Exclusive
                    } else {
                        SharedWalGlobalLock::Shared { holders: 1 }
                    },
                );
                let local_lock = local.held.entry(offset).or_default();
                if exclusive {
                    local_lock.exclusive += 1;
                } else {
                    local_lock.shared += 1;
                }
                Ok(true)
            }
            Ok(false) => Ok(false),
        },
    }
}

fn release_shared_wal_lock(
    entry: &SharedWalRegistryEntry,
    offset: u64,
    shared: usize,
    exclusive: usize,
) -> Result<()> {
    debug_assert!(shared == 0 || exclusive == 0);
    let mut entry_state = entry.entry.lock();
    if entry_state.poisoned && !entry_state.locks.contains_key(&offset) {
        return Ok(());
    }
    let remove_lock = match entry_state.locks.get_mut(&offset) {
        Some(SharedWalGlobalLock::Shared { holders }) if shared != 0 => {
            if *holders < shared {
                return Err(LimboError::LockingError(
                    "shared WAL shared lock reference count underflow".into(),
                ));
            }
            *holders -= shared;
            *holders == 0
        }
        Some(SharedWalGlobalLock::Exclusive) if exclusive != 0 => true,
        _ => {
            return Err(LimboError::LockingError(
                "shared WAL lock released with a mismatched mode".into(),
            ));
        }
    };

    if remove_lock {
        if let Err(err) = unlock_range(entry_state.handle, offset) {
            if let Some(SharedWalGlobalLock::Shared { holders }) =
                entry_state.locks.get_mut(&offset)
            {
                *holders += shared;
            }
            return Err(err);
        }
        entry_state.locks.remove(&offset);
    }
    Ok(())
}

pub(crate) fn shared_wal_lock_byte(
    original: HANDLE,
    state: &Mutex<SharedWalLockState>,
    offset: u64,
    exclusive: bool,
) -> Result<()> {
    if lock_shared_wal_byte(original, state, offset, exclusive, false)? {
        Ok(())
    } else {
        Err(LimboError::LockingError(
            "Failed locking shared WAL coordination file. File is locked by another process".into(),
        ))
    }
}

pub(crate) fn shared_wal_try_lock_byte(
    original: HANDLE,
    state: &Mutex<SharedWalLockState>,
    offset: u64,
    exclusive: bool,
) -> Result<bool> {
    lock_shared_wal_byte(original, state, offset, exclusive, true)
}

pub(crate) fn shared_wal_unlock_byte(state: &Mutex<SharedWalLockState>, offset: u64) -> Result<()> {
    let mut local = state.lock();
    let (shared, exclusive) = {
        let held = local.held.get(&offset).ok_or_else(|| {
            LimboError::LockingError("shared WAL lock released without an acquisition".into())
        })?;
        if held.exclusive != 0 {
            (0, 1)
        } else if held.shared != 0 {
            (1, 0)
        } else {
            unreachable!("empty shared WAL lock state was retained");
        }
    };

    let entry = local.entry.as_ref().ok_or_else(|| {
        LimboError::LockingError("shared WAL lock released without a registry entry".into())
    })?;
    release_shared_wal_lock(entry, offset, shared, exclusive)?;

    let held = local
        .held
        .get_mut(&offset)
        .expect("shared WAL local lock disappeared while releasing");
    if exclusive != 0 {
        held.exclusive -= 1;
    } else {
        held.shared -= 1;
    }
    if held.shared == 0 && held.exclusive == 0 {
        local.held.remove(&offset);
    }
    Ok(())
}

/// Probe whether no other process holds `offset` without changing this
/// process's aggregate shared-lock state.
pub(crate) fn shared_wal_probe_exclusive_byte(
    original: HANDLE,
    state: &Mutex<SharedWalLockState>,
    offset: u64,
) -> Result<bool> {
    let mut local = state.lock();
    let entry = acquire_shared_wal_entry(original, &mut local)?;
    let mut entry_state = entry.entry.lock();
    if entry_state.poisoned {
        return Err(LimboError::LockingError(
            "shared WAL lock state is unavailable after a failed lifetime lock restoration".into(),
        ));
    }

    let had_shared_lock = matches!(
        entry_state.locks.get(&offset),
        Some(SharedWalGlobalLock::Shared { .. })
    );
    if matches!(
        entry_state.locks.get(&offset),
        Some(SharedWalGlobalLock::Exclusive)
    ) {
        return Ok(false);
    }

    // Use a disposable handle for the exclusive probe. If releasing that
    // exclusive lock fails, closing the handle still guarantees it cannot
    // survive past the probe and block restoration of the shared lock.
    let probe_handle = reopen_shared_wal_handle(entry_state.handle)?;
    if had_shared_lock {
        if let Err(err) = unlock_range(entry_state.handle, offset) {
            if unsafe { CloseHandle(probe_handle) } == FALSE {
                tracing::error!(
                    file_id = ?entry.key,
                    error = %std::io::Error::last_os_error(),
                    "failed closing shared WAL exclusive probe handle"
                );
            }
            return Err(err);
        }
    }

    let probe = match lock_range(probe_handle, offset, true, true) {
        Ok(true) => {
            let result = unlock_range(probe_handle, offset).map(|()| true);
            if unsafe { CloseHandle(probe_handle) } == FALSE {
                tracing::error!(
                    file_id = ?entry.key,
                    error = %std::io::Error::last_os_error(),
                    "failed closing shared WAL exclusive probe handle"
                );
            }
            result
        }
        Ok(false) => {
            if unsafe { CloseHandle(probe_handle) } == FALSE {
                tracing::error!(
                    file_id = ?entry.key,
                    error = %std::io::Error::last_os_error(),
                    "failed closing shared WAL exclusive probe handle"
                );
            }
            Ok(false)
        }
        Err(err) => {
            if unsafe { CloseHandle(probe_handle) } == FALSE {
                tracing::error!(
                    file_id = ?entry.key,
                    error = %std::io::Error::last_os_error(),
                    "failed closing shared WAL exclusive probe handle"
                );
            }
            Err(err)
        }
    };

    if had_shared_lock {
        match lock_range(entry_state.handle, offset, false, false) {
            Ok(true) => {}
            Ok(false) => {
                entry_state.poisoned = true;
                return Err(LimboError::LockingError(
                    "shared WAL lifetime lock restoration was blocked after exclusive probe".into(),
                ));
            }
            Err(err) => {
                entry_state.poisoned = true;
                return Err(LimboError::LockingError(format!(
                    "shared WAL lifetime lock restoration failed after exclusive probe: {err}"
                )));
            }
        }
    }

    probe
}

pub(crate) fn release_shared_wal_locks_on_drop(path: &Path, state: &Mutex<SharedWalLockState>) {
    let (held, entry) = {
        let mut state = state.lock();
        (std::mem::take(&mut state.held), state.entry.take())
    };
    let Some(entry) = entry else {
        debug_assert!(held.is_empty());
        return;
    };
    for (offset, held) in held {
        if let Err(err) = release_shared_wal_lock(&entry, offset, held.shared, held.exclusive) {
            tracing::error!(
                path = %path.display(),
                offset,
                ?err,
                "failed releasing shared WAL lock while dropping Windows file"
            );
        }
    }
    release_shared_wal_entry(entry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;

    struct CurrentDirGuard(PathBuf);

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).expect("restore current directory");
        }
    }

    fn shared_wal_test_state() -> (tempfile::NamedTempFile, PathBuf, Mutex<SharedWalLockState>) {
        let file = tempfile::NamedTempFile::new().expect("create coordination file");
        let path = stable_lock_path(file.path());
        (file, path, Mutex::new(SharedWalLockState::default()))
    }

    #[test]
    fn shared_wal_lock_uses_cached_path_after_current_dir_change() {
        let current_dir = std::env::current_dir().expect("read current directory");
        let _restore = CurrentDirGuard(current_dir);
        let root = tempfile::tempdir().expect("create root directory");
        let first_dir = root.path().join("first");
        let second_dir = root.path().join("second");
        std::fs::create_dir_all(&first_dir).expect("create first directory");
        std::fs::create_dir_all(&second_dir).expect("create second directory");
        std::fs::write(first_dir.join("coordination.tshm"), []).expect("create coordination file");

        std::env::set_current_dir(&first_dir).expect("enter first directory");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("coordination.tshm")
            .expect("open coordination file");
        let path =
            stable_lock_path_for_handle(file.as_raw_handle(), Path::new("coordination.tshm"));
        let state = Mutex::new(SharedWalLockState::default());
        shared_wal_lock_byte(file.as_raw_handle(), &state, 0, false)
            .expect("acquire shared lifetime lock");

        std::env::set_current_dir(&second_dir).expect("enter second directory");
        shared_wal_unlock_byte(&state, 0).expect("release shared lifetime lock");
        release_shared_wal_locks_on_drop(&path, &state);
    }

    #[test]
    fn probe_unlock_failure_preserves_shared_lifetime_lock() {
        let (file, path, state) = shared_wal_test_state();
        shared_wal_lock_byte(file.as_raw_handle(), &state, 0, false)
            .expect("acquire shared lifetime lock");

        inject_lock_failures([InjectedLockFailure::Unlock { offset: 0 }]);
        assert!(shared_wal_probe_exclusive_byte(file.as_raw_handle(), &state, 0).is_err());

        shared_wal_unlock_byte(&state, 0).expect("release preserved shared lifetime lock");
        release_shared_wal_locks_on_drop(&path, &state);
    }

    #[test]
    fn probe_failure_restores_shared_lifetime_lock() {
        let (file, path, state) = shared_wal_test_state();
        shared_wal_lock_byte(file.as_raw_handle(), &state, 0, false)
            .expect("acquire shared lifetime lock");

        inject_lock_failures([InjectedLockFailure::Lock {
            offset: 0,
            exclusive: true,
            fail_immediately: true,
        }]);
        assert!(shared_wal_probe_exclusive_byte(file.as_raw_handle(), &state, 0).is_err());

        shared_wal_unlock_byte(&state, 0).expect("release restored shared lifetime lock");
        release_shared_wal_locks_on_drop(&path, &state);
    }

    #[test]
    fn shared_wal_locks_for_different_files_use_independent_entries() {
        // CP-T1.4: replacing a name cannot redirect locks away from the mapped file object.
        let root = tempfile::tempdir().expect("create coordination directory");
        let path = root.path().join("coordination.tshm");
        let first_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create original coordination file");
        let first_state = Mutex::new(SharedWalLockState::default());
        let moved_path = root.path().join("renamed.tshm");
        std::fs::rename(&path, &moved_path).expect("rename original coordination file");
        let second_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("replace coordination pathname");
        let second_state = Mutex::new(SharedWalLockState::default());
        shared_wal_lock_byte(first_file.as_raw_handle(), &first_state, 0, false)
            .expect("acquire first shared lifetime lock");
        shared_wal_lock_byte(second_file.as_raw_handle(), &second_state, 0, false)
            .expect("acquire second shared lifetime lock");

        let first_entry = first_state
            .lock()
            .entry
            .as_ref()
            .expect("first lock entry")
            .entry
            .clone();
        let second_entry = second_state
            .lock()
            .entry
            .as_ref()
            .expect("second lock entry")
            .entry
            .clone();
        assert!(!Arc::ptr_eq(&first_entry, &second_entry));
        drop(first_entry);
        drop(second_entry);

        let peer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&moved_path)
            .expect("open peer on original file object");
        assert!(lock_range(peer.as_raw_handle(), 1, true, true).expect("peer locks original"));
        assert!(
            !shared_wal_probe_exclusive_byte(first_file.as_raw_handle(), &first_state, 1)
                .expect("original probe observes its peer")
        );
        assert!(
            shared_wal_probe_exclusive_byte(second_file.as_raw_handle(), &second_state, 1)
                .expect("replacement probe is independent")
        );
        unlock_range(peer.as_raw_handle(), 1).expect("release peer lock");

        shared_wal_unlock_byte(&first_state, 0).expect("release first shared lifetime lock");
        release_shared_wal_locks_on_drop(&path, &first_state);
        shared_wal_unlock_byte(&second_state, 0).expect("release second shared lifetime lock");
        release_shared_wal_locks_on_drop(&path, &second_state);
    }

    #[test]
    fn failed_lifetime_lock_restoration_poisoned_entry_cleans_up_on_drop() {
        let (file, path, state) = shared_wal_test_state();
        shared_wal_lock_byte(file.as_raw_handle(), &state, 0, false)
            .expect("acquire shared lifetime lock");

        inject_lock_failures([InjectedLockFailure::Lock {
            offset: 0,
            exclusive: false,
            fail_immediately: false,
        }]);
        assert!(shared_wal_probe_exclusive_byte(file.as_raw_handle(), &state, 0).is_err());
        assert!(shared_wal_try_lock_byte(file.as_raw_handle(), &state, 1, true).is_err());

        shared_wal_unlock_byte(&state, 0).expect("release poisoned lifetime lock state");
        release_shared_wal_locks_on_drop(&path, &state);

        let reopened_state = Mutex::new(SharedWalLockState::default());
        shared_wal_lock_byte(file.as_raw_handle(), &reopened_state, 0, false)
            .expect("reopen clean shared lifetime lock state");
        shared_wal_unlock_byte(&reopened_state, 0)
            .expect("release reopened shared lifetime lock state");
        release_shared_wal_locks_on_drop(&path, &reopened_state);
    }
}
