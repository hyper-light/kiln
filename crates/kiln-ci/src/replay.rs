//! A recorded session's tool calls replayed offline against a split's variants (kiln plan
//! 01a1dd2ea7a777a28970a43fc5f766d0 P7.1b; docs/ci.md W20-R11, "Replays"): which of its calls a replay runs, and how;
//! the script that runs them in a variant's microVM; what came of each; and when its microVM's use is read.
//!
//! - **What replays** ([`plan`]): its shell commands, each a command line or its words, and the files it wrote whole,
//!   in their order. A replay runs no model, asks no person and reaches nothing past its microVM, so a call one of
//!   those made or answered isn't replayed, and neither is one its record doesn't hold whole; each is counted by why
//!   ([`Skip`]).
//! - **How** ([`script`]): one POSIX shell script, the replay's microVM's own command. Each step starts in the folder
//!   the one before ended in, the session's own first, as a harness's shell carries its folder from call to call; its
//!   input is the null device and its output goes nowhere. Each step's end is written to [`ENDED`] under [`AT`] as it
//!   ends, then [`DONE`]; the script then holds its run until kiln ends it, so its microVM's use is read whole as it
//!   is deleted (Hex reads no use of a run that ended on its own).
//! - **What came of it** ([`ends`], [`outcome`]): each step's end against its record's: a shell's exit code where the
//!   record says one, a write's success. A step that ended otherwise diverged.
//! - **When its use is read** ([`wait_ms`]): where the recording says the step running now would end, doubled for each
//!   read since that found no step ended (binary exponential backoff, so a replay slower than its recording is read
//!   O(log) times a step), and never sooner than a read itself took.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where a replay's microVM sees its scratch folder: its script and its steps' ends.
pub const AT: &str = "/kiln-replay";
/// The script's name in [`AT`].
pub const SCRIPT: &str = "replay.sh";
/// The file in [`AT`] each step's end is written to as it ends (`<step> <status>`, a line each), then [`DONE`].
pub const ENDED: &str = "ended";
/// The line [`ENDED`] ends with once every step has ended.
pub const DONE: &str = "done";

/// What a recorded call does, as its harness's adapter said it (kiln's `harness::model::ToolKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Shell,
    Edit,
    Read,
    Search,
    Web,
    Agent,
    Skill,
    Todo,
    Question,
    Plan,
    Mcp,
    Other,
}

/// How a recorded call ended in its session (kiln's `harness::model::ToolStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ran {
    Pending,
    Running,
    Done,
    Error,
    Denied,
    Cancelled,
}

/// A tool call as its session's record keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recorded {
    pub kind: Kind,
    /// Its harness's own name for its tool (`Bash`, `Write`).
    pub name: String,
    pub input: serde_json::Value,
    pub ran: Ran,
    /// A shell's exit code, where its record says one.
    pub exit_code: Option<i32>,
    /// From its start to its end as recorded, in milliseconds; none where it has no end.
    pub ms: Option<u64>,
    /// Its record keeps only part of a string of its input (kiln keeps so much of each).
    pub shortened: bool,
}

/// Why a recorded call isn't replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    /// It never ended in its session: denied, interrupted, or still waiting or running as recorded.
    NeverEnded,
    /// Its record keeps only part of its input: run, it would be another call.
    Shortened,
    /// It reached past its machine (the web): a replay's microVM reaches nothing.
    Network,
    /// A model (a subagent, a skill), a person (a question, a plan) or a server (an MCP tool) made or answered it, and
    /// none of them is there offline.
    NotOffline,
    /// Its harness did it in its own process for its model (a read, a search, its to-do list): nothing of it runs or
    /// changes in the folder.
    HarnessOwn,
    /// An edit in place of a file as its session had it: a replay's folder starts empty.
    EditInPlace,
    /// A task its session left running in the background: a replay runs its steps one after another.
    Background,
    /// Its input names nothing a replay can run (no command, no path and whole content), or holds a NUL, which no
    /// shell word carries.
    Unreadable,
}

/// A recorded call as a replay runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    /// A command line, run by the shell.
    Line { line: String },
    /// A command's words, run as they are.
    Words { words: Vec<String> },
    /// A whole file written: its path (from the folder the step starts in, where it isn't absolute) and its content.
    Write { path: String, content: String },
}

/// A step of a replay: the call it is (its index among the recorded calls), how it runs, the end its record says it
/// had (none where the record says none), and its time as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Planned {
    pub call: usize,
    pub step: Step,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<i32>,
    pub ms: u64,
}

/// What of a session's recorded calls a replay runs, and why the rest aren't.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub calls: usize,
    pub steps: Vec<Planned>,
    pub skipped: BTreeMap<Skip, usize>,
}

/// The first string of `v` at one of `keys`.
fn text<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(serde_json::Value::as_str))
}

