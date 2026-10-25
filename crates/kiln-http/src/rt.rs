//! The runtime underneath: hyper-rt's leaf futures (socket readiness, timers) accept only the
//! waker of the task being polled. A connection is polled by its own task, so its waits are native;
//! under a combinator or library that substitutes its own waker, a wait runs in a helper task on the
//! same shard instead ([`Bridge`]), which wakes whoever polled last.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use hyper_rt::{RtError, TaskId};

/// Whether `cx` carries the waker of the hyper-rt task being polled now (what hyper-rt's leaf
/// futures require). False under a substituted waker, or off the runtime.
pub(crate) fn is_native(cx: &Context<'_>) -> bool {
    match (hyper_rt::waker::word_of(cx.waker()), hyper_rt::futures::current_task()) {
        (Some(word), Some(task)) => word == task.0,
        _ => false,
    }
}

/// A hyper-rt error as `io::Error`, keeping the OS errno so its `ErrorKind` means something.
pub(crate) fn to_io(error: RtError) -> std::io::Error {
    match error {
        RtError::DriverRefused { code: Some(code), .. } => std::io::Error::from_raw_os_error(code),
        RtError::WouldBlock { .. } => std::io::Error::from(std::io::ErrorKind::WouldBlock),
        other => std::io::Error::other(other),
    }
}

struct BridgeState<T> {
    outcome: Option<T>,
    waker: Option<Waker>,
}

/// A leaf future awaited by a helper task on the current shard, for a poll under a foreign waker:
/// the helper polls it natively, keeps its output and wakes the waker that polled the bridge last.
/// Dropping the bridge cancels the helper.
pub(crate) struct Bridge<T: Send + 'static> {
    shared: Arc<Mutex<BridgeState<T>>>,
    task: TaskId,
}

impl<T: Send + 'static> Bridge<T> {
    pub(crate) fn spawn<F: Future<Output = T> + 'static>(leaf: F, waker: &Waker) -> Result<Self, RtError> {
        let shared = Arc::new(Mutex::new(BridgeState { outcome: None, waker: Some(waker.clone()) }));
        let helper = Arc::clone(&shared);
        let task = hyper_rt::futures::spawn_detached(async move {
            let outcome = leaf.await;
            let waker = {
                let mut state = helper.lock().unwrap_or_else(PoisonError::into_inner);
                state.outcome = Some(outcome);
                state.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        })?;
        Ok(Self { shared, task })
    }

    pub(crate) fn poll(&self, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        match state.outcome.take() {
            Some(outcome) => Poll::Ready(outcome),
            None => {
                if !state.waker.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                    state.waker = Some(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

impl<T: Send + 'static> Drop for Bridge<T> {
    fn drop(&mut self) {
        // A helper that finished already is a stale id: refused, harmlessly.
        let _ = hyper_rt::futures::cancel(self.task);
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A deadline on the shard's timing wheel; under a foreign waker it runs in a [`Bridge`]. A timer the
/// runtime refuses completes at once rather than never.
pub(crate) struct Sleep {
    deadline: Instant,
    native: Option<hyper_rt::futures::Sleep>,
    bridged: Option<Bridge<Result<(), RtError>>>,
}

impl Sleep {
    pub(crate) fn until(deadline: Instant) -> Sleep {
        Sleep { deadline, native: None, bridged: None }
    }

    pub(crate) fn after(d: Duration) -> Sleep {
        let now = Instant::now();
        Sleep::until(now.checked_add(d).unwrap_or(now))
    }

    fn remaining_ns(&self) -> u64 {
        nanos(self.deadline.saturating_duration_since(Instant::now()))
    }

    /// Ready once the deadline has passed.
    pub(crate) fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            self.native = None;
            self.bridged = None;
            return Poll::Ready(());
        }
        if is_native(cx) {
            self.bridged = None;
            let remaining = self.remaining_ns();
            let sleep = self.native.get_or_insert_with(|| hyper_rt::futures::sleep(remaining));
            return match Pin::new(sleep).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(_) => Poll::Ready(()),
            };
        }
        self.native = None;
        if self.bridged.is_none() {
            let remaining = self.remaining_ns();
            match Bridge::spawn(hyper_rt::futures::sleep(remaining), cx.waker()) {
                Ok(bridge) => self.bridged = Some(bridge),
                Err(_) => return Poll::Ready(()),
            }
        }
        match self.bridged.as_ref().map(|bridge| bridge.poll(cx)) {
            Some(Poll::Pending) => Poll::Pending,
            _ => {
                self.bridged = None;
                Poll::Ready(())
            }
        }
    }
}

impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.poll_expired(cx)
    }
}
