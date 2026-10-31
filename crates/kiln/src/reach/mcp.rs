//! `kiln reach mcp`: kiln's MCP server for the agents kiln starts on a runner (`docs/reach.md` §7,
//! docs/access.md), over stdio, talking to the runner's reach socket. Every harness that takes MCP
//! servers gets it as `kiln`: Claude Code through `--mcp-config`, Codex through `-c mcp_servers.*`,
//! ACP agents through their session's `mcpServers`, Antigravity through its own config (`agy mcp
//! add`, once for every session), each session's identity in agy's environment.
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
//! - `output` / `stop`: a background run's new output, and stopping it;
//! - `agents_*`, `goals_*`, `plans_*`: kiln's operations an agent uses most, as tools of their own
//!   (the agents it starts, its own goal stack, plans kept and run), asked of the server like the
//!   rest.
//!
//! The session's preferred runner (`KILN_REACH_ON`) is the default for `on`; the session itself
//! (`KILN_SESSION`) is whom kiln's operations are asked for, and the instructions tell the agent
//! that's who it is.

use std::collections::HashMap;
use std::io::{BufRead, Write as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::local::{Client, Ev, Req, RunReq};
use super::wire::Signal;
use crate::mcp::code;
use crate::ops::Input;

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
/// Word of what it couldn't read on its way to the server at once, at most.
const NOTING_MOST: usize = 4;

fn instructions(prefer: Option<&str>, machines: &str, session: &str) -> String {
    let mut s = "This is kiln's own MCP server: the machines of this kiln (its runners), and kiln itself (CI, deployments). \
When a step has to happen on another machine (the person's Mac, a server, the box with the GPU), run it there yourself: \
don't hand the person a command to type. When a step needs a person at that machine (a password, a system dialog to \
allow, something physical), start it yourself and tell them exactly what will appear and what to do. Everything else \
runs on this machine, with your own tools: use another only when this one can't do the step, because it needs another \
platform (build and test for Linux x86-64 from a Mac: needs: \"linux/amd64\"), a GPU (needs: \"+gpu\"), or more memory, \
disk or cores than this machine has (needs: \"cpus=16,mem=32g\": that many cores idle and that much memory free); `on` \
names a machine for a step that has to happen on that one. `run` runs one command on one machine. `code_run` runs a \
JavaScript program against kiln, and only what it returns comes back to you: the same build or test on several machines at \
once, returning just what failed (`const [mac, linux] = await Promise.all([kiln.machines.run({needs: \"macos\", command: \
\"cargo test\"}), kiln.machines.run({needs: \"linux/amd64\", command: \"cargo test\"})])`), or CI runs and deployments \
(`kiln.ci.*`, `kiln.cd.*`; `code_search` lists every operation and its input). The repository you are in is synced to \
the other machine first (only what changed; git sees the same state there), and the command runs in the matching \
folder, so you edit here and it builds or tests there; for a command that isn't about this repository, pass sync: false (it runs \
in that machine's home folder). `runners` lists the machines, this one first when it suits. Long commands can run in the background \
(background: true), then `output` and `stop`. Nothing a command leaves running outlives it: what a run leaves behind \
(`nohup … &`, a daemon) ends when it does, and what your own shell commands leave running on their own ends when your \
turn does; run a server or anything that must keep going as a background task of your shell tool, or a background run. To hand parts of the work to other agents (any harness, any machine), \
`agents_start` them with a clear brief, then carry on or end your turn: kiln tells you when each is done (`agents_wait` \
when you can't go on without one), and `agents_result` reads what it concluded: \
they get no more permissions than you have, and what they ask a person stays a person's to answer (the kiln-agents \
skill says how). To show the person a file from this machine (a screenshot, a recording, a PDF, a log), write its full \
path in your message, as a Markdown image for a picture (`![phone view](/tmp/shot.png)`) or a link for the rest: kiln \
shows it in the conversation and keeps a copy, so it still shows after the file goes. To show something live (a chart, a \
table, a diagram, a mock-up, a small tool to try), write a page and show it with show_page: it runs in the conversation, \
sized to fit, in kiln's look; look at it with preview_page first. A page that needs this machine (your dev server, its files) \
you show live with show_live: it streams to them as they look, and use_live lets you use it with them."
        .to_owned();
    s.push(' ');
    s.push_str(crate::mcp::GOALS_AND_PLANS);
    if !session.is_empty() {
        s.push(' ');
        s.push_str(&crate::mcp::you_are(session));
    }
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

/// What this server's own tool `tool` takes ([`input`]): its schema is the tool's `inputSchema`,
/// and each call's arguments are read as it before it runs ([`Server::call`]). None for kiln's
/// operations an agent has as tools: the kiln server reads those (`crate::mcp::runner_ask`).
fn tool_input(tool: &str) -> Option<Input> {
    Some(match tool {
        "code_run" => Input::of::<input::CodeRun>(),
        "code_search" => Input::of::<crate::mcp::input::CodeSearch>(),
        "runners" => Input::of::<input::Runners>(),
        "run" => Input::of::<input::Run>(),
        "output" => Input::of::<input::Output>(),
        "stop" => Input::of::<input::Stop>(),
        "preview_page" => Input::of::<input::PreviewPage>(),
        "show_page" => Input::of::<input::ShowPage>(),
        "show_live" => Input::of::<input::ShowLive>(),
        "use_live" => Input::of::<input::UseLive>(),
        _ => return None,
    })
}

/// What `kiln.machines.<name>` takes, as [`tool_input`] says for a tool: its schema is the
/// operation's `input` (`code_search`), and each call's input is read as it ([`Program::call`]).
fn machine_input(name: &str) -> Option<Input> {
    Some(match name {
        "machines.list" => Input::of::<input::MachinesList>(),
        "machines.run" | "machines.start" => Input::of::<input::Command>(),
        "machines.output" => Input::of::<input::Output>(),
        "machines.stop" => Input::of::<input::Stop>(),
        _ => return None,
    })
}

/// Where a command of `run` and `kiln.machines.run` goes, as its runner reads it: `on` and `needs`
/// as one spelling ([`super::directory::target`]), the session's preferred runner when neither is
/// given.
fn run_target(args: &Value, prefer: Option<&str>) -> Result<String, String> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    let (on, needs) = (s("on"), s("needs").unwrap_or_default());
    let on = on.or(if needs.is_empty() { prefer } else { None }).unwrap_or_default();
    super::directory::target(on, needs).map_err(|e| format!("`needs`: {e}"))
}

/// kiln's operations an agent has as tools of its own ([`crate::mcp::agent_ops`]: `agents_start`,
/// `goals_plan`, `plans_run`, …), as this binary knows them: what's offered when the server can't
/// say (one from before it said, or out of reach).
fn agent_tools() -> Vec<Value> {
    crate::mcp::agent_ops().iter().map(|o| tool_of(o.name, &json!(o.summary), &o.input.schema(), o.read_only, o.destructive)).collect()
}

/// One operation as an agent's tool, asked of the server for this session like the rest of kiln's
/// operations: what an agent starts here is its child, and the goals it lines up without naming an
/// agent are its own.
fn tool_of(op: &str, summary: &Value, input: &Value, read_only: bool, destructive: bool) -> Value {
    json!({
        "name": crate::mcp::tool_name(op),
        "title": op,
        "description": summary,
        "inputSchema": input,
        "annotations": {"readOnlyHint": read_only, "destructiveHint": destructive, "openWorldHint": false},
    })
}

/// The agent tools as the server lists them (`agent_tools`, each as its registry has it); `None`
/// from a server that doesn't say.
fn agent_tools_of(registry: &[Value], names: Option<&[String]>) -> Option<Vec<Value>> {
    let names = names?;
    let by_name: HashMap<&str, &Value> = registry.iter().filter_map(|o| Some((o.get("name")?.as_str()?, o))).collect();
    Some(
        names
            .iter()
            .filter_map(|n| by_name.get(n.as_str()))
            .filter_map(|o| {
                let op = o.get("name")?.as_str()?;
                let flag = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
                Some(tool_of(op, o.get("summary").unwrap_or(&Value::Null), o.get("input").unwrap_or(&json!({"type": "object"})), flag("read_only"), flag("destructive")))
            })
            .collect(),
    )
}

/// The operation a tool is, by its name (`agents_start` is `agents.start`), among `tools`.
fn op_of(tools: &[Value], tool: &str) -> Option<String> {
    tools.iter().find(|t| t.get("name").and_then(Value::as_str) == Some(tool)).and_then(|t| t.get("title")?.as_str().map(str::to_owned))
}

/// What a list of tools is, to tell when it changes.
fn fingerprint(tools: &Value) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tools.to_string().hash(&mut h);
    h.finish()
}

/// The tools as this binary knows them alone.
#[cfg(test)]
fn tools() -> Value {
    let mut t = base_tools();
    if let Some(list) = t.as_array_mut() {
        list.extend(agent_tools());
    }
    t
}

