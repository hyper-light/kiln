//! What kiln's UI receives over the wire, against a real kiln server, and byte budgets on it, so a
//! change that sends more fails here (W21-R15: presses answer at once, which a page still loading
//! megabytes can't). T3 Code keeps budgets the same way
//! (`apps/server/integration/transferBudgetV2.integration.test.ts`,
//! `docs/internals/performance-regressions.md`); kiln's are measured on its own encoding, not the UI
//! mock's.
//!
//! Four workloads, each read as the UI reads it:
//!
//! - an agent page's cold open: a session's socket as `ui/src/api/harness.ts` attaches
//!   (`TAKES`: `?z=gzip&patch=state&patch=changes&window=1&lean=1`), without gzip, and as a tab from
//!   before the bounded window (no `window`, no `lean`), to its `synced`;
//! - a tab's `/api/events` while agents work and CI runs (`ui/src/api/events.ts`);
//! - Ship while CI is busy: the GETs Ship's list and an opened row make, reloaded on their events
//!   at most once a second per key and on their 15 s polls, as `ui/src/lib/resource.ts` does;
//! - the main pages' largest GETs (agents, runs, Ship's rows, runners, the overview), and what gzip
//!   would make of each.
//!
//! The sessions are a stand-in Claude Code (`tests/fixtures/fake-claude-replay`) replaying sessions
//! shaped like kiln's own: see [`Shape`] for where each number comes from. Their text is kiln's own
//! source, so it compresses as code and command output do.
//!
//! Every request sends what a browser sends (`Accept-Encoding`, `Sec-WebSocket-Extensions`), so when
//! the server compresses, these numbers fall without a change here. Each budget says what it
//! measures, today's value and which track is to lower it.
//!
//! `cargo test -p kiln --test wire_budgets -- --nocapture` prints the tables.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, start};
use serde_json::{Value, json};

// ------------------------------------------------------------------------------------- workloads

/// A Claude Code session's shape. Measured on 2026-11-04 over the 118 Claude Code sessions of kiln's
/// own development on adacachy (`~/.claude/projects/-home-adalundhe-Projects-kiln/*.jsonl`): tool
/// calls, assistant text blocks, the person's prompts and the distinct files edited, per session.
/// The median session and one at the 90th percentile of each (each count's own p90, so a long
/// session rather than any one session).
struct Shape {
    name: &'static str,
    tools: usize,
    texts: usize,
    turns: usize,
    files: usize,
}

const MEDIAN: Shape = Shape { name: "median", tools: 232, texts: 93, turns: 5, files: 10 };
const LONG: Shape = Shape { name: "p90", tools: 1840, texts: 597, turns: 39, files: 62 };

/// Sizes as (median, mean) bytes, from the same sessions: each tool's result text and its input's
/// JSON, the assistant's text blocks and the person's prompts. [`sized`] makes nine in ten the
/// median and the tenth what brings the mean to the mean, so both hold.
const BASH_OUT: (usize, usize) = (763, 1591);
const BASH_IN: (usize, usize) = (325, 678);
/// Every tool not named here (MCP tools, mostly kiln's `run` and `output`, then ToolSearch, Agent...).
const OTHER_OUT: (usize, usize) = (310, 2265);
const OTHER_IN: (usize, usize) = (212, 561);
const READ_OUT: (usize, usize) = (1045, 3025);
/// A new file's content (Write's input).
const WRITE_IN: (usize, usize) = (2742, 5447);
/// An edit's `structuredPatch` lines (one hunk, 3 lines of context each side): median 10, mean 18.6.
const EDIT_LINES: (usize, usize) = (10, 19);
const TEXT: (usize, usize) = (150, 422);
const PROMPT: (usize, usize) = (658, 1747);

/// The tools' shares of every call in those sessions (Bash 70.2%, the rest 17.7%, Read 6.2%,
/// Edit and MultiEdit 4.2%, Write 1.8%).
const SHARES: [(Tool, f64); 5] = [(Tool::Bash, 0.702), (Tool::Other, 0.177), (Tool::Read, 0.062), (Tool::Edit, 0.042), (Tool::Write, 0.018)];

/// How fast a working agent says things: 18.71 records (an assistant block or a tool's result) a
/// minute of active work, the median over those sessions (gaps under a minute; a person's pause or
/// a long wait left out). An agent of the events workload says one this often.
const RECORD_EVERY_MS: u64 = 60_000 * 100 / 1871;

/// Runners of the events workload's server: its own (CI's legs) and the replaying one.
const RUNNERS: usize = 2;

/// Agents working at once in the events workload: this effort's own, the five tracks of
/// `~/.cache/kiln-wire/BRIEF.md` and their lead.
const AGENTS: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Bash,
    Other,
    Read,
    Edit,
    Write,
}

/// The `i`th of a kind's sizes: nine in ten the median, the tenth what brings the mean to `mean`.
fn sized(i: usize, (median, mean): (usize, usize)) -> usize {
    if i % 10 == 9 { (10 * mean).saturating_sub(9 * median).max(median) } else { median }
}

/// kiln's own source, read in order and handed out in slices: text that compresses as the code and
/// command output a session carries do.
struct Corpus {
    text: String,
    at: usize,
    files: Vec<PathBuf>,
}

impl Corpus {
    fn load() -> anyhow::Result<Self> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        let mut dirs = vec![root];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d)?.flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files.push(p);
                }
            }
        }
        files.sort();
        let mut text = String::new();
        for f in &files {
            text.push_str(&std::fs::read_to_string(f)?);
        }
        anyhow::ensure!(!text.is_empty(), "no source under {}", env!("CARGO_MANIFEST_DIR"));
        Ok(Self { text, at: 0, files })
    }

    /// The next `n` bytes of it (to a character's end), from where the last left off.
    fn take(&mut self, n: usize) -> String {
        let mut out = String::with_capacity(n + 4);
        while out.len() < n {
            if self.at >= self.text.len() {
                self.at = 0;
            }
            let mut end = (self.at + (n - out.len())).min(self.text.len());
            while end > self.at && !self.text.is_char_boundary(end) {
                end -= 1;
            }
            if end == self.at {
                end += 1;
                while end < self.text.len() && !self.text.is_char_boundary(end) {
                    end += 1;
                }
            }
            out.push_str(self.text.get(self.at..end).unwrap_or(""));
            self.at = end;
        }
        out
    }

    /// `n` whole lines of it.
    fn lines(&mut self, n: usize) -> String {
        (0..n).map(|_| format!("{}\n", self.take(44).replace('\n', " "))).collect()
    }

    /// The next `n` bytes of it, as [`Corpus::take`] cuts them, for what the person or the agent says (a
    /// prompt, a text), past any slice kiln takes for a mention of a file (`kiln::harness::files::mentioned`).
    /// A slice cut at a byte count can begin inside a path, and a file a message mentions brings each cold
    /// open of its session a frame of what's kept of it. Once fa3b55d7b78 moved kiln's source, the median
    /// session's 34th text began `/ci.md W27-R08).` (cut from a comment's `docs/ci.md`): one frame more in
    /// each of its cold opens, past the whole session's budget (W21-R15). The shapes ([`Shape`]) count no
    /// mentions, and the budgets were set with none.
    fn said(&mut self, n: usize) -> String {
        // Once round the corpus at the most: kiln's source isn't all mentions.
        for _ in 0..self.text.len() / n.max(1) {
            let text = self.take(n);
            if kiln::harness::files::mentioned(&text).is_empty() {
                return text;
            }
        }
        self.take(n)
    }
}

