//! Waits that end at their deadlines (AC-T7.4.1; docs/design/deadline-waits.md): a condition variable, and the mutex
//! it waits with, whose timed waits return by their deadline on every platform Hex runs on.
//!
//! On Linux and Windows they are std's. On macOS std's condition variable waits in
//! `pthread_cond_timedwait_relative_np`, which XNU times with the leeway of the waiting thread's latency QoS tier:
//! at utility QoS (the kiln runner's commands), up to four times the wait and 75 ms late, twice that while the
//! machine's user is idle. There a waiter waits on a kqueue of its own instead, woken by its user event, its
//! deadline a one-shot timer marked `NOTE_CRITICAL`, which XNU fires without leeway.
//!
//! The API is std's but for a wait's failure, [`Failed`]: std's has only poison, and macOS's waiter needs a kqueue
//! the OS may refuse. Beside them, a channel whose timed receive waits on that condition variable ([`mpsc`]), and a
//! timer to a deadline ([`rest_until`]): on macOS std's own (`mpsc`'s receive and `park_timeout`, dispatch semaphores;
//! `sleep`, a Mach semaphore) take the same leeway.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::fmt;
use std::io;
#[cfg(not(target_os = "macos"))]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
pub mod mpsc;

#[cfg(target_os = "macos")]
pub use macos::{Condvar, Mutex, MutexGuard, rest_until};
#[cfg(not(target_os = "macos"))]
pub use std::sync::{Mutex, MutexGuard};

/// Why a wait couldn't wait.
#[derive(Debug)]
pub enum Failed {
    /// Its mutex was poisoned: a thread panicked holding it.
    Poisoned,
    /// The OS refused the waiter what it waits on: on macOS its kqueue (kqueue(2), kevent(2)); never elsewhere.
    Os(io::Error),
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned => {
                f.write_str("the lock it waits with was poisoned by a thread that panicked")
            }
            Self::Os(why) => write!(f, "the OS refused the waiter its kqueue: {why}"),
        }
    }
}

impl std::error::Error for Failed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Poisoned => None,
            Self::Os(why) => Some(why),
        }
    }
}

/// For the waits whose callers speak `io::Error`: the OS's refusal as it was, a poisoned lock as `Other`.
impl From<Failed> for io::Error {
    fn from(why: Failed) -> Self {
        match why {
            Failed::Os(why) => why,
            Failed::Poisoned => io::Error::other(Failed::Poisoned),
        }
    }
}

/// Whether a timed wait ended at its deadline rather than by a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitTimeoutResult(pub(crate) bool);

impl WaitTimeoutResult {
    /// It ended at its deadline, no notification having come.
    pub fn timed_out(&self) -> bool {
        self.0
    }
}

/// std's condition variable, its failures [`Failed`]. A wait may wake spuriously, as std's may.
#[cfg(not(target_os = "macos"))]
#[derive(Debug, Default)]
pub struct Condvar(std::sync::Condvar);

#[cfg(not(target_os = "macos"))]
impl Condvar {
    pub const fn new() -> Self {
        Self(std::sync::Condvar::new())
    }

    /// Lets `guard` go until notified, and takes it again.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> Result<MutexGuard<'a, T>, Failed> {
        self.0.wait(guard).map_err(|_| Failed::Poisoned)
    }

    /// Waits, as [`Condvar::wait`] does, while `condition` holds.
    pub fn wait_while<'a, T, F>(
        &self,
        guard: MutexGuard<'a, T>,
        condition: F,
    ) -> Result<MutexGuard<'a, T>, Failed>
    where
        F: FnMut(&mut T) -> bool,
    {
        self.0
            .wait_while(guard, condition)
            .map_err(|_| Failed::Poisoned)
    }

    /// Lets `guard` go until notified or `dur` has passed, and takes it again.
    pub fn wait_timeout<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        dur: Duration,
    ) -> Result<(MutexGuard<'a, T>, WaitTimeoutResult), Failed> {
        let (guard, timed) = self
            .0
            .wait_timeout(guard, dur)
            .map_err(|_| Failed::Poisoned)?;
        Ok((guard, WaitTimeoutResult(timed.timed_out())))
    }

    /// Waits, as [`Condvar::wait_timeout`] does, while `condition` holds, until `dur` has passed.
    pub fn wait_timeout_while<'a, T, F>(
        &self,
        guard: MutexGuard<'a, T>,
        dur: Duration,
        condition: F,
    ) -> Result<(MutexGuard<'a, T>, WaitTimeoutResult), Failed>
    where
        F: FnMut(&mut T) -> bool,
    {
        let (guard, timed) = self
            .0
            .wait_timeout_while(guard, dur, condition)
            .map_err(|_| Failed::Poisoned)?;
        Ok((guard, WaitTimeoutResult(timed.timed_out())))
    }

    pub fn notify_one(&self) {
        self.0.notify_one();
    }

    pub fn notify_all(&self) {
        self.0.notify_all();
    }
}

/// Waits until `deadline`: std's `park_timeout`, its wake by another thread's unpark taken as early, so the wait goes
/// on until the deadline. Never refused here.
#[cfg(not(target_os = "macos"))]
pub fn rest_until(deadline: Instant) -> Result<(), Failed> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::park_timeout(left);
    }
}

#[cfg(test)]
mod tests;
