//! The CI engine's runs in the store (docs/ci.md §2.4, W04 design): a `runs` row with its plan's
//! inputs and event, one `jobs` row per leg with its steps, annotations and summaries, and the
//! plan's log: every report the plan took, in order, so a run is rebuilt exactly by replay after a
//! restart (the plan is pure). v1 code reads none of these rows (they have a `workflow_id` or a
//! `job_id`).

use kiln_ci::plan::LegSpec;
use rusqlite::{OptionalExtension, params};

use crate::hub::Hub;
use crate::store::{Repo, Run, Status};

/// A run about to be created.
#[derive(Debug, Clone, Default)]
pub struct NewRun {
    pub id: String,
    /// The workflow's file (`.github/workflows/ci.yml`).
    pub workflow: String,
    /// What the run list shows: the workflow's name.
    pub name: String,
    pub event: String,
    pub branch: String,
    pub sha: String,
    pub message: String,
    pub author: String,
    pub actor: String,
    pub trigger: serde_json::Value,
    pub trusted: bool,
    pub priority: u8,
    pub payload: serde_json::Value,
    /// The model and the plan's inputs (JSON), for replay.
    pub model: String,
    pub input: String,
}

/// Creates the run's row, its event and its plan's inputs; returns its number and its numeric id
/// (`github.run_id`).
pub async fn create(hub: &Hub, repo: &Repo, n: NewRun) -> anyhow::Result<(i64, i64)> {
    let repo_id = repo.id.clone();
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            let number: i64 = tx.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM runs WHERE repo_id = ?1", [&repo_id], |r| r.get(0))?;
            tx.execute(
                "INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at, workflow_id, actor, trigger, trust, priority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10, ?11, ?12, ?13, ?14, ?15)",
                params![n.id, repo_id, number, n.name, n.event, n.branch, n.sha, n.message, n.author, now, n.workflow, n.actor, n.trigger.to_string(), if n.trusted { "trusted" } else { "untrusted" }, n.priority],
            )?;
            let rowid: i64 = tx.query_row("SELECT rowid FROM runs WHERE id = ?1", [&n.id], |r| r.get(0))?;
            tx.execute("INSERT INTO ci_events (run_id, payload) VALUES (?1, ?2)", params![n.id, n.payload.to_string()])?;
            tx.execute("INSERT INTO ci_plans (run_id, workflow, input) VALUES (?1, ?2, ?3)", params![n.id, n.model, n.input])?;
            tx.commit()?;
            Ok((number, rowid))
        })
        .await
}

/// `run-name`, once the plan evaluated it.
pub async fn set_run_name(hub: &Hub, run: &str, name: Option<String>) -> anyhow::Result<()> {
    let run = run.to_owned();
    hub.store.call(move |c| Ok(c.execute("UPDATE runs SET run_name = ?2 WHERE id = ?1", params![run, name]).map(|_| ())?)).await
}

/// A run's approval (W18-R06): `required`, then `approved` or `rejected` by `by`.
pub async fn set_approval(hub: &Hub, run: &str, state: &str, by: Option<&str>) -> anyhow::Result<()> {
    let (run, state, by) = (run.to_owned(), state.to_owned(), by.map(str::to_owned));
    hub.store
        .call(move |c| {
            let at = by.as_ref().map(|_| crate::util::now_unix());
            Ok(c.execute("UPDATE runs SET approval = ?2, approved_by = ?3, approved_at = ?4 WHERE id = ?1", params![run, state, by, at]).map(|_| ())?)
        })
        .await
}

