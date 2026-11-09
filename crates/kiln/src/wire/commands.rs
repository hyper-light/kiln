//! Commands from the server to a runner process (docs/wire.md, "Commands"). Each is an item of
//! one of that process's command streams: its own two, or, to a process that takes them
//! (`CAP_SESSION_STREAMS`; runner-fan), an agent session's own two for what's for that session
//! (`session_of`), which the loop on the shard that owns the session takes, in the session's
//! order, waiting for no other session's. The process answers it once it applied it: an
//! ACK, or a NACK saying why not. It goes again under its number until then, and the process
//! takes it once however often it comes.
//!
//! - **Parked** commands (a leg to cancel, a session to close) wait while their runner is away.
//!   They're kept, and sent again after a reconnect from where the runner has them.
//! - **Live** commands (keystrokes, a terminal to open, a leg to run) are refused at once while
//!   their runner is away (its link down). A runner that has just connected and not yet said what
//!   it has isn't away: a live command sent then goes once it has. If the link drops before one is
//!   answered, it ends unanswered (unreachable if it never went): sent again later, it would
//!   surprise. Within a live link, a live command goes again like any item.
//!
//! Each stream's name carries the server's incarnation and a generation (`p:<born>.<gen>`,
//! `l:<born>.<gen>`; a session's own, `p:<born>.<gen>/<session>`). A runner process that started again has neither stream. Its parked commands
//! not yet answered go again on a new generation, and its live ones end unanswered. On the
//! runner, a stream's newer generation, or a server that started again, lets go of those before
//! it, and an item of one let go of that comes late is dropped ([`Current`]).
//!
//! Sans-io: the hub pushes commands, hands in the runner's frames and its link's comings and
//! goings, and sends what [`Commands::poll`] gives. Each command's answer comes back on the
//! receiver [`Commands::push`] gave for it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use futures::channel::oneshot;

use super::channel::{Event, Frame, Outcome, Refusal, Seq, Sender};

pub type PeerId = u64;

/// What a runner process keeps unanswered of its commands, at the most: past it, a command is
/// refused here rather than kept.
pub const ROOM: usize = 4 << 20;
/// How long a command sent on a live link waits for its answer before it goes again.
pub const PATIENCE: Duration = Duration::from_secs(3);
/// The most a command carries: the frame its item rides (`ServerMsg::Wire`, on the control lane) is
/// at most 1 MiB (the transport's bound for its class), and the item's head takes some of that. One
/// larger would never go, and every command after it would wait behind it: it's answered at once,
/// and counted as a frame the server let go (`refused`).
pub const ITEM_MOST: usize = (1 << 20) - 1024;
/// How long a parked command waits for its runner to come back before it ends unreachable.
pub const PARK_FOR: Duration = Duration::from_secs(24 * 3600);

/// How a command ended, for whoever sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Its runner applied it.
    Taken,
    /// Its runner refused it, saying how and why.
    Refused { code: Refusal, why: String },
    /// It never reached its runner: the runner was away when it was sent (a live command), or
    /// is gone for good.
    Unreachable { why: String },
    /// Its runner may or may not have applied it: the link dropped before the runner answered (a
    /// live command), or no answer came in time.
    Unanswered { why: String },
    /// Sent to a runner from before the channel, which can't say what became of it.
    Sent,
    /// Its runner is away: it goes once the runner is back (a parked command, for a caller that
    /// doesn't wait for that).
    Parked,
}

impl Answer {
    /// The runner applied it.
    pub fn taken(&self) -> bool {
        matches!(self, Self::Taken)
    }

    /// What an HTTP caller is told when its runner didn't take it: None when it did (or can't
    /// say, a runner from before the channel).
    pub fn failure(&self, runner: &str) -> Option<(http::StatusCode, String)> {
        use http::StatusCode;
        match self {
            Self::Taken | Self::Sent | Self::Parked => None,
            Self::Refused { code: Refusal::Unknown, why } => Some((StatusCode::NOT_FOUND, format!("{runner} doesn't have it: {why}"))),
            Self::Refused { code: Refusal::StaleEpoch, why } => Some((StatusCode::CONFLICT, format!("{runner} let go of it: {why}"))),
            Self::Refused { why, .. } => Some((StatusCode::CONFLICT, format!("{runner} refused it: {why}"))),
            Self::Unreachable { why } => Some((StatusCode::SERVICE_UNAVAILABLE, format!("{runner} can't be reached: {why}"))),
            Self::Unanswered { why } => Some((StatusCode::GATEWAY_TIMEOUT, format!("{runner} didn't say whether it did it: {why}"))),
        }
    }

    /// In words, for a log or an error.
    pub fn words(&self) -> String {
        match self {
            Self::Taken => "taken".to_owned(),
            Self::Refused { code, why } => format!("refused ({code:?}): {why}"),
            Self::Unreachable { why } => format!("unreachable: {why}"),
            Self::Unanswered { why } => format!("unanswered: {why}"),
            Self::Sent => "sent to a runner that can't answer".to_owned(),
            Self::Parked => "parked until its runner is back".to_owned(),
        }
    }
}

/// Which of its command streams a command rides, by how it waits while its runner is away: of its runner process's own, or
/// of its agent session's own (a runner announcing `CAP_SESSION_STREAMS`; runner-fan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Parked,
    Live,
}

struct Waiting {
    answer: oneshot::Sender<Answer>,
    body: Vec<u8>,
    at: Instant,
}

struct Stream {
    prefix: &'static str,
    /// The agent session it's one of the own streams of (runner-fan): none for its runner process's own.
    session: Option<String>,
    generation: u64,
    waiting: BTreeMap<Seq, Waiting>,
}

impl Stream {
    fn new(prefix: &'static str, session: Option<&str>) -> Self {
        Self { prefix, session: session.map(str::to_owned), generation: 1, waiting: BTreeMap::new() }
    }

    fn name(&self, born: u64) -> String {
        match &self.session {
            Some(session) => format!("{}:{born}.{}/{session}", self.prefix, self.generation),
            None => format!("{}:{born}.{}", self.prefix, self.generation),
        }
    }
}

/// A runner process's command streams of one owner, one for each way a command waits: its own, or an agent session's.
struct Pair {
    parked: Stream,
    live: Stream,
}

impl Pair {
    fn new(session: Option<&str>) -> Self {
        Self { parked: Stream::new("p", session), live: Stream::new("l", session) }
    }

    fn of(&mut self, kind: Kind) -> &mut Stream {
        match kind {
            Kind::Parked => &mut self.parked,
            Kind::Live => &mut self.live,
        }
    }

    /// Its stream named `name` (under incarnation `born`), and its kind.
    fn named(&mut self, name: &str, born: u64) -> Option<(Kind, &mut Stream)> {
        if self.parked.name(born) == name {
            Some((Kind::Parked, &mut self.parked))
        } else if self.live.name(born) == name {
            Some((Kind::Live, &mut self.live))
        } else {
            None
        }
    }

    fn into_waiting(self) -> impl Iterator<Item = Waiting> {
        self.parked.waiting.into_values().chain(self.live.waiting.into_values())
    }
}

