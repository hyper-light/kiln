//! U15: operational conversation-store telemetry, separate from Amphora replication status.
//! Reports describe the server's and runners' own work; legacy mirror reports are ignored.
//! Series and notes retain bounded recent observations without custom feed/copy tracking.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;

use super::telemetry::{PeerView, Report, ShipView, StoreReport, Upkeeping, WAIT_BUCKETS, wait_quantile};

/// How often the server samples the board.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(5);
/// Each store's series: fine, an hour at 5 s; coarse, a day at a minute.
const FINE: (i64, usize) = (5_000, 720);
const COARSE: (i64, usize) = (60_000, 1440);
const DAY_MS: i64 = COARSE.0 * COARSE.1 as i64;
/// What happened lately, at most.
const NOTES: usize = 200;
/// The replication log's lines, at most.
const LOG: usize = 5_000;
/// A runner's report older than this isn't current (it reports every few seconds).
const STALE_MS: i64 = 30_000;
/// Reports and series retain at most this many store identities. The oldest observation is
/// evicted when a new identity would exceed this bound.
const STORES: usize = 512;
/// The server's store's key.
pub const SERVER: &str = "server";

/// A machine that runs kiln's runner, as the server knows it.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub name: String,
    pub connected: bool,
    pub version: String,
}

/// What the server sees now, for the board.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub now: i64,
    /// Its own store (None: it keeps none).
    pub server: Option<StoreReport>,
    pub version: String,
    pub machines: Vec<Machine>,
    /// Compatibility restore response supplied by `Registry::restore_view`.
    pub restore: serde_json::Value,
}

/// One store, as the board shows it.
#[derive(Debug, Clone, Serialize)]
pub struct StoreStatus {
    /// Its key, for its series: `server`, `<machine>/<store>`, or `<machine>/`.
    pub key: String,
    /// `server` or `runner`.
    pub role: &'static str,
    pub machine: String,
    /// The store's id (None: a machine that keeps none).
    pub id: Option<String>,
    /// The kiln it runs.
    pub version: Option<String>,
    pub connected: bool,
    /// `live` means a current own-store report; `offline` means no current connection/report.
    /// `no_store` means a connected machine has not reported a store. No state asserts replication.
    pub state: &'static str,
    /// Unix ms of what the board last heard of it.
    pub heard: Option<i64>,
    /// What it has done since its kiln started, and its file (None: it hasn't said: a runner too
    /// old, or not heard from since the server started).
    pub report: Option<StoreReport>,
    /// Retired custom-feed compatibility field; always absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed: Option<FeedStatus>,
    /// Retired custom-mirror compatibility field; always absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<MirrorStatus>,
    /// Over the last minute, per second.
    pub rates: Rates,
    /// Writes per second over the last ten minutes, a point every 5 s (None: not known then).
    pub spark: Vec<Option<f64>>,
}

/// Retired custom-feed shape retained for existing status consumers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FeedStatus {
    /// Through where the server has it.
    pub rev: u64,
    /// Its last write (None: it hasn't said), and how many rows the server lacks of it.
    pub top: Option<u64>,
    pub behind: Option<u64>,
    /// Unix ms the server last took from it (0: never).
    pub at: i64,
    /// The server's side, since it started (None: not heard since).
    pub intake: Option<PeerView>,
    /// The runner's side (None: it hasn't said).
    pub ship: Option<ShipView>,
}

/// Retired custom-mirror shape retained for existing status consumers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MirrorStatus {
    pub chosen_at: i64,
    pub last_seen: i64,
    pub ship: ShipView,
    /// How many rows of the server's store its copy lacks (None: not known now).
    pub behind: Option<u64>,
}

/// Per second, over the last minute (None: not known).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Rates {
    /// Rows written into the store.
    pub writes: Option<f64>,
    /// Replication batches are unknown until supplied by Amphora.
    pub batches: Option<f64>,
    /// Replication wire bytes are unknown until supplied by Amphora.
    pub wire: Option<f64>,
    /// How much of the time the store's writer was busy (0 to 1).
    pub busy: Option<f64>,
}

