//! The Win32 calls `kiln install` needs, declared by hand: whether this process runs elevated
//! (`GetTokenInformation(TokenElevation)`), the user's `Path` (`HKCU\Environment`) and the
//! broadcast that tells the desktop it changed (`WM_SETTINGCHANGE`), and running a program as an
//! administrator (`ShellExecuteExW` with the `runas` verb: Windows asks the person at the desktop).
//! And one every kiln makes: how big a volume is and how much of it this user may still write
//! (`GetDiskFreeSpaceExW`), what `statvfs` tells elsewhere. And a runner's: keeping its memory
//! from its user's other processes (its own DACL, `SetKernelObjectSecurity`).
//!
//! user32 and shell32 are looked up at run time (`LoadLibraryExW` + `GetProcAddress`), so a kiln
//! that never installs anything (an agent, a supervisor) doesn't load them.
//!
//! Self-contained (std only), so it type-checks for Windows from any host:
//! `rustc --edition 2024 --crate-type lib --target x86_64-pc-windows-gnu`. The structure layout on
//! 64-bit Windows is asserted at compile time against the value of the Windows SDK headers.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

const TOKEN_QUERY: u32 = 0x0008;
/// `TOKEN_INFORMATION_CLASS::TokenElevation`.
const TOKEN_ELEVATION_CLASS: u32 = 20;
/// `HKEY_CURRENT_USER`: `(HKEY)(ULONG_PTR)(LONG)0x80000001`, sign-extended.
const HKEY_CURRENT_USER: isize = 0x8000_0001_u32 as i32 as isize;
const KEY_QUERY_VALUE: u32 = 0x0001;
const KEY_SET_VALUE: u32 = 0x0002;
pub const REG_EXPAND_SZ: u32 = 2;
const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const HWND_BROADCAST: isize = 0xffff;
const WM_SETTINGCHANGE: u32 = 0x001A;
const SMTO_ABORTIFHUNG: u32 = 0x0002;
const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
const SEE_MASK_NOASYNC: u32 = 0x0000_0100;
const SEE_MASK_FLAG_NO_UI: u32 = 0x0000_0400;
const SW_HIDE: i32 = 0;
const WAIT_OBJECT_0: u32 = 0;
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x800;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
const SDDL_REVISION_1: u32 = 1;
/// A process's DACL while [`private_memory`] keeps it: SYSTEM may do anything (`PROCESS_ALL_ACCESS`);
/// its owner, this user, only wait on it, ask after it and end it (`SYNCHRONIZE`,
/// `PROCESS_QUERY_LIMITED_INFORMATION`, `PROCESS_TERMINATE`), and the OWNER RIGHTS entry takes
/// away an owner's implicit right to change that.
const PRIVATE_DACL: &str = "D:P(A;;0x1fffff;;;SY)(A;;0x101001;;;OW)";
/// What `ShellExecuteExW` fails with when the person says no to the elevation prompt.
pub const ERROR_CANCELLED: i32 = 1223;

/// `TOKEN_ELEVATION`.
#[repr(C)]
struct TokenElevation {
    token_is_elevated: u32,
}

/// `SHELLEXECUTEINFOW`.
#[repr(C)]
struct ShellExecuteInfo {
    size: u32,
    mask: u32,
    hwnd: *mut c_void,
    verb: *const u16,
    file: *const u16,
    parameters: *const u16,
    directory: *const u16,
    show: i32,
    inst_app: *mut c_void,
    id_list: *mut c_void,
    class: *const u16,
    key_class: *mut c_void,
    hot_key: u32,
    icon_or_monitor: *mut c_void,
    process: *mut c_void,
}

// The layout of the Windows SDK (and mingw-w64) headers on 64-bit Windows.
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<ShellExecuteInfo>() == 112);
    assert!(offset_of!(ShellExecuteInfo, show) == 48);
    assert!(offset_of!(ShellExecuteInfo, hot_key) == 88);
    assert!(offset_of!(ShellExecuteInfo, process) == 104);
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    #[cfg(test)]
    fn GetCurrentProcessId() -> u32;
    #[cfg(test)]
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
    fn GetConsoleWindow() -> *mut c_void;
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    fn GetDiskFreeSpaceExW(directory: *const u16, available: *mut u64, total: *mut u64, free: *mut u64) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(token: *mut c_void, class: u32, info: *mut c_void, len: u32, returned: *mut u32) -> i32;
    fn RegOpenKeyExW(key: isize, subkey: *const u16, options: u32, access: u32, result: *mut isize) -> i32;
    fn RegQueryValueExW(key: isize, name: *const u16, reserved: *mut u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32;
    fn RegSetValueExW(key: isize, name: *const u16, reserved: u32, kind: u32, data: *const u8, len: u32) -> i32;
    fn RegCloseKey(key: isize) -> i32;
    fn GetKernelObjectSecurity(handle: *mut c_void, info: u32, sd: *mut c_void, len: u32, needed: *mut u32) -> i32;
    fn SetKernelObjectSecurity(handle: *mut c_void, info: u32, sd: *const c_void) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl: *const u16, revision: u32, sd: *mut *mut c_void, len: *mut u32) -> i32;
}

