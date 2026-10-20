//! The server's board of every conversation store (`docs/conversations.md`, "Telemetry"): its own,
//! each runner's, and each mirror's copy of its own; where each feed stands and how far behind it
//! is; each store's series for its dashboard (an hour at 5 s, a day at a minute, in memory, like
//! the machines' samples); and what happened to them lately, noted as it samples.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;

use super::feed::peer_name;
use super::mirror::MirrorView;
use super::telemetry::{PeerView, Report, ShipView, StoreReport, Upkeeping, wait_quantile};

/// How often the server samples the board.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(5);
/// Each store's series: fine, an hour at 5 s; coarse, a day at a minute.
const FINE: (i64, usize) = (5_000, 720);
const COARSE: (i64, usize) = (60_000, 1440);
const DAY_MS: i64 = COARSE.0 * COARSE.1 as i64;
/// What happened lately, at most.
const NOTES: usize = 200;
/// A runner's report older than this isn't current (it reports every few seconds).
const STALE_MS: i64 = 30_000;
/// A store whose feed said anything this recently is alive (one a runner too old to report).
const ALIVE_MS: i64 = 10 * 60_000;
/// Rows behind that are worth a note.
const LAG_ROWS: u64 = 5_000;
/// The server's store's key, and the beginning of a mirror's copy's.
pub const SERVER: &str = "server";
const MIRROR: &str = "mirror:";

/// A machine that runs kiln's runner, as the server knows it.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub name: String,
    pub connected: bool,
    pub version: String,
}

/// A store whose feed the server's store has taken, as it keeps it (`peers`): its name
/// ([`peer_name`]), through where it has it, and when it last took from it (unix ms).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerRow {
    pub name: String,
    pub rev: u64,
    pub at: i64,
}

/// What the server sees now, for the board.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub now: i64,
    /// Its own store (None: it keeps none).
    pub server: Option<StoreReport>,
    pub version: String,
    /// Where each feed into its store stands, since kiln started.
    pub intake: Vec<PeerView>,
    pub mirrors: Vec<(MirrorView, ShipView)>,
    pub mirrors_wanted: usize,
    pub machines: Vec<Machine>,
    pub peers: Vec<PeerRow>,
    /// Where restoring its store from a mirror's copy stands (`Registry::restore_view`).
    pub restore: serde_json::Value,
}

/// One store, as the board shows it.
#[derive(Debug, Clone, Serialize)]
pub struct StoreStatus {
    /// Its key, for its series: `server`, `<machine>/<store>` (a runner's), `mirror:<machine>`, or
    /// `<machine>/` (a machine that keeps none).
    pub key: String,
    /// `server`, `runner` or `mirror`.
    pub role: &'static str,
    pub machine: String,
    /// The store's id (None: a machine that keeps none).
    pub id: Option<String>,
    /// The kiln it runs.
    pub version: Option<String>,
    pub connected: bool,
    /// `live` (the server's), `in_sync`, `catching_up`, `behind` (and nothing moving), `offline`,
    /// `feeding` (a runner too old to say how far behind it is), `no_store`.
    pub state: &'static str,
    /// Unix ms of what the board last heard of it.
    pub heard: Option<i64>,
    /// What it has done since its kiln started, and its file (None: it hasn't said: a runner too
    /// old, or not heard from since the server started).
    pub report: Option<StoreReport>,
    /// A runner's store's feed to the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed: Option<FeedStatus>,
    /// The server's feed to a mirror's copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<MirrorStatus>,
    /// Over the last minute, per second.
    pub rates: Rates,
    /// Writes per second over the last ten minutes, a point every 5 s (None: not known then).
    pub spark: Vec<Option<f64>>,
}

/// A runner's store's feed to the server's.
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