/// Something that happened to a store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    pub at: i64,
    pub key: String,
    /// `info`, `warn` or `error`.
    pub level: &'static str,
    pub text: String,
}

/// A compatibility log line (`GET /api/conversations/log`). The board emits operational
/// notes only; it cannot infer replication traffic from conversation counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLine {
    pub at: i64,
    /// Its store's key on the board.
    pub key: String,
    pub kind: &'static str,
    /// `info`, `warn` or `error`.
    pub level: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved: Option<Moved>,
}

/// Retired traffic payload shape retained for existing log consumers; the board never emits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Moved {
    pub batches: u64,
    pub rows: u64,
    pub bytes: u64,
    pub rev: Option<u64>,
}

/// The board, as `GET /api/conversations/status` answers.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub at: i64,
    /// The server keeps a store.
    pub on: bool,
    pub mirrors_wanted: usize,
    pub restore: serde_json::Value,
    pub stores: Vec<StoreStatus>,
    /// Newest first.
    pub notes: Vec<Note>,
}

/// One store's sample: counters as they stand (rates come from the difference).
#[derive(Debug, Clone, Copy, Default)]
struct Point {
    t: i64,
    /// It said what it did ([`StoreStatus::report`]).
    has: bool,
    bytes: u64,
    free: u64,
    wal: u64,
    sessions: Option<u64>,
    writes: u64,
    commits: u64,
    busy_ms: u64,
    timeouts: u64,
    dropped: u64,
    p50: Option<u64>,
    p99: Option<u64>,
    up: bool,
}

struct Tier {
    step: i64,
    cap: usize,
    points: VecDeque<Point>,
    /// Waits counted when it last took a point, for the quantiles of those since.
    waits: Vec<u64>,
}

impl Tier {
    fn new((step, cap): (i64, usize)) -> Self {
        Self { step, cap, points: VecDeque::new(), waits: Vec::new() }
    }

    /// A point, if it's time for one: the fine tier takes each sample (one that comes too soon
    /// after the last aside); the coarse one, the first of each of its steps.
    fn push(&mut self, mut p: Point, waits: &[u64]) {
        if self.points.back().is_some_and(|last| if self.step == FINE.0 { p.t.saturating_sub(last.t) < self.step / 2 } else { p.t.div_euclid(self.step) == last.t.div_euclid(self.step) }) {
            return;
        }
        // The waits since the last point (all of them, if the counters began again).
        let since: Vec<u64> = if self.waits.len() == waits.len() && self.waits.iter().zip(waits).all(|(a, b)| a <= b) { waits.iter().zip(&self.waits).map(|(b, a)| b - a).collect() } else { waits.to_vec() };
        (p.p50, p.p99) = (wait_quantile(&since, 0.5), wait_quantile(&since, 0.99));
        self.waits = waits.to_vec();
        self.points.push_back(p);
        while self.points.len() > self.cap {
            self.points.pop_front();
        }
    }
}

struct Series {
    fine: Tier,
    coarse: Tier,
}

impl Default for Series {
    fn default() -> Self {
        Self { fine: Tier::new(FINE), coarse: Tier::new(COARSE) }
    }
}

/// A rate per second between two counters `dt_ms` apart (None: they began again between).
#[allow(clippy::cast_precision_loss)]
fn rate(a: u64, b: u64, dt_ms: i64) -> Option<f64> {
    (b >= a && dt_ms > 0).then(|| (b - a) as f64 * 1000.0 / dt_ms as f64)
}

/// A store's series over a window, a column each, as `GET /api/conversations/series` answers:
/// rates per second, sizes in bytes, waits in ms, `t` in unix seconds.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SeriesView {
    pub key: String,
    /// Seconds between points.
    pub step: i64,
    pub t: Vec<i64>,
    pub bytes: Vec<Option<u64>>,
    pub free: Vec<Option<u64>>,
    pub wal: Vec<Option<u64>>,
    pub sessions: Vec<Option<u64>>,
    pub writes: Vec<Option<f64>>,
    pub commits: Vec<Option<f64>>,
    pub busy: Vec<Option<f64>>,
    pub p50: Vec<Option<u64>>,
    pub p99: Vec<Option<u64>>,
    pub timeouts: Vec<Option<u64>>,
    pub dropped: Vec<Option<u64>>,
    pub behind: Vec<Option<u64>>,
    pub batches: Vec<Option<f64>>,
    pub wire: Vec<Option<f64>>,
    pub up: Vec<u8>,
}

