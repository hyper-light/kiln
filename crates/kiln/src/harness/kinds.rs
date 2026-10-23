//! The registry of harnesses kiln can run as structured sessions, and the adapter each one uses.
//! Adding a harness is adding an entry here and, if it speaks a protocol kiln does not know yet,
//! an adapter. `verified` says whether kiln has been tested against a real install of it.

use std::process::Command as Process;

use super::acp::Acp;
use super::antigravity::Agy;
use super::claude::Claude;
use super::codex::Codex;
use super::core::Core;
use super::model::Command;
use super::session::Spec;

pub struct Kind {
    pub id: &'static str,
    pub name: &'static str,
    /// The executable looked up on PATH.
    pub program: &'static str,
    /// Tested end to end against a real install.
    pub verified: bool,
    /// How kiln talks to it.
    pub protocol: Protocol,
    /// Its own terminal interface: the program and arguments that open it.
    pub screen: &'static [&'static str],
    /// That interface starts on a slash command given as its first argument (`claude /doctor`).
    pub screen_takes_command: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Claude Code's stream-json + control protocol.
    ClaudeStreamJson,
    /// Codex's app-server (JSON-RPC v2).
    CodexAppServer,
    /// The Agent Client Protocol, started with these arguments.
    Acp(&'static [&'static str]),
    /// Antigravity's headless multi-turn stream-json (`agy --input-format stream-json`).
    AgyStreamJson,
    /// No structured mode: it runs as a terminal program, in a kiln terminal.
    Terminal,
}

const KINDS: &[Kind] = &[
    Kind { id: "claude", name: "Claude Code", program: "claude", verified: true, protocol: Protocol::ClaudeStreamJson, screen: &["claude"], screen_takes_command: true },
    Kind { id: "codex", name: "Codex", program: "codex", verified: true, protocol: Protocol::CodexAppServer, screen: &["codex"], screen_takes_command: false },
    Kind { id: "devin", name: "Devin", program: "devin", verified: true, protocol: Protocol::Acp(&["acp"]), screen: &["devin"], screen_takes_command: false },
    Kind { id: "gemini", name: "Gemini CLI", program: "gemini", verified: false, protocol: Protocol::Acp(&["--acp"]), screen: &["gemini"], screen_takes_command: false },
    Kind { id: "opencode", name: "opencode", program: "opencode", verified: false, protocol: Protocol::Acp(&["acp"]), screen: &["opencode"], screen_takes_command: false },
    Kind { id: "goose", name: "Goose", program: "goose", verified: false, protocol: Protocol::Acp(&["acp"]), screen: &["goose", "session"], screen_takes_command: false },
    // Amp has no ACP mode of its own; its ACP adapter (amp-acp) wraps the signed-in Amp CLI.
    Kind { id: "amp", name: "Amp", program: "amp-acp", verified: false, protocol: Protocol::Acp(&[]), screen: &["amp"], screen_takes_command: false },
    Kind { id: "antigravity", name: "Antigravity CLI", program: "agy", verified: true, protocol: Protocol::AgyStreamJson, screen: &["agy"], screen_takes_command: false },
];

