//! conpty-custody: can a supervisor be a pseudoconsole's custodian from its creation, its runners
//! only clients of it, so that a runner's upgrade moves no terminal at all?
//!
//! One binary, three roles:
//! - `conpty-custody <scenario> <dir>`: the custodian. It creates the pseudoconsole
//!   (`CreatePseudoConsole`) and its two pipes, starts the shell in it (`CreateProcessW` with
//!   `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`), and starts clients, giving each the shell's I/O: the
//!   input pipe's write end and the output pipe's read end, duplicated into it (`DuplicateHandle`
//!   with the client's process handle) or inherited (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`). It keeps
//!   the HPCON, so a client's resize is a request it serves (`ResizePseudoConsole`).
//! - `conpty-custody client <label>`: a client (a runner), driven line by line over its stdin by
//!   the custodian and answering on its stdout. It reads the shell's output on a thread of its own
//!   and writes the shell's input.
//! - `conpty-custody exit-child <mode> <path>`: a custodian that goes away (returns without
//!   `ClosePseudoConsole`, is killed, or closes first) under the `exit` scenario's driver.
//!
//! Scenarios: probe, basic, backlog, continuity, timing, exit, all. Results are printed as
//! `RESULT` and `STAT` lines and kept in `<dir>/results.txt`; raw output beside them.
//!
//! Windows only. std only, Win32 declared by hand (layouts asserted against the SDK's sizes).

