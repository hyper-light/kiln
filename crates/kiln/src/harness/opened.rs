//! An agent's app opened on a person's own device (docs/previews.md, "On your device"): its dev
//! server, in the device's own browser, through kiln. It's served from the site pages have of their
//! own (`[server] pages_url`), never kiln's: there it would be one of kiln's own pages, with the
//! person's kiln session.
//!
//! * **Opening.** A person asks for one (`POST /api/harness/sessions/:id/opened`) and gets a link
//!   at pages' site with a code good once, for ten minutes. Followed, the code becomes a cookie of
//!   that site ([`COOKIE`], HttpOnly), and the device goes on to the app's page.
//! * **Forwarding.** Every request there with the cookie (pages' own `/v/` aside) goes to the app's
//!   port on the agent's machine, over a connection through its runner ([`Tunnels`],
//!   `crate::browser::ports`), with what it carried but the cookie. `Host`, `Origin` and `Referer`
//!   say the app's own `localhost` address, so a dev server's checks hold. Bodies stream both ways;
//!   an upgrade (a dev server's hot reload) is asked of the app first and the two connections are
//!   then piped. The app's redirects to its own address stay on pages' site, and its cookies are
//!   that site's alone. A service worker isn't let in: it would outlive the opening, on a site
//!   pages share.
//! * **Ending.** Twelve hours on, when its person ends it, or when its conversation closes.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use http_body_util::BodyExt as _;
use kiln_http::client::http1::SendRequest;
use parking_lot::Mutex;
use serde::Serialize;

use crate::browser::ports::{CHUNK, WINDOW};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::proto::{CAP_PORTS, LANE_PORTS, ServerMsg};

/// The cookie naming a device's opening, at pages' site.
pub const COOKIE: &str = "kiln_open";
/// How long a link's code is good for (once).
const CODE_FOR: i64 = 10 * 60 * 1000;
/// How long an opening lasts.
const OPEN_FOR: i64 = 12 * 60 * 60 * 1000;
/// Connections to an app kept for its next requests.
const IDLE: usize = 6;
/// Headers about one connection, never forwarded either way.
const HOP: [&str; 8] = ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"];
/// Kiln's side's addresses and the opening's cookie: never forwarded (the app's own take their
/// place).
const LOCAL: [&str; 9] = ["host", "origin", "referer", "cookie", "forwarded", "x-forwarded-for", "x-forwarded-host", "x-forwarded-proto", "x-real-ip"];

// ------------------------------------------------------------------------------ connections

/// The server's connections to ports of agents' machines (their runners' side:
/// `crate::browser::ports`).
#[derive(Default)]
pub struct Tunnels {
    next: AtomicU64,
    conns: Mutex<HashMap<u64, Far>>,
}

/// A connection's far end, as the server knows it.
struct Far {
    machine: String,
    /// What its port sent, for its [`Conn`] to read (closed: it ended).
    into: futures::channel::mpsc::UnboundedSender<Bytes>,
    /// How much of what was sent its port its runner took.
    acked: futures::channel::mpsc::UnboundedSender<u64>,
    /// Waiting for it to reach its port.
    opened: Option<futures::channel::oneshot::Sender<Result<(), String>>>,
}

