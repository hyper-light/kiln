//! Typed messages, step 6 (docs/wire.md, "What can't be read"): the UI's sockets answer what they
//! can't read. An agent session's socket, the events socket, a runner's terminal, a live page of
//! the session's (a stream), a CI leg's log and the leg's terminal are each sent unreadable JSON, an
//! unknown tag, a binary frame where only text is taken (a terminal takes binary frames: they're
//! keystrokes), a frame the page couldn't read of what came down (`unread`), and then a frame past
//! the socket's limit. Each is answered typed (`nack`, `refused`) or closed with 1009, counted in
//! `kiln_wire_refused_total`, audited as `security.wire_refused` naming the person, and flagged on
//! its page: the agent's for the session's sockets (its live page's too), the runner's for its
//! terminals (its legs' too; the events and log sockets are no agent's or runner's). A runner's
//! tunnel, which takes datagrams, bindings and `unread`, is sent text that isn't a binding, a
//! binding cut short, an `unread`, an empty datagram and a frame past its bound: answered and noted
//! the same, from its address (it's open, and nobody's yet).
//!
//! A deployment's log socket needs a cluster running the app's pods, which this test doesn't have:
//! `cd::live`'s unit test sends it the same cases, its pod's log followed from a stand-in for
//! Kubernetes's API.
//!
//! The harness is a stand-in for Claude Code (`tests/fixtures/fake-claude`): it calls no model. The
//! runner's preview browser is a stand-in too, that never answers: kiln waits 45 s for a browser to
//! start, and the live page stays open meanwhile.
//!
//! `cargo test -p kiln --test hostile_sockets`

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http};
use serde_json::{Value, json};

/// How long an answer may take.
const WITHIN: Duration = Duration::from_secs(20);

/// What a page sends up for a frame that came down that it couldn't read.
const UNREAD: &str = r#"{"t":"unread","of":"hologram","why":"unknown frame"}"#;

/// A page's socket: what it sends, and its frames as they come (opcode, payload), read on a thread
/// of their own.
struct Sock {
    to: TcpStream,
    frames: mpsc::Receiver<(u8, Vec<u8>)>,
}

impl Sock {
    /// A socket to `path`, opened with the request header `auth` (`Authorization: ...`, `Cookie: ...`).
    fn open(port: u16, auth: &str, path: &str) -> anyhow::Result<Self> {
        let mut s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_read_timeout(Some(Duration::from_secs(120)))?;
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )?;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.extend(agents::take(&mut s, 1)?);
            anyhow::ensure!(head.len() < 16 << 10, "no handshake answer: {}", String::from_utf8_lossy(&head));
        }
        anyhow::ensure!(head.starts_with(b"HTTP/1.1 101"), "not upgraded: {}", String::from_utf8_lossy(&head));
        let to = s.try_clone()?;
        let (tx, frames) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(f) = agents::frame(&mut s) {
                let close = f.0 == 8;
                if tx.send(f).is_err() || close {
                    return;
                }
            }
        });
        Ok(Self { to, frames })
    }

    fn text(&mut self, t: &str) -> anyhow::Result<()> {
        agents::send_text(&mut self.to, t)
    }

    /// A binary frame, masked as a client's are (RFC 6455, §5.2).
    fn binary(&mut self, b: &[u8]) -> anyhow::Result<()> {
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut f = vec![0x82u8, 0x80 | u8::try_from(b.len())?];
        f.extend(mask);
        f.extend(b.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
        self.to.write_all(&f)?;
        Ok(())
    }

    /// The head of a text frame of 17 MiB, past every page's socket's limit; its payload never comes.
    fn past_limit(&mut self) -> anyhow::Result<()> {
        let mut f = vec![0x81u8, 0x80 | 127];
        f.extend(u64::try_from(17usize << 20)?.to_be_bytes());
        f.extend([0x37u8, 0xfa, 0x21, 0x3d]);
        self.to.write_all(&f)?;
        Ok(())
    }

    /// The first text frame within `within` that `want` holds for, the frames before it let go.
    fn text_where(&self, within: Duration, want: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).ok_or_else(|| anyhow::anyhow!("no such frame within {within:?}"))?;
            let (op, payload) = self.frames.recv_timeout(left)?;
            anyhow::ensure!(op != 8, "the socket closed: {payload:?}");
            if op == 1
                && let Ok(v) = serde_json::from_slice::<Value>(&payload)
                && want(&v)
            {
                return Ok(v);
            }
        }
    }

    /// The code the socket was closed with, within `within`.
    fn closed(&self, within: Duration) -> anyhow::Result<u16> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).ok_or_else(|| anyhow::anyhow!("not closed within {within:?}"))?;
            let (op, payload) = self.frames.recv_timeout(left)?;
            if op == 8 {
                let code = payload.first_chunk::<2>().map(|c| u16::from_be_bytes(*c)).ok_or_else(|| anyhow::anyhow!("a close without a code"))?;
                return Ok(code);
            }
        }
    }
}

