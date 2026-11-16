//! A deployment experiment's live traffic split (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1, the person's
//! decision 7), through kiln's API: each new session of its target is one of its variants, drawn by weight as it
//! opens, kept for its life; a variant that can't start is counted failing; pausing stops the draws; deciding
//! promotes one variant for every later session.
//!
//! The runner here runs a stand-in harness and starts no microVM (this build carries no Shards): its host-engine
//! variants open sessions, and a microVM variant's starts are refused there, which is that variant's failure. The
//! measures a microVM gives (start within its kind, footprint, host CPU) are the runner's sessions' records
//! (`Start::measured`), proven over Hex's simulator in the crate, and on real microVMs by ROOT's runs.

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::agents::{self, Talk};
use common::{Options, Server, http, start};
use serde_json::{Value, json};

fn op(s: &Server, name: &str, input: Value) -> anyhow::Result<(u16, Value)> {
    let (st, body) = http(s.port, "POST", &format!("/api/ops/{name}"), &s.admin, Some(&input))?;
    Ok((st, serde_json::from_str(&body).unwrap_or(Value::String(body))))
}

/// Waits until the runner `box` has told the server how it runs sessions (its engine, P2.3): a session placed
/// before it is refused ("hasn't said yet how it runs sessions"), as placement reads the engine from its facts.
fn said_its_engine(s: &Server) -> anyhow::Result<()> {
    let path = format!("/api/runners/{}/engine", common::machine("box"));
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let (st, body) = http(s.port, "GET", &path, &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if st == 200 && v.get("engine").is_some_and(|e| !e.is_null()) {
            return Ok(());
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "box never said its engine: {st} {body}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Opens `count` sessions of claude on the runner `box`, as the console does: their infos.
fn open(s: &Server, cwd: &std::path::Path, count: usize) -> anyhow::Result<Vec<Value>> {
    let ask = json!({ "target": format!("agent:{}", common::machine("box")), "spec": { "kind": "claude", "cwd": cwd.display().to_string(), "prompt": "stream" }, "count": count });
    let (code, body) = common::http_within(s.port, "POST", "/api/harness/sessions", &s.admin, Some(&ask), Duration::from_secs(120))?;
    anyhow::ensure!(code == 200, "{code} {body}");
    let v: Value = serde_json::from_str(&body)?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

fn info(s: &Server, id: &str) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?.1).unwrap_or_default())
}

fn arm_of(i: &Value) -> Option<String> {
    i.pointer("/experiment/arm").and_then(Value::as_str).map(str::to_owned)
}

fn results(s: &Server, id: &str) -> anyhow::Result<Value> {
    let (_, v) = op(s, "experiments.get", json!({ "experiment": id }))?;
    Ok(v.pointer("/experiment").cloned().unwrap_or_default())
}

fn arm<'a>(e: &'a Value, name: &str) -> Option<&'a Value> {
    e.pointer("/results/arms").and_then(Value::as_array).and_then(|a| a.iter().find(|x| x.get("name").and_then(Value::as_str) == Some(name)))
}