impl SeriesView {
    fn of(key: &str, tier: &Tier, since: i64) -> Self {
        let mut v = SeriesView { key: key.to_owned(), step: tier.step / 1000, ..Default::default() };
        let mut prev: Option<&Point> = None;
        for p in &tier.points {
            if p.t >= since {
                let both = prev.filter(|q| q.has && p.has);
                let dt = prev.map_or(0, |q| p.t.saturating_sub(q.t));
                let have = |x: u64| p.has.then_some(x);
                v.t.push(p.t / 1000);
                v.bytes.push(have(p.bytes));
                v.free.push(have(p.free));
                v.wal.push(have(p.wal));
                v.sessions.push(p.sessions.filter(|_| p.has));
                v.writes.push(both.and_then(|q| rate(q.writes, p.writes, dt)));
                v.commits.push(both.and_then(|q| rate(q.commits, p.commits, dt)));
                v.busy.push(both.and_then(|q| rate(q.busy_ms, p.busy_ms, dt)).map(|ms| (ms / 1000.0).min(1.0)));
                v.p50.push(p.p50.filter(|_| p.has));
                v.p99.push(p.p99.filter(|_| p.has));
                v.timeouts.push(both.and_then(|q| p.timeouts.checked_sub(q.timeouts)));
                v.dropped.push(both.and_then(|q| p.dropped.checked_sub(q.dropped)));
                v.behind.push(None);
                v.batches.push(None);
                v.wire.push(None);
                v.up.push(u8::from(p.up));
            }
            prev = Some(p);
        }
        v
    }
}

/// What a runner said of one of its stores, and when.
struct Heard {
    machine: String,
    at: i64,
    report: Report,
}

/// What the board noted of a store at its last sample, to note what changed since.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Brief {
    connected: bool,
    dropped: u64,
    timeouts: u64,
    compacting: bool,
    compacted: i64,
    pruned: i64,
}

/// The server's board.
#[derive(Default)]
pub struct Board {
    heard: Mutex<HashMap<String, Heard>>,
    series: Mutex<HashMap<String, Series>>,
    notes: Mutex<VecDeque<Note>>,
    /// The compatibility operational log, oldest first.
    log: Mutex<VecDeque<LogLine>>,
    /// The last sample's operational counters.
    last: Mutex<Option<HashMap<String, Brief>>>,
}

fn fresh(at: i64, now: i64) -> bool {
    now.saturating_sub(at) <= STALE_MS
}

