//! A viewer's bounded first load of an agent session (docs/wire.md, "Windows"): the newest of its
//! events up to a bound, whole turns where they fit, and older pages before it as the person
//! scrolls back to them; and the lean form of a tool call, its bodies (output, input, diff hunks)
//! held back until a person opens it. Pure: what it decides comes from the events alone.
//!
//! The bounds are T3 Code's (`docs/internals/performance-regressions.md` in T3 Code: "Cold opens
//! and history pages stop at about 1 MiB of encoded rows. A window that holds a user turn also
//! stops at 10 user turns (20 on older pages) or 500 rows, so ordinary turns stay whole."). They
//! carry over: T3 Code's timeline is the same thing kiln's is (a coding agent's messages, thoughts
//! and tool calls, read in a browser that parses each frame's JSON whole), and kiln's own sessions
//! hold about ten tool calls a person's turn (38,570 calls over 3,910 turns in twelve of this
//! repository's longest Claude Code transcripts), so ten turns are about a hundred rows and stay
//! well inside 500. `window_bounds_measured` measures what they come to for a long session.

use serde_json::Value;

/// A window (and an older page) stops past about this many bytes of its events as sent (T3 Code's
/// 1 MiB, above). Not a hard cap: a window holds at least one event, however large.
pub const BYTES: usize = 1 << 20;
/// A first load holds at most this many of a person's turns (T3 Code's 10).
pub const TURNS: usize = 10;
/// An older page holds at most this many (T3 Code's 20 on older pages).
pub const PAGE_TURNS: usize = 20;
/// A window holds at most this many events (T3 Code's 500 rows).
pub const ROWS: usize = 500;

/// Whether `e` begins a person's turn: a message to the main thread (not a subagent's).
pub fn begins_turn(e: &Value) -> bool {
    e.get("kind").and_then(Value::as_str) == Some("user") && e.get("parent").is_none_or(Value::is_null)
}

/// Where a window of `events[..end]` begins: from `end` back, as many events as fit [`BYTES`] and
/// [`ROWS`], and at most `turns` of a person's turns. When a bound stops it inside a turn, it
/// begins where the oldest whole turn it holds does, so that turn's prompt and work come together;
/// only a turn longer than the bounds alone is cut, at the bounds. `size` says what an event takes
/// as sent, `turn` whether it begins a turn ([`begins_turn`]). At least one event when there's one.
pub fn start<T>(events: &[T], end: usize, turns: usize, size: impl Fn(&T) -> usize, turn: impl Fn(&T) -> bool) -> usize {
    start_at(end.min(events.len()), turns, |i| events.get(i).map(|e| (size(e), turn(e))))
}

/// [`start`] of events read by their place (`event(i)`: what event `i` takes as sent, and whether it
/// begins a turn), for events not held in one slice (a copy's chunks).
pub fn start_at(end: usize, turns: usize, event: impl Fn(usize) -> Option<(usize, bool)>) -> usize {
    let (mut bytes, mut rows, mut held) = (0usize, 0usize, 0usize);
    // The oldest turn's start it holds whole, so far.
    let mut whole: Option<usize> = None;
    let mut at = end;
    while let Some(i) = at.checked_sub(1) {
        let Some((n, begins)) = event(i) else { break };
        if rows > 0 && (bytes.saturating_add(n) > BYTES || rows >= ROWS) {
            return whole.unwrap_or(at);
        }
        bytes = bytes.saturating_add(n);
        rows += 1;
        at = i;
        if begins {
            held += 1;
            whole = Some(i);
            if held >= turns {
                return i;
            }
        }
    }
    0
}

/// The tool kinds whose bodies the timeline shows only once a person opens a call: a command's
/// output, what a read or a search returned, a fetch's page, an edit's hunks (its head says the
/// files and the lines added and taken out). The rest go whole: kiln reads what their answers say
/// (a page shown, a live page, research, a question's answer, a subagent's result, a plan).
const LEAN_KINDS: [&str; 5] = ["shell", "read", "search", "web", "edit"];

