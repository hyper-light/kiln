//! What jobs record through the facade (docs/ci.md W14-R11), in GitHub's shapes:
//! - commit statuses (`POST /statuses/{sha}`, `GET /commits/{ref}/statuses`, `/status`);
//! - check runs (`POST /check-runs`, `PATCH`/`GET /check-runs/{id}`, `GET /commits/{ref}/check-runs`);
//! - releases (create, read, update, delete; by id, latest or tag) with assets uploaded to
//!   `upload_url` (`/api/uploads/…`, GHES's path) and kept in the store (W08);
//! - dependency snapshots (`POST /dependency-graph/snapshots`), as build tools' submission
//!   actions send them.
//!
//! For a repository kiln mirrors from GitHub they live in kiln, which doesn't write to GitHub
//! (the App does that, W14-R10).

use http::StatusCode;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::repos::Site;
use crate::ci::Fail;
use crate::http::server::{self as srv, Request, Response};

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

fn parse_time(v: Option<&Value>) -> Option<i64> {
    let s = v?.as_str()?;
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok().map(time::OffsetDateTime::unix_timestamp)
}

fn invalid(m: &str) -> Response {
    srv::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "message": "Validation Failed", "errors": [{ "message": m }] }))
}

fn bot() -> Value {
    json!({ "login": "github-actions[bot]", "id": 41898282, "type": "Bot" })
}

// ------------------------------------------------------------------ statuses

fn status_json(s: &Site<'_>, id: i64, state: &str, context: &str, description: Option<&str>, target: Option<&str>, at: i64) -> Value {
    json!({ "url": format!("{}/statuses/{id}", s.api), "id": id, "node_id": format!("kiln-status-{id}"), "state": state, "description": description, "target_url": target,
            "context": context, "created_at": rfc3339(at), "updated_at": rfc3339(at), "creator": bot() })
}

/// `POST /repos/{o}/{r}/statuses/{sha}`.
pub async fn create_status(s: &Site<'_>, sha: &str, body: &Value) -> Result<Response, Fail> {
    let state = body.get("state").and_then(Value::as_str).unwrap_or_default().to_owned();
    if !matches!(state.as_str(), "error" | "failure" | "pending" | "success") {
        return Ok(invalid("state is error, failure, pending or success"));
    }
    let Some(full) = s.resolve(sha).await else { return Ok(srv::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "message": format!("No commit found for SHA: {sha}") }))) };
    let context = body.get("context").and_then(Value::as_str).filter(|c| !c.is_empty()).unwrap_or("default").to_owned();
    let description = body.get("description").and_then(Value::as_str).map(|d| d.chars().take(140).collect::<String>());
    let target = body.get("target_url").and_then(Value::as_str).map(str::to_owned);
    let (repo, st, ctx, d, t, f) = (s.repo.id.clone(), state.clone(), context.clone(), description.clone(), target.clone(), full);
    let (id, at) = s
        .hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute("INSERT INTO ci_statuses (repo_id, sha, state, context, description, target_url, creator, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'github-actions[bot]', ?7)", params![repo, f, st, ctx, d, t, now])?;
            Ok((c.last_insert_rowid(), now))
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::CREATED, &status_json(s, id, &state, &context, description.as_deref(), target.as_deref(), at)))
}

type StatusRow = (i64, String, String, Option<String>, Option<String>, i64);