struct Peer {
    tx: Sender,
    /// Its own streams: every command it takes, but an agent session's where it takes those on the session's own.
    own: Pair,
    /// Each agent session's own streams, from the session's first command there (runner-fan): to a runner process that
    /// takes them (`lanes`), so the runner takes each session's commands on the loop of the shard that owns the session,
    /// in their order, and no session's waits for another's.
    sessions: HashMap<String, Pair>,
    /// It takes an agent session's commands on the session's own streams (`CAP_SESSION_STREAMS`).
    lanes: bool,
    /// The link is up: the runner process is connected, and may not have said what it has yet.
    link: bool,
    /// The link is up and the runner has said what it has.
    up: bool,
    /// It said it keeps the channel (`CAP_WIRE`), on this link or one before.
    wire: bool,
    /// It connected and hasn't said yet whether it keeps the channel, since then: its commands
    /// wait for that, a live one too ([`Commands::coming`]).
    coming: Option<Instant>,
}

/// How long a runner process that connected has, at the most, to say whether it keeps the channel
/// (its `Caps`, which follow its `Build` at once) when it says nothing else meanwhile: one that
/// says something else first is from before the channel at once ([`Commands::is_coming`]). The
/// server reads what its runners say in order, so a busy one can hear a runner's `Caps` long after
/// its hello: the clock is for a runner that says nothing, not a short wait.
pub const COMING_FOR: Duration = Duration::from_secs(60);

/// A call on the commands, run by their [`Owner`].
type Call = Box<dyn FnOnce(&mut Commands) + Send>;

/// The server's [`Commands`] without a lock (goal 5 of the API runtime program, plan 01a1a59c2aae):
/// a task of their own owns them and takes every change and question as a message, in the order they
/// come. A caller waits for its turn, not on a lock, and no shard stalls another holding them.
pub struct Owner {
    tx: crate::memory::queue::Sender<Call>,
}

impl Owner {
    /// `commands`, owned by a task started for them now.
    pub fn start(commands: Commands) -> Self {
        // Unbounded, and bounded by its callers (goal 11, plan 01a1a87b2e56 step 3): every call but
        // `tell`'s is waited for ([`Owner::ask`]), so those are one for each task waiting on the commands;
        // `tell`'s are `crate::hub::Hub::command_soon`'s, a command each for a session told at once (an
        // account's sessions held at its limit, a stale session's snapshot asked), each at most
        // [`ITEM_MOST`]. What the commands keep unanswered is [`ROOM`] a runner process at the most.
        // Lock-free (`crate::memory::queue`; goal 5, plan 01a1a59c2aae, its top goal): an async-channel's
        // wait took event-listener's `Mutex`, each caller's shard sending (a terminal's keystrokes, every
        // `command_start`) and the owner's waiting.
        let (tx, rx) = crate::memory::queue::unbounded::<Call>(&crate::memory::owners::CHANNELS);
        drop(crate::rt::spawn(async move {
            let mut commands = commands;
            while let Ok(call) = rx.recv().await {
                call(&mut commands);
            }
        }));
        Self { tx }
    }

    /// `f` run on the commands in its turn, not waited for.
    pub fn tell(&self, f: impl FnOnce(&mut Commands) + Send + 'static) {
        // Unbounded: refused only once the owner has stopped (the server's going).
        let _ = self.tx.try_send(Box::new(f));
    }

    /// `f` run on the commands in its turn, after what was sent before it; its answer. None once the
    /// owner has stopped.
    pub async fn ask<R: Send + 'static>(&self, f: impl FnOnce(&mut Commands) -> R + Send + 'static) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        self.tell(move |c| {
            let _ = tx.send(f(c));
        });
        rx.await.ok()
    }
}

/// Every runner process's command streams, on the server.
pub struct Commands {
    born: u64,
    room: usize,
    patience: Duration,
    peers: HashMap<PeerId, Peer>,
}

impl Commands {
    /// `born`: this server process's incarnation.
    pub fn new(born: u64) -> Self {
        Self::with(born, ROOM, PATIENCE)
    }

    pub fn with(born: u64, room: usize, patience: Duration) -> Self {
        Self { born, room, patience, peers: HashMap::new() }
    }

    fn peer(&mut self, peer: PeerId) -> &mut Peer {
        let (room, patience) = (self.room, self.patience);
        self.peers.entry(peer).or_insert_with(|| Peer { tx: Sender::new(room, patience), own: Pair::new(None), sessions: HashMap::new(), lanes: false, link: false, up: false, wire: false, coming: None })
    }

