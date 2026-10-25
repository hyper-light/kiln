//! Forwarding to a peered cluster (docs/peers.md), the same on the server and in every runner's
//! front door. Two ways in:
//!
//! * **At a front door's domain**, a peer's address (`peer-<slug>.<domain>`) is its UI, as if
//!   opened there ([`Forwarder::serve`]).
//! * **At this cluster's own address** (kiln reached by address, through its one port), a peer is
//!   under it, at `/.kiln-peer/<slug>/` ([`PATH`]): its API at `…/api/…` forwarded, its pages this
//!   cluster's app drawing it from there ([`Forwarder::serve_path`]). Nothing it answers runs as
//!   this cluster's page: its answers come sandboxed (`Content-Security-Policy: sandbox`).
//!
//! * **Entering.** A frame starts at [`ENTER`] (`/.kiln-peer/<slug>/enter` under this cluster's
//!   address), posted the grant the server signed for the person (`crate::peers::frame`). The grant
//!   becomes a cookie for that address (or that path) alone and the frame goes on to the page
//!   asked. Every other request needs that cookie, checked here with this cluster's public key:
//!   who has no grant for the peer gets nothing from it.
//! * **Forwarding.** Requests go to the peer's address with everything they carried except this
//!   cluster's cookies and credentials (and connection headers): the grant takes their place, as
//!   the peer's session cookie, and the peer checks it with the key it got in the challenge.
//!   `Host`, `Origin` and `Referer` say the peer's own address, so its UI server's checks hold.
//!   Bodies stream both ways; a WebSocket's upgrade is asked of the peer first and its answer is
//!   the browser's, then the two connections are piped. The peer's cookies are dropped (they'd
//!   land on this cluster's names), and a redirect to its own address stays in the frame.
//! * **Outbound.** Through the machine's proxy when its network wants one (`crate::http::proxy`);
//!   over HTTPS trusting the public roots and the peer's own CA, from its response code.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use http_body_util::BodyExt as _;
use serde::{Deserialize, Serialize};

use super::codes::Grant;
use crate::http::client::Roots;
use crate::http::io::{RtFuturesIo, RtIo};
use crate::http::server::{self as srv, Request, Response};
use crate::http::tls::{ClientTls, TlsStream};

/// Where a frame starts at a peer's address: the grant is posted here.
pub const ENTER: &str = "/.kiln-peer/enter";
/// Where peers are under this cluster's own address: `/.kiln-peer/<slug>/`.
pub const PATH: &str = "/.kiln-peer/";
/// What a peer's address starts with: `peer-<slug>.<domain>`.
pub const LABEL: &str = "peer-";
/// The largest form posted to [`ENTER`].
const ENTER_LIMIT: usize = 16 << 10;

/// Headers about one connection, never forwarded either way.
const HOP: [&str; 8] = ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"];
/// This side's credentials and addresses: never forwarded (the grant and the peer's address
/// take their place).
const LOCAL: [&str; 10] = ["cookie", "authorization", "host", "origin", "referer", "forwarded", "x-forwarded-for", "x-forwarded-host", "x-forwarded-proto", "x-real-ip"];

/// What a forwarder knows: this cluster's key (to check grants) and each peer it opens.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Table {
    /// This cluster's public key (base64url) and its id: grants are signed with it.
    pub key: String,
    pub kid: String,
    pub peers: Vec<Route>,
}

/// One peer, as a forwarder needs it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    /// Its name in addresses: `peer-<slug>.<domain>`.
    pub slug: String,
    pub name: String,
    /// Its key id: the audience the grants for it name.
    pub kid: String,
    /// Its address, as reached from here.
    pub url: String,
    /// Its CA (DER, base64url): trusted for its address, beside the public roots.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ca: String,
}

/// A peer ready to forward to: its address parsed, its TLS made once.
struct Live {
    route: Route,
    host: String,
    port: u16,
    https: bool,
    /// `scheme://authority`, as the peer names itself.
    origin: String,
    /// `authority`, for `Host`.
    authority: String,
    tls: Arc<ClientTls>,
}

/// A [`Table`] ready to forward with.
pub struct Forwarder {
    table: Table,
    key: Vec<u8>,
    peers: Vec<Live>,
}

