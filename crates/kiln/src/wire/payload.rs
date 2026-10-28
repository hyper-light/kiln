//! What frames carry (docs/wire.md, "What can't be read"): the JSON in a runner's and the server's
//! messages, read once, where its frame is read, into the type it is.
//!
//! Each variant of [`AgentMsg`] and [`ServerMsg`] that carries JSON names its payload's type in
//! [`payload_of`], matched without a catch-all arm, and [`payload`] reads it:
//! - a tag this build doesn't know (a newer peer's kind of message, or of something in one) is
//!   `unknown`;
//! - anything else it can't read (not JSON, cut short, a field missing or of the wrong type) is
//!   `malformed`.
//!
//! What can't be read is refused to its sender: on the channel its item is NACKed, off it its
//! sender is told with `Unread`; and it's noted (`crate::wire::unread`). Handlers take the value
//! read, and none reads JSON of its own.
//!
//! A field a known payload has that this build doesn't know is taken, as docs/wire.md says (JSON,
//! so either side can add fields): counted in `kiln_wire_ignored_fields_total{wire}`, one for each
//! such field of each message, and logged once an hour for each wire and field. It's never refused
//! or audited. serde can't say which fields it ignored inside an internally tagged enum (it reads
//! one whole first), and most of these payloads are one, so they're found by writing the value
//! read back and seeing what of what came it lacks.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use kiln_seams::Clock;
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::unread::{Code, Recent, Wire};
use crate::proto::{AgentMsg, ServerMsg};

/// How often each wire's unknown field is said in the log.
pub const LOG_EVERY: Duration = Duration::from_secs(3600);
/// The most wires and fields kept for when each was last said: past it, the least recent is let
/// go (and said again the next time it comes).
pub const LOGGED_MOST: usize = 1024;
/// The most unknown fields counted and said of one message.
pub const FIELDS_MOST: usize = 32;
/// The most of a tag this build doesn't know, said as what a message was.
const TAG_MOST: usize = 64;

/// Why what a message carries couldn't be read, as `crate::wire::unread::Unread` says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    /// Its message's variant (`Harness`, `HarnessCmd`).
    pub of: &'static str,
    /// What it was: its variant, and the kind it said when that's what this build doesn't know.
    /// Never what it said.
    pub what: String,
    pub code: Code,
    pub why: String,
    /// The agent session it was about, for a session's payload.
    pub session: Option<String>,
    /// What waits on it: told why too, so it isn't left waiting.
    pub waits: Option<Waits>,
}

impl Unreadable {
    /// About agent session `session`: its page shows it too.
    fn about(mut self: Box<Self>, session: &str) -> Box<Self> {
        self.session = Some(session.to_owned());
        self
    }

    fn waits(mut self: Box<Self>, waits: Waits) -> Box<Self> {
        self.waits = Some(waits);
        self
    }

    /// An answer saying why it couldn't be read, for what asked it: `{"error", "code"}`, the code
    /// `unknown` for a kind its reader doesn't know.
    pub fn error(&self) -> Value {
        serde_json::json!({ "error": format!("{} couldn't be read ({}): {}", self.what, self.code.name(), self.why), "code": self.code.name() })
    }
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} couldn't be read ({}): {}", self.what, self.code.name(), self.why)
    }
}

impl std::error::Error for Unreadable {}

/// What waits on a message that couldn't be read, told why beside its refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waits {
    /// Its sender, for the answer to its question `req`: answered with why.
    Asking(u64),
    /// Whoever here asked question `req`, which it answers: told why.
    Answering(u64),
    /// The viewers of stream `stream`, which it was to open: it's said to have failed.
    Stream(String),
    /// Agent session `session`, which it was to start: said refused, so it can be dismissed.
    Opening(String),
    /// The send of message `message` (its id, read of what couldn't be read as a command) to agent
    /// session `session`, for its `epoch`: the session's inbox here says what became of it
    /// (`crate::harness::host::unread_message`), a repeat of one it took answered as one.
    Sending { session: String, message: String, epoch: u64 },
}

/// The longest message id read of a command that can't be read ([`message_of`]).
const MESSAGE_MOST: usize = 256;

/// The id of a session's message (`{"id": …}`), read of `json` as a plain value: what a command
/// that can't be read was, when that much of it can be.
fn message_of(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let id = v.get("id")?.as_str()?;
    (!id.is_empty() && id.len() <= MESSAGE_MOST).then(|| id.to_owned())
}

/// `json`, what a message of variant `of` carries on `wire`, read as a `T`: `unknown` for a tag
/// this build doesn't know, `malformed` for the rest it can't read. The fields of it `T` doesn't
/// have are taken, counted and said in the log ([`Ignored`]).
pub fn payload<T: DeserializeOwned + Serialize>(wire: Wire, of: &'static str, json: &str) -> Result<T, Box<Unreadable>> {
    let came: Value = serde_json::from_str(json).map_err(|e| {
        Box::new(Unreadable {
            of,
            what: of.to_owned(),
            code: Code::Malformed,
            why: if e.is_eof() { "cut short".to_owned() } else { format!("not JSON: {e}") },
            session: None,
            waits: None,
        })
    })?;
    let read = T::deserialize(&came).map_err(|e| refused(of, &e))?;
    if let Ok(kept) = serde_json::to_value(&read) {
        let fields = unknown_fields(&came, &kept);
        if !fields.is_empty() {
            let said = ignored_here().lock().note(wire, &fields);
            for field in said {
                tracing::info!(wire = wire.name(), of, %field, "a field this build doesn't know, in a message it read: taken (said once an hour for each wire and field)");
            }
        }
    }
    Ok(read)
}

/// Why `T` couldn't be read from what came: a tag it doesn't know is `unknown`, the rest
/// `malformed`, said without the values that came (a secret's, say).
fn refused(of: &'static str, e: &serde_json::Error) -> Box<Unreadable> {
    let said = e.to_string();
    Box::new(match unknown_tag(&said) {
        Some(tag) => Unreadable { of, what: format!("{of} ({tag})"), code: Code::Unknown, why: said, session: None, waits: None },
        None => Unreadable { of, what: of.to_owned(), code: Code::Malformed, why: without_values(&said), session: None, waits: None },
    })
}

/// The tag serde says it doesn't know: `unknown variant `teleport`, expected …`.
fn unknown_tag(said: &str) -> Option<String> {
    let rest = said.strip_prefix("unknown variant `")?;
    let tag = rest.split('`').next()?;
    Some(tag.chars().take(TAG_MOST).collect())
}

/// serde's words without the value it found: `invalid type: string "hunter2", expected u32` is
/// `invalid type: string, expected u32`.
pub(crate) fn without_values(said: &str) -> String {
    for kind in ["invalid type: ", "invalid value: "] {
        if let Some(rest) = said.strip_prefix(kind)
            && let Some((found, expected)) = rest.split_once(", expected ")
        {
            let found = found.split(['`', '"']).next().unwrap_or_default().trim_end();
            return format!("{kind}{found}, expected {expected}");
        }
    }
    said.to_owned()
}

