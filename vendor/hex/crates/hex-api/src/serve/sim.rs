//! [`SimServers`]: the serve seam's simulator (CLAUDE.md §9). The sources it serves are names its test gives it
//! ([`SimServers::serve`]); each stream it makes is one end of a socket pair (Unix) or a pipe (Windows), on whose other
//! end it writes the stream's number and then lets that end go, so a probe ([`SimServers::probe`]) reads back which ask
//! made the stream it is given (for which box, of which source, and whether read-only) and then reads its end: a server
//! that answers once and hangs up, holding no descriptor of the process past the ask.

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex, MutexGuard};

use super::{Servers, Stream};
use crate::{BoxId, Refusal, ServedMount};

/// Most streams a [`SimServers`] makes, every box's together; past it an ask is refused `Exhausted` (the simulator's own
/// bound on its records, as [`crate::lease::sim::SIM_LEASES`] is the lease simulator's).
pub const SIM_STREAMS: usize = crate::lease::sim::SIM_LEASES;

/// Servers in memory, over the sources the test names ([`SimServers::serve`]).
#[derive(Clone, Default)]
pub struct SimServers {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    sources: BTreeSet<String>,
    /// Each stream made, in the order asked: its number is its place here.
    made: Vec<Made>,
}

/// A stream the simulator made: for which box, of which source, and whether read-only.
struct Made {
    id: String,
    source: String,
    read_only: bool,
}

/// The end the simulator writes a stream's number on, and then lets go.
#[cfg(unix)]
type Ours = std::os::unix::net::UnixStream;
/// The end the simulator writes a stream's number on, and then lets go.
#[cfg(windows)]
type Ours = std::io::PipeWriter;

/// What a probe of a simulated stream reads back: the ask that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    pub id: String,
    pub source: String,
    pub read_only: bool,
}

impl SimServers {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, Refusal> {
        self.state
            .lock()
            .map_err(|_| Refusal::Broken("the serve simulator's lock is poisoned".to_owned()))
    }

    /// Serves `source` from now.
    pub fn serve(&self, source: &str) -> Result<(), Refusal> {
        self.state()?.sources.insert(source.to_owned());
        Ok(())
    }

    /// How many streams it has made, every box's together.
    pub fn made(&self) -> Result<usize, Refusal> {
        Ok(self.state()?.made.len())
    }

    /// The ask that made `stream`, one of this simulator's, as the number written first on it says; read once of a
    /// stream, its number being its first bytes alone. Refused `Invalid` where no number of a stream it made comes.
    pub fn probe(&self, stream: &Stream) -> Result<Probed, Refusal> {
        let number = read_number(stream)?;
        let state = self.state()?;
        let made = state.made.get(number).ok_or_else(|| Refusal::Invalid {
            what: "a probed stream",
            why: format!("number {number} names no stream the simulator made"),
        })?;
        Ok(Probed {
            id: made.id.clone(),
            source: made.source.clone(),
            read_only: made.read_only,
        })
    }
}

impl Servers for SimServers {
    fn stream(&self, id: &BoxId, share: &ServedMount) -> Result<Stream, Refusal> {
        let mut state = self.state()?;
        if !state.sources.contains(&share.source) {
            return Err(Refusal::Invalid {
                what: "a served share",
                why: format!("the simulator serves no source {:?}", share.source),
            });
        }
        if state.made.len() >= SIM_STREAMS {
            return Err(Refusal::Exhausted {
                what: "simulated served streams",
                bound: SIM_STREAMS,
            });
        }
        let number = state.made.len();
        let (stream, mut ours) = pair()?;
        ours.write_all(format!("{number}\n").as_bytes())
            .map_err(|e| Refusal::os("writing a simulated stream's number", e))?;
        drop(ours);
        state.made.push(Made {
            id: id.to_string(),
            source: share.source.clone(),
            read_only: share.read_only,
        });
        Ok(stream)
    }
}

/// A stream and the simulator's end of it.
#[cfg(unix)]
fn pair() -> Result<(Stream, Ours), Refusal> {
    let (theirs, ours) = std::os::unix::net::UnixStream::pair()
        .map_err(|e| Refusal::os("making a simulated stream", e))?;
    Ok((theirs.into(), ours))
}

/// A stream and the simulator's end of it: a pipe, whose reading end is the stream a probe reads.
#[cfg(windows)]
fn pair() -> Result<(Stream, Ours), Refusal> {
    let (theirs, ours) =
        std::io::pipe().map_err(|e| Refusal::os("making a simulated stream", e))?;
    Ok((theirs.into(), ours))
}

/// The number written first on `stream`: its decimal digits up to its line's end, read a byte at a time from a copy of
/// it, no more digits than a `usize` has.
fn read_number(stream: &Stream) -> Result<usize, Refusal> {
    let copy = stream
        .try_clone()
        .map_err(|e| Refusal::os("copying a probed stream", e))?;
    let mut reader = std::fs::File::from(copy);
    let most = usize::MAX.to_string().len();
    let mut digits = String::with_capacity(most);
    loop {
        let mut byte = [0u8; 1];
        reader
            .read_exact(&mut byte)
            .map_err(|e| Refusal::os("reading a probed stream's number", e))?;
        match byte {
            [b'\n'] => {
                return digits.parse().map_err(|_| Refusal::Invalid {
                    what: "a probed stream",
                    why: format!("{digits:?} is no stream's number"),
                });
            }
            [digit] if digit.is_ascii_digit() && digits.len() < most => {
                digits.push(char::from(digit))
            }
            [other] => {
                return Err(Refusal::Invalid {
                    what: "a probed stream",
                    why: format!(
                        "byte {other:#04x} past its number's {} digits at most, or no digit",
                        most
                    ),
                });
            }
        }
    }
}
