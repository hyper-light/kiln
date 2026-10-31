//! U01: retained executions keep Scheduler allocations after the plan becomes terminal.
//! Ownership events commit before transport ACK. Time only requests cleanup; it never frees
//! capacity. This journal authenticates proof and restores allocations, not placement decisions.

use kiln_ci::sched::state::{Allocation, ReservationMode};
use kiln_ci::wire::JobEventV2;
use crate::sql::params;
use crate::hub::Hub;
use super::{Dispatch, Input, Outcome, now_ms};

const LIMIT: usize = 4096;
const RECEIPT_LIMIT: usize = 16384;

#[derive(Debug, Clone)]
struct Record {
    job: String,
    attempt: u32,
    agent: String,
    execution: Option<String>,
    until: i64,
    allocation: Allocation,
    ordinary: Option<super::ordinary::Journal>,
    transferred: bool,
    released: bool,
}

// U01: receipts recognize authenticated phase replays without restoring capacity. No TTL or
// eviction can turn an accepted phase into an unrecognized one; exhaustion fences new writes.
async fn accepted(hub: &Hub, job: &str, attempt: u32, agent: &str, execution: &str, until: i64, released: bool) -> anyhow::Result<bool> {
    let (job, agent, execution) = (job.to_owned(), agent.to_owned(), execution.to_owned());
    hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_receipts WHERE job = ?1 AND attempt = ?2 AND agent = ?3 AND execution = ?4 AND released = ?5 AND until_at = ?6)",
        params![job, attempt, agent, execution, released, if released { 0 } else { until }], |r| r.get(0))?)).await
}

fn parent_grant(c: &crate::sql::Connection<'_>, record: &Record) -> anyhow::Result<Option<super::executors::ExecutorGrant>> {
    let Some(binding) = &record.allocation.parent_binding else { return Ok(None); };
    let json: String = c.query_row("SELECT v FROM kv WHERE k = ?1", [format!("u01/executor/{}", binding.executor)], |r| r.get(0))?;
    anyhow::ensure!(json.len() <= 16384, "U01 executor grant exceeds its byte budget");
    let grant: super::executors::ExecutorGrant = serde_json::from_str(&json)?;
    anyhow::ensure!(binding.job == record.job && binding.attempt == record.attempt && grant.executor == binding.executor
        && grant.snapshot.executor == binding.executor && grant.snapshot.runner == record.allocation.runner
        && grant.generation == binding.generation && grant.allocation == binding.allocation
        && grant.physical_execution.as_deref() == Some(binding.physical_execution.as_str()) && grant.binding.as_ref() == Some(binding),
        "U01 retained parent does not own this executor grant");
    Ok(Some(grant))
}

async fn records(hub: &Hub) -> anyhow::Result<Vec<Record>> {
    let rows = hub.store.call(|c| {
        let mut q = c.prepare("SELECT job, attempt, agent, execution, until_at, allocation, transferred, released FROM ci_retained_ownership ORDER BY job, attempt LIMIT 4097")?;
        Ok(q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?, r.get::<_, bool>(6)?, r.get::<_, bool>(7)?)))?.collect::<Result<Vec<_>, _>>()?)
    }).await?;
    anyhow::ensure!(rows.len() <= LIMIT, "U01 retained ownership exceeds its record budget");
    let mut identities = std::collections::BTreeSet::new();
    let mut records = Vec::new();
    for (job,attempt,agent,execution,until,json,transferred,released) in rows {
        let Some(allocation) = super::ordinary::allocation_json(&json)? else {
            super::ordinary::validate_marker(&job,attempt,&agent,execution.as_deref(),until,transferred,released)?;
            continue;
        };
        let ordinary = super::ordinary::journal_json(&json)?;
        if let Some(journal) = &ordinary {
            anyhow::ensure!(journal.owner.job == job && journal.owner.attempt == attempt && journal.owner.host == agent
                && execution.as_deref() == Some(journal.owner.execution.as_str()) && released == (journal.physical != "pending")
                && (!transferred || journal.logical != "running"), "ordinary SQL owner/phase changed its original journal");
        }
        anyhow::ensure!(!job.is_empty() && job.len() <= 512 && !agent.is_empty() && agent.len() <= 256 && attempt > 0
            && execution.as_ref().is_none_or(|id| !id.is_empty() && id.len() <= 256), "U01 invalid durable execution owner");
        anyhow::ensure!(identities.insert(allocation.id.clone()), "U01 duplicate durable allocation identity");
        anyhow::ensure!(allocation.id.starts_with("retained-") && allocation.id.len() <= 128
            && allocation.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !allocation.runner.is_empty() && allocation.runner.len() <= 256
            && allocation.request.cpu.is_finite() && allocation.request.cpu >= 0.0,
            "U01 invalid durable retained allocation");
        records.push(Record { job,attempt,agent,execution,until,allocation,ordinary,transferred,released });
    }
    Ok(records)
}

