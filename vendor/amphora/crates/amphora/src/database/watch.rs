//! Cancellation-safe, bounded async wait registrations over durable published revisions.
use super::*;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[derive(Default)]
pub(super) struct Watchers {
    next: u64,
    pub(super) wakers: BTreeMap<u64, Waker>,
}
const WAIT_MAX: usize = 4096;

pub struct Watch {
    namespace: Namespace,
    family: String,
    after: Rev,
    registration: Option<u64>,
    remote: Option<crate::transport::Watch>,
}
impl Namespace {
    /// Watches durable family revisions. Remote waits have a thirty-second deadline and
    /// share a bounded reactor with at most 4096 registrations per client process.
    pub fn watch(&self, family: &str, after: Rev) -> Result<Watch, Refusal> {
        api::family(family)?;
        let until = Instant::now()
            .checked_add(Duration::from_secs(30))
            .ok_or(Refusal::Timeout)?;
        let namespace = self.resolved(until)?;
        let remote = match namespace.remote()? {
            Some(remote) => Some(remote.watch(family, after, until)?),
            None => None,
        };
        Ok(Watch {
            namespace,
            family: family.into(),
            after,
            registration: None,
            remote,
        })
    }
}
impl Future for Watch {
    type Output = Result<Rev, Refusal>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(remote) = self.remote.as_mut() {
            return Pin::new(remote).poll(cx);
        }
        let changed = Arc::clone(&self.namespace.database.shared.changed);
        let mut watchers = match locked(&changed.watchers) {
            Ok(v) => v,
            Err(e) => return Poll::Ready(Err(e)),
        };
        // Registration and the second readiness check share the wakeup lock. A publisher updates
        // durable metadata before taking that lock, so a ready change cannot fall between them.
        match self
            .namespace
            .try_wait_selected(&self.family, self.after, Instant::now())
        {
            Ok(Some(at)) => {
                if let Some(id) = self.registration.take() {
                    watchers.wakers.remove(&id);
                }
                Poll::Ready(Ok(at))
            }
            Err(error) => {
                if let Some(id) = self.registration.take() {
                    watchers.wakers.remove(&id);
                }
                Poll::Ready(Err(error))
            }
            Ok(None) => {
                let id = match self.registration {
                    Some(id) => id,
                    None => {
                        if let Err(error) = api::bound(
                            "watch registrations",
                            watchers.wakers.len().saturating_add(1),
                            WAIT_MAX,
                        ) {
                            return Poll::Ready(Err(error));
                        }
                        let Some(id) = watchers.next.checked_add(1) else {
                            return Poll::Ready(Err(unavailable(
                                "watch registration identity exhausted",
                            )));
                        };
                        watchers.next = id;
                        self.registration = Some(id);
                        id
                    }
                };
                watchers.wakers.insert(id, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(remote) = self.remote.take() {
            drop(remote);
            return;
        }
        if let Some(id) = self.registration.take() {
            match self.namespace.database.shared.changed.watchers.lock() {
                Ok(mut watchers) => {
                    watchers.wakers.remove(&id);
                }
                Err(error) => {
                    tracing::warn!(target: "amphora.records", event = "watch_cleanup_failed", error = %error, "watch registry lock was poisoned")
                }
            }
        }
    }
}
