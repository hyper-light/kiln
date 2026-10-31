//! kiln's `deploy` event (docs/ci.md W03-R09, W15-R06): CD drives CI. As KilnCD publishes its
//! apps' statuses (`cd_apps`), an app that turns `synced` (in sync and healthy, or a sync of it
//! succeeding), `degraded`, or whose sync `failed`, raises `deploy` in the repository whose
//! `.kiln/deploy.yaml` defines it: that repository's default-branch workflows that take it run
//! (smoke tests after staging turns healthy, diagnostics when it degrades); one that turns
//! degraded wakes the agents attached to its environment (W20-R08's `degraded`).

use std::sync::Arc;

use kiln_ci::trigger::{Event, Source};
use crate::sql::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::cd::store::ControllerScope;

/// What an app was, as last published: in sync, healthy, its latest operation and that
/// operation's phase.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Seen {
    synced: bool,
    healthy: bool,
    degraded: bool,
    op: Option<(u64, String)>,
}

impl Seen {
    fn of(a: &Value) -> Seen {
        let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or_default();
        let op = a.get("operation").filter(|o| !o.is_null()).map(|o| (o.get("id").and_then(Value::as_u64).unwrap_or(0), o.get("phase").and_then(Value::as_str).unwrap_or_default().to_owned()));
        Seen { synced: s("sync") == "Synced", healthy: s("health") == "Healthy", degraded: s("health") == "Degraded", op }
    }
}

/// The state an app moved into, if it's one `deploy` fires on: `failed` (its sync), `degraded`, or
/// `synced`. The first status seen of an app only records it.
fn moved(before: Option<&Seen>, now: &Seen, new_incarnation: bool) -> Option<&'static str> {
    let before = before?;
    let new_op = (now.op.is_some() && new_incarnation) || now.op.as_ref().map(|(id, _)| *id) != before.op.as_ref().map(|(id, _)| *id);
    let phase = |s: &Seen| s.op.as_ref().map(|(_, p)| p.to_ascii_lowercase()).unwrap_or_default();
    let failed = |s: &Seen| matches!(phase(s).as_str(), "failed" | "error");
    if failed(now) && (new_op || !failed(before)) {
        return Some("failed");
    }
    if now.degraded && !before.degraded {
        return Some("degraded");
    }
    let succeeded_again = new_op && phase(now) == "succeeded";
    if now.synced && now.healthy && (!(before.synced && before.healthy) || succeeded_again) {
        return Some("synced");
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Accepted { sequence: u64, #[serde(default)] controller: Option<ControllerScope>, seen: Seen }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Transition {
    repo: crate::store::Repo,
    app: String,
    environment: String,
    state: String,
    status: Value,
    git_ref: String,
    sha: Option<String>,
}

fn seen_key(app: &str) -> String { format!("ci/deploy-control/{}", crate::util::sha256_hex(app.as_bytes())) }

/// U04/U14: the async CD action carrier awaits this before publishing a status. Control state
/// and its exact intent commit together; UI batching can never erase an intermediate move.
pub(crate) async fn accept_status(hub: &Hub, controller: &ControllerScope, definition: &kiln_cd::apps::AppDef, status: Value) -> anyhow::Result<()> {
    let origin = match &definition.origin { kiln_cd::apps::Origin::Deploy { repo } => repo, _ => return Ok(()) };
    let repo = origin_repo(hub, origin).await?;
    let app = definition.id();
    let definition = definition.clone();
    let now = Seen::of(&status);
    let controller = controller.clone();
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let key = seen_key(&app);
        let saved: Option<String> = tx.query_row("SELECT v FROM kv WHERE k = ?1", [&key], |r| r.get(0)).optional()?;
        let before: Option<Accepted> = saved.as_deref().map(serde_json::from_str).transpose()?;
        let new_incarnation = before.as_ref().and_then(|before| before.controller.as_ref()) != Some(&controller);
        if !new_incarnation && before.as_ref().is_some_and(|before| before.seen == now) { return Ok(()); }
        let state = moved(before.as_ref().map(|before| &before.seen), &now, new_incarnation);
        let sequence = before.as_ref().map_or(Ok(1), |before| before.sequence.checked_add(1).ok_or_else(|| anyhow::anyhow!("CD control sequence exhausted")))?;
        if let Some(state) = state {
            // A terminal operation has the same receipt whether its live status or its
            // immutable Records result reaches SQL first. Later health recoveries remain
            // separate moves, even when they refer to that already completed operation.
            let terminal = now.op.as_ref().filter(|(_, phase)| match state {
                "failed" => matches!(phase.to_ascii_lowercase().as_str(), "failed" | "error"),
                "synced" => phase.eq_ignore_ascii_case("succeeded") && (new_incarnation || before.as_ref().and_then(|before| before.seen.op.as_ref()) != now.op.as_ref()),
                _ => false,
            });
            let intent = terminal.map(|(op, _)| operation_key(&controller, &app, *op, state)).unwrap_or_else(|| format!("cd-status:{app}:{sequence}"));
            stage_transition(&tx, &intent, &controller, repo, &definition, state, status)?;
        }
        tx.execute("INSERT INTO kv(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v", params![key, serde_json::to_string(&Accepted { sequence, controller: Some(controller), seen: now })?])?;
        tx.commit()?;
        Ok(())
    }).await
}