/// A session of `shape` for the stand-in to replay in `cwd` (`.fake-replay.jsonl`, the files it
/// edits copied there from kiln's source), one record every `every_ms`: its prompts, the first the
/// one it opens with. Its last record is [`END`].
fn lay_session(shape: &Shape, corpus: &mut Corpus, cwd: &Path, every_ms: u64) -> anyhow::Result<Vec<String>> {
    std::fs::create_dir_all(cwd)?;
    // The files it edits: `files` of kiln's own, evenly through them.
    let pool: Vec<String> = (0..shape.files)
        .filter_map(|i| corpus.files.get(i * corpus.files.len() / shape.files.max(1)).cloned())
        .enumerate()
        .map(|(i, f)| -> anyhow::Result<String> {
            let rel = format!("src/f{i:03}_{}", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            let to = cwd.join(&rel);
            if let Some(dir) = to.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::copy(&f, &to)?;
            Ok(rel)
        })
        .collect::<anyhow::Result<_>>()?;
    let mut lines = vec![json!({ "every_ms": every_ms }).to_string()];
    let mut used = [0usize; 5];
    let mut texts = 0;
    for i in 0..shape.tools {
        let turn = 1 + i * shape.turns / shape.tools.max(1);
        // The kind furthest behind its share.
        let (k, (tool, _)) = SHARES
            .iter()
            .enumerate()
            .max_by(|(a, (_, sa)), (b, (_, sb))| {
                let behind = |s: f64, n: usize| s * (i + 1) as f64 - used.get(n).copied().unwrap_or(0) as f64;
                behind(*sa, *a).total_cmp(&behind(*sb, *b))
            })
            .unwrap_or((0, &(Tool::Bash, 0.0)));
        let n = used.get(k).copied().unwrap_or(0);
        if let Some(u) = used.get_mut(k) {
            *u += 1;
        }
        let record = match tool {
            Tool::Bash => json!({ "turn": turn, "kind": "tool", "name": "Bash", "input": { "command": corpus.take(sized(n, BASH_IN)).replace('\n', " "), "description": "Look at the code" }, "output": corpus.take(sized(n, BASH_OUT)) }),
            Tool::Other => json!({ "turn": turn, "kind": "tool", "name": "mcp__kiln__run", "input": { "command": corpus.take(sized(n, OTHER_IN)).replace('\n', " ") }, "output": corpus.take(sized(n, OTHER_OUT)) }),
            Tool::Read => {
                let text = corpus.take(sized(n, READ_OUT));
                let numbered: String = text.lines().enumerate().map(|(l, t)| format!("{:>6}\t{t}\n", l + 1)).collect();
                let path = pool.get(n % pool.len().max(1)).cloned().unwrap_or_default();
                json!({ "turn": turn, "kind": "tool", "name": "Read", "input": { "file_path": cwd.join(path).display().to_string() }, "output": numbered })
            }
            Tool::Edit => {
                // Six lines of context and `k` lines out, `k` in.
                let k = sized(n, EDIT_LINES).saturating_sub(6) / 2;
                let path = pool.get(n % pool.len().max(1)).cloned().unwrap_or_default();
                json!({ "turn": turn, "kind": "tool", "name": "Edit", "path": path, "at": n * 7919 + 40, "remove": k.max(1), "insert": corpus.lines(k.max(1)) })
            }
            Tool::Write => json!({ "turn": turn, "kind": "tool", "name": "Write", "path": format!("src/new_{n:03}.rs"), "text": corpus.take(sized(n, WRITE_IN)) }),
        };
        lines.push(record.to_string());
        // The texts spread evenly between the calls.
        let due = (i + 1) * shape.texts / shape.tools.max(1);
        while texts < due {
            lines.push(json!({ "turn": turn, "kind": "text", "text": corpus.said(sized(texts, TEXT)) }).to_string());
            texts += 1;
        }
    }
    lines.push(json!({ "turn": shape.turns, "kind": "text", "text": END }).to_string());
    std::fs::write(cwd.join(".fake-replay.jsonl"), lines.join("\n") + "\n")?;
    Ok((0..shape.turns).map(|t| corpus.said(sized(t, PROMPT))).collect())
}

/// The last thing a replayed session says.
const END: &str = "That's the whole of it: the workload is done.";

/// A runner of the server whose `claude` is the replaying stand-in.
fn replay_runner(dir: &Path, port: u16, name: &str) -> anyhow::Result<(agents::Runner, PathBuf)> {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("replay-bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude-replay/claude"), bin.join("claude"))?;
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))?;
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    agents::runner_env(dir, port, name, None, Talk { lines: 0, every_ms: 0 }, &[("PATH", &path)])
}

/// Opens a stand-in Claude Code session on runner `name` in `cwd`, prompted with `prompt`: its id.
fn open(s: &Server, name: &str, cwd: &Path, prompt: &str) -> anyhow::Result<String> {
    let body = json!({"target": format!("agent:{}", common::machine(name)), "spec": {"kind": "claude", "cwd": cwd.display().to_string(), "prompt": prompt}});
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (code, text) = common::http_within(s.port, "POST", "/api/harness/sessions", &s.admin, Some(&body), Duration::from_secs(75))?;
        if code == 200
            && let Some(id) = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.pointer("/0/id").and_then(Value::as_str).map(str::to_owned))
        {
            return Ok(id);
        }
        anyhow::ensure!(Instant::now() < deadline, "couldn't open a session: {code} {text}");
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Runs session `id` (in `cwd`) through its prompts after the first, each once the turn before is
/// done, then waits until the server's copy holds its last record.
fn finish(s: &Server, id: &str, cwd: &Path, prompts: &[String]) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(900);
    for (k, next) in prompts.iter().enumerate().skip(1) {
        while !cwd.join(format!(".fake-replay-turn-{k}")).exists() {
            anyhow::ensure!(Instant::now() < deadline, "turn {k} of {id} never ended\n{}", s.log());
            std::thread::sleep(Duration::from_millis(100));
        }
        let c = json!({"type": "send", "id": format!("budget-{id}-{k}"), "text": next});
        let (st, body) = http(s.port, "POST", &format!("/api/harness/sessions/{id}/command"), &s.admin, Some(&c))?;
        anyhow::ensure!(st == 200, "turn {}'s prompt: {st} {body}", k + 1);
    }
    loop {
        let (frames, _) = agents::attach(s.port, &s.admin, id, "")?;
        if agents::events(&frames).iter().any(|e| e.get("text").and_then(Value::as_str) == Some(END)) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "the server's copy of {id} never held its end\n{}", s.log());
        std::thread::sleep(Duration::from_secs(1));
    }
}

