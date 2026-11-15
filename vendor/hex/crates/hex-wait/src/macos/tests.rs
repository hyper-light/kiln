//! macOS's deadline wait as XNU holds it (AC-T7.4.1; docs/design/deadline-waits.md): what a wait arms, read back from
//! the kernel while it waits, so the mechanism its timeliness rests on is held rather than a clock. How late a wait
//! comes past its deadline on a loaded host is the host's, which the client's deadline tests record for the census and
//! hold to nothing (the waits audit, ROOT's ruling of 2026-11-13).
//!
//! The read-back is libproc's, as `lskq(1)` reads a kqueue (CLAUDE.md §12): `proc_pidinfo(PROC_PIDLISTFDS)` lists the
//! process's kqueues, `proc_pidfdinfo(PROC_PIDFDKQUEUE_EXTINFO)` reads each one's knotes, `proc_pidinfo`'s
//! `PROC_PIDTHREADID64INFO` whether the waiter has blocked, and `mach_timebase_info` turns a timer's Mach ticks into
//! nanoseconds.

#![allow(unsafe_code)]

use std::mem::size_of;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// XNU's flavor reading a kqueue's knotes back (`bsd/sys/proc_info_private.h`), each a [`KeventExtinfo`], its answer
/// how many the kqueue holds; the SDK ships neither.
const PROC_PIDFDKQUEUE_EXTINFO: libc::c_int = 9;
/// The most knotes XNU reads back from one kqueue (`PROC_PIDFDKQUEUE_KNOTES_MAX`, `bsd/sys/proc_info_private.h`).
const KNOTES_MAX: usize = 128 * 1024;
/// XNU's flavor reading a thread by its 64-bit id (`sys/proc_info.h`), which libc doesn't name.
const PROC_PIDTHREADID64INFO: libc::c_int = 15;

/// XNU's `struct kevent_qos_s` (`bsd/sys/event_private.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct KeventQos {
    _ident: u64,
    filter: i16,
    flags: u16,
    _qos: i32,
    _udata: u64,
    _fflags: u32,
    _xflags: u32,
    _data: i64,
    _ext: [u64; 4],
}

/// XNU's `struct kevent_extinfo` (`bsd/sys/event_private.h`): a knote as libproc reads it back, with the flags and data
/// it was registered with (`kn_sfflags`, and `kn_sdata`, a relative timer's interval in Mach ticks).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct KeventExtinfo {
    kev: KeventQos,
    sdata: u64,
    _status: i32,
    sfflags: i32,
    _reserved: [u64; 2],
}

/// A timer a kqueue of this process holds, as the kernel keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timer {
    flags: u16,
    fflags: u32,
    interval: Duration,
}

/// A deadline no read-back here outlasts: the longest a wait arms whole (`isize::MAX` nanoseconds, about 292 years,
/// [`super::sys::Kqueue::wait`]'s), so its waiter waits until the test lets it go.
fn far() -> Duration {
    Duration::from_nanos(u64::try_from(isize::MAX).unwrap())
}

/// A condition variable whose deadline wait the read-back looks at: hex-wait's, or std's, which hex-wait replaces on
/// macOS. Its state: whether its waiter has said it waits, and whether it has been let go.
trait Waits: Default + Send + Sync + 'static {
    /// Says it waits, under its lock, then waits until let go or `dur` has passed: whether it timed out.
    fn wait(&self, dur: Duration) -> bool;
    /// Whether its waiter has said it waits and let its lock go, which only its wait does.
    fn waiting(&self) -> bool;
    /// Lets its waiter go.
    fn release(&self);
}

#[derive(Default)]
struct Hex(super::Mutex<(bool, bool)>, super::Condvar);

impl Waits for Hex {
    fn wait(&self, dur: Duration) -> bool {
        let mut state = self.0.lock().unwrap();
        state.0 = true;
        let (_state, timed) = self
            .1
            .wait_timeout_while(state, dur, |state| !state.1)
            .unwrap();
        timed.timed_out()
    }

    fn waiting(&self) -> bool {
        self.0.lock().unwrap().0
    }

