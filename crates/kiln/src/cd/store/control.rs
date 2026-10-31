//! U15/B2/U19: accepted CD controls survive Quiver loss and feed compaction. The domain state,
//! immutable descriptor and bounded pending accounting share one Records transaction. The
//! existing CD worker transfers them to CI's control outbox before retiring the source row.

use amphora_api::{Scan, Row};
use kiln_cd::apps::AppDef;
use serde_json::{Value, json};

use super::*;

const FAMILY: &str = "cd-controls";
const META: &[u8] = b"meta";
const PREFIX: &[u8] = b"pending/";
const COUNT: usize = 128;
const BYTES: usize = 8 << 20;
const PAGE: usize = 16;

#[derive(Default, Serialize, Deserialize)]
struct Meta { sequence: u64, count: usize, bytes: usize }
#[derive(Serialize, Deserialize)]
struct Pending { key: String, descriptor: Value }
#[derive(Serialize)]
pub(super) struct AppState { id: String, before: Option<AppDef>, after: Option<AppDef> }

fn invalid(what: &str, error: impl std::fmt::Display) -> Refusal { Refusal::Invalid { what: what.into(), why: error.to_string() } }
fn encode(value: &impl Serialize) -> Result<Vec<u8>, Refusal> { serde_json::to_vec(value).map_err(|e| invalid("CD control", e)) }

pub(super) fn before(data: &Data, command: &Command, context: Option<&AppDef>) -> Result<Option<AppState>, Refusal> {
    let id = match command {
        Command::PutApp(app) => app.id(),
        Command::DeleteApp(id) | Command::Pin { app: id, .. } => id.clone(),
        Command::Deployed { deployment, .. } => deployment.app.clone(),
        Command::Rollout { id, record, .. } => match record.as_ref().map(|r| r.app.clone()).or_else(|| data.rollouts.get(id).map(|r| r.app.clone())) { Some(id) => id, None => return Ok(None) },
        _ => return Ok(None),
    };
    if context.is_some_and(|app| app.id() != id) { return Err(invalid("CD accepted app", "definition belongs to another app")); }
    Ok(Some(AppState { before: context.cloned().or_else(|| data.apps.get(&id).cloned()), id, after: None }))
}

fn meta(namespace: &Namespace) -> Result<(Option<Rev>, Meta), Refusal> {
    match namespace.get(FAMILY, META, Consistency::Linearizable, deadline())? {
        Some(row) => {
            let value: Meta = serde_json::from_slice(&row.value).map_err(|e| invalid("CD control accounting", e))?;
            if value.count > COUNT || value.bytes > BYTES || value.sequence < value.count as u64 { return Err(invalid("CD control accounting", "pending bounds or sequence disagree")); }
            Ok((Some(row.rev), value))
        }
        None => Ok((None, Meta::default())),
    }
}
fn condition(key: &[u8], rev: Option<Rev>) -> Cond {
    match rev {
        Some(rev) => Cond::At { family: FAMILY.into(), key: key.to_vec(), rev },
        None => Cond::Absent { family: FAMILY.into(), key: key.to_vec() },
    }
}

pub(super) fn admit(namespace: &Namespace, controller: &ControllerScope, previous: Rev, data: &Data, changes: &[Change], mut app: Option<AppState>) -> Result<Tx, Refusal> {
    let (rev, mut queue) = meta(namespace)?;
    if queue.count >= COUNT { return Err(Refusal::Full { limit: COUNT }); }
    queue.sequence = queue.sequence.checked_add(1).ok_or_else(|| invalid("CD control sequence", "exhausted"))?;
    if let Some(app) = &mut app
        && !changes.iter().any(|change| matches!(change, Change::AppDeleted(id) if id == &app.id))
    {
        app.after = if changes.iter().any(|change| matches!(change, Change::Deployed(_))) { app.before.clone() } else { data.apps.get(&app.id).cloned().or_else(|| app.before.clone()) };
    }
    let deployment = changes.iter().find_map(|change| match change { Change::Deployed(id) => data.history.get(id).and_then(|history| history.back()).cloned(), _ => None });
    let descriptor = json!({"consumer":"cd", "controller":controller, "sequence":queue.sequence, "previous_state_revision":previous, "changes":changes, "apps":app.into_iter().collect::<Vec<_>>(), "deployment":deployment});
    let pending = Pending { key: format!("cd/{}", queue.sequence), descriptor };
    let value = encode(&pending)?;
    if value.len() > amphora_api::VALUE_MAX { return Err(Refusal::TooLarge { what: "CD control descriptor".into(), size: value.len(), limit: amphora_api::VALUE_MAX }); }
    queue.bytes = queue.bytes.checked_add(value.len()).ok_or_else(|| invalid("CD pending controls", "byte accounting overflow"))?;
    if queue.bytes > BYTES { return Err(Refusal::TooLarge { what: "CD pending controls".into(), size: queue.bytes, limit: BYTES }); }
    queue.count += 1;
    let key = format!("pending/{:020}", queue.sequence).into_bytes();
    Ok(Tx { conds: vec![condition(META, rev), condition(&key, None)], writes: vec![
        Write::Put { family: FAMILY.into(), key: META.to_vec(), value: encode(&queue)? },
        Write::Put { family: FAMILY.into(), key, value },
    ] })
}

