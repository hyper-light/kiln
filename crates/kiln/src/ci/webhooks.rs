//! Outgoing webhooks (docs/ci.md W19-R07): kiln's events, POSTed to subscribers as they happen.
//!
//! A subscription is a `webhook` integration (W11-R06) whose config lists `events`: the types it
//! wants ([`EVENTS`], or `*`), from what its scope holds (the server, an organization's
//! repositories, one repository's). Each event goes as the JSON kiln's own clients get, with
//! `X-Kiln-Event`, `X-Kiln-Delivery` and `X-Kiln-Signature-256` (HMAC-SHA256 of the body with
//! the integration's secret), in order for each subscriber and retried with backoff. Every
//! delivery is logged with its body before it is sent, so a restart resumes what was waiting and
//! any of them can be sent again (`POST /api/ci/webhooks/deliveries/:id/redeliver`).

use std::collections::{HashMap, HashSet};

use futures::{FutureExt as _, StreamExt as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::StatusCode;
use crate::sql::{OptionalExtension, params};
use serde_json::{Value, json};

use super::Fail;
use super::integrations::{self, Integration};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// The events a subscriber may have, and what each is. The rest of what the UI hears (sign-in
/// flows, terminals, host samples) never leaves.
pub const EVENTS: &[(&str, &str)] = &[
    ("run", "a run was created, changed status or finished"),
    ("job", "a job was queued, started, moved through a step or finished (its steps included)"),
    ("agent", "a runner connected, changed, started draining, finished a job or went away"),
    ("queue", "how many jobs are queued and running, on every change"),
    ("cd_plan", "a deploy plan was made for a push"),
    ("cd_apps", "deployed apps changed state"),
    ("cd_app_deleted", "an app was deleted"),
    // W19-R06's events, as notification rules see them.
    ("run.failed", "a run failed"),
    ("run.fixed", "a run succeeded after failures on its branch and workflow"),
    ("run.succeeded", "a run succeeded"),
    ("run.cancelled", "a run was cancelled"),
    ("approval.waiting", "a run waits for a reviewer to deploy to an environment"),
    ("deploy.succeeded", "a job deployed to an environment"),
    ("deploy.failed", "a job's deployment to an environment failed"),
    ("leg.flaky", "a run had flaky tests: a retry passed them, or an attempt failed them and another passed them"),
    ("land.ejected", "diffs were taken out of the land queue, and why"),
    ("runner.offline", "a runner went away and hasn't come back for two minutes"),
    ("review.requested", "a diff or change request asks someone for a review"),
    ("comment.reply", "a comment published in a thread, or on someone's diff"),
    ("mention", "a comment or review names someone (@ada)"),
    ("quota.near", "a repository's cache is past 90 % of its quota"),
    ("secret.unused", "a secret no run has read in 90 days"),
    ("machine.battery_low", "a machine on battery is low (20% or less): it sleeps soon, and its runner with it"),
    // An agent's notes (docs/agentic.md N2, `crate::push`), as people's devices get them.
    ("agent.needs_you", "an agent waits on a person: an approval, a question or a plan"),
    ("agent.finished", "an agent's turn ended (not stopped by a person)"),
    ("agent.stopped", "an agent crashed or couldn't start"),
    ("agent.paused", "an agent reached a usage limit and waits for it to reset"),
];

/// Sent only when named: they come too often for `*`.
const BUSY: &[&str] = &["queue"];

/// How long a list of subscriptions is used before it is read again.
const FRESH: Duration = Duration::from_secs(5);

/// W19-R07: bound recovery payloads and concurrent subscribers; pending work stays in Amphora.
const PAGE: i64 = 64;
const WORKERS: usize = 32;
const RETRY: Duration = Duration::from_secs(1);

/// Seconds to wait before each attempt.
const ATTEMPTS: &[u64] = &[0, 2, 15, 60];

/// How long deliveries are logged.
const KEEP: i64 = 14 * 86_400;

/// The types a subscription names.
fn wanted(config: &Value) -> Vec<String> {
    match config.get("events") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|x| x.trim().to_owned()).filter(|x| !x.is_empty()).collect(),
        Some(Value::String(s)) => s.split(',').map(|x| x.trim().to_owned()).filter(|x| !x.is_empty()).collect(),
        _ => Vec::new(),
    }
}

