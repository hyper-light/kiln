//! Step 1 of the plan "No crash or reconnect replays a whole session" (docs/wire.md,
//! "Revisions"). A viewer that reconnects says how much of the server's copy of a session it had
//! (`?after=<born>.<rev>`). It gets only what changed since, not the whole session again. Under
//! another incarnation's number, it gets the session whole.
//!
//! The harness is a stand-in for Claude Code (`tests/fixtures/fake-claude`): it calls no model.
//!
//! `cargo test -p kiln --test resume`

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::agents::{self, Talk};
use common::{Options, start};
use serde_json::Value;

/// A frame's kind.
fn t(f: &Value) -> &str {
    f.get("t").and_then(Value::as_str).unwrap_or_default()
}

#[test]
fn a_viewer_that_reconnects_gets_only_what_changed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: "" })?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "resume-runner", None, Talk { lines: 400, every_ms: 50 })?;
    agents::connected(s.port, &s.admin, "resume-runner", &log, Duration::from_secs(120))?;
    let id = agents::open_session(s.port, &s.admin, "resume-runner", &work, Duration::from_secs(60))?;

    // A viewer with part of the stream: the attach's snapshot, then live frames, their revision kept.
    std::thread::sleep(Duration::from_secs(3));
    let mut ws = agents::socket(s.port, &s.admin, &format!("/api/harness/sessions/{id}/ws"))?;
    let (frames, _) = agents::until_synced(&mut ws)?;
    let open = frames.first().cloned().unwrap_or_default();
    assert_eq!(t(&open), "open", "an attach without after= gets the session whole: {open}");
    let born = open.get("born").and_then(Value::as_u64).unwrap_or_default();
    let mut rev = open.get("rev").and_then(Value::as_u64).unwrap_or_default();
    let mut had = agents::lines(&agents::events(&frames));
    for _ in 0..30 {
        let (op, payload) = agents::frame(&mut ws)?;
        if op != 1 {
            continue;
        }
        let f: Value = serde_json::from_slice(&payload)?;
        rev = rev.max(f.get("rev").and_then(Value::as_u64).unwrap_or_default());
        had.extend(agents::lines(&agents::events(std::slice::from_ref(&f))));
    }
    drop(ws);
    anyhow::ensure!(born != 0 && rev != 0, "the copy said no revision: born {born}, rev {rev}");
    let last_had = had.iter().max().copied().unwrap_or_default();

    // Away for a while, as the session goes on.
    std::thread::sleep(Duration::from_secs(4));
    let (resumed, bytes) = agents::attach(s.port, &s.admin, &id, &format!("?after={born}.{rev}"))?;
    assert_eq!(resumed.first().map(t), Some("resumed"), "it resumes: {:?}", resumed.first());
    let got = agents::lines(&agents::events(&resumed));
    anyhow::ensure!(!got.is_empty(), "the session went on while it was away, yet the resume has no lines");
    // None of what it had, and every line after what it had, once each, in order.
    let again: Vec<usize> = got.iter().copied().filter(|n| had.contains(n)).collect();
    assert!(again.is_empty(), "the resume sent lines the viewer had: {again:?}");
    let want: Vec<usize> = ((last_had + 1)..=got.iter().max().copied().unwrap_or_default()).collect();
    assert_eq!(got, want, "the lines since, each once, in order");
    // Beyond the events changed since and the newest state, at most 4 KiB.
    let counted: usize = resumed.iter().filter(|f| matches!(t(f), "events" | "state" | "meta" | "changes")).map(|f| f.to_string().len()).sum();
    let beyond = bytes.saturating_sub(counted);
    assert!(beyond <= 4096, "the resume sent {beyond} bytes besides the events changed since and the newest state");

    // Another incarnation's number: the session whole, from its first line.
    let (whole, _) = agents::attach(s.port, &s.admin, &id, &format!("?after={}.{rev}", born.wrapping_add(1)))?;
    assert_eq!(whole.first().map(t), Some("open"), "under another incarnation it starts over");
    assert_eq!(agents::lines(&agents::events(&whole)).first(), Some(&0), "whole, from the first line");
    Ok(())
}
