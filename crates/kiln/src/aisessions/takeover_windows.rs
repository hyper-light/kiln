//! U18/U03: a Windows process handle binds creation-time validation and termination.
//! Native-width PEB reads verify environment ownership; inaccessible/cross-width processes
//! and all job members are refused. No sysinfo empty-environment inference grants authority.
#![allow(unsafe_code)]
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use super::ProcessIdentity;

type Handle = *mut c_void;
#[repr(C)]
#[derive(Default)]
struct FileTime { low: u32, high: u32 }
#[repr(C)]
#[derive(Default)]
struct Basic { status: usize, peb: usize, affinity: usize, priority: usize, pid: usize, parent: usize }

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn GetProcessTimes(process: Handle, created: *mut FileTime, exited: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
    fn TerminateProcess(process: Handle, code: u32) -> i32;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut i32) -> i32;
    fn IsWow64Process(process: Handle, result: *mut i32) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn ReadProcessMemory(process: Handle, address: *const c_void, output: *mut c_void, size: usize, read: *mut usize) -> i32;
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(process: Handle, class: u32, output: *mut c_void, size: u32, returned: *mut u32) -> i32;
}
fn error() -> String { std::io::Error::last_os_error().to_string() }

pub(super) fn open(pid: u32, terminate: bool) -> Result<OwnedHandle, String> {
    // PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ; optional PROCESS_TERMINATE.
    // SAFETY: scalar inputs; success transfers one newly owned kernel handle to this function.
    let handle = unsafe { OpenProcess(0x1000 | 0x0010 | if terminate { 1 } else { 0 }, 0, pid) };
    if handle.is_null() { return Err(error()); }
    // SAFETY: successful OpenProcess returned a valid exclusively owned handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub(super) fn identity(handle: &OwnedHandle) -> Result<ProcessIdentity, String> {
    let (mut created, mut exited, mut kernel, mut user) = (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
    // SAFETY: four correctly sized writable FILETIME outputs; handle remains owned by caller.
    if unsafe { GetProcessTimes(handle.as_raw_handle(), &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user) } == 0 { return Err(error()); }
    if exited.low != 0 || exited.high != 0 { return Err("observed process has exited".into()); }
    let created_100ns = (u64::from(created.high) << 32) | u64::from(created.low);
    if created_100ns == 0 { return Err("process creation identity unavailable".into()); }
    Ok(ProcessIdentity::Windows { created_100ns })
}

pub(super) fn observe(pid: u32) -> Result<ProcessIdentity, String> { identity(&open(pid, false)?) }

fn read(handle: &OwnedHandle, address: usize, output: &mut [u8]) -> Result<(), String> {
    if address == 0 { return Err("missing process environment address".into()); }
    let mut count = 0;
    // SAFETY: remote address is never dereferenced locally; Windows validates it. The output
    // slice is valid for precisely its declared length and the byte count has writable storage.
    if unsafe { ReadProcessMemory(handle.as_raw_handle(), address as *const c_void, output.as_mut_ptr().cast(), output.len(), &raw mut count) } == 0 || count != output.len() { return Err(error()); }
    Ok(())
}
fn pointer(handle: &OwnedHandle, address: usize) -> Result<usize, String> {
    let mut bytes = [0u8; std::mem::size_of::<usize>()];
    read(handle, address, &mut bytes)?;
    Ok(usize::from_ne_bytes(bytes))
}

fn inspect(handle: &OwnedHandle) -> Result<u32, String> {
    let (mut job, mut target_wow, mut own_wow) = (0, 0, 0);
    // SAFETY: valid owned handle, null means any job; output points to writable BOOL storage.
    if unsafe { IsProcessInJob(handle.as_raw_handle(), std::ptr::null_mut(), &raw mut job) } == 0 { return Err(error()); }
    if job != 0 { return Err("process belongs to a job; external ownership is not proven".into()); }
    // SAFETY: both valid process handles and correctly sized BOOL outputs.
    if unsafe { IsWow64Process(handle.as_raw_handle(), &raw mut target_wow) } == 0 || unsafe { IsWow64Process(GetCurrentProcess(), &raw mut own_wow) } == 0 { return Err(error()); }
    if target_wow != own_wow { return Err("cross-width process environment is unverifiable".into()); }
    let mut basic = Basic::default();
    let mut returned = 0;
    let size = u32::try_from(std::mem::size_of::<Basic>()).map_err(|_| "invalid process info size")?;
    // SAFETY: ProcessBasicInformation has six native pointer-width slots. Cross-width access
    // was rejected, and the buffer/length exactly match this native ABI.
    if unsafe { NtQueryInformationProcess(handle.as_raw_handle(), 0, (&raw mut basic).cast(), size, &raw mut returned) } < 0 || returned != size { return Err("process environment metadata unavailable".into()); }
    let (params_offset, environment_offset) = if cfg!(target_pointer_width = "64") { (0x20, 0x80) } else { (0x10, 0x48) };
    // PEB.ProcessParameters and RTL_USER_PROCESS_PARAMETERS.Environment native layouts.
    // These internal offsets are intentionally fail-closed on an unsupported/unreadable ABI.
    let params = pointer(handle, basic.peb.checked_add(params_offset).ok_or("invalid PEB address")?)?;
    let mut address = pointer(handle, params.checked_add(environment_offset).ok_or("invalid process parameters")?)?;
    let prefix: Vec<u16> = format!("{}=", crate::harness::survivors::MARK).encode_utf16().collect();
    let mut entry = Vec::new();
    let mut previous_nul = false;
    for _ in 0..1024 {
        let length = 256usize.min(4096usize.saturating_sub(address % 4096));
        let mut bytes = vec![0u8; length];
        read(handle, address, &mut bytes)?;
        for &pair in bytes.as_chunks::<2>().0 {
            let ch = u16::from_le_bytes(pair);
            if ch == 0 {
                if previous_nul { return u32::try_from(basic.parent).map_err(|_| "invalid parent process id".into()); }
                if entry.get(..prefix.len()).is_some_and(|key: &[u16]| key.iter().zip(&prefix).all(|(a, b)| (if (97..=122).contains(a) { *a - 32 } else { *a }) == *b)) {
                    return Err("process belongs to a kiln session".into());
                }
                entry.clear();
                previous_nul = true;
            } else {
                entry.push(ch);
                previous_nul = false;
            }
        }
        address = address.checked_add(length).ok_or("invalid environment extent")?;
    }
    Err("process environment exceeds verification bound".into())
}

pub(super) fn unmanaged(pid: u32, handle: &OwnedHandle) -> Result<(), String> {
    let mut descendant = identity(handle)?;
    let mut parent = inspect(handle)?;
    for _ in 0..64 {
        // Idle/System are the Windows ancestry boundary, never a structured harness.
        if parent <= 4 { return Ok(()); }
        if parent == pid { return Err("cyclic process ancestry".into()); }
        let ancestor = open(parent, false)?;
        let before = identity(&ancestor)?;
        if let (ProcessIdentity::Windows { created_100ns: ancestor_start }, ProcessIdentity::Windows { created_100ns: descendant_start }) = (&before, &descendant)
            && ancestor_start > descendant_start {
            return Err("ancestor PID was reused".into());
        }
        parent = inspect(&ancestor)?;
        if identity(&ancestor)? != before { return Err("process ancestry changed".into()); }
        descendant = before;
    }
    Err("process ancestry exceeds verification bound".into())
}

pub(super) fn terminate(handle: &OwnedHandle) -> Result<(), String> {
    // SAFETY: the same owned handle used for identity/ownership validation, never a reused PID.
    if unsafe { TerminateProcess(handle.as_raw_handle(), 1) } == 0 { return Err(error()); }
    Ok(())
}
