//! macOS's event sources and local socket for the node's socket core (native plan step 8b), in safe calls
//! only (no new `unsafe`, CLAUDE.md §12):
//! - the socket is AF_UNIX `SOCK_STREAM`, as macOS has no `SOCK_SEQPACKET` local socket (it refuses one,
//!   EPROTONOSUPPORT, measured on studio-3, macOS 26.2, 2026-11-07), so each message is framed
//!   ([`hex_core::stream`]) and the core gets them whole;
//! - an exec's stdio comes as descriptors (`SCM_RIGHTS`) with its message's first byte. XNU's stream
//!   receive stops before a record that carries control data once it has copied any (measured on studio-3:
//!   two framed messages each with descriptors, then one without, came as three reads' worth, the
//!   descriptors with the read that began at their message), so a read that brings descriptors begins at
//!   the message they belong to; descriptors that come while a frame is under way are misplaced, and that
//!   message is refused;
//! - one kqueue on the I/O thread watches everything (the lead's call, 2026-11-07: latency first, no thread
//!   hop on a program's end and no scan of every descriptor on a wake): `EVFILT_READ` and `EVFILT_WRITE` on
//!   the listener and the connections, level-triggered as the core asks; `EVFILT_PROC` on exec'd programs;
//!   `EVFILT_USER` for [`Os::wake`] (`hex_runtime::macos::sys::Kqueue`, the macOS class's wrapper of it);
//! - a message the stream takes only part of is finished here when the socket has room again, and until
//!   then the core's next sends would block, so to the core every send is whole or nothing;
//! - only this user or root is served: the socket's file is this user's, mode 0600, which XNU checks for a
//!   connect (`bsd/kern/uipc_usrreq.c`, `unp_connect`: `vnode_authorize` with `KAUTH_VNODE_WRITE_DATA`),
//!   and each peer's effective uid is read (getpeereid(3)) and refused by name otherwise, as Linux's is.

use std::collections::{BTreeMap, VecDeque};
use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hex_api::{Refusal, Stdio};
use hex_core::stream::{self, Deframer};
use hex_core::wire::{self, Streams};
use hex_runtime::macos::sys::{self, Kqueue};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::OFlags;
use rustix::io::{Errno, FdFlags};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendFlags,
    Shutdown, SocketAddrUnix, SocketType,
};

use super::{Accepted, EVENTS, Event, Interest, Os, Peer, Platform, Recv, Sent, Unwatched};

/// What macOS lets the node hold ([`Platform`]), as it starts:
/// - descriptors: its soft `RLIMIT_NOFILE` (raised to its hard limit where that is finite), within
///   `kern.maxfilesperproc`, as XNU's open holds a process to the less of the two (`proc_limitgetcur_nofile`);
/// - threads: `kern.num_taskthreads`, the most one process runs;
/// - programs: each a process, so what `fork1` holds the node's user to (`bsd/kern/kern_fork.c`): `kern.maxproc`,
///   and for any user but root its soft `RLIMIT_NPROC`.
pub fn platform() -> Result<Platform, Refusal> {
    let read =
        |what: &'static str, got: std::io::Result<usize>| got.map_err(|e| Refusal::os(what, e));
    let soft = |resource| {
        rustix::process::getrlimit(resource)
            .current
            .map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX))
    };
    let processes = read("reading kern.maxproc", sys::max_processes())?;
    Ok(Platform {
        descriptors: soft(rustix::process::Resource::Nofile).min(read(
            "reading kern.maxfilesperproc",
            sys::max_files_per_process(),
        )?),
        threads: read("reading kern.num_taskthreads", sys::max_threads_per_task())?,
        programs: if rustix::process::getuid().is_root() {
            processes
        } else {
            soft(rustix::process::Resource::Nproc).min(processes)
        },
    })
}

/// Connections waiting to be accepted, which the I/O thread accepts as they come: as many as the kernel queues for a
/// socket, none picked (Ada's rule, 2026-11-06). listen(2) caps a larger backlog at the kernel's own, Linux's
/// `net.core.somaxconn` and XNU's `kern.ipc.somaxconn` (`solisten`), so this asks for the most and gets the kernel's.
const BACKLOG: i32 = i32::MAX;
/// A frame's prefix, its body's length (`hex_core::stream`'s `u32`).
const PREFIX: usize = size_of::<u32>();

