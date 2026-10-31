//! What the UI's sockets carry (docs/wire.md, "The UI's sockets"): each socket's frames, one Rust
//! type for what goes down and one for what comes up. The server builds what it sends from these,
//! and the UI reads them as the TypeScript `super::ts` makes of them.
//!
//! The server's copy of an agent session holds what its runner said of it as JSON (a runner of
//! another release may say more than this one knows, and the copy keeps it), so a session's parts
//! go down as that JSON: the TypeScript says what they are (`ts(as = ..)`).
//!
//! What comes up a socket is read here into its type, or refused (docs/wire.md, "What can't be
//! read"): [`came`] says what a WebSocket message is, and [`session_up`], [`terminal_up`],
//! [`stream_up`] and [`unread_up`] read a socket's text. What can't be read is answered on the
//! socket, typed (a `nack` on the session socket, a `refused` frame on the others), and noted from
//! the person on it ([`Socket`]). A page that can't read a frame that came down says so up the same
//! socket ([`UnreadFrame`]), and the server, which sent it, notes that too.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tungstenite::protocol::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;

use crate::harness::core::Update;
use crate::harness::model::Command;
use crate::http::ws::Message;
use crate::wire::unread::{Code, Page, Sender, Unread, Wire};

/// What goes down an agent session's socket (`GET /api/harness/sessions/:id/ws`): a frame, with the
/// revision of the server's copy it brings a viewer to when it's one of the session's feed
/// (docs/wire.md, "Revisions"). Every frame the socket sends is one, and so is every frame of the
/// session's feed on the bus: the registry and its viewers share it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SessionDown {
    #[serde(flatten)]
    pub frame: SessionFrame,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub rev: Option<u64>,
}

impl SessionDown {
    /// The frame as its socket sends it.
    pub fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl From<SessionFrame> for SessionDown {
    fn from(frame: SessionFrame) -> Self {
        Self { frame, rev: None }
    }
}

/// A frame of an agent session's socket, tagged `t`: the session's own (as its runner's
/// [`crate::harness::core::Update`]s say it, and as the server's copy has it), and the answers to
/// what the viewer sent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SessionFrame {
    /// The session whole follows. `born`: the server process's incarnation, in a snapshot (a live
    /// `open`, a runner announcing the session again, has none).
    Open {
        #[cfg_attr(test, ts(as = "crate::harness::session::Info"))]
        info: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        born: Option<u64>,
    },
    /// Only what changed since the revision the viewer asked for (`?after=`) follows, then `synced`.
    Resumed {
        #[cfg_attr(test, ts(as = "crate::harness::session::Info"))]
        info: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        born: Option<u64>,
    },
    Meta {
        #[cfg_attr(test, ts(as = "crate::harness::model::Meta"))]
        meta: Value,
    },
    State {
        #[cfg_attr(test, ts(as = "crate::harness::model::State"))]
        state: Value,
    },
    /// What changed in the state since the last one (asked for with `?patch=state`): the fields set,
    /// and those gone.
    StatePatch {
        #[cfg_attr(test, ts(type = "Partial<AgentState>"))]
        set: serde_json::Map<String, Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gone: Vec<String>,
    },
    /// The session's whole changeset so far.
    Changes {
        #[cfg_attr(test, ts(as = "Vec<crate::harness::model::FileChange>"))]
        files: Value,
    },
    /// What changed in the changeset since the last one (asked for with `?patch=changes`): the files
    /// set, each in place of the one with its key or after the rest when new, and those gone.
    ChangesPatch {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(test, ts(as = "Vec<crate::harness::model::FileChange>"))]
        set: Vec<Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(test, ts(as = "Vec<crate::harness::model::FileKey>"))]
        gone: Vec<Value>,
    },
    /// Files finished messages mention, kept by the server (ahead of the events in a snapshot).
    Files { files: Vec<crate::harness::kept::Kept> },
    /// Upserts by id.
    Events {
        #[cfg_attr(test, ts(as = "Vec<crate::harness::model::Event>"))]
        events: Vec<Value>,
    },
    /// Text to append to a streaming event.
    Stream { id: String, text: String },
    /// The answer to one client's request (file suggestions ...).
    Reply { client: String, value: Value },
    /// What became of a message sent to the session (CP-T3.2), as its runner said.
    Inbox { said: Box<crate::harness::inbox::Said> },
    /// The timeline starts over (a snapshot follows); `from`, the number of its first event.
    Reset {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        from: Option<u64>,
    },
    /// The session is gone: the socket ends with this.
    Closed,
    /// The snapshot sent since `open` (or `resumed`) is complete.
    Synced,
    /// What a viewer sent without an id didn't get through (as clients from before ids are told).
    Error {
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        refused: Option<crate::harness::inbox::Refusal>,
    },
    /// A command sent under `id` was taken (docs/wire.md, "The UI's sockets").
    Ack {
        id: String,
        outcome: Outcome,
        /// What its inbox said, when it took a message (`accepted`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        accepted: Option<crate::harness::inbox::Answer>,
    },
    /// A command sent under `id` wasn't: its kind (`code`) and why; `again` when it may go again
    /// under its id. `id` null for what couldn't be read far enough to have one.
    Nack {
        id: Option<String>,
        code: String,
        why: String,
        again: bool,
        /// Its inbox's refusal, when that's what it was.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(test, ts(optional))]
        refused: Option<crate::harness::inbox::Refusal>,
    },
}

