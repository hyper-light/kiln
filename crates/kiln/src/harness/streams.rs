//! Pages streamed from agents' machines (docs/previews.md, "Streams"), relayed by the server to
//! whoever views them: a stream runs where its session does (`crate::browser::stream`), its
//! chunks come up its runner's stream lane, and go out to each viewer's socket as they come. A
//! viewer that joins gets the decoder's configuration and a fresh key frame (asked of the runner),
//! so it sees the page at once; one too slow for a chunk loses it, and the next key frame mends it.
//! With nobody looking, the stream pauses (nothing is captured or sent), and after a while it
//! closes.
//!
//! A recording's files are made on its stream's machine; once it ends the server copies them into
//! kiln's store of bytes (docs/blobs.md, `recordings/<session>/<recording>/<file>`, each up to
//! [`RECORDING_MAX`]) and serves them from there, so they show while the machine is away. They go
//! when the conversation closes, as they do on the machine.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::proto::{CAP_STREAM, ServerMsg};

/// A stream nobody has looked at for this long closes.
const UNWATCHED: Duration = Duration::from_secs(600);
/// Chunks a viewer may be behind before it loses one.
const BEHIND: usize = 240;
/// The largest recording file kiln copies into its store of bytes; a larger one is served from its
/// machine alone.
pub const RECORDING_MAX: u64 = 1 << 30;
/// Recordings' copies under way at once, at most; past that, a recording is served from its machine.
const COPIES_MAX: usize = 8;

/// What a viewer is sent: a chunk (as its runner sent it) or an event (JSON).
#[derive(Debug, Clone)]
pub enum ToViewer {
    Chunk(Bytes),
    Event(String),
}

/// One stream, as the server relays it.
pub struct Live {
    pub id: String,
    pub session: String,
    pub machine: String,
    /// What it was opened with (`crate::browser::stream::Spec`), and by whom.
    pub spec: Value,
    pub by: String,
    pub at: i64,
    viewers: Mutex<Vec<(u64, async_channel::Sender<ToViewer>)>>,
    /// The decoders' configurations (video, audio), for viewers that join.
    video: Mutex<Option<Bytes>>,
    audio: Mutex<Option<Bytes>>,
    /// What it said that a viewer joining should know: opened, where it is, its title, the field
    /// it's typed in and where its fields are, whether text is selected.
    said: Mutex<HashMap<String, String>>,
    /// When the last viewer left (unix ms; 0 while someone looks).
    unwatched: AtomicI64,
    /// The size its viewer last asked for: asked again as it opens (one asked while its runner was
    /// still setting it up wasn't heard).
    resized: Mutex<Option<Value>>,
}

#[derive(Default)]
pub struct Relay {
    live: Mutex<HashMap<String, Arc<Live>>>,
    next: AtomicU64,
    /// Streams that ended lately, by id: whose they were (a recording's end can come after).
    ended: Mutex<HashMap<String, Owner>>,
    /// Recordings made of each session's streams, as their runners said (`crate::browser::record`).
    recordings: Mutex<HashMap<String, Vec<Recording>>>,
    /// Recordings' copies into kiln's store of bytes under way.
    copying: AtomicU64,
}

/// What's left to do once a runner's event is relayed ([`Relay::event`]).
pub struct Evented {
    /// As it opens, the stream and the size its viewer asked for meanwhile, to ask of it again.
    pub again: Option<(Arc<Live>, Value)>,
    /// A recording that ended, with its session, for its files to be kept ([`Relay::keep`]).
    pub ended: Option<(String, Recording)>,
}

/// Whose a stream is: its session, its machine, its profile.
#[derive(Debug, Clone)]
struct Owner {
    session: String,
    machine: String,
    profile: Option<String>,
}

/// A recording of a stream, on its machine: its video (MP4), its log of what was done, and that as
/// a test.
#[derive(Debug, Clone, Serialize)]
pub struct Recording {
    pub id: String,
    pub stream: String,
    pub at: i64,
    pub seconds: f64,
    pub bytes: u64,
    pub pictures: u64,
    pub video: String,
    pub inputs: String,
    pub test: String,
    #[serde(skip)]
    pub machine: String,
    #[serde(skip)]
    profile: Option<String>,
    /// Its files kiln's store of bytes holds (`video`, `inputs`, `test`), as they were copied.
    #[serde(skip)]
    pub kept: HashMap<String, super::files::Meta>,
}

impl Recording {
    /// Whether `person` may see it: one of a stream in a person's own profile is theirs alone.
    pub fn for_whom(&self, person: Option<&str>) -> bool {
        visible(self.profile.as_deref(), person)
    }

