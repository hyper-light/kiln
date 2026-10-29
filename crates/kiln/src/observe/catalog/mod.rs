//! Service Catalog (`docs/observe.md` §10.8), Datadog's Software Catalog: services' definitions
//! (Datadog's schema v2.2) from Datadog's API and from `service.datadog.yaml` in the repositories
//! kiln mirrors; every service defined or seen (its traces, its deployments), with what kiln knows of
//! it, and Datadog's three scorecards of it.

pub mod definition;
pub mod http;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub use definition::Definition;

use crate::hub::Hub;

/// How often the repositories' definitions are read again.
pub const REREAD: Duration = Duration::from_secs(300);
/// The files a repository's definitions are read from, at its root, in this order.
pub const FILES: [&str; 2] = ["service.datadog.yaml", "service.datadog.yml"];
/// How long what is costly to know of every service (traces of the last day, logs correlated with
/// them) is kept before it is read again.
const FRESH: Duration = Duration::from_secs(300);

/// How often the repositories are read again and the costly facts kept: `KILN_OBSERVE_CATALOG_EVERY_MS`
/// (a test's), else [`REREAD`].
fn every() -> Duration {
    std::env::var("KILN_OBSERVE_CATALOG_EVERY_MS").ok().and_then(|v| v.parse().ok()).map_or(REREAD, Duration::from_millis)
}
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// Where a definition came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Datadog's API (or kiln's).
    Api,
    /// A repository kiln mirrors: its name, the file, the commit it was read at.
    Repo { repo: String, file: String, sha: String },
}

/// A definition kept, where it came from and when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub definition: Definition,
    pub source: Source,
    /// When it was made or last changed (ms).
    pub modified: i64,
}

/// Why a definition was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutError {
    Invalid(String),
    /// A repository defines the service: changed there.
    InRepo(String),
    Unsaved(String),
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(e) => f.write_str(e),
            Self::InRepo(r) => write!(f, "the repository {r} defines this service: change its service.datadog.yaml"),
            Self::Unsaved(e) => write!(f, "not kept: {e}"),
        }
    }
}

/// What kiln knows of a service, for its page and its scorecards.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Facts {
    /// The last hour's requests, errors and 95th percentile (s), from the trace metrics.
    pub requests: f64,
    pub errors: f64,
    pub p95_s: Option<f64>,
    /// Seen in traces in the last day.
    pub traced: bool,
    /// Its issues still open (§8.8).
    pub open_issues: u32,
    /// The monitors and SLOs that read it (`service:<name>` in their query or tags).
    pub monitors: Vec<Related>,
    pub slos: Vec<Related>,
    /// Its deployments (§10.7): the last (ms), how many in 30 days, how many of those failed.
    pub last_deployment: Option<i64>,
    pub deployments_30d: u64,
    pub failed_30d: u64,
    /// Its logs of the last day that carry a trace id.
    pub logs_traced: u64,
}

/// A monitor or SLO that reads a service: its id, name and how it stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Related {
    pub id: String,
    pub name: String,
    pub state: String,
}

/// A scorecard rule, passed or not, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub scorecard: &'static str,
    pub id: &'static str,
    pub name: &'static str,
    pub passed: bool,
    pub why: String,
}