async fn statuses_of(s: &Site<'_>, sha: &str) -> anyhow::Result<Vec<StatusRow>> {
    let (repo, sha) = (s.repo.id.clone(), sha.to_owned());
    s.hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT id, state, context, description, target_url, created_at FROM ci_statuses WHERE repo_id = ?1 AND sha = ?2 ORDER BY id DESC")?;
            let rows = q.query_map(params![repo, sha], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// `GET /repos/{o}/{r}/commits/{ref}/statuses` (all, newest first) and `/status` (combined).
pub async fn statuses(s: &Site<'_>, r: &str, combined: bool) -> Result<Response, Fail> {
    let Some(sha) = s.resolve(r).await else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
    let rows = statuses_of(s, &sha).await.map_err(internal)?;
    if !combined {
        let list: Vec<Value> = rows.iter().map(|(id, st, ctx, d, t, at)| status_json(s, *id, st, ctx, d.as_deref(), t.as_deref(), *at)).collect();
        return Ok(srv::json(StatusCode::OK, &Value::Array(list)));
    }
    // The newest status of each context; failure beats pending beats success.
    let mut seen = std::collections::BTreeSet::new();
    let latest: Vec<&StatusRow> = rows.iter().filter(|r| seen.insert(r.2.clone())).collect();
    let state = if latest.iter().any(|r| r.1 == "error" || r.1 == "failure") {
        "failure"
    } else if latest.is_empty() || latest.iter().any(|r| r.1 == "pending") {
        "pending"
    } else {
        "success"
    };
    let list: Vec<Value> = latest.iter().map(|(id, st, ctx, d, t, at)| status_json(s, *id, st, ctx, d.as_deref(), t.as_deref(), *at)).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "state": state, "statuses": list, "sha": sha, "total_count": latest.len(), "commit_url": format!("{}/commits/{sha}", s.api), "url": format!("{}/commits/{sha}/status", s.api) })))
}

// ------------------------------------------------------------------ check runs

type CheckRow = (i64, String, String, String, Option<String>, Option<String>, Option<String>, String, Option<i64>, Option<i64>);

const CHECK_SELECT: &str = "SELECT id, head_sha, name, status, conclusion, details_url, external_id, output, started_at, completed_at FROM ci_check_runs";

fn check_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CheckRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
}

fn check_json(s: &Site<'_>, c: &CheckRow) -> Value {
    let (id, sha, name, status, conclusion, details, external, output, started, completed) = c;
    let output: Value = serde_json::from_str(output).unwrap_or_else(|_| json!({}));
    json!({ "id": id, "node_id": format!("kiln-check-{id}"), "head_sha": sha, "external_id": external, "url": format!("{}/check-runs/{id}", s.api),
            "html_url": format!("{}/runs/{id}", s.html), "details_url": details, "status": status, "conclusion": conclusion,
            "started_at": started.map(rfc3339), "completed_at": completed.map(rfc3339), "output": output, "name": name,
            "app": { "slug": "github-actions", "name": "GitHub Actions" }, "pull_requests": [] })
}

fn output_of(body: &Value) -> Value {
    let o = body.get("output").cloned().unwrap_or_else(|| json!({}));
    let n = o.get("annotations").and_then(Value::as_array).map_or(0, Vec::len);
    json!({ "title": o.get("title"), "summary": o.get("summary"), "text": o.get("text"), "annotations_count": n })
}

/// `POST /repos/{o}/{r}/check-runs`.
pub async fn create_check(s: &Site<'_>, body: &Value) -> Result<Response, Fail> {
    let name = body.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
    let head = body.get("head_sha").and_then(Value::as_str).unwrap_or_default().to_owned();
    if name.is_empty() || head.is_empty() {
        return Ok(invalid("name and head_sha are required"));
    }
    let Some(sha) = s.resolve(&head).await else { return Ok(invalid("head_sha is not a commit of the repository")) };
    let conclusion = body.get("conclusion").and_then(Value::as_str).map(str::to_owned);
    let status = if conclusion.is_some() { "completed".to_owned() } else { body.get("status").and_then(Value::as_str).unwrap_or("queued").to_owned() };
    let details = body.get("details_url").and_then(Value::as_str).map(str::to_owned);
    let external = body.get("external_id").and_then(Value::as_str).map(str::to_owned);
    let output = output_of(body).to_string();
    let started = parse_time(body.get("started_at"));
    let completed = parse_time(body.get("completed_at")).or_else(|| (status == "completed").then(crate::util::now_unix));
    let repo = s.repo.id.clone();
    let row = s
        .hub
        .store
        .call(move |c| {
            c.execute(
                "INSERT INTO ci_check_runs (repo_id, head_sha, name, status, conclusion, details_url, external_id, output, started_at, completed_at, creator, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, ?11), ?10, 'github-actions[bot]', ?11)",
                params![repo, sha, name, status, conclusion, details, external, output, started, completed, crate::util::now_unix()],
            )?;
            let id = c.last_insert_rowid();
            Ok(c.query_row(&format!("{CHECK_SELECT} WHERE id = ?1"), [id], check_row)?)
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::CREATED, &check_json(s, &row)))
}

