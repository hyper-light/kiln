//! A round: a fresh kiln of the build under test, filled as production's is (agents' long
//! conversations on several runners), then the window it's measured in, while people view agents
//! over the UI's sockets, agents work and stream, agents call the API and MCP, and pages load,
//! with CPU hogs beside it when asked.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::{Frame, Http, PATIENCE, Socket};
use crate::fleet::Fleet;
use crate::measure::{self, Class, Sample, Scrape};
use crate::standin::{Lcg, STAMP, now_us};

/// What a round does. The defaults are production's shape at a busy hour (`docs/runtime-lab.md`
/// says where each comes from).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    /// Runners, each its own `kiln agent`.
    pub runners: usize,
    /// Each live session's conversation before the window, in events.
    pub sessions: Vec<u64>,
    /// Sessions that work through the window: a message, then a turn of `turn_events` events
    /// `turn_every_ms` apart, then `turn_think_ms` before the next.
    pub active: usize,
    pub turn_events: u64,
    pub turn_every_ms: u64,
    pub turn_think_ms: u64,
    /// People viewing agents: each opens one, watches it `dwell_s`, then opens another.
    pub viewers: usize,
    pub dwell_s: u64,
    /// The UI's and agents' API reads, `read_think_ms` apart for each reader.
    pub readers: usize,
    pub read_think_ms: u64,
    /// Agents' MCP calls, `mcp_think_ms` apart for each.
    pub mcp: usize,
    pub mcp_think_ms: u64,
    /// Pages and their files loading, `page_think_ms` apart.
    pub pages: usize,
    pub page_think_ms: u64,
    /// Seconds of load before the window, and of the window.
    pub warmup_s: u64,
    pub window_s: u64,
}

impl Profile {
    /// Production's shape (its `/metrics`, 2026-11-03: ten runners, 134 live feeds, 4 to 8 people
    /// watching, conversations to about 20,000 events), at a busy hour's pace.
    pub fn realistic() -> Self {
        let mut sessions = vec![20_000, 20_000];
        sessions.extend(std::iter::repeat_n(4_000, 6));
        sessions.extend(std::iter::repeat_n(400, 24));
        Self {
            runners: 4,
            sessions,
            active: 8,
            turn_events: 40,
            turn_every_ms: 25,
            turn_think_ms: 2_000,
            viewers: 8,
            dwell_s: 20,
            readers: 4,
            read_think_ms: 50,
            mcp: 2,
            mcp_think_ms: 250,
            pages: 1,
            page_think_ms: 100,
            warmup_s: 5,
            window_s: 60,
        }
    }

    /// The realistic profile with no think time for its people and readers: each viewer opens the next
    /// conversation as soon as the last one is open, each reader asks again as soon as it is answered, a closed
    /// loop at the most its clients can ask. A server of one shard falls behind it on purpose (goal 6, plan
    /// 01a1a59c2abe step 4: what it sheds, by class, and what it still serves).
    pub fn overload() -> Self {
        Self { dwell_s: 0, read_think_ms: 0, ..Self::realistic() }
    }

    /// The realistic profile at production's whole shape rather than its busy hour's share: its ten
    /// runners and all 134 of its live feeds (2 of 20,000 events, 6 of 4,000, the rest of 400), every
    /// one at work, at the same pace. What many sessions changing at once cost a server that kept them
    /// under one lock, and don't cost one that gives each its own shard (goal 4, plan 01a1a59c2a9c
    /// step 5).
    pub fn many() -> Self {
        let mut sessions = vec![20_000, 20_000];
        sessions.extend(std::iter::repeat_n(4_000, 6));
        sessions.extend(std::iter::repeat_n(400, 134 - 8));
        Self { runners: 10, active: sessions.len(), sessions, ..Self::realistic() }
    }

    /// A small one, to try the lab out (and for its own test).
    pub fn small() -> Self {
        Self {
            runners: 1,
            sessions: vec![300, 100, 100],
            active: 2,
            turn_events: 10,
            turn_every_ms: 50,
            turn_think_ms: 1_000,
            viewers: 2,
            dwell_s: 5,
            readers: 1,
            read_think_ms: 100,
            mcp: 1,
            mcp_think_ms: 500,
            pages: 1,
            page_think_ms: 500,
            warmup_s: 2,
            window_s: 10,
        }
    }
}

/// A build under test: a label, its `kiln` binary, and what its server's environment has besides.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Build {
    pub label: String,
    pub kiln: PathBuf,
    pub env: Vec<(String, String)>,
}

/// The machine around a round.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Machine {
    pub host: String,
    pub os: String,
    /// CPUs the lab may use (its affinity, its cgroup's quota): what hogs are counted against.
    pub cpus: usize,
    /// The CPUs it may run on (Linux: its affinity, `Cpus_allowed_list`).
    pub affinity: Option<String>,
    /// The load averages and CPU pressure (Linux, `some avg10`) before and after the window.
    pub load_before: Option<[f64; 3]>,
    pub load_after: Option<[f64; 3]>,
    pub cpu_pressure_before: Option<f64>,
    pub cpu_pressure_after: Option<f64>,
    /// The lab's CPUs' mean clock (Linux, `/proc/cpuinfo`) before and after the window: a machine
    /// whose clocks moved (another's work on it, its power limits) runs everything at another pace.
    #[serde(default)]
    pub mhz_before: Option<f64>,
    #[serde(default)]
    pub mhz_after: Option<f64>,
    /// The server's CPUs, where they're its own (`--server-cpus`), and their mean clock before and
    /// after the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_cpus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_mhz_before: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_mhz_after: Option<f64>,
}

fn loadavg() -> Option<[f64; 3]> {
    let text = if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/loadavg").ok()?
    } else {
        let out = Command::new("sysctl").args(["-n", "vm.loadavg"]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).replace(['{', '}'], "")
    };
    let mut it = text.split_whitespace().filter_map(|w| w.parse::<f64>().ok());
    Some([it.next()?, it.next()?, it.next()?])
}

/// The CPUs `list` names (`0-3,8,10-11`, as Linux writes an affinity).
pub fn cpus_in(list: &str) -> Vec<usize> {
    list.split(',')
        .filter_map(|part| {
            let part = part.trim();
            let (a, b) = part.split_once('-').unwrap_or((part, part));
            Some(a.parse::<usize>().ok()?..=b.parse::<usize>().ok()?)
        })
        .flatten()
        .collect()
}

/// The mean clock of the CPUs the lab may run on, in MHz (Linux).
fn cpu_mhz(affinity: Option<&str>) -> Option<f64> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let mine = affinity.map(cpus_in);
    let (mut cpu, mut sum, mut n) = (None::<usize>, 0.0, 0u32);
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        match k.trim() {
            "processor" => cpu = v.trim().parse().ok(),
            "cpu MHz" => {
                if let (Some(c), Ok(mhz)) = (cpu, v.trim().parse::<f64>())
                    && mine.as_ref().is_none_or(|m| m.contains(&c))
                {
                    sum += mhz;
                    n += 1;
                }
            }
            _ => {}
        }
    }
    (n > 0).then(|| sum / f64::from(n))
}

