//! Shards that protect themselves, kiln's side (goal 6, plan 01a1a59c2abe): the class of every request (step
//! 3), shedding by class when a shard's queue or a pool's line stands past its calm (step 4), and what kiln-rt's
//! watchdog sees, on `/metrics` and in the log (step 2).
//!
//! **Classes** (`hyper_rt::coop::Class`). Each request is Critical (health, sign-in, a person's pages and
//! actions), Normal (the API) or Background (agents' and CI's polling, catch-ups, sync, telemetry), read from
//! its method, its path and the credentials it carries ([`class_of`]). The connection's task runs at the most
//! urgent class it serves now ([`Connection`]: an HTTP/2 connection serves many requests in one task), so its
//! shard gives it its class's share and urgency. The credentials are read, not checked: a forged cookie buys
//! a request a place ahead and nothing past its own authentication, which it meets as ever.
//!
//! **Shedding.** Goal 1's probe on each shard (`crate::shard_health`) ticks the shard's watches every period
//! ([`tick`]), with no probe of this module's own: the API's (Normal) reads the shard's delay, how late the
//! probe, a Normal task, ran; Background's reads what the shard's Background tasks waited from joining a ready
//! list to their polls (`Counters::queue_ns_background`). Each watch learns its queue's calm, the typical wait (the
//! geometric mean) of its periods outside its standing, and sheds only on a queue that *stands*, as CoDel bounds
//! its queues (research 01a1a59c2abf, f14): a queue that sits past its calm for a whole interval is one the server
//! falls behind; one that comes back is not, however late it ran in between. Calm is bursty: in the lab's calm
//! (fedora, 2026-11-05, research f32) the store share's jobs waited 307 to 620 ms at p99, one request's reads queued
//! behind each other while the share held 3% of its places on average, and every round's shards ran a probe 250 ms
//! late; the CUSUM this replaced read those as shifts and shed 6.5 Background requests a calm round. Every number
//! is the platform's or the shard's own, not picked here:
//! - **The interval**, a tolerance: Kubernetes' default probe timeout, 1 s
//!   (`crate::headroom::settings::PLATFORM_TOLERANCE_MS`), past which a request waiting fails a health probe of that
//!   timeout (research f18: starvation that fails health checks cascades), and the soonest `Retry-After` brings a
//!   shed client back (HTTP's unit, a second).
//! - **Past its calm**: a period's wait past the calm and past the floor, the shard's quantum (a shorter wait is no
//!   queue: a task waits that long behind the poll running as it woke, whatever the load; a probe's floor adds its
//!   timer's tick, a lateness the shard's timers have by design).
//! - **A shard's queue stands** while most of the last tolerance's periods were past its calm (its median wait over
//!   a tolerance past its calm) and, where the OS keeps a thread's CPU time (Linux, macOS), the shard ran most of
//!   that tolerance. Most, not every: a shard 82% busy under the lab's flood, its probes 6 ms late at the median and
//!   pages 16 ms, still found some probes idle each second, and a test of every period stood in 3 windows of 12
//!   rounds. While it runs most of the time: a probe lands at any moment, so most can wait behind the shard's own
//!   work only if it runs most of the time; a mostly idle shard whose wakes the host makes late (a hog holding the
//!   CPU to the scheduler's tick on a kernel that preempts lazily) is not overloaded by its own work, and shedding
//!   would not shorten its waits.
//! - **A pool's line stands** once every period for a tolerance was past its calm, the line never emptying, and work
//!   that came after it formed kept it standing (the pools, below).
//! - **Background is shed** once a queue stands; **then Normal**, once it has stood another tolerance with
//!   Background shed: shedding Background did not drain it. Critical never. Shedding stops once the queue no longer
//!   stands.
//!
//! A shard sheds Background while either of its watches stands, and Normal while the API's has stood its second
//! tolerance: shedding the API frees no time for Background's own queue, the less urgent. A lone late probe, a stall
//! (the watchdog's) and a server's start holding a shard for seconds (one late probe each) never make most of a
//! tolerance. A period's wait is learned into the calm once it leaves the tolerance's window, if the queue never
//! stood while it was in it (a pool's line: once its excursion comes back short of standing), so an overload, its
//! onset with it, never becomes the calm it is measured against.
//!
//! **The pools.** What a request waits on off its shard waits in kiln-rt's pools (goal 3): the compute pool for a
//! thread, each of the blocking pool's shares (`dns`, `store`, `fs`) for one of its places, kiln.db's writer for
//! its turn. No shard's delay shows it (research 01a1a59c2abf, f28: API reads at a 2 to 3 s p99 while the one
//! shard's delay stayed under 2 ms). So the first shard's probe also ticks a watch for each pool ([`tick_pools`]),
//! on the mean wait of the jobs the pool told of over the period (`crate::pool_health`): none told and none
//! waiting, nothing queued; none told while some wait, the watch holds where it was (their waits are told only
//! as they start or end). A pool's line stands only once work that came *after* it formed has kept it standing a
//! whole tolerance: it has stood a tolerance longer than the longest wait its jobs told since it formed. The reads
//! of one request submitted at once (an attach's) wait behind each other exactly as long as the line has stood,
//! so however long they take to work off they shed nothing (sheds there would refuse requests the batch holds up
//! and do nothing to drain it); arrivals that outrun the pool keep its line standing past its jobs' waits. A pool's
//! queue is first come, first served whatever a job's class, so the order is the classes'. The pools serve every
//! shard: a standing pool sheds on every shard, and one watch each, kept by the first shard's thread.
//!
//! Shedding is off until `[server] shed = true` declares it, as what kiln decides on its own is (docs/ci.md
//! §0.3, "Opt-in automation"); every shed says why (its body, `x-kiln-shed`, the log, `/metrics`). The
//! classes and the watches run either way, on `/metrics` and `/readyz`.
//!
//! **What kiln's clients do**: a shed answer did nothing (the handler never ran), so kiln's HTTP client sends
//! the request again after `Retry-After`, within its deadline, its body too when it can be sent again (bytes, or a
//! file read again from its start), and a WebSocket's opening likewise (`crate::http::client`). `/readyz` says
//! what is shed, and answers 503 when every shard sheds the API.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use http::{HeaderMap, Method, StatusCode, header};
pub use hyper_rt::coop::Class;

use crate::http::server::{self as srv, Request, Response};
use crate::pool_health::{self, QUEUES};

/// The session cookie a person's browser carries (`crate::api`'s).
const SESSION_COOKIE: &str = "kiln_session=";

// ------------------------------------------------------------------------------------- classes

/// The class of a request (step 3), from its method, path and the credentials it carries (read, not checked).
pub fn class_of(method: &Method, path: &str, headers: &HeaderMap) -> Class {
    // The first segments, padded with empty ones, and how many there were.
    let mut parts = path.trim_matches('/').split('/').filter(|s| !s.is_empty());
    let mut count = 0;
    let seg: [&str; 5] = std::array::from_fn(|_| match parts.next() {
        Some(part) => {
            count += 1;
            part
        }
        None => "",
    });
    let read = matches!(*method, Method::GET | Method::HEAD);
    let person = person(headers);
    if let Some(class) = health_or_sign_in(read, &seg) {
        return class;
    }
    if background(method, path, &seg, seg.get(..count).unwrap_or(&[]), person) {
        return Class::Background;
    }
    if git(path) {
        return Class::Normal;
    }
    match (seg[0] == "api", read) {
        // A person's pages, and the shell the sign-in page needs: what a browser loads.
        (false, true) if page(&seg) => Class::Critical,
        // A person's actions: changes from a signed-in browser.
        (true, false) if person => Class::Critical,
        _ => Class::Normal,
    }
}

/// Whether the request carries a person's session (the cookie a browser holds) and no token.
fn person(headers: &HeaderMap) -> bool {
    headers.get(header::AUTHORIZATION).is_none()
        && headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.split(';').any(|kv| kv.trim_start().starts_with(SESSION_COOKIE)))
}

/// Health and sign-in: always Critical.
fn health_or_sign_in(read: bool, seg: &[&str; 5]) -> Option<Class> {
    match seg {
        ["healthz" | "readyz" | "metrics", ..] if read => Some(Class::Critical),
        ["api", "auth", ..] => Some(Class::Critical),
        ["api", "me", "", ..] if read => Some(Class::Critical),
        _ => None,
    }
}

/// Git's smart HTTP (served at the root, `<owner>/<repo>.git/...`): clones and pushes, the API's.
fn git(path: &str) -> bool {
    path.contains(".git/") || path.ends_with(".git") || path.contains("/info/refs") || path.ends_with("/git-upload-pack") || path.ends_with("/git-receive-pack")
}

/// Agents' and CI's polling, catch-ups, sync and telemetry: what may wait without harm to a person. `segs` are the
/// path's segments as they are, `seg` the first of them padded.
fn background(method: &Method, path: &str, seg: &[&str; 5], segs: &[&str], person: bool) -> bool {
    let read = matches!(*method, Method::GET | Method::HEAD);
    match seg {
        // Replication and sync between machines.
        ["api", "sync", "blobs" | "bundles", ..] | ["api", "cluster", "copy", ..] => true,
        ["api", "observe", "v1", "snapshot", ..] => true,
        // Telemetry pushed in.
        ["v1", "metrics" | "logs" | "traces" | "firehose" | "pubsub", ..] | ["api", "v1", "write", ..] => true,
        [_, "Export", "", ..] if *method == Method::POST => true,
        // CI's transfers and the fleet's builds and downloads.
        ["api", "jobs", _, "source" | "artifacts", ..] | ["api", "updates", "jobs", ..] | ["dl", ..] => true,
        // A runner's tunnel, a join's polling, agents waiting.
        ["api", "agents", "tunnel", ..] => true,
        ["api", "join-requests", id, ..] if read && !id.is_empty() => true,
        ["api", "harness", "agents", "wait", ..] => true,
        // A machine catching a session up, not a person's page.
        ["api", "harness", "sessions", _, "events"] if read && !person => true,
        _ => crate::observe::ddagent::is_datadog(segs) || crate::ci::registry::is_registry(path) || crate::ci::registry::proxy::is_proxy(path),
    }
}

