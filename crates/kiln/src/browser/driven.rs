//! Pages of engines other than Chromium, for streams (docs/previews.md, "Engines"): Firefox, the
//! machine's own, over its WebDriver BiDi; WebKit, as Safari on a Mac (`safaridriver`) or WebKitGTK
//! elsewhere (`WebKitWebDriver`, when it's installed), over WebDriver. Neither has a capture a page
//! can be streamed from, so a stream takes the page's picture as it changes ([`Driven::shot`]: its
//! bytes the same when it hasn't) and kiln's capture extension encodes it as it does a captured
//! tab's (`super::stream`). What its viewers do goes to it as WebDriver actions; what it does
//! (where it goes, its title, a field taking text, a dialog) comes back as a Chromium tab's does.
//! They have no sound here.
//!
//! Firefox runs one browser a profile (a conversation's, a person's own, or a clean one each time),
//! each stream a tab of its; Safari takes one automated window at a time, and keeps nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::StreamExt as _;
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::stream::Input;

/// Which engine a stream's page runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Chromium,
    Firefox,
    Webkit,
}

impl Engine {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "" | "chromium" | "chrome" => Self::Chromium,
            "firefox" | "gecko" => Self::Firefox,
            "webkit" | "safari" => Self::Webkit,
            other => anyhow::bail!("a live page runs in chromium, firefox or webkit, not {other:?}"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Chromium => "chromium",
            Self::Firefox => "firefox",
            Self::Webkit => "webkit",
        }
    }
}

/// How long a browser with no pages stays up (a stream opened again soon finds it running).
const IDLE: Duration = Duration::from_secs(600);
/// The most a WebDriver answer may be (a picture of a big window).
const ANSWER: usize = 64 << 20;

// ------------------------------------------------------------------------------ Firefox's BiDi

/// A WebDriver BiDi connection: commands by number, events to the pages they're about.
struct Bidi {
    out: async_channel::Sender<String>,
    waiting: Mutex<HashMap<u64, futures::channel::oneshot::Sender<Result<Value, String>>>>,
    next: AtomicU64,
    /// Each page's events, by its context.
    pages: Mutex<HashMap<String, async_channel::Sender<(String, Value)>>>,
}

impl Bidi {
    async fn connect(url: &str) -> anyhow::Result<Arc<Self>> {
        let ws = crate::http::client::Client::public()?.websocket(url, ANSWER).await?;
        let (mut tx, mut rx) = ws.split();
        let (out, outgoing) = async_channel::unbounded::<String>();
        let bidi = Arc::new(Self { out, waiting: Mutex::default(), next: AtomicU64::new(0), pages: Mutex::default() });
        crate::rt::spawn(async move {
            while let Ok(m) = outgoing.recv().await {
                if tx.send(crate::http::ws::text(m)).await.is_err() {
                    break;
                }
            }
        });
        let weak = Arc::downgrade(&bidi);
        crate::rt::spawn(async move {
            while let Some(Ok(m)) = rx.next().await {
                let crate::http::ws::Message::Text(t) = m else { continue };
                let Some(b) = weak.upgrade() else { return };
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = b.waiting.lock().remove(&id) {
                        let r = match v.get("type").and_then(Value::as_str) {
                            Some("error") => Err(format!("{}: {}", v.get("error").and_then(Value::as_str).unwrap_or("error"), v.get("message").and_then(Value::as_str).unwrap_or(""))),
                            _ => Ok(v.get("result").cloned().unwrap_or_default()),
                        };
                        let _ = tx.send(r);
                    }
                } else if let (Some(method), Some(params)) = (v.get("method").and_then(Value::as_str), v.get("params")) {
                    let context = params.get("context").or_else(|| params.pointer("/source/context")).and_then(Value::as_str).unwrap_or("");
                    if let Some(page) = b.pages.lock().get(context) {
                        let _ = page.try_send((method.to_owned(), params.clone()));
                    }
                }
            }
            // The connection's gone: whatever waits, waits no more; its pages end.
            if let Some(b) = weak.upgrade() {
                b.waiting.lock().clear();
                for (_, p) in b.pages.lock().drain() {
                    let _ = p.try_send(("closed".to_owned(), json!({})));
                }
            }
        });
        Ok(bidi)
    }

    async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.waiting.lock().insert(id, tx);
        self.out.send(json!({"id": id, "method": method, "params": params}).to_string()).await.map_err(|_| anyhow::anyhow!("the browser's connection is gone"))?;
        match crate::rt::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => anyhow::bail!("{method}: {e}"),
            Ok(Err(_)) => anyhow::bail!("the browser's connection is gone"),
            Err(_) => {
                self.waiting.lock().remove(&id);
                anyhow::bail!("{method} took too long")
            }
        }
    }
}

/// A BiDi remote value as plain JSON.
fn plain(v: &Value) -> Value {
    let inner = v.get("value");
    match v.get("type").and_then(Value::as_str) {
        Some("string" | "boolean") => inner.cloned().unwrap_or_default(),
        Some("number") => inner.filter(|n| n.is_number()).cloned().unwrap_or_default(),
        Some("array") => Value::Array(inner.and_then(Value::as_array).map(|a| a.iter().map(plain).collect()).unwrap_or_default()),
        Some("object") => {
            let mut m = serde_json::Map::new();
            for pair in inner.and_then(Value::as_array).into_iter().flatten() {
                if let (Some(k), Some(v)) = (pair.get(0).and_then(Value::as_str), pair.get(1)) {
                    m.insert(k.to_owned(), plain(v));
                }
            }
            Value::Object(m)
        }
        _ => Value::Null,
    }
}

// ------------------------------------------------------------------------------ browsers

/// A process just started: stopped (its whole group) unless it's taken.
struct Started(Option<std::process::Child>);

