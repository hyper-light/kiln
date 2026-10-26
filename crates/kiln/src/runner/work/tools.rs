//! The tools' own cache protocols on the runner's cache service (docs/ci.md W24-R08, AC08), so
//! each tool's existing remote-cache client works against the runner unchanged. Each keeps its
//! entries in a namespace of its own, under the token's scope; a token rides as each tool sends
//! one (a bearer token, or Basic's password).
//!
//! | Tool | Point it at | Requests |
//! |---|---|---|
//! | sccache | `SCCACHE_WEBDAV_ENDPOINT=<url>/sccache`, `SCCACHE_WEBDAV_TOKEN` | WebDAV as sccache's backend (OpenDAL's) speaks it: `GET`, `PUT`, `HEAD`, `PROPFIND` (depth 0), `MKCOL`, `DELETE`; folders are implied |
//! | Bazel, Buck2 | `--remote_cache=<url>/bazel`, `--remote_header=Authorization=Bearer <token>` | `GET`, `HEAD`, `PUT` of `/ac/<sha256>` and `/cas/<sha256>` (a `/cas/` upload is checked against its name) |
//! | Gradle | `buildCache { remote<HttpBuildCache> { url = "<url>/gradle/"; credentials { … } } }` | `GET`, `HEAD`, `PUT` of `/<key>`, Basic auth |
//! | ccache | `remote_storage = http://kiln:<token>@<host:port>/ccache` (Basic auth, which every ccache with HTTP storage sends) | `GET`, `HEAD`, `PUT`, `DELETE` of `/<key path>` (any layout) |
//! | Turborepo | `TURBO_API=<url>`, `TURBO_TOKEN`, `TURBO_TEAM` | Vercel's remote cache API v8: `/v8/artifacts/status`, `GET`, `HEAD`, `PUT` of `/v8/artifacts/<hash>` (its duration and tag kept), `POST /v8/artifacts` (a query) and `/v8/artifacts/events` |
//! | Go | `GOCACHEPROG="kiln gocacheprog"` | `kiln gocacheprog` (`super::gocacheprog`) over kiln's own API |

use std::collections::BTreeMap;
use std::sync::Arc;

use http::{HeaderName, Method, StatusCode, header};
use serde_json::{Value, json};

use super::service::{Service, msg, unauthorized};
use super::store::is_digest;
use super::token::Claims;
use crate::http::server::{self as srv, Request, Response};

/// What sccache's WebDAV backend asks a folder or a file for (RFC 4918 §9.1): `allprop`, answered
/// with what OpenDAL reads (`getlastmodified` always, `getcontentlength` for a file).
const DAV_NS: &str = "DAV:";

/// A tool's request, routed by its first segment; `None` when it's no tool's path.
pub(crate) async fn route(svc: &Arc<Service>, req: Request, segs: &[&str], folder: bool) -> Option<anyhow::Result<Response>> {
    Some(match segs {
        ["sccache", rest @ ..] => webdav(svc, req, "sccache", "/sccache", rest, folder).await,
        ["bazel", rest @ ..] => bazel(svc, req, rest).await,
        [kind @ ("ac" | "cas"), hash] => bazel(svc, req, &[*kind, *hash]).await,
        ["gradle", rest @ ..] => named(svc, req, "gradle", rest).await,
        ["ccache", rest @ ..] => named(svc, req, "ccache", rest).await,
        ["v8", rest @ ..] | ["turbo", "v8", rest @ ..] => turbo(svc, req, rest).await,
        _ => return None,
    })
}

fn may_read(c: &Claims) -> Option<Response> {
    c.reads().is_empty().then(|| msg(StatusCode::FORBIDDEN, "this token doesn't read"))
}

fn may_write(c: &Claims) -> Option<Response> {
    c.writes().is_none().then(|| msg(StatusCode::FORBIDDEN, "this token doesn't write"))
}

/// A plain HTTP cache: `GET`, `HEAD`, `PUT` and `DELETE` of a name (Gradle's, ccache's).
async fn named(svc: &Arc<Service>, req: Request, ns: &str, rest: &[&str]) -> anyhow::Result<Response> {
    let c = match svc.claims(&req) {
        Ok(c) => c,
        Err(why) => return Ok(unauthorized(why)),
    };
    let name = rest.join("/");
    if name.is_empty() {
        return Ok(msg(StatusCode::NOT_FOUND, "an entry is named after the cache's path"));
    }
    entry_request(svc, &c, req, ns, &name, BTreeMap::new()).await
}

