//! The plan "No crash or reconnect replays a whole session" (docs/wire.md, "Revisions" and
//! "Restarts"). A viewer that reconnects says how much of the server's copy of a session it had
//! (`?after=<born>.<rev>`). It gets only what changed since, not the whole session again. Under
//! another incarnation's number, it gets the session whole. A runner killed mid-session and
//! started again sends only what the server's copy lacks, and the copy ends up holding every event
//! once, in order. So does a server killed mid-session: it starts again with its copies rebuilt
//! from its store, and its runners send only what they lack.
//!
//! The harness is a stand-in for Claude Code (`tests/fixtures/fake-claude`): it calls no model.
//!
//! `cargo test -p kiln --test resume`

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

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

/// The stand-in's messages in `events`, by the process and turn that said them: each's lines, in
/// the order the copy holds them (an event's id is `msg_<pid>_<turn>_<line>:0`).
fn turns(events: &[Value]) -> BTreeMap<(u64, u64), Vec<u64>> {
    let mut out: BTreeMap<(u64, u64), Vec<u64>> = BTreeMap::new();
    for id in events.iter().filter_map(|e| e.get("id").and_then(Value::as_str)) {
        let Some(rest) = id.strip_prefix("msg_").and_then(|r| r.strip_suffix(":0")) else { continue };
        let n: Vec<u64> = rest.split('_').filter_map(|p| p.parse().ok()).collect();
        if let [pid, turn, line] = n.as_slice() {
            out.entry((*pid, *turn)).or_default().push(*line);
        }
    }
    out
}

/// A field of a log line (`key=value`), as a number.
fn field(line: &str, key: &str) -> Option<usize> {
    line.split_whitespace().find_map(|w| w.strip_prefix(&format!("{key}="))).and_then(|v| v.trim_matches('"').parse().ok())
}

/// How many of its turn's `lines` a session's copy holds, once it holds more than ten: the turn is
/// under way, however slowly a loaded machine started it, and the test stops its runner or server
/// there. A fixed wait didn't do: under a full workspace gate, three seconds went by before the
/// turn's first line.
fn mid_turn(s: &common::Server, id: &str, lines: u64) -> anyhow::Result<usize> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (frames, _) = agents::attach(s.port, &s.admin, id, "")?;
        let had = agents::lines(&agents::events(&frames)).len();
        if had > 10 {
            anyhow::ensure!((had as u64) < lines, "the turn was over ({had} lines) before the test could stop it mid-turn");
            return Ok(had);
        }
        anyhow::ensure!(Instant::now() < deadline, "the turn was {had} lines in after a minute: the test needs it mid-turn");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_runner_that_starts_again_sends_only_what_the_server_lacks() -> anyhow::Result<()> {
    const LINES: u64 = 150;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: "" })?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let talk = Talk { lines: LINES as usize, every_ms: 40 };
    let (mut first, log) = agents::runner(dir.path(), s.port, "restart-runner", None, talk)?;
    agents::connected(s.port, &s.admin, "restart-runner", &log, Duration::from_secs(120))?;
    let id = agents::open_session(s.port, &s.admin, "restart-runner", &work, Duration::from_secs(60))?;

    // Killed mid-turn, as a crash would: its session's copy holds part of the turn.
    let had = mid_turn(&s, &id, LINES)?;
    first.0.kill()?;
    first.0.wait()?;
    let runner_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let connections = |text: &str| text.matches("connected to server").count();
    let upto = runner_log().len();
    let seen = connections(&runner_log());

    // Started again on the same state: it continues the session, its cut turn first.
    std::thread::sleep(Duration::from_secs(1));
    let (_again, _) = agents::runner(dir.path(), s.port, "restart-runner", None, talk)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while connections(&runner_log()) == seen {
        anyhow::ensure!(Instant::now() < deadline, "the runner didn't connect again:\n{}", runner_log());
        std::thread::sleep(Duration::from_millis(100));
    }
    // Once its new turn has ended, the copy holds the cut turn's lines and the new turn's, each
    // once, in order.
    let deadline = Instant::now() + Duration::from_secs(180);
    let (events, said) = loop {
        let (_, info) = common::http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
        let info: Value = serde_json::from_str(&info).unwrap_or_default();
        let (frames, _) = agents::attach(s.port, &s.admin, &id, "")?;
        let events = agents::events(&frames);
        let said = turns(&events);
        let whole = said.values().filter(|lines| lines.len() as u64 == LINES).count();
        if info.pointer("/last_turn/how").and_then(Value::as_str) == Some("done") && said.len() == 2 && whole >= 1 {
            break (events, said);
        }
        anyhow::ensure!(Instant::now() < deadline, "the session never finished its turn after the restart\nsession: {info}\nturns: {said:?}\nrunner:\n{}", runner_log());
        std::thread::sleep(Duration::from_secs(1));
    };
    let ids: Vec<&str> = events.iter().filter_map(|e| e.get("id").and_then(Value::as_str)).collect();
    let mut once = ids.clone();
    once.sort_unstable();
    once.dedup();
    assert_eq!(once.len(), ids.len(), "an event twice in the copy");
    let seqs: Vec<u64> = events.iter().filter_map(|e| e.get("seq").and_then(Value::as_u64)).collect();
    assert!(seqs.windows(2).all(|w| w.first() < w.get(1)), "the copy's events out of order: {seqs:?}");
    let (cut, new): (Vec<_>, Vec<_>) = said.values().partition(|lines| (lines.len() as u64) < LINES);
    let cut = cut.first().cloned().cloned().unwrap_or_default();
    assert_eq!(cut, (0..cut.len() as u64).collect::<Vec<_>>(), "the cut turn's lines, each once, in order");
    assert!(cut.len() + 5 >= had, "the cut turn kept {} of the {had} lines the copy had", cut.len());
    assert_eq!(new.first().map(|l| l.as_slice()), Some((0..LINES).collect::<Vec<_>>().as_slice()), "the new turn's lines, each once, in order");

    // The runner sent only what the copy lacked: not the session whole, and beyond its events
    // after the copy's newest and its newest state, at most 4 KiB.
    let since = runner_log().get(upto..).unwrap_or_default().to_owned();
    let start: Vec<&str> = since.lines().filter(|l| l.contains("updates start on the channel") && l.contains(&id)).collect();
    assert!(!start.iter().any(|l| l.contains("on the channel, whole")), "the session went whole after the restart:\n{}", start.join("\n"));
    let delta = start.iter().find(|l| l.contains("after what the server's copy has")).ok_or_else(|| anyhow::anyhow!("no stream started after the restart:\n{since}"))?;
    let beyond = field(delta, "beyond").ok_or_else(|| anyhow::anyhow!("no beyond= in {delta}"))?;
    assert!(beyond <= 4096, "the restarted runner sent {beyond} bytes beyond the events after the copy's and its newest state: {delta}");
    // And the server's copy went on: it started from its runner's Open once, the first time.
    let server = std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
    let fresh = server.lines().filter(|l| l.contains("an agent session's copy starts from its machine") && l.contains(&id)).count();
    let went_on = server.lines().filter(|l| l.contains("an agent session's copy goes on from its machine") && l.contains(&id)).count();
    assert_eq!((fresh, went_on), (1, 1), "the copy started over {fresh} times and went on {went_on} times");
    Ok(())
}

