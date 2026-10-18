//! `kiln reach mcp`: kiln's MCP server for the agents kiln starts on a runner (`docs/reach.md` §7,
//! docs/access.md), over stdio, talking to the runner's reach socket. Every harness that takes MCP
//! servers gets it as `kiln`: Claude Code through `--mcp-config`, Codex through `-c mcp_servers.*`,
//! ACP agents through their session's `mcpServers`.
//!
//! - `code_run`: a JavaScript program against kiln, in the bounded QuickJS sandbox
//!   ([`crate::mcp::code`]) here on the runner. `kiln.machines.*` run commands on the kiln's
//!   machines (below); kiln's operations (`kiln.ci.*`, `kiln.cd.*`) are asked of the kiln server
//!   through the runner, for this session, which acts for whoever started it
//!   ([`crate::mcp::runner_ask`]). Calls run side by side, and only what the program returns comes
//!   back. `code_search` runs a program over the operations.
//! - `runners`: which machines there are, best first for what the work needs;
//! - `run`: a command on one of them (by name, or by what it must be: `linux/amd64`), this
//!   repository synced over first; returns its output and exit code, or runs on in the background;
//! - `output` / `stop`: a background run's new output, and stopping it.
//!
//! The session's preferred runner (`KILN_REACH_ON`) is the default for `on`; the session itself
//! (`KILN_SESSION`) is whom kiln's operations are asked for.

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::local::{Client, Ev, Req, RunReq};
use super::wire::Signal;
use crate::mcp::code;

/// Output a tool answer carries at most (the middle goes, as Claude Code's own Bash tool does).
const OUTPUT_MAX: usize = 30_000;
/// Output a program's call gets at most: the program filters it before anything reaches the agent.
const CODE_OUTPUT_MAX: usize = 1_000_000;
/// A foreground run returns after this long by default (it then keeps running in the background).
const WAIT_DEFAULT_S: u64 = 600;
/// A program runs this long by default, and at most `CODE_WALL_MAX`.
const CODE_WALL_DEFAULT: Duration = Duration::from_secs(600);
const CODE_WALL_MAX: Duration = Duration::from_secs(3600);
/// kiln's operations, as the server lists them, are asked for again after this.
const REGISTRY_TTL: Duration = Duration::from_secs(60);

fn instructions(prefer: Option<&str>, machines: &str) -> String {
    let mut s = "This is kiln's own MCP server: the machines of this kiln (its runners), and kiln itself (CI, deployments). \
When a step has to happen on another machine (the person's Mac, a server, the box with the GPU), run it there yourself: \
don't hand the person a command to type. When a step needs a person at that machine (a password, a system dialog to \
allow, something physical), start it yourself and tell them exactly what will appear and what to do. Use another machine \
too when work needs another platform or more power than this one: build and test for Linux x86-64 from a Mac (on: \
\"linux/amd64\"), use the machine with the GPU (on: \"+gpu\"). `run` runs one command on one machine. `code_run` runs a \
JavaScript program against kiln, and only what it returns comes back to you: the same build or test on several machines at \
once, returning just what failed (`const [mac, linux] = await Promise.all([kiln.machines.run({on: \"macos\", command: \
\"cargo test\"}), kiln.machines.run({on: \"linux/amd64\", command: \"cargo test\"})])`), or CI runs and deployments \
(`kiln.ci.*`, `kiln.cd.*`; `code_search` lists every operation and its input). The repository you are in is synced to \
the other machine first (only what changed; git sees the same state there), and the command runs in the matching \
folder, so edit here and build or test there; for a command that isn't about this repository, pass sync: false (it runs \
in that machine's home folder). `runners` lists the machines, best first. Long commands can run in the background \
(background: true), then `output` and `stop`."
        .to_owned();
    if !machines.is_empty() {
        s.push_str(&format!(" This kiln's machines: {machines}."));
    }
    if let Some(p) = prefer {
        s.push_str(&format!(" This session's person prefers commands to run on {p}: `run` goes there unless you say otherwise."));
    }
    s
}

/// This kiln's machines, as the instructions name them ("adacachy (linux/x86_64, this machine),
/// …"): asked of the runner's reach socket, briefly (empty when it doesn't answer in time).
fn machines(sock: &std::path::Path) -> String {
    let sock = sock.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ask = || -> anyhow::Result<Value> {
            let mut c = Client::connect(&sock)?;
            c.send(&Req::Runners { want: None })?;
            match c.event()? {
                Some(Ev::Runners(j)) => Ok(serde_json::from_str(&j)?),
                other => anyhow::bail!("unexpected answer {other:?}"),
            }
        };
        let _ = tx.send(ask());
    });
    let Ok(Ok(v)) = rx.recv_timeout(Duration::from_secs(2)) else { return String::new() };
    describe(&v)
}

