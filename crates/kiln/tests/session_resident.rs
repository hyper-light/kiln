//! Goal 11, plan 01a1a87b2e56 step 2: what keeping a long agent session costs the server. A session of
//! 20,000 events (production's longest, docs/runtime-lab.md "The lab"), sent as its runner sends it, a
//! hundred at a time, into a scratch server's copies (`crate::harness::copies`), which keep its newest
//! (`crate::harness::core::MAX_EVENTS`); then it ends. Its cost is what the process holds with it open
//! beyond what it held before: each batch is made as it's sent and let go of once the server took it, so
//! only what the server keeps is counted (its copy, and what its feed and its store hold of it); and what
//! of that its end lets go. The allocator counts every thread's (hyper-measure's `Counting`), so what the
//! owner shard and the pools hold is in it.
//!
//! The events are the lab's mix (`crates/kiln-lab/src/standin.rs`, `benches/session_feed.rs`): a fifth tool
//! calls with their output, the rest thoughts and replies of a few hundred words.

use std::sync::Arc;

use hyper_measure::alloc;
use kiln::harness::core::Update;
use kiln::harness::model::{Body, Event, ToolCall, ToolKind, ToolStatus};
use kiln::harness::registry::Location;
use kiln::harness::session::Info;
use kiln::hub::Hub;
use serde_json::json;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const ID: &str = "resident-long";

/// The events the session sends: production's longest conversation.
const EVENTS: u64 = 20_000;

/// The events' randomness, the same each run: a cell, so a draw can size another's arguments.
struct Lcg(std::cell::Cell<u64>);

