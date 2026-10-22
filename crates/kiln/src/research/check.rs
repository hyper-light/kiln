//! kiln's look for each quote a finding cites (docs/research.md, "Checks"), so a citation counts
//! as far as it holds up rather than as far as the agent says:
//!
//! - a file, or a page on the web, is read on the agent's own machine (`crate::workspace`'s
//!   questions to it), where the agent read it: kiln reaches nothing the agent couldn't, and a page
//!   only on the public internet (`crate::http::io::Reach`), each redirect checked again;
//! - what a measurement printed is looked for in the agent's own conversation, among its tools'
//!   output; a record of kiln's in kiln (a plan's words, an agent's conversation).
//!
//! A quote is found when its words are there in order, however they're spaced, in any case, with
//! curly quotes and dashes as plain ones (`…` or `...` in a quote stands for words left out).
//! What can't be read (a PDF, a page its scripts draw, one behind a sign-in, a machine whose kiln is
//! older) is left unchecked, saying why: it counts as quoted, not as found.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Check, CheckState, Type};
use crate::hub::Hub;

/// Looks one question to a machine carries, and how long a page may take.
const BATCH: usize = 6;
const PAGE_TIME: Duration = Duration::from_secs(15);
/// The most of a page, and of a file, read.
const PAGE_MAX: usize = 8 << 20;
const FILE_MAX: u64 = 16 << 20;
/// Redirects followed.
const HOPS: usize = 5;
/// A page with fewer words than this is drawn by its scripts: what it says isn't in it.
const THIN: usize = 400;

/// One look a machine is asked for: a file (relative to the agent's folder) or a page, and the
/// words to look for in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Look {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
}

/// How one look came out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub state: CheckState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
}

impl Seen {
    fn of(state: CheckState, said: impl Into<Option<String>>) -> Self {
        Self { state, said: said.into() }
    }
}

/// One citation's look, as research takes it back.
#[derive(Debug, Clone)]
pub struct Result {
    pub finding: String,
    /// Where it is among its finding's citations, then its counterclaims'.
    pub index: usize,
    pub source: String,
    pub quote: Option<String>,
    pub check: Check,
}

// ------------------------------------------------------------------ words