/// `PATCH /repos/{o}/{r}/check-runs/{id}`.
pub async fn update_check(s: &Site<'_>, id: &str, body: &Value) -> Result<Response, Fail> {
    let Ok(id) = id.parse::<i64>() else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
    let (repo, b) = (s.repo.id.clone(), body.clone());
    let row = s
        .hub
        .store
        .call(move |c| {
            let Some(mut cur) = c.query_row(&format!("{CHECK_SELECT} WHERE id = ?1 AND repo_id = ?2"), params![id, repo], check_row).optional()? else { return Ok(None) };
            if let Some(n) = b.get("name").and_then(Value::as_str) {
                cur.2 = n.to_owned();
            }
            if let Some(st) = b.get("status").and_then(Value::as_str) {
                cur.3 = st.to_owned();
            }
            if let Some(cl) = b.get("conclusion").and_then(Value::as_str) {
                cur.4 = Some(cl.to_owned());
                cur.3 = "completed".into();
                cur.9 = cur.9.or_else(|| Some(crate::util::now_unix()));
            }
            if let Some(d) = b.get("details_url").and_then(Value::as_str) {
                cur.5 = Some(d.to_owned());
            }
            if b.get("output").is_some() {
                cur.7 = output_of(&b).to_string();
            }
            if let Some(t) = parse_time(b.get("completed_at")) {
                cur.9 = Some(t);
            }
            c.execute(
                "UPDATE ci_check_runs SET name = ?2, status = ?3, conclusion = ?4, details_url = ?5, output = ?6, completed_at = ?7 WHERE id = ?1",
                params![id, cur.2, cur.3, cur.4, cur.5, cur.7, cur.9],
            )?;
            Ok(Some(cur))
        })
        .await
        .map_err(internal)?;
    Ok(match row {
        Some(r) => srv::json(StatusCode::OK, &check_json(s, &r)),
        None => srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" })),
    })
}

/// `GET /repos/{o}/{r}/check-runs/{id}`, or a commit's (`/commits/{ref}/check-runs`).
pub async fn checks(s: &Site<'_>, id: Option<&str>, r: Option<&str>) -> Result<Response, Fail> {
    let repo = s.repo.id.clone();
    if let Some(id) = id {
        let Ok(id) = id.parse::<i64>() else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
        let row = s.hub.store.call(move |c| Ok(c.query_row(&format!("{CHECK_SELECT} WHERE id = ?1 AND repo_id = ?2"), params![id, repo], check_row).optional()?)).await.map_err(internal)?;
        return Ok(match row {
            Some(r) => srv::json(StatusCode::OK, &check_json(s, &r)),
            None => srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" })),
        });
    }
    let Some(sha) = s.resolve(r.unwrap_or_default()).await else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
    let rows = s
        .hub
        .store
        .call(move |c| {
            let mut q = c.prepare(&format!("{CHECK_SELECT} WHERE repo_id = ?1 AND head_sha = ?2 ORDER BY id DESC"))?;
            let rows = q.query_map(params![repo, sha], check_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let list: Vec<Value> = rows.iter().map(|r| check_json(s, r)).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "total_count": list.len(), "check_runs": list })))
}

// ------------------------------------------------------------------ releases

type ReleaseRow = (i64, String, String, Option<String>, Option<String>, bool, bool, String, i64, Option<i64>);

const RELEASE_SELECT: &str = "SELECT id, tag_name, target_commitish, name, body, draft = 1, prerelease = 1, author, created_at, published_at FROM ci_releases";

fn release_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReleaseRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
}

