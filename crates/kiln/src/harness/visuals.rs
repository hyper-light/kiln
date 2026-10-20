//! Pages an agent shows in its conversation (docs/previews.md, "Pages"). Its `show_page` tool
//! publishes a page: the server keeps it with every file it uses (pictures, fonts, styles,
//! scripts and data, from the agent's machine; libraries and data it loads from the web, pinned so
//! a viewer's browser never goes there) and puts kiln's bridge in it.
//!
//! A page is served as its machine has it: at `/v/<id>` followed by its own path (the id
//! unguessable), its files beside it at theirs, and what it loaded from the web at
//! `/v/<id>/_web/<scheme>/<host>/<path>`. So whatever the page and its files say relative to
//! themselves (`./lib.js`, `url(../fonts/a.woff2)`, a module's own imports) leads where it led on
//! the machine; only full paths and web addresses in them are rewritten to their copies. What's
//! kept is what the page names and what it loaded when kiln's browser on the agent's machine looked
//! at it, its dynamic imports and fetches too.
//!
//! It's served sandboxed: its scripts run, but away from the viewer's kiln session (an opaque
//! origin: no cookies, no storage, no calls to kiln as the viewer). The bridge (MCP Apps' messages,
//! and kiln's) tells the conversation the page's height, opens links outside it, takes kiln's
//! theme, pauses what moves (animations, frames, media, sound) while the page is out of view, keeps
//! what the person did in it, and carries a choice made in it back to the agent
//! (`kiln.reply(value, label)`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::hub::Hub;

/// The page itself.
pub const MOST_PAGE: usize = 2 << 20;
/// Each file it uses, and all of them.
pub const MOST_FILE: u64 = 32 << 20;
pub const MOST_FILES: u64 = 128 << 20;
/// How many files one page may use.
pub const MOST_USED: usize = 200;

/// A published page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Visual {
    pub id: String,
    pub session: String,
    pub title: String,
    /// Where the page itself is, under `/v/<id>` (its path on its machine).
    pub entry: String,
    /// The page and every file it uses, each where it's served.
    pub files: Vec<Used>,
    /// Its height at each width a viewer's conversation might have, as measured in the browser
    /// on the agent's machine; a frame takes its size from these before the page loads.
    #[serde(default)]
    pub heights: Vec<(u32, u32)>,
    /// The same in other engines (`firefox`, `webkit`): as the machine's Firefox measured it, and as
    /// viewers' own browsers did at their widths (`Visuals::seen`). A viewer's frame takes its own
    /// engine's where there are any near its width.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub engines: std::collections::BTreeMap<String, Vec<(u32, u32)>>,
    /// Unix ms.
    pub at: i64,
}

/// A file a page uses: where it's served (under `/v/<id>`), what it was (a path on the agent's
/// machine, or an address on the web), and the copy kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Used {
    pub path: String,
    pub from: String,
    pub sha: String,
    pub mime: String,
    pub size: u64,
}

/// A reference in a page or one of its files: where it is (in bytes) and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub start: usize,
    pub end: usize,
    pub value: String,
    /// Only quoted (a path in a script or another attribute), not something the page loads by
    /// itself: when there's no such file, it's left as it is.
    pub quoted: bool,
}

/// Names of files a page or a style may use (what's quoted in a script counts only with these).
const USED_KINDS: [&str; 30] = [
    "png", "jpg", "jpeg", "gif", "webp", "avif", "svg", "ico", "bmp", "mp4", "webm", "mov", "mp3", "wav", "ogg", "oga", "flac", "m4a", "woff", "woff2", "ttf", "otf", "css", "js", "mjs", "json", "csv",
    "wasm", "glb", "gltf",
];

