//! U01: atomic process creation inside a named Windows job. The kernel applies JOB_LIST
//! before any child instruction runs, so a runner crash cannot strand an unassigned child.
#![allow(unsafe_code)]

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::os::windows::process::ExitStatusExt as _;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
use std::sync::Arc;

type Handle = *mut c_void;

#[repr(C)]
#[derive(Default)]
struct Startup {
    size: u32, reserved: *mut u16, desktop: *mut u16, title: *mut u16,
    x: u32, y: u32, width: u32, height: u32, chars_x: u32, chars_y: u32,
    fill: u32, flags: u32, show: u16, reserved_len: u16, reserved_bytes: *mut u8,
    input: Handle, output: Handle, error: Handle,
}
#[repr(C)]
struct ExtendedStartup { startup: Startup, attributes: *mut c_void }
#[repr(C)]
#[derive(Default)]
struct ProcessInfo { process: Handle, thread: Handle, pid: u32, tid: u32 }
#[repr(C)]
struct Security { size: u32, descriptor: *mut c_void, inherit: i32 }

#[cfg(target_pointer_width = "64")]
const _: () = { assert!(std::mem::size_of::<Startup>() == 104); assert!(std::mem::size_of::<ExtendedStartup>() == 112); };

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreatePipe(read: *mut Handle, write: *mut Handle, security: *const Security, size: u32) -> i32;
    fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> i32;
    fn InitializeProcThreadAttributeList(list: *mut c_void, count: u32, flags: u32, bytes: *mut usize) -> i32;
    fn UpdateProcThreadAttribute(list: *mut c_void, flags: u32, key: usize, value: *const c_void, bytes: usize, old: *mut c_void, old_bytes: *mut usize) -> i32;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn CreateProcessW(program: *const u16, command: *mut u16, process_security: *const c_void, thread_security: *const c_void,
        inherit: i32, flags: u32, environment: *const c_void, cwd: *const u16, startup: *const ExtendedStartup, process: *mut ProcessInfo) -> i32;
    fn WaitForSingleObject(handle: Handle, timeout: u32) -> u32;
    fn GetExitCodeProcess(handle: Handle, code: *mut u32) -> i32;
    fn TerminateProcess(handle: Handle, code: u32) -> i32;
}

struct Attributes { memory: Vec<usize>, initialized: bool }
impl Attributes {
    fn new() -> io::Result<Self> {
        let mut bytes = 0usize;
        // SAFETY: the first call asks for the allocation size and cannot spawn anything.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &raw mut bytes) };
        if bytes == 0 || bytes > 1 << 20 { return Err(io::Error::other("invalid Windows attribute allocation size")); }
        let count = bytes.div_ceil(std::mem::size_of::<usize>());
        let mut out = Self { memory: vec![0; count], initialized: false };
        // SAFETY: pointer-aligned storage for the size Windows requested.
        if unsafe { InitializeProcThreadAttributeList(out.pointer(), 2, 0, &raw mut bytes) } == 0 { return Err(io::Error::last_os_error()); }
        out.initialized = true;
        Ok(out)
    }
    fn pointer(&mut self) -> *mut c_void { self.memory.as_mut_ptr().cast() }
    fn set(&mut self, key: usize, value: &[Handle]) -> io::Result<()> {
        // SAFETY: the caller keeps the backing handles alive until CreateProcessW returns.
        if unsafe { UpdateProcThreadAttribute(self.pointer(), 0, key, value.as_ptr().cast(), std::mem::size_of_val(value), std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: a successfully initialized list, held until process creation completed.
        if self.initialized { unsafe { DeleteProcThreadAttributeList(self.pointer()) }; }
    }
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut out: Vec<u16> = value.encode_wide().collect();
    if out.contains(&0) { return Err(io::Error::other("NUL in Windows process argument")); }
    out.push(0);
    Ok(out)
}

fn quote(argument: &OsStr, output: &mut Vec<u16>) -> io::Result<()> {
    output.push(u16::from(b'"'));
    let mut slashes = 0usize;
    for ch in argument.encode_wide() {
        if ch == 0 { return Err(io::Error::other("NUL in Windows process argument")); }
        if ch == u16::from(b'\\') { slashes += 1; continue; }
        if ch == u16::from(b'"') {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2 + 1));
        } else {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
        }
        output.push(ch);
        slashes = 0;
    }
    output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2));
    output.push(u16::from(b'"'));
    Ok(())
}

