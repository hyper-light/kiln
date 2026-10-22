//! The server's view of every agent session, wherever it runs: its own (`local`) and each
//! runner's. A mirror per session (the latest state, meta and timeline, as JSON) serves any number
//! of viewers on any device; their commands go to the machine that runs the session.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::core::{MAX_EVENTS, Update};
use super::files;
use super::kept::{Job, Keeper, Kept};
use super::host::{Host, Inventory, Limits};
use super::kinds::KindInfo;
use super::model::{Command, State, Status};
use super::session::{Info, MAX_NAME, Spec, SubagentBrief};
use crate::hub::Hub;
use crate::proto::{LANE_TERM, ServerMsg};
use crate::rt::Broadcast;

/// Frames buffered per viewer before a slow one is resynced.
const FANOUT: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local,
    Machine(String),
}

impl Location {
    pub fn target(&self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::Machine(m) => format!("agent:{m}"),
        }
    }

    pub fn of_target(target: &str) -> Self {
        match target.strip_prefix("agent:") {
            Some(m) => Self::Machine(m.to_owned()),
            None => Self::Local,
        }
    }
}

struct Mirror {
    info: Info,
    /// The last name of its own its machine reported for it (empty: none yet).
    reported: String,
    location: Location,
    meta: Value,
    state: Value,
    /// The state as its viewers last had it sent (what a state patch is made against).
    sent: Value,
    changes: Value,
    events: Vec<Value>,
    index: HashMap<String, usize>,
    out: Broadcast<Frame>,
}

impl Mirror {
    fn new(info: Info, location: Location) -> Self {
        Self {
            info,
            reported: String::new(),
            location,
            meta: Value::Null,
            state: Value::Null,
            sent: Value::Null,
            changes: Value::Null,
            events: Vec::new(),
            index: HashMap::new(),
            out: Broadcast::new(FANOUT),
        }
    }

    /// The state as a frame for its viewers, with what changed in it since the last one.
    fn state_frame(&mut self) -> Option<Frame> {
        if self.out.receiver_count() == 0 {
            // The next viewer starts from a snapshot, not the last viewer's patch baseline.
            self.sent = Value::Null;
            return None;
        }
        let text = json!({"t": "state", "state": self.state}).to_string();
        let patch = state_patch(&self.sent, &self.state);
        self.sent.clone_from(&self.state);
        Some(Frame { text: Arc::from(text), patch: patch.map(Arc::from) })
    }

    fn reindex(&mut self) {
        self.index = self.events.iter().enumerate().filter_map(|(i, e)| Some((e.get("id")?.as_str()?.to_owned(), i))).collect();
    }

    fn upsert(&mut self, e: Value) {
        let Some(id) = e.get("id").and_then(Value::as_str).map(str::to_owned) else { return };
        if let Some(slot) = self.index.get(&id).and_then(|i| self.events.get_mut(*i)) {
            *slot = e;
            return;
        }
        self.index.insert(id, self.events.len());
        self.events.push(e);
        if self.events.len() > MAX_EVENTS + MAX_EVENTS / 4 {
            let drop = self.events.len() - MAX_EVENTS;
            self.events.drain(..drop);
            self.reindex();
        }
    }

    fn append(&mut self, id: &str, text: &str) {
        if let Some(e) = self.index.get(id).and_then(|i| self.events.get_mut(*i))
            && let Some(Value::String(s)) = e.get_mut("text")
        {
            s.push_str(text);
        }
    }

    /// The frames that bring a new viewer to the current state; `files`, what's kept of the files
    /// its messages mention (`Keeper::frame`), goes ahead of the events.
    fn snapshot(&self, files: Option<String>) -> Vec<String> {
        let mut out = vec![json!({"t": "open", "info": self.info}).to_string()];
        if !self.meta.is_null() {
            out.push(json!({"t": "meta", "meta": self.meta}).to_string());
        }
        if !self.state.is_null() {
            out.push(json!({"t": "state", "state": self.state}).to_string());
        }
        if !self.changes.is_null() {
            out.push(json!({"t": "changes", "files": self.changes}).to_string());
        }
        out.extend(files);
        let mut batch: Vec<&Value> = Vec::new();
        let mut size = 0usize;
        for e in &self.events {
            let n = e.to_string().len();
            if size + n > 256 * 1024 && !batch.is_empty() {
                out.push(json!({"t": "events", "events": batch}).to_string());
                batch = Vec::new();
                size = 0;
            }
            size += n;
            batch.push(e);
        }
        if !batch.is_empty() {
            out.push(json!({"t": "events", "events": batch}).to_string());
        }
        out
    }
}

/// How a command fared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    Ok,
    NoSession,
    /// The session's machine is not connected right now.
    Unreachable,
}

/// What a viewer gets on attach: the catch-up frames and the live ones after them.
pub struct Attachment {
    pub frames: Vec<String>,
    pub live: async_broadcast::Receiver<Frame>,
}

/// A live frame for a session's viewers. A state comes as well as only what changed since the
/// last one (`patch`), for a viewer that takes that: during a turn its usage and activity change
/// all the time, its skills, plugins and servers seldom.
#[derive(Clone)]
pub struct Frame {
    pub text: Arc<str>,
    pub patch: Option<Arc<str>>,
}

impl Frame {
    pub fn of(text: String) -> Self {
        Self { text: Arc::from(text), patch: None }
    }

    /// The frame for a viewer that takes state patches, or one that doesn't.
    pub fn for_viewer(&self, patches: bool) -> &str {
        match &self.patch {
            Some(p) if patches => p,
            _ => &self.text,
        }
    }
}

/// `{"t":"state_patch","set":{..},"gone":[..]}`: the top-level fields of `now` that differ from
/// `was`, and those it no longer has. None when there is nothing to patch (no state before).
fn state_patch(was: &Value, now: &Value) -> Option<String> {
    let (Value::Object(was), Value::Object(now)) = (was, now) else { return None };
    let set: serde_json::Map<String, Value> = now.iter().filter(|(k, v)| was.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let gone: Vec<&String> = was.keys().filter(|k| !now.contains_key(*k)).collect();
    Some(if gone.is_empty() { json!({"t": "state_patch", "set": set}) } else { json!({"t": "state_patch", "set": set, "gone": gone}) }.to_string())
}

/// A file uploaded for a message, kept briefly until the message is sent.
struct Upload {
    name: String,
    mime: String,
    data: bytes::Bytes,
    at: std::time::Instant,
}

/// Uploads kept at most this long, and this much in all.
const UPLOAD_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);
const UPLOADS_MAX: usize = 512 << 20;
/// Pieces a file is sent to a machine in (lane frames are bounded).
const FILE_CHUNK: usize = 512 * 1024;

/// Restoring the conversation store from a mirror's copy of it (`docs/conversations.md`,
/// "Mirrors"): `GET /api/conversations/restore`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Restore {
    Idle,
    /// `from` was asked for its copy.
    Asked { from: String, at: i64 },
    /// `from` is sending its copy `store`.
    Copying { from: String, store: String, at: i64 },
    /// Done: `sessions` came back.
    Done { from: String, sessions: usize, at: i64 },
    Failed { from: String, error: String, at: i64 },
}

/// A mirror asked for its copy that hasn't begun sending it after this long isn't answering.
const RESTORE_ANSWER_MS: i64 = 120_000;

