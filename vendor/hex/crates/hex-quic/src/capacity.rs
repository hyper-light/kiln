//! The machine's capacity view over QUIC (joint plan step 13, `docs/design/capacity-view.md` CV9): a holder opens a
//! connection to the keeper's port ([`CAPACITY_PORT`]) and says each of its [`ToKeeper`]s on it; the keeper answers
//! each there, and says there each hint owed to it, every message framed as [`hex_core::capacity`] lays it out. A
//! connection is a link: once it ends, its holder is away and keeps what it holds (CV5). [`Server`] serves a keeper's
//! [`CapacityView`] on the port's acceptor to the holders its host authorizes alone, each speaking only as the name its
//! certificate carries (CV8, CV9); [`Link`] is a holder's end.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hex_api::capacity::{CapacityView, Close, ToHolder, ToKeeper};
use hex_core::capacity as codec;
use hex_core::lanes::DATA_BOUND;
use hex_core::stream::Deframer;

pub use hex_core::lanes::CAPACITY_PORT;

use crate::{Acceptor, Conn, Endpoint, PeerId, Refused, Why};

fn os(op: &'static str) -> impl Fn(io::Error) -> Refused {
    move |e| match e.kind() {
        io::ErrorKind::TimedOut => Refused::Deadline { what: op },
        _ => Refused::Os {
            op,
            why: e.to_string(),
        },
    }
}

/// A connection's writes, one whole frame at a time: its own answers, and the hints other links' messages owe it.
struct Writer {
    conn: Conn,
    frame: Mutex<()>,
}

impl Writer {
    fn say(&self, msg: &ToHolder) -> Result<(), Refused> {
        let bytes = codec::encode(msg).map_err(Refused::Message)?;
        let _whole = self.frame.lock().map_err(|_| Refused::Ended)?;
        self.conn
            .write_all(&bytes, None)
            .map_err(os("saying a capacity message"))
    }
}

/// Where the lane says what it refused, a line at a time (`hex.capacity event=...`): its host's log.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// A keeper served on its port (module docs): its view, behind the lock each link's messages take in turn, each
/// link's connection, so a hint reaches the link it is owed to, and the holders it serves.
pub struct Server<V> {
    view: Mutex<V>,
    links: Mutex<BTreeMap<u64, Arc<Writer>>>,
    next: AtomicU64,
    /// The holders its host authorizes, by the peer id each one's certificate names (`peer_id` of the name kiln's CA
    /// enrolled it as), and that name: a link speaks only as its own, and any other peer's words are refused.
    holders: BTreeMap<PeerId, String>,
    log: Log,
}