/// The fields of what `came` that what was read of it, written back (`kept`), lacks: a path each
/// (`info.new`, `events[].new`), at most [`FIELDS_MOST`]. A field left out of what's written back
/// when it's empty (none, false, 0, "", [], {}) counts only when it came with something in it.
pub fn unknown_fields(came: &Value, kept: &Value) -> Vec<String> {
    fn walk(came: &Value, kept: &Value, at: &str, out: &mut BTreeSet<String>) {
        match (came, kept) {
            (Value::Object(c), Value::Object(k)) => {
                for (key, v) in c {
                    if out.len() >= FIELDS_MOST {
                        return;
                    }
                    let path = if at.is_empty() { key.clone() } else { format!("{at}.{key}") };
                    match k.get(key) {
                        Some(kv) => walk(v, kv, &path, out),
                        None if !empty(v) => {
                            out.insert(path);
                        }
                        None => {}
                    }
                }
            }
            (Value::Array(c), Value::Array(k)) if c.len() == k.len() => {
                let path = format!("{at}[]");
                for (v, kv) in c.iter().zip(k) {
                    walk(v, kv, &path, out);
                }
            }
            _ => {}
        }
    }
    fn empty(v: &Value) -> bool {
        match v {
            Value::Null => true,
            Value::Bool(b) => !b,
            Value::Number(n) => n.as_f64() == Some(0.0),
            Value::String(s) => s.is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
        }
    }
    let mut out = BTreeSet::new();
    walk(came, kept, "", &mut out);
    out.into_iter().collect()
}

/// The fields this build doesn't know that came in payloads it read: counted, and each wire's
/// field said once an hour.
pub struct Ignored {
    clock: Arc<dyn Clock>,
    said: Recent<(Wire, String), Option<Instant>>,
}

impl Ignored {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Ignored { clock, said: Recent::new(LOGGED_MOST) }
    }

    /// `fields` of a message on `wire`: counted, and those not said within the hour returned, to
    /// say now.
    pub fn note(&mut self, wire: Wire, fields: &[String]) -> Vec<String> {
        crate::metrics::wire().ignored.add(&[wire.name()], u64::try_from(fields.len()).unwrap_or(u64::MAX));
        let now = self.clock.now();
        let mut say = Vec::new();
        for f in fields {
            let (last, _) = self.said.entry((wire, f.clone()), || None);
            if last.is_none_or(|at| now.saturating_duration_since(at) >= LOG_EVERY) {
                *last = Some(now);
                say.push(f.clone());
            }
        }
        say
    }
}

/// This process's [`Ignored`], on real time.
fn ignored_here() -> &'static Mutex<Ignored> {
    static I: OnceLock<Mutex<Ignored>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(Ignored::new(crate::rt::clock())))
}

/// A message as [`payload_of`] gives it: one that carries no JSON as it came, or what one carries,
/// read into its type, with the rest of it.
pub enum Carried<M, P> {
    /// A message that carries no JSON.
    Nothing(M),
    /// What one carries, read.
    Read(P),
}

/// A message whose variants may carry JSON ([`AgentMsg`], [`ServerMsg`]), read by [`payload_of`].
pub trait Carries: Sized {
    /// Each of its variants that carries JSON, with that read into its type.
    type Read;
    /// What carries JSON of it read into its type, or why it couldn't be.
    fn read(self) -> Result<Carried<Self, Self::Read>, Box<Unreadable>>;
}

/// `msg`, with what it carries read into its type ([`payload`]): each variant that carries JSON
/// names the type here, matched without a catch-all arm. Read where its frame is read, once.
pub fn payload_of<M: Carries>(msg: M) -> Result<Carried<M, M::Read>, Box<Unreadable>> {
    msg.read()
}

/// An answer by `req` to something the server asked a runner ([`AgentMsg::Answer`],
/// `AiTranscript`, `AiView`, `HarnessTaskOutput`, `HarnessHistory`, `DirList`). It's of the type
/// its question asks for, so it's read once it's matched to its question (`crate::hub`'s asks), by
/// [`payload`] as that type.
pub struct Answer {
    pub of: &'static str,
    pub req: u64,
    pub json: Option<String>,
}

/// What a runner's messages carry, as the server reads it: each of [`AgentMsg`]'s variants that
/// carries JSON, with its payload's type in place of the JSON.
pub enum FromRunner {
    /// `AiSessions`: the AI coding sessions running on its machine.
    AiSessions(Vec<crate::aisessions::AiSession>),
    /// `AiTranscript`, `AiView`, `HarnessTaskOutput`, `HarnessHistory`, `DirList`, `Answer`.
    Answer(Answer),
    /// `Harness`: one update of agent session `session`.
    Harness { session: String, update: Box<crate::harness::core::Update> },
    HarnessInventory(Box<crate::harness::host::Inventory>),
    Sync { msg: Box<crate::sync::wire::Msg>, raft: Vec<u8> },
    FrontDoorStatus(Box<crate::frontdoor::Status>),
    JobEvent { job: String, event: Box<crate::proto::JobEvent> },
    Facts(Box<kiln_ci::facts::RunnerFacts>),
    ReachHello(crate::reach::relay::Announce),
    /// `ReachAsk`: a question for the server.
    ReachAsk { req: u64, ask: Box<crate::reach::Question> },
    Convo { msg: Box<crate::convo::feed::Msg>, data: Vec<u8> },
    Power(crate::os::battery::Battery),
    HarnessFileData { req: u64, answer: crate::harness::files::Answer, data: Vec<u8> },
    /// `StreamEvent`: what stream `stream` says (opened, navigated, loaded, focus, dialog, closed,
    /// error).
    StreamEvent { stream: String, event: Value },
    Executor(crate::runner::pool::Said),
    Undeployed(kiln_ci::wire::Undeployed),
    Cluster { msg: Box<crate::store::learner::Msg>, raft: Vec<u8> },
    /// `WorkState`: what it holds warm, for placement (docs/ci.md W24-R18).
    WorkState(Box<kiln_ci::work::slots::WorkState>),
    /// `AgentLimit`: the most agent sessions its machine has at work at once, and where that comes
    /// from (docs/agents.md, "How many agents a machine runs").
    AgentLimit(crate::harness::limit::Held),
    /// `Identity`: a certificate asked for, for a key the runner process made and keeps (docs/ci.md
    /// W27-R01).
    Identity(crate::proto::IdentityAsk),
    /// `Observe`: its replica's word in observe's group (docs/observe.md §5), and what it carries.
    Observe { msg: Box<crate::observe::wire::Msg>, bytes: Vec<u8> },
    /// `HeadroomStatus`: how its machine's headroom does (docs/headroom.md), kept and shown as it
    /// came: of it, the server reads only the fingerprint of the settings in use.
    HeadroomStatus(Value),
}

impl Carries for AgentMsg {
    type Read = FromRunner;