/// Datadog's three scorecards' rules of a service: its definition (none: a service only seen), what
/// kiln knows of it, now (ms).
pub fn rules(k: Option<&Kept>, f: &Facts, now: i64) -> Vec<Rule> {
    let d = k.map(|k| &k.definition);
    let rule = |scorecard, id, name, passed: bool, yes: String, no: &str| Rule { scorecard, id, name, passed, why: if passed { yes } else { no.to_owned() } };
    const READY: &str = "Production readiness";
    const OBSERVED: &str = "Observability";
    const OWNED: &str = "Ownership and documentation";
    let recent = f.last_deployment.is_some_and(|t| now.saturating_sub(t) <= 90 * DAY_MS);
    let repo = d.is_some_and(|d| d.links_to(&["repo"])) || k.is_some_and(|k| matches!(k.source, Source::Repo { .. }));
    vec![
        rule(READY, "slos", "Has SLOs", !f.slos.is_empty(), format!("{} read it", f.slos.len()), "no SLO reads it"),
        rule(READY, "monitors", "Has monitors", !f.monitors.is_empty(), format!("{} read it", f.monitors.len()), "no monitor reads it"),
        rule(READY, "on_call", "Names an on-call", d.is_some_and(Definition::on_call), "PagerDuty or Opsgenie".into(), "no PagerDuty or Opsgenie service in its definition"),
        rule(READY, "deployed", "Deployed in the last 3 months", recent, "it was".into(), "no deployment of it in 3 months"),
        rule(OBSERVED, "deployment_tracking", "Its deployments are tracked", f.deployments_30d > 0, format!("{} in 30 days", f.deployments_30d), "no deployment of it in 30 days"),
        rule(
            OBSERVED,
            "logs_correlation",
            "Its logs are correlated with its traces",
            f.logs_traced > 0,
            format!("{} of the last day's logs carry a trace id", f.logs_traced),
            "none of the last day's logs carries a trace id",
        ),
        rule(OWNED, "team", "Has a team", d.and_then(|d| d.team.as_deref()).is_some_and(|t| !t.trim().is_empty()), d.and_then(|d| d.team.clone()).unwrap_or_default(), "no team in its definition"),
        rule(OWNED, "contacts", "Has contacts", d.is_some_and(|d| !d.contacts.is_empty()), format!("{}", d.map_or(0, |d| d.contacts.len())), "no contact in its definition"),
        rule(OWNED, "repos", "Has a code repository", repo, "linked".into(), "no repo link, nor defined in a repository"),
        rule(OWNED, "docs", "Has docs", d.is_some_and(|d| d.links_to(&["doc", "runbook"])), "linked".into(), "no doc or runbook link"),
    ]
}

/// What is costly to know of every service, kept a while.
#[derive(Debug, Default)]
struct Costly {
    at: Option<std::time::Instant>,
    traced: BTreeSet<String>,
    logs_traced: HashMap<String, u64>,
}

/// The server's catalog.
pub struct Catalog {
    hub: Option<Arc<Hub>>,
    path: PathBuf,
    api: Mutex<BTreeMap<String, Kept>>,
    repos: Mutex<BTreeMap<String, Kept>>,
    costly: async_lock::Mutex<Costly>,
}

static CATALOG: OnceLock<Arc<Catalog>> = OnceLock::new();

pub fn get() -> Option<&'static Arc<Catalog>> {
    CATALOG.get()
}

pub fn set(c: Arc<Catalog>) {
    let _ = CATALOG.set(c);
}

/// The team the catalog names for `service`: what a deployment that names none takes (§10.7).
pub fn team_of(service: &str) -> Option<String> {
    get()?.definition(service).and_then(|k| k.definition.team).map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    definitions: Vec<Kept>,
}

impl Catalog {
    /// The definitions kept in `dir` (`catalog.json`); the repositories' read through `hub`.
    pub fn open(dir: &Path, hub: Option<Arc<Hub>>) -> Arc<Self> {
        let path = dir.join("catalog.json");
        let saved: Saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let api = saved.definitions.into_iter().map(|k| (k.definition.dd_service.clone(), k)).collect();
        Arc::new(Self { hub, path, api: Mutex::new(api), repos: Mutex::new(BTreeMap::new()), costly: async_lock::Mutex::new(Costly::default()) })
    }

