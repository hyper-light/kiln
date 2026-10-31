//! Where each app stands, as Ship's Deploys tab shows it (docs/ci.md W26-R24): its stand in
//! Mission control's words, the word that says it, the one line of what it's doing now, and the
//! three pips of its latest deploy (Sync, Health, Soak); its components, grouped by the workload
//! that owns them or else by what they are; a rollout's steps; its history, its deploys and its
//! source's commits as one list, any of which a rollback can take it to; and why each of the
//! strip's verbs can't apply, where one can't.
//!
//! The server reads KilnCD's state (each app's status, history, rollback pin and rollouts, the live
//! tree, its source's log) into these. Pure: the time is passed in, nothing is read or written
//! here.

use std::collections::{BTreeMap, VecDeque};

use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;

use crate::engine::{OpPhase, OpStatus};
use crate::journey::{Glyph, Stand};
use crate::model::{Health, Sync};
use crate::rollout::{State, Step, Verdict};
use crate::state::{Deployment, Pin, RolloutRecord};
use crate::tree::{self, Node};

/// How long a deploy that came up healthy soaks before it counts as held: kiln's guard's window.
pub const SOAK_MS: u64 = 10 * 60 * 1000;

/// Most lines an app's history holds.
pub const HISTORY: usize = 60;

/// What the server knows of one app.
#[derive(Clone, Copy, Debug)]
pub struct App<'a> {
    pub sync: Sync,
    pub health: Health,
    /// Its sync under way, or its last.
    pub operation: Option<&'a OpStatus>,
    /// Why its status is what it is: a render that failed, a cluster not reached.
    pub message: Option<&'a str>,
    /// Why its sources don't read.
    pub error: Option<&'a str>,
    /// Argo CD deploys it; kiln only compares.
    pub shadow: bool,
    pub auto_sync: bool,
    /// Its deployments, oldest first.
    pub history: &'a VecDeque<Deployment>,
    /// Where a rollback holds it.
    pub pin: Option<&'a Pin>,
    /// Its rollout under way, else its newest.
    pub rollout: Option<&'a RolloutRecord>,
    /// It deploys a chart's version, not a commit of a repository of its own.
    pub chart: bool,
    /// How many commits its source's revision has after the one that runs, as far as was looked.
    pub ahead: Option<u32>,
    /// The revision its inputs are at now.
    pub revision: Option<&'a str>,
}

/// One deploy: its sync's record, and the guard's word on it after, when there is one (the two
/// share an op).
#[derive(Clone, Copy, Debug)]
pub struct Deploy<'a> {
    pub sync: &'a Deployment,
    pub word: Option<&'a Deployment>,
}

impl<'a> Deploy<'a> {
    /// Its number in the app's history: its sync's.
    pub fn id(&self) -> u64 {
        self.sync.id
    }

    /// Its sync succeeded, and the guard's word, if any, held.
    pub fn succeeded(&self) -> bool {
        self.sync.succeeded && self.word.is_none_or(|w| w.succeeded)
    }

    /// Its last record: the guard's word, else its sync's.
    pub fn last(&self) -> &'a Deployment {
        self.word.unwrap_or(self.sync)
    }
}

/// An app's deploys, oldest first.
pub fn deploys(history: &VecDeque<Deployment>) -> Vec<Deploy<'_>> {
    let mut out: Vec<Deploy<'_>> = Vec::new();
    for d in history {
        match out.last_mut().filter(|x| x.sync.op == d.op && x.sync.revision == d.revision) {
            Some(x) => x.word = Some(d),
            None => out.push(Deploy { sync: d, word: None }),
        }
    }
    out
}

/// The three stages of a deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Sync,
    Health,
    Soak,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pip {
    pub stage: Phase,
    pub state: Glyph,
    /// How far a soak has come, 0 to 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
}

/// Where an app stands, what it's doing now, and its latest deploy's pips.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Standing {
    pub stand: Stand,
    pub word: String,
    pub line: String,
    pub pips: Vec<Pip>,
    /// Who did what it shows: a person, `agent:…`, or what kiln did on its own (`auto-sync`,
    /// `self-heal`, `auto-rollback`, `promotion`).
    pub by: Option<String>,
    /// When (ms since the epoch); 0 when nothing has happened to it yet.
    pub at_ms: u64,
    /// What runs: its commit, short, or its chart's version.
    pub runs: Option<String>,
}

/// A revision as people read it: a commit's first seven hex digits, else as it is (a version).
pub fn short(rev: &str) -> String {
    if (7..=64).contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit()) { rev.chars().take(7).collect() } else { rev.to_owned() }
}

/// What kiln did on its own, by a deployment's trigger.
fn kiln_did(trigger: &str) -> Option<&'static str> {
    match trigger {
        "auto" => Some("auto-sync"),
        "self-heal" => Some("self-heal"),
        "rollback" => Some("auto-rollback"),
        "promotion" => Some("promotion"),
        _ => None,
    }
}

fn by_of(d: &Deployment) -> Option<String> {
    d.by.clone().or_else(|| kiln_did(&d.trigger).map(str::to_owned))
}

/// An app's health as a glyph.
pub fn health_glyph(h: Health) -> Glyph {
    match h {
        Health::Healthy => Glyph::Healthy,
        Health::Progressing => Glyph::Running,
        Health::Degraded => Glyph::Degraded,
        Health::Missing => Glyph::Failed,
        Health::Suspended => Glyph::Waiting,
        Health::Unknown => Glyph::None,
    }
}

fn health_word(h: Health) -> &'static str {
    match h {
        Health::Healthy => "healthy",
        Health::Progressing => "progressing",
        Health::Degraded => "degraded",
        Health::Missing => "missing",
        Health::Suspended => "suspended",
        Health::Unknown => "health unknown",
    }
}

fn pips(sync: Glyph, health: Glyph, soak: Glyph, p: Option<f64>) -> Vec<Pip> {
    vec![Pip { stage: Phase::Sync, state: sync, p: None }, Pip { stage: Phase::Health, state: health, p: None }, Pip { stage: Phase::Soak, state: soak, p }]
}

/// A deploy's pips: how its sync went, its health (the app's, for the deploy that runs now), and
/// its soak: the guard's word where it gave one, else healthy for [`SOAK_MS`] after the sync.
fn pips_of(d: Option<&Deploy<'_>>, health: Health, now_ms: u64) -> Vec<Pip> {
    let Some(d) = d else { return pips(Glyph::None, Glyph::None, Glyph::None, None) };
    if !d.sync.succeeded {
        return pips(Glyph::Failed, Glyph::None, Glyph::None, None);
    }
    if let Some(w) = d.word {
        return if w.succeeded { pips(Glyph::Passed, Glyph::Healthy, Glyph::Passed, None) } else { pips(Glyph::Passed, Glyph::Failed, Glyph::None, None) };
    }
    let h = health_glyph(health);
    if h != Glyph::Healthy {
        return pips(Glyph::Passed, h, Glyph::None, None);
    }
    let since = now_ms.saturating_sub(d.sync.at_ms);
    if since < SOAK_MS { pips(Glyph::Passed, h, Glyph::Soaking, Some(since as f64 / SOAK_MS as f64)) } else { pips(Glyph::Passed, h, Glyph::Passed, None) }
}

/// A message's first line, at most 90 characters.
fn first_line(s: &str) -> String {
    let l = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    if l.chars().count() > 90 { format!("{}…", l.chars().take(89).collect::<String>()) } else { l.to_owned() }
}

/// How a deploy failed, as its line says it: its message where that says so itself (`failed its
/// health check`), else `failed: …`, else `failed`.
fn failed(message: &str) -> String {
    let why = first_line(message);
    match why.as_str() {
        "" => "failed".to_owned(),
        w if w.starts_with("failed") => w.to_owned(),
        w => format!("failed: {w}"),
    }
}

/// How long, in the fewest words: `40s`, `3m`, `12h`, `6d`.
pub fn span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3_600 => format!("{}m", s / 60),
        3_600..86_400 => format!("{}h", s / 3_600),
        _ => format!("{}d", s / 86_400),
    }
}

fn running<'a>(a: &App<'a>) -> Option<&'a OpStatus> {
    a.operation.filter(|o| matches!(o.phase, OpPhase::DryRun | OpPhase::Running | OpPhase::Failing | OpPhase::Terminating))
}

