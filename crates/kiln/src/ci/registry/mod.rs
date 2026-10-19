//! kiln's own OCI registry (docs/ci.md W12-R01): the OCI Distribution Specification v1.1 at
//! `/v2/`, on the main origin (and on `[ci.registry] listen` when set).
//! - **Blobs** live in the content-addressed store (W08's `cas`), so a layer kiln already has (in a
//!   cache, an artifact, another repository) is stored once; a blob is pulled from a repository only
//!   once it was pushed (or mounted) there.
//! - **Uploads**: monolithic (`POST …?digest=`), chunked (`PATCH`, then `PUT …?digest=`, with
//!   `Content-Range` checked against what came before), cross-repository mounts.
//! - **Manifests** (images, indexes, artifacts) in SQLite, by digest and tag; their blobs (or, for
//!   an index, its manifests) must be there first. One with a `subject` is found through the
//!   referrers API.
//! - **Content management**: deleting manifests, tags and blobs; collecting what nothing uses.
//! - **Auth**: [`auth`].

pub mod auth;
pub mod mirror;
pub mod proxy;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use http::{Method, StatusCode, header};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use self::auth::{Access, Action, Credentials};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

const API_VERSION: &str = "registry/2.0";
/// The biggest manifest kiln takes (the spec suggests at least 4 MiB).
const MAX_MANIFEST: usize = 4 << 20;
/// Uploads untouched this long are dropped.
const UPLOAD_TTL: i64 = 24 * 3600;

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

/// An error in the registry's words (`{"errors": [{code, message, detail}]}`).
struct Error {
    status: StatusCode,
    code: &'static str,
    message: String,
}

fn err(status: StatusCode, code: &'static str, message: impl Into<String>) -> Error {
    Error { status, code, message: message.into() }
}

fn internal(e: anyhow::Error) -> Error {
    err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", format!("{e:#}"))
}

type Answer = Result<Response, Error>;

fn with(r: Response, name: &'static str, value: &str) -> Response {
    srv::with_header(r, http::HeaderName::from_static(name), value)
}

fn base(r: Response) -> Response {
    with(r, "docker-distribution-api-version", API_VERSION)
}

impl Error {
    fn response(self, hub: &Hub, challenge: Option<(&str, &[Action])>) -> Response {
        let www = auth::challenge(hub, challenge);
        self.answer(&www)
    }

    /// The error, with `www` as its challenge when it is a 401.
    fn answer(self, www: &str) -> Response {
        let body = json!({ "errors": [{ "code": self.code, "message": self.message, "detail": Value::Null }] });
        let mut r = base(srv::json(self.status, &body));
        if self.status == StatusCode::UNAUTHORIZED {
            r = srv::with_header(r, header::WWW_AUTHENTICATE, www);
        }
        r
    }
}

/// Whether a repository name is one the spec allows.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.split('/').all(|part| {
            let b = part.as_bytes();
            let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
            if b.is_empty() || !b.first().copied().is_some_and(alnum) || !b.last().copied().is_some_and(alnum) {
                return false;
            }
            // Separators: `.`, `_`, `__`, or runs of `-`, each between alphanumerics.
            let mut i = 0;
            while i < b.len() {
                let c = b.get(i).copied().unwrap_or(0);
                if alnum(c) {
                    i += 1;
                    continue;
                }
                let start = i;
                while b.get(i).is_some_and(|x| !alnum(*x)) {
                    i += 1;
                }
                let sep = part.get(start..i).unwrap_or_default();
                if !(sep == "." || sep == "_" || sep == "__" || sep.bytes().all(|x| x == b'-')) {
                    return false;
                }
            }
            true
        })
}

/// A digest the registry stores: `sha256:` and 64 hex digits; its hex.
fn sha256_of(digest: &str) -> Option<&str> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(hex)
}

fn digest_ok(digest: &str) -> Result<&str, Error> {
    match digest.split_once(':') {
        Some(("sha256", _)) => sha256_of(digest).ok_or_else(|| err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", format!("`{digest}` isn't a sha256 digest"))),
        Some((alg, _)) if !alg.is_empty() => Err(err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", format!("kiln's registry stores sha256 digests, not {alg}"))),
        _ => Err(err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", format!("`{digest}` isn't a digest"))),
    }
}

/// What a request is about: `(name, what)` from the path after `/v2/`.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Base,
    Catalog,
    Blob(&'a str, &'a str),
    Uploads(&'a str),
    Upload(&'a str, &'a str),
    Manifest(&'a str, &'a str),
    Tags(&'a str),
    Referrers(&'a str, &'a str),
}

fn route_of(rest: &str) -> Option<Route<'_>> {
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return Some(Route::Base);
    }
    if rest == "_catalog" {
        return Some(Route::Catalog);
    }
    let rest = rest.trim_end_matches('/');
    if let Some(name) = rest.strip_suffix("/tags/list") {
        return Some(Route::Tags(name));
    }
    if let Some(name) = rest.strip_suffix("/blobs/uploads") {
        return Some(Route::Uploads(name));
    }
    // The marker nearest the end decides: a repository may be called `blobs` or `manifests`.
    let at = |m: &'static str| rest.rfind(m).map(|i| (i, m));
    let (i, marker) = [at("/blobs/uploads/"), at("/blobs/"), at("/manifests/"), at("/referrers/")].into_iter().flatten().max_by_key(|(i, m)| (*i, m.len()))?;
    let (name, what) = (rest.get(..i)?, rest.get(i + marker.len()..)?);
    if name.is_empty() || what.is_empty() || what.contains('/') {
        return None;
    }
    Some(match marker {
        "/blobs/uploads/" => Route::Upload(name, what),
        "/blobs/" => Route::Blob(name, what),
        "/manifests/" => Route::Manifest(name, what),
        _ => Route::Referrers(name, what),
    })
}

/// Where an upload's bytes gather.
fn upload_path(hub: &Hub, id: &str) -> PathBuf {
    crate::ci::services::cas::staging(hub).join(format!("registry-{id}"))
}

/// Who a request is, and what it may do: its registry token's access, or what its PAT or job
/// token's caller may do with the name.
enum Who {
    Anonymous,
    Token(Access),
    Caller(crate::authz::Caller),
}

impl Who {
    fn may(&self, hub: &Hub, name: &str, action: Action) -> bool {
        match self {
            Who::Anonymous => auth::may(hub, None, name, &[action]).contains(&action),
            Who::Token(a) => a.allows(name, action) || (action == Action::Pull && auth::anonymous(hub, name)),
            Who::Caller(c) => auth::may(hub, Some(c), name, &[action]).contains(&action),
        }
    }

    fn name(&self) -> String {
        match self {
            Who::Anonymous => "anonymous".to_owned(),
            Who::Token(a) => a.sub.clone(),
            Who::Caller(c) => c.name.clone(),
        }
    }
}

/// The caller a secret is: the admin token's owner, a PAT's, an agent's or a job's.
async fn caller_of(hub: &Hub, secret: &str) -> Option<crate::authz::Caller> {
    if crate::util::ct_eq(hub.admin_token.as_bytes(), secret.as_bytes()) {
        return Some(crate::authz::Caller::owner());
    }
    crate::authz::gate::caller(hub, Some(secret), None).await
}

async fn who(hub: &Hub, req: &Request) -> Result<Who, Error> {
    match auth::credentials(hub, srv::header_str(req, &header::AUTHORIZATION)) {
        Credentials::None => Ok(Who::Anonymous),
        Credentials::Registry(a) => Ok(Who::Token(a)),
        Credentials::Secret(s) => caller_of(hub, &s).await.map(Who::Caller).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "those credentials aren't kiln's")),
        Credentials::Invalid => Err(err(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "the token is invalid or expired")),
    }
}