fn describe(v: &Value) -> String {
    let me = v.get("me").and_then(Value::as_str).unwrap_or("");
    let entries = v.get("runners").and_then(Value::as_array).into_iter().flatten().chain(v.get("passed").and_then(Value::as_array).into_iter().flatten());
    let mut out: Vec<String> = Vec::new();
    for e in entries {
        let r = e.get("runner").unwrap_or(e);
        let Some(name) = r.get("name").and_then(Value::as_str) else { continue };
        if out.iter().any(|o| o.starts_with(&format!("{name} ("))) {
            continue;
        }
        let platform = format!("{}/{}", r.get("os").and_then(Value::as_str).unwrap_or("?"), r.get("arch").and_then(Value::as_str).unwrap_or("?"));
        let here = if name == me { ", this machine" } else { "" };
        out.push(format!("{name} ({platform}{here})"));
    }
    out.join(", ")
}

const WANT: &str = "What the machine must be (optional): a runner's name, or `linux/amd64`, `macos`, `arm64`, `linux,+gpu`, `cpus>=16`, `mem>=32g`.";

/// A command's input, as `run` and `kiln.machines.run` take it (`run` adds how long to wait).
fn command_schema(waits: bool) -> Value {
    let mut props = json!({
        "command": {"type": "string", "description": "Shell text, run by bash in the synced copy of this repository."},
        "on": {"type": "string", "description": "Where: a runner's name or what it must be (`linux/amd64`). Default: the session's preferred runner, else the best of all."},
        "sync": {"type": "boolean", "description": "Copy this repository over first and run in it (default: true inside a git repository)."},
        "cwd": {"type": "string", "description": "With sync: the folder here whose repository is synced and whose counterpart the command runs in (default: the session's folder). Without sync: the folder on the other machine (default: its home)."},
        "timeout_s": {"type": "number", "description": "Stop the command after this many seconds."},
        "shell": {"type": "string", "description": "A named shell over there: its folder and exported variables carry over between runs that name it."},
        "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Environment for the command."}
    });
    if waits && let Some(p) = props.as_object_mut() {
        p.insert("wait_s".into(), json!({"type": "number", "description": "Return after this many seconds even if it is still running (it keeps running in the background; default 600)."}));
        p.insert("background".into(), json!({"type": "boolean", "description": "Return at once with an id; read with `output`, end with `stop`."}));
    }
    json!({"type": "object", "properties": props, "required": ["command"]})
}