impl Forwarder {
    /// Every peer whose address parses (one that doesn't is left out, saying why).
    pub fn new(table: Table) -> Forwarder {
        let key = B64.decode(&table.key).unwrap_or_default();
        let peers = table
            .peers
            .iter()
            .filter_map(|r| match live(r) {
                Ok(l) => Some(l),
                Err(e) => {
                    tracing::warn!(peer = %r.name, url = %r.url, error = %format!("{e:#}"), "a peered cluster's address can't be used");
                    None
                }
            })
            .collect();
        Forwarder { table, key, peers }
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    /// The peer a `Host` (no port) names, when it is `peer-<slug>.<anything>` and `slug` is one.
    pub fn slug_of_host<'a>(&self, host: &'a str) -> Option<&'a str> {
        let slug = host.strip_prefix(LABEL)?.split('.').next()?;
        self.has(slug).then_some(slug)
    }

    pub fn has(&self, slug: &str) -> bool {
        self.peers.iter().any(|p| p.route.slug == slug)
    }

    /// The peer a path is under (`/.kiln-peer/<slug>/…`, `slug` one of them), and the rest of
    /// the path (`/…`, or nothing).
    pub fn slug_of_path<'a>(&self, path: &'a str) -> Option<(&'a str, &'a str)> {
        let rest = path.strip_prefix(PATH)?;
        let (slug, tail) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
        self.has(slug).then_some((slug, tail))
    }

    /// Answer `req` at the peer `slug`'s address: entering, else forwarding with the grant it
    /// carries. `secure`: the browser reached this address over HTTPS (the cookie says so).
    pub async fn serve(&self, slug: &str, req: Request, secure: bool) -> Response {
        let Some(live) = self.peers.iter().find(|p| p.route.slug == slug) else {
            return srv::text(StatusCode::NOT_FOUND, "no such peered cluster here");
        };
        if req.uri().path() == ENTER {
            return self.enter(live, req, secure, "").await;
        }
        let Some(grant) = cookie(req.headers(), &cookie_name(slug)).filter(|g| self.check(g, live).is_some()) else {
            return opens_from_picker(live, StatusCode::UNAUTHORIZED);
        };
        forward(live, req, &grant, "").await
    }

    /// Answer `req` under this cluster's own address for the peer `slug` (`/.kiln-peer/<slug>/…`):
    /// entering, else its API (`…/api/…`) with the grant the path's cookie holds. Its pages are
    /// this cluster's app, which the caller serves.
    pub async fn serve_path(&self, slug: &str, req: Request, secure: bool) -> Response {
        let Some(live) = self.peers.iter().find(|p| p.route.slug == slug) else {
            return srv::text(StatusCode::NOT_FOUND, "no such peered cluster here");
        };
        let base = format!("{PATH}{slug}");
        let tail = req.uri().path().strip_prefix(base.as_str()).unwrap_or_default().to_owned();
        if tail == "/enter" {
            return self.enter(live, req, secure, &base).await;
        }
        if tail != "/api" && !tail.starts_with("/api/") {
            return srv::text(StatusCode::NOT_FOUND, "not found");
        }
        let Some(grant) = cookie(req.headers(), &cookie_name(slug)).filter(|g| self.check(g, live).is_some()) else {
            return opens_from_picker(live, StatusCode::UNAUTHORIZED);
        };
        forward(live, without_base(req, &base), &grant, &base).await
    }

    /// The grant `token` is, when this cluster signed it for `live` and it hasn't expired.
    fn check(&self, token: &str, live: &Live) -> Option<Grant> {
        Grant::verify(token, &self.key, &live.route.kid, crate::util::now_unix())
    }

    /// `POST /.kiln-peer/enter` (`g`: the grant, `to`: the page): the grant as this address's
    /// cookie, then the page. A `GET` with them in its query works too. Under this cluster's own
    /// address (`base`: `/.kiln-peer/<slug>`), the cookie is that path's alone and the page is
    /// under it.
    async fn enter(&self, live: &Live, req: Request, secure: bool, base: &str) -> Response {
        let form = if req.method() == Method::POST {
            match srv::body_bytes(req.into_body(), ENTER_LIMIT).await {
                Ok(b) => parse_form(&String::from_utf8_lossy(&b)),
                Err(_) => Vec::new(),
            }
        } else {
            let q = srv::query(&req);
            q.into_iter().collect()
        };
        let get = |k: &str| form.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let (grant, to) = (get("g"), get("to"));
        let Some(g) = self.check(&grant, live) else {
            return opens_from_picker(live, StatusCode::FORBIDDEN);
        };
        let max_age = (g.e - crate::util::now_unix()).max(0);
        let to = if to.starts_with('/') && !to.starts_with("//") && !to.starts_with(ENTER) { to } else { "/".to_owned() };
        let (to, path) = if base.is_empty() { (to, "/".to_owned()) } else { (format!("{base}{to}"), format!("{base}/")) };
        let secure = if secure { "; Secure" } else { "" };
        let set = format!("{}={grant}; Path={path}; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}", cookie_name(&live.route.slug));
        let mut r = srv::status(StatusCode::SEE_OTHER);
        for (k, v) in [(header::LOCATION, to.as_str()), (header::SET_COOKIE, set.as_str()), (header::CACHE_CONTROL, "no-store")] {
            if let Ok(v) = HeaderValue::from_str(v) {
                r.headers_mut().insert(k, v);
            }
        }
        r
    }
}

