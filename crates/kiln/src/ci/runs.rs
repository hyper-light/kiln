//! The CI engine's runs in the store (docs/ci.md §2.4, W04 design): a `runs` row with its plan's
//! inputs and event, one `jobs` row per leg with its steps, annotations and summaries, and the
//! plan's log: every report the plan took, in order, so a run is rebuilt exactly by replay after a
//! restart (the plan is pure). v1 code reads none of these rows (they have a `workflow_id` or a
//! `job_id`).

use kiln_ci::plan::LegSpec;
use crate::sql::{OptionalExtension, params};

use crate::hub::Hub;
use crate::store::{Repo, Run, Status};

/// U03: called after the hub authenticates the reporting process, before it sends Received.
/// The first report for this owner and attempt wins, including across retries/reconnects.
/// Legacy attempt numbers are normalized before import, so a result arriving during migration
/// has the same identity after the workflow engine takes ownership.
pub(crate) async fn stage_completion(hub: &Hub, agent: &str, result: &crate::proto::JobResult, expected_attempt: Option<u32>, origin: Option<&crate::hub::CompletionOrigin>) -> anyhow::Result<Option<(String, u32)>> {
    anyhow::ensure!(result.status.is_terminal(), "a job completion must have a terminal status");
    let (agent, job, payload) = (agent.to_owned(), result.job.clone(), serde_json::to_string(result)?);
    // U05: preserve the exact wire result outside the shared SQL writer. A JSON
    // reconstruction cannot substitute for the original producer's result digest.
    let wire=crate::proto::encode(result)?;
    let nonce=origin.map(|proof|proof.process_nonce().to_owned());
    let received=hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        // The admission lookup above the await is advisory. Recheck against the SAME
        // durable binding before first-result mutation or positive retired receipt.
        anyhow::ensure!(completion_report_allowed(&tx,&job,&agent,expected_attempt,nonce.as_deref(),&wire)?,"required original completion binding changed before its own SQL acceptance; channel input must retry");
        let row: Option<(Option<String>, String, u32, bool)> = tx.query_row("SELECT agent, status, attempt, job_id IS NULL FROM jobs WHERE id = ?1", [&job], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
        let Some((owner, status, attempt, legacy)) = row else { return Ok(None) };
        if owner.as_deref() != Some(agent.as_str()) { return Ok(None) }
        let attempt = if legacy { attempt.saturating_add(1) } else { attempt.max(1) };
        if expected_attempt.is_some_and(|expected| expected != attempt) { return Ok(None) }
        let lost: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_attempts WHERE leg_id = ?1 AND number = ?2 AND status = 'done' AND failure_class = 'runner-lost')", params![job, attempt], |r| r.get(0))?;
        if lost { return Ok(None) }
        if Status::parse(&status).is_terminal() {
            // Received may itself have been lost after the plan committed and retired its
            // receipt. Acknowledge only that durable owner/attempt; never enqueue it again.
            let committed: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_attempts WHERE leg_id = ?1 AND number = ?2 AND runner = ?3 AND status = 'done')", params![job, attempt, agent], |r| r.get(0))?;
            return Ok(committed.then_some((job, attempt)));
        }
        if status != "running" { return Ok(None) }
        tx.execute("INSERT INTO ci_completion_reports (job, attempt, agent, payload, received_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (job, attempt, agent) DO NOTHING", params![job, attempt, agent, payload, crate::util::now_unix()])?;
        if let Some(binding)=crate::logs::accepted::producer::registered(&tx,&job,attempt,&agent)? {
            // First result retention and its required log dependency share this SAME
            // acceptance. Received means result retained; it never closes the logs.
            if let crate::logs::accepted::producer::Close::Refused(why)=crate::logs::accepted::producer::note_result(&tx,&binding,&wire)? {
                anyhow::bail!("original required completion was refused: {why}");
            }
        }
        tx.commit()?;
        Ok(Some((job, attempt)))
    }).await?;
    if received.is_some() {hub.logs.producer_progress();}
    Ok(received)
}

