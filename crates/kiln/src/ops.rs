//! Every operation, once (docs/ci.md W20-R01): deployments' ([`crate::cd::ops`]), releases by
//! API ([`crate::deploy::ops`]), CI's ([`crate::ci::ops`]) and kiln's own: agents'
//! ([`crate::harness::ops`]), plans' ([`crate::plans::ops`]) and goal stacks'
//! ([`crate::goals::ops`]). Each has its name, summary, input schema and whether it changes
//! anything. The MCP tools and code mode's `kiln.*` are this list. Each operation is checked
//! against the route table as its HTTP route is, so a caller may do through them exactly what it
//! may do over HTTP.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::authz::Caller;
use crate::hub::Hub;

/// A failure: an HTTP status and why.
pub type Fail = (StatusCode, String);
/// An operation's answer.
pub type Out = Result<Value, Fail>;
pub(crate) type Fut = Pin<Box<dyn Future<Output = Out> + Send>>;

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

/// One of kiln's own operations (agents, plans, goal stacks): what it is, the HTTP route it is
/// (method and path, for the route table and the audit trail), and what it does.
pub struct Op {
    /// Its name, dotted (`agents.start`).
    pub name: &'static str,
    pub summary: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub input: fn() -> Value,
    /// Its route, from the input: method and path segments (`["api", "harness", "agents"]`).
    pub route: fn(&Value) -> Result<(Method, Vec<String>), Fail>,
    pub run: fn(Cx, Value) -> Fut,
}

/// A string field of an input, required.
pub fn text(v: &Value, k: &str) -> Result<String, Fail> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("`{k}` is required")))
}

/// An input's JSON Schema: an object with these properties.
pub fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

/// A route's path, as segments.
pub fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// kiln's own operations: agents', plans', goal stacks', runners', attachments', and access and
/// the audit trail's, in the order they're listed.
pub fn own() -> &'static [Op] {
    static OPS: std::sync::OnceLock<Vec<Op>> = std::sync::OnceLock::new();
    OPS.get_or_init(|| {
        crate::harness::ops::list_ops()
            .into_iter()
            .chain(crate::plans::ops::list_ops())
            .chain(crate::goals::ops::list_ops())
            .chain(crate::research::ops::list_ops())
            .chain(crate::doctor::list_ops())
            .chain(crate::ci::attach::list_ops())
            .chain(crate::authz::ops::list_ops())
            .collect()
    })
}

/// Every operation this server has: CI's, deployments' when they're on, and kiln's own.
pub fn all(hub: &Hub) -> Vec<Info> {
    listed(hub.cfg.cd.enabled)
}

/// The operations as `GET /api/ops` lists them (W20-R01): each one's name, summary, whether it
/// changes anything, and its input's JSON Schema.
pub fn catalog(hub: &Hub) -> Value {
    let ops: Vec<Value> = all(hub).iter().map(|o| json!({ "name": o.name, "summary": o.summary, "read_only": o.read_only, "destructive": o.destructive, "input": (o.input)() })).collect();
    json!({ "operations": ops })
}