    /// Its file of `what` (`video`, `inputs`, `test`).
    pub fn file(&self, what: &str) -> Option<&str> {
        match what {
            "video" => Some(&self.video),
            "inputs" => Some(&self.inputs),
            "test" => Some(&self.test),
            _ => None,
        }
    }
}

/// Whether `person` may see what's of a stream in `profile`.
fn visible(profile: Option<&str>, person: Option<&str>) -> bool {
    match profile {
        Some(p) if p.starts_with("personal:") => person.is_some_and(|me| personal(me) == p),
        _ => true,
    }
}

impl Relay {
    pub fn get(&self, id: &str) -> Option<Arc<Live>> {
        self.live.lock().get(id).cloned()
    }

    /// A session's streams.
    pub fn of(&self, session: &str) -> Vec<Arc<Live>> {
        self.live.lock().values().filter(|l| l.session == session).cloned().collect()
    }

    /// Open a stream for `session` on its machine; its id. Its runner must stream
    /// ([`CAP_STREAM`]); the server's own sessions don't.
    pub async fn open(&self, hub: &Arc<Hub>, session: &str, mut spec: Value, by: &str) -> anyhow::Result<Arc<Live>> {
        let machine = match hub.harness.location(session) {
            Some(super::registry::Location::Machine(m)) => m,
            Some(super::registry::Location::Local) => anyhow::bail!("streams come from runners: this session runs on the server itself"),
            None => anyhow::bail!("no such agent session"),
        };
        let url = spec.get("url").and_then(Value::as_str).unwrap_or("").trim().to_owned();
        anyhow::ensure!(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://") || url.starts_with("about:"), "which page? An address (http, https or file)");
        if let Some(o) = spec.as_object_mut() {
            // A conversation's streams share its cookies, and nobody else's, unless the person
            // asked for a clean profile or their own (`profile_for`).
            let kept = o.get("profile").and_then(Value::as_str).is_some_and(|p| p == "clean" || p.starts_with("personal:"));
            if !kept {
                o.insert("profile".into(), json!(format!("conversation:{session}")));
            }
        }
        let id = hex::encode(rand::random::<[u8; 16]>());
        let msg = ServerMsg::StreamOpen { stream: id.clone(), session: session.to_owned(), json: spec.to_string() };
        // The runner process holding the session streams it: it must know how.
        let caps = hub.routed_caps(&machine, &msg).await;
        anyhow::ensure!(caps & CAP_STREAM != 0, "{machine} runs a kiln too old to stream pages: upgrade its runner");
        // Another engine, or a device: a runner from before would stream Chromium as its viewer is.
        let other = spec.get("engine").and_then(Value::as_str).is_some_and(|e| !e.is_empty() && e != "chromium") || spec.get("device").and_then(Value::as_str).is_some_and(|d| !d.is_empty());
        anyhow::ensure!(!other || caps & crate::proto::CAP_ENGINES != 0, "{machine} runs a kiln too old for other engines and devices: upgrade its runner");
        let live = Arc::new(Live {
            id: id.clone(),
            session: session.to_owned(),
            machine: machine.clone(),
            spec: spec.clone(),
            by: by.to_owned(),
            at: crate::util::now_ms(),
            viewers: Mutex::default(),
            video: Mutex::default(),
            audio: Mutex::default(),
            said: Mutex::default(),
            unwatched: AtomicI64::new(crate::util::now_ms()),
            resized: Mutex::default(),
        });
        self.live.lock().insert(id.clone(), live.clone());
        // A live command (`crate::wire::commands`): refused at once while its runner is away, and
        // answered once its runner opened the page. Unanswered, it goes on: its picture says.
        let answer = hub.command_start(&machine, &msg).await.answer(crate::hub::COMMAND_WITHIN).await;
        if let a @ (crate::wire::commands::Answer::Refused { .. } | crate::wire::commands::Answer::Unreachable { .. }) = answer {
            self.live.lock().remove(&id);
            anyhow::bail!("{machine}: {}", a.words());
        }
        crate::rt::spawn(close_when_unwatched(hub.clone(), id));
        Ok(live)
    }

    /// A chunk from a runner: kept when it's a decoder's configuration, sent on to each viewer.
    pub fn chunk(&self, machine: &str, id: &str, data: Vec<u8>) {
        let Some(live) = self.get(id).filter(|l| l.machine == machine) else { return };
        let data = Bytes::from(data);
        match (data.first(), data.get(1)) {
            (Some(b'c'), Some(b'a')) => *live.audio.lock() = Some(data.clone()),
            (Some(b'c'), _) => *live.video.lock() = Some(data.clone()),
            _ => {}
        }
        live.send(&ToViewer::Chunk(data));
    }

    /// An event from a runner: kept when a joining viewer needs it, sent on to each viewer; a
    /// stream that closed or failed to open goes. What's left to do is answered ([`Evented`]).
    pub fn event(&self, machine: &str, id: &str, mut json: String) -> Evented {
        let mut v: Value = serde_json::from_str(&json).unwrap_or_default();
        // A recording's end: kept, and said to its viewers with the id to get its files by.
        let ended = v.get("recorded").and_then(|r| self.recorded(machine, id, r));
        if let Some((_, rec)) = &ended {
            if let Some(r) = v.get_mut("recorded").and_then(Value::as_object_mut) {
                r.insert("id".into(), json!(rec.id));
            }
            json = v.to_string();
        }
        let mut after = Evented { again: None, ended };
        let Some(live) = self.get(id).filter(|l| l.machine == machine) else { return after };
        // Its tabs as they were before the ones it said last (said later, arrived first): no news.
        let seq = |v: &Value| v.pointer("/tabs/seq").and_then(Value::as_u64);
        if let Some(new) = seq(&v)
            && live.said.lock().get("tabs").and_then(|kept| serde_json::from_str::<Value>(kept).ok()).and_then(|kept| seq(&kept)).is_some_and(|kept| kept > new)
        {
            return after;
        }
        if let Some(k) = v.as_object().and_then(|o| o.keys().next()).cloned() {
            if k == "opened"
                && let Some(r) = live.resized.lock().clone()
            {
                after.again = Some((live.clone(), json!({"resize": r})));
            }
            // What a viewer joining later needs to show it as it is now (whether its window can be
            // seen where it runs, too: Safari's on a locked Mac; its tabs).
            if matches!(k.as_str(), "opened" | "navigated" | "loaded" | "focus" | "fields" | "selected" | "hidden" | "tabs") {
                live.said.lock().insert(k.clone(), json.clone());
            }
            if matches!(k.as_str(), "closed" | "error") {
                self.live.lock().remove(id);
                self.ended(&live);
            }
            if k == "error" {
                tracing::warn!(stream = id, machine, error = %v.get("error").map(|e| e.to_string()).unwrap_or_default(), "a stream couldn't open");
            }
        }
        live.send(&ToViewer::Event(json));
        after
    }

    /// Copies a recording's files into kiln's store of bytes, in the background; past
    /// [`COPIES_MAX`] copies under way, it's served from its machine alone.
    pub fn keep(&self, hub: &Arc<Hub>, session: String, rec: Recording) {
        if self.copying.fetch_add(1, Ordering::AcqRel) < COPIES_MAX as u64 {
            crate::rt::spawn(keep_recording(Arc::clone(hub), session, rec));
        } else {
            self.copying.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!(target: "kiln.blobs", event = "put", recording = %rec.id, "too many recordings being copied: this one is served from its machine");
        }
    }

    /// A viewer joins: what it needs first, then the stream as it comes (a fresh key frame asked
    /// of the runner; the stream goes on if it was paused).
    pub async fn join(&self, hub: &Hub, live: &Arc<Live>) -> (u64, async_channel::Receiver<ToViewer>) {
        let (tx, rx) = async_channel::bounded(BEHIND);
        for e in live.said.lock().values() {
            let _ = tx.try_send(ToViewer::Event(e.clone()));
        }
        for c in [live.video.lock().clone(), live.audio.lock().clone()].into_iter().flatten() {
            let _ = tx.try_send(ToViewer::Chunk(c));
        }
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let first = {
            let mut v = live.viewers.lock();
            v.push((n, tx));
            v.len() == 1
        };
        live.unwatched.store(0, Ordering::Relaxed);
        let c = if first { json!({"pause": false, "keyframe": true}) } else { json!({"keyframe": true}) };
        live.control(hub, &c).await;
        (n, rx)
    }

    /// A viewer leaves; with nobody left, the stream pauses.
    pub async fn leave(&self, hub: &Hub, live: &Arc<Live>, viewer: u64) {
        let empty = {
            let mut v = live.viewers.lock();
            v.retain(|(n, _)| *n != viewer);
            v.is_empty()
        };
        if empty {
            live.unwatched.store(crate::util::now_ms(), Ordering::Relaxed);
            live.control(hub, &json!({"pause": true})).await;
        }
    }

    /// A stream that ended: whose it was, kept a while (a recording's end comes after).
    fn ended(&self, live: &Live) {
        let mut g = self.ended.lock();
        if g.len() > 256 {
            g.clear();
        }
        g.insert(live.id.clone(), Owner { session: live.session.clone(), machine: live.machine.clone(), profile: live.spec.get("profile").and_then(Value::as_str).map(str::to_owned) });
    }

    /// A recording of stream `id` done on `machine` (its runner's `recorded`): kept with its
    /// session, when the stream is that machine's.
    fn recorded(&self, machine: &str, id: &str, r: &Value) -> Option<(String, Recording)> {
        let owner = self.get(id).map(|l| Owner { session: l.session.clone(), machine: l.machine.clone(), profile: l.spec.get("profile").and_then(Value::as_str).map(str::to_owned) }).or_else(|| self.ended.lock().get(id).cloned());
        let owner = owner.filter(|o| o.machine == machine)?;
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let rec = Recording {
            id: hex::encode(rand::random::<[u8; 16]>()),
            stream: id.to_owned(),
            at: r.get("at").and_then(Value::as_i64).unwrap_or_else(crate::util::now_ms),
            seconds: r.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
            bytes: r.get("bytes").and_then(Value::as_u64).unwrap_or(0),
            pictures: r.get("pictures").and_then(Value::as_u64).unwrap_or(0),
            video: s("video"),
            inputs: s("inputs"),
            test: s("test"),
            machine: owner.machine,
            profile: owner.profile,
            kept: HashMap::new(),
        };
        self.recordings.lock().entry(owner.session.clone()).or_default().push(rec.clone());
        Some((owner.session, rec))
    }

    /// A recording's file copied into kiln's store of bytes: served from there from now on.
    /// Whether the recording is known still (not, once its conversation has closed).
    fn kept(&self, session: &str, rid: &str, what: &str, meta: super::files::Meta) -> bool {
        match self.recordings.lock().get_mut(session).and_then(|v| v.iter_mut().find(|r| r.id == rid)) {
            Some(r) => {
                r.kept.insert(what.to_owned(), meta);
                true
            }
            None => false,
        }
    }

    /// Session `session`'s recordings `person` may see.
    pub fn recordings(&self, session: &str, person: Option<&str>) -> Vec<Recording> {
        self.recordings.lock().get(session).map(|v| v.iter().filter(|r| r.for_whom(person)).cloned().collect()).unwrap_or_default()
    }

    /// Close a stream (its tab goes on its runner).
    pub async fn close(&self, hub: &Hub, id: &str) -> bool {
        let Some(live) = self.live.lock().remove(id) else { return false };
        self.ended(&live);
        hub.command_start(&live.machine, &ServerMsg::StreamClose { stream: live.id.clone(), session: live.session.clone() }).await.log_when_answered(crate::hub::LOG_WITHIN);
        live.send(&ToViewer::Event(json!({"closed": {"why": "it was closed"}}).to_string()));
        true
    }

    /// A conversation that's closed: its streams go, and its recordings (their files go with it on
    /// its machine, and from kiln's store of bytes).
    pub async fn forget(&self, hub: &Hub, session: &str) {
        for l in self.of(session) {
            self.close(hub, &l.id).await;
        }
        self.recordings.lock().remove(session);
        let (blobs, prefix) = (Arc::clone(&hub.blobs.blobs), format!("{}/", recordings_prefix(session)));
        crate::rt::spawn(async move {
            let mut gone = 0u64;
            for _ in 0..1000 {
                let Ok(page) = blobs.list(&prefix, None, kiln_seams::blobs::LIST_MAX).await else { break };
                for o in &page.objects {
                    if blobs.delete(&o.key).await.is_ok() {
                        gone = gone.saturating_add(1);
                    }
                }
                if !page.truncated || page.objects.is_empty() {
                    break;
                }
            }
            if gone > 0 {
                crate::metrics::blobs().collected.add(&["recordings"], gone);
            }
        });
    }

    /// A machine gone: its streams end (their viewers are told).
    pub fn machine_lost(&self, machine: &str) {
        let gone: Vec<Arc<Live>> = {
            let mut g = self.live.lock();
            let ids: Vec<String> = g.values().filter(|l| l.machine == machine).map(|l| l.id.clone()).collect();
            ids.iter().filter_map(|i| g.remove(i)).collect()
        };
        for l in &gone {
            self.ended(l);
        }
        for l in gone {
            l.send(&ToViewer::Event(json!({"closed": {"why": format!("{machine} went away")}}).to_string()));
        }
    }
}

/// Where a session's recordings are kept in kiln's store of bytes: `recordings/<session>` (its id
/// when that's a plain name, else its SHA-256).
pub fn recordings_prefix(session: &str) -> String {
    let name = if !session.is_empty() && session.len() <= 128 && session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        session.to_owned()
    } else {
        crate::util::sha256_hex(session.as_bytes())
    };
    format!("recordings/{name}")
}

