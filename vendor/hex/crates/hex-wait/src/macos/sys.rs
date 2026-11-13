//! A waiter's kqueue (kqueue(2), kevent(2)): its user event, which a notification triggers, and a one-shot timer to
//! its deadline marked `NOTE_CRITICAL`, which XNU arms as a critical user timer, the one kind it gives no leeway
//! whatever the waiting thread's QoS (docs/design/deadline-waits.md). rustix makes the kqueue; its `kevent` can't
//! mark a timer `NOTE_CRITICAL`, so libc's makes the calls. XNU makes every kqueue close-on-exec and close-on-fork.

// This file's `unsafe` is named in CLAUDE.md §12; every block below carries its SAFETY comment.
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Duration;

/// The user event's ident, and the timer's.
const USER: usize = 0;
const TIMER: usize = 1;

/// What ended a [`Kqueue::wait`].
pub(super) enum Woke {
    /// Its user event: a notification chose its waiter.
    User,
    /// Its timer: the deadline it was armed for has come.
    Timer,
    /// A signal, or no event: its waiter looks again.
    Interrupted,
}

/// One waiter's kqueue, its user event added (`EVFILT_USER`, cleared as it is read).
pub(super) struct Kqueue(OwnedFd);

impl Kqueue {
    pub(super) fn new() -> io::Result<Self> {
        let kq = Self(rustix::event::kqueue::kqueue()?);
        kq.kevent(
            &[event(
                USER,
                libc::EVFILT_USER,
                libc::EV_ADD | libc::EV_CLEAR,
                0,
                0,
            )],
            &mut [],
        )?;
        Ok(kq)
    }

    /// Wakes its waiter.
    pub(super) fn trigger(&self) -> io::Result<()> {
        self.kevent(
            &[event(USER, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0)],
            &mut [],
        )
        .map(drop)
    }

    /// Blocks until its user event is triggered or, given `left` (never zero), a one-shot `NOTE_CRITICAL` timer of
    /// `left` fires.
    pub(super) fn wait(&self, left: Option<Duration>) -> io::Result<Woke> {
        let armed;
        let changes: &[libc::kevent] = match left {
            Some(left) => {
                // Nanoseconds, isize::MAX of them at most (292 years).
                let ns = isize::try_from(left.as_nanos()).unwrap_or(isize::MAX);
                armed = [event(
                    TIMER,
                    libc::EVFILT_TIMER,
                    libc::EV_ADD | libc::EV_ONESHOT,
                    libc::NOTE_NSECONDS | libc::NOTE_CRITICAL,
                    ns,
                )];
                &armed
            }
            None => &[],
        };
        let mut out = [event(0, 0, 0, 0, 0)];
        let n = match self.kevent(changes, &mut out) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(Woke::Interrupted),
            other => other?,
        };
        let Some(got) = out.first().filter(|_| n > 0) else {
            return Ok(Woke::Interrupted);
        };
        if got.flags & libc::EV_ERROR != 0 {
            return Err(io::Error::from_raw_os_error(
                i32::try_from(got.data).unwrap_or(libc::EINVAL),
            ));
        }
        Ok(match got.filter {
            libc::EVFILT_USER => Woke::User,
            libc::EVFILT_TIMER => Woke::Timer,
            _ => Woke::Interrupted,
        })
    }

    /// kevent(2): applies `changes`, then waits with no timeout for at most `events.len()` events, or, with room for
    /// none, returns at once.
    fn kevent(&self, changes: &[libc::kevent], events: &mut [libc::kevent]) -> io::Result<usize> {
        let nchanges = libc::c_int::try_from(changes.len())
            .map_err(|_| io::Error::other("too many kevent changes"))?;
        let nevents = libc::c_int::try_from(events.len())
            .map_err(|_| io::Error::other("too many kevent events"))?;
        // SAFETY: kevent reads `nchanges` events from `changes` and writes at most `nevents` into `events`, both
        // slices alive for the call, on the kqueue this owns; the timeout is null, so none is read. No event names a
        // descriptor or carries a pointer: its ident is a number and its udata null.
        let n = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                changes.as_ptr(),
                nchanges,
                events.as_mut_ptr(),
                nevents,
                std::ptr::null(),
            )
        };
        usize::try_from(n).map_err(|_| io::Error::last_os_error())
    }
}

/// A kevent with no udata.
fn event(ident: usize, filter: i16, flags: u16, fflags: u32, data: isize) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data,
        udata: std::ptr::null_mut(),
    }
}