impl Started {
    fn take(&mut self) -> Option<std::process::Child> {
        self.0.take()
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            stop_process(p);
        }
    }
}

/// A process and its group, stopped.
fn stop_process(mut p: std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(p.id()) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
    }
    let _ = p.kill();
    let _ = p.wait();
}

/// A browser of another engine's, running: Firefox and its connection, or a WebDriver (and the
/// one window it drives).
pub struct Remote {
    conn: Conn,
    /// Firefox's process, or the WebDriver's (each in a process group of its own).
    process: Mutex<Option<std::process::Child>>,
    /// A clean profile's folder, removed as it stops.
    scratch: Option<PathBuf>,
    pages: AtomicUsize,
}

enum Conn {
    Bidi(Arc<Bidi>),
    /// `http://127.0.0.1:<port>/session/<id>`.
    Classic(String),
}

impl Remote {
    fn stop(&self) {
        if let Some(p) = self.process.lock().take() {
            stop_process(p);
        }
        if let Some(d) = &self.scratch {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stop();
    }
}

fn remotes() -> &'static futures::lock::Mutex<HashMap<String, Arc<Remote>>> {
    static R: OnceLock<futures::lock::Mutex<HashMap<String, Arc<Remote>>>> = OnceLock::new();
    R.get_or_init(futures::lock::Mutex::default)
}

/// Every browser of another engine's, stopped (the runner is going; a test is done).
pub async fn stop_all() {
    let all: Vec<Arc<Remote>> = remotes().lock().await.drain().map(|(_, r)| r).collect();
    for r in all {
        r.stop();
    }
}

/// What a Firefox kiln starts leaves in its profile: whose it is (the runner's process) and its own
/// process, so a runner that went without stopping it (a crash, a restart) has it stopped later.
const OWNER: &str = "kiln.pid";

/// Firefoxes a runner that's gone left running, stopped, and their clean profiles removed (once a
/// process, and before a kept profile is used again).
fn reap(dir: &Path, only: Option<&Path>) {
    let root = dir.join("browser").join("firefox");
    let folders: Vec<PathBuf> = match only {
        Some(f) => vec![f.to_path_buf()],
        None => std::fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path()).collect(),
    };
    for f in folders {
        let Ok(text) = std::fs::read_to_string(f.join(OWNER)) else { continue };
        let mut it = text.split_whitespace().filter_map(|n| n.parse::<u32>().ok());
        let (Some(runner), Some(firefox)) = (it.next(), it.next()) else { continue };
        if runner == std::process::id() || super::alive(runner) {
            continue;
        }
        // Still that Firefox (its command names this profile), not a process since given its id.
        #[cfg(target_os = "linux")]
        let ours = std::fs::read(format!("/proc/{firefox}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains(&*f.to_string_lossy()));
        #[cfg(not(target_os = "linux"))]
        let ours = super::alive(firefox);
        #[cfg(unix)]
        if ours && let Ok(pid) = i32::try_from(firefox) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
            tracing::info!(profile = %f.display(), "stopped a Firefox a runner before this one left running");
        }
        #[cfg(not(unix))]
        let _ = ours;
        let _ = std::fs::remove_file(f.join(OWNER));
        if f.file_name().is_some_and(|n| n.to_string_lossy().starts_with("clean-")) {
            let _ = std::fs::remove_dir_all(&f);
        }
    }
}

/// The engine's browser for `profile`, started when it isn't running.
async fn remote(dir: &Path, engine: Engine, profile: &str, sim: Option<&super::simulators::Sim>) -> anyhow::Result<Arc<Remote>> {
    static REAPED: OnceLock<()> = OnceLock::new();
    if REAPED.set(()).is_ok() {
        let d = dir.to_path_buf();
        let _ = crate::rt::blocking(move || reap(&d, None)).await;
    }
    // Safari keeps nothing, and takes one automated window: one for every profile.
    let key = match (engine, sim) {
        (Engine::Webkit, Some(s)) => format!("webkit:sim:{}", s.udid),
        (Engine::Webkit, None) => "webkit".to_owned(),
        _ if profile == "clean" => format!("{}:clean:{}", engine.name(), hex::encode(rand::random::<[u8; 6]>())),
        _ => format!("{}:{profile}", engine.name()),
    };
    let mut g = remotes().lock().await;
    if let Some(r) = g.get(&key)
        && r.process.lock().as_mut().is_some_and(|p| matches!(p.try_wait(), Ok(None)))
    {
        return Ok(r.clone());
    }
    let r = Arc::new(match engine {
        Engine::Firefox => start_firefox(dir, profile).await?,
        Engine::Webkit => start_webdriver(sim).await?,
        Engine::Chromium => anyhow::bail!("Chromium's pages are the capture extension's"),
    });
    g.insert(key, r.clone());
    Ok(r)
}