/// Integration config validation: every event named is one of [`EVENTS`], or `*`.
pub fn check_events(config: &Value) -> Result<(), String> {
    match config.get("events") {
        None | Some(Value::Null | Value::String(_) | Value::Array(_)) => {}
        Some(_) => return Err("`events` is a list of event types, or one string of them, comma-separated".into()),
    }
    match wanted(config).into_iter().find(|w| w != "*" && !EVENTS.iter().any(|(k, _)| k == w)) {
        Some(w) => Err(format!("no event `{w}` (events: {}, or *)", EVENTS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", "))),
        None => Ok(()),
    }
}

fn takes(want: &[String], kind: &str) -> bool {
    EVENTS.iter().any(|(k, _)| *k == kind) && want.iter().any(|w| w == kind || (w == "*" && !BUSY.contains(&kind)))
}

/// Whether an event about repository `repo` (`(id, owner/name)`; `None`: about no repository)
/// reaches subscription `i` by its scope.
fn in_scope(i: &Integration, repo: Option<&(String, String)>) -> bool {
    match (i.scope.split_once(':'), repo) {
        (None, _) => i.scope == "server" || i.scope.is_empty(),
        (Some(("org", o)), Some((_, name))) => name.split('/').next().is_some_and(|x| x.eq_ignore_ascii_case(o)),
        (Some(("repo", r)), Some((id, name))) => r == id || r.eq_ignore_ascii_case(name),
        _ => false,
    }
}

/// One event for one subscriber: its logged delivery.
struct Out {
    delivery: String,
    i: Integration,
    kind: String,
    body: Arc<str>,
}

/// U14: only types whose sole live publishers first stage an accepted control descriptor.
/// runs::finish_run, runs::finish_leg_with_deployment, land::eject and notify::runner_offline
/// own those boundaries. Deployment sources require the actual LegDeployment and exact persisted
/// run/leg/attempt/environment correlation. The attachment outbox consumer atomically stages
/// deliveries with children and retirement. run.fixed/cancelled, approvals, conversation-derived
/// notices and UI snapshots remain on the live path.
pub(crate) fn durable_source_kind(kind: &str) -> bool {
    matches!(kind, "run.failed" | "run.succeeded" | "leg.flaky" | "land.ejected" | "runner.offline" | "deploy.succeeded" | "deploy.failed")
}

/// U14/W19-R07: exact recipient/event intent, prepared before the caller's materialization
/// transaction. The outbox owner stages it with children and parent retirement atomically.
#[derive(Clone, Debug)]
pub(crate) struct DeliveryIntent {
    integration: String,
    integration_id: String,
    kind: String,
    delivery: String,
    body: Arc<str>,
}

/// Recipients are frozen at the first successful durable materialization, not at later HTTP
/// delivery. The immutable source id plus integration id identifies exactly one delivery.
pub(crate) async fn prepare_source(hub: &Hub, source: &str, event: &Value) -> anyhow::Result<Vec<DeliveryIntent>> {
    anyhow::ensure!(!source.is_empty(), "webhook source identity is empty");
    let Some(kind) = event.get("type").and_then(Value::as_str).filter(|kind| EVENTS.iter().any(|(name, _)| name == kind)) else { return Ok(Vec::new()); };
    let body: Arc<str> = Arc::from(serde_json::to_string(event)?);
    anyhow::ensure!(body.len() <= kiln_seams::bus::PAYLOAD_MAX, "webhook source event exceeds the event payload limit");
    let repo = repo_of(hub, event, &mut HashMap::new()).await?;
    let mut intents = Vec::new();
    for i in integrations::list(hub).await?.into_iter().filter(|i| i.kind == "webhook" && takes(&wanted(&i.config), kind) && in_scope(i, repo.as_ref())) {
        let delivery = crate::util::sha256_hex(&serde_json::to_vec(&("kiln/webhook/source/v1", source, &i.id))?);
        anyhow::ensure!(intents.len() < 4096, "webhook source fanout exceeds 4096 recipients");
        intents.push(DeliveryIntent { integration: i.name, integration_id: i.id, kind: kind.to_owned(), delivery, body: body.clone() });
    }
    Ok(intents)
}

