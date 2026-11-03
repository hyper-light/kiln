//! bench session_feed: what the server's copy of an agent session costs to keep and to show
//! (docs/wire.md, "Revisions"; harness/registry.rs, harness/live.rs). A long session, as a lead
//! agent's is after days: its kept events (`KILN_FEED_EVENTS`, 4,000: notes, thoughts, reads and
//! commands with their output, edits with their diffs), a state with its queue and tasks, its
//! harness's catalog, and a changeset of 200 files (about 700 KB). Then, with a viewer watching:
//! - a viewer's snapshot (`attach`), its frames as the socket sends them, and packed as a page
//!   asks (gzip, `?z=gzip`);
//! - a viewer catching up (`frames_since`) past 50 updates;
//! - what each kind of update costs the copy and its feed: events one at a time, a reply's words
//!   streaming in, states (activity, usage), and changeset patches (a file at a time);
//! - the viewer reading what those updates sent it, and the frames as it takes them.
//!
//! For each case: the median time of `KILN_FEED_RUNS` runs (5), and in the last of them the allocations
//! and reallocations, the bytes allocated, the peak of live bytes, and the page faults, all across
//! the process (the runtime's threads and the blocking pool too), per item where the case has many.
//!
//! ```text
//! cargo bench -p kiln --bench session_feed [-- <a part of the cases' names>]
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use hyper_measure::{alloc, faults};
use kiln::harness::core::Update;
use kiln::harness::model::{Body, Event, FileChange, FileDiff, FileKey, Hunk, Level, State, ToolCall, ToolKind, ToolStatus};
use kiln::harness::registry::Location;
use kiln::harness::session::Info;
use kiln::hub::Hub;
use serde_json::json;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const ID: &str = "bench-long";

/// A deterministic stream of numbers (no seed from outside: every run the same session).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const WORDS: &[&str] = &[
    "the", "retry", "loop", "reads", "its", "deadline", "from", "same", "clock", "as", "backoff", "so", "a", "slow", "tick", "can", "no", "longer", "put", "before", "first",
    "attempt", "`crates/kiln/src/harness/registry.rs`", "viewer", "snapshot", "feed", "and", "then", "which", "is", "why", "tests", "pass", "**now**", "with", "every", "frame",
];

fn word(i: usize) -> &'static str {
    WORDS.get(i % WORDS.len()).copied().unwrap_or("the")
}

fn words(r: &mut Lcg, n: usize) -> String {
    let mut s = String::with_capacity(n * 7);
    for i in 0..n {
        if i > 0 {
            s.push(if i % 17 == 0 { '\n' } else { ' ' });
        }
        s.push_str(word(r.below(WORDS.len() as u64) as usize));
    }
    s
}

/// `base` words and up to `spread` more.
fn some_words(r: &mut Lcg, base: usize, spread: u64) -> String {
    let n = base + r.below(spread) as usize;
    words(r, n)
}

fn lines(r: &mut Lcg, n: usize, prefix: &str) -> String {
    (0..n).map(|k| format!("{prefix}{k:04} {}", some_words(r, 6, 8))).collect::<Vec<_>>().join("\n")
}

/// `base` lines and up to `spread` more.
fn some_lines(r: &mut Lcg, base: usize, spread: u64, prefix: &str) -> String {
    let n = base + r.below(spread) as usize;
    lines(r, n, prefix)
}

fn hunk(r: &mut Lcg, at: u32, n: usize) -> Hunk {
    let lines: Vec<String> = (0..n)
        .map(|k| {
            let mark = match k % 5 {
                1 => '-',
                2 | 3 => '+',
                _ => ' ',
            };
            format!("{mark}    let deadline_{k} = clock.now() + budget_{};", r.below(1000))
        })
        .collect();
    Hunk { old_start: at, old_lines: n as u32, new_start: at, new_lines: n as u32, lines }
}

