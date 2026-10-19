//! The code host legs fetch from (docs/ci.md W14, its first part): git's smart HTTP over the
//! server's mirror of the run's repository, so `actions/checkout` works unmodified with the leg's
//! own token as `GITHUB_TOKEN`: no GitHub credentials, private repositories included, at LAN
//! speed. `GITHUB_SERVER_URL` points here; the request is served by `git http-backend` (protocol
//! v0 to v2, gzipped requests, fetches by SHA), and a token may read only its own run's
//! repository.

use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use http::StatusCode;

use super::{Fail, runs, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

fn unauthorized() -> Response {
    let r = srv::text(StatusCode::UNAUTHORIZED, "a leg's token is needed (GITHUB_TOKEN)");
    srv::with_header(r, http::header::WWW_AUTHENTICATE, "Basic realm=\"kiln\"")
}

/// `/_kiln/git/<owner>/<repo>[.git]/info/refs` and `…/git-upload-pack`; a native repository's
/// (W14-R02) are [`super::hosting`]'s, pushes too.
pub async fn git(hub: &Arc<Hub>, rest: &[&str], req: Request) -> Result<Response, Fail> {
    let [owner, name, tail @ ..] = rest else { return Err((StatusCode::NOT_FOUND, "no such repository".into())) };
    let name = name.trim_end_matches(".git");
    let full = format!("{owner}/{name}");
    let repos = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if let Some((repo, path)) = repos.iter().find(|r| r.name.eq_ignore_ascii_case(&full)).and_then(|r| super::hosting::native_path(hub, r).map(|p| (r, p))) {
        if !matches!(tail, ["info", "refs"] | ["git-upload-pack"] | ["git-receive-pack"]) {
            return Err((StatusCode::NOT_FOUND, "not a git request".into()));
        }
        return super::hosting::serve(hub, repo, &path, tail, req).await;
    }
    if !matches!(tail, ["info", "refs"] | ["git-upload-pack"]) {
        return Err((StatusCode::FORBIDDEN, "this code host serves fetches only".into()));
    }
    // A leg's GITHUB_TOKEN, with `contents: read` (W10-R04).
    let Some(job) = token::job_of_request(hub, req.headers()).await else { return Ok(unauthorized()) };
    if !job.may("contents", kiln_ci::plan::perms::Level::Read) {
        return Err((StatusCode::FORBIDDEN, "this job's token lacks `contents: read`".into()));
    }
    let leg = job.leg;
    let run = runs::run_of(hub, &leg).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.ok_or((StatusCode::UNAUTHORIZED, "the leg is gone".to_owned()))?;
    if !run.repo_name.eq_ignore_ascii_case(&full) {
        return Err((StatusCode::FORBIDDEN, format!("this leg's token reads {} only", run.repo_name)));
    }
    let repo = repos.into_iter().find(|r| r.id == run.repo_id).ok_or((StatusCode::NOT_FOUND, "no such repository".to_owned()))?;
    let mirror = crate::gitops::ensure_commit(hub, &repo, &run.sha).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("the mirror: {e:#}")))?;
    let (Some(root), Some(dir)) = (mirror.parent(), mirror.file_name()) else { return Err((StatusCode::INTERNAL_SERVER_ERROR, "the mirror's path".into())) };
    let path_info = format!("/{}/{}", dir.to_string_lossy(), tail.join("/"));
    let cgi = Cgi { root, path_info: &path_info, configs: &["uploadpack.allowAnySHA1InWant=true", "uploadpack.allowFilter=true"], user: "kiln", method: req.method().as_str().to_owned(), query: req.uri().query().unwrap_or_default().to_owned(), headers: req.headers().clone() };
    let body = srv::body_bytes(req.into_body(), 64 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    backend(&cgi, body.to_vec(), None).await
}

/// A request for `git http-backend`: the repositories' root, the path below it, `-c` settings and
/// the CGI's request variables.
pub(crate) struct Cgi<'a> {
    pub root: &'a std::path::Path,
    pub path_info: &'a str,
    pub configs: &'a [&'a str],
    pub user: &'a str,
    pub method: String,
    pub query: String,
    pub headers: http::HeaderMap,
}

/// What `git http-backend` answers, its request body `prefix` and then, when given, the rest of
/// the request as it arrives (a push's pack, never held whole).
pub(crate) async fn backend(cgi: &Cgi<'_>, prefix: Vec<u8>, rest: Option<hyper::body::Incoming>) -> Result<Response, Fail> {
    let a = run(cgi, prefix, rest).await?;
    let mut r = srv::response(a.status, &a.content_type, srv::channel_body(a.body));
    for (k, v) in a.extra {
        if let Ok(name) = http::header::HeaderName::from_bytes(k.as_bytes()) {
            r = srv::with_header(r, name, &v);
        }
    }
    Ok(r)
}

