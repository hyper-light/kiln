//! `kiln reach mcp`: reach as tools for the agents kiln starts on a runner (`docs/reach.md` §7),
//! an MCP server over stdio that talks to the runner's reach socket. Any harness that takes MCP
//! servers gets them: Claude Code through `--mcp-config`, Codex through `-c mcp_servers.*`.
//!
//! - `runners`: which machines there are, best first for what the work needs;
//! - `run`: a command on one of them (by name, or by what it must be: `linux/amd64`), this
//!   repository synced over first; returns its output and exit code, or runs on in the background;
//! - `output` / `stop`: a background run's new output, and stopping it.
//!
//! The session's preferred runner (`KILN_REACH_ON`) is the default for `on`.

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::local::{Client, Ev, Req, RunReq};
use super::wire::Signal;

/// Output a tool answer carries at most (the middle goes, as Claude Code's own Bash tool does).
const OUTPUT_MAX: usize = 30_000;
/// A foreground run returns after this long by default (it then keeps running in the background).
const WAIT_DEFAULT_S: u64 = 600;

fn instructions(prefer: Option<&str>) -> String {
    let mut s = "kiln reach runs commands on the other machines of this kiln (its runners), end-to-end encrypted. Use it when \
work needs another platform or more power than this machine: build and test for Linux x86-64 from a Mac (`run` with \
on: \"linux/amd64\"), use the machine with the GPU (on: \"+gpu\"), or run on a machine by name. The repository you are in \
is synced to that machine first (only what changed; git sees the same state there), and the command runs in the matching \
folder, so edit here and build or test there. `runners` lists the machines, best first. Long commands can run in the \
background (background: true), then `output` and `stop`."
        .to_owned();
    if let Some(p) = prefer {
        s.push_str(&format!(" This session's person prefers commands to run on {p}: `run` goes there unless you say otherwise."));
    }
    s
}

