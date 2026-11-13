//! The macOS box helper's protocol (native plan step 2). On macOS a box runs as a user of its own, from
//! a pool an installer makes; starting a program as that user, arming a running member's physical-footprint
//! ledger limit, and routing its Mach exceptions are all privileged, so a privileged launchd daemon — the
//! helper — does them, and only them, for the node agent, which runs unprivileged. The node asks over this
//! typed, bounded protocol on a Unix `SOCK_STREAM` socket; the helper does one of a fixed set of things and
//! **refuses everything else by name** (CLAUDE.md §3).
//!
//! Pure (CLAUDE.md §9): the shell (`hex_runtime::macos`) moves the messages and the stdio descriptors a
//! `Start` rides with (`SCM_RIGHTS`); this says what each message is, and refuses by type every one that
//! says nothing valid — a hostile-input test drives the parser (CLAUDE.md §6).
//!
//! A message is a 16-byte header (`HEXM`, a `u16` version, a `u16` kind, a `u64` id the answer carries
//! back), then the body its kind defines. Integers are little-endian; a string is a `u32` length and its
//! bytes; an enum a byte naming its variant. The format changes append-only, as [`crate::wire`]'s does.
//! Requests sit below `0x8000`, answers from it. macOS local sockets have no `SOCK_SEQPACKET`
//! (`hex_client` refuses the node's own socket on macOS for the same reason), so the shell frames each
//! message on the stream with a `u32` length before it ([`read_request`]/[`read_answer`] take one whole
//! message's bytes); a descriptor rides out of band with its message (`SCM_RIGHTS`), never in the bytes.

use hex_api::{BoxId, Exit, Program};

use crate::wire::{self, Fault, Reader, Writer};

pub mod managed;

/// Every helper message's first bytes: "HEX macOS helper".
pub const MAGIC: [u8; 4] = *b"HEXM";
/// The version this side speaks; it reads every one from [`FIRST`].
pub const VERSION: u16 = 1;
/// The first version, which every side reads: a hello and a refusal of an unreadable message go in it.
pub const FIRST: u16 = 1;
/// The header: magic, version, kind, id.
pub const HEADER_BYTES: usize = 16;
/// The most bytes a macOS exec takes for its arguments and environment: `ARG_MAX`
/// (`bsd/sys/syslimits.h:80-81`, xnu `f6217f891ac0`), charged per string as its bytes, its NUL and its
/// pointer (`bsd/kern/kern_exec.c:6108-6112`, `E2BIG` past it).
pub const ARG_MAX: usize = 1024 * 1024;
/// The longest macOS path, its working folder's bound (`bsd/sys/syslimits.h:111`).
pub const PATH_MAX: usize = 1024;
/// The most supplementary groups a macOS process holds (`bsd/sys/syslimits.h:104`).
pub const NGROUPS_MAX: usize = 16;
/// Largest helper message: the largest [`Request::Start`], whose program is the largest a macOS exec takes,
/// so any program the helper could start fits and none it couldn't is sent. A program string costs 4 bytes
/// of length here against its NUL and 8-byte pointer in `ARG_MAX` (an environment pair 8 here against
/// `=`, NUL and pointer), so its argv and environment take at most `ARG_MAX`; beside them go the header, the
/// longest box id, two list counts, the working folder, the user and the leader flag.
pub const MAX_MESSAGE: usize = HEADER_BYTES
    + (4 + BoxId::MAX_LEN)
    + ARG_MAX
    + 2 * 4
    + (1 + 4 + PATH_MAX)
    + (1 + 4 + 4 + 4 + 4 * NGROUPS_MAX)
    + 1;
/// The descriptors a [`Request::Start`] carries: the program's stdin, stdout and stderr.
pub const START_FDS: usize = 3;

mod kind {
    pub const HELLO: u16 = 0x0001;
    pub const ACQUIRE: u16 = 0x0002;
    pub const START: u16 = 0x0003;
    pub const END: u16 = 0x0004;
    pub const INSPECT: u16 = 0x0005;
    pub const SIGNAL: u16 = 0x0006;
    pub const WAIT: u16 = 0x0007;
    /// An acquire naming the box's process limit: appended after the first version's kinds, so a helper
    /// that predates it refuses it by kind.
    pub const ACQUIRE_PIDS: u16 = 0x0008;
    /// A kill of one program in a box: appended after every earlier kind, so a helper that predates it
    /// refuses it by kind.
    pub const KILL: u16 = 0x0009;

