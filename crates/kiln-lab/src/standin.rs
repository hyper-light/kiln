//! The lab's stand-in for Claude Code: what a runner runs as `claude` (the lab's own binary, copied
//! under that name), speaking stream-json as Claude Code 2.1.296 does, so every update goes the way
//! a real session's does: the runner's harness reads it, keeps it, and sends it to the server, which
//! keeps its copy and feeds its viewers. It calls no model.
//!
//! What it says is a long session's mix (as `crates/kiln/benches/session_feed.rs` builds one): a
//! fifth of its events tool calls with their output (reads, commands, edits with their changes,
//! searches), the rest thoughts and replies of a few hundred words. Each message it's sent says
//! what to do, by its words:
//! - `lab:prefill:<n>`: `n` events as fast as the runner takes them (a session that has run for
//!   days, before the lab measures);
//! - `lab:turn:<n>:<ms>`: `n` events, `ms` apart (an agent at work while people watch);
//! - `lab:hog:<threads>:<secs>`: `threads` threads busy on the CPU for `secs` seconds, an event a
//!   second meanwhile (an agent's build or test run beside the server: goal 8's headroom lab,
//!   docs/priority.md);
//! - `lab:subagent:<n>:<ms>`: a subagent's turn, as Claude Code's Task tool runs one: the Task's call,
//!   then `n` events of the subagent's own, `ms` apart, each its child (`parent_tool_use_id` the Task's
//!   id), then the Task's result, which updates the Task's tool event `n` events after it began
//!   (copies-lat2's A2(b) shape; Ada's own sessions run subagents everywhere);
//! - anything else: a short turn.
//!
//! Every event carries `@lab:<µs since the epoch>` from when it was written, so a viewer can tell how
//! long it took to reach it (the lab's machine has one clock).

use std::io::BufRead as _;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// The version it says it is (the stand-in kiln's tests use).
const VERSION: &str = "2.1.296";

/// The marker each event's stamp follows.
pub const STAMP: &str = "@lab:";

/// Microseconds since the epoch, now.
pub fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// A deterministic stream of numbers: the same session says the same things every round.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const WORDS: &[&str] = &[
    "the", "retry", "loop", "reads", "its", "deadline", "from", "same", "clock", "as", "backoff", "so", "a", "slow", "tick", "can", "no", "longer", "put", "before", "first",
    "attempt", "`crates/kiln/src/harness/registry.rs`", "viewer", "snapshot", "feed", "and", "then", "which", "is", "why", "tests", "pass", "**now**", "with", "every", "frame",
];

fn words(r: &mut Lcg, n: u64) -> String {
    let mut s = String::with_capacity(usize::try_from(n).unwrap_or(0) * 7);
    for i in 0..n {
        if i > 0 {
            s.push(if i % 17 == 0 { '\n' } else { ' ' });
        }
        let w = usize::try_from(r.below(WORDS.len() as u64)).unwrap_or(0);
        s.push_str(WORDS.get(w).copied().unwrap_or("the"));
    }
    s
}