/// The machine's Firefox (`KILN_FIREFOX` says where, when it's somewhere else).
fn firefox() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KILN_FIREFOX").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from("/Applications/Firefox.app/Contents/MacOS/firefox"));
        if let Some(h) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(h).join("Applications/Firefox.app/Contents/MacOS/firefox"));
        }
    }
    #[cfg(windows)]
    for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(b) = std::env::var_os(base) {
            candidates.push(PathBuf::from(b).join("Mozilla Firefox").join("firefox.exe"));
        }
    }
    let exe = if cfg!(windows) { "firefox.exe" } else { "firefox" };
    for d in std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default() {
        candidates.push(d.join(exe));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// What a profile of Firefox's starts with: no first run, no updates or reports, touch events for
/// touches, sound and video let play.
const PREFS: &str = r#"user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.aboutwelcome.enabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("app.update.disabledForTesting", true);
user_pref("browser.tabs.warnOnClose", false);
user_pref("dom.w3c_touch_events.enabled", 1);
user_pref("media.autoplay.default", 0);
"#;

async fn start_firefox(dir: &Path, profile: &str) -> anyhow::Result<Remote> {
    let bin = firefox().ok_or_else(|| anyhow::anyhow!("Firefox isn't installed on this machine: install it to stream pages in it"))?;
    let key: String = profile.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let clean = profile == "clean";
    let folder = dir.join("browser").join("firefox").join(if clean { format!("clean-{}-{}", std::process::id(), hex::encode(rand::random::<[u8; 4]>())) } else { key });
    std::fs::create_dir_all(&folder)?;
    // A kept profile a runner before this one left its Firefox holding is let go first.
    reap(dir, Some(&folder));
    std::fs::write(folder.join("user.js"), PREFS)?;
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["--headless", "--no-remote", "--profile"]).arg(&folder).arg("--remote-debugging-port=0");
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("{} didn't start: {e}", bin.display()))?;
    // It says where its BiDi listens, then goes on saying things (read on, so it never blocks).
    let stderr = child.stderr.take();
    let _ = std::fs::write(folder.join(OWNER), format!("{} {}", std::process::id(), child.id()));
    let mut started = Started(Some(child));
    let stderr = stderr.ok_or_else(|| anyhow::anyhow!("Firefox's output isn't there"))?;
    let (tx, rx) = async_channel::bounded::<String>(1);
    std::thread::Builder::new().name("kiln-firefox-log".into()).spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(at) = line.find("ws://") {
                let _ = tx.try_send(line.get(at..).unwrap_or("").trim().to_owned());
            }
        }
    })?;
    let url = match crate::rt::timeout(Duration::from_secs(30), rx.recv()).await {
        Ok(Ok(u)) => u,
        _ => anyhow::bail!("Firefox didn't say where its WebDriver BiDi listens"),
    };
    let bidi = Bidi::connect(&format!("{}/session", url.trim_end_matches('/'))).await?;
    bidi.call("session.new", json!({"capabilities": {"alwaysMatch": {"acceptInsecureCerts": true, "unhandledPromptBehavior": {"default": "ignore"}}}})).await?;
    // What its pages do (a Firefox from before `historyUpdated`, all but that).
    let wanted = ["browsingContext.load", "browsingContext.fragmentNavigated", "browsingContext.userPromptOpened", "browsingContext.contextDestroyed", "script.message"];
    let all: Vec<&str> = wanted.iter().copied().chain(std::iter::once("browsingContext.historyUpdated")).collect();
    if bidi.call("session.subscribe", json!({"events": all})).await.is_err() {
        bidi.call("session.subscribe", json!({"events": wanted})).await?;
    }
    Ok(Remote { conn: Conn::Bidi(bidi), process: Mutex::new(started.take()), scratch: clean.then_some(folder), pages: AtomicUsize::new(0) })
}

/// The engines this machine has, besides Chromium: whether it has Firefox, and a WebDriver for
/// WebKit.
pub fn available() -> Value {
    json!({"chromium": true, "firefox": firefox().is_some(), "webkit": webdriver().is_some(), "ios": ios()})
}

