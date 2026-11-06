//! One protocol socket's task (goal 3 of the browser protocol program, plan
//! `01a1a832eb2f711f9c6f51d88effe89d` steps 2 to 4): goal 2's server half (`kiln_bp::server::Server`)
//! driven over the WebSocket. What came is handed to it as it comes; whenever the socket can take a
//! message, it's asked for the next one, by priority, of at most what the window and the chunk allow
//! (`super::send`); its subscriptions start from the topics' owner and its changes come from the
//! owner's broadcast. Everything runs in this one task: no lock, nothing shared but by message.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::FutureExt as _;
use futures::StreamExt as _;
use kiln_bp::answers::Answers;
use kiln_bp::conn::{Prio, Trust};
use kiln_bp::frames::{Call, CloseCode, EndCode, Method, OpenRefusal, Outcome, Topic};
use kiln_bp::server::{Event, Server, ServerConfig};
use kiln_bp::tickets::{Principal, Redeem};
use kiln_bp::topic::Start;
use kiln_bp::window::{Config as WindowConfig, Window};
use tungstenite::protocol::WebSocketConfig;

use super::owner::{Ask, Handle, Started};
use super::{Control, Stats, send};
use crate::http::ws::{self, Message};
use crate::hub::Hub;

/// Shape: how often the socket reads what the kernel says of it and sizes its chunk and mark, ms:
/// often enough to follow a path that changes within a few of its round trips, seldom enough to
/// cost nothing (one `getsockopt` a tenth of a second).
const SAMPLE_MS: u64 = 100;

/// Shape: the room a transmit has when the window is full: a probe, an acknowledgement or an idle
/// frame fits, nothing else does.
const CONTROL_ROOM: usize = 24;

/// Shape: the most requests one socket runs at once; past it a request is answered `Nack` (again).
const REQUESTS_AT_ONCE: usize = 32;

/// Shape: the most an answer's body holds (the protocol's largest frame, less its envelope).
const ANSWER_MOST: usize = (1 << 20) - 1024;

/// The WebSocket's settings for the protocol: messages up to `max_up` from the page; tungstenite's
/// write buffer 0, so each message is written and flushed as it's given, and its bound one message
/// of the largest chunk (and a control frame beside it), so nothing builds up there.
pub(super) fn ws_config(max_up: usize) -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(max_up))
        .max_frame_size(Some(max_up))
        .write_buffer_size(0)
        .max_write_buffer_size(send::CHUNK_MAX.saturating_add(send::WS_HEADER).saturating_mul(2).saturating_add(1024))
}

/// What a socket's task is given.
pub(super) struct Socket {
    pub hub: Arc<Hub>,
    pub caller: crate::authz::Caller,
    pub owner: Handle,
    pub socket: ws::WebSocket,
    pub control: Option<Arc<Control>>,
    pub stats: Arc<Stats>,
    pub version: u32,
}

/// The open's ticket, redeemed in the server's tickets, and only for the caller who upgraded: a
/// ticket of another's is refused as not one this server made for them.
struct Redeemer<'a> {
    live: &'a super::Live,
    who: &'a str,
}

impl Redeem for Redeemer<'_> {
    fn redeem(&mut self, ticket: &[u8], now: u64) -> Result<Principal, OpenRefusal> {
        let p = self.live.tickets.update(|t| t.redeem(ticket, now))?;
        if p.who == self.who { Ok(p) } else { Err(OpenRefusal::BadTicket) }
    }
}

/// A message being written: the sender, given back with how it went.
type Sending = Pin<Box<dyn Future<Output = (ws::Sender, Result<(), tungstenite::Error>)> + Send>>;

fn now_ms() -> u64 {
    u64::try_from(crate::util::now_ms()).unwrap_or(0)
}

/// The overload class of the most urgent frame in a message (`crate::overload`): a person's keystrokes
/// and the connection's own frames are Critical, live state and streams Normal, bulk Background.
fn class_of(prio: Prio) -> crate::overload::Class {
    match prio {
        Prio::Control | Prio::Interactive => crate::overload::Class::Critical,
        Prio::State | Prio::Stream => crate::overload::Class::Normal,
        Prio::Bulk => crate::overload::Class::Background,
    }
}

/// Who a subscriber is for the topics' access, decided once (`crate::bp::allowed`).
async fn scope_of(hub: &Hub, caller: &crate::authz::Caller) -> crate::bp::Caller {
    let repos = match crate::authz::gate::readable(hub, caller, "runs.read").await {
        Some(list) => list.into_iter().collect(),
        None => hub.store.repos().await.unwrap_or_default().into_iter().map(|r| r.name).collect(),
    };
    crate::bp::Caller { owner: caller.is_owner(), person: crate::ci::inbox::person(&crate::ci::review::who(caller).0), repos, sessions: Default::default() }
}

