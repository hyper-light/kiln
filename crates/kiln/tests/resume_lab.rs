//! Plan step 5 of "No crash or reconnect replays a whole session" (docs/wire.md, "Revisions" and
//! "Restarts"): the lab. Three agent sessions stream on a runner whose link is cut every so often,
//! watched by four viewers, one of which stops reading long enough to fall behind the copy. A viewer drops its socket
//! and comes back; the runner is killed (kill -9) and started again; then so is the server. At the
//! end a checker holds the server's copies and every viewer to each line once, in order. The lab
//! prints what each fault cost in bytes, each within its gap plus 4 KiB a session.
//!
//! The harness is a stand-in for Claude Code (`tests/fixtures/fake-claude`): it calls no model.
//!
//! `cargo test -p kiln --features chaos --test resume_lab --release`

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::Options;
use common::agents::{self, Talk};
use common::lab::{Viewer, copy, field, turns};
use serde_json::Value;

const LINES: u64 = 2000;

/// Whether each of `ids`' turns has ended with its newest turn's every line in its copy.
fn settled(port: u16, admin: &str, ids: &[String]) -> anyhow::Result<bool> {
    for id in ids {
        let (_, info) = common::http(port, "GET", &format!("/api/harness/sessions/{id}"), admin, None)?;
        let info: Value = serde_json::from_str(&info).unwrap_or_default();
        if info.pointer("/last_turn/how").and_then(Value::as_str) != Some("done") {
            return Ok(false);
        }
        let said = turns(&copy(port, admin, id)?);
        if !said.values().any(|l| l.len() as u64 == LINES) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[test]
fn every_frame_once_in_order_through_every_fault() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // Its sessions' live feeds keep 64 frames for their viewers: one that stops reading soon falls
    // behind.
    let mut s = common::start_env(dir.path(), &Options { agent: false, ci: "", extra: "" }, false, &[("KILN_CHAOS", "fanout=64")])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    // Its link cut for 3 s every 9 s, throughout.
    let talk = Talk { lines: LINES as usize, every_ms: 5 };
    let (mut runner, log) = agents::runner(dir.path(), s.port, "lab-runner", Some("drop=9s,down=3s"), talk)?;
    agents::connected(s.port, &s.admin, "lab-runner", &log, Duration::from_secs(120))?;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let w = work.join(format!("s{}", ids.len()));
        std::fs::create_dir_all(&w)?;
        ids.push(agents::open_session(s.port, &s.admin, "lab-runner", &w, Duration::from_secs(90))?);
    }
    let (first, second, third) = match ids.as_slice() {
        [a, b, c] => (a.clone(), b.clone(), c.clone()),
        _ => anyhow::bail!("three sessions: {ids:?}"),
    };
    let mut viewers = vec![
        Viewer::new("v1", &first, None),
        Viewer::new("v2 (stalls)", &first, Some((50, Duration::from_secs(14)))),
        Viewer::new("v3", &second, None),
        Viewer::new("v4", &third, None),
    ];
    for v in &mut viewers {
        v.connect(s.port, &s.admin)?;
    }
    let runner_log = || std::fs::read_to_string(&log).unwrap_or_default();
    let mut costs: Vec<String> = Vec::new();
    let pump = |viewers: &mut Vec<Viewer>, port: u16, admin: &str, for_: Duration| -> anyhow::Result<()> {
        let until = Instant::now() + for_;
        while Instant::now() < until {
            for v in viewers.iter_mut() {
                v.pump(port, admin)?;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    };

    // A viewer drops its socket and comes back, saying what it had.
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(2))?;
    if let Some(v) = viewers.first_mut() {
        v.drop_socket();
    }
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(2))?;
    let v = viewers.first().ok_or_else(|| anyhow::anyhow!("no viewers"))?;
    if v.whole || v.connects != 2 {
        let about = |file: &std::path::Path| std::fs::read_to_string(file).map(|l| { let all: Vec<&str> = l.lines().collect(); all.iter().rev().take(150).rev().map(|x| x.chars().take(260).collect::<String>()).collect::<Vec<_>>().join("\n") }).unwrap_or_default();
        eprintln!("--- the server about {}:\n{}", v.session, about(&dir.path().join("server.log")));
        eprintln!("--- the runner about {}:\n{}", v.session, about(&log));
    }
    assert!(!v.whole && v.connects == 2, "{} came back to {} after {} connections ({} bytes, rev {} of {}), which ended: {:?}", v.name, if v.whole { "the session whole" } else { "a resume" }, v.connects, v.bytes, v.rev, v.born, v.ends);
    costs.push(format!("{} reconnected: {} bytes, {} of them beyond the events changed since and the newest state", v.name, v.bytes, v.beyond));
    assert!(v.beyond <= 4096, "{}", costs.last().cloned().unwrap_or_default());

    // The stalled viewer falls behind its copy's broadcast, and is caught up in-band.
    let stalled = |viewers: &[Viewer]| viewers.get(1).map_or(0, |v| v.caught_up);
    let deadline = Instant::now() + Duration::from_secs(60);
    while stalled(&viewers) == 0 && Instant::now() < deadline {
        pump(&mut viewers, s.port, &s.admin, Duration::from_millis(500))?;
    }
    assert!(stalled(&viewers) > 0, "the viewer that stopped reading wasn't caught up in-band");

    // The runner killed mid-turn and started again on its state.
    let upto = runner_log().len();
    runner.0.kill()?;
    runner.0.wait()?;
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(1))?;
    let (again, _) = agents::runner(dir.path(), s.port, "lab-runner", Some("drop=9s,down=3s"), talk)?;
    drop(runner);
    let runner_kept = again;
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(8))?;
    let starts = |from: usize| -> Vec<String> {
        runner_log().get(from..).unwrap_or_default().lines().filter(|l| l.contains("updates start on the channel") && ids.iter().any(|id| l.contains(id))).map(str::to_owned).collect()
    };
    let after_runner = starts(upto);
    assert!(!after_runner.iter().any(|l| l.contains("on the channel, whole")), "a session went whole after the runner started again:\n{}", after_runner.join("\n"));
    for l in &after_runner {
        let beyond = field(l, "beyond").unwrap_or(usize::MAX);
        costs.push(format!("runner restart: a stream started again, {beyond} bytes beyond its events after the copy's and its newest state"));
        assert!(beyond <= 4096, "{l}");
    }

    // The server killed mid-turn and started again on its data.
    let upto = runner_log().len();
    s.restart()?;
    pump(&mut viewers, s.port, &s.admin, Duration::from_secs(10))?;
    let after_server = starts(upto);
    assert!(!after_server.iter().any(|l| l.contains("on the channel, whole")), "a session went whole after the server started again:\n{}", after_server.join("\n"));
    for l in &after_server {
        let beyond = field(l, "beyond").unwrap_or(usize::MAX);
        costs.push(format!("server restart: a stream started again, {beyond} bytes beyond its events after the copy's and its newest state"));
        assert!(beyond <= 4096, "{l}");
    }

    // Once every session's turns have ended: the copies, and every viewer as it caught up.
    let deadline = Instant::now() + Duration::from_secs(300);
    while !settled(s.port, &s.admin, &ids)? {
        anyhow::ensure!(Instant::now() < deadline, "the sessions never settled\nrunner:\n{}\nserver:\n{}", runner_log(), s.log());
        pump(&mut viewers, s.port, &s.admin, Duration::from_secs(1))?;
    }
    let mut lost = 0usize;
    let mut doubled = 0usize;
    for id in &ids {
        let events = copy(s.port, &s.admin, id)?;
        let mut seen = std::collections::HashSet::new();
        doubled += events.iter().filter_map(|e| e.get("id").and_then(Value::as_str)).filter(|i| !seen.insert(i.to_owned())).count();
        let seqs: Vec<u64> = events.iter().filter_map(|e| e.get("seq").and_then(Value::as_u64)).collect();
        assert!(seqs.windows(2).all(|w| w.first() < w.get(1)), "the copy of {id} out of order");
        let said = turns(&events);
        for (turn, lines) in &said {
            // Each turn's lines once, in order, one after another: a turn the runner's restart cut
            // ends early, and the oldest may begin past its first lines, the copy keeping a
            // bounded window of the timeline.
            let start = lines.first().copied().unwrap_or(0);
            assert_eq!(*lines, (start..start + lines.len() as u64).collect::<Vec<_>>(), "turn {turn:?} of {id} isn't each line once, in order");
        }
        assert!(said.values().any(|l| l.len() as u64 == LINES && l.first() == Some(&0)), "a turn of {id}, whole");
        // Each of its viewers caught up to it: everything the copy has, and nothing newer it lacks
        // (a viewer keeps what the copy let go of past its window).
        let want: BTreeMap<String, Value> = events.iter().filter_map(|e| Some((e.get("id")?.as_str()?.to_owned(), e.clone()))).collect();
        let oldest = events.iter().filter_map(|e| e.get("seq").and_then(Value::as_u64)).min().unwrap_or(0);
        let off = |v: &Viewer| -> (usize, usize) {
            let missing = want.iter().filter(|(k, e)| v.events.get(*k) != Some(*e)).count();
            let extra = v.events.iter().filter(|(k, e)| !want.contains_key(*k) && e.get("seq").and_then(Value::as_u64).is_some_and(|q| q >= oldest)).count();
            (missing, extra)
        };
        let deadline = Instant::now() + Duration::from_secs(90);
        while viewers.iter().any(|v| v.session == *id && off(v) != (0, 0)) && Instant::now() < deadline {
            pump(&mut viewers, s.port, &s.admin, Duration::from_millis(500))?;
        }
        for v in viewers.iter().filter(|v| v.session == *id) {
            let (missing, extra) = off(v);
            lost += missing;
            doubled += extra;
            if (missing, extra) != (0, 0) {
                eprintln!(
                    "{} never caught up to the copy of {id}: {missing} events missing or behind, {extra} it shouldn't have; connected {} times, its connections ended: {:?}; {} tries to connect again failed, the last: {}; {} frames on its socket now, the last {:?}",
                    v.name, v.connects, v.ends, v.failed.0, v.failed.1, v.frames, v.last
                );
            }
        }
    }
    for v in &viewers {
        costs.push(format!("{}: connected {} times, caught up in-band {} times", v.name, v.connects, v.caught_up));
    }
    for c in &costs {
        eprintln!("{c}");
    }
    eprintln!("frames lost: {lost}, doubled: {doubled}, over {} viewers and {} copies", viewers.len(), ids.len());
    let cuts = runner_log().matches("chaos: dropping the connection to the server").count();
    assert!(cuts >= 2, "the link was cut {cuts} times; the lab needs it cut while the sessions stream");
    assert_eq!((lost, doubled), (0, 0), "every frame once, in order");
    drop(runner_kept);
    Ok(())
}
