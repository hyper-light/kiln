//! GitHub's cache service in the protocol GitHub Enterprise Server speaks (docs/ci.md W08-R06,
//! W08-R07): `ACTIONS_CACHE_URL` + `_apis/artifactcache/…`. The toolkit uses it against any server
//! that isn't github.com, so it is what current `actions/cache` and every setup action's cache
//! talk to on kiln.
//!
//! - `GET cache?keys=&version=`: a restore. 200 with the entry and a signed download URL, or 204.
//! - `POST caches`: reserve a save. 200 `{cacheId}`, or 409 when the key exists.
//! - `PATCH caches/<id>`: a part of the archive, at its `Content-Range`. Parts come in parallel.
//! - `POST caches/<id>`: commit. The archive is chunked into the store and the entry is ready.
//!
//! What a leg may do comes from its token: the scopes it restores from, in order, and the one
//! it saves to (W08-R14). A refusal starts with GitHub's "cache read denied:" or "cache write
//! denied:", so the toolkit turns it into a warning instead of failing the step.

use std::path::PathBuf;
use std::sync::Arc;

use http::{Method, StatusCode, header};
use kiln_ci::store::cache::{self, Entry};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;

use super::blob::{self, Target};
use super::cas;
use crate::ci::token::Claims;
use crate::ci::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// A reservation with no upload activity for this long is abandoned, and its key free again.
const ABANDONED_SECS: i64 = 3600;
/// The most a single part may carry (the toolkit sends 32 MiB, at most 128 MiB).
const MAX_PART: u64 = 256 << 20;
/// How long a restore's download URL works.
const DOWNLOAD_TTL: i64 = 6 * 3600;

fn msg(status: StatusCode, message: &str) -> Response {
    srv::json(status, &serde_json::json!({ "message": message }))
}