impl SessionFrame {
    /// The frame a session's viewers are sent for its runner's update `u`, as the server's copy
    /// takes it (`harness::registry`).
    pub fn of(u: &Update) -> Self {
        let json = |v: Result<Value, serde_json::Error>| v.unwrap_or(Value::Null);
        match u {
            Update::Open { info, .. } => SessionFrame::Open { info: json(serde_json::to_value(info)), born: None },
            Update::Closed => SessionFrame::Closed,
            Update::State { state } => SessionFrame::State { state: json(serde_json::to_value(state)) },
            Update::Meta { meta } => SessionFrame::Meta { meta: json(serde_json::to_value(meta)) },
            Update::Events { events } => SessionFrame::Events { events: events.iter().map(|e| json(serde_json::to_value(e))).collect() },
            Update::Changes { files } => SessionFrame::Changes { files: json(serde_json::to_value(files)) },
            Update::ChangesPatch { set, gone } => SessionFrame::ChangesPatch {
                set: set.iter().map(|f| json(serde_json::to_value(f))).collect(),
                gone: gone.iter().map(|k| json(serde_json::to_value(k))).collect(),
            },
            Update::Stream { id, text } => SessionFrame::Stream { id: id.clone(), text: text.clone() },
            Update::Reply { client, value } => SessionFrame::Reply { client: client.clone(), value: value.clone() },
            Update::Inbox { said } => SessionFrame::Inbox { said: said.clone() },
            Update::Reset { from } => SessionFrame::Reset { from: *from },
        }
    }
}

/// What became of a command that was taken (`crate::harness::asked`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AckOutcome"))]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Applied, by its runner or the server.
    Taken,
    /// Parked for its runner, which is away: it goes once the runner's back.
    Parked,
    /// Its session's inbox took the message (`accepted` says how).
    Accepted,
    /// Sent to a runner that can't say more.
    Unchecked,
    /// Its inbox didn't say in time: sent again under its id, it's answered what became of it.
    Pending,
}

/// What comes up an agent session's socket: a command under the id its client made, a frame that
/// came down that the page couldn't read, or a command as clients from before ids send it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub enum SessionUp {
    Under { t: Cmd, id: String, c: Command },
    Unread(UnreadFrame),
    Bare(Command),
}

/// The tag of a command under an id.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Cmd {
    Cmd,
}

/// What a page sends up any of the UI's sockets for a frame that came down that it couldn't read
/// (`{"t": "unread", "of", "why"}`): the frame's tag (`of`), or nothing when it couldn't read that
/// much, and why. The server, which sent the frame, notes it (the socket's `*_down` wire): a frame
/// whose tag the page doesn't know is `unknown`, one it couldn't read `malformed`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct UnreadFrame {
    pub t: UnreadTag,
    pub of: String,
    pub why: String,
}

/// The tag of an [`UnreadFrame`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum UnreadTag {
    Unread,
}

impl UnreadFrame {
    /// Its code: `unknown` for a frame whose tag the page could read, `malformed` for one it couldn't.
    pub fn code(&self) -> Code {
        if self.of.is_empty() { Code::Malformed } else { Code::Unknown }
    }
}

/// What a socket answers a frame that came up it couldn't read (the session socket answers a
/// `nack` instead): what it was (`of`: its tag, or `text` or `binary` when it couldn't be read as
/// far as one), its code, and why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "RefusedFrame"))]
pub struct Refused {
    pub of: String,
    pub code: Code,
    pub why: String,
}

impl Refused {
    pub fn malformed(of: &str, why: impl Into<String>) -> Self {
        Refused { of: of.to_owned(), code: Code::Malformed, why: why.into() }
    }

    pub fn unknown(of: &str, why: impl Into<String>) -> Self {
        Refused { of: of.to_owned(), code: Code::Unknown, why: why.into() }
    }

    /// A binary frame on a socket that takes text alone.
    pub fn binary() -> Self {
        Refused::malformed("binary", "this socket takes text frames, not binary ones")
    }
}