/// Where a recording's file `what` is kept.
pub fn recording_key(session: &str, rid: &str, what: &str) -> String {
    format!("{}/{rid}/{what}", recordings_prefix(session))
}

/// Copies a recording's files from its machine into kiln's store of bytes, a piece at a time,
/// each whole or not at all: one that changes on the way, or is larger than [`RECORDING_MAX`],
/// stays on its machine alone.
async fn keep_recording(hub: Arc<Hub>, session: String, rec: Recording) {
    let location = super::registry::Location::Machine(rec.machine.clone());
    for (what, path) in [("video", &rec.video), ("inputs", &rec.inputs), ("test", &rec.test)] {
        if path.is_empty() {
            continue;
        }
        let key = recording_key(&session, &rec.id, what);
        match copy_file(&hub, &location, &session, path, &key).await {
            Ok(meta) => {
                // Its conversation closed while it was copied: the copy goes as the rest did.
                if !hub.harness.streams.kept(&session, &rec.id, what, meta) && hub.blobs.blobs.delete(&key).await.is_ok() {
                    crate::metrics::blobs().collected.inc(&["recordings"]);
                }
            }
            Err(e) => tracing::warn!(target: "kiln.blobs", event = "put", recording = %rec.id, file = what, error = %format!("{e:#}"), "a recording's file stays on its machine alone"),
        }
    }
    hub.harness.streams.copying.fetch_sub(1, Ordering::AcqRel);
}