/// Where an app stands now (docs/ci.md W26-R24). The first that holds, in this order: Argo CD
/// deploys it; its sources don't read, or don't render, or its cluster isn't reached; a sync runs;
/// its rollout waits for a person, or runs; a rollback holds it; its latest deploy failed; it's
/// degraded or missing; it's out of sync (needing a person unless auto-sync syncs it); it's
/// progressing or suspended; it was never compared; else it's synced, soaking first.
pub fn standing(a: &App<'_>, now_ms: u64) -> Standing {
    let all = deploys(a.history);
    let latest = all.last();
    let current = all.iter().rev().find(|d| d.succeeded());
    let runs = current.and_then(|d| d.sync.revision.as_deref()).map(short);
    let runs_line = match (&runs, current) {
        (Some(r), _) if a.chart => format!("chart {r}"),
        (Some(r), Some(c)) => format!("#{} runs {r}", c.id()),
        (None, Some(c)) => format!("#{} runs", c.id()),
        _ => "never deployed".to_owned(),
    };
    let ahead = a.ahead.filter(|n| *n > 0).map(|n| format!("trunk is {n} ahead"));
    let (by, at_ms) = latest.map_or((None, 0), |d| (by_of(d.last()), d.last().at_ms));
    let out = |stand: Stand, word: &str, line: String, pips: Vec<Pip>| Standing { stand, word: word.to_owned(), line, pips, by: by.clone(), at_ms, runs: runs.clone() };
    let latest_pips = || pips_of(latest, a.health, now_ms);
    if a.shadow {
        return out(Stand::Waiting, "Argo CD's", "Argo CD deploys it and kiln compares it: take it over to deploy it here".into(), latest_pips());
    }
    if let Some(e) = a.error {
        return out(Stand::Failing, "Can't read", first_line(e), latest_pips());
    }
    if let Some(m) = a.message.filter(|m| m.starts_with("render failed")) {
        return out(Stand::Failing, "Can't render", first_line(m), pips(Glyph::Error, Glyph::None, Glyph::None, None));
    }
    if a.sync == Sync::Unknown
        && let Some(m) = a.message
    {
        return out(Stand::Failing, "Can't compare", first_line(m), latest_pips());
    }
    if let Some(op) = running(a) {
        let to = a.revision.map(short).unwrap_or_default();
        let word = if op.phase == OpPhase::Terminating { "Stopping" } else { "Syncing" };
        let mut line = format!("syncing {to}").trim_end().to_owned();
        if !op.message.is_empty() && op.message != "syncing" {
            line = format!("{line} · {}", first_line(&op.message));
        }
        let mut s = out(Stand::Shipping, word, line, pips(Glyph::Running, Glyph::None, Glyph::None, None));
        s.by = op.by.clone().or_else(|| kiln_did(op.trigger.as_str()).map(str::to_owned));
        s.at_ms = op.started_ms;
        return s;
    }
    if let Some(r) = a.rollout.filter(|r| !r.rollout.finished()) {
        let n = latest.map(|d| format!("#{}", d.id())).unwrap_or_else(|| r.revision.as_deref().map(short).unwrap_or_default());
        let back = r.back_to.map(|b| format!(" · #{b} runs")).unwrap_or_default();
        if let State::Paused { why } = &r.rollout.state {
            // What it waits for, in a person's words: an approval, a promote, else the rollout's own.
            let waits = match (r.rollout.plan.steps.get(r.rollout.step), r.rollout.held) {
                (_, true) => "is paused: Resume lets it go on".to_owned(),
                (Some(Step::Approve), _) => "waits for your approval".to_owned(),
                (Some(Step::Pause { ms: None }), _) => "waits for you to promote it".to_owned(),
                _ => format!("waits for you: {}", first_line(why)),
            };
            return out(Stand::NeedsYou, "Needs you", format!("{n} {waits}{back}"), pips(Glyph::Requested, Glyph::None, Glyph::None, None));
        }
        let steps = r.rollout.plan.steps.len();
        let at = (r.rollout.step + 1).min(steps.max(1));
        return out(Stand::Shipping, "Rolling out", format!("{n} · {}% · step {at} of {}", r.rollout.weight, steps.max(1)), pips(Glyph::Passed, Glyph::Running, Glyph::None, None));
    }
    if let Some(pin) = a.pin {
        let target = if pin.to > 0 { format!("#{}", pin.to) } else { pin.revision.as_deref().map(short).unwrap_or_else(|| "a commit".into()) };
        // What it went back from: the newest deploy since that failed, rollbacks left out.
        let cause = all.iter().rev().find(|d| !d.succeeded() && d.sync.trigger != "rollback" && (pin.to == 0 || d.id() > pin.to));
        let (line, p) = match cause {
            Some(c) => {
                let line = format!("#{} {} · back on {target}", c.id(), failed(&c.last().message));
                (line, pips(if c.sync.succeeded { Glyph::Passed } else { Glyph::Failed }, if c.sync.succeeded { Glyph::Failed } else { Glyph::None }, Glyph::None, None))
            }
            None => (format!("back on {target} · held there until git changes it"), latest_pips()),
        };
        let mut s = out(Stand::RolledBack, "Rolled back", line, p);
        s.by = pin.by.clone().or(s.by);
        s.at_ms = pin.at_ms.max(s.at_ms);
        return s;
    }
    if let Some(l) = latest.filter(|d| !d.succeeded()) {
        let word = if l.sync.succeeded { "Failed its checks" } else { "Sync failed" };
        let mut line = format!("#{} {}", l.id(), failed(&l.last().message));
        if let Some(c) = current {
            line = format!("{line} · #{} runs", c.id());
        }
        return out(Stand::Failing, word, line, latest_pips());
    }
    let health = health_word(a.health);
    // Missing before its first deploy is what it is to be waiting for one.
    if a.health == Health::Degraded || (a.health == Health::Missing && current.is_some()) {
        let word = if a.health == Health::Degraded { "Degraded" } else { "Missing" };
        let why = a.message.map(first_line).filter(|m| !m.is_empty()).map(|m| format!(": {m}")).unwrap_or_default();
        return out(Stand::Failing, word, format!("{runs_line} · {health}{why}"), latest_pips());
    }
    if a.sync == Sync::OutOfSync {
        let next = match (a.auto_sync, current, a.revision) {
            (true, _, _) => "auto-sync syncs it".to_owned(),
            (false, Some(_), _) => health.to_owned(),
            (false, None, Some(r)) => format!("{} waits for a sync", short(r)),
            (false, None, None) => "waits for its first sync".to_owned(),
        };
        let parts: Vec<String> = [Some(runs_line.clone()), ahead.clone(), Some(next)].into_iter().flatten().collect();
        let stand = if a.auto_sync { Stand::Waiting } else { Stand::NeedsYou };
        return out(stand, "Out of sync", parts.join(" · "), latest_pips());
    }
    match (a.health, a.sync) {
        (Health::Progressing, _) => out(Stand::Shipping, "Progressing", format!("{runs_line} · progressing"), latest_pips()),
        (Health::Suspended, _) => out(Stand::Waiting, "Suspended", format!("{runs_line} · suspended"), latest_pips()),
        (_, Sync::Unknown) => out(Stand::Waiting, "Comparing", "not compared yet".into(), latest_pips()),
        _ => {
            let p = latest_pips();
            match p.iter().find(|x| x.stage == Phase::Soak && x.state == Glyph::Soaking).and(latest) {
                Some(d) => {
                    let left = SOAK_MS.saturating_sub(now_ms.saturating_sub(d.sync.at_ms));
                    out(Stand::Shipping, "Soaking", format!("{runs_line} · {health} · soaks {} more", span(left)), p)
                }
                None => out(Stand::Shipped, "Synced", format!("{runs_line} · {health}"), p),
            }
        }
    }
}

/// Where an app sorts by its stand and word: what needs someone first. A rollout waiting on a
/// person goes before what failed or went back; an app out of sync for a person to sync goes after
/// them, as trunk being ahead isn't something gone wrong.
pub fn rank(stand: Stand, word: &str) -> u8 {
    match stand {
        Stand::NeedsYou if word == "Out of sync" => 3,
        Stand::NeedsYou => 0,
        Stand::Failing => 1,
        Stand::RolledBack => 2,
        Stand::Shipping => 4,
        Stand::Waiting => 5,
        Stand::Shipped => 6,
        _ => 7,
    }
}