/// `GET /v2/token?service=kiln&scope=…`: a registry token for the scopes the caller may have.
pub async fn token(hub: &Arc<Hub>, req: Request) -> Response {
    let q: HashMap<String, Vec<String>> = req.uri().query().unwrap_or_default().split('&').filter_map(|kv| kv.split_once('=')).fold(HashMap::new(), |mut m, (k, v)| {
        m.entry(k.to_owned()).or_default().push(srv::decode(v));
        m
    });
    let scopes = q.get("scope").cloned().unwrap_or_default();
    let caller = match auth::credentials(hub, srv::header_str(&req, &header::AUTHORIZATION)) {
        Credentials::None => None,
        Credentials::Secret(s) => match caller_of(hub, &s).await {
            Some(c) => Some(c),
            None => return err(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "those credentials aren't kiln's").response(hub, None),
        },
        Credentials::Registry(_) | Credentials::Invalid => return err(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "ask for a token with a PAT or a job token").response(hub, None),
    };
    let access = auth::grant(hub, caller.as_ref(), &scopes);
    let t = auth::token(hub, &access);
    let issued = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default();
    base(srv::json(StatusCode::OK, &json!({ "token": t, "access_token": t, "expires_in": auth::TOKEN_SECS, "issued_at": issued })))
}

