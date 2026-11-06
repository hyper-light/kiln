//! The topics' owner (goal 3 of the browser protocol program, plan `01a1a832eb2f711f9c6f51d88effe89d`
//! step 2): one task per server that holds every topic's log (`kiln_bp::topic::TopicLog`) and goal 2's
//! adapters (`crate::bp::Feed`), fed what the hub emits. Each event is read and made into its topics'
//! changes once, whatever the number of sockets (the events socket reads each event once per socket),
//! and each change goes to every protocol socket at once as one shared `Arc` (a broadcast each socket
//! filters by the topics it holds). A socket asks the owner how a subscription starts (resumed from
//! its cursor, or a snapshot) by message, with no lock: the owner is the only one that touches the
//! logs (the lockless rule, `scripts/locks/hot.toml`).
//!
//! A change pushed after a subscription's start was computed is pushed to it again by the broadcast;
//! the server half drops what it already has by revision, so a start and the broadcast never lose or
//! repeat a change between them.

use std::collections::HashMap;
use std::sync::Arc;

use kiln_bp::frames::{Cursor, Topic};
use kiln_bp::topic::{Published, Start, TopicLog};

use crate::rt::Broadcast;
use crate::wire::events::ServerEvent;

/// A topic's change, as every socket shares it.
#[derive(Debug)]
pub struct Change {
    pub topic: Topic,
    pub published: Published,
}

/// What a socket asks the owner.
#[derive(Debug)]
pub enum Ask {
    /// How subscription `sub` of a socket starts, from `cursor`; answered on `reply`.
    Start { sub: u32, topic: Topic, cursor: Option<Cursor>, reply: async_channel::Sender<Started> },
}

/// A subscription's start, for its socket.
#[derive(Debug)]
pub struct Started {
    pub sub: u32,
    pub topic: Topic,
    pub start: Start,
}

/// Shape: the events waiting for the owner. Past it an event is dropped and counted (a broadcast
/// subscriber of the hub's falls behind the same way): the topics it would have changed are then
/// behind until their next change, as an events socket that overflowed is until it refetches.
const EVENTS_WAITING: usize = 4_096;

/// Shape: the asks waiting for the owner: a subscription's start each.
const ASKS_WAITING: usize = 4_096;

/// Shape: the changes a socket may fall behind the broadcast before it overflows and resumes every
/// subscription it has from its cursors.
const CHANGES_WAITING: usize = 8_192;

/// The owner's handle: where events and asks go, and the changes come from.
#[derive(Clone)]
pub struct Handle {
    events: async_channel::Sender<Arc<ServerEvent>>,
    asks: async_channel::Sender<Ask>,
    changes: Arc<Broadcast<Arc<Change>>>,
    born: u64,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").field("born", &self.born).finish_non_exhaustive()
    }
}

impl Handle {
    /// Starts the owner of topics of the server incarnation `born` (its clock's milliseconds).
    pub fn start(born: u64) -> Self {
        let (events, events_rx) = async_channel::bounded(EVENTS_WAITING);
        let (asks, asks_rx) = async_channel::bounded(ASKS_WAITING);
        let changes = Arc::new(Broadcast::new(CHANGES_WAITING));
        let out = Arc::clone(&changes);
        crate::rt::spawn(async move { run(born, events_rx, asks_rx, &out).await });
        Self { events, asks, changes, born }
    }

    pub fn born(&self) -> u64 {
        self.born
    }

    /// One of the hub's events, for the topics: never waits (past the bound it's dropped and counted).
    pub fn event(&self, e: &ServerEvent) {
        if self.events.try_send(Arc::new(e.clone())).is_err() {
            super::metrics().events_dropped.inc();
        }
    }

    /// Asks how a subscription starts; false when the owner can take no more asks now.
    pub fn ask(&self, a: Ask) -> bool {
        self.asks.try_send(a).is_ok()
    }

    /// The changes from now on.
    pub fn changes(&self) -> crate::rt::Subscription<Arc<Change>> {
        self.changes.subscribe()
    }
}

/// The owner's loop: events made into changes, asks answered, until both channels close.
async fn run(born: u64, events: async_channel::Receiver<Arc<ServerEvent>>, asks: async_channel::Receiver<Ask>, out: &Broadcast<Arc<Change>>) {
    use futures::FutureExt as _;
    let mut topics = Topics::new(born);
    loop {
        futures::select_biased! {
            a = asks.recv().fuse() => match a {
                Ok(Ask::Start { sub, topic, cursor, reply }) => {
                    let start = topics.start(&topic, cursor.as_ref());
                    let _ = reply.try_send(Started { sub, topic, start });
                }
                Err(_) => return,
            },
            e = events.recv().fuse() => match e {
                Ok(e) => {
                    for c in topics.event(&e) {
                        out.send(Arc::new(c));
                    }
                }
                Err(_) => return,
            },
        }
    }
}

/// Every topic the server has: their logs, and goal 2's adapters' memory. Pure: what changes comes
/// from the events given.
#[derive(Debug)]
pub struct Topics {
    born: u64,
    feed: crate::bp::Feed,
    logs: HashMap<(u32, String), TopicLog>,
}

impl Topics {
    pub fn new(born: u64) -> Self {
        Self { born, feed: crate::bp::Feed::new(), logs: HashMap::new() }
    }

    fn log(&mut self, topic: &Topic) -> Option<&mut TopicLog> {
        let spec = crate::bp::spec(topic.kind)?;
        let born = self.born;
        Some(self.logs.entry((topic.kind, topic.key.clone())).or_insert_with(|| TopicLog::new(spec, born)))
    }

    /// The changes an event makes, each applied to its topic's log.
    pub fn event(&mut self, e: &ServerEvent) -> Vec<Change> {
        // A run's repository is learned from its own events (`Feed`), never read from the store here.
        let outs = self.feed.event(e, &|_| None);
        let mut changes = Vec::with_capacity(outs.len());
        for o in outs {
            let Some(log) = self.log(&o.topic) else { continue };
            match log.publish(o.delta) {
                Ok(published) => changes.push(Change { topic: o.topic, published }),
                Err(m) => tracing::debug!(?m, kind = o.topic.kind, "a topic's change didn't fit its state"),
            }
        }
        changes
    }

    /// How a subscription to `topic` from `cursor` starts.
    pub fn start(&mut self, topic: &Topic, cursor: Option<&Cursor>) -> Start {
        match self.log(topic) {
            Some(log) => log.start(cursor),
            None => Start::Refuse { code: kiln_bp::frames::EndCode::Unknown, why: format!("no topic of kind {}", topic.kind) },
        }
    }
}