pub(super) fn page(namespace: &Namespace) -> anyhow::Result<Vec<Row>> {
    Ok(namespace.scan(FAMILY, Scan { prefix: PREFIX.to_vec(), after: None, limit: PAGE, at: Consistency::Linearizable }, deadline())?.rows)
}

async fn transfer(store: &crate::store::Store, row: &Row) -> anyhow::Result<()> {
    let pending: Pending = serde_json::from_slice(&row.value)?;
    store.call(move |c| {
        let tx = crate::store::tx(c)?;
        crate::ci::schedule::outbox::control(&tx, &pending.key, pending.descriptor)?;
        tx.commit()?;
        Ok(())
    }).await
}

fn acknowledge(namespace: &Namespace, row: &Row) -> anyhow::Result<()> {
    for _ in 0..RETRIES {
        let Some(current) = namespace.get(FAMILY, &row.key, Consistency::Linearizable, deadline())? else { return Ok(()); };
        anyhow::ensure!(current.rev == row.rev && current.value == row.value, "CD pending control was replaced before its durable acknowledgement");
        let (rev, mut queue) = meta(namespace)?;
        queue.count = queue.count.checked_sub(1).context("CD pending control count underflow")?;
        queue.bytes = queue.bytes.checked_sub(row.value.len()).context("CD pending control byte underflow")?;
        let tx = Tx { conds: vec![condition(&row.key, Some(row.rev)), condition(META, rev)], writes: vec![
            Write::Delete { family: FAMILY.into(), key: row.key.clone() },
            Write::Put { family: FAMILY.into(), key: META.to_vec(), value: encode(&queue)? },
        ] };
        match namespace.commit(tx, deadline()) { Ok(_) => return Ok(()), Err(Refusal::Conflict { .. }) => continue, Err(e) => return Err(e.into()) }
    }
    anyhow::bail!("CD control acknowledgement kept changing; its durable source remains pending")
}

