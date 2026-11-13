//! The Windows calls the node agent's socket and its client make that Rust can't make safely (native plan
//! step 8; the
//! decision "the Windows node, on one completion port" in docs/design/runtime.md): one I/O completion
//! port that every source reports on, Windows' AF_UNIX stream sockets (afunix.sys, Windows 10 1803 on),
//! each connection's overlapped receive and send and the listener's overlapped `AcceptEx`, a program's
//! end by a wait completion packet on its process handle, queued to the port by the kernel, a host's own
//! connection's readiness by its socket's network events through such a packet ([`SourceWatch`]), and who a
//! connection's peer is (its pid by `SIO_AF_UNIX_GETPEERPID`, then its process's user). `windows-sys` binds
//! these as `unsafe` foreign functions, but for ntdll's wait completion packet calls, which Hex declares
//! itself; std has no AF_UNIX socket on Windows and no completion port. Each `unsafe` block states the
//! invariant that makes it sound.
//!
//! An overlapped operation's `OVERLAPPED` and buffer are the kernel's from its start until its
//! completion is dequeued. Each is an [`Op`] on the heap, handed to the kernel as a raw pointer and taken
//! back exactly once: from the port when it completes, or at once when the start fails (no completion is
//! queued then). So nothing the kernel may write is freed or moved under it, whatever its connection does
//! meanwhile: closing a socket ends its operations, and their completions still bring their `Op`s back.

// This file's `unsafe` is named in CLAUDE.md §12; every block below carries its SAFETY comment.
#![allow(unsafe_code)]
#![cfg(windows)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{
    AsRawHandle, AsRawSocket, AsSocket, BorrowedHandle, BorrowedSocket, FromRawHandle, OwnedHandle,
    RawHandle, RawSocket,
};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle,
    ERROR_OPERATION_ABORTED, FALSE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, STATUS_PENDING,
    TRUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, AcceptEx, FD_CLOSE, FD_READ, FD_WRITE, FIONBIO, INVALID_SOCKET, LINGER, SD_BOTH,
    SD_RECEIVE, SD_SEND, SO_LINGER, SO_UPDATE_ACCEPT_CONTEXT, SOCK_STREAM, SOCKADDR_UN, SOCKET,
    SOCKET_ERROR, SOL_SOCKET, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSA_IO_PENDING,
    WSABUF, WSADATA, WSAECONNABORTED, WSAECONNRESET, WSAEMFILE, WSAENOBUFS, WSAEventSelect,
    WSAGetLastError, WSAIoctl, WSARecv, WSASend, WSASocketW, WSAStartup, bind, closesocket,
    connect, ioctlsocket, listen, recv, send, setsockopt, shutdown,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinLocalSystemSid,
};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
    PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, OpenProcess, OpenProcessToken, PROCESS_DUP_HANDLE,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResetEvent,
};

/// `SIO_AF_UNIX_GETPEERPID`: `_WSAIOR(IOC_VENDOR, 256)` (afunix.h), which windows-sys doesn't define.
const SIO_AF_UNIX_GETPEERPID: u32 = 0x5800_0100;
/// The completion key of every socket's operations: which one is in its [`Op`].
const KEY_IO: usize = 0;
/// The completion key [`Port::wake`] posts.
const KEY_WAKE: usize = 1;
/// The completion key a program's end posts; its token is the entry's pointer-sized field.
const KEY_PROGRAM: usize = 2;
/// The completion key a watched socket's readiness posts ([`SourceWatch`]); its token is the entry's
/// pointer-sized field.
const KEY_SOURCE: usize = 3;
/// Room `AcceptEx` takes for each end's address: the address and 16 bytes (AcceptEx's documentation).
const ADDRESS_ROOM: usize = size_of::<SOCKADDR_UN>() + 16;
/// A size as Winsock's `int` lengths take it (every one here is a structure's, far below `i32::MAX`).
fn int_len(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// A size as Win32's `DWORD` lengths take it.
fn dword_len(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
/// Connections waiting for an accept, the provider's own bound (`SOMAXCONN`, i32::MAX, listen(2) on Windows):
/// the node takes them as they come, one `AcceptEx` at a time; no picked backlog (native plan step 8).
const BACKLOG: i32 = i32::MAX;

/// Winsock, started once a process (`WSAStartup` is counted; nothing here ends it).
pub fn winsock() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let r = *STARTED.get_or_init(|| {
        // SAFETY: a WSADATA for the call to fill, alive for it; 2.2 is the version every Windows has.
        let mut data: WSADATA = unsafe { zeroed() };
        // SAFETY: as above.
        unsafe { WSAStartup(0x0202, &raw mut data) }
    });
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(r))
    }
}

fn wsa_error() -> io::Error {
    // SAFETY: reads this thread's last Winsock error; no arguments.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

fn last_error() -> io::Error {
    // SAFETY: reads this thread's last error; no arguments.
    io::Error::from_raw_os_error(unsafe { GetLastError() }.cast_signed())
}

/// Whether a socket call failed for want of sockets or memory: the agent is spent, and accepting waits.
pub fn is_spent(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(WSAEMFILE | WSAENOBUFS))
}

/// Whether a socket call failed because its peer closed with data unread (a reset).
pub fn is_reset(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(WSAECONNRESET | WSAECONNABORTED))
}

fn address(path: &Path) -> io::Result<SOCKADDR_UN> {
    let text = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a socket path that isn't UTF-8",
        )
    })?;
    let mut addr = SOCKADDR_UN {
        sun_family: AF_UNIX,
        sun_path: [0; 108],
    };
    // afunix.sys takes the path as bytes, NUL-terminated within `sun_path`.
    if text.len() >= addr.sun_path.len() || text.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "a socket path of {} bytes, past sockaddr_un's {}",
                text.len(),
                addr.sun_path.len().saturating_sub(1)
            ),
        ));
    }
    for (d, b) in addr.sun_path.iter_mut().zip(text.bytes()) {
        *d = i8::from_ne_bytes([b]);
    }
    Ok(addr)
}

