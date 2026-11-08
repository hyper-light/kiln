//! Folder changes on macOS (goal 9): an FSEvents stream for each [`Watched`], file-level events
//! (`kFSEventStreamCreateFlagFileEvents`) delivered on one serial dispatch queue as they come
//! (latency 0, `NoDefer`), never on a clock. FSEvents goes by path and watches everything below a
//! path, so a watch's depth and names are kept here, against its folders as the OS names them
//! (links resolved: `/tmp` is `/private/tmp`); a folder not there yet is watched by its path,
//! with its path's changes asked for too (`WatchRoot`), so its making is heard. Events the OS
//! dropped or coalesced (it must scan below, the root changed) count as a change.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use super::Watched;
use crate::os::sense::cf;

type StreamRef = *mut c_void;

#[repr(C)]
struct Context {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> *const c_void>,
}

#[repr(C)]
struct ArrayCallBacks {
    version: isize,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

type Callback = extern "C" fn(stream: *const c_void, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, ids: *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(alloc: *const c_void, callback: Callback, context: *mut Context, paths: *const c_void, since: u64, latency: f64, flags: u32) -> StreamRef;
    fn FSEventStreamSetDispatchQueue(stream: StreamRef, queue: *mut c_void);
    fn FSEventStreamStart(stream: StreamRef) -> u8;
    fn FSEventStreamStop(stream: StreamRef);
    fn FSEventStreamInvalidate(stream: StreamRef);
    fn FSEventStreamRelease(stream: StreamRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: ArrayCallBacks;
    fn CFArrayCreate(alloc: *const c_void, values: *const *const c_void, n: isize, callbacks: *const ArrayCallBacks) -> *const c_void;
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
}

/// `kFSEventStreamEventIdSinceNow`.
const SINCE_NOW: u64 = u64::MAX;
/// `kFSEventStreamCreateFlagNoDefer | kFSEventStreamCreateFlagWatchRoot | kFSEventStreamCreateFlagFileEvents`.
const FLAGS: u32 = 0x02 | 0x04 | 0x10;
/// `kFSEventStreamEventFlagMustScanSubDirs | UserDropped | KernelDropped | RootChanged`: what the OS
/// couldn't say file by file.
const LOST: u32 = 0x01 | 0x02 | 0x04 | 0x20;

/// What a stream's callback reads: its watch, and its folders as the OS names them.
struct Info {
    watched: Weak<Watched>,
    roots: Vec<PathBuf>,
}

extern "C" fn retain(info: *const c_void) -> *const c_void {
    // SAFETY: `info` is an `Arc<Info>`'s pointer (`Arc::into_raw`), alive while counted.
    unsafe { Arc::increment_strong_count(info.cast::<Info>()) };
    info
}

extern "C" fn release(info: *const c_void) {
    // SAFETY: as `retain`'s: one count taken there (or by `start`) given back.
    unsafe { Arc::decrement_strong_count(info.cast::<Info>()) };
}

extern "C" fn heard(_stream: *const c_void, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, _ids: *const u64) {
    if info.is_null() || paths.is_null() || flags.is_null() {
        return;
    }
    // SAFETY: the stream's info, kept by its retain while it can call this.
    let info = unsafe { &*info.cast_const().cast::<Info>() };
    let Some(w) = info.watched.upgrade() else { return };
    // SAFETY: FSEvents passes `n` C strings (no `UseCFTypes`) and `n` flags.
    let (paths, flags) = unsafe { (std::slice::from_raw_parts(paths.cast_const().cast::<*const c_char>(), n), std::slice::from_raw_parts(flags, n)) };
    let kept = paths.iter().zip(flags).any(|(p, f)| {
        if f & LOST != 0 {
            return true;
        }
        if p.is_null() {
            return false;
        }
        // SAFETY: a NUL-terminated path FSEvents owns for the call.
        let path = Path::new(std::ffi::OsStr::from_bytes(unsafe { CStr::from_ptr(*p) }.to_bytes()));
        w.covers_from(&info.roots, path)
    });
    if kept {
        w.tell();
    }
}

/// `dir` as the OS names it: its nearest folder that's there, links resolved, and the rest.
fn real(dir: &Path) -> PathBuf {
    let Some((at, _)) = super::nearest(dir) else { return dir.to_path_buf() };
    let rest = dir.strip_prefix(&at).map(Path::to_path_buf).unwrap_or_default();
    at.canonicalize().map_or_else(|_| dir.to_path_buf(), |c| c.join(rest))
}

/// The streams running, by their watch's id (each as an address: a stream is the OS's object).
fn streams() -> &'static Mutex<HashMap<u64, usize>> {
    static S: OnceLock<Mutex<HashMap<u64, usize>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn queue() -> usize {
    static Q: OnceLock<usize> = OnceLock::new();
    // SAFETY: a label that lives for good; a serial queue (no attributes). Never released: the
    // process's for its life.
    *Q.get_or_init(|| unsafe { dispatch_queue_create(c"kiln-watch".as_ptr(), std::ptr::null()) } as usize)
}

pub(super) fn start(w: &Arc<Watched>) {
    let roots: Vec<PathBuf> = w.dirs().iter().map(|d| real(d.as_path())).collect();
    if roots.is_empty() {
        return;
    }
    let strings: Vec<cf::Owned> = roots.iter().filter_map(|r| CString::new(r.clone().into_os_string().into_vec()).ok()).map(|c| cf::string(&c)).filter(|s| !s.0.is_null()).collect();
    let values: Vec<*const c_void> = strings.iter().map(|s| s.0).collect();
    let info = Arc::into_raw(Arc::new(Info { watched: Arc::downgrade(w), roots }));
    let mut context = Context { version: 0, info: info.cast_mut().cast(), retain: Some(retain), release: Some(release), copy_description: None };
    // SAFETY: CF strings this holds; the type callbacks CF exports; the array is released below.
    let array = cf::Owned(unsafe { CFArrayCreate(std::ptr::null(), values.as_ptr(), isize::try_from(values.len()).unwrap_or(0), &raw const kCFTypeArrayCallBacks) });
    let q = queue();
    // SAFETY: a context copied by the call (its info retained through `retain`); a live array.
    let stream = if array.0.is_null() || q == 0 { std::ptr::null_mut() } else { unsafe { FSEventStreamCreate(std::ptr::null(), heard, &raw mut context, array.0, SINCE_NOW, 0.0, FLAGS) } };
    // The count `into_raw` took: the stream holds its own now, or nothing does.
    release(info.cast());
    if stream.is_null() {
        tracing::warn!(dirs = ?w.dirs(), "FSEvents wouldn't watch these folders: what waits on them waits on its other events");
        return;
    }
    // SAFETY: a stream just made, a queue that lives for good.
    let started = unsafe {
        FSEventStreamSetDispatchQueue(stream, q as *mut c_void);
        FSEventStreamStart(stream)
    };
    if started == 0 {
        // SAFETY: the stream made above, never started: invalidated and released once.
        unsafe {
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);
        }
        tracing::warn!(dirs = ?w.dirs(), "FSEvents wouldn't start watching these folders: what waits on them waits on its other events");
        return;
    }
    streams().lock().insert(w.id, stream as usize);
}

pub(super) fn forget(id: u64) {
    let Some(stream) = streams().lock().remove(&id) else { return };
    let stream = stream as StreamRef;
    // SAFETY: a stream `start` started and only this removes: stopped, invalidated and released once.
    unsafe {
        FSEventStreamStop(stream);
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
    }
}

/// Until one of `pids` exits (goal 9): a kqueue of their `EVFILT_PROC`/`NOTE_EXIT`, readable once
/// one has, as hyper-rt's readiness says; one already gone at once; with none to watch, never.
pub(super) async fn exited(pids: &[u32]) {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    // SAFETY: kqueue() takes nothing and returns a descriptor or -1, owned from here.
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return std::future::pending().await;
    }
    // SAFETY: a descriptor kqueue() just returned, owned by nothing else.
    let kq = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut watched = 0usize;
    for pid in pids {
        // SAFETY: an all-zero kevent is a valid value of the C struct.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = usize::try_from(*pid).unwrap_or(0);
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT;
        // SAFETY: one change, no events asked back, no timeout.
        let r = unsafe { libc::kevent(kq.as_raw_fd(), &raw const ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if r < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return;
            }
            continue;
        }
        watched += 1;
    }
    if watched == 0 {
        return std::future::pending().await;
    }
    loop {
        crate::os::proc::wait_readable(&kq).await;
        // Readiness said so; the queue says whether it's so (a refused wait returns early).
        // SAFETY: an all-zero kevent is a valid value of the C struct.
        let mut out: libc::kevent = unsafe { std::mem::zeroed() };
        let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: no changes; room for one event; no wait.
        if unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &raw mut out, 1, &raw const zero) } > 0 {
            return;
        }
    }
}
