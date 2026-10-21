//! kiln's runtime: hyper-raft's hyper-rt (thread-per-core shards with an epoll driver and a timing
//! wheel, a blocking pool, signals, DNS) behind the handful of primitives the rest of kiln uses.
//!
//! * One process-wide [`hyper_rt::Runtime`], started on first use with the sizing chosen by
//!   [`configure`] (server, agent or CLI). Shards never steal work and there is no preemption, so
//!   anything CPU-heavy or blocking goes to the blocking pool ([`blocking`]).
//! * [`spawn`] keeps a task on the calling shard (locality: sockets and timers stay where they were
//!   registered); spawns from outside the runtime are spread round-robin over the shards.
//! * hyper-rt's leaf futures (timers, socket readiness, its sync channels) accept only the polling
//!   task's own waker. Everything exported here works under any waker: timers fall back to a
//!   helper task ([`Bridge`]) when polled through a combinator or library that substitutes its own
//!   waker, and the cross-task primitives are built on `event-listener` / `async-channel` /
//!   `async-broadcast`, which work with any executor.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use futures::FutureExt as _;
use hyper_rt::{RtError, Runtime, RuntimeConfig, SlotHolder, TaskId};

// ------------------------------------------------------------------------------------- sizing

/// How big the process's runtime is. Chosen once, before first use ([`configure`]).
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    /// Shard threads.
    pub shards: u16,
    /// Task slots per shard (about 650 B of RSS each with timers and interests at 2x).
    pub tasks_per_shard: usize,
    /// Places in the blocking pool's shares: (dns, store, fs).
    pub pool: (usize, usize, usize),
}

impl Profile {
    /// The server: HTTP connections, websockets, the scheduler and GitOps loops.
    pub fn server() -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let shards = u16::try_from((cpus / 4).clamp(2, 4)).unwrap_or(2);
        Self { shards, tasks_per_shard: 8192, pool: (4, 2, 8) }
    }

    /// An agent: a few jobs, their output pumps, terminals.
    pub fn agent() -> Self {
        Self { shards: 2, tasks_per_shard: 2048, pool: (2, 1, 6) }
    }

    /// One-shot CLI commands and tests.
    pub fn tool() -> Self {
        Self { shards: 2, tasks_per_shard: 4096, pool: (2, 2, 4) }
    }

    fn with_env_overrides(mut self) -> Self {
        if let Some(n) = std::env::var("KILN_RT_SHARDS").ok().and_then(|v| v.parse::<u16>().ok()) {
            self.shards = n.clamp(1, 64);
        }
        if let Some(n) = std::env::var("KILN_RT_TASKS").ok().and_then(|v| v.parse::<usize>().ok()) {
            self.tasks_per_shard = n.clamp(256, 1 << 20);
        }
        self
    }
}

static PROFILE: OnceLock<Profile> = OnceLock::new();

/// Choose the runtime's size. Only the first call before the runtime starts has any effect.
pub fn configure(profile: Profile) {
    let _ = PROFILE.set(profile);
}

/// hyper-rt configuration (see the sizing rules in hyper-rt's docs): timers at twice the tasks so the
/// wheel never runs dry, a 250 µs step budget so sockets stay responsive on a busy shard, no spin.
pub fn runtime_config(shards: u16, tasks: usize) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        tasks_per_shard: tasks,
        timers_per_shard: tasks.saturating_mul(2),
        interests_per_shard: hyper_rt::runtime::interests_for(tasks),
        ring_entries: 1024,
        step_budget_ns: 250_000,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// The blocking pool's shares.
pub const SHARE_FS: &str = "fs";
pub const SHARE_STORE: &str = "store";

struct Rt {
    /// Held for the life of the process: dropping it would stop the shards.
    _runtime: Runtime,
    holders: Vec<SlotHolder>,
    next: AtomicUsize,
}

static RT: OnceLock<Result<Rt, String>> = OnceLock::new();

