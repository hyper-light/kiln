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
    /// The engine it runs in: `chromium` (the default), `firefox` or `webkit` (`super::driven`).
    #[serde(default)]
    pub engine: Option<String>,
    /// A device it's laid out as (`super::devices`): its window is then the device's, whatever
    /// its viewers' are.
    #[serde(default)]
    pub device: Option<String>,
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
/// bytes; see [`frame`]) or an event (JSON: opened, navigated, loaded, focus, fields, selected,
/// copied, a dialog, closed, an error).
#[derive(Debug, Clone)]
pub enum Out {
    Frame { stream: String, data: Vec<u8> },
    Event { stream: String, json: String },
}

/// Where a stream's output goes: its events (waited for, so none is lost) and its chunks (one that
/// finds no room is lost, and the next key frame mends it), each its own way, so what a stream says
/// never waits behind its pictures (`serve_open` sends events first).
#[derive(Clone)]
pub struct Outs {
    pub events: async_channel::Sender<Out>,
    pub chunks: async_channel::Sender<Out>,
}

impl Outs {
    /// Both one way (a test reads them as they come).
    pub fn one(tx: async_channel::Sender<Out>) -> Self {
        Self { events: tx.clone(), chunks: tx }
    }
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
    /// Since when it's had no stream (unix ms): started ahead of one, or its last one closed.
    idle_since: std::sync::atomic::AtomicI64,
}

/// One stream: its tab, and what its viewers do, in order.
pub struct Stream {
    pub id: String,
    /// The agent session it streams for.
    pub session: String,
    /// The browser that encodes it: its page's own (a Chromium tab's), or one that encodes the
    /// pictures of another engine's page.
    streamer: Arc<Streamer>,
    page: Surface,
    /// The device it's laid out as, if it's held to one.
    device: Option<super::devices::Device>,
    /// When its viewers last did something (unix ms): another engine's page is looked at more
    /// often just after.
    touched: std::sync::atomic::AtomicI64,
    touches: Mutex<HashMap<i64, (f64, f64)>>,
    inputs: async_channel::Sender<Vec<Input>>,
    /// What its viewers ask of it, done in order (`controlled`).
    controls: async_channel::Sender<Value>,
    /// Laid out as a phone's (touch, a phone's viewport): what its viewer is.
    mobile: std::sync::atomic::AtomicBool,
    /// In a person's own profile (their sign-ins): theirs to use, not an agent's.
    personal: bool,
    /// Where what it says goes (its viewers, through the server).
    out: async_channel::Sender<Out>,
    /// kiln's state here (its recordings go under it).
    dir: PathBuf,
    /// Its window, as last laid out (what a recording starts with).
    viewport: Mutex<Value>,
    /// A recording under way (`super::record`), and whether its viewers had let it pause (it
    /// doesn't while it records: what an agent does with nobody looking is recorded too).
    recording: Mutex<Option<Arc<super::record::Recording>>>,
    paused: std::sync::atomic::AtomicBool,
}

/// What a stream's page is: a Chromium tab (captured as its compositor draws it) and the others
/// it opened, or a page of another engine's (`super::driven`: its picture taken as it changes).
pub(crate) enum Surface {
    Tab(Tabs),
    Driven(super::driven::Driven),
}

/// A Chromium stream's tabs, as a browser's: the one shown (captured, and what its viewers do goes
/// to it), and the rest, each opened by its page (a link to a new tab, a sign-in's popup) or by its
/// viewers.
pub(crate) struct Tabs {
    list: Mutex<Vec<Tab>>,
    /// The shown tab's target.
    shown: Mutex<String>,
    /// One change of its capture at a time (a tab shown, a new size): two at once would capture a
    /// tab twice, one of them never let go.
    showing: futures::lock::Mutex<()>,
    /// Each list said is numbered, as it was then (one said later may arrive first).
    said: std::sync::atomic::AtomicU64,
}

#[derive(Clone)]
struct Tab {
    page: Arc<Page>,
    /// Chrome's own id for it, what the capture extension captures it by.
    chrome: i64,
    /// The stream's tab that opened it, if one did.
    opener: Option<String>,
    url: String,
    title: String,
}

/// The most tabs a stream has open (a page opening tabs in a loop stops being heard at that).
const TABS: usize = 30;

impl Tabs {
    fn new(page: Page, chrome: i64) -> Self {
        let shown = page.target().to_owned();
        Self { list: Mutex::new(vec![Tab { page: Arc::new(page), chrome, opener: None, url: String::new(), title: String::new() }]), shown: Mutex::new(shown), showing: futures::lock::Mutex::new(()), said: std::sync::atomic::AtomicU64::new(0) }
    }

    /// The tab shown.
    fn page(&self) -> anyhow::Result<Arc<Page>> {
        let shown = self.shown.lock().clone();
        self.list.lock().iter().find(|t| t.page.target() == shown).map(|t| t.page.clone()).ok_or_else(|| anyhow::anyhow!("its tabs are all closed"))
    }

    fn get(&self, target: &str) -> Option<Tab> {
        self.list.lock().iter().find(|t| t.page.target() == target).cloned()
    }

    fn has(&self, target: &str) -> bool {
        self.list.lock().iter().any(|t| t.page.target() == target)
    }

    fn pages(&self) -> Vec<Arc<Page>> {
        self.list.lock().iter().map(|t| t.page.clone()).collect()
    }

    /// As its viewers are told them: `{"list": [{id, url, title}], "shown": id, "seq": n}`, `seq`
    /// numbering it as it was then (the server and its viewers keep the newest).
    fn said(&self) -> Value {
        let l = self.list.lock();
        let seq = self.said.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let list: Vec<Value> = l.iter().map(|t| json!({"id": t.page.target(), "url": t.url, "title": t.title})).collect();
        json!({"tabs": {"list": list, "shown": *self.shown.lock(), "seq": seq}})
    }

    /// A tab's address or title, as it changed: whether it did.
    fn changed(&self, target: &str, url: Option<&str>, title: Option<&str>) -> bool {
        let mut l = self.list.lock();
        let Some(t) = l.iter_mut().find(|t| t.page.target() == target) else { return false };
        let mut changed = false;
        if let Some(u) = url.filter(|u| *u != t.url) {
            u.clone_into(&mut t.url);
            changed = true;
        }
        if let Some(n) = title.filter(|n| *n != t.title) {
            n.clone_into(&mut t.title);
            changed = true;
        }
        changed
    }
}

impl Surface {
    async fn eval(&self, js: &str) -> anyhow::Result<Value> {
        match self {
            Self::Tab(t) => t.page()?.eval(js).await,
            Self::Driven(d) => d.eval(js).await,
        }
    }

    async fn navigate(&self, url: &str) -> anyhow::Result<()> {
        match self {
            Self::Tab(t) => t.page()?.call("Page.navigate", json!({"url": url})).await.map(|_| ()),
            Self::Driven(d) => d.navigate(url).await,
        }
    }

    async fn reload(&self) -> anyhow::Result<()> {
        match self {
            Self::Tab(t) => t.page()?.call("Page.reload", json!({})).await.map(|_| ()),
            Self::Driven(d) => d.reload().await,
        }
    }

    /// Back (`-1`) or forward (`1`).
    async fn history(&self, delta: i64) -> anyhow::Result<()> {
        match self {
            Self::Tab(t) => {
                let p = t.page()?;
                let h = p.call("Page.getNavigationHistory", json!({})).await?;
                let at = h.get("currentIndex").and_then(Value::as_i64).unwrap_or(0) + delta;
                if let Some(entry) = h.get("entries").and_then(Value::as_array).and_then(|e| usize::try_from(at).ok().and_then(|i| e.get(i))) {
                    p.call("Page.navigateToHistoryEntry", json!({"entryId": entry.get("id")})).await?;
                }
                Ok(())
            }
            Self::Driven(d) => d.history(delta).await,
        }
    }

    /// Its window; every tab's (each is laid out for it before it's shown).
    async fn viewport(&self, width: u32, height: u32, scale: f64, mobile: bool) -> anyhow::Result<()> {
        match self {
            Self::Tab(t) => {
                let shown = t.page()?;
                shown.viewport(width, height, scale, mobile).await?;
                for p in t.pages().into_iter().filter(|p| !Arc::ptr_eq(p, &shown)) {
                    let _ = p.viewport(width, height, scale, mobile).await;
                }
                Ok(())
            }
            Self::Driven(d) => d.viewport(width, height, scale).await,
        }
    }

    async fn settle(&self) {
        match self {
            Self::Tab(t) => {
                if let Ok(p) = t.page() {
                    let _ = p.settle().await;
                }
            }
            Self::Driven(_) => crate::rt::sleep(Duration::from_millis(150)).await,
        }
    }