fn base_tools() -> Value {
    let schema = |tool: &str| tool_input(tool).map_or(Value::Null, |i| i.schema());
    json!([
        {
            "name": "code_run",
            "description": "Run a JavaScript program against kiln and get back only what it returns. `await kiln.machines.run({needs, command})` runs a shell command on one of this kiln's machines that has what it needs (or `on`, one by name) (this repository synced over first, as with `run`) and resolves when it ends, with {runner, exit_code, output}; start several and `await Promise.all([...])` to run them side by side. Also `kiln.machines.list({want})`, `kiln.machines.start(...)` (background: {id}), `kiln.machines.output({id, wait_s})`, `kiln.machines.stop({id})`, and kiln's CI and deployments under `kiln.ci.*` and `kiln.cd.*` (`code_search` lists every operation and its input; `kiln.call(name, input)` calls one by name). A refused call throws an Error with `result`. `input` is given as `input`. Bounded: 10 minutes unless timeout_s says otherwise (at most an hour), 16 calls at once, 4 MiB per answer; runs a program ends without are stopped.",
            "inputSchema": schema("code_run"),
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": true},
        },
        {
            "name": "code_search",
            "description": "Run a JavaScript program over kiln's operations and return what it returns. The program sees `registry` (every operation: {name, call, summary, read_only, destructive, input}) and `skills` ({name, description}). Example: `return registry.filter(o => o.name.startsWith(\"machines.\") || o.name.startsWith(\"ci.runs\")).map(o => ({call: o.call, input: o.input}))`.",
            "inputSchema": schema("code_search"),
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "runners",
            "description": "The machines (kiln runners) commands can run on, best first for `want` (this one first when it suits and has memory to spare): platform, idle cores, free memory, installed agents, and why others don't suit. `want` is what it must be and have, as `run`'s `needs` takes it (`linux/amd64`, `+gpu`, `cpus=8,mem=32g`, `tool:cargo`), or a runner's name.",
            "inputSchema": schema("runners"),
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "run",
            "description": "Run a shell command (bash) on another machine, for a step this one can't do: another platform, a GPU, more memory, disk or cores than it has, or one that has to happen on that machine (everything else, run here with your own tools). `needs` picks it: what it must be and have (`linux/amd64`, `macos/arm64`, `+gpu`, `tool:cargo`, `cpus=8,mem=32g` idle and free): this machine when it suits and has room to spare, else the idlest that suits; `on` names a machine when the step has to happen on that one. By default this repository is synced there first (git-tracked and untracked-but-not-ignored files plus .git; build output like target/ stays home) and the command runs in the matching folder. What the command leaves running when it ends (`nohup … &`, a daemon) ends with it: run a server with background: true, and `stop` it when done. Returns the exit code and output (stdout and stderr interleaved, the middle cut past 30k characters). Set background to return at once with an id for `output` and `stop`.",
            "inputSchema": schema("run"),
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": true},
        },
        {
            "name": "output",
            "description": "A background run's output since the last read, and whether it has ended (with its exit code). wait_s waits up to that long for it to end.",
            "inputSchema": schema("output"),
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        },
        {
            "name": "stop",
            "description": "Stop a background run (Ctrl-C, then terminate if it doesn't stop).",
            "inputSchema": schema("stop"),
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": false},
        },
        {
            "name": "preview_page",
            "description": "See a page as a person will, in kiln's own browser on this machine: HTML you wrote, an HTML file, or an address (a dev server here, http://localhost:5173, or the web). You get a picture of it, what its console printed (errors with their stack), how tall it is, and how it behaved for a moment after it loaded: layout shift (0: nothing moved), long tasks, frames a second. Check your work with it before you say it's done or show it. A phone by default (390×844, 2× pixels, dark); set width, height, scale and dark to see others. measure: true also gives its height at each width a viewer's conversation might have. The picture is saved at picture_path: to show it to the person, put that path in your message as a Markdown image. The first preview on a machine downloads the browser once (about 200 MB), so it can take a minute.",
            "inputSchema": schema("preview_page"),
            "annotations": {"readOnlyHint": true, "openWorldHint": true},
        },
        {
            "name": "show_page",
            "description": "Show the person a page in the conversation, live and interactive: a chart, a table, a diagram, a mock-up, a small tool to try. Write a whole HTML document (or give an HTML file). kiln keeps it with every file it uses: pictures, styles, scripts, fonts, data, by full path or next to the file; libraries from the web (pin their versions) are fetched once by the server and kept. It's shown inline at the conversation's width, as tall as it is, in kiln's look: dark, transparent background, Geist, and kiln's colors as CSS variables (--foreground, --muted-foreground, --border, --card, --accent, --success, --warning, --destructive, --chart-1 to --chart-5, --font-sans, --font-mono, --radius): use them. Make it fit any width from 320 px up (no fixed widths). It runs sandboxed: its scripts work, but it reaches nothing beyond its own files and has no cookies or storage. In it, kiln.reply(value, label) offers the person's choice back to you as their next message; kiln.save(state) keeps what they did, handed back to kiln.onrestore(state) when it's shown again. What moves (animations, video, sound) pauses while it's out of view. Look at it with preview_page first. The answer says what kiln's browser saw: errors it printed, layout shift, its height at each width.",
            "inputSchema": schema("show_page"),
            "annotations": {"readOnlyHint": false, "openWorldHint": true},
        },
        {
            "name": "show_live",
            "description": "Show the person a page live in the conversation, running here on this machine: your dev server (http://localhost:5173), a page that needs this machine's files, network or GPU, or any site. Its picture and sound stream to them while they look, at their screen's size, and what they do in it (taps, typing, scrolling) goes to it. For a page you wrote that runs on its own, use show_page instead. The answer says which stream it is. To test it in another engine, say engine: firefox (this machine's Firefox) or webkit (Safari on a Mac); to see it as a device does, say device (its window, density, touch and user agent the device's).",
            "inputSchema": schema("show_live"),
            "annotations": {"readOnlyHint": false, "openWorldHint": true},
        },
        {
            "name": "use_live",
            "description": "Use a page you showed with show_live, as the person can: click or tap at a point (CSS pixels of its window), type text, press a key by name (Enter, Tab, Escape, ArrowDown, a), scroll, go to an address, back, forward, reload, wait; its tabs: show one, open a new one, close one (a link to a new tab or a popup opens a tab of its own, shown). Its actions run in order; then you get a picture of it, its address and title, and its tabs. The person sees what you did as you do it, live, in the same page. Record it too: {\"record\": true} starts a recording (video, and a log of what's done in it by you or the person), {\"record\": false} stops it and answers with its files: the video (MP4), the log (.inputs.jsonl) and a Playwright test of what was done (.spec.ts) to turn into a real one. {\"replay\": \"<.inputs.jsonl path>\"} does a recording's log again in the page.",
            "inputSchema": schema("use_live"),
            "annotations": {"readOnlyHint": false, "openWorldHint": true},
        },
    ])
}

