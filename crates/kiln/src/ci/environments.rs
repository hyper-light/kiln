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

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use http::{Method, StatusCode};
use kiln_ci::env::{Protection, Review};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension as _, params};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Fail;
use crate::authz::{Caller, Scope};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// An environment, as runs use it.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvDef {
    pub name: String,
    pub protection: Protection,
    pub url: Option<String>,
    /// `file` (trunk's `.kiln/environments.yaml`), `kiln` (made in kiln), `run` (named by a job).
    pub source: String,
    /// Agents may approve its deployments.
    pub agents_approve: bool,
}

/// One environment's CI keys in `.kiln/environments.yaml`; the rest are CD's.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct FileEnv {
    #[serde(flatten)]
    protection: Protection,
    url: Option<String>,
    agents: FileAgents,
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
                    let protection = if f.protection.check().is_ok() { f.protection } else { Protection::default() };
                    out.insert(env.to_ascii_lowercase(), EnvDef { name: env, protection, url: f.url, source: "file".into(), agents_approve: f.agents.approve });
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
    let rows: Vec<(String, String, Option<String>, String)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT name, protection, url, source FROM ci_environments WHERE repo_id = ?1 ORDER BY name")?;
            let rows = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    for (name, protection, url, source) in rows {
        let protection: Protection = serde_json::from_str(&protection).unwrap_or_default();
        out.entry(name.to_ascii_lowercase()).or_insert(EnvDef { name, protection, url, source, agents_approve: false });
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
    Ok(EnvDef { name: name.to_owned(), protection: Protection::default(), url: None, source: "run".into(), agents_approve: false })
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
    json!({ "name": d.name, "source": d.source, "protection": d.protection, "url": d.url, "agents_can_approve": d.agents_approve, "latest_deployment": latest })
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
}