fn start() -> Result<Rt, String> {
    let p = PROFILE.get().copied().unwrap_or_else(Profile::tool).with_env_overrides();
    let (dns, store, fs) = p.pool;
    let pool = hyper_rt::blocking::Config {
        workers: dns + store + fs,
        shares: vec![
            hyper_rt::blocking::Share { name: hyper_rt::dns::SHARE, limit: dns, waiting: 256 },
            hyper_rt::blocking::Share { name: SHARE_STORE, limit: store, waiting: 16_384 },
            hyper_rt::blocking::Share { name: SHARE_FS, limit: fs, waiting: 8_192 },
        ],
    };
    match hyper_rt::blocking::start(pool) {
        Ok(()) | Err(RtError::BadConfig { .. }) => {}
        Err(e) => return Err(format!("blocking pool: {e}")),
    }
    let runtime = Runtime::start(&runtime_config(p.shards, p.tasks_per_shard)).map_err(|e| format!("runtime: {e}"))?;
    let holders: Vec<SlotHolder> = runtime.shard_ids().iter().filter_map(|id| runtime.holder_of(*id)).collect();
    if holders.is_empty() {
        return Err("runtime started without shards".to_owned());
    }
    tracing::debug!(shards = p.shards, tasks = p.tasks_per_shard, "hyper-rt runtime started");
    Ok(Rt { _runtime: runtime, holders, next: AtomicUsize::new(0) })
}

fn rt() -> anyhow::Result<&'static Rt> {
    RT.get_or_init(start).as_ref().map_err(|e| anyhow::anyhow!("hyper-rt failed to start: {e}"))
}

/// Whether the caller runs on a hyper-rt shard (inside a task).
pub fn on_shard() -> bool {
    hyper_rt::futures::shard_id().is_some()
}

// ------------------------------------------------------------------------------------ spawning

type Boxed = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A future handed to the runtime through a slot, so a refused submission (full control queue,
/// full arena) leaves it in place for the next attempt instead of dropping it.
fn handoff(fut: Boxed) -> Arc<Mutex<Option<Boxed>>> {
    Arc::new(Mutex::new(Some(fut)))
}

fn bootstrap(slot: &Arc<Mutex<Option<Boxed>>>) -> impl Future<Output = ()> + Send + 'static {
    let slot = Arc::clone(slot);
    async move {
        let fut = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(fut) = fut {
            fut.await;
        }
    }
}

fn submit(slot: &Arc<Mutex<Option<Boxed>>>) -> bool {
    // On a shard: keep the task here (no cross-thread hop; its sockets and timers stay local).
    if on_shard() && hyper_rt::futures::spawn_detached(bootstrap(slot)).is_ok() {
        return true;
    }
    submit_spread(slot)
}

fn submit_spread(slot: &Arc<Mutex<Option<Boxed>>>) -> bool {
    let Ok(rt) = rt() else { return false };
    let n = rt.holders.len();
    let start = rt.next.fetch_add(1, Ordering::Relaxed);
    for i in 0..n {
        if let Some(holder) = rt.holders.get(start.wrapping_add(i) % n)
            && hyper_rt::runtime::submit_to_holder(*holder, bootstrap(slot)).is_ok()
        {
            return true;
        }
    }
    false
}

/// Whether the runtime took the task (refused on every shard: dropped, and said so).
fn spawn_boxed(fut: Boxed) -> bool {
    let slot = handoff(fut);
    if submit(&slot) {
        return true;
    }
    if !on_shard() {
        // Off the runtime (startup, tests): wait out a burst that filled the control queues.
        for _ in 0..2000 {
            std::thread::sleep(Duration::from_millis(2));
            if submit(&slot) {
                return true;
            }
        }
    }
    crate::metrics::global().panics_caught.inc();
    tracing::error!("hyper-rt refused a task on every shard (arenas or control queues full); task dropped");
    false
}

