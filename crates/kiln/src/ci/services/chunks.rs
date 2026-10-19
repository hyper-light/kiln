//! kiln's own cache protocol (docs/ci.md W08-R09), for `kiln/cache@v1`: entries are the store's
//! objects (`cas`), moved chunk by chunk, so a runner that keeps the chunks it has seen (its local
//! tier) downloads only those it lacks and uploads only new ones. Chunks are content-defined
//! (`kiln_ci::store::cdc`, the same cuts on both sides): a `target/` that changed by 1 % moves
//! about 1 % of its bytes. Entries follow GitHub's rules (`kiln_ci::store::cache`: keys, versions,
//! the leg's scopes and cache mode), plus a scope of the repository's own (`repo`) that every ref
//! reads and the repository's trusted runs write.
//!
//! - `POST /_kiln/cache/lookup` `{ keys, version, scope }`: the entry, its chunk list, or 204.
//! - `POST /_kiln/cache/missing` `{ chunks }`: those the repository's entries don't hold (what
//!   other repositories stored is never said: no probing for content through dedup).
//! - `PUT /_kiln/cache/chunks/<sha256>`: a chunk, checked against its digest.
//! - `GET /_kiln/cache/entries/<id>/chunks/<sha256>`: a chunk of an entry the leg may read.
//! - `POST /_kiln/cache/entries` `{ key, version, scope, object }`: an entry of stored chunks,
//!   its whole content's SHA-256 checked before it's kept.

use std::sync::Arc;

use http::{Method, StatusCode};
use kiln_ci::store::cache::{self, Entry};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::cas;
use crate::ci::token::Claims;
use crate::ci::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// The repository-wide scope (`scope: repo`).
const REPO_SCOPE: &str = "repo";

fn msg(status: StatusCode, message: &str) -> Response {
    srv::json(status, &json!({ "message": message }))
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// `/_kiln/cache/<rest>`, for a leg's token.
pub async fn route(hub: &Arc<Hub>, rest: &[&str], req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else {
        return Ok(msg(StatusCode::UNAUTHORIZED, "a leg's ACTIONS_RUNTIME_TOKEN is needed"));
    };
    let method = req.method().clone();
    match (method, rest) {
        (Method::POST, ["lookup"]) => lookup(hub, &claims, req).await,
        (Method::POST, ["missing"]) => missing(hub, &claims, req).await,
        (Method::PUT, ["chunks", digest]) => put_chunk(hub, &claims, digest, req).await,
        (Method::GET, ["entries", id, "chunks", digest]) => get_chunk(hub, &claims, id, digest).await,
        (Method::POST, ["entries"]) => commit(hub, &claims, req).await,
        _ => Ok(msg(StatusCode::NOT_FOUND, "not found")),
    }
}

/// The scopes a leg reads, in order: its own (GitHub's), then the repository's when asked.
fn read_scopes(claims: &Claims, scope: &str) -> Vec<String> {
    let mut s = claims.read_scopes();
    if scope == REPO_SCOPE && !s.is_empty() {
        s.push(REPO_SCOPE.to_owned());
    }
    s
}

/// The scope a leg saves to: its ref's (GitHub's), or the repository's; none when its cache mode
/// doesn't save.
fn write_scope(claims: &Claims, scope: &str) -> Option<String> {
    let own = claims.write_scope()?;
    Some(if scope == REPO_SCOPE { REPO_SCOPE.to_owned() } else { own })
}

async fn entries(hub: &Hub, repo: &str) -> anyhow::Result<Vec<(Entry, Option<String>)>> {
    let repo = repo.to_owned();
    hub.store
        .call(move |c| {
            let mut q = c.prepare("SELECT id, scope, key, version, COALESCE(size, 0), created_at, COALESCE(last_used_at, created_at), object FROM ci_caches WHERE repo_id = ?1")?;
            let rows = q.query_map([&repo], |r| {
                let object: Option<String> = r.get(7)?;
                Ok((
                    Entry {
                        id: r.get(0)?,
                        scope: r.get(1)?,
                        key: r.get(2)?,
                        version: r.get(3)?,
                        size: u64::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
                        created_at: r.get(5)?,
                        last_used_at: r.get(6)?,
                        ready: object.is_some(),
                    },
                    object,
                ))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

#[derive(Deserialize)]
struct Lookup {
    keys: Vec<String>,
    version: String,
    #[serde(default)]
    scope: String,
}

async fn lookup(hub: &Arc<Hub>, claims: &Claims, req: Request) -> Result<Response, Fail> {
    let l: Lookup = srv::body_json(req.into_body(), 1 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if let Err(e) = cache::check_keys(&l.keys) {
        return Ok(msg(StatusCode::BAD_REQUEST, &e));
    }
    let scopes = read_scopes(claims, &l.scope);
    if scopes.is_empty() {
        return Ok(msg(StatusCode::FORBIDDEN, &format!("cache read denied: this job's cache-mode is `{}`", claims.cm)));
    }
    let all = entries(hub, &claims.repo).await.map_err(internal)?;
    let plain: Vec<Entry> = all.iter().map(|(e, _)| e.clone()).collect();
    let Some(hit) = cache::lookup(&plain, &l.keys, &l.version, &scopes).cloned() else {
        crate::ci::insights::cache_event(hub, claims, l.keys.first().map_or("", String::as_str), "miss", None);
        return Ok(srv::status(StatusCode::NO_CONTENT));
    };
    let Some(digest) = all.iter().find(|(e, _)| e.id == hit.id).and_then(|(_, o)| o.clone()) else { return Ok(srv::status(StatusCode::NO_CONTENT)) };
    let Some(obj) = cas::object(hub, &digest).await.map_err(internal)? else { return Ok(srv::status(StatusCode::NO_CONTENT)) };
    crate::ci::insights::cache_event(hub, claims, &hit.key, "hit", Some(hit.size));
    let id = hit.id;
    hub.store.call(move |c| Ok(c.execute("UPDATE ci_caches SET last_used_at = ?2 WHERE id = ?1", params![id, crate::util::now_unix()])?)).await.map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "id": hit.id, "key": hit.key, "scope": hit.scope, "object": { "digest": obj.digest, "size": obj.size, "chunks": obj.chunks } })))
}

#[derive(Deserialize)]
struct Missing {
    chunks: Vec<String>,
}

/// Which of `chunks` to send: all but those the repository's own entries hold and the store
/// still has. A chunk only another repository stored is asked for too (it's kept once anyway),
/// so dedup never says what others hold.
async fn missing(hub: &Arc<Hub>, claims: &Claims, req: Request) -> Result<Response, Fail> {
    let m: Missing = srv::body_json(req.into_body(), 16 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if claims.write_scope().is_none() {
        return Ok(msg(StatusCode::FORBIDDEN, &format!("cache write denied: this job's cache-mode is `{}`", claims.cm)));
    }
    let (repo, asked) = (claims.repo.clone(), m.chunks.clone());
    let held: std::collections::HashSet<String> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT 1 FROM ci_cache_chunks WHERE repo_id = ?1 AND digest = ?2")?;
            let mut held = std::collections::HashSet::new();
            for d in &asked {
                if q.exists(params![repo, d])? {
                    held.insert(d.clone());
                }
            }
            Ok(held)
        })
        .await
        .map_err(internal)?;
    let h = Arc::clone(hub);
    let need: Vec<String> = crate::rt::blocking(move || m.chunks.into_iter().filter(|d| !(held.contains(d) && cas::chunk_file(&h, d).is_some())).collect()).await.map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "missing": need })))
}