fn tools() -> Value {
    json!([
        {
            "name": "code_run",
            "description": "Run a JavaScript program against kiln and get back only what it returns. `await kiln.machines.run({on, command})` runs a shell command on one of this kiln's machines (this repository synced over first, as with `run`) and resolves when it ends, with {runner, exit_code, output}; start several and `await Promise.all([...])` to run them side by side. Also `kiln.machines.list({want})`, `kiln.machines.start(...)` (background: {id}), `kiln.machines.output({id, wait_s})`, `kiln.machines.stop({id})`, and kiln's CI and deployments under `kiln.ci.*` and `kiln.cd.*` (`code_search` lists every operation and its input; `kiln.call(name, input)` calls one by name). A refused call throws an Error with `result`. `input` is given as `input`. Bounded: 10 minutes unless timeout_s says otherwise (at most an hour), 16 calls at once, 4 MiB per answer; runs a program ends without are stopped.",
            "inputSchema": {"type": "object", "properties": {
                "program": {"type": "string", "description": "The body of an async function; `return` its answer."},
                "input": {"description": "Any JSON, available as `input`."},
                "timeout_s": {"type": "number", "description": "How long the program may run (default 600, at most 3600)."}
            }, "required": ["program"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": true},
        },
        {
            "name": "code_search",
            "description": "Run a JavaScript program over kiln's operations and return what it returns. The program sees `registry` (every operation: {name, call, summary, read_only, destructive, input}) and `skills` ({name, description}). Example: `return registry.filter(o => o.name.startsWith(\"machines.\") || o.name.startsWith(\"ci.runs\")).map(o => ({call: o.call, input: o.input}))`.",
            "inputSchema": {"type": "object", "properties": {"program": {"type": "string", "description": "The body of an async function; `return` its answer."}}, "required": ["program"]},
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "runners",
            "description": "The machines (kiln runners) commands can run on, best first for `want`: platform, idle cores, free memory, installed agents, and why others don't suit. `want` is a runner's name or what it must be: `linux/amd64`, `macos`, `arm64`, `linux,+gpu`, `cpus>=16`, `mem>=32g`.",
            "inputSchema": {"type": "object", "properties": {"want": {"type": "string", "description": "What the machine must be (optional)."}}},
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "run",
            "description": "Run a shell command (bash) on another machine. `on` picks it: a runner's name, or what it must be (`linux/amd64`, `macos/arm64`, `+gpu`, `cpus>=16`); the idlest that suits is used. By default this repository is synced there first (git-tracked and untracked-but-not-ignored files plus .git; build output like target/ stays home) and the command runs in the matching folder. Returns the exit code and output (stdout and stderr interleaved, the middle cut past 30k characters). Set background to return at once with an id for `output` and `stop`.",
            "inputSchema": command_schema(true),
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": true},
        },
        {
            "name": "output",
            "description": "A background run's output since the last read, and whether it has ended (with its exit code). wait_s waits up to that long for it to end.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}, "wait_s": {"type": "number"}}, "required": ["id"]},
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "stop",
            "description": "Stop a background run (Ctrl-C, then terminate if it doesn't stop).",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": false},
        },
    ])
}

/// `kiln.machines.*`, as `code_search` lists them.
fn machine_ops() -> Vec<Value> {
    let op = |name: &str, summary: &str, read_only: bool, destructive: bool, input: Value| {
        json!({"name": name, "call": format!("kiln.{name}"), "summary": summary, "read_only": read_only, "destructive": destructive, "input": input})
    };
    vec![
        op(
            "machines.list",
            "The machines (kiln runners) commands can run on, best first for `want`: {me, runners: [{runner: {name, os, arch, ...}, why}], passed: [...]}.",
            true,
            false,
            json!({"type": "object", "properties": {"want": {"type": "string", "description": WANT}}}),
        ),
        op(
            "machines.run",
            "Run a shell command on a machine and wait for it to end: {runner, exit_code, output (stdout and stderr interleaved; the middle cut past 1,000,000 characters), why, synced}. A command that fails still resolves, with its exit code; a machine that can't be reached rejects.",
            false,
            true,
            command_schema(false),
        ),
        op("machines.start", "Start a shell command on a machine in the background: {id, runner, output so far}. Then machines.output and machines.stop (or the `output` and `stop` tools).", false, true, command_schema(false)),
        op(
            "machines.output",
            "A background run's output since the last read, and whether it has ended (exit_code). wait_s waits up to that long for it to end.",
            true,
            false,
            json!({"type": "object", "properties": {"id": {"type": "string"}, "wait_s": {"type": "number"}}, "required": ["id"]}),
        ),
        op("machines.stop", "Stop a background run (Ctrl-C, then terminate if it doesn't stop).", false, true, json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]})),
    ]
}

/// A run being followed: what it said so far, and how to signal it.
#[derive(Default)]
struct Run {
    runner: String,
    why: String,
    synced: Option<Value>,
    output: String,
    /// How much of `output` was handed out already.
    read: usize,
    exit: Option<Value>,
    error: Option<String>,
    signal: Option<std::os::unix::net::UnixStream>,
}

type Cell = Arc<(Mutex<Run>, std::sync::Condvar)>;
type Runs = Arc<Mutex<HashMap<String, Cell>>>;

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    let head: String = chars.iter().take(2_000).collect();
    let tail: String = chars.iter().skip(chars.len().saturating_sub(max - 2_000)).collect();
    format!("{head}\n[… {} characters cut …]\n{tail}", chars.len().saturating_sub(max))
}

/// Follow a run's events into `cell` until it ends (on a thread of its own).
fn follow(mut client: Client, cell: Cell) {
    std::thread::spawn(move || {
        loop {
            let ev = client.event();
            let (lock, cv) = &*cell;
            let Ok(mut r) = lock.lock() else { return };
            match ev {
                Ok(Some(Ev::Picked { runner, why })) => {
                    r.runner = runner;
                    r.why = why;
                }
                Ok(Some(Ev::Synced { root, sent, removed, bytes, ms })) => r.synced = Some(json!({"to": root, "files_sent": sent, "removed": removed, "bytes": bytes, "ms": ms})),
                Ok(Some(Ev::Output { data, .. })) => r.output.push_str(&String::from_utf8_lossy(&data)),
                Ok(Some(Ev::Exit { code, error, lost })) => {
                    r.exit = Some(json!({"exit_code": code, "error": error, "output_dropped_bytes": lost}));
                    cv.notify_all();
                    return;
                }
                Ok(Some(Ev::Error(e))) => {
                    r.error = Some(e);
                    cv.notify_all();
                    return;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    if r.exit.is_none() && r.error.is_none() {
                        r.error = Some("the kiln runner closed the connection".into());
                    }
                    cv.notify_all();
                    return;
                }
            }
            cv.notify_all();
        }
    });
}

