//! GitHub's artifact service as v1 to v3 of `actions/upload-artifact` and `download-artifact`
//! speak it (`@actions/artifact` before 2.0): file containers at `ACTIONS_RUNTIME_URL` (docs/ci.md
//! W08-R08). GitHub still takes it from an action its version check doesn't refuse (bat's
//! `upload-artifact@master`, 2.0.1) and keeps what comes as it keeps v4's. So does kiln: such an
//! artifact is one of its run's like any other, its files zipped as its size is set, and read by
//! v4's actions and the REST API too; the run's v4 artifacts are read this way as well.
//!
//! - `POST _apis/pipelines/workflows/<run>/artifacts`: the artifact's container, where its files
//!   go (`fileContainerResourceUrl`). Another upload by its name adds to it, as v3 had it: a
//!   matrix's legs uploading to one artifact.
//! - `PUT _apis/resources/Containers/<id>?itemPath=<name>/<path>`: a file, in `Content-Range`
//!   chunks, gzipped (`Content-Encoding: gzip`) when that's smaller.
//! - `PATCH _apis/pipelines/workflows/<run>/artifacts?artifactName=<name>`: its size, which zips
//!   what came (with what it had) and finishes it.
//! - `GET _apis/pipelines/workflows/<run>/artifacts`: the run's artifacts. `GET
//!   _apis/resources/Containers/<id>?itemPath=<name>`: an artifact's files, each with where it's
//!   read: the same, `itemPath` the file's.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use http::{Method, StatusCode, header};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::{blob, cache_v1, cas};
use crate::ci::token::Claims;
use crate::ci::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// A file's chunk, at most (the toolkit sends 8 MiB).
const MAX_PART: u64 = 256 << 20;
/// What's left on this machine for reads or by an upload never finished goes after this.
const KEPT: std::time::Duration = std::time::Duration::from_secs(86_400);

/// One artifact finishing at a time: a matrix's legs can finish one together.
static FINISHING: async_lock::Mutex<()> = async_lock::Mutex::new(());