/// The lean form of event `e` for a viewer that takes it so (`?lean=1`): a settled tool call of
/// [`LEAN_KINDS`] without its output, its input (`null`), or its diffs' hunks (each diff keeps its
/// path and says its lines added and taken out, `add` and `del`), and `held` saying how many bytes
/// of each were held back. The whole event comes on demand
/// (`GET /api/harness/sessions/:id/events/:event`). None: it goes as it is: not a tool call of
/// those kinds, nothing to hold back, or one the timeline shows open as it comes (running,
/// waiting, failed: those come whole, as the person sees them at once).
pub fn lean(e: &Value) -> Option<Value> {
    if e.get("kind").and_then(Value::as_str) != Some("tool") {
        return None;
    }
    let tool = e.get("tool").and_then(Value::as_str)?;
    if !LEAN_KINDS.contains(&tool) {
        return None;
    }
    match e.get("status").and_then(Value::as_str) {
        Some("running" | "pending" | "error") | None => return None,
        Some(_) => {}
    }
    if e.get("exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0) {
        return None;
    }
    let output = e.get("output").and_then(Value::as_str).map_or(0, str::len);
    let input = match e.get("input") {
        None | Some(Value::Null) => 0,
        Some(Value::Object(o)) if o.is_empty() => 0,
        Some(v) => serde_json::to_string(v).map_or(0, |s| s.len()),
    };
    let diffs = e.get("diffs").and_then(Value::as_array);
    let lines: usize = diffs.into_iter().flatten().map(|d| hunk_lines(d).count()).sum();
    if output == 0 && input == 0 && lines == 0 {
        return None;
    }
    let mut v = e.clone();
    let o = v.as_object_mut()?;
    o.remove("output");
    o.insert("input".into(), Value::Null);
    if let Some(Value::Array(diffs)) = o.get_mut("diffs") {
        for d in diffs.iter_mut() {
            let (add, del) = hunk_lines(d).fold((0u64, 0u64), |(a, r), l| match l.as_bytes().first() {
                Some(b'+') => (a + 1, r),
                Some(b'-') => (a, r + 1),
                _ => (a, r),
            });
            if let Some(d) = d.as_object_mut() {
                d.insert("hunks".into(), Value::Array(Vec::new()));
                d.insert("add".into(), add.into());
                d.insert("del".into(), del.into());
            }
        }
    }
    o.insert("held".into(), serde_json::json!({ "output": output, "input": input, "lines": lines }));
    Some(v)
}