impl Lcg {
    fn next(&self) -> u64 {
        let n = self.0.get().wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0.set(n);
        n >> 33
    }
    fn below(&self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const WORDS: &[&str] = &[
    "the", "retry", "loop", "reads", "its", "deadline", "from", "same", "clock", "as", "backoff", "so", "a", "slow", "tick", "can", "no", "longer", "put", "before", "first",
    "attempt", "`crates/kiln/src/harness/registry.rs`", "viewer", "snapshot", "feed", "and", "then", "which", "is", "why", "tests", "pass", "**now**", "with", "every", "frame",
];

fn words(r: &Lcg, n: u64) -> String {
    let mut s = String::new();
    for i in 0..n {
        if i > 0 {
            s.push(if i % 17 == 0 { '\n' } else { ' ' });
        }
        let w = usize::try_from(r.below(WORDS.len() as u64)).unwrap_or(0);
        s.push_str(WORDS.get(w).copied().unwrap_or("the"));
    }
    s
}

fn lines(r: &Lcg, n: u64, prefix: &str) -> String {
    (0..n).map(|k| format!("{prefix}{k:04} {}", words(r, 6 + r.below(8)))).collect::<Vec<_>>().join("\n")
}

fn tool(r: &Lcg, i: u64, at: i64) -> ToolCall {
    let (name, kind, title, output) = match r.below(10) {
        0..=2 => ("Read", ToolKind::Read, format!("/work/crates/kiln/src/part{}.rs", i % 97), lines(r, 40 + r.below(160), "    ")),
        3..=5 => ("Bash", ToolKind::Shell, "cargo test -p kiln harness::registry".to_owned(), lines(r, 10 + r.below(120), "test harness::registry::case_")),
        6 | 7 => ("Edit", ToolKind::Edit, format!("/work/crates/kiln/src/part{}.rs", i % 97), "The file has been updated.".to_owned()),
        8 => ("Grep", ToolKind::Search, "deadline".to_owned(), lines(r, 5 + r.below(30), "crates/kiln/src/x.rs:")),
        _ => ("mcp__kiln__run", ToolKind::Mcp, "cargo test --target aarch64-apple-darwin".to_owned(), lines(r, 20, "ok ")),
    };
    let input = match kind {
        ToolKind::Edit => json!({"file_path": title, "old_string": lines(r, 6 + r.below(24), "    let deadline = clock.now() + budget_"), "new_string": lines(r, 6 + r.below(24), "    let deadline = clock.now() + backoff_")}),
        _ => json!({"command": title, "description": words(r, 6)}),
    };
    ToolCall {
        tool_id: format!("toolu_{i:08}"),
        name: name.to_owned(),
        tool: kind,
        title,
        detail: None,
        input,
        status: ToolStatus::Done,
        output: Some(output),
        truncated: false,
        diffs: Vec::new(),
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

fn event(r: &Lcg, i: u64) -> Event {
    let at = 1_790_000_000_000 + i64::try_from(i).unwrap_or(0) * 1_000;
    let body = match r.below(20) {
        0..=6 => Body::Assistant { text: words(r, 40 + r.below(400)), streaming: false },
        7..=9 => Body::Thinking { text: words(r, 60 + r.below(500)), streaming: false, redacted: false },
        10 => Body::Assistant { text: words(r, 12), streaming: false },
        _ => Body::Tool(Box::new(tool(r, i, at))),
    };
    Event { id: format!("ev-{i:06}"), seq: i, at, parent: None, by: None, body }
}

/// A scratch server's hub, as the server's own tests make one (`benches/session_feed.rs`); with
/// `G11_RESIDENT_CONVERSATIONS=off`, without the conversation store (`[conversations] off`), so what the
/// store holds of the session shows apart from the rest.
async fn scratch() -> anyhow::Result<Arc<Hub>> {
    let dir = tempfile::tempdir()?.keep();
    let mut cfg = kiln::config::Config::default();
    cfg.server.data_dir = dir.clone();
    cfg.terminal.enabled = false;
    cfg.conversations.off = std::env::var("G11_RESIDENT_CONVERSATIONS").is_ok_and(|v| v == "off");
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
    Hub::new_on(kiln::rt::clock(), Arc::new(cfg), store, logs, net, pki, "resident-admin-token".into(), "resident-join-token".into(), kiln::rt::Shutdown::new())
}

/// Each of `updates` to the session's owner, as a runner's link hands them over, then back once it took them.
async fn ingest(hub: &Arc<Hub>, there: &Location, updates: Vec<Update>) {
    for u in updates {
        hub.harness.ingest_from(hub, there, ID, u, false).await;
    }
    hub.harness.applied(ID).await;
}

/// The bytes the process holds now, every thread's, after the shards and pools have taken what was sent.
async fn held(hub: &Arc<Hub>) -> i64 {
    hub.harness.applied(ID).await;
    // The blocking pool's and the store's writer's work in flight lands before the count.
    for _ in 0..20 {
        kiln::rt::sleep(std::time::Duration::from_millis(50)).await;
    }
    settled().await;
    alloc::read_process().live
}

/// Until the copy has done what it does after taking its events: its sealed chunks packed on the compute
/// pool, and kept compact once they are (goal 11, plan 01a1a87b2e56 step 2): what it keeps as text and
/// packed stops changing for two seconds (a debug build packs slowly), a couple of minutes at most.
async fn settled() {
    let now = || (kiln::memory::SESSION_ENTRIES.bytes(), kiln::harness::copies::SEGMENT_BYTES.load(std::sync::atomic::Ordering::Relaxed));
    let (mut last, mut still) = (now(), 0);
    for _ in 0..600 {
        kiln::rt::sleep(std::time::Duration::from_millis(200)).await;
        let seen = now();
        still = if seen == last { still + 1 } else { 0 };
        if still >= 10 {
            return;
        }
        last = seen;
    }
}

/// Goal 11, plan 01a1a87b2e56 step 2: a 20,000-event session's resident cost, said in bytes and per event kept.
#[test]
fn g11_s2_a_20000_event_sessions_resident_cost() -> anyhow::Result<()> {
    kiln::rt::configure(kiln::rt::Profile::server());
    kiln::rt::block_on(async {
        anyhow::ensure!(alloc::installed(), "the counting allocator isn't installed");
        let hub = scratch().await?;
        let there = Location::Machine("resident-box".into());
        let r = Lcg(std::cell::Cell::new(0x5eed_f00d));
        let info = Info { id: ID.into(), kind: "claude".into(), kind_name: "Claude Code".into(), name: "Retry storm".into(), cwd: "/work".into(), ..Info::default() };
        alloc::begin_process();
        let before = held(&hub).await;
        // Each batch made as it's sent and let go of once taken, as a runner's link hands them over: what's
        // counted is what the server keeps, never what was sent.
        ingest(&hub, &there, vec![Update::Open { info: Box::new(info), since: None }]).await;
        let mut text = 0usize;
        for batch in 0..EVENTS / 100 {
            let events: Vec<Event> = (1..=100).map(|k| event(&r, batch * 100 + k)).collect();
            text += events.iter().map(|e| serde_json::to_string(e).map_or(0, |s| s.len())).sum::<usize>();
            ingest(&hub, &there, vec![Update::Events { events }]).await;
        }
        // The events it kept, as a viewer's snapshot brings them (the view let go of at once).
        let kept: usize = hub.harness.attach(ID).map_or(0, |a| {
            a.snapshot.frames().iter().filter_map(|f| serde_json::from_str::<serde_json::Value>(f).ok()).filter_map(|v| v.get("events").and_then(serde_json::Value::as_array).map(Vec::len)).sum()
        });
        let with = held(&hub).await;
        let bus_with = hub.bus.stats().memory_used;
        // What the copy keeps as text and packed, and what waits for the store's writer (`kiln::memory`).
        let (entries, segments, queued) = (kiln::memory::SESSION_ENTRIES.bytes(), kiln::harness::copies::SEGMENT_BYTES.load(std::sync::atomic::Ordering::Relaxed), kiln::memory::CONVO_QUEUE.bytes());
        ingest(&hub, &there, vec![Update::Closed]).await;
        let after = held(&hub).await;
        let bus_after = hub.bus.stats().memory_used;
        let open = with - before;
        let per = open / i64::try_from(kept.max(1)).unwrap_or(1);
        println!(
            "g11 resident (conversation store {}): {EVENTS} events sent ({:.1} MB of JSON), {kept} kept; while open the server held {:.2} MB more \
             ({per} bytes an event kept), {:.2} MB of it let go at its end (process {:.2} MB before, {:.2} MB with it, {:.2} MB after); \
             the bus's own count {:.2} MB with it, {:.2} MB after; with it, kept as text {:.2} MB, packed {:.2} MB, waiting for the store {:.2} MB",
            if hub.harness.convo.is_some() { "on" } else { "off" },
            text as f64 / 1e6,
            open as f64 / 1e6,
            (with - after) as f64 / 1e6,
            before as f64 / 1e6,
            with as f64 / 1e6,
            after as f64 / 1e6,
            bus_with as f64 / 1e6,
            bus_after as f64 / 1e6,
            entries as f64 / 1e6,
            segments as f64 / 1e6,
            queued as f64 / 1e6
        );
        anyhow::ensure!(kept > 0, "the session kept no events");
        Ok(())
    })
}