/// The service's refusal, as its clients read one: a status, and a message they show.
fn message(status: StatusCode, msg: &str) -> Response {
    srv::json(status, &json!({ "message": msg, "typeKey": "ArtifactService" }))
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// Where an artifact's files wait for its finish, each by its path in the artifact (hex): as it
/// came, `.gzip` when it came gzipped, and beside it (`.total`) how long it is whole.
fn files_dir(hub: &Hub, id: i64) -> PathBuf {
    cas::staging(hub).join(format!("pipelines-{id}"))
}

/// A staged file's length once it's all there.
fn total_of(data: &Path) -> PathBuf {
    let mut s = data.as_os_str().to_owned();
    s.push(".total");
    PathBuf::from(s)
}

/// Where artifacts' zips and files are kept for reads (by the zip's digest).
fn read_dir(hub: &Hub) -> PathBuf {
    cas::staging(hub).join("pipelines-read")
}

fn container_url(origin: &str, id: i64) -> String {
    format!("{origin}/_apis/resources/Containers/{id}")
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

/// An artifact as the service lists it.
fn container_json(origin: &str, id: i64, name: &str, size: i64, expires: Option<i64>) -> Value {
    let mut v = json!({
        "containerId": id, "size": size, "signedContent": null, "fileContainerResourceUrl": container_url(origin, id),
        "type": "actions_storage", "name": name, "url": container_url(origin, id),
    });
    if let (Some(t), Some(o)) = (expires, v.as_object_mut()) {
        o.insert("expiresOn".into(), json!(rfc3339(t)));
    }
    v
}

fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// `_apis/pipelines/workflows/<run>/artifacts`, for a leg's `ACTIONS_RUNTIME_TOKEN`: its own run,
/// whatever run the path says.
pub async fn artifacts(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let Some(c) = token::of_request(hub, req.headers()) else { return Ok(message(StatusCode::UNAUTHORIZED, "a leg's ACTIONS_RUNTIME_TOKEN is needed")) };
    let origin = blob::origin(&req);
    let name = srv::query(&req).get("artifactName").cloned();
    match (req.method().clone(), name) {
        (Method::POST, _) => create(hub, &c, &origin, req).await,
        (Method::PATCH, Some(name)) => finish(hub, &c, &origin, &name).await,
        (Method::GET, name) => list(hub, &c, &origin, name).await,
        _ => Ok(message(StatusCode::METHOD_NOT_ALLOWED, "artifacts take POST, PATCH (with artifactName) and GET")),
    }
}

async fn create(hub: &Hub, c: &Claims, origin: &str, req: Request) -> Result<Response, Fail> {
    let body: Value = srv::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let name = body.get("Name").and_then(Value::as_str).unwrap_or_default().to_owned();
    if let Err(e) = super::twirp::valid_name(&name) {
        return Ok(message(StatusCode::BAD_REQUEST, &e));
    }
    let now = crate::util::now_unix();
    let max_days = i64::from(hub.cfg.ci.artifacts.retention_days.max(1));
    let days = body.get("RetentionDays").and_then(Value::as_i64).filter(|d| *d > 0).map_or(max_days, |d| d.min(max_days));
    let (run, leg, n) = (c.run.clone(), c.sub.clone(), name.clone());
    let made = hub
        .store
        .call(move |db| {
            let tx = db.transaction()?;
            let existing: Option<(i64, bool, Option<i64>)> =
                tx.query_row("SELECT id, v3 = 1, expires_at FROM ci_artifacts WHERE run_id = ?1 AND name = ?2", params![run, n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            let made = match existing {
                // Its container: what's uploaded now adds to what it has.
                Some((id, true, expires)) => Ok((id, expires)),
                Some((_, false, _)) => Err("an artifact with this name was uploaded by v4 of the artifact actions, and it can't be added to"),
                None => {
                    let expires = now + days * 86_400;
                    tx.execute(
                        "INSERT INTO ci_artifacts (run_id, leg_id, name, created_at, expires_at, mime_type, v3) VALUES (?1, ?2, ?3, ?4, ?5, 'application/zip', 1)",
                        params![run, leg, n, now, expires],
                    )?;
                    Ok((tx.last_insert_rowid(), Some(expires)))
                }
            };
            tx.commit()?;
            Ok(made)
        })
        .await
        .map_err(internal)?;
    match made {
        Ok((id, expires)) => Ok(srv::json(StatusCode::CREATED, &container_json(origin, id, &name, -1, expires))),
        Err(why) => Ok(message(StatusCode::CONFLICT, why)),
    }
}

/// `itemPath`'s path inside artifact `name` (`/` its separator, as a Windows client's `\` is
/// taken to be), or `None` when it's outside it or no plain path.
fn inside(name: &str, item: &str) -> Option<String> {
    let item = item.replace('\\', "/");
    let rest = item.strip_prefix(name)?.strip_prefix('/')?;
    let ok = !rest.is_empty() && rest.split('/').all(|s| !s.is_empty() && s != "." && s != "..");
    ok.then(|| rest.to_owned())
}

/// `Content-Range: bytes <start>-<end>/<total>` → (start, end, total).
fn content_range(req: &Request) -> Option<(u64, u64, u64)> {
    let v = req.headers().get(header::CONTENT_RANGE)?.to_str().ok()?;
    let spec = v.trim().strip_prefix("bytes")?.trim();
    let (range, total) = spec.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let (start, end, total): (u64, u64, u64) = (a.trim().parse().ok()?, b.trim().parse().ok()?, total.trim().parse().ok()?);
    (end >= start && end < total).then_some((start, end, total))
}

/// An artifact of the leg's run, by its container.
struct Artifact {
    id: i64,
    name: String,
    /// v1 to v3 of the artifact actions made it.
    v3: bool,
    /// Its content once finished: a zip, or (v4's `archive: false`) one file.
    object: Option<String>,
    zipped: bool,
}

async fn artifact_of(hub: &Hub, c: &Claims, id: &str) -> Result<Option<Artifact>, Fail> {
    let Ok(id) = id.parse::<i64>() else { return Ok(None) };
    let run = c.run.clone();
    hub.store
        .call(move |db| {
            Ok(db
                .query_row(
                    "SELECT name, v3 = 1, CASE WHEN finalized = 1 AND (expires_at IS NULL OR expires_at > ?3) THEN object END, COALESCE(mime_type, 'application/zip') LIKE '%zip%'
                     FROM ci_artifacts WHERE id = ?1 AND run_id = ?2",
                    params![id, run, crate::util::now_unix()],
                    |r| Ok(Artifact { id, name: r.get(0)?, v3: r.get(1)?, object: r.get(2)?, zipped: r.get(3)? }),
                )
                .optional()?)
        })
        .await
        .map_err(internal)
}

/// `_apis/resources/Containers/<id>`: a file of the artifact up (`PUT`), or its files listed or
/// one of them read (`GET`).
pub async fn container(hub: &Arc<Hub>, id: &str, req: Request) -> Result<Response, Fail> {
    let Some(c) = token::of_request(hub, req.headers()) else { return Ok(message(StatusCode::UNAUTHORIZED, "a leg's ACTIONS_RUNTIME_TOKEN is needed")) };
    let Some(a) = artifact_of(hub, &c, id).await? else { return Ok(message(StatusCode::NOT_FOUND, "no such container in this run")) };
    let item = srv::query(&req).get("itemPath").cloned().unwrap_or_default();
    let origin = blob::origin(&req);
    match *req.method() {
        Method::PUT if a.v3 => upload(hub, a.id, &a.name, &item, req).await,
        Method::PUT => Ok(message(StatusCode::CONFLICT, "this artifact was uploaded by v4 of the artifact actions, and it can't be added to")),
        Method::GET if a.object.is_some() => read(hub, &a, &item, &origin).await,
        Method::GET => Ok(message(StatusCode::NOT_FOUND, "the artifact isn't finished")),
        _ => Ok(message(StatusCode::METHOD_NOT_ALLOWED, "a container takes PUT and GET")),
    }
}

async fn upload(hub: &Hub, id: i64, name: &str, item: &str, req: Request) -> Result<Response, Fail> {
    let Some(path) = inside(name, item) else { return Ok(message(StatusCode::BAD_REQUEST, &format!("itemPath must name a file inside the artifact ({name}/<path>): {item}"))) };
    let Some((start, end, total)) = content_range(&req) else { return Ok(message(StatusCode::BAD_REQUEST, "Content-Range: bytes <start>-<end>/<total> is required")) };
    let max = hub.cfg.ci.artifacts.max_size_gb.saturating_mul(1 << 30);
    let len = end - start + 1;
    if len > MAX_PART || total > max {
        return Ok(message(StatusCode::PAYLOAD_TOO_LARGE, &format!("the file is over the artifact's {max} bytes, or the chunk over {MAX_PART}")));
    }
    let gzip = req.headers().get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.trim().eq_ignore_ascii_case("gzip"));
    let base = files_dir(hub, id).join(hex::encode(path.as_bytes()));
    let gzipped = base.with_extension("gzip");
    let (target, other) = if gzip { (gzipped, base) } else { (base, gzipped) };
    // A file's first chunk starts it over (a retry of the upload, or a new upload by its name).
    if start == 0 {
        let (t, o) = (target.clone(), other);
        crate::rt::blocking(move || {
            for p in [total_of(&t), t, total_of(&o), o] {
                let _ = std::fs::remove_file(p);
            }
        })
        .await
        .map_err(internal)?;
    }
    if let Err(e) = cache_v1::write_at(req, &target, start, len).await {
        return Ok(message(StatusCode::BAD_REQUEST, &format!("{e:#}")));
    }
    let whole = total_of(&target);
    crate::rt::blocking(move || std::fs::write(whole, total.to_string())).await.map_err(internal)?.map_err(|e| internal(e.into()))?;
    Ok(srv::json(StatusCode::CREATED, &json!({ "containerId": id, "path": format!("{name}/{path}"), "itemType": "file", "status": "created" })))
}

/// The artifact's files zipped, after what its zip had (`prior`), into `out`: the staged files
/// it took. A file uploaded again is the new one; one still uploading (by another leg) waits for
/// the next finish.
fn zip_up(files: &Path, prior: Option<&Path>, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut staged: Vec<(String, PathBuf, bool)> = Vec::new();
    if files.is_dir() {
        for e in std::fs::read_dir(files)? {
            let p = e?.path();
            let (stem, gzip) = match p.extension().and_then(|x| x.to_str()) {
                Some("gzip") => (p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned(), true),
                Some(_) => continue,
                None => (p.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_owned(), false),
            };
            let Some(name) = hex::decode(&stem).ok().and_then(|b| String::from_utf8(b).ok()) else { continue };
            let whole = std::fs::read_to_string(total_of(&p)).ok().and_then(|t| t.trim().parse::<u64>().ok());
            if whole.is_some_and(|t| std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) < t) {
                continue;
            }
            staged.push((name, p, gzip));
        }
    }
    staged.sort();
    // Both, only by a race of two uploads: one is kept.
    staged.dedup_by(|a, b| a.0 == b.0);
    let mut w = zip::ZipWriter::new(std::fs::File::create(out)?);
    if let Some(prior) = prior {
        let mut a = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(prior)?))?;
        for i in 0..a.len() {
            let f = a.by_index_raw(i)?;
            let name = f.name()?.into_owned();
            if f.is_dir() || staged.iter().any(|(n, _, _)| *n == name) {
                continue;
            }
            w.raw_copy_file(f)?;
        }
    }
    for (name, data, gzip) in &staged {
        let len = std::fs::metadata(data)?.len();
        let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(len >= u64::from(u32::MAX));
        w.start_file(name.as_str(), o)?;
        let src = std::fs::File::open(data)?;
        if *gzip {
            std::io::copy(&mut flate2::read::GzDecoder::new(src), &mut w)?;
        } else {
            std::io::copy(&mut std::io::BufReader::new(src), &mut w)?;
        }
    }
    w.finish()?;
    Ok(staged.into_iter().map(|(_, p, _)| p).collect())
}