impl Tunnels {
    /// A connection to port `port` of session `session`'s machine `machine`, once it's there.
    pub async fn open(&self, hub: &Arc<Hub>, machine: &str, session: &str, port: u16) -> anyhow::Result<Conn> {
        let conn = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let msg = ServerMsg::PortOpen { conn, session: session.to_owned(), port };
        anyhow::ensure!(hub.routed_caps(machine, &msg).await & CAP_PORTS != 0, "{machine} runs a kiln too old to open its ports: upgrade its runner");
        let (into, from) = futures::channel::mpsc::unbounded();
        let (acked, mut acks) = futures::channel::mpsc::unbounded();
        let (tx, rx) = futures::channel::oneshot::channel();
        self.conns.lock().insert(conn, Far { machine: machine.to_owned(), into, acked, opened: Some(tx) });
        if !hub.send_to_agent(machine, LANE_PORTS, &msg).await {
            self.conns.lock().remove(&conn);
            anyhow::bail!("{machine} isn't connected");
        }
        match crate::rt::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(why))) => {
                self.conns.lock().remove(&conn);
                anyhow::bail!("{why}");
            }
            _ => {
                self.close(hub, conn, session).await;
                anyhow::bail!("{machine} didn't reach port {port} in time");
            }
        }
        // What's written goes to the port in order, within its window; the connection ends when
        // its writer does.
        let (out, mut written) = futures::channel::mpsc::channel::<Bytes>(8);
        let (h, m, s) = (hub.clone(), machine.to_owned(), session.to_owned());
        crate::rt::spawn(async move {
            let mut unacked = 0u64;
            'writing: while let Some(b) = written.next().await {
                for piece in b.chunks(CHUNK) {
                    while let Ok(n) = acks.try_recv() {
                        unacked = unacked.saturating_sub(n);
                    }
                    while unacked >= WINDOW {
                        match acks.next().await {
                            Some(n) => unacked = unacked.saturating_sub(n),
                            None => break 'writing,
                        }
                    }
                    unacked += u64::try_from(piece.len()).unwrap_or(u64::MAX);
                    if !h.send_to_agent(&m, LANE_PORTS, &ServerMsg::PortData { conn, session: s.clone(), data: piece.to_vec() }).await {
                        break 'writing;
                    }
                }
            }
            h.harness.ports.close(&h, conn, &s).await;
        });
        // What's read here is acknowledged to the runner as it's taken (apart from the writing, so
        // neither direction's window waits on the other's).
        let (taken, mut took) = futures::channel::mpsc::unbounded::<u64>();
        let (h, m, s) = (hub.clone(), machine.to_owned(), session.to_owned());
        crate::rt::spawn(async move {
            while let Some(bytes) = took.next().await {
                if !h.send_to_agent(&m, LANE_PORTS, &ServerMsg::PortAck { conn, session: s.clone(), bytes }).await {
                    break;
                }
            }
        });
        Ok(Conn { from, pending: Bytes::new(), out, taken })
    }

    /// Its runner reached the port (or didn't: `error`).
    pub fn opened(&self, machine: &str, conn: u64, error: Option<String>) {
        if let Some(tx) = self.conns.lock().get_mut(&conn).filter(|f| f.machine == machine).and_then(|f| f.opened.take()) {
            let _ = tx.send(error.map_or(Ok(()), Err));
        }
    }

    /// Bytes from its port.
    pub fn data(&self, machine: &str, conn: u64, data: Vec<u8>) {
        if let Some(f) = self.conns.lock().get(&conn).filter(|f| f.machine == machine) {
            let _ = f.into.unbounded_send(Bytes::from(data));
        }
    }

    /// Its runner took `bytes` more of what was sent its port.
    pub fn ack(&self, machine: &str, conn: u64, bytes: u64) {
        if let Some(f) = self.conns.lock().get(&conn).filter(|f| f.machine == machine) {
            let _ = f.acked.unbounded_send(bytes);
        }
    }

    /// Its port ended it (its reader sees the end, after what came before).
    pub fn ended(&self, machine: &str, conn: u64, error: Option<String>) {
        let mut g = self.conns.lock();
        if g.get(&conn).is_some_and(|f| f.machine == machine)
            && let Some(mut f) = g.remove(&conn)
            && let Some(tx) = f.opened.take()
        {
            let _ = tx.send(Err(error.unwrap_or_else(|| "the port closed the connection".to_owned())));
        }
    }

    /// Done with it here: its runner is told.
    pub async fn close(&self, hub: &Hub, conn: u64, session: &str) {
        let far = self.conns.lock().remove(&conn);
        if let Some(f) = far {
            let _ = hub.send_to_agent(&f.machine, LANE_PORTS, &ServerMsg::PortClose { conn, session: session.to_owned() }).await;
        }
    }

    /// Its machine went away: its connections end.
    pub fn machine_lost(&self, machine: &str) {
        self.conns.lock().retain(|_, f| f.machine != machine);
    }
}

