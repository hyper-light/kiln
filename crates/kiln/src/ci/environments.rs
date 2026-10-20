//! Environments, CI's half (docs/ci.md W15-R01, R03, R04, R08): what each repository's
//! environments are, who reviews a deployment, and the deployments jobs make.
//!
//! - **Definitions** come from trunk's `.kiln/environments.yaml`, whose CI keys are the
//!   protection rules ([`kiln_ci::env::Protection`]: `reviewers`, `prevent-self-review`,
//!   `wait-timer`, `branches`, `tags`) and `url` (CD reads the same file for its keys), and from
//!   kiln itself (`/api/ci/environments`, `kiln env`). A job naming an environment neither has
//!   makes it, with no rules, as GitHub does.
//! - **Reviews** (`POST /api/ci/runs/:id/approve`) count when their author is a listed
//!   reviewer (the owner always is; an agent only where `agents: { approve: true }`), and not
//!   the run's actor under `prevent-self-review`.
//! - **Deployments**: a job bound to an environment (unless `deployment: false`) records one when
//!   it starts and its status when it ends, with the job's evaluated `environment.url`; the
//!   facade's `/deployments` serves them, and actions can make their own there.
//! - **Kinds** (W15-R02): what an environment deploys to, and where ([`Targets`]): a `kubernetes`
//!   one's clusters and namespace (kiln CD deploys there); the runners a `host`, `container` or
//!   `microvm` one deploys on, which its jobs then run on ([`routing`]); a `platform` one's
//!   integration, whose credentials its jobs get.
//! - **The starter set** (W15-R08): `dev`, `staging` and `production` for a repository, in one
//!   step; and **the board** (W15-R09): every environment the caller may read, with its latest
//!   deployment and what waits to deploy there.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use http::{Method, StatusCode};
use kiln_ci::env::{Protection, Review};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Fail;
use crate::authz::{Caller, Scope};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// What an environment deploys to (W15-R02).
pub const KINDS: [&str; 5] = ["kubernetes", "host", "container", "microvm", "platform"];

/// Where an environment deploys (W15-R02): a `kubernetes` one's clusters (kiln CD's) and
/// namespace; the runners a `host`, `container` or `microvm` one deploys on, as a routing
/// constraint (`label deploy-web`, `os == linux && label web`); a `platform` one's integration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Targets {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub clusters: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runners: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration: Option<String>,
}

impl Targets {
    /// Only what `kind` deploys to.
    fn of(self, kind: Option<&str>) -> Targets {
        match kind {
            Some("kubernetes") => Targets { clusters: self.clusters, namespace: self.namespace.filter(|n| !n.trim().is_empty()), ..Targets::default() },
            Some("host" | "container" | "microvm") => Targets { runners: self.runners.map(|r| r.trim().to_owned()), ..Targets::default() },
            Some("platform") => Targets { integration: self.integration.map(|i| i.trim().to_owned()), ..Targets::default() },
            _ => Targets::default(),
        }
    }
}

/// That `kind` is one of [`KINDS`] and has where it deploys.
pub fn check_kind(kind: Option<&str>, t: &Targets) -> Result<(), String> {
    let Some(kind) = kind else { return Ok(()) };
    match kind {
        "kubernetes" if t.clusters.iter().all(|c| c.trim().is_empty()) => Err("a kubernetes environment deploys to one or more of kiln CD's clusters".into()),
        "host" | "container" | "microvm" => match t.runners.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            None => Err(format!("a {kind} environment deploys on runners: say which, as a constraint they meet (`label deploy-web`, `os == linux`)")),
            Some(r) => kiln_ci::sched::routing::Constraint::parse(r).map(|_| ()).map_err(|e| format!("its runners ({r}): {e}")),
        },
        "platform" if t.integration.as_deref().is_none_or(|i| i.trim().is_empty()) => Err("a platform environment deploys through one of kiln's integrations: say which".into()),
        "kubernetes" | "platform" => Ok(()),
        other => Err(format!("`{other}` isn't a kind of environment: kubernetes, host, container, microvm or platform")),
    }
}

/// What a leg deploying to `def` asks of its runner (W15-R02): a `host` environment's jobs run on
/// its runners, a `container` one's on those of them with Docker, a `microvm` one's on those that
/// run microVMs. Nothing for the other kinds, whose deploying is CD's or the platform's.
pub fn routing(def: &EnvDef) -> Vec<String> {
    let Some(r) = def.targets.runners.as_deref().map(str::trim).filter(|r| !r.is_empty()) else { return Vec::new() };
    match def.kind.as_deref() {
        Some("host") => vec![r.to_owned()],
        Some("container") => vec![r.to_owned(), "executors has docker".into()],
        Some("microvm") => vec![r.to_owned(), "executors has firecracker || executors has shards".into()],
        _ => Vec::new(),
    }
}

/// An environment, as runs use it.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvDef {
    pub name: String,
    pub protection: Protection,
    pub url: Option<String>,
    /// One of [`KINDS`]; `None` for one with no targets of its own (as GitHub's).
    pub kind: Option<String>,
    pub targets: Targets,
    /// `file` (trunk's `.kiln/environments.yaml`), `kiln` (made in kiln), `run` (named by a job).
    pub source: String,
    /// Agents may approve its deployments.
    pub agents_approve: bool,
    /// At most this many of its deployments at once (W15-R01); the rest wait their turn.
    pub concurrency: Option<u32>,
}