/// A diff's hunks' lines.
fn hunk_lines(d: &Value) -> impl Iterator<Item = &str> {
    d.get("hunks").and_then(Value::as_array).into_iter().flatten().filter_map(|h| h.get("lines").and_then(Value::as_array)).flatten().filter_map(Value::as_str)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn user(seq: u64) -> Value {
        json!({"id": format!("u{seq}"), "seq": seq, "at": 0, "kind": "user", "text": "do it"})
    }

    fn note(seq: u64) -> Value {
        json!({"id": format!("a{seq}"), "seq": seq, "at": 0, "kind": "assistant", "text": "ok"})
    }

    fn shell(seq: u64, status: &str, exit: Option<i64>, output: &str) -> Value {
        let mut v = json!({"id": format!("t{seq}"), "seq": seq, "at": 0, "kind": "tool", "tool_id": "x", "name": "Bash", "tool": "shell", "title": "ls", "input": {"command": "ls"}, "status": status, "output": output, "started": 0});
        if let (Some(c), Some(o)) = (exit, v.as_object_mut()) {
            o.insert("exit_code".into(), c.into());
        }
        v
    }

    /// Turns of `per` events each (a prompt, then notes), `n` of them.
    fn turns(n: u64, per: u64) -> Vec<Value> {
        let mut out = Vec::new();
        for t in 0..n {
            for k in 0..per {
                let seq = t * per + k + 1;
                out.push(if k == 0 { user(seq) } else { note(seq) });
            }
        }
        out
    }

    fn at(events: &[Value], end: usize, turns: usize, size: usize) -> usize {
        start(events, end, turns, |_| size, begins_turn)
    }

    #[test]
    fn a_window_holds_the_last_ten_turns_whole() {
        let ev = turns(30, 5);
        let s = at(&ev, ev.len(), TURNS, 100);
        assert_eq!(s, ev.len() - 10 * 5);
        assert!(ev.get(s).is_some_and(begins_turn));
        // An older page: twenty before it.
        let p = at(&ev, s, PAGE_TURNS, 100);
        assert_eq!(s - p, 20 * 5);
    }

    #[test]
    fn past_its_bytes_it_begins_at_the_oldest_whole_turn() {
        // Each event 100 KiB: ten fit 1 MiB, so two turns of four whole, and the third cut off.
        let ev = turns(6, 4);
        let s = at(&ev, ev.len(), TURNS, 100 << 10);
        assert_eq!(s, ev.len() - 8);
        assert!(ev.get(s).is_some_and(begins_turn));
    }

    #[test]
    fn a_turn_longer_than_the_bounds_is_cut_at_them() {
        let ev = turns(1, ROWS as u64 + 50);
        let s = at(&ev, ev.len(), TURNS, 10);
        assert_eq!(ev.len() - s, ROWS);
        // One event larger than the bytes alone still goes.
        let big = turns(1, 3);
        assert_eq!(at(&big, big.len(), TURNS, BYTES * 2), big.len() - 1);
    }

    #[test]
    fn a_short_session_goes_whole() {
        let ev = turns(3, 3);
        assert_eq!(at(&ev, ev.len(), TURNS, 10), 0);
        assert_eq!(at(&[] as &[Value], 0, TURNS, 10), 0);
    }

    #[test]
    fn a_subagents_prompt_begins_no_turn() {
        let sub = json!({"id": "s", "seq": 1, "at": 0, "kind": "user", "text": "go", "parent": "t9"});
        assert!(!begins_turn(&sub));
        assert!(begins_turn(&user(1)));
        assert!(!begins_turn(&note(1)));
    }

    #[test]
    fn a_settled_command_goes_without_its_output_and_says_its_sizes() -> anyhow::Result<()> {
        let e = shell(3, "done", Some(0), "hello\nworld\n");
        let l = lean(&e).ok_or_else(|| anyhow::anyhow!("not lean"))?;
        assert!(l.get("output").is_none());
        assert_eq!(l.get("input"), Some(&Value::Null));
        assert_eq!(l.pointer("/held/output"), Some(&json!(12)));
        assert_eq!(l.pointer("/held/input"), Some(&json!(16)));
        // What its head shows stays.
        for k in ["id", "seq", "title", "status", "exit_code", "started", "tool", "name"] {
            assert_eq!(l.get(k), e.get(k), "{k}");
        }
        Ok(())
    }

    #[test]
    fn what_shows_open_goes_whole() {
        assert!(lean(&shell(1, "running", None, "half")).is_none());
        assert!(lean(&shell(1, "error", None, "boom")).is_none());
        assert!(lean(&shell(1, "done", Some(2), "failed")).is_none());
        assert!(lean(&note(1)).is_none());
        let ask = json!({"id": "q", "seq": 1, "at": 0, "kind": "tool", "tool": "question", "status": "done", "output": "yes", "input": {}});
        assert!(lean(&ask).is_none());
        // Nothing to hold back.
        let bare = json!({"id": "r", "seq": 1, "at": 0, "kind": "tool", "tool": "read", "status": "done", "input": {}});
        assert!(lean(&bare).is_none());
    }

    #[test]
    fn an_edit_keeps_its_files_and_counts_but_not_its_hunks() -> anyhow::Result<()> {
        let e = json!({"id": "e", "seq": 1, "at": 0, "kind": "tool", "tool": "edit", "status": "done", "input": {"old_string": "a", "new_string": "b"},
            "diffs": [{"path": "src/a.rs", "hunks": [{"old_start": 1, "old_lines": 2, "new_start": 1, "new_lines": 3, "lines": [" x", "-a", "+b", "+c"]}]}, {"path": "b.rs", "created": true, "hunks": [{"old_start": 0, "old_lines": 0, "new_start": 1, "new_lines": 1, "lines": ["+n"]}]}]});
        let l = lean(&e).ok_or_else(|| anyhow::anyhow!("not lean"))?;
        assert_eq!(l.pointer("/diffs/0/path"), Some(&json!("src/a.rs")));
        assert_eq!(l.pointer("/diffs/0/hunks"), Some(&json!([])));
        assert_eq!((l.pointer("/diffs/0/add"), l.pointer("/diffs/0/del")), (Some(&json!(2)), Some(&json!(1))));
        assert_eq!(l.pointer("/diffs/1/created"), Some(&json!(true)));
        assert_eq!(l.pointer("/diffs/1/add"), Some(&json!(1)));
        assert_eq!(l.pointer("/held/lines"), Some(&json!(5)));
        Ok(())
    }
}