fn text_result(v: &Value, error: bool) -> Value {
    json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(v).unwrap_or_default()}], "structuredContent": v, "isError": error})
}

/// Wait up to `secs` for `cell` to end; what it says now (new output only when `fresh`, cut past
/// `max` characters), and whether it failed.
fn report(id: &str, cell: &Cell, secs: u64, fresh: bool, max: usize) -> (Value, bool) {
    let lost = || (json!({"error": "lost track of the run"}), true);
    let (lock, cv) = &**cell;
    let Ok(mut r) = lock.lock() else { return lost() };
    let deadline = Instant::now() + Duration::from_secs(secs);
    while r.exit.is_none() && r.error.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match cv.wait_timeout(r, left) {
            Ok((g, _)) => r = g,
            Err(_) => return lost(),
        }
    }
    let out = if fresh { r.output.get(r.read..).unwrap_or_default().to_owned() } else { r.output.clone() };
    r.read = r.output.len();
    let done = r.exit.is_some() || r.error.is_some();
    let mut v = json!({"runner": r.runner, "output": clip(&out, max), "running": !done});
    if let Some(o) = v.as_object_mut() {
        if !done {
            o.insert("id".into(), json!(id));
        }
        if !r.why.is_empty() && !fresh {
            o.insert("why".into(), json!(r.why));
        }
        if let Some(s) = &r.synced
            && !fresh
        {
            o.insert("synced".into(), s.clone());
        }
        if let Some(e) = &r.exit
            && let Some(m) = e.as_object()
        {
            for (k, x) in m {
                if !x.is_null() && x != &json!(0) || k == "exit_code" {
                    o.insert(k.clone(), x.clone());
                }
            }
        }
        if let Some(e) = &r.error {
            o.insert("error".into(), json!(e));
        }
    }
    let failed = r.error.is_some() || r.exit.as_ref().and_then(|e| e.get("error")).is_some_and(|e| !e.is_null());
    (v, failed)
}

fn running(v: &Value) -> bool {
    v.get("running").and_then(Value::as_bool) == Some(true)
}

/// kiln's operations as the server lists them for this session, when it does: (when asked, the
/// registry, why there is none).
type Registry = (Instant, Vec<Value>, Option<String>);

struct Server {
    sock: std::path::PathBuf,
    prefer: Option<String>,
    cwd: String,
    /// This agent session: whom kiln's operations are asked for.
    session: String,
    runs: Runs,
    next: Mutex<u64>,
    registry: Mutex<Option<Registry>>,
}

impl Server {
    fn runners(&self, want: Option<String>) -> anyhow::Result<Value> {
        let mut c = Client::connect(&self.sock)?;
        c.send(&Req::Runners { want })?;
        match c.event()? {
            Some(Ev::Runners(j)) => Ok(serde_json::from_str(&j)?),
            Some(Ev::Error(e)) => anyhow::bail!("{e}"),
            other => anyhow::bail!("unexpected answer {other:?}"),
        }
    }