/// The server's feed to a mirror's copy.
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
    /// Feed batches: taken (the server's, a runner's), or sent (a mirror's).
    pub batches: Option<f64>,
    /// Their bytes on the wire.
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
    top: u64,
    commits: u64,
    busy_ms: u64,
    timeouts: u64,
    dropped: u64,
    p50: Option<u64>,
    p99: Option<u64>,
    behind: Option<u64>,
    batches: u64,
    wire: u64,
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
        if self.points.back().is_some_and(|last| if self.step == FINE.0 { p.t - last.t < self.step / 2 } else { p.t.div_euclid(self.step) == last.t.div_euclid(self.step) }) {
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
                let dt = prev.map_or(0, |q| p.t - q.t);
                let have = |x: u64| p.has.then_some(x);
                v.t.push(p.t / 1000);
                v.bytes.push(have(p.bytes));
                v.free.push(have(p.free));
                v.wal.push(have(p.wal));
                v.sessions.push(p.sessions.filter(|_| p.has));
                v.writes.push(both.and_then(|q| rate(q.top, p.top, dt)));
                v.commits.push(both.and_then(|q| rate(q.commits, p.commits, dt)));
                v.busy.push(both.and_then(|q| rate(q.busy_ms, p.busy_ms, dt)).map(|ms| (ms / 1000.0).min(1.0)));
                v.p50.push(p.p50.filter(|_| p.has));
                v.p99.push(p.p99.filter(|_| p.has));
                v.timeouts.push(both.and_then(|q| p.timeouts.checked_sub(q.timeouts)));
                v.dropped.push(both.and_then(|q| p.dropped.checked_sub(q.dropped)));
                v.behind.push(p.behind);
                v.batches.push(prev.and_then(|q| rate(q.batches, p.batches, dt)));
                v.wire.push(prev.and_then(|q| rate(q.wire, p.wire, dt)));
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
    state: &'static str,
    behind: Option<u64>,
    /// Behind enough to say so ([`LAG_ROWS`], or nothing moving), until it has caught up: a feed
    /// that keeps up under load is a batch or two behind and back every few seconds, which is
    /// nothing to note.
    lagging: bool,
    gaps: u64,
    errors: u64,
    stalls: u64,
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
    /// The last sample's briefs (None: it hasn't sampled yet), and its restore.
    last: Mutex<Option<(HashMap<String, Brief>, serde_json::Value)>>,
    /// The server's store's peers, as last read.
    peers: Mutex<Vec<PeerRow>>,
}

fn fresh(at: i64, now: i64) -> bool {
    now - at <= STALE_MS
}

impl Board {
    /// The server's store's peers, as just read ([`super::store::Store::peers`]).
    pub fn set_peers(&self, rows: Vec<PeerRow>) {
        *self.peers.lock().unwrap_or_else(PoisonError::into_inner) = rows;
    }

