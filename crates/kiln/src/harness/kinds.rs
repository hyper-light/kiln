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
    Kind { id: "claude", name: "Claude Code", program: "claude", verified: true, protocol: Protocol::ClaudeStreamJson },
    Kind { id: "codex", name: "Codex", program: "codex", verified: true, protocol: Protocol::CodexAppServer },
    Kind { id: "gemini", name: "Gemini CLI", program: "gemini", verified: false, protocol: Protocol::Acp(&["--acp"]) },
    Kind { id: "opencode", name: "opencode", program: "opencode", verified: false, protocol: Protocol::Acp(&["acp"]) },
    Kind { id: "goose", name: "Goose", program: "goose", verified: false, protocol: Protocol::Acp(&["acp"]) },
    // Amp has no ACP mode of its own; its ACP adapter (amp-acp) wraps the signed-in Amp CLI.
    Kind { id: "amp", name: "Amp", program: "amp-acp", verified: false, protocol: Protocol::Acp(&[]) },
    // Antigravity's CLI has no documented structured mode: it runs in a terminal.
    Kind { id: "antigravity", name: "Antigravity CLI", program: "agy", verified: false, protocol: Protocol::Terminal },
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
            Protocol::ClaudeStreamJson => Adapter::Claude(Box::new(Claude::new(spec.allow_bypass))),
            Protocol::CodexAppServer => Adapter::Codex(Box::new(Codex::from_spec(spec))),
            Protocol::Acp(_) | Protocol::Terminal => Adapter::Acp(Box::new(Acp::new(spec.resume.clone()))),
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
        for (k, v) in &spec.env {
            // The shell's own terminal plumbing stays out of the agent's environment.
            if !matches!(k.as_str(), "KILN_TERM_SESSION" | "KILN_HARNESS_SOCK" | "TERM" | "COLUMNS" | "LINES") {
                p.env(k, v);
            }
        }
        for k in super::claude::SCRUB_ENV {
            p.env_remove(k);
        }
        match self.protocol {
            Protocol::ClaudeStreamJson => {
                let session_id = spec.resume.is_none().then(|| uuid::Uuid::now_v7().to_string());
                p.args(super::claude::args(spec, session_id.as_deref()));
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
