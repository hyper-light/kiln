//! A stand-in for the machine's microVMs in tests (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step
//! 17): images whose members are programs of this machine, each launch kept as it was asked, its
//! member run as the seam's host implementation runs a program, named as a microVM of its own and its
//! stdio said to ride the link its runs are given. What it stands in for is Shards' image boot through
//! Hex; what it holds a session to is the seam's contract alone.

use std::path::{Path, PathBuf};

use futures::future::BoxFuture;
use kiln_seams::sandbox::{Boundary, Egress, Holds, Launch, Limits, Link, Output, Refusal, Run, Sandbox, Signal};
use parking_lot::Mutex;

use super::host::HostSandbox;

/// Images, by name, of one member each, which runs `program`.
pub struct FakeVms {
    images: Vec<(String, String, PathBuf)>,
    launched: Mutex<Vec<Launch>>,
    /// The link each run's stdio is said to ride, once it's started.
    link: Option<Link>,
}

impl FakeVms {
    /// One image, `image`, whose member `member` runs `program`.
    pub fn with(image: &str, member: &str, program: &Path) -> Self {
        Self { images: vec![(image.to_owned(), member.to_owned(), program.to_path_buf())], launched: Mutex::new(Vec::new()), link: None }
    }

    /// Its runs' stdio said to ride `link`.
    pub fn linked(self, link: Link) -> Self {
        Self { link: Some(link), ..self }
    }

    /// Every launch asked of it, in order.
    pub fn launched(&self) -> Vec<Launch> {
        self.launched.lock().clone()
    }
}

impl Sandbox for FakeVms {
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        Box::pin(async move {
            let n = {
                let mut launched = self.launched.lock();
                launched.push(launch.clone());
                launched.len()
            };
            let Boundary::MicroVm { image } = &launch.boundary else { return Err(Refusal::Unsupported("a run outside a microVM".into())) };
            let program = self.images.iter().find(|(i, m, _)| i == image && *m == launch.program).map(|(_, _, p)| p.to_string_lossy().into_owned()).ok_or_else(|| Refusal::NoSuchProgram(format!("{image}'s {}", launch.program)))?;
            let run = HostSandbox.launch(Launch { program, shares: Vec::new(), limits: Limits::NONE, egress: Egress::Any, boundary: Boundary::Host, ..launch }).await?;
            Ok(Box::new(FakeRun { run, id: format!("fake-vm-{n}"), link: self.link.clone() }) as Box<dyn Run>)
        })
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["microvm"]
    }

    fn holds(&self) -> Holds {
        super::host::HOLDS
    }
}

/// A member's run in its stand-in microVM: the program's run, named as the microVM it stands in for.
struct FakeRun {
    run: Box<dyn Run>,
    id: String,
    link: Option<Link>,
}

impl Run for FakeRun {
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>> {
        self.run.stdin(bytes)
    }

    fn close_stdin(&self) {
        self.run.close_stdin();
    }

    fn next(&self) -> BoxFuture<'_, Option<Output>> {
        self.run.next()
    }

    fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        self.run.signal(signal)
    }

    fn kill(&self) {
        self.run.kill();
    }

    fn id(&self) -> Option<String> {
        Some(self.id.clone())
    }

    fn link(&self) -> BoxFuture<'_, Option<Link>> {
        let link = self.link.clone();
        Box::pin(async move { link })
    }
}