/// A page a browser loads: anything but the API, the pushes and downloads above, and the files machines fetch.
fn page(seg: &[&str; 5]) -> bool {
    !matches!(seg[0], "v1" | "v2" | "dl" | "install.sh" | "install.ps1" | "ca.pem" | "ca.crt" | "ca.mobileconfig")
}

/// What one connection serves now, by class, so its task runs at the most urgent of them: an HTTP/2
/// connection serves many requests in one task, an HTTP/1.1 one a request at a time. Made per connection
/// (`crate::http::server`).
#[derive(Debug, Default)]
pub struct Connection {
    /// Requests served now, by `Class as usize`.
    serving: [AtomicU32; 3],
}

impl Connection {
    /// A connection's, its task reading its first request (Critical: [`Connection::apply`]).
    pub fn new() -> Arc<Self> {
        let conn = Arc::new(Self::default());
        conn.apply();
        conn
    }

    /// A request begins: classed, its class put on it (an extension the API reads), and the connection's task
    /// set to the most urgent class it now serves. The guard ends it.
    pub fn begin(self: &Arc<Self>, req: &mut Request) -> Serving {
        let class = class_of(req.method(), req.uri().path(), req.headers());
        req.extensions_mut().insert(class);
        if let Some(n) = self.serving.get(class as usize) {
            n.fetch_add(1, Ordering::Relaxed);
        }
        self.apply();
        Serving { conn: Arc::clone(self), class }
    }

    /// The task's class: the most urgent served now; serving none, Critical, since what it does then is read its
    /// next request's head, which costs little and must come before the request's own class is known.
    fn apply(&self) {
        let class = [Class::Critical, Class::Normal, Class::Background]
            .into_iter()
            .find(|class| self.serving.get(*class as usize).is_some_and(|n| n.load(Ordering::Relaxed) > 0))
            .unwrap_or(Class::Critical);
        let _ = hyper_rt::coop::set_class(class);
    }
}

/// A request a [`Connection`] serves, until it drops.
#[derive(Debug)]
pub struct Serving {
    conn: Arc<Connection>,
    class: Class,
}

impl Serving {
    /// The request's class: what its handler's task starts in where it is placed apart (goal 7, `crate::placement`).
    pub fn class(&self) -> Class {
        self.class
    }
}

/// A response's body that keeps its request's [`Serving`] until it ends: a body streamed after the handler
/// returned (a download, a log) goes on at its request's class, not at the class of a connection between
/// requests.
struct Guarded {
    body: srv::Body,
    _serving: Serving,
}

