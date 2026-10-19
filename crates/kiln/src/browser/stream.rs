//! Pages streamed from this machine (docs/previews.md, "Streams"): a page that needs the agent's
//! machine (its dev server, its files, its network, its GPU) runs here, in a real browser, and its
//! picture and sound go to whoever looks at it, live; what they do in it comes back.
//!
//! Each stream is a tab of a browser of its profile's (a conversation's: its cookies kept between
//! its streams, removed as it closes; a person's own, kept across conversations; or a clean one,
//! a browser of the stream's own), with kiln's capture extension in it (`extension/`).
//! The extension captures the tab as its compositor draws it, only when something changed, and
//! encodes it with WebCodecs: H.264 (or VP8/VP9/AV1 when asked) at the viewer's own size and pixel
//! density, Opus for its sound, a sharp key frame once the page is still. Its chunks come to kiln
//! over the DevTools protocol (the binding `kilnOut` in the extension's offscreen document); what
//! the viewer does goes to the tab as DevTools input (pointer, touch, wheel, keys by meaning, text
//! and composition).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::Browser;
use super::page::Page;

const MANIFEST: &str = include_str!("extension/manifest.json");
const WORKER: &str = include_str!("extension/sw.js");
const OFFSCREEN_HTML: &str = include_str!("extension/offscreen.html");
const OFFSCREEN_JS: &str = include_str!("extension/offscreen.js");
/// The extension's id: Chrome's, from the public key in its manifest.
pub const EXTENSION_ID: &str = "nlhjdnlmecmfjkicpfhniegbklccffbb";

/// The most of a chunk that goes in one message to the server (its interactive frames' bound is a
/// mebibyte); a bigger one goes in parts.
pub const PART: usize = 900 * 1024;
/// A profile's browser with no streams stops after this long.
const IDLE_BROWSER: Duration = Duration::from_secs(600);

/// What a stream is: the page, the viewer's window (CSS pixels, density, a phone's or not), the
/// browser profile it runs in, and how it's encoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub url: String,
    /// A phone's until its viewer says its own size.
    #[serde(default = "phone_width")]
    pub width: u32,
    #[serde(default = "phone_height")]
    pub height: u32,
    #[serde(default = "two")]
    pub scale: f64,
    #[serde(default)]
    pub mobile: bool,
    /// `conversation:<session>` (kept between its streams), `personal:<them>` (a person's own,
    /// kept across conversations) or `clean` (new each time).
    #[serde(default = "clean")]
    pub profile: String,
    /// "h264" (the default), "vp8", "vp9", "av1".
    #[serde(default)]
    pub codec: Option<String>,
    /// Bits a second (4 Mb/s by default).
    #[serde(default)]
    pub bitrate: Option<u32>,
    /// The agent session it streams for (the server's message says it).
    #[serde(default)]
    pub session: Option<String>,
}

fn two() -> f64 {
    2.0
}
fn phone_width() -> u32 {
    390
}
fn phone_height() -> u32 {
    640
}
fn clean() -> String {
    "clean".to_owned()
}

/// What a stream sends the server: a chunk (`data`: kind, flags, timestamp, part, then the
/// bytes; see [`frame`]) or an event (JSON: opened, navigated, focus, closed, an error).
#[derive(Debug, Clone)]
pub enum Out {
    Frame { stream: String, data: Vec<u8> },
    Event { stream: String, json: String },
}

/// A chunk as the server gets it: `kind` (`b'v'` video, `b'a'` audio, `b'c'` a decoder's
/// configuration), `flags` (`b'k'` a key frame, `b'a'` the audio's configuration), the timestamp
/// (µs, big-endian), this part's index and the count of parts (a byte each), then the bytes.
pub fn frame(kind: u8, flags: u8, ts: i64, part: u8, parts: u8, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + bytes.len());
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&ts.to_be_bytes());
    out.push(part);
    out.push(parts);
    out.extend_from_slice(bytes);
    out
}

/// The extension's folder under kiln's state, written when this build's isn't there.
fn extension(dir: &Path) -> anyhow::Result<PathBuf> {
    let mut h = Sha256::new();
    for part in [MANIFEST, WORKER, OFFSCREEN_HTML, OFFSCREEN_JS] {
        h.update(part.as_bytes());
    }
    let tag = hex::encode(h.finalize()).get(..12).unwrap_or("x").to_owned();
    let at = dir.join("browser").join("extension").join(tag);
    if !at.join("offscreen.js").exists() {
        std::fs::create_dir_all(&at)?;
        std::fs::write(at.join("manifest.json"), MANIFEST)?;
        std::fs::write(at.join("sw.js"), WORKER)?;
        std::fs::write(at.join("offscreen.html"), OFFSCREEN_HTML)?;
        std::fs::write(at.join("offscreen.js"), OFFSCREEN_JS)?;
    }
    Ok(at)
}

/// The folder name of a kept profile ("conversation:<session>", "personal:<who>").
fn profile_key(profile: &str) -> String {
    profile.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_")
}

