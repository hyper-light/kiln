//! The events socket's feed (docs/wire.md, "The events socket"): topics, coalescing, resume, and
//! what a burst costs a page before and after (W21-R15: what the page waits on is what it's sent).

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::wire::events::ServerEvent;

fn frame(seq: u64, v: Value) -> Arc<Frame> {
    Arc::new(Frame::new(seq, Arc::from(v.to_string())))
}

/// The frames of a batch's text, and where it says the feed is.
fn read(batch: &str) -> anyhow::Result<(String, Vec<Value>)> {
    let v: Value = serde_json::from_str(batch)?;
    anyhow::ensure!(v.get("t").and_then(Value::as_str) == Some("batch"), "not a batch: {batch}");
    let at = v.get("at").and_then(Value::as_str).unwrap_or_default().to_owned();
    let events = v.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok((at, events))
}

fn kinds(events: &[Value]) -> Vec<String> {
    events.iter().map(|e| e.get("type").and_then(Value::as_str).unwrap_or_default().to_owned()).collect()
}

const AT: At = At { epoch: 7, seq: 0 };

/// A batch is what `EventsDown::Batch` serializes to, though it's written from the frames' text.
#[test]
fn events_batch_is_its_type() -> anyhow::Result<()> {
    let events = [ServerEvent::Updates, ServerEvent::Queue(crate::wire::events::Queue::Counts { queued: 2, running: 1 }), ServerEvent::HarnessClosed { id: "a1".into() }];
    let texts: Vec<String> = events.iter().map(ServerEvent::text).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let written: Value = serde_json::from_str(&batch(At { epoch: 9, seq: 41 }, &refs))?;
    let typed = serde_json::to_value(crate::wire::events::EventsDown::Batch { at: "9.41".into(), events: events.to_vec() })?;
    anyhow::ensure!(written == typed, "{written} isn't {typed}");
    let empty: Value = serde_json::from_str(&batch(At { epoch: 9, seq: 0 }, &[]))?;
    anyhow::ensure!(empty == json!({ "t": "batch", "at": "9.0", "events": [] }), "{empty}");
    Ok(())
}

/// What the filters read of a frame is what they read of its JSON before: the repository it names
/// (a name, `{name}`, or its run's), a leg's run, an inbox's person, an agent's id.
#[test]
fn events_meta_reads_what_the_filters_read() {
    let m = Meta::of(&json!({ "type": "ci_image", "repo": "acme/app", "name": "x" }).to_string());
    assert_eq!((m.kind.as_str(), m.repo.as_deref(), m.parsed), ("ci_image", Some("acme/app"), true));
    let m = Meta::of(&json!({ "type": "run.failed", "repo": { "id": "1", "name": "acme/app" }, "run": { "id": "r" } }).to_string());
    assert_eq!(m.repo.as_deref(), Some("acme/app"));
    let m = Meta::of(&json!({ "type": "run", "run": { "id": "r1", "repo_name": "acme/web" } }).to_string());
    assert_eq!((m.repo.as_deref(), m.key.clone()), (Some("acme/web"), Some(Key::State("run:r1".into()))));
    let m = Meta::of(&json!({ "type": "job", "job": { "id": "j1", "run_id": "r1" } }).to_string());
    assert_eq!((m.repo.as_deref(), m.job_run.as_deref()), (None, Some("r1")));
    let m = Meta::of(&json!({ "type": "inbox", "person": "user:ada" }).to_string());
    assert_eq!(m.person.as_deref(), Some("user:ada"));
    assert_eq!(Meta::of(&json!({ "type": "harness", "info": { "id": "a1" } }).to_string()).agent, Agent::Of(Some("a1".into())));
    assert_eq!(Meta::of(&json!({ "type": "harness_closed", "id": "a1" }).to_string()).agent, Agent::Of(Some("a1".into())));
    assert_eq!(Meta::of(&json!({ "type": "agent_change", "agent": "a1" }).to_string()).agent, Agent::Of(Some("a1".into())));
    assert_eq!(Meta::of(&json!({ "type": "harness" }).to_string()).agent, Agent::Of(None));
    assert_eq!(Meta::of(&json!({ "type": "agent", "agent": { "name": "box" } }).to_string()).agent, Agent::Free);
    let m = Meta::of("not json");
    assert!(!m.parsed && m.kind.is_empty() && m.key.is_none());
}