/// A connection to a port, as kiln-http's client speaks over it.
pub struct Conn {
    from: futures::channel::mpsc::UnboundedReceiver<Bytes>,
    pending: Bytes,
    out: futures::channel::mpsc::Sender<Bytes>,
    taken: futures::channel::mpsc::UnboundedSender<u64>,
}

impl futures::io::AsyncRead for Conn {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.pending.is_empty() {
            match this.from.poll_next_unpin(cx) {
                Poll::Ready(Some(b)) => this.pending = b,
                // Its port ended it.
                Poll::Ready(None) => return Poll::Ready(Ok(0)),
                Poll::Pending => return Poll::Pending,
            }
        }
        let n = this.pending.len().min(buf.len());
        let piece = this.pending.split_to(n);
        if let Some(to) = buf.get_mut(..n) {
            to.copy_from_slice(&piece);
        }
        let _ = this.taken.unbounded_send(u64::try_from(n).unwrap_or(u64::MAX));
        Poll::Ready(Ok(n))
    }
}

impl futures::io::AsyncWrite for Conn {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.out.poll_ready(cx) {
            Poll::Ready(Ok(())) => match this.out.start_send(Bytes::copy_from_slice(buf)) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            },
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().out.close_channel();
        Poll::Ready(Ok(()))
    }
}

// ------------------------------------------------------------------------------ openings

/// One app opened on a person's devices.
#[derive(Debug, Clone, Serialize)]
pub struct Opening {
    /// What it's named by in kiln's API (its key is its token, never shown).
    pub id: String,
    pub session: String,
    pub port: u16,
    /// The page it opens at.
    pub path: String,
    /// Who opened it, as the audit names them.
    pub by: String,
    pub at: i64,
    pub until: i64,
    /// The person it's for (`Caller::opener`): theirs alone.
    #[serde(skip)]
    person: Option<String>,
    /// Connections to the app, kept for its next requests.
    #[serde(skip)]
    idle: Arc<Mutex<Vec<SendRequest<srv::Body>>>>,
}

/// Apps opened on people's devices, by their tokens; links' codes not yet followed.
#[derive(Default)]
pub struct Openings {
    open: Mutex<HashMap<String, Opening>>,
    codes: Mutex<HashMap<String, (String, i64)>>,
}

impl Openings {
    /// Open port `port` of session `session` for `person` (its page `path`): it, and its link's code.
    pub fn create(&self, session: &str, port: u16, path: &str, by: &str, person: Option<String>) -> (Opening, String) {
        let now = crate::util::now_ms();
        let token = hex::encode(rand::random::<[u8; 32]>());
        let code = hex::encode(rand::random::<[u8; 16]>());
        let o = Opening {
            id: hex::encode(rand::random::<[u8; 16]>()),
            session: session.to_owned(),
            port,
            path: if path.starts_with('/') { path.to_owned() } else { format!("/{path}") },
            by: by.to_owned(),
            at: now,
            until: now + OPEN_FOR,
            person,
            idle: Arc::default(),
        };
        self.open.lock().retain(|_, o| o.until > now);
        self.open.lock().insert(token.clone(), o.clone());
        let mut codes = self.codes.lock();
        codes.retain(|_, (_, until)| *until > now);
        codes.insert(code.clone(), (token, now + CODE_FOR));
        (o, code)
    }

    /// A link's code, followed: its opening's token (once).
    fn redeem(&self, code: &str) -> Option<(String, Opening)> {
        let (token, until) = self.codes.lock().remove(code)?;
        (until > crate::util::now_ms()).then_some(())?;
        let o = self.get(&token)?;
        Some((token, o))
    }

    fn get(&self, token: &str) -> Option<Opening> {
        self.open.lock().get(token).filter(|o| o.until > crate::util::now_ms()).cloned()
    }

    /// Session `session`'s openings for `person`.
    pub fn of(&self, session: &str, person: Option<&str>) -> Vec<Opening> {
        let now = crate::util::now_ms();
        let mut v: Vec<Opening> = self.open.lock().values().filter(|o| o.session == session && o.until > now && o.person.as_deref() == person).cloned().collect();
        v.sort_by_key(|o| std::cmp::Reverse(o.at));
        v
    }

