//! The host's sandbox (CP-T4.1; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 17): a program as
//! kiln has always run one, a process on the machine as the runner's user, in its folder, with the
//! machine's environment under its own. It holds none of the seam's limits: a launch that asks for a
//! memory or process limit, a share other than an even one, egress short of any host, a folder
//! shared at another path or read-only, or a credential to fill in at its egress (D242: a process on
//! the machine has no egress outside itself) is refused `Unsupported`, typed, never taken and ignored
//! (on Linux, CP-T7.1's process boxes are what will hold them). Signals reach its process group on
//! Unix.

use std::path::{Path, PathBuf};
use std::process::Command;

use futures::future::BoxFuture;
use kiln_seams::sandbox::{Boundary, Holds, Launch, Refusal, Run, Sandbox};

use super::ProcRun;

/// What the host holds a run to: nothing of the seam's but signals, where its processes take them.
pub const HOLDS: Holds = Holds { memory: false, processes: false, weights: false, no_egress: false, egress_by_host: false, shares: false, signals: cfg!(unix), credentials: false };

/// The host's sandbox.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostSandbox;

impl HostSandbox {
    /// The process `launch` is: its program (a path, else found on `PATH`), its arguments, its
    /// folder as its working directory, and its environment over the machine's.
    pub fn command(launch: &Launch) -> Result<Command, Refusal> {
        HOLDS.check(launch)?;
        match &launch.boundary {
            Boundary::Host => {}
            Boundary::Container { .. } => return Err(Refusal::Unsupported("a run in a container".into())),
            Boundary::MicroVm { .. } => return Err(Refusal::Unsupported("a run in a microVM".into())),
        }
        let mut cmd = Command::new(program(&launch.program)?);
        cmd.args(&launch.args);
        if let Some(folder) = &launch.folder {
            if !Path::new(folder).is_dir() {
                return Err(Refusal::Invalid { what: "its folder", why: format!("{folder} is not a directory on this machine") });
            }
            cmd.current_dir(folder);
        }
        for (k, v) in &launch.env {
            cmd.env(k, v);
        }
        Ok(cmd)
    }
}

/// `p` as a program to run: a path names one, a bare name is looked up on `PATH`.
fn program(p: &str) -> Result<PathBuf, Refusal> {
    let path = Path::new(p);
    if path.is_absolute() || path.components().count() > 1 {
        return if path.is_file() { Ok(path.to_path_buf()) } else { Err(Refusal::NoSuchProgram(p.to_owned())) };
    }
    crate::os::which(p).ok_or_else(|| Refusal::NoSuchProgram(p.to_owned()))
}