/// What a project's heading says of its apps (each one's stand and word): how many, and how many
/// stand as its most pressing one does (`2 apps · 1 waits on you`, `1 app · rolled back`); and
/// whether that one needs someone.
pub fn project_say(apps: &[(Stand, &str)]) -> (String, bool) {
    let n = apps.len();
    let count = if n == 1 { "1 app".to_owned() } else { format!("{n} apps") };
    let Some((top, word)) = apps.iter().min_by_key(|(s, w)| rank(*s, w)).copied() else { return (count, false) };
    let k = apps.iter().filter(|(s, w)| *s == top && *w == word).count();
    let phrase = match word {
        "Needs you" if k == 1 => "waits on you".to_owned(),
        "Needs you" => "wait on you".to_owned(),
        w => w.to_lowercase(),
    };
    let say = match (n, k) {
        (1, _) => format!("{count} · {phrase}"),
        (n, k) if n == k => format!("{count} · all {phrase}"),
        _ => format!("{count} · {k} {phrase}"),
    };
    (say, matches!(top, Stand::NeedsYou | Stand::Failing | Stand::RolledBack))
}

/// One object of an app, as a line: its key as the API's routes take it (`apps~Deployment~web~api`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub key: String,
    pub group: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub state: Glyph,
    /// What its line says (`up 3m · 4 restarts`, `ClusterIP · 3 ports`, `differs from git`).
    pub v: String,
    /// It has logs to read: a pod, what runs pods, a Compose project, a host's service.
    pub logs: bool,
    /// It differs from git: what a sync would change in it reads as a diff.
    pub differs: bool,
    /// The node a pod runs on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The container a pod's logs read unless another is asked for: its first that isn't an init
    /// container (`cd::live::container_of`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

/// An app's objects of one owner or one sort.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Group {
    /// `Deployment kiln`, or what its objects are (`Network`, `Config`, `Storage`, `Jobs`, `Access`,
    /// `Other`, a target's kind).
    pub name: String,
    pub note: String,
    /// The workload it's of, as a line of its own.
    pub owner: Option<Item>,
    pub items: Vec<Item>,
}

fn is_workload(group: &str, kind: &str) -> bool {
    matches!((group, kind), ("apps", "Deployment" | "StatefulSet" | "DaemonSet") | ("batch", "CronJob") | ("argoproj.io", "Rollout"))
}

fn has_logs(group: &str, kind: &str) -> bool {
    matches!(
        (group, kind),
        ("", "Pod") | ("apps", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet") | ("batch", "Job" | "CronJob") | ("argoproj.io", "Rollout") | ("compose.kiln.dev", _) | ("host.kiln.dev", _)
    )
}

/// What sort a top-level object is, and where its sort goes among the others.
fn sort_of(n: &Node) -> (u8, String) {
    let (g, k) = (n.group.as_str(), n.kind.as_str());
    match (g, k) {
        _ if n.hook => (4, "Jobs".into()),
        ("", "Service" | "Endpoints") | ("networking.k8s.io", _) | ("gateway.networking.k8s.io", _) | ("discovery.k8s.io", _) => (1, "Network".into()),
        ("", "ConfigMap" | "Secret") => (2, "Config".into()),
        ("", "PersistentVolumeClaim" | "PersistentVolume") | ("storage.k8s.io", _) => (3, "Storage".into()),
        ("batch", "Job") => (4, "Jobs".into()),
        ("", "ServiceAccount") | ("rbac.authorization.k8s.io", _) => (5, "Access".into()),
        (g, k) if g.ends_with(".kiln.dev") => (0, k.to_owned()),
        _ => (6, "Other".into()),
    }
}

fn info<'a>(n: &'a Node, name: &str) -> Option<&'a str> {
    n.info.iter().find(|i| i.name == name).map(|i| i.value.as_str()).filter(|v| !v.is_empty())
}

/// A ready count's whole (`2/3` → 3).
fn wanted(n: &Node) -> Option<u64> {
    n.ready.as_deref().and_then(|r| r.split_once('/')).and_then(|(_, w)| w.trim().parse().ok())
}

fn age_ms(n: &Node, now_ms: u64) -> Option<u64> {
    let at = n.created_at.as_deref().and_then(tree::rfc3339_secs)?;
    Some(now_ms.saturating_sub(u64::try_from(at).ok()? * 1000))
}

fn differs(n: &Node) -> bool {
    n.managed && (n.missing || n.requires_pruning || n.sync.as_deref() == Some("OutOfSync"))
}

fn node_health(n: &Node) -> Glyph {
    match n.health.as_deref() {
        Some("Healthy") => Glyph::Healthy,
        Some("Progressing") => Glyph::Running,
        Some("Degraded") => Glyph::Degraded,
        Some("Missing") => Glyph::Failed,
        Some("Suspended") => Glyph::Waiting,
        _ if n.managed && n.sync.as_deref() == Some("Synced") => Glyph::Healthy,
        _ => Glyph::None,
    }
}

/// An object's glyph: what a sync would do to it, else how it runs.
fn item_state(n: &Node) -> Glyph {
    if n.requires_pruning && n.managed {
        return Glyph::Superseded;
    }
    if n.missing {
        return Glyph::Queued;
    }
    if differs(n) {
        return Glyph::Changes;
    }
    match (n.group.as_str(), n.kind.as_str()) {
        ("", "Pod") => match n.status.as_deref().unwrap_or_default() {
            "Running" => Glyph::Running,
            "Completed" | "Succeeded" => Glyph::Passed,
            "Terminating" => Glyph::Superseded,
            s if s.is_empty() || s == "Pending" || s == "ContainerCreating" || s == "PodInitializing" || s.starts_with("Init:") => Glyph::Queued,
            _ => Glyph::Failed,
        },
        ("apps", "ReplicaSet") if wanted(n) == Some(0) => Glyph::Superseded,
        ("batch", "Job") => match n.health.as_deref() {
            Some("Healthy") => Glyph::Passed,
            Some("Degraded") => Glyph::Failed,
            Some("Progressing") => Glyph::Running,
            _ => Glyph::None,
        },
        _ => node_health(n),
    }
}

fn plural(n: u64, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

/// What an object's line says.
fn item_value(n: &Node, now_ms: u64) -> String {
    if n.requires_pruning && n.managed {
        return "git dropped it".into();
    }
    if n.missing {
        return "not in the cluster yet".into();
    }
    if differs(n) {
        return "differs from git".into();
    }
    let age = age_ms(n, now_ms).map(span);
    match (n.group.as_str(), n.kind.as_str()) {
        ("", "Pod") => {
            let status = n.status.clone().unwrap_or_default();
            let mut v = match (status.as_str(), &age) {
                ("Running", Some(a)) => format!("up {a}"),
                ("Running", None) => "running".into(),
                (s, _) => s.to_owned(),
            };
            if let Some(r) = n.restarts.filter(|r| *r > 0) {
                v = format!("{v} · {}", plural(r.unsigned_abs(), "restart"));
            }
            v
        }
        ("apps", "ReplicaSet") => wanted(n).map(|w| plural(w, "pod")).unwrap_or_default(),
        ("apps", "Deployment" | "StatefulSet" | "DaemonSet") | ("argoproj.io", "Rollout") => n.ready.as_deref().map(|r| format!("{r} ready")).unwrap_or_default(),
        ("", "Service") => {
            let ports = info(n, "Ports").map(|p| p.split(", ").filter(|x| !x.is_empty()).count() as u64).unwrap_or(0);
            [info(n, "Type").map(str::to_owned), (ports > 0).then(|| plural(ports, "port"))].into_iter().flatten().collect::<Vec<_>>().join(" · ")
        }
        ("networking.k8s.io", "Ingress") => info(n, "Hosts").unwrap_or_default().to_owned(),
        ("", "PersistentVolumeClaim") => [info(n, "Phase"), info(n, "Capacity")].into_iter().flatten().collect::<Vec<_>>().join(" · "),
        ("batch", "Job") => match (n.health.as_deref(), &age) {
            (Some("Healthy"), Some(a)) => format!("completed · made {a} ago"),
            (Some("Healthy"), None) => "completed".into(),
            (Some("Degraded"), _) => "failed".into(),
            _ => "running".into(),
        },
        ("batch", "CronJob") => info(n, "Schedule").unwrap_or_default().to_owned(),
        _ => match n.sync.as_deref() {
            Some("Synced") => "synced".into(),
            _ => n.health.as_deref().map(str::to_lowercase).unwrap_or_default(),
        },
    }
}

fn item(n: &Node, now_ms: u64) -> Item {
    Item {
        key: format!("{}~{}~{}~{}", n.group, n.kind, n.namespace, n.name),
        group: n.group.clone(),
        kind: n.kind.clone(),
        namespace: n.namespace.clone(),
        name: n.name.clone(),
        state: item_state(n),
        v: item_value(n, now_ms),
        logs: has_logs(&n.group, &n.kind),
        differs: differs(n),
        node: n.node.clone(),
        container: n.containers.iter().find(|c| !c.init).or(n.containers.first()).map(|c| c.name.clone()),
    }
}

/// What a workload's group says of it: ready of wanted, and how it differs or ails.
fn owner_note(n: &Node) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(r) = n.ready.as_deref().filter(|r| !r.is_empty()) {
        parts.push(format!("{r} ready"));
    }
    if n.missing {
        parts.push("not in the cluster yet".into());
    } else if differs(n) {
        parts.push("out of sync".into());
    }
    match n.health.as_deref() {
        Some("Degraded") => parts.push("degraded".into()),
        Some("Progressing") => parts.push("progressing".into()),
        _ => {}
    }
    parts.join(" · ")
}

