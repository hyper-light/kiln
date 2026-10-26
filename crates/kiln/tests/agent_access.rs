//! Plan "Agent sessions respect who opened them" (docs/agents.md, "Whose agents"). Two developers
//! and a maintainer, each signed in. A developer reaches only the sessions they opened: another's
//! isn't in their list, and every route, the session's socket and the agents operations answer 404
//! for it. The opener and a maintainer reach it as before. Opening an agent on a runner, and reading
//! the sessions that runner keeps, need runners.terminal there. Commands and closes are audited as
//! the person who made them.
//!
//! `cargo test -p kiln --test agent_access`

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// One request, as the person whose session cookie is `cookie` (none: nobody): the status and the
/// body, and the response's head.
fn call(port: u16, method: &str, path: &str, cookie: Option<&str>, body: Option<&Value>) -> anyhow::Result<(u16, String, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let cookie = cookie.map(|c| format!("Cookie: kiln_session={c}\r\n")).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { common::dechunk(rest) } else { rest.to_owned() };
    Ok((status, body, head.to_owned()))
}

/// A person signed in: their name and session cookie.
struct Person {
    name: &'static str,
    cookie: String,
}

impl Person {
    fn call(&self, s: &Server, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, String)> {
        let (st, b, _) = call(s.port, method, path, Some(&self.cookie), body)?;
        Ok((st, b))
    }
}

/// Adds person `name` with `grants`, and signs them in with the one-time link the owner makes them.
fn person(s: &Server, name: &'static str, grants: &[&str]) -> anyhow::Result<Person> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", &format!("/api/users/{name}/link"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{name}'s link: {st} {body}");
    let url = serde_json::from_str::<Value>(&body)?.get("url").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no link: {body}"))?;
    let path = url.find("/api/").and_then(|at| url.get(at..)).ok_or_else(|| anyhow::anyhow!("a link without a path: {url}"))?;
    let (_, _, head) = call(s.port, "GET", path, None, None)?;
    let cookie = head
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .find_map(|l| l.split_once(':').map(|(_, v)| v.trim()).and_then(|v| v.split(';').next()).and_then(|v| v.strip_prefix("kiln_session=")))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{name} wasn't signed in: {head}"))?;
    Ok(Person { name, cookie })
}

/// Opens a stand-in Claude Code session on runner `box` as `p`: its id. Tries again while the
/// runner hasn't said what it offers yet.
fn open(s: &Server, p: &Person, cwd: &std::path::Path) -> anyhow::Result<String> {
    let ask = json!({"target": "agent:box", "spec": {"kind": "claude", "cwd": cwd.display().to_string(), "prompt": "stream"}});
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let (st, body) = p.call(s, "POST", "/api/harness/sessions", Some(&ask))?;
        if st == 200
            && let Some(id) = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.pointer("/0/id").and_then(Value::as_str).map(str::to_owned))
        {
            return Ok(id);
        }
        anyhow::ensure!(st != 403 && st != 404 && Instant::now() < deadline, "{} couldn't open a session: {st} {body}", p.name);
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// A session's socket as `p`: the handshake's status, and the socket once it's upgraded.
fn socket(s: &Server, p: &Person, id: &str) -> anyhow::Result<(u16, Option<TcpStream>)> {
    let mut t = TcpStream::connect(("127.0.0.1", s.port))?;
    t.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(
        t,
        "GET /api/harness/sessions/{id}/ws HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: kiln_session={}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        p.cookie
    )?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") && head.len() < 16 << 10 {
        head.extend(agents::take(&mut t, 1)?);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Ok((status, (status == 101).then_some(t)))
}