fn used_kind(value: &str) -> bool {
    let path = value.split(['?', '#']).next().unwrap_or(value);
    path.rsplit_once('.').is_some_and(|(_, ext)| USED_KINDS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// The value of a quoted (or bare) attribute starting at `at` (just after its `=`).
fn attr_value(text: &str, at: usize) -> Option<(usize, usize)> {
    let rest = text.get(at..)?;
    let lead = rest.len() - rest.trim_start().len();
    let start = at + lead;
    let first = text.get(start..)?.chars().next()?;
    if first == '"' || first == '\'' {
        let inner = start + 1;
        let len = text.get(inner..)?.find(first)?;
        Some((inner, inner + len))
    } else {
        let len = text.get(start..)?.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(0);
        (len > 0).then_some((start, start + len))
    }
}

/// The files a page refers to: in the attributes that load something (`src`, `href`, `poster`,
/// `data`, `srcset`), in styles' `url(...)`, modules' imports, and full paths and web addresses of
/// files quoted in scripts. In order; a value that appears twice is two references.
pub fn references(html: &str) -> Vec<Ref> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    for attr in ["src", "href", "poster", "data", "srcset"] {
        let needle = format!("{attr}=");
        let mut from = 0;
        while let Some(i) = lower.get(from..).and_then(|r| r.find(&needle)) {
            let at = from + i;
            from = at + needle.len();
            // The whole attribute name, not the end of another ("data-src=", "xsrc=").
            let before = lower.get(..at).and_then(|b| b.chars().last());
            if !before.is_some_and(char::is_whitespace) {
                continue;
            }
            let Some((s, e)) = attr_value(html, at + needle.len()) else { continue };
            let Some(value) = html.get(s..e) else { continue };
            if attr == "srcset" {
                // "a.png 1x, b.png 2x": each candidate's address.
                let mut off = s;
                for cand in value.split(',') {
                    let lead = cand.len() - cand.trim_start().len();
                    let url = cand.split_whitespace().next().unwrap_or("");
                    if !url.is_empty() {
                        out.push(Ref { start: off + lead, end: off + lead + url.len(), value: url.to_owned(), quoted: false });
                    }
                    off += cand.len() + 1;
                }
            } else if !value.trim().is_empty() {
                out.push(Ref { start: s, end: e, value: value.to_owned(), quoted: false });
            }
        }
    }
    out.extend(css_references(html));
    out.extend(script_references(html));
    out.sort_by_key(|r| r.start);
    out.dedup_by_key(|r| r.start);
    out
}

/// `url(...)` and `@import "..."` in a style sheet or a page's styles.
pub fn css_references(css: &str) -> Vec<Ref> {
    let mut out = Vec::new();
    let lower = css.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower.get(from..).and_then(|r| r.find("url(")) {
        let open = from + i + 4;
        let Some(close) = css.get(open..).and_then(|r| r.find(')')) else { break };
        let Some(inner) = css.get(open..open + close) else { break };
        let t = inner.trim();
        let lead = inner.len() - inner.trim_start().len();
        let (s, v) = match t.chars().next() {
            Some(q @ ('"' | '\'')) if t.len() >= 2 && t.ends_with(q) => (open + lead + 1, t.get(1..t.len() - 1).unwrap_or("")),
            _ => (open + lead, t),
        };
        if !v.is_empty() {
            out.push(Ref { start: s, end: s + v.len(), value: v.to_owned(), quoted: false });
        }
        from = open + close + 1;
    }
    // `@import "a.css"` (an `@import url(...)` was found above).
    let mut from = 0;
    while let Some(i) = lower.get(from..).and_then(|r| r.find("@import")) {
        let at = from + i + 7;
        from = at;
        let rest = css.get(at..).unwrap_or("");
        let lead = rest.len() - rest.trim_start().len();
        if let Some(q @ ('"' | '\'')) = rest.trim_start().chars().next() {
            let s = at + lead + 1;
            if let Some(len) = css.get(s..).and_then(|r| r.find(q))
                && let Some(v) = css.get(s..s + len)
            {
                out.push(Ref { start: s, end: s + len, value: v.to_owned(), quoted: false });
            }
        }
    }
    out
}

/// What scripts load: modules' imports (`import ... from "./a.js"`, `import("./b.js")`, `export
/// ... from`), and full paths and web addresses of files they quote.
pub fn script_references(js: &str) -> Vec<Ref> {
    let mut out = Vec::new();
    for q in ['"', '\'', '`'] {
        let mut from = 0;
        while let Some(i) = js.get(from..).and_then(|r| r.find(q)) {
            let s = from + i + 1;
            let Some(len) = js.get(s..).and_then(|r| r.find(q)) else { break };
            let e = s + len;
            from = e + 1;
            let Some(v) = js.get(s..e) else { continue };
            if v.is_empty() || v.contains(char::is_whitespace) || v.len() > 2048 {
                continue;
            }
            // An import's specifier: what comes before the quote is `from`, `import` or `import(`.
            let before = js.get(..s - 1).unwrap_or("").trim_end();
            let imported = before.ends_with("from") || before.ends_with("import") || before.ends_with("import(");
            let relative = v.starts_with("./") || v.starts_with("../");
            let full = (v.starts_with('/') && !v.starts_with("//")) || v.starts_with("~/") || v.starts_with("file://");
            let web = v.starts_with("https://") || v.starts_with("http://") || v.starts_with("//");
            if (imported && (relative || full || web)) || ((full || web) && used_kind(v)) {
                out.push(Ref { start: s, end: e, value: v.to_owned(), quoted: !imported });
            }
        }
    }
    out.sort_by_key(|r| r.start);
    out.dedup_by_key(|r| r.start);
    out
}

/// What a reference is relative to: a folder of the agent's machine, or an address on the web.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    Local(String),
    Web(String),
}

/// Where a reference leads: a file on the agent's machine, the web, or nothing to fetch (in the
/// page, `data:`, an anchor).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Local(String),
    Web(String),
}

pub fn target(value: &str, base: &Base) -> Option<Target> {
    let v = value.trim();
    if v.is_empty() || v.starts_with('#') || v.starts_with("data:") || v.starts_with("blob:") || v.starts_with("javascript:") || v.starts_with("mailto:") || v.starts_with("about:") {
        return None;
    }
    if v.starts_with("https://") || v.starts_with("http://") {
        return Some(Target::Web(v.split('#').next().unwrap_or(v).to_owned()));
    }
    if let Some(rest) = v.strip_prefix("//") {
        let scheme = match base {
            Base::Web(u) if u.starts_with("http://") => "http",
            _ => "https",
        };
        return Some(Target::Web(format!("{scheme}://{}", rest.split('#').next().unwrap_or(rest))));
    }
    match base {
        // In a file from the web, a path is on its site.
        Base::Web(url) => {
            if v.contains(':') && !v.starts_with('/') && !v.starts_with('.') {
                return None;
            }
            Some(Target::Web(joined(url, v.split('#').next().unwrap_or(v))))
        }
        Base::Local(dir) => {
            let path = v.strip_prefix("file://").map_or(v, |p| p.strip_prefix("localhost").unwrap_or(p));
            let path = path.split(['?', '#']).next().unwrap_or(path);
            if path.contains(':') {
                return None;
            }
            Some(Target::Local(super::files::resolve(&percent_decoded(path), dir)?))
        }
    }
}