/// Wait for a spawned task's output. Dropping the handle detaches the task (it keeps running).
pub struct JoinHandle<T> {
    rx: futures::channel::oneshot::Receiver<T>,
    abort: futures::future::AbortHandle,
}

impl<T> JoinHandle<T> {
    /// Stop the task at its next poll (its future is dropped).
    pub fn abort(&self) {
        self.abort.abort();
    }
}

impl<T> Future for JoinHandle<T> {
    /// `Err` when the task was aborted, panicked, or could not be started.
    type Output = Result<T, futures::channel::oneshot::Canceled>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx).poll(cx)
    }
}

/// Run `fut` as its own task: on the current shard when called from one, otherwise on the next
/// shard round-robin. A panic ends only that task (hyper-rt catches it; the handle reports `Err`).
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    let (fut, abort) = futures::future::abortable(fut);
    let _ = spawn_boxed(Box::pin(async move {
        if let Ok(v) = fut.await {
            let _ = tx.send(v);
        }
    }));
    JoinHandle { rx, abort }
}

/// The runtime as kiln-seams' [`kiln_seams::Spawn`]: a task it can't take is refused, typed.
#[derive(Debug, Clone, Copy, Default)]
pub struct RtSpawn;

impl kiln_seams::Spawn for RtSpawn {
    fn spawn_boxed(&self, task: futures::future::BoxFuture<'static, ()>) -> Result<(), kiln_seams::spawn::Refused> {
        if spawn_boxed(task) {
            return Ok(());
        }
        let p = PROFILE.get().copied().unwrap_or_else(Profile::tool);
        Err(kiln_seams::spawn::Refused::Full { limit: usize::from(p.shards).saturating_mul(p.tasks_per_shard) })
    }
}

/// Run `fut` on the next shard round-robin wherever the caller is: spreads accepted connections
/// over every shard. Falls back to the current shard when the others refuse.
pub fn spawn_spread<F: Future<Output = ()> + Send + 'static>(fut: F) {
    let slot = handoff(Box::pin(fut));
    if submit_spread(&slot) || submit(&slot) {
        return;
    }
    tracing::error!("hyper-rt refused a connection task on every shard; connection dropped");
}

/// Run a `!Send` future as a task on the current shard. Refused off the runtime.
pub fn spawn_local<F: Future<Output = ()> + 'static>(fut: F) -> Result<TaskId, RtError> {
    hyper_rt::futures::spawn_detached(fut)
}

/// Run `fut` to completion on the runtime, blocking the calling (non-runtime) thread: `main` and
/// tests. A panic inside it becomes an error.
pub fn block_on<T, F>(fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    anyhow::ensure!(!on_shard(), "block_on called from inside the runtime");
    rt()?;
    let (tx, mut rx) = hyper_rt::sync::oneshot::<anyhow::Result<T>>().map_err(|e| anyhow::anyhow!("oneshot: {e}"))?;
    let _ = spawn_boxed(Box::pin(async move {
        // A panic says what it was (a failed assertion, in a test), rather than leaving it lost on
        // the thread that ran it.
        let out = match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
            Ok(r) => r,
            Err(p) => Err(anyhow::anyhow!("the task panicked: {}", crate::sup::panic_message(p.as_ref()))),
        };
        let _ = tx.send(out);
    }));
    match rx.blocking_recv() {
        Ok(r) => r,
        Err(_) => anyhow::bail!("the task ended without a result (it could not start)"),
    }
}

// ------------------------------------------------------------------------------- blocking pool

/// Run `f` on the blocking pool's `fs` share (files, `/proc`, archives, anything that blocks).
pub async fn blocking<R, F>(f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    blocking_in(SHARE_FS, f).await
}