impl http_body::Body for Guarded {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    fn poll_frame(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>> {
        std::pin::Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// `resp`, its body keeping `serving` until it ends ([`Guarded`]).
pub fn while_sent(resp: Response, serving: Serving) -> Response {
    resp.map(|body| http_body_util::BodyExt::boxed(Guarded { body, _serving: serving }))
}

impl Drop for Serving {
    fn drop(&mut self) {
        if let Some(n) = self.conn.serving.get(self.class as usize) {
            n.fetch_sub(1, Ordering::Relaxed);
        }
        self.conn.apply();
    }
}


// ------------------------------------------------------------------------------------- shedding

/// The tolerance, microseconds: how long a queue stands before it sheds, and again before it sheds the API (the
/// module's notes).
const TOLERANCE_US: f64 = crate::headroom::settings::PLATFORM_TOLERANCE_MS as f64 * 1e3;

/// What one shard sheds and why, written by its monitor (on the shard) and read by the requests served there and
/// by `/metrics`.
struct ShardState {
    holder: hyper_rt::SlotHolder,
    /// 0: nothing; 1: Background requests; 2: Normal and Background.
    level: AtomicU8,
    /// Each class's queueing delay over the monitor's last period, its calm (0: not known), and how long its queue
    /// has stood past its calm (0: it is back), nanoseconds, by `Class as usize`.
    delay_ns: [AtomicU64; 3],
    calm_ns: [AtomicU64; 3],
    over_ns: [AtomicU64; 3],
    /// What the shard counted of its tasks yielding at their share (`hyper_rt::coop`), as last read.
    coop_yields: AtomicU64,
}

static SHARDS: OnceLock<Vec<ShardState>> = OnceLock::new();
/// Requests shed, by `Class as usize`.
static SHED: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
/// `[server] shed`: whether a standing queue sheds (it is shown either way).
static ENABLED: AtomicBool = AtomicBool::new(false);

/// What work waiting in the pools sheds, and why, by `pool_health::QUEUES`' order: written by the first shard's
/// probe, which ticks the pools' watches ([`tick_pools`]), and read by every request and by `/metrics`.
struct PoolStates {
    /// As [`ShardState::level`]: 0, nothing; 1, Background requests; 2, Normal and Background.
    level: [AtomicU8; QUEUES.len()],
    /// Each pool's mean wait over the last period its watch read, its calm (0: not known), and how long its line has
    /// stood past its calm (0: it is back or empty), nanoseconds.
    delay_ns: [AtomicU64; QUEUES.len()],
    calm_ns: [AtomicU64; QUEUES.len()],
    over_ns: [AtomicU64; QUEUES.len()],
}

static POOLS: PoolStates = PoolStates {
    level: [const { AtomicU8::new(0) }; QUEUES.len()],
    delay_ns: [const { AtomicU64::new(0) }; QUEUES.len()],
    calm_ns: [const { AtomicU64::new(0) }; QUEUES.len()],
    over_ns: [const { AtomicU64::new(0) }; QUEUES.len()],
};

fn pool_level(i: usize) -> u8 {
    POOLS.level.get(i).map_or(0, |l| l.load(Ordering::Relaxed))
}

fn pool_delay_ns(i: usize) -> u64 {
    POOLS.delay_ns.get(i).map_or(0, |d| d.load(Ordering::Relaxed))
}

/// How the log and the answers name pool `i` of `pool_health::QUEUES`.
fn pool_name(i: usize) -> String {
    match QUEUES.get(i) {
        Some(_) if i == 0 => "the compute pool".to_owned(),
        Some(&pool_health::STORE_WRITE) => "kiln.db's writer".to_owned(),
        Some(share) => format!("the blocking pool's {share} share"),
        None => "a pool".to_owned(),
    }
}

fn shards() -> &'static [ShardState] {
    SHARDS.get().map_or(&[], Vec::as_slice)
}

/// The state of the shard this thread runs, if it runs one of the runtime's.
fn here() -> Option<&'static ShardState> {
    let id = hyper_rt::futures::shard_id()?;
    shards().iter().find(|s| s.holder.shard() == id.0)
}

/// Sets up each shard's shedding (the server's; `enabled` is `[server] shed`): goal 1's probe on each shard
/// ticks it (`crate::shard_health`, [`tick`]). How many shards there are.
pub fn start(enabled: bool) -> usize {
    ENABLED.store(enabled, Ordering::Relaxed);
    let holders = crate::rt::shard_holders();
    let states: Vec<ShardState> = holders
        .iter()
        .map(|holder| ShardState {
            holder: *holder,
            level: AtomicU8::new(0),
            delay_ns: Default::default(),
            calm_ns: Default::default(),
            over_ns: Default::default(),
            coop_yields: AtomicU64::new(0),
        })
        .collect();
    let count = states.len();
    if SHARDS.set(states).is_err() {
        return 0;
    }
    count
}

fn micros_since(base: Instant) -> u64 {
    u64::try_from(base.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// How a queue's watch decides that it stands (the module's notes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    /// A pool's line: every period past its calm, never emptying, and kept standing by work that came after it formed.
    Line,
    /// A shard's queue: most of the last tolerance's periods past its calm, while the shard ran most of it.
    Most,
}

/// One period of a queue, as a [`Standing`] reads it.
#[derive(Clone, Copy, Debug)]
struct Period {
    /// When, microseconds on the watch's own clock.
    now_us: u64,
    /// The period's mean wait, nanoseconds; `None`: none told while some wait (a pool tells a wait as its job
    /// starts or ends), so the queue is where it was.
    wait_ns: Option<u64>,
    /// The longest wait told over the period, nanoseconds (a pool's; a shard's watch has its mean alone).
    longest_ns: u64,
    /// Whether anything waits now: a pool's line holds a job; a shard's queue is read from its waits alone.
    waiting: bool,
    /// The longest wait that is no queue: the shard's quantum (a probe's adds its timer's tick).
    floor_ns: u64,
    rule: Rule,
    /// A shard's: the share of the last tolerance its thread ran on the CPU, where the OS keeps a thread's CPU time.
    busy: Option<f64>,
}

impl Period {
    /// A shard's period: the queue's mean wait `wait_ns` over it (the probe's lateness, or a class's mean wait), and
    /// how much of the last tolerance the shard ran.
    fn of_shard(now_us: u64, wait_ns: u64, floor_ns: u64, busy: Option<f64>) -> Self {
        Self { now_us, wait_ns: Some(wait_ns), longest_ns: wait_ns, waiting: true, floor_ns, rule: Rule::Most, busy }
    }
}

/// One queue's watch (the module's notes): what it learned of its calm and whether its queue stands now. Kept by
/// the thread whose probe ticks it, so nothing else reaches it.
#[derive(Clone, Debug, Default)]
struct Standing {
    /// The calm: ln of each calm period's wait (µs, never under the floor) added up, and how many.
    calm_ln_sum: f64,
    calm_n: u64,
    /// A line's excursion under way: its periods, folded into the calm if it comes back short of standing.
    excursion_ln_sum: f64,
    excursion_n: u64,
    /// Since when (µs) the queue has been past its calm (a line: every period; a shard's queue: most of the last
    /// tolerance's); when it stood, held until it comes back.
    over_since_us: Option<u64>,
    stood_at_us: Option<u64>,
    /// The longest wait told since it went past its calm, and the latest period's wait, µs.
    longest_us: f64,
    last_us: f64,
    /// A shard's queue: the last tolerance's periods (when, past its calm, ln of its wait), how many were past it,
    /// when the watch began, the last period it stood in, and how much of the last tolerance the shard ran.
    window: VecDeque<(u64, bool, f64)>,
    over_in_window: usize,
    started_us: Option<u64>,
    stood_last_us: Option<u64>,
    busy: Option<f64>,
}

impl Standing {
    /// The calm wait, µs: the geometric mean of the calm periods' waits; with none yet, the floor `floor_us`.
    fn calm_us(&self, floor_us: f64) -> f64 {
        if self.calm_n == 0 {
            return floor_us;
        }
        (self.calm_ln_sum / self.calm_n as f64).exp().max(floor_us)
    }

    /// One period of its queue; what it sheds now: 0, nothing; 1, Background (it stood); 2, Normal too (it stood
    /// another tolerance with Background shed).
    fn period(&mut self, p: Period) -> u8 {
        let floor_us = (p.floor_ns as f64 / 1e3).max(1.0);
        let calm_us = self.calm_us(floor_us);
        let sample_us = p.wait_ns.map(|ns| (ns as f64 / 1e3).max(floor_us));
        // Past its calm: a wait past the floor and past the calm; none told while some wait, as it was.
        let over = match (p.wait_ns, sample_us) {
            (Some(ns), Some(us)) => ns > p.floor_ns && us > calm_us,
            _ => true,
        };
        match p.rule {
            Rule::Line => self.line(p, over, sample_us),
            Rule::Most => self.most(p, over, sample_us.unwrap_or(floor_us)),
        }
    }

    /// A pool's line: it stands once every period for a tolerance was past its calm, the line never emptying, and it
    /// stood a tolerance longer than the longest wait its jobs told since (the module's notes).
    fn line(&mut self, p: Period, over: bool, sample_us: Option<f64>) -> u8 {
        if !p.waiting || !over {
            // Back at its calm, or its line emptied: an excursion that came back short of standing was calm's own.
            if self.stood_at_us.is_none() {
                self.calm_ln_sum += self.excursion_ln_sum;
                self.calm_n = self.calm_n.saturating_add(self.excursion_n);
            }
            self.excursion_ln_sum = 0.0;
            self.excursion_n = 0;
            self.over_since_us = None;
            self.stood_at_us = None;
            self.longest_us = 0.0;
            if let Some(us) = sample_us {
                self.calm_ln_sum += us.ln();
                self.calm_n = self.calm_n.saturating_add(1);
                self.last_us = us;
            }
            return 0;
        }
        let since = *self.over_since_us.get_or_insert(p.now_us);
        if let Some(us) = sample_us {
            self.last_us = us;
            self.longest_us = self.longest_us.max(us).max(p.longest_ns as f64 / 1e3);
            if self.stood_at_us.is_none() {
                self.excursion_ln_sum += us.ln();
                self.excursion_n = self.excursion_n.saturating_add(1);
                // Work that came after the line formed kept it standing, not one batch worked off.
                if p.now_us.saturating_sub(since) as f64 - self.longest_us >= TOLERANCE_US {
                    self.stood_at_us = Some(p.now_us);
                }
            }
        }
        self.level(p.now_us)
    }

    /// A shard's queue: it stands while most of the last tolerance's periods were past its calm (its median wait over
    /// a tolerance past its calm) and, where the OS keeps a thread's CPU time, the shard ran most of that tolerance:
    /// a probe lands at any moment, so most can wait behind the shard's own work only if it runs most of the time;
    /// a mostly idle shard whose wakes the host makes late (a hog holding the CPU to the scheduler's tick) is not
    /// overloaded by its own work, and shedding would not shorten its waits. A period's wait is learned into the calm
    /// as it leaves the window, if the queue never stood while it was in it: an overload's onset is no part of it.
    fn most(&mut self, p: Period, over: bool, sample_us: f64) -> u8 {
        let started = *self.started_us.get_or_insert(p.now_us);
        self.last_us = sample_us;
        self.busy = p.busy;
        self.window.push_back((p.now_us, over, sample_us.ln()));
        self.over_in_window += usize::from(over);
        while let Some(&(at, was_over, ln_us)) = self.window.front() {
            if (p.now_us.saturating_sub(at) as f64) < TOLERANCE_US {
                break;
            }
            self.window.pop_front();
            self.over_in_window = self.over_in_window.saturating_sub(usize::from(was_over));
            if self.stood_last_us.is_none_or(|stood| stood < at) {
                self.calm_ln_sum += ln_us;
                self.calm_n = self.calm_n.saturating_add(1);
            }
        }
        let whole = p.now_us.saturating_sub(started) as f64 >= TOLERANCE_US;
        let most_over = whole && self.over_in_window.saturating_mul(2) > self.window.len();
        let ran_most = p.busy.is_none_or(|busy| busy * 2.0 > 1.0);
        if !(most_over && ran_most) {
            self.over_since_us = None;
            self.stood_at_us = None;
            return 0;
        }
        self.stood_last_us = Some(p.now_us);
        if self.stood_at_us.is_none() {
            self.over_since_us = Some(p.now_us);
            self.stood_at_us = Some(p.now_us);
        }
        self.level(p.now_us)
    }

    /// What a queue that stood at `stood_at_us` sheds at `now_us`: Background at once, Normal a tolerance later.
    fn level(&self, now_us: u64) -> u8 {
        match self.stood_at_us {
            Some(at) if now_us.saturating_sub(at) as f64 >= TOLERANCE_US => 2,
            Some(_) => 1,
            None => 0,
        }
    }

    /// How long its queue has been past its calm at `now_us`, µs: 0 when it is back.
    fn over_for_us(&self, now_us: u64) -> u64 {
        self.over_since_us.map_or(0, |since| now_us.saturating_sub(since))
    }

    /// Why it sheds, for the log: `None` while it hasn't stood.
    fn why(&self, name: &str, now_us: u64, floor_ns: u64, rule: Rule) -> Option<String> {
        self.stood_at_us?;
        let (calm_ms, over_s, now_ms) = (self.calm_us((floor_ns as f64 / 1e3).max(1.0)) / 1e3, self.over_for_us(now_us) as f64 / 1e6, self.last_us / 1e3);
        Some(match rule {
            Rule::Line => format!(
                "{name}: its line stood {over_s:.2} s past its calm of {calm_ms:.2} ms, a tolerance past the longest wait its jobs told, {:.1} ms (now {now_ms:.1} ms)",
                self.longest_us / 1e3
            ),
            Rule::Most => format!(
                "{name}: {} of its last {} periods past its calm of {calm_ms:.2} ms{}, standing {over_s:.2} s (now {now_ms:.1} ms)",
                self.over_in_window,
                self.window.len(),
                self.busy.map_or_else(String::new, |busy| format!(", the shard running {:.0}% of them", busy * 100.0))
            ),
        })
    }
}

/// What a shard sheds from its two watches' levels: Background while either has stood, Normal while the API's has
/// stood its second tolerance (the module's notes).
fn shard_level(normal: u8, background: u8) -> u8 {
    normal.max(background.min(1))
}

/// One shard's watches and what they last read, kept by the shard's own thread: the probe that ticks them runs
/// there alone, so nothing else ever reaches them.
struct Watches {
    base: Instant,
    /// The API's: on the shard's delay (goal 1's probe, a Normal task, late against its timer).
    normal: Standing,
    /// Background work's: on the Background tasks' waits, from the runtime's counts.
    background: Standing,
    last: hyper_rt::shard_loop::Counters,
    /// The shard thread's CPU time at each period of the last tolerance (when, ns), for how much of it the shard ran.
    cpu: VecDeque<(u64, u64)>,
    /// The watchdog's newest report of the shard already logged.
    reported: u64,
    /// The pools' watches: the first shard's alone.
    pools: Option<PoolWatches>,
}

/// A watch for each of `pool_health::QUEUES`, and what they last read. The pools serve every shard, so the first
/// shard's probe ticks them for all, and its thread alone keeps them.
struct PoolWatches {
    watches: [Standing; QUEUES.len()],
    last: [pool_health::Queue; QUEUES.len()],
}

thread_local! {
    static WATCHES: std::cell::RefCell<Option<Watches>> = const { std::cell::RefCell::new(None) };
}

/// Shard `index`'s tick, on the shard, from goal 1's probe (`crate::shard_health`, every `shard_health::PERIOD`):
/// `late` is how late the probe ran against its timer, the shard's delay. Feeds the watches, publishes what to
/// shed, and logs what the watchdog reported of the shard.
///
/// - **Normal** (the API) sheds on the shard's delay: the probe is a Normal task, so its lateness is what the
///   API's own work waits on the shard, the classes ahead of it and its own share counted. Its floor is the
///   quantum and the timer's tick: a probe that waited for the poll running as its timer fell due, and for the
///   tick, waited for no queue.
/// - **Background** sheds on what the shard's Background tasks waited, from joining a ready list to their polls,
///   over the period (`Counters::queue_ns_background`): Background work waits behind its own kind alone and a
///   share of the others', so its own flood shows there and not in the API's delay. A period with no Background
///   poll had none waiting: the floor.
///
/// The route waits goal 1 counts (`crate::route_latency`) are no signal here: they run from a request's head read
/// off its socket, which its connection's task reads at Critical (`Connection`) and serves at once, so they do not
/// carry the waits a class makes.
pub fn tick(index: usize, late: Duration, cpu_ns: Option<u64>) {
    let (Some(state), Some(now)) = (shards().get(index), hyper_rt::futures::shard_counters()) else {
        return;
    };
    let quantum_ns = hyper_rt::futures::step_budget_ns().unwrap_or(0);
    let probe_floor_ns = quantum_ns.saturating_add(hyper_rt::futures::timer_tick_ns());
    WATCHES.with(|cell| {
        let mut held = cell.borrow_mut();
        let w = held.get_or_insert_with(|| Watches {
            base: Instant::now(),
            normal: Standing::default(),
            background: Standing::default(),
            last: now,
            cpu: VecDeque::new(),
            reported: 0,
            pools: (index == 0).then(|| PoolWatches { watches: Default::default(), last: pool_health::queues() }),
        });
        let now_us = micros_since(w.base);
        let busy = busy_share(&mut w.cpu, now_us, cpu_ns);
        let last = w.last;
        let waits = [
            mean_ns(now.queue_ns_background, last.queue_ns_background, now.queue_polls_background, last.queue_polls_background),
            mean_ns(now.queue_ns_normal, last.queue_ns_normal, now.queue_polls_normal, last.queue_polls_normal),
            mean_ns(now.queue_ns_critical, last.queue_ns_critical, now.queue_polls_critical, last.queue_polls_critical),
        ];
        let late_ns = nanos(late);
        let normal = w.normal.period(Period::of_shard(now_us, late_ns, probe_floor_ns, busy));
        let background = w.background.period(Period::of_shard(now_us, waits[0].unwrap_or(0), quantum_ns, busy));
        publish(state, waits, late_ns, w, now_us, (quantum_ns, probe_floor_ns));
        let level = shard_level(normal, background);
        if state.level.swap(level, Ordering::Relaxed) != level {
            note_level(index, state, level, w, now_us, (quantum_ns, probe_floor_ns));
        }
        state.coop_yields.store(now.coop_yields, Ordering::Relaxed);
        w.reported = log_stall(index, state.holder, w.reported);
        w.last = now;
        if let Some(pools) = w.pools.as_mut() {
            tick_pools(pools, quantum_ns, now_us);
        }
    });
}

/// The share of the last tolerance the shard's thread ran on the CPU, from its CPU time now (`cpu_ns`, where the OS keeps
/// a thread's: Linux and macOS) and at each period since, kept in `cpu`; `None` where it isn't kept.
fn busy_share(cpu: &mut VecDeque<(u64, u64)>, now_us: u64, cpu_ns: Option<u64>) -> Option<f64> {
    let Some(cpu_ns) = cpu_ns else {
        cpu.clear();
        return None;
    };
    cpu.push_back((now_us, cpu_ns));
    while cpu.len() > 1 && cpu.front().is_some_and(|&(at, _)| now_us.saturating_sub(at) as f64 > TOLERANCE_US) {
        cpu.pop_front();
    }
    let &(from_us, from_ns) = cpu.front()?;
    let wall_ns = now_us.saturating_sub(from_us).saturating_mul(1_000);
    (wall_ns > 0).then(|| cpu_ns.saturating_sub(from_ns) as f64 / wall_ns as f64)
}

/// The pools' tick, on the first shard, from its probe: each pool's period to its watch ([`pool_sample`]), and what
/// each sheds published for every shard.
fn tick_pools(p: &mut PoolWatches, floor_ns: u64, now_us: u64) {
    let now = pool_health::queues();
    let longest_us = pool_health::take_longest();
    for (i, (((w, queue), last), longest_us)) in p.watches.iter_mut().zip(now).zip(p.last).zip(longest_us).enumerate() {
        let wait_ns = pool_sample(queue, last);
        let level = w.period(Period { now_us, wait_ns, longest_ns: longest_us.saturating_mul(1_000), waiting: queue.waiting, floor_ns, rule: Rule::Line, busy: None });
        if let (Some(ns), Some(slot)) = (wait_ns, POOLS.delay_ns.get(i)) {
            slot.store(ns.max(floor_ns), Ordering::Relaxed);
        }
        if let Some(calm) = POOLS.calm_ns.get(i) {
            calm.store((w.calm_us((floor_ns as f64 / 1e3).max(1.0)) * 1e3) as u64, Ordering::Relaxed);
        }
        if let Some(over) = POOLS.over_ns.get(i) {
            over.store(w.over_for_us(now_us).saturating_mul(1_000), Ordering::Relaxed);
        }
        if POOLS.level.get(i).is_some_and(|slot| slot.swap(level, Ordering::Relaxed) != level) {
            note_pool_level(i, level, w, now_us, floor_ns);
        }
    }
    p.last = now;
}

/// What a pool's period, from `last` to `now`, gives its watch, in nanoseconds before the floor: the mean wait of the
/// jobs whose waits were told over it (the compute pool's as each job's first piece starts, a share's as each job
/// ends); none told and none waiting, nothing queued, `0` (the floor); none told while some wait, no sample
/// (`None`), since their waits are told only once they start or end: the watch holds where it was.
fn pool_sample(now: pool_health::Queue, last: pool_health::Queue) -> Option<u64> {
    let told = now.waits.saturating_sub(last.waits);
    match now.waited_us.saturating_sub(last.waited_us).checked_div(told) {
        Some(us) => Some(us.saturating_mul(1_000)),
        None if now.waiting => None,
        None => Some(0),
    }
}

/// The delays, calms and standing a tick read, for answers, `/metrics` and `/readyz`: each class's mean wait from the
/// runtime's counts, but Normal's, which is the shard's delay its watch reads. `floors` are the quantum (Background's)
/// and the probe's (Normal's).
fn publish(state: &ShardState, waits: [Option<u64>; 3], late_ns: u64, w: &Watches, now_us: u64, floors: (u64, u64)) {
    let delays = [waits[0], Some(late_ns), waits[2]];
    for (class, delay) in [Class::Background, Class::Normal, Class::Critical].into_iter().zip(delays) {
        if let (Some(delay), Some(slot)) = (delay, state.delay_ns.get(class as usize)) {
            slot.store(delay, Ordering::Relaxed);
        }
    }
    for (class, watch, floor_ns) in [(Class::Background, &w.background, floors.0), (Class::Normal, &w.normal, floors.1)] {
        if let Some(calm) = state.calm_ns.get(class as usize) {
            calm.store((watch.calm_us((floor_ns as f64 / 1e3).max(1.0)) * 1e3) as u64, Ordering::Relaxed);
        }
        if let Some(over) = state.over_ns.get(class as usize) {
            over.store(watch.over_for_us(now_us).saturating_mul(1_000), Ordering::Relaxed);
        }
    }
}

/// The mean of a class's waits over a period, from the counts before and after: `None` with no poll of it.
fn mean_ns(sum: u64, sum_before: u64, polls: u64, polls_before: u64) -> Option<u64> {
    sum.saturating_sub(sum_before).checked_div(polls.saturating_sub(polls_before))
}

/// What a level sheds, in words.
fn what(level: u8) -> &'static str {
    if level >= 2 { "normal and background" } else { "background" }
}

/// The log of a change of what a shard sheds, with why. `floors` as [`publish`]'s.
fn note_level(index: usize, state: &ShardState, level: u8, w: &Watches, now_us: u64, floors: (u64, u64)) {
    let reasons: Vec<String> = [w.normal.why("normal", now_us, floors.1, Rule::Most), w.background.why("background", now_us, floors.0, Rule::Most)].into_iter().flatten().collect();
    let delay = |class: Class| state.delay_ns.get(class as usize).map_or(0, |d| d.load(Ordering::Relaxed));
    match level {
        0 => tracing::info!(shard = index, "shard no longer sheds requests: its queue came back to its calm"),
        _ => tracing::warn!(
            shard = index,
            sheds = what(level),
            normal_delay_us = delay(Class::Normal) / 1_000,
            background_delay_us = delay(Class::Background) / 1_000,
            why = %reasons.join("; "),
            shedding = ENABLED.load(Ordering::Relaxed),
            "shard overloaded: its queue stood past its calm"
        ),
    }
}

/// The log of a change of what work waiting in pool `i` sheds, with why.
fn note_pool_level(i: usize, level: u8, w: &Standing, now_us: u64, floor_ns: u64) {
    let pool = pool_name(i);
    match level {
        0 => tracing::info!(pool = %pool, "work waiting in a pool no longer sheds requests: its line came back to its calm, or emptied"),
        _ => tracing::warn!(
            pool = %pool,
            sheds = what(level),
            delay_us = pool_delay_ns(i) / 1_000,
            why = %w.why(&pool, now_us, floor_ns, Rule::Line).unwrap_or_default(),
            shedding = ENABLED.load(Ordering::Relaxed),
            "work waiting in a pool stood past its calm: every shard sheds requests"
        ),
    }
}

/// Logs the watchdog's newest report of shard `holder` when it is past `reported`; the newest number logged.
fn log_stall(index: usize, holder: hyper_rt::SlotHolder, reported: u64) -> u64 {
    match hyper_rt::watch::report(holder) {
        Some(r) if r.number > reported => {
            tracing::warn!(
                shard = index,
                task = r.task.0.slot(),
                spawned_at = %r.spawned_at.map_or_else(|| "unknown".to_owned(), ToString::to_string),
                held_ms = r.held_ns / 1_000_000,
                cpu_ms = r.cpu_ns.map(|ns| ns / 1_000_000),
                whose = r.whose.as_str(),
                "a task held its shard past the watchdog's limit; new work went to other shards"
            );
            r.number
        }
        _ => reported,
    }
}

/// The answer for a request of `class` the shard serving it sheds now, or `None` to serve it: 503 at once,
/// `Retry-After` the queueing delay that sheds it in whole seconds (at least one, HTTP's unit; the shard's class's,
/// else the pool's whose line sheds it), `x-kiln-shed` its class.
pub fn shed(class: Class) -> Option<Response> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let state = here()?;
    let (delay_ns, calm_ns, pool) = if sheds(state.level.load(Ordering::Relaxed), class) {
        // Normal's own delay sheds it; Background is shed by either watch: the longer delay says when to come back.
        let delay = |c: Class| state.delay_ns.get(c as usize).map_or(0, |d| d.load(Ordering::Relaxed));
        let delay_ns = if class == Class::Background { delay(Class::Background).max(delay(Class::Normal)) } else { delay(class) };
        (delay_ns, state.calm_ns.get(class as usize).map_or(0, |d| d.load(Ordering::Relaxed)), None)
    } else {
        let i = pool_shedding(class)?;
        (pool_delay_ns(i), POOLS.calm_ns.get(i).map_or(0, |d| d.load(Ordering::Relaxed)), Some(i))
    };
    if let Some(n) = SHED.get(class as usize) {
        n.fetch_add(1, Ordering::Relaxed);
    }
    Some(answer(class, delay_ns, calm_ns, pool))
}

