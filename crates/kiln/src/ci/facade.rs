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

/// `/_kiln/git/<owner>/<repo>[.git]/info/refs` and `…/git-upload-pack`.
pub async fn git(hub: &Arc<Hub>, rest: &[&str], req: Request) -> Result<Response, Fail> {
    // kiln's own repositories (docs/deploy-api.md §7), with readers' tokens of their own.
    if let ["_hosted", hosted @ ..] = rest {
        return crate::deploy::serve::git(hub, hosted, req).await;
    }
    let [owner, name, tail @ ..] = rest else { return Err((StatusCode::NOT_FOUND, "no such repository".into())) };
    let name = name.trim_end_matches(".git");
    let full = format!("{owner}/{name}");
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
    let repos = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let repo = repos.into_iter().find(|r| r.id == run.repo_id).ok_or((StatusCode::NOT_FOUND, "no such repository".to_owned()))?;
    let mirror = crate::gitops::ensure_commit(hub, &repo, &run.sha).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("the mirror: {e:#}")))?;
    let (Some(root), Some(dir)) = (mirror.parent(), mirror.file_name()) else { return Err((StatusCode::INTERNAL_SERVER_ERROR, "the mirror's path".into())) };
    backend(req, root, &dir.to_string_lossy(), tail).await
}

/// Serves a fetch from the bare repository `<root>/<dir>` with `git http-backend`: `tail` is
/// `info/refs` or `git-upload-pack`.
pub(crate) async fn backend(req: Request, root: &std::path::Path, dir: &str, tail: &[&str]) -> Result<Response, Fail> {
    let header = |h: &str| req.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let mut cmd = Command::new("git");
    cmd.args(["-c", "uploadpack.allowAnySHA1InWant=true", "-c", "uploadpack.allowFilter=true", "http-backend"])
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", format!("/{dir}/{}", tail.join("/")))
        .env("REQUEST_METHOD", req.method().as_str())
        .env("QUERY_STRING", req.uri().query().unwrap_or_default())
        .env("REMOTE_USER", "kiln")
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
    let body = srv::body_bytes(req.into_body(), 64 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    cmd.env("CONTENT_LENGTH", body.len().to_string());
    let d = crate::proc::spawn_duplex(cmd).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("git http-backend: {e:#}")))?;
    let stdin = d.stdin.clone();
    drop(d.stdin);
    crate::rt::spawn(async move {
        if !body.is_empty() {
            let _ = stdin.send(body.to_vec()).await;
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
    let mut r = srv::response(status, &content_type, srv::channel_body(rx));
    for (k, v) in extra {
        if let Ok(name) = http::header::HeaderName::from_bytes(k.as_bytes()) {
            r = srv::with_header(r, name, &v);
        }
    }
    Ok(r)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
