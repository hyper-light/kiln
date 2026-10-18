//! Outgoing webhooks (docs/ci.md W19-R07): kiln's events, POSTed to subscribers as they happen.
//!
//! A subscription is a `webhook` integration (W11-R06) whose config lists `events`: the types it
//! wants ([`EVENTS`], or `*`), from what its scope holds (the server, an organization's
//! repositories, one repository's). Each event goes as the JSON kiln's own clients get, with
//! `X-Kiln-Event`, `X-Kiln-Delivery` and `X-Kiln-Signature-256` (HMAC-SHA256 of the body with
//! the integration's secret), in order for each subscriber and retried with backoff. Every
//! delivery is logged with its body before it is sent, so a restart resumes what was waiting and
//! any of them can be sent again (`POST /api/ci/webhooks/deliveries/:id/redeliver`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::StatusCode;
use rusqlite::{OptionalExtension, params};
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
];

/// Sent only when named: they come too often for `*`.
const BUSY: &[&str] = &["queue"];

/// How long a list of subscriptions is used before it is read again.
const FRESH: Duration = Duration::from_secs(5);

/// Events waiting for one subscriber, beyond which they are logged as dropped (to send again).
const QUEUE: usize = 1000;

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
    row: i64,
    delivery: String,
    i: Integration,
    kind: String,
    body: Arc<str>,
}

/// Follows the hub's events for as long as the server runs, a worker for each subscriber.
pub fn spawn(hub: Arc<Hub>) {
    crate::rt::spawn(async move {
        let mut rx = hub.events.subscribe();
        let mut runs: HashMap<String, (String, String)> = HashMap::new();
        let (mut subs, mut read_at): (Vec<Integration>, Option<Instant>) = (Vec::new(), None);
        let mut queues: HashMap<String, async_channel::Sender<Out>> = HashMap::new();
        // What a restart interrupted, first.
        resume(&hub, &mut queues).await;
        let mut pruned = Instant::now();
        loop {
            let text = match rx.recv().await {
                Ok(t) => t,
                Err(async_broadcast::RecvError::Overflowed(n)) => {
                    tracing::warn!(missed = n, "outgoing webhooks fell behind; some events weren't sent");
                    continue;
                }
                Err(async_broadcast::RecvError::Closed) => return,
            };
            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
            let Some(kind) = event.get("type").and_then(Value::as_str).filter(|k| EVENTS.iter().any(|(e, _)| e == k)) else { continue };
            if read_at.is_none_or(|t| t.elapsed() > FRESH) {
                if let Ok(all) = integrations::list(&hub).await {
                    subs = all.into_iter().filter(|i| i.kind == "webhook" && !wanted(&i.config).is_empty()).collect();
                    // A subscription that's gone: its worker finds so, and ends.
                    queues.retain(|id, _| subs.iter().any(|i| &i.id == id));
                }
                read_at = Some(Instant::now());
            }
            if pruned.elapsed() > Duration::from_secs(3600) {
                let before = crate::util::now_unix() - KEEP;
                let _ = hub.store.call(move |c| Ok(c.execute("DELETE FROM webhook_deliveries WHERE ts < ?1 AND status != 'pending'", [before]).map(|_| ())?)).await;
                pruned = Instant::now();
            }
            let takers: Vec<&Integration> = subs.iter().filter(|i| takes(&wanted(&i.config), kind)).collect();
            if takers.is_empty() {
                continue;
            }
            let repo = repo_of(&hub, &event, &mut runs).await;
            for i in takers.into_iter().filter(|i| in_scope(i, repo.as_ref())) {
                let delivery = crate::util::new_id();
                let Some(row) = begin(&hub, &i.name, kind, &delivery, &text).await else { continue };
                enqueue(&hub, &mut queues, Out { row, delivery, i: i.clone(), kind: kind.to_owned(), body: Arc::clone(&text) }).await;
            }
        }
    });
}

