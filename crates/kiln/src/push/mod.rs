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
use std::time::{Duration, Instant};

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
    /// Tests: the events its listener took, and the turns it ended by itself, its next event to wait for its next poll
    /// ([`listen`]): its own, where a thread's counted every listener that thread ran, another test's among them.
    #[cfg(test)]
    took: (std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64),
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
    hub.emit(&crate::wire::events::ServerEvent::Push);
}

/// Whom agent `id` acts for (`crate::mcp::opener_key`): `user:<id>`, else the owner.
async fn opener_of(hub: &Hub, id: &str) -> String {
    hub.store.kv_get(crate::mcp::opener_key(id)).await.ok().flatten().filter(|o| o == "owner" || o.starts_with("user:")).unwrap_or_else(|| "owner".to_owned())
}

/// A frame [`run`] follows, read: an agent session's info, a session gone, a machine's battery low.
enum Followed {
    Harness(Box<crate::harness::session::Info>),
    Closed(String),
    BatteryLow { machine: String, text: String },
}

/// A frame whose kind isn't at its front ([`crate::wire::events::kind_of`]): its kind, read whole.
#[derive(serde::Deserialize)]
struct Tagged {
    #[serde(rename = "type")]
    kind: String,
}

/// What [`run`] reads of an agent session's `harness` frame: its info.
#[derive(serde::Deserialize)]
struct Heard {
    info: crate::harness::session::Info,
}

/// What [`run`] reads of a `harness_closed` frame: the session's id.
#[derive(serde::Deserialize)]
struct Closed {
    id: String,
}

/// What [`run`] reads of a `machine.battery_low` frame: the machine, and what to say.
#[derive(serde::Deserialize)]
struct Low {
    #[serde(default)]
    machine: String,
    #[serde(default)]
    text: String,
}

/// What [`run`] follows of the frame `text`, if anything. Its kind is read off its front, what it doesn't
/// follow passed over unparsed, and the rest parsed once, straight into what it reads of them (goal 4, plan
/// 01a1a59c2a9c step 5: every change of an agent session's info is a `harness` frame, which was parsed whole,
/// its info copied out of that and read again).
fn followed(text: &str) -> Option<Followed> {
    let kind = match crate::wire::events::kind_of(text) {
        Some(k) => std::borrow::Cow::Borrowed(k),
        None => std::borrow::Cow::Owned(serde_json::from_str::<Tagged>(text).ok()?.kind),
    };
    match &*kind {
        "harness" => serde_json::from_str::<Heard>(text).ok().map(|h| Followed::Harness(Box::new(h.info))),
        "harness_closed" => serde_json::from_str::<Closed>(text).ok().map(|c| Followed::Closed(c.id)),
        "machine.battery_low" => serde_json::from_str::<Low>(text).ok().map(|l| Followed::BatteryLow { machine: l.machine, text: l.text }),
        _ => None,
    }
}

/// What parsing a `harness` frame is expected to take, by its length ([`follow`]): learned as a runner frame's read is
/// (`crate::wire::payload::Expect`), from each parse's cost to its thread (its CPU time unless it waited).
static FOLLOWING: crate::wire::payload::Expect = crate::wire::payload::Expect::nothing_yet();

/// [`followed`]: a `harness` frame parsed where it's cheaper, as a runner frame's read is placed (goal 9, plan 01a1a87b2cec
/// step 5; [`parses_here`]): its whole info, every subagent, approval and limit of it, parsed on the listener's shard for
/// every change of every session, held it 1 to 6 ms in g9p7 (step 3), so each went to the compute pool, however small;
/// on a Mac's lab (2026-11-06) they were 41.5 a second, parsed in 11.5 µs and waited for 60 µs, each a worker's wake and
/// the listener's. Other frames' kinds are read off their fronts here, and those few parsed here too. Each is waited
/// for, so the listener still takes them in the order they came.
async fn follow(text: Arc<str>) -> Option<Followed> {
    if crate::wire::events::kind_of(&text) != Some("harness") {
        return followed(&text);
    }
    let len = u64::try_from(text.len()).unwrap_or(u64::MAX);
    let here = parses_here(&FOLLOWING, len, crate::rt::Sides::here());
    let (parsed, cost) = if here {
        let (f, spent) = hyper_rt::futures::spent(|| followed(&text));
        (f, spent.compute_cost_ns())
    } else {
        match crate::rt::compute(move || {
            let (f, spent) = hyper_rt::futures::spent(|| followed(&text));
            (f, spent.compute_cost_ns())
        })
        .await
        {
            Ok(parsed) => parsed,
            Err(_) => return None,
        }
    };
    FOLLOWING.learn(len, cost, here);
    parsed
}