// ---------------------------------------------------------------------------------- the wire

/// What a browser asks for (Chrome's `Accept-Encoding`).
const ACCEPT: &str = "gzip, deflate, br, zstd";

/// One GET as a browser makes it: the bytes that came (head, body and its framing) and the body.
struct Got {
    wire: usize,
    body: Vec<u8>,
}

fn get(port: u16, token: &str, path: &str) -> anyhow::Result<Got> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nAccept: application/json\r\nAccept-Encoding: {ACCEPT}\r\nConnection: close\r\n\r\n")?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("{path}: no head"))?;
    let head = String::from_utf8_lossy(raw.get(..split).unwrap_or_default()).to_ascii_lowercase();
    anyhow::ensure!(head.starts_with("http/1.1 200"), "{path}: {}", head.lines().next().unwrap_or_default());
    let mut body = raw.get(split + 4..).unwrap_or_default().to_vec();
    if head.contains("transfer-encoding: chunked") {
        body = dechunk(&body);
    }
    let encoding = head.lines().find_map(|l| l.strip_prefix("content-encoding:")).map(|e| e.trim().to_owned());
    let body = match encoding.as_deref() {
        None | Some("identity") => body,
        Some("gzip") => read_all(flate2::read::GzDecoder::new(body.as_slice()))?,
        Some("deflate") => read_all(flate2::read::ZlibDecoder::new(body.as_slice()))?,
        Some("br") => read_all(brotli::Decompressor::new(body.as_slice(), 4096))?,
        Some(other) => anyhow::bail!("{path} came {other}: this test decodes gzip, deflate and br; give it {other}'s decoder"),
    };
    Ok(Got { wire: raw.len(), body })
}

fn read_all(mut r: impl std::io::Read) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    r.read_to_end(&mut out)?;
    Ok(out)
}

fn dechunk(mut s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = s.windows(2).position(|w| w == b"\r\n") {
        let size = String::from_utf8_lossy(s.get(..eol).unwrap_or_default()).into_owned();
        let n = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        let rest = s.get(eol + 2..).unwrap_or_default();
        out.extend_from_slice(rest.get(..n).unwrap_or(rest));
        s = rest.get(n + 2..).unwrap_or_default();
    }
    out
}

/// `body` gzip'd as flate2's default (level 6) makes it: what the agent socket's `z=gzip` uses
/// (`Snapshot::packed`), so what compressing the response would send.
fn gzipped(body: &[u8]) -> usize {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    if gz.write_all(body).is_err() {
        return body.len();
    }
    gz.finish().map_or(body.len(), |b| b.len())
}

/// A WebSocket frame's head for a payload of `n` bytes, sent by the server (unmasked; RFC 6455 §5.2).
fn ws_head(n: usize) -> usize {
    2 + if n >= 1 << 16 { 8 } else if n >= 126 { 2 } else { 0 }
}

/// What permessage-deflate with context takeover (RFC 7692; T3 Code's socket) would send for a
/// stream of messages: one deflate stream, each message sync-flushed and its 4-byte tail dropped
/// (§7.2.1), with its frame head.
struct Deflate(flate2::Compress);

impl Deflate {
    fn new() -> Self {
        Self(flate2::Compress::new(flate2::Compression::default(), false))
    }

    fn push(&mut self, msg: &[u8]) -> usize {
        let start = self.0.total_in();
        let mut out: Vec<u8> = Vec::with_capacity(msg.len() / 2 + 64);
        loop {
            let done = usize::try_from(self.0.total_in() - start).unwrap_or(usize::MAX);
            let rest = msg.get(done..).unwrap_or_default();
            if out.capacity() - out.len() < 64 {
                out.reserve(out.capacity().max(64));
            }
            let (was_in, was_out) = (self.0.total_in(), out.len());
            if self.0.compress_vec(rest, &mut out, flate2::FlushCompress::Sync).is_err() {
                break;
            }
            let all_in = usize::try_from(self.0.total_in() - start).unwrap_or(usize::MAX) >= msg.len();
            // Flushed once a call with all of it in left room in the output (or made no progress).
            if (all_in && out.len() < out.capacity()) || (self.0.total_in() == was_in && out.len() == was_out) {
                break;
            }
        }
        let n = if out.ends_with(&[0, 0, 0xff, 0xff]) { out.len() - 4 } else { out.len() };
        n + ws_head(n)
    }
}

/// A WebSocket as a browser opens it (offering permessage-deflate), counting what comes down.
struct Ws {
    s: TcpStream,
    /// The server took permessage-deflate: its compressed messages are inflated here (reset after
    /// each when it said `server_no_context_takeover`).
    inflate: Option<(flate2::Decompress, bool)>,
    /// Bytes received so far, the handshake's answer included.
    wire: usize,
}

/// One message from the server: its opcode, its payload (inflated), the bytes it came in.
struct Msg {
    op: u8,
    payload: Vec<u8>,
    wire: usize,
}

impl Ws {
    fn open(port: u16, token: &str, path: &str) -> anyhow::Result<Self> {
        let mut s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_read_timeout(Some(Duration::from_secs(120)))?;
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\nAccept-Encoding: {ACCEPT}\r\n\r\n"
        )?;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.extend(agents::take(&mut s, 1)?);
            anyhow::ensure!(head.len() < 16 << 10, "no handshake answer: {}", String::from_utf8_lossy(&head));
        }
        let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
        anyhow::ensure!(text.starts_with("http/1.1 101"), "not upgraded: {text}");
        let ext = text.lines().find_map(|l| l.strip_prefix("sec-websocket-extensions:")).unwrap_or_default().to_owned();
        let inflate = ext.contains("permessage-deflate").then(|| (flate2::Decompress::new(false), ext.contains("server_no_context_takeover")));
        Ok(Self { s, inflate, wire: head.len() })
    }

    fn next(&mut self) -> anyhow::Result<Msg> {
        let mut payload = Vec::new();
        let mut wire = 0;
        let mut first: Option<(u8, bool)> = None;
        loop {
            let h = agents::take(&mut self.s, 2)?;
            let (b0, b1) = (h.first().copied().unwrap_or(0), h.get(1).copied().unwrap_or(0));
            let (ext, len) = match b1 & 0x7f {
                126 => (2, agents::take(&mut self.s, 2)?.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b))),
                127 => (8, agents::take(&mut self.s, 8)?.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b))),
                n => (0, usize::from(n)),
            };
            let body = agents::take(&mut self.s, len)?;
            wire += 2 + ext + len;
            self.wire += 2 + ext + len;
            let op = b0 & 0x0f;
            if op == 9 {
                // A ping: answered, as a browser does.
                self.pong(&body)?;
                continue;
            }
            if op >= 8 {
                return Ok(Msg { op, payload: body, wire });
            }
            let (op, compressed) = *first.get_or_insert((op, b0 & 0x40 != 0));
            payload.extend(body);
            if b0 & 0x80 == 0 {
                continue;
            }
            if compressed && let Some((d, reset)) = self.inflate.as_mut() {
                payload.extend_from_slice(&[0, 0, 0xff, 0xff]);
                let mut out = Vec::with_capacity(payload.len() * 4 + 64);
                let start = d.total_in();
                loop {
                    let done = usize::try_from(d.total_in() - start).unwrap_or(usize::MAX);
                    if out.capacity() - out.len() < 1024 {
                        out.reserve(out.capacity().max(1024));
                    }
                    let (was_in, was_out) = (d.total_in(), out.len());
                    d.decompress_vec(payload.get(done..).unwrap_or_default(), &mut out, flate2::FlushDecompress::Sync)?;
                    let all_in = usize::try_from(d.total_in() - start).unwrap_or(usize::MAX) >= payload.len();
                    if (all_in && out.len() < out.capacity()) || (d.total_in() == was_in && out.len() == was_out) {
                        break;
                    }
                }
                if *reset {
                    d.reset(false);
                }
                payload = out;
            }
            return Ok(Msg { op, payload, wire });
        }
    }

    fn pong(&mut self, body: &[u8]) -> anyhow::Result<()> {
        let n = u8::try_from(body.len().min(125))?;
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut f = vec![0x8a, 0x80 | n];
        f.extend(mask);
        f.extend(body.iter().take(125).zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
        self.s.write_all(&f)?;
        Ok(())
    }
}

