//! Insights (docs/ci.md W19-R01, R02; W08-R13; W05-R10's costs from history).
//!
//! - **Rollups.** Every minute, each hour that has passed is rolled up from the legs, runs and
//!   steps that finished in it, one row per repository, workflow, job, step, executor and runner
//!   (`ci_rollups`, kiln_ci's [`Rollup`]); a day is its hours merged. Hours are kept 35 days,
//!   days for good. What finished since the last rolled-up hour is read live, so insights are
//!   current to the second.
//! - **Runner time.** Each minute adds every runner's connected seconds, slot-seconds and busy
//!   slot-seconds to its hour (`ci_runner_time`), for utilization.
//! - **Cache events.** Lookups (hit or miss) and saves, by key, with their bytes
//!   (`ci_cache_events`), for hit rates and time saved.
//! - **The API** (`/api/ci/insights/…`): repositories (their share of runner time against their
//!   fair-share weight), a repository's workflows, a workflow's jobs, slowest steps, executors
//!   and pools (with costs), cache and critical path, and runners (utilization, failure classes).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::StatusCode;
use kiln_ci::insights::{Fact, Rollup};
use rusqlite::params;
use serde_json::{Value, json};

use super::Fail;
use super::token::Claims;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
/// An hour is rolled up once it is this far behind, when what finished in it is written.
const SETTLE: i64 = 120;
/// How long hourly rollups, runner time and cache events are kept; daily rollups stay.
const KEEP_HOURS: i64 = 35 * DAY;
const KEEP_EVENTS: i64 = 90 * DAY;
/// Hours rolled up per pass while catching up (a first start, a long time down).
const PER_PASS: usize = 96;
/// How far back a query reads live what isn't rolled up yet.
const LIVE: i64 = 3 * DAY;

fn floor(t: i64, step: i64) -> i64 {
    t - t.rem_euclid(step)
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// What a rollup row is about. `job` empty: a run; `step` empty: a leg.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Key {
    repo: String,
    workflow: String,
    job: String,
    step: String,
    executor: String,
    runner: String,
}

impl Key {
    fn is_run(&self) -> bool {
        self.job.is_empty()
    }

    fn is_leg(&self) -> bool {
        !self.job.is_empty() && self.step.is_empty()
    }

    fn is_step(&self) -> bool {
        !self.step.is_empty()
    }
}

/// The engine's statuses as the conclusions rollups count.
fn conclusion(s: &str) -> String {
    match s {
        "succeeded" => "success",
        "failed" | "error" => "failure",
        other => other,
    }
    .to_owned()
}

fn failed(c: &str) -> bool {
    matches!(c, "failure" | "timed_out" | "startup_failure")
}

// ------------------------------------------------------------------ cache events (W08-R13)