impl Kind {
    pub fn all() -> &'static [Kind] {
        KINDS
    }

    pub fn get(id: &str) -> Option<&'static Kind> {
        KINDS.iter().find(|k| k.id == id)
    }

    /// Installed on this machine.
    pub fn available(&self) -> bool {
        crate::os::which(self.program).is_some()
    }

    /// kiln can run it as a structured session (else only as a terminal program).
    pub fn structured(&self) -> bool {
        self.protocol != Protocol::Terminal
    }

    pub fn adapter(&self, spec: &Spec) -> Adapter {
        match self.protocol {
            Protocol::ClaudeStreamJson => Adapter::Claude(Box::new(Claude::new(spec.allow_bypass).with_config_dir(claude_config_dir(spec)))),
            Protocol::CodexAppServer => Adapter::Codex(Box::new(Codex::from_spec(spec))),
            Protocol::AgyStreamJson => Adapter::Agy(Box::new(Agy::new(spec))),
            Protocol::Acp(_) | Protocol::Terminal => Adapter::Acp(Box::new(Acp::from_spec(spec))),
        }
    }

    /// The process to start for session `session` of `spec` (cwd, environment and arguments).
    #[cfg_attr(not(unix), allow(unused_variables))]
    pub fn launch(&self, spec: &Spec, session: &str) -> anyhow::Result<Process> {
        if self.protocol == Protocol::Terminal {
            anyhow::bail!("{} has no structured mode; run it in a terminal ({})", self.name, self.program);
        }
        let program = match &spec.program {
            Some(p) => std::path::PathBuf::from(p),
            None => crate::os::which(self.program).ok_or_else(|| anyhow::anyhow!("{} ({}) is not installed on this machine", self.name, self.program))?,
        };
        let mut p = Process::new(program);
        let cwd = super::session::resolve_cwd(spec.cwd.as_deref());
        if !std::path::Path::new(&cwd).is_dir() {
            anyhow::bail!("{cwd} is not a directory on this machine");
        }
        p.current_dir(&cwd);
        set_env(&mut p, spec);
        match self.protocol {
            Protocol::ClaudeStreamJson => {
                let session_id = spec.resume.is_none().then(|| uuid::Uuid::now_v7().to_string());
                p.args(super::claude::args(spec, session_id.as_deref()));
                // kiln's MCP server: on a runner, its own (code mode over the kiln's machines and
                // kiln's operations, through the runner); else the server's, where this machine's
                // kiln serves it.
                let local = local_tools(spec);
                match (&local, crate::mcp::kit()) {
                    #[cfg(unix)]
                    (Some((kiln, sock, prefer)), _) => {
                        p.arg("--mcp-config").arg(crate::reach::mcp::claude_config(kiln, sock, *prefer, session));
                    }
                    // Its own token: kiln knows which agent calls, and whom it acts for.
                    (_, Some(kit)) => {
                        p.arg("--mcp-config").arg(kit.config_for(session));
                    }
                    _ => {}
                }
                // kiln's skills.
                if let Some(dir) = crate::mcp::kit().map(|k| k.plugin_dir.as_path()).or_else(crate::mcp::plugin_dir) {
                    p.arg("--plugin-dir").arg(dir);
                }
                // File snapshots before edits, so a rewind can restore them.
                p.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "1");
            }
            // Model, mode and resume go over the protocol (thread/start, turn/start).
            Protocol::CodexAppServer => {
                p.args(super::codex::args());
                // The most subagents it runs at once (its Agent settings), as Codex's own limit.
                if let Some(n) = spec.max_subagents {
                    p.arg("-c").arg(format!("agents.max_concurrent_threads_per_session={n}"));
                }
                #[cfg(unix)]
                if let Some((kiln, sock, prefer)) = local_tools(spec) {
                    p.args(crate::reach::mcp::codex_config(&kiln, sock, prefer, session));
                }
            }
            Protocol::Acp(args) => {
                p.args(args);
            }
            // Model, effort, mode and the conversation are flags: a change starts it again.
            Protocol::AgyStreamJson => {
                p.args(super::antigravity::args(spec));
            }
            Protocol::Terminal => {}
        }
        Ok(p)
    }
}

impl Kind {
    /// The harness's own CLI with these arguments (`mcp add ...`), run the way the session runs
    /// the harness: same executable, directory and environment, so it changes the configuration
    /// the session uses. Its program is the session's own when the spec names one.
    pub fn cli(&self, spec: &Spec, args: &[String]) -> anyhow::Result<Process> {
        if !self.structured() {
            anyhow::bail!("{} has no command line kiln can run", self.name);
        }
        let program = match &spec.program {
            Some(p) => std::path::PathBuf::from(p),
            None => crate::os::which(self.program).ok_or_else(|| anyhow::anyhow!("{} ({}) is not installed on this machine", self.name, self.program))?,
        };
        let mut p = Process::new(program);
        let cwd = spec.cwd.clone().unwrap_or_else(|| crate::os::home_dir().to_string_lossy().into_owned());
        if std::path::Path::new(&cwd).is_dir() {
            p.current_dir(&cwd);
        }
        set_env(&mut p, spec);
        // Nothing it runs from here may open a browser on the machine's desktop (a sign-in is
        // kiln's to show, wherever the person is).
        p.env("BROWSER", "true");
        p.args(args);
        p.stdin(std::process::Stdio::null());
        Ok(p)
    }
}