/// One environment's CI keys in `.kiln/environments.yaml`; the rest are CD's (`clusters` is
/// both's).
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct FileEnv {
    #[serde(flatten)]
    protection: Protection,
    url: Option<String>,
    agents: FileAgents,
    concurrency: Option<u32>,
    /// W15-R02; without it, what its targets say (`clusters`: kubernetes, `integration`:
    /// platform, `runners`: host).
    kind: Option<String>,
    clusters: Vec<String>,
    namespace: Option<String>,
    runners: Option<String>,
    integration: Option<String>,
}

impl FileEnv {
    /// Its kind and targets, or none when they can't be used (said in the log).
    fn kind(&self, repo: &str, env: &str) -> (Option<String>, Targets) {
        let kind = self.kind.clone().or_else(|| {
            let k = if !self.clusters.is_empty() {
                "kubernetes"
            } else if self.integration.is_some() {
                "platform"
            } else if self.runners.is_some() {
                "host"
            } else {
                return None;
            };
            Some(k.to_owned())
        });
        let targets = Targets { clusters: self.clusters.clone(), namespace: self.namespace.clone(), runners: self.runners.clone(), integration: self.integration.clone() }.of(kind.as_deref());
        match check_kind(kind.as_deref(), &targets) {
            Ok(()) => (kind, targets),
            Err(e) => {
                tracing::warn!(repo, environment = env, error = e, "an environment's kind can't be used; it has none");
                (None, Targets::default())
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileAgents {
    approve: bool,
}

const FRESH: Duration = Duration::from_secs(20);
type Cached = HashMap<String, (Instant, BTreeMap<String, EnvDef>)>;
static FILES: Mutex<Option<Cached>> = Mutex::new(None);

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

/// Trunk's environments (cached for a few seconds); none when there is no file or it can't be
/// read (which is logged).
async fn from_file(hub: &Hub, repo: &Repo) -> BTreeMap<String, EnvDef> {
    if let Some((at, m)) = FILES.lock().get_or_insert_with(HashMap::new).get(&repo.id)
        && at.elapsed() < FRESH
    {
        return m.clone();
    }
    let mut out = BTreeMap::new();
    if let Some((name, text)) = super::repo_config::trunk_file(hub, repo, &[".kiln/environments.yaml", ".kiln/environments.yml"]).await {
        match serde_yaml::from_slice::<BTreeMap<String, FileEnv>>(&text) {
            Ok(m) => {
                for (env, f) in m {
                    if let Err(e) = f.protection.check() {
                        tracing::warn!(repo = %repo.name, file = name, environment = env, error = e, "an environment's protection can't be used; it has none");
                    }
                    let (kind, targets) = f.kind(&repo.name, &env);
                    let protection = if f.protection.check().is_ok() { f.protection } else { Protection::default() };
                    out.insert(env.to_ascii_lowercase(), EnvDef { name: env, protection, url: f.url, kind, targets, source: "file".into(), agents_approve: f.agents.approve, concurrency: f.concurrency.filter(|n| *n > 0) });
                }
            }
            Err(e) => tracing::warn!(repo = %repo.name, file = name, error = %e, "the repository's environments can't be read"),
        }
    }
    FILES.lock().get_or_insert_with(HashMap::new).insert(repo.id.clone(), (Instant::now(), out.clone()));
    out
}

/// Every environment of a repository, by lower-cased name: trunk's file, then kiln's own.
pub async fn defs(hub: &Hub, repo: &Repo) -> anyhow::Result<BTreeMap<String, EnvDef>> {
    let mut out = from_file(hub, repo).await;
    let id = repo.id.clone();
    type Row = (String, String, Option<String>, String, Option<i64>, Option<String>, String);
    let rows: Vec<Row> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT name, protection, url, source, concurrency, kind, targets FROM ci_environments WHERE repo_id = ?1 ORDER BY name")?;
            let rows = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    for (name, protection, url, source, concurrency, kind, targets) in rows {
        let protection: Protection = serde_json::from_str(&protection).unwrap_or_default();
        let concurrency = concurrency.and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
        let targets: Targets = serde_json::from_str(&targets).unwrap_or_default();
        out.entry(name.to_ascii_lowercase()).or_insert(EnvDef { name, protection, url, kind, targets, source, agents_approve: false, concurrency });
    }
    Ok(out)
}

/// The environment a job names, made (with no rules) when the repository has none of that name.
pub async fn ensure(hub: &Hub, repo: &Repo, name: &str) -> anyhow::Result<EnvDef> {
    if let Some(d) = defs(hub, repo).await?.remove(&name.to_ascii_lowercase()) {
        return Ok(d);
    }
    let (id, n) = (repo.id.clone(), name.to_owned());
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO ci_environments (repo_id, name, protection, source, created_at, created_by) VALUES (?1, ?2, '{}', 'run', ?3, 'github-actions')",
                params![id, n, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await?;
    Ok(EnvDef { name: name.to_owned(), protection: Protection::default(), url: None, kind: None, targets: Targets::default(), source: "run".into(), agents_approve: false, concurrency: None })
}

/// The reviews of a run's deployments to `env`.
pub async fn reviews(hub: &Hub, run: &str, env: &str) -> anyhow::Result<Vec<Review>> {
    let (r, e) = (run.to_owned(), env.to_owned());
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT principal, approved, comment, created_at FROM ci_env_reviews WHERE run_id = ?1 AND environment = ?2 COLLATE NOCASE ORDER BY id")?;
            let rows = st.query_map(params![r, e], |x| Ok(Review { by: x.get(0)?, approved: x.get(1)?, comment: x.get(2)?, at: x.get(3)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

// ------------------------------------------------------------------ deployments

/// A deployment begins: its record, `in_progress`. `None` when it can't be written (logged).
#[allow(clippy::too_many_arguments)]
pub async fn started(hub: &Hub, repo: &Repo, env: &str, run: &str, leg: &str, sha: &str, git_ref: &str, creator: &str) -> Option<i64> {
    let (id, e, r, l, s, g, by) = (repo.id.clone(), env.to_owned(), run.to_owned(), leg.to_owned(), sha.to_owned(), git_ref.to_owned(), creator.to_owned());
    let production = env.eq_ignore_ascii_case("production") || env.eq_ignore_ascii_case("prod");
    let r = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute(
                "INSERT INTO ci_deployments (repo_id, environment, sha, git_ref, task, payload, creator, run_id, leg_id, production, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'deploy', '{}', ?5, ?6, ?7, ?8, ?9, ?9)",
                params![id, e, s, g, by, r, l, production, now],
            )?;
            let dep = c.last_insert_rowid();
            c.execute("INSERT INTO ci_deployment_statuses (deployment_id, state, creator, created_at) VALUES (?1, 'in_progress', 'github-actions[bot]', ?2)", params![dep, now])?;
            Ok(dep)
        })
        .await;
    r.map_err(|e| tracing::warn!(error = %format!("{e:#}"), "a deployment could not be recorded")).ok()
}

/// A deployment's new status (`success`, `failure`, `error`, `inactive`, …), with its URL.
pub async fn status(hub: &Hub, deployment: i64, state: &str, url: Option<String>, description: Option<String>, creator: &str) -> anyhow::Result<i64> {
    let (st, by) = (state.to_owned(), creator.to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute(
                "INSERT INTO ci_deployment_statuses (deployment_id, state, description, environment_url, creator, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![deployment, st, description, url, by, now],
            )?;
            c.execute("UPDATE ci_deployments SET updated_at = ?2 WHERE id = ?1", params![deployment, now])?;
            Ok(c.last_insert_rowid())
        })
        .await
}

// ------------------------------------------------------------------ API

/// The repository a request names (`?repo=owner/name` or its id).
async fn repo_of(hub: &Hub, q: &HashMap<String, String>) -> Result<Repo, Fail> {
    let key = q.get("repo").filter(|r| !r.is_empty()).ok_or_else(|| bad("say which repository: ?repo=owner/name"))?;
    hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.id == *key || r.name.eq_ignore_ascii_case(key)).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{key}`")))
}

fn env_json(d: &EnvDef, latest: Option<Value>) -> Value {
    json!({ "name": d.name, "source": d.source, "kind": d.kind, "targets": d.targets, "protection": d.protection, "url": d.url, "agents_can_approve": d.agents_approve, "concurrency": d.concurrency, "latest_deployment": latest })
}

/// The latest deployment of each environment of a repository, by lower-cased name.
async fn latest(hub: &Hub, repo: &Repo) -> anyhow::Result<HashMap<String, Value>> {
    let id = repo.id.clone();
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT d.environment, d.id, d.sha, d.git_ref, d.creator, d.run_id, d.created_at,
                        (SELECT s.state FROM ci_deployment_statuses s WHERE s.deployment_id = d.id ORDER BY s.id DESC LIMIT 1),
                        (SELECT s.environment_url FROM ci_deployment_statuses s WHERE s.deployment_id = d.id AND s.environment_url IS NOT NULL ORDER BY s.id DESC LIMIT 1)
                 FROM ci_deployments d WHERE d.repo_id = ?1 AND d.id = (SELECT MAX(x.id) FROM ci_deployments x WHERE x.repo_id = d.repo_id AND x.environment = d.environment COLLATE NOCASE)",
            )?;
            let rows = st.query_map([&id], |r| {
                Ok((
                    r.get::<_, String>(0)?.to_ascii_lowercase(),
                    json!({ "id": r.get::<_, i64>(1)?, "sha": r.get::<_, String>(2)?, "ref": r.get::<_, String>(3)?, "creator": r.get::<_, String>(4)?, "run": r.get::<_, Option<String>>(5)?,
                            "created_at": r.get::<_, i64>(6)?, "state": r.get::<_, Option<String>>(7)?, "url": r.get::<_, Option<String>>(8)? }),
                ))
            })?;
            Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
        })
        .await
}

#[derive(Deserialize)]
struct EnvBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    protection: Option<Protection>,
    #[serde(default)]
    url: Option<String>,
    /// At most this many deployments at once.
    #[serde(default)]
    concurrency: Option<u32>,
    /// One of [`KINDS`], and where it deploys.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    targets: Option<Targets>,
}

/// Makes `d` in kiln (or changes the one made here): what saving an environment does.
async fn save(hub: &Hub, repo: &Repo, d: &EnvDef, by: &str) -> anyhow::Result<()> {
    let (id, n, p, u, who, limit, kind, targets) =
        (repo.id.clone(), d.name.clone(), serde_json::to_string(&d.protection).unwrap_or_default(), d.url.clone(), by.to_owned(), d.concurrency, d.kind.clone(), serde_json::to_string(&d.targets).unwrap_or_default());
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO ci_environments (repo_id, name, protection, url, source, created_at, created_by, concurrency, kind, targets) VALUES (?1, ?2, ?3, ?4, 'kiln', ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(repo_id, name) DO UPDATE SET protection = ?3, url = ?4, source = 'kiln', concurrency = ?7, kind = ?8, targets = ?9",
                params![id, n, p, u, crate::util::now_unix(), who, limit, kind, targets],
            )?;
            Ok(())
        })
        .await?;
    hub.store.audit(by.to_owned(), "ci.environments.save".into(), format!("{} {}", repo.name, d.name)).await;
    Ok(())
}

/// A repository's starter set (W15-R08): `dev`, `staging` and `production`, each made unless the
/// repository has one of its name. Staging and production deploy only from the trunk, production
/// once its reviewer approves: whoever made the set (the owner, made by the server's own token).
async fn starter(hub: &Hub, repo: &Repo, caller: Option<&Caller>, by: &str) -> Result<Response, Fail> {
    let all = defs(hub, repo).await.map_err(internal)?;
    let trunk = super::events::facts(hub, repo).await.default_branch;
    let reviewer = caller.map(person_of).filter(|p| !p.is_empty() && *p != "admin").map_or_else(|| "owner".to_owned(), str::to_owned);
    let set = [
        ("dev", Protection::default()),
        ("staging", Protection { branches: Some(vec![trunk.clone()]), ..Protection::default() }),
        ("production", Protection { branches: Some(vec![trunk]), reviewers: vec![reviewer], ..Protection::default() }),
    ];
    let (mut made, mut kept) = (Vec::new(), Vec::new());
    for (name, protection) in set {
        if let Some(d) = all.get(name) {
            kept.push(d.name.clone());
            continue;
        }
        let d = EnvDef { name: name.to_owned(), protection, url: None, kind: None, targets: Targets::default(), source: "kiln".into(), agents_approve: false, concurrency: None };
        save(hub, repo, &d, by).await.map_err(internal)?;
        made.push(name.to_owned());
    }
    Ok(srv::json(StatusCode::OK, &json!({ "repo": repo.name, "made": made, "kept": kept })))
}

/// The legs waiting to deploy (of `repo`'s runs, or every run's), newest run first: (repository
/// id, the environment's lower-cased name, the leg, its run and why it waits).
async fn waiting_legs(hub: &Hub, repo: Option<&str>) -> anyhow::Result<Vec<(String, String, Value)>> {
    let repo = repo.map(str::to_owned);
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT r.repo_id, json_extract(j.wait_reason, '$.environment'), j.id, j.run_id, r.number, j.key, j.display, j.wait_reason, j.queued_at
                 FROM jobs j JOIN runs r ON r.id = j.run_id
                 WHERE j.status = 'queued' AND j.job_id IS NOT NULL AND json_extract(j.wait_reason, '$.environment') IS NOT NULL AND (?1 IS NULL OR r.repo_id = ?1)
                 ORDER BY r.created_at DESC, j.rowid",
            )?;
            let rows = st.query_map([repo], |x| {
                let why: Value = x.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
                Ok((
                    x.get::<_, String>(0)?,
                    x.get::<_, String>(1)?.to_ascii_lowercase(),
                    json!({ "leg": x.get::<_, String>(2)?, "run": x.get::<_, String>(3)?, "number": x.get::<_, i64>(4)?, "key": x.get::<_, String>(5)?, "name": x.get::<_, Option<String>>(6)?, "wait": why, "queued_at": x.get::<_, Option<i64>>(8)? }),
                ))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// The board (W15-R09): every environment of every repository `caller` may read runs of, each
/// with its latest deployment and the legs waiting to deploy there (for a review, a wait timer,
/// a window, a promotion, a gate or their turn), newest run first.
async fn board(hub: &Hub, caller: Option<&Caller>) -> Result<Response, Fail> {
    let may = match caller {
        Some(c) => crate::authz::gate::readable(hub, c, "runs.read").await,
        None => None,
    };
    let repos: Vec<Repo> = hub.store.repos().await.map_err(internal)?.into_iter().filter(|r| may.as_ref().is_none_or(|m| m.contains(&r.name))).collect();
    let waits = waiting_legs(hub, None).await.map_err(internal)?;
    let mut rows = Vec::new();
    for repo in &repos {
        let all = defs(hub, repo).await.map_err(internal)?;
        if all.is_empty() {
            continue;
        }
        let mut last = latest(hub, repo).await.map_err(internal)?;
        for (key, d) in &all {
            let mut v = env_json(d, last.remove(key));
            let waiting: Vec<&Value> = waits.iter().filter(|(r, e, _)| *r == repo.id && e == key).map(|(_, _, w)| w).collect();
            if let Some(o) = v.as_object_mut() {
                o.insert("repo".into(), json!(repo.name));
                o.insert("waiting".into(), json!(waiting));
            }
            rows.push(v);
        }
    }
    Ok(srv::json(StatusCode::OK, &json!({ "environments": rows })))
}

/// `/api/ci/environments[/:name[/deployments]]?repo=`.
pub async fn route(hub: &Hub, method: &Method, tail: &[&str], q: &HashMap<String, String>, body: Value, caller: Option<&Caller>) -> Result<Response, Fail> {
    if (method, tail) == (&Method::GET, &["board"][..]) {
        return board(hub, caller).await;
    }
    let repo = repo_of(hub, q).await?;
    let by = caller.map_or_else(|| "admin".to_owned(), |c| c.name.clone());
    match (method, tail) {
        (&Method::POST, ["starter"]) => starter(hub, &repo, caller, &by).await,
        (&Method::GET, []) => {
            let all = defs(hub, &repo).await.map_err(internal)?;
            let mut last = latest(hub, &repo).await.map_err(internal)?;
            let mut list: Vec<Value> = all.iter().map(|(k, d)| env_json(d, last.remove(k))).collect();
            // The repository's environments on its other CI providers (docs/ci.md W23-R01, R05).
            list.extend(super::providers::environments(hub, &repo).await);
            Ok(srv::json(StatusCode::OK, &json!({ "repo": repo.name, "environments": list })))
        }
        (&Method::GET, [name]) => {
            let key = name.to_ascii_lowercase();
            let d = defs(hub, &repo).await.map_err(internal)?.remove(&key).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no environment `{name}`")))?;
            let last = latest(hub, &repo).await.map_err(internal)?.remove(&key);
            let mut v = env_json(&d, last);
            // What waits to deploy there (W15-R08).
            let waits: Vec<Value> = waiting_legs(hub, Some(&repo.id)).await.map_err(internal)?.into_iter().filter(|(_, e, _)| *e == key).map(|(_, _, w)| w).collect();
            if let Some(o) = v.as_object_mut() {
                o.insert("repo".into(), json!(repo.name));
                o.insert("waiting".into(), json!(waits));
            }
            Ok(srv::json(StatusCode::OK, &v))
        }
        (&Method::GET, [name, "deployments"]) => deployments(hub, &repo, Some(name), q).await,
        (&Method::POST, []) | (&Method::PUT | &Method::PATCH, [_]) => {
            let b: EnvBody = serde_json::from_value(body).map_err(|e| bad(format!("{e}")))?;
            let name = match tail {
                [n] => (*n).to_owned(),
                _ => b.name.clone().filter(|n| !n.trim().is_empty()).ok_or_else(|| bad("an environment has a name"))?,
            };
            if name.len() > 255 || name.contains('/') || name.trim() != name {
                return Err(bad("an environment's name is up to 255 characters, without slashes or surrounding spaces"));
            }
            let all = defs(hub, &repo).await.map_err(internal)?;
            if all.get(&name.to_ascii_lowercase()).is_some_and(|d| d.source == "file") {
                return Err((StatusCode::CONFLICT, format!("`{name}` is defined in .kiln/environments.yaml: change it there")));
            }
            let protection = b.protection.unwrap_or_default();
            protection.check().map_err(bad)?;
            if b.concurrency == Some(0) {
                return Err(bad("a concurrency limit is 1 or more (leave it out for none)"));
            }
            let kind = b.kind.map(|k| k.trim().to_ascii_lowercase()).filter(|k| !k.is_empty());
            let targets = b.targets.unwrap_or_default().of(kind.as_deref());
            check_kind(kind.as_deref(), &targets).map_err(bad)?;
            let d = EnvDef { name, protection, url: b.url, kind, targets, source: "kiln".into(), agents_approve: false, concurrency: b.concurrency };
            save(hub, &repo, &d, &by).await.map_err(internal)?;
            let mut v = env_json(&d, None);
            let (diagnostics, preview) = effect(hub, &repo, &d).await;
            if let Some(o) = v.as_object_mut() {
                o.insert("diagnostics".into(), json!(diagnostics));
                o.insert("preview".into(), preview);
            }
            Ok(srv::json(if *method == Method::POST { StatusCode::CREATED } else { StatusCode::OK }, &v))
        }
        (&Method::DELETE, [name]) => {
            let (id, n) = (repo.id.clone(), (*name).to_owned());
            let gone = hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_environments WHERE repo_id = ?1 AND name = ?2 COLLATE NOCASE", params![id, n])?)).await.map_err(internal)?;
            if gone == 0 {
                return Err((StatusCode::NOT_FOUND, format!("no environment `{name}` made in kiln (one in .kiln/environments.yaml is removed there)")));
            }
            hub.store.audit(by, "ci.environments.delete".into(), format!("{} {name}", repo.name)).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        _ => Err((StatusCode::NOT_FOUND, "no such environments route".into())),
    }
}

/// What an environment's change does now (docs/ci.md W20-R03): warnings for a reviewer who isn't
/// anyone here, for one reviewer who can't approve their own runs, and for a trunk its branches
/// rule leaves out; and what a deployment from the trunk would meet now.
async fn effect(hub: &Hub, repo: &Repo, d: &EnvDef) -> (Vec<Value>, Value) {
    let p = &d.protection;
    let mut diagnostics = Vec::new();
    let people: Vec<String> = hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT name FROM users WHERE disabled_at IS NULL")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap_or_default();
    for r in p.reviewers.iter().filter(|r| !r.contains(':') && *r != "admin" && *r != "owner") {
        if !people.iter().any(|n| n.eq_ignore_ascii_case(r)) {
            diagnostics.push(json!({ "level": "warning", "message": format!("reviewer {r} isn't anyone here: they can't approve") }));
        }
    }
    if p.prevent_self_review && p.reviewers.len() == 1 && !p.reviewers.iter().any(|r| r.contains(':')) {
        diagnostics.push(json!({ "level": "warning", "message": format!("{} is the only reviewer and can't approve their own runs: their deployments to {} wait for no one", p.reviewers.join(""), d.name) }));
    }
    let trunk = super::events::facts(hub, repo).await.default_branch;
    let now = crate::util::now_unix();
    let meets = match kiln_ci::env::gate(&d.name, p, &format!("refs/heads/{trunk}"), now, now, &[]) {
        kiln_ci::env::Gate::Open => "starts at once".to_owned(),
        kiln_ci::env::Gate::Refused(why) => {
            diagnostics.push(json!({ "level": "warning", "message": format!("the trunk ({trunk}) may not deploy to {}: {why}", d.name) }));
            format!("refused: {why}")
        }
        kiln_ci::env::Gate::Rejected { by, .. } => format!("rejected by {by}"),
        kiln_ci::env::Gate::Waiting { reviewers, until, window, promotion, gate } => {
            let mut waits: Vec<String> = Vec::new();
            if !reviewers.is_empty() {
                waits.push(format!("a review by one of {}", reviewers.join(", ")));
            }
            if let Some(w) = window {
                waits.push(w);
            }
            if let Some(pr) = promotion {
                waits.push(pr);
            }
            if let Some(g) = gate {
                waits.push(g);
            }
            if waits.is_empty() && until.is_some() {
                waits.push(format!("its wait timer ({} minutes)", p.wait_timer));
            }
            format!("waits for {}", waits.join("; "))
        }
    };
    (diagnostics, json!({ "trunk": trunk, "a_deployment_from_the_trunk": meets }))
}

/// A repository's deployments, newest first: of one environment, or (`?environment=`) any.
pub async fn deployments(hub: &Hub, repo: &Repo, env: Option<&str>, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (id, e) = (repo.id.clone(), env.map(str::to_owned).or_else(|| q.get("environment").cloned()));
    let limit = q.get("limit").and_then(|l| l.parse::<i64>().ok()).unwrap_or(50).clamp(1, 500);
    let rows = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT d.id, d.environment, d.sha, d.git_ref, d.creator, d.run_id, d.leg_id, d.created_at,
                        (SELECT s.state FROM ci_deployment_statuses s WHERE s.deployment_id = d.id ORDER BY s.id DESC LIMIT 1),
                        (SELECT s.environment_url FROM ci_deployment_statuses s WHERE s.deployment_id = d.id AND s.environment_url IS NOT NULL ORDER BY s.id DESC LIMIT 1)
                 FROM ci_deployments d WHERE d.repo_id = ?1 AND (?2 IS NULL OR d.environment = ?2 COLLATE NOCASE) ORDER BY d.id DESC LIMIT ?3",
            )?;
            let rows = st.query_map(params![id, e, limit], |r| {
                Ok(json!({ "id": r.get::<_, i64>(0)?, "environment": r.get::<_, String>(1)?, "sha": r.get::<_, String>(2)?, "ref": r.get::<_, String>(3)?, "creator": r.get::<_, String>(4)?,
                           "run": r.get::<_, Option<String>>(5)?, "leg": r.get::<_, Option<String>>(6)?, "created_at": r.get::<_, i64>(7)?, "state": r.get::<_, Option<String>>(8)?, "url": r.get::<_, Option<String>>(9)? }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "deployments": rows })))
}

#[derive(Deserialize)]
struct ApproveBody {
    /// The environments decided (default: every one the run waits for).
    #[serde(default)]
    environments: Vec<String>,
    /// `approved` (default) or `rejected`.
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    comment: String,
}

/// The person a caller acts for: `alice` of `user:alice`, of `agent:x for user:alice`.
fn person_of(caller: &Caller) -> &str {
    let user = caller.name.split(" for ").last().unwrap_or(&caller.name).split(" of ").last().unwrap_or(&caller.name);
    user.strip_prefix("user:").unwrap_or(user)
}

/// The teams and groups a kiln user belongs to on the hosts they sign in with, as of their last
/// sign-in (W23-R08: GitHub's `org/team`s, GitLab's groups' paths, Bitbucket's workspaces).
async fn memberships(hub: &Hub, user: &str) -> Vec<String> {
    let user = user.to_owned();
    hub.store
        .call(move |c| {
            let mut q = c.prepare("SELECT i.groups FROM user_identities i JOIN users u ON u.id = i.user_id WHERE u.name = ?1 COLLATE NOCASE AND u.disabled_at IS NULL")?;
            let rows = q.query_map([&user], |r| r.get::<_, Option<String>>(0))?;
            let mut all = Vec::new();
            for g in rows {
                all.extend(g?.and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok()).unwrap_or_default());
            }
            Ok(all)
        })
        .await
        .unwrap_or_default()
}

/// Whether `caller` may review deployments to `env` of a run by `actor` in a repository of
/// `owner`'s. A listed reviewer is a person (`alice`) or a team (`team:acme/web`, or `team:web`
/// for the owner's), which `teams`, the caller's memberships, must hold.
fn may_review(caller: &Caller, env: &EnvDef, actor: &str, owner: &str, teams: &[String]) -> Result<(), String> {
    if caller.is_owner() {
        return Ok(());
    }
    let user = person_of(caller);
    if caller.kind == crate::authz::gate::Kind::Agent && !env.agents_approve {
        return Err(format!("agents don't approve deployments to {} (its `agents: {{ approve: true }}` allows it)", env.name));
    }
    let listed = |r: &String| match r.strip_prefix("team:") {
        Some(t) => teams.iter().any(|m| m.eq_ignore_ascii_case(t) || m.eq_ignore_ascii_case(&format!("{owner}/{t}"))),
        None => r.strip_prefix("user:").unwrap_or(r).eq_ignore_ascii_case(user),
    };
    if !env.protection.reviewers.is_empty() && !env.protection.reviewers.iter().any(listed) {
        return Err(format!("{user} is not a required reviewer of {} (nor in a team that is)", env.name));
    }
    if env.protection.prevent_self_review && user.eq_ignore_ascii_case(actor) {
        return Err(format!("{} prevents self-review: {user} started this run", env.name));
    }
    Ok(())
}

/// `POST /api/ci/runs/:id/approve` with `{ environments?, state?, comment? }`: a review of the
/// run's waiting deployments; the dispatcher lets them go, or fails them, at once.
pub async fn approve(hub: &Hub, run: &str, body: Value, caller: Option<&Caller>) -> Result<Response, Fail> {
    let b: ApproveBody = serde_json::from_value(body).map_err(|e| bad(format!("{e}")))?;
    let approved = match b.state.as_deref().unwrap_or("approved") {
        "approved" | "approve" => true,
        "rejected" | "reject" => false,
        s => return Err(bad(format!("`{s}`: a review is `approved` or `rejected`"))),
    };
    // Another provider's run (W23-R01): its provider takes the review, after kiln's own check.
    if let Some((repo, provider, ext)) = super::providers::of_run(hub, run).await.map_err(internal)? {
        let owner = Caller::owner();
        let who = caller.unwrap_or(&owner);
        if !who.is_owner() && !who.can("runs.approve", &Scope::Repo { repo: repo.name.clone() }) {
            return Err((StatusCode::FORBIDDEN, format!("{} may not approve runs of {}: that needs runs.approve there", who.name, repo.name)));
        }
        let v = super::providers::approve(hub, &repo, &provider, &ext, &b.environments, approved, &b.comment, caller).await?;
        hub.store.audit(who.name.clone(), if approved { "ci.runs.approve".into() } else { "ci.runs.reject".into() }, format!("{run} on {provider}: {}", b.environments.join(", "))).await;
        return Ok(srv::json(StatusCode::OK, &v));
    }
    let rid = run.to_owned();
    let row: Option<(String, String)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT repo_id, actor FROM runs WHERE id = ?1", [&rid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
        .await
        .map_err(internal)?;
    let Some((repo_id, actor)) = row else { return Err((StatusCode::NOT_FOUND, "no such run".into())) };
    let repo = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.id == repo_id).ok_or_else(|| (StatusCode::NOT_FOUND, "no such run".into()))?;
    // An outside contributor's run waiting to start (W18-R06): approved (or not) in one step.
    if b.environments.is_empty() && super::runs::approval(hub, run).await.map_err(internal)?.as_deref() == Some("required") {
        let owner = Caller::owner();
        let who = caller.unwrap_or(&owner);
        if !who.is_owner() && !who.can("runs.approve", &Scope::Repo { repo: repo.name.clone() }) {
            return Err((StatusCode::FORBIDDEN, format!("{} may not approve runs of {}: that needs runs.approve there", who.name, repo.name)));
        }
        let (tx, rx) = async_channel::bounded(1);
        hub.ci.send(super::dispatch::Msg::ApproveRun { run: run.to_owned(), by: who.name.clone(), approve: approved, reply: tx });
        let late = || (StatusCode::GATEWAY_TIMEOUT, "the dispatcher didn't answer".to_owned());
        let decided = crate::rt::timeout(Duration::from_secs(10), rx.recv()).await.map_err(|_| late())?.map_err(|_| late())?;
        if !decided {
            return Err((StatusCode::CONFLICT, "the run waits for no approval".into()));
        }
        hub.store.audit(who.name.clone(), if approved { "ci.runs.approve".into() } else { "ci.runs.reject".into() }, format!("{run} (an outside contributor's run, by {actor})")).await;
        return Ok(srv::json(StatusCode::OK, &json!({ "run": run, "approval": "run", "state": if approved { "approved" } else { "rejected" } })));
    }
    // The environments the run waits for, unless the body names them.
    let waiting = waiting_envs(hub, run).await.map_err(internal)?;
    let named: Vec<String> = if b.environments.is_empty() { waiting.clone() } else { b.environments.clone() };
    if named.is_empty() {
        return Err((StatusCode::CONFLICT, "the run waits for no deployment review".into()));
    }
    let all = defs(hub, &repo).await.map_err(internal)?;
    let owner = Caller::owner();
    let who = caller.unwrap_or(&owner);
    let teams = if who.is_owner() { Vec::new() } else { memberships(hub, person_of(who)).await };
    let repo_owner = repo.name.split_once('/').map_or(repo.name.as_str(), |(o, _)| o);
    for e in &named {
        let d = all.get(&e.to_ascii_lowercase()).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no environment `{e}`")))?;
        if !who.is_owner() && !who.can("runs.approve", &Scope::Environment { repo: repo.name.clone(), environment: d.name.clone() }) {
            return Err((StatusCode::FORBIDDEN, format!("{} may not review deployments to {}: that needs runs.approve there", who.name, d.name)));
        }
        may_review(who, d, &actor, repo_owner, &teams).map_err(|m| (StatusCode::FORBIDDEN, m))?;
    }
    let (rid, by, comment, envs) = (run.to_owned(), who.name.clone(), b.comment.clone(), named.clone());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            for e in &envs {
                c.execute("INSERT INTO ci_env_reviews (run_id, environment, approved, comment, principal, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![rid, e, approved, comment, by, now])?;
            }
            Ok(())
        })
        .await
        .map_err(internal)?;
    hub.store.audit(who.name.clone(), if approved { "ci.runs.approve".into() } else { "ci.runs.reject".into() }, format!("{run} {}", named.join(","))).await;
    hub.ci.send(super::dispatch::Msg::Reviewed { run: run.to_owned() });
    Ok(srv::json(StatusCode::OK, &json!({ "run": run, "environments": named, "state": if approved { "approved" } else { "rejected" } })))
}

/// The environments a KilnCI run waits on a review for.
pub(crate) async fn waiting(hub: &Hub, run: &str) -> anyhow::Result<Vec<String>> {
    waiting_envs(hub, run).await
}

/// The environments a run's legs wait on a review for (their wait reasons say so).
async fn waiting_envs(hub: &Hub, run: &str) -> anyhow::Result<Vec<String>> {
    let r = run.to_owned();
    let reasons: Vec<String> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT wait_reason FROM jobs WHERE run_id = ?1 AND status = 'queued' AND wait_reason IS NOT NULL")?;
            let rows = st.query_map([&r], |x| x.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    let mut envs: Vec<String> = reasons
        .iter()
        .filter_map(|t| serde_json::from_str::<Value>(t).ok())
        .filter(|v| v.get("reason").and_then(Value::as_str) == Some("approval") && v.get("reviewers").and_then(Value::as_array).is_some_and(|a| !a.is_empty()))
        .filter_map(|v| v.get("environment").and_then(Value::as_str).map(str::to_owned))
        .collect();
    envs.sort();
    envs.dedup();
    Ok(envs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(reviewers: &[&str], self_review: bool, agents: bool) -> EnvDef {
        EnvDef {
            name: "production".into(),
            protection: Protection { reviewers: reviewers.iter().map(|r| (*r).to_owned()).collect(), prevent_self_review: self_review, ..Protection::default() },
            url: None,
            kind: None,
            targets: Targets::default(),
            source: "file".into(),
            agents_approve: agents,
            concurrency: None,
        }
    }

    #[test]
    fn w15_r03_who_may_review() {
        let none = || Some(crate::authz::Rights::default());
        let alice = Caller::with(crate::authz::gate::Kind::User, "user:alice", none(), None);
        let may = |c: &Caller, e: &EnvDef, actor: &str| may_review(c, e, actor, "acme", &[]);
        assert!(may(&alice, &env(&["alice"], false, false), "bob").is_ok());
        assert!(may(&alice, &env(&["user:ALICE"], false, false), "bob").is_ok());
        assert!(may(&alice, &env(&["carol"], false, false), "bob").is_err(), "not a listed reviewer");
        assert!(may(&alice, &env(&["alice"], true, false), "alice").is_err(), "self-review prevented");
        assert!(may(&alice, &env(&[], false, false), "alice").is_ok(), "no reviewers listed: anyone with runs.approve");
        let agent = Caller::with(crate::authz::gate::Kind::Agent, "agent:fixer for user:alice", none(), none());
        assert!(may(&agent, &env(&["alice"], false, false), "bob").is_err(), "agents only where allowed");
        assert!(may(&agent, &env(&["alice"], false, true), "bob").is_ok());
        assert!(may(&Caller::owner(), &env(&["carol"], true, false), "admin").is_ok(), "the owner always may");
        // Teams: the host's `org/team`, or a team of the repository's owner.
        let web = vec!["acme".to_owned(), "acme/web".to_owned()];
        assert!(may_review(&alice, &env(&["team:acme/web"], false, false), "bob", "acme", &web).is_ok());
        assert!(may_review(&alice, &env(&["team:web"], false, false), "bob", "acme", &web).is_ok(), "the owner's team");
        assert!(may_review(&alice, &env(&["team:web"], false, false), "bob", "other", &web).is_err(), "another owner's team");
        assert!(may_review(&alice, &env(&["team:acme/ops"], false, false), "bob", "acme", &web).is_err(), "not in it");
        assert!(may_review(&alice, &env(&["team:acme/web"], true, false), "alice", "acme", &web).is_err(), "a team doesn't lift self-review");
    }

    #[test]
    fn w15_r01_the_file_s_ci_keys() -> Result<(), String> {
        let m: BTreeMap<String, FileEnv> = serde_yaml::from_str("production:\n  branches: [main]\n  clusters: [in-cluster]\n  reviewers: [alice]\n  wait-timer: 5\n  url: https://example.com\n  agents: { approve: true, deploy: false }\n  concurrency: 1\nstaging: {}\n").map_err(|e| e.to_string())?;
        let p = m.get("production").ok_or("production")?;
        assert_eq!(p.protection.branches, Some(vec!["main".to_owned()]));
        assert_eq!((p.protection.reviewers.clone(), p.protection.wait_timer, p.url.clone(), p.agents.approve), (vec!["alice".to_owned()], 5, Some("https://example.com".to_owned()), true));
        assert!(m.get("staging").is_some_and(|s| s.protection.is_empty() && s.concurrency.is_none()));
        assert_eq!(p.concurrency, Some(1));
        Ok(())
    }
}
