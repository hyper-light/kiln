//! Outgoing webhooks (docs/ci.md W19-R07): kiln's events, POSTed to subscribers as they happen.
//!
//! A subscription is a `webhook` integration (W11-R06) whose config lists `events`: the types it
//! wants ([`EVENTS`], or `*`), from what its scope holds (the server, an organization's
//! repositories, one repository's). Each event goes as the JSON kiln's own clients get, with
//! `X-Kiln-Event`, `X-Kiln-Delivery` and `X-Kiln-Signature-256` (HMAC-SHA256 of the body with
//! the integration's secret).
//!
//! **Delivered from kiln.events** (docs/bus.md, "kiln's events"; CP-T2.2 step 8): the events are
//! read from the bus's persisted topic in the order they were recorded, from a position kept in
//! kiln.db; each event's deliveries (one for each subscription that takes it) are logged with
//! their bodies, pending, in the transaction that moves the position past it, each with an id that
//! is the same however often the event is read (`X-Kiln-Delivery`, for a receiver to know one it
//! had). A sender for each subscription POSTs its deliveries one at a time, oldest first, retrying
//! with exponential backoff for a day ([`crate::events::RETRY_FOR`]): in order, at least once, and
//! a kill -9 of the server loses none. Any delivery can be sent again
//! (`POST /api/ci/webhooks/deliveries/:id/redeliver`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use http::StatusCode;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::Fail;
use super::integrations::{self, Integration};
use crate::events::{Due, Event, Log, Logged, Reader, Senders, Tried};
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

/// Deliveries waiting for one subscriber, beyond which its next are logged as dropped (to send
/// again): a day of kiln's busiest events with room.
const BACKLOG: i64 = 100_000;

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

/// Reads kiln.events for the subscriptions, and sends what it logs, for as long as the server runs.
pub fn spawn(hub: Arc<Hub>) {
    let reader = Arc::new(Webhooks { senders: Senders::new(Deliveries), subs: parking_lot::Mutex::new((Vec::new(), None)), runs: parking_lot::Mutex::new(HashMap::new()) });
    let (resumed, senders) = (Arc::clone(&hub), Arc::clone(&reader.senders));
    crate::rt::spawn(async move {
        // What a restart left waiting, first.
        if let Err(e) = senders.resume(&resumed).await {
            tracing::warn!(error = %format!("{e:#}"), "the webhook deliveries a restart left couldn't be read");
        }
    });
    crate::events::read(&hub, reader);
    let weak = Arc::downgrade(&hub);
    crate::rt::spawn(async move {
        loop {
            let Some(hub) = weak.upgrade() else { return };
            let before = hub.clock.unix_s() - KEEP;
            let _ = hub.store.call(move |c| Ok(c.execute("DELETE FROM webhook_deliveries WHERE ts < ?1 AND status != 'pending'", [before]).map(|_| ())?)).await;
            let clock = Arc::clone(&hub.clock);
            drop(hub);
            clock.sleep(Duration::from_secs(3600)).await;
        }
    });
}

/// The reader of kiln.events for the subscriptions.
struct Webhooks {
    senders: Arc<Senders<Deliveries>>,
    /// The subscriptions, and when they were read.
    subs: parking_lot::Mutex<(Vec<Integration>, Option<Instant>)>,
    /// Runs' repositories, as jobs' events need them.
    runs: parking_lot::Mutex<HashMap<String, (String, String)>>,
}

impl Webhooks {
    /// The subscriptions, read again every [`FRESH`]. Unreadable, the event waits for them: taken
    /// as having none, it would be passed over for every subscriber.
    async fn subscriptions(&self, hub: &Hub) -> anyhow::Result<Vec<Integration>> {
        {
            let subs = self.subs.lock();
            if subs.1.is_some_and(|t| t.elapsed() <= FRESH) {
                return Ok(subs.0.clone());
            }
        }
        let subs: Vec<Integration> = integrations::list(hub).await?.into_iter().filter(|i| i.kind == "webhook" && !wanted(&i.config).is_empty()).collect();
        *self.subs.lock() = (subs.clone(), Some(Instant::now()));
        Ok(subs)
    }
}