/// Forward `req` to the peer with `grant`: a WebSocket piped, anything else streamed. `base`: the
/// path the peer is under at this cluster's own address (empty at its own address).
async fn forward(live: &Live, req: Request, grant: &str, base: &str) -> Response {
    let r = if crate::http::ws::is_upgrade(&req) { forward_ws(live, req, grant, base).await } else { forward_http(live, req, grant, base).await };
    r.unwrap_or_else(|e| {
        tracing::debug!(peer = %live.route.name, error = %format!("{e:#}"), "forwarding to a peered cluster failed");
        srv::text(StatusCode::BAD_GATEWAY, &unanswered(&live.route, &format!("{e:#}")))
    })
}

/// `req` as the peer has it: its path without `base` (`/.kiln-peer/<slug>`).
fn without_base<B>(req: http::Request<B>, base: &str) -> http::Request<B> {
    let (mut parts, body) = req.into_parts();
    let pq = parts.uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str);
    let rest = pq.strip_prefix(base).filter(|r| r.starts_with('/')).unwrap_or("/");
    if let Ok(u) = rest.parse() {
        parts.uri = u;
    }
    http::Request::from_parts(parts, body)
}

/// What the frame says when the peer didn't answer: why, and the fix when it's a known one.
fn unanswered(route: &Route, error: &str) -> String {
    let name = &route.name;
    // TLS spoken to a port that answers plain HTTP (a kiln server's own port, without HTTPS on).
    if route.url.starts_with("https://") && (error.contains("InvalidContentType") || error.contains("corrupt message")) {
        let plain = route.url.replacen("https://", "http://", 1);
        return format!("{name} didn't answer over HTTPS: {} answers plain HTTP. Change its address to {plain} (Clusters → {name} → Change address).", route.url);
    }
    format!("{name} didn't answer: {error}")
}

/// The cookie holding a peer's grant at its address, or under this cluster's (per peer).
pub fn cookie_name(slug: &str) -> String {
    format!("kiln_peer_{slug}")
}

fn live(r: &Route) -> anyhow::Result<Live> {
    let uri: http::Uri = r.url.trim().parse().map_err(|e| anyhow::anyhow!("{e}"))?;
    let https = match uri.scheme_str() {
        Some("https") => true,
        Some("http") => false,
        other => anyhow::bail!("scheme {other:?} is neither http nor https"),
    };
    let authority = uri.authority().map(|a| a.as_str().to_owned()).ok_or_else(|| anyhow::anyhow!("no host"))?;
    let host = uri.host().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_owned();
    anyhow::ensure!(!host.is_empty(), "no host");
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let ca = B64.decode(&r.ca).unwrap_or_default();
    let roots = if ca.is_empty() { Roots::WebPki } else { Roots::WebPkiPlusDer(ca) };
    let tls = Arc::new(ClientTls::new(roots, None)?);
    let origin = format!("{}://{authority}", if https { "https" } else { "http" });
    Ok(Live { route: r.clone(), host, port, https, origin, authority, tls })
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
}

fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (srv::decode(&k.replace('+', " ")), srv::decode(&v.replace('+', " ")))).collect()
}

/// What someone without a grant for the peer sees at its address.
fn opens_from_picker(live: &Live, status: StatusCode) -> Response {
    let name = live.route.name.replace(['<', '>', '&', '"'], "");
    let page = format!(
        "<!doctype html><meta charset=utf-8><title>{name}</title><body style=\"background:#0b0c0e;color:#d9dbe0;font:14px system-ui;padding:40px\"><p>{name} opens from kiln's cluster picker.</p><p style=\"color:#7b7e88\">Open it there again; what let you in here has expired.</p>"
    );
    srv::response(status, "text/html; charset=utf-8", srv::full(page))
}

