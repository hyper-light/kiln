//! Headroom (docs/headroom.md): a machine never gets so busy that what matters on it stops
//! working. The decisions are `kiln_ci::headroom`'s, pure code; this is the runner's side of them:
//! probing the targets at the intervals the engine derives, reading the machine (its disks'
//! counters, its free memory, each group of work's CPU, writes and memory), holding sessions'
//! next turns until the engine lets them start, pacing running groups to the shares it gives, and
//! keeping what it learned so a restart starts from it. It only ever slows what the runner itself
//! started (or what the user's service manager runs for agents), so it needs no privileges, and it
//! runs on the runner alone: with the server away, from settings it kept.

pub mod control;
mod pace;
mod probes;
pub mod settings;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use kiln_ci::headroom::{Engine, Group, Learned, Observation, Target, Waiting};
use parking_lot::{Condvar, Mutex};
use serde::Serialize;

pub use kiln_ci::headroom::Class;
pub use settings::{Push, Settings};

/// The settings file in the runner's headroom folder.
const SETTINGS: &str = "settings.json";
/// What the engine learned, kept between runs.
const LEARNED: &str = "learned.json";

/// Who may start their next turn.
#[derive(Default)]
struct Admit {
    /// Sessions let in, until their turn ends (or they do).
    granted: HashMap<String, u64>,
    /// Sessions held, since when (µs since the epoch).
    waiting: HashMap<String, u64>,
    /// Sessions whose turn ended since the engine last looked.
    ended: Vec<String>,
    /// Sessions let in whose held work hasn't been woken for yet (each wakes once a grant).
    fresh: std::collections::HashSet<String>,
}

struct Shared {
    /// A runner started headroom in this process (else every session is let in, as before).
    running: AtomicBool,
    enabled: AtomicBool,
    /// Another process of this machine paces; this one only holds work back.
    standby: AtomicBool,
    admit: Mutex<Admit>,
    /// Held sessions wait on this; the engine notifies it when one may start.
    room: event_listener::Event,
    kick: (Mutex<bool>, Condvar),
    pushed: Mutex<Option<Settings>>,
    latest: Mutex<Option<String>>,
    changed: crate::rt::Notify,
}

static SHARED: LazyLock<Shared> = LazyLock::new(|| Shared {
    running: AtomicBool::new(false),
    enabled: AtomicBool::new(true),
    standby: AtomicBool::new(false),
    admit: Mutex::new(Admit::default()),
    room: event_listener::Event::new(),
    kick: (Mutex::new(false), Condvar::new()),
    pushed: Mutex::new(None),
    latest: Mutex::new(None),
    changed: crate::rt::Notify::new(),
});

fn kick() {
    let (lock, cv) = &SHARED.kick;
    *lock.lock() = true;
    cv.notify_one();
}

fn now_us() -> u64 {
    u64::try_from(crate::util::now_ms()).unwrap_or(0).saturating_mul(1000)
}

/// May agent session `session` start its next turn now? A person's own message always may. When
/// it may not, it's held, and [`room`] completes when the engine lets it start. Cheap, and always
/// yes where no runner started headroom.
pub fn admit(session: &str, class: Class) -> bool {
    let s = &*SHARED;
    if !s.running.load(Ordering::Acquire) || !s.enabled.load(Ordering::Acquire) || class == Class::Person {
        return true;
    }
    let now = now_us();
    let mut a = s.admit.lock();
    if a.granted.get(session).is_some_and(|until| *until > now) {
        return true;
    }
    a.granted.remove(session);
    let first = !a.waiting.contains_key(session);
    a.waiting.entry(session.to_owned()).or_insert(now);
    drop(a);
    if first {
        tracing::info!(session, "headroom: held until the engine lets it in");
        kick();
    }
    false
}

/// A session's turn ended: its place goes, and what the turn took is learned.
pub fn turn_ended(session: &str) {
    if session.is_empty() || !SHARED.running.load(Ordering::Acquire) {
        return;
    }
    let mut a = SHARED.admit.lock();
    a.granted.remove(session);
    a.fresh.remove(session);
    a.ended.push(session.to_owned());
}

