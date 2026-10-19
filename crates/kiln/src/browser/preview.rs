//! What an agent sees of a page before it shows it (docs/previews.md): how it looks at a width and
//! in an appearance, what it printed, how tall it is at each width a viewer might have (so its
//! frame in the conversation is the right size before it loads), and how it behaves for a moment
//! after it loads: layout shift, long tasks, frames painted.
//!
//! A page given as HTML or as a file is loaded from an origin of its own that only this browser
//! knows ([`LOCAL`], its paths the machine's own): kiln answers each of its requests from the
//! machine's files (never a secret), so modules, `fetch` and relative paths work as they will where
//! the page is shown, and kiln learns every file it loads, and every file from the web.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::cdp::{Cdp, Event};
use super::page::{Page, Said};

/// The widths a viewer's conversation column might have, phones to desktops: a page's height is
/// measured at each, and a frame takes the taller of the two nearest its own width.
pub const WIDTHS: [u32; 10] = [320, 360, 390, 430, 520, 640, 728, 860, 1000, 1144];

/// The origin a page given as HTML or a file is loaded from: `https://kiln.invalid/home/ada/page.html`
/// is `/home/ada/page.html`. Nothing outside this browser answers for it (`.invalid` never
/// resolves).
pub const LOCAL: &str = "https://kiln.invalid";

/// The most a page's file may be.
const MOST_FILE: u64 = 32 << 20;

fn width() -> u32 {
    390
}
fn height() -> u32 {
    844
}
fn yes() -> bool {
    true
}

/// A preview asked for: the page (its HTML, a file, or an address) and how to show it.
#[derive(Debug, Clone, Deserialize)]
pub struct Ask {
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// For `html`, the path it stands at: relative references are in its folder.
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default = "width")]
    pub width: u32,
    #[serde(default = "height")]
    pub height: u32,
    /// Pixel density (2 by default).
    #[serde(default)]
    pub scale: Option<f64>,
    /// A phone's (touch, a phone's viewport rules); by default when narrower than 600.
    #[serde(default)]
    pub mobile: Option<bool>,
    #[serde(default = "yes")]
    pub dark: bool,
    #[serde(default)]
    pub reduced_motion: bool,
    /// How long it's watched after it loads, in ms (1500 by default, at most 10 000).
    #[serde(default)]
    pub watch_ms: Option<u32>,
    /// Its height at each of [`WIDTHS`].
    #[serde(default)]
    pub measure: bool,
    /// "window" (the default), "page" (all of it) or "none".
    #[serde(default)]
    pub picture: Option<String>,
    /// The page as kiln shows it in a conversation (`crate::harness::visuals`): its theme, fonts
    /// and bridge in.
    #[serde(default)]
    pub as_shown: bool,
}

/// What a page loaded: files of this machine (their paths) and files from the web.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loaded {
    pub local: Vec<String>,
    pub web: Vec<String>,
}

/// What a preview saw.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Seen {
    pub url: String,
    pub width: u32,
    pub height: u32,
    /// How tall the page is at `width`.
    pub content_height: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub heights: Vec<(u32, u32)>,
    pub said: Vec<Said>,
    /// Cumulative layout shift while it loaded and was watched (0 is none: nothing moved).
    pub layout_shift: f64,
    pub long_tasks: u32,
    pub longest_task_ms: f64,
    /// Frames the page could paint a second while it was watched.
    pub fps: f64,
    /// Where the picture (a PNG) was saved on this machine: put that path in a message (as a
    /// Markdown image) and kiln shows it there. Kept a day.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture_path: Option<String>,
    /// For a page given as HTML or a file: what it loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loaded: Option<Loaded>,
}

/// Counts what a page does from its first moment: layout shifts (every one: nobody touches a page
/// being looked at, and an emulated phone's shifts all read as after input), long tasks, frames.
const WATCH: &str = r"(() => {
  if (window.__kilnMetrics) return;
  const m = { cls: 0, longTasks: 0, longest: 0, frames: 0, since: 0 };
  try { new PerformanceObserver((l) => { for (const e of l.getEntries()) m.cls += e.value; }).observe({ type: 'layout-shift', buffered: true }); } catch (e) {}
  try { new PerformanceObserver((l) => { for (const e of l.getEntries()) { m.longTasks += 1; m.longest = Math.max(m.longest, e.duration); } }).observe({ type: 'longtask', buffered: true }); } catch (e) {}
  const tick = () => { m.frames += 1; requestAnimationFrame(tick); };
  requestAnimationFrame(() => { m.since = performance.now(); requestAnimationFrame(tick); });
  Object.defineProperty(window, '__kilnMetrics', { value: m });
})();";