fn tool(r: &mut Lcg, i: u64, at: i64) -> ToolCall {
    let pick = r.below(10);
    let (name, kind, title, output, diffs) = match pick {
        0..=2 => ("Read", ToolKind::Read, format!("/home/ada/src/kiln/crates/kiln/src/part{}.rs", i % 97), Some(some_lines(r, 40, 160, "    ")), Vec::new()),
        3..=5 => ("Bash", ToolKind::Shell, "cargo test -p kiln harness::registry".to_owned(), Some(some_lines(r, 10, 120, "test harness::registry::case_")), Vec::new()),
        6 | 7 => {
            let path = format!("/home/ada/src/kiln/crates/kiln/src/part{}.rs", i % 97);
            let hunks = (0..1 + r.below(3))
                .map(|h| {
                    let n = 6 + r.below(24) as usize;
                    hunk(r, 40 + h as u32 * 50, n)
                })
                .collect();
            let d = FileDiff { path: path.clone(), created: false, hunks };
            ("Edit", ToolKind::Edit, path, None, vec![d])
        }
        8 => ("Grep", ToolKind::Search, "deadline".to_owned(), Some(some_lines(r, 5, 30, "crates/kiln/src/x.rs:")), Vec::new()),
        _ => ("mcp__kiln__run", ToolKind::Mcp, "cargo test --target aarch64-apple-darwin".to_owned(), Some(lines(r, 20, "ok ")), Vec::new()),
    };
    ToolCall {
        tool_id: format!("toolu_{i:08}"),
        name: name.to_owned(),
        tool: kind,
        title,
        detail: None,
        input: json!({"command": "cargo test -p kiln", "description": words(r, 6)}),
        status: ToolStatus::Done,
        output,
        truncated: false,
        diffs,
        todos: Vec::new(),
        task: None,
        exit_code: (kind == ToolKind::Shell).then_some(0),
        started: at,
        ended: Some(at + 1_200),
        server: None,
        tool_short: None,
        skill: None,
        runner: None,
    }
}

/// Event `i` of the session, about as a long Claude Code session's run: a fifth of them a tool
/// call, thoughts and notes between, now and then the person.
fn event(r: &mut Lcg, i: u64) -> Event {
    let at = 1_790_000_000_000 + i as i64 * 1_000;
    let body = match r.below(20) {
        0 | 1 => Body::User { text: some_words(r, 20, 200), images: 0, client_id: Some(format!("c{i}")), origin: None, attachments: Vec::new(), reply_to: None },
        2..=6 => Body::Assistant { text: some_words(r, 40, 400), streaming: false },
        7..=9 => Body::Thinking { text: some_words(r, 60, 500), streaming: false, redacted: false },
        10 => Body::Notice { level: Level::Info, text: words(r, 12) },
        _ => Body::Tool(Box::new(tool(r, i, at))),
    };
    Event { id: format!("ev-{i:06}"), seq: i, at, parent: None, by: None, body }
}

fn file(r: &mut Lcg, n: usize, at: i64) -> FileChange {
    FileChange {
        path: format!("crates/kiln/src/part{n}.rs"),
        tree: None,
        created: n.is_multiple_of(9),
        deleted: false,
        added: 10 + r.below(90) as u32,
        removed: r.below(40) as u32,
        hunks: (0..3).map(|h| hunk(r, 10 + h * 60, 20)).collect(),
        truncated: false,
        at,
    }
}