fn tools() -> Value {
    json!([
        {
            "name": "runners",
            "description": "The machines (kiln runners) commands can run on, best first for `want`: platform, idle cores, free memory, installed agents, and why others don't suit. `want` is a runner's name or what it must be: `linux/amd64`, `macos`, `arm64`, `linux,+gpu`, `cpus>=16`, `mem>=32g`.",
            "inputSchema": {"type": "object", "properties": {"want": {"type": "string", "description": "What the machine must be (optional)."}}},
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "run",
            "description": "Run a shell command (bash) on another machine. `on` picks it: a runner's name, or what it must be (`linux/amd64`, `macos/arm64`, `+gpu`, `cpus>=16`); the idlest that suits is used. By default this repository is synced there first (git-tracked and untracked-but-not-ignored files plus .git; build output like target/ stays home) and the command runs in the matching folder. Returns the exit code and output (stdout and stderr interleaved, the middle cut past 30k characters). Set background to return at once with an id for `output` and `stop`.",
            "inputSchema": {"type": "object", "properties": {
                "command": {"type": "string", "description": "Shell text, run by bash in the synced copy of this repository."},
                "on": {"type": "string", "description": "Where: a runner's name or what it must be (`linux/amd64`). Default: the session's preferred runner, else the best of all."},
                "sync": {"type": "boolean", "description": "Copy this repository over first and run in it (default: true inside a git repository)."},
                "cwd": {"type": "string", "description": "With sync: the folder here whose repository is synced and whose counterpart the command runs in (default: the session's folder). Without sync: the folder on the other machine (default: its home)."},
                "timeout_s": {"type": "number", "description": "Stop the command after this many seconds."},
                "wait_s": {"type": "number", "description": "Return after this many seconds even if it is still running (it keeps running in the background; default 600)."},
                "background": {"type": "boolean", "description": "Return at once with an id; read with `output`, end with `stop`."},
                "shell": {"type": "string", "description": "A named shell over there: its folder and exported variables carry over between runs that name it."},
                "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Environment for the command."}
            }, "required": ["command"]},
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

type Runs = Arc<Mutex<HashMap<String, Arc<(Mutex<Run>, std::sync::Condvar)>>>>;

fn clip(s: &str) -> String {
    if s.chars().count() <= OUTPUT_MAX {
        return s.to_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    let head: String = chars.iter().take(2_000).collect();
    let tail: String = chars.iter().skip(chars.len().saturating_sub(OUTPUT_MAX - 2_000)).collect();
    format!("{head}\n[… {} characters cut …]\n{tail}", chars.len().saturating_sub(OUTPUT_MAX))
}

/// Follow a run's events into `cell` until it ends (on a thread of its own).
fn follow(mut client: Client, cell: Arc<(Mutex<Run>, std::sync::Condvar)>) {
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

/// Wait up to `secs` for `cell` to end; what it says now (new output only when `fresh`).
fn report(id: &str, cell: &Arc<(Mutex<Run>, std::sync::Condvar)>, secs: u64, fresh: bool) -> Value {
    let (lock, cv) = &**cell;
    let Ok(mut r) = lock.lock() else { return text_result(&json!({"error": "lost track of the run"}), true) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while r.exit.is_none() && r.error.is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match cv.wait_timeout(r, left) {
            Ok((g, _)) => r = g,
            Err(_) => return text_result(&json!({"error": "lost track of the run"}), true),
        }
    }
    let out = if fresh { r.output.get(r.read..).unwrap_or_default().to_owned() } else { r.output.clone() };
    r.read = r.output.len();
    let done = r.exit.is_some() || r.error.is_some();
    let mut v = json!({"runner": r.runner, "output": clip(&out), "running": !done});
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
    text_result(&v, failed)
}

struct Server {
    sock: std::path::PathBuf,
    prefer: Option<String>,
    cwd: String,
    runs: Runs,
    next: Mutex<u64>,
}

impl Server {
    fn call(&self, name: &str, args: &Value) -> Value {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.trim().is_empty());
        match name {
            "runners" => {
                let ask = || -> anyhow::Result<Value> {
                    let mut c = Client::connect(&self.sock)?;
                    c.send(&Req::Runners { want: s("want") })?;
                    match c.event()? {
                        Some(Ev::Runners(j)) => Ok(serde_json::from_str(&j)?),
                        Some(Ev::Error(e)) => anyhow::bail!("{e}"),
                        other => anyhow::bail!("unexpected answer {other:?}"),
                    }
                };
                match ask() {
                    Ok(v) => text_result(&v, false),
                    Err(e) => text_result(&json!({"error": format!("{e:#}")}), true),
                }
            }
            "run" => {
                let Some(command) = s("command") else { return text_result(&json!({"error": "`command` is required"}), true) };
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
                let start = || -> anyhow::Result<Client> {
                    let mut c = Client::connect(&self.sock)?;
                    c.send(&Req::Run(Box::new(req)))?;
                    Ok(c)
                };
                let client = match start() {
                    Ok(c) => c,
                    Err(e) => return text_result(&json!({"error": format!("{e:#}")}), true),
                };
                let id = {
                    let Ok(mut n) = self.next.lock() else { return text_result(&json!({"error": "busy"}), true) };
                    *n += 1;
                    format!("r{}", *n)
                };
                let signal = client.sender().ok();
                let cell = Arc::new((Mutex::new(Run { signal, ..Run::default() }), std::sync::Condvar::new()));
                if let Ok(mut runs) = self.runs.lock() {
                    runs.insert(id.clone(), Arc::clone(&cell));
                }
                follow(client, Arc::clone(&cell));
                let background = args.get("background").and_then(Value::as_bool).unwrap_or(false);
                let wait = if background { 2 } else { args.get("wait_s").and_then(Value::as_f64).map_or(WAIT_DEFAULT_S, |w| w.max(0.0) as u64) };
                let v = report(&id, &cell, wait, false);
                // Finished runs are forgotten; unfinished ones stay for `output` and `stop`.
                if v.get("structuredContent").and_then(|c| c.get("running")).and_then(Value::as_bool) == Some(false)
                    && let Ok(mut runs) = self.runs.lock()
                {
                    runs.remove(&id);
                }
                v
            }
            "output" => {
                let Some(id) = s("id") else { return text_result(&json!({"error": "`id` is required"}), true) };
                let Some(cell) = self.runs.lock().ok().and_then(|r| r.get(&id).cloned()) else { return text_result(&json!({"error": format!("no run {id} (it ended and was collected, or never was)")}), true) };
                let wait = args.get("wait_s").and_then(Value::as_f64).map_or(0, |w| w.max(0.0) as u64);
                let v = report(&id, &cell, wait, true);
                if v.get("structuredContent").and_then(|c| c.get("running")).and_then(Value::as_bool) == Some(false)
                    && let Ok(mut runs) = self.runs.lock()
                {
                    runs.remove(&id);
                }
                v
            }
            "stop" => {
                let Some(id) = s("id") else { return text_result(&json!({"error": "`id` is required"}), true) };
                let Some(cell) = self.runs.lock().ok().and_then(|r| r.get(&id).cloned()) else { return text_result(&json!({"error": format!("no run {id}")}), true) };
                if let Ok(mut r) = cell.0.lock()
                    && let Some(w) = r.signal.as_mut()
                {
                    let _ = super::local::send_on(w, &Req::Signal(Signal::Interrupt));
                }
                let v = report(&id, &cell, 5, true);
                if v.get("structuredContent").and_then(|c| c.get("running")).and_then(Value::as_bool) == Some(true)
                    && let Ok(mut r) = cell.0.lock()
                    && let Some(w) = r.signal.as_mut()
                {
                    let _ = super::local::send_on(w, &Req::Signal(Signal::Terminate));
                }
                if let Ok(mut runs) = self.runs.lock() {
                    runs.remove(&id);
                }
                v
            }
            other => text_result(&json!({"error": format!("no tool {other}")}), true),
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
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let server = Arc::new(Server { sock, prefer, cwd, runs: Arc::default(), next: Mutex::new(0) });
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
                            "capabilities": {"tools": {"listChanged": false}},
                            "serverInfo": {"name": "kiln-reach", "title": "kiln reach", "version": env!("CARGO_PKG_VERSION")},
                            "instructions": instructions(server.prefer.as_deref()),
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
            "resources/list" => say(&out, &rpc_ok(id, json!({"resources": []}))),
            "prompts/list" => say(&out, &rpc_ok(id, json!({"prompts": []}))),
            other => say(&out, &rpc_error(id, -32601, &format!("method not found: {other}"))),
        }
    }
    Ok(())
}

/// How a harness started on this runner gets these tools: Claude Code's `--mcp-config` value.
pub fn claude_config(kiln: &std::path::Path, sock: &str, prefer: Option<&str>) -> String {
    let mut env = json!({"KILN_REACH_SOCK": sock});
    if let (Some(p), Some(m)) = (prefer, env.as_object_mut()) {
        m.insert("KILN_REACH_ON".into(), json!(p));
    }
    json!({"mcpServers": {"kiln-reach": {"type": "stdio", "command": kiln, "args": ["reach", "mcp"], "env": env}}}).to_string()
}

/// The same for Codex: `-c` overrides adding the server.
pub fn codex_config(kiln: &std::path::Path, sock: &str, prefer: Option<&str>) -> Vec<String> {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mut env = format!("{{KILN_REACH_SOCK={}", q(sock));
    if let Some(p) = prefer {
        env.push_str(&format!(", KILN_REACH_ON={}", q(p)));
    }
    env.push('}');
    vec![
        "-c".into(),
        format!("mcp_servers.kiln_reach.command={}", q(&kiln.to_string_lossy())),
        "-c".into(),
        "mcp_servers.kiln_reach.args=[\"reach\", \"mcp\"]".into(),
        "-c".into(),
        format!("mcp_servers.kiln_reach.env={env}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_output_keeps_its_head_and_tail() {
        let s: String = (0..50_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let c = clip(&s);
        assert!(c.starts_with(&s[..100]));
        assert!(c.ends_with(&s[s.len() - 100..]));
        assert!(c.contains("characters cut"));
        assert_eq!(clip("short"), "short");
    }

    #[test]
    fn harness_configs_name_the_socket_and_the_preference() -> anyhow::Result<()> {
        let c: Value = serde_json::from_str(&claude_config(std::path::Path::new("/opt/kiln"), "/run/x.sock", Some("linux/amd64")))?;
        assert_eq!(c.pointer("/mcpServers/kiln-reach/command"), Some(&json!("/opt/kiln")));
        assert_eq!(c.pointer("/mcpServers/kiln-reach/env/KILN_REACH_ON"), Some(&json!("linux/amd64")));
        let x = codex_config(std::path::Path::new("/opt/kiln"), "/run/x.sock", None);
        assert!(x.iter().any(|a| a == "mcp_servers.kiln_reach.command=\"/opt/kiln\""));
        assert!(x.iter().any(|a| a == "mcp_servers.kiln_reach.env={KILN_REACH_SOCK=\"/run/x.sock\"}"));
        assert!(tools().as_array().is_some_and(|t| t.len() == 4));
        assert!(instructions(Some("adacachy")).contains("prefers commands to run on adacachy"));
        Ok(())
    }
}