fn command_line(cmd: &Command) -> io::Result<Vec<u16>> {
    let program = std::path::Path::new(cmd.get_program());
    let shell = program.file_name().and_then(OsStr::to_str).is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe"));
    let mut command = Vec::new();
    quote(program.as_os_str(), &mut command)?;
    for arg in cmd.get_args() {
        command.push(u16::from(b' '));
        if shell {
            // U01/U03: CI builds cmd.exe /S /C CALL with raw_arg because cmd has its own
            // parser. get_args retains their text but hides the raw flag; preserve the shell
            // command exactly instead of applying the C runtime's backslash/quote encoding.
            let text = wide(arg)?;
            command.extend(text.iter().take(text.len().saturating_sub(1)).copied());
        } else {
            quote(arg, &mut command)?;
        }
    }
    command.push(0);
    if command.len() > 32767 { return Err(io::Error::other("Windows command exceeds its 32767 character limit")); }
    Ok(command)
}

fn pipe(child_reads: bool) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let security = Security { size: u32::try_from(std::mem::size_of::<Security>()).unwrap_or(0), descriptor: std::ptr::null_mut(), inherit: 1 };
    let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: two writable handle outputs and the correctly sized attributes.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, &raw const security, 0) } == 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: successful CreatePipe returned two independently owned valid handles.
    let read = unsafe { OwnedHandle::from_raw_handle(read) };
    let write = unsafe { OwnedHandle::from_raw_handle(write) };
    let (child, parent) = if child_reads { (read, write) } else { (write, read) };
    // SAFETY: parent pipe endpoints must never be inherited by the command.
    if unsafe { SetHandleInformation(parent.as_raw_handle(), 1, 0) } == 0 { return Err(io::Error::last_os_error()); }
    Ok((parent, child))
}

pub(crate) struct Child {
    process: Arc<OwnedHandle>,
    pub pid: u32,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

fn process_signaled(process: &OwnedHandle) -> io::Result<bool> {
    // SAFETY: callers retain this exact process handle throughout the nonblocking probe.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
        0 => Ok(true),
        258 => Ok(false),
        u32::MAX => Err(io::Error::last_os_error()),
        _ => Err(io::Error::other("unexpected Windows process wait result")),
    }
}

/// U01: job-zero accounting alone does not publish an owned child's final process result.
/// Keep every nonsignaled handle; an error leaves the original set intact for recovery.
pub(super) fn children_empty(children: &mut Vec<Arc<OwnedHandle>>) -> io::Result<bool> {
    let mut pending = Vec::new();
    for child in children.iter() {
        if !process_signaled(child)? { pending.push(Arc::clone(child)); }
    }
    *children = pending;
    Ok(children.is_empty())
}

impl Child {
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if !process_signaled(&self.process)? { return Ok(None); }
        let mut code = 0;
        // SAFETY: process has signaled termination and code is writable.
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) } == 0 { return Err(io::Error::last_os_error()); }
        Ok(Some(ExitStatus::from_raw(code)))
    }
    pub fn kill(&mut self) -> io::Result<()> {
        // SAFETY: only this owned process is terminated; Group kills all its descendants.
        if unsafe { TerminateProcess(self.process.as_raw_handle(), 1) } == 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }
}