/// A profile's browser, streaming: its extension's worker and offscreen document attached, and
/// where each stream's chunks go.
struct Streamer {
    browser: Arc<Browser>,
    /// A clean profile's: its one stream's, stopped (and its profile removed) as that closes.
    alone: bool,
    worker: String,
    offscreen: futures::lock::Mutex<Option<String>>,
    outs: Arc<Mutex<HashMap<String, async_channel::Sender<Out>>>>,
}

/// One stream: its tab, and what its viewers do, in order.
pub struct Stream {
    pub id: String,
    /// The agent session it streams for.
    pub session: String,
    streamer: Arc<Streamer>,
    page: Page,
    touches: Mutex<HashMap<i64, (f64, f64)>>,
    inputs: async_channel::Sender<Vec<Input>>,
    /// Laid out as a phone's (touch, a phone's viewport): what its viewer is.
    mobile: std::sync::atomic::AtomicBool,
    /// In a person's own profile (their sign-ins): theirs to use, not an agent's.
    personal: bool,
    /// Where what it says goes (its viewers, through the server).
    out: async_channel::Sender<Out>,
}

/// This machine's streams, by id.
#[derive(Default)]
pub struct Streams {
    streamers: futures::lock::Mutex<HashMap<String, Arc<Streamer>>>,
    open: Mutex<HashMap<String, Arc<Stream>>>,
}

static STREAMS: std::sync::OnceLock<Streams> = std::sync::OnceLock::new();

pub fn streams() -> &'static Streams {
    STREAMS.get_or_init(Streams::default)
}

impl Streams {
    /// The browser for `profile`, started (with the extension) when it isn't running.
    async fn streamer(&self, dir: &Path, profile: &str) -> anyhow::Result<Arc<Streamer>> {
        let mut g = self.streamers.lock().await;
        if let Some(s) = g.get(profile)
            && s.browser.alive()
        {
            return Ok(s.clone());
        }
        let ext = extension(dir)?;
        // A conversation's or a person's profile is kept between its streams; a clean one isn't.
        let persistent = (profile.starts_with("conversation:") || profile.starts_with("personal:")).then(|| dir.join("browser").join("streams").join(profile_key(profile)));
        let browser = Arc::new(Browser::start_with(dir, super::Launch { profile: persistent, extension: Some(ext), id: EXTENSION_ID }).await?);
        let cdp = browser.cdp.clone();
        let worker = super::target(&cdp, |t| t.get("type").and_then(Value::as_str) == Some("service_worker") && t.get("url").and_then(Value::as_str).is_some_and(|u| u.starts_with(&format!("chrome-extension://{EXTENSION_ID}/")))).await?;
        let worker = cdp.call(None, "Target.attachToTarget", json!({"targetId": worker, "flatten": true})).await?.get("sessionId").and_then(Value::as_str).unwrap_or("").to_owned();
        let s = Arc::new(Streamer { browser, alone: profile.starts_with("clean:"), worker, offscreen: futures::lock::Mutex::new(None), outs: Arc::default() });
        g.insert(profile.to_owned(), s.clone());
        Ok(s)
    }