impl Sandbox for HostSandbox {
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        Box::pin(async move {
            let cmd = Self::command(&launch)?;
            let proc = crate::proc::spawn_duplex(cmd).map_err(|e| Refusal::Invalid { what: "its process", why: format!("{e:#}") })?;
            let run: Box<dyn Run> = Box::new(ProcRun::new(proc));
            Ok(run)
        })
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["host"]
    }

    fn holds(&self) -> Holds {
        HOLDS
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::net::TcpListener;
    use std::time::Duration;

    use kiln_seams::sandbox::{CredentialGrant, Egress, Limits, SandboxFixture, Share, cases};

    use super::*;

    /// The host's fixture: the machine's own programs, a folder of its own, and two listeners of its
    /// own standing for the suite's hosts.
    struct Fixture {
        sandbox: HostSandbox,
        folder: Option<tempfile::TempDir>,
        hosts: Vec<(&'static str, Option<TcpListener>)>,
    }

    impl Fixture {
        fn new() -> Self {
            let listen = || TcpListener::bind("127.0.0.1:0").ok();
            Self { sandbox: HostSandbox, folder: tempfile::tempdir().ok(), hosts: vec![("api.anthropic.com", listen()), ("example.com", listen())] }
        }

        fn sh(&self, script: &str, args: &[&str]) -> Launch {
            let mut all = vec!["-c", script, "sh"];
            all.extend_from_slice(args);
            Launch { limits: Limits::NONE, egress: Egress::Any, ..Launch::of("sh", &all) }
        }
    }

    impl SandboxFixture for Fixture {
        fn sandbox(&self) -> &dyn Sandbox {
            &self.sandbox
        }
        fn say(&self, text: &str) -> Launch {
            self.sh("printf '%s\\n' \"$1\"", &[text])
        }
        fn echo(&self) -> Launch {
            self.sh("exec cat", &[])
        }
        fn fail(&self, code: i32) -> Launch {
            self.sh("echo \"failing with $1\" >&2; exit \"$1\"", &[&code.to_string()])
        }
        fn wait(&self) -> Launch {
            self.sh("exec sleep 3600", &[])
        }
        fn hog(&self, bytes: u64, limit: Option<u64>) -> Launch {
            // dd holds a block of `bs` bytes.
            let l = self.sh("exec dd if=/dev/zero of=/dev/null bs=\"$1\" count=1", &[&bytes.to_string()]);
            Launch { limits: Limits { memory_bytes: limit, ..Limits::NONE }, ..l }
        }
        fn spawn(&self, n: u32, limit: Option<u32>) -> Launch {
            let l = self.sh("i=0; while [ \"$i\" -lt \"$1\" ]; do sleep 0 & i=$((i+1)); done; wait", &[&n.to_string()]);
            Launch { limits: Limits { processes: limit, ..Limits::NONE }, ..l }
        }
        fn connect(&self, host: &str, egress: Egress) -> Launch {
            let port = self.hosts.iter().find(|(h, _)| *h == host).and_then(|(_, l)| l.as_ref()?.local_addr().ok()).map_or(0, |a| a.port());
            Launch { limits: Limits::NONE, egress, ..Launch::of("bash", &["-c", "exec 3<>\"/dev/tcp/127.0.0.1/$1\"", "bash", &port.to_string()]) }
        }
        // The host fills in no credential, so a launch asking it to is refused before it runs.
        fn fill(&self, _host: &str, _reference: &str, credentials: Vec<CredentialGrant>, egress: Egress) -> Launch {
            Launch { limits: Limits::NONE, egress, credentials, ..Launch::of("true", &[]) }
        }
        fn pwd(&self, folder: &str) -> Launch {
            Launch { folder: Some(folder.to_owned()), ..self.sh("exec pwd -P", &[]) }
        }
        fn folder(&self) -> String {
            let Some(dir) = &self.folder else { return "/nonexistent-folder".into() };
            std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf()).to_string_lossy().into_owned()
        }
    }

    /// CP-T4.1 (AC-T4.1.1), step 17: the host's sandbox passes the Sandbox suite: stdio, ends,
    /// kills, signals, programs and folders as the seam says, and every limit and grant it doesn't
    /// hold refused `Unsupported`.
    #[test]
    fn the_host_sandbox_passes_the_sandbox_suite() -> anyhow::Result<()> {
        let (report, all) = crate::rt::block_on(async {
            let cases = cases::<Fixture>();
            let report = kiln_seams::conformance::run_within("Sandbox (host)", &cases, || async { Fixture::new() }, || crate::rt::sleep(Duration::from_secs(30))).await;
            anyhow::Ok((report, cases.len()))
        })?;
        let passed = report.into_result().map_err(anyhow::Error::msg)?;
        assert_eq!(passed, all);
        Ok(())
    }

    /// CP-T4.1: a launch the host can't hold is refused before anything starts, naming what it asked.
    #[test]
    fn the_host_refuses_a_microvm_a_limit_and_a_moved_share_by_name() {
        let vm = Launch { boundary: Boundary::MicroVm { image: "x".into() }, limits: Limits::NONE, egress: Egress::Any, ..Launch::of("true", &[]) };
        assert_eq!(HostSandbox::command(&vm).err(), Some(Refusal::Unsupported("a run in a microVM".into())));
        let limited = Launch { egress: Egress::Any, ..Launch::of("true", &[]) };
        assert_eq!(HostSandbox::command(&limited).err(), Some(Refusal::Unsupported("a memory limit".into())));
        let moved = Launch { shares: vec![Share { at: "/workspace".into(), ..Share::at_its_path("/tmp") }], limits: Limits::NONE, egress: Egress::Any, ..Launch::of("true", &[]) };
        assert_eq!(HostSandbox::command(&moved).err(), Some(Refusal::Unsupported("a folder shared at another path or read-only".into())));
        let missing = Launch { limits: Limits::NONE, egress: Egress::Any, ..Launch::of("/nonexistent/program", &[]) };
        assert_eq!(HostSandbox::command(&missing).err(), Some(Refusal::NoSuchProgram("/nonexistent/program".into())));
    }
}
