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

/// Panics injected on purpose: kiln's code never panics (clippy denies every way to), so these are
/// what a bug in a dependency, or one that slipped through, would raise. Each must stay where it
/// happened: the subsystem restarts, the task ends, the connection drops; the process goes on.
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A real panic, as a bug would raise it.
    #[allow(clippy::panic)]
    fn injected(what: &str) -> anyhow::Result<()> {
        panic!("injected: {what}")
    }

    #[test]
    fn a_subsystem_that_panics_is_restarted_and_its_siblings_run_on() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let reg = Registry::default();
            let stop = Shutdown::new();
            // A sibling that just keeps working.
            let ticks = Arc::new(AtomicU32::new(0));
            let (t, s) = (Arc::clone(&ticks), stop.clone());
            spawn(&reg, "steady", stop.clone(), move || {
                let (t, s) = (Arc::clone(&t), s.clone());
                async move {
                    while s.sleep(Duration::from_millis(10)).await {
                        t.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(())
                }
            });
            // A subsystem with a bug: its first two starts panic, the third finishes.
            let starts = Arc::new(AtomicU32::new(0));
            let st = Arc::clone(&starts);
            let flaky = spawn(&reg, "flaky", stop.clone(), move || {
                let n = st.fetch_add(1, Ordering::SeqCst);
                async move { if n < 2 { injected("a bug in a subsystem") } else { Ok(()) } }
            });
            let _ = flaky.await;
            let before = ticks.load(Ordering::SeqCst);
            crate::rt::sleep(Duration::from_millis(100)).await;
            let after = ticks.load(Ordering::SeqCst);
            stop.cancel();
            let snap = reg.snapshot();
            let f = snap.iter().find(|c| c.name == "flaky").ok_or_else(|| anyhow::anyhow!("flaky was never registered"))?;
            anyhow::ensure!(f.restarts == 2 && f.state == "finished", "{f:?}");
            anyhow::ensure!(f.last_error.as_deref().is_some_and(|e| e.contains("panic: injected: a bug in a subsystem")), "{f:?}");
            anyhow::ensure!(starts.load(Ordering::SeqCst) == 3, "started {} times", starts.load(Ordering::SeqCst));
            anyhow::ensure!(after > before, "the sibling stopped working ({before} -> {after})");
            anyhow::Ok(())
        })
    }

    #[test]
    fn a_task_that_panics_is_contained_and_counted() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let caught = crate::metrics::global().panics_caught.get();
            let task = spawn_guarded("doomed", async { injected("a bug in one task") });
            anyhow::ensure!(task.await.is_ok(), "the guard should have ended the task, not lost it");
            anyhow::ensure!(crate::metrics::global().panics_caught.get() > caught, "the panic was not counted");
            // The runtime goes on running other work.
            let answer = crate::rt::spawn(async { 41 + 1 }).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            anyhow::ensure!(answer == 42);
            anyhow::Ok(())
        })
    }

    /// Panics on `/boom`; answers "ok" otherwise.
    struct Boom;

    impl crate::http::server::Handler for Boom {
        async fn handle(self: Arc<Self>, req: crate::http::server::Request) -> crate::http::server::Response {
            if req.uri().path() == "/boom" {
                let _ = injected("a bug in one request");
            }
            crate::http::server::text(::http::StatusCode::OK, "ok")
        }
    }

    #[test]
    fn a_request_that_panics_ends_only_its_own_connection() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let tcp = std::net::TcpListener::bind("127.0.0.1:0")?;
            let addr = tcp.local_addr()?;
            let tcp = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(tcp), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let handler = Arc::new(Boom);
            crate::rt::spawn(async move {
                while let Ok(stream) = tcp.accept().await {
                    let (h, remote) = (Arc::clone(&handler), stream.peer_addr().ok());
                    crate::rt::spawn(crate::http::server::serve_connection(crate::http::io::RtIo::new(stream), h, remote));
                }
            });
            let client = crate::http::client::Client::public()?;
            let boom = client.get(&format!("http://{addr}/boom")).send().await;
            anyhow::ensure!(boom.is_err(), "the panicking request should have lost its connection");
            for _ in 0..3 {
                let ok = client.get(&format!("http://{addr}/ok")).send().await?;
                anyhow::ensure!(ok.status() == 200, "status {} after a request panicked", ok.status());
            }
            anyhow::Ok(())
        })
    }
}