/// U14: caller owns the transaction; a failed insert must roll back children and retirement.
pub(crate) fn stage(c: &crate::sql::Connection<'_>, intents: &[DeliveryIntent]) -> anyhow::Result<()> {
    for intent in intents { insert_intent(c, intent)?; }
    Ok(())
}

#[derive(Debug)]
struct Pending {
    row: i64,
    integration: String,
    integration_id: Option<String>,
    kind: String,
    delivery: String,
    body: String,
}

impl Pending {
    fn subscriber(&self) -> String {
        match &self.integration_id {
            Some(id) => format!("id/{id}"),
            None => format!("legacy/{}", self.integration),
        }
    }
}

/// W19-R07: Quiver carries events; Amphora retains delivery intent after receipt. This cannot
/// reconstruct ephemeral events lost before receipt (topic lag or a process restart). Accepted
/// kinds selected by durable_source_kind instead stage delivery intents during outbox fanout.
pub fn spawn(hub: Arc<Hub>) {
    // The existing delivery workers recover independently even when ingress is waiting on SQL
    // or Quiver. No second event queue: only bounded pending delivery rows are read here.
    crate::rt::spawn(deliver_pending(Arc::clone(&hub)));
    crate::rt::spawn(async move {
        let mut rx = hub.events.subscribe();
        let mut runs: HashMap<String, (String, String)> = HashMap::new();
        let (mut subs, mut read_at): (Vec<Integration>, Option<Instant>) = (Vec::new(), None);
        loop {
            let received = futures::select! {
                _ = hub.shutdown.cancelled().fuse() => return,
                received = rx.recv().fuse() => received,
            };
            let text = match received {
                Ok(t) => t,
                Err(crate::bus::RecvError::Overflowed(n)) => {
                    tracing::warn!(missed = n, "outgoing webhook source gap before durable receipt; pending delivery recovery cannot repair it");
                    continue;
                }
                Err(crate::bus::RecvError::Unavailable(error)) => {
                    tracing::warn!(%error, "outgoing webhook event bus unavailable; retrying the same receiver offset");
                    if !hub.shutdown.sleep(RETRY).await { return; }
                    continue;
                }
                Err(crate::bus::RecvError::Closed) => return,
            };
            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
            let Some(kind) = event.get("type").and_then(Value::as_str).filter(|k| EVENTS.iter().any(|(e, _)| e == k)) else { continue };
            // U14: accepted controls already own exact durable fanout; their UI publications
            // must not create a second random-identity delivery of the same transition.
            if durable_source_kind(kind) { continue; }
            if read_at.is_none_or(|t| t.elapsed() > FRESH) {
                loop {
                    match integrations::list(&hub).await {
                        Ok(all) => {
                            subs = all.into_iter().filter(|i| i.kind == "webhook" && !wanted(&i.config).is_empty()).collect();
                            read_at = Some(Instant::now());
                            break;
                        }
                        Err(error) => tracing::warn!(error = %format!("{error:#}"), "webhook subscriptions unreadable; retaining received event"),
                    }
                    if !hub.shutdown.sleep(RETRY).await { return; }
                }
            }
            let repo = loop {
                match repo_of(&hub, &event, &mut runs).await {
                    Ok(repo) => break repo,
                    Err(error) => tracing::warn!(error = %format!("{error:#}"), "webhook scope unreadable; retaining received event"),
                }
                if !hub.shutdown.sleep(RETRY).await { return; }
            };
            // Freeze recipients and payload before retrying. Do not consume the next Quiver
            // event until every selected subscriber's pending row has committed.
            for i in subs.iter().filter(|i| takes(&wanted(&i.config), kind) && in_scope(i, repo.as_ref())) {
                let delivery = crate::util::new_id();
                loop {
                    match begin(&hub, &i.name, &i.id, kind, &delivery, &text).await {
                        Ok(_) => break,
                        Err(error) => tracing::warn!(integration = %i.name, %delivery, error = %format!("{error:#}"), "webhook intent not committed; retaining subscriber and event for retry"),
                    }
                    if !hub.shutdown.sleep(RETRY).await { return; }
                }
            }
        }
    });
}