/// Only frames of a thing's whole state or "fetch it again" coalesce: notices, samples, parts of
/// lists don't.
#[test]
fn events_only_state_and_refetches_coalesce() {
    let key = |v: Value| Meta::of(&v.to_string()).key;
    assert_eq!(key(json!({ "type": "harness", "info": { "id": "a" } })), Some(Key::State("harness:a".into())));
    assert_eq!(key(json!({ "type": "harness_closed", "id": "a" })), Some(Key::State("harness:a".into())), "its going is the same thing's");
    assert_eq!(key(json!({ "type": "agent", "agent": { "name": "box" } })), Some(Key::State("agent:box".into())));
    assert_eq!(key(json!({ "type": "update_job", "job": { "id": "u" } })), Some(Key::State("update_job:u".into())));
    assert_eq!(key(json!({ "type": "queue", "queued": 1, "running": 0 })), Some(Key::Latest("queue".into())));
    assert_eq!(key(json!({ "type": "queue", "entry": "e1", "state": "testing" })), Some(Key::State("queue:e1".into())));
    assert!(matches!(key(json!({ "type": "updates" })), Some(Key::Latest(_))));
    assert!(matches!(key(json!({ "type": "front_door", "runner": "box" })), Some(Key::Latest(_))));
    for never in [
        json!({ "type": "run.failed", "repo": { "name": "acme/app" }, "run": { "id": "r" } }),
        json!({ "type": "sample", "agent": "box", "sample": {} }),
        json!({ "type": "cd_apps", "apps": [] }),
        json!({ "type": "comment", "diff": "D1" }),
        json!({ "type": "agent.needs_you", "agent": { "id": "a" } }),
        json!({ "type": "resync" }),
        json!({ "type": "something_new", "id": "x" }),
    ] {
        assert_eq!(key(never.clone()), None, "{never}");
    }
}

/// Topics: a page gets the types it asked for, `resync` always; asking for new ones says which.
#[test]
fn events_topics_filter() {
    let mut out = Outbox::new(Some(topics("run, job,,harness")));
    assert!(out.wants("run") && out.wants("job") && out.wants("harness") && out.wants("resync"));
    assert!(!out.wants("sample") && !out.wants("agent"));
    let added = out.set_topics(Some(topics("run,agent")));
    assert_eq!(added, topics("agent"));
    assert!(!out.wants("job") && out.wants("agent"));
    // Every type, and from every type to some: nothing new to catch up on.
    assert!(out.set_topics(None).is_empty());
    assert!(out.wants("sample"));
    assert!(out.set_topics(Some(topics("run"))).is_empty());
    assert!(Outbox::new(None).wants("anything"));
}

/// Coalescing keeps each thing's newest, where the newest was, and every frame that isn't a
/// thing's: different things keep their order.
#[test]
fn events_coalesce_keeps_the_newest_in_order() -> anyhow::Result<()> {
    let mut out = Outbox::new(None);
    let fs = [
        json!({ "type": "job", "job": { "id": "j1", "status": "queued" } }),
        json!({ "type": "run", "run": { "id": "r1", "status": "running" } }),
        json!({ "type": "run.failed", "repo": { "name": "a" }, "run": { "id": "r0" } }),
        json!({ "type": "job", "job": { "id": "j2", "status": "running" } }),
        json!({ "type": "job", "job": { "id": "j1", "status": "running" } }),
        json!({ "type": "updates" }),
        json!({ "type": "harness", "info": { "id": "a1", "doing": "reading" } }),
        json!({ "type": "updates" }),
        json!({ "type": "harness_closed", "id": "a1" }),
        json!({ "type": "sample", "agent": "box", "sample": { "cpu": 1 } }),
        json!({ "type": "sample", "agent": "box", "sample": { "cpu": 2 } }),
    ];
    for (i, f) in fs.iter().enumerate() {
        out.add(frame(i as u64 + 1, f.clone()));
    }
    let text = out.flush(AT, false).ok_or_else(|| anyhow::anyhow!("nothing sent"))?;
    let (_, events) = read(&text)?;
    let ids: Vec<String> = events
        .iter()
        .map(|e| {
            let k = e.get("type").and_then(Value::as_str).unwrap_or_default();
            let id = e.pointer("/job/id").or_else(|| e.pointer("/run/id")).or_else(|| e.get("id")).or_else(|| e.pointer("/sample/cpu")).map(|v| v.to_string().replace('"', "")).unwrap_or_default();
            format!("{k}:{id}")
        })
        .collect();
    assert_eq!(ids, ["run:r1", "run.failed:r0", "job:j2", "job:j1", "updates:", "harness_closed:a1", "sample:1", "sample:2"], "{text}");
    // The newest of j1, not the first.
    assert_eq!(events.get(3).and_then(|e| e.pointer("/job/status")), Some(&json!("running")));
    Ok(())
}

