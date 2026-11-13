//! The node's client to the macOS box helper (native plan step 2). The node agent runs unprivileged; to
//! run a box's programs as a pool user, arm their physical-footprint ledger limits and catch their Mach
//! exceptions it reaches the privileged helper (a launchd daemon) over a Unix `SOCK_STREAM` socket, with
//! the typed, bounded protocol of [`hex_core::helper`]. macOS local sockets have no `SOCK_SEQPACKET`, so
//! each message is framed with a `u32` length; a `Start`'s three stdio descriptors ride with it by
//! `SCM_RIGHTS`. Synchronous: one request, one answer, as the node drives the seam in process.
//!
//! No `unsafe`: rustix wraps the socket calls (`socket`, `connect`, `sendmsg`, `recvmsg`), so this isn't a
//! file CLAUDE.md §12 names. The helper daemon's own privileged calls (setuid-spawn, the ledger limit, the
//! exception server) are the §12 file's.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use hex_api::{BoxId, Exit, Program, Refusal};
use hex_core::helper::{self, Answer, MAX_MESSAGE, Refused, Request, START_FDS, State, VERSION};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};

pub(crate) mod frame;

/// A connection to the box helper. Dropping it closes the socket; the helper ends the boxes it was
/// managing for this node on its own (its own disconnect rule), as the Linux node's socket does.
pub struct Helper {
    fd: OwnedFd,
    /// The version both sides settled on at [`Helper::connect`].
    version: u16,
    /// The id of the next request, so an answer is matched to its request.
    next_id: AtomicU64,
    /// Where the helper listens, for messages and refusals that name it.
    path: PathBuf,
    /// Held across one request and its answer, so callers on several threads (the node's calls, and each
    /// waiting program's [`Helper::wait`]) never interleave their frames on the one stream.
    turn: Mutex<()>,
}