fn lines(r: &mut Lcg, n: u64, prefix: &str) -> String {
    (0..n)
        .map(|k| {
            let w = 6 + r.below(8);
            format!("{prefix}{k:04} {}", words(r, w))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What one event of a turn writes: its stream-json lines.
fn event(r: &mut Lcg, session: &str, turn: u64, n: u64) -> Vec<Value> {
    let stamp = format!("{STAMP}{} ", now_us());
    let id = format!("{session}_{turn}_{n}");
    let assistant = |content: Value| {
        json!({
            "type": "assistant",
            "message": {"id": format!("msg_{id}"), "type": "message", "role": "assistant", "model": "claude-opus-5-5", "content": [content], "stop_reason": null, "usage": {"input_tokens": 1, "output_tokens": 1}},
            "parent_tool_use_id": null,
            "session_id": session,
        })
    };
    match r.below(20) {
        0..=6 => {
            let w = 40 + r.below(400);
            vec![assistant(json!({"type": "text", "text": format!("{stamp}{}", words(r, w))}))]
        }
        7..=9 => {
            let w = 60 + r.below(500);
            vec![assistant(json!({"type": "thinking", "thinking": format!("{stamp}{}", words(r, w)), "signature": ""}))]
        }
        10 => vec![assistant(json!({"type": "text", "text": format!("{stamp}{}", words(r, 12))}))],
        _ => {
            let tool = format!("toolu_{id}");
            let (name, input, output) = match r.below(10) {
                0..=2 => {
                    let n = 40 + r.below(160);
                    ("Read", json!({"file_path": format!("/work/crates/kiln/src/part{}.rs", r.below(97))}), lines(r, n, "    "))
                }
                3..=5 => {
                    let n = 10 + r.below(120);
                    ("Bash", json!({"command": "cargo test -p kiln harness::registry", "description": format!("{stamp}{}", words(r, 6))}), lines(r, n, "test harness::registry::case_"))
                }
                6 | 7 => {
                    let n = 6 + r.below(24);
                    let old = lines(r, n, "    let deadline = clock.now() + budget_");
                    let new = lines(r, n, "    let deadline = clock.now() + backoff_");
                    ("Edit", json!({"file_path": format!("/work/crates/kiln/src/part{}.rs", r.below(97)), "old_string": old, "new_string": new, "replace_all": false}), "The file has been updated.".to_owned())
                }
                8 => {
                    let n = 5 + r.below(30);
                    ("Grep", json!({"pattern": "deadline", "path": "/work"}), lines(r, n, "crates/kiln/src/x.rs:"))
                }
                _ => ("mcp__kiln__run", json!({"command": "cargo test --target aarch64-apple-darwin"}), lines(r, 20, "ok ")),
            };
            vec![
                assistant(json!({"type": "tool_use", "id": tool, "name": name, "input": input})),
                json!({
                    "type": "user",
                    "message": {"role": "user", "content": [{"tool_use_id": tool, "type": "tool_result", "content": format!("{stamp}{output}"), "is_error": false}]},
                    "parent_tool_use_id": null,
                    "session_id": session,
                }),
            ]
        }
    }
}

/// What a message's words ask of the turn: how many events, and how far apart.
fn plan(text: &str) -> (u64, Duration) {
    let mut it = text.split_whitespace().find_map(|w| w.strip_prefix("lab:")).unwrap_or("").split(':');
    match (it.next(), it.next(), it.next()) {
        (Some("prefill"), Some(n), _) => (n.parse().unwrap_or(0), Duration::ZERO),
        (Some("turn"), Some(n), Some(ms)) => (n.parse().unwrap_or(0), Duration::from_millis(ms.parse().unwrap_or(0))),
        // Its events come as it burns ([`hog_plan`]), or as its subagent works ([`subagent_plan`]).
        (Some("hog" | "subagent"), _, _) => (0, Duration::ZERO),
        _ => (5, Duration::from_millis(100)),
    }
}

/// A hog turn's words (`lab:hog:<threads>:<secs>`): how many threads, and for how long.
fn hog_plan(text: &str) -> Option<(u64, Duration)> {
    let mut it = text.split_whitespace().find_map(|w| w.strip_prefix("lab:hog:"))?.split(':');
    Some((it.next()?.parse().ok()?, Duration::from_secs(it.next()?.parse().ok()?)))
}

/// A subagent turn's words (`lab:subagent:<n>:<ms>`): how many of the subagent's events, and how far apart.
fn subagent_plan(text: &str) -> Option<(u64, Duration)> {
    let mut it = text.split_whitespace().find_map(|w| w.strip_prefix("lab:subagent:"))?.split(':');
    Some((it.next()?.parse().ok()?, Duration::from_millis(it.next()?.parse().ok()?)))
}

/// The `k`th event of a subagent turn of `n` events (`lab:subagent:<n>:<ms>`), as Claude Code's stream-json says a Task
/// tool's run: first the Task's call (`k` 0), then the subagent's own events (`k` 1 to `n`), each a long session's mix as
/// [`event`] writes it with the Task's id as its `parent_tool_use_id`, then the Task's result (`k` `n + 1`), which
/// updates the Task's tool event `n` events after it began. Past that, nothing.
fn subagent_event(r: &mut Lcg, session: &str, turn: u64, k: u64, n: u64) -> Vec<Value> {
    let stamp = format!("{STAMP}{} ", now_us());
    let task = format!("toolu_{session}_{turn}_task");
    if k == 0 {
        return vec![json!({
            "type": "assistant",
            "message": {"id": format!("msg_{session}_{turn}_task"), "type": "message", "role": "assistant", "model": "claude-opus-5-5", "content": [{"type": "tool_use", "id": task, "name": "Task", "input": {"description": format!("{stamp}{}", words(r, 5)), "prompt": words(r, 40), "subagent_type": "general-purpose"}}], "stop_reason": null, "usage": {"input_tokens": 1, "output_tokens": 1}},
            "parent_tool_use_id": null,
            "session_id": session,
        })];
    }
    if k <= n {
        let mut lines = event(r, session, turn, k.saturating_sub(1));
        for line in &mut lines {
            if let Some(o) = line.as_object_mut() {
                o.insert("parent_tool_use_id".to_owned(), Value::String(task.clone()));
            }
        }
        return lines;
    }
    if k == n.saturating_add(1) {
        let w = 60 + r.below(200);
        return vec![json!({
            "type": "user",
            "message": {"role": "user", "content": [{"tool_use_id": task, "type": "tool_result", "content": format!("{stamp}{}", words(r, w)), "is_error": false}]},
            "parent_tool_use_id": null,
            "session_id": session,
        })];
    }
    Vec::new()
}

/// `threads` threads busy on the CPU until `until`, as the lab's own hogs are (`crate::lab::hog`).
fn burn(threads: u64, until: Instant) -> Vec<std::thread::JoinHandle<()>> {
    (0..threads)
        .filter_map(|_| {
            std::thread::Builder::new()
                .spawn(move || {
                    let mut x = 0u64;
                    while Instant::now() < until {
                        for _ in 0..100_000 {
                            x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
                        }
                    }
                })
                .ok()
        })
        .collect()
}

/// The words of a user message as stream-json sends it (a string, or text blocks).
fn text_of(m: &Value) -> String {
    match m.pointer("/message/content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Writes `lines` to stdout, flushing when `flush`.
fn out(w: &mut impl std::io::Write, v: &Value, flush: bool) -> std::io::Result<()> {
    serde_json::to_writer(&mut *w, v)?;
    w.write_all(b"\n")?;
    if flush {
        w.flush()?;
    }
    Ok(())
}

/// The stand-in's own run: answers control requests at once, and takes each user message as a turn.
pub fn main(args: &[String]) -> anyhow::Result<()> {
    if args.iter().any(|a| a == "--version") {
        println!("{VERSION} (Claude Code)");
        return Ok(());
    }
    let session = args
        .windows(2)
        .find(|w| w.first().is_some_and(|f| f == "--session-id" || f == "--resume"))
        .and_then(|w| w.get(1).cloned())
        .unwrap_or_else(|| format!("lab-{}", now_us()));
    let short: String = session.chars().filter(char::is_ascii_alphanumeric).take(12).collect();
    // `KILN_LAB_SEED` (the browser lab's): the same seed and folder say the same things in any
    // run, whatever the session's id (goal 1 of the browser protocol program, step 3).
    let key = match std::env::var("KILN_LAB_SEED") {
        Ok(s) => format!("{s}/{}", std::env::current_dir().ok().and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default()),
        Err(_) => session.clone(),
    };
    let seed = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    let mut r = Lcg(seed);
    let (tx, rx) = mpsc::channel::<Value>();
    let stdout = std::io::stdout();
    // Control requests are answered as they come, a turn under way or not, from the reader: the
    // writer's lines and the reader's go out whole, one at a time.
    let writer = std::sync::Arc::new(std::sync::Mutex::new(std::io::BufWriter::with_capacity(1 << 20, stdout)));
    let answers = std::sync::Arc::clone(&writer);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(m) = serde_json::from_str::<Value>(&line) else { continue };
            match m.get("type").and_then(Value::as_str) {
                Some("control_request") => {
                    let initialize = m.pointer("/request/subtype").and_then(Value::as_str) == Some("initialize");
                    let response = if initialize { json!({"commands": [], "models": [], "agents": [], "current_permission_mode": "default", "claude_code_version": VERSION}) } else { json!({}) };
                    let a = json!({"type": "control_response", "response": {"subtype": "success", "request_id": m.get("request_id"), "response": response}});
                    if let Ok(mut w) = answers.lock() {
                        let _ = out(&mut *w, &a, true);
                    }
                }
                Some("user") => {
                    let gone = tx.send(m).is_err();
                    if gone {
                        break;
                    }
                }
                _ => {}
            }
        }
    });
    let mut turn = 0u64;
    // Its stdin closing (its runner gone) ends it.
    while let Ok(m) = rx.recv() {
        turn += 1;
        let (n, every) = plan(&text_of(&m));
        let command = m.get("uuid").and_then(Value::as_str).map(str::to_owned);
        let say = |v: Value, flush: bool| -> anyhow::Result<()> {
            let mut w = writer.lock().map_err(|_| anyhow::anyhow!("the writer was poisoned"))?;
            out(&mut *w, &v, flush)?;
            Ok(())
        };
        if let Some(c) = &command {
            say(json!({"type": "command_lifecycle", "command_uuid": c, "state": "started", "uuid": format!("{short}-{turn}-s"), "session_id": session}), false)?;
        }
        say(json!({"type": "system", "subtype": "init", "cwd": std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default(), "session_id": session, "tools": [], "mcp_servers": [], "model": "claude-opus-5-5", "permissionMode": "default", "claude_code_version": VERSION}), true)?;
        // A hog turn: its threads busy until the clock says it's over (time its group is stopped
        // counts), an event a second meanwhile.
        if let Some((threads, secs)) = hog_plan(&text_of(&m)) {
            let until = Instant::now() + secs;
            let burners = burn(threads, until);
            let mut k = 0;
            while Instant::now() < until {
                for l in event(&mut r, &short, turn, k) {
                    say(l, true)?;
                }
                k += 1;
                std::thread::sleep(Duration::from_secs(1).min(until.saturating_duration_since(Instant::now())));
            }
            for b in burners {
                let _ = b.join();
            }
        }
        // A subagent turn: the Task's call, its subagent's events, the Task's result, `every` apart.
        if let Some((n, every)) = subagent_plan(&text_of(&m)) {
            for k in 0..n.saturating_add(2) {
                for l in subagent_event(&mut r, &short, turn, k, n) {
                    say(l, true)?;
                }
                if k <= n {
                    std::thread::sleep(every);
                }
            }
        }
        for k in 0..n {
            let lines = event(&mut r, &short, turn, k);
            let paced = !every.is_zero();
            for l in lines {
                // A prefill goes as fast as the runner reads, flushed a batch at a time.
                say(l, paced || k % 64 == 63)?;
            }
            if paced {
                std::thread::sleep(every);
            }
        }
        say(json!({"type": "result", "subtype": "success", "is_error": false, "duration_ms": 1, "num_turns": 1, "result": "done", "session_id": session, "total_cost_usd": 0, "usage": {"input_tokens": 1, "output_tokens": 1}}), command.is_none())?;
        if let Some(c) = &command {
            say(json!({"type": "command_lifecycle", "command_uuid": c, "state": "completed", "uuid": format!("{short}-{turn}-c"), "session_id": session}), true)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 8 (plan 01a1a59c2b29 step 5): a hog turn is read from its words, and burns the CPU only
    /// until its time is up.
    #[test]
    fn g8_s5_a_hog_turn_burns_for_its_time_and_no_longer() {
        assert_eq!(hog_plan("lab:hog:4:30"), Some((4, Duration::from_secs(30))));
        assert_eq!(hog_plan("please lab:hog:2:1 now"), Some((2, Duration::from_secs(1))));
        assert_eq!(hog_plan("lab:turn:40:25"), None);
        assert_eq!(hog_plan("lab:hog:x:1"), None);
        assert_eq!(plan("lab:hog:4:30"), (0, Duration::ZERO));
        let t = Instant::now();
        for b in burn(2, t + Duration::from_millis(50)) {
            let _ = b.join();
        }
        assert!(t.elapsed() < Duration::from_secs(5), "burned {:?}", t.elapsed());
    }

    /// copies-lat2's A2(b) shape (main's ask, 2026-11-06): a subagent turn is read from its words, and its Task's tool
    /// event is updated, by the Task's result, more than 70 events after it began, every event between the subagent's
    /// own (its `parent_tool_use_id` the Task's id) and the result its parent's (none).
    #[test]
    fn a2b_a_subagent_turn_updates_its_task_more_than_70_events_after_it_began() {
        assert_eq!(subagent_plan("lab:subagent:100:25"), Some((100, Duration::from_millis(25))));
        assert_eq!(subagent_plan("go lab:subagent:3:0 now"), Some((3, Duration::ZERO)));
        assert_eq!(subagent_plan("lab:turn:40:25"), None);
        assert_eq!(plan("lab:subagent:100:25"), (0, Duration::ZERO), "the turn's events are the subagent's alone");
        let n = crate::lab::Profile::subagents().subagent_events;
        let mut r = Lcg(7);
        let lines: Vec<Value> = (0..n + 3).flat_map(|k| subagent_event(&mut r, "s1", 2, k, n)).collect();
        let call = lines.first().and_then(|l| l.pointer("/message/content/0"));
        assert_eq!(call.and_then(|c| c.get("name")).and_then(Value::as_str), Some("Task"), "{call:?}");
        let task = call.and_then(|c| c.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
        assert!(!task.is_empty(), "the Task's call has no id: {call:?}");
        let result = lines.iter().position(|l| l.pointer("/message/content/0/tool_use_id").and_then(Value::as_str) == Some(task.as_str()));
        assert_eq!(result, Some(lines.len() - 1), "the Task's result is the turn's last line");
        let between = lines.get(1..lines.len() - 1).unwrap_or_default();
        assert!(between.len() > 70 && between.len() as u64 >= n, "{} lines between the Task's call and its result", between.len());
        assert!(between.iter().all(|l| l.get("parent_tool_use_id").and_then(Value::as_str) == Some(task.as_str())), "a line between isn't the subagent's");
        assert_eq!(lines.last().and_then(|l| l.get("parent_tool_use_id")), Some(&Value::Null));
        assert_eq!(subagent_event(&mut r, "s1", 2, n + 2, n), Vec::<Value>::new(), "nothing past the result");
    }
}
