//! The frames (docs/browser-protocol.md, "The frame kinds"): every kind's body, the types inside
//! them, the envelope (`kind`, `len`, body) and reading a carrier message of frames under the
//! protocol's rules.

use crate::schema::{Registry, Shape};
use crate::wire::{Bits, Body, Bytes, Reader, Refusal, Why, Wire, Writer};
use crate::{bp_enum, bp_struct};

/// The protocol's version: the newest this build speaks.
pub const VERSION: u32 = 1;
/// The oldest version this build still speaks (version N serves N-1).
pub const OLDEST: u32 = 1;
/// A frame's body, whole, is at most this long.
pub const MAX_FRAME: usize = 1 << 20;

bp_struct! {
    /// A topic: its kind (the catalogue's) and its key.
    pub struct Topic {
        pub kind: u32,
        pub key: String,
    }
}

bp_struct! {
    /// Where a subscription is: the server's incarnation and the topic's revision.
    pub struct Cursor {
        pub born: u64,
        pub rev: u64,
    }
}

bp_struct! {
    /// A position at which a write's effects show.
    pub struct Mark {
        pub topic: Topic,
        pub cursor: Cursor,
    }
}

bp_enum! {
    pub enum Method {
        Get = 0,
        Post = 1,
        Put = 2,
        Patch = 3,
        Delete = 4,
    }
}

bp_enum! {
    /// What a request asks.
    pub enum Call {
        /// A request for the server's route table, as REST would make it.
        Http { method: Method, path: String, body: Bytes } = 0,
        /// A command to an agent session: its `Command` as JSON.
        Command { session: String, command: Bytes } = 1,
    }
}

bp_enum! {
    /// An answer, as today's command answers are (docs/wire.md, "The UI's sockets").
    pub enum Outcome {
        Taken = 0,
        Accepted { epoch: u64, seq: u64, at: i64, repeat: bool } = 1,
        Unchecked = 2,
        Pending = 3,
        Parked = 4,
        Nack { code: String, why: String, again: bool } = 5,
        Reply { status: u16, body: Bytes } = 6,
    }
}

/// How long an answer lasts (docs/wire.md): kept and repeated, kept until replaced, or not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finality {
    Final,
    ForNow,
    NotThisTime,
}

impl Outcome {
    pub fn finality(&self) -> Finality {
        match self {
            Outcome::Parked => Finality::ForNow,
            Outcome::Pending | Outcome::Nack { again: true, .. } => Finality::NotThisTime,
            Outcome::Taken | Outcome::Accepted { .. } | Outcome::Unchecked | Outcome::Nack { again: false, .. } | Outcome::Reply { .. } => Finality::Final,
        }
    }
}

bp_enum! {
    /// An item's key in a collection topic.
    pub enum Key {
        Num { n: u64 } = 0,
        Str { s: String } = 1,
        /// The n-th `Str` key this subscription has been sent since its last snapshot or resume.
        Ref { n: u32 } = 2,
    }
}

bp_enum! {
    /// What changed.
    pub enum Delta {
        /// The topic's one record: the fields `mask` has bits for, in field order, each encoded
        /// as its type is.
        Set { mask: Bits, fields: Vec<Bytes> } = 0,
        /// An item, whole: every field of its record, in order.
        Put { key: Key, fields: Vec<Bytes> } = 1,
        /// Some fields of an item.
        Patch { key: Key, mask: Bits, fields: Vec<Bytes> } = 2,
        Remove { key: Key } = 3,
        /// Text or bytes at an offset of the topic's stream.
        Append { at: u64, data: Bytes } = 4,
        /// Text at an offset of one field of one item.
        Extend { key: Key, field: u32, at: u64, data: Bytes } = 5,
    }
}

bp_enum! {
    pub enum OpenRefusal {
        BadTicket = 0,
        Expired = 1,
        Reused = 2,
        TooOld = 3,
        TooNew = 4,
        Schema = 5,
        Busy = 6,
        Malformed = 7,
    }
}

bp_enum! {
    pub enum EndCode {
        Done = 0,
        Gone = 1,
        Denied = 2,
        Unknown = 3,
        Limit = 4,
    }
}

bp_enum! {
    pub enum CloseCode {
        Bye = 0,
        Expired = 1,
        SwitchBuild = 2,
        Revoked = 3,
        Shutdown = 4,
        Protocol = 5,
    }
}