    fn read(self) -> Result<Carried<Self, FromRunner>, Box<Unreadable>> {
        const W: Wire = Wire::RunnerPayload;
        let read = match self {
            AgentMsg::AiSessions { json } => FromRunner::AiSessions(payload(W, "AiSessions", &json)?),
            AgentMsg::AiTranscript { req, json } => FromRunner::Answer(Answer { of: "AiTranscript", req, json }),
            AgentMsg::Harness { session, json } => match payload(W, "Harness", &json) {
                Ok(update) => FromRunner::Harness { session, update },
                Err(e) => return Err(e.about(&session)),
            },
            AgentMsg::HarnessInventory { json } => FromRunner::HarnessInventory(payload(W, "HarnessInventory", &json)?),
            AgentMsg::AiView { req, json } => FromRunner::Answer(Answer { of: "AiView", req, json }),
            AgentMsg::HarnessTaskOutput { req, json } => FromRunner::Answer(Answer { of: "HarnessTaskOutput", req, json }),
            AgentMsg::DirList { req, json } => FromRunner::Answer(Answer { of: "DirList", req, json: Some(json) }),
            AgentMsg::HarnessHistory { req, json } => FromRunner::Answer(Answer { of: "HarnessHistory", req, json }),
            AgentMsg::Sync { json, raft } => FromRunner::Sync { msg: payload(W, "Sync", &json)?, raft },
            AgentMsg::FrontDoorStatus { json } => FromRunner::FrontDoorStatus(payload(W, "FrontDoorStatus", &json)?),
            AgentMsg::JobEvent { job, json } => FromRunner::JobEvent { job, event: payload(W, "JobEvent", &json)? },
            AgentMsg::Facts { json } => FromRunner::Facts(payload(W, "Facts", &json)?),
            AgentMsg::ReachHello { json } => FromRunner::ReachHello(payload(W, "ReachHello", &json)?),
            AgentMsg::ReachAsk { req, json } => FromRunner::ReachAsk { req, ask: payload(W, "ReachAsk", &json).map_err(|e| e.waits(Waits::Asking(req)))? },
            AgentMsg::Convo { json, data } => FromRunner::Convo { msg: payload(W, "Convo", &json)?, data },
            AgentMsg::Power { json } => FromRunner::Power(payload(W, "Power", &json)?),
            AgentMsg::Answer { req, json } => FromRunner::Answer(Answer { of: "Answer", req, json }),
            AgentMsg::HarnessFileData { req, json, data } => FromRunner::HarnessFileData { req, answer: payload(W, "HarnessFileData", &json).map_err(|e| e.waits(Waits::Answering(req)))?, data },
            AgentMsg::StreamEvent { stream, json } => FromRunner::StreamEvent { stream, event: payload(W, "StreamEvent", &json)? },
            AgentMsg::Executor { json } => FromRunner::Executor(payload(W, "Executor", &json)?),
            AgentMsg::Undeployed { json } => FromRunner::Undeployed(payload(W, "Undeployed", &json)?),
            AgentMsg::Cluster { json, raft } => FromRunner::Cluster { msg: payload(W, "Cluster", &json)?, raft },
            AgentMsg::WorkState { json } => FromRunner::WorkState(payload(W, "WorkState", &json)?),
            AgentMsg::AgentLimit { json } => FromRunner::AgentLimit(payload(W, "AgentLimit", &json)?),
            AgentMsg::Identity { json } => FromRunner::Identity(payload(W, "Identity", &json)?),
            AgentMsg::Observe { json, bytes } => FromRunner::Observe { msg: payload(W, "Observe", &json)?, bytes },
            AgentMsg::HeadroomStatus { json } => FromRunner::HeadroomStatus(payload(W, "HeadroomStatus", &json)?),
            m @ (AgentMsg::Hello(_)
            | AgentMsg::Heartbeat { .. }
            | AgentMsg::Sample(_)
            | AgentMsg::JobStarted { .. }
            | AgentMsg::StepStarted { .. }
            | AgentMsg::StepFinished { .. }
            | AgentMsg::JobFinished(_)
            | AgentMsg::JobSample { .. }
            | AgentMsg::Reject { .. }
            | AgentMsg::TermOutput { .. }
            | AgentMsg::TermExit { .. }
            | AgentMsg::TermList { .. }
            | AgentMsg::TermReplay { .. }
            | AgentMsg::Instance(_)
            | AgentMsg::Build { .. }
            | AgentMsg::UpgradeLog { .. }
            | AgentMsg::UpgradeDone { .. }
            | AgentMsg::Reach { .. }
            | AgentMsg::UpdatePrefs { .. }
            | AgentMsg::StreamFrame { .. }
            | AgentMsg::PortOpened { .. }
            | AgentMsg::PortData { .. }
            | AgentMsg::PortAck { .. }
            | AgentMsg::PortClose { .. }
            | AgentMsg::Caps { .. }
            | AgentMsg::Wire(_)
            | AgentMsg::NetDrops { .. }
            | AgentMsg::HarnessHandOver { .. }
            | AgentMsg::HarnessAdopted { .. }
            | AgentMsg::Unread { .. }
            | AgentMsg::LinkUnread { .. }) => return Ok(Carried::Nothing(m)),
        };
        Ok(Carried::Read(read))
    }
}

/// What the server's messages carry, as a runner reads it: each of [`ServerMsg`]'s variants that
/// carries JSON, with its payload's type in place of the JSON.
pub enum FromServer {
    HarnessOpen { session: String, spec: Box<crate::harness::session::Spec> },
    HarnessCmd { session: String, command: Box<crate::harness::model::Command> },
    /// `HarnessSend`: a message for agent session `session`'s inbox, for its epoch `epoch`.
    HarnessSend { session: String, epoch: u64, command: Box<crate::harness::model::Command> },
    Sync { msg: Box<crate::sync::wire::Msg>, raft: Vec<u8> },
    FrontDoor(Box<crate::frontdoor::Push>),
    AssignV2(Box<kiln_ci::wire::AssignmentV2>),
    SecretsFor { job: String, secrets: Vec<crate::proto::SecretFor> },
    /// `ReachAnswer`: the server's answer to a runner's `ReachAsk`.
    ReachAnswer { req: u64, answer: Value },
    Convo { msg: Box<crate::convo::feed::Msg>, data: Vec<u8> },
    StreamOpen { stream: String, session: String, spec: Box<crate::browser::stream::Spec> },
    /// `StreamControl`: what a viewer asks of stream `stream` (`crate::browser::stream::Stream::control`).
    StreamControl { stream: String, session: String, control: Value },
    StreamInput { stream: String, session: String, input: Vec<crate::browser::stream::Input> },
    /// `Preview`: what a person opens live from agent session `session` (`{"op": "look"}`).
    Preview { req: u64, session: String, ask: Value },
    Workspace { req: u64, session: String, ask: Box<crate::workspace::Ask> },
    Executor(crate::runner::pool::Ask),
    Undeploy(kiln_ci::wire::Undeploy),
    Cluster { msg: Box<crate::store::learner::Msg>, raft: Vec<u8> },
    /// `AgentLimit`: what a person set from the server for the most agent sessions this machine
    /// has at work at once (none: its config's holds).
    AgentLimit(crate::harness::limit::Asked),
    /// `Identity`: the certificate for an identity it asked for, or why there's none (docs/ci.md
    /// W27-R01).
    Identity(crate::proto::IdentityGiven),
    /// `Observe`: the server's word to its replica in observe's group, and what it carries.
    Observe { msg: Box<crate::observe::wire::Msg>, bytes: Vec<u8> },
    /// `Headroom`: this machine's headroom settings (docs/headroom.md).
    Headroom(Box<crate::headroom::Push>),
}

impl Carries for ServerMsg {
    type Read = FromServer;

