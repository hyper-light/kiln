//! Agents attached to what they look after (docs/ci.md W20-R08 to R10; docs/agentic.md K2, R4).
//!
//! An attachment says what it looks after (`on`, written as access scopes are: `repo:acme/app`,
//! `workflow:acme/app:.github/workflows/ci.yml`, `environment:acme/app:production`, `pool:gpu`,
//! `runner:metal-1`, or `runners` for every runner), which agent (a harness kiln starts for each
//! incident, on the machine and in the folder it names, or a live agent it tells), what wakes it,
//! what it may do, its limits, its playbook and who reviews what it opens.
//!
//! kiln listens to its own events. One that wakes an attachment opens an incident, or adds to the
//! open one with the same signature (what failed and how, with numbers, paths and hashes taken
//! out), so one cause never starts two agents. The agent gets the bundle (what failed, the failure
//! excerpts, the last green run, earlier incidents like it) with its playbook. It acts as
//! `agent:<id>` for whoever attached it, with no more than its capabilities give ([`limit_for`]),
//! and says what it found and did on the incident (`incidents.report`). A limit reached leaves the
//! incident saying so; one switch pauses everything attached to a repository
//! (`attachments.pause`).
//!
//! What wakes one: `failure` (a run of what it looks after failed, or a deployment to its
//! environment), `ejected` (the land queue took a change out), `flaky` (a run had flaky tests),
//! `stuck` (a job has waited past its limit for a runner), `slow` (a run's critical path took
//! longer than its workflow's recent median by more than its margin), `unroutable` (a job no
//! runner can take, W05), `degraded` (an app kiln CD deploys to its environment turned degraded,
//! W15), `schedule` (cron, UTC), `mention` (`@<name>` in a comment); for runners, `offline` (gone
//! past its grace), `upgrade` (an upgrade failed) and `check` (kiln's checks of the machine found
//! something failing).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::authz::{Caller, Rights, Scope};
use crate::hub::Hub;
use crate::ops::{Cx, Fail, Op, Out, path, schema};
use crate::store::Repo;

/// What wakes an attachment.
pub const WAKES: &[&str] = &["failure", "ejected", "flaky", "stuck", "slow", "unroutable", "degraded", "schedule", "mention", "offline", "upgrade", "check"];
/// What wakes one on runners.
const RUNNER_WAKES: &[&str] = &["offline", "upgrade", "check", "schedule"];
/// What an attachment's agent may do (W20-R09).
pub const CAN: &[&str] = &["read", "rerun", "cancel", "diagnose", "fix", "edit-pipelines", "quarantine", "comment", "experiment", "manage-runners", "deploy", "land"];
/// What an incident's agent may say it was.
pub const CLASSES: &[&str] = &["flaky", "infrastructure", "regression", "pipeline", "dependency", "other"];
/// How kiln looks for stuck jobs, schedules and failing checks.
const TICK: Duration = Duration::from_secs(60);
/// How often kiln runs its checks of a runner an attachment wakes on (`check`).
const CHECK_EVERY: i64 = 30 * 60;
/// Times an incident keeps of its happening.
const SEEN: usize = 50;
/// Incidents kept.
const KEEP: usize = 500;
/// Failure lines in a bundle, for each failed job.
const EXCERPT_LINES: usize = 40;
/// Failed jobs a bundle shows.
const JOBS: usize = 4;

fn akey(id: &str) -> String {
    format!("attach/a/{id}")
}

fn ikey(id: &str) -> String {
    format!("attach/i/{id}")
}

/// Which attachment an agent kiln started for one is (its capabilities hold it).
fn agent_key(agent: &str) -> String {
    format!("attach/agent/{agent}")
}

/// An attachment, as kiln keeps it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentAttachment"))]
#[serde(default)]
pub struct Attachment {
    pub id: String,
    /// What people and `@mentions` call it (`ci-doctor`): letters, digits and `-`, unique.
    pub name: String,
    /// What it looks after: `repo:<owner>/<repo>`, `workflow:<owner>/<repo>:<path>`,
    /// `environment:<owner>/<repo>:<env>`, `pool:<name>`, `runner:<name>`, or `runners`.
    pub on: String,
    /// Only runs on these branches wake it (none: any).
    pub branches: Vec<String>,
    /// On a repository: only these workflows' runs wake it (a path, a file name or its stem:
    /// `ci`, `ci.yml`, `.github/workflows/ci.yml`; none: any).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(test, ts(as = "Option<Vec<String>>", optional))]
    pub workflows: Vec<String>,
    /// On a repository: only deployments to these environments wake it (none: any).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(test, ts(as = "Option<Vec<String>>", optional))]
    pub environments: Vec<String>,
    /// Where it's kept when not in kiln: `<owner>/<repo>:.kiln/agents.yaml` (changed there, by a
    /// change to that repository's trunk).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub source: Option<String>,
    /// A new agent for each incident…
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub harness: Option<Harness>,
    /// …or a live one, told of each.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub session: Option<String>,
    pub wake: Vec<String>,
    /// When it wakes on `schedule`: five-field cron, UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub schedule: Option<String>,
    pub can: Vec<String>,
    pub limits: Limits,
    /// What its agent does when it wakes, in your words.
    pub playbook: String,
    /// Who reviews what it opens.
    pub reviewers: Vec<String>,
    /// Whom its agents act for (`owner`, `user:<id>`): whoever attached it.
    pub by: String,
    pub paused: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The agent an attachment starts for each incident.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AttachHarness"))]
#[serde(default)]
pub struct Harness {
    /// `claude`, `codex`, `gemini`, …
    pub kind: String,
    /// A machine's name, or what it needs (`linux/amd64`, `+gpu`), as `agents.start` takes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub on: Option<String>,
    /// The folder it starts in there (a checkout of the repository, for one that looks after code).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub mode: Option<String>,
}

/// What keeps an attachment from running away.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AttachLimits"))]
#[serde(default)]
pub struct Limits {
    /// Incidents it takes on in a day (UTC): past them, an incident waits for a person.
    pub sessions_per_day: u32,
    /// Times a failure may come back from reruns before its agent is told to stop rerunning it.
    pub reruns_per_failure: u32,
    /// How long a job waits for a runner before `stuck` wakes it.
    pub stuck_minutes: u32,
    /// How much longer than its workflow's recent median a run's critical path takes, in percent,
    /// before `slow` wakes it.
    pub slow_percent: u32,
    /// Tokens its agents spend in a day (UTC), its incidents' sessions together: past them, an
    /// incident waits for a person.
    pub tokens_per_day: u64,
    /// Runner minutes the runs its agents ask for and rerun take in a day: past them, the same.
    pub runner_minutes_per_day: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { sessions_per_day: 10, reruns_per_failure: 2, stuck_minutes: 15, slow_percent: 25, tokens_per_day: 5_000_000, runner_minutes_per_day: 300 }
    }
}

/// Something an attachment was woken for.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Incident {
    pub id: String,
    pub attachment: String,
    /// The attachment's name, and what it looks after.
    pub name: String,
    pub on: String,
    /// What woke it.
    pub wake: String,
    pub signature: String,
    /// The repository and the runner it is about, when it is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// What happened, in a line.
    pub title: String,
    /// `open` (its agent is on it), `reported` (it said what it found), `resolved`, `waiting` (a
    /// limit, a pause or an agent that couldn't start kept it from an agent: `why`), `dismissed`.
    pub state: String,
    /// Each time it happened, newest last.
    pub seen: Vec<Seen>,
    /// What its agent was given.
    pub bundle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// What its agent found it was ([`CLASSES`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// What its agent found and did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    /// What its agent opened or points to: diffs, runs, pages.
    pub links: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub at: i64,
    pub updated_at: i64,
}

/// One time an incident happened.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "IncidentSeen"))]
#[serde(default)]
pub struct Seen {
    pub at: i64,
    pub what: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub run: Option<String>,
}

// ------------------------------------------------------------------ keeping them

/// Every attachment, by name.
pub async fn all(hub: &Hub) -> Vec<Attachment> {
    let mut v: Vec<Attachment> = hub.store.kv_prefix("attach/a/".to_owned()).await.unwrap_or_default().into_iter().filter_map(|(_, t)| serde_json::from_str(&t).ok()).collect();
    v.extend(from_repos(hub).await);
    v.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.on.cmp(&b.on)));
    v
}

/// Where kiln keeps whether an attachment kept in a repository is paused (the file says what it
/// is; pausing it is a person's switch here).
fn paused_key(id: &str) -> String {
    format!("attach/paused/{id}")
}

/// How long a repository's attached agents are read again at most.
const REPO_FRESH: Duration = Duration::from_secs(60);

/// Attachments kept in repositories' trunks, by hub (its data folder), when last read.
static FROM_REPOS: parking_lot::Mutex<Option<std::collections::HashMap<std::path::PathBuf, (std::time::Instant, Vec<Attachment>)>>> = parking_lot::Mutex::new(None);

/// Attachments kept in repositories' `.kiln/agents.yaml` on trunk (docs/ci.md W20-R08), read
/// like all of a repository's policy: from its trunk, never from a change.
async fn from_repos(hub: &Hub) -> Vec<Attachment> {
    let at = hub.cfg.server.data_dir.clone();
    let cached = FROM_REPOS.lock().get_or_insert_with(Default::default).get(&at).filter(|(t, _)| t.elapsed() < REPO_FRESH).map(|(_, v)| v.clone());
    let list = match cached {
        Some(v) => v,
        None => {
            let mut out = Vec::new();
            for repo in hub.store.repos().await.unwrap_or_default().into_iter().filter(|r| r.enabled) {
                let Some((file, text)) = trunk_agents(hub, &repo).await else { continue };
                let (list, errors) = parse_file(&repo.name, &file, &String::from_utf8_lossy(&text));
                for e in errors {
                    tracing::warn!(repo = %repo.name, file, error = %e, "an agent in a repository's .kiln/agents.yaml can't be attached");
                }
                for mut a in list {
                    a.playbook = playbook_of(hub, &repo, &a.playbook).await;
                    out.push(a);
                }
            }
            FROM_REPOS.lock().get_or_insert_with(Default::default).insert(at, (std::time::Instant::now(), out.clone()));
            out
        }
    };
    let mut out = Vec::with_capacity(list.len());
    for mut a in list {
        a.paused = hub.store.kv_get(paused_key(&a.id)).await.ok().flatten().is_some();
        out.push(a);
    }
    out
}

/// A repository's `.kiln/agents.yaml` on trunk: its name and contents.
async fn trunk_agents(hub: &Hub, repo: &Repo) -> Option<(String, Vec<u8>)> {
    const NAMES: &[&str] = &[".kiln/agents.yaml", ".kiln/agents.yml"];
    // One kiln hosts: its own copy; one it follows: its mirror.
    if let Some(path) = crate::ci::hosting::native_path(hub, repo) {
        let p = path.to_string_lossy().into_owned();
        let trunk = repo.default_branch.clone().unwrap_or_else(|| "main".to_owned());
        for name in NAMES {
            if let Ok(text) = crate::gitops::run_git(hub, &["-C", &p, "show", &format!("{trunk}:{name}")], crate::gitops::LS_REMOTE_TIMEOUT).await {
                return Some(((*name).to_owned(), text));
            }
        }
        return None;
    }
    crate::ci::repo_config::trunk_file(hub, repo, NAMES).await
}

/// A playbook given as a file of the repository (`.kiln/agents/ci-doctor.md`): its text from
/// trunk; else the words given.
async fn playbook_of(hub: &Hub, repo: &Repo, given: &str) -> String {
    let path = given.trim();
    let is_path = !path.is_empty() && !path.contains('\n') && !path.contains(' ') && (path.ends_with(".md") || path.ends_with(".txt") || path.starts_with(".kiln/"));
    if !is_path {
        return given.to_owned();
    }
    let file = path.trim_start_matches("./");
    let text = match crate::ci::hosting::native_path(hub, repo) {
        Some(p) => {
            let trunk = repo.default_branch.clone().unwrap_or_else(|| "main".to_owned());
            crate::gitops::run_git(hub, &["-C", &p.to_string_lossy(), "show", &format!("{trunk}:{file}")], crate::gitops::LS_REMOTE_TIMEOUT).await.ok()
        }
        None => crate::ci::repo_config::trunk_file(hub, repo, &[file]).await.map(|(_, t)| t),
    };
    match text {
        Some(t) => String::from_utf8_lossy(&t).chars().take(20_000).collect(),
        None => format!("(the playbook {file} isn't on the repository's trunk)"),
    }
}