/// `kiln.machines.*`, as `code_search` lists them, each one's input [`machine_input`]'s.
fn machine_ops() -> Vec<Value> {
    let op = |name: &str, summary: &str, read_only: bool, destructive: bool| {
        let input = machine_input(name).map_or(Value::Null, |i| i.schema());
        json!({"name": name, "call": format!("kiln.{name}"), "summary": summary, "read_only": read_only, "destructive": destructive, "input": input})
    };
    vec![
        op("machines.list", "The machines (kiln runners) commands can run on, best first for `want`: {me, runners: [{runner: {name, os, arch, ...}, why}], passed: [...]}.", true, false),
        op(
            "machines.run",
            "Run a shell command on a machine and wait for it to end: {runner, exit_code, output (stdout and stderr interleaved; the middle cut past 1,000,000 characters), why, synced}. A command that fails still resolves, with its exit code; a machine that can't be reached rejects.",
            false,
            true,
        ),
        op("machines.start", "Start a shell command on a machine in the background: {id, runner, output so far}. Then machines.output and machines.stop (or the `output` and `stop` tools).", false, true),
        op("machines.output", "A background run's output since the last read, and whether it has ended (exit_code). wait_s waits up to that long for it to end.", true, false),
        op("machines.stop", "Stop a background run (Ctrl-C, then terminate if it doesn't stop).", false, true),
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

/// What a preview saw (on one line, first: the conversation shows that), with its picture for the
/// agent to look at (unless it's too big to send).
fn picture_result(answer: &Value, picture: Option<&str>) -> Value {
    use base64::Engine as _;
    let mut content = vec![json!({"type": "text", "text": answer.to_string()})];
    if let Some(path) = picture
        && let Ok(png) = std::fs::read(path)
        && png.len() <= 8 << 20
    {
        content.push(json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(&png), "mimeType": "image/png"}));
    }
    json!({"content": content, "structuredContent": answer, "isError": false})
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
/// registry, why there is none, which are agents' tools when it says).
type Registry = (Instant, Vec<Value>, Option<String>, Option<Vec<String>>);

struct Server {
    sock: std::path::PathBuf,
    prefer: Option<String>,
    cwd: String,
    /// This agent session: whom kiln's operations are asked for.
    session: String,
    runs: Runs,
    next: Mutex<u64>,
    registry: Mutex<Option<Registry>>,
    /// The agent tools as the server last listed them: kept while it's out of reach.
    agent_tools: Mutex<Option<Vec<Value>>>,
    /// What the agent was last given as the tools (`fingerprint`): when the server's change, it's
    /// told to ask again (`notifications/tools/list_changed`).
    listed: Mutex<Option<u64>>,
    /// Word of what it couldn't read on its way to the server (`Server::note`).
    noting: std::sync::atomic::AtomicUsize,
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
        let mut env: Vec<(String, String)> = args.get("env").and_then(Value::as_object).map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned()))).collect()).unwrap_or_default();
        // The copy of its tree over there is this agent's, for the rules that keep copies
        // (docs/reach.md §5.1).
        if sync && !self.session.is_empty() && !env.iter().any(|(k, _)| k == super::keeper::AGENT_ENV) {
            env.push((super::keeper::AGENT_ENV.to_owned(), self.session.clone()));
        }
        let req = RunReq {
            target: run_target(args, self.prefer.as_deref())?,
            command,
            cwd: Some(cwd.clone()),
            remote_cwd: if sync { None } else { s("cwd") },
            sync,
            env: super::directory::with_needs(env, s("needs").as_deref().unwrap_or_default()),
            stdin: false,
            timeout_ms: args.get("timeout_s").and_then(Value::as_f64).map(|t| (t.max(0.0) * 1000.0) as u64),
            shell: s("shell"),
            detach: false,
        };
        let mut client = Client::connect(&self.sock).map_err(|e| format!("{e:#}"))?;
        let reuse = args.get("reuse").and_then(Value::as_bool).unwrap_or(false);
        client.send(&if reuse { Req::RunReuse(Box::new(req)) } else { Req::Run(Box::new(req)) }).map_err(|e| format!("{e:#}"))?;
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

    /// `r`, a message its agent sent that it couldn't read, told the server for this session
    /// ([`Server::tell`]).
    fn note(self: &Arc<Self>, r: &crate::mcp::rpc::Refused) {
        if r.unreadable() {
            self.tell(&r.what, r.code(), &r.noted());
        }
    }

    /// A call (`what`: `tools/call run`, `code_run kiln.machines.run`) whose input doesn't fit what
    /// it takes, told the server as a message it couldn't read (`malformed`, [`Server::tell`]).
    fn misfit(self: &Arc<Self>, what: &str, m: &crate::ops::Misfit) {
        self.tell(what, crate::wire::unread::Code::Malformed, &m.noted());
    }

    /// Word of a message its agent sent that it couldn't read, told the server for this session
    /// (`crate::reach::Question::Unread`), on a thread of its own so the agent waits for nothing:
    /// at most [`NOTING_MOST`] on their way at once, and past that one isn't told (it's answered
    /// all the same).
    fn tell(self: &Arc<Self>, what: &str, code: crate::wire::unread::Code, why: &str) {
        use std::sync::atomic::Ordering;
        if self.session.is_empty() {
            return;
        }
        if self.noting.fetch_add(1, Ordering::SeqCst) >= NOTING_MOST {
            self.noting.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let (server, q) = (Arc::clone(self), json!({"op": "unread", "what": what, "code": code, "why": why}));
        std::thread::spawn(move || {
            let _ = server.ask(q);
            server.noting.fetch_sub(1, Ordering::SeqCst);
        });
    }

    /// Ask the machine's preview browser, through the runner (`crate::browser::ask`).
    fn browser(&self, q: &Value) -> anyhow::Result<Value> {
        let mut c = Client::connect(&self.sock)?;
        c.send(&Req::Browser { json: q.to_string() })?;
        match c.event()? {
            Some(Ev::Answer(j)) => Ok(serde_json::from_str(&j)?),
            Some(Ev::Error(e)) => anyhow::bail!("{e}"),
            None => anyhow::bail!("the kiln runner closed the connection (a runner older than kiln's previews doesn't have a browser: upgrade it)"),
            other => anyhow::bail!("unexpected answer {other:?}"),
        }
    }

    /// Show a page live in the conversation (the `show_live` tool): streamed from this machine by
    /// its runner, through the server, to whoever looks.
    fn show_live(&self, args: &Value) -> anyhow::Result<Value> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.trim().is_empty());
        let url = s("url").ok_or_else(|| anyhow::anyhow!("which page? Give its address"))?;
        let title = s("title").unwrap_or_else(|| url.clone());
        let opened = self.ask(json!({"op": "stream", "do": "open", "url": url, "title": title, "engine": s("engine"), "device": s("device")}))?;
        match (opened.get("ok"), opened.get("error").and_then(Value::as_str)) {
            (Some(v), _) => Ok(json!({"kiln_stream": v})),
            (None, Some(e)) if e.contains("unknown") => anyhow::bail!("this kiln server doesn't stream pages yet: upgrade it"),
            (None, e) => anyhow::bail!("{}", e.unwrap_or("kiln couldn't stream the page")),
        }
    }

    /// Show a page in the conversation (the `show_page` tool): looked at first in this machine's
    /// browser as the conversation shows it (what it loads, and its heights at each width a viewer
    /// might have, so its frame is the right size before it loads), then published by the server
    /// with every file it uses. The answer, and the picture of it.
    fn show_page(&self, args: &Value) -> anyhow::Result<(Value, Option<String>)> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.trim().is_empty());
        let title = s("title").unwrap_or_else(|| "A page".to_owned());
        // The page, and where it stands: relative references are in its folder.
        let (html, entry) = match (s("html"), s("path")) {
            (Some(html), _) => (html, format!("{}/kiln-page.html", self.cwd.trim_end_matches('/'))),
            (None, Some(path)) => {
                let full = crate::harness::files::resolve(&path, &self.cwd).ok_or_else(|| anyhow::anyhow!("{path} isn't a path on this machine"))?;
                let html = std::fs::read_to_string(&full).map_err(|e| anyhow::anyhow!("{full}: {e}"))?;
                (html, full)
            }
            _ => anyhow::bail!("which page? Give its html, or the path of an HTML file"),
        };
        let look = json!({"op": "preview", "html": html, "entry": entry, "as_shown": true, "measure": true, "picture": "window"});
        let (mut seen, picture, heights, loaded) = match self.browser(&look) {
            Ok(seen) => {
                let heights = seen.get("heights").cloned().unwrap_or(Value::Null);
                let picture = seen.get("picture_path").and_then(Value::as_str).map(str::to_owned);
                let loaded = seen.get("loaded").cloned().unwrap_or(Value::Null);
                (seen, picture, heights, loaded)
            }
            Err(e) => (json!({"error": format!("kiln's browser here couldn't look at it ({e:#}); it's shown all the same, sized as it loads")}), None, Value::Null, Value::Null),
        };
        let published = self.ask(json!({"op": "page", "do": "publish", "title": title, "entry": entry, "html": html, "loaded": loaded}))?;
        let page = match (published.get("ok"), published.get("error").and_then(Value::as_str)) {
            (Some(p), _) => p.clone(),
            (None, Some(e)) if e.contains("unknown") => anyhow::bail!("this kiln server doesn't show pages yet: upgrade it"),
            (None, e) => anyhow::bail!("{}", e.unwrap_or("kiln couldn't keep the page")),
        };
        let id = page.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
        let page = if heights.is_array() {
            match self.ask(json!({"op": "page", "do": "measured", "id": id, "heights": heights, "engines": seen.get("engines").cloned().unwrap_or(Value::Null)})) {
                Ok(a) => a.get("ok").cloned().unwrap_or(page),
                Err(_) => page,
            }
        } else {
            page
        };
        if let Some(o) = seen.as_object_mut() {
            // Said once, in the page's own entry; what it loaded is now the page's.
            for k in ["heights", "url", "loaded"] {
                o.remove(k);
            }
        }
        Ok((json!({"kiln_page": page, "seen": seen}), picture))
    }

    /// kiln's operations on the server, for this session (asked again after a minute). When the
    /// server can't be asked, the ones this kiln knows, and why they can't be called: a program
    /// calling one learns that, rather than finding `kiln.ci` undefined.
    fn remote(&self) -> (Vec<Value>, Option<String>) {
        let (reg, why, _) = self.remote_with_tools();
        (reg, why)
    }

    /// [`Self::remote`], and which of the operations are agents' tools, when the server says.
    fn remote_with_tools(&self) -> (Vec<Value>, Option<String>, Option<Vec<String>>) {
        if let Ok(g) = self.registry.lock()
            && let Some((at, reg, why, tools)) = g.as_ref()
            && at.elapsed() < REGISTRY_TTL
        {
            return (reg.clone(), why.clone(), tools.clone());
        }
        let (reg, why, tools) = match self.ask(json!({"op": "registry"})) {
            Ok(a) => match (a.get("registry").and_then(Value::as_array), a.get("error").and_then(Value::as_str)) {
                (Some(r), _) => {
                    let tools = a.get("agent_tools").and_then(Value::as_array).map(|t| t.iter().filter_map(|n| n.as_str().map(str::to_owned)).collect());
                    (r.clone(), None, tools)
                }
                (None, Some(e)) => (Vec::new(), Some(e.to_owned()), None),
                // A server from before: "unknown reach question".
                (None, None) => (Vec::new(), Some("this kiln server doesn't offer its operations to agents on runners yet: upgrade it".to_owned()), None),
            },
            Err(e) => (Vec::new(), Some(format!("{e:#}")), None),
        };
        let reg = if why.is_some() { code::registry_of(&crate::ops::listed(true)).as_array().cloned().unwrap_or_default() } else { reg };
        if let Ok(mut g) = self.registry.lock() {
            *g = Some((Instant::now(), reg.clone(), why.clone(), tools.clone()));
        }
        (reg, why, tools)
    }

    /// The tools to offer: this server's own, then kiln's operations an agent has as tools, as
    /// the kiln server lists them (those it last listed while it's out of reach; this binary's own
    /// from a server that doesn't say), so a deploy reaches the agents at work.
    fn tools(&self) -> Value {
        let (reg, why, names) = self.remote_with_tools();
        let listed = if why.is_none() { agent_tools_of(&reg, names.as_deref()) } else { None };
        let agent = match listed {
            Some(t) => {
                if let Ok(mut g) = self.agent_tools.lock() {
                    *g = Some(t.clone());
                }
                t
            }
            None => self.agent_tools.lock().ok().and_then(|g| g.clone()).unwrap_or_else(agent_tools),
        };
        let mut t = base_tools();
        if let Some(list) = t.as_array_mut() {
            list.extend(agent);
        }
        t
    }

    /// Whether it offers a tool `name` (`tools`).
    fn has(&self, name: &str) -> bool {
        self.tools().as_array().is_some_and(|t| t.iter().any(|t| t.get("name").and_then(Value::as_str) == Some(name)))
    }

    fn call(self: &Arc<Self>, name: &str, args: &Value) -> Value {
        // Arguments one of its own tools doesn't take, or of the wrong type: a tool error saying
        // where and why, as an operation's misfit is, and the server told.
        if let Some(m) = tool_input(name).and_then(|i| i.fits(args).err()) {
            self.misfit(&format!("tools/call {name}"), &m);
            return text_result(&m.answer(name), true);
        }
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
            "preview_page" => {
                let mut q = args.clone();
                if let Some(o) = q.as_object_mut() {
                    o.insert("op".into(), json!("preview"));
                }
                match self.browser(&q) {
                    Ok(seen) => picture_result(&seen, seen.get("picture_path").and_then(Value::as_str)),
                    Err(e) => text_result(&json!({"error": format!("{e:#}")}), true),
                }
            }
            "use_live" => {
                let mut q = args.clone();
                if let Some(o) = q.as_object_mut() {
                    o.insert("op".into(), json!("act"));
                    o.insert("session".into(), json!(self.session));
                }
                match self.browser(&q) {
                    Ok(done) => picture_result(&done, done.get("picture_path").and_then(Value::as_str)),
                    Err(e) => text_result(&json!({"error": format!("{e:#}")}), true),
                }
            }
            "show_live" => match self.show_live(args) {
                Ok(answer) => json!({"content": [{"type": "text", "text": answer.to_string()}], "structuredContent": answer, "isError": false}),
                Err(e) => text_result(&json!({"error": format!("{e:#}")}), true),
            },
            "show_page" => match self.show_page(args) {
                Ok((answer, picture)) => picture_result(&answer, picture.as_deref()),
                Err(e) => text_result(&json!({"error": format!("{e:#}")}), true),
            },
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
            // kiln's agents, goals and plans operations, asked of the server for this session.
            other => match op_of(self.tools().as_array().map_or(&[][..], Vec::as_slice), other) {
                Some(op) => match self.ask(json!({"op": "call", "name": op, "input": args})) {
                    Ok(a) => match a.get("ok") {
                        Some(v) => text_result(v, false),
                        None => text_result(&a, true),
                    },
                    Err(e) => text_result(&json!({"error": format!("{e:#}")}), true),
                },
                None => text_result(&json!({"error": format!("no tool {other}")}), true),
            },
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
        // An input `kiln.machines.*` doesn't take (null is none, as an operation's is): refused as
        // an operation's misfit is in code mode, saying where and why, and the server told.
        if let Some(wants) = machine_input(name)
            && let Err(m) = wants.fits(&if input.is_null() { json!({}) } else { input.clone() })
        {
            self.server.misfit(&format!("code_run kiln.{name}"), &m);
            let mut a = m.answer(name);
            if let Some(o) = a.as_object_mut() {
                o.insert("status".into(), json!(http::StatusCode::BAD_REQUEST.as_u16()));
            }
            return Err(a);
        }
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

/// `kiln reach mcp`: serve MCP on stdin/stdout until stdin closes. Tool calls run side by side.
pub fn serve() -> anyhow::Result<()> {
    let sock = super::local::find_socket()?;
    let prefer = std::env::var("KILN_REACH_ON").ok().filter(|s| !s.trim().is_empty());
    let session = std::env::var("KILN_SESSION").unwrap_or_default();
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let machines = machines(&sock);
    let server = Arc::new(Server { sock, prefer, cwd, session, runs: Arc::default(), next: Mutex::new(0), registry: Mutex::new(None), agent_tools: Mutex::new(None), listed: Mutex::new(None), noting: Default::default() });
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let say = |out: &Arc<Mutex<std::io::Stdout>>, v: &Value| {
        if let Ok(mut o) = out.lock() {
            let _ = writeln!(o, "{v}");
            let _ = o.flush();
        }
    };
    // The tools the server offers change (a deploy): the agent is told to ask for them again,
    // once it has asked for them at all.
    {
        let (server, out) = (Arc::clone(&server), Arc::clone(&out));
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(REGISTRY_TTL);
                let Some(before) = server.listed.lock().ok().and_then(|g| *g) else { continue };
                let now = fingerprint(&server.tools());
                if now != before {
                    if let Ok(mut g) = server.listed.lock() {
                        *g = Some(now);
                    }
                    say(&out, &json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
                }
            }
        });
    }
    let mut stdin = std::io::stdin().lock();
    while let Ok(Some(line)) = next_line(&mut stdin) {
        // A line past what a message may be is refused unread, and noted.
        let Ok(line) = line else {
            let r = Box::new(crate::mcp::rpc::too_large());
            server.note(&r);
            say(&out, &r.answer());
            continue;
        };
        if line.trim_ascii().is_empty() {
            continue;
        }
        // Each message read into a request, or refused as JSON-RPC 2.0 says and noted (`rpc`). A
        // batch (MCP before 2025-06-18) is answered whole, each message in turn; a message alone is
        // answered as it's done, a tool's call on a thread of its own.
        let body = crate::mcp::rpc::body(&line);
        let lone = (!body.batch).then_some(&out);
        let answers: Vec<Value> = body.messages.into_iter().filter_map(|m| answer(&server, &machines, m, lone)).collect();
        if let Some(a) = crate::mcp::rpc::reply(body.batch, answers) {
            say(&out, &a);
        }
    }
    Ok(())
}

