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
use kiln_http::Incoming;
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

pub(super) fn with(mut v: Value, k: &str, x: Value) -> Value {
    if let Some(m) = v.as_object_mut() {
        m.insert(k.into(), x);
    }
    v
}

pub(super) fn from_query(q: &HashMap<String, String>, keys: &[&str]) -> Value {
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

pub(super) fn res_input(p: &str, n: &str, key: &str, q: &HashMap<String, String>, keys: &[&str]) -> Result<Value, Fail> {
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
/// then their lines arrive as they are written (`live::follow`). What the page sends up that can't
/// be read is answered `refused` and noted from its person.
fn follow_logs(hub: Arc<Hub>, cd: Arc<super::Cd>, input: Value, req: Request) -> Result<Response, Fail> {
    use crate::wire::unread::Wire;
    let person = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
    let sock = crate::wire::ui::Socket::of(Wire::LogUp, Wire::LogDown, &person, None);
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
                if let Err(e) = super::live::follow(&hub, &sock, c, pods, q, socket).await {
                    tracing::debug!(error = %format!("{e:#}"), "a log stream ended");
                }
            }
            Err((_, e)) => {
                let (mut tx, _) = socket.split();
                let _ = tx.send(ws::text(crate::wire::ui::CdLogDown::Error { pod: String::new(), message: e }.text())).await;
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
        return follow_logs(Arc::clone(hub), cd, input, req);
    }
    // U12: legacy URLs retain the same caller identity and agent restrictions as
    // the named HTTP/MCP operation, including the domain's own audit records.
    let actor = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |caller| caller.name.clone());
    let body: Incoming = req.into_body();
    let cx = Cx { hub: hub.clone(), cd, actor };
    if method == Method::GET && rest == ["ops"] {
        let list: Vec<Value> = ops::ops().iter().map(|o| json!({"name": o.name, "summary": o.summary, "read_only": o.read_only, "destructive": o.destructive, "input": o.input.schema()})).collect();
        return Ok(srv::json(StatusCode::OK, &json!({"ops": list})));
    }
    let (name, input) = if let (&Method::POST, ["ops", name]) = (&method, rest) {
        ((*name).to_owned(), json_body(body).await?)
    } else {
        let (op, alias) = ops::http_alias(&method, rest)?;
        let value = match alias.body {
            crate::ops::Body::Ignore => json!({}),
            crate::ops::Body::Json => json_body(body).await?,
            crate::ops::Body::Text => {
                let bytes = srv::body_bytes(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
                json!(String::from_utf8_lossy(&bytes))
            }
        };
        (op.name.to_owned(), (alias.input)(rest, q, &value)?)
    };
    let out = ops::call(cx, &name, input).await?;
    Ok(srv::json(StatusCode::OK, &out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u12_cd_aliases_dispatch_and_authorize_from_their_descriptors() -> Result<(), Fail> {
        for op in ops::ops() {
            for alias in op.route.aliases {
                let method = Method::from_bytes(alias.method.as_bytes()).map_err(|e| bad(e.to_string()))?;
                let rest: Vec<&str> = alias.segments.iter().map(|segment| if *segment == "*" { "sample" } else { *segment }).collect();
                assert_eq!(ops::http_alias(&method, &rest)?.0.name, op.name);
                let mut full = vec!["api", "cd"];
                full.extend(&rest);
                assert_eq!(crate::authz::rule(&method, &full).capability, op.route.authorization.capability);
                assert_eq!(crate::authz::rule(&Method::POST, &["api", "cd", "ops", op.name]).capability, op.route.authorization.capability);
            }
        }
        let (op, alias) = ops::http_alias(&Method::POST, &["apps", "team", "web", "sync"])?;
        assert_eq!(op.name, "cd.apps.sync");
        let input = (alias.input)(&["apps", "team", "web", "sync"], &HashMap::new(), &json!({"app": "other/app", "prune": true}))?;
        assert_eq!(input, json!({"app": "team/web", "prune": true}));
        let (read, alias) = ops::http_alias(&Method::GET, &["apps", "team", "web"])?;
        assert_eq!((alias.input)(&["apps", "team", "web"], &HashMap::new(), &Value::Null)?, json!({"app": "team/web"}));
        let reader = crate::authz::Caller::with(crate::authz::gate::Kind::User, "reader", Some(crate::authz::Rights {
            grants: vec![crate::authz::Grant::parse("runs.read").map_err(bad)?], custom: HashMap::new(),
        }), None);
        assert!(crate::authz::gate::decide(&reader, &read.route.authorization, Some(&crate::authz::Scope::Server)));
        assert!(!crate::authz::gate::decide(&reader, &op.route.authorization, Some(&crate::authz::Scope::Server)));
        let doc = crate::authz::openapi("test", &crate::ops::listed(true));
        assert_eq!(doc.pointer("/paths/~1api~1cd~1apps~1{segment_1}~1{segment_2}~1sync/post/x-kiln-operation"), Some(&json!("cd.apps.sync")));
        Ok(())
    }

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
