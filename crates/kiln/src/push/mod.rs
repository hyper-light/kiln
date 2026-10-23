//! Notifications to people's devices (docs/notifications.md): the browser's own push (Web Push,
//! RFC 8030), encrypted for each device (RFC 8291) and signed by this server (RFC 8292), when an
//! agent needs you, finishes, stops or pauses, and when a machine with agents runs low on battery.
//!
//! - **Keys** ([`vapid`]): the server makes its key pair once and keeps it with its data.
//! - **Encryption** ([`ece`]): one record per message, for one device's keys.
//! - **Devices** ([`store`]): each one a person turned notifications on for, what it hears, how its
//!   last delivery went; the agents each person muted; what was said of each agent.
//! - **When** ([`watch`]): a change of where an agent stands, settled so a state that comes and
//!   goes says nothing, and never the same thing twice.
//! - **Delivery** ([`send`]): to each device of the person who started the agent (the owner, on a
//!   kiln nobody else signs in to), that hears it and hasn't muted the agent.
//! - **Everywhere else** (docs/agentic.md N2): each note is an event too (`agent.needs_you`,
//!   `agent.finished`, `agent.stopped`, `agent.paused`), for outgoing webhooks and the server's
//!   notification rules.
//! - **API** ([`api`]): `/api/push`.

pub mod api;
pub mod ece;
pub mod send;
pub mod store;
pub mod vapid;
pub mod watch;

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::hub::Hub;

use serde::{Deserialize, Serialize};

/// `[push]` (docs/notifications.md).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Push {
    /// Push to people's devices at all. Off: Settings says so, and no device is sent anything.
    pub enabled: bool,
    /// Who push services may contact about this server (VAPID's `sub`): `mailto:you@example.com` or
    /// an `https://` URL. Empty: the server's public URL when it is `https://`, else
    /// `mailto:kiln@localhost`.
    pub contact: String,
    /// Push endpoints allowed on this machine or a private network, besides the public internet's
    /// push services (a push service of your own, a test's): hosts, addresses or networks, as
    /// `[ci] allow_private_targets` writes them. Such an endpoint may be plain `http://`.
    pub allow_private: Vec<String>,
}

impl Default for Push {
    fn default() -> Self {
        Self { enabled: true, contact: String::new(), allow_private: Vec::new() }
    }
}

impl Push {
    /// VAPID's `sub`: `contact`, else the public URL when it is https, else a placeholder.
    pub fn contact_for(&self, public_url: &str) -> String {
        let c = self.contact.trim();
        if !c.is_empty() {
            return c.to_owned();
        }
        let url = public_url.trim().trim_end_matches('/');
        if url.starts_with("https://") { url.to_owned() } else { "mailto:kiln@localhost".to_owned() }
    }

    /// Where pushes may go: the public internet, and what `allow_private` lets through.
    pub fn reach(&self) -> crate::http::io::Reach {
        crate::http::io::Reach { allow: self.allow_private.clone() }
    }
}

/// How long a note waits for a device that's away: needs you holds a day; the rest, six hours.
const TTL_NEEDS: u32 = 24 * 3600;
const TTL_OTHER: u32 = 6 * 3600;
/// How often whether the push services can be reached is checked again.
const PROBE_EVERY: i64 = 10 * 60;
/// The push services checked: Google's (Chrome, Edge on Android), Mozilla's (Firefox), Apple's
/// (Safari, iPhone and iPad).
const SERVICES: &[&str] = &["https://fcm.googleapis.com/", "https://updates.push.services.mozilla.com/", "https://web.push.apple.com/"];

/// Whether this server reached the push services when it last looked.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Reached {
    /// `None`: not checked yet.
    pub ok: Option<bool>,
    pub error: Option<String>,
    /// Unix seconds.
    pub checked_at: i64,
    #[serde(skip)]
    checking: bool,
}

/// The server's notifications: its key, what it watches, what it found of the push services.
#[derive(Default)]
pub struct Service {
    keys: Mutex<Option<Result<Arc<vapid::Keys>, String>>>,
    pub watch: Mutex<watch::Watch>,
    reached: Mutex<Reached>,
}