bp_enum! {
    /// docs/wire.md's codes.
    pub enum UnreadCode {
        Unknown = 0,
        Malformed = 1,
    }
}

bp_enum! {
    /// Why a frame couldn't be read, by `wire::Why`'s index.
    pub enum UnreadWhy {
        CutShort = 0,
        TooLarge = 1,
        BadVarint = 2,
        BadValue = 3,
        TooDeep = 4,
        UnknownKind = 5,
        UnknownVariant = 6,
        LeftOver = 7,
        OutOfOrder = 8,
    }
}

impl UnreadWhy {
    pub fn of(why: Why) -> Self {
        match why {
            Why::CutShort => Self::CutShort,
            Why::TooLarge => Self::TooLarge,
            Why::BadVarint => Self::BadVarint,
            Why::BadValue => Self::BadValue,
            Why::TooDeep => Self::TooDeep,
            Why::UnknownKind => Self::UnknownKind,
            Why::UnknownVariant => Self::UnknownVariant,
            Why::LeftOver => Self::LeftOver,
            Why::OutOfOrder => Self::OutOfOrder,
        }
    }
}

bp_struct! {
    pub struct Open {
        pub version: u32,
        pub oldest: u32,
        pub caps: Bits,
        pub schema: Bits,
        pub ticket: Bytes,
        pub idle_ms: u32,
        pub window: u64,
        pub build: String,
    }
}

bp_struct! {
    pub struct Accepted {
        pub version: u32,
        pub caps: Bits,
        pub idle_ms: u32,
        pub born: u64,
        pub fragment: u32,
        pub build: String,
    }
}

bp_struct! {
    pub struct Refused {
        pub code: OpenRefusal,
        pub why: String,
        pub build: String,
    }
}

bp_struct! {
    pub struct Request {
        pub id: u64,
        pub call: Call,
    }
}

bp_struct! {
    /// The body of `Answer` and of `SecretAnswer`.
    pub struct Answer {
        pub id: u64,
        pub outcome: Outcome,
        pub at: Option<Mark>,
    }
}

bp_struct! {
    pub struct Subscribe {
        pub sub: u32,
        pub topic: Topic,
        pub cursor: Option<Cursor>,
        pub credit: u64,
    }
}

bp_struct! {
    pub struct Unsubscribe {
        pub sub: u32,
    }
}

bp_struct! {
    /// Begins a snapshot (`Snapshot`) or a resume (`Resumed`, from `cursor`).
    pub struct Begin {
        pub sub: u32,
        pub cursor: Cursor,
    }
}

bp_struct! {
    pub struct Change {
        pub sub: u32,
        pub rev: u64,
        pub step: u64,
        pub delta: Delta,
    }
}

bp_struct! {
    pub struct Tail {
        pub sub: u32,
        pub data: Bytes,
    }
}

bp_struct! {
    pub struct End {
        pub sub: u32,
        pub code: EndCode,
        pub why: String,
    }
}

bp_struct! {
    pub struct Credit {
        pub sub: u32,
        pub upto: u64,
    }
}

bp_struct! {
    pub struct Fragment {
        pub lane: u32,
        pub last: bool,
        pub data: Bytes,
    }
}

bp_struct! {
    pub struct Idle {
        pub got: u64,
    }
}

bp_struct! {
    pub struct Probe {
        pub nonce: u32,
    }
}

bp_struct! {
    pub struct ProbeAck {
        pub nonce: u32,
        pub got: u64,
    }
}

bp_struct! {
    pub struct Unread {
        pub kind: u32,
        pub code: UnreadCode,
        pub why: UnreadWhy,
    }
}

bp_struct! {
    pub struct Compressed {
        pub ctx: u8,
        /// How many bytes the batch inflates to: a browser's stream inflater reads until it has
        /// them, and nothing past the bound is ever made.
        pub len: u32,
        pub data: Bytes,
    }
}

bp_struct! {
    pub struct Close {
        pub code: CloseCode,
        pub why: String,
        pub build: String,
    }
}

bp_struct! {
    pub struct Input {
        pub sub: u32,
        pub data: Bytes,
    }
}

bp_struct! {
    pub struct Resize {
        pub sub: u32,
        pub cols: u16,
        pub rows: u16,
    }
}

/// Which way a kind goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    Up,
    Down,
    Both,
}

impl Way {
    pub fn name(self) -> &'static str {
        match self {
            Way::Up => "up",
            Way::Down => "down",
            Way::Both => "both",
        }
    }
}