/// `.kiln/agents.yaml`, as written (ci-agents.md's format).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct File {
    agents: std::collections::BTreeMap<String, Entry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Entry {
    /// `claude`, `codex`, `acp:<name>`.
    harness: String,
    model: Option<String>,
    effort: Option<String>,
    mode: Option<String>,
    /// Where its agents run: a machine's name, or what it needs.
    machine: Option<String>,
    /// Its folder there.
    cwd: Option<String>,
    /// A live agent to tell instead.
    session: Option<String>,
    attach: AttachTo,
    wake: Vec<WakeEntry>,
    can: Vec<String>,
    limits: FileLimits,
    /// Its playbook: a file of the repository, or the words.
    instructions: Option<String>,
    reviewers: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AttachTo {
    pipelines: Vec<String>,
    workflows: Vec<String>,
    environments: Vec<String>,
    branches: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WakeEntry {
    Name(String),
    Schedule { schedule: String },
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct FileLimits {
    per_day: PerDay,
    reruns_per_failure: Option<u32>,
    stuck_minutes: Option<u32>,
    slow_percent: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct PerDay {
    sessions: Option<u32>,
    runner_minutes: Option<u64>,
    /// `5000000`, `500k`, `5M`.
    tokens: Option<serde_yaml::Value>,
}

/// A number of tokens as a file writes it: `5000000`, `500k`, `5M`, `1.5M`.
fn amount(v: &serde_yaml::Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    let s = v.as_str()?.trim().replace('_', "");
    let (digits, times) =
        [("k", 1e3), ("K", 1e3), ("m", 1e6), ("M", 1e6), ("b", 1e9), ("B", 1e9)].iter().find_map(|(suffix, times)| s.strip_suffix(suffix).map(|d| (d.to_owned(), *times))).unwrap_or((s.clone(), 1.0));
    let n: f64 = digits.trim().parse().ok()?;
    (n.is_finite() && n >= 0.0).then(|| (n * times).round() as u64)
}

/// The attachments `.kiln/agents.yaml` of repository `repo` keeps, each made right as
/// [`check`] makes one, and why each that can't be one isn't.
pub fn parse_file(repo: &str, file: &str, text: &str) -> (Vec<Attachment>, Vec<String>) {
    let parsed: File = match serde_yaml::from_str(text) {
        Ok(f) => f,
        Err(e) => return (Vec::new(), vec![format!("{file}: {e}")]),
    };
    let (mut out, mut errors) = (Vec::new(), Vec::new());
    for (name, e) in parsed.agents {
        let mut wake = Vec::new();
        let mut schedule = None;
        for w in e.wake {
            match w {
                WakeEntry::Name(n) => wake.push(n),
                WakeEntry::Schedule { schedule: s } => {
                    wake.push("schedule".to_owned());
                    schedule = Some(s);
                }
            }
        }
        let kind = e.harness.trim().trim_start_matches("acp:").to_owned();
        let tokens = e.limits.per_day.tokens.as_ref().map(amount);
        if tokens == Some(None) {
            errors.push(format!("{name}: `limits.per-day.tokens`: a number of tokens (5000000, 500k, 5M)"));
            continue;
        }
        let d = Limits::default();
        let mut a = Attachment {
            id: format!("{}.{}", repo.replace('/', "__"), name.trim().to_ascii_lowercase()),
            name: name.clone(),
            on: format!("repo:{repo}"),
            branches: e.attach.branches,
            workflows: e.attach.pipelines.into_iter().chain(e.attach.workflows).collect(),
            environments: e.attach.environments,
            source: Some(format!("{repo}:{file}")),
            harness: e.session.is_none().then_some(Harness { kind, on: e.machine, cwd: e.cwd, model: e.model, effort: e.effort, mode: e.mode }),
            session: e.session,
            wake,
            schedule,
            can: e.can,
            limits: Limits {
                sessions_per_day: e.limits.per_day.sessions.unwrap_or(d.sessions_per_day),
                reruns_per_failure: e.limits.reruns_per_failure.unwrap_or(d.reruns_per_failure),
                stuck_minutes: e.limits.stuck_minutes.unwrap_or(d.stuck_minutes),
                slow_percent: e.limits.slow_percent.unwrap_or(d.slow_percent),
                tokens_per_day: tokens.flatten().unwrap_or(d.tokens_per_day),
                runner_minutes_per_day: e.limits.per_day.runner_minutes.unwrap_or(d.runner_minutes_per_day),
            },
            playbook: e.instructions.unwrap_or_default(),
            reviewers: e.reviewers,
            // Kept in the repository's trunk, as its policy is: it acts for kiln's owner, held to
            // what it may do there.
            by: "owner".to_owned(),
            ..Attachment::default()
        };
        match check(&mut a) {
            Ok(()) => out.push(a),
            Err(why) => errors.push(format!("{name}: {why}")),
        }
    }
    (out, errors)
}

/// The attachment with id or name `key`.
pub async fn get(hub: &Hub, key: &str) -> Option<Attachment> {
    let key = key.trim();
    if let Some(a) = hub.store.kv_get(akey(key)).await.ok().flatten().and_then(|t| serde_json::from_str(&t).ok()) {
        return Some(a);
    }
    let all = all(hub).await;
    all.iter().find(|a| a.id == key).or_else(|| all.iter().find(|a| a.name == key)).cloned()
}

/// Kept in a repository: changed there, not here.
fn kept_elsewhere(a: &Attachment) -> Result<(), Fail> {
    match &a.source {
        Some(src) => Err((StatusCode::CONFLICT, format!("{} is kept in {src}: change it there, by a change to that repository's trunk (pausing it is the one switch kept here)", a.name))),
        None => Ok(()),
    }
}

async fn put(hub: &Hub, a: &Attachment) {
    if let Ok(t) = serde_json::to_string(a) {
        let _ = hub.store.kv_set(akey(&a.id), t).await;
    }
    hub.emit(&crate::wire::events::ServerEvent::Attachment(crate::wire::events::AttachmentChange::Saved { attachment: Box::new(a.clone()) }));
}

/// Incidents, newest first.
pub async fn incidents(hub: &Hub) -> Vec<Incident> {
    let mut v: Vec<Incident> = hub.store.kv_prefix("attach/i/".to_owned()).await.unwrap_or_default().into_iter().filter_map(|(_, t)| serde_json::from_str(&t).ok()).collect();
    v.sort_by_key(|i| std::cmp::Reverse((i.at, i.id.clone())));
    v
}

pub async fn incident(hub: &Hub, id: &str) -> Option<Incident> {
    hub.store.kv_get(ikey(id.trim())).await.ok().flatten().and_then(|t| serde_json::from_str(&t).ok())
}

async fn put_incident(hub: &Hub, i: &Incident) {
    if let Ok(t) = serde_json::to_string(i) {
        let _ = hub.store.kv_set(ikey(&i.id), t).await;
    }
    hub.emit(&crate::wire::events::ServerEvent::Incident { incident: Box::new(crate::wire::events::IncidentBrief::of(i)) });
}

/// An incident as lists show it (without its bundle).
pub fn brief(i: &Incident) -> Value {
    serde_json::to_value(crate::wire::events::IncidentBrief::of(i)).unwrap_or(Value::Null)
}

/// [`brief`] as it was built before [`crate::wire::events::IncidentBrief`], for its test.
#[cfg(test)]
fn brief_was(i: &Incident) -> Value {
    json!({
        "id": i.id, "attachment": i.attachment, "name": i.name, "on": i.on, "wake": i.wake, "title": i.title, "repo": i.repo, "runner": i.runner,
        "state": i.state, "agent": i.agent, "class": i.class, "report": i.report, "links": i.links, "why": i.why,
        "times": i.seen.len(), "last": i.seen.last(), "at": i.at, "updated_at": i.updated_at,
    })
}

/// Keeps the newest [`KEEP`] incidents.
async fn prune(hub: &Hub) {
    let all = incidents(hub).await;
    for i in all.iter().skip(KEEP) {
        let _ = hub.store.kv_delete(ikey(&i.id)).await;
    }
}

// ------------------------------------------------------------------ what one is

/// What `on` is: a scope access names (`None` for `runners`, every runner).
pub fn scope_of(on: &str) -> Result<Option<Scope>, String> {
    let on = on.trim();
    if on == "runners" {
        return Ok(None);
    }
    match Scope::parse(on)? {
        Scope::Server | Scope::Org { .. } => Err(format!("`{on}`: an attachment looks after a repository, a workflow, an environment, a pool, a runner, or `runners`")),
        s => Ok(Some(s)),
    }
}

fn on_runners(on: &str) -> bool {
    on == "runners" || on.starts_with("runner:") || on.starts_with("pool:")
}

/// An attachment asked for, made right or refused, saying why.
pub fn check(a: &mut Attachment) -> Result<(), String> {
    a.name = a.name.trim().to_ascii_lowercase();
    if a.name.is_empty() || a.name.len() > 40 || !a.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || a.name.starts_with('-') {
        return Err("`name`: up to 40 letters, digits and `-` (people mention it as @name)".into());
    }
    a.on = a.on.trim().to_owned();
    scope_of(&a.on)?;
    let runners = on_runners(&a.on);
    a.wake.retain(|w| !w.trim().is_empty());
    if a.wake.is_empty() {
        a.wake = if runners { vec!["offline".into(), "upgrade".into()] } else { vec!["failure".into()] };
    }
    for w in &a.wake {
        if !WAKES.contains(&w.as_str()) {
            return Err(format!("`wake`: {w} isn't one of {}", WAKES.join(", ")));
        }
        if runners != RUNNER_WAKES.contains(&w.as_str()) && w != "schedule" {
            return Err(if runners {
                format!("`wake`: {w} is about code; an attachment on runners wakes on {}", RUNNER_WAKES.join(", "))
            } else {
                format!("`wake`: {w} is about runners: attach it to `runners`, a runner or a pool")
            });
        }
    }
    match (a.wake.iter().any(|w| w == "schedule"), a.schedule.as_deref().map(str::trim).filter(|s| !s.is_empty())) {
        (true, None) => {
            return Err("`schedule`: a five-field cron (UTC), for `wake: schedule`".into());
        }
        (_, Some(s)) => {
            kiln_ci::trigger::cron::Cron::parse(s)?;
        }
        (false, None) => a.schedule = None,
    }
    if a.can.is_empty() {
        a.can = vec!["read".into()];
    }
    for c in &a.can {
        if !CAN.contains(&c.as_str()) {
            return Err(format!("`can`: {c} isn't one of {}", CAN.join(", ")));
        }
    }
    a.session = a.session.take().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    match (&a.harness, &a.session) {
        (Some(_), Some(_)) => return Err("an attachment starts an agent (`harness`) or tells a live one (`session`), not both".into()),
        (None, None) => return Err("say which agent: `harness` (one kiln starts for each incident) or `session` (a live agent it tells)".into()),
        (Some(h), None) if crate::harness::kinds::Kind::get(&h.kind).is_none() => return Err(format!("`harness.kind`: unknown harness {:?}", h.kind)),
        _ => {}
    }
    let l = &a.limits;
    if l.sessions_per_day == 0
        || l.sessions_per_day > 500
        || l.reruns_per_failure > 20
        || l.stuck_minutes == 0
        || l.stuck_minutes > 24 * 60
        || l.slow_percent == 0
        || l.slow_percent > 1000
        || l.tokens_per_day == 0
        || l.runner_minutes_per_day == 0
    {
        return Err("`limits`: 1 to 500 incidents a day, at most 20 reruns per failure, a job stuck after 1 minute to a day, a run slow from 1% to 1000% over its median, and some tokens and runner minutes a day".into());
    }
    a.branches.retain(|b| !b.trim().is_empty());
    if a.playbook.len() > 20_000 {
        return Err("`playbook`: at most 20,000 characters".into());
    }
    Ok(())
}

/// The capabilities `can` gives on what it looks after, as grants (`runs.rerun@repo:acme/app`).
pub fn grants(a: &Attachment) -> Vec<String> {
    let scope = if a.on == "runners" { "server".to_owned() } else { a.on.clone() };
    // It always reads what it looks after (its incident too).
    let mut caps: Vec<&str> = vec!["runs.read", "logs.read", "artifacts.read", "diffs.read", "git.read", "runners.read"];
    for c in &a.can {
        caps.extend(match c.as_str() {
            "read" => &[][..],
            "rerun" => &["runs.rerun"][..],
            "cancel" => &["runs.cancel"][..],
            "diagnose" => &["runs.debug", "runners.read", "runners.terminal"][..],
            "fix" | "quarantine" => &["diffs.submit"][..],
            "edit-pipelines" => &["diffs.submit", "workflows.edit"][..],
            "comment" => &["diffs.review"][..],
            "experiment" => &["experiments.run"][..],
            "manage-runners" => &["runners.manage", "pools.manage"][..],
            "deploy" => &["environments.deploy"][..],
            "land" => &["diffs.land"][..],
            _ => &[][..],
        });
    }
    caps.sort_unstable();
    caps.dedup();
    caps.into_iter().map(|c| format!("{c}@{scope}")).collect()
}

/// What agent session `session` may do at most, when kiln started it for an attachment: its
/// capabilities, on what it looks after (its MCP calls are held to them, `crate::mcp`).
pub async fn limit_for(hub: &Hub, session: &str) -> Option<Rights> {
    let id = hub.store.kv_get(agent_key(session)).await.ok().flatten()?;
    let a = hub.store.kv_get(akey(&id)).await.ok().flatten().and_then(|t| serde_json::from_str::<Attachment>(&t).ok());
    // An attachment removed since: its agent may only read.
    let gs = a.map_or_else(|| vec!["runs.read".to_owned()], |a| grants(&a));
    let parsed = gs.iter().filter_map(|g| crate::authz::Grant::parse(g).ok()).collect();
    Some(Rights { grants: parsed, custom: Default::default() })
}

// ------------------------------------------------------------------ what wakes one

/// What an event is about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct About {
    pub repo: Option<String>,
    pub workflow: Option<String>,
    pub branch: Option<String>,
    pub environment: Option<String>,
    pub runner: Option<String>,
    pub labels: Vec<String>,
}

/// Whether attachment `a` looks after what `about` is about.
pub fn looks_after(a: &Attachment, about: &About) -> bool {
    let branch_ok = || a.branches.is_empty() || about.branch.as_deref().is_some_and(|b| a.branches.iter().any(|x| x == b));
    match scope_of(&a.on) {
        Ok(None) => about.runner.is_some(),
        Ok(Some(Scope::Runner { runner })) => about.runner.as_deref() == Some(runner.as_str()),
        Ok(Some(Scope::Pool { pool })) => about.runner.is_some() && about.labels.contains(&pool),
        Ok(Some(Scope::Repo { repo })) => {
            about.repo.as_deref() == Some(repo.as_str())
                && branch_ok()
                && (a.workflows.is_empty() || about.workflow.as_deref().is_some_and(|w| a.workflows.iter().any(|x| same_workflow(w, x))))
                && (a.environments.is_empty() || about.environment.as_deref().is_some_and(|e| a.environments.iter().any(|x| x == e)))
        }
        Ok(Some(Scope::Workflow { repo, path })) => about.repo.as_deref() == Some(repo.as_str()) && about.workflow.as_deref().is_some_and(|w| same_workflow(w, &path)) && branch_ok(),
        Ok(Some(Scope::Environment { repo, environment })) => about.repo.as_deref() == Some(repo.as_str()) && about.environment.as_deref() == Some(environment.as_str()),
        _ => false,
    }
}

/// Whether workflow `w` (as a run names it) is the one `name` names: its path, its file's name,
/// or that without its extension (`ci` is `.github/workflows/ci.yml`).
fn same_workflow(w: &str, name: &str) -> bool {
    let file = w.rsplit('/').next().unwrap_or(w);
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    w == name || file == name || stem == name || w.ends_with(&format!("/{name}")) || name.ends_with(&format!("/{w}"))
}

/// The repository an attachment looks after (none: it looks after runners).
fn repo_of(a: &Attachment) -> Option<String> {
    match scope_of(&a.on) {
        Ok(Some(Scope::Repo { repo } | Scope::Workflow { repo, .. } | Scope::Environment { repo, .. })) => Some(repo),
        _ => None,
    }
}

/// One thing that wakes attachments: what it is about, how it reads, and what goes to the agent.
#[derive(Debug, Clone, Default)]
pub struct Trigger {
    pub wake: &'static str,
    pub about: About,
    /// The same cause, the same signature.
    pub signature: String,
    pub title: String,
    pub seen: Seen,
    /// What the agent is given about it.
    pub detail: String,
}

/// A line with what makes it this time's taken out: numbers, hashes, paths, quoted values and
/// addresses, so the same failure reads the same every time.
pub fn normalized(line: &str) -> String {
    let mut out = String::new();
    for word in line.split_whitespace() {
        let w = word.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\''));
        let hexish = w.len() >= 7 && w.chars().all(|c| c.is_ascii_hexdigit());
        let token = if w.contains('/') || w.contains('\\') {
            "<path>"
        } else if hexish || w.chars().any(|c| c.is_ascii_digit()) {
            "<n>"
        } else {
            word
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
    }
    out.chars().take(240).collect()
}

fn s<'a>(v: &'a Value, p: &str) -> Option<&'a str> {
    v.pointer(p).and_then(Value::as_str).filter(|x| !x.is_empty())
}

