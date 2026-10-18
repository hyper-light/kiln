//! The audit log, v2 (docs/ci.md W19-AC04): every mutating route in the route table writes
//! exactly one audit row, with the right principal and on-behalf-of. Generated from the table:
//! each rule that changes something is called by an agent token acting for a user, with ids
//! where the pattern has wildcards. The token may do nothing, so no handler runs and nothing
//! changes, yet each call writes its one row, refused. A change that's let through writes one row
//! too, with what its handler recorded as its detail.

#![cfg(unix)]

mod common;

use common::{Options, Server, concrete, http, start};
use serde_json::{Value, json};

/// The audit rows after `after` (by id), oldest first.
fn rows_after(s: &Server, after: i64) -> anyhow::Result<Vec<Value>> {
    let (status, body) = http(s.port, "GET", "/api/audit?limit=50", &s.admin, None)?;
    anyhow::ensure!(status == 200, "audit: {status} {body}");
    let all: Vec<Value> = serde_json::from_str(&body)?;
    let mut new: Vec<Value> = all.into_iter().filter(|r| r.get("id").and_then(Value::as_i64).is_some_and(|i| i > after)).collect();
    new.reverse();
    Ok(new)
}

fn last_id(s: &Server) -> anyhow::Result<i64> {
    Ok(rows_after(s, -1)?.last().and_then(|r| r.get("id")).and_then(Value::as_i64).unwrap_or(0))
}

#[test]
fn w19_ac04_every_change_writes_one_row() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    // A user, and an agent acting for her that may only look.
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "user: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "agent", "agent": "fixer", "user": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "token: {st} {body}");
    let token = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))?;

    // Every rule that changes something, as the agent.
    let mut checked = 0;
    let mut wrong = Vec::new();
    for rule in kiln::authz::ROUTES {
        let method = match rule.method {
            Some("GET") => continue,
            Some(m) => m,
            None => "POST",
        };
        let path = concrete(rule.pattern);
        let before = last_id(&s)?;
        let (status, _) = http(s.port, method, &path, &token, Some(&json!({})))?;
        let rows = rows_after(&s, before)?;
        checked += 1;
        let ok = rows.len() == 1
            && rows.first().is_some_and(|r| {
                r.get("principal").and_then(Value::as_str) == Some("agent:fixer")
                    && r.get("on_behalf_of").and_then(Value::as_str) == Some("user:ada")
                    && r.get("result").and_then(Value::as_str) == Some(status.to_string().as_str())
                    && r.get("action").and_then(Value::as_str).is_some_and(|a| a.starts_with(method))
                    && r.get("target").and_then(Value::as_str) == Some(path.as_str())
            });
        if !ok {
            wrong.push(format!("{method} {path} ({status}): {rows:?}"));
        }
    }
    assert!(checked > 40, "{checked} mutating routes");
    assert!(wrong.is_empty(), "{} of {checked} routes didn't write exactly one right row:\n{}", wrong.len(), wrong.join("\n"));

    // A change let through writes one row too, with what its handler recorded.
    let before = last_id(&s)?;
    let (status, body) = http(s.port, "PUT", "/api/ci/variables/REGION?scope=server", &s.admin, Some(&json!({ "value": "eu" })))?;
    anyhow::ensure!((200..300).contains(&status), "variable: {status} {body}");
    let rows = rows_after(&s, before)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = rows.first().cloned().unwrap_or_default();
    assert_eq!(row.get("principal").and_then(Value::as_str), Some("admin"));
    assert!(row.get("detail").and_then(Value::as_str).is_some_and(|d| d.contains("ci.variable.set")), "{row}");
    assert_eq!(row.get("scope").and_then(Value::as_str), Some("server"));
    // Filters by any field.
    let (_, body) = http(s.port, "GET", "/api/audit?principal=agent:fixer&result=403&limit=5", &s.admin, None)?;
    let filtered: Vec<Value> = serde_json::from_str(&body)?;
    assert!(!filtered.is_empty() && filtered.iter().all(|r| r.get("principal").and_then(Value::as_str) == Some("agent:fixer")), "{body}");
    Ok(())
}