/// A thing's frame identical to what the socket was last sent of it isn't sent again; a refetch is,
/// each batch; a change of topics or a `resync` forgets what was sent.
#[test]
fn events_identical_frames_are_dropped() -> anyhow::Result<()> {
    let run = |status: &str| json!({ "type": "run", "run": { "id": "r1", "status": status } });
    let mut out = Outbox::new(None);
    out.add(frame(1, run("running")));
    out.add(frame(2, json!({ "type": "updates" })));
    let (_, e) = read(&out.flush(AT, false).unwrap_or_default())?;
    assert_eq!(kinds(&e), ["run", "updates"]);
    out.add(frame(3, run("running")));
    out.add(frame(4, json!({ "type": "updates" })));
    let (_, e) = read(&out.flush(AT, false).unwrap_or_default())?;
    assert_eq!(kinds(&e), ["updates"], "the same run again isn't sent; a refetch is");
    // Nothing left to say: nothing sent, unless it's the socket's first.
    out.add(frame(5, run("running")));
    assert_eq!(out.flush(AT, false), None);
    out.add(frame(6, run("running")));
    let (at, e) = read(&out.flush(At { epoch: 7, seq: 6 }, true).unwrap_or_default())?;
    assert_eq!((at.as_str(), e.len()), ("7.6", 0));
    out.add(frame(7, run("succeeded")));
    let (_, e) = read(&out.flush(AT, false).unwrap_or_default())?;
    assert_eq!(e.first().and_then(|e| e.pointer("/run/status")), Some(&json!("succeeded")));
    // Back to what it was: a change, sent.
    out.add(frame(8, run("running")));
    assert!(out.flush(AT, false).is_some());
    // Topics changed: what was sent is forgotten (it may have missed a change meanwhile).
    out.set_topics(Some(topics("run")));
    out.add(frame(9, run("running")));
    assert!(out.flush(AT, false).is_some(), "after its topics changed, the same frame goes");
    out.resync();
    out.add(frame(10, run("running")));
    let (_, e) = read(&out.flush(AT, false).unwrap_or_default())?;
    assert_eq!(kinds(&e), ["resync", "run"], "a resync forgets what was sent too: the same frame goes, after it");
    Ok(())
}

/// A resync goes first, and what was gathered before it goes with it.
#[test]
fn events_overflow_says_resync() -> anyhow::Result<()> {
    let mut out = Outbox::new(None);
    out.add(frame(1, json!({ "type": "updates" })));
    out.resync();
    out.add(frame(9, json!({ "type": "places" })));
    let (_, e) = read(&out.flush(AT, false).unwrap_or_default())?;
    assert_eq!(kinds(&e), ["resync", "places"]);
    Ok(())
}

/// Frames of types newly asked for, from before, go in their place by number.
#[test]
fn events_earlier_frames_keep_their_order() -> anyhow::Result<()> {
    let mut out = Outbox::new(Some(topics("run")));
    out.add(frame(5, json!({ "type": "run", "run": { "id": "r" } })));
    out.add_earlier([frame(3, json!({ "type": "job", "job": { "id": "j" } })), frame(6, json!({ "type": "job", "job": { "id": "k" } }))]);
    let (_, e) = read(&out.flush(AT, false).unwrap_or_default())?;
    assert_eq!(kinds(&e), ["job", "run", "job"]);
    Ok(())
}