/// A cache lookup (`hit`, `miss`) or `save` by a leg, recorded off the request's path.
pub fn cache_event(hub: &Arc<Hub>, c: &Claims, key: &str, kind: &'static str, bytes: Option<u64>) {
    crate::metrics::global().ci_cache.inc(&[kind]);
    let (hub, repo, run, leg, key) = (Arc::clone(hub), c.repo.clone(), c.run.clone(), c.sub.clone(), key.to_owned());
    let bytes = bytes.and_then(|b| i64::try_from(b).ok());
    crate::rt::spawn(async move {
        let now = crate::util::now_unix();
        let _ = hub
            .store
            .call(move |c| Ok(c.execute("INSERT INTO ci_cache_events (ts, repo_id, run_id, leg_id, key, kind, bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![now, repo, run, leg, key, kind, bytes]).map(|_| ())?))
            .await;
    });
}

/// A cache key's prefix: the key without the hashes and numbers it ends in
/// (`Linux-cargo-3f9a…` → `Linux-cargo`).
pub fn key_prefix(key: &str) -> String {
    let mut parts: Vec<&str> = key.split('-').collect();
    while parts.len() > 1 && parts.last().is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()) || (p.len() >= 8 && p.chars().all(|c| c.is_ascii_hexdigit()))) {
        parts.pop();
    }
    parts.join("-")
}

// ------------------------------------------------------------------ rollups

/// Rolls up and samples for as long as the server runs.
pub fn spawn(hub: Arc<Hub>) {
    crate::rt::spawn(async move {
        let mut last = Instant::now();
        // The first pass soon after starting, then one a minute.
        let mut wait = Duration::from_secs(5);
        loop {
            crate::rt::sleep(wait).await;
            wait = Duration::from_secs(60);
            // Seconds since the last sample, at most two minutes' worth after a stall.
            let secs = last.elapsed().as_secs_f64().min(120.0);
            last = Instant::now();
            if let Err(e) = sample(&hub, secs).await {
                tracing::warn!(error = %format!("{e:#}"), "runner time could not be sampled");
            }
            if let Err(e) = roll(&hub).await {
                tracing::warn!(error = %format!("{e:#}"), "insights could not be rolled up");
            }
        }
    });
}

/// Adds `secs` of each runner's time to its hour: connected, slots and busy slots.
async fn sample(hub: &Hub, secs: f64) -> anyhow::Result<()> {
    let bucket = floor(crate::util::now_unix(), HOUR);
    let rows: Vec<(String, f64, f64)> = hub.agents.lock().values().filter(|a| a.connected && a.slots > 0 && !a.outpost).map(|a| (a.name.clone(), f64::from(a.slots), f64::from(a.busy.min(a.slots)))).collect();
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            for (runner, slots, busy) in rows {
                tx.execute(
                    "INSERT INTO ci_runner_time (bucket, runner, connected, slot_seconds, busy_seconds) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (bucket, runner) DO UPDATE SET connected = connected + excluded.connected, slot_seconds = slot_seconds + excluded.slot_seconds, busy_seconds = busy_seconds + excluded.busy_seconds",
                    params![bucket, runner, secs, secs * slots, secs * busy],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

/// The first bucket of `grain` not rolled up yet: after the last one, else the day the oldest
/// finished leg or run is in (so its day is whole). `None` with nothing to roll up.
async fn next(hub: &Hub, grain: &'static str) -> anyhow::Result<Option<i64>> {
    hub.store
        .call(move |c| {
            let step = if grain == "day" { DAY } else { HOUR };
            let last: Option<i64> = c.query_row("SELECT MAX(bucket) FROM ci_rollup_done WHERE grain = ?1", [grain], |r| r.get(0))?;
            if let Some(l) = last {
                return Ok(Some(l + step));
            }
            let first: Option<i64> = if grain == "day" {
                c.query_row("SELECT MIN(bucket) FROM ci_rollup_done WHERE grain = 'hour'", [], |r| r.get(0))?
            } else {
                c.query_row(
                    "SELECT MIN(t) FROM (SELECT MIN(finished_at) AS t FROM jobs WHERE job_id IS NOT NULL AND finished_at IS NOT NULL UNION ALL SELECT MIN(finished_at) FROM runs WHERE workflow_id IS NOT NULL AND finished_at IS NOT NULL)",
                    [],
                    |r| r.get(0),
                )?
            };
            Ok(first.map(|t| floor(t, DAY)))
        })
        .await
}

/// Rolls up the hours that have passed and the days they complete; drops what is past keeping.
async fn roll(hub: &Hub) -> anyhow::Result<()> {
    let now = crate::util::now_unix();
    let Some(mut hour) = next(hub, "hour").await? else { return Ok(()) };
    let mut n = 0;
    while hour + HOUR + SETTLE <= now && n < PER_PASS {
        let rows = facts(hub, hour, hour + HOUR, HOUR, Filter::default()).await?;
        write(hub, "hour", hour, rows.into_iter().map(|((_, k), r)| (k, r)).collect()).await?;
        hour += HOUR;
        n += 1;
    }
    crate::metrics::global().ci_rollup_lag.set((now - hour).max(0));
    while let Some(day) = next(hub, "day").await?.filter(|d| d + DAY <= hour) {
        let hours = stored(hub, "hour", day, day + DAY, Filter::default()).await?;
        let mut merged: HashMap<Key, Rollup> = HashMap::new();
        for (_, k, r) in hours {
            merged.entry(k).or_default().merge(&r);
        }
        write(hub, "day", day, merged).await?;
    }
    let (hours, events) = (now - KEEP_HOURS, now - KEEP_EVENTS);
    hub.store
        .call(move |c| {
            c.execute("DELETE FROM ci_rollups WHERE grain = 'hour' AND bucket < ?1", [hours])?;
            c.execute("DELETE FROM ci_runner_time WHERE bucket < ?1", [events])?;
            c.execute("DELETE FROM ci_cache_events WHERE ts < ?1", [events])?;
            Ok(())
        })
        .await
}

async fn write(hub: &Hub, grain: &'static str, bucket: i64, rows: HashMap<Key, Rollup>) -> anyhow::Result<()> {
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM ci_rollups WHERE grain = ?1 AND bucket = ?2", params![grain, bucket])?;
            {
                let mut st = tx.prepare("INSERT INTO ci_rollups (grain, bucket, repo_id, workflow, job, step, executor, runner, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)")?;
                for (k, r) in &rows {
                    st.execute(params![grain, bucket, k.repo, k.workflow, k.job, k.step, k.executor, k.runner, serde_json::to_string(r)?])?;
                }
            }
            tx.execute("INSERT OR IGNORE INTO ci_rollup_done (grain, bucket) VALUES (?1, ?2)", params![grain, bucket])?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// Which rows a query wants.
#[derive(Debug, Clone, Default)]
struct Filter {
    repo: Option<String>,
    workflow: Option<String>,
    runner: Option<String>,
}

impl Filter {
    fn takes(&self, k: &Key) -> bool {
        self.repo.as_ref().is_none_or(|r| r == &k.repo) && self.workflow.as_ref().is_none_or(|w| w == &k.workflow) && self.runner.as_ref().is_none_or(|r| r == &k.runner)
    }
}

/// Stored rollups of `grain` with buckets in `[a, b)`.
async fn stored(hub: &Hub, grain: &'static str, a: i64, b: i64, f: Filter) -> anyhow::Result<Vec<(i64, Key, Rollup)>> {
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT bucket, repo_id, workflow, job, step, executor, runner, data FROM ci_rollups
                 WHERE grain = ?1 AND bucket >= ?2 AND bucket < ?3 AND (?4 IS NULL OR repo_id = ?4) AND (?5 IS NULL OR workflow = ?5) AND (?6 IS NULL OR runner = ?6)",
            )?;
            let rows = st.query_map(params![grain, a, b, f.repo, f.workflow, f.runner], |x| {
                let key = Key { repo: x.get(1)?, workflow: x.get(2)?, job: x.get(3)?, step: x.get(4)?, executor: x.get(5)?, runner: x.get(6)? };
                Ok((x.get::<_, i64>(0)?, key, x.get::<_, String>(7)?))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (bucket, key, data) = r?;
                out.push((bucket, key, serde_json::from_str::<Rollup>(&data).unwrap_or_default()));
            }
            Ok(out)
        })
        .await
}

/// One attempt of a leg: its number, conclusion, failure class, start and end.
type Attempt = (i64, Option<String>, Option<String>, Option<i64>, Option<i64>);

/// Rollups of what finished in `[a, b)`, by bucket (`step` seconds long) and key: runs, legs
/// (with their attempts' runner time, flakiness and failure class) and steps.
async fn facts(hub: &Hub, a: i64, b: i64, step: i64, f: Filter) -> anyhow::Result<HashMap<(i64, Key), Rollup>> {
    hub.store
        .call(move |c| {
            let mut out: HashMap<(i64, Key), Rollup> = HashMap::new();
            let mut attempts: HashMap<String, Vec<Attempt>> = HashMap::new();
            {
                let mut st = c.prepare("SELECT a.leg_id, a.number, a.conclusion, a.failure_class, a.started_at, a.finished_at FROM ci_attempts a JOIN jobs j ON j.id = a.leg_id WHERE j.job_id IS NOT NULL AND j.finished_at >= ?1 AND j.finished_at < ?2")?;
                let rows = st.query_map(params![a, b], |x| Ok((x.get::<_, String>(0)?, (x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?, x.get(5)?))))?;
                for r in rows {
                    let (leg, at) = r?;
                    attempts.entry(leg).or_default().push(at);
                }
            }
            {
                let mut st = c.prepare(
                    "SELECT j.id, r.repo_id, COALESCE(r.workflow_id, ''), COALESCE(j.job_id, ''), COALESCE(j.executor, ''), COALESCE(j.agent, ''), COALESCE(j.conclusion, j.status),
                            j.queued_at, j.started_at, j.finished_at, COALESCE(j.cpu_seconds, 0), COALESCE(j.peak_rss, 0), COALESCE(j.attempt, 1), j.waits
                     FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.job_id IS NOT NULL AND j.finished_at >= ?1 AND j.finished_at < ?2",
                )?;
                let rows = st.query_map(params![a, b], |x| {
                    let key = Key { repo: x.get(1)?, workflow: x.get(2)?, job: x.get(3)?, step: String::new(), executor: x.get(4)?, runner: x.get(5)? };
                    let times: (Option<i64>, Option<i64>, i64) = (x.get(7)?, x.get(8)?, x.get(9)?);
                    Ok((x.get::<_, String>(0)?, key, x.get::<_, String>(6)?, times, x.get::<_, f64>(10)?, x.get::<_, i64>(11)?, x.get::<_, i64>(12)?, x.get::<_, Option<String>>(13)?))
                })?;
                for r in rows {
                    let (id, key, status, (queued, started, finished), cpu, rss, attempt, waits) = r?;
                    if !f.takes(&key) {
                        continue;
                    }
                    let concl = conclusion(&status);
                    let tries = attempts.get(&id).map(Vec::as_slice).unwrap_or_default();
                    let duration = started.map(|s| (finished - s).max(0) as f64);
                    let ran: f64 = tries.iter().filter_map(|t| Some((t.4? - t.3?).max(0) as f64)).sum();
                    let waits: BTreeMap<String, f64> = waits.and_then(|w| serde_json::from_str(&w).ok()).unwrap_or_default();
                    let queue = if waits.is_empty() { started.zip(queued).map(|(s, q)| (s - q).max(0) as f64) } else { Some(waits.values().sum()) };
                    let class = failed(&concl).then(|| tries.iter().max_by_key(|t| t.0).and_then(|t| t.2.clone()).unwrap_or_else(|| "failure".into()));
                    let fact = Fact {
                        flaky: concl == "success" && tries.iter().any(|t| t.1.as_deref().is_some_and(failed)),
                        conclusion: concl,
                        duration,
                        queue,
                        cpu_seconds: cpu,
                        peak_rss: u64::try_from(rss).unwrap_or(0),
                        runner_seconds: if ran > 0.0 { ran } else { duration.unwrap_or(0.0) },
                        attempts: u32::try_from(attempt.max(i64::try_from(tries.len()).unwrap_or(1))).unwrap_or(1),
                        waits,
                        failure_class: class,
                    };
                    out.entry((floor(finished, step), key)).or_default().add(&fact);
                }
            }
            {
                let mut st = c.prepare("SELECT repo_id, COALESCE(workflow_id, ''), COALESCE(conclusion, status), created_at, started_at, finished_at, COALESCE(attempt, 1) FROM runs WHERE workflow_id IS NOT NULL AND finished_at >= ?1 AND finished_at < ?2")?;
                let rows = st.query_map(params![a, b], |x| Ok((Key { repo: x.get(0)?, workflow: x.get(1)?, ..Key::default() }, x.get::<_, String>(2)?, x.get::<_, i64>(3)?, x.get::<_, Option<i64>>(4)?, x.get::<_, i64>(5)?, x.get::<_, i64>(6)?)))?;
                for r in rows {
                    let (key, status, created, started, finished, attempt) = r?;
                    if !f.takes(&key) {
                        continue;
                    }
                    let concl = conclusion(&status);
                    let fact = Fact {
                        flaky: concl == "success" && attempt > 1,
                        conclusion: concl,
                        duration: Some((finished - started.unwrap_or(created)).max(0) as f64),
                        queue: started.map(|s| (s - created).max(0) as f64),
                        attempts: u32::try_from(attempt).unwrap_or(1),
                        ..Fact::default()
                    };
                    out.entry((floor(finished, step), key)).or_default().add(&fact);
                }
            }
            {
                let mut st = c.prepare(
                    "SELECT r.repo_id, COALESCE(r.workflow_id, ''), COALESCE(j.job_id, ''), s.name, COALESCE(s.conclusion, s.status), s.started_at, s.finished_at, j.finished_at
                     FROM steps s JOIN jobs j ON j.id = s.job_id JOIN runs r ON r.id = j.run_id
                     WHERE j.job_id IS NOT NULL AND j.finished_at >= ?1 AND j.finished_at < ?2 AND s.started_at IS NOT NULL AND s.finished_at IS NOT NULL",
                )?;
                let rows = st.query_map(params![a, b], |x| Ok((Key { repo: x.get(0)?, workflow: x.get(1)?, job: x.get(2)?, step: x.get(3)?, ..Key::default() }, x.get::<_, String>(4)?, x.get::<_, i64>(5)?, x.get::<_, i64>(6)?, x.get::<_, i64>(7)?)))?;
                for r in rows {
                    let (key, status, started, finished, leg_finished) = r?;
                    if !f.takes(&key) {
                        continue;
                    }
                    let fact = Fact { conclusion: conclusion(&status), duration: Some((finished - started).max(0) as f64), ..Fact::default() };
                    out.entry((floor(leg_finished, step), key)).or_default().add(&fact);
                }
            }
            Ok(out)
        })
        .await
}

/// Rollups from `since` to now by bucket of `grain` (`DAY` or `HOUR`): stored days, stored hours
/// after them, and what finished since the last rolled-up hour, read live.
async fn load(hub: &Hub, f: &Filter, since: i64, grain: i64) -> anyhow::Result<Vec<(i64, Key, Rollup)>> {
    let now = crate::util::now_unix();
    let since = floor(since, grain);
    let hour_end = next(hub, "hour").await?.unwrap_or(now).max(since);
    let mut out: HashMap<(i64, Key), Rollup> = HashMap::new();
    let mut put = |bucket: i64, k: Key, r: &Rollup| out.entry((floor(bucket, grain), k)).or_default().merge(r);
    let hours_from = if grain == DAY {
        let day_end = next(hub, "day").await?.unwrap_or(since).clamp(since, hour_end);
        for (b, k, r) in stored(hub, "day", since, day_end, f.clone()).await? {
            put(b, k, &r);
        }
        day_end
    } else {
        since
    };
    for (b, k, r) in stored(hub, "hour", hours_from, hour_end, f.clone()).await? {
        put(b, k, &r);
    }
    // Live: what the rollups don't have yet, at most the last few days (while a first start
    // catches up, older hours fill in as they are rolled up).
    for ((b, k), r) in facts(hub, hour_end.max(now - LIVE), now + 1, grain, f.clone()).await? {
        put(b, k, &r);
    }
    Ok(out.into_iter().map(|((b, k), r)| (b, k, r)).collect())
}

/// Rows merged by what `group` names them (rows it names nothing are left out).
fn by<F: Fn(&Key) -> Option<String>>(rows: &[(i64, Key, Rollup)], group: F) -> BTreeMap<String, Rollup> {
    let mut out: BTreeMap<String, Rollup> = BTreeMap::new();
    for (_, k, r) in rows {
        if let Some(g) = group(k) {
            out.entry(g).or_default().merge(r);
        }
    }
    out
}

/// The rows `pick` takes, merged by bucket, oldest first.
fn series<F: Fn(&Key) -> bool>(rows: &[(i64, Key, Rollup)], pick: F) -> Vec<Value> {
    let mut out: BTreeMap<i64, Rollup> = BTreeMap::new();
    for (b, k, r) in rows {
        if pick(k) {
            out.entry(*b).or_default().merge(r);
        }
    }
    out.into_iter().map(|(b, r)| summary(&r, json!({ "bucket": b }))).collect()
}

/// A rollup as the API shows it, with `extra`'s fields first.
fn summary(r: &Rollup, extra: Value) -> Value {
    let mut v = extra;
    if let (Some(o), Ok(Value::Object(s))) = (v.as_object_mut(), serde_json::to_value(r.summary())) {
        o.extend(s);
    }
    v
}

/// Each job's median seconds, over the last two weeks of a workflow (W05-R10's expected cost).
pub async fn costs(hub: &Hub, repo: &str, workflow: &str) -> Arc<HashMap<String, f64>> {
    let f = Filter { repo: Some(repo.to_owned()), workflow: Some(workflow.to_owned()), runner: None };
    let rows = load(hub, &f, crate::util::now_unix() - 14 * DAY, DAY).await.unwrap_or_default();
    let jobs = by(&rows, |k| k.is_leg().then(|| k.job.clone()));
    Arc::new(jobs.into_iter().filter_map(|(j, r)| Some((j, r.duration.quantile(0.5)?))).collect())
}

// ------------------------------------------------------------------ the API

/// A runner's pool: its `pool:<name>` (or `pool=<name>`) label, else `default`.
async fn pools(hub: &Hub) -> HashMap<String, String> {
    let rows = hub.store.agents().await.unwrap_or_default();
    rows.into_iter().map(|a| (a.name, a.labels.iter().find_map(|l| l.strip_prefix("pool:").or_else(|| l.strip_prefix("pool="))).unwrap_or("default").to_owned())).collect()
}

fn cost(hub: &Hub, pool: &str, minutes: f64) -> Option<f64> {
    let rates = &hub.cfg.ci.insights.cost_per_minute;
    rates.get(pool).or_else(|| rates.get("default")).map(|r| r * minutes)
}

/// The window a request asks for: `?days=` (30 by default), with its grain (`?grain=hour|day`;
/// hours for two days or less).
fn window(q: &HashMap<String, String>) -> (i64, i64, i64) {
    let days: i64 = q.get("days").and_then(|d| d.parse().ok()).unwrap_or(30).clamp(1, 400);
    let grain = match q.get("grain").map(String::as_str) {
        Some("hour") if days <= 35 => HOUR,
        Some("day") => DAY,
        _ if days <= 2 => HOUR,
        _ => DAY,
    };
    (crate::util::now_unix() - days * DAY, days, grain)
}

async fn repo_named(hub: &Hub, repo: &str) -> Result<crate::store::Repo, Fail> {
    hub.store.repos().await.map_err(internal)?.into_iter().find(|x| x.id == repo || x.name == repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))
}

/// Each repository's runs, runner time, its share of all runner time and its fair-share
/// weight's share (W05-R10).
async fn shares(hub: &Hub, rows: &[(i64, Key, Rollup)]) -> anyhow::Result<Vec<Value>> {
    let names: HashMap<String, String> = hub.store.repos().await?.into_iter().map(|r| (r.id, r.name)).collect();
    let runs = by(rows, |k| k.is_run().then(|| k.repo.clone()));
    let legs = by(rows, |k| k.is_leg().then(|| k.repo.clone()));
    let total: f64 = legs.values().map(|r| r.runner_seconds).sum();
    let weight = |id: &str| names.get(id).and_then(|n| hub.cfg.ci.fairness.weights.get(n)).copied().filter(|w| *w > 0.0).unwrap_or(1.0);
    let ids: Vec<&String> = runs.keys().chain(legs.keys()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let weights: f64 = ids.iter().map(|id| weight(id)).sum();
    Ok(ids
        .into_iter()
        .map(|id| {
            let secs = legs.get(id).map_or(0.0, |r| r.runner_seconds);
            let r = runs.get(id).cloned().unwrap_or_default();
            json!({
                "id": id, "name": names.get(id), "runs": r.summary(), "runner_minutes": secs / 60.0,
                "share": (total > 0.0).then(|| secs / total), "weight": weight(id), "weight_share": (weights > 0.0).then(|| weight(id) / weights),
                "cost": cost(hub, "default", secs / 60.0),
            })
        })
        .collect())
}

/// `GET /api/ci/insights/…`.
pub async fn route(hub: &Hub, rest: &[&str], q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (since, days, grain) = window(q);
    let head = json!({ "since": since, "days": days, "grain": if grain == HOUR { "hour" } else { "day" }, "currency": hub.cfg.ci.insights.currency });
    let with = |mut v: Value| {
        if let (Some(o), Some(h)) = (v.as_object_mut(), head.as_object()) {
            o.extend(h.clone());
        }
        srv::json(StatusCode::OK, &v)
    };
    match rest {
        ["repos"] => {
            let rows = load(hub, &Filter::default(), since, DAY).await.map_err(internal)?;
            Ok(with(json!({ "repos": shares(hub, &rows).await.map_err(internal)? })))
        }
        ["repos", repo] => {
            let r = repo_named(hub, repo).await?;
            // The share needs everyone's runner time.
            let all = load(hub, &Filter::default(), since, grain).await.map_err(internal)?;
            let share = shares(hub, &all).await.map_err(internal)?.into_iter().find(|s| s.get("id").and_then(Value::as_str) == Some(r.id.as_str()));
            let rows: Vec<(i64, Key, Rollup)> = all.into_iter().filter(|(_, k, _)| k.repo == r.id).collect();
            let names = workflow_names(hub, &r.id).await.map_err(internal)?;
            let workflows: Vec<Value> = by(&rows, |k| k.is_run().then(|| k.workflow.clone()))
                .into_iter()
                .map(|(w, run)| {
                    let s = series(&rows, |k| k.is_run() && k.workflow == w);
                    summary(&run, json!({ "workflow": w, "name": names.get(&w), "series": s }))
                })
                .collect();
            Ok(with(json!({ "repo": { "id": r.id, "name": r.name }, "share": share, "workflows": workflows })))
        }
        ["workflows", repo] => {
            let r = repo_named(hub, repo).await?;
            let wf = q.get("workflow").cloned().ok_or((StatusCode::BAD_REQUEST, "which workflow: ?workflow=<path>".to_owned()))?;
            let f = Filter { repo: Some(r.id.clone()), workflow: Some(wf.clone()), runner: None };
            let rows = load(hub, &f, since, grain).await.map_err(internal)?;
            let runs = by(&rows, |k| k.is_run().then(String::new)).remove("").unwrap_or_default();
            let jobs: Vec<Value> = by(&rows, |k| k.is_leg().then(|| k.job.clone()))
                .into_iter()
                .map(|(j, legs)| {
                    let s = series(&rows, |k| k.is_leg() && k.job == j);
                    summary(&legs, json!({ "job": j, "series": s }))
                })
                .collect();
            let mut steps: Vec<(String, String, Rollup)> = by(&rows, |k| k.is_step().then(|| format!("{}\u{0}{}", k.job, k.step)))
                .into_iter()
                .filter_map(|(k, r)| {
                    let (job, step) = k.split_once('\u{0}')?;
                    Some((job.to_owned(), step.to_owned(), r))
                })
                .collect();
            steps.sort_by(|a, b| b.2.duration.quantile(0.5).unwrap_or(0.0).total_cmp(&a.2.duration.quantile(0.5).unwrap_or(0.0)));
            let steps: Vec<Value> = steps
                .into_iter()
                .take(20)
                .map(|(job, step, r)| json!({ "job": job, "step": step, "count": r.count, "p50": r.duration.quantile(0.5), "p90": r.duration.quantile(0.9), "mean": r.duration.mean(), "total": r.duration.sum, "success_rate": r.success_rate() }))
                .collect();
            let pool_of = pools(hub).await;
            // Legs that never started ran nowhere.
            let executors: Vec<Value> = by(&rows, |k| (k.is_leg() && !k.executor.is_empty()).then(|| format!("{}\u{0}{}", k.executor, pool_of.get(&k.runner).map_or("default", String::as_str))))
                .into_iter()
                .filter_map(|(k, r)| {
                    let (executor, pool) = k.split_once('\u{0}')?;
                    let minutes = r.runner_seconds / 60.0;
                    Some(json!({ "executor": executor, "pool": pool, "legs": r.count, "runner_minutes": minutes, "cpu_seconds": r.cpu_seconds, "peak_rss": r.peak_rss, "cost": cost(hub, pool, minutes) }))
                })
                .collect();
            let cache = cache_insights(hub, &r.id, Some(&wf), since).await.map_err(internal)?;
            let critical = critical(hub, &r.id, &wf).await.map_err(internal)?;
            let name = workflow_names(hub, &r.id).await.map_err(internal)?.remove(&wf);
            let s = series(&rows, Key::is_run);
            Ok(with(summary(&runs, json!({ "repo": { "id": r.id, "name": r.name }, "workflow": wf, "name": name, "series": s, "jobs": jobs, "steps": steps, "executors": executors, "cache": cache, "critical_path": critical }))))
        }
        ["runners"] => {
            let rows = load(hub, &Filter::default(), since, DAY).await.map_err(internal)?;
            let time = runner_time(hub, since, None).await.map_err(internal)?;
            let pool_of = pools(hub).await;
            let legs = by(&rows, |k| (k.is_leg() && !k.runner.is_empty()).then(|| k.runner.clone()));
            let names: std::collections::BTreeSet<&String> = legs.keys().chain(time.keys()).collect();
            let runners: Vec<Value> = names.into_iter().map(|n| runner_json(hub, n, legs.get(n), time.get(n).map(|t| t.iter().fold((0.0, 0.0, 0.0), |a, (_, c, s, b)| (a.0 + c, a.1 + s, a.2 + b))), pool_of.get(n))).collect();
            Ok(with(json!({ "runners": runners })))
        }
        ["runners", name] => {
            let f = Filter { runner: Some((*name).to_owned()), ..Filter::default() };
            let rows = load(hub, &f, since, grain).await.map_err(internal)?;
            let time = runner_time(hub, since, Some(name)).await.map_err(internal)?.remove(*name).unwrap_or_default();
            let legs = by(&rows, |k| k.is_leg().then(String::new)).remove("");
            let totals = time.iter().fold((0.0, 0.0, 0.0), |a, (_, c, s, b)| (a.0 + c, a.1 + s, a.2 + b));
            let pool_of = pools(hub).await;
            let mut out = runner_json(hub, name, legs.as_ref(), Some(totals), pool_of.get(*name));
            // Utilization by bucket, beside the legs that finished in it.
            let mut buckets: BTreeMap<i64, (f64, f64, f64)> = BTreeMap::new();
            for (b, c, s, busy) in &time {
                let e = buckets.entry(floor(*b, grain)).or_default();
                (e.0, e.1, e.2) = (e.0 + c, e.1 + s, e.2 + busy);
            }
            let legs_by: BTreeMap<i64, Rollup> = rows.iter().filter(|(_, k, _)| k.is_leg()).fold(BTreeMap::new(), |mut m, (b, _, r)| {
                m.entry(*b).or_default().merge(r);
                m
            });
            let points: std::collections::BTreeSet<i64> = buckets.keys().chain(legs_by.keys()).copied().collect();
            let s: Vec<Value> = points
                .into_iter()
                .map(|b| {
                    let (c, slots, busy) = buckets.get(&b).copied().unwrap_or_default();
                    let r = legs_by.get(&b).cloned().unwrap_or_default();
                    summary(&r, json!({ "bucket": b, "connected_minutes": c / 60.0, "slot_minutes": slots / 60.0, "busy_minutes": busy / 60.0, "utilization": (slots > 0.0).then(|| busy / slots) }))
                })
                .collect();
            let names: HashMap<String, String> = hub.store.repos().await.map_err(internal)?.into_iter().map(|r| (r.id, r.name)).collect();
            let repos: Vec<Value> = by(&rows, |k| k.is_leg().then(|| k.repo.clone())).into_iter().map(|(repo, r)| json!({ "id": repo, "name": names.get(&repo), "legs": r.count, "runner_minutes": r.runner_seconds / 60.0, "failure": r.failure })).collect();
            if let Some(o) = out.as_object_mut() {
                o.insert("series".into(), Value::Array(s));
                o.insert("repos".into(), Value::Array(repos));
            }
            Ok(with(out))
        }
        _ => Err((StatusCode::NOT_FOUND, "no such insight".into())),
    }
}

/// A runner's line: its time, utilization, legs and failure classes.
fn runner_json(hub: &Hub, name: &str, legs: Option<&Rollup>, time: Option<(f64, f64, f64)>, pool: Option<&String>) -> Value {
    let (connected, slots, busy) = time.unwrap_or_default();
    let r = legs.cloned().unwrap_or_default();
    let pool = pool.map_or("default", String::as_str);
    let minutes = r.runner_seconds / 60.0;
    json!({
        "runner": name, "pool": pool, "connected_minutes": connected / 60.0, "slot_minutes": slots / 60.0, "busy_minutes": busy / 60.0,
        "utilization": (slots > 0.0).then(|| busy / slots), "legs": r.summary(), "runner_minutes": minutes, "cost": cost(hub, pool, minutes),
    })
}

/// Runners' time by hour since `since`: (bucket, connected, slot-seconds, busy slot-seconds).
async fn runner_time(hub: &Hub, since: i64, runner: Option<&str>) -> anyhow::Result<BTreeMap<String, Vec<(i64, f64, f64, f64)>>> {
    let runner = runner.map(str::to_owned);
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT runner, bucket, connected, slot_seconds, busy_seconds FROM ci_runner_time WHERE bucket >= ?1 AND (?2 IS NULL OR runner = ?2) ORDER BY bucket")?;
            let rows = st.query_map(params![floor(since, HOUR), runner], |x| Ok((x.get::<_, String>(0)?, (x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?))))?;
            let mut out: BTreeMap<String, Vec<(i64, f64, f64, f64)>> = BTreeMap::new();
            for r in rows {
                let (n, t) = r?;
                out.entry(n).or_default().push(t);
            }
            Ok(out)
        })
        .await
}

/// Workflows' names, from their latest runs.
async fn workflow_names(hub: &Hub, repo: &str) -> anyhow::Result<HashMap<String, String>> {
    let repo = repo.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT workflow_id, pipeline FROM runs WHERE repo_id = ?1 AND workflow_id IS NOT NULL AND pipeline != '' ORDER BY created_at")?;
            let rows = st.query_map([repo], |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?)))?;
            Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
        })
        .await
}