/// Hands `out` to its subscriber's worker; when that one is too far behind, the delivery stays
/// logged as dropped, to send again.
async fn enqueue(hub: &Arc<Hub>, queues: &mut HashMap<String, async_channel::Sender<Out>>, out: Out) {
    let tx = queues.entry(out.i.id.clone()).or_insert_with(|| worker(Arc::clone(hub)));
    if let Err(async_channel::TrySendError::Full(out) | async_channel::TrySendError::Closed(out)) = tx.try_send(out) {
        tracing::warn!(integration = %out.i.name, "a webhook subscriber is too far behind; an event was logged, not sent");
        finish(hub, out.row, "dropped", None, 0, Some(format!("not sent: {QUEUE} events were waiting"))).await;
    }
}

/// Deliveries a restart interrupted (logged, never finished), sent now, in order.
async fn resume(hub: &Arc<Hub>, queues: &mut HashMap<String, async_channel::Sender<Out>>) {
    let rows: Vec<(i64, String, String, String, String)> = hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT id, integration, event, delivery, body FROM webhook_deliveries WHERE status = 'pending' ORDER BY id")?;
            let rows = st.query_map([], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap_or_default();
    if !rows.is_empty() {
        tracing::info!(deliveries = rows.len(), "sending the webhook deliveries a restart interrupted");
    }
    for (row, name, kind, delivery, body) in rows {
        match integrations::get(hub, &name).await.ok().flatten() {
            Some(i) => enqueue(hub, queues, Out { row, delivery, i, kind, body: Arc::from(body) }).await,
            None => finish(hub, row, "failed", None, 0, Some(format!("the integration {name} is gone"))).await,
        }
    }
}

/// A subscriber's worker: its events, one at a time, in order, each with the subscription as it
/// is by then.
fn worker(hub: Arc<Hub>) -> async_channel::Sender<Out> {
    let (tx, rx) = async_channel::bounded::<Out>(QUEUE);
    crate::rt::spawn(async move {
        while let Ok(mut out) = rx.recv().await {
            match integrations::get(&hub, &out.i.name).await {
                Ok(Some(i)) if i.id == out.i.id => out.i = i,
                Ok(_) => {
                    finish(&hub, out.row, "failed", None, 0, Some("the integration is gone".into())).await;
                    continue;
                }
                Err(_) => {}
            }
            let (status, code, attempts, error) = send(&hub, &out, ATTEMPTS).await;
            finish(&hub, out.row, status, code, attempts, error).await;
        }
    });
    tx
}

/// The repository an event is about: its own, a run's, or a job's run's.
async fn repo_of(hub: &Hub, event: &Value, runs: &mut HashMap<String, (String, String)>) -> Option<(String, String)> {
    if let (Some(id), Some(name)) = (event.pointer("/repo/id").and_then(Value::as_str), event.pointer("/repo/name").and_then(Value::as_str)) {
        return Some((id.to_owned(), name.to_owned()));
    }
    if let Some(r) = event.get("run") {
        let (id, name) = (r.get("repo_id").and_then(Value::as_str)?, r.get("repo_name").and_then(Value::as_str)?);
        if let Some(run) = r.get("id").and_then(Value::as_str) {
            runs.insert(run.to_owned(), (id.to_owned(), name.to_owned()));
        }
        return Some((id.to_owned(), name.to_owned()));
    }
    let run = event.pointer("/job/run_id").and_then(Value::as_str)?.to_owned();
    if let Some(r) = runs.get(&run) {
        return Some(r.clone());
    }
    let found = hub.store.run(run.clone()).await.ok().flatten().map(|r| (r.repo_id, r.repo_name))?;
    if runs.len() > 10_000 {
        runs.clear();
    }
    runs.insert(run, found.clone());
    Some(found)
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

/// Logs a delivery about to be queued: its row.
async fn begin(hub: &Hub, name: &str, kind: &str, delivery: &str, body: &str) -> Option<i64> {
    let (name, kind, delivery, body, now) = (name.to_owned(), kind.to_owned(), delivery.to_owned(), body.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            c.execute("INSERT INTO webhook_deliveries (ts, integration, event, delivery, status, attempts, body) VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5)", params![now, name, kind, delivery, body])?;
            Ok(c.last_insert_rowid())
        })
        .await
        .ok()
}

/// Logs how a delivery went (attempts add to what it had).
async fn finish(hub: &Hub, row: i64, status: &'static str, code: Option<u16>, attempts: i64, error: Option<String>) {
    let now = crate::util::now_unix();
    let _ = hub.store.call(move |c| Ok(c.execute("UPDATE webhook_deliveries SET ts = ?2, status = ?3, code = ?4, attempts = attempts + ?5, error = ?6 WHERE id = ?1", params![row, now, status, code, attempts, error]).map(|_| ())?)).await;
}

/// `GET /api/ci/webhooks/deliveries?integration=&status=&limit=`: newest first, without bodies;
/// `GET …/deliveries/:id`, with its body; `POST …/deliveries/:id/redeliver`: sends it once more
/// (the same delivery id, one attempt) and answers how it went.
pub async fn route(hub: &Hub, method: &http::Method, rest: &[&str], q: &HashMap<String, String>) -> Result<Response, Fail> {
    let fail = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let row = |x: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
        Ok(json!({ "id": x.get::<_, i64>(0)?, "ts": x.get::<_, i64>(1)?, "integration": x.get::<_, String>(2)?, "event": x.get::<_, String>(3)?, "delivery": x.get::<_, String>(4)?, "status": x.get::<_, String>(5)?, "code": x.get::<_, Option<i64>>(6)?, "attempts": x.get::<_, i64>(7)?, "error": x.get::<_, Option<String>>(8)? }))
    };
    const COLUMNS: &str = "id, ts, integration, event, delivery, status, code, attempts, error";
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
            let found: Option<(Value, String)> = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {COLUMNS}, body FROM webhook_deliveries WHERE id = ?1"), [id], |x| Ok((row(x)?, x.get(9)?))).optional()?)).await.map_err(fail)?;
            let (mut d, body) = found.ok_or_else(|| (StatusCode::NOT_FOUND, "no such delivery".to_owned()))?;
            if let Some(o) = d.as_object_mut() {
                o.insert("body".into(), serde_json::from_str(&body).unwrap_or(Value::String(body)));
            }
            Ok(srv::json(StatusCode::OK, &d))
        }
        (&http::Method::POST, ["deliveries", id, "redeliver"]) => {
            let id: i64 = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "no such delivery".to_owned()))?;
            let found: Option<(String, String, String, String)> = hub.store.call(move |c| Ok(c.query_row("SELECT integration, event, delivery, body FROM webhook_deliveries WHERE id = ?1", [id], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?))).optional()?)).await.map_err(fail)?;
            let (name, kind, delivery, body) = found.ok_or_else(|| (StatusCode::NOT_FOUND, "no such delivery".to_owned()))?;
            let i = integrations::get(hub, &name).await.map_err(fail)?.ok_or_else(|| (StatusCode::GONE, format!("the integration {name} is gone")))?;
            let out = Out { row: id, delivery, i, kind, body: Arc::from(body) };
            let (status, code, attempts, error) = send(hub, &out, &[0]).await;
            finish(hub, id, status, code, attempts, error.clone()).await;
            Ok(srv::json(StatusCode::OK, &json!({ "id": id, "delivery": out.delivery, "status": status, "code": code, "error": error })))
        }
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(scope: &str, events: Value) -> Integration {
        Integration { id: "i1".into(), kind: "webhook".into(), name: "audit".into(), config: json!({ "url": "http://x", "events": events }), has_secret: true, scope: scope.into(), created_by: "admin".into(), created_at: 0, updated_at: 0, verified_at: None, last_error: None, last_used_at: None }
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
    }
}