    fn save(&self, api: &BTreeMap<String, Kept>) -> Result<(), String> {
        let saved = Saved { definitions: api.values().cloned().collect() };
        let bytes = serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?;
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())
    }

    /// The definition of `service`: a repository's before the API's.
    pub fn definition(&self, service: &str) -> Option<Kept> {
        self.repos.lock().get(service).cloned().or_else(|| self.api.lock().get(service).cloned())
    }

    /// Every definition, by service: a repository's before the API's.
    pub fn definitions(&self) -> BTreeMap<String, Kept> {
        let mut all = self.api.lock().clone();
        all.extend(self.repos.lock().iter().map(|(k, v)| (k.clone(), v.clone())));
        all
    }

    /// A definition made or changed through the API.
    pub fn put(&self, d: Definition) -> Result<Kept, PutError> {
        definition::check(&d).map_err(PutError::Invalid)?;
        let name = d.dd_service.trim().to_owned();
        if let Some(k) = self.repos.lock().get(&name)
            && let Source::Repo { repo, .. } = &k.source
        {
            return Err(PutError::InRepo(repo.clone()));
        }
        let mut api = self.api.lock();
        let kept = Kept { definition: Definition { dd_service: name.clone(), ..d }, source: Source::Api, modified: super::clock_ms() };
        api.insert(name, kept.clone());
        self.save(&api).map_err(PutError::Unsaved)?;
        Ok(kept)
    }

    /// A definition the API made removed: whether there was one. One a repository makes is refused.
    pub fn remove(&self, service: &str) -> Result<bool, PutError> {
        if let Some(k) = self.repos.lock().get(service)
            && let Source::Repo { repo, .. } = &k.source
        {
            return Err(PutError::InRepo(repo.clone()));
        }
        let mut api = self.api.lock();
        let had = api.remove(service).is_some();
        if had {
            self.save(&api).map_err(PutError::Unsaved)?;
        }
        Ok(had)
    }

    /// The repositories' definitions read again: each mirrored repository's `service.datadog.yaml`
    /// at its default branch's head. A file that does not read is said in the log and passed over
    /// (what it defined before is kept no more); a service two repositories define is the first's
    /// (by name). How many are defined.
    pub async fn read_repos(&self) -> usize {
        let Some(hub) = self.hub.as_ref() else { return 0 };
        let Ok(repos) = hub.store.repos().await else { return self.repos.lock().len() };
        let mut found: BTreeMap<String, Kept> = BTreeMap::new();
        let mut sorted = repos;
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for repo in sorted {
            let path = crate::gitops::mirror_path(hub, &repo.name);
            if !path.join("HEAD").exists() {
                continue;
            }
            let p = path.to_string_lossy().into_owned();
            let sha = crate::gitops::run_git(hub, &["-C", &p, "rev-parse", "HEAD"], crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
            for file in FILES {
                let Ok(bytes) = crate::gitops::run_git(hub, &["-C", &p, "show", &format!("HEAD:{file}")], crate::gitops::LS_REMOTE_TIMEOUT).await else { continue };
                match definition::parse(&String::from_utf8_lossy(&bytes)) {
                    Ok(defs) => {
                        for d in defs {
                            let name = d.dd_service.trim().to_owned();
                            if found.contains_key(&name) {
                                tracing::warn!(service = %name, repo = %repo.name, "the catalog: a service another repository defines too: the first's kept");
                                continue;
                            }
                            let source = Source::Repo { repo: repo.name.clone(), file: file.to_owned(), sha: sha.clone() };
                            found.insert(name, Kept { definition: d, source, modified: super::clock_ms() });
                        }
                    }
                    Err(e) => tracing::warn!(repo = %repo.name, file, error = %e, "the catalog: a service definition file that does not read"),
                }
                break;
            }
        }
        let n = found.len();
        // What did not change keeps when it was read.
        let mut repos = self.repos.lock();
        for (name, k) in &mut found {
            if let Some(was) = repos.get(name)
                && was.definition == k.definition
                && was.source == k.source
            {
                k.modified = was.modified;
            }
        }
        *repos = found;
        n
    }

    /// Reads the repositories' definitions now and every [`REREAD`], until `shutdown`.
    pub async fn run(self: Arc<Self>, shutdown: crate::rt::Shutdown) {
        loop {
            self.read_repos().await;
            if !shutdown.sleep(every()).await {
                return;
            }
        }
    }

    /// What is costly to know of every service: who was traced in the last day, how many of each
    /// one's logs carry a trace id. Kept [`FRESH`].
    async fn costly(&self, now: i64) -> (BTreeSet<String>, HashMap<String, u64>) {
        let mut c = self.costly.lock().await;
        if c.at.is_none_or(|t| t.elapsed() > FRESH.min(every())) {
            c.traced = traced_since(now - DAY_MS, now).await.unwrap_or_default();
            c.logs_traced = logs_traced(now - DAY_MS, now).await.unwrap_or_default();
            c.at = Some(std::time::Instant::now());
        }
        (c.traced.clone(), c.logs_traced.clone())
    }

    /// Every service, defined or seen, with what kiln knows of it, now (ms).
    pub async fn services(&self, now: i64) -> Vec<(String, Option<Kept>, Facts)> {
        let defined = self.definitions();
        let (traced, logs) = self.costly(now).await;
        let red = red_by_service(now - HOUR_MS, now).await.unwrap_or_default();
        let dora: BTreeSet<String> = super::dora::get().map(|d| d.read(|l| l.facets().0)).unwrap_or_default().into_iter().collect();
        let mut names: BTreeSet<String> = defined.keys().cloned().collect();
        names.extend(traced.iter().cloned());
        names.extend(dora);
        let monitors = monitors();
        let slos = slos();
        let issues = open_issues();
        names
            .into_iter()
            .map(|name| {
                let facts = facts_of(&name, now, red.get(&name), traced.contains(&name), logs.get(&name).copied().unwrap_or(0), &monitors, &slos, &issues);
                let kept = defined.get(&name).cloned();
                (name, kept, facts)
            })
            .collect()
    }

    /// One service (defined or seen), with what kiln knows of it now (its own, not the list's kept
    /// facts); none of one neither.
    pub async fn service(&self, name: &str, now: i64) -> Option<(Option<Kept>, Facts)> {
        let kept = self.definition(name);
        let hour = red_of(name, now - HOUR_MS, now).await;
        let traced = hour.is_some() || red_of(name, now - DAY_MS, now).await.is_some();
        let deployed = super::dora::get().is_some_and(|d| d.read(|l| l.facets().0.iter().any(|s| s == name)));
        if kept.is_none() && !traced && !deployed {
            return None;
        }
        let logs = logs_traced_of(name, now - DAY_MS, now).await.unwrap_or(0);
        let facts = facts_of(name, now, hour.as_ref(), traced, logs, &monitors(), &slos(), &open_issues());
        Some((kept, facts))
    }
}

