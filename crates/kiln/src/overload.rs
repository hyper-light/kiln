//! Shards that protect themselves, kiln's side (goal 6, plan 01a1a59c2abe): the class of every request (step
//! 3), shedding by class when a shard's queueing delay leaves its calm (step 4), and what kiln-rt's watchdog
//! sees, on `/metrics` and in the log (step 2).
//!
//! **Classes** (`hyper_rt::coop::Class`). Each request is Critical (health, sign-in, a person's pages and
//! actions), Normal (the API) or Background (agents' and CI's polling, catch-ups, sync, telemetry), read from
//! its method, its path and the credentials it carries ([`class_of`]). The connection's task runs at the most
//! urgent class it serves now ([`Connection`]: an HTTP/2 connection serves many requests in one task), so its
//! shard gives it its class's share and urgency. The credentials are read, not checked: a forged cookie buys
//! a request a place ahead and nothing past its own authentication, which it meets as ever.
//!
//! **Shedding.** Goal 1's probe on each shard (`crate::shard_health`) ticks the shard's detectors every period
//! ([`tick`]), with no probe of this module's own: the API's (Normal) detector reads the shard's delay, how late
//! the probe, a Normal task, ran; Background's reads what the shard's Background tasks waited from joining a
//! ready list to their polls (`Counters::queue_ns_background`). Each detector is headroom's CUSUM on
//! log-latency against the calm it learns (`kiln_ci::headroom::detect`), its threshold calibrated on the calm
//! excursions that ended without harm against a false-alarm budget. Background requests on a shard whose
//! Background or Normal detector alarms are answered 503 at once, before their bodies are read, with
//! `Retry-After` and `x-kiln-shed` saying which class was shed; Normal ones too while the Normal detector
//! alarms; Critical never ([`shed`]). Every number is the platform's or a person's, not picked here:
//! - the period, 10 ms: goal 1's probes', Node's `monitorEventLoopDelay` default (research 01a1a59c2abf, f20);
//! - the tolerance, a queueing delay that is harm at once: Kubernetes' default probe timeout, 1 s
//!   (`crate::headroom::settings::PLATFORM_TOLERANCE_MS`), past which a request waiting fails a health probe of
//!   that timeout (research f18: starvation that fails health checks cascades);
//! - the false alarms a day: headroom's default (`Settings::false_alarms_per_day`, 1), a person's choice there.
//!
//! - the floor of a period's mean, the shard's step quantum: a shorter wait is no queue (a task waits that long
//!   behind the poll running as it woke, whatever the load).
//!
//! A detector alarms when a class's delay leaves its calm toward the tolerance by more than its calm ever did
//! (CUSUM's reference is half the log-distance from calm to the tolerance), so the limit is learned from the
//! shard's own calm: from a calm at the floor, kiln's 250 µs quantum, a class whose mean wait passes about
//! 16 ms (the geometric mean of the floor and the tolerance) is shed within a few periods. While it sheds,
//! nothing is learned as calm: shedding ends the excursions it starts on.
//! Shedding is off until `[server] shed = true` declares it, as what kiln decides on its own is (docs/ci.md
//! §0.3, "Opt-in automation"); every shed says why (its body, `x-kiln-shed`, the log, `/metrics`). The
//! classes and the detectors run either way, on `/metrics` and `/readyz`.
//!
//! **What kiln's clients do**: a shed answer did nothing (the handler never ran), so kiln's HTTP client sends
//! the request again after `Retry-After`, within its deadline, when the body can be sent again
//! (`crate::http::client`). `/readyz` says what is shed, and answers 503 when every shard sheds the API.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use http::{HeaderMap, Method, StatusCode, header};
pub use hyper_rt::coop::Class;
use kiln_ci::headroom::detect::{Detector, Event};

use crate::http::server::{self as srv, Request, Response};

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

/// What one shard sheds and why, written by its monitor (on the shard) and read by the requests served there and
/// by `/metrics`.
struct ShardState {
    holder: hyper_rt::SlotHolder,
    /// 0: nothing; 1: Background requests; 2: Normal and Background.
    level: AtomicU8,
    /// Each class's queueing delay over the monitor's last period, and its calm (0: not known), nanoseconds, by
    /// `Class as usize`.
    delay_ns: [AtomicU64; 3],
    calm_ns: [AtomicU64; 3],
    /// What the shard counted of its tasks yielding at their share (`hyper_rt::coop`), as last read.
    coop_yields: AtomicU64,
}