const READ: &str = r"(() => {
  const m = window.__kilnMetrics;
  if (!m) return null;
  const s = (performance.now() - m.since) / 1000;
  return { cls: m.cls, longTasks: m.longTasks, longest: m.longest, fps: s > 0 && m.since > 0 ? m.frames / s : 0 };
})()";

/// How tall the page is: as a conversation's frame measures it (`crate::harness::visuals`), its
/// own height, not the window's.
const CONTENT_HEIGHT: &str = "(() => { const d = document.documentElement, b = document.body; \
const bottom = b ? b.getBoundingClientRect().bottom + (parseFloat(getComputedStyle(b).marginBottom) || 0) : 0; \
return Math.ceil(Math.max(d.getBoundingClientRect().height, bottom)); })()";

/// Where a page given as HTML stands when nothing says: kiln's state folder.
fn default_entry(dir: &Path) -> PathBuf {
    dir.join("browser").join("pages").join("page.html")
}

/// Preview a page in the machine's browser (state in `dir`).
pub async fn preview(dir: &Path, ask: Ask) -> anyhow::Result<Seen> {
    let browser = super::shared(dir).await?;
    let mut page = browser.page().await?;
    let r = run(&mut page, dir, ask).await;
    page.close().await;
    r
}

async fn run(page: &mut Page, dir: &Path, ask: Ask) -> anyhow::Result<Seen> {
    // The page and what it stands at, when kiln serves it.
    let local = match (&ask.html, &ask.path, &ask.url) {
        (Some(html), _, _) => {
            let entry = ask.entry.clone().unwrap_or_else(|| default_entry(dir).to_string_lossy().into_owned());
            Some((entry, html.clone()))
        }
        (None, Some(path), _) => {
            let p = crate::harness::files::expand(path).ok_or_else(|| anyhow::anyhow!("{path} isn't a full path"))?;
            let text = std::fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
            Some((p.to_string_lossy().into_owned(), text))
        }
        (None, None, Some(_)) => None,
        _ => anyhow::bail!("which page? Give its html, its path, or its url"),
    };
    let scale = ask.scale.unwrap_or(2.0).clamp(1.0, 4.0);
    let (w, h) = (ask.width.clamp(240, 3840), ask.height.clamp(120, 4000));
    let mobile = ask.mobile.unwrap_or(w < 600);
    page.viewport(w, h, scale, mobile).await?;
    page.appearance(ask.dark, ask.reduced_motion).await?;
    page.before_load(WATCH).await?;
    let (url, server) = match local {
        Some((entry, html)) => {
            let html = if ask.as_shown { crate::harness::visuals::with_bridge(&html) } else { html };
            let s = Server::start(page, &entry, html).await?;
            (local_url(&entry), Some(s))
        }
        None => (ask.url.clone().unwrap_or_default(), None),
    };
    let r = look(page, dir, &ask, &url, (w, h, scale, mobile)).await;
    let loaded = server.map(Server::stop);
    let mut seen = r?;
    seen.loaded = loaded;
    Ok(seen)
}

async fn look(page: &mut Page, dir: &Path, ask: &Ask, url: &str, (w, h, scale, mobile): (u32, u32, f64, bool)) -> anyhow::Result<Seen> {
    page.go(url, Duration::from_secs(5)).await?;
    crate::rt::sleep(Duration::from_millis(u64::from(ask.watch_ms.unwrap_or(1500).min(10_000)))).await;
    let m = page.eval(READ).await?;
    let num = |k: &str| m.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    // Heights as a frame has them: the window as short as it goes (so the page's own height is
    // what's measured), and a desktop's viewport rules (a frame lays out at its own width,
    // whatever a page tells a phone).
    let measure = async |page: &Page, width: u32| -> anyhow::Result<u32> {
        page.viewport(width, 8, 1.0, false).await?;
        page.settle().await?;
        Ok(page.eval(CONTENT_HEIGHT).await?.as_u64().and_then(|v| u32::try_from(v).ok()).unwrap_or(0))
    };
    let mut heights = Vec::new();
    if ask.measure {
        for width in WIDTHS {
            heights.push((width, measure(page, width).await?));
        }
    }
    let content_height = measure(page, w).await?;
    page.viewport(w, h, scale, mobile).await?;
    page.settle().await?;
    let picture = match ask.picture.as_deref() {
        Some("none") => None,
        Some("page") => Some(page.picture(true).await?),
        _ => Some(page.picture(false).await?),
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let long_tasks = num("longTasks") as u32;
    Ok(Seen {
        url: url.to_owned(),
        width: w,
        height: h,
        content_height,
        heights,
        said: page.said().await,
        layout_shift: (num("cls") * 10_000.0).round() / 10_000.0,
        long_tasks,
        longest_task_ms: num("longest").round(),
        fps: num("fps").round(),
        picture_path: match picture {
            Some(png) => Some(save(dir, &png)?),
            None => None,
        },
        loaded: None,
    })
}

/// Answers a page's requests to [`LOCAL`] from this machine's files while it's looked at, and
/// notes what it loads.
struct Server {
    stop: Arc<AtomicBool>,
    loaded: Arc<Mutex<Loaded>>,
}

impl Server {
    async fn start(page: &Page, entry: &str, html: String) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let loaded = Arc::new(Mutex::new(Loaded::default()));
        let events = page.listen();
        let (cdp, session) = (page.cdp(), page.session().to_owned());
        crate::rt::spawn(serve(cdp, session, events, entry.to_owned(), Arc::new(html), stop.clone(), loaded.clone()));
        page.call("Network.enable", json!({})).await?;
        page.call("Fetch.enable", json!({"patterns": [{"urlPattern": format!("{LOCAL}/*"), "requestStage": "Request"}]})).await?;
        Ok(Self { stop, loaded })
    }

    fn stop(self) -> Loaded {
        self.stop.store(true, Ordering::Relaxed);
        let mut l = self.loaded.lock().clone();
        l.local.sort();
        l.local.dedup();
        l.web.sort();
        l.web.dedup();
        l
    }
}