/// The services `text` (a query, or a tag) names as `service:<name>`, each a whole value: what a
/// monitor or an SLO reads.
pub fn services_named(text: &str) -> impl Iterator<Item = &str> {
    const KEY: &str = "service:";
    text.match_indices(KEY).filter_map(move |(at, _)| {
        let before = text.get(..at).and_then(|s| s.chars().next_back());
        if before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '@')) {
            return None;
        }
        let rest = text.get(at + KEY.len()..)?;
        let end = rest.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '/'))).unwrap_or(rest.len());
        rest.get(..end).filter(|v| !v.is_empty())
    })
}

/// Whether `text` names `service` (`service:<name>`, a whole value).
pub fn names_service(text: &str, service: &str) -> bool {
    services_named(text).any(|s| s == service)
}

/// Each service's monitors or SLOs (their id, name, query, tags and state), by the services their
/// queries and tags name: one look at each, not one for each service.
pub fn by_service(all: Vec<(String, String, String, Vec<String>, String)>) -> HashMap<String, Vec<Related>> {
    let mut out: HashMap<String, Vec<Related>> = HashMap::new();
    for (id, name, query, tags, state) in all {
        let mut named: BTreeSet<&str> = services_named(&query).collect();
        named.extend(tags.iter().filter_map(|t| t.strip_prefix("service:")));
        for s in named {
            out.entry(s.to_owned()).or_default().push(Related { id: id.clone(), name: name.clone(), state: state.clone() });
        }
    }
    out
}

/// The monitors (their id, name, state now) by the services they read.
fn monitors() -> HashMap<String, Vec<Related>> {
    let Ok(ms) = super::api::monitors_kept() else { return HashMap::new() };
    by_service(ms.list().into_iter().map(|m| (m.id.clone(), m.name.clone(), m.query.clone(), m.tags.clone(), ms.overall(&m.id).to_owned())).collect())
}

/// The SLOs (their id and name) by the services they read.
fn slos() -> HashMap<String, Vec<Related>> {
    let Ok(ss) = super::api::slos_kept() else { return HashMap::new() };
    by_service(
        ss.list()
            .into_iter()
            .map(|s| (s.id.clone(), s.name.clone(), [s.good.clone(), s.total.clone(), s.query.clone()].into_iter().flatten().collect::<Vec<_>>().join(" "), s.tags.clone(), String::new()))
            .collect(),
    )
}