/// A WebSocket message that came up a page's socket, as its socket takes it.
#[derive(Debug)]
pub enum Came {
    Text(tungstenite::Utf8Bytes),
    /// Bytes: a terminal's keystrokes, or, on a socket that takes text alone, [`Refused::binary`].
    Binary(bytes::Bytes),
    /// A ping or a pong: tungstenite answers a ping itself.
    Control,
    /// The page closed the socket, or its connection broke.
    Gone,
    /// What couldn't be read and ends the socket: it's closed with `close`, as RFC 6455 says (§7.4.1):
    /// 1009 for a message past its limit, 1007 for text that isn't UTF-8, 1002 for what breaks the
    /// protocol.
    Ends { close: CloseFrame, refused: Refused },
}

/// What `m`, the next of a page's socket, is.
pub fn came(m: Option<Result<Message, tungstenite::Error>>) -> Came {
    use tungstenite::error::{Error, ProtocolError};
    // A close frame's reason is short (RFC 6455 §5.5: its payload is 125 bytes at most).
    let ends = |code, reason: &'static str, refused| Came::Ends { close: CloseFrame { code, reason: reason.into() }, refused };
    match m {
        Some(Ok(Message::Text(t))) => Came::Text(t),
        Some(Ok(Message::Binary(b))) => Came::Binary(b),
        Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => Came::Control,
        Some(Ok(Message::Close(_))) | None => Came::Gone,
        Some(Err(Error::Capacity(e))) => ends(CloseCode::Size, "a frame past this socket's limit", Refused::malformed("frame", format!("past this socket's limit: {e}"))),
        Some(Err(Error::Utf8(e))) => ends(CloseCode::Invalid, "text that isn't UTF-8", Refused::malformed("text", format!("not UTF-8: {e}"))),
        Some(Err(Error::Protocol(ProtocolError::ResetWithoutClosingHandshake))) => Came::Gone,
        Some(Err(Error::Protocol(e))) => ends(CloseCode::Protocol, "not a WebSocket frame", Refused::malformed("frame", format!("not a WebSocket frame: {e}"))),
        Some(Err(_)) => Came::Gone,
    }
}

/// What came up a socket other than the session's, read: one of its own frames, or a frame that
/// came down that the page couldn't read.
#[derive(Debug, Clone, PartialEq)]
pub enum Up<T> {
    Frame(T),
    Unread(UnreadFrame),
}

/// `text` as a JSON object, or why it isn't one (`malformed`).
fn object(text: &str) -> Result<serde_json::Map<String, Value>, Refused> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(o)) => Ok(o),
        Ok(_) => Err(Refused::malformed("text", "not a JSON object")),
        Err(e) => Err(Refused::malformed("text", format!("not JSON: {e}"))),
    }
}

/// `o` as an [`UnreadFrame`], when it's tagged `t: unread`.
fn unread_in(o: &serde_json::Map<String, Value>) -> Option<Result<UnreadFrame, Refused>> {
    (o.get("t").and_then(Value::as_str) == Some("unread")).then(|| serde_json::from_value(Value::Object(o.clone())).map_err(|e| Refused::malformed("unread", e.to_string())))
}

/// `v` read as `T`, an enum tagged `tag`: `unknown` when its tag names none of `T`'s variants (`no
/// <kind> <tag>`), `malformed` when it isn't an object, has no tag, or doesn't fit the variant its
/// tag names. An unknown field in a known variant is taken, as peers of other releases send them.
pub fn tagged<T: DeserializeOwned>(v: Value, tag: &str, kind: &str) -> Result<T, Refused> {
    let Some(t) = v.get(tag).and_then(Value::as_str).map(str::to_owned) else {
        return Err(Refused::malformed(kind, if v.is_object() { format!("no `{tag}`") } else { "not a JSON object".to_owned() }));
    };
    serde_json::from_value(v).map_err(|e| {
        // The tag alone, read: only a tag `T` doesn't know fails as an unknown variant.
        let alone = serde_json::from_value::<T>(serde_json::json!({ tag: t }));
        match alone {
            Err(a) if a.to_string().starts_with("unknown variant") => Refused::unknown(&t, format!("no {kind} {t}")),
            _ => Refused::malformed(&t, e.to_string()),
        }
    })
}

/// What a viewer sent up a session's socket that couldn't be read: the command's id when that much
/// could be read, whether it came as clients from before ids send theirs (or couldn't be read far
/// enough to say: such a client is told with an `error` too), and the refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub id: Option<String>,
    pub bare: bool,
    pub refused: Refused,
}

/// A command's id as a viewer sends it: 1 to [`crate::harness::ops::KEY_MAX`] bytes, no control
/// characters.
fn command_id(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|i| !i.is_empty() && i.len() <= crate::harness::ops::KEY_MAX && !i.chars().any(char::is_control)).map(str::to_owned)
}