/// How a recorded call replays, or why it doesn't.
fn step_of(c: &Recorded) -> Result<(Step, Option<i32>), Skip> {
    if !matches!(c.ran, Ran::Done | Ran::Error) {
        return Err(Skip::NeverEnded);
    }
    match c.kind {
        Kind::Shell | Kind::Edit if c.shortened => Err(Skip::Shortened),
        Kind::Shell => {
            if c.input.get("run_in_background").and_then(serde_json::Value::as_bool) == Some(true) {
                return Err(Skip::Background);
            }
            let step = match c.input.get("command") {
                Some(serde_json::Value::Array(words)) => {
                    let words: Option<Vec<String>> = words.iter().map(|w| w.as_str().map(str::to_owned)).collect();
                    Step::Words { words: words.filter(|w| !w.is_empty()).ok_or(Skip::Unreadable)? }
                }
                _ => Step::Line { line: text(&c.input, &["command", "cmd"]).filter(|l| !l.trim().is_empty()).ok_or(Skip::Unreadable)?.to_owned() },
            };
            // Its exit code where the record says one; else the record's own word: done is 0, an error isn't known.
            let expect = c.exit_code.or((c.ran == Ran::Done).then_some(0));
            Ok((step, expect))
        }
        Kind::Edit => {
            let path = text(&c.input, &["file_path", "path", "filePath"]);
            let content = text(&c.input, &["content"]);
            match (path, content) {
                (Some(path), Some(content)) if !path.is_empty() => Ok((Step::Write { path: path.to_owned(), content: content.to_owned() }, (c.ran == Ran::Done).then_some(0))),
                _ => Err(Skip::EditInPlace),
            }
        }
        Kind::Web => Err(Skip::Network),
        Kind::Agent | Kind::Skill | Kind::Question | Kind::Plan | Kind::Mcp => Err(Skip::NotOffline),
        Kind::Read | Kind::Search | Kind::Todo => Err(Skip::HarnessOwn),
        Kind::Other => Err(Skip::Unreadable),
    }
}

/// Whether `s` holds a NUL, which no shell word carries.
fn nul(s: &str) -> bool {
    s.contains('\0')
}

/// What of `calls`, a session's recorded tool calls in their order, a replay runs (module docs).
pub fn plan(calls: &[Recorded]) -> Plan {
    let mut p = Plan { calls: calls.len(), ..Plan::default() };
    for (call, c) in calls.iter().enumerate() {
        let made = step_of(c).and_then(|(step, expect)| {
            let held = match &step {
                Step::Line { line } => nul(line),
                Step::Words { words } => words.iter().any(|w| nul(w)),
                Step::Write { path, content } => nul(path) || nul(content),
            };
            if held { Err(Skip::Unreadable) } else { Ok((step, expect)) }
        });
        match made {
            Ok((step, expect)) => p.steps.push(Planned { call, step, expect, ms: c.ms.unwrap_or(0) }),
            Err(why) => *p.skipped.entry(why).or_insert(0) += 1,
        }
    }
    p
}

/// `s` as one shell word: single-quoted, each `'` closed, escaped and opened again (POSIX.1-2017 XCU 2.2.3).
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The script that runs `plan`'s steps from `folder`, the folder its session ran in (module docs).
pub fn script(plan: &Plan, folder: &str) -> String {
    let at = quote(AT);
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str("# A session's tool calls replayed by kiln (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.1b).\n");
    s.push_str(&format!("r={at}\nd={}\n", quote(folder)));
    s.push_str(&format!(": > \"$r/{ENDED}\"\n"));
    for (n, p) in plan.steps.iter().enumerate() {
        let body = match &p.step {
            Step::Line { line } => format!("eval {}", quote(line)),
            Step::Words { words } => words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" "),
            Step::Write { path, content } => {
                let path = quote(path);
                format!("mkdir -p -- \"$(dirname -- {path})\" && printf '%s' {} > {path}", quote(content))
            }
        };
        // Its folder carried to the next step as it ended, as a harness's shell carries it (an `exit` too).
        s.push_str(&format!("( trap 'pwd -P > \"$r/dir\"' EXIT; cd -- \"$d\" && {body} ) </dev/null >/dev/null 2>&1; echo \"{n} $?\" >> \"$r/{ENDED}\"\n"));
        s.push_str("n=$(cat -- \"$r/dir\" 2>/dev/null) && d=$n\n");
    }
    s.push_str(&format!("echo {DONE} >> \"$r/{ENDED}\"\n"));
    // Held until kiln ends its run: a FIFO opened both ways never ends its read and holds no CPU (a regular built-in's
    // redirection that fails fails the command alone, never the shell, as `exec`'s would: POSIX.1-2017 XCU 2.8.1).
    s.push_str("mkfifo /tmp/kiln-replay-hold 2>/dev/null && read -r _ <>/tmp/kiln-replay-hold\n");
    s.push_str("command -v tail >/dev/null 2>&1 && exec tail -f /dev/null\n");
    s
}

