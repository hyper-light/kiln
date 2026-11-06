//! Where the server's work runs on kiln-rt's shards (goal 7 of the API runtime program, plan
//! `01a1a59c2aea73e487fb6fcfe90c5fcc`, research `01a1a59c2aeb70e9824606865d6762ca`): one of four placements, picked
//! by `[server] placement` (or `KILN_RT_PLACEMENT`), each from the setting alone, so the lab can A/B them on one
//! build (docs/runtime-lab.md, "Goal 7: placement").
//!
//! - **`round-robin`** (today's): each accepted connection goes to the next shard round-robin, past one the
//!   watchdog sees held (`hyper_rt::watch::stalled`), and its requests are served in its task (f1, f2).
//! - **`steer`**: each new connection goes to the less delayed of two shards drawn at random, held ones passed
//!   over (power of two choices, f10); each request on a connection already open (an HTTP/2 stream, an HTTP/1.1
//!   request on a kept-alive connection) runs on its connection's shard unless a shard drawn at random is less
//!   delayed by more than a step quantum, or its own is held. A shard's delay is goal 1's probe's
//!   (`crate::shard_health::delay_now`): how late its latest probe ran, or how overdue its next is. A request is
//!   weighed against its own shard and one other, not two drawn afresh: moving it costs a hop each way between
//!   threads, worth paying only for a wait longer than the quantum, which goal 6 takes as the least wait that is a
//!   queue (`crate::overload`). Drawing at random rather than taking the least delayed of all is what keeps
//!   placers acting on the same stale reading from all piling onto one shard (f10).
//! - **`steal`**: connections as `round-robin`; each request's handler is a movable task (`hyper_rt::movable`) on
//!   its connection's shard, which an idle shard takes from a busy one's queue, and a held shard's queue is
//!   handed to the others (f5, f7, f9, f14 to f16). The connection's socket and timers stay on its shard.
//! - **`both`**: connections steered, requests movable.
//!
//! A WebSocket is placed with its upgrade request: its task is spawned where that request's handler ran, and
//! serves its frames there. Its heavy messages, a viewer's catch-ups, are made off the shards (the blocking
//! pool, goal 3) from state its session's owner keeps (goal 4), so no message of one is heavy on its shard.
//!
//! `round-robin` is the default: the lab's A/B under CPU hogs in the production-like layout found no other
//! placement better for people's p99 beyond noise (docs/runtime-lab.md, "Goal 7: placement"), and the others
//! stay behind the setting for later A/B tests. Each placement is counted on `/metrics`
//! (`kiln_placement_total`), with what the movable tasks did (`kiln_rt_movable_*`).

use std::cell::Cell;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use hyper_rt::coop::Class;
use serde::{Deserialize, Serialize};

/// How the server places its work on shards (the module's notes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    /// Connections dealt round-robin, requests served in their connection's task. The default, as the lab's
    /// A/B picked it (step 5): under CPU hogs in the production-like layout no other placement gave people's
    /// requests a better p99 beyond noise (docs/runtime-lab.md, "Goal 7: placement").
    #[default]
    RoundRobin,
    /// Connections and requests steered to the less delayed shard.
    Steer,
    /// Requests movable: an idle shard takes them from a busy one.
    Steal,
    /// Connections steered, requests movable.
    Both,
}

impl Placement {
    /// Every placement, in the setting's order.
    pub const ALL: [Placement; 4] = [Placement::RoundRobin, Placement::Steer, Placement::Steal, Placement::Both];