/// `/v2/…`: the registry. `rest` is the path after `/v2`.
pub async fn serve(hub: &Arc<Hub>, rest: &str, req: Request) -> Response {
    let Some(route) = route_of(rest) else { return err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such route").response(hub, None) };
    let needed = needs(&route, req.method());
    let who = match who(hub, &req).await {
        Ok(w) => w,
        Err(e) => return e.response(hub, needed.as_ref().map(|(n, a)| (*n, std::slice::from_ref(a)))),
    };
    if let Some((name, action)) = &needed {
        if !valid_name(name) {
            return err(StatusCode::BAD_REQUEST, "NAME_INVALID", format!("`{name}` isn't a repository name")).response(hub, None);
        }
        if !who.may(hub, name, *action) {
            let (status, code) = if matches!(who, Who::Anonymous) || matches!(&who, Who::Token(a) if a.sub == "anonymous") { (StatusCode::UNAUTHORIZED, "UNAUTHORIZED") } else { (StatusCode::FORBIDDEN, "DENIED") };
            let e = err(status, code, format!("{} may not {} {name}", who.name(), action_word(*action)));
            // A mount asks for the source's pull too, so the token the client gets covers both (as
            // distribution's challenge does); without it the mount is an upload.
            let from = query(&req).get("from").filter(|f| matches!(route, Route::Uploads(_)) && valid_name(f) && query(&req).contains_key("mount")).cloned();
            return match from {
                Some(from) => e.answer(&auth::challenge_for(hub, &[(name, &[Action::Pull, Action::Push]), (from.as_str(), &[Action::Pull])])),
                None => e.response(hub, Some((name, std::slice::from_ref(action)))),
            };
        }
    } else if matches!(route, Route::Base) && matches!(who, Who::Anonymous) {
        // `GET /v2/` with no credentials: where to get them (a client then asks for a token, an
        // anonymous one where pulls are open).
        return err(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "sign in with a token").response(hub, None);
    }
    // A mirror's name (W12-R02): what's pulled comes from the upstream first; nothing is pushed.
    if let Some((name, _)) = &needed
        && let Some((m, upstream_name)) = mirror::of(hub, name)
    {
        let pull = matches!(*req.method(), Method::GET | Method::HEAD);
        let r = match (&route, pull) {
            (Route::Manifest(_, reference), true) => match mirror::manifest(hub, m, name, upstream_name, reference).await {
                Ok(Some(f)) => get_manifest(hub, name, &f.digest, req.method() == Method::HEAD).await.map(|r| if f.stale { mirror::warn_stale(r, &m.upstream) } else { r }),
                Ok(None) => Err(err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", format!("{} has no {upstream_name}:{reference}", m.upstream))),
                Err(e) => Err(e),
            },
            (Route::Blob(_, digest), true) => match mirror::blob(hub, m, name, upstream_name, digest).await {
                Ok(true) => get_blob(hub, name, digest, &req).await,
                Ok(false) => Err(err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", format!("{} has no blob {digest} in {upstream_name}", m.upstream))),
                Err(e) => Err(e),
            },
            (Route::Tags(_) | Route::Referrers(..), true) => Err(err(StatusCode::NOT_FOUND, "UNSUPPORTED", format!("a mirror of {} serves manifests and blobs", m.upstream))),
            _ => Err(err(StatusCode::FORBIDDEN, "DENIED", format!("{name} is a mirror of {}: it takes no pushes", m.upstream))),
        };
        return r.unwrap_or_else(|e| e.response(hub, None));
    }
    let r = match route {
        Route::Base => Ok(base(srv::json(StatusCode::OK, &json!({})))),
        Route::Catalog => catalog(hub, &who, &req).await,
        Route::Blob(name, digest) => match *req.method() {
            Method::GET | Method::HEAD => get_blob(hub, name, digest, &req).await,
            Method::DELETE => delete_blob(hub, name, digest).await,
            _ => Err(unsupported()),
        },
        Route::Uploads(name) => match *req.method() {
            Method::POST => start_upload(hub, &who, name, req).await,
            _ => Err(unsupported()),
        },
        Route::Upload(name, id) => match *req.method() {
            Method::GET => upload_status(hub, name, id).await,
            Method::PATCH => patch_upload(hub, name, id, req).await,
            Method::PUT => finish_upload(hub, name, id, req).await,
            Method::DELETE => cancel_upload(hub, name, id).await,
            _ => Err(unsupported()),
        },
        Route::Manifest(name, reference) => match *req.method() {
            Method::GET | Method::HEAD => get_manifest(hub, name, reference, req.method() == Method::HEAD).await,
            Method::PUT => put_manifest(hub, &who, name, reference, req).await,
            Method::DELETE => delete_manifest(hub, name, reference).await,
            _ => Err(unsupported()),
        },
        Route::Tags(name) => tags(hub, name, &req).await,
        Route::Referrers(name, digest) => referrers(hub, name, digest, &req).await,
    };
    r.unwrap_or_else(|e| e.response(hub, None))
}

fn unsupported() -> Error {
    err(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED", "the registry doesn't do that")
}

fn action_word(a: Action) -> &'static str {
    match a {
        Action::Pull => "pull from",
        Action::Push => "push to",
        Action::Delete => "delete from",
    }
}

/// The name and action a request needs (none: the base and the catalog, which filter by name).
fn needs<'a>(route: &Route<'a>, method: &Method) -> Option<(&'a str, Action)> {
    let write = |m: &Method| if *m == Method::DELETE { Action::Delete } else { Action::Push };
    match route {
        Route::Base | Route::Catalog => None,
        Route::Blob(n, _) | Route::Manifest(n, _) => Some((n, if matches!(*method, Method::GET | Method::HEAD) { Action::Pull } else { write(method) })),
        Route::Uploads(n) | Route::Upload(n, _) => Some((n, Action::Push)),
        Route::Tags(n) | Route::Referrers(n, _) => Some((n, Action::Pull)),
    }
}

fn query(req: &Request) -> HashMap<String, String> {
    srv::query(req)
}

// ------------------------------------------------------------------ blobs

/// The blob's size, if it was pushed (or mounted) to `name`.
async fn linked(hub: &Hub, name: &str, digest: &str) -> Result<Option<u64>, Error> {
    let (n, d) = (name.to_owned(), digest.to_owned());
    let size: Option<i64> = hub.store.call(move |c| Ok(c.query_row("SELECT size FROM registry_blobs WHERE repo = ?1 AND digest = ?2", params![n, d], |r| r.get(0)).optional()?)).await.map_err(internal)?;
    Ok(size.map(|s| u64::try_from(s).unwrap_or(0)))
}

async fn link(hub: &Hub, name: &str, digest: &str, size: u64) -> Result<(), Error> {
    let (n, d, s) = (name.to_owned(), digest.to_owned(), i64::try_from(size).unwrap_or(i64::MAX));
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute("INSERT INTO registry_repos (name, created_at) VALUES (?1, ?2) ON CONFLICT (name) DO NOTHING", params![n, now])?;
            c.execute("INSERT INTO registry_blobs (repo, digest, size, created_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (repo, digest) DO NOTHING", params![n, d, s, now])?;
            Ok(())
        })
        .await
        .map_err(internal)
}

