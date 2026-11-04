//! kiln's runtime: kiln-rt, its own copy of hyper-raft's hyper-rt (thread-per-core shards with an
//! epoll driver and a timing wheel, a blocking pool, a compute pool, signals, DNS) behind the handful
//! of primitives the rest of kiln uses.
//!
//! * One process-wide [`hyper_rt::Runtime`], started on first use with the sizing chosen by
//!   [`configure`] (server, agent or CLI). Shards never steal work and there is no preemption, so
//!   CPU-heavy work goes to the compute pool ([`compute`], [`compute_pieces`]: off the shards and below
//!   them in the OS's priority) and calls that block go to the blocking pool ([`blocking`]).
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
    /// Places in the blocking pool's shares: (dns, store, fs). The blocking pool is for calls that block a
    /// thread; CPU work goes to the compute pool ([`compute`]; goal 3, plan 01a1a59c2a8e step 4), which the
    /// pool's `fs` share carried before (the UI's compression, password hashing, digests of bytes in
    /// memory). What each share's size rests on:
    /// - `dns`: `getaddrinfo` calls at once, hyper-rt's resolver's;
    /// - `store`: the store's statements, which its one connection takes one at a time (`crate::store`): a
    ///   place running and one waiting its turn, so a slow statement holds these, not the `fs` share's;
    /// - `fs`: everything else that blocks: files, `/proc`, archives, a child's wait.
    ///
    /// The counts are kiln's from when it moved onto hyper-rt (f202896cb2d), not yet measured: each
    /// share's size from what its calls wait under realistic load is the lab's to say (goal 1's
    /// `kiln-lab`, not yet on main).
    pub pool: (usize, usize, usize),
    /// Whether the runtime says how it was sized at `info` (a server's and a runner's logs), or only at
    /// `debug` (a command's, whose output is the person's).
    pub announce: bool,
}