#[test]
fn a_server_that_starts_again_has_its_runners_send_only_what_its_copies_lack() -> anyhow::Result<()> {
    const LINES: u64 = 200;
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options { agent: false, ci: "", extra: "" })?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "steady-runner", None, Talk { lines: LINES as usize, every_ms: 50 })?;
    agents::connected(s.port, &s.admin, "steady-runner", &log, Duration::from_secs(120))?;
    let id = agents::open_session(s.port, &s.admin, "steady-runner", &work, Duration::from_secs(60))?;

    // Killed mid-turn, as a crash would; the turn goes on on its runner meanwhile.
    mid_turn(&s, &id, LINES)?;
    let runner_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let server_log = || std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
    let connections = |text: &str| text.matches("connected to server").count();
    let (upto, seen, server_upto) = (runner_log().len(), connections(&runner_log()), server_log().len());
    s.restart()?;

    // Started again, it has the session's copy at once, rebuilt from its store.
    let (rebuilt, _) = agents::attach(s.port, &s.admin, &id, "")?;
    anyhow::ensure!(!agents::lines(&agents::events(&rebuilt)).is_empty(), "the copy wasn't rebuilt from the store: {:?}", rebuilt.first());
    // Its runner comes back, and the turn ends with every line in the copy once, in order.
    let deadline = Instant::now() + Duration::from_secs(120);
    while connections(&runner_log()) == seen {
        anyhow::ensure!(Instant::now() < deadline, "the runner didn't connect again:\n{}", runner_log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let deadline = Instant::now() + Duration::from_secs(180);
    let said = loop {
        let (_, info) = common::http(s.port, "GET", &format!("/api/harness/sessions/{id}"), &s.admin, None)?;
        let info: Value = serde_json::from_str(&info).unwrap_or_default();
        let (frames, _) = agents::attach(s.port, &s.admin, &id, "")?;
        let said = turns(&agents::events(&frames));
        if info.pointer("/last_turn/how").and_then(Value::as_str) == Some("done") && said.values().any(|l| l.len() as u64 >= LINES) {
            break said;
        }
        anyhow::ensure!(Instant::now() < deadline, "the turn never ended in the copy\nsession: {info}\nturns: {said:?}\nrunner:\n{}", runner_log());
        std::thread::sleep(Duration::from_secs(1));
    };
    assert_eq!(said.len(), 1, "one turn: {said:?}");
    assert_eq!(said.values().next().cloned().unwrap_or_default(), (0..LINES).collect::<Vec<_>>(), "every line once, in order");

    // Its runner sent only what the rebuilt copy lacked: not the session whole, and at most 4 KiB
    // beyond its events after the copy's newest and its newest state.
    let since = runner_log().get(upto..).unwrap_or_default().to_owned();
    let start: Vec<&str> = since.lines().filter(|l| l.contains("updates start on the channel") && l.contains(&id)).collect();
    assert!(!start.iter().any(|l| l.contains("on the channel, whole")), "the session went whole after the restart:\n{}", start.join("\n"));
    let delta = start.iter().find(|l| l.contains("after what the server's copy has")).ok_or_else(|| anyhow::anyhow!("no stream started again after the restart:\n{since}"))?;
    let beyond = field(delta, "beyond").ok_or_else(|| anyhow::anyhow!("no beyond= in {delta}"))?;
    assert!(beyond <= 4096, "the runner sent {beyond} bytes beyond the events after the copy's and its newest state: {delta}");
    // And the server's new copy went on from what its store had: it never started over.
    let after = server_log().get(server_upto..).unwrap_or_default().to_owned();
    let fresh = after.lines().filter(|l| l.contains("an agent session's copy starts from its machine") && l.contains(&id)).count();
    let went_on = after.lines().filter(|l| l.contains("an agent session's copy goes on from its machine") && l.contains(&id)).count();
    assert_eq!((fresh, went_on), (0, 1), "after the restart the copy started over {fresh} times and went on {went_on} times");
    Ok(())
}