/// Whether a session gets the runner's own kiln MCP server (`crate::reach::mcp`), and how to
/// start it: this kiln binary, the runner's reach socket, the session's preferred runner.
#[cfg(unix)]
fn local_tools(spec: &Spec) -> Option<(std::path::PathBuf, &'static str, Option<&str>)> {
    let pref = spec.reach.as_deref().map(str::trim).filter(|r| !r.is_empty());
    if pref == Some("off") {
        return None;
    }
    let sock = crate::reach::local::socket()?;
    let kiln = std::env::current_exe().ok()?;
    Some((kiln, sock, pref))
}

/// Without Unix sockets there is no runner of the machine to talk to.
#[cfg(not(unix))]
fn local_tools(_spec: &Spec) -> Option<(std::path::PathBuf, &'static str, Option<&str>)> {
    None
}

/// The MCP servers an ACP agent's session gets (`session/new`'s `mcpServers`): the runner's own.
pub fn acp_servers(spec: &Spec, session: &str) -> Vec<serde_json::Value> {
    #[cfg(unix)]
    if let Some((kiln, sock, prefer)) = local_tools(spec) {
        return vec![crate::reach::mcp::acp_config(&kiln, sock, prefer, session)];
    }
    let _ = (spec, session);
    Vec::new()
}

/// The session's environment over the machine's: what its shell had (minus the shell's own
/// terminal plumbing), without what a Claude Code session sets for its children.
fn set_env(p: &mut Process, spec: &Spec) {
    // `kiln run` in the agent's shell reaches other runners through this machine's runner.
    if let Some(sock) = crate::reach::local::socket() {
        p.env("KILN_REACH_SOCK", sock);
    }
    for (k, v) in &spec.env {
        if !matches!(k.as_str(), "KILN_TERM_SESSION" | "KILN_HARNESS_SOCK" | "KILN_REACH_SOCK" | "TERM" | "COLUMNS" | "LINES") {
            p.env(k, v);
        }
    }
    // `kiln spawn` and `kiln agents` in its commands: what it starts are its children (the socket
    // knows the agent by its process).
    if let Some(sock) = super::socket::served() {
        p.env("KILN_HARNESS_SOCK", sock);
    }
    for k in super::claude::SCRUB_ENV {
        p.env_remove(k);
    }
}

/// Where Claude Code keeps its configuration for this session: `CLAUDE_CONFIG_DIR` from the
/// session's environment or the machine's, else `~/.claude`.
fn claude_config_dir(spec: &Spec) -> String {
    spec.env
        .iter()
        .rev()
        .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok())
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| crate::os::home_dir().join(".claude").to_string_lossy().into_owned())
}

/// A harness adapter: turns the harness's output into the normalized model and commands into
/// its input. Pure state machines over [`Core`]; the session owns the process.
pub enum Adapter {
    Claude(Box<Claude>),
    Codex(Box<Codex>),
    Acp(Box<Acp>),
    Agy(Box<Agy>),
}

/// An adapter written out whole, for this process's next image (an in-place upgrade).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "adapter", content = "state", rename_all = "snake_case")]
pub enum Frozen {
    Claude(Box<Claude>),
    Codex(Box<Codex>),
    Acp(Box<Acp>),
    Agy(Box<Agy>),
}

impl Adapter {
    /// The MCP servers a session it opens gets, for an adapter that gives them over its protocol
    /// (ACP); the others get theirs as they start ([`Kind::launch`]).
    pub fn offer_mcp(&mut self, servers: Vec<serde_json::Value>) {
        if let Self::Acp(a) = self {
            a.offer_mcp(servers);
        }
    }

    /// Its state, whole, to go on with in the next image ([`Adapter::thaw`]).
    pub fn freeze(self) -> Frozen {
        match self {
            Self::Claude(a) => Frozen::Claude(a),
            Self::Codex(a) => Frozen::Codex(a),
            Self::Acp(a) => Frozen::Acp(a),
            Self::Agy(a) => Frozen::Agy(a),
        }
    }