/// A [`Stream`]'s token before the core watches it.
const UNWATCHED: u64 = u64::MAX;

/// One connection: its socket, its token once watched, and the framing between its bytes and the core's
/// messages.
pub struct Stream {
    fd: OwnedFd,
    token: AtomicU64,
    framing: Mutex<Framing>,
}

struct Framing {
    deframer: Deframer,
    /// Whole messages read and not yet taken by the core, each with what came with it.
    ready: VecDeque<Message>,
    /// Descriptors that came at a message's first byte, for that message.
    passed: Vec<OwnedFd>,
    /// Descriptors came while a frame was under way, or more than a message takes.
    cut: bool,
    /// The rest of a message the stream took only part of.
    unsent: Vec<u8>,
    /// The stream can't be read further (an oversized frame): the next read is its end.
    broken: bool,
}

struct Message {
    bytes: Vec<u8>,
    /// Its declared size, when it was past the bound and not kept.
    size: usize,
    passed: Vec<OwnedFd>,
    cut: bool,
}

/// The listener's tag in the kqueue; the core's tokens start above it.
const LISTENER: u64 = 0;

/// What the I/O thread watches: the connections, by token, and what for.
#[derive(Default)]
struct Watching {
    conns: BTreeMap<u64, (Arc<Stream>, Interest)>,
    /// The host's own connections' tokens ([`super::Source`]): reported as they are ready.
    sources: std::collections::BTreeSet<u64>,
}

/// What [`Os::wake`] and a program already gone leave for the next wait.
#[derive(Default)]
struct Woken {
    wake: bool,
    /// Tokens of watched programs that ended before they could be watched.
    ended: VecDeque<u64>,
}

/// The listener and the kqueue that watches it, the connections and the programs.
pub struct MacOs {
    listener: OwnedFd,
    kq: Kqueue,
    watching: Mutex<Watching>,
    woken: Mutex<Woken>,
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("a socket lock was poisoned".to_owned())
}

/// Descriptors made close-on-exec and non-blocking: macOS's `socket` and `accept` take no flags.
fn own(fd: &OwnedFd, what: &'static str) -> Result<(), Refusal> {
    rustix::io::fcntl_setfd(fd, FdFlags::CLOEXEC).map_err(|e| Refusal::os(what, e))?;
    let flags = rustix::fs::fcntl_getfl(fd).map_err(|e| Refusal::os(what, e))?;
    rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(|e| Refusal::os(what, e))
}

impl MacOs {
    /// A socket at `path`, readable and writable by this user alone, and its kqueue. A socket left at
    /// `path` by an agent that has ended is replaced; one an agent still serves, or a file that isn't a
    /// socket, is refused.
    pub fn bind(path: &Path) -> Result<Self, Refusal> {
        super::unix::clear(path, SocketType::STREAM)?;
        let addr = SocketAddrUnix::new(path).map_err(|e| Refusal::Invalid {
            what: "socket path",
            why: format!("{}: {e}", path.display()),
        })?;
        let listener = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
            .map_err(|e| Refusal::os("making the node agent's socket", e))?;
        own(&listener, "making the node agent's socket")?;
        rustix::net::bind(&listener, &addr).map_err(|e| Refusal::Os {
            op: "binding the node agent's socket",
            why: format!("{}: {e}", path.display()),
        })?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| Refusal::os("restricting the node agent's socket to its user", e))?;
        rustix::net::listen(&listener, BACKLOG)
            .map_err(|e| Refusal::os("listening on the node agent's socket", e))?;
        let kq = Kqueue::new().map_err(|e| Refusal::os("making the socket's kqueue", e))?;
        kq.fd(&listener, LISTENER, false, false)
            .map_err(|e| Refusal::os("watching the node agent's socket", e))?;
        Ok(Self {
            listener,
            kq,
            watching: Mutex::new(Watching::default()),
            woken: Mutex::new(Woken::default()),
        })
    }

    fn watching(&self) -> Result<MutexGuard<'_, Watching>, Refusal> {
        self.watching.lock().map_err(poisoned)
    }
}

/// A watched program, by its pid.
pub struct Watch {
    pid: i32,
}

impl Os for MacOs {
    type Conn = Arc<Stream>;
    type Watch = Watch;
    type Passed = OwnedFd;