/// The ring: what's after a number, a gap once the oldest went, and a number from a feed that's
/// not this one (another server process) isn't taken.
#[test]
fn events_resume_across_a_gap_and_an_epoch() {
    let feed = Feed::with_capacity(42, 4);
    assert_eq!(feed.head(), 0);
    assert!(matches!(feed.since(0), Since::Frames(f) if f.is_empty()));
    assert_eq!(feed.resume(None), (0, false));
    assert_eq!(feed.resume(Some("42.0")), (0, false));
    for i in 0..3 {
        feed.push(Arc::from(json!({ "type": "updates", "i": i }).to_string()));
    }
    assert_eq!(feed.at(), At { epoch: 42, seq: 3 });
    let seqs = |s: Since| match s {
        Since::Frames(f) => Some(f.iter().map(|f| f.seq).collect::<Vec<_>>()),
        Since::Gap => None,
    };
    assert_eq!(seqs(feed.since(1)), Some(vec![2, 3]));
    assert_eq!(seqs(feed.since(3)), Some(vec![]));
    assert_eq!(seqs(feed.since(4)), None, "a number past the head isn't this feed's");
    // Resumes where it was.
    assert_eq!(feed.resume(Some("42.1")), (1, false));
    // Another server's number, or one that doesn't read: from now, told `resync`.
    assert_eq!(feed.resume(Some("41.1")), (3, true));
    assert_eq!(feed.resume(Some("nonsense")), (3, true));
    for i in 3..7 {
        feed.push(Arc::from(json!({ "type": "updates", "i": i }).to_string()));
    }
    // 7 pushed, 4 kept (4..=7): after 3 is whole, after 2 is a gap.
    assert_eq!(seqs(feed.since(3)), Some(vec![4, 5, 6, 7]));
    assert_eq!(seqs(feed.since(2)), None);
    assert_eq!(feed.resume(Some("42.2")), (7, true));
    assert_eq!(feed.resume(Some("42.3")), (3, false));
    assert_eq!(feed.kept().len(), 4);
}

/// The ring is the size of the broadcast it stands beside (its justification).
#[test]
fn events_ring_is_the_broadcasts_size() -> anyhow::Result<()> {
    let hub = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/hub.rs"))?;
    anyhow::ensure!(hub.contains(&format!("events: Broadcast::new({RING})")), "the events broadcast's size isn't the feed's RING ({RING})");
    Ok(())
}

/// A socket's sends of `events` (each when it's emitted, in ms), as `pump_events` makes them: each
/// frame at once (before), or batched (after): a send at once after a quiet window, else at the
/// window's end, or once [`MAX_BATCH`] are gathered. Its frames and bytes.
fn sends(events: &[(u64, Arc<str>)], topics_: Option<&str>, batched: bool) -> (usize, usize) {
    let mut out = Outbox::new(topics_.map(topics));
    let window = u64::try_from(WINDOW.as_millis()).unwrap_or(50);
    let (mut frames, mut bytes) = (0, 0);
    let mut sent_at: Option<u64> = None;
    let mut due: Option<u64> = None;
    let send = |out: &mut Outbox, seq: u64, frames: &mut usize, bytes: &mut usize| {
        if let Some(t) = out.flush(At { epoch: 1, seq }, false) {
            *frames += 1;
            *bytes += t.len();
        }
    };
    for (i, (t, text)) in events.iter().enumerate() {
        let seq = i as u64 + 1;
        if let Some(d) = due.filter(|d| *d <= *t) {
            send(&mut out, seq - 1, &mut frames, &mut bytes);
            sent_at = Some(d);
            due = None;
        }
        let f = Arc::new(Frame::new(seq, Arc::clone(text)));
        if !out.wants(&f.meta().kind) {
            continue;
        }
        if !batched {
            frames += 1;
            bytes += text.len();
            continue;
        }
        out.add(f);
        let when = due.unwrap_or_else(|| sent_at.map_or(*t, |s| (s + window).max(*t)));
        if when <= *t || out.len() >= MAX_BATCH {
            send(&mut out, seq, &mut frames, &mut bytes);
            sent_at = Some(*t);
            due = None;
        } else {
            due = Some(when);
        }
    }
    if !out.is_empty() {
        send(&mut out, events.len() as u64, &mut frames, &mut bytes);
    }
    (frames, bytes)
}

/// An agent's entry as `harness` carries it, at its `n`th change.
fn agent_entry(i: usize, n: usize) -> anyhow::Result<Arc<str>> {
    let info: crate::harness::session::Info = serde_json::from_value(json!({
        "id": format!("01a19{i:027x}"),
        "kind": "claude",
        "kind_name": "Claude Code",
        "target": "agent:adacachy",
        "name": format!("perf/wire-{i}"),
        "cwd": format!("/home/ada/.cache/kiln-wire/w{i}"),
        "tree": format!("/home/ada/.cache/kiln-wire/w{i}"),
        "created_at": 1_793_000_000_000_i64,
        "status": "running",
        "title": "Make the events socket subscribable, batched and resumable",
        "model": "claude-opus-5-5",
        "mode": "bypassPermissions",
        "harness_session": "5b0f0c5e-2b8e-4a4e-9d55-2a3c1e0f9a11",
        "tasks_running": 1,
        "cost_usd": (1.25 + n as f64 * 0.013),
        "activity": "tool",
        "last_activity": (1_793_000_100_000_i64 + n as i64 * 400),
        "goal": "A tab receives only the events it uses, batched, and a reconnect resumes instead of reloading everything.",
        "summary": format!("Reading crates/kiln/src/api.rs around pump_events to see how Narrow filters each frame ({n})."),
        "doing": format!("Running cargo clippy -p kiln ({n})"),
        "todos_done": (n % 7),
        "todos_total": 9,
        "files_changed": 4,
        "lines_added": (120 + n),
        "lines_removed": 31,
        "context_pct": (40 + (n % 50)),
        "turn_started": 1_793_000_090_000_i64,
        "account": "ada@arcee.ai",
        "plan": "max",
    }))?;
    Ok(Arc::from(ServerEvent::Harness { info: Box::new(info) }.text()))
}

