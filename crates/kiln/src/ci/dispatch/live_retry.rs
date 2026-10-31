//! U01/W05-R28: one in-place pipeline retry, owned by the existing dispatcher.
//! Durable log/reset acceptance precedes acknowledgment. Unknown keeps the SAME
//! selected queue pause and intent; no scheduler clone replaces live siblings.
use super::*;
use kiln_ci::plan::retry::Selection;
use kiln_ci::sched::state::PipelineRetryPause;
use crate::sql::{Connection,params};

const KIND:&str="retry-subgraph";
const REFUSED:&str="retry-subgraph-refused";
const READER_EPOCH:u32=6;

#[derive(Debug,thiserror::Error)]
#[error("unsupported durable plan-log kind `{0}`; replay remains fenced")]
pub(super) struct UnknownPlanLogKind(pub(super) String);

pub(super) fn known_log_kind(kind:&str)->Result<(),UnknownPlanLogKind> {
    if matches!(kind,"leg"|"cancel"|KIND|REFUSED) {Ok(())}else{Err(UnknownPlanLogKind(kind.into()))}
}

#[derive(Clone,Debug,serde::Serialize,serde::Deserialize)]
pub(super) struct Job {
    id:String,
    key:String,
    status:String,
    from:u32,
    next:u32,
    placement:Option<kiln_ci::sched::Placement>,
}
#[derive(Clone,Debug,serde::Serialize,serde::Deserialize)]
pub(super) struct Intent {
    pub(super) version:u32,
    pub(super) selection:Selection,
    run:String,
    run_attempt:u32,
    seq:i64,
    actor:String,
    workflow:String,
    jobs:Vec<Job>,
}

pub(super) struct Pending {
    intent:Intent,
    /// SAME original Store/owner and acknowledged application writer floor.
    boundary:Arc<crate::store::StoreWriterBoundary>,
    pause:PipelineRetryPause,
    committed:bool,
    applied:bool,
    /// Derived from the pre-retry plan while its SQL outcome is unresolved.
    /// Commit supersedes only these selected logical effects. Noncommit resumes
    /// them; neither branch forgets an exposed process or external operation.
    old_actions:std::collections::VecDeque<PlanAction>,
    actions:std::collections::VecDeque<PlanAction>,
    error:Option<String>,
    pub(super) cancel:Option<String>,
    rejected:bool,
    resumed:bool,
}

/// Inspect the actual original execution proof, never just terminal job status.
fn physically_settled(c:&Connection<'_>,job:&Job)->anyhow::Result<()> {
    let unresolved:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_ownership WHERE job=?1 AND released=0)",[&job.id],|r|r.get(0))?;
    anyhow::ensure!(!unresolved,"`{}` still has an original physical owner",job.key);
    let terminal=Status::parse(&job.status).is_terminal();
    if !terminal {return Ok(());}
    let durable:Option<(String,String,Option<String>)>=c.query_row("SELECT physical_receipt,status,runner FROM ci_attempts WHERE leg_id=?1 AND number=?2 AND physical_receipt IS NOT NULL",params![job.id,job.from],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((json,status,runner))=durable {
        let journal=ordinary::journal_json(&json)?.ok_or_else(||anyhow::anyhow!("original physical receipt is not an ordinary owner"))?;
        anyhow::ensure!(status=="done" && journal.owner.job==job.id && journal.owner.attempt==job.from
            && runner.as_deref()==Some(journal.owner.host.as_str()) && matches!(journal.physical.as_str(),"stopped"|"no_start"),
            "`{}` has a mismatched original durable physical receipt",job.key);
        return Ok(());
    }
    let legacy:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_receipts p JOIN ci_attempts a ON a.leg_id=p.job AND a.number=p.attempt AND a.runner=p.agent WHERE p.job=?1 AND p.attempt=?2 AND p.released=1 AND a.status='done')",params![job.id,job.from],|r|r.get(0))?;
    if legacy {return Ok(());}
    let row:Option<(String,Option<String>,bool)>=c.query_row("SELECT allocation,execution,released FROM ci_retained_ownership WHERE job=?1 AND attempt=?2",params![job.id,job.from],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((json,execution,released))=row {
        anyhow::ensure!(released,"`{}` has not physically retired",job.key);
        if let Some(journal)=ordinary::journal_json(&json)? {
            anyhow::ensure!(journal.owner.job==job.id && journal.owner.attempt==job.from
                && execution.as_deref()==Some(journal.owner.execution.as_str())
                && matches!(journal.physical.as_str(),"stopped"|"no_start") && journal.logical!="running",
                "`{}` lacks its original terminal physical receipt",job.key);
            return Ok(());
        }
        let Some(execution)=execution else {anyhow::bail!("`{}` has unsupported historical physical ownership",job.key)};
        let positive:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_receipts WHERE job=?1 AND attempt=?2 AND execution=?3 AND released=1)",params![job.id,job.from,execution],|r|r.get(0))?;
        anyhow::ensure!(positive,"`{}` lacks its original authenticated release receipt",job.key);
        return Ok(());
    }
    // Imported skipped dependents were never executed. Their imported attempt
    // records must agree with that specific lifecycle; a completed target is
    // never permitted through missing-journal inference.
    if job.status=="skipped" {
        let unexposed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_attempts WHERE leg_id=?1 AND number=?2 AND status IN ('pending','queued','done') AND started_at IS NULL AND (runner IS NULL OR runner='-') AND EXISTS(SELECT 1 FROM jobs j WHERE j.id=leg_id AND j.attempt=number AND j.status='skipped' AND j.started_at IS NULL AND (j.agent IS NULL OR j.agent='-')))",params![job.id,job.from],|r|r.get(0))?;
        anyhow::ensure!(unexposed,"`{}` has no accepted never-exposed skip",job.key);
        return Ok(());
    }
    anyhow::bail!("`{}` needs its original physical retirement proof before in-place retry; historical ownership is unsupported",job.key)
}