type AssetRow = (i64, String, Option<String>, String, i64, i64, i64);

async fn assets_of(s: &Site<'_>, release: i64) -> anyhow::Result<Vec<AssetRow>> {
    s.hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT id, name, label, content_type, size, created_at, downloads FROM ci_release_assets WHERE release_id = ?1 ORDER BY id")?;
            let rows = q.query_map([release], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

fn asset_json(s: &Site<'_>, tag: &str, a: &AssetRow) -> Value {
    let (id, name, label, ct, size, at, downloads) = a;
    json!({ "url": format!("{}/releases/assets/{id}", s.api), "browser_download_url": format!("{}/releases/download/{tag}/{name}", s.html), "id": id,
            "node_id": format!("kiln-asset-{id}"), "name": name, "label": label, "state": "uploaded", "content_type": ct, "size": size,
            "download_count": downloads, "created_at": rfc3339(*at), "updated_at": rfc3339(*at), "uploader": bot() })
}

async fn release_json(s: &Site<'_>, r: &ReleaseRow) -> anyhow::Result<Value> {
    let (id, tag, target, name, body, draft, pre, _, created, published) = r;
    let assets: Vec<Value> = assets_of(s, *id).await?.iter().map(|a| asset_json(s, tag, a)).collect();
    let uploads = format!("{}/api/uploads/repos/{}/releases/{id}/assets{{?name,label}}", s.origin, s.repo.name);
    Ok(json!({
        "url": format!("{}/releases/{id}", s.api), "html_url": format!("{}/releases/tag/{tag}", s.html), "assets_url": format!("{}/releases/{id}/assets", s.api),
        "upload_url": uploads, "tarball_url": format!("{}/tarball/{tag}", s.api), "zipball_url": format!("{}/zipball/{tag}", s.api),
        "id": id, "node_id": format!("kiln-release-{id}"), "tag_name": tag, "target_commitish": target, "name": name, "body": body,
        "draft": draft, "prerelease": pre, "created_at": rfc3339(*created), "published_at": published.map(rfc3339), "author": bot(), "assets": assets,
    }))
}

/// A release by id or tag (`filter` names `?2`), or the latest published one (`arg` `None`).
async fn one_release(s: &Site<'_>, filter: &str, arg: Option<String>) -> anyhow::Result<Option<ReleaseRow>> {
    let repo = s.repo.id.clone();
    let sql = match arg {
        Some(_) => format!("{RELEASE_SELECT} WHERE repo_id = ?1 AND {filter}"),
        None => format!("{RELEASE_SELECT} WHERE repo_id = ?1 AND draft = 0 AND prerelease = 0 ORDER BY published_at DESC, id DESC LIMIT 1"),
    };
    s.hub
        .store
        .call(move |c| {
            Ok(match arg {
                Some(a) => c.query_row(&sql, params![repo, a], release_row).optional()?,
                None => c.query_row(&sql, params![repo], release_row).optional()?,
            })
        })
        .await
}

/// `GET /releases`, `/releases/{id}`, `/releases/latest`, `/releases/tags/{tag}`.
pub async fn releases(s: &Site<'_>, which: &[&str]) -> Result<Response, Fail> {
    let row = match which {
        [] => {
            let repo = s.repo.id.clone();
            let rows = s
                .hub
                .store
                .call(move |c| {
                    let mut q = c.prepare(&format!("{RELEASE_SELECT} WHERE repo_id = ?1 ORDER BY id DESC"))?;
                    let rows = q.query_map([repo], release_row)?;
                    Ok(rows.collect::<Result<Vec<_>, _>>()?)
                })
                .await
                .map_err(internal)?;
            let mut list = Vec::new();
            for r in &rows {
                list.push(release_json(s, r).await.map_err(internal)?);
            }
            return Ok(srv::json(StatusCode::OK, &Value::Array(list)));
        }
        ["latest"] => one_release(s, "", None).await,
        ["tags", tag] => one_release(s, "tag_name = ?2", Some((*tag).to_owned())).await,
        [id] => one_release(s, "id = ?2", Some((*id).to_owned())).await,
        _ => return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))),
    }
    .map_err(internal)?;
    match row {
        Some(r) => Ok(srv::json(StatusCode::OK, &release_json(s, &r).await.map_err(internal)?)),
        None => Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))),
    }
}