type SendMessageTimeoutFn = unsafe extern "system" fn(isize, u32, usize, isize, u32, u32, *mut usize) -> isize;
type ShellExecuteExFn = unsafe extern "system" fn(*mut ShellExecuteInfo) -> i32;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `name` in system DLL `dll`, loaded on first use; null when either is missing.
fn lookup(dll: &str, name: &CStr) -> *mut c_void {
    let dll = wide(dll);
    // SAFETY: a NUL-terminated name; only the system directory is searched.
    let module = unsafe { LoadLibraryExW(dll.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a loaded module (never freed) and a NUL-terminated name.
    unsafe { GetProcAddress(module, name.as_ptr()) }
}

fn send_message_timeout() -> Option<SendMessageTimeoutFn> {
    static F: OnceLock<usize> = OnceLock::new();
    let p = *F.get_or_init(|| lookup("user32.dll", c"SendMessageTimeoutW") as usize);
    // SAFETY: user32's SendMessageTimeoutW, whose signature the type spells out.
    (p != 0).then(|| unsafe { std::mem::transmute::<usize, SendMessageTimeoutFn>(p) })
}

fn shell_execute_ex() -> Option<ShellExecuteExFn> {
    static F: OnceLock<usize> = OnceLock::new();
    let p = *F.get_or_init(|| lookup("shell32.dll", c"ShellExecuteExW") as usize);
    // SAFETY: shell32's ShellExecuteExW, whose signature the type spells out.
    (p != 0).then(|| unsafe { std::mem::transmute::<usize, ShellExecuteExFn>(p) })
}

/// Whether this process runs with an administrator's full rights: elevated, or UAC is off.
pub fn elevated() -> bool {
    let mut token = std::ptr::null_mut();
    // SAFETY: this process's pseudo-handle and a place for the token handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut e = TokenElevation { token_is_elevated: 0 };
    let mut returned = 0u32;
    let size = u32::try_from(std::mem::size_of::<TokenElevation>()).unwrap_or(4);
    // SAFETY: an open token and a buffer the size of TOKEN_ELEVATION.
    let ok = unsafe { GetTokenInformation(token, TOKEN_ELEVATION_CLASS, (&raw mut e).cast(), size, &mut returned) } != 0;
    // SAFETY: the token is ours to close.
    unsafe { CloseHandle(token) };
    ok && e.token_is_elevated != 0
}

/// `HKCU\Environment`, opened for `access`; closed when dropped.
struct EnvironmentKey(isize);

impl EnvironmentKey {
    fn open(access: u32) -> io::Result<Self> {
        let subkey = wide("Environment");
        let mut key = 0isize;
        // SAFETY: a predefined root key, a NUL-terminated subkey and a place for the result.
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut key) };
        if rc == ERROR_SUCCESS { Ok(Self(key)) } else { Err(io::Error::from_raw_os_error(rc)) }
    }
}

impl Drop for EnvironmentKey {
    fn drop(&mut self) {
        // SAFETY: a key this value opened.
        unsafe { RegCloseKey(self.0) };
    }
}

/// The user's `Path` as stored (`%VARIABLES%` not expanded) and its registry type (`REG_SZ`, or
/// `REG_EXPAND_SZ`, which is written back as such); `None` when the user has none of their own.
pub fn user_path() -> io::Result<Option<(String, u32)>> {
    let key = EnvironmentKey::open(KEY_QUERY_VALUE)?;
    let name = wide("Path");
    // Grown and asked again while the value grows under us.
    let mut buf: Vec<u16> = vec![0; 2048];
    for _ in 0..8 {
        let mut kind = 0u32;
        let mut len = u32::try_from(buf.len() * 2).unwrap_or(u32::MAX);
        // SAFETY: an open key, a NUL-terminated name, and a buffer of `len` bytes.
        let rc = unsafe { RegQueryValueExW(key.0, name.as_ptr(), std::ptr::null_mut(), &mut kind, buf.as_mut_ptr().cast(), &mut len) };
        match rc {
            ERROR_SUCCESS => {
                let units = usize::try_from(len).unwrap_or(0) / 2;
                let value = buf.get(..units.min(buf.len())).unwrap_or_default();
                let end = value.iter().position(|&u| u == 0).unwrap_or(value.len());
                return Ok(Some((String::from_utf16_lossy(value.get(..end).unwrap_or_default()), kind)));
            }
            ERROR_FILE_NOT_FOUND => return Ok(None),
            // ERROR_MORE_DATA: `len` says how much it needs.
            _ if usize::try_from(len).is_ok_and(|n| n > buf.len() * 2) => buf.resize(usize::try_from(len).unwrap_or(0) / 2 + 1, 0),
            _ => return Err(io::Error::from_raw_os_error(rc)),
        }
    }
    Err(io::Error::other("the user's Path kept changing while it was read"))
}