/// A run's failed jobs: id, name, key.
async fn failed_jobs(hub: &Hub, run: &str) -> Vec<(String, String, String)> {
    let run = run.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, display, key FROM jobs WHERE run_id = ?1 AND status IN ('failed', 'timed_out') ORDER BY COALESCE(finished_at, 0), display LIMIT 20")?;
            let rows = st.query_map([&run], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap_or_default()
}

/// The first line of a failure excerpt that reads like the error (else its last).
fn error_line(f: &Value) -> String {
    let lines: Vec<&str> = f.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("text").and_then(Value::as_str)).collect();
    let marks = ["error", "panicked", "fail", "exception", "fatal", "assert"];
    lines.iter().find(|l| marks.iter().any(|m| l.to_lowercase().contains(m))).or_else(|| lines.last()).map(|l| l.trim().to_owned()).unwrap_or_default()
}

/// What a failed run is: its failed jobs, each one's failing step and excerpt, and the last green
/// run of its workflow on its branch.
async fn failure_of(hub: &Hub, v: &Value, wake: &'static str) -> Trigger {
    let repo = s(v, "/repo/name").or_else(|| s(v, "/run/repo")).unwrap_or_default().to_owned();
    let run = s(v, "/run/id").unwrap_or_default().to_owned();
    let workflow = s(v, "/run/workflow").unwrap_or_default().to_owned();
    let branch = s(v, "/run/branch").unwrap_or_default().to_owned();
    let url = s(v, "/run/url").map(str::to_owned);
    let number = v.pointer("/run/number").and_then(Value::as_i64).unwrap_or_default();
    let name = s(v, "/run/name").unwrap_or(&workflow).to_owned();
    let environment = v.get("environments").and_then(Value::as_array).and_then(|e| e.first()).and_then(Value::as_str).map(str::to_owned);
    let mut detail = format!("{name} #{number} on {branch} failed ({repo}, commit {}).", s(v, "/run/sha").map(|x| x.get(..12).unwrap_or(x)).unwrap_or("?"));
    if let Some(u) = &url {
        detail.push_str(&format!(" It's at {u}."));
    }
    if let Some(e) = &environment {
        detail.push_str(&format!(" Its deployment to {e} failed."));
    }
    let jobs = failed_jobs(hub, &run).await;
    let mut sig = String::new();
    let mut first_error = String::new();
    for (i, (leg, display, key)) in jobs.iter().take(JOBS).enumerate() {
        let f = crate::ci::logsearch::failure(hub, leg, EXCERPT_LINES).await.unwrap_or(Value::Null);
        let step = f.get("step_name").and_then(Value::as_str).unwrap_or("?");
        let err = error_line(&f);
        if i == 0 {
            sig = format!("{key}:{step}:{}", normalized(&err));
            first_error.clone_from(&err);
        }
        detail.push_str(&format!("\n\nFailed: {display}, at step \"{step}\" (job {leg}). Its log around the error:\n"));
        for l in f.get("lines").and_then(Value::as_array).into_iter().flatten() {
            detail.push_str(&format!("{:>5} | {}\n", l.get("line").and_then(Value::as_u64).unwrap_or(0), l.get("text").and_then(Value::as_str).unwrap_or_default()));
        }
    }
    if jobs.len() > JOBS {
        detail.push_str(&format!("\n…and {} more failed jobs.", jobs.len() - JOBS));
    }
    if let Some((n, sha)) = last_green(hub, v, &workflow, &branch, &run).await {
        detail.push_str(&format!("\n\nThe last green run of it on {branch} was #{n}, at commit {}: what changed since is between that commit and this one.", sha.get(..12).unwrap_or(&sha)));
    }
    let title = if first_error.is_empty() { format!("{name} failed on {branch}") } else { format!("{name} failed on {branch}: {}", first_error.chars().take(140).collect::<String>()) };
    Trigger {
        wake,
        about: About { repo: Some(repo.clone()), workflow: Some(workflow.clone()), branch: Some(branch), environment, ..About::default() },
        signature: format!("{wake}:{repo}:{workflow}:{}", if sig.is_empty() { "?".to_owned() } else { sig }),
        title,
        seen: Seen { at: crate::util::now_unix(), what: format!("{name} #{number}"), url, run: Some(run) },
        detail,
    }
}

/// The last green run of `workflow` on `branch` in the event's repository: its number and commit.
async fn last_green(hub: &Hub, v: &Value, workflow: &str, branch: &str, run: &str) -> Option<(i64, String)> {
    let repo_id = s(v, "/repo/id")?.to_owned();
    let (wf, br, run) = (workflow.to_owned(), branch.to_owned(), run.to_owned());
    hub.store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            // Green before the failing run was made (a later push that already fixed it isn't
            // "the last green"), newest made first; a run green on a rerun counts.
            Ok(c.query_row(
                "SELECT number, sha FROM runs WHERE repo_id = ?1 AND COALESCE(workflow_id, '') = ?2 AND branch = ?3 AND conclusion = 'success'
                 AND created_at < (SELECT created_at FROM runs WHERE id = ?4) ORDER BY created_at DESC LIMIT 1",
                rusqlite::params![repo_id, wf, br, run],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await
        .ok()
        .flatten()
}

/// What `v` (one of kiln's events) wakes, when it wakes anything.
async fn trigger_of(hub: &Hub, v: &Value) -> Vec<Trigger> {
    let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "run.failed" => vec![failure_of(hub, v, "failure").await],
        "deploy.failed" => vec![failure_of(hub, v, "failure").await],
        "leg.flaky" => {
            let mut t = failure_of(hub, v, "flaky").await;
            // The tests, as the event names them (`flaky: a, b`).
            let tests = s(v, "/run/message").and_then(|m| m.strip_prefix("flaky: ")).unwrap_or("tests a retry passed").to_owned();
            t.title = format!("Flaky in {}: {tests}", t.seen.what);
            t.signature = format!("flaky:{}:{tests}", t.about.repo.clone().unwrap_or_default());
            t.detail = format!("{}\n\nFlaky in it (a retry passed them, or one attempt failed them and another passed): {tests}.", t.detail);
            vec![t]
        }
        "land.ejected" => {
            let repo = s(v, "/repo/name").unwrap_or_default().to_owned();
            let what = s(v, "/run/name").unwrap_or("a change").to_owned();
            let why = s(v, "/run/message").or_else(|| s(v, "/message")).unwrap_or("the land queue gave no reason");
            let url = s(v, "/run/url").map(str::to_owned);
            vec![Trigger {
                wake: "ejected",
                about: About { repo: Some(repo.clone()), branch: s(v, "/run/branch").map(str::to_owned), ..About::default() },
                signature: format!("ejected:{repo}:{}", normalized(why)),
                title: format!("{what} was taken out of the land queue: {}", why.chars().take(140).collect::<String>()),
                seen: Seen { at: crate::util::now_unix(), what: what.clone(), url: url.clone(), run: None },
                detail: format!("The land queue of {repo} took {what} out: {why}{}", url.map(|u| format!("\n\nIt's at {u}.")).unwrap_or_default()),
            }]
        }
        "runner.offline" => {
            let name = s(v, "/runner/name").unwrap_or_default().to_owned();
            let gone = v.pointer("/runner/gone_for").and_then(Value::as_u64).unwrap_or_default();
            let labels: Vec<String> = v.pointer("/runner/labels").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
            vec![Trigger {
                wake: "offline",
                about: About { runner: Some(name.clone()), labels: labels.clone(), ..About::default() },
                signature: format!("offline:{name}"),
                title: format!("Runner {name} is offline"),
                seen: Seen { at: crate::util::now_unix(), what: format!("gone {} min", gone / 60), url: None, run: None },
                detail: format!("Runner {name} ({}) went away {} minutes ago and hasn't come back.", if labels.is_empty() { "no labels".to_owned() } else { labels.join(", ") }, gone / 60),
            }]
        }
        "updates" => {
            upgrades_through(hub).await;
            upgrades_failed(hub).await
        }
        "comment" => mentions(hub, v).await,
        _ => Vec::new(),
    }
}

/// Upgrade jobs kiln has told attachments of, so each wakes them once.
static UPGRADES_SEEN: parking_lot::Mutex<Option<HashSet<String>>> = parking_lot::Mutex::new(None);
/// Upgrade jobs that went through, and that kiln has told incidents' agents of.
static UPGRADES_THROUGH: parking_lot::Mutex<Option<HashSet<String>>> = parking_lot::Mutex::new(None);

/// Runner upgrades that went through since kiln last looked: the agent of an incident about that
/// runner's upgrading (open, or reported on) hears of it, to check and resolve it rather than
/// wait on an upgrade that is over.
async fn upgrades_through(hub: &Hub) {
    let through: Vec<crate::updates::Job> = {
        let mut seen = UPGRADES_THROUGH.lock();
        let first = seen.is_none();
        let seen = seen.get_or_insert_with(HashSet::new);
        // Those that went through before the server started aren't news.
        hub.updates.jobs().into_iter().filter(|j| j.kind == "runner" && j.state == crate::updates::JobState::Done).filter(|j| seen.insert(j.id.clone()) && !first).collect()
    };
    if through.is_empty() {
        return;
    }
    for (agent, text) in through_news(&through, &incidents(hub).await) {
        crate::workspace::follow::deliver(hub, &agent, text).await;
    }
}

/// Whom upgrades `through` are news to, and what they hear: each agent of an incident about that
/// runner's upgrading that is open or reported on.
fn through_news(through: &[crate::updates::Job], all: &[Incident]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for j in through {
        for i in all.iter().filter(|i| i.wake == "upgrade" && i.runner.as_deref() == Some(j.target.as_str()) && matches!(i.state.as_str(), "open" | "reported")) {
            let Some(agent) = i.agent.clone() else { continue };
            let text = format!(
                "Upgrading runner {} went through: upgrade {} ({}) brought it to build {}. If that's what incident {} is about, check it and report it resolved.",
                j.target,
                j.id,
                j.method,
                j.to_build.as_deref().unwrap_or("?"),
                i.id
            );
            out.push((agent, text));
        }
    }
    out
}

/// Runner upgrades that failed since kiln last looked.
async fn upgrades_failed(hub: &Hub) -> Vec<Trigger> {
    let jobs = hub.updates.jobs();
    let mut seen = UPGRADES_SEEN.lock();
    let first = seen.is_none();
    let seen = seen.get_or_insert_with(HashSet::new);
    let mut out = Vec::new();
    for j in jobs.iter().filter(|j| j.kind == "runner" && j.state == crate::updates::JobState::Failed) {
        // Those that failed before the server started aren't news.
        if !seen.insert(j.id.clone()) || first {
            continue;
        }
        let labels = hub.agents.lock().get(&j.target).map(|a| a.labels.clone()).unwrap_or_default();
        let error = j.error.clone().unwrap_or_else(|| "it gave no reason".to_owned());
        let tail: Vec<&str> = j.log.iter().rev().take(30).rev().map(String::as_str).collect();
        out.push(Trigger {
            wake: "upgrade",
            about: About { runner: Some(j.target.clone()), labels, ..About::default() },
            signature: format!("upgrade:{}:{}", j.target, normalized(&error)),
            title: format!("Upgrading {} failed: {}", j.target, error.chars().take(140).collect::<String>()),
            seen: Seen { at: crate::util::now_unix(), what: format!("upgrade {} ({})", j.id, j.method), url: None, run: None },
            detail: format!(
                "Upgrading runner {} ({} to {}, {}) failed: {error}\n\nThe upgrade's last lines:\n{}",
                j.target,
                j.from_build.as_deref().unwrap_or("?"),
                j.to_build.as_deref().unwrap_or("?"),
                j.method,
                tail.join("\n")
            ),
        });
    }
    out
}

/// Attachments mentioned (`@name`) in a comment.
async fn mentions(hub: &Hub, v: &Value) -> Vec<Trigger> {
    let body = s(v, "/body").unwrap_or_default();
    if !body.contains('@') {
        return Vec::new();
    }
    let diff = s(v, "/diff").unwrap_or_default();
    let by = s(v, "/author").unwrap_or("someone");
    // An attachment's own agent mentioning it isn't a call for it.
    if by.starts_with("agent:") {
        return Vec::new();
    }
    let repo = match crate::ci::diffs::find(hub, diff).await {
        Ok(Some(d)) => Some(d.repo),
        _ => None,
    };
    let url = format!("{}/diffs/{diff}", hub.cfg.server.public_url.trim_end_matches('/'));
    let mut out = Vec::new();
    for a in all(hub).await {
        let said = body.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '@')).any(|w| w.strip_prefix('@').is_some_and(|n| n.eq_ignore_ascii_case(&a.name)));
        if !said || !a.wake.iter().any(|w| w == "mention") {
            continue;
        }
        // One kept in a repository answers there only.
        if a.source.is_some() && repo.is_some() && repo_of(&a) != repo {
            continue;
        }
        out.push(Trigger {
            wake: "mention",
            about: About { repo: repo.clone(), ..About::default() },
            signature: format!("mention:{}:{diff}:{}", a.id, crate::util::now_unix()),
            title: format!("{by} asked @{} on {diff}", a.name),
            seen: Seen { at: crate::util::now_unix(), what: format!("{by} on {diff}"), url: Some(url.clone()), run: None },
            detail: format!("{by} wrote, on {diff} ({url}):\n\n{body}"),
        });
    }
    out
}

// ------------------------------------------------------------------ waking

/// One incident's bookkeeping at a time.
static WAKING: futures::lock::Mutex<()> = futures::lock::Mutex::new(());

/// Wakes what `t` wakes.
pub async fn fire(hub: &Arc<Hub>, t: &Trigger) {
    for a in all(hub).await {
        // A mention calls the one it names; the rest wake what looks after what they're about.
        let called = if t.wake == "mention" { t.signature.starts_with(&format!("mention:{}:", a.id)) } else { looks_after(&a, &t.about) };
        if called && a.wake.iter().any(|w| w == t.wake) {
            wake(hub, &a, t).await;
        }
    }
}