/// Completes when held work may be worth asking about again: listening starts when this is
/// called, so room given before it's awaited still wakes it.
pub fn room() -> impl std::future::Future<Output = ()> {
    SHARED.room.listen()
}

/// Whether `session` was let in since its held work last woke for it (once a grant: what wakes
/// for it takes it).
pub fn take_grant(session: &str) -> bool {
    SHARED.admit.lock().fresh.remove(session)
}

/// Whether `session` is held now.
pub fn held(session: &str) -> bool {
    SHARED.admit.lock().waiting.contains_key(session)
}

/// What a runner's headroom works from.
#[derive(Clone, Debug)]
pub struct Ctx {
    /// `<state>/headroom`: its settings, what it learned, its note of what it paced.
    pub dir: PathBuf,
    /// The server the runner reports to: its health is a target (`Settings::automatic`).
    pub server_url: String,
    /// The runner's data folder: a sync there is a target (`Settings::automatic`).
    pub data_dir: PathBuf,
}

/// The runner's headroom, for whoever reports it to the server.
#[derive(Clone)]
pub struct Handle;

impl Handle {
    /// What the server sent (`ServerMsg::Headroom`): applied at once, and kept for the next start.
    pub fn push(&self, json: &str) {
        match serde_json::from_str::<Push>(json).map_err(|e| e.to_string()).and_then(|p| p.settings.validated().map(|s| (s, p.standby))) {
            Ok((s, standby)) => {
                SHARED.standby.store(standby, Ordering::Release);
                *SHARED.pushed.lock() = Some(s);
                kick();
            }
            Err(e) => tracing::warn!(error = %e, "headroom: settings from the server don't read; keeping the ones in use"),
        }
    }

    /// The newest status (JSON [`Status`]).
    pub fn latest(&self) -> Option<String> {
        SHARED.latest.lock().clone()
    }

    /// Completes when the status changed since it was last read through here.
    pub async fn changed(&self) {
        SHARED.changed.notified().await;
    }
}

/// The argument that makes kiln a pacer's guard (`kiln headroom-guard <record>`).
pub const GUARD_ARG: &str = pace::GUARD_ARG;

/// `kiln headroom-guard <record>`: a pacer's guard, which starts again what its pacer stopped once
/// the pacer ends, however it ends (docs/headroom.md, "Failing safe").
pub fn guard_main(record: Option<std::ffi::OsString>) -> std::process::ExitCode {
    match record {
        Some(r) => pace::guard(Path::new(&r)),
        None => std::process::ExitCode::FAILURE,
    }
}

/// Starts headroom for this runner (once per process; later calls get the same).
pub fn start(ctx: Ctx) -> Handle {
    if SHARED.running.swap(true, Ordering::AcqRel) {
        return Handle;
    }
    let settings = read_json::<Settings>(&ctx.dir.join(SETTINGS)).and_then(|s| s.validated().ok()).unwrap_or_default();
    apply_shared(&settings);
    let spawned = std::thread::Builder::new().name("kiln-headroom".into()).spawn(move || run(ctx, settings));
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "headroom: couldn't start; work starts unchecked");
        SHARED.running.store(false, Ordering::Release);
    }
    Handle
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn apply_shared(s: &Settings) {
    SHARED.enabled.store(s.enabled, Ordering::Release);
}

/// The targets probed: the settings' own, and kiln's when `automatic`.
fn targets(s: &Settings, ctx: &Ctx) -> Vec<settings::Target> {
    let mut out = Vec::new();
    if s.automatic {
        out.push(settings::Target { name: "runner disk".into(), probe: settings::Probe::Fsync { path: ctx.data_dir.display().to_string() }, tolerance_ms: None });
        if ctx.server_url.starts_with("http://") {
            out.push(settings::Target { name: "kiln server".into(), probe: settings::Probe::Http { url: format!("{}/healthz", ctx.server_url.trim_end_matches('/')) }, tolerance_ms: None });
        }
    }
    out.extend(s.targets.iter().cloned());
    out
}

/// One group of work, as the status shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GroupStatus {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub cpu_pct: u32,
    pub write_mb_s: u32,
    pub mem_mb: u64,
    /// Its share of time, in percent (100: full speed).
    pub share: u8,
    pub why: String,
}

