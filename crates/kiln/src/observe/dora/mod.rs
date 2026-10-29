//! Delivery: DORA metrics (`docs/observe.md` §10.7), Datadog's DORA Metrics: deployments and
//! failures, from kiln CD's deploys and Datadog's API, kept in a ledger on the server; each
//! deployment's change lead time read from kiln's mirror of its repository; change failures found as
//! Datadog finds them (a rollback, a revert, a hotfix) or marked by hand; the four keys over a range,
//! and each a metric and an event.

pub mod git;
pub mod http;
pub mod ledger;
pub mod levels;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::logs::record::Record;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub use ledger::{ChangeFailure, Commit, Deployment, Failure, Found, Git, Lead, Ledger, Query, Remedy, RemedyKind, Summary};

use crate::hub::Hub;

/// The days the ledger keeps.
pub const KEEP_DAYS: i64 = 400;

/// A line of the ledger's file: an entry as it now stands, or one gone.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Line {
    Deployment(Box<Deployment>),
    Failure(Box<Failure>),
    DeploymentGone(String),
    FailureGone(String),
}

/// The ledger's file: lines appended, rewritten whole once it holds twice what stands.
struct Journal {
    path: PathBuf,
    file: Option<std::fs::File>,
    /// Lines in the file.
    lines: usize,
    /// A write failed: the next is the whole ledger.
    stale: bool,
}

impl Journal {
    fn append(&mut self, lines: &[Line]) -> std::io::Result<()> {
        let mut buf = Vec::with_capacity(256 * lines.len());
        for l in lines {
            serde_json::to_writer(&mut buf, l).map_err(std::io::Error::other)?;
            buf.push(b'\n');
        }
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            self.file = Some(std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?);
        }
        let Some(f) = self.file.as_mut() else { return Err(std::io::Error::other("the ledger's file is not open")) };
        f.write_all(&buf)?;
        f.sync_data()?;
        self.lines += lines.len();
        Ok(())
    }

    /// The file rewritten as `all` (every entry that stands), at once.
    fn rewrite(&mut self, all: &[Line]) -> std::io::Result<()> {
        let mut buf = Vec::with_capacity(256 * all.len());
        for l in all {
            serde_json::to_writer(&mut buf, l).map_err(std::io::Error::other)?;
            buf.push(b'\n');
        }
        self.file = None;
        crate::sync::group::save_atomic(&self.path, &buf)?;
        self.lines = all.len();
        self.stale = false;
        Ok(())
    }
}

/// The ledger read from `path`: each line applied in turn (a line that does not read, a write cut
/// short, passed over), what is older than `keep_from` (ns) dropped. And how many lines it had.
fn load(path: &Path, keep_from: i64) -> (Ledger, usize) {
    let mut l = Ledger::default();
    let Ok(text) = std::fs::read_to_string(path) else { return (l, 0) };
    let (mut lines, mut bad) = (0usize, 0usize);
    for line in text.lines().filter(|s| !s.trim().is_empty()) {
        lines += 1;
        match serde_json::from_str::<Line>(line) {
            Ok(Line::Deployment(d)) => {
                l.put(*d);
            }
            Ok(Line::Failure(f)) => {
                l.put_failure(*f);
            }
            Ok(Line::DeploymentGone(id)) => {
                l.remove(&id);
            }
            Ok(Line::FailureGone(id)) => {
                l.remove_failure(&id);
            }
            Err(_) => bad += 1,
        }
    }
    if bad > 0 {
        tracing::warn!(lines = bad, file = %path.display(), "DORA's ledger: lines that do not read passed over");
    }
    l.expire(keep_from);
    l.shrink();
    (l, lines)
}

/// Every entry of `l` as a line.
fn lines_of(l: &Ledger) -> Vec<Line> {
    let mut out: Vec<Line> = l.deployments().map(|d| Line::Deployment(Box::new(d))).collect();
    out.extend(l.failures().map(|f| Line::Failure(Box::new(f.clone()))));
    out
}

type Key = (String, String);

/// The server's deployments and failures.
pub struct Dora {
    hub: Option<Arc<Hub>>,
    ledger: Mutex<Ledger>,
    journal: Arc<Mutex<Journal>>,
    /// Changes to the ledger, one at a time: its file in its order.
    writing: async_lock::Mutex<()>,
    /// Each service and environment's deployments, one at a time: each finds the one before it.
    turns: Mutex<HashMap<Key, Arc<async_lock::Mutex<()>>>>,
}