fn percent_decoded(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(c) = bytes.next() {
        if c == b'%' {
            let mut ahead = bytes.clone();
            if let (Some(h), Some(l)) = (ahead.next().and_then(|h| char::from(h).to_digit(16)), ahead.next().and_then(|l| char::from(l).to_digit(16)))
                && let Ok(v) = u8::try_from(h * 16 + l)
            {
                out.push(v);
                bytes = ahead;
                continue;
            }
        }
        out.push(c);
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// Where a file is served, under `/v/<id>`: a file of the machine at its own path, one from the
/// web under `/_web/<scheme>/<host>/<path>` (a query, when it has one, made part of its name).
pub fn served_at(t: &Target) -> String {
    match t {
        Target::Local(p) => {
            let p = p.replace('\\', "/");
            if p.starts_with('/') { p } else { format!("/{p}") }
        }
        Target::Web(url) => {
            let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
            let (rest, query) = rest.split_once('?').map_or((rest, None), |(r, q)| (r, Some(q)));
            let rest = if rest.contains('/') { rest.to_owned() } else { format!("{rest}/") };
            let mut path = format!("/_web/{scheme}/{rest}");
            if path.ends_with('/') {
                path.push_str("index");
            }
            if let Some(q) = query.filter(|q| !q.is_empty()) {
                path.push_str(&format!("~{}", hex::encode(Sha256::digest(q.as_bytes())).get(..12).unwrap_or("")));
            }
            path
        }
    }
}

/// A path as an address's path: what isn't plain encoded (a space, `#`, `?`, `%`, quotes).
pub fn url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b'~' | b':' | b'@' | b'+' | b'!' | b'$' | b'&' | b'(' | b')' | b'*' | b',' | b';' | b'=') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `text` with each reference whose value `to` maps rewritten to that (nothing else touched).
pub fn rewrite(text: &str, refs: &[Ref], to: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for r in refs {
        let Some(new) = to.get(&r.value) else { continue };
        if r.start < at {
            continue;
        }
        out.push_str(text.get(at..r.start).unwrap_or(""));
        out.push_str(new);
        at = r.end;
    }
    out.push_str(text.get(at..).unwrap_or(""));
    out
}

/// kiln's theme for a page (dark; the conversation sends its own as it changes): the names the
/// page's styles use, as MCP Apps' hosts give them, and the fonts the console reads in.
pub const THEME: [(&str, &str); 22] = [
    ("--background", "transparent"),
    ("--foreground", "#e9ebee"),
    ("--muted", "#15171b"),
    ("--muted-foreground", "#8f929b"),
    ("--card", "#0c0d10"),
    ("--card-foreground", "#e9ebee"),
    ("--border", "#2b2d32"),
    ("--input", "#1d1f23"),
    ("--ring", "#bab0e3"),
    ("--primary", "#ececee"),
    ("--primary-foreground", "#08090a"),
    ("--accent", "#bab0e3"),
    ("--accent-foreground", "#08090a"),
    ("--destructive", "#d1aaad"),
    ("--warning", "#d8bb95"),
    ("--success", "#a8c9bc"),
    ("--chart-1", "#b8d9d2"),
    ("--chart-2", "#a6c4ed"),
    ("--chart-3", "#b8a6d5"),
    ("--chart-4", "#d6a2aa"),
    ("--chart-5", "#d8cbb0"),
    ("--radius", "8px"),
];

/// What goes at the top of a page: kiln's theme and fonts, then the bridge.
fn head_extra() -> String {
    let vars: String = THEME.iter().map(|(k, v)| format!("{k}:{v};")).collect();
    format!(
        r#"<meta name="color-scheme" content="dark light"><style data-kiln>@font-face{{font-family:"Geist Variable";src:url("/v/_/fonts/geist.woff2") format("woff2");font-weight:100 900;font-display:block}}@font-face{{font-family:"Geist Mono Variable";src:url("/v/_/fonts/geist-mono.woff2") format("woff2");font-weight:100 900;font-display:block}}:root{{{vars}--font-sans:"Geist Variable",ui-sans-serif,system-ui,sans-serif;--font-mono:"Geist Mono Variable",ui-monospace,monospace;color-scheme:dark}}html,body{{margin:0;background:var(--background);color:var(--foreground);font-family:var(--font-sans)}}html{{scrollbar-width:none}}html::-webkit-scrollbar{{display:none}}</style><script data-kiln>{BRIDGE}</script>"#
    )
}

/// The bridge, in the page (see the module's notes, and docs/previews.md).
const BRIDGE: &str = include_str!("bridge.js");

/// Just past the opening tag `name` in `lower` (`<head>`, `<head lang=...>`; not `<header>`).
fn after_tag(lower: &str, name: &str) -> Option<usize> {
    let open = format!("<{name}");
    let mut from = 0;
    while let Some(i) = lower.get(from..).and_then(|r| r.find(&open)) {
        let at = from + i;
        let next = lower.get(at + open.len()..).and_then(|r| r.chars().next());
        if next.is_some_and(|c| c == '>' || c.is_whitespace()) {
            return lower.get(at..).and_then(|r| r.find('>')).map(|g| at + g + 1);
        }
        from = at + open.len();
    }
    None
}

/// The page with kiln's theme and bridge at the top of its head (or of the document).
pub fn with_bridge(html: &str) -> String {
    let extra = head_extra();
    let lower = html.to_ascii_lowercase();
    match after_tag(&lower, "head").or_else(|| after_tag(&lower, "html")) {
        Some(i) => format!("{}{extra}{}", html.get(..i).unwrap_or(""), html.get(i..).unwrap_or("")),
        None => format!("<!doctype html><html><head>{extra}</head><body>{html}</body></html>"),
    }
}

/// The type a page's file is served as: what its name says for code, styles and data (the runner
/// calls those text, to show them in a message, and a browser runs no script and takes no style
/// served as text), else what its machine (or the web) said it is.
pub fn served_type(name: &str, found: &str) -> String {
    let path = name.split(['?', '#']).next().unwrap_or(name);
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "csv" => "text/csv; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "glb" => "model/gltf-binary",
        "gltf" => "model/gltf+json",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => found,
    }
    .to_owned()
}