    /// Whether runner process `peer` takes commands on the channel (it said so, or sent a frame of
    /// it).
    pub fn keeps(&self, peer: PeerId) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.wire || p.coming.is_some())
    }

    /// Runner process `peer` connected: it says in a moment whether it keeps the channel (its
    /// `Caps`). Until it does, its commands wait for it, a live one too, rather than being refused
    /// as if it were away, or going the way a runner's from before the channel do.
    pub fn coming(&mut self, peer: PeerId, now: Instant) {
        let p = self.peer(peer);
        if !p.link {
            p.coming = Some(now);
        }
    }

    /// Runner process `peer` doesn't keep the channel: it said so, or it didn't say it does within
    /// [`COMING_FOR`]. What waited for it, each with the answer to give it, goes the way a
    /// runner's from before the channel gets it. One that said it does on a link before goes on
    /// as one that's away.
    pub fn not_wire(&mut self, peer: PeerId) -> Vec<(Vec<u8>, oneshot::Sender<Answer>)> {
        let born = self.born;
        match self.peers.get_mut(&peer) {
            Some(p) if p.wire => {
                p.coming = None;
                if !p.link {
                    restart(p, Kind::Live, born, false);
                }
                Vec::new()
            }
            Some(_) => self.peers.remove(&peer).map(|p| waiting_of(p).map(|w| (w.body, w.answer)).collect()).unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// The runner processes that connected and didn't say within [`COMING_FOR`] whether they keep
    /// the channel.
    pub fn overdue(&self, now: Instant) -> Vec<PeerId> {
        self.peers.iter().filter(|(_, p)| p.coming.is_some_and(|at| now.saturating_duration_since(at) >= COMING_FOR)).map(|(peer, _)| *peer).collect()
    }

    /// Whether runner process `peer` connected and hasn't said yet whether it keeps the channel.
    pub fn is_coming(&self, peer: PeerId) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.coming.is_some())
    }

    /// Whether runner process `peer`'s link is up and it said what it has.
    pub fn is_up(&self, peer: PeerId) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.up)
    }

    /// Whether runner process `peer` is away: its link is down, and it isn't connecting. One whose
    /// link is back but hasn't said yet what it has isn't (its commands go once it has), as a live
    /// command to it isn't refused ([`Commands::push`]).
    pub fn is_away(&self, peer: PeerId) -> bool {
        self.peers.get(&peer).is_none_or(|p| !p.link && p.coming.is_none())
    }

    /// Runner process `peer` keeps the channel, on a link that just came up: its commands go on
    /// it from now on, once it says what it has (a live one sent meanwhile waits for that, rather
    /// than being refused as if its runner were away). A link the server took for up went without
    /// the server hearing it go (a runner that started again on the same certificate is the same
    /// peer): as when it drops, its live commands end, and its parked ones wait for what the new
    /// link's first `Have` says (one that started again has nothing: they go on a new generation).
    pub fn connected(&mut self, peer: PeerId) {
        let born = self.born;
        let p = self.peer(peer);
        if p.up {
            p.up = false;
            p.tx.down();
            restart(p, Kind::Live, born, true);
        }
        p.link = true;
        p.wire = true;
        p.coming = None;
    }

    /// Runner process `peer` takes an agent session's commands on the session's own streams (`on`: its `Caps` say
    /// `CAP_SESSION_STREAMS`; runner-fan), or not.
    pub fn lanes(&mut self, peer: PeerId, on: bool) {
        self.peer(peer).lanes = on;
    }

    /// Queues `body` (an encoded `ServerMsg`) as a command to `peer`, for agent session `session` where it's one of a
    /// session's (`session_lane`): on the session's own streams to a runner process that takes them, else on the process's
    /// own. Its answer comes on the receiver. A live command to a runner that's away (its link down), or one past the room,
    /// is answered at once; one to a runner that has just connected goes once it has said what it has.
    pub fn push(&mut self, peer: PeerId, kind: Kind, session: Option<&str>, body: Vec<u8>, now: Instant) -> oneshot::Receiver<Answer> {
        let (tx, rx) = oneshot::channel();
        let born = self.born;
        let p = self.peer(peer);
        if kind == Kind::Live && !p.link && p.coming.is_none() {
            let _ = tx.send(Answer::Unreachable { why: "its runner is away".into() });
            return rx;
        }
        if body.len() > ITEM_MOST {
            crate::metrics::net().dropped(peer, crate::proto::LANE_CONTROL, "refused", 1);
            let _ = tx.send(Answer::Unreachable { why: format!("it's too large to send: {} bytes, more than the {ITEM_MOST} a command carries", body.len()) });
            return rx;
        }
        let Peer { tx: sender, own, sessions, lanes, .. } = p;
        let stream = match session.filter(|_| *lanes) {
            Some(session) => sessions.entry(session.to_owned()).or_insert_with(|| Pair::new(Some(session))).of(kind),
            None => own.of(kind),
        };
        match sender.push(&stream.name(born), body.clone()) {
            Ok(seq) => {
                stream.waiting.insert(seq, Waiting { answer: tx, body, at: now });
            }
            Err(_) => {
                let _ = tx.send(Answer::Unreachable { why: "too many commands wait for its runner already".into() });
            }
        }
        rx
    }

    /// A frame from runner process `peer` for its command streams: answers, a want, or what it has
    /// after a reconnect.
    pub fn on_frame(&mut self, peer: PeerId, frame: Frame) {
        let born = self.born;
        let have = matches!(frame, Frame::Have(_));
        let p = self.peer(peer);
        let events = p.tx.on_frame(frame);
        // A frame of the channel says it keeps it.
        p.wire = true;
        p.coming = None;
        if have {
            p.link = true;
            p.up = true;
        }
        let Peer { tx: sender, own, sessions, .. } = p;
        for e in events {
            match e {
                Event::Answered { stream, seq, outcome } => {
                    let Some((_, s)) = pair_of(own, sessions, &stream).and_then(|pair| pair.named(&stream, born)) else { continue };
                    if let Some(w) = s.waiting.remove(&seq) {
                        let _ = w.answer.send(match outcome {
                            Outcome::Taken => Answer::Taken,
                            Outcome::Refused { code, why } => Answer::Refused { code, why },
                        });
                    }
                }
                // The runner has less of a stream than it answered: it started again. Its parked
                // commands not yet answered go again on a new generation; its live ones end.
                Event::Regressed { stream, .. } => {
                    if let Some((kind, s)) = pair_of(own, sessions, &stream).and_then(|pair| pair.named(&stream, born)) {
                        restart_stream(sender, s, kind, born, true);
                    }
                }
            }
        }
    }

    /// Runner process `peer`'s link went down: its live commands not yet answered end (unreachable
    /// when it never said what it has on the link, so none of them went), and its parked ones wait
    /// for it.
    pub fn down(&mut self, peer: PeerId) {
        let born = self.born;
        let Some(p) = self.peers.get_mut(&peer) else { return };
        let went = p.up;
        p.link = false;
        p.up = false;
        p.coming = None;
        p.tx.down();
        restart(p, Kind::Live, born, went);
    }

    /// Runner process `peer` is gone for good (retired, forgotten): every command it hadn't
    /// answered ends unreachable.
    pub fn gone(&mut self, peer: PeerId, why: &str) {
        let Some(p) = self.peers.remove(&peer) else { return };
        for w in waiting_of(p) {
            let _ = w.answer.send(Answer::Unreachable { why: why.to_owned() });
        }
    }

    /// Agent session `session` closed for good (its close answered): its own streams go, on every runner process, what
    /// still waited on them ending unreachable, so a process that ran many sessions keeps no streams of those gone
    /// (runner-fan).
    pub fn forget_session(&mut self, session: &str) {
        let born = self.born;
        for p in self.peers.values_mut() {
            let Some(pair) = p.sessions.remove(session) else { continue };
            p.tx.drop_stream(&pair.parked.name(born));
            p.tx.drop_stream(&pair.live.name(born));
            for w in pair.into_waiting() {
                let _ = w.answer.send(Answer::Unreachable { why: format!("session {session} closed") });
            }
        }
    }

    /// What to send now, to whom: commands not yet sent on their link, and those whose answer is
    /// overdue. Parked commands waiting past [`PARK_FOR`] end unreachable.
    pub fn poll(&mut self, now: Instant) -> Vec<(PeerId, Frame)> {
        let born = self.born;
        let mut out = Vec::new();
        for (peer, p) in &mut self.peers {
            let Peer { tx: sender, own, sessions, .. } = p;
            for parked in std::iter::once(&mut own.parked).chain(sessions.values_mut().map(|pair| &mut pair.parked)) {
                let stale: Vec<Seq> = parked.waiting.iter().filter(|(_, w)| now.saturating_duration_since(w.at) >= PARK_FOR).map(|(s, _)| *s).collect();
                if stale.is_empty() {
                    continue;
                }
                // Its runner never came back for them: the stream starts over without them.
                for seq in stale {
                    if let Some(w) = parked.waiting.remove(&seq) {
                        let _ = w.answer.send(Answer::Unreachable { why: "its runner didn't come back in time".into() });
                    }
                }
                restart_stream(sender, parked, Kind::Parked, born, true);
            }
            out.extend(sender.poll(now).into_iter().map(|f| (*peer, f)));
        }
        out
    }

    /// When something here next comes due on its own, as of `now` (goal 9: the commands loop waits
    /// for this, never a tick): a runner process that connected reaching [`COMING_FOR`] without
    /// saying whether it keeps the channel ([`Commands::overdue`]), a parked command reaching
    /// [`PARK_FOR`], or what a process's sender next sends on its clock (an item not sent yet, an
    /// answer overdue at its stream's RTO, a want's again: `channel::Sender::due`). None while
    /// nothing waits on a clock: what the runners' words free is polled for as they come.
    pub fn due(&self, now: Instant) -> Option<Instant> {
        let mut due: Option<Instant> = None;
        let mut at = |t: Option<Instant>| {
            if let Some(t) = t {
                due = Some(due.map_or(t, |d: Instant| d.min(t)));
            }
        };
        for p in self.peers.values() {
            at(p.coming.map(|since| since.checked_add(COMING_FOR).unwrap_or(now)));
            for parked in std::iter::once(&p.own.parked).chain(p.sessions.values().map(|pair| &pair.parked)) {
                at(parked.waiting.values().map(|w| w.at.checked_add(PARK_FOR).unwrap_or(now)).min());
            }
            at(p.tx.due(now));
        }
        due
    }

    /// Commands waiting for an answer, across every runner process.
    pub fn waiting(&self) -> usize {
        self.peers
            .values()
            .map(|p| std::iter::once(&p.own).chain(p.sessions.values()).map(|pair| pair.parked.waiting.len().saturating_add(pair.live.waiting.len())).sum::<usize>())
            .sum()
    }
}