/// Run `f` on a named share of the blocking pool. Waits (instead of failing) while the share's
/// queue is full; if the pool is unavailable the job gets a thread of its own.
pub async fn blocking_in<R, F>(share: &'static str, f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    rt()?;
    let (tx, rx) = futures::channel::oneshot::channel::<R>();
    let job: Arc<Mutex<Option<(F, futures::channel::oneshot::Sender<R>)>>> = Arc::new(Mutex::new(Some((f, tx))));
    let mut delay = Duration::from_millis(1);
    loop {
        let j = Arc::clone(&job);
        let submitted = hyper_rt::blocking::run(share, move || {
            let taken = j.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some((f, tx)) = taken {
                let _ = tx.send(f());
            }
        });
        match submitted {
            Ok(_receiver) => break,
            Err(RtError::Capacity { .. }) => {
                sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            Err(e) => {
                tracing::warn!(error = %e, share, "blocking pool unavailable; running on a thread");
                let j = Arc::clone(&job);
                std::thread::Builder::new()
                    .name("kiln-blocking".into())
                    .spawn(move || {
                        let taken = j.lock().unwrap_or_else(PoisonError::into_inner).take();
                        if let Some((f, tx)) = taken {
                            let _ = tx.send(f());
                        }
                    })
                    .context("spawn blocking thread")?;
                break;
            }
        }
    }
    rx.await.map_err(|_| anyhow::anyhow!("blocking job failed (it panicked)"))
}

// ------------------------------------------------------------------------------------- bridging

/// Map a hyper-rt refusal to `io::Error`, keeping the OS errno so `ErrorKind` stays meaningful.
pub fn to_io(error: RtError) -> io::Error {
    match error {
        RtError::DriverRefused { code: Some(code), .. } => io::Error::from_raw_os_error(code),
        RtError::WouldBlock { .. } => io::Error::from(io::ErrorKind::WouldBlock),
        other => io::Error::other(other),
    }
}

/// Whether `cx` carries the waker of the hyper-rt task being polled right now (what hyper-rt's leaf
/// futures require). False under a combinator's or library's own waker, or off the runtime.
pub fn is_native(cx: &Context<'_>) -> bool {
    match (hyper_rt::waker::word_of(cx.waker()), hyper_rt::futures::current_task()) {
        (Some(word), Some(task)) => word == task.0,
        _ => false,
    }
}

struct BridgeState<T> {
    outcome: Option<T>,
    waker: Option<Waker>,
}

/// A hyper-rt leaf future awaited by a helper task on the current shard, so it can be awaited under
/// a foreign waker: the helper polls it with its own waker, stores the output and wakes whichever
/// waker polled the bridge last. Dropping the bridge cancels the helper.
pub struct Bridge<T: Send + 'static> {
    shared: Arc<Mutex<BridgeState<T>>>,
    task: TaskId,
}

impl<T: Send + 'static> Bridge<T> {
    pub fn spawn<F: Future<Output = T> + 'static>(leaf: F, waker: &Waker) -> Result<Self, RtError> {
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

    pub fn poll(&self, cx: &mut Context<'_>) -> Poll<T> {
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
        // A helper that already finished is a stale id: refused, harmlessly.
        let _ = hyper_rt::futures::cancel(self.task);
    }
}

// --------------------------------------------------------------------------------------- timers

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A sleep until a deadline on the shard's timing wheel; under a foreign waker it runs in a
/// [`Bridge`]. If the runtime refuses the timer it completes rather than hang. `Send + Sync`, so it
/// also serves as hyper's `Sleep`.
pub struct Sleep {
    deadline: Instant,
    native: Option<hyper_rt::futures::Sleep>,
    bridged: Option<Bridge<Result<(), RtError>>>,
}

impl Sleep {
    pub fn until(deadline: Instant) -> Self {
        Self { deadline, native: None, bridged: None }
    }

    fn remaining_ns(&self) -> u64 {
        nanos(self.deadline.saturating_duration_since(Instant::now()))
    }
}

impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
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