    /// A picture of it (PNG).
    async fn picture(&self) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::Tab(t) => t.page()?.picture(false).await,
            Self::Driven(d) => Ok(d.shot(true).await?.0),
        }
    }
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
        // Its pages said as they come and go: a tab a stream's page opens is the stream's.
        crate::rt::spawn(tabs_said(cdp.listen_browser()));
        cdp.call(None, "Target.setDiscoverTargets", json!({"discover": true})).await?;
        let s = Arc::new(Streamer { browser, alone: profile.starts_with("clean:"), worker, offscreen: futures::lock::Mutex::new(None), outs: Arc::default(), idle_since: std::sync::atomic::AtomicI64::new(crate::util::now_ms()) });
        g.insert(profile.to_owned(), s.clone());
        Ok(s)
    }

    /// A conversation's browser, started ahead of its first stream (a person is choosing what to
    /// open: `super::apps`'s look), so what they open opens at once. One nothing opens in stops
    /// a while later, as one whose streams closed does.
    pub fn warm(&'static self, dir: &Path, session: &str) {
        let (dir, profile) = (dir.to_path_buf(), format!("conversation:{session}"));
        crate::rt::spawn(async move {
            let running = self.streamers.lock().await.get(&profile).is_some_and(|s| s.browser.alive());
            if running {
                return;
            }
            match self.streamer(&dir, &profile).await {
                Ok(s) => {
                    crate::rt::sleep(IDLE_BROWSER).await;
                    self.stop_if_idle(&s).await;
                }
                Err(e) => tracing::debug!(error = %format!("{e:#}"), "no browser started ahead of a stream"),
            }
        });
    }

    /// Open a stream of `spec` as `id`; its chunks and events go to `outs`.
    pub async fn open(&self, dir: &Path, id: &str, spec: &Spec, outs: Outs) -> anyhow::Result<Value> {
        let out = outs.events.clone();
        let engine = super::driven::Engine::parse(spec.engine.as_deref().unwrap_or(""))?;
        let device = match spec.device.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            Some(n) => Some(super::devices::device(n).ok_or_else(|| anyhow::anyhow!("kiln knows no device {n:?}: {}", super::devices::DEVICES.iter().map(|d| d.name).collect::<Vec<_>>().join(", ")))?),
            None => None,
        };
        // Its window: a device's, else its viewer's.
        let (width, height, scale, mobile) = device.map_or((spec.width, spec.height, spec.scale.clamp(1.0, 3.0), spec.mobile), |d| (d.width, d.height, d.scale, d.mobile));
        if engine != super::driven::Engine::Chromium {
            return self.open_driven(dir, id, spec, engine, device, (width, height, scale, mobile), outs).await;
        }
        // A clean profile is new each time: a browser of the stream's own.
        let profile = if spec.profile == "clean" { format!("clean:{id}") } else { spec.profile.clone() };
        let s = self.streamer(dir, &profile).await?;
        let mark = hex::encode(rand::random::<[u8; 8]>());
        let page = s.browser.tab(&format!("about:blank#kiln-{mark}")).await?;
        page.viewport(width, height, scale, mobile).await?;
        if let Some(agent) = device.map(|d| d.agent).filter(|a| !a.is_empty()) {
            page.call("Emulation.setUserAgentOverride", json!({"userAgent": agent})).await?;
        }
        page.call("Page.enable", json!({})).await?;
        // The tab, as the extension knows it.
        let mut tab = Value::Null;
        // A moment for the extension to see it (longer on a busy machine, and for a browser just
        // started, whose extension's worker may not have run its script yet).
        for _ in 0..100 {
            if let Ok(t) = super::evaluate(&s.browser.cdp, &s.worker, &format!("kiln.tab({})", json!(mark))).await
                && t.is_number()
            {
                tab = t;
                break;
            }
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
        let chrome = tab.as_i64().ok_or_else(|| anyhow::anyhow!("the capture extension doesn't see the stream's tab"))?;
        s.outs.lock().insert(id.to_owned(), outs.chunks.clone());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (pw, ph) = ((f64::from(width) * scale).round() as u32, (f64::from(height) * scale).round() as u32);
        // Its bitrate as it asked, else as its size and rate want (`bitrateFor`).
        let opts = json!({"width": pw, "height": ph, "framerate": 60, "bitrate": spec.bitrate.unwrap_or(4_000_000), "asked": spec.bitrate.is_some(), "codec": spec.codec.clone().unwrap_or_else(|| "h264".to_owned())});
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
        let (controls, ordered) = async_channel::bounded::<Value>(64);
        let first = page.target().to_owned();
        let stream = Arc::new(Stream::new(id, spec, s.clone(), Surface::Tab(Tabs::new(page, chrome)), device, (inputs, controls), out.clone(), dir, (width, height, scale, mobile)));
        crate::rt::spawn(watch(stream.clone(), first));
        crate::rt::spawn(apply(Arc::downgrade(&stream), queued));
        crate::rt::spawn(controlled(Arc::downgrade(&stream), ordered));
        let opened = json!({"opened": {"width": width, "height": height, "scale": scale, "audio": started.get("audio"), "platform": std::env::consts::OS, "engine": "chromium", "device": device}});
        self.went(id, stream, &spec.url, opened, out).await
    }

    /// A stream set up, open before its page loads: what its viewers ask of it meanwhile reaches
    /// it, and they're told it opened (its window, a device's) before its first picture. Its page
    /// then goes to its address; one that can't closes.
    async fn went(&self, id: &str, stream: Arc<Stream>, url: &str, opened: Value, out: async_channel::Sender<Out>) -> anyhow::Result<Value> {
        self.open.lock().insert(id.to_owned(), stream.clone());
        let _ = out.send(Out::Event { stream: id.to_owned(), json: opened.to_string() }).await;
        if let Err(e) = stream.page.navigate(url).await {
            drop(stream);
            self.close(id).await;
            return Err(e);
        }
        Ok(opened)
    }

    /// A stream of another engine's page (`super::driven`): its pictures, taken as it changes,
    /// encoded by a Chromium of kiln's own (`kiln.pushed`).
    #[allow(clippy::too_many_arguments)]
    async fn open_driven(&self, dir: &Path, id: &str, spec: &Spec, engine: super::driven::Engine, device: Option<super::devices::Device>, window: (u32, u32, f64, bool), outs: Outs) -> anyhow::Result<Value> {
        let out = outs.events.clone();
        let (width, height, scale, mobile) = window;
        let enc = self.streamer(dir, "encode").await?;
        let page = super::driven::Driven::open(dir, engine, &spec.profile, width, height, scale, device.as_ref()).await?;
        if let Some(agent) = device.map(|d| d.agent).filter(|a| !a.is_empty()) {
            page.agent(agent).await;
        }
        enc.outs.lock().insert(id.to_owned(), outs.chunks.clone());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (pw, ph) = ((f64::from(width) * scale).round() as u32, (f64::from(height) * scale).round() as u32);
        let opts = json!({"width": pw, "height": ph, "framerate": 30, "bitrate": spec.bitrate.unwrap_or(4_000_000), "asked": spec.bitrate.is_some(), "codec": spec.codec.clone().unwrap_or_else(|| "h264".to_owned())});
        let started = match super::evaluate(&enc.browser.cdp, &enc.worker, &format!("kiln.pushed({}, {opts})", json!(id))).await {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => v,
            other => {
                enc.outs.lock().remove(id);
                page.close().await;
                anyhow::bail!("its pictures couldn't be encoded: {}", other.map(|v| v.to_string()).unwrap_or_else(|e| format!("{e:#}")));
            }
        };
        drop(started);
        self.listen(&enc).await?;
        let (inputs, queued) = async_channel::bounded::<Vec<Input>>(256);
        let (controls, ordered) = async_channel::bounded::<Value>(64);
        let stream = Arc::new(Stream::new(id, spec, enc, Surface::Driven(page), device, (inputs, controls), out.clone(), dir, window));
        crate::rt::spawn(said(stream.clone(), out.clone()));
        crate::rt::spawn(pump(Arc::downgrade(&stream)));
        crate::rt::spawn(apply(Arc::downgrade(&stream), queued));
        crate::rt::spawn(controlled(Arc::downgrade(&stream), ordered));
        let opened = json!({"opened": {"width": width, "height": height, "scale": scale, "mobile": mobile, "audio": false, "platform": std::env::consts::OS, "engine": engine.name(), "device": device}});
        self.went(id, stream, &spec.url, opened, out).await
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
                let Some((stream, kind, flags, ts, bytes)) = parse(payload) else { continue };
                // A recording of it takes what it sends as it's sent (`super::record`).
                let recording = streams().open.lock().get(&stream).and_then(|s| s.recording.lock().clone());
                if let Some(r) = recording {
                    r.chunk(kind, flags, ts, &bytes);
                }
                let Some(msgs) = chunk(&stream, kind, flags, ts, &bytes) else { continue };
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

    /// The stream whose tab `target` is.
    fn owning(&self, target: &str) -> Option<Arc<Stream>> {
        self.open.lock().values().find(|s| matches!(&s.page, Surface::Tab(t) if t.has(target))).cloned()
    }

    /// Close a stream: its capture stops, its tab goes. A browser left with no streams stops a
    /// while later (unless one opens meanwhile); a clean profile's stops now, and its profile goes.
    pub async fn close(&self, id: &str) {
        let Some(s) = self.open.lock().remove(id) else { return };
        // A recording of it is finished first (and its viewers told where it is).
        if s.recording.lock().is_some()
            && let Err(e) = s.record_stop().await
        {
            tracing::warn!(stream = %id, error = %format!("{e:#}"), "a recording didn't finish as its stream closed");
        }
        let cdp = s.streamer.browser.cdp.clone();
        let off = s.streamer.offscreen.lock().await.clone().unwrap_or_default();
        let _ = super::evaluate(&cdp, &off, &format!("kiln.stop({})", json!(id))).await;
        s.streamer.outs.lock().remove(id);
        // Its tabs, whoever else still holds the stream (their watchers end as they do).
        match &s.page {
            Surface::Tab(t) => {
                for p in t.pages() {
                    let _ = cdp.call(None, "Target.closeTarget", json!({"targetId": p.target()})).await;
                }
            }
            Surface::Driven(d) => d.close().await,
        }
        let streamer = s.streamer.clone();
        drop(s);
        streamer.idle_since.store(crate::util::now_ms(), std::sync::atomic::Ordering::Relaxed);
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

    /// Stop a profile's browser that has had no streams open for [`IDLE_BROWSER`].
    async fn stop_if_idle(&self, streamer: &Arc<Streamer>) {
        let idle = crate::util::now_ms() - streamer.idle_since.load(std::sync::atomic::Ordering::Relaxed);
        if streamer.outs.lock().is_empty() && idle >= i64::try_from(IDLE_BROWSER.as_millis()).unwrap_or(i64::MAX) - 1000 {
            let mut g = self.streamers.lock().await;
            g.retain(|_, s| !Arc::ptr_eq(s, streamer));
            drop(g);
            streamer.browser.stop();
            tracing::info!("a streaming browser stopped: no streams for a while");
        }
    }

    /// A session's streams on this machine close (it closed), and its conversation's profile and
    /// recordings go with them: its browser stops, its cookies, storage and recordings are removed.
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
        // Its recordings were kept with it.
        let key: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect();
        let recordings = dir.join("browser").join("recordings").join(key);
        if !session.is_empty() && recordings.exists() {
            let _ = crate::rt::blocking(move || std::fs::remove_dir_all(recordings)).await;
        }
    }
}

/// `ServerMsg::StreamOpen` on a runner: the stream opened, its chunks and events sent up on their
/// lane; whether it opened, said.
pub async fn serve_open(dir: PathBuf, id: String, session: String, json: String, up: async_channel::Sender<crate::exec::Up>) {
    use crate::exec::Up;
    use futures::FutureExt as _;
    let (tx, rx) = async_channel::bounded::<Out>(512);
    let (chunks, chunked) = async_channel::bounded::<Out>(512);
    let fwd = up.clone();
    crate::rt::spawn(async move {
        loop {
            // What the stream says first, then its pictures: none of it waits behind them.
            let o = futures::select_biased! {
                o = rx.recv().fuse() => o,
                o = chunked.recv().fuse() => o,
            };
            let Ok(o) = o else {
                // One way ended: the other's rest, then done.
                let rest = if rx.is_closed() { &chunked } else { &rx };
                while let Ok(o) = rest.recv().await {
                    if fwd.send(Up::Stream(agent_msg(o))).await.is_err() {
                        return;
                    }
                }
                return;
            };
            if fwd.send(Up::Stream(agent_msg(o))).await.is_err() {
                return;
            }
        }
    });
    let spec = serde_json::from_str::<Spec>(&json).map(|mut s| {
        s.session = Some(session);
        s
    });
    let said = match spec {
        Ok(spec) => match streams().open(&dir, &id, &spec, Outs { events: tx.clone(), chunks }).await {
            Ok(v) => v,
            Err(e) => json!({"error": format!("{e:#}")}),
        },
        Err(e) => json!({"error": format!("not a stream's spec: {e}")}),
    };
    let _ = tx.send(Out::Event { stream: id, json: said.to_string() }).await;
}