/// What every stream of runner process `p` holds waiting, its own and its sessions'.
fn waiting_of(p: Peer) -> impl Iterator<Item = Waiting> {
    p.own.into_waiting().chain(p.sessions.into_values().flat_map(Pair::into_waiting))
}

/// Of a runner process's streams, its own or an agent session's, those stream `name` is one of.
fn pair_of<'a>(own: &'a mut Pair, sessions: &'a mut HashMap<String, Pair>, name: &str) -> Option<&'a mut Pair> {
    match session_of(name) {
        Some(session) => sessions.get_mut(session),
        None => Some(own),
    }
}

/// Starts every stream of `p`'s of `kind` over on a new generation, its own and each of its sessions' ([`restart_stream`]).
fn restart(p: &mut Peer, kind: Kind, born: u64, went: bool) {
    let Peer { tx, own, sessions, .. } = p;
    restart_stream(tx, own.of(kind), kind, born, went);
    for pair in sessions.values_mut() {
        restart_stream(tx, pair.of(kind), kind, born, went);
    }
}

/// Starts `stream` (one of `tx`'s, of `kind`) over on a new generation: a parked stream with what it hadn't had answered,
/// in order; a live one empty, what it hadn't had answered ending unanswered (`went`: it may have reached the runner), or
/// unreachable (it can't have).
fn restart_stream(tx: &mut Sender, stream: &mut Stream, kind: Kind, born: u64, went: bool) {
    tx.drop_stream(&stream.name(born));
    let waiting = std::mem::take(&mut stream.waiting);
    stream.generation = stream.generation.saturating_add(1);
    let name = stream.name(born);
    for (_, w) in waiting {
        match kind {
            Kind::Live if went => {
                let _ = w.answer.send(Answer::Unanswered { why: "the link to its runner dropped before it answered".into() });
            }
            Kind::Live => {
                let _ = w.answer.send(Answer::Unreachable { why: "its runner went away before it said what it has".into() });
            }
            Kind::Parked => match tx.push(&name, w.body.clone()) {
                Ok(seq) => {
                    stream.waiting.insert(seq, w);
                }
                Err(_) => {
                    let _ = w.answer.send(Answer::Unreachable { why: "too many commands wait for its runner already".into() });
                }
            },
        }
    }
}

/// On the runner: a command stream's kind, from its name, so a newer generation lets go of the
/// ones before it ([`Current`]).
pub fn prefix_of(stream: &str) -> Option<&'static str> {
    match stream.split_once(':').map(|(p, _)| p) {
        Some("p") => Some("p"),
        Some("l") => Some("l"),
        _ => None,
    }
}

/// The agent session a command stream is of: `p:<born>.<gen>/<session>` and `l:<born>.<gen>/<session>` are a session's
/// own (to a runner announcing `CAP_SESSION_STREAMS`; runner-fan), which its session loop on the runner takes. None for a
/// runner process's own streams (`p:<born>.<gen>`), or a name that isn't a command stream's.
pub fn session_of(stream: &str) -> Option<&str> {
    prefix_of(stream)?;
    let (_, after) = stream.split_once(':')?;
    after.split_once('/').map(|(_, session)| session).filter(|s| !s.is_empty())
}

/// A command stream's kind, its server's incarnation, its generation and the agent session it's one of the own streams of
/// (none: its runner process's own), from its name.
fn named(stream: &str) -> Option<(&'static str, u64, u64, Option<&str>)> {
    let prefix = prefix_of(stream)?;
    let (born, rest) = stream.split_once(':')?.1.split_once('.')?;
    let generation = rest.split_once('/').map_or(rest, |(g, _)| g);
    Some((prefix, born.parse().ok()?, generation.parse().ok()?, session_of(stream)))
}

/// Whether, `by` having come as a newer stream ([`Stand::Newer`]), the receiver lets go of command stream `stream`: an
/// older generation of the same kind of the same owner (its runner process, or one agent session), or any of another
/// incarnation of its server.
pub fn superseded(by: &str, stream: &str) -> bool {
    match (named(by), named(stream)) {
        (Some((kind, born, _, session)), Some((k, b, _, s))) => b != born || (k == kind && s == session),
        _ => false,
    }
}

/// How many incarnations of its server a runner remembers having let go of.
const GONE_KEPT: usize = 16;

/// On the runner: which of its server's command streams are current. A newer generation of one
/// (its link dropped, or this process started again) lets go of those before it, as the streams
/// of a server that started again let go of the old server's. An item of a stream let go of that
/// comes late (held up below the channel, or sent twice) is dropped, not taken: its server dropped
/// that stream as it started the next, so nothing waits for its answer, and taken anew it would be
/// applied a second time. Each kind of each owner's streams has its generation: the runner process's
/// own, and each agent session's own (runner-fan), each taken by the loop that takes that owner's.
#[derive(Default)]
pub struct Current {
    /// By kind and owner (none: the runner process): the incarnation and generation of the newest stream.
    newest: HashMap<(&'static str, Option<String>), (u64, u64)>,
    /// Incarnations let go of, the newest last.
    gone: VecDeque<u64>,
}

/// What a stream an item came on is to its runner ([`Current::of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stand {
    /// The newest of its kind and owner, or not a command stream: the item goes as it comes.
    Current,
    /// Newer than the newest of its kind and owner: the receiver lets go of what it supersedes ([`superseded`]).
    Newer,
    /// One let go of: the item is dropped.
    Gone,
}

impl Current {
    pub fn new() -> Self {
        Self::default()
    }

