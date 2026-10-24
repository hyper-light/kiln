//! CP-T2.1, T-T2.1.1: the UI's live feed of an agent session, on kiln's bus (docs/bus.md), in the
//! session lab and the sync lab, unchanged in what it shows. Each lab is a real kiln server; its
//! session is Claude Code as kiln drives it, played by a stand-in program that prints what Claude
//! Code prints (stream-json: its answer to kiln's initialize, its init, then message after message
//! streamed in parts), so every frame goes the way a real session's does.
//!
//! - **The session lab:** a session of the server's own. Its frames go from its harness to the
//!   server's mirror of it, onto its feed, and out to each viewer's socket.
//! - **The sync lab:** a session on the server's runner. Its frames go from the runner to the server
//!   over the runner's link, kept in step with the runner's (the mirror the server syncs from the
//!   machine it runs on), then onto the feed and out.
//!
//! In each, two viewers watch from the start, one taking states whole and one as what changed in
//! them (`?patch=state`), the way the UI's page does. What each shows, once the session has said all
//! it says, is what a viewer that attaches then is shown: the same state, the same meta, the same
//! timeline, message for message (the session's every message in it); the session's end reaches
//! them both. And the frames went over the bus: its counters say so.
//!
//! A third lab fills a small bus with one session's feed, so another's frames and its resync marker
//! are refused, and holds every viewer of that feed to starting over within 5 s.

#![cfg(unix)]

mod common;

use std::io::{ErrorKind, Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use common::Options;
use serde_json::{Value, json};

/// Messages the stand-in streams.
const MESSAGES: usize = 30;

// ---------------------------------------------------------------------------------- the stand-in

/// The stand-in for Claude Code: its lines as Claude Code 2.1.296 prints them (kiln's harness tests
/// hold kiln to the same), each message as parts and then whole, once the lab says go (a file, `go`
/// beside it, so every message comes while the lab's viewers watch); then it waits for kiln to
/// close it.
fn stand_in(dir: &Path) -> anyhow::Result<String> {
    stand_in_as(dir, "claude", "go", MESSAGES, 0)
}

/// A stand-in named `name` that streams `messages` messages once `go`, a file beside it, exists,
/// each message's text `message number i` and `pad` more characters.
fn stand_in_as(dir: &Path, name: &str, go: &str, messages: usize, pad: usize) -> anyhow::Result<String> {
    let path = dir.join(name);
    let go = dir.join(go).to_string_lossy().into_owned();
    let pad = "x".repeat(pad);
    let script = format!(
        r#"#!/bin/sh
# kiln's live-feed labs: a stand-in for Claude Code (crates/kiln/tests/live_feed_labs.rs).
emit() {{ printf '%s\n' "$1"; }}
emit '{{"type":"control_response","response":{{"subtype":"success","request_id":"kiln_1","response":{{"commands":[],"models":[],"agents":[],"current_permission_mode":"default","claude_code_version":"2.1.296"}}}}}}'
emit '{{"type":"system","subtype":"init","cwd":"/work","session_id":"lab","tools":[],"mcp_servers":[],"model":"claude-opus-5-5","permissionMode":"default","claude_code_version":"2.1.296","agents":[]}}'
while [ ! -e '{go}' ]; do sleep 0.05; done
i=1
while [ "$i" -le {messages} ]; do
  emit '{{"type":"stream_event","event":{{"type":"message_start","message":{{"id":"msg_'$i'","type":"message","role":"assistant","model":"claude-opus-5-5","content":[]}}}},"parent_tool_use_id":null,"uuid":"s'$i'a"}}'
  emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":""}}}},"parent_tool_use_id":null,"uuid":"s'$i'b","api_message_id":"msg_'$i'"}}'
  emit '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"message "}}}},"parent_tool_use_id":null,"uuid":"s'$i'c","api_message_id":"msg_'$i'"}}'
  sleep 0.02
  emit '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"number '$i'{pad}"}}}},"parent_tool_use_id":null,"uuid":"s'$i'd","api_message_id":"msg_'$i'"}}'
  emit '{{"type":"assistant","message":{{"id":"msg_'$i'","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{{"type":"text","text":"message number '$i'{pad}"}}]}},"parent_tool_use_id":null,"session_id":"lab","uuid":"a'$i'"}}'
  i=$((i + 1))
done
emit '{{"type":"result","subtype":"success","is_error":false,"duration_ms":35,"num_turns":1,"result":"done","stop_reason":"end_turn","session_id":"lab","total_cost_usd":0,"modelUsage":{{}}}}'
exec cat > /dev/null
"#
    );
    std::fs::write(&path, script)?;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path.to_string_lossy().into_owned())
}