/// What a sort's group says: how many of its objects differ from git, else that it's synced.
fn sort_note(items: &[Item], managed: usize) -> String {
    let k = items.iter().filter(|i| i.differs).count();
    match (k, managed) {
        (0, 0) => String::new(),
        (0, _) => "synced".into(),
        (1, _) => "1 differs from git".into(),
        (k, _) => format!("{k} differ from git"),
    }
}

/// An app's components (its tree's nodes): each workload a group of its own, its pods (newest
/// first) and ReplicaSets under it; the rest by what they are, in a fixed order. A Service's
/// EndpointSlices and a workload's ControllerRevisions are left out, as people don't act on them.
pub fn components(nodes: &[Node], now_ms: u64) -> Vec<Group> {
    let by_uid: FxHashMap<&str, &Node> = nodes.iter().filter_map(|n| Some((n.uid.as_deref()?, n))).collect();
    let mut kids: FxHashMap<&str, Vec<&Node>> = FxHashMap::default();
    for n in nodes {
        for p in &n.parents {
            kids.entry(p.as_str()).or_default().push(n);
        }
    }
    let quiet = |n: &Node| matches!((n.group.as_str(), n.kind.as_str()), ("apps", "ControllerRevision") | ("discovery.k8s.io", "EndpointSlice"));
    let mut owners: Vec<Group> = Vec::new();
    let mut sorts: BTreeMap<(u8, String), (Vec<Item>, usize)> = BTreeMap::new();
    for n in nodes.iter().filter(|n| n.parents.iter().all(|p| !by_uid.contains_key(p.as_str()))) {
        if !is_workload(&n.group, &n.kind) {
            if quiet(n) {
                continue;
            }
            let e = sorts.entry(sort_of(n)).or_default();
            e.0.push(item(n, now_ms));
            e.1 += usize::from(n.managed);
            continue;
        }
        let mut under: Vec<&Node> = Vec::new();
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        let mut todo: Vec<&str> = n.uid.as_deref().into_iter().collect();
        while let Some(u) = todo.pop() {
            for k in kids.get(u).into_iter().flatten() {
                let Some(ku) = k.uid.as_deref() else { continue };
                if !seen.insert(ku) {
                    continue;
                }
                todo.push(ku);
                if !quiet(k) {
                    under.push(k);
                }
            }
        }
        let order = |k: &Node| match (k.group.as_str(), k.kind.as_str()) {
            ("", "Pod") => 0,
            ("apps", "ReplicaSet") => 1,
            ("batch", "Job") => 2,
            _ => 3,
        };
        under.sort_by(|a, b| order(a).cmp(&order(b)).then_with(|| b.created_at.cmp(&a.created_at)).then_with(|| a.name.cmp(&b.name)));
        owners.push(Group { name: format!("{} {}", n.kind, n.name), note: owner_note(n), owner: Some(item(n, now_ms)), items: under.into_iter().map(|k| item(k, now_ms)).collect() });
    }
    owners.sort_by(|a, b| a.name.cmp(&b.name));
    for ((_, name), (mut items, managed)) in sorts {
        // A Service before what routes to it.
        let first = |i: &Item| !(i.group.is_empty() && i.kind == "Service");
        items.sort_by(|a, b| first(a).cmp(&first(b)).then_with(|| a.kind.cmp(&b.kind)).then_with(|| a.name.cmp(&b.name)));
        owners.push(Group { note: sort_note(&items, managed), name, owner: None, items });
    }
    owners
}

/// A commit of the app's source, as its history lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub author: String,
    /// Unix seconds.
    pub at: i64,
    /// The deploy whose render it renders the same as.
    pub same_as: Option<u64>,
}

/// One line of an app's history: a deploy, or a commit of its source none ran.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    /// The deploy's number, when one ran it.
    pub deploy: Option<u64>,
    /// The commit, whole, when known; else what the deploy ran (a chart's version).
    pub sha: Option<String>,
    pub short: Option<String>,
    pub subject: String,
    /// Who deployed it, or who wrote it.
    pub by: Option<String>,
    /// When it was deployed, else when it was made (ms since the epoch).
    pub at_ms: u64,
    pub state: Glyph,
    /// It runs now.
    pub now: bool,
    /// An older deploy than what runs.
    pub dim: bool,
    /// Where Roll back takes the app to go to it: the deploy (`to`), else the commit
    /// (`revision`); neither for what runs now.
    pub to: Option<u64>,
    pub revision: Option<String>,
    /// The deploy whose render the commit renders the same as.
    pub same_as: Option<u64>,
}

/// A deploy's commit of source `source`, else its revision.
fn commit_of(d: &Deployment, source: usize) -> Option<&str> {
    d.commits.get(source).and_then(Option::as_deref).or(d.revision.as_deref())
}

fn same_commit(a: &str, b: &str) -> bool {
    a.len() >= 7 && b.len() >= 7 && (a.starts_with(b) || b.starts_with(a))
}

/// An app's history (docs/ci.md W26-R25): its source's commits newest first, each that a deploy
/// ran as that deploy (its newest), then the deploys of commits older than those, newest first;
/// at most [`HISTORY`] lines. What runs now carries `health`'s glyph; a failed deploy fails;
/// older deploys are dim. Each line but what runs says how Roll back goes to it.
pub fn history(history: &VecDeque<Deployment>, commits: &[Commit], source: usize, health: Glyph) -> Vec<Line> {
    let all = deploys(history);
    let current = all.iter().rev().find(|d| d.succeeded()).map(Deploy::id);
    let deploy_line = |d: &Deploy<'_>, subject: Option<&Commit>| {
        let sha = commit_of(d.sync, source).map(str::to_owned);
        let now = Some(d.id()) == current;
        let ok = d.succeeded();
        Line {
            deploy: Some(d.id()),
            short: sha.as_deref().map(short),
            subject: subject.map_or_else(|| first_line(&d.last().message), |c| c.subject.clone()),
            by: by_of(d.sync),
            at_ms: d.last().at_ms,
            state: if now {
                health
            } else if !ok {
                Glyph::Failed
            } else if d.sync.trigger == "rollback" {
                Glyph::RolledBack
            } else {
                Glyph::Superseded
            },
            now,
            dim: !now && current.is_some_and(|c| d.id() < c),
            to: (ok && !now).then_some(d.id()),
            revision: (!ok).then(|| sha.clone()).flatten().filter(|s| s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit())),
            same_as: None,
            sha,
        }
    };
    let mut shown: FxHashSet<u64> = FxHashSet::default();
    let mut out: Vec<Line> = Vec::new();
    for c in commits {
        match all.iter().rev().find(|d| commit_of(d.sync, source).is_some_and(|s| same_commit(s, &c.sha))) {
            Some(d) if shown.insert(d.id()) => out.push(deploy_line(d, Some(c))),
            Some(_) => {}
            None => out.push(Line {
                deploy: None,
                sha: Some(c.sha.clone()),
                short: Some(short(&c.sha)),
                subject: c.subject.clone(),
                by: Some(c.author.clone()).filter(|a| !a.is_empty()),
                at_ms: u64::try_from(c.at).unwrap_or(0) * 1000,
                state: Glyph::None,
                now: false,
                dim: false,
                to: None,
                revision: Some(c.sha.clone()),
                same_as: c.same_as,
            }),
        }
    }
    for d in all.iter().rev().filter(|d| !shown.contains(&d.id())) {
        out.push(deploy_line(d, None));
    }
    out.truncate(HISTORY);
    out
}