    pub const ANSWER: u16 = 0x8000;
    pub const A_HELLO: u16 = ANSWER | HELLO;
    pub const A_ACQUIRE: u16 = ANSWER | ACQUIRE;
    pub const A_START: u16 = ANSWER | START;
    pub const A_END: u16 = ANSWER | END;
    pub const A_INSPECT: u16 = ANSWER | INSPECT;
    pub const A_SIGNAL: u16 = ANSWER | SIGNAL;
    pub const A_WAIT: u16 = ANSWER | WAIT;
    pub const A_KILL: u16 = ANSWER | KILL;
    pub const A_REFUSED: u16 = ANSWER | 0x00ff;
}

/// What the node asks the helper to do. The helper does exactly these; a message of any other kind is
/// refused at [`read_request`] as a kind this version doesn't define.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Settle the version both sides speak, before anything else.
    Hello { version: u16 },
    /// Take a free pool user for a box with this memory bound and reserve (both in bytes); the helper binds
    /// the user to the box and arms its exception handling. Its processes are capped at `pids_max` when the
    /// box's spec names one, else at the platform's per-uid ceiling. Refused when the box already has one,
    /// or the pool is empty. Encoded as [`kind::ACQUIRE`] without a limit, [`kind::ACQUIRE_PIDS`] with one.
    Acquire {
        box_id: BoxId,
        bound: u64,
        reserve: u64,
        pids_max: Option<u32>,
    },
    /// Start `program` as the box's pool user, armed at its apportioned ledger limit, its three stdio
    /// descriptors riding with the message. `leader` is the box's first program (a new session); a later
    /// one (the node's `exec`) joins the user's processes. Refused when the box has no user.
    Start {
        box_id: BoxId,
        program: Program,
        leader: bool,
    },
    /// End the box: the helper kills every process of its user and frees the user back to the pool only once
    /// none is left.
    End { box_id: BoxId },
    /// What the box holds now, by its user's processes.
    Inspect { box_id: BoxId },
    /// Send a signal to every member of the box (the node can't signal another user's processes itself).
    /// `signal` is the Linux signal number, as the rest of Hex carries it; the helper maps it to macOS's
    /// (`hex_core::darwin`), as the process class does. Refused as not running when no member is live.
    Signal { box_id: BoxId, signal: i32 },
    /// How program `pid`, which the helper started in the box, ended. The helper is its parent, so only it
    /// can reap it (`waitid` is the parent's: `bsd/kern/kern_exit.c:3194-3361` returns `ECHILD` to any
    /// other); the node asks once it has seen the program's `NOTE_EXIT` on a kqueue of its own. Answered
    /// [`Answer::Waited`] with `None` while it runs, and its end only once: a program the helper didn't
    /// start in the box, or whose end was read already, is [`Exit::Unknown`].
    Wait { box_id: BoxId, pid: i32 },
    /// End program `pid`, which the helper started in the box and holds running, with `SIGKILL`: one
    /// program's own kill (HEX-RT-16), which the node can't send a pool user's process itself; the box and
    /// its other programs run on, and the program's end is read by [`Request::Wait`] as any end is. A
    /// program the box doesn't hold running (ended, its end read, or never started there) is sent nothing
    /// and answered as killed, as a second kill of an ended program is done on every class.
    Kill { box_id: BoxId, pid: i32 },
}

/// What the box holds now, as the helper reports it (`proc_listpids(PROC_UID_ONLY)` of the box's user, each
/// member's footprint summed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    /// Its members running now.
    pub members: u32,
    /// Its members' footprint summed, in bytes.
    pub footprint: u64,
    /// The highest footprint the helper has read.
    pub peak: u64,
    /// Its processes, as its user's `RLIMIT_NPROC` counts them.
    pub processes: u32,
    /// Times a member crossed its ledger share.
    pub crossings: u64,
    /// Whether the box has been ended for its memory bound.
    pub ended_over: bool,
}