    fn release(&self) {
        self.0.lock().unwrap().1 = true;
        self.1.notify_all();
    }
}

#[derive(Default)]
struct Std(std::sync::Mutex<(bool, bool)>, std::sync::Condvar);

impl Waits for Std {
    fn wait(&self, dur: Duration) -> bool {
        let mut state = self.0.lock().unwrap();
        state.0 = true;
        let (_state, timed) = self
            .1
            .wait_timeout_while(state, dur, |state| !state.1)
            .unwrap();
        timed.timed_out()
    }

    fn waiting(&self) -> bool {
        self.0.lock().unwrap().0
    }

    fn release(&self) {
        self.0.lock().unwrap().1 = true;
        self.1.notify_all();
    }
}

/// This process's id as libproc takes it.
fn pid() -> libc::c_int {
    libc::c_int::try_from(std::process::id()).unwrap()
}

/// The calling thread's 64-bit id, as `PROC_PIDTHREADID64INFO` takes it.
fn thread_id() -> u64 {
    let mut id = 0u64;
    // SAFETY: with no thread named (0), pthread_threadid_np writes the calling thread's id to `id`, a valid u64.
    assert_eq!(unsafe { libc::pthread_threadid_np(0, &raw mut id) }, 0);
    id
}

/// Thread `id`'s run state (`TH_STATE_*`), or `None` once it has gone.
fn run_state(id: u64) -> Option<libc::c_int> {
    // SAFETY: proc_threadinfo is integers and a byte array, for which all zeros is a value.
    let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(size_of::<libc::proc_threadinfo>()).unwrap();
    // SAFETY: proc_pidinfo writes at most `size` bytes, one proc_threadinfo, into `info`, which holds that many; the
    // flavor reads this process's thread whose 64-bit id is `id`.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid(),
            PROC_PIDTHREADID64INFO,
            id,
            (&raw mut info).cast(),
            size,
        )
    };
    if wrote == size {
        Some(info.pth_run_state)
    } else {
        None
    }
}

/// Every timer the kqueues of this process hold, read back from the kernel now: each kqueue listed, then its knotes.
/// A descriptor closed or reused since its listing reads as no kqueue.
fn timers() -> Vec<Timer> {
    let pid = pid();
    // Its room first: XNU answers the size of every descriptor slot the process has, and twenty more.
    // SAFETY: with no buffer, proc_pidinfo writes nothing and answers the room the listing needs.
    let room =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let slots = usize::try_from(room).unwrap() / size_of::<libc::proc_fdinfo>();
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        slots
    ];
    let size = libc::c_int::try_from(slots * size_of::<libc::proc_fdinfo>()).unwrap();
    // SAFETY: proc_pidinfo writes at most `size` bytes, whole proc_fdinfo entries, into `fds`, which holds that many.
    let wrote =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size) };
    fds.truncate(usize::try_from(wrote).unwrap() / size_of::<libc::proc_fdinfo>());
    let kqueue = u32::try_from(libc::PROX_FDTYPE_KQUEUE).unwrap();
    let (numer, denom) = timebase();
    let mut timers = Vec::new();
    for fd in fds.iter().filter(|fd| fd.proc_fdtype == kqueue) {
        for knote in knotes(pid, fd.proc_fd) {
            if knote.kev.filter == libc::EVFILT_TIMER {
                let ns = u128::from(knote.sdata) * u128::from(numer) / u128::from(denom);
                timers.push(Timer {
                    flags: knote.kev.flags,
                    fflags: u32::from_ne_bytes(knote.sfflags.to_ne_bytes()),
                    interval: Duration::from_nanos(u64::try_from(ns).unwrap()),
                });
            }
        }
    }
    timers
}

