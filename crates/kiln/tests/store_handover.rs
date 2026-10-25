//! A runner's upgrade hands its conversation stores to the new build (docs/conversations.md, "A
//! runner's upgrade"). The runner being upgraded says which store the new build goes on with first
//! (`Frame::Stores`), and one from before says nothing: the new build then opens one of its own. Here,
//! one that ends before saying anything (it died, or their link broke, just after the new build said
//! it was ready): the new build keeps a store all the same, the one in its own folder, and feeds the
//! server from it; and, never promoted, it takes messages again a while after (CP-T3.2,
//! docs/sends.md, "Blue/green").
//!
//! `cargo test -p kiln --test store_handover`

#![cfg(unix)]

mod common;

use std::io::Read as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, start};
use serde_json::Value;

const NAME: &str = "green-runner";

/// A server, and a runner started as a new build taking over from a runner being upgraded that
/// hears it say it's ready, then is gone: the runner and its log, once it said so (what it said).
fn ended_early(dir: &std::path::Path) -> anyhow::Result<(Server, agents::Runner, PathBuf, Value)> {
    let s = start(dir, &Options { agent: false, ci: "", extra: "" })?;
    let sock = dir.join("handover.sock");
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
    let from = sock.to_string_lossy().into_owned();
    let (runner, log) = agents::runner_with(dir, s.port, NAME, None, Talk { lines: 1, every_ms: 10 }, &[("KILN_TAKE_OVER_FROM", from.as_str())])?;
    let ready = said.recv_timeout(Duration::from_secs(120)).map_err(|_| anyhow::anyhow!("the new build never said it was ready:\n{}", std::fs::read_to_string(&log).unwrap_or_default()))??;
    Ok((s, runner, log, ready))
}

#[test]
fn a_new_build_whose_old_runner_ends_before_handing_its_stores_over_keeps_one_of_its_own() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (s, _runner, log, ready) = ended_early(dir.path())?;
    let runner_log = || std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(ready.pointer("/ready/stores"), Some(&Value::Bool(true)), "it said it takes the stores over: {ready}");

    // Its store, opened in its own folder, its feed to the server's.
    let deadline = Instant::now() + Duration::from_secs(90);
    let store = loop {
        let (_, body) = common::http(s.port, "GET", "/api/conversations/status", &s.admin, None)?;
        let board: Value = serde_json::from_str(&body).unwrap_or_default();
        let fed = board.get("stores").and_then(Value::as_array).into_iter().flatten().find(|st| {
            st.get("role").and_then(Value::as_str) == Some("runner") && st.get("machine").and_then(Value::as_str) == Some(NAME) && st.get("connected").and_then(Value::as_bool) == Some(true) && st.get("id").is_some_and(Value::is_string)
        });
        if let Some(st) = fed.and_then(|st| st.get("id")).and_then(Value::as_str) {
            break st.to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "the new build fed the server no store\nboard: {body}\nrunner:\n{}", runner_log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let line = runner_log().lines().find(|l| l.contains("the conversations go on in this store")).map(str::to_owned).unwrap_or_default();
    assert!(line.contains(&store) && line.contains("handed=false"), "it went on in a store of its own, {store}: {line:?}");
    assert!(dir.path().join(format!("{NAME}-state")).join("conversations.db").exists(), "its store is in its own folder");
    Ok(())
}

/// CP-T3.2: a new build holds messages till it's promoted, so that a rollback never forgets what it
/// took; one whose take-over ended part-way, the runner it upgrades gone, takes them again a while
/// after (`TAKE_OVER_GRACE`), as one that took everything over does.
#[test]
fn cp_t3_2_a_new_build_whose_take_over_ends_part_way_takes_messages_again() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (_s, _runner, log, _) = ended_early(dir.path())?;
    let runner_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(120);
    let line = loop {
        if let Some(l) = runner_log().lines().find(|l| l.contains("this runner takes messages again")) {
            break l.to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "the new build still holds its messages:\n{}", runner_log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(line.contains("took_over=false"), "it hadn't taken everything over: {line}");
    Ok(())
}
