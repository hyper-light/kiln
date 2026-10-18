//! kiln's UI server: TanStack Start under Bun (`ui/server/main.ts`), run and watched by the kiln
//! server. It renders pages and React Server Components and answers the UI's server functions.
//! The kiln server serves the UI's static files itself (`crate::ui`) and forwards the rest of the
//! UI's requests here (`serve`), streaming both ways. If it exits it starts again; without Bun or
//! its files, kiln serves the app's shell and the app runs in the browser.
//!
//! Where things are: `KILN_UI_DIR` (default `/usr/local/lib/kiln/ui`, which holds `server/main.ts`
//! and `dist/server`), `KILN_BUN` (default: `bun` on the PATH).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use http::{HeaderMap, Method, StatusCode, header};
use http_body_util::BodyExt as _;

use crate::http::server::{self as srv, Request, Response};
use crate::http::client::Roots;
use crate::http::tls::ClientTls;
use crate::hub::Hub;

/// The UI server's port while it's up; 0 while it isn't.
static PORT: AtomicU16 = AtomicU16::new(0);

/// Headers about one connection, never forwarded.
const HOP: [&str; 8] = ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"];

/// How long a start may take to accept connections.
const STARTING: Duration = Duration::from_secs(20);

/// The UI's answer to `req`: a static file, else the UI server's (any method), else, for a page,
/// the shell. A server function or server component asked for while the UI server isn't there gets
/// a 503, never the shell: the page asking would take HTML for its answer.
pub async fn serve(req: Request) -> Response {
    let get = matches!(*req.method(), Method::GET | Method::HEAD);
    if get && let Some(r) = crate::ui::static_file(&req) {
        return r;
    }
    let page = get && srv::header_str(&req, &header::ACCEPT).is_some_and(|a| a.contains("text/html"));
    let port = PORT.load(Ordering::Relaxed);
    if port == 0 {
        return if page { crate::ui::shell(&req) } else { srv::text(StatusCode::SERVICE_UNAVAILABLE, "kiln's UI server isn't running") };
    }
    // The shell stands in for a page the UI server couldn't answer (it's restarting, say).
    let fallback = page.then(|| crate::ui::shell(&req));
    match forward(req, port).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "the UI server didn't answer");
            fallback.unwrap_or_else(|| srv::text(StatusCode::BAD_GATEWAY, "kiln's UI server didn't answer"))
        }
    }
}

fn tls() -> anyhow::Result<&'static ClientTls> {
    static TLS: OnceLock<ClientTls> = OnceLock::new();
    if let Some(t) = TLS.get() {
        return Ok(t);
    }
    let t = ClientTls::new(Roots::WebPki, None)?;
    Ok(TLS.get_or_init(|| t))
}

/// The scheme the browser used. kiln's own listener speaks HTTP (the front door terminates TLS
/// before it), so: what a proxy in front said, else the scheme of the request's own Origin or
/// Referer when it names this host (a browser sends one on every server function call). The UI
/// server checks a call's Origin against it, so a page's own calls pass and other sites' don't.
fn scheme(headers: &HeaderMap, host: &str) -> &'static str {
    let said = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok());
    match said {
        Some("https") => return "https",
        Some("http") => return "http",
        _ => {}
    }
    let named = [header::ORIGIN, header::REFERER].into_iter().filter_map(|h| headers.get(h)?.to_str().ok()?.parse::<http::Uri>().ok());
    for u in named {
        if u.authority().is_some_and(|a| a.as_str().eq_ignore_ascii_case(host)) {
            return if u.scheme_str() == Some("https") { "https" } else { "http" };
        }
    }
    "http"
}

/// Forwards `req` to the UI server and streams its answer back.
async fn forward(req: Request, port: u16) -> anyhow::Result<Response> {
    let (parts, body) = req.into_parts();
    let host = parts.headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    let mut b = http::Request::builder().method(parts.method).uri(parts.uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str));
    for (k, v) in &parts.headers {
        if !HOP.contains(&k.as_str()) {
            b = b.header(k, v);
        }
    }
    b = b.header("x-forwarded-proto", scheme(&parts.headers, &host)).header("x-forwarded-host", &host);
    let up = b.body(body.map_err(std::io::Error::other).boxed())?;
    let resp = crate::http::io::send(tls()?, "127.0.0.1", port, false, up).await?;
    let (mut head, body) = resp.into_parts();
    for h in HOP {
        head.headers.remove(h);
    }
    Ok(http::Response::from_parts(head, body.map_err(std::io::Error::other).boxed()))
}