async fn put_chunk(hub: &Arc<Hub>, claims: &Claims, digest: &str, req: Request) -> Result<Response, Fail> {
    if claims.write_scope().is_none() {
        return Ok(msg(StatusCode::FORBIDDEN, &format!("cache write denied: this job's cache-mode is `{}`", claims.cm)));
    }
    let data = srv::body_bytes(req.into_body(), kiln_ci::store::cdc::MAX).await.map_err(|e| (StatusCode::PAYLOAD_TOO_LARGE, format!("{e:#}")))?;
    let (h, d) = (Arc::clone(hub), digest.to_owned());
    let written = crate::rt::blocking(move || cas::put_chunk(&h, &d, &data)).await.map_err(internal)?;
    match written {
        Ok(n) => Ok(srv::json(StatusCode::OK, &json!({ "new_bytes": n }))),
        Err(e) => Ok(msg(StatusCode::BAD_REQUEST, &format!("{e:#}"))),
    }
}

async fn get_chunk(hub: &Arc<Hub>, claims: &Claims, id: &str, digest: &str) -> Result<Response, Fail> {
    let Ok(id) = id.parse::<i64>() else { return Ok(msg(StatusCode::NOT_FOUND, "no such entry")) };
    let repo = claims.repo.clone();
    let row: Option<(String, Option<String>)> = hub.store.call(move |c| Ok(c.query_row("SELECT scope, object FROM ci_caches WHERE id = ?1 AND repo_id = ?2", params![id, repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await.map_err(internal)?;
    let Some((scope, Some(object))) = row else { return Ok(msg(StatusCode::NOT_FOUND, "no such entry")) };
    if !read_scopes(claims, REPO_SCOPE).contains(&scope) {
        return Ok(msg(StatusCode::FORBIDDEN, "cache read denied: the entry is another ref's"));
    }
    let Some(obj) = cas::object(hub, &object).await.map_err(internal)? else { return Ok(msg(StatusCode::NOT_FOUND, "no such entry")) };
    if !obj.chunks.iter().any(|(d, _)| d == digest) {
        return Ok(msg(StatusCode::NOT_FOUND, "the entry has no such chunk"));
    }
    let Some(path) = cas::chunk_file(hub, digest) else { return Ok(msg(StatusCode::NOT_FOUND, "the chunk is gone")) };
    let data = crate::rt::blocking(move || std::fs::read(path)).await.map_err(internal)?.map_err(|e| internal(e.into()))?;
    Ok(srv::response(StatusCode::OK, "application/octet-stream", srv::full(bytes::Bytes::from(data))))
}

#[derive(Deserialize)]
struct Commit {
    key: String,
    version: String,
    #[serde(default)]
    scope: String,
    object: ObjectIn,
}

#[derive(Deserialize)]
struct ObjectIn {
    digest: String,
    size: u64,
    chunks: Vec<(String, u64)>,
}

/// An entry of stored chunks: each there, their sizes adding up, their content hashing to the
/// object's digest.
async fn commit(hub: &Arc<Hub>, claims: &Claims, req: Request) -> Result<Response, Fail> {
    let c: Commit = srv::body_json(req.into_body(), 16 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let Some(scope) = write_scope(claims, &c.scope) else {
        return Ok(msg(StatusCode::FORBIDDEN, &format!("cache write denied: this job's cache-mode is `{}`", claims.cm)));
    };
    if let Err(e) = cache::check_keys(std::slice::from_ref(&c.key)) {
        return Ok(msg(StatusCode::BAD_REQUEST, &e));
    }
    let max = hub.cfg.ci.cache.max_entry_gb.saturating_mul(1 << 30);
    if c.object.size > max {
        return Ok(msg(StatusCode::BAD_REQUEST, &format!("Cache size of ~{} MB is over the {} GB limit, not saving cache.", c.object.size >> 20, hub.cfg.ci.cache.max_entry_gb)));
    }
    let all = entries(hub, &claims.repo).await.map_err(internal)?;
    let plain: Vec<Entry> = all.into_iter().map(|(e, _)| e).collect();
    if !cache::may_reserve(&plain, &scope, &c.key, &c.version) {
        return Ok(msg(StatusCode::CONFLICT, &format!("Cache already exists. Scope: {scope}, Key: {}, Version: {}", c.key, c.version)));
    }
    let sizes: u64 = c.object.chunks.iter().map(|(_, n)| *n).sum();
    if sizes != c.object.size || c.object.chunks.iter().any(|(_, n)| *n == 0 || *n > kiln_ci::store::cdc::MAX as u64) {
        return Ok(msg(StatusCode::BAD_REQUEST, "the chunks' sizes don't add up to the object's"));
    }
    // The content, read back in order: every chunk there and as long as said, the whole as the
    // digest says.
    let (h, list) = (Arc::clone(hub), c.object.chunks.clone());
    let checked = crate::rt::blocking(move || -> Result<String, String> {
        let mut whole = Sha256::new();
        for (d, n) in &list {
            let Some(p) = cas::chunk_file(&h, d) else { return Err(format!("chunk {d} isn't stored: upload it first")) };
            let data = std::fs::read(&p).map_err(|e| format!("chunk {d}: {e}"))?;
            if data.len() as u64 != *n {
                return Err(format!("chunk {d} is {} bytes, not {n}", data.len()));
            }
            whole.update(&data);
        }
        Ok(hex::encode(whole.finalize()))
    })
    .await
    .map_err(internal)?;
    let digest = match checked {
        Ok(d) => d,
        Err(why) => return Ok(msg(StatusCode::BAD_REQUEST, &why)),
    };
    if digest != c.object.digest {
        return Ok(msg(StatusCode::BAD_REQUEST, &format!("the chunks make sha256:{digest}, not the object's sha256:{}", c.object.digest)));
    }
    let obj = cas::Object { digest: digest.clone(), size: c.object.size, chunks: c.object.chunks.clone() };
    cas::record(hub, &obj).await.map_err(internal)?;
    let (repo, leg, key, version, size, chunks) = (claims.repo.clone(), claims.sub.clone(), c.key.clone(), c.version.clone(), i64::try_from(c.object.size).unwrap_or(i64::MAX), c.object.chunks.clone());
    let made = hub
        .store
        .call(move |db| {
            let tx = db.transaction()?;
            let now = crate::util::now_unix();
            let n = tx.execute(
                "INSERT OR IGNORE INTO ci_caches (repo_id, scope, key, version, object, size, created_at, last_used_at, created_by_leg) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
                params![repo, scope, key, version, digest, size, now, leg],
            )?;
            for (d, _) in &chunks {
                tx.execute("INSERT OR IGNORE INTO ci_cache_chunks (repo_id, digest) VALUES (?1, ?2)", params![repo, d])?;
            }
            let id = tx.last_insert_rowid();
            tx.commit()?;
            Ok((n > 0).then_some(id))
        })
        .await
        .map_err(internal)?;
    let Some(id) = made else { return Ok(msg(StatusCode::CONFLICT, &format!("Cache already exists. Key: {}, Version: {}", c.key, c.version))) };
    crate::ci::insights::cache_event(hub, claims, &c.key, "save", Some(c.object.size));
    let (h, repo) = (Arc::clone(hub), claims.repo.clone());
    crate::rt::spawn(async move {
        if let Err(e) = super::cache_v1::evict(&h, Some(&repo)).await {
            tracing::warn!(error = %format!("{e:#}"), "cache eviction failed");
        }
    });
    Ok(srv::json(StatusCode::CREATED, &json!({ "id": id })))
}