    /// Open a stream of `spec` as `id`; its chunks and events go to `out`.
    pub async fn open(&self, dir: &Path, id: &str, spec: &Spec, out: async_channel::Sender<Out>) -> anyhow::Result<Value> {
        // A clean profile is new each time: a browser of the stream's own.
        let profile = if spec.profile == "clean" { format!("clean:{id}") } else { spec.profile.clone() };
        let s = self.streamer(dir, &profile).await?;
        let mark = hex::encode(rand::random::<[u8; 8]>());
        let page = s.browser.tab(&format!("about:blank#kiln-{mark}")).await?;
        let scale = spec.scale.clamp(1.0, 3.0);
        page.viewport(spec.width, spec.height, scale, spec.mobile).await?;
        page.call("Page.enable", json!({})).await?;
        // The tab, as the extension knows it.
        let mut tab = Value::Null;
        // A moment for the extension to see it (longer on a busy machine).
        for _ in 0..100 {
            tab = super::evaluate(&s.browser.cdp, &s.worker, &format!("kiln.tab({})", json!(mark))).await?;
            if tab.is_number() {
                break;
            }
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
        anyhow::ensure!(tab.is_number(), "the capture extension doesn't see the stream's tab");
        s.outs.lock().insert(id.to_owned(), out.clone());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (pw, ph) = ((f64::from(spec.width) * scale).round() as u32, (f64::from(spec.height) * scale).round() as u32);
        let opts = json!({"width": pw, "height": ph, "framerate": 60, "bitrate": spec.bitrate.unwrap_or(4_000_000), "codec": spec.codec.clone().unwrap_or_else(|| "h264".to_owned())});
        let started = super::evaluate(&s.browser.cdp, &s.worker, &format!("kiln.capture({tab}, {}, {opts})", json!(id))).await?;
        if started.get("ok").and_then(Value::as_bool) != Some(true) {
            s.outs.lock().remove(id);
            page.close().await;
            anyhow::bail!("the page couldn't be captured: {}", started.get("error").and_then(Value::as_str).unwrap_or("it said nothing"));
        }
        self.listen(&s).await?;
        let _ = super::evaluate(&s.browser.cdp, &s.offscreen.lock().await.clone().unwrap_or_default(), &format!("kiln.keyframe({})", json!(id))).await;
        // What the page does that the viewer's side shows: where it is, and a field taking text.
        page.call("Runtime.enable", json!({})).await?;
        page.call("Runtime.addBinding", json!({"name": "kilnFocus"})).await?;
        page.before_load(FOCUS).await?;
        let (inputs, queued) = async_channel::bounded::<Vec<Input>>(256);
        let stream = Arc::new(Stream { id: id.to_owned(), session: spec.session.clone().unwrap_or_default(), streamer: s.clone(), page, touches: Mutex::default(), inputs, mobile: std::sync::atomic::AtomicBool::new(spec.mobile), personal: spec.profile.starts_with("personal:"), out: out.clone() });
        crate::rt::spawn(watch(stream.clone(), out.clone()));
        // What viewers do, applied in the order they did it.
        let applying = Arc::downgrade(&stream);
        crate::rt::spawn(async move {
            while let Ok(events) = queued.recv().await {
                let Some(s) = applying.upgrade() else { return };
                if let Err(e) = s.input(events).await {
                    tracing::debug!(stream = %s.id, error = %format!("{e:#}"), "a viewer's input didn't reach the page");
                }
            }
        });
        stream.page.call("Page.navigate", json!({"url": spec.url})).await?;
        self.open.lock().insert(id.to_owned(), stream);
        Ok(json!({"opened": {"width": spec.width, "height": spec.height, "scale": scale, "audio": started.get("audio"), "platform": std::env::consts::OS}}))
    }

    /// The offscreen document's binding attached (once a browser's first capture made it), its
    /// chunks handed to their streams.
    async fn listen(&self, s: &Arc<Streamer>) -> anyhow::Result<()> {
        let mut off = s.offscreen.lock().await;
        if off.is_some() {
            return Ok(());
        }
        let cdp = s.browser.cdp.clone();
        let target = super::target(&cdp, |t| t.get("url").and_then(Value::as_str) == Some(&format!("chrome-extension://{EXTENSION_ID}/offscreen.html"))).await?;
        let session = cdp.call(None, "Target.attachToTarget", json!({"targetId": target, "flatten": true})).await?.get("sessionId").and_then(Value::as_str).unwrap_or("").to_owned();
        let events = cdp.listen(Some(&session));
        cdp.call(Some(&session), "Runtime.enable", json!({})).await?;
        cdp.call(Some(&session), "Runtime.addBinding", json!({"name": "kilnOut"})).await?;
        let outs = s.outs.clone();
        crate::rt::spawn(async move {
            while let Ok(e) = events.recv().await {
                if e.method != "Runtime.bindingCalled" || e.params.get("name").and_then(Value::as_str) != Some("kilnOut") {
                    continue;
                }
                let payload = e.params.get("payload").and_then(Value::as_str).unwrap_or("");
                let Some((stream, msgs)) = chunk(payload) else { continue };
                let tx = outs.lock().get(&stream).cloned();
                if let Some(tx) = tx {
                    for m in msgs {
                        // A viewer too slow for a chunk loses it (the next key frame mends it).
                        let _ = tx.try_send(m);
                    }
                }
            }
        });
        *off = Some(session);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<Stream>> {
        self.open.lock().get(id).cloned()
    }

    /// Close a stream: its capture stops, its tab goes. A browser left with no streams stops a
    /// while later (unless one opens meanwhile); a clean profile's stops now, and its profile goes.
    pub async fn close(&self, id: &str) {
        let Some(s) = self.open.lock().remove(id) else { return };
        let cdp = s.streamer.browser.cdp.clone();
        let off = s.streamer.offscreen.lock().await.clone().unwrap_or_default();
        let _ = super::evaluate(&cdp, &off, &format!("kiln.stop({})", json!(id))).await;
        s.streamer.outs.lock().remove(id);
        // Its tab, whoever else still holds the stream (its watcher ends as the tab does).
        let _ = cdp.call(None, "Target.closeTarget", json!({"targetId": s.page.target()})).await;
        let streamer = s.streamer.clone();
        drop(s);
        if streamer.alone {
            self.streamers.lock().await.retain(|_, s| !Arc::ptr_eq(s, &streamer));
            streamer.browser.stop();
            return;
        }
        crate::rt::spawn(async move {
            crate::rt::sleep(IDLE_BROWSER).await;
            streams().stop_if_idle(&streamer).await;
        });
    }

    /// Stop a profile's browser that has no streams open.
    async fn stop_if_idle(&self, streamer: &Arc<Streamer>) {
        if streamer.outs.lock().is_empty() {
            let mut g = self.streamers.lock().await;
            g.retain(|_, s| !Arc::ptr_eq(s, streamer));
            drop(g);
            streamer.browser.stop();
            tracing::info!("a streaming browser stopped: no streams for a while");
        }
    }

    /// A session's streams on this machine close (it closed), and its conversation's profile goes
    /// with them: its browser stops, its cookies and storage are removed.
    pub async fn close_session(&self, dir: &Path, session: &str) {
        let ids: Vec<String> = self.open.lock().values().filter(|s| s.session == session).map(|s| s.id.clone()).collect();
        for id in ids {
            self.close(&id).await;
        }
        let profile = format!("conversation:{session}");
        let browser = self.streamers.lock().await.remove(&profile);
        if let Some(s) = browser {
            s.browser.stop();
        }
        let kept = dir.join("browser").join("streams").join(profile_key(&profile));
        if kept.exists() {
            let _ = crate::rt::blocking(move || std::fs::remove_dir_all(kept)).await;
        }
    }
}

/// `ServerMsg::StreamOpen` on a runner: the stream opened, its chunks and events sent up on their
/// lane; whether it opened, said.
pub async fn serve_open(dir: PathBuf, id: String, session: String, json: String, up: async_channel::Sender<crate::exec::Up>) {
    use crate::exec::Up;
    use crate::proto::AgentMsg;
    let (tx, rx) = async_channel::bounded::<Out>(512);
    let fwd = up.clone();
    crate::rt::spawn(async move {
        while let Ok(o) = rx.recv().await {
            let m = match o {
                Out::Frame { stream, data } => AgentMsg::StreamFrame { stream, data },
                Out::Event { stream, json } => AgentMsg::StreamEvent { stream, json },
            };
            if fwd.send(Up::Stream(m)).await.is_err() {
                return;
            }
        }
    });
    let spec = serde_json::from_str::<Spec>(&json).map(|mut s| {
        s.session = Some(session);
        s
    });
    let said = match spec {
        Ok(spec) => match streams().open(&dir, &id, &spec, tx.clone()).await {
            Ok(v) => v,
            Err(e) => json!({"error": format!("{e:#}")}),
        },
        Err(e) => json!({"error": format!("not a stream's spec: {e}")}),
    };
    let _ = tx.send(Out::Event { stream: id, json: said.to_string() }).await;
}

/// `ServerMsg::StreamInput` on a runner: queued for its stream, in order.
pub fn serve_input(id: &str, json: &str) {
    let Some(s) = streams().get(id) else { return };
    match serde_json::from_str::<Vec<Input>>(json) {
        Ok(events) => {
            // A viewer far ahead of the page loses what the page can't take.
            let _ = s.inputs.try_send(events);
        }
        Err(e) => tracing::debug!(stream = id, error = %e, "a viewer's input isn't any kiln knows"),
    }
}

/// `ServerMsg::StreamControl` on a runner.
pub async fn serve_control(id: String, json: String) {
    let Some(s) = streams().get(&id) else { return };
    let c: Value = serde_json::from_str(&json).unwrap_or_default();
    if let Err(e) = s.control(&c).await {
        tracing::debug!(stream = %id, error = %format!("{e:#}"), "a viewer's request didn't reach the stream");
    }
}

/// What an agent does in its live page (`crate::browser::ask`'s `act`): its `actions` in order
/// (`{"click": [x, y]}`, `{"tap": [x, y]}`, `{"type": "text"}`, `{"key": "Enter"}`, `{"scroll":
/// [dx, dy]}`, `{"go": url}`, `{"back": true}`, `{"forward": true}`, `{"reload": true}`,
/// `{"wait_ms": n}`), then (`picture`, by default) a picture of it. Its viewers are told what it did.
#[derive(Debug, Deserialize)]
pub struct Act {
    pub stream: String,
    /// The agent session asking (its MCP server's): only its own streams are its to use.
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub actions: Vec<Value>,
    #[serde(default = "yes")]
    pub picture: bool,
}

fn yes() -> bool {
    true
}

pub async fn act(dir: &Path, a: Act) -> anyhow::Result<Value> {
    let s = streams().get(&a.stream).filter(|s| s.session == a.session).ok_or_else(|| anyhow::anyhow!("no stream {} here (it may have closed): show the page again with show_live", a.stream))?;
    anyhow::ensure!(!s.personal, "that page is open in its person's own profile (their sign-ins): it's theirs to use, not yours. Show it yourself with show_live to use it");
    let mut did = Vec::new();
    let point = |v: &Value| -> Option<(f64, f64)> {
        let p = v.as_array()?;
        Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
    };
    for act in a.actions.iter().take(50) {
        let Some((verb, arg)) = act.as_object().and_then(|o| o.iter().next()) else { continue };
        match verb.as_str() {
            "click" | "tap" => {
                let (x, y) = point(arg).ok_or_else(|| anyhow::anyhow!("{verb} takes [x, y]"))?;
                let pt = if verb == "tap" { "touch" } else { "mouse" };
                let p = |k: &str, buttons: i64| Input::Pointer { k: k.to_owned(), x, y, button: 0, buttons, pt: pt.to_owned(), id: 1, mods: 0, clicks: 1 };
                s.input(vec![p("down", 1), p("up", 0)]).await?;
                did.push(json!({verb: [x, y]}));
            }
            "type" => {
                let text = arg.as_str().unwrap_or("").to_owned();
                s.input(vec![Input::Text { text: text.clone() }]).await?;
                did.push(json!({"type": text}));
            }
            "key" => {
                let key = arg.as_str().unwrap_or("").to_owned();
                let code = if key.chars().count() == 1 { key.chars().next().map(|c| if c.is_ascii_alphabetic() { format!("Key{}", c.to_ascii_uppercase()) } else if c.is_ascii_digit() { format!("Digit{c}") } else { String::new() }).unwrap_or_default() } else { key.clone() };
                let text = if key.chars().count() == 1 { key.clone() } else if key == "Enter" { "\r".to_owned() } else { String::new() };
                let k = |k: &str| Input::Key { k: k.to_owned(), key: key.clone(), code: code.clone(), text: if k == "down" { text.clone() } else { String::new() }, mods: 0, rep: false, loc: 0 };
                s.input(vec![k("down"), k("up")]).await?;
                did.push(json!({"key": key}));
            }
            "scroll" => {
                let (dx, dy) = point(arg).ok_or_else(|| anyhow::anyhow!("scroll takes [dx, dy]"))?;
                let size = s.page.eval("[innerWidth / 2, innerHeight / 2]").await.unwrap_or_default();
                let (x, y) = point(&size).unwrap_or((100.0, 100.0));
                s.input(vec![Input::Wheel { x, y, dx, dy, mods: 0 }]).await?;
                did.push(json!({"scroll": [dx, dy]}));
            }
            "go" | "back" | "forward" | "reload" => {
                s.control(&json!({verb: arg})).await?;
                did.push(json!({verb: arg}));
                crate::rt::sleep(Duration::from_millis(400)).await;
            }
            "wait_ms" => crate::rt::sleep(Duration::from_millis(arg.as_u64().unwrap_or(0).min(10_000))).await,
            other => anyhow::bail!("use_live doesn't do {other:?}"),
        }
    }
    // Its viewers see who's at it.
    let _ = s.out.send(Out::Event { stream: s.id.clone(), json: json!({"agent": {"did": did}}).to_string() }).await;
    // A moment for the page to answer what was done.
    crate::rt::sleep(Duration::from_millis(300)).await;
    let _ = s.page.settle().await;
    let url = s.page.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    let title = s.page.eval("document.title").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    let picture = if a.picture {
        let png = s.page.picture(false).await?;
        let pictures = dir.join("browser").join("pictures");
        std::fs::create_dir_all(&pictures)?;
        let f = pictures.join(format!("live-{}-{}.png", crate::util::now_ms(), std::process::id()));
        std::fs::write(&f, png)?;
        Some(f.to_string_lossy().into_owned())
    } else {
        None
    };
    Ok(json!({"stream": s.id, "url": url, "title": title, "did": did, "picture_path": picture}))
}

/// A chunk from the extension (`<stream> <kind> <flags> <timestamp> <base64>`), as messages to the
/// server: in parts when it's bigger than one may be.
fn chunk(payload: &str) -> Option<(String, Vec<Out>)> {
    let mut it = payload.splitn(5, ' ');
    let (stream, kind, flags, ts, data) = (it.next()?, it.next()?, it.next()?, it.next()?, it.next().unwrap_or(""));
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    let ts: i64 = ts.parse().unwrap_or(0);
    let kind = *kind.as_bytes().first()?;
    let flags = *flags.as_bytes().first().unwrap_or(&b'-');
    if kind == b'e' {
        return Some((stream.to_owned(), vec![Out::Event { stream: stream.to_owned(), json: String::from_utf8_lossy(&bytes).into_owned() }]));
    }
    let parts: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(PART).collect() };
    let n = u8::try_from(parts.len()).ok()?;
    let out = parts.iter().enumerate().filter_map(|(i, p)| Some(Out::Frame { stream: stream.to_owned(), data: frame(kind, flags, ts, u8::try_from(i).ok()?, n, p) })).collect();
    Some((stream.to_owned(), out))
}

/// Tells kiln when a field that takes text has the focus (so a phone's keyboard comes up for it),
/// and when it hasn't.
const FOCUS: &str = r"(() => {
  if (window.__kilnFocus) return;
  window.__kilnFocus = true;
  const editable = (e) => e && (e.isContentEditable || (e.tagName === 'INPUT' && !['button','checkbox','color','file','hidden','image','radio','range','reset','submit'].includes(e.type)) || e.tagName === 'TEXTAREA' || e.tagName === 'SELECT');
  const say = () => { try { const e = document.activeElement; kilnFocus(JSON.stringify({ editable: !!editable(e), type: e && e.type || '', mode: e && e.inputMode || '' })); } catch (x) {} };
  addEventListener('focusin', say, true);
  addEventListener('focusout', () => setTimeout(say, 0), true);
})();";