// ------------------------------------------------------------------------------ agent cold open

/// What an agent page receives on opening, to its `synced`.
#[derive(Default)]
struct ColdOpen {
    /// Bytes on the wire, the handshake's answer included.
    wire: usize,
    /// Socket messages, and the frames in them (a packed snapshot is one message of many frames).
    messages: usize,
    frames: usize,
    /// The frames' JSON, decoded.
    decoded: usize,
    /// What permessage-deflate would have sent of the same frames: with context takeover (one
    /// stream), and without it (each frame on its own, `server_no_context_takeover`).
    deflate: usize,
    deflate_each: usize,
    /// Decoded bytes by frame kind (`t`).
    by_kind: BTreeMap<String, (usize, usize)>,
    /// Timeline events, and of their tool calls the JSON of each part.
    events: usize,
    tool_output: usize,
    tool_input: usize,
    tool_diffs: usize,
}

impl ColdOpen {
    fn frame(&mut self, text: &[u8]) -> anyhow::Result<bool> {
        let v: Value = serde_json::from_slice(text)?;
        let t = v.get("t").and_then(Value::as_str).unwrap_or_default().to_owned();
        self.frames += 1;
        self.decoded += text.len();
        self.deflate_each += Deflate::new().push(text);
        let k = self.by_kind.entry(t.clone()).or_default();
        k.0 += 1;
        k.1 += text.len();
        let len = |v: Option<&Value>| v.map_or(0, |v| v.to_string().len());
        for e in v.get("events").and_then(Value::as_array).into_iter().flatten() {
            self.events += 1;
            if e.get("kind").and_then(Value::as_str) == Some("tool") {
                self.tool_output += len(e.get("output"));
                self.tool_input += len(e.get("input"));
                self.tool_diffs += len(e.get("diffs"));
            }
        }
        Ok(t == "synced")
    }
}

/// Attaches to session `id` as the UI does (`query` its query) and reads to `synced`.
fn cold_open(s: &Server, id: &str, query: &str) -> anyhow::Result<ColdOpen> {
    let mut ws = Ws::open(s.port, &s.admin, &format!("/api/harness/sessions/{id}/ws{query}"))?;
    let mut o = ColdOpen::default();
    // The frames as one deflate stream (a socket's context carries over between its messages).
    let mut stream = Deflate::new();
    let mut deflate = 0;
    loop {
        let m = ws.next()?;
        anyhow::ensure!(m.op < 8, "the socket closed before synced: {}", String::from_utf8_lossy(&m.payload));
        o.messages += 1;
        let mut synced = false;
        if m.op == 2 {
            // A packed snapshot (`z=gzip`): its frames a line each, `synced` the last.
            let text = read_all(flate2::read::GzDecoder::new(m.payload.as_slice()))?;
            for line in text.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                synced |= o.frame(line)?;
            }
            deflate += m.wire;
        } else {
            synced = o.frame(&m.payload)?;
            deflate += stream.push(&m.payload);
        }
        if synced {
            break;
        }
    }
    o.wire = ws.wire;
    o.deflate = deflate;
    Ok(o)
}

// --------------------------------------------------------------------------------------- budgets

/// A measured value and its budget: the most it may be.
struct Budget {
    what: String,
    value: usize,
    most: usize,
    /// The track expected to lower it (`~/.cache/kiln-wire/BRIEF.md`), or "" to hold it.
    track: &'static str,
}

fn kib(n: usize) -> String {
    if n >= 1 << 20 { format!("{:.2} MiB", n as f64 / f64::from(1 << 20)) } else { format!("{:.1} KiB", n as f64 / 1024.0) }
}

/// Prints the budgets and fails on any over its budget.
fn hold(title: &str, budgets: &[Budget]) -> anyhow::Result<()> {
    println!("\n{title}: budgets\n{:<58} {:>14} {:>14}  lowered by", "what", "today", "budget");
    let mut over = Vec::new();
    for b in budgets {
        println!("{:<58} {:>14} {:>14}  {}", b.what, b.value, b.most, b.track);
        if b.value > b.most {
            over.push(format!("{}: {} > {}", b.what, b.value, b.most));
        }
    }
    anyhow::ensure!(over.is_empty(), "over budget (lower a budget when your change lowers its value; raise one only with why):\n{}", over.join("\n"));
    Ok(())
}

// ------------------------------------------------------------------------------------ the tests