/// `/api/ci/environments[/:name[/deployments]]?repo=`.
pub async fn route(hub: &Hub, method: &Method, tail: &[&str], q: &HashMap<String, String>, body: Value, caller: Option<&Caller>) -> Result<Response, Fail> {
    let repo = repo_of(hub, q).await?;
    let by = caller.map_or_else(|| "admin".to_owned(), |c| c.name.clone());
    match (method, tail) {
        (&Method::GET, []) => {
            let all = defs(hub, &repo).await.map_err(internal)?;
            let mut last = latest(hub, &repo).await.map_err(internal)?;
            let mut list: Vec<Value> = all.iter().map(|(k, d)| env_json(d, last.remove(k))).collect();
            // The repository's environments on its other CI providers (docs/ci.md W23-R01, R05).
            list.extend(super::providers::environments(hub, &repo).await);
            Ok(srv::json(StatusCode::OK, &json!({ "repo": repo.name, "environments": list })))
        }
        (&Method::GET, [name]) => {
            let d = defs(hub, &repo).await.map_err(internal)?.remove(&name.to_ascii_lowercase()).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no environment `{name}`")))?;
            let last = latest(hub, &repo).await.map_err(internal)?.remove(&name.to_ascii_lowercase());
            Ok(srv::json(StatusCode::OK, &env_json(&d, last)))
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
            let (id, n, p, u, who) = (repo.id.clone(), name.clone(), serde_json::to_string(&protection).unwrap_or_default(), b.url.clone(), by.clone());
            hub.store
                .call(move |c| {
                    c.execute(
                        "INSERT INTO ci_environments (repo_id, name, protection, url, source, created_at, created_by) VALUES (?1, ?2, ?3, ?4, 'kiln', ?5, ?6)
                         ON CONFLICT(repo_id, name) DO UPDATE SET protection = ?3, url = ?4, source = 'kiln'",
                        params![id, n, p, u, crate::util::now_unix(), who],
                    )?;
                    Ok(())
                })
                .await
                .map_err(internal)?;
            hub.store.audit(by, "ci.environments.save".into(), format!("{} {name}", repo.name)).await;
            let d = EnvDef { name, protection, url: b.url, source: "kiln".into(), agents_approve: false };
            Ok(srv::json(if *method == Method::POST { StatusCode::CREATED } else { StatusCode::OK }, &env_json(&d, None)))
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

/// Whether `caller` may review deployments to `env` of a run by `actor`.
fn may_review(caller: &Caller, env: &EnvDef, actor: &str) -> Result<(), String> {
    if caller.is_owner() {
        return Ok(());
    }
    let user = caller.name.split(" for ").last().unwrap_or(&caller.name).split(" of ").last().unwrap_or(&caller.name);
    let user = user.strip_prefix("user:").unwrap_or(user);
    if caller.kind == crate::authz::gate::Kind::Agent && !env.agents_approve {
        return Err(format!("agents don't approve deployments to {} (its `agents: {{ approve: true }}` allows it)", env.name));
    }
    if !env.protection.reviewers.is_empty() && !env.protection.reviewers.iter().any(|r| r.strip_prefix("user:").unwrap_or(r).eq_ignore_ascii_case(user)) {
        return Err(format!("{user} is not a required reviewer of {}", env.name));
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
    for e in &named {
        let d = all.get(&e.to_ascii_lowercase()).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no environment `{e}`")))?;
        if !who.is_owner() && !who.can("runs.approve", &Scope::Environment { repo: repo.name.clone(), environment: d.name.clone() }) {
            return Err((StatusCode::FORBIDDEN, format!("{} may not review deployments to {}: that needs runs.approve there", who.name, d.name)));
        }
        may_review(who, d, &actor).map_err(|m| (StatusCode::FORBIDDEN, m))?;
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
            source: "file".into(),
            agents_approve: agents,
        }
    }

    #[test]
    fn w15_r03_who_may_review() {
        let none = || Some(crate::authz::Rights::default());
        let alice = Caller::with(crate::authz::gate::Kind::User, "user:alice", none(), None);
        assert!(may_review(&alice, &env(&["alice"], false, false), "bob").is_ok());
        assert!(may_review(&alice, &env(&["user:ALICE"], false, false), "bob").is_ok());
        assert!(may_review(&alice, &env(&["carol"], false, false), "bob").is_err(), "not a listed reviewer");
        assert!(may_review(&alice, &env(&["alice"], true, false), "alice").is_err(), "self-review prevented");
        assert!(may_review(&alice, &env(&[], false, false), "alice").is_ok(), "no reviewers listed: anyone with runs.approve");
        let agent = Caller::with(crate::authz::gate::Kind::Agent, "agent:fixer for user:alice", none(), none());
        assert!(may_review(&agent, &env(&["alice"], false, false), "bob").is_err(), "agents only where allowed");
        assert!(may_review(&agent, &env(&["alice"], false, true), "bob").is_ok());
        assert!(may_review(&Caller::owner(), &env(&["carol"], true, false), "admin").is_ok(), "the owner always may");
    }

    #[test]
    fn w15_r01_the_file_s_ci_keys() -> Result<(), String> {
        let m: BTreeMap<String, FileEnv> = serde_yaml::from_str("production:\n  branches: [main]\n  clusters: [in-cluster]\n  reviewers: [alice]\n  wait-timer: 5\n  url: https://example.com\n  agents: { approve: true, deploy: false }\nstaging: {}\n").map_err(|e| e.to_string())?;
        let p = m.get("production").ok_or("production")?;
        assert_eq!(p.protection.branches, Some(vec!["main".to_owned()]));
        assert_eq!((p.protection.reviewers.clone(), p.protection.wait_timer, p.url.clone(), p.agents.approve), (vec!["alice".to_owned()], 5, Some("https://example.com".to_owned()), true));
        assert!(m.get("staging").is_some_and(|s| s.protection.is_empty()));
        Ok(())
    }
}