/// A session held, as the status shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HeldStatus {
    pub session: String,
    pub since_ms: i64,
    pub why: String,
    /// When it's expected to start (ms since the epoch), when known.
    pub at_ms: Option<i64>,
}

/// What a runner reports (`AgentMsg::HeadroomStatus`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    /// The fingerprint of the settings in use ([`Settings::fingerprint`]).
    pub settings: String,
    pub enabled: bool,
    /// How this machine paces work: `cgroup`, or `none` (work only waits to start).
    pub pacing: &'static str,
    #[serde(flatten)]
    pub engine: kiln_ci::headroom::Status,
    pub probe_every_ms: BTreeMap<String, f64>,
    pub groups: Vec<GroupStatus>,
    pub held: Vec<HeldStatus>,
    /// What changed last, and why.
    pub events: Vec<String>,
}

/// The machine's write rate from its disks' own counters (/proc/diskstats: sectors written, of
/// whole physical devices), between two reads.
#[derive(Default)]
struct Disks {
    last: Option<(std::time::Instant, u64)>,
}

impl Disks {
    fn sectors_written() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/diskstats").ok()?;
        let mut total = 0u64;
        let mut any = false;
        for line in text.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (Some(name), Some(sectors)) = (f.get(2), f.get(9).and_then(|v| v.parse::<u64>().ok())) else { continue };
            // Whole physical devices only: a partition or a mapper would count the same writes twice.
            if Path::new("/sys/block").join(name).join("device").exists() {
                total += sectors;
                any = true;
            }
        }
        any.then_some(total)
    }

    /// Bytes written a second since the last read (none on the first, or where the system can't say).
    fn rate(&mut self) -> Option<f64> {
        let now = std::time::Instant::now();
        let s = Self::sectors_written()?;
        let r = self.last.map(|(at, s0)| s.saturating_sub(s0) as f64 * 512.0 / now.duration_since(at).as_secs_f64().max(f64::MIN_POSITIVE));
        self.last = Some((now, s));
        r
    }
}

/// Memory the machine has free for new work, in bytes: MemAvailable on Linux, what the system says
/// is available elsewhere.
fn mem_free() -> f64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("MemAvailable:")).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<f64>().ok()))
        .map_or_else(
            || {
                let mut sys = sysinfo::System::new();
                sys.refresh_memory_specifics(sysinfo::MemoryRefreshKind::nothing().with_ram());
                sys.available_memory() as f64
            },
            |kib| kib * 1024.0,
        )
}

/// A group as the engine knows it: an agent session by its id (as it waits and is let in), anything
/// else by where it is.
fn id_of(g: &pace::Group) -> String {
    g.session.clone().unwrap_or_else(|| g.dir.display().to_string())
}