static DORA: OnceLock<Arc<Dora>> = OnceLock::new();

/// The server's, once it is open.
pub fn get() -> Option<&'static Arc<Dora>> {
    DORA.get()
}

pub fn set(d: Arc<Dora>) {
    let _ = DORA.set(d);
}

fn now_ns() -> i64 {
    super::clock_ms().saturating_mul(1_000_000)
}

impl Dora {
    /// The ledger kept in `dir` (`dora.jsonl`), commits read through `hub`'s mirrors (none: no lead
    /// times, no reverts or hotfixes found).
    pub fn open(dir: &Path, hub: Option<Arc<Hub>>) -> Arc<Self> {
        let path = dir.join("dora.jsonl");
        let (ledger, lines) = load(&path, now_ns().saturating_sub(KEEP_DAYS * ledger::DAY_NS));
        let (d, f) = ledger.len();
        let mut journal = Journal { path, file: None, lines, stale: false };
        if lines > 2 * (d + f) + 64
            && let Err(e) = journal.rewrite(&lines_of(&ledger))
        {
            tracing::warn!(error = %e, "DORA's ledger: could not be rewritten");
        }
        Arc::new(Self { hub, ledger: Mutex::new(ledger), journal: Arc::new(Mutex::new(journal)), writing: async_lock::Mutex::new(()), turns: Mutex::new(HashMap::new()) })
    }