/// `POST /releases` (`id = None`) and `PATCH /releases/{id}`.
pub async fn save_release(s: &Site<'_>, id: Option<&str>, body: &Value) -> Result<Response, Fail> {
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
    let flag = |k: &str| body.get(k).and_then(Value::as_bool);
    let now = crate::util::now_unix();
    let repo = s.repo.id.clone();
    let created = match id {
        None => {
            let Some(tag) = text("tag_name").filter(|t| !t.is_empty()) else { return Ok(invalid("tag_name is required")) };
            let target = text("target_commitish").unwrap_or_else(|| s.default_branch.clone());
            let (name, notes, draft, pre) = (text("name"), text("body"), flag("draft").unwrap_or(false), flag("prerelease").unwrap_or(false));
            let t = tag.clone();
            let made = s
                .hub
                .store
                .call(move |c| {
                    let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM ci_releases WHERE repo_id = ?1 AND tag_name = ?2)", params![repo, t], |r| r.get(0))?;
                    if exists {
                        return Ok(None);
                    }
                    c.execute(
                        "INSERT INTO ci_releases (repo_id, tag_name, target_commitish, name, body, draft, prerelease, author, created_at, published_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'github-actions[bot]', ?8, ?9)",
                        params![repo, t, target, name, notes, draft, pre, now, (!draft).then_some(now)],
                    )?;
                    Ok(Some(c.last_insert_rowid()))
                })
                .await
                .map_err(internal)?;
            match made {
                Some(id) => id,
                None => return Ok(srv::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "message": "Validation Failed", "errors": [{ "resource": "Release", "code": "already_exists", "field": "tag_name" }] }))),
            }
        }
        Some(id) => {
            let Ok(id) = id.parse::<i64>() else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
            let b = body.clone();
            let found = s
                .hub
                .store
                .call(move |c| {
                    let Some(mut r) = c.query_row(&format!("{RELEASE_SELECT} WHERE repo_id = ?1 AND id = ?2"), params![repo, id], release_row).optional()? else { return Ok(false) };
                    let text = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_owned);
                    if let Some(t) = text("tag_name") {
                        r.1 = t;
                    }
                    if let Some(t) = text("target_commitish") {
                        r.2 = t;
                    }
                    if b.get("name").is_some() {
                        r.3 = text("name");
                    }
                    if b.get("body").is_some() {
                        r.4 = text("body");
                    }
                    if let Some(d) = b.get("draft").and_then(Value::as_bool) {
                        r.5 = d;
                    }
                    if let Some(p) = b.get("prerelease").and_then(Value::as_bool) {
                        r.6 = p;
                    }
                    let published = if r.5 { None } else { r.9.or(Some(crate::util::now_unix())) };
                    c.execute(
                        "UPDATE ci_releases SET tag_name = ?2, target_commitish = ?3, name = ?4, body = ?5, draft = ?6, prerelease = ?7, published_at = ?8 WHERE id = ?1",
                        params![id, r.1, r.2, r.3, r.4, r.5, r.6, published],
                    )?;
                    Ok(true)
                })
                .await
                .map_err(internal)?;
            if !found {
                return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" })));
            }
            id
        }
    };
    let row = one_release(s, "id = ?2", Some(created.to_string())).await.map_err(internal)?.ok_or((StatusCode::INTERNAL_SERVER_ERROR, "the release is gone".to_owned()))?;
    let status = if id.is_none() { StatusCode::CREATED } else { StatusCode::OK };
    Ok(srv::json(status, &release_json(s, &row).await.map_err(internal)?))
}

