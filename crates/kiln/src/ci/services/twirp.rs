//! GitHub's results service over Twirp (docs/ci.md W08-R05): `POST /twirp/<service>/<method>`
//! at the root of `ACTIONS_RESULTS_URL`'s origin (the toolkit drops any path), JSON in and
//! out, as `@actions/artifact` speaks it.
//!
//! The encoding follows protobuf's JSON mapping as the toolkit uses it:
//! - field names in snake_case;
//! - int64 values as strings;
//! - wrapper types (`StringValue`, `Int64Value`) as bare values;
//! - timestamps in RFC 3339, in UTC.
//!
//! Errors are Twirp's `{"code", "msg"}`, since the client parses every response as JSON.
//!
//! `ArtifactService` (W08-R08):
//! - CreateArtifact gives a signed upload URL (`blob`). The zip is uploaded as Azure block blobs
//!   and chunked into the store.
//! - FinalizeArtifact checks the size and `sha256:` hash and makes the artifact immutable.
//! - ListArtifacts, GetSignedArtifactURL and DeleteArtifact work within the leg's run.

use std::sync::Arc;

use http::StatusCode;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::blob::{self, Target};
use super::cas;
use crate::ci::token::Claims;
use crate::ci::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

const ARTIFACTS: &str = "github.actions.results.api.v1.ArtifactService";
/// How long upload and download URLs work.
const URL_TTL: i64 = 6 * 3600;

/// A Twirp error.
fn err(code: &str, msg: &str) -> Response {
    let status = match code {
        "not_found" | "bad_route" => StatusCode::NOT_FOUND,
        "already_exists" => StatusCode::CONFLICT,
        "invalid_argument" | "malformed" => StatusCode::BAD_REQUEST,
        "permission_denied" => StatusCode::FORBIDDEN,
        "unauthenticated" => StatusCode::UNAUTHORIZED,
        "failed_precondition" => StatusCode::PRECONDITION_FAILED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    srv::json(status, &json!({ "code": code, "msg": msg }))
}

/// `/twirp/<service>/<method>`.
pub async fn route(hub: &Arc<Hub>, service: &str, method: &str, req: Request) -> Result<Response, Fail> {
    if req.method() != http::Method::POST {
        return Ok(err("bad_route", "Twirp takes POST"));
    }
    let Some(claims) = token::of_request(hub, req.headers()) else { return Ok(err("unauthenticated", "a leg's ACTIONS_RUNTIME_TOKEN is needed")) };
    let origin = blob::origin(&req);
    let body = match srv::body_bytes(req.into_body(), 1 << 20).await {
        Ok(b) if b.is_empty() => json!({}),
        Ok(b) => match serde_json::from_slice::<Value>(&b) {
            Ok(v) => v,
            Err(e) => return Ok(err("malformed", &format!("the request isn't JSON: {e}"))),
        },
        Err(e) => return Ok(err("malformed", &format!("{e:#}"))),
    };
    let out = match (service, method) {
        (ARTIFACTS, "CreateArtifact") => create(hub, &claims, &body, &origin).await,
        (ARTIFACTS, "FinalizeArtifact") => finalize(hub, &claims, &body).await,
        (ARTIFACTS, "ListArtifacts") => list(hub, &claims, &body).await,
        (ARTIFACTS, "GetSignedArtifactURL") => signed_url(hub, &claims, &body, &origin).await,
        (ARTIFACTS, "DeleteArtifact") => delete(hub, &claims, &body).await,
        _ => return Ok(err("bad_route", &format!("no such method: {service}/{method}"))),
    };
    Ok(out.unwrap_or_else(|e| err("internal", &format!("{e:#}"))))
}

/// A field by its proto name, or its JSON camelCase name.
fn field<'a>(v: &'a Value, snake: &str) -> Option<&'a Value> {
    v.get(snake).or_else(|| {
        let camel: String = snake.split('_').enumerate().map(|(i, p)| if i == 0 { p.to_owned() } else { p.get(..1).unwrap_or_default().to_uppercase() + p.get(1..).unwrap_or_default() }).collect();
        v.get(&camel)
    })
}

/// A string, or a `StringValue` (bare, or `{"value": …}`).
fn string(v: &Value, name: &str) -> Option<String> {
    let f = field(v, name)?;
    f.as_str().or_else(|| f.get("value").and_then(Value::as_str)).map(str::to_owned)
}

/// An int64: a string or a number, possibly wrapped.
fn int(v: &Value, name: &str) -> Option<i64> {
    let f = field(v, name)?;
    let f = f.get("value").unwrap_or(f);
    f.as_i64().or_else(|| f.as_str().and_then(|s| s.parse().ok()))
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

fn parse_time(s: &str) -> Option<i64> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok().map(time::OffsetDateTime::unix_timestamp)
}

