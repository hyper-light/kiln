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
//! - A reader that stops (step 3): the writer stalls with every layer below the scheduler within its
//!   bound, and an answer asked then overtakes everything the scheduler holds.
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
    upgrade_over(TcpStream::connect(("127.0.0.1", port))?, port, headers)
}

/// [`upgrade`] over a connection made already.
fn upgrade_over(mut s: TcpStream, port: u16, headers: &str) -> anyhow::Result<(String, TcpStream)> {
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
    /// The bytes of the server's messages read.
    got: u64,
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
                2 => {
                    self.got += u64::try_from(payload.len())?;
                    self.client.receive(&payload, ms(self.t0));
                }
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
    let mut page = Page { s, client: Client::new(ClientConfig::default(), &t, 0), events: Vec::new(), t0, got: 0 };
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

/// The server's numbers of its one protocol socket (`GET /api/live/sockets`).
fn the_socket(port: u16, admin: &str) -> anyhow::Result<Value> {
    let (code, body) = http(port, "GET", "/api/live/sockets", admin, None)?;
    anyhow::ensure!(code == 200, "sockets: {code} {body}");
    let v: Value = serde_json::from_str(&body)?;
    v.pointer("/sockets/0").cloned().ok_or_else(|| anyhow::anyhow!("no socket: {body}"))
}

fn n(v: &Value, field: &str) -> u64 {
    v.get(field).and_then(Value::as_u64).unwrap_or(0)
}

/// bp-g3 step 3, the loopback test of a reader that stops. Do: a page subscribed to the owner's
/// events topic, its socket's receive buffer small (4 KiB), stops reading; notices marked meanwhile
/// (45 KB each, carried in that topic: Stream frames) flood it, the window off. Expect: the writer
/// stalls at the kernel's mark (the bytes it has sent stop growing, a message waiting to be written)
/// while the scheduler's queue grows, the kernel's unsent bytes within the
/// mark and a chunk, and each layer below the scheduler (tungstenite, TLS's none here) within a
/// chunk and its frame's header. Then a request is made (its answer one control frame, ahead of any
/// subscription's) and, once its answer waits, the page reads again. Expect: the answer comes
/// before every byte the writer hadn't sent when it was asked, and the topic's changes go on after it.
#[test]
fn bp_g3_s3_a_reader_that_stops_gets_the_urgent_frame_first() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // The window off (`[server] browser_window = false`), so what stops the writer is the kernel's mark:
    // on loopback the window (two chunks of 16 KiB) fills before the mark (sized from loopback's rate) does.
    let server = common::start_env(dir.path(), &Options::default(), false, &[("KILN_LIVE", "1"), ("KILN_BROWSER_WINDOW", "0")])?;
    let (port, admin) = (server.port, server.admin.clone());
    let t = ticket(port, &admin)?;
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
    sock.set_recv_buffer_size(4 << 10)?;
    sock.connect(&std::net::SocketAddr::from(([127, 0, 0, 1], port)).into())?;
    let (head, s) = upgrade_over(sock.into(), port, &format!("Authorization: Bearer {admin}\r\nSec-WebSocket-Protocol: kiln-bp.{}\r\n", kiln_bp::VERSION))?;
    anyhow::ensure!(status(&head) == 101, "{head}");
    let t0 = Instant::now();
    // A credit far past what the flood sends, so what stops the writer is the kernel's mark, not goal
    // 2's credit (which the page grants as it reads, so it would stop it first).
    let cfg = ClientConfig { window: 1 << 30, ..ClientConfig::default() };
    let mut page = Page { s, client: Client::new(cfg, &t, 0), events: Vec::new(), t0, got: 0 };
    page.until("opened", |e| e.iter().any(|e| matches!(e, ClientEvent::Opened { .. })))?;
    page.client.subscribe(1, Topic { kind: kiln::bp::kind::EVENTS, key: String::new() }, 0);
    page.until("ready", |e| e.iter().any(|e| matches!(e, ClientEvent::Ready { sub: 1, .. })))?;

    // The page stops reading; the flood, of keys that don't compress (the topic's frames are
    // compressed: goal 2's by trust class).
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut key = |i: usize, j: usize| {
        let mut k = format!("{i:04}-{j:03}-");
        for _ in 0..288 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            k.push(char::from(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_".get(usize::try_from(seed % 64).unwrap_or(0)).copied().unwrap_or(b'x')));
        }
        k
    };
    let mut last_sent = 0u64;
    let mut stalled = None;
    for i in 0..60 {
        let keys: Vec<String> = (0..150).map(|j| key(i, j)).collect();
        let (code, body) = http(port, "POST", "/api/push/notices", &admin, Some(&serde_json::json!({ "keys": keys, "how": "seen" })))?;
        anyhow::ensure!(code == 200, "a notice: {code} {body}");
        if i >= 8 && i % 4 == 0 {
            std::thread::sleep(Duration::from_millis(400));
            let v = the_socket(port, &admin)?;
            let sent = n(&v, "sent");
            let queued: u64 = v.get("queued").and_then(Value::as_array).map_or(0, |q| q.iter().filter_map(Value::as_u64).sum());
            if sent.saturating_sub(last_sent) <= 256 && queued > 0 {
                stalled = Some(v);
                break;
            }
            last_sent = sent;
        }
    }
    let at_stall = stalled.ok_or_else(|| anyhow::anyhow!("the writer never stalled"))?;
    // More waits, in the scheduler alone.
    for i in 60..64 {
        let keys: Vec<String> = (0..150).map(|j| key(i, j)).collect();
        let (code, _) = http(port, "POST", "/api/push/notices", &admin, Some(&serde_json::json!({ "keys": keys, "how": "seen" })))?;
        anyhow::ensure!(code == 200);
    }
    std::thread::sleep(Duration::from_millis(400));
    let v = the_socket(port, &admin)?;
    let queued = |v: &Value| -> u64 { v.get("queued").and_then(Value::as_array).map_or(0, |q| q.iter().filter_map(Value::as_u64).sum()) };
    // Stalled: nothing more sent while the page doesn't read, but the window's bounded asks for an
    // acknowledgement (a probe's few bytes each, step 4) when it's the window that's full.
    assert!(n(&v, "sent").saturating_sub(n(&at_stall, "sent")) <= 256, "stalled: nothing more sent while the page doesn't read: {at_stall} then {v}");
    assert!(queued(&v) > queued(&at_stall), "the scheduler's queue grows: {at_stall} then {v}");
    // Against the largest mark and chunk the socket used: on loopback the first rate it measures is
    // the loopback's (a 16 KiB chunk, a mark of 128 KiB), then the stopped reader's (the kernel's
    // delivery rate falls to the page's), and what the kernel took under the larger mark it holds on.
    let (mark, chunk) = (n(&v, "mark_max"), n(&v, "chunk_max"));
    assert!(mark > 0 && chunk > 0, "{v}");
    assert!(n(&v, "unsent") <= mark + chunk, "the kernel's unsent bytes within the mark and a chunk: {v}");
    assert!(n(&v, "held_max") <= chunk && n(&v, "held_ws_max") <= chunk + 10, "tungstenite held a message at most: {v}");
    assert!(n(&v, "held_ws_max").max(n(&v, "held_max")) + n(&v, "held_tls_max") <= 2 * chunk, "user space below the scheduler within two chunks: {v}");
    // It stalls at the mark: a message waits to be written, the kernel taking no more.
    eprintln!("stalled: {v}");
    assert!(n(&v, "held") > 0, "the writer stalls at the mark, a message waiting to be written: {v}");

    // A request while stalled; its answer (one frame: a path the API hasn't, its 404, so no fragment of
    // it can come before the rest) waits on the control lane.
    page.client.request(7, Call::Http { method: Method::Get, path: "/api/bp-g3-s3-no-such-path".into(), body: kiln_bp::Bytes::default() });
    page.flush()?;
    let deadline = Instant::now() + WITHIN;
    while the_socket(port, &admin)?.pointer("/queued/0").and_then(Value::as_u64).unwrap_or(0) == 0 {
        anyhow::ensure!(Instant::now() < deadline, "the answer never waited: {}", the_socket(port, &admin)?);
        std::thread::sleep(Duration::from_millis(50));
    }
    let then = the_socket(port, &admin)?;
    anyhow::ensure!(n(&then, "sent") == n(&v, "sent"), "the writer was still stalled when the answer waited: {v} then {then}");
    let sent_then = n(&then, "sent");

    // The page reads again: what comes before the answer was all sent before it was asked.
    let mut before = None;
    let mut changes_after = 0usize;
    let deadline = Instant::now() + WITHIN;
    while changes_after == 0 {
        if Instant::now() >= deadline {
            anyhow::bail!("no change after the answer; read {} B, the answer after {before:?}; heard {:?}; the socket: {}", page.got, page.events.iter().filter(|e| !matches!(e, ClientEvent::Change { .. })).collect::<Vec<_>>(), the_socket(port, &admin)?);
        }
        let (op, payload) = agents::frame(&mut page.s)?;
        if op != 2 {
            continue;
        }
        let read = page.got;
        page.got += u64::try_from(payload.len())?;
        page.client.receive(&payload, ms(page.t0));
        while let Some(e) = page.client.poll_event() {
            match e {
                ClientEvent::Answer { id: 7, .. } if before.is_none() => before = Some(read),
                ClientEvent::Change { sub: 1, .. } if before.is_some() => changes_after += 1,
                ClientEvent::Change { .. } => {}
                other => page.events.push(other),
            }
        }
        page.flush()?;
    }
    let before = before.ok_or_else(|| anyhow::anyhow!("no answer"))?;
    assert!(before <= sent_then, "the answer came after {before} B, more than the {sent_then} B sent before it was asked");
    eprintln!("the answer after {before} B of {sent_then} sent before it was asked; {changes_after} change(s) after it");
    Ok(())
}