/// A run's approval, when it needed one.
pub async fn approval(hub: &Hub, run: &str) -> anyhow::Result<Option<String>> {
    let run = run.to_owned();
    hub.store.call(move |c| Ok(c.query_row("SELECT approval FROM runs WHERE id = ?1", [&run], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())).await
}

/// The v1-shaped job spec the run pages read, for a leg.
fn planned_json(spec: &LegSpec) -> serde_json::Value {
    let steps: Vec<serde_json::Value> = spec
        .def()
        .map(|j| {
            j.steps
                .iter()
                .map(|s| serde_json::json!({ "name": step_name(s), "run": s.run.as_ref().map(|r| r.src.clone()).or_else(|| s.uses.clone()).unwrap_or_default() }))
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "key": spec.key, "spec_id": spec.job, "display": spec.name, "needs": [], "labels": spec.runs_on.labels,
        "image": spec.container, "toolchain": null, "env": {}, "timeout_minutes": spec.timeout_minutes, "retries": 0,
        "exclusive": false, "resources": {}, "artifacts": [], "cache": [], "condition": null, "steps": steps,
    })
}

/// A step's name before the runner evaluates it: its `name`, or what GitHub shows ("Run …").
pub fn step_name(s: &kiln_ci::wf::ir::Step) -> String {
    match (&s.name, &s.run, &s.uses) {
        (Some(n), _, _) => n.src.clone(),
        (None, Some(r), _) => format!("Run {}", r.src.lines().next().unwrap_or_default().trim()),
        (None, None, Some(u)) => format!("Run {u}"),
        _ => "Step".into(),
    }
}

/// A leg's row and its steps; `status` is `queued` for a leg the scheduler gets, or where the plan
/// left it (skipped, failed, cancelled), with `error` saying why.
pub async fn insert_leg(hub: &Hub, run: &str, id: &str, spec: &LegSpec, status: Status, error: Option<String>) -> anyhow::Result<()> {
    let (run, id) = (run.to_owned(), id.to_owned());
    let labels = serde_json::to_string(&spec.runs_on.labels).unwrap_or_else(|_| "[]".into());
    let planned = planned_json(spec).to_string();
    let leg = serde_json::to_string(spec).unwrap_or_default();
    let (key, display, job, matrix) = (spec.key.clone(), spec.name.clone(), spec.job.clone(), spec.matrix.to_string());
    let index = spec.strategy.get("job-index").and_then(serde_json::Value::as_f64).map(|v| v as i64);
    let group = spec.concurrency.as_ref().map(|g| g.group.clone());
    let steps: Vec<String> = spec.def().map(|j| j.steps.iter().map(step_name).collect()).unwrap_or_default();
    // The jobs it needs, as their full ids: a called workflow's jobs need their siblings.
    let prefix = spec.job.rsplit_once('/').map(|(p, _)| format!("{p}/")).unwrap_or_default();
    let needed: Vec<String> = spec.needs.as_object().map(|o| o.keys().map(|k| format!("{prefix}{k}")).collect()).unwrap_or_default();
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            let terminal = status.is_terminal();
            // The legs of those jobs, by key, which is what the run's graph links (W21).
            let mut needs: Vec<String> = Vec::new();
            for n in &needed {
                let mut q = tx.prepare("SELECT key FROM jobs WHERE run_id = ?1 AND job_id = ?2 AND run_attempt = (SELECT attempt FROM runs WHERE id = ?1) ORDER BY rowid")?;
                for k in q.query_map(params![run, n], |r| r.get::<_, String>(0))? {
                    needs.push(k?);
                }
            }
            let needs_json = serde_json::to_string(&needs).unwrap_or_else(|_| "[]".into());
            let mut planned: serde_json::Value = serde_json::from_str(&planned).unwrap_or_default();
            if let Some(o) = planned.as_object_mut() {
                o.insert("needs".into(), serde_json::json!(needs));
            }
            tx.execute(
                "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, attempt, queued_at, finished_at, error, job_id, matrix, leg_index, leg, concurrency_group, run_attempt)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?16, ?7, 1, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, (SELECT attempt FROM runs WHERE id = ?2))",
                params![id, run, key, display, status.as_str(), labels, planned.to_string(), (!terminal).then_some(now), terminal.then_some(now), error, job, matrix, index, leg, group, needs_json],
            )?;
            for (i, name) in steps.iter().enumerate() {
                let st = if terminal { "skipped" } else { "pending" };
                tx.execute("INSERT INTO steps (job_id, idx, name, status) VALUES (?1, ?2, ?3, ?4)", params![id, i64::try_from(i).unwrap_or(0), name, st])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

/// The reason a leg waits, by its kind (`capacity`, `concurrency`, …); `scheduling` without one.
fn wait_kind(reason: Option<&str>) -> String {
    reason.and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok()).and_then(|v| v.get("reason").and_then(serde_json::Value::as_str).map(str::to_owned)).unwrap_or_else(|| "scheduling".into())
}

/// Ends a waiting leg's current wait (W19-R01): the seconds since it began go to its reason's
/// total in `jobs.waits`, and the next wait begins now.
fn close_wait(c: &rusqlite::Connection, leg: &str, now: i64) -> anyhow::Result<()> {
    let row: Option<(Option<String>, Option<i64>, Option<String>)> =
        c.query_row("SELECT wait_reason, COALESCE(wait_since, queued_at), waits FROM jobs WHERE id = ?1 AND status IN ('queued', 'pending', 'waiting')", [leg], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let Some((reason, Some(since), waits)) = row else { return Ok(()) };
    let mut w: std::collections::BTreeMap<String, f64> = waits.and_then(|w| serde_json::from_str(&w).ok()).unwrap_or_default();
    let (kind, secs) = (wait_kind(reason.as_deref()), (now - since).max(0));
    if secs > 0 {
        crate::metrics::global().ci_wait_seconds.add(&[&kind], secs.unsigned_abs());
        *w.entry(kind).or_default() += secs as f64;
    }
    c.execute("UPDATE jobs SET waits = ?2, wait_since = ?3 WHERE id = ?1", params![leg, serde_json::to_string(&w)?, now])?;
    Ok(())
}

/// A leg the scheduler gave a runner.
pub async fn claim(hub: &Hub, leg: &str, agent: &str, executor: &str, attempt: u32) -> anyhow::Result<()> {
    let (leg, agent, executor) = (leg.to_owned(), agent.to_owned(), executor.to_owned());
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            close_wait(&tx, &leg, now)?;
            // The attempt (W19-R01); one before it that never reported ended with its runner.
            tx.execute("UPDATE ci_attempts SET status = 'done', conclusion = 'failure', failure_class = 'runner-lost', finished_at = ?3 WHERE leg_id = ?1 AND number < ?2 AND status = 'running'", params![leg, attempt, now])?;
            tx.execute(
                "INSERT INTO ci_attempts (id, leg_id, number, status, runner, executor, queued_at, started_at) VALUES (?1, ?2, ?3, 'running', ?4, ?5, (SELECT queued_at FROM jobs WHERE id = ?2), ?6)
                 ON CONFLICT (leg_id, number) DO UPDATE SET status = 'running', runner = excluded.runner, executor = excluded.executor, queued_at = excluded.queued_at, started_at = excluded.started_at,
                   conclusion = NULL, failure_class = NULL, finished_at = NULL, exit_code = NULL, error = NULL",
                params![crate::util::new_id(), leg, attempt, agent, executor, now],
            )?;
            tx.execute(
                "UPDATE jobs SET status = 'running', agent = ?2, executor = ?3, attempt = ?4, started_at = ?5, finished_at = NULL, exit_code = NULL, error = NULL, wait_reason = NULL, wait_since = NULL WHERE id = ?1",
                params![leg, agent, executor, attempt, now],
            )?;
            tx.execute(
                "UPDATE runs SET status = 'running', started_at = COALESCE(started_at, ?2) WHERE id = (SELECT run_id FROM jobs WHERE id = ?1) AND status IN ('queued', 'pending')",
                params![leg, now],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// How an attempt ended (W19-R01): its conclusion and, when it failed, why.
pub struct AttemptEnd {
    pub conclusion: &'static str,
    pub class: Option<&'static str>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub peak_rss: Option<u64>,
    pub cpu_seconds: Option<f64>,
}

pub async fn attempt_finished(hub: &Hub, leg: &str, attempt: u32, end: AttemptEnd) -> anyhow::Result<()> {
    let leg = leg.to_owned();
    crate::metrics::global().ci_attempts.inc(&[end.conclusion, end.class.unwrap_or("")]);
    let rss = end.peak_rss.and_then(|r| i64::try_from(r).ok());
    hub.store
        .call(move |c| {
            c.execute(
                "UPDATE ci_attempts SET status = 'done', conclusion = ?3, failure_class = ?4, exit_code = ?5, error = ?6, peak_rss = ?7, cpu_seconds = ?8, finished_at = ?9 WHERE leg_id = ?1 AND number = ?2",
                params![leg, attempt, end.conclusion, end.class, end.exit_code, end.error, rss, end.cpu_seconds, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
}

/// A leg waiting again: a retry, or a runner that refused it.
pub async fn requeue(hub: &Hub, leg: &str, attempt: u32, note: Option<String>) -> anyhow::Result<()> {
    let leg = leg.to_owned();
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute(
                "UPDATE jobs SET status = 'queued', agent = NULL, executor = NULL, attempt = ?2, queued_at = ?3, started_at = NULL, error = ?4, wait_since = ?3 WHERE id = ?1",
                params![leg, attempt, now, note],
            )?;
            c.execute("UPDATE steps SET status = 'pending', started_at = NULL, finished_at = NULL, exit_code = NULL WHERE job_id = ?1", [&leg])?;
            Ok(())
        })
        .await
}

/// The legs kept now: each with its runner and until when.
pub async fn kept_legs(hub: &Hub) -> anyhow::Result<Vec<(String, String, i64)>> {
    hub.store
        .call(|c| {
            let mut st = c.prepare("SELECT id, agent, kept_until FROM jobs WHERE kept_until > ?1 AND agent IS NOT NULL")?;
            let rows = st.query_map([crate::util::now_unix()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// A failed leg kept until `until` (W17-R02), or no longer kept.
pub async fn kept(hub: &Hub, leg: &str, until: Option<i64>) -> anyhow::Result<()> {
    let leg = leg.to_owned();
    hub.store.call(move |c| Ok(c.execute("UPDATE jobs SET kept_until = ?2 WHERE id = ?1", params![leg, until]).map(|_| ())?)).await
}

/// When a leg was queued (Unix seconds).
pub async fn queued_at(hub: &Hub, leg: &str) -> anyhow::Result<i64> {
    let leg = leg.to_owned();
    hub.store.call(move |c| Ok(c.query_row("SELECT COALESCE(queued_at, 0) FROM jobs WHERE id = ?1", [&leg], |r| r.get::<_, i64>(0))?)).await
}

/// Why a leg waits now (W05-R13; `null`: no longer for that). A new kind of reason ends the
/// wait for the last one (W19-R01).
pub async fn wait_reason(hub: &Hub, leg: &str, reason: &serde_json::Value) -> anyhow::Result<()> {
    let (leg, r) = (leg.to_owned(), (!reason.is_null()).then(|| reason.to_string()));
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            let was: Option<Option<String>> = tx.query_row("SELECT wait_reason FROM jobs WHERE id = ?1", [&leg], |x| x.get(0)).optional()?;
            if was.as_ref().is_some_and(|w| wait_kind(w.as_deref()) != wait_kind(r.as_deref())) {
                close_wait(&tx, &leg, crate::util::now_unix())?;
            }
            tx.execute("UPDATE jobs SET wait_reason = ?2 WHERE id = ?1", params![leg, r])?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// A leg done for good.
pub async fn finish_leg(hub: &Hub, leg: &str, status: Status, conclusion: &str, outputs: Option<String>, error: Option<String>) -> anyhow::Result<()> {
    let (leg, conclusion) = (leg.to_owned(), conclusion.to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            // One that never started waited until now; an attempt still open ends with it.
            close_wait(c, &leg, now)?;
            c.execute("UPDATE ci_attempts SET status = 'done', conclusion = ?2, finished_at = ?3 WHERE leg_id = ?1 AND status = 'running'", params![leg, conclusion, now])?;
            c.execute(
                "UPDATE jobs SET status = ?2, conclusion = ?3, outputs = COALESCE(?4, outputs), error = COALESCE(?5, error), finished_at = ?6 WHERE id = ?1",
                params![leg, status.as_str(), conclusion, outputs, error, now],
            )?;
            c.execute("UPDATE steps SET status = 'skipped' WHERE job_id = ?1 AND status IN ('pending', 'running')", [&leg])?;
            Ok(())
        })
        .await?;
    // Its logs go into the search index (W19-R04).
    super::logsearch::kick();
    Ok(())
}

/// A step started on the runner: its evaluated name replaces the planned one (or the step is new:
/// a post step).
pub async fn step_started(hub: &Hub, leg: &str, idx: u32, name: &str, kind: &str, action: Option<String>) -> anyhow::Result<()> {
    let (leg, name, kind) = (leg.to_owned(), name.to_owned(), kind.to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute(
                "INSERT INTO steps (job_id, idx, name, status, started_at, kind, action, started_seq)
                 VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6, (SELECT COALESCE(MAX(started_seq), 0) + 1 FROM steps WHERE job_id = ?1))
                 ON CONFLICT (job_id, idx) DO UPDATE SET name = ?3, status = 'running', started_at = ?4, kind = ?5, action = ?6, started_seq = excluded.started_seq",
                params![leg, idx, name, now, kind, action],
            )?;
            Ok(())
        })
        .await
}

#[allow(clippy::too_many_arguments)]
pub async fn step_finished(hub: &Hub, leg: &str, idx: u32, status: Status, outcome: &str, conclusion: &str, exit_code: Option<i32>, outputs: String) -> anyhow::Result<()> {
    let (leg, outcome, conclusion) = (leg.to_owned(), outcome.to_owned(), conclusion.to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute(
                "UPDATE steps SET status = ?3, outcome = ?4, conclusion = ?5, exit_code = ?6, outputs = ?7, finished_at = ?8, started_at = COALESCE(started_at, ?8) WHERE job_id = ?1 AND idx = ?2",
                params![leg, idx, status.as_str(), outcome, conclusion, exit_code, outputs, now],
            )?;
            Ok(())
        })
        .await
}

#[allow(clippy::too_many_arguments)]
pub async fn annotation(hub: &Hub, leg: &str, idx: Option<u32>, level: &str, title: Option<String>, message: &str, file: Option<String>, line: Option<u32>, end_line: Option<u32>, col: Option<u32>, end_col: Option<u32>) -> anyhow::Result<()> {
    let (leg, level, message) = (leg.to_owned(), level.to_owned(), message.to_owned());
    hub.store
        .call(move |c| {
            // At most 1,000 a leg (W06-R16).
            let n: i64 = c.query_row("SELECT COUNT(*) FROM ci_annotations WHERE leg_id = ?1", [&leg], |r| r.get(0))?;
            if n < 1_000 {
                c.execute(
                    "INSERT INTO ci_annotations (leg_id, step_idx, level, title, message, file, line, end_line, col, end_col) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![leg, idx, level, title, message, file, line, end_line, col, end_col],
                )?;
            }
            Ok(())
        })
        .await
}

pub async fn summary(hub: &Hub, leg: &str, idx: u32, markdown: &str) -> anyhow::Result<()> {
    let (leg, md) = (leg.to_owned(), markdown.to_owned());
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO ci_summaries (leg_id, step_idx, markdown) VALUES (?1, ?2, ?3) ON CONFLICT (leg_id, step_idx) DO UPDATE SET markdown = ?3",
                params![leg, idx, md],
            )?;
            Ok(())
        })
        .await
}

/// The run's end.
pub async fn finish_run(hub: &Hub, run: &str, status: Status, conclusion: &str, error: Option<String>) -> anyhow::Result<()> {
    let (run, conclusion) = (run.to_owned(), conclusion.to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute(
                "UPDATE runs SET status = ?2, conclusion = ?3, error = COALESCE(?4, error), finished_at = ?5, started_at = COALESCE(started_at, ?5) WHERE id = ?1",
                params![run, status.as_str(), conclusion, error, now],
            )?;
            c.execute("UPDATE ci_plans SET done = 1 WHERE run_id = ?1", [&run])?;
            Ok(())
        })
        .await
}

/// One report the plan took (W05-R25), in the run's current attempt.
pub async fn log(hub: &Hub, run: &str, seq: i64, kind: &str, leg: Option<String>, report: Option<String>) -> anyhow::Result<()> {
    let (run, kind) = (run.to_owned(), kind.to_owned());
    hub.store
        .call(move |c| Ok(c.execute("INSERT INTO ci_plan_log (run_id, seq, kind, leg, report, attempt) VALUES (?1, ?2, ?3, ?4, ?5, (SELECT attempt FROM runs WHERE id = ?1))", params![run, seq, kind, leg, report]).map(|_| ())?))
        .await
}

/// A new attempt of a finished run (W05-R17), started by `by`: its number, the run queued again.
pub async fn start_attempt(hub: &Hub, run: &str, by: &str) -> anyhow::Result<u32> {
    let (run, by) = (run.to_owned(), by.to_owned());
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute("UPDATE runs SET attempt = attempt + 1, status = 'queued', conclusion = NULL, error = NULL, started_at = NULL, finished_at = NULL WHERE id = ?1", [&run])?;
            tx.execute("UPDATE ci_plans SET done = 0 WHERE run_id = ?1", [&run])?;
            let n: u32 = tx.query_row("SELECT attempt FROM runs WHERE id = ?1", [&run], |r| r.get(0))?;
            tx.execute("INSERT OR REPLACE INTO ci_run_attempts (run_id, attempt, actor, created_at) VALUES (?1, ?2, ?3, ?4)", params![run, n, by, crate::util::now_unix()])?;
            tx.commit()?;
            Ok(n)
        })
        .await
}

/// A leg the new attempt doesn't run again (W05-R17): a copy of the last attempt's, with its
/// steps and result, `reused_from` naming it.
pub async fn keep_leg(hub: &Hub, run: &str, old: &str, id: &str) -> anyhow::Result<()> {
    let (run, old, id) = (run.to_owned(), old.to_owned(), id.to_owned());
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, agent, executor, attempt, queued_at, started_at, finished_at, exit_code, error,
                                   job_id, matrix, leg_index, outputs, conclusion, requirements, routing, concurrency_group, priority, input_hash, reused_from, environment, leg, run_attempt)
                 SELECT ?1, run_id, key, display, status, labels, needs, spec, agent, executor, attempt, queued_at, started_at, finished_at, exit_code, error,
                        job_id, matrix, leg_index, outputs, conclusion, requirements, routing, concurrency_group, priority, input_hash, COALESCE(reused_from, id), environment, leg,
                        (SELECT attempt FROM runs WHERE id = ?3)
                 FROM jobs WHERE id = ?2",
                params![id, old, run],
            )?;
            tx.execute(
                "INSERT INTO steps (job_id, idx, name, status, started_at, finished_at, exit_code, kind, step_id, action, outcome, conclusion, outputs, attempt)
                 SELECT ?1, idx, name, status, started_at, finished_at, exit_code, kind, step_id, action, outcome, conclusion, outputs, attempt FROM steps WHERE job_id = ?2",
                params![id, old],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// The run of a leg.
pub async fn run_of(hub: &Hub, leg: &str) -> anyhow::Result<Option<Run>> {
    let leg = leg.to_owned();
    let run: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT run_id FROM jobs WHERE id = ?1", [&leg], |r| r.get(0)).optional()?)).await?;
    match run {
        Some(r) => hub.store.run(r).await,
        None => Ok(None),
    }
}