/// A bounded page contains only each subscriber's oldest pending row. Active rows remain
/// pending until their outcome commits, so a later row cannot overtake them.
async fn pending(hub: &Hub) -> anyhow::Result<Vec<Pending>> {
    hub.store.call(|c| {
        let mut st = c.prepare("SELECT d.id, d.integration, d.integration_id, d.event, d.delivery, d.body FROM webhook_deliveries d WHERE d.status = 'pending' AND NOT EXISTS (SELECT 1 FROM webhook_deliveries earlier WHERE earlier.status = 'pending' AND earlier.id < d.id AND ((d.integration_id IS NOT NULL AND earlier.integration_id = d.integration_id) OR (earlier.integration = d.integration AND (d.integration_id IS NULL OR earlier.integration_id IS NULL)))) ORDER BY d.id LIMIT ?1")?;
        Ok(st.query_map([PAGE], |x| Ok(Pending { row: x.get(0)?, integration: x.get(1)?, integration_id: x.get(2)?, kind: x.get(3)?, delivery: x.get(4)?, body: x.get(5)? }))?.collect::<Result<Vec<_>, _>>()?)
    }).await
}

async fn deliver_pending(hub: Arc<Hub>) {
    let mut active = HashSet::new();
    let mut workers = futures::stream::FuturesUnordered::new();
    let mut pruned = Instant::now();
    loop {
        match pending(&hub).await {
            Ok(rows) => for out in rows {
                if active.len() >= WORKERS { break; }
                if active.insert(out.subscriber()) {
                    workers.push(deliver_row(Arc::clone(&hub), out));
                }
            },
            Err(error) => tracing::warn!(error = %format!("{error:#}"), "pending webhook scan failed; retrying"),
        }
        if pruned.elapsed() > Duration::from_secs(3600) {
            let before = crate::util::now_unix() - KEEP;
            match hub.store.call(move |c| Ok(c.execute("DELETE FROM webhook_deliveries WHERE ts < ?1 AND status != 'pending'", [before])?)).await {
                Ok(_) => pruned = Instant::now(),
                Err(error) => tracing::warn!(error = %format!("{error:#}"), "webhook history pruning failed"),
            }
        }
        if workers.is_empty() {
            if !hub.shutdown.sleep(RETRY).await { return; }
        } else {
            futures::select! {
                _ = hub.shutdown.cancelled().fuse() => return,
                name = workers.next().fuse() => { if let Some(name) = name { active.remove(&name); } },
                _ = crate::rt::sleep(RETRY).fuse() => {},
            }
        }
    }
}

async fn deliver_row(hub: Arc<Hub>, pending: Pending) -> String {
    let subscriber = pending.subscriber();
    let Pending { row, integration: name, integration_id, kind, delivery, body } = pending;
    if integration_id.is_none() {
        tracing::warn!(integration = %name, row, "replaying legacy name-only webhook delivery; immutable recipient identity was not recorded");
    }
    let outcome = loop {
        match integrations::get(&hub, &name).await {
            Ok(Some(i)) if recipient_matches(integration_id.as_deref(), &i) => {
                let out = Out { delivery, i, kind, body: Arc::from(body) };
                break send(&hub, &out, ATTEMPTS).await;
            }
            Ok(_) => break ("failed", None, 0, Some("the original webhook integration is gone or its name belongs to another integration".into())),
            Err(error) => tracing::warn!(integration = %name, error = %format!("{error:#}"), "webhook integration unreadable; retaining pending delivery"),
        }
        if !hub.shutdown.sleep(RETRY).await { return subscriber; }
    };
    // Retry just the outcome write, never repeat HTTP because the outcome write failed.
    // A crash after HTTP and before commit can still redeliver the same delivery id.
    loop {
        if let Err(error) = finish(&hub, row, outcome.0, outcome.1, outcome.2, outcome.3.clone()).await {
            tracing::warn!(integration = %name, row, error = %format!("{error:#}"), "webhook outcome not committed; subscriber remains blocked for retry");
        } else { return subscriber; }
        if !hub.shutdown.sleep(RETRY).await { return subscriber; }
    }
}

/// U14: name-only matching is limited to explicit pre-migration compatibility rows.
fn recipient_matches(id: Option<&str>, current: &Integration) -> bool {
    current.kind == "webhook" && id.is_none_or(|id| id == current.id)
}