/// `GET`, `HEAD`, `PUT` or `DELETE` of the entry `name` of `ns`; a `PUT` keeps `meta` with it.
async fn entry_request(svc: &Arc<Service>, c: &Claims, req: Request, ns: &str, name: &str, meta: BTreeMap<String, String>) -> anyhow::Result<Response> {
    let method = req.method().clone();
    match method {
        Method::GET | Method::HEAD => {
            if let Some(r) = may_read(c) {
                return Ok(r);
            }
            let Some((_, e)) = svc.find_entry(c, ns, name).await? else { return Ok(msg(StatusCode::NOT_FOUND, "no such entry")) };
            Ok(svc.entry_response(&e, method == Method::HEAD).await?.unwrap_or_else(|| msg(StatusCode::NOT_FOUND, "no such entry")))
        }
        Method::PUT => {
            if let Some(r) = may_write(c) {
                return Ok(r);
            }
            let (digest, size) = match svc.take_body(req.into_body(), None).await? {
                Ok(k) => k,
                Err(r) => return Ok(r),
            };
            svc.set(c, ns, name, &digest, size, meta).await?;
            Ok(srv::status(StatusCode::CREATED))
        }
        Method::DELETE => {
            let Some(scope) = c.writes() else { return Ok(msg(StatusCode::FORBIDDEN, "this token doesn't write")) };
            let gone = svc.store().remove_entry(&scope, ns, name).await?;
            Ok(srv::status(if gone { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }))
        }
        _ => Ok(srv::with_header(msg(StatusCode::METHOD_NOT_ALLOWED, "not a method of this cache"), header::ALLOW, "GET, HEAD, PUT, DELETE")),
    }
}

// --------------------------------------------------------------------------------------- WebDAV

/// WebDAV over the entries of `ns`, as sccache's backend speaks it: a file is an entry named by
/// its path; folders are implied (every folder exists, so a client's `MKCOL` of a file's parents
/// stops at its first look).
async fn webdav(svc: &Arc<Service>, req: Request, ns: &str, base: &str, rest: &[&str], folder: bool) -> anyhow::Result<Response> {
    let method = req.method().as_str().to_owned();
    if method == "OPTIONS" {
        let r = srv::with_header(srv::status(StatusCode::OK), HeaderName::from_static("dav"), "1");
        return Ok(srv::with_header(r, header::ALLOW, "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, MKCOL"));
    }
    let c = match svc.claims(&req) {
        Ok(c) => c,
        Err(why) => return Ok(unauthorized(why)),
    };
    let path = rest.join("/");
    match method.as_str() {
        "MKCOL" => Ok(match may_write(&c) {
            Some(r) => r,
            None => srv::status(StatusCode::CREATED),
        }),
        "PROPFIND" => {
            if let Some(r) = may_read(&c) {
                return Ok(r);
            }
            let href = if path.is_empty() { format!("{base}/") } else if folder { format!("{base}/{path}/") } else { format!("{base}/{path}") };
            if folder || path.is_empty() {
                return Ok(multistatus(&href, None));
            }
            match svc.find_entry(&c, ns, &path).await? {
                Some((_, e)) => Ok(multistatus(&href, Some((e.size, e.at, &e.digest)))),
                None => Ok(msg(StatusCode::NOT_FOUND, "no such file")),
            }
        }
        _ if folder && matches!(method.as_str(), "GET" | "HEAD") => Ok(msg(StatusCode::NOT_FOUND, "a folder has no content")),
        _ if path.is_empty() => Ok(msg(StatusCode::METHOD_NOT_ALLOWED, "the root is a folder")),
        _ => entry_request(svc, &c, req, ns, &path, BTreeMap::new()).await,
    }
}