    fn wait(&self, events: &mut Vec<Event>, timeout: Option<Duration>) -> Result<(), Refusal> {
        let (conns, sources): (BTreeMap<u64, (Arc<Stream>, Interest)>, _) = {
            let watching = self.watching()?;
            let conns = watching
                .conns
                .iter()
                .map(|(t, (s, i))| (*t, (Arc::clone(s), *i)))
                .collect();
            (conns, watching.sources.clone())
        };
        // What is ready without a wait: a wake, programs gone before they were watched, and messages read
        // whole and not yet taken.
        let now = self.ready_now(&conns, events)?;
        let mut got = Vec::with_capacity(sys::EVENTS);
        self.kq
            .wait(
                None,
                if now { Some(Duration::ZERO) } else { timeout },
                &mut got,
            )
            .map_err(|e| Refusal::os("waiting on the socket's kqueue", e))?;
        for e in &got {
            let (token, readable, writable, hung_up) = match *e {
                sys::Event::User => {
                    if std::mem::take(&mut self.woken.lock().map_err(poisoned)?.wake)
                        && !events.contains(&Event::Wake)
                    {
                        events.push(Event::Wake);
                    }
                    continue;
                }
                sys::Event::Readable { tag: LISTENER, .. } => {
                    events.push(Event::Listener);
                    continue;
                }
                sys::Event::Exit { tag, .. } => (tag, true, false, false),
                sys::Event::Readable { tag, ended } => {
                    let read = sources.contains(&tag)
                        || conns
                            .get(&tag)
                            .is_some_and(|(_, i)| matches!(i, Interest::Read));
                    (tag, read, false, ended)
                }
                sys::Event::Writable { tag, ended } if sources.contains(&tag) => {
                    (tag, false, true, ended)
                }
                sys::Event::Writable { tag, ended } => {
                    let Some((conn, interest)) = conns.get(&tag) else {
                        continue;
                    };
                    let flushed = finish_unsent(conn)?;
                    if flushed && matches!(interest, Interest::Read) {
                        // Its rest sent: back to watching only its reads.
                        self.kq
                            .fd(&conn.fd, tag, true, false)
                            .map_err(|e| Refusal::os("watching a connection", e))?;
                    }
                    (
                        tag,
                        false,
                        flushed && matches!(interest, Interest::Write),
                        ended,
                    )
                }
                // This socket kqueue registers no signal or timer filters.
                sys::Event::Signal { .. } | sys::Event::Timer | sys::Event::Fork { .. } => continue,
            };
            match events
                .iter_mut()
                .find(|ev| matches!(ev, Event::Ready { token: t, .. } if *t == token))
            {
                Some(Event::Ready {
                    readable: r,
                    writable: w,
                    hung_up: h,
                    ..
                }) => {
                    *r |= readable;
                    *w |= writable;
                    *h |= hung_up;
                }
                _ if readable || writable || hung_up => events.push(Event::Ready {
                    token,
                    readable,
                    writable,
                    hung_up,
                }),
                _ => {}
            }
        }
        Ok(())
    }

    fn wake(&self) -> Result<(), Refusal> {
        self.woken.lock().map_err(poisoned)?.wake = true;
        self.kq
            .trigger()
            .map_err(|e| Refusal::os("waking the socket's thread", e))
    }

    fn accept(&self) -> Accepted<Arc<Stream>> {
        loop {
            return match rustix::net::accept(&self.listener) {
                Ok(fd) => match own(&fd, "accepting on the node agent's socket").and_then(|()| {
                    rustix::net::sockopt::set_socket_nosigpipe(&fd, true)
                        .map_err(|e| Refusal::os("accepting on the node agent's socket", e))
                }) {
                    Ok(()) => Accepted::Conn(Arc::new(Stream {
                        fd,
                        token: AtomicU64::new(UNWATCHED),
                        framing: Mutex::new(Framing {
                            deframer: Deframer::new(wire::MAX_MESSAGE),
                            ready: VecDeque::new(),
                            passed: Vec::new(),
                            cut: false,
                            unsent: Vec::new(),
                            broken: false,
                        }),
                    })),
                    Err(e) => Accepted::Failed(e),
                },
                Err(Errno::AGAIN) => Accepted::None,
                Err(Errno::INTR | Errno::CONNABORTED) => continue,
                Err(Errno::MFILE | Errno::NFILE | Errno::NOBUFS | Errno::NOMEM) => Accepted::Spent,
                Err(e) => Accepted::Failed(Refusal::os("accepting on the node agent's socket", e)),
            };
        }
    }