/// What a stream sent, as its runner sends it to the server.
fn agent_msg(o: Out) -> crate::proto::AgentMsg {
    match o {
        Out::Frame { stream, data } => crate::proto::AgentMsg::StreamFrame { stream, data },
        Out::Event { stream, json } => crate::proto::AgentMsg::StreamEvent { stream, json },
    }
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

/// `ServerMsg::StreamControl` on a runner: queued for its stream, done in order (`controlled`).
pub fn serve_control(id: &str, json: &str) {
    let Some(s) = streams().get(id) else { return };
    match serde_json::from_str::<Value>(json) {
        Ok(c) => {
            let _ = s.controls.try_send(c);
        }
        Err(e) => tracing::debug!(stream = id, error = %e, "a viewer's request isn't any kiln knows"),
    }
}

/// What its viewers ask of a stream, done in the order they asked, one at a time: a new size and a
/// tab shown at once would each capture a tab, and an older size could be laid out last. Of sizes
/// asked one after another while it was busy, the last.
async fn controlled(s: std::sync::Weak<Stream>, ordered: async_channel::Receiver<Value>) {
    let only_size = |c: &Value| c.as_object().is_some_and(|o| o.len() == 1 && o.contains_key("resize"));
    while let Ok(mut c) = ordered.recv().await {
        while let Ok(next) = ordered.try_recv() {
            if !(only_size(&c) && only_size(&next)) {
                let Some(st) = s.upgrade() else { return };
                if let Err(e) = st.control(&c).await {
                    tracing::debug!(stream = %st.id, error = %format!("{e:#}"), "a viewer's request didn't reach the stream");
                }
            }
            c = next;
        }
        let Some(st) = s.upgrade() else { return };
        if let Err(e) = st.control(&c).await {
            tracing::debug!(stream = %st.id, error = %format!("{e:#}"), "a viewer's request didn't reach the stream");
        }
    }
}

/// What an agent does in its live page (`crate::browser::ask`'s `act`): its `actions` in order
/// (`{"click": [x, y]}`, `{"tap": [x, y]}`, `{"type": "text"}`, `{"key": "Enter"}`, `{"scroll":
/// [dx, dy]}`, `{"go": url}`, `{"back": true}`, `{"forward": true}`, `{"reload": true}`, `{"tab":
/// {"show": id}}`, `{"tab": {"new": url}}`, `{"tab": {"close": id}}`, `{"wait_ms": n}`), then
/// (`picture`, by default) a picture of it, and its tabs. Its viewers are told what it did.
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
    let mut recorded = None;
    for act in a.actions.iter().take(50) {
        let Some((verb, arg)) = act.as_object().and_then(|o| o.iter().next()) else { continue };
        match verb.as_str() {
            // Its recording: begun, or ended (its files in the answer).
            "record" if arg.as_bool() == Some(false) => {
                recorded = Some(s.record_stop().await?);
                did.push(json!({"record": false}));
            }
            "record" => {
                s.record_start().await?;
                did.push(json!({"record": true}));
            }
            "replay" => {
                let path = arg.as_str().ok_or_else(|| anyhow::anyhow!("replay takes a recording's .inputs.jsonl path"))?;
                let n = s.replay(path).await?;
                did.push(json!({"replay": path, "steps": n}));
            }
            _ => {
                if let Some(d) = act_one(&s, verb, arg).await? {
                    s.log(json!({"by": "agent", "act": d}));
                    did.push(d);
                }
            }
        }
    }
    // Its viewers see who's at it.
    let _ = s.out.send(Out::Event { stream: s.id.clone(), json: json!({"agent": {"did": did}}).to_string() }).await;
    // A moment for the page to answer what was done.
    crate::rt::sleep(Duration::from_millis(300)).await;
    s.page.settle().await;
    let url = s.page.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    let title = s.page.eval("document.title").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    let picture = if a.picture {
        let png = s.page.picture().await?;
        let pictures = dir.join("browser").join("pictures");
        std::fs::create_dir_all(&pictures)?;
        let f = pictures.join(format!("live-{}-{}.png", crate::util::now_ms(), std::process::id()));
        std::fs::write(&f, png)?;
        Some(f.to_string_lossy().into_owned())
    } else {
        None
    };
    let mut answer = json!({"stream": s.id, "url": url, "title": title, "did": did, "picture_path": picture});
    if let (Some(o), Some(r)) = (answer.as_object_mut(), recorded) {
        o.insert("recorded".into(), r);
    }
    if let (Some(o), Surface::Tab(t)) = (answer.as_object_mut(), &s.page) {
        o.insert("tabs".into(), t.said().get("tabs").cloned().unwrap_or_default());
    }
    Ok(answer)
}

/// One of the agent's actions in stream `s`: what it did, to say (none for a wait).
async fn act_one(s: &Stream, verb: &str, arg: &Value) -> anyhow::Result<Option<Value>> {
    let point = |v: &Value| -> Option<(f64, f64)> {
        let p = v.as_array()?;
        Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
    };
    Ok(match verb {
        "click" | "tap" => {
            let (x, y) = point(arg).ok_or_else(|| anyhow::anyhow!("{verb} takes [x, y]"))?;
            let pt = if verb == "tap" { "touch" } else { "mouse" };
            let p = |k: &str, buttons: i64| Input::Pointer { k: k.to_owned(), x, y, button: 0, buttons, pt: pt.to_owned(), id: 1, mods: 0, clicks: 1 };
            s.input(vec![p("down", 1), p("up", 0)]).await?;
            Some(json!({verb: [x, y]}))
        }
        "type" => {
            let text = arg.as_str().unwrap_or("").to_owned();
            s.input(vec![Input::Text { text: text.clone() }]).await?;
            Some(json!({"type": text}))
        }
        "key" => {
            let key = arg.as_str().unwrap_or("").to_owned();
            let code = if key.chars().count() == 1 { key.chars().next().map(|c| if c.is_ascii_alphabetic() { format!("Key{}", c.to_ascii_uppercase()) } else if c.is_ascii_digit() { format!("Digit{c}") } else { String::new() }).unwrap_or_default() } else { key.clone() };
            let text = if key.chars().count() == 1 { key.clone() } else if key == "Enter" { "\r".to_owned() } else { String::new() };
            let k = |k: &str| Input::Key { k: k.to_owned(), key: key.clone(), code: code.clone(), text: if k == "down" { text.clone() } else { String::new() }, mods: 0, rep: false, loc: 0 };
            s.input(vec![k("down"), k("up")]).await?;
            Some(json!({"key": key}))
        }
        "scroll" => {
            let (dx, dy) = point(arg).ok_or_else(|| anyhow::anyhow!("scroll takes [dx, dy]"))?;
            let size = s.page.eval("[innerWidth / 2, innerHeight / 2]").await.unwrap_or_default();
            let (x, y) = point(&size).unwrap_or((100.0, 100.0));
            s.input(vec![Input::Wheel { x, y, dx, dy, mods: 0 }]).await?;
            Some(json!({"scroll": [dx, dy]}))
        }
        "go" | "back" | "forward" | "reload" | "tab" => {
            s.control(&json!({verb: arg})).await?;
            crate::rt::sleep(Duration::from_millis(400)).await;
            Some(json!({verb: arg}))
        }
        "wait_ms" => {
            crate::rt::sleep(Duration::from_millis(arg.as_u64().unwrap_or(0).min(10_000))).await;
            None
        }
        other => anyhow::bail!("use_live doesn't do {other:?}"),
    })
}

/// A chunk from the extension (`<stream> <kind> <flags> <timestamp> <base64>`): its stream, kind,
/// flags, time and bytes.
fn parse(payload: &str) -> Option<(String, u8, u8, i64, Vec<u8>)> {
    let mut it = payload.splitn(5, ' ');
    let (stream, kind, flags, ts, data) = (it.next()?, it.next()?, it.next()?, it.next()?, it.next().unwrap_or(""));
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    Some((stream.to_owned(), *kind.as_bytes().first()?, *flags.as_bytes().first().unwrap_or(&b'-'), ts.parse().unwrap_or(0), bytes))
}

/// A chunk as messages to the server: in parts when it's bigger than one may be.
fn chunk(stream: &str, kind: u8, flags: u8, ts: i64, bytes: &[u8]) -> Option<Vec<Out>> {
    if kind == b'e' {
        return Some(vec![Out::Event { stream: stream.to_owned(), json: String::from_utf8_lossy(bytes).into_owned() }]);
    }
    let parts: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(PART).collect() };
    let n = u8::try_from(parts.len()).ok()?;
    Some(parts.iter().enumerate().filter_map(|(i, p)| Some(Out::Frame { stream: stream.to_owned(), data: frame(kind, flags, ts, u8::try_from(i).ok()?, n, p) })).collect())
}

/// Tells kiln what the page's focused field is when one takes text (so the viewer's own field is
/// like it: its keyboard comes up for it, its password manager fills it), where the page's fields
/// are (so a tap on one brings the keyboard up at once, in the tap's own gesture), and whether
/// text is selected (to copy). Each said only when it changed.
/// The same, for an engine of another kind's (`super::driven`): its binding is theirs to make.
pub(crate) const FOCUS_SOURCE: &str = FOCUS;

const FOCUS: &str = r"(() => {
  if (window.__kilnFocus) return;
  window.__kilnFocus = {};
  const editable = (e) => e && (e.isContentEditable || (e.tagName === 'INPUT' && !['button','checkbox','color','file','hidden','image','radio','range','reset','submit'].includes(e.type)) || e.tagName === 'TEXTAREA');
  const like = (e) => ({ type: e.isContentEditable ? 'text' : (e.type || 'text'), mode: e.inputMode || '', ac: (e.getAttribute('autocomplete') || '').trim(), hint: e.enterKeyHint || '',
    label: ((e.labels && e.labels[0] && e.labels[0].textContent) || e.placeholder || e.getAttribute('aria-label') || '').trim().slice(0, 60), multi: e.tagName === 'TEXTAREA' || e.isContentEditable });
  const said = {};
  const tell = (k, v) => { const s = JSON.stringify({ [k]: v }); if (said[k] !== s) { said[k] = s; try { kilnFocus(s); } catch (x) {} } };
  const focus = () => { const e = document.activeElement; tell('focus', editable(e) && !e.disabled && !e.readOnly ? { editable: true, ...like(e) } : { editable: false }); };
  addEventListener('focusin', focus, true);
  addEventListener('focusout', () => setTimeout(focus, 0), true);
  let soon = 0;
  const fields = () => {
    soon = 0;
    const out = [];
    for (const e of document.querySelectorAll('input,textarea,[contenteditable]')) {
      if (!editable(e) || e.disabled || e.readOnly) continue;
      const r = e.getBoundingClientRect();
      if (r.width < 2 || r.height < 2 || r.bottom < 0 || r.right < 0 || r.top > innerHeight || r.left > innerWidth) continue;
      const l = like(e);
      out.push([Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height), l.type, l.mode, l.ac]);
      if (out.length >= 40) break;
    }
    tell('fields', out);
  };
  const later = () => { if (!soon) soon = setTimeout(fields, 200); };
  addEventListener('scroll', later, { capture: true, passive: true });
  addEventListener('resize', later);
  addEventListener('load', later);
  new MutationObserver(later).observe(document, { subtree: true, childList: true, attributes: true, attributeFilter: ['style', 'class', 'hidden', 'disabled', 'readonly'] });
  const selected = () => { const e = document.activeElement; tell('selected', e && (e.tagName === 'INPUT' || e.tagName === 'TEXTAREA') ? e.type !== 'password' && typeof e.selectionStart === 'number' && e.selectionEnd > e.selectionStart : !!String(getSelection() || '')); };
  document.addEventListener('selectionchange', selected, true);
  addEventListener('focusin', selected, true);
  // All of it again (its tab is shown again: its viewers knew another's).
  window.__kilnFocus.again = () => { for (const k of Object.keys(said)) delete said[k]; focus(); fields(); selected(); };
})();";

/// The page's own frame rate over half a second (its animation frames, as its compositor runs them).
const PAGE_FPS: &str = "new Promise((r) => { let n = 0; const t0 = performance.now(); const f = () => { n++; if (performance.now() - t0 < 500) requestAnimationFrame(f); else r(Math.round((n * 1000) / (performance.now() - t0))); }; requestAnimationFrame(f); })";

/// What's selected in the page, to copy: a field's selection (never a password's), else the
/// document's.
const SELECTED: &str = r"(() => { const e = document.activeElement; if (e && typeof e.selectionStart === 'number' && (e.tagName === 'INPUT' || e.tagName === 'TEXTAREA')) return e.type === 'password' ? '' : e.value.slice(e.selectionStart, e.selectionEnd); return String(getSelection() || ''); })()";

/// What viewers do, applied in the order they did it (and logged, when it's recording). What
/// piled up while the page was busy goes together (`merged`): it never falls behind.
async fn apply(s: std::sync::Weak<Stream>, queued: async_channel::Receiver<Vec<Input>>) {
    while let Ok(mut events) = queued.recv().await {
        while let Ok(more) = queued.try_recv() {
            events.extend(more);
        }
        let events = merged(events);
        let Some(s) = s.upgrade() else { return };
        for e in &events {
            s.log(json!({"by": "person", "input": e}));
        }
        if let Err(e) = s.input(events).await {
            tracing::debug!(stream = %s.id, error = %format!("{e:#}"), "a viewer's input didn't reach the page");
        }
    }
}