impl Profile {
    /// The server: HTTP connections, websockets, the scheduler and GitOps loops.
    pub fn server() -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let shards = u16::try_from((cpus / 4).clamp(2, 4)).unwrap_or(2);
        Self { shards, tasks_per_shard: 8192, pool: (4, 2, 8), announce: true }
    }

    /// An agent: a few jobs, their output pumps, terminals.
    pub fn agent() -> Self {
        Self { shards: 2, tasks_per_shard: 2048, pool: (2, 1, 6), announce: true }
    }

    /// One-shot CLI commands and tests.
    pub fn tool() -> Self {
        Self { shards: 2, tasks_per_shard: 4096, pool: (2, 2, 4), announce: false }
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

/// The cores of the process's own its shards are fixed to (docs/priority.md, "Cores of its own";
/// goal 8, plan 01a1a59c2b29 step 6), as [`pin`] chose them.
static CORES: OnceLock<Vec<u32>> = OnceLock::new();

/// Fix the runtime's shards to `cores`, cores of the process's own (the server's `[server] cores`),
/// in their order: only the first call before the runtime starts has any effect.
pub fn pin(cores: Vec<u32>) {
    let _ = CORES.set(cores);
}

/// The core each of `shards` shards is fixed to, from cores of the process's own: the first kept
/// for its threads that aren't shards (its blocking pool, its timers' helpers), as hyper-rt's
/// placement keeps one, and each shard on the next, around again when there are more shards than
/// cores; with a single core, every shard on it.
fn shard_cores(cores: &[u32], shards: u16) -> Vec<u32> {
    let rest = cores.get(1..).filter(|r| !r.is_empty()).unwrap_or(cores);
    (0..usize::from(shards)).filter_map(|i| rest.get(i.checked_rem(rest.len())?).copied()).collect()
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
    let mut config = runtime_config(p.shards, p.tasks_per_shard);
    if let Some(cores) = CORES.get() {
        config.cores = shard_cores(cores, p.shards);
        config.pin = !config.cores.is_empty();
    }
    start_compute(&p, &config);
    let runtime = Runtime::start(&config).map_err(|e| format!("runtime: {e}"))?;
    let holders: Vec<SlotHolder> = runtime.shard_ids().iter().filter_map(|id| runtime.holder_of(*id)).collect();
    if holders.is_empty() {
        return Err("runtime started without shards".to_owned());
    }
    // Each shard asks for a slice of its step (goal 3, plan 01a1a59c2a8e step 1): waking on a CPU a compute
    // pool thread holds, it takes the CPU at once (`hyper_rt::compute::prefer_current_thread`; Linux).
    for holder in &holders {
        let slice = config.step_budget_ns;
        let _ = hyper_rt::runtime::submit_to_holder(*holder, async move {
            if let Err(refusal) = hyper_rt::compute::prefer_current_thread(slice) {
                tracing::debug!(%refusal, "a shard kept the OS's slice");
            }
        });
    }
    tracing::debug!(shards = p.shards, tasks = p.tasks_per_shard, "hyper-rt runtime started");
    Ok(Rt { _runtime: runtime, holders, next: AtomicUsize::new(0) })
}

/// The compute pool (goal 3 of the API runtime program, plan 01a1a59c2a8e step 1; `hyper_rt::compute`): as
/// many threads as the CPU the process is granted (its cgroup's quota or its cpuset, else the OS's count)
/// less its shards, at a lower OS priority than the shards that still has its share of the CPU against
/// other work in the process's cgroup, as requests wait on its jobs (`Rank::Below`); a piece of a job as
/// long as a shard's step, so a long job gives its thread to a waiting one as a task gives its shard; room
/// for a job for every task the shards can hold, each of which awaits its own. Said with how its size was
/// derived.
fn start_compute(p: &Profile, config: &RuntimeConfig) {
    if hyper_rt::compute::running() {
        return;
    }
    let grant = hyper_rt::compute::Grant::of_process();
    let shards = usize::from(p.shards);
    let workers = hyper_rt::compute::workers_for(&grant, shards);
    let pool = hyper_rt::compute::Config { workers, queued: shards.saturating_mul(p.tasks_per_shard), piece_ns: config.step_budget_ns, rank: hyper_rt::compute::Rank::Below };
    match hyper_rt::compute::start(pool) {
        Ok(started) => {
            let priority = match started.priority {
                Ok(priority) => priority.to_string(),
                Err(refusal) => format!("the process's own ({refusal})"),
            };
            let size = format!("{} threads: {grant}, less {shards} shards", started.workers);
            if p.announce {
                tracing::info!(size = %size, priority = %priority, lowered = started.lowered, "compute pool started");
            } else {
                tracing::debug!(size = %size, priority = %priority, lowered = started.lowered, "compute pool started");
            }
        }
        Err(e) => tracing::warn!(error = %e, "the compute pool didn't start: CPU-heavy jobs run on threads of their own"),
    }
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

/// The runtime's shards (it's started if it hasn't been): 0 when it couldn't start.
pub fn shard_count() -> usize {
    rt().map_or(0, |rt| rt.holders.len())
}

/// Run `fut` on shard `i` (of [`shard_count`]) and no other, for a task whose work is that shard's
/// own: a probe of its health (`crate::shard_health`), or a test holding one shard and not the
/// rest. Whether the shard took it.
pub fn spawn_on<F: Future<Output = ()> + Send + 'static>(i: usize, fut: F) -> bool {
    let Ok(rt) = rt() else { return false };
    rt.holders.get(i).is_some_and(|holder| hyper_rt::runtime::submit_to_holder(*holder, fut).is_ok())
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

/// Run `f` on the blocking pool's `fs` share: files, `/proc`, archives, anything that blocks. Work that
/// only computes goes to [`compute`].
pub async fn blocking<R, F>(f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    blocking_in(SHARE_FS, f).await
}

// --------------------------------------------------------------------------------- compute pool

pub use hyper_rt::compute::{Step, Turn};

/// Run `f` on the compute pool: CPU-heavy work (building snapshots, serializing big payloads,
/// compressing, diffing, hashing), off every shard and below them in the OS's priority, so one heavy
/// request holds up no other (goal 3, plan 01a1a59c2a8e; `hyper_rt::compute`). Its result comes back
/// here. Waits while the pool is at its bound; with no pool running, `f` gets a thread of its own. A
/// job that can run long goes in pieces: [`compute_pieces`].
pub async fn compute<R, F>(f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    rt()?;
    let (tx, rx) = futures::channel::oneshot::channel::<R>();
    let job = move || {
        let _ = tx.send(f());
    };
    to_compute(job, |job| hyper_rt::compute::run(job).map(drop), |job| job()).await?;
    rx.await.map_err(|_| anyhow::anyhow!("compute job failed (it panicked)"))
}

/// Run a job in pieces on the compute pool: `step` is called with `state` and its piece's [`Turn`]
/// until it returns [`Step::Done`]. Between units of its work it asks [`Turn::over`], and returns
/// [`Step::More`] when told yes: its piece has had a shard's step and another job waits for the thread,
/// which it then gives up, going on after (big JSON written a part at a time, a long diff a file at a
/// time). Otherwise as [`compute`].
pub async fn compute_pieces<S, T, F>(state: S, mut step: F) -> anyhow::Result<T>
where
    S: Send + 'static,
    T: Send + 'static,
    F: FnMut(&mut S, &Turn<'_>) -> Step<T> + Send + 'static,
{
    rt()?;
    let (tx, rx) = futures::channel::oneshot::channel::<T>();
    let piece = move |(state, tx): &mut (S, Option<futures::channel::oneshot::Sender<T>>), turn: &Turn<'_>| match step(state, turn) {
        Step::Done(value) => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(value);
            }
            Step::Done(())
        }
        Step::More => Step::More,
    };
    to_compute(((state, Some(tx)), piece), |(state, piece)| hyper_rt::compute::run_pieces(state, piece).map(drop), |(state, piece)| hyper_rt::compute::run_here(state, piece)).await?;
    rx.await.map_err(|_| anyhow::anyhow!("compute job failed (it panicked)"))
}