/// An AF_UNIX stream socket, closed as it drops.
#[derive(Debug)]
pub struct Socket(SOCKET);

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: a socket this value owns, closed once; its pending operations end, and their
        // completions bring their own `Op`s back to the port.
        unsafe { closesocket(self.0) };
    }
}

impl AsSocket for Socket {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        // SAFETY: this value owns a live Winsock SOCKET, which has the same width as RawSocket on Windows;
        // the borrow cannot outlive it and transfers no ownership (only Socket::drop closes it).
        unsafe { BorrowedSocket::borrow_raw(self.0 as RawSocket) }
    }
}

/// Sets a live socket's physical mode (`ioctlsocket`, FIONBIO), shared by every descriptor for it. Unlike
/// a Stream wrapper's logical mode this remains in force after the wrapper drops (HEX-RT-17).
pub fn set_nonblocking(socket: BorrowedSocket<'_>, on: bool) -> io::Result<()> {
    let mut mode = u32::from(on);
    let raw = SOCKET::try_from(socket.as_raw_socket()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a socket value past Winsock's width",
        )
    })?;
    // SAFETY: the borrowed socket stays live and mode is a writable u32 for the duration of FIONBIO.
    let rc = unsafe { ioctlsocket(raw, FIONBIO, &raw mut mode) };
    if rc == SOCKET_ERROR {
        return Err(wsa_error());
    }
    Ok(())
}

/// Receives directly into the caller's buffer on an owned stream's socket (HEX-RT-17).
pub fn receive_socket(socket: BorrowedSocket<'_>, bytes: &mut [u8]) -> io::Result<usize> {
    let raw = SOCKET::try_from(socket.as_raw_socket()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a socket value past Winsock's width",
        )
    })?;
    let len = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    // SAFETY: the socket is borrowed live; len bytes of this caller's buffer are writable for the call.
    let n = unsafe { recv(raw, bytes.as_mut_ptr(), len, 0) };
    usize::try_from(n).map_err(|_| wsa_error())
}

/// Sends directly from the caller's buffer on an owned stream's socket (HEX-RT-17).
pub fn send_socket(socket: BorrowedSocket<'_>, bytes: &[u8]) -> io::Result<usize> {
    let raw = SOCKET::try_from(socket.as_raw_socket()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a socket value past Winsock's width",
        )
    })?;
    let len = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    // SAFETY: the socket is borrowed live; len readable bytes of the caller's buffer outlive the call.
    let n = unsafe { send(raw, bytes.as_ptr(), len, 0) };
    usize::try_from(n).map_err(|_| wsa_error())
}

/// Ends the requested halves of a stream socket without changing who owns it (HEX-RT-17).
pub fn shutdown_socket(socket: BorrowedSocket<'_>, how: std::net::Shutdown) -> io::Result<()> {
    let raw = SOCKET::try_from(socket.as_raw_socket()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a socket value past Winsock's width",
        )
    })?;
    let how = match how {
        std::net::Shutdown::Read => SD_RECEIVE,
        std::net::Shutdown::Write => SD_SEND,
        std::net::Shutdown::Both => SD_BOTH,
    };
    // SAFETY: a live borrowed socket and a documented shutdown direction.
    if unsafe { shutdown(raw, how) } == SOCKET_ERROR {
        return Err(wsa_error());
    }
    Ok(())
}

impl Socket {
    /// A new socket: overlapped (so it can be on a port), never inherited by a child.
    fn new() -> io::Result<Socket> {
        winsock()?;
        // SAFETY: no protocol info; the flags are documented ones. The socket returned is owned here.
        let s = unsafe {
            WSASocketW(
                i32::from(AF_UNIX),
                SOCK_STREAM,
                0,
                null(),
                0,
                WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
            )
        };
        if s == INVALID_SOCKET {
            return Err(wsa_error());
        }
        Ok(Socket(s))
    }

    /// A listener bound at `path` (a file afunix.sys makes; one there already is refused).
    pub fn listen(path: &Path) -> io::Result<Socket> {
        let addr = address(path)?;
        let s = Socket::new()?;
        // SAFETY: an address of its own length, alive for the call.
        if unsafe {
            bind(
                s.0,
                (&raw const addr).cast(),
                int_len(size_of::<SOCKADDR_UN>()),
            )
        } != 0
        {
            return Err(wsa_error());
        }
        // SAFETY: a bound socket this value owns.
        if unsafe { listen(s.0, BACKLOG) } != 0 {
            return Err(wsa_error());
        }
        Ok(s)
    }

    /// A blocking connection to the listener at `path` (afunix.sys connects at once, or refuses).
    pub fn connect(path: &Path) -> io::Result<Socket> {
        let addr = address(path)?;
        let s = Socket::new()?;
        // SAFETY: an address of its own length, alive for the call.
        if unsafe {
            connect(
                s.0,
                (&raw const addr).cast(),
                int_len(size_of::<SOCKADDR_UN>()),
            )
        } != 0
        {
            return Err(wsa_error());
        }
        Ok(s)
    }