fn state(r: &mut Lcg, k: u64) -> State {
    let mut s = State {
        title: "Retry backoff rework".into(),
        goal: words(r, 60),
        summary: words(r, 80),
        cwd: "/home/ada/src/kiln".into(),
        model: "claude-opus-5-5".into(),
        mode: "auto".into(),
        doing: Some(format!("Running cargo test ({k})")),
        activity: Some(format!("Step {k}")),
        queue: serde_json::from_value(json!((0..5).map(|q| json!({"id": format!("q{q}"), "text": words(r, 200), "at": 1_790_000_000_000_i64 + q})).collect::<Vec<_>>())).unwrap(),
        tasks: (0..8)
            .map(|t| kiln::harness::model::Task {
                id: format!("t{t}"),
                description: words(r, 6),
                kind: "general-purpose".into(),
                status: "completed".into(),
                prompt: Some(words(r, 120)),
                summary: Some(words(r, 60)),
                class: Some("agent".into()),
                ..kiln::harness::model::Task::default()
            })
            .collect(),
        todos: serde_json::from_value(json!((0..20).map(|t| json!({"text": words(r, 10), "status": if t < 12 { "completed" } else { "pending" }})).collect::<Vec<_>>())).unwrap(),
        ..State::default()
    };
    s.usage.cost_usd = k as f64 * 0.01;
    s.usage.input_tokens = 812_000 + k;
    s
}

fn meta() -> kiln::harness::model::Meta {
    let commands: Vec<serde_json::Value> = (0..120).map(|c| json!({"name": format!("command-{c}"), "description": "What the command does, in a sentence or two, as its harness says it.", "hint": "[args]", "group": "Session", "route": {"kind": "prompt"}})).collect();
    serde_json::from_value(json!({"version": "2.1.310", "commands": commands, "features": ["interrupt", "set_mode", "set_model", "steer", "send_now"]})).unwrap()
}

/// A scratch server's hub, as the server's own tests make one (`hub::tests::scratch_hub_on`).
async fn scratch() -> anyhow::Result<Arc<Hub>> {
    let dir = tempfile::tempdir()?.keep();
    let mut cfg = kiln::config::Config::default();
    cfg.server.data_dir = dir.clone();
    cfg.terminal.enabled = false;
    let pki_dir = dir.join("pki");
    let store = kiln::rt::blocking(kiln::store::Store::in_memory).await??;
    let (pki, leaf) = kiln::rt::blocking(move || -> anyhow::Result<_> {
        let pki = kiln::pki::Pki::open(&pki_dir)?;
        let leaf = pki.server_leaf(&[])?;
        Ok((pki, leaf))
    })
    .await??;
    let pki = Arc::new(pki);
    let (net, _events) = kiln::net::start(kiln::net::Spec { bind: "127.0.0.1:0".parse()?, ca_der: pki.ca_der.clone(), leaf, server: true, dial_deadline: kiln::net::DIAL_DEADLINE, revoked_keys: Vec::new() })?;
    let logs = kiln::logs::Logs::new(dir.join("logs"), 1 << 20);
    Hub::new_on(kiln::rt::clock(), Arc::new(cfg), store, logs, net, pki, "bench-admin-token".into(), "bench-join-token".into(), kiln::rt::Shutdown::new())
}

/// One case's measurements.
struct Measured {
    median: Duration,
    counts: alloc::Counts,
    faults: u64,
}

/// Runs `f` `runs` times: the median time, and the last run's allocations and faults (warm, the
/// runs before it having grown the heap as a server that has run a while has).
fn measure(runs: usize, mut f: impl FnMut()) -> Measured {
    let mut times = Vec::with_capacity(runs);
    let mut counts = alloc::Counts::default();
    let mut faulted = 0;
    let last = runs.max(1) - 1;
    for run in 0..=last {
        let f0 = faults::read().ok();
        if run == last {
            alloc::begin_process();
        }
        let t0 = Instant::now();
        f();
        times.push(t0.elapsed());
        if run == last {
            counts = alloc::end_process();
            faulted = f0.zip(faults::read().ok()).map_or(0, |(a, b)| b.since(&a).total());
        }
    }
    times.sort();
    Measured { median: times.get(times.len() / 2).copied().unwrap_or_default(), counts, faults: faulted }
}