async fn copy_file(hub: &Hub, location: &super::registry::Location, session: &str, path: &str, key: &str) -> anyhow::Result<super::files::Meta> {
    use super::files::PIECE;
    let mut at = 0u64;
    let mut writer = crate::blobs::io::Writer::new(hub.blobs.blobs.as_ref(), key);
    let mut first: Option<super::files::Meta> = None;
    loop {
        let piece = super::kept::piece(hub, location, session, path, at, PIECE).await?;
        let Some((answer, data)) = piece else {
            writer.abort().await;
            anyhow::bail!("its machine's runner doesn't read files");
        };
        let Some(meta) = answer.meta else {
            writer.abort().await;
            anyhow::bail!(answer.error.unwrap_or_else(|| "it couldn't be read".to_owned()));
        };
        let m = first.get_or_insert_with(|| meta.clone());
        if meta.size != m.size || meta.mtime != m.mtime || meta.size > RECORDING_MAX {
            writer.abort().await;
            anyhow::bail!("it's over {} MiB, or it changed while it was copied", RECORDING_MAX >> 20);
        }
        if data.is_empty() {
            break;
        }
        at = at.saturating_add(data.len() as u64);
        if let Err(e) = writer.write(&data).await {
            writer.abort().await;
            return Err(anyhow::anyhow!("{e}"));
        }
        if at >= meta.size {
            break;
        }
    }
    let Some(meta) = first.filter(|m| m.size == at) else {
        writer.abort().await;
        anyhow::bail!("{at} bytes of it were read: it ended short");
    };
    writer.finish().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(meta)
}