impl Board {
    /// U15: accept only a runner's own operational report. The retired feed payload is
    /// discarded, histogram storage is bounded, and oldest reports yield to newer identities.
    pub fn heard(&self, machine: &str, store: &str, mut report: Report, now: i64) {
        if report.role != "own" {
            return;
        }
        report.feed = None;
        report.store.waits.truncate(WAIT_BUCKETS);
        // Invalid remote histogram counters cannot produce a quantile; keep it unknown.
        if report.store.waits.iter().try_fold(0u64, |n, v| n.checked_add(*v)).is_none() {
            report.store.waits.clear();
        }
        let key = format!("{machine}/{store}");
        let mut heard = self.heard.lock().unwrap_or_else(PoisonError::into_inner);
        if !heard.contains_key(&key) && heard.len() >= STORES {
            let oldest = heard.iter().min_by_key(|(_, h)| h.at).map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                heard.remove(&oldest);
            }
        }
        heard.insert(key, Heard { machine: machine.to_owned(), at: now, report });
    }

    /// Every store as the board sees it now.
    pub fn status(&self, seen: &Seen) -> Status {
        let mut stores = self.stores(seen);
        {
            let series = self.series.lock().unwrap_or_else(PoisonError::into_inner);
            for s in &mut stores {
                if let Some(ser) = series.get(&s.key) {
                    (s.rates, s.spark) = rates_and_spark(&ser.fine, seen.now);
                }
            }
        }
        let notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner).iter().rev().cloned().collect();
        Status { at: seen.now, on: seen.server.is_some(), mirrors_wanted: 0, restore: seen.restore.clone(), stores, notes }
    }

    /// A sample: each store's series takes a point, and what changed since the last sample is
    /// noted. The board as it stands then.
    pub fn sample(&self, seen: &Seen) -> Status {
        let stores = self.stores(seen);
        {
            let mut series = self.series.lock().unwrap_or_else(PoisonError::into_inner);
            // Expired and oldest series yield before a new store identity is sampled.
            series.retain(|_, ser| ser.fine.points.back().is_some_and(|p| seen.now.saturating_sub(p.t) < DAY_MS));
            for s in &stores {
                let (p, waits) = point(s, seen.now);
                if !series.contains_key(&s.key) && series.len() >= STORES {
                    let oldest = series.iter().min_by_key(|(_, ser)| ser.fine.points.back().map(|p| p.t)).map(|(key, _)| key.clone());
                    if let Some(oldest) = oldest {
                        series.remove(&oldest);
                    }
                }
                let ser = series.entry(s.key.clone()).or_default();
                ser.fine.push(p, &waits);
                ser.coarse.push(p, &waits);
            }
        }
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let briefs: HashMap<String, Brief> = stores.iter().map(|s| (s.key.clone(), brief(s))).collect();
        let mut new = Vec::new();
        match last.as_ref() {
            None => new.push(Note { at: seen.now, key: SERVER.into(), level: "info", text: format!("telemetry began: {} stores", stores.iter().filter(|s| s.id.is_some()).count()) }),
            Some(before) => {
                for s in &stores {
                    if let Some(now_b) = briefs.get(&s.key) {
                        notes_of(s, before.get(&s.key), now_b, seen.now, &mut new);
                    }
                }
            }
        }
        *last = Some(briefs);
        drop(last);
        let lines: Vec<LogLine> = new.iter().map(|n| LogLine { at: n.at, key: n.key.clone(), kind: "note", level: n.level, text: n.text.clone(), moved: None }).collect();
        if !lines.is_empty() {
            let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
            log.extend(lines);
            while log.len() > LOG {
                log.pop_front();
            }
        }
        if !new.is_empty() {
            let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
            notes.extend(new);
            while notes.len() > NOTES {
                notes.pop_front();
            }
        }
        self.status(seen)
    }

    /// The replication log, newest first: the lines after `since` (unix ms) that have `q` in their
    /// words or their store's key (any case; empty: every line), at most `limit`.
    pub fn log(&self, since: i64, q: &str, limit: usize) -> Vec<LogLine> {
        let q = q.trim().to_lowercase();
        let has = |l: &LogLine| q.is_empty() || l.text.to_lowercase().contains(&q) || l.key.to_lowercase().contains(&q);
        self.log.lock().unwrap_or_else(PoisonError::into_inner).iter().rev().take_while(|l| l.at > since).filter(|l| has(l)).take(limit).cloned().collect()
    }

    /// How many lines the replication log holds, and how many of them have `q` (as [`Board::log`]).
    pub fn log_len(&self, q: &str) -> (usize, usize) {
        let q = q.trim().to_lowercase();
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let matched = if q.is_empty() { log.len() } else { log.iter().filter(|l| l.text.to_lowercase().contains(&q) || l.key.to_lowercase().contains(&q)).count() };
        (log.len(), matched)
    }

    /// Store `key`'s series over the last `window_s` seconds (an hour or less: at 5 s; more: at a
    /// minute, up to a day).
    pub fn series(&self, key: &str, window_s: i64, now: i64) -> Option<SeriesView> {
        let series = self.series.lock().unwrap_or_else(PoisonError::into_inner);
        let ser = series.get(key)?;
        let tier = if window_s <= FINE.0 / 1000 * FINE.1 as i64 { &ser.fine } else { &ser.coarse };
        Some(SeriesView::of(key, tier, now.saturating_sub(window_s.clamp(1, DAY_MS / 1000).saturating_mul(1000))))
    }

    /// The stores, from what the server sees and what runners said.
    fn stores(&self, seen: &Seen) -> Vec<StoreStatus> {
        let now = seen.now;
        let heard = self.heard.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = Vec::new();
        if let Some(r) = &seen.server {
            out.push(StoreStatus {
                key: SERVER.into(),
                role: "server",
                machine: SERVER.into(),
                id: Some(r.id.clone()),
                version: Some(seen.version.clone()),
                connected: true,
                state: "live",
                heard: Some(r.file.at),
                report: Some(r.clone()),
                feed: None,
                mirror: None,
                rates: Rates::default(),
                spark: Vec::new(),
            });
        }
        let machine = |name: &str| seen.machines.iter().find(|m| m.name == name);
        // Current own-store reports are shown together. If a machine has only old reports,
        // retain its latest one as an offline operational observation.
        let mut by_machine: HashMap<&str, Vec<(&String, &Heard)>> = HashMap::new();
        for (key, report) in heard.iter() {
            by_machine.entry(&report.machine).or_default().push((key, report));
        }
        let mut shown = Vec::new();
        for mut reports in by_machine.into_values() {
            if reports.iter().any(|(_, h)| fresh(h.at, now)) {
                reports.retain(|(_, h)| fresh(h.at, now));
            } else {
                reports.sort_by_key(|(_, h)| std::cmp::Reverse(h.at));
                reports.truncate(1);
            }
            shown.extend(reports);
        }
        shown.sort_by_key(|(key, _)| *key);
        for (key, said) in shown {
            if out.len() >= STORES { break; }
            let mach = machine(&said.machine);
            let connected = fresh(said.at, now) && mach.is_none_or(|m| m.connected);
            out.push(StoreStatus {
                key: key.clone(),
                role: "runner",
                machine: said.machine.clone(),
                id: Some(said.report.store.id.clone()),
                version: Some(said.report.version.clone()),
                connected,
                state: if connected { "live" } else { "offline" },
                heard: Some(said.at),
                report: Some(said.report.store.clone()),
                feed: None,
                mirror: None,
                rates: Rates::default(),
                spark: Vec::new(),
            });
        }
        // A connected machine without an own-store report supplies no evidence about a store.
        for m in seen.machines.iter().take(STORES) {
            if out.len() >= STORES { break; }
            if !out.iter().any(|s| s.role == "runner" && s.machine == m.name) {
                out.push(StoreStatus {
                    key: format!("{}/", m.name),
                    role: "runner",
                    machine: m.name.clone(),
                    id: None,
                    version: Some(m.version.clone()),
                    connected: m.connected,
                    state: if m.connected { "no_store" } else { "offline" },
                    heard: None,
                    report: None,
                    feed: None,
                    mirror: None,
                    rates: Rates::default(),
                    spark: Vec::new(),
                });
            }
        }
        out
    }
}