    /// What `stream`, the one an item came on, is: the newest of its kind and owner from now when it's newer.
    pub fn of(&mut self, stream: &str) -> Stand {
        let Some((prefix, born, generation, session)) = named(stream) else { return Stand::Current };
        if self.gone.contains(&born) {
            return Stand::Gone;
        }
        let key = (prefix, session.map(str::to_owned));
        match self.newest.get(&key).copied() {
            Some((b, g)) if b == born && generation == g => return Stand::Current,
            Some((b, g)) if b == born && generation < g => return Stand::Gone,
            _ => {}
        }
        // A newer generation, or another incarnation (one that started since: a server's that
        // ended sends nothing more), whose streams let go of the old one's too.
        let old: Vec<u64> = self.newest.values().map(|(b, _)| *b).filter(|b| *b != born).collect();
        for b in old {
            if !self.gone.contains(&b) {
                self.gone.push_back(b);
            }
        }
        while self.gone.len() > GONE_KEPT {
            self.gone.pop_front();
        }
        self.newest.retain(|_, (b, _)| *b == born);
        self.newest.insert(key, (born, generation));
        Stand::Newer
    }

    /// Agent session `session` ended here: what's known of its own streams goes (runner-fan).
    pub fn forget_session(&mut self, session: &str) {
        self.newest.retain(|(_, s), _| s.as_deref() != Some(session));
    }
}

#[cfg(test)]
mod tests {
    use super::super::channel::{Arrival, Receiver};
    use super::*;

