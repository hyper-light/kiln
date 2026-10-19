//! Runner images kiln built (docs/ci.md W13-R11): what `kiln/build-image@v1` pushed to kiln's
//! registry (W12), registered (`ci_images`) with their platforms, for pools and routing rules.
//!
//! - A leg reports each image it built (`JobEventV2::ImageBuilt`). The server registers one only
//!   as its own registry has it: under the leg's own repository (`<repo>/images/<name>`), the
//!   digest one of its manifests there, the platforms read from that manifest (an index's
//!   members, else its config's `os` and `architecture`), never taken from the report.
//! - `GET /api/ci/images?repo=&name=`: the images, newest first.

use std::collections::HashMap;

use http::StatusCode;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// A config blob past this isn't read for its platform.
const MAX_CONFIG: u64 = 1 << 20;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// Registers the image `name` that leg `leg` built and pushed as `reference`
/// (`<host>/<repo>/images/<name>:<tag>`), `digest` as it said.
pub async fn built(hub: &Hub, leg: &str, name: &str, reference: &str, digest: &str, reported: &[String]) -> anyhow::Result<()> {
    let l = leg.to_owned();
    let row: Option<(String, String, String)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT j.run_id, r.repo_id, p.name FROM jobs j JOIN runs r ON r.id = j.run_id JOIN repos p ON p.id = r.repo_id WHERE j.id = ?1", [&l], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?))).optional()?))
        .await?;
    let Some((run, repo_id, repo)) = row else { return Ok(()) };
    // The runner names kiln's registry as it reaches it; the repository and tag are what count.
    let want = format!("{}/images/{name}", repo.to_ascii_lowercase());
    let path = reference.split_once('/').map_or("", |(_, p)| p);
    let path = path.split('@').next().unwrap_or(path);
    let (local, tag) = match path.rsplit_once(':') {
        Some((n, t)) if !t.contains('/') => (n.to_owned(), t.to_owned()),
        _ => (path.to_owned(), "latest".to_owned()),
    };
    anyhow::ensure!(local == want, "{reference}: {repo}'s legs build images under {want}");
    // The image built, else what its tag names now.
    let (n, d, t) = (local.clone(), digest.to_owned(), tag.clone());
    let found: Option<(String, Vec<u8>)> = hub
        .store
        .call(move |c| {
            let by_digest = c.query_row("SELECT digest, body FROM registry_manifests WHERE repo = ?1 AND digest = ?2", params![n, d], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            Ok(match by_digest {
                Some(m) => Some(m),
                None => c
                    .query_row("SELECT m.digest, m.body FROM registry_tags t JOIN registry_manifests m ON m.repo = t.repo AND m.digest = t.digest WHERE t.repo = ?1 AND t.tag = ?2", params![n, t], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?,
            })
        })
        .await?;
    let (digest, body) = found.ok_or_else(|| anyhow::anyhow!("{reference} isn't in kiln's registry"))?;
    let manifest: Value = serde_json::from_slice(&body)?;
    let mut platforms = platforms_of(hub, &manifest).await;
    if platforms.is_empty() {
        platforms = reported.to_vec();
    }
    let own = crate::githost::host_of(&hub.cfg.server.public_url);
    let (n, r, d, p) = (name.to_owned(), format!("{own}/{local}:{tag}"), digest.clone(), serde_json::to_string(&platforms)?);
    let leg = leg.to_owned();
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO ci_images (repo_id, name, reference, digest, platforms, run_id, leg_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![repo_id, n, r, d, p, run, leg, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await?;
    hub.emit(&json!({ "type": "ci_image", "repo": repo, "name": name, "digest": digest, "platforms": platforms }));
    Ok(())
}

/// The platforms an image is for: an index's members (its attestation manifests aside), else
/// its config's.
async fn platforms_of(hub: &Hub, manifest: &Value) -> Vec<String> {
    let platform = |p: &Value| {
        let (os, arch) = (p.get("os")?.as_str()?, p.get("architecture")?.as_str()?);
        (os != "unknown" && arch != "unknown").then(|| match p.get("variant").and_then(Value::as_str) {
            Some(v) if !v.is_empty() => format!("{os}/{arch}/{v}"),
            _ => format!("{os}/{arch}"),
        })
    };
    if let Some(members) = manifest.get("manifests").and_then(Value::as_array) {
        return members.iter().filter_map(|m| m.get("platform").and_then(platform)).collect();
    }
    let Some(hex) = manifest.pointer("/config/digest").and_then(Value::as_str).and_then(|d| d.strip_prefix("sha256:")) else { return Vec::new() };
    let Ok(Some(obj)) = crate::ci::services::cas::object(hub, hex).await else { return Vec::new() };
    if obj.size > MAX_CONFIG {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let rx = crate::ci::services::cas::read(hub, &obj, 0, obj.size);
    while let Ok(b) = rx.recv().await {
        buf.extend_from_slice(&b);
    }
    serde_json::from_slice::<Value>(&buf).ok().and_then(|c| platform(&c)).into_iter().collect()
}

/// `GET /api/ci/images?repo=&name=`.
pub async fn list(hub: &Hub, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let some = |k: &str| q.get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let repos = hub.store.repos().await.map_err(internal)?;
    let repo = match some("repo") {
        Some(r) => Some(repos.iter().find(|x| x.id == r || x.name == r).map(|x| x.id.clone()).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{r}`")))?),
        None => None,
    };
    let name = some("name");
    type Row = (i64, String, String, String, String, String, Option<String>, i64);
    let rows: Vec<Row> = hub
        .store
        .call(move |c| {
            let mut s = c.prepare(
                "SELECT id, repo_id, name, reference, digest, platforms, run_id, created_at FROM ci_images
                 WHERE (?1 IS NULL OR repo_id = ?1) AND (?2 IS NULL OR name = ?2) ORDER BY created_at DESC, id DESC LIMIT 500",
            )?;
            let rows = s.query_map(params![repo, name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let names: HashMap<&str, &str> = repos.iter().map(|r| (r.id.as_str(), r.name.as_str())).collect();
    let images: Vec<Value> = rows
        .into_iter()
        .map(|(id, repo_id, name, reference, digest, platforms, run, at)| {
            let image = format!("{}@{digest}", reference.rsplit_once(':').filter(|(_, t)| !t.contains('/')).map_or(reference.as_str(), |(r, _)| r));
            json!({
                "id": id, "repo": names.get(repo_id.as_str()), "name": name, "reference": reference, "digest": digest, "image": image,
                "platforms": serde_json::from_str::<Value>(&platforms).unwrap_or_default(), "run_id": run, "created_at": at,
            })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "images": images })))
}