async fn finish(hub: &Hub, c: &Claims, origin: &str, name: &str) -> Result<Response, Fail> {
    let _one = FINISHING.lock().await;
    let (run, n) = (c.run.clone(), name.to_owned());
    let row: Option<(i64, bool, Option<String>, Option<i64>)> = hub
        .store
        .call(move |db| {
            Ok(db
                .query_row("SELECT id, v3 = 1, object, expires_at FROM ci_artifacts WHERE run_id = ?1 AND name = ?2", params![run, n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .optional()?)
        })
        .await
        .map_err(internal)?;
    let Some((id, v3, prior, expires)) = row else { return Ok(message(StatusCode::NOT_FOUND, &format!("An Artifact with the name {name} was not found"))) };
    if !v3 {
        return Ok(message(StatusCode::CONFLICT, "this artifact was uploaded by v4 of the artifact actions"));
    }
    let prior = match prior {
        Some(d) => Some(local_zip(hub, &d).await.map_err(internal)?),
        None => None,
    };
    let files = files_dir(hub, id);
    let zip = cas::staging(hub).join(format!("pipelines-{id}.zip"));
    let (f, z) = (files.clone(), zip.clone());
    let took = crate::rt::blocking(move || zip_up(&f, prior.as_deref(), &z)).await.map_err(internal)?.map_err(internal)?;
    let obj = cas::put_file(hub, &zip).await.map_err(internal)?;
    let (digest, size) = (obj.digest.clone(), i64::try_from(obj.size).unwrap_or(i64::MAX));
    let d = digest.clone();
    hub.store.call(move |db| Ok(db.execute("UPDATE ci_artifacts SET object = ?2, size = ?3, digest = 'sha256:' || ?2, finalized = 1 WHERE id = ?1", params![id, d, size])?)).await.map_err(internal)?;
    // Kept for the reads that follow; what it took is in it now.
    let kept = read_dir(hub).join(format!("{digest}.zip"));
    crate::rt::blocking(move || {
        let _ = kept.parent().map(std::fs::create_dir_all);
        if std::fs::rename(&zip, &kept).is_err() {
            let _ = std::fs::remove_file(&zip);
        }
        for p in took {
            let _ = std::fs::remove_file(total_of(&p));
            let _ = std::fs::remove_file(p);
        }
        // Empty unless another leg is uploading to it.
        let _ = std::fs::remove_dir(files);
    })
    .await
    .map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &container_json(origin, id, name, size, expires)))
}

