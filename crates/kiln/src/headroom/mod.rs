//! Headroom (docs/headroom.md): a machine never gets so busy that what matters on it stops
//! working. kiln measures what it protects directly (its targets: a small fsync where they write,
//! their health answers), keeps a window of how much heavy work may run at full speed (grown while
//! every target meets its goal, halved when one misses), holds new work past it before it starts,
//! and paces the work already running past it: stopped part of every period, the lowest class and
//! the heaviest writer first. It only ever slows what the runner itself started, so it needs no
//! privileges, and it runs on the runner alone: with the server away, with settings it kept.
//!
//! A runner restarted starts the window small, so what it was running resumes a few at a time.

pub mod control;
mod pace;
mod probes;
pub mod settings;
pub mod window;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use serde::Serialize;

pub use settings::{Push, Settings};
pub use window::Class;

/// The control's beat: how often targets are looked at (a probe finishing wakes it sooner).
const BEAT: Duration = Duration::from_millis(250);
/// How often groups of work are found and measured.
const MEASURE: Duration = Duration::from_secs(1);
/// The settings file in the runner's headroom folder.
const SETTINGS: &str = "settings.json";

/// What sessions consult: how much heavy work runs, and who holds a place.
#[derive(Default)]
struct Admit {
    allowed: usize,
    /// Agent sessions whose work is heavy now.
    heavy: HashSet<String>,
    /// Heavy groups that are no session (a build run outside its session's group).
    heavy_others: usize,
    /// Sessions let in whose work hasn't shown yet, until when (ms).
    reserved: HashMap<String, i64>,
    /// Sessions held, since when (ms).
    waiting: HashMap<String, i64>,
}

struct Shared {
    /// A runner started headroom in this process (else every session is let in, as before).
    running: AtomicBool,
    enabled: AtomicBool,
    hold_ms: Mutex<i64>,
    max_sessions: Mutex<Option<usize>>,
    admit: Mutex<Admit>,
    /// Held sessions wait on this; the control notifies it when there's room.
    room: event_listener::Event,
    kick: (Mutex<bool>, Condvar),
    pushed: Mutex<Option<Settings>>,
    /// Another process of this machine paces; this one only holds work back.
    standby: AtomicBool,
    latest: Mutex<Option<String>>,
    changed: crate::rt::Notify,
    restoring: AtomicBool,
}

static SHARED: LazyLock<Shared> = LazyLock::new(|| Shared {
    running: AtomicBool::new(false),
    enabled: AtomicBool::new(true),
    hold_ms: Mutex::new(30_000),
    max_sessions: Mutex::new(None),
    admit: Mutex::new(Admit::default()),
    room: event_listener::Event::new(),
    kick: (Mutex::new(false), Condvar::new()),
    pushed: Mutex::new(None),
    standby: AtomicBool::new(false),
    latest: Mutex::new(None),
    changed: crate::rt::Notify::new(),
    restoring: AtomicBool::new(false),
});

fn kick() {
    let (lock, cv) = &SHARED.kick;
    *lock.lock() = true;
    cv.notify_one();
}

/// May agent session `session` start new work now (a turn, its queue's next message)? A person's
/// own message always may. When it may not, it's held: [`room`] completes when it's worth asking
/// again. Cheap, and always yes where no runner started headroom.
pub fn admit(session: &str, class: Class) -> bool {
    let s = &*SHARED;
    if !s.running.load(Ordering::Acquire) || !s.enabled.load(Ordering::Acquire) || class == Class::Person {
        return true;
    }
    let now = crate::util::now_ms();
    let mut a = s.admit.lock();
    if a.heavy.contains(session) || a.reserved.contains_key(session) {
        return true;
    }
    let used = a.heavy.len() + a.heavy_others + a.reserved.len();
    if used < a.allowed {
        let hold = *s.hold_ms.lock();
        a.reserved.insert(session.to_owned(), now + hold);
        a.waiting.remove(session);
        true
    } else {
        let first = !a.waiting.contains_key(session);
        a.waiting.entry(session.to_owned()).or_insert(now);
        drop(a);
        if first {
            tracing::debug!(session, "headroom: new work held: the machine is at its window");
            kick();
        }
        false
    }
}

/// Completes when held work is worth asking about again (room opened, or settings changed).
pub async fn room() {
    SHARED.room.listen().await;
}

/// Whether `session` is held now.
pub fn held(session: &str) -> bool {
    SHARED.admit.lock().waiting.contains_key(session)
}

/// The most agent sessions this machine may run, when its settings say (new ones only: sessions
/// a restart continues are never turned away).
pub fn max_sessions() -> Option<usize> {
    if SHARED.restoring.load(Ordering::Acquire) { None } else { *SHARED.max_sessions.lock() }
}