/// A time's day, UTC (`2026-10-19`).
fn day_of(t: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(t).map(|d| format!("{}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())).unwrap_or_default()
}

/// The start of today, UTC.
fn today() -> i64 {
    let now = crate::util::now_unix();
    now - now.rem_euclid(86_400)
}

/// Attachment `a` is woken by `t`: the open incident with its signature hears of it again, or a
/// new one gets its agent (within its limits).
pub async fn wake(hub: &Arc<Hub>, a: &Attachment, t: &Trigger) -> Option<Incident> {
    let _one = WAKING.lock().await;
    let now = crate::util::now_unix();
    let all = incidents(hub).await;
    // The same cause, not yet resolved: one more time it happened, not another agent.
    if let Some(mut i) = all.iter().find(|i| i.attachment == a.id && i.signature == t.signature && matches!(i.state.as_str(), "open" | "waiting" | "reported")).cloned() {
        i.seen.push(t.seen.clone());
        if i.seen.len() > SEEN {
            i.seen.remove(0);
        }
        i.updated_at = now;
        // Reported on, and back: its agent hears of it, and it's open again.
        if i.state == "reported" {
            i.state = "open".to_owned();
            if let Some(agent) = i.agent.clone() {
                let at = t.seen.url.as_deref().map(|u| format!(" ({u})")).unwrap_or_default();
                crate::workspace::follow::deliver(hub, &agent, format!("It happened again: {}{at}. Incident {} is open again; report on it again once you know more.", t.seen.what, i.id)).await;
            }
        }
        // A failure that keeps coming back from reruns: its agent stops rerunning it.
        let again = t.seen.run.as_ref().map_or(0, |r| i.seen.iter().filter(|s| s.run.as_ref() == Some(r)).count());
        if t.wake == "failure" && again > 1 && u32::try_from(again - 1).unwrap_or(u32::MAX) > a.limits.reruns_per_failure {
            if let Some(agent) = i.agent.clone() {
                crate::workspace::follow::deliver(
                    hub,
                    &agent,
                    format!(
                        "{} has failed again after {} reruns, past {}'s limit of {}: don't rerun it again. Say what you found with incidents.report (incident {}).",
                        t.seen.what,
                        again - 1,
                        a.name,
                        a.limits.reruns_per_failure,
                        i.id
                    ),
                )
                .await;
            }
            i.why = Some(format!("rerun {} times, past its limit of {}", again - 1, a.limits.reruns_per_failure));
        }
        put_incident(hub, &i).await;
        return Some(i);
    }
    let mut i = Incident {
        id: crate::util::new_id(),
        attachment: a.id.clone(),
        name: a.name.clone(),
        on: a.on.clone(),
        wake: t.wake.to_owned(),
        signature: t.signature.clone(),
        repo: t.about.repo.clone(),
        runner: t.about.runner.clone(),
        title: t.title.clone(),
        state: "open".to_owned(),
        seen: vec![t.seen.clone()],
        at: now,
        updated_at: now,
        ..Incident::default()
    };
    let earlier: Vec<&Incident> = all.iter().filter(|x| x.attachment == a.id && x.signature == t.signature && x.report.is_some()).take(3).collect();
    i.bundle = bundle(a, &i, t, &earlier);
    let today = all.iter().filter(|x| x.attachment == a.id && x.agent.is_some() && x.at >= today()).count();
    if a.paused {
        i.state = "waiting".to_owned();
        i.why = Some(format!("{} is paused", a.name));
    } else if u32::try_from(today).unwrap_or(u32::MAX) >= a.limits.sessions_per_day {
        i.state = "waiting".to_owned();
        i.why = Some(format!("{} has taken on {today} incidents today, its limit: a person takes this one, or raises the limit", a.name));
    } else if let Some(why) = over_budget(hub, a, &all).await {
        i.state = "waiting".to_owned();
        i.why = Some(why);
    } else {
        match start(hub, a, &i).await {
            Ok(agent) => i.agent = Some(agent),
            Err(why) => {
                i.state = "waiting".to_owned();
                i.why = Some(format!("its agent couldn't start: {why}"));
            }
        }
    }
    put_incident(hub, &i).await;
    hub.store.audit(format!("attachment:{}", a.name), "incident.open".into(), format!("{} {}: {}{}", i.id, a.on, i.title, i.why.as_deref().map(|w| format!(" ({w})")).unwrap_or_default())).await;
    prune(hub).await;
    Some(i)
}

/// Whether `a`'s agents have spent today (UTC) what its limits give them, and how: the tokens of
/// its incidents' sessions, and the runner minutes of the runs they asked for and the attempts
/// they reran (an agent acts as `agent:<its first eight>…`).
async fn over_budget(hub: &Arc<Hub>, a: &Attachment, all: &[Incident]) -> Option<String> {
    let since = today();
    let agents: Vec<String> = all.iter().filter(|x| x.attachment == a.id && x.at >= since).filter_map(|x| x.agent.clone()).collect();
    let tokens: u64 = agents.iter().filter_map(|id| hub.harness.state(id)).map(|s| s.usage.input_tokens + s.usage.output_tokens).sum();
    if tokens >= a.limits.tokens_per_day {
        return Some(format!("{}'s agents have spent {tokens} tokens today, past its limit of {}: a person takes this one, or raises the limit", a.name, a.limits.tokens_per_day));
    }
    let likes: Vec<String> = agents.iter().map(|id| format!("agent:{}%", id.chars().take(8).collect::<String>())).collect();
    let seconds: i64 = hub
        .store
        .call(move |c| {
            let mut total = 0;
            for like in &likes {
                total += c.query_row(
                    "SELECT COALESCE(SUM(j.finished_at - j.started_at), 0) FROM jobs j JOIN runs r ON r.id = j.run_id
                     WHERE j.job_id IS NOT NULL AND j.started_at >= ?2 AND j.finished_at IS NOT NULL
                       AND ((j.run_attempt = 1 AND r.actor LIKE ?1) OR EXISTS (SELECT 1 FROM ci_run_attempts t WHERE t.run_id = j.run_id AND t.attempt = j.run_attempt AND t.actor LIKE ?1))",
                    rusqlite::params![like, since],
                    |r| r.get::<_, i64>(0),
                )?;
            }
            Ok(total)
        })
        .await
        .unwrap_or(0);
    let minutes = u64::try_from(seconds / 60).unwrap_or(0);
    (minutes >= a.limits.runner_minutes_per_day).then(|| {
        format!(
            "the runs {}'s agents asked for and reran took {minutes} runner minutes today, past its limit of {}: a person takes this one, or raises the limit",
            a.name, a.limits.runner_minutes_per_day
        )
    })
}

/// What an incident's agent is given: who it is, what happened, what it may do, its playbook, and
/// how it reports.
fn bundle(a: &Attachment, i: &Incident, t: &Trigger, earlier: &[&Incident]) -> String {
    let mut b = format!("You are {}, attached to {} in kiln, and this woke you ({}): {}\n\n{}", a.name, a.on, t.wake, t.title, t.detail);
    if !earlier.is_empty() {
        b.push_str("\n\nThe same thing before:");
        for e in earlier {
            b.push_str(&format!(
                "\n- {} (incident {}): {} — {}",
                day_of(e.at),
                e.id,
                e.class.as_deref().unwrap_or("not classed"),
                e.report.as_deref().unwrap_or_default().lines().next().unwrap_or_default()
            ));
        }
    }
    b.push_str(&format!(
        "\n\nWhat you may do here: {}. kiln refuses the rest; anything more is a person's to say (ask them). Rerun a failure at most {} times.",
        a.can.join(", "),
        a.limits.reruns_per_failure
    ));
    if !a.reviewers.is_empty() {
        b.push_str(&format!(" What you open is reviewed by {}.", a.reviewers.join(", ")));
    }
    if !a.playbook.trim().is_empty() {
        b.push_str(&format!("\n\nYour playbook:\n{}", a.playbook.trim()));
    }
    b.push_str(&format!(
        "\n\nWhen you know what it is, say so with incidents.report: {{\"incident\": \"{}\", \"class\": one of {}, \"summary\": what you found and did, \"links\": [what you opened], \"state\": \"resolved\" when it's fixed}}.",
        i.id,
        CLASSES.join(", ")
    ));
    b
}

/// Starts attachment `a`'s agent for incident `i` (or tells its live one): the agent's id.
async fn start(hub: &Arc<Hub>, a: &Attachment, i: &Incident) -> Result<String, String> {
    if let Some(s) = &a.session {
        let info = hub.harness.info(s).ok_or_else(|| format!("its agent {s} isn't running"))?;
        if info.status == crate::harness::model::Status::Exited {
            return Err(format!("its agent {} has ended", info.name));
        }
        crate::workspace::follow::deliver(hub, s, i.bundle.clone()).await;
        let _ = hub.store.kv_set(agent_key(s), a.id.clone()).await;
        return Ok(s.clone());
    }
    let h = a.harness.clone().ok_or_else(|| "it names no agent".to_owned())?;
    let starter = crate::harness::lineage::Starter { parent: None, opener: Some(a.by.clone()), actor: format!("attachment:{}", a.name), plan_run: None, asker: None };
    let name: String = format!("{}: {}", a.name, i.title).chars().take(80).collect();
    let ask = crate::harness::lineage::Ask {
        kind: h.kind,
        on: h.on,
        cwd: h.cwd,
        prompt: Some(i.bundle.clone()),
        model: h.model,
        effort: h.effort,
        mode: h.mode,
        name: Some(name),
        count: Some(1),
        ..Default::default()
    };
    let infos = crate::harness::lineage::start(hub, &starter, ask).await.map_err(|(_, m)| m)?;
    let id = infos.first().map(|x| x.id.clone()).ok_or_else(|| "no agent started".to_owned())?;
    let _ = hub.store.kv_set(agent_key(&id), a.id.clone()).await;
    Ok(id)
}

/// Looks for stuck jobs, schedules due and failing checks, once a minute.
async fn ticks(hub: Arc<Hub>) {
    let mut checked: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    // The last minute schedules were looked at: a tick that comes late still sees each minute once.
    let mut looked = crate::util::now_unix() / 60;
    while hub.shutdown.sleep(TICK).await {
        let attached = all(&hub).await;
        let minute = crate::util::now_unix() / 60;
        let minutes: Vec<time::OffsetDateTime> = (looked.max(minute - 10) + 1..=minute).filter_map(|m| time::OffsetDateTime::from_unix_timestamp(m * 60).ok()).collect();
        looked = minute;
        if attached.is_empty() {
            continue;
        }
        for a in attached.iter().filter(|a| !a.paused) {
            if a.wake.iter().any(|w| w == "schedule")
                && let Some(cron) = a.schedule.as_deref()
                && minutes.iter().any(|m| crate::gitops::cron_matches(cron, *m))
            {
                let t = Trigger {
                    wake: "schedule",
                    about: About::default(),
                    signature: "schedule".to_owned(),
                    title: format!("{}'s schedule ({cron})", a.name),
                    seen: Seen { at: crate::util::now_unix(), what: "its schedule".to_owned(), url: None, run: None },
                    detail: format!("It's time for {}'s scheduled work ({cron}, UTC).", a.name),
                };
                wake(&hub, a, &t).await;
            }
        }
        for t in stuck(&hub, &attached).await {
            fire(&hub, &t).await;
        }
        for t in unroutable(&hub, &attached).await {
            fire(&hub, &t).await;
        }
        // kiln's checks of the runners attachments wake on, every half hour each.
        let mut runners: Vec<String> = Vec::new();
        for a in attached.iter().filter(|a| !a.paused && a.wake.iter().any(|w| w == "check")) {
            let names: Vec<String> = hub
                .agents
                .lock()
                .values()
                .filter(|r| !r.ephemeral && !r.outpost && looks_after(a, &About { runner: Some(r.name.clone()), labels: r.labels.clone(), ..About::default() }))
                .map(|r| r.name.clone())
                .collect();
            runners.extend(names);
        }
        runners.sort();
        runners.dedup();
        let now_s = crate::util::now_unix();
        for r in runners {
            if checked.get(&r).is_some_and(|t| now_s - t < CHECK_EVERY) {
                continue;
            }
            checked.insert(r.clone(), now_s);
            if let Ok(v) = crate::doctor::diagnose(&hub, &r).await {
                for f in v.get("findings").and_then(Value::as_array).into_iter().flatten().filter(|f| f.get("level").and_then(Value::as_str) == Some("fail")) {
                    let id = f.get("id").and_then(Value::as_str).unwrap_or("?");
                    let title = f.get("title").and_then(Value::as_str).unwrap_or_default();
                    let labels = hub.agents.lock().get(&r).map(|x| x.labels.clone()).unwrap_or_default();
                    let t = Trigger {
                        wake: "check",
                        about: About { runner: Some(r.clone()), labels, ..About::default() },
                        signature: format!("check:{r}:{id}"),
                        title: format!("{r}: {title}"),
                        seen: Seen { at: now_s, what: format!("check {id}"), url: None, run: None },
                        detail: format!(
                            "kiln's checks of runner {r} found this failing: {title}\n\n{}\n\nAll its findings: {}",
                            f.get("detail").and_then(Value::as_str).unwrap_or_default(),
                            v.get("findings").cloned().unwrap_or(Value::Null)
                        ),
                    };
                    fire(&hub, &t).await;
                }
            }
        }
    }
}

/// Jobs waiting for a runner past the limit of an attachment that wakes on them.
async fn stuck(hub: &Hub, attached: &[Attachment]) -> Vec<Trigger> {
    let Some(least) = attached.iter().filter(|a| !a.paused && a.wake.iter().any(|w| w == "stuck")).map(|a| a.limits.stuck_minutes).min() else {
        return Vec::new();
    };
    let before = crate::util::now_unix() - i64::from(least) * 60;
    let rows: Vec<(String, String, String, String, String, String, i64, i64)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                // Waiting for a runner, not held by the dispatcher (an approval, a window, a wait
                // timer, a concurrency group, a retry's backoff, a quota): timed from the start of
                // its current wait.
                "SELECT j.id, j.display, j.labels, r.id, p.name, COALESCE(r.workflow_id, ''), r.number, COALESCE(j.wait_since, j.queued_at) FROM jobs j JOIN runs r ON r.id = j.run_id JOIN repos p ON p.id = r.repo_id
                 WHERE j.status = 'queued' AND COALESCE(j.wait_since, j.queued_at) < ?1
                   AND (j.wait_reason IS NULL OR json_extract(j.wait_reason, '$.reason') IN ('capacity', 'oversized', 'headroom', 'no-runner', 'unroutable', 'capability', 'scaling', 'reservation', 'other-runner'))
                 ORDER BY COALESCE(j.wait_since, j.queued_at) LIMIT 50",
            )?;
            let rows = st.query_map([before], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap_or_default();
    let now = crate::util::now_unix();
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    rows.into_iter()
        .map(|(job, display, labels, run, repo, workflow, number, queued)| Trigger {
            wake: "stuck",
            about: About { repo: Some(repo.clone()), workflow: Some(workflow.clone()), ..About::default() },
            signature: format!("stuck:{repo}:{workflow}:{labels}"),
            title: format!("{display} has waited {} min for a runner ({labels})", (now - queued) / 60),
            seen: Seen { at: now, what: format!("run #{number}, {display}"), url: Some(format!("{base}/runs/{run}")), run: Some(run) },
            detail: format!("Job {display} (job {job}) of run #{number} of {repo} has been waiting for a runner for {} minutes. It asks for {labels}.", (now - queued) / 60),
        })
        .collect()
}

/// How many of a workflow's last green runs (on the run's branch) say how long its critical path
/// usually takes, and the fewest that do.
const SLOW_RUNS: i64 = 10;
const SLOW_FEWEST: usize = 3;

/// A run's critical path: how long it took, in seconds (each of its jobs from its first leg's
/// start to its last's end), and its jobs.
async fn critical_path(hub: &Hub, run: &str) -> Option<(i64, Vec<String>)> {
    let g = super::runs_api::graph_json(&hub.store, run, None).await.ok()?;
    let path: Vec<String> = g.get("critical_path")?.as_array()?.iter().filter_map(Value::as_str).map(str::to_owned).collect();
    let nodes = g.get("nodes")?.as_array()?;
    let span =
        |j: &str| nodes.iter().find(|n| n.get("job").and_then(Value::as_str) == Some(j)).and_then(|n| Some(n.get("finished_at")?.as_i64()? - n.get("started_at")?.as_i64()?)).unwrap_or(0).max(0);
    let took: i64 = path.iter().map(|j| span(j)).sum();
    (took > 0).then_some((took, path))
}

/// A green run (`run.succeeded`): the attachments that look after it and wake on `slow`, each with
/// what wakes it, when its critical path took longer than the median of its workflow's last green
/// runs on its branch by more than their margin (`slow_percent`).
async fn slow(hub: &Hub, v: &Value) -> Vec<(Attachment, Trigger)> {
    let repo = s(v, "/repo/name").or_else(|| s(v, "/run/repo")).unwrap_or_default().to_owned();
    let workflow = s(v, "/run/workflow").unwrap_or_default().to_owned();
    let branch = s(v, "/run/branch").unwrap_or_default().to_owned();
    let about = About { repo: Some(repo.clone()), workflow: Some(workflow.clone()), branch: Some(branch.clone()), ..About::default() };
    let watching: Vec<Attachment> = all(hub).await.into_iter().filter(|a| !a.paused && a.wake.iter().any(|w| w == "slow") && looks_after(a, &about)).collect();
    let Some(run) = s(v, "/run/id").map(str::to_owned) else { return Vec::new() };
    if watching.is_empty() {
        return Vec::new();
    }
    let Some((took, path)) = critical_path(hub, &run).await else { return Vec::new() };
    let (repo_id, wf, br, this) = (s(v, "/repo/id").unwrap_or_default().to_owned(), workflow.clone(), branch.clone(), run.clone());
    let before: Vec<String> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT id FROM runs WHERE repo_id = ?1 AND COALESCE(workflow_id, '') = ?2 AND branch = ?3 AND conclusion = 'success' AND id != ?4
                   AND created_at < (SELECT created_at FROM runs WHERE id = ?4) ORDER BY created_at DESC LIMIT ?5",
            )?;
            let rows = st.query_map(rusqlite::params![repo_id, wf, br, this, SLOW_RUNS], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap_or_default();
    let mut past: Vec<i64> = Vec::new();
    for r in &before {
        if let Some((t, _)) = critical_path(hub, r).await {
            past.push(t);
        }
    }
    if past.len() < SLOW_FEWEST {
        return Vec::new();
    }
    past.sort_unstable();
    let median = past.get(past.len() / 2).copied().unwrap_or(0);
    if median <= 0 {
        return Vec::new();
    }
    let over = (took - median) * 100 / median;
    let number = v.pointer("/run/number").and_then(Value::as_i64).unwrap_or_default();
    let name = s(v, "/run/name").unwrap_or(&workflow).to_owned();
    let url = s(v, "/run/url").map(str::to_owned);
    watching
        .into_iter()
        .filter(|a| over > i64::from(a.limits.slow_percent))
        .map(|a| {
            let t = Trigger {
                wake: "slow",
                about: about.clone(),
                signature: format!("slow:{repo}:{workflow}:{branch}"),
                title: format!("{name} on {branch}: its critical path took {took} s, {over}% over its median of {median} s"),
                seen: Seen { at: crate::util::now_unix(), what: format!("{name} #{number}"), url: url.clone(), run: Some(run.clone()) },
                detail: format!(
                    "{name} #{number} on {branch} ({repo}) succeeded, but its critical path ({}) took {took} s: {over}% more than the median of its last {} green runs on {branch}, {median} s, past {}'s margin of {}%. Its insights say where the time goes (`ci.insights.workflows`), and its graph which jobs wait on which.",
                    path.join(" → "),
                    past.len(),
                    a.name,
                    a.limits.slow_percent
                ),
            };
            (a, t)
        })
        .collect()
}

/// An app kiln CD deploys turned degraded (W15): what looks after `environment` of `repo` (whose
/// deploy file makes the app) and wakes on `degraded` is woken.
pub async fn degraded(hub: &Arc<Hub>, repo: &str, app: &str, environment: &str, status: &Value) {
    if environment.is_empty() || all(hub).await.is_empty() {
        return;
    }
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let said = status.get("message").and_then(Value::as_str).filter(|m| !m.is_empty()).map(|m| format!(": {m}")).unwrap_or_default();
    let revision = status.get("revision").and_then(Value::as_str).map(|r| format!(" It runs revision {}.", r.get(..12).unwrap_or(r))).unwrap_or_default();
    let t = Trigger {
        wake: "degraded",
        about: About { repo: Some(repo.to_owned()), environment: Some(environment.to_owned()), ..About::default() },
        signature: format!("degraded:{repo}:{environment}:{app}"),
        title: format!("{app} is degraded in {environment}"),
        seen: Seen { at: crate::util::now_unix(), what: app.to_owned(), url: Some(format!("{base}/deployments/apps/{app}")), run: None },
        detail: format!(
            "kiln CD's app {app}, which {repo} deploys to {environment}, turned degraded{said}.{revision} Its resources and their health are on its page; CD's operations (`cd.apps.*`) say what it synced last."
        ),
    };
    fire(hub, &t).await;
}

/// How long a job may be unroutable before `unroutable` wakes on it: a runner that's on its way
/// (joining, coming back) isn't news.
const UNROUTABLE_GRACE: i64 = 60;

/// Jobs no runner can take (W05's `unroutable`: no runner meets what they ask for), past a
/// minute, when an attachment wakes on them. One incident a set of unmet requirements.
async fn unroutable(hub: &Hub, attached: &[Attachment]) -> Vec<Trigger> {
    if !attached.iter().any(|a| !a.paused && a.wake.iter().any(|w| w == "unroutable")) {
        return Vec::new();
    }
    let before = crate::util::now_unix() - UNROUTABLE_GRACE;
    let rows: Vec<(String, String, String, String, String, i64, Option<String>)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT j.id, j.display, r.id, p.name, COALESCE(r.workflow_id, ''), r.number, j.wait_reason FROM jobs j JOIN runs r ON r.id = j.run_id JOIN repos p ON p.id = r.repo_id
                 WHERE j.status = 'queued' AND j.job_id IS NOT NULL AND json_extract(j.wait_reason, '$.reason') = 'unroutable' AND COALESCE(j.wait_since, j.queued_at) < ?1
                 ORDER BY COALESCE(j.wait_since, j.queued_at) LIMIT 50",
            )?;
            let rows = st.query_map([before], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap_or_default();
    let now = crate::util::now_unix();
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    rows.into_iter()
        .map(|(job, display, run, repo, workflow, number, why)| {
            let failing: Vec<String> = why
                .and_then(|w| serde_json::from_str::<Value>(&w).ok())
                .and_then(|w| w.get("failing").and_then(Value::as_array).map(|f| f.iter().filter_map(Value::as_str).map(str::to_owned).collect()))
                .unwrap_or_default();
            let unmet = if failing.is_empty() { "what it asks for".to_owned() } else { failing.join(", ") };
            Trigger {
                wake: "unroutable",
                about: About { repo: Some(repo.clone()), workflow: Some(workflow.clone()), ..About::default() },
                signature: format!("unroutable:{repo}:{workflow}:{unmet}"),
                title: format!("{display} can't run anywhere: no runner meets {unmet}"),
                seen: Seen { at: now, what: format!("run #{number}, {display}"), url: Some(format!("{base}/runs/{run}")), run: Some(run) },
                detail: format!("Job {display} (job {job}) of run #{number} of {repo} waits for a runner none of kiln's can be: none meets {unmet}. Its leg's routing (`ci.legs.routing`) says, runner by runner, what each lacks."),
            }
        })
        .collect()
}

/// Listens to kiln's events and wakes what they wake, for as long as the server runs.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    crate::rt::spawn(ticks(Arc::clone(&hub)));
    // Upgrades that failed or went through before now aren't news.
    let _ = upgrades_failed(&hub).await;
    upgrades_through(&hub).await;
    let mut rx = hub.events.subscribe();
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            m = rx.recv().fuse() => match m {
                Ok(text) => {
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
                    if kind == "run.succeeded" {
                        for (a, t) in slow(&hub, &v).await {
                            wake(&hub, &a, &t).await;
                        }
                    } else if matches!(kind, "run.failed" | "deploy.failed" | "leg.flaky" | "land.ejected" | "runner.offline" | "updates" | "comment") && !all(&hub).await.is_empty() {
                        for t in trigger_of(&hub, &v).await {
                            fire(&hub, &t).await;
                        }
                    }
                }
                Err(async_broadcast::RecvError::Overflowed(_)) => {}
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

// ------------------------------------------------------------------ operations

const ATTACHMENT: &str = "An attachment's id or name.";

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

/// Whether `caller` may attach agents to (or change what's attached to) `on`.
fn may_attach(caller: &Caller, on: &str) -> Result<(), Fail> {
    let scope = scope_of(on).map_err(bad)?.unwrap_or(Scope::Server);
    if caller.can("agents.attach", &scope) { Ok(()) } else { Err((StatusCode::FORBIDDEN, format!("attaching agents to {on} needs `agents.attach` there"))) }
}

/// Whether `caller` may read what's attached to `on`.
fn may_read(caller: &Caller, on: &str) -> bool {
    let scope = scope_of(on).ok().flatten().unwrap_or(Scope::Server);
    caller.can(if on_runners(on) { "runners.read" } else { "runs.read" }, &scope)
}

/// Whom a caller is, for an attachment's agents to act for: the owner, or a user.
fn whom(caller: &Caller) -> String {
    caller.user.as_ref().map_or_else(|| "owner".to_owned(), |u| format!("user:{u}"))
}

async fn list(cx: Cx, v: Value) -> Out {
    let on = v.get("on").and_then(Value::as_str).map(str::trim).filter(|o| !o.is_empty());
    let incs = incidents(&cx.hub).await;
    let since = today();
    let list: Vec<Value> = all(&cx.hub)
        .await
        .into_iter()
        .filter(|a| on.is_none_or(|o| a.on == o || a.on.starts_with(&format!("{}:", o.replace("repo:", "workflow:"))) || a.on.starts_with(&format!("{}:", o.replace("repo:", "environment:")))))
        .filter(|a| may_read(&cx.caller, &a.on))
        .map(|a| {
            let mine: Vec<&Incident> = incs.iter().filter(|i| i.attachment == a.id).collect();
            let open = mine.iter().filter(|i| i.state == "open").count();
            let today = mine.iter().filter(|i| i.agent.is_some() && i.at >= since).count();
            json!({"attachment": a, "open": open, "today": today, "latest": mine.first().map(|i| brief(i))})
        })
        .collect();
    Ok(json!({"attachments": list}))
}

async fn get_one(cx: Cx, v: Value) -> Out {
    let key = crate::ops::text(&v, "attachment")?;
    let a = get(&cx.hub, &key).await.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no attachment {key}")))?;
    if !may_read(&cx.caller, &a.on) {
        return Err((StatusCode::NOT_FOUND, format!("no attachment {key}")));
    }
    let incs: Vec<Value> = incidents(&cx.hub).await.iter().filter(|i| i.attachment == a.id).take(50).map(brief).collect();
    Ok(json!({"attachment": a, "grants": grants(&a), "incidents": incs}))
}

async fn save(cx: Cx, v: Value) -> Out {
    let mut want: Attachment = serde_json::from_value(v.clone()).map_err(|e| bad(format!("{e}")))?;
    let key = v.get("attachment").and_then(Value::as_str).map(str::to_owned).or_else(|| Some(want.id.clone()).filter(|i| !i.is_empty()));
    let before = match &key {
        Some(k) => Some(get(&cx.hub, k).await.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no attachment {k}")))?),
        None => None,
    };
    if let Some(b) = &before {
        may_attach(&cx.caller, &b.on)?;
        kept_elsewhere(b)?;
    }
    want.source = None;
    check(&mut want).map_err(bad)?;
    may_attach(&cx.caller, &want.on)?;
    // The live agent it tells must be one its person may steer.
    if let Some(s) = &want.session {
        let info = cx.hub.harness.info(s).ok_or_else(|| bad(format!("no agent {s}")))?;
        if let Some(me) = cx.caller.agent.as_deref()
            && me != info.id
            && !crate::harness::lineage::descends(&cx.hub.harness.list(), &info.id, me)
        {
            return Err((StatusCode::FORBIDDEN, format!("{} isn't an agent you may tell: an agent attaches only itself or agents it started", info.name)));
        }
    }
    let taken = all(&cx.hub).await.into_iter().any(|a| a.source.is_none() && a.name == want.name && before.as_ref().is_none_or(|b| b.id != a.id));
    if taken {
        return Err((StatusCode::CONFLICT, format!("an attachment is called {} already", want.name)));
    }
    let now = crate::util::now_unix();
    want.id = before.as_ref().map_or_else(crate::util::new_id, |b| b.id.clone());
    want.created_at = before.as_ref().map_or(now, |b| b.created_at);
    want.updated_at = now;
    // Its agents act for whoever attached it: changing it makes them act for whoever changed it.
    want.by = whom(&cx.caller);
    put(&cx.hub, &want).await;
    cx.hub
        .store
        .audit(
            cx.caller.name.clone(),
            if before.is_some() { "attachment.update" } else { "attachment.create" }.into(),
            format!("{} on {}: wakes on {}, may {}", want.name, want.on, want.wake.join(", "), want.can.join(", ")),
        )
        .await;
    Ok(json!({"attachment": want, "grants": grants(&want)}))
}

async fn remove(cx: Cx, v: Value) -> Out {
    let key = crate::ops::text(&v, "attachment")?;
    let a = get(&cx.hub, &key).await.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no attachment {key}")))?;
    may_attach(&cx.caller, &a.on)?;
    kept_elsewhere(&a)?;
    let _ = cx.hub.store.kv_delete(akey(&a.id)).await;
    cx.hub.emit(&crate::wire::events::ServerEvent::Attachment(crate::wire::events::AttachmentChange::Removed { removed: a.id.clone() }));
    cx.hub.store.audit(cx.caller.name.clone(), "attachment.remove".into(), format!("{} on {}", a.name, a.on)).await;
    Ok(json!({"removed": a.id}))
}

/// Pauses (or resumes) one attachment, or everything attached to `on` (a repository: its
/// workflows' and environments' too).
async fn pause(cx: Cx, v: Value) -> Out {
    let paused = v.get("paused").and_then(Value::as_bool).unwrap_or(true);
    let one = v.get("attachment").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let on = v.get("on").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let targets: Vec<Attachment> = match (one, on) {
        (Some(k), _) => vec![get(&cx.hub, k).await.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no attachment {k}")))?],
        (None, Some(o)) => {
            let repo = o.strip_prefix("repo:");
            all(&cx.hub).await.into_iter().filter(|a| a.on == o || repo.is_some_and(|r| a.on.starts_with(&format!("workflow:{r}:")) || a.on.starts_with(&format!("environment:{r}:")))).collect()
        }
        (None, None) => {
            return Err(bad("say which: `attachment`, or `on` (a repository pauses everything attached to it)"));
        }
    };
    let mut changed = Vec::new();
    for mut a in targets {
        may_attach(&cx.caller, &a.on)?;
        if a.paused != paused {
            a.paused = paused;
            if a.source.is_some() {
                let _ = if paused { cx.hub.store.kv_set(paused_key(&a.id), "1".to_owned()).await } else { cx.hub.store.kv_delete(paused_key(&a.id)).await };
                cx.hub.emit(&crate::wire::events::ServerEvent::Attachment(crate::wire::events::AttachmentChange::Saved { attachment: Box::new(a.clone()) }));
            } else {
                a.updated_at = crate::util::now_unix();
                put(&cx.hub, &a).await;
            }
            changed.push(a.name.clone());
        }
    }
    cx.hub.store.audit(cx.caller.name.clone(), if paused { "attachment.pause" } else { "attachment.resume" }.into(), changed.join(", ")).await;
    Ok(json!({"paused": paused, "attachments": changed}))
}

async fn incidents_list(cx: Cx, v: Value) -> Out {
    let state = v.get("state").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let run = v.get("run").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let of = v.get("attachment").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let of = match of {
        Some(k) => Some(get(&cx.hub, k).await.map_or_else(|| k.to_owned(), |a| a.id)),
        None => None,
    };
    let limit = v.get("limit").and_then(|l| l.as_u64().or_else(|| l.as_str().and_then(|s| s.parse().ok()))).unwrap_or(100).min(500);
    let list: Vec<Value> = incidents(&cx.hub)
        .await
        .iter()
        .filter(|i| state.is_none_or(|s| i.state == s) && of.as_deref().is_none_or(|a| i.attachment == a) && run.is_none_or(|r| i.seen.iter().any(|x| x.run.as_deref() == Some(r))) && may_read(&cx.caller, &i.on))
        .take(usize::try_from(limit).unwrap_or(100))
        .map(brief)
        .collect();
    Ok(json!({"incidents": list}))
}

async fn incident_get(cx: Cx, v: Value) -> Out {
    let id = crate::ops::text(&v, "incident")?;
    let i =
        incident(&cx.hub, &id).await.filter(|i| may_read(&cx.caller, &i.on) || cx.caller.agent.as_deref() == i.agent.as_deref()).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no incident {id}")))?;
    Ok(json!({"incident": i}))
}

/// An incident's agent (or a person who may attach there) says what it was and what was done.
async fn report(cx: Cx, v: Value) -> Out {
    let id = crate::ops::text(&v, "incident")?;
    let mut i = incident(&cx.hub, &id).await.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no incident {id}")))?;
    let its_agent = cx.caller.agent.is_some() && cx.caller.agent.as_deref() == i.agent.as_deref();
    if !its_agent && may_attach(&cx.caller, &i.on).is_err() {
        return Err((StatusCode::FORBIDDEN, "only the incident's agent, or a person who may attach agents there, reports on it".to_owned()));
    }
    if let Some(c) = v.get("class").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty()) {
        if !CLASSES.contains(&c) {
            return Err(bad(format!("`class`: one of {}", CLASSES.join(", "))));
        }
        i.class = Some(c.to_owned());
    }
    if let Some(summary) = v.get("summary").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        i.report = Some(summary.chars().take(8000).collect());
    }
    for l in v.get("links").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        if !i.links.iter().any(|x| x == l) && i.links.len() < 50 {
            i.links.push(l.chars().take(500).collect());
        }
    }
    let state = v.get("state").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("reported");
    if !matches!(state, "reported" | "resolved" | "dismissed" | "open") {
        return Err(bad("`state`: reported, resolved, dismissed or open"));
    }
    if state == "dismissed" && its_agent {
        return Err((StatusCode::FORBIDDEN, "dismissing an incident is a person's to do".to_owned()));
    }
    i.state = state.to_owned();
    i.updated_at = crate::util::now_unix();
    put_incident(&cx.hub, &i).await;
    cx.hub
        .store
        .audit(
            cx.caller.name.clone(),
            "incident.report".into(),
            format!("{} {}: {} {}", i.id, i.state, i.class.as_deref().unwrap_or("-"), i.report.as_deref().unwrap_or_default().lines().next().unwrap_or_default()),
        )
        .await;
    Ok(json!({"incident": brief(&i)}))
}

fn key_of(v: &Value, k: &str) -> Result<String, Fail> {
    crate::ops::text(v, k)
}

/// An attachment's fields, as operations take them.
fn attachment_schema() -> Value {
    json!({
            "name": {"type": "string", "description": "What people and @mentions call it: letters, digits and -."},
            "on": {"type": "string", "description": "What it looks after: repo:<owner>/<repo>, workflow:<owner>/<repo>:<path>, environment:<owner>/<repo>:<env>, pool:<name>, runner:<name>, or runners."},
            "branches": {"type": "array", "items": {"type": "string"}, "description": "Only runs on these branches wake it (none: any)."},
            "harness": {"type": "object", "description": "The agent kiln starts for each incident: {kind, on?, cwd?, model?, effort?, mode?} (on and cwd: the machine and the checkout it works in).", "properties": {"kind": {"type": "string"}, "on": {"type": "string"}, "cwd": {"type": "string"}, "model": {"type": "string"}, "effort": {"type": "string"}, "mode": {"type": "string"}}},
            "session": {"type": "string", "description": "Or a live agent's id, told of each incident instead."},
            "wake": {"type": "array", "items": {"type": "string", "enum": WAKES}, "description": "What wakes it: failure, ejected, flaky, stuck, slow, unroutable, degraded, schedule, mention; on runners: offline, upgrade, check."},
            "schedule": {"type": "string", "description": "Five-field cron (UTC), for wake: schedule."},
            "can": {"type": "array", "items": {"type": "string", "enum": CAN}, "description": "What its agent may do (W20-R09); kiln refuses it the rest."},
            "limits": {"type": "object", "properties": {"sessions_per_day": {"type": "integer"}, "reruns_per_failure": {"type": "integer"}, "stuck_minutes": {"type": "integer"}, "slow_percent": {"type": "integer"}, "tokens_per_day": {"type": "integer"}, "runner_minutes_per_day": {"type": "integer"}}},
            "playbook": {"type": "string", "description": "What its agent does when it wakes."},
            "reviewers": {"type": "array", "items": {"type": "string"}},
            "paused": {"type": "boolean"}
    })
}

/// Attachments' and incidents' operations (kiln's own: `attachments.*`, `incidents.*`).
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "attachments.list",
            summary: "Agents attached to what they look after (repositories, workflows, environments, runner pools, runners): what wakes each, what it may do, its open incidents and today's count against its limit. `on` keeps those on one thing (a repository's include its workflows' and environments').",
            read_only: true,
            destructive: false,
            input: || schema(json!({"on": {"type": "string"}}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "attachments"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "attachments.get",
            summary: "One attachment: everything it says, the grants its agents get, and its incidents.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"attachment": {"type": "string", "description": ATTACHMENT}}), &["attachment"]),
            route: |v| Ok((Method::GET, path(&["api", "attachments", &key_of(v, "attachment")?]))),
            run: |cx, v| Box::pin(get_one(cx, v)),
        },
        Op {
            name: "attachments.save",
            summary: "Attach an agent to what it looks after, or change an attachment (`attachment`: its id or name). Its agents act for you, with no more than `can` gives them on what it looks after; needs `agents.attach` there.",
            read_only: false,
            destructive: false,
            input: || {
                let mut props = attachment_schema();
                if let Some(o) = props.as_object_mut() {
                    o.insert("attachment".into(), json!({"type": "string", "description": "To change one: its id or name."}));
                }
                schema(props, &["name", "on"])
            },
            route: |v| {
                Ok(match v.get("attachment").and_then(Value::as_str) {
                    Some(k) => (Method::PUT, path(&["api", "attachments", k])),
                    None => (Method::POST, path(&["api", "attachments"])),
                })
            },
            run: |cx, v| Box::pin(save(cx, v)),
        },
        Op {
            name: "attachments.remove",
            summary: "Detach an agent: nothing wakes it any more (its incidents stay).",
            read_only: false,
            destructive: true,
            input: || schema(json!({"attachment": {"type": "string", "description": ATTACHMENT}}), &["attachment"]),
            route: |v| Ok((Method::DELETE, path(&["api", "attachments", &key_of(v, "attachment")?]))),
            run: |cx, v| Box::pin(remove(cx, v)),
        },
        Op {
            name: "attachments.pause",
            summary: "Pause (or with `paused: false`, resume) one attachment, or everything attached to `on`: a repository's switch pauses its workflows' and environments' too. While paused, what would wake it waits as an incident for a person.",
            read_only: false,
            destructive: false,
            input: || schema(json!({"attachment": {"type": "string", "description": ATTACHMENT}, "on": {"type": "string"}, "paused": {"type": "boolean"}}), &[]),
            route: |_| Ok((Method::POST, path(&["api", "attachments", "pause"]))),
            run: |cx, v| Box::pin(pause(cx, v)),
        },
        Op {
            name: "incidents.list",
            summary: "What woke attached agents, newest first: each incident's title, state (open, reported, resolved, waiting, dismissed), its agent, its class and report. `state` and `attachment` filter.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"state": {"type": "string"}, "attachment": {"type": "string"}, "run": {"type": "string", "description": "A run's incidents: those it happened in."}, "limit": {"type": "integer"}}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "incidents"]))),
            run: |cx, v| Box::pin(incidents_list(cx, v)),
        },
        Op {
            name: "incidents.get",
            summary: "One incident, with the bundle its agent was given and each time it happened.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"incident": {"type": "string"}}), &["incident"]),
            route: |v| Ok((Method::GET, path(&["api", "incidents", &key_of(v, "incident")?]))),
            run: |cx, v| Box::pin(incident_get(cx, v)),
        },
        Op {
            name: "incidents.report",
            summary: "Say what an incident was and what was done (its agent, or a person): `class` (flaky, infrastructure, regression, pipeline, dependency, other), `summary`, `links` (diffs opened, runs), `state` (reported, the default; resolved; dismissed, a person's only).",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({"incident": {"type": "string"}, "class": {"type": "string", "enum": CLASSES}, "summary": {"type": "string"}, "links": {"type": "array", "items": {"type": "string"}}, "state": {"type": "string", "enum": ["reported", "resolved", "dismissed", "open"]}}),
                    &["incident"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "incidents", &key_of(v, "incident")?, "report"]))),
            run: |cx, v| Box::pin(report(cx, v)),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doctor(on: &str) -> Attachment {
        Attachment { name: "CI-Doctor".into(), on: on.into(), harness: Some(Harness { kind: "claude".into(), ..Harness::default() }), ..Attachment::default() }
    }

    /// An incident's brief is the JSON it was before it was a type (`wire::events::IncidentBrief`).
    #[test]
    fn brief_is_what_it_was() -> anyhow::Result<()> {
        let mut i = Incident { id: "i1".into(), name: "ci-doctor".into(), title: "CI failed".into(), state: "open".into(), links: vec!["D13".into()], at: 5, updated_at: 6, ..Incident::default() };
        anyhow::ensure!(brief(&i) == brief_was(&i), "{}", brief(&i));
        i.repo = Some("acme/app".into());
        i.agent = Some("a1".into());
        i.seen = vec![Seen { at: 5, what: "run.failed".into(), ..Seen::default() }];
        anyhow::ensure!(brief(&i) == brief_was(&i), "{}", brief(&i));
        Ok(())
    }

    #[test]
    fn an_attachment_is_made_right_or_refused_saying_why() {
        let mut a = doctor("repo:acme/app");
        assert_eq!(check(&mut a), Ok(()));
        assert_eq!((a.name.as_str(), a.wake.clone(), a.can.clone()), ("ci-doctor", vec!["failure".to_owned()], vec!["read".to_owned()]), "a name as mentions take it; failures and reading by default");
        let mut r = doctor("runners");
        assert_eq!(check(&mut r), Ok(()));
        assert_eq!(r.wake, vec!["offline".to_owned(), "upgrade".to_owned()], "a runner doctor wakes on machines");
        let refused = |mut a: Attachment| check(&mut a).err().unwrap_or_default();
        assert!(refused(doctor("server")).contains("looks after"));
        assert!(refused(doctor("repo:")).contains("scope") || refused(doctor("repo:")).contains("repo"));
        assert!(refused(Attachment { wake: vec!["offline".into()], ..doctor("repo:acme/app") }).contains("about runners"));
        assert!(refused(Attachment { wake: vec!["failure".into()], ..doctor("runner:metal-1") }).contains("about code"));
        assert!(refused(Attachment { wake: vec!["schedule".into()], ..doctor("repo:acme/app") }).contains("cron"));
        assert!(refused(Attachment { wake: vec!["schedule".into()], schedule: Some("every day".into()), ..doctor("repo:acme/app") }).contains("five fields"));
        assert_eq!(check(&mut Attachment { wake: vec!["schedule".into()], schedule: Some("0 6 * * 1".into()), ..doctor("repo:acme/app") }), Ok(()));
        assert!(refused(Attachment { can: vec!["approve".into()], ..doctor("repo:acme/app") }).contains("`can`"));
        assert!(refused(Attachment { harness: None, ..doctor("repo:acme/app") }).contains("which agent"));
        assert!(refused(Attachment { session: Some("s1".into()), ..doctor("repo:acme/app") }).contains("not both"));
        assert!(refused(Attachment { harness: Some(Harness { kind: "nope".into(), ..Harness::default() }), ..doctor("repo:acme/app") }).contains("unknown harness"));
        assert!(refused(Attachment { name: "@doc".into(), ..doctor("repo:acme/app") }).contains("`name`"));
        assert!(refused(Attachment { limits: Limits { sessions_per_day: 0, ..Limits::default() }, ..doctor("repo:acme/app") }).contains("`limits`"));
    }

    #[test]
    fn an_attached_agent_may_do_what_its_capabilities_give_on_what_it_looks_after() {
        let a = Attachment { can: vec!["rerun".into(), "fix".into()], ..doctor("repo:acme/app") };
        let g = grants(&a);
        for want in ["runs.read@repo:acme/app", "runs.rerun@repo:acme/app", "diffs.submit@repo:acme/app"] {
            assert!(g.iter().any(|x| x == want), "{want} in {g:?}");
        }
        assert!(!g.iter().any(|x| x.starts_with("diffs.land") || x.starts_with("environments.deploy")), "{g:?}");
        let parsed = g.iter().filter_map(|x| crate::authz::Grant::parse(x).ok()).collect();
        let r = Rights { grants: parsed, custom: Default::default() };
        let app = Scope::Repo { repo: "acme/app".into() };
        assert!(r.allows("runs.rerun", &app) && !r.allows("runs.rerun", &Scope::Repo { repo: "acme/other".into() }) && !r.allows("runs.cancel", &app));
        let doc = Attachment { can: vec!["manage-runners".into()], ..doctor("runners") };
        assert!(grants(&doc).iter().any(|x| x == "runners.manage@server"));
    }

    #[test]
    fn what_it_looks_after_is_what_wakes_it() {
        let about = |repo: &str, wf: &str, branch: &str| About { repo: Some(repo.into()), workflow: Some(wf.into()), branch: Some(branch.into()), ..About::default() };
        let mut a = doctor("repo:acme/app");
        assert!(looks_after(&a, &about("acme/app", ".github/workflows/ci.yml", "feature")));
        assert!(!looks_after(&a, &about("acme/other", ".github/workflows/ci.yml", "main")));
        a.branches = vec!["main".into()];
        assert!(!looks_after(&a, &about("acme/app", ".github/workflows/ci.yml", "feature")) && looks_after(&a, &about("acme/app", "x", "main")));
        let w = doctor("workflow:acme/app:.github/workflows/ci.yml");
        assert!(looks_after(&w, &about("acme/app", ".github/workflows/ci.yml", "main")) && !looks_after(&w, &about("acme/app", ".github/workflows/release.yml", "main")));
        let e = doctor("environment:acme/app:production");
        assert!(looks_after(&e, &About { environment: Some("production".into()), ..about("acme/app", "deploy.yml", "main") }));
        assert!(!looks_after(&e, &about("acme/app", "deploy.yml", "main")));
        let runner = |name: &str, labels: &[&str]| About { runner: Some(name.into()), labels: labels.iter().map(|l| (*l).to_owned()).collect(), ..About::default() };
        assert!(looks_after(&doctor("runners"), &runner("metal-1", &[])) && !looks_after(&doctor("runners"), &about("acme/app", "x", "main")));
        assert!(looks_after(&doctor("runner:metal-1"), &runner("metal-1", &[])) && !looks_after(&doctor("runner:metal-1"), &runner("metal-2", &[])));
        assert!(looks_after(&doctor("pool:gpu"), &runner("metal-2", &["gpu", "linux"])) && !looks_after(&doctor("pool:gpu"), &runner("metal-1", &["linux"])));
    }

    /// ci-agents.md's example, as a repository keeps it on trunk.
    #[test]
    fn w20_r08_agents_kept_in_a_repository_read_as_written() -> anyhow::Result<()> {
        let text = r#"
isolation: [worktree, microvm]
agents:
  ci-doctor:
    harness: claude
    model: opus
    effort: high
    machine: linux/amd64
    cwd: ~/src/app
    attach: { pipelines: [ci], branches: [main] }
    wake: [failure, flaky, stuck, ejected]
    can: [read, rerun, diagnose, fix, comment, quarantine]
    limits: { per-day: { sessions: 10, runner-minutes: 600, tokens: 5M }, reruns-per-failure: 2 }
    instructions: .kiln/agents/ci-doctor.md
    reviewers: [ada]
  pipeline-gardener:
    harness: codex
    attach: { repo: true }
    wake: [{ schedule: "0 6 * * 1" }]
    can: [read, diagnose, edit-pipelines, comment]
  broken:
    harness: nope
"#;
        let (list, errors) = parse_file("acme/app", ".kiln/agents.yaml", text);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors.first().is_some_and(|e| e.starts_with("broken: ") && e.contains("unknown harness")), "{errors:?}");
        let [doctor, gardener] = list.as_slice() else { anyhow::bail!("{list:?}") };
        assert_eq!((doctor.id.as_str(), doctor.name.as_str(), doctor.on.as_str()), ("acme__app.ci-doctor", "ci-doctor", "repo:acme/app"));
        assert_eq!((doctor.workflows.clone(), doctor.branches.clone()), (vec!["ci".to_owned()], vec!["main".to_owned()]));
        assert_eq!(doctor.source.as_deref(), Some("acme/app:.kiln/agents.yaml"));
        assert_eq!(doctor.harness, Some(Harness { kind: "claude".into(), on: Some("linux/amd64".into()), cwd: Some("~/src/app".into()), model: Some("opus".into()), effort: Some("high".into()), mode: None }));
        assert_eq!((doctor.limits.sessions_per_day, doctor.limits.reruns_per_failure), (10, 2));
        assert_eq!((doctor.limits.runner_minutes_per_day, doctor.limits.tokens_per_day), (600, 5_000_000), "its day's runner minutes and tokens");
        assert_eq!((doctor.playbook.as_str(), doctor.by.as_str()), (".kiln/agents/ci-doctor.md", "owner"));
        assert_eq!((gardener.wake.clone(), gardener.schedule.as_deref()), (vec!["schedule".to_owned()], Some("0 6 * * 1")));
        // Its pipelines narrow what wakes it: a run of ci.yml does, release.yml's doesn't.
        let run = |wf: &str| About { repo: Some("acme/app".into()), workflow: Some(wf.into()), branch: Some("main".into()), ..About::default() };
        assert!(looks_after(doctor, &run(".github/workflows/ci.yml")) && !looks_after(doctor, &run(".github/workflows/release.yml")));
        assert!(looks_after(gardener, &run(".github/workflows/release.yml")));
        assert_eq!(parse_file("acme/app", ".kiln/agents.yaml", "agents: [").1.len(), 1, "a file that isn't YAML says so");
        Ok(())
    }

    #[test]
    fn the_same_failure_reads_the_same_every_time() {
        let one = normalized("error: test sync::reconnect panicked at src/sync.rs:41:9 after 1500 ms (run 9f3c2e1a7b)");
        let two = normalized("error: test sync::reconnect panicked at src/sync.rs:57:2 after 2210 ms (run 0a1b2c3d4e)");
        assert_eq!(one, two);
        assert!(one.contains("sync::reconnect") && one.contains("<path>") && one.contains("<n>"), "{one}");
        assert_ne!(normalized("error: disk full"), normalized("error: connection refused"));
    }

    /// An upgrade that goes through is news to the agent of an incident about that runner's
    /// upgrading, open or reported on: it checks and resolves it instead of waiting on an upgrade
    /// that's over. Other runners', other kinds of incident and settled ones hear nothing.
    #[test]
    fn an_upgrade_that_went_through_is_news_to_its_incident_s_agent() {
        let mut job = crate::updates::new_job("runner", "metal-1", "native", Some("9e912b75a6b6".into()), Some("669cb1ae027a".into()));
        job.state = crate::updates::JobState::Done;
        let incident = |id: &str, wake: &str, runner: &str, state: &str| Incident {
            id: id.into(),
            wake: wake.into(),
            runner: Some(runner.into()),
            state: state.into(),
            agent: Some(format!("agent-{id}")),
            ..Incident::default()
        };
        let all = [
            incident("i1", "upgrade", "metal-1", "reported"),
            incident("i2", "upgrade", "metal-2", "open"),
            incident("i3", "offline", "metal-1", "open"),
            incident("i4", "upgrade", "metal-1", "resolved"),
            incident("i5", "upgrade", "metal-1", "open"),
        ];
        let news = through_news(std::slice::from_ref(&job), &all);
        assert_eq!(news.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(), ["agent-i1", "agent-i5"]);
        let first = news.first().map(|(_, text)| text.as_str()).unwrap_or_default();
        assert!(first.contains("brought it to build 669cb1ae027a") && first.contains("incident i1") && first.contains("report it resolved"), "{first}");
    }
}