/// What a viewer sent up a session's socket, read; or why it couldn't be: a command type this
/// server doesn't know (or a frame tag) is `unknown`, the rest `malformed` (docs/wire.md, "The UI's
/// sockets").
pub fn session_up(text: &str) -> Result<SessionUp, Unreadable> {
    let o = object(text).map_err(|refused| Unreadable { id: None, bare: true, refused })?;
    if let Some(f) = unread_in(&o) {
        return f.map(SessionUp::Unread).map_err(|refused| Unreadable { id: None, bare: false, refused });
    }
    let new = |id, refused| Unreadable { id, bare: false, refused };
    match o.get("t") {
        None => tagged(Value::Object(o), "type", "command").map(SessionUp::Bare).map_err(|refused| Unreadable { id: None, bare: true, refused }),
        Some(Value::String(t)) if t == "cmd" => {
            let Some(id) = command_id(o.get("id")) else {
                return Err(new(None, Refused::malformed("cmd", format!("a command's id is 1 to {} bytes with no control characters", crate::harness::ops::KEY_MAX))));
            };
            let Some(c) = o.get("c").cloned() else { return Err(new(Some(id), Refused::malformed("cmd", "no command (`c`)"))) };
            match tagged(c, "type", "command") {
                Ok(c) => Ok(SessionUp::Under { t: Cmd::Cmd, id, c }),
                Err(refused) => Err(new(Some(id), refused)),
            }
        }
        Some(Value::String(t)) => Err(new(None, Refused::unknown(t, format!("no frame {t}")))),
        Some(_) => Err(new(None, Refused::malformed("text", "its `t` isn't a string"))),
    }
}

/// What a page sent up a socket whose only frame up is [`UnreadFrame`] (the events and log
/// sockets), read; or why it couldn't be.
pub fn unread_up(text: &str) -> Result<UnreadFrame, Refused> {
    let o = object(text)?;
    if let Some(f) = unread_in(&o) {
        return f;
    }
    match o.get("t").and_then(Value::as_str) {
        Some(t) => Err(Refused::unknown(t, format!("no frame {t}"))),
        None => Err(Refused::malformed("text", "no `t`")),
    }
}

/// What a page sent up a terminal's socket as text, read; or why it couldn't be.
pub fn terminal_up(text: &str) -> Result<Up<TerminalUp>, Refused> {
    let o = object(text)?;
    if let Some(f) = unread_in(&o) {
        return f.map(Up::Unread);
    }
    tagged(Value::Object(o), "type", "frame").map(Up::Frame)
}

/// What a page sent up a live page's socket, read; or why it couldn't be. Its frames are told by
/// their keys (`input`, `control`, `webrtc`): one with none of them is `unknown`, named by its `t`
/// or one of its keys.
pub fn stream_up(text: &str) -> Result<Up<StreamUp>, Refused> {
    let o = object(text)?;
    if let Some(f) = unread_in(&o) {
        return f.map(Up::Unread);
    }
    if !["input", "control", "webrtc"].iter().any(|k| o.contains_key(*k)) {
        let of = o.get("t").and_then(Value::as_str).or_else(|| o.keys().next().map(String::as_str)).unwrap_or("text").to_owned();
        return Err(Refused::unknown(&of, format!("no frame {of}")));
    }
    serde_json::from_value(Value::Object(o)).map(Up::Frame).map_err(|e| Refused::malformed("text", e.to_string()))
}

/// A page's socket, as what it can't read is noted (`crate::wire::unread`): its wires up and down,
/// who's on it (a person), and the page its refusals show on (an agent's for a session's sockets, a
/// runner's for its terminals; none for the rest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socket {
    pub up: Wire,
    pub down: Wire,
    pub from: Sender,
    pub page: Option<Page>,
}

impl Socket {
    /// A person's socket.
    pub fn of(up: Wire, down: Wire, person: &str, page: Option<Page>) -> Self {
        Socket { up, down, from: Sender::Person { user: person.to_owned() }, page }
    }

    /// What came up that couldn't be read, as it's noted.
    pub fn refused(&self, r: &Refused) -> Unread {
        Unread { wire: self.up, from: self.from.clone(), what: r.of.clone(), code: r.code, why: r.why.clone() }
    }

    /// A frame that came down that the page couldn't read, as it said, as it's noted.
    pub fn unread(&self, f: &UnreadFrame) -> Unread {
        let what = if f.of.is_empty() { "frame".to_owned() } else { f.of.clone() };
        Unread { wire: self.down, from: self.from.clone(), what, code: f.code(), why: f.why.clone() }
    }
}