pub struct Registry {
    mirrors: Mutex<HashMap<String, Mirror>>,
    uploads: Mutex<HashMap<String, Upload>>,
    /// What each machine offers ("local" included).
    kinds: Mutex<HashMap<String, Vec<KindInfo>>>,
    /// The server's own sessions.
    pub local: Arc<Host>,
    /// The server's conversation store (docs/conversations.md), when it's on.
    pub convo: Option<crate::convo::store::Store>,
    /// What runners' feeds send into it (`crate::convo::feed`), and what to answer them.
    pub feed: Option<crate::convo::feed::Intake>,
    feed_out: Option<async_channel::Receiver<crate::convo::feed::Out>>,
    /// The runners that keep a copy of it (`crate::convo::mirror`), and what to send them.
    pub copies: Option<Arc<crate::convo::mirror::Mirrors>>,
    copies_out: Option<async_channel::Receiver<(hyper_transport::PeerId, String, Vec<u8>)>>,
    /// Every store, as the server sees it: the System page's (`super::stores`).
    pub board: crate::convo::board::Board,
    local_rx: async_channel::Receiver<(String, Update)>,
    /// Where `kiln spawn` in the server's own terminals reaches `local`.
    local_sock: Mutex<Option<String>>,
    /// The shell pid of each of the server's own terminals (their directory and environment are
    /// where agents started "in this shell" begin).
    local_shells: Mutex<HashMap<String, u32>>,
    dir: std::path::PathBuf,
    /// The server machine's transcripts, for its past sessions (made on first use).
    scanner: Arc<Mutex<Option<crate::aisessions::Scanner>>>,
    /// Names people gave sessions, by id (the "rename" control), kept in `names.json`: a session
    /// takes its name here at once, whatever its machine's runner knows of names (one from before
    /// kiln named sessions never reports it), and keeps it across restarts.
    names: Mutex<HashMap<String, String>>,
    /// Sessions kiln doesn't keep going when they stop part-way (`goals::keep`; the "keep_going"
    /// control), kept in `keep-off.json` so it holds across restarts.
    keep_off: Mutex<std::collections::HashSet<String>>,
    /// Each session's latest epoch (`State::epoch`) seen, those that went on on another machine at
    /// least once, kept in `epochs.json`: a machine that announces an older copy lets go of it.
    epochs: Mutex<HashMap<String, u64>>,
    /// Restoring the conversation store from a mirror's copy of it.
    restore: Mutex<Restore>,
    /// The mirrors to ask for their copy as they connect: the store was found empty at start
    /// while the server had mirrors (it was lost, the rest wasn't).
    restore_auto: Mutex<Vec<String>>,
    /// Files finished messages mention, kept so they still show after they go (`super::kept`).
    pub keeper: Keeper,
    /// Pages agents show in their conversations (`super::visuals`).
    pub visuals: super::visuals::Visuals,
    /// Pages streamed from agents' machines, relayed to their viewers (`super::streams`).
    pub streams: super::streams::Relay,
    /// Streams to their viewers over WebRTC, when the server has it (`super::webrtc`).
    pub webrtc: super::webrtc::Webrtc,
    /// Connections to ports of agents' machines, and the apps opened on people's devices over
    /// them (`super::opened`).
    pub ports: super::opened::Tunnels,
    pub opened: super::opened::Openings,
    /// Tests: what every session started here runs instead of its harness (a program that isn't
    /// there, so no harness ever starts, whatever this machine has installed).
    #[cfg(test)]
    test_program: Mutex<Option<String>>,
}

/// The name a session's machine reports as its own: none when it's empty or its harness's name.
fn own_name<'a>(name: &'a str, kind_name: &str) -> Option<&'a str> {
    let n = name.trim();
    (!n.is_empty() && !n.eq_ignore_ascii_case(kind_name.trim())).then_some(n)
}

/// Whose name a session shows once its machine reports `reported` in a state.
#[derive(Debug, PartialEq, Eq)]
enum Naming<'a> {
    /// A name of its own the machine gives anew (not the `last` it gave): it's kept here too.
    Adopt(&'a str),
    /// The name kept here: the machine has none of its own (a runner from before names, or one
    /// upgraded since), or hasn't taken the one given here yet.
    Keep,
    /// The machine's: nothing is kept here.
    Theirs,
}

fn naming<'a>(reported: &'a str, last: &str, kind_name: &str, kept: bool) -> Naming<'a> {
    match own_name(reported, kind_name) {
        Some(own) if own != last => Naming::Adopt(own),
        _ if kept => Naming::Keep,
        _ => Naming::Theirs,
    }
}