fn cpu_pressure() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/pressure/cpu").ok()?;
    text.lines().find(|l| l.starts_with("some"))?.split_whitespace().find_map(|w| w.strip_prefix("avg10="))?.parse().ok()
}

pub fn machine() -> Machine {
    let host = Command::new("hostname").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
    let affinity = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:").map(|v| v.trim().to_owned())));
    Machine { host, os: std::env::consts::OS.to_owned(), cpus: std::thread::available_parallelism().map_or(1, std::num::NonZero::get), affinity, ..Machine::default() }
}

/// Where a round's server and its hogs run (goal 10, plan 01a1a87b2dae step 1). On CPUs of their own
/// (`--server-cpus`, Linux's `taskset`), the lab's clients, runners and stand-ins keep the CPUs the lab
/// was started on to themselves, so the lab times the server, not itself. In production's layout
/// (`--layout production`, Linux with systemd's user manager) the server runs in a scope of its own at
/// `CPUWeight=1000`, as kiln.slice gives it on bare metal, and the hogs in one at `CPUWeight=1`, as CI
/// legs and agents run (docs/priority.md); `shared`, the hogs run beside everything in the lab's own
/// cgroup, as before.
#[derive(Clone, Debug, Default)]
pub struct Place {
    pub cpus: Option<String>,
    pub layout: String,
}

/// The weights production's layout gives the server and the hogs (`systemd.resource-control`'s
/// `CPUWeight`): kiln.slice's on bare metal, and CI legs' and agents' (docs/priority.md).
const SERVER_WEIGHT: u32 = 1000;
const HOG_WEIGHT: u32 = 1;

impl Place {
    pub fn of(cpus: Option<&str>, layout: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(matches!(layout, "shared" | "production"), "no layout {layout}: shared or production");
        anyhow::ensure!(layout == "shared" || cfg!(target_os = "linux"), "--layout production needs Linux with systemd's user manager");
        if let Some(c) = cpus {
            anyhow::ensure!(cfg!(target_os = "linux"), "--server-cpus needs Linux (taskset)");
            anyhow::ensure!(!cpus_in(c).is_empty(), "--server-cpus takes a CPU list (16-31, or 0-3,8), not {c:?}");
        }
        Ok(Self { cpus: cpus.map(str::to_owned), layout: layout.to_owned() })
    }

    fn production(&self) -> bool {
        self.layout == "production"
    }

    /// `program`, placed: in production's layout in a scope `unit` of its own at `weight` (systemd-run
    /// execs it in place, so its pid and process group stay the lab's to stop), on the server's CPUs
    /// when it has its own.
    pub fn command(&self, program: &Path, weight: u32, unit: &str) -> Command {
        let mut wrap: Vec<OsString> = Vec::new();
        if self.production() {
            wrap.extend(["systemd-run", "--user", "--scope", "--quiet", "--collect"].map(OsString::from));
            wrap.push(format!("--unit={unit}").into());
            wrap.push(format!("--property=CPUWeight={weight}").into());
            wrap.push("--".into());
        }
        if let Some(c) = &self.cpus {
            wrap.extend(["taskset", "-c"].map(OsString::from));
            wrap.push(c.into());
        }
        match wrap.split_first() {
            Some((first, rest)) => {
                let mut cmd = Command::new(first);
                cmd.args(rest).arg(program);
                cmd
            }
            None => Command::new(program),
        }
    }

    /// The server's command (`program`), placed.
    pub fn server(&self, program: &Path, unit: &str) -> Command {
        self.command(program, SERVER_WEIGHT, unit)
    }
}

/// A round's result: each number it reports, and what it saw by class.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Round {
    pub build: String,
    pub index: usize,
    pub hogs: usize,
    pub machine: Machine,
    /// Seconds the fill took (every session's conversation in), not measured beyond that.
    pub fill_s: f64,
    pub shards: usize,
    pub numbers: BTreeMap<String, f64>,
    pub seen: BTreeMap<String, measure::Seen>,
    /// What held the shards past 100 µs in the window, and over the fill (`kiln_shard_held_*`, goal 9, plan
    /// 01a1a87b2cec step 2), each the most first: a build without the series has none.
    #[serde(default)]
    pub held: Vec<measure::Held>,
    #[serde(default)]
    pub fill_held: Vec<measure::Held>,
    /// A build that counts its locks (`scripts/locks.py census`, goal 5 of the API runtime program,
    /// plan 01a1a59c2aae step 1): each place a lock is taken, by its site and kind, with what the
    /// window and the fill took there. Kept apart from `numbers`, which an A/B compares one by one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub locks: BTreeMap<String, LockSeen>,
}

/// One place a lock is taken, as a build that counts its locks says on `/metrics` (`kiln_lock_*`):
/// what the window, and the fill, took there.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LockSeen {
    /// `mutex`, `read`, `write` (parking_lot's), `std_mutex`, `std_read`, `std_write`.
    pub kind: String,
    /// Acquisitions in the window, those that found it held, and the seconds those waited.
    pub taken: f64,
    pub contended: f64,
    pub wait_s: f64,
    /// The same over the fill.
    pub fill_taken: f64,
    pub fill_contended: f64,
    pub fill_wait_s: f64,
    /// The longest any one acquisition there waited, by the window's end.
    pub max_wait_s: f64,
    /// The first addresses of the locks taken there, in hex: sites that take one lock share its.
    pub addrs: String,
}

/// The lock census of a build that counts its locks, between the fill's scrapes and the window's.
fn lock_census(fill: (&Scrape, &Scrape), window: (&Scrape, &Scrape)) -> BTreeMap<String, LockSeen> {
    let mut out: BTreeMap<String, LockSeen> = BTreeMap::new();
    let label = |l: &[(String, String)], k: &str| l.iter().find(|(lk, _)| lk == k).map(|(_, v)| v.clone()).unwrap_or_default();
    for (name, labels, value) in &window.1.series {
        let (site, kind) = (label(labels, "site"), label(labels, "kind"));
        if !name.starts_with("kiln_lock_") || site.is_empty() {
            continue;
        }
        let want = [("site", site.as_str()), ("kind", kind.as_str())];
        let since = |s: &Scrape| s.get(name, &want).unwrap_or(0.0);
        let e = out.entry(format!("{site} {kind}")).or_insert_with(|| LockSeen { kind: kind.clone(), ..LockSeen::default() });
        match name.as_str() {
            "kiln_lock_taken_total" => {
                e.taken = value - since(window.0);
                e.fill_taken = since(fill.1) - since(fill.0);
            }
            "kiln_lock_contended_total" => {
                e.contended = value - since(window.0);
                e.fill_contended = since(fill.1) - since(fill.0);
            }
            "kiln_lock_wait_seconds_total" => {
                e.wait_s = value - since(window.0);
                e.fill_wait_s = since(fill.1) - since(fill.0);
            }
            "kiln_lock_wait_max_seconds" => e.max_wait_s = *value,
            "kiln_lock_addresses" => e.addrs = label(labels, "addrs"),
            _ => {}
        }
    }
    out
}