async fn serve(cdp: Arc<Cdp>, session: String, events: async_channel::Receiver<Event>, entry: String, html: Arc<String>, stop: Arc<AtomicBool>, loaded: Arc<Mutex<Loaded>>) {
    while !stop.load(Ordering::Relaxed) {
        let e = match crate::rt::timeout(Duration::from_millis(250), events.recv()).await {
            Ok(Ok(e)) => e,
            Ok(Err(_)) => return,
            Err(_) => continue,
        };
        match e.method.as_str() {
            // Each answered on its own: a big file holds up nothing else.
            "Fetch.requestPaused" => {
                crate::rt::spawn(answer(cdp.clone(), session.clone(), e.params, entry.clone(), html.clone(), loaded.clone()));
            }
            "Network.responseReceived" => {
                let r = e.params.get("response");
                let url = r.and_then(|r| r.get("url")).and_then(Value::as_str).unwrap_or("");
                let status = r.and_then(|r| r.get("status")).and_then(Value::as_u64).unwrap_or(0);
                let kind = e.params.get("type").and_then(Value::as_str).unwrap_or("");
                if (url.starts_with("https://") || url.starts_with("http://")) && !url.starts_with(LOCAL) && (200..300).contains(&status) && kind != "Document" {
                    loaded.lock().web.push(url.to_owned());
                }
            }
            _ => {}
        }
    }
}

/// One request to [`LOCAL`]: the page itself, kiln's fonts, or a file of this machine.
async fn answer(cdp: Arc<Cdp>, session: String, params: Value, entry: String, html: Arc<String>, loaded: Arc<Mutex<Loaded>>) {
    let id = params.get("requestId").and_then(Value::as_str).unwrap_or("").to_owned();
    let url = params.get("request").and_then(|r| r.get("url")).and_then(Value::as_str).unwrap_or("");
    let path = local_path(url).unwrap_or_default();
    let (status, mime, body) = if path == entry {
        (200, "text/html; charset=utf-8".to_owned(), html.as_bytes().to_vec())
    } else if let Some(name) = path.strip_prefix("/v/_/fonts/") {
        match crate::ui::font(name) {
            Some(b) => (200, "font/woff2".to_owned(), b.to_vec()),
            None => (404, "text/plain".to_owned(), b"no such font".to_vec()),
        }
    } else {
        let p = path.clone();
        match crate::rt::blocking(move || crate::harness::files::read_whole(&p, MOST_FILE)).await {
            Ok(Ok((meta, data))) => {
                loaded.lock().local.push(path.clone());
                (200, crate::harness::visuals::served_type(&path, &meta.mime), data)
            }
            Ok(Err(e)) => (404, "text/plain; charset=utf-8".to_owned(), e.into_bytes()),
            Err(e) => (500, "text/plain; charset=utf-8".to_owned(), format!("{e:#}").into_bytes()),
        }
    };
    let mut headers = vec![
        json!({"name": "Content-Type", "value": mime}),
        // Its own origin is opaque (sandboxed, as where it's shown): modules, fonts and fetches
        // of its files are cross-origin.
        json!({"name": "Access-Control-Allow-Origin", "value": "*"}),
        json!({"name": "Cache-Control", "value": "no-store"}),
    ];
    if path == entry {
        headers.push(json!({"name": "Content-Security-Policy", "value": "sandbox allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-downloads allow-pointer-lock"}));
    }
    let body = base64::engine::general_purpose::STANDARD.encode(&body);
    let _ = cdp.call(Some(&session), "Fetch.fulfillRequest", json!({"requestId": id, "responseCode": status, "responseHeaders": headers, "body": body})).await;
}