/// What the helper answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Hello {
        version: u16,
    },
    Acquired {
        uid: u32,
    },
    Started {
        pid: i32,
    },
    Ended,
    State(State),
    Signalled,
    /// A [`Request::Wait`]'s program's end, `None` while it runs.
    Waited(Option<Exit>),
    Refused(Refused),
    /// A [`Request::Kill`] done: the program is ended, or was already.
    Killed,
}

/// Why the helper refused a request — each by name (CLAUDE.md §3), so the node learns what and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("the box {0} has no pool user; acquire one first")]
    UnknownBox(BoxId),
    #[error("the box {0} already holds a pool user")]
    AlreadyAcquired(BoxId),
    #[error("the pool has no free user")]
    NoFreeUser,
    #[error("starting the program failed: {0}")]
    StartFailed(String),
    #[error("the helper does not do that: {0}")]
    NotPermitted(String),
    #[error("the request was malformed: {0}")]
    Malformed(String),
    #[error("the box {0} has no live member")]
    NotRunning(BoxId),
    /// Growing the pool by an account failed: the directory read or write that did, named.
    #[error("growing the box users' pool failed: {0}")]
    GrowFailed(String),
}

impl Request {
    fn kind(&self) -> u16 {
        match self {
            Request::Hello { .. } => kind::HELLO,
            Request::Acquire { pids_max: None, .. } => kind::ACQUIRE,
            Request::Acquire {
                pids_max: Some(_), ..
            } => kind::ACQUIRE_PIDS,
            Request::Start { .. } => kind::START,
            Request::End { .. } => kind::END,
            Request::Inspect { .. } => kind::INSPECT,
            Request::Signal { .. } => kind::SIGNAL,
            Request::Wait { .. } => kind::WAIT,
            Request::Kill { .. } => kind::KILL,
        }
    }

    /// The descriptors this request carries with it: three for a [`Request::Start`], none else.
    pub fn descriptors(&self) -> usize {
        match self {
            Request::Start { .. } => START_FDS,
            _ => 0,
        }
    }
}

/// Writes a helper message's header: [`MAGIC`] (not [`crate::wire::MAGIC`]), the version, the kind, the id.
fn put_header(w: &mut Writer, version: u16, kind: u16, id: u64) {
    w.raw(&MAGIC);
    w.u16(version);
    w.u16(kind);
    w.u64(id);
}

/// A refused request as a [`Refused`] reason carries it.
fn refused_tag(r: &Refused) -> u8 {
    match r {
        Refused::UnknownBox(_) => 0,
        Refused::AlreadyAcquired(_) => 1,
        Refused::NoFreeUser => 2,
        Refused::StartFailed(_) => 3,
        Refused::NotPermitted(_) => 4,
        Refused::Malformed(_) => 5,
        Refused::NotRunning(_) => 6,
        Refused::GrowFailed(_) => 7,
    }
}

/// Encodes `request` in `version` with `id`, as a message of at most [`MAX_MESSAGE`] bytes. The stdio
/// descriptors of a [`Request::Start`] ride with the message itself, not in its bytes.
pub fn encode_request(version: u16, id: u64, request: &Request) -> Result<Vec<u8>, Fault> {
    let mut w = Writer::new(MAX_MESSAGE);
    put_header(&mut w, version, request.kind(), id);
    match request {
        Request::Hello { version } => w.u16(*version),
        Request::Acquire {
            box_id,
            bound,
            reserve,
            pids_max,
        } => {
            w.string(box_id.as_str())?;
            w.u64(*bound);
            w.u64(*reserve);
            if let Some(n) = pids_max {
                w.u32(*n);
            }
        }
        Request::Start {
            box_id,
            program,
            leader,
        } => {
            w.string(box_id.as_str())?;
            wire::put_program(&mut w, program)?;
            w.bool(*leader);
        }
        Request::End { box_id } | Request::Inspect { box_id } => w.string(box_id.as_str())?,
        Request::Signal { box_id, signal } => {
            w.string(box_id.as_str())?;
            w.i32(*signal);
        }
        Request::Wait { box_id, pid } | Request::Kill { box_id, pid } => {
            w.string(box_id.as_str())?;
            w.i32(*pid);
        }
    }
    w.done()
}