/// The next line of `input`, without its newline: none at its end; `Err` for one past what a message
/// may be (`rpc::BODY_MOST`), the rest of it read and let go.
fn next_line(input: &mut impl BufRead) -> std::io::Result<Option<Result<Vec<u8>, ()>>> {
    let (mut line, mut over, mut any) = (Vec::new(), false, false);
    loop {
        let buf = input.fill_buf()?;
        if buf.is_empty() {
            return Ok(any.then_some(if over { Err(()) } else { Ok(line) }));
        }
        any = true;
        let (took, ended) = match buf.iter().position(|b| *b == b'\n') {
            Some(at) => (at + 1, Some(at)),
            None => (buf.len(), None),
        };
        if !over {
            line.extend_from_slice(buf.get(..ended.unwrap_or(took)).unwrap_or_default());
            if line.len() > crate::mcp::rpc::BODY_MOST {
                over = true;
                line = Vec::new();
            }
        }
        input.consume(took);
        if ended.is_some() {
            return Ok(Some(if over { Err(()) } else { Ok(line) }));
        }
    }
}

/// One message's answer: none for a notification, nor for a tool's call when `out` is given (its
/// answer goes there once it's done).
fn answer(server: &Arc<Server>, machines: &str, m: Result<crate::mcp::rpc::Request, Box<crate::mcp::rpc::Refused>>, out: Option<&Arc<Mutex<std::io::Stdout>>>) -> Option<Value> {
    use crate::mcp::rpc;
    let refused = |r: Box<rpc::Refused>| {
        server.note(&r);
        Some(r.answer())
    };
    let req = match m {
        Ok(req) => req,
        Err(r) => return refused(r),
    };
    // A notification is never answered; one no client sends is noted all the same.
    let Some(id) = req.id.clone() else {
        if let Some(r) = req.unknown_notification() {
            server.note(&r);
        }
        return None;
    };
    // A list's cursor is a string (its lists come whole).
    if matches!(req.method.as_str(), "tools/list" | "resources/list" | "resources/templates/list" | "prompts/list")
        && let Err(r) = req.params::<rpc::Paginated>()
    {
        return refused(r);
    }
    Some(match req.method.as_str() {
        "initialize" => {
            let p: rpc::Initialize = match req.params() {
                Ok(p) => p,
                Err(r) => return refused(r),
            };
            let version = p.protocol_version.unwrap_or_else(|| "2025-06-18".to_owned());
            rpc_ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": true}, "resources": {"listChanged": false}},
                    "serverInfo": {"name": "kiln", "title": "kiln", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": instructions(server.prefer.as_deref(), machines, &server.session),
                }),
            )
        }
        "ping" => rpc_ok(id, json!({})),
        "tools/list" => {
            let t = server.tools();
            if let Ok(mut g) = server.listed.lock() {
                *g = Some(fingerprint(&t));
            }
            rpc_ok(id, json!({"tools": t}))
        }
        "tools/call" => {
            let p: rpc::ToolsCall = match req.params() {
                Ok(p) => p,
                Err(r) => return refused(r),
            };
            // A tool it doesn't have is a protocol error (MCP 2025-11-25), not the tool's: looked
            // for where the call runs, as listing the tools may ask the server.
            let unknown = req.unknown_tool(&p.name);
            let call = move |server: &Arc<Server>| {
                if !server.has(&p.name) {
                    server.note(&unknown);
                    return unknown.answer();
                }
                rpc_ok(id, server.call(&p.name, &Value::Object(p.arguments.unwrap_or_default())))
            };
            let Some(out) = out else { return Some(call(server)) };
            let (server, out) = (Arc::clone(server), Arc::clone(out));
            std::thread::spawn(move || {
                let v = call(&server);
                if let Ok(mut o) = out.lock() {
                    let _ = writeln!(o, "{v}");
                    let _ = o.flush();
                }
            });
            return None;
        }
        // kiln's skills (deploying, diagnosing): resources, as the server offers them.
        "resources/list" => rpc_ok(id, json!({"resources": crate::mcp::skills::resources()})),
        "resources/read" => {
            let p: rpc::ResourcesRead = match req.params() {
                Ok(p) => p,
                Err(r) => return refused(r),
            };
            match crate::mcp::skills::read(&p.uri) {
                Some(c) => rpc_ok(id, json!({"contents": [c]})),
                None => rpc::Refused::new(id, -32002, format!("no resource {}", p.uri)).answer(),
            }
        }
        "resources/templates/list" => rpc_ok(id, json!({"resourceTemplates": []})),
        "prompts/list" => rpc_ok(id, json!({"prompts": []})),
        _ => return refused(Box::new(req.unknown_method())),
    })
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

/// The same for Antigravity, which reads MCP servers only from its own config
/// (`~/.gemini/config/mcp_config.json`; no flag or session setting adds one): `agy mcp add`'s
/// arguments putting this server there, the same for every session. Which session it serves is
/// in agy's environment (`agy_env`), which agy hands on to the servers it starts.
pub fn agy_add(kiln: &std::path::Path) -> Vec<String> {
    ["mcp", "add", "kiln"].map(str::to_owned).into_iter().chain([kiln.to_string_lossy().into_owned(), "reach".into(), "mcp".into()]).collect()
}

/// The environment a session's agy is started with, for this server (`agy_add`).
pub fn agy_env(sock: &str, prefer: Option<&str>, session: &str) -> Vec<(&'static str, String)> {
    env_of(sock, prefer, session)
}

/// Whether agy's MCP config (`config`, its `mcp_config.json`) has this server as `agy_add` puts
/// it, or has it turned off (`agy mcp disable kiln`: a person's choice, kept).
pub fn agy_has(config: &Value, kiln: &std::path::Path) -> bool {
    let Some(s) = config.pointer("/mcpServers/kiln") else { return false };
    s.get("disabled").and_then(Value::as_bool) == Some(true)
        || (s.get("command").and_then(Value::as_str) == Some(kiln.to_string_lossy().as_ref()) && s.get("args") == Some(&json!(["reach", "mcp"])))
}

/// The tools of this server Antigravity calls without asking (it can't ask once started): showing
/// and previewing a page. Its others are agy's to decide on.
pub const AGY_ALLOWED: [&str; 2] = ["mcp(kiln/show_page)", "mcp(kiln/preview_page)"];