/// `/_apis/artifactcache/<rest>`.
pub async fn route(hub: &Arc<Hub>, rest: &[&str], req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else {
        return Ok(msg(StatusCode::UNAUTHORIZED, "a leg's ACTIONS_RUNTIME_TOKEN is needed"));
    };
    let method = req.method().clone();
    match (method, rest) {
        (Method::GET, ["cache"]) => lookup(hub, &claims, &req).await,
        (Method::GET, ["caches"]) => list(hub, &claims, &req).await,
        (Method::POST, ["caches"]) => reserve(hub, &claims, req).await,
        (Method::PATCH, ["caches", id]) => part(hub, &claims, id, req).await,
        (Method::POST, ["caches", id]) => commit(hub, &claims, id, req).await,
        _ => Ok(msg(StatusCode::NOT_FOUND, "not found")),
    }
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

async fn entries(hub: &Hub, repo: &str) -> anyhow::Result<Vec<Entry>> {
    let repo = repo.to_owned();
    hub.store
        .call(move |c| {
            let mut q = c.prepare("SELECT id, scope, key, version, COALESCE(size, 0), created_at, COALESCE(last_used_at, created_at), object IS NOT NULL FROM ci_caches WHERE repo_id = ?1")?;
            let rows = q.query_map([&repo], |r| {
                Ok(Entry {
                    id: r.get(0)?,
                    scope: r.get(1)?,
                    key: r.get(2)?,
                    version: r.get(3)?,
                    size: u64::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
                    created_at: r.get(5)?,
                    last_used_at: r.get(6)?,
                    ready: r.get(7)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

fn entry_json(e: &Entry, location: Option<String>) -> serde_json::Value {
    let mut v = serde_json::json!({ "cacheKey": e.key, "scope": e.scope, "cacheVersion": e.version, "creationTime": rfc3339(e.created_at) });
    if let (Some(l), Some(o)) = (location, v.as_object_mut()) {
        o.insert("archiveLocation".into(), serde_json::json!(l));
    }
    v
}

async fn lookup(hub: &Arc<Hub>, claims: &Claims, req: &Request) -> Result<Response, Fail> {
    let q = srv::query(req);
    let keys: Vec<String> = q.get("keys").map(|k| k.split(',').map(str::to_owned).collect()).unwrap_or_default();
    let version = q.get("version").cloned().unwrap_or_default();
    if let Err(e) = cache::check_keys(&keys) {
        return Ok(msg(StatusCode::BAD_REQUEST, &e));
    }
    let scopes = claims.read_scopes();
    if scopes.is_empty() {
        return Ok(msg(StatusCode::FORBIDDEN, &format!("cache read denied: this job's cache-mode is `{}`", claims.cm)));
    }
    let all = entries(hub, &claims.repo).await.map_err(internal)?;
    let Some(hit) = cache::lookup(&all, &keys, &version, &scopes) else {
        crate::ci::insights::cache_event(hub, claims, keys.first().map_or("", String::as_str), "miss", None);
        return Ok(srv::status(StatusCode::NO_CONTENT));
    };
    crate::ci::insights::cache_event(hub, claims, &hit.key, "hit", Some(hit.size));
    let id = hit.id;
    hub.store.call(move |c| Ok(c.execute("UPDATE ci_caches SET last_used_at = ?2 WHERE id = ?1", params![id, crate::util::now_unix()])?)).await.map_err(internal)?;
    let location = blob::url(hub, &blob::origin(req), Target::Cache(id), "r", DOWNLOAD_TTL);
    Ok(srv::json(StatusCode::OK, &entry_json(hit, Some(location))))
}

/// `GET caches?key=`: what the toolkit lists for its debug output after a miss.
async fn list(hub: &Arc<Hub>, claims: &Claims, req: &Request) -> Result<Response, Fail> {
    let key = srv::query(req).get("key").cloned().unwrap_or_default().to_lowercase();
    let scopes = claims.read_scopes();
    let all = entries(hub, &claims.repo).await.map_err(internal)?;
    let found: Vec<serde_json::Value> = all.iter().filter(|e| e.ready && scopes.contains(&e.scope) && e.key.to_lowercase() == key).map(|e| entry_json(e, None)).collect();
    Ok(srv::json(StatusCode::OK, &serde_json::json!({ "totalCount": found.len(), "artifactCaches": found })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reserve {
    key: String,
    version: String,
    #[serde(default)]
    cache_size: Option<u64>,
}

async fn reserve(hub: &Arc<Hub>, claims: &Claims, req: Request) -> Result<Response, Fail> {
    let r: Reserve = srv::body_json(req.into_body(), 1 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let Some(scope) = claims.write_scope() else {
        return Ok(msg(StatusCode::FORBIDDEN, &format!("cache write denied: this job's cache-mode is `{}`", claims.cm)));
    };
    if let Err(e) = cache::check_keys(std::slice::from_ref(&r.key)) {
        return Ok(msg(StatusCode::BAD_REQUEST, &e));
    }
    let max = hub.cfg.ci.cache.max_entry_gb.saturating_mul(1 << 30);
    if let Some(n) = r.cache_size.filter(|n| *n > max) {
        return Ok(msg(StatusCode::BAD_REQUEST, &format!("Cache size of ~{} MB ({n} B) is over the {} GB limit, not saving cache.", n >> 20, hub.cfg.ci.cache.max_entry_gb)));
    }
    let (repo, leg) = (claims.repo.clone(), claims.sub.clone());
    let abandoned = drop_abandoned(hub, &repo).await.map_err(internal)?;
    for id in abandoned {
        let dir = cas::staging(hub).join(Target::Cache(id).id());
        let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
    }
    let all = entries(hub, &repo).await.map_err(internal)?;
    if !cache::may_reserve(&all, &scope, &r.key, &r.version) {
        return Ok(msg(StatusCode::CONFLICT, &format!("Cache already exists. Scope: {scope}, Key: {}, Version: {}", r.key, r.version)));
    }
    let (key, version) = (r.key.clone(), r.version.clone());
    let id = hub
        .store
        .call(move |c| {
            c.execute(
                "INSERT INTO ci_caches (repo_id, scope, key, version, created_at, last_used_at, created_by_leg) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
                params![repo, scope, key, version, crate::util::now_unix(), leg],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await;
    match id {
        Ok(id) => Ok(srv::json(StatusCode::OK, &serde_json::json!({ "cacheId": id }))),
        // Another leg reserved the same key a moment ago.
        Err(_) => Ok(msg(StatusCode::CONFLICT, &format!("Cache already exists. Key: {}, Version: {}", r.key, r.version))),
    }
}

/// Removes this repository's reservations that stopped uploading; returns their ids.
async fn drop_abandoned(hub: &Hub, repo: &str) -> anyhow::Result<Vec<i64>> {
    let repo = repo.to_owned();
    hub.store
        .call(move |c| {
            let before = crate::util::now_unix() - ABANDONED_SECS;
            let ids: Vec<i64> = {
                let mut q = c.prepare("SELECT id FROM ci_caches WHERE repo_id = ?1 AND object IS NULL AND COALESCE(last_used_at, created_at) < ?2")?;
                let rows = q.query_map(params![repo, before], |r| r.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for id in &ids {
                c.execute("DELETE FROM ci_caches WHERE id = ?1", [id])?;
            }
            Ok(ids)
        })
        .await
}

/// A reservation of this leg's that is still uploading.
async fn uploading(hub: &Hub, claims: &Claims, id: &str) -> Result<Option<i64>, Fail> {
    let Ok(id) = id.parse::<i64>() else { return Ok(None) };
    let leg = claims.sub.clone();
    hub.store
        .call(move |c| Ok(c.query_row("SELECT id FROM ci_caches WHERE id = ?1 AND created_by_leg = ?2 AND object IS NULL", params![id, leg], |r| r.get::<_, i64>(0)).optional()?))
        .await
        .map_err(internal)
}

fn upload_path(hub: &Hub, id: i64) -> PathBuf {
    cas::staging(hub).join(Target::Cache(id).id()).join("upload")
}

/// `Content-Range: bytes <start>-<end>/*` → (start, length).
fn content_range(req: &Request) -> Option<(u64, u64)> {
    let v = req.headers().get(header::CONTENT_RANGE)?.to_str().ok()?;
    let spec = v.trim().strip_prefix("bytes")?.trim();
    let (range, _) = spec.split_once('/').unwrap_or((spec, "*"));
    let (a, b) = range.split_once('-')?;
    let (start, end): (u64, u64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (end >= start).then_some((start, end - start + 1))
}

async fn part(hub: &Arc<Hub>, claims: &Claims, id: &str, req: Request) -> Result<Response, Fail> {
    let Some(id) = uploading(hub, claims, id).await? else { return Ok(msg(StatusCode::NOT_FOUND, "no such cache upload for this job")) };
    let Some((start, len)) = content_range(&req) else { return Ok(msg(StatusCode::BAD_REQUEST, "Content-Range: bytes <start>-<end>/* is required")) };
    let max = hub.cfg.ci.cache.max_entry_gb.saturating_mul(1 << 30);
    if len > MAX_PART || start.saturating_add(len) > max {
        return Ok(msg(StatusCode::BAD_REQUEST, "the part is too large"));
    }
    let path = upload_path(hub, id);
    match write_at(req, &path, start, len).await {
        Ok(()) => {}
        Err(e) => return Ok(msg(StatusCode::BAD_REQUEST, &format!("{e:#}"))),
    }
    hub.store.call(move |c| Ok(c.execute("UPDATE ci_caches SET last_used_at = ?2 WHERE id = ?1", params![id, crate::util::now_unix()])?)).await.map_err(internal)?;
    Ok(srv::status(StatusCode::OK))
}

/// Writes a request body of exactly `len` bytes at `offset` in `path` (created if missing).
async fn write_at(req: Request, path: &std::path::Path, offset: u64, len: u64) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        let d = dir.to_path_buf();
        crate::rt::blocking(move || std::fs::create_dir_all(d)).await??;
    }
    let p = path.to_path_buf();
    let file = Arc::new(crate::rt::blocking(move || std::fs::File::options().create(true).write(true).truncate(false).open(p)).await??);
    let mut body = req.into_body();
    let (mut at, mut buf) = (offset, Vec::with_capacity(1 << 20));
    let end = offset.saturating_add(len);
    loop {
        let next = srv::next_chunk(&mut body).await?;
        if let Some(c) = &next {
            buf.extend_from_slice(c);
        }
        if buf.len() >= 1 << 20 || (next.is_none() && !buf.is_empty()) {
            let n = u64::try_from(buf.len()).unwrap_or(u64::MAX);
            anyhow::ensure!(at.saturating_add(n) <= end, "the part is longer than its Content-Range");
            let (f, data, off) = (Arc::clone(&file), std::mem::take(&mut buf), at);
            crate::rt::blocking(move || crate::os::write_all_at(&f, &data, off)).await??;
            at += n;
        }
        if next.is_none() {
            break;
        }
    }
    anyhow::ensure!(at == end, "the part has {} bytes; its Content-Range says {len}", at - offset);
    Ok(())
}

#[derive(Deserialize)]
struct Commit {
    size: u64,
}

async fn commit(hub: &Arc<Hub>, claims: &Claims, id: &str, req: Request) -> Result<Response, Fail> {
    let Some(id) = uploading(hub, claims, id).await? else { return Ok(msg(StatusCode::NOT_FOUND, "no such cache upload for this job")) };
    let c: Commit = srv::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let path = upload_path(hub, id);
    let p = path.clone();
    let have = crate::rt::blocking(move || std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).await.map_err(internal)?;
    if have != c.size {
        return Ok(msg(StatusCode::BAD_REQUEST, &format!("The uploaded cache is {have} bytes; the commit says {}.", c.size)));
    }
    let obj = cas::put_file(hub, &path).await.map_err(internal)?;
    let (digest, size) = (obj.digest, i64::try_from(obj.size).unwrap_or(i64::MAX));
    let key: Option<String> = hub
        .store
        .call(move |c| {
            c.execute("UPDATE ci_caches SET object = ?2, size = ?3, last_used_at = ?4 WHERE id = ?1 AND object IS NULL", params![id, digest, size, crate::util::now_unix()])?;
            Ok(c.query_row("SELECT key FROM ci_caches WHERE id = ?1", [id], |r| r.get(0)).optional()?)
        })
        .await
        .map_err(internal)?;
    crate::ci::insights::cache_event(hub, claims, key.as_deref().unwrap_or(""), "save", Some(obj.size));
    if let Some(dir) = path.parent().map(std::path::Path::to_path_buf) {
        let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
    }
    // Over the repository's quota, the least recently used entries go now.
    let h = Arc::clone(hub);
    let repo = claims.repo.clone();
    crate::rt::spawn(async move {
        if let Err(e) = evict(&h, Some(&repo)).await {
            tracing::warn!(error = %format!("{e:#}"), "cache eviction failed");
        }
    });
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// Applies eviction (W08-R06) to one repository's entries, or every repository's; returns how
/// many entries went.
pub async fn evict(hub: &Hub, repo: Option<&str>) -> anyhow::Result<usize> {
    let repos: Vec<String> = match repo {
        Some(r) => vec![r.to_owned()],
        None => hub.store.call(|c| {
            let mut q = c.prepare("SELECT DISTINCT repo_id FROM ci_caches")?;
            let rows = q.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?,
    };
    let quota = hub.cfg.ci.cache.repo_quota_gb.saturating_mul(1 << 30);
    let idle = i64::try_from(hub.cfg.ci.cache.max_idle_days.saturating_mul(86_400)).unwrap_or(i64::MAX);
    let mut gone = 0;
    for r in repos {
        let all = entries(hub, &r).await?;
        let ids = cache::evict(&all, crate::util::now_unix(), idle, quota);
        gone += ids.len();
        // W19-R06: what's left, against the quota (`quota.near`).
        let used: u64 = all.iter().filter(|e| !ids.contains(&e.id)).map(|e| e.size).sum();
        crate::ci::notify::cache_use(hub, r.clone(), used, quota).await;
        hub.store
            .call(move |c| {
                let tx = c.transaction()?;
                for id in &ids {
                    tx.execute("DELETE FROM ci_caches WHERE id = ?1", [id])?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
    }
    Ok(gone)
}