/// One step of a rollout, as a line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepLine {
    /// 1-based.
    pub n: usize,
    pub state: Glyph,
    pub what: String,
    pub v: String,
}

/// A rollout as the pane shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rollout {
    pub id: String,
    /// What it rolls out.
    pub revision: Option<String>,
    pub state: Glyph,
    /// `20% · step 2 of 4 · checks passing`.
    pub note: String,
    pub steps: Vec<StepLine>,
    pub under_way: bool,
    /// What it waits for a person to do, while it does.
    pub waits: Option<String>,
}

fn step_what(s: &Step) -> String {
    match s {
        Step::Weight { weight } => format!("weight {weight}%"),
        Step::Pause { ms: Some(ms) } => format!("wait {}", span(*ms)),
        Step::Pause { ms: None } => "wait for a promote".into(),
        Step::Checks { names, .. } => format!("checks: {}", names.join(", ")),
        Step::Watch { names, window_ms } => format!("watch {} for {}", names.join(", "), span(*window_ms)),
        Step::Approve => "a person approves".into(),
    }
}

/// How a step's checks are going.
fn checks_word(r: &crate::rollout::Rollout) -> &'static str {
    let v: Vec<Option<Verdict>> = r.checks.iter().chain(&r.background).map(|c| c.verdict).collect();
    if v.iter().any(|x| matches!(x, Some(Verdict::Fail | Verdict::Error))) {
        "failing"
    } else if !v.is_empty() && v.iter().all(|x| *x == Some(Verdict::Pass)) {
        "passing"
    } else {
        "measuring"
    }
}

/// A rollout's steps and where it stands, `now_ms` for what a timed pause has left.
pub fn rollout(id: &str, rec: &RolloutRecord, now_ms: u64) -> Rollout {
    let r = &rec.rollout;
    let total = r.plan.steps.len();
    let gone = matches!(r.state, State::Aborted { .. } | State::Superseded { .. });
    let steps: Vec<StepLine> = r
        .plan
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (state, v) = if i < r.step || r.state == State::Succeeded {
                (Glyph::Passed, "done".to_owned())
            } else if i == r.step {
                match (&r.state, s) {
                    (State::Aborted { why }, _) => (Glyph::Failed, first_line(why)),
                    (State::Superseded { .. }, _) => (Glyph::Superseded, "superseded".to_owned()),
                    (State::Paused { why }, _) => (Glyph::Requested, first_line(why)),
                    (_, Step::Pause { ms: Some(ms) }) => (Glyph::Running, format!("{} left", span(ms.saturating_sub(now_ms.saturating_sub(r.step_ms))))),
                    (_, Step::Checks { .. } | Step::Watch { .. }) => (Glyph::Running, checks_word(r).to_owned()),
                    (_, Step::Weight { .. }) if r.moving.is_some() => (Glyph::Running, "moving traffic".to_owned()),
                    _ => (Glyph::Running, "now".to_owned()),
                }
            } else if gone {
                (Glyph::Superseded, String::new())
            } else {
                (Glyph::None, if i == r.step + 1 { "next".to_owned() } else { String::new() })
            };
            StepLine { n: i + 1, state, what: step_what(s), v }
        })
        .collect();
    let (state, said) = match &r.state {
        State::Running => (Glyph::Running, if r.checks.is_empty() && r.background.is_empty() { "running".to_owned() } else { format!("checks {}", checks_word(r)) }),
        State::Paused { why } => (Glyph::Requested, first_line(why)),
        State::Succeeded => (Glyph::Passed, "done".to_owned()),
        State::Aborted { why } => (Glyph::Failed, format!("aborted: {}", first_line(why))),
        State::Superseded { by } => (Glyph::Superseded, format!("superseded by {}", short(by))),
    };
    let at = (r.step + 1).min(total.max(1));
    Rollout {
        id: id.to_owned(),
        revision: rec.revision.as_deref().map(short),
        state,
        note: format!("{}% · step {at} of {} · {said}", r.weight, total.max(1)),
        steps,
        under_way: !r.finished(),
        waits: match &r.state {
            State::Paused { why } => Some(why.clone()),
            _ => None,
        },
    }
}