/// A path's address on [`LOCAL`] (each segment encoded; a Windows path's drive first).
pub fn local_url(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = if p.starts_with('/') { p } else { format!("/{p}") };
    let mut out = String::from(LOCAL);
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b'~' | b':' | b'@' | b'+') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path an address on [`LOCAL`] stands for.
pub fn local_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix(LOCAL)?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let mut it = rest.bytes();
    while let Some(c) = it.next() {
        if c == b'%' {
            let mut ahead = it.clone();
            if let (Some(h), Some(l)) = (ahead.next().and_then(|h| char::from(h).to_digit(16)), ahead.next().and_then(|l| char::from(l).to_digit(16)))
                && let Ok(v) = u8::try_from(h * 16 + l)
            {
                bytes.push(v);
                it = ahead;
                continue;
            }
        }
        bytes.push(c);
    }
    let p = String::from_utf8(bytes).ok()?;
    // A Windows path: `/C:/Users/...` is `C:/Users/...`.
    let drive = p.as_bytes().get(2) == Some(&b':') && p.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic);
    Some(if cfg!(windows) && drive { p.get(1..).unwrap_or("").to_owned() } else { p })
}

/// A picture into the machine's pictures (those older than a day go), its path.
fn save(dir: &Path, png: &[u8]) -> anyhow::Result<String> {
    let pictures = dir.join("browser").join("pictures");
    std::fs::create_dir_all(&pictures)?;
    if let Ok(entries) = std::fs::read_dir(&pictures) {
        for e in entries.flatten() {
            if e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(86_400)) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let f = pictures.join(format!("preview-{}-{}.png", crate::util::now_ms(), std::process::id()));
    std::fs::write(&f, png)?;
    Ok(f.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_files_address_is_its_path_on_kilns_own_origin() {
        assert_eq!(local_url("/tmp/a b/shot#1.html"), "https://kiln.invalid/tmp/a%20b/shot%231.html");
        assert_eq!(local_path("https://kiln.invalid/tmp/a%20b/shot%231.html?x=1").as_deref(), Some("/tmp/a b/shot#1.html"));
        assert_eq!(local_path("https://example.com/a.png"), None);
    }

    /// The real thing: the pinned browser downloaded (once, into a folder of the test's own),
    /// started over its pipe, a page served to it from this machine's files, looked at, measured.
    #[test]
    #[ignore = "downloads and starts the real browser (about 200 MB)"]
    fn the_real_browser_looks_at_a_page() -> anyhow::Result<()> {
        let dir = std::env::var_os("KILN_BROWSER_TEST_DIR").map_or_else(|| std::env::temp_dir().join("kiln-browser-test"), PathBuf::from);
        let work = dir.join("work");
        std::fs::create_dir_all(&work)?;
        std::fs::write(work.join("lib.js"), "export const n = 7;\n")?;
        let html = "<!doctype html><body style='margin:0'><div id=d style='height:50px'></div><script type=module>import { n } from './lib.js'; d.textContent = n; console.log('n is', n);</script></body>";
        let ask: Ask = serde_json::from_value(json!({"html": html, "entry": work.join("page.html").to_string_lossy(), "measure": true, "as_shown": true}))?;
        let d = dir.clone();
        let seen = crate::rt::block_on(async move { preview(&d, ask).await })?;
        assert!(seen.heights.len() == WIDTHS.len() && seen.heights.iter().all(|(_, h)| *h == 50), "{:?}", seen.heights);
        assert!(seen.said.iter().any(|s| s.text == "n is 7"), "{:?}", seen.said);
        assert!(seen.loaded.as_ref().is_some_and(|l| l.local.iter().any(|p| p.ends_with("lib.js"))), "{:?}", seen.loaded);
        assert!(seen.picture_path.as_deref().is_some_and(|p| std::fs::metadata(p).is_ok_and(|m| m.len() > 1000)));
        Ok(())
    }

    #[test]
    fn an_ask_takes_a_phones_size_and_the_dark_by_default() -> anyhow::Result<()> {
        let a: Ask = serde_json::from_str(r#"{"html": "<p>hi</p>"}"#)?;
        assert_eq!((a.width, a.height, a.dark, a.measure), (390, 844, true, false));
        Ok(())
    }
}