#![allow(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    eprintln!("conpty-custody runs on Windows only");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match win::main(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("conpty-custody: {e}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::fs::File;
    use std::io::{self, BufRead as _, BufReader, Write as _};
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, mpsc};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    #[allow(non_camel_case_types, non_snake_case, clippy::upper_case_acronyms, dead_code)]
    mod ffi {
        use std::ffi::c_void;

        pub type HANDLE = *mut c_void;
        pub type BOOL = i32;
        pub type HRESULT = i32;
        pub type HPCON = *mut c_void;

        pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
        pub const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
        pub const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
        pub const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
        pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        pub const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
        pub const DUPLICATE_CLOSE_SOURCE: u32 = 0x1;
        pub const DUPLICATE_SAME_ACCESS: u32 = 0x2;
        pub const WAIT_OBJECT_0: u32 = 0;
        pub const WAIT_TIMEOUT: u32 = 0x102;
        pub const STILL_ACTIVE: u32 = 259;
        pub const SYNCHRONIZE: u32 = 0x0010_0000;
        pub const PROCESS_TERMINATE: u32 = 0x0001;
        pub const PROCESS_DUP_HANDLE: u32 = 0x0040;
        pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        pub const TH32CS_SNAPPROCESS: u32 = 0x2;
        pub const HANDLE_FLAG_INHERIT: u32 = 0x1;
        pub const STD_ERROR_HANDLE: u32 = -12i32 as u32;
        pub const ERROR_ACCESS_DENIED: i32 = 5;
        pub const ERROR_BROKEN_PIPE: i32 = 109;
        pub const ERROR_OPERATION_ABORTED: i32 = 995;
        pub const DACL_SECURITY_INFORMATION: u32 = 0x4;
        pub const SDDL_REVISION_1: u32 = 1;
        /// What portable-pty 0.9.0 passes (`win/psuedocon.rs`): INHERIT_CURSOR | RESIZE_QUIRK |
        /// WIN32_INPUT_MODE.
        pub const PORTABLE_PTY_FLAGS: u32 = 0x1 | 0x2 | 0x4;

        #[repr(C)]
        #[derive(Clone, Copy)]
        pub struct COORD {
            pub X: i16,
            pub Y: i16,
        }

        #[repr(C)]
        pub struct SECURITY_ATTRIBUTES {
            pub nLength: u32,
            pub lpSecurityDescriptor: *mut c_void,
            pub bInheritHandle: BOOL,
        }

        #[repr(C)]
        pub struct STARTUPINFOW {
            pub cb: u32,
            pub lpReserved: *mut u16,
            pub lpDesktop: *mut u16,
            pub lpTitle: *mut u16,
            pub dwX: u32,
            pub dwY: u32,
            pub dwXSize: u32,
            pub dwYSize: u32,
            pub dwXCountChars: u32,
            pub dwYCountChars: u32,
            pub dwFillAttribute: u32,
            pub dwFlags: u32,
            pub wShowWindow: u16,
            pub cbReserved2: u16,
            pub lpReserved2: *mut u8,
            pub hStdInput: HANDLE,
            pub hStdOutput: HANDLE,
            pub hStdError: HANDLE,
        }

        #[repr(C)]
        pub struct STARTUPINFOEXW {
            pub StartupInfo: STARTUPINFOW,
            pub lpAttributeList: *mut c_void,
        }

        #[repr(C)]
        pub struct PROCESS_INFORMATION {
            pub hProcess: HANDLE,
            pub hThread: HANDLE,
            pub dwProcessId: u32,
            pub dwThreadId: u32,
        }

        #[repr(C)]
        pub struct PROCESSENTRY32W {
            pub dwSize: u32,
            pub cntUsage: u32,
            pub th32ProcessID: u32,
            pub th32DefaultHeapID: usize,
            pub th32ModuleID: u32,
            pub cntThreads: u32,
            pub th32ParentProcessID: u32,
            pub pcPriClassBase: i32,
            pub dwFlags: u32,
            pub szExeFile: [u16; 260],
        }

        // The SDK's sizes on 64-bit Windows.
        #[cfg(target_pointer_width = "64")]
        const _: () = {
            assert!(size_of::<STARTUPINFOW>() == 104);
            assert!(size_of::<STARTUPINFOEXW>() == 112);
            assert!(size_of::<PROCESS_INFORMATION>() == 24);
            assert!(size_of::<PROCESSENTRY32W>() == 568);
            assert!(size_of::<SECURITY_ATTRIBUTES>() == 24);
            assert!(size_of::<COORD>() == 4);
        };

        #[link(name = "kernel32")]
        unsafe extern "system" {
            pub fn CreatePipe(read: *mut HANDLE, write: *mut HANDLE, sa: *const SECURITY_ATTRIBUTES, size: u32) -> BOOL;
            pub fn CreatePseudoConsole(size: COORD, input: HANDLE, output: HANDLE, flags: u32, pc: *mut HPCON) -> HRESULT;
            pub fn ResizePseudoConsole(pc: HPCON, size: COORD) -> HRESULT;
            pub fn ClosePseudoConsole(pc: HPCON);
            pub fn InitializeProcThreadAttributeList(list: *mut c_void, count: u32, flags: u32, size: *mut usize) -> BOOL;
            pub fn UpdateProcThreadAttribute(
                list: *mut c_void,
                flags: u32,
                attribute: usize,
                value: *const c_void,
                size: usize,
                previous: *mut c_void,
                returned: *mut usize,
            ) -> BOOL;
            pub fn DeleteProcThreadAttributeList(list: *mut c_void);
            pub fn CreateProcessW(
                app: *const u16,
                cmdline: *mut u16,
                pa: *const c_void,
                ta: *const c_void,
                inherit: BOOL,
                flags: u32,
                env: *const c_void,
                cwd: *const u16,
                si: *const STARTUPINFOW,
                pi: *mut PROCESS_INFORMATION,
            ) -> BOOL;
            pub fn DuplicateHandle(
                source_process: HANDLE,
                source: HANDLE,
                target_process: HANDLE,
                target: *mut HANDLE,
                access: u32,
                inherit: BOOL,
                options: u32,
            ) -> BOOL;
            pub fn GetCurrentProcess() -> HANDLE;
            pub fn CloseHandle(h: HANDLE) -> BOOL;
            pub fn ReadFile(h: HANDLE, buf: *mut c_void, len: u32, read: *mut u32, overlapped: *mut c_void) -> BOOL;
            pub fn WriteFile(h: HANDLE, buf: *const c_void, len: u32, written: *mut u32, overlapped: *mut c_void) -> BOOL;
            pub fn PeekNamedPipe(h: HANDLE, buf: *mut c_void, len: u32, read: *mut u32, avail: *mut u32, left: *mut u32) -> BOOL;
            pub fn GetNamedPipeInfo(h: HANDLE, flags: *mut u32, out_size: *mut u32, in_size: *mut u32, max: *mut u32) -> BOOL;
            pub fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
            pub fn GetExitCodeProcess(h: HANDLE, code: *mut u32) -> BOOL;
            pub fn TerminateProcess(h: HANDLE, code: u32) -> BOOL;
            pub fn OpenProcess(access: u32, inherit: BOOL, pid: u32) -> HANDLE;
            pub fn QueryPerformanceCounter(count: *mut i64) -> BOOL;
            pub fn QueryPerformanceFrequency(freq: *mut i64) -> BOOL;
            pub fn CancelSynchronousIo(thread: HANDLE) -> BOOL;
            pub fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
            pub fn Process32FirstW(snapshot: HANDLE, entry: *mut PROCESSENTRY32W) -> BOOL;
            pub fn Process32NextW(snapshot: HANDLE, entry: *mut PROCESSENTRY32W) -> BOOL;
            pub fn IsProcessInJob(process: HANDLE, job: HANDLE, result: *mut BOOL) -> BOOL;
            pub fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> BOOL;
            pub fn GetStdHandle(which: u32) -> HANDLE;
            pub fn SetHandleInformation(h: HANDLE, mask: u32, flags: u32) -> BOOL;
            pub fn LocalFree(mem: *mut c_void) -> *mut c_void;
            pub fn GetModuleHandleW(name: *const u16) -> *mut c_void;
            pub fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }

        pub const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
        pub const TOKEN_QUERY: u32 = 0x0008;
        pub const ERROR_NOT_ALL_ASSIGNED: i32 = 1300;

        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        pub struct LUID {
            pub LowPart: u32,
            pub HighPart: i32,
        }

        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        pub struct LUID_AND_ATTRIBUTES {
            pub Luid: LUID,
            pub Attributes: u32,
        }

        #[repr(C)]
        pub struct TOKEN_PRIVILEGES {
            pub PrivilegeCount: u32,
            pub Privileges: [LUID_AND_ATTRIBUTES; 1],
        }

        const _: () = {
            assert!(size_of::<LUID_AND_ATTRIBUTES>() == 12);
            assert!(size_of::<TOKEN_PRIVILEGES>() == 16);
        };

        #[link(name = "advapi32")]
        unsafe extern "system" {
            pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl: *const u16, revision: u32, sd: *mut *mut c_void, size: *mut u32) -> BOOL;
            pub fn SetKernelObjectSecurity(h: HANDLE, info: u32, sd: *const c_void) -> BOOL;
            pub fn OpenProcessToken(process: HANDLE, access: u32, token: *mut HANDLE) -> BOOL;
            pub fn LookupPrivilegeValueW(system: *const u16, name: *const u16, luid: *mut LUID) -> BOOL;
            pub fn AdjustTokenPrivileges(token: HANDLE, disable_all: BOOL, new: *const TOKEN_PRIVILEGES, len: u32, previous: *mut TOKEN_PRIVILEGES, ret: *mut u32) -> BOOL;
        }
    }

    use ffi::*;

    // ------------------------------------------------------------------ handles, time, text

    /// An owned kernel handle, closed as it drops.
    pub struct H(HANDLE);

    // SAFETY: a kernel handle is a process-wide value; any thread may use or close it.
    unsafe impl Send for H {}
    // SAFETY: as above; nothing here mutates through a shared H.
    unsafe impl Sync for H {}

    impl H {
        fn raw(&self) -> HANDLE {
            self.0
        }

        fn valid(&self) -> bool {
            !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE
        }

        fn into_raw(self) -> HANDLE {
            let h = self.0;
            std::mem::forget(self);
            h
        }
    }

    impl Drop for H {
        fn drop(&mut self) {
            if self.valid() {
                // SAFETY: we own the handle.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// A handle a thread borrows (its owner outlives the thread).
    #[derive(Clone, Copy)]
    struct Raw(HANDLE);

    // SAFETY: see H.
    unsafe impl Send for Raw {}

    fn os_err(what: &str) -> io::Error {
        let e = io::Error::last_os_error();
        io::Error::new(e.kind(), format!("{what}: {e}"))
    }

    fn ok(r: BOOL, what: &str) -> io::Result<()> {
        if r != 0 { Ok(()) } else { Err(os_err(what)) }
    }

    fn other(s: impl Into<String>) -> io::Error {
        io::Error::other(s.into())
    }

    fn qpc() -> i64 {
        let mut c = 0i64;
        // SAFETY: plain out-parameter.
        unsafe { QueryPerformanceCounter(&mut c) };
        c
    }

    fn freq() -> i64 {
        static F: OnceLock<i64> = OnceLock::new();
        *F.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: plain out-parameter.
            unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(1)
        })
    }

    /// QPC ticks as milliseconds (QPC is one counter for every process of a machine).
    fn ms(ticks: i64) -> f64 {
        ticks as f64 * 1000.0 / freq() as f64
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn escape(b: &[u8]) -> String {
        let mut s = String::with_capacity(b.len());
        for &c in b {
            match c {
                b'\\' => s.push_str("\\\\"),
                0x20..=0x7e => s.push(char::from(c)),
                _ => s.push_str(&format!("\\x{c:02x}")),
            }
        }
        s
    }

    fn unescape(s: &str) -> Vec<u8> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            if c == b'\\' {
                match b.get(i + 1) {
                    Some(b'\\') => {
                        out.push(b'\\');
                        i += 2;
                    }
                    Some(b'x') => {
                        let hex = b.get(i + 2..i + 4).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
                        if let Some(v) = hex {
                            out.push(v);
                        }
                        i += 4;
                    }
                    _ => {
                        out.push(c);
                        i += 1;
                    }
                }
            } else {
                out.push(c);
                i += 1;
            }
        }
        out
    }

    /// Text without its VT sequences (CSI, OSC, and two-byte escapes).
    fn strip_vt(b: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            if c != 0x1b {
                out.push(c);
                i += 1;
                continue;
            }
            match b.get(i + 1) {
                Some(b'[') => {
                    i += 2;
                    while b.get(i).is_some_and(|x| !(0x40..=0x7e).contains(x)) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    i += 2;
                    while let Some(&x) = b.get(i) {
                        if x == 0x07 {
                            i += 1;
                            break;
                        }
                        if x == 0x1b && b.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                Some(_) => i += 2,
                None => i += 1,
            }
        }
        out
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || hay.len() < needle.len() {
            return None;
        }
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// The number printed after `label` (spaces and VT sequences between are skipped).
    fn number_after(text: &[u8], label: &str) -> Option<u64> {
        let plain = strip_vt(text);
        let at = find(&plain, label.as_bytes())? + label.len();
        let rest = plain.get(at..)?;
        let digits: String = rest.iter().skip_while(|c| **c == b' ').take_while(|c| c.is_ascii_digit()).map(|c| char::from(*c)).collect();
        digits.parse().ok()
    }

    // ------------------------------------------------------------------ results

    static RESULTS: Mutex<Option<File>> = Mutex::new(None);

    fn report(line: &str) {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
        if let Some(f) = RESULTS.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }

    fn stat(name: &str, unit: &str, v: &[f64]) {
        let mut s = v.to_vec();
        s.sort_by(f64::total_cmp);
        let n = s.len();
        let (Some(min), Some(max)) = (s.first().copied(), s.last().copied()) else {
            report(&format!("STAT {name} n=0"));
            return;
        };
        let p = |q: f64| {
            let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
            s.get(rank - 1).copied().unwrap_or(max)
        };
        report(&format!(
            "STAT {name} n={n} p50={:.3}{unit} p90={:.3}{unit} p99={:.3}{unit} max={:.3}{unit} min={:.3}{unit}",
            p(0.5),
            p(0.9),
            p(0.99),
            max,
            min
        ));
    }

    // ------------------------------------------------------------------ pipes and processes

    fn pipe(size: u32) -> io::Result<(H, H)> {
        let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: null_mut(), bInheritHandle: 0 };
        let (mut r, mut w) = (null_mut(), null_mut());
        // SAFETY: out-parameters and a valid SECURITY_ATTRIBUTES.
        ok(unsafe { CreatePipe(&mut r, &mut w, &sa, size) }, "CreatePipe")?;
        Ok((H(r), H(w)))
    }

    fn set_inheritable(h: HANDLE, on: bool) -> io::Result<()> {
        // SAFETY: h is a handle we hold.
        ok(unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, u32::from(on)) }, "SetHandleInformation")
    }

    fn write_all(h: HANDLE, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let len = u32::try_from(data.len()).unwrap_or(u32::MAX).min(1 << 20);
            let mut n = 0u32;
            // SAFETY: the buffer is valid for len bytes; synchronous I/O.
            ok(unsafe { WriteFile(h, data.as_ptr().cast(), len, &mut n, null_mut()) }, "WriteFile")?;
            data = data.get(n as usize..).unwrap_or_default();
        }
        Ok(())
    }

    /// How many bytes the pipe holds unread.
    fn peek(h: HANDLE) -> io::Result<u32> {
        let mut avail = 0u32;
        // SAFETY: no buffer; out-parameter only.
        ok(unsafe { PeekNamedPipe(h, null_mut(), 0, null_mut(), &mut avail, null_mut()) }, "PeekNamedPipe")?;
        Ok(avail)
    }

    /// The pipe's buffer sizes as the system reports them: (outbound, inbound).
    fn pipe_sizes(h: HANDLE) -> Option<(u32, u32)> {
        let (mut flags, mut out, mut inn, mut max) = (0u32, 0u32, 0u32, 0u32);
        // SAFETY: out-parameters.
        (unsafe { GetNamedPipeInfo(h, &mut flags, &mut out, &mut inn, &mut max) } != 0).then_some((out, inn))
    }

    fn wait(h: HANDLE, ms: u32) -> bool {
        // SAFETY: h is a waitable handle we hold.
        unsafe { WaitForSingleObject(h, ms) == WAIT_OBJECT_0 }
    }

    fn exit_code(h: HANDLE) -> Option<u32> {
        let mut c = 0u32;
        // SAFETY: out-parameter.
        (unsafe { GetExitCodeProcess(h, &mut c) } != 0 && c != STILL_ACTIVE).then_some(c)
    }

    fn terminate(h: HANDLE) {
        // SAFETY: a process handle we hold (a process this program started).
        unsafe { TerminateProcess(h, 1) };
    }

    fn open_process(pid: u32, access: u32) -> io::Result<H> {
        // SAFETY: plain call.
        let h = unsafe { OpenProcess(access, 0, pid) };
        if h.is_null() { Err(os_err(&format!("OpenProcess({pid})"))) } else { Ok(H(h)) }
    }

    struct Proc {
        pid: u32,
        ppid: u32,
        exe: String,
    }

    fn processes() -> io::Result<Vec<Proc>> {
        // SAFETY: plain call.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return Err(os_err("CreateToolhelp32Snapshot"));
        }
        let snap = H(snap);
        // SAFETY: a POD struct; zero is a valid value.
        let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut v = Vec::new();
        // SAFETY: e is sized as the API asks.
        let mut more = unsafe { Process32FirstW(snap.raw(), &mut e) } != 0;
        while more {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            v.push(Proc { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, exe: String::from_utf16_lossy(e.szExeFile.get(..len).unwrap_or_default()) });
            // SAFETY: as above.
            more = unsafe { Process32NextW(snap.raw(), &mut e) } != 0;
        }
        Ok(v)
    }

    fn in_job(h: HANDLE) -> Option<bool> {
        let mut b: BOOL = 0;
        // SAFETY: out-parameter; a null job asks "any job".
        (unsafe { IsProcessInJob(h, null_mut(), &mut b) } != 0).then_some(b != 0)
    }

    fn session_of(pid: u32) -> Option<u32> {
        let mut s = 0u32;
        // SAFETY: out-parameter.
        (unsafe { ProcessIdToSessionId(pid, &mut s) } != 0).then_some(s)
    }

    /// Duplicate `h` (ours) into `target` (a process handle with PROCESS_DUP_HANDLE): the value
    /// is valid in that process.
    fn dup_to(target: HANDLE, h: HANDLE, access: Option<u32>, inherit: bool) -> io::Result<HANDLE> {
        let mut out = null_mut();
        let (acc, opt) = match access {
            Some(a) => (a, 0),
            None => (0, DUPLICATE_SAME_ACCESS),
        };
        // SAFETY: handles we hold; out-parameter.
        ok(unsafe { DuplicateHandle(GetCurrentProcess(), h, target, &mut out, acc, BOOL::from(inherit), opt) }, "DuplicateHandle")?;
        Ok(out)
    }

    /// Close a handle we duplicated into `target`.
    fn close_in(target: HANDLE, remote: HANDLE) {
        // SAFETY: DUPLICATE_CLOSE_SOURCE with no target closes `remote` in `target`.
        unsafe { DuplicateHandle(target, remote, null_mut(), null_mut(), 0, 0, DUPLICATE_CLOSE_SOURCE) };
    }

    /// A process attribute list (its values live as long as it does).
    struct Attrs {
        mem: Vec<u64>,
        handles: Vec<HANDLE>,
    }

    impl Attrs {
        fn new(count: u32) -> io::Result<Self> {
            let mut size = 0usize;
            // SAFETY: the sizing call.
            unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size) };
            if size == 0 {
                return Err(os_err("InitializeProcThreadAttributeList (size)"));
            }
            let mut mem = vec![0u64; size.div_ceil(8)];
            // SAFETY: mem holds `size` bytes, 8-aligned.
            ok(unsafe { InitializeProcThreadAttributeList(mem.as_mut_ptr().cast(), count, 0, &mut size) }, "InitializeProcThreadAttributeList")?;
            Ok(Self { mem, handles: Vec::new() })
        }

        fn ptr(&mut self) -> *mut c_void {
            self.mem.as_mut_ptr().cast()
        }

        fn inherit(&mut self, list: &[HANDLE]) -> io::Result<()> {
            self.handles = list.to_vec();
            let p = self.handles.as_ptr();
            let bytes = self.handles.len() * size_of::<HANDLE>();
            let l = self.ptr();
            // SAFETY: the list and the handle array live as long as self.
            ok(unsafe { UpdateProcThreadAttribute(l, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, p.cast(), bytes, null_mut(), null_mut()) }, "UpdateProcThreadAttribute(HANDLE_LIST)")
        }

        fn pseudoconsole(&mut self, pc: HPCON) -> io::Result<()> {
            let l = self.ptr();
            // SAFETY: the HPCON value itself is the attribute's value (the docs' sample, portable-pty).
            ok(unsafe { UpdateProcThreadAttribute(l, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, pc.cast_const(), size_of::<HPCON>(), null_mut(), null_mut()) }, "UpdateProcThreadAttribute(PSEUDOCONSOLE)")
        }
    }

    impl Drop for Attrs {
        fn drop(&mut self) {
            let l = self.ptr();
            // SAFETY: initialized in new().
            unsafe { DeleteProcThreadAttributeList(l) };
        }
    }

    /// Start `cmdline`, inheriting exactly `inherit` (nothing when empty), with these standard
    /// handles (invalid ones when None, as portable-pty starts a pseudoconsole's program), and
    /// attached to the pseudoconsole `pc` if given.
    fn spawn(cmdline: &str, inherit: &[HANDLE], stdio: Option<[HANDLE; 3]>, flags: u32, pc: Option<HPCON>) -> io::Result<(H, u32)> {
        // SAFETY: POD; zero is valid.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        let [i, o, e] = stdio.unwrap_or([INVALID_HANDLE_VALUE; 3]);
        si.StartupInfo.hStdInput = i;
        si.StartupInfo.hStdOutput = o;
        si.StartupInfo.hStdError = e;
        let count = u32::from(!inherit.is_empty()) + u32::from(pc.is_some());
        let mut attrs = Attrs::new(count.max(1))?;
        if !inherit.is_empty() {
            attrs.inherit(inherit)?;
        }
        if let Some(pc) = pc {
            attrs.pseudoconsole(pc)?;
        }
        si.lpAttributeList = attrs.ptr();
        let mut cmd = wide(cmdline);
        // SAFETY: POD; zero is valid.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: every pointer is valid for the call; cmd is mutable as CreateProcessW wants.
        ok(
            unsafe {
                CreateProcessW(
                    null(),
                    cmd.as_mut_ptr(),
                    null(),
                    null(),
                    BOOL::from(!inherit.is_empty()),
                    EXTENDED_STARTUPINFO_PRESENT | flags,
                    null(),
                    null(),
                    &si.StartupInfo,
                    &mut pi,
                )
            },
            &format!("CreateProcessW({cmdline})"),
        )?;
        drop(H(pi.hThread));
        Ok((H(pi.hProcess), pi.dwProcessId))
    }

    /// An inheritable copy of our stderr for a child, if we have one.
    fn stderr_for_child() -> Option<H> {
        // SAFETY: plain call.
        let e = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        if e.is_null() || e == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: GetCurrentProcess is a pseudo handle for ourselves.
        dup_to(unsafe { GetCurrentProcess() }, e, None, true).ok().map(H)
    }

    // ------------------------------------------------------------------ the reader thread

    #[derive(Default)]
    struct Got {
        data: Vec<u8>,
        /// (end offset, QPC) of each read.
        marks: Vec<(usize, i64)>,
        ended: Option<(String, i64)>,
        answered: u32,
        reads: u64,
    }

    type OnEnd = Box<dyn Fn(&str, i64) + Send>;

    /// Reads a pseudoconsole's output pipe on a thread of its own until stopped
    /// (`CancelSynchronousIo`: the pipe is synchronous, as ConPTY requires) or the pipe ends.
    struct Reader {
        thread: Option<JoinHandle<()>>,
        got: Arc<(Mutex<Got>, Condvar)>,
        stop: Arc<AtomicBool>,
        scanned: usize,
    }

    /// Terminal queries a terminal answers: a cursor position report and primary device
    /// attributes, which conhost may ask its terminal for.
    fn answers(chunk: &[u8]) -> Vec<&'static [u8]> {
        let mut v = Vec::new();
        let mut i = 0;
        while let Some(at) = chunk.get(i..).and_then(|c| find(c, b"\x1b[")) {
            let s = chunk.get(i + at..).unwrap_or_default();
            if s.starts_with(b"\x1b[6n") {
                v.push(&b"\x1b[1;1R"[..]);
            } else if s.starts_with(b"\x1b[c") || s.starts_with(b"\x1b[0c") {
                v.push(&b"\x1b[?1;0c"[..]);
            }
            i += at + 2;
        }
        v
    }

    impl Reader {
        fn start(from: Raw, answer: Option<Raw>, sink: Option<File>, on_end: Option<OnEnd>) -> io::Result<Self> {
            let got = Arc::new((Mutex::new(Got::default()), Condvar::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (g, s) = (Arc::clone(&got), Arc::clone(&stop));
            let thread = std::thread::Builder::new().name("pc-reader".into()).spawn(move || {
                // Whole values, not their raw fields: Raw is what is Send.
                let (from, answer, mut sink) = (from, answer, sink);
                let mut buf = vec![0u8; 64 * 1024];
                let why = loop {
                    if s.load(Ordering::SeqCst) {
                        break "stopped".to_owned();
                    }
                    let mut n = 0u32;
                    // SAFETY: buf is valid for its length; synchronous read.
                    let r = unsafe { ReadFile(from.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n, null_mut()) };
                    let at = qpc();
                    if r == 0 {
                        let e = io::Error::last_os_error();
                        match e.raw_os_error() {
                            Some(ERROR_OPERATION_ABORTED) if s.load(Ordering::SeqCst) => break "cancelled".to_owned(),
                            Some(ERROR_OPERATION_ABORTED) => continue,
                            Some(ERROR_BROKEN_PIPE) => break "broken-pipe".to_owned(),
                            _ => break format!("error({e})"),
                        }
                    }
                    if n == 0 {
                        break "eof".to_owned();
                    }
                    let chunk = buf.get(..n as usize).unwrap_or_default();
                    if let Some(f) = sink.as_mut() {
                        let _ = f.write_all(chunk);
                    }
                    let replies = if answer.is_some() { answers(chunk) } else { Vec::new() };
                    {
                        let (m, cv) = &*g;
                        let mut st = m.lock().unwrap_or_else(PoisonError::into_inner);
                        st.data.extend_from_slice(chunk);
                        let end = st.data.len();
                        st.marks.push((end, at));
                        st.answered += replies.len() as u32;
                        st.reads += 1;
                        cv.notify_all();
                    }
                    if let Some(a) = answer {
                        for r in replies {
                            let _ = write_all(a.0, r);
                        }
                    }
                };
                let at = qpc();
                let (m, cv) = &*g;
                m.lock().unwrap_or_else(PoisonError::into_inner).ended = Some((why.clone(), at));
                cv.notify_all();
                if let Some(cb) = on_end
                    && why != "stopped"
                    && why != "cancelled"
                {
                    cb(&why, at);
                }
            })?;
            Ok(Self { thread: Some(thread), got, stop, scanned: 0 })
        }

        /// Stop reading: once this returns nothing more is taken from the pipe here. A read that
        /// completed before the cancel keeps its bytes (they are in `data`).
        fn stop(&mut self) -> String {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let h = t.as_raw_handle();
                loop {
                    if wait(h, 0) {
                        break;
                    }
                    // SAFETY: the thread's handle (THREAD_ALL_ACCESS) while it runs.
                    unsafe { CancelSynchronousIo(h) };
                    if wait(h, 1) {
                        break;
                    }
                }
                let _ = t.join();
            }
            self.ended().map(|e| e.0).unwrap_or_default()
        }

        fn ended(&self) -> Option<(String, i64)> {
            self.got.0.lock().unwrap_or_else(PoisonError::into_inner).ended.clone()
        }

        fn len(&self) -> usize {
            self.got.0.lock().unwrap_or_else(PoisonError::into_inner).data.len()
        }

        fn data(&self) -> Vec<u8> {
            self.got.0.lock().unwrap_or_else(PoisonError::into_inner).data.clone()
        }

        fn first_read(&self) -> Option<i64> {
            self.got.0.lock().unwrap_or_else(PoisonError::into_inner).marks.first().map(|m| m.1)
        }

        fn answered(&self) -> u32 {
            self.got.0.lock().unwrap_or_else(PoisonError::into_inner).answered
        }

        /// Wait until `marker` shows in the output after `from`: the offset just past it and the
        /// QPC of the read that brought its last byte.
        fn wait_for(&mut self, marker: &[u8], from: usize, timeout: Duration) -> Option<(usize, i64)> {
            let deadline = Instant::now() + timeout;
            let (m, cv) = &*self.got;
            let mut st = m.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                let start = from.max(self.scanned.saturating_sub(marker.len()));
                if let Some(pos) = st.data.get(start..).and_then(|d| find(d, marker)) {
                    let end = start + pos + marker.len();
                    let at = st.marks.iter().find(|(e, _)| *e >= end).map_or(0, |(_, q)| *q);
                    self.scanned = end;
                    return Some((end, at));
                }
                self.scanned = st.data.len();
                if st.ended.is_some() {
                    return None;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                st = cv.wait_timeout(st, left).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
            }
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            if self.thread.is_some() {
                self.stop();
            }
        }
    }

    // ------------------------------------------------------------------ the client

    fn say(line: &str) {
        let mut o = io::stdout().lock();
        let _ = writeln!(o, "{line}");
        let _ = o.flush();
    }

    /// kiln's runner's own DACL while it keeps a leg's key (`crates/kiln/src/os/win32.rs`
    /// `PRIVATE_DACL`): SYSTEM all; its owner only SYNCHRONIZE, QUERY_LIMITED_INFORMATION and
    /// TERMINATE.
    fn private_dacl() -> io::Result<()> {
        let sddl = wide("D:P(A;;0x1fffff;;;SY)(A;;0x101001;;;OW)");
        let mut sd = null_mut();
        // SAFETY: valid string; out-parameter, freed below.
        ok(unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) }, "ConvertStringSecurityDescriptorToSecurityDescriptorW")?;
        // SAFETY: our own process; sd is a valid descriptor.
        let r = unsafe { SetKernelObjectSecurity(GetCurrentProcess(), DACL_SECURITY_INFORMATION, sd) };
        let e = (r == 0).then(|| os_err("SetKernelObjectSecurity"));
        // SAFETY: allocated by the conversion above.
        unsafe { LocalFree(sd) };
        e.map_or(Ok(()), Err)
    }

    /// Turn SeDebugPrivilege on or off in this process's token (an elevated runner has it on,
    /// which lets OpenProcess past any process DACL). Whether it was held at all.
    fn debug_privilege(on: bool) -> io::Result<bool> {
        let mut tok = null_mut();
        // SAFETY: our own process; out-parameter.
        ok(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut tok) }, "OpenProcessToken")?;
        let tok = H(tok);
        let name = wide("SeDebugPrivilege");
        let mut luid = LUID::default();
        // SAFETY: valid string; out-parameter.
        ok(unsafe { LookupPrivilegeValueW(null(), name.as_ptr(), &mut luid) }, "LookupPrivilegeValueW")?;
        let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: if on { 0x2 } else { 0 } }] };
        // SAFETY: a valid TOKEN_PRIVILEGES; no previous state asked for.
        ok(unsafe { AdjustTokenPrivileges(tok.raw(), 0, &tp, 0, null_mut(), null_mut()) }, "AdjustTokenPrivileges")?;
        // It succeeds without the privilege, saying so in the last error.
        Ok(io::Error::last_os_error().raw_os_error() != Some(ERROR_NOT_ALL_ASSIGNED))
    }

    type ReleaseFn = unsafe extern "system" fn(HPCON) -> HRESULT;

    /// ReleasePseudoConsole, where the system has it (Windows 11 24H2, build 26100, and later).
    fn release_pseudoconsole() -> Option<ReleaseFn> {
        let m = wide("kernel32.dll");
        // SAFETY: kernel32 is loaded in every process; the name is NUL-terminated.
        let p = unsafe { GetProcAddress(GetModuleHandleW(m.as_ptr()), c"ReleasePseudoConsole".as_ptr().cast()) };
        // SAFETY: the documented signature, `HRESULT WINAPI ReleasePseudoConsole(HPCON)`.
        (!p.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, ReleaseFn>(p) })
    }

    fn client(label: &str) -> io::Result<i32> {
        say(&format!("HELLO {} {}", std::process::id(), qpc()));
        let mut input: Option<Raw> = None;
        let mut shell: Option<Raw> = None;
        let mut conhost: Option<Raw> = None;
        let mut reader: Option<Reader> = None;
        let mut sink: Option<PathBuf> = None;
        let mut answer = true;
        let mut cursor = 0usize;
        let mut last_send = 0i64;
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = line?;
            let (cmd, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            match cmd {
                "NOANSWER" => answer = false,
                "SINK" => sink = Some(PathBuf::from(rest)),
                "HANDLES" => {
                    let v: Vec<usize> = rest.split(' ').filter_map(|x| usize::from_str_radix(x, 16).ok()).collect();
                    let &[i, o, s, k] = v.as_slice() else {
                        say(&format!("ERR bad HANDLES {rest}"));
                        continue;
                    };
                    input = Some(Raw(i as HANDLE));
                    shell = (s != 0).then_some(Raw(s as HANDLE));
                    conhost = (k != 0).then_some(Raw(k as HANDLE));
                    let file = match sink.as_ref().map(File::create).transpose() {
                        Ok(f) => f,
                        Err(e) => {
                            say(&format!("ERR sink {e}"));
                            None
                        }
                    };
                    let on_end: OnEnd = Box::new(|why: &str, at: i64| say(&format!("READER-ENDED {why} {at}")));
                    let t = qpc();
                    match Reader::start(Raw(o as HANDLE), answer.then_some(Raw(i as HANDLE)), file, Some(on_end)) {
                        Ok(r) => {
                            reader = Some(r);
                            cursor = 0;
                            say(&format!("ATTACHED {t}"));
                        }
                        Err(e) => say(&format!("ERR reader {e}")),
                    }
                }
                "SEND" => {
                    let Some(i) = input else {
                        say("ERR no handles");
                        continue;
                    };
                    let mut b = rest.as_bytes().to_vec();
                    b.push(b'\r');
                    last_send = qpc();
                    if let Err(e) = write_all(i.0, &b) {
                        say(&format!("ERR send {e}"));
                    }
                }
                "EXPECT" | "GRAB" => {
                    let Some((marker, t)) = rest.rsplit_once(' ') else {
                        say(&format!("ERR bad {cmd} {rest}"));
                        continue;
                    };
                    let timeout = Duration::from_millis(t.parse().unwrap_or(10_000));
                    let Some(r) = reader.as_mut() else {
                        say("ERR no reader");
                        continue;
                    };
                    match r.wait_for(marker.as_bytes(), cursor, timeout) {
                        Some((end, at)) => {
                            if cmd == "GRAB" {
                                let d = r.data();
                                say(&format!("GRABBED {}", escape(d.get(cursor..end).unwrap_or_default())));
                            } else {
                                say(&format!("SAW {marker} {at} {last_send}"));
                            }
                            cursor = end;
                        }
                        None => {
                            let tail = r.data();
                            let from = tail.len().saturating_sub(300);
                            say(&format!("TIMEOUT {marker} bytes={} tail={}", tail.len(), escape(tail.get(from..).unwrap_or_default())));
                        }
                    }
                }
                "STOP" => {
                    let t0 = qpc();
                    let (why, data, answered) = match reader.as_mut() {
                        Some(r) => (r.stop(), r.data(), r.answered()),
                        None => (String::from("none"), Vec::new(), 0),
                    };
                    let t1 = qpc();
                    if !rest.is_empty()
                        && rest != "-"
                        && let Err(e) = std::fs::write(rest, &data)
                    {
                        say(&format!("ERR write {e}"));
                    }
                    say(&format!("STOPPED {t0} {t1} {} {why} {answered}", data.len()));
                }
                "ASKRESIZE" => say(&format!("RESIZE {rest} {}", qpc())),
                "RESIZED" => say(&format!("RESIZEACK {rest} {}", qpc())),
                "DACL" => match private_dacl() {
                    Ok(()) => say("DACL ok"),
                    Err(e) => say(&format!("DACL err {e}")),
                },
                "WATCH" => {
                    watch(Path::new(rest), reader.take(), shell, conhost);
                    return Ok(0);
                }
                "EXIT" => return Ok(rest.parse().unwrap_or(0)),
                _ => say(&format!("ERR unknown command {cmd} ({label})")),
            }
        }
        // stdin ended: the custodian went away.
        Ok(3)
    }

    /// Watch the shell, conhost and the output pipe until all three end (or 20 s), and write
    /// when each did to `path`: the custodian is about to go away.
    fn watch(path: &Path, mut reader: Option<Reader>, shell: Option<Raw>, conhost: Option<Raw>) {
        say("WATCHING");
        let start = qpc();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut shell_end: Option<(i64, Option<u32>)> = None;
        let mut conhost_end: Option<(i64, Option<u32>)> = None;
        loop {
            if shell_end.is_none()
                && let Some(s) = shell
                && wait(s.0, 0)
            {
                shell_end = Some((qpc(), exit_code(s.0)));
            }
            if conhost_end.is_none()
                && let Some(k) = conhost
                && wait(k.0, 0)
            {
                conhost_end = Some((qpc(), exit_code(k.0)));
            }
            let pipe_end = reader.as_ref().and_then(Reader::ended);
            if (shell_end.is_some() && conhost_end.is_some() && pipe_end.is_some()) || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let pipe_end = reader.as_ref().and_then(Reader::ended);
        let bytes = reader.as_ref().map_or(0, Reader::len);
        let mut s = format!("start={start}\nbytes={bytes}\n");
        match shell_end {
            Some((at, code)) => s.push_str(&format!("shell_exit={at}\nshell_code={}\n", code.map_or("?".into(), |c| format!("{c:#x}")))),
            None => s.push_str("shell_exit=none\n"),
        }
        match conhost_end {
            Some((at, code)) => s.push_str(&format!("conhost_exit={at}\nconhost_code={}\n", code.map_or("?".into(), |c| format!("{c:#x}")))),
            None => s.push_str("conhost_exit=none\n"),
        }
        match pipe_end {
            Some((why, at)) => s.push_str(&format!("pipe_end={at}\npipe_why={why}\n")),
            None => s.push_str("pipe_end=none\n"),
        }
        let _ = std::fs::write(path, s);
        if let Some(r) = reader.as_mut() {
            r.stop();
        }
    }

    // ------------------------------------------------------------------ the custodian's side

    /// A client as the custodian sees it: its process, its command pipe, its answers.
    struct Client {
        proc: H,
        pid: u32,
        ctl: H,
        rx: mpsc::Receiver<(String, i64)>,
        pending: RefCell<Vec<(String, i64)>>,
        /// QPC as the custodian started it, and as its HELLO arrived.
        spawned: i64,
        hello: i64,
        label: String,
    }

    fn lines_of(h: H, tag: String) -> io::Result<mpsc::Receiver<(String, i64)>> {
        let (tx, rx) = mpsc::channel();
        // SAFETY: we own h; File takes it over.
        let file = unsafe { File::from_raw_handle(h.into_raw()) };
        std::thread::Builder::new().name(tag).spawn(move || {
            for line in BufReader::new(file).lines() {
                let Ok(line) = line else { break };
                if tx.send((line, qpc())).is_err() {
                    break;
                }
            }
        })?;
        Ok(rx)
    }

    impl Client {
        fn start(label: &str, extra: &[HANDLE]) -> io::Result<Client> {
            let exe = std::env::current_exe()?;
            let (to_r, to_w) = pipe(0)?;
            let (from_r, from_w) = pipe(0)?;
            set_inheritable(to_r.raw(), true)?;
            set_inheritable(from_w.raw(), true)?;
            let err = stderr_for_child();
            let mut list = vec![to_r.raw(), from_w.raw()];
            if let Some(e) = &err {
                list.push(e.raw());
            }
            list.extend_from_slice(extra);
            let e = err.as_ref().map_or(INVALID_HANDLE_VALUE, H::raw);
            let cmdline = format!("\"{}\" client {label}", exe.display());
            let spawned = qpc();
            let (proc, pid) = spawn(&cmdline, &list, Some([to_r.raw(), from_w.raw(), e]), CREATE_NO_WINDOW, None)?;
            drop((to_r, from_w, err));
            let rx = lines_of(from_r, format!("ctl-{label}"))?;
            let mut c = Client { proc, pid, ctl: to_w, rx, pending: RefCell::new(Vec::new()), spawned, hello: 0, label: label.to_owned() };
            let (_, at) = c.line("HELLO", Duration::from_secs(30))?;
            c.hello = at;
            Ok(c)
        }

        fn cmd(&self, s: &str) -> io::Result<()> {
            write_all(self.ctl.raw(), format!("{s}\n").as_bytes())
        }

        /// The next answer whose first word is `word` (others are kept for later).
        fn line(&self, word: &str, timeout: Duration) -> io::Result<(Vec<String>, i64)> {
            let split = |l: &str| l.split(' ').map(str::to_owned).collect::<Vec<_>>();
            {
                let mut p = self.pending.borrow_mut();
                if let Some(i) = p.iter().position(|(l, _)| l.split(' ').next() == Some(word)) {
                    let (l, at) = p.remove(i);
                    return Ok((split(&l), at));
                }
            }
            let deadline = Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.rx.recv_timeout(left) {
                    Ok((l, at)) => {
                        let head = l.split(' ').next().unwrap_or_default();
                        if head == word {
                            return Ok((split(&l), at));
                        }
                        if head == "ERR" || head == "TIMEOUT" {
                            return Err(other(format!("client {}: {l}", self.label)));
                        }
                        self.pending.borrow_mut().push((l, at));
                    }
                    Err(_) => return Err(other(format!("client {}: no {word} within {timeout:?}", self.label))),
                }
            }
        }

        /// SEND then EXPECT; the SAW answer: (seen QPC, sent QPC).
        fn round_trip(&self, send: &str, marker: &str, timeout_ms: u64) -> io::Result<(i64, i64)> {
            self.cmd(&format!("SEND {send}"))?;
            self.cmd(&format!("EXPECT {marker} {timeout_ms}"))?;
            let (f, _) = self.line("SAW", Duration::from_millis(timeout_ms + 5000))?;
            Ok((num(&f, 2), num(&f, 3)))
        }

        fn ready(&self, shell: Shell) -> io::Result<()> {
            self.round_trip(&shell.echo("RE", "ADY#"), "READY#", 30_000).map(|_| ())
        }

        /// STOP: (asked QPC, stopped QPC, bytes it read, why its reader ended).
        fn stop(&self, save: Option<&Path>) -> io::Result<(i64, i64, usize, String)> {
            self.cmd(&format!("STOP {}", save.map_or("-".to_owned(), |p| p.display().to_string())))?;
            let (f, _) = self.line("STOPPED", Duration::from_secs(20))?;
            Ok((num(&f, 1), num(&f, 2), num(&f, 3) as usize, f.get(4).cloned().unwrap_or_default()))
        }

        /// Ask it to exit; the QPC its exit was seen at.
        fn exit(&self) -> i64 {
            let _ = self.cmd("EXIT 0");
            if !wait(self.proc.raw(), 10_000) {
                terminate(self.proc.raw());
                wait(self.proc.raw(), 5_000);
            }
            qpc()
        }
    }

    impl Drop for Client {
        fn drop(&mut self) {
            if !wait(self.proc.raw(), 0) {
                terminate(self.proc.raw());
            }
        }
    }

    fn num(f: &[String], i: usize) -> i64 {
        f.get(i).and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Shell {
        Cmd,
        Powershell,
    }

    impl Shell {
        fn name(self) -> &'static str {
            match self {
                Shell::Cmd => "cmd",
                Shell::Powershell => "powershell",
            }
        }

        fn command_line(self) -> String {
            match self {
                Shell::Cmd => std::env::var("COMSPEC").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_owned()),
                Shell::Powershell => r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -NoLogo -NoProfile".to_owned(),
            }
        }

        /// A command printing `a` then `b`; its own text never has them joined.
        fn echo(self, a: &str, b: &str) -> String {
            match self {
                Shell::Cmd => format!("echo {a}^{b}"),
                Shell::Powershell => format!("echo ('{a}'+'{b}')"),
            }
        }

        fn set_var(self, k: &str, v: &str) -> String {
            match self {
                Shell::Cmd => format!("set {k}={v}"),
                Shell::Powershell => format!("$env:{k}='{v}'"),
            }
        }

        fn echo_var(self, prefix: &str, k: &str) -> String {
            match self {
                Shell::Cmd => format!("echo {prefix}%{k}%"),
                Shell::Powershell => format!("echo ('{prefix}'+$env:{k})"),
            }
        }

        /// Prints L1..Ln, one per line, then writes `marker` (the shell got through it).
        fn print_lines(self, n: u64, marker: &Path) -> String {
            match self {
                Shell::Cmd => format!("(for /L %i in (1,1,{n}) do @echo L%i) & echo done> \"{}\"", marker.display()),
                Shell::Powershell => format!("1..{n} | % {{ 'L'+$_ }}; Set-Content -LiteralPath '{}' -Value done", marker.display()),
            }
        }
    }

    /// A pseudoconsole with its shell, held by the custodian: the HPCON, the input pipe's write
    /// end and the output pipe's read end (the other ends are conhost's).
    struct Console {
        pc: HPCON,
        input: H,
        output: H,
        shell: H,
        shell_pid: u32,
        conhost: Option<(u32, H)>,
        closed: bool,
    }

    fn conhost_children(me: u32) -> Vec<u32> {
        processes().map(|ps| ps.into_iter().filter(|p| p.ppid == me && matches!(p.exe.to_ascii_lowercase().as_str(), "conhost.exe" | "openconsole.exe")).map(|p| p.pid).collect()).unwrap_or_default()
    }

    impl Console {
        fn open(shell: Shell, cols: i16, rows: i16, flags: u32, pipe_size: u32) -> io::Result<Console> {
            let me = std::process::id();
            let before = conhost_children(me);
            let (in_r, in_w) = pipe(pipe_size)?;
            let (out_r, out_w) = pipe(pipe_size)?;
            let mut pc: HPCON = null_mut();
            // SAFETY: valid pipe handles; out-parameter.
            let hr = unsafe { CreatePseudoConsole(COORD { X: cols, Y: rows }, in_r.raw(), out_w.raw(), flags, &mut pc) };
            if hr != 0 {
                return Err(other(format!("CreatePseudoConsole: HRESULT {hr:#010x}")));
            }
            let started = spawn(&shell.command_line(), &[], None, 0, Some(pc));
            // conhost has its own copies ("Creating a Pseudoconsole session": free them after
            // CreateProcess), so the pipes break when it goes.
            drop((in_r, out_w));
            let (sh, pid) = match started {
                Ok(x) => x,
                Err(e) => {
                    // SAFETY: created above.
                    unsafe { ClosePseudoConsole(pc) };
                    return Err(e);
                }
            };
            let conhost = conhost_children(me).into_iter().find(|p| !before.contains(p)).and_then(|p| open_process(p, SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION).ok().map(|h| (p, h)));
            Ok(Console { pc, input: in_w, output: out_r, shell: sh, shell_pid: pid, conhost, closed: false })
        }

        fn conhost_pid(&self) -> u32 {
            self.conhost.as_ref().map_or(0, |c| c.0)
        }

        fn resize(&self, cols: i16, rows: i16) -> HRESULT {
            // SAFETY: an open pseudoconsole.
            unsafe { ResizePseudoConsole(self.pc, COORD { X: cols, Y: rows }) }
        }

        /// ClosePseudoConsole; how long the call took (ms).
        fn close(&mut self) -> f64 {
            if self.closed {
                return 0.0;
            }
            let t = qpc();
            // SAFETY: an open pseudoconsole, closed once.
            unsafe { ClosePseudoConsole(self.pc) };
            self.closed = true;
            ms(qpc() - t)
        }

        /// Close it and make sure its shell is gone: (shell's exit ms after the close, its code).
        fn finish(&mut self) -> (Option<f64>, Option<u32>) {
            let t = qpc();
            self.close();
            let gone = wait(self.shell.raw(), 10_000);
            let at = gone.then(|| ms(qpc() - t));
            let code = exit_code(self.shell.raw());
            if !gone {
                terminate(self.shell.raw());
            }
            (at, code)
        }
    }

    impl Drop for Console {
        fn drop(&mut self) {
            self.finish();
        }
    }

    /// Give `c` the shell's I/O by DuplicateHandle with its process handle (the one
    /// CreateProcess returned); the ms the duplications took.
    fn attach_dup(con: &Console, c: &Client) -> io::Result<f64> {
        let t0 = qpc();
        let p = c.proc.raw();
        let i = dup_to(p, con.input.raw(), None, false)?;
        let o = dup_to(p, con.output.raw(), None, false)?;
        let s = dup_to(p, con.shell.raw(), Some(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION), false)?;
        let k = match &con.conhost {
            Some((_, h)) => dup_to(p, h.raw(), Some(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION), false)?,
            None => null_mut(),
        };
        let took = ms(qpc() - t0);
        c.cmd(&format!("HANDLES {:x} {:x} {:x} {:x}", i as usize, o as usize, s as usize, k as usize))?;
        Ok(took)
    }

    /// Start a client that inherits the shell's I/O (PROC_THREAD_ATTRIBUTE_HANDLE_LIST with
    /// inheritable duplicates made for it, closed here once it has them).
    fn start_inheriting(label: &str, con: &Console) -> io::Result<Client> {
        // SAFETY: a pseudo handle for ourselves.
        let me = unsafe { GetCurrentProcess() };
        let i = H(dup_to(me, con.input.raw(), None, true)?);
        let o = H(dup_to(me, con.output.raw(), None, true)?);
        let s = H(dup_to(me, con.shell.raw(), Some(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION), true)?);
        let k = match &con.conhost {
            Some((_, h)) => Some(H(dup_to(me, h.raw(), Some(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION), true)?)),
            None => None,
        };
        let mut extra = vec![i.raw(), o.raw(), s.raw()];
        if let Some(k) = &k {
            extra.push(k.raw());
        }
        let c = Client::start(label, &extra)?;
        let kv = k.as_ref().map_or(0, |k| k.raw() as usize);
        c.cmd(&format!("HANDLES {:x} {:x} {:x} {:x}", i.raw() as usize, o.raw() as usize, s.raw() as usize, kv))?;
        Ok(c)
    }

    // ------------------------------------------------------------------ analysis

    /// The numbers of the `L<n>` lines in some output, VT sequences stripped.
    fn tokens(b: &[u8]) -> Vec<u64> {
        let s = strip_vt(b);
        let mut v = Vec::new();
        let mut i = 0;
        while let Some(&c) = s.get(i) {
            if c != b'L' {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            let mut n = 0u64;
            while let Some(d) = s.get(j).filter(|d| d.is_ascii_digit()) {
                n = n.saturating_mul(10).saturating_add(u64::from(d - b'0'));
                j += 1;
            }
            if j > i + 1 {
                v.push(n);
            }
            i = j.max(i + 1);
        }
        v
    }

    struct Seq {
        seen: usize,
        missing: usize,
        dups: usize,
        breaks: usize,
        outside: usize,
        gaps: Vec<(u64, u64)>,
    }

    fn check_seq(v: &[u64], n: u64) -> Seq {
        let mut count = vec![0u32; usize::try_from(n).unwrap_or(0) + 1];
        let mut outside = 0;
        for &x in v {
            match usize::try_from(x).ok().filter(|_| x >= 1).and_then(|i| count.get_mut(i)) {
                Some(c) => *c += 1,
                None => outside += 1,
            }
        }
        let missing = count.iter().skip(1).filter(|&&c| c == 0).count();
        let dups = count.iter().map(|&c| c.saturating_sub(1) as usize).sum();
        let breaks = v.windows(2).filter(|w| w.get(1) <= w.first()).count();
        let mut gaps = Vec::new();
        let mut run: Option<(u64, u64)> = None;
        for x in 1..=n {
            let absent = count.get(usize::try_from(x).unwrap_or(0)).is_some_and(|&c| c == 0);
            match (absent, run) {
                (true, None) => run = Some((x, x)),
                (true, Some((a, _))) => run = Some((a, x)),
                (false, Some(r)) => {
                    gaps.push(r);
                    run = None;
                }
                (false, None) => {}
            }
        }
        if let Some(r) = run {
            gaps.push(r);
        }
        Seq { seen: v.len(), missing, dups, breaks, outside, gaps }
    }

    fn gaps_text(g: &[(u64, u64)]) -> String {
        if g.is_empty() {
            return "none".into();
        }
        let mut s: Vec<String> = g.iter().take(6).map(|(a, b)| if a == b { format!("{a}") } else { format!("{a}-{b}") }).collect();
        if g.len() > 6 {
            s.push(format!("...(+{})", g.len() - 6));
        }
        s.join(",")
    }

    fn read_or_empty(p: &Path) -> Vec<u8> {
        std::fs::read(p).unwrap_or_default()
    }

    // ------------------------------------------------------------------ scenarios

    pub fn main(args: &[String]) -> io::Result<i32> {
        let arg = |i: usize| args.get(i).map(String::as_str);
        match arg(1) {
            Some("client") => client(arg(2).unwrap_or("?")),
            Some("exit-child") => exit_child(arg(2).unwrap_or("return"), Path::new(arg(3).unwrap_or("exit-child"))),
            Some(scenario) => {
                let dir = PathBuf::from(arg(2).ok_or_else(|| other("usage: conpty-custody <scenario> <dir>"))?);
                std::fs::create_dir_all(&dir)?;
                let f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("results.txt"))?;
                *RESULTS.lock().unwrap_or_else(PoisonError::into_inner) = Some(f);
                report(&format!("RUN scenario={scenario} custodian_pid={} qpc_hz={}", std::process::id(), freq()));
                run(scenario, &dir, args.get(3..).unwrap_or_default())?;
                Ok(0)
            }
            None => {
                eprintln!("usage: conpty-custody <probe|basic|backlog|continuity|timing|exit|all> <dir> [n]");
                Ok(2)
            }
        }
    }

    fn run(scenario: &str, dir: &Path, rest: &[String]) -> io::Result<()> {
        let n: usize = rest.first().and_then(|s| s.parse().ok()).unwrap_or(100);
        let each = |name: &str, r: io::Result<()>| {
            if let Err(e) = r {
                report(&format!("RESULT {name} error={}", escape(e.to_string().as_bytes())));
            }
        };
        match scenario {
            "probe" => each("probe", probe(dir)),
            "basic" => {
                each("basic.cmd", basic(dir, Shell::Cmd));
                each("basic.powershell", basic(dir, Shell::Powershell));
            }
            "backlog" => each("backlog", backlog(dir)),
            "continuity" => each("continuity", continuity(dir, n.min(20))),
            "timing" => each("timing", timing(dir, n)),
            "exit" => {
                each("exit", exit_driver(dir, n.min(10)));
                each("close-undrained", close_undrained(dir));
            }
            "shell-exit" => each("shell-exit", shell_exit(dir, n.min(10))),
            "all" => {
                each("probe", probe(dir));
                each("basic.cmd", basic(dir, Shell::Cmd));
                each("basic.powershell", basic(dir, Shell::Powershell));
                each("backlog", backlog(dir));
                each("continuity", continuity(dir, 5));
                each("timing", timing(dir, 100));
                each("exit", exit_driver(dir, 3));
                each("close-undrained", close_undrained(dir));
                each("shell-exit", shell_exit(dir, 3));
            }
            other_name => return Err(other(format!("unknown scenario {other_name}"))),
        }
        Ok(())
    }

    /// What conhost writes first, which queries it asks the terminal, and how long a shell takes
    /// to be ready; with and without portable-pty's flags. Also which ConPTY entry points the
    /// system exports.
    fn probe(dir: &Path) -> io::Result<()> {
        for module in ["kernel32.dll", "kernelbase.dll"] {
            let m = wide(module);
            // SAFETY: valid string; both modules are loaded in every process.
            let h = unsafe { GetModuleHandleW(m.as_ptr()) };
            let mut have = Vec::new();
            for f in ["CreatePseudoConsole", "ResizePseudoConsole", "ClosePseudoConsole", "ReleasePseudoConsole", "ConptyReleasePseudoConsole", "ShowHidePseudoConsole", "ClearPseudoConsole"] {
                let name: Vec<u8> = f.bytes().chain(std::iter::once(0)).collect();
                // SAFETY: valid module handle (or null, which fails) and string.
                let p = if h.is_null() { null_mut() } else { unsafe { GetProcAddress(h, name.as_ptr()) } };
                have.push(format!("{f}={}", !p.is_null()));
            }
            report(&format!("RESULT probe.exports {module} {}", have.join(" ")));
        }
        for (shell, flags) in [(Shell::Cmd, 0), (Shell::Cmd, PORTABLE_PTY_FLAGS), (Shell::Powershell, 0), (Shell::Powershell, PORTABLE_PTY_FLAGS)] {
            let tag = format!("probe-{}-flags{flags}", shell.name());
            let t0 = qpc();
            let con = Console::open(shell, 120, 30, flags, 0)?;
            let opened = ms(qpc() - t0);
            let sizes = pipe_sizes(con.output.raw());
            let c = Client::start("probe", &[])?;
            let raw = dir.join(format!("{tag}.out"));
            c.cmd(&format!("SINK {}", raw.display()))?;
            let attached_at = qpc();
            attach_dup(&con, &c)?;
            let (seen, _) = c.round_trip(&shell.echo("RE", "ADY#"), "READY#", 30_000)?;
            let (_, _, bytes, _) = c.stop(None)?;
            c.exit();
            let data = read_or_empty(&raw);
            let head = data.get(..data.len().min(600)).unwrap_or_default();
            report(&format!(
                "RESULT probe {tag} create_pc_and_shell_ms={opened:.2} client_attached_to_ready_ms={:.2} bytes_to_ready={bytes} pipe_buffers(out,in)={sizes:?} conhost_pid={} head={}",
                ms(seen - attached_at),
                con.conhost_pid(),
                escape(head)
            ));
        }
        Ok(())
    }

    /// The core: client 1 (DuplicateHandle) uses the shell and exits, the shell lives on, client 2
    /// (inherited) takes it over and resizes it through the custodian, client 3 (behind kiln's
    /// private DACL) is reached through its creation handle; then who owns what, and close.
    fn basic(dir: &Path, shell: Shell) -> io::Result<()> {
        let name = format!("basic.{}", shell.name());
        let me = std::process::id();
        let mut con = Console::open(shell, 120, 30, 0, 0)?;
        let procs = processes()?;
        let parent_of = |pid: u32| procs.iter().find(|p| p.pid == pid).map(|p| p.ppid);
        let exe_of = |pid: u32| procs.iter().find(|p| p.pid == pid).map_or(String::new(), |p| p.exe.clone());
        let ch = con.conhost_pid();
        // SAFETY: a pseudo handle for ourselves.
        let me_h = unsafe { GetCurrentProcess() };
        report(&format!(
            "RESULT {name}.ownership custodian={me} shell={} shell_exe={} shell_parent={:?} conhost={ch} conhost_exe={} conhost_parent={:?} in_job(custodian,shell,conhost)=({:?},{:?},{:?}) session(custodian,shell,conhost)=({:?},{:?},{:?})",
            con.shell_pid,
            exe_of(con.shell_pid),
            parent_of(con.shell_pid),
            exe_of(ch),
            parent_of(ch),
            in_job(me_h),
            in_job(con.shell.raw()),
            con.conhost.as_ref().and_then(|c| in_job(c.1.raw())),
            session_of(me),
            session_of(con.shell_pid),
            session_of(ch)
        ));
        report(&format!("RESULT {name}.pipes output_pipe_buffers(out,in)={:?} input_pipe_buffers(out,in)={:?}", pipe_sizes(con.output.raw()), pipe_sizes(con.input.raw())));

        // Client 1: DuplicateHandle into it.
        let c1 = Client::start("c1", &[])?;
        let dup_ms = attach_dup(&con, &c1)?;
        c1.ready(shell)?;
        c1.cmd(&format!("SEND {}", shell.set_var("CUSTODY", "from-client-1")))?;
        c1.round_trip(&shell.echo("C1", "-OK#"), "C1-OK#", 15_000)?;
        let (_, _, c1_bytes, c1_why) = c1.stop(Some(&dir.join(format!("{name}-c1.out"))))?;
        let c1_pid = c1.pid;
        c1.exit();
        drop(c1);
        std::thread::sleep(Duration::from_millis(500));
        let shell_alive = !wait(con.shell.raw(), 0);
        let conhost_alive = con.conhost.as_ref().is_some_and(|c| !wait(c.1.raw(), 0));
        report(&format!(
            "RESULT {name}.client1 how=DuplicateHandle dup_ms={dup_ms:.3} pid={c1_pid} read_bytes={c1_bytes} reader_end={c1_why} exited=yes shell_alive_500ms_after={shell_alive} conhost_alive_500ms_after={conhost_alive}"
        ));

        // Client 2: inherited; sees client 1's shell state, and resizes through the custodian.
        let c2 = start_inheriting("c2", &con)?;
        let (seen, sent) = c2.round_trip(&shell.echo_var("C2-sees-", "CUSTODY"), "C2-sees-from-client-1", 15_000)?;
        report(&format!("RESULT {name}.client2 how=HANDLE_LIST same_shell_state=yes first_round_trip_ms={:.2}", ms(seen - sent)));
        c2.cmd("ASKRESIZE 100 40")?;
        let (req, req_at) = c2.line("RESIZE", Duration::from_secs(10))?;
        let cols: i16 = req.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
        let rows: i16 = req.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
        let t0 = qpc();
        let hr = con.resize(cols, rows);
        let t1 = qpc();
        c2.cmd(&format!("RESIZED {hr:#x} {t1}"))?;
        let (ack, _) = c2.line("RESIZEACK", Duration::from_secs(10))?;
        let client_round = ms(num(&ack, 3) - num(&req, 3));
        let mut size_seen = String::from("never");
        for attempt in 1..=5 {
            let ok_size = match shell {
                Shell::Cmd => {
                    c2.cmd("SEND mode con")?;
                    c2.cmd("GRAB Code page: 10000")?;
                    let (g, _) = c2.line("GRABBED", Duration::from_secs(15))?;
                    let text = unescape(&g.get(1..).unwrap_or_default().join(" "));
                    let (c, r) = (number_after(&text, "Columns:"), number_after(&text, "Lines:"));
                    size_seen = format!("{c:?}x{r:?}@attempt{attempt}");
                    c == Some(u64::try_from(cols).unwrap_or(0)) && r == Some(u64::try_from(rows).unwrap_or(0))
                }
                Shell::Powershell => {
                    let marker = format!("SIZE={cols}x{rows}@END");
                    c2.cmd("SEND echo ('SI'+'ZE='+[Console]::WindowWidth+'x'+[Console]::WindowHeight+[char]64+'END')")?;
                    c2.cmd("GRAB @END 10000")?;
                    let (g, _) = c2.line("GRABBED", Duration::from_secs(15))?;
                    let text = String::from_utf8_lossy(&strip_vt(&unescape(&g.get(1..).unwrap_or_default().join(" ")))).into_owned();
                    let hit = text.contains(&marker);
                    size_seen = format!("{}@attempt{attempt}", if hit { marker } else { escape(text.trim().as_bytes()) });
                    hit
                }
            };
            if ok_size {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        report(&format!(
            "RESULT {name}.resize asked_by=client2 over=custodian's_channel ResizePseudoConsole={hr:#x} call_us={:.1} client_request_to_ack_ms={client_round:.3} request_reached_custodian_ms={:.3} shell_sees={size_seen}",
            ms(t1 - t0) * 1000.0,
            ms(req_at - num(&req, 3))
        ));
        c2.stop(Some(&dir.join(format!("{name}-c2.out"))))?;
        c2.exit();
        drop(c2);

        // Client 3: kiln's runner DACL first; the custodian still reaches it with the handle
        // CreateProcess gave it, while OpenProcess for PROCESS_DUP_HANDLE is refused.
        let c3 = Client::start("c3", &[])?;
        c3.cmd("DACL")?;
        let (d, _) = c3.line("DACL", Duration::from_secs(10))?;
        let try_open = || match open_process(c3.pid, PROCESS_DUP_HANDLE) {
            Ok(_) => "granted".to_owned(),
            Err(e) => format!("refused({})", e.raw_os_error().map_or_else(|| e.to_string(), |c| if c == ERROR_ACCESS_DENIED { "ERROR_ACCESS_DENIED".into() } else { c.to_string() })),
        };
        let reopen_as_is = try_open();
        let held = debug_privilege(false);
        let reopen_without_debug = try_open();
        let reopen = format!("{reopen_as_is}(token_as_is) {reopen_without_debug}(SeDebugPrivilege_off,held={held:?})");
        // The creation handle, with SeDebugPrivilege still off.
        let dup = attach_dup(&con, &c3);
        let reached = match &dup {
            Ok(_) => c3.round_trip(&shell.echo_var("C3-sees-", "CUSTODY"), "C3-sees-from-client-1", 15_000).is_ok(),
            Err(_) => false,
        };
        report(&format!(
            "RESULT {name}.client3 private_dacl={} open_process_dup_handle={reopen} duplicate_via_creation_handle={} shell_reached={reached}",
            d.get(1).cloned().unwrap_or_default(),
            dup.as_ref().map_or_else(|e| format!("failed({e})"), |_| "ok".into())
        ));
        c3.stop(None)?;
        c3.exit();
        drop(c3);
        let _ = debug_privilege(true);

        // Close with nobody attached and nobody reading.
        let t = qpc();
        let close_ms = con.close();
        let shell_gone = wait(con.shell.raw(), 10_000).then(|| ms(qpc() - t));
        let shell_code = exit_code(con.shell.raw());
        let conhost_gone = con.conhost.as_ref().and_then(|c| wait(c.1.raw(), 10_000).then(|| ms(qpc() - t)));
        report(&format!(
            "RESULT {name}.close ClosePseudoConsole_ms={close_ms:.3} shell_exited_after_ms={shell_gone:?} shell_code={} conhost_exited_after_ms={conhost_gone:?}",
            shell_code.map_or("?".into(), |c| format!("{c:#x}"))
        ));
        Ok(())
    }

    /// Bytes the shell writes while no client reads: held in the pipe, or lost; how much the
    /// pipe holds; and whether the shell blocks.
    fn backlog(dir: &Path) -> io::Result<()> {
        for &(lines, pipe_size) in &[(100u64, 0u32), (1_000, 0), (10_000, 0), (100_000, 0), (10_000, 1 << 20)] {
            let tag = format!("backlog-{lines}-pipe{pipe_size}");
            let marker = dir.join(format!("{tag}.done"));
            let _ = std::fs::remove_file(&marker);
            let con = Console::open(Shell::Cmd, 120, 30, 0, pipe_size)?;
            let sizes = pipe_sizes(con.output.raw());
            let c1 = Client::start("b1", &[])?;
            attach_dup(&con, &c1)?;
            c1.ready(Shell::Cmd)?;
            c1.cmd(&format!("SEND {}", Shell::Cmd.print_lines(lines, &marker)))?;
            let c1_out = dir.join(format!("{tag}-c1.out"));
            let (_, stopped, c1_bytes, _) = c1.stop(Some(&c1_out))?;
            c1.exit();
            drop(c1);
            // Nobody reads now.
            let mut max_avail = 0u32;
            let mut last = 0u32;
            let mut last_change = qpc();
            let mut done_at: Option<i64> = None;
            let mut first_full: Option<(f64, u32)> = None;
            loop {
                let avail = peek(con.output.raw())?;
                let now = qpc();
                if avail != last {
                    last = avail;
                    last_change = now;
                }
                max_avail = max_avail.max(avail);
                if done_at.is_none() && marker.exists() {
                    done_at = Some(now);
                }
                let quiet = ms(now - last_change);
                if first_full.is_none() && quiet > 500.0 && avail > 0 {
                    first_full = Some((ms(last_change - stopped), avail));
                }
                if ms(now - stopped) > 8000.0 || (done_at.is_some() && quiet > 1000.0) || quiet > 3000.0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let held = peek(con.output.raw())?;
            let shell_alive = !wait(con.shell.raw(), 0);
            // The next client takes the terminal over and reads the rest.
            let c2 = Client::start("b2", &[])?;
            let t_attach = qpc();
            attach_dup(&con, &c2)?;
            let (seen, _) = c2.round_trip(&Shell::Cmd.echo("FIN", "ISH#"), "FINISH#", 300_000)?;
            let c2_out = dir.join(format!("{tag}-c2.out"));
            let (_, _, c2_bytes, _) = c2.stop(Some(&c2_out))?;
            c2.exit();
            drop(c2);
            let mut all = read_or_empty(&c1_out);
            all.extend(read_or_empty(&c2_out));
            let seq = check_seq(&tokens(&all), lines);
            report(&format!(
                "RESULT backlog lines={lines} pipe_size_asked={pipe_size} pipe_buffers(out,in)={sizes:?} c1_read={c1_bytes} held_in_pipe_max={max_avail} held_at_takeover={held} plateau_after_ms={:?} shell_finished_while_unread={} shell_alive={shell_alive} c2_read={c2_bytes} c2_drain_to_end_ms={:.1} lines_seen={} missing={} duplicated={} order_breaks={} outside={} gaps={}",
                first_full.map(|f| format!("{:.0}", f.0)),
                done_at.map_or("no".into(), |d| format!("yes({:.0}ms)", ms(d - stopped))),
                ms(seen - t_attach),
                seq.seen,
                seq.missing,
                seq.dups,
                seq.breaks,
                seq.outside,
                gaps_text(&seq.gaps)
            ));
        }
        Ok(())
    }

    /// Output under way while client 1 stops and client 2 takes over: exactly once, in order?
    /// The custodian drains between them, or not; or client 1 is killed mid-read.
    fn continuity(dir: &Path, reps: usize) -> io::Result<()> {
        let n = 50_000u64;
        for mode in ["nodrain", "drain", "kill"] {
            for rep in 0..reps {
                let tag = format!("cont-{mode}-{rep}");
                let marker = dir.join(format!("{tag}.done"));
                let _ = std::fs::remove_file(&marker);
                let con = Console::open(Shell::Cmd, 120, 30, 0, 0)?;
                let c1_out = dir.join(format!("{tag}-c1.out"));
                let c2_out = dir.join(format!("{tag}-c2.out"));
                let drained_out = dir.join(format!("{tag}-custodian.out"));
                let c1 = Client::start("k1", &[])?;
                c1.cmd(&format!("SINK {}", c1_out.display()))?;
                attach_dup(&con, &c1)?;
                c1.ready(Shell::Cmd)?;
                c1.cmd(&format!("SEND {}", Shell::Cmd.print_lines(n, &marker)))?;
                // Typed ahead while the shell is busy: it sits in the input pipe or conhost's
                // input buffer across the hand-over.
                c1.cmd(&format!("SEND {}", Shell::Cmd.echo("TYPED", "-BY-C1#")))?;
                std::thread::sleep(Duration::from_millis(200));
                let mut c1_bytes = 0usize;
                if mode == "kill" {
                    terminate(c1.proc.raw());
                    wait(c1.proc.raw(), 5_000);
                } else {
                    c1_bytes = c1.stop(None)?.2;
                }
                let mut drain = if mode == "drain" { Some(Reader::start(Raw(con.output.raw()), None, Some(File::create(&drained_out)?), None)?) } else { None };
                if mode != "kill" {
                    c1.exit();
                }
                drop(c1);
                // The gap: the next runner coming up.
                std::thread::sleep(Duration::from_millis(300));
                let drained = match drain.as_mut() {
                    Some(r) => {
                        r.stop();
                        r.len()
                    }
                    None => 0,
                };
                drop(drain);
                let finished_in_gap = marker.exists();
                let c2 = Client::start("k2", &[])?;
                c2.cmd(&format!("SINK {}", c2_out.display()))?;
                attach_dup(&con, &c2)?;
                c2.round_trip(&Shell::Cmd.echo("FIN", "ISH#"), "FINISH#", 300_000)?;
                let (_, _, c2_bytes, _) = c2.stop(None)?;
                c2.exit();
                drop(c2);
                let a = read_or_empty(&c1_out);
                let b = read_or_empty(&drained_out);
                let c = read_or_empty(&c2_out);
                let (ta, tb, tc) = (tokens(&a), tokens(&b), tokens(&c));
                let mut all = a.clone();
                all.extend_from_slice(&b);
                all.extend_from_slice(&c);
                let seq = check_seq(&tokens(&all), n);
                let plain = strip_vt(&all);
                let typed = match (find(&plain, b"TYPED-BY-C1#"), find(&plain, b"FINISH#"), find(&plain, format!("L{n}\r").as_bytes())) {
                    (Some(t), Some(f), Some(l)) if l < t && t < f => "yes(after the last line, before client 2's)".to_owned(),
                    (t, f, l) => format!("no(at={t:?},finish={f:?},last_line={l:?})"),
                };
                report(&format!(
                    "RESULT continuity mode={mode} rep={rep} lines={n} c1_bytes={} custodian_drained={drained} c2_bytes={} shell_finished_in_gap={finished_in_gap} c1_last=L{:?} custodian_first..last=L{:?}..L{:?} c2_first=L{:?} lines_seen={} missing={} duplicated={} order_breaks={} outside={} gaps={} typed_ahead_by_client1_ran={typed}",
                    a.len().max(c1_bytes),
                    c.len().max(c2_bytes),
                    ta.last(),
                    tb.first(),
                    tb.last(),
                    tc.first(),
                    seq.seen,
                    seq.missing,
                    seq.dups,
                    seq.breaks,
                    seq.outside,
                    gaps_text(&seq.gaps)
                ));
                drop(con);
            }
        }
        Ok(())
    }

    /// How long a hand-over between clients takes: warm (the next client already running) and
    /// cold (started after the last one exits), by DuplicateHandle or inheritance; with the
    /// shell's own echo round trip and DuplicateHandle's cost beside.
    fn timing(dir: &Path, n: usize) -> io::Result<()> {
        let _ = dir;
        let con = Console::open(Shell::Cmd, 120, 30, 0, 0)?;
        let mut c = Client::start("t", &[])?;
        attach_dup(&con, &c)?;
        c.ready(Shell::Cmd)?;
        let mut rtt = Vec::new();
        for i in 0..n {
            let (seen, sent) = c.round_trip(&Shell::Cmd.echo("E", &format!("{i}#")), &format!("E{i}#"), 10_000)?;
            rtt.push(ms(seen - sent));
        }
        stat("timing.echo_round_trip(one client, steady)", "ms", &rtt);
        let mut dups = Vec::new();
        for _ in 0..1000 {
            let t = qpc();
            let h = dup_to(c.proc.raw(), con.input.raw(), None, false)?;
            dups.push(ms(qpc() - t) * 1000.0);
            close_in(c.proc.raw(), h);
        }
        stat("timing.DuplicateHandle(one pipe handle into a client)", "us", &dups);

        // Warm: the next client is up before the last one stops.
        let (mut total, mut gap, mut stop, mut dupt, mut echo) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for k in 0..n {
            let next = Client::start(&format!("w{k}"), &[])?;
            let (asked, stopped, _, _) = c.stop(None)?;
            let d = attach_dup(&con, &next)?;
            let (seen, sent) = next.round_trip(&Shell::Cmd.echo("W", &format!("{k}#")), &format!("W{k}#"), 10_000)?;
            let (att, _) = next.line("ATTACHED", Duration::from_secs(5))?;
            echo.push(ms(seen - sent));
            total.push(ms(seen - stopped));
            gap.push(ms(num(&att, 1) - stopped));
            stop.push(ms(stopped - asked));
            dupt.push(d * 1000.0);
            c.exit();
            c = next;
        }
        stat("timing.warm.stop_reading(CancelSynchronousIo+join)", "ms", &stop);
        stat("timing.warm.duplicate_4_handles", "us", &dupt);
        stat("timing.warm.unread_gap(old stopped -> new reading)", "ms", &gap);
        stat("timing.warm.new_client_echo_round_trip", "ms", &echo);
        stat("timing.warm.handover(old stopped -> new sees its echo)", "ms", &total);

        for how in ["DuplicateHandle", "HANDLE_LIST"] {
            let (mut total, mut spawn_ms, mut gap, mut exit_ms, mut echo) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for k in 0..n {
                let (_, stopped, _, _) = c.stop(None)?;
                let exited = c.exit();
                let next = if how == "DuplicateHandle" {
                    let next = Client::start(&format!("c{k}"), &[])?;
                    attach_dup(&con, &next)?;
                    next
                } else {
                    start_inheriting(&format!("i{k}"), &con)?
                };
                let tag = if how == "DuplicateHandle" { "C" } else { "I" };
                let (seen, sent) = next.round_trip(&Shell::Cmd.echo(tag, &format!("{k}#")), &format!("{tag}{k}#"), 10_000)?;
                let (att, _) = next.line("ATTACHED", Duration::from_secs(5))?;
                echo.push(ms(seen - sent));
                total.push(ms(seen - stopped));
                spawn_ms.push(ms(next.hello - next.spawned));
                gap.push(ms(num(&att, 1) - stopped));
                exit_ms.push(ms(exited - stopped));
                c = next;
            }
            stat(&format!("timing.cold.{how}.old_exit_seen(after it stopped)"), "ms", &exit_ms);
            stat(&format!("timing.cold.{how}.start_client(CreateProcess -> its HELLO)"), "ms", &spawn_ms);
            stat(&format!("timing.cold.{how}.unread_gap(old stopped -> new reading)"), "ms", &gap);
            stat(&format!("timing.cold.{how}.new_client_echo_round_trip"), "ms", &echo);
            stat(&format!("timing.cold.{how}.handover(old stopped -> new sees its echo)"), "ms", &total);
        }
        c.stop(None)?;
        c.exit();
        Ok(())
    }

    /// The custodian goes away with a client attached and reading: it returns without
    /// ClosePseudoConsole, is killed, or closes first. What becomes of the shell, conhost and the
    /// client's pipe; timed from a driver that watches them all.
    fn exit_driver(dir: &Path, reps: usize) -> io::Result<()> {
        let exe = std::env::current_exe()?;
        for mode in ["return", "kill", "close", "release"] {
            for rep in 0..reps {
                let tag = dir.join(format!("exit-{mode}-{rep}"));
                let _ = std::fs::remove_file(tag.with_extension("watch"));
                let (to_r, to_w) = pipe(0)?;
                let (from_r, from_w) = pipe(0)?;
                set_inheritable(to_r.raw(), true)?;
                set_inheritable(from_w.raw(), true)?;
                let err = stderr_for_child();
                let mut list = vec![to_r.raw(), from_w.raw()];
                if let Some(e) = &err {
                    list.push(e.raw());
                }
                let e = err.as_ref().map_or(INVALID_HANDLE_VALUE, H::raw);
                let (sub, _) = spawn(&format!("\"{}\" exit-child {mode} \"{}\"", exe.display(), tag.display()), &list, Some([to_r.raw(), from_w.raw(), e]), CREATE_NO_WINDOW, None)?;
                drop((to_r, from_w, err));
                let rx = lines_of(from_r, "exit-child".into())?;
                let next = |word: &str| -> io::Result<(Vec<String>, i64)> {
                    let deadline = Instant::now() + Duration::from_secs(60);
                    loop {
                        let left = deadline.saturating_duration_since(Instant::now());
                        let (l, at) = rx.recv_timeout(left).map_err(|_| other(format!("exit-child: no {word}")))?;
                        let f: Vec<String> = l.split(' ').map(str::to_owned).collect();
                        if f.first().map(String::as_str) == Some(word) {
                            return Ok((f, at));
                        }
                        if f.first().map(String::as_str) == Some("ERR") {
                            return Err(other(format!("exit-child: {l}")));
                        }
                    }
                };
                let (ready, _) = next("READY")?;
                let pid = |i: usize| ready.get(i).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                let (client_pid, shell_pid, conhost_pid) = (pid(1), pid(2), pid(3));
                let watch_access = SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
                let client = open_process(client_pid, watch_access | PROCESS_TERMINATE)?;
                let shell = open_process(shell_pid, watch_access | PROCESS_TERMINATE)?;
                let conhost = open_process(conhost_pid, watch_access).ok();
                std::thread::sleep(Duration::from_millis(300));
                let mut close_ms = None;
                let mut released = None;
                let t0 = if mode == "kill" {
                    let t = qpc();
                    terminate(sub.raw());
                    t
                } else {
                    write_all(to_w.raw(), b"GO\n")?;
                    if mode == "close" {
                        let (c, _) = next("CLOSED")?;
                        close_ms = Some(ms(num(&c, 2) - num(&c, 1)));
                    }
                    if mode == "release" {
                        let (r, _) = next("RELEASED")?;
                        released = r.get(1).cloned();
                    }
                    let (x, _) = next("EXITING")?;
                    num(&x, 1)
                };
                let sub_gone = wait(sub.raw(), 10_000).then(|| ms(qpc() - t0));
                let mut shell_at = None;
                let mut conhost_at = None;
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline && (shell_at.is_none() || (conhost.is_some() && conhost_at.is_none())) {
                    if shell_at.is_none() && wait(shell.raw(), 0) {
                        shell_at = Some(ms(qpc() - t0));
                    }
                    if conhost_at.is_none()
                        && let Some(k) = &conhost
                        && wait(k.raw(), 0)
                    {
                        conhost_at = Some(ms(qpc() - t0));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                // The client writes what it saw, then exits.
                let client_done = wait(client.raw(), 25_000);
                let w = String::from_utf8_lossy(&read_or_empty(&tag.with_extension("watch"))).into_owned();
                let field = |k: &str| w.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::to_owned);
                let rel = |k: &str| field(k).and_then(|v| v.parse::<i64>().ok()).map(|q| format!("{:.1}", ms(q - t0)));
                report(&format!(
                    "RESULT exit mode={mode} rep={rep} ClosePseudoConsole_ms={close_ms:?} ReleasePseudoConsole={released:?} custodian_gone_ms={sub_gone:?} shell_exit_ms={:?} shell_code={} conhost_exit_ms={:?} client(alive_till_it_reported)={} client_pipe_end_ms={:?} client_pipe_why={:?} client_saw_shell_exit_ms={:?} client_read_bytes={:?}",
                    shell_at.map(|v| format!("{v:.1}")),
                    exit_code(shell.raw()).map_or("?".into(), |c| format!("{c:#x}")),
                    conhost_at.map(|v| format!("{v:.1}")),
                    client_done,
                    rel("pipe_end"),
                    field("pipe_why"),
                    rel("shell_exit"),
                    field("bytes")
                ));
                for h in [&client, &shell] {
                    if !wait(h.raw(), 0) {
                        terminate(h.raw());
                    }
                }
                if !wait(sub.raw(), 0) {
                    terminate(sub.raw());
                }
            }
        }
        Ok(())
    }

    /// A custodian that hands its terminal to a client, then goes away as `mode` says: `return`
    /// (exits without ClosePseudoConsole), `kill` (waits to be killed), `close` (closes it first).
    fn exit_child(mode: &str, tag: &Path) -> io::Result<i32> {
        let con = Console::open(Shell::Cmd, 120, 30, 0, 0)?;
        let c = Client::start("x", &[])?;
        attach_dup(&con, &c)?;
        c.ready(Shell::Cmd)?;
        c.cmd(&format!("WATCH {}", tag.with_extension("watch").display()))?;
        c.line("WATCHING", Duration::from_secs(10))?;
        say(&format!("READY {} {} {}", c.pid, con.shell_pid, con.conhost_pid()));
        let mut go = String::new();
        let _ = io::stdin().read_line(&mut go);
        if mode == "close" {
            let t0 = qpc();
            // SAFETY: open, closed once; the process exits below without dropping `con`.
            unsafe { ClosePseudoConsole(con.pc) };
            let t1 = qpc();
            say(&format!("CLOSED {t0} {t1}"));
        }
        if mode == "release" {
            let hr = match release_pseudoconsole() {
                // SAFETY: an open pseudoconsole.
                Some(f) => unsafe { f(con.pc) },
                None => -1,
            };
            say(&format!("RELEASED {hr:#x}"));
        }
        say(&format!("EXITING {}", qpc()));
        // No destructors: nothing here closes the pseudoconsole or ends the client.
        std::process::exit(0);
    }

    /// The shell exits by itself (`exit`) while the custodian holds the pseudoconsole, as is, or
    /// after ReleasePseudoConsole: does conhost go, does the client's pipe end, and when; and does
    /// ResizePseudoConsole still work after the release.
    fn shell_exit(dir: &Path, reps: usize) -> io::Result<()> {
        let _ = dir;
        for mode in ["keep", "release"] {
            for rep in 0..reps {
                let mut con = Console::open(Shell::Cmd, 120, 30, 0, 0)?;
                let released = if mode == "release" {
                    // SAFETY: an open pseudoconsole.
                    release_pseudoconsole().map(|f| unsafe { f(con.pc) })
                } else {
                    None
                };
                let c = Client::start("s", &[])?;
                attach_dup(&con, &c)?;
                c.ready(Shell::Cmd)?;
                let resized = con.resize(100, 40);
                let t0 = qpc();
                c.cmd("SEND exit")?;
                let (mut shell_at, mut conhost_at, mut pipe_end) = (None, None, None);
                while ms(qpc() - t0) < 3000.0 && (shell_at.is_none() || conhost_at.is_none() || pipe_end.is_none()) {
                    if shell_at.is_none() && wait(con.shell.raw(), 0) {
                        shell_at = Some(ms(qpc() - t0));
                    }
                    if conhost_at.is_none()
                        && let Some(k) = &con.conhost
                        && wait(k.1.raw(), 0)
                    {
                        conhost_at = Some(ms(qpc() - t0));
                    }
                    if pipe_end.is_none()
                        && let Ok((f, at)) = c.line("READER-ENDED", Duration::from_millis(1))
                    {
                        pipe_end = Some(format!("{}@{:.1}ms", f.get(1).cloned().unwrap_or_default(), ms(at - t0)));
                    }
                }
                let code = exit_code(con.shell.raw());
                let t1 = qpc();
                let close_ms = con.close();
                let mut after_close = None;
                if pipe_end.is_none()
                    && let Ok((f, at)) = c.line("READER-ENDED", Duration::from_secs(3))
                {
                    after_close = Some(format!("{}@{:.1}ms", f.get(1).cloned().unwrap_or_default(), ms(at - t1)));
                }
                let conhost_after = con.conhost.as_ref().and_then(|k| wait(k.1.raw(), 3000).then(|| ms(qpc() - t1)));
                report(&format!(
                    "RESULT shell-exit mode={mode} rep={rep} ReleasePseudoConsole={} ResizePseudoConsole_after={resized:#x} shell_exit_ms={shell_at:?} shell_code={} conhost_exit_ms={conhost_at:?} client_pipe_end={pipe_end:?} (within 3 s of exit) ClosePseudoConsole_ms={close_ms:.3} client_pipe_end_after_close={after_close:?} conhost_exit_after_close_ms={conhost_after:?}",
                    released.map_or("-".into(), |h| format!("{h:#x}")),
                    code.map_or("running".into(), |c| format!("{c:#x}"))
                ));
                c.stop(None)?;
                c.exit();
            }
        }
        Ok(())
    }

    /// ClosePseudoConsole while nobody reads and the output pipe is full: does it return, does
    /// the shell end, and what does a reader find afterwards?
    fn close_undrained(dir: &Path) -> io::Result<()> {
        let marker = dir.join("close-undrained.done");
        let _ = std::fs::remove_file(&marker);
        let mut con = Console::open(Shell::Cmd, 120, 30, 0, 0)?;
        let c1 = Client::start("u1", &[])?;
        attach_dup(&con, &c1)?;
        c1.ready(Shell::Cmd)?;
        c1.cmd(&format!("SEND {}", Shell::Cmd.print_lines(200_000, &marker)))?;
        c1.stop(None)?;
        c1.exit();
        drop(c1);
        std::thread::sleep(Duration::from_millis(1000));
        let held = peek(con.output.raw())?;
        let t0 = qpc();
        let close_ms = con.close();
        let shell_gone = wait(con.shell.raw(), 5_000).then(|| ms(qpc() - t0));
        let conhost_gone = con.conhost.as_ref().and_then(|c| wait(c.1.raw(), 1_000).then(|| ms(qpc() - t0)));
        report(&format!(
            "RESULT close-undrained held_in_pipe={held} ClosePseudoConsole_ms={close_ms:.3} shell_exit_ms(nobody reading)={shell_gone:?} shell_code={} conhost_exit_ms={conhost_gone:?}",
            exit_code(con.shell.raw()).map_or("running".into(), |c| format!("{c:#x}"))
        ));
        // Now read what's left, as a late client would.
        let mut r = Reader::start(Raw(con.output.raw()), None, None, None)?;
        let t1 = qpc();
        let mut shell_after = None;
        let mut conhost_after = None;
        while ms(qpc() - t1) < 10_000.0 {
            if shell_after.is_none() && wait(con.shell.raw(), 0) {
                shell_after = Some(ms(qpc() - t1));
            }
            if conhost_after.is_none()
                && let Some(c) = &con.conhost
                && wait(c.1.raw(), 0)
            {
                conhost_after = Some(ms(qpc() - t1));
            }
            if r.ended().is_some() && shell_after.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let end = r.ended();
        let bytes = r.len();
        let first = r.first_read().map(|q| ms(q - t1));
        r.stop();
        report(&format!(
            "RESULT close-undrained.after_draining read_bytes={bytes} first_read_ms={first:?} pipe_end={:?} shell_exit_ms={shell_after:?} shell_code={} conhost_exit_ms={conhost_after:?}",
            end.map(|(w, q)| format!("{w}@{:.1}ms", ms(q - t1))),
            exit_code(con.shell.raw()).map_or("running".into(), |c| format!("{c:#x}"))
        ));
        Ok(())
    }
}