async fn get_blob(hub: &Hub, name: &str, digest: &str, req: &Request) -> Answer {
    let hex = digest_ok(digest)?;
    let Some(size) = linked(hub, name, digest).await? else { return Err(err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", format!("{name} has no blob {digest}"))) };
    let Some(obj) = crate::ci::services::cas::object(hub, hex).await.map_err(internal)? else {
        return Err(err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", format!("{digest}'s content is gone")));
    };
    let (status, start, end) = match crate::ci::services::blob::byte_range(req, size) {
        Ok(None) => (StatusCode::OK, 0, size),
        Ok(Some((s, e))) => (StatusCode::PARTIAL_CONTENT, s, e),
        Err(()) => return Ok(srv::with_header(base(srv::status(StatusCode::RANGE_NOT_SATISFIABLE)), header::CONTENT_RANGE, &format!("bytes */{size}"))),
    };
    let body = if req.method() == Method::HEAD { srv::empty() } else { srv::channel_body(crate::ci::services::cas::read(hub, &obj, start, end)) };
    let mut r = base(srv::response(status, "application/octet-stream", body));
    r = srv::with_header(r, header::CONTENT_LENGTH, &(end - start).to_string());
    r = with(r, "docker-content-digest", digest);
    r = srv::with_header(r, header::ACCEPT_RANGES, "bytes");
    if status == StatusCode::PARTIAL_CONTENT {
        r = srv::with_header(r, header::CONTENT_RANGE, &format!("bytes {start}-{}/{size}", end.saturating_sub(1)));
    }
    Ok(r)
}

async fn delete_blob(hub: &Hub, name: &str, digest: &str) -> Answer {
    digest_ok(digest)?;
    let (n, d) = (name.to_owned(), digest.to_owned());
    let gone = hub.store.call(move |c| Ok(c.execute("DELETE FROM registry_blobs WHERE repo = ?1 AND digest = ?2", params![n, d])?)).await.map_err(internal)?;
    if gone == 0 {
        return Err(err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", format!("{name} has no blob {digest}")));
    }
    Ok(base(srv::status(StatusCode::ACCEPTED)))
}

// ------------------------------------------------------------------ uploads

fn upload_answer(status: StatusCode, name: &str, id: &str, size: u64) -> Response {
    let mut r = base(srv::status(status));
    r = srv::with_header(r, header::LOCATION, &format!("/v2/{name}/blobs/uploads/{id}"));
    r = with(r, "docker-upload-uuid", id);
    // What has arrived: `0-<last>` (`0-0` before anything has).
    with(r, "range", &format!("0-{}", size.saturating_sub(1)))
}

async fn upload_size(hub: &Hub, name: &str, id: &str) -> Result<u64, Error> {
    let (n, i) = (name.to_owned(), id.to_owned());
    let size: Option<i64> = hub.store.call(move |c| Ok(c.query_row("SELECT size FROM registry_uploads WHERE id = ?1 AND repo = ?2", params![i, n], |r| r.get(0)).optional()?)).await.map_err(internal)?;
    size.map(|s| u64::try_from(s).unwrap_or(0)).ok_or_else(|| err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", format!("no upload {id} to {name}")))
}

/// `POST /v2/<name>/blobs/uploads/`: a mount (`?mount=&from=`), a monolithic upload (`?digest=`
/// with the blob as the body), or the start of one.
async fn start_upload(hub: &Hub, who: &Who, name: &str, req: Request) -> Answer {
    let q = query(&req);
    if let Some(digest) = q.get("mount") {
        digest_ok(digest)?;
        let from = q.get("from").cloned().unwrap_or_default();
        if !from.is_empty()
            && valid_name(&from)
            && who.may(hub, &from, Action::Pull)
            && let Some(size) = linked(hub, &from, digest).await?
        {
            link(hub, name, digest, size).await?;
            let mut r = base(srv::status(StatusCode::CREATED));
            r = srv::with_header(r, header::LOCATION, &format!("/v2/{name}/blobs/{digest}"));
            return Ok(with(r, "docker-content-digest", digest));
        }
        // Not mountable: an upload starts instead, as the spec says.
    }
    let id = crate::util::new_id();
    let (n, i) = (name.to_owned(), id.clone());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute("INSERT INTO registry_uploads (id, repo, size, created_at, updated_at) VALUES (?1, ?2, 0, ?3, ?3)", params![i, n, now])?;
            Ok(())
        })
        .await
        .map_err(internal)?;
    match q.get("digest").filter(|d| !d.is_empty()) {
        Some(digest) => {
            let digest = digest.clone();
            let size = append(hub, &id, 0, req).await?;
            commit_upload(hub, name, &id, &digest, size).await
        }
        None => Ok(upload_answer(StatusCode::ACCEPTED, name, &id, 0)),
    }
}

async fn upload_status(hub: &Hub, name: &str, id: &str) -> Answer {
    let size = upload_size(hub, name, id).await?;
    Ok(upload_answer(StatusCode::NO_CONTENT, name, id, size))
}

/// A `Content-Range` of an upload's chunk (`<start>-<end>`, inclusive).
fn content_range(req: &Request) -> Option<(u64, u64)> {
    let v = srv::header_str(req, &header::CONTENT_RANGE)?;
    let v = v.trim().trim_start_matches("bytes").trim().trim_start_matches('=');
    let (a, b) = v.split('/').next()?.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// `PATCH /v2/<name>/blobs/uploads/<id>`: the next chunk, which must start where the last ended.
async fn patch_upload(hub: &Hub, name: &str, id: &str, req: Request) -> Answer {
    let size = upload_size(hub, name, id).await?;
    if let Some((start, _)) = content_range(&req)
        && start != size
    {
        let r = err(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID", format!("the chunk starts at {start}; {size} bytes have arrived")).response(hub, None);
        return Ok(upload_answer_into(r, name, id, size));
    }
    let n = append(hub, id, size, req).await?;
    Ok(upload_answer(StatusCode::ACCEPTED, name, id, size + n))
}

fn upload_answer_into(r: Response, name: &str, id: &str, size: u64) -> Response {
    let r = srv::with_header(r, header::LOCATION, &format!("/v2/{name}/blobs/uploads/{id}"));
    with(with(r, "docker-upload-uuid", id), "range", &format!("0-{}", size.saturating_sub(1)))
}

/// `PUT /v2/<name>/blobs/uploads/<id>?digest=`: the last chunk (if any), then the blob checked
/// against its digest and kept.
async fn finish_upload(hub: &Hub, name: &str, id: &str, req: Request) -> Answer {
    let size = upload_size(hub, name, id).await?;
    let digest = query(&req).get("digest").cloned().filter(|d| !d.is_empty()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "`digest`: the blob's"))?;
    if let Some((start, _)) = content_range(&req)
        && start != size
    {
        return Err(err(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID", format!("the chunk starts at {start}; {size} bytes have arrived")));
    }
    let n = append(hub, id, size, req).await?;
    commit_upload(hub, name, id, &digest, size + n).await
}

async fn cancel_upload(hub: &Hub, name: &str, id: &str) -> Answer {
    upload_size(hub, name, id).await?;
    drop_upload(hub, id).await;
    Ok(base(srv::status(StatusCode::NO_CONTENT)))
}

async fn drop_upload(hub: &Hub, id: &str) {
    let i = id.to_owned();
    let _ = hub.store.call(move |c| Ok(c.execute("DELETE FROM registry_uploads WHERE id = ?1", [&i])?)).await;
    let p = upload_path(hub, id);
    let _ = crate::rt::blocking(move || std::fs::remove_file(p)).await;
}

/// Writes a request's body onto the upload's file at `at`: the bytes it held.
async fn append(hub: &Hub, id: &str, at: u64, req: Request) -> Result<u64, Error> {
    let limit = hub.cfg.ci.registry.max_blob_bytes();
    let path = upload_path(hub, id);
    let p = path.clone();
    let file = crate::rt::blocking(move || -> std::io::Result<std::fs::File> {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&p)
    })
    .await
    .map_err(internal)?
    .map_err(|e| internal(e.into()))?;
    let file = Arc::new(file);
    let mut body = req.into_body();
    let (mut off, mut batch, mut held) = (at, Vec::<Bytes>::new(), 0usize);
    loop {
        let next = srv::next_chunk(&mut body).await.map_err(|e| err(StatusCode::BAD_REQUEST, "BLOB_UPLOAD_INVALID", format!("the body: {e:#}")))?;
        let done = next.is_none();
        if let Some(c) = next {
            held += c.len();
            batch.push(c);
        }
        if held >= (1 << 20) || (done && held > 0) {
            let total = off.saturating_add(u64::try_from(held).unwrap_or(u64::MAX));
            if total > limit {
                return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "SIZE_INVALID", format!("a blob here is at most {limit} bytes")));
            }
            let (f, pieces, start) = (Arc::clone(&file), std::mem::take(&mut batch), off);
            crate::rt::blocking(move || -> std::io::Result<()> {
                let mut o = start;
                for piece in &pieces {
                    crate::os::write_all_at(&f, piece, o)?;
                    o = o.saturating_add(u64::try_from(piece.len()).unwrap_or(u64::MAX));
                }
                Ok(())
            })
            .await
            .map_err(internal)?
            .map_err(|e| internal(e.into()))?;
            off = total;
            held = 0;
        }
        if done {
            break;
        }
    }
    let (i, size) = (id.to_owned(), i64::try_from(off).unwrap_or(i64::MAX));
    hub.store.call(move |c| Ok(c.execute("UPDATE registry_uploads SET size = ?2, updated_at = ?3 WHERE id = ?1", params![i, size, crate::util::now_unix()])?)).await.map_err(internal)?;
    Ok(off - at)
}

/// The upload checked against `digest`, put into the store and linked to `name`.
async fn commit_upload(hub: &Hub, name: &str, id: &str, digest: &str, size: u64) -> Answer {
    let hex = match digest_ok(digest) {
        Ok(h) => h.to_owned(),
        Err(e) => {
            drop_upload(hub, id).await;
            return Err(e);
        }
    };
    let path = upload_path(hub, id);
    let p = path.clone();
    let _ = crate::rt::blocking(move || std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(p)).await;
    let obj = crate::ci::services::cas::put_file(hub, &path).await.map_err(internal)?;
    if obj.digest != hex || obj.size != size {
        drop_upload(hub, id).await;
        return Err(err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", format!("the upload is sha256:{}, not {digest}", obj.digest)));
    }
    link(hub, name, digest, size).await?;
    drop_upload(hub, id).await;
    let mut r = base(srv::status(StatusCode::CREATED));
    r = srv::with_header(r, header::LOCATION, &format!("/v2/{name}/blobs/{digest}"));
    Ok(with(r, "docker-content-digest", digest))
}

// ------------------------------------------------------------------ manifests

/// A stored manifest: its digest, media type and bytes.
struct Stored {
    digest: String,
    media_type: String,
    body: Vec<u8>,
}

async fn find_manifest(hub: &Hub, name: &str, reference: &str) -> Result<Option<Stored>, Error> {
    let (n, r) = (name.to_owned(), reference.to_owned());
    hub.store
        .call(move |c| {
            let digest: Option<String> = if r.contains(':') { Some(r.clone()) } else { c.query_row("SELECT digest FROM registry_tags WHERE repo = ?1 AND tag = ?2", params![n, r], |x| x.get(0)).optional()? };
            let Some(d) = digest else { return Ok(None) };
            Ok(c.query_row("SELECT digest, media_type, body FROM registry_manifests WHERE repo = ?1 AND digest = ?2", params![n, d], |x| Ok(Stored { digest: x.get(0)?, media_type: x.get(1)?, body: x.get(2)? })).optional()?)
        })
        .await
        .map_err(internal)
}

async fn get_manifest(hub: &Hub, name: &str, reference: &str, head: bool) -> Answer {
    if reference.contains(':') {
        digest_ok(reference)?;
    }
    let Some(m) = find_manifest(hub, name, reference).await? else { return Err(err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", format!("{name} has no manifest {reference}"))) };
    let len = m.body.len();
    let body = if head { srv::empty() } else { srv::full(m.body) };
    let mut r = base(srv::response(StatusCode::OK, &m.media_type, body));
    r = srv::with_header(r, header::CONTENT_LENGTH, &len.to_string());
    Ok(with(r, "docker-content-digest", &m.digest))
}

/// Tags are `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
fn valid_tag(t: &str) -> bool {
    let b = t.as_bytes();
    !b.is_empty() && b.len() <= 128 && b.first().is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c))
}

/// `PUT /v2/<name>/manifests/<reference>`: a manifest, by tag or digest. The blobs it refers to (an
/// index: its manifests) must be in the repository.
async fn put_manifest(hub: &Hub, who: &Who, name: &str, reference: &str, req: Request) -> Answer {
    let by_digest = reference.contains(':');
    if by_digest {
        digest_ok(reference)?;
    } else if !valid_tag(reference) {
        return Err(err(StatusCode::BAD_REQUEST, "TAG_INVALID", format!("`{reference}` isn't a tag")));
    }
    let content_type = srv::header_str(&req, &header::CONTENT_TYPE).map(|c| c.split(';').next().unwrap_or(c).trim().to_owned());
    let body = srv::body_bytes(req.into_body(), MAX_MANIFEST).await.map_err(|e| err(StatusCode::PAYLOAD_TOO_LARGE, "SIZE_INVALID", format!("a manifest here is at most {MAX_MANIFEST} bytes: {e:#}")))?;
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&body)));
    if by_digest && reference != digest {
        return Err(err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", format!("the manifest is {digest}, not {reference}")));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|e| err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", format!("not JSON: {e}")))?;
    let media_type = v.get("mediaType").and_then(Value::as_str).map(str::to_owned).or(content_type).unwrap_or_else(|| OCI_MANIFEST.to_owned());
    let index = media_type == OCI_INDEX || media_type == DOCKER_LIST || v.get("manifests").is_some_and(Value::is_array);
    // What it refers to must be here.
    if index {
        for m in v.get("manifests").and_then(Value::as_array).into_iter().flatten() {
            let d = m.get("digest").and_then(Value::as_str).unwrap_or_default();
            digest_ok(d).map_err(|_| err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", format!("an entry's digest `{d}` isn't one")))?;
            if find_manifest(hub, name, d).await?.is_none() {
                return Err(err(StatusCode::BAD_REQUEST, "MANIFEST_UNKNOWN", format!("the index names {d}, which {name} hasn't got")));
            }
        }
    } else {
        let mut blobs: Vec<String> = Vec::new();
        if let Some(c) = v.pointer("/config/digest").and_then(Value::as_str) {
            blobs.push(c.to_owned());
        }
        blobs.extend(v.get("layers").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("digest").and_then(Value::as_str)).map(str::to_owned));
        for d in &blobs {
            digest_ok(d).map_err(|_| err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", format!("a blob's digest `{d}` isn't one")))?;
            if linked(hub, name, d).await?.is_none() {
                return Err(err(StatusCode::BAD_REQUEST, "MANIFEST_BLOB_UNKNOWN", format!("the manifest names {d}, which {name} hasn't got")));
            }
        }
    }
    let subject = v.pointer("/subject/digest").and_then(Value::as_str).map(str::to_owned);
    // What a referrer is: its artifact type, else its config's media type (OCI 1.1).
    let artifact_type = v.get("artifactType").and_then(Value::as_str).map(str::to_owned).or_else(|| (!index).then(|| v.pointer("/config/mediaType").and_then(Value::as_str).map(str::to_owned)).flatten());
    let annotations = v.get("annotations").filter(|a| a.is_object()).map(Value::to_string);
    let (n, d, mt, at, sb, an, bytes, tag) = (name.to_owned(), digest.clone(), media_type, artifact_type, subject.clone(), annotations, body.to_vec(), (!by_digest).then(|| reference.to_owned()));
    let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let fresh_tag = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            tx.execute("INSERT INTO registry_repos (name, created_at) VALUES (?1, ?2) ON CONFLICT (name) DO NOTHING", params![n, now])?;
            tx.execute(
                "INSERT INTO registry_manifests (repo, digest, media_type, artifact_type, subject, annotations, body, size, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (repo, digest) DO NOTHING",
                params![n, d, mt, at, sb, an, bytes, size, now],
            )?;
            let mut fresh = None;
            if let Some(t) = &tag {
                let before: Option<String> = tx.query_row("SELECT digest FROM registry_tags WHERE repo = ?1 AND tag = ?2", params![n, t], |r| r.get(0)).optional()?;
                tx.execute(
                    "INSERT INTO registry_tags (repo, tag, digest, updated_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (repo, tag) DO UPDATE SET digest = excluded.digest, updated_at = excluded.updated_at",
                    params![n, t, d, now],
                )?;
                if before.as_deref() != Some(d.as_str()) {
                    fresh = Some(if before.is_some() { "updated" } else { "published" });
                }
            }
            tx.commit()?;
            Ok(fresh)
        })
        .await
        .map_err(internal)?;
    if let Some(action) = fresh_tag {
        published(hub, who, name, reference, &digest, action).await;
    }
    let mut r = base(srv::status(StatusCode::CREATED));
    r = srv::with_header(r, header::LOCATION, &format!("/v2/{name}/manifests/{digest}"));
    r = with(r, "docker-content-digest", &digest);
    if let Some(s) = subject {
        r = with(r, "oci-subject", &s);
    }
    Ok(r)
}

