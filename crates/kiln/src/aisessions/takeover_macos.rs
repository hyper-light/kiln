//! U18/U03: reuse the OS's audit-token signalling contract already used by leased_macos.
//! The kernel checks pidversion atomically; no task_for_pid entitlement or bare-PID kill.
//! Permission denial and unavailable identity/environment data refuse takeover explicitly.
#![allow(unsafe_code)]
use std::ffi::c_void;
use nix::libc;
use super::ProcessIdentity;

#[repr(C)]
#[derive(Default)]
struct Unique { uuid: [u8; 16], unique: u64, parent: u64, version: u32, parent_version: u32, reserved: [u64; 2] }
#[link(name = "System")]
unsafe extern "C" {
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, size: i32) -> i32;
    fn proc_signal_with_audittoken(token: *const u32, signal: i32) -> i32;
}
fn error() -> String { std::io::Error::last_os_error().to_string() }

pub(super) fn observe(pid: u32) -> Result<ProcessIdentity, String> {
    let pid = i32::try_from(pid).map_err(|_| "invalid process id")?;
    let mut boot = [0u8; 128];
    let mut size = boot.len();
    // SAFETY: valid NUL-terminated sysctl key, bounded writable buffer and size output.
    if unsafe { libc::sysctlbyname(c"kern.bootsessionuuid".as_ptr(), boot.as_mut_ptr().cast(), &raw mut size, std::ptr::null_mut(), 0) } != 0 { return Err(error()); }
    let boot = std::str::from_utf8(boot.get(..size).ok_or("invalid boot identity length")?).map_err(|_| "invalid boot identity")?.trim_end_matches('\0').to_owned();
    if boot.len() < 16 || !boot.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') { return Err("boot identity unavailable".into()); }
    let mut unique = Unique::default();
    // SAFETY: exact PROC_PIDUNIQIDENTIFIERINFO buffer/layout, matching leased_macos.
    if unsafe { proc_pidinfo(pid, 17, 0, (&raw mut unique).cast(), 56) } != 56 { return Err(error()); }
    if unique.unique == 0 { return Err("unique process identity unavailable".into()); }
    Ok(ProcessIdentity::MacOs { boot, unique: unique.unique, version: unique.version })
}

fn unmarked(pid: u32) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|_| "invalid process id")?;
    // KERN_PROCARGS2 provides argc, executable path, argv and environment. A successful
    // bounded kernel read is required; an empty sysinfo environment is not evidence.
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut bytes = vec![0u8; 1024 * 1024];
    let mut size = bytes.len();
    // SAFETY: the MIB and output buffer cover their declared sizes; no process is modified.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, bytes.as_mut_ptr().cast(), &raw mut size, std::ptr::null_mut(), 0) } != 0 { return Err(error()); }
    let bytes = bytes.get(..size).ok_or("invalid process environment length")?;
    let count: [u8; 4] = bytes.get(..4).ok_or("missing argument count")?.try_into().map_err(|_| "invalid argument count")?;
    let argc = usize::try_from(i32::from_ne_bytes(count)).map_err(|_| "invalid argument count")?;
    if argc > 65536 { return Err("argument count exceeds verification bound".into()); }
    let mut rest = bytes.get(4..).ok_or("missing executable")?;
    let end = rest.iter().position(|b| *b == 0).ok_or("unterminated executable")?;
    rest = rest.get(end.saturating_add(1)..).ok_or("missing arguments")?;
    while rest.first() == Some(&0) { rest = rest.get(1..).ok_or("missing arguments")?; }
    for _ in 0..argc {
        let end = rest.iter().position(|b| *b == 0).ok_or("unterminated argument")?;
        rest = rest.get(end.saturating_add(1)..).ok_or("missing environment")?;
    }
    let prefix = format!("{}=", crate::harness::survivors::MARK);
    loop {
        let end = rest.iter().position(|b| *b == 0).ok_or("unterminated environment")?;
        let value = rest.get(..end).ok_or("invalid environment extent")?;
        if value.is_empty() { return Ok(()); }
        if value.starts_with(prefix.as_bytes()) { return Err("process belongs to a kiln session".into()); }
        rest = rest.get(end.saturating_add(1)..).ok_or("missing environment terminator")?;
    }
}

fn parent(pid: u32) -> Result<u32, String> {
    let pid = i32::try_from(pid).map_err(|_| "invalid process id")?;
    // SAFETY: this kernel ABI struct contains only integers and fixed byte arrays.
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = i32::try_from(std::mem::size_of_val(&info)).map_err(|_| "invalid process info size")?;
    // SAFETY: the output and size match the libc-defined PROC_PIDT_SHORTBSDINFO layout.
    if unsafe { proc_pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO, 0, (&raw mut info).cast(), size) } != size { return Err(error()); }
    Ok(info.pbsi_ppid)
}

pub(super) fn unmanaged(pid: u32) -> Result<(), String> {
    let mut at = pid;
    for _ in 0..64 {
        if at == 1 { return Ok(()); }
        if crate::os::contain::session_of(at).is_some() { return Err("process belongs to a managed session".into()); }
        let before = observe(at)?;
        unmarked(at)?;
        let parent = parent(at)?;
        if observe(at)? != before { return Err("process ancestry changed".into()); }
        if parent <= 1 { return Ok(()); }
        if parent == at { return Err("cyclic process ancestry".into()); }
        at = parent;
    }
    Err("process ancestry exceeds verification bound".into())
}

pub(super) fn signal(pid: u32, identity: &ProcessIdentity, force: bool) -> Result<(), String> {
    let ProcessIdentity::MacOs { version, .. } = identity else { return Err("wrong platform process identity".into()); };
    let mut token = [0u32; 8];
    let pid_slot = token.get_mut(5).ok_or("invalid audit token layout")?;
    *pid_slot = pid;
    let version_slot = token.get_mut(7).ok_or("invalid audit token layout")?;
    *version_slot = *version;
    // SAFETY: exact audit_token_t layout. The kernel checks pidversion during signalling,
    // binding TERM/KILL to the observed process even if its PID was just reassigned.
    if unsafe { proc_signal_with_audittoken(token.as_ptr(), if force { libc::SIGKILL } else { libc::SIGTERM }) } != 0 { return Err(format!("identity-bound audit-token signal unavailable or denied: {}", error())); }
    Ok(())
}