/// The open issues' count by service.
fn open_issues() -> HashMap<String, u32> {
    let mut out = HashMap::new();
    if let Ok(is) = super::api::issues() {
        for i in is.list() {
            if matches!(i.status, super::errors::Status::ForReview | super::errors::Status::Reviewed) {
                *out.entry(i.service.clone()).or_default() += 1;
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn facts_of(
    name: &str,
    now: i64,
    red: Option<&(f64, f64, Option<f64>)>,
    traced: bool,
    logs_traced: u64,
    monitors: &HashMap<String, Vec<Related>>,
    slos: &HashMap<String, Vec<Related>>,
    issues: &HashMap<String, u32>,
) -> Facts {
    let (requests, errors, p95_s) = red.copied().unwrap_or((0.0, 0.0, None));
    let (last_deployment, deployments_30d, failed_30d) = super::dora::get().map(|d| d.read(|l| deployments_of(l, name, now))).unwrap_or((None, 0, 0));
    Facts {
        requests,
        errors,
        p95_s,
        traced: traced || requests > 0.0,
        open_issues: issues.get(name).copied().unwrap_or(0),
        monitors: monitors.get(name).cloned().unwrap_or_default(),
        slos: slos.get(name).cloned().unwrap_or_default(),
        last_deployment,
        deployments_30d,
        failed_30d,
        logs_traced,
    }
}

/// `name`'s last deployment (ms), and its deployments of the last 30 days and how many failed.
pub fn deployments_of(l: &super::dora::Ledger, name: &str, now: i64) -> (Option<i64>, u64, u64) {
    let q = super::dora::Query {
        from: (now - 30 * DAY_MS).saturating_mul(1_000_000),
        to: now.saturating_mul(1_000_000).saturating_add(1),
        service: Some(name.to_owned()),
        ..super::dora::Query::default()
    };
    let s = l.summary(&q);
    (l.last_of(name).map(|ns| ns / 1_000_000), s.deployments, s.change_failures)
}

/// Each service's requests, errors and 95th percentile over `from..to` (ms), from the trace
/// metrics.
async fn red_by_service(from: i64, to: i64) -> Option<HashMap<String, (f64, f64, Option<f64>)>> {
    super::api::apart(move |store| {
        let rows = kiln_obs::apm::red(kiln_obs::query::eval::Source::Shared(store), false, &[], "service", from, to).map_err(|e| super::api::fail(::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(rows.into_iter().filter(|r| r.hits > 0.0).map(|r| (r.group.clone(), (r.hits, r.errors, r.p95))).collect())
    })
    .await
    .ok()
}

/// One service's requests, errors and 95th percentile over `from..to` (ms); none of none.
async fn red_of(name: &str, from: i64, to: i64) -> Option<(f64, f64, Option<f64>)> {
    let name = name.to_owned();
    super::api::apart(move |store| {
        let rows = kiln_obs::apm::red(kiln_obs::query::eval::Source::Shared(store), false, &[("service", name.as_str())], "service", from, to)
            .map_err(|e| super::api::fail(::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(rows.into_iter().find(|r| r.hits > 0.0 && r.group == name).map(|r| (r.hits, r.errors, r.p95)))
    })
    .await
    .ok()
    .flatten()
}

/// How many of one service's logs over `from..to` (ms) carry a trace id.
async fn logs_traced_of(name: &str, from: i64, to: i64) -> Option<u64> {
    use kiln_obs::logs::store::Measure;
    let quoted = name.replace(['\\', '"'], "");
    let search = kiln_obs::logs::search::parse(&format!("service:\"{quoted}\" trace_id:*")).ok()?;
    super::api::read_apart(kiln_obs::store::Store::logs_at, move |at, _| {
        let agg =
            at.aggregate(&search, from.saturating_mul(1_000_000), to.saturating_mul(1_000_000), &[], &Measure::Count).map_err(|e| super::api::fail(::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(agg.groups.first().map_or(0, |(_, v)| v.max(0.0) as u64))
    })
    .await
    .ok()
}

/// The services traced over `from..to` (ms).
async fn traced_since(from: i64, to: i64) -> Option<BTreeSet<String>> {
    red_by_service(from, to).await.map(|m| m.into_keys().collect())
}

/// How many of each service's logs over `from..to` (ms) carry a trace id.
async fn logs_traced(from: i64, to: i64) -> Option<HashMap<String, u64>> {
    use kiln_obs::logs::store::{Facet, Measure};
    let search = kiln_obs::logs::search::parse("trace_id:*").ok()?;
    super::api::read_apart(kiln_obs::store::Store::logs_at, move |at, _| {
        let agg = at
            .aggregate(&search, from.saturating_mul(1_000_000), to.saturating_mul(1_000_000), &[Facet::parse("service")], &Measure::Count)
            .map_err(|e| super::api::fail(::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(agg.groups.into_iter().filter_map(|(vals, v)| Some((vals.into_iter().next()?, v.max(0.0) as u64))).collect())
    })
    .await
    .ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn def(yaml: &str) -> Definition {
        definition::parse(yaml).unwrap().remove(0)
    }

    /// `service:<name>` found as a whole value, in a query's braces or a tag; a longer name or
    /// another tag's key not mistaken for it.
    #[test]
    fn a_query_names_its_service_whole() {
        for (text, yes) in [
            ("avg(last_5m):avg:app.latency{service:shop} > 1", true),
            ("sum:hits{env:prod,service:shop}.as_count()", true),
            ("service:shop status:error", true),
            ("logs(\"service:shop\").index(\"*\").rollup(\"count\") > 10", true),
            ("avg:x{service:shop-api}", false),
            ("avg:x{service:shopping}", false),
            ("avg:x{my.service:shop}", false),
            ("avg:x{@service:shop}", false),
        ] {
            assert_eq!(names_service(text, "shop"), yes, "{text}");
        }
    }

    /// Every service a query names, once a monitor's or SLO's text is read: each its own whole value.
    #[test]
    fn a_querys_services_are_read_once() {
        let named: Vec<&str> = services_named("sum:x{service:a,env:p} by {host} / avg:y{service:b-c} + logs(\"service:d status:error\")").collect();
        assert_eq!(named, ["a", "b-c", "d"]);
        assert_eq!(services_named("avg:x{my.service:a}").count(), 0);
        let idx = by_service(vec![
            ("m1".into(), "one".into(), "avg:x{service:a}".into(), vec!["service:b".into(), "team:t".into()], "OK".into()),
            ("m2".into(), "two".into(), "avg:x{service:a} by {service}".into(), vec![], "Alert".into()),
        ]);
        assert_eq!(idx.get("a").map(Vec::len), Some(2));
        assert_eq!(idx.get("b").map(|v| v.iter().map(|r| r.id.as_str()).collect::<Vec<_>>()), Some(vec!["m1"]));
    }

    /// Each scorecard rule, passed and failed, as its facts and definition say.
    #[test]
    fn the_scorecards_rules_follow_the_facts() {
        let now = 1_790_000_000_000;
        let full = Kept {
            definition: def(
                "schema-version: v2.2\ndd-service: shop\nteam: checkout\ncontacts: [{type: email, contact: a@b.co}]\nlinks: [{name: run, type: runbook, url: https://w/r}]\nintegrations: {pagerduty: {service-url: https://acme.pagerduty.com/service-directory/PX1}}\n",
            ),
            source: Source::Repo { repo: "acme/shop".into(), file: "service.datadog.yaml".into(), sha: "a".repeat(40) },
            modified: now,
        };
        let related = |n: usize| (0..n).map(|i| Related { id: format!("m{i}"), name: format!("m{i}"), state: "OK".into() }).collect::<Vec<_>>();
        let good = Facts { monitors: related(2), slos: related(1), last_deployment: Some(now - DAY_MS), deployments_30d: 3, logs_traced: 9, ..Facts::default() };
        let passed: Vec<&str> = rules(Some(&full), &good, now).into_iter().filter(|r| r.passed).map(|r| r.id).collect();
        assert_eq!(passed, ["slos", "monitors", "on_call", "deployed", "deployment_tracking", "logs_correlation", "team", "contacts", "repos", "docs"]);
        // Only seen, nothing known: every rule failed, each saying why.
        let bare = rules(None, &Facts::default(), now);
        assert!(bare.iter().all(|r| !r.passed && !r.why.is_empty()), "{bare:?}");
        // A deployment 4 months ago: deployed in 3 months, no.
        let old = Facts { last_deployment: Some(now - 120 * DAY_MS), ..Facts::default() };
        assert!(!rules(None, &old, now).iter().any(|r| r.id == "deployed" && r.passed));
    }

    /// Made through the API, kept across an opening; a repository's definition of the same service
    /// stands before it and refuses its change.
    #[test]
    fn definitions_are_kept_and_a_repositorys_stands() {
        let dir = tempfile::tempdir().unwrap();
        let c = Catalog::open(dir.path(), None);
        let kept = c.put(def("schema-version: v2.2\ndd-service: shop\nteam: checkout\n")).unwrap();
        assert_eq!(kept.source, Source::Api);
        assert!(c.put(def("schema-version: v2.2\ndd-service: shop\n")).is_ok());
        let again = Catalog::open(dir.path(), None);
        assert_eq!(again.definition("shop").map(|k| k.definition.team), Some(None), "the change kept");
        again.repos.lock().insert(
            "shop".into(),
            Kept {
                definition: def("schema-version: v2.2\ndd-service: shop\nteam: repo-team\n"),
                source: Source::Repo { repo: "acme/shop".into(), file: FILES[0].into(), sha: String::new() },
                modified: 1,
            },
        );
        assert_eq!(again.definition("shop").and_then(|k| k.definition.team), Some("repo-team".into()));
        assert!(matches!(again.put(def("schema-version: v2.2\ndd-service: shop\n")), Err(PutError::InRepo(r)) if r == "acme/shop"));
        assert!(matches!(again.remove("shop"), Err(PutError::InRepo(_))));
        assert!(again.remove("nope").is_ok_and(|had| !had));
    }
}
