//! U10: protected Flow identity, durable decisions and common concurrency admission. Every
//! admission/count/update runs inside the product's one SQL transaction. A holder is released
//! only after its durable job/release has ended; expiry alone never frees execution capacity.
//! Environment identity comes from jobs.leg, the canonical plan, never jobs.spec, its UI projection.

use crate::sql::{OptionalExtension as _, params};
use kiln_ci::env::{EnvironmentId, EnvironmentScope, Review};
use serde::{Deserialize, Serialize};
use crate::store::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub id: String,
    pub environment: EnvironmentId,
    pub repo: String,
    pub run: String,
    pub local_environment: String,
    pub app: String,
    pub version: String,
    pub sha: String,
    pub actor: String,
    pub attempt: i64,
}

pub fn identity(repo: &str, def: &super::EnvDef) -> EnvironmentId {
    def.protection.policy.clone().unwrap_or_else(|| EnvironmentId { scope: EnvironmentScope::Repository(repo.into()), name: def.name.clone() })
}

fn row(r: &crate::sql::Row<'_>) -> crate::sql::Result<Flow> {
    let scope: Vec<u8> = r.get(1)?;
    let (scope, name): (EnvironmentScope, String) = serde_json::from_slice(&scope).map_err(|e| crate::sql::Error::Database(e.to_string()))?;
    Ok(Flow { id: r.get(0)?, environment: EnvironmentId { scope, name }, repo: r.get(2)?, run: r.get(3)?, local_environment: r.get(4)?, app: r.get(5)?, version: r.get(6)?, sha: r.get(7)?, actor: r.get(8)?, attempt:r.get(9)? })
}
const COLUMNS: &str = "id, environment_key, repo_id, run_id, local_environment, app, version, sha, actor, run_attempt";

pub async fn get(store: &Store, id: &str) -> anyhow::Result<Option<Flow>> {
    let id = id.to_owned();
    store.call(move |c| Ok(c.query_row(&format!("SELECT {COLUMNS} FROM environment_flows WHERE id=?1"), [&id], row).optional()?)).await
}

pub async fn for_job(store: &Store, run: &str, environment: &str) -> anyhow::Result<Option<Flow>> {
    let (run, environment) = (run.to_owned(), environment.to_owned());
    store.call(move |c| Ok(c.query_row(&format!("SELECT {COLUMNS} FROM environment_flows WHERE run_id=?1 AND local_environment=?2 COLLATE NOCASE AND run_attempt=(SELECT attempt FROM runs WHERE id=?1)"), params![run, environment], row).optional()?)).await
}

pub async fn create(store: &Store, flow: Flow, by: String, now: i64) -> anyhow::Result<Flow> {
    let key = flow.environment.key()?;
    store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let actual: (String, String, String,i64) = tx.query_row("SELECT repo_id, sha, actor,attempt FROM runs WHERE id=?1", [&flow.run], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        anyhow::ensure!(actual == (flow.repo.clone(), flow.sha.clone(), flow.actor.clone(),flow.attempt), "run identity changed before the Flow was recorded");
        // Attaching after a job passed its gate could retroactively import an approval. Refuse.
        let started: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE run_id=?1 AND json_extract(leg,'$.environment.name')=?2 COLLATE NOCASE AND run_attempt=?3 AND (started_at IS NOT NULL OR id IN(SELECT object_id FROM environment_reservations WHERE kind='job')))", params![flow.run,flow.local_environment,flow.attempt], |r| r.get(0))?;
        let reviewed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_env_reviews WHERE run_id=?1 AND environment=?2 COLLATE NOCASE)",params![flow.run,flow.local_environment],|r|r.get(0))?;
        anyhow::ensure!(!started && !reviewed, "declare the protected Flow before its environment job passes protection or receives a legacy review");
        tx.execute("INSERT INTO environment_flows(id,environment_key,repo_id,run_id,local_environment,app,version,sha,actor,created_by,created_at,run_attempt) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)", params![flow.id,key,flow.repo,flow.run,flow.local_environment,flow.app,flow.version,flow.sha,flow.actor,by,now,flow.attempt])?;
        tx.commit()?;
        Ok(flow)
    }).await
}

