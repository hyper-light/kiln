//! The browser protocol over one WebSocket, end to end (goal 3 of the browser protocol program, plan
//! `01a1a832eb2f711f9c6f51d88effe89d` step 2): a server with `[server] live` on (`KILN_LIVE=1`), its
//! endpoint `GET /api/live` beside today's sockets.
//!
//! - The handshake: no credentials 401; an Origin not the server's 403; no version it speaks 400
//!   naming those it does; permessage-deflate offered and never echoed; the version taken named.
//! - The protocol through goal 2's Rust client (`kiln_bp::client::Client`) over that socket: open
//!   with a ticket, a subscription's snapshot and then a change the hub made (the queue's counts,
//!   which an events socket opening beside it has the hub say), a request answered by the API's
//!   own route, and a malformed frame answered typed with the socket staying open.
//! - Fuzzed upgrades and first frames: the server answers each and never panics.
//!
//! `cargo test -p kiln --test live`

#![cfg(unix)]

mod common;

use std::io::Write as _;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use common::{Options, agents, http};
use kiln_bp::client::{Client, ClientConfig, ClientEvent};
use kiln_bp::frames::{Call, Method, Outcome, Topic};
use serde_json::Value;

/// How long an answer may take.
const WITHIN: Duration = Duration::from_secs(30);

/// A GET of `/api/live` with `headers`: the answer's head (status line and headers, lowercased
/// names), its body as far as it came, and the connection.
fn upgrade(port: u16, headers: &str) -> anyhow::Result<(String, TcpStream)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(WITHIN))?;
    write!(s, "GET /api/live HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.extend(agents::take(&mut s, 1)?);
        anyhow::ensure!(head.len() < 16 << 10, "no answer: {}", String::from_utf8_lossy(&head));
    }
    Ok((String::from_utf8_lossy(&head).into_owned(), s))
}

/// The status of an answer's head.
fn status(head: &str) -> u16 {
    head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

/// A header's value in an answer's head, by its name in any case.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| l.split_once(':').filter(|(n, _)| n.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
}

/// Sends `payload` as a client's binary message, masked.
fn send_binary(s: &mut TcpStream, payload: &[u8]) -> anyhow::Result<()> {
    let mut f = vec![0x82u8];
    match payload.len() {
        n if n < 126 => f.push(0x80 | u8::try_from(n)?),
        n if n < 1 << 16 => {
            f.push(0x80 | 126);
            f.extend(u16::try_from(n)?.to_be_bytes());
        }
        n => {
            f.push(0x80 | 127);
            f.extend(u64::try_from(n)?.to_be_bytes());
        }
    }
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    f.extend(mask);
    f.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
    s.write_all(&f)?;
    Ok(())
}