impl Live {
    fn send(&self, m: &ToViewer) {
        let mut slow = false;
        self.viewers.lock().retain(|(_, tx)| match tx.try_send(m.clone()) {
            Ok(()) => true,
            Err(async_channel::TrySendError::Full(_)) => {
                slow = true;
                true
            }
            Err(async_channel::TrySendError::Closed(_)) => false,
        });
        let _ = slow;
    }

    /// What a viewer asks of the stream, to its runner.
    pub async fn control(&self, hub: &Hub, c: &Value) {
        if let Some(r) = c.get("resize") {
            *self.resized.lock() = Some(r.clone());
        }
        // In order, each answered, none waited for: a viewer's asks follow one another closely.
        hub.command_start(&self.machine, &ServerMsg::StreamControl { stream: self.id.clone(), session: self.session.clone(), json: c.to_string() }).await.log_when_answered(crate::hub::LOG_WITHIN);
    }

    /// Its picture's codec (WebCodecs' string), as its last configuration said, else as it was
    /// opened (H.264 unless it said otherwise).
    pub fn codec(&self) -> String {
        let said = self.video.lock().as_ref().and_then(|c| serde_json::from_slice::<Value>(c.get(12..)?).ok()).and_then(|v| v.get("codec").and_then(Value::as_str).map(str::to_owned));
        said.or_else(|| self.spec.get("codec").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_owned)).unwrap_or_else(|| "h264".to_owned())
    }

    /// What a viewer did, to its runner.
    pub async fn input(&self, hub: &Hub, events: &Value) {
        hub.command_start(&self.machine, &ServerMsg::StreamInput { stream: self.id.clone(), session: self.session.clone(), json: events.to_string() }).await.log_when_answered(crate::hub::LOG_WITHIN);
    }

    /// How it is, for the API.
    /// Whether `person` (`Caller::opener`; none for a job, an agent or a narrowed token) may see
    /// and use it: one in a person's own profile (their sign-ins) is theirs alone, whoever else
    /// the conversation is shared with.
    pub fn for_whom(&self, person: Option<&str>) -> bool {
        visible(self.spec.get("profile").and_then(Value::as_str), person)
    }

    pub fn view(&self) -> Value {
        let said: HashMap<String, Value> = self.said.lock().iter().map(|(k, v)| (k.clone(), serde_json::from_str::<Value>(v).ok().and_then(|v| v.get(k).cloned()).unwrap_or_default())).collect();
        json!({"id": self.id, "session": self.session, "machine": self.machine, "spec": self.spec, "by": self.by, "at": self.at, "viewers": self.viewers.lock().len(), "said": said})
    }
}