/// Input as little as it can be and still do the same: a run of wheel turns as one (their deltas
/// summed, at the last one's point), a run of a pointer's moves as its last.
fn merged(events: Vec<Input>) -> Vec<Input> {
    let mut out: Vec<Input> = Vec::with_capacity(events.len());
    for e in events {
        if let Some(last) = out.last_mut()
            && merge(last, &e)
        {
            continue;
        }
        out.push(e);
    }
    out
}

/// `e` into `last` when it's more of the same (`merged`); whether it was.
fn merge(last: &mut Input, e: &Input) -> bool {
    match (last, e) {
        (Input::Wheel { x, y, dx, dy, mods }, Input::Wheel { x: x2, y: y2, dx: dx2, dy: dy2, mods: m2 }) if mods == m2 => {
            (*x, *y) = (*x2, *y2);
            *dx += dx2;
            *dy += dy2;
            true
        }
        (Input::Pointer { k, x, y, buttons, pt, id, mods, .. }, Input::Pointer { k: k2, x: x2, y: y2, buttons: b2, pt: pt2, id: id2, mods: m2, .. })
            if k == "move" && k2 == "move" && buttons == b2 && pt == pt2 && id == id2 && mods == m2 =>
        {
            (*x, *y) = (*x2, *y2);
            true
        }
        _ => false,
    }
}

/// What another engine's page does (`super::driven::Driven::said`), to its viewers.
async fn said(s: Arc<Stream>, out: async_channel::Sender<Out>) {
    let Surface::Driven(d) = &s.page else { return };
    while let Ok(ev) = d.said.recv().await {
        if let Some(u) = ev.pointer("/navigated/url").and_then(Value::as_str) {
            s.log(json!({"navigated": u}));
        }
        let closed = ev.get("closed").is_some();
        if out.send(Out::Event { stream: s.id.clone(), json: ev.to_string() }).await.is_err() || closed {
            return;
        }
    }
}