    fn read(self) -> Result<Carried<Self, FromServer>, Box<Unreadable>> {
        const W: Wire = Wire::ServerPayload;
        let read = match self {
            ServerMsg::HarnessOpen { session, json } => match payload(W, "HarnessOpen", &json) {
                Ok(spec) => FromServer::HarnessOpen { session, spec },
                Err(e) => return Err(e.about(&session).waits(Waits::Opening(session))),
            },
            ServerMsg::HarnessCmd { session, json } => match payload(W, "HarnessCmd", &json) {
                Ok(command) => FromServer::HarnessCmd { session, command },
                Err(e) => return Err(e.about(&session)),
            },
            ServerMsg::HarnessSend { session, epoch, json } => match payload(W, "HarnessSend", &json) {
                Ok(command) => FromServer::HarnessSend { session, epoch, command },
                // Its id, when that much of it can be read: the session's inbox answers it.
                Err(e) => match message_of(&json) {
                    Some(message) => return Err(e.about(&session).waits(Waits::Sending { session, message, epoch })),
                    None => return Err(e.about(&session)),
                },
            },
            ServerMsg::Sync { json, raft } => FromServer::Sync { msg: payload(W, "Sync", &json)?, raft },
            ServerMsg::FrontDoor { json } => FromServer::FrontDoor(payload(W, "FrontDoor", &json)?),
            ServerMsg::AssignV2 { json } => FromServer::AssignV2(payload(W, "AssignV2", &json)?),
            ServerMsg::SecretsFor { job, json } => FromServer::SecretsFor { job, secrets: payload(W, "SecretsFor", &json)? },
            ServerMsg::ReachAnswer { req, json } => FromServer::ReachAnswer { req, answer: payload(W, "ReachAnswer", &json)? },
            ServerMsg::Convo { json, data } => FromServer::Convo { msg: payload(W, "Convo", &json)?, data },
            ServerMsg::StreamOpen { stream, session, json } => match payload(W, "StreamOpen", &json) {
                Ok(spec) => FromServer::StreamOpen { stream, session, spec },
                Err(e) => return Err(e.about(&session).waits(Waits::Stream(stream))),
            },
            ServerMsg::StreamControl { stream, session, json } => match payload(W, "StreamControl", &json) {
                Ok(control) => FromServer::StreamControl { stream, session, control },
                Err(e) => return Err(e.about(&session)),
            },
            ServerMsg::StreamInput { stream, session, json } => match payload(W, "StreamInput", &json) {
                Ok(input) => FromServer::StreamInput { stream, session, input },
                Err(e) => return Err(e.about(&session)),
            },
            ServerMsg::Preview { req, session, json } => match payload(W, "Preview", &json) {
                Ok(ask) => FromServer::Preview { req, session, ask },
                Err(e) => return Err(e.about(&session).waits(Waits::Asking(req))),
            },
            ServerMsg::Workspace { req, session, json } => match payload(W, "Workspace", &json) {
                Ok(ask) => FromServer::Workspace { req, session, ask },
                Err(e) => return Err(e.about(&session).waits(Waits::Asking(req))),
            },
            ServerMsg::Executor { json } => FromServer::Executor(payload(W, "Executor", &json)?),
            ServerMsg::Undeploy { json } => FromServer::Undeploy(payload(W, "Undeploy", &json)?),
            ServerMsg::Cluster { json, raft } => FromServer::Cluster { msg: payload(W, "Cluster", &json)?, raft },
            ServerMsg::AgentLimit { json } => FromServer::AgentLimit(payload(W, "AgentLimit", &json)?),
            ServerMsg::Identity { json } => FromServer::Identity(payload(W, "Identity", &json)?),
            ServerMsg::Observe { json, bytes } => FromServer::Observe { msg: payload(W, "Observe", &json)?, bytes },
            ServerMsg::Headroom { json } => FromServer::Headroom(payload(W, "Headroom", &json)?),
            m @ (ServerMsg::Welcome { .. }
            | ServerMsg::Refused { .. }
            | ServerMsg::Assign(_)
            | ServerMsg::Cancel { .. }
            | ServerMsg::Drain
            | ServerMsg::TermOpen { .. }
            | ServerMsg::TermInput { .. }
            | ServerMsg::TermResize { .. }
            | ServerMsg::TermClose { .. }
            | ServerMsg::TermAttach { .. }
            | ServerMsg::AiTranscript { .. }
            | ServerMsg::HarnessClose { .. }
            | ServerMsg::AiView { .. }
            | ServerMsg::HarnessFile { .. }
            | ServerMsg::Retire { .. }
            | ServerMsg::Upgrade { .. }
            | ServerMsg::HarnessTaskOutput { .. }
            | ServerMsg::DirList { .. }
            | ServerMsg::HarnessHistory { .. }
            | ServerMsg::JobTerminalOpen { .. }
            | ServerMsg::KeepRelease { .. }
            | ServerMsg::ProbeFacts
            | ServerMsg::Reach { .. }
            | ServerMsg::KeepExtend { .. }
            | ServerMsg::JobRerunStep { .. }
            | ServerMsg::JobExec { .. }
            | ServerMsg::HarnessPackage { .. }
            | ServerMsg::HarnessRelease { .. }
            | ServerMsg::UpdateAvailable { .. }
            | ServerMsg::Diagnose { .. }
            | ServerMsg::Repair { .. }
            | ServerMsg::HarnessFileRead { .. }
            | ServerMsg::StreamClose { .. }
            | ServerMsg::PortOpen { .. }
            | ServerMsg::PortData { .. }
            | ServerMsg::PortAck { .. }
            | ServerMsg::PortClose { .. }
            | ServerMsg::Received { .. }
            | ServerMsg::Caps { .. }
            | ServerMsg::Wire(_)
            | ServerMsg::Kept { .. }
            | ServerMsg::Unread { .. }
            | ServerMsg::ReachRefused { .. }) => return Ok(Carried::Nothing(m)),
        };
        Ok(Carried::Read(read))
    }
}