/// What the socket asked for its congestion control, and what came of it, in words.
async fn congestion(hub: &Hub, control: Option<&Control>) -> String {
    let Some(socket) = control.and_then(|c| c.socket.as_ref()) else { return "not offered (no handle to the socket on this platform)".to_owned() };
    let have = |s: &hyper_rt::tcp::SocketOptions| s.congestion().ok().flatten().map_or_else(|| "not offered".to_owned(), |c| c.to_string());
    let Some(want) = hub.live.congestion else { return format!("{} (the host's default; [server] browser_congestion unset)", have(socket)) };
    match socket.set_congestion(want) {
        Ok(true) => {
            let got = have(socket);
            let path = format!("/sys/module/tcp_{got}/version");
            let version = crate::rt::blocking(move || std::fs::read_to_string(path).ok()).await.ok().flatten().map(|v| v.trim().to_owned());
            format!("{got} (asked {want}; module version {})", version.as_deref().unwrap_or("not declared: mainline's v1"))
        }
        Ok(false) => format!("{} (asked {want}: not offered on this platform)", have(socket)),
        Err(e) => format!("{} ({e}: the host's administrator loads it and adds it to net.ipv4.tcp_allowed_congestion_control)", have(socket)),
    }
}

/// The socket's task until it ends.
pub(super) async fn run(s: Socket) -> anyhow::Result<()> {
    let Socket { hub, caller, owner, socket, control, stats, version } = s;
    let _ = stats.congestion.set(congestion(&hub, control.as_deref()).await);
    let scope = scope_of(&hub, &caller).await;
    let mut changes = owner.changes();
    // Lock-free queues (`crate::memory::queue`; goal 5, plan 01a1a59c2aae, its top goal): as async-channels, each
    // turn of this loop listed a wait on each on event-listener's `Mutex`, and took it again to drop it.
    let (started_tx, started_rx) = crate::memory::queue::places::<Started>(&crate::memory::owners::CHANNELS, 256);
    let (answer_tx, answer_rx) = crate::memory::queue::places::<(u64, Outcome)>(&crate::memory::owners::CHANNELS, REQUESTS_AT_ONCE);
    let mut running = 0usize;
    let now = now_ms();
    let cfg = ServerConfig { version, build: env!("CARGO_PKG_VERSION").to_owned(), fragment: send::fragment(send::CHUNK_MIN), ..ServerConfig::default() };
    let mut server = Server::new(cfg, stats.id, hub.live.born, now);
    let mut answers = Answers::new();
    let mut window = Window::new(WindowConfig::default());
    let mut rate = send::Rate::default();
    let mut chunk = send::CHUNK_MIN;
    let mut mark = 0u32;
    let mut sampled = 0u64;
    let mut since_ask = 0u64;
    let mut app_limited = false;
    // Each subscription's topic, and which subscriptions each topic feeds.
    let mut topic_of: HashMap<u32, Topic> = HashMap::new();
    let mut subs_of: HashMap<(u32, String), Vec<u32>> = HashMap::new();
    let (tx, mut rx) = socket.split();
    let mut tx = Some(tx);
    let mut sending: futures::future::Fuse<Sending> = futures::future::Fuse::terminated();
    let sender = crate::wire::unread::Sender::Person { user: caller.name.clone() };
    loop {
        let now = now_ms();
        // What the server half heard.
        while let Some(e) = server.poll_event() {
            match e {
                Event::Opened { .. } => {}
                Event::Request { id, call } => {
                    if running >= REQUESTS_AT_ONCE {
                        server.answer(id, Outcome::Nack { code: "busy".into(), why: "too many requests at once on this socket".into(), again: true }, None, Trust::Secret, &mut answers, now);
                        continue;
                    }
                    running = running.saturating_add(1);
                    let (hub, caller, reply) = (Arc::clone(&hub), caller.clone(), answer_tx.clone());
                    crate::rt::spawn(async move {
                        let outcome = request(hub, caller, call).await;
                        let _ = reply.send((id, outcome)).await;
                    });
                }
                Event::Subscribe { sub, topic, cursor } | Event::Resume { sub, topic, cursor } => {
                    let Some(spec) = crate::bp::spec(topic.kind) else {
                        server.start(sub, kiln_bp::conn::TopicSpec { kind: topic.kind, class: kiln_bp::conn::Class::State, prio: Prio::State, trust: Trust::Secret }, Start::Refuse { code: EndCode::Unknown, why: "no such topic".into() });
                        continue;
                    };
                    if !crate::bp::allowed(&topic, &scope) {
                        server.start(sub, spec, Start::Refuse { code: EndCode::Denied, why: "not yours to see".into() });
                        continue;
                    }
                    if let Some(old) = topic_of.insert(sub, topic.clone())
                        && let Some(list) = subs_of.get_mut(&(old.kind, old.key))
                    {
                        list.retain(|s| *s != sub);
                    }
                    subs_of.entry((topic.kind, topic.key.clone())).or_default().push(sub);
                    if !owner.ask(Ask::Start { sub, topic, cursor, reply: started_tx.clone() }) {
                        server.start(sub, spec, Start::Refuse { code: EndCode::Limit, why: "the server is busy: subscribe again".into() });
                    }
                }
                Event::Unsubscribed { sub } => {
                    if let Some(t) = topic_of.remove(&sub)
                        && let Some(list) = subs_of.get_mut(&(t.kind, t.key))
                    {
                        list.retain(|s| *s != sub);
                    }
                }
                Event::Acked { got, rtt, at } => {
                    window.on_ack(now, got, rtt, at, app_limited);
                    app_limited = false;
                }
                Event::Input { .. } | Event::Resize { .. } => {
                    // A terminal's keystrokes over the protocol are goal 9's; today's terminal socket takes them.
                }
                Event::Closed { .. } => {}
                ref refusal @ (Event::OpenRefused { .. } | Event::Unreadable { .. } | Event::Unread { .. }) => {
                    if let Some(u) = crate::bp::unread(refusal, sender.clone()) {
                        hub.note_unread(u, None).await;
                    }
                }
            }
        }
        // The path, as the kernel and the window measure it: the chunk and the mark sized from it.
        if now.saturating_sub(sampled) >= SAMPLE_MS {
            sampled = now;
            sample(control.as_deref(), &stats, &mut rate);
            let r = rate.get(window.rate());
            let c = send::chunk(r);
            let m = send::mark(r, c);
            if c != chunk {
                chunk = c;
                server.set_fragment(send::fragment(chunk));
                window.set_floor(2 * u64::try_from(chunk).unwrap_or(u64::MAX));
            }
            if m != mark {
                mark = m;
                if let Some(c) = control.as_deref() {
                    if let Some(s) = c.socket.as_ref() {
                        let _ = s.set_notsent_lowat(mark);
                    }
                    c.tls_limit.store(chunk, Ordering::Relaxed);
                }
            }
            stats.chunk.store(u64::try_from(chunk).unwrap_or(0), Ordering::Relaxed);
            stats.mark.store(u64::from(mark), Ordering::Relaxed);
            stats.rate.store(r.unwrap_or(0), Ordering::Relaxed);
            stats.min_rtt_ms.store(window.min_rtt().unwrap_or(0), Ordering::Relaxed);
            for (q, n) in stats.queued.iter().zip(server.queued_by_prio()) {
                q.store(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
            }
        }
        // The next message, when the socket can take one.
        if let Some(mut t) = tx.take() {
            let chunk64 = u64::try_from(chunk).unwrap_or(u64::MAX);
            if window.stalled(chunk64, server.acks_out()) {
                server.ask_ack();
            }
            // The window's room, and a whole chunk while nothing is in flight (but while the round trip is
            // probed, when the probe goes alone in a control frame's room).
            let probing = matches!(window.phase(), kiln_bp::window::Phase::ProbeRtt { .. });
            let room_w = if window.in_flight() == 0 && !probing { send::room(chunk) } else { usize::try_from(window.room()).unwrap_or(usize::MAX) };
            let room = send::room(chunk).min(room_w).max(CONTROL_ROOM);
            match server.poll_transmit(room, now) {
                Some(m) => {
                    let len = m.bytes.len();
                    let len64 = u64::try_from(len).unwrap_or(u64::MAX);
                    window.on_sent(len64);
                    since_ask = since_ask.saturating_add(len64);
                    if since_ask >= window.ack_every(chunk64) {
                        server.ask_ack();
                        since_ask = 0;
                    }
                    if len < room / 2 && room_w >= chunk {
                        app_limited = true;
                    }
                    let over = window.in_flight().saturating_sub(window.window().saturating_add(chunk64));
                    stats.over_window.fetch_max(over, Ordering::Relaxed);
                    stats.window.store(window.window(), Ordering::Relaxed);
                    stats.unacked.store(window.in_flight(), Ordering::Relaxed);
                    stats.held.store(len64, Ordering::Relaxed);
                    stats.held_max.fetch_max(len64, Ordering::Relaxed);
                    stats.sent.fetch_add(len64, Ordering::Relaxed);
                    if let Some(p) = m.frames.iter().map(|f| f.prio).min() {
                        let _ = hyper_rt::coop::set_class(class_of(p));
                    }
                    sending = (Box::pin(async move {
                        let r = t.send(Message::binary(m.bytes)).await;
                        (t, r)
                    }) as Sending)
                        .fuse();
                }
                None => {
                    if room_w >= chunk {
                        app_limited = true;
                    }
                    if server.is_closed() {
                        let _ = t.close(None).await;
                        return Ok(());
                    }
                    // Nothing to write: reading what comes is what's next, and it costs little.
                    let _ = hyper_rt::coop::set_class(crate::overload::Class::Critical);
                    tx = Some(t);
                }
            }
        }
        let wake = server.poll_timeout(now).min(sampled.saturating_add(SAMPLE_MS)).saturating_sub(now).clamp(1, 1_000);
        futures::select_biased! {
            (t, r) = sending => {
                r?;
                stats.held.store(0, Ordering::Relaxed);
                tx = Some(t);
            }
            m = rx.next().fuse() => match m {
                Some(Ok(Message::Binary(b))) => {
                    stats.received.fetch_add(u64::try_from(b.len()).unwrap_or(0), Ordering::Relaxed);
                    let mut redeem = Redeemer { live: &hub.live, who: &caller.name };
                    server.receive(&b, now_ms(), &mut redeem, &mut answers);
                }
                Some(Ok(Message::Text(_))) => {
                    // The protocol is binary: a text message is noted, as an unreadable frame is.
                    let u = crate::wire::unread::Unread { wire: crate::wire::unread::Wire::BpUp, from: sender.clone(), what: "a text message".into(), code: crate::wire::unread::Code::Malformed, why: "the protocol's messages are binary".into() };
                    hub.note_unread(u, None).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    server.close(CloseCode::Bye, "the page closed the socket", "");
                    return Ok(());
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
            a = answer_rx.recv().fuse() => {
                if let Ok((id, outcome)) = a {
                    running = running.saturating_sub(1);
                    server.answer(id, outcome, None, Trust::Secret, &mut answers, now_ms());
                }
            }
            s = started_rx.recv().fuse() => {
                if let Ok(Started { sub, topic, start }) = s
                    && topic_of.get(&sub) == Some(&topic)
                    && let Some(spec) = crate::bp::spec(topic.kind)
                {
                    server.start(sub, spec, start);
                }
            }
            c = changes.recv().fuse() => match c {
                Ok(c) => {
                    if let Some(list) = subs_of.get(&(c.topic.kind, c.topic.key.clone())) {
                        for sub in list {
                            server.push(*sub, &c.published);
                        }
                    }
                }
                Err(async_broadcast::RecvError::Overflowed(_)) => {
                    // Fell behind the broadcast: every subscription starts again from what it was sent.
                    for (sub, topic) in &topic_of {
                        let _ = owner.ask(Ask::Start { sub: *sub, topic: topic.clone(), cursor: None, reply: started_tx.clone() });
                    }
                }
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
            () = crate::rt::sleep(Duration::from_millis(wake)).fuse() => {}
        }
    }
}

/// What the kernel says of the socket, into its numbers and the rate.
fn sample(control: Option<&Control>, stats: &Stats, rate: &mut send::Rate) {
    let Some(info) = control.and_then(|c| c.socket.as_ref()).and_then(|s| s.tcp_info().ok().flatten()) else { return };
    if let Some(u) = info.unsent_bytes {
        stats.unsent.store(u64::from(u), Ordering::Relaxed);
    }
    if let Some(f) = info.in_flight_bytes {
        stats.in_flight.store(u64::from(f), Ordering::Relaxed);
    }
    if let Some(r) = info.retransmitted_bytes {
        stats.retransmitted.store(r, Ordering::Relaxed);
    }
    if let Some(d) = info.delivery_rate {
        rate.kernel(d, info.delivery_rate_app_limited.unwrap_or(false));
    }
}

/// A request's answer: an HTTP read run as the caller, through the API's own routes and checks
/// (`crate::api::live_read`); writes and commands over the protocol are goal 9's.
async fn request(hub: Arc<Hub>, caller: crate::authz::Caller, call: Call) -> Outcome {
    match call {
        Call::Http { method: Method::Get, path, .. } => {
            let (status, body) = Box::pin(crate::api::live_read(hub, caller, &path, ANSWER_MOST)).await;
            Outcome::Reply { status, body: kiln_bp::Bytes(body) }
        }
        Call::Http { .. } => Outcome::Nack { code: "unsupported".into(), why: "writes over the protocol come with goal 9: use the API".into(), again: false },
        Call::Command { .. } => Outcome::Nack { code: "unsupported".into(), why: "an agent's commands over the protocol come with goal 9: use its session's socket".into(), again: false },
    }
}