/// `v`'s field `k` as text.
fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// Signs in person `name` with `grants` through the one-time link the owner makes them: their
/// session cookie.
fn person(srv: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(srv.port, "POST", "/api/users", &srv.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(srv.port, "POST", &format!("/api/users/{name}/link"), &srv.admin, None)?;
    anyhow::ensure!(st == 200, "{name}'s link: {st} {body}");
    let url = serde_json::from_str::<Value>(&body)?.get("url").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no link: {body}"))?;
    let path = url.find("/api/").and_then(|at| url.get(at..)).ok_or_else(|| anyhow::anyhow!("a link without a path: {url}"))?;
    let mut c = TcpStream::connect(("127.0.0.1", srv.port))?;
    c.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(c, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", srv.port)?;
    let mut head = String::new();
    c.read_to_string(&mut head)?;
    head.lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .find_map(|l| l.split_once(':').map(|(_, v)| v.trim()).and_then(|v| v.split(';').next()).and_then(|v| v.strip_prefix("kiln_session=")))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{name} wasn't signed in: {head}"))
}

/// `kiln_wire_refused_total` for `wire` and `code`, as the server exports it.
fn counted(srv: &Server, wire: &str, code: &str) -> anyhow::Result<u64> {
    let (st, body) = http(srv.port, "GET", "/metrics", &srv.admin, None)?;
    anyhow::ensure!(st == 200, "metrics: {st}");
    let series = format!("kiln_wire_refused_total{{wire=\"{wire}\",code=\"{code}\"}} ");
    Ok(body.lines().find_map(|l| l.strip_prefix(&series)).and_then(|n| n.trim().parse().ok()).unwrap_or(0))
}

/// The refusals kept for a page (`refused`, as `GET /api/agents` and a session's info carry it):
/// how many of `wire` and `code`.
fn kept(refused: Option<&Value>, wire: &str, code: &str) -> u64 {
    refused
        .and_then(|r| r.get("counts"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|c| s(c, "wire") == wire && s(c, "code") == code)
        .and_then(|c| c.get("n").and_then(Value::as_u64))
        .unwrap_or(0)
}

/// Waits until `check` holds, saying `what` it waited for if it doesn't.
fn eventually(what: &str, check: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    eventually_within(WITHIN, what, check)
}

/// [`eventually`], waiting `within`.
fn eventually_within(within: Duration, what: &str, mut check: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    while !check()? {
        anyhow::ensure!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// `git` in `dir`, with no configuration but this test's.
fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// The file that lets the test's CI leg end: written when dropped, however the test ends.
struct Release(PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "");
    }
}

/// The newest run of `repo`'s leg, once its first step has said `held`: its log is live.
fn held_leg(srv: &Server, repo: &str) -> anyhow::Result<String> {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let (_, body) = http(srv.port, "GET", &format!("/api/ci/runs?repo={repo}&limit=5"), &srv.admin, None)?;
        let run = serde_json::from_str::<Value>(&body).unwrap_or_default().pointer("/runs/0/id").and_then(Value::as_str).map(str::to_owned);
        if let Some(run) = run {
            let (_, detail) = http(srv.port, "GET", &format!("/api/ci/runs/{run}"), &srv.admin, None)?;
            let leg = serde_json::from_str::<Value>(&detail).unwrap_or_default().pointer("/legs/0/id").and_then(Value::as_str).map(str::to_owned);
            if let Some(leg) = leg {
                let (_, log) = http(srv.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &srv.admin, None)?;
                if log.contains("held") {
                    return Ok(leg);
                }
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "no leg of {repo} held: {body}\n{}", srv.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A terminal's socket at `path`, sent what it can't read: answered `refused` (its binary frames
/// are keystrokes), then closed with 1009.
fn hostile_terminal(srv: &Server, auth: &str, path: &str) -> anyhow::Result<()> {
    let refused = |v: &Value| s(v, "type") == "refused";
    let mut term = Sock::open(srv.port, auth, path)?;
    term.text_where(WITHIN, |v| s(v, "type") == "session")?;
    term.text("not json")?;
    let got = term.text_where(WITHIN, refused)?;
    anyhow::ensure!((s(&got, "of"), s(&got, "code")) == ("text", "malformed"), "{path}, not JSON: {got}");
    term.text(r#"{"type":"teleport"}"#)?;
    let got = term.text_where(WITHIN, refused)?;
    anyhow::ensure!((s(&got, "of"), s(&got, "code"), s(&got, "why")) == ("teleport", "unknown", "no frame teleport"), "{path}, an unknown tag: {got}");
    term.text(UNREAD)?;
    term.past_limit()?;
    anyhow::ensure!(term.closed(WITHIN)? == 1009, "past its limit, {path} closes with 1009");
    Ok(())
}

/// A socket at `path` whose only frame up is `unread` (the events and log sockets), sent what it
/// can't read: each answered with a `refused` frame, then closed with 1009.
fn hostile_unread_only(srv: &Server, auth: &str, path: &str) -> anyhow::Result<()> {
    let refused = |v: &Value| s(v, "type") == "refused";
    let mut sock = Sock::open(srv.port, auth, path)?;
    sock.text("not json")?;
    let got = sock.text_where(WITHIN, refused)?;
    anyhow::ensure!((s(&got, "of"), s(&got, "code")) == ("text", "malformed"), "{path}, not JSON: {got}");
    sock.text(r#"{"t":"hologram"}"#)?;
    let got = sock.text_where(WITHIN, refused)?;
    anyhow::ensure!((s(&got, "of"), s(&got, "code"), s(&got, "why")) == ("hologram", "unknown", "no frame hologram"), "{path}, an unknown tag: {got}");
    sock.binary(&[0, 1, 2])?;
    let got = sock.text_where(WITHIN, refused)?;
    anyhow::ensure!((s(&got, "of"), s(&got, "code")) == ("binary", "malformed"), "{path}, binary: {got}");
    sock.text(UNREAD)?;
    sock.past_limit()?;
    anyhow::ensure!(sock.closed(WITHIN)? == 1009, "past its limit, {path} closes with 1009");
    Ok(())
}

#[test]
fn every_socket_answers_what_it_cant_read_and_notes_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // A repository whose workflow holds a CI leg running on the runner, until the test lets it go.
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join("README.md"), "hostile\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[terminal]\nenabled = true\n\n[[repos]]\nname = \"acme/hostile\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let srv = common::start(dir.path(), &Options { agent: false, ci: "kiln_workflows = true", extra: &extra })?;
    // The runner's preview browser: a stand-in that never answers.
    let browser = kiln::browser::install::exe_in(&dir.path().join("box-state"))?;
    std::fs::create_dir_all(browser.parent().ok_or_else(|| anyhow::anyhow!("{} has no folder", browser.display()))?)?;
    std::fs::write(&browser, "#!/bin/sh\nexec sleep 60\n")?;
    std::fs::set_permissions(&browser, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let (_runner, log) = agents::runner_with(dir.path(), srv.port, "box", None, Talk { lines: 1, every_ms: 10 }, &[("KILN_LABELS", "box")])?;
    agents::connected(srv.port, &srv.admin, "box", &log, Duration::from_secs(120))?;
    let release = Release(dir.path().join("release"));
    eventually_within(Duration::from_secs(120), "the repository wasn't polled", || {
        let (_, repos) = http(srv.port, "GET", "/api/repos", &srv.admin, None)?;
        Ok(!repos.contains("\"last_polled_at\":null"))
    })?;
    let hold = format!(
        "on: push\njobs:\n  hold:\n    runs-on: box\n    checkout: false\n    steps:\n      - shell: sh\n        run: echo held; for i in $(seq 1 600); do [ -f \"{}\" ] && break; sleep 1; done\n",
        release.0.display()
    );
    std::fs::write(repo.join(".kiln/hold.yaml"), hold)?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "hold"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let id = agents::open_session(srv.port, &srv.admin, "box", &work, Duration::from_secs(120))?;
    let admin = format!("Authorization: Bearer {}", srv.admin);

    // An agent session's socket answers each with a nack (the plan's example among them).
    let mut ws = Sock::open(srv.port, &admin, &format!("/api/harness/sessions/{id}/ws"))?;
    ws.text_where(WITHIN, |v| s(v, "t") == "synced")?;
    let nack = |v: &Value| s(v, "t") == "nack";
    ws.text("not json")?;
    let got = ws.text_where(WITHIN, nack)?;
    anyhow::ensure!(s(&got, "code") == "malformed" && got.get("id") == Some(&Value::Null), "not JSON: {got}");
    ws.text(r#"{"t":"cmd","id":"c1","c":{"type":"teleport"}}"#)?;
    let got = ws.text_where(WITHIN, nack)?;
    anyhow::ensure!(got == json!({"t": "nack", "id": "c1", "code": "unsupported", "why": "no command teleport", "again": false}), "teleport: {got}");
    ws.binary(&[0, 1, 2])?;
    let got = ws.text_where(WITHIN, nack)?;
    anyhow::ensure!(s(&got, "code") == "malformed" && got.get("id") == Some(&Value::Null), "binary: {got}");
    ws.text(UNREAD)?;
    ws.past_limit()?;
    anyhow::ensure!(ws.closed(WITHIN)? == 1009, "past its limit, the session socket closes with 1009");

    // A live page of the session's: its own `refused` frame each, noted on the agent's page.
    let (st, body) = http(srv.port, "POST", &format!("/api/harness/sessions/{id}/streams"), &srv.admin, Some(&json!({ "url": "about:blank" })))?;
    anyhow::ensure!(st == 201, "a live page: {st} {body}");
    let sid = serde_json::from_str::<Value>(&body)?.get("id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("a live page without an id: {body}"))?;
    let mut page = Sock::open(srv.port, &admin, &format!("/api/harness/sessions/{id}/streams/{sid}/ws"))?;
    let refused_page = |v: &Value| v.get("refused").is_some();
    let of = |v: &Value, k: &str| v.get("refused").map(|r| s(r, k).to_owned()).unwrap_or_default();
    page.text("not json")?;
    let got = page.text_where(WITHIN, refused_page)?;
    anyhow::ensure!((of(&got, "of"), of(&got, "code")) == ("text".into(), "malformed".into()), "a live page, not JSON: {got}");
    page.text(r#"{"t":"hologram"}"#)?;
    let got = page.text_where(WITHIN, refused_page)?;
    anyhow::ensure!((of(&got, "of"), of(&got, "code"), of(&got, "why")) == ("hologram".into(), "unknown".into(), "no frame hologram".into()), "a live page, an unknown tag: {got}");
    page.binary(&[0, 1, 2])?;
    let got = page.text_where(WITHIN, refused_page)?;
    anyhow::ensure!((of(&got, "of"), of(&got, "code")) == ("binary".into(), "malformed".into()), "a live page, binary: {got}");
    page.text(UNREAD)?;
    page.past_limit()?;
    anyhow::ensure!(page.closed(WITHIN)? == 1009, "past its limit, a live page's socket closes with 1009");

    // The events socket, a person's: a `refused` frame each.
    let ada = person(&srv, "ada", &["viewer"])?;
    hostile_unread_only(&srv, &format!("Cookie: kiln_session={ada}"), "/api/events")?;

    // A terminal on the runner: a `refused` frame each; its binary frames are keystrokes.
    hostile_terminal(&srv, &admin, "/api/terminal/ws?target=agent:box&cols=80&rows=24")?;

    // A runner's tunnel: a `refused` text frame each, which runners read; an `unread` is noted,
    // and nothing goes back (the next `refused` is the empty datagram's).
    let refused = |v: &Value| s(v, "type") == "refused";
    let mut tunnel = Sock::open(srv.port, "X-Kiln-Test: hostile", "/api/agents/tunnel")?;
    tunnel.text("hello")?;
    let got = tunnel.text_where(WITHIN, refused)?;
    anyhow::ensure!((s(&got, "of"), s(&got, "code"), s(&got, "why")) == ("hello", "unknown", "no frame hello"), "not a binding: {got}");
    tunnel.text("bind 00")?;
    let got = tunnel.text_where(WITHIN, refused)?;
    anyhow::ensure!((s(&got, "of"), s(&got, "code")) == ("bind", "malformed"), "a binding cut short: {got}");
    tunnel.text(UNREAD)?;
    tunnel.binary(&[])?;
    let got = tunnel.text_where(WITHIN, refused)?;
    anyhow::ensure!((s(&got, "of"), s(&got, "code")) == ("datagram", "malformed"), "an empty datagram (an unread is answered nothing): {got}");
    tunnel.past_limit()?;
    anyhow::ensure!(tunnel.closed(WITHIN)? == 1009, "past its bound, a tunnel closes with 1009");

    // The CI leg's log, and its terminal (on its runner's page).
    let leg = held_leg(&srv, "acme/hostile")?;
    hostile_unread_only(&srv, &admin, &format!("/api/jobs/{leg}/logs/ws?step=0"))?;
    hostile_terminal(&srv, &admin, &format!("/api/ci/legs/{leg}/terminal?cols=80&rows=24"))?;
    drop(release);

    // Counted, by wire and code.
    let want = [
        ("session_up", "malformed", 3),
        ("session_up", "unknown", 1),
        ("session_down", "unknown", 1),
        ("stream_up", "malformed", 3),
        ("stream_up", "unknown", 1),
        ("stream_down", "unknown", 1),
        ("events_up", "malformed", 3),
        ("events_up", "unknown", 1),
        ("events_down", "unknown", 1),
        ("terminal_up", "malformed", 4),
        ("terminal_up", "unknown", 2),
        ("terminal_down", "unknown", 2),
        ("tunnel_up", "unknown", 1),
        ("tunnel_up", "malformed", 3),
        ("tunnel_down", "unknown", 1),
        ("log_up", "malformed", 3),
        ("log_up", "unknown", 1),
        ("log_down", "unknown", 1),
    ];
    for (wire, code, n) in want {
        eventually(&format!("kiln_wire_refused_total{{wire={wire},code={code}}} isn't {n}"), || Ok(counted(&srv, wire, code)? == n))?;
    }

    // Audited, naming the person: the first of each wire and code in a minute at once.
    let (st, body) = http(srv.port, "GET", "/api/audit?action=security.wire_refused&limit=200", &srv.admin, None)?;
    anyhow::ensure!(st == 200, "audit: {st} {body}");
    let rows: Vec<Value> = serde_json::from_str(&body)?;
    let row = |principal: &str, wire: &str, code: &str| rows.iter().find(|r| s(r, "principal") == principal && s(r, "target") == wire && s(r, "result") == code).cloned();
    for (principal, wire, code) in [
        ("admin", "session_up", "malformed"),
        ("admin", "session_up", "unknown"),
        ("admin", "session_down", "unknown"),
        ("admin", "stream_up", "malformed"),
        ("admin", "stream_up", "unknown"),
        ("admin", "stream_down", "unknown"),
        ("user:ada", "events_up", "malformed"),
        ("user:ada", "events_up", "unknown"),
        ("user:ada", "events_down", "unknown"),
        ("admin", "terminal_up", "malformed"),
        ("admin", "terminal_up", "unknown"),
        ("admin", "terminal_down", "unknown"),
        ("address:127.0.0.1", "tunnel_up", "unknown"),
        ("address:127.0.0.1", "tunnel_up", "malformed"),
        ("address:127.0.0.1", "tunnel_down", "unknown"),
        ("admin", "log_up", "malformed"),
        ("admin", "log_up", "unknown"),
        ("admin", "log_down", "unknown"),
    ] {
        anyhow::ensure!(row(principal, wire, code).is_some(), "no audit row for {principal}, {wire}, {code}: {rows:?}");
    }
    let teleport = row("admin", "session_up", "unknown").unwrap_or_default();
    anyhow::ensure!(s(&teleport, "detail") == "teleport: no command teleport", "{teleport}");

    // Flagged on its page: the session's sockets' (its live page's too) on the agent's, the
    // terminals' (its leg's too) on the runner's.
    eventually("the agent's page doesn't show what its sockets refused", || {
        let (_, body) = http(srv.port, "GET", &format!("/api/harness/sessions/{id}"), &srv.admin, None)?;
        let info: Value = serde_json::from_str(&body).unwrap_or_default();
        let r = info.get("refused");
        Ok([("session_up", "malformed", 3), ("session_up", "unknown", 1), ("session_down", "unknown", 1), ("stream_up", "malformed", 3), ("stream_up", "unknown", 1), ("stream_down", "unknown", 1)]
            .iter()
            .all(|(w, c, n)| kept(r, w, c) == *n))
    })?;
    eventually("the runner's page doesn't show what its terminals refused", || {
        let (_, body) = http(srv.port, "GET", "/api/agents", &srv.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        let r = list.iter().find(|a| s(a, "name") == "box").and_then(|a| a.get("refused"));
        Ok(kept(r, "terminal_up", "malformed") == 4 && kept(r, "terminal_up", "unknown") == 2 && kept(r, "terminal_down", "unknown") == 2)
    })?;
    Ok(())
}
