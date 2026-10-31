//! U04/W03-R10: durable trigger intent in Amphora, with Quiver wakeups. Delivery is at least
//! once. Deterministic run/command identities and the consumer's transaction retire an intent;
//! an in-memory send never does. This is recovery state for the one scheduler, not a scheduler.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use kiln_seams::bus::{Bus as _, Class, Topic};
use serde::{Deserialize, Serialize};

use crate::hub::Hub;
use crate::sql::{Connection, OptionalExtension as _, params};
use crate::store::Repo;
use super::super::dispatch::{Given, Msg, Start};

const PAYLOAD_MAX: usize = 8 << 20;
const PAGE: usize = 128;
const RETRY: Duration = Duration::from_secs(5);
const WAKE_TOPIC: &str = "ci/triggers";
const WAKE_RETENTION: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkflowSource {
    pub repo: Repo,
    pub git_ref: String,
    pub sha: String,
    pub path: String,
    pub cron: String,
    pub inputs: serde_json::Value,
    /// Already parsed for the default branch; alternate branches are loaded after acceptance.
    pub workflow: Option<kiln_ci::wf::Workflow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Workflow {
    pub repo: Repo,
    pub event: kiln_ci::trigger::Event,
    pub path: String,
    pub workflow: kiln_ci::wf::Workflow,
    pub message: String,
    pub author: String,
    /// Resolved once and persisted before publication, so a retry keeps the same action pins.
    pub actions: Option<BTreeMap<String, kiln_ci::wire::ActionRef>>,
    pub inputs: serde_json::Value,
    #[serde(default)]
    pub error: Option<String>,
}

/// W03-R07/R14: complete accepted source, independent of mutable refs/configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Ingress {
    pub repo: Repo,
    pub event: kiln_ci::trigger::Event,
    pub workflows: Vec<Workflow>,
    #[serde(default)]
    pub trunk_before: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Payload {
    /// Accepted domain control intent; UI publication is only a wakeup.
    Control { descriptor: serde_json::Value },
    AttachmentIntent { attachment: String, trigger: Box<super::super::attach::PersistedTrigger> },
    Message { session: String, text: String },
    WorkflowSource(Box<WorkflowSource>),
    Workflow(Box<Workflow>),
    Attachment { attachment: String, minute: i64, cron: String },
    Skipped { reason: String },
    Rejected { reason: String },
    Ingress(Box<Ingress>),
}

impl Payload {
    fn kind(&self) -> &'static str {
        match self {
            Self::Ingress(_) => "ingress",
            Self::Control { .. } => "control",
            Self::AttachmentIntent { .. } => "attachment",
            Self::Message { .. } => "message",
            Self::WorkflowSource(_) | Self::Workflow(_) => "workflow",
            Self::Attachment { .. } => "attachment",
            Self::Skipped { .. } => "skipped",
            Self::Rejected { .. } => "rejected",
        }
    }
}

/// The occurrence key includes its declared owner, path and index, rather than mutable heads.
pub(crate) struct Occurrence<'a> {
    pub owner: &'a str,
    pub path: &'a str,
    pub entry: usize,
    pub minute: i64,
    pub cron: &'a str,
}

pub(crate) fn run_id(id: &str) -> String {
    crate::util::sha256_hex(format!("kiln/trigger/run/v1/{id}").as_bytes()).chars().take(32).collect()
}

fn id(o: &Occurrence<'_>) -> anyhow::Result<String> {
    Ok(crate::util::sha256_hex(&serde_json::to_vec(&(o.owner, o.path, o.entry, o.minute))?))
}

/// The occurrence and its complete recovery payload are one durable transaction. The legacy
/// fire key remains the compatibility deduplication boundary, including pre-upgrade fires.
pub(crate) async fn enqueue(hub: &Hub, o: Occurrence<'_>, payload: Payload) -> anyhow::Result<bool> {
    let id = id(&o)?;
    let run = run_id(&id);
    let kind = payload.kind().to_owned();
    let encoded = serde_json::to_string(&payload)?;
    anyhow::ensure!(encoded.len() <= PAYLOAD_MAX, "scheduled trigger exceeds {PAYLOAD_MAX} bytes");
    let (owner, path, entry, minute, cron) = (o.owner.to_owned(), o.path.to_owned(), i64::try_from(o.entry)?, o.minute, o.cron.to_owned());
    let fresh = hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        if tx.execute("INSERT OR IGNORE INTO ci_schedule_fires (repo_id, path, entry, minute, cron) VALUES (?1, ?2, ?3, ?4, ?5)", params![owner, path, entry, minute, cron])? == 0 {
            return Ok(false);
        }
        let now = crate::util::now_unix();
        tx.execute("INSERT INTO ci_trigger_outbox (id, run_id, kind, payload, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)", params![id, run, kind, encoded, now])?;
        tx.commit()?;
        Ok(true)
    }).await?;
    if fresh { kick(hub).await; }
    Ok(fresh)
}