/// What kind of text a file is, to find what it refers to (and rewrite those).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Text {
    Html,
    Css,
    Script,
}

fn text_kind(mime: &str) -> Option<Text> {
    let m = mime.split(';').next().unwrap_or("").trim();
    match m {
        "text/html" => Some(Text::Html),
        "text/css" => Some(Text::Css),
        "text/javascript" | "application/javascript" => Some(Text::Script),
        _ => None,
    }
}

fn refs_in(text: &str, kind: Text) -> Vec<Ref> {
    match kind {
        Text::Html => references(text),
        Text::Css => css_references(text),
        Text::Script => script_references(text),
    }
}

/// Widths an engine's heights are kept at, as its viewers measured them.
const SEEN_WIDTHS: usize = 24;

/// Published pages, kept beside the files conversations keep (one store of content).
pub struct Visuals {
    dir: PathBuf,
    blobs: PathBuf,
    cache: Mutex<HashMap<String, Visual>>,
    /// A page's heights changed one at a time (its machine's, its viewers').
    heights: Mutex<()>,
}

impl Visuals {
    pub fn new(harness_dir: &Path) -> Self {
        Self { dir: harness_dir.join("visuals"), blobs: harness_dir.join("files").join("blobs"), cache: Mutex::default(), heights: Mutex::default() }
    }

    fn file(&self, id: &str) -> Option<PathBuf> {
        (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| self.dir.join(format!("{id}.json")))
    }

    pub fn get(&self, id: &str) -> Option<Visual> {
        if let Some(v) = self.cache.lock().get(id) {
            return Some(v.clone());
        }
        let v: Visual = serde_json::from_slice(&std::fs::read(self.file(id)?).ok()?).ok()?;
        self.cache.lock().insert(id.to_owned(), v.clone());
        Some(v)
    }

    fn save(&self, v: &Visual) -> anyhow::Result<()> {
        let path = self.file(&v.id).ok_or_else(|| anyhow::anyhow!("not a page's id"))?;
        std::fs::create_dir_all(&self.dir)?;
        let tmp = path.with_extension("json.part");
        std::fs::write(&tmp, serde_json::to_vec(v)?)?;
        std::fs::rename(&tmp, &path)?;
        self.cache.lock().insert(v.id.clone(), v.clone());
        Ok(())
    }

    /// Where content `sha` is kept.
    pub fn blob(&self, sha: &str) -> PathBuf {
        self.blobs.join(sha.to_ascii_lowercase())
    }

    /// Bytes into the store (once, whatever uses them); their SHA-256.
    fn store(&self, bytes: &[u8]) -> anyhow::Result<String> {
        let sha = hex::encode(Sha256::digest(bytes));
        let path = self.blob(&sha);
        if !path.exists() {
            std::fs::create_dir_all(&self.blobs)?;
            let tmp = self.blobs.join(format!("{sha}.{}.part", crate::util::now_ms()));
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(sha)
    }

    /// The heights the agent's machine measured, for its own session's page.
    pub fn measured(&self, id: &str, session: &str, heights: Vec<(u32, u32)>, engines: std::collections::BTreeMap<String, Vec<(u32, u32)>>) -> anyhow::Result<Visual> {
        let _one = self.heights.lock();
        let mut v = self.get(id).ok_or_else(|| anyhow::anyhow!("no such page"))?;
        anyhow::ensure!(v.session == session, "that page is another conversation's");
        let sane = |hs: Vec<(u32, u32)>| {
            let mut hs: Vec<(u32, u32)> = hs.into_iter().filter(|(w, h)| *w > 0 && *w <= 10_000 && *h > 0 && *h <= 100_000).take(24).collect();
            hs.sort_unstable();
            hs
        };
        v.heights = sane(heights);
        v.engines = engines.into_iter().filter(|(e, _)| matches!(e.as_str(), "firefox" | "webkit")).map(|(e, hs)| (e, sane(hs))).filter(|(_, hs)| !hs.is_empty()).collect();
        self.save(&v)?;
        Ok(v)
    }

    /// A height a viewer's own browser measured of its conversation's page at its width (`engine`:
    /// its engine, one other than the Chromium its machine measured it in at every width): kept
    /// with the page, so the next viewer in that engine has its frame at the page's height before
    /// the page loads. At most [`SEEN_WIDTHS`] widths an engine; past that, the new one's neighbour
    /// goes.
    pub fn seen(&self, id: &str, session: &str, engine: &str, width: u32, height: u32) -> anyhow::Result<()> {
        anyhow::ensure!(matches!(engine, "firefox" | "webkit"), "heights are kept for firefox and webkit, not {engine:?}");
        anyhow::ensure!((200..=10_000).contains(&width) && (1..=100_000).contains(&height), "not a page's width and height");
        let _one = self.heights.lock();
        let mut v = self.get(id).ok_or_else(|| anyhow::anyhow!("no such page"))?;
        anyhow::ensure!(v.session == session, "that page is another conversation's");
        let hs = v.engines.entry(engine.to_owned()).or_default();
        if hs.contains(&(width, height)) {
            return Ok(());
        }
        hs.retain(|(w, _)| *w != width);
        hs.push((width, height));
        hs.sort_unstable();
        while hs.len() > SEEN_WIDTHS {
            let at = hs.iter().position(|(w, _)| *w == width).unwrap_or(0);
            let left = at.checked_sub(1).and_then(|i| hs.get(i).map(|n| (i, width.abs_diff(n.0))));
            let right = hs.get(at + 1).map(|n| (at + 1, width.abs_diff(n.0)));
            let Some((i, _)) = [left, right].into_iter().flatten().min_by_key(|(_, d)| *d) else { break };
            hs.remove(i);
        }
        self.save(&v)
    }

    /// Every content kept for a page (the keeper's sweep leaves these).
    pub fn shas(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            if let Ok(v) = serde_json::from_slice::<Visual>(&std::fs::read(e.path()).unwrap_or_default()) {
                out.extend(v.files.into_iter().map(|f| f.sha));
            }
        }
        out
    }