/// A tag pushed: the `registry_package` event (W12-R07), for kiln's own and its repository's
/// workflows.
async fn published(hub: &Hub, who: &Who, name: &str, tag: &str, digest: &str, action: &str) {
    let package = json!({
        "name": name, "package_type": "CONTAINER", "ecosystem": "CONTAINER", "registry": { "url": hub.cfg.server.public_url, "type": "kiln" },
        "package_version": { "version": digest, "container_metadata": { "tag": { "name": tag, "digest": digest } } },
    });
    hub.emit(&json!({ "type": "registry_package", "action": action, "name": name, "tag": tag, "digest": digest, "by": who.name() }));
    let scope = auth::scope_of(name);
    let crate::authz::Scope::Repo { repo } = scope else { return };
    let Ok(repos) = hub.store.repos().await else { return };
    if let Some(r) = repos.into_iter().find(|r| r.name.eq_ignore_ascii_case(&repo))
        && let Err(e) = crate::ci::events::registry_package(hub, &r, action, &package, &who.name()).await
    {
        tracing::debug!(repo = %r.name, error = %e.message, "registry_package: no run");
    }
}

async fn delete_manifest(hub: &Hub, name: &str, reference: &str) -> Answer {
    let (n, r) = (name.to_owned(), reference.to_owned());
    let by_digest = reference.contains(':');
    if by_digest {
        digest_ok(reference)?;
    }
    let gone = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let gone = if by_digest {
                tx.execute("DELETE FROM registry_tags WHERE repo = ?1 AND digest = ?2", params![n, r])?;
                tx.execute("DELETE FROM registry_manifests WHERE repo = ?1 AND digest = ?2", params![n, r])?
            } else {
                // A tag: only the tag goes; the manifest stays, by its digest.
                tx.execute("DELETE FROM registry_tags WHERE repo = ?1 AND tag = ?2", params![n, r])?
            };
            tx.commit()?;
            Ok(gone)
        })
        .await
        .map_err(internal)?;
    if gone == 0 {
        return Err(err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", format!("{name} has no manifest {reference}")));
    }
    Ok(base(srv::status(StatusCode::ACCEPTED)))
}