/// Required logs belong to the original authenticated process, not a host name or
/// Hello running inventory. None preserves legacy contracts; a strict attempt needs
/// an explicit attempt and arrival proof even for a read-only Received replay.
pub(crate) fn completion_report_allowed(c:&crate::sql::Connection,job:&str,agent:&str,expected:Option<u32>,nonce:Option<&str>,wire:&[u8])->anyhow::Result<bool> {
    let row:Option<(u32,String)>=c.query_row("SELECT CASE WHEN job_id IS NULL THEN attempt+1 ELSE MAX(attempt,1) END,status FROM jobs WHERE id=?1",[job],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some(attempt)=expected.or_else(||row.as_ref().map(|r|r.0)) else{return Ok(true)};
    let Some(binding)=crate::logs::accepted::producer::registered(c,job,attempt,agent)? else{return Ok(true)};
    if expected!=Some(binding.owner.attempt) || nonce!=Some(binding.owner.process_nonce.as_str()) {return Ok(false);}
    // Validate the exact declared encoding and first-result identity for BOTH new
    // acceptance and read-only Received replay; host/inventory cannot substitute it.
    crate::proto::job_log::result_hash(wire)?;
    Ok(match crate::logs::accepted::producer::stored_result(c,&binding)? {
        Some(first)=>first==wire,
        None=>!row.is_some_and(|(_,status)|Status::parse(&status).is_terminal()),
    })
}

fn completion_in(c:&crate::sql::Connection,job:&str,attempt:u32,agent:&str)->anyhow::Result<Option<crate::proto::JobResult>> {
    if let Some(binding)=crate::logs::accepted::producer::registered(c,job,attempt,agent)? {
        let bytes=crate::logs::accepted::producer::stored_result(c,&binding)?;
        return bytes.map(|bytes|crate::proto::read_whole(&bytes).map_err(|error|anyhow::anyhow!("original producer result: {}",error.why("JobResult")))).transpose();
    }
    let payload:Option<String>=c.query_row("SELECT payload FROM ci_completion_reports WHERE job = ?1 AND attempt = ?2 AND agent = ?3",params![job,attempt,agent],|r|r.get(0)).optional()?;
    payload.map(|payload|serde_json::from_str(&payload).map_err(anyhow::Error::from)).transpose()
}

pub(crate) async fn completion(hub: &Hub, job: &str, attempt: u32, agent: &str) -> anyhow::Result<Option<crate::proto::JobResult>> {
    let (job,agent)=(job.to_owned(),agent.to_owned());
    hub.store.call(move |c|completion_in(c,&job,attempt,&agent)).await
}

/// U05: readonly Received replay validates the original exact report in the SAME
/// callback as its positive ledger/retired receipt lookup. It stages no work.
pub(crate) fn completion_report_received(c:&crate::sql::Connection,job:&str,agent:&str,attempt:u32,nonce:Option<&str>,wire:&[u8])->anyhow::Result<bool> {
    anyhow::ensure!(completion_report_allowed(c,job,agent,Some(attempt),nonce,wire)?,"required original replay differs from its durable report or authenticated process; channel input must retry");
    if completion_in(c,job,attempt,agent)?.is_some() {return Ok(true)}
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM jobs j JOIN ci_attempts a ON a.leg_id = j.id WHERE j.id = ?1 AND j.agent = ?3 AND a.runner = ?3 AND a.number = ?2 AND (CASE WHEN j.job_id IS NULL THEN j.attempt + 1 ELSE MAX(j.attempt, 1) END) = ?2 AND j.status IN ('succeeded', 'failed', 'cancelled', 'skipped') AND a.status = 'done' AND COALESCE(a.failure_class, '') != 'runner-lost')",params![job,attempt,agent],|r|r.get::<_,bool>(0))?)
}

/// U05: the result ledger and producer closure are separate receipts. No-contract
/// legacy work is unchanged; a required contract never becomes legacy on missing data.
pub(crate) fn completion_logs_ready(c:&crate::sql::Connection,job:&str,attempt:u32,agent:&str)->anyhow::Result<bool> {
    let Some(binding)=crate::logs::accepted::producer::registered(c,job,attempt,agent)? else{return Ok(true)};
    let Some(bytes)=crate::logs::accepted::producer::stored_result(c,&binding)? else{return Ok(false)};
    Ok(matches!(crate::logs::accepted::producer::close(c,&binding,&bytes)?,crate::logs::accepted::producer::Close::Closed(receipt) if matches!(receipt.view,crate::proto::JobLogViewV1::Captured(_))))
}