pub(crate) async fn kick(hub: &Hub) {
    let wake = async {
        hub.bus.create(WAKE_TOPIC, Topic { class: Class::Ephemeral, retain: WAKE_RETENTION }).await?;
        hub.bus.publish(WAKE_TOPIC, None, Vec::new()).await
    }.await;
    if let Err(error) = wake { tracing::warn!(error = %error, "trigger wakeup refused; accepted work remains in Amphora"); }
}

pub(crate) async fn accept(hub: &Hub, key: String, payload: Payload) -> anyhow::Result<String> {
    let id = hub.store.call(move |c| insert(c, &key, &payload)).await?;
    kick(hub).await;
    Ok(id)
}

/// One immutable workflow child shape for every accepted control source.
pub(crate) fn workflow(repo: &Repo, event: &kiln_ci::trigger::Event, loaded: &super::super::load::Loaded, message: &str, author: &str) -> Payload {
    let error = loaded.has_errors().then(|| super::super::events::startup_error(loaded));
    Payload::Workflow(Box::new(Workflow { repo: repo.clone(), event: event.clone(), path: loaded.path.clone(), workflow: loaded.workflow.clone().unwrap_or_default(), message: message.into(), author: author.into(), actions: None, inputs: serde_json::Value::Null, error }))
}

/// U04/U14: call inside the domain mutation's existing transaction. Repeating its stable
/// source key keeps the first accepted descriptor, including across later snapshot revisions.
/// No separate event log or bus is introduced: this is the scheduler's canonical intent table.
pub(crate) fn control(c: &Connection<'_>, key: &str, descriptor: serde_json::Value) -> anyhow::Result<String> {
    anyhow::ensure!(!key.is_empty() && key.len() <= 4096, "invalid control source key");
    insert(c, &format!("control/{key}"), &Payload::Control { descriptor })
}

pub(crate) fn insert(c: &Connection<'_>, key: &str, payload: &Payload) -> anyhow::Result<String> {
    let id = crate::util::sha256_hex(format!("kiln/trigger/intent/v1/{key}").as_bytes());
    let encoded = serde_json::to_string(payload)?;
    anyhow::ensure!(encoded.len() <= PAYLOAD_MAX, "control trigger exceeds {PAYLOAD_MAX} bytes");
    let now = crate::util::now_unix();
    c.execute("INSERT INTO ci_trigger_outbox (id, run_id, kind, payload, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5) ON CONFLICT(id) DO NOTHING", params![id, run_id(&id), payload.kind(), encoded, now])?;
    Ok(id)
}

/// Prepare all children before publication, then atomically hand recovery from source to the
/// exact accepted fanout. A retry before this commit has performed no external side effects.
pub(crate) async fn materialize(hub: &Hub, source: &str, children: Vec<(String, Payload)>) -> anyhow::Result<()> {
    materialize_with_deliveries(hub, source, children, Vec::new()).await
}

/// W19-R07/U04: the first successful materialization freezes webhook recipients together with
/// attachment/workflow children. Preparation or SQL refusal leaves the parent retryable; after
/// commit, even an empty recipient set is final and a later subscription cannot join this event.
pub(crate) async fn materialize_with_deliveries(hub: &Hub, source: &str, children: Vec<(String, Payload)>, deliveries: Vec<super::super::webhooks::DeliveryIntent>) -> anyhow::Result<()> {
    anyhow::ensure!(children.len() <= 4096, "control source fanout exceeds 4096 intents");
    anyhow::ensure!(deliveries.len() <= 4096, "control source exceeds 4096 webhook recipients");
    let source = source.to_owned();
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_trigger_outbox WHERE id = ?1 AND state IN ('pending', 'materialized'))", [&source], |r| r.get(0))?;
        if !pending { return Ok(()); }
        super::super::webhooks::stage(&tx, &deliveries)?;
        for (key, payload) in children { insert(&tx, &format!("{source}/{key}"), &payload)?; }
        tx.execute("UPDATE ci_trigger_outbox SET state = 'done', updated_at = ?2 WHERE id = ?1 AND state IN ('pending', 'materialized')", params![source, crate::util::now_unix()])?;
        tx.commit()?;
        Ok(())
    }).await?;
    kick(hub).await;
    Ok(())
}

