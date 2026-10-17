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
                // File snapshots before edits, so a rewind can restore them.
                p.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "1");
            }
            // Model, mode and resume go over the protocol (thread/start, turn/start).
            Protocol::CodexAppServer => {
                p.args(super::codex::args());
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

/// The session's environment over the machine's: what its shell had (minus the shell's own
/// terminal plumbing), without what a Claude Code session sets for its children.
fn set_env(p: &mut Process, spec: &Spec) {
    for (k, v) in &spec.env {
        if !matches!(k.as_str(), "KILN_TERM_SESSION" | "KILN_HARNESS_SOCK" | "TERM" | "COLUMNS" | "LINES") {
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

impl Adapter {
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
        match self {
            Self::Claude(a) => a.run_done(core, job, code, output),
            Self::Codex(a) => a.run_done(core, job, code, output),
            Self::Acp(a) => a.run_done(core, job, code, output),
        }
    }

    /// Every few seconds while the process runs: a chance to poll what the harness only reports
    /// when asked (plan usage, servers still connecting).
    pub fn tick(&mut self, core: &mut Core) {
        match self {
            Self::Claude(a) => a.tick(core),
            Self::Codex(a) => a.tick(core),
            Self::Acp(_) => {}
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
    /// Its permission modes (the ones that skip every check are left out). ACP agents announce
    /// theirs per session, so theirs are empty here.
    #[serde(default)]
    pub modes: Vec<super::model::ModeOption>,
    #[serde(default)]
    pub default_mode: String,
    /// kiln runs it as a structured session; false: it runs in a kiln terminal (`program`).
    #[serde(default = "yes")]
    pub structured: bool,
    /// The executable (what a terminal runs for one that is not structured).
    #[serde(default)]
    pub program: String,
}

fn yes() -> bool {
    true
}

impl Kind {
    /// Its permission modes, and the one a new session starts in.
    pub fn modes(&self) -> (Vec<super::model::ModeOption>, &'static str) {
        match self.protocol {
            Protocol::ClaudeStreamJson => (super::claude::modes(false), "default"),
            Protocol::CodexAppServer => (super::codex::modes(false), "auto"),
            Protocol::Acp(_) | Protocol::Terminal => (Vec::new(), ""),
        }
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
                structured: k.structured(),
                program: k.program.to_owned(),
            }
        })
        .collect()
}