/// Encodes `answer` in `version` with the request's `id`.
pub fn encode_answer(version: u16, id: u64, answer: &Answer) -> Result<Vec<u8>, Fault> {
    let mut w = Writer::new(MAX_MESSAGE);
    let kind = match answer {
        Answer::Hello { .. } => kind::A_HELLO,
        Answer::Acquired { .. } => kind::A_ACQUIRE,
        Answer::Started { .. } => kind::A_START,
        Answer::Ended => kind::A_END,
        Answer::State(_) => kind::A_INSPECT,
        Answer::Signalled => kind::A_SIGNAL,
        Answer::Waited(_) => kind::A_WAIT,
        Answer::Refused(_) => kind::A_REFUSED,
        Answer::Killed => kind::A_KILL,
    };
    put_header(&mut w, version, kind, id);
    match answer {
        Answer::Hello { version } => w.u16(*version),
        Answer::Acquired { uid } => w.u32(*uid),
        Answer::Started { pid } => w.i32(*pid),
        Answer::Ended | Answer::Signalled | Answer::Killed => {}
        Answer::Waited(exit) => put_exit(&mut w, *exit),
        Answer::State(s) => {
            w.u32(s.members);
            w.u64(s.footprint);
            w.u64(s.peak);
            w.u32(s.processes);
            w.u64(s.crossings);
            w.bool(s.ended_over);
        }
        Answer::Refused(r) => {
            w.u8(refused_tag(r));
            match r {
                Refused::UnknownBox(b) | Refused::AlreadyAcquired(b) | Refused::NotRunning(b) => {
                    w.string(b.as_str())?
                }
                Refused::StartFailed(m)
                | Refused::NotPermitted(m)
                | Refused::Malformed(m)
                | Refused::GrowFailed(m) => w.string(m)?,
                Refused::NoFreeUser => {}
            }
        }
    }
    w.done()
}

/// Reads the header of a `version`-this-side message: `(version, kind, id)`, or the fault that it isn't a
/// helper message this side can read.
fn header(bytes: &[u8]) -> Result<(Reader<'_>, u16, u16, u64), Fault> {
    let mut r = Reader::new(bytes);
    let (version, kind, id) = read_header(&mut r)?;
    if version == 0 || version > VERSION {
        return Err(Fault::Version {
            got: version,
            speaks: VERSION,
        });
    }
    Ok((r, version, kind, id))
}

/// The header with [`MAGIC`] checked (the shared [`Reader::header`] checks [`crate::wire::MAGIC`]).
fn read_header(r: &mut Reader<'_>) -> Result<(u16, u16, u64), Fault> {
    if r.array::<4>()? != MAGIC {
        return Err(Fault::Magic);
    }
    Ok((r.u16()?, r.u16()?, r.u64()?))
}

/// Reads a request from one message's `bytes`, carrying `descriptors` descriptors. Refuses, by a typed
/// [`Fault`], a message that isn't a helper request this version defines, that a `Start` without exactly
/// its three descriptors, and one with bytes past what its kind defines (CLAUDE.md §3: nothing is skipped).
pub fn read_request(bytes: &[u8], descriptors: usize) -> Result<(u16, u64, Request), Fault> {
    if bytes.len() > MAX_MESSAGE {
        return Err(Fault::Oversized {
            bytes: bytes.len(),
            bound: MAX_MESSAGE,
        });
    }
    let (mut r, version, kind, id) = header(bytes)?;
    let request = match kind {
        kind::HELLO => Request::Hello { version: r.u16()? },
        kind::ACQUIRE => Request::Acquire {
            box_id: wire::box_id(&mut r)?,
            bound: r.u64()?,
            reserve: r.u64()?,
            pids_max: None,
        },
        kind::ACQUIRE_PIDS => Request::Acquire {
            box_id: wire::box_id(&mut r)?,
            bound: r.u64()?,
            reserve: r.u64()?,
            pids_max: Some(r.u32()?),
        },
        kind::START => Request::Start {
            box_id: wire::box_id(&mut r)?,
            program: wire::program(&mut r)?,
            leader: r.bool("leader")?,
        },
        kind::END => Request::End {
            box_id: wire::box_id(&mut r)?,
        },
        kind::INSPECT => Request::Inspect {
            box_id: wire::box_id(&mut r)?,
        },
        kind::SIGNAL => Request::Signal {
            box_id: wire::box_id(&mut r)?,
            signal: r.i32()?,
        },
        kind::WAIT => Request::Wait {
            box_id: wire::box_id(&mut r)?,
            pid: r.i32()?,
        },
        kind::KILL => Request::Kill {
            box_id: wire::box_id(&mut r)?,
            pid: r.i32()?,
        },
        other => {
            return Err(Fault::Kind {
                kind: other,
                version,
                expected: "a helper request",
            });
        }
    };
    r.end(kind, version)?;
    let want = request.descriptors();
    if want != descriptors {
        return Err(Fault::Descriptors {
            what: "a start",
            want,
            got: descriptors,
        });
    }
    Ok((version, id, request))
}

