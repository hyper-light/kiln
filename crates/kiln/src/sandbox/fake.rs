//! A stand-in for the machine's microVMs in tests (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step
//! 17): images whose members are programs of this machine, each launch kept as it was asked, its
//! member run as the seam's host implementation runs a program. What it stands in for is Shards'
//! image boot through Hex; what it holds a session to is the seam's contract alone.

use std::path::{Path, PathBuf};

use futures::future::BoxFuture;
use kiln_seams::sandbox::{Boundary, Egress, Holds, Launch, Limits, Refusal, Run, Sandbox};
use parking_lot::Mutex;

use super::host::HostSandbox;

/// Images, by name, of one member each, which runs `program`.
pub struct FakeVms {
    images: Vec<(String, String, PathBuf)>,
    launched: Mutex<Vec<Launch>>,
}

impl FakeVms {
    /// One image, `image`, whose member `member` runs `program`.
    pub fn with(image: &str, member: &str, program: &Path) -> Self {
        Self { images: vec![(image.to_owned(), member.to_owned(), program.to_path_buf())], launched: Mutex::new(Vec::new()) }
    }

    /// Every launch asked of it, in order.
    pub fn launched(&self) -> Vec<Launch> {
        self.launched.lock().clone()
    }
}

impl Sandbox for FakeVms {
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        Box::pin(async move {
            self.launched.lock().push(launch.clone());
            let Boundary::MicroVm { image } = &launch.boundary else { return Err(Refusal::Unsupported("a run outside a microVM".into())) };
            let program = self.images.iter().find(|(i, m, _)| i == image && *m == launch.program).map(|(_, _, p)| p.to_string_lossy().into_owned()).ok_or_else(|| Refusal::NoSuchProgram(format!("{image}'s {}", launch.program)))?;
            HostSandbox.launch(Launch { program, shares: Vec::new(), limits: Limits::NONE, egress: Egress::Any, boundary: Boundary::Host, ..launch }).await
        })
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["microvm"]
    }

    fn holds(&self) -> Holds {
        super::host::HOLDS
    }
}