/// The UI server's files and the Bun to run them, when both are there.
fn files() -> Option<(PathBuf, PathBuf)> {
    let dir = std::env::var_os("KILN_UI_DIR").map_or_else(|| PathBuf::from("/usr/local/lib/kiln/ui"), PathBuf::from);
    let main = dir.join("server").join("main.ts");
    if !main.is_file() || !dir.join("dist").join("server").join("server.js").is_file() {
        return None;
    }
    let bun = std::env::var_os("KILN_BUN").map(PathBuf::from).or_else(|| on_path("bun"))?;
    Some((bun, main))
}

fn on_path(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// Where the UI server reaches kiln's API: this server's own HTTP listener.
fn api_url(http_addr: &str) -> String {
    let (host, port) = http_addr.rsplit_once(':').unwrap_or(("0.0.0.0", "7070"));
    let host = match host.trim_matches(['[', ']']) {
        "" | "0.0.0.0" | "::" => "127.0.0.1",
        h => h,
    };
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
    format!("http://{host}:{port}")
}

fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

async fn listening(port: u16) -> bool {
    let until = Instant::now() + STARTING;
    while Instant::now() < until {
        if crate::http::io::connect("127.0.0.1", port).await.is_ok() {
            return true;
        }
        crate::rt::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn start(bun: &Path, main: &Path, port: u16, api: &str) -> std::io::Result<Child> {
    Command::new(bun)
        .arg(main)
        .env("PORT", port.to_string())
        .env("HOST", "127.0.0.1")
        .env("KILN_API_URL", api)
        .env("NODE_ENV", "production")
        .stdin(Stdio::null())
        .spawn()
}

/// Runs the UI server for as long as kiln does, starting it again when it exits (backing off up to
/// 30 s when it keeps failing).
pub async fn run(hub: std::sync::Arc<Hub>) -> anyhow::Result<()> {
    let Some((bun, main)) = files() else {
        tracing::info!("no UI server here (Bun or its files are missing): every page is the app's shell, rendered in the browser");
        hub.shutdown.cancelled().await;
        return Ok(());
    };
    let api = api_url(&hub.cfg.server.http_addr);
    let mut backoff = Duration::from_secs(1);
    loop {
        let port = free_port()?;
        let started = Instant::now();
        let mut child = match start(&bun, &main, port, &api) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, bun = %bun.display(), "can't start the UI server");
                hub.shutdown.cancelled().await;
                return Ok(());
            }
        };
        if listening(port).await {
            PORT.store(port, Ordering::Relaxed);
            tracing::info!(port, "UI server up");
        } else {
            tracing::warn!(port, "the UI server didn't start listening in time");
        }
        // Until it exits, or kiln stops.
        let status = loop {
            if let Ok(Some(s)) = child.try_wait() {
                break Some(s);
            }
            futures::select_biased! {
                () = hub.shutdown.cancelled().fuse() => {
                    PORT.store(0, Ordering::Relaxed);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(());
                }
                () = crate::rt::sleep(Duration::from_millis(500)).fuse() => {}
            }
        };
        PORT.store(0, Ordering::Relaxed);
        backoff = if started.elapsed() > Duration::from_secs(60) { Duration::from_secs(1) } else { (backoff * 2).min(Duration::from_secs(30)) };
        tracing::warn!(status = ?status, again_in = ?backoff, "the UI server exited; starting it again");
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = crate::rt::sleep(backoff).fuse() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_api_is_reached_on_this_servers_own_listener() {
        assert_eq!(api_url("0.0.0.0:7070"), "http://127.0.0.1:7070");
        assert_eq!(api_url("[::]:8080"), "http://127.0.0.1:8080");
        assert_eq!(api_url("10.0.0.5:7070"), "http://10.0.0.5:7070");
        assert_eq!(api_url("[fd00::1]:7070"), "http://[fd00::1]:7070");
    }

    #[test]
    fn the_scheme_is_the_browsers_for_this_host_only() {
        let h = |pairs: &[(&'static str, &'static str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, http::HeaderValue::from_static(v));
            }
            m
        };
        assert_eq!(scheme(&h(&[("origin", "https://kiln.dev")]), "kiln.dev"), "https");
        assert_eq!(scheme(&h(&[("referer", "https://kiln.dev/runs")]), "kiln.dev"), "https");
        assert_eq!(scheme(&h(&[("origin", "https://evil.example")]), "kiln.dev"), "http");
        assert_eq!(scheme(&h(&[("x-forwarded-proto", "https")]), "kiln.dev"), "https");
        assert_eq!(scheme(&h(&[]), "192.168.1.77:7070"), "http");
    }
}
