//! The registry of harnesses kiln can run as structured sessions, and the adapter each one uses.
//! Adding a harness is adding an entry here and, if it speaks a protocol kiln does not know yet,
//! an adapter. `verified` says whether kiln has been tested against a real install of it.

use std::process::Command as Process;

use super::acp::Acp;
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
    /// No structured mode: it runs as a terminal program, in a kiln terminal.
    Terminal,
}

const KINDS: &[Kind] = &[
    Kind { id: "claude", name: "Claude Code", program: "claude", verified: true, protocol: Protocol::ClaudeStreamJson, screen: &["claude"], screen_takes_command: true },
    Kind { id: "codex", name: "Codex", program: "codex", verified: true, protocol: Protocol::CodexAppServer, screen: &["codex"], screen_takes_command: false },
    Kind { id: "gemini", name: "Gemini CLI", program: "gemini", verified: false, protocol: Protocol::Acp(&["--acp"]), screen: &["gemini"], screen_takes_command: false },
    Kind { id: "opencode", name: "opencode", program: "opencode", verified: false, protocol: Protocol::Acp(&["acp"]), screen: &["opencode"], screen_takes_command: false },
    Kind { id: "goose", name: "Goose", program: "goose", verified: false, protocol: Protocol::Acp(&["acp"]), screen: &["goose", "session"], screen_takes_command: false },
    // Amp has no ACP mode of its own; its ACP adapter (amp-acp) wraps the signed-in Amp CLI.
    Kind { id: "amp", name: "Amp", program: "amp-acp", verified: false, protocol: Protocol::Acp(&[]), screen: &["amp"], screen_takes_command: false },
    // Antigravity's CLI has no documented structured mode: it runs in a terminal.
    Kind { id: "antigravity", name: "Antigravity CLI", program: "agy", verified: false, protocol: Protocol::Terminal, screen: &["agy"], screen_takes_command: false },
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
            Protocol::Acp(_) | Protocol::Terminal => Adapter::Acp(Box::new(Acp::new(spec.resume.clone()).with_mode(spec.mode.as_deref()))),
        }
    }

    /// The process to start for `spec` (cwd, environment and arguments).
    pub fn launch(&self, spec: &Spec) -> anyhow::Result<Process> {
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
                // kiln's MCP server (code mode, deployments) and skills, where this machine's kiln
                // serves them.
                if let Some(kit) = crate::mcp::kit() {
                    p.arg("--mcp-config").arg(&kit.claude_mcp_config);
                    p.arg("--plugin-dir").arg(&kit.plugin_dir);
                }
                // Reach: commands on other runners, where this runner reaches them.
                #[cfg(unix)]
                if let Some((kiln, sock, prefer)) = reach_tools(spec) {
                    p.arg("--mcp-config").arg(crate::reach::mcp::claude_config(&kiln, sock, prefer));
                }
                // File snapshots before edits, so a rewind can restore them.
                p.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "1");
            }
            // Model, mode and resume go over the protocol (thread/start, turn/start).
            Protocol::CodexAppServer => {
                p.args(super::codex::args());
                #[cfg(unix)]
                if let Some((kiln, sock, prefer)) = reach_tools(spec) {
                    p.args(crate::reach::mcp::codex_config(&kiln, sock, prefer));
                }
            }
            Protocol::Acp(args) => {
                p.args(args);
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

/// Whether a session gets reach's tools, and how to start them: this kiln binary, the runner's
/// reach socket, the session's preferred runner.
#[cfg(unix)]
fn reach_tools(spec: &Spec) -> Option<(std::path::PathBuf, &'static str, Option<&str>)> {
    let pref = spec.reach.as_deref().map(str::trim).filter(|r| !r.is_empty());
    if pref == Some("off") {
        return None;
    }
    let sock = crate::reach::local::socket()?;
    let kiln = std::env::current_exe().ok()?;
    Some((kiln, sock, pref))
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
}

/// An adapter written out whole, for this process's next image (an in-place upgrade).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "adapter", content = "state", rename_all = "snake_case")]
pub enum Frozen {
    Claude(Box<Claude>),
    Codex(Box<Codex>),
    Acp(Box<Acp>),
}

impl Adapter {
    /// Its state, whole, to go on with in the next image ([`Adapter::thaw`]).
    pub fn freeze(self) -> Frozen {
        match self {
            Self::Claude(a) => Frozen::Claude(a),
            Self::Codex(a) => Frozen::Codex(a),
            Self::Acp(a) => Frozen::Acp(a),
        }
    }

    pub fn thaw(f: Frozen) -> Self {
        match f {
            Frozen::Claude(a) => Self::Claude(a),
            Frozen::Codex(a) => Self::Codex(a),
            Frozen::Acp(a) => Self::Acp(a),
        }
    }

    pub fn start(&mut self, core: &mut Core) {
        match self {
            Self::Claude(a) => a.start(core),
            Self::Codex(a) => a.start(core),
            Self::Acp(a) => a.start(core),
        }
    }

    pub fn line(&mut self, core: &mut Core, line: &str) {
        match self {
            Self::Claude(a) => a.line(core, line),
            Self::Codex(a) => a.line(core, line),
            Self::Acp(a) => a.line(core, line),
        }
    }

    pub fn command(&mut self, core: &mut Core, c: Command) {
        match self {
            Self::Claude(a) => a.command(core, c),
            Self::Codex(a) => a.command(core, c),
            Self::Acp(a) => a.command(core, c),
        }
    }

    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        match self {
            Self::Claude(a) => a.exited(core, code, error),
            Self::Codex(a) => a.exited(core, code, error),
            Self::Acp(a) => a.exited(core, code, error),
        }
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
            Self::Acp(_) => {}
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
        }
    }

    /// A pause ended (it reset, or a person resumed it): the turn it cut off goes on, then kiln's
    /// queue.
    pub fn go_on(&mut self, core: &mut Core, p: super::model::Pause) {
        match self {
            Self::Claude(a) => a.go_on(core, p),
            Self::Codex(a) => a.go_on(core, p),
            Self::Acp(a) => a.go_on(core, p),
        }
    }

    /// Start with a conversation carried over from another harness, in the harness's own form
    /// (Codex: Responses items for its new thread; Claude Code reads its own from a transcript).
    pub fn seed(&mut self, items: Vec<serde_json::Value>) {
        if let Self::Codex(a) = self {
            a.seed(items);
        }
    }

    /// That conversation is in (or there was none).
    pub fn seeded(&self) -> bool {
        match self {
            Self::Codex(a) => a.seeded(),
            Self::Claude(_) | Self::Acp(_) => true,
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
    /// a confirmation, never a default). ACP agents announce theirs per session, so theirs are
    /// empty here.
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

/// "xhigh" → "Xhigh", as the adapters name their levels.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
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
            Protocol::Acp(_) | Protocol::Terminal => (Vec::new(), ""),
        }
    }

    /// A conversation can be carried into it from another harness (and out of it): kiln reads
    /// and writes its own record of one.
    pub fn switchable(&self) -> bool {
        matches!(self.protocol, Protocol::ClaudeStreamJson | Protocol::CodexAppServer)
    }

    /// The effort levels a new session can start at, before the harness says which its model has.
    pub fn efforts(&self) -> Vec<super::model::ControlOption> {
        let levels: &[&str] = match self.protocol {
            Protocol::ClaudeStreamJson => super::claude::EFFORTS,
            Protocol::CodexAppServer => super::codex::EFFORTS,
            Protocol::Acp(_) | Protocol::Terminal => &[],
        };
        levels
            .iter()
            .map(|e| super::model::ControlOption { id: (*e).to_owned(), label: capitalize(e), description: String::new(), dangerous: false })
            .collect()
    }
}

pub fn offered() -> Vec<KindInfo> {
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
    known
        .iter()
        .map(|(id, label, efforts)| super::model::ModelOption { id: (*id).to_owned(), label: (*label).to_owned(), description: String::new(), efforts: efforts.iter().map(|e| (*e).to_owned()).collect() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each adapter, started (its first requests to the harness under way), is written out and read
    /// back whole: what an in-place upgrade hands the next image of the runner.
    #[test]
    fn every_adapter_is_written_out_and_read_back_whole() -> anyhow::Result<()> {
        for id in ["claude", "codex", "gemini"] {
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
}
