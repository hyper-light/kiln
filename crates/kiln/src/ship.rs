//! Ship's read model (plan 01a175b3a21174bc8769f5e784b90ecb): each commit of a repository and where
//! it stands, from the records kiln keeps for it (the plan's "Sources by commit"): the land queue;
//! its runs, their legs and each leg's attempts; the release pipeline's legs and the images built;
//! each environment's deployments; Deploy API releases of it; and, for the commit production runs,
//! each runner's replant. Where it stands is `kiln_cd::journey`'s to say: this reads the records
//! into a journey and shapes the JSON. Every field is new, and a client must take any as missing.
//!
//! - `GET /api/ship/commits?repo=&limit=&cursor=`: a repository's commits, newest first, cursor-paged.
//! - `GET /api/ship/commits/:repo/:sha`: one commit, its records and where it stands.

use std::collections::HashMap;

use http::StatusCode;
use kiln_cd::journey::{self, Attempt, Author, Build, Check, Journey, Land, Leg, Part, Platform, Production, Replant, Runner, RunnerReplant, Staging};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

pub type Fail = (StatusCode, String);

/// How long kiln's guard watches production after a deploy before it counts as held.
const GUARD_MS: i64 = 10 * 60 * 1000;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// A runner, as the updates know it: which commit it runs, and whether its upgrade to production's
/// commit failed (and why).
#[derive(Debug, Clone, Default)]
pub struct RunnerSeen {
    pub name: String,
    pub connected: bool,
    pub commit: Option<String>,
    pub failed: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct RunRec {
    id: String,
    pipeline: String,
    branch: String,
    message: String,
    actor: String,
    created_at: i64,
}

#[derive(Debug, Clone, Default)]
struct AttemptRec {
    status: String,
    conclusion: Option<String>,
    class: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct LegRec {
    run_id: String,
    name: String,
    status: String,
    conclusion: Option<String>,
    reused: bool,
    error: Option<String>,
    attempts: Vec<AttemptRec>,
}

/// One environment's deployment of a commit: its latest status and when it was set (unix seconds).
#[derive(Debug, Clone, Default)]
struct DeployRec {
    environment: String,
    production: bool,
    sha: String,
    first_row: i64,
    state: Option<String>,
    description: Option<String>,
    at: i64,
}

#[derive(Debug, Clone, Default)]
struct ReleaseRec {
    state: String,
    message: Option<String>,
    rolled_back: bool,
}

/// Everything kiln keeps for one commit, as read.
#[derive(Debug, Clone, Default)]
struct Records {
    repo: String,
    sha: String,
    first_row: i64,
    runs: Vec<RunRec>,
    legs: Vec<LegRec>,
    images: Vec<String>,
    land: Option<(String, Option<String>)>,
    own: Vec<DeployRec>,
    current: Vec<DeployRec>,
    releases: Vec<ReleaseRec>,
    /// A newer commit production waits on: its sha, and how far behind it this one is.
    waiting: Option<(String, i64)>,
    riders: u32,
    tags: Vec<String>,
}

fn short(sha: &str) -> String {
    sha.get(..7).unwrap_or(sha).to_string()
}

fn is_release(pipeline: &str) -> bool {
    pipeline.to_ascii_lowercase().contains("release")
}

/// kiln's own failures (a runner lost, the infrastructure, an image pull, a full disk): not the code's.
fn kilns_own(class: Option<&str>) -> bool {
    matches!(class, Some("runner-lost" | "infra" | "image-pull" | "disk-full"))
}

fn attempt_of(status: &str, conclusion: Option<&str>, class: Option<&str>) -> Attempt {
    match (status, conclusion) {
        ("queued" | "waiting" | "pending" | "blocked", _) => Attempt::Queued,
        ("running" | "in_progress", _) => Attempt::Running,
        (_, Some("success" | "neutral" | "skipped")) => Attempt::Passed,
        (_, Some("cancelled")) => Attempt::Cancelled,
        (_, Some(_)) if kilns_own(class) => Attempt::KilnError,
        (_, Some(_)) => Attempt::Failed,
        _ => Attempt::Queued,
    }
}

fn part_of(status: &str, conclusion: Option<&str>) -> Part {
    match attempt_of(status, conclusion, None) {
        Attempt::Queued => Part::Queued,
        Attempt::Running => Part::Running,
        Attempt::Passed | Attempt::Reused => Part::Passed,
        Attempt::Cancelled => Part::Superseded,
        Attempt::Failed | Attempt::KilnError => Part::Failed,
    }
}

/// Reads one commit's records (`sha`: the whole sha, or any unique prefix of seven or more).
fn load(c: &rusqlite::Connection, repo: &str, sha: &str) -> anyhow::Result<Option<Records>> {
    let found: Option<(String, String, String, i64)> = c
        .query_row(
            "SELECT p.id, p.name, r.sha, MIN(r.rowid) FROM runs r JOIN repos p ON p.id = r.repo_id
             WHERE (p.name = ?1 OR p.id = ?1) AND (r.sha = ?2 OR (length(?2) >= 7 AND r.sha LIKE ?2 || '%')) GROUP BY p.id, r.sha LIMIT 1",
            params![repo, sha],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((repo_id, repo_name, sha, first_row)) = found else { return Ok(None) };
    let mut rec = Records { repo: repo_name, sha: sha.clone(), first_row, ..Records::default() };

    let mut st = c.prepare("SELECT id, pipeline, branch, message, author, created_at FROM runs WHERE repo_id = ?1 AND sha = ?2 ORDER BY rowid")?;
    rec.runs = st
        .query_map(params![repo_id, sha], |r| Ok(RunRec { id: r.get(0)?, pipeline: r.get(1)?, branch: r.get(2)?, message: r.get(3)?, actor: r.get(4)?, created_at: r.get(5)? }))?
        .collect::<Result<_, _>>()?;

    let mut st = c.prepare(
        "SELECT j.id, j.run_id, COALESCE(NULLIF(j.display, ''), j.key), j.status, j.conclusion, j.reused_from IS NOT NULL, j.error
         FROM jobs j JOIN runs r ON r.id = j.run_id WHERE r.repo_id = ?1 AND r.sha = ?2 AND j.job_id IS NOT NULL AND j.run_attempt = r.attempt ORDER BY j.rowid",
    )?;
    let legs: Vec<(String, LegRec)> = st
        .query_map(params![repo_id, sha], |r| {
            Ok((r.get::<_, String>(0)?, LegRec { run_id: r.get(1)?, name: r.get(2)?, status: r.get(3)?, conclusion: r.get(4)?, reused: r.get(5)?, error: r.get(6)?, attempts: Vec::new() }))
        })?
        .collect::<Result<_, _>>()?;
    let mut at = c.prepare("SELECT status, conclusion, failure_class FROM ci_attempts WHERE leg_id = ?1 ORDER BY number")?;
    for (id, mut leg) in legs {
        leg.attempts = at.query_map([&id], |r| Ok(AttemptRec { status: r.get(0)?, conclusion: r.get(1)?, class: r.get(2)? }))?.collect::<Result<_, _>>()?;
        rec.legs.push(leg);
    }

    let mut st = c.prepare("SELECT i.digest FROM ci_images i JOIN runs r ON r.id = i.run_id WHERE r.repo_id = ?1 AND r.sha = ?2 ORDER BY i.id")?;
    rec.images = st.query_map(params![repo_id, sha], |r| r.get(0))?.collect::<Result<_, _>>()?;

    rec.land = c
        .query_row("SELECT state, reason FROM land_queue WHERE repo_id = ?1 AND candidate = ?2 ORDER BY updated_at DESC LIMIT 1", params![repo_id, sha], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;

    // Each environment's deployments: this commit's latest in each, and what each runs now (its
    // newest deployment that succeeded).
    let deploy = |r: &rusqlite::Row<'_>| -> rusqlite::Result<DeployRec> {
        Ok(DeployRec {
            environment: r.get(0)?,
            production: r.get(1)?,
            sha: r.get(2)?,
            first_row: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            state: r.get(4)?,
            description: r.get(5)?,
            at: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
        })
    };
    let cols = "d.environment, (d.production = 1 OR lower(d.environment) = 'production'), d.sha,
         (SELECT MIN(x.rowid) FROM runs x WHERE x.repo_id = d.repo_id AND x.sha = d.sha),
         (SELECT s.state FROM ci_deployment_statuses s WHERE s.deployment_id = d.id ORDER BY s.id DESC LIMIT 1),
         (SELECT s.description FROM ci_deployment_statuses s WHERE s.deployment_id = d.id ORDER BY s.id DESC LIMIT 1),
         (SELECT s.created_at FROM ci_deployment_statuses s WHERE s.deployment_id = d.id ORDER BY s.id DESC LIMIT 1)";
    let mut st = c.prepare(&format!("SELECT {cols} FROM ci_deployments d WHERE d.repo_id = ?1 AND d.sha = ?2 AND d.id = (SELECT MAX(e.id) FROM ci_deployments e WHERE e.repo_id = d.repo_id AND e.environment = d.environment AND e.sha = d.sha)"))?;
    rec.own = st.query_map(params![repo_id, sha], deploy)?.collect::<Result<_, _>>()?;
    let mut st = c.prepare(&format!(
        "SELECT {cols} FROM ci_deployments d WHERE d.repo_id = ?1 AND d.id = (SELECT MAX(e.id) FROM ci_deployments e WHERE e.repo_id = d.repo_id AND e.environment = d.environment
           AND (SELECT s.state FROM ci_deployment_statuses s WHERE s.deployment_id = e.id ORDER BY s.id DESC LIMIT 1) = 'success')"
    ))?;
    rec.current = st.query_map([&repo_id], deploy)?.collect::<Result<_, _>>()?;

    // Deploy API releases of this commit (by version), and a newer one production waits on.
    let mut st = c.prepare(
        "SELECT state, json_extract(spec, '$.message'), json_extract(spec, '$.rolled_back_by') IS NOT NULL FROM deploy_releases
         WHERE lower(environment) = 'production' AND (json_extract(spec, '$.version') = ?1 OR (length(json_extract(spec, '$.version')) >= 7 AND ?1 LIKE json_extract(spec, '$.version') || '%')) ORDER BY id",
    )?;
    rec.releases = st.query_map([&sha], |r| Ok(ReleaseRec { state: r.get(0)?, message: r.get(1)?, rolled_back: r.get(2)? }))?.collect::<Result<_, _>>()?;
    rec.waiting = c
        .query_row(
            "SELECT x.sha, MIN(x.rowid) FROM deploy_releases d JOIN runs x ON x.repo_id = ?1 AND (x.sha = json_extract(d.spec, '$.version') OR x.sha LIKE json_extract(d.spec, '$.version') || '%')
             WHERE d.state = 'waiting' AND lower(d.environment) = 'production' GROUP BY x.sha ORDER BY MIN(x.rowid) DESC LIMIT 1",
            [&repo_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    // The commits a production approval would ship: those newer than what production runs, up to this one.
    let prod_row = rec.current.iter().find(|d| d.production).map(|d| d.first_row).unwrap_or(0);
    let branch = rec.runs.first().map(|r| r.branch.clone()).unwrap_or_default();
    rec.riders = c.query_row(
        "SELECT COUNT(*) FROM (SELECT MIN(rowid) AS f FROM runs WHERE repo_id = ?1 AND branch = ?2 GROUP BY sha) WHERE f > ?3 AND f <= ?4",
        params![repo_id, branch, prod_row, first_row],
        |r| r.get::<_, u32>(0),
    )?;
    let mut st = c.prepare("SELECT tag_name FROM ci_releases WHERE repo_id = ?1 AND target_commitish = ?2 ORDER BY id")?;
    rec.tags = st.query_map(params![repo_id, sha], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(Some(rec))
}

/// A commit's records as `kiln_cd::journey` reads them, `now` in ms.
fn journey_of(rec: &Records, runners: &[RunnerSeen], now: i64) -> Journey {
    let first = rec.runs.first();
    let author = Author::Person(first.map(|r| r.actor.clone()).unwrap_or_default());
    let land = match rec.land.as_ref().map(|(s, why)| (s.as_str(), why)) {
        Some(("testing", _)) => Land::Testing,
        Some(("queued", _)) => Land::Queued { ahead: 0 },
        Some(("ejected", why)) => Land::Ejected { why: why.clone().unwrap_or_default() },
        _ => Land::Landed,
    };
    let run_pipeline: HashMap<&str, &str> = rec.runs.iter().map(|r| (r.id.as_str(), r.pipeline.as_str())).collect();
    let mut checks: Vec<Check> = Vec::new();
    let mut platforms: Vec<Platform> = Vec::new();
    for leg in &rec.legs {
        let pipeline = run_pipeline.get(leg.run_id.as_str()).copied().unwrap_or_default();
        if is_release(pipeline) {
            platforms.push(Platform { name: leg.name.clone(), state: part_of(&leg.status, leg.conclusion.as_deref()) });
            continue;
        }
        let attempts: Vec<Attempt> = if leg.reused {
            vec![Attempt::Reused]
        } else if leg.attempts.is_empty() {
            vec![attempt_of(&leg.status, leg.conclusion.as_deref(), None)]
        } else {
            leg.attempts.iter().map(|a| attempt_of(&a.status, a.conclusion.as_deref(), a.class.as_deref())).collect()
        };
        let l = Leg { name: leg.name.clone(), attempts, note: leg.error.clone().unwrap_or_default() };
        match checks.iter_mut().find(|c| c.pipeline == pipeline) {
            Some(c) => c.legs.push(l),
            None => checks.push(Check { pipeline: pipeline.to_string(), legs: vec![l] }),
        }
    }
    let release_running = platforms.iter().any(|p| matches!(p.state, Part::Queued | Part::Running));
    let image = if !rec.images.is_empty() || !release_running { Part::Passed } else { Part::Running };
    let build = Build { platforms, image };

    let checks_ok = !matches!(journey::pips(&Journey { checks: checks.clone(), ..empty() }, now).get(1).map(|p| p.state), Some(journey::Glyph::Failed));
    let newer = |d: &DeployRec| d.first_row > rec.first_row;
    let own = |prod: bool| rec.own.iter().find(|d| d.production == prod && (prod || lower_is(&d.environment, "staging") || !rec.own.iter().any(|x| !x.production && lower_is(&x.environment, "staging"))));
    let current = |prod: bool| rec.current.iter().find(|d| d.production == prod && (prod || lower_is(&d.environment, "staging") || !rec.current.iter().any(|x| !x.production && lower_is(&x.environment, "staging"))));

    let staging = match (own(false), current(false)) {
        _ if !checks_ok => Staging::Skipped,
        (Some(d), cur) => match d.state.as_deref() {
            Some("success") if cur.map(|c| c.sha == rec.sha).unwrap_or(false) => Staging::Healthy,
            Some("success" | "inactive") => Staging::Moved,
            Some("queued" | "pending" | "in_progress") => Staging::Syncing,
            _ => Staging::NotYet,
        },
        (None, Some(cur)) if newer(cur) => Staging::Rode { with: short(&cur.sha) },
        _ => Staging::NotYet,
    };

    let cur_prod = current(true);
    let in_production = cur_prod.map(|c| c.sha == rec.sha).unwrap_or(false);
    let waiting_release = rec.releases.iter().any(|r| r.state == "waiting");
    let rolled = rec.releases.iter().rev().find(|r| r.rolled_back || r.state == "failed");
    let production = if waiting_release {
        Production::WaitsForApproval { commits: rec.riders.max(1) }
    } else if let Some(r) = rolled.filter(|r| r.rolled_back) {
        Production::RolledBack { to: cur_prod.map(|c| short(&c.sha)).unwrap_or_default(), why: r.message.clone().unwrap_or_default(), acknowledged: false }
    } else if let Some(d) = own(true) {
        let at = d.at.saturating_mul(1000);
        match d.state.as_deref() {
            Some("queued" | "pending" | "in_progress") => Production::Delivering,
            Some("success") if in_production && now < at.saturating_add(GUARD_MS) => Production::Guard { since: at, until: at.saturating_add(GUARD_MS) },
            Some("success") if in_production => Production::Runs,
            Some("success" | "inactive") => Production::Moved,
            Some("failure" | "error") => Production::RolledBack { to: cur_prod.map(|c| short(&c.sha)).unwrap_or_default(), why: d.description.clone().unwrap_or_default(), acknowledged: false },
            _ => Production::NotYet,
        }
    } else {
        match (cur_prod, &rec.waiting) {
            (Some(cur), _) if newer(cur) && checks_ok => Production::Rode { with: short(&cur.sha) },
            (_, Some((sha, row))) if *row > rec.first_row && checks_ok && cur_prod.map(|c| c.first_row < rec.first_row).unwrap_or(true) => Production::Rides { with: short(sha) },
            _ => Production::NotYet,
        }
    };

    let replant = in_production.then(|| Replant {
        server: Part::Passed,
        runners: runners
            .iter()
            .map(|r| {
                let on = r.commit.as_deref().map(|c| rec.sha.starts_with(c) || c.starts_with(&rec.sha)).unwrap_or(false);
                let state = if r.failed.is_some() {
                    Runner::Failed
                } else if !r.connected {
                    Runner::Offline
                } else if on {
                    Runner::On
                } else {
                    Runner::Waiting
                };
                RunnerReplant { name: r.name.clone(), state, why: r.failed.clone().unwrap_or_default() }
            })
            .collect(),
    });

    Journey { author, land, checks, build, staging, production, replant, in_production, handled_by: None, fixed_by: None }
}

fn lower_is(s: &str, want: &str) -> bool {
    s.eq_ignore_ascii_case(want)
}

fn empty() -> Journey {
    Journey {
        author: Author::Person(String::new()),
        land: Land::Landed,
        checks: Vec::new(),
        build: Build { platforms: Vec::new(), image: Part::Passed },
        staging: Staging::NotYet,
        production: Production::NotYet,
        replant: None,
        in_production: false,
        handled_by: None,
        fixed_by: None,
    }
}

/// A commit as the list shows it.
fn summary(rec: &Records, runners: &[RunnerSeen], now: i64) -> Value {
    let j = journey_of(rec, runners, now);
    let s = journey::standing(&j, now);
    let first = rec.runs.first();
    let envs: Vec<Value> = rec.current.iter().filter(|d| d.sha == rec.sha).map(|d| json!({ "environment": d.environment, "production": d.production })).collect();
    json!({
        "repo": rec.repo, "sha": rec.sha, "short": short(&rec.sha),
        "message": first.map(|r| r.message.lines().next().unwrap_or_default().to_string()).unwrap_or_default(),
        "author": first.map(|r| r.actor.clone()).unwrap_or_default(),
        "branch": first.map(|r| r.branch.clone()).unwrap_or_default(),
        "at": first.map(|r| r.created_at).unwrap_or_default(),
        "stand": s.stand, "line": s.line, "pips": s.pips,
        "environments": envs, "tags": rec.tags,
        "runs": rec.runs.iter().map(|r| json!({ "id": r.id, "pipeline": r.pipeline })).collect::<Vec<_>>(),
        "cursor": rec.first_row,
    })
}

/// A commit with every record its stand came from.
fn detail(rec: &Records, runners: &[RunnerSeen], now: i64) -> Value {
    let mut v = summary(rec, runners, now);
    let j = journey_of(rec, runners, now);
    let checks: Vec<Value> = j
        .checks
        .iter()
        .map(|c| json!({ "pipeline": c.pipeline, "legs": c.legs.iter().map(|l| json!({ "name": l.name, "state": journey::leg_glyph(l), "attempts": l.attempts.len(), "note": l.note })).collect::<Vec<_>>() }))
        .collect();
    let part = |p: Part| match p {
        Part::Queued => "queued",
        Part::Running => "running",
        Part::Passed => "passed",
        Part::Failed => "failed",
        Part::Superseded => "superseded",
    };
    let build = json!({
        "platforms": j.build.platforms.iter().map(|p| json!({ "name": p.name, "state": part(p.state) })).collect::<Vec<_>>(),
        "image": { "state": part(j.build.image), "digest": rec.images.last() },
    });
    let deployments: Vec<Value> = rec.own.iter().map(|d| json!({ "environment": d.environment, "production": d.production, "state": d.state, "description": d.description, "at": d.at })).collect();
    let replant: Vec<Value> = j.replant.as_ref().map(|r| r.runners.iter().map(|x| json!({ "name": x.name, "state": format!("{:?}", x.state).to_lowercase(), "why": x.why })).collect()).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("checks".into(), json!(checks));
        o.insert("build".into(), build);
        o.insert("deployments".into(), json!(deployments));
        o.insert("releases".into(), json!(rec.releases.iter().map(|r| json!({ "state": r.state, "message": r.message, "rolled_back": r.rolled_back })).collect::<Vec<_>>()));
        o.insert("replant".into(), json!(replant));
    }
    v
}

/// A repository's commits, newest first, a page at a time (`cursor`: the last page's last
/// `cursor`), of the repositories named in `only` (JSON, those the caller may read), or all.
pub async fn list_in(store: &crate::store::Store, q: &HashMap<String, String>, only: Option<String>, runners: Vec<RunnerSeen>, now: i64) -> Result<Value, Fail> {
    let limit = q.get("limit").and_then(|l| l.parse::<i64>().ok()).unwrap_or(30).clamp(1, 100);
    let cursor = q.get("cursor").and_then(|c| c.parse::<i64>().ok()).unwrap_or(i64::MAX);
    let repo = q.get("repo").filter(|v| !v.is_empty()).cloned();
    let recs = store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT p.name, r.sha, MIN(r.rowid) AS f FROM runs r JOIN repos p ON p.id = r.repo_id
                 WHERE (?1 IS NULL OR p.name = ?1 OR p.id = ?1) AND (?4 IS NULL OR p.name IN (SELECT value FROM json_each(?4)))
                 GROUP BY r.repo_id, r.sha HAVING f < ?2 ORDER BY f DESC LIMIT ?3",
            )?;
            let keys: Vec<(String, String)> = st.query_map(params![repo, cursor, limit + 1, only], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            let mut out = Vec::new();
            for (repo, sha) in keys {
                if let Some(rec) = load(c, &repo, &sha)? {
                    out.push(rec);
                }
            }
            Ok(out)
        })
        .await
        .map_err(internal)?;
    let more = recs.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    let page: Vec<Value> = recs.iter().take(usize::try_from(limit).unwrap_or(30)).map(|r| summary(r, &runners, now)).collect();
    let next = if more { page.last().and_then(|c| c.get("cursor")).cloned() } else { None };
    Ok(json!({ "commits": page, "next_cursor": next }))
}

/// One commit of a repository, with the records its stand came from.
pub async fn get_in(store: &crate::store::Store, repo: &str, sha: &str, runners: Vec<RunnerSeen>, now: i64) -> Result<Value, Fail> {
    let (repo, sha) = (repo.to_owned(), sha.to_owned());
    let rec = store.call(move |c| load(c, &repo, &sha)).await.map_err(internal)?;
    rec.map(|r| detail(&r, &runners, now)).ok_or_else(|| (StatusCode::NOT_FOUND, "no such commit".to_string()))
}

/// The runners as the updates know them, for the commit production runs.
async fn runners_seen(hub: &std::sync::Arc<Hub>) -> Vec<RunnerSeen> {
    let jobs = hub.updates.jobs();
    crate::updates::runners(hub)
        .await
        .into_iter()
        .map(|r| {
            let failed = jobs
                .iter()
                .rev()
                .find(|j| j.kind == "runner" && j.target == r.name)
                .filter(|j| matches!(j.state, crate::updates::JobState::Failed))
                .map(|j| j.error.clone().unwrap_or_else(|| "the upgrade failed".to_string()));
            RunnerSeen { name: r.name, connected: r.connected, commit: r.commit, failed }
        })
        .collect()
}

/// `GET /api/ship/commits?…`
pub async fn list(hub: &std::sync::Arc<Hub>, q: &HashMap<String, String>, caller: Option<&crate::authz::Caller>) -> Result<Response, Fail> {
    let only: Option<String> = match caller {
        Some(c) => crate::authz::gate::readable(hub, c, "runs.read").await.map(|names| serde_json::to_string(&names).unwrap_or_else(|_| "[]".into())),
        None => None,
    };
    let runners = runners_seen(hub).await;
    Ok(srv::json(StatusCode::OK, &list_in(&hub.store, q, only, runners, crate::util::now_ms()).await?))
}

/// `GET /api/ship/commits/:repo/:sha`
pub async fn get(hub: &std::sync::Arc<Hub>, repo: &str, sha: &str) -> Result<Response, Fail> {
    let runners = runners_seen(hub).await;
    Ok(srv::json(StatusCode::OK, &get_in(&hub.store, repo, sha, runners, crate::util::now_ms()).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const C1: &str = "1111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const C2: &str = "2222222bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C3: &str = "3333333ccccccccccccccccccccccccccccccccc";
    const NOW_S: i64 = 1_800_000_000;

    /// kiln's own repository, three commits on main: the oldest in production with every runner
    /// replanted onto it; the next a flaky pass healthy in staging, production waiting on a person
    /// for it; the newest a person's, failing its checks.
    fn fixture() -> String {
        let leg = |id: &str, run: &str, name: &str, status: &str, conclusion: &str| {
            format!("INSERT INTO jobs (id, run_id, key, display, status, conclusion, labels, needs, spec, job_id, run_attempt) VALUES ('{id}', '{run}', '{name}', '{name}', '{status}', '{conclusion}', '[]', '[]', '{{}}', '{name}', 1);")
        };
        let run = |id: &str, n: i64, pipeline: &str, sha: &str, author: &str, msg: &str| {
            format!("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at, attempt) VALUES ('{id}', 'kiln', {n}, '{pipeline}', 'push', 'main', '{sha}', '{msg}', '{author}', 'completed', {NOW_S}, 1);")
        };
        let mut s = String::from("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('kiln', 'hyper-light/kiln', 'https://example.invalid/kiln.git', 1);");
        s += &run("r1", 1, "ci", C1, "ada", "deploy: a rollback holds");
        s += &leg("l1", "r1", "rust lint + test", "completed", "success");
        s += &run("r2", 2, "release", C1, "ada", "deploy: a rollback holds");
        for (i, p) in ["Linux", "Windows", "macOS"].iter().enumerate() {
            s += &leg(&format!("p1{i}"), "r2", p, "completed", "success");
        }
        s += &run("r3", 3, "ci", C2, "Kiln Distro", "docs: the train notes");
        s += &leg("l2", "r3", "playwright", "completed", "success");
        s += "INSERT INTO ci_attempts (id, leg_id, number, status, conclusion, failure_class) VALUES ('a1', 'l2', 1, 'done', 'failure', 'failure'), ('a2', 'l2', 2, 'done', 'success', NULL);";
        s += &run("r4", 4, "release", C2, "Kiln Distro", "docs: the train notes");
        s += &leg("p2", "r4", "Linux", "completed", "success");
        s += &format!("INSERT INTO ci_images (repo_id, name, reference, digest, run_id, created_at) VALUES ('kiln', 'kiln', 'kiln:2222222', 'sha256:abc', 'r4', {NOW_S});");
        s += &run("r5", 5, "ci", C3, "mei", "sync: a move carries its homes");
        s += &leg("l3", "r5", "rust lint + test", "completed", "failure");
        s += "UPDATE jobs SET error = 'assertion failed in sync::reconnect' WHERE id = 'l3';";
        let deploy = |id: i64, env: &str, sha: &str, prod: i64, at: i64| {
            format!("INSERT INTO ci_deployments (id, repo_id, environment, sha, git_ref, creator, production, created_at, updated_at) VALUES ({id}, 'kiln', '{env}', '{sha}', 'main', 'kiln', {prod}, {at}, {at});
                     INSERT INTO ci_deployment_statuses (deployment_id, state, creator, created_at) VALUES ({id}, 'success', 'kiln', {at});")
        };
        s += &deploy(1, "staging", C1, 0, NOW_S - 7200);
        s += &deploy(2, "production", C1, 1, NOW_S - 3600);
        s += &deploy(3, "staging", C2, 0, NOW_S - 600);
        s += &format!("INSERT INTO deploy_releases (app, environment, state, spec, created_at) VALUES ('kiln', 'production', 'waiting', '{{\"version\": \"{C2}\"}}', {NOW_S});");
        s
    }

    fn runners() -> Vec<RunnerSeen> {
        vec![
            RunnerSeen { name: "metal-ryzen-01".into(), connected: true, commit: Some(short(C1)), failed: None },
            RunnerSeen { name: "metal-epyc-01".into(), connected: true, commit: Some(C1.into()), failed: None },
        ]
    }

    fn field<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
        path.iter().try_fold(v, |v, k| v.get(k))
    }

    #[test]
    fn ship_commits_of_kilns_own_repository_from_its_records() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let now = NOW_S * 1000;
            let list = list_in(&store, &HashMap::from([("repo".to_string(), "hyper-light/kiln".to_string())]), None, runners(), now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let commits = list.get("commits").and_then(Value::as_array).cloned().unwrap_or_default();
            let got: Vec<(String, String, String)> = commits
                .iter()
                .map(|c| (field(c, &["short"]).and_then(Value::as_str).unwrap_or_default().to_string(), field(c, &["stand"]).and_then(Value::as_str).unwrap_or_default().to_string(), field(c, &["line"]).and_then(Value::as_str).unwrap_or_default().to_string()))
                .collect();
            assert_eq!(
                got,
                vec![
                    ("3333333".to_string(), "needs_you".to_string(), "rust lint + test · assertion failed in sync::reconnect".to_string()),
                    ("2222222".to_string(), "needs_you".to_string(), "Approve for production · 1 commit".to_string()),
                    ("1111111".to_string(), "shipped".to_string(), "In production · runners 2/2".to_string()),
                ]
            );

            let one = get_in(&store, "hyper-light/kiln", "2222222", runners(), now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let pip = |stage: &str| one.get("pips").and_then(Value::as_array).and_then(|ps| ps.iter().find(|p| p.get("stage").and_then(Value::as_str) == Some(stage))).and_then(|p| p.get("state")).and_then(Value::as_str).map(str::to_string);
            assert_eq!(pip("checks").as_deref(), Some("flaky"));
            assert_eq!(pip("staging").as_deref(), Some("healthy"));
            assert_eq!(pip("production").as_deref(), Some("needs"));
            assert_eq!(field(&one, &["build", "image", "digest"]).and_then(Value::as_str), Some("sha256:abc"));
            assert_eq!(field(&one, &["checks"]).and_then(Value::as_array).map(Vec::len), Some(1));

            let shipped = get_in(&store, "hyper-light/kiln", C1, runners(), now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(field(&shipped, &["replant"]).and_then(Value::as_array).map(Vec::len), Some(2));
            let missing = get_in(&store, "hyper-light/kiln", "deadbee", runners(), now).await;
            assert!(matches!(missing, Err((StatusCode::NOT_FOUND, _))));
            Ok(())
        })
    }
}