    /// With `on`, receives and sends fail `WouldBlock` instead of waiting. The common client core waits on
    /// this socket's borrowed handle and retries only when it is ready (native plan step 8).
    pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        set_nonblocking(self.as_socket(), on)
    }

    /// Sends what it can of `bytes`, blocking unless this socket is non-blocking.
    pub fn send(&self, bytes: &[u8]) -> io::Result<usize> {
        send_socket(self.as_socket(), bytes)
    }

    /// Receives into `buf`, blocking unless this socket is non-blocking; zero is its peer's end.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        receive_socket(self.as_socket(), buf)
    }

    /// Shuts the socket both ways: its peer sees the end, and its own pending receive completes.
    pub fn shutdown(&self) {
        // SAFETY: a socket this value owns.
        unsafe { shutdown(self.0, SD_BOTH) };
    }

    /// Makes this socket's close a reset (`SO_LINGER`, no linger): its peer's read then fails at once, if no
    /// end-of-stream reached it before (afunix.sys doesn't wake a read that missed one). What the peer hasn't
    /// read is discarded.
    pub fn reset_on_close(&self) -> io::Result<()> {
        let linger = LINGER {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: a socket this value owns; the option's own value and its length.
        let rc = unsafe {
            setsockopt(
                self.0,
                SOL_SOCKET,
                SO_LINGER,
                (&raw const linger).cast(),
                int_len(size_of::<LINGER>()),
            )
        };
        if rc == SOCKET_ERROR {
            return Err(wsa_error());
        }
        Ok(())
    }

    /// The pid of the process that connected (afunix.sys records it at the connect).
    pub fn peer_pid(&self) -> io::Result<u32> {
        let mut pid = 0u32;
        let mut wrote = 0u32;
        // SAFETY: no input; `pid`, 4 bytes, is the output, alive for the call; not overlapped.
        let rc = unsafe {
            WSAIoctl(
                self.0,
                SIO_AF_UNIX_GETPEERPID,
                null(),
                0,
                (&raw mut pid).cast(),
                dword_len(size_of::<u32>()),
                &raw mut wrote,
                null_mut(),
                None,
            )
        };
        if rc != 0 {
            return Err(wsa_error());
        }
        Ok(pid)
    }
}

/// What an [`Op`] is doing.
enum Kind {
    /// An accept into `socket`, its addresses written into the op's buffer.
    Accept {
        socket: Socket,
        listener: SOCKET,
    },
    Recv,
    Send,
}

/// One overlapped operation: its `OVERLAPPED` first (so the kernel's pointer to it is a pointer to the
/// op), its buffer, and what it is for. The kernel's from its start until its completion is dequeued.
#[repr(C)]
struct Op {
    overlapped: OVERLAPPED,
    tag: u64,
    buf: Vec<u8>,
    kind: Kind,
}

impl Op {
    fn new(tag: u64, buf: Vec<u8>, kind: Kind) -> Box<Op> {
        Box::new(Op {
            // SAFETY: an all-zero OVERLAPPED is the documented start of an operation.
            overlapped: unsafe { zeroed() },
            tag,
            buf,
            kind,
        })
    }
}

/// A finished operation, or a wake or program's end, as the port reports it.
pub enum Done {
    /// [`Port::wake`] was called.
    Wake,
    /// The watched program `tag` ended.
    Program(u64),
    /// The socket watched under `tag` ([`SourceWatch`]) has what its arm watched for.
    Source(u64),
    /// An accept under `tag` finished: the connection, or why not.
    Accepted { tag: u64, conn: io::Result<Socket> },
    /// A receive under `tag` finished: its buffer, the first `n` bytes read (0: the peer's end).
    Received {
        tag: u64,
        buf: Vec<u8>,
        n: io::Result<usize>,
    },
    /// A send under `tag` finished: its buffer, the first `n` bytes sent.
    Sent {
        tag: u64,
        buf: Vec<u8>,
        n: io::Result<usize>,
    },
}

struct PortHandle(HANDLE);

// SAFETY: a completion port's handle may be used from any thread (it is the documented way threads hand
// each other work), and this value only closes it, once, when the last holder drops.
unsafe impl Send for PortHandle {}
// SAFETY: as above: every call on it is thread-safe in Windows.
unsafe impl Sync for PortHandle {}

