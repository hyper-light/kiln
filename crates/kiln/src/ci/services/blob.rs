//! Signed blob URLs (docs/ci.md W08-R03): the subset of Azure's Blob REST API that GitHub's
//! toolkit uses through `@azure/storage-blob`, at `/_kiln/blob/<id>?se=…&sp=…&sig=…`.
//!
//! - **Reads:** `GET` and `HEAD`, with `Range` or `x-ms-range`.
//! - **Uploads:**
//!   - Put Block: `PUT ?comp=block&blockid=…`, the way `uploadStream` sends every upload.
//!   - Put Block List: `PUT ?comp=blocklist`, with the XML list of block ids.
//!   - A single `PUT` with `x-ms-blob-type: BlockBlob`.
//!
//! A committed upload is chunked into the store (`cas`). A URL names its target and what it
//! allows (`r` or `w`) until when, signed with a server key. A tampered or expired URL gets 403.

use std::path::PathBuf;
use std::sync::Arc;

use http::{Method, StatusCode, header};
use rusqlite::{OptionalExtension, params};

use super::cas;
use crate::ci::Fail;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// The largest block accepted (the toolkit sends 8 MiB blocks; Azure allows up to 4000 MiB).
const MAX_BLOCK: u64 = 256 << 20;

/// What a blob URL reads or writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Cache(i64),
    Artifact(i64),
}

impl Target {
    pub fn id(self) -> String {
        match self {
            Target::Cache(n) => format!("cache-{n}"),
            Target::Artifact(n) => format!("artifact-{n}"),
        }
    }

    pub fn parse(s: &str) -> Option<Target> {
        if let Some(n) = s.strip_prefix("cache-") {
            return n.parse().ok().map(Target::Cache);
        }
        s.strip_prefix("artifact-").and_then(|n| n.parse().ok()).map(Target::Artifact)
    }
}

fn key(hub: &Hub) -> String {
    crate::util::hmac_hex(hub.admin_token.as_bytes(), b"kiln ci blob urls v1")
}

fn sign(key: &str, id: &str, perm: &str, expiry: i64) -> String {
    crate::util::hmac_hex(key.as_bytes(), format!("blob\n{id}\n{perm}\n{expiry}").as_bytes())
}

/// The origin a request reached the server at (`http://host:port`), for URLs it hands back.
pub fn origin(req: &Request) -> String {
    let tls = req.extensions().get::<srv::Secure>().is_some() || req.headers().get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).map(str::to_owned).or_else(|| req.uri().authority().map(|a| a.to_string())).unwrap_or_else(|| "localhost".into());
    format!("{}://{host}", if tls { "https" } else { "http" })
}

/// A signed URL for `target` allowing `perm` (`r` or `w`) for `ttl_secs`.
pub fn url(hub: &Hub, origin: &str, target: Target, perm: &str, ttl_secs: i64) -> String {
    url_at(&key(hub), origin, target, perm, crate::util::now_unix() + ttl_secs)
}

fn url_at(key: &str, origin: &str, target: Target, perm: &str, expiry: i64) -> String {
    let id = target.id();
    format!("{origin}/_kiln/blob/{id}?sv=2023-11-03&se={expiry}&sp={perm}&sig={}", sign(key, &id, perm, expiry))
}

/// Whether a request's signature allows `perm` on `id` now.
fn allowed(key: &str, id: &str, q: &std::collections::HashMap<String, String>, perm: &str, now: i64) -> bool {
    let (Some(se), Some(sp), Some(sig)) = (q.get("se"), q.get("sp"), q.get("sig")) else { return false };
    let Ok(expiry) = se.parse::<i64>() else { return false };
    sp == perm && now < expiry && crate::util::ct_eq(sig.as_bytes(), sign(key, id, sp, expiry).as_bytes())
}

/// An error as Azure words it: an XML body and `x-ms-error-code`.
fn azure_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>{code}</Code><Message>{}</Message></Error>", xml_escape(message));
    let r = srv::response(status, "application/xml", srv::full(body));
    srv::with_header(r, http::HeaderName::from_static("x-ms-error-code"), code)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn azure_headers(mut r: Response) -> Response {
    for (k, v) in [("x-ms-version", "2023-11-03"), ("x-ms-request-id", &crate::util::new_id())] {
        if let Ok(name) = http::HeaderName::from_bytes(k.as_bytes()) {
            r = srv::with_header(r, name, v);
        }
    }
    r
}