    /// Start a command as `args` says (the `run` tool's input): its id, and what follows it.
    fn start(&self, args: &Value) -> Result<(String, Cell), String> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.trim().is_empty());
        let command = s("command").ok_or("`command` is required")?;
        let cwd = s("cwd").unwrap_or_else(|| self.cwd.clone());
        let in_repo = std::process::Command::new("git").arg("-C").arg(&cwd).args(["rev-parse", "--is-inside-work-tree"]).output().is_ok_and(|o| o.status.success());
        let sync = args.get("sync").and_then(Value::as_bool).unwrap_or(in_repo);
        let env: Vec<(String, String)> = args.get("env").and_then(Value::as_object).map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned()))).collect()).unwrap_or_default();
        let req = RunReq {
            target: s("on").or_else(|| self.prefer.clone()).unwrap_or_default(),
            command,
            cwd: Some(cwd.clone()),
            remote_cwd: if sync { None } else { s("cwd") },
            sync,
            env,
            stdin: false,
            timeout_ms: args.get("timeout_s").and_then(Value::as_f64).map(|t| (t.max(0.0) * 1000.0) as u64),
            shell: s("shell"),
            detach: false,
        };
        let mut client = Client::connect(&self.sock).map_err(|e| format!("{e:#}"))?;
        client.send(&Req::Run(Box::new(req))).map_err(|e| format!("{e:#}"))?;
        let id = {
            let mut n = self.next.lock().map_err(|_| "busy".to_owned())?;
            *n += 1;
            format!("r{}", *n)
        };
        let signal = client.sender().ok();
        let cell: Cell = Arc::new((Mutex::new(Run { signal, ..Run::default() }), std::sync::Condvar::new()));
        if let Ok(mut runs) = self.runs.lock() {
            runs.insert(id.clone(), Arc::clone(&cell));
        }
        follow(client, Arc::clone(&cell));
        Ok((id, cell))
    }

    /// What run `id` says after waiting up to `secs`; a run that ended is forgotten.
    fn wait(&self, id: &str, cell: &Cell, secs: u64, fresh: bool, max: usize) -> (Value, bool) {
        let (v, failed) = report(id, cell, secs, fresh, max);
        if !running(&v)
            && let Ok(mut runs) = self.runs.lock()
        {
            runs.remove(id);
        }
        (v, failed)
    }

    fn cell(&self, id: &str) -> Option<Cell> {
        self.runs.lock().ok().and_then(|r| r.get(id).cloned())
    }

    /// Stop run `id`: Ctrl-C, then terminate if it still runs 5 s later.
    fn stop(&self, id: &str, max: usize) -> (Value, bool) {
        let Some(cell) = self.cell(id) else { return (json!({"error": format!("no run {id}")}), true) };
        if let Ok(mut r) = cell.0.lock()
            && let Some(w) = r.signal.as_mut()
        {
            let _ = super::local::send_on(w, &Req::Signal(Signal::Interrupt));
        }
        let (v, failed) = report(id, &cell, 5, true, max);
        if running(&v)
            && let Ok(mut r) = cell.0.lock()
            && let Some(w) = r.signal.as_mut()
        {
            let _ = super::local::send_on(w, &Req::Signal(Signal::Terminate));
        }
        if let Ok(mut runs) = self.runs.lock() {
            runs.remove(id);
        }
        (v, failed)
    }

    /// Ask the kiln server, through the runner, for this session.
    fn ask(&self, mut q: Value) -> anyhow::Result<Value> {
        anyhow::ensure!(!self.session.is_empty(), "this MCP server wasn't told its agent session (KILN_SESSION), so it can't ask kiln for anything");
        if let Some(o) = q.as_object_mut() {
            o.insert("session".into(), json!(self.session));
        }
        let mut c = Client::connect(&self.sock)?;
        c.send(&Req::Ask { json: q.to_string() })?;
        match c.event()? {
            Some(Ev::Answer(j)) => Ok(serde_json::from_str(&j)?),
            Some(Ev::Error(e)) => anyhow::bail!("{e}"),
            None => anyhow::bail!("the kiln runner closed the connection (a runner older than kiln's operations for agents doesn't answer them: upgrade it)"),
            other => anyhow::bail!("unexpected answer {other:?}"),
        }
    }

    /// kiln's operations on the server, for this session (asked again after a minute). When the
    /// server can't be asked, the ones this kiln knows, and why they can't be called: a program
    /// calling one learns that, rather than finding `kiln.ci` undefined.
    fn remote(&self) -> (Vec<Value>, Option<String>) {
        if let Ok(g) = self.registry.lock()
            && let Some((at, reg, why)) = g.as_ref()
            && at.elapsed() < REGISTRY_TTL
        {
            return (reg.clone(), why.clone());
        }
        let (reg, why) = match self.ask(json!({"op": "registry"})) {
            Ok(a) => match (a.get("registry").and_then(Value::as_array), a.get("error").and_then(Value::as_str)) {
                (Some(r), _) => (r.clone(), None),
                (None, Some(e)) => (Vec::new(), Some(e.to_owned())),
                // A server from before: "unknown reach question".
                (None, None) => (Vec::new(), Some("this kiln server doesn't offer its operations to agents on runners yet: upgrade it".to_owned())),
            },
            Err(e) => (Vec::new(), Some(format!("{e:#}"))),
        };
        let reg = if why.is_some() { code::registry_of(&crate::ops::listed(true)).as_array().cloned().unwrap_or_default() } else { reg };
        if let Ok(mut g) = self.registry.lock() {
            *g = Some((Instant::now(), reg.clone(), why.clone()));
        }
        (reg, why)
    }

    fn call(self: &Arc<Self>, name: &str, args: &Value) -> Value {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.trim().is_empty());
        match name {
            "code_run" | "code_search" => {
                let program = args.get("program").and_then(Value::as_str).unwrap_or("").to_owned();
                let (remote, unavailable) = self.remote();
                let host = Arc::new(Program { server: Arc::clone(self), remote, unavailable, mine: Mutex::new(Vec::new()) });
                let wall = args.get("timeout_s").and_then(Value::as_f64).map_or(CODE_WALL_DEFAULT, |t| Duration::from_secs_f64(t.clamp(1.0, CODE_WALL_MAX.as_secs_f64())));
                let mode = if name == "code_search" { code::Mode::Search } else { code::Mode::Run { input: args.get("input").cloned().unwrap_or(Value::Null) } };
                let limits = code::Limits { wall, ..code::Limits::default() };
                match code::run_on(host, mode, &program, limits) {
                    Ok(out) => text_result(&out, false),
                    Err(e) => text_result(&json!({"error": e.message, "code": e.code, "calls": e.calls}), true),
                }
            }
            "runners" => match self.runners(s("want")) {
                Ok(v) => text_result(&v, false),
                Err(e) => text_result(&json!({"error": format!("{e:#}")}), true),
            },
            "run" => {
                let (id, cell) = match self.start(args) {
                    Ok(r) => r,
                    Err(e) => return text_result(&json!({"error": e}), true),
                };
                let background = args.get("background").and_then(Value::as_bool).unwrap_or(false);
                let wait = if background { 2 } else { args.get("wait_s").and_then(Value::as_f64).map_or(WAIT_DEFAULT_S, |w| w.max(0.0) as u64) };
                // Finished runs are forgotten; unfinished ones stay for `output` and `stop`.
                let (v, failed) = self.wait(&id, &cell, wait, false, OUTPUT_MAX);
                text_result(&v, failed)
            }
            "output" => {
                let Some(id) = s("id") else { return text_result(&json!({"error": "`id` is required"}), true) };
                let Some(cell) = self.cell(&id) else { return text_result(&json!({"error": format!("no run {id} (it ended and was collected, or never was)")}), true) };
                let wait = args.get("wait_s").and_then(Value::as_f64).map_or(0, |w| w.max(0.0) as u64);
                let (v, failed) = self.wait(&id, &cell, wait, true, OUTPUT_MAX);
                text_result(&v, failed)
            }
            "stop" => {
                let Some(id) = s("id") else { return text_result(&json!({"error": "`id` is required"}), true) };
                let (v, failed) = self.stop(&id, OUTPUT_MAX);
                text_result(&v, failed)
            }
            other => text_result(&json!({"error": format!("no tool {other}")}), true),
        }
    }
}