// ------------------------------------------------------------------------------------ a viewer

/// A viewer's socket: the upgrade, then the server's frames as they come (RFC 6455).
struct Socket {
    stream: TcpStream,
    buf: Vec<u8>,
    /// A message being put together from its fragments.
    partial: Vec<u8>,
}

impl Socket {
    fn open(port: u16, path: &str, token: &str) -> anyhow::Result<Self> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(Duration::from_millis(200)))?;
        let ask = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nAuthorization: Bearer {token}\r\n\r\n"
        );
        stream.write_all(ask.as_bytes())?;
        let mut s = Self { stream, buf: Vec::new(), partial: Vec::new() };
        let deadline = Instant::now() + Duration::from_secs(30);
        let head = loop {
            if let Some(end) = s.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(s.buf.get(..end).unwrap_or_default()).into_owned();
                s.buf.drain(..end.saturating_add(4));
                break head;
            }
            anyhow::ensure!(Instant::now() < deadline, "no answer to the upgrade");
            s.fill()?;
        };
        anyhow::ensure!(head.starts_with("HTTP/1.1 101"), "the upgrade was refused: {head}");
        Ok(s)
    }

    /// Reads what's there; false when nothing came within the read timeout.
    fn fill(&mut self) -> anyhow::Result<bool> {
        let mut chunk = [0u8; 65536];
        match self.stream.read(&mut chunk) {
            Ok(0) => anyhow::bail!("the server closed the socket"),
            Ok(n) => {
                self.buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                Ok(true)
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The next text message, if one comes before `until`; `None` once the server closes.
    fn next(&mut self, until: Instant) -> anyhow::Result<Option<String>> {
        loop {
            if let Some((fin, opcode, payload, used)) = frame(&self.buf)? {
                self.buf.drain(..used);
                match opcode {
                    0..=2 => {
                        self.partial.extend_from_slice(&payload);
                        if fin {
                            return Ok(Some(String::from_utf8(std::mem::take(&mut self.partial))?));
                        }
                    }
                    8 => return Ok(None),
                    _ => {}
                }
                continue;
            }
            anyhow::ensure!(Instant::now() < until, "no frame came in time");
            match self.fill() {
                Ok(_) => {}
                Err(e) if e.to_string().contains("closed the socket") => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }
}

/// A whole frame at the start of `buf`: its FIN, opcode, payload and length, once all of it came.
fn frame(buf: &[u8]) -> anyhow::Result<Option<(bool, u8, Vec<u8>, usize)>> {
    let (Some(b0), Some(b1)) = (buf.first(), buf.get(1)) else { return Ok(None) };
    anyhow::ensure!(b1 & 0x80 == 0, "a server's frame is never masked");
    let (len, at) = match b1 & 0x7f {
        126 => match buf.get(2..4) {
            Some(b) => (u64::from(u16::from_be_bytes([b.first().copied().unwrap_or(0), b.get(1).copied().unwrap_or(0)])), 4),
            None => return Ok(None),
        },
        127 => match buf.get(2..10).and_then(|b| <[u8; 8]>::try_from(b).ok()) {
            Some(b) => (u64::from_be_bytes(b), 10),
            None => return Ok(None),
        },
        n => (u64::from(n), 2),
    };
    let end = at + usize::try_from(len)?;
    match buf.get(at..end) {
        Some(payload) => Ok(Some((b0 & 0x80 != 0, b0 & 0x0f, payload.to_vec(), end))),
        None => Ok(None),
    }
}

/// What a viewer shows, from the frames it was sent: the reducer the UI's page applies.
#[derive(Debug, Default, Clone, PartialEq)]
struct Shown {
    meta: Value,
    state: Value,
    changes: Value,
    /// The timeline, each event by its id in the order it first came.
    events: Vec<(String, Value)>,
    synced: bool,
    closed: bool,
    frames: usize,
    /// Frames after its first snapshot: what came live.
    live: usize,
    /// Snapshots it was sent (each begins with `open`): more than one means it started over.
    opens: usize,
}

impl Shown {
    fn apply(&mut self, frame: &str) -> anyhow::Result<()> {
        let v: Value = serde_json::from_str(frame)?;
        self.frames += 1;
        if self.synced {
            self.live += 1;
        }
        match v.get("t").and_then(Value::as_str).unwrap_or_default() {
            "open" => {
                self.opens += 1;
                self.meta = Value::Null;
                self.state = Value::Null;
                self.changes = Value::Null;
                self.events.clear();
                self.synced = false;
            }
            "meta" => self.meta = v.get("meta").cloned().unwrap_or_default(),
            "state" => self.state = v.get("state").cloned().unwrap_or_default(),
            "state_patch" => {
                if let Some(state) = self.state.as_object_mut() {
                    for (k, val) in v.get("set").and_then(Value::as_object).into_iter().flatten() {
                        state.insert(k.clone(), val.clone());
                    }
                    for k in v.get("gone").and_then(Value::as_array).into_iter().flatten() {
                        state.remove(k.as_str().unwrap_or_default());
                    }
                }
            }
            "changes" => self.changes = v.get("files").cloned().unwrap_or_default(),
            "events" => {
                for e in v.get("events").and_then(Value::as_array).into_iter().flatten() {
                    let Some(id) = e.get("id").and_then(Value::as_str) else { continue };
                    match self.events.iter_mut().find(|(have, _)| have == id) {
                        Some((_, slot)) => *slot = e.clone(),
                        None => self.events.push((id.to_owned(), e.clone())),
                    }
                }
            }
            "stream" => {
                let (Some(id), Some(text)) = (v.get("id").and_then(Value::as_str), v.get("text").and_then(Value::as_str)) else { return Ok(()) };
                if let Some((_, e)) = self.events.iter_mut().find(|(have, _)| have == id)
                    && let Some(Value::String(s)) = e.get_mut("text")
                {
                    s.push_str(text);
                }
            }
            "reset" => self.events.clear(),
            "synced" => self.synced = true,
            "closed" => self.closed = true,
            _ => {}
        }
        Ok(())
    }

    /// What it shows, without how it came: state, meta, changes and the timeline.
    fn view(&self) -> (Value, Value, Value, Vec<(String, Value)>) {
        (self.state.clone(), self.meta.clone(), self.changes.clone(), self.events.clone())
    }

    /// The texts of its timeline.
    fn texts(&self) -> Vec<String> {
        self.events.iter().filter_map(|(_, e)| e.get("text").and_then(Value::as_str).map(str::to_owned)).collect()
    }
}

/// A viewer that reads on its own thread until told to stop or its session ends.
struct Viewer {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shown: std::sync::Arc<std::sync::Mutex<Shown>>,
    thread: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

impl Viewer {
    fn watch(port: u16, id: &str, token: &str, patches: bool) -> anyhow::Result<Self> {
        let path = format!("/api/harness/sessions/{id}/ws{}", if patches { "?patch=state" } else { "" });
        let mut socket = Socket::open(port, &path, token)?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shown = std::sync::Arc::new(std::sync::Mutex::new(Shown::default()));
        let (s, sh) = (std::sync::Arc::clone(&stop), std::sync::Arc::clone(&shown));
        let thread = std::thread::spawn(move || -> anyhow::Result<()> {
            while !s.load(std::sync::atomic::Ordering::Acquire) {
                match socket.next(Instant::now() + Duration::from_millis(500)) {
                    Ok(Some(frame)) => sh.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.apply(&frame)?,
                    Ok(None) => return Ok(()),
                    Err(e) if e.to_string().contains("in time") => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        });
        Ok(Self { stop, shown, thread: Some(thread) })
    }

    fn shown(&self) -> anyhow::Result<Shown> {
        Ok(self.shown.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.clone())
    }

    fn end(mut self) -> anyhow::Result<Shown> {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.join().map_err(|_| anyhow::anyhow!("the viewer's thread panicked"))??;
        }
        self.shown()
    }
}

/// What a viewer that attaches now is shown: its snapshot, up to `synced`.
fn fresh(port: u16, id: &str, token: &str) -> anyhow::Result<Shown> {
    let mut socket = Socket::open(port, &format!("/api/harness/sessions/{id}/ws"), token)?;
    let mut shown = Shown::default();
    let until = Instant::now() + Duration::from_secs(30);
    while !shown.synced {
        let Some(frame) = socket.next(until)? else { anyhow::bail!("the session ended before its snapshot") };
        shown.apply(&frame)?;
    }
    Ok(shown)
}

fn wait_for(what: &str, within: Duration, mut f: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    loop {
        if f()? {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "{what}: not within {within:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A counter's value in the server's metrics.
fn metric(port: u16, token: &str, name: &str) -> anyhow::Result<f64> {
    let (_, body) = common::http(port, "GET", "/metrics", token, None)?;
    Ok(body.lines().find_map(|l| l.strip_prefix(name).and_then(|rest| rest.trim().parse::<f64>().ok())).unwrap_or(0.0))
}

/// The sum of a family's series, its labels whatever they are (`kiln_bus_refused_total{op="…"}`).
fn total(port: u16, token: &str, name: &str) -> anyhow::Result<f64> {
    let (_, body) = common::http(port, "GET", "/metrics", token, None)?;
    let series = |l: &str| l.strip_prefix(name).is_some_and(|rest| rest.starts_with(' ') || rest.starts_with('{'));
    body.lines()
        .filter(|l| series(l))
        .map(|l| l.rsplit_once(' ').map(|(_, v)| v).unwrap_or_default().parse::<f64>().map_err(|e| anyhow::anyhow!("{l}: {e}")))
        .sum()
}

/// A session of `program` started on `target` (`local`, or `agent:<name>`), once it runs on its
/// machine (its harness answered kiln's initialize, so the server's mirror of it is the machine's).
fn session(port: u16, admin: &str, target: &str, work: &Path, program: &str, name: &str) -> anyhow::Result<String> {
    let ask = json!({"target": target, "spec": {"kind": "claude", "cwd": work.to_string_lossy(), "program": program, "name": name}, "count": 1});
    let (st, body) = common::http(port, "POST", "/api/harness/sessions", admin, Some(&ask))?;
    anyhow::ensure!(st == 200, "the session: {st} {body}");
    let infos: Value = serde_json::from_str(&body)?;
    let id = infos.as_array().and_then(|a| a.first()).and_then(|i| i.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(!id.is_empty(), "no session id in {body}");
    wait_for("the session, running on its machine", Duration::from_secs(90), || {
        let (st, body) = common::http(port, "GET", &format!("/api/harness/sessions/{id}"), admin, None)?;
        let status = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("status").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        Ok(st == 200 && matches!(status.as_str(), "idle" | "running" | "waiting"))
    })?;
    Ok(id)
}

// ----------------------------------------------------------------------------------------- a lab

/// A runner of the lab's own, joined to its server, hosting agent sessions; stopped as it's dropped.
struct Runner(std::process::Child);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Runner {
    /// `kiln agent` named `name`, joined to the server whose data is under `dir`, its own files and
    /// home there too.
    fn join(dir: &Path, s: &common::Server, name: &str) -> anyhow::Result<Self> {
        let token = std::fs::read_to_string(dir.join("data").join("join.token"))?;
        let (home, state, work) = (dir.join("runner-home"), dir.join("runner-state"), dir.join("runner-work"));
        for d in [&home, &state, &work] {
            std::fs::create_dir_all(d)?;
        }
        let log = std::fs::File::create(dir.join("runner.log"))?;
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_kiln"));
        cmd.args(["agent", "--server", &format!("http://127.0.0.1:{}", s.port), "--token", token.trim(), "--name", name, "--labels", "lab"])
            .arg("--state-dir")
            .arg(&state)
            .arg("--work-dir")
            .arg(&work)
            .env("HOME", &home)
            .env("NO_COLOR", "1")
            .stdout(log.try_clone()?)
            .stderr(log);
        for var in ["KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_LOG", "KILN_SERVER", "KILN_JOIN_TOKEN", "KILN_AGENT_NAME"] {
            cmd.env_remove(var);
        }
        Ok(Self(cmd.spawn()?))
    }
}

/// A session on the server ("local"), or on a runner of the lab's own (`runner`): watched as its
/// stand-in streams, what each viewer shows checked against what one attaching after is shown.
fn lab(runner: Option<&str>) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let program = stand_in(dir.path())?;
    // Terminals on, and so agent sessions: the server's own.
    let s = common::start(dir.path(), &Options { extra: "[terminal]\nenabled = true\n", ..Options::default() })?;
    let (port, admin) = (s.port, s.admin.clone());
    let (target, _runner) = match runner {
        None => ("local".to_owned(), None),
        Some(name) => {
            let r = Runner::join(dir.path(), &s, name)?;
            wait_for("the lab's runner, connected", Duration::from_secs(90), || {
                let (_, body) = common::http(port, "GET", "/api/agents", &admin, None)?;
                let v: Value = serde_json::from_str(&body).unwrap_or_default();
                let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
                Ok(list.iter().any(|x| x.get("name").and_then(Value::as_str) == Some(name) && x.get("connected").and_then(Value::as_bool) == Some(true)))
            })
            .map_err(|e| anyhow::anyhow!("{e:#}\nthe runner's log:\n{}", std::fs::read_to_string(dir.path().join("runner.log")).unwrap_or_default()))?;
            (format!("agent:{name}"), Some(r))
        }
    };
    // Viewers attach once the session runs where it was started.
    let id = session(port, &admin, &target, &work, &program, "live-feed lab")?;
    let published_before = metric(port, &admin, "kiln_bus_published_total")?;

    // Two viewers from the start: one takes states whole, one as what changed in them.
    let whole = Viewer::watch(port, &id, &admin, false)?;
    let patched = Viewer::watch(port, &id, &admin, true)?;
    wait_for("both viewers sent their snapshots", Duration::from_secs(30), || Ok(whole.shown()?.synced && patched.shown()?.synced))?;
    std::fs::write(dir.path().join("go"), b"")?;

    // The session says all it says: every message whole in a viewer attaching now.
    let want: Vec<String> = (1..=MESSAGES).map(|i| format!("message number {i}")).collect();
    let mut last = Shown::default();
    wait_for("every message, whole, in a snapshot", Duration::from_secs(120), || {
        last = fresh(port, &id, &admin)?;
        let texts = last.texts();
        Ok(want.iter().all(|w| texts.contains(w)))
    })
    .map_err(|e| {
        let listed = common::http(port, "GET", &format!("/api/harness/sessions/{id}"), &admin, None).map(|(_, b)| b).unwrap_or_default();
        let runner_log = std::fs::read_to_string(dir.path().join("runner.log")).unwrap_or_default();
        let runner_tail: Vec<&str> = runner_log.lines().rev().take(60).collect::<Vec<_>>().into_iter().rev().collect();
        anyhow::anyhow!(
            "{e:#}\nthe session, as listed: {listed}\nwhat a fresh viewer was shown: {:?} (state {})\nthe server's log:\n{}\nthe runner's log:\n{}",
            last.texts(),
            last.state,
            s.log(),
            runner_tail.join("\n")
        )
    })?;
    // And the viewers have taken what the feed holds: what they show stops changing, and is that.
    wait_for("the viewers showing what a viewer attaching now is shown", Duration::from_secs(60), || {
        let now = fresh(port, &id, &admin)?;
        let (w, p) = (whole.shown()?, patched.shown()?);
        last = now;
        Ok(w.view() == last.view() && p.view() == last.view())
    })
    .map_err(|e| anyhow::anyhow!("{e:#}\nwhole: {:?}\npatched: {:?}\nfresh: {:?}", whole.shown().map(|s| s.texts()), patched.shown().map(|s| s.texts()), last.texts()))?;
    for (who, v) in [("whole", whole.shown()?), ("patched", patched.shown()?)] {
        anyhow::ensure!(v.synced && v.live > MESSAGES && !v.closed, "the {who} viewer took {} frames, {} of them live", v.frames, v.live);
        // Live all the way: the one snapshot it attached with, never another, so what it shows
        // came over the feed, not from starting over.
        anyhow::ensure!(v.opens == 1, "the {who} viewer started over: {} snapshots", v.opens);
        let texts = v.texts();
        anyhow::ensure!(want.iter().all(|w| texts.contains(w)), "the {who} viewer misses messages: {texts:?}");
    }
    // The frames went over the bus, which refused none and told no viewer it fell behind.
    let published = metric(port, &admin, "kiln_bus_published_total")? - published_before;
    anyhow::ensure!(published >= MESSAGES as f64, "{published} frames published on the bus");
    for name in ["kiln_bus_lag_notices_total", "kiln_bus_refused_total"] {
        let n = total(port, &admin, name)?;
        anyhow::ensure!(n == 0.0, "{name} is {n}");
    }

    // The session ends: both viewers are told.
    let (st, body) = common::http(port, "DELETE", &format!("/api/harness/sessions/{id}"), &admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "closing the session: {st} {body}");
    wait_for("both viewers told the session ended", Duration::from_secs(60), || Ok(whole.shown()?.closed && patched.shown()?.closed))?;
    whole.end()?;
    patched.end()?;
    Ok(())
}

/// T-T2.1.1, the session lab: a session of the server's own, watched live, shows what a viewer
/// attaching after it is shown.
#[test]
fn the_live_feed_in_the_session_lab_shows_what_a_fresh_viewer_is_shown() -> anyhow::Result<()> {
    lab(None)
}

/// T-T2.1.1, the sync lab: a session on a runner of the lab's own, its frames synced from the
/// runner to the server over the runner's link, watched live, shows what a viewer attaching after
/// it is shown.
#[test]
fn the_live_feed_in_the_sync_lab_shows_what_a_fresh_viewer_is_shown() -> anyhow::Result<()> {
    lab(Some("lab-runner"))
}

/// CP-T2.1, test 4: a full bus refuses a feed's frames and the resync marker after them, and every
/// viewer of that feed starts over within 5 s of the refusal, none waiting on silently. The bus is
/// small (256 KiB) and a first session, watched so its feed is kept, fills it with large frames; a
/// topic makes room for a message by letting its own oldest go, never another topic's, so the
/// second session's frames, and its resync marker, find no room and are refused.
#[test]
fn a_full_bus_refusing_a_feed_sends_every_viewer_of_it_back_within_5_s() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    // 60 messages of 8 KiB of text: more than the bus holds, so the filler's feed takes all of it.
    let filler = stand_in_as(dir.path(), "filler", "filler.go", 60, 8192)?;
    let starved = stand_in_as(dir.path(), "starved", "starved.go", MESSAGES, 0)?;
    let s = common::start(dir.path(), &Options { extra: "[terminal]\nenabled = true\n\n[bus]\nmemory = 262144\n", ..Options::default() })?;
    let (port, admin) = (s.port, s.admin.clone());

    let filling = session(port, &admin, "local", &work, &filler, "filler")?;
    let keeper = Viewer::watch(port, &filling, &admin, false)?;
    wait_for("the filler's viewer sent its snapshot", Duration::from_secs(30), || Ok(keeper.shown()?.synced))?;
    std::fs::write(dir.path().join("filler.go"), b"")?;
    wait_for("the bus full", Duration::from_secs(60), || {
        let (used, limit) = (metric(port, &admin, "kiln_bus_memory_bytes")?, metric(port, &admin, "kiln_bus_memory_limit_bytes")?);
        Ok(limit > 0.0 && used >= limit * 0.9)
    })?;

    let starving = session(port, &admin, "local", &work, &starved, "starved")?;
    let viewers = [Viewer::watch(port, &starving, &admin, false)?, Viewer::watch(port, &starving, &admin, true)?];
    wait_for("the starved session's viewers sent their snapshots", Duration::from_secs(30), || {
        for v in &viewers {
            if !v.shown()?.synced {
                return Ok(false);
            }
        }
        Ok(true)
    })?;
    let refused = total(port, &admin, "kiln_bus_refused_total")?;
    let opens = viewers.iter().map(|v| v.shown().map(|s| s.opens)).collect::<anyhow::Result<Vec<_>>>()?;
    std::fs::write(dir.path().join("starved.go"), b"")?;
    wait_for("the starved session's frames refused", Duration::from_secs(60), || Ok(total(port, &admin, "kiln_bus_refused_total")? > refused))?;
    // Seen refused now, and refused no later: every viewer of it starts over (a snapshot more than
    // it had) within 5 s.
    wait_for("every viewer of the starved feed starting over", Duration::from_secs(5), || {
        for (v, before) in viewers.iter().zip(&opens) {
            if v.shown()?.opens <= *before {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .map_err(|e| anyhow::anyhow!("{e:#}\nthe viewers' snapshots: {:?}, before: {opens:?}\nthe server's log:\n{}", viewers.iter().map(|v| v.shown().map(|s| s.opens).unwrap_or(0)).collect::<Vec<_>>(), s.log()))?;
    for v in viewers {
        v.end()?;
    }
    keeper.end()?;
    Ok(())
}