/// Why each of the strip's verbs can't apply to an app now; a verb missing from it can. `back`:
/// there's something to roll back to (a deployment before, or a commit of its source).
pub fn why_not(a: &App<'_>, back: bool) -> BTreeMap<&'static str, String> {
    let mut out: BTreeMap<&'static str, String> = BTreeMap::new();
    let syncing = running(a).is_some();
    let rollout = a.rollout.filter(|r| !r.rollout.finished());
    if a.shadow {
        for v in ["sync", "rollback", "auto_sync", "follow"] {
            out.insert(v, "Argo CD deploys it: take it over first".into());
        }
    }
    if syncing {
        for v in ["sync", "rollback"] {
            out.entry(v).or_insert_with(|| "a sync is running: stop it first".into());
        }
    } else {
        out.insert("stop", "no sync is running".into());
    }
    if let Some(m) = a.message.filter(|m| m.starts_with("render failed")) {
        out.entry("sync").or_insert_with(|| format!("its {}", first_line(m)));
    }
    if !back {
        out.entry("rollback").or_insert_with(|| "nothing to go back to yet".into());
    }
    if rollout.is_none() {
        for v in ["promote", "abort"] {
            out.insert(v, "no rollout is under way".into());
        }
    }
    let approval = rollout.is_some_and(|r| matches!(&r.rollout.state, State::Paused { .. }) && matches!(r.rollout.plan.steps.get(r.rollout.step), Some(Step::Approve)));
    if !approval {
        for v in ["approve", "deny"] {
            out.insert(v, "nothing waits for an approval".into());
        }
    }
    if a.pin.is_none() {
        out.entry("follow").or_insert_with(|| "it follows git already".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Trigger;
    use crate::rollout::{Plan, Rollout as Machine};

    const NOW: u64 = 1_793_400_000_000;
    const MIN: u64 = 60_000;

    #[allow(clippy::too_many_arguments)]
    fn dep(id: u64, op: u64, rev: &str, ok: bool, trigger: &str, by: Option<&str>, at_ms: u64, message: &str) -> Deployment {
        Deployment {
            app: "default/web".into(),
            op,
            revision: Some(rev.into()),
            trigger: trigger.into(),
            by: by.map(str::to_owned),
            succeeded: ok,
            message: message.into(),
            at_ms,
            id,
            commits: vec![Some(rev.into())],
            ..Deployment::default()
        }
    }

    const C1: &str = "1111111111111111111111111111111111111111";
    const C2: &str = "2222222222222222222222222222222222222222";
    const C3: &str = "3333333333333333333333333333333333333333";
    const C4: &str = "4444444444444444444444444444444444444444";

    fn app<'a>(history: &'a VecDeque<Deployment>) -> App<'a> {
        App { sync: Sync::Synced, health: Health::Healthy, operation: None, message: None, error: None, shadow: false, auto_sync: false, history, pin: None, rollout: None, chart: false, ahead: None, revision: Some(C2) }
    }

    fn glyphs(s: &Standing) -> Vec<Glyph> {
        s.pips.iter().map(|p| p.state).collect()
    }

    fn record(steps: Vec<Step>, state: State, step: usize, weight: u8) -> RolloutRecord {
        let mut r = Machine::new(Plan { steps, ..Plan::default() }, NOW - 4 * MIN);
        r.state = state;
        r.step = step;
        r.weight = weight;
        RolloutRecord {
            app: "default/web".into(),
            environment: Some("production".into()),
            target: "aws-lambda:prod/us-east-1".into(),
            key: "fn".into(),
            object: serde_json::Value::Null,
            revision: Some(C3.into()),
            previous: Some(C2.into()),
            back_to: Some(2),
            blue: serde_json::Value::Null,
            green: serde_json::Value::Null,
            preview_url: None,
            stable_url: None,
            rollout: r,
            checks: BTreeMap::new(),
            started_ms: NOW - 4 * MIN,
            updated_ms: NOW,
            version: 1,
        }
    }

    #[test]
    fn w26_r24_each_app_stands_as_its_latest_deploy_says() {
        // Synced and held: two deploys, the newest long past its soak.
        let h: VecDeque<Deployment> = [dep(1, 1, C1, true, "manual", Some("ada"), NOW - 50 * MIN, "synced"), dep(2, 2, C2, true, "auto", None, NOW - 30 * MIN, "synced")].into();
        let s = standing(&app(&h), NOW);
        assert_eq!((s.stand, s.word.as_str(), s.line.as_str()), (Stand::Shipped, "Synced", "#2 runs 2222222 · healthy"));
        assert_eq!(glyphs(&s), [Glyph::Passed, Glyph::Healthy, Glyph::Passed]);
        assert_eq!((s.by.as_deref(), s.at_ms, s.runs.as_deref()), (Some("auto-sync"), NOW - 30 * MIN, Some("2222222")));

        // Soaking: synced four minutes ago.
        let h: VecDeque<Deployment> = [dep(1, 1, C1, true, "manual", Some("ada"), NOW - 4 * MIN, "synced")].into();
        let s = standing(&app(&h), NOW);
        assert_eq!((s.stand, s.word.as_str(), s.line.as_str()), (Stand::Shipping, "Soaking", "#1 runs 1111111 · healthy · soaks 6m more"));
        assert!(s.pips.iter().any(|p| p.stage == Phase::Soak && p.state == Glyph::Soaking && p.p.is_some_and(|p| (p - 0.4).abs() < 1e-9)), "{:?}", s.pips);

        // Out of sync, auto-sync off: a person's to sync; with it on, kiln will.
        let h: VecDeque<Deployment> = [dep(1, 1, C1, true, "manual", Some("agent:01a18b08"), NOW - 36 * MIN, "synced")].into();
        let mut a = app(&h);
        a.sync = Sync::OutOfSync;
        a.ahead = Some(2);
        let s = standing(&a, NOW);
        assert_eq!((s.stand, s.word.as_str(), s.line.as_str()), (Stand::NeedsYou, "Out of sync", "#1 runs 1111111 · trunk is 2 ahead · healthy"));
        assert_eq!(glyphs(&s), [Glyph::Passed, Glyph::Healthy, Glyph::Passed]);
        a.auto_sync = true;
        assert_eq!(standing(&a, NOW).stand, Stand::Waiting);

        // A sync under way: its revision, who started it and when.
        let op = OpStatus { id: 3, phase: OpPhase::Running, trigger: Trigger::Manual, attempt: 1, message: "syncing".into(), by: Some("ada".into()), started_ms: NOW - 5_000, finished_ms: None };
        let mut a = app(&h);
        a.operation = Some(&op);
        let s = standing(&a, NOW);
        assert_eq!((s.stand, s.word.as_str(), s.line.as_str(), s.by.as_deref(), s.at_ms), (Stand::Shipping, "Syncing", "syncing 2222222", Some("ada"), NOW - 5_000));
        assert_eq!(glyphs(&s), [Glyph::Running, Glyph::None, Glyph::None]);

        // Its latest sync failed: failing, what runs said.
        let h: VecDeque<Deployment> = [dep(1, 1, C1, true, "manual", None, NOW - 60 * MIN, "synced"), dep(2, 2, C2, false, "manual", Some("ada"), NOW - MIN, "one or more objects failed to apply\nmore")].into();
        let s = standing(&app(&h), NOW);
        assert_eq!((s.stand, s.word.as_str(), s.line.as_str()), (Stand::Failing, "Sync failed", "#2 failed: one or more objects failed to apply · #1 runs"));
        assert_eq!(glyphs(&s), [Glyph::Failed, Glyph::None, Glyph::None]);

        // Rolled back: #3 failed its health check (the guard's word), and kiln went back to #1.
        let h: VecDeque<Deployment> = [
            dep(1, 1, C1, true, "manual", None, NOW - 3 * 60 * MIN, "synced"),
            dep(2, 2, C3, true, "manual", Some("ada"), NOW - 3 * 60 * MIN + MIN, "synced"),
            dep(3, 2, C3, false, "manual", Some("ada"), NOW - 3 * 60 * MIN + 2 * MIN, "failed its health check"),
            dep(4, 3, C1, true, "rollback", None, NOW - 3 * 60 * MIN + 3 * MIN, "synced"),
        ]
        .into();
        let pin = Pin { to: 1, revision: Some(C1.into()), commits: vec![Some(C1.into())], head: 7, by: Some("auto-rollback".into()), at_ms: NOW - 3 * 60 * MIN + 3 * MIN };
        let mut a = app(&h);
        a.pin = Some(&pin);
        let s = standing(&a, NOW);
        assert_eq!((s.stand, s.word.as_str(), s.line.as_str()), (Stand::RolledBack, "Rolled back", "#2 failed its health check · back on #1"));
        assert_eq!(glyphs(&s), [Glyph::Passed, Glyph::Failed, Glyph::None]);
        assert_eq!(s.by.as_deref(), Some("auto-rollback"));
        // Rolled back by a person to a commit none ran: held there.
        let pin = Pin { to: 0, revision: Some(C4.into()), by: Some("ada".into()), ..Pin::default() };
        let h: VecDeque<Deployment> = [dep(1, 1, C1, true, "manual", None, NOW - 60 * MIN, "synced"), dep(2, 2, C4, true, "rollback", Some("ada"), NOW - 30 * MIN, "synced")].into();
        let mut a = app(&h);
        a.pin = Some(&pin);
        let s = standing(&a, NOW);
        assert_eq!((s.stand, s.line.as_str(), s.by.as_deref()), (Stand::RolledBack, "back on 4444444 · held there until git changes it", Some("ada")));

        // A rollout waiting for a person's approval, and one under way.
        let h: VecDeque<Deployment> = [dep(2, 1, C2, true, "manual", None, NOW - 60 * MIN, "synced"), dep(3, 2, C3, true, "manual", Some("ada"), NOW - 4 * MIN, "synced")].into();
        let waiting = record(vec![Step::Weight { weight: 20 }, Step::Approve, Step::Weight { weight: 100 }], State::Paused { why: "waiting for approval".into() }, 1, 20);
        let mut a = app(&h);
        a.rollout = Some(&waiting);
        let s = standing(&a, NOW);
        assert_eq!((s.stand, s.word.as_str(), s.line.as_str()), (Stand::NeedsYou, "Needs you", "#3 waits for your approval · #2 runs"));
        assert_eq!(glyphs(&s), [Glyph::Requested, Glyph::None, Glyph::None]);
        let going = record(vec![Step::Weight { weight: 20 }, Step::Checks { names: vec!["errors".into()], against: crate::rollout::Against::Production, parallel: false }, Step::Weight { weight: 50 }, Step::Weight { weight: 100 }], State::Running, 1, 20);
        a.rollout = Some(&going);
        let s = standing(&a, NOW);
        assert_eq!((s.stand, s.word.as_str(), s.line.as_str()), (Stand::Shipping, "Rolling out", "#3 · 20% · step 2 of 4"));

        // Degraded; a render that failed; a cluster not reached; Argo CD's; a chart.
        let h: VecDeque<Deployment> = [dep(1, 1, C1, true, "manual", None, NOW - 60 * MIN, "synced")].into();
        let mut a = app(&h);
        a.health = Health::Degraded;
        a.message = Some("Deployment web exceeded its progress deadline");
        assert_eq!(standing(&a, NOW).line, "#1 runs 1111111 · degraded: Deployment web exceeded its progress deadline");
        assert_eq!(standing(&a, NOW).stand, Stand::Failing);
        let mut a = app(&h);
        a.message = Some("render failed: helm: chart not found");
        let s = standing(&a, NOW);
        assert_eq!((s.stand, s.word.as_str()), (Stand::Failing, "Can't render"));
        assert_eq!(glyphs(&s), [Glyph::Error, Glyph::None, Glyph::None]);
        let mut a = app(&h);
        a.sync = Sync::Unknown;
        a.message = Some("cluster kobs is not connected");
        assert_eq!(standing(&a, NOW).word, "Can't compare");
        let mut a = app(&h);
        a.shadow = true;
        assert_eq!(standing(&a, NOW).word, "Argo CD's");
        let h: VecDeque<Deployment> = [dep(1, 1, "92.2.0", true, "manual", Some("admin"), NOW - 6 * 24 * 60 * MIN, "synced")].into();
        let mut a = app(&h);
        a.chart = true;
        assert_eq!(standing(&a, NOW).line, "chart 92.2.0 · healthy");
        // Never deployed nor compared.
        let none = VecDeque::new();
        let mut a = app(&none);
        a.sync = Sync::Unknown;
        a.health = Health::Unknown;
        let s = standing(&a, NOW);
        assert_eq!((s.stand, s.word.as_str(), s.by, s.at_ms), (Stand::Waiting, "Comparing", None, 0));
    }

    #[test]
    fn w26_r24_a_projects_heading_says_its_most_pressing() {
        assert_eq!(project_say(&[(Stand::NeedsYou, "Needs you"), (Stand::Shipping, "Syncing")]), ("2 apps · 1 waits on you".to_owned(), true));
        assert_eq!(project_say(&[(Stand::RolledBack, "Rolled back")]), ("1 app · rolled back".to_owned(), true));
        assert_eq!(project_say(&[(Stand::NeedsYou, "Out of sync"), (Stand::Shipped, "Synced"), (Stand::Shipped, "Synced")]), ("3 apps · 1 out of sync".to_owned(), true));
        assert_eq!(project_say(&[(Stand::Shipped, "Synced"), (Stand::Shipped, "Synced")]), ("2 apps · all synced".to_owned(), false));
        assert_eq!(project_say(&[(Stand::NeedsYou, "Needs you"), (Stand::NeedsYou, "Needs you"), (Stand::Shipped, "Synced")]), ("3 apps · 2 wait on you".to_owned(), true));
        assert_eq!(project_say(&[(Stand::NeedsYou, "Out of sync"), (Stand::NeedsYou, "Needs you")]), ("2 apps · 1 waits on you".to_owned(), true));
        let mut by: Vec<(Stand, &str)> =
            vec![(Stand::Shipped, "Synced"), (Stand::Waiting, "Comparing"), (Stand::NeedsYou, "Out of sync"), (Stand::Failing, "Sync failed"), (Stand::NeedsYou, "Needs you"), (Stand::Shipping, "Syncing"), (Stand::RolledBack, "Rolled back")];
        by.sort_by_key(|(s, w)| rank(*s, w));
        assert_eq!(by.iter().map(|(_, w)| *w).collect::<Vec<_>>(), ["Needs you", "Sync failed", "Rolled back", "Out of sync", "Syncing", "Comparing", "Synced"]);
    }

    fn node(group: &str, kind: &str, name: &str, uid: &str, parents: &[&str]) -> Node {
        Node {
            id: uid.into(),
            uid: Some(uid.into()),
            group: group.into(),
            version: "v1".into(),
            kind: kind.into(),
            namespace: "kiln".into(),
            name: name.into(),
            parents: parents.iter().map(|p| (*p).to_owned()).collect(),
            managed: parents.is_empty(),
            health: None,
            message: None,
            sync: parents.is_empty().then(|| "Synced".to_owned()),
            hook: false,
            missing: false,
            requires_pruning: false,
            why: None,
            tracked_by: None,
            shared_with: Vec::new(),
            marks_only: false,
            created_at: None,
            info: Vec::new(),
            status: None,
            ready: None,
            restarts: None,
            images: Vec::new(),
            containers: Vec::new(),
            node: None,
        }
    }

    fn with_info(mut n: Node, pairs: &[(&str, &str)]) -> Node {
        n.info = pairs.iter().map(|(k, v)| tree::Info { name: (*k).into(), value: (*v).into() }).collect();
        n
    }

    #[test]
    fn w26_r24_components_group_by_their_workload_then_by_what_they_are() {
        let mut deploy = node("apps", "Deployment", "kiln", "d1", &[]);
        deploy.ready = Some("1/1".into());
        deploy.health = Some("Healthy".into());
        deploy.sync = Some("OutOfSync".into());
        let mut rs_new = node("apps", "ReplicaSet", "kiln-cd9dc7b7", "rs2", &["d1"]);
        rs_new.ready = Some("1/1".into());
        rs_new.created_at = Some("2026-11-01T05:30:00Z".into());
        let mut rs_old = node("apps", "ReplicaSet", "kiln-6f1c02aa", "rs1", &["d1"]);
        rs_old.ready = Some("0/0".into());
        rs_old.created_at = Some("2026-10-30T05:30:00Z".into());
        let mut pod = node("", "Pod", "kiln-cd9dc7b7-rsmwz", "p1", &["rs2"]);
        pod.status = Some("Running".into());
        pod.restarts = Some(4);
        pod.created_at = Some("2026-11-01T05:30:00Z".into());
        pod.node = Some("kiln-m02".into());
        let container = |name: &str, init: bool| tree::Container { name: name.into(), image: format!("{name}:1"), init, ready: Some(true), restarts: Some(0), state: None, reason: None, last: None };
        pod.containers = vec![container("migrate", true), container("kiln", false), container("guard", false)];
        let revision = node("apps", "ControllerRevision", "kiln-1", "cr1", &["d1"]);
        let svc = with_info(node("", "Service", "kiln", "s1", &[]), &[("Type", "ClusterIP"), ("Ports", "7070/TCP, 7443/TCP, 7071/UDP")]);
        let slice = node("discovery.k8s.io", "EndpointSlice", "kiln-abc", "es1", &["s1"]);
        let ing = with_info(node("networking.k8s.io", "Ingress", "kiln", "i1", &[]), &[("Hosts", "kiln.lan")]);
        let mut cm = node("", "ConfigMap", "kiln-config", "cm1", &[]);
        cm.sync = Some("OutOfSync".into());
        let secret = node("", "Secret", "kiln", "sec1", &[]);
        let pvc = with_info(node("", "PersistentVolumeClaim", "kiln-data", "pvc1", &[]), &[("Phase", "Bound"), ("Capacity", "200Gi")]);
        let mut job = node("batch", "Job", "kiln-guard-94lwq", "j1", &[]);
        job.health = Some("Healthy".into());
        job.created_at = Some("2026-10-31T17:30:00Z".into());
        let mut gone = node("", "ConfigMap", "old-config", "cm2", &[]);
        gone.requires_pruning = true;
        // 2026-11-01T05:33:00Z.
        let now = 1_793_511_180_000;
        let groups = components(&[deploy, rs_new, rs_old, pod, revision, svc, slice, ing, cm, secret, pvc, job, gone], now);
        let names: Vec<(&str, &str)> = groups.iter().map(|g| (g.name.as_str(), g.note.as_str())).collect();
        assert_eq!(names, [("Deployment kiln", "1/1 ready · out of sync"), ("Network", "synced"), ("Config", "2 differ from git"), ("Storage", "synced"), ("Jobs", "synced")]);
        let lines = |g: &Group| g.items.iter().map(|i| (i.kind.clone(), i.name.clone(), i.state, i.v.clone())).collect::<Vec<_>>();
        let first = groups.first().map(lines).unwrap_or_default();
        assert_eq!(
            first,
            [
                ("Pod".to_owned(), "kiln-cd9dc7b7-rsmwz".to_owned(), Glyph::Running, "up 3m · 4 restarts".to_owned()),
                ("ReplicaSet".to_owned(), "kiln-cd9dc7b7".to_owned(), Glyph::None, "1 pod".to_owned()),
                ("ReplicaSet".to_owned(), "kiln-6f1c02aa".to_owned(), Glyph::Superseded, "0 pods".to_owned()),
            ]
        );
        // A pod says where it runs and whose logs it reads: its first container that isn't an init's.
        let pod = groups.first().and_then(|g| g.items.first());
        assert_eq!(pod.map(|i| (i.node.as_deref(), i.container.as_deref())), Some((Some("kiln-m02"), Some("kiln"))));
        let owner = groups.first().and_then(|g| g.owner.clone());
        assert!(owner.as_ref().is_some_and(|o| o.key == "apps~Deployment~kiln~kiln" && o.differs && o.logs && o.state == Glyph::Changes), "{owner:?}");
        let rest: Vec<_> = groups.iter().skip(1).flat_map(lines).collect();
        assert_eq!(
            rest,
            [
                ("Service".to_owned(), "kiln".to_owned(), Glyph::Healthy, "ClusterIP · 3 ports".to_owned()),
                ("Ingress".to_owned(), "kiln".to_owned(), Glyph::Healthy, "kiln.lan".to_owned()),
                ("ConfigMap".to_owned(), "kiln-config".to_owned(), Glyph::Changes, "differs from git".to_owned()),
                ("ConfigMap".to_owned(), "old-config".to_owned(), Glyph::Superseded, "git dropped it".to_owned()),
                ("Secret".to_owned(), "kiln".to_owned(), Glyph::Healthy, "synced".to_owned()),
                ("PersistentVolumeClaim".to_owned(), "kiln-data".to_owned(), Glyph::Healthy, "Bound · 200Gi".to_owned()),
                ("Job".to_owned(), "kiln-guard-94lwq".to_owned(), Glyph::Passed, "completed · made 12h ago".to_owned()),
            ]
        );
        // A Compose project with no live tree: its objects by their kind, logs read.
        let mut project = node("compose.kiln.dev", "Project", "web", "web", &[]);
        project.uid = None;
        project.health = Some("Healthy".into());
        let groups = components(&[project], now);
        assert_eq!(groups.iter().map(|g| (g.name.as_str(), g.items.iter().all(|i| i.logs))).collect::<Vec<_>>(), [("Project", true)]);
    }

    #[test]
    fn w26_r24_history_is_main_commits_with_the_deploys_that_ran_them() {
        let h: VecDeque<Deployment> = [
            dep(149, 1, "a946ea5a946ea5a946ea5a946ea5a946ea5a946e", true, "manual", Some("ada"), NOW - 2 * 24 * 60 * MIN, "synced"),
            dep(151, 2, "b626724b626724b626724b626724b626724b6267", true, "auto", None, NOW - 24 * 60 * MIN, "synced"),
            dep(152, 3, "d90465fd90465fd90465fd90465fd90465fd9046", false, "manual", Some("ada"), NOW - 23 * 60 * MIN, "apply failed"),
            dep(153, 4, "15878aa15878aa15878aa15878aa15878aa15878", true, "manual", Some("agent:01a18b08"), NOW - 36 * MIN, "synced"),
        ]
        .into();
        let commit = |sha: &str, subject: &str, ago: u64| Commit { sha: sha.into(), subject: subject.into(), author: "user.email".into(), at: ((NOW - ago) / 1000) as i64, same_as: None };
        let commits = [
            commit("021a29f021a29f021a29f021a29f021a29f021a2", "The agent page's composer", 60 * MIN),
            Commit { same_as: Some(153), ..commit("b1ef50db1ef50db1ef50db1ef50db1ef50db1ef5", "Merge ship-made-by", 2 * 60 * MIN) },
            commit("15878aa15878aa15878aa15878aa15878aa15878", "Merge main into fix/frozen-front-door", 3 * 60 * MIN),
            commit("d90465fd90465fd90465fd90465fd90465fd9046", "Runner upgrades", 24 * 60 * MIN),
        ];
        let lines = history(&h, &commits, 0, Glyph::Healthy);
        let got: Vec<(Option<u64>, Option<&str>, Glyph, bool, bool, Option<u64>, Option<&str>)> = lines.iter().map(|l| (l.deploy, l.short.as_deref(), l.state, l.now, l.dim, l.to, l.revision.as_deref().map(|r| r.get(..7).unwrap_or(r)))).collect();
        assert_eq!(
            got,
            [
                (None, Some("021a29f"), Glyph::None, false, false, None, Some("021a29f")),
                (None, Some("b1ef50d"), Glyph::None, false, false, None, Some("b1ef50d")),
                (Some(153), Some("15878aa"), Glyph::Healthy, true, false, None, None),
                (Some(152), Some("d90465f"), Glyph::Failed, false, true, None, Some("d90465f")),
                (Some(151), Some("b626724"), Glyph::Superseded, false, true, Some(151), None),
                (Some(149), Some("a946ea5"), Glyph::Superseded, false, true, Some(149), None),
            ]
        );
        assert_eq!(lines.get(1).and_then(|l| l.same_as), Some(153));
        assert_eq!(lines.get(2).map(|l| (l.subject.as_str(), l.by.as_deref(), l.at_ms)), Some(("Merge main into fix/frozen-front-door", Some("agent:01a18b08"), NOW - 36 * MIN)));
        // A deploy of no commit of its source (a chart's version) still lists, by its message.
        let h: VecDeque<Deployment> = [dep(1, 1, "92.2.0", true, "manual", Some("admin"), NOW, "successfully synced")].into();
        let lines = history(&h, &[], 0, Glyph::Healthy);
        assert_eq!(lines.iter().map(|l| (l.short.as_deref(), l.subject.as_str(), l.now)).collect::<Vec<_>>(), [(Some("92.2.0"), "successfully synced", true)]);
    }

    #[test]
    fn w26_r24_a_rollouts_steps_say_where_it_is() {
        let steps = vec![
            Step::Weight { weight: 20 },
            Step::Checks { names: vec!["error rate".into(), "latency".into()], against: crate::rollout::Against::Production, parallel: false },
            Step::Weight { weight: 50 },
            Step::Weight { weight: 100 },
        ];
        let r = rollout("ro1", &record(steps.clone(), State::Running, 1, 20), NOW);
        assert_eq!(r.steps.iter().map(|s| (s.state, s.what.as_str(), s.v.as_str())).collect::<Vec<_>>(), [
            (Glyph::Passed, "weight 20%", "done"),
            (Glyph::Running, "checks: error rate, latency", "measuring"),
            (Glyph::None, "weight 50%", "next"),
            (Glyph::None, "weight 100%", ""),
        ]);
        assert_eq!((r.note.as_str(), r.under_way, r.waits.as_deref(), r.revision.as_deref()), ("20% · step 2 of 4 · running", true, None, Some("3333333")));
        let r = rollout("ro1", &record(steps.clone(), State::Aborted { why: "error rate 4% over 1%".into() }, 1, 0), NOW);
        assert_eq!(r.steps.iter().map(|s| s.state).collect::<Vec<_>>(), [Glyph::Passed, Glyph::Failed, Glyph::Superseded, Glyph::Superseded]);
        assert_eq!((r.state, r.under_way), (Glyph::Failed, false));
        let r = rollout("ro1", &record(steps, State::Succeeded, 3, 100), NOW);
        assert!(r.steps.iter().all(|s| s.state == Glyph::Passed));
    }

    #[test]
    fn w26_r24_each_verb_says_why_it_cant() {
        let h: VecDeque<Deployment> = [dep(1, 1, C1, true, "manual", None, NOW - 60 * MIN, "synced")].into();
        let a = app(&h);
        let w = why_not(&a, true);
        assert_eq!(w.keys().copied().collect::<Vec<_>>(), ["abort", "approve", "deny", "follow", "promote", "stop"]);
        let op = OpStatus { id: 3, phase: OpPhase::Running, trigger: Trigger::Manual, attempt: 1, message: String::new(), by: None, started_ms: NOW, finished_ms: None };
        let mut a = app(&h);
        a.operation = Some(&op);
        let w = why_not(&a, false);
        assert_eq!(w.get("sync").map(String::as_str), Some("a sync is running: stop it first"));
        assert_eq!(w.get("rollback").map(String::as_str), Some("a sync is running: stop it first"));
        assert!(!w.contains_key("stop"));
        let waiting = record(vec![Step::Weight { weight: 20 }, Step::Approve], State::Paused { why: "waiting for approval".into() }, 1, 20);
        let pin = Pin { to: 1, ..Pin::default() };
        let mut a = app(&h);
        a.rollout = Some(&waiting);
        a.pin = Some(&pin);
        let w = why_not(&a, true);
        assert_eq!(w.keys().copied().collect::<Vec<_>>(), ["stop"]);
        let mut a = app(&h);
        a.shadow = true;
        assert_eq!(why_not(&a, true).get("sync").map(String::as_str), Some("Argo CD deploys it: take it over first"));
    }
}