/// One program's calls ([`code::Host`]): `machines.*` here, the rest asked of the server.
struct Program {
    server: Arc<Server>,
    remote: Vec<Value>,
    /// Why kiln's operations aren't there, when they aren't.
    unavailable: Option<String>,
    /// Runs it waits on: stopped if it ends without them.
    mine: Mutex<Vec<String>>,
}

impl code::Host for Program {
    fn names(&self) -> Vec<String> {
        machine_ops().iter().chain(self.remote.iter()).filter_map(|o| o.get("name").and_then(Value::as_str).map(str::to_owned)).collect()
    }

    fn registry(&self) -> Value {
        Value::Array(machine_ops().into_iter().chain(self.remote.iter().cloned()).collect())
    }

    fn call(&self, name: &str, input: Value) -> Result<Value, Value> {
        let err = |e: String| json!({"error": e});
        let id_of = |input: &Value| input.get("id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| err("`id` is required".into()));
        match name {
            "machines.list" => self.server.runners(input.get("want").and_then(Value::as_str).map(str::to_owned)).map_err(|e| err(format!("{e:#}"))),
            "machines.run" | "machines.start" => {
                let (id, cell) = self.server.start(&input).map_err(err)?;
                let (v, failed) = if name == "machines.run" {
                    if let Ok(mut m) = self.mine.lock() {
                        m.push(id.clone());
                    }
                    // Until it ends: the program's own time bounds it.
                    self.server.wait(&id, &cell, CODE_WALL_MAX.as_secs(), false, CODE_OUTPUT_MAX)
                } else {
                    self.server.wait(&id, &cell, 2, false, CODE_OUTPUT_MAX)
                };
                // A command that failed resolves with its exit code; a run that couldn't be had rejects.
                if failed && v.get("exit_code").is_none() { Err(v) } else { Ok(v) }
            }
            "machines.output" => {
                let id = id_of(&input)?;
                let cell = self.server.cell(&id).ok_or_else(|| err(format!("no run {id} (it ended and was collected, or never was)")))?;
                let wait = input.get("wait_s").and_then(Value::as_f64).map_or(0, |w| w.max(0.0) as u64);
                Ok(self.server.wait(&id, &cell, wait, true, CODE_OUTPUT_MAX).0)
            }
            "machines.stop" => {
                let id = id_of(&input)?;
                let (v, failed) = self.server.stop(&id, CODE_OUTPUT_MAX);
                if failed && v.get("exit_code").is_none() && running(&v) { Err(v) } else { Ok(v) }
            }
            _ if self.remote.iter().any(|o| o.get("name").and_then(Value::as_str) == Some(name)) => {
                if let Some(why) = &self.unavailable {
                    return Err(err(format!("kiln's own operations can't be called from here: {why}")));
                }
                let a = self.server.ask(json!({"op": "call", "name": name, "input": input})).map_err(|e| err(format!("{e:#}")))?;
                match a.get("ok") {
                    Some(v) => Ok(v.clone()),
                    None => Err(a),
                }
            }
            _ => Err(err(format!("no operation {name}: code_search lists them"))),
        }
    }

    fn abandon(&self) {
        let ids = self.mine.lock().map(|m| m.clone()).unwrap_or_default();
        for id in ids.into_iter().filter(|id| self.server.cell(id).is_some()) {
            let server = Arc::clone(&self.server);
            std::thread::spawn(move || {
                let _ = server.stop(&id, OUTPUT_MAX);
            });
        }
    }
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// `kiln reach mcp`: serve MCP on stdin/stdout until stdin closes. Tool calls run side by side.
pub fn serve() -> anyhow::Result<()> {
    let sock = super::local::find_socket()?;
    let prefer = std::env::var("KILN_REACH_ON").ok().filter(|s| !s.trim().is_empty());
    let session = std::env::var("KILN_SESSION").unwrap_or_default();
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let machines = machines(&sock);
    let server = Arc::new(Server { sock, prefer, cwd, session, runs: Arc::default(), next: Mutex::new(0), registry: Mutex::new(None) });
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let say = |out: &Arc<Mutex<std::io::Stdout>>, v: &Value| {
        if let Ok(mut o) = out.lock() {
            let _ = writeln!(o, "{v}");
            let _ = o.flush();
        }
    };
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                say(&out, &rpc_error(Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            }
        };
        let Some(id) = msg.get("id").cloned().filter(|i| !i.is_null()) else { continue };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_owned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method.as_str() {
            "initialize" => {
                let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18").to_owned();
                say(
                    &out,
                    &rpc_ok(
                        id,
                        json!({
                            "protocolVersion": version,
                            "capabilities": {"tools": {"listChanged": false}, "resources": {"listChanged": false}},
                            "serverInfo": {"name": "kiln", "title": "kiln", "version": env!("CARGO_PKG_VERSION")},
                            "instructions": instructions(server.prefer.as_deref(), &machines),
                        }),
                    ),
                );
            }
            "ping" => say(&out, &rpc_ok(id, json!({}))),
            "tools/list" => say(&out, &rpc_ok(id, json!({"tools": tools()}))),
            "tools/call" => {
                let (server, out) = (Arc::clone(&server), Arc::clone(&out));
                std::thread::spawn(move || {
                    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                    let args = params.get("arguments").cloned().unwrap_or(json!({}));
                    let v = server.call(name, &args);
                    if let Ok(mut o) = out.lock() {
                        let _ = writeln!(o, "{}", rpc_ok(id, v));
                        let _ = o.flush();
                    }
                });
            }
            // kiln's skills (deploying, diagnosing): resources, as the server offers them.
            "resources/list" => say(&out, &rpc_ok(id, json!({"resources": crate::mcp::skills::resources()}))),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
                match crate::mcp::skills::read(uri) {
                    Some(c) => say(&out, &rpc_ok(id, json!({"contents": [c]}))),
                    None => say(&out, &rpc_error(id, -32002, &format!("no resource {uri}"))),
                }
            }
            "resources/templates/list" => say(&out, &rpc_ok(id, json!({"resourceTemplates": []}))),
            "prompts/list" => say(&out, &rpc_ok(id, json!({"prompts": []}))),
            other => say(&out, &rpc_error(id, -32601, &format!("method not found: {other}"))),
        }
    }
    Ok(())
}

