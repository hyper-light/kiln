//! The machine's capacity view over QUIC (joint plan step 13, `docs/design/capacity-view.md` CV9): a holder opens a
//! connection to the keeper's port ([`CAPACITY_PORT`]) and says each of its [`ToKeeper`]s on it; the keeper answers
//! each there, and says there each hint owed to it, every message framed as [`hex_core::capacity`] lays it out. A
//! connection is a link: once it ends, its holder is away and keeps what it holds (CV5). [`Server`] serves a keeper's
//! [`CapacityView`] on the port's acceptor to the holders its host authorizes alone, each speaking only as the name its
//! certificate carries (CV8, CV9); [`Link`] is a holder's end.
//!
//! A link's words are said by a writer thread of its own, in the order the keeper decided them, and the hint owed to a
//! link waits there coalesced until the connection drains (CV7: backpressure, no timer): no host and no other link ever
//! writes on it, so a holder that stops reading stalls its own link alone (the review's E-1).

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use hex_api::capacity::{CapacityView, Close, ToHolder, ToKeeper};
use hex_core::capacity as codec;
use hex_core::lanes::DATA_BOUND;
use hex_core::patience::{Patience, Piece};
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

/// A message's wait for room on its link, its holder reading (decision 9): within the slowest seen; past it, its
/// run-out told once and the holder followed by its link's life, the write ending only with the link. The time it took,
/// however long, is the sample W* rises to.
static SAYS: Patience = Patience::measuring("capacity.say");

/// A link's writes, each a whole message said by the link's own writer thread in the order the keeper decided them
/// (CV7; the review's E-1): the answers of its reader, which waits for each to go before it reads on, and the hint owed
/// to it, which waits coalesced while unsent and goes once the connection drains. No host and no other link writes on
/// it, so a holder that stops reading stalls its own link alone.
struct Writer {
    conn: Conn,
    /// Its holder, for a run-out's told line: the name its certificate carries where the host authorizes it.
    holder: Option<String>,
    /// The link's number, for a run-out's told line.
    link: u64,
    outbox: Mutex<Outbox>,
    moved: Condvar,
    /// Each message said whole on the link: the thread that said it, whether it was a hint, and its bytes (a test's
    /// look at who writes and how much, the review's E-1).
    #[cfg(test)]
    said_by: Mutex<Vec<(std::thread::ThreadId, bool, usize)>>,
}

/// What a link's writer has to say, in the order the keeper decided it (R04): at most the reader's one answer, then
/// the hint owed, the newest view (CV8: one hint a holder). A hint decided before an answer is that answer's to carry,
/// the answer carrying the view, so the unsent hint goes with it; one decided after waits behind it.
#[derive(Default)]
struct Outbox {
    answer: Option<ToHolder>,
    hint: Option<ToHolder>,
    /// The answers said.
    answered: u64,
    /// The writer is done: its reader ended the link, or a write failed (why).
    ended: Option<Option<Refused>>,
}

impl Writer {
    fn outbox(&self) -> Result<MutexGuard<'_, Outbox>, Refused> {
        self.outbox.lock().map_err(|_| Refused::Ended)
    }

    /// The reader's answer, decided under the view's lock, after which it waits for it to go ([`Writer::answered`]):
    /// its count among the answers said, once it has.
    fn answer(&self, answer: ToHolder) -> Result<u64, Refused> {
        let mut outbox = self.outbox()?;
        if let Some(why) = &outbox.ended {
            return Err(why.clone().unwrap_or(Refused::Ended));
        }
        outbox.hint = None;
        outbox.answer = Some(answer);
        let count = outbox.answered.checked_add(1).ok_or(Refused::Ended)?;
        drop(outbox);
        self.moved.notify_all();
        Ok(count)
    }

    /// The hint owed, decided under the view's lock: it replaces an unsent one, the newest view (CV7).
    fn hint(&self, hint: ToHolder) -> Result<(), Refused> {
        let mut outbox = self.outbox()?;
        if outbox.ended.is_some() {
            return Ok(());
        }
        outbox.hint = Some(hint);
        drop(outbox);
        self.moved.notify_all();
        Ok(())
    }

    /// Waits for the reader's answer `count` to be said; refused once the writer can say no more. Its wait is the
    /// writer's, which follows the holder by its link ([`SAYS`]).
    fn answered(&self, count: u64) -> Result<(), Refused> {
        let outbox = self.outbox()?;
        let outbox = self
            .moved
            .wait_while(outbox, |outbox| {
                outbox.answered < count && outbox.ended.is_none()
            })
            .map_err(|_| Refused::Ended)?;
        match &outbox.ended {
            Some(why) if outbox.answered < count => Err(why.clone().unwrap_or(Refused::Ended)),
            _ => Ok(()),
        }
    }

    /// The link's reader is done: its writer ends once its current message goes, or the link's reset cuts it.
    fn end(&self) {
        if let Ok(mut outbox) = self.outbox.lock()
            && outbox.ended.is_none()
        {
            outbox.ended = Some(None);
        }
        self.moved.notify_all();
    }

    /// The writer's thread: says each message as it is decided, an answer before the hint after it, until its reader
    /// ends the link or a write fails.
    fn write(&self) {
        loop {
            let (msg, answer) = {
                let Ok(outbox) = self.outbox() else {
                    return;
                };
                let Ok(mut outbox) = self.moved.wait_while(outbox, |outbox| {
                    outbox.ended.is_none() && outbox.answer.is_none() && outbox.hint.is_none()
                }) else {
                    return;
                };
                if outbox.ended.is_some() {
                    return;
                }
                match (outbox.answer.take(), outbox.hint.take()) {
                    (Some(answer), hint) => {
                        outbox.hint = hint;
                        (answer, true)
                    }
                    (None, Some(hint)) => (hint, false),
                    (None, None) => continue,
                }
            };
            let said = self.say(&msg);
            let Ok(mut outbox) = self.outbox() else {
                return;
            };
            match said {
                Ok(()) if answer => outbox.answered = outbox.answered.saturating_add(1),
                Ok(()) => {}
                Err(why) => {
                    outbox.ended = Some(Some(why));
                    drop(outbox);
                    self.moved.notify_all();
                    return;
                }
            }
            drop(outbox);
            self.moved.notify_all();
        }
    }

    /// Says `msg`, frame by frame, each placed whole or not at all, waiting for room as decision 9 waits ([`SAYS`]).
    fn say(&self, msg: &ToHolder) -> Result<(), Refused> {
        let bytes = codec::encode(msg).map_err(Refused::Message)?;
        let asked = Instant::now();
        let mut window = SAYS
            .deadline()
            .and_then(|w_star| asked.checked_add(w_star).map(|by| (w_star, by)));
        for frame in bytes.chunks(DATA_BOUND) {
            loop {
                match self.conn.write_all(frame, window.map(|(_, by)| by)) {
                    Ok(()) => break,
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                        if let Some((w_star, _)) = window.take() {
                            self.ran_out(w_star, asked);
                        }
                    }
                    Err(e) => return Err(os("saying a capacity message")(e)),
                }
            }
        }
        SAYS.answered(asked.elapsed());
        #[cfg(test)]
        if let Ok(mut said) = self.said_by.lock() {
            said.push((
                std::thread::current().id(),
                matches!(msg, ToHolder::Hint { .. }),
                bytes.len(),
            ));
        }
        Ok(())
    }

    /// Past W*, the run-out told once, naming the holder the write follows by its link from now on.
    fn ran_out(&self, w_star: std::time::Duration, asked: Instant) {
        let waited = asked.elapsed();
        match &self.holder {
            Some(name) => SAYS.ran_out(
                w_star,
                waited,
                &[
                    Piece::Kind("the holder "),
                    Piece::Name(name),
                    Piece::Kind(" reading its capacity link"),
                ],
            ),
            None => SAYS.ran_out(
                w_star,
                waited,
                &[
                    Piece::Kind("the peer of capacity link "),
                    Piece::Number(self.link),
                    Piece::Kind(" reading it"),
                ],
            ),
        }
    }
}