/// The refusal when a request names another run, or (where it matters) another job: a token
/// acts only for its own.
fn refused(c: &Claims, body: &Value, own_job: bool) -> Option<Response> {
    let run = string(body, "workflow_run_backend_id").unwrap_or_default();
    let job = string(body, "workflow_job_run_backend_id").unwrap_or_default();
    (run != c.run || (own_job && job != c.sub)).then(|| err("permission_denied", "this token belongs to another job"))
}

/// GitHub's artifact name rules.
pub fn valid_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("Provided artifact name input during validation is empty".into());
    }
    for bad in ['"', ':', '<', '>', '|', '*', '?', '\r', '\n', '\\', '/'] {
        if name.contains(bad) {
            return Err(format!("The artifact name is not valid: {name}. Contains the following character: {bad:?}. Invalid characters include: \" : < > | * ? \\r \\n \\ /."));
        }
    }
    Ok(())
}

async fn create(hub: &Hub, c: &Claims, body: &Value, origin: &str) -> anyhow::Result<Response> {
    if let Some(r) = refused(c, body, true) {
        return Ok(r);
    }
    let name = string(body, "name").unwrap_or_default();
    if let Err(e) = valid_name(&name) {
        return Ok(err("invalid_argument", &e));
    }
    let now = crate::util::now_unix();
    let max_days = i64::from(hub.cfg.ci.artifacts.retention_days.max(1));
    let latest = now + max_days * 86_400;
    let expires = string(body, "expires_at").and_then(|t| parse_time(&t)).map_or(latest, |t| t.clamp(now + 86_400, latest));
    let mime = string(body, "mime_type");
    let (run, leg, n) = (c.run.clone(), c.sub.clone(), name.clone());
    let made = hub
        .store
        .call(move |db| {
            let tx = db.transaction()?;
            let existing: Option<(i64, bool)> = tx.query_row("SELECT id, finalized = 1 FROM ci_artifacts WHERE run_id = ?1 AND name = ?2", params![run, n], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            match existing {
                Some((_, true)) => return Ok(Err("an artifact with this name already exists on the workflow run")),
                // An upload that never finished starts over.
                Some((id, false)) => {
                    tx.execute("DELETE FROM ci_artifacts WHERE id = ?1", [id])?;
                }
                None => {}
            }
            tx.execute(
                "INSERT INTO ci_artifacts (run_id, leg_id, name, created_at, expires_at, mime_type) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![run, leg, n, now, expires, mime],
            )?;
            let id = tx.last_insert_rowid();
            tx.commit()?;
            Ok(Ok(id))
        })
        .await?;
    let id = match made {
        Ok(id) => id,
        Err(why) => return Ok(err("already_exists", why)),
    };
    let url = blob::url(hub, origin, Target::Artifact(id), "w", URL_TTL);
    Ok(srv::json(StatusCode::OK, &json!({ "ok": true, "signed_upload_url": url })))
}

async fn finalize(hub: &Hub, c: &Claims, body: &Value) -> anyhow::Result<Response> {
    if let Some(r) = refused(c, body, true) {
        return Ok(r);
    }
    let name = string(body, "name").unwrap_or_default();
    let size = int(body, "size");
    let hash = string(body, "hash");
    let (run, n) = (c.run.clone(), name.clone());
    let row: Option<(i64, Option<String>, Option<i64>, bool)> = hub
        .store
        .call(move |db| Ok(db.query_row("SELECT id, object, size, finalized = 1 FROM ci_artifacts WHERE run_id = ?1 AND name = ?2", params![run, n], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?))
        .await?;
    let Some((id, object, stored, done)) = row else { return Ok(err("not_found", &format!("no artifact named {name} in this run"))) };
    if done {
        return Ok(err("already_exists", "the artifact is already finalized"));
    }
    let Some(object) = object else { return Ok(err("failed_precondition", "the artifact's upload is not complete")) };
    if let (Some(want), Some(have)) = (size, stored)
        && want != have
    {
        return Ok(err("invalid_argument", &format!("the artifact is {have} bytes; finalize says {want}")));
    }
    let digest = format!("sha256:{object}");
    if let Some(h) = hash.filter(|h| !h.is_empty())
        && !h.eq_ignore_ascii_case(&digest)
    {
        return Ok(err("invalid_argument", &format!("the artifact's content hashes to {digest}, not {h}")));
    }
    hub.store.call(move |db| Ok(db.execute("UPDATE ci_artifacts SET finalized = 1, digest = ?2 WHERE id = ?1", params![id, digest])?)).await?;
    Ok(srv::json(StatusCode::OK, &json!({ "ok": true, "artifact_id": id.to_string() })))
}

/// A finalized, unexpired artifact of the run.
struct Row {
    id: i64,
    leg: String,
    name: String,
    size: i64,
    created_at: i64,
    digest: Option<String>,
}

async fn rows(hub: &Hub, run: &str, name: Option<String>, id: Option<i64>) -> anyhow::Result<Vec<Row>> {
    let run = run.to_owned();
    hub.store
        .call(move |db| {
            let mut q = db.prepare(
                "SELECT id, COALESCE(leg_id, ''), name, COALESCE(size, 0), created_at, digest FROM ci_artifacts
                 WHERE run_id = ?1 AND finalized = 1 AND (expires_at IS NULL OR expires_at > ?2)
                   AND (?3 IS NULL OR name = ?3) AND (?4 IS NULL OR id = ?4) ORDER BY id",
            )?;
            let r = q.query_map(params![run, crate::util::now_unix(), name, id], |r| Ok(Row { id: r.get(0)?, leg: r.get(1)?, name: r.get(2)?, size: r.get(3)?, created_at: r.get(4)?, digest: r.get(5)? }))?;
            Ok(r.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

async fn list(hub: &Hub, c: &Claims, body: &Value) -> anyhow::Result<Response> {
    if let Some(r) = refused(c, body, false) {
        return Ok(r);
    }
    let found = rows(hub, &c.run, string(body, "name_filter"), int(body, "id_filter")).await?;
    let artifacts: Vec<Value> = found
        .iter()
        .map(|a| {
            let mut v = json!({
                "workflow_run_backend_id": c.run, "workflow_job_run_backend_id": a.leg, "database_id": a.id.to_string(),
                "name": a.name, "size": a.size.to_string(), "created_at": rfc3339(a.created_at),
            });
            if let (Some(d), Some(o)) = (&a.digest, v.as_object_mut()) {
                o.insert("digest".into(), json!(d));
            }
            v
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "artifacts": artifacts })))
}

async fn signed_url(hub: &Hub, c: &Claims, body: &Value, origin: &str) -> anyhow::Result<Response> {
    if let Some(r) = refused(c, body, false) {
        return Ok(r);
    }
    let name = string(body, "name").unwrap_or_default();
    let found = rows(hub, &c.run, Some(name.clone()), None).await?;
    let Some(a) = found.last() else { return Ok(err("not_found", &format!("no artifact named {name} in this run"))) };
    Ok(srv::json(StatusCode::OK, &json!({ "signed_url": blob::url(hub, origin, Target::Artifact(a.id), "r", URL_TTL) })))
}

async fn delete(hub: &Hub, c: &Claims, body: &Value) -> anyhow::Result<Response> {
    if let Some(r) = refused(c, body, false) {
        return Ok(r);
    }
    let name = string(body, "name").unwrap_or_default();
    let (run, n) = (c.run.clone(), name.clone());
    let id: Option<i64> = hub
        .store
        .call(move |db| {
            let id: Option<i64> = db.query_row("SELECT id FROM ci_artifacts WHERE run_id = ?1 AND name = ?2", params![run, n], |r| r.get(0)).optional()?;
            if let Some(id) = id {
                db.execute("DELETE FROM ci_artifacts WHERE id = ?1", [id])?;
            }
            Ok(id)
        })
        .await?;
    let Some(id) = id else { return Ok(err("not_found", &format!("no artifact named {name} in this run"))) };
    let dir = cas::staging(hub).join(Target::Artifact(id).id());
    let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
    Ok(srv::json(StatusCode::OK, &json!({ "ok": true, "artifact_id": id.to_string() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w08_r05_fields_as_the_toolkit_encodes_them() {
        let v = json!({ "workflow_run_backend_id": "r", "size": "42", "id_filter": "7", "name_filter": "dist", "hash": { "value": "sha256:ab" }, "expiresAt": "2026-10-18T00:00:00.000Z" });
        assert_eq!(string(&v, "workflow_run_backend_id").as_deref(), Some("r"));
        assert_eq!(int(&v, "size"), Some(42));
        assert_eq!(int(&v, "id_filter"), Some(7));
        assert_eq!(string(&v, "hash").as_deref(), Some("sha256:ab"));
        assert_eq!(string(&v, "expires_at").and_then(|t| parse_time(&t)), Some(1_792_281_600));
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn w08_r08_artifact_names_as_github_checks_them() {
        assert!(valid_name("dist-linux_x64.1").is_ok());
        for bad in ["a/b", "a:b", "a*b", "", "  ", "a\nb", "a\\b"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
    }
}