/// What goes down a terminal's socket (`/api/terminal/ws`, and a leg's, `/api/ci/legs/:id/terminal`),
/// as text: its output goes as binary frames beside these.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalDown {
    /// First on every attach: which session this socket is on.
    Session { session: crate::termsess::Info },
    /// The session isn't known (yet), e.g. its machine is reconnecting: try again by its id.
    Missing { session: String },
    /// The session's machine dropped; it keeps running there: attach again by its id.
    Reconnect,
    /// The session's size changed (the device in use set it): draw at exactly this size.
    Size { cols: u16, rows: u16 },
    Exit { code: Option<i32> },
    Error { message: String },
    /// What the page sent as text that couldn't be read.
    Refused(Refused),
}

impl TerminalDown {
    pub fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// What comes up a terminal's socket as text (keystrokes go as binary frames too).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalUp {
    Resize { cols: u16, rows: u16 },
    Input { data: String },
    Ping,
}

/// What goes down a job step's log socket (`/api/jobs/:id/logs/ws?step=`) as text: the log goes as
/// binary frames beside these.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogDown {
    /// A step's status changed.
    Step {
        idx: u32,
        #[cfg_attr(test, ts(as = "crate::store::Status"))]
        status: String,
    },
    /// The log is whole: the socket ends with this.
    End,
    /// What the page sent that couldn't be read.
    Refused(Refused),
}

impl LogDown {
    pub fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// What goes down a live page's socket (`/api/harness/sessions/:id/streams/:stream/ws`) as text: its
/// picture and sound go as binary frames beside these.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub enum StreamDown {
    /// The server's WebRTC, to this viewer.
    Webrtc { webrtc: WebrtcDown },
    /// What the page sent that couldn't be read.
    Refused { refused: Refused },
    /// What the stream's runner said of it ([`StreamSaid`]).
    Said(StreamSaid),
}