impl Reader for Webhooks {
    fn name(&self) -> &'static str {
        "webhooks"
    }

    fn kind(&self) -> &'static str {
        "webhook"
    }

    fn plan<'a>(&'a self, hub: &'a Arc<Hub>, e: &'a Event) -> BoxFuture<'a, anyhow::Result<Logged>> {
        Box::pin(async move {
            if !EVENTS.iter().any(|(k, _)| *k == e.kind) {
                return Ok(Logged::none());
            }
            let subs = self.subscriptions(hub).await.map_err(|e| e.context("the webhook subscriptions"))?;
            let takers: Vec<Integration> = subs.into_iter().filter(|i| takes(&wanted(&i.config), &e.kind)).collect();
            if takers.is_empty() {
                return Ok(Logged::none());
            }
            // A run or a job gone since its event leaves nothing to send.
            let Some(body) = body(hub, e).await? else { return Ok(Logged::none()) };
            let repo = repo_of(hub, &body, &self.runs).await;
            let takers: Vec<Integration> = takers.into_iter().filter(|i| in_scope(i, repo.as_ref())).collect();
            if takers.is_empty() {
                return Ok(Logged::none());
            }
            let (now, now_ms, kind, text) = (hub.clock.unix_s(), hub.clock.unix_ms(), e.kind.clone(), body.to_string());
            let rows: Vec<(String, String)> = takers.iter().map(|i| (i.name.clone(), delivery_id(e, &i.id))).collect();
            let targets = takers.into_iter().map(|i| i.name).collect();
            crate::metrics::events().logged.add(&["webhook"], u64::try_from(rows.len()).unwrap_or(0));
            Ok(Logged {
                write: Box::new(move |c: &Connection| {
                    for (name, delivery) in rows {
                        let waiting: i64 = c.query_row("SELECT COUNT(*) FROM webhook_deliveries WHERE status = 'pending' AND integration = ?1", [&name], |r| r.get(0))?;
                        let (status, error) = if waiting >= BACKLOG { ("dropped", Some(format!("not sent: {BACKLOG} deliveries were waiting"))) } else { ("pending", None) };
                        c.execute(
                            "INSERT INTO webhook_deliveries (ts, integration, event, delivery, status, attempts, error, body, first_at, next_at) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?8)",
                            params![now, name, kind, delivery, status, error, text, now_ms],
                        )?;
                    }
                    Ok(())
                }),
                targets,
            })
        })
    }

    fn wake(&self, hub: &Arc<Hub>, targets: Vec<String>) {
        for t in targets {
            self.senders.wake(hub, &t);
        }
    }
}

/// A delivery's id: the same for an event and a subscription however often the event is read
/// (`X-Kiln-Delivery`), 32 hex digits as kiln's ids are.
fn delivery_id(e: &Event, integration: &str) -> String {
    let mut id = crate::util::sha256_hex(format!("kiln.events/{}/{integration}", e.n).as_bytes());
    id.truncate(32);
    id
}

/// What a subscriber is sent for `e`: the JSON kiln's own clients get. A run's or a job's change
/// is the run or job as it is now, at the status and conclusion the change gave it; `None` once
/// it's gone.
async fn body(hub: &Hub, e: &Event) -> anyhow::Result<Option<Value>> {
    let text = |key: &str| e.about.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let changed = |v: &mut Value| {
        for key in ["status", "conclusion"] {
            if let (Some(o), Some(at)) = (v.as_object_mut(), e.about.get(key)) {
                o.insert(key.to_owned(), at.clone());
            }
        }
    };
    Ok(match e.kind.as_str() {
        "run" => hub.store.run(text("run")).await?.map(|r| {
            let mut run = serde_json::to_value(r).unwrap_or(Value::Null);
            changed(&mut run);
            json!({ "type": "run", "run": run })
        }),
        "job" => hub.store.job(text("job")).await?.map(|mut j| {
            j.spec = None;
            let mut job = serde_json::to_value(j).unwrap_or(Value::Null);
            changed(&mut job);
            json!({ "type": "job", "job": job })
        }),
        _ => Some(match super::notify::shown(e) {
            Some(shown) => shown,
            None => {
                let mut v = Value::Object(e.about.clone());
                if let Some(o) = v.as_object_mut() {
                    o.insert("type".into(), json!(e.kind));
                }
                v
            }
        }),
    })
}