    /// What `f` makes of the ledger, its lines written to its file (the whole rewritten when the
    /// file holds twice what stands, or a write before failed).
    async fn change<R>(&self, f: impl FnOnce(&mut Ledger) -> (R, Vec<Line>)) -> R {
        let _w = self.writing.lock().await;
        let (r, lines, whole) = {
            let mut l = self.ledger.lock();
            let (r, lines) = f(&mut l);
            let (d, n) = l.len();
            let j = self.journal.lock();
            let whole = (j.stale || j.lines + lines.len() > 2 * (d + n) + 64).then(|| {
                l.expire(now_ns().saturating_sub(KEEP_DAYS * ledger::DAY_NS));
                lines_of(&l)
            });
            (r, lines, whole)
        };
        if lines.is_empty() && whole.is_none() {
            return r;
        }
        let journal = self.journal.clone();
        let done = crate::rt::blocking(move || {
            let mut j = journal.lock();
            let res = match &whole {
                Some(all) => j.rewrite(all),
                None => j.append(&lines),
            };
            if res.is_err() {
                j.stale = true;
            }
            res
        })
        .await;
        match done {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "DORA's ledger: a change was not written; the next writes the whole"),
            Err(e) => tracing::warn!(error = %e, "DORA's ledger: a change was not written; the next writes the whole"),
        }
        r
    }

    fn turn(&self, service: &str, env: &str) -> Arc<async_lock::Mutex<()>> {
        self.turns.lock().entry((service.to_owned(), env.to_owned())).or_default().clone()
    }

    /// What reads the ledger, a moment.
    pub fn read<R>(&self, f: impl FnOnce(&Ledger) -> R) -> R {
        f(&self.ledger.lock())
    }

    /// The commits `d` brings after `prev` (the one before it): none when there is no previous, it
    /// is more than 60 days older, they name no commit or different repositories, or kiln does not
    /// mirror theirs.
    async fn commits(&self, prev: Option<&Deployment>, d: &Deployment) -> Option<Vec<Commit>> {
        let hub = self.hub.as_ref()?;
        let p = prev?;
        if d.finished_at.saturating_sub(p.finished_at) > ledger::LEAD_WINDOW_NS {
            return None;
        }
        let (pg, dg) = (p.git.as_ref()?, d.git.as_ref()?);
        // One repository, however each names it (its URL, kiln's name for it).
        let repo = git::repo_of(hub, &dg.repository_url).await?;
        if git::normal_url(&pg.repository_url) != git::normal_url(&dg.repository_url) && git::repo_of(hub, &pg.repository_url).await.is_none_or(|r| r.id != repo.id) {
            return None;
        }
        match git::between(hub, &repo, &pg.commit_sha, &dg.commit_sha).await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, service = %d.service, "DORA: a deployment's commits could not be read: no lead time");
                None
            }
        }
    }

    /// A deployment taken: kept in its place (its id's, if it has one), its commits read from the one
    /// before it, what it marks failed marked; the one after it read again when it came late; its
    /// metrics and events written. `unless_running`: none when what it deploys is what the one
    /// before it deployed (a redeploy). What it is, as kept, back.
    pub async fn deploy(&self, mut d: Deployment, unless_running: bool) -> Option<Deployment> {
        // A deployment that names no team takes its service's, the catalog's (§10.8).
        if d.team.is_empty()
            && let Some(team) = super::catalog::team_of(&d.service)
        {
            d.team = team;
        }
        let turn = self.turn(&d.service, &d.env);
        let _t = turn.lock().await;
        let (prev, next, existed) = self.read(|l| {
            let prev = l.before(&d.service, &d.env, d.finished_at, &d.id);
            let next = l.after(&d.service, &d.env, d.finished_at, &d.id);
            (prev, next, l.deployment(&d.id).is_some())
        });
        if unless_running && prev.as_ref().is_some_and(|p| same_deploy(p, &d)) {
            return None;
        }
        let commits = self.commits(prev.as_ref(), &d).await;
        let (lead, each, shas, titles) = commits.as_deref().map(|c| ledger::read_commits(d.finished_at, c)).unwrap_or_default();
        (d.lead, d.shas, d.titles) = (lead, shas, titles);
        let id = d.id.clone();
        let (kept, marked) = self
            .change(|l| {
                l.put(d);
                let mut marked: Vec<Deployment> = Vec::new();
                for m in l.detect(&id, commits.as_deref().unwrap_or(&[])) {
                    if l.mark(&m.failed, m.found, Some((&id, m.kind)))
                        && let Some(f) = l.deployment(&m.failed)
                    {
                        marked.push(f);
                    }
                }
                let kept = l.deployment(&id);
                let mut lines: Vec<Line> = marked.iter().map(|f| Line::Deployment(Box::new(f.clone()))).collect();
                lines.extend(kept.iter().map(|k| Line::Deployment(Box::new(k.clone()))));
                ((kept, marked), lines)
            })
            .await;
        // It came before one kept already: that one's commits are from it now.
        if let (Some(n), Some(k)) = (next, kept.as_ref()) {
            self.reread(k, &n).await;
        }
        if let Some(k) = &kept
            && !existed
        {
            emit_deployment(k, &each);
        }
        for f in &marked {
            emit_change_failure(f);
        }
        kept
    }

    /// `next`'s commits read again from `prev`, the one before it now.
    async fn reread(&self, prev: &Deployment, next: &Deployment) {
        let commits = self.commits(Some(prev), next).await;
        let (lead, _, shas, titles) = commits.as_deref().map(|c| ledger::read_commits(next.finished_at, c)).unwrap_or_default();
        let id = next.id.clone();
        self.change(|l| match l.set_commits(&id, lead, &shas, &titles) {
            Some(n) => ((), vec![Line::Deployment(Box::new(n))]),
            None => ((), Vec::new()),
        })
        .await;
    }

    /// A failure taken (or one of its id said again: its end, sent later): kept; counted when it is
    /// new, its duration when it is first over.
    pub async fn fail(&self, f: Failure) -> Failure {
        let was = self
            .change(|l| {
                let was = l.put_failure(f.clone());
                (was, vec![Line::Failure(Box::new(f.clone()))])
            })
            .await;
        emit_failure(&f, was.as_ref());
        f
    }

    /// The deployment `id` marked failed (or not), by hand; its remedy the deployment `remedy` names
    /// (by id, or by version: the first of that version after it, its service and environment's).
    /// What it is now, or why not.
    pub async fn patch(&self, id: &str, failed: bool, remedy: Option<RemedyBy>) -> Result<Deployment, PatchError> {
        let (service, env) = self.read(|l| l.deployment(id).map(|d| (d.service.clone(), d.env.clone()))).ok_or(PatchError::NoDeployment)?;
        let turn = self.turn(&service, &env);
        let _t = turn.lock().await;
        let (changed, now) = self
            .change(|l| {
                let Some(d) = l.deployment(id) else { return (Err(PatchError::NoDeployment), Vec::new()) };
                let remedy = match &remedy {
                    None => None,
                    Some(r) => match r.find(l, &d) {
                        Some(found) if found == d.id => return (Err(PatchError::OwnRemedy), Vec::new()),
                        Some(found) => Some((found, r.kind)),
                        None => return (Err(PatchError::NoRemedy), Vec::new()),
                    },
                };
                let changed = if failed { l.mark(id, Found::Manual, remedy.as_ref().map(|(r, k)| (r.as_str(), *k))) } else { l.unmark(id) };
                let mut lines = Vec::new();
                let now = l.deployment(id);
                if changed {
                    if let Some(r) = remedy.as_ref().and_then(|(r, _)| l.deployment(r)) {
                        lines.push(Line::Deployment(Box::new(r)));
                    }
                    if let Some(old) = d.change_failure.as_ref().and_then(|cf| cf.remedy.as_ref()).and_then(|r| l.deployment(&r.id)) {
                        lines.push(Line::Deployment(Box::new(old)));
                    }
                    lines.extend(now.iter().map(|n| Line::Deployment(Box::new(n.clone()))));
                }
                (Ok((changed && d.change_failure.is_none(), now)), lines)
            })
            .await?;
        let now = now.ok_or(PatchError::NoDeployment)?;
        if changed {
            emit_change_failure(&now);
        }
        Ok(now)
    }

    /// The deployment `id` gone: whether it was there.
    pub async fn remove_deployment(&self, id: &str) -> bool {
        let id = id.to_owned();
        self.change(|l| match l.remove(&id) {
            Some(d) => {
                let mut lines: Vec<Line> = d.remedies.iter().filter_map(|f| l.deployment(f)).map(|f| Line::Deployment(Box::new(f))).collect();
                if let Some(r) = d.change_failure.as_ref().and_then(|cf| cf.remedy.as_ref()).and_then(|r| l.deployment(&r.id)) {
                    lines.push(Line::Deployment(Box::new(r)));
                }
                lines.push(Line::DeploymentGone(id.clone()));
                (true, lines)
            }
            None => (false, Vec::new()),
        })
        .await
    }

    /// The failure `id` gone: whether it was there.
    pub async fn remove_failure(&self, id: &str) -> bool {
        let id = id.to_owned();
        self.change(|l| match l.remove_failure(&id) {
            Some(_) => (true, vec![Line::FailureGone(id.clone())]),
            None => (false, Vec::new()),
        })
        .await
    }
}