/// Text as quotes are looked for in it: lower case, curly quotes and dashes as plain ones, spaces
/// (any, any number) as one.
pub fn norm(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        let c = match ch {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' | '`' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2026}' => {
                out.push_str("...");
                space = false;
                continue;
            }
            c if c.is_whitespace() => ' ',
            c => c,
        };
        if c == ' ' {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.extend(c.to_lowercase());
    }
    out
}

/// Whether `text` holds `quote`: its words in order (`...` standing for words left out).
pub fn holds(text: &str, quote: &str) -> bool {
    let t = norm(text);
    let q = norm(quote);
    let pieces: Vec<&str> = q.split("...").map(str::trim).filter(|p| !p.is_empty()).collect();
    if pieces.is_empty() {
        return false;
    }
    let mut at = 0;
    for p in pieces {
        match t.get(at..).and_then(|rest| rest.find(p)) {
            Some(i) => at += i + p.len(),
            None => return false,
        }
    }
    true
}

/// A page's words: its markup, scripts, styles and comments gone, its entities read.
pub fn html_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'<') {
            let rest = lower.get(i..).unwrap_or("");
            let skip_to = |end: &str| rest.find(end).map_or(bytes.len(), |e| i + e + end.len());
            i = if rest.starts_with("<!--") {
                skip_to("-->")
            } else if rest.starts_with("<script") {
                skip_to("</script>")
            } else if rest.starts_with("<style") {
                skip_to("</style>")
            } else {
                skip_to(">")
            };
            out.push(' ');
            continue;
        }
        let next = html.get(i..).and_then(|r| r.find('<')).map_or(html.len(), |n| i + n);
        out.push_str(&entities(html.get(i..next).unwrap_or("")));
        i = next;
    }
    out
}

/// HTML's entities read: the named ones prose uses, and any by number.
fn entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or(""));
        let after = rest.get(at + 1..).unwrap_or("");
        let end = after.find(';').filter(|e| *e <= 10);
        let name = end.and_then(|e| after.get(..e)).unwrap_or("");
        let ch = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "ndash" | "mdash" | "minus" => Some('-'),
            "lsquo" | "rsquo" => Some('\''),
            "ldquo" | "rdquo" => Some('"'),
            "hellip" => Some('\u{2026}'),
            n if n.starts_with("#x") || n.starts_with("#X") => n.get(2..).and_then(|h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32),
            n if n.starts_with('#') => n.get(1..).and_then(|d| d.parse::<u32>().ok()).and_then(char::from_u32),
            _ => None,
        };
        match (ch, end) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = after.get(e + 1..).unwrap_or("");
            }
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ------------------------------------------------------------------ on the agent's machine

/// The looks a machine was asked for, on that machine: each file read under `cwd` (the agent's
/// folder), each page fetched, and the quote looked for in it.
pub async fn local(cwd: &Path, looks: Vec<Look>) -> Vec<Seen> {
    let mut out = Vec::with_capacity(looks.len());
    for l in looks.into_iter().take(BATCH * 2) {
        let seen = match (l.url, l.path) {
            (Some(url), _) => page(&url, l.quote.as_deref()).await,
            (None, Some(path)) => file(cwd, &path, l.quote).await,
            (None, None) => Seen::of(CheckState::Unchecked, "nothing to look at".to_owned()),
        };
        out.push(seen);
    }
    out
}

/// A file's path, as a locator gives it: without the lines it points to; `~` the home folder;
/// relative to the agent's folder.
fn path_of(cwd: &Path, locator: &str) -> PathBuf {
    let l = locator.trim();
    let l = l.rsplit_once(':').filter(|(head, tail)| !head.is_empty() && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c == '-')).map_or(l, |(head, _)| head);
    if let Some(rest) = l.strip_prefix("~/") {
        return crate::os::home_dir().join(rest);
    }
    let p = Path::new(l);
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

async fn file(cwd: &Path, locator: &str, quote: Option<String>) -> Seen {
    let path = path_of(cwd, locator);
    let looked = crate::rt::blocking(move || {
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => return Seen::of(CheckState::Gone, format!("{} isn't there", path.display())),
        };
        if meta.is_dir() {
            return Seen::of(CheckState::Exists, "a folder".to_owned());
        }
        if meta.len() > FILE_MAX {
            return Seen::of(CheckState::Unchecked, "too big to read through here".to_owned());
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return Seen::of(CheckState::Unchecked, format!("couldn't read it: {e}")),
        };
        if bytes.get(..8192).unwrap_or(&bytes).contains(&0) {
            return Seen::of(CheckState::Exists, "not text: there, its content not looked through".to_owned());
        }
        match quote {
            None => Seen::of(CheckState::Exists, None),
            Some(q) if holds(&String::from_utf8_lossy(&bytes), &q) => Seen::of(CheckState::Found, None),
            Some(_) => Seen::of(CheckState::NotFound, "the file doesn't hold those words".to_owned()),
        }
    })
    .await;
    looked.unwrap_or_else(|e| Seen::of(CheckState::Unchecked, format!("{e:#}")))
}

/// Where a redirect goes, from `url`.
fn resolve(url: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_owned();
    }
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let host = rest.split('/').next().unwrap_or(rest);
    if let Some(abs) = location.strip_prefix("//") {
        return format!("{scheme}://{abs}");
    }
    if location.starts_with('/') {
        return format!("{scheme}://{host}{location}");
    }
    let base = rest.rsplit_once('/').map_or(rest, |(b, _)| b);
    format!("{scheme}://{base}/{location}")
}

async fn page(url: &str, quote: Option<&str>) -> Seen {
    let url = match url.trim() {
        u if u.starts_with("doi:") => format!("https://doi.org/{}", u.trim_start_matches("doi:").trim()),
        u => u.to_owned(),
    };
    let client = match crate::http::client::Client::public() {
        Ok(c) => c,
        Err(e) => return Seen::of(CheckState::Unchecked, format!("no web client here: {e:#}")),
    };
    let reach = crate::http::io::Reach::default();
    let mut at = url;
    for _ in 0..=HOPS {
        let got = client.get(&at).reaching(&reach).header("accept", "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5").timeout(PAGE_TIME).send().await;
        let resp = match got {
            Ok(r) => r,
            Err(e) => {
                let why = format!("{e:#}");
                let private = why.contains("private address");
                return Seen::of(CheckState::Unchecked, if private { "a private address: kiln reads only public pages".to_owned() } else { format!("couldn't reach it: {why}") });
            }
        };
        let status = resp.status();
        if (300..400).contains(&status) {
            match resp.header("location") {
                Some(l) => {
                    at = resolve(&at, l);
                    continue;
                }
                None => return Seen::of(CheckState::Unchecked, format!("HTTP {status} going nowhere")),
            }
        }
        if matches!(status, 404 | 410) {
            return Seen::of(CheckState::Gone, format!("HTTP {status}"));
        }
        if matches!(status, 401 | 403) {
            return Seen::of(CheckState::Unchecked, format!("HTTP {status}: it needs a sign-in, or turned kiln away"));
        }
        if !resp.is_success() {
            return Seen::of(CheckState::Unchecked, format!("HTTP {status}"));
        }
        let kind = resp.header("content-type").unwrap_or("").to_ascii_lowercase();
        if kind.contains("pdf") {
            return Seen::of(CheckState::Unchecked, "a PDF: its words aren't read here".to_owned());
        }
        let html = kind.contains("html") || kind.is_empty();
        let text_like = html || kind.starts_with("text/") || kind.contains("json") || kind.contains("xml");
        if !text_like {
            return Seen::of(CheckState::Exists, format!("there ({kind}), not words to look through"));
        }
        let body = match resp.bytes(PAGE_MAX).await {
            Ok(b) => b,
            Err(e) => return Seen::of(CheckState::Unchecked, format!("couldn't read it all: {e:#}")),
        };
        let raw = String::from_utf8_lossy(&body);
        let text = if html { html_text(&raw) } else { raw.into_owned() };
        return match quote {
            None => Seen::of(CheckState::Exists, None),
            Some(q) if holds(&text, q) => Seen::of(CheckState::Found, None),
            Some(_) if html && norm(&text).len() < THIN => Seen::of(CheckState::Unchecked, "the page draws its words with scripts".to_owned()),
            Some(_) => Seen::of(CheckState::NotFound, "the page doesn't hold those words".to_owned()),
        };
    }
    Seen::of(CheckState::Unchecked, "too many redirects".to_owned())
}

// ------------------------------------------------------------------ on the server

/// What a source's citation is checked by: the agent's machine (a file, a page), kiln itself (a
/// measurement in the agent's conversation, a record), or nothing.
enum By {
    Machine(Look),
    Measurement,
    Record(String),
    Nothing(&'static str),
}

/// How `source` is checked, the agent working on machine `agent_machine` (none: no agent's).
fn by(source: &super::Source, quote: Option<&str>, agent_machine: Option<&str>) -> By {
    let l = source.locator.trim();
    let web = l.starts_with("https://") || l.starts_with("http://") || l.starts_with("doi:");
    match source.kind {
        Type::Measurement => By::Measurement,
        Type::Record => By::Record(l.to_owned()),
        _ if web => By::Machine(Look { url: Some(l.to_owned()), quote: quote.map(str::to_owned), path: None }),
        k if k.internal() && !l.is_empty() => {
            // A file on another machine than the agent's isn't looked for.
            if source.machine.as_deref().map(str::trim).filter(|m| !m.is_empty()).is_some_and(|m| agent_machine != Some(m)) {
                return By::Nothing("on another machine than the agent's");
            }
            By::Machine(Look { path: Some(l.to_owned()), quote: quote.map(str::to_owned), url: None })
        }
        _ => By::Nothing("nothing to fetch or read: no link or file"),
    }
}

/// The words of a conversation's events (each event's text, its tools' output) that hold `quote`,
/// in agent `agent`'s conversation: whether any does.
async fn in_conversation(hub: &Hub, agent: &str, quote: &str) -> Option<bool> {
    let store = hub.harness.convo.clone()?;
    let words: Vec<&str> = quote.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).take(8).collect();
    let q = crate::convo::store::Query { q: words.join(" "), session: Some(agent.to_owned()), limit: Some(50), ..Default::default() };
    let rows = crate::rt::blocking(move || store.search(q)).await.ok()?.ok()?;
    let quote = quote.to_owned();
    Some(rows.iter().any(|r| {
        let mut text = String::new();
        if let Ok(v) = serde_json::from_str::<Value>(&r.body) {
            strings(&v, &mut text);
        }
        holds(&text, &quote)
    }))
}

/// Every string in `v`, one after another.
fn strings(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

async fn record(hub: &Hub, locator: &str, quote: Option<&str>) -> Seen {
    let Some(q) = quote else { return Seen::of(CheckState::Unchecked, "nothing quoted".to_owned()) };
    if let Some(id) = locator.strip_prefix("plan:") {
        return match crate::plans::get(hub, id.trim()).await {
            Ok(p) if holds(&crate::plans::markdown(&p), q) => Seen::of(CheckState::Found, None),
            Ok(_) => Seen::of(CheckState::NotFound, "the plan doesn't hold those words".to_owned()),
            Err(_) => Seen::of(CheckState::Gone, "no such plan".to_owned()),
        };
    }
    let agent = locator.strip_prefix("agent:").or_else(|| locator.strip_prefix("conversation:"));
    match agent {
        Some(a) => match in_conversation(hub, a.trim(), q).await {
            Some(true) => Seen::of(CheckState::Found, None),
            Some(false) => Seen::of(CheckState::NotFound, "that conversation doesn't hold those words".to_owned()),
            None => Seen::of(CheckState::Unchecked, "no conversations are kept here".to_owned()),
        },
        None => Seen::of(CheckState::Unchecked, "a record kiln can't open: name it plan:<id> or agent:<id>".to_owned()),
    }
}

/// Looks for research `id`'s quotes still to be looked for, and writes how each came out.
pub async fn run(hub: &Arc<Hub>, id: &str) {
    let Ok(r) = super::get(hub, id).await else { return };
    let now = crate::util::now_ms;
    let info = r.agent.as_deref().and_then(|a| hub.harness.info(a));
    let agent_machine = info.as_ref().map(|i| crate::harness::wait::machine(&i.target).to_owned());
    let mut results: Vec<Result> = Vec::new();
    let mut machine: Vec<(Result, Look)> = Vec::new();
    for f in &r.findings {
        let cites = f.cites.iter().chain(f.against.iter().flat_map(|k| k.cites.iter()));
        for (index, c) in cites.enumerate() {
            if c.check.as_ref().is_none_or(|k| k.state != CheckState::Pending) {
                continue;
            }
            let Some(source) = r.sources.iter().find(|s| s.id == c.source) else { continue };
            let base = |check: Check| Result { finding: f.id.clone(), index, source: c.source.clone(), quote: c.quote.clone(), check };
            match by(source, c.quote.as_deref(), agent_machine.as_deref()) {
                By::Machine(look) => machine.push((base(Check { state: CheckState::Pending, at: now(), said: None }), look)),
                By::Measurement => {
                    let agent = r.agent.clone().unwrap_or_default();
                    let seen = match (c.quote.as_deref(), agent.is_empty()) {
                        (None, _) => Seen::of(CheckState::Unchecked, "nothing quoted of what it printed".to_owned()),
                        (Some(_), true) => Seen::of(CheckState::Unchecked, "no agent's conversation to look in".to_owned()),
                        (Some(q), false) => match in_conversation(hub, &agent, q).await {
                            Some(true) => Seen::of(CheckState::Found, "in the agent's own tool output".to_owned()),
                            Some(false) => Seen::of(CheckState::NotFound, "not in the agent's tool output".to_owned()),
                            None => Seen::of(CheckState::Unchecked, "no conversations are kept here".to_owned()),
                        },
                    };
                    results.push(base(Check { state: seen.state, at: now(), said: seen.said }));
                }
                By::Record(locator) => {
                    let seen = record(hub, &locator, c.quote.as_deref()).await;
                    results.push(base(Check { state: seen.state, at: now(), said: seen.said }));
                }
                By::Nothing(why) => results.push(base(Check { state: CheckState::Unchecked, at: now(), said: Some(why.to_owned()) })),
            }
        }
    }
    // Files and pages, on the agent's machine, a few at a time; with no agent (a person's
    // research), pages from the server, files not.
    for batch in machine.chunks(BATCH) {
        let looks: Vec<Look> = batch.iter().map(|(_, l)| l.clone()).collect();
        let seen: Vec<Seen> = match &info {
            Some(info) => match crate::workspace::change::ask_machine(hub, info, &crate::workspace::Ask::Cite { looks: looks.clone() }).await {
                Ok(v) => serde_json::from_value(v.get("looks").cloned().unwrap_or(Value::Null)).unwrap_or_default(),
                Err((_, why)) => looks.iter().map(|_| Seen::of(CheckState::Unchecked, format!("its machine couldn't look: {why}"))).collect(),
            },
            None => {
                let mut v = Vec::new();
                for l in &looks {
                    v.push(match &l.url {
                        Some(u) => page(u, l.quote.as_deref()).await,
                        None => Seen::of(CheckState::Unchecked, "no agent's machine to read the file on".to_owned()),
                    });
                }
                v
            }
        };
        for (i, (res, _)) in batch.iter().enumerate() {
            let s = seen.get(i).cloned().unwrap_or_else(|| Seen::of(CheckState::Unchecked, "its machine didn't say".to_owned()));
            results.push(Result { check: Check { state: s.state, at: now(), said: s.said }, ..res.clone() });
        }
    }
    if let Err(e) = super::checked(hub, id, results).await {
        tracing::warn!(research = %id, error = %e, "research checks");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_is_found_however_it_is_spaced_or_cased_and_with_words_left_out() {
        let text = "fn search(&self,\n        q: Query) -> Result<Vec<Row>> {\n  // the “newest” first — by an index\n}";
        assert!(holds(text, "fn search(&self, q: Query)"));
        assert!(holds(text, "the \"newest\" first - by an index"));
        assert!(holds(text, "FN SEARCH(&SELF, ... by an index"));
        assert!(holds(text, "fn search … newest"));
        assert!(!holds(text, "by an index ... fn search"), "in order");
        assert!(!holds(text, "fn find"));
        assert!(!holds(text, "   "), "nothing to look for");
    }

    #[test]
    fn a_page_s_words_are_its_text_not_its_markup() {
        let html = "<html><head><style>p{color:red}</style><script>var x = 'Trigram';</script></head><body><!-- not this --><h1>The&nbsp;Trigram Tokenizer</h1><p>It supports <b>substring</b> &amp; &#x4C;IKE &ldquo;matches&rdquo; &#8212; fast.</p></body></html>";
        let t = html_text(html);
        assert!(holds(&t, "The Trigram Tokenizer It supports substring & LIKE \"matches\" - fast."), "{t}");
        assert!(!t.contains("color:red") && !t.contains("var x") && !t.contains("not this"), "{t}");
        assert_eq!(entities("a &unknown; b & c"), "a &unknown; b & c");
    }

    #[test]
    fn a_redirect_is_followed_where_it_says() {
        assert_eq!(resolve("https://a.org/x/y", "https://b.org/z"), "https://b.org/z");
        assert_eq!(resolve("https://a.org/x/y", "/z"), "https://a.org/z");
        assert_eq!(resolve("https://a.org/x/y", "z"), "https://a.org/x/z");
        assert_eq!(resolve("https://a.org/x/y", "//c.org/q"), "https://c.org/q");
    }

    #[test]
    fn files_are_found_by_where_their_locator_says_and_looked_through() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("a.rs"), "pub fn search(&self, q: Query) {}\n")?;
        std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 3])?;
        assert_eq!(path_of(dir.path(), "a.rs:1-3"), dir.path().join("a.rs"));
        let looks = vec![
            Look { path: Some("a.rs:1".into()), quote: Some("pub fn search(&self, q: Query)".into()), url: None },
            Look { path: Some("a.rs".into()), quote: Some("fn find".into()), url: None },
            Look { path: Some("gone.rs".into()), quote: Some("x".into()), url: None },
            Look { path: Some("b.bin".into()), quote: Some("x".into()), url: None },
            Look { path: Some("a.rs".into()), quote: None, url: None },
        ];
        let at = dir.path().to_path_buf();
        let seen = crate::rt::block_on(async move { anyhow::Ok(local(&at, looks).await) })?;
        let states: Vec<CheckState> = seen.iter().map(|s| s.state).collect();
        assert_eq!(states, [CheckState::Found, CheckState::NotFound, CheckState::Gone, CheckState::Exists, CheckState::Exists]);
        Ok(())
    }
}