/// Whether this Mac can run iOS simulators (Xcode, an iOS runtime): asked of `simctl` at most once
/// a minute.
fn ios() -> bool {
    static SEEN: OnceLock<Mutex<Option<(std::time::Instant, bool)>>> = OnceLock::new();
    if !cfg!(target_os = "macos") {
        return false;
    }
    let seen = SEEN.get_or_init(Mutex::default);
    if let Some((at, ok)) = *seen.lock()
        && at.elapsed() < Duration::from_secs(60)
    {
        return ok;
    }
    let ok = std::process::Command::new("xcrun")
        .args(["simctl", "list", "-j", "runtimes"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .is_some_and(|v| super::simulators::has_runtime(&v));
    *seen.lock() = Some((std::time::Instant::now(), ok));
    ok
}

/// Safari's WebDriver on a Mac, WebKitGTK's elsewhere (`KILN_WEBDRIVER` says another's: one that
/// takes `--port=`, started with `KILN_WEBDRIVER_CAPS`' capabilities).
fn webdriver() -> Option<(PathBuf, bool)> {
    if let Some(p) = std::env::var_os("KILN_WEBDRIVER").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some((p, false));
    }
    if cfg!(target_os = "macos") {
        let p = PathBuf::from("/usr/bin/safaridriver");
        return p.is_file().then_some((p, true));
    }
    let mut candidates = vec![PathBuf::from("/usr/bin/WebKitWebDriver"), PathBuf::from("/usr/libexec/webkit2gtk-4.1/WebKitWebDriver"), PathBuf::from("/usr/lib/webkit2gtk-4.1/WebKitWebDriver")];
    for d in std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default() {
        candidates.push(d.join("WebKitWebDriver"));
    }
    candidates.into_iter().find(|p| p.is_file()).map(|p| (p, false))
}

async fn start_webdriver(sim: Option<&super::simulators::Sim>) -> anyhow::Result<Remote> {
    let Some((bin, safari)) = webdriver() else {
        anyhow::bail!(
            "{}",
            if cfg!(target_os = "macos") { "this Mac has no safaridriver" } else { "WebKit's WebDriver isn't installed on this machine (Debian and Ubuntu: apt install webkit2gtk-driver)" }
        );
    };
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let mut cmd = std::process::Command::new(&bin);
    if safari {
        cmd.arg("-p").arg(port.to_string());
    } else {
        cmd.arg(format!("--port={port}"));
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut started = Started(Some(cmd.spawn().map_err(|e| anyhow::anyhow!("{} didn't start: {e}", bin.display()))?));
    let root = format!("http://127.0.0.1:{port}");
    let mut up = false;
    for _ in 0..50 {
        if http(&format!("{root}/status"), None).await.is_ok() {
            up = true;
            break;
        }
        crate::rt::sleep(Duration::from_millis(100)).await;
    }
    anyhow::ensure!(up, "{} didn't answer", bin.display());
    let asked = std::env::var("KILN_WEBDRIVER_CAPS").ok().and_then(|c| serde_json::from_str::<Value>(&c).ok());
    let caps = match (asked, sim) {
        (Some(c), _) => json!({"capabilities": {"alwaysMatch": c}}),
        // An iPhone's or iPad's Mobile Safari, in kiln's simulator of it.
        (None, Some(s)) => json!({"capabilities": {"alwaysMatch": {"platformName": "iOS", "browserName": "Safari", "safari:useSimulator": true, "safari:deviceUDID": s.udid}}}),
        (None, None) if safari => json!({"capabilities": {"alwaysMatch": {"browserName": "safari"}}}),
        (None, None) => json!({"capabilities": {"alwaysMatch": {}}}),
    };
    let made = http(&format!("{root}/session"), Some(caps)).await.map_err(|e| {
        if safari && format!("{e:#}").to_ascii_lowercase().contains("remote automation") {
            anyhow::anyhow!("Safari's remote automation is off on this Mac: run `safaridriver --enable` there once (it asks for an administrator's password)")
        } else {
            e
        }
    })?;
    let id = made.pointer("/value/sessionId").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("the WebDriver started no session: {made}"))?;
    Ok(Remote { conn: Conn::Classic(format!("{root}/session/{id}")), process: Mutex::new(started.take()), scratch: None, pages: AtomicUsize::new(0) })
}

/// A WebDriver HTTP command: its answer's JSON (an error's, as an error).
async fn http(url: &str, body: Option<Value>) -> anyhow::Result<Value> {
    let c = crate::http::client::Client::public()?;
    let req = match body {
        Some(b) if b.is_null() => c.request(crate::http::client::Method::Delete, url),
        Some(b) => c.post(url).json(&b),
        None => c.get(url),
    };
    let resp = req.timeout(Duration::from_secs(60)).send().await?;
    let ok = resp.is_success();
    let v: Value = resp.json(ANSWER).await.unwrap_or_default();
    if !ok {
        anyhow::bail!("{}: {}", v.pointer("/value/error").and_then(Value::as_str).unwrap_or("error"), v.pointer("/value/message").and_then(Value::as_str).unwrap_or(""));
    }
    Ok(v)
}

// ------------------------------------------------------------------------------ a page

/// A page of another engine's that a stream drives.
pub struct Driven {
    pub engine: Engine,
    remote: Arc<Remote>,
    /// Its BiDi context (a tab); a WebDriver session's one window has none.
    context: String,
    /// What the page did (`{"navigated"}`, `{"loaded"}`, `{"focus"}`, `{"dialog"}`, `{"closed"}`).
    pub said: async_channel::Receiver<Value>,
    said_tx: async_channel::Sender<Value>,
    /// An iOS simulator's: touched, never moused, its window its device's.
    pub simulated: bool,
}

/// Tells kiln what a field taking text is, where its fields are, whether text is selected
/// (`super::stream`'s FOCUS), through `say`.
fn focus_script() -> String {
    let src = super::stream::FOCUS_SOURCE.replace("kilnFocus(", "__kilnSay(");
    format!("(say) => {{ globalThis.__kilnSay = (s) => say(s); {src} }}")
}

impl Driven {
    /// A page of `engine`'s, in `profile`, at `width` by `height` (CSS pixels, `scale` density).
    pub async fn open(dir: &Path, engine: Engine, profile: &str, width: u32, height: u32, scale: f64, device: Option<&super::devices::Device>) -> anyhow::Result<Self> {
        // An iPhone or iPad in WebKit, on a Mac: its simulator's own Mobile Safari.
        let sim = match device.filter(|d| engine == Engine::Webkit && cfg!(target_os = "macos") && super::simulators::is_ios(d)) {
            Some(d) => Some(super::simulators::ensure(d).await?),
            None => None,
        };
        let remote = remote(dir, engine, profile, sim.as_ref()).await?;
        let (said_tx, said) = async_channel::bounded(256);
        let context = match &remote.conn {
            Conn::Bidi(b) => {
                let made = b.call("browsingContext.create", json!({"type": "tab"})).await?;
                let ctx = made.get("context").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Firefox made no tab"))?.to_owned();
                let (tx, rx) = async_channel::bounded::<(String, Value)>(256);
                b.pages.lock().insert(ctx.clone(), tx);
                b.call("script.addPreloadScript", json!({"functionDeclaration": focus_script(), "arguments": [{"type": "channel", "value": {"channel": "kiln"}}], "contexts": [ctx]})).await?;
                crate::rt::spawn(events(Arc::downgrade(b), ctx.clone(), rx, said_tx.clone()));
                ctx
            }
            Conn::Classic(base) => {
                anyhow::ensure!(remote.pages.load(Ordering::Relaxed) == 0, "Safari takes one automated window at a time: close the other WebKit stream first");
                crate::rt::spawn(poll(base.clone(), Arc::downgrade(&remote), said_tx.clone()));
                String::new()
            }
        };
        remote.pages.fetch_add(1, Ordering::Relaxed);
        let d = Self { engine, remote, context, said, said_tx, simulated: sim.is_some() };
        // A simulator's window is its device's own.
        if !d.simulated {
            d.viewport(width, height, scale).await?;
        }
        Ok(d)
    }

    fn bidi(&self) -> Option<&Arc<Bidi>> {
        match &self.remote.conn {
            Conn::Bidi(b) => Some(b),
            Conn::Classic(_) => None,
        }
    }

    fn base(&self) -> &str {
        match &self.remote.conn {
            Conn::Classic(b) => b,
            Conn::Bidi(_) => "",
        }
    }

    pub async fn navigate(&self, url: &str) -> anyhow::Result<()> {
        match self.bidi() {
            Some(b) => b.call("browsingContext.navigate", json!({"context": self.context, "url": url, "wait": "none"})).await.map(|_| ()),
            None => http(&format!("{}/url", self.base()), Some(json!({"url": url}))).await.map(|_| ()),
        }
    }

    pub async fn reload(&self) -> anyhow::Result<()> {
        match self.bidi() {
            Some(b) => b.call("browsingContext.reload", json!({"context": self.context, "wait": "none"})).await.map(|_| ()),
            None => http(&format!("{}/refresh", self.base()), Some(json!({}))).await.map(|_| ()),
        }
    }

    /// Back (`-1`) or forward (`1`) in its history.
    pub async fn history(&self, delta: i64) -> anyhow::Result<()> {
        match self.bidi() {
            Some(b) => b.call("browsingContext.traverseHistory", json!({"context": self.context, "delta": delta})).await.map(|_| ()),
            None => http(&format!("{}/{}", self.base(), if delta < 0 { "back" } else { "forward" }), Some(json!({}))).await.map(|_| ()),
        }
    }

    /// Its window, `width` by `height` CSS pixels at `scale` (Safari's density is its screen's).
    pub async fn viewport(&self, width: u32, height: u32, scale: f64) -> anyhow::Result<()> {
        match self.bidi() {
            Some(b) => b.call("browsingContext.setViewport", json!({"context": self.context, "viewport": {"width": width, "height": height}, "devicePixelRatio": scale})).await.map(|_| ()),
            None => {
                // The window's own edges, then a window that leaves the page that much.
                let edges = self.eval("[Math.max(0, Math.round(outerWidth - innerWidth)), Math.max(0, Math.round(outerHeight - innerHeight))]").await.unwrap_or_default();
                let n = |i: usize| edges.get(i).and_then(Value::as_u64).and_then(|e| u32::try_from(e).ok()).unwrap_or(0);
                // Whole pixels: WebDriver takes no others.
                http(&format!("{}/window/rect", self.base()), Some(json!({"width": width + n(0), "height": height + n(1)}))).await.map(|_| ())
            }
        }
    }

    /// What it says it is (a device's user agent): where the engine lets kiln say so.
    pub async fn agent(&self, ua: &str) {
        if let Some(b) = self.bidi() {
            let _ = b.call("emulation.setUserAgentOverride", json!({"userAgent": ua, "contexts": [self.context]})).await;
        }
    }

    /// Its picture: JPEG from Firefox (`png` asks for PNG), PNG from WebDriver; the same bytes
    /// while it hasn't changed.
    pub async fn shot(&self, png: bool) -> anyhow::Result<(Vec<u8>, &'static str)> {
        use base64::Engine as _;
        let (b64, mime) = match self.bidi() {
            Some(b) => {
                let format = if png { json!({"type": "image/png"}) } else { json!({"type": "image/jpeg", "quality": 0.85}) };
                let r = b.call("browsingContext.captureScreenshot", json!({"context": self.context, "origin": "viewport", "format": format})).await?;
                (r.get("data").and_then(Value::as_str).unwrap_or("").to_owned(), if png { "image/png" } else { "image/jpeg" })
            }
            None => (http(&format!("{}/screenshot", self.base()), None).await?.get("value").and_then(Value::as_str).unwrap_or("").to_owned(), "image/png"),
        };
        Ok((base64::engine::general_purpose::STANDARD.decode(b64)?, mime))
    }

    /// `js` (an expression), evaluated in the page: its value as JSON.
    pub async fn eval(&self, js: &str) -> anyhow::Result<Value> {
        match self.bidi() {
            Some(b) => {
                let r = b.call("script.evaluate", json!({"expression": js, "target": {"context": self.context}, "awaitPromise": true, "resultOwnership": "none"})).await?;
                if r.get("type").and_then(Value::as_str) == Some("exception") {
                    anyhow::bail!("{}", r.pointer("/exceptionDetails/text").and_then(Value::as_str).unwrap_or("it threw"));
                }
                Ok(r.get("result").map(plain).unwrap_or_default())
            }
            None => Ok(http(&format!("{}/execute/sync", self.base()), Some(json!({"script": format!("return ({js});"), "args": []}))).await?.get("value").cloned().unwrap_or_default()),
        }
    }

    /// What a viewer (or the agent) did, as WebDriver actions: each run of one kind of input in a
    /// call of its own, in order.
    pub async fn input(&self, events: &[Input]) -> anyhow::Result<()> {
        // A phone has no mouse: what a pointer does there, a finger does.
        let touched: Vec<Input>;
        let events = if self.simulated {
            touched = events.iter().cloned().map(|e| match e {
                Input::Pointer { k, x, y, button, buttons, pt: _, id, mods, clicks } => Input::Pointer { k, x, y, button, buttons, pt: "touch".to_owned(), id, mods, clicks },
                other => other,
            }).collect();
            &touched[..]
        } else {
            events
        };
        for (source, actions) in action_runs(events) {
            if source == "wheel" && self.bidi().is_none() {
                // Not every WebDriver has the wheel: the page scrolls itself where it was turned.
                for a in &actions {
                    let n = |k: &str| a.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                    self.eval(&format!("(e => (e && e.scrollBy ? e : window).scrollBy({}, {}))(document.elementFromPoint({}, {}))", n("deltaX"), n("deltaY"), n("x"), n("y"))).await?;
                }
                continue;
            }
            let src = sources(&source, actions);
            match self.bidi() {
                Some(b) => b.call("input.performActions", json!({"context": self.context, "actions": [src]})).await.map(|_| ())?,
                None => http(&format!("{}/actions", self.base()), Some(json!({"actions": [src]}))).await.map(|_| ())?,
            }
        }
        Ok(())
    }

    /// Done with it: its tab closes; its browser stops a while after its last page (a clean
    /// profile's at once).
    pub async fn close(&self) {
        match &self.remote.conn {
            Conn::Bidi(b) => {
                b.pages.lock().remove(&self.context);
                let _ = b.call("browsingContext.close", json!({"context": self.context})).await;
            }
            Conn::Classic(_) => {}
        }
        let _ = self.said_tx.try_send(json!({"closed": {"why": "it was closed"}}));
        if self.remote.pages.fetch_sub(1, Ordering::Relaxed) > 1 {
            return;
        }
        let r = self.remote.clone();
        // A clean profile's browser, and Safari's window, go now (one opened next finds neither);
        // a kept profile's a while later, unless a page opens in it meanwhile.
        if r.scratch.is_some() || matches!(r.conn, Conn::Classic(_)) {
            retire(&r).await;
            return;
        }
        crate::rt::spawn(async move {
            crate::rt::sleep(IDLE).await;
            if r.pages.load(Ordering::Relaxed) == 0 {
                retire(&r).await;
            }
        });
    }
}

/// A browser with no pages left, stopped and forgotten.
async fn retire(r: &Arc<Remote>) {
    remotes().lock().await.retain(|_, x| !Arc::ptr_eq(x, r));
    if let Conn::Classic(base) = &r.conn {
        let _ = http(base, Some(Value::Null)).await;
    }
    r.stop();
}

impl Driven {
    /// Whether its browser is still there.
    pub fn alive(&self) -> bool {
        self.remote.process.lock().as_mut().is_some_and(|p| matches!(p.try_wait(), Ok(None)))
    }
}

/// What answers a measured page's requests: a URL's status, headers and body.
pub type Answer = Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, (u16, Vec<(String, String)>, Vec<u8>)> + Send + Sync>;

/// A tab of this machine's Firefox that a page is measured in (`super::preview`): its requests to
/// kiln's own origin answered by kiln, its window as narrow or wide as asked.
pub struct Measuring {
    remote: Arc<Remote>,
    bidi: Arc<Bidi>,
    context: String,
    events: async_channel::Receiver<(String, Value)>,
}

impl Measuring {
    /// A tab of a Firefox kept for measuring; none when this machine has no Firefox.
    pub async fn open(dir: &Path) -> anyhow::Result<Option<Self>> {
        if firefox().is_none() {
            return Ok(None);
        }
        let remote = remote(dir, Engine::Firefox, "measure", None).await?;
        let Conn::Bidi(b) = &remote.conn else { anyhow::bail!("Firefox without its BiDi") };
        let bidi = b.clone();
        let ctx = bidi.call("browsingContext.create", json!({"type": "tab"})).await?.get("context").and_then(Value::as_str).unwrap_or("").to_owned();
        let (tx, events) = async_channel::bounded::<(String, Value)>(1024);
        bidi.pages.lock().insert(ctx.clone(), tx);
        remote.pages.fetch_add(1, Ordering::Relaxed);
        Ok(Some(Self { remote, bidi, context: ctx, events }))
    }

    /// Load `url`, every request to `local` answered by `answer`; done when it's loaded.
    pub async fn load(&self, url: &str, local: &str, answer: Answer) -> anyhow::Result<()> {
        let host = local.trim_start_matches("https://").trim_end_matches('/');
        // The Firefox kept for measuring hears every request of its own (its tabs are only these).
        self.bidi.call("session.subscribe", json!({"events": ["network.beforeRequestSent"]})).await?;
        self.bidi.call("network.addIntercept", json!({"phases": ["beforeRequestSent"], "urlPatterns": [{"type": "pattern", "protocol": "https", "hostname": host}], "contexts": [self.context]})).await?;
        let (bidi, events) = (self.bidi.clone(), self.events.clone());
        crate::rt::spawn(async move {
            while let Ok((method, p)) = events.recv().await {
                if method != "network.beforeRequestSent" || p.get("isBlocked").and_then(Value::as_bool) != Some(true) {
                    continue;
                }
                let (Some(request), Some(u)) = (p.pointer("/request/request").and_then(Value::as_str), p.pointer("/request/url").and_then(Value::as_str)) else { continue };
                let (request, answer, bidi) = (request.to_owned(), answer.clone(), bidi.clone());
                let u = u.to_owned();
                // Each answered on its own: a big file holds up nothing else.
                crate::rt::spawn(async move {
                    use base64::Engine as _;
                    let (status, headers, body) = answer(u).await;
                    let headers: Vec<Value> = headers.into_iter().map(|(k, v)| json!({"name": k, "value": {"type": "string", "value": v}})).collect();
                    let body = json!({"type": "base64", "value": base64::engine::general_purpose::STANDARD.encode(&body)});
                    let _ = bidi.call("network.provideResponse", json!({"request": request, "statusCode": status, "reasonPhrase": if status == 200 { "OK" } else { "Not Found" }, "headers": headers, "body": body})).await;
                });
            }
        });
        self.bidi.call("browsingContext.navigate", json!({"context": self.context, "url": url, "wait": "complete"})).await?;
        Ok(())
    }

    /// Its height (`js` says it) in a window `width` wide.
    pub async fn height_at(&self, width: u32, js: &str) -> anyhow::Result<u32> {
        self.bidi.call("browsingContext.setViewport", json!({"context": self.context, "viewport": {"width": width, "height": 8}, "devicePixelRatio": 1})).await?;
        let eval = |e: &str| self.bidi.call("script.evaluate", json!({"expression": e, "target": {"context": self.context}, "awaitPromise": true, "resultOwnership": "none"}));
        eval("new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))").await?;
        let r = eval(js).await?;
        Ok(r.get("result").map(plain).and_then(|v| v.as_u64()).and_then(|v| u32::try_from(v).ok()).unwrap_or(0))
    }

    /// Done: its tab closes (the Firefox stays a while for the next).
    pub async fn close(self) {
        self.bidi.pages.lock().remove(&self.context);
        let _ = self.bidi.call("browsingContext.close", json!({"context": self.context})).await;
        if self.remote.pages.fetch_sub(1, Ordering::Relaxed) > 1 {
            return;
        }
        let r = self.remote.clone();
        crate::rt::spawn(async move {
            crate::rt::sleep(IDLE).await;
            if r.pages.load(Ordering::Relaxed) == 0 {
                retire(&r).await;
            }
        });
    }
}

/// A BiDi page's events, as a stream's viewers have them.
async fn events(bidi: std::sync::Weak<Bidi>, context: String, rx: async_channel::Receiver<(String, Value)>, said: async_channel::Sender<Value>) {
    while let Ok((method, p)) = rx.recv().await {
        let ev = match method.as_str() {
            "browsingContext.load" => {
                let url = p.get("url").and_then(Value::as_str).unwrap_or("").to_owned();
                let _ = said.send(json!({"navigated": {"url": url}})).await;
                let title = match bidi.upgrade() {
                    Some(b) => b.call("script.evaluate", json!({"expression": "document.title", "target": {"context": context}, "awaitPromise": false})).await.ok().and_then(|r| r.get("result").map(plain)),
                    None => None,
                };
                Some(json!({"loaded": {"title": title.and_then(|t| t.as_str().map(str::to_owned)).unwrap_or_default()}}))
            }
            "browsingContext.fragmentNavigated" | "browsingContext.historyUpdated" => Some(json!({"navigated": {"url": p.get("url").and_then(Value::as_str).unwrap_or("")}})),
            "browsingContext.userPromptOpened" => {
                if let Some(b) = bidi.upgrade() {
                    let _ = b.call("browsingContext.handleUserPrompt", json!({"context": context, "accept": true})).await;
                }
                Some(json!({"dialog": {"type": p.get("type"), "message": p.get("message")}}))
            }
            "browsingContext.contextDestroyed" | "closed" => {
                let _ = said.send(json!({"closed": {"why": "the page went away"}})).await;
                return;
            }
            "script.message" => p.pointer("/data/value").and_then(Value::as_str).and_then(|s| serde_json::from_str::<Value>(s).ok()),
            _ => None,
        };
        if let Some(ev) = ev
            && said.send(ev).await.is_err()
        {
            return;
        }
    }
}

/// A WebDriver session's page, asked what it does (it says nothing by itself): where it is, its
/// title, what its script said, a dialog.
async fn poll(base: String, remote: std::sync::Weak<Remote>, said: async_channel::Sender<Value>) {
    let (mut url, mut title) = (String::new(), String::new());
    let script = format!("({})(s => (window.__kilnSaid = window.__kilnSaid || []).push(s))", focus_script());
    let mut unanswered = 0;
    loop {
        crate::rt::sleep(Duration::from_millis(400)).await;
        if remote.upgrade().is_none() || said.is_closed() {
            return;
        }
        if let Ok(a) = http(&format!("{base}/alert/text"), None).await {
            let _ = http(&format!("{base}/alert/accept"), Some(json!({}))).await;
            let _ = said.send(json!({"dialog": {"message": a.get("value")}})).await;
        }
        let Ok(now) = http(&format!("{base}/url"), None).await else {
            // Its browser went (closed, crashed): so does the page.
            unanswered += 1;
            if unanswered >= 10 {
                let _ = said.send(json!({"closed": {"why": "the browser went away"}})).await;
                return;
            }
            continue;
        };
        unanswered = 0;
        let now = now.get("value").and_then(Value::as_str).unwrap_or("").to_owned();
        if now != url {
            url.clone_from(&now);
            let _ = said.send(json!({"navigated": {"url": now}})).await;
            // A new document: what tells kiln about its fields, again.
            let _ = http(&format!("{base}/execute/sync"), Some(json!({"script": format!("{script}; return 1;"), "args": []}))).await;
        }
        if let Ok(t) = http(&format!("{base}/title"), None).await {
            let t = t.get("value").and_then(Value::as_str).unwrap_or("").to_owned();
            if t != title {
                title.clone_from(&t);
                let _ = said.send(json!({"loaded": {"title": t}})).await;
            }
        }
        if let Ok(v) = http(&format!("{base}/execute/sync"), Some(json!({"script": "const s = window.__kilnSaid || []; window.__kilnSaid = []; return s;", "args": []}))).await {
            for s in v.get("value").and_then(Value::as_array).into_iter().flatten() {
                if let Some(ev) = s.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                    let _ = said.send(ev).await;
                }
            }
        }
    }
}