/// A text frame a client sends (masked, RFC 6455 §5.3).
fn send_text(t: &mut TcpStream, text: &str) -> anyhow::Result<()> {
    let payload = text.as_bytes();
    let mut frame = vec![0x81u8];
    match u16::try_from(payload.len()) {
        Ok(n) if n < 126 => frame.push(0x80 | u8::try_from(n)?),
        Ok(n) => {
            frame.push(0x80 | 126);
            frame.extend(n.to_be_bytes());
        }
        Err(_) => {
            frame.push(0x80 | 127);
            frame.extend(u64::try_from(payload.len())?.to_be_bytes());
        }
    }
    let key = [0x5au8, 0x17, 0xc3, 0x09];
    frame.extend(key);
    frame.extend(payload.iter().zip(key.iter().cycle()).map(|(b, k)| b ^ k));
    t.write_all(&frame)?;
    Ok(())
}

/// The audit rows about session `id` (a request to one of its routes, or a row naming it): each its
/// principal, action and detail.
fn audited(s: &Server, id: &str) -> anyhow::Result<Vec<(String, String, String)>> {
    let (st, body) = http(s.port, "GET", "/api/audit?limit=200", &s.admin, None)?;
    anyhow::ensure!(st == 200, "audit: {st} {body}");
    let rows: Vec<Value> = serde_json::from_str(&body)?;
    let s = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    Ok(rows.iter().filter(|r| s(r, "target").contains(id) || s(r, "detail").contains(id)).map(|r| (s(r, "principal"), s(r, "action"), s(r, "detail"))).collect())
}