impl StreamDown {
    pub fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// What a live page's runner says of it, one key naming what (the server says `closed` too): what
/// each says is the runner's, passed on as it said it. A runner's event of a kind this build doesn't
/// know isn't passed on: it's refused, from the runner ([`StreamSaid::read`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum StreamSaid {
    /// It opened: its size, platform, engine and device.
    Opened(#[cfg_attr(test, ts(type = "unknown"))] Value),
    /// It couldn't open, or stopped.
    Error(#[cfg_attr(test, ts(type = "unknown"))] Value),
    /// What an agent did on it.
    Agent(#[cfg_attr(test, ts(type = "unknown"))] Value),
    Navigated(#[cfg_attr(test, ts(type = "unknown"))] Value),
    Loaded(#[cfg_attr(test, ts(type = "unknown"))] Value),
    /// Whether an editable field has its focus, and what kind.
    Focus(#[cfg_attr(test, ts(type = "unknown"))] Value),
    /// Where its fields are.
    Fields(#[cfg_attr(test, ts(type = "unknown"))] Value),
    /// Whether text is selected on it.
    Selected(#[cfg_attr(test, ts(type = "unknown"))] Value),
    /// Whether its window can be seen where it runs (Safari's on a locked Mac).
    Hidden(#[cfg_attr(test, ts(type = "unknown"))] Value),
    Tabs(#[cfg_attr(test, ts(type = "unknown"))] Value),
    /// A dialog it showed, answered for it.
    Dialog(#[cfg_attr(test, ts(type = "unknown"))] Value),
    /// How its encoder and page keep up.
    Stats(#[cfg_attr(test, ts(type = "unknown"))] Value),
    Recording(#[cfg_attr(test, ts(type = "unknown"))] Value),
    Recorded(#[cfg_attr(test, ts(type = "unknown"))] Value),
    /// Text copied on it.
    Copied(#[cfg_attr(test, ts(type = "unknown"))] Value),
    Closed(#[cfg_attr(test, ts(type = "unknown"))] Value),
}

impl StreamSaid {
    /// A runner's event of a live page, read: `unknown` when it names a kind this build doesn't
    /// know, `malformed` when it isn't JSON, or isn't an object with one key.
    pub fn read(text: &str) -> Result<Self, Refused> {
        let v: Value = serde_json::from_str(text).map_err(|e| Refused::malformed("text", format!("not JSON: {e}")))?;
        let kind = match v.as_object().map(|o| (o.len(), o.keys().next().cloned())) {
            Some((1, Some(kind))) => kind,
            Some((n, _)) => return Err(Refused::malformed("text", format!("{n} keys, not one naming what it says"))),
            None => return Err(Refused::malformed("text", "not a JSON object")),
        };
        serde_json::from_value(v).map_err(|e| match e.to_string() {
            e if e.starts_with("unknown variant") => Refused::unknown(&kind, format!("no stream event {kind}")),
            e => Refused::malformed(&kind, e),
        })
    }

    /// Its kind, as its one key says it.
    pub fn kind(&self) -> &'static str {
        match self {
            StreamSaid::Opened(_) => "opened",
            StreamSaid::Error(_) => "error",
            StreamSaid::Agent(_) => "agent",
            StreamSaid::Navigated(_) => "navigated",
            StreamSaid::Loaded(_) => "loaded",
            StreamSaid::Focus(_) => "focus",
            StreamSaid::Fields(_) => "fields",
            StreamSaid::Selected(_) => "selected",
            StreamSaid::Hidden(_) => "hidden",
            StreamSaid::Tabs(_) => "tabs",
            StreamSaid::Dialog(_) => "dialog",
            StreamSaid::Stats(_) => "stats",
            StreamSaid::Recording(_) => "recording",
            StreamSaid::Recorded(_) => "recorded",
            StreamSaid::Copied(_) => "copied",
            StreamSaid::Closed(_) => "closed",
        }
    }

    /// What it says.
    pub fn what(&self) -> &Value {
        match self {
            StreamSaid::Opened(v)
            | StreamSaid::Error(v)
            | StreamSaid::Agent(v)
            | StreamSaid::Navigated(v)
            | StreamSaid::Loaded(v)
            | StreamSaid::Focus(v)
            | StreamSaid::Fields(v)
            | StreamSaid::Selected(v)
            | StreamSaid::Hidden(v)
            | StreamSaid::Tabs(v)
            | StreamSaid::Dialog(v)
            | StreamSaid::Stats(v)
            | StreamSaid::Recording(v)
            | StreamSaid::Recorded(v)
            | StreamSaid::Copied(v)
            | StreamSaid::Closed(v) => v,
        }
    }
}

/// The server's WebRTC to a stream's viewer: whether it can, its answer to the viewer's offer (or
/// why not), and its peer gone.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum WebrtcDown {
    Can(bool),
    Answer(String),
    Error(String),
    Gone(String),
}

/// What comes up a live page's socket: what its person did, what they ask of the stream, and their
/// side of WebRTC, any of them in one frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StreamUp {
    /// Input events, to the stream's runner as they are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(type = "Array<Record<string, unknown>>"))]
    pub input: Option<Vec<Value>>,
    /// What's asked of the stream (resize, pause, keyframe, go, reload, back, forward), to its runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(type = "Record<string, unknown>"))]
    pub control: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub webrtc: Option<WebrtcUp>,
}

/// A viewer's side of WebRTC: its offer, whether its peer shows the stream, its peer closed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct WebrtcUp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub offer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub playing: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub close: Option<bool>,
}

/// What goes down a deployment's log socket (`/api/cd/apps/:app/resources/:key/logs/ws`): its pods'
/// lines as they're written, a pod whose log couldn't be followed, and the end, once every log has
/// ended.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CdLogDown {
    Lines { lines: Vec<CdLogLine> },
    Error { pod: String, message: String },
    End,
    /// What the page sent that couldn't be read.
    Refused(Refused),
}

impl CdLogDown {
    pub fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// One line of a deployment's log: its pod, its time (RFC 3339, or "" when the line had none), its
/// text.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CdLogLine {
    pub pod: String,
    pub ts: String,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session's feed ends with the type's `closed` (`harness::live`).
    #[test]
    fn the_feeds_last_frame_is_closed() {
        assert_eq!(crate::harness::live::closed(), r#"{"t":"closed"}"#);
    }

    fn refused(r: Result<impl std::fmt::Debug, Unreadable>) -> Unreadable {
        match r {
            Err(u) => u,
            Ok(up) => Unreadable { id: None, bare: false, refused: Refused::malformed("test", format!("read as {up:?}")) },
        }
    }

    /// Typed messages, step 6 (the plan's example): a command type this server doesn't know is
    /// `unknown` under its id, said `no command teleport`; a frame it doesn't know, unknown without
    /// one; the rest malformed, under the command's id when that much could be read.
    #[test]
    fn a_session_socket_reads_what_comes_up_or_says_why_not() -> anyhow::Result<()> {
        let teleport = refused(session_up(r#"{"t":"cmd","id":"c1","c":{"type":"teleport"}}"#));
        assert_eq!(teleport, Unreadable { id: Some("c1".into()), bare: false, refused: Refused::unknown("teleport", "no command teleport") });
        let frame = refused(session_up(r#"{"t":"hologram"}"#));
        assert_eq!((frame.id, frame.refused.code, frame.refused.why.as_str()), (None, Code::Unknown, "no frame hologram"));
        // A known command that doesn't fit is malformed, under its id: a nested enum's unknown value
        // too, which isn't a command this server doesn't know.
        for (text, id) in [
            (r#"{"t":"cmd","id":"c2","c":{"type":"set_model"}}"#, Some("c2")),
            (r#"{"t":"cmd","id":"c3","c":{"type":"approve","id":"a1","decision":"maybe"}}"#, Some("c3")),
            (r#"{"t":"cmd","id":"c4"}"#, Some("c4")),
            (r#"{"t":"cmd","id":"c5","c":5}"#, Some("c5")),
            (r#"{"t":"cmd","id":"","c":{"type":"interrupt"}}"#, None),
            (r#"{"t":"cmd","c":{"type":"interrupt"}}"#, None),
            (r#"{"t":5}"#, None),
            (r#"{"t":"unread","of":"x"}"#, None),
            ("not json", None),
            ("[1,2,3]", None),
        ] {
            let u = refused(session_up(text));
            assert_eq!((u.id.as_deref(), u.refused.code), (id, Code::Malformed), "{text}: {u:?}");
        }
        // As clients from before ids send them: unknown, or malformed, with no id, told with an
        // `error` too.
        let bare = refused(session_up(r#"{"type":"teleport"}"#));
        assert_eq!((bare.id, bare.bare, bare.refused.code), (None, true, Code::Unknown));
        assert!(refused(session_up(r#"{"type":"set_model"}"#)).bare);
        assert!(refused(session_up("not json")).bare);
        assert!(!refused(session_up(r#"{"t":"cmd","id":"c4"}"#)).bare);
        // What reads: a command under its id, a bare one, an unread frame; an unknown field in a
        // known command is taken.
        assert_eq!(session_up(r#"{"t":"cmd","id":"c1","c":{"type":"interrupt","later":1}}"#).ok(), Some(SessionUp::Under { t: Cmd::Cmd, id: "c1".into(), c: Command::Interrupt }));
        assert_eq!(session_up(r#"{"type":"interrupt"}"#).ok(), Some(SessionUp::Bare(Command::Interrupt)));
        let unread = UnreadFrame { t: UnreadTag::Unread, of: "hologram".into(), why: "unknown frame".into() };
        assert_eq!(session_up(r#"{"t":"unread","of":"hologram","why":"unknown frame"}"#).ok(), Some(SessionUp::Unread(unread.clone())));
        assert_eq!(serde_json::to_value(SessionUp::Unread(unread))?, serde_json::json!({"t": "unread", "of": "hologram", "why": "unknown frame"}));
        Ok(())
    }

    #[test]
    fn the_other_sockets_read_their_frames_and_unread_or_say_why_not() {
        let unread = || UnreadFrame { t: UnreadTag::Unread, of: String::new(), why: "not JSON".into() };
        let said = r#"{"t":"unread","of":"","why":"not JSON"}"#;
        assert_eq!(unread_up(said), Ok(unread()));
        assert_eq!(unread().code(), Code::Malformed);
        assert_eq!(unread_up(r#"{"t":"hologram"}"#).map_err(|r| (r.code, r.of)), Err((Code::Unknown, "hologram".to_owned())));
        assert_eq!(unread_up(r#"{"type":"resize"}"#).map_err(|r| r.code), Err(Code::Malformed));
        assert_eq!(terminal_up(r#"{"type":"resize","cols":80,"rows":24}"#), Ok(Up::Frame(TerminalUp::Resize { cols: 80, rows: 24 })));
        assert_eq!(terminal_up(said), Ok(Up::Unread(unread())));
        assert_eq!(terminal_up(r#"{"type":"teleport"}"#).map_err(|r| (r.code, r.why)), Err((Code::Unknown, "no frame teleport".to_owned())));
        assert_eq!(terminal_up(r#"{"type":"resize","cols":-1}"#).map_err(|r| r.code), Err(Code::Malformed));
        assert_eq!(terminal_up("{").map_err(|r| (r.code, r.of)), Err((Code::Malformed, "text".to_owned())));
        assert_eq!(stream_up(r#"{"webrtc":{"playing":true},"later":1}"#), Ok(Up::Frame(StreamUp { webrtc: Some(WebrtcUp { playing: Some(true), ..WebrtcUp::default() }), ..StreamUp::default() })));
        assert_eq!(stream_up(said), Ok(Up::Unread(unread())));
        assert_eq!(stream_up(r#"{"teleport":{}}"#).map_err(|r| (r.code, r.of)), Err((Code::Unknown, "teleport".to_owned())));
        assert_eq!(stream_up(r#"{"input":"keys"}"#).map_err(|r| r.code), Err(Code::Malformed));
    }

    /// What a live page's runner says is one of its kinds, one key naming it: each reads, and goes
    /// down a live page's socket as it came; a kind this build doesn't know is `unknown`, the rest
    /// `malformed`.
    #[test]
    fn what_a_runner_says_of_a_live_page_is_one_of_its_kinds() {
        let kinds = ["opened", "error", "agent", "navigated", "loaded", "focus", "fields", "selected", "hidden", "tabs", "dialog", "stats", "recording", "recorded", "copied", "closed"];
        for kind in kinds {
            let text = serde_json::json!({ kind: {"a": 1} }).to_string();
            let said = StreamSaid::read(&text);
            assert_eq!(said.as_ref().map(StreamSaid::kind), Ok(kind), "{text}");
            assert_eq!(said.as_ref().map(StreamSaid::what), Ok(&serde_json::json!({"a": 1})));
            assert_eq!(said.map(|s| StreamDown::Said(s).text()), Ok(text));
        }
        let read = |t: &str| StreamSaid::read(t).map_err(|r| (r.of, r.code));
        assert_eq!(read(r#"{"hologram":{}}"#), Err(("hologram".into(), Code::Unknown)));
        assert_eq!(read(r#"{"loaded":{},"navigated":{}}"#), Err(("text".into(), Code::Malformed)));
        assert_eq!(read(r#"{}"#), Err(("text".into(), Code::Malformed)));
        assert_eq!(read(r#""loaded""#), Err(("text".into(), Code::Malformed)));
        assert_eq!(read("{not json"), Err(("text".into(), Code::Malformed)));
    }

    /// What a page's socket can't read ends it only past its limit, as text that isn't UTF-8, or as
    /// what isn't a WebSocket frame, each closed with the code RFC 6455 gives it; a peer gone is gone.
    #[test]
    fn what_ends_a_socket_is_closed_with_its_code() {
        use tungstenite::error::{CapacityError, Error, ProtocolError};
        let code = |c: Came| match c {
            Came::Ends { close, refused } => Some((u16::from(close.code), refused.code)),
            _ => None,
        };
        assert_eq!(code(came(Some(Err(Error::Capacity(CapacityError::MessageTooLong { size: 17 << 20, max_size: 16 << 20 }))))), Some((1009, Code::Malformed)));
        assert_eq!(code(came(Some(Err(Error::Utf8("invalid".into()))))), Some((1007, Code::Malformed)));
        assert_eq!(code(came(Some(Err(Error::Protocol(ProtocolError::UnmaskedFrameFromClient))))), Some((1002, Code::Malformed)));
        assert!(matches!(came(Some(Err(Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)))), Came::Gone));
        assert!(matches!(came(Some(Err(Error::ConnectionClosed))), Came::Gone));
        assert!(matches!(came(None), Came::Gone));
        assert!(matches!(came(Some(Ok(Message::Ping(bytes::Bytes::new())))), Came::Control));
        assert!(matches!(came(Some(Ok(Message::binary(vec![1u8])))), Came::Binary(_)));
    }

    /// A refusal is noted from the person on the socket, on its page, on the wire up; what the page
    /// couldn't read of what came down, on the wire down.
    #[test]
    fn a_socket_notes_from_its_person_on_its_page() {
        let s = Socket::of(Wire::TerminalUp, Wire::TerminalDown, "user:ada", Some(Page::Runner("fedora".into())));
        let u = s.refused(&Refused::binary());
        assert_eq!((u.wire, u.from, u.what.as_str(), u.code), (Wire::TerminalUp, Sender::Person { user: "user:ada".into() }, "binary", Code::Malformed));
        let u = s.unread(&UnreadFrame { t: UnreadTag::Unread, of: "hologram".into(), why: "unknown frame".into() });
        assert_eq!((u.wire, u.what.as_str(), u.code), (Wire::TerminalDown, "hologram", Code::Unknown));
    }

    /// Every update a runner sends goes to a session's viewers as the session socket's frame of the
    /// same tag (`harness::registry`), and reads back as one.
    #[test]
    fn an_update_goes_down_as_its_frame() -> anyhow::Result<()> {
        let updates = [
            Update::Closed,
            Update::Stream { id: "e1".into(), text: "more".into() },
            Update::Reset { from: Some(3) },
            Update::Reply { client: "c1".into(), value: serde_json::json!(["a.rs"]) },
            Update::Changes { files: Vec::new() },
            Update::Events { events: Vec::new() },
        ];
        for u in updates {
            let frame = SessionDown::from(SessionFrame::of(&u));
            assert_eq!(serde_json::to_value(&frame)?, serde_json::to_value(&u)?, "{u:?}");
            let back: SessionDown = serde_json::from_str(&frame.text())?;
            assert_eq!(back, frame);
        }
        // With its revision, as the feed carries it.
        let stamped = SessionDown { frame: SessionFrame::Stream { id: "e1".into(), text: "x".into() }, rev: Some(7) };
        assert_eq!(serde_json::from_str::<SessionDown>(&stamped.text())?, stamped);
        assert!(serde_json::from_str::<SessionDown>(r#"{"t":"hologram"}"#).is_err(), "a tag it doesn't have");
        Ok(())
    }
}
