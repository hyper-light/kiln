//! Where the node's socket has no event sources yet: an [`Os`] that can't be made, so [`super::Server::bind`]
//! is refused naming the platform (CLAUDE.md §12) and every method is unreachable by construction. macOS's
//! (kqueue) and Windows' (an I/O completion port) replace it (native plan step 8).

use std::path::Path;
use std::time::Duration;

use hex_api::{Refusal, Stdio};
use hex_core::wire::Streams;

use super::{Accepted, Event, Interest, Os, Peer, Recv, Sent, Unwatched};

/// An OS whose node socket isn't built yet.
pub struct Unsupported {
    never: Never,
}

/// What can't be made.
pub enum Never {}

impl Unsupported {
    /// Refused: this OS's event sources aren't built yet.
    pub fn bind(path: &Path) -> Result<Self, Refusal> {
        Err(Refusal::Platform {
            what: "the node agent's socket",
            platform: std::env::consts::OS,
            why: format!(
                "{}: its event sources and local socket aren't built for this OS yet (native plan step 8)",
                path.display()
            ),
        })
    }
}

impl Os for Unsupported {
    type Conn = Never;
    type Watch = Never;
    type Passed = Never;

    fn wait(&self, _: &mut Vec<Event>, _: Option<Duration>) -> Result<(), Refusal> {
        match self.never {}
    }
    fn wake(&self) -> Result<(), Refusal> {
        match self.never {}
    }
    fn accept(&self) -> Accepted<Never> {
        match self.never {}
    }
    fn accepting(&self, _: bool) -> Result<(), Refusal> {
        match self.never {}
    }
    fn stop_listening(&self) {
        match self.never {}
    }
    fn peer(&self, conn: &Never) -> Peer {
        match *conn {}
    }
    fn watch(&self, conn: &Never, _: u64, _: Interest) -> Result<(), Refusal> {
        match *conn {}
    }
    fn rewatch(&self, conn: &Never, _: u64, _: Interest) -> Result<(), Refusal> {
        match *conn {}
    }
    fn unwatch(&self, conn: &Never) {
        match *conn {}
    }
    fn recv(&self, conn: &Never, _: &mut [u8]) -> Recv<Never> {
        match *conn {}
    }
    fn send(&self, conn: &Never, _: &[u8], _: &[hex_api::End]) -> Sent {
        match *conn {}
    }
    fn shutdown(&self, conn: &Never) {
        match *conn {}
    }
    fn hung_up(&self, conn: &Never) -> bool {
        match *conn {}
    }
    fn watch_program(&self, _: u32, _: u64) -> Result<Never, Unwatched> {
        match self.never {}
    }
    fn unwatch_program(&self, watch: Never) {
        match watch {}
    }
    fn stdio(&self, _: Streams, passed: Vec<Never>) -> Result<Stdio, Refusal> {
        match passed.into_iter().next() {
            Some(never) => match never {},
            None => match self.never {},
        }
    }

    fn watch_source(&self, _: super::Handle<'_>, _: u64, _: bool) -> Result<(), Refusal> {
        match self.never {}
    }

    fn unwatch_source(&self, _: super::Handle<'_>, _: u64) {
        match self.never {}
    }
}
