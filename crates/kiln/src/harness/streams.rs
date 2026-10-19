//! Pages streamed from agents' machines (docs/previews.md, "Streams"), relayed by the server to
//! whoever views them: a stream runs where its session does (`crate::browser::stream`), its
//! chunks come up its runner's stream lane, and go out to each viewer's socket as they come. A
//! viewer that joins gets the decoder's configuration and a fresh key frame (asked of the runner),
//! so it sees the page at once; one too slow for a chunk loses it, and the next key frame mends it.
//! With nobody looking, the stream pauses (nothing is captured or sent), and after a while it
//! closes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::proto::{CAP_STREAM, LANE_STREAM, ServerMsg};

/// A stream nobody has looked at for this long closes.
const UNWATCHED: Duration = Duration::from_secs(600);
/// Chunks a viewer may be behind before it loses one.
const BEHIND: usize = 240;

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
    /// What it said that a viewer joining should know: opened, where it is, its title.
    said: Mutex<HashMap<String, String>>,
    /// When the last viewer left (unix ms; 0 while someone looks).
    unwatched: AtomicI64,
}

#[derive(Default)]
pub struct Relay {
    live: Mutex<HashMap<String, Arc<Live>>>,
    next: AtomicU64,
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
        });
        self.live.lock().insert(id.clone(), live.clone());
        if !hub.send_to_agent(&machine, LANE_STREAM, &msg).await {
            self.live.lock().remove(&id);
            anyhow::bail!("{machine} is not connected");
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
    /// stream that closed or failed to open goes.
    pub fn event(&self, machine: &str, id: &str, json: String) {
        let Some(live) = self.get(id).filter(|l| l.machine == machine) else { return };
        let v: Value = serde_json::from_str(&json).unwrap_or_default();
        if let Some(k) = v.as_object().and_then(|o| o.keys().next()).cloned() {
            if matches!(k.as_str(), "opened" | "navigated" | "loaded") {
                live.said.lock().insert(k.clone(), json.clone());
            }
            if matches!(k.as_str(), "closed" | "error") {
                self.live.lock().remove(id);
            }
            if k == "error" {
                tracing::warn!(stream = id, machine, error = %v.get("error").map(|e| e.to_string()).unwrap_or_default(), "a stream couldn't open");
            }
        }
        live.send(&ToViewer::Event(json));
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

    /// Close a stream (its tab goes on its runner).
    pub async fn close(&self, hub: &Hub, id: &str) -> bool {
        let Some(live) = self.live.lock().remove(id) else { return false };
        let _ = hub.send_to_agent(&live.machine, LANE_STREAM, &ServerMsg::StreamClose { stream: live.id.clone(), session: live.session.clone() }).await;
        live.send(&ToViewer::Event(json!({"closed": {"why": "it was closed"}}).to_string()));
        true
    }

    /// A conversation that's closed: its streams go.
    pub async fn forget(&self, hub: &Hub, session: &str) {
        for l in self.of(session) {
            self.close(hub, &l.id).await;
        }
    }

    /// A machine gone: its streams end (their viewers are told).
    pub fn machine_lost(&self, machine: &str) {
        let gone: Vec<Arc<Live>> = {
            let mut g = self.live.lock();
            let ids: Vec<String> = g.values().filter(|l| l.machine == machine).map(|l| l.id.clone()).collect();
            ids.iter().filter_map(|i| g.remove(i)).collect()
        };
        for l in gone {
            l.send(&ToViewer::Event(json!({"closed": {"why": format!("{machine} went away")}}).to_string()));
        }
    }
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
        let _ = hub.send_to_agent(&self.machine, LANE_STREAM, &ServerMsg::StreamControl { stream: self.id.clone(), session: self.session.clone(), json: c.to_string() }).await;
    }

    /// What a viewer did, to its runner.
    pub async fn input(&self, hub: &Hub, events: &Value) {
        let _ = hub.send_to_agent(&self.machine, LANE_STREAM, &ServerMsg::StreamInput { stream: self.id.clone(), session: self.session.clone(), json: events.to_string() }).await;
    }

    /// How it is, for the API.
    /// Whether `person` (`Caller::opener`; none for a job, an agent or a narrowed token) may see
    /// and use it: one in a person's own profile (their sign-ins) is theirs alone, whoever else
    /// the conversation is shared with.
    pub fn for_whom(&self, person: Option<&str>) -> bool {
        match self.spec.get("profile").and_then(Value::as_str) {
            Some(p) if p.starts_with("personal:") => person.is_some_and(|me| personal(me) == p),
            _ => true,
        }
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
            let spec = json!({"url": url, "title": title, "width": 390, "height": 640, "scale": 2.0, "mobile": false});
            let live = hub.harness.streams.open(hub, session, spec, &format!("agent:{session}")).await?;
            Ok(json!({"id": live.id, "session": session, "url": url, "title": title}))
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
        // It closed on its machine: it's gone, and its viewers are told.
        r.event("m1", "v1", json!({"closed": {"why": "the page went away"}}).to_string());
        assert!(r.get("v1").is_none());
        assert_eq!(rx.len(), 5);
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
    fn a_machine_gone_ends_its_streams_and_no_others() {
        let r = Relay::default();
        r.live.lock().insert("a".into(), live("a", "m1"));
        r.live.lock().insert("b".into(), live("b", "m2"));
        r.machine_lost("m1");
        assert!(r.get("a").is_none() && r.get("b").is_some());
    }
}