/// Whether a `harness` frame of `len` bytes is parsed on the listener's shard, by what `expect` learned of such parses
/// against `sides` (the shard's quantum and the compute pool's round trip, `crate::rt::Sides::take_here`): where it fits a
/// turn and costs no more than the pool's round trip; off it past either, and while nothing is learned (the first teaches).
fn parses_here(expect: &crate::wire::payload::Expect, len: u64, sides: crate::rt::Sides) -> bool {
    expect.on_shard_with(len, sides).is_some()
}

/// Follows the agents (the hub's `harness` events) and the machines' batteries for as long as the
/// server runs, and says what each calls for.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    // The machines' batteries on their lane (`Hub::battery_events`: the hub's other events don't wake this listener to be
    // passed over; goal 9, plan 01a1a87b2cec step 5), agent sessions' on their owners' lanes (`Hub::harness_events`; goal 4,
    // plan 01a1a59c2a9c step 5): both read, each as it comes. Each lane holds as many as the hub's ring held in all: what an
    // agent's notes are made of it misses none it can help.
    let (rx, sessions) = (hub.subscribe_battery()?, hub.harness_events.subscribe(crate::hub::EVENTS)?);
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
    listen(&hub, rx, sessions).await;
    Ok(())
}

#[cfg(test)]
/// The next event waiting already for the listener on `battery` or `sessions`, a battery's first; none where neither has one.
fn waiting(battery: &mut crate::memory::lanes::Reader<Arc<str>>, sessions: &mut crate::memory::lanes::Reader<Arc<str>>) -> Option<Result<Arc<str>, async_broadcast::RecvError>> {
    for lane in [battery, sessions] {
        match lane.try_recv() {
            Ok(text) => return Some(Ok(text)),
            Err(async_broadcast::TryRecvError::Overflowed(n)) => return Some(Err(async_broadcast::RecvError::Overflowed(n))),
            Err(async_broadcast::TryRecvError::Closed) => return Some(Err(async_broadcast::RecvError::Closed)),
            Err(async_broadcast::TryRecvError::Empty) => {}
        }
    }
    None
}

/// [`run`]'s listening, its lanes `battery` and `sessions` subscribed, until they close. Its events are taken in turns on its
/// shard (`crate::rt::Turns`), each a piece: one waiting already goes on in the turn it has, one it waited for starts a turn
/// afresh, and the turn ends, its poll with it, once its next piece would pass the shard's quantum, whether or not anything
/// else wants the shard (goal 9, plan 01a1a87b2cec step 3). It took every event waiting in one poll: as the lab's fill
/// wrote its sessions' backlogs, their changes held its shard 0.36 ms on the CPU calm and 0.79 ms beside CPU hogs at the
/// longest (g9p35, "the server's own tasks", `server.rs:206`).
///
/// The notes waiting to settle are its too: it sleeps until the first is due beside its lanes, armed while a note waits and
/// let go as soon as none does, and says those due when it finds them so, busy or woken (goal 9, plan 01a1a87b2cec step 5:
/// each note slept in a task of its own, which woke when it was due even after a change had called it off, as a turn's
/// next calls off its end's: 2.2 timer wakes a second in the lab, train 34's census).
async fn listen(hub: &Arc<Hub>, mut rx: crate::memory::lanes::Reader<Arc<str>>, mut sessions: crate::memory::lanes::Reader<Arc<str>>) {
    use futures::FutureExt as _;
    let mut turns = crate::rt::Turns::new();
    let mut settling: Option<(Instant, crate::rt::Sleep)> = None;
    loop {
        let mut next = hub.push.watch.lock().next_due();
        if next.is_some_and(|at| at <= Instant::now()) {
            settle(hub);
            next = hub.push.watch.lock().next_due();
        }
        if settling.as_ref().map(|(at, _)| *at) != next {
            settling = next.map(|at| (at, crate::rt::sleep_until(at)));
        }
        let got = match waiting(&mut rx, &mut sessions) {
            Some(got) => got,
            None => {
                let woke = {
                    let settled = async {
                        match settling.as_mut() {
                            Some((_, sleep)) => sleep.await,
                            None => std::future::pending().await,
                        }
                    };
                    futures::select_biased! {
                        m = rx.recv().fuse() => Some(m),
                        m = sessions.recv().fuse() => Some(m),
                        () = settled.fuse() => None,
                    }
                };
                // Waited for: the shard went to its other tasks meanwhile, and this is a turn's first piece.
                turns = crate::rt::Turns::new();
                match woke {
                    Some(got) => got,
                    None => {
                        settling = None;
                        settle(hub);
                        continue;
                    }
                }
            }
        };
        let text = match got {
            Ok(t) => Some(t),
            Err(async_broadcast::RecvError::Overflowed(n)) => {
                tracing::warn!(missed = n, "notifications fell behind the agents' changes");
                None
            }
            Err(async_broadcast::RecvError::Closed) => return,
        };
        if let Some(what) = match text {
            Some(text) => follow(text).await,
            None => None,
        } {
            heed(hub, what);
        }
        #[cfg(test)]
        {
            hub.push.took.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            hub.push.took.1.fetch_add(u64::from(turns.over()), std::sync::atomic::Ordering::Relaxed);
        }
        turns.end().await;
    }
}