/// agy's settings (`settings`, its `~/.gemini/antigravity-cli/settings.json`) with `AGY_ALLOWED` in
/// `permissions.allow`, each unless a rule of a person's covers it already (allowed, asked or
/// denied, the tool or all of this server's or every server's: agy's deny > ask > allow then
/// decides). None when nothing needs adding, or the settings aren't as agy writes them.
pub fn agy_allow(settings: &Value) -> Option<Value> {
    let covers = |rule: &str| {
        let rules = ["allow", "deny", "ask"].into_iter().filter_map(|k| settings.pointer(&format!("/permissions/{k}")).and_then(Value::as_array)).flatten();
        rules.filter_map(Value::as_str).any(|r| r == rule || r == "mcp(kiln/*)" || r == "mcp(*)")
    };
    let missing: Vec<&str> = AGY_ALLOWED.into_iter().filter(|r| !covers(r)).collect();
    if missing.is_empty() {
        return None;
    }
    let mut out = settings.clone();
    let permissions = out.as_object_mut()?.entry("permissions").or_insert_with(|| json!({}));
    let allow = permissions.as_object_mut()?.entry("allow").or_insert_with(|| json!([]));
    allow.as_array_mut()?.extend(missing.into_iter().map(Value::from));
    Some(out)
}

/// The devices `show_live` lays a page out as, by name (`crate::browser::devices`).
fn devices() -> Vec<&'static str> {
    crate::browser::devices::DEVICES.iter().map(|d| d.name).collect()
}

/// What this server's own tools take ([`tool_input`]), and code mode's `kiln.machines.*` here
/// ([`machine_input`]). `code_search` takes `crate::mcp::input::CodeSearch`, as the server's does.
pub mod input {
    use std::collections::BTreeMap;

    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CodeRun {
        /// The body of an async function; `return` its answer.
        pub program: String,
        /// Any JSON, available as `input`.
        pub input: Option<serde_json::Value>,
        /// How long the program may run (default 600, at most 3600).
        pub timeout_s: Option<f64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Runners {
        /// What the machine must be (optional).
        pub want: Option<String>,
    }

    /// A command, as `run` takes it: [`Command`]'s, and how long to wait for it.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Run {
        /// Shell text, run by bash in the synced copy of this repository.
        pub command: String,
        /// What the machine must be and have: `linux/amd64`, `macos`, `+gpu` (a label),
        /// `tool:cargo`, `cpus>=16` (cores in all), and the room the command asks for, idle and
        /// free there now: `cpus=8`, `mem=32g`, `disk=50g`, `gpus=1`; `exclusive`: no CI work on
        /// it. Picks this machine when it has them and room to spare, else the idlest that has
        /// them.
        pub needs: Option<String>,
        /// A runner by name, for a step that has to happen on that machine (it must still meet
        /// `needs`). Default: what `needs` picks; with neither, the session's preferred runner,
        /// else this machine.
        pub on: Option<String>,
        /// Copy this repository over first and run in it (default: true inside a git repository).
        pub sync: Option<bool>,
        /// With sync: the folder here whose repository is synced and whose counterpart the command
        /// runs in (default: the session's folder). Without sync: the folder on the other machine
        /// (default: its home).
        pub cwd: Option<String>,
        /// Stop the command after this many seconds.
        pub timeout_s: Option<f64>,
        /// A named shell over there: its folder and exported variables carry over between runs
        /// that name it.
        pub shell: Option<String>,
        /// Environment for the command.
        pub env: Option<BTreeMap<String, String>>,
        /// Give back the result of an identical earlier run instead of running again (the same
        /// command, folder, environment and machine, over an identical tree; only successes are
        /// kept).
        pub reuse: Option<bool>,
        /// Return after this many seconds even if it is still running (it keeps running in the
        /// background; default 600).
        pub wait_s: Option<f64>,
        /// Return at once with an id; read with `output`, end with `stop`.
        pub background: Option<bool>,
    }