async fn list(hub: &Hub, c: &Claims, origin: &str, name: Option<String>) -> Result<Response, Fail> {
    let run = c.run.clone();
    let found: Vec<(i64, String, i64, Option<i64>)> = hub
        .store
        .call(move |db| {
            let mut q = db.prepare(
                "SELECT id, name, COALESCE(size, 0), expires_at FROM ci_artifacts
                 WHERE run_id = ?1 AND finalized = 1 AND object IS NOT NULL AND (expires_at IS NULL OR expires_at > ?2) AND (?3 IS NULL OR name = ?3) ORDER BY id",
            )?;
            let rows = q.query_map(params![run, crate::util::now_unix(), name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let value: Vec<Value> = found.iter().map(|(id, name, size, expires)| container_json(origin, *id, name, *size, *expires)).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "count": value.len(), "value": value })))
}

/// The zip with object `digest` on this machine: from the store the first time, kept after.
async fn local_zip(hub: &Hub, digest: &str) -> anyhow::Result<PathBuf> {
    let path = read_dir(hub).join(format!("{digest}.zip"));
    let p = path.clone();
    if crate::rt::blocking(move || p.is_file()).await? {
        return Ok(path);
    }
    let obj = cas::object(hub, digest).await?.ok_or_else(|| anyhow::anyhow!("the artifact's content is gone"))?;
    let part = path.with_extension(format!("{}.part", crate::util::new_id()));
    let (sink, done) = crate::rt::file_sink(part.clone(), obj.size)?;
    let rx = cas::read(hub, &obj, 0, obj.size);
    while let Ok(chunk) = rx.recv().await {
        if sink.send(chunk).await.is_err() {
            break;
        }
    }
    drop(sink);
    let n = done.await?;
    anyhow::ensure!(n == obj.size, "the artifact's content ended after {n} of {} bytes", obj.size);
    let (from, to) = (part, path.clone());
    crate::rt::blocking(move || std::fs::rename(from, to)).await??;
    Ok(path)
}

