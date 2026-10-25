//! `kiln spawn` and `kiln agents`: start and manage AI coding agents from inside a kiln shell.
//! They run in this shell's directory and environment, as kiln-native sessions of the machine's
//! kiln, and show up under this shell in kiln's UI on every device. `kiln spawn --on` starts them
//! on another machine, and `kiln agents wait` and `kiln agents result` wait for them and read what
//! they concluded, wherever they run (asked of the kiln server, `agents.*`). Run by an agent, what
//! it starts are its children, held to no more than it has (docs/agents.md).

#[cfg(unix)]
use std::io::BufRead as _;
use std::io::Write as _;

use serde_json::Value;

use super::model::SendMode;
use super::socket::Request;

#[derive(clap::Args, Clone, Debug)]
pub struct SpawnArgs {
    /// The harness: claude, codex, ... (`kiln agents kinds` lists them).
    pub kind: String,
    /// The first message (optional).
    pub prompt: Vec<String>,
    /// How many agents to start.
    #[arg(short = 'n', long, default_value_t = 1)]
    pub count: usize,
    #[arg(long)]
    pub model: Option<String>,
    /// Permission mode (claude: default, acceptEdits, plan, auto, dontAsk).
    #[arg(long)]
    pub mode: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    /// Start them on another machine: its name (`local` for the server), or what it needs
    /// (`linux/amd64`, `macos`, `+gpu`, `cpus>=16`), the idlest that suits and has the harness.
    #[arg(long)]
    pub on: Option<String>,
    /// Their folder there (with --on; by default its home).
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
}

#[derive(clap::Args, Clone, Debug)]
pub struct AgentsArgs {
    #[command(subcommand)]
    pub cmd: Option<AgentsCmd>,
    /// Every agent on this machine, not just this shell's.
    #[arg(long, global = true)]
    pub all: bool,
}

