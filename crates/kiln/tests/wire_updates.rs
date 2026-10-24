//! Plan step 4 of "Nothing kiln sends between its processes is fire-and-forget" (docs/wire.md):
//! an agent session's updates go over the channel, and survive a cut link.
//!
//! A runner's link to the server is cut for 30 seconds at a time (`KILN_WIRE_CUT`) while its
//! session streams. Every update still reaches the server's copy of the session once, in order,
//! and the session is never sent whole again. A terminal printing through the same cuts reaches
//! its viewer byte for byte: every line once, in order.
//!
//! The harness is a stand-in for Claude Code (`tests/fixtures/fake-claude`): it calls no model.
//!
//! `cargo test -p kiln --features chaos --test wire_updates`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Options, http, start};
use serde_json::{Value, json};

/// The runner, stopped when the test ends.
struct Runner(Child);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A runner of the server at `port`, its link cut for `down` every `every`, the stand-in harness
/// first on its PATH; its log at `log`.
fn runner(dir: &std::path::Path, port: u16, name: &str, every: &str, down: &str, lines: usize) -> anyhow::Result<(Runner, std::path::PathBuf)> {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    let fake = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude/claude");
    std::fs::copy(&fake, bin.join("claude"))?;
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let log_path = dir.join("runner.log");
    let log = std::fs::File::create(&log_path)?;
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let child = Command::new(env!("CARGO_BIN_EXE_kiln"))
        .args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", name, "--chaos", &format!("drop={every},down={down}")])
        .arg("--work-dir")
        .arg(dir.join("runner-work"))
        .arg("--state-dir")
        .arg(dir.join("runner-state"))
        .env("HOME", &home)
        .env("PATH", &path)
        .env("NO_COLOR", "1")
        .env("FAKE_CLAUDE_LINES", lines.to_string())
        .env("FAKE_CLAUDE_EVERY_MS", "100")
        .env_remove("KILN_REACH_SOCK")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    Ok((Runner(child), log_path))
}

/// How long each cut lasts: 30 seconds, or `KILN_WIRE_CUT` (`60s` shows a minute's cut holds).
fn cut() -> String {
    std::env::var("KILN_WIRE_CUT").unwrap_or_else(|_| "30s".to_owned())
}

/// Waits for the runner's next connection to begin, then a moment more.
fn just_connected(log: &std::path::Path, deadline: Instant) {
    let count = || std::fs::read_to_string(log).unwrap_or_default().matches("connected to server").count();
    let seen = count();
    while count() == seen && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
}

/// One WebSocket frame from the server (unmasked): its opcode and payload.
fn frame(s: &mut std::net::TcpStream) -> anyhow::Result<(u8, Vec<u8>)> {
    let h = take(s, 2)?;
    let (b0, b1) = (h.first().copied().unwrap_or(0), h.get(1).copied().unwrap_or(0));
    let len = match b1 & 0x7f {
        126 => take(s, 2)?.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b)),
        127 => take(s, 8)?.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b)),
        n => usize::from(n),
    };
    Ok((b0 & 0x0f, take(s, len)?))
}

/// A WebSocket to `path` on the server at `port`, upgraded.
fn socket(port: u16, token: &str, path: &str) -> anyhow::Result<std::net::TcpStream> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.extend(take(&mut s, 1)?);
        anyhow::ensure!(head.len() < 16 << 10, "no handshake answer: {}", String::from_utf8_lossy(&head));
    }
    anyhow::ensure!(head.starts_with(b"HTTP/1.1 101"), "not upgraded: {}", String::from_utf8_lossy(&head));
    Ok(s)
}

/// Reads exactly `n` bytes.
fn take(s: &mut std::net::TcpStream, n: usize) -> anyhow::Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    s.read_exact(&mut b)?;
    Ok(b)
}