/// One of a zip's files.
struct Entry {
    name: String,
    size: u64,
}

fn entries(zip: &Path) -> anyhow::Result<Vec<Entry>> {
    let mut a = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(zip)?))?;
    let mut out = Vec::new();
    for i in 0..a.len() {
        let f = a.by_index_raw(i)?;
        if !f.is_dir() {
            out.push(Entry { name: f.name()?.into_owned(), size: f.size() });
        }
    }
    Ok(out)
}

/// The artifact's files under `item` listed, or (`item` naming one) that file.
async fn read(hub: &Hub, a: &Artifact, item: &str, origin: &str) -> Result<Response, Fail> {
    let (id, name, digest) = (a.id, a.name.as_str(), a.object.clone().unwrap_or_default());
    let item = item.replace('\\', "/");
    let Some(under) = (if item == name { Some(String::new()) } else { item.strip_prefix(name).and_then(|r| r.strip_prefix('/')).map(str::to_owned) }) else {
        return Ok(message(StatusCode::NOT_FOUND, &format!("no such item in the artifact: {item}")));
    };
    let obj = cas::object(hub, &digest).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, "the artifact's content is gone".to_owned()))?;
    // One file uploaded unzipped (v4's `archive: false`): the artifact's only one, by its name.
    let (zip, all) = if a.zipped {
        let zip = local_zip(hub, &digest).await.map_err(internal)?;
        let z = zip.clone();
        (Some(zip), crate::rt::blocking(move || entries(&z)).await.map_err(internal)?.map_err(internal)?)
    } else {
        (None, vec![Entry { name: name.to_owned(), size: obj.size }])
    };
    if let Some(e) = all.iter().find(|e| e.name == under) {
        return match zip {
            Some(zip) => file(hub, &zip, &digest, e).await,
            None => {
                let r = srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(cas::read(hub, &obj, 0, obj.size)));
                Ok(srv::with_header(r, header::CONTENT_LENGTH, &obj.size.to_string()))
            }
        };
    }
    let prefix = if under.is_empty() { String::new() } else { format!("{}/", under.trim_end_matches('/')) };
    let value: Vec<Value> = all
        .iter()
        .filter(|e| e.name.starts_with(&prefix))
        .map(|e| {
            let path = format!("{name}/{}", e.name);
            let at = format!("{}?itemPath={}", container_url(origin, id), pct(&path));
            json!({
                "containerId": id, "scopeIdentifier": "00000000-0000-0000-0000-000000000000", "path": path, "itemType": "file", "status": "created",
                "fileLength": e.size, "fileEncoding": 1, "fileType": 1, "itemLocation": format!("{at}&metadata=True"), "contentLocation": at, "contentId": "",
            })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "count": value.len(), "value": value })))
}