    /// A conversation that's closed: its pages go (their content, at the keeper's next sweep).
    pub fn forget(&self, session: &str) {
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            if serde_json::from_slice::<Visual>(&std::fs::read(e.path()).unwrap_or_default()).is_ok_and(|v| v.session == session) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        self.cache.lock().retain(|_, v| v.session != session);
    }
}

/// What an agent's MCP server asks of its conversation's pages (`crate::mcp::runner_ask`, op
/// "page"): `{"do": "publish", "title", "entry", "html", "loaded"?: {"local", "web"}}` publishes
/// one (`entry`: where the page stands on its machine; `loaded`: what kiln's browser there saw it
/// load); `{"do": "measured", "id", "heights"}` gives the heights its machine's browser measured.
/// The answer: the page, where it is.
pub async fn ask(hub: &Arc<Hub>, session: &str, q: &Value) -> anyhow::Result<Value> {
    let s = |k: &str| q.get(k).and_then(Value::as_str);
    let v = match s("do") {
        Some("publish") => {
            let html = s("html").ok_or_else(|| anyhow::anyhow!("which page? Its html is missing"))?;
            let entry = s("entry").ok_or_else(|| anyhow::anyhow!("where does the page stand? Its entry is missing"))?;
            let loaded: crate::browser::preview::Loaded = serde_json::from_value(q.get("loaded").cloned().unwrap_or_default()).unwrap_or_default();
            publish(hub, session, s("title").unwrap_or(""), entry, html, &loaded).await?
        }
        Some("measured") => {
            let heights: Vec<(u32, u32)> = serde_json::from_value(q.get("heights").cloned().unwrap_or_default()).unwrap_or_default();
            let engines: std::collections::BTreeMap<String, Vec<(u32, u32)>> = serde_json::from_value(q.get("engines").cloned().unwrap_or_default()).unwrap_or_default();
            let (h, id, session) = (hub.clone(), s("id").unwrap_or("").to_owned(), session.to_owned());
            crate::rt::blocking(move || h.harness.visuals.measured(&id, &session, heights, engines)).await??
        }
        other => anyhow::bail!("pages don't do {other:?}"),
    };
    let path = format!("/v/{}{}", v.id, url_path(&v.entry));
    Ok(serde_json::json!({
        "id": v.id,
        "title": v.title,
        "path": path,
        "url": format!("{}{path}", hub.cfg.server.pages_url.as_deref().unwrap_or(&hub.cfg.server.public_url).trim_end_matches('/')),
        "files": v.files.len().saturating_sub(1),
        "heights": v.heights,
        "engines": v.engines,
    }))
}

/// A file being kept: what it is, and (for text) its content, to look into and rewrite.
struct Kept {
    from: Target,
    sha: String,
    mime: String,
    size: u64,
    text: Option<(Text, String)>,
}