// ------------------------------------------------------------------------------ actions

/// WebDriver's key values for keys that type nothing.
fn key_value(key: &str) -> Option<String> {
    if key.chars().count() == 1 {
        return Some(key.to_owned());
    }
    let code = match key {
        "Cancel" => 0xE001,
        "Help" => 0xE002,
        "Backspace" => 0xE003,
        "Tab" => 0xE004,
        "Clear" => 0xE005,
        "Enter" => 0xE007,
        "Shift" => 0xE008,
        "Control" => 0xE009,
        "Alt" => 0xE00A,
        "Pause" => 0xE00B,
        "Escape" => 0xE00C,
        "PageUp" => 0xE00E,
        "PageDown" => 0xE00F,
        "End" => 0xE010,
        "Home" => 0xE011,
        "ArrowLeft" => 0xE012,
        "ArrowUp" => 0xE013,
        "ArrowRight" => 0xE014,
        "ArrowDown" => 0xE015,
        "Insert" => 0xE016,
        "Delete" => 0xE017,
        "Meta" => 0xE03D,
        k => {
            let n: u32 = k.strip_prefix('F')?.parse().ok().filter(|n| (1..=12).contains(n))?;
            0xE031 + n - 1
        }
    };
    char::from_u32(code).map(String::from)
}