    pub fn thaw(f: Frozen) -> Self {
        match f {
            Frozen::Claude(a) => Self::Claude(a),
            Frozen::Codex(a) => Self::Codex(a),
            Frozen::Acp(a) => Self::Acp(a),
            Frozen::Agy(a) => Self::Agy(a),
        }
    }

    pub fn start(&mut self, core: &mut Core) {
        // A new process: nothing an earlier one ran, in this harness or the one before a switch,
        // goes on under it.
        core.end_harness_work();
        match self {
            Self::Claude(a) => a.start(core),
            Self::Codex(a) => a.start(core),
            Self::Acp(a) => a.start(core),
            Self::Agy(a) => a.start(core),
        }
    }

    pub fn line(&mut self, core: &mut Core, line: &str) {
        match self {
            Self::Claude(a) => a.line(core, line),
            Self::Codex(a) => a.line(core, line),
            Self::Acp(a) => a.line(core, line),
            Self::Agy(a) => a.line(core, line),
        }
    }

    pub fn command(&mut self, core: &mut Core, c: Command) {
        match self {
            Self::Claude(a) => a.command(core, c),
            Self::Codex(a) => a.command(core, c),
            Self::Acp(a) => a.command(core, c),
            Self::Agy(a) => a.command(core, c),
        }
    }

    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        match self {
            Self::Claude(a) => a.exited(core, code, error),
            Self::Codex(a) => a.exited(core, code, error),
            Self::Acp(a) => a.exited(core, code, error),
            Self::Agy(a) => a.exited(core, code, error),
        }
        // Whatever the harness and whatever its adapter knew: what it ran ends with its process
        // (a switch, a stop, a restart or a crash), its subagents' and shells' calls with it.
        core.end_harness_work();
    }

    /// A run (`Effect::Run` or `Effect::Fetch`) ended: its exit code (None: it could not start
    /// or was stopped) and its output (stdout then stderr, shortened).
    pub fn run_done(&mut self, core: &mut Core, job: &str, code: Option<i32>, output: &str) {
        if let Some(at) = job.strip_prefix(super::pause::CHECKPOINT_JOB) {
            super::pause::checkpointed(core, at, code, output);
            return;
        }
        match self {
            Self::Claude(a) => a.run_done(core, job, code, output),
            Self::Codex(a) => a.run_done(core, job, code, output),
            Self::Acp(a) => a.run_done(core, job, code, output),
            Self::Agy(a) => a.run_done(core, job, code, output),
        }
    }

    /// Every few seconds while the process runs: a chance to poll what the harness only reports
    /// when asked (plan usage, servers still connecting).
    pub fn tick(&mut self, core: &mut Core) {
        // A pause's timer: time to check whether its limit reset (or to stop waiting for the answer).
        let key = core.state.harness_session.clone().unwrap_or_else(|| core.state.cwd.clone());
        match super::pause::step(core, Core::now(), &key) {
            super::pause::Step::Check => self.check_limits(core),
            super::pause::Step::GoOn(p) => self.go_on(core, *p),
            super::pause::Step::Nothing => {}
        }
        match self {
            Self::Claude(a) => a.tick(core),
            Self::Codex(a) => a.tick(core),
            Self::Agy(a) => a.tick(core),
            Self::Acp(a) => a.tick(core),
        }
    }

    /// Ask the harness whether a pause's limit has reset (its answer goes to `pause::checked`). One
    /// that can't say goes on, and finds out.
    fn check_limits(&mut self, core: &mut Core) {
        match self {
            Self::Claude(a) => a.check_limits(core),
            Self::Codex(a) => a.check_limits(core),
            Self::Acp(a) => {
                if let Some(p) = super::pause::release(core, "The limit's reset time came; going on.") {
                    a.go_on(core, p);
                }
            }
            Self::Agy(a) => {
                if let Some(p) = super::pause::release(core, "The limit's reset time came; going on.") {
                    a.go_on(core, p);
                }
            }
        }
    }

    /// A pause ended (it reset, or a person resumed it): the turn it cut off goes on, then kiln's
    /// queue.
    pub fn go_on(&mut self, core: &mut Core, p: super::model::Pause) {
        match self {
            Self::Claude(a) => a.go_on(core, p),
            Self::Codex(a) => a.go_on(core, p),
            Self::Acp(a) => a.go_on(core, p),
            Self::Agy(a) => a.go_on(core, p),
        }
    }

    /// Start with a conversation carried over from another harness, in the harness's own form
    /// (Codex: Responses items for its new thread; Claude Code reads its own from a transcript).
    pub fn seed(&mut self, items: Vec<serde_json::Value>) {
        // The conversation in words (`moving::Carry::Words`), for a harness that takes it only with
        // its first message.
        let words = || items.iter().find(|i| i.get("type").and_then(serde_json::Value::as_str) == Some(super::moving::CONTEXT)).and_then(|i| i.get("text")).and_then(serde_json::Value::as_str).map(str::to_owned);
        match self {
            Self::Codex(a) => a.seed(items),
            Self::Acp(a) => a.offer_context(words()),
            Self::Agy(a) => a.offer_context(words()),
            Self::Claude(_) => {}
        }
    }

    /// That conversation is in (or there was none).
    pub fn seeded(&self) -> bool {
        match self {
            Self::Codex(a) => a.seeded(),
            Self::Claude(_) | Self::Acp(_) | Self::Agy(_) => true,
        }
    }
}