/// What the tab does that its viewer's side shows: where it is (its address and title), a field
/// taking text or not, a dialog (dismissed), its end.
async fn watch(s: Arc<Stream>, out: async_channel::Sender<Out>) {
    let events = s.page.listen();
    while let Ok(e) = events.recv().await {
        let ev = match e.method.as_str() {
            "Page.frameNavigated" if e.params.get("frame").and_then(|f| f.get("parentId")).is_none() => {
                let url = e.params.get("frame").and_then(|f| f.get("url")).and_then(Value::as_str).unwrap_or("");
                Some(json!({"navigated": {"url": url}}))
            }
            "Page.loadEventFired" => {
                let title = s.page.eval("document.title").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
                Some(json!({"loaded": {"title": title}}))
            }
            "Runtime.bindingCalled" if e.params.get("name").and_then(Value::as_str) == Some("kilnFocus") => {
                serde_json::from_str::<Value>(e.params.get("payload").and_then(Value::as_str).unwrap_or("{}")).ok().map(|f| json!({"focus": f}))
            }
            // A page's alert, confirm or prompt would hold it still: answered, and said.
            "Page.javascriptDialogOpening" => {
                let _ = s.page.call("Page.handleJavaScriptDialog", json!({"accept": true})).await;
                Some(json!({"dialog": {"type": e.params.get("type"), "message": e.params.get("message")}}))
            }
            "Inspector.detached" | "Target.detachedFromTarget" => {
                let _ = out.send(Out::Event { stream: s.id.clone(), json: json!({"closed": {"why": "the page went away"}}).to_string() }).await;
                return;
            }
            _ => None,
        };
        if let Some(ev) = ev
            && out.send(Out::Event { stream: s.id.clone(), json: ev.to_string() }).await.is_err()
        {
            return;
        }
    }
}