    /// End opening `id` of session `session`, `person`'s: whether it was theirs.
    pub fn end(&self, session: &str, id: &str, person: Option<&str>) -> bool {
        let mut g = self.open.lock();
        let before = g.len();
        g.retain(|_, o| !(o.id == id && o.session == session && o.person.as_deref() == person));
        g.len() < before
    }

    /// Its conversation closed: its openings end.
    pub fn forget(&self, session: &str) {
        self.open.lock().retain(|_, o| o.session != session);
    }
}

// ------------------------------------------------------------------------------ at pages' site

/// `GET /o/:code` at pages' site: the link's code becomes this device's cookie for its opening,
/// and the device goes on to the app's page.
pub fn enter(hub: &Hub, code: &str) -> Response {
    let Some((token, o)) = hub.harness.opened.redeem(code) else {
        return said(StatusCode::GONE, "This link was used already, or it's too old", "Open the app on your device from kiln again: a link works once, for ten minutes.");
    };
    let secure = hub.cfg.server.pages_url.as_deref().is_some_and(|u| u.trim().starts_with("https://"));
    let max_age = ((o.until - crate::util::now_ms()) / 1000).max(0);
    let mut r = srv::status(StatusCode::FOUND);
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&format!("{COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{}", if secure { "; Secure" } else { "" })) {
        h.insert(header::SET_COOKIE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&o.path) {
        h.insert(header::LOCATION, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    r
}

/// What a request at pages' site holds of an opening (its cookie).
pub enum Holds {
    Nothing,
    Open(Opening),
    /// One that ended.
    Ended,
}

/// The opening a request at pages' site is for, by its cookie.
pub fn of_request(hub: &Hub, req: &Request) -> Holds {
    match cookie(req.headers(), COOKIE) {
        None => Holds::Nothing,
        Some(token) => hub.harness.opened.get(&token).map_or(Holds::Ended, Holds::Open),
    }
}

/// What a device sees for an opening that ended.
pub fn ended() -> Response {
    said(StatusCode::GONE, "This app isn't open here any more", "Its opening ended (after twelve hours, from kiln, or with its conversation). Open it on your device from kiln again.")
}

/// A request at pages' site for opening `o`: to its app, and the app's answer back.
pub async fn forward(hub: &Arc<Hub>, o: &Opening, req: Request) -> Response {
    // A service worker would outlive the opening, on a site pages share.
    if req.headers().contains_key("service-worker") {
        return srv::text(StatusCode::FORBIDDEN, "kiln doesn't let an app opened through it install a service worker");
    }
    let machine = match hub.harness.location(&o.session) {
        Some(super::registry::Location::Machine(m)) => m,
        _ => return said(StatusCode::BAD_GATEWAY, "Its conversation isn't on a machine kiln reaches", "Open the app on your device from its conversation again."),
    };
    let site = hub.cfg.server.pages_url.as_deref().unwrap_or("").trim().trim_end_matches('/').to_owned();
    let result = if upgrading(req.headers()) { forward_upgrade(hub, o, &machine, &site, req).await } else { forward_http(hub, o, &machine, &site, req).await };
    result.unwrap_or_else(|e| {
        tracing::debug!(session = %o.session, port = o.port, error = %format!("{e:#}"), "an opened app didn't answer");
        said(StatusCode::BAD_GATEWAY, &format!("localhost:{} on {machine} didn't answer", o.port), &format!("{e:#}. Is its server still running?"))
    })
}

fn upgrading(h: &HeaderMap) -> bool {
    h.contains_key(header::UPGRADE) && h.get_all(header::CONNECTION).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
}

/// A connection to the app to send a request on: one kept and ready, else a new one.
async fn sender(hub: &Arc<Hub>, o: &Opening, machine: &str) -> anyhow::Result<SendRequest<srv::Body>> {
    {
        let mut idle = o.idle.lock();
        idle.retain(|s| !s.is_closed());
        if let Some(i) = idle.iter().position(SendRequest::is_ready) {
            return Ok(idle.swap_remove(i));
        }
    }
    let conn = hub.harness.ports.open(hub, machine, &o.session, o.port).await?;
    let (sender, connection) = kiln_http::client::http1::handshake::<_, srv::Body>(conn).await?;
    crate::rt::spawn_local(async move {
        let _ = connection.with_upgrades().await;
    })
    .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
    Ok(sender)
}

async fn forward_http(hub: &Arc<Hub>, o: &Opening, machine: &str, site: &str, req: Request) -> anyhow::Result<Response> {
    let (parts, body) = req.into_parts();
    let mut up = http::Request::builder().method(parts.method).uri(target(&parts.uri)).body(body.map_err(io::Error::other).boxed())?;
    *up.headers_mut() = upstream_headers(&parts.headers, o.port, site);
    let mut s = sender(hub, o, machine).await?;
    let resp = s.send_request(up).await?;
    // Kept for the next request once this one's answer is read.
    let mut idle = o.idle.lock();
    if idle.len() < IDLE {
        idle.push(s);
    }
    drop(idle);
    let (mut head, body) = resp.into_parts();
    downstream_headers(&mut head.headers, o.port, site);
    Ok(http::Response::from_parts(head, body.map_err(io::Error::other).boxed()))
}

/// An upgrade (a WebSocket): asked of the app first, its answer the device's; then both
/// connections piped until either ends.
async fn forward_upgrade(hub: &Arc<Hub>, o: &Opening, machine: &str, site: &str, mut req: Request) -> anyhow::Result<Response> {
    let device = kiln_http::upgrade::on(&mut req);
    let (parts, _) = req.into_parts();
    let mut up = http::Request::builder().method(Method::GET).uri(target(&parts.uri)).body(srv::empty())?;
    let mut headers = upstream_headers(&parts.headers, o.port, site);
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Some(v) = parts.headers.get(header::UPGRADE) {
        headers.insert(header::UPGRADE, v.clone());
    }
    *up.headers_mut() = headers;
    let conn = hub.harness.ports.open(hub, machine, &o.session, o.port).await?;
    let (mut sender, connection) = kiln_http::client::http1::handshake::<_, srv::Body>(conn).await?;
    crate::rt::spawn_local(async move {
        let _ = connection.with_upgrades().await;
    })
    .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
    let mut resp = sender.send_request(up).await?;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        let (mut head, body) = resp.into_parts();
        downstream_headers(&mut head.headers, o.port, site);
        return Ok(http::Response::from_parts(head, body.map_err(io::Error::other).boxed()));
    }
    let app = kiln_http::upgrade::on(&mut resp);
    let mut out = http::Response::new(srv::empty());
    *out.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    for k in [header::UPGRADE, header::CONNECTION, header::SEC_WEBSOCKET_ACCEPT, header::SEC_WEBSOCKET_PROTOCOL, header::SEC_WEBSOCKET_EXTENSIONS] {
        if let Some(v) = resp.headers().get(&k) {
            out.headers_mut().insert(k, v.clone());
        }
    }
    let port = o.port;
    crate::rt::spawn_local(async move {
        match (device.await, app.await) {
            (Ok(d), Ok(a)) => {
                if let Err(e) = crate::frontdoor::proxy::pipe(d, a).await {
                    tracing::debug!(port, error = %e, "an opened app's upgraded connection ended");
                }
            }
            (d, a) => tracing::debug!(port, device = d.is_ok(), app = a.is_ok(), "an opened app's upgrade didn't complete"),
        }
    })
    .map_err(|e| anyhow::anyhow!("spawn upgrade pipe: {e}"))?;
    Ok(out)
}

fn target(uri: &http::Uri) -> String {
    uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str).to_owned()
}

