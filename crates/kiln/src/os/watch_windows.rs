//! Folder changes on Windows (goal 9): `FindFirstChangeNotificationW` handles a thread waits on
//! (`WaitForMultipleObjects`, no timeout), with an event of its own that ends it when its watch
//! goes. Windows says no names, so every change under a watched folder counts, and the waiter's
//! reading again finds what's new or nothing. A watch reaching every folder below watches its
//! folders' trees whole; one reaching a few watches each of those folders alone, and looks at them
//! again after each change (a folder made or gone); a folder not there yet is watched from its
//! nearest folder that is. Unchecked here: no Windows machine in this lane.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use super::{Depth, Watched};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn FindFirstChangeNotificationW(path: *const u16, subtree: i32, filter: u32) -> *mut c_void;
    fn FindNextChangeNotification(handle: *mut c_void) -> i32;
    fn FindCloseChangeNotification(handle: *mut c_void) -> i32;
    fn WaitForMultipleObjects(count: u32, handles: *const *mut c_void, all: i32, ms: u32) -> u32;
    fn CreateEventW(attributes: *mut c_void, manual: i32, initial: i32, name: *const u16) -> *mut c_void;
    fn SetEvent(handle: *mut c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// `FILE_NOTIFY_CHANGE_FILE_NAME | DIR_NAME | ATTRIBUTES | LAST_WRITE`.
const FILTER: u32 = 0x1 | 0x2 | 0x4 | 0x10;
const INFINITE: u32 = u32::MAX;
const WAIT_OBJECT_0: u32 = 0;
/// `MAXIMUM_WAIT_OBJECTS`, less the thread's own end.
const MOST: usize = 63;

/// The ends of each watch's threads, by its id (each an event's handle, as an address).
fn ends() -> &'static Mutex<HashMap<u64, Vec<usize>>> {
    static E: OnceLock<Mutex<HashMap<u64, Vec<usize>>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(HashMap::new()))
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// The folders to wait on now, each with whether its tree is: those `dirs` name as deep as
/// `depth` reaches, a missing one's nearest folder that's there in its stead.
fn folders(dirs: &[PathBuf], depth: Depth) -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = Vec::new();
    for d in dirs {
        match super::nearest(d) {
            Some((at, 0)) if depth == Depth::All => out.push((at, true)),
            Some((at, 0)) => below(&at, 0, depth, &mut out),
            Some((at, _)) => out.push((at, false)),
            None => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

fn below(dir: &Path, n: u32, depth: Depth, out: &mut Vec<(PathBuf, bool)>) {
    out.push((dir.to_path_buf(), false));
    if !depth.reaches(n.saturating_add(1)) {
        return;
    }
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            below(&e.path(), n.saturating_add(1), depth, out);
        }
    }
}

pub(super) fn start(w: &Arc<Watched>) {
    let all = folders(w.dirs(), w.depth());
    // Every folder there, each watched alone or as a whole tree: what's waited on stays as it is
    // (in threads of 63). Else (a few folders deep, or one not there yet) one thread looks at the
    // folders again after each change, as one made or gone may move them.
    let fixed = matches!(w.depth(), Depth::All | Depth::Below(0)) && w.dirs().iter().all(|d| d.is_dir());
    let chunks: Vec<Vec<(PathBuf, bool)>> = if fixed { all.chunks(MOST).map(<[_]>::to_vec).collect() } else { vec![Vec::new()] };
    for chunk in chunks {
        // SAFETY: an unnamed, auto-reset event, not signalled.
        let end = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null()) };
        if end.is_null() {
            tracing::warn!("a folder watch's thread couldn't be made: what waits on it waits on its other events");
            continue;
        }
        ends().lock().entry(w.id).or_default().push(end as usize);
        let (weak, end) = (Arc::downgrade(w), end as usize);
        let spawned = std::thread::Builder::new().name("kiln-watch".into()).spawn(move || run(&weak, end, if fixed { Some(chunk) } else { None }));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "a folder watch's thread couldn't start: what waits on it waits on its other events");
        }
    }
}