/// What a viewer does, as the server sends it (`ServerMsg::StreamInput`): each event, in order.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "t")]
pub enum Input {
    /// A pointer: `k` down, move, up or cancel; at `x`, `y` (CSS pixels in the page); its
    /// `button` (0 main, 1 middle, 2 secondary) and `buttons`; `pt` mouse, touch or pen; `id` its
    /// pointer's id; `mods` (alt 1, ctrl 2, meta 4, shift 8).
    #[serde(rename = "p")]
    Pointer {
        k: String,
        x: f64,
        y: f64,
        #[serde(default)]
        button: i64,
        #[serde(default)]
        buttons: i64,
        #[serde(default)]
        pt: String,
        #[serde(default)]
        id: i64,
        #[serde(default)]
        mods: i64,
        #[serde(default)]
        clicks: i64,
    },
    /// A wheel or a trackpad's scroll.
    #[serde(rename = "w")]
    Wheel {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        #[serde(default)]
        mods: i64,
    },
    /// A key, by meaning (`key`) and place (`code`): `k` down or up, `text` what it types.
    #[serde(rename = "k")]
    Key {
        k: String,
        key: String,
        #[serde(default)]
        code: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        mods: i64,
        #[serde(default)]
        rep: bool,
        #[serde(default)]
        loc: i64,
    },
    /// Text typed as a whole (a phone's keyboard, a composition finished, a paste).
    #[serde(rename = "x")]
    Text { text: String },
    /// A composition under way (an input method's), with its selection.
    #[serde(rename = "i")]
    Compose {
        text: String,
        #[serde(default)]
        start: i64,
        #[serde(default)]
        end: i64,
    },
}