/// The milliseconds since the test began: the client's clock.
fn ms(t0: Instant) -> u64 {
    u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// A page's protocol over a socket: its client, and what it has heard.
struct Page {
    s: TcpStream,
    client: Client,
    events: Vec<ClientEvent>,
    t0: Instant,
}

impl Page {
    fn flush(&mut self) -> anyhow::Result<()> {
        while let Some(t) = self.client.poll_transmit(64 << 10, ms(self.t0)) {
            send_binary(&mut self.s, &t.bytes)?;
        }
        Ok(())
    }

    /// Reads the server's messages until `done` holds of what was heard, within [`WITHIN`].
    fn until(&mut self, what: &str, done: impl Fn(&[ClientEvent]) -> bool) -> anyhow::Result<()> {
        let deadline = Instant::now() + WITHIN;
        self.flush()?;
        while !done(&self.events) {
            anyhow::ensure!(Instant::now() < deadline, "{what}: not within {WITHIN:?}; heard {:?}", self.events);
            let (op, payload) = agents::frame(&mut self.s)?;
            match op {
                2 => self.client.receive(&payload, ms(self.t0)),
                8 => anyhow::bail!("{what}: the server closed the socket; heard {:?}", self.events),
                _ => {}
            }
            while let Some(e) = self.client.poll_event() {
                self.events.push(e);
            }
            self.flush()?;
        }
        Ok(())
    }
}

/// A ticket for the admin, as a page fetches one.
fn ticket(port: u16, admin: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;
    let (code, body) = http(port, "POST", "/api/live/ticket", admin, None)?;
    anyhow::ensure!(code == 200, "ticket: {code} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let t = v.get("ticket").and_then(Value::as_str).unwrap_or_default();
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(t)?)
}

/// bp-g3 step 2: the handshake, case by case.
#[test]
fn bp_g3_s2_the_handshake_checks_credentials_origin_and_version() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = common::start_env(dir.path(), &Options::default(), false, &[("KILN_LIVE", "1")])?;
    let (port, admin) = (server.port, server.admin.clone());
    let auth = format!("Authorization: Bearer {admin}\r\n");
    let ours = format!("Origin: http://127.0.0.1:{port}\r\n");
    let v1 = format!("Sec-WebSocket-Protocol: kiln-bp.{}\r\n", kiln_bp::VERSION);

    let (head, _) = upgrade(port, &format!("{ours}{v1}"))?;
    assert_eq!(status(&head), 401, "no credentials: {head}");
    let (head, _) = upgrade(port, &format!("{auth}Origin: https://evil.example\r\n{v1}"))?;
    assert_eq!(status(&head), 403, "another site's page: {head}");
    let (head, _) = upgrade(port, &format!("{auth}{ours}Sec-WebSocket-Protocol: kiln-bp.999, chat\r\n"))?;
    assert_eq!(status(&head), 400, "no version this server speaks: {head}");
    assert!(header(&head, "sec-websocket-protocol").is_some_and(|v| v.contains(&format!("kiln-bp.{}", kiln_bp::VERSION))), "the versions offered: {head}");
    let (head, _) = upgrade(port, &format!("{auth}{ours}{v1}Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n"))?;
    assert_eq!(status(&head), 101, "upgraded: {head}");
    assert_eq!(header(&head, "sec-websocket-protocol"), Some(format!("kiln-bp.{}", kiln_bp::VERSION).as_str()), "the version taken: {head}");
    assert!(header(&head, "sec-websocket-extensions").is_none(), "deflate never echoed: {head}");
    // No Origin: a client that isn't a browser.
    let (head, _) = upgrade(port, &format!("{auth}{v1}"))?;
    assert_eq!(status(&head), 101, "no origin: {head}");

    // The server says it serves the endpoint.
    let (code, body) = http(port, "GET", "/api/me", &admin, None)?;
    assert_eq!(code, 200);
    let me: Value = serde_json::from_str(&body)?;
    assert_eq!(me.pointer("/live/path").and_then(Value::as_str), Some("/api/live"), "{body}");
    Ok(())
}

