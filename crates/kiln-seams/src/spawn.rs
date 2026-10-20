//! `Spawn`: tasks, run on their own. Kiln's runtime is the real one (`kiln::rt::RtSpawn`); the
//! simulator, [`SimSpawn`], runs its tasks only when the test says, one poll at a time in the
//! order they were woken, so a run is the same every time.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{AbortHandle, BoxFuture, abortable};
use futures::task::ArcWake;
use parking_lot::Mutex;

use crate::conformance::{Case, Failure, ensure, fine, same};

/// Why a task wasn't taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("the executor already runs its most tasks ({limit})")]
    Full { limit: usize },
    #[error("the executor has stopped")]
    Stopped,
}

/// Something that runs tasks.
pub trait Spawn: Send + Sync + 'static {
    /// Runs `task` on its own, to its end, whether or not anything waits for it; refused (and
    /// dropped, never run) when the executor can't take it.
    fn spawn_boxed(&self, task: BoxFuture<'static, ()>) -> Result<(), Refused>;
}

/// How a task ended without its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Ended {
    #[error("the task was aborted")]
    Aborted,
    /// It panicked, or its executor dropped it.
    #[error("the task ended without an output (it panicked, or its executor dropped it)")]
    Lost,
}

const RUNNING: u8 = 0;
const ABORTED: u8 = 1;

/// A task's output once it ends. Dropping the handle leaves the task running.
pub struct JoinHandle<T> {
    rx: oneshot::Receiver<T>,
    abort: AbortHandle,
    how: Arc<AtomicU8>,
}

impl<T> JoinHandle<T> {
    /// Ends the task at its next wait: what comes after that wait never runs.
    pub fn abort(&self) {
        self.abort.abort();
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, Ended>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(v)) => Poll::Ready(Ok(v)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(if self.how.load(Ordering::Acquire) == ABORTED { Ended::Aborted } else { Ended::Lost })),
        }
    }
}

/// Runs `fut` as a task of `on`, its output through the handle.
pub fn spawn<T, F>(on: &dyn Spawn, fut: F) -> Result<JoinHandle<T>, Refused>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let (fut, abort) = abortable(fut);
    let how = Arc::new(AtomicU8::new(RUNNING));
    let marks = Arc::clone(&how);
    on.spawn_boxed(Box::pin(async move {
        match fut.await {
            Ok(v) => {
                let _ = tx.send(v);
            }
            Err(_) => marks.store(ABORTED, Ordering::Release),
        }
    }))?;
    Ok(JoinHandle { rx, abort, how })
}

// ---------------------------------------------------------------------------------- simulator

/// Most tasks a [`SimSpawn`] holds at once by default.
pub const SIM_TASKS: usize = 10_000;
/// Most polls one [`SimSpawn::run_until_stalled`] makes.
pub const SIM_POLLS: u64 = 1_000_000;

/// An executor that runs its tasks only in [`SimSpawn::run_until_stalled`]: each woken task is
/// polled once, in the order they were woken (first spawned, first run), until none is ready.
/// A task that panics ends alone (counted in `panics()`). Clones share one executor.
#[derive(Clone)]
pub struct SimSpawn {
    inner: Arc<ExecInner>,
}

struct ExecInner {
    limit: usize,
    state: Mutex<ExecState>,
}

#[derive(Default)]
struct ExecState {
    tasks: HashMap<u64, Option<BoxFuture<'static, ()>>>,
    ready: VecDeque<u64>,
    queued: HashSet<u64>,
    next: u64,
    panics: u64,
    stopped: bool,
}

/// How a [`SimSpawn::run_until_stalled`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ran {
    /// No task was ready: `polls` were made.
    Stalled { polls: u64 },
    /// It made its most polls with tasks still ready (one that wakes itself forever).
    Budget,
}

struct TaskWaker {
    id: u64,
    exec: Weak<ExecInner>,
}

impl ArcWake for TaskWaker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        if let Some(exec) = arc_self.exec.upgrade() {
            let mut st = exec.state.lock();
            if st.tasks.contains_key(&arc_self.id) && st.queued.insert(arc_self.id) {
                st.ready.push_back(arc_self.id);
            }
        }
    }
}

impl Default for SimSpawn {
    fn default() -> Self {
        Self::new()
    }
}

impl SimSpawn {
    pub fn new() -> Self {
        Self::with_limit(SIM_TASKS)
    }

    /// An executor that holds at most `limit` tasks at once.
    pub fn with_limit(limit: usize) -> Self {
        Self { inner: Arc::new(ExecInner { limit, state: Mutex::new(ExecState::default()) }) }
    }

    /// Tasks not yet ended.
    pub fn tasks(&self) -> usize {
        self.inner.state.lock().tasks.len()
    }

    /// Tasks that ended by panicking.
    pub fn panics(&self) -> u64 {
        self.inner.state.lock().panics
    }