/// A leg as `job` carries it (its steps, at `step`).
fn leg(i: usize, status: &str, step: usize) -> anyhow::Result<Arc<str>> {
    let steps: Vec<Value> = (0..4)
        .map(|s| {
            let st = if s < step { "succeeded" } else if s == step { status } else { "pending" };
            let name = ["Set up", "Checkout", "cargo test", "Upload"].get(s).copied().unwrap_or("step");
            json!({ "idx": s, "name": name, "status": st, "started_at": null, "finished_at": null, "exit_code": null })
        })
        .collect();
    let job: crate::store::Job = serde_json::from_value(json!({
        "id": format!("01a1a{i:027x}"),
        "run_id": "01a1a000000000000000000000000run",
        "key": format!("test ({i})"),
        "display": format!("test (shard {i})"),
        "status": status,
        "labels": ["linux", "x86_64"],
        "needs": ["build"],
        "agent": format!("runner-{}", i % 8),
        "executor": "docker",
        "attempt": 1,
        "queued_at": 1_793_000_000,
        "started_at": 1_793_000_010,
        "finished_at": null,
        "exit_code": null,
        "error": null,
        "peak_rss": null,
        "cpu_seconds": null,
        "steps": steps,
    }))?;
    Ok(Arc::from(ServerEvent::Job { job: Box::new(job) }.text()))
}

fn the_run(status: &str) -> anyhow::Result<Arc<str>> {
    let run: crate::store::Run = serde_json::from_value(json!({
        "id": "01a1a000000000000000000000000run", "repo_id": "1", "repo_name": "hyper-light/kiln", "number": 4812, "pipeline": ".kiln/ci.yaml", "event": "push", "branch": "perf/wire-events",
        "sha": "01d3ff1c99b0000000000000000000000000000", "message": "Merge g8b-finish", "author": "adalundhe", "status": status, "created_at": 1_793_000_000, "started_at": 1_793_000_001, "finished_at": null, "error": null, "concurrency_group": null,
    }))?;
    Ok(Arc::from(ServerEvent::Run { run: Box::new(run) }.text()))
}

/// A runner's record as `agent` carries it (its legs running).
fn runner(r: usize, running: &[String], seen: i64) -> Arc<str> {
    Arc::from(
        json!({ "type": "agent", "agent": { "name": format!("runner-{r}"), "label": null, "peer": null, "labels": ["linux", "x86_64", "docker"], "slots": 8, "hostname": format!("runner-{r}"), "os": "linux", "arch": "x86_64", "version": "0.2.0 (01d3ff1c99b)", "ephemeral": false, "connected": true, "last_seen": seen, "enrolled_at": 1_792_000_000, "running": running, "busy": running.len(), "draining": false, "sample": null, "battery": null, "dropped": [], "dropped_toward": [], "refused": null, "terminal": true } })
            .to_string(),
    )
}