static SHARDS: OnceLock<Vec<ShardState>> = OnceLock::new();
/// Requests shed, by `Class as usize`.
static SHED: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
/// `[server] shed`: whether alarms shed (they are counted and shown either way).
static ENABLED: AtomicBool = AtomicBool::new(false);

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
            coop_yields: AtomicU64::new(0),
        })
        .collect();
    let count = states.len();
    if SHARDS.set(states).is_err() {
        return 0;
    }
    count
}

/// The detectors' settings: the platform's tolerance and headroom's false-alarm budget (the module's notes).
fn detector(now_us: u64) -> Detector {
    let mut d = Detector::new(f64::from(crate::headroom::settings::PLATFORM_TOLERANCE_MS) * 1e3, now_us);
    // Every alarm here ends by shedding: none of its excursion is calm to learn from.
    d.lag_us = Some(0.0);
    d
}

fn budget_per_day() -> f64 {
    crate::headroom::settings::Settings::default().false_alarms_per_day
}

fn micros_since(base: Instant) -> u64 {
    u64::try_from(base.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// One shard's detectors and what they last read, kept by the shard's own thread: the probe that ticks them runs
/// there alone, so nothing else ever reaches them.
struct Detectors {
    base: Instant,
    /// The API's: on the shard's delay (goal 1's probe, a Normal task, late against its timer).
    normal: Detector,
    /// Background work's: on the Background tasks' waits, from the runtime's counts.
    background: Detector,
    last: hyper_rt::shard_loop::Counters,
    /// The watchdog's newest report of the shard already logged.
    reported: u64,
}

thread_local! {
    static DETECTORS: std::cell::RefCell<Option<Detectors>> = const { std::cell::RefCell::new(None) };
}

/// Shard `index`'s tick, on the shard, from goal 1's probe (`crate::shard_health`, every `shard_health::PERIOD`): `late` is
/// how late the probe ran against its timer, the shard's delay. Feeds the detectors, publishes what to shed, and
/// logs what the watchdog reported of the shard.
///
/// - **Normal** (the API) sheds on the shard's delay: the probe is a Normal task, so its lateness is what the
///   API's own work waits on the shard, the classes ahead of it and its own share counted.
/// - **Background** sheds on what the shard's Background tasks waited, from joining a ready list to their polls,
///   over the period (`Counters::queue_ns_background`): Background work waits behind its own kind alone and a
///   share of the others', so its own flood shows there and not in the API's delay.
///
/// Each floored at the shard's quantum (a shorter wait is no queue). The route waits goal 1 counts
/// (`crate::route_latency`) are no signal here: they run from a request's head read off its socket, which its
/// connection's task reads at Critical (`Connection`) and serves at once, so they do not carry the waits a class
/// makes.
pub fn tick(index: usize, late: Duration) {
    let (Some(state), Some(now)) = (shards().get(index), hyper_rt::futures::shard_counters()) else {
        return;
    };
    let floor_ns = hyper_rt::futures::step_budget_ns().unwrap_or(0);
    DETECTORS.with(|cell| {
        let mut held = cell.borrow_mut();
        let d = held.get_or_insert_with(|| Detectors { base: Instant::now(), normal: detector(0), background: detector(0), last: now, reported: 0 });
        let now_us = micros_since(d.base);
        let last = d.last;
        let waits = [
            mean_ns(now.queue_ns_background, last.queue_ns_background, now.queue_polls_background, last.queue_polls_background),
            mean_ns(now.queue_ns_normal, last.queue_ns_normal, now.queue_polls_normal, last.queue_polls_normal),
            mean_ns(now.queue_ns_critical, last.queue_ns_critical, now.queue_polls_critical, last.queue_polls_critical),
        ];
        let late_ns = u64::try_from(late.as_nanos()).unwrap_or(u64::MAX);
        let events = [feed(&mut d.background, waits[0], floor_ns, now_us), feed(&mut d.normal, Some(late_ns), floor_ns, now_us)];
        publish(state, waits, late_ns, d);
        let level = if d.normal.alarm.is_some() {
            2
        } else if d.background.alarm.is_some() {
            1
        } else {
            0
        };
        if state.level.swap(level, Ordering::Relaxed) != level {
            note_level(index, state, level, &events);
        }
        state.coop_yields.store(now.coop_yields, Ordering::Relaxed);
        d.reported = log_stall(index, state.holder, d.reported);
        d.last = now;
    });
}

/// The delays and calms a tick read, for answers, `/metrics` and `/readyz`: each class's mean wait from the
/// runtime's counts, but Normal's, which is the shard's delay its detector reads.
fn publish(state: &ShardState, waits: [Option<u64>; 3], late_ns: u64, d: &Detectors) {
    let delays = [waits[0], Some(late_ns), waits[2]];
    for (class, delay) in [Class::Background, Class::Normal, Class::Critical].into_iter().zip(delays) {
        if let (Some(delay), Some(slot)) = (delay, state.delay_ns.get(class as usize)) {
            slot.store(delay, Ordering::Relaxed);
        }
    }
    for (class, detector) in [(Class::Background, &d.background), (Class::Normal, &d.normal)] {
        if let Some(calm) = state.calm_ns.get(class as usize) {
            calm.store(detector.calm_us().map_or(0, |us| (us * 1e3) as u64), Ordering::Relaxed);
        }
    }
}

/// The mean of a class's waits over a period, from the counts before and after: `None` with no poll of it.
fn mean_ns(sum: u64, sum_before: u64, polls: u64, polls_before: u64) -> Option<u64> {
    sum.saturating_sub(sum_before).checked_div(polls.saturating_sub(polls_before))
}

/// One period's mean wait to a class's detector, in microseconds, never under `floor_ns` (the shard's quantum: a
/// shorter wait is no queue). A class with no poll in the period had nothing waiting: the floor, so an alarm ends
/// once its work is gone, as when shedding sent it away. The floor also keeps the scheduler's sub-quantum noise
/// (a task polled at once, a wait of nanoseconds) out of the calm: without it, a calm learned above it by a few
/// hundred microseconds read a wait of nanoseconds as a shift down, a new regime, whose first sample, whatever it
/// was, became the calm.
fn feed(d: &mut Detector, mean_ns: Option<u64>, floor_ns: u64, now_us: u64) -> Event {
    let ns = mean_ns.unwrap_or(0).max(floor_ns).max(1);
    d.sample(now_us, ns as f64 / 1e3, false, budget_per_day())
}

/// The log of a change of what a shard sheds, with why.
fn note_level(index: usize, state: &ShardState, level: u8, events: &[Event; 2]) {
    let why: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::Alarm(why) | Event::Regime(why) => Some(why.as_str()),
            _ => None,
        })
        .collect();
    let delay = |class: Class| state.delay_ns.get(class as usize).map_or(0, |d| d.load(Ordering::Relaxed));
    match level {
        0 => tracing::info!(shard = index, "shard no longer sheds requests: its queueing delay is back at its calm"),
        _ => tracing::warn!(
            shard = index,
            sheds = if level >= 2 { "normal and background" } else { "background" },
            normal_delay_us = delay(Class::Normal) / 1_000,
            background_delay_us = delay(Class::Background) / 1_000,
            why = %why.join("; "),
            shedding = ENABLED.load(Ordering::Relaxed),
            "shard overloaded: its queueing delay left its calm"
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
/// `Retry-After` the class's queueing delay in whole seconds (at least one, HTTP's unit), `x-kiln-shed` its class.
pub fn shed(class: Class) -> Option<Response> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let state = here()?;
    if !sheds(state.level.load(Ordering::Relaxed), class) {
        return None;
    }
    if let Some(n) = SHED.get(class as usize) {
        n.fetch_add(1, Ordering::Relaxed);
    }
    let delay_ns = state.delay_ns.get(class as usize).map_or(0, |d| d.load(Ordering::Relaxed));
    let calm_ns = state.calm_ns.get(class as usize).map_or(0, |d| d.load(Ordering::Relaxed));
    Some(answer(class, delay_ns, calm_ns))
}

/// Whether a shard at `level` sheds a request of `class`: Background from level 1, Normal from 2, Critical never.
fn sheds(level: u8, class: Class) -> bool {
    match class {
        Class::Background => level >= 1,
        Class::Normal => level >= 2,
        Class::Critical => false,
    }
}

/// The answer to a request of `class` shed at a queueing delay of `delay_ns` against a calm of `calm_ns`.
fn answer(class: Class, delay_ns: u64, calm_ns: u64) -> Response {
    let retry = delay_ns.div_ceil(1_000_000_000).max(1);
    let why = format!(
        "overloaded: this server's shard sheds {} requests now (its queueing delay {:.1} ms, calm {:.2} ms); try again in {retry} s",
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

/// What `/readyz` says of overload: `None` when no shard sheds; else whether every shard sheds the API (Normal),
/// and a line saying what each sheds.
pub fn readiness() -> Option<(bool, String)> {
    let all = shards();
    let levels: Vec<u8> = all.iter().map(|s| s.level.load(Ordering::Relaxed)).collect();
    if levels.iter().all(|l| *l == 0) {
        return None;
    }
    // Not ready only while every shard refuses the API; with shedding off, nothing is refused.
    let every = ENABLED.load(Ordering::Relaxed) && !levels.is_empty() && levels.iter().all(|l| *l >= 2);
    let mut line = String::from("overloaded:");
    for (index, level) in levels.iter().enumerate().filter(|(_, l)| **l > 0) {
        let what = if *level >= 2 { "normal and background" } else { "background" };
        let _ = write!(line, " shard {index} sheds {what} requests;");
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

/// The watchdog's findings, each shard's classes' queueing delay and what it sheds, and the requests shed.
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
    let _ = writeln!(o, "# HELP kiln_shard_shedding 1 while the shard sheds the class's requests.\n# TYPE kiln_shard_shedding gauge");
    for (index, state) in shards().iter().enumerate() {
        for class in [Class::Critical, Class::Normal, Class::Background] {
            let delay = state.delay_ns.get(class as usize).map_or(0, |d| d.load(Ordering::Relaxed));
            let _ = writeln!(o, "kiln_shard_queue_delay_seconds{{shard=\"{index}\",class=\"{}\"}} {:.6}", class.as_str(), delay as f64 / 1e9);
        }
        let level = state.level.load(Ordering::Relaxed);
        let _ = writeln!(o, "kiln_shard_shedding{{shard=\"{index}\",class=\"background\"}} {}", u8::from(level >= 1));
        let _ = writeln!(o, "kiln_shard_shedding{{shard=\"{index}\",class=\"normal\"}} {}", u8::from(level >= 2));
    }
    let _ = writeln!(o, "# HELP kiln_shed_total Requests answered 503 at once because their shard was overloaded, by class.\n# TYPE kiln_shed_total counter");
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

    /// Step 4: a detector fed a class's mean waits alarms within a few periods of the waits leaving the calm it
    /// learned (waits of nanoseconds to a few hundred microseconds under a quantum of 250, then 50 ms and 100 ms: a
    /// busy shard's round of tasks), stays quiet through the calm with its noise, also after a load that only
    /// doubled the calm (Background's busy, Normal's waits a poll), and clears once the waits are back, or once the
    /// class has nothing waiting at all.
    #[test]
    fn g6_s4_a_detector_alarms_soon_after_the_waits_leave_their_calm_and_clears_after() {
        const QUANTUM_NS: u64 = 250_000;
        for overload_ns in [50_000_000u64, 100_000_000] {
            let mut d = detector(0);
            let mut now_us = 0;
            for i in 0..2_000u64 {
                now_us += 10_000;
                let wait = [20, 1_000, 40_000, 125_000, 300_000, 3, 600].get((i % 7) as usize).copied().unwrap_or(0);
                assert!(!matches!(feed(&mut d, Some(wait), QUANTUM_NS, now_us), Event::Alarm(_)), "calm, sample {i}");
            }
            for _ in 0..500u64 {
                now_us += 10_000;
                feed(&mut d, Some(2 * QUANTUM_NS), QUANTUM_NS, now_us);
            }
            assert!(d.alarm.is_none(), "a doubled calm is no overload");
            let mut alarmed_after = None;
            for i in 0..20u64 {
                now_us += 10_000;
                feed(&mut d, Some(overload_ns), QUANTUM_NS, now_us);
                if d.alarm.is_some() {
                    alarmed_after = Some(i + 1);
                    break;
                }
            }
            assert!(alarmed_after.is_some_and(|n| n <= 5), "alarmed after {alarmed_after:?} periods at {overload_ns} ns");
            for _ in 0..50 {
                now_us += 10_000;
                feed(&mut d, None, QUANTUM_NS, now_us);
            }
            assert!(d.alarm.is_none(), "cleared once nothing waits");
        }
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
        let r = answer(Class::Background, 30_000_000, 40_000);
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()), Some("1"));
        assert_eq!(r.headers().get("x-kiln-shed").and_then(|v| v.to_str().ok()), Some("background"));
        let r = answer(Class::Normal, 2_500_000_000, 40_000);
        assert_eq!(r.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()), Some("3"), "a delay of 2.5 s: three");
    }
}