/// The process's client for push services (public roots), built once.
fn client() -> anyhow::Result<&'static crate::http::client::Client> {
    crate::http::client::github()
}

impl Service {
    /// The server's key pair: read (or made) once, on the blocking pool.
    pub async fn keys(hub: &Arc<Hub>) -> Result<Arc<vapid::Keys>, String> {
        if let Some(k) = hub.push.keys.lock().clone() {
            return k;
        }
        let h = Arc::clone(hub);
        crate::rt::blocking(move || {
            let mut g = h.push.keys.lock();
            if g.is_none() {
                let path = h.cfg.data("push").join("vapid.pk8");
                let loaded = vapid::Keys::load_or_create(&path).map(Arc::new).map_err(|e| format!("{e:#}"));
                if let Err(e) = &loaded {
                    tracing::warn!(error = %e, "push is off: the server's push key can't be used");
                }
                *g = Some(loaded);
            }
            g.clone().unwrap_or_else(|| Err("the push key isn't loaded".into()))
        })
        .await
        .unwrap_or_else(|e| Err(format!("{e:#}")))
    }

    /// What was found of the push services, looking again in the background when it's old.
    pub fn reached(hub: &Arc<Hub>) -> Reached {
        let now = crate::util::now_unix();
        let (stale, seen) = {
            let mut g = hub.push.reached.lock();
            let stale = !g.checking && now - g.checked_at >= PROBE_EVERY;
            if stale {
                g.checking = true;
            }
            (stale, g.clone())
        };
        if stale {
            crate::rt::spawn(probe(Arc::clone(hub)));
        }
        seen
    }
}

/// Whether any push service answers (any HTTP status will do): a server without outbound HTTPS
/// can't notify anyone.
async fn probe(hub: Arc<Hub>) {
    let found = match client() {
        Ok(c) => {
            let tries = SERVICES.iter().map(|url| async move {
                let host = url.trim_start_matches("https://").trim_end_matches('/');
                c.get(url).timeout(Duration::from_secs(6)).send().await.map(|_| ()).map_err(|e| format!("{host}: {e:#}"))
            });
            futures::future::join_all(tries).await
        }
        Err(e) => vec![Err(format!("{e:#}"))],
    };
    let ok = found.iter().any(Result::is_ok);
    let error = if ok { None } else { found.into_iter().find_map(Result::err) };
    if let Some(e) = &error {
        tracing::warn!(error = %e, "the push services can't be reached from this server: devices won't get notifications");
    }
    *hub.push.reached.lock() = Reached { ok: Some(ok), error, checked_at: crate::util::now_unix(), checking: false };
    hub.emit(&json!({ "type": "push" }));
}

/// Whom agent `id` acts for (`crate::mcp::opener_key`): `user:<id>`, else the owner.
async fn opener_of(hub: &Hub, id: &str) -> String {
    hub.store.kv_get(crate::mcp::opener_key(id)).await.ok().flatten().filter(|o| o == "owner" || o.starts_with("user:")).unwrap_or_else(|| "owner".to_owned())
}

/// Follows the agents (the hub's `harness` events) and the machines' batteries for as long as the
/// server runs, and says what each calls for.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let mut rx = hub.events.subscribe();
    if hub.cfg.push.enabled {
        let _ = Service::keys(&hub).await;
    }
    // What was said before a restart isn't said again.
    match hub.store.call(|c| store::said(c)).await {
        Ok(said) => {
            let mut w = hub.push.watch.lock();
            for (agent, kind, key) in said {
                w.remember(&agent, &kind, &key);
            }
        }
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "what was said of agents couldn't be read: some notes may be said again"),
    }
    loop {
        let text = match rx.recv().await {
            Ok(t) => t,
            Err(async_broadcast::RecvError::Overflowed(n)) => {
                tracing::warn!(missed = n, "notifications fell behind the agents' changes");
                continue;
            }
            Err(async_broadcast::RecvError::Closed) => return Ok(()),
        };
        if !(text.contains("\"type\":\"harness") || text.contains("\"type\":\"machine.battery_low\"")) {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(&text) else { continue };
        match ev.get("type").and_then(Value::as_str) {
            Some("harness") => {
                let Some(info) = ev.get("info").cloned().and_then(|i| serde_json::from_value::<crate::harness::session::Info>(i).ok()) else { continue };
                let due = hub.push.watch.lock().heard(info);
                if let Some(d) = due {
                    let h = Arc::clone(&hub);
                    crate::rt::spawn(async move {
                        crate::rt::sleep(d.after).await;
                        let note = h.push.watch.lock().due(&d.agent, d.seq);
                        if let Some(n) = note {
                            tell(&h, n).await;
                        }
                    });
                }
            }
            Some("harness_closed") => {
                let Some(id) = ev.get("id").and_then(Value::as_str).map(str::to_owned) else { continue };
                hub.push.watch.lock().closed(&id);
                let h = Arc::clone(&hub);
                crate::rt::spawn(async move {
                    let _ = h.store.call(move |c| store::forget(c, &id)).await;
                });
            }
            Some("machine.battery_low") => {
                let machine = ev.get("machine").and_then(Value::as_str).unwrap_or_default().to_owned();
                let text = ev.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
                crate::rt::spawn(battery(Arc::clone(&hub), machine, text));
            }
            _ => {}
        }
    }
}