/// The server's environment: the runner's reach socket, the session's preferred runner, and the
/// session.
fn env_of(sock: &str, prefer: Option<&str>, session: &str) -> Vec<(&'static str, String)> {
    let mut env = vec![("KILN_REACH_SOCK", sock.to_owned())];
    if let Some(p) = prefer {
        env.push(("KILN_REACH_ON", p.to_owned()));
    }
    if !session.is_empty() {
        env.push(("KILN_SESSION", session.to_owned()));
    }
    env
}

/// How a harness started on this runner gets this server: Claude Code's `--mcp-config` value.
pub fn claude_config(kiln: &std::path::Path, sock: &str, prefer: Option<&str>, session: &str) -> String {
    let env: serde_json::Map<String, Value> = env_of(sock, prefer, session).into_iter().map(|(k, v)| (k.to_owned(), json!(v))).collect();
    json!({"mcpServers": {"kiln": {"type": "stdio", "command": kiln, "args": ["reach", "mcp"], "env": env}}}).to_string()
}

/// The same for Codex: `-c` overrides adding the server, its tool calls given as long as a
/// program may run (Codex otherwise stops waiting after 60 s).
pub fn codex_config(kiln: &std::path::Path, sock: &str, prefer: Option<&str>, session: &str) -> Vec<String> {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let env: Vec<String> = env_of(sock, prefer, session).into_iter().map(|(k, v)| format!("{k}={}", q(&v))).collect();
    vec![
        "-c".into(),
        format!("mcp_servers.kiln.command={}", q(&kiln.to_string_lossy())),
        "-c".into(),
        "mcp_servers.kiln.args=[\"reach\", \"mcp\"]".into(),
        "-c".into(),
        format!("mcp_servers.kiln.env={{{}}}", env.join(", ")),
        "-c".into(),
        format!("mcp_servers.kiln.tool_timeout_sec={}", CODE_WALL_MAX.as_secs() + 60),
    ]
}

