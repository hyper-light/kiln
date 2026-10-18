//! `POST /api/ci/plan-preview` (docs/ci.md W04-R29): what a workflow would run for an event,
//! without running it. The builder (W21) and agents (W20) send the workflow being edited
//! (`content`), or name one of a commit's (`path`):
//!
//! ```json
//! { "repo": "acme/app", "ref": "main", "sha": "…", "path": ".github/workflows/ci.yml",
//!   "content": "…", "event": "push" | { "name": "…", "payload": {…}, "inputs": {…} } }
//! ```
//!
//! The answer: the workflow's diagnostics, whether the event starts it (and why not), and the
//! plan: legs with their values and routing against the runners connected now, what was decided
//! and why, combinations removed, and jobs decided only when their needs finish.

use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use kiln_ci::expr::Dialect;
use serde_json::{Value, json};

use super::Fail;
use super::dispatch::{Msg, Start};
use super::{events, load};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

pub async fn route(hub: &Arc<Hub>, caller: Option<&crate::authz::Caller>, body: hyper::body::Incoming) -> Result<Response, Fail> {
    let bad = |m: String| (StatusCode::BAD_REQUEST, m);
    if !hub.ci.enabled() {
        return Err((StatusCode::NOT_FOUND, "The CI engine is off ([ci] enabled).".into()));
    }
    let b: Value = srv::body_json(body, 4 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    let name = b.get("repo").and_then(Value::as_str).ok_or_else(|| bad("`repo` is required".into()))?;
    let repos = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let repo = repos.into_iter().find(|r| r.name.eq_ignore_ascii_case(name)).ok_or_else(|| (StatusCode::NOT_FOUND, format!("No repository {name}")))?;
    // The route is open to anyone who reads runs somewhere; this repository must be one of theirs.
    if caller.is_some_and(|c| !c.can("runs.read", &crate::authz::Scope::Repo { repo: repo.name.clone() })) {
        return Err((StatusCode::NOT_FOUND, format!("No repository {name}")));
    }
    let refused = |r: events::Refused| (r.status, r.message);
    let (full_ref, sha) = events::preview_ref(hub, &repo, b.get("ref").and_then(Value::as_str).unwrap_or_default(), b.get("sha").and_then(Value::as_str)).await.map_err(refused)?;
    let path = b.get("path").and_then(Value::as_str).map(str::to_owned);
    // The workflow: as edited, or the commit's.
    let (path, workflow, diagnostics) = match b.get("content").and_then(Value::as_str) {
        Some(text) => {
            let p = path.unwrap_or_else(|| ".github/workflows/preview.yml".into());
            let dialect = if p.starts_with(".kiln/") { Dialect::Kiln } else { Dialect::Gha };
            let (text, q) = (text.to_owned(), p.clone());
            let (w, d) = crate::rt::blocking(move || kiln_ci::wf::parse_workflow(&q, &text, dialect)).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            (p, w, load::stored(&d))
        }
        None => {
            let p = path.ok_or_else(|| bad("`path` or `content` is required".into()))?;
            let commit = load::load(hub, &repo, &sha).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            let l = commit.workflows.into_iter().find(|l| l.path == p || l.path.rsplit('/').next() == Some(p.as_str())).ok_or_else(|| (StatusCode::NOT_FOUND, format!("No workflow {p} at {sha}")))?;
            (l.path, l.workflow, l.diagnostics)
        }
    };
    let errors = diagnostics.iter().any(|d| d.severity == kiln_ci::wf::Severity::Error);
    let base = json!({ "repo": repo.name, "ref": full_ref, "sha": sha, "path": path, "diagnostics": diagnostics });
    let (Some(w), false) = (workflow, errors) else {
        return Ok(srv::json(StatusCode::OK, &merge(base, json!({ "runs": false, "why": "the workflow has errors", "plan": Value::Null }))));
    };
    let (event_name, payload, given) = match b.get("event") {
        Some(Value::Object(o)) => (o.get("name").and_then(Value::as_str).unwrap_or("push").to_owned(), o.get("payload").cloned(), o.get("inputs").and_then(Value::as_object).cloned().unwrap_or_default()),
        Some(Value::String(s)) => (s.clone(), None, b.get("inputs").and_then(Value::as_object).cloned().unwrap_or_default()),
        _ => ("push".to_owned(), None, b.get("inputs").and_then(Value::as_object).cloned().unwrap_or_default()),
    };
    let declared = w.on.iter().find(|t| t.event == "workflow_dispatch").map(|t| t.inputs.clone());
    let (event, inputs) = events::preview_event(hub, &repo, &event_name, &full_ref, &sha, payload.as_ref(), &given, declared.as_deref(), &path).await.map_err(refused)?;
    let matched = kiln_ci::trigger::matches(&w, &event);
    // What the action policies refuse among its own `uses:` (a run checks the nested ones too).
    let policy = super::actions::refusals(&super::actions::policies(hub, &repo).await, super::actions::uses_of(&w));
    let (tx, rx) = async_channel::bounded(1);
    let start = Start { repo, event, path, workflow: Arc::new(w), message: String::new(), author: String::new(), actions: Default::default(), inputs, given: Default::default() };
    hub.ci.send(Msg::Preview { start: Box::new(start), reply: tx });
    let plan = match crate::rt::timeout(Duration::from_secs(60), rx.recv()).await {
        Ok(Ok(p)) => p,
        _ => return Err((StatusCode::SERVICE_UNAVAILABLE, "The CI dispatcher didn't answer.".into())),
    };
    let why = matched.as_ref().err().map(ToString::to_string);
    Ok(srv::json(StatusCode::OK, &merge(base, json!({ "event": event_name, "runs": matched.is_ok() && policy.is_empty(), "why": why, "policy": policy, "plan": plan }))))
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
        a.extend(b);
    }
    a
}