pub(crate) async fn finish(hub: &Hub, id: &str, failed: bool, diagnostic: Option<String>) -> anyhow::Result<()> {
    let id = id.to_owned();
    hub.store.call(move |c| {
        c.execute("UPDATE ci_trigger_outbox SET state = ?2, diagnostic = ?3, updated_at = ?4 WHERE id = ?1 AND state IN ('pending', 'materialized')", params![id, if failed { "failed" } else { "done" }, diagnostic, crate::util::now_unix()])?;
        Ok(())
    }).await
}

/// Validate the persisted correlation before allowing an occurrence to name a run. A stale or
/// hostile command cannot use another occurrence to adopt unrelated run metadata.
pub(crate) fn validate(c: &Connection<'_>, id: &str, repo: &str, path: &str, event: &str, sha: &str, model: &str) -> anyhow::Result<String> {
    let (run, kind, text, state): (String, String, String, String) = c.query_row("SELECT run_id, kind, payload, state FROM ci_trigger_outbox WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    if !matches!(state.as_str(), "pending" | "materialized") {
        let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE id = ?1)", [&run], |r| r.get(0))?;
        anyhow::ensure!(exists, "retired trigger {id} cannot recreate a removed run");
    }
    let payload: Payload = serde_json::from_str(&text)?;
    let Payload::Workflow(w) = payload else { anyhow::bail!("trigger {id} is not a workflow") };
    anyhow::ensure!(kind == "workflow" && run == run_id(id) && w.repo.id == repo && w.path == path && w.event.name == event && w.event.sha == sha && serde_json::to_string(&w.workflow)? == model, "trigger {id} does not own this run");
    Ok(run)
}