/// Rechecked inside the final status/effect transaction, including replay. A prior
/// dispatcher check cannot authorize a changed attempt or a missing original receipt.
fn require_completion_logs(c:&crate::sql::Connection,job:&str)->anyhow::Result<()> {
    let row:Option<(u32,Option<String>)>=c.query_row("SELECT attempt,agent FROM jobs WHERE id=?1",[job],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((attempt,Some(agent)))=row {
        anyhow::ensure!(completion_logs_ready(c,job,attempt,&agent)?,"required original producer logs have no own complete captured receipt; terminal effects remain pending");
    }
    Ok(())
}

pub(crate) async fn pending_completions(hub: &Hub) -> anyhow::Result<Vec<(String, u32, String)>> {
    hub.store.call(|c| {
        let mut query = c.prepare("SELECT job, attempt, agent FROM ci_completion_reports ORDER BY received_at, job, attempt, agent LIMIT 128")?;
        Ok(query.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?)
    }).await
}

/// Forget the receipt only after durable job state contains its outcome or has moved to a
/// newer owner/attempt. A failed write leaves the receipt available for recovery.
pub(crate) async fn retire_completion(hub: &Hub, job: &str, attempt: u32, agent: &str) -> anyhow::Result<bool> {
    let (job, agent) = (job.to_owned(), agent.to_owned());
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let row: Option<(String, u32, Option<String>, bool)> = tx.query_row("SELECT status, attempt, agent, job_id IS NULL FROM jobs WHERE id = ?1", [&job], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
        let done = row.is_none_or(|(status, now, owner, legacy)| Status::parse(&status).is_terminal() || (if legacy { now.saturating_add(1) } else { now.max(1) }) > attempt || owner.as_deref() != Some(agent.as_str()));
        if done {
            tx.execute("DELETE FROM ci_completion_reports WHERE job = ?1 AND attempt = ?2 AND agent = ?3", params![job, attempt, agent])?;
        }
        tx.commit()?;
        Ok(done)
    }).await
}

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
    create_trigger(hub, repo, n, None).await
}

/// U04: materialization may repeat after a crash; only the persisted occurrence may reuse
/// its own deterministic run id. Random/manual run creation retains collision refusal.
pub(crate) async fn create_trigger(hub: &Hub, repo: &Repo, n: NewRun, trigger: Option<String>) -> anyhow::Result<(i64, i64)> {
    let repo_id = repo.id.clone();
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            if let Some(trigger) = &trigger {
                let owned = super::schedule::outbox::validate(&tx, trigger, &repo_id, &n.workflow, &n.event, &n.sha, &n.model)?;
                anyhow::ensure!(owned == n.id, "trigger {trigger} does not own run {}", n.id);
                let existing: Option<(i64, i64, String, String, String, String, String)> = tx.query_row("SELECT r.number, r.rowid, r.repo_id, r.workflow_id, r.event, r.sha, p.workflow FROM runs r JOIN ci_plans p ON p.run_id = r.id WHERE r.id = ?1", [&n.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).optional()?;
                if let Some((number, rowid, repo, workflow, event, sha, model)) = existing {
                    anyhow::ensure!(repo == repo_id && workflow == n.workflow && event == n.event && sha == n.sha && model == n.model, "trigger {trigger} collides with unrelated run metadata");
                    return Ok((number, rowid));
                }
            }
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
            let tx = crate::store::tx(c)?;
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
            super::logsearch::stage_finished(&tx,&id)?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// U01: a dependency released an imported pending job. Its id, logs and prior attempts stay
/// with it; only the shared planner may make it runnable now.
pub async fn activate_imported(hub: &Hub, id: &str, spec: &LegSpec) -> anyhow::Result<u32> {
    let (id, leg, job) = (id.to_owned(), serde_json::to_string(spec)?, spec.job.clone());
    hub.store.call(move |c| {
        let n = c.execute("UPDATE jobs SET status = 'queued', leg = ?2, job_id = ?3, queued_at = COALESCE(queued_at, ?4) WHERE id = ?1 AND status = 'pending'", params![id, leg, job, crate::util::now_unix()])?;
        anyhow::ensure!(n == 1, "imported job {id} is no longer pending");
        Ok(c.query_row("SELECT attempt FROM jobs WHERE id = ?1", [&id], |r| r.get(0))?)
    }).await
}

/// The reason a leg waits, by its kind (`capacity`, `concurrency`, …); `scheduling` without one.
fn wait_kind(reason: Option<&str>) -> String {
    reason.and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok()).and_then(|v| v.get("reason").and_then(serde_json::Value::as_str).map(str::to_owned)).unwrap_or_else(|| "scheduling".into())
}

/// Ends a waiting leg's current wait (W19-R01): the seconds since it began go to its reason's
/// total in `jobs.waits`, and the next wait begins now.
fn close_wait(c: &crate::sql::Connection, leg: &str, now: i64) -> anyhow::Result<()> {
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
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        claim_in(&tx, &leg, &agent, &executor, attempt, crate::util::now_unix())?;
        tx.commit()?;
        Ok(())
    }).await
}