/// The pool whose line sheds `class` now, the one that waited longest when several do: `None` when none does.
fn pool_shedding(class: Class) -> Option<usize> {
    (0..QUEUES.len()).filter(|i| sheds(pool_level(*i), class)).max_by_key(|i| pool_delay_ns(*i))
}

/// Whether a shard at `level` sheds a request of `class`: Background from level 1, Normal from 2, Critical never.
fn sheds(level: u8, class: Class) -> bool {
    match class {
        Class::Background => level >= 1,
        Class::Normal => level >= 2,
        Class::Critical => false,
    }
}

/// The answer to a request of `class` shed at a queueing delay of `delay_ns` against a calm of `calm_ns`: the
/// shard's, or pool `pool`'s (of `pool_health::QUEUES`).
fn answer(class: Class, delay_ns: u64, calm_ns: u64, pool: Option<usize>) -> Response {
    let retry = delay_ns.div_ceil(1_000_000_000).max(1);
    let (who, queue) = match pool {
        None => ("this server's shard", "its queueing delay".to_owned()),
        Some(i) => ("this server", format!("{}'s wait", pool_name(i))),
    };
    let why = format!(
        "overloaded: {who} sheds {} requests now ({queue} {:.1} ms, standing past its calm of {:.2} ms); try again in {retry} s",
        class.as_str(),
        delay_ns as f64 / 1e6,
        calm_ns as f64 / 1e6
    );
    let resp = srv::with_header(srv::text(StatusCode::SERVICE_UNAVAILABLE, &why), header::RETRY_AFTER, &retry.to_string());
    srv::with_header(resp, header::HeaderName::from_static("x-kiln-shed"), class.as_str())
}