/// What [`ENDED`] says so far: each step's end in the order they ended, and whether every one has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ends {
    pub ended: Vec<(usize, i32)>,
    pub done: bool,
}

/// [`ENDED`]'s text as the script writes it. A line it can't read (one being written as it was read) ends what's read.
pub fn ends(text: &str) -> Ends {
    let mut e = Ends::default();
    for line in text.lines() {
        if line == DONE {
            e.done = true;
            break;
        }
        let Some((n, status)) = line.split_once(' ') else { break };
        let (Ok(n), Ok(status)) = (n.parse::<usize>(), status.parse::<i32>()) else { break };
        e.ended.push((n, status));
    }
    e
}

/// What came of a replay: how many of its steps ended, and how many of those ended otherwise than recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub steps: usize,
    pub ended: usize,
    pub diverged: usize,
}

/// `ends` against `plan`'s record of each step's end.
pub fn outcome(plan: &Plan, ends: &Ends) -> Outcome {
    let diverged = ends.ended.iter().filter(|(n, status)| plan.steps.get(*n).and_then(|p| p.expect).is_some_and(|want| want != *status)).count();
    Outcome { steps: plan.steps.len(), ended: ends.ended.len(), diverged }
}

/// How long to wait before reading a replay's use again (module docs): `ended` of `plan`'s steps have ended, `idle`
/// reads since one found a step end, and the last read took `read_ms`. None once every step has ended.
pub fn wait_ms(plan: &Plan, ended: usize, idle: u32, read_ms: u64) -> Option<u64> {
    let recorded = plan.steps.get(ended)?.ms;
    // A millisecond, the unit the recording keeps, at the least: a step recorded as taking none waits one.
    let base = recorded.max(read_ms).max(1);
    Some(base.saturating_mul(1u64.checked_shl(idle).unwrap_or(u64::MAX)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(kind: Kind, name: &str, input: serde_json::Value, ran: Ran, exit_code: Option<i32>) -> Recorded {
        Recorded { kind, name: name.into(), input, ran, exit_code, ms: Some(40), shortened: false }
    }

    /// P7.1b: a session's shell commands and whole-file writes replay in their order, each with the end its record
    /// says; every other call is counted by why it doesn't: a model's, a person's or a server's, the web, its harness's
    /// own reads, an edit in place, a background task, one that never ended, and one its record shortened.
    #[test]
    fn p7_1b_a_sessions_commands_and_writes_replay_and_the_rest_are_counted_by_why() {
        let calls = [
            call(Kind::Shell, "Bash", json!({"command": "cargo test", "description": "tests"}), Ran::Done, Some(0)),
            call(Kind::Edit, "Write", json!({"file_path": "src/a.rs", "content": "fn a() {}\n"}), Ran::Done, None),
            call(Kind::Shell, "shell", json!({"command": ["bash", "-lc", "make"]}), Ran::Error, Some(2)),
            call(Kind::Edit, "Edit", json!({"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}), Ran::Done, None),
            call(Kind::Read, "Read", json!({"file_path": "src/a.rs"}), Ran::Done, None),
            call(Kind::Search, "Grep", json!({"pattern": "fn"}), Ran::Done, None),
            call(Kind::Web, "WebFetch", json!({"url": "https://example.com"}), Ran::Done, None),
            call(Kind::Agent, "Task", json!({"prompt": "go"}), Ran::Done, None),
            call(Kind::Mcp, "mcp__kiln__run", json!({}), Ran::Done, None),
            call(Kind::Shell, "Bash", json!({"command": "npm run dev", "run_in_background": true}), Ran::Done, None),
            call(Kind::Shell, "Bash", json!({"command": "rm -rf /"}), Ran::Denied, None),
            Recorded { shortened: true, ..call(Kind::Shell, "Bash", json!({"command": "cat <<EOF\n… 9 bytes omitted …\nEOF"}), Ran::Done, Some(0)) },
            call(Kind::Shell, "Bash", json!({"description": "no command"}), Ran::Done, Some(0)),
            call(Kind::Shell, "Bash", json!({"command": "echo a\u{0}b"}), Ran::Done, Some(0)),
        ];
        let p = plan(&calls);
        assert_eq!(p.calls, 14);
        let steps: Vec<(usize, &Step, Option<i32>)> = p.steps.iter().map(|s| (s.call, &s.step, s.expect)).collect();
        assert_eq!(
            steps,
            [
                (0, &Step::Line { line: "cargo test".into() }, Some(0)),
                (1, &Step::Write { path: "src/a.rs".into(), content: "fn a() {}\n".into() }, Some(0)),
                (2, &Step::Words { words: vec!["bash".into(), "-lc".into(), "make".into()] }, Some(2)),
            ]
        );
        let skipped: Vec<(Skip, usize)> = p.skipped.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(
            skipped,
            [(Skip::NeverEnded, 1), (Skip::Shortened, 1), (Skip::Network, 1), (Skip::NotOffline, 2), (Skip::HarnessOwn, 2), (Skip::EditInPlace, 1), (Skip::Background, 1), (Skip::Unreadable, 2)]
        );
        assert_eq!(p.steps.iter().map(|s| s.ms).collect::<Vec<_>>(), [40, 40, 40]);
    }

    /// P7.1b: the script runs each step from the folder the one before ended in, the session's own first, its quotes
    /// carried whole, writes each step's end as it ends, then `done`, and holds its run; nothing in it reaches past its
    /// microVM.
    #[test]
    fn p7_1b_the_script_runs_each_step_in_order_and_says_each_end() {
        let p = plan(&[
            call(Kind::Shell, "Bash", json!({"command": "echo 'it''s' && cd sub"}), Ran::Done, Some(0)),
            call(Kind::Edit, "Write", json!({"file_path": "a b/c.txt", "content": "x'y\n"}), Ran::Done, None),
            call(Kind::Shell, "shell", json!({"command": ["ls", "-la"]}), Ran::Done, Some(0)),
        ]);
        let s = script(&p, "/work/it's");
        assert!(s.starts_with("#!/bin/sh\n"), "{s}");
        assert!(s.contains("d='/work/it'\\''s'\n"), "{s}");
        assert!(s.contains(r#"eval 'echo '\''it'\'''\''s'\'' && cd sub' ) </dev/null >/dev/null 2>&1; echo "0 $?" >> "$r/ended""#), "{s}");
        assert!(s.contains(r#"printf '%s' 'x'\''y"#), "{s}");
        assert!(s.contains(r#"'ls' '-la' ) </dev/null >/dev/null 2>&1; echo "2 $?""#), "{s}");
        let done = s.find("echo done").unwrap_or(0);
        assert!(done > s.find("echo \"2 $?\"").unwrap_or(usize::MAX), "done after the last step: {s}");
        assert!(s.contains("n=$(cat -- \"$r/dir\" 2>/dev/null) && d=$n\n"), "a step's folder carried only where it said one: {s}");
        assert!(s.find("read -r _ <>/tmp/kiln-replay-hold").is_some_and(|hold| hold > done), "it holds its run once done: {s}");
    }

    /// P7.1b: what `ended` says, read as the script writes it, a line being written ending what's read; and a step that
    /// ended otherwise than its record says diverged, where its record says an end.
    #[test]
    fn p7_1b_steps_ends_are_read_and_held_to_their_records() {
        let p = plan(&[
            call(Kind::Shell, "Bash", json!({"command": "true"}), Ran::Done, Some(0)),
            call(Kind::Shell, "Bash", json!({"command": "false"}), Ran::Error, Some(1)),
            call(Kind::Shell, "Bash", json!({"command": "cargo test"}), Ran::Error, None),
        ]);
        let e = ends("0 0\n1 0\n2 101\ndone\n");
        assert_eq!(e, Ends { ended: vec![(0, 0), (1, 0), (2, 101)], done: true });
        assert_eq!(outcome(&p, &e), Outcome { steps: 3, ended: 3, diverged: 1 });
        assert_eq!(ends("0 0\n1 "), Ends { ended: vec![(0, 0)], done: false });
        assert_eq!(ends(""), Ends::default());
    }

    /// P7.1b: a replay's use is read where the recording says the step running now ends, twice as long after each read
    /// that found none end, never sooner than a read took, and no more once every step has ended.
    #[test]
    fn p7_1b_reads_are_paced_by_the_recording() {
        let mut p = plan(&[call(Kind::Shell, "Bash", json!({"command": "a"}), Ran::Done, Some(0)), call(Kind::Shell, "Bash", json!({"command": "b"}), Ran::Done, Some(0))]);
        if let Some(s) = p.steps.get_mut(1) {
            s.ms = 0;
        }
        assert_eq!(wait_ms(&p, 0, 0, 3), Some(40));
        assert_eq!(wait_ms(&p, 0, 2, 3), Some(160));
        assert_eq!(wait_ms(&p, 0, 0, 90), Some(90), "never sooner than a read takes");
        assert_eq!(wait_ms(&p, 1, 0, 0), Some(1), "a step recorded as taking no time waits a millisecond");
        assert_eq!(wait_ms(&p, 0, 70, 3), Some(u64::MAX), "doubling saturates");
        assert_eq!(wait_ms(&p, 2, 0, 3), None);
    }
}