/// `name`'s value among the request's cookies.
fn cookie(h: &HeaderMap, name: &str) -> Option<String> {
    h.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| c.trim().strip_prefix(name)?.strip_prefix('=').map(str::to_owned))
}

/// The headers a request goes to the app with: what it carried, but the opening's cookie and
/// kiln's side's addresses, which become the app's own (`localhost:<port>`). Where it was asked
/// (`site`) goes as `X-Forwarded-*`, for an app making absolute addresses.
fn upstream_headers(incoming: &HeaderMap, port: u16, site: &str) -> HeaderMap {
    let own = format!("http://localhost:{port}");
    let mut h = HeaderMap::new();
    for (k, v) in incoming {
        if !HOP.contains(&k.as_str()) && !LOCAL.contains(&k.as_str()) {
            h.append(k, v.clone());
        }
    }
    let mut set = |k: header::HeaderName, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(k, v);
        }
    };
    set(header::HOST, &format!("localhost:{port}"));
    if incoming.contains_key(header::ORIGIN) {
        set(header::ORIGIN, &own);
    }
    if let Some(path) = incoming.get(header::REFERER).and_then(|v| v.to_str().ok()).and_then(|r| r.parse::<http::Uri>().ok()).and_then(|u| u.path_and_query().map(|p| p.as_str().to_owned())) {
        set(header::REFERER, &format!("{own}{path}"));
    }
    let cookies: Vec<&str> = incoming.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).map(str::trim).filter(|c| !c.is_empty() && c.split('=').next() != Some(COOKIE)).collect();
    if !cookies.is_empty() {
        set(header::COOKIE, &cookies.join("; "));
    }
    if let Ok(u) = site.parse::<http::Uri>() {
        if let Some(a) = u.authority() {
            set(header::HeaderName::from_static("x-forwarded-host"), a.as_str());
        }
        if let Some(scheme) = u.scheme_str() {
            set(header::HeaderName::from_static("x-forwarded-proto"), scheme);
        }
    }
    h
}

