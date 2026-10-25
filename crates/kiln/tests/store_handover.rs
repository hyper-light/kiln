//! A runner's upgrade hands its conversation stores to the new build (docs/conversations.md, "A
//! runner's upgrade"). The runner being upgraded says which store the new build goes on with first
//! (`Frame::Stores`), and one from before says nothing: the new build then opens one of its own. Here,
//! one that ends before saying anything (it died, or their link broke, just after the new build said
//! it was ready): the new build keeps a store all the same, the one in its own folder, and feeds the
//! server from it.
//!
//! `cargo test -p kiln --test store_handover`

#![cfg(unix)]

mod common;

use std::io::Read as _;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, start};
use serde_json::Value;

#[test]
fn a_new_build_whose_old_runner_ends_before_handing_its_stores_over_keeps_one_of_its_own() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: "" })?;
    // The runner being upgraded: it hears the new build say it's ready, then is gone.
    let sock = dir.path().join("handover.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock)?;
    let (said_tx, said) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ready = (|| -> anyhow::Result<Value> {
            let (mut conn, _) = listener.accept()?;
            let mut len = [0u8; 4];
            conn.read_exact(&mut len)?;
            let mut frame = vec![0u8; usize::try_from(u32::from_be_bytes(len))?];
            conn.read_exact(&mut frame)?;
            Ok(serde_json::from_slice(&frame)?)
        })();
        let _ = said_tx.send(ready);
    });
    let name = "green-runner";
    let from = sock.to_string_lossy().into_owned();
    let (_runner, log) = agents::runner_with(dir.path(), s.port, name, None, Talk { lines: 1, every_ms: 10 }, &[("KILN_TAKE_OVER_FROM", from.as_str())])?;
    let runner_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let ready = said.recv_timeout(Duration::from_secs(120)).map_err(|_| anyhow::anyhow!("the new build never said it was ready:\n{}", runner_log()))??;
    assert_eq!(ready.pointer("/ready/stores"), Some(&Value::Bool(true)), "it said it takes the stores over: {ready}");

    // Its store, opened in its own folder, its feed to the server's.
    let deadline = Instant::now() + Duration::from_secs(90);
    let store = loop {
        let (_, body) = common::http(s.port, "GET", "/api/conversations/status", &s.admin, None)?;
        let board: Value = serde_json::from_str(&body).unwrap_or_default();
        let fed = board.get("stores").and_then(Value::as_array).into_iter().flatten().find(|st| {
            st.get("role").and_then(Value::as_str) == Some("runner") && st.get("machine").and_then(Value::as_str) == Some(name) && st.get("connected").and_then(Value::as_bool) == Some(true) && st.get("id").is_some_and(Value::is_string)
        });
        if let Some(st) = fed.and_then(|st| st.get("id")).and_then(Value::as_str) {
            break st.to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "the new build fed the server no store\nboard: {body}\nrunner:\n{}", runner_log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let line = runner_log().lines().find(|l| l.contains("the conversations go on in this store")).map(str::to_owned).unwrap_or_default();
    assert!(line.contains(&store) && line.contains("handed=false"), "it went on in a store of its own, {store}: {line:?}");
    assert!(dir.path().join(format!("{name}-state")).join("conversations.db").exists(), "its store is in its own folder");
    Ok(())
}