/// The byte range a read asks for (`Range` or `x-ms-range`), as `[start, end)` within `size`.
/// `None` for the whole object. A range past the end is refused with `Err`.
/// The byte range a request asks for of `size` bytes, `[start, end)`: `None` for all of it, `Err` when
/// it can't be met.
pub(crate) fn byte_range(req: &Request, size: u64) -> Result<Option<(u64, u64)>, ()> {
    range(req, size)
}

fn range(req: &Request, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let v = req.headers().get("x-ms-range").or_else(|| req.headers().get(header::RANGE)).and_then(|v| v.to_str().ok());
    parse_range(v, size)
}

fn parse_range(v: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = v.and_then(|v| v.trim().strip_prefix("bytes=")) else { return Ok(None) };
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            // The last n bytes.
            let n: u64 = n.parse().map_err(|_| ())?;
            (size.saturating_sub(n), size)
        }
        (s, "") => (s.parse().map_err(|_| ())?, size),
        (s, e) => (s.parse().map_err(|_| ())?, e.parse::<u64>().map_err(|_| ())?.saturating_add(1).min(size)),
    };
    if start >= size || start >= end {
        return Err(());
    }
    Ok(Some((start, end)))
}

/// What a target reads as: its object, content type and download name.
struct Readable {
    object: String,
    mime: String,
    name: Option<String>,
}

async fn readable(hub: &Hub, t: Target) -> anyhow::Result<Option<Readable>> {
    hub.store
        .call(move |c| {
            Ok(match t {
                Target::Cache(id) => c
                    .query_row("SELECT object FROM ci_caches WHERE id = ?1 AND object IS NOT NULL", [id], |r| r.get::<_, String>(0))
                    .optional()?
                    .map(|object| Readable { object, mime: "application/octet-stream".into(), name: None }),
                Target::Artifact(id) => c
                    .query_row("SELECT object, COALESCE(mime_type, 'application/zip'), name FROM ci_artifacts WHERE id = ?1 AND finalized = 1 AND object IS NOT NULL", [id], |r| {
                        Ok(Readable { object: r.get(0)?, mime: r.get(1)?, name: Some(r.get(2)?) })
                    })
                    .optional()?,
            })
        })
        .await
}