/// What a machine offers, for the "new agent" dialog.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct KindInfo {
    pub id: String,
    pub name: String,
    pub verified: bool,
    pub available: bool,
    /// Its permission modes, those that skip every check marked `dangerous` (a choice that takes
    /// a confirmation, never a default). ACP agents other than Devin announce theirs per session.
    #[serde(default)]
    pub modes: Vec<super::model::ModeOption>,
    #[serde(default)]
    pub default_mode: String,
    /// The effort levels it takes, lowest first (empty: none, or announced per session). Starting
    /// without one leaves the model's own default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<super::model::ControlOption>,
    /// kiln runs it as a structured session; false: it runs in a kiln terminal (`program`).
    #[serde(default = "yes")]
    pub structured: bool,
    /// The executable (what a terminal runs for one that is not structured).
    #[serde(default)]
    pub program: String,
    /// The models it offers: as its sessions on this machine last listed them, else the ones it
    /// is known to have (empty: not known).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<super::model::ModelOption>,
    /// A session of another harness can go on in this one, its conversation carried over
    /// (`Command::SetModel::kind`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub switch: bool,
}

fn yes() -> bool {
    true
}


impl Kind {
    /// Its permission modes, and the one a new session starts in.
    pub fn modes(&self) -> (Vec<super::model::ModeOption>, &'static str) {
        match self.protocol {
            Protocol::ClaudeStreamJson => {
                let mut m = super::claude::modes(true);
                // As root (outside a sandbox) Claude Code refuses to skip its checks.
                if !super::claude::bypass_possible() {
                    m.retain(|x| !x.dangerous);
                }
                (m, "default")
            }
            Protocol::CodexAppServer => (super::codex::modes(true), "auto"),
            Protocol::AgyStreamJson => (super::antigravity::modes(), super::antigravity::DEFAULT_MODE),
            Protocol::Acp(_) if self.id == "devin" => (
                [
                    ("accept-edits", "Code", "Write and edit code", false),
                    ("smart", "Smart", "Auto-approve actions the model judges safe", false),
                    ("ask", "Ask", "Answer questions without code changes", false),
                    ("plan", "Plan", "Plan changes before implementing", false),
                    ("bypass", "Bypass Permissions", "Auto-approve all tool calls unless your organization's policy requires approval", true),
                ]
                .into_iter()
                .map(|(id, label, description, dangerous)| super::model::ModeOption { id: id.into(), label: label.into(), description: description.into(), dangerous })
                .collect(),
                "accept-edits",
            ),
            Protocol::Acp(_) | Protocol::Terminal => (Vec::new(), ""),
        }
    }

    /// A conversation can be carried into it from another harness (and out of it): kiln reads
    /// and writes its own record of one.
    pub fn switchable(&self) -> bool {
        matches!(self.protocol, Protocol::ClaudeStreamJson | Protocol::CodexAppServer)
    }