/// A note that settled: kept as said, sent as an event (webhooks, the server's notification
/// rules), and pushed to the devices of whoever started the agent.
async fn tell(hub: &Arc<Hub>, n: watch::Note) {
    let event = n.kind.event();
    let id = n.info.id.clone();
    let (title, text) = (watch::label(&n.info), watch::line(&n.info, n.kind));
    let (agent, key) = (id.clone(), n.key.clone());
    if let Err(e) = hub.store.call(move |c| store::say(c, &agent, event, &key, crate::util::now_unix())).await {
        tracing::warn!(error = %format!("{e:#}"), "couldn't keep what was said of an agent");
    }
    let opener = opener_of(hub, &id).await;
    let machine = n.info.target.strip_prefix("agent:").unwrap_or("server").to_owned();
    let url = format!("{}/agents/{id}", hub.cfg.server.public_url.trim_end_matches('/'));
    let ev = json!({
        "type": event,
        "agent": { "id": id, "name": title, "harness": n.info.kind, "harness_name": n.info.kind_name, "machine": machine, "cwd": n.info.cwd, "opener": opener },
        "title": title,
        "text": text,
        "url": url,
    });
    hub.emit(&ev);
    crate::rt::spawn(crate::ci::notify::agent_event(Arc::clone(hub), ev));
    if !hub.cfg.push.enabled || handled(hub, &opener, &n).await {
        return;
    }
    let payload = json!({ "title": title, "body": text, "tag": id, "url": format!("/agents/{id}"), "agent": id, "kind": event, "at": crate::util::now_ms() });
    let (urgency, ttl) = if n.kind == watch::Kind::NeedsYou { ("high", TTL_NEEDS) } else { ("normal", TTL_OTHER) };
    let sent = push_to(hub, &opener, event, Some(&id), &payload, urgency, &watch::topic(&id), ttl).await;
    tracing::debug!(agent = %id, event, devices = sent, "a note about an agent");
}

/// The notices note `n` is about, as cards and a person's marks name them (`store::Mark`), and
/// since when its agent must have been seen for a finished turn to be dealt with.
fn notices_of(n: &watch::Note) -> (Vec<String>, Option<i64>) {
    let id = &n.info.id;
    match n.kind {
        watch::Kind::NeedsYou => (n.info.approvals.iter().map(|a| a.id.clone()).collect(), None),
        watch::Kind::Stopped => (n.info.exit.as_ref().filter(|e| e.at > 0).map(|e| format!("stopped:{id}:{}", e.at)).into_iter().collect(), None),
        watch::Kind::Paused => (n.info.pause.as_ref().map(|p| format!("limit:{id}:{}", p.since)).into_iter().collect(), None),
        watch::Kind::Finished => (vec![format!("seen:{id}")], Some(n.info.last_turn.as_ref().map_or(i64::MAX, |t| t.at))),
    }
}