    fn accepting(&self, on: bool) -> Result<(), Refusal> {
        self.kq
            .fd(&self.listener, LISTENER, on, false)
            .map_err(|e| Refusal::os("pausing or resuming the node agent's socket", e))
    }

    fn stop_listening(&self) {
        let _ = self.kq.forget_fd(&self.listener);
    }

    fn peer(&self, conn: &Arc<Stream>) -> Peer {
        let me = rustix::process::geteuid().as_raw();
        match sys::peer_euid(&conn.fd) {
            Ok(uid) if uid == me || uid == 0 => Peer::Allowed,
            Ok(uid) => Peer::Refused(format!("uid {uid} isn't the node agent's user ({me})")),
            Err(_) => Peer::Gone,
        }
    }

    fn watch(&self, conn: &Arc<Stream>, token: u64, interest: Interest) -> Result<(), Refusal> {
        conn.token.store(token, Ordering::SeqCst);
        let unsent = !conn.framing.lock().map_err(poisoned)?.unsent.is_empty();
        self.watching()?
            .conns
            .insert(token, (Arc::clone(conn), interest));
        self.kq
            .fd(
                &conn.fd,
                token,
                matches!(interest, Interest::Read),
                matches!(interest, Interest::Write) || unsent,
            )
            .map_err(|e| Refusal::os("watching a connection", e))
    }

    fn rewatch(&self, conn: &Arc<Stream>, token: u64, interest: Interest) -> Result<(), Refusal> {
        self.watch(conn, token, interest)
    }

    fn unwatch(&self, conn: &Arc<Stream>) {
        if let Ok(mut w) = self.watching.lock() {
            w.conns.retain(|_, (s, _)| !Arc::ptr_eq(s, conn));
        }
        let _ = self.kq.forget_fd(&conn.fd);
    }

    fn recv(&self, conn: &Arc<Stream>, buf: &mut [u8]) -> Recv<OwnedFd> {
        let Ok(mut f) = conn.framing.lock() else {
            return Recv::Gone;
        };
        loop {
            if let Some(m) = f.ready.pop_front() {
                let n = m.bytes.len().min(buf.len());
                if let (Some(into), Some(from)) = (buf.get_mut(..n), m.bytes.get(..n)) {
                    into.copy_from_slice(from);
                }
                return Recv::Message {
                    size: m.size,
                    got: n,
                    passed: m.passed,
                    cut: m.cut,
                };
            }
            if f.broken {
                return Recv::Gone;
            }
            match read_some(&conn.fd, &mut f) {
                Ok(true) => {}
                // Its end: a stream holds no empty message (each frame has its prefix), so a read of
                // nothing, after every whole message it sent, is its client gone.
                Ok(false) => return Recv::Gone,
                Err(Errno::AGAIN) => return Recv::WouldBlock,
                Err(Errno::CONNRESET) => return Recv::Reset,
                Err(_) => return Recv::Gone,
            }
        }
    }

    /// One whole frame, `fds` riding at its first byte (a given answer's streams), as the client reads them.
    fn send(&self, conn: &Arc<Stream>, bytes: &[u8], fds: &[hex_api::End]) -> Sent {
        let Ok(mut f) = conn.framing.lock() else {
            return Sent::Gone;
        };
        if !f.unsent.is_empty() {
            return Sent::WouldBlock;
        }
        let Ok(framed) = stream::frame(bytes, wire::MAX_MESSAGE) else {
            return Sent::Gone;
        };
        let borrowed: Vec<std::os::fd::BorrowedFd<'_>> =
            fds.iter().map(std::os::fd::AsFd::as_fd).collect();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
        let mut control = rustix::net::SendAncillaryBuffer::new(&mut space);
        if !borrowed.is_empty()
            && !control.push(rustix::net::SendAncillaryMessage::ScmRights(&borrowed))
        {
            return Sent::Gone;
        }
        loop {
            let sent = if borrowed.is_empty() {
                rustix::net::send(&conn.fd, &framed, SendFlags::DONTWAIT)
            } else {
                rustix::net::sendmsg(
                    &conn.fd,
                    &[std::io::IoSlice::new(&framed)],
                    &mut control,
                    SendFlags::DONTWAIT,
                )
            };
            return match sent {
                Ok(n) if n >= framed.len() => Sent::Sent,
                Ok(n) => {
                    // Taken in part: the rest goes when the socket has room ([`finish_unsent`]), which
                    // its write filter now reports.
                    f.unsent = framed.get(n..).unwrap_or_default().to_vec();
                    let token = conn.token.load(Ordering::SeqCst);
                    if token != UNWATCHED && self.kq.fd(&conn.fd, token, true, true).is_err() {
                        return Sent::Gone;
                    }
                    Sent::Sent
                }
                Err(Errno::INTR) => continue,
                Err(Errno::AGAIN) => Sent::WouldBlock,
                Err(_) => Sent::Gone,
            };
        }
    }