/// `f` of each item on the compute pool, in pieces ([`compute_pieces`]): an item a unit, its piece
/// ended where another job waits, so a long list holds no pool thread long. The outputs in the items'
/// order.
pub async fn compute_map<T, R, F>(items: Vec<T>, f: F) -> anyhow::Result<Vec<R>>
where
    T: Send + 'static,
    R: Send + 'static,
    F: FnMut(T) -> R + Send + 'static,
{
    let out = Vec::with_capacity(items.len());
    compute_pieces((items.into_iter(), out, f), |(todo, out, f), turn| {
        for item in todo.by_ref() {
            out.push(f(item));
            if turn.over() {
                return Step::More;
            }
        }
        Step::Done(std::mem::take(out))
    })
    .await
}

/// Hands `job` to the compute pool by `submit`. At the pool's bound it waits, as [`blocking_in`] does
/// for a full share, and asks again with the job handed back; with no pool running it runs the job by
/// `alone` on a thread of its own.
async fn to_compute<J: Send + 'static>(mut job: J, submit: impl Fn(J) -> Result<(), hyper_rt::compute::Refused<J>>, alone: fn(J)) -> anyhow::Result<()> {
    let mut delay = Duration::from_millis(1);
    loop {
        match submit(job) {
            Ok(()) => return Ok(()),
            Err(refused) if matches!(refused.error, RtError::Capacity { .. }) => {
                job = refused.job;
                sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            Err(refused) => {
                tracing::warn!(error = %refused.error, "compute pool unavailable; running on a thread of its own");
                let job = refused.job;
                std::thread::Builder::new().name("kiln-compute".into()).spawn(move || alone(job)).context("spawn a compute thread")?;
                return Ok(());
            }
        }
    }
}