impl Drop for PortHandle {
    fn drop(&mut self) {
        // SAFETY: the port this value made, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// One I/O completion port, shared by what posts to it (a program watch's callback holds it too).
#[derive(Clone)]
pub struct Port(Arc<PortHandle>);

impl Port {
    pub fn new() -> io::Result<Port> {
        // SAFETY: INVALID_HANDLE_VALUE and no port ask for a new port; one thread runs its completions.
        let h = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
        if h.is_null() {
            return Err(last_error());
        }
        Ok(Port(Arc::new(PortHandle(h))))
    }

    /// Puts `socket`'s operations on this port.
    pub fn add(&self, socket: &Socket) -> io::Result<()> {
        // SAFETY: an overlapped socket `socket` owns, and this port; a socket joins one port, once.
        let h = unsafe { CreateIoCompletionPort(socket.0 as HANDLE, self.0.0, KEY_IO, 0) };
        if h.is_null() {
            return Err(last_error());
        }
        Ok(())
    }

    /// Makes the next (or current) [`Port::wait`] return [`Done::Wake`].
    pub fn wake(&self) -> io::Result<()> {
        // SAFETY: this port; a post carries no OVERLAPPED, so nothing is the kernel's.
        if unsafe { PostQueuedCompletionStatus(self.0.0, 0, KEY_WAKE, null()) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// Waits at most `timeout` (`None`: until one comes) for completions, appending at most `max` to
    /// `done`. A timeout appends none.
    pub fn wait(
        &self,
        done: &mut Vec<Done>,
        timeout: Option<Duration>,
        max: usize,
    ) -> io::Result<()> {
        let room = max.clamp(1, 256);
        let mut entries: Vec<OVERLAPPED_ENTRY> = Vec::with_capacity(room);
        let ms = match timeout {
            None => INFINITE,
            Some(t) => u32::try_from(t.as_millis())
                .unwrap_or(INFINITE.saturating_sub(1))
                .min(INFINITE.saturating_sub(1)),
        };
        let mut got = 0u32;
        let ask = u32::try_from(room).unwrap_or(1);
        // SAFETY: room for `ask` entries, which the call writes the first `got` of; not alertable.
        let ok = unsafe {
            GetQueuedCompletionStatusEx(
                self.0.0,
                entries.as_mut_ptr(),
                ask,
                &raw mut got,
                ms,
                FALSE,
            )
        };
        if ok == 0 {
            let e = last_error();
            return match e.raw_os_error() {
                Some(c) if c == WAIT_TIMEOUT.cast_signed() => Ok(()),
                _ => Err(e),
            };
        }
        let got = usize::try_from(got).unwrap_or(0).min(room);
        // SAFETY: the call initialised the first `got` entries, within the capacity.
        unsafe { entries.set_len(got) };
        for e in entries {
            done.push(match e.lpCompletionKey {
                KEY_WAKE => Done::Wake,
                KEY_PROGRAM => Done::Program(e.lpOverlapped as usize as u64),
                KEY_SOURCE => Done::Source(e.lpOverlapped as usize as u64),
                _ => {
                    // SAFETY: on KEY_IO every entry's pointer is an `Op` a start handed the kernel by
                    // `Box::into_raw`, its OVERLAPPED its first field; its completion comes once, so it
                    // is taken back once, here.
                    let op = unsafe { Box::from_raw(e.lpOverlapped.cast::<Op>()) };
                    finished(*op, e.dwNumberOfBytesTransferred)
                }
            });
        }
        Ok(())
    }
}

/// What a dequeued operation did: its status is the OVERLAPPED's (`Internal`, an NTSTATUS), read through
/// the socket's own result call where it failed.
fn finished(op: Op, bytes: u32) -> Done {
    let failed = op.overlapped.Internal != 0;
    let n = if failed {
        Err(
            match u32::try_from(op.overlapped.Internal).unwrap_or(u32::MAX) {
                // STATUS_CANCELLED: its socket closed under it.
                0xC000_0120 => io::Error::from_raw_os_error(ERROR_OPERATION_ABORTED.cast_signed()),
                // STATUS_CONNECTION_RESET, STATUS_CONNECTION_ABORTED, STATUS_LOCAL_DISCONNECT,
                // STATUS_REMOTE_DISCONNECT: its peer reset it.
                0xC000_020D | 0xC000_0241 | 0xC000_013B | 0xC000_013C => {
                    io::Error::from_raw_os_error(WSAECONNRESET)
                }
                other => io::Error::other(format!(
                    "an overlapped socket call ended NTSTATUS {other:#x}"
                )),
            },
        )
    } else {
        Ok(usize::try_from(bytes).unwrap_or(0))
    };
    let Op { tag, buf, kind, .. } = op;
    match kind {
        Kind::Accept { socket, listener } => Done::Accepted {
            tag,
            conn: n.and_then(|_| {
                // The accepted socket takes the listener's properties, so its peer and shutdown work.
                // SAFETY: the listener's value, its own length, as SO_UPDATE_ACCEPT_CONTEXT takes.
                let rc = unsafe {
                    setsockopt(
                        socket.0,
                        SOL_SOCKET,
                        SO_UPDATE_ACCEPT_CONTEXT,
                        (&raw const listener).cast(),
                        int_len(size_of::<SOCKET>()),
                    )
                };
                if rc == SOCKET_ERROR {
                    return Err(wsa_error());
                }
                Ok(socket)
            }),
        },
        Kind::Recv => Done::Received { tag, buf, n },
        Kind::Send => Done::Sent { tag, buf, n },
    }
}

/// Whether a start's failure means it is under way (its completion will come) or done with no completion.
fn started(rc: i32) -> io::Result<()> {
    if rc == 0 {
        return Ok(());
    }
    let e = wsa_error();
    if e.raw_os_error() == Some(WSA_IO_PENDING) {
        Ok(())
    } else {
        Err(e)
    }
}

/// Starts accepting one connection on `listener`, reported on `port` as [`Done::Accepted`] under `tag`.
/// The connection's socket is on `port` already (a socket joins a port once: a second join is refused).
pub fn accept(port: &Port, listener: &Socket, tag: u64) -> io::Result<()> {
    let socket = Socket::new()?;
    port.add(&socket)?;
    let into = socket.0;
    let room = ADDRESS_ROOM.saturating_mul(2);
    let op = Box::into_raw(Op::new(
        tag,
        vec![0u8; room],
        Kind::Accept {
            socket,
            listener: listener.0,
        },
    ));
    let mut got = 0u32;
    // SAFETY: `op` is a live Box the kernel holds until its completion: its OVERLAPPED and its buffer of
    // both addresses' room (no data asked for) stay put, as the op is not touched again until the port
    // returns it. The accepted socket is the op's, unassociated with any connection yet.
    let ok: windows_sys::core::BOOL = unsafe {
        AcceptEx(
            listener.0,
            into,
            (*op).buf.as_mut_ptr().cast(),
            0,
            dword_len(ADDRESS_ROOM),
            dword_len(ADDRESS_ROOM),
            &raw mut got,
            &raw mut (*op).overlapped,
        )
    };
    let r = if ok != 0 {
        Ok(())
    } else {
        started(SOCKET_ERROR)
    };
    if r.is_err() {
        // SAFETY: the start failed, so no completion is queued: the op is this function's again, once.
        drop(unsafe { Box::from_raw(op) });
    }
    r
}

/// Starts a receive of up to `buf.len()` bytes on `socket`, reported as [`Done::Received`] under `tag`.
/// A refused start gives the buffer back.
pub fn receive(socket: &Socket, tag: u64, buf: Vec<u8>) -> Result<(), (io::Error, Vec<u8>)> {
    let op = Box::into_raw(Op::new(tag, buf, Kind::Recv));
    let mut flags = 0u32;
    // SAFETY: `op` is live until its completion (see `accept`); the WSABUF names its own buffer.
    let mut wsabuf = unsafe {
        WSABUF {
            len: u32::try_from((*op).buf.len()).unwrap_or(u32::MAX),
            buf: (*op).buf.as_mut_ptr(),
        }
    };
    // SAFETY: as above: the buffer and OVERLAPPED are the kernel's until the completion, the WSABUF array
    // is read during the call only, and no completion routine is given.
    let rc = unsafe {
        WSARecv(
            socket.0,
            &raw mut wsabuf,
            1,
            null_mut(),
            &raw mut flags,
            &raw mut (*op).overlapped,
            None,
        )
    };
    started(rc).map_err(|e| {
        // SAFETY: the start failed, so no completion is queued: the op is this function's again, once.
        let op = unsafe { Box::from_raw(op) };
        (e, op.buf)
    })
}

/// Starts a send of all of `bytes` on `socket`, reported as [`Done::Sent`] under `tag`. A refused start
/// gives the bytes back.
pub fn send_all(socket: &Socket, tag: u64, bytes: Vec<u8>) -> Result<(), (io::Error, Vec<u8>)> {
    let op = Box::into_raw(Op::new(tag, bytes, Kind::Send));
    // SAFETY: `op` is live until its completion (see `accept`); the WSABUF names its own bytes.
    let mut wsabuf = unsafe {
        WSABUF {
            len: u32::try_from((*op).buf.len()).unwrap_or(u32::MAX),
            buf: (*op).buf.as_mut_ptr(),
        }
    };
    // SAFETY: as in `receive`.
    let rc = unsafe {
        WSASend(
            socket.0,
            &raw mut wsabuf,
            1,
            null_mut(),
            0,
            &raw mut (*op).overlapped,
            None,
        )
    };
    started(rc).map_err(|e| {
        // SAFETY: the start failed, so no completion is queued: the op is this function's again, once.
        let op = unsafe { Box::from_raw(op) };
        (e, op.buf)
    })
}

// ntdll's wait completion packets, which windows-sys doesn't bind (undocumented in the SDK; the Windows
// thread pool and Go's runtime use them): a packet associated with an object and a port, which the kernel
// queues to the port itself once the object is signalled, with no thread between.
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateWaitCompletionPacket(
        packet: *mut HANDLE,
        access: u32,
        attributes: *const c_void,
    ) -> i32;
    fn NtAssociateWaitCompletionPacket(
        packet: HANDLE,
        port: HANDLE,
        target: HANDLE,
        key: *const c_void,
        apc: *const c_void,
        status: i32,
        information: usize,
        already: *mut u8,
    ) -> i32;
    fn NtCancelWaitCompletionPacket(packet: HANDLE, remove_signaled: u8) -> i32;
}

/// The access a packet is made with (`GENERIC_ALL`, as Go's runtime makes its packets).
const PACKET_ACCESS: u32 = 0x1000_0000;

/// A new wait completion packet, associated with nothing yet.
fn wait_packet() -> io::Result<OwnedHandle> {
    let mut raw: HANDLE = null_mut();
    // SAFETY: the packet's handle is written on success; no attributes.
    let status = unsafe { NtCreateWaitCompletionPacket(&raw mut raw, PACKET_ACCESS, null()) };
    if status < 0 {
        return Err(io::Error::other(format!(
            "NtCreateWaitCompletionPacket ended NTSTATUS {status:#x}"
        )));
    }
    // SAFETY: a handle just made, owned from here.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// A watched program: its process handle (which names it, not its pid, so a kill can't reach a process
/// given the pid since) and the wait completion packet that queues its end to the port. Chosen over a
/// registered wait (`RegisterWaitForSingleObject`, a pool thread's hop on every end) by measurement:
/// signal to notice p50 12.9 against 28.2 us, p99 0.82 against 2.43 ms, faster in 281 of 300 paired rounds
/// under load (bench/native/records/endwait-jupiter-20261108T022608Z.txt). That run fell in jupiter's NTFS
/// stall and is void as timing, so the choice is provisional until it is retaken on a clean jupiter.
pub struct ProgramWatch {
    _process: OwnedHandle,
    packet: OwnedHandle,
}

/// Watches the program `pid` until it ends, reported once on `port` as [`Done::Program`] under `tag`. One that
/// ended already is reported at once: a packet associated with a process signalled already is queued as it is
/// associated (`a_program_ended_before_its_watch_is_reported_once`), so nothing posts its end again.
pub fn watch_program(port: &Port, pid: u32, tag: u64) -> io::Result<ProgramWatch> {
    let tag = usize::try_from(tag).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "a token past a pointer's size")
    })?;
    // SAFETY: opens the process `pid` names now for waiting on and reading its user; a process
    // stays (as an object) while a handle is open, so this handle names it until it is closed.
    let h = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            FALSE,
            pid,
        )
    };
    if h.is_null() {
        let error = last_error();
        // OpenProcess reports INVALID_PARAMETER when the program already ended and its pid is gone.
        if error
            .raw_os_error()
            .and_then(|code| u32::try_from(code).ok())
            == Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER)
        {
            return Err(io::Error::new(io::ErrorKind::NotFound, error));
        }
        return Err(error);
    }
    // SAFETY: a handle just opened, owned from here.
    let process = unsafe { OwnedHandle::from_raw_handle(h) };
    let packet = wait_packet()?;
    // Told whether the process had ended already, which changes nothing: its packet is queued either way.
    let mut already = 0u8;
    // SAFETY: the packet, the port and the process are live handles (the port's for as long as the watch
    // is, the Os owning both); the key and the token are posted back as values, which the port's
    // KEY_PROGRAM tells its wait never to take as an `Op`.
    let status = unsafe {
        NtAssociateWaitCompletionPacket(
            packet.as_raw_handle(),
            port.0.0,
            process.as_raw_handle(),
            KEY_PROGRAM as *const c_void,
            tag as *const c_void,
            0,
            0,
            &raw mut already,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "NtAssociateWaitCompletionPacket ended NTSTATUS {status:#x}"
        )));
    }
    Ok(ProgramWatch {
        _process: process,
        packet,
    })
}

