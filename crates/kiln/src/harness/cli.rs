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
pub(crate) fn request(req: &Request) -> anyhow::Result<Value> {
    request_for(req, std::time::Duration::from_secs(30))
}

/// A request whose answer may take up to `wait` (a wait for agents).
#[cfg(unix)]
fn request_for(req: &Request, wait: std::time::Duration) -> anyhow::Result<Value> {
    request_at(&socket()?, req, wait)
}

/// [`request_for`], to the socket at `path`.
#[cfg(unix)]
fn request_at(path: &std::path::Path, req: &Request, wait: std::time::Duration) -> anyhow::Result<Value> {
    let s = std::os::unix::net::UnixStream::connect(path).map_err(|e| anyhow::anyhow!("cannot reach kiln at {}: {e}", path.display()))?;
    answered(exchange(s, req, wait)?)
}

/// `req` on the connection `s`, and kiln's answer, whatever it says.
#[cfg(unix)]
fn exchange(mut s: std::os::unix::net::UnixStream, req: &Request, wait: std::time::Duration) -> anyhow::Result<Value> {
    s.set_read_timeout(Some(wait))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut answer = String::new();
    std::io::BufReader::new(s).read_line(&mut answer)?;
    serde_json::from_str(&answer).map_err(|e| anyhow::anyhow!("unreadable answer from kiln: {e}"))
}

/// kiln's answer `v`, unless it refused, saying why.
#[cfg(unix)]
fn answered(v: Value) -> anyhow::Result<Value> {
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        anyhow::bail!("{}", v.get("error").and_then(Value::as_str).unwrap_or("kiln refused the request"));
    }
    Ok(v)
}

/// What the runner holding this agent session's key answers `req`, a signature or a credential
/// (W27-R03): asked of the runner process that started the session first, on its own socket
/// (`KILN_KEY_SOCK`), then of the machine's (`KILN_HARNESS_SOCK`).
#[cfg(unix)]
pub(crate) fn request_keys(req: &Request) -> anyhow::Result<Value> {
    request_keys_at(std::env::var_os("KILN_KEY_SOCK").map(std::path::PathBuf::from).as_deref(), &socket()?, req)
}