/// bp-g3 step 2: the protocol through it, beside today's events socket.
#[test]
fn bp_g3_s2_a_subscription_a_request_and_a_malformed_frame() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = common::start_env(dir.path(), &Options::default(), false, &[("KILN_LIVE", "1")])?;
    let (port, admin) = (server.port, server.admin.clone());
    let t = ticket(port, &admin)?;
    let (head, s) = upgrade(port, &format!("Authorization: Bearer {admin}\r\nSec-WebSocket-Protocol: kiln-bp.{}\r\n", kiln_bp::VERSION))?;
    anyhow::ensure!(status(&head) == 101, "{head}");
    let t0 = Instant::now();
    let mut page = Page { s, client: Client::new(ClientConfig::default(), &t, 0), events: Vec::new(), t0 };
    page.until("opened", |e| e.iter().any(|e| matches!(e, ClientEvent::Opened { .. })))?;

    // The queue's counts: a snapshot (nothing yet), then the change the hub makes when an events socket
    // opens beside it (the events socket's own frames are as they were: JSON text).
    let queue = Topic { kind: kiln::bp::kind::QUEUE, key: String::new() };
    page.client.subscribe(1, queue, 0);
    page.until("ready", |e| e.iter().any(|e| matches!(e, ClientEvent::Ready { sub: 1, .. })))?;
    let mut events = agents::socket(port, &admin, "/api/events")?;
    page.until("the queue's change", |e| e.iter().any(|e| matches!(e, ClientEvent::Change { sub: 1, .. })))?;
    assert!(page.client.state(1).is_some_and(|s| !s.record.is_empty()), "the record came");
    let mut kinds = Vec::new();
    while kinds.len() < 50 && kinds.last().map(String::as_str) != Some("queue") {
        let (op, text) = agents::frame(&mut events)?;
        assert_eq!(op, 1, "today's events socket sends text");
        let ev: Value = serde_json::from_slice(&text)?;
        kinds.push(ev.get("type").and_then(Value::as_str).unwrap_or_default().to_owned());
    }
    assert_eq!(kinds.last().map(String::as_str), Some("queue"), "today's events socket heard the queue as before: {kinds:?}");

    // A request, answered by the API's own route as the caller (its authenticated routes: the open ones,
    // `/api/me` and sign-in, stay HTTP's).
    page.client.request(7, Call::Http { method: Method::Get, path: "/api/system".into(), body: kiln_bp::Bytes::default() });
    page.until("an answer", |e| e.iter().any(|e| matches!(e, ClientEvent::Answer { id: 7, .. })))?;
    let reply = page.events.iter().find_map(|e| match e {
        ClientEvent::Answer { id: 7, outcome: Outcome::Reply { status, body }, .. } => Some((*status, body.0.clone())),
        _ => None,
    });
    let (code, body) = reply.ok_or_else(|| anyhow::anyhow!("a reply: {:?}", page.events))?;
    assert_eq!(code, 200);
    let system: Value = serde_json::from_slice(&body)?;
    assert_eq!(system.pointer("/live/path").and_then(Value::as_str), Some("/api/live"), "the server's own answer");

    // A malformed frame: answered typed, and the socket stays open for the next request.
    send_binary(&mut page.s, &[0xff; 12])?;
    page.until("the refusal", |e| e.iter().any(|e| matches!(e, ClientEvent::Unread { .. })))?;
    page.client.request(8, Call::Http { method: Method::Get, path: "/api/overview".into(), body: kiln_bp::Bytes::default() });
    page.until("an answer after it", |e| e.iter().any(|e| matches!(e, ClientEvent::Answer { id: 8, outcome: Outcome::Reply { status: 200, .. }, .. })))?;
    let (code, body) = http(port, "GET", "/api/live/sockets", &admin, None)?;
    assert_eq!(code, 200);
    let sockets: Option<Value> = serde_json::from_str(&body).ok();
    let held = sockets.as_ref().and_then(|v| v.get("sockets")).and_then(Value::as_array).map_or(0, Vec::len);
    assert_eq!(held, 1, "one socket held: {sockets:?}");
    // A write over the protocol is goal 9's: refused, not run.
    page.client.request(9, Call::Http { method: Method::Post, path: "/api/live/ticket".into(), body: kiln_bp::Bytes::default() });
    page.until("a write refused", |e| e.iter().any(|e| matches!(e, ClientEvent::Answer { id: 9, outcome: Outcome::Nack { .. }, .. })))?;
    let (code, metrics) = http(port, "GET", "/metrics", &admin, None)?;
    assert_eq!(code, 200);
    assert!(metrics.contains("kiln_proto_sockets 1"), "the socket on /metrics");
    assert!(metrics.lines().any(|l| l.starts_with("kiln_proto_mark_bytes{socket=")), "its mark on /metrics");
    Ok(())
}