/// W21-R15: an agent page's cold open, a median and a long session, as the UI attaches
/// (`ui/src/api/harness.ts` TAKES: a bounded window, lean events), the same without gzip, and as a
/// tab from before the window does (the whole session).
#[test]
fn w21_r15_agent_page_cold_open_budget() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: "" })?;
    let (_runner, log) = replay_runner(dir.path(), s.port, "budget-runner")?;
    agents::connected(s.port, &s.admin, "budget-runner", &log, Duration::from_secs(120))?;
    agents::headroom_off(s.port, &s.admin, "budget-runner", Duration::from_secs(60))?;
    let mut corpus = Corpus::load()?;
    let mut opened = Vec::new();
    for shape in [&MEDIAN, &LONG] {
        let cwd = dir.path().join(format!("session-{}", shape.name));
        // As fast as the stand-in can say it: the cold open depends on what was said, not when.
        let prompts = lay_session(shape, &mut corpus, &cwd, 0)?;
        let id = open(&s, "budget-runner", &cwd, prompts.first().map_or("", String::as_str))?;
        opened.push((shape, id, cwd, prompts));
    }
    for (_, id, cwd, prompts) in &opened {
        finish(&s, id, cwd, prompts)?;
    }
    println!("\nagent page cold open (to synced)");
    println!("{:<8} {:<14} {:>12} {:>9} {:>7} {:>12} {:>12} {:>12} {:>7} {:>12} {:>12} {:>12}", "session", "query", "wire", "messages", "frames", "decoded", "deflate ctx", "deflate each", "events", "tool output", "tool input", "tool diffs");
    let mut budgets = Vec::new();
    for (shape, id, _, _) in &opened {
        for (query, label) in [("?z=gzip&patch=state&patch=changes&window=1&lean=1", "UI (z=gzip)"), ("?patch=state&patch=changes&window=1&lean=1", "UI, no gzip"), ("?z=gzip&patch=state&patch=changes", "whole (older)")] {
            let o = cold_open(&s, id, query)?;
            println!(
                "{:<8} {:<14} {:>12} {:>9} {:>7} {:>12} {:>12} {:>12} {:>7} {:>12} {:>12} {:>12}",
                shape.name,
                label,
                kib(o.wire),
                o.messages,
                o.frames,
                kib(o.decoded),
                kib(o.deflate),
                kib(o.deflate_each),
                o.events,
                kib(o.tool_output),
                kib(o.tool_input),
                kib(o.tool_diffs)
            );
            for (k, (n, b)) in &o.by_kind {
                println!("{:<8} {:<14}   {k:<10} {n:>5} frames {:>12}", "", "", kib(*b));
            }
            let most = |b: &[(&str, &str, usize)]| b.iter().find(|(sh, q, _)| *sh == shape.name && *q == label).map_or(0, |(_, _, n)| *n);
            budgets.push(Budget { what: format!("cold open {} {label}: wire bytes", shape.name), value: o.wire, most: most(COLD_WIRE), track: "agent" });
            budgets.push(Budget { what: format!("cold open {} {label}: decoded bytes", shape.name), value: o.decoded, most: most(COLD_DECODED), track: "agent" });
            budgets.push(Budget { what: format!("cold open {} {label}: frames", shape.name), value: o.frames, most: most(COLD_FRAMES), track: "agent" });
        }
    }
    hold("agent page cold open", &budgets)
}

// The budgets. Each is the most of four to seven runs on adacachy (2026-11-04, origin/main 01d3ff1c99b) times
// one plus the runs' spread ((max - min) / min) plus 7%: the noise seen, and T3 Code's tightest
// margin over observed (`TransferBudgetReport.integration.ts`, LONG_TURN_BUDGET "about 7% above the
// observed"), so a run's noise passes and a regression of a tenth fails. Each says its value today
// (the most of the four, and their spread) and the track of `~/.cache/kiln-wire/BRIEF.md` expected
// to lower it: that track sets the budget to its own measured value by the same rule as it lands.
// On 2026-11-05 every budget below was set so from five runs on adacachy of the five tracks together
// (perf/wire-ship); where the rule would have raised one, it was kept as it was.

/// Cold open, bytes on the wire to `synced` (the handshake's answer included): (session, query, most).
/// Before the tracks, the whole session: median 179,669 (z=gzip; spread 0.17%) and 879,561 (no gzip;
/// 0); p90 1,505,082 (0.07%) and 7,196,976 (0). With the agent track's window and lean events and
/// the compress track's permessage-deflate: median 70,453 to 70,538 (z=gzip) and 70,444 to 70,527
/// (no gzip), p90 166,862 to 167,052 and 166,752 to 166,954 (spreads 0.1%); a tab from before the
/// window, median 180,380 to 180,438, p90 1,496,911 to 1,498,266 (where the rule would raise its
/// budget from before the tracks, that one stays). Raised with origin/main 04b541b044f, for goal 10
/// (plan 01a1a87b2dae step 3): a packed snapshot deflates what it writes at open time at the fast
/// level, and only whole chunks at the default one. A window is written at open time, so z=gzip
/// came to median 100,766 to 102,748 and p90 244,269 to 245,133, and a tab from before the window
/// median 202,501 to 204,054 (five runs).
const COLD_WIRE: &[(&str, &str, usize)] = &[
    ("median", "UI (z=gzip)", 111_962),
    ("median", "UI, no gzip", 75_547),
    ("median", "whole (older)", 219_903),
    ("p90", "UI (z=gzip)", 263_160),
    ("p90", "UI, no gzip", 178_844),
    ("p90", "whole (older)", 1_604_501),
];
/// Cold open, the frames' JSON as the page parses it. Before the tracks: median 879,337, p90
/// 7,196,512. With the window: median 307,288 to 307,289, p90 661,169 to 661,170; a tab from before
/// it, median 883,969 and p90 7,182,436 (no spread).
const COLD_DECODED: &[(&str, &str, usize)] = &[
    ("median", "UI (z=gzip)", 328_801),
    ("median", "UI, no gzip", 328_801),
    ("median", "whole (older)", 940_891),
    ("p90", "UI (z=gzip)", 707_453),
    ("p90", "UI, no gzip", 707_453),
    ("p90", "whole (older)", 7_685_208),
];
/// Cold open, frames to `synced` (a packed snapshot's lines counted). Before the tracks: median 9,
/// p90 33; with the window 7. A tab from before the window: since goal 10 (a frame per chunk of the
/// copy's events) median 10 and p90 41.
const COLD_FRAMES: &[(&str, &str, usize)] = &[("median", "UI (z=gzip)", 8), ("median", "UI, no gzip", 8), ("median", "whole (older)", 10), ("p90", "UI (z=gzip)", 8), ("p90", "UI, no gzip", 8), ("p90", "whole (older)", 44)];

