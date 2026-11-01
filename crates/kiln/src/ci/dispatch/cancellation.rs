//! W05-R25/U03: a cancellation receipt means the current attempt's intent committed.
//! The existing dispatcher still owns its effects, and execution stop proof owns capacity.

use std::collections::VecDeque;

use super::*;

#[derive(Debug, thiserror::Error)]
pub enum CancelError {
    #[error("cancellation was not confirmed: {0}; retry the same run attempt")]
    Unavailable(&'static str),
    #[error("cancellation could not be durably confirmed: {0}; retry the same run attempt")]
    Commit(String),
    #[error("the run attempt changed before cancellation; refresh the run before retrying")]
    AttemptChanged,
    #[error("invalid cancellation: {0}")]
    Invalid(&'static str),
}

impl CancelError {
    pub(crate) fn commit(error: anyhow::Error) -> Self { Self::Commit(format!("{error:#}")) }
}

pub(super) fn was_held(report: Option<&str>) -> anyhow::Result<bool> {
    let Some(report) = report else { return Ok(false) };
    let value: serde_json::Value = serde_json::from_str(report)?;
    anyhow::ensure!(value.is_object(), "a cancellation report must be an object");
    match value.get("held") {
        None => Ok(false), // Historical cancellation reports predate this optional field.
        Some(serde_json::Value::Bool(held)) => Ok(*held),
        Some(_) => anyhow::bail!("a cancellation report has an invalid held flag"),
    }
}

#[derive(Default)]
pub(super) struct Pending {
    /// The accepted log sequence also gives cleanup legs stable ids across unknown outcomes.
    pub(super) seq: i64,
    pub(super) actions: VecDeque<PlanAction>,
    /// A scheduler cancellation can settle before its SQL write succeeds. The scheduler has
    /// consumed that command, so this same dispatcher retains the exact effect for retry.
    pub(super) settles: HashMap<String, Option<String>>,
}

#[derive(Clone)]
pub(super) struct Request {
    expected: Option<u32>,
    reason: String,
    held: bool,
}

impl Handle {
    pub async fn cancel(&self, run: &str, attempt: u32, reason: &str) -> Result<(), CancelError> {
        if !self.available() { return Err(CancelError::Unavailable("the workflow dispatcher is unavailable")); }
        if run.len() > 1024 || reason.len() > 16 * 1024 { return Err(CancelError::Invalid("the run id or reason exceeds its limit")); }
        static PENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        struct Waiter;
        impl Drop for Waiter {
            fn drop(&mut self) { PENDING.fetch_sub(1, Ordering::AcqRel); }
        }
        let mut pending = PENDING.load(Ordering::Acquire);
        loop {
            if pending >= 128 || self.tx.len() >= 128 { return Err(CancelError::Unavailable("the cancellation queue is full")); }
            match PENDING.compare_exchange_weak(pending, pending + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(actual) => pending = actual,
            }
        }
        let _waiter = Waiter;
        let (reply, rx) = async_channel::bounded(1);
        self.tx.try_send(Msg::Cancel { run: run.to_owned(), attempt, reason: reason.to_owned(), reply })
            .map_err(|_| CancelError::Unavailable("the workflow dispatcher closed"))?;
        match crate::rt::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(CancelError::Unavailable("the workflow dispatcher closed before confirming the commit")),
            Err(_) => Err(CancelError::Unavailable("the workflow dispatcher did not confirm the commit in time")),
        }
    }
}

impl Dispatch {
    pub(super) async fn accept_cancel(&mut self, run: &str, expected: Option<u32>, reason: &str) -> Result<(), CancelError> {
        if self.pipeline_retries.contains_key(run) {return Err(CancelError::Unavailable("the original pipeline retry outcome is pending"));}
        let Some(state) = self.runs.get(run) else {
            // Internal group commands for an already-retired run remain idempotent. An API
            // request must distinguish that from an unfinished plan not owned after recovery.
            let Some(expected) = expected else { return Ok(()) };
            let id = run.to_owned();
            let saved: Option<(u32, String)> = self.hub.store.call(move |c| Ok(c.query_row("SELECT attempt, status FROM runs WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await.map_err(CancelError::commit)?;
            return match saved {
                Some((attempt, _)) if attempt != expected => Err(CancelError::AttemptChanged),
                Some((_, status)) if Status::parse(&status).is_terminal() => Ok(()),
                _ => Err(CancelError::Unavailable("the unfinished plan is not owned by the dispatcher")),
            };
        };
        let proposed = state.seq.checked_add(1).ok_or_else(|| CancelError::Commit("the plan sequence is exhausted".into()))?;
        let held = state.cancel_request.as_ref().map_or(state.held.is_some(), |r| r.held);
        let expected = expected.or(state.cancel_request.as_ref().and_then(|r| r.expected));
        let reason = state.cancel_request.as_ref().map_or_else(|| reason.to_owned(), |r| r.reason.clone());
        let retry = Request { expected, reason: reason.clone(), held };
        let id = run.to_owned();
        let committed = self.hub.store.call(move |c| {
            let tx = crate::store::tx(c)?;
            let attempt: u32 = tx.query_row("SELECT attempt FROM runs WHERE id = ?1", [&id], |r| r.get(0))?;
            if expected.is_some_and(|expected| expected != attempt) { return Ok(Err(CancelError::AttemptChanged)); }
            // A timed-out database call has an unknown outcome. A retry reuses the first
            // cancellation for this attempt, rather than appending or changing its meaning.
            let existing: Option<(i64, Option<String>)> = tx.query_row("SELECT seq, report FROM ci_plan_log WHERE run_id = ?1 AND attempt = ?2 AND kind = 'cancel' ORDER BY seq LIMIT 1", crate::sql::params![id, attempt], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if let Some((seq, report)) = existing {
                let held = was_held(report.as_deref())?;
                return Ok(Ok((seq, held)));
            }
            tx.execute("INSERT INTO ci_plan_log (run_id, seq, kind, report, attempt) VALUES (?1, ?2, 'cancel', ?3, ?4)", crate::sql::params![id, proposed, serde_json::json!({ "reason": reason, "held": held }).to_string(), attempt])?;
            tx.commit()?;
            Ok(Ok((proposed, held)))
        }).await;
        let committed = match committed {
            Ok(result) => result?,
            Err(error) => {
                // A deadline has an unknown outcome. In particular, a held=true intent may
                // already be durable; its original StartRun cannot overtake confirmation.
                if let Some(state) = self.runs.get_mut(run)
                    && state.cancellation.is_none() && state.cancel_request.is_none()
                { state.cancel_request = Some(retry); }
                self.timers.insert(now_ms().saturating_add(1_000));
                return Err(CancelError::commit(error));
            }
        };
        // No sequence, held action or pure-plan state changes until the canonical call commits.
        let Some(state) = self.runs.get_mut(run) else { return Ok(()) };
        state.cancel_request = None;
        state.seq = state.seq.max(committed.0);
        if state.cancellation.is_none() {
            let actions = if committed.1 {
                state.held = None;
                state.awaiting = None;
                vec![PlanAction::Finish(PlanC::Cancelled)]
            } else {
                state.plan.on_cancel(&Source { files: &state.files, timing: &state.timing, tree: &state.tree })
            };
            state.cancellation = Some(Pending { seq: committed.0, actions: actions.into(), ..Pending::default() });
        }
        Ok(())
    }

    pub(super) fn cancellation_job_id(&self, run: &str, key: &str) -> Option<String> {
        let seq = self.runs.get(run)?.cancellation.as_ref()?.seq;
        Some(crate::util::derive_id(&["ci/cancellation-cleanup", run, &seq.to_string(), key]))
    }

    pub(super) async fn cancellation_saved(&self, run: &str, key: &str, id: &str, status: Status) -> anyhow::Result<bool> {
        if self.cancellation_job_id(run, key).is_none() { return Ok(false); }
        let Some(saved) = self.hub.store.job(id.to_owned()).await? else { return Ok(false) };
        anyhow::ensure!(saved.run_id == run && saved.key == key && saved.status == status, "the cancellation cleanup id belongs to another effect");
        Ok(true)
    }

    pub(super) async fn cancellation_result(&mut self, run: &str, key: &str, id: &str, report: &LegReport) -> anyhow::Result<bool> {
        let Some(state) = self.runs.get_mut(run).filter(|r| r.cancellation.is_some()) else { return Ok(false) };
        state.keys.insert(key.to_owned(), id.to_owned());
        let seq = state.seq.checked_add(1).ok_or_else(|| anyhow::anyhow!("the plan sequence is exhausted"))?;
        self.cancellation_report(run, seq, key, report_json(report)).await?;
        let Some(state) = self.runs.get_mut(run) else { return Ok(true) };
        state.keep.remove(key);
        state.seq = seq;
        let actions = state.plan.on_leg_finished(key, report.clone(), &Source { files: &state.files, timing: &state.timing, tree: &state.tree });
        if let Some(pending) = state.cancellation.as_mut() { pending.actions.extend(actions); }
        Ok(true)
    }

    /// The exact report may already have committed when a product call times out. A retry
    /// confirms those same bytes and attempt instead of getting stuck on the log's primary key.
    pub(super) async fn cancellation_report(&self, run: &str, seq: i64, leg: &str, report: String) -> anyhow::Result<()> {
        let (run, leg) = (run.to_owned(), leg.to_owned());
        self.hub.store.call(move |c| {
            let tx = crate::store::tx(c)?;
            let attempt: u32 = tx.query_row("SELECT attempt FROM runs WHERE id = ?1", [&run], |r| r.get(0))?;
            let existing: Option<(String, Option<String>, Option<String>, u32)> = tx.query_row("SELECT kind, leg, report, attempt FROM ci_plan_log WHERE run_id = ?1 AND seq = ?2", crate::sql::params![run, seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
            if let Some(existing) = existing {
                anyhow::ensure!(existing == ("leg".into(), Some(leg), Some(report), attempt), "the cancellation report sequence belongs to another effect");
                return Ok(());
            }
            tx.execute("INSERT INTO ci_plan_log (run_id, seq, kind, leg, report, attempt) VALUES (?1, ?2, 'leg', ?3, ?4, ?5)", crate::sql::params![run, seq, leg, report, attempt])?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(super) async fn apply_cancellation(&mut self, run: &str) {
        loop {
            let action = self.runs.get_mut(run).and_then(|r| r.cancellation.as_mut()).and_then(|p| p.actions.pop_front());
            let Some(action) = action else { break };
            if let Err(error) = Box::pin(self.actions(run, vec![action.clone()])).await {
                if let Some(pending) = self.runs.get_mut(run).and_then(|r| r.cancellation.as_mut()) { pending.actions.push_front(action); }
                self.timers.insert(now_ms().saturating_add(1_000));
                tracing::warn!(run, error = %format!("{error:#}"), "committed cancellation effects remain owned for retry");
                break;
            }
        }
    }

    pub(super) async fn retry_cancellations(&mut self) {
        let requests: Vec<(String, Request)> = self.runs.iter().filter_map(|(run, state)| state.cancel_request.clone().map(|request| (run.clone(), request))).collect();
        for (run, request) in requests {
            match self.accept_cancel(&run, request.expected, &request.reason).await {
                Ok(()) => self.apply_cancellation(&run).await,
                Err(error) => tracing::warn!(run, error = %error, "unconfirmed cancellation remains owned for retry"),
            }
        }
        let runs: Vec<String> = self.runs.iter().filter(|(_, r)| r.cancellation.as_ref().is_some_and(|p| !p.actions.is_empty() || !p.settles.is_empty())).map(|(id, _)| id.clone()).collect();
        for run in runs {
            let settles = self.runs.get_mut(&run).and_then(|r| r.cancellation.as_mut()).map(|p| std::mem::take(&mut p.settles)).unwrap_or_default();
            for (leg, reason) in settles {
                if let Err(error) = Box::pin(self.settled(&leg, Outcome::Cancelled, reason.clone())).await {
                    if let Some(pending) = self.runs.get_mut(&run).and_then(|r| r.cancellation.as_mut()) { pending.settles.insert(leg.clone(), reason); }
                    self.timers.insert(now_ms().saturating_add(1_000));
                    tracing::warn!(leg, error = %format!("{error:#}"), "committed cancellation settlement remains owned for retry");
                }
            }
            self.apply_cancellation(&run).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture(matrix: bool) -> anyhow::Result<(Arc<Hub>, Dispatch)> {
        let hub = crate::hub::tests::scratch_hub_with(|cfg| cfg.ci.enabled = true).await?;
        let repo = hub.store.upsert_repo(Repo {
            id: "cancel-repo".into(), name: "test/cancel".into(), clone_url: "file:///missing".into(),
            branches: vec!["main".into()], poll_seconds: 60, enabled: true, created_at: 0,
            last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: Some("main".into()), mirror: None,
        }).await?;
        let strategy = if matrix { "    strategy:\n      max-parallel: 1\n      matrix:\n        value: [one, two]\n" } else { "" };
        let source = format!("name: cancel\non: push\njobs:\n  work:\n    runs-on: [missing-runner]\n{strategy}    steps: [{{run: echo work}}]\n");
        let (workflow, diagnostics) = kiln_ci::wf::parse_workflow(".kiln/cancel.yaml", &source, kiln_ci::expr::Dialect::Kiln);
        let workflow = workflow.ok_or_else(|| anyhow::anyhow!("{diagnostics:?}"))?;
        runs::create(&hub, &repo, runs::NewRun {
            id: "cancel-run".into(), workflow: ".kiln/cancel.yaml".into(), name: "cancel".into(),
            event: "push".into(), branch: "main".into(), sha: "saved-sha".into(), trusted: true,
            model: serde_json::to_string(&workflow)?,
            input: serde_json::json!({ "github": { "ref": "refs/heads/main", "run_attempt": "1" }, "trusted": true, "inputs": {}, "vars": {} }).to_string(),
            ..runs::NewRun::default()
        }).await?;
        let mut dispatch = dispatcher(Arc::clone(&hub));
        dispatch.restore().await?;
        hub.ci.on.store(true, Ordering::Relaxed);
        hub.ci.workflow_on.store(true, Ordering::Relaxed);
        Ok((hub, dispatch))
    }

    fn dispatcher(hub: Arc<Hub>) -> Dispatch {
        Dispatch {
            sched: Scheduler::new(sched_config(&hub, Vec::new())), hub, routing: Default::default(),
            held: HashMap::new(), runs: HashMap::new(), legs: HashMap::new(), timers: BTreeSet::new(), next: Vec::new(),
            in_commands: false, pods: HashSet::new(), absent_after: None, decisions: HashMap::new(), warm: HashMap::new(),
            warm_seq: 0, admission: admission::State::default(), pending_losses: HashMap::new(), log_settle: Default::default(), pipeline_retries:Default::default(),
        }
    }

    /// W05-R25/U03: failed acceptance cannot consume the plan; acknowledged intent survives a
    /// crash before any effect, and replay settles waiting legs rather than starting them.
    #[test]
    fn u03_cancel_ack_requires_commit_and_recovers_the_same_attempt() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (hub, mut dispatch) = fixture(true).await?;
            hub.ci.workflow_on.store(false, Ordering::Relaxed);
            assert!(hub.ci.available() && !hub.ci.enabled(), "U03: disabled ingress still permits lifecycle cancellation");
            let rx = hub.ci.rx.lock().take().ok_or_else(|| anyhow::anyhow!("missing fixture receiver"))?;
            let legacy = hub.store.cancel_run("cancel-run".into(), "wrong controller".into()).await;
            let refusal = legacy.err().ok_or_else(|| anyhow::anyhow!("legacy mutation accepted a canonical run"))?;
            assert!(refusal.is::<crate::store::CanonicalRunCancellation>(), "legacy mutation must preserve its typed ownership refusal: {refusal:#}");
            let before = dispatch.runs.get("cancel-run").map(|r| (r.seq, r.plan.jobs())).ok_or_else(|| anyhow::anyhow!("missing fixture plan"))?;
            // A real SQL constraint refuses the cancel insert. No fake store or receipt path.
            hub.store.call(|c| {
                c.execute("INSERT INTO ci_plan_log (run_id, seq, kind, attempt) VALUES ('cancel-run', 999, 'refuse-cancel', 1)", [])?;
                c.execute("CREATE UNIQUE INDEX u03_refuse_cancel_log ON ci_plan_log(run_id)", [])?;
                Ok(())
            }).await?;
            let (answer, handled) = futures::join!(hub.cancel_run("cancel-run", "first request"), async {
                dispatch.handle(rx.recv().await?).await;
                Ok::<(), anyhow::Error>(())
            });
            handled?;
            assert!(matches!(answer, Err(CancelError::Commit(_))), "refused log inserts cannot be acknowledged");
            let unchanged = dispatch.runs.get("cancel-run").ok_or_else(|| anyhow::anyhow!("plan disappeared after refused commit"))?;
            assert_eq!((unchanged.seq, unchanged.plan.jobs()), before);
            assert!(unchanged.cancellation.is_none());
            hub.store.call(|c| {
                c.execute("DROP INDEX u03_refuse_cancel_log", [])?;
                c.execute("DELETE FROM ci_plan_log WHERE kind = 'refuse-cancel'", [])?;
                Ok(())
            }).await?;
            // Crash at the actual request/reply boundary: commit and return the receipt, while
            // deliberately withholding apply_cancellation, exactly what a process loss does.
            let (answer, staged) = futures::join!(hub.cancel_run("cancel-run", "accepted request"), async {
                let Msg::Cancel { run, attempt, reason, reply } = rx.recv().await? else { anyhow::bail!("unexpected fixture message") };
                let result = dispatch.accept_cancel(&run, Some(attempt), &reason).await;
                let _ = reply.try_send(result);
                Ok::<(), anyhow::Error>(())
            });
            answer?;
            staged?;
            assert!(hub.store.jobs("cancel-run".into(), false).await?.iter().any(|j| !j.status.is_terminal()), "intent receipt is independent of physical completion");
            // A lost response may be retried with a different reason; the first intent wins.
            dispatch.accept_cancel("cancel-run", Some(1), "duplicate request").await?;
            let logs: Vec<String> = hub.store.call(|c| {
                let mut query = c.prepare("SELECT report FROM ci_plan_log WHERE run_id = 'cancel-run' AND kind = 'cancel'")?;
                Ok(query.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?)
            }).await?;
            assert_eq!(logs.len(), 1);
            assert!(logs.first().is_some_and(|report| report.contains("first request")), "the original unconfirmed request remains owned for retry");
            assert!(matches!(dispatch.accept_cancel("cancel-run", Some(2), "stale attempt").await, Err(CancelError::AttemptChanged)));
            drop(dispatch);
            let mut recovered = dispatcher(Arc::clone(&hub));
            recovered.restore().await?;
            assert_eq!(hub.store.run("cancel-run".into()).await?.map(|r| r.status), Some(Status::Cancelled));
            let jobs = hub.store.jobs("cancel-run".into(), false).await?;
            assert_eq!(jobs.len(), 2);
            assert!(jobs.iter().all(|j| j.status == Status::Cancelled));
            hub.ci.on.store(false, Ordering::Relaxed);
            hub.cancel_run("cancel-run", "already completed").await?;
            Ok(())
        })
    }

    /// W05-R25/U03: a held run's accepted cancellation survives restart without replaying its
    /// initial queue, and unavailable dispatch cannot fall through to legacy SQL cancellation.
    #[test]
    fn u03_cancel_held_recovery_and_unavailable_dispatch_keep_canonical_ownership() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (hub, mut dispatch) = fixture(false).await?;
            hub.store.call(|c| {
                c.execute("DELETE FROM steps WHERE job_id IN (SELECT id FROM jobs WHERE run_id = 'cancel-run')", [])?;
                c.execute("DELETE FROM ci_attempts WHERE leg_id IN (SELECT id FROM jobs WHERE run_id = 'cancel-run')", [])?;
                c.execute("DELETE FROM jobs WHERE run_id = 'cancel-run'", [])?;
                Ok(())
            }).await?;
            dispatch.legs.clear();
            let state = dispatch.runs.get_mut("cancel-run").ok_or_else(|| anyhow::anyhow!("missing held fixture"))?;
            state.keys.clear();
            state.held = Some(Vec::new());
            hub.ci.on.store(false, Ordering::Relaxed);
            hub.ci.runs.lock().clear();
            assert!(matches!(hub.cancel_run("cancel-run", "not accepted").await, Err(CancelError::Unavailable(_))));
            assert!(!hub.store.run("cancel-run".into()).await?.is_some_and(|r| r.status.is_terminal()));
            hub.store.call(|c| {
                c.execute("INSERT INTO ci_plan_log (run_id, seq, kind, attempt) VALUES ('cancel-run', 999, 'refuse-cancel', 1)", [])?;
                c.execute("CREATE UNIQUE INDEX u03_refuse_held_cancel ON ci_plan_log(run_id)", [])?;
                Ok(())
            }).await?;
            assert!(matches!(dispatch.accept_cancel("cancel-run", Some(1), "held cancellation").await, Err(CancelError::Commit(_))));
            dispatch.commands(vec![Command::StartRun { run: "cancel-run".into() }]).await;
            assert!(dispatch.runs.get("cancel-run").is_some_and(|r| r.held.is_some()), "an unconfirmed held intent fences its original StartRun");
            // The database committed, but its response was lost: replay the same intent rather
            // than letting the held run start or appending a second cancel sequence.
            hub.store.call(|c| {
                c.execute("DROP INDEX u03_refuse_held_cancel", [])?;
                c.execute("DELETE FROM ci_plan_log WHERE kind = 'refuse-cancel'", [])?;
                c.execute("INSERT INTO ci_plan_log (run_id, seq, kind, report, attempt) VALUES ('cancel-run', 1, 'cancel', '{\"reason\":\"held cancellation\",\"held\":true}', 1)", [])?;
                Ok(())
            }).await?;
            dispatch.accept_cancel("cancel-run", Some(1), "held cancellation").await?;
            drop(dispatch);
            let mut recovered = dispatcher(Arc::clone(&hub));
            recovered.restore().await?;
            assert_eq!(hub.store.run("cancel-run".into()).await?.map(|r| r.status), Some(Status::Cancelled));
            assert!(hub.store.jobs("cancel-run".into(), false).await?.is_empty(), "held cancellation cannot start its withheld initial actions");
            let handle = Handle::default();
            assert!(matches!(handle.cancel("run", 1, "disabled").await, Err(CancelError::Unavailable(_))));
            handle.on.store(true, Ordering::Relaxed);
            handle.workflow_on.store(true, Ordering::Relaxed);
            drop(handle.rx.lock().take());
            assert!(matches!(handle.cancel("run", 1, "closed").await, Err(CancelError::Unavailable(_))));
            Ok(())
        })
    }

    /// W05-R25/U03: after acceptance, a consumed scheduler settlement whose SQL write fails
    /// stays owned by the existing dispatch loop and succeeds on its next retry.
    #[test]
    fn u03_cancel_committed_settlement_retries_without_another_controller() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (hub, mut dispatch) = fixture(true).await?;
            // A real unique constraint permits the live row, then refuses its cancellation.
            hub.store.call(|c| {
                c.execute("INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec) SELECT 'cancel-blocker', run_id, 'blocker', display, 'cancelled', labels, needs, spec FROM jobs WHERE run_id = 'cancel-run' LIMIT 1", [])?;
                c.execute("CREATE UNIQUE INDEX u03_refuse_cancel_settle ON jobs(status)", [])?;
                Ok(())
            }).await?;
            dispatch.accept_cancel("cancel-run", Some(1), "durable intent").await?;
            dispatch.apply_cancellation("cancel-run").await;
            assert!(dispatch.runs.get("cancel-run").and_then(|r| r.cancellation.as_ref()).is_some_and(|p| !p.settles.is_empty()), "a failed consumed scheduler command must retain its exact settlement");
            hub.store.call(|c| {
                c.execute("DROP INDEX u03_refuse_cancel_settle", [])?;
                c.execute("DELETE FROM jobs WHERE id = 'cancel-blocker'", [])?;
                Ok(())
            }).await?;
            dispatch.tick().await;
            assert_eq!(hub.store.run("cancel-run".into()).await?.map(|r| r.status), Some(Status::Cancelled));
            let jobs = hub.store.jobs("cancel-run".into(), false).await?;
            assert_eq!(jobs.len(), 2);
            assert!(jobs.iter().all(|j| j.status == Status::Cancelled));
            let count: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ci_plan_log WHERE run_id = 'cancel-run' AND kind = 'cancel'", [], |r| r.get(0))?)).await?;
            assert_eq!(count, 1);
            Ok(())
        })
    }
}