/// P7.1, its ACs 1, 2 and 4 and its tests:
/// - a split's variants are deployments with weights (AC1); what isn't one is refused;
/// - with weights 70/30 over 64 opened sessions, each session is assigned one variant, recorded on it, and the
///   variants' shares fall within their binomial interval (AC2's draw; 3.29 standard errors, 99.9 %);
/// - a session's variant never changes: read again after its harness starts again, it's the same, and the split's
///   draw of it agrees (AC2's sticky);
/// - pausing stops the draws: a session opened while paused is no variant (AC4);
/// - a variant that can't start (a microVM on a runner that starts none) is counted failing, every one of its
///   sessions among its failures, none dropped (AC3's failures);
/// - deciding promotes one variant: every later session is it, promoted, drawn none (AC4).
#[test]
fn p7_1_a_split_assigns_by_weight_keeps_it_counts_failures_and_promotes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (_runner, log) = agents::runner_with(dir.path(), s.port, "box", None, Talk { lines: 1, every_ms: 10 }, &[])?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;
    said_its_engine(&s)?;
    let work = dir.path().join("work-split");
    std::fs::create_dir_all(&work)?;
    let host = |name: &str, weight: f64| json!({ "name": name, "weight": weight, "harness": "claude", "engine": "host" });

    for (ask, says) in [
        (json!({ "target": { "harness": "claude" }, "arms": [host("a", 70.0), host("b", 20.0)] }), "add up to 100"),
        (json!({ "target": { "harness": "claude" }, "arms": [host("a", 100.0)] }), "from 2 to"),
        (json!({ "target": { "harness": "claude" }, "arms": [host("a", 50.0), { "name": "b", "weight": 50, "harness": "claude", "engine": "host", "size": { "cpus": 2 } }] }), "the host engine"),
        (json!({ "target": { "harness": "notepad" }, "arms": [host("a", 50.0), host("b", 50.0)] }), "none of kiln's harnesses"),
        (json!({ "target": {}, "arms": [host("a", 50.0), host("b", 50.0)] }), "`target`"),
    ] {
        let (st, refused) = op(&s, "experiments.start", ask)?;
        assert!(st == 400 && refused.to_string().contains(says), "{says}: {st} {refused}");
    }

    let (st, started) = op(&s, "experiments.start", json!({ "question": "Which way do sessions do better?", "target": { "harness": "claude" }, "arms": [host("a", 70.0), host("b", 30.0)] }))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {started}");
    let id = started.pointer("/experiment/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (st, again) = op(&s, "experiments.start", json!({ "target": { "harness": "claude" }, "arms": [host("a", 50.0), host("b", 50.0)] }))?;
    assert!(st == 409 && again.to_string().contains(&id), "one split a target: {st} {again}");

    let mut sessions = Vec::new();
    for _ in 0..4 {
        sessions.extend(open(&s, &work, 16)?);
    }
    assert_eq!(sessions.len(), 64);
    let arms: Vec<Option<String>> = sessions.iter().map(arm_of).collect();
    assert!(arms.iter().all(|a| matches!(a.as_deref(), Some("a" | "b"))) && sessions.iter().all(|i| i.pointer("/experiment/experiment") == Some(&json!(id))), "each session one variant, recorded on it: {sessions:?}");
    let a_share = arms.iter().filter(|a| a.as_deref() == Some("a")).count() as f64 / 64.0;
    let se = (0.7f64 * 0.3 / 64.0).sqrt();
    assert!((a_share - 0.7).abs() <= 3.29 * se, "a's share {a_share} is outside its binomial interval");
    let e = results(&s, &id)?;
    let assigned = |e: &Value, n: &str| arm(e, n).and_then(|a| a.get("assigned")).and_then(Value::as_u64).unwrap_or(0);
    assert_eq!(assigned(&e, "a") + assigned(&e, "b"), 64, "{e}");
    assert_eq!(e.pointer("/results/level"), Some(&json!(0.95)), "{e}");

    // Sticky: its harness started again, it's the variant it was.
    let first = sessions.first().and_then(|i| i.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
    let was = arm_of(&info(&s, &first)?);
    let (code, body) = http(s.port, "POST", &format!("/api/harness/agents/{first}/restart"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!((200..300).contains(&code), "the restart was refused: {code} {body}");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(arm_of(&info(&s, &first)?), was, "a session's variant never changes");

    // Paused: no draw.
    let (st, paused) = op(&s, "experiments.pause", json!({ "experiment": id }))?;
    assert!(st == 200 && paused.pointer("/experiment/state") == Some(&json!("paused")), "{st} {paused}");
    let opened = open(&s, &work, 1)?;
    assert!(opened.iter().all(|i| i.get("experiment").is_none_or(Value::is_null)), "opened while paused, drawn none: {opened:?}");
    assert_eq!(assigned(&results(&s, &id)?, "a") + assigned(&results(&s, &id)?, "b"), 64);
    let (st, _) = op(&s, "experiments.resume", json!({ "experiment": id }))?;
    assert_eq!(st, 200);

    // A failing variant: a microVM on a runner that starts none. Each of its sessions is refused and counted.
    let (st, cancelled) = op(&s, "experiments.cancel", json!({ "experiment": id }))?;
    assert!(st == 200 && cancelled.pointer("/experiment/state") == Some(&json!("cancelled")), "{st} {cancelled}");
    let (st, failing) = op(&s, "experiments.start", json!({ "target": { "harness": "claude" }, "arms": [host("ok", 50.0), { "name": "vm", "weight": 50, "harness": "claude", "engine": "microvm", "size": { "cpus": 1, "memory_mib": 512 } }] }))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {failing}");
    let fid = failing.pointer("/experiment/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut ok = Vec::new();
    for _ in 0..2 {
        ok.extend(open(&s, &work, 16)?);
    }
    assert!(ok.iter().all(|i| arm_of(i).as_deref() == Some("ok")), "the sessions that opened are ok's: {ok:?}");
    let e = results(&s, &fid)?;
    let vm = arm(&e, "vm").cloned().unwrap_or_default();
    let n_vm = vm.get("assigned").and_then(Value::as_u64).unwrap_or(0);
    assert!(n_vm > 0 && n_vm as usize + ok.len() == 32, "every draw kept, none dropped: {e}");
    assert!(
        vm.pointer("/failures/failed").and_then(Value::as_u64) == Some(n_vm) && vm.pointer("/failures/known").and_then(Value::as_u64) == Some(n_vm) && vm.pointer("/outcomes/refused").and_then(Value::as_u64) == Some(n_vm),
        "each of vm's sessions counted failing: {vm}"
    );
    assert!(vm.pointer("/failures/rate/low").and_then(Value::as_f64).is_some(), "with its rate's sequence: {vm}");

    // Decided: every later session is the promoted variant, drawn none.
    let (st, decided) = op(&s, "experiments.decide", json!({ "experiment": fid, "variant": "ok" }))?;
    assert!(st == 200 && decided.pointer("/experiment/state") == Some(&json!("done")), "{st} {decided}");
    let later = open(&s, &work, 4)?;
    assert!(later.iter().all(|i| arm_of(i).as_deref() == Some("ok") && i.pointer("/experiment/promoted") == Some(&json!(true))), "{later:?}");
    let e = results(&s, &fid)?;
    let total: u64 = ["ok", "vm"].iter().map(|n| arm(&e, n).and_then(|a| a.get("assigned")).and_then(Value::as_u64).unwrap_or(0)).sum();
    assert_eq!(total, 32, "a promoted variant is no draw: {e}");
    Ok(())
}
