//! Every operation, once (docs/ci.md W20-R01): deployments' ([`crate::cd::ops`]), releases by
//! API ([`crate::deploy::ops`]) and CI's ([`crate::ci::ops`]), each with its name, summary, input
//! schema and whether it changes anything. The MCP tools and code mode's `kiln.*` are this list. Each operation is checked
//! against the route table as its HTTP route is, so a caller may do through them exactly what it
//! may do over HTTP.

use std::sync::Arc;

use http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::authz::Caller;
use crate::hub::Hub;

/// A failure: an HTTP status and why.
pub type Fail = (StatusCode, String);
/// An operation's answer.
pub type Out = Result<Value, Fail>;

/// Who calls.
#[derive(Clone)]
pub struct Cx {
    pub hub: Arc<Hub>,
    /// As the route table sees it; its name is who the audit trail says acted.
    pub caller: Caller,
}

/// What an operation is, as lists and tools show it.
pub struct Info {
    pub name: &'static str,
    pub summary: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub input: fn() -> Value,
}

/// Every operation this server has: CI's, and deployments' when they're on.
pub fn all(hub: &Hub) -> Vec<Info> {
    listed(hub.cfg.cd.enabled)
}

/// CI's operations, and deployments' and releases' with `cd`.
pub fn listed(cd: bool) -> Vec<Info> {
    let mut out: Vec<Info> = Vec::new();
    if cd {
        out.extend(crate::cd::ops::ops().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
        out.extend(crate::deploy::ops::ops().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
    }
    out.extend(crate::ci::ops::ops().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
    out
}

/// Runs operation `name` for `cx`'s caller, when the route table lets it.
pub async fn call(cx: Cx, name: &str, input: Value) -> Out {
    let input = if input.is_null() { json!({}) } else { input };
    if name.starts_with("ci.") {
        return crate::ci::ops::call(cx, name, input).await;
    }
    if name.starts_with("deploy.") {
        return crate::deploy::ops::call(cx, name, input).await;
    }
    let op = crate::cd::ops::ops().iter().find(|o| o.name == name).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no operation {name}")))?;
    // Deployments' routes: reading needs runs.read, the rest environments.deploy.
    let method = if op.read_only { Method::GET } else { Method::POST };
    let run = async {
        crate::authz::gate::check(&cx.hub, &cx.caller, &method, &["api", "cd", "ops", name], &Default::default()).await?;
        let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "deployments aren't on here, or are starting ([cd] enabled)".to_owned()))?;
        crate::cd::ops::call(crate::cd::ops::Cx { hub: Arc::clone(&cx.hub), cd, actor: cx.caller.name.clone() }, name, input).await
    };
    if op.read_only {
        return run.await;
    }
    // A change is audited as its route's would be (docs/ci.md W19-R05): one row.
    let path = format!("/api/cd/ops/{name}");
    crate::audit::request(&cx.hub, crate::audit::Req { caller: &cx.caller, method: &method, path: &path, q: &Default::default(), ip: None }, run, |r: &Out| match r {
        Ok(_) => 200,
        Err((s, _)) => s.as_u16(),
    })
    .await
}