/// Another engine's page's picture, taken as it changes and handed to its encoder (`kiln.push`):
/// often while it changes or was just used, seldom while it's still (a picture that hasn't changed
/// goes nowhere), not at all while nobody looks and nothing records.
async fn pump(s: std::sync::Weak<Stream>) {
    use base64::Engine as _;
    let mut last: Vec<u8> = Vec::new();
    let mut changed = std::time::Instant::now();
    loop {
        let (looked, gap) = {
            let Some(st) = s.upgrade() else { return };
            let Surface::Driven(d) = &st.page else { return };
            let looked = !st.paused.load(std::sync::atomic::Ordering::Relaxed) || st.recording.lock().is_some();
            let used = crate::util::now_ms() - st.touched.load(std::sync::atomic::Ordering::Relaxed) < 2000;
            let busy = changed.elapsed() < Duration::from_secs(2) || used;
            let quick = if d.engine == super::driven::Engine::Firefox { Duration::from_millis(33) } else { Duration::from_millis(150) };
            (looked, if !looked { Duration::from_millis(500) } else if busy { quick } else { Duration::from_millis(300) })
        };
        crate::rt::sleep(gap).await;
        if !looked {
            continue;
        }
        let Some(st) = s.upgrade() else { return };
        let Surface::Driven(d) = &st.page else { return };
        match d.shot(false).await {
            Ok((bytes, mime)) if bytes != last => {
                let off = st.streamer.offscreen.lock().await.clone().unwrap_or_default();
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                if let Err(e) = super::evaluate(&st.streamer.browser.cdp, &off, &format!("kiln.push({}, \"{b64}\", \"{mime}\")", json!(st.id))).await {
                    tracing::debug!(stream = %st.id, error = %format!("{e:#}"), "a picture didn't reach its encoder");
                }
                last = bytes;
                changed = std::time::Instant::now();
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(stream = %st.id, error = %format!("{e:#}"), "no picture of the page");
                if d.said.is_closed() {
                    return;
                }
                crate::rt::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// What a tab does that its viewers' side shows, while it's the one shown: where it is (its address
/// and title), a field taking text or not, a dialog (dismissed); its end. Each tab's address and
/// title go in the stream's list of tabs, shown or not.
async fn watch(s: Arc<Stream>, target: String) {
    let Surface::Tab(tabs) = &s.page else { return };
    let Some(page) = tabs.get(&target).map(|t| t.page) else { return };
    let events = page.listen();
    while let Ok(e) = events.recv().await {
        let shown = *tabs.shown.lock() == target;
        let ev = match e.method.as_str() {
            "Page.frameNavigated" if e.params.get("frame").and_then(|f| f.get("parentId")).is_none() => {
                let url = e.params.get("frame").and_then(|f| f.get("url")).and_then(Value::as_str).unwrap_or("");
                if tabs.changed(&target, Some(url), None) {
                    s.tell_tabs().await;
                }
                if shown {
                    s.log(json!({"navigated": url}));
                }
                shown.then(|| json!({"navigated": {"url": url}}))
            }
            "Page.loadEventFired" => {
                let title = page.eval("document.title").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
                if tabs.changed(&target, None, Some(&title)) {
                    s.tell_tabs().await;
                }
                shown.then(|| json!({"loaded": {"title": title}}))
            }
            // `{"focus": ...}`, `{"fields": [...]}` or `{"selected": bool}`, as the page said it.
            "Runtime.bindingCalled" if shown && e.params.get("name").and_then(Value::as_str) == Some("kilnFocus") => serde_json::from_str::<Value>(e.params.get("payload").and_then(Value::as_str).unwrap_or("{}"))
                .ok()
                .filter(|v| v.as_object().is_some_and(|o| o.len() == 1 && ["focus", "fields", "selected"].iter().any(|k| o.contains_key(*k)))),
            // A page's alert, confirm or prompt would hold it still: answered, and said.
            "Page.javascriptDialogOpening" => {
                let _ = page.call("Page.handleJavaScriptDialog", json!({"accept": true})).await;
                shown.then(|| json!({"dialog": {"type": e.params.get("type"), "message": e.params.get("message")}}))
            }
            "Inspector.detached" | "Target.detachedFromTarget" => {
                s.tab_gone(&target).await;
                return;
            }
            _ => None,
        };
        if let Some(ev) = ev
            && s.out.send(Out::Event { stream: s.id.clone(), json: ev.to_string() }).await.is_err()
        {
            return;
        }
    }
}

/// What a profile's browser says of its pages: a tab a stream's page opened (a link to a new tab,
/// a popup) becomes the stream's (`Stream::adopt`); one changed or gone is said to its stream's
/// viewers. Each in a task of its own: a stream slow to take what it's told holds up no other's.
async fn tabs_said(events: async_channel::Receiver<super::cdp::Event>) {
    while let Ok(e) = events.recv().await {
        let info = e.params.get("targetInfo");
        let Some(id) = info.and_then(|i| i.get("targetId")).or_else(|| e.params.get("targetId")).and_then(Value::as_str).map(str::to_owned) else { continue };
        let said = |k: &str| info.and_then(|i| i.get(k)).and_then(Value::as_str);
        match e.method.as_str() {
            "Target.targetCreated" if said("type") == Some("page") => {
                let Some(opener) = said("openerId").filter(|o| !o.is_empty()).map(str::to_owned) else { continue };
                let Some(s) = streams().owning(&opener) else {
                    tracing::debug!(tab = %id, opener = %opener, "a tab opened by none of the streams' tabs");
                    continue;
                };
                crate::rt::spawn(async move {
                    if let Err(e) = s.adopt(id, Some(opener), false).await {
                        tracing::debug!(stream = %s.id, error = %format!("{e:#}"), "a tab its page opened isn't the stream's");
                    }
                });
            }
            "Target.targetInfoChanged" => {
                if let Some(s) = streams().owning(&id)
                    && let Surface::Tab(t) = &s.page
                    && t.changed(&id, said("url"), said("title"))
                {
                    crate::rt::spawn(async move { s.tell_tabs().await });
                }
            }
            "Target.targetDestroyed" => {
                if let Some(s) = streams().owning(&id) {
                    crate::rt::spawn(async move { s.tab_gone(&id).await });
                }
            }
            _ => {}
        }
    }
}

/// What a viewer does, as the server sends it (`ServerMsg::StreamInput`): each event, in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
            // A letter or a digit by where it is (`KeyA`, `Digit1`), punctuation by its key's own
            // code (`Period` is 190: its character's, 46, is Delete's).
            if let Some(c) = code.strip_prefix("Key").and_then(|c| c.chars().next()) {
                i64::from(u32::from(c.to_ascii_uppercase()))
            } else if let Some(c) = code.strip_prefix("Digit").and_then(|c| c.chars().next()) {
                i64::from(u32::from(c))
            } else if let Some(n) = oem(code) {
                n
            } else {
                // Nowhere (a phone's keyboard): by what it types; something else, none (its text
                // is what it types).
                match key.chars().next() {
                    Some(c) if c.is_ascii_alphanumeric() => i64::from(u32::from(c.to_ascii_uppercase())),
                    Some(c) => punctuation(c).unwrap_or(0),
                    None => 0,
                }
            }
        }
    }
}

/// Windows' codes for the keys of punctuation and the number pad, by where they are.
fn oem(code: &str) -> Option<i64> {
    Some(match code {
        "Space" => 32,
        "Semicolon" => 186,
        "Equal" => 187,
        "Comma" => 188,
        "Minus" => 189,
        "Period" => 190,
        "Slash" => 191,
        "Backquote" => 192,
        "BracketLeft" => 219,
        "Backslash" => 220,
        "BracketRight" => 221,
        "Quote" => 222,
        "NumpadMultiply" => 106,
        "NumpadAdd" => 107,
        "NumpadSubtract" => 109,
        "NumpadDecimal" => 110,
        "NumpadDivide" => 111,
        c => return c.strip_prefix("Numpad").and_then(|d| d.parse::<i64>().ok()).filter(|d| (0..=9).contains(d)).map(|d| 96 + d),
    })
}

/// The same, by what they type (as a US keyboard has them).
fn punctuation(c: char) -> Option<i64> {
    Some(match c {
        ';' | ':' => 186,
        '=' | '+' => 187,
        ',' | '<' => 188,
        '-' | '_' => 189,
        '.' | '>' => 190,
        '/' | '?' => 191,
        '`' | '~' => 192,
        '[' | '{' => 219,
        '\\' | '|' => 220,
        ']' | '}' => 221,
        '\'' | '"' => 222,
        ')' => 48,
        '!' => 49,
        '@' => 50,
        '#' => 51,
        '$' => 52,
        '%' => 53,
        '^' => 54,
        '&' => 55,
        '*' => 56,
        '(' => 57,
        _ => return None,
    })
}

impl Stream {
    #[allow(clippy::too_many_arguments)]
    fn new(id: &str, spec: &Spec, streamer: Arc<Streamer>, page: Surface, device: Option<super::devices::Device>, (inputs, controls): (async_channel::Sender<Vec<Input>>, async_channel::Sender<Value>), out: async_channel::Sender<Out>, dir: &Path, window: (u32, u32, f64, bool)) -> Self {
        let (width, height, scale, mobile) = window;
        Self {
            id: id.to_owned(),
            session: spec.session.clone().unwrap_or_default(),
            streamer,
            page,
            device,
            touched: std::sync::atomic::AtomicI64::new(0),
            touches: Mutex::default(),
            inputs,
            controls,
            mobile: std::sync::atomic::AtomicBool::new(mobile),
            personal: spec.profile.starts_with("personal:"),
            out,
            dir: dir.to_path_buf(),
            viewport: Mutex::new(json!({"width": width, "height": height, "scale": scale, "mobile": mobile})),
            recording: Mutex::default(),
            paused: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Into its recording's log, when it's recording.
    fn log(&self, what: Value) {
        if let Some(r) = self.recording.lock().as_ref() {
            r.log(what);
        }
    }

    /// To its viewers.
    async fn say(&self, ev: Value) {
        let _ = self.out.send(Out::Event { stream: self.id.clone(), json: ev.to_string() }).await;
    }

    /// Its tabs, to its viewers (`Tabs::said`).
    async fn tell_tabs(&self) {
        if let Surface::Tab(t) = &self.page {
            self.say(t.said()).await;
        }
    }

    /// Its window as last laid out: CSS pixels, pixel density, a phone's or not.
    fn window(&self) -> (u32, u32, f64, bool) {
        let v = self.viewport.lock().clone();
        let n = |k: &str, or: u32| v.get(k).and_then(Value::as_u64).and_then(|x| u32::try_from(x).ok()).unwrap_or(or);
        (n("width", 390), n("height", 640), v.get("scale").and_then(Value::as_f64).unwrap_or(2.0), v.get("mobile").and_then(Value::as_bool).unwrap_or(false))
    }

    /// A tab of its own from now on: one its page opened (`opener`; shown when Chrome shows it, as
    /// a link to a new tab is, not one opened in the background), or one its viewer asked for
    /// (`show`).
    async fn adopt(self: &Arc<Self>, target: String, opener: Option<String>, show: bool) -> anyhow::Result<()> {
        let Surface::Tab(tabs) = &self.page else { return Ok(()) };
        if tabs.has(&target) {
            return Ok(());
        }
        tracing::debug!(stream = %self.id, tab = %target, opener = ?opener, "a tab of its own");
        anyhow::ensure!(tabs.list.lock().len() < TABS, "it has {TABS} tabs open already");
        let cdp = self.streamer.browser.cdp.clone();
        let page = Page::attach(self.streamer.browser.clone(), None, target.clone()).await?;
        // Laid out as its others are (a device's window and agent), saying what they say.
        let (width, height, scale, mobile) = self.window();
        page.viewport(width, height, scale, mobile).await?;
        if let Some(agent) = self.device.map(|d| d.agent).filter(|a| !a.is_empty()) {
            page.call("Emulation.setUserAgentOverride", json!({"userAgent": agent})).await?;
        }
        page.call("Runtime.addBinding", json!({"name": "kilnFocus"})).await?;
        page.before_load(FOCUS).await?;
        let _ = page.eval(FOCUS).await;
        // Chrome's id for it, once its extension sees it.
        let mut found = Value::Null;
        for _ in 0..100 {
            if let Ok(v) = super::evaluate(&cdp, &self.streamer.worker, &format!("kiln.tabOf({})", json!(target))).await
                && v.get("tab").is_some_and(Value::is_number)
            {
                found = v;
                break;
            }
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
        let chrome = found.get("tab").and_then(Value::as_i64).ok_or_else(|| anyhow::anyhow!("the capture extension doesn't see the tab"))?;
        let info = cdp.call(None, "Target.getTargetInfo", json!({"targetId": target})).await.unwrap_or_default();
        let said = |k: &str| info.get("targetInfo").and_then(|i| i.get(k)).and_then(Value::as_str).unwrap_or("").to_owned();
        {
            let mut l = tabs.list.lock();
            if l.iter().any(|t| t.page.target() == target) {
                return Ok(());
            }
            l.push(Tab { page: Arc::new(page), chrome, opener, url: said("url"), title: said("title") });
        }
        crate::rt::spawn(watch(self.clone(), target.clone()));
        if show || found.get("active").and_then(Value::as_bool) == Some(true) {
            self.show(&target).await
        } else {
            self.tell_tabs().await;
            Ok(())
        }
    }

    /// Show its tab `target`: captured from now on (the same encoders, from a key frame), what its
    /// viewers do goes to it, and they're told where it is.
    async fn show(&self, target: &str) -> anyhow::Result<()> {
        let Surface::Tab(tabs) = &self.page else { anyhow::bail!("only Chromium's pages have tabs") };
        let asked = std::time::Instant::now();
        let _one = tabs.showing.lock().await;
        tracing::debug!(stream = %self.id, tab = target, waited_ms = asked.elapsed().as_millis(), "showing a tab");
        let tab = tabs.get(target).ok_or_else(|| anyhow::anyhow!("it has no such tab (it may have closed)"))?;
        if *tabs.shown.lock() != target {
            let cdp = &self.streamer.browser.cdp;
            let (width, height, scale, mobile) = self.window();
            let _ = tab.page.viewport(width, height, scale, mobile).await;
            let moved = super::evaluate(cdp, &self.streamer.worker, &format!("kiln.switchTo({}, {})", tab.chrome, json!(self.id))).await?;
            anyhow::ensure!(moved.get("ok").and_then(Value::as_bool) == Some(true), "its tab couldn't be captured: {}", moved.get("error").and_then(Value::as_str).unwrap_or("it said nothing"));
            target.clone_into(&mut tabs.shown.lock());
            // Chrome's tab too (its page sees itself in front).
            let _ = cdp.call(None, "Target.activateTarget", json!({"targetId": target})).await;
        }
        tracing::debug!(stream = %self.id, tab = target, ms = asked.elapsed().as_millis(), "a tab shown");
        // Its viewers: its tabs, where it is, its title, its fields.
        self.tell_tabs().await;
        let url = tab.page.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or(tab.url);
        let title = tab.page.eval("document.title").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or(tab.title);
        self.log(json!({"navigated": url}));
        self.say(json!({"navigated": {"url": url}})).await;
        self.say(json!({"loaded": {"title": title}})).await;
        let _ = tab.page.eval("window.__kilnFocus && window.__kilnFocus.again && window.__kilnFocus.again()").await;
        Ok(())
    }

    /// Its tab `target` closed (by its page, its viewer or the browser): with it shown, the one
    /// that opened it is in its place, else the last opened; with none left, the stream ends.
    async fn tab_gone(&self, target: &str) {
        let Surface::Tab(tabs) = &self.page else { return };
        let next = {
            let mut l = tabs.list.lock();
            let Some(i) = l.iter().position(|t| t.page.target() == target) else { return };
            let gone = l.remove(i);
            gone.opener.filter(|o| l.iter().any(|t| t.page.target() == o)).or_else(|| l.last().map(|t| t.page.target().to_owned()))
        };
        // One closing as its stream does says nothing more.
        if streams().get(&self.id).is_none() {
            return;
        }
        let shown = *tabs.shown.lock() == target;
        match next {
            None => self.say(json!({"closed": {"why": "its last tab closed"}})).await,
            Some(n) if shown => {
                if let Err(e) = self.show(&n).await {
                    tracing::debug!(stream = %self.id, error = %format!("{e:#}"), "no tab shown in place of one that closed");
                }
            }
            Some(_) => self.tell_tabs().await,
        }
    }

    /// What its viewer asks of its tabs: `{"show": id}`, `{"new": url}` (a blank one, with no
    /// address) or `{"close": id}`.
    async fn tab(&self, t: &Value) -> anyhow::Result<()> {
        let Surface::Tab(tabs) = &self.page else { anyhow::bail!("only Chromium's pages have tabs here") };
        tracing::debug!(stream = %self.id, asked = %t, "its viewer asked of its tabs");
        let cdp = &self.streamer.browser.cdp;
        if let Some(id) = t.get("show").and_then(Value::as_str) {
            self.show(id).await?;
        }
        if let Some(url) = t.get("new") {
            let url = url.as_str().map(str::trim).filter(|u| !u.is_empty()).unwrap_or("about:blank");
            anyhow::ensure!(tabs.list.lock().len() < TABS, "it has {TABS} tabs open already");
            let made = cdp.call(None, "Target.createTarget", json!({"url": url})).await?;
            let target = made.get("targetId").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("the browser made no tab"))?.to_owned();
            let me = streams().get(&self.id).ok_or_else(|| anyhow::anyhow!("it closed"))?;
            let opener = tabs.shown.lock().clone();
            me.adopt(target, Some(opener), true).await?;
        }
        if let Some(id) = t.get("close").and_then(Value::as_str) {
            anyhow::ensure!(tabs.has(id), "it has no such tab");
            cdp.call(None, "Target.closeTarget", json!({"targetId": id})).await?;
            self.tab_gone(id).await;
        }
        Ok(())
    }

    /// Record it from now (`super::record`): the stream goes on while it does, from a key frame.
    pub async fn record_start(&self) -> anyhow::Result<Value> {
        anyhow::ensure!(self.recording.lock().is_none(), "it's recording already");
        let url = self.page.eval("location.href").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        let session: String = self.session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect();
        let dir = self.dir.join("browser").join("recordings").join(if session.is_empty() { "streams".to_owned() } else { session });
        let name = format!("live-{}-{}", crate::util::now_ms(), self.id.chars().take(6).collect::<String>());
        let viewport = self.viewport.lock().clone();
        let r = crate::rt::blocking(move || super::record::Recording::start(&dir, &name, &url, viewport)).await??;
        let files = r.files.clone();
        *self.recording.lock() = Some(Arc::new(r));
        let cdp = &self.streamer.browser.cdp;
        let off = self.streamer.offscreen.lock().await.clone().unwrap_or_default();
        let id = json!(self.id);
        super::evaluate(cdp, &off, &format!("kiln.pause({id}, false)")).await?;
        super::evaluate(cdp, &off, &format!("kiln.keyframe({id})")).await?;
        let ev = json!({"recording": {"on": true, "at": crate::util::now_ms(), "video": files.video}});
        let _ = self.out.send(Out::Event { stream: self.id.clone(), json: ev.to_string() }).await;
        Ok(ev)
    }

    /// Stop recording it: its files, finished (and its viewers told where they are).
    pub async fn record_stop(&self) -> anyhow::Result<Value> {
        let r = self.recording.lock().take().ok_or_else(|| anyhow::anyhow!("it isn't recording"))?;
        let title = self.page.eval("document.title").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        let done = crate::rt::blocking(move || r.stop(&title)).await??;
        // Nobody looking: it pauses again.
        if self.paused.load(std::sync::atomic::Ordering::Relaxed) {
            let off = self.streamer.offscreen.lock().await.clone().unwrap_or_default();
            let _ = super::evaluate(&self.streamer.browser.cdp, &off, &format!("kiln.pause({}, true)", json!(self.id))).await;
        }
        let recorded = json!({"recorded": {"session": self.session, "at": crate::util::now_ms(), "video": done.files.video, "inputs": done.files.inputs, "test": done.files.test, "seconds": done.seconds, "bytes": done.bytes, "pictures": done.pictures}});
        for ev in [json!({"recording": {"on": false}}), recorded.clone()] {
            let _ = self.out.send(Out::Event { stream: self.id.clone(), json: ev.to_string() }).await;
        }
        Ok(recorded.get("recorded").cloned().unwrap_or_default())
    }

    /// Do again what a recording's log (`path`) did, as it was done: its page, its window, then
    /// each input and action after its pause (`super::record::replay_steps`).
    async fn replay(&self, path: &str) -> anyhow::Result<usize> {
        let p = PathBuf::from(path);
        let log = crate::rt::blocking(move || super::record::read_log(&p)).await??;
        let (url, viewport, steps) = super::record::replay_steps(&log);
        if let Some(v) = viewport {
            let n = |k: &str| v.get(k).and_then(Value::as_u64).and_then(|x| u32::try_from(x).ok());
            if let (Some(w), Some(h)) = (n("width"), n("height")) {
                let scale = v.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
                let mobile = v.get("mobile").and_then(Value::as_bool).unwrap_or(false);
                self.page.viewport(w, h, scale, mobile).await?;
                self.mobile.store(mobile, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(u) = url {
            self.page.navigate(&u).await?;
            crate::rt::sleep(Duration::from_millis(500)).await;
        }
        let n = steps.len();
        for (pause, step) in steps {
            crate::rt::sleep(pause).await;
            if let Some(i) = step.get("input").cloned().and_then(|i| serde_json::from_value::<Input>(i).ok()) {
                self.input(vec![i]).await?;
            } else if let Some((verb, arg)) = step.get("act").and_then(Value::as_object).and_then(|o| o.iter().next()) {
                act_one(self, verb, arg).await?;
            }
        }
        Ok(n)
    }

    /// What a viewer did, to the tab.
    pub async fn input(&self, events: Vec<Input>) -> anyhow::Result<()> {
        self.touched.store(crate::util::now_ms(), std::sync::atomic::Ordering::Relaxed);
        let page = match &self.page {
            Surface::Tab(t) => t.page()?,
            Surface::Driven(d) => return d.input(&events).await,
        };
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
                    page.call("Input.dispatchTouchEvent", json!({"type": kind, "touchPoints": points, "modifiers": mods})).await?;
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
                    page.call("Input.dispatchMouseEvent", json!({"type": kind, "x": x, "y": y, "button": which, "buttons": buttons, "clickCount": clicks.max(if kind == "mouseMoved" { 0 } else { 1 }), "modifiers": mods})).await?;
                }
                Input::Wheel { x, y, dx, dy, mods } => {
                    page.call("Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy, "modifiers": mods})).await?;
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
                    page.call("Input.dispatchKeyEvent", p).await?;
                }
                Input::Text { text } => {
                    page.call("Input.insertText", json!({"text": text})).await?;
                }
                Input::Compose { text, start, end } => {
                    page.call("Input.imeSetComposition", json!({"text": text, "selectionStart": start, "selectionEnd": end})).await?;
                }
            }
        }
        Ok(())
    }

    /// What a viewer asks of the stream (`ServerMsg::StreamControl`): `{"resize": {width, height,
    /// scale, mobile}}`, `{"pause": bool}`, `{"keyframe": true}`, `{"go": url}`, `{"reload": true}`,
    /// `{"back": true}`, `{"forward": true}`, `{"tab": {"show" | "new" | "close": ...}}` (`Self::tab`),
    /// `{"stats": true}` (its counts, said as `{"stats": ...}`).
    pub async fn control(&self, c: &Value) -> anyhow::Result<()> {
        let off = self.streamer.offscreen.lock().await.clone().unwrap_or_default();
        let cdp = &self.streamer.browser.cdp;
        let id = json!(self.id);
        // Held to a device, its window is the device's, whatever its viewers' is.
        if let Some(r) = c.get("resize").filter(|_| self.device.is_none()) {
            let w = r.get("width").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(390).clamp(200, 3840);
            let h = r.get("height").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(844).clamp(200, 3840);
            let scale = r.get("scale").and_then(Value::as_f64).unwrap_or(2.0).clamp(1.0, 3.0);
            let mobile = r.get("mobile").and_then(Value::as_bool).unwrap_or(false);
            self.page.viewport(w, h, scale, mobile).await?;
            self.mobile.store(mobile, std::sync::atomic::Ordering::Relaxed);
            let vp = json!({"width": w, "height": h, "scale": scale, "mobile": mobile});
            *self.viewport.lock() = vp.clone();
            self.log(json!({"viewport": vp}));
            // A field being typed in stays in sight as the window shrinks (a phone's keyboard came up).
            let _ = self.page.eval("document.activeElement && document.activeElement !== document.body && document.activeElement.scrollIntoView({ block: 'nearest' })").await;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (pw, ph) = ((f64::from(w) * scale).round() as u32, (f64::from(h) * scale).round() as u32);
            // A tab's captured again at its new size, once it's laid out at it (a capture keeps
            // the size it began at); another engine's pictures come at it.
            if let Surface::Tab(t) = &self.page {
                // One change of its capture at a time: a tab shown meanwhile would be captured
                // twice, one of them never let go.
                let _one = t.showing.lock().await;
                let shown = t.shown.lock().clone();
                if let Some(tab) = t.get(&shown) {
                    let _ = tab.page.settle().await;
                    let again = super::evaluate(cdp, &self.streamer.worker, &format!("kiln.recapture({}, {id}, {pw}, {ph})", tab.chrome)).await?;
                    anyhow::ensure!(again.get("ok").and_then(Value::as_bool) == Some(true), "its tab couldn't be captured at its new size: {}", again.get("error").and_then(Value::as_str).unwrap_or("it said nothing"));
                }
            }
        }
        if let Some(p) = c.get("pause").and_then(Value::as_bool) {
            self.paused.store(p, std::sync::atomic::Ordering::Relaxed);
            // While it records it goes on, looked at or not.
            if !p || self.recording.lock().is_none() {
                super::evaluate(cdp, &off, &format!("kiln.pause({id}, {p})")).await?;
            }
        }
        match c.get("record").and_then(Value::as_bool) {
            Some(true) => {
                self.record_start().await?;
            }
            Some(false) => {
                self.record_stop().await?;
            }
            None => {}
        }
        if c.get("keyframe").and_then(Value::as_bool) == Some(true) {
            super::evaluate(cdp, &off, &format!("kiln.keyframe({id})")).await?;
        }
        // What's selected, for its viewer's clipboard (at most a megabyte of it).
        if c.get("copy").and_then(Value::as_bool) == Some(true) {
            let text: String = self.page.eval(SELECTED).await?.as_str().unwrap_or("").chars().take(1 << 20).collect();
            let _ = self.out.send(Out::Event { stream: self.id.clone(), json: json!({"copied": {"text": text}}).to_string() }).await;
        }
        if let Some(url) = c.get("go").and_then(Value::as_str) {
            self.page.navigate(url).await?;
        }
        if c.get("reload").and_then(Value::as_bool) == Some(true) {
            self.page.reload().await?;
        }
        for (k, delta) in [("back", -1), ("forward", 1)] {
            if c.get(k).and_then(Value::as_bool) == Some(true) {
                self.page.history(delta).await?;
            }
        }
        if let Some(t) = c.get("tab") {
            self.tab(t).await?;
        }
        // Its counts, for whoever's measuring: what its capture took, encoded and let go since
        // last asked, and the page's own frame rate.
        if c.get("stats").and_then(Value::as_bool) == Some(true) {
            let encoder = super::evaluate(cdp, &off, &format!("kiln.stats({id})")).await.unwrap_or_default();
            let page = self.page.eval(PAGE_FPS).await.unwrap_or_default();
            let scrolled = self.page.eval("[scrollX, scrollY]").await.unwrap_or_default();
            self.say(json!({"stats": {"encoder": encoder, "page_fps": page, "scrolled": scrolled, "window": *self.viewport.lock()}})).await;
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
    fn input_that_piled_up_goes_together() -> anyhow::Result<()> {
        let wheel = |dy: f64| Input::Wheel { x: 10.0, y: dy, dx: 0.0, dy, mods: 0 };
        let p = |k: &str, x: f64| Input::Pointer { k: k.into(), x, y: 5.0, button: 0, buttons: 0, pt: "mouse".into(), id: 1, mods: 0, clicks: 0 };
        let out = merged(vec![wheel(10.0), wheel(20.0), wheel(30.0), p("move", 1.0), p("move", 2.0), p("down", 2.0), p("move", 3.0), Input::Text { text: "a".into() }, wheel(5.0)]);
        let said: Vec<String> = out.iter().map(|e| serde_json::to_string(e).unwrap_or_default()).collect();
        assert_eq!(said.len(), 6, "{said:?}");
        // The wheel's three as one, at the last's point; the moves as their last, the press kept
        // in its place.
        assert!(said.first().is_some_and(|w| w.contains("\"dy\":60.0") && w.contains("\"y\":30.0")), "{said:?}");
        assert!(said.get(1).is_some_and(|m| m.contains("\"k\":\"move\"") && m.contains("\"x\":2.0")), "{said:?}");
        assert!(said.get(2).is_some_and(|d| d.contains("\"k\":\"down\"")), "{said:?}");
        assert!(said.get(5).is_some_and(|w| w.contains("\"dy\":5.0")), "{said:?}");
        Ok(())
    }

    #[test]
    fn a_chunk_bigger_than_a_message_goes_in_parts() -> anyhow::Result<()> {
        let big = base64::engine::general_purpose::STANDARD.encode(vec![7u8; PART * 2 + 10]);
        let (stream, kind, flags, ts, bytes) = parse(&format!("s1 v k 1234 {big}")).ok_or_else(|| anyhow::anyhow!("not a chunk"))?;
        assert_eq!((stream.as_str(), kind, flags, ts, bytes.len()), ("s1", b'v', b'k', 1234, PART * 2 + 10));
        let out = chunk(&stream, kind, flags, ts, &bytes).unwrap_or_default();
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
            let opened = streams().open(&dir, "test-stream", &spec, Outs::one(tx)).await?;
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

    /// A site on loopback for a test: each path's page.
    fn site(pages: &'static [(&'static str, &'static str)]) -> anyhow::Result<String> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let at = format!("http://{}", l.local_addr()?);
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let n = std::io::Read::read(&mut c, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let body = pages.iter().find(|(p, _)| *p == path).map_or("", |(_, b)| *b);
                let _ = std::io::Write::write_all(&mut c, format!("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes());
            }
        });
        Ok(at)
    }

    /// What a test's stream sent: its events, and its pictures' times as they came.
    #[derive(Default)]
    struct Heard {
        events: Vec<Value>,
        times: Vec<i64>,
    }

    impl Heard {
        /// What came until `done` says so, or `within` passed: whether it did.
        async fn until(&mut self, rx: &async_channel::Receiver<Out>, within: Duration, done: impl Fn(&Self) -> bool) -> bool {
            let end = std::time::Instant::now() + within;
            while std::time::Instant::now() < end {
                if done(self) {
                    return true;
                }
                match crate::rt::timeout(end.saturating_duration_since(std::time::Instant::now()).min(Duration::from_millis(100)), rx.recv()).await {
                    Ok(Ok(Out::Event { json, .. })) => self.events.push(serde_json::from_str(&json).unwrap_or_default()),
                    Ok(Ok(Out::Frame { data, .. })) if data.first() == Some(&b'v') => {
                        if let Some(ts) = data.get(2..10).and_then(|b| <[u8; 8]>::try_from(b).ok()) {
                            self.times.push(i64::from_be_bytes(ts));
                        }
                    }
                    _ => {}
                }
            }
            done(self)
        }

        /// The last list of tabs said: their ids, and the shown one's.
        fn tabs(&self) -> (Vec<String>, String) {
            let Some(t) = self.events.iter().rev().find_map(|e| e.get("tabs")) else { return (Vec::new(), String::new()) };
            let ids = t.get("list").and_then(Value::as_array).map(|l| l.iter().filter_map(|x| x.get("id").and_then(Value::as_str).map(str::to_owned)).collect()).unwrap_or_default();
            (ids, t.get("shown").and_then(Value::as_str).unwrap_or("").to_owned())
        }
    }

    /// How a page scrolled by the wheel streams: where it is, and the pictures its capture takes.
    #[test]
    #[ignore = "downloads and starts the real browser (about 200 MB)"]
    fn a_scrolled_page_streams_as_it_scrolls() -> anyhow::Result<()> {
        static PAGES: &[(&str, &str)] = &[("/", "<title>long</title><body style='margin:0'><script>for (let i = 0; i < 3000; i++) document.write('<div style=\"height:20px;background:hsl(' + (i * 7 % 360) + ',60%,40%)\">line ' + i + '</div>')</script></body>")];
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        let at = site(PAGES)?;
        crate::rt::block_on(async move {
            let spec: Spec = serde_json::from_value(json!({"url": format!("{at}/"), "width": 394, "height": 538, "scale": 2, "profile": "clean"}))?;
            let (tx, rx) = async_channel::bounded::<Out>(4096);
            streams().open(&dir, "scroll-test", &spec, Outs::one(tx)).await?;
            let s = streams().get("scroll-test").ok_or_else(|| anyhow::anyhow!("not open"))?;
            s.control(&json!({"pause": false, "keyframe": true})).await?;
            let mut h = Heard::default();
            h.until(&rx, Duration::from_secs(3), |_| false).await;
            let before = h.times.len();
            let t0 = std::time::Instant::now();
            // A wheel's turn each sixtieth of a second, as a viewer's come, for two seconds.
            for _ in 0..120 {
                s.inputs.try_send(vec![Input::Wheel { x: 200.0, y: 200.0, dx: 0.0, dy: 40.0, mods: 0 }])?;
                h.until(&rx, Duration::from_millis(16), |_| false).await;
            }
            let secs = t0.elapsed().as_secs_f64();
            h.until(&rx, Duration::from_millis(300), |_| false).await;
            let y = s.page.eval("scrollY").await?.as_f64().unwrap_or(0.0);
            let pictures = h.times.len() - before;
            streams().close("scroll-test").await;
            // All of it reached the page, and its pictures came as it moved.
            assert!((y - 4800.0).abs() < 1.0, "scrolled to {y}");
            #[allow(clippy::cast_precision_loss)]
            let rate = pictures as f64 / secs;
            assert!(rate > 40.0, "{pictures} pictures in {secs:.2} s");
            Ok(())
        })
    }

    /// A stream resized by its viewer, to sizes a capture would come odd at if left to choose: it's
    /// captured again at its size exactly, even, at the page's rate, and no picture is a key frame
    /// of its own (cut to even, each was: an H.264 stream at many times its bitrate).
    #[test]
    #[ignore = "downloads and starts the real browser (about 200 MB)"]
    fn a_resized_stream_comes_at_its_size_with_few_key_frames() -> anyhow::Result<()> {
        static PAGES: &[(&str, &str)] = &[("/", "<body style='margin:0;background:#123'><div id=b style='width:40px;height:40px;background:#bab0e3;position:absolute'></div><script>let t=0;(function f(){t++;b.style.left=(t*3%300)+'px';b.style.top=(t*2%300)+'px';requestAnimationFrame(f)})()</script></body>")];
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        let at = site(PAGES)?;
        crate::rt::block_on(async move {
            let mut out = Vec::new();
            for (n, (w, h, scale)) in [(394u32, 538u32, 2.0f64), (395, 539, 1.0), (390, 533, 2.0)].into_iter().enumerate() {
                let id = format!("odd-test-{n}");
                // Opened at a phone's size, then its viewer's own (as a viewer's frame says it).
                let spec: Spec = serde_json::from_value(json!({"url": format!("{at}/"), "width": 390, "height": 640, "scale": 2, "profile": "clean"}))?;
                let (tx, rx) = async_channel::bounded::<Out>(4096);
                streams().open(&dir, &id, &spec, Outs::one(tx)).await?;
                let s = streams().get(&id).ok_or_else(|| anyhow::anyhow!("not open"))?;
                s.control(&json!({"pause": false, "keyframe": true})).await?;
                s.control(&json!({"resize": {"width": w, "height": h, "scale": scale, "mobile": false}})).await?;
                let mut heard = Heard::default();
                heard.until(&rx, Duration::from_secs(2), |_| false).await;
                let off = s.streamer.offscreen.lock().await.clone().unwrap_or_default();
                let _ = super::super::evaluate(&s.streamer.browser.cdp, &off, &format!("kiln.stats({})", json!(id))).await?;
                heard.until(&rx, Duration::from_secs(2), |_| false).await;
                let stats = super::super::evaluate(&s.streamer.browser.cdp, &off, &format!("kiln.stats({})", json!(id))).await?;
                streams().close(&id).await;
                let n = |k: &str| stats.get(k).and_then(Value::as_u64).unwrap_or(0);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let want = json!([(f64::from(w) * scale).round() as u32 / 2 * 2, (f64::from(h) * scale).round() as u32 / 2 * 2]);
                let track = stats.get("track").and_then(Value::as_array).map(|t| json!([t.first(), t.get(1)])).unwrap_or_default();
                if track != want || n("keys") > 2 || n("taken") < 100 {
                    out.push(format!("{w}x{h}@{scale}: want {want}, {stats}"));
                }
            }
            assert!(out.is_empty(), "{out:#?}");
            Ok(())
        })
    }

    /// Tabs, as a browser has them: a link to a new tab opens one, shown (its pictures go on, their
    /// times after the last's); the first shown again; the second closed; one its viewer opened.
    #[test]
    #[ignore = "downloads and starts the real browser (about 200 MB)"]
    fn a_link_to_a_new_tab_opens_one() -> anyhow::Result<()> {
        static PAGES: &[(&str, &str)] = &[
            ("/", "<title>first</title><body style='margin:0'><a href='/two' target=_blank style='display:block;width:300px;height:100px;background:#123'>two</a><button onclick=\"window.open('/two', 'pop', 'width=300,height=200')\" style='display:block;width:300px;height:100px'>pop</button></body>"),
            ("/two", "<title>second</title><body style='margin:0;background:#bab0e3'><div id=b style='width:40px;height:40px;background:#123;position:absolute'></div><script>let t=0;(function f(){t++;b.style.left=(t*3%200)+'px';requestAnimationFrame(f)})()</script></body>"),
        ];
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        let at = site(PAGES)?;
        crate::rt::block_on(async move {
            let spec: Spec = serde_json::from_value(json!({"url": format!("{at}/"), "width": 320, "height": 240, "scale": 1, "profile": "clean"}))?;
            let (tx, rx) = async_channel::bounded::<Out>(4096);
            streams().open(&dir, "tabs-test", &spec, Outs::one(tx)).await?;
            let s = streams().get("tabs-test").ok_or_else(|| anyhow::anyhow!("not open"))?;
            s.control(&json!({"pause": false, "keyframe": true})).await?;
            let mut h = Heard::default();
            let loaded = h.until(&rx, Duration::from_secs(10), |h| h.events.iter().any(|e| e.pointer("/loaded/title") == Some(&json!("first")))).await;
            assert!(loaded, "the first page didn't load: {:?}", h.events);
            let (first, _) = h.tabs();
            let first = first.first().cloned().unwrap_or_default();
            // Its link, clicked: a new tab, shown.
            let click = |k: &str, buttons: i64| Input::Pointer { k: k.into(), x: 100.0, y: 50.0, button: 0, buttons, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 };
            s.input(vec![click("down", 1), click("up", 0)]).await?;
            let opened = h.until(&rx, Duration::from_secs(10), |h| {
                let (ids, shown) = h.tabs();
                ids.len() == 2 && !shown.is_empty() && shown != first
            }).await;
            assert!(opened, "no second tab shown: {:?}", h.events);
            let (ids, second) = h.tabs();
            let before = h.times.len();
            h.until(&rx, Duration::from_secs(2), |_| false).await;
            assert!(h.times.len() > before + 10, "its pictures stopped after the switch: {} then {}", before, h.times.len());
            assert!(h.events.iter().any(|e| e.pointer("/loaded/title") == Some(&json!("second"))), "{:?}", h.events);
            // The first again; its popup.
            s.control(&json!({"tab": {"show": first}})).await?;
            assert!(h.until(&rx, Duration::from_secs(5), |h| h.tabs().1 == first).await, "the first tab wasn't shown again");
            let pop = |k: &str, buttons: i64| Input::Pointer { k: k.into(), x: 100.0, y: 150.0, button: 0, buttons, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 };
            s.input(vec![pop("down", 1), pop("up", 0)]).await?;
            let popped = h.until(&rx, Duration::from_secs(10), |h| h.tabs().0.len() == 3).await;
            assert!(popped, "no popup tab: {:?}", h.tabs());
            let (all, popup) = h.tabs();
            s.control(&json!({"tab": {"close": popup}})).await?;
            assert!(h.until(&rx, Duration::from_secs(5), |h| h.tabs().0.len() == 2 && h.tabs().1 == first).await, "the popup closed, its opener not shown: {:?} of {all:?}", h.tabs());
            // The second closed.
            s.control(&json!({"tab": {"close": second}})).await?;
            assert!(h.until(&rx, Duration::from_secs(5), |h| h.tabs().0.len() == 1).await, "the second tab didn't close: {:?}", h.tabs());
            // One its viewer opened.
            s.control(&json!({"tab": {"new": format!("{at}/two")}})).await?;
            let third = h.until(&rx, Duration::from_secs(10), |h| {
                let (ids, shown) = h.tabs();
                ids.len() == 2 && shown != first
            }).await;
            assert!(third, "no new tab: {:?}", h.tabs());
            crate::rt::sleep(Duration::from_millis(500)).await;
            let title = s.page.eval("document.title").await?;
            h.until(&rx, Duration::from_secs(1), |_| false).await;
            streams().close("tabs-test").await;
            assert_eq!(title, json!("second"));
            assert!(ids.contains(&first), "{ids:?}");
            // Its pictures' times only went on.
            let back: Vec<_> = h.times.windows(2).filter(|w| w.get(1) < w.first()).collect();
            assert!(back.is_empty(), "times went back: {back:?}");
            Ok(())
        })
    }

    /// Another engine: this machine's Firefox, its page's pictures taken as it changes and encoded
    /// as a tab's are (a device's window, its user agent); a click reaching it; where it went.
    #[test]
    #[ignore = "needs Firefox installed, and downloads and starts the real browser (about 200 MB)"]
    fn firefox_streams_a_page_as_a_device() -> anyhow::Result<()> {
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        crate::rt::block_on(async move {
            let page = "data:text/html,<title>fox</title><body style='margin:0;background:%23123'><div id=b style='width:40px;height:40px;background:%23bab0e3;position:absolute'></div><script>let t=0;addEventListener('pointerdown',()=>document.title='clicked');setInterval(()=>{t++;b.style.left=(t*7%25200)+'px'},50)</script></body>";
            // A conversation's profile (kept), as the Tools tab opens one.
            let spec: Spec = serde_json::from_value(json!({"url": page, "profile": std::env::var("KILN_TEST_PROFILE").unwrap_or_else(|_| "clean".into()), "engine": "firefox", "device": "iPhone SE"}))?;
            let (tx, rx) = async_channel::bounded::<Out>(4096);
            let opened = streams().open(&dir, "test-firefox", &spec, Outs::one(tx)).await?;
            assert_eq!(opened.pointer("/opened/engine"), Some(&json!("firefox")), "{opened}");
            assert_eq!(opened.pointer("/opened/device/name"), Some(&json!("iPhone SE")), "{opened}");
            let s = streams().get("test-firefox").ok_or_else(|| anyhow::anyhow!("not open"))?;
            s.control(&json!({"pause": false, "keyframe": true})).await?;
            let (mut configs, mut keys, mut frames, mut size, mut said) = (0, 0, 0, json!(null), Vec::new());
            let end = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < end {
                match crate::rt::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Ok(Out::Frame { data, .. })) => match (data.first(), data.get(1)) {
                        (Some(b'c'), _) => {
                            configs += 1;
                            size = data.get(12..).and_then(|b| serde_json::from_slice::<Value>(b).ok()).unwrap_or_default();
                        }
                        (Some(b'v'), Some(b'k')) => keys += 1,
                        (Some(b'v'), _) => frames += 1,
                        _ => {}
                    },
                    Ok(Ok(Out::Event { json, .. })) => said.push(json),
                    _ => {}
                }
            }
            // A resize from a viewer leaves a device's window be.
            s.control(&json!({"resize": {"width": 800, "height": 600, "scale": 1, "mobile": false}})).await?;
            let window = s.page.eval("[innerWidth, innerHeight, devicePixelRatio, navigator.userAgent]").await?;
            s.input(vec![Input::Pointer { k: "down".into(), x: 100.0, y: 100.0, button: 0, buttons: 1, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }, Input::Pointer { k: "up".into(), x: 100.0, y: 100.0, button: 0, buttons: 0, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }]).await?;
            crate::rt::sleep(Duration::from_millis(300)).await;
            let title = s.page.eval("document.title").await?;
            streams().close("test-firefox").await;
            super::super::driven::stop_all().await;
            assert!(configs >= 1 && keys >= 1 && frames > 20, "configs {configs}, key frames {keys}, frames {frames}");
            assert_eq!((size.get("codedWidth"), size.get("codedHeight")), (Some(&json!(750)), Some(&json!(1334))), "the device's window at its density: {size}");
            assert_eq!(window.as_array().and_then(|w| w.get(..3)), Some(&[json!(375), json!(667), json!(2)][..]), "{window}");
            assert!(window.get(3).and_then(Value::as_str).is_some_and(|ua| ua.contains("iPhone")), "{window}");
            assert_eq!(title, json!("clicked"));
            assert!(said.iter().any(|e| e.contains("\"loaded\"") && e.contains("fox")), "{said:?}");
            Ok(())
        })
    }

    /// A viewer that joins once another engine's page has gone still is sent its picture at once
    /// (a key frame of the last one, asked for as it joins).
    #[test]
    #[ignore = "needs Firefox installed, and downloads and starts the real browser (about 200 MB)"]
    fn firefox_answers_a_late_viewer() -> anyhow::Result<()> {
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        crate::rt::block_on(async move {
            let page = "data:text/html,<title>still</title><body style='margin:0;background:%23246'><p style='color:white'>still</p></body>";
            let spec: Spec = serde_json::from_value(json!({"url": page, "profile": "clean", "engine": "firefox", "device": "iPhone SE"}))?;
            let (tx, rx) = async_channel::bounded::<Out>(4096);
            streams().open(&dir, "test-late", &spec, Outs::one(tx)).await?;
            let s = streams().get("test-late").ok_or_else(|| anyhow::anyhow!("not open"))?;
            // Nobody looks for a while: the page goes still.
            crate::rt::sleep(Duration::from_secs(3)).await;
            while rx.try_recv().is_ok() {}
            s.control(&json!({"pause": false, "keyframe": true})).await?;
            let (mut configs, mut keys) = (0, 0);
            let end = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < end {
                if let Ok(Ok(Out::Frame { data, .. })) = crate::rt::timeout(Duration::from_millis(100), rx.recv()).await {
                    match (data.first(), data.get(1)) {
                        (Some(b'c'), _) => configs += 1,
                        (Some(b'v'), Some(b'k')) => keys += 1,
                        _ => {}
                    }
                }
            }
            streams().close("test-late").await;
            super::super::driven::stop_all().await;
            assert!(configs >= 1 && keys >= 1, "configs {configs}, key frames {keys}");
            Ok(())
        })
    }

    /// An iPhone's Mobile Safari in kiln's simulator of it (`super::super::simulators`), on a Mac
    /// with Xcode and an iOS runtime: a page from this Mac streams from it (its 1179 pixels a
    /// column less, H.264's sizes being even), laid out as the phone lays it out, and a tap on it
    /// is a touch where it was.
    #[test]
    #[ignore = "needs a Mac with Xcode and an iOS simulator runtime (it boots a simulator)"]
    fn an_iphone_streams_from_its_simulator() -> anyhow::Result<()> {
        if !cfg!(target_os = "macos") {
            return Ok(());
        }
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        // The page, from this Mac (the simulator shares its network): whatever's asked, it.
        let site = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = site.local_addr()?.port();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let page = "<!doctype html><meta name=viewport content='width=device-width'><title>phone</title><body style='margin:0;background:#123;height:2000px'><script>addEventListener('pointerdown',e=>document.title='tapped '+Math.round(e.clientX)+','+Math.round(e.clientY)+' '+e.pointerType)</script></body>";
            for conn in site.incoming().flatten() {
                let mut conn = conn;
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf);
                let _ = write!(conn, "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}", page.len());
            }
        });
        crate::rt::block_on(async move {
            let spec: Spec = serde_json::from_value(json!({"url": format!("http://127.0.0.1:{port}/"), "width": 393, "height": 852, "scale": 3, "profile": "clean", "engine": "webkit", "device": "iPhone 15"}))?;
            let (tx, rx) = async_channel::bounded::<Out>(4096);
            let opened = streams().open(&dir, "test-iphone", &spec, Outs::one(tx)).await?;
            assert_eq!((opened.pointer("/opened/engine"), opened.pointer("/opened/device/name")), (Some(&json!("webkit")), Some(&json!("iPhone 15"))), "{opened}");
            let s = streams().get("test-iphone").ok_or_else(|| anyhow::anyhow!("not open"))?;
            s.control(&json!({"pause": false, "keyframe": true})).await?;
            let mut keys = 0;
            let end = std::time::Instant::now() + Duration::from_secs(8);
            while std::time::Instant::now() < end {
                if let Ok(Ok(Out::Frame { data, .. })) = crate::rt::timeout(Duration::from_millis(200), rx.recv()).await
                    && data.first() == Some(&b'v')
                    && data.get(1) == Some(&b'k')
                {
                    keys += 1;
                }
            }
            let window = s.page.eval("[innerWidth, navigator.userAgent]").await?;
            s.input(vec![Input::Pointer { k: "down".into(), x: 100.0, y: 200.0, button: 0, buttons: 1, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }, Input::Pointer { k: "up".into(), x: 100.0, y: 200.0, button: 0, buttons: 0, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }]).await?;
            crate::rt::sleep(Duration::from_millis(800)).await;
            let title = s.page.eval("document.title").await?;
            streams().close("test-iphone").await;
            super::super::driven::stop_all().await;
            assert!(keys >= 1, "key frames {keys}");
            assert_eq!(window.get(0), Some(&json!(393)), "laid out at the phone's width: {window}");
            assert!(window.get(1).and_then(Value::as_str).is_some_and(|ua| ua.contains("iPhone")), "{window}");
            assert_eq!(title, json!("tapped 100,200 touch"), "a tap, as a touch where it was");
            Ok(())
        })
    }

    /// WebKit's way in, WebDriver over HTTP (Safari's, WebKitGTK's), tried with whichever
    /// WebDriver `KILN_WEBDRIVER` names (and `KILN_WEBDRIVER_CAPS` its capabilities: chromedriver
    /// does here), else on a Mac with its own Safari as kiln drives it: its pictures (PNG) as it
    /// changes, a click reaching it, where it went, said.
    #[test]
    #[ignore = "needs KILN_WEBDRIVER (a WebDriver) or a Mac with Safari's remote automation on, and downloads and starts the real browser (about 200 MB)"]
    fn webdriver_streams_a_page() -> anyhow::Result<()> {
        if std::env::var_os("KILN_WEBDRIVER").is_none() && !(cfg!(target_os = "macos") && std::path::Path::new("/usr/bin/safaridriver").is_file()) {
            return Ok(());
        }
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        crate::rt::block_on(async move {
            let page = "data:text/html,<title>driven</title><body style='margin:0;background:%23123'><div id=b style='width:40px;height:40px;background:%23bab0e3;position:absolute'></div><script>let t=0;addEventListener('pointerdown',e=>document.title='clicked '+e.clientX+','+e.clientY);setInterval(()=>{t++;b.style.left=(t*7%25200)+'px'},80)</script></body>";
            let spec: Spec = serde_json::from_value(json!({"url": page, "width": 400, "height": 300, "scale": 1, "profile": "clean", "engine": "webkit"}))?;
            let (tx, rx) = async_channel::bounded::<Out>(4096);
            let opened = streams().open(&dir, "test-webdriver", &spec, Outs::one(tx)).await?;
            assert_eq!(opened.pointer("/opened/engine"), Some(&json!("webkit")), "{opened}");
            let s = streams().get("test-webdriver").ok_or_else(|| anyhow::anyhow!("not open"))?;
            s.control(&json!({"pause": false, "keyframe": true})).await?;
            // Its pictures for four seconds from its first (a browser's window takes a while to come
            // up). A picture after a pause starts from a key frame, so a slower engine's are mostly
            // key frames: they're counted all the same.
            let (mut keys, mut frames, mut said) = (0, 0, Vec::new());
            let start = std::time::Instant::now();
            let mut end = start + Duration::from_secs(15);
            while std::time::Instant::now() < end {
                match crate::rt::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Ok(Out::Frame { data, .. })) if data.first() == Some(&b'v') => {
                        if keys + frames == 0 {
                            end = std::time::Instant::now() + Duration::from_secs(4);
                        }
                        if data.get(1) == Some(&b'k') { keys += 1 } else { frames += 1 }
                    }
                    Ok(Ok(Out::Event { json, .. })) => said.push(json),
                    _ => {}
                }
            }
            let window = s.page.eval("[innerWidth, innerHeight]").await?;
            // Safari's own (no other WebDriver named): its Mac's display is kept awake meanwhile.
            let awake = std::env::var_os("KILN_WEBDRIVER").is_some() || std::process::Command::new("pmset").args(["-g", "assertions"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("caffeinate"));
            s.input(vec![Input::Pointer { k: "down".into(), x: 120.0, y: 90.0, button: 0, buttons: 1, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }, Input::Pointer { k: "up".into(), x: 120.0, y: 90.0, button: 0, buttons: 0, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }]).await?;
            let clicked = std::time::Instant::now();
            let mut title = s.page.eval("document.title").await?;
            while title != json!("clicked 120,90") && clicked.elapsed() < Duration::from_secs(3) {
                crate::rt::sleep(Duration::from_millis(100)).await;
                title = s.page.eval("document.title").await?;
            }
            eprintln!("pictures in 4 s: {keys} key frames, {frames} more; the click landed in {:?}", clicked.elapsed());
            streams().close("test-webdriver").await;
            crate::rt::sleep(Duration::from_millis(500)).await;
            super::super::driven::stop_all().await;
            assert!(keys >= 1 && keys + frames >= 5, "key frames {keys}, frames {frames}");
            assert_eq!(window, json!([400, 300]), "its window, as asked");
            assert!(awake, "Safari's Mac kept awake while it streams (caffeinate)");
            assert_eq!(title, json!("clicked 120,90"));
            assert!(said.iter().any(|e| e.contains("\"navigated\"")) && said.iter().any(|e| e.contains("\"loaded\"") && e.contains("driven")), "{said:?}");
            Ok(())
        })
    }

    #[test]
    fn keys_have_their_codes_and_punctuation_is_no_other_key() {
        assert_eq!(key_code("Enter", "Enter"), 13);
        assert_eq!(key_code("a", "KeyA"), 65);
        assert_eq!(key_code("!", "Digit1"), 49);
        assert_eq!(key_code("F5", "F5"), 116);
        // A period isn't Delete (46), nor a percent sign the left arrow (37).
        assert_eq!(key_code(".", "Period"), 190);
        assert_eq!(key_code(".", ""), 190);
        assert_eq!(key_code("%", ""), 53);
        assert_eq!(key_code("-", "Minus"), 189);
        assert_eq!(key_code("'", ""), 222);
        assert_eq!(key_code("4", "Numpad4"), 100);
        assert_eq!(key_code("é", ""), 0);
    }
}