// ------------------------------------------------------------------------------------- the load

/// What every worker shares: where the server is, the sessions, and when to stop.
struct Shared {
    port: u16,
    admin: String,
    sessions: Vec<(String, u64)>,
    active: Vec<String>,
    stop: AtomicBool,
    /// Ids for the messages the lab sends.
    next: AtomicU64,
    /// How long a viewer watches one agent.
    dwell: Duration,
}

fn jitter(r: &mut Lcg, d: Duration) -> Duration {
    // ±25%, so workers don't fall into step.
    let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    Duration::from_millis(ms.saturating_mul(75) / 100 + r.below(ms / 2 + 1))
}

/// Sleeps `d`, waking early when the round stops: whether it's still on.
fn nap(s: &Shared, d: Duration) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if s.stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep((until - Instant::now()).min(Duration::from_millis(100)));
    }
    !s.stop.load(Ordering::Relaxed)
}

/// How long a worker waits before its next request: its think time, or longer where its last answer said to come
/// back later (`Retry-After`): a shed request's client waits that out before asking again, as kiln's own do
/// (`crate::http::client` in kiln; goal 6, plan 01a1a59c2abe step 4), rather than asking again at once.
fn pause(think: Duration, retry_after: Option<Duration>) -> Duration {
    retry_after.map_or(think, |r| r.max(think))
}

fn timed(class: Class, what: &'static str, f: impl FnOnce() -> anyhow::Result<u16>) -> Sample {
    let at_us = now_us();
    let t = Instant::now();
    let ok = matches!(f(), Ok(status) if status < 400);
    Sample { class, at_us, took_us: u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX), ok, what }
}

/// One viewer's watch of one session: when its attach began and when its snapshot came (µs since the
/// epoch), and each frame it read then, by its stamp, with how long that took to reach it. Set against
/// the other viewers' attaches, they say whether a frame published while one viewer attaches reaches
/// the others without waiting for it (goal 10, plan 01a1a87b2dae step 3).
pub struct Watch {
    pub viewer: usize,
    pub session: String,
    pub from_us: u64,
    pub to_us: u64,
    pub frames: Vec<(u64, u64)>,
}

/// The stamps (`@lab:<µs>`) in a frame.
fn stamps(text: &[u8]) -> impl Iterator<Item = u64> + '_ {
    let marker = STAMP.as_bytes();
    let mut at = 0;
    std::iter::from_fn(move || {
        while let Some(i) = text.get(at..).and_then(|rest| rest.windows(marker.len()).position(|w| w == marker)) {
            let start = at + i + marker.len();
            let digits = text.get(start..).map_or(0, |r| r.iter().take_while(|b| b.is_ascii_digit()).count());
            at = start + digits;
            if let Some(n) = text.get(start..start + digits).and_then(|d| std::str::from_utf8(d).ok()).and_then(|d| d.parse().ok()) {
                return Some(n);
            }
        }
        None
    })
}

/// A person viewing agents: opens one's conversation (its whole, packed, as the UI asks), watches
/// its live frames, then another.
fn viewer(s: &Shared, i: usize) -> (Vec<Sample>, Vec<Watch>) {
    let mut out = Vec::new();
    let mut watches = Vec::new();
    let mut r = Lcg(0x9e37_79b9 ^ (i as u64));
    let dwell = s.dwell;
    while !s.stop.load(Ordering::Relaxed) {
        // Most watch an agent at work; one in four opens any (the long ones among them).
        let pick = if r.below(4) > 0 && !s.active.is_empty() {
            s.active.get(usize::try_from(r.below(s.active.len() as u64)).unwrap_or(0)).cloned()
        } else {
            s.sessions.get(usize::try_from(r.below(s.sessions.len() as u64)).unwrap_or(0)).map(|(id, _)| id.clone())
        };
        let Some(id) = pick else { break };
        let at_us = now_us();
        let t = Instant::now();
        let path = format!("/api/harness/sessions/{id}/ws?z=gzip&patch=state&patch=changes");
        let mut sock = match Socket::open(s.port, &s.admin, &path, Duration::from_millis(200)) {
            Ok(sock) => sock,
            Err(_) => {
                out.push(Sample { class: Class::Attach, at_us, took_us: 0, ok: false, what: "" });
                nap(s, Duration::from_secs(1));
                continue;
            }
        };
        // Opened: its packed snapshot came.
        let opened = loop {
            match sock.next() {
                Ok(Frame::Binary) => break true,
                Ok(Frame::Nothing | Frame::Text(_)) if t.elapsed() < PATIENCE && !s.stop.load(Ordering::Relaxed) => {}
                _ => break false,
            }
        };
        // One the round's end cut short is no sample.
        if s.stop.load(Ordering::Relaxed) {
            break;
        }
        out.push(Sample { class: Class::Attach, at_us, took_us: u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX), ok: opened, what: "" });
        if !opened {
            continue;
        }
        let mut watch = Watch { viewer: i, session: id, from_us: at_us, to_us: now_us(), frames: Vec::new() };
        let until = Instant::now() + jitter(&mut r, dwell);
        let mut seen = HashSet::new();
        while Instant::now() < until && !s.stop.load(Ordering::Relaxed) {
            match sock.next() {
                Ok(Frame::Text(text)) => {
                    let now = now_us();
                    for stamp in stamps(&text) {
                        if seen.insert(stamp) {
                            out.push(Sample { class: Class::Websocket, at_us: stamp, took_us: now.saturating_sub(stamp), ok: true, what: "" });
                            watch.frames.push((stamp, now.saturating_sub(stamp)));
                        }
                    }
                }
                // A catch-up (a whole again), or nothing yet.
                Ok(Frame::Binary | Frame::Nothing) => {}
                Ok(Frame::Closed) | Err(_) => break,
            }
        }
        sock.close();
        watches.push(watch);
    }
    (out, watches)
}

/// An agent at work on `id`: a message, its turn, a pause, again.
fn driver(s: &Shared, id: &str, p: &Profile, i: usize) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut r = Lcg(0x5eed ^ ((i as u64) << 8));
    let mut http = Http::new(s.port, &s.admin);
    let turn = Duration::from_millis(p.turn_events.saturating_mul(p.turn_every_ms));
    let path = format!("/api/harness/sessions/{id}/command");
    // Not all at once.
    if !nap(s, jitter(&mut r, Duration::from_millis(p.turn_think_ms))) {
        return out;
    }
    while !s.stop.load(Ordering::Relaxed) {
        let n = s.next.fetch_add(1, Ordering::Relaxed);
        let body = json!({"type": "send", "id": format!("lab-{n}"), "text": format!("lab:turn:{}:{}", p.turn_events, p.turn_every_ms)}).to_string();
        let mut retry_after = None;
        out.push(timed(Class::ApiWrite, "command", || {
            http.send("POST", &path, Some(body.as_bytes())).map(|a| {
                retry_after = a.retry_after;
                a.status
            })
        }));
        if !nap(s, pause(turn + jitter(&mut r, Duration::from_millis(p.turn_think_ms)), retry_after)) {
            break;
        }
    }
    out
}