/// The subscriptions' deliveries, as their senders use them.
struct Deliveries;

impl Log for Deliveries {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn due(&self, c: &Connection, target: &str) -> rusqlite::Result<Option<Due>> {
        c.query_row(
            "SELECT id, COALESCE(first_at, ts * 1000), COALESCE(next_at, 0), attempts FROM webhook_deliveries WHERE status = 'pending' AND integration = ?1 ORDER BY id LIMIT 1",
            [target],
            |r| Ok(Due { row: r.get(0)?, first_at: r.get(1)?, next_at: r.get(2)?, tries: r.get(3)? }),
        )
        .optional()
    }

    fn send<'a>(&'a self, hub: &'a Arc<Hub>, row: i64) -> BoxFuture<'a, Tried> {
        Box::pin(async move {
            let found: anyhow::Result<Option<(String, String, String, String)>> =
                hub.store.call(move |c| Ok(c.query_row("SELECT integration, event, delivery, body FROM webhook_deliveries WHERE id = ?1", [row], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?))).optional()?)).await;
            let (name, kind, delivery, body) = match found {
                Ok(Some(found)) => found,
                Ok(None) => return Tried::Done { status: "failed", code: None, error: Some("the delivery is gone".into()) },
                Err(e) => return Tried::Failed { code: None, error: format!("{e:#}") },
            };
            let i = match integrations::get(hub, &name).await {
                Ok(Some(i)) => i,
                Ok(None) => return Tried::Done { status: "failed", code: None, error: Some(format!("the integration {name} is gone")) },
                Err(e) => return Tried::Failed { code: None, error: format!("{e:#}") },
            };
            post(hub, &i, &kind, &delivery, &body).await
        })
    }

    fn tried(&self, c: &Connection, row: i64, a: &crate::events::After<'_>) -> rusqlite::Result<()> {
        c.execute("UPDATE webhook_deliveries SET ts = ?2, status = ?3, code = ?4, attempts = attempts + 1, error = ?5, next_at = ?6 WHERE id = ?1", params![row, a.now, a.status, a.code, a.error, a.next_at])?;
        Ok(())
    }

    fn pending(&self, c: &Connection) -> rusqlite::Result<Vec<String>> {
        let mut st = c.prepare("SELECT DISTINCT integration FROM webhook_deliveries WHERE status = 'pending'")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    fn waiting(&self, c: &Connection) -> rusqlite::Result<i64> {
        c.query_row("SELECT COUNT(*) FROM webhook_deliveries WHERE status = 'pending'", [], |r| r.get(0))
    }
}

/// The repository an event's body is about: its own, a run's, or a job's run's (`runs`: those
/// known, by run).
async fn repo_of(hub: &Hub, event: &Value, runs: &parking_lot::Mutex<HashMap<String, (String, String)>>) -> Option<(String, String)> {
    let keep = |run: String, repo: (String, String)| {
        let mut runs = runs.lock();
        if runs.len() > 10_000 {
            runs.clear();
        }
        runs.insert(run, repo);
    };
    if let (Some(id), Some(name)) = (event.pointer("/repo/id").and_then(Value::as_str), event.pointer("/repo/name").and_then(Value::as_str)) {
        return Some((id.to_owned(), name.to_owned()));
    }
    if let Some(r) = event.get("run") {
        let (id, name) = (r.get("repo_id").and_then(Value::as_str)?, r.get("repo_name").and_then(Value::as_str)?);
        if let Some(run) = r.get("id").and_then(Value::as_str) {
            keep(run.to_owned(), (id.to_owned(), name.to_owned()));
        }
        return Some((id.to_owned(), name.to_owned()));
    }
    let run = event.pointer("/job/run_id").and_then(Value::as_str)?.to_owned();
    let known = runs.lock().get(&run).cloned();
    if known.is_some() {
        return known;
    }
    let found = hub.store.run(run.clone()).await.ok().flatten().map(|r| (r.repo_id, r.repo_name))?;
    keep(run, found.clone());
    Some(found)
}