    fn shutdown(&self, conn: &Arc<Stream>) {
        let _ = rustix::net::shutdown(&conn.fd, Shutdown::Both);
    }

    fn hung_up(&self, conn: &Arc<Stream>) -> bool {
        let mut fds = [PollFd::new(&conn.fd, PollFlags::HUP)];
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        match rustix::event::poll(&mut fds, Some(&zero)) {
            Ok(_) => fds
                .first()
                .is_some_and(|f| f.revents().intersects(PollFlags::HUP | PollFlags::ERR)),
            Err(_) => true,
        }
    }

    fn watch_program(&self, pid: u32, token: u64) -> Result<Watch, Unwatched> {
        let unwatched = |why: String| {
            Unwatched::Runs(Refusal::Os {
                op: "watching a started program",
                why: format!(
                    "{why}: it runs on unwatched by the socket, and the node's sweep reaps it"
                ),
            })
        };
        let pid = i32::try_from(pid).map_err(|_| unwatched(format!("pid {pid} isn't one")))?;
        let started = match sys::rusage(pid) {
            Ok(Some(r)) => r.started,
            Ok(None) => 0,
            Err(e) => return Err(unwatched(e.to_string())),
        };
        let gone = self
            .kq
            .watch(&[pid], token)
            .map_err(|e| unwatched(e.to_string()))?;
        // XNU's EVFILT_PROC catches only what comes after its attach (`bsd/kern/kern_event.c`,
        // `filt_procattach`), so a program that ended before it (a short one often does, and a zombie isn't
        // refused with ESRCH) would never be reported. Its own record says whether it has ended: read after
        // the attach, an end before it is reported here and one after it by the kqueue; both is one end the
        // core takes once.
        let ended = gone.contains(&pid)
            || match sys::rusage(pid) {
                Ok(Some(r)) => r.ended || r.started != started,
                Ok(None) => true,
                Err(_) => false,
            };
        if ended {
            if let Ok(mut w) = self.woken.lock() {
                w.ended.push_back(token);
            }
            let _ = self.kq.trigger();
        }
        Ok(Watch { pid })
    }

    fn unwatch_program(&self, watch: Watch) {
        let _ = self.kq.unwatch(watch.pid);
    }

    fn stdio(&self, streams: Streams, passed: Vec<OwnedFd>) -> Result<Stdio, Refusal> {
        super::unix::stdio_of(streams, passed)
    }

    fn watch_source(
        &self,
        handle: super::Handle<'_>,
        token: u64,
        write: bool,
    ) -> Result<(), Refusal> {
        self.watching()?.sources.insert(token);
        self.kq
            .fd(handle, token, true, write)
            .map_err(|e| Refusal::os("watching the node's guest connection", e))
    }

    fn unwatch_source(&self, handle: super::Handle<'_>, token: u64) {
        if let Ok(mut w) = self.watching.lock() {
            w.sources.remove(&token);
        }
        let _ = self.kq.forget_fd(handle);
    }
}