pub(super) async fn drain(namespace: &Namespace, store: &crate::store::Store) -> anyhow::Result<bool> {
    let ns = namespace.clone();
    let rows = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || page(&ns)).await??;
    let more = rows.len() == PAGE;
    for row in rows {
        // SQL failure/unknown outcome retains the source. A crash after SQL commit retries the
        // same stable key, then retires the source only after SQL acknowledges its descriptor.
        transfer(store, &row).await?;
        let ns = namespace.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || acknowledge(&ns, &row)).await??;
    }
    Ok(more)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_cd::state::Deployment;

    #[test]
    fn u15_b2_cd_pending_controls_survive_compaction_and_adapter_restart_then_deduplicate_sql_transfer() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let path = hub.cfg.server.data_dir.join("control-source");
            let bus: Arc<dyn kiln_seams::Bus> = Arc::new(hub.bus.clone());
            let store = Store::open(hub.store.database(), &path, Arc::clone(&bus), hub.shutdown.clone()).await?;
            let controller = store.controller_scope().clone();
            let mut app = kiln_cd::apps::from_argo(&json!({"apiVersion":"argoproj.io/v1alpha1","kind":"Application","metadata":{"name":"runtime-app"},"spec":{"project":"default","source":{"repoURL":"https://example.invalid/repo","path":"deploy"},"destination":{"namespace":"staging"}}}), None, &|_| None).map_err(|e| anyhow::anyhow!(e.0))?;
            app.origin = kiln_cd::apps::Origin::Deploy { repo: "project/repository".into() };
            app.environment = Some("staging".into());
            let id = app.id();
            let command = |op, succeeded| Command::Deployed { deployment: Box::new(Deployment { app: id.clone(), op, succeeded, ..Default::default() }), limit: 1 };
            store.submit_with_app(command(1, false), Some(app.clone())).await?.map_err(anyhow::Error::msg)?;
            app.environment = Some("production".into());
            store.submit_with_app(command(2, true), Some(app)).await?.map_err(anyhow::Error::msg)?;
            let namespace = hub.store.database().namespace("cd-engine")?;
            let rows = page(&namespace)?;
            assert_eq!(rows.len(), 2);
            let failed: Pending = serde_json::from_slice(&rows.first().context("first control")?.value)?;
            assert_eq!(failed.descriptor.get("controller"), Some(&json!(controller)));
            assert_eq!(failed.descriptor.pointer("/deployment/succeeded"), Some(&json!(false)));
            assert_eq!(failed.descriptor.pointer("/deployment/id"), Some(&json!(1)));
            assert_eq!(failed.descriptor.pointer("/apps/0/after/environment"), Some(&json!("staging")));
            assert_eq!(store.read(|d| d.history.get(&id).map(|h| h.len())), Some(1), "history already dropped the failure");
            namespace.compact(namespace.head(deadline())?, deadline())?;
            drop(store);
            let reopened = Store::open(hub.store.database(), &path, bus, hub.shutdown.clone()).await?;
            assert_ne!(reopened.controller_scope(), &controller);
            // The adapter's ephemeral delivered cursor starts at current state, but pending
            // controls remain independently readable even after the version feed is compacted.
            let recovered = page(&namespace)?;
            assert_eq!(recovered.len(), 2);
            assert_eq!(recovered.first().map(|row| &row.value), rows.first().map(|row| &row.value), "reopen preserves the accepted scope and entire pending descriptor");
            transfer(&hub.store, recovered.first().context("recovered first control")?).await?;
            assert_eq!(page(&namespace)?.len(), 2, "SQL acceptance alone does not prematurely drop its source");
            reopened.drain_controls(&hub.store).await?;
            assert!(page(&namespace)?.is_empty());
            let count: i64 = hub.store.call(|c| Ok(c.query_row("SELECT count(*) FROM ci_trigger_outbox WHERE kind='control' AND json_extract(payload,'$.descriptor.consumer')='cd'", [], |r| r.get(0))?)).await?;
            assert_eq!(count, 2, "retry after SQL commit and before source acknowledgement is deduplicated");
            let (_, queue) = meta(&namespace)?;
            assert_eq!((queue.count, queue.bytes), (0, 0));
            hub.shutdown.cancel();
            Ok(())
        })
    }

    #[test]
    fn u15_b2_cd_pending_capacity_refuses_before_state_ack_and_frees_only_after_durable_transfer() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let store = Store::open(hub.store.database(), &hub.cfg.server.data_dir.join("control-capacity"), Arc::new(hub.bus.clone()), hub.shutdown.clone()).await?;
            for n in 0..COUNT { store.submit(Command::PutSetting("sequence".into(), json!(n))).await?.map_err(anyhow::Error::msg)?; }
            assert!(store.submit(Command::PutSetting("refused".into(), json!(true))).await?.is_err());
            assert!(!store.read(|d| d.settings.contains_key("refused")), "a full source queue cannot acknowledge the state mutation");
            store.drain_controls(&hub.store).await?;
            store.submit(Command::PutSetting("after-transfer".into(), json!(true))).await?.map_err(anyhow::Error::msg)?;
            let namespace = hub.store.database().namespace("cd-engine")?;
            let mut accepted = 0;
            for n in 0..PAGE {
                let command = Command::Deployed { deployment: Box::new(Deployment { app:"bounded".into(), op:n as u64, message:"m".repeat(600 << 10), ..Default::default() }), limit: 1 };
                match store.submit(command).await? {
                    Ok(()) => accepted += 1,
                    Err(error) => { assert!(error.contains("CD pending controls"), "{error}"); break; }
                }
            }
            assert!(accepted > 0 && accepted < PAGE, "aggregate byte bound stops acceptance before the count bound");
            let history = store.read(|d| d.history.get("bounded").and_then(|h| h.back()).cloned());
            assert_eq!(history.map(|d| d.id), Some(accepted as u64));
            let (_, queue) = meta(&namespace)?;
            assert!(queue.count < COUNT && queue.bytes <= BYTES);
            assert!(store.submit(Command::Deployed { deployment: Box::new(Deployment { app:"oversized".into(), message:"m".repeat(amphora_api::VALUE_MAX), ..Default::default() }), limit: 1 }).await?.is_err());
            assert!(!store.read(|d| d.history.contains_key("oversized")), "an oversized descriptor never leaves accepted domain state without its intent");
            let (_, before) = meta(&namespace)?;
            hub.store.call(|c| { c.execute("DROP TABLE ci_trigger_outbox", [])?; Ok(()) }).await?;
            assert!(store.drain_controls(&hub.store).await.is_err());
            let (_, after) = meta(&namespace)?;
            assert_eq!((after.count, after.bytes), (before.count, before.bytes), "a refused SQL handoff never acknowledges the pending Records source");
            hub.shutdown.cancel();
            Ok(())
        })
    }
}