/// Sessions a restart continues are being opened (no session limit applies to them).
pub fn restoring(on: bool) {
    SHARED.restoring.store(on, Ordering::Release);
}

/// What a runner's headroom works from.
#[derive(Clone, Debug)]
pub struct Ctx {
    /// `<state>/headroom`: its settings and its note of what it paced.
    pub dir: PathBuf,
    /// The server the runner reports to: its health is a target (`Settings::automatic`).
    pub server_url: String,
    /// The runner's data folder: an fsync there is a target (`Settings::automatic`).
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

/// Starts headroom for this runner (once per process; later calls get the same).
pub fn start(ctx: Ctx) -> Handle {
    if SHARED.running.swap(true, Ordering::AcqRel) {
        return Handle;
    }
    let settings = load(&ctx.dir).unwrap_or_default();
    apply_shared(&settings);
    let spawned = std::thread::Builder::new().name("kiln-headroom".into()).spawn(move || run(ctx, settings));
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "headroom: couldn't start; work starts unchecked");
        SHARED.running.store(false, Ordering::Release);
    }
    Handle
}

fn load(dir: &std::path::Path) -> Option<Settings> {
    let raw = std::fs::read(dir.join(SETTINGS)).ok()?;
    serde_json::from_slice::<Settings>(&raw).ok().and_then(|s| s.validated().ok())
}

fn apply_shared(s: &Settings) {
    SHARED.enabled.store(s.enabled, Ordering::Release);
    *SHARED.hold_ms.lock() = i64::from(s.admit_hold_secs) * 1000;
    *SHARED.max_sessions.lock() = s.max_sessions.map(|n| n as usize);
    SHARED.admit.lock().allowed = s.window.start as usize;
}

/// The targets probed: the settings' own, and kiln's when `automatic`.
fn targets(s: &Settings, ctx: &Ctx) -> Vec<settings::Target> {
    let mut out = Vec::new();
    if s.automatic {
        out.push(settings::Target { name: "runner disk".into(), probe: settings::Probe::Fsync { path: ctx.data_dir.display().to_string() }, goal_ms: 1000 });
        if ctx.server_url.starts_with("http://") {
            out.push(settings::Target { name: "kiln server".into(), probe: settings::Probe::Http { url: format!("{}/healthz", ctx.server_url.trim_end_matches('/')) }, goal_ms: 2000 });
        }
    }
    out.extend(s.targets.iter().cloned());
    out
}

/// One target, as the status shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TargetStatus {
    pub name: String,
    pub probe: settings::Probe,
    pub goal_ms: u32,
    #[serde(flatten)]
    pub verdict: probes::Verdict,
}

/// One group of work, as the status shows it (heavy or paced ones only).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GroupStatus {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub cpu_pct: u32,
    pub write_mb_s: u32,
    pub heavy: bool,
    /// Its share of time, in percent (100: not paced).
    pub share: u8,
}

/// What a runner reports (`AgentMsg::HeadroomStatus`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    /// The fingerprint of the settings in use ([`Settings::fingerprint`]).
    pub settings: String,
    pub enabled: bool,
    /// How this machine paces work: `cgroup`, or `none` (work past the window only waits).
    pub pacing: &'static str,
    pub window: f64,
    pub allowed: usize,
    pub healthy: bool,
    pub targets: Vec<TargetStatus>,
    pub groups: Vec<GroupStatus>,
    /// Sessions whose new work waits for room.
    pub held: usize,
    /// Sessions let in whose work hasn't shown yet.
    pub starting: usize,
}