impl Drop for ProgramWatch {
    fn drop(&mut self) {
        // The packet's wait is cancelled, and a completion it queued already removed from the port, so an
        // unwatched program's end comes to no one; then both handles close.
        // SAFETY: the packet this watch made, cancelled once before it closes.
        unsafe { NtCancelWaitCompletionPacket(self.packet.as_raw_handle(), 1) };
    }
}

/// A host's own connection watched on the port (native plan step 8, the node core's `Source`: its guest's
/// control connection), by its socket's network events: Winsock records on an event of the watch's own
/// whether the socket has bytes to read, has ended, or has room to send (`WSAEventSelect`), and a wait
/// completion packet queues that event's signal to the port as [`Done::Source`], as a program's end is
/// queued, no thread between. Not a zero-byte receive, which tells when bytes or the end came but never when
/// room came back. The socket stays its owner's: it never joins the port, none of its I/O comes there, and
/// none of it waits (Winsock makes a watched socket non-blocking, as a source's reads and writes are).
pub struct SourceWatch {
    event: OwnedHandle,
    packet: OwnedHandle,
    tag: usize,
}

/// A borrowed socket as Winsock names it.
fn raw_socket(socket: BorrowedSocket<'_>) -> io::Result<SOCKET> {
    SOCKET::try_from(socket.as_raw_socket()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a socket past a pointer's size",
        )
    })
}