/// The same for an ACP agent: its session's `mcpServers` (stdio).
pub fn acp_config(kiln: &std::path::Path, sock: &str, prefer: Option<&str>, session: &str) -> Value {
    let env: Vec<Value> = env_of(sock, prefer, session).into_iter().map(|(k, v)| json!({"name": k, "value": v})).collect();
    json!({"name": "kiln", "command": kiln, "args": ["reach", "mcp"], "env": env})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_output_keeps_its_head_and_tail() {
        let s: String = (0..50_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let c = clip(&s, OUTPUT_MAX);
        assert!(c.starts_with(&s[..100]));
        assert!(c.ends_with(&s[s.len() - 100..]));
        assert!(c.contains("characters cut"));
        assert_eq!(clip("short", OUTPUT_MAX), "short");
        assert_eq!(clip(&s, CODE_OUTPUT_MAX), s, "a program gets far more");
    }

    #[test]
    fn harness_configs_name_the_socket_the_preference_and_the_session() -> anyhow::Result<()> {
        let c: Value = serde_json::from_str(&claude_config(std::path::Path::new("/opt/kiln"), "/run/x.sock", Some("linux/amd64"), "s1"))?;
        assert_eq!(c.pointer("/mcpServers/kiln/command"), Some(&json!("/opt/kiln")));
        assert_eq!(c.pointer("/mcpServers/kiln/env/KILN_REACH_ON"), Some(&json!("linux/amd64")));
        assert_eq!(c.pointer("/mcpServers/kiln/env/KILN_SESSION"), Some(&json!("s1")));
        let x = codex_config(std::path::Path::new("/opt/kiln"), "/run/x.sock", None, "s1");
        assert!(x.iter().any(|a| a == "mcp_servers.kiln.command=\"/opt/kiln\""));
        assert!(x.iter().any(|a| a == "mcp_servers.kiln.env={KILN_REACH_SOCK=\"/run/x.sock\", KILN_SESSION=\"s1\"}"), "{x:?}");
        assert!(x.iter().any(|a| a == "mcp_servers.kiln.tool_timeout_sec=3660"), "Codex waits as long as a program may run");
        let a = acp_config(std::path::Path::new("/opt/kiln"), "/run/x.sock", None, "s1");
        assert_eq!(a.get("name"), Some(&json!("kiln")));
        assert_eq!(a.pointer("/env/1"), Some(&json!({"name": "KILN_SESSION", "value": "s1"})));
        let t = tools();
        let names: Vec<&str> = t.as_array().into_iter().flatten().filter_map(|t| t.get("name").and_then(Value::as_str)).collect();
        assert_eq!(names, ["code_run", "code_search", "runners", "run", "output", "stop"]);
        assert!(instructions(Some("adacachy"), "").contains("prefers commands to run on adacachy"));
        let i = instructions(None, "");
        assert!(i.contains("run it there yourself") && i.contains("don't hand the person a command to type") && i.contains("code_run"));
        let v = json!({"me": "adacachy", "runners": [{"runner": {"name": "adacachy", "os": "linux", "arch": "x86_64"}}, {"runner": {"name": "mac", "os": "macos", "arch": "aarch64"}}], "passed": [{"runner": {"name": "box", "os": "windows", "arch": "x86_64"}}]});
        assert_eq!(describe(&v), "adacachy (linux/x86_64, this machine), mac (macos/aarch64), box (windows/x86_64)");
        assert!(instructions(None, &describe(&v)).contains("This kiln's machines: adacachy (linux/x86_64, this machine), mac (macos/aarch64)"));
        Ok(())
    }

    #[test]
    fn a_program_sees_the_machines_and_the_servers_operations() {
        let ops = machine_ops();
        let names: Vec<&str> = ops.iter().filter_map(|o| o.get("name").and_then(Value::as_str)).collect();
        assert_eq!(names, ["machines.list", "machines.run", "machines.start", "machines.output", "machines.stop"]);
        assert!(ops.iter().all(|o| o.pointer("/input/type") == Some(&json!("object"))));
    }
}