/// A store's point now, and its waits counted so far.
fn point(s: &StoreStatus, now: i64) -> (Point, Vec<u64>) {
    let mut p = Point { t: now, up: s.connected, ..Point::default() };
    let mut waits = Vec::new();
    if let Some(r) = &s.report {
        p.has = s.connected;
        (p.bytes, p.free, p.wal, p.sessions, p.writes) = (r.file.bytes, r.file.free, r.file.wal, r.file.sessions, r.rows);
        (p.commits, p.busy_ms, p.timeouts, p.dropped) = (r.commits, r.busy_ms, r.timeouts, r.dropped);
        waits.extend(r.waits.iter().copied().take(WAIT_BUCKETS));
        if waits.iter().try_fold(0u64, |n, v| n.checked_add(*v)).is_none() {
            waits.clear();
        }
    }
    (p, waits)
}

/// Rates over the last minute, and writes per second over the last ten minutes.
fn rates_and_spark(t: &Tier, now: i64) -> (Rates, Vec<Option<f64>>) {
    let recent: Vec<&Point> = t.points.iter().filter(|p| now.saturating_sub(p.t) <= 10 * 60_000).collect();
    let spark = recent
        .windows(2)
        .map(|w| match w {
            [a, b] if a.has && b.has => rate(a.writes, b.writes, b.t.saturating_sub(a.t)),
            _ => None,
        })
        .collect();
    let mut r = Rates::default();
    if let (Some(last), Some(first)) = (t.points.back(), t.points.iter().find(|p| now.saturating_sub(p.t) <= 65_000)) {
        let dt = last.t.saturating_sub(first.t);
        if dt > 0 && first.has && last.has {
            r.writes = rate(first.writes, last.writes, dt);
            r.busy = rate(first.busy_ms, last.busy_ms, dt).map(|ms| (ms / 1000.0).min(1.0));
        }
    }
    (r, spark)
}

