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

/// What a stream's page is: a Chromium tab (captured as its compositor draws it), or a page of
/// another engine's (`super::driven`: its picture taken as it changes).
pub(crate) enum Surface {
    Tab(Page),
    Driven(super::driven::Driven),
}

impl Surface {
    async fn eval(&self, js: &str) -> anyhow::Result<Value> {
        match self {
            Self::Tab(p) => p.eval(js).await,
            Self::Driven(d) => d.eval(js).await,
        }
    }

    async fn navigate(&self, url: &str) -> anyhow::Result<()> {
        match self {
            Self::Tab(p) => p.call("Page.navigate", json!({"url": url})).await.map(|_| ()),
            Self::Driven(d) => d.navigate(url).await,
        }
    }

    async fn reload(&self) -> anyhow::Result<()> {
        match self {
            Self::Tab(p) => p.call("Page.reload", json!({})).await.map(|_| ()),
            Self::Driven(d) => d.reload().await,
        }
    }

    /// Back (`-1`) or forward (`1`).
    async fn history(&self, delta: i64) -> anyhow::Result<()> {
        match self {
            Self::Tab(p) => {
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

    async fn viewport(&self, width: u32, height: u32, scale: f64, mobile: bool) -> anyhow::Result<()> {
        match self {
            Self::Tab(p) => p.viewport(width, height, scale, mobile).await,
            Self::Driven(d) => d.viewport(width, height, scale).await,
        }
    }

    async fn settle(&self) {
        match self {
            Self::Tab(p) => {
                let _ = p.settle().await;
            }
            Self::Driven(_) => crate::rt::sleep(Duration::from_millis(150)).await,
        }
    }

    /// A picture of it (PNG).
    async fn picture(&self) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::Tab(p) => p.picture(false).await,
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

    /// Open a stream of `spec` as `id`; its chunks and events go to `out`.
    pub async fn open(&self, dir: &Path, id: &str, spec: &Spec, out: async_channel::Sender<Out>) -> anyhow::Result<Value> {
        let engine = super::driven::Engine::parse(spec.engine.as_deref().unwrap_or(""))?;
        let device = match spec.device.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            Some(n) => Some(super::devices::device(n).ok_or_else(|| anyhow::anyhow!("kiln knows no device {n:?}: {}", super::devices::DEVICES.iter().map(|d| d.name).collect::<Vec<_>>().join(", ")))?),
            None => None,
        };
        // Its window: a device's, else its viewer's.
        let (width, height, scale, mobile) = device.map_or((spec.width, spec.height, spec.scale.clamp(1.0, 3.0), spec.mobile), |d| (d.width, d.height, d.scale, d.mobile));
        if engine != super::driven::Engine::Chromium {
            return self.open_driven(dir, id, spec, engine, device, (width, height, scale, mobile), out).await;
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
        anyhow::ensure!(tab.is_number(), "the capture extension doesn't see the stream's tab");
        s.outs.lock().insert(id.to_owned(), out.clone());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (pw, ph) = ((f64::from(width) * scale).round() as u32, (f64::from(height) * scale).round() as u32);
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
        let stream = Arc::new(Stream::new(id, spec, s.clone(), Surface::Tab(page), device, inputs, out.clone(), dir, (width, height, scale, mobile)));
        crate::rt::spawn(watch(stream.clone(), out.clone()));
        crate::rt::spawn(apply(Arc::downgrade(&stream), queued));
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
    async fn open_driven(&self, dir: &Path, id: &str, spec: &Spec, engine: super::driven::Engine, device: Option<super::devices::Device>, window: (u32, u32, f64, bool), out: async_channel::Sender<Out>) -> anyhow::Result<Value> {
        let (width, height, scale, mobile) = window;
        let enc = self.streamer(dir, "encode").await?;
        let page = super::driven::Driven::open(dir, engine, &spec.profile, width, height, scale, device.as_ref()).await?;
        if let Some(agent) = device.map(|d| d.agent).filter(|a| !a.is_empty()) {
            page.agent(agent).await;
        }
        enc.outs.lock().insert(id.to_owned(), out.clone());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (pw, ph) = ((f64::from(width) * scale).round() as u32, (f64::from(height) * scale).round() as u32);
        let opts = json!({"width": pw, "height": ph, "framerate": 30, "bitrate": spec.bitrate.unwrap_or(4_000_000), "codec": spec.codec.clone().unwrap_or_else(|| "h264".to_owned())});
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
        let stream = Arc::new(Stream::new(id, spec, enc, Surface::Driven(page), device, inputs, out.clone(), dir, window));
        crate::rt::spawn(said(stream.clone(), out.clone()));
        crate::rt::spawn(pump(Arc::downgrade(&stream)));
        crate::rt::spawn(apply(Arc::downgrade(&stream), queued));
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
        // Its tab, whoever else still holds the stream (its watcher ends as the tab does).
        match &s.page {
            Surface::Tab(p) => {
                let _ = cdp.call(None, "Target.closeTarget", json!({"targetId": p.target()})).await;
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
        "go" | "back" | "forward" | "reload" => {
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
  window.__kilnFocus = true;
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
})();";

/// What's selected in the page, to copy: a field's selection (never a password's), else the
/// document's.
const SELECTED: &str = r"(() => { const e = document.activeElement; if (e && typeof e.selectionStart === 'number' && (e.tagName === 'INPUT' || e.tagName === 'TEXTAREA')) return e.type === 'password' ? '' : e.value.slice(e.selectionStart, e.selectionEnd); return String(getSelection() || ''); })()";

/// What viewers do, applied in the order they did it (and logged, when it's recording).
async fn apply(s: std::sync::Weak<Stream>, queued: async_channel::Receiver<Vec<Input>>) {
    while let Ok(events) = queued.recv().await {
        let Some(s) = s.upgrade() else { return };
        for e in &events {
            s.log(json!({"by": "person", "input": e}));
        }
        if let Err(e) = s.input(events).await {
            tracing::debug!(stream = %s.id, error = %format!("{e:#}"), "a viewer's input didn't reach the page");
        }
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

/// What the tab does that its viewer's side shows: where it is (its address and title), a field
/// taking text or not, a dialog (dismissed), its end.
async fn watch(s: Arc<Stream>, out: async_channel::Sender<Out>) {
    let Surface::Tab(page) = &s.page else { return };
    let events = page.listen();
    while let Ok(e) = events.recv().await {
        let ev = match e.method.as_str() {
            "Page.frameNavigated" if e.params.get("frame").and_then(|f| f.get("parentId")).is_none() => {
                let url = e.params.get("frame").and_then(|f| f.get("url")).and_then(Value::as_str).unwrap_or("");
                s.log(json!({"navigated": url}));
                Some(json!({"navigated": {"url": url}}))
            }
            "Page.loadEventFired" => {
                let title = page.eval("document.title").await.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
                Some(json!({"loaded": {"title": title}}))
            }
            // `{"focus": ...}`, `{"fields": [...]}` or `{"selected": bool}`, as the page said it.
            "Runtime.bindingCalled" if e.params.get("name").and_then(Value::as_str) == Some("kilnFocus") => serde_json::from_str::<Value>(e.params.get("payload").and_then(Value::as_str).unwrap_or("{}"))
                .ok()
                .filter(|v| v.as_object().is_some_and(|o| o.len() == 1 && ["focus", "fields", "selected"].iter().any(|k| o.contains_key(*k)))),
            // A page's alert, confirm or prompt would hold it still: answered, and said.
            "Page.javascriptDialogOpening" => {
                let _ = page.call("Page.handleJavaScriptDialog", json!({"accept": true})).await;
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
    fn new(id: &str, spec: &Spec, streamer: Arc<Streamer>, page: Surface, device: Option<super::devices::Device>, inputs: async_channel::Sender<Vec<Input>>, out: async_channel::Sender<Out>, dir: &Path, window: (u32, u32, f64, bool)) -> Self {
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
            Surface::Tab(p) => p,
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
    /// `{"back": true}`, `{"forward": true}`.
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
            // A tab's capture takes its new size; another engine's pictures come at it.
            if matches!(self.page, Surface::Tab(_)) {
                super::evaluate(cdp, &off, &format!("kiln.resize({id}, {pw}, {ph})")).await?;
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
            let opened = streams().open(&dir, "test-firefox", &spec, tx).await?;
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
            streams().open(&dir, "test-late", &spec, tx).await?;
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
    /// with Xcode, an iOS runtime and Safari's remote automation on (`safaridriver --enable`): a
    /// page from this Mac streams from it, laid out as the phone lays it out, and a tap on it is a
    /// touch where it was.
    #[test]
    #[ignore = "needs a Mac with Xcode, an iOS simulator runtime and `safaridriver --enable` (it boots a simulator)"]
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
            let opened = streams().open(&dir, "test-iphone", &spec, tx).await?;
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
    /// does here): its pictures (PNG) as it changes, a click reaching it, where it went, said.
    #[test]
    #[ignore = "needs KILN_WEBDRIVER (a WebDriver), and downloads and starts the real browser (about 200 MB)"]
    fn webdriver_streams_a_page() -> anyhow::Result<()> {
        if std::env::var_os("KILN_WEBDRIVER").is_none() {
            return Ok(());
        }
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        crate::rt::block_on(async move {
            let page = "data:text/html,<title>driven</title><body style='margin:0;background:%23123'><div id=b style='width:40px;height:40px;background:%23bab0e3;position:absolute'></div><script>let t=0;addEventListener('pointerdown',e=>document.title='clicked '+e.clientX+','+e.clientY);setInterval(()=>{t++;b.style.left=(t*7%25200)+'px'},80)</script></body>";
            let spec: Spec = serde_json::from_value(json!({"url": page, "width": 400, "height": 300, "scale": 1, "profile": "clean", "engine": "webkit"}))?;
            let (tx, rx) = async_channel::bounded::<Out>(4096);
            let opened = streams().open(&dir, "test-webdriver", &spec, tx).await?;
            assert_eq!(opened.pointer("/opened/engine"), Some(&json!("webkit")), "{opened}");
            let s = streams().get("test-webdriver").ok_or_else(|| anyhow::anyhow!("not open"))?;
            s.control(&json!({"pause": false, "keyframe": true})).await?;
            let (mut keys, mut frames, mut said) = (0, 0, Vec::new());
            let end = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < end {
                match crate::rt::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Ok(Out::Frame { data, .. })) => match (data.first(), data.get(1)) {
                        (Some(b'v'), Some(b'k')) => keys += 1,
                        (Some(b'v'), _) => frames += 1,
                        _ => {}
                    },
                    Ok(Ok(Out::Event { json, .. })) => said.push(json),
                    _ => {}
                }
            }
            let window = s.page.eval("[innerWidth, innerHeight]").await?;
            s.input(vec![Input::Pointer { k: "down".into(), x: 120.0, y: 90.0, button: 0, buttons: 1, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }, Input::Pointer { k: "up".into(), x: 120.0, y: 90.0, button: 0, buttons: 0, pt: "mouse".into(), id: 1, mods: 0, clicks: 1 }]).await?;
            crate::rt::sleep(Duration::from_millis(300)).await;
            let title = s.page.eval("document.title").await?;
            streams().close("test-webdriver").await;
            crate::rt::sleep(Duration::from_millis(500)).await;
            super::super::driven::stop_all().await;
            assert!(keys >= 1 && frames >= 5, "key frames {keys}, frames {frames}");
            assert_eq!(window, json!([400, 300]), "its window, as asked");
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