impl Registry {
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self::with(dir, &crate::config::Conversations::default(), crate::rt::clock())
    }

    /// The registry, its store in `dir`, its mirrors as `conversations` says, the server's own
    /// sessions' host on `clock` (the hub's).
    pub fn with(dir: std::path::PathBuf, conversations: &crate::config::Conversations, clock: Arc<dyn kiln_seams::Clock>) -> Self {
        let (tx, rx) = async_channel::bounded(8192);
        // Every session's conversation, on every machine, kept on disk as it comes in
        // (docs/conversations.md): the server's own sessions' too, as they pass through `ingest`.
        let convo = if conversations.off { None } else { crate::convo::open(&dir, conversations.upkeep()) };
        // No record goes before kiln's store of bytes has it: held back entirely until the archive
        // of transcripts (`super::archive`) says how far it has come.
        if let Some(c) = &convo {
            c.hold_prune(0);
        }
        // Runners that keep a store send it here; the server keeps their sessions from that. What
        // a mirror sends back to restore the store from isn't sent to it again.
        let echoes = std::sync::Arc::new(crate::convo::feed::Echoes::default());
        let (feed, feed_out) = match convo.as_ref().map(|st| crate::convo::feed::Intake::start(st.clone(), crate::convo::feed::Brings::TheirMachine, Some(echoes.clone()))) {
            Some(Ok((i, o))) => (Some(i), Some(o)),
            Some(Err(e)) => {
                tracing::warn!(error = %format!("{e:#}"), "runners' conversation stores won't reach the server's");
                (None, None)
            }
            None => (None, None),
        };
        let (copies, copies_out) = match convo.clone().filter(|_| conversations.mirrors > 0) {
            Some(st) => {
                let (m, out) = crate::convo::mirror::Mirrors::start(st, conversations.mirrors, conversations.mirror_on.clone(), dir.join("mirrors.json"), Some(echoes));
                (Some(m), Some(out))
            }
            None => (None, None),
        };
        let restore_auto = match (&copies, &convo) {
            (Some(m), Some(st)) if st.is_empty().unwrap_or(false) => m.chosen_machines(),
            _ => Vec::new(),
        };
        if !restore_auto.is_empty() {
            tracing::info!(mirrors = ?restore_auto, "the conversation store is empty: it is restored from a mirror's copy once one connects");
        }
        let local = Host::with_clock(tx, Some(dir.join("local.json")), Limits::default(), clock);
        // The server's own agents' workspaces of their own are kept beside their record.
        crate::workspace::slates::init(&dir);
        let keeper = Keeper::new(&dir);
        let visuals = super::visuals::Visuals::new(&dir);
        Self {
            mirrors: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            kinds: Mutex::new(HashMap::new()),
            local,
            convo,
            feed,
            feed_out,
            copies,
            copies_out,
            board: crate::convo::board::Board::default(),
            local_rx: rx,
            local_sock: Mutex::new(None),
            local_shells: Mutex::new(HashMap::new()),
            names: Mutex::new(std::fs::read(dir.join("names.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            keep_off: Mutex::new(std::fs::read(dir.join("keep-off.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            epochs: Mutex::new(std::fs::read(dir.join("epochs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            dir,
            scanner: Arc::new(Mutex::new(None)),
            restore: Mutex::new(Restore::Idle),
            restore_auto: Mutex::new(restore_auto),
            keeper,
            visuals,
            streams: super::streams::Relay::default(),
            webrtc: super::webrtc::Webrtc::default(),
            ports: super::opened::Tunnels::default(),
            opened: super::opened::Openings::default(),
            #[cfg(test)]
            test_program: Mutex::new(None),
        }
    }

    /// Tests: sessions started here from now on run `program` instead of their harness.
    #[cfg(test)]
    pub fn set_test_program(&self, program: &str) {
        *self.test_program.lock() = Some(program.to_owned());
    }

    /// Where restoring the conversation store from a mirror's copy stands.
    pub fn restore_view(&self) -> Restore {
        let mut r = self.restore.lock();
        if let Restore::Asked { from, at } = &*r
            && crate::util::now_ms() - at > RESTORE_ANSWER_MS
        {
            *r = Restore::Failed { from: from.clone(), error: "no answer: its kiln may be too old to send its copy back".into(), at: crate::util::now_ms() };
        }
        r.clone()
    }

    /// Ask `machine`, a mirror, for its copy of the conversation store, to restore this one from:
    /// what the copy brings fills in what the store lacks (`docs/conversations.md`, "Mirrors").
    pub fn restore_from(&self, hub: &Arc<Hub>, machine: &str) -> anyhow::Result<()> {
        let st = self.convo.as_ref().ok_or_else(|| anyhow::anyhow!("the conversation store is off"))?;
        anyhow::ensure!(self.feed.is_some(), "this server takes no conversation feeds");
        let peer = hub.instances.lock().newest_with(machine, crate::proto::CAP_CONVO).ok_or_else(|| anyhow::anyhow!("{machine} isn't connected, or keeps no conversation store"))?;
        if matches!(self.restore_view(), Restore::Asked { .. } | Restore::Copying { .. }) {
            anyhow::bail!("a restore is already under way");
        }
        *self.restore.lock() = Restore::Asked { from: machine.to_owned(), at: crate::util::now_ms() };
        self.restore_auto.lock().clear();
        tracing::info!(%machine, "asked a mirror for its copy of the conversation store, to restore this one from");
        let json = crate::convo::feed::encode(&crate::convo::feed::Msg::Restore { store: st.id().to_owned() });
        let h = Arc::clone(hub);
        crate::rt::spawn(async move {
            if let Ok(b) = crate::proto::encode(&crate::proto::ServerMsg::Convo { json, data: Vec::new() }) {
                h.net.send(peer, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await;
            }
        });
        Ok(())
    }

    /// Mirror `machine` begins sending back its copy `store`: whether it was asked for.
    pub fn restore_accepts(&self, machine: &str, store: &str) -> bool {
        let mut r = self.restore.lock();
        match &*r {
            Restore::Asked { from, .. } if from == machine => {
                *r = Restore::Copying { from: machine.to_owned(), store: store.to_owned(), at: crate::util::now_ms() };
                true
            }
            // Said again (it asks again after a silence).
            Restore::Copying { from, store: s, .. } => from == machine && s == store,
            _ => false,
        }
    }

    /// Mirror `machine` has sent back its copy `store` whole (an empty `store`: it has none):
    /// the sessions it brought back are listed, away, on the machines they ran on.
    pub fn restore_done(&self, hub: &Arc<Hub>, machine: &str, store: &str) {
        let state = self.restore.lock().clone();
        match state {
            Restore::Asked { from, .. } if from == machine && store.is_empty() => {
                *self.restore.lock() = Restore::Failed { from, error: format!("{machine} keeps no copy of the conversation store"), at: crate::util::now_ms() };
            }
            Restore::Copying { from, store: s, .. } if from == machine && s == store => {
                let (h, st) = (Arc::clone(hub), self.convo.clone());
                crate::rt::spawn(async move {
                    let ended = match st {
                        Some(st) => crate::rt::blocking(move || st.end_copy()).await.and_then(|r| r),
                        None => Err(anyhow::anyhow!("the conversation store is off")),
                    };
                    let at = crate::util::now_ms();
                    match ended {
                        Ok(sessions) => {
                            tracing::info!(machine = %from, sessions, "the conversation store was restored from a mirror's copy");
                            Self::list_away(&h).await;
                            *h.harness.restore.lock() = Restore::Done { from, sessions, at };
                        }
                        Err(e) => *h.harness.restore.lock() = Restore::Failed { from, error: format!("{e:#}"), at },
                    }
                });
            }
            _ => {}
        }
    }

    /// Runner `machine` keeping a store connected: asked for its copy when the store was found
    /// empty at start and it is one of the server's mirrors.
    pub fn restore_due(&self, hub: &Arc<Hub>, machine: &str) {
        let due = self.restore_auto.lock().iter().any(|m| m == machine);
        if due
            && matches!(self.restore_view(), Restore::Idle)
            && let Err(e) = self.restore_from(hub, machine)
        {
            tracing::warn!(%machine, error = %format!("{e:#}"), "couldn't ask a mirror for its copy of the conversation store");
        }
    }

    fn save_names(&self) {
        let names = self.names.lock().clone();
        if let Err(e) = self.keep_json("names.json", &names) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep the agents' names");
        }
    }

    /// `value` as `file` in the registry's folder, whole or not at all.
    fn keep_json(&self, file: &str, value: &impl serde::Serialize) -> anyhow::Result<()> {
        let path = self.dir.join(file);
        std::fs::create_dir_all(&self.dir)?;
        let tmp = path.with_extension("json.part");
        std::fs::write(&tmp, serde_json::to_vec(value)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Whether kiln keeps a session going when it stops part-way (the "keep_going" control, from
    /// any device): kept here, so it holds across restarts whatever its machine's runner knows.
    fn set_keep_going(&self, hub: &Hub, id: &str, on: bool) {
        let off: Vec<String> = {
            let mut g = self.keep_off.lock();
            if on {
                g.remove(id);
            } else {
                g.insert(id.to_owned());
            }
            g.iter().cloned().collect()
        };
        if let Err(e) = self.keep_json("keep-off.json", &off) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep which agents kiln doesn't keep going");
        }
        let info = {
            let mut g = self.mirrors.lock();
            let Some(m) = g.get_mut(id) else { return };
            m.info.keep_off = !on;
            m.info.clone()
        };
        hub.emit(&json!({"type": "harness", "info": info}));
    }

    /// A session is given a name here (the "rename" control from any device; empty gives it its
    /// harness's name back): its page and every list show it at once.
    fn rename(&self, hub: &Hub, id: &str, value: Option<&Value>) {
        let name: String = value.and_then(Value::as_str).unwrap_or("").trim().chars().take(MAX_NAME).collect();
        {
            let mut g = self.names.lock();
            if name.is_empty() {
                g.remove(id);
            } else {
                g.insert(id.to_owned(), name.clone());
            }
        }
        self.save_names();
        let info = {
            let mut g = self.mirrors.lock();
            let Some(m) = g.get_mut(id) else { return };
            m.info.name = if name.is_empty() { m.info.kind_name.clone() } else { name };
            if let Some(o) = m.state.as_object_mut() {
                o.insert("name".into(), json!(m.info.name));
                if let Some(frame) = m.state_frame() {
                    m.out.send(frame);
                }
            }
            m.info.clone()
        };
        hub.emit(&json!({"type": "harness", "info": info}));
    }

    /// Environment for one of the server's own terminals, so `kiln spawn` there works.
    pub fn terminal_env(&self, terminal: &str) -> Vec<(&'static str, String)> {
        let mut env = vec![("KILN_TERM_SESSION", terminal.to_owned())];
        if let Some(sock) = self.local_sock.lock().clone() {
            env.push(("KILN_HARNESS_SOCK", sock));
        }
        env
    }

    /// One of the server's own terminals started (its shell runs as `pid`).
    pub fn note_shell(&self, terminal: &str, pid: Option<u32>) {
        let mut g = self.local_shells.lock();
        if let Some(pid) = pid {
            g.insert(terminal.to_owned(), pid);
        }
        // Forget shells that are gone.
        if g.len() > 256 {
            g.retain(|_, p| crate::os::pid_alive(*p));
        }
    }

    /// The server's own sessions' updates, into the mirrors (supervised; restartable).
    /// Sessions of machines that are away, as the server's store has them (the server started
    /// again since they went away): listed as away, so they can be moved on, until their machine
    /// says what it has.
    pub fn restore_away(&self, hub: &Hub, away: Vec<(crate::convo::store::Unended, Vec<crate::convo::rows::EventRow>)>) {
        let mut infos = Vec::new();
        {
            let mut g = self.mirrors.lock();
            for (u, events) in away {
                let Ok(mut info) = serde_json::from_str::<Info>(&u.info) else { continue };
                // The machine whose feed brought it, else the one its updates came from.
                let Some(machine) = u.machine.clone().or_else(|| info.target.strip_prefix("agent:").map(str::to_owned)) else { continue };
                info.target = format!("agent:{machine}");
                if g.contains_key(&u.id) || hub.agents.lock().get(&machine).is_some_and(|a| a.connected) {
                    continue;
                }
                info.status = Status::Disconnected;
                let state: Value = u.state.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or(Value::Null);
                // Old stored list entries predate native subagent briefs. Their last task
                // snapshot still supplies those read-only children while the machine is away.
                if let Some(tasks) = state.get("tasks").and_then(|v| serde_json::from_value::<Vec<super::model::Task>>(v.clone()).ok()) {
                    info.subagents = SubagentBrief::of(&tasks);
                }
                let mut m = Mirror::new(info.clone(), Location::Machine(machine));
                m.meta = u.meta.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or(Value::Null);
                m.state = state;
                if let Some(o) = m.state.as_object_mut() {
                    o.insert("status".into(), json!("disconnected"));
                }
                // A conversation cleared since: what came before it is history, not its timeline.
                let cleared = m.state.get("cleared").and_then(Value::as_u64).unwrap_or(0);
                for e in events.into_iter().filter(|e| e.seq >= cleared) {
                    if let Ok(v) = serde_json::from_str::<Value>(&e.body) {
                        m.upsert(v);
                    }
                }
                g.insert(u.id, m);
                infos.push(info);
            }
        }
        if !infos.is_empty() {
            tracing::info!(sessions = infos.len(), "agent sessions on machines that are away, from the conversation store");
        }
        for info in infos {
            hub.emit(&json!({"type": "harness", "info": info}));
        }
    }

    /// Lists the sessions of machines that are away, from the store (the last month's): after a
    /// start, and after a restore from a mirror's copy.
    pub async fn list_away(hub: &Arc<Hub>) {
        let Some(st) = hub.harness.convo.clone() else { return };
        const AWAY_DAYS: i64 = 30;
        const AWAY_EVENTS: usize = 200;
        let since = crate::util::now_ms() - AWAY_DAYS * 24 * 3600 * 1000;
        let read = crate::rt::blocking(move || -> anyhow::Result<Vec<(crate::convo::store::Unended, Vec<crate::convo::rows::EventRow>)>> {
            let mut out = Vec::new();
            for u in st.unended(since)? {
                let events = st.events(&u.id, None, AWAY_EVENTS)?;
                out.push((u, events));
            }
            Ok(out)
        })
        .await;
        match read {
            Ok(Ok(away)) => hub.harness.restore_away(hub, away),
            Ok(Err(e)) | Err(e) => tracing::warn!(error = %format!("{e:#}"), "sessions of machines that are away couldn't be read from the conversation store"),
        }
    }

    pub async fn run_local(hub: Arc<Hub>) -> anyhow::Result<()> {
        static RESTORED: std::sync::Once = std::sync::Once::new();
        // Sessions of machines that are away: listed from the store.
        Self::list_away(&hub).await;
        let rx = hub.harness.local_rx.clone();
        let offered = crate::rt::blocking(super::kinds::offered).await.unwrap_or_default();
        hub.harness.kinds.lock().insert("local".to_owned(), offered);
        RESTORED.call_once(|| {
            // Files finished messages mention, kept as they come.
            crate::rt::spawn(Keeper::run(hub.clone()));
            hub.harness.local.restore_from(hub.harness.convo.as_ref());
            // The server's own sessions' harness records, followed into its store.
            if let Some(st) = &hub.harness.convo {
                let local = hub.harness.local.clone();
                if let Err(e) = crate::convo::record::Recorder::start(st.clone(), move || local.record_sources()) {
                    tracing::warn!(error = %format!("{e:#}"), "the harnesses' records won't be kept in the conversation store");
                }
            }
            // What the server sends its mirrors, on the feed's lane.
            if let Some(out) = hub.harness.copies_out.clone() {
                let h = hub.clone();
                crate::rt::spawn(async move {
                    while let Ok((peer, json, data)) = out.recv().await {
                        if let Ok(b) = crate::proto::encode(&crate::proto::ServerMsg::Convo { json, data }) {
                            h.net.send(peer, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await;
                        }
                    }
                });
            }
            // What the server answers runners' feeds, on the feed's lane.
            if let Some(out) = hub.harness.feed_out.clone() {
                let h = hub.clone();
                crate::rt::spawn(async move {
                    while let Ok(o) = out.recv().await {
                        if let Ok(b) = crate::proto::encode(&crate::proto::ServerMsg::Convo { json: o.json, data: Vec::new() }) {
                            h.net.send(o.peer, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await;
                        }
                    }
                });
            }
            // What the server's own terminals and agents ask of kiln (`kiln spawn --on`, `kiln
            // agents wait`) is answered here.
            let forward = Some(crate::harness::ops::forward(Arc::clone(&hub)));
            match super::socket::serve_with(&hub.harness.local, super::socket::path("server", &hub.harness.dir), forward) {
                Ok(p) => *hub.harness.local_sock.lock() = Some(p.to_string_lossy().into_owned()),
                Err(e) => tracing::info!(error = %format!("{e:#}"), "`kiln spawn` will not be available in the server's own terminals"),
            }
        });
        loop {
            let Ok((id, u)) = rx.recv().await else { return Ok(()) };
            hub.harness.ingest(&hub, &Location::Local, &id, u);
        }
    }

    pub fn list(&self) -> Vec<Info> {
        let mut v: Vec<Info> = self.mirrors.lock().values().map(|m| m.info.clone()).collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    pub fn kinds(&self) -> HashMap<String, Vec<KindInfo>> {
        let mut all = self.kinds.lock().clone();
        // The server's own harnesses: the models its sessions listed since it started.
        for k in all.get_mut("local").into_iter().flatten() {
            let m = super::kinds::models_of(&k.id);
            if !m.is_empty() {
                k.models = m;
            }
        }
        all
    }

    /// A session's meta as last reported (its harness's catalog: slash commands, models, modes).
    pub fn meta(&self, id: &str) -> Option<Value> {
        self.mirrors.lock().get(id).map(|m| m.meta.clone())
    }

    pub fn location(&self, id: &str) -> Option<Location> {
        self.mirrors.lock().get(id).map(|m| m.location.clone())
    }

    /// A task's output, read on the session's machine (`None`: no such session).
    pub async fn task_output(&self, hub: &Hub, id: &str, task: &str, since: Option<u64>) -> anyhow::Result<Option<Value>> {
        match self.location(id) {
            None => Ok(None),
            Some(Location::Local) => {
                let Some(s) = self.local.get(id) else { return Ok(None) };
                let task = task.to_owned();
                Ok(Some(crate::rt::blocking(move || s.task_output(&task, since)).await?))
            }
            Some(Location::Machine(m)) => Ok(hub.harness_task_output(&m, id, task, since).await?.and_then(|j| serde_json::from_str(&j).ok())),
        }
    }

    /// A machine's past sessions, newest first, as JSON (`None`: that machine does not look, e.g.
    /// an ephemeral runner). The server's own skip what its sessions are running.
    pub async fn history(&self, hub: &Hub, target: &str, kind: Option<String>, limit: usize) -> anyhow::Result<Option<Value>> {
        match Location::of_target(target) {
            Location::Local => {
                let running: std::collections::HashSet<String> = self.local.list().into_iter().filter_map(|i| i.harness_session).collect();
                let scanner = Arc::clone(&self.scanner);
                let past = crate::rt::blocking(move || {
                    let mut g = scanner.lock();
                    g.get_or_insert_with(|| crate::aisessions::Scanner::new(crate::os::home_dir())).history(kind.as_deref(), limit, &running)
                })
                .await?;
                Ok(Some(serde_json::to_value(past)?))
            }
            Location::Machine(m) => Ok(hub.harness_history(&m, kind, u32::try_from(limit).unwrap_or(u32::MAX)).await?.and_then(|j| serde_json::from_str(&j).ok())),
        }
    }

    /// Apply one update from a machine and pass it on to the session's viewers.
    pub fn ingest(&self, hub: &Hub, from: &Location, id: &str, u: Update) {
        self.ingest_from(hub, from, id, u, false);
    }

    /// Machine `machine` is where session `id` runs (or the server doesn't know the session yet):
    /// what it says of it counts.
    pub fn runs_on(&self, id: &str, machine: &str) -> bool {
        self.mirrors.lock().get(id).is_none_or(|m| matches!(&m.location, Location::Machine(at) if at == machine))
    }

    /// Session `id` is one kiln knows, and it runs on `machine`.
    pub fn located_on(&self, id: &str, machine: &str) -> bool {
        self.mirrors.lock().get(id).is_some_and(|m| matches!(&m.location, Location::Machine(at) if at == machine))
    }

    /// A copy of session `id` at `epoch` is from before it went on elsewhere: its machine lets go
    /// of it.
    pub fn stale(&self, id: &str, epoch: u64) -> bool {
        self.epochs.lock().get(id).is_some_and(|e| *e > epoch)
    }

    /// Session `id` is at `epoch` now (epochs only rise), kept when it changes.
    fn raise_epoch(&self, id: &str, epoch: u64) {
        let snapshot = {
            let mut g = self.epochs.lock();
            let e = g.entry(id.to_owned()).or_insert(0);
            if *e >= epoch {
                return;
            }
            *e = epoch;
            g.clone()
        };
        if let Err(e) = self.keep_json("epochs.json", &snapshot) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep the agents' epochs");
        }
    }

    /// An update of session `id` from `from`. `shipped`: its machine keeps it in a store it sends
    /// the server's (`crate::convo::feed`), which keeps it from that, not from here.
    pub fn ingest_from(&self, hub: &Hub, from: &Location, id: &str, u: Update, shipped: bool) {
        let mut changed: Option<Info> = None;
        let mut gone = false;
        // Finished messages' mentions of files, to keep once the mirrors are let go.
        let mut jobs: Vec<Job> = Vec::new();
        let mut save_names = false;
        // The account's other sessions, which pause too (`hold_account`).
        let mut hold: Option<(Command, Vec<(String, Location)>)> = None;
        // Its account, and the window reached and when that starts over.
        let mut hold_for: Option<(String, Option<String>, Option<i64>)> = None;
        // Where it runs, and the name given here, are the server's to say: to the mirror and to
        // viewers alike (a session announced again — a switch of harness, a reconnect — keeps its
        // machine and its name). The machine's own name is kept aside: it counts once it changes it.
        let mut reported = String::new();
        let u = match u {
            Update::Open { mut info } => {
                info.target = from.target();
                own_name(&info.name, &info.kind_name).unwrap_or_default().clone_into(&mut reported);
                if let Some(n) = self.names.lock().get(id) {
                    info.name.clone_from(n);
                }
                info.keep_off = self.keep_off.lock().contains(id);
                Update::Open { info }
            }
            other => other,
        };
        match &u {
            Update::Open { info } if info.epoch > 0 => self.raise_epoch(id, info.epoch),
            Update::Open { .. } => {}
            // Only the machine a session runs on speaks for it: a copy left behind where it went
            // on from says nothing more of it.
            _ => {
                if self.mirrors.lock().get(id).is_some_and(|m| m.location != *from) {
                    return;
                }
            }
        }
        if let Some(store) = self.convo.as_ref().filter(|_| !shipped) {
            store.send(id, &u);
        }
        {
            let mut g = self.mirrors.lock();
            if let Update::Open { info } = &u {
                let info = (**info).clone();
                let m = g.entry(id.to_owned()).or_insert_with(|| Mirror::new(info.clone(), from.clone()));
                m.info = info.clone();
                m.reported = reported;
                m.location = from.clone();
                m.events.clear();
                m.index.clear();
                m.changes = Value::Null;
                changed = Some(info);
            }
            let Some(m) = g.get_mut(id) else {
                drop(g);
                // A session this server does not know yet (it restarted): ask for all of it.
                if let Location::Machine(machine) = from
                    && !matches!(u, Update::Closed)
                {
                    self.request_snapshot(hub, machine, id);
                }
                return;
            };
            match &u {
                Update::Open { .. } | Update::Reply { .. } => {}
                Update::Closed => gone = true,
                Update::Reset { .. } => {
                    m.events.clear();
                    m.index.clear();
                }
                Update::State { state } => {
                    m.state = serde_json::to_value(state).unwrap_or(Value::Null);
                    let before = m.info.clone();
                    m.info.apply(state);
                    m.info.last_activity = crate::util::now_ms();
                    // Its name: one of its own the machine gives anew; else the one given here.
                    let mut names = self.names.lock();
                    match naming(&state.name, &m.reported, &m.info.kind_name, names.contains_key(id)) {
                        Naming::Adopt(own) => {
                            m.reported = own.to_owned();
                            names.insert(id.to_owned(), own.to_owned());
                            save_names = true;
                        }
                        Naming::Keep => {
                            if let Some(n) = names.get(id) {
                                m.info.name.clone_from(n);
                                if let Some(o) = m.state.as_object_mut() {
                                    o.insert("name".into(), json!(n));
                                }
                            }
                        }
                        Naming::Theirs => {}
                    }
                    drop(names);
                    if m.info.differs(&before) {
                        changed = Some(m.info.clone());
                    }
                    if before.pause.is_none()
                        && let Some(p) = m.info.pause.as_ref().filter(|p| p.reason == super::pause::LIMIT && p.by == "harness")
                        && let Some(account) = m.info.account.clone()
                    {
                        let c = Command::LimitHold { window: p.window.clone(), label: p.label.clone(), until: p.until, said: p.said.clone() };
                        hold = Some((c, Vec::new()));
                        // Collected below, once this mirror is let go.
                        hold_for = Some((account, p.window.clone(), p.until));
                    }
                }
                Update::Meta { meta } => m.meta = serde_json::to_value(meta).unwrap_or(Value::Null),
                Update::Changes { files } => m.changes = serde_json::to_value(files).unwrap_or(Value::Null),
                Update::Events { events } => {
                    for e in events {
                        if let Some(job) = mentions(id, &m.info.cwd, &m.location, e) {
                            jobs.push(job);
                        }
                        if let Ok(v) = serde_json::to_value(e) {
                            m.upsert(v);
                        }
                    }
                }
                Update::Stream { id, text } => m.append(id, text),
            }
            // A state goes out with the name given here in it (the machine's may lack it).
            let frame = match &u {
                Update::State { .. } => m.state_frame(),
                _ if m.out.receiver_count() > 0 => serde_json::to_string(&u).ok().map(Frame::of),
                _ => None,
            };
            if let Some(frame) = frame {
                m.out.send(frame);
            }
            if gone {
                g.remove(id);
                save_names = self.names.lock().remove(id).is_some() || save_names;
            }
            if let (Some((account, window, until)), Some((_, targets))) = (hold_for.as_ref(), hold.as_mut()) {
                let now = crate::util::now_ms();
                *targets = g
                    .iter()
                    .filter(|(other, o)| other.as_str() != id && o.info.account.as_deref() == Some(account.as_str()) && o.info.pause.is_none() && o.info.status != Status::Exited)
                    // Not one whose own limits show the limit isn't its own: another account's
                    // (an account id gone stale), another model's.
                    .filter(|(_, o)| super::pause::shares(&o.info.limits, &o.info.model, window.as_deref(), *until, now) != Some(false))
                    .map(|(other, o)| (other.clone(), o.location.clone()))
                    .collect();
            }
        }
        if let Some((c, targets)) = hold {
            self.hold_account(hub, &c, targets);
        }
        for job in jobs {
            self.keeper.enqueue(job);
        }
        if save_names {
            self.save_names();
        }
        if gone {
            self.keeper.forget(id);
            self.visuals.forget(id);
            self.opened.forget(id);
            super::archive::closed(hub, id);
            hub.emit(&json!({"type": "harness_closed", "id": id}));
        } else if let Some(info) = changed {
            hub.emit(&json!({"type": "harness", "info": info}));
        }
    }

    /// A session reached its account's usage limit: the account's other sessions, on every machine,
    /// pause until it resets too, rather than each finding out by failing; each one whose own usage
    /// says the limit is its own as well (`pause::account_hold`).
    fn hold_account(&self, hub: &Hub, c: &Command, targets: Vec<(String, Location)>) {
        let Ok(json) = serde_json::to_string(c) else { return };
        for (id, loc) in targets {
            match loc {
                Location::Local => {
                    self.local.command(&id, c.clone());
                }
                Location::Machine(machine) => {
                    let msg = ServerMsg::HarnessCmd { session: id, json: json.clone() };
                    let net = hub.net.clone();
                    let peer = hub.agents.lock().get(&machine).and_then(|a| a.peer);
                    if let (Some(peer), Ok(b)) = (peer, crate::proto::encode(&msg)) {
                        crate::rt::spawn(async move {
                            net.send(peer, LANE_TERM, crate::net::Kind::Term, b).await;
                        });
                    }
                }
            }
        }
    }

    fn request_snapshot(&self, hub: &Hub, machine: &str, id: &str) {
        let msg = ServerMsg::HarnessCmd { session: id.to_owned(), json: json!({"type": "snapshot"}).to_string() };
        let machine = machine.to_owned();
        let net = hub.net.clone();
        let peer = hub.agents.lock().get(&machine).and_then(|a| a.peer);
        if let (Some(peer), Ok(b)) = (peer, crate::proto::encode(&msg)) {
            crate::rt::spawn(async move {
                net.send(peer, LANE_TERM, crate::net::Kind::Term, b).await;
            });
        }
    }

    /// A machine (re)connected and said what it runs: forget sessions it no longer has.
    pub fn inventory(&self, hub: &Hub, machine: &str, inv: Inventory) {
        let keep: std::collections::HashSet<String> = inv.sessions.into_iter().collect();
        let gone: Vec<String> = {
            let mut g = self.mirrors.lock();
            let gone: Vec<String> = g
                .iter()
                .filter(|(id, m)| m.location == Location::Machine(machine.to_owned()) && !keep.contains(*id))
                .map(|(id, _)| id.clone())
                .collect();
            for id in &gone {
                if let Some(m) = g.remove(id) {
                    m.out.send(Frame::of(json!({"t": "closed"}).to_string()));
                }
            }
            gone
        };
        self.kinds.lock().insert(format!("agent:{machine}"), inv.kinds);
        for id in gone {
            super::archive::closed(hub, &id);
            hub.emit(&json!({"type": "harness_closed", "id": id}));
        }
    }

    /// The machine went away: its sessions keep running there and come back when it reconnects.
    pub fn machine_lost(&self, hub: &Hub, machine: &str) {
        self.streams.machine_lost(machine);
        self.ports.machine_lost(machine);
        let mut infos = Vec::new();
        {
            let mut g = self.mirrors.lock();
            for m in g.values_mut().filter(|m| m.location == Location::Machine(machine.to_owned())) {
                m.info.status = Status::Disconnected;
                if let Some(o) = m.state.as_object_mut() {
                    o.insert("status".into(), json!("disconnected"));
                }
                if let Some(frame) = m.state_frame() {
                    m.out.send(frame);
                }
                infos.push(m.info.clone());
            }
        }
        self.kinds.lock().remove(&format!("agent:{machine}"));
        for info in infos {
            hub.emit(&json!({"type": "harness", "info": info}));
        }
    }

    /// One runner process of a machine went away while another of the machine stays connected
    /// (see `crate::instances`): the sessions it held keep running there and come back with it.
    pub fn sessions_lost(&self, hub: &Hub, ids: &[String]) {
        let mut infos = Vec::new();
        {
            let mut g = self.mirrors.lock();
            for id in ids {
                let Some(m) = g.get_mut(id) else { continue };
                m.info.status = Status::Disconnected;
                if let Some(o) = m.state.as_object_mut() {
                    o.insert("status".into(), json!("disconnected"));
                }
                if let Some(frame) = m.state_frame() {
                    m.out.send(frame);
                }
                infos.push(m.info.clone());
            }
        }
        for info in infos {
            hub.emit(&json!({"type": "harness", "info": info}));
        }
    }

    /// Join a session's stream. None when there is no such session.
    pub fn attach(&self, id: &str) -> Option<Attachment> {
        // What's kept is read before the mirrors are locked: the first time, off disk.
        let files = self.keeper.frame(id);
        let g = self.mirrors.lock();
        let m = g.get(id)?;
        Some(Attachment { frames: m.snapshot(files), live: m.out.subscribe() })
    }

    /// Catch-up frames for a viewer that fell behind.
    pub fn frames(&self, id: &str) -> Option<Vec<String>> {
        let files = self.keeper.frame(id);
        self.mirrors.lock().get(id).map(|m| m.snapshot(files))
    }

    /// Files a finished message mentioned, kept now (`super::kept`): its viewers hear of them.
    pub fn files_added(&self, id: &str, added: &[Kept]) {
        if let Some(m) = self.mirrors.lock().get(id) {
            m.out.send(Frame::of(json!({"t": "files", "files": added}).to_string()));
        }
    }

    /// Where a session runs, when one of its messages mentions `path` (a relative mention, in its
    /// folder): a viewer may read that file from its machine, and no other.
    pub fn mentioned_where(&self, id: &str, path: &str) -> Option<Location> {
        let listed = self.keeper.lists_path(id, path);
        let g = self.mirrors.lock();
        let m = g.get(id)?;
        let cwd = &m.info.cwd;
        let said = || {
            m.events
                .iter()
                .rev()
                .filter(|e| matches!(e.get("kind").and_then(Value::as_str), Some("user" | "assistant")))
                .filter_map(|e| e.get("text").and_then(Value::as_str))
                .any(|t| files::mentioned(t).iter().any(|p| files::resolve(p, cwd).as_deref() == Some(path)))
        };
        (listed || said()).then(|| m.location.clone())
    }

    /// Start sessions on `target`. Sessions on machines are named here and appear at once
    /// (starting); the machine fills them in.
    pub async fn open(&self, hub: &Hub, target: &str, spec: Spec, count: usize) -> anyhow::Result<Vec<Info>> {
        let count = count.clamp(1, 16);
        let loc = Location::of_target(target);
        let mut out = Vec::new();
        for i in 0..count {
            let mut spec = spec.clone();
            #[cfg(test)]
            if spec.program.is_none() {
                spec.program.clone_from(&self.test_program.lock());
            }
            if count > 1 {
                spec.name = Some(format!("{} {}", spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| default_name(&spec.kind)), i + 1));
            }
            match &loc {
                Location::Local => {
                    // Started on one of the server's own shells: in that shell's directory.
                    if let Some(pid) = spec.shell.as_ref().and_then(|t| self.local_shells.lock().get(t).copied()) {
                        let (cwd, env) = crate::rt::blocking(move || super::host::shell_context(pid)).await.unwrap_or((None, Vec::new()));
                        if spec.cwd.is_none() {
                            spec.cwd = cwd;
                        }
                        if spec.env.is_empty() {
                            spec.env = env;
                        }
                    }
                    // A workspace of its own first: a volume over its folder, mounted, its folder in it.
                    let id = crate::util::new_id();
                    crate::workspace::prepare(&mut spec, &id).await.map_err(|e| e.context("its workspace"))?;
                    let s = self.local.open(Some(id), spec)?;
                    let mut info = s.info();
                    info.target = loc.target();
                    // Seed the mirror so the session lists before its first update arrives.
                    self.mirrors.lock().entry(info.id.clone()).or_insert_with(|| Mirror::new(info.clone(), loc.clone()));
                    out.push(info);
                }
                Location::Machine(machine) => {
                    let id = crate::util::new_id();
                    let json = serde_json::to_string(&spec)?;
                    if !hub.send_to_agent(machine, LANE_TERM, &ServerMsg::HarnessOpen { session: id.clone(), json }).await {
                        anyhow::bail!("machine {machine} is not connected");
                    }
                    let now = crate::util::now_ms();
                    let kind = super::kinds::Kind::get(&spec.kind);
                    let info = Info {
                        id: id.clone(),
                        kind: spec.kind.clone(),
                        kind_name: kind.map_or_else(|| spec.kind.clone(), |k| k.name.to_owned()),
                        target: loc.target(),
                        name: spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| default_name(&spec.kind)),
                        shell: spec.shell.clone(),
                        cwd: spec.cwd.clone().unwrap_or_default(),
                        created_at: now,
                        status: Status::Starting,
                        title: String::new(),
                        model: spec.model.clone().unwrap_or_default(),
                        mode: spec.mode.clone().unwrap_or_default(),
                        harness_session: spec.resume.clone(),
                        approvals: Vec::new(),
                        queued: 0,
                        queue: Vec::new(),
                        tasks_running: 0,
                        subagents: Vec::new(),
                        cost_usd: 0.0,
                        activity: None,
                        last_activity: now,
                        exit: None,
                        goal: spec.prompt.clone().map(|p| super::model::clip_middle(&p, 200).0).unwrap_or_default(),
                        objective: None,
                        summary: String::new(),
                        doing: None,
                        epoch: 0,
                        todos_done: 0,
                        todos_total: 0,
                        files_changed: 0,
                        lines_added: 0,
                        lines_removed: 0,
                        context_pct: None,
                        turn_started: None,
                        limits: Vec::new(),
                        account: None,
                        plan: None,
                        left_behind: 0,
                        reach: spec.reach.clone(),
                        pause: None,
                        last_turn: None,
                        keep_off: false,
                        move_on_limit: false,
                        move_plan: Default::default(),
                        pause_at: None,
                        max_subagents: None,
                        parent: spec.parent.clone(),
                        plan_run: spec.plan_run.clone(),
                        no_bypass: !spec.allow_bypass,
                        workspace: spec.workspace.clone(),
                    };
                    self.mirrors.lock().entry(id).or_insert_with(|| Mirror::new(info.clone(), loc.clone()));
                    out.push(info);
                }
            }
        }
        for info in &out {
            hub.emit(&json!({"type": "harness", "info": info}));
        }
        Ok(out)
    }

    /// Keep an uploaded file until a message refers to it (by the returned id).
    pub fn upload(&self, name: String, mime: String, data: bytes::Bytes) -> anyhow::Result<String> {
        let mut g = self.uploads.lock();
        g.retain(|_, u| u.at.elapsed() < UPLOAD_TTL);
        let used: usize = g.values().map(|u| u.data.len()).sum();
        if used + data.len() > UPLOADS_MAX {
            anyhow::bail!("too many uploads waiting; send them first");
        }
        let id = crate::util::new_id();
        g.insert(id.clone(), Upload { name, mime, data, at: std::time::Instant::now() });
        Ok(id)
    }

    /// Attachments that refer to uploads, with the uploads' bytes in them instead: what one
    /// message to many agents carries to each (an upload is taken by the first message that sends
    /// it). One whose upload is gone (or expired) is left out.
    pub fn inline_uploads(&self, attachments: Vec<super::model::Attachment>) -> Vec<super::model::Attachment> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        attachments
            .into_iter()
            .filter_map(|mut a| {
                a.path = None;
                let Some(fid) = a.file_id.take() else { return (!a.data_base64.is_empty()).then_some(a) };
                let u = self.uploads.lock().remove(&fid)?;
                if a.name.is_empty() {
                    a.name = u.name;
                }
                if a.mime.is_empty() {
                    a.mime = u.mime;
                }
                a.data_base64 = b64.encode(&u.data);
                Some(a)
            })
            .collect()
    }

    /// A message's attachments, made ready for where the session runs: uploads resolved, and
    /// for a machine, every file sent ahead in pieces (the message then refers to them by id).
    async fn route_attachments(&self, hub: &Hub, id: &str, loc: &Location, c: Command) -> Result<Command, Sent> {
        use base64::Engine as _;
        let Command::Send { id: mid, text, mode, attachments, overrides } = c else { return Ok(c) };
        if attachments.is_empty() {
            return Ok(Command::Send { id: mid, text, mode, attachments, overrides });
        }
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut out = Vec::new();
        for mut a in attachments {
            a.path = None;
            // The bytes: inline, or an upload.
            let data: Option<bytes::Bytes> = match a.file_id.take().and_then(|f| self.uploads.lock().remove(&f)) {
                Some(u) => {
                    if a.name.is_empty() {
                        a.name = u.name;
                    }
                    if a.mime.is_empty() {
                        a.mime = u.mime;
                    }
                    Some(u.data)
                }
                None => b64.decode(a.data_base64.as_bytes()).ok().map(bytes::Bytes::from),
            };
            let Some(data) = data.filter(|d| !d.is_empty()) else { continue };
            match loc {
                Location::Local => {
                    let fid = crate::util::new_id();
                    let (host, sid, f) = (Arc::clone(&self.local), id.to_owned(), fid.clone());
                    let stored = crate::rt::blocking(move || host.receive(&sid, &f, 0, &data, true)).await;
                    if !matches!(stored, Ok(Ok(()))) {
                        continue;
                    }
                    a.data_base64.clear();
                    a.file_id = Some(fid);
                }
                Location::Machine(machine) => {
                    let fid = crate::util::new_id();
                    let total = data.len();
                    let mut offset = 0usize;
                    while offset < total || total == 0 {
                        let end = (offset + FILE_CHUNK).min(total);
                        let piece = data.get(offset..end).map(<[u8]>::to_vec).unwrap_or_default();
                        let msg = ServerMsg::HarnessFile { session: id.to_owned(), file: fid.clone(), offset: offset as u64, data: piece, done: end >= total };
                        if !hub.send_to_agent(machine, LANE_TERM, &msg).await {
                            return Err(Sent::Unreachable);
                        }
                        offset = end;
                        if end >= total {
                            break;
                        }
                    }
                    a.data_base64.clear();
                    a.file_id = Some(fid);
                }
            }
            out.push(a);
        }
        Ok(Command::Send { id: mid, text, mode, attachments: out, overrides })
    }

    /// A harness signed in on a machine (kiln's /login): each of that machine's sessions of it is
    /// told, to take up the account (Claude Code starts again with it once its turn ends; Codex reads
    /// it again).
    pub async fn signed_in(&self, hub: &Hub, target: &str, kind: &str) {
        let ids: Vec<String> = self.list().into_iter().filter(|i| i.target == target && i.kind == kind).map(|i| i.id).collect();
        for id in ids {
            let _ = self.command(hub, &id, Command::Control { id: "account".into(), value: None }).await;
        }
    }

    /// A command for a session, from any device.
    pub async fn command(&self, hub: &Hub, id: &str, c: Command) -> Sent {
        let Some(loc) = self.location(id) else { return Sent::NoSession };
        // Kiln's own (`goals::keep`), kept here: its harness has nothing to do with it.
        if let Command::Control { id: control, value } = &c
            && control == "keep_going"
        {
            self.set_keep_going(hub, id, value.as_ref().and_then(Value::as_bool).unwrap_or(true));
            return Sent::Ok;
        }
        // A name is kept here: the session takes it at once, whatever its machine can do (and
        // its harness names its conversation too, where it does).
        let renamed = matches!(&c, Command::Control { id: control, .. } if control == "rename");
        if let Command::Control { id: control, value } = &c
            && control == "rename"
        {
            self.rename(hub, id, value.as_ref());
        }
        let c = match self.route_attachments(hub, id, &loc, c).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        match Some(loc) {
            None => Sent::NoSession,
            Some(Location::Local) => {
                if self.local.command(id, c) {
                    Sent::Ok
                } else {
                    Sent::NoSession
                }
            }
            Some(Location::Machine(machine)) => {
                let Ok(json) = serde_json::to_string(&c) else { return Sent::NoSession };
                // A name is given even while the machine is away: it's kept here.
                if hub.send_to_agent(&machine, LANE_TERM, &ServerMsg::HarnessCmd { session: id.to_owned(), json }).await || renamed {
                    Sent::Ok
                } else {
                    Sent::Unreachable
                }
            }
        }
    }

    /// Stop a session for good.
    /// Session `id` goes on on machine `to` (in folder `cwd`, else the one it worked in), from the
    /// server's conversation store, because its own machine is gone (`docs/conversations.md`,
    /// "Resuming on another machine"). From here on the copy on the machine it was on doesn't
    /// count: that machine lets go of it if it comes back. Ok once the package is on its way.
    pub async fn resume_on(&self, hub: &Hub, id: &str, to: &str, cwd: Option<String>) -> Result<(), String> {
        let store = self.convo.clone().ok_or_else(|| "this server keeps no conversations (KILN_CONVERSATIONS is off)".to_owned())?;
        let (from, epoch) = {
            let g = self.mirrors.lock();
            let m = g.get(id).ok_or_else(|| "no such agent session".to_owned())?;
            (m.location.clone(), m.state.get("epoch").and_then(Value::as_u64).unwrap_or(0))
        };
        let from_name = match &from {
            Location::Local => return Err("it runs on the server itself, which is up: move it instead".into()),
            Location::Machine(m) => m.clone(),
        };
        if hub.agents.lock().get(&from_name).is_some_and(|a| a.connected) {
            return Err(format!("{from_name} is online: move the session from there instead"));
        }
        if !hub.agents.lock().get(to).is_some_and(|a| a.connected) {
            return Err(format!("{to} isn't connected"));
        }
        if hub.runner_caps(to) & crate::proto::CAP_CONVO == 0 {
            return Err(format!("{to} runs a kiln too old to take a session from the server; upgrade it first"));
        }
        let (st, i, f, t) = (store.clone(), id.to_owned(), from_name.clone(), to.to_owned());
        let pkg = crate::rt::blocking(move || crate::convo::resume::package(&st, &i, &f, &t, cwd.as_deref()))
            .await
            .map_err(|e| format!("{e:#}"))?
            .map_err(|e| format!("{e:#}"))?;
        let bytes = serde_json::to_vec(&pkg).map_err(|e| e.to_string())?;
        // Its copy where it was doesn't count from now on: not here, not in the store (rows that
        // machine still sends are left out).
        let next = epoch.max(pkg.state.epoch) + 1;
        self.raise_epoch(id, next);
        store.put(vec![crate::convo::rows::Row::Session { session: id.to_owned(), info: None, meta: None, state: None, changes: None, epoch: Some(next), open: false, at: crate::util::now_ms() }]);
        let info = {
            let mut g = self.mirrors.lock();
            g.get_mut(id).map(|m| {
                m.location = Location::Machine(to.to_owned());
                m.info.target = m.location.target();
                m.info.status = Status::Starting;
                m.info.clone()
            })
        };
        if let Some(info) = info {
            hub.emit(&json!({"type": "harness", "info": info}));
        }
        tracing::info!(session = %id, from = %from_name, %to, bytes = bytes.len(), "agent session goes on on another machine, from the server's store");
        // In pieces, on the conversation lane.
        const PIECE: usize = 3 << 20;
        let pieces: Vec<&[u8]> = bytes.chunks(PIECE).collect();
        let mut offset = 0u64;
        for (n, piece) in pieces.iter().enumerate() {
            let msg = ServerMsg::HarnessPackage { session: id.to_owned(), offset, data: piece.to_vec(), done: n + 1 == pieces.len() };
            if !hub.send_to_agent(to, crate::proto::LANE_CONVO, &msg).await {
                return Err(format!("{to} went away while the session was on its way"));
            }
            offset += piece.len() as u64;
        }
        Ok(())
    }

    pub async fn close(&self, hub: &Hub, id: &str) -> bool {
        let loc = self.location(id);
        let ok = match &loc {
            None => false,
            Some(Location::Local) => self.local.close(id),
            Some(Location::Machine(machine)) => hub.send_to_agent(machine, LANE_TERM, &ServerMsg::HarnessClose { session: id.to_owned() }).await,
        };
        // A machine that is gone cannot confirm: forget it here; it is dropped there on reconnect.
        // Ended in the store too, or it would be listed as away again after a restart.
        if loc.is_some() && (!ok || matches!(loc, Some(Location::Local))) {
            if let Some(m) = self.mirrors.lock().remove(id) {
                m.out.send(Frame::of(json!({"t": "closed"}).to_string()));
            }
            self.keeper.forget(id);
            self.visuals.forget(id);
            self.streams.forget(hub, id).await;
            self.opened.forget(id);
            if let Some(store) = &self.convo {
                store.put(vec![crate::convo::rows::Row::Closed { session: id.to_owned(), at: crate::util::now_ms() }]);
            }
            super::archive::closed(hub, id);
            hub.emit(&json!({"type": "harness_closed", "id": id}));
        }
        ok || loc.is_some()
    }

    /// The parsed state of a session (for checks before acting on it).
    pub fn state(&self, id: &str) -> Option<State> {
        self.mirrors.lock().get(id).and_then(|m| serde_json::from_value(m.state.clone()).ok())
    }

    /// A session as lists show it.
    pub fn info(&self, id: &str) -> Option<Info> {
        self.mirrors.lock().get(id).map(|m| m.info.clone())
    }

    /// What waits in session `id`'s queue, as its machine last said, the messages whole.
    pub fn queue(&self, id: &str) -> Vec<super::model::QueueItem> {
        let g = self.mirrors.lock();
        g.get(id).and_then(|m| m.state.get("queue")).and_then(|q| serde_json::from_value(q.clone()).ok()).unwrap_or_default()
    }

    /// What `agents.get` and `agents.result` read of a session (`harness::ops`): its state as
    /// last reported, its last reply on its main thread (an `assistant` event) and its changeset.
    pub fn conclusion(&self, id: &str) -> Option<(Value, Option<Value>, Value)> {
        let g = self.mirrors.lock();
        let m = g.get(id)?;
        let reply = m.events.iter().rev().find(|e| e.get("kind").and_then(Value::as_str) == Some("assistant") && e.get("parent").is_none_or(Value::is_null)).cloned();
        Some((m.state.clone(), reply, m.changes.clone()))
    }

    /// Tests: session `id`'s changeset, as its machine would report it.
    #[cfg(test)]
    pub fn set_changes(&self, id: &str, files: Value) -> bool {
        self.mirrors.lock().get_mut(id).map(|m| m.changes = files).is_some()
    }
}

/// A finished message's mentions of files, to keep (`super::kept`): what the person or the agent
/// said, never a tool's input or output.
fn mentions(id: &str, cwd: &str, location: &Location, e: &super::model::Event) -> Option<Job> {
    let text = match &e.body {
        super::model::Body::User { text, .. } | super::model::Body::Assistant { text, streaming: false } => text,
        _ => return None,
    };
    let paths: Vec<String> = files::mentioned(text).iter().filter_map(|p| files::resolve(p, cwd)).collect();
    (!paths.is_empty()).then(|| Job { session: id.to_owned(), seq: e.seq, paths, location: location.clone() })
}

fn default_name(kind: &str) -> String {
    super::kinds::Kind::get(kind).map_or_else(|| kind.to_owned(), |k| k.name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_given_here_holds_whatever_the_machine_knows_of_names() {
        // A runner from before names reports none: the name given here shows.
        assert_eq!(naming("", "", "Claude Code", true), Naming::Keep);
        assert_eq!(naming("", "", "Claude Code", false), Naming::Theirs);
        // Upgraded since, it reports its harness's name (in any case): still the one given here.
        assert_eq!(naming("Claude Code", "", "Claude Code", true), Naming::Keep);
        assert_eq!(naming("claude code ", "", "Claude Code", true), Naming::Keep);
        // A machine that names it itself: taken, and kept here too.
        assert_eq!(naming("slates-6d", "", "Claude Code", false), Naming::Adopt("slates-6d"));
        assert_eq!(naming("Mantle", "slates-6d", "Codex", true), Naming::Adopt("Mantle"));
        // Renamed here while the machine still reports its old one: the new one shows until the
        // machine takes it.
        assert_eq!(naming("slates-6d", "slates-6d", "Claude Code", true), Naming::Keep);
    }

    #[test]
    fn a_state_goes_out_whole_and_as_what_changed_in_it() -> anyhow::Result<()> {
        let mut m = Mirror::new(Info::default(), Location::Local);
        let mut live = m.out.subscribe();
        let skills = json!([{"name": "deploy", "description": "Ship it. ".repeat(200)}]);
        m.state = json!({"status": "running", "activity": "reading", "skills": skills, "pause": {"reason": "limit"}});
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        // The first goes whole: there is nothing before it to patch.
        assert!(f.patch.is_none());
        assert_eq!(f.for_viewer(true), f.for_viewer(false));
        m.out.send(f);
        m.state = json!({"status": "running", "activity": "editing", "skills": skills});
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.out.send(f);
        let first = live.try_recv()?;
        let second = live.try_recv()?;
        let whole: Value = serde_json::from_str(second.for_viewer(false))?;
        assert_eq!(whole, json!({"t": "state", "state": m.state}));
        // As what changed: the activity set, the pause gone, the skills (most of it) left out.
        let patch: Value = serde_json::from_str(second.for_viewer(true))?;
        assert_eq!(patch, json!({"t": "state_patch", "set": {"activity": "editing"}, "gone": ["pause"]}));
        assert!(second.for_viewer(true).len() * 20 < second.for_viewer(false).len());
        // Applied to what came first, it makes the state now.
        let mut had: Value = serde_json::from_str(first.for_viewer(true))?;
        let had = had.get_mut("state").and_then(Value::as_object_mut).ok_or_else(|| anyhow::anyhow!("no state"))?;
        for (k, v) in patch.get("set").and_then(Value::as_object).into_iter().flatten() {
            had.insert(k.clone(), v.clone());
        }
        for k in patch.get("gone").and_then(Value::as_array).into_iter().flatten() {
            had.remove(k.as_str().unwrap_or_default());
        }
        assert_eq!(Value::Object(had.clone()), m.state);
        Ok(())
    }

    #[test]
    fn states_without_viewers_do_not_serialize_or_keep_a_patch_baseline() -> anyhow::Result<()> {
        let mut m = Mirror::new(Info::default(), Location::Local);
        let viewer = m.out.subscribe();
        m.state = json!({"status": "running"});
        assert!(m.state_frame().is_some());
        drop(viewer);
        m.state = json!({"status": "running", "activity": "editing"});
        assert!(m.state_frame().is_none());
        assert!(m.sent.is_null());
        let _viewer = m.out.subscribe();
        let snapshot = m.snapshot(None);
        assert!(snapshot.iter().any(|f| serde_json::from_str::<Value>(f).ok() == Some(json!({"t": "state", "state": m.state}))));
        m.state = json!({"status": "idle"});
        let frame = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        // A field that appeared while nobody watched is absent from the whole state too.
        assert!(frame.patch.is_none());
        assert_eq!(serde_json::from_str::<Value>(frame.for_viewer(true))?, json!({"t": "state", "state": m.state}));
        Ok(())
    }

    #[test]
    fn names_are_kept_across_restarts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let r = Registry::new(dir.path().to_owned());
        r.names.lock().insert("s1".into(), "Mantle".into());
        r.save_names();
        let again = Registry::new(dir.path().to_owned());
        assert_eq!(again.names.lock().get("s1").map(String::as_str), Some("Mantle"));
        Ok(())
    }
}