/// [`request_keys`], of the runner's own socket `own` and the machine's `machine`. The machine's
/// answers where the runner has ended (its socket's file gone, or nothing answers on it), where it
/// no longer runs the session (an upgrade handed the session to the machine's new build: it says
/// `elsewhere`), and where it had no socket of its own (a build from before them).
#[cfg(unix)]
pub(crate) fn request_keys_at(own: Option<&std::path::Path>, machine: &std::path::Path, req: &Request) -> anyhow::Result<Value> {
    let wait = std::time::Duration::from_secs(30);
    if let Some(own) = own {
        match std::os::unix::net::UnixStream::connect(own) {
            Ok(s) => {
                let v = exchange(s, req, wait)?;
                if v.get("elsewhere").and_then(Value::as_bool) != Some(true) {
                    return answered(v);
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {}
            Err(e) => anyhow::bail!("cannot reach kiln at {}: {e}", own.display()),
        }
    }
    request_at(machine, req, wait)
}

#[cfg(not(unix))]
pub(crate) fn request(_req: &Request) -> anyhow::Result<Value> {
    anyhow::bail!("kiln spawn is not available on Windows yet")
}

#[cfg(not(unix))]
pub(crate) fn request_keys(_req: &Request) -> anyhow::Result<Value> {
    anyhow::bail!("an agent session's key is asked of its runner over a Unix socket, which Windows doesn't have yet")
}

/// `kiln harness-hook`: a harness's hook before each of its tool calls (Codex's, given by kiln as
/// it starts it: `codex::hook_args`; Antigravity's, in its hooks.json: `antigravity::hooks_with`).
#[derive(clap::Args, Debug)]
pub struct HarnessHookArgs {
    /// The harness socket of the runner the session is on (none: the one the harness was given,
    /// `KILN_HARNESS_SOCK`).
    #[arg(long)]
    pub sock: Option<std::path::PathBuf>,
    /// Antigravity's hook, for this event of its (`antigravity::HOOK_EVENTS`): agy names none in
    /// its input, and takes an answer of its own form even when kiln holds nothing.
    #[arg(long)]
    pub agy: Option<String>,
}

/// The hook's input (stdin) goes to the session it runs in, told by its process, and the session's
/// answer is printed once it gives it: a call a person holds waits for them, up to a week. What goes
/// wrong lets the call go (nothing printed): a kiln that can't be reached holds nothing.
/// Antigravity's is told its event and its conversation's tokens so far, and gets its own pass
/// (`antigravity::hook_pass`) when kiln has no answer, as when agy runs outside kiln.
#[cfg(unix)]
pub fn harness_hook(a: HarnessHookArgs) -> anyhow::Result<()> {
    let pass = || {
        if let Some(ev) = &a.agy {
            println!("{}", super::antigravity::hook_pass(ev));
        }
    };
    let mut input = String::new();
    if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input) {
        eprintln!("kiln: the hook's input was unreadable: {e}");
        pass();
        return Ok(());
    }
    let mut input = serde_json::from_str(&input).unwrap_or(Value::Null);
    let Some(sock) = a.sock.clone().or_else(|| std::env::var_os("KILN_HARNESS_SOCK").map(std::path::PathBuf::from)) else {
        pass();
        return Ok(());
    };
    if let Some(ev) = &a.agy {
        let transcript = input.get("transcriptPath").and_then(Value::as_str).and_then(|p| read_small(std::path::Path::new(p)));
        input = super::antigravity::hook_input(input, ev, transcript.as_deref());
    }
    match request_at(&sock, &Request::Hook { input }, std::time::Duration::from_secs(8 * 24 * 3600)) {
        Ok(v) => match v.get("answer").filter(|a| a.as_object().is_some_and(|o| !o.is_empty())) {
            Some(answer) => println!("{answer}"),
            None => pass(),
        },
        Err(e) => {
            eprintln!("kiln: {e:#}");
            pass();
        }
    }
    Ok(())
}

/// A file's text, unless it's past 64 MiB (a transcript too long to count). Read whole on each of
/// the hook's calls (every tool call and model call of the conversation): fine for transcripts as
/// agy keeps them, a cost that grows with a long one.
#[cfg(unix)]
fn read_small(path: &std::path::Path) -> Option<String> {
    let len = std::fs::metadata(path).ok()?.len();
    (len <= 64 << 20).then(|| std::fs::read_to_string(path).ok()).flatten()
}

/// No harness socket on Windows yet: nothing of kiln's holds the call.
#[cfg(not(unix))]
pub fn harness_hook(a: HarnessHookArgs) -> anyhow::Result<()> {
    if let Some(ev) = &a.agy {
        println!("{}", super::antigravity::hook_pass(ev));
    }
    Ok(())
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A runner's socket at `path` that gives `answer` to every request, on a thread of its own:
    /// how many it was asked.
    fn answering(path: &std::path::Path, answer: Value) -> anyhow::Result<std::sync::Arc<std::sync::atomic::AtomicUsize>> {
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&asked);
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut line = String::new();
                if let Ok(r) = s.try_clone() {
                    let _ = std::io::BufReader::new(r).read_line(&mut line);
                }
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = s.write_all(format!("{answer}\n").as_bytes());
            }
        });
        Ok(asked)
    }

    /// W27-R03, 2026-11-02: an agent session's signature and credential are asked of the runner
    /// process that started it, on its own socket, not of whichever process serves the machine's
    /// (an upgrade's old build, which neither knew the request nor held the key). The machine's
    /// answers once that runner has ended, where it no longer runs the session (an upgrade handed
    /// it over), and where it had no socket of its own; a refusal of the runner's own stands.
    #[test]
    fn w27_r03_a_sessions_key_is_asked_of_its_own_runner_then_the_machines() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let req = Request::Credential { forget: false };
        let machine = dir.path().join("harness-m.sock");
        let at_machine = answering(&machine, serde_json::json!({"ok": true, "from": "machine"}))?;
        let from = |v: anyhow::Result<Value>| -> anyhow::Result<String> { Ok(v?.get("from").and_then(Value::as_str).unwrap_or_default().to_owned()) };

        let own = dir.path().join("harness-m-p1.sock");
        let at_own = answering(&own, serde_json::json!({"ok": true, "from": "own"}))?;
        anyhow::ensure!(from(request_keys_at(Some(&own), &machine, &req))? == "own", "not asked of the session's own runner");
        anyhow::ensure!(at_machine.load(std::sync::atomic::Ordering::SeqCst) == 0, "the machine's socket was asked too");
        anyhow::ensure!(at_own.load(std::sync::atomic::Ordering::SeqCst) == 1);

        let handed = dir.path().join("harness-m-p2.sock");
        answering(&handed, serde_json::json!({"ok": false, "elsewhere": true, "error": "no agent session this kiln process runs asks"}))?;
        anyhow::ensure!(from(request_keys_at(Some(&handed), &machine, &req))? == "machine", "a session handed over wasn't asked of the machine's");

        let ended = dir.path().join("harness-m-p3.sock");
        drop(std::os::unix::net::UnixListener::bind(&ended)?);
        anyhow::ensure!(from(request_keys_at(Some(&ended), &machine, &req))? == "machine", "an ended runner's socket wasn't passed over");
        anyhow::ensure!(from(request_keys_at(Some(&dir.path().join("harness-m-p4.sock")), &machine, &req))? == "machine", "a missing socket wasn't passed over");
        anyhow::ensure!(from(request_keys_at(None, &machine, &req))? == "machine", "a session of an older runner wasn't asked of the machine's");

        let refusing = dir.path().join("harness-m-p5.sock");
        answering(&refusing, serde_json::json!({"ok": false, "error": "agent session s1 has no key at epoch 2"}))?;
        let before = at_machine.load(std::sync::atomic::Ordering::SeqCst);
        let refused = request_keys_at(Some(&refusing), &machine, &req);
        anyhow::ensure!(refused.as_ref().is_err_and(|e| e.to_string().contains("no key at epoch 2")), "{refused:?}");
        anyhow::ensure!(at_machine.load(std::sync::atomic::Ordering::SeqCst) == before, "the runner's own refusal went to the machine's");
        Ok(())
    }
}
