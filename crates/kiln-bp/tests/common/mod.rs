//! A loopback for the connection core's tests: a server's half and a client's half joined by a
//! virtual link (a rate, a one-way delay, and an ordered queue), the server's host (tickets,
//! answers, topics) played by the test, and time as a number of milliseconds. No sockets.

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};

use kiln_bp::answers::Answers;
use kiln_bp::client::{Client, ClientConfig, ClientEvent};
use kiln_bp::conn::{Class, Prio, TopicSpec, Trust};
use kiln_bp::frames::*;
use kiln_bp::server::{Event, Server, ServerConfig};
use kiln_bp::tickets::{Principal, Tickets};
use kiln_bp::topic::TopicLog;

pub const BORN: u64 = 1_793_825_000_000;

/// The topic kinds the tests use.
pub fn spec(kind: u32) -> TopicSpec {
    match kind {
        1 => TopicSpec { kind, class: Class::State, prio: Prio::State, trust: Trust::Records },
        2 => TopicSpec { kind, class: Class::Telemetry, prio: Prio::State, trust: Trust::Records },
        3 => TopicSpec { kind, class: Class::Stream, prio: Prio::Stream, trust: Trust::Text },
        4 => TopicSpec { kind, class: Class::Stream, prio: Prio::Interactive, trust: Trust::Text },
        _ => TopicSpec { kind, class: Class::Stream, prio: Prio::Bulk, trust: Trust::Text },
    }
}

/// The server's side: its tickets, answers and topics, and who's subscribed to what.
#[derive(Default)]
pub struct Host {
    pub tickets: Tickets,
    pub answers: Answers,
    pub topics: BTreeMap<(u32, String), TopicLog>,
    /// (connection, sub) for each topic.
    pub subs: BTreeMap<(u32, String), Vec<(u64, u32)>>,
    pub requests: Vec<(u64, u64, Call)>,
    pub events: Vec<Event>,
    /// Answer requests at once, `Taken`.
    pub auto_answer: bool,
    /// Topic kinds the caller may not see.
    pub denied: Vec<u32>,
}

impl Host {
    pub fn new() -> Self {
        Self { tickets: Tickets::new(), auto_answer: true, ..Default::default() }
    }

    pub fn ticket(&mut self, who: &str, n: u8, now: u64) -> Vec<u8> {
        self.tickets.mint([n; 32], Principal { who: who.into(), session: format!("s-{who}"), expires_at: None }, now)
    }

    pub fn topic(&mut self, kind: u32, key: &str) -> &mut TopicLog {
        self.topics.entry((kind, key.to_owned())).or_insert_with(|| TopicLog::new(spec(kind), BORN))
    }

    /// Takes what a server's connection heard.
    pub fn serve(&mut self, s: &mut Server, now: u64) {
        while let Some(e) = s.poll_event() {
            match &e {
                Event::Subscribe { sub, topic, cursor } | Event::Resume { sub, topic, cursor } => {
                    let key = (topic.kind, topic.key.clone());
                    if self.denied.contains(&topic.kind) {
                        s.start(*sub, spec(topic.kind), kiln_bp::topic::Start::Refuse { code: EndCode::Denied, why: "not yours".into() });
                    } else {
                        let log = self.topics.entry(key.clone()).or_insert_with(|| TopicLog::new(spec(topic.kind), BORN));
                        s.start(*sub, spec(topic.kind), log.start(cursor.as_ref()));
                        let list = self.subs.entry(key).or_default();
                        if !list.contains(&(s.id(), *sub)) {
                            list.push((s.id(), *sub));
                        }
                    }
                }
                Event::Unsubscribed { sub } => {
                    for l in self.subs.values_mut() {
                        l.retain(|x| *x != (s.id(), *sub));
                    }
                }
                Event::Request { id, call } => {
                    self.requests.push((s.id(), *id, call.clone()));
                    if self.auto_answer {
                        s.answer(*id, Outcome::Taken, None, Trust::Secret, &mut self.answers, now);
                    }
                }
                _ => {}
            }
            self.events.push(e);
        }
    }