/// The UI's reads as people move about (lists, an agent's page, its history, runs), and agents'.
fn reader(s: &Shared, p: &Profile, i: usize) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut r = Lcg(0xa11ce ^ ((i as u64) << 16));
    let mut http = Http::new(s.port, &s.admin);
    while !s.stop.load(Ordering::Relaxed) {
        let id = s.sessions.get(usize::try_from(r.below(s.sessions.len() as u64)).unwrap_or(0)).map_or("", |(id, _)| id.as_str());
        // Each read is told apart by its route, so a run says which of them its tail is.
        let (what, path) = match r.below(20) {
            0..=5 => ("sessions", "/api/harness/sessions".to_owned()),
            6..=9 => ("session", format!("/api/harness/sessions/{id}")),
            10..=12 => ("events", format!("/api/harness/sessions/{id}/events?limit=50")),
            13 | 14 => ("agents", "/api/agents".to_owned()),
            15 => ("me", "/api/me".to_owned()),
            16 => ("overview", "/api/overview".to_owned()),
            17 => ("runs", "/api/runs".to_owned()),
            18 => ("plans", format!("/api/plans?session={id}")),
            _ => ("goals", format!("/api/harness/sessions/{id}/goals")),
        };
        let mut retry_after = None;
        out.push(timed(Class::ApiRead, what, || {
            http.send("GET", &path, None).map(|a| {
                retry_after = a.retry_after;
                a.status
            })
        }));
        if !nap(s, pause(jitter(&mut r, Duration::from_millis(p.read_think_ms)), retry_after)) {
            break;
        }
    }
    out
}

/// An agent's MCP calls: who else is working, the machines.
fn mcp(s: &Shared, p: &Profile, i: usize, tools: &[String]) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut r = Lcg(0x3c9 ^ ((i as u64) << 24));
    let mut http = Http::new(s.port, &s.admin);
    let mut n = 0u64;
    while !s.stop.load(Ordering::Relaxed) {
        n += 1;
        let tool = tools.get(usize::try_from(r.below(tools.len().max(1) as u64)).unwrap_or(0));
        let what = match tool.map(String::as_str) {
            Some("agents_list") => "mcp.agents_list",
            Some("runners") => "mcp.runners",
            _ => "mcp.tools",
        };
        let body = match tool {
            Some(tool) => json!({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": tool, "arguments": {}}}),
            None => json!({"jsonrpc": "2.0", "id": n, "method": "tools/list", "params": {}}),
        }
        .to_string();
        let mut retry_after = None;
        out.push(timed(Class::ApiWrite, what, || {
            http.send("POST", "/api/mcp", Some(body.as_bytes())).map(|a| {
                retry_after = a.retry_after;
                a.status
            })
        }));
        if !nap(s, pause(jitter(&mut r, Duration::from_millis(p.mcp_think_ms)), retry_after)) {
            break;
        }
    }
    out
}

/// A page and the files it names, as a browser loads them.
fn pages(s: &Shared, p: &Profile, i: usize) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut r = Lcg(0xbead ^ ((i as u64) << 32));
    let mut http = Http::browser(s.port, &s.admin);
    let mut files: Vec<String> = Vec::new();
    while !s.stop.load(Ordering::Relaxed) {
        let id = s.sessions.get(usize::try_from(r.below(s.sessions.len() as u64)).unwrap_or(0)).map_or("", |(id, _)| id.as_str());
        let mut body = Vec::new();
        out.push(timed(Class::Page, "page", || {
            let a = http.send("GET", &format!("/agents/{id}"), None)?;
            body = a.body;
            Ok(a.status)
        }));
        if files.is_empty() {
            let text = String::from_utf8_lossy(&body);
            files = text.split('"').filter(|w| w.starts_with("/assets/") && !w.contains(' ')).map(str::to_owned).take(4).collect();
        }
        for f in &files {
            out.push(timed(Class::Page, "file", || http.send("GET", f, None).map(|a| a.status)));
        }
        if !nap(s, jitter(&mut r, Duration::from_millis(p.page_think_ms))) {
            break;
        }
    }
    out
}

// ------------------------------------------------------------------------------------- the round

/// CPU hogs: `n` processes of the lab's own, each spinning until its stdin closes (the lab ends
/// them, or its end does), on the server's CPUs where it has its own. In production's layout, one
/// process of `n` threads in a scope `unit` of its own at the least weight ([`Place`]).
pub struct Hogs(Vec<Child>);

impl Hogs {
    pub fn start(n: usize, place: &Place, unit: &str) -> anyhow::Result<Self> {
        let me = std::env::current_exe()?;
        let mut v = Vec::with_capacity(n);
        let each = if place.production() { usize::from(n > 0) } else { n };
        for _ in 0..each {
            let mut cmd = place.command(&me, HOG_WEIGHT, unit);
            cmd.arg("hog");
            if place.production() {
                cmd.arg("--threads").arg(n.to_string());
            }
            v.push(cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?);
        }
        Ok(Self(v))
    }
}

impl Drop for Hogs {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// A hog: `threads` threads spinning until its stdin closes.
pub fn hog(threads: usize) {
    let stop = Arc::new(AtomicBool::new(false));
    let s = Arc::clone(&stop);
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut sink);
        s.store(true, Ordering::Relaxed);
    });
    let spin = |stop: &AtomicBool| {
        let mut x = 0u64;
        while !stop.load(Ordering::Relaxed) {
            for _ in 0..100_000 {
                x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
            }
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..threads.max(1) {
            scope.spawn(|| spin(&stop));
        }
        spin(&stop);
    });
}

/// How much the machine's IO is held back now (Linux PSI, `full avg10`, percent), where the OS says.
fn io_full() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/pressure/io").ok()?;
    text.lines().find(|l| l.starts_with("full"))?.split_whitespace().find_map(|w| w.strip_prefix("avg10="))?.parse().ok()
}