/// Publish a page for a session: every file it uses (what it and its files name, and what kiln's
/// browser saw it load) read from the session's machine or fetched from the web into the store,
/// full paths and web addresses in them rewritten to where their copies are served, kiln's bridge
/// put in.
pub async fn publish(hub: &Arc<Hub>, session: &str, title: &str, entry: &str, html: &str, loaded: &crate::browser::preview::Loaded) -> anyhow::Result<Visual> {
    anyhow::ensure!(html.len() <= MOST_PAGE, "the page is {} KB; a page may be up to {} KB (its pictures and files go beside it, by path)", html.len() / 1024, MOST_PAGE / 1024);
    let location = hub.harness.location(session).ok_or_else(|| anyhow::anyhow!("no such agent session"))?;
    let id = hex::encode(rand::random::<[u8; 16]>());
    let entry_target = Target::Local(entry.to_owned());
    let dir_of = |t: &Target| -> Base {
        match t {
            Target::Local(p) => Base::Local(Path::new(p).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default()),
            Target::Web(u) => Base::Web(u.clone()),
        }
    };
    // What to keep: the page's references, and its files' in turn, breadth first; what kiln's
    // browser saw it load besides.
    let mut kept: HashMap<Target, Kept> = HashMap::new();
    let mut queue: VecDeque<(Target, bool)> = VecDeque::new();
    let mut seen: HashSet<Target> = HashSet::new();
    let mut missing = Vec::new();
    let mut total = 0u64;
    let page_refs = references(html);
    let page_base = dir_of(&entry_target);
    for r in &page_refs {
        if let Some(t) = target(&r.value, &page_base)
            && t != entry_target
            && seen.insert(t.clone())
        {
            queue.push_back((t, r.quoted));
        }
    }
    for p in &loaded.local {
        let t = Target::Local(p.clone());
        if t != entry_target && seen.insert(t.clone()) {
            queue.push_back((t, false));
        }
    }
    for u in &loaded.web {
        let t = Target::Web(u.clone());
        if seen.insert(t.clone()) {
            queue.push_back((t, false));
        }
    }
    while let Some((t, quoted)) = queue.pop_front() {
        anyhow::ensure!(kept.len() < MOST_USED, "the page uses more than {MOST_USED} files");
        let got = match &t {
            Target::Local(path) => local_file(hub, &location, session, path).await,
            Target::Web(url) => web_file(hub, url).await,
        };
        let k = match got {
            Ok(k) => k,
            // One of the agent's files that can't be read is said (the page would show it
            // broken); one only quoted may be no file at all; one from the web that can't be
            // fetched now is left out (the page may only reach its own files).
            Err(e) => {
                match &t {
                    Target::Local(path) if !quoted => missing.push(format!("{path}: {e:#}")),
                    Target::Local(path) => tracing::debug!(path, error = %format!("{e:#}"), "a path a page quotes isn't a file it can use"),
                    Target::Web(url) => tracing::debug!(url, error = %format!("{e:#}"), "a page's file from the web couldn't be fetched"),
                }
                continue;
            }
        };
        total += k.size;
        anyhow::ensure!(total <= MOST_FILES, "the files the page uses come to more than {} MB", MOST_FILES >> 20);
        if let Some((kind, text)) = &k.text {
            let base = dir_of(&t);
            for r in refs_in(text, *kind) {
                if let Some(next) = target(&r.value, &base)
                    && next != entry_target
                    && seen.insert(next.clone())
                {
                    queue.push_back((next, r.quoted));
                }
            }
        }
        kept.insert(t, Kept { from: k.from, sha: k.sha, mime: k.mime, size: k.size, text: k.text });
    }
    anyhow::ensure!(missing.is_empty(), "files the page uses couldn't be read: {}", missing.join("; "));

    // Full paths and web addresses, in the page and its text files, to where their copies are.
    let prefix = format!("/v/{id}");
    let to_copy = |text: &str, kind: Text, base: &Base| -> (String, bool) {
        let refs = refs_in(text, kind);
        let mut to = HashMap::new();
        for r in &refs {
            let v = r.value.trim();
            let absolute = v.starts_with('/') || v.starts_with("~/") || v.starts_with("file:") || v.starts_with("http:") || v.starts_with("https:");
            if !absolute {
                continue;
            }
            if let Some(t) = target(v, base)
                && (kept.contains_key(&t) || t == entry_target)
            {
                to.insert(r.value.clone(), format!("{prefix}{}", url_path(&served_at(&t))));
            }
        }
        if to.is_empty() { (text.to_owned(), false) } else { (rewrite(text, &refs, &to), true) }
    };
    let mut files: Vec<Used> = Vec::new();
    let mut rewritten: Vec<(usize, String)> = Vec::new();
    for (t, k) in &kept {
        let mut u = Used { path: served_at(t), from: k.origin(), sha: k.sha.clone(), mime: k.mime.clone(), size: k.size };
        if let Some((kind, text)) = &k.text {
            let (new, changed) = to_copy(text, *kind, &dir_of(t));
            if changed {
                u.size = new.len() as u64;
                rewritten.push((files.len(), new));
            }
        }
        files.push(u);
    }
    let (page, _) = to_copy(html, Text::Html, &page_base);
    let page = with_bridge(&page);
    let v = Visual { id, session: session.to_owned(), title: title.trim().chars().take(200).collect::<String>(), entry: served_at(&entry_target), files, heights: Vec::new(), engines: std::collections::BTreeMap::new(), at: crate::util::now_ms() };
    let h = hub.clone();
    let entry_from = entry.to_owned();
    crate::rt::blocking(move || -> anyhow::Result<Visual> {
        let visuals = &h.harness.visuals;
        let mut v = v;
        for (i, text) in rewritten {
            let sha = visuals.store(text.as_bytes())?;
            if let Some(f) = v.files.get_mut(i) {
                f.sha = sha;
            }
        }
        let sha = visuals.store(page.as_bytes())?;
        v.files.push(Used { path: v.entry.clone(), from: entry_from, sha, mime: "text/html; charset=utf-8".to_owned(), size: page.len() as u64 });
        visuals.save(&v)?;
        Ok(v)
    })
    .await?
}

impl Kept {
    fn origin(&self) -> String {
        match &self.from {
            Target::Local(p) => p.clone(),
            Target::Web(u) => u.clone(),
        }
    }
}

/// A file of the agent's into the store (its text kept to look into, for a page's code and styles).
async fn local_file(hub: &Arc<Hub>, location: &super::registry::Location, session: &str, path: &str) -> anyhow::Result<Kept> {
    let (sha, meta) = super::kept::store_file(hub, location, session, path, MOST_FILE).await?;
    let mime = served_type(path, &meta.mime);
    let text = match text_kind(&mime) {
        Some(kind) => {
            let blob = hub.harness.visuals.blob(&sha);
            Some((kind, crate::rt::blocking(move || std::fs::read_to_string(blob)).await??))
        }
        None => None,
    };
    Ok(Kept { from: Target::Local(path.to_owned()), sha, mime, size: meta.size, text })
}