    pub fn peers(&self) -> Vec<PeerRow> {
        self.peers.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// What runner `machine` said of its store `store` (its own, or a mirror's copy), at `now`.
    pub fn heard(&self, machine: &str, store: &str, report: Report, now: i64) {
        self.heard.lock().unwrap_or_else(PoisonError::into_inner).insert(peer_name(machine, store), Heard { machine: machine.to_owned(), at: now, report });
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
        Status { at: seen.now, on: seen.server.is_some(), mirrors_wanted: seen.mirrors_wanted, restore: seen.restore.clone(), stores, notes }
    }

    /// A sample: each store's series takes a point, and what changed since the last sample is
    /// noted. The board as it stands then.
    pub fn sample(&self, seen: &Seen) -> Status {
        let stores = self.stores(seen);
        {
            let mut series = self.series.lock().unwrap_or_else(PoisonError::into_inner);
            // What the server's store took from every feed.
            let taken = seen.intake.iter().fold((0, 0), |(b, w), v| (b + v.batches, w + v.bytes));
            for s in &stores {
                let (mut p, waits) = point(s, seen.now);
                if s.role == "server" {
                    (p.batches, p.wire) = taken;
                }
                let ser = series.entry(s.key.clone()).or_default();
                ser.fine.push(p, &waits);
                ser.coarse.push(p, &waits);
            }
            // A store not on the board for a day goes from its series too.
            series.retain(|_, ser| ser.fine.points.back().is_some_and(|p| seen.now - p.t < DAY_MS));
        }
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let briefs: HashMap<String, Brief> = stores.iter().map(|s| (s.key.clone(), brief(s, last.as_ref().and_then(|(b, _)| b.get(&s.key))))).collect();
        let mut new = Vec::new();
        match last.as_ref() {
            None => new.push(Note { at: seen.now, key: SERVER.into(), level: "info", text: format!("telemetry began: {} stores", stores.iter().filter(|s| s.id.is_some()).count()) }),
            Some((before, restore)) => {
                for s in &stores {
                    if let Some(now_b) = briefs.get(&s.key) {
                        notes_of(s, before.get(&s.key), now_b, seen.now, &mut new);
                    }
                }
                for (key, b) in before {
                    if !briefs.contains_key(key) && key.starts_with(MIRROR) && b.state != "no_store" {
                        new.push(Note { at: seen.now, key: key.clone(), level: "info", text: format!("{} no longer keeps a copy of the server's store", key.strip_prefix(MIRROR).unwrap_or(key)) });
                    }
                }
                if *restore != seen.restore {
                    new.push(restore_note(&seen.restore, seen.now));
                }
            }
        }
        *last = Some((briefs, seen.restore.clone()));
        drop(last);
        if !new.is_empty() {
            let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
            notes.extend(new);
            while notes.len() > NOTES {
                notes.pop_front();
            }
        }
        self.status(seen)
    }

    /// Store `key`'s series over the last `window_s` seconds (an hour or less: at 5 s; more: at a
    /// minute, up to a day).
    pub fn series(&self, key: &str, window_s: i64, now: i64) -> Option<SeriesView> {
        let series = self.series.lock().unwrap_or_else(PoisonError::into_inner);
        let ser = series.get(key)?;
        let tier = if window_s <= FINE.0 / 1000 * FINE.1 as i64 { &ser.fine } else { &ser.coarse };
        Some(SeriesView::of(key, tier, now - window_s.max(1) * 1000))
    }

    /// The stores, from what the server sees and what runners said.
    fn stores(&self, seen: &Seen) -> Vec<StoreStatus> {
        let now = seen.now;
        let heard = self.heard.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = Vec::new();
        let server_top = seen.server.as_ref().map(|r| r.file.top);
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
        // The mirrors' copies: what they said, by store, so a copy restored from isn't listed
        // as a runner's store.
        let copies: HashSet<&str> = heard.values().filter(|h| h.report.role == "mirror").map(|h| h.report.store.id.as_str()).collect();
        // Runners' stores: what the server's store keeps of them, what fed it since it started,
        // and what runners said.
        let mut names: Vec<String> = seen.peers.iter().map(|p| p.name.clone()).collect();
        names.extend(seen.intake.iter().filter(|v| !v.copy).map(|v| peer_name(&v.machine, &v.store)));
        names.extend(heard.iter().filter(|(_, h)| h.report.role == "own").map(|(k, _)| k.clone()));
        names.sort();
        names.dedup();
        names.retain(|name| name.split_once('/').is_some_and(|(_, store)| !copies.contains(store)) && !seen.intake.iter().any(|v| v.copy && peer_name(&v.machine, &v.store) == *name));
        // A runner process upgraded side by side has a store of its own, so a machine's past
        // stores stay in `peers`: each machine shows the stores alive now, else its latest.
        let last_heard = |name: &str| {
            let said = heard.get(name).filter(|h| h.report.role == "own").map_or(0, |h| h.at);
            let fed = seen.intake.iter().filter(|v| peer_name(&v.machine, &v.store) == name).map(|v| v.last_batch.max(v.hello_at)).max().unwrap_or(0);
            let row = seen.peers.iter().filter(|p| p.name == name).map(|p| p.at).max().unwrap_or(0);
            (said.max(fed).max(row), fresh(said, now) || now - fed < ALIVE_MS)
        };
        let mut by_machine: HashMap<&str, Vec<(String, i64, bool)>> = HashMap::new();
        for name in &names {
            if let Some((m, _)) = name.split_once('/') {
                let (last, alive) = last_heard(name);
                by_machine.entry(m).or_default().push((name.clone(), last, alive));
            }
        }
        let mut shown: Vec<String> = by_machine
            .into_values()
            .flat_map(|mut v| {
                if v.iter().any(|x| x.2) {
                    v.retain(|x| x.2);
                } else {
                    v.sort_by_key(|x| std::cmp::Reverse(x.1));
                    v.truncate(1);
                }
                v.into_iter().map(|x| x.0)
            })
            .collect();
        shown.sort();
        for name in shown {
            let Some((m, store)) = name.split_once('/') else { continue };
            let intake = seen.intake.iter().find(|v| v.machine == m && v.store == store).cloned();
            let row = seen.peers.iter().find(|p| p.name == name);
            let said = heard.get(&name).filter(|h| h.report.role == "own");
            let current = said.filter(|h| fresh(h.at, now));
            let mach = machine(m);
            // Connected: its report is current; a runner too old to report, its machine is.
            let reports_any = heard.values().any(|h| h.machine == m);
            let connected = current.is_some() || (!reports_any && mach.is_some_and(|x| x.connected) && intake.is_some());
            let rev = intake.as_ref().map(|v| v.rev).or(row.map(|r| r.rev)).unwrap_or(0);
            let top = said.map(|h| h.report.store.file.top);
            let behind = top.map(|t| t.saturating_sub(rev));
            let moving = intake.as_ref().is_some_and(|v| now - v.last_batch < 15_000);
            let state = match (connected, behind) {
                (false, _) => "offline",
                (true, Some(0)) => "in_sync",
                (true, Some(_)) if moving => "catching_up",
                (true, Some(_)) => "behind",
                (true, None) => "feeding",
            };
            out.push(StoreStatus {
                key: name.clone(),
                role: "runner",
                machine: m.to_owned(),
                id: Some(store.to_owned()),
                version: said.map(|h| h.report.version.clone()).or_else(|| mach.map(|x| x.version.clone())),
                connected,
                state,
                heard: said.map(|h| h.at).or(intake.as_ref().map(|v| v.last_batch.max(v.hello_at)).filter(|t| *t > 0)).or(row.map(|r| r.at)),
                report: said.map(|h| h.report.store.clone()),
                feed: Some(FeedStatus { rev, top, behind, at: intake.as_ref().map(|v| v.last_batch).filter(|t| *t > 0).or(row.map(|r| r.at)).unwrap_or(0), intake, ship: said.and_then(|h| h.report.feed.clone()) }),
                mirror: None,
                rates: Rates::default(),
                spark: Vec::new(),
            });
        }
        // Machines that keep no store: none of theirs has fed the server's.
        for m in &seen.machines {
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
        // The mirrors' copies.
        for (v, ship) in &seen.mirrors {
            let said = heard.values().filter(|h| h.machine == v.machine && h.report.role == "mirror").max_by_key(|h| h.at);
            let behind = match (server_top, ship.acked) {
                (Some(top), Some(acked)) if v.connected => Some(top.saturating_sub(acked)),
                _ => None,
            };
            let moving = ship.last_sent.is_some_and(|t| now - t < 15_000);
            let state = match (v.connected, behind) {
                (false, _) => "offline",
                (true, Some(0)) => "in_sync",
                (true, Some(_)) if moving => "catching_up",
                (true, Some(_)) => "behind",
                (true, None) => "feeding",
            };
            out.push(StoreStatus {
                key: format!("{MIRROR}{}", v.machine),
                role: "mirror",
                machine: v.machine.clone(),
                id: said.map(|h| h.report.store.id.clone()),
                version: said.map(|h| h.report.version.clone()).or_else(|| machine(&v.machine).map(|x| x.version.clone())),
                connected: v.connected,
                state,
                heard: said.map(|h| h.at).or(ship.last_ack),
                report: said.map(|h| h.report.store.clone()),
                feed: None,
                mirror: Some(MirrorStatus { chosen_at: v.chosen_at, last_seen: v.last_seen, ship: ship.clone(), behind }),
                rates: Rates::default(),
                spark: Vec::new(),
            });
        }
        out
    }
}

/// A store's point now, and its waits counted so far.
fn point(s: &StoreStatus, now: i64) -> (Point, Vec<u64>) {
    let mut p = Point { t: now, up: s.connected, ..Point::default() };
    let mut waits = Vec::new();
    if let Some(r) = &s.report {
        p.has = true;
        (p.bytes, p.free, p.wal, p.sessions, p.top) = (r.file.bytes, r.file.free, r.file.wal, r.file.sessions, r.file.top);
        (p.commits, p.busy_ms, p.timeouts, p.dropped) = (r.commits, r.busy_ms, r.timeouts, r.dropped);
        waits.clone_from(&r.waits);
    }
    if let Some(f) = &s.feed {
        p.behind = f.behind;
        if let Some(i) = &f.intake {
            (p.batches, p.wire) = (i.batches, i.bytes);
        }
    }
    if let Some(m) = &s.mirror {
        (p.behind, p.batches, p.wire) = (m.behind, m.ship.batches, m.ship.bytes);
    }
    (p, waits)
}

/// Rates over the last minute, and writes per second over the last ten minutes.
fn rates_and_spark(t: &Tier, now: i64) -> (Rates, Vec<Option<f64>>) {
    let recent: Vec<&Point> = t.points.iter().filter(|p| now - p.t <= 10 * 60_000).collect();
    let spark = recent
        .windows(2)
        .map(|w| match w {
            [a, b] if a.has && b.has => rate(a.top, b.top, b.t - a.t),
            _ => None,
        })
        .collect();
    let mut r = Rates::default();
    if let (Some(last), Some(first)) = (t.points.back(), t.points.iter().find(|p| now - p.t <= 65_000)) {
        let dt = last.t - first.t;
        if dt > 0 {
            if first.has && last.has {
                r.writes = rate(first.top, last.top, dt);
                r.busy = rate(first.busy_ms, last.busy_ms, dt).map(|ms| (ms / 1000.0).min(1.0));
            }
            r.batches = rate(first.batches, last.batches, dt);
            r.wire = rate(first.wire, last.wire, dt);
        }
    }
    (r, spark)
}

/// What to note of store `s`, as it was at the last sample (`was`; None: it wasn't on the board).
fn brief(s: &StoreStatus, was: Option<&Brief>) -> Brief {
    let file = s.report.as_ref().map(|r| &r.file);
    let intake = s.feed.as_ref().and_then(|f| f.intake.as_ref());
    let ship = s.feed.as_ref().and_then(|f| f.ship.as_ref()).or(s.mirror.as_ref().map(|m| &m.ship));
    let behind = s.feed.as_ref().and_then(|f| f.behind).or(s.mirror.as_ref().and_then(|m| m.behind));
    let lagging = s.connected && if was.is_some_and(|b| b.lagging) { behind != Some(0) } else { behind.is_some_and(|n| n >= LAG_ROWS) || s.state == "behind" };
    Brief {
        connected: s.connected,
        state: s.state,
        behind,
        lagging,
        gaps: intake.map_or(0, |i| i.gaps),
        errors: intake.map_or(0, |i| i.errors),
        stalls: ship.map_or(0, |v| v.stalls),
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
        "mirror" => format!("{}'s copy", s.machine),
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
        match s.role {
            "mirror" => note("info", format!("{} keeps a copy of the server's store", s.machine)),
            "runner" if s.id.is_some() => note("info", format!("{name} reached the server")),
            _ => {}
        }
        return;
    };
    if was.connected != now_b.connected && s.id.is_some() {
        if now_b.connected { note("info", format!("{} is back", s.machine)) } else { note("warn", format!("{name} is offline")) }
    }
    if now_b.connected && was.connected {
        let behind = now_b.behind.unwrap_or(0);
        match (was.lagging, now_b.lagging) {
            (true, false) => note("info", format!("{name} caught up")),
            (false, true) if now_b.state == "behind" => note("warn", format!("{name} is {behind} rows behind and nothing is moving")),
            (false, true) => note("info", format!("{name} is {behind} rows behind")),
            (true, true) if now_b.state == "behind" && was.state != "behind" => note("warn", format!("{name} is {behind} rows behind and nothing is moving")),
            _ => {}
        }
    }
    if now_b.gaps > was.gaps {
        note("info", format!("a batch of {name} didn't follow what the server had; it starts again from there"));
    }
    if now_b.errors > was.errors {
        note("warn", format!("{} batch{} of {name} couldn't be taken; it sends again", now_b.errors - was.errors, if now_b.errors - was.errors == 1 { "" } else { "es" }));
    }
    if now_b.stalls > was.stalls {
        note("warn", format!("the feed of {name} went quiet for 30 s and asked again where it stands"));
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

fn restore_note(restore: &serde_json::Value, at: i64) -> Note {
    let s = |k: &str| restore.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    let (level, text) = match s("state").as_str() {
        "asked" => ("info", format!("asked {} for its copy, to restore the server's store from", s("from"))),
        "copying" => ("info", format!("restoring the server's store from {}'s copy", s("from"))),
        "done" => ("info", format!("the server's store was restored from {}'s copy: {} sessions came back", s("from"), restore.get("sessions").and_then(serde_json::Value::as_u64).unwrap_or(0))),
        "failed" => ("error", format!("restoring the server's store from {}'s copy failed: {}", s("from"), s("error"))),
        _ => ("info", "no restore under way".to_owned()),
    };
    Note { at, key: SERVER.into(), level, text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convo::telemetry::FileView;

    fn report(id: &str, top: u64) -> StoreReport {
        StoreReport { id: id.into(), file: FileView { at: 1, bytes: 10 << 20, free: 1 << 20, top, ..FileView::default() }, waits: vec![0; 16], ..StoreReport::default() }
    }

    fn seen(now: i64) -> Seen {
        Seen {
            now,
            server: Some(report("srv", 500)),
            version: "test".into(),
            machines: vec![Machine { name: "fedora".into(), connected: true, version: "test".into() }, Machine { name: "jupiter".into(), connected: false, version: "old".into() }],
            peers: vec![PeerRow { name: "fedora/abc".into(), rev: 90, at: 1 }],
            restore: serde_json::json!({ "state": "idle" }),
            ..Seen::default()
        }
    }

    /// Store `key` on the board.
    fn store<'a>(st: &'a Status, key: &str) -> anyhow::Result<&'a StoreStatus> {
        st.stores.iter().find(|s| s.key == key).ok_or_else(|| anyhow::anyhow!("no {key} on the board"))
    }

    #[test]
    fn each_store_shows_how_far_behind_it_is_and_a_machine_without_one_says_so() -> anyhow::Result<()> {
        let b = Board::default();
        b.heard("fedora", "abc", Report { role: "own".into(), version: "test".into(), store: report("abc", 100), feed: None }, 1_000);
        let st = b.status(&seen(2_000));
        let keys: Vec<(&str, &str)> = st.stores.iter().map(|s| (s.key.as_str(), s.state)).collect();
        assert_eq!(keys, vec![("server", "live"), ("fedora/abc", "behind"), ("jupiter/", "offline")]);
        assert_eq!(store(&st, "fedora/abc")?.feed.as_ref().and_then(|f| f.behind), Some(10));
        // Taken through its top: in sync.
        let mut s = seen(3_000);
        s.intake = vec![PeerView { machine: "fedora".into(), store: "abc".into(), rev: 100, last_batch: 2_900, batches: 3, rows: 10, ..PeerView::default() }];
        assert_eq!(store(&b.status(&s), "fedora/abc")?.state, "in_sync");
        // Not heard from for long: offline.
        assert_eq!(store(&b.status(&seen(100_000)), "fedora/abc")?.state, "offline");
        Ok(())
    }

    #[test]
    fn a_machine_shows_its_stores_alive_now_else_only_its_latest() {
        let b = Board::default();
        let mut s = seen(1_000_000);
        // Two past instances' stores of fedora, neither heard from: its latest stands for it.
        s.peers = vec![PeerRow { name: "fedora/old".into(), rev: 5, at: 100 }, PeerRow { name: "fedora/newer".into(), rev: 7, at: 200 }];
        let keys = |st: &Status| st.stores.iter().filter(|x| x.machine == "fedora").map(|x| x.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&b.status(&s)), vec!["fedora/newer"]);
        // One reporting now: it alone, however recent the others in `peers`.
        b.heard("fedora", "live", Report { role: "own".into(), version: "test".into(), store: report("live", 1), feed: None }, 999_000);
        s.peers.push(PeerRow { name: "fedora/live".into(), rev: 1, at: 50 });
        assert_eq!(keys(&b.status(&s)), vec!["fedora/live"]);
    }

    #[test]
    fn samples_make_series_with_rates_and_note_what_changed() -> anyhow::Result<()> {
        let b = Board::default();
        let mut s = seen(0);
        // fedora's store 9,910 rows ahead of what the server has of it (`peers`: 90).
        b.heard("fedora", "abc", Report { role: "own".into(), version: "test".into(), store: report("abc", 10_000), feed: None }, 0);
        b.sample(&s);
        s.now = 5_000;
        let mut r = report("srv", 1_500);
        r.busy_ms = 500;
        s.server = Some(r);
        b.heard("fedora", "abc", Report { role: "own".into(), version: "test".into(), store: report("abc", 10_000), feed: None }, 5_000);
        s.intake = vec![PeerView { machine: "fedora".into(), store: "abc".into(), rev: 10_000, last_batch: 4_000, batches: 2, bytes: 4_000, ..PeerView::default() }];
        let st = b.sample(&s);
        let v = b.series("server", 3600, 5_000).ok_or_else(|| anyhow::anyhow!("no series for the server"))?;
        assert_eq!(v.t, vec![0, 5]);
        assert_eq!(v.writes, vec![None, Some(200.0)]);
        assert_eq!(v.busy, vec![None, Some(0.1)]);
        let f = b.series("fedora/abc", 3600, 5_000).ok_or_else(|| anyhow::anyhow!("no series for fedora"))?;
        assert_eq!(f.behind, vec![Some(9_910), Some(0)]);
        assert_eq!(f.batches, vec![None, Some(0.4)]);
        let texts: Vec<&str> = st.notes.iter().map(|n| n.text.as_str()).collect();
        assert!(texts.contains(&"fedora's store caught up"), "{texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("telemetry began")), "{texts:?}");
        assert_eq!(store(&st, "server")?.rates.writes, Some(200.0));
        // A few rows behind and back, as a feed that keeps up under load is: nothing to note.
        let before = st.notes.len();
        for (i, rev) in [(2, 9_990), (3, 10_000), (4, 9_995), (5, 10_000)] {
            s.now = i * 5_000;
            b.heard("fedora", "abc", Report { role: "own".into(), version: "test".into(), store: report("abc", 10_000), feed: None }, s.now);
            s.intake = vec![PeerView { machine: "fedora".into(), store: "abc".into(), rev, last_batch: s.now - 100, batches: 2, bytes: 4_000, ..PeerView::default() }];
            b.sample(&s);
        }
        assert_eq!(b.status(&s).notes.len(), before);
        Ok(())
    }
}