/// `/_kiln/blob/<id>`.
pub async fn serve(hub: &Arc<Hub>, id: &str, req: Request) -> Result<Response, Fail> {
    let Some(target) = Target::parse(id) else { return Ok(azure_error(StatusCode::NOT_FOUND, "BlobNotFound", "The specified blob does not exist.")) };
    let q = srv::query(&req);
    let now = crate::util::now_unix();
    let k = key(hub);
    let method = req.method().clone();
    if method == Method::GET || method == Method::HEAD {
        if !allowed(&k, id, &q, "r", now) {
            return Ok(azure_error(StatusCode::FORBIDDEN, "AuthenticationFailed", "The URL's signature is not valid or has expired."));
        }
        return read(hub, target, &req).await;
    }
    if method != Method::PUT {
        return Ok(azure_error(StatusCode::METHOD_NOT_ALLOWED, "UnsupportedHttpVerb", "The resource doesn't support the specified HTTP verb."));
    }
    if !allowed(&k, id, &q, "w", now) {
        return Ok(azure_error(StatusCode::FORBIDDEN, "AuthenticationFailed", "The URL's signature is not valid or has expired."));
    }
    if !writable(hub, target).await.map_err(internal)? {
        return Ok(azure_error(StatusCode::CONFLICT, "BlobImmutableDueToPolicy", "This blob is finalized and can't be written."));
    }
    match q.get("comp").map(String::as_str) {
        Some("block") => {
            let Some(block) = q.get("blockid").filter(|b| !b.is_empty() && b.len() <= 128) else {
                return Ok(azure_error(StatusCode::BAD_REQUEST, "InvalidQueryParameterValue", "blockid is required."));
            };
            let path = stage_dir(hub, target).join("blocks").join(hex::encode(block.as_bytes()));
            match receive(req, &path, MAX_BLOCK).await {
                Ok(_) => Ok(azure_headers(srv::status(StatusCode::CREATED))),
                Err(e) => Ok(azure_error(StatusCode::BAD_REQUEST, "InvalidBlobOrBlock", &format!("{e:#}"))),
            }
        }
        Some("blocklist") => {
            let mime = req.headers().get("x-ms-blob-content-type").and_then(|v| v.to_str().ok()).map(str::to_owned);
            let body = srv::body_bytes(req.into_body(), 4 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let ids = block_list(&String::from_utf8_lossy(&body));
            let dir = stage_dir(hub, target);
            let limit = limit(hub, target);
            let assembled = dir.join("assembled");
            let (d, a) = (dir.clone(), assembled.clone());
            let joined = crate::rt::blocking(move || assemble(&d, &ids, &a, limit)).await.map_err(internal)?;
            if let Err(e) = joined {
                return Ok(azure_error(StatusCode::BAD_REQUEST, "InvalidBlockList", &e));
            }
            commit(hub, target, &assembled, mime).await
        }
        Some(other) => Ok(azure_error(StatusCode::BAD_REQUEST, "InvalidQueryParameterValue", &format!("comp={other} is not supported."))),
        None => {
            if req.headers().get("x-ms-blob-type").and_then(|v| v.to_str().ok()) != Some("BlockBlob") {
                return Ok(azure_error(StatusCode::BAD_REQUEST, "InvalidHeaderValue", "x-ms-blob-type must be BlockBlob."));
            }
            let mime = req.headers().get("x-ms-blob-content-type").or_else(|| req.headers().get(header::CONTENT_TYPE)).and_then(|v| v.to_str().ok()).map(str::to_owned);
            let assembled = stage_dir(hub, target).join("assembled");
            if let Err(e) = receive(req, &assembled, limit(hub, target)).await {
                return Ok(azure_error(StatusCode::BAD_REQUEST, "InvalidBlobOrBlock", &format!("{e:#}")));
            }
            commit(hub, target, &assembled, mime).await
        }
    }
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn stage_dir(hub: &Hub, t: Target) -> PathBuf {
    cas::staging(hub).join(t.id())
}

fn limit(hub: &Hub, t: Target) -> u64 {
    let gb = match t {
        Target::Cache(_) => hub.cfg.ci.cache.max_entry_gb,
        Target::Artifact(_) => hub.cfg.ci.artifacts.max_size_gb,
    };
    gb.saturating_mul(1 << 30)
}

/// Whether a target still takes writes: a cache entry still uploading, an artifact not
/// finalized.
async fn writable(hub: &Hub, t: Target) -> anyhow::Result<bool> {
    hub.store
        .call(move |c| {
            Ok(match t {
                Target::Cache(id) => c.query_row("SELECT object IS NULL FROM ci_caches WHERE id = ?1", [id], |r| r.get::<_, bool>(0)).optional()?.unwrap_or(false),
                Target::Artifact(id) => c.query_row("SELECT finalized = 0 FROM ci_artifacts WHERE id = ?1", [id], |r| r.get::<_, bool>(0)).optional()?.unwrap_or(false),
            })
        })
        .await
}

/// Writes a request body to `path`, refusing more than `max` bytes or fewer than its
/// `Content-Length` says.
pub async fn receive(req: Request, path: &std::path::Path, max: u64) -> anyhow::Result<u64> {
    let declared = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    anyhow::ensure!(declared.is_none_or(|n| n <= max), "the body is over {max} bytes");
    let (sink, done) = crate::rt::file_sink(path.to_path_buf(), max)?;
    let mut body = req.into_body();
    while let Some(chunk) = srv::next_chunk(&mut body).await? {
        if sink.send(chunk).await.is_err() {
            break;
        }
    }
    drop(sink);
    let n = done.await?;
    if let Some(d) = declared {
        anyhow::ensure!(n == d, "the body ended after {n} of {d} bytes");
    }
    Ok(n)
}

/// The block ids of a Put Block List body, in order (`Latest`, `Committed` or `Uncommitted`).
fn block_list(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        let after = rest.get(open + 1..).unwrap_or_default();
        let Some(close) = after.find('>') else { break };
        let tag = after.get(..close).unwrap_or_default();
        let body = after.get(close + 1..).unwrap_or_default();
        if matches!(tag, "Latest" | "Committed" | "Uncommitted") {
            let end = body.find("</").unwrap_or(body.len());
            out.push(body.get(..end).unwrap_or_default().trim().to_owned());
            rest = body.get(end..).unwrap_or_default();
        } else {
            rest = body;
        }
    }
    out
}

/// Joins the named blocks, in order, into `out`.
fn assemble(dir: &std::path::Path, ids: &[String], out: &std::path::Path, limit: u64) -> Result<(), String> {
    let mut f = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut total = 0u64;
    for id in ids {
        let p = dir.join("blocks").join(hex::encode(id.as_bytes()));
        let mut b = std::fs::File::open(&p).map_err(|_| format!("The block list names block {id}, which was never uploaded."))?;
        total += std::io::copy(&mut b, &mut f).map_err(|e| e.to_string())?;
        if total > limit {
            return Err(format!("The blob is over {limit} bytes."));
        }
    }
    f.sync_data().map_err(|e| e.to_string())
}

/// Chunks an assembled upload into the store and attaches it to its target.
async fn commit(hub: &Hub, target: Target, assembled: &std::path::Path, mime: Option<String>) -> Result<Response, Fail> {
    let obj = cas::put_file(hub, assembled).await.map_err(internal)?;
    let (digest, size) = (obj.digest.clone(), i64::try_from(obj.size).unwrap_or(i64::MAX));
    let attached = hub
        .store
        .call(move |c| {
            let n = match target {
                Target::Cache(id) => c.execute("UPDATE ci_caches SET object = ?2, size = ?3, last_used_at = ?4 WHERE id = ?1 AND object IS NULL", params![id, digest, size, crate::util::now_unix()])?,
                Target::Artifact(id) => c.execute("UPDATE ci_artifacts SET object = ?2, size = ?3, mime_type = COALESCE(?4, mime_type) WHERE id = ?1 AND finalized = 0", params![id, digest, size, mime])?,
            };
            Ok(n > 0)
        })
        .await
        .map_err(internal)?;
    let dir = stage_dir(hub, target);
    let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
    if !attached {
        return Ok(azure_error(StatusCode::CONFLICT, "BlobImmutableDueToPolicy", "This blob was finalized while it uploaded."));
    }
    let r = azure_headers(srv::status(StatusCode::CREATED));
    let r = srv::with_header(r, header::ETAG, &format!("\"{}\"", obj.digest));
    Ok(srv::with_header(r, header::LAST_MODIFIED, &httpdate(crate::util::now_unix())))
}

async fn read(hub: &Arc<Hub>, target: Target, req: &Request) -> Result<Response, Fail> {
    let Some(rd) = readable(hub, target).await.map_err(internal)? else {
        return Ok(azure_error(StatusCode::NOT_FOUND, "BlobNotFound", "The specified blob does not exist."));
    };
    let Some(obj) = cas::object(hub, &rd.object).await.map_err(internal)? else {
        return Ok(azure_error(StatusCode::NOT_FOUND, "BlobNotFound", "The blob's content is gone."));
    };
    if let Target::Cache(id) = target {
        let _ = hub.store.call(move |c| Ok(c.execute("UPDATE ci_caches SET last_used_at = ?2 WHERE id = ?1", params![id, crate::util::now_unix()])?)).await;
    }
    let (status, start, end) = match range(req, obj.size) {
        Ok(None) => (StatusCode::OK, 0, obj.size),
        Ok(Some((s, e))) => (StatusCode::PARTIAL_CONTENT, s, e),
        Err(()) => {
            let r = azure_error(StatusCode::RANGE_NOT_SATISFIABLE, "InvalidRange", "The range specified is invalid for the current size of the resource.");
            return Ok(srv::with_header(r, header::CONTENT_RANGE, &format!("bytes */{}", obj.size)));
        }
    };
    let body = if req.method() == Method::HEAD { srv::empty() } else { srv::channel_body(cas::read(hub, &obj, start, end)) };
    let mut r = azure_headers(srv::response(status, &rd.mime, body));
    r = srv::with_header(r, header::CONTENT_LENGTH, &(end - start).to_string());
    r = srv::with_header(r, header::ACCEPT_RANGES, "bytes");
    r = srv::with_header(r, header::ETAG, &format!("\"{}\"", obj.digest));
    r = srv::with_header(r, http::HeaderName::from_static("x-ms-blob-type"), "BlockBlob");
    if status == StatusCode::PARTIAL_CONTENT {
        r = srv::with_header(r, header::CONTENT_RANGE, &format!("bytes {start}-{}/{}", end - 1, obj.size));
    }
    // A file uploaded without zipping downloads under its own name (download-artifact reads it).
    if let Some(name) = rd.name.filter(|_| !rd.mime.contains("zip")) {
        let ascii: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
        r = srv::with_header(r, header::CONTENT_DISPOSITION, &format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", pct(&name)));
    }
    Ok(r)
}

fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// An HTTP date (`Last-Modified`).
pub fn httpdate(unix: i64) -> String {
    let f = time::macros::format_description!("[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT");
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&f).ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w08_ac04_signatures_are_checked_for_target_permission_and_expiry() {
        let k = "secret";
        let u = url_at(k, "http://h:1", Target::Artifact(7), "w", 2000);
        let q: std::collections::HashMap<String, String> = u.split_once('?').map(|(_, q)| q).unwrap_or_default().split('&').filter_map(|kv| kv.split_once('=')).map(|(a, b)| (a.to_owned(), b.to_owned())).collect();
        assert!(u.starts_with("http://h:1/_kiln/blob/artifact-7?"));
        assert!(allowed(k, "artifact-7", &q, "w", 1999));
        assert!(!allowed(k, "artifact-7", &q, "w", 2000), "expired");
        assert!(!allowed(k, "artifact-7", &q, "r", 1000), "a write URL doesn't read");
        assert!(!allowed(k, "artifact-8", &q, "w", 1000), "another blob");
        assert!(!allowed("other", "artifact-7", &q, "w", 1000), "another key");
        let mut tampered = q.clone();
        tampered.insert("se".into(), "9999999999".into());
        assert!(!allowed(k, "artifact-7", &tampered, "w", 1000), "a later expiry isn't signed");
        assert_eq!(Target::parse("cache-12"), Some(Target::Cache(12)));
        assert_eq!(Target::parse("artifact-x"), None);
    }

    #[test]
    fn w08_ac04_ranges_at_every_boundary() {
        let r = |v: &str| parse_range(Some(v), 10);
        assert_eq!(parse_range(None, 10), Ok(None));
        assert_eq!(r("bytes=0-0"), Ok(Some((0, 1))));
        assert_eq!(r("bytes=0-9"), Ok(Some((0, 10))));
        assert_eq!(r("bytes=9-9"), Ok(Some((9, 10))));
        assert_eq!(r("bytes=5-"), Ok(Some((5, 10))));
        assert_eq!(r("bytes=-3"), Ok(Some((7, 10))));
        assert_eq!(r("bytes=-30"), Ok(Some((0, 10))));
        // An end past the size is cut to it; a start at or past it is refused.
        assert_eq!(r("bytes=8-100"), Ok(Some((8, 10))));
        assert_eq!(r("bytes=10-12"), Err(()));
        assert_eq!(r("bytes=5-4"), Err(()));
        assert_eq!(r("bytes=x-1"), Err(()));
        assert_eq!(httpdate(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn w08_ac04_block_lists_keep_their_order() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><BlockList><Latest>QUFB</Latest><Uncommitted>QkJC</Uncommitted><Latest> Q0ND </Latest></BlockList>"#;
        assert_eq!(block_list(xml), vec!["QUFB", "QkJC", "Q0ND"]);
        assert!(block_list("<BlockList></BlockList>").is_empty());
    }

    #[test]
    fn w08_ac04_assembly_refuses_a_missing_block() -> Result<(), String> {
        let d = tempfile::tempdir().map_err(|e| e.to_string())?;
        std::fs::create_dir_all(d.path().join("blocks")).map_err(|e| e.to_string())?;
        for (id, body) in [("b", "BB"), ("a", "A")] {
            std::fs::write(d.path().join("blocks").join(hex::encode(id)), body).map_err(|e| e.to_string())?;
        }
        let out = d.path().join("out");
        assemble(d.path(), &["b".into(), "a".into(), "b".into()], &out, 100)?;
        assert_eq!(std::fs::read_to_string(&out).map_err(|e| e.to_string())?, "BBABB");
        assert!(assemble(d.path(), &["a".into(), "zzz".into()], &out, 100).is_err());
        assert!(assemble(d.path(), &["b".into(), "b".into()], &out, 3).is_err(), "over the limit");
        Ok(())
    }
}