/// Removes a round's kiln (gigabytes of stores) a file at a time, each file waiting while the
/// machine's IO is held back a quarter of the time or more (`full avg10` 25%: the pace kiln's agents
/// keep big deletes to since 2026-11-04, when deletes at once on one btrfs volume took production
/// down), so it never goes in one burst that stalls the filesystem for everything else on it.
pub fn remove_paced(dir: &Path) {
    let (mut files, mut dirs, mut stack) = (Vec::new(), Vec::new(), vec![dir.to_path_buf()]);
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        dirs.push(d);
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(_) => files.push(e.path()),
                Err(_) => {}
            }
        }
    }
    for f in files {
        // Ten minutes at the most, then on.
        for _ in 0..600 {
            if io_full().is_none_or(|p| p < 25.0) {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let _ = std::fs::remove_file(&f);
    }
    for d in dirs.into_iter().rev() {
        let _ = std::fs::remove_dir(&d);
    }
}

/// The stand-in, where the runners find it: the lab's own binary as `claude`, in `dir/bin`.
pub fn stand_in(dir: &Path) -> anyhow::Result<PathBuf> {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    let to = bin.join(if cfg!(windows) { "claude.exe" } else { "claude" });
    if !to.exists() {
        std::fs::copy(std::env::current_exe()?, &to)?;
    }
    Ok(bin)
}

/// The session ids and their status, from the server's list.
fn statuses(http: &mut Http) -> anyhow::Result<BTreeMap<String, String>> {
    let a = http.send_settled("GET", "/api/harness/sessions", None)?;
    let v: Value = serde_json::from_slice(&a.body)?;
    let list = v.as_array().cloned().or_else(|| v.get("sessions").and_then(Value::as_array).cloned()).unwrap_or_default();
    Ok(list.iter().filter_map(|i| Some((i.get("id")?.as_str()?.to_owned(), i.get("status")?.as_str()?.to_owned()))).collect())
}

/// A command the lab runs as each part of a round begins and ends, from `KILN_LAB_HOOK` (goal 9, plan
/// 01a1a87b2cec step 2: a profiler of the round's server over its window): `<hook> <event> <server's pid>
/// <folder>`, the event `fill-start`, `fill-end`, `window-start` or `window-end`, the folder one of the
/// round's own beside its kiln's (`<round>.hook`, kept when the kiln's is removed). The lab waits for it, so
/// it starts what lasts in the background; one that fails is said and the round goes on.
struct Hook {
    command: Option<PathBuf>,
    pid: u32,
    dir: PathBuf,
}

impl Hook {
    fn from_env(fleet: &Fleet, dir: &Path) -> Self {
        let command = std::env::var_os("KILN_LAB_HOOK").filter(|c| !c.is_empty()).map(PathBuf::from);
        Self { command, pid: fleet.server.child.id(), dir: dir.with_extension("hook") }
    }

    fn run(&self, event: &str) {
        let Some(command) = &self.command else { return };
        let ran = std::fs::create_dir_all(&self.dir).map_err(anyhow::Error::from).and_then(|()| Ok(Command::new(command).arg(event).arg(self.pid.to_string()).arg(&self.dir).stdin(Stdio::null()).status()?));
        match ran {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("kiln-lab: the hook's {event} exited {status}"),
            Err(e) => eprintln!("kiln-lab: the hook's {event} didn't run: {e:#}"),
        }
    }
}

/// One round of `build` with `hogs`, in `dir` (left for the next round's: the caller removes it).
pub fn round(build: &Build, p: &Profile, hogs: usize, place: &Place, dir: &Path, bin: &Path, index: usize) -> anyhow::Result<Round> {
    std::fs::create_dir_all(dir)?;
    let mut fleet = Fleet::server(&build.kiln, dir, &build.env, place, &format!("kiln-lab-server-{}-{index}", std::process::id()))?;
    let names: Vec<String> = (0..p.runners.max(1)).map(|r| format!("lab-runner-{r}")).collect();
    for n in &names {
        fleet.runner(&build.kiln, n, bin)?;
    }
    fleet.runners_ready(Duration::from_secs(180))?;

    // The fill: each session opened on its runner, its conversation written as fast as it's taken. The
    // server's /metrics either side of it too: the conversation feed at its busiest, every session's
    // conversation through its runner's shipper and the server's intake at once (goal 3's heavy path).
    let scrape = |fleet: &Fleet| -> anyhow::Result<Scrape> { Ok(Scrape::parse(&String::from_utf8_lossy(&Http::new(fleet.port, &fleet.admin).send("GET", "/metrics", None)?.body))) };
    let hook = Hook::from_env(&fleet, dir);
    let fill_before = scrape(&fleet)?;
    hook.run("fill-start");
    let filled = Instant::now();
    let mut opened: Vec<(String, u64)> = Vec::new();
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let (port, admin, dir) = (fleet.port, fleet.admin.as_str(), fleet.dir.as_path());
        let handles: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(r, name)| {
                let mine: Vec<(usize, u64)> = p.sessions.iter().copied().enumerate().filter(|(i, _)| i % names.len() == r).collect();
                scope.spawn(move || -> anyhow::Result<Vec<(String, u64)>> {
                    mine.iter().map(|(i, n)| Ok((crate::fleet::open(port, admin, name, &dir.join(format!("w/s{i}")), &format!("lab:prefill:{n}"), Duration::from_secs(120))?, *n))).collect()
                })
            })
            .collect();
        for h in handles {
            opened.extend(h.join().map_err(|_| anyhow::anyhow!("a session opener panicked"))??);
        }
        Ok(())
    })?;
    let mut http = Http::new(fleet.port, &fleet.admin);
    let deadline = Instant::now() + Duration::from_secs(1800);
    let mut filled_ids: HashSet<String> = HashSet::new();
    loop {
        let st = statuses(&mut http)?;
        for (id, n) in &opened {
            // Idle once its first turn ended, with its conversation in the server's store (where
            // this build keeps one: its newest event's place).
            if filled_ids.contains(id) || st.get(id).is_none_or(|s| s != "idle") {
                continue;
            }
            let newest = http.send_settled("GET", &format!("/api/harness/sessions/{id}/events?limit=1"), None)?;
            let seq = serde_json::from_slice::<Value>(&newest.body).ok().and_then(|v| v.pointer("/events/0/seq").and_then(Value::as_u64));
            if newest.status != 200 || seq.is_none_or(|s| s >= *n) {
                filled_ids.insert(id.clone());
            }
        }
        if filled_ids.len() == opened.len() {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the fill didn't finish: {} of {} sessions in", filled_ids.len(), opened.len());
        std::thread::sleep(Duration::from_millis(250));
    }
    let fill_s = filled.elapsed().as_secs_f64();
    hook.run("fill-end");
    let fill_after = scrape(&fleet)?;

    // The MCP tools agents call, where this build has them.
    let tools: Vec<String> = {
        let a = http.send_settled("POST", "/api/mcp", Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}).to_string().as_bytes()))?;
        let v: Value = serde_json::from_slice(&a.body).unwrap_or_default();
        let have: HashSet<String> = v.pointer("/result/tools").and_then(Value::as_array).map(|t| t.iter().filter_map(|t| t.get("name")?.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        ["agents_list", "runners"].iter().filter(|t| have.contains(**t)).map(|t| (*t).to_owned()).collect()
    };

    // The biggest sessions work while people watch: those are the agents that run for days.
    let mut by_size = opened.clone();
    by_size.sort_by_key(|s| std::cmp::Reverse(s.1));
    let active: Vec<String> = by_size.iter().take(p.active).map(|(id, _)| id.clone()).collect();
    let shared = Shared {
        port: fleet.port,
        admin: fleet.admin.clone(),
        sessions: opened.clone(),
        active: active.clone(),
        stop: AtomicBool::new(false),
        next: AtomicU64::new(0),
        dwell: Duration::from_secs(p.dwell_s),
    };

    let mut m = machine();
    m.server_cpus.clone_from(&place.cpus);
    let hogs_run = if hogs > 0 { Some(Hogs::start(hogs, place, &format!("kiln-lab-hogs-{}-{index}", std::process::id()))?) } else { None };
    type Window = (Vec<Sample>, Vec<Watch>, Scrape, Scrape, (u64, u64));
    let (samples, watches, before, after, window) = std::thread::scope(|scope| -> anyhow::Result<Window> {
        let s = &shared;
        let mut views = Vec::new();
        for i in 0..p.viewers {
            views.push(scope.spawn(move || viewer(s, i)));
        }
        let mut handles = Vec::new();
        for (i, id) in active.iter().enumerate() {
            handles.push(scope.spawn(move || driver(s, id, p, i)));
        }
        for i in 0..p.readers {
            handles.push(scope.spawn(move || reader(s, p, i)));
        }
        let tools = &tools;
        for i in 0..p.mcp {
            handles.push(scope.spawn(move || mcp(s, p, i, tools)));
        }
        for i in 0..p.pages {
            handles.push(scope.spawn(move || pages(s, p, i)));
        }
        std::thread::sleep(Duration::from_secs(p.warmup_s));
        let mut scraper = Http::new(s.port, &s.admin);
        m.load_before = loadavg();
        m.cpu_pressure_before = cpu_pressure();
        m.mhz_before = cpu_mhz(m.affinity.as_deref());
        m.server_mhz_before = place.cpus.as_deref().and_then(|c| cpu_mhz(Some(c)));
        let before = Scrape::parse(&String::from_utf8_lossy(&scraper.send("GET", "/metrics", None)?.body));
        hook.run("window-start");
        let start = now_us();
        std::thread::sleep(Duration::from_secs(p.window_s));
        let end = now_us();
        hook.run("window-end");
        let after = Scrape::parse(&String::from_utf8_lossy(&scraper.send("GET", "/metrics", None)?.body));
        m.load_after = loadavg();
        m.cpu_pressure_after = cpu_pressure();
        m.mhz_after = cpu_mhz(m.affinity.as_deref());
        m.server_mhz_after = place.cpus.as_deref().and_then(|c| cpu_mhz(Some(c)));
        s.stop.store(true, Ordering::Relaxed);
        let (mut samples, mut watches) = (Vec::new(), Vec::new());
        for h in views {
            let (seen, watched) = h.join().map_err(|_| anyhow::anyhow!("a viewer panicked"))?;
            samples.extend(seen);
            watches.extend(watched);
        }
        for h in handles {
            samples.extend(h.join().map_err(|_| anyhow::anyhow!("a worker panicked"))?);
        }
        Ok((samples, watches, before, after, (start, end)))
    })?;
    drop(hogs_run);
    let in_window: Vec<Sample> = samples.into_iter().filter(|s| s.at_us >= window.0 && s.at_us < window.1).collect();
    #[allow(clippy::cast_precision_loss)]
    let secs = (window.1.saturating_sub(window.0)) as f64 / 1e6;
    let seen = measure::seen(&in_window);
    let (mut numbers, shards) = numbers(&seen, &before, &after, secs);
    numbers.insert("fill.s".into(), fill_s);
    server_numbers(&mut numbers, "fill.", &fill_before, &fill_after, fill_s);
    let (held, fill_held) = (measure::held(&before, &after, secs), measure::held(&fill_before, &fill_after, fill_s));
    let locks = lock_census((&fill_before, &fill_after), (&before, &after));
    drop(fleet);
    let mut seen: BTreeMap<String, measure::Seen> = seen.into_iter().map(|(c, s)| (c.name().to_owned(), s)).collect();
    seen.extend(measure::seen_by_what(&in_window));
    seen.extend(frames_against_attaches(&watches, window));
    Ok(Round { build: build.label.clone(), index, hogs, machine: m, fill_s, shards, numbers, seen, held, fill_held, locks })
}

