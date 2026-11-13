//! Where the node agent's socket isn't available: a client that can't be made, so every call of it is
//! unreachable by construction, and [`Client::connect`]'s refusal names the platform (CLAUDE.md §12).

use std::path::Path;

use hex_api::{
    BoxId, BoxInfo, BoxSpec, Class, Proc, Program, Refusal, Runtime, Signal, Stdio, Usage,
};
use hex_core::netns;

/// A client of the node agent's socket, which this platform can't make.
pub struct Client {
    never: Never,
}

enum Never {}

impl Client {
    /// Refused: the socket passes descriptors over `SOCK_SEQPACKET` (unix(7)), which this platform's
    /// local sockets lack.
    pub fn connect(path: &Path) -> Result<Self, Refusal> {
        Err(Refusal::Platform {
            what: "the node agent's socket",
            platform: std::env::consts::OS,
            why: format!(
                "{}: it passes descriptors over SOCK_SEQPACKET (unix(7)), which this platform's local sockets lack",
                path.display()
            ),
        })
    }

    /// The agent's network namespace counts: unreachable, as every call of a client that can't be made.
    pub fn netns(&self) -> Result<netns::Counts, Refusal> {
        match self.never {}
    }
}

impl Runtime for Client {
    fn classes(&self) -> Vec<Class> {
        match self.never {}
    }

    fn create(&self, _spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        match self.never {}
    }

    fn start(&self, _id: &BoxId) -> Result<(), Refusal> {
        match self.never {}
    }

    fn exec(
        &self,
        _id: &BoxId,
        _program: Program,
        _stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        match self.never {}
    }

    fn run(
        &self,
        _spec: BoxSpec,
        _program: Program,
        _stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        match self.never {}
    }

    fn signal(&self, _id: &BoxId, _signal: Signal) -> Result<(), Refusal> {
        match self.never {}
    }

    fn inspect(&self, _id: &BoxId) -> Result<BoxInfo, Refusal> {
        match self.never {}
    }

    fn usage(&self, _id: &BoxId) -> Result<Usage, Refusal> {
        match self.never {}
    }

    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        match self.never {}
    }

    fn delete(&self, _id: &BoxId) -> Result<(), Refusal> {
        match self.never {}
    }
}