// ------------------------------------------------------------------ discovery

/// A page of a sorted list: `n` after `last`, and whether there is more.
fn page(all: Vec<String>, q: &HashMap<String, String>) -> Result<(Vec<String>, Option<String>), Error> {
    let n = match q.get("n").filter(|n| !n.is_empty()) {
        Some(n) => Some(n.parse::<usize>().map_err(|_| err(StatusCode::BAD_REQUEST, "PAGINATION_NUMBER_INVALID", format!("`n={n}`")))?),
        None => None,
    };
    let last = q.get("last").cloned().unwrap_or_default();
    let rest: Vec<String> = all.into_iter().filter(|t| last.is_empty() || *t > last).collect();
    match n {
        Some(n) if rest.len() > n => {
            let shown: Vec<String> = rest.into_iter().take(n).collect();
            let next = shown.last().cloned();
            Ok((shown, next))
        }
        _ => Ok((rest, None)),
    }
}

async fn tags(hub: &Hub, name: &str, req: &Request) -> Answer {
    let n = name.to_owned();
    let (known, mut all): (bool, Vec<String>) = hub
        .store
        .call(move |c| {
            let known = c.query_row("SELECT 1 FROM registry_repos WHERE name = ?1", [&n], |_| Ok(())).optional()?.is_some();
            let mut q = c.prepare("SELECT tag FROM registry_tags WHERE repo = ?1")?;
            let rows = q.query_map([&n], |r| r.get::<_, String>(0))?;
            Ok((known, rows.collect::<Result<Vec<_>, _>>()?))
        })
        .await
        .map_err(internal)?;
    if !known {
        return Err(err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", format!("no repository {name}")));
    }
    all.sort();
    let q = query(req);
    let (shown, next) = page(all, &q)?;
    let mut r = base(srv::json(StatusCode::OK, &json!({ "name": name, "tags": shown })));
    if let (Some(last), Some(n)) = (next, q.get("n")) {
        r = srv::with_header(r, header::LINK, &format!("</v2/{name}/tags/list?n={n}&last={}>; rel=\"next\"", crate::ci::cli::enc(&last)));
    }
    Ok(r)
}

async fn catalog(hub: &Hub, who: &Who, req: &Request) -> Answer {
    let mut all: Vec<String> = hub
        .store
        .call(|c| {
            let mut q = c.prepare("SELECT name FROM registry_repos")?;
            let rows = q.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    all.retain(|n| who.may(hub, n, Action::Pull));
    all.sort();
    let q = query(req);
    let (shown, next) = page(all, &q)?;
    let mut r = base(srv::json(StatusCode::OK, &json!({ "repositories": shown })));
    if let (Some(last), Some(n)) = (next, q.get("n")) {
        r = srv::with_header(r, header::LINK, &format!("</v2/_catalog?n={n}&last={}>; rel=\"next\"", crate::ci::cli::enc(&last)));
    }
    Ok(r)
}

/// `GET /v2/<name>/referrers/<digest>?artifactType=`: the manifests whose subject is `digest`, as an
/// image index of their descriptors.
async fn referrers(hub: &Hub, name: &str, digest: &str, req: &Request) -> Answer {
    digest_ok(digest)?;
    let q = query(req);
    let want = q.get("artifactType").cloned().filter(|t| !t.is_empty());
    let (n, d) = (name.to_owned(), digest.to_owned());
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, String, Option<String>, i64, Option<String>)> = hub
        .store
        .call(move |c| {
            let mut s = c.prepare("SELECT digest, media_type, artifact_type, size, annotations FROM registry_manifests WHERE repo = ?1 AND subject = ?2 ORDER BY created_at, digest")?;
            let rows = s.query_map(params![n, d], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let manifests: Vec<Value> = rows
        .into_iter()
        .filter(|(_, _, at, ..)| want.as_ref().is_none_or(|w| at.as_deref() == Some(w.as_str())))
        .map(|(digest, media_type, artifact_type, size, annotations)| {
            let mut d = json!({ "mediaType": media_type, "digest": digest, "size": size });
            if let Some(o) = d.as_object_mut() {
                if let Some(t) = artifact_type {
                    o.insert("artifactType".into(), json!(t));
                }
                if let Some(a) = annotations.and_then(|a| serde_json::from_str::<Value>(&a).ok()) {
                    o.insert("annotations".into(), a);
                }
            }
            d
        })
        .collect();
    let body = json!({ "schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": manifests });
    let mut r = base(srv::response(StatusCode::OK, OCI_INDEX, srv::full(body.to_string())));
    if want.is_some() {
        r = with(r, "oci-filters-applied", "artifactType");
    }
    Ok(r)
}

// ------------------------------------------------------------------ collection

/// What a collection removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Collected {
    pub uploads: usize,
    pub blobs: usize,
}

/// Drops uploads left a day, and each repository's blobs that none of its manifests use (and were
/// pushed more than an hour ago, so an image being pushed keeps its layers); the store's own
/// collection then frees their content once nothing else uses it.
pub async fn collect(hub: &Hub) -> anyhow::Result<Collected> {
    let now = crate::util::now_unix();
    let (stale, blobs): (Vec<String>, usize) = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let stale: Vec<String> = {
                let mut q = tx.prepare("SELECT id FROM registry_uploads WHERE updated_at < ?1")?;
                let rows = q.query_map([now - UPLOAD_TTL], |r| r.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            tx.execute("DELETE FROM registry_uploads WHERE updated_at < ?1", [now - UPLOAD_TTL])?;
            // Every digest each repository's manifests name.
            let mut used: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
            {
                let mut q = tx.prepare("SELECT repo, body FROM registry_manifests")?;
                let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
                for row in rows {
                    let (repo, body) = row?;
                    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let set = used.entry(repo).or_default();
                    if let Some(d) = v.pointer("/config/digest").and_then(Value::as_str) {
                        set.insert(d.to_owned());
                    }
                    for l in v.get("layers").and_then(Value::as_array).into_iter().flatten() {
                        if let Some(d) = l.get("digest").and_then(Value::as_str) {
                            set.insert(d.to_owned());
                        }
                    }
                }
            }
            let linked: Vec<(String, String)> = {
                let mut q = tx.prepare("SELECT repo, digest FROM registry_blobs WHERE created_at < ?1")?;
                let rows = q.query_map([now - 3600], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            let mut gone = 0;
            for (repo, digest) in linked {
                if !used.get(&repo).is_some_and(|s| s.contains(&digest)) {
                    gone += tx.execute("DELETE FROM registry_blobs WHERE repo = ?1 AND digest = ?2", params![repo, digest])?;
                }
            }
            tx.commit()?;
            Ok((stale, gone))
        })
        .await?;
    for id in &stale {
        let p = upload_path(hub, id);
        let _ = crate::rt::blocking(move || std::fs::remove_file(p)).await;
    }
    Ok(Collected { uploads: stale.len(), blobs })
}

/// Collects every hour, for as long as the server runs.
pub async fn collect_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        if !hub.shutdown.sleep(std::time::Duration::from_secs(3600)).await {
            return Ok(());
        }
        match collect(&hub).await {
            Ok(c) if c.uploads + c.blobs > 0 => tracing::info!(uploads = c.uploads, blobs = c.blobs, "the registry collected"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "the registry's collection"),
        }
    }
}

/// Whether a path is the registry's (`/v2`, `/v2/…`).
pub fn is_registry(path: &str) -> bool {
    path == "/v2" || path.starts_with("/v2/")
}

/// The registry alone, for `[ci.registry] listen`, or one mirror at the root of its own address
/// (`prefix`: its names are the upstream's, as Docker's `registry-mirrors` sends them).
pub struct Only {
    pub hub: Arc<Hub>,
    pub prefix: Option<String>,
}

impl srv::Handler for Only {
    async fn handle(self: Arc<Self>, mut req: Request) -> Response {
        let path = req.uri().path().to_owned();
        if !is_registry(&path) {
            return err(StatusCode::NOT_FOUND, "UNSUPPORTED", "only the registry is here (`/v2/`)").response(&self.hub, None);
        }
        if let Some(prefix) = &self.prefix
            && let Some(rest) = path.strip_prefix("/v2/").filter(|r| !r.is_empty() && *r != "token" && !r.starts_with("token?"))
        {
            let q = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
            if let Ok(uri) = format!("/v2/{prefix}/{rest}{q}").parse() {
                *req.uri_mut() = uri;
            }
        }
        handle(&self.hub, req).await
    }
}

/// The registry's answer to `path` (the whole path, `/v2/…`).
pub async fn handle(hub: &Arc<Hub>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    if path == "/v2/token" || path == "/v2/token/" {
        return token(hub, req).await;
    }
    let rest = path.strip_prefix("/v2").unwrap_or_default().to_owned();
    serve(hub, &rest, req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w12_r01_names_routes_digests() {
        for good in ["a", "acme/app", "acme/app-web", "a/b/c", "a.b_c__d--e", "x0/y1"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "A", "acme/", "/acme", "acme//app", "-a", "a-", "a___b", "a..b", "a/B"] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(route_of(""), Some(Route::Base));
        assert_eq!(route_of("/"), Some(Route::Base));
        assert_eq!(route_of("/_catalog"), Some(Route::Catalog));
        assert_eq!(route_of("/acme/app/web/blobs/sha256:ab"), Some(Route::Blob("acme/app/web", "sha256:ab")));
        assert_eq!(route_of("/acme/app/blobs/uploads/"), Some(Route::Uploads("acme/app")));
        assert_eq!(route_of("/acme/app/blobs/uploads/u1"), Some(Route::Upload("acme/app", "u1")));
        assert_eq!(route_of("/acme/app/manifests/latest"), Some(Route::Manifest("acme/app", "latest")));
        assert_eq!(route_of("/acme/app/tags/list"), Some(Route::Tags("acme/app")));
        assert_eq!(route_of("/acme/app/referrers/sha256:ab"), Some(Route::Referrers("acme/app", "sha256:ab")));
        // A repository may be called `blobs`: the last marker decides.
        assert_eq!(route_of("/acme/blobs/manifests/v1"), Some(Route::Manifest("acme/blobs", "v1")));
        assert!(sha256_of(&format!("sha256:{}", "a".repeat(64))).is_some());
        assert!(sha256_of("sha256:abc").is_none() && sha256_of(&format!("sha512:{}", "a".repeat(64))).is_none());
        assert!(valid_tag("v1.2.3") && valid_tag("_x") && !valid_tag("-x") && !valid_tag(&"a".repeat(129)));
    }

    #[test]
    fn w12_r01_pages() {
        let all: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| (*s).to_owned()).collect();
        let q = |n: &str, last: &str| [("n".to_owned(), n.to_owned()), ("last".to_owned(), last.to_owned())].into_iter().filter(|(_, v)| !v.is_empty()).collect::<HashMap<_, _>>();
        let (shown, next) = page(all.clone(), &q("2", "")).unwrap_or_default();
        assert_eq!((shown, next.as_deref()), (vec!["a".to_owned(), "b".to_owned()], Some("b")));
        let (shown, next) = page(all.clone(), &q("2", "b")).unwrap_or_default();
        assert_eq!((shown, next), (vec!["c".to_owned(), "d".to_owned()], None));
        let (shown, _) = page(all, &q("", "c")).unwrap_or_default();
        assert_eq!(shown, vec!["d".to_owned()]);
    }
}