/// POSTs one delivery once: how it went.
async fn post(hub: &Hub, i: &Integration, kind: &str, delivery: &str, body: &str) -> Tried {
    let Some(url) = i.config.get("url").and_then(Value::as_str).filter(|u| !u.trim().is_empty()) else { return Tried::Done { status: "failed", code: None, error: Some("the integration has no `url`".into()) } };
    let key = match integrations::token(hub, i).await {
        Ok(k) => k,
        Err(e) => return Tried::Failed { code: None, error: format!("its signing key: {e:#}") },
    };
    let client = match crate::http::client::Client::public() {
        Ok(c) => c,
        Err(e) => return Tried::Failed { code: None, error: format!("{e:#}") },
    };
    let body = body.as_bytes().to_vec();
    let sig = key.map(|k| format!("sha256={}", crate::util::hmac_hex(k.as_bytes(), &body)));
    // A URL someone gave kiln: never this machine or a private network unless allowed (W22-AC07).
    let mut req = client.post(url).reaching(&hub.cfg.ci.reach()).header("content-type", "application/json").header("user-agent", "kiln-webhooks").header("x-kiln-event", kind).header("x-kiln-delivery", delivery).body(body).timeout(Duration::from_secs(20));
    if let Some(s) = &sig {
        req = req.header("x-kiln-signature-256", s);
    }
    match req.send().await {
        Ok(r) if r.is_success() => Tried::Sent { code: Some(r.status()) },
        Ok(r) => Tried::Failed { code: Some(r.status()), error: format!("HTTP {}", r.status()) },
        Err(e) => Tried::Failed { code: None, error: format!("{e:#}") },
    }
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
                    let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM webhook_deliveries WHERE (?1 IS NULL OR integration = ?1) AND (?2 IS NULL OR status = ?2) ORDER BY id DESC LIMIT +?3"))?;
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
            let (status, code, error) = match post(hub, &i, &kind, &delivery, &body).await {
                Tried::Sent { code } => ("sent", code, None),
                Tried::Failed { code, error } => ("failed", code, Some(error)),
                Tried::Done { status, code, error } => (status, code, error),
            };
            // One still waiting on its sender stays its sender's, unless this one went.
            let (now, recorded_error) = (hub.clock.unix_s(), error.clone());
            hub.store
                .call(move |c| Ok(c.execute("UPDATE webhook_deliveries SET ts = ?2, status = CASE WHEN status = 'pending' AND ?3 != 'sent' THEN status ELSE ?3 END, code = ?4, attempts = attempts + 1, error = ?5 WHERE id = ?1", params![id, now, status, code, recorded_error]).map(|_| ())?))
                .await
                .map_err(fail)?;
            Ok(srv::json(StatusCode::OK, &json!({ "id": id, "delivery": delivery, "status": status, "code": code, "error": error })))
        }
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::sync::atomic::{AtomicBool, Ordering};

    use kiln_seams::clock::SimClock;

    use super::*;

    /// A local endpoint: down (answering 503) while `down` is set; up, it answers 200 and keeps
    /// each request's delivery id and body, in the order they came.
    pub(crate) struct Endpoint {
        pub port: u16,
        pub down: Arc<AtomicBool>,
        pub got: Arc<parking_lot::Mutex<Vec<(String, Value)>>>,
        pub refused: Arc<std::sync::atomic::AtomicUsize>,
    }

    pub(crate) fn endpoint() -> anyhow::Result<Endpoint> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let (down, got, refused) = (Arc::new(AtomicBool::new(false)), Arc::new(parking_lot::Mutex::new(Vec::new())), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let (d, g, r) = (Arc::clone(&down), Arc::clone(&got), Arc::clone(&refused));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream);
                let (mut length, mut delivery) = (0usize, String::new());
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if let Some(v) = lower.strip_prefix("x-kiln-delivery:") {
                        delivery = v.trim().to_owned();
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let up = !d.load(Ordering::SeqCst);
                if up {
                    g.lock().push((delivery, serde_json::from_slice(&body).unwrap_or(Value::Null)));
                } else {
                    r.fetch_add(1, Ordering::SeqCst);
                }
                let answer = if up { "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n" } else { "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n" };
                let _ = reader.get_mut().write_all(answer.as_bytes());
            }
        });
        Ok(Endpoint { port, down, got, refused })
    }

    /// Lets `d` pass on `clock`, a second at a time, the hub's tasks running between.
    pub(crate) async fn pass(clock: &SimClock, d: Duration) {
        for _ in 0..d.as_secs() {
            clock.advance(Duration::from_secs(1));
            crate::rt::sleep(Duration::from_millis(3)).await;
        }
    }

    /// T-T2.2.1 (CP-T2.2 step 8, AC-T2.2.1): do subscribe a webhook to runs' changes, take its
    /// endpoint down for 10 minutes while runs are made and moved, then bring it back; expect
    /// every change delivered once it's back, in the order made, at least once (a delivery seen
    /// twice with the same id), having been retried with backoff while it was down.
    #[test]
    fn t_t2_2_1_a_webhook_endpoint_down_for_10_minutes_gets_every_event_in_order() -> anyhow::Result<()> {
        let clock = SimClock::new();
        crate::rt::block_on(async move {
            let hub = crate::hub::tests::scratch_hub_on(Arc::new(clock.clone()), |cfg| cfg.ci.allow_private_targets = vec!["127.0.0.1".into()]).await?;
            let ep = endpoint()?;
            ep.down.store(true, Ordering::SeqCst);
            let url = format!("http://127.0.0.1:{}/hook", ep.port);
            integrations::create(&hub, "webhook", "audit", json!({ "url": url, "events": ["run"] }), integrations::Secrets { secret: Some("s3cret"), signing: None }, None, "test").await.map_err(anyhow::Error::msg)?;
            crate::events::spawn(&hub);
            spawn(Arc::clone(&hub));
            // The reader takes the topic from where it is once it's made: wait for it to stand.
            for _ in 0..2000 {
                if hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM event_positions WHERE reader = 'webhooks'", [], |r| r.get::<_, i64>(0))?)).await? == 1 {
                    break;
                }
                pass(&clock, Duration::from_secs(1)).await;
            }
            hub.store.call(|c| Ok(c.execute("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('r1', 'acme/app', '', 0)", []).map(|_| ())?)).await?;
            // Ten minutes down: a run made, and 15 s later started, every 30 s.
            let mut made: Vec<(String, String)> = Vec::new();
            for n in 1..=20 {
                let id = format!("run{n:02}");
                let run = id.clone();
                hub.store.call(move |c| Ok(c.execute("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at) VALUES (?1, 'r1', ?2, 'ci', 'push', 'main', 'abc', 'queued', 0)", params![run, n]).map(|_| ())?)).await?;
                made.push((id.clone(), "queued".into()));
                pass(&clock, Duration::from_secs(15)).await;
                let run = id.clone();
                hub.store.call(move |c| Ok(c.execute("UPDATE runs SET status = 'running' WHERE id = ?1", [run]).map(|_| ())?)).await?;
                made.push((id, "running".into()));
                pass(&clock, Duration::from_secs(15)).await;
            }
            let refused = ep.refused.load(Ordering::SeqCst);
            assert!(ep.got.lock().is_empty(), "nothing went while it was down");
            assert!(refused >= 8, "tried again and again with backoff over 10 minutes: {refused} tries");
            ep.down.store(false, Ordering::SeqCst);
            // Back: everything goes within the longest wait between tries, and then some.
            let mut waited = 0;
            while ep.got.lock().iter().map(|(d, _)| d.clone()).collect::<std::collections::HashSet<_>>().len() < made.len() && waited < 900 {
                pass(&clock, Duration::from_secs(5)).await;
                waited += 5;
            }
            let got = ep.got.lock().clone();
            let mut seen = std::collections::HashMap::new();
            let mut order = Vec::new();
            for (delivery, body) in &got {
                let at = (body.pointer("/run/id").and_then(Value::as_str).unwrap_or_default().to_owned(), body.pointer("/run/status").and_then(Value::as_str).unwrap_or_default().to_owned());
                if let Some(before) = seen.insert(delivery.clone(), at.clone()) {
                    assert_eq!(before, at, "a delivery sent twice is the same event");
                    continue;
                }
                order.push(at);
            }
            assert_eq!(order, made, "every change, in the order made, after {waited} s back");
            Ok::<(), anyhow::Error>(())
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
        assert!(takes(&wanted(&json!({ "events": "agent.needs_you" })), "agent.needs_you") && takes(&wanted(&all.config), "agent.finished"));
        assert!(check_events(&json!({ "events": ["agent.needs_you", "agent.finished", "agent.stopped", "agent.paused"] })).is_ok());
    }
}