/// The headers a request goes to the peer with (`origin`: `scheme://authority` as the peer names
/// itself; `base`: the path it is under here, if any).
fn upstream_headers(incoming: &HeaderMap, origin: &str, authority: &str, grant: &str, base: &str) -> HeaderMap {
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
    set(header::HOST, authority);
    if incoming.contains_key(header::ORIGIN) {
        set(header::ORIGIN, origin);
    }
    // The page it came from, at the peer's own address.
    if let Some(path) = incoming.get(header::REFERER).and_then(|v| v.to_str().ok()).and_then(|r| r.parse::<http::Uri>().ok()).and_then(|u| u.path_and_query().map(|p| p.as_str().to_owned())) {
        let path = if base.is_empty() { path.as_str() } else { path.strip_prefix(base).filter(|p| p.starts_with('/')).unwrap_or("/") };
        set(header::REFERER, &format!("{origin}{path}"));
    }
    set(header::COOKIE, &format!("kiln_session={grant}"));
    h
}

/// The headers a peer's answer comes back with (`base`: the path it is under here, if any).
fn downstream_headers(h: &mut HeaderMap, origin: &str, base: &str) {
    for k in HOP {
        h.remove(k);
    }
    // Its cookies would land on this cluster's names.
    h.remove(header::SET_COOKIE);
    // A redirect to its own address stays in the frame (under its path here).
    let to = h.get(header::LOCATION).and_then(|v| v.to_str().ok()).and_then(|l| match l.strip_prefix(origin) {
        Some(rest) => Some(if rest.starts_with('/') { rest.to_owned() } else { format!("/{rest}") }),
        None => (!base.is_empty() && l.starts_with('/') && !l.starts_with("//")).then(|| l.to_owned()),
    });
    if let Some(to) = to
        && let Ok(v) = HeaderValue::from_str(&format!("{base}{to}"))
    {
        h.insert(header::LOCATION, v);
    }
    // Under this cluster's own address, nothing the peer sends runs as this cluster's page: a page
    // of its opened here gets an origin of its own, and no answer is taken for another type.
    if !base.is_empty() {
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox allow-scripts allow-forms allow-popups allow-downloads"));
        h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    }
}

fn target(uri: &http::Uri) -> String {
    uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str).to_owned()
}

async fn forward_http(live: &Live, req: Request, grant: &str, base: &str) -> anyhow::Result<Response> {
    let (parts, body) = req.into_parts();
    let mut up = http::Request::builder().method(parts.method).uri(target(&parts.uri)).body(body.map_err(std::io::Error::other).boxed())?;
    *up.headers_mut() = upstream_headers(&parts.headers, &live.origin, &live.authority, grant, base);
    let resp = crate::http::io::send(&live.tls, &live.host, live.port, live.https, up).await?;
    let (mut head, body) = resp.into_parts();
    downstream_headers(&mut head.headers, &live.origin, base);
    Ok(http::Response::from_parts(head, body.map_err(std::io::Error::other).boxed()))
}