impl MacOs {
    /// Puts in `events` what is ready without a wait: a wake, programs gone before they were watched, and
    /// connections with messages read whole and not taken. Whether there was any.
    fn ready_now(
        &self,
        conns: &BTreeMap<u64, (Arc<Stream>, Interest)>,
        events: &mut Vec<Event>,
    ) -> Result<bool, Refusal> {
        let before = events.len();
        {
            let mut w = self.woken.lock().map_err(poisoned)?;
            if std::mem::take(&mut w.wake) {
                events.push(Event::Wake);
            }
            while events.len() < EVENTS {
                let Some(token) = w.ended.pop_front() else {
                    break;
                };
                events.push(Event::Ready {
                    token,
                    readable: true,
                    writable: false,
                    hung_up: false,
                });
            }
        }
        for (token, (s, interest)) in conns {
            if events.len() >= EVENTS {
                break;
            }
            let held = s.framing.lock().map_err(poisoned)?;
            if matches!(interest, Interest::Read) && (!held.ready.is_empty() || held.broken) {
                events.push(Event::Ready {
                    token: *token,
                    readable: true,
                    writable: false,
                    hung_up: false,
                });
            }
        }
        Ok(events.len() > before)
    }
}

/// Sends what the stream took only part of, as far as it takes now. Whether all of it is out.
fn finish_unsent(s: &Stream) -> Result<bool, Refusal> {
    let mut f = s.framing.lock().map_err(poisoned)?;
    while !f.unsent.is_empty() {
        match rustix::net::send(&s.fd, &f.unsent, SendFlags::DONTWAIT) {
            Ok(n) => {
                let len = f.unsent.len();
                f.unsent.drain(..n.min(len));
            }
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => return Ok(false),
            Err(_) => {
                // Its client is gone: the hang-up closes it.
                f.unsent.clear();
            }
        }
    }
    Ok(true)
}

/// Reads what the stream has and frames it into whole messages. `Ok(false)` at its end.
fn read_some(fd: &OwnedFd, f: &mut Framing) -> Result<bool, Errno> {
    let mut bytes = vec![0u8; stream::MAX_FRAME];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
    let msg = loop {
        let mut control = RecvAncillaryBuffer::new(&mut space);
        match rustix::net::recvmsg(
            fd,
            &mut [IoSliceMut::new(&mut bytes)],
            &mut control,
            RecvFlags::DONTWAIT,
        ) {
            Ok(msg) => {
                let fds: Vec<OwnedFd> = control
                    .drain()
                    .filter_map(|m| match m {
                        RecvAncillaryMessage::ScmRights(fds) => Some(fds),
                        _ => None,
                    })
                    .flatten()
                    .collect();
                break (msg, fds);
            }
            Err(Errno::INTR) => continue,
            Err(e) => return Err(e),
        }
    };
    let (msg, fds) = msg;
    if msg.bytes == 0 && fds.is_empty() {
        return Ok(false);
    }
    for fd in &fds {
        // A descriptor that came is this process's now: close-on-exec, as every other (macOS's recvmsg has no
        // MSG_CMSG_CLOEXEC). A failure is the connection's, never dropped (CLAUDE.md §3).
        rustix::io::fcntl_setfd(fd, FdFlags::CLOEXEC)?;
    }
    if !fds.is_empty() {
        // Descriptors begin a message; ones that come while a frame is under way are misplaced.
        if f.deframer.in_frame() || !f.passed.is_empty() {
            f.cut = true;
        }
        f.passed.extend(fds);
    }
    if msg.flags.contains(ReturnFlags::CTRUNC) {
        f.cut = true;
    }
    let mut input = bytes.get(..msg.bytes).unwrap_or_default();
    while !input.is_empty() {
        let at_start = !f.deframer.in_frame();
        match f.deframer.take(input) {
            Ok((took, done)) => {
                input = input.get(took..).unwrap_or_default();
                if let Some(message) = done {
                    let passed = std::mem::take(&mut f.passed);
                    let cut = std::mem::take(&mut f.cut);
                    f.ready.push_back(Message {
                        size: message.len(),
                        bytes: message,
                        passed,
                        cut,
                    });
                }
            }
            Err(wire::Fault::Oversized { bytes, .. }) => {
                // Its size is said, with its header where this read holds it, so the core refuses it by
                // name to the request it is; the stream can't be read past it.
                let header = if at_start {
                    input
                        .get(PREFIX..)
                        .unwrap_or_default()
                        .iter()
                        .take(wire::HEADER_BYTES)
                        .copied()
                        .collect()
                } else {
                    Vec::new()
                };
                f.ready.push_back(Message {
                    bytes: header,
                    size: bytes,
                    passed: std::mem::take(&mut f.passed),
                    cut: false,
                });
                f.broken = true;
                return Ok(true);
            }
            Err(_) => {
                f.broken = true;
                return Ok(true);
            }
        }
    }
    Ok(true)
}