/// Whether `principal` dealt with what note `n` says already, on any device: each request put
/// away or acted on, the stop or pause put away, or (a finished turn) the agent seen since. No
/// device hears it then; the event still goes to rules and webhooks.
async fn handled(hub: &Hub, principal: &str, n: &watch::Note) -> bool {
    let (keys, seen_since) = notices_of(n);
    if keys.is_empty() {
        return false;
    }
    let (p, k) = (principal.to_owned(), keys.clone());
    let Ok(marks) = hub.store.call(move |c| store::marked(c, &p, &k)).await else { return false };
    dealt_with(&keys, seen_since, &marks)
}

/// [`handled`], from the marks of `keys` the person has.
fn dealt_with(keys: &[String], seen_since: Option<i64>, marks: &[store::Mark]) -> bool {
    match seen_since {
        Some(since) => marks.iter().any(|m| m.how == "seen" && m.at >= since),
        None => keys.iter().all(|k| marks.iter().any(|m| &m.key == k && m.how != "seen")),
    }
}

/// A machine with agents on it is low on battery: each person with an agent there hears it.
async fn battery(hub: Arc<Hub>, machine: String, text: String) {
    let target = format!("agent:{machine}");
    let agents: Vec<String> = hub.push.watch.lock().infos().filter(|i| i.target == target && watch::stand_of(i) != watch::Stand::Ended).map(|i| i.id.clone()).collect();
    if agents.is_empty() || !hub.cfg.push.enabled {
        return;
    }
    let mut people = std::collections::BTreeSet::new();
    for a in &agents {
        people.insert(opener_of(&hub, a).await);
    }
    let tag = format!("battery:{machine}");
    let payload = json!({ "title": format!("{machine} is low on battery"), "body": text, "tag": tag, "url": "/runners", "kind": "machine.battery_low", "at": crate::util::now_ms() });
    for p in people {
        push_to(&hub, &p, "machine.battery_low", None, &payload, "normal", &watch::topic(&tag), TTL_OTHER).await;
    }
}

/// Pushes `payload` to each of `principal`'s devices that hears `kind` (and hasn't muted `agent`);
/// how many.
#[allow(clippy::too_many_arguments)]
async fn push_to(hub: &Arc<Hub>, principal: &str, kind: &str, agent: Option<&str>, payload: &Value, urgency: &'static str, topic: &str, ttl: u32) -> usize {
    let (p, k, a) = (principal.to_owned(), kind.to_owned(), agent.map(str::to_owned));
    let devices = hub
        .store
        .call(move |c| {
            if let Some(a) = &a
                && store::is_muted(c, &p, a)?
            {
                return Ok(Vec::new());
            }
            store::hearing(c, &p, &k)
        })
        .await
        .unwrap_or_default();
    let body: Arc<[u8]> = Arc::from(serde_json::to_vec(payload).unwrap_or_default());
    for d in &devices {
        let (h, d, body, topic) = (Arc::clone(hub), d.clone(), Arc::clone(&body), topic.to_owned());
        crate::rt::spawn(async move {
            deliver(&h, &d, &body, urgency, Some(&topic), ttl, send::WAITS).await;
        });
    }
    devices.len()
}