/// CI's operations, deployments' and releases' with `cd`, and kiln's own.
pub fn listed(cd: bool) -> Vec<Info> {
    let mut out: Vec<Info> = Vec::new();
    if cd {
        out.extend(crate::cd::ops::ops().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
        out.extend(crate::deploy::ops::ops().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
    }
    out.extend(crate::ci::ops::ops().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
    out.extend(own().iter().map(|o| Info { name: o.name, summary: o.summary, read_only: o.read_only, destructive: o.destructive, input: o.input }));
    out
}

/// Operations on one agent that an agent calling them without `agent` means for itself: its own
/// goal stack, and putting its own work up for review.
const YOURS: &[&str] = &["goals.get", "goals.add", "goals.set", "goals.control", "goals.done", "goals.update", "goals.split", "goals.plan", "agents.change"];

/// `input` to operation `name` as `caller` means it: an agent's own id where it left out `agent`
/// on one of [`YOURS`].
fn yours(caller: &Caller, name: &str, mut input: Value) -> Value {
    if YOURS.contains(&name)
        && let (Some(me), Some(o)) = (caller.agent.as_deref(), input.as_object_mut())
        && o.get("agent").and_then(Value::as_str).is_none_or(|a| a.trim().is_empty())
    {
        o.insert("agent".into(), json!(me));
    }
    input
}

/// Runs operation `name` for `cx`'s caller, when the route table lets it.
pub async fn call(cx: Cx, name: &str, input: Value) -> Out {
    let input = yours(&cx.caller, name, if input.is_null() { json!({}) } else { input });
    // CI's, and the hosts' pull requests (W23-R06) and experiments (W20-R11) beside them.
    if name.starts_with("ci.") || name.starts_with("host.") || name.starts_with("experiments.") {
        return crate::ci::ops::call(cx, name, input).await;
    }
    if name.starts_with("deploy.") {
        return crate::deploy::ops::call(cx, name, input).await;
    }
    if let Some(op) = own().iter().find(|o| o.name == name) {
        let (method, segs) = (op.route)(&input)?;
        let path = format!("/{}", segs.join("/"));
        let run = async {
            let s: Vec<&str> = segs.iter().map(String::as_str).collect();
            crate::authz::gate::check(&cx.hub, &cx.caller, &method, &s, &Default::default()).await?;
            (op.run)(cx.clone(), input).await
        };
        if op.read_only {
            return run.await;
        }
        // A change is audited as its route's would be (docs/ci.md W19-R05): one row.
        return crate::audit::request(&cx.hub, crate::audit::Req { caller: &cx.caller, method: &method, path: &path, q: &Default::default(), ip: None }, run, |r: &Out| match r {
            Ok(_) => 200,
            Err((s, _)) => s.as_u16(),
        })
        .await;
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

/// Runs one of kiln's own operations for its HTTP route, which the route table has let through
/// and the audit trail records already.
pub async fn run_own(cx: Cx, name: &str, input: Value) -> Out {
    let op = own().iter().find(|o| o.name == name).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no operation {name}")))?;
    (op.run)(cx, input).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of kiln's own operations is a route the table names (none falls through to the
    /// administrators' `settings.manage`), needs what its route needs (agents' `agents.run`,
    /// runners' `runners.read` or `runners.manage`), and reads exactly when its route is a GET.
    #[test]
    fn kiln_s_own_operations_are_routes_of_the_table() {
        let probe = json!({"agent": "a1", "id": "p1", "run": "r1", "step": 1, "action": "start", "text": "x", "goals": ["g"], "runner": "mac", "policy": "auto", "attachment": "ci-doctor", "incident": "i1", "user": "ada", "grant": "g1", "name": "auditor", "token": "t1"});
        assert!(own().iter().any(|o| o.name == "agents.start") && own().iter().any(|o| o.name == "plans.run") && own().iter().any(|o| o.name == "goals.add"));
        for op in own() {
            let route = (op.route)(&probe);
            assert!(route.is_ok(), "{}", op.name);
            let Ok((method, segs)) = route else { continue };
            let s: Vec<&str> = segs.iter().map(String::as_str).collect();
            let rule = crate::authz::rule(&method, &s);
            assert_ne!(rule.pattern, crate::authz::UNNAMED.pattern, "{} ({method} /{}) has a rule of its own", op.name, s.join("/"));
            let needs = match op.name {
                "runners.list" | "runners.diagnose" | "updates.list" => "runners.read",
                "runners.repair" | "updates.retry" | "updates.policy" => "runners.manage",
                // Attachments and incidents: reading them is reading runs (each narrowed to what it
                // looks after); attaching is `agents.attach`; reporting, the incident's own check.
                "attachments.list" | "attachments.get" | "incidents.list" | "incidents.get" => "runs.read",
                "attachments.save" | "attachments.remove" | "attachments.pause" => "agents.attach",
                "incidents.report" => crate::authz::SIGNED_IN,
                // Access: people, grants and custom roles are `users.manage`'s; tokens are each
                // caller's own (anyone's with `tokens.manage`); the audit trail is `audit.read`'s.
                "roles.list" => crate::authz::SIGNED_IN,
                n if n.starts_with("users.") || n.starts_with("grants.") || n.starts_with("roles.") => "users.manage",
                n if n.starts_with("tokens.") => "tokens.manage",
                "audit.list" => "audit.read",
                _ => "agents.run",
            };
            assert_eq!(rule.capability, needs, "{}", op.name);
            assert_eq!(op.read_only, method == Method::GET, "{}: read-only exactly when its route reads", op.name);
            assert!((op.input)().get("type").is_some(), "{}", op.name);
        }
        // Listed whether deployments are on or not.
        assert!(listed(false).iter().any(|o| o.name == "agents.wait"));
        assert!(listed(false).iter().any(|o| o.name == "plans.create"));
    }

    /// An agent's own goal stack and change request need no id: left out, `agent` is the agent
    /// calling. One it names stays; a person's input, and other operations', are as they came.
    #[test]
    fn an_agent_s_own_goals_and_change_need_no_id() {
        let mut agent = Caller::with(crate::authz::gate::Kind::Agent, "agent:01a1", None, None);
        agent.agent = Some("01a1".into());
        assert_eq!(yours(&agent, "goals.add", json!({"goals": ["g"]})), json!({"goals": ["g"], "agent": "01a1"}));
        assert_eq!(yours(&agent, "goals.control", json!({"agent": " ", "action": "start"})), json!({"agent": "01a1", "action": "start"}));
        assert_eq!(yours(&agent, "agents.change", json!({})), json!({"agent": "01a1"}));
        assert_eq!(yours(&agent, "goals.get", json!({"agent": "child"})), json!({"agent": "child"}));
        assert_eq!(yours(&agent, "agents.stop", json!({})), json!({}), "ending an agent always names it");
        assert_eq!(yours(&Caller::owner(), "goals.add", json!({"goals": ["g"]})), json!({"goals": ["g"]}));
    }
}