impl super::Group {
    pub(crate) fn spawn_leased(&self, cmd: Command) -> io::Result<Child> {
        let program = std::path::Path::new(cmd.get_program());
        if !program.is_absolute() { return Err(io::Error::other("leased Windows command requires an absolute executable")); }
        let application = wide(program.as_os_str())?;
        let mut command = command_line(&cmd)?;
        let cwd = cmd.get_current_dir().map(|p| wide(p.as_os_str())).transpose()?;
        // Reach constructs a complete env_clear command. Refuse missing explicit environment
        // rather than accidentally inheriting runner secrets through a second environment path.
        let mut environment = Vec::<u16>::new();
        for (key, value) in cmd.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))) {
            let key = wide(key)?; let value = wide(value)?;
            environment.extend(key.iter().take(key.len().saturating_sub(1)).copied());
            environment.push(u16::from(b'='));
            environment.extend(value);
        }
        if environment.is_empty() { return Err(io::Error::other("leased Windows command lacks an explicit environment")); }
        environment.push(0);
        let (input, child_input) = pipe(true)?;
        let (output, child_output) = pipe(false)?;
        let (error, child_error) = pipe(false)?;
        let inherited = [child_input.as_raw_handle(), child_output.as_raw_handle(), child_error.as_raw_handle()];
        let jobs = [self.job.handle.0];
        let mut attributes = Attributes::new()?;
        attributes.set(0x0002_0002, &inherited)?; // PROC_THREAD_ATTRIBUTE_HANDLE_LIST
        attributes.set(0x0002_000d, &jobs)?; // PROC_THREAD_ATTRIBUTE_JOB_LIST
        let startup = ExtendedStartup { startup: Startup {
            size: u32::try_from(std::mem::size_of::<ExtendedStartup>()).unwrap_or(0), flags: 0x100,
            input: inherited.first().copied().unwrap_or(std::ptr::null_mut()),
            output: inherited.get(1).copied().unwrap_or(std::ptr::null_mut()),
            error: inherited.get(2).copied().unwrap_or(std::ptr::null_mut()), ..Startup::default()
        }, attributes: attributes.pointer() };
        let mut process = ProcessInfo::default();
        // This is also the proof lock: do not allow job-zero to be observed between kernel
        // creation and retaining the exact child's handle, including an immediate exit.
        let mut children = self.job.children.lock();
        // SAFETY: all strings are NUL-terminated, environment is double-NUL-terminated, pipes
        // and attributes remain alive. JOB_LIST makes membership atomic with creation.
        let created = unsafe { CreateProcessW(application.as_ptr(), command.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1,
            0x0008_0600, environment.as_ptr().cast(), cwd.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()), &raw const startup, &raw mut process) };
        if created == 0 { return Err(io::Error::last_os_error()); }
        // SAFETY: successful CreateProcessW returned owned process and thread handles.
        let handle = Arc::new(unsafe { OwnedHandle::from_raw_handle(process.process) });
        let _thread = unsafe { OwnedHandle::from_raw_handle(process.thread) };
        children.push(Arc::clone(&handle));
        Ok(Child { process: handle, pid: process.pid, stdin: Some(input.into()), stdout: Some(output.into()), stderr: Some(error.into()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an allocated native Windows runner; run centrally"]
    fn u01_windows_atomic_job_assignment_recovers_until_kernel_empty() -> anyhow::Result<()> {
        let id = format!("ci-u01-{}", crate::util::new_id());
        let group = crate::os::contain::Group::of_leased(&id)?;
        let system = std::env::var_os("SystemRoot").ok_or_else(|| io::Error::other("Windows has no SystemRoot"))?;
        let executable = std::path::PathBuf::from(&system).join("System32").join("PING.EXE");
        let mut cmd = Command::new(executable);
        cmd.env_clear().env("SystemRoot", system).args(["-n", "30", "127.0.0.1"]);
        let mut child = group.spawn_leased(cmd)?;
        let outcome = (|| -> anyhow::Result<()> {
            anyhow::ensure!(!group.empty_proven()?, "atomic creation must have assigned the live child");
            let recovered = crate::os::contain::Group::recover_leased(&group.lease_identity(), &id)?;
            anyhow::ensure!(Arc::ptr_eq(&group.job, &recovered.job), "recovery must retain the original child's process-exit barrier");
            anyhow::ensure!(!recovered.empty_proven()?, "recovery must open the original job");
            anyhow::ensure!(crate::os::contain::Group::recover_leased(&group.lease_identity(), "ci-other").is_err(), "another execution cannot release this job");
            recovered.kill();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !recovered.empty_proven()? {
                anyhow::ensure!(std::time::Instant::now() < until, "job termination was not proved");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            anyhow::ensure!(child.try_wait()?.is_some(), "the atomic child must be reaped after job zero proof");
            anyhow::ensure!(child.try_wait()?.and_then(|status| status.code()) == Some(1), "the original process handle must retain the job's exit code");
            Ok(())
        })();
        group.kill();
        group.remove();
        outcome
    }

    #[test]
    fn u01_windows_quoting_preserves_backslashes_before_quotes() -> anyhow::Result<()> {
        let mut output = Vec::new();
        quote(OsStr::new("a\\\"b\\"), &mut output)?;
        anyhow::ensure!(String::from_utf16(&output)? == "\"a\\\\\\\"b\\\\\"", "CreateProcess argument quoting changed");
        Ok(())
    }

    #[test]
    fn u01_windows_cmd_call_preserves_runtime_raw_shell_quotes() -> anyhow::Result<()> {
        use std::os::windows::process::CommandExt as _;
        let mut cmd = Command::new(r"C:\Windows\System32\cmd.exe");
        cmd.raw_arg("/D").raw_arg("/S").raw_arg("/C").raw_arg("\"CALL \"C:\\a b\\step.cmd\"\"");
        let line = command_line(&cmd)?;
        let text = String::from_utf16(line.get(..line.len().saturating_sub(1)).unwrap_or_default())?;
        anyhow::ensure!(text.ends_with(" /D /S /C \"CALL \"C:\\a b\\step.cmd\"\""), "cmd's CALL command must retain its own quoting: {text}");
        Ok(())
    }

}