/// Events: `run`, `job`, `run.*` and `ci_verdict` wire bytes per job of the push. Before the tracks
/// 11,207, no spread (the first run counted only `run` and `job`: 10,250); with the events and
/// compress tracks 680 to 718 (spread 5.6%).
const EVENTS_CI_PER_JOB: usize = 809;
/// Events: `harness` frames per agent-hour (one an agent's record, about: [`RECORD_EVERY_MS`]).
/// Before the tracks 1,161 (seven runs, spread 3.8%); with them 1,101 to 1,165 (5.8%: the rule
/// would raise it, so it stays). The events track's (coalescing per entity, batching).
const EVENTS_HARNESS_FRAMES_PER_AGENT_HOUR: usize = 1_286;
/// Events: `harness` wire bytes per agent-minute (each frame the agent's whole entry, about 990 B).
/// Before the tracks 19,177 (seven runs, spread 3.7%); with the compress track's permessage-deflate
/// 1,308 to 1,647 (25.9%).
const EVENTS_HARNESS_WIRE_PER_AGENT_MIN: usize = 2_190;
/// Events: `headroom` frames (about 65 B each) per runner-minute: a runner reports its headroom's
/// state whenever it changes, which a loaded machine's readings do often. Before the tracks 326 to
/// 583 over seven runs; with them 305 to 528 over five (spread 73%: it follows the host's load, so
/// this budget is loose by need). The events track's topics keep them from a page that shows no
/// headroom; this tab, like the headroom page, asks for every type.
const EVENTS_HEADROOM_FRAMES_PER_RUNNER_MIN: usize = 952;
/// Events: every other frame (`sample`, `keep`, `offers` ...) a minute. Before the tracks 35 to 40
/// (spread 14%); with them 35 to 43 (23%: the rule would raise it, so it stays). Raised with the load
/// balancer line, whose `offers` frames tell the offers view its runners' room changed (docs/reach.md
/// §2.2; 20 a session here): main at c9c48f77 37, the line 63 to 67 (spread 6.3%) over five runs on
/// kiln-a094a12d (adacachy runs production), 2026-11-08.
const EVENTS_REST_FRAMES_PER_MIN: usize = 76;
/// Events: those frames' wire bytes a minute. Before the tracks 10,165 to 10,807 (spread 6.3%); with
/// them 2,518 to 3,010 (19.5%).
const EVENTS_REST_WIRE_PER_MIN: usize = 3_809;
/// Ship: wire bytes per load, by key. Before the tracks the list 1,151 (one row; spread 2.0%), the
/// opened row 5,740 (12 jobs; 2.3%); compressed, the list 694 to 841 (21%), the row 1,223 to 1,357
/// (11%).
const SHIP_WIRE_PER_LOAD: &[(&str, usize)] = &[("Ship list", 1_079), ("opened row", 1_601)];
/// Ship's loads a minute over both keys. Before `ship` frames, 87 to 111, held to what
/// `resource.ts` allowed (128 for two keys: each once a second after its last and its 15 s polls);
/// with them (W21-R15), 10 to 11 (spread 10%): its polls, and a frame it can't place.
const SHIP_LOADS_PER_MIN: usize = 13;
/// Those loads' wire bytes a minute. Before `ship` frames, 290 to 370 KiB; with them and
/// compression, 10,510 to 11,179 (spread 6.4%).
const SHIP_LOAD_WIRE_PER_MIN: usize = 12_674;
/// The `ship` frames a Ship tab is sent instead, wire bytes a minute: 3,668 to 5,207 (spread 42%:
/// how many rows a refresh finds changed follows when the legs end).
const SHIP_FRAMES_WIRE_PER_MIN: usize = 7_757;
/// The pages' GETs, wire bytes. Before the tracks: agents list 6,374 (6 agents; spread 7.1%, what
/// each is doing), runs 1,965 (4 runs), a run 2,960 (5 jobs), Ship rows 1,159 (1 row), a Ship row
/// 6,552, runners 4,334 (2), overview 6,761 (0.4%). Compressed: agents list 2,179 to 2,414 (10.8%),
/// runs 640 to 653 (2.0%), a run 824 to 863 (4.7%), Ship rows 779 to 784 (0.6%), a Ship row 1,359 to
/// 1,365 (0.4%), runners 1,521 to 1,548 (1.8%), overview 1,167 to 1,172 (0.4%). Runners raised with the
/// load balancer line, whose runners' facts carry their storage and memory level (lb/sense): main at
/// c9c48f77 1,589, the line 1,744 to 1,761 (spread 1.0%) over five runs on kiln-a094a12d, 2026-11-08.
const API_WIRE: &[(&str, usize)] = &[("agents list", 2_844), ("runs", 712), ("a run", 965), ("Ship rows", 844), ("Ship row", 1_467), ("runners", 1_902), ("overview", 1_260)];

/// A key of Ship's that the simulated tab keeps loaded: what it GETs, how often it polls
/// (`ui/src/views/ship/index.tsx`, `jobs.tsx`), what it last loaded, and what its loads came to.
struct Key {
    name: &'static str,
    path: String,
    /// A commit's detail (else the repositories' list).
    detail: bool,
    /// When its last load started and ended: a frame that comes meanwhile loads it again once it
    /// lands (`resource.ts`: it was read before the frame).
    started: Instant,
    settled: Instant,
    polled: Instant,
    pending: bool,
    /// What its last load said: whom its rows are for (`viewer`, from memory) and its `dv`.
    viewer: bool,
    dv: Option<String>,
    loads: usize,
    wire: usize,
    decoded: usize,
    gzip: usize,
}

impl Key {
    /// Whether the page takes `frame`, a `ship` frame, in place (`ui/src/views/ship/live.ts`), as
    /// far as one repository's list and one commit's detail go; false: it loads again. The
    /// repositories' list is taken as loading again on any frame that says a commit went or its
    /// pace line moved, as live.ts does for the row it shows.
    fn takes(&mut self, frame: &Value, sha: &str) -> bool {
        if !self.viewer || frame.get("whole").is_some() {
            return false;
        }
        let gone = frame.get("gone").and_then(Value::as_array).is_some_and(|g| !g.is_empty());
        if !self.detail {
            return !gone && frame.get("pace").is_none();
        }
        if frame.get("gone").and_then(Value::as_array).is_some_and(|g| g.iter().any(|x| x.as_str() == Some(sha))) {
            return false;
        }
        let Some(row) = frame.get("rows").and_then(Value::as_array).and_then(|rows| rows.iter().find(|r| r.pointer("/row/sha").and_then(Value::as_str) == Some(sha))) else { return true };
        let (dv, base) = (row.get("dv").and_then(Value::as_str), row.get("base").and_then(Value::as_str));
        let held = self.dv.as_deref();
        match dv {
            Some(d) if Some(d) == held => true,
            Some(d) if base.is_some() && base == held => {
                self.dv = Some(d.to_owned());
                true
            }
            _ => false,
        }
    }
}
/// `ui/src/lib/resource.ts`: a load asked for by events waits `EVENT_GAP_MS` after the last, and
/// Ship's keys poll every 15 s (`pollMs: 15_000`).
const EVENT_GAP: Duration = Duration::from_millis(1000);
const SHIP_POLL: Duration = Duration::from_secs(15);

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

/// kiln's own push to main (its `.kiln` on 01d3ff1c99b): the workflows a push to main runs, each
/// job's step count as written (ci 5 jobs and 18 steps, bench 3 and 6, image 1 and 5, release 3 and
/// 7). Each step says a few lines and takes [`STEP_S`].
const PUSH_TO_MAIN: &[(&str, &[usize])] = &[("ci", &[4, 4, 4, 3, 3]), ("bench", &[2, 2, 2]), ("image", &[5]), ("release", &[3, 2, 2])];

/// A step's seconds: kiln's own take minutes; these are short so the run fits in a test, two at a
/// time on the test server's runner (its two slots). The events a job makes don't depend on it; the
/// per-second rates below scale with it (bytes per job are given as well).
const STEP_S: u64 = 2;

fn workflows() -> Vec<(String, String)> {
    PUSH_TO_MAIN
        .iter()
        .map(|(name, jobs)| {
            let mut y = format!("name: {name}\non: push\njobs:\n");
            for (j, steps) in jobs.iter().enumerate() {
                y.push_str(&format!("  {name}-{j}:\n    runs-on: local\n    steps:\n"));
                for k in 0..*steps {
                    y.push_str(&format!("      - name: step {k}\n        run: for i in $(seq 1 20); do echo \"{name} {j} {k} line $i\"; done; sleep {STEP_S}\n"));
                }
            }
            (format!(".kiln/{name}.yaml"), y)
        })
        .collect()
}