    /// Takes no more tasks; those held are dropped.
    pub fn stop(&self) {
        let dropped: Vec<BoxFuture<'static, ()>> = {
            let mut st = self.inner.state.lock();
            st.stopped = true;
            st.ready.clear();
            st.queued.clear();
            st.tasks.drain().filter_map(|(_, t)| t).collect()
        };
        drop(dropped);
    }

    /// Polls ready tasks, one poll each in the order they were woken, until none is ready.
    pub fn run_until_stalled(&self) -> Ran {
        let mut polls = 0u64;
        loop {
            if polls >= SIM_POLLS {
                return Ran::Budget;
            }
            let next = {
                let mut st = self.inner.state.lock();
                let Some(id) = st.ready.pop_front() else { return Ran::Stalled { polls } };
                st.queued.remove(&id);
                st.tasks.get_mut(&id).and_then(Option::take).map(|t| (id, t))
            };
            let Some((id, mut task)) = next else { continue };
            polls = polls.saturating_add(1);
            let waker = futures::task::waker(Arc::new(TaskWaker { id, exec: Arc::downgrade(&self.inner) }));
            let mut cx = Context::from_waker(&waker);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task.as_mut().poll(&mut cx)));
            let mut st = self.inner.state.lock();
            match outcome {
                Ok(Poll::Pending) => {
                    if let Some(slot) = st.tasks.get_mut(&id) {
                        *slot = Some(task);
                    }
                }
                Ok(Poll::Ready(())) => {
                    st.tasks.remove(&id);
                }
                Err(_) => {
                    st.tasks.remove(&id);
                    st.panics = st.panics.saturating_add(1);
                }
            }
        }
    }
}