/// A complete saved plan (or a terminal startup failure) owns recovery once materialized.
pub(crate) async fn materialized(hub: &Hub, id: &str, w: &Start) -> anyhow::Result<bool> {
    let (id, repo, path, event, sha, model) = (id.to_owned(), w.repo.id.clone(), w.path.clone(), w.event.name.clone(), w.event.sha.clone(), serde_json::to_string(&*w.workflow)?);
    hub.store.call(move |c| {
        let run = validate(c, &id, &repo, &path, &event, &sha, &model)?;
        let row: Option<(String, String)> = c.query_row("SELECT r.status, p.input FROM runs r JOIN ci_plans p ON p.run_id = r.id WHERE r.id = ?1", [&run], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let done = row.is_some_and(|(status, input)| crate::store::Status::parse(&status).is_terminal() || !input.is_empty());
        Ok(done)
    }).await
}

/// U04: immediate callers deliver the first accepted descriptor through this same owner.
/// A retry must never substitute a newly computed payload for the durable source.
pub(crate) async fn deliver_accepted(hub: &Arc<Hub>, id: &str) -> anyhow::Result<()> {
    let key = id.to_owned();
    let row: Option<(String, String)> = hub.store.call(move |c| Ok(c.query_row("SELECT kind, payload FROM ci_trigger_outbox WHERE id = ?1 AND state IN ('pending', 'materialized')", [key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await?;
    let Some((kind, text)) = row else { return Ok(()); };
    if text.len() > PAYLOAD_MAX {
        return finish(hub, id, true, Some("persisted trigger exceeds the delivery size limit".into())).await;
    }
    deliver(hub, id, &kind, &text).await
}

async fn deliver(hub: &Arc<Hub>, id: &str, kind: &str, text: &str) -> anyhow::Result<()> {
    let payload: Payload = match serde_json::from_str(text) {
        Ok(p) => p,
        Err(e) => return finish(hub, id, true, Some(format!("invalid persisted trigger payload: {e}"))).await,
    };
    if payload.kind() != kind {
        return finish(hub, id, true, Some("persisted trigger kind does not match its payload".into())).await;
    }
    match payload {
        Payload::Ingress(source) => {
            if !hub.ci.enabled() || hub.ci.restored().is_none() { return Ok(()); }
            if let Some(before) = &source.trunk_before {
                let branch = source.event.git_ref.strip_prefix("refs/heads/").unwrap_or_default();
                super::super::notify::trunk_moved(hub, &source.repo, branch, before, &source.event.sha).await;
            }
            if source.event.name == "pull_request" && source.event.action == "closed"
                && let Some(n) = source.event.payload.pointer("/pull_request/number").and_then(serde_json::Value::as_u64)
            {
                let merged = source.event.payload.pointer("/pull_request/merged").and_then(serde_json::Value::as_bool).unwrap_or(false);
                super::super::previews::pull_request_closed(hub, &source.repo, n, merged).await;
            }
            let children = source.workflows.into_iter().map(|w| (w.path.clone(), Payload::Workflow(Box::new(w)))).collect();
            materialize(hub, id, children).await
        }
        Payload::Control { descriptor } => {
            match descriptor.get("consumer").and_then(serde_json::Value::as_str) {
                Some("conversation") => {
                    let source: super::super::diffs::Conversation = match serde_json::from_value(descriptor.get("conversation").cloned().unwrap_or_default()) {
                        Ok(source) => source,
                        Err(error) => return finish(hub,id,true,Some(format!("invalid persisted conversation snapshot: {error}"))).await,
                    };
                    let event = source.event.clone();
                    if !matches!(event.get("type").and_then(serde_json::Value::as_str),Some("comment" | "review")) || source.diff.repo != source.repo.name || event.get("diff").and_then(serde_json::Value::as_str) != Some(source.diff.name.as_str()) || source.version.n == 0 {
                        return finish(hub,id,true,Some("invalid persisted conversation event correlation".into())).await;
                    }
                    if hub.cfg.ci.enabled && (!hub.ci.enabled() || hub.ci.restored().is_none()) { return Ok(()); }
                    let mut children = super::super::diffs::conversation(hub, source).await?;
                    children.push(("attachment-event".into(), Payload::Control { descriptor: serde_json::json!({"consumer":"attachment", "event":event}) }));
                    materialize(hub, id, children).await
                }
                Some("attachment") => {
                    let Some(event) = descriptor.get("event") else { return finish(hub, id, true, Some("attachment descriptor has no event".into())).await; };
                    let children = super::super::attach::control(hub, event).await?;
                    let deliveries = if event.get("type").and_then(serde_json::Value::as_str).is_some_and(super::super::webhooks::durable_source_kind) {
                        super::super::webhooks::prepare_source(hub, id, event).await?
                    } else { Vec::new() };
                    materialize_with_deliveries(hub, id, children, deliveries).await
                }
                Some("upgrade") => {
                    let job: crate::updates::Job = match serde_json::from_value(descriptor.get("job").cloned().unwrap_or_default()) {
                        Ok(job) => job,
                        Err(error) => return finish(hub,id,true,Some(format!("invalid persisted upgrade snapshot: {error}"))).await,
                    };
                    let children = super::super::attach::upgrade_control(hub, job).await?;
                    materialize(hub, id, children).await
                }
                Some("cd") => super::super::cd_events::deliver_control(hub, id, descriptor).await,
                _ => finish(hub, id, true, Some("invalid control descriptor consumer".into())).await,
            }
        }
        Payload::AttachmentIntent { attachment, trigger } => super::super::attach::deliver_trigger(hub, id, &attachment, *trigger).await,
        Payload::Message { session, text } => {
            crate::workspace::follow::deliver_once(hub, &session, text, &format!("trigger:{id}")).await?;
            finish(hub, id, false, None).await
        }
        Payload::Skipped { reason } => finish(hub, id, false, Some(reason)).await,
        Payload::Attachment { attachment, minute, cron } => super::super::attach::deliver_schedule(hub, id, &attachment, minute, &cron).await,
        Payload::Rejected { reason } => finish(hub, id, true, Some(reason)).await,
        Payload::WorkflowSource(source) => {
            if !hub.ci.enabled() || hub.ci.restored().is_none() { return Ok(()) }
            let workflow = Box::new(super::prepare(hub, *source).await?);
            let encoded = serde_json::to_string(&Payload::Workflow(workflow.clone()))?;
            anyhow::ensure!(encoded.len() <= PAYLOAD_MAX, "prepared scheduled trigger exceeds {PAYLOAD_MAX} bytes");
            let (key, expected) = (id.to_owned(), text.to_owned());
            let installed = hub.store.call(move |c| Ok(c.execute("UPDATE ci_trigger_outbox SET payload = ?2, updated_at = ?3 WHERE id = ?1 AND state = 'pending' AND payload = ?4", params![key, encoded, crate::util::now_unix(), expected])? == 1)).await?;
            if installed { deliver_workflow(hub, id, *workflow).await } else { Ok(()) }
        }
        Payload::Workflow(w) => deliver_workflow(hub, id, *w).await,
    }
}

async fn deliver_workflow(hub: &Arc<Hub>, id: &str, mut w: Workflow) -> anyhow::Result<()> {
        if !hub.ci.enabled() || hub.ci.restored().is_none() { return Ok(()) }
        if let Some(error) = &w.error { return fail_workflow(hub, id, &w, error).await; }
        if let Err(why) = kiln_ci::trigger::matches(&w.workflow, &w.event) {
            return finish(hub, id, false, Some(format!("workflow does not match the scheduled event: {why}"))).await;
        }
        if w.actions.is_none() && w.error.is_none() {
            let expected = serde_json::to_string(&Payload::Workflow(Box::new(w.clone())))?;
            match super::super::events::resolve_actions(hub, &w.repo, &w.event, &w.path, &w.workflow).await {
                Ok(actions) => w.actions = Some(actions),
                Err(why) => w.error = Some(why),
            }
            let encoded = serde_json::to_string(&Payload::Workflow(Box::new(w.clone())))?;
            anyhow::ensure!(encoded.len() <= PAYLOAD_MAX, "resolved scheduled trigger exceeds {PAYLOAD_MAX} bytes");
            let key = id.to_owned();
            let installed = hub.store.call(move |c| Ok(c.execute("UPDATE ci_trigger_outbox SET payload = ?2, updated_at = ?3 WHERE id = ?1 AND state = 'pending' AND payload = ?4", params![key, encoded, crate::util::now_unix(), expected])? == 1)).await?;
            if !installed { return Ok(()); }
        }
        if let Some(error) = &w.error {
            return fail_workflow(hub, id, &w, error).await;
        }
        hub.ci.send(Msg::TriggerReady { id: id.to_owned() });
        Ok(())
}

async fn fail_workflow(hub: &Hub, id: &str, w: &Workflow, error: &str) -> anyhow::Result<()> {
    let run = run_id(id);
    let n = super::super::runs::NewRun { id: run.clone(), workflow: w.path.clone(), name: w.workflow.name.clone().unwrap_or_else(|| w.path.clone()), event: w.event.name.clone(), branch: w.event.ref_name().to_owned(), sha: w.event.sha.clone(), message: w.message.clone(), author: w.author.clone(), actor: w.event.actor.clone(), trigger: serde_json::json!({ "event": w.event.name, "ref": w.event.git_ref }), trusted: w.event.trusted, priority: 50, payload: w.event.payload.clone(), model: serde_json::to_string(&w.workflow)?, input: String::new() };
    super::super::runs::create_trigger(hub, &w.repo, n, Some(id.to_owned())).await?;
    super::super::runs::finish_run(hub, &run, crate::store::Status::Failed, "startup_failure", Some(error.to_owned())).await?;
    finish(hub, id, false, Some(error.to_owned())).await?;
    hub.emit_run(&run).await;
    Ok(())
}

/// Loading the compact command key happens inside the dispatcher, so retries never queue a
/// second copy of an entire workflow payload in memory.
pub(crate) async fn start(hub: &Hub, id: &str) -> anyhow::Result<Option<Start>> {
    let key = id.to_owned();
    let text: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT payload FROM ci_trigger_outbox WHERE id = ?1 AND kind = 'workflow' AND state IN ('pending', 'materialized')", [key], |r| r.get(0)).optional()?)).await?;
    let Some(text) = text else { return Ok(None) };
    let w = match serde_json::from_str(&text) {
        Ok(Payload::Workflow(w)) => w,
        Ok(_) => { finish(hub, id, true, Some("workflow trigger has another payload kind".into())).await?; return Ok(None); }
        Err(e) => { finish(hub, id, true, Some(format!("invalid persisted workflow trigger: {e}"))).await?; return Ok(None); }
    };
    if let Some(error) = &w.error { fail_workflow(hub, id, &w, error).await?; return Ok(None); }
    let actions = w.actions.ok_or_else(|| anyhow::anyhow!("workflow trigger's action pins are not ready"))?;
    Ok(Some(Start { repo: w.repo, event: w.event, path: w.path, workflow: Arc::new(w.workflow), message: w.message, author: w.author, actions, inputs: w.inputs, given: Given { durable_trigger: Some(id.to_owned()), ..Given::default() } }))
}

pub(crate) async fn drain(hub: &Arc<Hub>) -> anyhow::Result<bool> {
    let now = crate::util::now_unix();
    let ids: Vec<String> = hub.store.call(move |c| {
        // Workflow ingress can be disabled independently of already-declared attachments.
        let mut q = c.prepare("SELECT id FROM ci_trigger_outbox WHERE state IN ('pending', 'materialized') AND available_at <= ?1 ORDER BY available_at, updated_at, id LIMIT ?2")?;
        Ok(q.query_map(params![now, i64::try_from(PAGE)?], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }).await?;
    let more = ids.len() == PAGE;
    for id in ids {
        let key = id.clone();
        let accepted = hub.store.call(move |c| Ok(c.execute("UPDATE ci_trigger_outbox SET available_at = ?2, updated_at = ?3 WHERE id = ?1 AND state IN ('pending', 'materialized') AND available_at <= ?3", params![key, now.saturating_add(5), now])? == 1)).await?;
        if !accepted { continue; }
        // One pass holds at most one bounded payload, not PAGE payloads. A retry cooldown
        // rotates unavailable consumers without throttling the next ready page.
        if let Err(e) = deliver_accepted(hub, &id).await {
            let diagnostic = format!("{e:#}").chars().take(4096).collect::<String>();
            tracing::warn!(trigger = %id, error = %diagnostic, "trigger delivery will retry from Amphora");
            hub.store.call(move |c| {
                c.execute("UPDATE ci_trigger_outbox SET attempts = attempts + 1, diagnostic = ?2, updated_at = ?3 WHERE id = ?1 AND state IN ('pending', 'materialized')", params![id, diagnostic, crate::util::now_unix()])?;
                Ok(())
            }).await?;
        }
    }
    Ok(more)
}

pub(crate) fn spawn(hub: Arc<Hub>) {
    crate::rt::spawn(async move {
        loop {
            // Capture the offset before scanning, so a publish racing the scan still wakes us.
            // An unavailable or lagged ephemeral topic only costs latency: Amphora owns work.
            let next = match hub.bus.bounds(WAKE_TOPIC).await {
                Ok((_, next)) => Some(next),
                Err(_) => {
                    if let Err(e) = hub.bus.create(WAKE_TOPIC, Topic { class: Class::Ephemeral, retain: WAKE_RETENTION }).await {
                        tracing::warn!(error = %e, "trigger wake topic unavailable; recovery continues by scan");
                    }
                    None
                }
            };
            match drain(&hub).await {
                Ok(true) => continue,
                Ok(false) => {},
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "trigger recovery scan failed"),
            }
            if let Some(next) = next {
                futures::select! {
                    wake = hub.bus.wait(WAKE_TOPIC, next).fuse() => {
                        if let Err(e) = wake { tracing::warn!(error = %e, "trigger wake refused; resuming from the durable outbox"); }
                    },
                    live = hub.shutdown.sleep(RETRY).fuse() => { if !live { break; } },
                }
            } else if !hub.shutdown.sleep(RETRY).await { break; }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W19-R07/U04: webhook rows, exact children and source retirement share one commit.
    /// No worker or HTTP is started; recovery uses only the existing pending tables.
    #[test]
    fn w19_r07_source_materialization_freezes_webhooks_and_rolls_back_refused_fanout() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let event = serde_json::json!({"type":"runner.offline","runner":{"name":"runner","labels":["linux"],"gone_for":180}});
            hub.store.call(|c| {
                c.execute("INSERT INTO ci_integrations (id,kind,name,config,scope_kind,scope_id,created_by,created_at,updated_at) VALUES ('recipient-1','webhook','audit',?1,'server','','person',0,0)", [serde_json::json!({"events":["runner.offline"]}).to_string()])?;
                Ok(())
            }).await?;
            let accepted = event.clone();
            let source = hub.store.call(move |c| control(c, "offline-atomic-fixture", serde_json::json!({"consumer":"attachment","event":accepted}))).await?;
            let children = || vec![("child".into(), Payload::Skipped { reason: "frozen child".into() })];
            let deliveries = super::super::super::webhooks::prepare_source(&hub, &source, &event).await?;
            assert_eq!(deliveries.len(), 1);
            hub.store.call(|c| { c.execute("ALTER TABLE ci_integrations RENAME TO held_integrations", [])?; Ok(()) }).await?;
            assert!(super::super::super::webhooks::prepare_source(&hub, &source, &event).await.is_err());
            hub.store.call(|c| { c.execute("ALTER TABLE held_integrations RENAME TO ci_integrations", [])?; Ok(()) }).await?;
            hub.store.call(|c| {
                c.execute("CREATE TRIGGER refuse_webhook BEFORE INSERT ON webhook_deliveries BEGIN SELECT RAISE(ABORT,'injected staging refusal'); END", [])?;
                Ok(())
            }).await?;
            assert!(materialize_with_deliveries(&hub, &source, children(), deliveries.clone()).await.is_err());
            hub.store.call(|c| {
                c.execute("DROP TRIGGER refuse_webhook", [])?;
                c.execute("CREATE TRIGGER refuse_child BEFORE INSERT ON ci_trigger_outbox WHEN NEW.kind = 'skipped' BEGIN SELECT RAISE(ABORT,'injected child refusal'); END", [])?;
                Ok(())
            }).await?;
            assert!(materialize_with_deliveries(&hub, &source, children(), deliveries.clone()).await.is_err());
            let key = source.clone();
            let state: (String, i64, i64) = hub.store.call(move |c| Ok((
                c.query_row("SELECT state FROM ci_trigger_outbox WHERE id = ?1", [key], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM webhook_deliveries", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM ci_trigger_outbox WHERE kind = 'skipped'", [], |r| r.get(0))?,
            ))).await?;
            assert_eq!(state, ("pending".into(), 0, 0));
            hub.store.call(|c| { c.execute("DROP TRIGGER refuse_child", [])?; Ok(()) }).await?;
            materialize_with_deliveries(&hub, &source, children(), deliveries.clone()).await?;
            // Crash after commit/before wake: retry, even with a new subscriber, cannot refanout.
            hub.store.call(|c| {
                c.execute("INSERT INTO ci_integrations (id,kind,name,config,scope_kind,scope_id,created_by,created_at,updated_at) VALUES ('recipient-2','webhook','later',?1,'server','','person',0,0)", [serde_json::json!({"events":["runner.offline"]}).to_string()])?;
                Ok(())
            }).await?;
            let later = super::super::super::webhooks::prepare_source(&hub, &source, &event).await?;
            assert_eq!(later.len(), 2);
            materialize_with_deliveries(&hub, &source, children(), later).await?;
            let key = source.clone();
            let state: (String, i64, i64) = hub.store.call(move |c| Ok((
                c.query_row("SELECT state FROM ci_trigger_outbox WHERE id = ?1", [key], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM webhook_deliveries WHERE integration_id = 'recipient-1' AND status = 'pending'", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM ci_trigger_outbox WHERE kind = 'skipped'", [], |r| r.get(0))?,
            ))).await?;
            assert_eq!(state, ("done".into(), 1, 1));
            let total: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM webhook_deliveries", [], |r| r.get(0))?)).await?;
            assert_eq!(total, 1);
            let empty_event = serde_json::json!({"type":"run.succeeded","repo":{"id":"repo","name":"test/repo"}});
            let saved_event = empty_event.clone();
            let empty_source = hub.store.call(move |c| control(c, "empty-fanout-fixture", serde_json::json!({"consumer":"attachment","event":saved_event}))).await?;
            let empty = super::super::super::webhooks::prepare_source(&hub, &empty_source, &empty_event).await?;
            assert!(empty.is_empty());
            materialize_with_deliveries(&hub, &empty_source, Vec::new(), empty).await?;
            hub.store.call(|c| { c.execute("UPDATE ci_integrations SET config = ?1", [serde_json::json!({"events":["*"]}).to_string()])?; Ok(()) }).await?;
            let added = super::super::super::webhooks::prepare_source(&hub, &empty_source, &empty_event).await?;
            assert_eq!(added.len(), 2);
            materialize_with_deliveries(&hub, &empty_source, Vec::new(), added).await?;
            let total: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM webhook_deliveries", [], |r| r.get(0))?)).await?;
            assert_eq!(total, 1, "an accepted empty recipient set is final");
            Ok(())
        })
    }

    /// U04: notifications and their in-memory publisher may disappear at either crash boundary.
    /// Re-reading the accepted intent must recover the same run, without a second run number.
    #[test]
    fn u04_lost_publish_and_run_creation_replay_keep_one_run() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo = hub.store.upsert_repo(Repo { id: "cron-repo".into(), name: "test/cron".into(), clone_url: "file:///not-used".into(), branches: vec!["main".into()], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: Some("main".into()), mirror: None }).await?;
            let path = ".kiln/nightly.yaml";
            let (workflow, diagnostics) = kiln_ci::wf::parse_workflow(path, "name: nightly\non:\n  schedule: ['0 6 * * *']\njobs:\n  check:\n    runs-on: [linux]\n    steps: [{run: echo ok}]\n", kiln_ci::expr::Dialect::Kiln);
            anyhow::ensure!(diagnostics.iter().all(|d| d.severity != kiln_ci::wf::Severity::Error));
            let workflow = workflow.ok_or_else(|| anyhow::anyhow!("fixture did not parse"))?;
            let occurrence = || Occurrence { owner: &repo.id, path, entry: 0, minute: 1_792_216_800, cron: "0 6 * * *" };
            let trigger = id(&occurrence())?;
            let event = kiln_ci::trigger::Event { name: "schedule".into(), action: "0 6 * * *".into(), repo: repo.name.clone(), git_ref: "refs/heads/main".into(), sha: "fixed-commit".into(), actor: "person".into(), source: kiln_ci::trigger::Source::Schedule, ..Default::default() };
            let payload = Payload::Workflow(Box::new(Workflow { repo: repo.clone(), event: event.clone(), path: path.into(), workflow: workflow.clone(), message: "message".into(), author: "person".into(), actions: Some(BTreeMap::new()), inputs: serde_json::Value::Null, error: None }));
            assert!(enqueue(&hub, occurrence(), payload.clone()).await?);
            assert!(!enqueue(&hub, occurrence(), payload).await?, "retry of an occurrence never creates another intent");
            // No receiver existed when enqueue published. The database alone reconstructs it.
            let recovered = start(&hub, &trigger).await?.ok_or_else(|| anyhow::anyhow!("accepted trigger disappeared"))?;
            let run = run_id(&trigger);
            let new = || super::super::super::runs::NewRun { id: run.clone(), workflow: path.into(), name: "nightly".into(), event: "schedule".into(), branch: "main".into(), sha: "fixed-commit".into(), model: serde_json::to_string(&workflow).unwrap_or_default(), ..Default::default() };
            let first = super::super::super::runs::create_trigger(&hub, &repo, new(), Some(trigger.clone())).await?;
            assert!(!materialized(&hub, &trigger, &recovered).await?, "an empty plan is still owned by the trigger");
            // Crash after row creation, before a complete plan and outbox retirement.
            let replayed = start(&hub, &trigger).await?.ok_or_else(|| anyhow::anyhow!("pending trigger disappeared"))?;
            assert_eq!(replayed.given.durable_trigger.as_deref(), Some(trigger.as_str()));
            assert_eq!(super::super::super::runs::create_trigger(&hub, &repo, new(), Some(trigger.clone())).await?, first);
            assert_eq!(hub.store.runs(Some(repo.id.clone()), 20, None).await?.len(), 1);
            let mut unrelated = new(); unrelated.sha = "another-commit".into();
            assert!(super::super::super::runs::create_trigger(&hub, &repo, unrelated, Some(trigger.clone())).await.is_err(), "an occurrence cannot adopt unrelated input");
            // Crash after the replay state commits, before clearing the intent. Its run still wins.
            let saved = run.clone();
            hub.store.call(move |c| {
                let tx = crate::store::tx(c)?;
                tx.execute("UPDATE ci_plans SET input = '{}' WHERE run_id = ?1", [&saved])?;
                tx.execute("UPDATE ci_trigger_outbox SET state = 'materialized' WHERE run_id = ?1", [&saved])?;
                tx.commit()?; Ok(())
            }).await?;
            assert!(materialized(&hub, &trigger, &replayed).await?);
            finish(&hub, &trigger, false, None).await?;
            assert!(start(&hub, &trigger).await?.is_none());
            assert_eq!(hub.store.runs(Some(repo.id), 20, None).await?.len(), 1);
            // U14: terminal state and accepted attachment control commit together, with no
            // UI publication. Duplicate completion is one source; a later attempt is new.
            super::super::super::runs::finish_run(&hub,&run,crate::store::Status::Failed,"failure",None).await?;
            super::super::super::runs::finish_run(&hub,&run,crate::store::Status::Failed,"failure",None).await?;
            assert_eq!(super::super::super::runs::start_attempt(&hub,&run,"person").await?,2);
            super::super::super::runs::finish_run(&hub,&run,crate::store::Status::Succeeded,"success",None).await?;
            let controls: Vec<(String,u32)> = hub.store.call(|c| Ok(c.prepare("SELECT json_extract(payload,'$.descriptor.event.type'),json_extract(payload,'$.descriptor.event.attempt') FROM ci_trigger_outbox WHERE kind = 'control' ORDER BY json_extract(payload,'$.descriptor.event.attempt')")?.query_map([],|r| Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?)).await?;
            assert_eq!(controls,vec![("run.failed".into(),1),("run.succeeded".into(),2)]);
            Ok(())
        })
    }

    #[test]
    fn u04_corrupt_payload_finishes_with_a_diagnostic_and_does_not_block_later_work() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            hub.store.call(|c| {
                c.execute("INSERT INTO ci_trigger_outbox (id, run_id, kind, payload, created_at, updated_at) VALUES ('corrupt', 'bad-run', 'workflow', '{bad', 0, 0)", [])?;
                Ok(())
            }).await?;
            assert!(enqueue(&hub, Occurrence { owner: "skip", path: "schedule", entry: 0, minute: 60, cron: "* * * * *" }, Payload::Skipped { reason: "unchanged".into() }).await?);
            hub.store.call(|c| {
                control(c,"bad-conversation",serde_json::json!({"consumer":"conversation","conversation":{}}))?;
                control(c,"bad-upgrade",serde_json::json!({"consumer":"upgrade","job":{}}))?;
                Ok(())
            }).await?;
            drain(&hub).await?;
            let states: Vec<(String, Option<String>)> = hub.store.call(|c| {
                Ok(c.prepare("SELECT state, diagnostic FROM ci_trigger_outbox ORDER BY created_at, id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?)
            }).await?;
            assert_eq!(states.len(), 4);
            for expected in ["invalid persisted trigger payload","invalid persisted conversation snapshot","invalid persisted upgrade snapshot"] {
                assert!(states.iter().any(|(state, why)| state == "failed" && why.as_deref().is_some_and(|s| s.contains(expected))),"{expected}: {states:?}");
            }
            assert!(states.iter().any(|(state, why)| state == "done" && why.as_deref() == Some("unchanged")));
            Ok(())
        })
    }
}