/// A zip's file, read: unzipped once, kept beside the zip.
async fn file(hub: &Hub, zip: &Path, digest: &str, e: &Entry) -> Result<Response, Fail> {
    let out = read_dir(hub).join(digest).join(hex::encode(e.name.as_bytes()));
    let (z, o, name) = (zip.to_path_buf(), out.clone(), e.name.clone());
    crate::rt::blocking(move || -> anyhow::Result<()> {
        if o.is_file() {
            return Ok(());
        }
        let mut a = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(&z)?))?;
        let mut f = a.by_name(&name)?;
        if let Some(d) = o.parent() {
            std::fs::create_dir_all(d)?;
        }
        let part = o.with_extension(crate::util::new_id());
        std::io::copy(&mut f, &mut std::fs::File::create(&part)?)?;
        std::fs::rename(part, &o)?;
        Ok(())
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    let body = srv::channel_body(crate::rt::file_source(out).map_err(internal)?);
    let r = srv::response(StatusCode::OK, "application/octet-stream", body);
    Ok(srv::with_header(r, header::CONTENT_LENGTH, &e.size.to_string()))
}

/// Upkeep: what's kept for reads, and uploads never finished, after a day.
pub async fn sweep(hub: &Hub) {
    let staging = cas::staging(hub);
    let _ = crate::rt::blocking(move || {
        let old = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > KEPT);
        let Ok(dir) = std::fs::read_dir(&staging) else { return };
        for e in dir.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name == "pipelines-read" {
                for kept in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                    let k = kept.path();
                    if old(&k) {
                        let _ = if k.is_dir() { std::fs::remove_dir_all(&k) } else { std::fs::remove_file(&k) };
                    }
                }
            } else if name.starts_with("pipelines-") && old(&p) {
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w08_r08_v3_item_paths_stay_inside_the_artifact() {
        assert_eq!(inside("dist", "dist/bat.tar.gz").as_deref(), Some("bat.tar.gz"));
        assert_eq!(inside("dist", "dist/sub/dir/f").as_deref(), Some("sub/dir/f"));
        assert_eq!(inside("dist", "dist\\win\\f.exe").as_deref(), Some("win/f.exe"), "a Windows client's separators");
        for bad in ["dist", "dist/", "other/f", "distx/f", "dist/../f", "dist/a//b", "dist/./f", "dist/a/.."] {
            assert_eq!(inside("dist", bad), None, "{bad}");
        }
    }

    #[test]
    fn w08_r08_v3_files_zip_with_what_the_artifact_had() -> anyhow::Result<()> {
        use std::io::{Read, Write};
        let tmp = tempfile::tempdir()?;
        let files = tmp.path().join("files");
        std::fs::create_dir_all(&files)?;
        // One as it came, one gzipped, one that replaces what the artifact had.
        std::fs::write(files.join(hex::encode("a.txt")), "plain\n")?;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"gzipped\n")?;
        std::fs::write(files.join(format!("{}.gzip", hex::encode("dir/b.txt"))), gz.finish()?)?;
        std::fs::write(files.join(hex::encode("old.txt")), "new\n")?;
        // Another leg's, still uploading: it waits for the next finish.
        std::fs::write(files.join(hex::encode("later.txt")), "lat")?;
        std::fs::write(total_of(&files.join(hex::encode("later.txt"))), "6")?;
        let prior = tmp.path().join("prior.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&prior)?);
        for (n, t) in [("old.txt", "old\n"), ("kept.txt", "kept\n")] {
            w.start_file(n, zip::write::SimpleFileOptions::default())?;
            w.write_all(t.as_bytes())?;
        }
        w.finish()?;
        let out = tmp.path().join("out.zip");
        assert_eq!(zip_up(&files, Some(&prior), &out)?.len(), 3, "the three whole files");
        let mut a = zip::ZipArchive::new(std::fs::File::open(&out)?)?;
        let mut read = |n: &str| -> anyhow::Result<String> {
            let mut s = String::new();
            a.by_name(n)?.read_to_string(&mut s)?;
            Ok(s)
        };
        assert_eq!((read("a.txt")?, read("dir/b.txt")?, read("old.txt")?, read("kept.txt")?), ("plain\n".into(), "gzipped\n".into(), "new\n".into(), "kept\n".into()));
        let listed: Vec<String> = entries(&out)?.into_iter().map(|e| format!("{} {}", e.name, e.size)).collect();
        assert_eq!(listed, ["kept.txt 5", "a.txt 6", "dir/b.txt 8", "old.txt 4"]);
        Ok(())
    }
}
