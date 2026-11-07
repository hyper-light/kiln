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

use anyhow::Context as _;
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
    /// Of the active sessions, how many work in subagent turns instead (the first of them, the longest): a Task's call,
    /// `subagent_events` events of its subagent's, `turn_every_ms` apart, then the Task's result, which updates the Task's
    /// tool event that many events after it began (copies-lat2's A2(b) shape, on shards with the other sessions; Ada's own
    /// sessions run subagents everywhere). The other sessions' frames are told apart from theirs
    /// ([`frames_by_subagents`]). None but in the `subagents` profile.
    #[serde(default)]
    pub subagents: usize,
    /// Each subagent turn's events between its Task's call and its result.
    #[serde(default)]
    pub subagent_events: u64,
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
            subagents: 0,
            subagent_events: 100,
        }
    }

    /// The realistic profile with two of its eight active sessions working in subagent turns of 100 events: each Task's
    /// tool event updated more than 70 events after it began, on shards with the other sessions (copies-lat2's A2(b)).
    pub fn subagents() -> Self {
        Self { subagents: 2, subagent_events: 100, ..Self::realistic() }
    }

    /// The realistic profile with no think time for its people and readers: each viewer opens the next
    /// conversation as soon as the last one is open, each reader asks again as soon as it is answered, a closed
    /// loop at the most its clients can ask. A server of one shard falls behind it on purpose (goal 6, plan
    /// 01a1a59c2abe step 4: what it sheds, by class, and what it still serves).
    pub fn overload() -> Self {
        Self { dwell_s: 0, read_think_ms: 0, ..Self::realistic() }
    }

    /// The overload profile with many more clients: 64 readers and 8 agents calling MCP, none thinking, so a server
    /// of one shard has more work than that shard can run, whatever its pools do (goal 6, plan 01a1a59c2abe step 4:
    /// under the overload profile, fedora 2026-11-05, the one shard was 34% busy while the pools' threads took the
    /// server's CPUs, so no queue of its own stood and a calm-learned limit had nothing to shed). Its readers' event
    /// catch-ups are Background, its other reads and the MCP calls Normal, its pages and `/api/me` Critical.
    pub fn flood() -> Self {
        Self { readers: 64, mcp: 8, mcp_think_ms: 0, ..Self::overload() }
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

    /// The profile `name` names (`--profile`).
    pub fn named(name: &str) -> anyhow::Result<Self> {
        Ok(match name {
            "small" => Self::small(),
            "realistic" => Self::realistic(),
            "overload" => Self::overload(),
            "many" => Self::many(),
            "flood" => Self::flood(),
            "subagents" => Self::subagents(),
            p => anyhow::bail!("no profile {p}: realistic, overload, flood, many, subagents or small"),
        })
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
            subagents: 0,
            subagent_events: 100,
        }
    }
}

/// A build under test: a label, its `kiln` binary, and what its server's environment has besides.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Build {
    pub label: String,
    pub kiln: PathBuf,
    pub env: Vec<(String, String)>,
    /// Its server's `[bus] memory` in bytes, when the run sets it (`--bus-memory`).
    #[serde(default)]
    pub bus_memory: Option<u64>,
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
    /// The lab's reads an interruption cut short in the window (`crate::rx::interrupted`): its processes stopped or
    /// frozen and let go on, or signalled. Its clients' latencies include those stops and its throughput lost them (goal 1,
    /// plan 01a1a59c2a68 step 3: g9p21's windows, frozen by fedora's runner's headroom a fifth to a third of the time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_reads: Option<u64>,
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
    /// Each number the round reports, every one finite: one that isn't is absent ([`finite_numbers`]).
    #[serde(deserialize_with = "finite_numbers")]
    pub numbers: BTreeMap<String, f64>,
    pub seen: BTreeMap<String, measure::Seen>,
    /// What held the shards past 100 µs in the window, and over the fill (`kiln_shard_held_*`, goal 9, plan
    /// 01a1a87b2cec step 2), each the most first: a build without the series has none.
    #[serde(default)]
    pub held: Vec<measure::Held>,
    #[serde(default)]
    pub fill_held: Vec<measure::Held>,
    /// The step quantum the server's shards ran with, µs (`kiln_shard_quantum_seconds`, goal 9, plan 01a1a87b2cec
    /// step 3): what the report holds what held the shards to. None from a build from before it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantum_us: Option<f64>,
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
    /// The copies of the site's location the build counted it under, summed in each figure above (goal 5, plan
    /// 01a1a59c2aae step 1; `lock_sums`). 0 from a kiln-lab from before it summed them, whose figures for a site
    /// of two or more took one copy's count at the start and another's at the end (`scripts/locks.py rank`
    /// refuses those runs).
    #[serde(default)]
    pub copies: usize,
}

/// One scrape's `kiln_lock_*` figures for a site and kind, over every series with its labels.
#[derive(Default)]
struct LockSums {
    taken: f64,
    contended: f64,
    wait_s: f64,
    max_wait_s: f64,
    addrs: Vec<String>,
    copies: usize,
}

