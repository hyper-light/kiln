//! `kiln spawn` and `kiln agents`: start and manage AI coding agents from inside a kiln shell.
//! They run in this shell's directory and environment, as kiln-native sessions of the machine's
//! kiln, and show up under this shell in kiln's UI on every device.

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
    let path = socket()?;
    let mut s = std::os::unix::net::UnixStream::connect(&path).map_err(|e| anyhow::anyhow!("cannot reach kiln at {}: {e}", path.display()))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
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

fn print(s: &str) {
    let _ = writeln!(std::io::stdout(), "{s}");
}

pub fn spawn(a: SpawnArgs) -> anyhow::Result<()> {
    let prompt = a.prompt.join(" ");
    let cwd = std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned());
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
        AgentsCmd::Send { id, text, steer, now } => {
            let mode = if now {
                SendMode::Now
            } else if steer {
                SendMode::Steer
            } else {
                SendMode::Queue
            };
            request(&Request::Send { id, text: text.join(" "), mode })?;
            print("sent");
        }
    }
    Ok(())
}