/// Where the lane says what it refused, a line at a time (`hex.capacity event=...`): its host's log.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// A holder refused, what the lane says of it: `hex.capacity event=refused peer=<n> link=<n> holder=<a JSON string>
/// why="not-authorized"`, the name it gave, if any, as it gave it.
fn refused_line(peer: PeerId, link: u64, holder: &str) -> String {
    use hex_api::event::Value::{Number, Text};
    hex_api::event::line(
        "hex.capacity",
        "refused",
        &[
            ("peer", Number(u128::from(peer))),
            ("link", Number(u128::from(link))),
            ("holder", Text(holder)),
            ("why", Text("not-authorized")),
        ],
    )
}

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

    /// Hands every hint owed to the link it is owed to, whose own writer says it once the link drains, never on this
    /// thread (CV7): after each message a link said, and after a change its host made. Never waits for a holder.
    pub fn hint(&self) -> Result<(), Refused> {
        let mut view = self.view.lock().map_err(|_| Refused::Ended)?;
        self.owed(&mut view)
    }

    /// The hints `view` owes, each handed to its link's writer under the view's lock, so each link says the views in
    /// the order they were decided.
    fn owed(&self, view: &mut V) -> Result<(), Refused> {
        let hints = view.hints().map_err(Refused::Node)?;
        let links = self.links.lock().map_err(|_| Refused::Ended)?;
        for (link, hint) in hints {
            // A link gone since is away: its next hello's sync tells it the view.
            if let Some(writer) = links.get(&link) {
                writer.hint(hint)?;
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
            holder: self.holders.get(&peer).cloned(),
            link: id,
            outbox: Mutex::new(Outbox::default()),
            moved: Condvar::new(),
            #[cfg(test)]
            said_by: Mutex::new(Vec::new()),
        });
        let writing = Arc::clone(&writer);
        let started = std::thread::Builder::new()
            .name("hex-capacity-say".into())
            .spawn(move || writing.write());
        if started.is_err() {
            writer.conn.reset(Why::Gone);
            return;
        }
        match self.links.lock() {
            Ok(mut links) => {
                links.insert(id, Arc::clone(&writer));
            }
            Err(_) => {
                writer.end();
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
        // Its writer ends with it: what it says once its reader is done is cut by the reset, which ends a write that
        // waits for room, so no writer outlives its link. Every answer went before its reader read on.
        writer.end();
        writer.conn.reset(broke.unwrap_or(Why::Gone));
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
                        (self.log)(&refused_line(peer, id, said));
                    }
                    let said = writer
                        .answer(closed)
                        .and_then(|count| writer.answered(count));
                    if said.is_err() {
                        return None;
                    }
                    continue;
                }
                // The answer, and the hints its change owes the other links, handed to the writers under the view's
                // lock, in the order decided, the others' even where this link's writer has ended; then this link
                // waits for its own answer to go before it reads on.
                let count = {
                    let Ok(mut view) = self.view.lock() else {
                        return Some(Why::Gone);
                    };
                    // The keeper stopped: its host boots it again, and every link with it.
                    let Ok(answer) = view.hear(id, msg) else {
                        return Some(Why::Gone);
                    };
                    let count = writer.answer(answer);
                    if self.owed(&mut view).is_err() {
                        return Some(Why::Gone);
                    }
                    count
                };
                let Ok(count) = count else {
                    return None;
                };
                if writer.answered(count).is_err() {
                    return None;
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