/// Windows' key codes for keys that type nothing (the protocol wants them for those).
fn key_code(key: &str, code: &str) -> i64 {
    match key {
        "Backspace" => 8,
        "Tab" => 9,
        "Enter" => 13,
        "Shift" => 16,
        "Control" => 17,
        "Alt" => 18,
        "Pause" => 19,
        "CapsLock" => 20,
        "Escape" => 27,
        " " => 32,
        "PageUp" => 33,
        "PageDown" => 34,
        "End" => 35,
        "Home" => 36,
        "ArrowLeft" => 37,
        "ArrowUp" => 38,
        "ArrowRight" => 39,
        "ArrowDown" => 40,
        "Insert" => 45,
        "Delete" => 46,
        "Meta" => 91,
        "ContextMenu" => 93,
        k if k.len() > 1 && k.starts_with('F') && k.get(1..).and_then(|n| n.parse::<i64>().ok()).is_some_and(|n| (1..=24).contains(&n)) => 111 + k.get(1..).and_then(|n| n.parse::<i64>().ok()).unwrap_or(1),
        _ => {
            // A letter or a digit by where it is (`KeyA`, `Digit1`), else what it types.
            if let Some(c) = code.strip_prefix("Key").and_then(|c| c.chars().next()) {
                i64::from(u32::from(c.to_ascii_uppercase()))
            } else if let Some(c) = code.strip_prefix("Digit").and_then(|c| c.chars().next()) {
                i64::from(u32::from(c))
            } else {
                key.chars().next().map_or(0, |c| i64::from(u32::from(c.to_ascii_uppercase())))
            }
        }
    }
}