impl Intent {
    fn prepare(c:&Connection<'_>,selection:Selection,run:String,seq:i64,actor:String,workflow:String,placed:HashMap<String,kiln_ci::sched::Placement>)->anyhow::Result<Self> {
        let run_attempt:u32=c.query_row("SELECT attempt FROM runs WHERE id=?1",[&run],|r|r.get(0))?;
        let mut jobs=Vec::new();
        for key in &selection.jobs {
            let (id,status,from,old):(String,String,u32,Option<String>)=c.query_row("SELECT id,status,attempt,placement FROM jobs WHERE run_id=?1 AND key=?2 AND run_attempt=?3 AND job_id IS NOT NULL",params![run,key,run_attempt],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            anyhow::ensure!(status!="running","`{key}` needs the target and is running");
            anyhow::ensure!(from>0 && matches!(status.as_str(),"pending"|"queued"|"waiting"|"succeeded"|"failed"|"cancelled"|"skipped"|"timed_out"),"`{key}` has unsupported original job state");
            let terminal=Status::parse(&status).is_terminal();
            let next=if terminal {from.checked_add(1).ok_or_else(||anyhow::anyhow!("job attempt is exhausted"))?}else{from};
            let placement=if key==&selection.target {placed.get(key).cloned()}else if terminal {None}else{old.map(|p|serde_json::from_str(&p)).transpose()?};
            jobs.push(Job {id,key:key.clone(),status,from,next,placement});
        }
        let target=jobs.iter().find(|j|j.key==selection.target).ok_or_else(||anyhow::anyhow!("retry target is absent"))?;
        anyhow::ensure!(Status::parse(&target.status).is_terminal() && target.status!="skipped","the retry target has not finished or was skipped");
        let intent=Intent {version:1,selection,run,run_attempt,seq,actor,workflow,jobs};
        intent.check_run(c)?;
        for job in &intent.jobs {physically_settled(c,job)?;}
        Ok(intent)
    }

    fn check_run(&self,c:&Connection<'_>)->anyhow::Result<()> {
        anyhow::ensure!(self.version==1,"unsupported original retry intent version");
        let (attempt,status,workflow,input):(u32,String,String,String)=c.query_row("SELECT r.attempt,r.status,p.workflow,p.input FROM runs r JOIN ci_plans p ON p.run_id=r.id WHERE r.id=?1",[&self.run],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        anyhow::ensure!(attempt==self.run_attempt && matches!(status.as_str(),"queued"|"pending"|"running"),"the original active run attempt changed");
        let input:serde_json::Value=serde_json::from_str(&input)?;
        anyhow::ensure!(input.get("legacy_import").and_then(serde_json::Value::as_bool)==Some(true),"the run is not an imported expanded pipeline");
        let workflow:Workflow=serde_json::from_str(&workflow)?;
        anyhow::ensure!(serde_json::to_string(&workflow)?==self.workflow,"the original pipeline workflow changed");
        let cancelled:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_plan_log WHERE run_id=?1 AND attempt=?2 AND kind='cancel')",params![self.run,self.run_attempt],|r|r.get(0))?;
        anyhow::ensure!(!cancelled,"the original run attempt has a cancellation intent");
        Ok(())
    }

    fn check_jobs(&self,c:&Connection<'_>)->anyhow::Result<()> {
        self.check_run(c)?;
        for job in &self.jobs {
            let row:(String,u32,String,u32)=c.query_row("SELECT status,attempt,key,run_attempt FROM jobs WHERE id=?1 AND run_id=?2 AND job_id IS NOT NULL",params![job.id,self.run],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            anyhow::ensure!(row==(job.status.clone(),job.from,job.key.clone(),self.run_attempt),"`{}` changed before retry acceptance",job.key);
            anyhow::ensure!(job.status!="running","`{}` needs the target and is still running",job.key);
            physically_settled(c,job)?;
        }
        Ok(())
    }

    /// A durable exact negative CAS fences a delayed original commit. An empty
    /// read alone cannot release the original pause: that call may still arrive.
    fn reconcile(&self,c:&Connection<'_>)->anyhow::Result<bool> {
        let tx=crate::store::tx(c)?;
        let encoded=serde_json::to_string(self)?;
        let previous:Option<(String,Option<String>)>=tx.query_row("SELECT kind,report FROM ci_plan_log WHERE run_id=?1 AND seq=?2",params![self.run,self.seq],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let accepted=if let Some((kind,report))=previous {
            anyhow::ensure!((kind==KIND || kind==REFUSED) && report.as_deref()==Some(encoded.as_str()),"the uncertain retry sequence belongs to another exact intent");
            kind==KIND
        }else {
            self.check_jobs(&tx)?;
            tx.execute("INSERT INTO ci_plan_log(run_id,seq,kind,leg,report,attempt) VALUES(?1,?2,?3,?4,?5,?6)",params![self.run,self.seq,REFUSED,self.selection.target,encoded,self.run_attempt])?;
            false
        };
        tx.commit()?;
        Ok(accepted)
    }

    /// Original intent/reset and every selected row effect commit together.
    fn commit(&self,c:&Connection<'_>)->anyhow::Result<()> {
        let tx=crate::store::tx(c)?;
        let encoded=serde_json::to_string(self)?;
        let previous:Option<(String,Option<String>)>=tx.query_row("SELECT kind,report FROM ci_plan_log WHERE run_id=?1 AND seq=?2",params![self.run,self.seq],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((kind,report))=previous {
            anyhow::ensure!(kind==KIND && report.as_deref()==Some(encoded.as_str()),"the retry sequence belongs to another exact intent");
            return Ok(());
        }
        self.check_jobs(&tx)?;
        let mut input:String=tx.query_row("SELECT input FROM ci_plans WHERE run_id=?1",[&self.run],|r|r.get(0))?;
        let mut value:serde_json::Value=serde_json::from_str(&input)?;
        let object=value.as_object_mut().ok_or_else(||anyhow::anyhow!("saved run input is not an object"))?;
        let mut placed:HashMap<String,kiln_ci::sched::Placement>=object.get("placed").map(|v|serde_json::from_value(v.clone())).transpose()?.unwrap_or_default();
        let mut fresh:HashSet<String>=object.get("retry_fresh").map(|v|serde_json::from_value(v.clone())).transpose()?.unwrap_or_default();
        for job in &self.jobs {
            match &job.placement {Some(p)=>{placed.insert(job.key.clone(),p.clone());},None=>{placed.remove(&job.key);}}
            fresh.insert(job.key.clone());
            let placement=job.placement.as_ref().map(serde_json::to_string).transpose()?;
            let n=tx.execute("UPDATE jobs SET status='pending',attempt=?2,agent=NULL,executor=NULL,queued_at=NULL,started_at=NULL,finished_at=NULL,exit_code=NULL,error=NULL,peak_rss=NULL,cpu_seconds=NULL,conclusion=NULL,outputs=NULL,input_hash=NULL,reused_from=NULL,wait_reason=NULL,wait_since=NULL,placement=?3 WHERE id=?1 AND attempt=?4 AND status=?5",
                params![job.id,job.next,placement,job.from,job.status])?;
            anyhow::ensure!(n==1,"retry lost the exact selected job attempt");
            tx.execute("UPDATE steps SET status='pending',started_at=NULL,finished_at=NULL,exit_code=NULL,outcome=NULL,conclusion=NULL,outputs=NULL,attempt=?2 WHERE job_id=?1",params![job.id,job.next])?;
        }
        object.insert("placed".into(),serde_json::to_value(placed)?);
        object.insert("retry_fresh".into(),serde_json::to_value(fresh)?);
        input=serde_json::to_string(&value)?;
        tx.execute("UPDATE ci_plans SET input=?2,done=0 WHERE run_id=?1",params![self.run,input])?;
        tx.execute("INSERT INTO ci_plan_log(run_id,seq,kind,leg,report,attempt) VALUES(?1,?2,?3,?4,?5,?6)",params![self.run,self.seq,KIND,self.selection.target,encoded,self.run_attempt])?;
        tx.commit()?;
        Ok(())
    }
}

impl Dispatch {
    pub(super) fn defer_pipeline_retry_action(&mut self,run:&str,action:&PlanAction)->bool {
        let Some(pending)=self.pipeline_retries.get_mut(run).filter(|p|!p.applied && !p.resumed) else{return false};
        let selected=match action {
            PlanAction::Queue(leg)|PlanAction::Settle {leg,..}=>pending.intent.selection.jobs.contains(&leg.job),
            PlanAction::Job {job,..}=>pending.intent.selection.jobs.contains(job),
            PlanAction::Cancel {leg,..}|PlanAction::Warn {leg,..}=>pending.intent.selection.jobs.contains(leg),
            PlanAction::Finish(_)=>true,
        };
        if selected {pending.old_actions.push_back(action.clone());}
        selected
    }

    pub(super) async fn retry_active_pipeline(&mut self,run:&str,key:&str,placed:HashMap<String,kiln_ci::sched::Placement>,by:&str)->anyhow::Result<u32> {
        anyhow::ensure!(!self.pipeline_retries.contains_key(run),"the original retry outcome is still pending");
        anyhow::ensure!(self.pipeline_retries.len()<128,"the dispatcher retry queue is full; no retry was accepted");
        let state=self.runs.get(run).ok_or_else(||anyhow::anyhow!("the dispatcher no longer owns this run"))?;
        anyhow::ensure!(state.cancellation.is_none() && state.cancel_request.is_none() && state.held.is_none() && state.awaiting.is_none(),"the run is held or cancelling");
        let selection=state.plan.pipeline_retry_selection(key).map_err(anyhow::Error::msg)?;
        let seq=state.seq.checked_add(1).ok_or_else(||anyhow::anyhow!("plan log sequence is exhausted"))?;
        let workflow=serde_json::to_string(&*state.wf)?;
        let (run_id,actor)=(run.to_owned(),by.to_owned());
        // The semantic reader migration is already acknowledged by this Store's
        // initializer. Fence actual old attachments before any pause or reset.
        // A replica/remote Store has no such positive producer and refuses here.
        let store=self.hub.store.clone();
        let boundary=Arc::new(crate::rt::blocking_in(crate::rt::SHARE_STORE,move||store.writer_boundary(READER_EPOCH)).await??);
        boundary.require_store(&self.hub.store)?;
        anyhow::ensure!(boundary.version()>=READER_EPOCH,"the original Store writer floor is too old");
        let intent=self.hub.store.call(move|c|Intent::prepare(c,selection,run_id,seq,actor,workflow,placed)).await?;
        for job in &intent.jobs {
            anyhow::ensure!(!self.legs.get(&job.id).is_some_and(|l|l.attempt!=job.from || l.agent.is_some()),"`{}` retains a live original execution",job.key);
            anyhow::ensure!(!self.held.contains_key(&job.id),"`{}` is held by an environment owner",job.key);
        }
        let expected:Vec<(String,u32)>=intent.jobs.iter().map(|j|(j.id.clone(),j.from)).collect();
        let identity=format!("{}:{}:{}",run,intent.run_attempt,seq);
        let pause=self.sched.pause_pipeline_retry(identity,&expected).map_err(anyhow::Error::msg)?;
        let number=intent.run_attempt;
        // Reserve even an unknown sequence: unrelated completion logs continue
        // after it, never overwriting the original accepted/uncertain intent.
        if let Some(state)=self.runs.get_mut(run) {state.seq=seq;}
        self.pipeline_retries.insert(run.into(),Pending {intent:intent.clone(),boundary:Arc::clone(&boundary),pause,committed:false,applied:false,old_actions:Default::default(),actions:Default::default(),error:None,cancel:None,rejected:false,resumed:false});
        // A quiet message queue must still reconcile this SAME accepted turn.
        self.timers.insert(now_ms().saturating_add(1_000));
        let store=self.hub.store.clone();
        let result=self.hub.store.call(move|c|{boundary.require_store(&store)?;intent.commit(c)}).await;
        match result {
            Ok(())=>{if let Some(p)=self.pipeline_retries.get_mut(run) {p.committed=true;}},
            Err(error)=>{
                if let Some(p)=self.pipeline_retries.get_mut(run) {p.error=Some(format!("{error:#}"));}
                // Do not resubmit, resume or replace an uncertain original turn.
                return Err(error.context("retry acceptance was not confirmed; its original queue pause is retained"));
            },
        }
        self.resolve_pipeline_retry(run).await?;
        Ok(number)
    }

    pub(super) async fn retry_pipeline_intents(&mut self) {
        let runs:Vec<String>=self.pipeline_retries.keys().cloned().collect();
        for run in runs {
            if let Err(error)=Box::pin(self.resolve_pipeline_retry(&run)).await {
                tracing::warn!(run,error=%format!("{error:#}"),"original pipeline retry remains held by its dispatcher");
            }
        }
    }

    pub(super) fn arm_pipeline_retry_timer(&mut self) {
        // Called at the END of tick: slow original maintenance/effects must not
        // consume this fresh wake during that tick's expired-timer sweep.
        // Reuse the existing wheel, not a new worker or blind resubmission.
        // Permanently unsupported physical conflicts stay owned without a spin.
        if self.pipeline_retries.values().any(|p|(p.applied || p.resumed) || self.sched.pipeline_retry_conflict(&p.pause).is_none()) {
            self.timers.insert(now_ms().saturating_add(1_000));
        }
    }

    pub(super) fn fence_pipeline_retry_conflicts(&mut self) {
        if self.pipeline_retries.values().any(|p|!p.applied && !p.resumed && self.sched.pipeline_retry_conflict(&p.pause).is_some()) {self.admission.ready=false;}
    }

    async fn retire_pipeline_retry(&mut self,run:&str)->anyhow::Result<Pending> {
        let pending=self.pipeline_retries.remove(run).ok_or_else(||anyhow::anyhow!("resolved retry custody disappeared"))?;
        if let Some(reason)=&pending.cancel && let Err(error)=Box::pin(self.cancel_run(run,reason)).await {
            // The existing cancellation owner retains Unknown and committed
            // effects. A pre-submission failure must instead return this SAME
            // original debt to Pending; removal alone proves no transfer.
            let transferred=self.runs.get(run).is_none_or(|state|state.cancel_request.is_some() || state.cancellation.is_some());
            if !transferred {self.pipeline_retries.insert(run.into(),pending);}
            return Err(error);
        }
        Ok(pending)
    }

    async fn resolve_pipeline_retry(&mut self,run:&str)->anyhow::Result<()> {
        let Some(pending)=self.pipeline_retries.get(run) else{return Ok(())};
        if !pending.applied && !pending.resumed && let Some(conflict)=self.sched.pipeline_retry_conflict(&pending.pause) {
            let refusal=conflict.status();
            self.admission.ready=false;
            return Err(refusal.into());
        }
        if !self.runs.contains_key(run) {
            let id=run.to_owned();
            let exists:bool=self.hub.store.call(move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE id=?1)",[id],|r|r.get(0))?)).await?;
            anyhow::ensure!(!exists,"the original retry run still exists without its dispatcher owner");
            // A confirmed canonical deletion supersedes only these unexposed
            // selected logical effects. Existing repo_deleted still owns every
            // unrelated running execution and its retained physical debt.
            if !pending.applied && !pending.resumed {self.sched.commit_pipeline_retry(&pending.pause).map_err(anyhow::Error::msg)?;}
            self.pipeline_retries.remove(run);
            return Ok(());
        }
        if !pending.committed && !pending.rejected {
            let intent=pending.intent.clone();
            let boundary=Arc::clone(&pending.boundary);
            let store=self.hub.store.clone();
            let accepted=self.hub.store.call(move|c|{boundary.require_store(&store)?;intent.reconcile(c)}).await?;
            if let Some(pending)=self.pipeline_retries.get_mut(run) {
                if accepted {pending.committed=true;}else{pending.rejected=true;}
            }
        }
        if self.pipeline_retries.get(run).is_some_and(|p|p.rejected) {
            let pending=self.pipeline_retries.get(run).ok_or_else(||anyhow::anyhow!("rejected retry custody disappeared"))?;
            if !pending.resumed {
                let commands=self.sched.resume_pipeline_retry(&pending.pause,now_ms()).map_err(anyhow::Error::msg)?;
                if let Some(p)=self.pipeline_retries.get_mut(run) {p.resumed=true;}
                if let Some(commands)=self.after(commands) {Box::pin(self.commands(commands)).await;}
            }
            // Turn off old-plan deferral only after the SAME original pause
            // resumed. Keep each action until its original effect succeeds.
            while let Some(action)=self.pipeline_retries.get(run).and_then(|p|p.old_actions.front()).cloned() {
                Box::pin(self.actions(run,vec![action])).await?;
                if let Some(p)=self.pipeline_retries.get_mut(run) {p.old_actions.pop_front();}
            }
            let pending=self.retire_pipeline_retry(run).await?;
            anyhow::bail!("original retry did not commit: {}",pending.error.unwrap_or_else(||"acceptance was refused".into()));
        }
        let pending=self.pipeline_retries.get(run).ok_or_else(||anyhow::anyhow!("committed retry custody disappeared"))?;
        if !pending.applied {
            let intent=pending.intent.clone();
            let state=self.runs.get(run).ok_or_else(||anyhow::anyhow!("committed retry's original run owner is unavailable"))?;
            let mut plan=state.plan.clone();
            let actions=plan.retry_pipeline(&intent.selection,&Source {files:&state.files,timing:&state.timing,tree:&state.tree}).map_err(anyhow::Error::msg)?;
            self.sched.commit_pipeline_retry(&pending.pause).map_err(anyhow::Error::msg)?;
            for job in &intent.jobs {self.legs.remove(&job.id);self.decisions.remove(&job.id);}
            let state=self.runs.get_mut(run).ok_or_else(||anyhow::anyhow!("committed retry run owner disappeared"))?;
            state.plan=plan;
            for job in &intent.jobs {
                state.keep.remove(&job.key);
                state.retry_fresh.insert(job.key.clone());
                match &job.placement {Some(p)=>{state.placed.insert(job.key.clone(),p.clone());},None=>{state.placed.remove(&job.key);}}
            }
            let pending=self.pipeline_retries.get_mut(run).ok_or_else(||anyhow::anyhow!("applied retry custody disappeared"))?;
            pending.applied=true;
            pending.old_actions.clear(); // positively superseded by this accepted reset
            pending.actions.extend(actions);
        }
        while let Some(action)=self.pipeline_retries.get(run).and_then(|p|p.actions.front()).cloned() {
            Box::pin(self.actions(run,vec![action])).await?;
            if let Some(p)=self.pipeline_retries.get_mut(run) {p.actions.pop_front();}
        }
        self.retire_pipeline_retry(run).await?;
        Ok(())
    }
}

pub(super) struct Log<'a> {
    pub(super) run:&'a str,
    pub(super) attempt:u32,
    pub(super) seq:i64,
    pub(super) leg:Option<&'a str>,
    pub(super) report:Option<&'a str>,
    pub(super) workflow:&'a Workflow,
}
impl Log<'_> {
    fn intent(&self)->anyhow::Result<Intent> {
        let intent:Intent=serde_json::from_str(self.report.ok_or_else(||anyhow::anyhow!("retry log has no original intent"))?)?;
        anyhow::ensure!(intent.version==1 && intent.run==self.run && intent.run_attempt==self.attempt && intent.seq==self.seq
            && self.leg==Some(intent.selection.target.as_str()) && !intent.actor.is_empty()
            && intent.workflow==serde_json::to_string(self.workflow)?,"retry log does not match its exact original run/sequence/workflow");
        let keys:Vec<String>=intent.jobs.iter().map(|job|job.key.clone()).collect();
        let ids:HashSet<&str>=intent.jobs.iter().map(|job|job.id.as_str()).collect();
        anyhow::ensure!(!intent.jobs.is_empty() && keys==intent.selection.jobs && ids.len()==intent.jobs.len()
            && intent.jobs.iter().all(|job|!job.id.is_empty() && job.from>0 && matches!(job.status.as_str(),"pending"|"queued"|"waiting"|"succeeded"|"failed"|"cancelled"|"skipped"|"timed_out")
                && job.next==if Status::parse(&job.status).is_terminal() {job.from.checked_add(1).unwrap_or(0)}else{job.from}),
            "retry log does not retain its exact selected prior attempts");
        let target=intent.jobs.iter().find(|job|job.key==intent.selection.target).ok_or_else(||anyhow::anyhow!("retry log target is missing"))?;
        anyhow::ensure!(Status::parse(&target.status).is_terminal() && target.status!="skipped","retry log target was not a completed execution");
        Ok(intent)
    }
}

pub(super) fn replay(log:Log<'_>,plan:&mut RunPlan,source:&dyn PlanSource,recovered:&mut Vec<PlanAction>,reported:&mut HashSet<String>)->anyhow::Result<()> {
    let intent=log.intent()?;
    let actions=plan.retry_pipeline(&intent.selection,source).map_err(anyhow::Error::msg)?;
    recovered.retain(|action|match action {
        PlanAction::Queue(leg)|PlanAction::Settle {leg,..}=>!intent.selection.jobs.contains(&leg.job),
        PlanAction::Job {job,..}=>!intent.selection.jobs.contains(job),
        PlanAction::Cancel {leg,..}|PlanAction::Warn {leg,..}=>!intent.selection.jobs.contains(leg),
        PlanAction::Finish(_)=>false,
    });
    for key in &intent.selection.jobs {reported.remove(key);}
    recovered.extend(actions);
    Ok(())
}

pub(super) fn replay_refused(log:Log<'_>,plan:&RunPlan)->anyhow::Result<()> {
    let intent=log.intent()?;
    let current=plan.pipeline_retry_selection(&intent.selection.target).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(current==intent.selection,"refused retry log changed its original affected subgraph");
    Ok(())
}

/// Extends the existing U01 retry scenario with the same ordinary protocol
/// simulator used by its retained-ownership oracle. No process is launched.
#[cfg(test)]
pub(super) async fn exercise_active_retry()->anyhow::Result<()> {
    use kiln_ci::sched::state::LegIn;
    use kiln_ci::wire::{OrdinaryOwnerV1,OrdinaryCommandV1,OrdinaryPublicationV1,OrdinaryPhaseV1,JobEventV2};
    let hub=crate::hub::tests::scratch_hub_with(|c|{c.ci.enabled=true;c.conversations.off=true;}).await?;
    struct Shutdown(Arc<Hub>);
    impl Drop for Shutdown {fn drop(&mut self) {self.0.shutdown.cancel();self.0.ci.tx.close();}}
    let _shutdown=Shutdown(Arc::clone(&hub));
    let repo=hub.store.upsert_repo(Repo {id:"active-retry-repo".into(),name:"test/active-retry".into(),clone_url:"file:///missing".into(),branches:vec!["main".into()],poll_seconds:60,enabled:true,created_at:0,last_polled_at:None,last_error:None,mode:"upstream".into(),default_branch:Some("main".into()),mirror:None}).await?;
    let workflow=crate::pipeline::parse("name: ci\njobs:\n  prepare: {steps: [{run: echo prepare}]}\n  target: {needs: [prepare], steps: [{run: echo target}]}\n  dependent: {needs: [target], steps: [{run: echo dependent}]}\n  sibling: {steps: [{run: echo sibling}]}\n")?;
    hub.store.create_run(crate::store::Run {id:"active-retry".into(),repo_id:repo.id,repo_name:repo.name,number:0,pipeline:"ci".into(),event:"push".into(),branch:"main".into(),sha:"saved-sha".into(),message:String::new(),author:"person".into(),status:Status::Queued,created_at:0,started_at:None,finished_at:None,error:None,concurrency_group:None},crate::pipeline::plan(&workflow,&crate::pipeline::Context::default())?).await?;
    hub.store.call(|c| {
        c.execute("UPDATE runs SET status='running',started_at=1 WHERE id='active-retry'",[])?;
        c.execute("UPDATE jobs SET status=CASE key WHEN 'prepare' THEN 'succeeded' WHEN 'dependent' THEN 'pending' ELSE 'queued' END,finished_at=CASE key WHEN 'prepare' THEN 2 ELSE NULL END WHERE run_id='active-retry'",[])?;
        Ok(())
    }).await?;
    super::super::legacy::import_run(&hub,"active-retry").await?;
    let mut d=Dispatch {pipeline_retries:Default::default(),admission:admission::State::default(),pending_losses:Default::default(),hub:Arc::clone(&hub),sched:Scheduler::new(sched_config(&hub,Vec::new())),routing:Default::default(),held:Default::default(),runs:Default::default(),legs:Default::default(),timers:Default::default(),next:Vec::new(),in_commands:false,pods:Default::default(),absent_after:None,decisions:Default::default(),warm:Default::default(),warm_seq:0};
    d.restore_admission().await?;
    let (model,input):(String,String)=hub.store.call(|c|Ok(c.query_row("SELECT workflow,input FROM ci_plans WHERE run_id='active-retry'",[],|r|Ok((r.get(0)?,r.get(1)?)))?)).await?;
    d.replay("active-retry",&model,&input).await?;
    d.ordinary_event("retry-host","retry-process",JobEventV2::OrdinaryDomain {epoch:"retry-domain".into(),process_nonce:"retry-process".into()}).await?;
    let jobs=hub.store.jobs("active-retry".into(),false).await?;
    let job=|key:&str|jobs.iter().find(|j|j.key==key).ok_or_else(||anyhow::anyhow!("missing active retry job {key}"));
    let target=job("target")?.id.clone();
    let sibling=job("sibling")?.id.clone();
    let dependent=job("dependent")?.id.clone();
    let mut owners=Vec::new();
    for (index,id) in [target.clone(),sibling.clone()].into_iter().enumerate() {
        let sequence=u64::try_from(index)?.checked_add(1).ok_or_else(||anyhow::anyhow!("fixture sequence overflow"))?;
        let owner=OrdinaryOwnerV1 {version:1,job:id.clone(),attempt:1,host:"retry-host".into(),process_nonce:"retry-process".into(),publication_epoch:"retry-domain".into(),connection:"a".repeat(64),grant:format!("retry-grant-{sequence}"),execution:format!("retry-execution-{sequence}"),allocation:format!("retained-retry-{sequence}"),command:OrdinaryCommandV1 {stream:"l:1.1".into(),seq:sequence}};
        let spec=d.legs.get(&id).ok_or_else(||anyhow::anyhow!("fixture canonical leg missing"))?.spec.clone();
        let state=d.runs.get("active-retry").ok_or_else(||anyhow::anyhow!("fixture canonical run missing"))?;
        let mut leg:LegIn=leg_in(state,"active-retry",&id,&spec,&hub.cfg.ci,&d.warm);leg.initial_attempt=1;
        d.sched.handle(Input::LegRunning {leg:Box::new(leg),runner:owner.host.clone(),attempt:1},1);
        let allocation=d.sched.allocation(&id,1,owner.allocation.clone()).ok_or_else(||anyhow::anyhow!("fixture original ordinary allocation missing"))?;
        d.claim_ordinary(owner.clone(),allocation,"host",None).await??;
        d.legs.get_mut(&id).ok_or_else(||anyhow::anyhow!("fixture original leg missing"))?.agent=Some(owner.host.clone());
        owners.push(owner);
    }
    // Both modeled executions really have accepted original owners. Only the
    // target publishes stop; the unrelated sibling stays running under its owner.
    let owner=owners.first().cloned().ok_or_else(||anyhow::anyhow!("fixture target owner missing"))?;
    d.sched.handle(Input::RunnerDraining {runner:"retry-host".into()},2);
    d.ordinary_event("retry-host","retry-process",JobEventV2::OrdinaryExecutionPublication {publication:OrdinaryPublicationV1 {version:1,epoch:"retry-domain".into(),seq:1,owner,phase:OrdinaryPhaseV1::Stopped,recovery:true}}).await?;
    d.finished("retry-host",JobResult {job:target.clone(),status:Status::Failed,exit_code:Some(1),error:None,peak_rss:None,cpu_seconds:None}).await?;
    let sibling_owner=owners.get(1).cloned().ok_or_else(||anyhow::anyhow!("fixture sibling owner missing"))?;
    let original=d.sched.allocation(&sibling,1,sibling_owner.allocation.clone()).ok_or_else(||anyhow::anyhow!("live sibling allocation missing"))?;
    let original_json={let sibling=sibling.clone();hub.store.call(move|c|Ok(c.query_row("SELECT allocation FROM ci_retained_ownership WHERE job=?1 AND attempt=1",[sibling],|r|r.get::<_,String>(0))?)).await?};
    // A timed-out original may arrive after its reconciliation read. Record a
    // negative decision at its exact reserved slot, then submit that delayed
    // original: it must refuse before resetting even one canonical row.
    let (selection,seq,workflow)={
        let state=d.runs.get("active-retry").ok_or_else(||anyhow::anyhow!("negative retry run missing"))?;
        (state.plan.pipeline_retry_selection("target").map_err(anyhow::Error::msg)?,state.seq.checked_add(1).ok_or_else(||anyhow::anyhow!("fixture plan sequence overflow"))?,serde_json::to_string(&*state.wf)?)
    };
    let refused=hub.store.call(move|c|Intent::prepare(c,selection,"active-retry".into(),seq,"user:person".into(),workflow,HashMap::new())).await?;
    let store=hub.store.clone();
    let boundary=Arc::new(crate::rt::blocking_in(crate::rt::SHARE_STORE,move||store.writer_boundary(READER_EPOCH)).await??);
    let original_intent=refused.clone();
    let proof=Arc::clone(&boundary);let store=hub.store.clone();
    assert!(!hub.store.call(move|c|{proof.require_store(&store)?;refused.reconcile(c)}).await?);
    let delayed=original_intent.clone();let store=hub.store.clone();
    assert!(hub.store.call(move|c|{boundary.require_store(&store)?;delayed.commit(c)}).await.is_err(),"delayed original reset escaped its negative fence");
    let unchanged=hub.store.job(target.clone()).await?.ok_or_else(||anyhow::anyhow!("fenced original target missing"))?;
    assert_eq!((unchanged.attempt,unchanged.status),(1,Status::Failed));
    let target_copy=target.clone();
    assert!(hub.store.call(move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_attempts WHERE leg_id=?1 AND number=1 AND physical_receipt IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM ci_retained_ownership WHERE job=?1)",[target_copy],|r|r.get::<_,bool>(0))?)).await?,"physical proof did not survive ordinary capacity-journal pruning");
    d.runs.get_mut("active-retry").ok_or_else(||anyhow::anyhow!("fenced retry run missing"))?.seq=seq;
    assert_eq!(d.retry_active_pipeline("active-retry","target",HashMap::new(),"user:person").await?,1,"active retry keeps its original run attempt");
    let retried=hub.store.job(target.clone()).await?.ok_or_else(||anyhow::anyhow!("retried target missing"))?;
    let waits=hub.store.job(dependent.clone()).await?.ok_or_else(||anyhow::anyhow!("retried dependent missing"))?;
    assert_eq!((retried.attempt,retried.status),(2,Status::Queued));
    assert_eq!((waits.attempt,waits.status),(2,Status::Pending));
    assert_eq!(d.sched.allocation(&sibling,1,sibling_owner.allocation.clone()),Some(original.clone()),"retry changed an unrelated original live allocation");
    let current={let sibling=sibling.clone();hub.store.call(move|c|Ok(c.query_row("SELECT allocation FROM ci_retained_ownership WHERE job=?1 AND attempt=1",[sibling],|r|r.get::<_,String>(0))?)).await?};
    assert_eq!(current,original_json,"retry replaced the unrelated authenticated execution journal");
    // Recovery consumes the committed reset amidst earlier completion reports.
    // It never copies the sibling into a fresh run attempt or creates a new id.
    let saved:(String,String)=hub.store.call(|c|Ok(c.query_row("SELECT workflow,input FROM ci_plans WHERE run_id='active-retry'",[],|r|Ok((r.get(0)?,r.get(1)?)))?)).await?;
    d.runs.remove("active-retry");
    d.legs.clear();
    d.sched=Scheduler::new(sched_config(&hub,Vec::new()));
    d.restore_admission().await?;
    d.restore_retained().await?;
    d.replay("active-retry",&saved.0,&saved.1).await?;
    assert_eq!(d.sched.allocation(&sibling,1,sibling_owner.allocation.clone()),Some(original.clone()));
    let state=d.runs.get("active-retry").ok_or_else(||anyhow::anyhow!("recovered retry run missing"))?;
    assert_eq!(state.keys.get("target"),Some(&target));
    assert_eq!(state.keys.get("dependent"),Some(&dependent));
    assert!(state.plan.jobs().iter().find(|j|j.id=="dependent").is_some_and(|j|j.state=="waiting"));
    // A direct-core recovery input cannot repopulate a paused selected ID or
    // refund the unrelated original allocation. This is a pure model copy,
    // never a replacement owner/controller for the real dispatcher above.
    let mut guarded=d.sched.clone();
    let pause=guarded.pause_pipeline_retry("fixture-conflict".into(),&[(target.clone(),2)]).map_err(anyhow::Error::msg)?;
    let conflict=LegIn {id:target.clone(),run:"active-retry".into(),repo:"test/active-retry".into(),..LegIn::default()};
    assert!(guarded.handle(Input::LegRunning {leg:Box::new(conflict),runner:"retry-host".into(),attempt:2},3).is_empty());
    assert!(!guarded.restore_executor(kiln_ci::sched::state::ExecutorSnapshot {executor:"conflicting-executor".into(),pool:"fixture".into(),runner:"retry-host".into(),reserved:Default::default(),leg:Some(target.clone()),binding:None,shape:None},false,3));
    assert!(guarded.executor_snapshot("conflicting-executor").is_none(),"paused restore recreated a selected physical owner");
    assert!(guarded.pipeline_retry_conflict(&pause).is_some());
    assert!(guarded.commit_pipeline_retry(&pause).is_err() && guarded.resume_pipeline_retry(&pause,4).is_err(),"physical conflict cleared its original pause");
    assert!(guarded.allocation(&target,2,"conflict".into()).is_none());
    assert_eq!(guarded.allocation(&sibling,1,sibling_owner.allocation.clone()),Some(original.clone()));
    // Finish the new target through the same actual claim/publication/ingest
    // path, then leave its dependent running. Retry must refuse before effects.
    for (index,id) in [target.clone(),dependent.clone()].into_iter().enumerate() {
        let sequence=u64::try_from(index)?.checked_add(3).ok_or_else(||anyhow::anyhow!("fixture sequence overflow"))?;
        let owner=OrdinaryOwnerV1 {version:1,job:id.clone(),attempt:2,host:"retry-host".into(),process_nonce:"retry-process".into(),publication_epoch:"retry-domain".into(),connection:"a".repeat(64),grant:format!("retry-grant-{sequence}"),execution:format!("retry-execution-{sequence}"),allocation:format!("retained-retry-{sequence}"),command:OrdinaryCommandV1 {stream:"l:1.1".into(),seq:sequence}};
        let spec=d.legs.get(&id).ok_or_else(||anyhow::anyhow!("new canonical retry leg missing"))?.spec.clone();
        let state=d.runs.get("active-retry").ok_or_else(||anyhow::anyhow!("active retry run missing"))?;
        let mut leg=leg_in(state,"active-retry",&id,&spec,&hub.cfg.ci,&d.warm);leg.initial_attempt=2;
        d.sched.handle(Input::LegRunning {leg:Box::new(leg),runner:owner.host.clone(),attempt:2},3);
        let allocation=d.sched.allocation(&id,2,owner.allocation.clone()).ok_or_else(||anyhow::anyhow!("new retry allocation missing"))?;
        d.claim_ordinary(owner.clone(),allocation,"host",None).await??;
        d.legs.get_mut(&id).ok_or_else(||anyhow::anyhow!("new claimed retry leg missing"))?.agent=Some(owner.host.clone());
        d.sched.handle(Input::RunnerDraining {runner:"retry-host".into()},4);
        if index==0 {
            d.ordinary_event("retry-host","retry-process",JobEventV2::OrdinaryExecutionPublication {publication:OrdinaryPublicationV1 {version:1,epoch:"retry-domain".into(),seq:2,owner,phase:OrdinaryPhaseV1::Stopped,recovery:true}}).await?;
            d.finished("retry-host",JobResult {job:id,status:Status::Succeeded,exit_code:Some(0),error:None,peak_rss:None,cpu_seconds:None}).await?;
        }
    }
    let before=hub.store.job(target.clone()).await?.ok_or_else(||anyhow::anyhow!("completed retry target missing"))?;
    let refused=d.retry_active_pipeline("active-retry","target",HashMap::new(),"user:person").await;
    assert!(refused.is_err_and(|error|format!("{error:#}").contains("running")),"running dependent was not refused");
    let after=hub.store.job(target).await?.ok_or_else(||anyhow::anyhow!("refused retry target missing"))?;
    assert_eq!((after.attempt,after.status),(before.attempt,before.status));
    assert!(d.pipeline_retries.is_empty(),"a refused running dependency created retry custody");
    assert_eq!(d.sched.allocation(&sibling,1,sibling_owner.allocation.clone()),Some(original.clone()),"refused retry changed unrelated original work");
    // Unknown durable records cannot default to a completion for the live
    // sibling. Replay refuses before replacing any live dispatcher/core state.
    let saved:(String,String)=hub.store.call(|c|{
        let seq:i64=c.query_row("SELECT COALESCE(MAX(seq),0)+1 FROM ci_plan_log WHERE run_id='active-retry'",[],|r|r.get(0))?;
        c.execute("INSERT INTO ci_plan_log(run_id,seq,kind,leg,attempt) VALUES('active-retry',?1,'unsupported-record','sibling',1)",[seq])?;
        Ok(c.query_row("SELECT workflow,input FROM ci_plans WHERE run_id='active-retry'",[],|r|Ok((r.get(0)?,r.get(1)?)))?)
    }).await?;
    let malformed=d.replay("active-retry",&saved.0,&saved.1).await;
    assert!(malformed.is_err_and(|error|error.is::<UnknownPlanLogKind>()),"unknown plan-log record was interpreted as a completion");
    assert_eq!(d.sched.allocation(&sibling,1,sibling_owner.allocation.clone()),Some(original.clone()));
    hub.store.call(|c|{c.execute("DELETE FROM ci_plan_log WHERE run_id='active-retry' AND kind='unsupported-record'",[])?;Ok(())}).await?;
    // Logical completion precedes original physical stop, then canonical
    // deletion cascades the optional attempt history. The actual later stop
    // still releases the original retained owner; absence is never stop proof.
    d.finished("retry-host",JobResult {job:sibling.clone(),status:Status::Succeeded,exit_code:Some(0),error:None,peak_rss:None,cpu_seconds:None}).await?;
    let retained=sibling_owner.allocation.clone();
    assert!(d.sched.allocation(&retained,1,retained.clone()).is_some(),"logical completion dropped original physical debt");
    hub.store.call(|c|{c.execute("DELETE FROM runs WHERE id='active-retry'",[])?;Ok(())}).await?;
    d.ordinary_event("retry-host","retry-process",JobEventV2::OrdinaryExecutionPublication {publication:OrdinaryPublicationV1 {version:1,epoch:"retry-domain".into(),seq:3,owner:sibling_owner,phase:OrdinaryPhaseV1::Stopped,recovery:true}}).await?;
    assert!(d.sched.allocation(&retained,1,retained.clone()).is_none(),"deleted canonical history blocked original positive retirement");
    assert!(d.admission.ready,"deleted optional attempt history fenced a valid original stop");
    hub.shutdown.cancel();
    hub.ci.tx.close();
    Ok(())
}
