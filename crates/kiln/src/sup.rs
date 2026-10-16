//! In-process supervision, Erlang style: every long-lived subsystem runs under [`spawn`], which
//! restarts it after an error *or* a panic with jittered exponential backoff. A child that stays up
//! for [`HEALTHY_AFTER`] has its backoff reset.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use parking_lot::Mutex;
use serde::Serialize;
use crate::rt::{JoinHandle, Shutdown};

pub const HEALTHY_AFTER: Duration = Duration::from_secs(60);
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
pub struct ChildStatus {
    pub name: String,
    pub state: &'static str,
    pub restarts: u64,
    pub last_error: Option<String>,
    pub since_unix: i64,
}

/// Registry of supervised children, exposed on `/api/system` and `/metrics`.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<Vec<ChildStatus>>>,
}

impl Registry {
    pub fn snapshot(&self) -> Vec<ChildStatus> {
        self.inner.lock().clone()
    }

    fn update(&self, name: &str, f: impl FnOnce(&mut ChildStatus)) {
        let mut g = self.inner.lock();
        if let Some(c) = g.iter_mut().find(|c| c.name == name) {
            f(c);
        } else {
            let mut c = ChildStatus {
                name: name.to_owned(),
                state: "starting",
                restarts: 0,
                last_error: None,
                since_unix: crate::util::now_unix(),
            };
            f(&mut c);
            g.push(c);
        }
    }
}

pub fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

pub fn jitter(d: Duration) -> Duration {
    let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let j = rand::random::<u64>() % (ms / 4 + 1);
    Duration::from_millis(ms.saturating_add(j))
}

/// Run `make()` forever (until `cancel`), restarting on `Err` or panic. Returning `Ok(())` means the
/// child finished on purpose and is not restarted.
pub fn spawn<F, Fut>(
    reg: &Registry,
    name: &str,
    cancel: Shutdown,
    mut make: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let reg = reg.clone();
    let name = name.to_owned();
    crate::rt::spawn(async move {
        let mut backoff = MIN_BACKOFF;
        loop {
            reg.update(&name, |c| {
                c.state = "running";
                c.since_unix = crate::util::now_unix();
            });
            let started = Instant::now();
            let fut = AssertUnwindSafe(make()).catch_unwind();
            let outcome = futures::select_biased! {
                () = cancel.cancelled().fuse() => {
                    reg.update(&name, |c| c.state = "stopped");
                    return;
                }
                r = fut.fuse() => r,
            };
            let err = match outcome {
                Ok(Ok(())) => {
                    reg.update(&name, |c| c.state = "finished");
                    tracing::info!(child = %name, "supervised task finished");
                    return;
                }
                Ok(Err(e)) => format!("{e:#}"),
                Err(p) => format!("panic: {}", panic_message(p.as_ref())),
            };
            if started.elapsed() >= HEALTHY_AFTER {
                backoff = MIN_BACKOFF;
            }
            tracing::error!(child = %name, error = %err, retry_in = ?backoff, "supervised task failed; restarting");
            reg.update(&name, |c| {
                c.state = "restarting";
                c.restarts += 1;
                c.last_error = Some(err);
            });
            crate::metrics::global().supervisor_restarts.inc();
            if !cancel.sleep(jitter(backoff)).await {
                reg.update(&name, |c| c.state = "stopped");
                return;
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    })
}

/// Spawn a one-shot task whose panic is logged instead of silently lost.
pub fn spawn_guarded<Fut>(name: &'static str, fut: Fut) -> JoinHandle<()>
where
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    crate::rt::spawn(async move {
        match AssertUnwindSafe(fut).catch_unwind().await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(task = name, error = %format!("{e:#}"), "task failed"),
            Err(p) => {
                crate::metrics::global().panics_caught.inc();
                tracing::error!(task = name, panic = %panic_message(p.as_ref()), "task panicked; contained");
            }
        }
    })
}