/// The answer for `req` when its shard sheds its class now (its class as [`Connection::begin`] put it, else read
/// from it).
pub fn shed_request(req: &Request) -> Option<Response> {
    let class = req.extensions().get::<Class>().copied().unwrap_or_else(|| class_of(req.method(), req.uri().path(), req.headers()));
    shed(class)
}

/// What `/readyz` says of overload: `None` when nothing sheds; else whether every shard sheds the API (Normal), by
/// its own queue or the pools', and a line saying what each shard and each pool sheds.
pub fn readiness() -> Option<(bool, String)> {
    let all = shards();
    let levels: Vec<u8> = all.iter().map(|s| s.level.load(Ordering::Relaxed)).collect();
    let pools: Vec<u8> = (0..QUEUES.len()).map(pool_level).collect();
    let pooled = pools.iter().copied().max().unwrap_or(0);
    if levels.iter().all(|l| *l == 0) && pooled == 0 {
        return None;
    }
    // Not ready only while every shard refuses the API; with shedding off, nothing is refused.
    let every = ENABLED.load(Ordering::Relaxed) && !levels.is_empty() && levels.iter().all(|l| (*l).max(pooled) >= 2);
    let mut line = String::from("overloaded:");
    for (index, level) in levels.iter().enumerate().filter(|(_, l)| **l > 0) {
        let _ = write!(line, " shard {index} sheds {} requests;", what(*level));
    }
    for (i, level) in pools.iter().enumerate().filter(|(_, l)| **l > 0) {
        let _ = write!(line, " work waiting in {} sheds {} requests;", pool_name(i), what(*level));
    }
    if !ENABLED.load(Ordering::Relaxed) {
        line.push_str(" (shedding is off: [server] shed = true turns it on)");
    }
    Some((every, line.trim_end_matches(';').to_owned()))
}

/// Requests shed so far, by class (Background, Normal; Critical is never shed).
pub fn shed_counts() -> [u64; 3] {
    std::array::from_fn(|i| SHED.get(i).map_or(0, |n| n.load(Ordering::Relaxed)))
}

// ------------------------------------------------------------------------------------- /metrics