/// Whether `a` and `b` deploy the same (their commit, else their version).
fn same_deploy(a: &Deployment, b: &Deployment) -> bool {
    let what = |d: &Deployment| d.git.as_ref().map(|g| g.commit_sha.to_ascii_lowercase()).filter(|s| !s.is_empty()).or_else(|| Some(d.version.clone()).filter(|v| !v.is_empty()));
    what(a).is_some_and(|x| what(b).as_deref() == Some(x.as_str()))
}

/// Why a deployment was not marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchError {
    NoDeployment,
    NoRemedy,
    OwnRemedy,
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoDeployment => "no such deployment",
            Self::NoRemedy => "no deployment of its service and environment is the remediation named",
            Self::OwnRemedy => "a deployment is not its own remediation",
        })
    }
}

/// The deployment that remedied a failed one, as a patch names it.
#[derive(Debug, Clone)]
pub struct RemedyBy {
    pub id: Option<String>,
    pub version: Option<String>,
    pub kind: RemedyKind,
}

impl RemedyBy {
    /// Its id in `l`: the deployment of that id, or the first of that version after `failed`, its
    /// service and environment's.
    fn find(&self, l: &Ledger, failed: &Deployment) -> Option<String> {
        if let Some(id) = &self.id {
            return l.deployment(id).filter(|d| d.service == failed.service && d.env == failed.env).map(|d| d.id);
        }
        let version = self.version.as_deref()?;
        let mut at = (failed.finished_at, failed.id.clone());
        while let Some(n) = l.after(&failed.service, &failed.env, at.0, &at.1) {
            if n.version == version {
                return Some(n.id.clone());
            }
            at = (n.finished_at, n.id.clone());
        }
        None
    }
}