/// What a person may open live from `session`, asked of its runner (`crate::browser::apps`):
/// `op` "look" (the ports its machine serves, its project's app), "start" (that app) or "stop".
pub async fn preview(hub: &Arc<Hub>, session: &str, op: &str) -> anyhow::Result<Value> {
    let machine = match hub.harness.location(session) {
        Some(super::registry::Location::Machine(m)) => m,
        Some(super::registry::Location::Local) => anyhow::bail!("live pages come from runners: this session runs on the server itself"),
        None => anyhow::bail!("no such agent session"),
    };
    // Starting an app waits for it to listen (its preview file says how long, at most ten minutes).
    let wait = if op == "start" { Duration::from_secs(610) } else { Duration::from_secs(20) };
    let json = json!({"op": op}).to_string();
    let answer = hub
        .ask_routed(&machine, crate::proto::CAP_PREVIEW, wait, |req| ServerMsg::Preview { req, session: session.to_owned(), json })
        .await?
        .ok_or_else(|| anyhow::anyhow!("{machine} runs a kiln too old for this: upgrade its runner"))?
        .ok_or_else(|| anyhow::anyhow!("{machine} said nothing"))?;
    let v: Value = serde_json::from_str(&answer)?;
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        anyhow::bail!("{e}");
    }
    Ok(v)
}

/// The profile a person's stream runs in: their conversation's (shared with its agent's streams,
/// the default), a clean one (fresh each time), or their own (kept on the agent's machine for
/// them, across conversations: where its agents can read it).
pub fn profile_for(session: &str, asked: Option<&str>, person: Option<&str>) -> anyhow::Result<String> {
    Ok(match asked.unwrap_or("conversation") {
        "conversation" => format!("conversation:{session}"),
        "clean" => "clean".to_owned(),
        "personal" => personal(person.ok_or_else(|| anyhow::anyhow!("a profile of your own is for a person signed in as themselves, not a job, an agent or a token with a narrower grant"))?),
        other => anyhow::bail!("a stream's profile is conversation, clean or personal, not {other:?}"),
    })
}

/// A person's own profile, by who they are (`Caller::opener`: "owner", "user:<id>"): readable,
/// and distinct for each.
fn personal(person: &str) -> String {
    use sha2::Digest as _;
    let readable: String = person.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).take(40).collect();
    let distinct: String = hex::encode(sha2::Sha256::digest(person.as_bytes())).chars().take(12).collect();
    format!("personal:{readable}-{distinct}")
}

/// What an agent's MCP server asks of its session's streams (`crate::mcp::runner_ask`, op
/// "stream"): `{"do": "open", "url", "title"?}` streams a page into its conversation. Its size is
/// a phone's until a viewer says its own.
pub async fn ask(hub: &Arc<Hub>, session: &str, q: &Value) -> anyhow::Result<Value> {
    match q.get("do").and_then(Value::as_str) {
        Some("open") => {
            let url = q.get("url").and_then(Value::as_str).unwrap_or("");
            let title = q.get("title").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).unwrap_or(url);
            let mut spec = json!({"url": url, "title": title, "width": 390, "height": 640, "scale": 2.0, "mobile": false});
            for k in ["engine", "device"] {
                if let (Some(o), Some(v)) = (spec.as_object_mut(), q.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())) {
                    o.insert(k.into(), json!(v));
                }
            }
            let live = hub.harness.streams.open(hub, session, spec, &format!("agent:{session}")).await?;
            Ok(json!({"id": live.id, "session": session, "url": url, "title": title, "engine": live.spec.get("engine"), "device": live.spec.get("device")}))
        }
        other => anyhow::bail!("streams don't do {other:?}"),
    }
}

