//! What the UI's sockets carry (docs/wire.md, "The UI's sockets"): each socket's frames, one Rust
//! type for what goes down and one for what comes up. The server builds what it sends from these,
//! and the UI reads them as the TypeScript `super::ts` makes of them.
//!
//! The server's copy of an agent session holds what its runner said of it as JSON (a runner of
//! another release may say more than this one knows, and the copy keeps it), so a session's parts
//! go down as that JSON: the TypeScript says what they are (`ts(as = ..)`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::model::Command;

/// What goes down an agent session's socket (`GET /api/harness/sessions/:id/ws`): a frame, with the
/// revision of the server's copy it brings a viewer to when it's one of the session's feed
/// (docs/wire.md, "Revisions").
#[derive(Debug, Clone, Serialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, PartialEq)]
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

/// What comes up an agent session's socket: a command under the id its client made, or a command as
/// clients from before ids send it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub enum SessionUp {
    Under { t: Cmd, id: String, c: Command },
    Bare(Command),
}

/// The tag of a command under an id.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Cmd {
    Cmd,
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
    /// What the stream's runner said of it, one key naming what (`opened`, `navigated`, `loaded`,
    /// `focus`, `fields`, `selected`, `hidden`, `tabs`, `dialog`, `closed`, `error` ...), passed on
    /// as it said it.
    Said(#[cfg_attr(test, ts(type = "Record<string, unknown>"))] Value),
}

impl StreamDown {
    pub fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
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

    /// The session's feed ends with a frame of its own making (`harness::live`): the type's.
    #[test]
    fn the_feeds_last_frame_is_closed() {
        assert_eq!(crate::harness::live::CLOSED, SessionDown::from(SessionFrame::Closed).text());
    }
}