/// `git http-backend`'s answer: its status and headers, and its body as it comes.
pub(crate) struct Answer {
    pub status: StatusCode,
    pub content_type: String,
    pub extra: Vec<(String, String)>,
    pub body: async_channel::Receiver<Bytes>,
}

impl Answer {
    /// The whole body, up to `cap` bytes.
    pub async fn bytes(self, cap: usize) -> Result<Vec<u8>, Fail> {
        let mut out = Vec::new();
        while let Ok(chunk) = self.body.recv().await {
            out.extend_from_slice(&chunk);
            if out.len() > cap {
                return Err((StatusCode::BAD_GATEWAY, "git's answer is too long".into()));
            }
        }
        Ok(out)
    }
}

/// Runs `git http-backend` for `cgi`: [`backend`]'s, its answer not yet a response.
pub(crate) async fn run(cgi: &Cgi<'_>, prefix: Vec<u8>, rest: Option<hyper::body::Incoming>) -> Result<Answer, Fail> {
    let header = |h: &str| cgi.headers.get(h).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let mut cmd = Command::new("git");
    for c in cgi.configs {
        cmd.args(["-c", c]);
    }
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", cgi.root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", cgi.path_info)
        .env("REQUEST_METHOD", &cgi.method)
        .env("QUERY_STRING", &cgi.query)
        .env("REMOTE_USER", cgi.user)
        .env("REMOTE_ADDR", "127.0.0.1");
    if let Some(ct) = header("content-type") {
        cmd.env("CONTENT_TYPE", ct);
    }
    if let Some(enc) = header("content-encoding") {
        cmd.env("HTTP_CONTENT_ENCODING", enc);
    }
    if let Some(p) = header("git-protocol") {
        cmd.env("GIT_PROTOCOL", p);
    }
    // The whole body's length: what was read, or what the request said it would send.
    match (&rest, header("content-length")) {
        (None, _) => {
            cmd.env("CONTENT_LENGTH", prefix.len().to_string());
        }
        (Some(_), Some(n)) => {
            cmd.env("CONTENT_LENGTH", n);
        }
        (Some(_), None) => {}
    }
    let d = crate::proc::spawn_duplex(cmd).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("git http-backend: {e:#}")))?;
    let stdin = d.stdin.clone();
    drop(d.stdin);
    crate::rt::spawn(async move {
        if !prefix.is_empty() && stdin.send(prefix).await.is_err() {
            return;
        }
        if let Some(mut body) = rest {
            while let Ok(Some(chunk)) = srv::next_chunk(&mut body).await {
                if stdin.send(chunk.to_vec()).await.is_err() {
                    return;
                }
            }
        }
        drop(stdin);
    });
    // The CGI's headers, then its body as it comes.
    let mut head: Vec<u8> = Vec::new();
    let mut rest_of: Vec<u8> = Vec::new();
    while let Ok(chunk) = d.stdout.recv().await {
        head.extend_from_slice(&chunk);
        if let Some(i) = find(&head, b"\r\n\r\n").map(|i| (i, 4)).or_else(|| find(&head, b"\n\n").map(|i| (i, 2))) {
            rest_of = head.split_off(i.0 + i.1);
            head.truncate(i.0);
            break;
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut status = StatusCode::OK;
    let mut content_type = "application/octet-stream".to_owned();
    let mut extra: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("status") {
            status = v.split_whitespace().next().and_then(|c| c.parse::<u16>().ok()).and_then(|c| StatusCode::from_u16(c).ok()).unwrap_or(StatusCode::OK);
        } else if k.eq_ignore_ascii_case("content-type") {
            content_type = v.to_owned();
        } else if k.eq_ignore_ascii_case("cache-control") || k.eq_ignore_ascii_case("expires") || k.eq_ignore_ascii_case("pragma") {
            extra.push((k.to_owned(), v.to_owned()));
        }
    }
    let (tx, rx) = async_channel::bounded::<Bytes>(16);
    let out = d.stdout;
    crate::rt::spawn(async move {
        if !rest_of.is_empty() && tx.send(Bytes::from(rest_of)).await.is_err() {
            return;
        }
        while let Ok(c) = out.recv().await {
            if tx.send(Bytes::from(c)).await.is_err() {
                return;
            }
        }
    });
    Ok(Answer { status, content_type, extra, body: rx })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