impl<V: CapacityView + Send + 'static> Server<V> {
    /// `view` served to `holders` alone, each the name kiln's CA enrolled it as; `log` hears each refusal.
    pub fn new(view: V, holders: &[String], log: Log) -> Arc<Self> {
        Arc::new(Self {
            view: Mutex::new(view),
            links: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(0),
            holders: holders
                .iter()
                .map(|name| (crate::peer_id(name), name.clone()))
                .collect(),
            log,
        })
    }

    /// The keeper's view, for its host: its own admissions changed, its room measured anew, a look.
    pub fn view(&self) -> &Mutex<V> {
        &self.view
    }

    /// Serves the port's connections until the endpoint ends, a thread a connection (the endpoint bounds how many
    /// it accepts, its `Shape`), each a link of its own: why it ended.
    pub fn serve(self: &Arc<Self>, acceptor: Acceptor) -> Refused {
        loop {
            let (peer, conn) = match acceptor.accept(None) {
                Ok(accepted) => accepted,
                Err(why) => return why,
            };
            let server = Arc::clone(self);
            let started = std::thread::Builder::new()
                .name("hex-capacity-link".into())
                .spawn(move || server.link(peer, conn));
            if let Err(e) = started {
                return Refused::Os {
                    op: "starting a capacity link's thread",
                    why: e.to_string(),
                };
            }
        }
    }

    /// Says every hint owed, each on the link it is owed to: after each message a link said, and after a change
    /// its host made.
    pub fn hint(&self) -> Result<(), Refused> {
        let hints = self
            .view
            .lock()
            .map_err(|_| Refused::Ended)?
            .hints()
            .map_err(Refused::Node)?;
        for (link, hint) in hints {
            let writer = self
                .links
                .lock()
                .map_err(|_| Refused::Ended)?
                .get(&link)
                .cloned();
            // A link gone since is away: its next hello's sync tells it the view.
            if let Some(writer) = writer {
                let _ = writer.say(&hint);
            }
        }
        Ok(())
    }

    fn link(&self, peer: PeerId, conn: Conn) {
        let Ok(id) = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        else {
            conn.reset(Why::Conns);
            return;
        };
        let writer = Arc::new(Writer {
            conn,
            frame: Mutex::new(()),
        });
        match self.links.lock() {
            Ok(mut links) => {
                links.insert(id, Arc::clone(&writer));
            }
            Err(_) => {
                writer.conn.reset(Why::Gone);
                return;
            }
        }
        let broke = self.read(id, peer, &writer);
        if let Ok(mut links) = self.links.lock() {
            links.remove(&id);
        }
        if let Ok(mut view) = self.view.lock() {
            let _ = view.link_down(id);
        }
        if let Some(why) = broke {
            writer.conn.reset(why);
        }
    }

    /// A word from `peer` the keeper doesn't hear: anything from a peer its host doesn't authorize, or a hello naming
    /// another holder than the one `peer`'s certificate names. Answered closed as not authorized.
    fn refused(&self, peer: PeerId, msg: &ToKeeper) -> Option<ToHolder> {
        let refused = match (self.holders.get(&peer), msg) {
            (None, _) => true,
            (Some(name), ToKeeper::Hello { holder, .. }) => holder != name,
            (Some(_), _) => false,
        };
        refused.then(|| ToHolder::Closed {
            rejected: msg.fence(),
            why: Close::NotAuthorized,
        })
    }

    /// Reads link `id`'s messages and answers each, until its holder ends it (none) or breaks it (why). A word it may
    /// not say is answered closed, typed, and logged once a link.
    fn read(&self, id: u64, peer: PeerId, writer: &Writer) -> Option<Why> {
        let mut deframer = codec::deframer();
        let mut buf = vec![0u8; DATA_BOUND];
        let mut logged = false;
        loop {
            let n = match writer.conn.read(&mut buf, None) {
                Ok(0) | Err(_) => return None,
                Ok(n) => n,
            };
            let mut input = buf.get(..n).unwrap_or_default();
            while !input.is_empty() {
                let Ok((taken, message)) = deframer.take(input) else {
                    return Some(Why::Protocol);
                };
                input = input.get(taken..).unwrap_or_default();
                let Some(bytes) = message else {
                    continue;
                };
                let Ok(msg) = codec::decode::<ToKeeper>(&bytes) else {
                    return Some(Why::Protocol);
                };
                if let Some(closed) = self.refused(peer, &msg) {
                    if !logged {
                        logged = true;
                        let said = match &msg {
                            ToKeeper::Hello { holder, .. } => holder.as_str(),
                            _ => "",
                        };
                        (self.log)(&format!(
                            "hex.capacity event=refused peer={peer:?} link={id} holder={said:?} why=not-authorized"
                        ));
                    }
                    if writer.say(&closed).is_err() {
                        return None;
                    }
                    continue;
                }
                let answer = match self.view.lock() {
                    Ok(mut view) => view.hear(id, msg),
                    Err(_) => return Some(Why::Gone),
                };
                // The keeper stopped: its host boots it again, and every link with it.
                let Ok(answer) = answer else {
                    return Some(Why::Gone);
                };
                if writer.say(&answer).is_err() {
                    return None;
                }
                if self.hint().is_err() {
                    return Some(Why::Gone);
                }
            }
        }
    }
}

/// A holder's end of its link to a keeper: the connection to the keeper's port, and what has come on it.
pub struct Link {
    conn: Conn,
    deframer: Deframer,
    buf: Vec<u8>,
    /// Bytes read past the messages taken, at most one read's.
    pending: Vec<u8>,
}

impl Link {
    /// Opens a link to the keeper at `peer`.
    pub fn open(endpoint: &Endpoint, peer: PeerId) -> Result<Self, Refused> {
        Ok(Self {
            conn: endpoint.open(peer, CAPACITY_PORT)?,
            deframer: codec::deframer(),
            buf: vec![0u8; DATA_BOUND],
            pending: Vec::new(),
        })
    }

    /// Says `msg` to the keeper, waiting for room until `deadline`.
    pub fn say(&self, msg: &ToKeeper, deadline: Option<Instant>) -> Result<(), Refused> {
        let bytes = codec::encode(msg).map_err(Refused::Message)?;
        self.conn
            .write_all(&bytes, deadline)
            .map_err(os("saying a capacity message"))
    }

    /// The keeper's next word, an answer or a hint, waiting for it until `deadline`; [`Refused::Closed`] once the
    /// keeper ended the link.
    pub fn hear(&mut self, deadline: Option<Instant>) -> Result<ToHolder, Refused> {
        loop {
            while !self.pending.is_empty() {
                let (taken, message) = self
                    .deframer
                    .take(&self.pending)
                    .map_err(Refused::Message)?;
                let rest = self.pending.split_off(taken.min(self.pending.len()));
                self.pending = rest;
                if let Some(bytes) = message {
                    return codec::decode(&bytes).map_err(Refused::Message);
                }
            }
            let n = self
                .conn
                .read(&mut self.buf, deadline)
                .map_err(os("hearing a capacity message"))?;
            if n == 0 {
                return Err(Refused::Closed { refusal: None });
            }
            self.pending
                .extend_from_slice(self.buf.get(..n).unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests;