/// Store the user's `Path` as registry type `kind` (what [`user_path`] read).
pub fn set_user_path(value: &str, kind: u32) -> io::Result<()> {
    let key = EnvironmentKey::open(KEY_SET_VALUE)?;
    let name = wide("Path");
    let data = wide(value);
    let len = u32::try_from(data.len() * 2).map_err(|_| io::Error::other("Path too long"))?;
    // SAFETY: an open key, a NUL-terminated name and `len` bytes of NUL-terminated UTF-16.
    let rc = unsafe { RegSetValueExW(key.0, name.as_ptr(), 0, kind, data.as_ptr().cast(), len) };
    if rc == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(rc)) }
}

/// Tell the desktop the user's environment changed (`WM_SETTINGCHANGE` "Environment"), so what
/// it starts from now on (Explorer, a new terminal) sees it. Best effort: a window that doesn't
/// answer within a few seconds is skipped.
pub fn announce_environment() {
    let Some(send) = send_message_timeout() else { return };
    let what = wide("Environment");
    let mut result = 0usize;
    // SAFETY: a broadcast with a NUL-terminated string that outlives the (synchronous) call.
    unsafe { send(HWND_BROADCAST, WM_SETTINGCHANGE, 0, what.as_ptr() as isize, SMTO_ABORTIFHUNG, 5000, &mut result) };
}