/// The headers the app's answer comes back with: its redirects to its own address go to pages'
/// site, its cookies are that site's alone (never the opening's own), and it's no one's to index.
fn downstream_headers(h: &mut HeaderMap, port: u16, site: &str) {
    for k in HOP {
        h.remove(k);
    }
    if let Some(l) = h.get(header::LOCATION).and_then(|v| v.to_str().ok()) {
        let own = [format!("http://localhost:{port}"), format!("http://127.0.0.1:{port}"), format!("http://[::1]:{port}")];
        if let Some(rest) = own.iter().find_map(|o| l.strip_prefix(o.as_str())) {
            let rest = if rest.starts_with('/') { rest.to_owned() } else { format!("/{rest}") };
            if let Ok(v) = HeaderValue::from_str(&format!("{site}{rest}")) {
                h.insert(header::LOCATION, v);
            }
        }
    }
    let cookies: Vec<HeaderValue> = h
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter(|c| c.split('=').next().map(str::trim) != Some(COOKIE))
        .filter_map(|c| HeaderValue::from_str(&c.split(';').map(str::trim).filter(|a| !a.to_ascii_lowercase().starts_with("domain=")).collect::<Vec<_>>().join("; ")).ok())
        .collect();
    h.remove(header::SET_COOKIE);
    for c in cookies {
        h.append(header::SET_COOKIE, c);
    }
    h.insert(header::HeaderName::from_static("x-robots-tag"), HeaderValue::from_static("noindex, nofollow"));
}