pub async fn reviews(store: &Store, action: &str) -> anyhow::Result<Vec<Review>> {
    let action = action.to_owned();
    store.call(move |c| {
        let mut q = c.prepare("SELECT principal,approved,comment,created_at FROM environment_action_reviews WHERE action_id=?1 ORDER BY created_at,principal")?;
        Ok(q.query_map([action], |r| Ok(Review { by:r.get(0)?,approved:r.get(1)?,comment:r.get(2)?,at:r.get(3)? }))?.collect::<Result<Vec<_>,_>>()?)
    }).await
}

pub async fn review(store: &Store, action: &str, review: Review) -> anyhow::Result<()> {
    let action = action.to_owned();
    store.call(move |c| {
        c.execute("INSERT INTO environment_action_reviews(action_id,principal,approved,comment,created_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(action_id,principal) DO UPDATE SET approved=?3,comment=?4,created_at=?5 WHERE environment_action_reviews.approved=1", params![action,review.by,review.approved,review.comment,review.at])?;
        Ok(())
    }).await
}

/// Link the exact approved app/version/environment once. Idempotent release retries keep the
/// same number; another release cannot borrow this approval even with the same display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowBinding { Bound, Refused(String) }

pub async fn bind_release(store: &Store, flow: &Flow, environment: &EnvironmentId, app: &str, version: Option<&str>, release: i64, artifacts: &std::collections::BTreeMap<String,crate::deploy::model::Artifact>) -> anyhow::Result<FlowBinding> {
    if !(flow.environment.key()? == environment.key()? && flow.app == app && Some(flow.version.as_str()) == version) {return Ok(FlowBinding::Refused("protected Flow does not authorize this environment/app/version".into()));}
    let fingerprint=serde_json::to_string(artifacts)?;
    let (id, release,run,sha,attempt) = (flow.id.clone(), release,flow.run.clone(),flow.sha.clone(),flow.attempt);
    store.call(move |c| {
        let tx=crate::store::tx(c)?;
        let (actual,status,current_attempt):(String,String,i64)=tx.query_row("SELECT sha,status,attempt FROM runs WHERE id=?1",[run],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        if !(actual==sha && attempt==current_attempt && !matches!(status.as_str(),"failed"|"cancelled"|"timed_out")) {tx.commit()?;return Ok(FlowBinding::Refused("the protected Flow's CI run failed, was cancelled, or changed identity".into()));}
        if release==0 {tx.commit()?;return Ok(FlowBinding::Bound);}
        let n = tx.execute("UPDATE environment_flows SET release_id=?2,release_fingerprint=?3 WHERE id=?1 AND (release_id IS NULL OR release_id=?2) AND (release_fingerprint IS NULL OR release_fingerprint=?3)", params![id,release,fingerprint])?;
        if n!=1 {tx.commit()?;return Ok(FlowBinding::Refused("protected Flow already belongs to another release or artifact set".into()));}
        tx.commit()?;
        Ok(FlowBinding::Bound)
    }).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission { Granted, Held(&'static str), Refused(String) }

pub async fn reserve(store: &Store, environment: &EnvironmentId, action: &str, kind: &str, object: &str, expected: &kiln_ci::env::Protection, now: i64) -> anyhow::Result<Admission> {
    let (key, action, kind, object) = (environment.key()?, action.to_owned(), kind.to_owned(), object.to_owned());
    let holder = format!("{kind}:{object}");
    let mut expected=expected.clone(); expected.policy=None;
    store.call(move |c| {
        let tx = crate::store::tx(c)?;
        // U10: the product reads the canonical policy in this SAME writer transaction.
        // Concurrent policy lowering cannot race an admission based on an older limit.
        let row=tx.records_get("kiln","environment.policy",&key)?.ok_or_else(||anyhow::anyhow!("canonical environment policy is missing"))?;
        let policy:kiln_ci::env::EnvironmentPolicy=serde_json::from_slice(&row.value)?;
        if policy.protection!=expected {tx.commit()?;return Ok(Admission::Held("environment policy changed during evaluation; checking it again"));}
        let limit=policy.concurrency;
        // An approval or rejection arriving between evaluation and admission is fenced here.
        // A Flow is explicit: neither matching versions nor an unrelated run can inherit it.
        let release_snapshot=if kind=="release" {
            let number:i64=object.parse()?;
            let row=tx.records_get("kiln","deploy.release",&crate::deploy::store::release_key(number))?.ok_or_else(||anyhow::anyhow!("release disappeared before admission"))?;
            let release:crate::deploy::model::Release=serde_json::from_slice(&row.value)?;
            if !release.state.in_flight() {tx.commit()?;return Ok(Admission::Refused("release already ended before admission".into()));}
            Some(release)
        }else{None};
        if kind=="job" {
            let status:String=tx.query_row("SELECT status FROM jobs WHERE id=?1",[&object],|r|r.get(0))?;
            if !matches!(status.as_str(),"queued"|"running") {tx.commit()?;return Ok(Admission::Refused("job already ended before admission".into()));}
            let active_flow:Option<String>=tx.query_row("SELECT f.id FROM environment_flows f JOIN jobs j ON j.run_id=f.run_id AND j.run_attempt=f.run_attempt JOIN runs r ON r.id=f.run_id AND r.attempt=f.run_attempt WHERE j.id=?1 AND json_extract(j.leg,'$.environment.name')=f.local_environment COLLATE NOCASE",[&object],|r|r.get(0)).optional()?;
            if active_flow.as_ref().is_some_and(|id|action!=format!("flow:{id}")) {tx.commit()?;return Ok(Admission::Held("protected Flow identity changed during admission; checking it again"));}
        }
        let mut reviews=Vec::new();
        if let Some(flow)=action.strip_prefix("flow:") {
            let (flow_key,run,local,release_id,sha,attempt):(Vec<u8>,String,String,Option<i64>,String,i64)=tx.query_row("SELECT environment_key,run_id,local_environment,release_id,sha,run_attempt FROM environment_flows WHERE id=?1",[flow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
            let (actual_sha,actual_attempt,status):(String,i64,String)=tx.query_row("SELECT sha,attempt,status FROM runs WHERE id=?1",[&run],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            if actual_sha!=sha || actual_attempt!=attempt || matches!(status.as_str(),"failed"|"cancelled"|"timed_out") {tx.commit()?;return Ok(Admission::Refused("protected Flow's CI run failed, was cancelled, or changed identity".into()));}
            anyhow::ensure!(flow_key==key,"Flow belongs to another environment");
            if kind=="job" {
                let (job_run,job_environment):(String,Option<String>)=tx.query_row("SELECT run_id,json_extract(leg,'$.environment.name') FROM jobs WHERE id=?1",[&object],|r|Ok((r.get(0)?,r.get(1)?)))?;
                anyhow::ensure!(job_run==run && job_environment.as_deref().is_some_and(|e|e.eq_ignore_ascii_case(&local)),"Flow belongs to another job boundary");
            }else{anyhow::ensure!(release_id.is_some_and(|id|id.to_string()==object),"Flow belongs to another release");}
            let mut q=tx.prepare("SELECT principal,approved,comment,created_at FROM environment_action_reviews WHERE action_id=?1")?;
            reviews=q.query_map([flow],|r|Ok(Review{by:r.get(0)?,approved:r.get(1)?,comment:r.get(2)?,at:r.get(3)?}))?.collect::<Result<Vec<_>,_>>()?;
        }else if kind=="job" {
            let (run,local):(String,Option<String>)=tx.query_row("SELECT run_id,json_extract(leg,'$.environment.name') FROM jobs WHERE id=?1",[&object],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let mut q=tx.prepare("SELECT principal,approved,comment,created_at FROM ci_env_reviews WHERE run_id=?1 AND environment=?2 COLLATE NOCASE AND COALESCE(protected_action,'job:'||run_id)='job:'||run_id")?;
            reviews=q.query_map(params![run,local],|r|Ok(Review{by:r.get(0)?,approved:r.get(1)?,comment:r.get(2)?,at:r.get(3)?}))?.collect::<Result<Vec<_>,_>>()?;
        }else {
            if let Some(release)=release_snapshot {
                reviews=release.approvals.into_iter().map(|r|Review{by:r.by,approved:r.approved,comment:r.note.unwrap_or_default(),at:r.at}).collect();
            }
        }
        let approval=kiln_ci::env::Protection{reviewers:policy.protection.reviewers.clone(),approvals:policy.protection.approvals.clone(),..Default::default()};
        match kiln_ci::env::gate("environment",&approval,"",now,now,&reviews) {
            kiln_ci::env::Gate::Open=>{},
            kiln_ci::env::Gate::Rejected{by,comment}=>{tx.commit()?;return Ok(Admission::Refused(format!("rejected by {by}: {comment}")));},
            kiln_ci::env::Gate::Refused(why)=>{tx.commit()?;return Ok(Admission::Refused(why));},
            kiln_ci::env::Gate::Waiting{..}=>{tx.commit()?;return Ok(Admission::Held("protected action is waiting for approval"));},
        }
        // Terminal observations are durable and monotonic. Restart never forgets live holders.
        tx.execute("DELETE FROM environment_reservations WHERE kind='job' AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.id=object_id AND j.status IN ('queued','running'))", [])?;
        let owned: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM environment_reservations WHERE environment_key=?1 AND action_id=?2)", params![key,action], |r|r.get(0))?;
        let active: i64 = tx.query_row("SELECT COUNT(DISTINCT action_id) FROM environment_reservations WHERE environment_key=?1", [&key], |r|r.get(0))?;
        if !owned && !kiln_ci::env::concurrency_available(limit, usize::try_from(active).unwrap_or(usize::MAX)) { tx.commit()?; return Ok(Admission::Held("shared environment concurrency limit reached")); }
        tx.execute("INSERT OR IGNORE INTO environment_reservations(environment_key,action_id,holder,kind,object_id,created_at) VALUES(?1,?2,?3,?4,?5,?6)", params![key,action,holder,kind,object,now])?;
        tx.commit()?;
        Ok(Admission::Granted)
    }).await
}

pub async fn release(store: &Store, kind: &str, object: &str) -> anyhow::Result<()> {
    let holder = format!("{kind}:{object}");
    store.call(move |c| { c.execute("DELETE FROM environment_reservations WHERE holder=?1", [holder])?; Ok(()) }).await
}

/// Ended releases are observations from the release domain. The SQL transaction below only
/// removes the exact observed holders; no clock-based lease can release a live deployment.
pub async fn reap_releases(store: &Store, records: &dyn kiln_seams::Records) -> anyhow::Result<()> {
    let ids: Vec<String> = store.call(|c| {
        let mut q=c.prepare("SELECT DISTINCT object_id FROM environment_reservations WHERE kind='release' LIMIT 4096")?;
        Ok(q.query_map([], |r|r.get(0))?.collect::<Result<Vec<_>,_>>()?)
    }).await?;
    for id in ids {
        let number: i64=id.parse()?;
        if let Some(r)=crate::deploy::store::release(records,number).await? && !r.state.in_flight() && !["undo","withdraw"].iter().any(|key|r.provider.get(*key).and_then(serde_json::Value::as_bool)==Some(true)) {
            release(store,"release",&id).await?;
        }
    }
    Ok(())
}

/// U10: grandfather live execution before queued work is replayed. Existing holders keep
/// capacity even when a policy was lowered while the server was down; they are never killed.
pub(crate) async fn restore_running(hub:&crate::hub::Hub)->anyhow::Result<()> {
    let rows:Vec<(String,String,String,String)>=hub.store.call(|c|{
        let mut q=c.prepare("SELECT j.id,j.run_id,r.repo_id,json_extract(j.leg,'$.environment.name') FROM jobs j JOIN runs r ON r.id=j.run_id WHERE j.status='running' AND json_extract(j.leg,'$.environment.name') IS NOT NULL")?;
        Ok(q.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<Result<Vec<_>,_>>()?)
    }).await?;
    let repos=hub.store.repos().await?;
    for (job,run,repo,name) in rows {
        let repo=repos.iter().find(|r|r.id==repo).ok_or_else(||anyhow::anyhow!("running environment job's repository is missing"))?;
        let def=super::ensure(hub,repo,&name).await?;
        let key=identity(&repo.id,&def).key()?;
        let action=for_job(&hub.store,&run,&name).await?.map_or_else(||format!("job:{job}"),|f|format!("flow:{}",f.id));
        hub.store.call(move|c|{
            c.execute("INSERT OR IGNORE INTO environment_reservations(environment_key,action_id,holder,kind,object_id,created_at) SELECT ?1,?2,?3,'job',id,COALESCE(started_at,0) FROM jobs WHERE id=?4 AND status='running'",params![key,action,format!("job:{job}"),job])?;
            Ok(())
        }).await?;
    }
    for env in crate::deploy::store::environments(&*hub.records).await? { restore_release_holders(&hub.store,&*hub.records,&env.name).await?; }
    Ok(())
}

/// Import only active release observations, using their state index rather than truncating
/// history. Explicit refusal at the work bound is safer than silently omitting a live holder.
pub async fn restore_release_holders(store:&Store,records:&dyn kiln_seams::Records,environment:&str)->anyhow::Result<()> {
    for state in [crate::deploy::model::State::Committing,crate::deploy::model::State::Delivering] {
        let active=crate::deploy::store::releases(records,crate::deploy::store::ReleaseQuery{environment:Some(environment.into()),state:Some(state),limit:1000,..Default::default()}).await?;
        anyhow::ensure!(active.len()<1000,"too many active releases to restore environment admission safely");
        for r in active {
            let key=EnvironmentId{scope:EnvironmentScope::Server,name:environment.into()}.key()?;
            let action=r.provider.get("protected_action").and_then(serde_json::Value::as_str).map_or_else(||format!("release:{}",r.id),|f|format!("flow:{f}"));
            store.call(move|c|{c.execute("INSERT OR IGNORE INTO environment_reservations(environment_key,action_id,holder,kind,object_id,created_at) VALUES(?1,?2,?3,'release',?4,?5)",params![key,action,format!("release:{}",r.id),r.id.to_string(),r.created_at])?;Ok(())}).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::env::{Approvals,EnvironmentPolicy,Protection};

    async fn fixture(protection:Protection)->anyhow::Result<(Store,crate::records::StoreRecords,EnvironmentId,Protection)> {
        let store=Store::in_memory_inline()?;
        store.call(|c|{
            c.execute("INSERT INTO repos(id,name,clone_url,created_at) VALUES('repo','acme/web','local',0)",[])?;
            c.execute("INSERT INTO runs(id,repo_id,number,pipeline,event,branch,sha,status,created_at,actor) VALUES('run','repo',1,'test','push','main','sha','queued',0,'builder')",[])?;
            for job in ["job-a","job-b"] {
                c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,leg) VALUES(?1,'run',?1,?1,'queued','[]','[]','{}',?2)",params![job,r#"{"environment":{"name":"prod"}}"#])?;
            }
            Ok(())
        }).await?;
        let records=crate::records::StoreRecords::open(store.clone())?;
        let id=EnvironmentId{scope:EnvironmentScope::Server,name:"prod".into()};
        super::super::policy::save(&records,&id,&EnvironmentPolicy{protection:protection.clone(),concurrency:Some(1),..Default::default()}).await?;
        for number in [11,12] {
            let release:crate::deploy::model::Release=serde_json::from_value(serde_json::json!({"id":number,"app":"web","environment":"prod","kind":"deploy","state":"waiting","requested_by":"builder","version":"v1","created_at":0}))?;
            let value=serde_json::to_vec(&release)?;
            store.call(move|c|{c.records_commit("kiln",amphora_api::Tx{conds:Vec::new(),writes:vec![amphora_api::Write::Put{family:"deploy.release".into(),key:crate::deploy::store::release_key(number),value}]})?;Ok(())}).await?;
        }
        Ok((store,records,id,protection))
    }

    /// U10/W09-AC07: a real planned row keeps protection in leg and only a presentation in spec.
    /// Its exact approved environment passes admission; a Flow cannot attach once it started.
    #[test]
    fn u10_canonical_leg_keeps_approval_and_started_flow_fences()->anyhow::Result<()> {
        crate::rt::block_on(async{
            let p=Protection{reviewers:vec!["alice".into()],..Default::default()};
            let (store,_records,id,p)=fixture(p).await?;
            assert!(matches!(reserve(&store,&id,"job:job-a","job","job-a",&p,1).await?,Admission::Held(_)));
            store.call(|c|{
                c.execute("INSERT INTO ci_env_reviews(run_id,environment,approved,comment,principal,created_at,protected_action) VALUES('run','prod',1,'','user:alice',1,'job:run')",[])?;
                Ok(())
            }).await?;
            assert_eq!(reserve(&store,&id,"job:job-a","job","job-a",&p,2).await?,Admission::Granted);
            // Remove the review to isolate the started/reserved-job guard, not the review guard.
            store.call(|c|{c.execute("DELETE FROM ci_env_reviews",[])?;Ok(())}).await?;
            let flow=Flow{id:"late".into(),environment:id,repo:"repo".into(),run:"run".into(),local_environment:"prod".into(),app:"web".into(),version:"v1".into(),sha:"sha".into(),actor:"builder".into(),attempt:1};
            assert!(create(&store,flow,"user:creator".into(),3).await.is_err(),"a Flow must not import an already admitted job");
            Ok(())
        })
    }

    #[test]
    fn u10_ci_and_release_compete_for_one_atomic_capacity_after_restart()->anyhow::Result<()> {
        crate::rt::block_on(async{
            let (store,_records,id,p)=fixture(Protection::default()).await?;
            let (job,release)=futures::join!(reserve(&store,&id,"job:job-a","job","job-a",&p,1),reserve(&store,&id,"release:11","release","11",&p,1));
            let outcomes=[job?,release?];
            assert_eq!(outcomes.iter().filter(|x|**x==Admission::Granted).count(),1);
            assert_eq!(outcomes.iter().filter(|x|matches!(x,Admission::Held(_))).count(),1);
            // New service handles see the same durable holders, with no clock-based expiry.
            let reopened=store.clone();
            assert!(matches!(reserve(&reopened,&id,"release:12","release","12",&p,i64::MAX).await?,Admission::Held(_)));
            Ok(())
        })
    }

    #[test]
    fn u10_policy_lowering_and_changed_rules_are_fenced_by_atomic_admission()->anyhow::Result<()> {
        crate::rt::block_on(async{
            let (store,records,id,p)=fixture(Protection::default()).await?;
            super::super::policy::save(&records,&id,&EnvironmentPolicy{concurrency:Some(2),..Default::default()}).await?;
            assert_eq!(reserve(&store,&id,"job:job-a","job","job-a",&p,1).await?,Admission::Granted);
            super::super::policy::save(&records,&id,&EnvironmentPolicy{concurrency:Some(1),..Default::default()}).await?;
            assert!(matches!(reserve(&store,&id,"release:11","release","11",&p,2).await?,Admission::Held(_)));
            let changed=Protection{wait_timer:5,..Default::default()};
            super::super::policy::save(&records,&id,&EnvironmentPolicy{protection:changed,concurrency:Some(5),..Default::default()}).await?;
            assert!(matches!(reserve(&store,&id,"job:job-b","job","job-b",&p,3).await?,Admission::Held(_)));
            Ok(())
        })
    }

    #[test]
    fn u10_flow_reuses_one_durable_approval_and_slot_without_authorizing_other_actions()->anyhow::Result<()> {
        crate::rt::block_on(async{
            let p=Protection{approvals:Some(Approvals{count:1,reviewers:vec!["alice".into()],self_approve:false}),..Default::default()};
            let (store,_records,id,p)=fixture(p).await?;
            let flow=Flow{id:"flow-a".into(),environment:id.clone(),repo:"repo".into(),run:"run".into(),local_environment:"prod".into(),app:"web".into(),version:"v1".into(),sha:"sha".into(),actor:"builder".into(),attempt:1};
            create(&store,flow.clone(),"user:creator".into(),0).await?;
            review(&store,&flow.id,Review{by:"user:alice".into(),approved:true,comment:String::new(),at:1}).await?;
            assert_eq!(for_job(&store,"run","prod").await?.map(|f|f.id),Some(flow.id.clone()));
            assert_eq!(reviews(&store,&flow.id).await?.len(),1);
            assert_eq!(reserve(&store,&id,"flow:flow-a","job","job-a",&p,2).await?,Admission::Granted);
            assert_eq!(bind_release(&store,&flow,&id,"web",Some("v1"),11,&Default::default()).await?,FlowBinding::Bound);
            assert_eq!(reserve(&store,&id,"flow:flow-a","release","11",&p,2).await?,Admission::Granted);
            assert!(matches!(reserve(&store,&id,"release:12","release","12",&p,2).await?,Admission::Held(_)));
            assert!(matches!(bind_release(&store,&flow,&id,"another-app",Some("v1"),11,&Default::default()).await?,FlowBinding::Refused(_)));
            assert!(matches!(bind_release(&store,&flow,&id,"web",Some("v1"),12,&Default::default()).await?,FlowBinding::Refused(_)));
            // A rejection arriving between preview and admission wins, and cannot be reopened.
            review(&store,&flow.id,Review{by:"user:alice".into(),approved:false,comment:"stop".into(),at:3}).await?;
            review(&store,&flow.id,Review{by:"user:alice".into(),approved:true,comment:String::new(),at:4}).await?;
            assert!(matches!(reserve(&store,&id,"flow:flow-a","release","11",&p,5).await?,Admission::Refused(_)));
            // A rerun is a different protected action even when its sha matches.
            store.call(|c|{c.execute("UPDATE runs SET attempt=2 WHERE id='run'",[])?;Ok(())}).await?;
            assert!(for_job(&store,"run","prod").await?.is_none());
            Ok(())
        })
    }
}