impl Spawn for SimSpawn {
    fn spawn_boxed(&self, task: BoxFuture<'static, ()>) -> Result<(), Refused> {
        let mut st = self.inner.state.lock();
        if st.stopped {
            return Err(Refused::Stopped);
        }
        if st.tasks.len() >= self.inner.limit {
            return Err(Refused::Full { limit: self.inner.limit });
        }
        let id = st.next;
        st.next = st.next.saturating_add(1);
        st.tasks.insert(id, Some(task));
        st.queued.insert(id);
        st.ready.push_back(id);
        Ok(())
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Spawn` conformance suite runs against.
pub trait SpawnFixture: Send + Sync {
    fn spawner(&self) -> Arc<dyn Spawn>;
    /// `fut`'s output, while the executor runs its tasks; `None` when it can't come (a simulated
    /// executor with nothing left to run; a real one, past a bounded wait).
    fn finish<'a, T: Send + 'a>(&'a self, fut: BoxFuture<'a, T>) -> BoxFuture<'a, Option<T>>;
}

/// The fixture for a [`SimSpawn`].
pub struct SimSpawnFixture(pub SimSpawn);

impl SpawnFixture for SimSpawnFixture {
    fn spawner(&self) -> Arc<dyn Spawn> {
        Arc::new(self.0.clone())
    }
    fn finish<'a, T: Send + 'a>(&'a self, mut fut: BoxFuture<'a, T>) -> BoxFuture<'a, Option<T>> {
        Box::pin(async move {
            loop {
                if let Some(v) = (&mut fut).now_or_never() {
                    return Some(v);
                }
                if matches!(self.0.run_until_stalled(), Ran::Stalled { polls: 0 }) {
                    return (&mut fut).now_or_never();
                }
            }
        })
    }
}

fn c1<F: SpawnFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let h = fine("spawn", spawn(f.spawner().as_ref(), async { 40 + 2 }))?;
        same("the task's output", f.finish(Box::pin(h)).await, Some(Ok(42)))
    })
}

fn c2<F: SpawnFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let (tx, rx) = oneshot::channel::<&'static str>();
        let h = fine("spawn", spawn(f.spawner().as_ref(), async move { tx.send("ran").ok() }))?;
        drop(h);
        same("what a task whose handle was dropped did", f.finish(Box::pin(rx)).await, Some(Ok("ran")))
    })
}

fn c3<F: SpawnFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        // Two tasks, each waiting on the other: both end only if they run side by side.
        let (ping_tx, ping_rx) = oneshot::channel::<u32>();
        let (pong_tx, pong_rx) = oneshot::channel::<u32>();
        let sp = f.spawner();
        let a = fine("spawn", spawn(sp.as_ref(), async move {
            let _ = ping_tx.send(1);
            pong_rx.await.unwrap_or(0)
        }))?;
        let b = fine("spawn", spawn(sp.as_ref(), async move {
            let n = ping_rx.await.unwrap_or(0);
            let _ = pong_tx.send(n.saturating_add(1));
            n
        }))?;
        same("the first task's output", f.finish(Box::pin(a)).await, Some(Ok(2)))?;
        same("the second task's output", f.finish(Box::pin(b)).await, Some(Ok(1)))
    })
}

fn c4<F: SpawnFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let (gate_tx, gate_rx) = oneshot::channel::<()>();
        let (after_tx, after_rx) = oneshot::channel::<()>();
        let h = fine("spawn", spawn(f.spawner().as_ref(), async move {
            let _ = gate_rx.await;
            let _ = after_tx.send(());
        }))?;
        h.abort();
        same("an aborted task's handle", f.finish(Box::pin(h)).await, Some(Err(Ended::Aborted)))?;
        let _ = gate_tx.send(());
        let after = f.finish(Box::pin(after_rx)).await;
        ensure(!matches!(after, Some(Ok(()))), || "what came after an aborted task's wait ran".into())
    })
}

#[expect(clippy::panic, reason = "the clause is that a task that panics ends alone: this one must panic")]
fn c5<F: SpawnFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let sp = f.spawner();
        let bad = fine("spawn", spawn(sp.as_ref(), async {
            panic!("a task that panics, on purpose (Spawn's conformance case SPAWN-5)");
        }))?;
        let good = fine("spawn", spawn(sp.as_ref(), async { 7 }))?;
        same("a panicking task's handle", f.finish(Box::pin(bad)).await.map(|r: Result<(), Ended>| r.err()), Some(Some(Ended::Lost)))?;
        same("another task's output", f.finish(Box::pin(good)).await, Some(Ok(7)))?;
        let after = fine("spawn after a panic", spawn(sp.as_ref(), async { 8 }))?;
        same("a task spawned after the panic", f.finish(Box::pin(after)).await, Some(Ok(8)))
    })
}

fn c6<F: SpawnFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let sp = f.spawner();
        let mut handles = Vec::new();
        for i in 0..1000u64 {
            handles.push(fine("spawn", spawn(sp.as_ref(), async move { i.saturating_mul(2) }))?);
        }
        let all = f.finish(Box::pin(futures::future::join_all(handles))).await;
        let sum: u64 = all.unwrap_or_default().into_iter().filter_map(Result::ok).sum();
        same("the sum of a thousand tasks' outputs", sum, 999_000)
    })
}

/// The `Spawn` contract, clause by clause (docs/seams.md, "Spawn").
pub fn cases<F: SpawnFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "SPAWN-1", says: "a spawned task runs to its end, and its handle gives its output", check: c1::<F> },
        Case { id: "SPAWN-2", says: "a task runs to its end whether or not its handle is kept", check: c2::<F> },
        Case { id: "SPAWN-3", says: "tasks run side by side: two that wait on each other both end", check: c3::<F> },
        Case { id: "SPAWN-4", says: "abort ends a task at its next wait: its handle says Aborted, and what came after never runs", check: c4::<F> },
        Case { id: "SPAWN-5", says: "a task that panics ends alone: its handle says Lost, and other tasks run on", check: c5::<F> },
        Case { id: "SPAWN-6", says: "a thousand tasks all run to their ends", check: c6::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    #[test]
    fn the_simulated_executor_passes_the_spawn_suite() -> Result<(), String> {
        futures::executor::block_on(run("Spawn", &cases::<SimSpawnFixture>(), || async { SimSpawnFixture(SimSpawn::new()) })).into_result().map(|_| ())
    }

    #[test]
    fn tasks_run_only_when_the_test_says_in_the_order_they_were_woken() {
        let ex = SimSpawn::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        for i in 0..3 {
            let o = Arc::clone(&order);
            assert!(ex.spawn_boxed(Box::pin(async move { o.lock().push(i) })).is_ok());
        }
        assert!(order.lock().is_empty(), "nothing runs until the test says");
        assert_eq!(ex.run_until_stalled(), Ran::Stalled { polls: 3 });
        assert_eq!(*order.lock(), vec![0, 1, 2]);
        assert_eq!(ex.tasks(), 0);
    }

    #[test]
    fn a_full_executor_refuses_and_a_stopped_one_too() {
        let ex = SimSpawn::with_limit(1);
        assert!(ex.spawn_boxed(Box::pin(futures::future::pending())).is_ok());
        assert_eq!(ex.spawn_boxed(Box::pin(async {})), Err(Refused::Full { limit: 1 }));
        ex.stop();
        assert_eq!(ex.tasks(), 0);
        assert_eq!(ex.spawn_boxed(Box::pin(async {})), Err(Refused::Stopped));
    }

    #[test]
    fn a_task_that_wakes_itself_forever_ends_the_run_at_its_budget() {
        let ex = SimSpawn::new();
        assert!(ex.spawn_boxed(Box::pin(futures::future::poll_fn(|cx| {
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        })))
        .is_ok());
        assert_eq!(ex.run_until_stalled(), Ran::Budget);
    }
}