    /// A command, as `kiln.machines.run` and `kiln.machines.start` take it.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Command {
        /// Shell text, run by bash in the synced copy of this repository.
        pub command: String,
        /// What the machine must be and have: `linux/amd64`, `macos`, `+gpu` (a label),
        /// `tool:cargo`, `cpus>=16` (cores in all), and the room the command asks for, idle and
        /// free there now: `cpus=8`, `mem=32g`, `disk=50g`, `gpus=1`; `exclusive`: no CI work on
        /// it. Picks this machine when it has them and room to spare, else the idlest that has
        /// them.
        pub needs: Option<String>,
        /// A runner by name, for a step that has to happen on that machine (it must still meet
        /// `needs`). Default: what `needs` picks; with neither, the session's preferred runner,
        /// else this machine.
        pub on: Option<String>,
        /// Copy this repository over first and run in it (default: true inside a git repository).
        pub sync: Option<bool>,
        /// With sync: the folder here whose repository is synced and whose counterpart the command
        /// runs in (default: the session's folder). Without sync: the folder on the other machine
        /// (default: its home).
        pub cwd: Option<String>,
        /// Stop the command after this many seconds.
        pub timeout_s: Option<f64>,
        /// A named shell over there: its folder and exported variables carry over between runs
        /// that name it.
        pub shell: Option<String>,
        /// Environment for the command.
        pub env: Option<BTreeMap<String, String>>,
        /// Give back the result of an identical earlier run instead of running again (the same
        /// command, folder, environment and machine, over an identical tree; only successes are
        /// kept).
        pub reuse: Option<bool>,
    }

    /// A background run's output (`output`, `kiln.machines.output`).
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Output {
        pub id: String,
        pub wait_s: Option<f64>,
    }

    /// A background run stopped (`stop`, `kiln.machines.stop`).
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Stop {
        pub id: String,
    }

    // The window, the whole page, or none. (Not a doc comment: it would be the description of
    // each field of this type.)
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum PreviewPagePicture {
        Window,
        Page,
        None,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct PreviewPage {
        /// A whole HTML document you wrote. Full paths in it (src="/tmp/shot.png") load those
        /// files.
        pub html: Option<String>,
        /// An HTML file on this machine (or a picture, a PDF).
        pub path: Option<String>,
        /// An address: a dev server on this machine or a page on the web.
        pub url: Option<String>,
        /// The window's width in CSS pixels: 390 (a phone) by default, 1280 for a desktop.
        pub width: Option<f64>,
        /// The window's height: 844 by default.
        pub height: Option<f64>,
        /// Pixel density: 2 by default.
        pub scale: Option<f64>,
        /// A dark appearance (prefers-color-scheme): true by default.
        pub dark: Option<bool>,
        /// prefers-reduced-motion: reduce.
        pub reduced_motion: Option<bool>,
        /// How long to watch it after it loads, in ms: 1500 by default, at most 10000.
        pub watch_ms: Option<f64>,
        /// Also its height at each width a viewer's conversation might have.
        pub measure: Option<bool>,
        /// The window (the default), the whole page, or none.
        pub picture: Option<PreviewPagePicture>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ShowPage {
        /// A few words for it: shown with it, and its name.
        pub title: String,
        /// A whole HTML document.
        pub html: Option<String>,
        /// Or an HTML file on this machine (files it uses are found next to it).
        pub path: Option<String>,
    }

    // Chromium, Firefox or WebKit. (Not a doc comment: it would be the description of each field
    // of this type.)
    /// Read as a live page's stream reads it (`crate::browser::driven::Engine::parse`): in any case,
    /// and `chrome`, `gecko` and `safari` for chromium, firefox and webkit, though its schema lists
    /// the three.
    #[derive(Debug, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum ShowLiveEngine {
        Chromium,
        Firefox,
        Webkit,
    }

    impl<'de> Deserialize<'de> for ShowLiveEngine {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            use crate::browser::driven::Engine;
            let s = String::deserialize(d)?;
            match Engine::parse(&s) {
                Ok(Engine::Chromium) => Ok(Self::Chromium),
                Ok(Engine::Firefox) => Ok(Self::Firefox),
                Ok(Engine::Webkit) => Ok(Self::Webkit),
                Err(_) => Err(serde::de::Error::unknown_variant(&s, &["chromium", "firefox", "webkit"])),
            }
        }
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ShowLive {
        /// Its address: on this machine (http://localhost:3000) or on the web.
        pub url: String,
        /// A few words for it.
        pub title: Option<String>,
        /// The engine it runs in (chromium by default).
        pub engine: Option<ShowLiveEngine>,
        /// A device it's laid out as (by default, as the person's screen is).
        #[schemars(extend("enum" = super::devices()))]
        pub device: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct UseLive {
        /// The stream's id, from show_live's answer.
        pub stream: String,
        /// In order: {"click": [x, y]}, {"tap": [x, y]}, {"type": "text"}, {"key": "Enter"},
        /// {"scroll": [dx, dy]}, {"go": "http://localhost:3000/"}, {"back": true}, {"forward":
        /// true}, {"reload": true}, {"tab": {"show": "<tab id>"}}, {"tab": {"new":
        /// "https://example.com/"}}, {"tab": {"close": "<tab id>"}}, {"wait_ms": 500}, {"record":
        /// true}, {"record": false}, {"replay": "/path/live-….inputs.jsonl"}.
        pub actions: Option<Vec<serde_json::Map<String, serde_json::Value>>>,
        /// A picture of it after (true by default).
        pub picture: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct MachinesList {
        /// What the machine must be and have (optional), as `needs` says it: `linux/amd64`,
        /// `macos`, `+gpu`, `cpus=8,mem=32g`, `tool:cargo`; or a runner's name.
        pub want: Option<String>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Antigravity reads MCP servers only from its own config: kiln's is put there once for every
    /// session (`agy mcp add`), whom it serves in agy's environment; again only when it names
    /// another kiln, and one a person turned off stays off.
    #[test]
    fn antigravity_gets_the_server_through_its_own_config() {
        let kiln = std::path::Path::new("/opt/kiln");
        assert_eq!(agy_add(kiln), ["mcp", "add", "kiln", "/opt/kiln", "reach", "mcp"]);
        assert_eq!(agy_env("/run/x.sock", Some("macos"), "s1"), [("KILN_REACH_SOCK", "/run/x.sock".to_owned()), ("KILN_REACH_ON", "macos".to_owned()), ("KILN_SESSION", "s1".to_owned())]);
        // As `agy mcp add` writes it (agy 1.4.2), beside a person's own.
        let put = json!({"mcpServers": {"kiln": {"args": ["reach", "mcp"], "command": "/opt/kiln", "disabled": false}, "github": {"command": "gh-mcp", "disabled": false}}});
        assert!(agy_has(&put, kiln));
        assert!(!agy_has(&put, std::path::Path::new("/opt/kiln-2")), "a runner upgraded since puts its own");
        assert!(!agy_has(&json!({}), kiln));
        assert!(!agy_has(&json!({"mcpServers": {}}), kiln));
        let off = json!({"mcpServers": {"kiln": {"args": ["reach", "mcp"], "command": "/opt/old", "disabled": true}}});
        assert!(agy_has(&off, kiln), "turned off by a person: kept");
    }

    /// Its page tools are allowed in agy's settings beside a person's own rules, unless a rule of
    /// theirs covers them (agy's deny > ask > allow decides then).
    #[test]
    fn antigravity_allows_its_page_tools_unless_a_person_said_otherwise() {
        let allow = |s: &Value| agy_allow(s).and_then(|s| s.pointer("/permissions/allow").cloned());
        // As agy keeps its settings, without rules: both added, the rest kept; once.
        let agys = json!({"model": "gemini-3.8-flash-low", "trustedWorkspaces": ["/w"]});
        let added = agy_allow(&agys);
        assert_eq!(added.as_ref().and_then(|s| s.pointer("/permissions/allow")), Some(&json!(AGY_ALLOWED)));
        assert_eq!(added.as_ref().and_then(|s| s.get("trustedWorkspaces")), Some(&json!(["/w"])));
        assert_eq!(added.as_ref().and_then(agy_allow), None);
        // A person's own rules stay, beside them.
        let theirs = json!({"permissions": {"allow": ["command(git)"], "deny": ["command(rm -rf)"]}});
        assert_eq!(allow(&theirs), Some(json!(["command(git)", "mcp(kiln/show_page)", "mcp(kiln/preview_page)"])));
        // One a person asks for, or this server or every server they deny or ask for, stays theirs.
        assert_eq!(allow(&json!({"permissions": {"ask": ["mcp(kiln/show_page)"]}})), Some(json!(["mcp(kiln/preview_page)"])));
        assert_eq!(agy_allow(&json!({"permissions": {"deny": ["mcp(kiln/*)"]}})), None);
        assert_eq!(agy_allow(&json!({"permissions": {"ask": ["mcp(*)"]}})), None);
        // Settings not as agy writes them are left alone.
        assert_eq!(agy_allow(&json!(["x"])), None);
        assert_eq!(agy_allow(&json!({"permissions": {"allow": "mcp(*)"}})), None);
    }

    /// What the agent sends its runner's MCP server is read as JSON-RPC 2.0 and MCP say: what
    /// isn't JSON or a request is refused (each message of a batch on its own, under a null id when
    /// its id can't be read), a method or a tool the server doesn't have too, and a valid
    /// notification isn't answered.
    #[test]
    fn what_the_agent_sends_is_read_as_json_rpc_says() {
        let server = Arc::new(Server { sock: "/nonexistent/kiln-reach.sock".into(), prefer: None, cwd: String::new(), session: String::new(), runs: Arc::default(), next: Mutex::new(0), registry: Mutex::new(None), agent_tools: Mutex::new(None), listed: Mutex::new(None), noting: Default::default() });
        let ask = |line: &str| {
            let body = crate::mcp::rpc::body(line.as_bytes());
            let answers = body.messages.into_iter().filter_map(|m| answer(&server, "", m, None)).collect();
            crate::mcp::rpc::reply(body.batch, answers)
        };
        let code = |v: Option<Value>| v.and_then(|v| v.pointer("/error/code").and_then(Value::as_i64));
        assert_eq!(code(ask("{")), Some(-32700));
        assert_eq!(code(ask("{}")), Some(-32600));
        let three = ask("[1,2,3]");
        let each: Option<Vec<(Option<Value>, Option<Value>)>> = three.as_ref().and_then(Value::as_array).map(|a| a.iter().map(|e| (e.pointer("/error/code").cloned(), e.get("id").cloned())).collect());
        assert_eq!(each, Some(vec![(Some(json!(-32600)), Some(Value::Null)); 3]), "{three:?}");
        assert_eq!(code(ask(r#"{"jsonrpc":"2.0","id":1}"#)), Some(-32600));
        assert_eq!(code(ask(r#"{"jsonrpc":"2.0","id":2,"method":"teleport"}"#)), Some(-32601));
        assert_eq!(code(ask(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"teleport","arguments":{}}}"#)), Some(-32602));
        assert_eq!(ask(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
        assert_eq!(ask(r#"{"jsonrpc":"2.0","method":"notifications/teleport"}"#), None, "no notification is answered, one no client sends neither");
        assert_eq!(code(ask(r#"{"jsonrpc":"2.0","id":5,"method":"tools/list","params":{"cursor":5}}"#)), Some(-32602), "a cursor is a string");
        assert_eq!(ask(r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#), Some(json!({"jsonrpc": "2.0", "id": 4, "result": {}})));
    }

    /// The agent tools are the server's: those it names (each as its registry has it, one it
    /// added since this binary was built included), and this binary's own from a server that
    /// doesn't say; the list's fingerprint tells a change.
    #[test]
    fn agent_tools_are_the_ones_the_server_names() {
        let registry = vec![
            json!({"name": "goals.add", "summary": "Line goals up.", "read_only": false, "destructive": false, "input": {"type": "object", "properties": {"goals": {"type": "array"}}}}),
            json!({"name": "goals.later", "summary": "A goal for later (new on the server).", "read_only": false, "destructive": false, "input": {"type": "object"}}),
            json!({"name": "ci.runs.list", "summary": "Runs.", "read_only": true, "destructive": false, "input": {"type": "object"}}),
        ];
        let names = vec!["goals.add".to_owned(), "goals.later".to_owned(), "goals.gone".to_owned()];
        let tools = agent_tools_of(&registry, Some(&names)).unwrap_or_default();
        let listed: Vec<&str> = tools.iter().filter_map(|t| t.get("name").and_then(Value::as_str)).collect();
        assert_eq!(listed, ["goals_add", "goals_later"], "named and in the registry; code mode's ci.runs.list isn't a tool");
        assert_eq!(tools.get(1).and_then(|t| t.get("description")), Some(&json!("A goal for later (new on the server).")));
        assert_eq!(op_of(&tools, "goals_later").as_deref(), Some("goals.later"));
        assert!(agent_tools_of(&registry, None).is_none(), "a server that doesn't say: this binary's own");
        let before = fingerprint(&json!(tools));
        let fewer = agent_tools_of(&registry, names.get(..1)).unwrap_or_default();
        assert_ne!(before, fingerprint(&json!(fewer)));
        assert_eq!(before, fingerprint(&json!(agent_tools_of(&registry, Some(&names)).unwrap_or_default())));
    }

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
        assert_eq!(names.get(..6), Some(&["code_run", "code_search", "runners", "run", "output", "stop"][..]));
        assert!(names.contains(&"agents_start") && names.contains(&"agents_wait") && names.contains(&"agents_result"), "agents' tools too: {names:?}");
        assert_eq!(names.get(6..10), Some(&["preview_page", "show_page", "show_live", "use_live"][..]), "then pages: {names:?}");
        // show_live offers the devices and engines kiln knows.
        let live = t.as_array().into_iter().flatten().find(|t| t.get("name") == Some(&json!("show_live"))).cloned().unwrap_or_default();
        let devices: Vec<&str> = crate::browser::devices::DEVICES.iter().map(|d| d.name).collect();
        assert_eq!(live.pointer("/inputSchema/properties/device/enum"), Some(&json!(devices)));
        assert_eq!(live.pointer("/inputSchema/properties/engine/enum"), Some(&json!(["chromium", "firefox", "webkit"])));
        let list = t.as_array().map_or(&[][..], Vec::as_slice);
        let agent_op = |tool: &str| op_of(list, tool);
        assert_eq!(agent_op("agents_wait").as_deref(), Some("agents.wait"));
        assert_eq!(agent_op("run"), None);
        // Its own goal stack and plans are tools too, the agent's own without naming it.
        for (tool, op) in [
            ("goals_add", "goals.add"),
            ("goals_get", "goals.get"),
            ("goals_control", "goals.control"),
            ("plans_create", "plans.create"),
            ("plans_run", "plans.run"),
            ("plans_runs_get", "plans.runs.get"),
        ] {
            assert!(names.contains(&tool), "{tool}: {names:?}");
            assert_eq!(agent_op(tool).as_deref(), Some(op));
        }
        assert_eq!(agent_op("plans_delete"), None, "deleting a plan is code mode's");
        let add = t.as_array().into_iter().flatten().find(|t| t.get("name") == Some(&json!("goals_add"))).cloned().unwrap_or_default();
        assert_eq!(add.pointer("/inputSchema/required"), Some(&json!(["goals"])), "an agent's own goals need no id");
        assert!(instructions(Some("adacachy"), "", "").contains("prefers commands to run on adacachy"));
        let i = instructions(None, "", "");
        assert!(i.contains("run it there yourself") && i.contains("don't hand the person a command to type") && i.contains("code_run"));
        assert!(i.contains("Everything else runs on this machine") && !i.contains("build or test there"), "{i}");
        assert!(i.contains("goals_add") && i.contains("plans_run") && !i.contains("You are kiln agent"), "{i}");
        assert!(instructions(None, "", "01a14ab9").contains("You are kiln agent 01a14ab9"), "an agent is told who it is");
        let v = json!({"me": "adacachy", "runners": [{"runner": {"name": "adacachy", "os": "linux", "arch": "x86_64"}}, {"runner": {"name": "mac", "os": "macos", "arch": "aarch64"}}], "passed": [{"runner": {"name": "box", "os": "windows", "arch": "x86_64"}}]});
        assert_eq!(describe(&v), "adacachy (linux/x86_64, this machine), mac (macos/aarch64), box (windows/x86_64)");
        assert!(instructions(None, &describe(&v), "").contains("This kiln's machines: adacachy (linux/x86_64, this machine), mac (macos/aarch64)"));
        Ok(())
    }

    #[test]
    fn a_program_sees_the_machines_and_the_servers_operations() {
        let ops = machine_ops();
        let names: Vec<&str> = ops.iter().filter_map(|o| o.get("name").and_then(Value::as_str)).collect();
        assert_eq!(names, ["machines.list", "machines.run", "machines.start", "machines.output", "machines.stop"]);
        assert!(ops.iter().all(|o| o.pointer("/input/type") == Some(&json!("object"))));
    }

    /// `run` and `kiln.machines.run` take `needs` beside `on`: the runner gets both as one spelling,
    /// the session's preferred runner only when neither is given, and a bad need is refused here.
    #[test]
    fn run_takes_needs_beside_on() {
        let t = |args: Value| run_target(&args, Some("mac"));
        assert_eq!(t(json!({"command": "make", "needs": "cpus=8,mem=32g,linux/amd64"})).as_deref(), Ok("cpus=8,mem=32g,linux/amd64"));
        assert_eq!(t(json!({"command": "make", "on": "adacachy", "needs": "mem=32g"})).as_deref(), Ok("adacachy,mem=32g"));
        assert_eq!(t(json!({"command": "make", "on": "linux/amd64"})).as_deref(), Ok("linux/amd64"));
        assert_eq!(t(json!({"command": "make", "needs": " "})).as_deref(), Ok("mac"));
        assert_eq!(run_target(&json!({"command": "make"}), None).as_deref(), Ok(""));
        assert_eq!(t(json!({"command": "make", "needs": "memm=4g"})), Err("`needs`: memm=4g: no such need".to_owned()));
        assert_eq!(tool_input("run").map(|i| i.schema()).and_then(|s| s.pointer("/properties/needs/type").cloned()), Some(json!("string")));
        let runs: Vec<Value> = machine_ops().into_iter().filter(|o| o.get("name").and_then(Value::as_str).is_some_and(|n| n == "machines.run" || n == "machines.start")).collect();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|o| o.pointer("/input/properties/needs").is_some()), "{runs:?}");
    }

    /// A line past what a message may be is refused whole and the rest of it let go; the next is
    /// read as it came.
    #[test]
    fn a_line_past_what_a_message_may_be_is_let_go_and_the_next_read() -> anyhow::Result<()> {
        let mut input = b"{\"a\":1}\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', crate::mcp::rpc::BODY_MOST + 1));
        input.extend_from_slice(b"\n{\"b\":2}");
        let mut r = std::io::BufReader::new(input.as_slice());
        assert_eq!(next_line(&mut r)?, Some(Ok(b"{\"a\":1}".to_vec())));
        assert_eq!(next_line(&mut r)?, Some(Err(())));
        assert_eq!(next_line(&mut r)?, Some(Ok(b"{\"b\":2}".to_vec())));
        assert_eq!(next_line(&mut r)?, None);
        Ok(())
    }

    /// A server of its own on `sock`, for session `session` (none: it tells the kiln server
    /// nothing).
    fn server_on(sock: &str, session: &str) -> Arc<Server> {
        Arc::new(Server { sock: sock.into(), prefer: None, cwd: String::new(), session: session.into(), runs: Arc::default(), next: Mutex::new(0), registry: Mutex::new(None), agent_tools: Mutex::new(None), listed: Mutex::new(None), noting: Default::default() })
    }

    /// Each of its own tools and of `kiln.machines.*`, with the schema it publishes: the tool's
    /// `inputSchema`, the operation's `input` (`code_search`'s registry); whether it's a tool.
    fn published() -> Vec<(String, Value, bool)> {
        let tools = base_tools().as_array().cloned().unwrap_or_default();
        let tools = tools.into_iter().filter_map(|t| Some((t.get("name")?.as_str()?.to_owned(), t.get("inputSchema")?.clone(), true)));
        let machines = machine_ops().into_iter().filter_map(|o| Some((o.get("name")?.as_str()?.to_owned(), o.get("input")?.clone(), false)));
        tools.chain(machines).collect()
    }

    /// What each of its own tools and of `kiln.machines.*` published as its input before it was
    /// typed, as it was written by hand.
    fn published_before() -> Vec<(&'static str, Value)> {
        let command = |waits: bool| {
            let mut props = json!({
                "command": {"type": "string", "description": "Shell text, run by bash in the synced copy of this repository."},
                "needs": {"type": "string", "description": "What the machine must be and have: `linux/amd64`, `macos`, `+gpu` (a label), `tool:cargo`, `cpus>=16` (cores in all), and the room the command asks for, idle and free there now: `cpus=8`, `mem=32g`, `disk=50g`, `gpus=1`; `exclusive`: no CI work on it. Picks this machine when it has them and room to spare, else the idlest that has them."},
                "on": {"type": "string", "description": "A runner by name, for a step that has to happen on that machine (it must still meet `needs`). Default: what `needs` picks; with neither, the session's preferred runner, else this machine."},
                "sync": {"type": "boolean", "description": "Copy this repository over first and run in it (default: true inside a git repository)."},
                "cwd": {"type": "string", "description": "With sync: the folder here whose repository is synced and whose counterpart the command runs in (default: the session's folder). Without sync: the folder on the other machine (default: its home)."},
                "timeout_s": {"type": "number", "description": "Stop the command after this many seconds."},
                "shell": {"type": "string", "description": "A named shell over there: its folder and exported variables carry over between runs that name it."},
                "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Environment for the command."},
                "reuse": {"type": "boolean", "description": "Give back the result of an identical earlier run instead of running again (the same command, folder, environment and machine, over an identical tree; only successes are kept)."}
            });
            if waits && let Some(p) = props.as_object_mut() {
                p.insert("wait_s".into(), json!({"type": "number", "description": "Return after this many seconds even if it is still running (it keeps running in the background; default 600)."}));
                p.insert("background".into(), json!({"type": "boolean", "description": "Return at once with an id; read with `output`, end with `stop`."}));
            }
            json!({"type": "object", "properties": props, "required": ["command"]})
        };
        vec![
            ("code_run", json!({"type": "object", "properties": {
                "program": {"type": "string", "description": "The body of an async function; `return` its answer."},
                "input": {"description": "Any JSON, available as `input`."},
                "timeout_s": {"type": "number", "description": "How long the program may run (default 600, at most 3600)."}
            }, "required": ["program"]})),
            ("code_search", json!({"type": "object", "properties": {"program": {"type": "string", "description": "The body of an async function; `return` its answer."}}, "required": ["program"]})),
            ("runners", json!({"type": "object", "properties": {"want": {"type": "string", "description": "What the machine must be (optional)."}}})),
            ("run", command(true)),
            ("output", json!({"type": "object", "properties": {"id": {"type": "string"}, "wait_s": {"type": "number"}}, "required": ["id"]})),
            ("stop", json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]})),
            ("preview_page", json!({"type": "object", "properties": {
                "html": {"type": "string", "description": "A whole HTML document you wrote. Full paths in it (src=\"/tmp/shot.png\") load those files."},
                "path": {"type": "string", "description": "An HTML file on this machine (or a picture, a PDF)."},
                "url": {"type": "string", "description": "An address: a dev server on this machine or a page on the web."},
                "width": {"type": "number", "description": "The window's width in CSS pixels: 390 (a phone) by default, 1280 for a desktop."},
                "height": {"type": "number", "description": "The window's height: 844 by default."},
                "scale": {"type": "number", "description": "Pixel density: 2 by default."},
                "dark": {"type": "boolean", "description": "A dark appearance (prefers-color-scheme): true by default."},
                "reduced_motion": {"type": "boolean", "description": "prefers-reduced-motion: reduce."},
                "watch_ms": {"type": "number", "description": "How long to watch it after it loads, in ms: 1500 by default, at most 10000."},
                "measure": {"type": "boolean", "description": "Also its height at each width a viewer's conversation might have."},
                "picture": {"type": "string", "enum": ["window", "page", "none"], "description": "The window (the default), the whole page, or none."}
            }})),
            ("show_page", json!({"type": "object", "properties": {
                "title": {"type": "string", "description": "A few words for it: shown with it, and its name."},
                "html": {"type": "string", "description": "A whole HTML document."},
                "path": {"type": "string", "description": "Or an HTML file on this machine (files it uses are found next to it)."}
            }, "required": ["title"]})),
            ("show_live", json!({"type": "object", "properties": {
                "url": {"type": "string", "description": "Its address: on this machine (http://localhost:3000) or on the web."},
                "title": {"type": "string", "description": "A few words for it."},
                "engine": {"type": "string", "enum": ["chromium", "firefox", "webkit"], "description": "The engine it runs in (chromium by default)."},
                "device": {"type": "string", "enum": ["iPhone 15", "iPhone SE", "Pixel 8", "iPad Air", "Laptop", "Desktop"], "description": "A device it's laid out as (by default, as the person's screen is)."}
            }, "required": ["url"]})),
            ("use_live", json!({"type": "object", "properties": {
                "stream": {"type": "string", "description": "The stream's id, from show_live's answer."},
                "actions": {"type": "array", "description": "In order: {\"click\": [x, y]}, {\"tap\": [x, y]}, {\"type\": \"text\"}, {\"key\": \"Enter\"}, {\"scroll\": [dx, dy]}, {\"go\": \"http://localhost:3000/\"}, {\"back\": true}, {\"forward\": true}, {\"reload\": true}, {\"tab\": {\"show\": \"<tab id>\"}}, {\"tab\": {\"new\": \"https://example.com/\"}}, {\"tab\": {\"close\": \"<tab id>\"}}, {\"wait_ms\": 500}, {\"record\": true}, {\"record\": false}, {\"replay\": \"/path/live-….inputs.jsonl\"}.", "items": {"type": "object"}},
                "picture": {"type": "boolean", "description": "A picture of it after (true by default)."}
            }, "required": ["stream"]})),
            ("machines.list", json!({"type": "object", "properties": {"want": {"type": "string", "description": "What the machine must be and have (optional), as `needs` says it: `linux/amd64`, `macos`, `+gpu`, `cpus=8,mem=32g`, `tool:cargo`; or a runner's name."}}})),
            ("machines.run", command(false)),
            ("machines.start", command(false)),
            ("machines.output", json!({"type": "object", "properties": {"id": {"type": "string"}, "wait_s": {"type": "number"}}, "required": ["id"]})),
            ("machines.stop", json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]})),
        ]
    }

    /// Each of its own tools and of `kiln.machines.*` publishes its input type's schema, and it
    /// says what the hand-written one did: the same fields, required, types, enums and
    /// descriptions, but for spelling (`ops::as_written_by_hand`: a number's `format`, `true` on a
    /// free-form object), and `additionalProperties: false`, which says it takes nothing else.
    #[test]
    fn its_tools_publish_their_types_saying_what_they_said_before() {
        let before = published_before();
        let now = published();
        assert_eq!(now.len(), before.len(), "every tool of its own and every machine operation: {now:?}");
        for (name, was) in before {
            let typed = tool_input(name).or_else(|| machine_input(name)).map(|i| i.schema());
            let schema = now.iter().find(|(n, _, _)| n == name).map(|(_, s, _)| s.clone());
            assert!(schema.is_some() && schema == typed, "{name} publishes its type's schema: {schema:?}");
            let schema = schema.unwrap_or_default();
            assert_eq!(schema.get("additionalProperties"), Some(&json!(false)), "{name} takes nothing else");
            assert_eq!(crate::ops::as_written_by_hand(&schema), was, "{name}");
        }
    }

    /// A value of the wrong type for a property whose schema is `s`: a number for a string, a
    /// string for the rest; none for one that may be any JSON.
    fn wrong_for(s: &Value) -> Option<Value> {
        let t = s.get("type")?.as_str()?;
        Some(if t == "string" { json!(1) } else { json!("x") })
    }

    /// Each of its own tools and of `kiln.machines.*` reads what it's given as its type before it
    /// runs: an argument it doesn't take, each one of the wrong type, and one it must have left out
    /// are refused saying which (`unknown_field`, `invalid_type`, `missing_field`) and where. A
    /// tool's is a tool error; a machine operation's is its call's refusal, with the 400 an
    /// operation's misfit has in code mode. A misspelled `program` is refused, not run empty.
    #[test]
    fn its_tools_refuse_what_they_dont_take_saying_which_and_where() {
        let server = server_on("/nonexistent/kiln-reach.sock", "");
        let program = Program { server: Arc::clone(&server), remote: Vec::new(), unavailable: None, mine: Mutex::new(Vec::new()) };
        // What each says of what it's given: the code and the path, and whether it's refused.
        let said = |name: &str, tool: bool, given: Value| {
            if tool {
                let a = server.call(name, &given);
                (a.pointer("/structuredContent/code").cloned(), a.pointer("/structuredContent/path").cloned(), a.get("isError") == Some(&json!(true)))
            } else {
                match code::Host::call(&program, name, given) {
                    Err(a) => (a.get("code").cloned(), a.get("path").cloned(), a.get("status") == Some(&json!(400))),
                    Ok(_) => (None, None, false),
                }
            }
        };
        let refused = |code: &str, path: &str| (Some(json!(code)), Some(json!(path)), true);
        let all = published();
        assert_eq!(all.len(), 15);
        for (name, schema, tool) in &all {
            assert_eq!(said(name, *tool, json!({"zz_not_a_field": 1})), refused("unknown_field", "/zz_not_a_field"), "{name}");
            for (field, s) in schema.get("properties").and_then(Value::as_object).into_iter().flatten() {
                let Some(w) = wrong_for(s) else { continue };
                assert_eq!(said(name, *tool, json!({ field.clone(): w })), refused("invalid_type", &format!("/{field}")), "{name}'s {field}");
            }
            if schema.get("required").and_then(Value::as_array).is_some_and(|r| !r.is_empty()) {
                assert_eq!(said(name, *tool, json!({})), refused("missing_field", ""), "{name}");
            }
        }
        assert_eq!(said("code_run", true, json!({"programm": "return 1"})), refused("unknown_field", "/programm"));
        let a = server.call("show_live", &json!({"url": "http://localhost:3000", "engine": "netscape"}));
        assert_eq!(a.pointer("/structuredContent/code"), Some(&json!("invalid_value")), "an engine it doesn't list: {a}");
        // What the stream always took still fits: an engine in any case, by its browser's name too.
        let live = tool_input("show_live");
        for engine in ["Safari", "chrome", "gecko", "WebKit", ""] {
            assert!(live.as_ref().is_some_and(|i| i.fits(&json!({"url": "http://localhost:3000", "engine": engine})).is_ok()), "{engine:?}");
        }
    }

    fn open(id: &str) -> crate::harness::core::Update {
        crate::harness::core::Update::Open { info: Box::new(crate::harness::session::Info { id: id.into(), kind: "claude".into(), ..Default::default() }), since: None }
    }

    /// A runner's reach socket at `at`, as its agents' MCP servers see it: each question it's asked
    /// (`Req::Ask`) is put to the kiln server `hub` as runner `box`'s, as the runner's own
    /// connection puts it (`crate::mcp::runner_ask`), and answered with what the server says. What
    /// was asked is handed to the receiver this returns once it's answered.
    fn reach_socket(hub: Arc<crate::hub::Hub>, at: &std::path::Path) -> anyhow::Result<std::sync::mpsc::Receiver<Value>> {
        use std::io::{Read as _, Write as _};
        let listener = std::os::unix::net::UnixListener::bind(at)?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let (hub, tx) = (Arc::clone(&hub), tx.clone());
                std::thread::spawn(move || {
                    let (mut de, mut buf) = (crate::reach::wire::Deframer::default(), vec![0u8; 64 << 10]);
                    let json = loop {
                        match de.take::<Req>() {
                            Ok(Some(Req::Ask { json })) => break json,
                            Ok(None) => {}
                            Ok(Some(_)) | Err(_) => return,
                        }
                        match conn.read(&mut buf) {
                            Ok(n) if n > 0 => de.push(buf.get(..n).unwrap_or_default()),
                            _ => return,
                        }
                    };
                    let asked: Value = serde_json::from_str(&json).unwrap_or_default();
                    let answer = match serde_json::from_value::<crate::reach::Question>(asked.clone()) {
                        Ok(q) => crate::rt::block_on(async move { Ok(crate::mcp::runner_ask(&hub, "box", &q).await) }).unwrap_or_default(),
                        Err(e) => json!({"error": format!("unknown reach question: {e}")}),
                    };
                    if let Ok(b) = crate::reach::wire::frame(&Ev::Answer(answer.to_string())) {
                        let _ = conn.write_all(&b);
                    }
                    let _ = tx.send(asked);
                });
            }
        });
        Ok(rx)
    }

    /// Through reach: a call of one of its own tools, or of `kiln.machines.*` in a program, whose
    /// input doesn't fit reaches the kiln server as a note (`ReachAsk`'s `unread`), as a message it
    /// couldn't read does, without the value that came. The server notes it against the session
    /// (wire `mcp_reach`, `malformed`): counted, on its page and its runner's, and audited naming
    /// it.
    #[test]
    fn through_reach_a_misfit_reaches_the_server_as_a_note() -> anyhow::Result<()> {
        use crate::wire::unread::Kept;
        let hub = crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            hub.agents.lock().insert("box".into(), crate::hub::tests::live_runner("box", "linux", "x86_64"));
            hub.harness.ingest(&hub, &crate::harness::registry::Location::Machine("box".into()), "s1", open("s1"));
            Ok(hub)
        })?;
        let dir = tempfile::tempdir()?;
        let sock = dir.path().join("reach.sock");
        let asked = reach_socket(Arc::clone(&hub), &sock)?;
        let server = server_on(&sock.to_string_lossy(), "s1");
        let counted = || crate::metrics::wire().refused.get(&["mcp_reach", "malformed"]);
        let before = counted();
        // `run`, given a timeout that isn't a number: a tool error saying which and where.
        let a = server.call("run", &json!({"command": "make", "timeout_s": "hunter2"}));
        let (code, path) = (a.pointer("/structuredContent/code"), a.pointer("/structuredContent/path"));
        assert!(a.get("isError") == Some(&json!(true)) && code == Some(&json!("invalid_type")) && path == Some(&json!("/timeout_s")), "{a}");
        // A program's `kiln.machines.run`, given a field it doesn't take: its call refused the same
        // way, which the program catches.
        let program = "try { await kiln.machines.run({comand: 'make'}); return 'ran'; } catch (e) { return e.result; }";
        let a = server.call("code_run", &json!({ "program": program }));
        let (code, path) = (a.pointer("/structuredContent/result/code"), a.pointer("/structuredContent/result/path"));
        assert!(code == Some(&json!("unknown_field")) && path == Some(&json!("/comand")), "{a}");
        // Each told the server, which noted it before it answered.
        let notes: Vec<Value> = std::iter::from_fn(|| asked.recv_timeout(Duration::from_secs(60)).ok()).filter(|q| q.get("op") == Some(&json!("unread"))).take(2).collect();
        let what: Vec<&str> = notes.iter().filter_map(|q| q.get("what").and_then(Value::as_str)).collect();
        assert!(what.contains(&"tools/call run") && what.contains(&"code_run kiln.machines.run"), "{notes:?}");
        assert!(notes.iter().all(|q| q.get("session") == Some(&json!("s1")) && q.get("code") == Some(&json!("malformed"))), "{notes:?}");
        let words = serde_json::to_string(&notes)?;
        assert!(words.contains("(at /timeout_s)") && words.contains("(at /comand)") && !words.contains("hunter2"), "noted without the value that came: {words}");
        assert!(counted() >= before + 2, "counted");
        crate::rt::block_on(async move {
            let session = hub.harness.list().into_iter().find(|i| i.id == "s1").and_then(|i| i.refused);
            anyhow::ensure!(session.as_ref().map(Kept::total) == Some(2), "the session's page: {session:?}");
            let runner = hub.agent_list().await.into_iter().find(|r| r.name == "box").and_then(|r| r.refused);
            anyhow::ensure!(runner.as_ref().map(Kept::total) == Some(2), "the runner's page: {runner:?}");
            let principal: String = hub.store.call(|c| Ok(c.query_row("SELECT principal FROM audit2 WHERE action = 'security.wire_refused' AND target = 'mcp_reach' LIMIT 1", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(principal == "agent:s1", "{principal}");
            Ok(())
        })
    }
}