/// Kqueue `fd`'s knotes, read back whole: read again with room for as many as it said it holds, up to XNU's most.
fn knotes(pid: libc::c_int, fd: libc::c_int) -> Vec<KeventExtinfo> {
    let mut room = 1;
    loop {
        let mut read = vec![KeventExtinfo::default(); room];
        let size = libc::c_int::try_from(room * size_of::<KeventExtinfo>()).unwrap();
        // SAFETY: proc_pidfdinfo writes at most `size` bytes, whole kevent_extinfo entries, into `read`, which holds
        // that many, laid out as XNU's; it answers how many knotes the kqueue holds, or below 0 for no kqueue.
        let held = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd,
                PROC_PIDFDKQUEUE_EXTINFO,
                read.as_mut_ptr().cast(),
                size,
            )
        };
        let Ok(held) = usize::try_from(held) else {
            return Vec::new();
        };
        if held <= room || room == KNOTES_MAX {
            read.truncate(held);
            return read;
        }
        room = held.min(KNOTES_MAX);
    }
}

/// Mach absolute time's units (mach_timebase_info(3)): a tick is `numer / denom` nanoseconds.
// libc marks mach_timebase_info deprecated in favour of the mach2 crate, which binds it identically.
#[allow(deprecated)]
fn timebase() -> (u32, u32) {
    let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: mach_timebase_info writes the two fields of the struct it is given, which outlives the call.
    assert_eq!(unsafe { libc::mach_timebase_info(&raw mut info) }, 0);
    assert!(info.numer > 0 && info.denom > 0);
    (info.numer, info.denom)
}

/// What `W`'s deadline wait of `dur` arms, read back from the kernel while it waits: every timer the process's
/// kqueues hold armed for what was left of `dur` (no more than it, and no less than it less the span from before the
/// wait to the read and a Mach tick of rounding). Read once its waiter has said it waits, let its lock go (which only
/// its wait does) and blocked, its one wait after that its wait's own; then it is let go, and expected not timed out.
fn armed_while_waiting<W: Waits>(dur: Duration) -> Vec<Timer> {
    let waits = Arc::new(W::default());
    let theirs = Arc::clone(&waits);
    let (told, thread) = mpsc::channel();
    let began = Instant::now();
    let waiter = std::thread::spawn(move || {
        told.send(thread_id()).unwrap();
        theirs.wait(dur)
    });
    let thread = thread.recv().unwrap();
    while !(waits.waiting() && run_state(thread) == Some(libc::TH_STATE_WAITING)) {
        if waiter.is_finished() {
            panic!("its waiter ended before it blocked: {:?}", waiter.join());
        }
        std::thread::yield_now();
    }
    let timers = timers();
    let span = began.elapsed();
    waits.release();
    assert!(!waiter.join().unwrap(), "let go, yet it timed out");
    let (numer, denom) = timebase();
    let tick = Duration::from_nanos(u64::from(numer.div_ceil(denom)));
    let least = dur.saturating_sub(span).saturating_sub(tick);
    timers
        .into_iter()
        .filter(|timer| timer.interval >= least && timer.interval <= dur)
        .collect()
}

/// AC-T7.4.1 (docs/design/deadline-waits.md): do wait on a condition variable with a deadline nobody reaches, and read
/// back from the kernel what its wait armed while it waits. Expect hex-wait's to hold one timer at its deadline,
/// one-shot, its interval in nanoseconds and marked `NOTE_CRITICAL`, which XNU fires with no leeway whatever the
/// waiting thread's QoS; and expect std's, whose waits XNU times with that leeway, to hold none, so the read-back
/// tells the two apart.
#[test]
fn ac_t7_4_1_a_deadline_wait_arms_a_critical_one_shot_timer_at_its_deadline() {
    let armed = armed_while_waiting::<Hex>(far());
    assert_eq!(armed.len(), 1, "one timer at its deadline: {armed:?}");
    let timer = armed[0];
    assert_eq!(
        timer.flags & libc::EV_ONESHOT,
        libc::EV_ONESHOT,
        "{timer:?}"
    );
    assert_eq!(
        timer.fflags,
        libc::NOTE_NSECONDS | libc::NOTE_CRITICAL,
        "{timer:?}"
    );
    let armed = armed_while_waiting::<Std>(far());
    assert!(armed.is_empty(), "std's wait armed {armed:?}");
}