    /// The setting's word for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Placement::RoundRobin => "round-robin",
            Placement::Steer => "steer",
            Placement::Steal => "steal",
            Placement::Both => "both",
        }
    }

    /// The placement a setting's word names.
    pub fn parse(word: &str) -> Option<Placement> {
        let word = word.trim();
        Placement::ALL.into_iter().find(|p| p.as_str().eq_ignore_ascii_case(word))
    }

    /// Whether new connections and requests are steered.
    pub fn steers(self) -> bool {
        matches!(self, Placement::Steer | Placement::Both)
    }

    /// Whether requests are movable tasks.
    pub fn steals(self) -> bool {
        matches!(self, Placement::Steal | Placement::Both)
    }

    fn index(self) -> usize {
        Placement::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

impl std::fmt::Display for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The placement in force: its index in [`Placement::ALL`], or [`UNSET`].
static MODE: AtomicU8 = AtomicU8::new(UNSET);

/// Format: no placement set yet.
const UNSET: u8 = u8::MAX;

/// Sets the placement: the server's, from `[server] placement` (which `KILN_RT_PLACEMENT` overrides), as it starts.
pub fn set(p: Placement) {
    MODE.store(u8::try_from(p.index()).unwrap_or(0), Ordering::Relaxed);
    tracing::info!(placement = %p, "the server places its work on shards: {}", what(p));
}

/// What a placement does, for the log.
fn what(p: Placement) -> &'static str {
    match p {
        Placement::RoundRobin => "connections round-robin, requests in their connection's task",
        Placement::Steer => "connections and requests to the less delayed of two shards",
        Placement::Steal => "connections round-robin, requests as movable tasks idle shards take",
        Placement::Both => "connections to the less delayed of two shards, requests as movable tasks",
    }
}

/// The placement in force: the one set, else `KILN_RT_PLACEMENT`'s (a process that reads no server config), else
/// `round-robin`.
pub fn get() -> Placement {
    let mode = MODE.load(Ordering::Relaxed);
    if mode != UNSET {
        return Placement::ALL.get(usize::from(mode)).copied().unwrap_or_default();
    }
    let p = std::env::var("KILN_RT_PLACEMENT").ok().as_deref().and_then(Placement::parse).unwrap_or_default();
    MODE.store(u8::try_from(p.index()).unwrap_or(0), Ordering::Relaxed);
    p
}

// ------------------------------------------------------------------------------------- the picks

/// What a pick knows of one shard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// Its delay now, nanoseconds (0 where none is known).
    pub delay_ns: u64,
    /// Whether the watchdog sees it held in one poll past its limit.
    pub stalled: bool,
}

/// A number below `n` from `draw`'s next word (Lemire's multiply-shift, its bias at most n / 2^64).
fn below(n: usize, draw: &mut impl FnMut() -> u64) -> usize {
    let wide = u128::from(draw()).saturating_mul(n as u128) >> 64;
    usize::try_from(wide).unwrap_or(0)
}

/// The less delayed of two different shards of `n` drawn at random (the first on a tie), held ones never drawn
/// while any other is not held; `None` with no shard. `look(i)` says what is seen of shard `i`.
pub fn pick_two(n: usize, look: impl Fn(usize) -> Seen, draw: &mut impl FnMut() -> u64) -> Option<usize> {
    let free: Vec<usize> = (0..n).filter(|i| !look(*i).stalled).collect();
    // Every shard held: any of them, as round-robin passes over held ones only while others aren't.
    let pool: Vec<usize> = if free.is_empty() { (0..n).collect() } else { free };
    let first = *pool.get(below(pool.len(), draw))?;
    if pool.len() < 2 {
        return Some(first);
    }
    // The second from the others: `below(len - 1)`, stepped past the first.
    let at = below(pool.len() - 1, draw);
    let second = pool.iter().copied().filter(|i| *i != first).nth(at).unwrap_or(first);
    Some(if look(second).delay_ns < look(first).delay_ns { second } else { first })
}

/// A request's shard, its connection's being `local` of `n`: `local`, unless one other shard drawn at random is not
/// held and is less delayed by more than `margin_ns`, or `local` is held and the other is not.
pub fn pick_from(local: usize, n: usize, margin_ns: u64, look: impl Fn(usize) -> Seen, draw: &mut impl FnMut() -> u64) -> usize {
    if n < 2 || local >= n {
        return local;
    }
    let other = below(n - 1, draw);
    let other = if other >= local { other + 1 } else { other };
    let (here, there) = (look(local), look(other));
    if there.stalled {
        return local;
    }
    if here.stalled || there.delay_ns.saturating_add(margin_ns) < here.delay_ns {
        other
    } else {
        local
    }
}

thread_local! {
    /// This thread's draws (xorshift64*), seeded from the clock and the thread's own cell.
    static DRAWS: Cell<u64> = const { Cell::new(0) };
}