/// A duration said shortly: `42s`, `12m`, `3h 5m`, `2d 4h`.
fn took(s: f64) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let s = s.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3_600 => format!("{}m", s / 60),
        3_600..86_400 => format!("{}h {}m", s / 3_600, s % 3_600 / 60),
        _ => format!("{}d {}h", s / 86_400, s % 86_400 / 3_600),
    }
}

/// A deployment's tags: its service, environment and team.
fn tags_of<'a>(service: &'a str, env: &'a str, team: &'a str) -> Vec<(&'a str, &'a str)> {
    let mut t = vec![("service", service)];
    if !env.is_empty() {
        t.push(("env", env));
    }
    if !team.is_empty() {
        t.push(("team", team));
    }
    t
}

fn count(out: &mut Vec<(SeriesKey, Vec<(i64, f64)>)>, metric: &str, tags: &[(&str, &str)], ts_ms: i64) {
    if let Ok(k) = SeriesKey::new(metric, tags.iter().copied(), Kind::Count) {
        out.push((k, vec![(ts_ms, 1.0)]));
    }
}

fn distribution(out: &mut Vec<(SeriesKey, Vec<(i64, f64)>)>, metric: &str, tags: &[(&str, &str)], ts_ms: i64, values: Vec<f64>) {
    if values.is_empty() {
        return;
    }
    match kiln_obs::dist::series_of(metric, tags, &[(ts_ms, values)]) {
        Ok(s) => out.extend(s),
        Err(e) => tracing::warn!(error = %e, metric, "DORA: a distribution was not written"),
    }
}

fn event(service: &str, ts_ns: i64, severity: u8, message: String, mut attrs: Vec<(String, String)>, tags: &[String]) -> Record {
    attrs.extend(tags.iter().filter_map(|t| t.split_once(':')).map(|(k, v)| (k.to_owned(), v.to_owned())));
    Record { ts: ts_ns, severity, service: service.to_owned(), host: String::new(), source: "dora".into(), message, attrs, trace_id: None, span_id: None }
}

/// A new deployment's metrics (`dora.deployments`, `dora.change_lead_time` of each commit) and
/// event.
fn emit_deployment(d: &Deployment, each: &[f64]) {
    let Some(o) = super::api::get() else { return };
    let ts = d.finished_at / 1_000_000;
    let tags = tags_of(&d.service, &d.env, &d.team);
    let mut out = Vec::new();
    count(&mut out, "dora.deployments", &tags, ts);
    distribution(&mut out, "dora.change_lead_time", &tags, ts, each.to_vec());
    o.write(out);
    let mut attrs = vec![("dora.kind".to_owned(), "deployment".to_owned()), ("dora.id".to_owned(), d.id.clone())];
    for (k, v) in [("env", &d.env), ("team", &d.team), ("version", &d.version)] {
        if !v.is_empty() {
            attrs.push((k.to_owned(), v.clone()));
        }
    }
    if let Some(g) = &d.git {
        attrs.push(("git.commit.sha".into(), g.commit_sha.clone()));
        attrs.push(("git.repository_url".into(), g.repository_url.clone()));
    }
    if let Some(l) = d.lead {
        attrs.push(("dora.commits".into(), l.commits.to_string()));
        attrs.push(("dora.change_lead_time".into(), format!("{:.0}", l.mean_s)));
    }
    let what = if d.version.is_empty() { d.git.as_ref().map(|g| g.commit_sha.get(..12).unwrap_or(&g.commit_sha).to_owned()).unwrap_or_default() } else { d.version.clone() };
    let message = format!("{} deployed{}{}", d.service, if what.is_empty() { String::new() } else { format!(" {what}") }, if d.env.is_empty() { String::new() } else { format!(" to {}", d.env) });
    o.log(vec![event(&d.service, d.finished_at, 9, message, attrs, &d.custom_tags)]);
}