macro_rules! kinds {
    ( $( $kind:ident = $idx:literal : $body:ident, $way:ident ; )+ ) => {
        /// A frame kind, by its index on the wire.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Kind {
            $( $kind = $idx, )+
        }

        /// Every kind, in index order.
        pub const KINDS: &[Kind] = &[ $( Kind::$kind, )+ ];

        impl Kind {
            pub fn index(self) -> u32 {
                self as u32
            }

            pub fn of_index(i: u32) -> Option<Kind> {
                match i {
                    $( $idx => Some(Kind::$kind), )+
                    _ => None,
                }
            }

            pub fn name(self) -> &'static str {
                match self {
                    $( Kind::$kind => stringify!($kind), )+
                }
            }

            /// The body type's name in the schema.
            pub fn body(self) -> &'static str {
                match self {
                    $( Kind::$kind => stringify!($body), )+
                }
            }

            pub fn way(self) -> Way {
                match self {
                    $( Kind::$kind => Way::$way, )+
                }
            }

            /// The version the kind came in: a peer speaking an older one is never sent it.
            pub fn since(self) -> u32 {
                1
            }
        }

        /// A frame, read whole.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Frame {
            $( $kind($body), )+
        }

        impl Frame {
            pub fn kind(&self) -> Kind {
                match self {
                    $( Frame::$kind(_) => Kind::$kind, )+
                }
            }

            fn put_body(&self, w: &mut Writer, version: u32) {
                match self {
                    $( Frame::$kind(b) => b.put_body(w, version), )+
                }
            }

            fn skip_body(kind: Kind, r: &mut Reader<'_>) -> Result<(), Refusal> {
                match kind {
                    $( Kind::$kind => <$body as Body>::skip_body(r), )+
                }
            }

            fn take_body(kind: Kind, r: &mut Reader<'_>) -> Result<Frame, Refusal> {
                match kind {
                    $( Kind::$kind => Ok(Frame::$kind(<$body as Body>::take_body(r)?)), )+
                }
            }

            /// The frame as the shared vectors hold it: `{"kind": .., "body": ..}`.
            pub fn json(&self) -> String {
                let mut s = format!("{{\"kind\":\"{}\",\"body\":", self.kind().name());
                match self {
                    $( Frame::$kind(b) => b.json(&mut s), )+
                }
                s.push('}');
                s
            }

            /// A frame drawn from `g`: its kind first, then its body (the TypeScript side draws
            /// the same).
            pub fn arb(g: &mut crate::rng::Rng) -> Frame {
                let k = KINDS.get(g.index(KINDS.len())).copied().unwrap_or(Kind::Idle);
                match k {
                    $( Kind::$kind => Frame::$kind(<$body as Wire>::arb(g)), )+
                }
            }
        }

        /// Every type the frames carry, in the schema.
        pub fn registry() -> Registry {
            let mut reg = Registry::default();
            $( <$body as Wire>::visit(&mut reg); )+
            crate::records::visit(&mut reg);
            reg
        }
    };
}

kinds! {
    Open = 0: Open, Up;
    Accepted = 1: Accepted, Down;
    Refused = 2: Refused, Down;
    Request = 3: Request, Up;
    Answer = 4: Answer, Down;
    Subscribe = 5: Subscribe, Up;
    Unsubscribe = 6: Unsubscribe, Up;
    Snapshot = 7: Begin, Down;
    Resumed = 8: Begin, Down;
    Change = 9: Change, Down;
    Tail = 10: Tail, Down;
    Ready = 11: Begin, Down;
    End = 12: End, Down;
    Credit = 13: Credit, Up;
    Fragment = 14: Fragment, Both;
    Idle = 15: Idle, Both;
    Probe = 16: Probe, Both;
    ProbeAck = 17: ProbeAck, Both;
    Unread = 18: Unread, Both;
    Compressed = 19: Compressed, Down;
    Close = 20: Close, Both;
    Input = 21: Input, Up;
    SecretAnswer = 22: Answer, Down;
    Resize = 23: Resize, Up;
}

/// The schema's canonical text and its hash, which `Open` carries.
pub fn schema_text() -> String {
    let kinds: Vec<(&str, u32, u32)> = KINDS.iter().map(|k| (k.name(), k.index(), k.since())).collect();
    registry().canonical(&kinds)
}