/// U01/U03: borrow the existing writer transaction so original ownership and the actual
/// job claim commit together before the prepared command is allowed to become visible.
pub(super) fn claim_in(c: &crate::sql::Connection<'_>, leg: &str, agent: &str, executor: &str, attempt: u32, now: i64) -> anyhow::Result<()> {
    // U03: any canonical assignment ends the imported execution's old-frame permit,
    // even when a rejection/reassignment reuses its attempt number on the same host.
    let saved: Option<(String, String)> = c.query_row("SELECT p.run_id, p.input FROM ci_plans p JOIN jobs j ON j.run_id = p.run_id WHERE j.id = ?1", [leg], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    if let Some((run, saved)) = saved {
        let mut input: serde_json::Value = serde_json::from_str(&saved)?;
        if input.get_mut("legacy_running").and_then(serde_json::Value::as_object_mut).is_some_and(|running| running.remove(leg).is_some()) {
            c.execute("UPDATE ci_plans SET input = ?2 WHERE run_id = ?1", params![run, input.to_string()])?;
        }
    }
    close_wait(c, leg, now)?;
    // The attempt (W19-R01); one before it that never reported ended with its runner.
    c.execute("UPDATE ci_attempts SET status = 'done', conclusion = 'failure', failure_class = 'runner-lost', finished_at = ?3 WHERE leg_id = ?1 AND number < ?2 AND status = 'running'", params![leg, attempt, now])?;
    c.execute(
        "INSERT INTO ci_attempts (id, leg_id, number, status, runner, executor, queued_at, started_at) VALUES (?1, ?2, ?3, 'running', ?4, ?5, (SELECT queued_at FROM jobs WHERE id = ?2), ?6)
         ON CONFLICT (leg_id, number) DO UPDATE SET status = 'running', runner = excluded.runner, executor = excluded.executor, queued_at = excluded.queued_at, started_at = excluded.started_at,
           conclusion = NULL, failure_class = NULL, finished_at = NULL, exit_code = NULL, error = NULL",
        params![crate::util::new_id(), leg, attempt, agent, executor, now],
    )?;
    c.execute(
        "UPDATE jobs SET status = 'running', agent = ?2, executor = ?3, attempt = ?4, started_at = ?5, finished_at = NULL, exit_code = NULL, error = NULL, wait_reason = NULL, wait_since = NULL WHERE id = ?1",
        params![leg, agent, executor, attempt, now],
    )?;
    c.execute(
        "UPDATE runs SET status = 'running', started_at = COALESCE(started_at, ?2) WHERE id = (SELECT run_id FROM jobs WHERE id = ?1) AND status IN ('queued', 'pending')",
        params![leg, now],
    )?;
    Ok(())
}

/// How an attempt ended (W19-R01): its conclusion and, when it failed, why.
#[derive(Clone)]
pub struct AttemptEnd {
    pub conclusion: &'static str,
    pub class: Option<&'static str>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub peak_rss: Option<u64>,
    pub cpu_seconds: Option<f64>,
}

pub async fn attempt_finished(hub: &Hub, leg: &str, attempt: u32, end: AttemptEnd) -> anyhow::Result<()> {
    attempt_end(hub, leg, attempt, end, false).await.map(|_| ())
}

/// The loss decision races a completion receipt in the same store transaction. Whichever was
/// committed first owns the transition; a reconnect cannot overwrite a result already received.
pub(crate) async fn attempt_lost(hub: &Hub, leg: &str, attempt: u32, end: AttemptEnd) -> anyhow::Result<bool> {
    attempt_end(hub, leg, attempt, end, true).await
}

async fn attempt_end(hub: &Hub, leg: &str, attempt: u32, end: AttemptEnd, losing: bool) -> anyhow::Result<bool> {
    let leg = leg.to_owned();
    let labels = (end.conclusion, end.class);
    let recorded = hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let recorded = attempt_end_in(&tx, &leg, attempt, &end, losing, crate::util::now_unix())?;
        if !recorded { return Ok(false); }
        tx.commit()?;
        Ok(recorded)
    }).await?;
    if recorded { attempt_metric(labels.0, labels.1); }
    Ok(recorded)
}