pub fn sleep(d: Duration) -> Sleep {
    Sleep::until(kiln_seams::clock::later(Instant::now(), d))
}

/// Real time, on the runtime's timers, as kiln-seams' [`kiln_seams::Clock`]: what production code
/// that takes a clock is given (tests give it a `kiln_seams::clock::SimClock`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RtClock;

impl kiln_seams::Clock for RtClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn unix_ms(&self) -> i64 {
        crate::util::now_ms()
    }
    fn sleep_until(&self, deadline: Instant) -> kiln_seams::clock::Sleep {
        Box::pin(sleep_until(deadline))
    }
}

/// The process's real clock.
pub fn clock() -> Arc<dyn kiln_seams::Clock> {
    Arc::new(RtClock)
}

pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep::until(deadline)
}

/// The deadline passed before the future finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline elapsed")
    }
}

impl std::error::Error for Elapsed {}

/// `fut`'s output, or `Err(Elapsed)` once `d` has passed (the future is then dropped). When both are
/// ready the output wins.
pub async fn timeout<F: Future>(d: Duration, fut: F) -> Result<F::Output, Elapsed> {
    let fut = pin!(fut);
    let deadline = pin!(sleep(d));
    match futures::future::select(fut, deadline).await {
        futures::future::Either::Left((v, _)) => Ok(v),
        futures::future::Either::Right(_) => Err(Elapsed),
    }
}

/// A periodic tick: the first `tick()` completes at once, then one per period. Ticks missed while
/// busy are skipped, not replayed. `tick()` is cancel-safe (state changes only when it completes).
pub struct Interval {
    period: Duration,
    next: Instant,
}

impl Interval {
    pub fn new(period: Duration) -> Self {
        Self { period: period.max(Duration::from_millis(1)), next: Instant::now() }
    }

    pub async fn tick(&mut self) {
        sleep_until(self.next).await;
        let now = Instant::now();
        self.next += self.period;
        if self.next <= now {
            self.next = now + self.period;
        }
    }
}

// ---------------------------------------------------------------------------------- shutdown

#[derive(Default)]
struct ShutdownInner {
    cancelled: AtomicBool,
    event: event_listener::Event,
}

/// A cancellation signal any number of tasks can wait on (process shutdown, a job's cancel).
#[derive(Clone, Default)]
pub struct Shutdown(Arc<ShutdownInner>);

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::SeqCst) {
            self.0.event.notify(usize::MAX);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Completes once [`Self::cancel`] has been called (at once if it already was).
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let listener = self.0.event.listen();
            if self.is_cancelled() {
                return;
            }
            listener.await;
        }
    }

    /// Sleep for `d`: `true` when the time passed, `false` when cancelled first.
    pub async fn sleep(&self, d: Duration) -> bool {
        if self.is_cancelled() {
            return false;
        }
        let cancelled = pin!(self.cancelled());
        let slept = pin!(sleep(d));
        matches!(futures::future::select(cancelled, slept).await, futures::future::Either::Right(_))
    }
}

// ------------------------------------------------------------------------------------- notify

/// Wake one waiter, or leave a permit for the next one (like a binary semaphore).
#[derive(Default)]
pub struct Notify {
    permit: AtomicBool,
    event: event_listener::Event,
}

impl Notify {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify(&self) {
        self.permit.store(true, Ordering::SeqCst);
        self.event.notify(1);
    }

    pub async fn notified(&self) {
        loop {
            if self.permit.swap(false, Ordering::SeqCst) {
                return;
            }
            let listener = self.event.listen();
            if self.permit.swap(false, Ordering::SeqCst) {
                return;
            }
            listener.await;
        }
    }
}

// ---------------------------------------------------------------------------------- broadcast

/// Fan-out to every current subscriber; slow subscribers lose the oldest messages (they see
/// `RecvError::Overflowed`) instead of holding up the sender.
pub struct Broadcast<T: Clone> {
    tx: async_broadcast::Sender<T>,
    _keep_open: async_broadcast::InactiveReceiver<T>,
}