    /// The effort levels a new session can start at, before the harness says which its model has:
    /// Codex's default model; Devin's common levels, safe for a model not learned yet too.
    pub fn efforts(&self) -> Vec<super::model::ControlOption> {
        let levels: Vec<String> = match self.protocol {
            Protocol::ClaudeStreamJson => super::claude::EFFORTS.iter().map(|e| (*e).to_owned()).collect(),
            Protocol::CodexAppServer => super::codex::default_efforts(),
            Protocol::AgyStreamJson => super::antigravity::EFFORTS.iter().map(|e| (*e).to_owned()).collect(),
            Protocol::Acp(_) if self.id == "devin" => common_efforts(&models_of("devin")),
            Protocol::Acp(_) | Protocol::Terminal => Vec::new(),
        };
        levels
            .iter()
            .map(|e| super::model::ControlOption { id: e.clone(), label: super::model::effort_label(e), description: String::new(), dangerous: false })
            .collect()
    }
}

fn common_efforts(models: &[super::model::ModelOption]) -> Vec<String> {
    models.first().map(|first| first.efforts.iter().filter(|e| models.iter().all(|m| m.efforts.contains(e))).cloned().collect()).unwrap_or_default()
}

pub fn offered() -> Vec<KindInfo> {
    #[cfg(not(test))]
    super::acp::warm_models();
    KINDS
        .iter()
        .map(|k| {
            let (modes, default_mode) = k.modes();
            KindInfo {
                id: k.id.to_owned(),
                name: k.name.to_owned(),
                verified: k.verified,
                available: k.available(),
                modes,
                default_mode: default_mode.to_owned(),
                efforts: k.efforts(),
                structured: k.structured(),
                program: k.program.to_owned(),
                models: models_of(k.id),
                switch: k.switchable(),
            }
        })
        .collect()
}

/// The models each harness's sessions on this machine last listed, by harness, and the file
/// they are kept in (so they are known before any session runs, after a restart too).
struct Seen {
    file: Option<std::path::PathBuf>,
    models: std::collections::HashMap<String, Vec<super::model::ModelOption>>,
}

static SEEN: std::sync::LazyLock<parking_lot::Mutex<Seen>> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(Seen { file: None, models: std::collections::HashMap::new() }));

/// Keep the models seen in `file` (and know the ones kept there). Blocking (reads it).
pub fn keep_models_in(file: std::path::PathBuf) {
    let kept: std::collections::HashMap<String, Vec<super::model::ModelOption>> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut g = SEEN.lock();
    for (k, v) in kept {
        g.models.entry(k).or_insert(v);
    }
    g.file = Some(file);
}

/// A session of harness `kind` listed these models: the machine's list for it from now on
/// (written to the file off the caller's thread).
pub fn note_models(kind: &str, models: &[super::model::ModelOption]) {
    if models.is_empty() {
        return;
    }
    let (file, all) = {
        let mut g = SEEN.lock();
        if g.models.get(kind).is_some_and(|m| m.as_slice() == models) {
            return;
        }
        g.models.insert(kind.to_owned(), models.to_vec());
        (g.file.clone(), g.models.clone())
    };
    if let (Some(file), Ok(json)) = (file, serde_json::to_vec_pretty(&all)) {
        crate::rt::spawn(async move {
            let _ = crate::rt::blocking(move || crate::util::write_secret(&file, &json, 0o600)).await;
        });
    }
}

/// The models harness `kind` offers on this machine: as its sessions last listed them, else the
/// ones it is known to have.
pub fn models_of(kind: &str) -> Vec<super::model::ModelOption> {
    if let Some(m) = SEEN.lock().models.get(kind).filter(|m| !m.is_empty()) {
        return m.clone();
    }
    known_models(kind)
}