/// The repository an event is about: its own, a run's, or a job's run's.
async fn repo_of(hub: &Hub, event: &Value, runs: &mut HashMap<String, (String, String)>) -> anyhow::Result<Option<(String, String)>> {
    if let (Some(id), Some(name)) = (event.pointer("/repo/id").and_then(Value::as_str), event.pointer("/repo/name").and_then(Value::as_str)) {
        return Ok(Some((id.to_owned(), name.to_owned())));
    }
    if let Some(r) = event.get("run") {
        let (Some(id), Some(name)) = (r.get("repo_id").and_then(Value::as_str), r.get("repo_name").and_then(Value::as_str)) else { return Ok(None) };
        if let Some(run) = r.get("id").and_then(Value::as_str) {
            runs.insert(run.to_owned(), (id.to_owned(), name.to_owned()));
        }
        return Ok(Some((id.to_owned(), name.to_owned())));
    }
    let Some(run) = event.pointer("/job/run_id").and_then(Value::as_str).map(str::to_owned) else { return Ok(None) };
    if let Some(r) = runs.get(&run) {
        return Ok(Some(r.clone()));
    }
    let Some(found) = hub.store.run(run.clone()).await?.map(|r| (r.repo_id, r.repo_name)) else { return Ok(None) };
    if runs.len() > 10_000 {
        runs.clear();
    }
    runs.insert(run, found.clone());
    Ok(Some(found))
}

/// POSTs one event, waiting `waits` seconds before each attempt until one is answered 2xx:
/// how it went, in how many attempts.
async fn send(hub: &Hub, out: &Out, waits: &[u64]) -> (&'static str, Option<u16>, i64, Option<String>) {
    let Some(url) = out.i.config.get("url").and_then(Value::as_str).filter(|u| !u.trim().is_empty()) else { return ("failed", None, 0, Some("the integration has no `url`".into())) };
    let key = match integrations::token(hub, &out.i).await {
        Ok(k) => k,
        Err(e) => return ("failed", None, 0, Some(format!("its signing key: {e:#}"))),
    };
    let client = match crate::http::client::Client::public() {
        Ok(c) => c,
        Err(e) => return ("failed", None, 0, Some(format!("{e:#}"))),
    };
    let body = out.body.as_bytes().to_vec();
    let sig = key.map(|k| format!("sha256={}", crate::util::hmac_hex(k.as_bytes(), &body)));
    let (mut code, mut error, mut attempts) = (None, None, 0);
    for wait in waits {
        if *wait > 0 {
            crate::rt::sleep(Duration::from_secs(*wait)).await;
        }
        attempts += 1;
        // A URL someone gave kiln: never this machine or a private network unless allowed (W22-AC07).
        let mut req = client.post(url).reaching(&hub.cfg.ci.reach()).header("content-type", "application/json").header("user-agent", "kiln-webhooks").header("x-kiln-event", &out.kind).header("x-kiln-delivery", &out.delivery).body(body.clone()).timeout(Duration::from_secs(20));
        if let Some(s) = &sig {
            req = req.header("x-kiln-signature-256", s);
        }
        match req.send().await {
            Ok(r) if r.is_success() => return ("sent", Some(r.status()), attempts, None),
            Ok(r) => (code, error) = (Some(r.status()), Some(format!("HTTP {}", r.status()))),
            Err(e) => (code, error) = (None, Some(format!("{e:#}"))),
        }
    }
    tracing::warn!(integration = %out.i.name, event = %out.kind, error = ?error, "a webhook wasn't delivered");
    ("failed", code, attempts, error)
}

/// Logs one received event for the same immutable subscriber across SQL retries.
async fn begin(hub: &Hub, name: &str, integration_id: &str, kind: &str, delivery: &str, body: &str) -> anyhow::Result<i64> {
    let intent = DeliveryIntent { integration: name.to_owned(), integration_id: integration_id.to_owned(), kind: kind.to_owned(), delivery: delivery.to_owned(), body: Arc::from(body) };
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let row = insert_intent(&tx, &intent)?;
        tx.commit()?;
        Ok(row)
    }).await
}