/// Run `f` on a thread of its own, not the pool's: for a job that mostly waits on other work, for
/// seconds to a minute (an MCP program waiting on its calls), so it holds none of the pool's few
/// places while it waits (the work it waits on may need them).
pub async fn on_thread<R, F>(name: &str, f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel::<R>();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .context("spawn a thread")?;
    rx.await.map_err(|_| anyhow::anyhow!("the job's thread ended without a result (it panicked)"))
}

/// Gives the shard to its other tasks once, where any of them want it (hyper-rt's `maybe_yield`,
/// after Seastar's): a loop whose waits are all ready at once (a channel that always has the next
/// item) otherwise holds its shard for as long as they are, and every socket and timer on it waits,
/// the HTTP listener among them. Where nothing else wants the shard it goes on at once.
pub async fn maybe_yield() {
    hyper_rt::futures::maybe_yield().await;
}

/// A loop's turns on its shard: it works on for as long as one of the shard's steps may (its step
/// quantum: hyper-rt's online estimate of what a wake costs, else its configured step budget), then
/// gives the shard to its other tasks where they want it ([`maybe_yield`]).
pub struct Turns(Instant);

impl Turns {
    pub fn new() -> Self {
        Self(Instant::now())
    }

    /// After a piece of the loop's work: its turn given up once it has had a step's quantum.
    pub async fn after(&mut self) {
        let quantum = hyper_rt::futures::step_budget_ns().map(Duration::from_nanos);
        if quantum.is_some_and(|q| self.0.elapsed() >= q) {
            maybe_yield().await;
            self.0 = Instant::now();
        }
    }
}