impl<T: Clone> Broadcast<T> {
    pub fn new(capacity: usize) -> Self {
        let (mut tx, rx) = async_broadcast::broadcast(capacity.max(1));
        tx.set_overflow(true);
        tx.set_await_active(false);
        Self { tx, _keep_open: rx.deactivate() }
    }

    /// Deliver to the current subscribers (dropped when there are none).
    pub fn send(&self, value: T) {
        let _ = self.tx.try_broadcast(value);
    }

    pub fn subscribe(&self) -> async_broadcast::Receiver<T> {
        self.tx.new_receiver()
    }

    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

// --------------------------------------------------------------------------------------- files

/// Read size for file streams.
const FILE_CHUNK: usize = 256 * 1024;
/// Bytes a file sink gathers before one write.
const SINK_BATCH: usize = 1 << 20;

/// Stream a file in chunks. Opening happens now (so a missing file is an error here); each chunk is
/// read on the blocking pool, and no thread waits while the consumer is slow. The stream ends at
/// end of file (or a read error, which is logged).
pub fn file_source(path: impl Into<PathBuf>) -> anyhow::Result<async_channel::Receiver<Bytes>> {
    let path = path.into();
    let file = Arc::new(std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?);
    let (tx, rx) = async_channel::bounded::<Bytes>(4);
    spawn(async move {
        let mut offset = 0u64;
        loop {
            let f = Arc::clone(&file);
            let read = blocking(move || {
                let mut buf = vec![0u8; FILE_CHUNK];
                let n = crate::os::read_at(&f, &mut buf, offset)?;
                buf.truncate(n);
                Ok::<_, io::Error>(buf)
            })
            .await;
            match read {
                Ok(Ok(buf)) if buf.is_empty() => return,
                Ok(Ok(buf)) => {
                    offset = offset.saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
                    if tx.send(Bytes::from(buf)).await.is_err() {
                        return;
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, path = %path.display(), "file read failed mid-stream");
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), path = %path.display(), "file read failed mid-stream");
                    return;
                }
            }
        }
    });
    Ok(rx)
}

/// Write a stream of chunks to a new file. Send chunks into the returned sender, drop it, then
/// await the future for the byte count. Writing runs concurrently (bounded), so the sender sees
/// back-pressure rather than unbounded buffering; exceeding `max` bytes fails the write.
pub fn file_sink(
    path: impl Into<PathBuf>,
    max: u64,
) -> anyhow::Result<(async_channel::Sender<Bytes>, impl Future<Output = anyhow::Result<u64>> + Send + 'static)> {
    let path = path.into();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = Arc::new(std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?);
    let (tx, rx) = async_channel::bounded::<Bytes>(16);
    let writer = spawn(async move {
        let mut total = 0u64;
        while let Ok(first) = rx.recv().await {
            let mut batch = vec![first];
            let mut size: usize = batch.iter().map(Bytes::len).sum();
            while size < SINK_BATCH {
                match rx.try_recv() {
                    Ok(more) => {
                        size = size.saturating_add(more.len());
                        batch.push(more);
                    }
                    Err(_) => break,
                }
            }
            let at = total;
            total = total.saturating_add(u64::try_from(size).unwrap_or(u64::MAX));
            anyhow::ensure!(total <= max, "upload exceeds {max} bytes");
            let f = Arc::clone(&file);
            blocking(move || {
                let mut off = at;
                for chunk in &batch {
                    crate::os::write_all_at(&f, chunk, off)?;
                    off = off.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                }
                Ok::<_, io::Error>(())
            })
            .await??;
        }
        let f = Arc::clone(&file);
        blocking(move || f.sync_data()).await??;
        Ok(total)
    });
    let done = async move { writer.await.map_err(|_| anyhow::anyhow!("file writer stopped"))? };
    Ok((tx, done))
}