/// Close a stream once nobody has looked at it for [`UNWATCHED`].
async fn close_when_unwatched(hub: Arc<Hub>, id: String) {
    loop {
        crate::rt::sleep(Duration::from_secs(30)).await;
        let Some(live) = hub.harness.streams.get(&id) else { return };
        let since = live.unwatched.load(Ordering::Relaxed);
        if since > 0 && crate::util::now_ms() - since > i64::try_from(UNWATCHED.as_millis()).unwrap_or(i64::MAX) {
            hub.harness.streams.close(&hub, &id).await;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(id: &str, machine: &str) -> Arc<Live> {
        Arc::new(Live {
            id: id.into(),
            session: "s1".into(),
            machine: machine.into(),
            spec: json!({"url": "http://localhost:5173/"}),
            by: "owner".into(),
            at: 0,
            viewers: Mutex::default(),
            video: Mutex::default(),
            audio: Mutex::default(),
            said: Mutex::default(),
            unwatched: AtomicI64::new(0),
            resized: Mutex::default(),
        })
    }

    #[test]
    fn a_viewer_is_sent_chunks_and_events_and_what_joiners_need_is_kept() -> anyhow::Result<()> {
        let r = Relay::default();
        let l = live("v1", "m1");
        r.live.lock().insert("v1".into(), l.clone());
        let (tx, rx) = async_channel::bounded(16);
        l.viewers.lock().push((1, tx));
        r.chunk("m1", "v1", crate::browser::stream::frame(b'c', b'-', 0, 0, 1, b"{\"codec\":\"avc1.42E01f\"}"));
        r.chunk("m1", "v1", crate::browser::stream::frame(b'c', b'a', 0, 0, 1, b"{\"codec\":\"opus\"}"));
        r.chunk("m1", "v1", crate::browser::stream::frame(b'v', b'k', 5, 0, 1, b"key"));
        r.event("m1", "v1", json!({"navigated": {"url": "http://localhost:5173/a"}}).to_string());
        // Another machine's chunk for it isn't its.
        r.chunk("m2", "v1", crate::browser::stream::frame(b'v', b'k', 6, 0, 1, b"not mine"));
        assert_eq!(rx.len(), 4);
        assert!(l.video.lock().as_ref().is_some_and(|c| c.ends_with(b"avc1.42E01f\"}")));
        assert!(l.audio.lock().as_ref().is_some_and(|c| c.ends_with(b"opus\"}")));
        assert!(l.said.lock().contains_key("navigated"));
        // Its window out of sight where it runs (Safari's, its Mac locked): a viewer joining later
        // is told so, as it is now.
        r.event("m1", "v1", json!({"hidden": true}).to_string());
        assert!(l.said.lock().get("hidden").is_some_and(|h| h.contains("true")));
        r.event("m1", "v1", json!({"hidden": false}).to_string());
        assert!(l.said.lock().get("hidden").is_some_and(|h| h.contains("false")));
        // A size its viewer asked for before it opened is asked again as it does.
        *l.resized.lock() = Some(json!({"width": 400, "height": 300, "scale": 2}));
        let again = r.event("m1", "v1", json!({"opened": {"width": 390}}).to_string()).again;
        assert_eq!(again.map(|(_, c)| c), Some(json!({"resize": {"width": 400, "height": 300, "scale": 2}})));
        while rx.try_recv().is_ok() {}
        // Its tabs, as they are now; a list said before (arriving after) is no news.
        r.event("m1", "v1", json!({"tabs": {"list": [{"id": "t1", "url": "http://localhost:5173/a", "title": "A"}], "shown": "t1", "seq": 2}}).to_string());
        r.event("m1", "v1", json!({"tabs": {"list": [], "shown": "", "seq": 1}}).to_string());
        assert!(l.said.lock().get("tabs").is_some_and(|t| t.contains("\"shown\":\"t1\"")));
        assert_eq!(rx.len(), 1);
        while rx.try_recv().is_ok() {}
        assert_eq!(l.codec(), "avc1.42E01f");
        assert_eq!(live("v9", "m1").codec(), "h264");
        // It closed on its machine: it's gone, and its viewers are told.
        r.event("m1", "v1", json!({"closed": {"why": "the page went away"}}).to_string());
        assert!(r.get("v1").is_none());
        assert_eq!(rx.len(), 1);
        Ok(())
    }

    #[test]
    fn a_persons_own_profile_is_theirs_alone() -> anyhow::Result<()> {
        assert_eq!(profile_for("s1", None, None)?, "conversation:s1");
        assert_eq!(profile_for("s1", Some("clean"), None)?, "clean");
        // Not a job's, an agent's or a narrowed token's; nor anything but the three.
        assert!(profile_for("s1", Some("personal"), None).is_err());
        assert!(profile_for("s1", Some("personal:user_x"), Some("owner")).is_err());
        let ada = profile_for("s1", Some("personal"), Some("user:ada"))?;
        assert!(ada.starts_with("personal:user_ada-"), "{ada}");
        // Two people whose names read alike still have their own.
        assert_ne!(ada, profile_for("s1", Some("personal"), Some("user_ada"))?);
        let mut l = live("p", "m1");
        if let Some(l) = Arc::get_mut(&mut l) {
            l.spec = json!({"url": "https://example.com/", "profile": ada});
        }
        assert!(l.for_whom(Some("user:ada")));
        assert!(!l.for_whom(Some("user:bo")) && !l.for_whom(Some("owner")) && !l.for_whom(None));
        // The conversation's own are everyone's it's shared with.
        assert!(live("c", "m1").for_whom(None));
        Ok(())
    }

    #[test]
    fn a_recording_is_kept_with_its_session_even_said_after_its_stream_ended() {
        let r = Relay::default();
        let l = live("v1", "m1");
        r.live.lock().insert("v1".into(), l.clone());
        r.event("m1", "v1", json!({"recorded": {"video": "/s/live-1.mp4", "inputs": "/s/live-1.inputs.jsonl", "test": "/s/live-1.spec.ts", "seconds": 2.5}}).to_string());
        // Its stream gone, a late one still counts; another machine's never does.
        r.live.lock().remove("v1");
        r.ended(&l);
        r.event("m1", "v1", json!({"recorded": {"video": "/s/live-2.mp4"}}).to_string());
        r.event("m2", "v1", json!({"recorded": {"video": "/x/theirs.mp4"}}).to_string());
        let recs = r.recordings("s1", None);
        assert_eq!(recs.iter().map(|x| x.video.as_str()).collect::<Vec<_>>(), ["/s/live-1.mp4", "/s/live-2.mp4"]);
        assert_eq!(recs.first().and_then(|x| x.file("test")), Some("/s/live-1.spec.ts"));
        assert!(recs.first().and_then(|x| x.file("elsewhere")).is_none());
        // A viewer is told its id.
        let l = live("v2", "m1");
        let (tx, rx) = async_channel::bounded(4);
        l.viewers.lock().push((1, tx));
        r.live.lock().insert("v2".into(), l);
        r.event("m1", "v2", json!({"recorded": {"video": "/s/live-3.mp4"}}).to_string());
        let said: Value = match rx.try_recv() {
            Ok(ToViewer::Event(e)) => serde_json::from_str(&e).unwrap_or_default(),
            _ => Value::Null,
        };
        assert!(said.pointer("/recorded/id").and_then(Value::as_str).is_some_and(|id| id.len() == 32), "{said}");
    }

    #[test]
    fn a_machine_gone_ends_its_streams_and_no_others() {
        let r = Relay::default();
        r.live.lock().insert("a".into(), live("a", "m1"));
        r.live.lock().insert("b".into(), live("b", "m2"));
        r.machine_lost("m1");
        assert!(r.get("a").is_none() && r.get("b").is_some());
    }

    /// CP-T10.1: a recording's file is copied into kiln's store of bytes whole (in parts, larger
    /// than one), and goes from it when its conversation closes.
    #[test]
    fn a_recordings_file_is_kept_in_the_store_until_its_conversation_closes() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let dir = tempfile::tempdir()?;
            let video = dir.path().join("live-1.mp4");
            let data = kiln_seams::blobs::noise((20 << 20) + 5, 9);
            std::fs::write(&video, &data)?;
            let key = recording_key("s1", "r1", "video");
            let meta = copy_file(&hub, &super::super::registry::Location::Local, "s1", &video.to_string_lossy(), &key).await?;
            assert_eq!(meta.size, data.len() as u64);
            let (kept, got) = hub.blobs.blobs.get(&key, None).await?;
            assert!(got == data && kept.etag == crate::util::sha256_hex(&data), "{} bytes kept", got.len());
            hub.harness.streams.forget(&hub, "s1").await;
            for _ in 0..500 {
                if matches!(hub.blobs.blobs.head(&key).await, Err(kiln_seams::blobs::Refusal::NotFound(_))) {
                    return Ok(());
                }
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            anyhow::bail!("the recording outlived its conversation in the store")
        })
    }
}