fn row(name: &str, per: u64, m: &Measured, note: &str) {
    let per = per.max(1) as f64;
    let c = &m.counts;
    println!(
        "{name:<30} {:>10.3} {:>12.1} {:>10.1} {:>12.0} {:>10.2} {:>10.1}  {note}",
        m.median.as_secs_f64() * 1e3 / per,
        c.allocations as f64 / per,
        c.reallocations as f64 / per,
        c.bytes as f64 / per,
        c.peak as f64 / 1e6,
        m.faults as f64 / per,
    );
}

fn main() -> anyhow::Result<()> {
    kiln::rt::configure(kiln::rt::Profile::server());
    let only: Option<String> = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let want = |name: &str| only.as_deref().is_none_or(|o| name.contains(o));
    let n_events: u64 = std::env::var("KILN_FEED_EVENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(4_000);
    let runs: usize = std::env::var("KILN_FEED_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let hub = kiln::rt::block_on(scratch())?;
    let there = Location::Machine("bench-box".into());
    let mut r = Lcg(0x5eed_f00d);

    // The long session, as its runner sends it: its Open, meta, state, changeset, then its events.
    let info = Info { id: ID.into(), kind: "claude".into(), kind_name: "Claude Code".into(), name: "Retry storm".into(), cwd: "/home/ada/src/kiln".into(), ..Info::default() };
    hub.harness.ingest(&hub, &there, ID, Update::Open { info: Box::new(info), since: None });
    hub.harness.ingest(&hub, &there, ID, Update::Meta { meta: Box::new(meta()) });
    hub.harness.ingest(&hub, &there, ID, Update::State { state: Box::new(state(&mut r, 0)) });
    let files: Vec<FileChange> = (0..200).map(|n| file(&mut r, n, 1_790_000_000_000)).collect();
    hub.harness.ingest(&hub, &there, ID, Update::Changes { files: files.clone() });
    let mut seq = 0u64;
    for chunk in 0..(n_events / 100) {
        let events: Vec<Event> = (0..100).map(|k| event(&mut r, chunk * 100 + k + 1)).collect();
        seq = chunk * 100 + 100;
        hub.harness.ingest(&hub, &there, ID, Update::Events { events });
    }

    println!("session_feed: {n_events} events, {runs} runs; per item: ms, allocations, reallocations, bytes allocated, peak MB (live), page faults");
    println!("{:<30} {:>10} {:>12} {:>10} {:>12} {:>10} {:>10}", "case", "ms", "allocs", "reallocs", "bytes", "peak MB", "faults");

    // A viewer's snapshot, as it attaches.
    let mut snapshot_bytes = 0usize;
    if want("snapshot") {
        let h = Arc::clone(&hub);
        let m = measure(runs, || {
            let a = h.harness.attach(ID).expect("the session");
            snapshot_bytes = a.frames().iter().map(String::len).sum();
            drop(a);
        });
        row("snapshot", 1, &m, &format!("{:.1} MB of frames", snapshot_bytes as f64 / 1e6));
        let h = Arc::clone(&hub);
        let mut packed = 0usize;
        let m = measure(runs, || {
            let a = h.harness.attach(ID).expect("the session");
            packed = a.snapshot.packed(r#"{"t":"synced"}"#).unwrap().len();
        });
        row("snapshot packed (gzip)", 1, &m, &format!("{:.1} MB packed", packed as f64 / 1e6));
    }

    // A viewer watching from here on, as a page watches (states and changesets as patches): the
    // updates below go out to it.
    let takes = kiln::harness::registry::Patches { state: true, changes: true };
    let viewer = hub.harness.attach_from(ID, None, takes).expect("the session");
    let topic = viewer.topic.clone();
    let mut at = viewer.at.unwrap_or_default();
    let rev_before = viewer.rev;

    if want("ingest events") {
        let mut evs: Vec<Event> = (0..1_000).map(|k| event(&mut r, seq + k + 1)).collect();
        seq += 1_000;
        let h = Arc::clone(&hub);
        let there = there.clone();
        let mut first = true;
        let m = measure(1, || {
            if first {
                first = false;
                for e in evs.drain(..) {
                    h.harness.ingest(&h, &there, ID, Update::Events { events: vec![e] });
                }
            }
        });
        row("ingest events", 1_000, &m, "one event an update");
    }

    if want("ingest stream") {
        seq += 1;
        let live = Event { id: format!("ev-{seq:06}"), seq, at: 1_790_100_000_000, parent: None, by: None, body: Body::Assistant { text: String::new(), streaming: true } };
        hub.harness.ingest(&hub, &there, ID, Update::Events { events: vec![live.clone()] });
        let h = Arc::clone(&hub);
        let there = there.clone();
        let mut first = true;
        let m = measure(1, || {
            if first {
                first = false;
                for k in 0..2_000 {
                    h.harness.ingest(&h, &there, ID, Update::Stream { id: live.id.clone(), text: format!("{} ", word(k)) });
                }
            }
        });
        row("ingest stream", 2_000, &m, "a word an update");
    }

    if want("ingest state") {
        let states: Vec<State> = (1..=500).map(|k| state(&mut r, k)).collect();
        let h = Arc::clone(&hub);
        let there = there.clone();
        let mut states = Some(states);
        let m = measure(1, || {
            for s in states.take().unwrap_or_default() {
                h.harness.ingest(&h, &there, ID, Update::State { state: Box::new(s) });
            }
        });
        row("ingest state", 500, &m, "activity, usage");
    }

    if want("ingest changes") {
        let patches: Vec<FileChange> = (0..200).map(|k| file(&mut r, (k * 7) % 200, 1_790_200_000_000 + k as i64)).collect();
        let h = Arc::clone(&hub);
        let there = there.clone();
        let mut patches = Some(patches);
        let m = measure(1, || {
            for f in patches.take().unwrap_or_default() {
                h.harness.ingest(&h, &there, ID, Update::ChangesPatch { set: vec![f], gone: Vec::<FileKey>::new() });
            }
        });
        row("ingest changes patch", 200, &m, "a file a patch, of 200 (~700 KB)");
    }

    if want("catch up") {
        let h = Arc::clone(&hub);
        let m = measure(runs, || {
            // Taken and written out, as a viewer's catch-up is (the server writes it on the blocking pool).
            let _ = h.harness.frames_since(ID, rev_before.saturating_add(1)).map(|(s, ..)| s.frames());
        });
        row("catch up", 1, &m, "past every update above");
    }

    if want("viewer reads") {
        use kiln_seams::Bus as _;
        let h = Arc::clone(&hub);
        let mut frames = 0u64;
        let mut bytes = 0usize;
        // As the server's viewer loop reads them (api.rs, `harness_attach`): each read where it is
        // in its payload, and taken as the payload's own bytes.
        let m = measure(1, || {
            loop {
                let batch = match futures::executor::block_on(h.bus.read(&topic, at, 16)) {
                    Ok(batch) => batch,
                    // Older than the feed keeps (the updates above are more than it does): on from its
                    // oldest, as a viewer that caught up reads on.
                    Err(kiln_seams::bus::Refusal::Behind { first, .. }) if first > at => {
                        at = first;
                        continue;
                    }
                    Err(_) => break,
                };
                if batch.messages.is_empty() {
                    break;
                }
                let next = batch.next;
                for msg in batch.messages {
                    let Some(range) = kiln::harness::live::decode_ref(&msg.payload).map(|f| f.range_for_viewer(takes)) else { continue };
                    let text = kiln::harness::live::into_text(msg.payload, range);
                    bytes += text.len();
                    frames += 1;
                }
                at = next;
            }
        });
        row("viewer reads", frames, &m, &format!("{frames} frames, {:.1} MB taken", bytes as f64 / 1e6));
    }
    drop(viewer);
    Ok(())
}