/// A file from the web (a library a page loads), fetched once by the server and kept. Only from
/// the public internet: never this machine, its networks, or a cloud's metadata service.
async fn web_file(hub: &Arc<Hub>, url: &str) -> anyhow::Result<Kept> {
    let client = crate::http::client::Client::public()?;
    let reach = crate::http::io::Reach::default();
    let mut at = url.to_owned();
    for _ in 0..6 {
        let resp = client.get(&at).reaching(&reach).timeout(Duration::from_secs(30)).send().await?;
        if matches!(resp.status(), 301 | 302 | 303 | 307 | 308) {
            let to = resp.header("location").ok_or_else(|| anyhow::anyhow!("{at} sent elsewhere, but not where"))?;
            at = joined(&at, to);
            continue;
        }
        anyhow::ensure!(resp.is_success(), "{at} answered {}", resp.status());
        let mime = served_type(&at, resp.header("content-type").unwrap_or("application/octet-stream"));
        let body = resp.bytes(usize::try_from(MOST_FILE).unwrap_or(usize::MAX)).await?;
        let size = body.len() as u64;
        let text = text_kind(&mime).map(|k| (k, String::from_utf8_lossy(&body).into_owned()));
        let h = hub.clone();
        let sha = crate::rt::blocking(move || h.harness.visuals.store(&body)).await??;
        return Ok(Kept { from: Target::Web(url.to_owned()), sha, mime, size, text });
    }
    anyhow::bail!("{url} sent elsewhere too many times")
}