/// Waits on `fixed`'s folders, or (none) the watch's as they stand at each turn, until `end`. Only
/// `end` signalled ends it, and only then is `end` closed: `forget` signals it once, after which
/// nothing else holds it.
fn run(w: &Weak<Watched>, end: usize, fixed: Option<Vec<(PathBuf, bool)>>) {
    loop {
        let folders = match &fixed {
            Some(f) => f.clone(),
            None => w.upgrade().map(|w| folders(w.dirs(), w.depth())).unwrap_or_default().into_iter().take(MOST).collect(),
        };
        let mut handles: Vec<*mut c_void> = Vec::new();
        for (dir, tree) in &folders {
            let path = wide(dir);
            // SAFETY: a NUL-terminated path; the handle is closed below.
            let h = unsafe { FindFirstChangeNotificationW(path.as_ptr(), i32::from(*tree), FILTER) };
            if !h.is_null() && h as isize != -1 {
                handles.push(h);
            }
        }
        handles.push(end as *mut c_void);
        // Some(true): `end` came; Some(false): look at the folders again; none: the wait failed.
        let outcome = loop {
            // SAFETY: live handles, as many as said; no timeout.
            let r = unsafe { WaitForMultipleObjects(u32::try_from(handles.len()).unwrap_or(0), handles.as_ptr(), 0, INFINITE) };
            let Some(i) = r.checked_sub(WAIT_OBJECT_0).and_then(|i| usize::try_from(i).ok()).filter(|i| *i < handles.len()) else { break None };
            if i.saturating_add(1) == handles.len() {
                break Some(true);
            }
            if let Some(h) = handles.get(i) {
                // SAFETY: a change handle of this wait's, waited on again.
                unsafe { FindNextChangeNotification(*h) };
            }
            if let Some(w) = w.upgrade() {
                w.tell();
            }
            if fixed.is_none() {
                break Some(false);
            }
        };
        for h in handles.iter().take(handles.len().saturating_sub(1)) {
            // SAFETY: change handles this turn opened, closed once.
            unsafe { FindCloseChangeNotification(*h) };
        }
        match outcome {
            Some(true) => {
                // SAFETY: this thread's end, signalled by `forget`, which holds it no more.
                unsafe { CloseHandle(end as *mut c_void) };
                return;
            }
            Some(false) => {}
            None => {
                tracing::warn!("a folder watch's wait failed: what waits on it waits on its other events");
                return;
            }
        }
    }
}

pub(super) fn forget(id: u64) {
    for end in ends().lock().remove(&id).unwrap_or_default() {
        // SAFETY: an event `start` made, closed by its thread as it ends.
        unsafe { SetEvent(end as *mut c_void) };
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn RegisterWaitForSingleObject(wait: *mut *mut c_void, object: *mut c_void, callback: unsafe extern "system" fn(*mut c_void, u8), context: *mut c_void, ms: u32, flags: u32) -> i32;
    fn UnregisterWaitEx(wait: *mut c_void, completion: *mut c_void) -> i32;
}

/// `SYNCHRONIZE`: enough to wait on a process.
const SYNCHRONIZE: u32 = 0x0010_0000;
/// `WT_EXECUTEONLYONCE`.
const ONCE: u32 = 0x8;
/// `INVALID_HANDLE_VALUE` as `UnregisterWaitEx`'s completion: it waits for a callback under way.
const WAIT_FOR_CALLBACKS: isize = -1;

unsafe extern "system" fn ended(context: *mut c_void, _timed_out: u8) {
    // SAFETY: the context is a `Notify` the waiting future keeps alive until its waits are
    // unregistered, callbacks under way included.
    let n = unsafe { &*context.cast_const().cast::<crate::rt::Notify>() };
    n.notify();
}

/// Each process handle and its wait, closed as the future ends.
struct Waits(Vec<(usize, usize)>);

impl Drop for Waits {
    fn drop(&mut self) {
        for (wait, process) in &self.0 {
            // SAFETY: a wait registered below, unregistered once, its callbacks waited for; then
            // the process handle it waited on, closed once.
            unsafe {
                UnregisterWaitEx(*wait as *mut c_void, WAIT_FOR_CALLBACKS as *mut c_void);
                CloseHandle(*process as *mut c_void);
            }
        }
    }
}

/// Until one of `pids` exits (goal 9): a thread-pool wait on each one's handle tells a notifier;
/// one already gone (no handle) at once; with none to watch, never.
pub(super) async fn exited(pids: &[u32]) {
    let notify = Box::new(crate::rt::Notify::new());
    let mut waits = Waits(Vec::new());
    for pid in pids {
        // SAFETY: an access mask and a pid; a handle or null.
        let process = unsafe { OpenProcess(SYNCHRONIZE, 0, *pid) };
        if process.is_null() {
            return;
        }
        let mut wait: *mut c_void = std::ptr::null_mut();
        let context = std::ptr::from_ref::<crate::rt::Notify>(&*notify).cast_mut().cast::<c_void>();
        // SAFETY: a live process handle; a callback and context that outlive the wait (`Waits`
        // unregisters it, waiting, before `notify` goes).
        if unsafe { RegisterWaitForSingleObject(&raw mut wait, process, ended, context, INFINITE, ONCE) } == 0 {
            // SAFETY: the handle just opened, closed once.
            unsafe { CloseHandle(process) };
            continue;
        }
        waits.0.push((wait as usize, process as usize));
    }
    if waits.0.is_empty() {
        return std::future::pending().await;
    }
    notify.notified().await;
    drop(waits);
}
