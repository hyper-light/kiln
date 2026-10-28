//! W24-R31: the reaper's policies, stored and served. A policy set at each scope (the system, a
//! runner, a repository, a session) reads back; the effective one takes hardware limits from the
//! system and the runner only (the stricter winning) and what to keep from the most specific scope,
//! naming the scope each value came from; bad policies are refused with why.
//!
//! `cargo test -p kiln --test reap_policies`

mod common;

use common::{Options, http, start};
use serde_json::{Value, json};

const GIB: u64 = 1 << 30;

fn put(port: u16, admin: &str, scope: &str, policy: Value) -> anyhow::Result<(u16, Value)> {
    let (status, body) = http(port, "PUT", &format!("/api/ci/work/reap/policies?scope={scope}&volume={}", 1000 * GIB), admin, Some(&json!({ "policy": policy })))?;
    Ok((status, serde_json::from_str(&body).unwrap_or(Value::String(body))))
}

#[test]
fn w24_r31_policies_by_scope_and_their_precedence() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;

    // One policy at each scope.
    for (scope, policy) in [
        ("server", json!({ "floor": "10%", "ceiling": "400GiB", "keep_last": 2, "max_age": "30d", "schedule": "30 3 * * *" })),
        ("runner:box", json!({ "floor": "200GiB", "ceiling": "20%", "keep_within": "2d" })),
        ("repo:acme/app", json!({ "keep_last": 5, "max_age": "14d" })),
        ("session:S1", json!({ "keep_last": 1, "schedule": "6h" })),
    ] {
        let (status, body) = put(s.port, &s.admin, scope, policy.clone())?;
        anyhow::ensure!(status == 200, "{scope}: {status} {body}");
        anyhow::ensure!(body.get("policy") == Some(&policy), "{scope} reads back as set: {body}");
    }

    // Read back, and the effective policy for all four.
    let (status, body) = http(s.port, "GET", &format!("/api/ci/work/reap/policies?runner=box&repo=acme/app&session=S1&volume={}", 1000 * GIB), &s.admin, None)?;
    anyhow::ensure!(status == 200, "{status} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let policies = v.get("policies").and_then(Value::as_array).cloned().unwrap_or_default();
    anyhow::ensure!(policies.len() == 4, "four policies: {v}");
    let e = v.get("effective").cloned().unwrap_or_default();
    let at = |k: &str, f: &str| e.get(k).and_then(|x| x.get(f)).cloned().unwrap_or(Value::Null);
    // Hardware: the stricter of the system and the runner; the floor's the runner's 200 GiB (over
    // 10% of 1000 GiB), the ceiling the runner's 20% (under 400 GiB).
    anyhow::ensure!(at("floor", "bytes") == json!(200 * GIB) && at("floor", "from") == json!("runner"), "floor: {e}");
    anyhow::ensure!(at("ceiling", "bytes") == json!(200 * GIB) && at("ceiling", "from") == json!("runner"), "ceiling: {e}");
    anyhow::ensure!(at("target", "from") == Value::Null, "target is kiln's default: {e}");
    // What to keep: the most specific scope that sets it.
    anyhow::ensure!(at("keep_last", "value") == json!(1) && at("keep_last", "from") == json!("session"), "keep_last: {e}");
    anyhow::ensure!(at("max_age", "value") == json!("14d") && at("max_age", "from") == json!("repo"), "max_age: {e}");
    anyhow::ensure!(at("keep_within", "value") == json!("2d") && at("keep_within", "from") == json!("runner"), "keep_within: {e}");
    anyhow::ensure!(at("schedule", "value") == json!("6h") && at("schedule", "from") == json!("session"), "schedule: {e}");

    // Without the session and the repository, the system's keep_last and schedule are in force.
    let (_, body) = http(s.port, "GET", "/api/ci/work/reap/policies?runner=box", &s.admin, None)?;
    let v: Value = serde_json::from_str(&body)?;
    let e = v.get("effective").cloned().unwrap_or_default();
    anyhow::ensure!(e.pointer("/keep_last/from") == Some(&json!("system")) && e.pointer("/schedule/value") == Some(&json!("30 3 * * *")), "{e}");

    // Refused, with why: hardware limits below the runner scope, a share past 100%, a bad cron
    // expression, an unknown key, an unknown scope.
    for (scope, policy, why) in [
        ("repo:acme/app", json!({ "floor": "10%" }), "not a repository"),
        ("server", json!({ "ceiling": "120%" }), "between 0% and 100%"),
        ("server", json!({ "schedule": "61 * * * *" }), "outside 0-59"),
        ("runner:box", json!({ "keep_forever": true }), "isn't a policy key"),
        ("cluster:x", json!({ "keep_last": 1 }), "isn't a scope"),
    ] {
        let (status, body) = put(s.port, &s.admin, scope, policy)?;
        anyhow::ensure!(status == 400 && body.to_string().contains(why), "{scope}: {status} {body}");
    }

    // `{}` removes a scope's policy; the audit log has every change.
    let (status, _) = put(s.port, &s.admin, "session:S1", json!({}))?;
    anyhow::ensure!(status == 200);
    let (_, body) = http(s.port, "GET", "/api/ci/work/reap/policies?session=S1", &s.admin, None)?;
    let v: Value = serde_json::from_str(&body)?;
    anyhow::ensure!(v.pointer("/effective/keep_last/from") == Some(&json!("system")), "{v}");
    let (_, audit) = http(s.port, "GET", "/api/audit?limit=50", &s.admin, None)?;
    anyhow::ensure!(audit.matches("ci.work.reap.set").count() >= 5, "audited: {audit}");
    Ok(())
}
