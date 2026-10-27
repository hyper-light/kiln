//! Typed messages, step 3 (docs/wire.md, "What can't be read"): a frame between a runner and the
//! server that can't be read is refused to its sender, typed, and noted (counted, audited, kept for
//! the runner's page). The channel makes up for it as for a frame lost below it, so the next valid
//! frame on the lane is applied and nothing it carried is lost or doubled.
//!
//! The faults are a test build's (`crate::runner::chaos`): `unknown=` makes a frame's variant one
//! past what its sender's enum has, `garble=` cuts it short, adds bytes after it or flips one. The
//! harness is a stand-in for Claude Code (`tests/fixtures/fake-claude`): it calls no model.
//!
//! `cargo test -p kiln --features chaos --test unread_frames`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http};
use serde_json::{Value, json};

/// A series' value in the server's metrics (0 when it has none).
fn metric(port: u16, admin: &str, series: &str) -> u64 {
    let (_, text) = http(port, "GET", "/metrics", admin, None).unwrap_or_default();
    text.lines().find_map(|l| l.strip_prefix(series).and_then(|v| v.trim().parse().ok())).unwrap_or(0)
}

/// `kiln_wire_refused_total` of `wire` and `code`.
fn refused(port: u16, admin: &str, wire: &str, code: &str) -> u64 {
    metric(port, admin, &format!("kiln_wire_refused_total{{wire=\"{wire}\",code=\"{code}\"}}"))
}

/// Runner `name`'s record (`GET /api/agents`).
fn record(port: u16, admin: &str, name: &str) -> Value {
    let (_, body) = http(port, "GET", "/api/agents", admin, None).unwrap_or_default();
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
    list.into_iter().find(|a| a.get("name").and_then(Value::as_str) == Some(name)).unwrap_or_default()
}