/// A random word (Vigna's xorshift64*): not for anything secret, only for drawing shards.
fn draw() -> u64 {
    DRAWS.with(|cell| {
        let mut x = cell.get();
        if x == 0 {
            let at = std::ptr::from_ref(cell).addr() as u64;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
            x = (at ^ now) | 1;
        }
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        cell.set(x);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}

/// What is seen of shard `i` of the runtime now.
fn seen(holders: &[hyper_rt::SlotHolder], i: usize) -> Seen {
    Seen {
        delay_ns: crate::shard_health::delay_now(i).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)),
        stalled: holders.get(i).is_some_and(|h| hyper_rt::watch::stalled(*h)),
    }
}

/// The shard a new connection goes to under steering: its registration, or `None` with no runtime.
pub fn connection_shard() -> Option<hyper_rt::SlotHolder> {
    let holders = crate::rt::shard_holders_ref();
    let i = pick_two(holders.len(), |i| seen(holders, i), &mut draw)?;
    count(Placed::Steered);
    holders.get(i).copied()
}

// ---------------------------------------------------------------------------------------- requests

/// What became of a placed piece of work, counted on `/metrics`.
#[derive(Clone, Copy, Debug)]
enum Placed {
    /// A new connection placed by two choices.
    Steered,
    /// A request run where its connection is.
    Local,
    /// A request sent to another shard.
    Moved,
    /// A request made a movable task.
    Movable,
    /// A request run in its connection's task because the shard it was placed on refused it.
    Refused,
}

static PLACED: [AtomicU64; 5] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

fn count(p: Placed) {
    if let Some(n) = PLACED.get(p as usize) {
        n.fetch_add(1, Ordering::Relaxed);
    }
}

/// Aborts a placed task when the request it serves goes away first (its connection closed), unless disarmed.
struct AbortOnDrop(Option<futures::future::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Runs `fut`, a request's handler, where the placement says (the module's notes), its task starting in `class`: in
/// the caller's task (`round-robin`, or a request steering keeps where it is), as a task on another shard
/// (`steer`), or as a movable task (`steal`, `both`). Its output; `None` when the task it was placed in ended
/// without one (its runtime stopping). Dropped before it ends, the placed task is aborted at its next poll.
///
/// `demoted`: the request lowered the caller's task to `class` from a more urgent one, its head read in that
/// class's turn (`crate::overload::Serving::demoted`; goal 6, plan 01a1a59c2abe step 3). Kept in the caller's task,
/// it first gives way to a more urgent task waiting on the shard (`hyper_rt::coop::give_way`), so a person's page
/// woken meanwhile goes first; else it ran on in Critical's turn, and a page queued behind every API call read
/// before it. With none waiting it goes on at once, its place kept. Placed apart, its task starts in `class`.
pub async fn request<F>(class: Class, demoted: bool, fut: F) -> Option<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    request_as(get(), class, demoted, fut).await
}

/// [`request`] under placement `p`, whatever the setting.
pub async fn request_in<F>(p: Placement, class: Class, fut: F) -> Option<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    request_as(p, class, false, fut).await
}

/// [`request`] under placement `p`.
async fn request_as<F>(p: Placement, class: Class, demoted: bool, fut: F) -> Option<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if p.steals() && hyper_rt::movable::available() {
        return placed(fut, |task| hyper_rt::movable::spawn(class, task).is_ok(), Placed::Movable).await;
    }
    if p.steers() && let Some(holder) = steered_away() {
        return placed(fut, |task| hyper_rt::runtime::submit_movable_as(holder, class, task).is_ok(), Placed::Moved).await;
    }
    count(Placed::Local);
    if demoted {
        hyper_rt::coop::give_way().await;
    }
    Some(fut.await)
}

/// The other shard a request on this one goes to under steering, when it should move (`pick_from`).
fn steered_away() -> Option<hyper_rt::SlotHolder> {
    let here = hyper_rt::futures::shard_id()?.0;
    let holders = crate::rt::shard_holders_ref();
    let local = holders.iter().position(|h| h.shard() == here)?;
    let margin = hyper_rt::futures::step_budget_ns().unwrap_or(crate::rt::STEP_BUDGET_NS);
    let to = pick_from(local, holders.len(), margin, |i| seen(holders, i), &mut draw);
    (to != local).then(|| holders.get(to).copied()).flatten()
}