    /// Publishes a change to a topic, pushing it to each connection subscribed.
    pub fn publish(&mut self, kind: u32, key: &str, d: Delta, conns: &mut [&mut Server]) -> u64 {
        let k = (kind, key.to_owned());
        let log = self.topics.entry(k.clone()).or_insert_with(|| TopicLog::new(spec(kind), BORN));
        let Ok(p) = log.publish(d) else { return 0 };
        for (conn, sub) in self.subs.get(&k).cloned().unwrap_or_default() {
            for s in conns.iter_mut() {
                if s.id() == conn {
                    s.push(sub, &p);
                }
            }
        }
        p.rev
    }
}

/// One way of a virtual link: a rate (bytes a millisecond, 0 for none), a one-way delay, and
/// the messages in flight.
#[derive(Debug, Default)]
pub struct Link {
    pub bytes_per_ms: f64,
    pub delay: u64,
    /// When the link is free to start the next message.
    pub free_at: f64,
    pub flight: VecDeque<(u64, Vec<u8>)>,
    pub sent: u64,
    /// Drop what's sent (a cut link).
    pub cut: bool,
}

impl Link {
    pub fn new(bytes_per_ms: f64, delay: u64) -> Self {
        Self { bytes_per_ms, delay, ..Default::default() }
    }

    /// Whether it can start a message at `now`.
    pub fn idle(&self, now: u64) -> bool {
        self.free_at <= now as f64
    }

    pub fn send(&mut self, now: u64, msg: Vec<u8>) {
        self.sent += msg.len() as u64;
        if self.cut {
            return;
        }
        let start = self.free_at.max(now as f64);
        let took = if self.bytes_per_ms > 0.0 { msg.len() as f64 / self.bytes_per_ms } else { 0.0 };
        self.free_at = start + took;
        let arrive = (self.free_at + self.delay as f64).ceil() as u64;
        self.flight.push_back((arrive, msg));
    }

    pub fn due(&mut self, now: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.flight.front().is_some_and(|(at, _)| *at <= now) {
            if let Some((_, m)) = self.flight.pop_front() {
                out.push(m);
            }
        }
        out
    }
}

/// A server connection and a client joined by a link.
pub struct Pair {
    pub server: Server,
    pub client: Client,
    pub up: Link,
    pub down: Link,
    pub client_events: Vec<ClientEvent>,
    /// The room a transmit asks for.
    pub room: usize,
}

impl Pair {
    pub fn open(_host: &mut Host, id: u64, scfg: ServerConfig, ccfg: ClientConfig, ticket: &[u8], now: u64) -> Self {
        Self { server: Server::new(scfg, id, BORN, now), client: Client::new(ccfg, ticket, now), up: Link::new(0.0, 0), down: Link::new(0.0, 0), client_events: Vec::new(), room: 64 * 1024 }
    }

    /// Moves bytes both ways at `now`, until nothing more moves now.
    pub fn step(&mut self, host: &mut Host, now: u64) {
        for _ in 0..10_000 {
            let mut moved = false;
            for m in self.up.due(now) {
                self.server.receive(&m, now, &mut host.tickets, &mut host.answers);
                moved = true;
            }
            host.serve(&mut self.server, now);
            for m in self.down.due(now) {
                self.client.receive(&m, now);
                moved = true;
            }
            while let Some(e) = self.client.poll_event() {
                self.client_events.push(e);
            }
            if self.up.idle(now)
                && let Some(t) = self.client.poll_transmit(self.room, now)
            {
                self.up.send(now, t.bytes);
                moved = true;
            }
            if self.down.idle(now)
                && let Some(t) = self.server.poll_transmit(self.room, now)
            {
                self.down.send(now, t.bytes);
                moved = true;
            }
            if !moved {
                return;
            }
        }
    }

    /// Runs from `from` to `to`, a millisecond at a time.
    pub fn run(&mut self, host: &mut Host, from: u64, to: u64) {
        for t in from..=to {
            self.step(host, t);
        }
    }

    pub fn take_events(&mut self) -> Vec<ClientEvent> {
        std::mem::take(&mut self.client_events)
    }
}