/// What a viewer gets on attaching to session `id`: the server's text frames (unmasked), up to
/// the `synced` that ends the snapshot.
fn snapshot(port: u16, token: &str, id: &str) -> anyhow::Result<Vec<Value>> {
    let mut s = socket(port, token, &format!("/api/harness/sessions/{id}/ws"))?;
    let mut frames = Vec::new();
    loop {
        let (op, payload) = frame(&mut s)?;
        if op != 1 {
            continue;
        }
        let v: Value = serde_json::from_slice(&payload)?;
        let synced = v.get("t").and_then(Value::as_str) == Some("synced");
        frames.push(v);
        if synced {
            return Ok(frames);
        }
    }
}

#[test]
fn a_sessions_updates_survive_a_cut_link() -> anyhow::Result<()> {
    const LINES: usize = 400;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: "" })?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    // Its link drops six seconds into every connection and stays cut for thirty (`cut`).
    let (_runner, log_path) = runner(dir.path(), s.port, "wire-runner", "6s", &cut(), LINES)?;
    let runner_log = || std::fs::read_to_string(&log_path).unwrap_or_default();

    // It connects, and offers the stand-in.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
        if body.contains("wire-runner") && body.contains("\"connected\":true") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the runner never connected: {body}\n{}\n{}", runner_log(), s.log());
        std::thread::sleep(Duration::from_millis(300));
    }

    // A session there, streaming through the cuts. It's opened just after the runner connects
    // again: opening isn't on the channel yet (plan step 3), so a cut as it goes would lose it.
    let open = json!({"target": "agent:wire-runner", "spec": {"kind": "claude", "cwd": work.display().to_string(), "prompt": "stream"}});
    let deadline = Instant::now() + Duration::from_secs(90);
    let id = loop {
        just_connected(&log_path, deadline);
        let (code, body) = http(s.port, "POST", "/api/harness/sessions", &s.admin, Some(&open))?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if code == 200
            && let Some(id) = v.pointer("/0/id").and_then(Value::as_str)
        {
            break id.to_owned();
        }
        // The runner may be between connections, or not have said what it offers yet.
        anyhow::ensure!(Instant::now() < deadline, "couldn't open a session: {code} {body}\n{}", runner_log());
    };

    // Every line in the server's copy once, in order, once the turn has ended and the runner is
    // connected again.
    let deadline = Instant::now() + Duration::from_secs(420);
    let lines = loop {
        // Its turn has ended (its status says whether its machine is connected, not that).
        let (_, info) = http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
        let info: Value = serde_json::from_str(&info).unwrap_or_default();
        let done = info.pointer("/last_turn/how").and_then(Value::as_str) == Some("done");
        if done {
            let frames = snapshot(s.port, &s.admin, &id)?;
            // The timeline's events, in order: each line the line of the event holding it.
            let events: Vec<String> = frames
                .iter()
                .filter(|f| f.get("t").and_then(Value::as_str) == Some("events"))
                .flat_map(|f| f.get("events").and_then(Value::as_array).cloned().unwrap_or_default())
                .map(|e| e.to_string())
                .collect();
            let lines: Vec<Vec<usize>> = (0..LINES).map(|n| events.iter().enumerate().filter(|(_, e)| e.contains(&format!("line {n:05}"))).map(|(i, _)| i).collect()).collect();
            if lines.iter().all(|at| !at.is_empty()) {
                break lines;
            }
        }
        if Instant::now() >= deadline {
            let (_, info) = http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
            let frames = snapshot(s.port, &s.admin, &id).map(|f| f.iter().map(Value::to_string).map(|t| t.chars().take(600).collect::<String>()).collect::<Vec<_>>().join("\n")).unwrap_or_else(|e| format!("{e:#}"));
            anyhow::bail!("the session's copy never held every line\nsession: {info}\nsnapshot:\n{frames}\nrunner:\n{}\nserver:\n{}", runner_log(), s.log());
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    let doubled: Vec<usize> = lines.iter().enumerate().filter(|(_, at)| at.len() > 1).map(|(n, _)| n).collect();
    assert!(doubled.is_empty(), "lines in more than one event of the server's copy: {doubled:?}");
    let order: Vec<usize> = lines.iter().filter_map(|at| at.first().copied()).collect();
    assert!(order.windows(2).all(|w| w.first() < w.get(1)), "lines out of order in the server's copy: {order:?}");
    // The link was cut while it streamed, and the session never went whole again.
    let log = runner_log();
    let cuts = log.matches("chaos: dropping the connection to the server").count();
    assert!(cuts >= 2, "the link was cut {cuts} times; the test needs it cut while the session streams:\n{log}");
    let whole = log.lines().filter(|l| l.contains("updates start on the channel, whole") && l.contains(&id)).count();
    assert_eq!(whole, 0, "the session went whole {whole} times over {cuts} cuts:\n{log}");
    // And the server's copy started from what the runner sent once: its first Open.
    let opens = s.log().lines().filter(|l| l.contains("an agent session's copy starts from its machine") && l.contains(&id)).count();
    assert_eq!(opens, 1, "the server's copy started over {opens} times over {cuts} cuts");
    Ok(())
}

#[test]
fn a_terminal_reaches_its_viewer_whole_through_cut_links() -> anyhow::Result<()> {
    const LINES: usize = 300;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: "[terminal]\nenabled = true\n" })?;
    let (_runner, log_path) = runner(dir.path(), s.port, "term-runner", "6s", &cut(), 1)?;
    let runner_log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
        if body.contains("term-runner") && body.contains("\"connected\":true") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the runner never connected: {body}\n{}", runner_log());
        std::thread::sleep(Duration::from_millis(300));
    }
    // A terminal printing a line every tenth of a second for thirty, opened as a connection begins
    // (opening isn't on the channel yet, plan step 3), watched throughout.
    just_connected(&log_path, deadline);
    let cmd = format!("for i in $(seq 1 {LINES}); do echo t$i.; sleep 0.1; done");
    let q: String = cmd.bytes().map(|b| if b.is_ascii_alphanumeric() { char::from(b).to_string() } else { format!("%{b:02X}") }).collect();
    let mut ws = socket(s.port, &s.admin, &format!("/api/terminal/ws?target=agent:term-runner&cols=120&rows=40&cmd={q}"))?;
    let mut screen = Vec::new();
    let mut said = Vec::new();
    let until = Instant::now() + Duration::from_secs(420);
    while Instant::now() < until {
        let (op, payload) = match frame(&mut ws) {
            Ok(f) => f,
            Err(e) => {
                said.push(format!("read: {e:#}"));
                break;
            }
        };
        match op {
            2 => screen.extend(payload),
            1 => {
                let text = String::from_utf8_lossy(&payload).into_owned();
                let exit = text.contains("\"exit\"");
                said.push(text);
                if exit {
                    break;
                }
            }
            8 => {
                said.push("closed".into());
                break;
            }
            _ => {}
        }
        if String::from_utf8_lossy(&screen).contains(&format!("t{LINES}.")) {
            break;
        }
    }
    // Its lines as the viewer got them: each `t<n>.`, in order.
    let text = String::from_utf8_lossy(&screen).into_owned();
    let mut seen = Vec::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find('t') {
        let after = rest.get(at + 1..).unwrap_or_default();
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && after.get(digits.len()..).is_some_and(|r| r.starts_with('.')) {
            seen.push(digits.parse::<usize>().unwrap_or(0));
        }
        rest = after;
    }
    let want: Vec<usize> = (1..=LINES).collect();
    assert_eq!(seen, want, "the viewer's lines aren't the terminal's, each once in order\nthe socket said: {said:?}\n{}", runner_log());
    let cuts = runner_log().matches("chaos: dropping the connection to the server").count();
    assert!(cuts >= 1, "the link was never cut while the terminal printed");
    Ok(())
}
