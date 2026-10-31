//! U01/U02, W05-R25: import persisted v1 work into the workflow engine without assigning it
//! again. Already-expanded keys and job ids stay stable; running work keeps its runner. The
//! transaction is the ownership boundary, so a restart either sees v1 rows or a complete plan.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Context as _;
use kiln_ci::expr::Dialect;
use kiln_ci::plan::{Conclusion, LegReport, NoSource, RunInput, RunPlan, Settings};
use kiln_ci::trigger::{Event, Source, context};
use kiln_ci::wf::ir::{Concurrency, Defaults, Job, JobKiln, Need, RunsOn, Step, StepKiln, Tpl, Workflow, YVal};
use crate::sql::{Connection, OptionalExtension as _, params};

use crate::hub::Hub;
use crate::pipeline::PlannedJob;
use crate::store::Status;

/// Import unfinished v1 runs in bounded batches before rebuilding the dispatcher.
pub async fn import_all(hub: &Hub) -> anyhow::Result<usize> {
    let mut imported = 0;
    let mut after = 0_i64;
    loop {
        let ids: Vec<(i64, String)> = hub.store.call(move |c| {
            let mut q = c.prepare("SELECT rowid, id FROM runs WHERE rowid > ?1 AND workflow_id IS NULL AND provider = 'kiln' AND status IN ('queued','pending','running') ORDER BY rowid LIMIT 128")?;
            Ok(q.query_map([after], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?)
        }).await?;
        if ids.is_empty() {
            return Ok(imported);
        }
        for (rowid, id) in ids {
            after = rowid;
            match import_run(hub, &id).await {
                Ok(yes) => imported += usize::from(yes),
                Err(e) => {
                    // Corrupt source is not proof that its assigned work stopped. Preserve
                    // that host's reservation before cancellation can make the source terminal.
                    // A failed quarantine must fence admission rather than strand live work.
                    let reason = format!("This legacy run could not be migrated to the workflow engine: {e:#}. Start its workflow again.");
                    tracing::warn!(run = %id, error = %reason, "a legacy run could not be imported");
                    super::dispatch::quarantine_run(hub, &id).await
                        .with_context(|| format!("legacy run {id} could not preserve its execution ownership after migration failed"))?;
                    if let Err(e) = hub.cancel_run(&id, &reason).await {
                        tracing::error!(run = %id, error = %format!("{e:#}"), "a legacy run could not be stopped after its migration failed");
                        continue;
                    }
                    if let Err(e) = super::runs::finish_run(hub, &id, Status::Failed, "startup_failure", Some(reason)).await {
                        tracing::error!(run = %id, error = %format!("{e:#}"), "a legacy run's migration failure could not be recorded");
                    }
                }
            }
        }
    }
}

/// Also used for a person's retry of a historical v1 run. False means it already has a plan.
pub async fn import_run(hub: &Hub, run: &str) -> anyhow::Result<bool> {
    let run = run.to_owned();
    let secrets: Vec<String> = hub.cfg.secrets.keys().cloned().collect();
    let server = hub.cfg.server.public_url.clone();
    hub.store.call(move |c| import(c, &run, &secrets, &server)).await
}

struct OldRun {
    repo: String,
    rowid: i64,
    number: i64,
    attempt: u32,
    name: String,
    event: String,
    branch: String,
    sha: String,
    actor: String,
    status: Status,
    group: Option<String>,
}

struct OldJob {
    id: String,
    key: String,
    status: Status,
    spec: PlannedJob,
    placement: Option<kiln_ci::sched::Placement>,
}

fn import(c: &Connection, run: &str, secrets: &[String], server: &str) -> anyhow::Result<bool> {
    let tx = crate::store::tx(c)?;
    let old = tx.query_row(
        "SELECT p.name, r.rowid, r.number, r.attempt, r.pipeline, r.event, r.branch, r.sha, r.author, r.status, r.concurrency_group
         FROM runs r JOIN repos p ON p.id = r.repo_id WHERE r.id = ?1 AND r.workflow_id IS NULL AND r.provider = 'kiln'",
        [run], |r| Ok(OldRun { repo: r.get(0)?, rowid: r.get(1)?, number: r.get(2)?, attempt: r.get(3)?, name: r.get(4)?, event: r.get(5)?, branch: r.get(6)?, sha: r.get(7)?, actor: r.get(8)?, status: Status::parse(&r.get::<_, String>(9)?), group: r.get(10)? }),
    ).optional()?;
    let Some(old) = old else { return Ok(false) };
    let raw = {
        let mut q = tx.prepare("SELECT id, key, status, spec, placement FROM jobs WHERE run_id = ?1 ORDER BY rowid")?;
        q.query_map([run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?)))?.collect::<Result<Vec<_>, _>>()?
    };
    let jobs: Vec<OldJob> = raw.into_iter().map(|(id, key, status, spec, placement)| {
        let spec = serde_json::from_str(&spec).with_context(|| format!("legacy run {run}, job {key}: its stored specification cannot be imported"))?;
        let placement = placement.as_deref().map(serde_json::from_str).transpose().with_context(|| format!("legacy run {run}, job {key}: its placement cannot be imported"))?;
        Ok(OldJob { id, key, status: Status::parse(&status), spec, placement })
    }).collect::<anyhow::Result<_>>()?;
    anyhow::ensure!(!jobs.is_empty(), "legacy run {run} has no saved jobs; start its workflow again");
    let keys: BTreeSet<&str> = jobs.iter().map(|j| j.key.as_str()).collect();
    anyhow::ensure!(keys.len() == jobs.len(), "legacy run {run} has duplicate job keys");
    for j in &jobs {
        anyhow::ensure!(j.spec.key == j.key && j.spec.needs.iter().all(|n| keys.contains(n.as_str())), "legacy run {run}, job {} has inconsistent saved dependencies", j.key);
    }
    let mut ordered = BTreeSet::new();
    loop {
        let before = ordered.len();
        for j in &jobs {
            if j.spec.needs.iter().all(|n| ordered.contains(n.as_str())) { ordered.insert(j.key.as_str()); }
        }
        if ordered.len() == before { break }
    }
    anyhow::ensure!(ordered.len() == jobs.len(), "legacy run {run} has a dependency cycle");
    let wf = Arc::new(workflow(&old, &jobs, secrets));
    let event = Event {
        name: match old.event.as_str() { "manual" => "workflow_dispatch", "tag" => "push", e => e }.into(),
        git_ref: format!("refs/{}/{}", if old.event == "tag" { "tags" } else { "heads" }, old.branch),
        sha: old.sha.clone(), actor: old.actor.clone(), trusted: true, source: Source::Poll,
        payload: serde_json::json!({ "repository": { "default_branch": old.branch } }), ..Event::default()
    };
    let repo = context::Repo { full_name: old.repo.clone(), id: context::id_of(&old.repo), default_branch: old.branch.clone(), server_url: server.into(), ..context::Repo::default() };
    let facts = context::RunFacts {
        run_id: old.rowid.unsigned_abs(), run_number: old.number.unsigned_abs(), run_attempt: old.attempt,
        workflow: old.name.clone(), workflow_ref: format!("{}/{}@{}", old.repo, wf.path, event.git_ref), workflow_sha: old.sha.clone(),
        api_url: format!("{}/api/v3", server.trim_end_matches('/')), graphql_url: format!("{}/api/graphql", server.trim_end_matches('/')),
        retention_days: 30, actor_id: context::id_of(&old.actor), triggering_actor: old.actor.clone(),
    };
    let github = context::github(&event, &repo, &facts);
    let input = RunInput { github: github.clone(), trusted: true, ..RunInput::default() };
    let (mut plan, _) = RunPlan::start(Arc::clone(&wf), input, Settings::default(), &NoSource).map_err(anyhow::Error::msg)?;
    // A finished prerequisite may have been inserted after its dependent (v1's rows were ordered
    // by job name). Feed results only once the canonical plan has materialized each key.
    let mut left: BTreeSet<String> = jobs.iter().filter(|j| j.status.is_terminal()).map(|j| j.key.clone()).collect();
    let mut reports = Vec::new();
    loop {
        let ready: Vec<&OldJob> = jobs.iter().filter(|j| left.contains(&j.key) && plan.spec_of(&j.key).is_some()).collect();
        if ready.is_empty() { break }
        for j in ready {
            let report = LegReport::of(conclusion(j.status));
            let _ = plan.on_leg_finished(&j.key, report.clone(), &NoSource);
            reports.push((j.key.clone(), report));
            left.remove(&j.key);
        }
    }
    for j in jobs.iter().filter(|j| j.status == Status::Running) {
        anyhow::ensure!(plan.spec_of(&j.key).is_some(), "legacy run {run}, running job {} cannot be recovered by its saved plan", j.key);
    }
    let placed: BTreeMap<_, _> = jobs.iter().filter_map(|j| j.placement.as_ref().map(|p| (j.key.clone(), p.clone()))).collect();
    // U03: old progress frames have no attempt field. Only executions already running at
    // adoption may use that adapter; a later canonical claim consumes this provenance.
    let legacy_running: BTreeMap<String, serde_json::Value> = {
        let mut q = tx.prepare("SELECT id, agent, attempt FROM jobs WHERE run_id = ?1 AND status = 'running' AND agent IS NOT NULL")?;
        let rows = q.query_map([run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, u32>(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(|(job, agent, attempt)| (job, serde_json::json!({ "agent": agent, "attempt": attempt.saturating_add(1) }))).collect()
    };
    let saved = serde_json::json!({ "github": github, "inputs": null, "vars": {}, "trusted": true, "event": event, "actions": {}, "legacy_import": true, "legacy_running": legacy_running, "placed": placed });
    tx.execute("UPDATE runs SET workflow_id = ?2, actor = ?3, conclusion = CASE WHEN finished_at IS NOT NULL THEN ?4 ELSE conclusion END WHERE id = ?1", params![run, wf.path, old.actor, conclusion(old.status).as_str()])?;
    tx.execute("INSERT INTO ci_plans (run_id, workflow, input, done) VALUES (?1, ?2, ?3, ?4)", params![run, serde_json::to_string(&*wf)?, saved.to_string(), old.status.is_terminal()])?;
    tx.execute("INSERT INTO ci_events (run_id, payload) VALUES (?1, ?2)", params![run, event.payload.to_string()])?;
    for j in &jobs {
        let spec = plan.spec_of(&j.key).map(serde_json::to_string).transpose()?;
        tx.execute(
            "UPDATE jobs SET job_id = key, leg = ?2, conclusion = CASE WHEN status IN ('succeeded','failed','cancelled','skipped','timed_out') THEN ?3 ELSE conclusion END,
             attempt = attempt + 1, run_attempt = ?4 WHERE id = ?1",
            params![j.id, spec, conclusion(j.status).as_str(), old.attempt],
        )?;
        tx.execute(
            "INSERT INTO ci_attempts (id, leg_id, number, status, conclusion, runner, executor, queued_at, started_at, finished_at, exit_code, error, peak_rss, cpu_seconds)
             SELECT ?2, id, attempt, CASE WHEN finished_at IS NULL THEN status ELSE 'done' END, conclusion, agent, executor, queued_at, started_at, finished_at, exit_code, error, peak_rss, cpu_seconds FROM jobs WHERE id = ?1",
            params![j.id, crate::util::new_id()],
        )?;
        super::logsearch::stage_finished(&tx,&j.id)?;
    }
    for (i, (key, report)) in reports.iter().enumerate() {
        tx.execute("INSERT INTO ci_plan_log (run_id, seq, kind, leg, report, attempt) VALUES (?1, ?2, 'leg', ?3, ?4, ?5)", params![run, i64::try_from(i).unwrap_or(i64::MAX).saturating_add(1), key, serde_json::json!({ "conclusion": report.conclusion }).to_string(), old.attempt])?;
    }
    tx.commit()?;
    Ok(true)
}

fn conclusion(s: Status) -> Conclusion {
    match s {
        Status::Succeeded => Conclusion::Success,
        Status::Skipped => Conclusion::Skipped,
        Status::Cancelled => Conclusion::Cancelled,
        Status::TimedOut => Conclusion::TimedOut,
        _ => Conclusion::Failure,
    }
}

fn tpl(s: &str) -> Tpl {
    // Stored commands, environments and labels have already been interpolated by v1. Preserve
    // any remaining interpolation-shaped text as data rather than evaluating it a second time.
    let s = if s.contains("${{") { format!("${{{{ '{}' }}}}", s.replace('\'', "''")) } else { s.to_owned() };
    Tpl::new(s, Default::default())
}

/// The one translation of an expanded legacy job into the shared workflow model (U02/U03).
pub(crate) fn job(s: &PlannedJob, status: Status, secrets: &[String]) -> Job {
    let mut env: BTreeMap<String, Tpl> = s.env.iter().map(|(k, v)| (k.clone(), tpl(v))).collect();
    for name in secrets {
        env.insert(name.clone(), Tpl::new(format!("${{{{ secrets['{}'] }}}}", name.replace('\'', "''")), Default::default()));
    }
    let resources: Vec<(String, YVal)> = [("cpu", &s.resources.cpu), ("memory", &s.resources.memory)].into_iter().filter_map(|(k, v)| v.as_deref().map(|v| (k.into(), YVal::Str(tpl(v))))).collect();
    Job {
        id: s.key.clone(), name: Some(tpl(&s.display)), needs: s.needs.iter().cloned().map(Need::Job).collect(),
        cond: (status != Status::Running && !status.is_terminal()).then(|| s.condition.as_ref().map(|c| Tpl::new(c.clone(), Default::default()))).flatten(),
        runs_on: Some(RunsOn::Labels(s.labels.iter().map(|s| tpl(s)).collect())), env: env.into_iter().collect(),
        timeout_minutes: Some(YVal::Num(f64::from(s.timeout_minutes))),
        steps: s.steps.iter().map(|s| Step {
            name: s.name.as_deref().map(tpl), run: Some(tpl(&s.run)), shell: s.shell.as_deref().map(tpl),
            env: s.env.iter().map(|(k, v)| (k.clone(), tpl(v))).collect(), working_directory: s.working_directory.as_deref().map(tpl),
            timeout_minutes: s.timeout_minutes.map(|m| YVal::Num(f64::from(m))), continue_on_error: Some(YVal::Bool(s.continue_on_error)),
            kiln: StepKiln { tty: s.tty, ..StepKiln::default() }, ..Step::default()
        }).collect(),
        kiln: JobKiln {
            toolchain: s.toolchain.clone(), image: s.image.as_deref().map(tpl), exclusive: s.exclusive,
            resources: (!resources.is_empty()).then_some(YVal::Map(resources)), requires: s.requires.iter().map(|s| tpl(s)).collect(),
            retry: (s.retries > 0).then(|| YVal::Map(vec![
                ("max".into(), YVal::Num(f64::from(s.retries))),
                ("on".into(), YVal::Seq(vec![YVal::Str(tpl("runner-lost"))])),
            ])),
            cache: s.cache.iter().map(|s| YVal::Str(tpl(s))).collect(), artifacts: s.artifacts.clone(), ..JobKiln::default()
        }, defaults: Defaults::default(), ..Job::default()
    }
}

fn workflow(old: &OldRun, jobs: &[OldJob], secrets: &[String]) -> Workflow {
    let jobs = jobs.iter().map(|j| job(&j.spec, j.status, secrets)).collect();
    Workflow {
        dialect: Some(Dialect::Kiln), path: format!(".kiln/legacy/{}.yaml", old.name), name: Some(old.name.clone()), jobs, checkout: true,
        concurrency: old.group.as_deref().map(|g| Concurrency { group: tpl(g.strip_prefix(&format!("{}:", old.repo)).unwrap_or(g)), ..Concurrency::default() }), ..Workflow::default()
    }
}