impl SourceWatch {
    /// A watch reported under `tag`, armed by [`SourceWatch::arm`].
    pub fn new(tag: u64) -> io::Result<SourceWatch> {
        let tag = usize::try_from(tag).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "a token past a pointer's size")
        })?;
        // SAFETY: no attributes and no name: a manual-reset event, unsignalled, as `WSACreateEvent` makes
        // one; owned below.
        let h = unsafe { CreateEventW(null(), TRUE, FALSE, null()) };
        if h.is_null() {
            return Err(last_error());
        }
        // SAFETY: a handle just made, owned from here.
        let event = unsafe { OwnedHandle::from_raw_handle(h) };
        Ok(SourceWatch {
            event,
            packet: wait_packet()?,
            tag,
        })
    }

    /// Arms the watch on `socket`, borrowed for the call: reported once on `port` when the socket has bytes
    /// to read or its peer has ended it, or, with `write`, room to send; at once if it has as it is armed
    /// (Winsock records what holds as a watch is set, its "persistence of network events"). The arm before is
    /// taken back first, with its report if queued and not yet taken; a report already on its way can't be
    /// (the packet's cancel answers `STATUS_PENDING`), and stands for this arm: once it is taken, the next arm
    /// reads afresh what holds.
    pub fn arm(&self, port: &Port, socket: BorrowedSocket<'_>, write: bool) -> io::Result<()> {
        // SAFETY: the packet this watch made: its wait, and its report queued and not taken, are cancelled.
        let cancelled = unsafe { NtCancelWaitCompletionPacket(self.packet.as_raw_handle(), 1) };
        if cancelled == STATUS_PENDING {
            return Ok(());
        }
        // SAFETY: the event this watch made.
        if unsafe { ResetEvent(self.event.as_raw_handle()) } == 0 {
            return Err(last_error());
        }
        let events = if write {
            FD_READ | FD_CLOSE | FD_WRITE
        } else {
            FD_READ | FD_CLOSE
        };
        // SAFETY: a socket borrowed for the call, and the event this watch owns (Winsock takes a reference
        // of its own to the event it records on).
        let rc = unsafe {
            WSAEventSelect(
                raw_socket(socket)?,
                self.event.as_raw_handle().addr().cast_signed(),
                events.cast_signed(),
            )
        };
        if rc == SOCKET_ERROR {
            return Err(wsa_error());
        }
        // Told whether the event was set already, which changes nothing: its packet is queued either way.
        let mut already = 0u8;
        // SAFETY: as in `watch_program`: the packet, the port and the event are live handles; the key and the
        // tag are posted back as values, which the port's KEY_SOURCE tells its wait never to take as an `Op`.
        let status = unsafe {
            NtAssociateWaitCompletionPacket(
                self.packet.as_raw_handle(),
                port.0.0,
                self.event.as_raw_handle(),
                KEY_SOURCE as *const c_void,
                self.tag as *const c_void,
                0,
                0,
                &raw mut already,
            )
        };
        if status < 0 {
            return Err(io::Error::other(format!(
                "NtAssociateWaitCompletionPacket ended NTSTATUS {status:#x}"
            )));
        }
        Ok(())
    }

    /// Stops watching `socket`, borrowed for the call: Winsock records nothing more on the watch's event (the
    /// socket stays non-blocking), and a report queued and not taken is taken back as the watch drops.
    pub fn stop(self, socket: BorrowedSocket<'_>) {
        if let Ok(raw) = raw_socket(socket) {
            // SAFETY: a socket borrowed for the call; no events and no event end its watch.
            unsafe { WSAEventSelect(raw, 0, 0) };
        }
    }
}

impl Drop for SourceWatch {
    fn drop(&mut self) {
        // SAFETY: the packet this watch made, cancelled once before it closes, a report it queued removed
        // from the port.
        unsafe { NtCancelWaitCompletionPacket(self.packet.as_raw_handle(), 1) };
    }
}