async fn origin_repo(hub: &Hub, origin: &str) -> anyhow::Result<crate::store::Repo> {
    hub.store.repos().await?.into_iter().find(|repo| repo.name == origin || repo.id == origin).ok_or_else(|| anyhow::anyhow!("CD control origin repository {origin} is unavailable"))
}

fn operation_key(controller: &ControllerScope, app: &str, op: u64, state: &str) -> String { format!("cd-operation:{}:{app}:{op}:{state}", controller.as_str()) }

fn stage_transition(c: &crate::sql::Connection<'_>, key: &str, controller: &ControllerScope, repo: crate::store::Repo, definition: &kiln_cd::apps::AppDef, state: &str, status: Value) -> anyhow::Result<String> {
    let git_ref: String = c.query_row("SELECT sha FROM refs WHERE repo_id = ?1 AND name = 'HEAD->'", [&repo.id], |r| r.get(0)).optional()?.unwrap_or_else(|| format!("refs/heads/{}", repo.default_branch.as_deref().unwrap_or("main")));
    let sha = c.query_row("SELECT sha FROM refs WHERE repo_id = ?1 AND name = ?2", params![repo.id, git_ref], |r| r.get(0)).optional()?;
    let transition = Transition { repo, app: definition.id(), environment: definition.environment.clone().unwrap_or_default(), state: state.into(), status, git_ref, sha };
    super::schedule::outbox::control(c, key, json!({"consumer":"cd", "controller":controller, "transition":transition}))
}

fn workflow_children(transition: &Transition, commit: &super::load::Commit, repository: Value, message: &str) -> Vec<(String,super::schedule::outbox::Payload)> {
    let by = transition.status.pointer("/operation/by").and_then(Value::as_str).filter(|by| !by.is_empty()).unwrap_or("kiln");
    let payload = json!({"action":transition.state,"repository":repository,"sender":{"login":by,"type":"User"},"kiln":{"deploy":{"app":transition.app,"environment":transition.environment,"state":transition.state,"sync":transition.status.get("sync"),"health":transition.status.get("health"),"revision":transition.status.get("revision"),"operation":transition.status.get("operation")}}});
    let event = Event { name:"deploy".into(), action:transition.state.clone(), repo:transition.repo.name.clone(), git_ref:transition.git_ref.clone(), sha:transition.sha.clone().unwrap_or_default(), actor:by.into(), trusted:true, source:Source::Cd, payload, message:Some(message.into()), ..Event::default() };
    commit.workflows.iter().filter(|loaded| loaded.runs && (loaded.has_errors() || loaded.workflow.as_ref().is_some_and(|workflow| kiln_ci::trigger::matches(workflow,&event).is_ok())))
        .map(|loaded| (format!("workflow/deploy/{}",loaded.path),super::schedule::outbox::workflow(&transition.repo,&event,loaded,message,by))).collect()
}

