//! `/api/cd`: deployments over HTTP, for the UI, the CLI and scripts (docs/cd.md). Each route is
//! an operation of [`super::ops`], the table MCP and code mode serve too; `POST
//! /api/cd/ops/<name>` runs any operation by name with a JSON body.
//!
//! Apps are addressed by their id's two parts, `/api/cd/apps/<project>/<name>`. Errors are
//! `{error}` with the status that says what went wrong; mutations are audited.

use std::collections::HashMap;
use std::sync::Arc;

use http::{Method, StatusCode};
use hyper::body::Incoming;
use serde_json::{Value, json};

use super::ops::{self, Cx, Fail};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

const LIMIT: usize = 8 << 20;

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

async fn json_body(body: Incoming) -> Result<Value, Fail> {
    let bytes = srv::body_bytes(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|e| bad(format!("the body is not JSON: {e}")))
}

fn with(mut v: Value, k: &str, x: Value) -> Value {
    if let Some(m) = v.as_object_mut() {
        m.insert(k.into(), x);
    }
    v
}

fn from_query(q: &HashMap<String, String>, keys: &[&str]) -> Value {
    let mut m = serde_json::Map::new();
    for k in keys {
        if let Some(v) = q.get(*k) {
            let x = match *k {
                "limit" | "max" => v.parse::<u64>().map(Value::from).unwrap_or(Value::Null),
                "hard" | "dry_run" | "prune" | "out_of_sync" => Value::Bool(v == "true" || v == "1"),
                _ => Value::String(v.clone()),
            };
            m.insert((*k).into(), x);
        }
    }
    Value::Object(m)
}

/// Routes `/api/cd/...` (`rest` is the path after `cd`).
pub async fn route(hub: &Arc<Hub>, method: &Method, rest: &[&str], q: &HashMap<String, String>, body: Incoming) -> Result<Response, Fail> {
    let Some(cd) = super::get() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "deployments are starting".into()));
    };
    let cx = Cx { hub: hub.clone(), cd, actor: "admin".into() };
    let app = |p: &str, n: &str| json!({"app": format!("{p}/{n}")});
    let (name, input) = match (method, rest) {
        (&Method::GET, ["ops"]) => {
            let list: Vec<Value> = ops::ops()
                .iter()
                .map(|o| json!({"name": o.name, "summary": o.summary, "read_only": o.read_only, "destructive": o.destructive, "input": (o.input)()}))
                .collect();
            return Ok(srv::json(StatusCode::OK, &json!({"ops": list})));
        }
        (&Method::POST, ["ops", name]) => ((*name).to_owned(), json_body(body).await?),
        (&Method::GET, ["summary"]) => ("cd.summary".into(), json!({})),
        (&Method::GET, ["apps"]) => ("cd.apps.list".into(), from_query(q, &["project", "cluster", "environment", "sync", "health", "q", "limit", "cursor"])),
        (&Method::GET, ["facets"]) => ("cd.apps.facets".into(), from_query(q, &["project", "cluster", "environment", "sync", "health", "q"])),
        (&Method::POST, ["apps"]) => ("cd.apps.create".into(), json!({"definition": json_body(body).await?})),
        (&Method::POST, ["import"]) => {
            let bytes = srv::body_bytes(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
            ("cd.import".into(), json!({"manifests": String::from_utf8_lossy(&bytes)}))
        }
        (&Method::GET, ["apps", p, n]) => ("cd.apps.get".into(), app(p, n)),
        (&Method::DELETE, ["apps", p, n]) => ("cd.apps.delete".into(), with(app(p, n), "cascade", q.get("cascade").map(|c| json!(c)).unwrap_or(Value::Null))),
        (&Method::GET, ["apps", p, n, "resources"]) => ("cd.apps.resources".into(), with(app(p, n), "out_of_sync", json!(q.get("out_of_sync").is_some_and(|v| v == "true")))),
        (&Method::GET, ["apps", p, n, "diff"]) => ("cd.apps.diff".into(), app(p, n)),
        (&Method::GET, ["apps", p, n, "history"]) => ("cd.apps.history".into(), app(p, n)),
        (&Method::POST, ["apps", p, n, "sync"]) => {
            let mut b = json_body(body).await?;
            if let Some(m) = b.as_object_mut() {
                m.insert("app".into(), json!(format!("{p}/{n}")));
            }
            ("cd.apps.sync".into(), b)
        }
        (&Method::POST, ["apps", p, n, "refresh"]) => ("cd.apps.refresh".into(), with(app(p, n), "hard", json!(q.get("hard").is_some_and(|v| v == "true" || v == "1")))),
        (&Method::POST, ["apps", p, n, "terminate"]) => ("cd.apps.terminate".into(), app(p, n)),
        (&Method::POST, ["apps", p, n, "take-over"]) => ("cd.apps.take_over".into(), app(p, n)),
        (&Method::POST, ["apps", p, n, "rollback"]) => {
            let mut b = json_body(body).await?;
            if let Some(m) = b.as_object_mut() {
                m.insert("app".into(), json!(format!("{p}/{n}")));
            }
            ("cd.apps.rollback".into(), b)
        }
        (&Method::POST, ["apps", p, n, "follow"]) => ("cd.apps.follow".into(), app(p, n)),
        (&Method::GET, ["clusters"]) => ("cd.clusters.list".into(), json!({})),
        (&Method::POST, ["clusters"]) => ("cd.clusters.add".into(), json_body(body).await?),
        (&Method::DELETE, ["clusters", name]) => ("cd.clusters.remove".into(), json!({"name": name})),
        (&Method::GET, ["repos"]) => ("cd.repos.list".into(), json!({})),
        (&Method::POST, ["repos"]) => ("cd.repos.add".into(), json_body(body).await?),
        (&Method::DELETE, ["repos"]) => ("cd.repos.remove".into(), json!({"url": q.get("url").cloned().unwrap_or_default()})),
        _ => return Err((StatusCode::NOT_FOUND, "no such deployments route".into())),
    };
    let out = ops::call(cx, &name, input).await?;
    Ok(srv::json(StatusCode::OK, &out))
}