fn run(ctx: Ctx, mut settings: Settings) {
    let _ = std::fs::create_dir_all(&ctx.dir);
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(kick);
    let specs = |s: &Settings| -> Vec<probes::Spec> { targets(s, &ctx).into_iter().map(|t| probes::Spec { tolerance: Duration::from_millis(u64::from(t.tolerance_ms())), name: t.name, probe: t.probe }).collect() };
    let engine_targets = |s: &Settings| -> Vec<Target> { targets(s, &ctx).into_iter().map(|t| Target { tolerance_us: f64::from(t.tolerance_ms()) * 1e3, served: t.probe.served(), name: t.name }).collect() };
    let learned_path = ctx.dir.join(LEARNED);
    let learned: Learned = read_json(&learned_path).unwrap_or_default();
    let mut engine = Engine::new(settings.policy(), engine_targets(&settings), learned, now_us());
    let mut probes = probes::Probes::start(specs(&settings), Arc::clone(&wake));
    let pacer = pace::Pacer::start(&ctx.dir);
    let mut meter = pace::Meter::default();
    let mut disks = Disks::default();
    let mut groups: Vec<pace::Group> = Vec::new();
    let mut loads: Vec<pace::Load> = Vec::new();
    let mut measured_at: Option<std::time::Instant> = None;
    let mut delivered: Option<f64> = None;
    let mut shares: HashMap<PathBuf, f64> = HashMap::new();
    let mut events: Vec<String> = Vec::new();
    let mut last_status: Option<String> = None;
    tracing::info!(pacing = pace::how(), targets = probes.specs.len(), "headroom: watching this machine");
    loop {
        {
            let (lock, cv) = &SHARED.kick;
            let mut k = lock.lock();
            if !*k {
                // Woken by a probe's answer, a held session, new settings, or by the soonest a probe
                // may be out long enough to matter.
                cv.wait_for(&mut k, Duration::from_micros(probes.fastest_us()));
            }
            *k = false;
        }
        if let Some(new) = SHARED.pushed.lock().take()
            && new != settings
        {
            if let Err(e) = crate::util::write_secret_unsynced(&ctx.dir.join(SETTINGS), &serde_json::to_vec_pretty(&new).unwrap_or_default(), 0o644) {
                tracing::warn!(error = %e, "headroom: couldn't keep the server's settings; they apply until the runner restarts");
            }
            tracing::info!(enabled = new.enabled, fingerprint = %new.fingerprint(), "headroom: settings from the server");
            settings = new;
            apply_shared(&settings);
            probes = probes::Probes::start(specs(&settings), Arc::clone(&wake));
            let learned = std::mem::take(&mut engine.learned);
            engine = Engine::new(settings.policy(), engine_targets(&settings), learned, now_us());
            SHARED.room.notify(usize::MAX);
        }
        // The machine and its groups, measured as often as the fastest probe: the time scale the
        // engine decides at.
        if measured_at.is_none_or(|m| m.elapsed() >= Duration::from_micros(probes.fastest_us())) {
            measured_at = Some(std::time::Instant::now());
            groups = pace::groups(settings.escapes);
            loads = meter.load(&groups);
            delivered = disks.rate();
        }
        let (answers, inflight) = probes.drain();
        let now = now_us();
        let (waiting, ended) = {
            let mut a = SHARED.admit.lock();
            a.granted.retain(|_, until| *until > now);
            let waiting: Vec<Waiting> = a.waiting.iter().map(|(s, since)| Waiting { id: s.clone(), key: s.clone(), class: Class::Agent, since_us: *since }).collect();
            (waiting, std::mem::take(&mut a.ended))
        };
        let running: Vec<Group> = groups
            .iter()
            .zip(&loads)
            .map(|(g, l)| Group {
                id: id_of(g),
                key: g.session.clone().unwrap_or_else(|| g.label.clone()),
                class: Class::Agent,
                protected: false,
                write_bps: l.write_rate,
                cpu_cores: l.cpu_pct / 100.0,
                mem_bytes: l.mem_bytes,
                share: shares.get(&g.dir).copied().unwrap_or(1.0),
            })
            .collect();
        // Sessions whose turn ended, or that ended (with processes or without): by the same id
        // they waited and run under.
        let obs = Observation { now_us: now, answers, inflight, delivered_bps: delivered, mem_free: mem_free(), running, waiting, ended };
        let plan = engine.step(&obs);
        for (name, every) in &plan.probe_every_us {
            probes.set_interval(name, *every);
        }
        // Pacing: the engine's shares, unless off or another process of this machine paces.
        let pacing = settings.enabled && !SHARED.standby.load(Ordering::Acquire);
        shares = if pacing {
            plan.shares.iter().filter(|p| p.share < 1.0).filter_map(|p| groups.iter().find(|g| id_of(g) == p.id).map(|g| (g.dir.clone(), p.share))).collect()
        } else {
            HashMap::new()
        };
        pacer.set(shares.iter().map(|(d, s)| (d.clone(), (s * kiln_ci::headroom::pace::SHARE_STEPS).floor().clamp(0.0, 99.0) as u8)).collect(), Duration::from_micros(probes.fastest_us()));
        // Admission: who the engine let start.
        // A place holds until the session's turn ends (or it does): how long it was expected to
        // run is the engine's to plan with, not when the place goes.
        let started: Vec<(String, u64)> = plan.decisions.iter().filter(|d| d.start).map(|d| (d.id.clone(), u64::MAX)).collect();
        if !started.is_empty() {
            let mut a = SHARED.admit.lock();
            for (s, until) in &started {
                a.waiting.remove(s);
                a.granted.insert(s.clone(), *until);
                a.fresh.insert(s.clone());
                let why = plan.decisions.iter().find(|d| &d.id == s).map(|d| d.why.clone()).unwrap_or_default();
                tracing::info!(session = %s, why, "headroom: let in");
            }
            drop(a);
            SHARED.room.notify(usize::MAX);
        }
        if !plan.events.is_empty() {
            for e in &plan.events {
                tracing::info!(event = %e, "headroom");
            }
            // The status says what changed last; the server keeps the history.
            events.clone_from(&plan.events);
            // What was learned is kept whenever something changed what it decides.
            if let Err(e) = crate::util::write_secret_unsynced(&learned_path, &serde_json::to_vec(&engine.learned).unwrap_or_default(), 0o644) {
                tracing::warn!(error = %e, "headroom: couldn't keep what it learned");
            }
        }
        let held: Vec<HeldStatus> = {
            let a = SHARED.admit.lock();
            plan.decisions
                .iter()
                .filter(|d| !d.start)
                .map(|d| HeldStatus {
                    session: d.id.clone(),
                    since_ms: a.waiting.get(&d.id).map_or(0, |s| i64::try_from(s / 1000).unwrap_or(0)),
                    why: d.why.clone(),
                    at_ms: d.at_us.map(|t| i64::try_from(t / 1000).unwrap_or(0)),
                })
                .collect()
        };
        let status = Status {
            settings: settings.fingerprint(),
            enabled: settings.enabled,
            pacing: pace::how(),
            probe_every_ms: plan.probe_every_us.iter().map(|(k, v)| (k.clone(), (v / 1e3).round())).collect(),
            groups: groups
                .iter()
                .zip(&loads)
                .filter(|(g, l)| l.write_rate > 0.0 || shares.contains_key(&g.dir))
                .map(|(g, l)| GroupStatus {
                    label: g.label.clone(),
                    session: g.session.clone(),
                    cpu_pct: l.cpu_pct.round() as u32,
                    write_mb_s: (l.write_rate / 1e6).round() as u32,
                    mem_mb: (l.mem_bytes / 1e6).round() as u64,
                    share: shares.get(&g.dir).map_or(100, |s| (s * kiln_ci::headroom::pace::SHARE_STEPS).floor() as u8),
                    why: plan.shares.iter().find(|p| p.id == g.dir.display().to_string()).map(|p| p.why.clone()).unwrap_or_default(),
                })
                .collect(),
            held,
            events: events.clone(),
            engine: plan.status,
        };
        // Told when what it says changes.
        if let Ok(json) = serde_json::to_string(&status)
            && last_status.as_deref() != Some(json.as_str())
        {
            *SHARED.latest.lock() = Some(json.clone());
            SHARED.changed.notify();
            last_status = Some(json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_runner_every_session_is_let_in() {
        if !SHARED.running.load(Ordering::Acquire) {
            assert!(admit("s1", Class::Agent));
        }
    }

    #[test]
    fn automatic_targets_are_the_runner_disk_and_its_server() {
        let ctx = Ctx { dir: "/x/headroom".into(), server_url: "http://192.168.49.2:30070".into(), data_dir: "/x".into() };
        let t = targets(&Settings::default(), &ctx);
        assert_eq!(t.len(), 2);
        assert_eq!(t.get(1).map(|t| &t.probe), Some(&settings::Probe::Http { url: "http://192.168.49.2:30070/healthz".into() }));
        assert!(t.iter().all(|t| t.tolerance_ms() == settings::PLATFORM_TOLERANCE_MS));
        let off = Settings { automatic: false, ..Settings::default() };
        assert!(targets(&off, &ctx).is_empty());
    }

    #[test]
    fn the_disks_counters_read_on_linux() {
        if cfg!(target_os = "linux") && Path::new("/proc/diskstats").exists() {
            let mut d = Disks::default();
            assert_eq!(d.rate(), None);
            std::thread::sleep(Duration::from_millis(20));
            assert!(d.rate().is_some_and(|r| r >= 0.0));
            assert!(mem_free() > 0.0);
        }
    }
}
