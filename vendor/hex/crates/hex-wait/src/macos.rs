//! macOS's condition variable (docs/design/deadline-waits.md): its waiters in a queue, each blocked on a kqueue of its
//! own ([`sys::Kqueue`]) until a notification takes it from the queue and triggers its user event, or its deadline's
//! timer fires. The queue is the one record of who waits: a waiter gone from it when it leaves was chosen by a
//! notification, one still in it was not. And the mutex it waits with: std's, whose guard knows it, so a wait lets
//! it go and takes it again.

mod sys;

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, LockResult, PoisonError};
use std::time::{Duration, Instant};

use crate::{Failed, WaitTimeoutResult};
use sys::{Kqueue, Woke};

/// std's mutex, whose guard knows it ([`MutexGuard`]), so a [`Condvar`] wait lets it go and takes it again.
#[derive(Default)]
pub struct Mutex<T> {
    inner: std::sync::Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: std::sync::Mutex::new(value),
        }
    }

    /// Takes the lock, as std's `Mutex::lock` does.
    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        match self.inner.lock() {
            Ok(inner) => Ok(MutexGuard { lock: self, inner }),
            Err(poisoned) => Err(PoisonError::new(MutexGuard {
                lock: self,
                inner: poisoned.into_inner(),
            })),
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// A [`Mutex`]'s guard: std's, and the mutex it guards.
pub struct MutexGuard<'a, T> {
    lock: &'a Mutex<T>,
    inner: std::sync::MutexGuard<'a, T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// A condition variable whose timed waits return by their deadline (the crate's doc). A wait wakes only when a
/// notification chooses it or its deadline passes, never spuriously.
#[derive(Default)]
pub struct Condvar {
    /// Who waits, oldest first, a notification taking from the front: at most one entry for each thread in one of
    /// its waits, a thread waiting in one at a time.
    waiting: std::sync::Mutex<VecDeque<Arc<Kqueue>>>,
}

impl fmt::Debug for Condvar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Condvar").finish_non_exhaustive()
    }
}

impl Condvar {
    pub const fn new() -> Self {
        Self {
            waiting: std::sync::Mutex::new(VecDeque::new()),
        }
    }

    /// Lets `guard` go until notified, and takes it again.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> Result<MutexGuard<'a, T>, Failed> {
        self.block(guard, None).map(|(guard, _)| guard)
    }

    /// Waits, as [`Condvar::wait`] does, while `condition` holds.
    pub fn wait_while<'a, T, F>(
        &self,
        mut guard: MutexGuard<'a, T>,
        mut condition: F,
    ) -> Result<MutexGuard<'a, T>, Failed>
    where
        F: FnMut(&mut T) -> bool,
    {
        while condition(&mut *guard) {
            guard = self.wait(guard)?;
        }
        Ok(guard)
    }

    /// Lets `guard` go until notified or `dur` has passed, and takes it again. A `dur` no deadline can hold (the
    /// clock can't add it to now) bounds nothing: the wait is for a notification alone, as std's, which clamps it
    /// to centuries, is.
    pub fn wait_timeout<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        dur: Duration,
    ) -> Result<(MutexGuard<'a, T>, WaitTimeoutResult), Failed> {
        self.block(guard, Instant::now().checked_add(dur))
    }

    /// Waits, as [`Condvar::wait_timeout`] does, while `condition` holds, until `dur` has passed: timed out if it
    /// still holds then.
    pub fn wait_timeout_while<'a, T, F>(
        &self,
        mut guard: MutexGuard<'a, T>,
        dur: Duration,
        mut condition: F,
    ) -> Result<(MutexGuard<'a, T>, WaitTimeoutResult), Failed>
    where
        F: FnMut(&mut T) -> bool,
    {
        let deadline = Instant::now().checked_add(dur);
        loop {
            if !condition(&mut *guard) {
                return Ok((guard, WaitTimeoutResult(false)));
            }
            if deadline.is_some_and(|at| Instant::now() >= at) {
                return Ok((guard, WaitTimeoutResult(true)));
            }
            guard = self.block(guard, deadline)?.0;
        }
    }

    /// Wakes the oldest waiter, if one waits.
    pub fn notify_one(&self) {
        let chosen = self.queue().pop_front();
        if let Some(waiter) = chosen {
            wake(&waiter);
        }
    }

    /// Wakes every waiter.
    pub fn notify_all(&self) {
        let chosen = std::mem::take(&mut *self.queue());
        for waiter in &chosen {
            wake(waiter);
        }
    }

    /// The queue. Its lock guards no invariant a panic could break (each hold is one `VecDeque` operation, none of
    /// which panics: a push has its room reserved first), so a poisoned one is taken as it is rather than lose a
    /// notification.
    fn queue(&self) -> std::sync::MutexGuard<'_, VecDeque<Arc<Kqueue>>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lets `guard` go, waits until a notification chooses this waiter or `deadline` passes, and takes the lock
    /// again: timed out unless a notification chose it.
    fn block<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        deadline: Option<Instant>,
    ) -> Result<(MutexGuard<'a, T>, WaitTimeoutResult), Failed> {
        let lock = guard.lock;
        let waiter = Arc::new(Kqueue::new().map_err(Failed::Os)?);
        {
            // Queued while the guard is held, so a notification made once it is let go finds this waiter.
            let mut waiting = self.queue();
            waiting
                .try_reserve(1)
                .map_err(|e| Failed::Os(io::Error::new(io::ErrorKind::OutOfMemory, e)))?;
            waiting.push_back(Arc::clone(&waiter));
        }
        drop(guard);
        let slept = sleep(&waiter, deadline);
        let chosen = {
            let mut waiting = self.queue();
            match waiting.iter().position(|w| Arc::ptr_eq(w, &waiter)) {
                Some(at) => {
                    waiting.remove(at);
                    false
                }
                None => true,
            }
        };
        if let Err(why) = slept {
            if chosen {
                // The notification that chose a waiter that can't wait goes to the next one, not nowhere.
                self.notify_one();
            }
            return Err(Failed::Os(why));
        }
        let guard = lock.lock().map_err(|_| Failed::Poisoned)?;
        Ok((guard, WaitTimeoutResult(!chosen)))
    }
}

/// Waits until `deadline`, on a kqueue of its own, its one-shot `NOTE_CRITICAL` timer armed for what is left.
pub fn rest_until(deadline: Instant) -> Result<(), Failed> {
    let timer = Kqueue::new().map_err(Failed::Os)?;
    // Nothing triggers its user event: each return is its timer's or a signal's, and the clock says which.
    sleep(&timer, Some(deadline)).map_err(Failed::Os)
}

/// Wakes a waiter a notification took from the queue. Its trigger can't fail: it touches the user event the live
/// kqueue's making added, which kevent(2) does without allocating and with no EBADF, ENOENT or EINVAL possible, the
/// kqueue held open by the notification's `Arc` meanwhile.
fn wake(waiter: &Kqueue) {
    let _ = waiter.trigger();
}

/// Blocks on `waiter`'s kqueue until a notification triggers it or `deadline` passes.
fn sleep(waiter: &Kqueue, deadline: Option<Instant>) -> io::Result<()> {
    loop {
        let left = match deadline {
            None => None,
            Some(at) => match at.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => Some(left),
                _ => return Ok(()),
            },
        };
        match waiter.wait(left)? {
            Woke::User => return Ok(()),
            // Its timer or a signal: the clock says whether the deadline has passed.
            Woke::Timer | Woke::Interrupted => {}
        }
    }
}