/// U01/U03: the completion/loss arbitration and physical debt transition share this SAME
/// borrowed writer connection. This helper never commits, opens another Store or emits a
/// success metric before its caller knows the enclosing transaction committed.
pub(super) fn attempt_end_in(c: &crate::sql::Connection<'_>, leg: &str, attempt: u32, end: &AttemptEnd, losing: bool, now: i64) -> anyhow::Result<bool> {
    // Loss/cancel cannot settle/retry this required attempt ahead of its own capture.
    // Keep the existing logical owner; physical proof/capacity release is independent.
    let agent:Option<Option<String>>=c.query_row("SELECT runner FROM ci_attempts WHERE leg_id=?1 AND number=?2",params![leg,attempt],|r|r.get(0)).optional()?;
    if let Some(Some(agent))=agent && !completion_logs_ready(c,leg,attempt,&agent)? {return Ok(false);}
    if losing && c.query_row("SELECT EXISTS(SELECT 1 FROM ci_completion_reports WHERE job = ?1 AND attempt = ?2)", params![leg, attempt], |r| r.get::<_, bool>(0))? { return Ok(false); }
    let rss = end.peak_rss.and_then(|r| i64::try_from(r).ok());
    c.execute("UPDATE ci_attempts SET status = 'done', conclusion = ?3, failure_class = ?4, exit_code = ?5, error = ?6, peak_rss = ?7, cpu_seconds = ?8, finished_at = ?9 WHERE leg_id = ?1 AND number = ?2",
        params![leg, attempt, end.conclusion, end.class, end.exit_code, end.error, rss, end.cpu_seconds, now])?;
    Ok(true)
}

pub(super) fn attempt_metric(conclusion: &'static str, class: Option<&'static str>) { crate::metrics::global().ci_attempts.inc(&[conclusion, class.unwrap_or("")]); }

/// A leg waiting again: a retry, or a runner that refused it.
pub async fn requeue(hub: &Hub, leg: &str, attempt: u32, note: Option<String>) -> anyhow::Result<()> {
    let leg = leg.to_owned();
    hub.store
        .call(move |c| requeue_in(c,&leg,attempt,note.as_deref(),crate::util::now_unix()))
        .await
}

/// Borrow the SAME existing writer transaction when readiness eligibility and requeue
/// must commit together. Public requeue preserves its historical SQL and clock boundary.
pub(super) fn requeue_in(c:&crate::sql::Connection<'_>,leg:&str,attempt:u32,note:Option<&str>,now:i64)->anyhow::Result<()> {
    c.execute(
        "UPDATE jobs SET status = 'queued', agent = NULL, executor = NULL, attempt = ?2, queued_at = ?3, started_at = NULL, error = ?4, wait_since = ?3 WHERE id = ?1",
        params![leg, attempt, now, note],
    )?;
    c.execute("UPDATE steps SET status = 'pending', started_at = NULL, finished_at = NULL, exit_code = NULL WHERE job_id = ?1", [leg])?;
    Ok(())
}