#[test]
fn a_person_reaches_the_agents_they_opened() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ..Options::default() })?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "box", None, Talk { lines: 3, every_ms: 20 })?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let dev1 = person(&s, "dev1", &["developer", "runners.terminal@runner:box"])?;
    let dev2 = person(&s, "dev2", &["developer", "runners.terminal@runner:box"])?;
    let mia = person(&s, "mia", &["maintainer"])?;
    let mine = open(&s, &dev1, &work)?;
    let theirs = open(&s, &dev2, &work)?;

    // dev2 on dev1's session: not in its list, and 404 through every route, the socket and the
    // agents operations.
    let (st, list) = dev2.call(&s, "GET", "/api/harness/sessions", None)?;
    assert!(st == 200 && list.contains(&theirs) && !list.contains(&mine), "dev2's list: {st} {list}");
    let interrupt = json!({"type": "interrupt"});
    let stop = json!({"agent": mine});
    let tries: [(&str, String, Option<&Value>); 8] = [
        ("GET", format!("/api/harness/sessions/{mine}"), None),
        ("POST", format!("/api/harness/sessions/{mine}/command"), Some(&interrupt)),
        ("GET", format!("/api/harness/sessions/{mine}/goals"), None),
        ("GET", format!("/api/harness/agents/{mine}"), None),
        ("POST", format!("/api/harness/agents/{mine}/interrupt"), Some(&interrupt)),
        ("POST", "/api/ops/agents.get".to_owned(), Some(&stop)),
        ("POST", "/api/ops/agents.stop".to_owned(), Some(&stop)),
        ("DELETE", format!("/api/harness/sessions/{mine}"), None),
    ];
    for (method, path, body) in &tries {
        let (st, said) = dev2.call(&s, method, path, *body)?;
        assert_eq!(st, 404, "dev2: {method} {path}: {said}");
    }
    let (st, _) = socket(&s, &dev2, &mine)?;
    assert_eq!(st, 404, "dev2's socket to dev1's session is refused before it's upgraded");

    // dev1 reaches it: reads it through those same routes and operations, watches it, commands it
    // by its socket and by a route.
    for (method, path, body) in tries.iter().filter(|(m, p, _)| *m == "GET" || p.ends_with("agents.get")) {
        let (st, said) = dev1.call(&s, method, path, *body)?;
        assert_eq!(st, 200, "dev1: {method} {path}: {said}");
    }
    let (st, t) = socket(&s, &dev1, &mine)?;
    let mut t = t.ok_or_else(|| anyhow::anyhow!("dev1's socket wasn't upgraded: {st}"))?;
    let _ = agents::until_synced(&mut t)?;
    send_text(&mut t, &interrupt.to_string())?;
    let (st, said) = dev1.call(&s, "POST", &format!("/api/harness/sessions/{mine}/command"), Some(&interrupt))?;
    assert!((200..300).contains(&st), "dev1 commands their own: {st} {said}");

    // mia, a maintainer, reaches both.
    let (st, list) = mia.call(&s, "GET", "/api/harness/sessions", None)?;
    assert!(st == 200 && list.contains(&mine) && list.contains(&theirs), "mia's list: {st} {list}");
    let (st, said) = mia.call(&s, "GET", &format!("/api/harness/sessions/{theirs}"), None)?;
    assert_eq!(st, 200, "mia reads dev2's: {said}");
    let (st, said) = mia.call(&s, "DELETE", &format!("/api/harness/sessions/{theirs}"), None)?;
    assert!((200..300).contains(&st), "mia closes dev2's: {st} {said}");

    // dev1 closes their own; the rows of their command and close name them, as does the
    // command on their socket.
    let (st, said) = dev1.call(&s, "DELETE", &format!("/api/harness/sessions/{mine}"), None)?;
    assert!((200..300).contains(&st), "dev1 closes their own: {st} {said}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let rows = loop {
        let rows = audited(&s, &mine)?;
        let named = |action: &str| rows.iter().any(|(p, a, d)| p == "user:dev1" && (a.starts_with(action) || d.starts_with(action)));
        if (named("DELETE") && named("POST") && named("agent.command")) || Instant::now() > deadline {
            break rows;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    for action in ["DELETE", "POST", "agent.command"] {
        assert!(rows.iter().any(|(p, a, d)| p == "user:dev1" && (a.starts_with(action) || d.starts_with(action))), "a row of {action} naming dev1: {rows:?}");
    }
    assert!(!rows.iter().any(|(p, _, _)| p == "admin"), "no row of dev1's names admin: {rows:?}");
    Ok(())
}

#[test]
fn opening_and_reading_a_machines_agents_need_its_terminal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ..Options::default() })?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "box", None, Talk { lines: 3, every_ms: 20 })?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let dan = person(&s, "dan", &["developer"])?;

    // Without runners.terminal there: no agent opened, none of what it keeps read, none of what
    // runs on it listed.
    let ask = json!({"target": "agent:box", "spec": {"kind": "claude", "cwd": work.display().to_string(), "prompt": "stream"}});
    let (st, said) = dan.call(&s, "POST", "/api/harness/sessions", Some(&ask))?;
    assert!(st == 403 && said.contains("runners.terminal"), "dan opens an agent on box: {st} {said}");
    for target in ["agent:box", "local"] {
        let (st, said) = dan.call(&s, "GET", &format!("/api/harness/history?target={target}"), None)?;
        assert!(st == 403 && said.contains("runners.terminal"), "dan reads {target}'s history: {st} {said}");
    }
    let (st, found) = dan.call(&s, "GET", "/api/ai-sessions", None)?;
    let found: Value = serde_json::from_str(&found).unwrap_or_default();
    assert!(st == 200 && !found.as_array().into_iter().flatten().any(|f| f.get("machine") == Some(&json!("box"))), "dan's found sessions: {found}");

    // With it, granted on box (and on the server, for its own): each works.
    for grant in ["runners.terminal@runner:box", "runners.terminal"] {
        let (st, body) = http(s.port, "POST", "/api/grants", &s.admin, Some(&json!({ "user": "dan", "grant": grant })))?;
        anyhow::ensure!((200..300).contains(&st), "granting {grant}: {st} {body}");
    }
    let id = open(&s, &dan, &work)?;
    let (st, said) = dan.call(&s, "GET", &format!("/api/harness/sessions/{id}"), None)?;
    assert_eq!(st, 200, "dan reads the agent he opened: {said}");
    for target in ["agent:box", "local"] {
        let (st, said) = dan.call(&s, "GET", &format!("/api/harness/history?target={target}"), None)?;
        assert!(st != 403, "dan reads {target}'s history: {st} {said}");
    }
    Ok(())
}