/// A deployment newly marked failed: `dora.change_failures` at its finish, `dora.recovery_time`
/// when it was remedied, and an event.
fn emit_change_failure(d: &Deployment) {
    let Some(o) = super::api::get() else { return };
    let Some(cf) = &d.change_failure else { return };
    let tags = tags_of(&d.service, &d.env, &d.team);
    let mut out = Vec::new();
    count(&mut out, "dora.change_failures", &tags, d.finished_at / 1_000_000);
    if let (Some(r), Some(ns)) = (&cf.remedy, d.recovery_ns()) {
        #[allow(clippy::cast_precision_loss)]
        distribution(&mut out, "dora.recovery_time", &tags, r.finished_at / 1_000_000, vec![ns as f64 / 1e9]);
    }
    o.write(out);
    let found = match cf.found {
        Found::Rollback => "a rollback",
        Found::Revert => "a revert",
        Found::Hotfix => "a hotfix",
        Found::Manual => "hand",
    };
    let mut attrs = vec![("dora.kind".to_owned(), "change_failure".to_owned()), ("dora.id".to_owned(), d.id.clone()), ("dora.found".to_owned(), found.to_owned())];
    if let Some(r) = &cf.remedy {
        attrs.push(("dora.remedy".into(), r.id.clone()));
    }
    let message = format!("{} deployment {} marked failed by {found}", d.service, d.id);
    o.log(vec![event(&d.service, now_ns(), 13, message, attrs, &d.custom_tags)]);
}

/// A failure's metrics and events: `dora.failures` when it is new, `dora.time_to_restore` when it is
/// first over.
fn emit_failure(f: &Failure, was: Option<&Failure>) {
    let Some(o) = super::api::get() else { return };
    let service = f.services.first().map_or("", String::as_str);
    let mut out = Vec::new();
    let mut records = Vec::new();
    let attrs = |kind: &str| {
        let mut a = vec![("dora.kind".to_owned(), kind.to_owned()), ("dora.id".to_owned(), f.id.clone()), ("services".to_owned(), f.services.join(","))];
        for (k, v) in [("env", &f.env), ("team", &f.team), ("severity", &f.severity)] {
            if !v.is_empty() {
                a.push((k.to_owned(), v.clone()));
            }
        }
        a
    };
    let name = if f.name.is_empty() { "a failure".to_owned() } else { f.name.clone() };
    if was.is_none() {
        for s in f.services.iter().map(String::as_str).chain(f.services.is_empty().then_some("")) {
            count(&mut out, "dora.failures", &tags_of(s, &f.env, &f.team), f.started_at / 1_000_000);
        }
        records.push(event(service, f.started_at, 17, format!("{name} began"), attrs("failure"), &f.custom_tags));
    }
    if let Some(end) = f.finished_at
        && was.is_none_or(|w| w.finished_at.is_none())
    {
        #[allow(clippy::cast_precision_loss)]
        let s = end.saturating_sub(f.started_at).max(0) as f64 / 1e9;
        for svc in f.services.iter().map(String::as_str).chain(f.services.is_empty().then_some("")) {
            distribution(&mut out, "dora.time_to_restore", &tags_of(svc, &f.env, &f.team), end / 1_000_000, vec![s]);
        }
        records.push(event(service, end, 9, format!("{name} over after {}", took(s)), attrs("failure_over"), &f.custom_tags));
    }
    o.write(out);
    if !records.is_empty() {
        o.log(records);
    }
}

/// A kiln CD deploy (`docs/observe.md` §10.7): a deployment of its app (in its environment, owned by
/// its `team` label), its commit its first git source's (`source`: that source's repository and
/// commit), when it succeeded and changed what runs.
pub fn cd_deployed(d: &kiln_cd::state::Deployment, started_ms: Option<u64>, env: Option<&str>, source: Option<(&str, &str)>, team: Option<&str>) {
    let Some(dora) = get() else { return };
    let Some(dep) = cd_deployment(d, started_ms, env, source, team) else { return };
    let dora = dora.clone();
    crate::rt::spawn(async move {
        dora.deploy(dep, true).await;
    });
}