impl Helper {
    /// Connects to the helper at `path`. `Ok(None)` when no helper is there (the socket is absent or
    /// refuses): the node then runs the rootless `process`-group class (step 1). `Ok(Some)` once the
    /// version handshake answers.
    pub fn connect(path: &Path) -> Result<Option<Self>, Refusal> {
        let fd = match rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None) {
            Ok(fd) => fd,
            Err(e) => return Err(Refusal::os("making the helper socket", e)),
        };
        // macOS's socket takes no close-on-exec flag: marked at once, so no program the node starts holds it.
        super::private_descriptor(&fd, false)
            .map_err(|e| Refusal::os("making the helper socket", e))?;
        // macOS has no MSG_NOSIGNAL on send; SO_NOSIGPIPE makes a write to a closed helper an EPIPE error,
        // not a SIGPIPE that would kill the node (best-effort: a failure here only risks the signal).
        let _ = rustix::net::sockopt::set_socket_nosigpipe(&fd, true);
        let addr =
            SocketAddrUnix::new(path).map_err(|e| Refusal::os("naming the helper socket", e))?;
        match rustix::net::connect(&fd, &addr) {
            Ok(()) => {}
            // No helper installed, or it isn't listening: run rootless.
            Err(Errno::NOENT | Errno::CONNREFUSED | Errno::CONNRESET | Errno::AGAIN) => {
                return Ok(None);
            }
            Err(e) => return Err(Refusal::os("reaching the helper", e)),
        }
        let helper = Self {
            fd,
            version: VERSION,
            next_id: AtomicU64::new(1),
            path: path.to_path_buf(),
            turn: Mutex::new(()),
        };
        // Settle the version both speak; the helper answers the highest it and we share.
        match helper.rpc(&Request::Hello { version: VERSION }, &[])? {
            Answer::Hello { version } if version >= 1 && version <= VERSION => {
                Ok(Some(Self { version, ..helper }))
            }
            Answer::Hello { version } => Err(Refusal::Broken(format!(
                "the helper speaks version {version}, past this node's {VERSION}"
            ))),
            other => Err(unexpected("hello", &other)),
        }
    }

    /// A client over an already-connected socket at `version`, without the hello handshake: for a caller
    /// that connected the socket itself (a test's socketpair to a daemon in the same process).
    #[cfg(test)]
    pub(crate) fn over(fd: OwnedFd, version: u16) -> Self {
        Self {
            fd,
            version,
            next_id: AtomicU64::new(1),
            path: PathBuf::from("<connected>"),
            turn: Mutex::new(()),
        }
    }

    /// Takes a pool user for `box_id` with memory bound and reserve in bytes, its processes capped at
    /// `pids_max` (else the platform's per-uid ceiling); returns the user's id.
    pub fn acquire(
        &self,
        box_id: &BoxId,
        bound: u64,
        reserve: u64,
        pids_max: Option<u32>,
    ) -> Result<u32, Refusal> {
        match self.rpc(
            &Request::Acquire {
                box_id: box_id.clone(),
                bound,
                reserve,
                pids_max,
            },
            &[],
        )? {
            Answer::Acquired { uid } => Ok(uid),
            Answer::Refused(r) => Err(refusal(r)),
            other => Err(unexpected("acquire", &other)),
        }
    }

    /// Starts `program` as the box's pool user, armed, its three stdio descriptors riding with the
    /// request; returns its pid.
    pub fn start(
        &self,
        box_id: &BoxId,
        program: &Program,
        stdio: [BorrowedFd<'_>; START_FDS],
        leader: bool,
    ) -> Result<i32, Refusal> {
        let fds: Vec<BorrowedFd<'_>> = stdio.to_vec();
        match self.rpc(
            &Request::Start {
                box_id: box_id.clone(),
                program: program.clone(),
                leader,
            },
            &fds,
        )? {
            Answer::Started { pid } => Ok(pid),
            Answer::Refused(r) => Err(refusal(r)),
            other => Err(unexpected("start", &other)),
        }
    }

    /// Ends the box: the helper kills every process of its user and frees the user back to the pool.
    pub fn end(&self, box_id: &BoxId) -> Result<(), Refusal> {
        match self.rpc(
            &Request::End {
                box_id: box_id.clone(),
            },
            &[],
        )? {
            Answer::Ended => Ok(()),
            Answer::Refused(r) => Err(refusal(r)),
            other => Err(unexpected("end", &other)),
        }
    }

    /// Sends `signal` (a Linux signal number) to every member of the box; the helper maps it to macOS's.
    pub fn signal(&self, box_id: &BoxId, signal: i32) -> Result<(), Refusal> {
        match self.rpc(
            &Request::Signal {
                box_id: box_id.clone(),
                signal,
            },
            &[],
        )? {
            Answer::Signalled => Ok(()),
            Answer::Refused(r) => Err(refusal(r)),
            other => Err(unexpected("signal", &other)),
        }
    }

    /// How program `pid` of the box ended, reaped by the helper (its parent); `None` while it runs. The
    /// node asks once it has seen the program's exit event; an end is answered once.
    pub fn wait(&self, box_id: &BoxId, pid: i32) -> Result<Option<Exit>, Refusal> {
        match self.rpc(
            &Request::Wait {
                box_id: box_id.clone(),
                pid,
            },
            &[],
        )? {
            Answer::Waited(exit) => Ok(exit),
            Answer::Refused(r) => Err(refusal(r)),
            other => Err(unexpected("wait", &other)),
        }
    }

    /// Ends program `pid` of the box with `SIGKILL`, by the helper's hand (the program is its pool user's,
    /// which this node may not signal); the box and its other programs run on. A program the box no longer
    /// holds running is sent nothing: killing an ended program is done (HEX-RT-16).
    pub fn kill(&self, box_id: &BoxId, pid: i32) -> Result<(), Refusal> {
        match self.rpc(
            &Request::Kill {
                box_id: box_id.clone(),
                pid,
            },
            &[],
        )? {
            Answer::Killed => Ok(()),
            Answer::Refused(r) => Err(refusal(r)),
            other => Err(unexpected("kill", &other)),
        }
    }

    /// What the box holds now.
    pub fn inspect(&self, box_id: &BoxId) -> Result<State, Refusal> {
        match self.rpc(
            &Request::Inspect {
                box_id: box_id.clone(),
            },
            &[],
        )? {
            Answer::State(s) => Ok(s),
            Answer::Refused(r) => Err(refusal(r)),
            other => Err(unexpected("inspect", &other)),
        }
    }

    /// One request and its answer. Frames the request (its `u32` length, then its bytes), sends it with
    /// `fds` riding by `SCM_RIGHTS`, reads the framed answer, and checks it carries the request's id.
    fn rpc(&self, request: &Request, fds: &[BorrowedFd<'_>]) -> Result<Answer, Refusal> {
        if fds.len() != request.descriptors() {
            return Err(Refusal::Invalid {
                what: "helper request",
                why: format!(
                    "{} carries {} descriptors, not the {} it takes",
                    self.path.display(),
                    fds.len(),
                    request.descriptors()
                ),
            });
        }
        let _turn = self
            .turn
            .lock()
            .map_err(|_| Refusal::Broken("the helper connection's lock is poisoned".to_owned()))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg =
            helper::encode_request(self.version, id, request).map_err(|e| Refusal::Invalid {
                what: "helper request",
                why: e.to_string(),
            })?;
        send_framed(self.fd.as_fd(), &msg, fds)?;
        let (bytes, extra) = recv_framed(self.fd.as_fd())?;
        // An answer carries no descriptors; close any the helper wrongly sent rather than leak them.
        drop(extra);
        let (_, answer_id, answer) = helper::read_answer(&bytes)
            .map_err(|e| Refusal::Broken(format!("the helper's answer is malformed: {e}")))?;
        if answer_id != id {
            return Err(Refusal::Broken(format!(
                "the helper answered request {answer_id}, not {id}"
            )));
        }
        Ok(answer)
    }
}

/// Maps a helper's typed refusal to the node's [`Refusal`], each by name (CLAUDE.md §3).
fn refusal(r: Refused) -> Refusal {
    match r {
        Refused::UnknownBox(b) => Refusal::NoSuchBox(b),
        Refused::NotRunning(b) => Refusal::NotRunning(b, "signal"),
        Refused::AlreadyAcquired(b) => Refusal::Exists(b),
        Refused::NoFreeUser => Refusal::Os {
            op: "acquire a box user from the helper",
            why: "the pool has no free user".to_owned(),
        },
        Refused::StartFailed(why) => Refusal::Os {
            op: "start a box member through the helper",
            why,
        },
        Refused::NotPermitted(why) => Refusal::Invalid {
            what: "helper request",
            why,
        },
        Refused::GrowFailed(why) => Refusal::Os {
            op: "grow the box users' pool through the helper",
            why,
        },
        Refused::Malformed(why) => {
            Refusal::Broken(format!("the helper rejected a message as malformed: {why}"))
        }
    }
}

fn unexpected(call: &str, answer: &Answer) -> Refusal {
    let kind = match answer {
        Answer::Hello { .. } => "a hello",
        Answer::Acquired { .. } => "an acquired user",
        Answer::Started { .. } => "a started program",
        Answer::Ended => "an ended box",
        Answer::State(_) => "a box's state",
        Answer::Signalled => "a signalled box",
        Answer::Waited(_) => "a program's end",
        Answer::Refused(_) => "a refusal",
        Answer::Killed => "a killed program",
    };
    Refusal::Broken(format!("the helper answered a {call} with {kind}"))
}

/// Sends `msg` framed with its `u32` length, `fds` riding with it by `SCM_RIGHTS` on the first write.
/// Loops until every byte is sent (a `SOCK_STREAM` write may be partial), carrying the descriptors only
/// on the first send, as `SCM_RIGHTS` attaches them to the bytes of one `sendmsg`.
pub(crate) fn send_framed(
    fd: BorrowedFd<'_>,
    msg: &[u8],
    fds: &[BorrowedFd<'_>],
) -> Result<(), Refusal> {
    frame::send(fd, msg, fds, MAX_MESSAGE, None).map_err(frame::Failure::refusal)
}

/// Reads the same legacy frame through the common bounded stream implementation.
pub(crate) fn recv_framed(fd: BorrowedFd<'_>) -> Result<(Vec<u8>, Vec<OwnedFd>), Refusal> {
    frame::receive(fd, MAX_MESSAGE, None).map_err(frame::Failure::refusal)
}

#[cfg(test)]
mod tests;