/// How many refusals of `wire` and `code` a runner's record keeps.
fn kept(rec: &Value, wire: &str, code: &str) -> u64 {
    rec.pointer("/refused/counts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("wire").and_then(Value::as_str) == Some(wire) && c.get("code").and_then(Value::as_str) == Some(code))
        .filter_map(|c| c.get("n").and_then(Value::as_u64))
        .sum()
}

/// The audit log's `security.wire_refused` rows for runner `name`.
fn audited(port: u16, admin: &str, name: &str) -> Vec<Value> {
    let (_, body) = http(port, "GET", &format!("/api/audit?action=security.wire_refused&principal=runner:{name}"), admin, None).unwrap_or_default();
    serde_json::from_str::<Value>(&body).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

/// A log without its colors: tracing styles its fields' names (`folded=` among them).
fn plain(log: &str) -> String {
    let mut out = String::with_capacity(log.len());
    let mut chars = log.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // An escape sequence, to its final letter.
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// How many of its frames the server told the runner it couldn't read: each `ServerMsg::Unread` the
/// runner logged, and those folded into it.
fn told(log: &str) -> u64 {
    log.lines()
        .filter(|l| l.contains("the server couldn't read a message from this runner"))
        .map(|l| 1 + l.split("folded=").nth(1).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0))
        .sum()
}

/// A runner that takes word of what can't be read (`CAP_UNREAD`), and a server, each making some of
/// its channel frames unreadable for the other (a variant it doesn't know), the runner its samples
/// too (garbled). Every line of a session still reaches the server's copy once, in order. Each side
/// refused what it couldn't read: the server noted it against the runner and told it so, and the
/// runner told the server of the server's. Counted, none went unanswered.
#[test]
fn frames_that_cant_be_read_are_refused_typed_and_noted_and_made_up_for() -> anyhow::Result<()> {
    const LINES: usize = 80;
    let dir = tempfile::tempdir()?;
    // The server's channel frames to the runner (`ServerMsg::Wire`, 54): about one in 5 unknown.
    let s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "" }, false, &[("KILN_CHAOS", "unknown=control/54:5")])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    // The runner's (`AgentMsg::Wire`, 46, on its terminal lane): about one in 7 unknown; and every
    // sample it sends (`AgentMsg::Sample`, 2) garbled.
    let (_runner, log_path) = agents::runner(dir.path(), s.port, "unread-runner", Some("unknown=term/46:7,garble=control/2:1"), Talk { lines: LINES, every_ms: 100 })?;
    let runner_log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    agents::connected(s.port, &s.admin, "unread-runner", &log_path, Duration::from_secs(120))?;
    let open = json!({"target": "agent:unread-runner", "spec": {"kind": "claude", "cwd": work.display().to_string(), "prompt": "stream"}});
    let deadline = Instant::now() + Duration::from_secs(90);
    let id = loop {
        let (code, body) = http(s.port, "POST", "/api/harness/sessions", &s.admin, Some(&open))?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if code == 200
            && let Some(id) = v.pointer("/0/id").and_then(Value::as_str)
        {
            break id.to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "couldn't open a session: {code} {body}\n{}", runner_log());
        std::thread::sleep(Duration::from_secs(1));
    };
    // Every line reaches the server's copy once, in order: what couldn't be read was made up for.
    // Making up for so many frames takes minutes (alone on a Mac Studio, 2 to 3), more beside the
    // other test of this file.
    let deadline = Instant::now() + Duration::from_secs(480);
    let lines = loop {
        let (_, info) = http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
        let info: Value = serde_json::from_str(&info).unwrap_or_default();
        if info.pointer("/last_turn/how").and_then(Value::as_str) == Some("done") {
            let frames = agents::attach(s.port, &s.admin, &id, "").map(|(frames, _)| frames)?;
            let events: Vec<String> = frames
                .iter()
                .filter(|f| f.get("t").and_then(Value::as_str) == Some("events"))
                .flat_map(|f| f.get("events").and_then(Value::as_array).cloned().unwrap_or_default())
                .map(|e| e.to_string())
                .collect();
            let lines: Vec<Vec<usize>> = (0..LINES).map(|n| events.iter().enumerate().filter(|(_, e)| e.contains(&format!("line {n:05}"))).map(|(i, _)| i).collect()).collect();
            if lines.iter().all(|at| !at.is_empty()) {
                break lines;
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "the session's copy never held every line\nsession: {info}\nrunner:\n{}\nserver:\n{}", runner_log(), s.log());
        std::thread::sleep(Duration::from_secs(2));
    };
    let doubled: Vec<usize> = lines.iter().enumerate().filter(|(_, at)| at.len() > 1).map(|(n, _)| n).collect();
    assert!(doubled.is_empty(), "lines in more than one event of the server's copy: {doubled:?}");
    let order: Vec<usize> = lines.iter().filter_map(|at| at.first().copied()).collect();
    assert!(order.windows(2).all(|w| w.first() < w.get(1)), "lines out of order in the server's copy: {order:?}");
    // Each side's refusals: counted, kept on the runner's page, audited naming it; and each answered.
    // Every frame of the runner's the server couldn't read (noted against it, on its page), it told
    // the runner of (`ServerMsg::Unread`, with those folded into it: an unknown kind's repeats are
    // folded, its kind told once an hour); and every frame of the server's the runner couldn't read
    // (said in its log), the runner told the server of (`AgentMsg::Unread`, noted against it as one
    // and those it folded in; what it folds it tells within a minute though no other word comes):
    // none went unanswered. No fault here touches either `Unread`. The faults go on meanwhile, so
    // each side is read after what it answers: an answer is never ahead of what it answers, and
    // they're equal once none is on its way.
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        let before = plain(&runner_log());
        let rec = record(s.port, &s.admin, "unread-runner");
        let after = plain(&runner_log());
        let counted = [refused(s.port, &s.admin, "runner", "unknown"), refused(s.port, &s.admin, "runner", "malformed"), refused(s.port, &s.admin, "server", "unknown")];
        let on_page = [kept(&rec, "runner", "unknown"), kept(&rec, "runner", "malformed"), kept(&rec, "server", "unknown")];
        let rows = audited(s.port, &s.admin, "unread-runner");
        let (runners, answered) = (kept(&rec, "runner", "unknown") + kept(&rec, "runner", "malformed"), told(&before));
        let (heard, servers) = (kept(&rec, "server", "unknown") + kept(&rec, "server", "malformed"), after.matches("a frame from the server couldn't be read").count() as u64);
        assert!(answered <= runners && heard <= servers, "an answer without a refusal: the server told the runner of {answered} of its {runners}, and heard of {heard} of the {servers} of its own the runner couldn't read");
        if counted.iter().all(|n| *n > 0) && on_page.iter().all(|n| *n > 0) && !rows.is_empty() && answered == runners && heard == servers {
            assert!(rows.iter().all(|r| r.get("target").and_then(Value::as_str).is_some_and(|t| t == "runner" || t == "server")), "{rows:?}");
            eprintln!("unanswered: 0 of the runner's {runners} frames the server couldn't read, 0 of the server's {servers} the runner couldn't");
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "not every refusal was noted and answered: counted {counted:?}, on its page {on_page:?}, {} audit rows; the server told the runner of {answered} of its {runners}, and heard of {heard} of the {servers} of its own the runner couldn't read\nrecord: {rec}\nrunner:\n{}",
            rows.len(),
            runner_log()
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

/// A runner from before (chaos `old`: no `Caps`, so it takes no word of what couldn't be read)
/// keeps working: its frames the server can't read are still counted, audited and kept for its
/// page, and it's told nothing new.
#[test]
fn a_runner_from_before_is_told_nothing_new_and_its_unreadable_frames_are_noted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "" }, false, &[])?;
    // Its samples (`AgentMsg::Sample`, 2), every one a variant the server doesn't know.
    let (_runner, log_path) = agents::runner(dir.path(), s.port, "old-unread", Some("old,unknown=control/2:1"), Talk { lines: 1, every_ms: 100 })?;
    let runner_log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    agents::connected(s.port, &s.admin, "old-unread", &log_path, Duration::from_secs(120))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let rec = record(s.port, &s.admin, "old-unread");
        let (counted, on_page, rows) = (refused(s.port, &s.admin, "runner", "unknown"), kept(&rec, "runner", "unknown"), audited(s.port, &s.admin, "old-unread"));
        if counted > 1 && on_page > 1 && !rows.is_empty() {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "its unreadable samples weren't noted: counted {counted}, on its page {on_page}, {} audit rows\nrecord: {rec}\nrunner:\n{}", rows.len(), runner_log());
        std::thread::sleep(Duration::from_secs(1));
    }
    assert!(!runner_log().contains("the server couldn't read a message from this runner"), "a runner from before was told:\n{}", runner_log());
    assert!(record(s.port, &s.admin, "old-unread").get("connected").and_then(Value::as_bool) == Some(true), "it went away");
    Ok(())
}