/// A short page for the device, in kiln's colors.
fn said(status: StatusCode, title: &str, why: &str) -> Response {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let page = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>{t}</title><body style=\"background:#0b0c0e;color:#d9dbe0;font:15px system-ui;padding:32px;line-height:1.5\"><p>{t}</p><p style=\"color:#8f929b\">{w}</p>",
        t = esc(title),
        w = esc(why)
    );
    let mut r = srv::response(status, "text/html; charset=utf-8", srv::full(page));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            if let (Ok(k), Ok(v)) = (header::HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                h.append(k, v);
            }
        }
        h
    }

    #[test]
    fn a_request_goes_to_the_app_as_its_own_without_the_openings_cookie() {
        let h = upstream_headers(
            &headers(&[
                ("host", "pages.example"),
                ("origin", "https://pages.example"),
                ("referer", "https://pages.example/src/main.ts?t=1"),
                ("cookie", "kiln_open=secret; theme=dark"),
                ("connection", "keep-alive"),
                ("accept", "text/html"),
                ("x-forwarded-for", "10.0.0.9"),
            ]),
            5173,
            "https://pages.example",
        );
        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        assert_eq!(get("host").as_deref(), Some("localhost:5173"));
        assert_eq!(get("origin").as_deref(), Some("http://localhost:5173"));
        assert_eq!(get("referer").as_deref(), Some("http://localhost:5173/src/main.ts?t=1"));
        assert_eq!(get("cookie").as_deref(), Some("theme=dark"), "the opening's cookie stays here");
        assert_eq!(get("accept").as_deref(), Some("text/html"));
        assert_eq!(get("x-forwarded-host").as_deref(), Some("pages.example"));
        assert_eq!(get("x-forwarded-proto").as_deref(), Some("https"));
        assert!(get("connection").is_none() && get("x-forwarded-for").is_none());
        // No cookie but the opening's: none at all.
        assert!(upstream_headers(&headers(&[("cookie", "kiln_open=secret")]), 5173, "https://pages.example").get("cookie").is_none());
    }

    #[test]
    fn an_answer_stays_on_pages_site() {
        let mut h = headers(&[
            ("location", "http://localhost:5173/login?next=%2F"),
            ("set-cookie", "sid=1; Domain=localhost; Path=/; HttpOnly"),
            ("set-cookie", "kiln_open=stolen; Path=/"),
            ("transfer-encoding", "chunked"),
            ("content-type", "text/html"),
        ]);
        downstream_headers(&mut h, 5173, "https://pages.example");
        assert_eq!(h.get("location").and_then(|v| v.to_str().ok()), Some("https://pages.example/login?next=%2F"));
        let cookies: Vec<&str> = h.get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).collect();
        assert_eq!(cookies, ["sid=1; Path=/; HttpOnly"], "its own cookies, pages' site's alone; never the opening's");
        assert!(h.get("transfer-encoding").is_none());
        assert_eq!(h.get("x-robots-tag").and_then(|v| v.to_str().ok()), Some("noindex, nofollow"));
        // Elsewhere is left be.
        let mut away = headers(&[("location", "https://accounts.example/oauth")]);
        downstream_headers(&mut away, 5173, "https://pages.example");
        assert_eq!(away.get("location").and_then(|v| v.to_str().ok()), Some("https://accounts.example/oauth"));
    }

    #[test]
    fn a_link_works_once_and_an_opening_is_its_persons() -> anyhow::Result<()> {
        let o = Openings::default();
        let (opening, code) = o.create("s1", 5173, "app", "user:ada", Some("user:ada".into()));
        assert_eq!(opening.path, "/app");
        let (token, got) = o.redeem(&code).ok_or_else(|| anyhow::anyhow!("the code didn't work"))?;
        assert_eq!((got.id.as_str(), got.port), (opening.id.as_str(), 5173));
        assert!(o.redeem(&code).is_none(), "once");
        assert!(o.get(&token).is_some());
        assert_eq!(o.of("s1", Some("user:ada")).len(), 1);
        assert!(o.of("s1", Some("user:bo")).is_empty(), "nobody else's to list");
        assert!(!o.end("s1", &opening.id, Some("user:bo")), "nor to end");
        assert!(o.end("s1", &opening.id, Some("user:ada")));
        assert!(o.get(&token).is_none(), "ended, its device's cookie opens nothing");
        let (_, code) = o.create("s2", 3000, "/", "owner", Some("owner".into()));
        let (token, _) = o.redeem(&code).ok_or_else(|| anyhow::anyhow!("the code didn't work"))?;
        o.forget("s2");
        assert!(o.get(&token).is_none(), "its conversation closed");
        Ok(())
    }

    #[test]
    fn upgrades_are_known_by_their_headers() {
        assert!(upgrading(&headers(&[("connection", "keep-alive, Upgrade"), ("upgrade", "websocket")])));
        assert!(!upgrading(&headers(&[("connection", "keep-alive")])));
        assert_eq!(cookie(&headers(&[("cookie", "a=1; kiln_open=t; b=2")]), COOKIE).as_deref(), Some("t"));
    }
}