/// A token's user, as `TOKEN_USER` and the SID after it, in a buffer of its own.
fn token_user(process: HANDLE) -> io::Result<Vec<u64>> {
    let mut token: HANDLE = null_mut();
    // SAFETY: a process handle opened with PROCESS_QUERY_LIMITED_INFORMATION (or this process's own); the
    // token handle written is owned below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(last_error());
    }
    // SAFETY: the token just opened, owned from here.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut need = 0u32;
    // SAFETY: a size query: no buffer, the size needed written to `need`.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            null_mut(),
            0,
            &raw mut need,
        )
    };
    let words = usize::try_from(need)
        .unwrap_or(0)
        .div_ceil(size_of::<u64>())
        .max(size_of::<TOKEN_USER>().div_ceil(size_of::<u64>()));
    // u64s, so the TOKEN_USER at its start is aligned.
    let mut buf = vec![0u64; words];
    let room = u32::try_from(words.saturating_mul(size_of::<u64>())).unwrap_or(u32::MAX);
    // SAFETY: `room` writable bytes of `buf`, aligned for TOKEN_USER.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            room,
            &raw mut need,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(buf)
}

/// The most ends one message carries: an exec's stdin, stdout and stderr (`hex_core::wire::MAX_FDS`).
pub const MAX_ENDS: usize = 3;
/// The most bytes an envelope ([`envelope`]) adds to a message: its count and that many handle values.
pub const ENVELOPE_MAX: usize = 1 + MAX_ENDS * size_of::<u64>();

fn envelope_fault(why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("a message's ends: {why}"),
    )
}

/// A message as the Windows node sends it to its caller: how many ends ride with it, each end's handle
/// value in the caller's process (the node gave the caller its own handle, [`Peer::give`]), then the
/// message. Windows' AF_UNIX passes no handles, so they ride in the stream (the stdio decision's pick, dup:
/// docs/design/runtime.md). Refused past [`MAX_ENDS`].
pub fn envelope(values: &[u64], message: &[u8]) -> io::Result<Vec<u8>> {
    let count = u8::try_from(values.len())
        .ok()
        .filter(|n| usize::from(*n) <= MAX_ENDS)
        .ok_or_else(|| envelope_fault("more ends than a message carries"))?;
    let mut out = Vec::with_capacity(ENVELOPE_MAX.saturating_add(message.len()));
    out.push(count);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(message);
    Ok(out)
}

/// A message from the Windows node, taken apart ([`envelope`]): the handle values of the ends that rode
/// with it, each this process's own now, and the message. Refused when malformed: a count past
/// [`MAX_ENDS`], or fewer bytes than its values.
pub fn unenvelope(payload: &[u8]) -> io::Result<(Vec<u64>, &[u8])> {
    let (&count, rest) = payload
        .split_first()
        .ok_or_else(|| envelope_fault("an empty message"))?;
    let count = usize::from(count);
    if count > MAX_ENDS {
        return Err(envelope_fault("more ends than a message carries"));
    }
    let (raw, message) = rest
        .split_at_checked(count.saturating_mul(size_of::<u64>()))
        .ok_or_else(|| envelope_fault("fewer bytes than its ends"))?;
    let (values, _) = raw.as_chunks::<{ size_of::<u64>() }>();
    let values = values
        .iter()
        .map(|value| u64::from_le_bytes(*value))
        .collect();
    Ok((values, message))
}

/// A message from the Windows node, taken apart ([`unenvelope`]) with the ends that rode with it as handles
/// this process owns: the node gave this process each handle the envelope names ([`Peer::give`]), for this
/// message alone. They close with the answer that doesn't take them.
pub fn take_given(payload: &[u8]) -> io::Result<(Vec<OwnedHandle>, &[u8])> {
    let (values, message) = unenvelope(payload)?;
    let ends = values
        .into_iter()
        .filter_map(|value| usize::try_from(value).ok())
        // SAFETY: the node gave this process each handle named here (DuplicateHandle into it) for this
        // message alone, so each is this process's, owned by this value from now on and closed once.
        .map(|value| unsafe { OwnedHandle::from_raw_handle(value as RawHandle) })
        .collect();
    Ok((ends, message))
}

/// A connected peer's process, opened to give it handles (`PROCESS_DUP_HANDLE`) and held for its
/// connection's life, so its pid can't come to name another process meanwhile.
pub struct Peer(OwnedHandle);

impl Peer {
    /// Opens process `pid` to give it handles: refused where this process may not open it so.
    pub fn open(pid: u32) -> io::Result<Peer> {
        // SAFETY: opens a process by pid with the one right named; the handle returned is owned below.
        let h = unsafe { OpenProcess(PROCESS_DUP_HANDLE, FALSE, pid) };
        if h.is_null() {
            return Err(last_error());
        }
        // SAFETY: OpenProcess succeeded, so `h` is a process handle this process now owns.
        Ok(Peer(unsafe { OwnedHandle::from_raw_handle(h) }))
    }

    /// Gives the peer a handle of its own to what `end` names (`DuplicateHandle`, the same access): its
    /// value in the peer's process, which only that process can use. `end` stays this process's.
    pub fn give(&self, end: BorrowedHandle<'_>) -> io::Result<u64> {
        let mut theirs: HANDLE = null_mut();
        // SAFETY: `end` is a live handle of this process and `self.0` the peer's, opened with
        // PROCESS_DUP_HANDLE; `theirs` is written. The source is not closed.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                end.as_raw_handle(),
                self.0.as_raw_handle(),
                &raw mut theirs,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        u64::try_from(theirs as usize).map_err(|_| envelope_fault("a handle value past 64 bits"))
    }