/// W20-R08 to R10 on a hub of its own, under a timeout, with kiln's events as the CI would send
/// them and no harness running (each agent's is a program that isn't there).
#[cfg(all(test, unix))]
mod end_to_end {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{Incident, incident, incidents};
    use crate::authz::{Caller, Scope};
    use crate::hub::Hub;
    use crate::ops::{Cx, call};

    fn within(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(f));
        });
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("it hung"),
        }
    }

    /// The first incident `pred` takes, once there is one.
    async fn wait_for(hub: &Hub, what: &str, pred: impl Fn(&Incident) -> bool) -> anyhow::Result<Incident> {
        for _ in 0..400 {
            if let Some(i) = incidents(hub).await.into_iter().find(|i| pred(i)) {
                return Ok(i);
            }
            crate::rt::sleep(Duration::from_millis(25)).await;
        }
        anyhow::bail!("no incident {what}: {:?}", incidents(hub).await.iter().map(|i| (&i.name, &i.title, &i.state, &i.why)).collect::<Vec<_>>())
    }

    /// `stuck` is waiting for a runner, timed from the start of the current wait; what the
    /// dispatcher holds on purpose (an approval, a window, a concurrency group) isn't.
    #[test]
    fn w20_r08_stuck_is_waiting_for_a_runner_not_held() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo = crate::store::Repo { id: "r1".into(), name: "acme/app".into(), clone_url: "https://example.com/acme/app.git".into(), branches: Vec::new(), poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None };
            let repo_id = hub.store.upsert_repo(repo).await?.id;
            let now = crate::util::now_unix();
            hub.store
                .call(move |c| {
                    c.execute("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at) VALUES ('run1', ?1, 7, 'ci', 'push', 'main', 'abc', 'running', ?2)", rusqlite::params![repo_id, now - 7200])?;
                    let legs: [(&str, i64, Option<i64>, Option<&str>); 4] = [
                        ("no-runner-yet", now - 1800, None, None),
                        ("awaiting-approval", now - 1800, None, Some(r#"{"reason":"approval","environment":"production"}"#)),
                        ("just-requeued", now - 3600, Some(now - 120), Some(r#"{"reason":"capacity"}"#)),
                        ("full-pool", now - 3600, Some(now - 1800), Some(r#"{"reason":"capacity"}"#)),
                    ];
                    for (leg, queued, since, reason) in legs {
                        c.execute(
                            "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, queued_at, wait_since, wait_reason) VALUES (?1, 'run1', ?1, ?1, 'queued', '[\"gpu\"]', '[]', '{}', ?2, ?3, ?4)",
                            rusqlite::params![leg, queued, since, reason],
                        )?;
                    }
                    Ok(())
                })
                .await?;
            let a = super::Attachment { wake: vec!["stuck".into()], limits: super::Limits { stuck_minutes: 15, ..super::Limits::default() }, ..super::Attachment::default() };
            let mut found: Vec<String> = super::stuck(&hub, &[a]).await.into_iter().filter_map(|t| t.seen.what.split(", ").nth(1).map(str::to_owned)).collect();
            found.sort();
            anyhow::ensure!(found == ["full-pool", "no-runner-yet"], "{found:?}");
            Ok(())
        })
    }

    fn acme() -> crate::store::Repo {
        crate::store::Repo {
            id: "r1".into(),
            name: "acme/app".into(),
            clone_url: "https://example.com/acme/app.git".into(),
            branches: Vec::new(),
            poll_seconds: 60,
            enabled: true,
            created_at: 0,
            last_polled_at: None,
            last_error: None,
            mode: "upstream".into(),
            default_branch: None,
            mirror: None,
        }
    }

    /// `unroutable`: a job no runner meets what it asks for, a minute on (a runner on its way isn't
    /// news), one incident for each set of unmet requirements, saying what nothing meets.
    #[test]
    fn w20_r08_unroutable_is_what_no_runner_meets() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo_id = hub.store.upsert_repo(acme()).await?.id;
            let now = crate::util::now_unix();
            hub.store
                .call(move |c| {
                    c.execute(
                        "INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at, workflow_id) VALUES ('run1', ?1, 7, 'ci', 'push', 'main', 'abc', 'running', ?2, '.github/workflows/ci.yml')",
                        rusqlite::params![repo_id, now - 600],
                    )?;
                    let legs: [(&str, i64, &str); 3] = [
                        ("windows", now - 300, r#"{"reason":"unroutable","failing":["os == windows"]}"#),
                        ("just-now", now - 10, r#"{"reason":"unroutable","failing":["os == macos"]}"#),
                        ("full-pool", now - 300, r#"{"reason":"capacity"}"#),
                    ];
                    for (leg, since, reason) in legs {
                        c.execute(
                            "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, queued_at, wait_since, wait_reason, job_id) VALUES (?1, 'run1', ?1, ?1, 'queued', '[]', '[]', '{}', ?2, ?2, ?3, ?1)",
                            rusqlite::params![leg, since, reason],
                        )?;
                    }
                    Ok(())
                })
                .await?;
            let on = super::Attachment { wake: vec!["unroutable".into()], ..super::Attachment::default() };
            let found = super::unroutable(&hub, std::slice::from_ref(&on)).await;
            let [t] = found.as_slice() else { anyhow::bail!("{found:?}") };
            anyhow::ensure!(t.seen.what == "run #7, windows" && t.title.contains("no runner meets os == windows") && t.signature.ends_with(":os == windows"), "{t:?}");
            anyhow::ensure!(t.about.repo.as_deref() == Some("acme/app") && t.about.workflow.as_deref() == Some(".github/workflows/ci.yml"), "{t:?}");
            let paused = super::Attachment { paused: true, ..on };
            anyhow::ensure!(super::unroutable(&hub, &[paused]).await.is_empty(), "nothing it wakes is awake");
            Ok(())
        })
    }

    /// `slow`: a green run whose critical path took longer than its workflow's median on its
    /// branch, over its last green runs, by more than the attachment's margin; the jobs beside the
    /// path don't count.
    #[test]
    fn w20_r08_slow_is_its_critical_path_past_the_median() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo_id = hub.store.upsert_repo(acme()).await?.id;
            let rid = repo_id.clone();
            hub.store
                .call(move |c| {
                    // build (100 s), then test; lint (50 s) beside them.
                    for (n, test) in [(1, 200), (2, 190), (3, 210), (4, 200), (5, 330), (6, 420)] {
                        let (run, at) = (format!("run{n}"), 10_000 * n);
                        c.execute(
                            "INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, conclusion, created_at, workflow_id) VALUES (?1, ?2, ?3, 'ci', 'push', 'main', 'abc', 'completed', 'success', ?4, '.github/workflows/ci.yml')",
                            rusqlite::params![run, rid, n, at],
                        )?;
                        for (job, start, took, needs) in [("build", at, 100, "[]"), ("lint", at, 50, "[]"), ("test", at + 100, test, r#"["build"]"#)] {
                            c.execute(
                                "INSERT INTO jobs (id, run_id, key, display, status, conclusion, labels, needs, spec, queued_at, started_at, finished_at, job_id) VALUES (?1, ?2, ?3, ?3, 'completed', 'success', '[]', ?4, '{}', ?5, ?5, ?6, ?3)",
                                rusqlite::params![format!("{run}-{job}"), run, job, needs, start, start + took],
                            )?;
                        }
                    }
                    Ok(())
                })
                .await?;
            let a = super::Attachment { id: "a1".into(), name: "ci-doctor".into(), on: "repo:acme/app".into(), wake: vec!["slow".into()], ..super::Attachment::default() };
            hub.store.kv_set(super::akey(&a.id), serde_json::to_string(&a)?).await?;
            let green = |n: i64| json!({"type": "run.succeeded", "repo": {"id": repo_id, "name": "acme/app"}, "run": {"repo": "acme/app", "id": format!("run{n}"), "number": n, "workflow": ".github/workflows/ci.yml", "name": "CI", "branch": "main", "url": format!("https://kiln.example/runs/run{n}")}});
            // Run 4: three runs before it, a median of 300 s; it took 300 s.
            anyhow::ensure!(super::slow(&hub, &green(4)).await.is_empty(), "as long as usual");
            // Run 5: 430 s against a median of 300 s (runs 1 to 4), 43% over: past the 25% margin.
            let found = super::slow(&hub, &green(5)).await;
            let [(woken, t)] = found.as_slice() else { anyhow::bail!("{found:?}") };
            anyhow::ensure!(woken.name == "ci-doctor" && t.wake == "slow" && t.title.contains("took 430 s, 43% over its median of 300 s"), "{t:?}");
            anyhow::ensure!(t.detail.contains("build → test") && !t.detail.contains("lint") && t.seen.run.as_deref() == Some("run5"), "{}", t.detail);
            // A wider margin: 43% is within it. Too few green runs before it: nothing to go by.
            let wide = super::Attachment { limits: super::Limits { slow_percent: 50, ..super::Limits::default() }, ..a };
            hub.store.kv_set(super::akey(&wide.id), serde_json::to_string(&wide)?).await?;
            anyhow::ensure!(super::slow(&hub, &green(5)).await.is_empty(), "within its margin");
            anyhow::ensure!(super::slow(&hub, &green(3)).await.is_empty(), "two runs before it say nothing");
            Ok(())
        })
    }

    /// W20-R10's daily budgets: past the runner minutes its agents' runs and reruns took today (a
    /// person's don't count), an attachment's next incident waits for a person; a rerun says whom
    /// it was started by.
    #[test]
    fn w20_r10_runner_minutes_a_day() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo_id = hub.store.upsert_repo(acme()).await?.id;
            let day = super::today();
            hub.store
                .call(move |c| {
                    // A person's run (8 min), and a run an agent asked for (6 min).
                    for (n, actor, minutes) in [(1, "user:ada", 8), (2, "agent:abcdefgh for user:ada", 6)] {
                        let run = format!("run{n}");
                        c.execute(
                            "INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, conclusion, created_at, workflow_id, actor) VALUES (?1, ?2, ?3, 'ci', 'push', 'main', 'abc', 'completed', 'failure', ?4, 'ci.yml', ?5)",
                            rusqlite::params![run, repo_id, n, day, actor],
                        )?;
                        c.execute(
                            "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, queued_at, started_at, finished_at, job_id) VALUES (?1, ?2, 'test', 'test', 'completed', '[]', '[]', '{}', ?3, ?3, ?4, 'test')",
                            rusqlite::params![format!("{run}-1"), run, day + 1, day + 1 + minutes * 60],
                        )?;
                    }
                    Ok(())
                })
                .await?;
            let a = super::Attachment { id: "a1".into(), name: "ci-doctor".into(), limits: super::Limits { runner_minutes_per_day: 10, ..super::Limits::default() }, ..super::Attachment::default() };
            let opened = super::Incident { id: "i1".into(), attachment: "a1".into(), agent: Some("abcdefgh12345678".into()), at: day, ..super::Incident::default() };
            anyhow::ensure!(super::over_budget(&hub, &a, std::slice::from_ref(&opened)).await.is_none(), "6 of its 10 minutes");
            // The agent reruns the person's run: its second attempt (5 min) is the agent's.
            let attempt = crate::ci::runs::start_attempt(&hub, "run1", "agent:abcdefgh for user:ada").await?;
            anyhow::ensure!(attempt == 2, "{attempt}");
            hub.store
                .call(move |c| {
                    c.execute(
                        "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, queued_at, started_at, finished_at, job_id, run_attempt) VALUES ('run1-2', 'run1', 'test', 'test', 'completed', '[]', '[]', '{}', ?1, ?1, ?2, 'test', 2)",
                        rusqlite::params![day + 1, day + 1 + 5 * 60],
                    )?;
                    Ok(())
                })
                .await?;
            let why = super::over_budget(&hub, &a, std::slice::from_ref(&opened)).await.unwrap_or_default();
            anyhow::ensure!(why.contains("took 11 runner minutes today, past its limit of 10"), "{why}");
            // Another attachment's agent, or yesterday's incident, isn't this one's to count.
            let theirs = super::Incident { attachment: "a2".into(), ..opened.clone() };
            let yesterday = super::Incident { at: day - 1, ..opened };
            anyhow::ensure!(super::over_budget(&hub, &a, &[theirs, yesterday]).await.is_none(), "not its");
            Ok(())
        })
    }

    fn failed(n: i64, workflow: &str) -> crate::wire::events::ServerEvent {
        use crate::wire::events::{NoticeKind, NoticeRun, RepoRef, RunNotice, ServerEvent};
        let run = NoticeRun {
            repo: "acme/app".into(),
            id: format!("run{n}"),
            number: n,
            workflow: workflow.into(),
            name: "CI".into(),
            branch: "main".into(),
            sha: "0123456789abcdef".into(),
            actor: "ada".into(),
            conclusion: "failure".into(),
            url: format!("https://kiln.example/runs/run{n}"),
            ..NoticeRun::default()
        };
        ServerEvent::RunNotice(Box::new(RunNotice { kind: NoticeKind::RunFailed, repo: RepoRef { id: "r1".into(), name: "acme/app".into() }, run, environments: Vec::new() }))
    }

    #[test]
    fn w20_r08_what_happens_wakes_the_agent_attached_to_it() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = super::run(h).await;
            });
            let cx = Cx { hub: Arc::clone(&hub), caller: Caller::owner() };
            let fail = |(c, m): crate::ops::Fail| anyhow::anyhow!("{c}: {m}");
            let folder = tempfile::tempdir()?;
            let cwd = folder.path().to_string_lossy().into_owned();
            let saved = call(cx.clone(), "attachments.save", json!({"name": "ci-doctor", "on": "repo:acme/app", "harness": {"kind": "claude", "on": "local", "cwd": cwd}, "wake": ["failure", "mention"], "can": ["rerun", "fix"], "playbook": "Find out why, then rerun or fix it.", "limits": {"sessions_per_day": 2}})).await.map_err(fail)?;
            anyhow::ensure!(saved.pointer("/attachment/by") == Some(&json!("owner")), "{saved}");
            let dup = call(cx.clone(), "attachments.save", json!({"name": "ci-doctor", "on": "repo:acme/other", "harness": {"kind": "codex"}})).await.err().map(|(_, m)| m).unwrap_or_default();
            anyhow::ensure!(dup.contains("called ci-doctor already"), "{dup}");

            // A failed run of what it looks after: an incident, and an agent with the bundle.
            hub.emit(&failed(1, ".github/workflows/ci.yml"));
            let first = wait_for(&hub, "for the failure", |i| i.name == "ci-doctor").await?;
            anyhow::ensure!(first.state == "open" && first.title.starts_with("CI failed on main") && first.bundle.contains("Find out why") && first.bundle.contains("incidents.report"), "{first:?}");
            let agent = first.agent.clone().ok_or_else(|| anyhow::anyhow!("no agent"))?;
            let info = hub.harness.info(&agent).ok_or_else(|| anyhow::anyhow!("its agent isn't listed"))?;
            anyhow::ensure!(info.name.starts_with("ci-doctor: CI failed"), "{}", info.name);

            // Its calls are held to what it may do, on what it looks after.
            let caller = crate::mcp::session_caller(&hub, &agent, None).await.map_err(anyhow::Error::msg)?;
            let app = Scope::Repo { repo: "acme/app".into() };
            let other = Scope::Repo { repo: "acme/other".into() };
            anyhow::ensure!(caller.can("runs.rerun", &app) && caller.can("diffs.submit", &app) && caller.can("runs.read", &app), "what it may do");
            anyhow::ensure!(!caller.can("diffs.land", &app) && !caller.can("environments.deploy", &app) && !caller.can("runs.rerun", &other), "no more");

            // The same failure again is the same incident: no second agent.
            hub.emit(&failed(2, ".github/workflows/ci.yml"));
            for _ in 0..400 {
                if incident(&hub, &first.id).await.is_some_and(|i| i.seen.len() == 2) {
                    break;
                }
                crate::rt::sleep(Duration::from_millis(25)).await;
            }
            let again = incident(&hub, &first.id).await.ok_or_else(|| anyhow::anyhow!("gone"))?;
            anyhow::ensure!(again.seen.len() == 2 && incidents(&hub).await.len() == 1, "{again:?}");

            // It says what it was.
            let by_it = Cx { hub: Arc::clone(&hub), caller: caller.clone() };
            let rep = call(by_it.clone(), "incidents.report", json!({"incident": first.id, "class": "regression", "summary": "D12 broke the parser; a revert is up as D13.", "links": ["D13"]}))
                .await
                .map_err(fail)?;
            anyhow::ensure!(rep.pointer("/incident/state") == Some(&json!("reported")) && rep.pointer("/incident/class") == Some(&json!("regression")), "{rep}");
            let dismiss = call(by_it, "incidents.report", json!({"incident": first.id, "state": "dismissed"})).await.err().map(|(c, _)| c.as_u16());
            anyhow::ensure!(dismiss == Some(403), "dismissing is a person's");

            // Paused, the repository's next failure waits for a person.
            call(cx.clone(), "attachments.pause", json!({"on": "repo:acme/app"})).await.map_err(fail)?;
            hub.emit(&failed(3, ".github/workflows/release.yml"));
            let paused = wait_for(&hub, "while paused", |i| i.title.contains("CI failed") && i.state == "waiting").await?;
            anyhow::ensure!(paused.agent.is_none() && paused.why.as_deref().is_some_and(|w| w.contains("paused")), "{paused:?}");
            call(cx.clone(), "attachments.pause", json!({"attachment": "ci-doctor", "paused": false})).await.map_err(fail)?;

            // A mention calls the one it names.
            hub.emit(&crate::wire::events::ServerEvent::Comment(Box::new(crate::wire::events::Comment { diff: "D9".into(), author: "user:ada".into(), body: "@ci-doctor why is this red?".into(), ..Default::default() })));
            let called = wait_for(&hub, "for the mention", |i| i.wake == "mention").await?;
            anyhow::ensure!(called.agent.is_some() && called.bundle.contains("why is this red?"), "{called:?}");

            // Its limit for the day reached (two agents), the next waits for a person.
            hub.emit(&failed(4, ".github/workflows/nightly.yml"));
            let limited = wait_for(&hub, "past its limit", |i| i.state == "waiting" && i.why.as_deref().is_some_and(|w| w.contains("limit"))).await?;
            anyhow::ensure!(limited.agent.is_none(), "{limited:?}");

            // A runner doctor wakes when a machine goes offline.
            call(cx.clone(), "attachments.save", json!({"name": "runner-doctor", "on": "runners", "harness": {"kind": "claude", "on": "local", "cwd": cwd}, "can": ["diagnose", "manage-runners"]}))
                .await
                .map_err(fail)?;
            hub.emit(&crate::wire::events::ServerEvent::RunnerOffline { runner: crate::wire::events::OfflineRunner { name: "metal-1".into(), labels: vec!["linux".into()], gone_for: 180 } });
            let gone = wait_for(&hub, "for the runner", |i| i.name == "runner-doctor").await?;
            anyhow::ensure!(gone.title == "Runner metal-1 is offline" && gone.agent.is_some() && gone.wake == "offline", "{gone:?}");
            let doc = crate::mcp::session_caller(&hub, gone.agent.as_deref().unwrap_or_default(), None).await.map_err(anyhow::Error::msg)?;
            anyhow::ensure!(doc.can("runners.manage", &Scope::Runner { runner: "metal-1".into() }) && !doc.can("runs.rerun", &app), "a runner doctor manages runners, not runs");

            // An app kiln CD deploys turning degraded wakes what looks after its environment.
            call(
                cx.clone(),
                "attachments.save",
                json!({"name": "deploy-doctor", "on": "environment:acme/app:production", "harness": {"kind": "claude", "on": "local", "cwd": cwd}, "wake": ["degraded"], "can": ["diagnose"]}),
            )
            .await
            .map_err(fail)?;
            super::degraded(&hub, "acme/app", "app-staging", "staging", &json!({"health": "Degraded"})).await;
            super::degraded(&hub, "acme/app", "app-production", "production", &json!({"health": "Degraded", "message": "2 of 3 pods fail their readiness probe", "revision": "0123456789abcdef"}))
                .await;
            let sick = wait_for(&hub, "for the environment", |i| i.name == "deploy-doctor").await?;
            anyhow::ensure!(sick.wake == "degraded" && sick.title == "app-production is degraded in production" && sick.agent.is_some(), "{sick:?}");
            anyhow::ensure!(sick.bundle.contains("readiness probe") && sick.bundle.contains("revision 0123456789ab."), "{}", sick.bundle);
            anyhow::ensure!(incidents(&hub).await.iter().filter(|i| i.wake == "degraded").count() == 1, "staging isn't what it looks after");

            // Listed, with what each is doing.
            let list = call(cx, "attachments.list", json!({})).await.map_err(fail)?;
            let names: Vec<&str> = list.get("attachments").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.pointer("/attachment/name").and_then(Value::as_str)).collect();
            anyhow::ensure!(names == ["ci-doctor", "deploy-doctor", "runner-doctor"], "{names:?}");
            Ok(())
        })
    }
}
