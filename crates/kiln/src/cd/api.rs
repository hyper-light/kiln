//! `/api/cd`: deployments over HTTP, for the UI, the CLI and scripts (docs/cd.md). Each route is
//! an operation of [`super::ops`], the table MCP and code mode serve too; `POST
//! /api/cd/ops/<name>` runs any operation by name with a JSON body.
//!
//! Apps are addressed by their id's two parts, `/api/cd/apps/<project>/<name>`, and one object
//! of an app's tree by a key of one path segment, `<group>~<kind>~<namespace>~<name>` (an empty
//! group or namespace stays empty: `~Pod~web~api-7d9f-x`, `rbac.authorization.k8s.io~ClusterRole~~kiln`).
//! Errors are `{error}` with the status that says what went wrong; mutations are audited.

use std::collections::HashMap;
use std::sync::Arc;

use http::{Method, StatusCode};
use hyper::body::Incoming;
use kiln_cd::obj::ResKey;
use serde_json::{Value, json};

use super::ops::{self, Cx, Fail};
use crate::http::server::{self as srv, Request, Response};
use crate::http::ws;
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
                "limit" | "max" | "max_apps" | "pr" => v.parse::<u64>().map(Value::from).unwrap_or(Value::Null),
                "hard" | "dry_run" | "prune" | "out_of_sync" | "fresh" | "previous" | "brief" => Value::Bool(v == "true" || v == "1"),
                "tail_lines" | "since_seconds" => v.parse::<u64>().map(Value::from).unwrap_or(Value::Null),
                _ => Value::String(v.clone()),
            };
            m.insert((*k).into(), x);
        }
    }
    Value::Object(m)
}

/// An object's key in a path: `<group>~<kind>~<namespace>~<name>`, or the tracking form
/// `<group>/<kind>:<namespace>/<name>`.
pub fn parse_key(seg: &str) -> Option<ResKey> {
    let ok = |k: &str, n: &str| !k.is_empty() && !n.is_empty();
    if seg.contains('~') {
        let parts: Vec<&str> = seg.split('~').collect();
        let [g, k, ns, n] = parts.as_slice() else { return None };
        return ok(k, n).then(|| ResKey::new(g, k, ns, n));
    }
    let (gk, nsn) = seg.split_once(':')?;
    let (g, k) = gk.rsplit_once('/')?;
    let (ns, n) = nsn.split_once('/')?;
    ok(k, n).then(|| ResKey::new(g, k, ns, n))
}

fn res_input(p: &str, n: &str, key: &str, q: &HashMap<String, String>, keys: &[&str]) -> Result<Value, Fail> {
    let k = parse_key(key).ok_or_else(|| bad(format!("{key} is not an object's key: <group>~<kind>~<namespace>~<name>")))?;
    let mut v = from_query(q, keys);
    if let Some(m) = v.as_object_mut() {
        m.insert("app".into(), json!(format!("{p}/{n}")));
        for (f, x) in [("group", &k.group), ("kind", &k.kind), ("namespace", &k.namespace), ("name", &k.name)] {
            m.insert(f.into(), json!(x.as_ref()));
        }
    }
    Ok(v)
}

