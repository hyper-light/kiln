//! The server's view of every agent session, wherever it runs: its own (`local`) and each
//! runner's. A mirror per session (the latest state, meta and timeline, as JSON) serves any number
//! of viewers on any device; their commands go to the machine that runs the session.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::core::{MAX_EVENTS, Update};
use super::host::{Host, Inventory, Limits};
use super::kinds::KindInfo;
use super::model::{Command, State, Status};
use super::session::{Info, Spec};
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
    location: Location,
    meta: Value,
    state: Value,
    changes: Value,
    events: Vec<Value>,
    index: HashMap<String, usize>,
    out: Broadcast<Arc<str>>,
}

impl Mirror {
    fn new(info: Info, location: Location) -> Self {
        Self { info, location, meta: Value::Null, state: Value::Null, changes: Value::Null, events: Vec::new(), index: HashMap::new(), out: Broadcast::new(FANOUT) }
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
            && let Some(t) = e.get_mut("text")
            && let Some(s) = t.as_str()
        {
            *t = Value::String(format!("{s}{text}"));
        }
    }

    /// The frames that bring a new viewer to the current state.
    fn snapshot(&self) -> Vec<String> {
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
    pub live: async_broadcast::Receiver<Arc<str>>,
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

pub struct Registry {
    mirrors: Mutex<HashMap<String, Mirror>>,
    uploads: Mutex<HashMap<String, Upload>>,
    /// What each machine offers ("local" included).
    kinds: Mutex<HashMap<String, Vec<KindInfo>>>,
    /// The server's own sessions.
    pub local: Arc<Host>,
    local_rx: async_channel::Receiver<(String, Update)>,
    /// Where `kiln spawn` in the server's own terminals reaches `local`.
    local_sock: Mutex<Option<String>>,
    /// The shell pid of each of the server's own terminals (their directory and environment are
    /// where agents started "in this shell" begin).
    local_shells: Mutex<HashMap<String, u32>>,
    dir: std::path::PathBuf,
}

impl Registry {
    pub fn new(dir: std::path::PathBuf) -> Self {
        let (tx, rx) = async_channel::bounded(8192);
        let local = Host::new(tx, Some(dir.join("local.json")), Limits::default());
        Self {
            mirrors: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            kinds: Mutex::new(HashMap::new()),
            local,
            local_rx: rx,
            local_sock: Mutex::new(None),
            local_shells: Mutex::new(HashMap::new()),
            dir,
        }
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
    pub async fn run_local(hub: Arc<Hub>) -> anyhow::Result<()> {
        static RESTORED: std::sync::Once = std::sync::Once::new();
        let rx = hub.harness.local_rx.clone();
        let offered = crate::rt::blocking(super::kinds::offered).await.unwrap_or_default();
        hub.harness.kinds.lock().insert("local".to_owned(), offered);
        RESTORED.call_once(|| {
            hub.harness.local.restore();
            match super::socket::serve(&hub.harness.local, super::socket::path("server", &hub.harness.dir)) {
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
        self.kinds.lock().clone()
    }

    pub fn location(&self, id: &str) -> Option<Location> {
        self.mirrors.lock().get(id).map(|m| m.location.clone())
    }

    /// Apply one update from a machine and pass it on to the session's viewers.
    pub fn ingest(&self, hub: &Hub, from: &Location, id: &str, u: Update) {
        let mut changed: Option<Info> = None;
        let mut gone = false;
        {
            let mut g = self.mirrors.lock();
            if let Update::Open { info } = &u {
                let mut info = (**info).clone();
                info.target = from.target();
                let m = g.entry(id.to_owned()).or_insert_with(|| Mirror::new(info.clone(), from.clone()));
                m.info = info.clone();
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
                Update::Reset => {
                    m.events.clear();
                    m.index.clear();
                }
                Update::State { state } => {
                    m.state = serde_json::to_value(state).unwrap_or(Value::Null);
                    let before = m.info.clone();
                    m.info.apply(state);
                    m.info.last_activity = crate::util::now_ms();
                    if m.info.differs(&before) {
                        changed = Some(m.info.clone());
                    }
                }
                Update::Meta { meta } => m.meta = serde_json::to_value(meta).unwrap_or(Value::Null),
                Update::Changes { files } => m.changes = serde_json::to_value(files).unwrap_or(Value::Null),
                Update::Events { events } => {
                    for e in events {
                        if let Ok(v) = serde_json::to_value(e) {
                            m.upsert(v);
                        }
                    }
                }
                Update::Stream { id, text } => m.append(id, text),
            }
            if let Ok(frame) = serde_json::to_string(&u) {
                m.out.send(Arc::from(frame));
            }
            if gone {
                g.remove(id);
            }
        }
        if gone {
            hub.emit(&json!({"type": "harness_closed", "id": id}));
        } else if let Some(info) = changed {
            hub.emit(&json!({"type": "harness", "info": info}));
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
                    m.out.send(Arc::from(json!({"t": "closed"}).to_string()));
                }
            }
            gone
        };
        self.kinds.lock().insert(format!("agent:{machine}"), inv.kinds);
        for id in gone {
            hub.emit(&json!({"type": "harness_closed", "id": id}));
        }
    }

    /// The machine went away: its sessions keep running there and come back when it reconnects.
    pub fn machine_lost(&self, hub: &Hub, machine: &str) {
        let mut infos = Vec::new();
        {
            let mut g = self.mirrors.lock();
            for m in g.values_mut().filter(|m| m.location == Location::Machine(machine.to_owned())) {
                m.info.status = Status::Disconnected;
                if let Some(o) = m.state.as_object_mut() {
                    o.insert("status".into(), json!("disconnected"));
                }
                m.out.send(Arc::from(json!({"t": "state", "state": m.state}).to_string()));
                infos.push(m.info.clone());
            }
        }
        self.kinds.lock().remove(&format!("agent:{machine}"));
        for info in infos {
            hub.emit(&json!({"type": "harness", "info": info}));
        }
    }

    /// Join a session's stream. None when there is no such session.
    pub fn attach(&self, id: &str) -> Option<Attachment> {
        let g = self.mirrors.lock();
        let m = g.get(id)?;
        Some(Attachment { frames: m.snapshot(), live: m.out.subscribe() })
    }

    /// Catch-up frames for a viewer that fell behind.
    pub fn frames(&self, id: &str) -> Option<Vec<String>> {
        self.mirrors.lock().get(id).map(Mirror::snapshot)
    }

    /// Start sessions on `target`. Sessions on machines are named here and appear at once
    /// (starting); the machine fills them in.
    pub async fn open(&self, hub: &Hub, target: &str, spec: Spec, count: usize) -> anyhow::Result<Vec<Info>> {
        let count = count.clamp(1, 16);
        let loc = Location::of_target(target);
        let mut out = Vec::new();
        for i in 0..count {
            let mut spec = spec.clone();
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
                    let s = self.local.open(None, spec)?;
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
                        tasks_running: 0,
                        cost_usd: 0.0,
                        activity: None,
                        last_activity: now,
                        exit: None,
                        goal: spec.prompt.clone().map(|p| super::model::clip_middle(&p, 200).0).unwrap_or_default(),
                        summary: String::new(),
                        doing: None,
                        todos_done: 0,
                        todos_total: 0,
                        files_changed: 0,
                        lines_added: 0,
                        lines_removed: 0,
                        context_pct: None,
                        turn_started: None,
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

    /// A command for a session, from any device.
    pub async fn command(&self, hub: &Hub, id: &str, c: Command) -> Sent {
        let Some(loc) = self.location(id) else { return Sent::NoSession };
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
                if hub.send_to_agent(&machine, LANE_TERM, &ServerMsg::HarnessCmd { session: id.to_owned(), json }).await {
                    Sent::Ok
                } else {
                    Sent::Unreachable
                }
            }
        }
    }

    /// Stop a session for good.
    pub async fn close(&self, hub: &Hub, id: &str) -> bool {
        let loc = self.location(id);
        let ok = match &loc {
            None => false,
            Some(Location::Local) => self.local.close(id),
            Some(Location::Machine(machine)) => hub.send_to_agent(machine, LANE_TERM, &ServerMsg::HarnessClose { session: id.to_owned() }).await,
        };
        // A machine that is gone cannot confirm: forget it here; it is dropped there on reconnect.
        if loc.is_some() && (!ok || matches!(loc, Some(Location::Local))) {
            if let Some(m) = self.mirrors.lock().remove(id) {
                m.out.send(Arc::from(json!({"t": "closed"}).to_string()));
            }
            hub.emit(&json!({"type": "harness_closed", "id": id}));
        }
        ok || loc.is_some()
    }

    /// The parsed state of a session (for checks before acting on it).
    pub fn state(&self, id: &str) -> Option<State> {
        self.mirrors.lock().get(id).and_then(|m| serde_json::from_value(m.state.clone()).ok())
    }
}

fn default_name(kind: &str) -> String {
    super::kinds::Kind::get(kind).map_or_else(|| kind.to_owned(), |k| k.name.to_owned())
}