/// How a `HarnessCmd` the server sends rides the channel (`crate::wire::command_class`), from its
/// JSON: the server's own, read here like every payload. One with a message's id is refused at once
/// while its runner is away, never parked (CP-T3.2); one it can't read parks.
pub fn command_class(json: &str) -> super::Class {
    match payload::<crate::harness::model::Command>(Wire::ServerPayload, "HarnessCmd", json) {
        Ok(c) if crate::harness::registry::message_id(&c).is_some() => super::Class::Command(super::Away::Refuse),
        Ok(c) => super::command_class(&c),
        Err(_) => super::Class::Command(super::Away::Park),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Up;
    use crate::wire::unread::Page;
    use kiln_seams::clock::SimClock;

    /// A variant that carries JSON, for the tests: how to make it around its JSON, a payload it
    /// takes, and one of a kind it doesn't know, for a payload tagged by its kind.
    struct Case<M> {
        of: &'static str,
        make: fn(String) -> M,
        good: String,
        unknown: Option<&'static str>,
        /// It takes any JSON (`Value`): only what isn't JSON is refused.
        any: bool,
    }

    impl<M> Case<M> {
        /// What it refuses, and as what: JSON cut short, JSON of the wrong type, a kind it doesn't
        /// know.
        fn bad(&self) -> Vec<(String, Code)> {
            let mut bad = vec![("{".to_owned(), Code::Malformed)];
            if !self.any {
                bad.push(("\"x\"".to_owned(), Code::Malformed));
            }
            bad.extend(self.unknown.map(|u| (u.to_owned(), Code::Unknown)));
            bad
        }
    }

    fn case<M>(of: &'static str, make: fn(String) -> M, good: &impl Serialize, unknown: Option<&'static str>) -> Case<M> {
        Case { of, make, good: serde_json::to_string(good).unwrap_or_default(), unknown, any: false }
    }

    /// A payload of any JSON (`Value`).
    fn any<M>(of: &'static str, make: fn(String) -> M, good: &str) -> Case<M> {
        Case { of, make, good: good.to_owned(), unknown: None, any: true }
    }

    /// The runner's answers to the server's questions: read once matched to their question.
    const ANSWERS: [&str; 6] = ["AiTranscript", "AiView", "HarnessTaskOutput", "DirList", "HarnessHistory", "Answer"];

    /// Each of `AgentMsg`'s variants that carries JSON but the answers ([`ANSWERS`]).
    fn runner_cases() -> Vec<Case<AgentMsg>> {
        let event = crate::proto::JobEvent { seq: Some(1), event: kiln_ci::wire::JobEventV2::StepStarted { idx: 0, name: "build".into(), kind: "run".into(), action: None } };
        let battery = crate::os::battery::Battery { percent: 50, ac: true, charging: false, minutes: None };
        vec![
            case("AiSessions", |json| AgentMsg::AiSessions { json }, &Vec::<crate::aisessions::AiSession>::new(), None),
            case("Harness", |json| AgentMsg::Harness { session: "s1".into(), json }, &crate::harness::core::Update::Closed, Some(r#"{"t":"teleport"}"#)),
            case("HarnessInventory", |json| AgentMsg::HarnessInventory { json }, &crate::harness::host::Inventory::default(), None),
            case("Sync", |json| AgentMsg::Sync { json, raft: Vec::new() }, &crate::sync::wire::Msg::Release, Some(r#"{"kind":"teleport"}"#)),
            case("FrontDoorStatus", |json| AgentMsg::FrontDoorStatus { json }, &crate::frontdoor::Status::default(), None),
            case("JobEvent", |json| AgentMsg::JobEvent { job: "leg-1".into(), json }, &event, Some(r#"{"seq":2,"t":"teleport"}"#)),
            case("Facts", |json| AgentMsg::Facts { json }, &kiln_ci::facts::RunnerFacts::default(), None),
            case("ReachHello", |json| AgentMsg::ReachHello { json }, &crate::reach::relay::Announce::default(), None),
            case("ReachAsk", |json| AgentMsg::ReachAsk { req: 3, json }, &crate::reach::Question::Directory, Some(r#"{"op":"teleport"}"#)),
            case("Convo", |json| AgentMsg::Convo { json, data: Vec::new() }, &crate::convo::feed::Msg::Restore { store: "st".into() }, Some(r#"{"t":"teleport"}"#)),
            case("Power", |json| AgentMsg::Power { json }, &battery, None),
            case("HarnessFileData", |json| AgentMsg::HarnessFileData { req: 4, json, data: Vec::new() }, &crate::harness::files::Answer::default(), None),
            any("StreamEvent", |json| AgentMsg::StreamEvent { stream: "st1".into(), json }, r#"{"opened":{}}"#),
            case("Executor", |json| AgentMsg::Executor { json }, &crate::runner::pool::Said::default(), None),
            case("Undeployed", |json| AgentMsg::Undeployed { json }, &kiln_ci::wire::Undeployed::default(), None),
            case("Cluster", |json| AgentMsg::Cluster { json, raft: Vec::new() }, &crate::store::learner::Msg::Leave, Some(r#"{"t":"teleport"}"#)),
            case("WorkState", |json| AgentMsg::WorkState { json }, &kiln_ci::work::slots::WorkState::default(), None),
            case("AgentLimit", |json| AgentMsg::AgentLimit { json }, &crate::harness::limit::Held { max_sessions: 8, from: crate::harness::limit::Source::Config, config: Some(8) }, None),
            case("Identity", |json| AgentMsg::Identity { json }, &crate::proto::IdentityAsk { kind: "runner".into(), csr: "csr".into(), ..Default::default() }, None),
            case("Observe", |json| AgentMsg::Observe { json, bytes: Vec::new() }, &crate::observe::wire::Msg::Release, Some(r#"{"kind":"teleport"}"#)),
            any("HeadroomStatus", |json| AgentMsg::HeadroomStatus { json }, r#"{"settings":"f1","enabled":true}"#),
        ]
    }

    /// Each of `ServerMsg`'s variants that carries JSON.
    fn server_cases() -> Vec<Case<ServerMsg>> {
        let spec = crate::harness::session::Spec { kind: "claude".into(), ..Default::default() };
        let send = r#"{"type":"send","id":"m1","text":"hi"}"#;
        let mut cases = vec![
            case("HarnessOpen", |json| ServerMsg::HarnessOpen { session: "s1".into(), json }, &spec, None),
            case("HarnessCmd", |json| ServerMsg::HarnessCmd { session: "s1".into(), json }, &crate::harness::model::Command::Interrupt, Some(r#"{"type":"teleport"}"#)),
            Case { of: "HarnessSend", make: |json| ServerMsg::HarnessSend { session: "s1".into(), epoch: 1, json }, good: send.into(), unknown: Some(r#"{"type":"teleport","id":"m2"}"#), any: false },
            case("Sync", |json| ServerMsg::Sync { json, raft: Vec::new() }, &crate::sync::wire::Msg::Release, Some(r#"{"kind":"teleport"}"#)),
            case("FrontDoor", |json| ServerMsg::FrontDoor { json }, &crate::frontdoor::Push::default(), None),
            case("AssignV2", |json| ServerMsg::AssignV2 { json }, &kiln_ci::wire::AssignmentV2::default(), None),
            case("SecretsFor", |json| ServerMsg::SecretsFor { job: "leg-1".into(), json }, &Vec::<crate::proto::SecretFor>::new(), None),
            any("ReachAnswer", |json| ServerMsg::ReachAnswer { req: 5, json }, r#"{"runners":[]}"#),
            case("Convo", |json| ServerMsg::Convo { json, data: Vec::new() }, &crate::convo::feed::Msg::Restore { store: "st".into() }, Some(r#"{"t":"teleport"}"#)),
            any("StreamControl", |json| ServerMsg::StreamControl { stream: "st1".into(), session: "s1".into(), json }, r#"{"resize":{"width":390}}"#),
            case("StreamInput", |json| ServerMsg::StreamInput { stream: "st1".into(), session: "s1".into(), json }, &Vec::<crate::browser::stream::Input>::new(), Some(r#"[{"t":"teleport"}]"#)),
            any("Preview", |json| ServerMsg::Preview { req: 6, session: "s1".into(), json }, r#"{"op":"look"}"#),
            Case { of: "Workspace", make: |json| ServerMsg::Workspace { req: 7, session: "s1".into(), json }, good: r#"{"op":"git"}"#.into(), unknown: Some(r#"{"op":"teleport"}"#), any: false },
            case("Executor", |json| ServerMsg::Executor { json }, &crate::runner::pool::Ask::default(), None),
            case("Undeploy", |json| ServerMsg::Undeploy { json }, &kiln_ci::wire::Undeploy::default(), None),
            case("Cluster", |json| ServerMsg::Cluster { json, raft: Vec::new() }, &crate::store::learner::Msg::Leave, Some(r#"{"t":"teleport"}"#)),
            case("AgentLimit", |json| ServerMsg::AgentLimit { json }, &crate::harness::limit::Asked { max_sessions: Some(8) }, None),
            case("Identity", |json| ServerMsg::Identity { json }, &crate::proto::IdentityGiven { kind: "runner".into(), uri: "kiln://runner/m1".into(), ..Default::default() }, None),
            case("Observe", |json| ServerMsg::Observe { json, bytes: Vec::new() }, &crate::observe::wire::Msg::Release, Some(r#"{"kind":"teleport"}"#)),
            case("Headroom", |json| ServerMsg::Headroom { json }, &crate::headroom::Push::default(), None),
        ];
        cases.push(Case { of: "StreamOpen", make: |json| ServerMsg::StreamOpen { stream: "st1".into(), session: "s1".into(), json }, good: r#"{"url":"https://example.com"}"#.into(), unknown: None, any: false });
        cases
    }

    /// Whether `msg` came through as what it carries, read.
    fn read<M: Carries>(msg: M) -> Result<bool, Box<Unreadable>> {
        payload_of(msg).map(|c| matches!(c, Carried::Read(_)))
    }

    /// Every variant that carries JSON, on both sides: what it takes it reads into its type; what
    /// isn't JSON, or isn't of its type, is `malformed`; a kind it doesn't know is `unknown`, saying
    /// the kind. Each refusal says which variant it was.
    #[test]
    fn every_payload_is_read_into_its_type_or_refused_saying_why() -> anyhow::Result<()> {
        let (runner, server) = (runner_cases(), server_cases());
        // On main as the plan was made, 22 of AgentMsg's and 16 of ServerMsg's, T3's HarnessSend
        // since; W24's WorkState; a machine's agent limit, AgentLimit; W27's identities and
        // observe's group, Identity and Observe, each way; and headroom's, HeadroomStatus and
        // Headroom.
        anyhow::ensure!(runner.len() + ANSWERS.len() == 27 && server.len() == 21, "{} and {} variants", runner.len() + ANSWERS.len(), server.len());
        fn check<M: Carries>(cases: Vec<Case<M>>) -> anyhow::Result<()> {
            for c in cases {
                anyhow::ensure!(read((c.make)(c.good.clone())).map_err(|e| anyhow::anyhow!("{}: {e}", c.of))?, "{} came through as it was", c.of);
                for (json, code) in c.bad() {
                    match read((c.make)(json.clone())) {
                        Err(e) => {
                            anyhow::ensure!(e.of == c.of && e.code == code && e.what.starts_with(c.of) && !e.why.is_empty(), "{}: {json} refused as {e:?}", c.of);
                            if code == Code::Unknown {
                                anyhow::ensure!(e.what.ends_with("(teleport)"), "{}: the kind it doesn't know is said: {}", c.of, e.what);
                            }
                        }
                        Ok(_) => anyhow::bail!("{}: {json} was taken", c.of),
                    }
                }
            }
            Ok(())
        }
        check(runner)?;
        check(server)?;
        // The answers come through as they are: their question says what they're read as.
        for of in ANSWERS {
            let msg = match of {
                "AiTranscript" => AgentMsg::AiTranscript { req: 1, json: Some("{".into()) },
                "AiView" => AgentMsg::AiView { req: 1, json: Some("{".into()) },
                "HarnessTaskOutput" => AgentMsg::HarnessTaskOutput { req: 1, json: Some("{".into()) },
                "DirList" => AgentMsg::DirList { req: 1, json: "{".into() },
                "HarnessHistory" => AgentMsg::HarnessHistory { req: 1, json: Some("{".into()) },
                _ => AgentMsg::Answer { req: 1, json: Some("{".into()) },
            };
            match payload_of(msg) {
                Ok(Carried::Read(FromRunner::Answer(a))) => anyhow::ensure!(a.of == of && a.req == 1 && a.json.as_deref() == Some("{"), "{of}"),
                _ => anyhow::bail!("{of} wasn't passed on as an answer"),
            }
        }
        Ok(())
    }

    /// What it says of what it couldn't read never quotes what came: a secret's value, say.
    #[test]
    fn why_never_says_what_came() {
        let e = payload::<Vec<crate::proto::SecretFor>>(Wire::ServerPayload, "SecretsFor", r#"[{"name":"token","value":17}]"#).err();
        assert!(e.as_ref().is_some_and(|e| e.code == Code::Malformed && e.why.starts_with("invalid type: integer, expected")), "{e:?}");
        let e = payload::<u32>(Wire::ServerPayload, "Test", r#""hunter2""#).err();
        assert!(e.as_ref().is_some_and(|e| !e.why.contains("hunter2") && e.why == "invalid type: string, expected u32"), "{e:?}");
        let e = payload::<u32>(Wire::ServerPayload, "Test", "12 13").err();
        assert!(e.as_ref().is_some_and(|e| e.why.starts_with("not JSON: trailing characters")), "{e:?}");
    }

    /// A field a payload has that this build doesn't know is taken: found inside a kind of a
    /// tagged enum (where serde doesn't say), each a path once per message, and empty fields left
    /// out when written back aren't taken for unknown ones.
    #[test]
    fn a_field_this_build_doesnt_know_is_taken_and_found_where_serde_cant_say() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        let said = |id: &str, seq: u64| Event { id: id.into(), seq, at: 1_000, parent: None, by: None, body: Body::Assistant { text: "hi".into(), streaming: false } };
        let mut came = serde_json::to_value(Update::Events { events: vec![said("a", 1), said("b", 2)] })?;
        // A newer build's: a field in each event, one more in the first (an event's body is
        // flattened into it, a kind of a tagged enum), and an empty one.
        for (at, field, v) in [("/events/0", "stars", serde_json::json!(3)), ("/events/1", "stars", serde_json::json!(4)), ("/events/0", "tone", serde_json::json!("warm")), ("", "lens", Value::Null)] {
            let o = if at.is_empty() { came.as_object_mut() } else { came.pointer_mut(at).and_then(Value::as_object_mut) };
            o.ok_or_else(|| anyhow::anyhow!("no {at}"))?.insert(field.into(), v);
        }
        let before = crate::metrics::wire().ignored.get(&[Wire::TunnelDown.name()]);
        let u: Update = payload(Wire::TunnelDown, "Harness", &came.to_string()).map_err(|e| anyhow::anyhow!("{e}"))?;
        anyhow::ensure!(matches!(&u, Update::Events { events } if events.len() == 2), "{u:?}");
        let fields = unknown_fields(&came, &serde_json::to_value(&u)?);
        anyhow::ensure!(fields == ["events[].stars", "events[].tone"], "{fields:?}");
        let counted = crate::metrics::wire().ignored.get(&[Wire::TunnelDown.name()]) - before;
        anyhow::ensure!(counted == 2, "one for each field of the message: {counted}");
        Ok(())
    }

    /// Each wire's unknown field is said once an hour, however often it comes.
    #[test]
    fn each_unknown_field_is_said_once_an_hour() {
        let clock = SimClock::new();
        let mut ignored = Ignored::new(Arc::new(clock.clone()));
        let fields = vec!["new".to_owned(), "events[].tone".to_owned()];
        assert_eq!(ignored.note(Wire::McpReach, &fields), fields, "said the first time");
        clock.advance(Duration::from_secs(59 * 60));
        assert!(ignored.note(Wire::McpReach, &fields).is_empty(), "not again within the hour");
        let first = fields.get(..1).unwrap_or_default();
        assert_eq!(ignored.note(Wire::McpHttp, first), first.to_vec(), "another wire's are its own");
        clock.advance(Duration::from_secs(60));
        assert_eq!(ignored.note(Wire::McpReach, &fields), fields, "said again an hour on");
    }

    /// Any string at all as any payload: never a panic, always a value or a refusal saying why.
    #[test]
    fn any_string_is_read_as_a_payload_or_refused() {
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let (mut taken, mut refused) = (0u32, 0u32);
        fn tally<M: Carries>(msg: M, taken: &mut u32, refused: &mut u32) {
            match payload_of(msg) {
                Ok(_) => *taken += 1,
                Err(e) => {
                    assert!(!e.why.is_empty() && !e.what.is_empty(), "{e:?}");
                    *refused += 1;
                }
            }
        }
        let mut strings = |good: &str| -> Vec<String> {
            let mut out = Vec::new();
            for _ in 0..300 {
                let len = usize::try_from(next() % 48).unwrap_or(0);
                let bytes: Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
                out.push(String::from_utf8_lossy(&bytes).into_owned());
                // What it takes, cut, a byte of it changed, or something put in it.
                let mut chars: Vec<char> = good.chars().collect();
                let at = usize::try_from(next()).unwrap_or(0) % chars.len().max(1);
                match next() % 3 {
                    0 => chars.truncate(at),
                    1 => {
                        if let Some(c) = chars.get_mut(at) {
                            *c = char::from(b' ' + (next() % 95) as u8);
                        }
                    }
                    _ => chars.insert(at, ['{', '}', '[', ']', '"', ',', ':', '0', 'x'].get(usize::try_from(next() % 9).unwrap_or(0)).copied().unwrap_or('x')),
                }
                out.push(chars.into_iter().collect());
            }
            out
        };
        for c in runner_cases() {
            for s in strings(&c.good) {
                tally((c.make)(s), &mut taken, &mut refused);
            }
        }
        for c in server_cases() {
            for s in strings(&c.good) {
                tally((c.make)(s), &mut taken, &mut refused);
            }
        }
        assert!(taken > 0 && refused > 0, "{taken} taken, {refused} refused");
    }

    /// A send whose command its runner can't read, but whose id it can (agreed with T3): what waits
    /// on it is its session's inbox here, for its epoch (`Waits::Sending`), which answers it as it
    /// answers any message (`harness::host`'s tests: a repeat of one it took answered as one, one
    /// never taken refused for good). The server hears of the frame, for the session's page too.
    #[test]
    fn a_send_its_runner_cant_read_goes_to_its_sessions_inbox() -> anyhow::Result<()> {
        let unread = |json: &str| payload_of(ServerMsg::HarnessSend { session: "s1".into(), epoch: 4, json: json.into() });
        let Err(e) = unread(r#"{"type":"teleport","id":"m1","text":"hi"}"#) else { anyhow::bail!("a command this runner doesn't know was read") };
        anyhow::ensure!(matches!(&e.waits, Some(Waits::Sending { session, message, epoch: 4 }) if session == "s1" && message == "m1"), "{:?}", e.waits);
        let said = crate::agent::unread_payload(&e, Some(0));
        anyhow::ensure!(said.iter().any(|u| matches!(u, Up::Msg(AgentMsg::Unread { session: Some(s), .. }) if s == "s1")), "the server is told, for the session's page");
        // No id that can be read: nothing waits on it here.
        let Err(e) = unread(r#"{"type":"teleport"}"#) else { anyhow::bail!("a command this runner doesn't know was read") };
        anyhow::ensure!(e.waits.is_none(), "{:?}", e.waits);
        Ok(())
    }

    /// What the runner sends of a payload of the server's it couldn't read: word of it to a server
    /// that takes it, and what waits on it told why; the command it came in NACKed with the code
    /// the channel has for it.
    #[test]
    fn a_runner_says_what_it_couldnt_read_of_the_servers() -> anyhow::Result<()> {
        for c in server_cases() {
            for (json, code) in c.bad() {
                let Err(e) = payload_of((c.make)(json.clone())) else { anyhow::bail!("{}: {json} was taken", c.of) };
                anyhow::ensure!(e.code.refusal() == if code == Code::Unknown { super::super::channel::Refusal::Unsupported } else { super::super::channel::Refusal::Malformed }, "{}", c.of);
                let said = crate::agent::unread_payload(&e, Some(0));
                let told = said.iter().any(|u| matches!(u, Up::Msg(AgentMsg::Unread { wire: Wire::ServerPayload, what, code: k, .. }) if what.starts_with(c.of) && *k == code));
                anyhow::ensure!(told, "{}: {json}: the server is told", c.of);
                anyhow::ensure!(!crate::agent::unread_payload(&e, None).iter().any(|u| matches!(u, Up::Msg(AgentMsg::Unread { .. }))), "{}: a server from before isn't", c.of);
                let waited = match c.of {
                    "Preview" | "Workspace" => said.iter().any(|u| matches!(u, Up::Term(AgentMsg::Answer { json: Some(j), .. }) if j.contains("\"error\""))),
                    "StreamOpen" => said.iter().any(|u| matches!(u, Up::Stream(AgentMsg::StreamEvent { stream, json }) if stream == "st1" && json.contains("\"error\""))),
                    "HarnessOpen" => said.iter().any(|u| matches!(u, Up::Term(AgentMsg::Harness { session, .. }) if session == "s1")),
                    _ => said.len() == 1,
                };
                anyhow::ensure!(waited, "{}: what waits on it is told: {} sent", c.of, said.len());
            }
        }
        Ok(())
    }

    /// What a runner's payloads that can't be read come to on the server, for every variant that
    /// carries JSON: off the channel, `Unread` to a runner that takes it (and a question it asked
    /// answered so, the server's own asker told so); on it (a session's updates), the item's NACK.
    /// Either way counted, audited naming the runner, and kept for its page, and for the session's
    /// for a session's update.
    #[test]
    fn a_runners_payloads_that_cant_be_read_are_refused_to_it_and_noted() -> anyhow::Result<()> {
        use crate::hub::tests::{asking, asking_file, hear, hear_item, hub_with_runner, refused_rows};
        use crate::proto::{CAP_UNREAD, CAP_WIRE, LANE_CONTROL};
        use crate::wire::channel::{Frame, Refusal};
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let hub = hub_with_runner(Arc::new(clock.clone()), "fedora", CAP_UNREAD | CAP_WIRE).await?;
            let mut noted = 0u64;
            let told_last = |hub: &crate::hub::Hub| hub.sent.lock().iter().rev().find_map(|(p, m)| match m {
                ServerMsg::Unread { wire, what, code, .. } if *p == 1 => Some((*wire, what.clone(), *code)),
                _ => None,
            });
            for c in runner_cases() {
                for (json, code) in c.bad() {
                    // Far enough apart that each is told (16 a second at most).
                    clock.advance(Duration::from_millis(100));
                    let (asked, file) = (asking::<Value>(&hub, 3), asking_file(&hub, 4));
                    hear(&hub, 1, LANE_CONTROL, &(c.make)(json.clone())).await?;
                    noted += 1;
                    let told = told_last(&hub);
                    anyhow::ensure!(told.as_ref().is_some_and(|(w, what, k)| *w == Wire::RunnerPayload && what.starts_with(c.of) && *k == code), "{}: {json}: told {told:?}", c.of);
                    match c.of {
                        "ReachAsk" => {
                            let answered = hub.sent.lock().iter().any(|(_, m)| matches!(m, ServerMsg::ReachAnswer { req: 3, json } if json.contains("\"error\"")));
                            anyhow::ensure!(answered, "its question answered so");
                        }
                        "HarnessFileData" => anyhow::ensure!(matches!(file.await, Ok(Err(_))), "the server's asker told so"),
                        _ => {}
                    }
                    drop(asked);
                }
            }
            // An answer that can't be read as its question asks: its asker is told why, and the
            // runner is refused it.
            clock.advance(Duration::from_millis(100));
            let asked = asking::<crate::doctor::Machine>(&hub, 9);
            hear(&hub, 1, LANE_CONTROL, &AgentMsg::Answer { req: 9, json: Some("[1, 2]".into()) }).await?;
            noted += 1;
            anyhow::ensure!(matches!(asked.await, Ok(Err(why)) if why.starts_with("invalid type")), "the asker is told why");
            anyhow::ensure!(told_last(&hub).is_some_and(|(_, what, k)| what == "Answer" && k == Code::Malformed), "the runner is told");
            // A session's update on the channel that can't be read: NACKed, never answered taken.
            let mut seq = 0;
            for (json, code) in runner_cases().into_iter().find(|c| c.of == "Harness").map(|c| c.bad()).unwrap_or_default() {
                seq += 1;
                clock.advance(Duration::from_millis(100));
                hear_item(&hub, 1, "s:s1#a1", seq, &AgentMsg::Harness { session: "s1".into(), json: json.clone() }).await?;
                noted += 1;
                let nack = hub.sent.lock().iter().rev().find_map(|(_, m)| match m {
                    ServerMsg::Wire(Frame::Ack(has)) if has.stream == "s:s1#a1" => has.nacks.iter().find(|n| n.seq == seq).map(|n| n.code),
                    _ => None,
                });
                let want = if code == Code::Unknown { Refusal::Unsupported } else { Refusal::Malformed };
                anyhow::ensure!(nack == Some(want), "{json}: NACKed {nack:?}");
            }
            let kept = hub.refusals.lock().kept(&Page::Runner("fedora".into())).cloned().unwrap_or_default();
            anyhow::ensure!(kept.total() == noted, "kept for the runner's page: {} of {noted}", kept.total());
            let harness = runner_cases().into_iter().find(|c| c.of == "Harness").map(|c| c.bad().len()).unwrap_or_default();
            let session = hub.refusals.lock().kept(&Page::Session("s1".into())).map(crate::wire::unread::Kept::total);
            anyhow::ensure!(session == Some(2 * u64::try_from(harness)?), "kept for the session's page too: {session:?}");
            // One row a minute for each code (the rest coalesced into the next).
            let rows = refused_rows(&hub, "runner:fedora").await?;
            let firsts: Vec<(String, String)> = rows.iter().map(|(t, r, _)| (t.clone(), r.clone())).collect();
            anyhow::ensure!(firsts == vec![("runner_payload".to_owned(), "malformed".to_owned()), ("runner_payload".to_owned(), "unknown".to_owned())], "{rows:?}");
            Ok(())
        })
    }

    /// A live page's event that's JSON but not one of `StreamSaid`'s kinds goes to no viewer, and
    /// is refused to its runner as a payload that can't be read is: `ServerMsg::Unread` to one that
    /// takes it, noted for its page.
    #[test]
    fn a_live_pages_event_of_a_kind_it_doesnt_know_is_refused_to_its_runner() -> anyhow::Result<()> {
        use crate::hub::tests::{hear, hub_with_runner};
        use crate::proto::{CAP_UNREAD, CAP_WIRE, LANE_CONTROL};
        crate::rt::block_on(async {
            let hub = hub_with_runner(Arc::new(SimClock::new()), "fedora", CAP_UNREAD | CAP_WIRE).await?;
            hear(&hub, 1, LANE_CONTROL, &AgentMsg::StreamEvent { stream: "st1".into(), json: r#"{"hologram":{}}"#.into() }).await?;
            let told = hub.sent.lock().iter().any(|(p, m)| matches!(m, ServerMsg::Unread { wire: Wire::RunnerPayload, what, code: Code::Unknown, .. } if *p == 1 && what == "StreamEvent hologram"));
            anyhow::ensure!(told, "the runner is told: {:?}", hub.sent.lock());
            let kept = hub.refusals.lock().kept(&Page::Runner("fedora".into())).map(crate::wire::unread::Kept::total);
            anyhow::ensure!(kept == Some(1), "kept for the runner's page: {kept:?}");
            Ok(())
        })
    }

    /// No handler reads JSON of its own: in the hub, the runner and the harness's host, a read of
    /// what came on a wire is [`payload`]'s, and every other read is of something of the machine's
    /// own (a file, a socket of its own, what it's about to send), each named here.
    #[test]
    fn no_handler_reads_a_payload_of_its_own() {
        const OWN: [(&str, &str); 7] = [
            ("serde_json::from_slice(&json).ok()?", "a session handed back by this machine's newer build, on a socket of its own"),
            ("serde_json::from_value(cut_value(v, budget))", "its own event, cut to fit a frame before it's sent"),
            ("let handed: Vec<HandedTerm> = match serde_json::from_slice(&raw)", "terminals the runner before it handed over, in a file"),
            ("Ok(raw) => serde_json::from_slice(&raw).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),", "its records of hand-overs, in files of its own"),
            ("serde_json::from_slice(&raw).unwrap_or_else(|e| {", "the sessions it handed itself, and its record of its sessions, in its files"),
            ("let state: super::model::State = kept.state.as_deref().and_then(|s| serde_json::from_str(s).ok())?;", "what its conversation store kept"),
            ("let mut events: Vec<super::model::Event> = kept.events.iter().filter_map(|e| serde_json::from_str(&e.body).ok()).collect();", "what its conversation store kept"),
        ];
        const READS: [&str; 7] = ["serde_json::from_str", "serde_json::from_slice", "serde_json::from_value", "serde_json::from_reader", "feed::decode(", "learner::decode(", "wire::decode("];
        let sources = [("hub.rs", include_str!("../hub.rs")), ("agent.rs", include_str!("../agent.rs")), ("harness/host.rs", include_str!("../harness/host.rs"))];
        let mut found = Vec::new();
        for (file, text) in sources {
            // Up to its tests.
            let code = text.split("\n#[cfg(test)]\nmod tests").next().unwrap_or(text).split("\n#[cfg(test)]\npub(crate) mod tests").next().unwrap_or(text);
            for (n, line) in code.lines().enumerate() {
                if READS.iter().any(|r| line.contains(r)) && !OWN.iter().any(|(own, _)| line.contains(own)) {
                    found.push(format!("{file}:{}: {}", n + 1, line.trim()));
                }
            }
        }
        assert!(found.is_empty(), "reads of a payload outside wire::payload: {} calls\n{}", found.len(), found.join("\n"));
    }
}