/// Hits, misses, saves, bytes and the time hits saved, by key prefix (W08-R13): a job's legs
/// that missed a prefix against those that hit it, by their mean durations, times the hits.
async fn cache_insights(hub: &Hub, repo: &str, workflow: Option<&str>, since: i64) -> anyhow::Result<Vec<Value>> {
    let (repo, workflow) = (repo.to_owned(), workflow.map(str::to_owned));
    let rows: Vec<(String, String, Option<i64>, Option<String>, Option<i64>, Option<i64>)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT e.key, e.kind, e.bytes, j.job_id, j.started_at, j.finished_at FROM ci_cache_events e LEFT JOIN jobs j ON j.id = e.leg_id LEFT JOIN runs r ON r.id = e.run_id
                 WHERE e.repo_id = ?1 AND e.ts >= ?2 AND (?3 IS NULL OR r.workflow_id = ?3)",
            )?;
            let rows = st.query_map(params![repo, since, workflow], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?, x.get(5)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    #[derive(Default)]
    struct P {
        hits: u64,
        misses: u64,
        saves: u64,
        restored: i64,
        saved: i64,
        /// By job: durations of legs that hit, and that missed.
        legs: BTreeMap<String, (Vec<f64>, Vec<f64>)>,
    }
    let mut by: BTreeMap<String, P> = BTreeMap::new();
    for (key, kind, bytes, job, started, finished) in rows {
        let p = by.entry(key_prefix(&key)).or_default();
        let took = started.zip(finished).map(|(s, f)| (f - s).max(0) as f64);
        match kind.as_str() {
            "hit" => {
                p.hits += 1;
                p.restored += bytes.unwrap_or(0);
                if let (Some(j), Some(t)) = (job, took) {
                    p.legs.entry(j).or_default().0.push(t);
                }
            }
            "miss" => {
                p.misses += 1;
                if let (Some(j), Some(t)) = (job, took) {
                    p.legs.entry(j).or_default().1.push(t);
                }
            }
            _ => {
                p.saves += 1;
                p.saved += bytes.unwrap_or(0);
            }
        }
    }
    let mean = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    Ok(by
        .into_iter()
        .map(|(prefix, p)| {
            let lookups = p.hits + p.misses;
            let saved: Option<f64> = p.legs.values().filter_map(|(hit, miss)| Some((mean(miss)? - mean(hit)?).max(0.0) * hit.len() as f64)).reduce(|a, b| a + b);
            json!({ "prefix": prefix, "hits": p.hits, "misses": p.misses, "saves": p.saves, "hit_rate": (lookups > 0).then(|| p.hits as f64 / lookups as f64), "bytes_restored": p.restored, "bytes_saved": p.saved, "seconds_saved": saved })
        })
        .collect())
}