/// The models harness `kind` is known to have, its default first.
fn known_models(kind: &str) -> Vec<super::model::ModelOption> {
    let known: &[(&str, &str, &[&str])] = match kind {
        // Claude Code's aliases: each the newest of its family.
        "claude" => &[("opus", "Opus", super::claude::EFFORTS), ("sonnet", "Sonnet", super::claude::EFFORTS), ("haiku", "Haiku", &[])],
        "codex" => &[
            ("gpt-6.1-sol", "GPT-6.1-Sol", &["low", "medium", "high", "xhigh", "max", "ultra"]),
            ("gpt-6-sol", "GPT-6-Sol", &["low", "medium", "high", "xhigh", "max", "ultra"]),
            ("gpt-6-astra", "GPT-6-Astra", &["low", "medium", "high", "xhigh", "max", "ultra"]),
            ("gpt-6-luna", "GPT-6-Luna", &["low", "medium", "high", "xhigh", "max"]),
        ],
        _ => &[],
    };
    // The first is the harness's default.
    known
        .iter()
        .enumerate()
        .map(|(i, (id, label, efforts))| super::model::ModelOption {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
            efforts: efforts.iter().map(|e| (*e).to_owned()).collect(),
            default: i == 0,
            ..super::model::ModelOption::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devin_runs_its_acp_protocol_and_keeps_its_terminal_interface() -> anyhow::Result<()> {
        let kind = Kind::get("devin").ok_or_else(|| anyhow::anyhow!("no Devin harness"))?;
        assert_eq!(kind.protocol, Protocol::Acp(&["acp"]));
        assert_eq!(kind.screen, &["devin"]);
        assert!(!kind.screen_takes_command);
        let cwd = tempfile::tempdir()?;
        let spec = Spec {
            kind: "devin".into(),
            program: Some("/path/to/devin".into()),
            cwd: Some(cwd.path().to_string_lossy().into_owned()),
            model: Some("default".into()),
            mode: Some("plan".into()),
            effort: Some("high".into()),
            resume: Some("conversation".into()),
            env: vec![("DEVIN_SESSION_TEST".into(), "kept".into()), ("CLAUDECODE".into(), "scrubbed".into())],
            ..Spec::default()
        };
        let launch = kind.launch(&spec, "session")?;
        assert_eq!(launch.get_program(), "/path/to/devin");
        assert_eq!(launch.get_args().collect::<Vec<_>>(), ["acp"]);
        assert_eq!(launch.get_current_dir(), Some(cwd.path()));
        let env: std::collections::HashMap<_, _> = launch.get_envs().collect();
        assert_eq!(env.get(std::ffi::OsStr::new("DEVIN_SESSION_TEST")), Some(&Some(std::ffi::OsStr::new("kept"))));
        assert_eq!(env.get(std::ffi::OsStr::new("CLAUDECODE")), Some(&None));
        assert_eq!(Kind::get("claude").map(|k| k.protocol), Some(Protocol::ClaudeStreamJson));
        assert_eq!(Kind::get("codex").map(|k| k.protocol), Some(Protocol::CodexAppServer));
        let (modes, default) = kind.modes();
        assert_eq!(default, "accept-edits");
        assert_eq!(modes.iter().map(|m| (m.id.as_str(), m.dangerous)).collect::<Vec<_>>(), [("accept-edits", false), ("smart", false), ("ask", false), ("plan", false), ("bypass", true)]);
        Ok(())
    }

    #[test]
    fn common_efforts_never_offers_a_level_missing_from_another_model() {
        use super::super::model::ModelOption;
        let mut models = vec![
            ModelOption { efforts: vec!["low".into(), "high".into()], ..ModelOption::default() },
            ModelOption { efforts: vec!["medium".into(), "high".into()], ..ModelOption::default() },
        ];
        assert_eq!(common_efforts(&models), ["high"]);
        models.push(ModelOption::default());
        assert!(common_efforts(&models).is_empty());
        assert!(common_efforts(&[]).is_empty());
    }

    #[test]
    fn codex_offers_every_effort_its_default_model_takes() {
        let known = known_models("codex");
        let default = known.iter().find(|m| m.default);
        assert_eq!(default.map(|m| m.id.as_str()), Some("gpt-6.1-sol"));
        assert_eq!(default.map(|m| m.efforts.clone()), Some(["low", "medium", "high", "xhigh", "max", "ultra"].map(str::to_owned).to_vec()));
        let labels: Vec<String> = ["low", "xhigh", "max", "ultra"].iter().map(|e| super::super::model::effort_label(e)).collect();
        assert_eq!(labels, ["Low", "Extra high", "Max", "Ultra"]);
    }

    /// Each adapter, started (its first requests to the harness under way), is written out and read
    /// back whole: what an in-place upgrade hands the next image of the runner.
    #[test]
    fn every_adapter_is_written_out_and_read_back_whole() -> anyhow::Result<()> {
        for id in ["claude", "codex", "gemini", "devin"] {
            let kind = Kind::get(id).ok_or_else(|| anyhow::anyhow!("no kind {id}"))?;
            let mut adapter = kind.adapter(&Spec { kind: id.to_owned(), ..Spec::default() });
            let mut core = Core::new();
            adapter.start(&mut core);
            let out = serde_json::to_value(adapter.freeze())?;
            assert!(out.pointer("/state/pending").and_then(serde_json::Value::as_object).is_some_and(|p| !p.is_empty()), "{id}: nothing under way to carry");
            let back = Adapter::thaw(serde_json::from_value(out.clone())?);
            assert_eq!(serde_json::to_value(back.freeze())?, out, "{id}");
        }
        Ok(())
    }

    /// A session moved from Codex to another harness: what Codex left live (an older image kept
    /// it so) can never be settled by the adapter after it, whichever that is.
    #[test]
    fn whatever_the_harness_what_it_ran_ends_with_its_process_and_a_new_one_inherits_none_of_it() {
        use super::super::model::{Body, Task, ToolCall, ToolKind, ToolStatus};
        let call = |id: &str| {
            Body::Tool(Box::new(ToolCall {
                tool_id: id.into(),
                name: "shell".into(),
                tool: ToolKind::Shell,
                title: "python3 -m http.server".into(),
                detail: None,
                input: serde_json::Value::Null,
                status: ToolStatus::Running,
                output: None,
                truncated: false,
                diffs: Vec::new(),
                todos: Vec::new(),
                task: None,
                exit_code: None,
                started: 0,
                ended: None,
                server: None,
                tool_short: None,
                skill: None,
                runner: None,
            }))
        };
        let row = |id: &str, tool: &str, class: &str| Task { id: id.into(), tool_id: Some(tool.into()), class: Some(class.into()), status: "running".into(), ..Task::default() };
        let mut core = Core::new();
        core.state.tasks = vec![row("terminal-exec-1", "exec-1", "shell"), row("01a15afd", "call_spawn", "agent"), Task { id: "left-77".into(), status: "running".into(), ..Task::default() }];
        core.upsert("exec-1", None, call("exec-1"));
        // An adapter that keeps no tasks of its own (ACP) still ends them with its process.
        let mut acp = Adapter::Acp(Box::new(Acp::new(None)));
        acp.exited(&mut core, Some(0), None);
        assert_eq!(core.state.tasks.iter().map(|t| t.status.as_str()).collect::<Vec<_>>(), ["stopped", "stopped", "running"]);
        assert!(core.get("exec-1").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Cancelled && t.ended.is_some())));
        // Codex's own exit ends its terminals' calls too, not only their rows.
        core.state.tasks.push(row("terminal-exec-2", "exec-2", "shell"));
        core.upsert("exec-2", None, call("exec-2"));
        let mut codex = Adapter::Codex(Box::new(Codex::new(None, None, None, false)));
        codex.exited(&mut core, None, None);
        assert!(core.get("exec-2").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Cancelled)));
        // A new process starts with nothing of an earlier one's still running.
        core.state.tasks.push(row("terminal-exec-3", "exec-3", "shell"));
        core.upsert("exec-3", None, call("exec-3"));
        let mut claude = Adapter::Claude(Box::new(Claude::new(false)));
        claude.start(&mut core);
        assert!(core.state.tasks.iter().filter(|t| t.id != "left-77").all(|t| !t.live()));
        assert!(core.get("exec-3").is_some_and(|e| matches!(&e.body, Body::Tool(t) if t.status == ToolStatus::Cancelled)));
        assert_eq!(core.state.tasks.iter().find(|t| t.id == "left-77").map(|t| t.status.as_str()), Some("running"), "kiln's own, taken back over");
    }
}
