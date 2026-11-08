//! What both halves of a connection share: the priority classes, topics' delivery classes and
//! trust classes, lanes, what `poll_transmit` hands a carrier, and each kind's properties as
//! docs/browser-protocol.md's frame table has them.

use crate::frames::Kind;

/// Priority classes, served in strict order (docs/browser-protocol.md, "Priorities").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Prio {
    Control = 0,
    Interactive = 1,
    State = 2,
    Stream = 3,
    Bulk = 4,
}

pub const PRIOS: [Prio; 5] = [Prio::Control, Prio::Interactive, Prio::State, Prio::Stream, Prio::Bulk];

impl Prio {
    pub fn name(self) -> &'static str {
        match self {
            Prio::Control => "control",
            Prio::Interactive => "interactive",
            Prio::State => "state",
            Prio::Stream => "stream",
            Prio::Bulk => "bulk",
        }
    }
}

/// How a topic's changes are delivered (docs/wire.md's classes): state and telemetry keep their
/// newest value per key while they wait; a stream's are never merged but for text appended to
/// the same place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    State,
    Telemetry,
    Stream,
}

/// Who influences what a frame carries, which decides the compression context it may share
/// (docs/browser-protocol.md, "Compression").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Trust {
    /// Never compressed: a secret, or what the server doesn't class.
    Secret,
    /// kiln's records (ctx 1).
    Records,
    /// Attacker-influenced text (ctx 2).
    Text,
}

impl Trust {
    /// Its compression context, none for secrets.
    pub fn ctx(self) -> Option<u8> {
        match self {
            Trust::Secret => None,
            Trust::Records => Some(1),
            Trust::Text => Some(2),
        }
    }
}

/// A topic kind's delivery: what the catalogue says of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopicSpec {
    pub kind: u32,
    pub class: Class,
    pub prio: Prio,
    pub trust: Trust,
}

/// Where a frame may go on a carrier with more than one (docs/browser-protocol.md, "Lane").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// The connection's own stream.
    Control,
    /// A subscription's own stream.
    Sub(u32),
    /// May go as a datagram (small, urgent, and loss-tolerant: an idle frame, a probe and its
    /// answer); on a carrier without datagrams, the control stream.
    Datagram,
}

/// One frame of a transmit: where it is in the bytes, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub kind: Kind,
    pub lane: Lane,
    pub prio: Prio,
    pub urgent: bool,
    pub start: usize,
    pub len: usize,
}

/// What `poll_transmit` hands the carrier: one carrier message of frames, and each frame's lane,
/// priority and urgency, so a carrier with streams and datagrams can split it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transmit {
    pub bytes: Vec<u8>,
    pub frames: Vec<Sent>,
}

impl Transmit {
    /// The bytes of one lane's frames, in order.
    pub fn lane_bytes(&self, lane: Lane) -> Vec<u8> {
        let mut out = Vec::new();
        for f in self.frames.iter().filter(|f| f.lane == lane) {
            if let Some(b) = self.bytes.get(f.start..f.start.saturating_add(f.len)) {
                out.extend_from_slice(b);
            }
        }
        out
    }
}

/// The frame table's columns for a kind, as docs/browser-protocol.md writes them (a test holds
/// the doc to these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindInfo {
    pub class: &'static str,
    pub prio: &'static str,
    pub coalesced: &'static str,
    pub compressed: &'static str,
    pub urgent: &'static str,
    pub lane: &'static str,
}

pub fn info(k: Kind) -> KindInfo {
    let i = |class, prio, coalesced, compressed, urgent, lane| KindInfo { class, prio, coalesced, compressed, urgent, lane };
    match k {
        Kind::Open => i("handshake", "control", "never", "never (a secret: the ticket)", "yes", "control"),
        Kind::Accepted => i("handshake", "control", "never", "never", "yes", "control"),
        Kind::Refused => i("handshake", "control", "never", "never", "yes", "control"),
        Kind::Request => i("command or ask, as its call says", "control", "never", "never", "yes", "control"),
        Kind::Answer => i("answer", "control", "never", "only when its route says its trust class", "yes", "control"),
        Kind::Subscribe => i("state", "control", "never", "never", "no", "control"),
        Kind::Unsubscribe => i("state", "control", "never", "never", "no", "control"),
        Kind::Snapshot => i("stream", "bulk", "never", "its topic's trust class", "no", "sub"),
        Kind::Resumed => i("stream", "its topic's", "never", "its topic's trust class", "no", "sub"),
        Kind::Change => i("its topic's (state, telemetry or stream)", "its topic's", "state, telemetry: newest per key; stream: appends joined", "its topic's trust class", "when its topic is interactive", "sub"),
        Kind::Tail => i("stream", "its topic's", "joined", "its topic's trust class", "when its topic is interactive", "sub"),
        Kind::Ready => i("stream", "its topic's", "never", "its topic's trust class", "no", "sub"),
        Kind::End => i("answer", "control", "never", "never", "no", "sub"),
        Kind::Credit => i("state", "control", "newest per `sub`", "never", "no", "control"),
        Kind::Fragment => i("carried", "its frame's", "never", "never", "its frame's", "its frame's"),
        Kind::Idle => i("telemetry", "control", "newest", "never", "no", "datagram"),
        Kind::Probe => i("ask", "control, ahead of all", "never", "never", "yes", "datagram"),
        Kind::ProbeAck => i("answer", "control", "never", "never", "yes", "datagram"),
        Kind::Unread => i("ack", "control", "never", "never", "no", "control"),
        Kind::Compressed => i("carried", "its frames'", "never", "(is compression)", "no", "its frames'"),
        Kind::Close => i("handshake", "control", "never", "never", "yes", "control"),
        Kind::Input => i("command, refused while away", "interactive", "never", "never", "yes", "control"),
        Kind::SecretAnswer => i("answer", "control", "never", "never (a secret)", "yes", "control"),
        Kind::Resize => i("state", "interactive", "newest per `sub`", "never", "yes", "control"),
    }
}

/// Kinds that carry a secret: never handed to a compressor (docs/browser-protocol.md,
/// "Compression").
pub fn secret(k: Kind) -> bool {
    matches!(k, Kind::Open | Kind::SecretAnswer)
}

/// Kinds that may never be merged or dropped while they wait: commands, asks, answers, acks.
pub fn never_coalesced(k: Kind) -> bool {
    info(k).coalesced == "never"
}

/// Kinds that aren't counted against credit: the connection's own small frames.
pub fn credit_exempt(k: Kind) -> bool {
    matches!(k, Kind::Accepted | Kind::Refused | Kind::Idle | Kind::Probe | Kind::ProbeAck | Kind::Unread | Kind::Close | Kind::End)
}

/// Capability bits (`Open.caps`, `Accepted.caps`): each new one a new bit, never reused.
pub const CAP_DEFLATE_RAW: u64 = 1 << 0;