/// What an event the listener follows calls for ([`listen`]).
fn heed(hub: &Arc<Hub>, what: Followed) {
    match what {
        Followed::Harness(info) => hub.push.watch.lock().heard(*info, Instant::now()),
        Followed::Closed(id) => {
            hub.push.watch.lock().closed(&id);
            let h = Arc::clone(hub);
            crate::rt::spawn(async move {
                let _ = h.store.call(move |c| store::forget(c, &id)).await;
            });
        }
        Followed::BatteryLow { machine, text } => {
            crate::rt::spawn(battery(Arc::clone(hub), machine, text));
        }
    }
}

/// The notes due now, each told in a task of its own ([`tell`]).
fn settle(hub: &Arc<Hub>) {
    let notes = hub.push.watch.lock().due(Instant::now());
    for n in notes {
        let h = Arc::clone(hub);
        crate::rt::spawn(async move { tell(&h, n).await });
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
    let agent = crate::wire::events::NoticeAgent {
        id: id.clone(),
        name: title.clone(),
        harness: n.info.kind.clone(),
        harness_name: n.info.kind_name.clone(),
        machine,
        cwd: n.info.cwd.clone(),
        opener: opener.clone(),
    };
    let ev = crate::wire::events::ServerEvent::AgentNotice(Box::new(crate::wire::events::AgentNotice { kind: n.kind.notice(), agent, title: title.clone(), text: text.clone(), url }));
    hub.emit(&ev);
    crate::rt::spawn(crate::ci::notify::agent_event(Arc::clone(hub), ev.value()));
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
    let payload = json!({ "title": format!("{} is low on battery", hub.shown(&machine)), "body": text, "tag": tag, "url": "/runners", "kind": "machine.battery_low", "at": crate::util::now_ms() });
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
    hub.emit(&crate::wire::events::ServerEvent::Push);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 5: a `harness` frame is parsed where it's cheaper, as a runner frame is read. Do: what
    /// parsing frames of 4 KB took, learned at 11.5 µs each (a Mac's lab), then at 1.5 ms (a large info's). Expect, against
    /// a quantum of 266.7 µs and a pool round trip of 60 µs: nothing learned, off the shard (the first teaches); the short
    /// parse on the listener's shard; the long one off it. Before, every one went to the pool.
    #[test]
    fn g9_s5_a_harness_frame_is_parsed_where_its_cheaper() {
        let sides = crate::rt::Sides { quantum_ns: Some(crate::rt::STEP_BUDGET_NS), trip_ns: Some(60_000) };
        let (short, long) = (crate::wire::payload::Expect::nothing_yet(), crate::wire::payload::Expect::nothing_yet());
        assert!(!parses_here(&short, 4_096, sides), "nothing learned: off the shard");
        for _ in 0..32 {
            short.learn(4_096, 11_500, true);
            long.learn(4_096, 1_500_000, false);
        }
        assert!(parses_here(&short, 4_096, sides), "11.5 µs: on the listener's shard");
        assert!(!parses_here(&long, 4_096, sides), "1.5 ms: off it");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the listener takes a burst of events in turns on its shard, its poll ended with each
    /// turn whether or not anything else wants the shard; it took every event waiting in one poll (the lab's fill: 0.36 ms on
    /// the CPU calm, 0.79 ms beside CPU hogs). Do: 1,500 machines' low batteries (each read on the listener's shard, a few µs:
    /// milliseconds in all, several of a loaded shard's quanta), sent before it looks. Expect: it takes them all, in more than
    /// one poll, ending a turn by itself at least once.
    #[test]
    fn g9_s3_the_listener_takes_a_burst_of_events_in_turns() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (rx, sessions) = (hub.subscribe_battery()?, hub.harness_events.subscribe(crate::hub::EVENTS)?);
            let n = 1_500u64;
            for i in 0..n {
                let low = crate::os::battery::Battery { percent: 9, ac: false, charging: false, minutes: None };
                hub.emit(&crate::wire::events::ServerEvent::MachineBatteryLow { machine: format!("m{i}"), battery: low, text: "low".into() });
            }
            let took_now = |p: &Service| (p.took.0.load(std::sync::atomic::Ordering::Relaxed), p.took.1.load(std::sync::atomic::Ordering::Relaxed));
            let (took, ended) = took_now(&hub.push);
            let (mut now, mut polls, began) = ((took, ended), 0u64, std::time::Instant::now());
            let mut listening = std::pin::pin!(listen(&hub, rx, sessions));
            std::future::poll_fn(|cx| {
                polls += 1;
                let _ = listening.as_mut().poll(cx);
                now = took_now(&hub.push);
                if now.0 - took >= n || began.elapsed() > std::time::Duration::from_secs(60) { std::task::Poll::Ready(()) } else { std::task::Poll::Pending }
            })
            .await;
            anyhow::ensure!(now.0 - took == n, "the listener took {} of {n} events in 60 s", now.0 - took);
            anyhow::ensure!(polls > 1, "{n} events in one poll");
            anyhow::ensure!(now.1 > ended, "no turn ended by itself in {polls} polls");
            Ok(())
        })
    }

    /// The watch's lock is held only for `heard`, `due` and `closed`, never across a wait: from
    /// many threads at once, each finishes.
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
                    let approvals = if i % 3 == 0 { vec![ApprovalBrief { id: format!("a{t}-{i}"), tool: ToolKind::Shell, tool_name: "Bash".into(), title: "ls".into(), at: i64::try_from(i).unwrap_or(0), agent: None }] } else { Vec::new() };
                    let status = if approvals.is_empty() { Status::Running } else { Status::Waiting };
                    let now = std::time::Instant::now();
                    svc.watch.lock().heard(Info { id: id.clone(), status, approvals, ..Info::default() }, now);
                    said += svc.watch.lock().due(now + watch::Kind::NeedsYou.settle()).len();
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

    /// Goal 4, plan 01a1a59c2a9c step 5: what the notifications' listener reads of each frame it follows,
    /// parsed once, is what the frame says, quotes and escapes and all; the kinds it doesn't follow it passes
    /// over; and a frame whose kind isn't at its front is read whole.
    #[test]
    fn g4_s5_push_reads_each_frame_it_follows_once() -> anyhow::Result<()> {
        use crate::harness::model::{Status, ToolKind};
        use crate::harness::session::{ApprovalBrief, Info};
        use crate::wire::events::{ServerEvent, harness_frame};
        let odd = "a \"quoted\" word, a \\ backslash and \"type\":\"harness_closed\"";
        let approvals = vec![ApprovalBrief { id: "a1".into(), tool: ToolKind::Shell, tool_name: "Bash".into(), title: odd.into(), at: 7, agent: None }];
        let info = Info { id: "s\"1".into(), kind: "claude".into(), status: Status::Waiting, summary: odd.into(), approvals, ..Info::default() };
        for text in [harness_frame(&serde_json::to_vec(&info)?).to_string(), ServerEvent::Harness { info: Box::new(info.clone()) }.text()] {
            let Some(Followed::Harness(read)) = followed(&text) else { anyhow::bail!("not read: {text}") };
            anyhow::ensure!(*read == info, "{text}");
        }
        let Some(Followed::Closed(id)) = followed(&ServerEvent::HarnessClosed { id: odd.into() }.text()) else { anyhow::bail!("a session gone, not read") };
        anyhow::ensure!(id == odd);
        let battery = crate::os::battery::Battery { percent: 9, ac: false, charging: false, minutes: Some(4) };
        let Some(Followed::BatteryLow { machine, text }) = followed(&ServerEvent::MachineBatteryLow { machine: "mac".into(), battery, text: odd.into() }.text()) else { anyhow::bail!("a battery low, not read") };
        anyhow::ensure!(machine == "mac" && text == odd);
        for text in [ServerEvent::Terminals.text(), ServerEvent::Comment(Box::default()).text(), ServerEvent::AiSessions { machine: "harness".into() }.text()] {
            anyhow::ensure!(followed(&text).is_none(), "{text}");
        }
        let Some(Followed::Closed(id)) = followed(r#"{ "type": "harness_closed", "id": "s1" }"#) else { anyhow::bail!("a frame read whole, not read") };
        anyhow::ensure!(id == "s1");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the listener reads a `harness` frame's info on the compute pool and the
    /// rest where it is, each as `followed` reads it: the same info, either way it's sent; a session gone; and
    /// nothing of a frame it passes over.
    #[test]
    fn g9_s3_a_harness_frame_is_read_off_the_shard_as_followed_reads_it() -> anyhow::Result<()> {
        use crate::harness::session::Info;
        use crate::wire::events::{ServerEvent, harness_frame};
        crate::rt::block_on(async {
            let info = Info { id: "s1".into(), kind: "claude".into(), summary: "a \"quoted\" word".into(), ..Info::default() };
            for text in [harness_frame(&serde_json::to_vec(&info)?), Arc::from(ServerEvent::Harness { info: Box::new(info.clone()) }.text())] {
                let Some(Followed::Harness(read)) = follow(Arc::clone(&text)).await else { anyhow::bail!("not read: {text}") };
                anyhow::ensure!(*read == info, "{text}");
            }
            let Some(Followed::Closed(id)) = follow(Arc::from(ServerEvent::HarnessClosed { id: "s1".into() }.text())).await else { anyhow::bail!("a session gone, not read") };
            anyhow::ensure!(id == "s1");
            anyhow::ensure!(follow(Arc::from(ServerEvent::Terminals.text())).await.is_none(), "a frame it passes over");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5 (train 34's census: push's settling timers, 2.2 timer wakes a second in the lab,
    /// each a turn's end its next turn called off): a note called off before it settles wakes nothing, and one left to
    /// settle goes once it's due. Do: the listener on a scratch hub; an agent at work asks for an approval and has it
    /// answered at once; then twice the settle with nothing. Expect: no timer of this file's ended a shard's park, unless
    /// the note went (a machine so loaded it settled first): each note slept in a task of its own, which woke when it
    /// was due even after it was called off. Then an approval left waiting: its note is said.
    #[test]
    fn g9_s5_a_note_called_off_before_it_settles_wakes_nothing() -> anyhow::Result<()> {
        use crate::harness::model::{Status, ToolKind};
        use crate::harness::session::{ApprovalBrief, Info};
        use crate::wire::events::ServerEvent;
        // Each bound here only ends a hang: a loaded machine takes what time it takes.
        const HANG: Duration = Duration::from_secs(300);
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let info = |status: Status, ask: Option<&str>| {
                let approvals = ask.map(|id| ApprovalBrief { id: id.into(), tool: ToolKind::Shell, tool_name: "Bash".into(), title: "ls".into(), at: 1, agent: None }).into_iter().collect();
                ServerEvent::Harness { info: Box::new(Info { id: "ag1".into(), kind: "claude".into(), status, approvals, ..Info::default() }) }
            };
            let said = |hub: &Arc<Hub>, key: &'static str| {
                let hub = Arc::clone(hub);
                async move { hub.store.call(|c| store::said(c)).await.map(|all| all.iter().filter(|(agent, kind, kept)| agent == "ag1" && kind == "agent.needs_you" && kept == key).count()) }
            };
            let listening = hub.harness_events.receiver_count();
            let task = crate::rt::spawn(run(Arc::clone(&hub)));
            let deadline = Instant::now() + HANG;
            while hub.harness_events.receiver_count() == listening {
                anyhow::ensure!(Instant::now() < deadline, "the listener never listened");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            hub.emit(&info(Status::Running, None));
            hub.emit(&info(Status::Waiting, Some("q1")));
            hub.emit(&info(Status::Running, None));
            // Slept in rt.rs: a sleep here would be a timer of this file's, and counted against it.
            let woke = crate::rt::timer_wakes_over("push/mod.rs", watch::Kind::NeedsYou.settle() * 2).await;
            let went = said(&hub, "q1").await?;
            anyhow::ensure!(woke <= u64::try_from(went)?, "{woke} timer wakes for a note called off before it settled ({went} said)");
            hub.emit(&info(Status::Waiting, Some("q2")));
            while said(&hub, "q2").await? == 0 {
                anyhow::ensure!(Instant::now() < deadline, "a note left to settle was never said");
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            task.abort();
            Ok(())
        })
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