/// `fut` in a task `submit` places (refused: back, and run here), its output back over a one-shot.
async fn placed<F>(fut: F, submit: impl FnOnce(crate::rt::Boxed) -> bool, how: Placed) -> Option<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel::<F::Output>();
    let (fut, abort) = futures::future::abortable(fut);
    let slot = crate::rt::handoff(Box::pin(async move {
        if let Ok(out) = fut.await {
            let _ = tx.send(out);
        }
    }));
    let mut guard = AbortOnDrop(Some(abort));
    if submit(Box::pin(crate::rt::bootstrap(&slot))) {
        count(how);
    } else {
        // Refused: the task never took it, so it is still in its slot; run here.
        count(Placed::Refused);
        crate::rt::bootstrap(&slot).await;
    }
    let out = rx.await.ok();
    guard.0 = None;
    out
}

// ----------------------------------------------------------------------------------------- metrics

/// `kiln_placement_*` and `kiln_rt_movable_*`, in the exposition.
pub fn render(o: &mut String) {
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    help(o, "kiln_placement_info", "gauge", "The placement in force (goal 7): round-robin, steer, steal or both, by the label.");
    let _ = writeln!(o, "kiln_placement_info{{placement=\"{}\"}} 1", get());
    help(o, "kiln_placement_total", "counter", "Work placed on shards, by how: steered (a new connection, by two choices), local (a request kept on its connection's shard), moved (sent to a less delayed shard), movable (a movable task), refused (the shard placed on refused it: run in its connection's task).");
    for (how, p) in [("steered", Placed::Steered), ("local", Placed::Local), ("moved", Placed::Moved), ("movable", Placed::Movable), ("refused", Placed::Refused)] {
        let n = PLACED.get(p as usize).map_or(0, |n| n.load(Ordering::Relaxed));
        let _ = writeln!(o, "kiln_placement_total{{how=\"{how}\"}} {n}");
    }
    if let Some(s) = crate::rt::movable_stats() {
        for (name, h, v) in [
            ("kiln_rt_movable_spawned_total", "Movable tasks spawned (requests under steal and both).", s.spawned),
            ("kiln_rt_movable_polls_total", "Polls of movable tasks.", s.polls),
            ("kiln_rt_movable_stolen_total", "Movable tasks an idle shard took from a sibling's queue.", s.stolen),
            ("kiln_rt_movable_shared_total", "Movable tasks shards took from the shared queues (woken off the shards, or overflowed).", s.from_shared),
            ("kiln_rt_movable_overflowed_total", "Movable tasks a full shard queue moved to the shared queue.", s.overflowed),
            ("kiln_rt_movable_handed_off_total", "Movable tasks the watchdog moved off a held shard's queue.", s.handed_off),
            ("kiln_rt_movable_notified_total", "Parked shards kicked to come and take queued movable tasks.", s.notified),
        ] {
            help(o, name, "counter", h);
            let _ = writeln!(o, "{name} {v}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Marsaglia's xorshift64 from `seed`: draws for a test, the same each run.
    fn draws(seed: u64) -> impl FnMut() -> u64 {
        let mut x = seed | 1;
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        }
    }

    /// Step 3: each placement from its setting's word, case and spaces aside; nothing else is one.
    #[test]
    fn g7_s3_each_placement_from_its_word() {
        for p in Placement::ALL {
            assert_eq!(Placement::parse(p.as_str()), Some(p));
            assert_eq!(Placement::parse(&format!(" {} ", p.as_str().to_uppercase())), Some(p));
        }
        assert_eq!(Placement::parse("roundrobin"), None);
        assert_eq!(Placement::parse(""), None);
        assert_eq!(Placement::default(), Placement::RoundRobin);
        assert!(Placement::Steer.steers() && !Placement::Steer.steals());
        assert!(Placement::Steal.steals() && !Placement::Steal.steers());
        assert!(Placement::Both.steals() && Placement::Both.steers());
        assert!(!Placement::RoundRobin.steals() && !Placement::RoundRobin.steers());
    }

    /// Step 1: of two shards, the less delayed always (both are drawn); of four, never the most delayed (it loses to
    /// whichever other is drawn with it), and more than one of the rest; a held shard never while another isn't;
    /// with every shard held, one of them.
    #[test]
    fn g7_s1_two_choices_take_the_less_delayed_and_pass_over_held_shards() {
        let mut draw = draws(7);
        let two = [3_000_000u64, 100];
        for _ in 0..100 {
            assert_eq!(pick_two(2, |i| Seen { delay_ns: two[i], stalled: false }, &mut draw), Some(1));
        }
        let four = [5_000_000u64, 100, 2_000_000, 0];
        let mut picked = [0usize; 4];
        for _ in 0..1000 {
            let i = pick_two(4, |i| Seen { delay_ns: four[i], stalled: false }, &mut draw).unwrap_or(9);
            picked[i] += 1;
        }
        assert_eq!(picked[0], 0, "the most delayed never: {picked:?}");
        assert!(picked.iter().filter(|n| **n > 0).count() > 1, "{picked:?}");
        // Shard 3, the least delayed, held: never picked while others aren't.
        for _ in 0..1000 {
            assert_ne!(pick_two(4, |i| Seen { delay_ns: four[i], stalled: i == 3 }, &mut draw), Some(3));
        }
        assert!(pick_two(4, |i| Seen { delay_ns: four[i], stalled: true }, &mut draw).is_some());
        assert_eq!(pick_two(1, |_| Seen::default(), &mut draw), Some(0));
        assert_eq!(pick_two(0, |_| Seen::default(), &mut draw), None);
    }

    /// Step 1: a request stays where its connection is unless the other drawn is less delayed by more than the
    /// margin, or its own shard is held; never to a held shard.
    #[test]
    fn g7_s1_a_request_moves_only_for_more_than_a_quantum() {
        let margin = 250_000;
        let mut draw = draws(3);
        let mut pick = |delays: [u64; 2], stalled: [bool; 2]| pick_from(0, 2, margin, |i| Seen { delay_ns: delays[i], stalled: stalled[i] }, &mut draw);
        assert_eq!(pick([300_000, 100_000], [false, false]), 0, "within the margin: stays");
        assert_eq!(pick([600_000, 100_000], [false, false]), 1, "past it: moves");
        assert_eq!(pick([0, 9_000_000], [true, false]), 1, "its own held: moves");
        assert_eq!(pick([9_000_000, 0], [false, true]), 0, "the other held: stays");
        assert_eq!(pick_from(0, 1, margin, |_| Seen::default(), &mut draw), 0, "one shard");
        assert_eq!(pick_from(5, 2, margin, |_| Seen::default(), &mut draw), 5, "a shard past the runtime's");
    }

    /// Step 5: the default is the placement the lab's A/B picked (docs/runtime-lab.md, "Goal 7: placement"):
    /// round-robin, the server's own default too.
    #[test]
    fn g7_s5_the_default_is_the_ab_s_pick() {
        assert_eq!(Placement::default(), Placement::RoundRobin);
        assert_eq!(crate::config::Config::default().server.placement, Placement::RoundRobin);
    }

    /// Step 3: the setting from `[server] placement`, round-robin when it isn't given, and a word that names no
    /// placement refused.
    #[test]
    fn g7_s3_the_setting_from_the_server_config() -> anyhow::Result<()> {
        for p in Placement::ALL {
            let cfg: crate::config::Config = toml::from_str(&format!("[server]\nplacement = \"{p}\"\n"))?;
            assert_eq!(cfg.server.placement, p);
        }
        let cfg: crate::config::Config = toml::from_str("[server]\nshed = true\n")?;
        assert_eq!(cfg.server.placement, Placement::RoundRobin);
        assert!(toml::from_str::<crate::config::Config>("[server]\nplacement = \"sideways\"\n").is_err());
        Ok(())
    }

    /// Step 3: each placement serves a request from its setting alone, on the runtime as it is (the work runs
    /// somewhere and its output comes back).
    #[test]
    fn g7_s3_each_placement_serves_a_request() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for p in Placement::ALL {
                for class in [Class::Critical, Class::Normal, Class::Background] {
                    let out = request_in(p, class, async move { crate::rt::sleep(std::time::Duration::from_millis(1)).await; p.as_str().len() }).await;
                    anyhow::ensure!(out == Some(p.as_str().len()), "{p} ({class:?}) answered {out:?}");
                }
            }
            Ok(())
        })
    }

    /// Goal 6, plan 01a1a59c2abe step 3: a request that lowered its connection's task from Critical (its head read
    /// in Critical's turn) and stays in that task gives way before its handler runs to a Critical task woken
    /// meanwhile on the shard (a page's connection, its socket ready), which goes first; not demoted, the handler
    /// runs at once, ahead of it. Do: on one shard, the connection's task at Critical starts a Critical task that
    /// waits, wakes it, lowers itself to Normal and runs a handler, demoted and then not. Expect: the Critical
    /// task's mark before the handler's, then after it.
    #[test]
    fn g6_s3_a_demoted_request_gives_way_to_a_woken_critical_task() -> anyhow::Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        let mut shard = hyper_rt::runtime::LocalRuntime::new(&hyper_rt::runtime::RuntimeConfig {
            shards: 1,
            tasks_per_shard: 16,
            timers_per_shard: 16,
            interests_per_shard: 32,
            ring_entries: 16,
            step_budget_ns: crate::rt::STEP_BUDGET_NS,
            timer_tick_ns: 50_000,
            batch: 16,
            pin: false,
            cores: Vec::new(),
            page_bytes: 4096,
            spin_ns: 0,
            wake_tracking: None,
            latency_objective_ns: None,
        })
        .map_err(|e| anyhow::anyhow!("a shard: {e}"))?;
        let order = shard.block_on(async {
            // Each mark's place: the woken Critical task's (`k`) and the handler's (`h`), twice.
            let at = Arc::new(AtomicUsize::new(0));
            let mut marks = Vec::new();
            for demoted in [true, false] {
                let _ = hyper_rt::coop::set_class(Class::Critical);
                let (wake, woken) = hyper_rt::sync::oneshot::<()>().map_err(|e| anyhow::anyhow!("{e}"))?;
                let (k, h) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
                let (waiting, at_k) = (Arc::clone(&k), Arc::clone(&at));
                let spawned = hyper_rt::futures::spawn(async move {
                    let _ = woken.await;
                    waiting.store(at_k.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
                });
                anyhow::ensure!(spawned.is_ok(), "spawned: {spawned:?}");
                // Its first poll, in which it waits.
                hyper_rt::futures::yield_now().await;
                anyhow::ensure!(wake.send(()).is_ok(), "woken");
                // The request read lowers the task to its own class.
                let _ = hyper_rt::coop::set_class(Class::Normal);
                let (handled, at_h) = (Arc::clone(&h), Arc::clone(&at));
                let out = request_as(Placement::RoundRobin, Class::Normal, demoted, async move {
                    handled.store(at_h.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
                    7
                })
                .await;
                anyhow::ensure!(out == Some(7), "answered {out:?}");
                // The Critical task's turn comes by now either way.
                hyper_rt::futures::yield_now().await;
                marks.push((demoted, k.load(Ordering::Relaxed), h.load(Ordering::Relaxed)));
            }
            Ok::<_, anyhow::Error>(marks)
        });
        let marks = order.map_err(|e| anyhow::anyhow!("the shard: {e}"))??;
        anyhow::ensure!(marks.len() == 2, "{marks:?}");
        for (demoted, k, h) in marks {
            anyhow::ensure!(k > 0 && h > 0, "both ran: {k}, {h}");
            if demoted {
                anyhow::ensure!(k < h, "demoted, the handler gave way to the Critical task: k {k}, h {h}");
            } else {
                anyhow::ensure!(h < k, "not demoted, the handler ran at once: k {k}, h {h}");
            }
        }
        Ok(())
    }

    /// Step 1's acceptance, on the picks: with one shard busy (its delay far past the others', then held too), no
    /// new connection goes to it, and every request on its connections moves off it.
    #[test]
    fn g7_s1_with_one_shard_busy_new_work_goes_to_the_others() {
        let mut draw = draws(0x9e37_79b9);
        for stalled in [false, true] {
            let look = |i: usize| Seen { delay_ns: if i == 0 { 80_000_000 } else { 50_000 }, stalled: stalled && i == 0 };
            for _ in 0..1000 {
                assert_ne!(pick_two(4, look, &mut draw), Some(0), "a new connection on the busy shard (held: {stalled})");
                assert_ne!(pick_from(0, 4, 250_000, look, &mut draw), 0, "a request stayed on the busy shard (held: {stalled})");
            }
        }
    }
}