// ----------------------------------------------------------------------------------------- dns

/// Resolve `host:port` (or an address literal) to one address, preferring IPv4.
pub async fn resolve(addr: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(a) = addr.parse::<SocketAddr>() {
        return Ok(a);
    }
    let (host, port) = addr.rsplit_once(':').with_context(|| format!("{addr}: expected host:port"))?;
    let port: u16 = port.parse().with_context(|| format!("{addr}: bad port"))?;
    let all = resolve_host(host, port).await?;
    all.iter()
        .find(|a| a.is_ipv4())
        .or_else(|| all.first())
        .copied()
        .with_context(|| format!("{addr}: no address"))
}

/// Every address for `host` (IPv6 brackets allowed), through hyper-rt's DNS (getaddrinfo on the
/// blocking pool).
pub async fn resolve_host(host: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    rt()?;
    hyper_rt::dns::resolve(bare, port, 8).await.map_err(|e| anyhow::anyhow!("resolve {bare}: {e}"))
}

// -------------------------------------------------------------------------------------- signals

pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGTERM: i32 = 15;

/// SIGINT / SIGTERM / SIGHUP as a stream. The subscription lives as long as this value (dropping
/// the last one restores the default action, i.e. the next SIGTERM kills the process).
pub struct Signals {
    rx: async_channel::Receiver<i32>,
}