    /// Closes a handle given to the peer that it never learned of, its message not sent
    /// (`DuplicateHandle` with `DUPLICATE_CLOSE_SOURCE` and no target).
    pub fn take_back(&self, value: u64) {
        let Ok(value) = usize::try_from(value) else {
            return;
        };
        // SAFETY: closes the handle `value` in the peer's process, one this side gave it and the peer was
        // never told of; no handle is made here (no target).
        unsafe {
            DuplicateHandle(
                self.0.as_raw_handle(),
                value as HANDLE,
                null_mut(),
                null_mut(),
                0,
                FALSE,
                DUPLICATE_CLOSE_SOURCE,
            )
        };
    }
}

/// Whether the process `pid` runs as this process's user or as LocalSystem (the node's peers: this user,
/// or the machine's own account, as Linux's are this user or root).
pub fn same_user_or_system(pid: u32) -> io::Result<bool> {
    // SAFETY: opens `pid` to read its token; owned below.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if h.is_null() {
        return Err(last_error());
    }
    // SAFETY: a handle just opened, owned from here.
    let theirs = unsafe { OwnedHandle::from_raw_handle(h) };
    let theirs = token_user(theirs.as_raw_handle())?;
    // SAFETY: the pseudo-handle of this process, which needs no closing.
    let mine = token_user(unsafe { GetCurrentProcess() })?;
    // SAFETY: each buffer holds a TOKEN_USER at its (u64-aligned) start, whose SID points within it, both
    // written by GetTokenInformation above and alive here.
    let (a, b) = unsafe {
        (
            (*theirs.as_ptr().cast::<TOKEN_USER>()).User.Sid,
            (*mine.as_ptr().cast::<TOKEN_USER>()).User.Sid,
        )
    };
    // SAFETY: two valid SIDs, as above.
    Ok(unsafe { EqualSid(a, b) } != 0 || unsafe { IsWellKnownSid(a, WinLocalSystemSid) } != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stdio decision's dup (native plan step 8): do put a message in an envelope with no ends' values,
    /// one, and three, and take each apart. Expect the same values and message back, each envelope one byte
    /// and eight per end longer than its message.
    #[test]
    fn a_message_and_its_ends_come_back_from_their_envelope() {
        let message = b"an answer";
        for values in [vec![], vec![7u64], vec![1, u64::MAX, 0x8000]] {
            let enveloped = envelope(&values, message).unwrap();
            assert_eq!(enveloped.len(), 1 + 8 * values.len() + message.len());
            let (back, rest) = unenvelope(&enveloped).unwrap();
            assert_eq!(back, values);
            assert_eq!(rest, message);
        }
    }

    /// do put four ends' values in an envelope, and take apart one that counts four, one short of its
    /// values, and an empty one. Expect each refused: no message carries more than three ends.
    #[test]
    fn an_envelope_past_its_bound_or_short_of_its_values_is_refused() {
        assert!(envelope(&[1, 2, 3, 4], b"m").is_err());
        assert!(unenvelope(&[4u8; 40]).is_err());
        assert!(unenvelope(&[2u8, 1, 2, 3]).is_err());
        assert!(unenvelope(&[]).is_err());
    }

    /// Native plan step 8, a host's own connection on the port (`watch_source` on Windows): do arm a socket's
    /// watch for room on a fresh connection, then for reading while it is quiet, after a byte comes, and again
    /// while that byte waits. Expect each arm reported once at most: at once when what it watches for holds
    /// as it is armed (room, a byte waiting), not while the socket is quiet, and once when the byte comes.
    #[test]
    fn a_sockets_watch_reports_each_arm_once() {
        use std::io::Write as _;
        use std::os::windows::io::AsSocket as _;
        const PATIENCE: Duration = Duration::from_secs(30);
        const TAG: u64 = 7;
        let port = Port::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        // The reports of the watch: the first waited for at most `patience`, then each queued with it.
        let reports = |patience: Duration| {
            let mut done = Vec::new();
            port.wait(&mut done, Some(patience), 16).unwrap();
            port.wait(&mut done, Some(Duration::ZERO), 16).unwrap();
            done.iter()
                .filter(|d| matches!(d, Done::Source(TAG)))
                .count()
        };
        let watch = SourceWatch::new(TAG).unwrap();
        watch.arm(&port, source.as_socket(), true).unwrap();
        assert_eq!(reports(PATIENCE), 1, "a connection with room, armed for it");
        watch.arm(&port, source.as_socket(), false).unwrap();
        assert_eq!(reports(Duration::ZERO), 0, "a quiet connection");
        peer.write_all(b"x").unwrap();
        assert_eq!(reports(PATIENCE), 1, "a connection a byte came to");
        watch.arm(&port, source.as_socket(), false).unwrap();
        assert_eq!(
            reports(PATIENCE),
            1,
            "a connection armed again while its byte waits"
        );
        watch.stop(source.as_socket());
    }

    /// Native plan step 8: do watch a program that has ended already (its process still held, so its pid
    /// names it). Expect its end reported once: a packet associated with an object signalled already is
    /// queued at once (as `a_sockets_watch_reports_each_arm_once` finds), so nothing posts it again.
    #[test]
    fn a_program_ended_before_its_watch_is_reported_once() {
        const PATIENCE: Duration = Duration::from_secs(30);
        const TAG: u64 = 9;
        let port = Port::new().unwrap();
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        child.wait().unwrap();
        let watch = watch_program(&port, child.id(), TAG).unwrap();
        let mut done = Vec::new();
        port.wait(&mut done, Some(PATIENCE), 16).unwrap();
        port.wait(&mut done, Some(Duration::ZERO), 16).unwrap();
        assert_eq!(
            done.iter()
                .filter(|d| matches!(d, Done::Program(TAG)))
                .count(),
            1,
            "a program ended before its watch wasn't reported once"
        );
        drop(watch);
    }
}