    fn t(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): the commands' owner takes its calls on a lock-free queue now.
    /// Calls told from every shard at once (as each terminal's keystrokes and every `command_start` are) are
    /// each run once, each sender's in the order it told them, and a question asked after them is answered
    /// once they've all run.
    #[test]
    fn g5_the_commands_owner_runs_calls_from_every_shard_once_in_order() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let owner = std::sync::Arc::new(Owner::start(Commands::new(1)));
            let (ran, log) = crate::memory::queue::unbounded::<(usize, usize)>(&crate::memory::owners::CHANNELS);
            let (senders, per) = (8usize, 500usize);
            let mut telling = Vec::new();
            for s in 0..senders {
                let (owner, ran) = (std::sync::Arc::clone(&owner), ran.clone());
                telling.push(crate::rt::spawn_spread_joined(async move {
                    for n in 0..per {
                        let ran = ran.clone();
                        owner.tell(move |_| {
                            let _ = ran.try_send((s, n));
                        });
                    }
                }));
            }
            drop(ran);
            for t in telling {
                t.await.map_err(|_| anyhow::anyhow!("a sender ended before its calls"))?;
            }
            let asked = crate::rt::timeout(Duration::from_secs(10), owner.ask(|c| c.waiting())).await.map_err(|_| anyhow::anyhow!("never answered"))?;
            anyhow::ensure!(asked == Some(0), "{asked:?}");
            let mut got = vec![Vec::new(); senders];
            while let Ok((s, n)) = log.try_recv() {
                got.get_mut(s).ok_or_else(|| anyhow::anyhow!("no sender {s}"))?.push(n);
            }
            for (s, theirs) in got.iter().enumerate() {
                anyhow::ensure!(theirs.iter().copied().eq(0..per), "sender {s}'s: {} of {per}, in order: {}", theirs.len(), theirs.windows(2).all(|w| w.first() < w.get(1)));
            }
            Ok(())
        })
    }

    /// The runner's side: takes every command it's sent (refusing those `refuse` picks), its
    /// answers back; what it took, in order.
    struct Runner {
        rx: Receiver,
        current: Current,
        took: Vec<Vec<u8>>,
    }

    impl Runner {
        fn new() -> Self {
            Self { rx: Receiver::new(), current: Current::new(), took: Vec::new() }
        }

        fn deliver(&mut self, frames: Vec<(PeerId, Frame)>, refuse: impl Fn(&[u8]) -> bool) -> Vec<Frame> {
            let mut back = Vec::new();
            for (_, f) in frames {
                if let Frame::Item { stream, seq, low, body } = f {
                    match self.current.of(&stream) {
                        Stand::Gone => continue,
                        Stand::Newer => {
                            let keep = stream.clone();
                            self.rx.forget_where(|s| s != keep && superseded(&keep, s));
                        }
                        Stand::Current => {}
                    }
                    match self.rx.on_item(&stream, seq, low, Instant::now()) {
                        Arrival::Take(_) => {
                            if refuse(&body) {
                                back.push(self.rx.refused(&stream, seq, Refusal::Unknown, "no such leg"));
                            } else {
                                self.took.push(body);
                                back.push(self.rx.took(&stream, seq));
                            }
                        }
                        Arrival::Answered(Some(a)) | Arrival::Gap(a) => back.push(a),
                        Arrival::Answered(None) => {}
                    }
                }
            }
            back
        }
    }

    fn answer(rx: &mut oneshot::Receiver<Answer>) -> Option<Answer> {
        rx.try_recv().ok().flatten()
    }

    #[test]
    fn a_command_is_answered_by_its_runner_and_a_refusal_says_why() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.on_frame(1, r.rx.have());
        let mut cancel = c.push(1, Kind::Parked, None, b"cancel a".to_vec(), base);
        let mut unknown = c.push(1, Kind::Parked, None, b"cancel nothing".to_vec(), base);
        for a in r.deliver(c.poll(base), |b| b == b"cancel nothing") {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut cancel), Some(Answer::Taken));
        assert_eq!(answer(&mut unknown), Some(Answer::Refused { code: Refusal::Unknown, why: "no such leg".into() }));
        assert_eq!(c.waiting(), 0);
    }

    /// A command too large for the frame its item rides is answered at once, never queued: it
    /// would never go, and the commands after it would wait behind it.
    #[test]
    fn a_command_too_large_for_a_frame_is_answered_at_once_and_the_next_goes() {
        // A peer of its own: the count is the process's.
        const PEER: PeerId = 9_001;
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.connected(PEER);
        c.on_frame(PEER, r.rx.have());
        let refused = || crate::metrics::net().toward(PEER).iter().filter(|d| d.lane == "control" && d.reason == "refused").map(|d| d.frames).sum::<u64>();
        let before = refused();
        let mut large = c.push(PEER, Kind::Live, None, vec![7; ITEM_MOST + 1], base);
        assert!(matches!(answer(&mut large), Some(Answer::Unreachable { why }) if why.contains("too large")));
        assert_eq!(refused(), before + 1, "counted as a frame let go toward its runner");
        let mut next = c.push(PEER, Kind::Live, None, b"interrupt".to_vec(), base);
        for a in r.deliver(c.poll(base), |_| false) {
            c.on_frame(PEER, a);
        }
        assert_eq!(answer(&mut next), Some(Answer::Taken));
        assert_eq!(r.took, vec![b"interrupt".to_vec()]);
    }

    #[test]
    fn a_parked_command_waits_for_its_runner_and_a_live_one_is_refused_while_it_is_away() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.connected(1);
        c.down(1);
        assert!(c.is_away(1), "its link is down");
        // Away (its link down): the live command ends at once, the parked one waits.
        let mut keys = c.push(1, Kind::Live, None, b"keys".to_vec(), base);
        assert!(matches!(answer(&mut keys), Some(Answer::Unreachable { .. })));
        let mut cancel = c.push(1, Kind::Parked, None, b"cancel a".to_vec(), base);
        assert!(c.poll(base).is_empty(), "nothing goes while it's away");
        c.connected(1);
        assert!(c.poll(base).is_empty(), "nothing goes before the runner says what it has");
        // Back, before it says what it has: not away (a parked command sent now is answered by the
        // runner, not as parked), though nothing goes yet.
        assert!(!c.is_away(1) && !c.is_up(1));
        c.on_frame(1, r.rx.have());
        for a in r.deliver(c.poll(t(base, 10)), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut cancel), Some(Answer::Taken));
        assert_eq!(r.took, vec![b"cancel a".to_vec()]);
    }

    /// The race's other half: a runner that connected again on its certificate (the same peer,
    /// its record from the link before saying it's away) is sent a live command between its hello
    /// and its Caps. It waits for them, rather than being refused as away.
    #[test]
    fn a_live_command_before_its_runner_says_it_keeps_the_channel_waits_for_that() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.connected(1);
        c.down(1);
        c.coming(1, base);
        let mut assign = c.push(1, Kind::Live, None, b"assign".to_vec(), base);
        assert_eq!(answer(&mut assign), None, "not refused: it's here, saying what it keeps");
        c.connected(1);
        c.on_frame(1, r.rx.have());
        for a in r.deliver(c.poll(t(base, 5)), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut assign), Some(Answer::Taken));
        assert_eq!(r.took, vec![b"assign".to_vec()]);
        // One that never says so in time, having said it keeps the channel before, is as away.
        c.down(1);
        c.coming(1, base);
        let mut late = c.push(1, Kind::Live, None, b"late".to_vec(), base);
        assert!(c.not_wire(1).is_empty(), "nothing goes the old way for it");
        assert!(matches!(answer(&mut late), Some(Answer::Unreachable { .. })), "it never went");
        assert!(c.keeps(1), "its parked ones still wait for it");
        assert!(matches!(answer(&mut c.push(1, Kind::Live, None, b"later".to_vec(), base)), Some(Answer::Unreachable { .. })), "away now");
    }

    /// A runner from before the channel never says it keeps it: what waited for it goes the way
    /// such a runner's does once [`COMING_FOR`] passes, and it's taken for what it is.
    #[test]
    fn what_waited_for_a_runner_from_before_the_channel_goes_its_way() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        c.coming(2, base);
        assert!(c.keeps(2), "it may keep it: it hasn't said");
        let mut retire = c.push(2, Kind::Parked, None, b"retire".to_vec(), base);
        let mut assign = c.push(2, Kind::Live, None, b"assign".to_vec(), base);
        assert!(c.overdue(t(base, 1000)).is_empty(), "within its time to say");
        assert_eq!(c.overdue(base + COMING_FOR), vec![2]);
        let waited = c.not_wire(2);
        let mut bodies: Vec<Vec<u8>> = waited.iter().map(|(b, _)| b.clone()).collect();
        bodies.sort();
        assert_eq!(bodies, vec![b"assign".to_vec(), b"retire".to_vec()]);
        assert!(!c.keeps(2), "not taken for one that keeps the channel");
        for (_, answer) in waited {
            let _ = answer.send(Answer::Sent);
        }
        assert_eq!((answer(&mut retire), answer(&mut assign)), (Some(Answer::Sent), Some(Answer::Sent)));
    }

    /// w20_ac05's race: a leg assigned the moment its runner connected, before it said what it
    /// has, was refused as though its runner were away, and the leg was never run.
    #[test]
    fn a_live_command_to_a_runner_that_just_connected_goes_once_it_says_what_it_has() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.connected(1);
        let mut assign = c.push(1, Kind::Live, None, b"assign".to_vec(), base);
        assert_eq!(answer(&mut assign), None, "not refused: its runner is here");
        assert!(c.poll(base).is_empty(), "nor sent before it says what it has");
        c.on_frame(1, r.rx.have());
        for a in r.deliver(c.poll(t(base, 5)), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut assign), Some(Answer::Taken));
        assert_eq!(r.took, vec![b"assign".to_vec()]);
        // Its link drops before it says what it has: nothing went, so it never reached it.
        let mut c = Commands::new(7);
        c.connected(2);
        let mut lost = c.push(2, Kind::Live, None, b"assign".to_vec(), base);
        c.down(2);
        assert!(matches!(answer(&mut lost), Some(Answer::Unreachable { .. })), "never sent: unreachable, not unanswered");
        assert_eq!(c.waiting(), 0);
    }

    #[test]
    fn a_link_that_drops_ends_live_commands_unanswered_and_sends_parked_ones_again_once() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.on_frame(1, r.rx.have());
        let mut keys = c.push(1, Kind::Live, None, b"keys".to_vec(), base);
        let mut close = c.push(1, Kind::Parked, None, b"close s".to_vec(), base);
        // Sent, and the runner took both, but its answers were lost with the link.
        let _lost = r.deliver(c.poll(base), |_| false);
        c.down(1);
        assert!(matches!(answer(&mut keys), Some(Answer::Unanswered { .. })), "it may have been applied: unanswered, not sent again");
        assert_eq!(answer(&mut close), None, "the parked one waits");
        // Back: the runner says what it has, and the parked command is answered without being taken twice.
        c.on_frame(1, r.rx.have());
        for a in r.deliver(c.poll(t(base, 50)), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut close), Some(Answer::Taken));
        assert_eq!(r.took, vec![b"keys".to_vec(), b"close s".to_vec()], "each taken once");
        // A live command on the new link goes on its new stream.
        let mut more = c.push(1, Kind::Live, None, b"more keys".to_vec(), t(base, 60));
        for a in r.deliver(c.poll(t(base, 60)), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut more), Some(Answer::Taken));
    }

    /// A live command's frame sent twice (or held up) below the channel, its copy coming once the
    /// link dropped and the stream went on to its next generation: dropped, not applied again.
    #[test]
    fn a_command_of_a_generation_let_go_of_that_comes_late_is_not_taken_again() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.on_frame(1, r.rx.have());
        let mut model = c.push(1, Kind::Live, None, b"model".to_vec(), base);
        let frames = c.poll(base);
        let copy = frames.clone();
        // Taken; its answer lost with the link, which drops.
        let _lost = r.deliver(frames, |_| false);
        c.down(1);
        assert!(matches!(answer(&mut model), Some(Answer::Unanswered { .. })));
        // Back, and the next live command goes on the stream's next generation.
        c.on_frame(1, r.rx.have());
        let mut keys = c.push(1, Kind::Live, None, b"keys".to_vec(), t(base, 50));
        for a in r.deliver(c.poll(t(base, 50)), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut keys), Some(Answer::Taken));
        // The first command's copy comes now: its stream was let go of.
        assert!(r.deliver(copy, |_| false).is_empty(), "nothing to answer for a stream let go of");
        assert_eq!(r.took, vec![b"model".to_vec(), b"keys".to_vec()], "each taken once");
    }

    #[test]
    fn a_runner_lets_go_of_a_server_that_started_again_and_of_older_generations() {
        let mut cur = Current::new();
        assert_eq!(cur.of("l:7.1"), Stand::Newer);
        assert_eq!(cur.of("l:7.1"), Stand::Current);
        assert_eq!(cur.of("p:7.1"), Stand::Newer);
        assert_eq!(cur.of("l:7.3"), Stand::Newer, "a newer generation");
        assert_eq!(cur.of("l:7.2"), Stand::Gone, "an older one");
        assert_eq!(cur.of("l:7.1"), Stand::Gone);
        assert_eq!(cur.of("p:7.1"), Stand::Current, "the other kind goes on");
        // The server started again (another incarnation): the old one's streams are let go of,
        // both kinds, whatever their generation.
        assert_eq!(cur.of("l:9.1"), Stand::Newer);
        assert_eq!(cur.of("p:7.1"), Stand::Gone);
        assert_eq!(cur.of("l:7.4"), Stand::Gone);
        assert_eq!(cur.of("p:9.1"), Stand::Newer);
        assert_eq!(cur.of("l:9.1"), Stand::Current);
        // Not a command stream: as it comes.
        assert_eq!(cur.of("s:abc#1"), Stand::Current);
    }

    #[test]
    fn a_runner_that_started_again_gets_its_parked_commands_again_on_a_new_generation() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.on_frame(1, r.rx.have());
        let mut first = c.push(1, Kind::Parked, None, b"one".to_vec(), base);
        for a in r.deliver(c.poll(base), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut first), Some(Answer::Taken));
        let mut second = c.push(1, Kind::Parked, None, b"two".to_vec(), t(base, 10));
        c.down(1);
        // It started again: it has nothing of the stream it had answered one of.
        let mut r = Runner::new();
        c.on_frame(1, r.rx.have());
        for a in r.deliver(c.poll(t(base, 20)), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut second), Some(Answer::Taken));
        assert_eq!(r.took, vec![b"two".to_vec()], "only what it hadn't answered, from 1 on the new generation");
    }

    /// Its process killed and started again on the same certificate (the same peer), before the
    /// server heard the old link go: the new one's commands start over as after a drop, and no
    /// command goes to the new process as if it were the old one.
    #[test]
    fn a_runner_that_started_again_on_the_same_peer_unheard_starts_over() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.connected(1);
        c.on_frame(1, r.rx.have());
        let mut one = c.push(1, Kind::Parked, None, b"one".to_vec(), base);
        for a in r.deliver(c.poll(base), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut one), Some(Answer::Taken));
        // Sent, never answered: the process that had them is gone.
        let mut two = c.push(1, Kind::Parked, None, b"two".to_vec(), t(base, 10));
        let mut keys = c.push(1, Kind::Live, None, b"keys".to_vec(), t(base, 10));
        let _gone_with_it = c.poll(t(base, 10));
        // The new process: its link, then what it has (nothing).
        c.connected(1);
        assert_eq!(answer(&mut keys), Some(Answer::Unanswered { why: "the link to its runner dropped before it answered".into() }));
        let mut r = Runner::new();
        c.on_frame(1, r.rx.have());
        let mut sent = 0;
        for _ in 0..3 {
            let frames = c.poll(t(base, 20));
            sent += frames.len();
            for a in r.deliver(frames, |_| false) {
                c.on_frame(1, a);
            }
        }
        assert_eq!(answer(&mut two), Some(Answer::Taken));
        assert_eq!(r.took, vec![b"two".to_vec()], "the parked one again, from 1 on a new generation; the live one not at all");
        assert_eq!(sent, 1, "sent once: no wants for what the old process had");
    }

    #[test]
    fn a_lost_frame_on_a_live_link_is_made_up_for() {
        let base = Instant::now();
        let mut c = Commands::with(7, ROOM, Duration::from_millis(100));
        let mut r = Runner::new();
        c.on_frame(1, r.rx.have());
        let mut a = c.push(1, Kind::Live, None, b"a".to_vec(), base);
        let mut b = c.push(1, Kind::Live, None, b"b".to_vec(), base);
        // The first item never arrives: the second is past a gap, and the runner wants the first.
        let frames: Vec<_> = c.poll(base).into_iter().skip(1).collect();
        for f in r.deliver(frames, |_| false) {
            c.on_frame(1, f);
        }
        for f in r.deliver(c.poll(t(base, 10)), |_| false) {
            c.on_frame(1, f);
        }
        assert_eq!((answer(&mut a), answer(&mut b)), (Some(Answer::Taken), Some(Answer::Taken)));
        assert_eq!(r.took, vec![b"a".to_vec(), b"b".to_vec()]);
        // An answer lost: the command goes again once its patience is up, and is answered again, not taken twice.
        let mut d = c.push(1, Kind::Live, None, b"d".to_vec(), t(base, 20));
        let _lost = r.deliver(c.poll(t(base, 20)), |_| false);
        assert!(c.poll(t(base, 50)).is_empty(), "not before its patience is up");
        for f in r.deliver(c.poll(t(base, 130)), |_| false) {
            c.on_frame(1, f);
        }
        assert_eq!(answer(&mut d), Some(Answer::Taken));
        assert_eq!(r.took.len(), 3);
    }

    /// Goal 9: what the commands loop waits for is each item's own due time, as poll would find it,
    /// and nothing while nothing waits on a clock. Do: an idle runner; one that connected and hasn't
    /// said whether it keeps the channel; a parked command to a runner away; a live one sent and its
    /// answer lost. Expect: none; COMING_FOR after its hello; PARK_FOR after the parked one came;
    /// its patience after it went, and poll sends it again then, not before; none once answered.
    #[test]
    fn g9_the_commands_due_time_is_each_items_own() {
        let base = Instant::now();
        let patience = Duration::from_millis(100);
        let mut c = Commands::with(7, ROOM, patience);
        let mut r = Runner::new();
        c.on_frame(1, r.rx.have());
        assert_eq!(c.due(base), None, "an idle runner's commands wait on no clock");
        c.coming(2, base);
        assert_eq!(c.due(base), Some(base + COMING_FOR), "the one coming: its COMING_FOR");
        assert_eq!(c.overdue(base + COMING_FOR), vec![2], "and it's overdue then, as due said");
        let _ = c.not_wire(2);
        c.connected(3);
        c.down(3);
        let _parked = c.push(3, Kind::Parked, None, b"cancel".to_vec(), t(base, 5));
        assert_eq!(c.due(t(base, 5)), Some(t(base, 5) + PARK_FOR), "a parked command to a runner away: its PARK_FOR");
        c.gone(3, "gone");
        let mut d = c.push(1, Kind::Live, None, b"d".to_vec(), t(base, 10));
        assert_eq!(c.due(t(base, 10)), Some(t(base, 10)), "not sent yet: due at once");
        let _lost = r.deliver(c.poll(t(base, 10)), |_| false);
        let due = c.due(t(base, 11));
        assert_eq!(due, Some(t(base, 10) + patience), "sent: due at its patience");
        assert!(c.poll(t(base, 109)).is_empty(), "poll agrees: nothing before");
        let again = c.poll(t(base, 110));
        assert_eq!(again.len(), 1, "and it goes again at its due time: {again:?}");
        for f in r.deliver(again, |_| false) {
            c.on_frame(1, f);
        }
        let _ = c.poll(t(base, 111));
        assert_eq!(answer(&mut d), Some(Answer::Taken));
        assert_eq!(c.due(t(base, 112)), None, "answered: nothing waits on a clock again");
    }

    #[test]
    fn a_runner_gone_for_good_ends_what_it_had_not_answered() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        c.connected(1);
        let mut parked = c.push(1, Kind::Parked, None, b"cancel".to_vec(), base);
        c.gone(1, "it was retired");
        assert_eq!(answer(&mut parked), Some(Answer::Unreachable { why: "it was retired".into() }));
        assert!(!c.keeps(1));
        // Past its parking: a parked command whose runner never came back ends unreachable.
        let mut c = Commands::new(7);
        c.connected(2);
        let mut late = c.push(2, Kind::Parked, None, b"close".to_vec(), base);
        let _ = c.poll(base + PARK_FOR);
        assert!(matches!(answer(&mut late), Some(Answer::Unreachable { .. })));
    }

    /// runner-fan (P): to a runner process that takes them (`CAP_SESSION_STREAMS`), a session's commands go on the session's
    /// own streams, and what isn't a session's on the process's; each is answered on its own, the session's command taken
    /// though the process's is refused; to one that doesn't take them, all on the process's, as before.
    #[test]
    fn runner_fan_a_sessions_commands_go_on_its_own_streams_to_a_runner_that_takes_them() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.on_frame(1, r.rx.have());
        c.lanes(1, true);
        let mut interrupt = c.push(1, Kind::Live, Some("s1"), b"interrupt".to_vec(), base);
        let mut assign = c.push(1, Kind::Live, None, b"assign".to_vec(), base);
        let frames = c.poll(base);
        let streams: Vec<String> = frames.iter().filter_map(|(_, f)| if let Frame::Item { stream, .. } = f { Some(stream.clone()) } else { None }).collect();
        assert_eq!(streams.iter().filter(|s| *s == "l:7.1/s1").count(), 1, "{streams:?}");
        assert_eq!(streams.iter().filter(|s| *s == "l:7.1").count(), 1, "{streams:?}");
        for a in r.deliver(frames, |b| b == b"assign") {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut interrupt), Some(Answer::Taken));
        assert!(matches!(answer(&mut assign), Some(Answer::Refused { .. })));
        // One that doesn't take them: on its own streams.
        let mut d = Commands::new(7);
        let r2 = Runner::new();
        d.on_frame(2, r2.rx.have());
        let _close = d.push(2, Kind::Parked, Some("s2"), b"close".to_vec(), base);
        let streams: Vec<String> = d.poll(base).into_iter().filter_map(|(_, f)| if let Frame::Item { stream, .. } = f { Some(stream) } else { None }).collect();
        assert_eq!(streams, vec!["p:7.1".to_owned()]);
    }

    /// runner-fan (P): a session's own streams keep the process's rules: its live command ends unanswered when the link
    /// drops, its parked one waits and goes again on a new generation, a runner that started again has it on a new one
    /// too; and once the session is closed for good, its streams go, what still waited ending unreachable.
    #[test]
    fn runner_fan_a_sessions_own_streams_go_down_restart_and_go_with_it() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let r = Runner::new();
        c.on_frame(1, r.rx.have());
        c.lanes(1, true);
        let mut live = c.push(1, Kind::Live, Some("s1"), b"keys".to_vec(), base);
        let mut parked = c.push(1, Kind::Parked, Some("s1"), b"approve".to_vec(), base);
        let _ = c.poll(base);
        c.down(1);
        assert!(matches!(answer(&mut live), Some(Answer::Unanswered { .. })), "the link dropped before it answered");
        assert_eq!(answer(&mut parked), None, "a parked one waits for its runner");
        // Back: the runner says it has nothing of the session's parked stream past what it had; the command goes again.
        let mut r = Runner::new();
        c.connected(1);
        c.on_frame(1, r.rx.have());
        for a in r.deliver(c.poll(base), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut parked), Some(Answer::Taken));
        assert_eq!(r.took, vec![b"approve".to_vec()]);
        // Closed for good: what still waits on its streams ends.
        let mut late = c.push(1, Kind::Parked, Some("s1"), b"edit".to_vec(), base);
        c.forget_session("s1");
        assert!(matches!(answer(&mut late), Some(Answer::Unreachable { .. })));
        assert_eq!(c.waiting(), 0);
    }

    /// runner-fan (P): on the runner, each kind of each owner's streams has a generation of its own: a session's newer
    /// generation lets go of its older ones, never another session's or the process's; another incarnation of the server
    /// lets go of every stream of the old one.
    #[test]
    fn runner_fan_each_owners_streams_have_their_own_generations() {
        let mut cur = Current::new();
        assert_eq!(cur.of("l:7.1/a"), Stand::Newer);
        assert_eq!(cur.of("l:7.1/b"), Stand::Newer);
        assert_eq!(cur.of("l:7.1"), Stand::Newer);
        assert_eq!(cur.of("l:7.2/a"), Stand::Newer, "a's newer generation");
        assert_eq!(cur.of("l:7.1/a"), Stand::Gone);
        assert_eq!(cur.of("l:7.1/b"), Stand::Current, "b's goes on");
        assert_eq!(cur.of("l:7.1"), Stand::Current, "the process's goes on");
        assert!(superseded("l:7.2/a", "l:7.1/a") && !superseded("l:7.2/a", "l:7.1/b") && !superseded("l:7.2/a", "l:7.1") && !superseded("l:7.2/a", "p:7.1/a"));
        assert!(superseded("l:9.1", "p:7.3/b"), "another incarnation lets go of every stream of the old");
        cur.forget_session("b");
        assert_eq!(cur.of("l:7.1/b"), Stand::Newer, "forgotten: its next is new again");
        assert_eq!(cur.of("l:9.1/a"), Stand::Newer);
        assert_eq!(cur.of("l:7.2/a"), Stand::Gone, "the old incarnation's");
    }

    /// runner-fan: a session's own command streams name it after their generation, whatever its id holds; a runner
    /// process's own streams, and streams that aren't command streams, name none.
    #[test]
    fn runner_fan_a_sessions_own_command_streams_name_it() {
        assert_eq!(session_of("p:1700.3/01a1aa52"), Some("01a1aa52"));
        assert_eq!(session_of("l:1700.1/a/b#c"), Some("a/b#c"), "everything after the generation's '/'");
        assert_eq!(session_of("p:1700.3"), None, "the process's own");
        assert_eq!(session_of("l:1700.3/"), None, "no session");
        assert_eq!(session_of("s:sess-1#b1.0"), None, "a runner's stream of a session's updates");
        assert_eq!(session_of("t:term#b1.0"), None);
    }
}