/// bp-g3 step 2: seeded junk in the upgrade's headers and the first frames; each is answered, the
/// server stays up, and nothing in its log panicked.
#[test]
fn bp_g3_s2_fuzzed_upgrades_and_first_frames_never_panic() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = common::start_env(dir.path(), &Options::default(), false, &[("KILN_LIVE", "1")])?;
    let (port, admin) = (server.port, server.admin.clone());
    let mut g = kiln_bp::rng::Rng::new(0xF022);
    let junk = |g: &mut kiln_bp::rng::Rng, n: usize| -> String { (0..n).map(|_| char::from(b' ' + u8::try_from(g.below(95)).unwrap_or(0))).collect() };
    for i in 0..120u32 {
        let protocol = match i % 4 {
            0 => format!("kiln-bp.{}", kiln_bp::VERSION),
            1 => junk(&mut g, 20),
            2 => format!("kiln-bp.{}", g.next_u64()),
            _ => String::new(),
        };
        let origin = match i % 3 {
            0 => String::new(),
            1 => format!("Origin: {}\r\n", junk(&mut g, 30)),
            _ => format!("Origin: http://127.0.0.1:{port}\r\n"),
        };
        let headers = format!("Authorization: Bearer {admin}\r\n{origin}Sec-WebSocket-Protocol: {protocol}\r\nX-Junk: {}\r\n", junk(&mut g, 40));
        let Ok((head, mut s)) = upgrade(port, &headers) else { continue };
        let code = status(&head);
        assert!(matches!(code, 101 | 400 | 403), "case {i}: {head}");
        if code == 101 {
            let n = 1 + g.index(300);
            let bytes: Vec<u8> = (0..n).map(|_| g.byte()).collect();
            let _ = send_binary(&mut s, &bytes);
            // An answer (a refusal of the open) or a close; either way, no panic.
            let _ = agents::frame(&mut s);
        }
    }
    let (code, _) = http(port, "GET", "/healthz", "", None)?;
    assert_eq!(code, 200, "the server is up");
    let log = server.log();
    assert!(!log.contains("panicked"), "the server panicked:\n{log}");
    Ok(())
}

/// bp-g3 step 2: a person's session cookie, as a browser sends it, opens the protocol from the server's
/// own page and not from another site's.
#[test]
fn bp_g3_s2_a_session_cookie_opens_it_from_the_servers_own_origin() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = common::start_env(dir.path(), &Options::default(), false, &[("KILN_LIVE", "1")])?;
    let (port, admin) = (server.port, server.admin.clone());
    let cookie = common::custody::person(port, &admin, "dev")?;
    let v1 = format!("Sec-WebSocket-Protocol: kiln-bp.{}\r\n", kiln_bp::VERSION);
    let (head, _) = upgrade(port, &format!("Cookie: kiln_session={cookie}\r\nOrigin: http://127.0.0.1:{port}\r\n{v1}Sec-WebSocket-Extensions: permessage-deflate\r\n"))?;
    assert_eq!(status(&head), 101, "the right cookie, Origin and version: {head}");
    assert_eq!(header(&head, "sec-websocket-protocol"), Some(format!("kiln-bp.{}", kiln_bp::VERSION).as_str()), "{head}");
    assert!(header(&head, "sec-websocket-extensions").is_none(), "deflate never echoed: {head}");
    let (head, _) = upgrade(port, &format!("Cookie: kiln_session={cookie}\r\nOrigin: https://evil.example\r\n{v1}"))?;
    assert_eq!(status(&head), 403, "the same cookie from another site's page: {head}");
    let (head, _) = upgrade(port, &format!("Cookie: kiln_session=not-a-session\r\nOrigin: http://127.0.0.1:{port}\r\n{v1}"))?;
    assert_eq!(status(&head), 401, "a cookie that isn't a session: {head}");
    Ok(())
}

/// bp-g3 step 2: a server without `[server] live` has no endpoint, and says so in the field an older
/// page ignores (optional and additive: `"live": null`).
#[test]
fn bp_g3_s2_a_server_without_it_says_so_and_has_no_endpoint() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = common::start(dir.path(), &Options::default())?;
    let (port, admin) = (server.port, server.admin.clone());
    for path in ["/api/me", "/api/system"] {
        let (code, body) = http(port, "GET", path, &admin, None)?;
        assert_eq!(code, 200, "{path}: {body}");
        let v: Value = serde_json::from_str(&body)?;
        assert!(v.get("live").is_some_and(Value::is_null), "{path} says it has no endpoint: {body}");
    }
    let (head, _) = upgrade(port, &format!("Authorization: Bearer {admin}\r\nSec-WebSocket-Protocol: kiln-bp.{}\r\n", kiln_bp::VERSION))?;
    assert_eq!(status(&head), 404, "no endpoint: {head}");
    Ok(())
}