#[derive(clap::Subcommand, Clone, Debug)]
pub enum AgentsCmd {
    /// List the agents (the default).
    List,
    /// Stop an agent (by id or id prefix), or all of this shell's.
    Stop { id: Option<String> },
    /// Interrupt what an agent (or all of this shell's) is doing.
    Interrupt { id: Option<String> },
    /// Send an agent a message (queued until its current turn ends, unless --steer or --now).
    Send {
        id: String,
        text: Vec<String>,
        /// Into the running turn, without stopping it.
        #[arg(long, conflicts_with = "now")]
        steer: bool,
        /// Stop the current turn and send at once.
        #[arg(long)]
        now: bool,
        /// An idempotency key: sent again with the same key, it's the same message, which the
        /// agent gets once.
        #[arg(long)]
        key: Option<String>,
    },
    /// Wait until agents (by id or id prefix, on any machine) are done: idle, needing a person,
    /// paused for a usage limit, stopped or ended. Prints where each stands.
    Wait {
        ids: Vec<String>,
        /// Seconds to wait at most (at most 600; again to wait longer).
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Return when the first of them is done.
        #[arg(long)]
        any: bool,
        /// Return when anything about them changes.
        #[arg(long)]
        change: bool,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// What an agent concluded: its last reply, how its last turn ended, what it changed.
    Result {
        id: String,
        /// Print the answer as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[cfg(unix)]
fn socket() -> anyhow::Result<std::path::PathBuf> {
    std::env::var_os("KILN_HARNESS_SOCK")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("not in a kiln shell: KILN_HARNESS_SOCK is not set (open a terminal in kiln, or on a machine running a kiln runner)"))
}

fn shell() -> Option<String> {
    std::env::var("KILN_TERM_SESSION").ok().filter(|s| !s.is_empty())
}

#[cfg(unix)]
fn request(req: &Request) -> anyhow::Result<Value> {
    request_for(req, std::time::Duration::from_secs(30))
}

/// A request whose answer may take up to `wait` (a wait for agents).
#[cfg(unix)]
fn request_for(req: &Request, wait: std::time::Duration) -> anyhow::Result<Value> {
    let path = socket()?;
    let mut s = std::os::unix::net::UnixStream::connect(&path).map_err(|e| anyhow::anyhow!("cannot reach kiln at {}: {e}", path.display()))?;
    s.set_read_timeout(Some(wait))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut answer = String::new();
    std::io::BufReader::new(s).read_line(&mut answer)?;
    let v: Value = serde_json::from_str(&answer).map_err(|e| anyhow::anyhow!("unreadable answer from kiln: {e}"))?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        anyhow::bail!("{}", v.get("error").and_then(Value::as_str).unwrap_or("kiln refused the request"));
    }
    Ok(v)
}

#[cfg(not(unix))]
fn request(_req: &Request) -> anyhow::Result<Value> {
    anyhow::bail!("kiln spawn is not available on Windows yet")
}

#[cfg(not(unix))]
fn request_for(_req: &Request, _wait: std::time::Duration) -> anyhow::Result<Value> {
    anyhow::bail!("kiln agents is not available on Windows yet")
}

/// One of kiln's operations, asked of the kiln server through this machine's kiln.
fn op(name: &str, input: Value, wait: std::time::Duration) -> anyhow::Result<Value> {
    let v = request_for(&Request::Op { name: name.to_owned(), input, terminal: shell() }, wait)?;
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

fn print(s: &str) {
    let _ = writeln!(std::io::stdout(), "{s}");
}

pub fn spawn(a: SpawnArgs) -> anyhow::Result<()> {
    let prompt = a.prompt.join(" ");
    // On another machine: the kiln server picks it and starts them there.
    if let Some(on) = a.on.clone() {
        let input = serde_json::json!({"kind": a.kind, "on": on, "cwd": a.cwd, "prompt": (!prompt.trim().is_empty()).then_some(prompt), "model": a.model, "mode": a.mode, "name": a.name, "count": a.count, "effort": a.effort});
        let v = op("agents.start", input, std::time::Duration::from_secs(60))?;
        for s in v.get("agents").and_then(Value::as_array).cloned().unwrap_or_default() {
            let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            print(&format!("started {} ({}) on {}{}", g("name"), short(&g("id")), g("machine"), if g("mode").is_empty() { String::new() } else { format!(", in {}", g("mode")) }));
        }
        print("`kiln agents wait <id>` waits for them; `kiln agents result <id>` says what they concluded");
        return Ok(());
    }
    let cwd = a.cwd.clone().or_else(|| std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned()));
    let env: Vec<(String, String)> = std::env::vars().collect();
    let v = request(&Request::Spawn {
        kind: a.kind,
        count: Some(a.count),
        prompt: (!prompt.trim().is_empty()).then_some(prompt),
        model: a.model,
        mode: a.mode,
        name: a.name,
        shell: shell(),
        cwd,
        env,
        program: None,
    })?;
    for s in v.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default() {
        let id = s.get("id").and_then(Value::as_str).unwrap_or("");
        let name = s.get("name").and_then(Value::as_str).unwrap_or("");
        print(&format!("started {name} ({})", short(id)));
    }
    print("open kiln's Agents page to work with them, or `kiln agents` here");
    Ok(())
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub fn agents(a: AgentsArgs) -> anyhow::Result<()> {
    let scope = if a.all { None } else { shell() };
    match a.cmd.unwrap_or(AgentsCmd::List) {
        AgentsCmd::List => {
            let v = request(&Request::List { shell: scope })?;
            let list = v.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
            if list.is_empty() {
                print(if a.all { "no agents on this machine" } else { "no agents in this shell (`kiln agents --all` for the whole machine)" });
                return Ok(());
            }
            for s in list {
                let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                let waiting = s.get("approvals").and_then(Value::as_array).map_or(0, Vec::len);
                let status = s.get("status").and_then(Value::as_str).unwrap_or("?");
                let status = if waiting > 0 { format!("waiting on you ({waiting})") } else { status.to_owned() };
                let title = if g("title").is_empty() { g("name") } else { g("title") };
                print(&format!("{}  {:<12} {:<18} {}  {}", short(&g("id")), g("kind"), status, g("cwd"), title));
            }
        }
        AgentsCmd::Stop { id } => {
            let v = request(&Request::Stop { id, shell: scope })?;
            print(&format!("stopped {}", v.get("stopped").and_then(Value::as_u64).unwrap_or(0)));
        }
        AgentsCmd::Interrupt { id } => {
            let v = request(&Request::Interrupt { id, shell: scope })?;
            print(&format!("interrupted {}", v.get("interrupted").and_then(Value::as_u64).unwrap_or(0)));
        }
        AgentsCmd::Send { id, text, steer, now, key } => {
            let mode = if now {
                SendMode::Now
            } else if steer {
                SendMode::Steer
            } else {
                SendMode::Queue
            };
            request(&Request::Send { id, text: text.join(" "), mode, key })?;
            print("sent");
        }
        AgentsCmd::Wait { ids, timeout, any, change, json } => {
            if ids.is_empty() {
                anyhow::bail!("say which agents to wait for (their ids, or the start of them)");
            }
            let timeout = timeout.min(600);
            let input = serde_json::json!({"agents": ids, "timeout_s": timeout, "any": any, "until": if change { "change" } else { "settled" }});
            let v = op("agents.wait", input, std::time::Duration::from_secs(timeout + 60))?;
            if json {
                print(&serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for s in v.get("agents").and_then(Value::as_array).cloned().unwrap_or_default() {
                let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                let what = match s.get("needs").and_then(Value::as_array).and_then(|n| n.first()) {
                    Some(n) => format!("asks a person: {}", n.get("title").and_then(Value::as_str).unwrap_or("")),
                    None => g("summary").lines().next().unwrap_or("").to_owned(),
                };
                print(&format!("{}  {:<10} {}  {}", short(&g("id")), g("stand"), g("name"), what));
            }
            // Messages other agents sent it, handed over here (taken out of its queue).
            for m in v.get("messages").and_then(Value::as_array).cloned().unwrap_or_default() {
                let g = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                let from = g("from");
                let who = if g("name").is_empty() { short(&from).to_owned() } else { format!("{} ({})", g("name"), short(&from)) };
                print(&format!("\nA message from {who}:\n{}", g("text").trim()));
            }
            if v.get("timed_out").and_then(Value::as_bool) == Some(true) {
                print(&format!("still going after {timeout}s: wait again to keep waiting"));
            }
        }
        AgentsCmd::Result { id, json } => {
            let v = op("agents.result", serde_json::json!({"agent": id}), std::time::Duration::from_secs(60))?;
            if json {
                print(&serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let a = v.get("agent").cloned().unwrap_or(Value::Null);
            let g = |x: &Value, k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            print(&format!("{} ({}) on {}, {}", g(&a, "name"), short(&g(&a, "id")), g(&a, "machine"), g(&a, "stand")));
            let reply = v.pointer("/reply/text").and_then(Value::as_str).unwrap_or("");
            if !reply.trim().is_empty() {
                print("");
                print(reply.trim());
                print("");
            }
            if let Some(t) = v.get("last_turn").filter(|t| !t.is_null()) {
                print(&format!("Its last turn: {}{}", g(t, "how"), t.get("why").and_then(Value::as_str).map(|w| format!(" ({w})")).unwrap_or_default()));
            }
            let c = v.get("changes").cloned().unwrap_or(Value::Null);
            let n = |k: &str| c.get(k).and_then(Value::as_u64).unwrap_or(0);
            if n("files_changed") > 0 {
                print(&format!("It changed {} file{}: +{} −{}", n("files_changed"), if n("files_changed") == 1 { "" } else { "s" }, n("lines_added"), n("lines_removed")));
                for f in c.get("files").and_then(Value::as_array).cloned().unwrap_or_default() {
                    print(&format!("  {}  +{} −{}", g(&f, "path"), f.get("added").and_then(Value::as_u64).unwrap_or(0), f.get("removed").and_then(Value::as_u64).unwrap_or(0)));
                }
            } else {
                print("It changed no files.");
            }
            if let Some(cost) = v.get("cost_usd").and_then(Value::as_f64).filter(|c| *c > 0.0) {
                print(&format!("Cost: ${cost:.2}"));
            }
        }
    }
    Ok(())
}
