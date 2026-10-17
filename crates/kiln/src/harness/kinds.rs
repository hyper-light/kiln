//! The registry of harnesses kiln can run as structured sessions, and the adapter each one uses.
//! Adding a harness is adding an entry here and, if it speaks a protocol kiln does not know yet,
//! an adapter. `verified` says whether kiln has been tested against a real install of it.

use std::process::Command as Process;

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
}

const KINDS: &[Kind] = &[
    Kind { id: "claude", name: "Claude Code", program: "claude", verified: true, protocol: Protocol::ClaudeStreamJson },
    Kind { id: "codex", name: "Codex", program: "codex", verified: true, protocol: Protocol::CodexAppServer },
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

    pub fn adapter(&self, spec: &Spec) -> Adapter {
        match self.protocol {
            Protocol::ClaudeStreamJson => Adapter::Claude(Box::new(Claude::new(spec.allow_bypass))),
            Protocol::CodexAppServer => Adapter::Codex(Box::new(Codex::new(spec.resume.clone(), spec.model.clone(), spec.mode.clone(), spec.allow_bypass))),
        }
    }

    /// The process to start for `spec` (cwd, environment and arguments).
    pub fn launch(&self, spec: &Spec) -> anyhow::Result<Process> {
        let program = match &spec.program {
            Some(p) => std::path::PathBuf::from(p),
            None => crate::os::which(self.program).ok_or_else(|| anyhow::anyhow!("{} ({}) is not installed on this machine", self.name, self.program))?,
        };
        let mut p = Process::new(program);
        let cwd = spec.cwd.clone().unwrap_or_else(|| crate::os::home_dir().to_string_lossy().into_owned());
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
                p.args(super::claude::args(spec.model.as_deref(), spec.mode.as_deref(), spec.resume.as_deref(), session_id.as_deref(), spec.allow_bypass));
            }
            // Model, mode and resume go over the protocol (thread/start, turn/start).
            Protocol::CodexAppServer => {
                p.args(super::codex::args());
            }
        }
        Ok(p)
    }
}

/// A harness adapter: turns the harness's output into the normalized model and commands into
/// its input. Pure state machines over [`Core`]; the session owns the process.
pub enum Adapter {
    Claude(Box<Claude>),
    Codex(Box<Codex>),
}

impl Adapter {
    pub fn start(&mut self, core: &mut Core) {
        match self {
            Self::Claude(a) => a.start(core),
            Self::Codex(a) => a.start(core),
        }
    }

    pub fn line(&mut self, core: &mut Core, line: &str) {
        match self {
            Self::Claude(a) => a.line(core, line),
            Self::Codex(a) => a.line(core, line),
        }
    }

    pub fn command(&mut self, core: &mut Core, c: Command) {
        match self {
            Self::Claude(a) => a.command(core, c),
            Self::Codex(a) => a.command(core, c),
        }
    }

    pub fn exited(&mut self, core: &mut Core, code: Option<i32>, error: Option<String>) {
        match self {
            Self::Claude(a) => a.exited(core, code, error),
            Self::Codex(a) => a.exited(core, code, error),
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
}

pub fn offered() -> Vec<KindInfo> {
    KINDS.iter().map(|k| KindInfo { id: k.id.to_owned(), name: k.name.to_owned(), verified: k.verified, available: k.available() }).collect()
}