fn insert_intent(c: &crate::sql::Connection<'_>, intent: &DeliveryIntent) -> anyhow::Result<i64> {
    anyhow::ensure!(!intent.integration_id.is_empty(), "webhook recipient identity is empty");
    let saved: Option<(i64, String, Option<String>, String, String)> = c.query_row("SELECT id, integration, integration_id, event, body FROM webhook_deliveries WHERE delivery = ?1", [&intent.delivery], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
    if let Some((id, subscriber, integration_id, event, payload)) = saved {
        anyhow::ensure!(subscriber == intent.integration && integration_id.as_ref() == Some(&intent.integration_id) && event == intent.kind && payload == intent.body.as_ref(), "webhook delivery identity belongs to another immutable intent");
        return Ok(id);
    }
    c.execute("INSERT INTO webhook_deliveries (ts, integration, integration_id, event, delivery, status, attempts, body) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6)", params![crate::util::now_unix(), intent.integration, intent.integration_id, intent.kind, intent.delivery, intent.body.as_ref()])?;
    Ok(c.last_insert_rowid())
}

/// Logs how a delivery went (attempts add to what it had).
async fn finish(hub: &Hub, row: i64, status: &'static str, code: Option<u16>, attempts: i64, error: Option<String>) -> anyhow::Result<()> {
    let now = crate::util::now_unix();
    hub.store.call(move |c| Ok(c.execute("UPDATE webhook_deliveries SET ts = ?2, status = ?3, code = ?4, attempts = attempts + ?5, error = ?6 WHERE id = ?1 AND status = 'pending'", params![row, now, status, code, attempts, error]).map(|_| ())?)).await
}

/// `GET /api/ci/webhooks/deliveries?integration=&status=&limit=`: newest first, without bodies;
/// `GET …/deliveries/:id`, with its body; `POST …/deliveries/:id/redeliver`: requeues it
/// with the same delivery id, preserving the subscriber's delivery order.
pub async fn route(hub: &Hub, method: &http::Method, rest: &[&str], q: &HashMap<String, String>) -> Result<Response, Fail> {
    let fail = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let row = |x: &crate::sql::Row<'_>| -> crate::sql::Result<Value> {
        Ok(json!({ "id": x.get::<_, i64>(0)?, "ts": x.get::<_, i64>(1)?, "integration": x.get::<_, String>(2)?, "event": x.get::<_, String>(3)?, "delivery": x.get::<_, String>(4)?, "status": x.get::<_, String>(5)?, "code": x.get::<_, Option<i64>>(6)?, "attempts": x.get::<_, i64>(7)?, "error": x.get::<_, Option<String>>(8)?, "integration_id": x.get::<_, Option<String>>(9)? }))
    };
    const COLUMNS: &str = "id, ts, integration, event, delivery, status, code, attempts, error, integration_id";
    match (method, rest) {
        (&http::Method::GET, ["deliveries"]) => {
            let (name, status) = (q.get("integration").cloned(), q.get("status").cloned());
            let limit: i64 = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100).clamp(1, 1000);
            let rows: Vec<Value> = hub
                .store
                .call(move |c| {
                    let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM webhook_deliveries WHERE (?1 IS NULL OR integration = ?1) AND (?2 IS NULL OR status = ?2) ORDER BY id DESC LIMIT ?3"))?;
                    let rows = st.query_map(params![name, status, limit], row)?;
                    Ok(rows.collect::<Result<Vec<_>, _>>()?)
                })
                .await
                .map_err(fail)?;
            Ok(srv::json(StatusCode::OK, &json!({ "deliveries": rows })))
        }
        (&http::Method::GET, ["deliveries", id]) => {
            let id: i64 = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "no such delivery".to_owned()))?;
            let found: Option<(Value, String)> = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {COLUMNS}, body FROM webhook_deliveries WHERE id = ?1"), [id], |x| Ok((row(x)?, x.get(10)?))).optional()?)).await.map_err(fail)?;
            let (mut d, body) = found.ok_or_else(|| (StatusCode::NOT_FOUND, "no such delivery".to_owned()))?;
            if let Some(o) = d.as_object_mut() {
                o.insert("body".into(), serde_json::from_str(&body).unwrap_or(Value::String(body)));
            }
            Ok(srv::json(StatusCode::OK, &d))
        }
        (&http::Method::POST, ["deliveries", id, "redeliver"]) => {
            let id: i64 = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "no such delivery".to_owned()))?;
            let found: Option<(String, Option<String>, String)> = hub.store.call(move |c| Ok(c.query_row("SELECT integration, integration_id, delivery FROM webhook_deliveries WHERE id = ?1", [id], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?))).optional()?)).await.map_err(fail)?;
            let (name, integration_id, delivery) = found.ok_or_else(|| (StatusCode::NOT_FOUND, "no such delivery".to_owned()))?;
            let current = integrations::get(hub, &name).await.map_err(fail)?.filter(|i| recipient_matches(integration_id.as_deref(), i));
            if current.is_none() { return Err((StatusCode::GONE, format!("the original integration {name} is gone"))); }
            // W19-R07: redelivery joins the same per-subscriber ordered pending path. It must
            // not race a worker already sending this row or overtake earlier pending rows.
            let queued = hub.store.call(move |c| Ok(c.execute("UPDATE webhook_deliveries SET status = 'pending', error = NULL WHERE id = ?1 AND status != 'pending'", [id])? == 1)).await.map_err(fail)?;
            Ok(srv::json(StatusCode::ACCEPTED, &json!({ "id": id, "delivery": delivery, "status": "pending", "queued": queued })))
        }
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W19-R07: interrupted delivery recovery is bounded, keeps one oldest row per subscriber,
    /// and only advances after the outcome commits. No HTTP or background worker is started.
    #[test]
    fn w19_r07_pending_recovery_is_bounded_and_ordered() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let first = begin(&hub, "audit", "audit-id", "run.failed", "first", "{\"n\":1}").await?;
            assert_eq!(first, begin(&hub, "audit", "audit-id", "run.failed", "first", "{\"n\":1}").await?);
            assert!(begin(&hub, "audit", "audit-id", "run.failed", "first", "{\"n\":2}").await.is_err());
            let second = begin(&hub, "audit", "audit-id", "run.failed", "second", "{\"n\":2}").await?;
            let rows = pending(&hub).await?;
            assert_eq!(rows.len(), 1);
            assert!(rows.iter().any(|r| r.row == first && r.delivery == "first" && r.body == "{\"n\":1}"));
            // A failed outcome write must not release the subscriber's next delivery.
            hub.store.call(|c| { c.execute_batch("ALTER TABLE webhook_deliveries RENAME TO held_webhook_deliveries")?; Ok(()) }).await?;
            assert!(finish(&hub, first, "sent", Some(200), 1, None).await.is_err());
            assert!(begin(&hub, "audit", "audit-id", "run.failed", "third", "{}").await.is_err());
            assert!(pending(&hub).await.is_err());
            hub.store.call(|c| { c.execute_batch("ALTER TABLE held_webhook_deliveries RENAME TO webhook_deliveries")?; Ok(()) }).await?;
            assert!(pending(&hub).await?.iter().any(|r| r.row == first));
            finish(&hub, first, "sent", Some(200), 1, None).await?;
            finish(&hub, first, "sent", Some(200), 1, None).await?;
            assert!(pending(&hub).await?.iter().any(|r| r.row == second));
            let attempts: i64 = hub.store.call(move |c| Ok(c.query_row("SELECT attempts FROM webhook_deliveries WHERE id = ?1", [first], |r| r.get(0))?)).await?;
            assert_eq!(attempts, 1, "an outcome commit retry does not double count attempts");
            for n in 0..PAGE + 3 {
                begin(&hub, &format!("subscriber-{n}"), &format!("id-{n}"), "run.failed", &format!("delivery-{n}"), "{}").await?;
            }
            assert_eq!(pending(&hub).await?.len(), usize::try_from(PAGE)?);
            Ok(())
        })
    }

    /// U14/W19-R07: frozen source fanout participates in the caller's transaction, retains
    /// exact context, deduplicates replay, and never follows a reused integration name.
    #[test]
    fn u14_w19_r07_source_staging_is_atomic_and_recipient_bound() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let config = json!({"url":"https://example.invalid/webhook","events":["run.failed"]});
            let original = integrations::create(&hub, "webhook", "audit", config.clone(), integrations::Secrets::default(), Some("server"), "admin").await.map_err(anyhow::Error::msg)?;
            let event = json!({"type":"run.failed","attempt":7,"repo":{"id":"repo","name":"owner/repo"},"run":{"id":"run","message":"exact accepted context"},"extra":{"keep":true}});
            let intents = prepare_source(&hub, "accepted/source", &event).await?;
            assert_eq!(intents.len(), 1);
            assert!(intents.iter().all(|intent| intent.integration_id == original.id && serde_json::from_str::<Value>(&intent.body).ok().as_ref() == Some(&event)));
            let rollback = intents.clone();
            hub.store.call(move |c| {
                let tx = crate::store::tx(c)?;
                stage(&tx, &rollback)?;
                // Simulate failure of another child before parent retirement: no commit.
                Ok(())
            }).await?;
            assert!(pending(&hub).await?.is_empty());
            let saved = intents.clone();
            hub.store.call(move |c| {
                let tx = crate::store::tx(c)?;
                stage(&tx, &saved)?;
                stage(&tx, &saved)?;
                tx.commit()?;
                Ok(())
            }).await?;
            assert_eq!(pending(&hub).await?.len(), 1);
            assert!(integrations::delete(&hub, "audit", "admin").await?);
            let replacement = integrations::create(&hub, "webhook", "audit", config, integrations::Secrets::default(), Some("server"), "admin").await.map_err(anyhow::Error::msg)?;
            assert_ne!(original.id, replacement.id);
            assert!(!recipient_matches(Some(&original.id), &replacement));
            assert!(recipient_matches(None, &replacement), "NULL-id rows retain explicit legacy name-only compatibility");
            let replay = prepare_source(&hub, "accepted/source", &event).await?;
            assert!(intents.iter().zip(replay.iter()).all(|(old, new)| old.delivery != new.delivery));
            Ok(())
        })
    }

    fn sub(scope: &str, events: Value) -> Integration {
        Integration { id: "i1".into(), kind: "webhook".into(), name: "audit".into(), config: json!({ "url": "http://x", "events": events }), has_secret: true, has_signing_secret: false, scope: scope.into(), created_by: "admin".into(), created_at: 0, updated_at: 0, verified_at: None, last_error: None, last_used_at: None }
    }

    #[test]
    fn w19_r07_subscriptions() {
        let all = sub("server", json!(["*"]));
        assert!(takes(&wanted(&all.config), "run") && takes(&wanted(&all.config), "agent"));
        assert!(!takes(&wanted(&all.config), "queue"), "`*` leaves out the busy ones");
        assert!(!takes(&wanted(&all.config), "account_flow") && !takes(&wanted(&json!({ "events": "account_flow" })), "account_flow"), "only the public events, even when named");
        let named = sub("server", json!("run, queue"));
        assert!(takes(&wanted(&named.config), "queue") && !takes(&wanted(&named.config), "job"));
        let repo = (String::from("r1"), String::from("acme/app"));
        assert!(in_scope(&sub("org:acme", json!(["*"])), Some(&repo)) && !in_scope(&sub("org:other", json!(["*"])), Some(&repo)));
        assert!(in_scope(&sub("repo:r1", json!(["*"])), Some(&repo)) && in_scope(&sub("repo:acme/app", json!(["*"])), Some(&repo)) && !in_scope(&sub("repo:r2", json!(["*"])), Some(&repo)));
        assert!(in_scope(&all, None) && !in_scope(&sub("org:acme", json!(["*"])), None), "events about no repository go to server-wide subscriptions only");
        assert!(check_events(&json!({ "events": "run, job" })).is_ok() && check_events(&json!({ "events": ["*"] })).is_ok() && check_events(&json!({})).is_ok());
        assert!(check_events(&json!({ "events": "run, sample" })).is_err() && check_events(&json!({ "events": 3 })).is_err());
        // An agent's notes (N2): named, or with `*`; about no repository, so server-wide only.
        assert!(["run.failed", "run.succeeded", "leg.flaky", "land.ejected", "runner.offline", "deploy.succeeded", "deploy.failed"].into_iter().all(durable_source_kind));
        assert!(!durable_source_kind("run.fixed") && !durable_source_kind("run.cancelled") && !durable_source_kind("queue") && !durable_source_kind("mention"));
        assert!(takes(&wanted(&json!({ "events": "agent.needs_you" })), "agent.needs_you") && takes(&wanted(&all.config), "agent.finished"));
        assert!(check_events(&json!({ "events": ["agent.needs_you", "agent.finished", "agent.stopped", "agent.paused"] })).is_ok());
    }
}