/// The latest finished run's critical path: its jobs and how long they took end to end.
async fn critical(hub: &Hub, repo: &str, workflow: &str) -> anyhow::Result<Value> {
    let (repo, wf) = (repo.to_owned(), workflow.to_owned());
    let run: Option<(String, i64)> = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            Ok(c.query_row("SELECT id, number FROM runs WHERE repo_id = ?1 AND workflow_id = ?2 AND finished_at IS NOT NULL ORDER BY finished_at DESC LIMIT 1", params![repo, wf], |x| Ok((x.get(0)?, x.get(1)?))).optional()?)
        })
        .await?;
    let Some((id, number)) = run else { return Ok(Value::Null) };
    let g = super::runs_api::graph_json(hub, &id, None).await.map_err(|(_, e)| anyhow::anyhow!(e))?;
    let path: Vec<String> = g.get("critical_path").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
    let span = |j: &str| -> i64 {
        g.get("nodes").and_then(Value::as_array).and_then(|n| n.iter().find(|x| x.get("job").and_then(Value::as_str) == Some(j))).map_or(0, |n| {
            let (s, f) = (n.get("started_at").and_then(Value::as_i64), n.get("finished_at").and_then(Value::as_i64));
            s.zip(f).map_or(0, |(s, f)| (f - s).max(0))
        })
    };
    let seconds: i64 = path.iter().map(|j| span(j)).sum();
    Ok(json!({ "run": id, "number": number, "jobs": path, "seconds": seconds }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w08_r13_prefixes() {
        assert_eq!(key_prefix("Linux-cargo-3f9a1c2b7d"), "Linux-cargo");
        assert_eq!(key_prefix("node-cache-Linux-x64-npm-0123abcd0123abcd"), "node-cache-Linux-x64-npm");
        assert_eq!(key_prefix("setup-go-Linux-ubuntu24-go-1.23.4-deadbeefcafe"), "setup-go-Linux-ubuntu24-go-1.23.4");
        assert_eq!(key_prefix("bench-2026-10-18"), "bench");
        assert_eq!(key_prefix("plain"), "plain");
        assert_eq!(key_prefix("12345678"), "12345678", "a key that is all hash keeps itself");
    }

    #[test]
    fn w19_r02_buckets() {
        assert_eq!(floor(7_205, HOUR), 7_200);
        assert_eq!(floor(-5, HOUR), -3_600);
        assert_eq!(floor(90_000, DAY), 86_400);
        let f = Filter { repo: Some("r1".into()), ..Filter::default() };
        assert!(f.takes(&Key { repo: "r1".into(), ..Key::default() }) && !f.takes(&Key { repo: "r2".into(), ..Key::default() }));
        assert!(Key { job: "build".into(), ..Key::default() }.is_leg() && Key::default().is_run() && Key { job: "b".into(), step: "s".into(), ..Key::default() }.is_step());
    }
}