/// Prepare all workflow children before handing durable recovery to them. When the scheduler
/// is still restoring, both the immutable transition and its Records control remain pending.
async fn deliver_transition(hub: &Arc<Hub>, id: &str, transition: Transition) -> anyhow::Result<bool> {
    let mut children = Vec::new();
    if transition.state == "degraded" {
        children.extend(super::attach::degraded_control(hub, &transition.repo.name, &transition.app, &transition.environment, &transition.status).await?);
    }
    if hub.cfg.ci.enabled {
        if !hub.ci.enabled() || hub.ci.restored().is_none() { return Ok(false); }
        if let Some(sha) = &transition.sha {
            let commit = super::load::load(hub, &transition.repo, sha).await?;
            let facts = super::events::facts(hub, &transition.repo).await;
            let (message, _) = crate::gitops::commit_info(hub, &transition.repo, sha).await;
            children.extend(workflow_children(&transition,&commit,facts.repo.payload(),&message));
        }
    }
    super::schedule::outbox::materialize(hub,id,children).await?;
    Ok(true)
}

/// The CD Records source is acknowledged only after this SQL control row exists. Copy retries
/// hit the same outbox identity; materialization below performs no external effects.
pub(crate) async fn deliver_control(hub: &Arc<Hub>, id: &str, descriptor: Value) -> anyhow::Result<()> {
    if let Some(value) = descriptor.get("transition") {
        deliver_transition(hub,id,serde_json::from_value(value.clone())?).await?;
        return Ok(());
    }
    // A finished operation is an immutable accepted outcome, independent of the latest
    // observed health/operation. A newer success must never erase an older failure while
    // Records controls drain or replay out of order (U04/U14).
    if let Some(value) = descriptor.get("deployment").filter(|value| !value.is_null()) {
        let deployment: kiln_cd::state::Deployment = serde_json::from_value(value.clone())?;
        let definition = descriptor.get("apps").and_then(Value::as_array).into_iter().flatten().find(|app| app.get("id").and_then(Value::as_str) == Some(deployment.app.as_str())).and_then(|app| app.get("after").filter(|value| !value.is_null()).or_else(|| app.get("before").filter(|value| !value.is_null()))).cloned();
        let Some(definition) = definition else {
            return super::schedule::outbox::finish(hub,id,false,Some("accepted CD deployment has no declared app automation definition".into())).await;
        };
        let definition: kiln_cd::apps::AppDef = serde_json::from_value(definition)?;
        let origin = match &definition.origin {
            kiln_cd::apps::Origin::Deploy { repo } => repo,
            _ => return super::schedule::outbox::finish(hub,id,false,Some("accepted CD deployment has no declared repository automation origin".into())).await,
        };
        let repo = origin_repo(hub,origin).await?;
        anyhow::ensure!(definition.id() == deployment.app, "accepted CD definition belongs to another deployment app");
        let state = if deployment.succeeded { "synced" } else { "failed" };
        let controller: ControllerScope = serde_json::from_value(descriptor.get("controller").cloned().ok_or_else(|| anyhow::anyhow!("accepted CD deployment has no controller incarnation"))?)?;
        let key = operation_key(&controller,&deployment.app,deployment.op,state);
        // Health belongs to live observations, not a later status queried during replay.
        let status = json!({"app":deployment.app,"sync":if deployment.succeeded {"Synced"} else {"OutOfSync"},"health":"Unknown","revision":deployment.revision,"message":deployment.message,"operation":{"id":deployment.op,"phase":if deployment.succeeded {"Succeeded"} else {"Failed"},"by":deployment.by}});
        let receipt = hub.store.call(move |c| {
            let tx = crate::store::tx(c)?;
            let receipt = stage_transition(&tx,&key,&controller,repo,&definition,state,status)?;
            tx.commit()?;
            Ok(receipt)
        }).await?;
        // Live status or an earlier retry may already own this operation's first accepted
        // payload. Read that immutable receipt instead of preparing from a later repo/ref.
        let lookup = receipt.clone();
        let (state,payload): (String,String) = hub.store.call(move |c| Ok(c.query_row("SELECT state,payload FROM ci_trigger_outbox WHERE id=?1", [&lookup], |row| Ok((row.get(0)?,row.get(1)?)))?)).await?;
        if state != "done" {
            anyhow::ensure!(matches!(state.as_str(), "pending" | "materialized"), "accepted CD transition {receipt} cannot retire its source after state {state}");
            let payload: super::schedule::outbox::Payload = serde_json::from_str(&payload)?;
            let super::schedule::outbox::Payload::Control { descriptor } = payload else { anyhow::bail!("accepted CD transition receipt has another payload kind"); };
            let transition = descriptor.get("transition").cloned().ok_or_else(|| anyhow::anyhow!("accepted CD transition receipt has no frozen transition"))?;
            if !deliver_transition(hub,&receipt,serde_json::from_value(transition)?).await? { return Ok(()); }
        }
        // The operation receipt is done only after its Workflow children and retirement
        // committed together. A crash here reuses that receipt and never fans out twice.
    }
    super::schedule::outbox::materialize(hub,id,Vec::new()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture(hub: &Hub) -> anyhow::Result<(crate::store::Repo,kiln_cd::apps::AppDef)> {
        let repo = hub.store.upsert_repo(crate::store::Repo { id:"deploy-controls".into(), name:"project/repository".into(), clone_url:"file:///not-used".into(), branches:vec!["main".into()], poll_seconds:60, enabled:true, created_at:0, last_polled_at:None, last_error:None, mode:"upstream".into(), default_branch:Some("main".into()), mirror:None }).await?;
        hub.store.set_ref(repo.id.clone(),"refs/heads/main".into(),"a".repeat(40)).await?;
        let mut app = kiln_cd::apps::from_argo(&json!({"apiVersion":"argoproj.io/v1alpha1","kind":"Application","metadata":{"name":"runtime-app"},"spec":{"project":"default","source":{"repoURL":"https://example.invalid/repo","path":"deploy"},"destination":{"namespace":"staging"}}}),None,&|_| None).map_err(|e| anyhow::anyhow!(e.0))?;
        app.origin = kiln_cd::apps::Origin::Deploy { repo:repo.name.clone() };
        app.environment = Some("staging".into());
        Ok((repo,app))
    }

    async fn controls(hub: &Hub) -> anyhow::Result<Vec<(String,String,Value)>> {
        let rows: Vec<(String,String,String)> = hub.store.call(|c| Ok(c.prepare("SELECT id,state,payload FROM ci_trigger_outbox WHERE kind='control' AND json_extract(payload,'$.descriptor.consumer')='cd'")?.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?)).await?;
        rows.into_iter().map(|(id,state,payload)| Ok((id,state,serde_json::from_str(&payload)?))).collect()
    }

    #[test]
    fn u04_cd_replayed_failure_keeps_accepted_context_and_waits_for_durable_workflow_children() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (repo,app) = fixture(&hub).await?;
            let store_path = hub.cfg.server.data_dir.join("cd-operation-incarnation");
            let store = crate::cd::store::Store::open(hub.store.database(), &store_path, Arc::new(hub.bus.clone()), hub.shutdown.clone()).await?;
            let controller = store.controller_scope().clone();
            accept_status(&hub,&controller,&app,json!({"sync":"OutOfSync","health":"Progressing"})).await?;
            accept_status(&hub,&controller,&app,json!({"sync":"OutOfSync","health":"Unknown","operation":{"id":7,"phase":"Failed"}})).await?;
            accept_status(&hub,&controller,&app,json!({"sync":"Synced","health":"Healthy","operation":{"id":10,"phase":"Succeeded"}})).await?;
            hub.store.set_ref(repo.id.clone(),"refs/heads/main".into(),"b".repeat(40)).await?;
            let mut changed = app.clone(); changed.environment = Some("production".into());
            let descriptor = |definition: &kiln_cd::apps::AppDef,op| json!({"consumer":"cd","controller":controller,"apps":[{"id":app.id(),"after":definition}],"deployment":kiln_cd::state::Deployment { app:app.id(), op, id:op, at_ms:op * 100, succeeded:false, by:Some("person".into()), ..Default::default() }});
            let duplicate = descriptor(&changed,7);
            let value = duplicate.clone();
            let parent = hub.store.call(move |c| super::super::schedule::outbox::control(c,"cd/duplicate-live",value)).await?;
            deliver_control(&hub,&parent,duplicate).await?;
            let rows = controls(&hub).await?;
            let sevens: Vec<_> = rows.iter().filter(|(_,_,payload)| payload.pointer("/descriptor/transition/status/operation/id") == Some(&json!(7))).collect();
            assert_eq!(sevens.len(),1,"live status and Records outcome share one operation receipt");
            assert_eq!(sevens.first().map(|(_,_,p)| p.pointer("/descriptor/transition/environment")),Some(Some(&json!("staging"))));
            assert_eq!(sevens.first().map(|(_,_,p)| p.pointer("/descriptor/transition/sha")),Some(Some(&json!("a".repeat(40)))));
            // This older failure was never observed live. The latest status is already op10.
            let delayed = descriptor(&app,6);
            let value = delayed.clone();
            let parent = hub.store.call(move |c| super::super::schedule::outbox::control(c,"cd/delayed-failure",value)).await?;
            deliver_control(&hub,&parent,delayed.clone()).await?;
            hub.store.set_ref(repo.id,"refs/heads/main".into(),"c".repeat(40)).await?;
            deliver_control(&hub,&parent,delayed.clone()).await?;
            let rows = controls(&hub).await?;
            let (receipt,state,payload) = rows.iter().find(|(_,_,payload)| payload.pointer("/descriptor/transition/status/operation/id") == Some(&json!(6))).ok_or_else(|| anyhow::anyhow!("older accepted failure was lost"))?;
            assert_eq!(state,"pending");
            assert!(rows.iter().any(|(id,state,_)| id == &parent && state == "pending"),"source cannot retire while workflow dispatch is not restored");
            let transition: Transition = serde_json::from_value(payload.pointer("/descriptor/transition").cloned().ok_or_else(|| anyhow::anyhow!("transition missing"))?)?;
            assert_eq!(transition.state,"failed");
            assert_eq!(transition.environment,"staging");
            assert_eq!(transition.sha,Some("b".repeat(40)),"retry retains the first accepted workflow commit");
            let saved: Accepted = serde_json::from_str(&hub.store.kv_get(seen_key(&app.id())).await?.ok_or_else(|| anyhow::anyhow!("status receipt missing"))?)?;
            assert_eq!(saved.seen.op,Some((10,"Succeeded".into())),"replaying records never rewinds live observations");
            // Simulate recovery after workflow preparation, with no runner or process actions.
            let path = ".kiln/diagnose.yaml";
            let (workflow,diagnostics) = kiln_ci::wf::parse_workflow(path,"on: [deploy]\njobs:\n  diagnose:\n    runs-on: [linux]\n    steps: [{run: echo fixture}]\n",kiln_ci::expr::Dialect::Kiln);
            anyhow::ensure!(diagnostics.iter().all(|d| d.severity != kiln_ci::wf::Severity::Error),"fixture workflow rejected");
            let loaded = super::super::load::Loaded { path:path.into(), dialect:kiln_ci::expr::Dialect::Kiln, runs:true, workflow:Some(workflow.ok_or_else(|| anyhow::anyhow!("fixture workflow missing"))?), diagnostics:Vec::new() };
            let commit = super::super::load::Commit { workflows:vec![loaded], ..Default::default() };
            let children = workflow_children(&transition,&commit,json!({"full_name":"project/repository"}),"accepted commit");
            assert_eq!(children.len(),1);
            super::super::schedule::outbox::materialize(&hub,receipt,children.clone()).await?;
            super::super::schedule::outbox::materialize(&hub,receipt,children).await?;
            // Crash after child commit, before retiring the Records control: replay only ACKs.
            deliver_control(&hub,&parent,delayed.clone()).await?;
            deliver_control(&hub,&parent,delayed).await?;
            let workflows: Vec<String> = hub.store.call(|c| Ok(c.prepare("SELECT payload FROM ci_trigger_outbox WHERE kind='workflow'")?.query_map([], |r| r.get(0))?.collect::<Result<_,_>>()?)).await?;
            assert_eq!(workflows.len(),1,"materialization and source replay cannot fan out twice");
            let child: super::super::schedule::outbox::Payload = serde_json::from_str(workflows.first().ok_or_else(|| anyhow::anyhow!("workflow child missing"))?)?;
            let super::super::schedule::outbox::Payload::Workflow(child) = child else { anyhow::bail!("wrong child kind"); };
            assert_eq!(child.event.sha,"b".repeat(40));
            assert_eq!(child.event.payload.pointer("/kiln/deploy/environment"),Some(&json!("staging")));
            assert_eq!(child.event.action,"failed");
            assert!(controls(&hub).await?.iter().any(|(id,state,_)| id == &parent && state == "done"));
            // Engine ids restart at one. Preserve the old accepted Records scope across
            // reopen, while a new controller's identical app/op/phase remains independent.
            let failed_one = json!({"sync":"OutOfSync","health":"Unknown","operation":{"id":1,"phase":"Failed"}});
            accept_status(&hub,&controller,&app,failed_one.clone()).await?;
            store.submit_with_app(kiln_cd::state::Command::Deployed { deployment:Box::new(kiln_cd::state::Deployment { app:app.id(), op:1, succeeded:false, ..Default::default() }), limit:1 },Some(app.clone())).await?.map_err(anyhow::Error::msg)?;
            drop(store);
            let reopened = crate::cd::store::Store::open(hub.store.database(), &store_path, Arc::new(hub.bus.clone()), hub.shutdown.clone()).await?;
            let next_controller = reopened.controller_scope();
            assert_ne!(next_controller,&controller,"a new engine instance needs a new operation identity");
            accept_status(&hub,next_controller,&app,failed_one.clone()).await?;
            accept_status(&hub,next_controller,&app,failed_one).await?;
            reopened.drain_controls(&hub.store).await?;
            let rows = controls(&hub).await?;
            let (source,_,payload) = rows.iter().find(|(_,_,payload)| payload.pointer("/descriptor/deployment/op") == Some(&json!(1))).ok_or_else(|| anyhow::anyhow!("old pending deployment missing"))?;
            let old_descriptor = payload.get("descriptor").cloned().ok_or_else(|| anyhow::anyhow!("old pending descriptor missing"))?;
            assert_eq!(old_descriptor.get("controller"),Some(&json!(controller)),"pending replay keeps its original incarnation");
            deliver_control(&hub,source,old_descriptor.clone()).await?;
            deliver_control(&hub,source,old_descriptor).await?;
            let rows = controls(&hub).await?;
            let ones: Vec<_> = rows.iter().filter(|(_,_,payload)| payload.pointer("/descriptor/transition/status/operation/id") == Some(&json!(1))).collect();
            assert_eq!(ones.len(),2,"same-scope live/Records retries deduplicate, reused operation ids across incarnations do not");
            for scope in [&controller,next_controller] {
                assert_eq!(ones.iter().filter(|(_,_,payload)| payload.pointer("/descriptor/controller") == Some(&json!(scope))).count(),1);
            }
            let saved: Accepted = serde_json::from_str(&hub.store.kv_get(seen_key(&app.id())).await?.ok_or_else(|| anyhow::anyhow!("new status receipt missing"))?)?;
            assert_eq!(saved.controller.as_ref(),Some(next_controller),"older Records replay never changes the live controller");
            hub.shutdown.cancel();
            Ok(())
        })
    }

    #[test]
    fn u04_cd_status_and_transition_commit_together_before_publication() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (_,app) = fixture(&hub).await?;
            let store = crate::cd::store::Store::open(hub.store.database(), &hub.cfg.server.data_dir.join("cd-status-acceptance"), Arc::new(hub.bus.clone()), hub.shutdown.clone()).await?;
            let controller = store.controller_scope().clone();
            accept_status(&hub,&controller,&app,json!({"sync":"Synced","health":"Healthy"})).await?;
            accept_status(&hub,&controller,&app,json!({"sync":"Synced","health":"Degraded"})).await?;
            accept_status(&hub,&controller,&app,json!({"sync":"Synced","health":"Healthy"})).await?;
            let rows = controls(&hub).await?;
            assert_eq!(rows.len(),2,"rapid degrade/recover changes are durable before UI batching");
            assert!(rows.iter().any(|(_,_,payload)| payload.pointer("/descriptor/transition/state") == Some(&json!("degraded"))));
            assert!(rows.iter().any(|(_,_,payload)| payload.pointer("/descriptor/transition/state") == Some(&json!("synced"))));
            let missing = json!({"consumer":"cd","apps":[],"deployment":kiln_cd::state::Deployment { app:app.id(), op:99, ..Default::default() }});
            let value = missing.clone();
            let source = hub.store.call(move |c| super::super::schedule::outbox::control(c,"cd/no-declared-origin",value)).await?;
            deliver_control(&hub,&source,missing).await?;
            assert!(controls(&hub).await?.iter().any(|(id,state,_)| id == &source && state == "done"),"valid history without an automation definition explicitly skips");
            let before = hub.store.kv_get(seen_key(&app.id())).await?;
            hub.store.call(|c| { c.execute("DROP TABLE ci_trigger_outbox",[])?; Ok(()) }).await?;
            assert!(accept_status(&hub,&controller,&app,json!({"sync":"Synced","health":"Degraded"})).await.is_err());
            assert_eq!(hub.store.kv_get(seen_key(&app.id())).await?,before,"refused intent leaves the prior accepted status available for retry");
            hub.shutdown.cancel();
            Ok(())
        })
    }

    fn seen(sync: &str, health: &str, op: Option<(i64, &str)>) -> Seen {
        Seen::of(&json!({ "sync": sync, "health": health, "operation": op.map(|(id, phase)| json!({ "id": id, "phase": phase })) }))
    }

    #[test]
    fn w03_r09_deploy_states() {
        let healthy = seen("Synced", "Healthy", Some((1, "Succeeded")));
        assert_eq!(moved(None, &healthy, false), None, "the first status only records");
        assert_eq!(moved(Some(&seen("OutOfSync", "Progressing", Some((1, "Running")))), &healthy, false), Some("synced"));
        assert_eq!(moved(Some(&healthy), &healthy, false), None, "nothing moved");
        assert_eq!(moved(Some(&healthy), &seen("Synced", "Healthy", Some((2, "Succeeded"))), false), Some("synced"), "a sync of it succeeded again");
        assert_eq!(moved(Some(&healthy), &seen("Synced", "Degraded", Some((1, "Succeeded"))), false), Some("degraded"));
        assert_eq!(moved(Some(&seen("Synced", "Degraded", None)), &seen("Synced", "Degraded", None), false), None, "still degraded");
        assert_eq!(moved(Some(&healthy), &seen("OutOfSync", "Healthy", Some((2, "Failed"))), false), Some("failed"));
        assert_eq!(moved(Some(&seen("OutOfSync", "Healthy", Some((2, "Failed")))), &seen("OutOfSync", "Healthy", Some((2, "Failed"))), false), None, "the same failure");
    }
}