/// U01: source corruption is not process death. Called BEFORE legacy cancellation or
/// terminalization. Unknown resource requests conservatively fence the whole assigned host.
/// The dispatcher adopts these records before admitting any new work.
pub async fn quarantine_run(hub: &Hub, run: &str) -> anyhow::Result<()> {
    let run = run.to_owned();
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let mut q = tx.prepare("SELECT id, agent, attempt, job_id IS NULL, COALESCE(kept_until, 0) FROM jobs WHERE run_id = ?1 AND (status = 'running' OR kept_until IS NOT NULL)")?;
        let jobs = q.query_map([&run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, u32>(2)?, r.get::<_, bool>(3)?, r.get::<_, i64>(4)?)))?.collect::<Result<Vec<_>, _>>()?;
        drop(q);
        for (job, agent, attempt, legacy, until) in jobs {
            let agent = agent.filter(|a| !a.is_empty()).ok_or_else(|| anyhow::anyhow!("U01 running job {job} has no host; admission must remain fenced until inventory supplies stop proof"))?;
            let attempt = if legacy { attempt.saturating_add(1) } else { attempt.max(1) };
            let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_ownership WHERE job = ?1 AND attempt = ?2)", params![job, attempt], |r| r.get(0))?;
            let count: usize = tx.query_row("SELECT count(*) FROM ci_retained_ownership", [], |r| r.get(0))?;
            anyhow::ensure!(exists || count < LIMIT, "U01 retained ownership table is full");
            let allocation = Allocation { id: format!("retained-{}", crate::util::new_id()), runner: agent.clone(), request: Default::default(), mode: ReservationMode::Whole,
                repo: "quarantine".into(), actor: String::new(), lane: String::new(), parent_binding: None };
            // Never shrink or replace an already authenticated execution's exact footprint.
            tx.execute("INSERT INTO ci_retained_ownership (job, attempt, agent, until_at, allocation, transferred, received_at) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6) ON CONFLICT(job, attempt) DO UPDATE SET transferred = 1",
                params![job, attempt, agent, until, serde_json::to_string(&allocation)?, crate::util::now_unix()])?;
        }
        tx.commit()?;
        Ok(())
    }).await
}

#[derive(Debug,thiserror::Error)]
#[error("LegacyNativeAssignmentsRequireDrain: restored native Running work lacks original physical ownership; drain every pre-feature assignment and old writer before activation")]
struct LegacyNativeAssignmentsRequireDrain;