fn run(ctx: Ctx, mut settings: Settings) {
    let _ = std::fs::create_dir_all(&ctx.dir);
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(kick);
    let mut probes = probes::Probes::start(targets(&settings, &ctx), Arc::clone(&wake));
    let pacer = pace::Pacer::start(&ctx.dir);
    let mut control = window::Control::new(&settings);
    let mut meter = pace::Meter::default();
    let mut groups: Vec<pace::Group> = Vec::new();
    let mut loads: Vec<pace::Load> = Vec::new();
    let mut shares: HashMap<PathBuf, u8> = HashMap::new();
    let mut measured: Option<Instant> = None;
    let base = Instant::now();
    let mut last_status: Option<Status> = None;
    let mut reported = Instant::now();
    let stop = AtomicBool::new(false);
    tracing::info!(pacing = pace::how(), targets = probes.targets.len(), window = control.window(), "headroom: watching this machine");
    while !stop.load(Ordering::Relaxed) {
        {
            let (lock, cv) = &SHARED.kick;
            let mut k = lock.lock();
            if !*k {
                cv.wait_for(&mut k, BEAT);
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
            probes = probes::Probes::start(targets(&settings, &ctx), Arc::clone(&wake));
            control.reset(&settings);
            SHARED.room.notify(usize::MAX);
        }
        let now = base.elapsed().as_millis() as u64;
        let verdicts = probes.verdicts();
        let healthy = !verdicts.iter().any(|v| matches!(v, probes::Verdict::Missed { .. }));
        if control.observe(&settings, now, healthy) {
            tracing::info!(window = control.window(), missed = ?probes.targets.iter().zip(&verdicts).filter(|(_, v)| matches!(v, probes::Verdict::Missed { .. })).map(|(t, _)| t.name.as_str()).collect::<Vec<_>>(), "headroom: a target missed its goal; the window halves");
        }
        if measured.is_none_or(|m| m.elapsed() >= MEASURE) {
            measured = Some(Instant::now());
            groups = pace::groups(settings.escapes);
            loads = meter.load(&groups);
        }
        let heavy_of = |l: &pace::Load| l.cpu_pct >= f64::from(settings.heavy.cpu_pct) || l.write_rate >= f64::from(settings.heavy.write_mb_s) * 1e6;
        let seen: Vec<window::Seen> = groups
            .iter()
            .zip(&loads)
            .map(|(g, l)| window::Seen { class: Class::Agent, heavy: heavy_of(l), write_rate: l.write_rate, duty: shares.get(&g.dir).copied().unwrap_or(100) })
            .collect();
        let planned = window::plan(&settings, &control, now, &seen);
        let before = shares.len();
        shares = if SHARED.standby.load(Ordering::Acquire) { HashMap::new() } else { groups.iter().zip(&planned).filter(|(_, d)| **d < 100).map(|(g, d)| (g.dir.clone(), *d)).collect() };
        if shares.len() != before {
            tracing::info!(paced = shares.len(), window = control.window(), "headroom: work paced");
        }
        pacer.set(shares.clone(), Duration::from_millis(u64::from(settings.pacing.period_ms)));
        // Admission: what's heavy now, and who holds a place.
        let wall = crate::util::now_ms();
        let (room, held, starting) = {
            let mut a = SHARED.admit.lock();
            a.allowed = control.allowed();
            a.heavy = groups.iter().zip(&seen).filter(|(_, s)| s.heavy).filter_map(|(g, _)| g.session.clone()).collect();
            a.heavy_others = groups.iter().zip(&seen).filter(|(g, s)| s.heavy && g.session.is_none()).count();
            let heavy = a.heavy.clone();
            a.reserved.retain(|sess, until| *until > wall && !heavy.contains(sess));
            let used = a.heavy.len() + a.heavy_others + a.reserved.len();
            let room = !a.waiting.is_empty() && used < a.allowed && healthy;
            // Held sessions are asked again on the next room; ones gone quiet are let go.
            a.waiting.retain(|_, since| wall - *since < 600_000);
            (room, a.waiting.len(), a.reserved.len())
        };
        if room {
            SHARED.room.notify(usize::MAX);
        }
        let status = Status {
            settings: settings.fingerprint(),
            enabled: settings.enabled,
            pacing: pace::how(),
            window: (control.window() * 10.0).round() / 10.0,
            allowed: control.allowed(),
            healthy,
            targets: probes.targets.iter().zip(&verdicts).map(|(t, v)| TargetStatus { name: t.name.clone(), probe: t.probe.clone(), goal_ms: t.goal_ms, verdict: v.clone() }).collect(),
            groups: groups
                .iter()
                .zip(&loads)
                .zip(&seen)
                .filter(|((g, _), s)| s.heavy || shares.contains_key(&g.dir))
                .map(|((g, l), s)| GroupStatus {
                    label: g.label.clone(),
                    session: g.session.clone(),
                    cpu_pct: l.cpu_pct.round() as u32,
                    write_mb_s: (l.write_rate / 1e6).round() as u32,
                    heavy: s.heavy,
                    share: shares.get(&g.dir).copied().unwrap_or(100),
                })
                .collect(),
            held,
            starting,
        };
        // Told at once when what it decides changes; latencies at most once a second.
        let decided = |s: &Status| (s.enabled, s.allowed, s.healthy, s.held, s.groups.iter().map(|g| (g.label.clone(), g.share)).collect::<Vec<_>>());
        let new_decision = last_status.as_ref().is_none_or(|l| decided(l) != decided(&status));
        if new_decision || (reported.elapsed() >= Duration::from_secs(1) && last_status.as_ref() != Some(&status)) {
            reported = Instant::now();
            *SHARED.latest.lock() = serde_json::to_string(&status).ok();
            SHARED.changed.notify();
            last_status = Some(status);
        }
    }
    drop(pacer);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_runner_every_session_is_let_in() {
        // No `start` in this test process: as before headroom.
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
        let off = Settings { automatic: false, ..Settings::default() };
        assert!(targets(&off, &ctx).is_empty());
    }
}