/// Reads an answer from one message's `bytes`.
pub fn read_answer(bytes: &[u8]) -> Result<(u16, u64, Answer), Fault> {
    if bytes.len() > MAX_MESSAGE {
        return Err(Fault::Oversized {
            bytes: bytes.len(),
            bound: MAX_MESSAGE,
        });
    }
    let (mut r, version, kind, id) = header(bytes)?;
    let answer = match kind {
        kind::A_HELLO => Answer::Hello { version: r.u16()? },
        kind::A_ACQUIRE => Answer::Acquired { uid: r.u32()? },
        kind::A_START => Answer::Started { pid: r.i32()? },
        kind::A_END => Answer::Ended,
        kind::A_INSPECT => Answer::State(State {
            members: r.u32()?,
            footprint: r.u64()?,
            peak: r.u64()?,
            processes: r.u32()?,
            crossings: r.u64()?,
            ended_over: r.bool("ended_over")?,
        }),
        kind::A_SIGNAL => Answer::Signalled,
        kind::A_WAIT => Answer::Waited(read_exit(&mut r)?),
        kind::A_KILL => Answer::Killed,
        kind::A_REFUSED => Answer::Refused(read_refused(&mut r)?),
        other => {
            return Err(Fault::Kind {
                kind: other,
                version,
                expected: "a helper answer",
            });
        }
    };
    r.end(kind, version)?;
    Ok((version, id, answer))
}

/// A [`Answer::Waited`]'s end: a tag (0 running, 1 a code, 2 a signal, 3 unknown), then the code or
/// signal as an `i32`.
fn put_exit(w: &mut Writer, exit: Option<Exit>) {
    match exit {
        None => w.u8(0),
        Some(Exit::Code(c)) => {
            w.u8(1);
            w.i32(c);
        }
        Some(Exit::Signal(s)) => {
            w.u8(2);
            w.i32(s);
        }
        Some(Exit::Unknown) => w.u8(3),
    }
}

fn read_exit(r: &mut Reader<'_>) -> Result<Option<Exit>, Fault> {
    Ok(match r.u8()? {
        0 => None,
        1 => Some(Exit::Code(r.i32()?)),
        2 => Some(Exit::Signal(r.i32()?)),
        3 => Some(Exit::Unknown),
        t => return Err(Fault::value("exit", format!("variant {t}"))),
    })
}

fn read_refused(r: &mut Reader<'_>) -> Result<Refused, Fault> {
    Ok(match r.u8()? {
        0 => Refused::UnknownBox(wire::box_id(r)?),
        1 => Refused::AlreadyAcquired(wire::box_id(r)?),
        2 => Refused::NoFreeUser,
        3 => Refused::StartFailed(r.string("reason")?),
        4 => Refused::NotPermitted(r.string("reason")?),
        5 => Refused::Malformed(r.string("reason")?),
        6 => Refused::NotRunning(wire::box_id(r)?),
        7 => Refused::GrowFailed(r.string("reason")?),
        t => return Err(Fault::value("refusal", format!("variant {t}"))),
    })
}

#[cfg(test)]
mod tests;