/// Run `program` with `parameters` (one command line) as an administrator, its window hidden,
/// and wait up to `timeout` for its exit code. Windows asks the person at the desktop first
/// (UAC); an error with raw code [`ERROR_CANCELLED`] means they said no.
pub fn run_elevated(program: &Path, parameters: &str, timeout: Duration) -> io::Result<u32> {
    let shell_execute = shell_execute_ex().ok_or_else(|| io::Error::other("shell32's ShellExecuteExW is not available"))?;
    let verb = wide("runas");
    let file = wide(&program.display().to_string());
    let params = wide(parameters);
    let mut info = ShellExecuteInfo {
        size: u32::try_from(std::mem::size_of::<ShellExecuteInfo>()).unwrap_or(0),
        mask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        // The prompt belongs to this console's window, so it comes up in front of it.
        // SAFETY: no arguments; null when there is no console.
        hwnd: unsafe { GetConsoleWindow() },
        verb: verb.as_ptr(),
        file: file.as_ptr(),
        parameters: params.as_ptr(),
        directory: std::ptr::null(),
        show: SW_HIDE,
        inst_app: std::ptr::null_mut(),
        id_list: std::ptr::null_mut(),
        class: std::ptr::null(),
        key_class: std::ptr::null_mut(),
        hot_key: 0,
        icon_or_monitor: std::ptr::null_mut(),
        process: std::ptr::null_mut(),
    };
    // SAFETY: a filled-in SHELLEXECUTEINFOW whose strings outlive the call.
    if unsafe { shell_execute(&mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.process.is_null() {
        return Err(io::Error::other("Windows started nothing to wait for"));
    }
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: the process handle ShellExecuteExW gave us, waited on, read and closed once.
    let (waited, code) = unsafe {
        let waited = WaitForSingleObject(info.process, ms);
        let mut code = 0u32;
        let got = GetExitCodeProcess(info.process, &mut code) != 0;
        CloseHandle(info.process);
        (waited == WAIT_OBJECT_0 && got, code)
    };
    if waited { Ok(code) } else { Err(io::Error::new(io::ErrorKind::TimedOut, format!("{} did not finish", program.display()))) }
}

/// The size of the volume holding `dir` and the bytes this user may still write on it (disk
/// quotas count), as `statvfs` says elsewhere. Any directory on the volume does, a `\\?\` one
/// (what `canonicalize` gives) too.
pub fn disk_space(dir: &Path) -> io::Result<(u64, u64)> {
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut available, mut total) = (0u64, 0u64);
    // SAFETY: a NUL-terminated path, and places for two of the counts (a ULARGE_INTEGER is a
    // u64); the third is optional.
    if unsafe { GetDiskFreeSpaceExW(path.as_ptr(), &mut available, &mut total, std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((total, available))
}

/// This process's DACL from before [`private_memory`] narrowed it (a self-relative security
/// descriptor), to put back.
static WIDE_OPEN: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);

/// While `on`, no other process of this user reaches this process's memory (docs/ci.md W27-R02: a
/// CI leg's key in it, out of its steps' reach): its DACL is [`PRIVATE_DACL`]. Off, the one it had.
pub fn private_memory(on: bool) -> io::Result<()> {
    let mut saved = WIDE_OPEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: no arguments; this process's pseudo-handle, valid as long as it runs.
    let me = unsafe { GetCurrentProcess() };
    if on {
        if saved.is_some() {
            return Ok(());
        }
        let mut needed = 0u32;
        // SAFETY: no buffer: this asks how big the descriptor is.
        unsafe { GetKernelObjectSecurity(me, DACL_SECURITY_INFORMATION, std::ptr::null_mut(), 0, &mut needed) };
        let mut before = vec![0u8; usize::try_from(needed).unwrap_or(0)];
        // SAFETY: a buffer of `needed` bytes for it.
        if unsafe { GetKernelObjectSecurity(me, DACL_SECURITY_INFORMATION, before.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let sddl = wide(PRIVATE_DACL);
        let mut sd = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string, and a place for the descriptor it allocates.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: this process and the descriptor just made, freed once after.
        let set = unsafe { SetKernelObjectSecurity(me, DACL_SECURITY_INFORMATION, sd) };
        let err = io::Error::last_os_error();
        // SAFETY: the descriptor ConvertStringSecurityDescriptorToSecurityDescriptorW allocated.
        unsafe { LocalFree(sd) };
        if set == 0 {
            return Err(err);
        }
        *saved = Some(before);
    } else if let Some(before) = saved.take() {
        // SAFETY: this process and the descriptor GetKernelObjectSecurity gave for it.
        if unsafe { SetKernelObjectSecurity(me, DACL_SECURITY_INFORMATION, before.as_ptr().cast()) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn LookupPrivilegeValueW(system: *const u16, name: *const u16, luid: *mut Luid) -> i32;
        fn AdjustTokenPrivileges(token: *mut c_void, disable_all: i32, new: *const OnePrivilege, len: u32, previous: *mut c_void, returned: *mut u32) -> i32;
    }

    /// `LUID`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Luid {
        low: u32,
        high: i32,
    }

    /// `TOKEN_PRIVILEGES` with one `LUID_AND_ATTRIBUTES`.
    #[repr(C)]
    struct OnePrivilege {
        count: u32,
        luid: Luid,
        attributes: u32,
    }

    /// This process's token without Debug programs enabled: an elevated administrator's opens any
    /// process whatever its DACL (as root reaches any process on Linux), so this checks what a
    /// step of an unprivileged runner (kiln installs runners never elevated) meets.
    fn without_debug_privilege() {
        const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
        let mut token = std::ptr::null_mut();
        // SAFETY: this process's pseudo-handle and a place for the token handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token) } == 0 {
            return;
        }
        let name = wide("SeDebugPrivilege");
        let mut luid = Luid { low: 0, high: 0 };
        // SAFETY: the local system, a NUL-terminated name and a place for its LUID.
        if unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) } != 0 {
            let disabled = OnePrivilege { count: 1, luid, attributes: 0 };
            let len = u32::try_from(std::mem::size_of::<OnePrivilege>()).unwrap_or(16);
            // SAFETY: an open token and one privilege to disable; the previous state isn't asked.
            unsafe { AdjustTokenPrivileges(token, 0, &disabled, len, std::ptr::null_mut(), std::ptr::null_mut()) };
        }
        // SAFETY: the token is ours to close.
        unsafe { CloseHandle(token) };
    }

    /// W27-R02: while its memory is kept private, no process of this user opens this one to read
    /// it (`OpenProcess` checks the DACL for this process's own id too); afterwards it can again.
    #[test]
    fn w27_r02_private_memory_refuses_reading_it() -> io::Result<()> {
        const PROCESS_VM_READ: u32 = 0x0010;
        without_debug_privilege();
        // SAFETY: no arguments.
        let pid = unsafe { GetCurrentProcessId() };
        let opens = || {
            // SAFETY: a process id and an access mask; a handle it gives is closed at once.
            let h = unsafe { OpenProcess(PROCESS_VM_READ, 0, pid) };
            // SAFETY: the handle just opened, closed once.
            !h.is_null() && unsafe { CloseHandle(h) } != 0
        };
        assert!(opens(), "readable before");
        private_memory(true)?;
        let refused = !opens();
        private_memory(false)?;
        assert!(refused, "its memory could be read while kept private");
        assert!(opens(), "readable again after");
        Ok(())
    }
}