/// What a burst costs a page, before (every frame as it comes) and after (batched, coalesced,
/// identical ones dropped; and on a page that doesn't use the burst's types). Measured, printed,
/// and held: the after is fewer frames and fewer bytes, and never more.
#[test]
fn events_burst_costs_fewer_frames_and_bytes() -> anyhow::Result<()> {
    // 50 agents at work for 10 s: each changes what it's doing in bursts of 3 (a tool's start, its
    // output, its end, 10 ms apart), once a second, their seconds spread over the agents.
    let mut agents = Vec::new();
    for i in 0..50_usize {
        for s in 0..10_usize {
            for b in 0..3_usize {
                let t = (s * 1000 + i * 20 + b * 10) as u64;
                agents.push((t, agent_entry(i, s * 3 + b)?));
            }
        }
    }
    agents.sort_by_key(|(t, _)| *t);
    // CI: a run of 200 legs on 8 runners over 60 s. Each leg: dispatched (its leg, its runner,
    // the run), 3 steps moving on (its leg), finished (its runner, its leg, the run), as the hub
    // emits them (`Hub::finish_job`: agent, job, run). The run's frames say the same until it ends.
    let mut ci = Vec::new();
    let mut running: Vec<Vec<String>> = vec![Vec::new(); 8];
    for i in 0..200_usize {
        let r = i % 8;
        let start = (i * 250) as u64;
        let id = format!("01a1a{i:027x}");
        if let Some(rs) = running.get_mut(r) {
            rs.push(id.clone());
        }
        let now = running.get(r).cloned().unwrap_or_default();
        ci.push((start, leg(i, "running", 0)?));
        ci.push((start + 1, runner(r, &now, start as i64)));
        ci.push((start + 2, the_run("running")?));
        for s in 1..4 {
            ci.push((start + s as u64 * 700, leg(i, "running", s)?));
        }
        if let Some(rs) = running.get_mut(r) {
            rs.retain(|j| *j != id);
        }
        let now = running.get(r).cloned().unwrap_or_default();
        let end = start + 2900;
        ci.push((end, runner(r, &now, end as i64)));
        ci.push((end + 1, leg(i, "succeeded", 4)?));
        ci.push((end + 2, the_run(if i == 199 { "succeeded" } else { "running" })?));
    }
    ci.sort_by_key(|(t, _)| *t);
    // The mix the budgets track measured on a busy server (perf/wire-budgets,
    // `w21_r15_events_ship_and_api_budgets`): headroom 11-19 frames/s from 2 runners (65 B), legs
    // 2.6-3.5/s (843 B), agents 1.9/s (~990 B), samples 0.6/s; here 15, 3, 2 and 0.6 a second for
    // 60 s. Most tabs show no headroom: they're asked for every other type.
    let mut mix = Vec::new();
    for k in 0..900_u64 {
        let runner = format!("runner-{}", k % 2);
        mix.push((k * 1000 / 15, Arc::from(ServerEvent::Headroom { runner }.text())));
    }
    for k in 0..180_usize {
        mix.push(((k as u64) * 1000 / 3, leg(k % 24, "running", k % 4)?));
    }
    for k in 0..120_usize {
        mix.push(((k as u64) * 500 + 7, agent_entry(k % 6, k)?));
    }
    for k in 0..36_u64 {
        let sample = json!({ "type": "sample", "agent": format!("runner-{}", k % 2), "sample": { "ts": 1_793_000_000 + k, "cpu_pct": 41.5, "load1": 7.2, "mem_used": 34_436_644_864_u64, "mem_total": 68_719_476_736_u64, "disk_used": 900_000_000_000_u64, "net_rx": 12_345_678, "net_tx": 2_345_678 } });
        mix.push((k * 1667 + 3, Arc::from(sample.to_string())));
    }
    mix.sort_by_key(|(t, _)| *t);
    let before = sends(&mix, None, false);
    let after = sends(&mix, None, true);
    let elsewhere = sends(&mix, Some("job,harness,harness_closed,sample,run,agent,queue"), true);
    println!("the measured mix, 60 s: {} events; before {} frames, {} bytes; after {} frames, {} bytes; on a tab without headroom {} frames, {} bytes", mix.len(), before.0, before.1, after.0, after.1, elsewhere.0, elsewhere.1);
    let mut wrong = Vec::new();
    if !(after.0 < before.0 && elsewhere.0 < after.0 && elsewhere.1 < before.1) {
        wrong.push(format!("the measured mix: before {before:?}, after {after:?}, elsewhere {elsewhere:?}"));
    }
    for (name, burst, other) in [("50 agents, 10 s", &agents, "run,job,agent,queue"), ("CI, 200 legs on 8 runners", &ci, "harness,harness_closed,queue")] {
        let before = sends(burst, None, false);
        let after = sends(burst, None, true);
        let elsewhere = sends(burst, Some(other), true);
        println!("{name}: {} events; before {} frames, {} bytes; after {} frames, {} bytes; on a page without its types {} frames, {} bytes", burst.len(), before.0, before.1, after.0, after.1, elsewhere.0, elsewhere.1);
        if !(after.0 < before.0 && after.1 <= before.1 && elsewhere.1 < after.1) {
            wrong.push(format!("{name}: before {before:?}, after {after:?}, elsewhere {elsewhere:?}"));
        }
    }
    anyhow::ensure!(wrong.is_empty(), "{}", wrong.join("\n"));
    Ok(())
}