/// A WebSocket: the upgrade asked of the peer first, its answer the browser's; then both
/// connections piped until either ends.
async fn forward_ws(live: &Live, mut req: Request, grant: &str, base: &str) -> anyhow::Result<Response> {
    let browser = kiln_http::upgrade::on(&mut req);
    let (parts, _) = req.into_parts();
    let mut up = http::Request::builder().method(Method::GET).uri(target(&parts.uri)).body(srv::empty())?;
    let mut headers = upstream_headers(&parts.headers, &live.origin, &live.authority, grant, base);
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    *up.headers_mut() = headers;
    let tcp = crate::http::io::connect_out(&live.host, live.port, live.https).await?;
    let mut sender = if live.https {
        let stream = TlsStream::connect(RtFuturesIo::new(tcp), live.tls.config()?, crate::http::tls::server_name(&live.host)?).await.map_err(|e| anyhow::anyhow!("TLS to {}: {e}", live.host))?;
        let (sender, conn) = kiln_http::client::http1::handshake::<_, srv::Body>(stream).await?;
        crate::rt::spawn_local(async move {
            let _ = conn.with_upgrades().await;
        })
        .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
        sender
    } else {
        let (sender, conn) = kiln_http::client::http1::handshake::<_, srv::Body>(RtIo::new(tcp)).await?;
        crate::rt::spawn_local(async move {
            let _ = conn.with_upgrades().await;
        })
        .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
        sender
    };
    let mut resp = sender.send_request(up).await?;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        let (mut head, body) = resp.into_parts();
        downstream_headers(&mut head.headers, &live.origin, base);
        return Ok(http::Response::from_parts(head, body.map_err(std::io::Error::other).boxed()));
    }
    let peer = kiln_http::upgrade::on(&mut resp);
    let mut out = http::Response::new(srv::empty());
    *out.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    for k in [header::UPGRADE, header::CONNECTION, header::SEC_WEBSOCKET_ACCEPT, header::SEC_WEBSOCKET_PROTOCOL, header::SEC_WEBSOCKET_EXTENSIONS] {
        if let Some(v) = resp.headers().get(&k) {
            out.headers_mut().insert(k, v.clone());
        }
    }
    let name = live.route.name.clone();
    crate::rt::spawn_local(async move {
        match (browser.await, peer.await) {
            (Ok(b), Ok(p)) => {
                if let Err(e) = crate::frontdoor::proxy::pipe(b, p).await {
                    tracing::debug!(peer = %name, error = %e, "a WebSocket to a peered cluster ended");
                }
            }
            (b, p) => tracing::debug!(peer = %name, browser = b.is_ok(), peer_side = p.is_ok(), "a WebSocket to a peered cluster didn't open"),
        }
    })
    .map_err(|e| anyhow::anyhow!("spawn WebSocket pipe: {e}"))?;
    Ok(out)
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
    fn a_request_goes_on_as_the_peers_own_with_the_grant_for_credentials() {
        let incoming = headers(&[
            ("host", "peer-work.kiln.home"),
            ("cookie", "kiln_session=ours; kiln_peer_work=g"),
            ("authorization", "Bearer kiln_pat_ours"),
            ("origin", "https://peer-work.kiln.home"),
            ("referer", "https://peer-work.kiln.home/agents?x=1"),
            ("connection", "keep-alive"),
            ("x-forwarded-for", "10.0.0.9"),
            ("accept", "text/html"),
            ("sec-websocket-key", "abc"),
        ]);
        let h = upstream_headers(&incoming, "https://kiln.work", "kiln.work", "kiln_peer_GRANT", "");
        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        assert_eq!(get("host").as_deref(), Some("kiln.work"));
        assert_eq!(get("origin").as_deref(), Some("https://kiln.work"));
        assert_eq!(get("referer").as_deref(), Some("https://kiln.work/agents?x=1"));
        // Our session and token never reach the peer: the grant is the only credential.
        assert_eq!(get("cookie").as_deref(), Some("kiln_session=kiln_peer_GRANT"));
        assert_eq!(get("authorization"), None);
        assert_eq!((get("connection"), get("x-forwarded-for")), (None, None));
        assert_eq!((get("accept").as_deref(), get("sec-websocket-key").as_deref()), (Some("text/html"), Some("abc")));
        // No Origin asked for none.
        assert_eq!(upstream_headers(&headers(&[("accept", "*/*")]), "https://kiln.work", "kiln.work", "g", "").get("origin"), None);
        // Under this cluster's address, the page it came from is the peer's page without the path.
        let under = upstream_headers(&headers(&[("referer", "http://192.168.1.77:7070/.kiln-peer/work/agents?x=1")]), "http://192.168.1.70:7070", "192.168.1.70:7070", "g", "/.kiln-peer/work");
        assert_eq!(under.get("referer").and_then(|v| v.to_str().ok()), Some("http://192.168.1.70:7070/agents?x=1"));
    }

    #[test]
    fn an_answer_keeps_its_cookies_and_redirects_to_itself() {
        let mut h = headers(&[("set-cookie", "kiln_session=theirs"), ("location", "https://kiln.work/login?next=/"), ("transfer-encoding", "chunked"), ("content-type", "text/html")]);
        downstream_headers(&mut h, "https://kiln.work", "");
        assert!(h.get("set-cookie").is_none() && h.get("transfer-encoding").is_none());
        assert_eq!(h.get("location").and_then(|v| v.to_str().ok()), Some("/login?next=/"));
        assert!(h.get("content-security-policy").is_none(), "at its own address its pages are its own");
        let mut elsewhere = headers(&[("location", "https://github.com/login")]);
        downstream_headers(&mut elsewhere, "https://kiln.work", "");
        assert_eq!(elsewhere.get("location").and_then(|v| v.to_str().ok()), Some("https://github.com/login"));
    }

    #[test]
    fn under_this_clusters_address_an_answer_stays_under_its_path_and_sandboxed() {
        let base = "/.kiln-peer/work";
        let mut h = headers(&[("location", "http://192.168.1.70:7070/login?next=/"), ("content-type", "text/html")]);
        downstream_headers(&mut h, "http://192.168.1.70:7070", base);
        assert_eq!(h.get("location").and_then(|v| v.to_str().ok()), Some("/.kiln-peer/work/login?next=/"));
        assert!(h.get("content-security-policy").and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("sandbox") && !c.contains("allow-same-origin")));
        assert_eq!(h.get("x-content-type-options").and_then(|v| v.to_str().ok()), Some("nosniff"));
        let mut relative = headers(&[("location", "/api/runs")]);
        downstream_headers(&mut relative, "http://192.168.1.70:7070", base);
        assert_eq!(relative.get("location").and_then(|v| v.to_str().ok()), Some("/.kiln-peer/work/api/runs"));
        let mut away = headers(&[("location", "//evil.example/x")]);
        downstream_headers(&mut away, "http://192.168.1.70:7070", base);
        assert_eq!(away.get("location").and_then(|v| v.to_str().ok()), Some("//evil.example/x"), "not taken for a path here");
    }

    #[test]
    fn a_request_under_a_peers_path_goes_on_without_it() {
        let req = |uri: &str| http::Request::builder().uri(uri).body(srv::empty()).map(|r| without_base(r, "/.kiln-peer/work"));
        assert_eq!(req("/.kiln-peer/work/api/me?x=1").map(|r| r.uri().to_string()).ok().as_deref(), Some("/api/me?x=1"));
        assert_eq!(req("/.kiln-peer/work").map(|r| r.uri().to_string()).ok().as_deref(), Some("/"));
        assert_eq!(req("/.kiln-peer/workers/api/me").map(|r| r.uri().to_string()).ok().as_deref(), Some("/"), "another peer's path isn't this one's");
    }

    #[test]
    fn a_peers_address_names_it() -> anyhow::Result<()> {
        let key = super::super::codes::Key::generate()?;
        let f = Forwarder::new(Table {
            key: B64.encode(key.public()),
            kid: key.id(),
            peers: vec![Route { slug: "work".into(), name: "Work".into(), kid: "k".into(), url: "https://kiln.work".into(), ca: String::new() }, Route { slug: "bad".into(), url: "ftp://x".into(), ..Route::default() }],
        });
        assert_eq!(f.slug_of_host("peer-work.kiln.home"), Some("work"));
        assert_eq!(f.slug_of_host("peer-work.localhost"), Some("work"));
        assert_eq!(f.slug_of_host("work.kiln.home"), None);
        assert_eq!(f.slug_of_host("peer-bad.kiln.home"), None, "an address that can't be used isn't served");
        assert_eq!(f.slug_of_path("/.kiln-peer/work/api/me"), Some(("work", "/api/me")));
        assert_eq!(f.slug_of_path("/.kiln-peer/work"), Some(("work", "")));
        assert_eq!(f.slug_of_path("/.kiln-peer/enter"), None, "the way in at a peer's own address names no peer");
        assert_eq!(f.slug_of_path("/.kiln-peer/bad/api/me"), None);
        assert_eq!(f.slug_of_path("/agents"), None);
        Ok(())
    }

    #[test]
    fn https_to_plain_http_says_how_to_fix_it() {
        let r = Route { name: "work".into(), url: "https://192.168.1.70:7070".into(), ..Route::default() };
        let said = unanswered(&r, "TLS to 192.168.1.70: received corrupt message of type InvalidContentType");
        assert!(said.contains("answers plain HTTP") && said.contains("http://192.168.1.70:7070") && said.contains("Change address"), "{said}");
        assert_eq!(unanswered(&r, "connection refused"), "work didn't answer: connection refused");
    }

    #[test]
    fn a_posted_form_reads_back() {
        assert_eq!(parse_form("g=kiln_peer_a.b&to=%2Fagents%3Fx%3D1"), vec![("g".to_owned(), "kiln_peer_a.b".to_owned()), ("to".to_owned(), "/agents?x=1".to_owned())]);
    }
}