/// The operational counters used to note a store's changes at the next sample.
fn brief(s: &StoreStatus) -> Brief {
    let file = s.report.as_ref().map(|r| &r.file);
    Brief {
        connected: s.connected,
        dropped: s.report.as_ref().map_or(0, |r| r.dropped),
        timeouts: s.report.as_ref().map_or(0, |r| r.timeouts),
        compacting: file.is_some_and(|f| matches!(f.upkeep, Upkeeping::Compacting(_))),
        compacted: file.and_then(|f| f.compacted.as_ref()).map_or(0, |c| c.at),
        pruned: file.and_then(|f| f.pruned.as_ref()).map_or(0, |p| p.at),
    }
}

/// How a store is named in a note.
fn named(s: &StoreStatus) -> String {
    match s.role {
        "server" => "the server's store".to_owned(),
        _ => format!("{}'s store", s.machine),
    }
}

#[allow(clippy::cast_precision_loss)]
fn gb(bytes: u64) -> String {
    if bytes >= 1 << 30 { format!("{:.2} GB", bytes as f64 / f64::from(1u32 << 30)) } else { format!("{:.0} MB", bytes as f64 / f64::from(1u32 << 20)) }
}

/// What changed of store `s` since the last sample (`was`; None: it wasn't on the board then).
fn notes_of(s: &StoreStatus, was: Option<&Brief>, now_b: &Brief, at: i64, out: &mut Vec<Note>) {
    let mut note = |level: &'static str, text: String| out.push(Note { at, key: s.key.clone(), level, text });
    let name = named(s);
    let Some(was) = was else {
        if s.role == "runner" && s.id.is_some() {
            note("info", format!("{name} reported its own activity"));
        }
        return;
    };
    if was.connected != now_b.connected && s.id.is_some() {
        if now_b.connected { note("info", format!("{} is back", s.machine)) } else { note("warn", format!("{name} is offline")) }
    }
    if now_b.dropped > was.dropped {
        note("error", format!("{name}: the writer couldn't keep up; {} updates were dropped", now_b.dropped - was.dropped));
    }
    if now_b.timeouts > was.timeouts {
        note("warn", format!("{name}: {} reads didn't get an answer in time", now_b.timeouts - was.timeouts));
    }
    let file = s.report.as_ref().map(|r| &r.file);
    if now_b.compacting && !was.compacting
        && let Some(f) = file
    {
        note("info", format!("{name}: making its file compact ({}, {} free)", gb(f.bytes), gb(f.free)));
    }
    if now_b.compacted > was.compacted
        && let Some(c) = file.and_then(|f| f.compacted.as_ref())
    {
        note("info", format!("{name}: its file was made compact, {} to {}, in {} s", gb(c.before), gb(c.after), c.ms / 1000));
    }
    if now_b.pruned > was.pruned
        && let Some(p) = file.and_then(|f| f.pruned.as_ref())
    {
        note("info", format!("{name}: {} transcript lines of agents that ended long ago were let go", p.lines));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convo::telemetry::FileView;

    fn report(id: &str, rows: u64) -> StoreReport {
        StoreReport { id: id.into(), rows, file: FileView { at: 1, bytes: 10 << 20, free: 1 << 20, top: rows, ..FileView::default() }, waits: vec![0; WAIT_BUCKETS], ..StoreReport::default() }
    }
    fn own(id: &str, rows: u64) -> Report {
        Report { role: "own".into(), version: "test".into(), store: report(id, rows), feed: Some(ShipView { acked: Some(rows), batches: 5, bytes: 500, ..ShipView::default() }) }
    }
    fn seen(now: i64) -> Seen {
        Seen {
            now,
            server: Some(report("srv", 500)),
            version: "test".into(),
            machines: vec![Machine { name: "fedora".into(), connected: true, version: "test".into() }, Machine { name: "jupiter".into(), connected: false, version: "old".into() }],
            restore: serde_json::json!({ "state": "idle" }),
        }
    }
    fn store<'a>(st: &'a Status, key: &str) -> anyhow::Result<&'a StoreStatus> {
        st.stores.iter().find(|s| s.key == key).ok_or_else(|| anyhow::anyhow!("no {key} on the board"))
    }

    #[test]
    fn u15_own_reports_show_activity_without_claiming_replication_and_ignore_mirror_reports() -> anyhow::Result<()> {
        let b = Board::default();
        b.heard("fedora", "abc", own("abc", 100), 1_000);
        let mut mirror = own("abc", 999);
        mirror.role = "mirror".into();
        b.heard("fedora", "abc", mirror, 1_500);
        let st = b.status(&seen(2_000));
        let keys: Vec<(&str, &str)> = st.stores.iter().map(|s| (s.key.as_str(), s.state)).collect();
        assert_eq!(keys, vec![("server", "live"), ("fedora/abc", "live"), ("jupiter/", "offline")]);
        let runner = store(&st, "fedora/abc")?;
        assert_eq!(runner.report.as_ref().map(|r| r.rows), Some(100));
        assert!(runner.feed.is_none() && runner.mirror.is_none());
        assert_eq!(st.mirrors_wanted, 0);
        assert_eq!(store(&b.status(&seen(100_000)), "fedora/abc")?.state, "offline");
        Ok(())
    }

    #[test]
    fn u15_series_keep_own_operation_rates_and_unknown_replication_columns() -> anyhow::Result<()> {
        let b = Board::default();
        let mut s = seen(0);
        b.heard("fedora", "abc", own("abc", 100), 0);
        b.sample(&s);
        s.now = 5_000;
        let mut report = report("srv", 1_500);
        report.busy_ms = 500;
        report.timeouts = 2;
        s.server = Some(report);
        b.heard("fedora", "abc", own("abc", 150), 5_000);
        let st = b.sample(&s);
        let server = b.series("server", 3600, 5_000).ok_or_else(|| anyhow::anyhow!("missing server series"))?;
        assert_eq!(server.t, vec![0, 5]);
        assert_eq!(server.writes, vec![None, Some(200.0)]);
        assert_eq!(server.busy, vec![None, Some(0.1)]);
        let runner = b.series("fedora/abc", 3600, 5_000).ok_or_else(|| anyhow::anyhow!("missing runner series"))?;
        assert_eq!(runner.writes, vec![None, Some(10.0)]);
        assert_eq!(runner.behind, vec![None, None]);
        assert_eq!(runner.batches, vec![None, None]);
        assert_eq!(runner.wire, vec![None, None]);
        assert!(store(&st, "server")?.rates.batches.is_none());
        assert!(store(&st, "server")?.rates.wire.is_none());
        let log = b.log(0, "READS", 100);
        assert!(log.iter().any(|l| l.text.contains("2 reads didn't get an answer")));
        assert!(log.iter().all(|l| l.kind == "note" && l.moved.is_none()));
        assert_eq!(b.log_len("READS").1, log.len());
        assert!(b.log(5_000, "", 100).is_empty());
        Ok(())
    }
}