/// Follows the logs of an object's pods over a WebSocket (docs/cd.md): the pods are found once,
/// then their lines arrive as they are written (`live::follow`).
fn follow_logs(cd: Arc<super::Cd>, input: Value, req: Request) -> Result<Response, Fail> {
    let (resp, upgrade) = ws::accept(req).map_err(|r| (r.status, r.reason.to_owned()))?;
    crate::rt::spawn(async move {
        let socket = match upgrade.await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "a log stream's upgrade failed");
                return;
            }
        };
        let found = async {
            let id = input.get("app").and_then(Value::as_str).unwrap_or("").to_owned();
            let f = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
            let k = ResKey::new(f("group"), f("kind"), f("namespace"), f("name"));
            let pod = Some(f("pod")).filter(|p| !p.is_empty());
            let (t, ids, _) = ops::log_pods(&cd, &id, &k, pod).await?;
            let q = super::live::LogQuery::from_json(&input);
            let pods: Vec<(String, String, String)> = ids
                .iter()
                .filter_map(|i| t.tree.nodes.iter().find(|n| n.id == *i))
                .filter_map(|p| Some((p.namespace.clone(), p.name.clone(), super::live::container_of(p, q.container.as_deref())?)))
                .collect();
            let name = cd.apps.read().get(&id).map(|e| e.def.destination.cluster.clone()).unwrap_or_default();
            let c = cd.clusters.read().get(&name).cloned().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, format!("cluster {name} is not connected")))?;
            Ok::<_, Fail>((c, pods, q))
        };
        match found.await {
            Ok((c, pods, q)) => {
                if let Err(e) = super::live::follow(c, pods, q, socket).await {
                    tracing::debug!(error = %format!("{e:#}"), "a log stream ended");
                }
            }
            Err((_, e)) => {
                let (mut tx, _) = socket.split();
                let _ = tx.send(ws::text(json!({"type": "error", "message": e}).to_string())).await;
                let _ = tx.close(None).await;
            }
        }
    });
    Ok(resp)
}