impl Dispatch {
    /// Read-only migration boundary. No historical grant/footprint or stop proof is
    /// invented from a job status, an empty receiver or a new-format marker.
    async fn require_legacy_native_drain(&self, job: Option<&str>) -> anyhow::Result<()> {
        let job = job.map(str::to_owned);
        let unbound: bool = self.hub.store.call(move |c| {
            let predicate = "j.status='running' AND COALESCE(j.executor,'host') IN ('host','docker') AND NOT EXISTS (SELECT 1 FROM ci_retained_ownership h WHERE h.job=j.id AND h.attempt=CASE WHEN j.job_id IS NULL THEN j.attempt+1 ELSE MAX(j.attempt,1) END)";
            Ok(match job {
                Some(job) => c.query_row(&format!("SELECT EXISTS(SELECT 1 FROM jobs j WHERE j.id=?1 AND {predicate})"),[job],|r|r.get(0))?,
                None => c.query_row(&format!("SELECT EXISTS(SELECT 1 FROM jobs j WHERE {predicate})"),[],|r|r.get(0))?,
            })
        }).await?;
        anyhow::ensure!(!unbound,LegacyNativeAssignmentsRequireDrain);
        Ok(())
    }
    pub(super) async fn restore_retained(&mut self) -> anyhow::Result<()> {
        // Refuse before legacy restore/quarantine can claim the new ordinary law.
        // Existing bootstrap aborts safely here; full physical drain is an operator
        // deployment prerequisite, never certified by this SQL read.
        self.require_legacy_native_drain(None).await?;
        // Old Kept rows have no execution identity. Restore them as whole-host quarantines;
        // even an elapsed UI deadline cannot establish that their processes disappeared.
        let runs = self.hub.store.call(|c| {
            let mut q = c.prepare("SELECT DISTINCT run_id FROM jobs j WHERE kept_until IS NOT NULL AND NOT EXISTS (SELECT 1 FROM ci_retained_ownership h WHERE h.job = j.id AND h.attempt = CASE WHEN j.job_id IS NULL THEN j.attempt + 1 ELSE MAX(j.attempt, 1) END) LIMIT 4097")?;
            Ok(q.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
        }).await?;
        anyhow::ensure!(runs.len() <= LIMIT, "U01 legacy retained inventory exceeds its budget");
        for run in runs { quarantine_run(&self.hub, &run).await?; }
        for record in records(&self.hub).await? {
            if record.released { continue; }
            if record.ordinary.is_none() || record.transferred || record.until != 0 {
                self.hub.ci.keep(&record.job, Some((record.agent.clone(), record.until)));
                self.sched.handle(Input::RetainExecution { leg: record.job.clone(), attempt: record.attempt }, now_ms());
            }
            if !record.transferred || self.admission.retained.contains(&record.allocation.id) { continue; }
            let commands = self.sched.handle(Input::LegRunning { leg: Box::new(record.allocation.leg()), runner: record.allocation.runner.clone(), attempt: 1 }, now_ms());
            self.next.extend(commands);
            self.admission.retained.insert(record.allocation.id);
        }
        Ok(())
    }

    /// U01: recovery may discover physical stopped proof after the logical job disappeared.
    /// Only the authenticated host sees its own exact execution identities; this read cannot
    /// release or resize any Scheduler allocation.
    pub(super) async fn retained_inventory(&self, agent: &str) -> anyhow::Result<serde_json::Value> {
        let retained = records(&self.hub).await?.into_iter().filter(|r| r.agent == agent && !r.released).map(|r| serde_json::json!({
            "job": r.job, "attempt": r.attempt, "execution": r.execution, "reservation": r.allocation.id,
            "physical_runner": r.allocation.runner, "transferred": r.transferred, "until": r.until,
            "ordinary_owner": r.ordinary.as_ref().map(|journal| &journal.owner),
        })).collect::<Vec<_>>();
        Ok(serde_json::json!({ "ok": true, "retained": retained }))
    }

    pub(super) async fn retained_event(&mut self, agent: &str, job: &str, event: JobEventV2) -> anyhow::Result<()> {
        anyhow::ensure!(self.admission.ready, "U01 ownership admission is fenced");
        if self.ordinary_kept(agent,job,event.clone()).await? { return Ok(()); }
        anyhow::ensure!(!agent.is_empty() && agent.len() <= 256 && !job.is_empty() && job.len() <= 512, "U01 invalid retained identity");
        let (execution, attempt, until, release) = match event {
            JobEventV2::KeptOwned { execution, attempt, until } => (Some(execution), attempt, until, false),
            JobEventV2::KeptReleased { execution, attempt } => (Some(execution), attempt, 0, true),
            JobEventV2::Kept { until } => {
                let attempt = self.legs.get(job).filter(|l| l.agent.as_deref() == Some(agent)).map(|l| l.attempt)
                    .or(records(&self.hub).await?.iter().rev().find(|r| r.job == job && r.agent == agent && !r.released).map(|r| r.attempt))
                    .ok_or_else(|| anyhow::anyhow!("U01 legacy kept event has no owned attempt"))?;
                (None, attempt, until, false)
            }
            _ => return Ok(()),
        };
        anyhow::ensure!(attempt > 0 && execution.as_ref().is_none_or(|id| !id.is_empty() && id.len() <= 256), "U01 invalid execution identity");
        let held = records(&self.hub).await?.into_iter().find(|r| r.job == job && r.attempt == attempt);
        if held.as_ref().is_none_or(|r| r.released)
            && let Some(id) = execution.as_deref()
            && accepted(&self.hub, job, attempt, agent, id, until, release).await? { return Ok(()); }
        if release {
            let Some(record) = held else {
                anyhow::bail!("U01 release has no accepted execution/attempt");
            };
            anyhow::ensure!(record.agent == agent && record.execution == execution && execution.is_some(), "U01 release does not own this execution/attempt");
            // Reject provisional guest cleanup before entering the write-failure fence. The
            // transaction rechecks this proof so only a durable exact parent stop can release.
            if record.allocation.parent_binding.is_some() {
                let owner = record.clone();
                self.hub.store.call(move |c| {
                    let grant = parent_grant(c, &owner)?.ok_or_else(|| anyhow::anyhow!("U01 missing parent ownership"))?;
                    anyhow::ensure!(grant.phase == "stopped", "U01 child cleanup cannot release a live parent executor");
                    Ok(())
                }).await?;
            }
            self.commit_retained(&record, true).await?;
            self.sched.handle(Input::ExecutionReleased { leg: job.to_owned(), attempt }, now_ms());
            if self.admission.retained.remove(&record.allocation.id) {
                let commands = self.sched.handle(Input::AttemptFinished { leg: record.allocation.id, attempt: 1, outcome: Outcome::Success }, now_ms());
                if let Some(commands) = self.after(commands) {
                    Box::pin(self.commands(commands)).await;
                }
            }
            self.refresh_retained_owner(job, agent).await?;
            self.prune_released().await?;
            return Ok(());
        }
        let mut record = match held {
            Some(r) => {
                anyhow::ensure!(r.agent == agent && (execution.is_none() || r.execution.is_none() || r.execution == execution), "U01 kept event changed its execution owner");
                r
            }
            None => {
                anyhow::ensure!(self.legs.get(job).is_some_and(|l| l.agent.as_deref() == Some(agent) && l.attempt == attempt), "U01 kept event has no assigned attempt");
                let allocation = self.sched.allocation(job, attempt, format!("retained-{}", crate::util::new_id())).ok_or_else(|| anyhow::anyhow!("U01 assigned allocation unavailable"))?;
                Record { job: job.to_owned(), attempt, agent: agent.to_owned(), execution: None, until, allocation, ordinary: None, transferred: false, released: false }
            }
        };
        anyhow::ensure!(!record.released, "U01 kept phase was not previously accepted");
        if execution.is_some() { record.execution = execution; }
        record.until = until;
        self.commit_retained(&record, false).await?;
        self.sched.handle(Input::RetainExecution { leg: job.to_owned(), attempt }, now_ms());
        self.hub.ci.keep(job, Some((agent.to_owned(), until)));
        Ok(())
    }

    /// U01: only the authenticated executor sibling calls this after journaling kernel stop
    /// proof. The normal release path rechecks that exact stopped grant in its transaction.
    pub(super) async fn retained_parent_stopped(&mut self, binding: &kiln_ci::wire::ParentBinding) -> anyhow::Result<()> {
        for record in records(&self.hub).await?.into_iter().filter(|r| !r.released && r.allocation.parent_binding.as_ref() == Some(binding)) {
            let execution = record.execution.ok_or_else(|| anyhow::anyhow!("U01 parent-bound retained owner lacks execution identity"))?;
            self.retained_event(&record.agent, &record.job, JobEventV2::KeptReleased { execution, attempt: record.attempt }).await?;
        }
        Ok(())
    }

    async fn commit_retained(&mut self, record: &Record, released: bool) -> anyhow::Result<()> {
        let record = record.clone();
        anyhow::ensure!(serde_json::to_string(&record.allocation)?.len() <= 16384, "U01 allocation exceeds its byte budget");
        let result = self.hub.store.call(move |c| {
            let tx = crate::store::tx(c)?;
            if released {
                if let Some(grant) = parent_grant(&tx, &record)? {
                    anyhow::ensure!(grant.phase == "stopped", "U01 child cleanup cannot release a live parent executor");
                }
            } else if record.transferred && let Some(mut grant) = parent_grant(&tx, &record)? {
                anyhow::ensure!(matches!(grant.phase.as_str(), "busy" | "stopping" | "retained" | "retained_stopping"), "U01 parent executor cannot transfer retained ownership");
                // Capacity moves to this retained allocation once; an already ordered Stop
                // remains durable through transfer and must survive controller recovery.
                grant.phase = if matches!(grant.phase.as_str(), "stopping" | "retained_stopping") { "retained_stopping" } else { "retained" }.into();
                tx.execute("UPDATE kv SET v = ?2 WHERE k = ?1", params![format!("u01/executor/{}", grant.executor), serde_json::to_string(&grant)?])?;
            }
            if let Some(execution) = &record.execution {
                let until = if released { 0 } else { record.until };
                let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_receipts WHERE job = ?1 AND attempt = ?2 AND agent = ?3 AND execution = ?4 AND released = ?5 AND until_at = ?6)", params![record.job, record.attempt, record.agent, execution, released, until], |r| r.get(0))?;
                if !exists {
                    let count: usize = tx.query_row("SELECT count(*) FROM ci_retained_receipts", [], |r| r.get(0))?;
                    // Reserve one release receipt for every possible active ownership tuple.
                    let budget = if released { RECEIPT_LIMIT } else { RECEIPT_LIMIT - LIMIT };
                    anyhow::ensure!(count < budget, "U01 accepted retained receipt budget exhausted");
                    tx.execute("INSERT INTO ci_retained_receipts(job,attempt,agent,execution,released,until_at) VALUES (?1,?2,?3,?4,?5,?6)", params![record.job, record.attempt, record.agent, execution, released, until])?;
                }
            }
            let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_ownership WHERE job = ?1 AND attempt = ?2)", params![record.job, record.attempt], |r| r.get(0))?;
            if !exists {
                let count: usize = tx.query_row("SELECT count(*) FROM ci_retained_ownership", [], |r| r.get(0))?;
                anyhow::ensure!(count < LIMIT, "U01 retained ownership table is full");
            }
            let changed = tx.execute("INSERT INTO ci_retained_ownership (job, attempt, agent, execution, until_at, allocation, transferred, released, received_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(job, attempt) DO UPDATE SET execution = COALESCE(ci_retained_ownership.execution, excluded.execution), until_at = excluded.until_at, transferred = MAX(ci_retained_ownership.transferred, excluded.transferred), released = MAX(ci_retained_ownership.released, excluded.released) WHERE ci_retained_ownership.agent = excluded.agent AND (ci_retained_ownership.execution IS NULL OR excluded.execution IS NULL OR ci_retained_ownership.execution = excluded.execution)",
                params![record.job, record.attempt, record.agent, record.execution, record.until, serde_json::to_string(&record.allocation)?, record.transferred, released, crate::util::now_unix()])?;
            anyhow::ensure!(changed == 1, "U01 durable execution owner changed");
            tx.execute("UPDATE jobs SET kept_until = ?4 WHERE id = ?1 AND agent = ?2 AND (CASE WHEN job_id IS NULL THEN attempt + 1 ELSE MAX(attempt, 1) END) = ?3", params![record.job, record.agent, record.attempt, if released { None } else { Some(record.until) }])?;
            tx.commit()?;
            Ok(())
        }).await;
        if result.is_err() {
            // The actor can still drain proof/recovery after restart, but cannot issue a new
            // physical assignment after an ownership commit failed.
            self.admission.ready = false;
            self.hub.ci.on.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    pub(super) async fn refresh_retained_owner(&self, job: &str, agent: &str) -> anyhow::Result<()> {
        let held = records(&self.hub).await?.iter().any(|r| r.job == job && r.agent == agent && !r.released);
        if !held {
            let mut kept = self.hub.ci.kept.lock();
            if let Some(owners) = kept.get_mut(job) {
                owners.remove(agent);
                if owners.is_empty() { kept.remove(job); }
            }
        }
        Ok(())
    }

    pub(super) async fn prune_released(&self) -> anyhow::Result<()> {
        // U01: accepted phase identities survive here in the bounded receipt journal; pruning
        // capacity records never permits a replay to recreate or reserve an allocation.
        self.hub.store.call(|c| {
            c.execute("DELETE FROM ci_retained_ownership WHERE released = 1 AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.id = ci_retained_ownership.job AND j.status IN ('queued', 'pending', 'running') AND (CASE WHEN j.job_id IS NULL THEN j.attempt + 1 ELSE MAX(j.attempt, 1) END) = ci_retained_ownership.attempt)", [])?;
            Ok(())
        }).await
    }

    pub(super) async fn prepare_retained(&mut self, agent: &str, job: &str, attempt: u32) -> anyhow::Result<Option<Allocation>> {
        anyhow::ensure!(self.admission.ready, "U01 ownership admission is fenced");
        let Some(mut record) = records(&self.hub).await?.into_iter().find(|r| r.job == job && r.attempt == attempt) else {
            // A detectable old native assignment cannot fall through lose -> refund.
            self.require_legacy_native_drain(Some(job)).await?;
            return Ok(None);
        };
        anyhow::ensure!(record.agent == agent, "U01 terminal attempt does not own retained allocation");
        if record.released { return Ok(None); }
        anyhow::ensure!(record.ordinary.is_none(), "ordinary terminal transition must share its original attempt writer");
        record.transferred = true;
        self.commit_retained(&record, false).await?;
        self.admission.retained.insert(record.allocation.id.clone());
        Ok(Some(record.allocation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use std::sync::Arc;
    use kiln_ci::sched::state::{Config, LegIn, Resources, RunnerIn, Scheduler};

    fn dispatcher(hub: Arc<Hub>) -> Dispatch {
        Dispatch { hub, sched: Scheduler::new(Config::default()), routing: Default::default(), held: Default::default(), runs: Default::default(), legs: Default::default(),
            timers: Default::default(), next: Vec::new(), in_commands: false, pods: Default::default(), absent_after: None, decisions: Default::default(), warm: Default::default(),
            warm_seq: 0, admission: Default::default(), pending_losses: Default::default() }
    }

    async fn fixture(hub: &Hub) -> anyhow::Result<String> {
        use crate::store::{Repo, Run, Status};
        let repo = hub.store.upsert_repo(Repo { id: "retained-repo".into(), name: "test/retained".into(), clone_url: "file:///missing".into(), branches: vec!["main".into()], poll_seconds: 60,
            enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: Some("main".into()), mirror: None }).await?;
        let workflow = crate::pipeline::parse("name: ci\njobs:\n  a:\n    runs-on: [local]\n    steps: [{run: echo prepared}]\n")?;
        let jobs = crate::pipeline::plan(&workflow, &crate::pipeline::Context::default())?;
        hub.store.create_run(Run { id: "retained-run".into(), repo_id: repo.id, repo_name: repo.name, number: 0, pipeline: "ci".into(), event: "manual".into(), branch: "main".into(), sha: "saved".into(),
            message: String::new(), author: "person".into(), status: Status::Queued, created_at: 0, started_at: None, finished_at: None, error: None, concurrency_group: None }, jobs).await?;
        hub.store.call(|c| { c.execute("UPDATE jobs SET status = 'running', agent = 'host', started_at = 1 WHERE run_id = 'retained-run'", [])?; Ok(()) }).await?;
        hub.store.jobs("retained-run".into(), false).await?.into_iter().next().map(|j| j.id).ok_or_else(|| anyhow::anyhow!("fixture job missing"))
    }

    /// U01: durable intent precedes terminal state, restart restores exact capacity, and
    /// execution/attempt/host authentication gates proof. An elapsed deadline changes none.
    #[test]
    fn u01_retained_durable_transfer_restart_authenticated_release_and_historical_replay() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let job = fixture(&hub).await?;
            let mut d = dispatcher(Arc::clone(&hub));
            d.restore_admission().await?;
            let runner = RunnerIn { id: "host".into(), slots: 1, ..RunnerIn::default() };
            d.sched.handle(Input::RunnerUp(Box::new(runner.clone())), 0);
            d.sched.handle(Input::LegQueued(Box::new(LegIn { id: job.clone(), run: "retained-run".into(), repo: "repo".into(), request: Resources { cpu: 2.0, mem: 20, ..Resources::default() }, ..LegIn::default() })), 1);
            let allocation = d.sched.allocation(&job, 1, "retained-test".into()).ok_or_else(|| anyhow::anyhow!("allocation missing"))?;
            let record = Record { job: job.clone(), attempt: 1, agent: "host".into(), execution: Some("execution-one".into()), until: 0, allocation: allocation.clone(), ordinary: None, transferred: false, released: false };
            d.commit_retained(&record, false).await?;
            d.retained_event("host", &job, JobEventV2::KeptOwned { execution: "execution-one".into(), attempt: 1, until: 0 }).await?;
            assert!(hub.ci.kept_by(&job, "host"), "deadline does not expire proof authorization");
            let transfer = d.prepare_retained("host", &job, 1).await?.ok_or_else(|| anyhow::anyhow!("transfer missing"))?;
            assert_eq!(transfer, allocation);
            assert!(records(&hub).await?.iter().any(|r| r.transferred), "transfer commits before terminal state");
            d.sched.handle(Input::RetainedFinished { leg: job.clone(), attempt: 1, reservation: Box::new(transfer), outcome: Outcome::Success }, 2);
            let completed = job.clone();
            hub.store.call(move |c| { c.execute("UPDATE jobs SET status = 'failed' WHERE id = ?1", [completed])?; Ok(()) }).await?;
            drop(d);
            let mut after = dispatcher(Arc::clone(&hub));
            after.restore_admission().await?;
            after.restore_retained().await?;
            after.sched.handle(Input::RunnerUp(Box::new(runner)), 3);
            after.sched.handle(Input::LegQueued(Box::new(LegIn { id: "waiting".into(), repo: "other".into(), ..LegIn::default() })), 4);
            assert!(after.sched.reason("waiting").is_some());
            assert_eq!(after.sched.allocation("retained-test", 1, "retained-test".into()), Some(allocation));
            let proof = JobEventV2::KeptReleased { execution: "execution-one".into(), attempt: 1 };
            assert!(after.retained_event("intruder", &job, proof.clone()).await.is_err());
            assert!(after.retained_event("host", &job, JobEventV2::KeptReleased { execution: "execution-stale".into(), attempt: 1 }).await.is_err());
            assert!(after.sched.reason("waiting").is_some());
            after.retained_event("host", &job, proof.clone()).await?;
            assert!(after.sched.reason("waiting").is_none(), "only committed host proof permits assignment");
            assert!(!hub.ci.kept_by(&job, "host"));
            assert!(records(&hub).await?.is_empty());
            after.retained_event("host", &job, proof).await?;
            // U01: a lost Owned ACK can arrive after Released was accepted and its capacity
            // tuple pruned. Restart still recognizes exactly that phase, never readopts it.
            let mut replay = dispatcher(Arc::clone(&hub));
            replay.restore_admission().await?;
            replay.restore_retained().await?;
            let owned = JobEventV2::KeptOwned { execution: "execution-one".into(), attempt: 1, until: 0 };
            replay.retained_event("host", &job, owned.clone()).await?;
            assert!(replay.retained_event("intruder", &job, owned).await.is_err());
            assert!(replay.retained_event("host", &job, JobEventV2::KeptOwned { execution: "execution-one".into(), attempt: 1, until: 1 }).await.is_err());
            assert!(replay.retained_event("host", &job, JobEventV2::KeptOwned { execution: "different".into(), attempt: 1, until: 0 }).await.is_err());
            assert!(replay.retained_event("host", &job, JobEventV2::KeptReleased { execution: "execution-one".into(), attempt: 2 }).await.is_err());
            assert!(records(&hub).await?.is_empty());
            assert!(replay.admission.retained.is_empty());
            assert!(!hub.ci.kept_by(&job, "host"));
            // Same existing behavioral oracle now exercises ordinary pre-enqueue custody,
            // proof/loss races, durable restart and permanent old-reader refusal.
            let ordinary_hub = crate::hub::tests::scratch_hub().await?;
            let mut ordinary = dispatcher(ordinary_hub);
            ordinary.restore_admission().await?;
            super::super::ordinary::exercise_oracle(&mut ordinary).await?;
            // A2: native routing can select a runner before its asynchronous ordinary
            // readiness. A known capture refusal requeues without a claim/failed attempt.
            let readiness_hub=crate::hub::tests::scratch_hub().await?;
            let readiness_job=fixture(&readiness_hub).await?;
            let queued=readiness_job.clone();
            readiness_hub.store.call(move|c|{c.execute("UPDATE jobs SET status='queued',agent=NULL,executor=NULL,attempt=0 WHERE id=?1",[queued])?;Ok(())}).await?;
            // Use the real existing canonical conversion; it retains the original job ID,
            // sets job_id and records its queued attempt1 before this deferral exercise.
            assert!(crate::ci::legacy::import_run(&readiness_hub,"retained-run").await?);
            let checked=readiness_job.clone();
            let canonical:(bool,u32)=readiness_hub.store.call(move|c|Ok(c.query_row("SELECT job_id IS NOT NULL,attempt FROM jobs WHERE id=?1",[checked],|row|Ok((row.get(0)?,row.get(1)?)))?)).await?;
            assert_eq!(canonical,(true,1));
            let mut readiness=dispatcher(Arc::clone(&readiness_hub));readiness.restore_admission().await?;
            readiness.sched.handle(Input::RunnerUp(Box::new(RunnerIn{id:"not-ready".into(),slots:1,..RunnerIn::default()})),0);
            readiness.sched.handle(Input::LegQueued(Box::new(LegIn{id:readiness_job.clone(),run:"retained-run".into(),repo:"repo".into(),..LegIn::default()})),1);
            assert!(readiness_hub.capture_newest_runner("not-ready",crate::proto::CAP_ORDINARY_EXECUTION_DEBT).is_err());
            assert!(readiness.sched.allocation(&readiness_job,1,"before-ready".into()).is_some());
            let before=super::super::now_ms();
            readiness.defer_ordinary_unexposed(&readiness_job,1,"the original channel has not supplied Have".into()).await?;
            let after=super::super::now_ms();
            assert!(readiness.admission.ready,"known readiness gap does not fence unrelated admission");
            let checked=readiness_job.clone();
            let (status,attempt,attempts,running_attempts,intents)=readiness_hub.store.call(move|c|{
                let (status,attempt):(String,u32)=c.query_row("SELECT status,attempt FROM jobs WHERE id=?1",[&checked],|row|Ok((row.get(0)?,row.get(1)?)))?;
                let attempts:i64=c.query_row("SELECT count(*) FROM ci_attempts WHERE leg_id=?1",[&checked],|row|row.get(0))?;
                let running_attempts:i64=c.query_row("SELECT count(*) FROM ci_attempts WHERE leg_id=?1 AND status='running'",[&checked],|row|row.get(0))?;
                let intents:i64=c.query_row("SELECT count(*) FROM ci_retained_ownership WHERE job=?1",[&checked],|row|row.get(0))?;
                Ok((status,attempt,attempts,running_attempts,intents))
            }).await?;
            assert_eq!((status.as_str(),attempt,attempts,running_attempts,intents),("queued",1,1,0,0),"deferral adds no attempt beyond the canonical imported queued attempt");
            assert!(readiness.sched.allocation(&readiness_job,1,"while-deferred".into()).is_none());
            let early=readiness.sched.handle(Input::Tick,before.saturating_add(14_999));
            assert!(!early.iter().any(|command|matches!(command,kiln_ci::sched::Command::Assign{..})),"no immediate alternate-host retry");
            let due=readiness.sched.handle(Input::Tick,after.saturating_add(15_001));
            assert!(due.iter().any(|command|matches!(command,kiln_ci::sched::Command::Assign{leg,attempt:1,..} if leg==&readiness_job)),"same attempt resumes only on existing timer");
            // Existing historical/running ownership cannot be relabelled a readiness Full.
            let allocation=readiness.sched.allocation(&readiness_job,1,"older-owned".into()).ok_or_else(||anyhow::anyhow!("readiness retry allocation absent"))?;
            readiness.commit_retained(&Record{job:readiness_job.clone(),attempt:1,agent:"not-ready".into(),execution:Some("older-execution".into()),until:0,allocation:allocation.clone(),ordinary:None,transferred:false,released:false},false).await?;
            assert!(readiness.defer_ordinary_unexposed(&readiness_job,1,"not an admission refusal".into()).await.is_err());
            assert!(!readiness.admission.ready);
            assert_eq!(readiness.sched.allocation(&readiness_job,1,"older-owned".into()),Some(allocation),"unresolved original charge remains held");
            // Already-cancelled SQL is not an eligible readiness disposition. The same
            // atomic callback must leave it cancelled and keep the exact assigned charge.
            let cancelled_hub=crate::hub::tests::scratch_hub().await?;
            let cancelled_job=fixture(&cancelled_hub).await?;
            let queued=cancelled_job.clone();
            cancelled_hub.store.call(move|c|{c.execute("UPDATE jobs SET status='queued',agent=NULL,executor=NULL,attempt=0 WHERE id=?1",[queued])?;Ok(())}).await?;
            assert!(crate::ci::legacy::import_run(&cancelled_hub,"retained-run").await?);
            let cancelled=cancelled_job.clone();
            cancelled_hub.store.call(move|c|{c.execute("UPDATE jobs SET status='cancelled' WHERE id=?1 AND job_id IS NOT NULL AND attempt=1",[cancelled])?;Ok(())}).await?;
            let checked=cancelled_job.clone();
            let canonical:bool=cancelled_hub.store.call(move|c|Ok(c.query_row("SELECT job_id IS NOT NULL FROM jobs WHERE id=?1",[checked],|row|row.get(0))?)).await?;
            assert!(canonical,"cancelled refusal also exercises a native canonical row");
            let mut cancelled_dispatch=dispatcher(Arc::clone(&cancelled_hub));cancelled_dispatch.restore_admission().await?;
            cancelled_dispatch.sched.handle(Input::RunnerUp(Box::new(RunnerIn{id:"cancelled-host".into(),slots:1,..RunnerIn::default()})),0);
            cancelled_dispatch.sched.handle(Input::LegQueued(Box::new(LegIn{id:cancelled_job.clone(),run:"retained-run".into(),repo:"repo".into(),..LegIn::default()})),1);
            let cancelled_allocation=cancelled_dispatch.sched.allocation(&cancelled_job,1,"cancelled-owned".into()).ok_or_else(||anyhow::anyhow!("cancelled fixture allocation absent"))?;
            assert!(cancelled_dispatch.defer_ordinary_unexposed(&cancelled_job,1,"not a retryable readiness gap".into()).await.is_err());
            assert_eq!(cancelled_dispatch.sched.allocation(&cancelled_job,1,"cancelled-owned".into()),Some(cancelled_allocation));
            let checked=cancelled_job.clone();
            let status:String=cancelled_hub.store.call(move|c|Ok(c.query_row("SELECT status FROM jobs WHERE id=?1",[checked],|row|row.get(0))?)).await?;
            assert_eq!(status,"cancelled");
            Ok(())
        })
    }

    /// U01: a malformed legacy running spec retains a whole-host quarantine even if its
    /// logical row is terminal. Inventory must bind identity before stopped proof can release.
    #[test]
    fn u01_corrupt_legacy_running_job_quarantines_host_until_inventory_stop_proof() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let job = fixture(&hub).await?;
            hub.store.call(|c| { c.execute("UPDATE jobs SET spec = '{broken' WHERE run_id = 'retained-run'", [])?; Ok(()) }).await?;
            let mut unbound = dispatcher(Arc::clone(&hub));
            unbound.restore_admission().await?;
            let refused = unbound.restore_retained().await.err().context("unbound pre-feature native work must require physical drain")?;
            assert!(refused.is::<LegacyNativeAssignmentsRequireDrain>());
            assert!(records(&hub).await?.is_empty(), "drain refusal must not fabricate a historical allocation");
            assert!(unbound.prepare_retained("host",&job,1).await.is_err(), "loss cannot refund an unbound original native assignment");
            quarantine_run(&hub, "retained-run").await?;
            hub.store.call(|c| { c.execute("UPDATE jobs SET status = 'failed' WHERE run_id = 'retained-run'", [])?; Ok(()) }).await?;
            let mut d = dispatcher(Arc::clone(&hub));
            d.restore_admission().await?;
            d.restore_retained().await?;
            let record = records(&hub).await?.into_iter().next().ok_or_else(|| anyhow::anyhow!("quarantine missing"))?;
            assert_eq!(record.allocation.mode, ReservationMode::Whole);
            d.sched.handle(Input::RunnerUp(Box::new(RunnerIn { id: "host".into(), slots: 8, ..RunnerIn::default() })), 0);
            d.sched.handle(Input::LegQueued(Box::new(LegIn { id: "waiting".into(), repo: "other".into(), ..LegIn::default() })), 1);
            assert!(d.sched.reason("waiting").is_some(), "bad source does not prove process death");
            let proof = JobEventV2::KeptReleased { execution: "recovered".into(), attempt: 1 };
            assert!(d.retained_event("host", &job, proof.clone()).await.is_err(), "legacy quarantine has not bound this execution");
            d.retained_event("host", &job, JobEventV2::KeptOwned { execution: "recovered".into(), attempt: 1, until: 0 }).await?;
            assert!(d.sched.reason("waiting").is_some());
            d.retained_event("host", &job, proof).await?;
            assert!(d.sched.reason("waiting").is_none());
            Ok(())
        })
    }
    /// U01: a retry can move the same job id to another host while its previous attempt is
    /// still retained. Releasing the older execution cannot drop newer authorization/capacity.
    #[test]
    fn u01_retained_old_attempt_release_preserves_new_host_ownership() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let job = fixture(&hub).await?;
            quarantine_run(&hub, "retained-run").await?;
            let mut d = dispatcher(Arc::clone(&hub));
            d.restore_admission().await?;
            d.restore_retained().await?;
            d.retained_event("host", &job, JobEventV2::KeptOwned { execution: "older".into(), attempt: 1, until: 0 }).await?;
            hub.store.call(|c| { c.execute("UPDATE jobs SET agent = 'other-host', attempt = 1 WHERE run_id = 'retained-run'", [])?; Ok(()) }).await?;
            let next = Record { job: job.clone(), attempt: 2, agent: "other-host".into(), execution: Some("newer".into()), until: 0,
                allocation: Allocation { id: "retained-newer".into(), runner: "other-host".into(), request: Default::default(), mode: ReservationMode::Whole, repo: "repo".into(), actor: String::new(), lane: String::new(), parent_binding: None },
                ordinary: None, transferred: true, released: false };
            d.commit_retained(&next, false).await?;
            d.restore_retained().await?;
            assert!(hub.ci.kept_by(&job, "host") && hub.ci.kept_by(&job, "other-host"));
            assert_eq!(d.retained_inventory("host").await?.get("retained").and_then(serde_json::Value::as_array).map(Vec::len), Some(1));
            assert_eq!(d.retained_inventory("other-host").await?.get("retained").and_then(serde_json::Value::as_array).map(Vec::len), Some(1));
            assert_eq!(d.retained_inventory("intruder").await?.get("retained").and_then(serde_json::Value::as_array).map(Vec::len), Some(0));
            d.retained_event("host", &job, JobEventV2::KeptReleased { execution: "older".into(), attempt: 1 }).await?;
            assert!(!hub.ci.kept_by(&job, "host"));
            assert!(hub.ci.kept_by(&job, "other-host"));
            let held = records(&hub).await?;
            assert_eq!(held.len(), 1);
            assert_eq!(held.first().map(|r| (r.attempt, r.agent.as_str())), Some((2, "other-host")));
            assert!(d.sched.allocation("retained-newer", 1, "retained-newer".into()).is_some());
            assert!(d.retained_event("host", &job, JobEventV2::KeptReleased { execution: "newer".into(), attempt: 2 }).await.is_err());
            assert!(d.sched.allocation("retained-newer", 1, "retained-newer".into()).is_some());
            Ok(())
        })
    }

    /// U01: child cleanup is provisional until the exact physical parent journals stopped.
    #[test]
    fn u01_parent_retained_transfer_and_stopped_proof_share_exact_grant() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let job = fixture(&hub).await?;
            quarantine_run(&hub, "retained-run").await?;
            let mut record = records(&hub).await?.into_iter().next().ok_or_else(|| anyhow::anyhow!("retained fixture missing"))?;
            let binding = kiln_ci::wire::ParentBinding { executor: "executor".into(), generation: "generation".into(), allocation: "parent-allocation".into(), physical_execution: "physical".into(), job: job.clone(), attempt: 1 };
            record.allocation.parent_binding = Some(binding.clone());
            record.execution = Some("child".into());
            let mut grant = super::super::executors::ExecutorGrant { executor: binding.executor.clone(), generation: binding.generation.clone(), allocation: binding.allocation.clone(),
                physical_execution: Some(binding.physical_execution.clone()), phase: "busy".into(), binding: Some(binding.clone()), pod_identity: None, startup: None,
                snapshot: kiln_ci::sched::state::ExecutorSnapshot { executor: binding.executor.clone(), pool: "pool".into(), runner: "host".into(), reserved: Default::default(), leg: Some(job.clone()), binding: Some(binding.clone()), shape: None } };
            let key = "u01/executor/executor".to_owned();
            hub.store.kv_set(key.clone(), serde_json::to_string(&grant)?).await?;
            // Replace only fixture footprint before exercising the normal transfer commit.
            let json = serde_json::to_string(&record.allocation)?;
            hub.store.call(move |c| { c.execute("UPDATE ci_retained_ownership SET allocation = ?1", [json])?; Ok(()) }).await?;
            let mut d = dispatcher(Arc::clone(&hub));
            d.restore_admission().await?;
            d.commit_retained(&record, false).await?;
            assert_eq!(super::super::executors::executor_grant(&hub, "executor").await?.map(|g| g.phase), Some("retained".into()));
            d.stop_executor_grant("executor").await?;
            d.commit_retained(&record, false).await?;
            assert_eq!(super::super::executors::executor_grant(&hub, "executor").await?.map(|g| g.phase), Some("retained_stopping".into()), "retained transfer cannot erase an accepted Stop intent");
            d.restore_executors().await?;
            assert!(d.sched.executor_snapshot("executor").is_none(), "recovery never recreates an executor allocation beside its retained owner");
            assert!(d.next.iter().any(|command| matches!(command, super::super::Command::StopExecutor { executor, .. } if executor == "executor")), "the same retained grant replays its Stop after recovery");
            let proof = JobEventV2::KeptReleased { execution: "child".into(), attempt: 1 };
            assert!(d.retained_event("host", &job, proof).await.is_err(), "child cleanup cannot free physical parent");
            assert!(!records(&hub).await?.first().is_some_and(|r| r.released));
            assert!(d.admission.ready, "rejected guest cleanup must leave stop-proof admission open");
            grant.phase = "stopped".into();
            grant.generation = "wrong-generation".into();
            hub.store.kv_set(key.clone(), serde_json::to_string(&grant)?).await?;
            d.admission.ready = true;
            assert!(d.retained_parent_stopped(&binding).await.is_err(), "stop proof must match the full grant");
            grant.generation = binding.generation.clone();
            hub.store.kv_set(key, serde_json::to_string(&grant)?).await?;
            d.admission.ready = true;
            d.retained_parent_stopped(&binding).await?;
            assert!(records(&hub).await?.first().is_some_and(|r| r.released));
            d.retained_parent_stopped(&binding).await?;
            Ok(())
        })
    }

}