/// Where a reference `to` in something at `from` leads (an address, or a path on its site).
fn joined(from: &str, to: &str) -> String {
    if to.starts_with("https://") || to.starts_with("http://") {
        return to.to_owned();
    }
    let scheme_end = from.find("://").map_or(0, |i| i + 3);
    let scheme = from.get(..scheme_end).unwrap_or("https://");
    if let Some(rest) = to.strip_prefix("//") {
        return format!("{scheme}{rest}");
    }
    let host_end = from.get(scheme_end..).and_then(|r| r.find('/')).map_or(from.len(), |i| scheme_end + i);
    let origin = from.get(..host_end).unwrap_or(from);
    let path = if to.starts_with('/') {
        to.to_owned()
    } else {
        let here = from.get(host_end..).unwrap_or("/").split(['?', '#']).next().unwrap_or("/");
        let dir = here.rsplit_once('/').map_or("", |(d, _)| d);
        format!("{dir}/{to}")
    };
    // Dots resolved, as a browser does.
    let (path, query) = path.split_once('?').map_or((path.as_str(), None), |(p, q)| (p, Some(q)));
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/').skip(1) {
        match seg {
            "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut out = format!("{origin}/{}", parts.join("/"));
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_viewers_own_engine_measures_its_page_at_its_width() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-visuals-seen-{}", rand::random::<u64>()));
        let vs = Visuals::new(&dir);
        let id = "0123456789abcdef0123456789abcdef".to_owned();
        vs.save(&Visual { id: id.clone(), session: "s1".into(), title: "t".into(), entry: "/p.html".into(), files: Vec::new(), heights: vec![(390, 500)], engines: std::collections::BTreeMap::new(), at: 0 })?;
        vs.seen(&id, "s1", "webkit", 390, 512)?;
        vs.seen(&id, "s1", "webkit", 1000, 300)?;
        // The same width again: the newer.
        vs.seen(&id, "s1", "webkit", 390, 520)?;
        assert_eq!(vs.get(&id).map(|v| v.engines.get("webkit").cloned()), Some(Some(vec![(390, 520), (1000, 300)])));
        // Not Chromium's (its machine measured those), not nonsense, not another conversation's.
        assert!(vs.seen(&id, "s1", "chromium", 390, 1).is_err());
        assert!(vs.seen(&id, "s1", "webkit", 390, 0).is_err() && vs.seen(&id, "s1", "webkit", 20, 100).is_err());
        assert!(vs.seen(&id, "s2", "webkit", 390, 100).is_err());
        // At most so many widths: the new one's neighbour goes.
        for w in 0..30u32 {
            vs.seen(&id, "s1", "firefox", 300 + w * 10, 100 + w)?;
        }
        let ff = vs.get(&id).and_then(|v| v.engines.get("firefox").cloned()).unwrap_or_default();
        assert_eq!(ff.len(), SEEN_WIDTHS);
        assert!(ff.contains(&(590, 129)) && ff.windows(2).all(|p| p.first().map(|a| a.0) < p.get(1).map(|b| b.0)), "{ff:?}");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn a_pages_references_are_its_loads_imports_and_quoted_full_paths() {
        let html = r#"<html><head><link rel="stylesheet" href="./style.css"><script src="https://cdn.example.com/chart.js"></script></head>
<body data-src="/not/this.png"><img src="/tmp/shot.png" srcset="/tmp/shot.png 1x, /tmp/shot@2x.png 2x" alt="x">
<div style="background: url('/tmp/bg.webp')"></div><p>The file /tmp/shot.png is shown above.</p>
<script type="module">import { a } from "./lib.js"; const model = "/home/ada/m.glb"; const word = "/not a path.png"; const route = "/api/runs";</script></body></html>"#;
        let refs = references(html);
        let values: Vec<&str> = refs.iter().map(|r| r.value.as_str()).collect();
        assert_eq!(values, vec!["./style.css", "https://cdn.example.com/chart.js", "/not/this.png", "/tmp/shot.png", "/tmp/shot.png", "/tmp/shot@2x.png", "/tmp/bg.webp", "./lib.js", "/home/ada/m.glb"]);
        let quoted: Vec<bool> = refs.iter().map(|r| r.quoted).collect();
        assert_eq!(quoted, vec![false, false, true, false, false, false, false, false, true], "a path only quoted (data-src, a script's) may not be a file");
    }

    #[test]
    fn a_modules_imports_are_what_it_loads() {
        let js = r#"import x from "./a.js"; export { y } from '../b.mjs'; const c = await import("./c.js"); import "https://cdn.example/d.js"; const s = "./not-an-import.js";"#;
        let v: Vec<String> = script_references(js).into_iter().map(|r| r.value).collect();
        assert_eq!(v, vec!["./a.js", "../b.mjs", "./c.js", "https://cdn.example/d.js"]);
    }

    #[test]
    fn rewriting_touches_the_references_and_nothing_else() {
        let html = r#"<img src="/tmp/shot.png"><p>/tmp/shot.png</p><img src='/tmp/shot.png'>"#;
        let refs = references(html);
        let to = HashMap::from([("/tmp/shot.png".to_owned(), "/v/x/tmp/shot.png".to_owned())]);
        assert_eq!(rewrite(html, &refs, &to), r#"<img src="/v/x/tmp/shot.png"><p>/tmp/shot.png</p><img src='/v/x/tmp/shot.png'>"#);
    }

    #[test]
    fn a_reference_leads_to_a_file_of_the_agents_or_the_web() {
        let here = Base::Local("/home/ada/page".into());
        assert_eq!(target("./style.css", &here), Some(Target::Local("/home/ada/page/style.css".into())));
        assert_eq!(target("/tmp/a%20b.png", &here), Some(Target::Local("/tmp/a b.png".into())));
        assert_eq!(target("//cdn.example.com/x.js", &here), Some(Target::Web("https://cdn.example.com/x.js".into())));
        assert_eq!(target("data:image/png;base64,AAAA", &here), None);
        assert_eq!(target("#top", &here), None);
        // In a file from the web, a path is on its site.
        let web = Base::Web("https://cdn.example.com/npm/lib@1/dist/lib.css".into());
        assert_eq!(target("../fonts/a.woff2", &web), Some(Target::Web("https://cdn.example.com/npm/lib@1/fonts/a.woff2".into())));
        assert_eq!(target("/npm/other@2/x.js", &web), Some(Target::Web("https://cdn.example.com/npm/other@2/x.js".into())));
    }

    #[test]
    fn files_are_served_where_they_were() {
        assert_eq!(served_at(&Target::Local("/home/ada/page/style.css".into())), "/home/ada/page/style.css");
        assert_eq!(served_at(&Target::Web("https://cdn.example.com/npm/lib@1/lib.js".into())), "/_web/https/cdn.example.com/npm/lib@1/lib.js");
        assert_eq!(served_at(&Target::Web("https://fonts.example.com".into())), "/_web/https/fonts.example.com/index");
        let q = served_at(&Target::Web("https://api.example.com/data.json?v=2".into()));
        assert!(q.starts_with("/_web/https/api.example.com/data.json~") && q.len() == "/_web/https/api.example.com/data.json~".len() + 12, "{q}");
    }

    #[test]
    fn the_bridge_goes_first_in_the_head_or_wraps_a_fragment() {
        let full = with_bridge("<!doctype html><html><head><title>t</title></head><body>hi</body></html>");
        let i = full.find("<script data-kiln>").unwrap_or(usize::MAX);
        assert!(i < full.find("<title>").unwrap_or(0), "the bridge comes before the page's own head");
        let bare = with_bridge("<p>hi</p>");
        assert!(bare.starts_with("<!doctype html><html><head>") && bare.contains("<body><p>hi</p></body>"));
        // A page with a header but no head: not inside its header.
        let header = with_bridge("<html><body><header>top</header></body></html>");
        assert!(header.starts_with("<html><meta name=\"color-scheme\""), "{}", header.get(..60).unwrap_or(""));
    }

    #[test]
    fn styles_references_are_found_quoted_or_not() {
        let css = r#"@import "base.css"; @font-face { src: url("fonts/a.woff2") format("woff2"); } .x { background: url(/tmp/b.png) } .y { mask: url('c.svg') }"#;
        let v: Vec<String> = css_references(css).into_iter().map(|r| r.value).collect();
        assert_eq!(v, vec!["fonts/a.woff2", "/tmp/b.png", "c.svg", "base.css"]);
    }

    #[test]
    fn code_styles_and_data_are_served_as_what_they_are() {
        assert_eq!(served_type("/w/style.css", "text/plain; charset=utf-8"), "text/css; charset=utf-8");
        assert_eq!(served_type("https://cdn.example/d3@7/d3.min.js?v=1", "text/plain"), "text/javascript; charset=utf-8");
        assert_eq!(served_type("/w/model.glb", "model/gltf-binary"), "model/gltf-binary");
        assert_eq!(served_type("/w/shot.png", "image/png"), "image/png");
    }

    #[test]
    fn a_redirect_leads_where_it_says() {
        assert_eq!(joined("https://unpkg.com/d3", "/d3@7.9.0/dist/d3.min.js"), "https://unpkg.com/d3@7.9.0/dist/d3.min.js");
        assert_eq!(joined("https://a.example/x/y.js", "z.js"), "https://a.example/x/z.js");
        assert_eq!(joined("https://a.example/x/y/z.css", "../f/a.woff2"), "https://a.example/x/f/a.woff2");
        assert_eq!(joined("https://a.example/x", "//b.example/q"), "https://b.example/q");
        assert_eq!(joined("https://a.example/x", "https://c.example/"), "https://c.example/");
    }
}