/// `DELETE /releases/{id}` and `DELETE /releases/assets/{id}`.
pub async fn delete(s: &Site<'_>, asset: bool, id: &str) -> Result<Response, Fail> {
    let Ok(id) = id.parse::<i64>() else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
    let repo = s.repo.id.clone();
    let n = s
        .hub
        .store
        .call(move |c| {
            Ok(if asset {
                c.execute("DELETE FROM ci_release_assets WHERE id = ?1 AND release_id IN (SELECT id FROM ci_releases WHERE repo_id = ?2)", params![id, repo])?
            } else {
                c.execute("DELETE FROM ci_release_assets WHERE release_id IN (SELECT id FROM ci_releases WHERE id = ?1 AND repo_id = ?2)", params![id, repo])?;
                c.execute("DELETE FROM ci_releases WHERE id = ?1 AND repo_id = ?2", params![id, repo])?
            })
        })
        .await
        .map_err(internal)?;
    Ok(if n > 0 { srv::status(StatusCode::NO_CONTENT) } else { srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" })) })
}

/// `GET /releases/{id}/assets`.
pub async fn assets(s: &Site<'_>, release: &str) -> Result<Response, Fail> {
    let Some(r) = one_release(s, "id = ?2", Some(release.to_owned())).await.map_err(internal)? else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
    let list: Vec<Value> = assets_of(s, r.0).await.map_err(internal)?.iter().map(|a| asset_json(s, &r.1, a)).collect();
    Ok(srv::json(StatusCode::OK, &Value::Array(list)))
}

/// An asset's content, by id (`GET /releases/assets/{id}` with `Accept: application/octet-stream`)
/// or by tag and name (`/<owner>/<repo>/releases/download/<tag>/<name>`).
pub async fn asset_content(s: &Site<'_>, by: AssetBy<'_>) -> Result<Response, Fail> {
    let repo = s.repo.id.clone();
    const SELECT: &str = "SELECT a.id, a.object, a.content_type, a.name FROM ci_release_assets a JOIN ci_releases r ON r.id = a.release_id WHERE r.repo_id = ?1";
    let (id, tag, name) = match by {
        AssetBy::Id(id) => (Some(id.to_owned()), None, None),
        AssetBy::Name(tag, name) => (None, Some(tag.to_owned()), Some(name.to_owned())),
    };
    let row: Option<(i64, String, String, String)> = s
        .hub
        .store
        .call(move |c| {
            let get = |r: &rusqlite::Row<'_>| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?));
            Ok(match (id, tag, name) {
                (Some(id), _, _) => c.query_row(&format!("{SELECT} AND a.id = ?2"), params![repo, id], get).optional()?,
                (None, Some(t), Some(n)) => c.query_row(&format!("{SELECT} AND r.tag_name = ?2 AND a.name = ?3"), params![repo, t, n], get).optional()?,
                _ => None,
            })
        })
        .await
        .map_err(internal)?;
    let Some((id, object, ct, name)) = row else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
    let Some(obj) = crate::ci::services::cas::object(s.hub, &object).await.map_err(internal)? else { return Ok(srv::json(StatusCode::GONE, &json!({ "message": "The asset's content is gone" }))) };
    let _ = s.hub.store.call(move |c| Ok(c.execute("UPDATE ci_release_assets SET downloads = downloads + 1 WHERE id = ?1", [id])?)).await;
    let size = obj.size;
    let r = srv::response(StatusCode::OK, &ct, srv::channel_body(crate::ci::services::cas::read(s.hub, &obj, 0, size)));
    let r = srv::with_header(r, http::header::CONTENT_LENGTH, &size.to_string());
    Ok(srv::with_header(r, http::header::CONTENT_DISPOSITION, &format!("attachment; filename=\"{}\"", name.replace('"', "_"))))
}

pub enum AssetBy<'a> {
    Id(&'a str),
    Name(&'a str, &'a str),
}

/// `POST /api/uploads/repos/{o}/{r}/releases/{id}/assets?name=&label=`: the body is the asset.
pub async fn upload_asset(s: &Site<'_>, release: &str, req: Request) -> Result<Response, Fail> {
    let q = srv::query(&req);
    let Some(name) = q.get("name").filter(|n| !n.is_empty() && !n.contains('/')).cloned() else { return Ok(invalid("name is required")) };
    let label = q.get("label").cloned();
    let Some(r) = one_release(s, "id = ?2", Some(release.to_owned())).await.map_err(internal)? else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
    let ct = req.headers().get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_owned();
    let staged = crate::ci::services::cas::staging(s.hub).join(format!("asset-{}", crate::util::new_id()));
    let max = s.hub.cfg.ci.artifacts.max_size_gb.saturating_mul(1 << 30);
    if let Err(e) = crate::ci::services::blob::receive(req, &staged, max).await {
        return Ok(invalid(&format!("{e:#}")));
    }
    let obj = crate::ci::services::cas::put_file(s.hub, &staged).await.map_err(internal)?;
    let st = staged.clone();
    let _ = crate::rt::blocking(move || std::fs::remove_file(st)).await;
    let (release_id, digest, size, n, l, c) = (r.0, obj.digest, i64::try_from(obj.size).unwrap_or(i64::MAX), name.clone(), label.clone(), ct.clone());
    let made = s
        .hub
        .store
        .call(move |db| {
            let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM ci_release_assets WHERE release_id = ?1 AND name = ?2)", params![release_id, n], |x| x.get(0))?;
            if exists {
                return Ok(None);
            }
            let now = crate::util::now_unix();
            db.execute(
                "INSERT INTO ci_release_assets (release_id, name, label, content_type, size, object, uploader, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'github-actions[bot]', ?7)",
                params![release_id, n, l, c, size, digest, now],
            )?;
            Ok(Some((db.last_insert_rowid(), now)))
        })
        .await
        .map_err(internal)?;
    let Some((id, at)) = made else {
        return Ok(srv::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "message": "Validation Failed", "errors": [{ "resource": "ReleaseAsset", "code": "already_exists", "field": "name" }] })));
    };
    Ok(srv::json(StatusCode::CREATED, &asset_json(s, &r.1, &(id, name, label, ct, size, at, 0))))
}