pub fn schema_hash() -> u64 {
    crate::schema::fnv64(schema_text().as_bytes())
}

/// The fields of a kind's body as the docs write them.
pub fn fields_of(kind: Kind) -> String {
    match registry().get(kind.body()) {
        Some(Shape::Struct { fields, .. }) => Shape::render_fields(fields),
        _ => String::new(),
    }
}

impl Frame {
    /// Writes the frame (kind, length, body) for a peer speaking `version`.
    pub fn encode_into(&self, version: u32, out: &mut Vec<u8>) {
        let mut body = Writer::new();
        self.put_body(&mut body, version);
        let mut w = Writer { buf: std::mem::take(out) };
        w.varint(u64::from(self.kind().index()));
        w.bytes(&body.buf);
        *out = w.buf;
    }

    pub fn encode(&self, version: u32) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(version, &mut out);
        out
    }
}

/// A frame read from a message, with how many bytes of newer trailing fields it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub frame: Frame,
    /// Bytes after the fields this build knows: a newer build's, counted, not refused.
    pub extra: usize,
}

/// A frame that couldn't be read: its kind when that much was read, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub kind: Option<u32>,
    pub why: Why,
}

/// Reads a body of `kind`: whole or refused.
pub fn decode_body(kind: u32, body: &[u8]) -> Result<Read, Fault> {
    let Some(k) = Kind::of_index(kind) else {
        return Err(Fault { kind: Some(kind), why: Why::UnknownKind });
    };
    // Checked whole first, allocating nothing, so a refused frame costs no memory.
    if let Err(e) = Frame::skip_body(k, &mut Reader::new(body)) {
        return Err(Fault { kind: Some(kind), why: e.why });
    }
    let mut r = Reader::new(body);
    match Frame::take_body(k, &mut r) {
        Ok(frame) => Ok(Read { frame, extra: r.left() }),
        Err(e) => Err(Fault { kind: Some(kind), why: e.why }),
    }
}

/// The frames of one carrier message, one at a time: each read whole or refused. A frame whose
/// length runs past the message refuses the rest of it; an unknown or malformed one is skipped by
/// its length, and the next is read.
#[derive(Debug, Clone)]
pub struct Frames<'a> {
    r: Reader<'a>,
    done: bool,
    max: usize,
}

impl<'a> Frames<'a> {
    pub fn new(msg: &'a [u8]) -> Self {
        Self { r: Reader::new(msg), done: false, max: MAX_FRAME }
    }

    /// With a smaller bound on a frame's body.
    pub fn with_max(msg: &'a [u8], max: usize) -> Self {
        Self { r: Reader::new(msg), done: false, max }
    }

    /// The next frame's kind and body, unread.
    pub fn next_raw(&mut self) -> Option<Result<(u32, &'a [u8]), Fault>> {
        if self.done || self.r.is_empty() {
            return None;
        }
        let kind = match self.r.varint() {
            Ok(k) => k,
            Err(e) => {
                self.done = true;
                return Some(Err(Fault { kind: None, why: e.why }));
            }
        };
        let Ok(kind) = u32::try_from(kind) else {
            self.done = true;
            return Some(Err(Fault { kind: None, why: Why::BadVarint }));
        };
        let len = match self.r.varint() {
            Ok(n) => n,
            Err(e) => {
                self.done = true;
                return Some(Err(Fault { kind: Some(kind), why: e.why }));
            }
        };
        let len = match usize::try_from(len) {
            Ok(n) if n <= self.max => n,
            _ => {
                self.done = true;
                return Some(Err(Fault { kind: Some(kind), why: Why::TooLarge }));
            }
        };
        match self.r.slice(len) {
            Ok(body) => Some(Ok((kind, body))),
            Err(e) => {
                self.done = true;
                Some(Err(Fault { kind: Some(kind), why: e.why }))
            }
        }
    }
}

impl Iterator for Frames<'_> {
    type Item = Result<Read, Fault>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_raw()? {
            Ok((kind, body)) => Some(decode_body(kind, body)),
            Err(e) => Some(Err(e)),
        }
    }
}

/// Every frame of a message, read.
pub fn decode_message(msg: &[u8]) -> Vec<Result<Read, Fault>> {
    Frames::new(msg).collect()
}

/// Frames written back to back into one message.
pub fn encode_message(frames: &[Frame], version: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames {
        f.encode_into(version, &mut out);
    }
    out
}