/// One push to one device, and what came of it kept for Settings.
async fn deliver(hub: &Arc<Hub>, d: &store::Device, body: &[u8], urgency: &'static str, topic: Option<&str>, ttl: u32, waits: &[u64]) -> send::Outcome {
    let outcome = match (Service::keys(hub).await, client(), vapid::unb64(&d.p256dh), vapid::unb64(&d.auth)) {
        (Ok(keys), Ok(client), Some(p256dh), Some(auth)) => {
            let (contact, reach) = (hub.cfg.push.contact_for(&hub.cfg.server.public_url), hub.cfg.push.reach());
            let sender = send::Sender { client, keys: &keys, contact: &contact, reach: &reach };
            sender.deliver(&send::Device { endpoint: &d.endpoint, p256dh: &p256dh, auth: &auth }, &send::Message { payload: body, urgency, topic, ttl }, waits).await
        }
        (Err(e), ..) => send::Outcome::Failed(e),
        (_, Err(e), ..) => send::Outcome::Failed(format!("{e:#}")),
        _ => send::Outcome::Failed("the device's keys aren't base64url".into()),
    };
    let (id, endpoint, now, o) = (d.id.clone(), d.endpoint.clone(), crate::util::now_unix(), outcome.clone());
    let service = vapid::origin(&d.endpoint).unwrap_or_default();
    match &outcome {
        send::Outcome::Delivered => {}
        send::Outcome::Gone(status) => tracing::info!(%service, status, device = %d.device, "a device's push subscription is gone: it's forgotten"),
        send::Outcome::Failed(why) => tracing::warn!(%service, device = %d.device, error = %why, "a push wasn't delivered"),
    }
    let _ = hub
        .store
        .call(move |c| match &o {
            send::Outcome::Delivered => store::delivered(c, &id, now),
            send::Outcome::Gone(_) => store::drop_endpoint(c, &endpoint).map(|_| ()),
            send::Outcome::Failed(why) => store::failed(c, &id, why, now),
        })
        .await;
    hub.emit(&json!({ "type": "push" }));
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watch's lock is held only for `heard`, `due` and `closed`, never across a wait: from
    /// many threads at once (as the event loop and the settling timers take it), each finishes.
    #[test]
    fn the_watch_is_never_held_across_a_wait() -> anyhow::Result<()> {
        use crate::harness::model::{Status, ToolKind};
        use crate::harness::session::{ApprovalBrief, Info};
        let svc = Arc::new(Service::default());
        let (tx, rx) = std::sync::mpsc::channel();
        for t in 0..4u64 {
            let (svc, tx) = (Arc::clone(&svc), tx.clone());
            std::thread::spawn(move || {
                let mut said = 0;
                for i in 0..400u64 {
                    let id = format!("ag{}", i % 5);
                    let approvals = if i % 3 == 0 { vec![ApprovalBrief { id: format!("a{t}-{i}"), tool: ToolKind::Shell, tool_name: "Bash".into(), title: "ls".into(), at: i64::try_from(i).unwrap_or(0) }] } else { Vec::new() };
                    let status = if approvals.is_empty() { Status::Running } else { Status::Waiting };
                    let due = svc.watch.lock().heard(Info { id: id.clone(), status, approvals, ..Info::default() });
                    if let Some(d) = due
                        && svc.watch.lock().due(&d.agent, d.seq).is_some()
                    {
                        said += 1;
                    }
                    if i % 97 == 0 {
                        svc.watch.lock().closed(&id);
                    }
                    let _ = svc.watch.lock().infos().count();
                }
                let _ = tx.send(said);
            });
        }
        let mut said = 0;
        for _ in 0..4 {
            said += rx.recv_timeout(Duration::from_secs(30)).map_err(|_| anyhow::anyhow!("a thread never finished: the watch's lock was held"))?;
        }
        assert!(said > 0, "approvals were said");
        Ok(())
    }

    #[test]
    fn the_contact_is_said_or_the_public_url_or_a_placeholder() {
        let mut p = Push::default();
        assert_eq!(p.contact_for("https://kiln.example.com/"), "https://kiln.example.com");
        assert_eq!(p.contact_for("http://adacachy:7070"), "mailto:kiln@localhost", "only an https URL says who runs it");
        p.contact = " mailto:ops@example.com ".into();
        assert_eq!(p.contact_for("https://kiln.example.com"), "mailto:ops@example.com");
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    fn mark(key: &str, how: &str, at: i64) -> store::Mark {
        store::Mark { key: key.into(), how: how.into(), at }
    }

    #[test]
    fn a_note_is_dealt_with_once_each_of_its_notices_is_or_its_agent_was_seen_since() {
        let keys = vec!["q1".to_owned(), "q2".to_owned()];
        assert!(!dealt_with(&keys, None, &[mark("q1", "away", 5)]), "one of two put away");
        assert!(dealt_with(&keys, None, &[mark("q1", "away", 5), mark("q2", "done", 6)]));
        assert!(!dealt_with(&keys, None, &[mark("q1", "seen", 5), mark("q2", "seen", 6)]), "seen isn't put away");
        let seen = vec!["seen:ag1".to_owned()];
        assert!(dealt_with(&seen, Some(100), &[mark("seen:ag1", "seen", 120)]), "seen after its turn ended");
        assert!(!dealt_with(&seen, Some(100), &[mark("seen:ag1", "seen", 90)]), "seen before");
        assert!(!dealt_with(&seen, Some(100), &[]));
    }
}