// ------------------------------------------------------------------ dependency snapshots

/// `POST /repos/{o}/{r}/dependency-graph/snapshots`: a build's resolved dependencies (Gradle's,
/// Maven's, sbt's submission actions), validated as GitHub validates them and kept.
pub async fn dependency_snapshot(s: &Site<'_>, body: &Value) -> Result<Response, Fail> {
    let text = |v: Option<&Value>| v.and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_owned);
    if body.get("version").and_then(Value::as_i64).is_none() {
        return Ok(invalid("version is required"));
    }
    let (Some(sha), Some(git_ref)) = (text(body.get("sha")), text(body.get("ref"))) else { return Ok(invalid("sha and ref are required")) };
    let (Some(job), Some(detector)) = (body.get("job"), body.get("detector")) else { return Ok(invalid("job and detector are required")) };
    let (Some(correlator), Some(name)) = (text(job.get("correlator")), text(detector.get("name"))) else { return Ok(invalid("job.correlator and detector.name are required")) };
    if !git_ref.starts_with("refs/") {
        return Ok(invalid("ref must be a fully qualified ref (refs/heads/…)"));
    }
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(invalid("sha must be a full 40-character commit SHA"));
    }
    let manifests = body.get("manifests").and_then(Value::as_object).map_or(0, serde_json::Map::len);
    let (repo, snap) = (s.repo.id.clone(), body.to_string());
    let (sh, rf, jb, dt) = (sha, git_ref, correlator, name);
    let (id, at) = s
        .hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute(
                "INSERT INTO ci_dependency_snapshots (repo_id, sha, git_ref, job, detector, manifests, snapshot, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![repo, sh, rf, jb, dt, i64::try_from(manifests).unwrap_or(i64::MAX), snap, now],
            )?;
            Ok((c.last_insert_rowid(), now))
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(
        StatusCode::CREATED,
        &json!({ "id": id, "created_at": rfc3339(at), "result": "SUCCESS", "message": "Dependency results for the repo have been successfully updated." }),
    ))
}