/// A PROPFIND's answer (207) for one resource: a folder (`file` none), or a file's size, time and
/// digest.
fn multistatus(href: &str, file: Option<(u64, i64, &str)>) -> Response {
    let (props, kind) = match file {
        Some((size, at, digest)) => (
            format!(
                "<D:getcontentlength>{size}</D:getcontentlength><D:getlastmodified>{}</D:getlastmodified><D:getetag>\"{digest}\"</D:getetag><D:getcontenttype>application/octet-stream</D:getcontenttype>",
                super::service::http_date(at)
            ),
            "<D:resourcetype/>",
        ),
        None => (format!("<D:getlastmodified>{}</D:getlastmodified>", super::service::http_date(crate::util::now_unix())), "<D:resourcetype><D:collection/></D:resourcetype>"),
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"{DAV_NS}\"><D:response><D:href>{}</D:href><D:propstat><D:prop>{props}{kind}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
        xml_escape(href)
    );
    let mut r = srv::response(StatusCode::MULTI_STATUS, "application/xml; charset=utf-8", srv::full(body));
    r = srv::with_header(r, HeaderName::from_static("dav"), "1");
    r
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ---------------------------------------------------------------------------------------- Bazel

/// Bazel's HTTP cache: `/ac/<sha256>` (action results, kept as named) and `/cas/<sha256>`
/// (content, named by its digest and checked against it).
async fn bazel(svc: &Arc<Service>, req: Request, rest: &[&str]) -> anyhow::Result<Response> {
    let c = match svc.claims(&req) {
        Ok(c) => c,
        Err(why) => return Ok(unauthorized(why)),
    };
    let (kind, hash) = match rest {
        [kind @ ("ac" | "cas"), hash] if is_digest(&hash.to_ascii_lowercase()) => (*kind, hash.to_ascii_lowercase()),
        _ => return Ok(msg(StatusCode::NOT_FOUND, "Bazel's cache is /ac/<sha256> and /cas/<sha256>")),
    };
    if kind == "ac" {
        return entry_request(svc, &c, req, "bazel-ac", &hash, BTreeMap::new()).await;
    }
    let method = req.method().clone();
    match method {
        Method::GET | Method::HEAD => {
            if let Some(r) = may_read(&c) {
                return Ok(r);
            }
            if svc.find_object(&c, &hash).await?.is_none() {
                return Ok(msg(StatusCode::NOT_FOUND, "no such blob"));
            }
            Ok(svc.object_response(&hash, method == Method::HEAD, &[]).await?.unwrap_or_else(|| msg(StatusCode::NOT_FOUND, "no such blob")))
        }
        Method::PUT => {
            if let Some(r) = may_write(&c) {
                return Ok(r);
            }
            let (digest, size) = match svc.take_body(req.into_body(), Some(&hash)).await? {
                Ok(k) => k,
                Err(r) => return Ok(r),
            };
            svc.hold(&c, &digest, size)?;
            Ok(srv::status(StatusCode::CREATED))
        }
        _ => Ok(srv::with_header(msg(StatusCode::METHOD_NOT_ALLOWED, "not a method of Bazel's cache"), header::ALLOW, "GET, HEAD, PUT")),
    }
}

// ------------------------------------------------------------------------------------ Turborepo

/// The headers of an artifact Turborepo sends and wants back.
const TURBO_KEPT: &[&str] = &["x-artifact-duration", "x-artifact-tag"];

/// Vercel's remote cache API (v8), as Turborepo calls it.
async fn turbo(svc: &Arc<Service>, req: Request, rest: &[&str]) -> anyhow::Result<Response> {
    let c = match svc.claims(&req) {
        Ok(c) => c,
        Err(why) => return Ok(unauthorized(why)),
    };
    let method = req.method().clone();
    match (&method, rest) {
        (&Method::GET, ["artifacts", "status"]) => Ok(srv::json(StatusCode::OK, &json!({ "status": if c.reads().is_empty() && c.writes().is_none() { "disabled" } else { "enabled" } }))),
        (&Method::POST, ["artifacts", "events"]) => Ok(srv::json(StatusCode::OK, &json!({}))),
        (&Method::POST, ["artifacts"]) => {
            if let Some(r) = may_read(&c) {
                return Ok(r);
            }
            let v: Value = srv::body_json(req.into_body(), 1 << 20).await.unwrap_or(Value::Null);
            let mut out = serde_json::Map::new();
            for h in v.get("hashes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                let found = svc.find_entry(&c, "turbo", h).await?;
                let info = match found {
                    Some((_, e)) => {
                        let duration = e.meta.get("x-artifact-duration").and_then(|d| d.parse::<u64>().ok()).unwrap_or(0);
                        let mut o = json!({ "size": e.size, "taskDurationMs": duration });
                        if let (Some(tag), Some(m)) = (e.meta.get("x-artifact-tag"), o.as_object_mut()) {
                            m.insert("tag".into(), json!(tag));
                        }
                        o
                    }
                    None => json!({ "error": { "message": "Artifact not found" } }),
                };
                out.insert(h.to_owned(), info);
            }
            Ok(srv::json(StatusCode::OK, &Value::Object(out)))
        }
        (&Method::PUT, ["artifacts", hash]) => {
            let meta: BTreeMap<String, String> = TURBO_KEPT.iter().filter_map(|k| Some(((*k).to_owned(), srv::header_str(&req, &HeaderName::from_static(k))?.to_owned()))).collect();
            if let Some(r) = may_write(&c) {
                return Ok(r);
            }
            let (digest, size) = match svc.take_body(req.into_body(), None).await? {
                Ok(k) => k,
                Err(r) => return Ok(r),
            };
            svc.set(&c, "turbo", hash, &digest, size, meta).await?;
            Ok(srv::json(StatusCode::ACCEPTED, &json!({ "urls": [hash] })))
        }
        (&Method::GET | &Method::HEAD, ["artifacts", hash]) => entry_request(svc, &c, req, "turbo", hash, BTreeMap::new()).await,
        _ => Ok(msg(StatusCode::NOT_FOUND, "not a path of the remote cache API")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::service::tests::{access, call, conn, on_thread, serve};
    use super::*;
    use crate::runner::work::client::{Conn, Reply};

    fn with(c: &mut Conn, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> anyhow::Result<Reply> {
        c.request(method, path, headers, body)
    }

    /// W24-AC08 (sccache, recorded): the WebDAV requests sccache 0.10's backend makes, in order,
    /// from its storage check through a compile's miss, store and hit.
    #[test]
    fn w24_ac08_sccache_webdav_as_sccache_speaks_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let t = svc.mint(&access("acme/app", "trusted", true, true))?;
            let other = svc.mint(&access("acme/other", "trusted", true, true))?;
            on_thread(move || -> anyhow::Result<()> {
                let mut c = conn(at);
                let bearer = format!("Bearer {t}");
                let auth = [("authorization", bearer.as_str())];
                let propfind = [("authorization", bearer.as_str()), ("depth", "0"), ("content-type", "application/xml")];
                let allprop = br#"<?xml version="1.0" encoding="utf-8" ?><D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#;
                // The storage check: read, then make the root's folder, then write.
                anyhow::ensure!(with(&mut c, "GET", "/sccache/.sccache_check", &auth, b"")?.status == 404);
                let r = with(&mut c, "PROPFIND", "/sccache/", &propfind, allprop)?;
                let xml = String::from_utf8_lossy(&r.body).into_owned();
                anyhow::ensure!(r.status == 207 && xml.contains("<D:collection/>") && xml.contains("<D:getlastmodified>") && xml.contains("HTTP/1.1 200 OK"), "{r:?}");
                anyhow::ensure!(with(&mut c, "PUT", "/sccache/.sccache_check", &auth, b"Hello, World!")?.status == 201);
                // A compile: a miss, its parents looked at (implied), its entry put, then a hit.
                let key = "a/b/c/abc0123456789abcdef";
                anyhow::ensure!(with(&mut c, "GET", &format!("/sccache/{key}"), &auth, b"")?.status == 404);
                anyhow::ensure!(with(&mut c, "PROPFIND", "/sccache/a/b/c/", &propfind, allprop)?.status == 207);
                anyhow::ensure!(with(&mut c, "MKCOL", "/sccache/a/b/c/", &auth, b"")?.status == 201);
                anyhow::ensure!(with(&mut c, "PUT", &format!("/sccache/{key}"), &auth, b"compiled object")?.status == 201);
                let hit = with(&mut c, "GET", &format!("/sccache/{key}"), &auth, b"")?;
                anyhow::ensure!(hit.status == 200 && hit.body == b"compiled object", "{hit:?}");
                // A file's PROPFIND: its size and time, as OpenDAL's stat reads them.
                let r = with(&mut c, "PROPFIND", &format!("/sccache/{key}"), &propfind, allprop)?;
                let xml = String::from_utf8_lossy(&r.body).into_owned();
                anyhow::ensure!(r.status == 207 && xml.contains("<D:getcontentlength>15</D:getcontentlength>") && xml.contains("<D:resourcetype/>") && xml.contains(" GMT</D:getlastmodified>"), "{xml}");
                anyhow::ensure!(with(&mut c, "PROPFIND", "/sccache/a/b/c/missing", &propfind, allprop)?.status == 404);
                // Another repository sees none of it; Basic's password carries the token too.
                anyhow::ensure!(call(&mut c, "GET", &format!("/sccache/{key}"), Some(&other), b"")?.status == 404);
                use base64::Engine as _;
                let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("kiln:{t}")));
                anyhow::ensure!(with(&mut c, "GET", &format!("/sccache/{key}"), &[("authorization", basic.as_str())], b"")?.status == 200);
                anyhow::ensure!(with(&mut c, "DELETE", &format!("/sccache/{key}"), &auth, b"")?.status == 204);
                anyhow::ensure!(with(&mut c, "GET", &format!("/sccache/{key}"), &auth, b"")?.status == 404);
                let o = with(&mut c, "OPTIONS", "/sccache/", &[], b"")?;
                anyhow::ensure!(o.header("dav") == Some("1"), "{o:?}");
                Ok(())
            })
            .await??;
            Ok(())
        })
    }

    /// W24-AC08 (Bazel, recorded): an action's miss, its outputs and result uploaded, then a hit
    /// for both; a blob that isn't what its name says is refused.
    #[test]
    fn w24_ac08_bazel_http_cache_as_bazel_speaks_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let t = svc.mint(&access("acme/app", "trusted", true, true))?;
            on_thread(move || -> anyhow::Result<()> {
                let mut c = conn(at);
                let action = crate::util::sha256_hex(b"an action");
                let output = b"an output file".to_vec();
                let out_digest = crate::util::sha256_hex(&output);
                let result = b"\x0a\x22an ActionResult".to_vec();
                anyhow::ensure!(call(&mut c, "GET", &format!("/bazel/ac/{action}"), Some(&t), b"")?.status == 404);
                anyhow::ensure!(call(&mut c, "PUT", &format!("/bazel/cas/{out_digest}"), Some(&t), &output)?.status == 201);
                anyhow::ensure!(call(&mut c, "PUT", &format!("/bazel/ac/{action}"), Some(&t), &result)?.status == 201);
                let r = call(&mut c, "GET", &format!("/bazel/ac/{action}"), Some(&t), b"")?;
                anyhow::ensure!(r.status == 200 && r.body == result, "{r:?}");
                let r = call(&mut c, "GET", &format!("/bazel/cas/{out_digest}"), Some(&t), b"")?;
                anyhow::ensure!(r.status == 200 && r.body == output, "{r:?}");
                anyhow::ensure!(call(&mut c, "HEAD", &format!("/bazel/cas/{out_digest}"), Some(&t), b"")?.status == 200);
                // The bare paths too, for a client given the service's root.
                anyhow::ensure!(call(&mut c, "GET", &format!("/cas/{out_digest}"), Some(&t), b"")?.status == 200);
                anyhow::ensure!(call(&mut c, "PUT", &format!("/bazel/cas/{}", crate::util::sha256_hex(b"other")), Some(&t), b"poison")?.status == 400);
                anyhow::ensure!(call(&mut c, "GET", "/bazel/ac/not-a-digest", Some(&t), b"")?.status == 404);
                Ok(())
            })
            .await??;
            Ok(())
        })
    }

    /// W24-AC08 (Gradle and ccache, recorded): Gradle's GET and PUT with Basic auth, challenged
    /// without it; ccache's subdirs layout, its existence check before a put, and a removal.
    #[test]
    fn w24_ac08_gradle_and_ccache_http_caches() -> anyhow::Result<()> {
        use base64::Engine as _;
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let t = svc.mint(&access("acme/app", "trusted", true, true))?;
            on_thread(move || -> anyhow::Result<()> {
                let mut c = conn(at);
                let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("gradle:{t}")));
                let gradle = [("authorization", basic.as_str()), ("content-type", "application/vnd.gradle.build-cache-artifact.v2"), ("x-gradle-version", "8.14")];
                let key = "/gradle/1f4c3a1d7e5b9f2a8c6d4e0b3a7f9c2d";
                let r = with(&mut c, "GET", key, &[], b"")?;
                anyhow::ensure!(r.status == 401 && r.header("www-authenticate").is_some_and(|v| v.starts_with("Basic")), "{r:?}");
                anyhow::ensure!(with(&mut c, "GET", key, &gradle, b"")?.status == 404);
                anyhow::ensure!(with(&mut c, "PUT", key, &gradle, b"a task's outputs")?.status == 201);
                let r = with(&mut c, "GET", key, &gradle, b"")?;
                anyhow::ensure!(r.status == 200 && r.body == b"a task's outputs", "{r:?}");
                // ccache: `<url>/<2 chars>/<rest>`; a HEAD first unless it overwrites.
                let bearer = format!("Bearer {t}");
                let auth = [("authorization", bearer.as_str())];
                let entry = "/ccache/5q/pc8u8ud7tcqbdkkj0b3tmb5ppl7gj7k";
                anyhow::ensure!(with(&mut c, "GET", entry, &auth, b"")?.status == 404);
                anyhow::ensure!(with(&mut c, "HEAD", entry, &auth, b"")?.status == 404);
                anyhow::ensure!(with(&mut c, "PUT", entry, &[("authorization", bearer.as_str()), ("content-type", "application/octet-stream")], b"cache entry")?.status == 201);
                let h = with(&mut c, "HEAD", entry, &auth, b"")?;
                anyhow::ensure!(h.status == 200 && h.header("content-length") == Some("11"), "{h:?}");
                anyhow::ensure!(with(&mut c, "GET", entry, &auth, b"")?.body == b"cache entry");
                anyhow::ensure!(with(&mut c, "DELETE", entry, &auth, b"")?.status == 204);
                anyhow::ensure!(with(&mut c, "GET", entry, &auth, b"")?.status == 404);
                // Its bazel layout is a path like any other.
                let ac = format!("/ccache/ac/{}", "0".repeat(64));
                anyhow::ensure!(with(&mut c, "PUT", &ac, &auth, b"x")?.status == 201 && with(&mut c, "GET", &ac, &auth, b"")?.status == 200);
                Ok(())
            })
            .await??;
            Ok(())
        })
    }

    /// W24-AC08 (Turborepo, recorded): the remote cache API v8 as turbo calls it; an artifact's
    /// duration and signature tag come back with it.
    #[test]
    fn w24_ac08_turborepo_remote_cache_api() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let t = svc.mint(&access("acme/app", "trusted", true, true))?;
            on_thread(move || -> anyhow::Result<()> {
                let mut c = conn(at);
                let r = call(&mut c, "GET", "/v8/artifacts/status", Some(&t), b"")?;
                anyhow::ensure!(r.status == 200 && serde_json::from_slice::<Value>(&r.body)?.get("status") == Some(&json!("enabled")), "{r:?}");
                let hash = "c0ffee1234567890";
                let path = format!("/v8/artifacts/{hash}?slug=kiln");
                anyhow::ensure!(call(&mut c, "GET", &path, Some(&t), b"")?.status == 404);
                anyhow::ensure!(call(&mut c, "HEAD", &path, Some(&t), b"")?.status == 404);
                let bearer = format!("Bearer {t}");
                let put = [("authorization", bearer.as_str()), ("content-type", "application/octet-stream"), ("x-artifact-duration", "1234"), ("x-artifact-tag", "c2lnbmF0dXJl"), ("x-artifact-client-ci", "kiln")];
                let r = with(&mut c, "PUT", &path, &put, b"tar.zst bytes")?;
                anyhow::ensure!(r.status == 202, "{r:?}");
                let r = call(&mut c, "GET", &path, Some(&t), b"")?;
                anyhow::ensure!(r.status == 200 && r.body == b"tar.zst bytes" && r.header("x-artifact-duration") == Some("1234") && r.header("x-artifact-tag") == Some("c2lnbmF0dXJl"), "{r:?}");
                let h = call(&mut c, "HEAD", &path, Some(&t), b"")?;
                anyhow::ensure!(h.status == 200 && h.header("x-artifact-duration") == Some("1234"), "{h:?}");
                let q = call(&mut c, "POST", "/v8/artifacts", Some(&t), br#"{"hashes":["c0ffee1234567890","nothere"]}"#)?;
                let v: Value = serde_json::from_slice(&q.body)?;
                anyhow::ensure!(v.pointer("/c0ffee1234567890/taskDurationMs") == Some(&json!(1234)) && v.pointer("/nothere/error").is_some(), "{v}");
                anyhow::ensure!(call(&mut c, "POST", "/v8/artifacts/events", Some(&t), br#"[{"sessionId":"s","source":"REMOTE","event":"HIT","hash":"c0ffee1234567890","duration":1234}]"#)?.status == 200);
                anyhow::ensure!(call(&mut c, "GET", "/turbo/v8/artifacts/c0ffee1234567890", Some(&t), b"")?.status == 200, "under a prefix too");
                anyhow::ensure!(call(&mut c, "GET", &path, None, b"")?.status == 401);
                Ok(())
            })
            .await??;
            Ok(())
        })
    }
}