impl Stream {
    /// What a viewer did, to the tab.
    pub async fn input(&self, events: Vec<Input>) -> anyhow::Result<()> {
        for e in events {
            match e {
                // A touch on a page laid out for a mouse (a phone's viewer before its size came)
                // is what a desktop browser makes of one: the mouse's (below).
                Input::Pointer { k, x, y, button, buttons, pt, id, mods, clicks } if pt == "touch" && self.mobile.load(std::sync::atomic::Ordering::Relaxed) => {
                    let kind = {
                        let mut t = self.touches.lock();
                        match k.as_str() {
                            "down" => {
                                t.insert(id, (x, y));
                                if t.len() == 1 { "touchStart" } else { "touchMove" }
                            }
                            "move" => {
                                t.insert(id, (x, y));
                                "touchMove"
                            }
                            "cancel" => {
                                t.clear();
                                "touchCancel"
                            }
                            _ => {
                                t.remove(&id);
                                if t.is_empty() { "touchEnd" } else { "touchMove" }
                            }
                        }
                    };
                    let points: Vec<Value> = self.touches.lock().iter().map(|(i, (x, y))| json!({"x": x, "y": y, "id": i})).collect();
                    let _ = (button, buttons, clicks);
                    self.page.call("Input.dispatchTouchEvent", json!({"type": kind, "touchPoints": points, "modifiers": mods})).await?;
                }
                Input::Pointer { k, x, y, button, buttons, mods, clicks, .. } => {
                    let kind = match k.as_str() {
                        "down" => "mousePressed",
                        "up" => "mouseReleased",
                        _ => "mouseMoved",
                    };
                    let which = match (k.as_str(), button) {
                        ("move", _) => "none",
                        (_, 1) => "middle",
                        (_, 2) => "right",
                        _ => "left",
                    };
                    self.page.call("Input.dispatchMouseEvent", json!({"type": kind, "x": x, "y": y, "button": which, "buttons": buttons, "clickCount": clicks.max(if kind == "mouseMoved" { 0 } else { 1 }), "modifiers": mods})).await?;
                }
                Input::Wheel { x, y, dx, dy, mods } => {
                    self.page.call("Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy, "modifiers": mods})).await?;
                }
                Input::Key { k, key, code, text, mods, rep, loc } => {
                    let down = k == "down";
                    // A key that types something goes as one that does (`keyDown` with its text);
                    // the rest as raw keys.
                    let kind = match (down, text.is_empty()) {
                        (true, false) => "keyDown",
                        (true, true) => "rawKeyDown",
                        (false, _) => "keyUp",
                    };
                    let vk = key_code(&key, &code);
                    let mut p = json!({"type": kind, "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": mods, "autoRepeat": rep, "location": loc});
                    if down
                        && !text.is_empty()
                        && let Some(o) = p.as_object_mut()
                    {
                        o.insert("text".into(), json!(text));
                        o.insert("unmodifiedText".into(), json!(text));
                    }
                    self.page.call("Input.dispatchKeyEvent", p).await?;
                }
                Input::Text { text } => {
                    self.page.call("Input.insertText", json!({"text": text})).await?;
                }
                Input::Compose { text, start, end } => {
                    self.page.call("Input.imeSetComposition", json!({"text": text, "selectionStart": start, "selectionEnd": end})).await?;
                }
            }
        }
        Ok(())
    }

    /// What a viewer asks of the stream (`ServerMsg::StreamControl`): `{"resize": {width, height,
    /// scale, mobile}}`, `{"pause": bool}`, `{"keyframe": true}`, `{"go": url}`, `{"reload": true}`,
    /// `{"back": true}`, `{"forward": true}`.
    pub async fn control(&self, c: &Value) -> anyhow::Result<()> {
        let off = self.streamer.offscreen.lock().await.clone().unwrap_or_default();
        let cdp = &self.streamer.browser.cdp;
        let id = json!(self.id);
        if let Some(r) = c.get("resize") {
            let w = r.get("width").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(390).clamp(200, 3840);
            let h = r.get("height").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(844).clamp(200, 3840);
            let scale = r.get("scale").and_then(Value::as_f64).unwrap_or(2.0).clamp(1.0, 3.0);
            let mobile = r.get("mobile").and_then(Value::as_bool).unwrap_or(false);
            self.page.viewport(w, h, scale, mobile).await?;
            self.mobile.store(mobile, std::sync::atomic::Ordering::Relaxed);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (pw, ph) = ((f64::from(w) * scale).round() as u32, (f64::from(h) * scale).round() as u32);
            super::evaluate(cdp, &off, &format!("kiln.resize({id}, {pw}, {ph})")).await?;
        }
        if let Some(p) = c.get("pause").and_then(Value::as_bool) {
            super::evaluate(cdp, &off, &format!("kiln.pause({id}, {p})")).await?;
        }
        if c.get("keyframe").and_then(Value::as_bool) == Some(true) {
            super::evaluate(cdp, &off, &format!("kiln.keyframe({id})")).await?;
        }
        if let Some(url) = c.get("go").and_then(Value::as_str) {
            self.page.call("Page.navigate", json!({"url": url})).await?;
        }
        if c.get("reload").and_then(Value::as_bool) == Some(true) {
            self.page.call("Page.reload", json!({})).await?;
        }
        for (k, delta) in [("back", -1), ("forward", 1)] {
            if c.get(k).and_then(Value::as_bool) == Some(true) {
                let h = self.page.call("Page.getNavigationHistory", json!({})).await?;
                let at = h.get("currentIndex").and_then(Value::as_i64).unwrap_or(0) + delta;
                if let Some(entry) = h.get("entries").and_then(Value::as_array).and_then(|e| usize::try_from(at).ok().and_then(|i| e.get(i))) {
                    self.page.call("Page.navigateToHistoryEntry", json!({"entryId": entry.get("id")})).await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_is_kilns_own() -> anyhow::Result<()> {
        // Chrome's id for an extension: the first half of its public key's SHA-256, as a to p.
        let m: Value = serde_json::from_str(MANIFEST)?;
        let key = base64::engine::general_purpose::STANDARD.decode(m.get("key").and_then(Value::as_str).unwrap_or(""))?;
        let id: String = hex::encode(Sha256::digest(&key)).chars().take(32).map(|c| char::from(b'a' + u8::try_from(c.to_digit(16).unwrap_or(0)).unwrap_or(0))).collect();
        assert_eq!(id, EXTENSION_ID);
        Ok(())
    }

    #[test]
    fn a_chunk_bigger_than_a_message_goes_in_parts() -> anyhow::Result<()> {
        let big = base64::engine::general_purpose::STANDARD.encode(vec![7u8; PART * 2 + 10]);
        let (stream, out) = chunk(&format!("s1 v k 1234 {big}")).unwrap_or_default();
        assert_eq!(stream, "s1");
        assert_eq!(out.len(), 3);
        let last = match out.get(2) {
            Some(Out::Frame { data, .. }) => data.clone(),
            other => anyhow::bail!("not a frame: {other:?}"),
        };
        assert_eq!((last.first(), last.get(1), last.get(10), last.get(11)), (Some(&b'v'), Some(&b'k'), Some(&2), Some(&3)));
        assert_eq!(i64::from_be_bytes(last.get(2..10).and_then(|b| b.try_into().ok()).unwrap_or_default()), 1234);
        Ok(())
    }

    /// The real thing: the pinned browser with kiln's extension, a page captured and encoded,
    /// its chunks and events as the server gets them, a click reaching it.
    #[test]
    #[ignore = "downloads and starts the real browser (about 200 MB)"]
    fn the_real_browser_streams_a_page() -> anyhow::Result<()> {
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        crate::rt::block_on(async move {
            let page = "data:text/html,<body style='margin:0;background:%23123'><div id=b style='width:40px;height:40px;background:%23bab0e3;position:absolute'></div><script>let t=0;addEventListener('pointerdown',()=>document.title='clicked');(function f(){t++;b.style.left=(t*3%25200)+'px';requestAnimationFrame(f)})()</script></body>";
            let spec: Spec = serde_json::from_value(json!({"url": page, "width": 320, "height": 240, "scale": 1, "profile": "clean"}))?;
            let (tx, rx) = async_channel::bounded::<Out>(4096);
            let opened = streams().open(&dir, "test-stream", &spec, tx).await?;
            assert!(opened.get("opened").is_some(), "{opened}");
            let s = streams().get("test-stream").ok_or_else(|| anyhow::anyhow!("not open"))?;
            s.control(&json!({"pause": false, "keyframe": true})).await?;
            let (mut configs, mut keys, mut frames) = (0, 0, 0);
            let end = std::time::Instant::now() + Duration::from_secs(4);
            while std::time::Instant::now() < end {
                if let Ok(Ok(Out::Frame { data, .. })) = crate::rt::timeout(Duration::from_millis(200), rx.recv()).await {
                    match (data.first(), data.get(1)) {
                        (Some(b'c'), _) => configs += 1,
                        (Some(b'v'), Some(b'k')) => keys += 1,
                        (Some(b'v'), _) => frames += 1,
                        _ => {}
                    }
                }
            }
            s.input(vec![Input::Pointer { k: "down".into(), x: 100.0, y: 100.0, button: 0, buttons: 1, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }, Input::Pointer { k: "up".into(), x: 100.0, y: 100.0, button: 0, buttons: 0, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }]).await?;
            crate::rt::sleep(Duration::from_millis(300)).await;
            let title = s.page.eval("document.title").await?;
            streams().close("test-stream").await;
            assert!(configs >= 1 && keys >= 1 && frames > 30, "configs {configs}, key frames {keys}, frames {frames}");
            assert_eq!(title, json!("clicked"));
            Ok(())
        })
    }

    #[test]
    fn keys_that_type_nothing_have_their_codes() {
        assert_eq!(key_code("Enter", "Enter"), 13);
        assert_eq!(key_code("a", "KeyA"), 65);
        assert_eq!(key_code("!", "Digit1"), 49);
        assert_eq!(key_code("F5", "F5"), 116);
    }
}
