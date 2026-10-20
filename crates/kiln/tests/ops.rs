//! W20-R01 (docs/ci.md): every operation over HTTP, in the OpenAPI document, and as `kiln op`,
//! all from the registry the MCP tools and code mode are made of. Each is listed with its input's
//! schema; run as its caller, it's checked as its own route is, and a change is audited once.

#![cfg(unix)]

mod common;

use std::process::Command;

use common::{Options, http, start};
use serde_json::{Value, json};

#[test]
fn w20_r01_operations_over_http_and_the_cli() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;

    // Listed, each with whether it changes anything and its input's schema.
    let (st, body) = http(s.port, "GET", "/api/ops", &s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let list: Value = serde_json::from_str(&body)?;
    let find = |name: &str| list.get("operations").and_then(Value::as_array).and_then(|o| o.iter().find(|x| x.get("name") == Some(&json!(name)))).cloned().unwrap_or_default();
    let runs = find("ci.runs.list");
    assert!(runs.pointer("/input/type") == Some(&json!("object")) && runs.get("read_only") == Some(&json!(true)), "{runs}");
    assert_eq!(find("ci.variables.set").get("read_only"), Some(&json!(false)));
    assert_eq!(http(s.port, "GET", "/api/ops", "nobody", None)?.0, 401);

    // Run as its caller: the owner's change goes through, audited once, as its route's would be.
    let (st, body) = http(s.port, "GET", "/api/audit?limit=1", &s.admin, None)?;
    anyhow::ensure!(st == 200, "{body}");
    let before = serde_json::from_str::<Value>(&body)?.pointer("/0/id").and_then(Value::as_i64).unwrap_or(0);
    let (st, body) = http(s.port, "POST", "/api/ops/ci.variables.set", &s.admin, Some(&json!({ "scope": "server", "name": "REGION", "value": "eu" })))?;
    assert_eq!(st, 200, "{body}");
    let (_, body) = http(s.port, "GET", "/api/audit?limit=20", &s.admin, None)?;
    let rows: Vec<Value> = serde_json::from_str::<Vec<Value>>(&body)?.into_iter().filter(|r| r.get("id").and_then(Value::as_i64).is_some_and(|i| i > before)).collect();
    assert_eq!(rows.len(), 1, "one row: {rows:?}");
    assert!(rows.first().and_then(|r| r.get("target")).and_then(Value::as_str).is_some_and(|t| t.contains("/api/ci/variables/REGION")), "{rows:?}");
    let (st, body) = http(s.port, "POST", "/api/ops/ci.variables.list", &s.admin, Some(&json!({ "scope": "server" })))?;
    assert!(st == 200 && body.contains("REGION") && body.contains("eu"), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/ops/ci.no.such", &s.admin, Some(&json!({})))?;
    assert_eq!(st, 404, "{body}");

    // A viewer's token reads, and is refused the change by its route's rule.
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": "ada", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{body}");
    let viewer = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert_eq!(http(s.port, "POST", "/api/ops/ci.runs.list", &viewer, Some(&json!({})))?.0, 200);
    let (st, body) = http(s.port, "POST", "/api/ops/ci.variables.set", &viewer, Some(&json!({ "scope": "server", "name": "REGION", "value": "us" })))?;
    assert!(st == 403 && body.contains("variables.manage"), "{st} {body}");

    // In the OpenAPI document: each, with its summary and its input's schema.
    let (st, body) = http(s.port, "GET", "/api/openapi.json", &s.admin, None)?;
    anyhow::ensure!(st == 200, "{body}");
    let doc: Value = serde_json::from_str(&body)?;
    let op = doc.pointer("/paths/~1api~1ops~1ci.variables.set/post").cloned().unwrap_or_default();
    assert!(op.get("summary").and_then(Value::as_str).is_some_and(|s| !s.is_empty()), "{op}");
    assert_eq!(op.pointer("/requestBody/content/application~1json/schema/required"), Some(&json!(["scope", "name", "value"])), "{op}");

    // `kiln op`: the list, what one takes, and one run.
    let url = format!("http://127.0.0.1:{}", s.port);
    let kiln = |args: &[&str]| -> anyhow::Result<String> {
        let o = Command::new(env!("CARGO_BIN_EXE_kiln")).args(args).args(["--url", &url, "--token", &s.admin]).env("HOME", dir.path()).output()?;
        anyhow::ensure!(o.status.success(), "kiln {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let listed = kiln(&["op"])?;
    assert!(listed.lines().any(|l| l.starts_with("ci.variables.set") && l.contains("(changes)")), "{listed}");
    assert!(kiln(&["op", "ci.variables.set", "--schema"])?.contains("\"required\""));
    let set = kiln(&["op", "ci.variables.set", "scope=server", "name=ZONE", "value=b"])?;
    assert!(set.contains("\"set\": true"), "{set}");
    let read = kiln(&["op", "ci.variables.list", r#"{"scope": "server"}"#])?;
    assert!(read.contains("ZONE"), "{read}");
    Ok(())
}

/// W20-R03: a write to routing rules, a pool or an environment answers with what it does now: a
/// warning for a rule no runner connected now meets, for a pool whose executor no runner can start, and for a
/// pool whose labels a runner carries itself; and what each heads for.
#[test]
fn w20_r03_writes_say_what_they_do() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    // The runner's facts: what it can run.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
        if body.contains("\"executors\"") {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "no facts: {body}");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // No machine runs Plan 9: the first rule is met by none, whatever the hardware.
    let rules = json!({ "rules": [{ "match": { "labels": ["plan9"] }, "require": ["os == plan9"] }, { "match": { "labels": ["local"] }, "executor": ["host"] }] });
    let (st, body) = http(s.port, "POST", "/api/ops/ci.routing.set", &s.admin, Some(&rules))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let warned: Vec<i64> = v.get("diagnostics").and_then(Value::as_array).into_iter().flatten().filter_map(|d| d.get("rule").and_then(Value::as_i64)).collect();
    assert_eq!(warned, vec![1], "only the rule no runner meets: {v}");
    assert!(v.get("preview").is_some_and(Value::is_array), "{v}");

    let pool = |spec: Value| -> anyhow::Result<Value> {
        let (st, body) = http(s.port, "POST", "/api/ops/ci.pools.set", &s.admin, Some(&spec))?;
        anyhow::ensure!(st == 200, "{st} {body}");
        Ok(serde_json::from_str(&body)?)
    };
    let vm = pool(json!({ "name": "vm", "executor": "firecracker", "image": "kiln/runner-ubuntu-24.04", "labels": ["vm"], "min_idle": 1, "max": 2 }))?;
    assert!(vm.to_string().contains("no runner connected now announces the firecracker executor"), "{vm}");
    assert_eq!(vm.pointer("/preview/heading_for"), Some(&json!(1)), "its minimum idle: {vm}");
    let mine = pool(json!({ "name": "mine", "executor": "docker", "image": "kiln/runner-ubuntu-24.04", "labels": ["local"], "max": 1 }))?;
    assert!(mine.to_string().contains("carries all of local itself"), "{mine}");

    // An environment: a reviewer who isn't anyone here, alone and not allowed their own; a
    // branches rule that leaves the trunk out.
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/env", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let env = |name: &str, protection: Value| -> anyhow::Result<Value> {
        let (st, body) = http(s.port, "POST", "/api/ops/ci.environments.save", &s.admin, Some(&json!({ "repo": "acme/env", "name": name, "protection": protection })))?;
        anyhow::ensure!(st == 200, "{st} {body}");
        Ok(serde_json::from_str(&body)?)
    };
    let prod = env("prod", json!({ "reviewers": ["ghost"], "prevent-self-review": true }))?;
    let said = prod.to_string();
    assert!(said.contains("reviewer ghost isn't anyone here") && said.contains("the only reviewer"), "{prod}");
    assert!(prod.pointer("/preview/a_deployment_from_the_trunk").and_then(Value::as_str).is_some_and(|m| m.contains("a review by one of ghost")), "{prod}");
    let staging = env("staging", json!({ "branches": ["release/*"] }))?;
    assert!(staging.to_string().contains("may not deploy to staging"), "{staging}");
    Ok(())
}