/// Routes `/api/cd/...` (`rest` is the path after `cd`).
pub async fn route(hub: &Arc<Hub>, rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let Some(cd) = super::get() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "deployments are starting".into()));
    };
    let method = req.method().clone();
    if let (&Method::GET, ["apps", p, n, "resources", key, "logs", "ws"]) = (&method, rest) {
        let input = res_input(p, n, key, q, &["container", "pod", "tail_lines", "since_seconds"])?;
        return follow_logs(cd, input, req);
    }
    let body: Incoming = req.into_body();
    let cx = Cx { hub: hub.clone(), cd, actor: "admin".into() };
    let app = |p: &str, n: &str| json!({"app": format!("{p}/{n}")});
    let (name, input) = match (&method, rest) {
        (&Method::GET, ["ops"]) => {
            let list: Vec<Value> = ops::ops()
                .iter()
                .map(|o| json!({"name": o.name, "summary": o.summary, "read_only": o.read_only, "destructive": o.destructive, "input": (o.input)()}))
                .collect();
            return Ok(srv::json(StatusCode::OK, &json!({"ops": list})));
        }
        (&Method::POST, ["ops", name]) => ((*name).to_owned(), json_body(body).await?),
        (&Method::GET, ["summary"]) => ("cd.summary".into(), json!({})),
        (&Method::GET, ["apps"]) => {
            ("cd.apps.list".into(), from_query(q, &["project", "cluster", "namespace", "environment", "sync", "health", "q", "limit", "cursor", "sort"]))
        }
        (&Method::GET, ["facets"]) => ("cd.apps.facets".into(), from_query(q, &["project", "cluster", "namespace", "environment", "sync", "health", "q"])),
        (&Method::POST, ["apps"]) => ("cd.apps.create".into(), json!({"definition": json_body(body).await?})),
        (&Method::POST, ["import"]) => {
            let bytes = srv::body_bytes(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
            ("cd.import".into(), json!({"manifests": String::from_utf8_lossy(&bytes)}))
        }
        (&Method::GET, ["apps", p, n]) => ("cd.apps.get".into(), app(p, n)),
        (&Method::DELETE, ["apps", p, n]) => ("cd.apps.delete".into(), with(app(p, n), "cascade", q.get("cascade").map(|c| json!(c)).unwrap_or(Value::Null))),
        (&Method::GET, ["apps", p, n, "resources"]) => ("cd.apps.resources".into(), with(app(p, n), "out_of_sync", json!(q.get("out_of_sync").is_some_and(|v| v == "true")))),
        (&Method::GET, ["apps", p, n, "diff"]) => ("cd.apps.diff".into(), app(p, n)),
        (&Method::GET, ["apps", p, n, "tree"]) => ("cd.apps.tree".into(), with(app(p, n), "fresh", json!(q.get("fresh").is_some_and(|v| v == "true" || v == "1")))),
        (&Method::GET, ["apps", p, n, "manifests"]) => ("cd.apps.manifests".into(), app(p, n)),
        (&Method::GET, ["apps", p, n, "resources", key]) => ("cd.apps.resource".into(), res_input(p, n, key, q, &[])?),
        (&Method::GET, ["apps", p, n, "resources", key, "events"]) => ("cd.apps.resource_events".into(), res_input(p, n, key, q, &[])?),
        (&Method::GET, ["apps", p, n, "resources", key, "logs"]) => {
            ("cd.apps.resource_logs".into(), res_input(p, n, key, q, &["container", "pod", "tail_lines", "since_seconds", "previous"])?)
        }
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
        (&Method::GET, ["plans"]) => {
            if q.contains_key("diff") {
                return Err(bad("plans of kiln's own diffs (D123) come with its review, which isn't built yet: plan the commit (commit=) or the stack (head=, base=)"));
            }
            let keys = ["repo", "commit", "head", "base", "trunk", "pr", "brief", "max_apps", "project", "cluster", "namespace", "environment", "q"];
            ("cd.plans.get".into(), from_query(q, &keys))
        }
        (&Method::POST, ["plans"]) => ("cd.plans.get".into(), json_body(body).await?),
        (&Method::GET, ["plans", "repos"]) => ("cd.plans.repos".into(), json!({})),
        (&Method::GET, ["clusters"]) => ("cd.clusters.list".into(), json!({})),
        (&Method::POST, ["clusters"]) => ("cd.clusters.add".into(), json_body(body).await?),
        (&Method::DELETE, ["clusters", name]) => ("cd.clusters.remove".into(), json!({"name": name})),
        (&Method::GET, ["repos"]) => ("cd.repos.list".into(), json!({})),
        (&Method::POST, ["repos"]) => ("cd.repos.add".into(), json_body(body).await?),
        (&Method::DELETE, ["repos"]) => ("cd.repos.remove".into(), json!({"url": q.get("url").cloned().unwrap_or_default()})),
        (&Method::GET, ["providers"]) => ("cd.providers.list".into(), json!({})),
        (&Method::GET, ["targets"]) => ("cd.targets.list".into(), json!({})),
        (&Method::GET, ["discover"]) => ("cd.discover".into(), from_query(q, &["provider", "account", "region", "project", "zone", "subscription", "resource-group", "environment", "scope", "name"])),
        (&Method::POST, ["validate"]) => ("cd.validate".into(), json_body(body).await?),
        (&Method::GET, ["images"]) => ("cd.images.list".into(), json!({})),
        (&Method::PUT, ["images", name]) => {
            let mut b = json_body(body).await?;
            if let Some(m) = b.as_object_mut() {
                m.insert("name".into(), json!(name));
            }
            ("cd.images.set".into(), b)
        }
        (&Method::DELETE, ["images", name]) => ("cd.images.set".into(), json!({"name": name, "kind": "local"})),
        _ => return Err((StatusCode::NOT_FOUND, "no such deployments route".into())),
    };
    let out = ops::call(cx, &name, input).await?;
    Ok(srv::json(StatusCode::OK, &out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_read_in_both_forms() {
        assert_eq!(parse_key("apps~Deployment~web~api"), Some(ResKey::new("apps", "Deployment", "web", "api")));
        assert_eq!(parse_key("~Pod~web~api-7d9f-x"), Some(ResKey::new("", "Pod", "web", "api-7d9f-x")));
        assert_eq!(
            parse_key("rbac.authorization.k8s.io~ClusterRole~~kiln"),
            Some(ResKey::new("rbac.authorization.k8s.io", "ClusterRole", "", "kiln"))
        );
        assert_eq!(parse_key("apps/Deployment:web/api"), Some(ResKey::new("apps", "Deployment", "web", "api")));
        assert_eq!(parse_key("/ConfigMap:web/cfg"), Some(ResKey::new("", "ConfigMap", "web", "cfg")));
        assert_eq!(parse_key("/PriorityClass:/kiln-control"), Some(ResKey::new("", "PriorityClass", "", "kiln-control")));
        for bad in ["", "Deployment", "apps~Deployment~web", "a~b~c~d~e", "~~web~api", "apps~Deployment~web~", "apps/Deployment:web"] {
            assert_eq!(parse_key(bad), None, "{bad}");
        }
    }
}