/// What DORA keeps of a kiln CD deploy: none of one that failed.
pub fn cd_deployment(d: &kiln_cd::state::Deployment, started_ms: Option<u64>, env: Option<&str>, source: Option<(&str, &str)>, team: Option<&str>) -> Option<Deployment> {
    if !d.succeeded {
        return None;
    }
    let git = source.filter(|(r, c)| !r.trim().is_empty() && git::is_sha(c.trim())).map(|(r, c)| Git { commit_sha: c.trim().to_ascii_lowercase(), repository_url: r.trim().to_owned() });
    let finished = i64::try_from(d.at_ms).unwrap_or(i64::MAX).saturating_mul(1_000_000);
    let started = started_ms.and_then(|s| i64::try_from(s).ok()).map_or(finished, |s| s.saturating_mul(1_000_000).min(finished));
    let app: String = d.app.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    Some(Deployment {
        id: format!("kiln-cd:{app}:{:08}", d.op),
        service: d.app.clone(),
        env: env.unwrap_or_default().to_owned(),
        team: team.unwrap_or_default().to_owned(),
        version: d.artifacts.iter().find_map(|a| a.tag.clone()).unwrap_or_default(),
        started_at: started,
        finished_at: finished,
        git,
        custom_tags: vec![format!("trigger:{}", d.trigger)],
        source: "kiln.cd".into(),
        lead: None,
        change_failure: None,
        remedies: Vec::new(),
        shas: Vec::new(),
        titles: Vec::new(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const H: i64 = 3_600 * ledger::NS_PER_S;

    /// A deployment `h` hours after `t0` (the test's, taken once: the clock moves as it runs).
    fn dep(t0: i64, id: &str, h: i64, c: char) -> Deployment {
        Deployment {
            id: id.into(),
            service: "shop".into(),
            env: "prod".into(),
            team: String::new(),
            version: format!("v{h}"),
            started_at: t0 + h * H - 1,
            finished_at: t0 + h * H,
            git: Some(Git { commit_sha: c.to_string().repeat(40), repository_url: "https://github.com/acme/shop".into() }),
            custom_tags: Vec::new(),
            source: "api".into(),
            lead: None,
            change_failure: None,
            remedies: Vec::new(),
            shas: Vec::new(),
            titles: Vec::new(),
        }
    }

    /// Every entry, as its line says it: two ledgers alike say the same.
    fn said(d: &Dora) -> Vec<String> {
        let mut v: Vec<String> = d.read(lines_of).iter().map(|x| serde_json::to_string(x).unwrap()).collect();
        v.sort();
        v
    }

    /// What is taken (deployments, a rollback's mark, a mark by hand, a failure and its end, a
    /// removal) is read back alike once the server starts again; a line cut short by a crash is
    /// passed over; the file is rewritten once it holds twice what stands.
    #[test]
    fn the_ledger_is_read_back_as_it_was_kept() {
        let t0 = now_ns() - 100 * H;
        let dir = tempfile::tempdir().unwrap();
        let d = Dora::open(dir.path(), None);
        let taking = d.clone();
        crate::rt::block_on(async move {
            let d = taking;
            for (id, h, c) in [("a", 0, '1'), ("b", 1, '2'), ("c", 2, '3'), ("back", 3, '1'), ("e", 4, '4'), ("f", 5, '5')] {
                d.deploy(dep(t0, id, h, c), false).await.unwrap();
            }
            d.patch("e", true, Some(RemedyBy { id: Some("f".into()), version: None, kind: RemedyKind::Rollforward })).await.unwrap();
            assert_eq!(d.patch("e", true, Some(RemedyBy { id: Some("e".into()), version: None, kind: RemedyKind::Rollback })).await.unwrap_err(), PatchError::OwnRemedy);
            assert_eq!(d.patch("nope", true, None).await.unwrap_err(), PatchError::NoDeployment);
            let f = Failure {
                id: "inc-1".into(),
                services: vec!["shop".into()],
                env: "prod".into(),
                team: String::new(),
                name: "down".into(),
                severity: "high".into(),
                started_at: now_ns() - 2 * H,
                finished_at: None,
                git: None,
                version: String::new(),
                custom_tags: Vec::new(),
            };
            d.fail(f.clone()).await;
            d.fail(Failure { finished_at: Some(now_ns() - H), ..f }).await;
            assert!(d.remove_deployment("a").await && !d.remove_deployment("a").await);
            anyhow::Ok(())
        })
        .unwrap();
        d.read(|l| {
            assert!(l.deployment("b").unwrap().change_failure.is_some(), "b rolled back");
            assert_eq!(l.deployment("e").unwrap().recovery_ns(), Some(H));
            assert!(l.failure("inc-1").unwrap().finished_at.is_some());
        });
        let before = said(&d);
        drop(d);
        // A crash mid-write: a line cut short.
        std::fs::OpenOptions::new().append(true).open(dir.path().join("dora.jsonl")).unwrap().write_all(b"{\"deployment\":{\"id\":\"half").unwrap();
        let again = Dora::open(dir.path(), None);
        assert_eq!(said(&again), before);
        // A deployment said again and again: the file rewritten once it holds twice what stands.
        let taking = again.clone();
        crate::rt::block_on(async move {
            for _ in 0..200 {
                taking.deploy(dep(t0, "f", 5, '5'), false).await.unwrap();
            }
            anyhow::Ok(())
        })
        .unwrap();
        let lines = std::fs::read_to_string(dir.path().join("dora.jsonl")).unwrap().lines().count();
        let (n, m) = again.read(Ledger::len);
        assert!(lines <= 2 * (n + m) + 64 + 2, "{lines} lines for {} entries", n + m);
        assert_eq!(said(&Dora::open(dir.path(), None)), said(&again));
    }

    /// A kiln CD deploy as DORA keeps it: its app the service, its environment, team, first git
    /// source and commit, its start; a failed one none, a source with no commit no git.
    #[test]
    fn a_kiln_cd_deploy_is_a_deployment_of_its_app() {
        let d = kiln_cd::state::Deployment {
            app: "shop web".into(),
            op: 7,
            revision: Some("main".into()),
            key: None,
            trigger: "auto".into(),
            by: None,
            succeeded: true,
            message: String::new(),
            applied: 3,
            pruned: 0,
            at_ms: 1_790_000_060_000,
            id: 0,
            artifacts: vec![kiln_cd::apps::ArtifactRef { name: "web".into(), reference: "r".into(), repository: "acme/web".into(), tag: Some("v1.2".into()), digest: "sha256:x".into() }],
            commits: vec![Some("A".repeat(40))],
            content: None,
        };
        let sha = "A".repeat(40);
        let dep = cd_deployment(&d, Some(1_790_000_000_000), Some("prod"), Some(("acme/shop", &sha)), Some("core")).unwrap();
        assert_eq!((dep.id.as_str(), dep.service.as_str(), dep.env.as_str(), dep.team.as_str(), dep.version.as_str()), ("kiln-cd:shop_web:00000007", "shop web", "prod", "core", "v1.2"));
        assert_eq!((dep.started_at, dep.finished_at), (1_790_000_000_000_000_000, 1_790_000_060_000_000_000));
        assert_eq!(dep.git, Some(Git { commit_sha: "a".repeat(40), repository_url: "acme/shop".into() }));
        assert_eq!(dep.custom_tags, vec!["trigger:auto".to_owned()]);
        assert!(cd_deployment(&kiln_cd::state::Deployment { succeeded: false, ..d.clone() }, None, None, None, None).is_none());
        let bare = cd_deployment(&d, None, None, Some(("acme/shop", "main")), None).unwrap();
        assert_eq!((bare.git, bare.started_at, bare.env.as_str()), (None, bare.finished_at, ""));
    }

    /// A deployment that comes after a later one is put in its place: the later one's previous is
    /// it, and a rollback past it is found as if they had come in order. A redeploy of what runs is
    /// none, when asked so (kiln CD's self-heals).
    #[test]
    fn deployments_go_in_their_place_whatever_order_they_come_in() {
        let t0 = now_ns() - 100 * H;
        let dir = tempfile::tempdir().unwrap();
        let d = Dora::open(dir.path(), None);
        crate::rt::block_on(async move {
            d.deploy(dep(t0, "one", 0, '1'), false).await.unwrap();
            d.deploy(dep(t0, "three", 2, '3'), false).await.unwrap();
            // Late: between them.
            d.deploy(dep(t0, "two", 1, '2'), false).await.unwrap();
            assert_eq!(d.read(|l| l.before("shop", "prod", dep(t0, "three", 2, '3').finished_at, "three").map(|x| x.id.clone())), Some("two".into()));
            d.deploy(dep(t0, "back", 3, '1'), false).await.unwrap();
            assert_eq!(d.read(|l| l.deployment("two").and_then(|x| x.change_failure.clone()).map(|cf| cf.found)), Some(Found::Rollback));
            assert!(d.deploy(dep(t0, "same", 4, '1'), true).await.is_none(), "a redeploy of what runs");
            assert!(d.deploy(dep(t0, "new", 5, '9'), true).await.is_some());
            anyhow::Ok(())
        })
        .unwrap();
    }
}