/// The watchdog's findings, each shard's classes' queueing delay, standing and what it sheds, the pools', and the
/// requests shed.
pub fn render(o: &mut String) {
    let holders = crate::rt::shard_holders();
    let mut series = |name: &str, kind: &str, help: &str, value: &dyn Fn(usize, hyper_rt::SlotHolder) -> Option<String>| {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}");
        for (index, holder) in holders.iter().enumerate() {
            if let Some(v) = value(index, *holder) {
                let _ = writeln!(o, "{name}{{shard=\"{index}\"}} {v}");
            }
        }
    };
    let stalls = |holder| hyper_rt::watch::stalls(holder).unwrap_or_default();
    series("kiln_shard_stalled", "gauge", "1 while kiln-rt's watchdog sees the shard held in one poll past its limit (new work goes to other shards).", &|_, h| Some(u8::from(stalls(h).stalled).to_string()));
    series("kiln_shard_stalls_total", "counter", "Polls the watchdog saw hold the shard past its limit (a step's worth of polls: batch x quantum).", &|_, h| Some(stalls(h).episodes.to_string()));
    series("kiln_shard_stalled_seconds_total", "counter", "Time those polls held the shard.", &|_, h| Some(format!("{:.6}", stalls(h).held_ns as f64 / 1e9)));
    series("kiln_shard_stall_moved_total", "counter", "Spawns waiting for the shard while it was held that the watchdog handed to another shard.", &|_, h| Some(stalls(h).moved.to_string()));
    series("kiln_shard_stall_reports_total", "counter", "Stalls reported (5 a minute a shard at most).", &|_, h| Some(stalls(h).reports.to_string()));
    series("kiln_shard_stall_reports_suppressed_total", "counter", "Stalls past the reports' cap, counted only.", &|_, h| Some(stalls(h).suppressed.to_string()));
    series("kiln_shard_coop_yields_total", "counter", "Polls that ended because their task used its share of the shard while other work waited.", &|i, _| {
        shards().get(i).map(|s| s.coop_yields.load(Ordering::Relaxed).to_string())
    });
    let _ = writeln!(o, "# HELP kiln_shard_last_stall_seconds How long the latest reported stall had held its shard, with its task, spawn site and whose the time was.\n# TYPE kiln_shard_last_stall_seconds gauge");
    for (index, holder) in holders.iter().enumerate() {
        if let Some(r) = hyper_rt::watch::report(*holder) {
            // A label value: quotes and backslashes (a Windows path's) kept out of the exposition format.
            let at = r.spawned_at.map_or_else(|| "unknown".to_owned(), ToString::to_string).replace('"', "'").replace('\\', "/");
            let _ = writeln!(o, "kiln_shard_last_stall_seconds{{shard=\"{index}\",task=\"{}\",spawned_at=\"{at}\",whose=\"{}\"}} {:.6}", r.task.0.slot(), r.whose.as_str(), r.held_ns as f64 / 1e9);
        }
    }
    let _ = writeln!(o, "# HELP kiln_shard_queue_delay_seconds Each class's mean wait from ready to run on the shard, over the last 10 ms.\n# TYPE kiln_shard_queue_delay_seconds gauge");
    let _ = writeln!(
        o,
        "# HELP kiln_shard_queue_standing_seconds How long each class's queue on the shard has stood, most of the last tolerance (1 s) past its calm while the shard ran most of it, 0 once it no longer does: it sheds while it stands.\n# TYPE kiln_shard_queue_standing_seconds gauge"
    );
    let _ = writeln!(o, "# HELP kiln_shard_shedding 1 while the shard's own queue, standing past its calm, sheds the class's requests (the pools': kiln_pool_shedding).\n# TYPE kiln_shard_shedding gauge");
    for (index, state) in shards().iter().enumerate() {
        for class in [Class::Critical, Class::Normal, Class::Background] {
            let delay = state.delay_ns.get(class as usize).map_or(0, |d| d.load(Ordering::Relaxed));
            let _ = writeln!(o, "kiln_shard_queue_delay_seconds{{shard=\"{index}\",class=\"{}\"}} {:.6}", class.as_str(), delay as f64 / 1e9);
        }
        for class in [Class::Normal, Class::Background] {
            let over = state.over_ns.get(class as usize).map_or(0, |d| d.load(Ordering::Relaxed));
            let _ = writeln!(o, "kiln_shard_queue_standing_seconds{{shard=\"{index}\",class=\"{}\"}} {:.6}", class.as_str(), over as f64 / 1e9);
        }
        let level = state.level.load(Ordering::Relaxed);
        let _ = writeln!(o, "kiln_shard_shedding{{shard=\"{index}\",class=\"background\"}} {}", u8::from(level >= 1));
        let _ = writeln!(o, "kiln_shard_shedding{{shard=\"{index}\",class=\"normal\"}} {}", u8::from(level >= 2));
    }
    let _ = writeln!(
        o,
        "# HELP kiln_pool_queue_delay_seconds Each pool's mean wait for a place over the shards' probe period, as its watch last read it: the compute pool's jobs for a thread, each of the blocking pool's shares' for one of its places, kiln.db's writer's calls for their turn.\n# TYPE kiln_pool_queue_delay_seconds gauge"
    );
    for (i, pool) in QUEUES.iter().enumerate() {
        let _ = writeln!(o, "kiln_pool_queue_delay_seconds{{pool=\"{pool}\"}} {:.6}", pool_delay_ns(i) as f64 / 1e9);
    }
    let _ = writeln!(
        o,
        "# HELP kiln_pool_queue_standing_seconds How long each pool's line has stood past its calm, never emptying, 0 once it came back: it sheds once it has stood a tolerance (1 s) longer than its jobs waited.\n# TYPE kiln_pool_queue_standing_seconds gauge"
    );
    for (i, pool) in QUEUES.iter().enumerate() {
        let over = POOLS.over_ns.get(i).map_or(0, |d| d.load(Ordering::Relaxed));
        let _ = writeln!(o, "kiln_pool_queue_standing_seconds{{pool=\"{pool}\"}} {:.6}", over as f64 / 1e9);
    }
    let _ = writeln!(o, "# HELP kiln_pool_shedding 1 while work standing in the pool's line past its calm sheds the class's requests on every shard.\n# TYPE kiln_pool_shedding gauge");
    for (i, pool) in QUEUES.iter().enumerate() {
        let level = pool_level(i);
        let _ = writeln!(o, "kiln_pool_shedding{{pool=\"{pool}\",class=\"background\"}} {}", u8::from(level >= 1));
        let _ = writeln!(o, "kiln_pool_shedding{{pool=\"{pool}\",class=\"normal\"}} {}", u8::from(level >= 2));
    }
    let _ = writeln!(o, "# HELP kiln_shed_total Requests answered 503 at once because their shard's queue, or a pool's line, stood past its calm, by class.\n# TYPE kiln_shed_total counter");
    for class in [Class::Background, Class::Normal] {
        let _ = writeln!(o, "kiln_shed_total{{class=\"{}\"}} {}", class.as_str(), SHED.get(class as usize).map_or(0, |n| n.load(Ordering::Relaxed)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: Method, path: &str, headers: &[(&str, &str)]) -> Class {
        let mut h = HeaderMap::new();
        for (k, v) in headers {
            if let (Ok(k), Ok(v)) = (header::HeaderName::from_bytes(k.as_bytes()), http::HeaderValue::from_str(v)) {
                h.append(k, v);
            }
        }
        class_of(&method, path, &h)
    }

    /// Step 3: every request is Critical (health, sign-in, a person's pages and actions), Normal (the API) or
    /// Background (agents' and CI's polling, catch-ups, sync, telemetry).
    #[test]
    fn g6_s3_every_request_has_its_class() {
        let cookie = [("cookie", "theme=dark; kiln_session=abc")];
        let token = [("authorization", "Bearer kiln_pat_x")];
        for (method, path, headers, class) in [
            (Method::GET, "/healthz", &[][..], Class::Critical),
            (Method::GET, "/readyz", &[][..], Class::Critical),
            (Method::GET, "/metrics", &[][..], Class::Critical),
            (Method::POST, "/api/auth/login", &[][..], Class::Critical),
            (Method::GET, "/api/me", &cookie[..], Class::Critical),
            (Method::GET, "/", &[][..], Class::Critical),
            (Method::GET, "/runs/42", &cookie[..], Class::Critical),
            (Method::GET, "/assets/index.js", &[][..], Class::Critical),
            (Method::POST, "/api/harness/sessions/s1/command", &cookie[..], Class::Critical),
            (Method::GET, "/api/runs", &cookie[..], Class::Normal),
            (Method::POST, "/api/harness/sessions/s1/command", &token[..], Class::Normal),
            (Method::GET, "/api/runs", &token[..], Class::Normal),
            (Method::POST, "/api/mcp", &token[..], Class::Normal),
            (Method::POST, "/api/hooks/github", &[][..], Class::Normal),
            (Method::GET, "/install.sh", &[][..], Class::Normal),
            (Method::PUT, "/api/sync/blobs/abc", &token[..], Class::Background),
            (Method::GET, "/api/sync/bundles/b1", &token[..], Class::Background),
            (Method::GET, "/api/cluster/copy", &token[..], Class::Background),
            (Method::POST, "/v1/traces", &token[..], Class::Background),
            (Method::POST, "/api/v1/write", &token[..], Class::Background),
            (Method::POST, "/opentelemetry.proto.collector.trace.v1.TraceService/Export", &[][..], Class::Background),
            (Method::GET, "/api/jobs/j1/source", &token[..], Class::Background),
            (Method::PUT, "/api/jobs/j1/artifacts/upload", &token[..], Class::Background),
            (Method::GET, "/dl/kiln/linux-amd64", &[][..], Class::Background),
            (Method::GET, "/api/agents/tunnel", &[][..], Class::Background),
            (Method::GET, "/api/join-requests/r1", &[][..], Class::Background),
            (Method::GET, "/api/harness/agents/wait", &token[..], Class::Background),
            (Method::GET, "/api/harness/sessions/s1/events", &token[..], Class::Background),
            (Method::GET, "/api/harness/sessions/s1/events", &cookie[..], Class::Normal),
        ] {
            assert_eq!(req(method.clone(), path, headers), class, "{method} {path} {headers:?}");
        }
    }

    /// A deterministic generator for the replayed traces (xorshift64*, as headroom's detector's tests'), so each
    /// replays the same every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        }

        fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
            lo + (hi - lo) * self.next()
        }

        /// An exponential draw of mean `mean`.
        fn exp(&mut self, mean: f64) -> f64 {
            -mean * (1.0 - self.next()).max(f64::MIN_POSITIVE).ln()
        }

        /// A standard normal (Box-Muller).
        fn normal(&mut self) -> f64 {
            let (a, b) = (self.next().max(1e-12), self.next());
            (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
        }
    }

    /// Goal 1's probe period, the watches' tick, µs.
    const PERIOD_US: u64 = 10_000;
    /// The server's step quantum (`crate::rt::STEP_BUDGET_NS`), and a probe's floor: the quantum and the timer's tick.
    const QUANTUM_NS: u64 = crate::rt::STEP_BUDGET_NS;
    const PROBE_FLOOR_NS: u64 = crate::rt::STEP_BUDGET_NS + 100_000;

    /// A pool's line replayed: each period as its watch reads it (the mean and longest wait told over it, a share
    /// telling each job's wait as it ends; whether a job waited at its tick), and the trace's own numbers, held to
    /// the lab's.
    struct Replay {
        periods: Vec<(Option<u64>, u64, bool)>,
        p99_ms: f64,
        most_ms: f64,
        busy: f64,
        per_s: f64,
    }

    /// `jobs` (arrival, run; µs) served first come first served on `places`, for `secs`.
    fn replay(mut jobs: Vec<(f64, f64)>, places: usize, secs: f64) -> Replay {
        jobs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let end_us = secs * 1e6;
        let n = (end_us / PERIOD_US as f64) as usize + 1;
        let mut told = vec![(0u64, 0f64, 0f64); n];
        let mut waiting = vec![0i64; n + 1];
        let mut free: std::collections::BinaryHeap<std::cmp::Reverse<u64>> = (0..places).map(|_| std::cmp::Reverse(0)).collect();
        let (mut waits, mut busy) = (Vec::with_capacity(jobs.len()), 0.0);
        for (at, run) in &jobs {
            let free_at = free.pop().map_or(0, |std::cmp::Reverse(t)| t) as f64;
            let start = at.max(free_at);
            let end = start + run;
            free.push(std::cmp::Reverse(end as u64));
            let wait = start - at;
            waits.push(wait);
            busy += end.min(end_us) - start.min(end_us);
            // Told at the first tick at or after its end; waiting at every tick from its arrival to its start.
            if let Some(t) = told.get_mut((end / PERIOD_US as f64).ceil() as usize) {
                t.0 += 1;
                t.1 += wait;
                t.2 = t.2.max(wait);
            }
            let (from, to) = ((at / PERIOD_US as f64).ceil() as usize, (start / PERIOD_US as f64).ceil() as usize);
            if to > from {
                if let Some(c) = waiting.get_mut(from) {
                    *c += 1;
                }
                if let Some(c) = waiting.get_mut(to.min(n)) {
                    *c -= 1;
                }
            }
        }
        let mut held = 0i64;
        let periods = told
            .iter()
            .zip(&waiting)
            .map(|(&(count, sum, longest), add)| {
                held += add;
                let wait = if count > 0 {
                    Some(((sum / count as f64) as u64) * 1_000)
                } else if held > 0 {
                    None
                } else {
                    Some(0)
                };
                (wait, (longest as u64) * 1_000, held > 0)
            })
            .collect();
        waits.sort_by(f64::total_cmp);
        let at = |q: f64| waits.get((waits.len() as f64 * q) as usize).or(waits.last()).copied().unwrap_or(0.0) / 1e3;
        Replay { periods, p99_ms: at(0.99), most_ms: at(1.0), busy: busy / end_us, per_s: jobs.len() as f64 / secs }
    }

    /// What a watch of `rule` sheds at each of `r`'s periods: a pool's line's, or (to show what the line's rule holds)
    /// a shard's queue's.
    fn watch_pool(r: &Replay, rule: Rule) -> Vec<u8> {
        let mut w = Standing::default();
        let mut now_us = 0;
        r.periods
            .iter()
            .map(|&(wait_ns, longest_ns, waiting)| {
                let level = w.period(Period { now_us, wait_ns, longest_ns, waiting, floor_ns: QUANTUM_NS, rule, busy: None });
                now_us += PERIOD_US;
                level
            })
            .collect()
    }

    /// What a shard's watch sheds at each probe of `late` (ns), on a probe's floor, the shard running `busy[k]` of the
    /// tolerance up to probe `k` (the last share given holding past the end of `busy`; none: no CPU time known).
    fn watch_shard(late: &[u64], busy: &[f64]) -> Vec<u8> {
        let mut w = Standing::default();
        late.iter()
            .enumerate()
            .map(|(k, &ns)| w.period(Period::of_shard(k as u64 * PERIOD_US, ns, PROBE_FLOOR_NS, busy.get(k).or(busy.last()).copied())))
            .collect()
    }

    /// The lab's calm shard runs about 7% of the time (fedora, 2026-11-05: `shards.busy_share` 0.064 to 0.073).
    const CALM_BUSY: f64 = 0.07;

    /// The first period at or after `from` whose level is at least `level`.
    fn first(levels: &[u8], level: u8, from: usize) -> Option<usize> {
        levels.iter().enumerate().skip(from).find(|(_, l)| **l >= level).map(|(k, _)| k)
    }

    /// The store share's calm in the lab (fedora, 2026-11-05, research 01a1a59c2abf f32): about 1,200 reads a second
    /// on its 24 places, each a fraction of a millisecond, and every 15 to 35 s one request's 700 to 1,300 reads of 8 to
    /// 20 ms at once (a conversation's attach), its jobs waiting 307 to 620 ms at p99 there while the share held under
    /// one place on average. µs.
    fn calm_store(seed: u64, secs: f64) -> Vec<(f64, f64)> {
        let mut r = Rng(seed);
        let mut jobs = Vec::new();
        let mut t = 0.0;
        while t < secs * 1e6 {
            t += r.exp(1e6 / 1_150.0);
            jobs.push((t, r.exp(200.0)));
        }
        let mut t = r.uniform(2e6, 10e6);
        while t < secs * 1e6 {
            let n = r.uniform(700.0, 1_300.0) as usize;
            for _ in 0..n {
                jobs.push((t, r.uniform(8_000.0, 20_000.0)));
            }
            t += r.uniform(15e6, 35e6);
        }
        jobs
    }

    /// One request's `n` reads of 8 to 20 ms submitted at once at `at_us`, as an attach's.
    fn fan_out(r: &mut Rng, jobs: &mut Vec<(f64, f64)>, n: usize, at_us: f64) {
        for _ in 0..n {
            jobs.push((at_us, r.uniform(8_000.0, 20_000.0)));
        }
    }

    /// A shard's probes in the lab's calm (research f32): most on time, within the timer's tick; a p99 of about 0.65 ms,
    /// a p99.9 of about 24 ms, and a probe 250 ms late in every round. Lateness, ns.
    fn calm_probes(r: &mut Rng, n: usize) -> Vec<u64> {
        (0..n)
            .map(|_| {
                let x = r.next();
                let us = if x < 0.985 {
                    r.uniform(5.0, 120.0)
                } else if x < 0.995 {
                    r.uniform(300.0, 900.0)
                } else if x < 0.999 {
                    r.uniform(1_000.0, 25_000.0)
                } else {
                    r.uniform(25_000.0, 250_000.0)
                };
                (us * 1e3) as u64
            })
            .collect()
    }

    fn quantile_ms(late: &[u64], q: f64) -> f64 {
        let mut s = late.to_vec();
        s.sort_unstable();
        s.get((s.len() as f64 * q) as usize).or(s.last()).copied().unwrap_or(0) as f64 / 1e6
    }

    /// Step 4: the store share's calm, replayed from the lab's (its fan-outs' jobs waiting hundreds of milliseconds
    /// at p99, its line standing up to most of a second), sheds nothing: main's CUSUM shed during each of these
    /// fan-outs (6.5 Background requests a calm round in the lab), since each read as a shift from a calm near the
    /// floor. Eight seeds of three minutes, each held to the lab's own numbers first.
    #[test]
    fn g6_s4_the_lab_calm_store_share_never_sheds() {
        for seed in 1..=8u64 {
            let r = replay(calm_store(seed, 180.0), 24, 180.0);
            assert!((300.0..=650.0).contains(&r.p99_ms), "seed {seed}: the lab's calm p99 wait (307 to 620 ms), not {:.0} ms", r.p99_ms);
            assert!((0.5..=1.0).contains(&r.busy) && (1_100.0..=1_300.0).contains(&r.per_s), "seed {seed}: the lab's calm share: {:.2} places, {:.0} a second", r.busy, r.per_s);
            let levels = watch_pool(&r, Rule::Line);
            assert_eq!(first(&levels, 1, 0), None, "seed {seed}: the calm sheds nothing (most wait {:.0} ms)", r.most_ms);
        }
    }

    /// Step 4: one request's reads submitted at once wait behind each other as long as the line has stood, so however
    /// long they take to work off they shed nothing: a fan-out of 1,700 (its last reads waiting about the
    /// tolerance, the waits main shed Normal at), and one of 4,000 (its line standing well past two tolerances, its
    /// reads waiting more than two seconds), each with the calm's reads behind it. Without the line's rule, the
    /// 4,000 would shed both classes.
    #[test]
    fn g6_s4_one_requests_reads_never_shed_however_long_they_take() {
        for seed in 1..=3u64 {
            for (n, most_s) in [(1_700, 0.8), (4_000, 2.0)] {
                let mut jobs = calm_store(seed, 60.0);
                fan_out(&mut Rng(seed ^ 0xfa11), &mut jobs, n, 30e6);
                let r = replay(jobs, 24, 60.0);
                assert!(r.most_ms >= most_s * 1e3, "seed {seed}: {n} reads wait past {most_s} s, not {:.0} ms", r.most_ms);
                assert_eq!(first(&watch_pool(&r, Rule::Line), 1, 0), None, "seed {seed}: {n} reads at once shed nothing");
                if n == 4_000 {
                    assert!(first(&watch_pool(&r, Rule::Most), 2, 0).is_some(), "seed {seed}: read as a shard's queue, with no line's rule, it would shed both classes");
                }
            }
        }
    }

    /// Step 4: work that outruns a pool sheds Background within a tolerance past its jobs' waits, then Normal a
    /// tolerance later, and nothing once its line drains: 20 s of jobs arriving at 1.25 times what 24 places serve,
    /// in the calm.
    #[test]
    fn g6_s4_work_outrunning_a_pool_sheds_background_then_normal_and_stops_once_drained() {
        for seed in 1..=3u64 {
            let mut jobs = calm_store(seed, 60.0);
            let mut r = Rng(seed ^ 0x0e7e);
            let (run_us, places) = (15_000.0, 24.0);
            let mut t = 20e6;
            loop {
                t += r.exp(run_us / places / 1.25);
                if t >= 40e6 {
                    break;
                }
                jobs.push((t, r.uniform(0.5, 1.5) * run_us));
            }
            let levels = watch_pool(&replay(jobs, 24, 60.0), Rule::Line);
            let onset = 2_000;
            assert_eq!(first(&levels, 1, 0).map(|k| k >= onset), Some(true), "seed {seed}: nothing shed before the work outran it");
            let background = first(&levels, 1, onset).unwrap_or(usize::MAX) - onset;
            assert!((100..=200).contains(&background), "seed {seed}: Background shed a tolerance past its jobs' waits: {} ms in", background * 10);
            let normal = first(&levels, 2, onset).unwrap_or(usize::MAX) - onset;
            assert_eq!(normal, background + 100, "seed {seed}: Normal a tolerance later");
            assert!(levels.iter().skip(5_000).all(|l| *l == 0), "seed {seed}: nothing once its line drained");
        }
    }

    /// Step 4: a closed loop past a pool's places (48 clients on 24 places, each asking again as soon as it is
    /// answered: its line holds 24 jobs a run long) sheds Background a tolerance and a run in, Normal a tolerance
    /// later, and nothing once the clients go.
    #[test]
    fn g6_s4_a_closed_loop_past_a_pools_places_sheds_and_stops() {
        use std::cmp::Reverse;
        for seed in 1..=3u64 {
            let mut jobs = calm_store(seed, 60.0);
            let mut r = Rng(seed ^ 0xc105);
            let mut places: std::collections::BinaryHeap<Reverse<u64>> = (0..24).map(|_| Reverse(20_000_000)).collect();
            let mut clients: std::collections::BinaryHeap<Reverse<u64>> = (0..48).map(|_| Reverse(20_000_000 + r.uniform(0.0, 1_000.0) as u64)).collect();
            while let Some(Reverse(at)) = clients.pop() {
                if at >= 40_000_000 {
                    continue;
                }
                let run = r.uniform(7_500.0, 22_500.0);
                let start = places.pop().map_or(at, |Reverse(free)| free.max(at));
                let end = start + run as u64;
                places.push(Reverse(end));
                jobs.push((at as f64, run));
                clients.push(Reverse(end));
            }
            let levels = watch_pool(&replay(jobs, 24, 60.0), Rule::Line);
            let onset = 2_000;
            let background = first(&levels, 1, 0).unwrap_or(usize::MAX).saturating_sub(onset);
            assert!((100..=110).contains(&background), "seed {seed}: Background a tolerance and a run in: {} ms", background * 10);
            assert_eq!(first(&levels, 2, 0).map(|k| k.saturating_sub(onset)), Some(background + 100), "seed {seed}: Normal a tolerance later");
            assert!(levels.iter().skip(4_200).all(|l| *l == 0), "seed {seed}: nothing once the clients went");
        }
    }

    /// Step 4: a shard's probes in the lab's calm shed nothing: most on time, a few hundred microseconds late at p99,
    /// a probe tens of milliseconds late at p99.9, one 250 ms late now and then, and a server's start holding the
    /// shard 0.88, 1.6 and 2.9 s (one late probe each; main's CUSUM shed at the 0.88 s one and at each past the
    /// tolerance, as idle servers' logs show).
    #[test]
    fn g6_s4_a_shards_calm_and_its_stalls_shed_nothing() {
        for seed in 1..=5u64 {
            let mut r = Rng(seed);
            let mut late = calm_probes(&mut r, 12_000);
            for (at, ns) in [(50, 1_613_000_000u64), (400, 2_933_000_000), (700, 880_000_000)] {
                if let Some(slot) = late.get_mut(at) {
                    *slot = ns;
                }
            }
            let window = late.get(1_000..).unwrap_or_default();
            let (p99, p999) = (quantile_ms(window, 0.99), quantile_ms(window, 0.999));
            assert!((0.5..=0.8).contains(&p99) && (15.0..=80.0).contains(&p999), "seed {seed}: the lab's calm probes (p99 0.6 to 0.77 ms, p99.9 18 to 31 ms), not {p99:.2} and {p999:.1} ms");
            assert_eq!(first(&watch_shard(&late, &[CALM_BUSY]), 1, 0), None, "seed {seed}: the calm and its stalls shed nothing");
        }
    }

    /// Step 4: CPU hogs on a kernel that preempts lazily make each of a shard's wakes wait up to a scheduler's tick
    /// (1 ms at 1000 Hz) for the hog on its CPU to let go: most of its probes run late past its calm, but the shard
    /// runs 7% of the time, so the waits are the host's, not a queue of its own work, and shedding would not shorten
    /// them: nothing is shed (the overload lab's one shard, 34% busy on CPUs the pools' threads took, was so too).
    #[test]
    fn g6_s4_a_mostly_idle_shards_late_wakes_shed_nothing() {
        for seed in 1..=5u64 {
            let mut r = Rng(seed);
            let mut late = calm_probes(&mut r, 3_000);
            let jitter: Vec<u64> = calm_probes(&mut r, 6_000).into_iter().map(|ns| ns + r.uniform(0.0, 1e6) as u64).collect();
            assert!(quantile_ms(&jitter, 0.99) > 1.0, "seed {seed}: a tick's jitter and the calm's tail");
            late.extend(jitter);
            assert_eq!(first(&watch_shard(&late, &[CALM_BUSY]), 1, 0), None, "seed {seed}: a mostly idle shard's late wakes are the host's: nothing shed");
        }
    }

    /// Step 4: a shard whose probes never come back to their calm (a saturated shard's: a few milliseconds late each,
    /// never under 0.6 ms, the shard running 95% of the time) sheds Background once most of a tolerance's probes were
    /// late (half a tolerance in), Normal a tolerance later, and nothing once most of a tolerance's are back.
    #[test]
    fn g6_s4_a_saturated_shard_sheds_background_then_normal_and_stops_once_back() {
        for seed in 1..=5u64 {
            let mut r = Rng(seed);
            let mut late = calm_probes(&mut r, 3_000);
            // The probe before it on time, so its queue stands from the first saturated one.
            if let Some(last) = late.last_mut() {
                *last = 0;
            }
            late.extend((0..1_500).map(|_| ((5e6f64.ln() + 0.8 * r.normal()).exp() as u64).max(600_000)));
            late.extend(calm_probes(&mut r, 500));
            let busy: Vec<f64> = (0..late.len()).map(|k| if (3_000..4_500).contains(&k) { 0.95 } else { CALM_BUSY }).collect();
            let levels = watch_shard(&late, &busy);
            let background = first(&levels, 1, 0).unwrap_or(usize::MAX);
            assert!((3_045..=3_051).contains(&background), "seed {seed}: Background once most of a tolerance's probes were late: {background}");
            assert_eq!(first(&levels, 2, 0), Some(background + 100), "seed {seed}: Normal a tolerance later");
            assert!(levels.iter().skip(4_551).all(|l| *l == 0), "seed {seed}: nothing once most of a tolerance's probes are back");
        }
    }

    /// The lab's flood (fedora, 2026-11-05 11:01-11:17Z, `--profile flood`, one shard): the shard ran 81 to 82% of the
    /// time and its probes ran 4.2 to 6.4 ms late at the median and 24 ms at p99 (its batches of polls held its timers
    /// and sockets: pages took 16 ms at the median), while a few each second found it idle and ran on time. Lateness, ns.
    fn flood_probes(r: &mut Rng, n: usize) -> Vec<u64> {
        (0..n)
            .map(|_| {
                let us = if r.next() < 0.075 { r.uniform(5.0, 120.0) } else { (6_200f64.ln() + 0.75 * r.normal()).exp().min(25_000.0) };
                (us * 1e3) as u64
            })
            .collect()
    }

    /// Step 4: the lab's flood, replayed: most of every tolerance's probes ran late past their calm while the shard ran
    /// 82% of the time, so Background is shed half a tolerance in and Normal a tolerance later, in every round. Read as
    /// a line, every period past its calm, as the rule before read a shard's queue, the few probes each second that found
    /// the shard idle kept it from standing: in the lab it stood in 3 windows of 12 rounds, and one shed-on round shed.
    #[test]
    fn g6_s4_the_lab_flood_sheds_background_then_normal_in_every_round() {
        let mut stood_as_a_line = 0;
        for seed in 1..=12u64 {
            let mut r = Rng(seed);
            let mut late = calm_probes(&mut r, 3_000);
            let flood = flood_probes(&mut r, 6_000);
            let (p50, p99) = (quantile_ms(&flood, 0.5), quantile_ms(&flood, 0.99));
            assert!((4.0..=7.0).contains(&p50) && (20.0..=25.0).contains(&p99), "seed {seed}: the lab's flood probes (p50 4.2 to 6.4 ms, p99 24 ms), not {p50:.1} and {p99:.1} ms");
            late.extend(flood);
            let busy: Vec<f64> = (0..late.len()).map(|k| if k < 3_000 { CALM_BUSY } else { 0.82 }).collect();
            let levels = watch_shard(&late, &busy);
            assert_eq!(first(&levels, 1, 0).map(|k| k >= 3_000), Some(true), "seed {seed}: nothing shed in the calm before");
            let background = first(&levels, 1, 3_000).unwrap_or(usize::MAX).saturating_sub(3_000);
            assert!((45..=60).contains(&background), "seed {seed}: Background once most of a tolerance's probes were late: {} ms in", background * 10);
            assert_eq!(first(&levels, 2, 3_000).map(|k| k - 3_000), Some(background + 100), "seed {seed}: Normal a tolerance later");
            let mut line = Standing::default();
            let read_as_a_line = late
                .iter()
                .enumerate()
                .map(|(k, &ns)| line.period(Period { now_us: k as u64 * PERIOD_US, wait_ns: Some(ns), longest_ns: 0, waiting: true, floor_ns: PROBE_FLOOR_NS, rule: Rule::Line, busy: None }))
                .max()
                .unwrap_or(0);
            stood_as_a_line += usize::from(read_as_a_line > 0);
        }
        assert!(stood_as_a_line <= 3, "read as a line, the flood stood in {stood_as_a_line} of 12 rounds");
    }

    /// Step 4: a shard sheds Background while either of its queues has stood, Normal while the API's has stood its
    /// second tolerance; Background's own queue, however long it stands, never sheds the API.
    #[test]
    fn g6_s4_a_shard_sheds_normal_only_for_the_apis_own_queue() {
        assert_eq!(shard_level(0, 0), 0);
        assert_eq!(shard_level(0, 1), 1);
        assert_eq!(shard_level(0, 2), 1, "a Background flood alone sheds Background");
        assert_eq!(shard_level(1, 0), 1, "the API's queue standing sheds Background first");
        assert_eq!(shard_level(2, 0), 2);
        assert_eq!(shard_level(2, 2), 2);
    }

    /// Step 4: the calm is learned from what didn't stand: a shard's excursion that never stood is folded into it as its
    /// periods leave the tolerance's window; one that stood is not, nor its onset, so an overload never becomes the
    /// calm it is measured against.
    #[test]
    fn g6_s4_an_excursion_that_never_stood_is_calm_and_one_that_stood_is_not() {
        let mut w = Standing::default();
        let mut now_us = 0;
        let mut step = |w: &mut Standing, ns: u64| {
            now_us += PERIOD_US;
            w.period(Period::of_shard(now_us, ns, QUANTUM_NS, Some(0.95)))
        };
        for _ in 0..1_000 {
            assert_eq!(step(&mut w, 0), 0);
        }
        let floor_us = QUANTUM_NS as f64 / 1e3;
        assert!((w.calm_us(floor_us) - floor_us).abs() < 1e-6, "a calm of nothing waiting: the floor");
        for _ in 0..50 {
            assert_eq!(step(&mut w, 10_000_000), 0, "half a tolerance at 10 ms is not most of one: nothing shed");
        }
        for _ in 0..150 {
            assert_eq!(step(&mut w, 0), 0);
        }
        let learned = w.calm_us(floor_us);
        assert!(learned > floor_us * 1.1, "the excursion that never stood is the calm's own: {learned:.0} µs");
        let mut shed = 0;
        for _ in 0..300 {
            shed = shed.max(step(&mut w, 50_000_000));
        }
        assert_eq!(shed, 2, "three tolerances at 50 ms shed both classes");
        let stops = (1..=100).find(|_| step(&mut w, 0) == 0);
        assert!(stops.is_some_and(|k| k <= 51), "it stops once most of a tolerance is back: {stops:?} periods");
        for _ in 0..150 {
            step(&mut w, 0);
        }
        // Only the calm's own periods since: the waits at the floor after it, which lower it. Twenty of the overload's
        // periods, 200 times the floor, would raise it about 9%; its onset's fifty, about 19%.
        let after = w.calm_us(floor_us);
        assert!(after <= learned, "the overload, its onset with it, is no part of the calm: {after:.1} against {learned:.1} µs");
    }

    /// Step 4, the pools: a pool's period is the mean of the waits told over it; with none told, the floor while
    /// none of its jobs waits, and no sample while some wait untold (their waits come once they start or end).
    #[test]
    fn g6_s4_a_pools_period_is_its_told_waits_mean_the_floor_when_idle_and_nothing_while_untold() {
        let q = |waits, waited_us, waiting| pool_health::Queue { waits, waited_us, waiting };
        assert_eq!(pool_sample(q(13, 3_500, true), q(10, 500, false)), Some(1_000_000), "three waits of a millisecond on average");
        assert_eq!(pool_sample(q(10, 500, false), q(10, 500, true)), Some(0), "none told and none waiting: the floor");
        assert_eq!(pool_sample(q(10, 500, true), q(10, 500, false)), None, "none told while some wait: no sample");
    }

    /// Step 4, the pools: while some wait untold the watch holds where it was, and a line that empties ends its
    /// standing whatever its waits; a line held up by long jobs that end in turn stands once its jobs' waits are told.
    #[test]
    fn g6_s4_a_line_that_empties_is_back_and_untold_waits_hold_it() {
        let mut w = Standing::default();
        let mut now_us = 0;
        let mut step = |w: &mut Standing, wait_ns: Option<u64>, waiting: bool| {
            now_us += PERIOD_US;
            w.period(Period { now_us, wait_ns, longest_ns: wait_ns.unwrap_or(0), waiting, floor_ns: QUANTUM_NS, rule: Rule::Line, busy: None })
        };
        for _ in 0..500 {
            step(&mut w, Some(0), false);
        }
        // Waits of 20 ms told each period, the line never emptying: Background a tolerance and 20 ms in, Normal a
        // tolerance later; untold periods hold it.
        let mut levels = Vec::new();
        for k in 0..300 {
            levels.push(step(&mut w, if k % 3 == 2 { None } else { Some(20_000_000) }, true));
        }
        assert_eq!(first(&levels, 1, 0), Some(102), "Background at the first told period past a tolerance and 20 ms");
        assert_eq!(first(&levels, 2, 0), Some(202), "Normal a tolerance later");
        assert_eq!(step(&mut w, Some(20_000_000), false), 0, "its line emptied: back, whatever it told");
    }

    /// Step 4: Background requests are shed first, then Normal ones, Critical never; the answer is 503 with
    /// `Retry-After` (the class's delay in whole seconds, at least one) and `x-kiln-shed` naming the class.
    #[test]
    fn g6_s4_background_is_shed_first_then_normal_and_critical_never() {
        for (level, background, normal) in [(0u8, false, false), (1, true, false), (2, true, true)] {
            assert!(!sheds(level, Class::Critical), "critical never");
            assert_eq!(sheds(level, Class::Background), background, "background at level {level}");
            assert_eq!(sheds(level, Class::Normal), normal, "normal at level {level}");
        }
        let r = answer(Class::Background, 30_000_000, 40_000, None);
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()), Some("1"));
        assert_eq!(r.headers().get("x-kiln-shed").and_then(|v| v.to_str().ok()), Some("background"));
        let r = answer(Class::Normal, 2_500_000_000, 40_000, None);
        assert_eq!(r.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()), Some("3"), "a delay of 2.5 s: three");
        // Shed for work standing in a pool: its wait says when to come back.
        let r = answer(Class::Normal, 1_500_000_000, 2_000_000, Some(2));
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()), Some("2"), "a pool's wait of 1.5 s: two");
        assert_eq!(r.headers().get("x-kiln-shed").and_then(|v| v.to_str().ok()), Some("normal"));
    }
}