/// The window's frames (stamped in it) told apart by how they met the attaches, as each viewer saw
/// them (goal 10, plan 01a1a87b2dae step 3): `websocket.own_attach`, those published before the viewer's
/// own snapshot came, which waited for it; `websocket.while_another_attaches`, those published while
/// another viewer of the same session attached (its snapshot not yet come), which shouldn't wait for
/// that; and `websocket.live`, the rest.
pub fn frames_against_attaches(watches: &[Watch], window: (u64, u64)) -> BTreeMap<String, measure::Seen> {
    let mut by: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for w in watches {
        for &(stamp, took) in &w.frames {
            if stamp < window.0 || stamp >= window.1 {
                continue;
            }
            let group = if stamp < w.to_us {
                "websocket.own_attach"
            } else if watches.iter().any(|o| o.viewer != w.viewer && o.session == w.session && o.from_us <= stamp && stamp < o.to_us) {
                "websocket.while_another_attaches"
            } else {
                "websocket.live"
            };
            by.entry(group.to_owned()).or_default().push(took);
        }
    }
    by.into_iter().map(|(k, took)| (k, measure::seen_of(took, 0))).collect()
}

/// The numbers a round reports, from what the lab saw and the server's `/metrics` either side of
/// the window: each class's median, p99 and p99.9 and its most (the stall tail), throughput, the
/// server's CPU for each request, and the shards' and routes' own figures where the build has them.
fn numbers(seen: &BTreeMap<Class, measure::Seen>, before: &Scrape, after: &Scrape, secs: f64) -> (BTreeMap<String, f64>, usize) {
    let mut n = BTreeMap::new();
    let mut requests = 0u64;
    for (c, s) in seen {
        let name = c.name();
        for (k, v) in [("p50_ms", s.p50_ms), ("p99_ms", s.p99_ms), ("p999_ms", s.p999_ms), ("max_ms", s.max_ms)] {
            if let Some(v) = v {
                n.insert(format!("{name}.{k}"), v);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        n.insert(format!("{name}.per_s"), s.n as f64 / secs.max(1e-9));
        // What failed (a status of 400 or more, or no answer): an overloaded server's sheds among them.
        #[allow(clippy::cast_precision_loss)]
        n.insert(format!("{name}.errors"), s.errors as f64);
        if *c != Class::Websocket {
            requests += s.n;
        }
    }
    let diff = |name: &str, want: &[(&str, &str)]| Some(after.get(name, want)? - before.get(name, want)?);
    if let Some(cpu) = diff("process_cpu_seconds_total", &[])
        && requests > 0
    {
        #[allow(clippy::cast_precision_loss)]
        n.insert("server.cpu_ms_per_request".into(), cpu * 1000.0 / requests as f64);
    }
    if let Some(rss) = after.get("process_resident_memory_bytes", &[]) {
        n.insert("server.rss_mb".into(), rss / 1e6);
    }
    // The resident memory by kind and what each of the server's caches and buffers holds, where the build
    // says them (`kiln_memory_*`; goal 11, plan 01a1a87b2e56 step 1): anon is what the server's own work
    // holds, file the binary's pages read in.
    for kind in after.values_of("kiln_memory_resident_bytes", "kind") {
        if let Some(b) = after.get("kiln_memory_resident_bytes", &[("kind", kind.as_str())]) {
            n.insert(format!("server.rss_{kind}_mb"), b / 1e6);
        }
    }
    for owner in after.values_of("kiln_memory_bytes", "owner") {
        if let Some(b) = after.get("kiln_memory_bytes", &[("owner", owner.as_str())]) {
            n.insert(format!("server.memory.{owner}_mb"), b / 1e6);
        }
    }
    let shards = server_numbers(&mut n, "", before, after, secs);
    // The routes': how long each class took and waited at the server.
    for class in ["page", "api_read", "api_write", "websocket", "runner"] {
        let want = [("class", class)];
        for (family, key) in [("kiln_route_seconds", "took"), ("kiln_route_wait_seconds", "wait")] {
            let d = measure::delta(&before.buckets(family, &want), &after.buckets(family, &want));
            if let Some(v) = measure::histogram_quantile(0.99, &d) {
                n.insert(format!("server.{class}.{key}_p99_ms"), v * 1000.0);
            }
        }
    }
    // The requests the server shed, by class (`kiln_shed_total`, goal 6), where the build counts them.
    for class in ["background", "normal"] {
        if let Some(v) = diff("kiln_shed_total", &[("class", class)]) {
            n.insert(format!("server.shed.{class}"), v);
        }
    }
    (n, shards)
}

/// The server's own numbers between two scrapes `secs` apart, each named after `prefix` (the window's, or
/// the fill's: `fill.`), where the build has their series: its CPU share; the shards' delays together, their
/// busy share on average and their slow polls a second, all and by cause (`task`, `blocking`, `host`,
/// `unknown`); the compute pool's jobs and pieces a second, how long its jobs waited for a thread and its
/// pieces ran, and its threads busy on average; and, for each of the blocking pool's shares, its jobs a
/// second, how long they waited for a place and ran, the places they held on average (their runs' time over
/// the window's), and the places Little's law asks of the rate and the p99 run (as hyper-rt sizes a shard's
/// tasks: `admission_limit`). How many shards there were.
fn server_numbers(n: &mut BTreeMap<String, f64>, prefix: &str, before: &Scrape, after: &Scrape, secs: f64) -> usize {
    let diff = |name: &str, want: &[(&str, &str)]| Some(after.get(name, want)? - before.get(name, want)?);
    let per_s = |v: f64| v / secs.max(1e-9);
    let histogram = |name: &str, want: &[(&str, &str)]| measure::delta(&before.buckets(name, want), &after.buckets(name, want));
    let mut put = |name: String, v: Option<f64>| {
        if let Some(v) = v {
            n.insert(format!("{prefix}{name}"), v);
        }
    };
    put("server.cpu_share".into(), diff("process_cpu_seconds_total", &[]).map(per_s));
    // The shards': their delays together and each shard's, to the microsecond (median, p99, p99.9 and the top
    // bucket reached: goal 9, plan 01a1a87b2cec step 1), their busy share on average and each one's, the
    // worst shard's p99.9 and busy share, and their slow polls.
    let shards = after.values_of("kiln_shard_polls_total", "shard");
    if !shards.is_empty() {
        let mut delay: Vec<(f64, f64)> = Vec::new();
        let mut busy = 0.0;
        let (mut worst_p999, mut worst_busy) = (None::<f64>, None::<f64>);
        let mut slow = [("task", 0.0), ("blocking", 0.0), ("host", 0.0), ("unknown", 0.0)];
        for sh in &shards {
            let want = [("shard", sh.as_str())];
            let d = histogram("kiln_shard_delay_seconds", &want);
            let p999 = delay_us(&mut put, &format!("shard{sh}."), &d);
            worst_p999 = most(worst_p999, p999);
            if delay.is_empty() {
                delay = d;
            } else {
                for ((_, total), (_, add)) in delay.iter_mut().zip(d) {
                    *total += add;
                }
            }
            let own = diff("kiln_shard_busy_seconds_total", &want).map(per_s);
            put(format!("shard{sh}.busy_share"), own);
            worst_busy = most(worst_busy, own);
            busy += own.unwrap_or(0.0);
            for (cause, total) in &mut slow {
                *total += diff("kiln_shard_slow_polls_total", &[("shard", sh.as_str()), ("cause", *cause)]).unwrap_or(0.0);
            }
        }
        delay_us(&mut put, "shards.", &delay);
        put("shards.worst_delay_p999_us".into(), worst_p999);
        put("shards.worst_busy_share".into(), worst_busy);
        #[allow(clippy::cast_precision_loss)]
        let count = shards.len() as f64;
        put("shards.busy_share".into(), Some(busy / count));
        put("shards.slow_polls_per_s".into(), Some(per_s(slow.iter().map(|(_, v)| v).sum())));
        for (cause, v) in slow {
            put(format!("shards.slow_polls_{cause}_per_s"), Some(per_s(v)));
        }
        // The most tasks any shard held, where the build says it: what a shard's task slots are sized from
        // (goal 11, plan 01a1a87b2e56 step 3).
        let most = shards.iter().filter_map(|sh| after.get("kiln_shard_tasks_most", &[("shard", sh.as_str())])).reduce(f64::max);
        put("shards.tasks_most".into(), most);
    }
    // The bus's (goal 11, plan 01a1a87b2e56 step 3): how long a live feed's frame took from its publish to a
    // viewer's read, the frames published and the viewers sent back to a snapshot: what a topic needs to keep.
    put("bus.published_per_s".into(), diff("kiln_bus_published_total", &[]).map(per_s));
    put("bus.lag_notices".into(), diff("kiln_bus_lag_notices_total", &[]));
    let delivery = histogram("kiln_bus_delivery_seconds", &[]);
    put("bus.delivery_p99_ms".into(), measure::histogram_quantile(0.99, &delivery).map(|v| v * 1000.0));
    put("bus.delivery_p999_ms".into(), measure::histogram_quantile(0.999, &delivery).map(|v| v * 1000.0));
    put("bus.delivery_top_ms".into(), measure::histogram_top(&delivery).map(|v| v * 1000.0));
    put("bus.topics".into(), after.get("kiln_bus_topics", &[]));
    // The compute pool's (goal 3): where the heavy work went, how long it waited and how long a piece held a
    // pool thread.
    if let Some(jobs) = diff("kiln_compute_jobs_total", &[]) {
        put("compute.jobs_per_s".into(), Some(per_s(jobs)));
        put("compute.pieces_per_s".into(), diff("kiln_compute_pieces_total", &[]).map(per_s));
        let (wait, piece) = (histogram("kiln_compute_wait_seconds", &[]), histogram("kiln_compute_piece_seconds", &[]));
        put("compute.wait_p99_ms".into(), measure::histogram_quantile(0.99, &wait).map(|v| v * 1000.0));
        put("compute.piece_p99_ms".into(), measure::histogram_quantile(0.99, &piece).map(|v| v * 1000.0));
        put("compute.piece_top_ms".into(), measure::histogram_top(&piece).map(|v| v * 1000.0));
        put("compute.busy".into(), diff("kiln_compute_piece_seconds_sum", &[]).map(per_s));
    }
    // The blocking pool's shares (goal 3, step 4: what their sizes are derived from).
    for share in after.values_of("kiln_blocking_run_seconds_count", "share") {
        let want = [("share", share.as_str())];
        let Some(jobs) = diff("kiln_blocking_run_seconds_count", &want) else { continue };
        let run_p99 = measure::histogram_quantile(0.99, &histogram("kiln_blocking_run_seconds", &want));
        put(format!("blocking.{share}.per_s"), Some(per_s(jobs)));
        put(format!("blocking.{share}.run_p99_ms"), run_p99.map(|v| v * 1000.0));
        put(format!("blocking.{share}.wait_p99_ms"), measure::histogram_quantile(0.99, &histogram("kiln_blocking_wait_seconds", &want)).map(|v| v * 1000.0));
        put(format!("blocking.{share}.busy"), diff("kiln_blocking_run_seconds_sum", &want).map(per_s));
        put(format!("blocking.{share}.little_p99"), run_p99.map(|w| per_s(jobs) * w));
    }
    shards.len()
}

/// The larger of two numbers either may lack.
fn most(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// A shard delay histogram's numbers, each `prefix` then its name, in microseconds: its median, p99, p99.9 and
/// the top bucket reached (an observation's bound from above: `+Inf` past the last). Its p99.9, if it has one.
fn delay_us(put: &mut impl FnMut(String, Option<f64>), prefix: &str, buckets: &[(f64, f64)]) -> Option<f64> {
    let us = |v: f64| v * 1e6;
    let p999 = measure::histogram_quantile(0.999, buckets).map(us);
    put(format!("{prefix}delay_p50_us"), measure::histogram_quantile(0.5, buckets).map(us));
    put(format!("{prefix}delay_p99_us"), measure::histogram_quantile(0.99, buckets).map(us));
    put(format!("{prefix}delay_p999_us"), p999);
    put(format!("{prefix}delay_max_us"), measure::histogram_top(buckets).filter(|v| v.is_finite()).map(us));
    p999
}

/// Which way each number is better: each class's throughput up, everything else (latency, CPU, delay,
/// slow polls, the pools' work) down.
pub fn better(name: &str) -> crate::stats::Better {
    let classes = ["page.", "api_read.", "api_write.", "websocket.", "attach."];
    if name.ends_with(".per_s") && classes.iter().any(|c| name.starts_with(c)) {
        crate::stats::Better::Higher
    } else {
        crate::stats::Better::Lower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_affinity_list_names_its_cpus() {
        assert_eq!(cpus_in("0-3,8,10-11"), vec![0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(cpus_in("16-31").len(), 16);
        assert!(cpus_in("").is_empty());
    }

    /// Goal 6, plan 01a1a59c2abe step 4: a worker waits out a shed answer's `Retry-After` before asking again,
    /// and its own think time when that is longer.
    #[test]
    fn g6_s4_a_worker_waits_out_a_sheds_retry_after() {
        let (s, ms) = (Duration::from_secs, Duration::from_millis);
        assert_eq!(pause(ms(50), None), ms(50), "no Retry-After: its think time");
        assert_eq!(pause(Duration::ZERO, Some(s(2))), s(2), "no think time: the Retry-After");
        assert_eq!(pause(s(3), Some(s(1))), s(3), "its think time when longer");
    }

    /// Goal 10, plan 01a1a87b2dae step 3: a frame is told apart by whether it waited for its own
    /// viewer's attach, came while another viewer of its session attached, or neither.
    #[test]
    fn g10_s3_frames_are_told_apart_by_the_attaches_they_met() {
        let w = |viewer: usize, session: &str, from_us: u64, to_us: u64, frames: Vec<(u64, u64)>| Watch { viewer, session: session.to_owned(), from_us, to_us, frames };
        let watches = vec![
            // Viewer 0 of a, attached 100 to 200: 150 waited for its snapshot; 250 and 320 after it.
            w(0, "a", 100, 200, vec![(150, 60), (250, 1), (320, 2)]),
            // Viewer 1 of a attaches 240 to 300: 250 came to viewer 0 while it did.
            w(1, "a", 240, 300, vec![(310, 3)]),
            // Viewer 2 of b, attaching 245 to 400: no bearing on a's frames.
            w(2, "b", 245, 400, vec![(410, 4), (9_999, 5)]),
        ];
        let seen = frames_against_attaches(&watches, (0, 1_000));
        let n = |k: &str| seen.get(k).map_or(0, |s| s.n);
        assert_eq!((n("websocket.own_attach"), n("websocket.while_another_attaches"), n("websocket.live")), (1, 1, 3), "outside the window isn't counted");
        assert_eq!(seen.get("websocket.own_attach").and_then(|s| s.p50_ms), Some(0.06));
    }

    /// Goal 10, plan 01a1a87b2dae step 1: the server and its hogs are placed on their own CPUs, and in
    /// production's layout in scopes of their own at their weights.
    #[test]
    fn g10_s1_the_server_and_hogs_are_placed_where_asked() {
        let args = |c: &Command| std::iter::once(c.get_program()).chain(c.get_args()).map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        let k = Path::new("/bin/kiln");
        assert_eq!(args(&Place::default().server(k, "u")), "/bin/kiln");
        if cfg!(target_os = "linux") {
            let p = Place::of(Some("16-31"), "shared").unwrap_or_default();
            assert_eq!(args(&p.server(k, "u")), "taskset -c 16-31 /bin/kiln");
            let p = Place::of(Some("16-31"), "production").unwrap_or_default();
            assert_eq!(args(&p.server(k, "u")), "systemd-run --user --scope --quiet --collect --unit=u --property=CPUWeight=1000 -- taskset -c 16-31 /bin/kiln");
            assert_eq!(args(&p.command(k, HOG_WEIGHT, "h")), "systemd-run --user --scope --quiet --collect --unit=h --property=CPUWeight=1 -- taskset -c 16-31 /bin/kiln");
        }
        assert!(Place::of(None, "elsewhere").is_err() && Place::of(Some("x"), "shared").is_err());
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: the many-sessions profile is production's whole shape, every
    /// session at work, at the realistic profile's sizes and pace.
    #[test]
    fn g4_s5_many_is_productions_whole_shape() {
        let (many, realistic) = (Profile::many(), Profile::realistic());
        assert_eq!((many.runners, many.sessions.len(), many.active), (10, 134, 134));
        assert!(realistic.sessions.iter().all(|n| many.sessions.contains(n)), "the realistic profile's sizes");
        assert_eq!(many.sessions.iter().filter(|n| **n == 400).count(), 126);
        assert_eq!((many.turn_every_ms, many.turn_think_ms, many.viewers, many.window_s), (realistic.turn_every_ms, realistic.turn_think_ms, realistic.viewers, realistic.window_s));
    }

    #[test]
    fn stamps_are_read_wherever_a_frame_has_them() {
        let f = br#"{"t":"events","events":[{"text":"@lab:1730000000123456 the retry"},{"text":"no stamp"},{"output":"@lab:42 x @lab:"}]}"#;
        assert_eq!(stamps(f).collect::<Vec<_>>(), vec![1_730_000_000_123_456, 42]);
        assert_eq!(stamps(b"nothing").count(), 0);
    }
}