impl Signals {
    pub fn new(kinds: &[i32]) -> anyhow::Result<Self> {
        use hyper_rt::signal::Signal;
        let wanted: Vec<Signal> = kinds.iter().flat_map(|k| crate::os::signal_kinds(*k).iter().copied()).collect();
        anyhow::ensure!(!wanted.is_empty(), "no supported signal requested");
        rt()?;
        let stream = hyper_rt::signal::subscribe(&wanted).map_err(|e| anyhow::anyhow!("subscribe to signals: {e}"))?;
        let (tx, rx) = async_channel::unbounded();
        // The stream is awaited at a task's top level (its own waker); consumers read the channel,
        // which works under any waker.
        spawn(async move {
            let mut failures = 0u32;
            loop {
                match stream.recv().await {
                    Ok(set) => {
                        failures = 0;
                        for s in set.iter() {
                            if tx.send(crate::os::signal_number(s)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        failures += 1;
                        tracing::warn!(error = %e, "signal stream error");
                        if failures > 10 {
                            return;
                        }
                        sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
        Ok(Self { rx })
    }

    /// The next signal (`None` once the subscription is gone).
    pub async fn next(&self) -> Option<i32> {
        self.rx.recv().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timers_spawn_and_cancellation() -> anyhow::Result<()> {
        block_on(async {
            let t = Instant::now();
            sleep(Duration::from_millis(30)).await;
            assert!(t.elapsed() >= Duration::from_millis(30));

            assert_eq!(timeout(Duration::from_millis(20), futures::future::pending::<()>()).await, Err(Elapsed));
            assert_eq!(timeout(Duration::from_secs(5), async { 7 }).await, Ok(7));

            let mut iv = Interval::new(Duration::from_millis(10));
            let t = Instant::now();
            for _ in 0..4 {
                iv.tick().await;
            }
            assert!(t.elapsed() >= Duration::from_millis(29));

            let h = spawn(async { 40 + 2 });
            assert_eq!(h.await.ok(), Some(42));

            let sd = Shutdown::new();
            let waiter = {
                let sd = sd.clone();
                spawn(async move { sd.cancelled().await })
            };
            let sd2 = sd.clone();
            spawn(async move {
                sleep(Duration::from_millis(10)).await;
                sd2.cancel();
            });
            assert!(timeout(Duration::from_secs(5), waiter).await.is_ok());
            assert!(!sd.sleep(Duration::from_secs(5)).await);
            assert!(Shutdown::new().sleep(Duration::from_millis(5)).await);

            let n = Arc::new(Notify::new());
            n.notify();
            timeout(Duration::from_secs(1), n.notified()).await?;

            let b = Broadcast::<u32>::new(4);
            let mut r = b.subscribe();
            b.send(1);
            assert_eq!(r.recv().await.ok(), Some(1));

            let v = blocking(|| 5 * 5).await?;
            assert_eq!(v, 25);
            let a = resolve("localhost:80").await?;
            assert_eq!(a.port(), 80);
            Ok(())
        })
    }

    /// kiln's runtime passes kiln-seams' Clock suite on real time (CP-M2, AC-M2.2): the clock the
    /// goal driver and the harness host take their time by in production (AC-M2.1).
    struct RtClockFixture;

    impl kiln_seams::clock::ClockFixture for RtClockFixture {
        fn clock(&self) -> Arc<dyn kiln_seams::Clock> {
            clock()
        }
        fn pass(&self, d: Duration) -> futures::future::BoxFuture<'_, ()> {
            Box::pin(sleep(d))
        }
        fn quantum(&self) -> Duration {
            Duration::from_millis(50)
        }
    }

    /// CP-M2, AC-M2.1 and AC-M2.2: production's clock passes the Clock suite.
    #[test]
    fn the_runtime_passes_the_clock_suite() -> anyhow::Result<()> {
        block_on(async {
            let cases = kiln_seams::clock::cases::<RtClockFixture>();
            // Each case fails, named, if it hasn't ended in 30 s.
            let n = kiln_seams::conformance::run_within("Clock (kiln's runtime)", &cases, || async { RtClockFixture }, || sleep(Duration::from_secs(30))).await.into_result().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(n == cases.len());
            Ok(())
        })
    }

    struct RtSpawnFixture;

    impl kiln_seams::spawn::SpawnFixture for RtSpawnFixture {
        fn spawner(&self) -> Arc<dyn kiln_seams::Spawn> {
            Arc::new(RtSpawn)
        }
        fn finish<'a, T: Send + 'a>(&'a self, fut: futures::future::BoxFuture<'a, T>) -> futures::future::BoxFuture<'a, Option<T>> {
            Box::pin(async move { timeout(Duration::from_secs(10), fut).await.ok() })
        }
    }

    /// CP-M2, AC-M2.2: kiln's runtime passes the Spawn suite.
    #[test]
    fn the_runtime_passes_the_spawn_suite() -> anyhow::Result<()> {
        block_on(async {
            let cases = kiln_seams::spawn::cases::<RtSpawnFixture>();
            let n = kiln_seams::conformance::run_within("Spawn (kiln's runtime)", &cases, || async { RtSpawnFixture }, || sleep(Duration::from_secs(30))).await.into_result().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(n == cases.len());
            Ok(())
        })
    }

    #[test]
    fn files_round_trip() -> anyhow::Result<()> {
        block_on(async {
            let dir = tempfile::tempdir()?;
            let p = dir.path().join("a/b.bin");
            let (tx, done) = file_sink(p.clone(), 10 << 20)?;
            let data: Vec<u8> = (0..(3 << 20)).map(|i: u32| (i % 251) as u8).collect();
            for chunk in data.chunks(70_000) {
                tx.send(Bytes::copy_from_slice(chunk)).await?;
            }
            drop(tx);
            assert_eq!(done.await?, u64::try_from(data.len())?);
            let rx = file_source(&p)?;
            let mut back = Vec::new();
            while let Ok(c) = rx.recv().await {
                back.extend_from_slice(&c);
            }
            assert_eq!(back, data);

            let (tx, done) = file_sink(dir.path().join("small"), 10)?;
            let _ = tx.send(Bytes::from_static(b"0123456789abc")).await;
            drop(tx);
            assert!(done.await.is_err());
            assert!(file_source(dir.path().join("missing")).is_err());
            Ok(())
        })
    }
}