/// The legs kept now: each with its runner and until when.
pub async fn kept_legs(hub: &Hub) -> anyhow::Result<Vec<(String, String, i64)>> {
    hub.store
        .call(|c| {
            let mut st = c.prepare("SELECT id, agent, kept_until FROM jobs WHERE kept_until IS NOT NULL AND agent IS NOT NULL")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// A failed leg kept until `until` (W17-R02), or no longer kept.
pub async fn kept(hub: &Hub, leg: &str, until: Option<i64>) -> anyhow::Result<()> {
    let leg = leg.to_owned();
    hub.store.call(move |c| Ok(c.execute("UPDATE jobs SET kept_until = ?2 WHERE id = ?1", params![leg, until]).map(|_| ())?)).await
}

/// Where a person's retry asked a leg to run (W05-R28), on its row for the API and the UI.
pub async fn set_placement(hub: &Hub, leg: &str, p: &kiln_ci::sched::Placement) -> anyhow::Result<()> {
    let (leg, json) = (leg.to_owned(), serde_json::to_string(p)?);
    hub.store.call(move |c| Ok(c.execute("UPDATE jobs SET placement = ?2 WHERE id = ?1", params![leg, json]).map(|_| ())?)).await
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
            let tx = crate::store::tx(c)?;
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

/// W19-R06/U14: the actual deployment accepted after a leg passed protection, supplied by
/// its dispatcher owner rather than inferred from an environment or historical deployment.
pub(crate) struct LegDeployment {
    pub id: i64,
    pub run: String,
    pub attempt: u32,
    pub environment: String,
}

/// A leg done for good without an actual accepted deployment.
pub async fn finish_leg(hub: &Hub, leg: &str, status: Status, conclusion: &str, outputs: Option<String>, error: Option<String>) -> anyhow::Result<()> {
    finish_leg_with_deployment(hub,leg,status,conclusion,outputs,error,None).await
}

/// W19-R06/U14: completion and its exact deployment control share one acceptance transaction.
pub(crate) async fn finish_leg_with_deployment(hub: &Hub, leg: &str, status: Status, conclusion: &str, outputs: Option<String>, error: Option<String>, deployment: Option<LegDeployment>) -> anyhow::Result<()> {
    let leg_id = leg.to_owned();
    let (leg, conclusion) = (leg.to_owned(), conclusion.to_owned());
    let base = hub.cfg.server.public_url.clone();
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let c = &tx;
            let now = crate::util::now_unix();
            // One that never started waited until now; an attempt still open ends with it.
            require_completion_logs(c,&leg)?;
            close_wait(c, &leg, now)?;
            c.execute("UPDATE ci_attempts SET status = 'done', conclusion = ?2, finished_at = ?3 WHERE leg_id = ?1 AND status = 'running'", params![leg, conclusion, now])?;
            c.execute(
                "UPDATE jobs SET status = ?2, conclusion = ?3, outputs = COALESCE(?4, outputs), error = COALESCE(?5, error), finished_at = ?6 WHERE id = ?1",
                params![leg, status.as_str(), conclusion, outputs, error, now],
            )?;
            c.execute("UPDATE steps SET status = 'skipped' WHERE job_id = ?1 AND status IN ('pending', 'running')", [&leg])?;
            super::notify::stage_leg_control(c, &leg, status, &base, deployment.as_ref())?;
            super::logsearch::stage_finished(&tx,&leg)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    super::schedule::outbox::kick(hub).await;
    crate::observe::kiln::leg_finished(hub, &leg_id).await;
    // Its logs go into the search index (W19-R04).
    super::logsearch::kick();
    Ok(())
}

/// U03: metadata-only input adapter for an execution imported while it was already running.
/// Hub authenticates the exact reporting process. Old frames carry no name, outcome, outputs
/// or attempt: preserve absent fields and require the original durable imported assignment.
/// The provenance check and metadata write share one writer transaction with canonical claims.
pub(crate) async fn imported_step(hub: &Hub, agent: &str, job: &str, idx: u32, status: Status, exit_code: Option<i32>) -> anyhow::Result<bool> {
    anyhow::ensure!(status == Status::Running || status.is_terminal(), "invalid legacy step progress status");
    let (agent, job) = (agent.to_owned(), job.to_owned());
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let assigned: Option<(String, u32)> = tx.query_row("SELECT p.input, j.attempt FROM jobs j JOIN runs r ON r.id = j.run_id JOIN ci_plans p ON p.run_id = r.id JOIN ci_attempts a ON a.leg_id = j.id AND a.number = j.attempt WHERE j.id = ?1 AND j.agent = ?2 AND a.runner = ?2 AND j.status = 'running' AND a.status = 'running' AND p.done = 0 AND j.run_attempt = r.attempt AND r.status IN ('queued','pending','running') AND NOT EXISTS(SELECT 1 FROM ci_completion_reports c WHERE c.job = j.id AND c.attempt = j.attempt)", params![job, agent], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((input, attempt)) = assigned else { return Ok(false) };
        let input: serde_json::Value = serde_json::from_str(&input)?;
        let origin = input.get("legacy_running").and_then(|running| running.get(job.as_str()));
        if input.get("legacy_import").and_then(serde_json::Value::as_bool) != Some(true)
            || origin.and_then(|v| v.get("agent")).and_then(serde_json::Value::as_str) != Some(agent.as_str())
            || origin.and_then(|v| v.get("attempt")).and_then(serde_json::Value::as_u64) != Some(u64::from(attempt)) { return Ok(false); }
        let now = crate::util::now_unix();
        let changed = if status == Status::Running {
            tx.execute("UPDATE steps SET status = 'running', started_at = COALESCE(started_at, ?3) WHERE job_id = ?1 AND idx = ?2 AND status IN ('pending','running')", params![job, idx, now])?
        } else {
            tx.execute("UPDATE steps SET status = ?3, exit_code = ?4, finished_at = ?5, started_at = COALESCE(started_at, ?5) WHERE job_id = ?1 AND idx = ?2 AND status IN ('pending','running')", params![job, idx, status.as_str(), exit_code, now])?
        };
        tx.commit()?;
        Ok(changed == 1)
    }).await
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
    let run_id = run.to_owned();
    let (run, conclusion) = (run.to_owned(), conclusion.to_owned());
    let base = hub.cfg.server.public_url.clone();
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let c = &tx;
            let now = crate::util::now_unix();
            c.execute(
                "UPDATE runs SET status = ?2, conclusion = ?3, error = COALESCE(?4, error), finished_at = ?5, started_at = COALESCE(started_at, ?5) WHERE id = ?1",
                params![run, status.as_str(), conclusion, error, now],
            )?;
            c.execute("UPDATE ci_plans SET done = 1 WHERE run_id = ?1", [&run])?;
            super::notify::stage_run_control(c, &run, status, &base)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    super::schedule::outbox::kick(hub).await;
    crate::observe::kiln::run_ended(hub, &run_id).await;
    Ok(())
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
            let tx = crate::store::tx(c)?;
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
            let tx = crate::store::tx(c)?;
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
            super::logsearch::stage_finished(&tx,&id)?;
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

#[cfg(test)]
mod deployment_control_tests {
    use super::*;

    /// W19-R06/U14: accepted deployment controls follow the live post-gate predicate, and
    /// malformed/stale context rolls the whole finish transaction back before publication.
    #[test]
    fn w19_r06_deployment_control_requires_exact_accepted_current_leg_context() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub=crate::hub::tests::scratch_hub().await?;
            hub.store.call(|c| {
                c.execute("INSERT INTO repos(id,name,clone_url,created_at) VALUES('deploy-repo','acme/deploy','local',0)",[])?;
                for (run,attempt) in [("current",1),("rerun",2)] {
                    c.execute("INSERT INTO runs(id,repo_id,number,pipeline,event,branch,sha,status,created_at,attempt) VALUES(?1,'deploy-repo',?2,'ship','push','main','sha','running',0,?2)",params![run,attempt])?;
                }
                for (leg,run,enabled) in [("gated","current",true),("no-record","current",true),("disabled","current",false),("actual","current",true),("historical","current",true),("old-attempt","rerun",true),("timed-out","current",true),("cancelled","current",true)] {
                    let spec=serde_json::json!({"environment":{"name":"prod","deployment":enabled}}).to_string();
                    c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,leg,environment,attempt,run_attempt) VALUES(?1,?2,?1,?1,'running','[]','[]',?3,?3,'prod',3,1)",params![leg,run,spec])?;
                }
                for (id,leg,run) in [(10,"actual","current"),(11,"historical","current"),(12,"old-attempt","rerun"),(13,"disabled","current"),(14,"timed-out","current"),(15,"cancelled","current")] {
                    c.execute("INSERT INTO ci_deployments(id,repo_id,environment,sha,git_ref,creator,run_id,leg_id,created_at,updated_at) VALUES(?1,'deploy-repo','prod','sha','main','person',?2,?3,0,0)",params![id,run,leg])?;
                }
                Ok(())
            }).await?;
            // An environment declaration (including deployment:false), refusal before the gate,
            // or even a historical deployment row cannot substitute for the actual context.
            for (leg,status) in [("gated",Status::Failed),("no-record",Status::TimedOut),("disabled",Status::Succeeded),("historical",Status::Succeeded)] {
                finish_leg(&hub,leg,status,"failure",None,None).await?;
            }
            let controls=hub.store.call(|c|Ok(c.query_row("SELECT COUNT(*) FROM ci_trigger_outbox WHERE kind='control'",[],|r|r.get::<_,u32>(0))?)).await?;
            assert_eq!(controls,0);
            let context=|id,run:&str,attempt,environment:&str|Some(LegDeployment{id,run:run.into(),attempt,environment:environment.into()});
            for (leg,deployment) in [
                ("old-attempt",context(12,"rerun",3,"prod")),
                ("no-record",context(10,"current",3,"prod")),
                ("disabled",context(13,"current",3,"prod")),
                ("actual",context(10,"current",2,"prod")),
                ("actual",context(10,"current",3,"staging")),
                ("actual",context(10,"another-run",3,"prod")),
            ] {
                assert!(finish_leg_with_deployment(&hub,leg,Status::Succeeded,"success",None,None,deployment).await.is_err());
            }
            let status=hub.store.call(|c|Ok(c.query_row("SELECT status FROM jobs WHERE id='actual'",[],|r|r.get::<_,String>(0))?)).await?;
            assert_eq!(status,"running","invalid context rolls completion back");
            finish_leg_with_deployment(&hub,"cancelled",Status::Cancelled,"cancelled",None,None,context(15,"current",3,"prod")).await?;
            finish_leg_with_deployment(&hub,"actual",Status::Succeeded,"success",None,None,context(10,"current",3,"prod")).await?;
            finish_leg_with_deployment(&hub,"actual",Status::Succeeded,"success",None,None,context(10,"current",3,"prod")).await?;
            finish_leg_with_deployment(&hub,"timed-out",Status::TimedOut,"timed_out",None,None,context(14,"current",3,"prod")).await?;
            let rows:Vec<(String,String)>=hub.store.call(|c|Ok(c.prepare("SELECT id,payload FROM ci_trigger_outbox WHERE kind='control' ORDER BY json_extract(payload,'$.descriptor.event.type') DESC")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?)).await?;
            let controls:Vec<(String,serde_json::Value)>=rows.into_iter().map(|(id,payload)|Ok((id,serde_json::from_str(&payload)?))).collect::<anyhow::Result<_>>()?;
            assert_eq!(controls.len(),2,"only actual success and timeout deploy; duplicate acceptance is one source");
            let (key,payload)=controls.first().ok_or_else(||anyhow::anyhow!("accepted deployment control"))?;
            assert_eq!(key,&crate::util::sha256_hex(b"kiln/trigger/intent/v1/control/leg:actual:3:deployment"));
            assert_eq!(payload.pointer("/descriptor/event/type"),Some(&serde_json::json!("deploy.succeeded")));
            assert_eq!(payload.pointer("/descriptor/event/environments"),Some(&serde_json::json!(["prod"])));
            assert_eq!(payload.pointer("/descriptor/event/attempt"),Some(&serde_json::json!(1)));
            assert_eq!(controls.get(1).and_then(|(_,p)|p.pointer("/descriptor/event/type")),Some(&serde_json::json!("deploy.failed")));
            Ok(())
        })
    }
}