/// Inputs as runs of WebDriver actions, each run one input source's: `("mouse" | "touch-<id>" |
/// "key" | "wheel", its actions)`.
fn action_runs(events: &[Input]) -> Vec<(String, Vec<Value>)> {
    let mut runs: Vec<(String, Vec<Value>)> = Vec::new();
    let mut add = |source: String, actions: Vec<Value>| match runs.last_mut() {
        Some((s, a)) if *s == source => a.extend(actions),
        _ => runs.push((source, actions)),
    };
    let at = |x: f64, y: f64| json!({"type": "pointerMove", "x": x.round(), "y": y.round(), "origin": "viewport", "duration": 0});
    for e in events {
        match e {
            Input::Pointer { k, x, y, button, pt, id, .. } => {
                let source = if pt == "touch" { format!("touch-{id}") } else { "mouse".to_owned() };
                let button = (*button).clamp(0, 4);
                let acts = match k.as_str() {
                    "move" => vec![at(*x, *y)],
                    "down" => vec![at(*x, *y), json!({"type": "pointerDown", "button": button})],
                    "up" | "cancel" => vec![at(*x, *y), json!({"type": "pointerUp", "button": button})],
                    _ => vec![],
                };
                if !acts.is_empty() {
                    add(source, acts);
                }
            }
            Input::Wheel { x, y, dx, dy, .. } => add("wheel".to_owned(), vec![json!({"type": "scroll", "x": x.round(), "y": y.round(), "deltaX": dx.round(), "deltaY": dy.round(), "origin": "viewport"})]),
            Input::Key { k, key, .. } => {
                if let Some(v) = key_value(key) {
                    add("key".to_owned(), vec![json!({"type": if k == "down" { "keyDown" } else { "keyUp" }, "value": v})]);
                }
            }
            Input::Text { text } => add("key".to_owned(), text.chars().flat_map(|c| [json!({"type": "keyDown", "value": c.to_string()}), json!({"type": "keyUp", "value": c.to_string()})]).collect()),
            Input::Compose { .. } => {}
        }
    }
    runs
}

