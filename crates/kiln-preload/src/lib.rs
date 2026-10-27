//! The tracer's interposition library (docs/ci.md W24-R05). Loaded with `LD_PRELOAD`, it wraps the
//! C library's calls that take a path (`open`, `openat`, `fopen`, `stat`, `lstat`, `access`,
//! `opendir`, `execve`) and `getenv`, which seccomp can't see, and writes a record of each to the
//! descriptor `KILN_TRACE_FD` names: `pid\tcall\tpath\n`, a relative path made absolute against the
//! working folder, a variable by its name. Each record is one `write` of at most 4,096 bytes, whole
//! on a pipe. A program that doesn't go through the C library (Go's, or one making system calls
//! itself) escapes it, and its unit is partial.
// It interposes on Linux's C library (glibc's `__errno_location`, `fopen64`): elsewhere the
// library is empty, as the tracer is Linux's.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]
// `open`'s and `openat`'s C definitions are variadic, which stable Rust can't write: defined with
// the mode as a third (or fourth) argument they receive it where the x86_64 and aarch64 Linux ABIs
// pass a variadic one.
#![allow(invalid_runtime_symbol_definitions)]

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use libc::mode_t;

/// The descriptor records go to: -2 before it's read, -1 for none.
static FD: AtomicI32 = AtomicI32::new(-2);

thread_local! {
    /// A record being written on this thread: what it calls isn't noted again.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

/// The C library's own `name`, looked up once.
fn real(slot: &AtomicUsize, name: &CStr) -> usize {
    let mut p = slot.load(Ordering::Relaxed);
    if p == 0 {
        p = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) } as usize;
        slot.store(p, Ordering::Relaxed);
    }
    p
}

static GETENV: AtomicUsize = AtomicUsize::new(0);

fn real_getenv(name: *const c_char) -> *mut c_char {
    match real(&GETENV, c"getenv") {
        0 => std::ptr::null_mut(),
        p => unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(*const c_char) -> *mut c_char>(p)(name) },
    }
}

fn fd() -> c_int {
    let f = FD.load(Ordering::Relaxed);
    if f != -2 {
        return f;
    }
    let v = real_getenv(c"KILN_TRACE_FD".as_ptr());
    let n = if v.is_null() { -1 } else { unsafe { CStr::from_ptr(v) }.to_str().ok().and_then(|s| s.parse().ok()).unwrap_or(-1) };
    FD.store(n, Ordering::Relaxed);
    n
}

/// Appends `bytes` to `buf` past `len`, as much as fits.
fn put(buf: &mut [u8], len: &mut usize, bytes: &[u8]) {
    for &c in bytes {
        if let Some(slot) = buf.get_mut(*len) {
            *slot = c;
            *len += 1;
        }
    }
}

/// `n` in decimal, into `out`: the digits' slice.
fn decimal(mut n: u32, out: &mut [u8; 10]) -> &[u8] {
    let mut at = out.len();
    loop {
        at -= 1;
        if let Some(d) = out.get_mut(at) {
            *d = b'0' + (n % 10) as u8;
        }
        n /= 10;
        if n == 0 || at == 0 {
            break;
        }
    }
    out.get(at..).unwrap_or_default()
}

/// Notes `call` on `what` (a path, or a variable's name for `getenv`).
fn note(call: &str, what: *const c_char) {
    let fd = fd();
    if fd < 0 || what.is_null() {
        return;
    }
    let _ = BUSY.try_with(|busy| {
        if busy.get() {
            return;
        }
        busy.set(true);
        let mut buf = [0u8; 4096];
        let mut len = 0usize;
        let mut digits = [0u8; 10];
        put(&mut buf, &mut len, decimal(unsafe { libc::getpid() }.unsigned_abs(), &mut digits));
        put(&mut buf, &mut len, b"\t");
        put(&mut buf, &mut len, call.as_bytes());
        put(&mut buf, &mut len, b"\t");
        let given = unsafe { CStr::from_ptr(what) }.to_bytes();
        if call != "getenv" && given.first() != Some(&b'/') {
            let mut cwd = [0u8; 2048];
            if !unsafe { libc::getcwd(cwd.as_mut_ptr().cast(), cwd.len()) }.is_null() {
                put(&mut buf, &mut len, unsafe { CStr::from_ptr(cwd.as_ptr().cast()) }.to_bytes());
                put(&mut buf, &mut len, b"/");
            }
        }
        put(&mut buf, &mut len, given);
        put(&mut buf, &mut len, b"\n");
        unsafe { libc::write(fd, buf.as_ptr().cast(), len) };
        busy.set(false);
    });
}

/// A call the C library couldn't make: its own isn't there.
fn missing<T>(fail: T) -> T {
    unsafe { *libc::__errno_location() = libc::ENOSYS };
    fail
}

macro_rules! wrap {
    ($slot:ident, $name:ident, $cname:literal, $path:ident, ($($arg:ident: $ty:ty),*) -> $ret:ty, $fail:expr) => {
        static $slot: AtomicUsize = AtomicUsize::new(0);

        /// # Safety
        /// As the C library's own.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) -> $ret {
            note(stringify!($name), $path);
            match real(&$slot, $cname) {
                0 => missing($fail),
                p => unsafe { std::mem::transmute::<usize, unsafe extern "C" fn($($ty),*) -> $ret>(p)($($arg),*) },
            }
        }
    };
}

wrap!(OPEN, open, c"open", path, (path: *const c_char, flags: c_int, mode: mode_t) -> c_int, -1);
wrap!(OPEN64, open64, c"open64", path, (path: *const c_char, flags: c_int, mode: mode_t) -> c_int, -1);
wrap!(OPEN_2, __open_2, c"__open_2", path, (path: *const c_char, flags: c_int) -> c_int, -1);
wrap!(OPEN64_2, __open64_2, c"__open64_2", path, (path: *const c_char, flags: c_int) -> c_int, -1);
wrap!(OPENAT, openat, c"openat", path, (dir: c_int, path: *const c_char, flags: c_int, mode: mode_t) -> c_int, -1);
wrap!(OPENAT64, openat64, c"openat64", path, (dir: c_int, path: *const c_char, flags: c_int, mode: mode_t) -> c_int, -1);
wrap!(FOPEN, fopen, c"fopen", path, (path: *const c_char, mode: *const c_char) -> *mut libc::FILE, std::ptr::null_mut());
wrap!(FOPEN64, fopen64, c"fopen64", path, (path: *const c_char, mode: *const c_char) -> *mut libc::FILE, std::ptr::null_mut());
wrap!(STAT, stat, c"stat", path, (path: *const c_char, buf: *mut c_void) -> c_int, -1);
wrap!(STAT64, stat64, c"stat64", path, (path: *const c_char, buf: *mut c_void) -> c_int, -1);
wrap!(LSTAT, lstat, c"lstat", path, (path: *const c_char, buf: *mut c_void) -> c_int, -1);
wrap!(LSTAT64, lstat64, c"lstat64", path, (path: *const c_char, buf: *mut c_void) -> c_int, -1);
wrap!(ACCESS, access, c"access", path, (path: *const c_char, mode: c_int) -> c_int, -1);
wrap!(OPENDIR, opendir, c"opendir", path, (path: *const c_char) -> *mut libc::DIR, std::ptr::null_mut());
wrap!(EXECVE, execve, c"execve", path, (path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> c_int, -1);

/// # Safety
/// As the C library's own.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    note("getenv", name);
    real_getenv(name)
}