/// W21-R15: a tab's events socket, Ship's reloads and the main pages' GETs, while [`AGENTS`] agents
/// work at a real agent's pace and kiln's own push to main runs.
#[test]
fn w21_r15_events_ship_and_api_budgets() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (_runner, log) = replay_runner(dir.path(), s.port, "busy-runner")?;
    agents::connected(s.port, &s.admin, "busy-runner", &log, Duration::from_secs(120))?;
    agents::headroom_off(s.port, &s.admin, "busy-runner", Duration::from_secs(60))?;

    // A repository of kiln's own, its first commit in.
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/ship", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/ship.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "ship\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while http(s.port, "GET", "/api/repos", &s.admin, None)?.1.contains("\"last_polled_at\":null") {
        anyhow::ensure!(Instant::now() < deadline, "the repository was never polled\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }

    // The agents, at work.
    let mut corpus = Corpus::load()?;
    for a in 0..AGENTS {
        let cwd = dir.path().join(format!("agent-{a}"));
        let prompts = lay_session(&MEDIAN, &mut corpus, &cwd, RECORD_EVERY_MS)?;
        open(&s, "busy-runner", &cwd, prompts.first().map_or("", String::as_str))?;
    }

    // A tab: its events socket read beside the test, every message with when it came.
    let (tx, rx) = mpsc::channel::<(String, usize, usize, usize, Instant, Option<Value>)>();
    let mut tab = Ws::open(s.port, &s.admin, "/api/events")?;
    std::thread::spawn(move || {
        // What permessage-deflate with context takeover would send of the same stream.
        let mut stream = Deflate::new();
        while let Ok(m) = tab.next() {
            if m.op >= 8 {
                return;
            }
            let v = serde_json::from_slice::<Value>(&m.payload).ok();
            let t = v.as_ref().and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
            // Ship's frames, kept whole: the simulated Ship tab takes them.
            let ship = v.filter(|_| t == "ship");
            if tx.send((t, m.wire, m.payload.len(), stream.push(&m.payload), Instant::now(), ship)).is_err() {
                return;
            }
        }
    });
    // Let the agents' first moments (opening, their first turn's start) pass.
    std::thread::sleep(Duration::from_secs(5));
    while rx.try_recv().is_ok() {}

    // The push: kiln's own workflows.
    for (path, text) in workflows() {
        std::fs::write(work.join(path), text)?;
    }
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "workflows"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let sha = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    let pushed = Instant::now();

    let now = Instant::now();
    let long_ago = now.checked_sub(EVENT_GAP + SHIP_POLL).unwrap_or(now);
    let key = |name, path: String, detail| Key { name, path, detail, started: long_ago, settled: long_ago, polled: long_ago, pending: true, viewer: false, dv: None, loads: 0, wire: 0, decoded: 0, gzip: 0 };
    let mut keys = [key("Ship list", "/api/ship/commits?by=repo&limit=100".to_owned(), false), key("opened row", format!("/api/ship/commits/acme/ship/{sha}"), true)];
    // The Ship tab's events: only `ship` frames (W21-R15; `ui/src/views/ship/live.ts` asks for no
    // other while the server's answers carry `viewer`).
    let (mut ship_frames, mut ship_wire, mut ship_placed) = (0usize, 0usize, 0usize);
    let mut by_type: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let (mut frames, mut wire, mut decoded, mut deflated) = (0usize, 0usize, 0usize, 0usize);
    let is_ci = |t: &str| t == "run" || t == "job" || t.starts_with("run.") || t == "ci_verdict";
    let mut checked = Instant::now();
    // When the last of CI's frames came: the window ends once the runs have finished and a whole
    // look at them (every 2 s) has gone by without one, so a run's last notices are in it.
    let mut last_ci = Instant::now();
    let mut ended: Option<Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(600);
    while ended.is_none() {
        anyhow::ensure!(Instant::now() < deadline, "the runs never finished\n{}", s.log());
        let first = rx.recv_timeout(Duration::from_millis(50)).ok();
        for (t, w, d, z, at, ship) in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            frames += 1;
            wire += w;
            decoded += d;
            deflated += z;
            if is_ci(&t) {
                last_ci = Instant::now();
            }
            let e = by_type.entry(t.clone()).or_default();
            *e = (e.0 + 1, e.1 + w, e.2 + d);
            let Some(frame) = ship else { continue };
            ship_frames += 1;
            ship_wire += w;
            for k in &mut keys {
                // A frame that came while its load was on its way: once it lands, it loads again.
                let during = at >= k.started && at <= k.settled;
                if during || !k.takes(&frame, &sha) {
                    k.pending = true;
                } else {
                    ship_placed += 1;
                }
            }
        }
        for k in &mut keys {
            let now = Instant::now();
            let poll = now.duration_since(k.polled) >= SHIP_POLL;
            if (k.pending && now.duration_since(k.settled) >= EVENT_GAP) || poll {
                if poll {
                    k.polled = now;
                }
                k.pending = false;
                // The row's commit has no Ship record until its first run does.
                k.started = Instant::now();
                if let Ok(got) = get(s.port, &s.admin, &k.path) {
                    k.loads += 1;
                    k.wire += got.wire;
                    k.decoded += got.body.len();
                    k.gzip += gzipped(&got.body);
                    let v = serde_json::from_slice::<Value>(&got.body).unwrap_or(Value::Null);
                    k.viewer = v.get("viewer").is_some();
                    k.dv = v.get("dv").and_then(Value::as_str).map(str::to_owned);
                }
                k.settled = Instant::now();
            }
        }
        if checked.elapsed() >= Duration::from_secs(2) {
            let quiet = last_ci < checked;
            checked = Instant::now();
            let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/ship&limit=20", &s.admin, None)?;
            let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
            if quiet && runs.len() >= PUSH_TO_MAIN.len() && runs.iter().all(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
                ended = Some(Instant::now());
            }
        }
    }
    let secs = ended.map_or(1.0, |e| e.duration_since(pushed).as_secs_f64()).max(1.0);
    let jobs: usize = PUSH_TO_MAIN.iter().map(|(_, j)| j.len()).sum();
    println!("\nevents socket, {AGENTS} agents working and kiln's push to main ({} workflows, {jobs} jobs) over {secs:.1} s", PUSH_TO_MAIN.len());
    println!("{:<16} {:>8} {:>10} {:>12} {:>12} {:>10}", "type", "frames", "frames/s", "wire", "wire/s", "avg frame");
    let mut types: Vec<_> = by_type.iter().collect();
    types.sort_by_key(|(_, (_, w, _))| std::cmp::Reverse(*w));
    for (t, (n, w, _)) in &types {
        println!("{t:<16} {n:>8} {:>10.2} {:>12} {:>12} {:>10}", *n as f64 / secs, kib(*w), kib((*w as f64 / secs) as usize), w / (*n).max(1));
    }
    println!("{:<16} {frames:>8} {:>10.2} {:>12} {:>12}", "all", frames as f64 / secs, kib(wire), kib((wire as f64 / secs) as usize));
    println!("decoded {}/s; permessage-deflate (context takeover) would send {}/s", kib((decoded as f64 / secs) as usize), kib((deflated as f64 / secs) as usize));
    // What a job of the push costs, what an agent at work costs a minute, and what comes regardless
    // (each runner's headroom reports, samples): each holds whatever the machine's speed, where a
    // rate over the whole window would move with how fast the runs went.
    let sum = |keep: &dyn Fn(&str) -> bool| by_type.iter().filter(|(t, _)| keep(t.as_str())).fold((0, 0), |(n, w), (_, (tn, tw, _))| (n + tn, w + tw));
    let (_, ci_wire) = sum(&is_ci);
    let (harness_n, harness_wire) = sum(&|t| t == "harness");
    let (headroom_n, _) = sum(&|t| t == "headroom");
    // Ship's frames are the Ship tab's, held below.
    let (rest_n, rest_wire) = sum(&|t| !is_ci(t) && t != "harness" && t != "headroom" && t != "ship");
    let agent_minutes = AGENTS as f64 * secs / 60.0;
    let runner_minutes = RUNNERS as f64 * secs / 60.0;
    println!("runs, legs and verdicts: {} per job of the push", kib(ci_wire / jobs.max(1)));
    println!("an agent at work: {:.1} harness frames, {} a minute", harness_n as f64 / agent_minutes, kib((harness_wire as f64 / agent_minutes) as usize));
    println!("a runner's headroom reports: {:.1} frames a minute", headroom_n as f64 / runner_minutes);
    println!("the rest (samples ...): {:.2} frames, {} a second", rest_n as f64 / secs, kib((rest_wire as f64 / secs) as usize));

    println!("\nShip while CI is busy (its keys' loads over {secs:.1} s)");
    println!("{:<12} {:>6} {:>10} {:>12} {:>12} {:>12} {:>14}", "key", "loads", "loads/min", "wire/min", "decoded/min", "gzip/min", "wire per load");
    let per_min = |n: usize| (n as f64 * 60.0 / secs) as usize;
    let mut budgets = vec![
        Budget { what: "events: runs+legs+verdicts wire bytes per job".into(), value: ci_wire / jobs.max(1), most: EVENTS_CI_PER_JOB, track: "events, compress" },
        Budget { what: "events: harness frames per agent-hour".into(), value: (harness_n as f64 * 60.0 / agent_minutes) as usize, most: EVENTS_HARNESS_FRAMES_PER_AGENT_HOUR, track: "events" },
        Budget { what: "events: harness wire bytes per agent-minute".into(), value: (harness_wire as f64 / agent_minutes) as usize, most: EVENTS_HARNESS_WIRE_PER_AGENT_MIN, track: "events, compress" },
        Budget { what: "events: headroom frames per runner-minute".into(), value: (headroom_n as f64 / runner_minutes) as usize, most: EVENTS_HEADROOM_FRAMES_PER_RUNNER_MIN, track: "events" },
        Budget { what: "events: the rest, frames/min".into(), value: per_min(rest_n), most: EVENTS_REST_FRAMES_PER_MIN, track: "events" },
        Budget { what: "events: the rest, wire bytes/min".into(), value: per_min(rest_wire), most: EVENTS_REST_WIRE_PER_MIN, track: "events, compress" },
    ];
    let mut loads = 0;
    for k in &keys {
        println!("{:<12} {:>6} {:>10.1} {:>12} {:>12} {:>12} {:>14}", k.name, k.loads, k.loads as f64 * 60.0 / secs, kib(per_min(k.wire)), kib(per_min(k.decoded)), kib(per_min(k.gzip)), kib(k.wire / k.loads.max(1)));
        loads += k.loads;
        let most = SHIP_WIRE_PER_LOAD.iter().find(|(n, _)| *n == k.name).map_or(0, |(_, m)| *m);
        budgets.push(Budget { what: format!("Ship {}: wire bytes per load", k.name), value: k.wire / k.loads.max(1), most, track: "ship, compress" });
    }
    println!("Ship: {} a minute over both keys", kib(per_min(keys.iter().map(|k| k.wire).sum())));
    println!("Ship's frames: {ship_frames} ({:.1} a minute), {} a minute; taken in place {ship_placed} times", ship_frames as f64 * 60.0 / secs, kib(per_min(ship_wire)));
    budgets.push(Budget { what: "Ship (list + opened row): loads/min".into(), value: per_min(loads), most: SHIP_LOADS_PER_MIN, track: "ship" });
    budgets.push(Budget { what: "Ship (list + opened row): load wire bytes/min".into(), value: per_min(keys.iter().map(|k| k.wire).sum()), most: SHIP_LOAD_WIRE_PER_MIN, track: "ship, compress" });
    budgets.push(Budget { what: "Ship: ship frames' wire bytes/min".into(), value: per_min(ship_wire), most: SHIP_FRAMES_WIRE_PER_MIN, track: "ship, compress" });

    // The main pages' largest GETs, now there's something in each.
    let run = serde_json::from_str::<Value>(&http(s.port, "GET", "/api/runs?limit=1", &s.admin, None)?.1)?;
    let run = run.pointer("/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let pages = [
        ("agents list", "/api/harness/sessions".to_owned()),
        ("runs", "/api/runs?limit=50".to_owned()),
        ("a run", format!("/api/runs/{run}")),
        ("Ship rows", "/api/ship/commits?by=repo&limit=100".to_owned()),
        ("Ship row", format!("/api/ship/commits/acme/ship/{sha}")),
        ("runners", "/api/agents".to_owned()),
        ("overview", "/api/overview".to_owned()),
        ("metrics history", "/api/metrics/history".to_owned()),
    ];
    println!("\nAPI JSON (GET as a browser asks, Accept-Encoding: {ACCEPT})");
    println!("{:<16} {:<44} {:>12} {:>12} {:>12} {:>7}", "page", "path", "wire", "decoded", "gzip", "saved");
    for (name, path) in &pages {
        let got = get(s.port, &s.admin, path)?;
        let gz = gzipped(&got.body);
        println!("{name:<16} {:<44} {:>12} {:>12} {:>12} {:>6.0}%", path.chars().take(44).collect::<String>(), kib(got.wire), kib(got.body.len()), kib(gz), 100.0 * (1.0 - gz as f64 / got.body.len().max(1) as f64));
        // Each page with a budget (the metrics history has none: it grows with the server's uptime,
        // a sample per machine every 5 s, so its size says how long the test ran).
        if let Some((_, most)) = API_WIRE.iter().find(|(n, _)| n == name) {
            budgets.push(Budget { what: format!("GET {name}: wire bytes"), value: got.wire, most: *most, track: "compress" });
        }
    }
    hold("events, Ship and API", &budgets)
}