/// A run of actions as its input source says it.
fn sources(source: &str, actions: Vec<Value>) -> Value {
    match source {
        "key" => json!({"type": "key", "id": "keyboard", "actions": actions}),
        "wheel" => json!({"type": "wheel", "id": "wheel", "actions": actions}),
        s if s.starts_with("touch-") => json!({"type": "pointer", "id": s, "parameters": {"pointerType": "touch"}, "actions": actions}),
        _ => json!({"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}, "actions": actions}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engines_are_named() -> anyhow::Result<()> {
        assert_eq!(Engine::parse("")?, Engine::Chromium);
        assert_eq!(Engine::parse("Firefox")?, Engine::Firefox);
        assert_eq!(Engine::parse("safari")?, Engine::Webkit);
        assert!(Engine::parse("lynx").is_err());
        Ok(())
    }

    #[test]
    fn inputs_become_webdriver_actions_by_source() {
        let p = |k: &str, pt: &str| Input::Pointer { k: k.to_owned(), x: 10.4, y: 20.6, button: 0, buttons: 0, pt: pt.to_owned(), id: 3, mods: 0, clicks: 1 };
        let key = |k: &str, key: &str| Input::Key { k: k.to_owned(), key: key.to_owned(), code: String::new(), text: String::new(), mods: 0, rep: false, loc: 0 };
        let runs = action_runs(&[p("down", "mouse"), p("up", "mouse"), key("down", "Enter"), key("up", "Enter"), Input::Text { text: "hé".into() }, p("down", "touch"), Input::Wheel { x: 1.0, y: 2.0, dx: 0.0, dy: 120.4, mods: 0 }]);
        let names: Vec<&str> = runs.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(names, ["mouse", "key", "touch-3", "wheel"], "each run one source, in order");
        let mouse = runs.first().map(|(_, a)| a.clone()).unwrap_or_default();
        assert_eq!(mouse.len(), 4);
        assert_eq!(mouse.first(), Some(&json!({"type": "pointerMove", "x": 10.0, "y": 21.0, "origin": "viewport", "duration": 0})));
        let keys = runs.get(1).map(|(_, a)| a.clone()).unwrap_or_default();
        assert_eq!(keys.first(), Some(&json!({"type": "keyDown", "value": "\u{E007}"})));
        assert_eq!(keys.len(), 6, "Enter down and up, then two characters typed");
        assert_eq!(key_value("F5").as_deref(), Some("\u{E035}"));
        assert_eq!(key_value("a").as_deref(), Some("a"));
        assert!(key_value("Unidentified").is_none());
        assert_eq!(sources("touch-3", vec![]).pointer("/parameters/pointerType"), Some(&json!("touch")));
    }

    #[test]
    fn bidi_values_are_plain_json() {
        let v = json!({"type": "array", "value": [{"type": "number", "value": 2}, {"type": "string", "value": "a"}, {"type": "object", "value": [["k", {"type": "boolean", "value": true}]]}, {"type": "undefined"}]});
        assert_eq!(plain(&v), json!([2, "a", {"k": true}, null]));
    }
}