impl Default for Turns {
    fn default() -> Self {
        Self::new()
    }
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

/// `f` of each item, at most `n` at once, all on this task: each future polled under this task's
/// own waker, which hyper-rt's I/O futures require (futures' `buffered` and `FuturesUnordered`
/// poll under wakers of their own, and a connect there is refused). The outputs in the items'
/// order.
pub async fn each<T, R, Fut: Future<Output = R>>(items: impl IntoIterator<Item = T>, n: usize, f: impl Fn(T) -> Fut) -> Vec<R> {
    let mut items = items.into_iter().enumerate();
    let n = n.max(1);
    let mut running: Vec<(usize, Pin<Box<Fut>>)> = Vec::with_capacity(n);
    let mut done: Vec<(usize, R)> = Vec::new();
    std::future::poll_fn(|cx| {
        loop {
            while running.len() < n {
                let Some((i, t)) = items.next() else { break };
                running.push((i, Box::pin(f(t))));
            }
            if running.is_empty() {
                return Poll::Ready(());
            }
            let before = running.len();
            let mut k = 0;
            while k < running.len() {
                let polled = match running.get_mut(k) {
                    Some((_, fut)) => fut.as_mut().poll(cx),
                    None => break,
                };
                match polled {
                    Poll::Ready(r) => {
                        let (i, _) = running.swap_remove(k);
                        done.push((i, r));
                    }
                    Poll::Pending => k += 1,
                }
            }
            // Nothing finished: each future pending has this task's waker.
            if running.len() == before {
                return Poll::Pending;
            }
        }
    })
    .await;
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

/// As [`each`], each output handed to `done` in the items' order as soon as it and every one before
/// it are ready; an item is begun (`f` called) only once one of the `n` places is free, after
/// `done` had every output ready before it, so what `done` changes `f` sees. Outputs ready before
/// their turn hold a place: `n` at most are begun and not yet handed over.
pub async fn each_in_order<T, R, Fut: Future<Output = R>>(items: impl IntoIterator<Item = T>, n: usize, f: impl Fn(T) -> Fut, mut done: impl FnMut(R)) {
    let mut items = items.into_iter().enumerate();
    let n = n.max(1);
    let mut running: Vec<(usize, Pin<Box<Fut>>)> = Vec::with_capacity(n);
    let mut ready: std::collections::BTreeMap<usize, R> = std::collections::BTreeMap::new();
    let mut next = 0usize;
    std::future::poll_fn(|cx| {
        loop {
            while let Some(r) = ready.remove(&next) {
                done(r);
                next += 1;
            }
            while running.len() + ready.len() < n {
                let Some((i, t)) = items.next() else { break };
                running.push((i, Box::pin(f(t))));
            }
            if running.is_empty() {
                return Poll::Ready(());
            }
            let before = running.len();
            let mut k = 0;
            while k < running.len() {
                let polled = match running.get_mut(k) {
                    Some((_, fut)) => fut.as_mut().poll(cx),
                    None => break,
                };
                match polled {
                    Poll::Ready(r) => {
                        let (i, _) = running.swap_remove(k);
                        ready.insert(i, r);
                    }
                    Poll::Pending => k += 1,
                }
            }
            // Nothing finished: each future pending has this task's waker.
            if running.len() == before {
                return Poll::Pending;
            }
        }
    })
    .await;
}

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

    /// Goal 8 (plan 01a1a59c2b29 step 6): with cores of its own the server's shards each get one,
    /// the first core kept for its other threads; more shards than cores go around again, and a
    /// single core takes them all.
    #[test]
    fn g8_s6_shards_are_fixed_to_cores_of_the_servers_own() {
        assert_eq!(shard_cores(&[8, 9, 10, 11], 2), vec![9, 10]);
        assert_eq!(shard_cores(&[8, 9, 10], 4), vec![9, 10, 9, 10]);
        assert_eq!(shard_cores(&[5], 2), vec![5, 5]);
        assert!(shard_cores(&[], 2).is_empty());
    }

    /// `each` keeps the items' order, runs no more than `n` at once, runs them at once (four
    /// 50 ms waits take ~one), and polls them under the task's own waker: a hyper-rt connect inside
    /// it works, as it would not under futures' `buffered`.
    #[test]
    fn each_runs_n_at_once_in_order_under_the_task_s_own_waker() -> anyhow::Result<()> {
        block_on(async {
            let running = Arc::new(AtomicUsize::new(0));
            let most = Arc::new(AtomicUsize::new(0));
            let t = Instant::now();
            let out = each(0..12u64, 4, |i| {
                let (running, most) = (running.clone(), most.clone());
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    // Later items finish first: the order kept is the items', not the finishing.
                    sleep(Duration::from_millis(50 - (i % 4) * 10)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    i * 10
                }
            })
            .await;
            assert_eq!(out, (0..12).map(|i| i * 10).collect::<Vec<_>>());
            assert_eq!(most.load(Ordering::SeqCst), 4);
            assert!(t.elapsed() < Duration::from_millis(400), "three rounds of 50 ms, not twelve: {:?}", t.elapsed());
            assert!(each(Vec::<u8>::new(), 3, |x| async move { x }).await.is_empty());
            // A connect under `each`: refused by the port, not by the runtime.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = listener.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let http = crate::http::client::Client::public()?;
            let url = format!("http://{addr}/");
            let got = each([0u8, 1], 2, |_| async { http.get(&url).timeout(Duration::from_millis(300)).send().await.map(|_| ()).map_err(|e| format!("{e:#}")) }).await;
            for r in &got {
                assert!(r.as_ref().err().is_none_or(|e| !e.contains("not on a shard thread")), "{r:?}");
            }
            Ok(())
        })
    }

    /// Outputs handed over in the items' order as they become ready, not all at the end; an item
    /// begun only once a place is free and what came before was handed over (so it sees what the
    /// handing changed); `n` at once at the most, ready-and-waiting ones counted; a connect works.
    #[test]
    fn each_in_order_hands_each_over_in_turn_and_begins_the_next_after() -> anyhow::Result<()> {
        block_on(async {
            let running = Arc::new(AtomicUsize::new(0));
            let most = Arc::new(AtomicUsize::new(0));
            let handed = Mutex::new(Vec::<u64>::new());
            // What each item saw handed over when it was begun.
            let seen = Mutex::new(Vec::<(u64, usize)>::new());
            let t = Instant::now();
            each_in_order(
                0..12u64,
                3,
                |i| {
                    let (running, most) = (running.clone(), most.clone());
                    seen.lock().unwrap_or_else(PoisonError::into_inner).push((i, handed.lock().unwrap_or_else(PoisonError::into_inner).len()));
                    async move {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        // Later items finish first: the order handed over is the items'.
                        sleep(Duration::from_millis(40 - (i % 3) * 10)).await;
                        running.fetch_sub(1, Ordering::SeqCst);
                        i * 10
                    }
                },
                |r| handed.lock().unwrap_or_else(PoisonError::into_inner).push(r),
            )
            .await;
            assert_eq!(*handed.lock().unwrap_or_else(PoisonError::into_inner), (0..12).map(|i| i * 10).collect::<Vec<_>>());
            assert!(most.load(Ordering::SeqCst) <= 3);
            // Three places: item i is begun only once i - 2 were handed over.
            for (i, before) in seen.lock().unwrap_or_else(PoisonError::into_inner).iter() {
                assert!(*before as u64 + 2 >= *i, "item {i} begun with {before} handed over");
            }
            assert!(t.elapsed() < Duration::from_millis(600), "about four rounds of 40 ms: {:?}", t.elapsed());
            let none = Mutex::new(0);
            each_in_order(Vec::<u8>::new(), 3, |x| async move { x }, |_| *none.lock().unwrap_or_else(PoisonError::into_inner) += 1).await;
            assert_eq!(*none.lock().unwrap_or_else(PoisonError::into_inner), 0);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = listener.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let http = crate::http::client::Client::public()?;
            let url = format!("http://{addr}/");
            let got = Mutex::new(Vec::new());
            each_in_order(
                [0u8, 1],
                2,
                |_| async { http.get(&url).timeout(Duration::from_millis(300)).send().await.map(|_| ()).map_err(|e| format!("{e:#}")) },
                |r| got.lock().unwrap_or_else(PoisonError::into_inner).push(r),
            )
            .await;
            for r in got.lock().unwrap_or_else(PoisonError::into_inner).iter() {
                assert!(r.as_ref().err().is_none_or(|e| !e.contains("not on a shard thread")), "{r:?}");
            }
            Ok(())
        })
    }

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

    /// Goal 3, plan 01a1a59c2a8e step 1: CPU-heavy work goes to the compute pool, started with the
    /// runtime: it runs on none of the shards, at a lower OS priority than theirs, and its result comes
    /// back to the task that asked; a job in pieces goes on to its end.
    #[test]
    fn g3_s1_compute_runs_off_the_shards_below_them_and_answers() -> anyhow::Result<()> {
        block_on(async {
            anyhow::ensure!(hyper_rt::compute::running(), "the compute pool started with the runtime");
            let here = hyper_rt::compute::thread_priority();
            let (on_a_shard, there) = compute(|| (on_shard(), hyper_rt::compute::thread_priority())).await?;
            anyhow::ensure!(!on_a_shard, "the job ran on a shard");
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            anyhow::ensure!(there.is_below(here), "the pool's thread at {there}, the shard's at {here}");
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            let _ = (here, there);
            let sum = compute_pieces((0u64, 0u64), |(i, sum), turn| {
                while *i < 1_000_000 {
                    *sum += *i;
                    *i += 1;
                    if turn.over() {
                        return Step::More;
                    }
                }
                Step::Done(*sum)
            })
            .await?;
            anyhow::ensure!(sum == 499_999_500_000, "{sum}");
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
