//! Hex's channel (AC-T7.4.1; docs/design/deadline-waits.md): bounded, a queue under hex-wait's [`Mutex`] whose
//! receiver waits on hex-wait's [`Condvar`], so a timed receive returns by its deadline on every platform. std's own
//! channel parks its receiver on a dispatch semaphore on macOS, which XNU times by the waiting thread's QoS as it
//! times std's condition variable. The API is std's `sync_channel`'s but for its failures: making a channel reserves
//! its room, which may be refused, and a receive's wait may fail as hex-wait's waits may ([`Failed`]).

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

pub use std::sync::mpsc::{TryRecvError, TrySendError};

use crate::{Condvar, Failed, Mutex, MutexGuard};

/// A channel holding at most `bound` messages its receiver hasn't taken, their room reserved now. A `bound` of 0 is
/// a rendezvous: a message goes only to a receiver waiting for it.
pub fn sync_channel<T>(bound: usize) -> Result<(SyncSender<T>, Receiver<T>), Failed> {
    let mut queue = VecDeque::new();
    queue
        .try_reserve_exact(bound.max(1))
        .map_err(|e| Failed::Os(io::Error::new(io::ErrorKind::OutOfMemory, e)))?;
    let chan = Arc::new(Chan {
        state: Mutex::new(State {
            queue,
            bound,
            senders: 1,
            receiving: true,
            waiting: false,
        }),
        came: Condvar::new(),
    });
    Ok((SyncSender(Arc::clone(&chan)), Receiver(chan)))
}

struct Chan<T> {
    state: Mutex<State<T>>,
    /// Told when a message comes or the last sender goes: what the receiver waits on.
    came: Condvar,
}

struct State<T> {
    /// What was sent and not yet taken: at most `bound`, or one for a rendezvous, within the room reserved.
    queue: VecDeque<T>,
    bound: usize,
    /// The senders, each holding the channel's `Arc`, whose count can't pass `isize::MAX`.
    senders: usize,
    /// The receiver is there.
    receiving: bool,
    /// The receiver waits for a message now: a rendezvous's room.
    waiting: bool,
}

impl<T> Chan<T> {
    /// Its state. The lock guards no invariant a panic could break (each hold is a few counter and `VecDeque`
    /// operations within the room reserved, none of which panics, and no message is dropped under it), so a poisoned
    /// one is taken as it is rather than lose a message.
    fn state(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A channel's sending half, as std's `SyncSender`.
pub struct SyncSender<T>(Arc<Chan<T>>);

impl<T> SyncSender<T> {
    /// Sends `message` if there is room for it now: refused, giving it back, when the channel is full (for a
    /// rendezvous, when the receiver isn't waiting) or the receiver has gone.
    pub fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let mut state = self.0.state();
        if !state.receiving {
            return Err(TrySendError::Disconnected(message));
        }
        let room = if state.bound == 0 {
            state.waiting && state.queue.is_empty()
        } else {
            state.queue.len() < state.bound
        };
        if !room {
            return Err(TrySendError::Full(message));
        }
        state.queue.push_back(message);
        drop(state);
        self.0.came.notify_one();
        Ok(())
    }
}

impl<T> Clone for SyncSender<T> {
    fn clone(&self) -> Self {
        let mut state = self.0.state();
        state.senders = state.senders.saturating_add(1);
        drop(state);
        Self(Arc::clone(&self.0))
    }
}

impl<T> Drop for SyncSender<T> {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.senders = state.senders.saturating_sub(1);
        let last = state.senders == 0;
        drop(state);
        if last {
            self.0.came.notify_all();
        }
    }
}

impl<T> fmt::Debug for SyncSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncSender").finish_non_exhaustive()
    }
}

/// A channel's receiving half, as std's `Receiver`.
pub struct Receiver<T>(Arc<Chan<T>>);

impl<T> Receiver<T> {
    /// The oldest message, if one has come.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut state = self.0.state();
        match state.queue.pop_front() {
            Some(message) => Ok(message),
            None if state.senders == 0 => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
    }

    /// The oldest message, waiting for one until every sender has gone.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.wait(None).map_err(|why| match why {
            RecvTimeoutError::Failed(why) => RecvError::Failed(why),
            // With no deadline, never Timeout.
            RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected => RecvError::Disconnected,
        })
    }

    /// The oldest message, waiting for one until `dur` has passed or every sender has gone. A `dur` no deadline can
    /// hold (the clock can't add it to now) bounds nothing: the wait is for a message alone, as std's is.
    pub fn recv_timeout(&self, dur: Duration) -> Result<T, RecvTimeoutError> {
        self.wait(Instant::now().checked_add(dur))
    }

    fn wait(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        let mut state = self.0.state();
        loop {
            state.waiting = false;
            if let Some(message) = state.queue.pop_front() {
                return Ok(message);
            }
            if state.senders == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let left = match deadline {
                None => None,
                Some(at) => match at.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => Some(left),
                    _ => return Err(RecvTimeoutError::Timeout),
                },
            };
            state.waiting = true;
            let waited = match left {
                None => self.0.came.wait(state),
                Some(left) => self
                    .0
                    .came
                    .wait_timeout(state, left)
                    .map(|(state, _)| state),
            };
            state = match waited {
                Ok(state) => state,
                Err(why) => {
                    self.0.state().waiting = false;
                    return Err(RecvTimeoutError::Failed(why));
                }
            };
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.receiving = false;
        state.waiting = false;
        let left = std::mem::take(&mut state.queue);
        drop(state);
        // Dropped once the lock is let go: a message's own drop may take locks, or close what it holds.
        drop(left);
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}

/// Why a receive with no deadline took no message.
#[derive(Debug)]
pub enum RecvError {
    /// Every sender has gone, and nothing is left to take.
    Disconnected,
    /// Its wait couldn't wait.
    Failed(Failed),
}

/// Why a receive with a deadline took no message.
#[derive(Debug)]
pub enum RecvTimeoutError {
    /// The deadline passed with nothing come.
    Timeout,
    /// Every sender has gone, and nothing is left to take.
    Disconnected,
    /// Its wait couldn't wait.
    Failed(Failed),
}

impl From<RecvError> for RecvTimeoutError {
    fn from(why: RecvError) -> Self {
        match why {
            RecvError::Disconnected => Self::Disconnected,
            RecvError::Failed(why) => Self::Failed(why),
        }
    }
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => f.write_str("every sender has gone"),
            Self::Failed(why) => why.fmt(f),
        }
    }
}

impl fmt::Display for RecvTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("the deadline passed with nothing come"),
            Self::Disconnected => f.write_str("every sender has gone"),
            Self::Failed(why) => why.fmt(f),
        }
    }
}

impl std::error::Error for RecvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Disconnected => None,
            Self::Failed(why) => Some(why),
        }
    }
}

impl std::error::Error for RecvTimeoutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Timeout | Self::Disconnected => None,
            Self::Failed(why) => Some(why),
        }
    }
}

#[cfg(test)]
mod tests;
