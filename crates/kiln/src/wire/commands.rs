//! Commands from the server to a runner process (docs/wire.md, "Commands"). Each is an item of
//! one of that process's two command streams. The process answers it once it applied it: an
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
//! `l:<born>.<gen>`). A runner process that started again has neither stream. Its parked commands
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

/// Which of a runner process's two command streams a command rides.
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
    generation: u64,
    waiting: BTreeMap<Seq, Waiting>,
}

impl Stream {
    fn new(prefix: &'static str) -> Self {
        Self { prefix, generation: 1, waiting: BTreeMap::new() }
    }

    fn name(&self, born: u64) -> String {
        format!("{}:{born}.{}", self.prefix, self.generation)
    }
}

struct Peer {
    tx: Sender,
    parked: Stream,
    live: Stream,
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
        self.peers.entry(peer).or_insert_with(|| Peer { tx: Sender::new(room, patience), parked: Stream::new("p"), live: Stream::new("l"), link: false, up: false, wire: false, coming: None })
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
            Some(_) => self.peers.remove(&peer).map(|p| p.parked.waiting.into_values().chain(p.live.waiting.into_values()).map(|w| (w.body, w.answer)).collect()).unwrap_or_default(),
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

    /// Queues `body` (an encoded `ServerMsg`) as a command to `peer`: its answer comes on the
    /// receiver. A live command to a runner that's away (its link down), or one past the room, is
    /// answered at once; one to a runner that has just connected goes once it has said what it has.
    pub fn push(&mut self, peer: PeerId, kind: Kind, body: Vec<u8>, now: Instant) -> oneshot::Receiver<Answer> {
        let (tx, rx) = oneshot::channel();
        let born = self.born;
        let p = self.peer(peer);
        if kind == Kind::Live && !p.link && p.coming.is_none() {
            let _ = tx.send(Answer::Unreachable { why: "its runner is away".into() });
            return rx;
        }
        let stream = match kind {
            Kind::Parked => &mut p.parked,
            Kind::Live => &mut p.live,
        };
        match p.tx.push(&stream.name(born), body.clone()) {
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
        for e in events {
            match e {
                Event::Answered { stream, seq, outcome } => {
                    let s = if stream == p.parked.name(born) {
                        &mut p.parked
                    } else if stream == p.live.name(born) {
                        &mut p.live
                    } else {
                        continue;
                    };
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
                    if stream == p.parked.name(born) {
                        restart(p, Kind::Parked, born, true);
                    } else if stream == p.live.name(born) {
                        restart(p, Kind::Live, born, true);
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
        for w in p.parked.waiting.into_values().chain(p.live.waiting.into_values()) {
            let _ = w.answer.send(Answer::Unreachable { why: why.to_owned() });
        }
    }

    /// What to send now, to whom: commands not yet sent on their link, and those whose answer is
    /// overdue. Parked commands waiting past [`PARK_FOR`] end unreachable.
    pub fn poll(&mut self, now: Instant) -> Vec<(PeerId, Frame)> {
        let born = self.born;
        let mut out = Vec::new();
        for (peer, p) in &mut self.peers {
            let stale: Vec<Seq> = p.parked.waiting.iter().filter(|(_, w)| now.saturating_duration_since(w.at) >= PARK_FOR).map(|(s, _)| *s).collect();
            if !stale.is_empty() {
                // Its runner never came back for them: the stream starts over without them.
                for seq in stale {
                    if let Some(w) = p.parked.waiting.remove(&seq) {
                        let _ = w.answer.send(Answer::Unreachable { why: "its runner didn't come back in time".into() });
                    }
                }
                restart(p, Kind::Parked, born, true);
            }
            out.extend(p.tx.poll(now).into_iter().map(|f| (*peer, f)));
        }
        out
    }

    /// Commands waiting for an answer, across every runner process.
    pub fn waiting(&self) -> usize {
        self.peers.values().map(|p| p.parked.waiting.len() + p.live.waiting.len()).sum()
    }
}

/// Starts one of `p`'s streams over on a new generation: a parked stream with what it hadn't had
/// answered, in order; a live one empty, what it hadn't had answered ending unanswered (`went`:
/// it may have reached the runner), or unreachable (it can't have).
fn restart(p: &mut Peer, kind: Kind, born: u64, went: bool) {
    let stream = match kind {
        Kind::Parked => &mut p.parked,
        Kind::Live => &mut p.live,
    };
    p.tx.drop_stream(&stream.name(born));
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
            Kind::Parked => match p.tx.push(&name, w.body.clone()) {
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

/// A command stream's kind, its server's incarnation and its generation, from its name.
fn named(stream: &str) -> Option<(&'static str, u64, u64)> {
    let prefix = prefix_of(stream)?;
    let (born, generation) = stream.split_once(':')?.1.split_once('.')?;
    Some((prefix, born.parse().ok()?, generation.parse().ok()?))
}

/// How many incarnations of its server a runner remembers having let go of.
const GONE_KEPT: usize = 16;

/// On the runner: which of its server's command streams are current. A newer generation of one
/// (its link dropped, or this process started again) lets go of those before it, as the streams
/// of a server that started again let go of the old server's. An item of a stream let go of that
/// comes late (held up below the channel, or sent twice) is dropped, not taken: its server dropped
/// that stream as it started the next, so nothing waits for its answer, and taken anew it would be
/// applied a second time.
#[derive(Default)]
pub struct Current {
    /// By kind: the incarnation and generation of the newest stream.
    newest: HashMap<&'static str, (u64, u64)>,
    /// Incarnations let go of, the newest last.
    gone: VecDeque<u64>,
}

/// What a stream an item came on is to its runner ([`Current::of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stand {
    /// The newest of its kind, or not a command stream: the item goes as it comes.
    Current,
    /// Newer than the newest of its kind (`Receiver::forget_where` lets go of the others).
    Newer(&'static str),
    /// One let go of: the item is dropped.
    Gone,
}

impl Current {
    pub fn new() -> Self {
        Self::default()
    }

    /// What `stream`, the one an item came on, is: the newest of its kind from now when it's newer.
    pub fn of(&mut self, stream: &str) -> Stand {
        let Some((prefix, born, generation)) = named(stream) else { return Stand::Current };
        if self.gone.contains(&born) {
            return Stand::Gone;
        }
        match self.newest.get(prefix).copied() {
            Some((b, g)) if b == born && generation == g => return Stand::Current,
            Some((b, g)) if b == born && generation < g => return Stand::Gone,
            _ => {}
        }
        // A newer generation, or another incarnation (one that started since: a server's that
        // ended sends nothing more), whose kinds let go of the old one's too.
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
        self.newest.insert(prefix, (born, generation));
        Stand::Newer(prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::super::channel::{Arrival, Receiver};
    use super::*;

    fn t(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
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
                        Stand::Newer(prefix) => {
                            let keep = stream.clone();
                            self.rx.forget_where(|s| s != keep && prefix_of(s) == Some(prefix));
                        }
                        Stand::Current => {}
                    }
                    match self.rx.on_item(&stream, seq, low, body) {
                        Arrival::Take(body) => {
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
        let mut cancel = c.push(1, Kind::Parked, b"cancel a".to_vec(), base);
        let mut unknown = c.push(1, Kind::Parked, b"cancel nothing".to_vec(), base);
        for a in r.deliver(c.poll(base), |b| b == b"cancel nothing") {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut cancel), Some(Answer::Taken));
        assert_eq!(answer(&mut unknown), Some(Answer::Refused { code: Refusal::Unknown, why: "no such leg".into() }));
        assert_eq!(c.waiting(), 0);
    }

    #[test]
    fn a_parked_command_waits_for_its_runner_and_a_live_one_is_refused_while_it_is_away() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        let mut r = Runner::new();
        c.connected(1);
        c.down(1);
        // Away (its link down): the live command ends at once, the parked one waits.
        let mut keys = c.push(1, Kind::Live, b"keys".to_vec(), base);
        assert!(matches!(answer(&mut keys), Some(Answer::Unreachable { .. })));
        let mut cancel = c.push(1, Kind::Parked, b"cancel a".to_vec(), base);
        assert!(c.poll(base).is_empty(), "nothing goes while it's away");
        c.connected(1);
        assert!(c.poll(base).is_empty(), "nothing goes before the runner says what it has");
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
        let mut assign = c.push(1, Kind::Live, b"assign".to_vec(), base);
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
        let mut late = c.push(1, Kind::Live, b"late".to_vec(), base);
        assert!(c.not_wire(1).is_empty(), "nothing goes the old way for it");
        assert!(matches!(answer(&mut late), Some(Answer::Unreachable { .. })), "it never went");
        assert!(c.keeps(1), "its parked ones still wait for it");
        assert!(matches!(answer(&mut c.push(1, Kind::Live, b"later".to_vec(), base)), Some(Answer::Unreachable { .. })), "away now");
    }

    /// A runner from before the channel never says it keeps it: what waited for it goes the way
    /// such a runner's does once [`COMING_FOR`] passes, and it's taken for what it is.
    #[test]
    fn what_waited_for_a_runner_from_before_the_channel_goes_its_way() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        c.coming(2, base);
        assert!(c.keeps(2), "it may keep it: it hasn't said");
        let mut retire = c.push(2, Kind::Parked, b"retire".to_vec(), base);
        let mut assign = c.push(2, Kind::Live, b"assign".to_vec(), base);
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
        let mut assign = c.push(1, Kind::Live, b"assign".to_vec(), base);
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
        let mut lost = c.push(2, Kind::Live, b"assign".to_vec(), base);
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
        let mut keys = c.push(1, Kind::Live, b"keys".to_vec(), base);
        let mut close = c.push(1, Kind::Parked, b"close s".to_vec(), base);
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
        let mut more = c.push(1, Kind::Live, b"more keys".to_vec(), t(base, 60));
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
        let mut model = c.push(1, Kind::Live, b"model".to_vec(), base);
        let frames = c.poll(base);
        let copy = frames.clone();
        // Taken; its answer lost with the link, which drops.
        let _lost = r.deliver(frames, |_| false);
        c.down(1);
        assert!(matches!(answer(&mut model), Some(Answer::Unanswered { .. })));
        // Back, and the next live command goes on the stream's next generation.
        c.on_frame(1, r.rx.have());
        let mut keys = c.push(1, Kind::Live, b"keys".to_vec(), t(base, 50));
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
        assert_eq!(cur.of("l:7.1"), Stand::Newer("l"));
        assert_eq!(cur.of("l:7.1"), Stand::Current);
        assert_eq!(cur.of("p:7.1"), Stand::Newer("p"));
        assert_eq!(cur.of("l:7.3"), Stand::Newer("l"), "a newer generation");
        assert_eq!(cur.of("l:7.2"), Stand::Gone, "an older one");
        assert_eq!(cur.of("l:7.1"), Stand::Gone);
        assert_eq!(cur.of("p:7.1"), Stand::Current, "the other kind goes on");
        // The server started again (another incarnation): the old one's streams are let go of,
        // both kinds, whatever their generation.
        assert_eq!(cur.of("l:9.1"), Stand::Newer("l"));
        assert_eq!(cur.of("p:7.1"), Stand::Gone);
        assert_eq!(cur.of("l:7.4"), Stand::Gone);
        assert_eq!(cur.of("p:9.1"), Stand::Newer("p"));
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
        let mut first = c.push(1, Kind::Parked, b"one".to_vec(), base);
        for a in r.deliver(c.poll(base), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut first), Some(Answer::Taken));
        let mut second = c.push(1, Kind::Parked, b"two".to_vec(), t(base, 10));
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
        let mut one = c.push(1, Kind::Parked, b"one".to_vec(), base);
        for a in r.deliver(c.poll(base), |_| false) {
            c.on_frame(1, a);
        }
        assert_eq!(answer(&mut one), Some(Answer::Taken));
        // Sent, never answered: the process that had them is gone.
        let mut two = c.push(1, Kind::Parked, b"two".to_vec(), t(base, 10));
        let mut keys = c.push(1, Kind::Live, b"keys".to_vec(), t(base, 10));
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
        let mut a = c.push(1, Kind::Live, b"a".to_vec(), base);
        let mut b = c.push(1, Kind::Live, b"b".to_vec(), base);
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
        let mut d = c.push(1, Kind::Live, b"d".to_vec(), t(base, 20));
        let _lost = r.deliver(c.poll(t(base, 20)), |_| false);
        assert!(c.poll(t(base, 50)).is_empty(), "not before its patience is up");
        for f in r.deliver(c.poll(t(base, 130)), |_| false) {
            c.on_frame(1, f);
        }
        assert_eq!(answer(&mut d), Some(Answer::Taken));
        assert_eq!(r.took.len(), 3);
    }

    #[test]
    fn a_runner_gone_for_good_ends_what_it_had_not_answered() {
        let base = Instant::now();
        let mut c = Commands::new(7);
        c.connected(1);
        let mut parked = c.push(1, Kind::Parked, b"cancel".to_vec(), base);
        c.gone(1, "it was retired");
        assert_eq!(answer(&mut parked), Some(Answer::Unreachable { why: "it was retired".into() }));
        assert!(!c.keeps(1));
        // Past its parking: a parked command whose runner never came back ends unreachable.
        let mut c = Commands::new(7);
        c.connected(2);
        let mut late = c.push(2, Kind::Parked, b"close".to_vec(), base);
        let _ = c.poll(base + PARK_FOR);
        assert!(matches!(answer(&mut late), Some(Answer::Unreachable { .. })));
    }
}