/// Each site's and kind's figures in one scrape, summed over its copies (goal 5, plan 01a1a59c2aae step 1). The
/// census keys its counters by the address of the site's `Location` (scripts/locks/census.rs), and the compiler
/// makes one of those for each instance of a generic caller and each codegen unit an inlined caller lands in, so
/// one site can have several counters, each its own series under the same labels: census-B's build has 53 such
/// sites of 280, event-listener's 2 or 3 copies each. Taking a site's start from its first series and its end
/// from its last paired two counters, which put event-listener's window below zero.
fn lock_sums(s: &Scrape) -> BTreeMap<(String, String), LockSums> {
    let label = |l: &[(String, String)], k: &str| l.iter().find(|(lk, _)| lk == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let mut out: BTreeMap<(String, String), LockSums> = BTreeMap::new();
    for (name, labels, value) in &s.series {
        let Some(figure) = name.strip_prefix("kiln_lock_") else { continue };
        let site = label(labels, "site");
        if site.is_empty() {
            continue;
        }
        let e = out.entry((site, label(labels, "kind"))).or_default();
        match figure {
            "taken_total" => {
                e.taken += value;
                e.copies += 1;
            }
            "contended_total" => e.contended += value,
            "wait_seconds_total" => e.wait_s += value,
            "wait_max_seconds" => e.max_wait_s = e.max_wait_s.max(*value),
            "addresses" => {
                for a in label(labels, "addrs").split_whitespace() {
                    if !e.addrs.iter().any(|x| x == a) {
                        e.addrs.push(a.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The lock census of a build that counts its locks, between the fill's scrapes and the window's: each site's
/// growth over each, its copies summed in every scrape first (`lock_sums`). A copy first taken between two
/// scrapes is in the second alone, and its counters began at zero, so no figure can come out below zero.
fn lock_census(fill: (&Scrape, &Scrape), window: (&Scrape, &Scrape)) -> BTreeMap<String, LockSeen> {
    let (fill_start, fill_end, start) = (lock_sums(fill.0), lock_sums(fill.1), lock_sums(window.0));
    let none = LockSums::default();
    lock_sums(window.1)
        .into_iter()
        .map(|(key, end)| {
            let (f0, f1, s) = (fill_start.get(&key).unwrap_or(&none), fill_end.get(&key).unwrap_or(&none), start.get(&key).unwrap_or(&none));
            let seen = LockSeen {
                kind: key.1.clone(),
                taken: end.taken - s.taken,
                contended: end.contended - s.contended,
                wait_s: end.wait_s - s.wait_s,
                fill_taken: f1.taken - f0.taken,
                fill_contended: f1.contended - f0.contended,
                fill_wait_s: f1.wait_s - f0.wait_s,
                max_wait_s: end.max_wait_s,
                addrs: end.addrs.join(" "),
                copies: end.copies,
            };
            (format!("{} {}", key.0, key.1), seen)
        })
        .collect()
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

/// Stops the window's clients as it goes, however the window's own thread leaves the window: at its end, on an error,
/// in a panic. The window's scope waits for every client, so this is what makes it end (goal 1, plan 01a1a59c2a68 step
/// 3: in goal 9's lab g9p21, round 3 calm's last scrape of /metrics failed and returned before saying stop, and the scope
/// waited 15 minutes on clients that kept loading the server, until the lab's unit was stopped; rounds 5 calm and hogs
/// the same).
struct Stops<'a>(&'a AtomicBool);

impl Drop for Stops<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
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
    Sample { class, at_us, took_us: u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX), ok, what, server: None, arrived_us: None }
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

/// Whether a session socket's text frame brings events, an event's own frame (`{"t":"events"…}`, or its text
/// streamed: `{"t":"stream"…}`): the frames whose stamps are deliveries, as `websocket` times them. A state's
/// frame quotes an event's words (its `summary` is the agent's latest), and so may a meta's: after an attach,
/// the first carried the stamp of an event the viewer took in its snapshot seconds before, counted as a frame
/// that took that long (goal 10, plan 01a1a87b2dae step 5: rt5's `websocket.own_attach` p50 0.9 s, most 2.6 s,
/// which were most of the class's p99.9 and all of its most).
fn brings_events(text: &[u8]) -> bool {
    text.starts_with(br#"{"t":"events""#) || text.starts_with(br#"{"t":"stream""#)
}

/// The stamps a live frame brings that are its own news, as `websocket` times them: each seen on this socket for the
/// first time, and written once the viewer had asked for the session (`from_us`). One written before is of an event the
/// viewer's snapshot already held, re-sent whole with an update to it, such as a Task's call in its result's frame
/// (g9p35-sub, 2026-11-06: every subagent frame read past 2.5 s was a call re-carried so, a whole subagent turn after it
/// was written, counted as `websocket.own_attach`): it says nothing of how long that frame took. Every stamp is marked
/// seen either way.
fn frames_news(text: &[u8], seen: &mut HashSet<u64>, from_us: u64) -> Vec<u64> {
    stamps(text).filter(|&stamp| seen.insert(stamp) && stamp >= from_us).collect()
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
                out.push(Sample { class: Class::Attach, at_us, took_us: 0, ok: false, what: "", server: None, arrived_us: None });
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
        out.push(Sample { class: Class::Attach, at_us, took_us: u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX), ok: opened, what: "", server: None, arrived_us: None });
        if !opened {
            continue;
        }
        let mut watch = Watch { viewer: i, session: id, from_us: at_us, to_us: now_us(), frames: Vec::new() };
        let until = Instant::now() + jitter(&mut r, dwell);
        let mut seen = HashSet::new();
        while Instant::now() < until && !s.stop.load(Ordering::Relaxed) {
            match sock.next() {
                Ok(Frame::Text(text)) if brings_events(&text) => {
                    // When the frame arrived at the socket, where it's stamped: the rest is the lab's own wake and read.
                    let (now, arrived_us) = (now_us(), sock.arrived());
                    for stamp in frames_news(&text, &mut seen, watch.from_us) {
                        out.push(Sample { class: Class::Websocket, at_us: stamp, took_us: now.saturating_sub(stamp), ok: true, what: "", server: None, arrived_us });
                        watch.frames.push((stamp, now.saturating_sub(stamp)));
                    }
                }
                // A catch-up (a whole again), a frame that only quotes events (a state, a meta), or nothing yet.
                Ok(Frame::Binary | Frame::Text(_) | Frame::Nothing) => {}
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
    // The first `subagents` of the active sessions work in subagent turns (their Task's call and result besides).
    let (text, events) = if i < p.subagents {
        (format!("lab:subagent:{}:{}", p.subagent_events, p.turn_every_ms), p.subagent_events.saturating_add(2))
    } else {
        (format!("lab:turn:{}:{}", p.turn_events, p.turn_every_ms), p.turn_events)
    };
    let turn = Duration::from_millis(events.saturating_mul(p.turn_every_ms));
    let path = format!("/api/harness/sessions/{id}/command");
    // Not all at once.
    if !nap(s, jitter(&mut r, Duration::from_millis(p.turn_think_ms))) {
        return out;
    }
    while !s.stop.load(Ordering::Relaxed) {
        let n = s.next.fetch_add(1, Ordering::Relaxed);
        let body = json!({"type": "send", "id": format!("lab-{n}"), "text": text}).to_string();
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
    // Each read asks the server for its own time with it, so the slowest are split (goal 10, plan 01a1a87b2dae
    // step 5: `measure::split`).
    let mut http = Http::new(s.port, &s.admin).timed();
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
        let (mut retry_after, mut server, mut arrived_us) = (None, None, None);
        let sample = timed(Class::ApiRead, what, || {
            http.send("GET", &path, None).map(|a| {
                retry_after = a.retry_after;
                server = a.server;
                arrived_us = a.arrived_us;
                a.status
            })
        });
        out.push(Sample { server, arrived_us, ..sample });
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

/// Keeps a scrape of the round's server's `/metrics` as it came, `name` in the round's `<round>.hook` folder (kept when
/// its kiln's is removed, [`Hook`]): every family a round reads and every one it doesn't yet, to read afterwards (goal 9,
/// plan 01a1a87b2cec step 5: the shards' timer wakes by site, `kiln_shard_timer_wakes_total`, and their kicks by sender,
/// `kiln_shard_kicks_in_total`, between the window's two). One that can't be written is said, and the round goes on.
fn keep_scrape(round: &Path, name: &str, body: &[u8]) {
    let dir = round.with_extension("hook");
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join(name), body)) {
        eprintln!("kiln-lab: the window's scrape {name} wasn't kept: {e}");
    }
}

/// A command the lab runs as each part of a round begins and ends, from `KILN_LAB_HOOK` (goal 9, plan
/// 01a1a87b2cec step 2: a profiler of the round's server over its window): `<hook> <event> <server's pid>
/// <folder>`, the event `fill-start`, `fill-end`, `window-start` or `window-end`, the folder one of the
/// round's own beside its kiln's (`<round>.hook`, kept when the kiln's is removed). The lab waits for it, so
/// it starts what lasts in the background; one that fails is said and the round goes on. It runs in a process
/// group of its own, and one still running after `within` is stopped with its group, said, and the round goes on
/// too (goal 1, plan 01a1a59c2a68 step 3: no wait of the lab's without an end).
struct Hook {
    command: Option<PathBuf>,
    pid: u32,
    dir: PathBuf,
    /// The longest the lab waits for it: [`PATIENCE`], the lab's bound on any one wait.
    within: Duration,
}

impl Hook {
    fn from_env(fleet: &Fleet, dir: &Path) -> Self {
        let command = std::env::var_os("KILN_LAB_HOOK").filter(|c| !c.is_empty()).map(PathBuf::from);
        Self { command, pid: fleet.server.child.id(), dir: dir.with_extension("hook"), within: PATIENCE }
    }

    fn run(&self, event: &str) {
        let Some(command) = &self.command else { return };
        let ran = std::fs::create_dir_all(&self.dir).map_err(anyhow::Error::from).and_then(|()| {
            let mut cmd = Command::new(command);
            cmd.arg(event).arg(self.pid.to_string()).arg(&self.dir).stdin(Stdio::null());
            #[cfg(unix)]
            std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
            Ok(waited(cmd.spawn()?, self.within)?)
        });
        match ran {
            Ok(Some(status)) if status.success() => {}
            Ok(Some(status)) => eprintln!("kiln-lab: the hook's {event} exited {status}"),
            Ok(None) => eprintln!("kiln-lab: the hook's {event} still ran after {:?}: stopped, with what it started", self.within),
            Err(e) => eprintln!("kiln-lab: the hook's {event} didn't run: {e:#}"),
        }
    }
}

/// Waits for `child` up to `within`: its status, or none once `within` passed, when it's killed with what it started
/// ([`kill_tree`]) and reaped. A wait the kill can't end either is left to itself, not waited on again.
fn waited(mut child: Child, within: Duration) -> std::io::Result<Option<std::process::ExitStatus>> {
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    match rx.recv_timeout(within) {
        Ok(status) => status.map(Some),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            kill_tree(pid);
            let _ = rx.recv_timeout(within);
            Ok(None)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other("its wait ended without its status")),
    }
}

/// Kills process `pid` and what it started: its process group, which it leads (Unix: the hook's, as `crate::fleet::clean`
/// makes the server's and the runners'), or its process tree (Windows).
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill").arg("-KILL").arg(format!("-{pid}")).stdout(Stdio::null()).stderr(Stdio::null()).status();
    #[cfg(windows)]
    let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// `p`'s runners of `build` for `fleet`'s server, each its own `kiln agent` whose `claude` is the stand-in in `bin`,
/// connected and with their headroom off: their names. (`kiln-lab rate` starts kiln this way too: goal 10, Ada's
/// fixed-rate benchmark.)
pub fn runners(fleet: &mut Fleet, build: &Build, p: &Profile, bin: &Path) -> anyhow::Result<Vec<String>> {
    let names: Vec<String> = (0..p.runners.max(1)).map(|r| format!("lab-runner-{r}")).collect();
    for n in &names {
        fleet.runner(&build.kiln, n, bin)?;
    }
    fleet.runners_ready(Duration::from_secs(180))?;
    Ok(names)
}

/// The fill: each of `p`'s sessions opened on its runner (of `names`), its conversation written as fast as it's
/// taken, until every one is idle with its conversation in the server's store: the sessions (each its id and its
/// events) and the seconds it took.
pub fn fill(fleet: &Fleet, p: &Profile, names: &[String]) -> anyhow::Result<(Vec<(String, u64)>, f64)> {
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
    Ok((opened, filled.elapsed().as_secs_f64()))
}

/// One round of `build` with `hogs`, in `dir` (left for the next round's: the caller removes it).
///
/// Each step's failure says which step it was (goal 1, plan 01a1a59c2a68 step 3: g9p21's rounds that failed said only
/// "Interrupted system call (os error 4)").
pub fn round(build: &Build, p: &Profile, hogs: usize, place: &Place, dir: &Path, bin: &Path, index: usize) -> anyhow::Result<Round> {
    std::fs::create_dir_all(dir).with_context(|| format!("the round's folder {}", dir.display()))?;
    let mut fleet = Fleet::server(&build.kiln, dir, &build.env, build.bus_memory, place, &format!("kiln-lab-server-{}-{index}", std::process::id())).context("the round's server")?;
    // The hook hears of the server before its runners connect: what a runner sends as it does (its Build, its
    // ReachHello) can be profiled from here (goal 9, plan 01a1a87b2cec step 3; `scripts/shard_lab/hook.sh`'s PERF_FROM).
    let hook = Hook::from_env(&fleet, dir);
    hook.run("server-up");
    let names = runners(&mut fleet, build, p, bin).context("the round's runners")?;

    // The fill: each session opened on its runner, its conversation written as fast as it's taken. The
    // server's /metrics either side of it too: the conversation feed at its busiest, every session's
    // conversation through its runner's shipper and the server's intake at once (goal 3's heavy path).
    let scrape = |fleet: &Fleet| -> anyhow::Result<Scrape> { Ok(Scrape::parse(&String::from_utf8_lossy(&Http::new(fleet.port, &fleet.admin).send("GET", "/metrics", None)?.body))) };
    let fill_before = scrape(&fleet).context("the fill's first scrape of the server's /metrics")?;
    hook.run("fill-start");
    let (opened, fill_s) = fill(&fleet, p, &names).context("the fill")?;
    hook.run("fill-end");
    let fill_after = scrape(&fleet).context("the fill's last scrape of the server's /metrics")?;
    let mut http = Http::new(fleet.port, &fleet.admin);

    // The MCP tools agents call, where this build has them.
    let tools: Vec<String> = {
        let a = http.send_settled("POST", "/api/mcp", Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}).to_string().as_bytes())).context("the MCP tools' list")?;
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
    let hogs_run = if hogs > 0 { Some(Hogs::start(hogs, place, &format!("kiln-lab-hogs-{}-{index}", std::process::id())).context("the round's hogs")?) } else { None };
    type Window = (Vec<Sample>, Vec<Watch>, Scrape, Scrape, (u64, u64));
    let (samples, watches, before, after, window) = std::thread::scope(|scope| -> anyhow::Result<Window> {
        let s = &shared;
        // However this thread leaves the window, its clients stop, so the scope that waits for them ends.
        let _stops = Stops(&s.stop);
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
        let before = scraper.send("GET", "/metrics", None).context("the window's first scrape of the server's /metrics")?.body;
        keep_scrape(dir, "window-before.prom", &before);
        let before = Scrape::parse(&String::from_utf8_lossy(&before));
        hook.run("window-start");
        let start = now_us();
        let interrupted = crate::rx::interrupted();
        std::thread::sleep(Duration::from_secs(p.window_s));
        let end = now_us();
        m.interrupted_reads = Some(crate::rx::interrupted().saturating_sub(interrupted));
        hook.run("window-end");
        let after = scraper.send("GET", "/metrics", None).context("the window's last scrape of the server's /metrics")?.body;
        keep_scrape(dir, "window-after.prom", &after);
        let after = Scrape::parse(&String::from_utf8_lossy(&after));
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
    if let Some(n) = m.interrupted_reads.filter(|n| *n > 0) {
        eprintln!(
            "kiln-lab: round {index}: {n} of the lab's reads were interrupted in its window: its processes were stopped or frozen (a pacer's cgroup.freeze, SIGSTOP) or signalled, so its latencies include those stops"
        );
    }
    let in_window: Vec<Sample> = samples.into_iter().filter(|s| s.at_us >= window.0 && s.at_us < window.1).collect();
    #[allow(clippy::cast_precision_loss)]
    let secs = (window.1.saturating_sub(window.0)) as f64 / 1e6;
    let seen = measure::seen(&in_window);
    let (mut numbers, shards) = numbers(&seen, &before, &after, secs);
    splits(&mut numbers, &in_window);
    arrivals(&mut numbers, &in_window);
    numbers.insert("fill.s".into(), fill_s);
    server_numbers(&mut numbers, "fill.", &fill_before, &fill_after, fill_s);
    // No number that isn't finite is written: JSON can't say one (serde_json writes a NaN or an infinity as null), and a
    // run that held one couldn't be read back (g9p35's round 5, `bus.delivery_top_ms` past its histogram's last bound).
    numbers.retain(|_, v| v.is_finite());
    let (held, fill_held) = (measure::held(&before, &after, secs), measure::held(&fill_before, &fill_after, fill_s));
    let locks = lock_census((&fill_before, &fill_after), (&before, &after));
    drop(fleet);
    let mut seen: BTreeMap<String, measure::Seen> = seen.into_iter().map(|(c, s)| (c.name().to_owned(), s)).collect();
    seen.extend(measure::seen_by_what(&in_window));
    seen.extend(frames_against_attaches(&watches, window));
    if p.subagents > 0 {
        let subagent_sessions: HashSet<&str> = active.iter().take(p.subagents).map(String::as_str).collect();
        seen.extend(frames_by_subagents(&watches, window, &subagent_sessions));
    }
    let quantum_us = measure::quantum_us(&after);
    Ok(Round { build: build.label.clone(), index, hogs, machine: m, fill_s, shards, numbers, seen, held, fill_held, quantum_us, locks })
}

/// What of a window's histogram `buckets` went past its last finite bound, as `<family>_past_last_bound`, their count (0
/// where none did), and `<family>_top_at_least_ms`, that bound, the least the top reached, where some did: a top past the
/// last bound is no number (`+Inf`, kept out of the round), and the tail it hides is said instead (main's ask, 2026-11-06:
/// g9p35's round 5 had 10 bus deliveries past 5 s calm and 35 with hogs). Nothing for a build without the histogram.
fn put_past(put: &mut impl FnMut(String, Option<f64>), family: &str, buckets: &[(f64, f64)]) {
    if buckets.is_empty() {
        return;
    }
    let past = measure::past_last_bound(buckets);
    put(format!("{family}_past_last_bound"), Some(past.map_or(0.0, |(n, _)| n)));
    put(format!("{family}_top_at_least_ms"), past.map(|(_, bound)| bound * 1000.0));
}

/// A round's numbers as a run.json holds them, those that aren't finite numbers dropped: a run written before the lab kept
/// them out holds a NaN or an infinity as null (serde_json's way; g9p35's round 5, `bus.delivery_top_ms`), which reads
/// back as no number at all, as the lab now writes it.
fn finite_numbers<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, f64>, D::Error> {
    let all: BTreeMap<String, Option<f64>> = BTreeMap::deserialize(d)?;
    Ok(all.into_iter().filter_map(|(k, v)| Some((k, v.filter(|v| v.is_finite())?))).collect())
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

/// The window's frames (stamped in it) told apart by whose session they're of, in a profile with subagent turns
/// (copies-lat2's A2(b)): `websocket.subagents`, those of the sessions working in subagent turns, and
/// `websocket.others`, the rest: whether the other sessions' frames keep their pace beside sessions whose tool events
/// are updated long after they began.
pub fn frames_by_subagents(watches: &[Watch], window: (u64, u64), subagents: &HashSet<&str>) -> BTreeMap<String, measure::Seen> {
    let mut by: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for w in watches {
        let group = if subagents.contains(w.session.as_str()) { "websocket.subagents" } else { "websocket.others" };
        for &(stamp, took) in &w.frames {
            if stamp >= window.0 && stamp < window.1 {
                by.entry(group.to_owned()).or_default().push(took);
            }
        }
    }
    by.into_iter().map(|(k, took)| (k, measure::seen_of(took, 0))).collect()
}

/// The slowest API reads of the window split ([`measure::split`]): those at or past the class's p99 and those at
/// or past its p99.9, each as `split.api_read.<q>.{wait,took,rest}_ms` (the server's wait before its handler, its
/// handler's time, the rest: the hops either way, the transfer, the client's reading; each the mean over them, so
/// the three add up to their mean) and `split.api_read.<q>.n`. Goal 10, plan 01a1a87b2dae step 5.
fn splits(n: &mut BTreeMap<String, f64>, window: &[Sample]) {
    for (q, at) in [("p99", 0.99), ("p999", 0.999)] {
        if let Some(s) = measure::split(window, Class::ApiRead, at) {
            let name = Class::ApiRead.name();
            n.insert(format!("split.{name}.{q}.wait_ms"), s.wait_ms);
            n.insert(format!("split.{name}.{q}.took_ms"), s.took_ms);
            n.insert(format!("split.{name}.{q}.rest_ms"), s.rest_ms);
            #[allow(clippy::cast_precision_loss)]
            n.insert(format!("split.{name}.{q}.n"), s.n as f64);
        }
    }
}

/// The window's API reads and frames split at their arrival on the lab's socket ([`measure::arrivals`]; goal 10, plan
/// 01a1a87b2dae step 5), where the socket stamps arrivals (Linux): `<class>.arrived_{p50,p99,p999}_ms`, to the arrival
/// (the server's part and its way's; an agent's and its runner's too, for a frame), and `<class>.lab_read_{p50,p99,p999}_ms`,
/// from it (the lab's own: its thread's wake onto its CPU, its read and its reading). The class's own figures are
/// the two together, each sample's: what's the lab's is told from what's the server's.
fn arrivals(n: &mut BTreeMap<String, f64>, window: &[Sample]) {
    for class in [Class::ApiRead, Class::Websocket] {
        let Some((to, from)) = measure::arrivals(window, class) else { continue };
        let name = class.name();
        for (part, seen) in [("arrived", to), ("lab_read", from)] {
            for (q, v) in [("p50", seen.p50_ms), ("p99", seen.p99_ms), ("p999", seen.p999_ms)] {
                if let Some(v) = v {
                    n.insert(format!("{name}.{part}_{q}_ms"), v);
                }
            }
        }
    }
}

/// The legs of a runner's message at the server that a round reports (`kiln_runner_wait_leg_seconds`), each by
/// its `leg` label: goal 10, plan 01a1a87b2dae step 4's waits, and step 5's `held`, the hub's loop's time with
/// each event it took; and step 5's `runner` and `server`, of each agent session's update whose runner said how
/// long it held it: that hold, on the runner's clock, and the server's time from its frame's arrival to its owner
/// done with it, on the server's. Set against a frame's time (`websocket`), what's past both is the wire's. And step
/// 4's `order`, with a loop a shard: a frame's wait for its runner's earlier ones on the hub's other loops, of the frames
/// that had any (its count against `to_loop`'s is how often); `to_control`, `to_loop` of the control loop's frames
/// alone, which tells the session loops' wait from it; and `net`, the net thread's hold of a frame before it handed it
/// to its loop, which tells that thread's part of `to_loop` from the queue's and the loop's wake.
const RUNNER_LEGS: [&str; 9] = ["to_loop", "in_loop", "to_owner", "held", "runner", "server", "order", "to_control", "net"];

/// The legs of an agent session's live frame at the server past its runner's wait that a round reports
/// (`kiln_frame_leg_seconds`; goal 10, plan 01a1a87b2dae step 5), each by its `leg` label: `apply`, its session's owner's
/// time with the runner's update, its frames published; `woken`, a frame's wait from its publish for its viewer's task to
/// take it up where the viewer runs on its session's owner's shard, and `woken_other` where it runs on another (a wake
/// across threads); and `write`, the viewer's write of its batch to its socket. Set against `server.runner.server` (the
/// runner's update's arrival to its owner done) and a frame's whole time (`websocket`), they tell where a frame's time
/// past its runner goes.
const FRAME_LEGS: [&str; 4] = ["apply", "woken", "woken_other", "write"];

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
    // What the allocator holds by kind, and its slack (`active` less `allocated`: room left in its pages and slabs),
    // where the build says them (`kiln_memory_allocator_bytes`, jemalloc's statistics; goal 11, plan 01a1a87b2e56
    // step 5): each arm of the A/B of jemalloc's settings moves one of them, and what A holds of each says which arms
    // can move anonymous memory beyond noise (docs/runtime-lab.md).
    let allocator = |kind: &str| after.get("kiln_memory_allocator_bytes", &[("kind", kind)]);
    for kind in after.values_of("kiln_memory_allocator_bytes", "kind") {
        if let Some(b) = allocator(&kind) {
            n.insert(format!("server.allocator.{kind}_mb"), b / 1e6);
        }
    }
    if let (Some(active), Some(allocated)) = (allocator("active"), allocator("allocated")) {
        n.insert("server.allocator.slack_mb".into(), (active - allocated) / 1e6);
    }
    let shards = server_numbers(&mut n, "", before, after, secs);
    per_request_wakes(&mut n, before, after, requests);
    per_request_pools(&mut n, before, after, requests);
    per_request_memory(&mut n, before, after, requests);
    // The routes': how long each class took and waited at the server, at p99 and at p99.9 (the bar's: goal 10,
    // plan 01a1a87b2dae step 5, so a class's tail can be set against its legs at the server).
    for class in ["page", "api_read", "api_write", "websocket", "runner"] {
        let want = [("class", class)];
        for (family, key) in [("kiln_route_seconds", "took"), ("kiln_route_wait_seconds", "wait")] {
            let d = measure::delta(&before.buckets(family, &want), &after.buckets(family, &want));
            for (q, at) in [("p99", 0.99), ("p999", 0.999)] {
                if let Some(v) = measure::histogram_quantile(at, &d) {
                    n.insert(format!("server.{class}.{key}_{q}_ms"), v * 1000.0);
                }
            }
        }
    }
    // A runner's message's wait by its legs, to the microsecond, where the build has them
    // (`kiln_runner_wait_leg_seconds`; goal 10, plan 01a1a87b2dae step 4): to the hub's loop taking its frame
    // (every frame's), that loop handing an agent session's update to the inbox of the shard that owns its
    // copy, and that owner taking it; and how long the loop held each event it took, its waits included (step 5:
    // set against `to_loop`, it tells the loop held by its events from the loop waiting for its shard); and of the
    // updates whose runner said how long it held them, that hold and the server's time with them (step 5).
    for leg in RUNNER_LEGS {
        let d = measure::delta(&before.buckets("kiln_runner_wait_leg_seconds", &[("leg", leg)]), &after.buckets("kiln_runner_wait_leg_seconds", &[("leg", leg)]));
        for (q, at) in [("p50", 0.5), ("p99", 0.99), ("p999", 0.999)] {
            if let Some(v) = measure::histogram_quantile(at, &d) {
                n.insert(format!("server.runner.{leg}_{q}_us"), v * 1e6);
            }
        }
    }
    // A live frame's legs past its runner's wait, to the microsecond, where the build has them (`kiln_frame_leg_seconds`;
    // goal 10, plan 01a1a87b2dae step 5): its owner's apply, its viewer's wake on the owner's shard and across shards,
    // and the write; with how many frames took each wake, so a run says how often a viewer was woken across threads.
    for leg in FRAME_LEGS {
        let d = measure::delta(&before.buckets("kiln_frame_leg_seconds", &[("leg", leg)]), &after.buckets("kiln_frame_leg_seconds", &[("leg", leg)]));
        for (q, at) in [("p50", 0.5), ("p99", 0.99), ("p999", 0.999)] {
            if let Some(v) = measure::histogram_quantile(at, &d) {
                n.insert(format!("server.frame.{leg}_{q}_us"), v * 1e6);
            }
        }
    }
    // A histogram's count is its `+Inf` bucket's, the last.
    let woken = |leg: &str| measure::delta(&before.buckets("kiln_frame_leg_seconds", &[("leg", leg)]), &after.buckets("kiln_frame_leg_seconds", &[("leg", leg)])).last().map(|(_, count)| *count);
    if let (Some(own), Some(other)) = (woken("woken"), woken("woken_other"))
        && own + other > 0.0
    {
        n.insert("server.frame.woken_other_share".into(), other / (own + other));
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
    // A number that isn't finite is none: a histogram's top past its last bound is `+Inf` (`measure::histogram_top`), which
    // JSON can't say (g9p35's round 5: a bus delivery past `kiln_bus_delivery_seconds`'s last bound, its
    // `bus.delivery_top_ms` written as null, and the run then unreadable).
    let mut put = |name: String, v: Option<f64>| {
        if let Some(v) = v.filter(|v| v.is_finite()) {
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
        // The same samples apart by where each came from, all shards together, where the build says it (goal 9, plan
        // 01a1a87b2cec step 5's quiet probe, `kiln_shard_delay_by_seconds`): `shards.by_work.*`, the shards' wakes by
        // their work and the probe's turn on a shard awake; `shards.by_probe.*`, the probe's own timer waking a shard
        // idle a whole period, an idle shard's delay alone. A build without the family samples the probe's timer every
        // period (`shards.delay_*` alone): another instrument.
        for by in ["work", "probe"] {
            let mut d: Vec<(f64, f64)> = Vec::new();
            for sh in &shards {
                let one = histogram("kiln_shard_delay_by_seconds", &[("shard", sh.as_str()), ("by", by)]);
                if d.is_empty() {
                    d = one;
                } else {
                    for ((_, total), (_, add)) in d.iter_mut().zip(one) {
                        *total += add;
                    }
                }
            }
            delay_us(&mut put, &format!("shards.by_{by}."), &d);
        }
        put("shards.worst_delay_p999_us".into(), worst_p999);
        put("shards.worst_busy_share".into(), worst_busy);
        #[allow(clippy::cast_precision_loss)]
        let count = shards.len() as f64;
        put("shards.busy_share".into(), Some(busy / count));
        put("shards.slow_polls_per_s".into(), Some(per_s(slow.iter().map(|(_, v)| v).sum())));
        for (cause, v) in slow {
            put(format!("shards.slow_polls_{cause}_per_s"), Some(per_s(v)));
        }
        // Their nudge (kiln's goal 8 step 7 and goal 6 step 3, `crate::nudge`), where the build says it: the share of the
        // window each shard's waits were nudged, and, all shards together, how long their host held each wake another
        // thread or their I/O made them, as each shard judges it (`held_wake_us`).
        let mut nudged = None::<f64>;
        let mut held = HeldWakes::default();
        for sh in &shards {
            let want = [("shard", sh.as_str())];
            let share = diff("kiln_shard_nudged_seconds_total", &want).map(per_s);
            put(format!("shard{sh}.nudged_share"), share);
            if let Some(share) = share {
                nudged = Some(nudged.unwrap_or(0.0) + share / count);
            }
            let wakes = |by: &str| diff("kiln_shard_wakeups_total", &[("shard", sh.as_str()), ("by", by)]);
            held.add(diff("kiln_shard_run_wait_seconds_total", &want), wakes("kick"), wakes("io"), wakes("timer"));
        }
        put("shards.nudged_share".into(), nudged);
        put("shards.held_wake_us".into(), held.per_wake_us());
        // How long their parks lasted, all shards together (goal 9, plan 01a1a87b2cec step 5), where the build says it:
        // the median and p99, and the share of parks under 16 µs, the first of hyper-rt's bounds (`PARK_BOUNDS_NS`) past
        // the cheapest wake measured on fedora (a socket's on the waiting thread's own CPU, 12.6 µs at its median:
        // the wake-2 lab): a spin as long would have served their work sooner than their wakes did.
        let mut parks: Vec<(f64, f64)> = Vec::new();
        for sh in &shards {
            let d = histogram("kiln_shard_park_seconds", &[("shard", sh.as_str())]);
            if parks.is_empty() {
                parks = d;
            } else {
                for ((_, total), (_, add)) in parks.iter_mut().zip(d) {
                    *total += add;
                }
            }
        }
        put("shards.park_p50_us".into(), measure::histogram_quantile(0.5, &parks).map(|v| v * 1e6));
        put("shards.park_p99_us".into(), measure::histogram_quantile(0.99, &parks).map(|v| v * 1e6));
        let short = parks.iter().find(|(bound, _)| (*bound - 16e-6).abs() < 1e-12).map(|(_, n)| *n);
        put("shards.parks_under_16us_share".into(), short.zip(parks.last().map(|(_, n)| *n)).filter(|(_, all)| *all > 0.0).map(|(n, all)| n / all));
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
    put_past(&mut put, "bus.delivery", &delivery);
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
        put_past(&mut put, "compute.piece", &piece);
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

/// What a wake by a shard's own timer makes it wait for its CPU on a calm machine, at the most, seconds: kiln's
/// `crate::nudge::TIMER_WAKE_NS` (the wake-2 lab, no hog: 0.4 to 0.7 µs a run).
const TIMER_WAKE_S: f64 = 0.7e-6;

/// The shards' waits for a CPU over a window and the wakes they took, summed (kiln's goal 8 step 7, `crate::nudge`):
/// how long their host held each wake another thread or their I/O made them, as each shard judges its own periods,
/// over the whole window: their run wait past [`TIMER_WAKE_S`] for each wake of their own timers, over their wakes by
/// a kick or I/O.
#[derive(Debug, Default)]
struct HeldWakes {
    waited_s: f64,
    others: f64,
    seen: bool,
}

impl HeldWakes {
    /// One shard's growth over the window: its run wait (seconds), and its wakes by a kick, by I/O and by its timers.
    fn add(&mut self, waited_s: Option<f64>, kick: Option<f64>, io: Option<f64>, timer: Option<f64>) {
        let (Some(waited_s), Some(kick), Some(io), Some(timer)) = (waited_s, kick, io, timer) else { return };
        self.seen = true;
        self.waited_s += (waited_s - timer * TIMER_WAKE_S).max(0.0);
        self.others += kick + io;
    }

    /// Microseconds a wake, where any shard said its wait and any took a wake by another thread or I/O.
    fn per_wake_us(&self) -> Option<f64> {
        (self.seen && self.others > 0.0).then(|| self.waited_s / self.others * 1e6)
    }
}

/// What each of the window's `requests` cost the shards in loop steps, polls, parks, wakes and system calls (goal
/// 9, plan 01a1a87b2cec step 5, beside `server.cpu_ms_per_request`): each family's growth over the window summed
/// over the shards, over the requests, as `shards.<what>_per_request`, where the build has the family; and, where
/// it has every one, the calls' sum (`shards.syscalls_per_request`). A park that waits is a system call and a
/// sleep, and a kick a system call on its sender; a park withdrawn costs neither.
fn per_request_wakes(n: &mut BTreeMap<String, f64>, before: &Scrape, after: &Scrape, requests: u64) {
    const FAMILIES: [(&str, &str, &[(&str, &str)]); 23] = [
        ("steps", "kiln_shard_steps_total", &[]),
        ("polls", "kiln_shard_polls_total", &[]),
        ("parks", "kiln_shard_parks_total", &[]),
        ("parks_withdrawn", "kiln_shard_parks_withdrawn_total", &[]),
        ("wakeups_kick", "kiln_shard_wakeups_total", &[("by", "kick")]),
        ("wakeups_io", "kiln_shard_wakeups_total", &[("by", "io")]),
        ("wakeups_timer", "kiln_shard_wakeups_total", &[("by", "timer")]),
        ("wakes_taken", "kiln_shard_wakes_taken_total", &[]),
        ("wakes_sent_own", "kiln_shard_wakes_sent_total", &[("to", "own")]),
        ("wakes_sent_other", "kiln_shard_wakes_sent_total", &[("to", "other")]),
        ("kicks_sent", "kiln_shard_kicks_sent_total", &[]),
        ("kicks_saved", "kiln_shard_kicks_saved_total", &[]),
        ("threads_woken", "kiln_shard_threads_woken_total", &[]),
        ("harvests", "kiln_shard_harvests_total", &[]),
        ("arms", "kiln_shard_arms_total", &[]),
        ("socket_accepts", "kiln_shard_socket_calls_total", &[("call", "accept")]),
        ("socket_reads", "kiln_shard_socket_calls_total", &[("call", "read")]),
        ("socket_writes", "kiln_shard_socket_calls_total", &[("call", "write")]),
        ("socket_reads_empty", "kiln_shard_socket_calls_idle_total", &[("call", "read")]),
        ("socket_writes_full", "kiln_shard_socket_calls_idle_total", &[("call", "write")]),
        // The spin before a park, where the server was asked for one (goal 9, plan 01a1a87b2cec step 4): its spins,
        // those that found work (a park, its wake and a kick saved each), and the seconds they spun, the CPU it cost.
        ("spins", "kiln_shard_spins_total", &[]),
        ("spin_hits", "kiln_shard_spin_hits_total", &[]),
        ("spin_seconds", "kiln_shard_spin_seconds_total", &[]),
    ];
    // Each of these a system call on a shard: their sum is what a request costs the shards in calls into the
    // kernel, as counted (docs/runtime-lab.md says what isn't).
    const CALLS: [&str; 8] = ["parks", "kicks_sent", "threads_woken", "harvests", "arms", "socket_accepts", "socket_reads", "socket_writes"];
    if requests == 0 {
        return;
    }
    #[allow(clippy::cast_precision_loss)]
    let requests = requests as f64;
    let shards = after.values_of("kiln_shard_polls_total", "shard");
    for (what, family, labels) in FAMILIES {
        let mut grown = None::<f64>;
        for sh in &shards {
            let mut want = vec![("shard", sh.as_str())];
            want.extend_from_slice(labels);
            if let (Some(a), Some(b)) = (after.get(family, &want), before.get(family, &want)) {
                *grown.get_or_insert(0.0) += a - b;
            }
        }
        if let Some(grown) = grown {
            n.insert(format!("shards.{what}_per_request"), grown / requests);
        }
    }
    let calls: Option<Vec<f64>> = CALLS.iter().map(|what| n.get(&format!("shards.{what}_per_request")).copied()).collect();
    if let Some(calls) = calls {
        n.insert("shards.syscalls_per_request".into(), calls.iter().sum());
    }
}

/// What each of the window's `requests` handed the pools (goal 9, plan 01a1a87b2cec step 5: each hand-off a wake
/// of a pool's thread and a wake back, beside `shards.*_per_request`): the compute pool's jobs, the workers its
/// jobs woke (each a system call on the thread that queued the job) and the times they went to park
/// (`compute.<what>_per_request`), and each of the blocking pool's shares' jobs, kiln.db's writer's among them
/// (`blocking.<share>.per_request`), where the build has their series.
fn per_request_pools(n: &mut BTreeMap<String, f64>, before: &Scrape, after: &Scrape, requests: u64) {
    if requests == 0 {
        return;
    }
    #[allow(clippy::cast_precision_loss)]
    let requests = requests as f64;
    let grown = |family: &str, want: &[(&str, &str)]| Some(after.get(family, want)? - before.get(family, want)?);
    for (what, family) in [("jobs", "kiln_compute_jobs_total"), ("unparks", "kiln_compute_unparks_total"), ("parks", "kiln_compute_parks_total")] {
        if let Some(v) = grown(family, &[]) {
            n.insert(format!("compute.{what}_per_request"), v / requests);
        }
    }
    for share in after.values_of("kiln_blocking_run_seconds_count", "share") {
        if let Some(v) = grown("kiln_blocking_run_seconds_count", &[("share", share.as_str())]) {
            n.insert(format!("blocking.{share}.per_request"), v / requests);
        }
    }
}

/// What each of the window's `requests` cost the server's memory in work (allocs-metric, beside
/// `server.cpu_ms_per_request`): the allocations its allocator served (`kiln_memory_allocations_total`, jemalloc's
/// count) as `server.allocations_per_request`, and the page faults it took by kind (`kiln_memory_page_faults_total`)
/// as `server.<kind>_faults_per_request`, where the build has their series.
fn per_request_memory(n: &mut BTreeMap<String, f64>, before: &Scrape, after: &Scrape, requests: u64) {
    if requests == 0 {
        return;
    }
    #[allow(clippy::cast_precision_loss)]
    let requests = requests as f64;
    let grown = |family: &str, want: &[(&str, &str)]| Some(after.get(family, want)? - before.get(family, want)?);
    if let Some(v) = grown("kiln_memory_allocations_total", &[]) {
        n.insert("server.allocations_per_request".into(), v / requests);
    }
    for kind in after.values_of("kiln_memory_page_faults_total", "kind") {
        if let Some(v) = grown("kiln_memory_page_faults_total", &[("kind", kind.as_str())]) {
            n.insert(format!("server.{kind}_faults_per_request"), v / requests);
        }
    }
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

    /// Goal 1, plan 01a1a59c2a68 step 3 (goal 9's lab g9p21, rounds 3 and 5 calm and 5 hogs): a window whose own thread
    /// fails before it says stop (its last scrape of /metrics, there) still stops its clients, and its scope ends with
    /// the failure rather than waiting on them while they keep loading the server. (Without [`Stops`], these clients
    /// give up by themselves after a minute, so the test fails rather than hangs.)
    #[test]
    fn g1_s3_a_window_that_fails_stops_its_clients() {
        let stop = AtomicBool::new(false);
        let gave_up = Duration::from_secs(60);
        let started = Instant::now();
        let (stopped, ended) = (AtomicU64::new(0), AtomicU64::new(0));
        let r: anyhow::Result<()> = std::thread::scope(|scope| {
            let _stops = Stops(&stop);
            for _ in 0..4 {
                scope.spawn(|| {
                    let t = Instant::now();
                    while !stop.load(Ordering::Relaxed) && t.elapsed() < gave_up {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    let to = if stop.load(Ordering::Relaxed) { &stopped } else { &ended };
                    to.fetch_add(1, Ordering::Relaxed);
                });
            }
            Err(anyhow::anyhow!("the connection was closed")).context("the window's last scrape of the server's /metrics")
        });
        let took = started.elapsed();
        let e = r.err().map(|e| format!("{e:#}"));
        assert_eq!(e.as_deref(), Some("the window's last scrape of the server's /metrics: the connection was closed"), "the window says why it failed");
        assert_eq!((stopped.load(Ordering::Relaxed), ended.load(Ordering::Relaxed)), (4, 0), "every client was told to stop");
        assert!(took < gave_up / 2, "the window ended in {took:?}");
    }

    /// Goal 1, plan 01a1a59c2a68 step 3: the lab waits for a hook only so long: one still running then is killed with
    /// what it started, reaped and said, and the round goes on; one that ends is waited for and its status said.
    #[cfg(unix)]
    #[test]
    fn g1_s3_a_hook_is_waited_for_only_so_long() -> anyhow::Result<()> {
        let mut long = Command::new("sh");
        long.args(["-c", "sleep 30 & sleep 30"]).stdin(Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut long, 0);
        let started = Instant::now();
        let status = waited(long.spawn()?, Duration::from_millis(300))?;
        let took = started.elapsed();
        anyhow::ensure!(status.is_none(), "{status:?}");
        anyhow::ensure!(took < Duration::from_secs(15), "a hook past its time was waited for {took:?}");
        let status = waited(Command::new("true").spawn()?, Duration::from_secs(30))?;
        anyhow::ensure!(status.is_some_and(|s| s.success()), "{status:?}");
        Ok(())
    }

    /// Goal 1, plan 01a1a59c2a68 step 3: a round whose window had the lab's reads interrupted (its processes stopped or
    /// frozen: g9p21's, by fedora's runner's headroom) says so in its report; one without says nothing of it.
    #[test]
    fn g1_s3_a_round_whose_lab_was_stopped_says_so() {
        let round = |interrupted: Option<u64>| Round {
            build: "build".into(),
            index: 1,
            hogs: 0,
            machine: Machine { interrupted_reads: interrupted, ..Machine::default() },
            fill_s: 1.0,
            shards: 1,
            numbers: BTreeMap::new(),
            seen: BTreeMap::new(),
            held: Vec::new(),
            fill_held: Vec::new(),
            quantum_us: None,
            locks: BTreeMap::new(),
        };
        let run = |rounds: Vec<Round>| crate::report::Run {
            command: "kiln-lab run".into(),
            started: "2026-11-06T14:07:00Z".into(),
            profile: Profile::small(),
            hogs: 0,
            layout: "production".into(),
            server_cpus: None,
            rounds,
            failures: Vec::new(),
        };
        let paced = crate::report::markdown(&run(vec![round(Some(7))]));
        assert!(paced.contains("**the lab was stopped or frozen in its window** (7 of its reads interrupted)"), "{paced}");
        for calm in [Some(0), None] {
            let md = crate::report::markdown(&run(vec![round(calm)]));
            assert!(!md.contains("stopped or frozen"), "{md}");
        }
    }

    /// Goal 8, plan 01a1a59c2b29 step 7 (and goal 6 step 3): each shard's nudged share of the window, their mean, and how
    /// long their host held each wake another thread or I/O made them over the window, its own timers' calm wait set
    /// aside; none where the build says none of it.
    #[test]
    fn g8_s7_the_shards_nudge_and_held_wakes_are_reported() {
        // Shard 0 nudged 30 s of the 60 s window, its run wait 15 ms over 100 kicks, 50 I/O wakes and 6,000 timer
        // wakes; shard 1 never nudged and waited 0.3 ms over 150 wakes by others and 100 by its timers.
        let scrape = |at: f64| {
            let mut t = String::new();
            for (sh, nudged, waited, kick, io, timer) in [(0, 30.0, 0.015, 100.0, 50.0, 6000.0), (1, 0.0, 0.0003, 100.0, 50.0, 100.0)] {
                t.push_str(&format!("kiln_shard_polls_total{{shard=\"{sh}\"}} {}\n", at * 1000.0));
                t.push_str(&format!("kiln_shard_nudged_seconds_total{{shard=\"{sh}\"}} {}\n", at * nudged));
                t.push_str(&format!("kiln_shard_run_wait_seconds_total{{shard=\"{sh}\"}} {}\n", at * waited));
                for (by, n) in [("kick", kick), ("io", io), ("timer", timer)] {
                    t.push_str(&format!("kiln_shard_wakeups_total{{shard=\"{sh}\",by=\"{by}\"}} {}\n", at * n));
                }
            }
            measure::Scrape::parse(&t)
        };
        let mut n = BTreeMap::new();
        server_numbers(&mut n, "", &scrape(0.0), &scrape(1.0), 60.0);
        let near = |name: &str, want: f64| n.get(name).is_some_and(|v| (v - want).abs() < 1e-9);
        assert!(near("shard0.nudged_share", 0.5) && near("shard1.nudged_share", 0.0) && near("shards.nudged_share", 0.25), "{n:?}");
        // (15 ms − 6,000 × 0.7 µs) + (0.3 ms − 100 × 0.7 µs) over 300 wakes by others: 36.8 µs a wake.
        assert!(near("shards.held_wake_us", (0.015 - 6000.0 * 0.7e-6 + 0.0003 - 100.0 * 0.7e-6) / 300.0 * 1e6), "{n:?}");
        let mut none = BTreeMap::new();
        server_numbers(&mut none, "", &measure::Scrape::parse("kiln_shard_polls_total{shard=\"0\"} 0\n"), &measure::Scrape::parse("kiln_shard_polls_total{shard=\"0\"} 9\n"), 60.0);
        assert!(!none.contains_key("shards.nudged_share") && !none.contains_key("shards.held_wake_us"), "{none:?}");
    }

    /// Goal 9, plan 01a1a87b2cec step 5's quiet probe: the shards' delay samples apart by where each came from, all
    /// shards together (`shards.by_work.*`, `shards.by_probe.*`) beside both together (`shards.delay_*`); none apart
    /// where the build has no `kiln_shard_delay_by_seconds` (the probe's timer every period: another instrument).
    #[test]
    fn g9_s5_the_shards_delay_is_reported_by_where_its_samples_came_from() {
        // Over the window: shard 0's work 900 samples under 100 µs and 100 under 1 ms, its probe's timer 10 under 250 µs;
        // shard 1's work 1,000 under 100 µs, no probe's.
        let scrape = |at: f64| {
            let mut t = String::new();
            for (sh, by, under_100us, under_250us, under_1ms) in [(0, "work", 900.0, 900.0, 1000.0), (0, "probe", 0.0, 10.0, 10.0), (1, "work", 1000.0, 1000.0, 1000.0), (1, "probe", 0.0, 0.0, 0.0)] {
                t.push_str(&format!("kiln_shard_polls_total{{shard=\"{sh}\"}} {}\n", at * 1000.0));
                for (le, n) in [("0.0001", under_100us), ("0.00025", under_250us), ("0.001", under_1ms), ("+Inf", under_1ms)] {
                    t.push_str(&format!("kiln_shard_delay_by_seconds_bucket{{shard=\"{sh}\",by=\"{by}\",le=\"{le}\"}} {}\n", at * n));
                }
            }
            measure::Scrape::parse(&t)
        };
        let mut n = BTreeMap::new();
        server_numbers(&mut n, "", &scrape(0.0), &scrape(1.0), 60.0);
        let got = |name: &str| n.get(name).copied();
        // 2,000 work samples, 1,900 under 100 µs: p99.9 at the 1,998th, in the bucket to 1 ms; the probe's 10, under 250 µs.
        assert!(got("shards.by_work.delay_p999_us").is_some_and(|v| v > 250.0 && v <= 1000.0), "{n:?}");
        assert!(got("shards.by_work.delay_p50_us").is_some_and(|v| v <= 100.0), "{n:?}");
        assert!(got("shards.by_probe.delay_p999_us").is_some_and(|v| v > 100.0 && v <= 250.0), "{n:?}");
        let mut none = BTreeMap::new();
        server_numbers(&mut none, "", &measure::Scrape::parse("kiln_shard_polls_total{shard=\"0\"} 0\n"), &measure::Scrape::parse("kiln_shard_polls_total{shard=\"0\"} 9\n"), 60.0);
        assert!(!none.keys().any(|k| k.starts_with("shards.by_")), "{none:?}");
    }

    /// copies-lat2's A2(b) (main's ask, 2026-11-06): the subagents profile is the realistic one with two of its active
    /// sessions in subagent turns of more than 70 events, and the window's frames are told apart by whose session.
    #[test]
    fn a2b_the_subagents_profile_tells_the_other_sessions_frames_apart() {
        let (sub, realistic) = (Profile::subagents(), Profile::realistic());
        assert_eq!((sub.subagents, sub.active, realistic.subagents), (2, realistic.active, 0));
        assert!(sub.subagent_events > 70, "{sub:?}");
        assert_eq!(Profile::named("subagents").map(|p| p.subagents).ok(), Some(2));
        let watch = |session: &str, frames: Vec<(u64, u64)>| Watch { viewer: 0, session: session.to_owned(), from_us: 0, to_us: 0, frames };
        let watches = vec![watch("s0", vec![(10, 500), (2_000, 9)]), watch("s5", vec![(20, 100), (30, 300)])];
        let subagents: HashSet<&str> = std::iter::once("s0").collect();
        let seen = frames_by_subagents(&watches, (0, 1_000), &subagents);
        assert_eq!(seen.get("websocket.subagents").map(|s| s.n), Some(1), "outside the window isn't counted");
        assert_eq!(seen.get("websocket.others").map(|s| s.n), Some(2));
    }

    /// Goal 6, plan 01a1a59c2abe step 4: the flood profile is the overload one with many more clients, none thinking.
    #[test]
    fn g6_s4_flood_is_overload_with_many_more_clients() {
        let (flood, overload) = (Profile::flood(), Profile::overload());
        assert!(flood.readers >= 16 * overload.readers && flood.mcp > overload.mcp, "{flood:?}");
        assert_eq!((flood.read_think_ms, flood.mcp_think_ms, flood.dwell_s), (0, 0, 0));
        assert_eq!((flood.sessions.len(), flood.viewers, flood.window_s), (overload.sessions.len(), overload.viewers, overload.window_s));
    }

    /// The lab writes no number that isn't finite (main's ask, 2026-11-06). In g9p35's round 5 a bus delivery took longer
    /// than `kiln_bus_delivery_seconds`'s last bound, so `bus.delivery_top_ms`, the histogram's top, was `+Inf`, which
    /// serde_json writes as null, and kiln-lab's report and table then refused the run (`invalid type: null, expected
    /// f64`). Now such a number is absent (the quantiles, finite, stay), and a run.json written before, its null in it,
    /// reads back without it.
    #[test]
    fn a_number_past_a_histograms_last_bound_is_absent_and_an_old_null_reads_back() {
        let scrape = |a: u32, b: u32, c: u32| {
            measure::Scrape::parse(&format!(
                "kiln_bus_delivery_seconds_bucket{{le=\"0.001\"}} {a}\nkiln_bus_delivery_seconds_bucket{{le=\"1\"}} {b}\nkiln_bus_delivery_seconds_bucket{{le=\"+Inf\"}} {c}\n"
            ))
        };
        let mut n = BTreeMap::new();
        server_numbers(&mut n, "", &scrape(0, 0, 0), &scrape(5, 9, 10), 60.0);
        assert!(n.values().all(|v| v.is_finite()), "{n:?}");
        assert_eq!(n.get("bus.delivery_top_ms"), None, "past the last bound: {n:?}");
        assert_eq!(n.get("bus.delivery_p99_ms"), Some(&1000.0), "{n:?}");
        let round = Round {
            build: "b".to_owned(),
            index: 5,
            hogs: 0,
            machine: Machine::default(),
            fill_s: 1.0,
            shards: 4,
            numbers: BTreeMap::from([("server.rss_mb".to_owned(), 296.0)]),
            seen: BTreeMap::new(),
            held: Vec::new(),
            fill_held: Vec::new(),
            quantum_us: None,
            locks: BTreeMap::new(),
        };
        let mut written = serde_json::to_value(&round).unwrap_or_default();
        if let Some(numbers) = written.get_mut("numbers").and_then(Value::as_object_mut) {
            numbers.insert("bus.delivery_top_ms".to_owned(), Value::Null);
        }
        let back = serde_json::from_value::<Round>(written).map_err(|e| e.to_string());
        assert_eq!(back.as_ref().map(|r| r.numbers.clone()), Ok(BTreeMap::from([("server.rss_mb".to_owned(), 296.0)])), "an old null reads back as no number");
    }

    /// main's ask (2026-11-06): a bus delivery past the histogram's last bound is said by how many went past it and the
    /// bound, the least the top reached, where the top itself is no number; with none past, the count is 0 and the top
    /// stands.
    #[test]
    fn a_bus_deliverys_top_past_the_last_bound_is_said_by_how_many_and_the_bound() {
        let scrape = |a: u32, b: u32, c: u32| {
            measure::Scrape::parse(&format!(
                "kiln_bus_delivery_seconds_bucket{{le=\"0.001\"}} {a}\nkiln_bus_delivery_seconds_bucket{{le=\"5\"}} {b}\nkiln_bus_delivery_seconds_bucket{{le=\"+Inf\"}} {c}\n"
            ))
        };
        let mut n = BTreeMap::new();
        server_numbers(&mut n, "", &scrape(0, 0, 0), &scrape(4_000, 4_524, 4_534), 60.0);
        assert_eq!((n.get("bus.delivery_past_last_bound"), n.get("bus.delivery_top_at_least_ms"), n.get("bus.delivery_top_ms")), (Some(&10.0), Some(&5000.0), None), "{n:?}");
        let mut none = BTreeMap::new();
        server_numbers(&mut none, "fill.", &scrape(0, 0, 0), &scrape(10, 12, 12), 60.0);
        assert_eq!((none.get("fill.bus.delivery_past_last_bound"), none.get("fill.bus.delivery_top_at_least_ms"), none.get("fill.bus.delivery_top_ms")), (Some(&0.0), None, Some(&5000.0)), "{none:?}");
    }

    /// main's ask (2026-11-06): the compute pool's piece past its histogram's last bound is said as the bus's delivery is.
    #[test]
    fn a_compute_pieces_top_past_the_last_bound_is_said_too() {
        let scrape = |jobs: u32, a: u32, c: u32| {
            measure::Scrape::parse(&format!(
                "kiln_compute_jobs_total {jobs}\nkiln_compute_piece_seconds_bucket{{le=\"0.01\"}} {a}\nkiln_compute_piece_seconds_bucket{{le=\"+Inf\"}} {c}\n"
            ))
        };
        let mut n = BTreeMap::new();
        server_numbers(&mut n, "", &scrape(0, 0, 0), &scrape(7, 5, 7), 60.0);
        assert_eq!((n.get("compute.piece_past_last_bound"), n.get("compute.piece_top_at_least_ms"), n.get("compute.piece_top_ms")), (Some(&2.0), Some(&10.0), None), "{n:?}");
        assert!(n.values().all(|v| v.is_finite()), "{n:?}");
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a runner's wait is reported by its legs, each to the microsecond, from
    /// the server's histogram of it between the window's two scrapes.
    #[test]
    fn g10_s4_a_runners_wait_is_reported_by_its_legs() {
        let legs = RUNNER_LEGS;
        assert_eq!(legs, ["to_loop", "in_loop", "to_owner", "held", "runner", "server", "order", "to_control", "net"], "goal 10 step 5: the loop's hold of each event, the runner's hold and the server's time are reported beside the legs; step 4: a frame's wait for its runner's earlier ones on the other loops, and the control loop's own wait");
        let scrape = |n: f64| {
            let mut t = String::new();
            for leg in legs {
                // Half the leg's observations at or under 0.5 ms, all of them at or under 1 ms.
                for (le, c) in [("0.0005", n / 2.0), ("0.001", n), ("+Inf", n)] {
                    t.push_str(&format!("kiln_runner_wait_leg_seconds_bucket{{leg=\"{leg}\",le=\"{le}\"}} {c}\n"));
                }
            }
            measure::Scrape::parse(&t)
        };
        let (n, _) = numbers(&BTreeMap::new(), &scrape(0.0), &scrape(1000.0), 60.0);
        for leg in legs {
            let p50 = n.get(&format!("server.runner.{leg}_p50_us")).copied();
            let p99 = n.get(&format!("server.runner.{leg}_p99_us")).copied();
            assert!(p50.is_some_and(|v| (v - 500.0).abs() < 1.0), "{leg}: p50 {p50:?}");
            assert!(p99.is_some_and(|v| (v - 990.0).abs() < 1.0), "{leg}: p99 {p99:?}");
        }
        let (n, _) = numbers(&BTreeMap::new(), &scrape(0.0), &scrape(0.0), 60.0);
        assert!(!n.keys().any(|k| k.contains("to_owner")), "a build without the legs reports none");
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a live frame's legs past its runner's wait are reported each to the
    /// microsecond, from the server's histogram between the window's two scrapes, with the share of the frames whose
    /// viewer was woken across threads; a build without them reports none.
    #[test]
    fn g10_s5_a_frames_legs_past_its_runner_are_reported() {
        assert_eq!(FRAME_LEGS, ["apply", "woken", "woken_other", "write"], "the server's `crate::route_latency::FrameLeg`, by name");
        // `woken_other` took three frames in four.
        let scrape = |n: f64| {
            let mut t = String::new();
            for leg in FRAME_LEGS {
                let k = if leg == "woken_other" { 3.0 } else { 1.0 };
                // Half the leg's observations at or under 0.1 ms, all of them at or under 0.2 ms.
                for (le, c) in [("0.0001", k * n / 2.0), ("0.0002", k * n), ("+Inf", k * n)] {
                    t.push_str(&format!("kiln_frame_leg_seconds_bucket{{leg=\"{leg}\",le=\"{le}\"}} {c}\n"));
                }
            }
            measure::Scrape::parse(&t)
        };
        let (n, _) = numbers(&BTreeMap::new(), &scrape(0.0), &scrape(1000.0), 60.0);
        for leg in FRAME_LEGS {
            let p50 = n.get(&format!("server.frame.{leg}_p50_us")).copied();
            let p99 = n.get(&format!("server.frame.{leg}_p99_us")).copied();
            assert!(p50.is_some_and(|v| (v - 100.0).abs() < 1.0), "{leg}: p50 {p50:?}");
            assert!(p99.is_some_and(|v| (v - 198.0).abs() < 1.0), "{leg}: p99 {p99:?}");
        }
        assert_eq!(n.get("server.frame.woken_other_share").copied(), Some(0.75));
        assert_eq!(crate::lab::better("server.frame.woken_other_share"), crate::stats::Better::Lower, "fewer wakes across threads is better");
        let (n, _) = numbers(&BTreeMap::new(), &scrape(0.0), &scrape(0.0), 60.0);
        assert!(!n.keys().any(|k| k.starts_with("server.frame.")), "a build without the legs, or a window with no frames, reports none: {:?}", n.keys().collect::<Vec<_>>());
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a round reports what the allocator holds at the window's end, by kind and its
    /// slack (`active` less `allocated`), in MB, so the A/B of jemalloc's settings compares each; a build whose
    /// allocator says nothing (glibc's malloc, musl's) reports none of them.
    #[test]
    fn g11_s5_a_round_reports_what_the_allocator_holds_by_kind() {
        let scrape = |says: bool| {
            let mut t = String::from("process_resident_memory_bytes 300000000\n");
            if says {
                for (kind, bytes) in [("allocated", 90_000_000), ("thread_caches", 6_000_000), ("active", 100_000_000), ("dirty", 20_000_000), ("muzzy", 0)] {
                    t.push_str(&format!("kiln_memory_allocator_bytes{{kind=\"{kind}\"}} {bytes}\n"));
                }
            }
            measure::Scrape::parse(&t)
        };
        let (n, _) = numbers(&BTreeMap::new(), &scrape(true), &scrape(true), 60.0);
        let at = |k: &str| n.get(k).copied();
        assert_eq!(
            [at("server.allocator.allocated_mb"), at("server.allocator.thread_caches_mb"), at("server.allocator.active_mb"), at("server.allocator.dirty_mb"), at("server.allocator.muzzy_mb")],
            [Some(90.0), Some(6.0), Some(100.0), Some(20.0), Some(0.0)]
        );
        assert_eq!(at("server.allocator.slack_mb"), Some(10.0));
        assert_eq!(crate::lab::better("server.allocator.dirty_mb"), crate::stats::Better::Lower, "less held is better");
        let (n, _) = numbers(&BTreeMap::new(), &scrape(false), &scrape(false), 60.0);
        assert!(n.contains_key("server.rss_mb") && !n.keys().any(|k| k.starts_with("server.allocator.")), "{:?}", n.keys().collect::<Vec<_>>());
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a round reports the runner's hold of its updates beside the server's time
    /// with them, each its own, to the microsecond (`server.runner.runner_*`, `server.runner.server_*`): here the
    /// runner's tail at 73.5 ms where the server's is under a millisecond, as g9p7's fifth round's frames would have
    /// shown if the runner held them. A build, or runners, without the stamp report the legs before them alone.
    #[test]
    fn g10_s5_a_round_reports_the_runners_hold_beside_the_servers_time() {
        let scrape = |n: f64, stamped: bool| {
            let mut t = String::new();
            for leg in RUNNER_LEGS.iter().filter(|l| stamped || !matches!(**l, "runner" | "server")) {
                // The runner's: half at or under 0.1 ms, the rest up to 75 ms. The rest: half at or under 0.5 ms, all at or under 1 ms.
                let buckets = if *leg == "runner" { [("0.0001", n / 2.0), ("0.075", n), ("+Inf", n)] } else { [("0.0005", n / 2.0), ("0.001", n), ("+Inf", n)] };
                for (le, c) in buckets {
                    t.push_str(&format!("kiln_runner_wait_leg_seconds_bucket{{leg=\"{leg}\",le=\"{le}\"}} {c}\n"));
                }
            }
            measure::Scrape::parse(&t)
        };
        let (n, _) = numbers(&BTreeMap::new(), &scrape(0.0, true), &scrape(1000.0, true), 60.0);
        let at = |k: &str| n.get(k).copied();
        let near = |k: &str, want: f64| at(k).is_some_and(|v| (v - want).abs() < 1.0);
        assert!(near("server.runner.runner_p50_us", 100.0) && near("server.runner.runner_p99_us", 73_502.0), "the runner's: {:?} {:?}", at("server.runner.runner_p50_us"), at("server.runner.runner_p99_us"));
        assert!(near("server.runner.server_p50_us", 500.0) && near("server.runner.server_p99_us", 990.0), "the server's: {:?} {:?}", at("server.runner.server_p50_us"), at("server.runner.server_p99_us"));
        let (n, _) = numbers(&BTreeMap::new(), &scrape(0.0, false), &scrape(1000.0, false), 60.0);
        assert!(n.contains_key("server.runner.to_owner_p99_us"), "the legs before them");
        assert!(!n.keys().any(|k| k.starts_with("server.runner.runner_") || k.starts_with("server.runner.server_")), "no stamp, neither: {:?}", n.keys().collect::<Vec<_>>());
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a round reports its slowest API reads split, at p99 and at p99.9, by name;
    /// a window whose reads the server didn't time reports none.
    #[test]
    fn g10_s5_a_round_reports_its_slowest_reads_split() {
        let read = |took_us: u64, server: Option<crate::client::ServerTime>| Sample { class: Class::ApiRead, at_us: 0, took_us, ok: true, what: "", server, arrived_us: None };
        let told = |wait_us, took_us| Some(crate::client::ServerTime { wait_us: Some(wait_us), took_us });
        // 1,000 reads, the slowest two (the 999th and 1,000th by nearest rank) 4 ms, the server's share 1.5 ms of each.
        let window: Vec<Sample> = (0..998).map(|_| read(200, told(10, 10))).chain([read(4_000, told(1_000, 500)), read(4_000, told(1_000, 500))]).collect();
        let mut n = BTreeMap::new();
        splits(&mut n, &window);
        let get = |k: &str| n.get(k).copied();
        assert_eq!((get("split.api_read.p999.wait_ms"), get("split.api_read.p999.took_ms"), get("split.api_read.p999.rest_ms"), get("split.api_read.p999.n")), (Some(1.0), Some(0.5), Some(2.5), Some(2.0)));
        assert!(get("split.api_read.p99.n").is_some_and(|c| c >= 1.0), "{n:?}");
        let mut none = BTreeMap::new();
        splits(&mut none, &[read(200, None)]);
        assert!(none.is_empty(), "{none:?}");
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a round reports its API reads and frames split at their arrival on the lab's
    /// socket where it was stamped: the part to it and the lab's own after it (its thread's wake and its read), each at
    /// p50, p99 and p99.9; a sample without an arrival is left out, an arrival past a sample's end counts at its end, and a
    /// window with none stamped (off Linux) reports nothing of it.
    #[test]
    fn g10_s5_a_round_reports_its_reads_and_frames_split_at_their_arrival() {
        // A thousand frames each 600 µs from the agent's write to the lab's read, 450 µs of it to their arrival.
        let frame = |i: u64| Sample { class: Class::Websocket, at_us: 1_000_000 + i, took_us: 600, ok: true, what: "", server: None, arrived_us: Some(1_000_000 + i + 450) };
        let mut window: Vec<Sample> = (0..1000).map(frame).collect();
        let read = |took_us: u64, arrived_us: Option<u64>| Sample { class: Class::ApiRead, at_us: 50, took_us, ok: true, what: "", server: None, arrived_us };
        window.extend([read(200, Some(260)), read(999, None)]);
        let mut n = BTreeMap::new();
        arrivals(&mut n, &window);
        let get = |k: &str| n.get(k).copied();
        assert_eq!((get("websocket.arrived_p50_ms"), get("websocket.arrived_p999_ms"), get("websocket.lab_read_p999_ms")), (Some(0.45), Some(0.45), Some(0.15)));
        assert_eq!((get("api_read.arrived_p50_ms"), get("api_read.lab_read_p50_ms")), (Some(0.2), Some(0.0)), "past its end: at its end, and the read not stamped left out");
        let mut none = BTreeMap::new();
        arrivals(&mut none, &[Sample { arrived_us: None, ..frame(0) }]);
        assert!(none.is_empty(), "{none:?}");
    }

    /// Goal 10, plan 01a1a87b2dae step 5: only a frame that brings events (or an event's streamed text) is a
    /// delivery; a state's or a meta's frame that quotes an event's words (a summary is the agent's latest) isn't,
    /// whatever stamp it carries.
    #[test]
    fn g10_s5_only_frames_that_bring_events_are_deliveries() {
        let event = format!(r#"{{"t":"events","events":[{{"id":"e1","text":"{STAMP}1700000000000000 the retry loop"}}],"rev":9}}"#);
        let stream = format!(r#"{{"t":"stream","id":"e2","text":"{STAMP}1700000000000500 more","rev":10}}"#);
        let state = format!(r#"{{"t":"state","state":{{"summary":"{STAMP}1699999990000000 the retry loop"}},"rev":11}}"#);
        let patch = format!(r#"{{"t":"state_patch","set":{{"summary":"{STAMP}1699999990000000 x"}},"rev":12}}"#);
        for (frame, brings) in [(&event, true), (&stream, true), (&state, false), (&patch, false)] {
            assert_eq!(brings_events(frame.as_bytes()), brings, "{frame}");
            assert_eq!(stamps(frame.as_bytes()).count(), 1, "the stamp is read either way: {frame}");
        }
    }

    /// Goal 10, plan 01a1a87b2dae step 5: each class's server-side wait and time taken are reported at p99.9 too.
    #[test]
    fn g10_s5_a_classs_server_numbers_reach_p999() {
        let scrape = |n: f64| {
            let mut t = String::new();
            for family in ["kiln_route_wait_seconds", "kiln_route_seconds"] {
                // A thousand observations: 990 at or under 1 ms, the rest at or under 10 ms.
                for (le, c) in [("0.001", n * 0.99), ("0.01", n), ("+Inf", n)] {
                    t.push_str(&format!("{family}_bucket{{class=\"websocket\",le=\"{le}\"}} {c}\n"));
                }
            }
            measure::Scrape::parse(&t)
        };
        let (n, _) = numbers(&BTreeMap::new(), &scrape(0.0), &scrape(1000.0), 60.0);
        for key in ["wait", "took"] {
            let p999 = n.get(&format!("server.websocket.{key}_p999_ms")).copied();
            assert!(p999.is_some_and(|v| (v - 9.1).abs() < 0.01), "{key}: p99.9 {p999:?}");
            assert!(n.get(&format!("server.websocket.{key}_p99_ms")).is_some_and(|v| (v - 1.0).abs() < 0.01), "{key}: p99");
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 5: what the window's requests cost the shards in polls, parks, wakes and kicks
    /// is reported per request, each family's growth summed over the shards and its labels kept apart; a family
    /// the build lacks, or a window with no requests, reports nothing.
    #[test]
    fn g9_s5_the_shards_parks_and_wakes_are_reported_per_request() {
        let scrape = |k: f64| {
            let mut t = String::new();
            for sh in ["0", "1"] {
                t.push_str(&format!("kiln_shard_polls_total{{shard=\"{sh}\"}} {}\n", 30.0 * k));
                t.push_str(&format!("kiln_shard_parks_total{{shard=\"{sh}\"}} {}\n", 10.0 * k));
                for (by, each) in [("kick", 5.0), ("io", 3.0), ("timer", 2.0)] {
                    t.push_str(&format!("kiln_shard_wakeups_total{{shard=\"{sh}\",by=\"{by}\"}} {}\n", each * k));
                }
                t.push_str(&format!("kiln_shard_kicks_sent_total{{shard=\"{sh}\"}} {}\n", 4.0 * k));
                for (call, each, idle) in [("accept", 1.0, None), ("read", 6.0, Some(2.0)), ("write", 3.0, Some(0.0))] {
                    t.push_str(&format!("kiln_shard_socket_calls_total{{shard=\"{sh}\",call=\"{call}\"}} {}\n", each * k));
                    if let Some(idle) = idle {
                        t.push_str(&format!("kiln_shard_socket_calls_idle_total{{shard=\"{sh}\",call=\"{call}\"}} {}\n", idle * k));
                    }
                }
            }
            measure::Scrape::parse(&t)
        };
        let mut n = BTreeMap::new();
        per_request_wakes(&mut n, &scrape(1.0), &scrape(2.0), 10);
        for (key, want) in [("socket_accepts", 0.2), ("socket_reads", 1.2), ("socket_writes", 0.6), ("socket_reads_empty", 0.4), ("socket_writes_full", 0.0)] {
            let got = n.get(&format!("shards.{key}_per_request")).copied();
            assert!(got.is_some_and(|v| (v - want).abs() < 1e-9), "{key}: {got:?}, want {want}");
        }
        assert!(!n.contains_key("shards.syscalls_per_request"), "the sum only where every call is counted: {n:?}");
        // Each shard's count grew by itself once more: two shards' growth over ten requests.
        for (key, want) in [("polls", 6.0), ("parks", 2.0), ("wakeups_kick", 1.0), ("wakeups_io", 0.6), ("wakeups_timer", 0.4), ("kicks_sent", 0.8)] {
            let got = n.get(&format!("shards.{key}_per_request")).copied();
            assert!(got.is_some_and(|v| (v - want).abs() < 1e-9), "{key}: {got:?}, want {want}");
        }
        assert!(!n.contains_key("shards.steps_per_request"), "a family the build lacks isn't reported: {n:?}");
        let mut none = BTreeMap::new();
        per_request_wakes(&mut none, &scrape(1.0), &scrape(2.0), 0);
        assert!(none.is_empty(), "no requests, no per-request numbers: {none:?}");
    }

    /// Goal 9, plan 01a1a87b2cec step 4: what a spin before each park did is reported per request where the build counts
    /// it, its spins, its hits and the seconds it spun, summed over the shards; a build without it reports none (and the
    /// spin isn't among the system calls: no call is made to spin).
    #[test]
    fn g9_s4_the_shards_spins_before_their_parks_are_reported_per_request() {
        let scrape = |k: f64| {
            let mut t = String::new();
            for sh in ["0", "1"] {
                t.push_str(&format!("kiln_shard_polls_total{{shard=\"{sh}\"}} {}\n", 30.0 * k));
                t.push_str(&format!("kiln_shard_spins_total{{shard=\"{sh}\"}} {}\n", 8.0 * k));
                t.push_str(&format!("kiln_shard_spin_hits_total{{shard=\"{sh}\"}} {}\n", 3.0 * k));
                t.push_str(&format!("kiln_shard_spin_seconds_total{{shard=\"{sh}\"}} {}\n", 0.0001 * k));
            }
            measure::Scrape::parse(&t)
        };
        let mut n = BTreeMap::new();
        per_request_wakes(&mut n, &scrape(1.0), &scrape(2.0), 10);
        for (key, want) in [("spins", 1.6), ("spin_hits", 0.6), ("spin_seconds", 0.00002)] {
            let got = n.get(&format!("shards.{key}_per_request")).copied();
            assert!(got.is_some_and(|v| (v - want).abs() < 1e-12), "{key}: {got:?}, want {want}");
        }
        let mut without = BTreeMap::new();
        let polls = |k: f64| measure::Scrape::parse(&format!("kiln_shard_polls_total{{shard=\"0\"}} {}\n", 30.0 * k));
        per_request_wakes(&mut without, &polls(1.0), &polls(2.0), 10);
        assert!(!without.contains_key("shards.spins_per_request"), "{without:?}");
    }

    /// Goal 9, plan 01a1a87b2cec step 5: where the build counts every kind of system call a shard makes, a request's
    /// calls are reported summed too: its parks, kicks, unparks, harvests, arms and socket calls.
    #[test]
    fn g9_s5_a_requests_system_calls_are_summed_where_all_are_counted() {
        let scrape = |k: f64| {
            let mut t = String::new();
            for (family, label, each) in [
                ("kiln_shard_polls_total", "", 9.0),
                ("kiln_shard_parks_total", "", 1.0),
                ("kiln_shard_kicks_sent_total", "", 2.0),
                ("kiln_shard_threads_woken_total", "", 3.0),
                ("kiln_shard_harvests_total", "", 4.0),
                ("kiln_shard_arms_total", "", 5.0),
                ("kiln_shard_socket_calls_total", ",call=\"accept\"", 6.0),
                ("kiln_shard_socket_calls_total", ",call=\"read\"", 7.0),
                ("kiln_shard_socket_calls_total", ",call=\"write\"", 8.0),
            ] {
                t.push_str(&format!("{family}{{shard=\"0\"{label}}} {}\n", each * k));
            }
            measure::Scrape::parse(&t)
        };
        let mut n = BTreeMap::new();
        per_request_wakes(&mut n, &scrape(1.0), &scrape(3.0), 4);
        // Each grew by twice itself, over four requests: (1 + 2 + ... + 8) * 2 / 4.
        let got = n.get("shards.syscalls_per_request").copied();
        assert!(got.is_some_and(|v| (v - 18.0).abs() < 1e-9), "{got:?}");
    }

    /// Goal 9, plan 01a1a87b2cec step 5: what the window's requests handed the pools is reported per request: the
    /// compute pool's jobs, unparks and parks, and each blocking share's jobs by its name; no requests, nothing.
    #[test]
    fn g9_s5_the_pools_hand_offs_are_reported_per_request() {
        let scrape = |k: f64| {
            let mut t = String::new();
            t.push_str(&format!("kiln_compute_jobs_total {}\n", 40.0 * k));
            t.push_str(&format!("kiln_compute_unparks_total {}\n", 30.0 * k));
            t.push_str(&format!("kiln_compute_parks_total {}\n", 20.0 * k));
            for (share, jobs) in [("store", 5.0), ("store_write", 60.0)] {
                t.push_str(&format!("kiln_blocking_run_seconds_count{{share=\"{share}\"}} {}\n", jobs * k));
            }
            measure::Scrape::parse(&t)
        };
        let mut n = BTreeMap::new();
        per_request_pools(&mut n, &scrape(1.0), &scrape(2.0), 10);
        for (key, want) in [("compute.jobs_per_request", 4.0), ("compute.unparks_per_request", 3.0), ("compute.parks_per_request", 2.0), ("blocking.store.per_request", 0.5), ("blocking.store_write.per_request", 6.0)] {
            let got = n.get(key).copied();
            assert!(got.is_some_and(|v| (v - want).abs() < 1e-9), "{key}: {got:?}, want {want}");
        }
        let mut none = BTreeMap::new();
        per_request_pools(&mut none, &scrape(1.0), &scrape(2.0), 0);
        assert!(none.is_empty(), "no requests, no per-request numbers: {none:?}");
    }

    /// allocs-metric: the window's allocations and page faults are reported per request, each kind of fault by its
    /// name, from the scrapes either side of it; no requests, or a build without the series, nothing.
    #[test]
    fn allocs_metric_allocations_and_faults_are_reported_per_request() {
        let scrape = |k: f64| {
            let mut t = format!("kiln_memory_allocations_total {}\n", 500.0 * k);
            for (kind, faults) in [("minor", 30.0), ("major", 2.0)] {
                t.push_str(&format!("kiln_memory_page_faults_total{{kind=\"{kind}\"}} {}\n", faults * k));
            }
            measure::Scrape::parse(&t)
        };
        let mut n = BTreeMap::new();
        per_request_memory(&mut n, &scrape(1.0), &scrape(2.0), 10);
        for (key, want) in [("server.allocations_per_request", 50.0), ("server.minor_faults_per_request", 3.0), ("server.major_faults_per_request", 0.2)] {
            let got = n.get(key).copied();
            assert!(got.is_some_and(|v| (v - want).abs() < 1e-9), "{key}: {got:?}, want {want}");
        }
        assert_eq!(n.len(), 3, "{n:?}");
        let mut none = BTreeMap::new();
        per_request_memory(&mut none, &scrape(1.0), &scrape(2.0), 0);
        assert!(none.is_empty(), "no requests, no per-request numbers: {none:?}");
        let older = measure::Scrape::parse("process_cpu_seconds_total 1\n");
        per_request_memory(&mut none, &older, &older, 10);
        assert!(none.is_empty(), "a build without the series: {none:?}");
    }

    /// Goal 9, plan 01a1a87b2cec step 5: how long the shards' parks lasted is reported from their histograms, all
    /// shards together: the median, the p99, and the share of parks under 16 µs.
    #[test]
    fn g9_s5_the_shards_park_times_are_reported_together() {
        let scrape = |k: f64| {
            let mut t = String::new();
            for sh in ["0", "1"] {
                t.push_str(&format!("kiln_shard_polls_total{{shard=\"{sh}\"}} {}\n", k));
                // Each shard's window: 10 parks, 2 under 2 µs, 4 under 16 µs, 8 under 128 µs, all under 8,192 µs.
                for (le, n) in [("0.000001", 0.0), ("0.000002", 2.0), ("0.000004", 2.0), ("0.000008", 2.0), ("0.000016", 4.0), ("0.000032", 4.0), ("0.000064", 4.0), ("0.000128", 8.0), ("0.000256", 8.0), ("0.000512", 8.0), ("0.001024", 8.0), ("0.002048", 8.0), ("0.004096", 9.0), ("0.008192", 10.0), ("+Inf", 10.0)] {
                    t.push_str(&format!("kiln_shard_park_seconds_bucket{{shard=\"{sh}\",le=\"{le}\"}} {}\n", n * k));
                }
            }
            measure::Scrape::parse(&t)
        };
        let mut n = BTreeMap::new();
        server_numbers(&mut n, "", &scrape(0.0), &scrape(1.0), 60.0);
        let got = |key: &str| n.get(key).copied();
        assert!(got("shards.parks_under_16us_share").is_some_and(|v| (v - 0.4).abs() < 1e-9), "{:?}", got("shards.parks_under_16us_share"));
        // The median of 20 parks, 8 under 16 µs and 16 under 128 µs: between 64 and 128 µs.
        assert!(got("shards.park_p50_us").is_some_and(|v| (64.0..=128.0).contains(&v)), "{:?}", got("shards.park_p50_us"));
        // Their p99, of the 2 between 4,096 and 8,192 µs.
        assert!(got("shards.park_p99_us").is_some_and(|v| (4096.0..=8192.0).contains(&v)), "{:?}", got("shards.park_p99_us"));
    }

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

    /// main's ask (2026-11-06): a stamp written before the viewer asked for the session, first seen in a later frame, is of
    /// an event its snapshot held, re-sent with an update: the Task's call in its result's frame is no frame's latency; the
    /// result's own stamp is, once; a stamp written once the attach began counts in its first frame.
    #[test]
    fn a_task_call_re_sent_in_its_results_frame_is_no_frames_latency() {
        let mut seen = HashSet::new();
        // The viewer asked for the session at 2,000 µs: the Task's call, written at 1,000, was in its snapshot; a subagent's
        // event came at 2,500, the Task's result at 5,000.
        assert_eq!(frames_news(br#"{"t":"events","events":[{"id":"e7","text":"@lab:2500 reading"}]}"#, &mut seen, 2_000), vec![2_500]);
        let result = br#"{"t":"events","events":[{"id":"toolu_task","body":{"input":{"description":"@lab:1000 audit"},"output":"@lab:5000 done"}}]}"#;
        assert_eq!(frames_news(result, &mut seen, 2_000), vec![5_000], "the call's stamp, re-carried, isn't the frame's");
        assert_eq!(frames_news(result, &mut seen, 2_000), Vec::<u64>::new(), "nothing counts twice");
        assert!(seen.contains(&1_000), "the re-carried stamp is marked seen: {seen:?}");
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

    /// Goal 5, plan 01a1a59c2aae step 1 (census-B's event-listener rates below zero): a site the build counts under
    /// several copies of its location has a series of each under the same labels, in the census table's order.
    /// Each scrape's copies are summed before the growth is taken, so a big copy at the window's start isn't
    /// paired against a small one at its end, and a copy first taken in the window, ahead of the others in the
    /// table, adds what it took.
    #[test]
    fn g5_s1_a_sites_copies_are_summed_before_its_growth() {
        let site = "event-listener@5.4.2:src/intrusive.rs:220:14";
        // Each copy as the census renders it: taken, held when asked, seconds waited, longest wait, addresses.
        let scrape = |copies: &[(u64, u64, f64, f64, &str)]| {
            let mut t = String::from("# TYPE kiln_lock_taken_total counter\n");
            for (taken, contended, wait, max, addrs) in copies {
                let labels = format!("site=\"{site}\",kind=\"std_mutex\"");
                t.push_str(&format!("kiln_lock_taken_total{{{labels}}} {taken}\nkiln_lock_contended_total{{{labels}}} {contended}\n"));
                t.push_str(&format!("kiln_lock_wait_seconds_total{{{labels}}} {wait}\nkiln_lock_wait_max_seconds{{{labels}}} {max}\n"));
                t.push_str(&format!("kiln_lock_addresses{{{labels},addrs=\"{addrs}\"}} 1\n"));
            }
            t.push_str("kiln_lock_sites_full_total 0\n");
            measure::Scrape::parse(&t)
        };
        let (big, small) = ((20_000, 1_000, 0.5, 0.25, "7f01 7f02"), (1_000, 50, 0.125, 0.0625, "7f03"));
        let fill = (scrape(&[]), scrape(&[(18_000, 900, 0.25, 0.25, "7f01 7f02"), (900, 40, 0.125, 0.0625, "7f03")]));
        let start = scrape(&[big, small]);
        // In the window the big copy took 6,000 and the small one 500, and a third, first taken in it, 300.
        let end = scrape(&[(300, 2, 0.0625, 0.03125, "7f04 7f01"), (26_000, 1_400, 0.75, 0.25, "7f01 7f02"), (1_500, 60, 0.125, 0.0625, "7f03")]);
        let locks = lock_census((&fill.0, &fill.1), (&start, &end));
        let seen = locks.get(&format!("{site} std_mutex"));
        let got = seen.map(|s| (s.taken, s.contended, s.wait_s, s.fill_taken, s.fill_contended, s.fill_wait_s, s.max_wait_s, s.addrs.as_str(), s.copies));
        assert_eq!(got, Some((6_800.0, 412.0, 0.3125, 18_900.0, 940.0, 0.375, 0.25, "7f04 7f01 7f02 7f03", 3)), "{locks:?}");
        assert_eq!(locks.len(), 1, "one entry a site and kind, whatever its copies: {locks:?}");
    }
}
