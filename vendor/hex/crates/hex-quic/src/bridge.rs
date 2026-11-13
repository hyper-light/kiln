//! A connection as a local stream socket (docs/design/hex-quic.md Q3): the owner gets one end of a socket pair,
//! a byte stream the node agent's client and the guest's protocol read and write as they do vsock's; two threads,
//! one a direction with one frame's buffer, copy between the pair's other end and the connection. Each ends on a
//! fact: the owner's close, the peer's End, or the connection's end. The bridge lasts as long as its connection:
//! once both halves have ended, or the connection is gone, both threads end and the pair's other end closes, so the
//! owner's socket ends both ways however its halves stood (AC-T7.4.1, HEX-QUIC-25 B3). A socket pair carries no reset,
//! so a connection cut short (gone before its peer's End) ends the owner's socket as cleanly as an End does; the bridge
//! records the cut, and the owner's end [`bridged`] gives reads it after the bytes that came before it (Q11).

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::Arc;
use std::time::Duration;

use hex_client::Cut;
use hex_client::guest::{Duplex, DuplexOwner, Handle, own_duplex};
use hex_core::lanes::{DATA_BOUND, Why};

use crate::Conn;

/// The owner's end: a Unix socket pair's on Unix; a loopback TCP pair's on Windows, whose std has no socket pair.
#[cfg(unix)]
pub type Local = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type Local = std::net::TcpStream;

#[cfg(unix)]
fn pair() -> io::Result<(Local, Local)> {
    Local::pair()
}

/// A loopback TCP pair: a listener on an ephemeral port, connected to and accepted from; only the connection whose
/// address is the connecting socket's own is taken, so another local process that connects first is closed. The
/// kernel queues at most the listener's backlog before ours, std's own bound on `listen`.
#[cfg(windows)]
fn pair() -> io::Result<(Local, Local)> {
    /// std's `TcpListener::bind` backlog (library/std/src/sys/net/connection/socket.rs, `listen(128)`).
    const BACKLOG: usize = 128;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let ours = Local::connect(listener.local_addr()?)?;
    let want = ours.local_addr()?;
    for _ in 0..=BACKLOG {
        let (theirs, from) = listener.accept()?;
        if from == want {
            ours.set_nodelay(true)?;
            theirs.set_nodelay(true)?;
            return Ok((ours, theirs));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "loopback connections other than ours filled the pair's listener",
    ))
}

/// `conn` as a local stream socket: the owner's end, its bytes the connection's. Its reads end cleanly whatever ended
/// the connection; [`bridged`]'s tell a cut.
pub fn socket(conn: Conn) -> io::Result<Local> {
    bridged(conn).map(|bridged| bridged.sock)
}

/// The owner's end of a bridged connection, read as the connection is: the bytes that came, then its End, or the
/// error that cut it short (a reset, the link's end: what its connection's read ended with before the peer's End,
/// recorded as the bridge ends the owner's socket), which the socket pair alone would read as an End.
pub(crate) struct Bridged {
    sock: Local,
    cut: Cut,
}

/// `conn` as the owner's end of a bridge that tells a cut.
pub(crate) fn bridged(conn: Conn) -> io::Result<Bridged> {
    let (owner, ours) = pair()?;
    let conn = Arc::new(conn);
    let cut = Cut::default();
    let up = (Arc::clone(&conn), ours.try_clone()?);
    std::thread::Builder::new()
        .name("hex-quic-up".to_owned())
        .spawn(move || upward(&up.0, up.1))?;
    let recorded = cut.clone();
    std::thread::Builder::new()
        .name("hex-quic-down".to_owned())
        .spawn(move || downward(&conn, ours, &recorded))?;
    Ok(Bridged { sock: owner, cut })
}

impl Read for Bridged {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.sock.read(buf)?;
        if n == 0
            && !buf.is_empty()
            && let Some(cut) = self.cut.error()
        {
            return Err(cut);
        }
        Ok(n)
    }
}

impl Write for Bridged {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.sock.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

impl Duplex for Bridged {
    fn try_clone(&self) -> io::Result<DuplexOwner> {
        own_duplex(Bridged {
            sock: Local::try_clone(&self.sock)?,
            cut: self.cut.clone(),
        })
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        Local::shutdown(&self.sock, how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Local::set_write_timeout(&self.sock, timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Local::set_read_timeout(&self.sock, timeout)
    }

    fn handle(&self) -> Handle<'_> {
        Duplex::handle(&self.sock)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        Local::set_nonblocking(&self.sock, on)
    }

    /// The cut the bridge records: its owner's caller reads the socket itself, whose pair can't report it.
    fn cut(&self) -> Option<Cut> {
        Some(self.cut.clone())
    }
}

/// What the owner writes, to the peer; the owner's close ends this side's half. The connection's end ends it too:
/// [`downward`] then ends its reading.
fn upward(conn: &Conn, mut from: Local) {
    let mut buf = vec![0u8; DATA_BOUND];
    loop {
        #[cfg(target_os = "macos")]
        if hex_client::wait_readable(std::os::fd::AsFd::as_fd(&from)).is_err() {
            conn.reset(Why::Gone);
            return;
        }
        match from.read(&mut buf) {
            Ok(0) => {
                let _ = conn.end(None);
                return;
            }
            Ok(n) => {
                if conn
                    .write_all(buf.get(..n).unwrap_or_default(), None)
                    .is_err()
                {
                    let _ = from.shutdown(Shutdown::Read);
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                conn.reset(Why::Gone);
                return;
            }
        }
    }
}

/// What the peer sends, to the owner; its End is the owner's end of input, and a cut its cut, recorded before the
/// owner's socket ends. It stays until the connection is done both ways or gone, so the connection's end ends
/// [`upward`] too, whichever half ended first.
fn downward(conn: &Conn, mut to: Local, cut: &Cut) {
    let mut buf = vec![0u8; DATA_BOUND];
    loop {
        match conn.read(&mut buf, None) {
            Ok(0) => {
                let _ = to.shutdown(Shutdown::Write);
                if conn.finished().is_err() {
                    let _ = to.shutdown(Shutdown::Read);
                }
                return;
            }
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    conn.reset(Why::Gone);
                    end(&to);
                    return;
                }
            }
            Err(e) => {
                cut.record(&e);
                end(&to);
                return;
            }
        }
    }
}

/// Ends the pair's other end one way at a time, writing first, so the owner reads its end whichever side half-closed
/// first: macOS refuses `SHUT_RDWR` on a socket whose reading has ended (ENOTCONN) and shuts nothing (Shards M236,
/// D175).
fn end(to: &Local) {
    let _ = to.shutdown(Shutdown::Write);
    let _ = to.shutdown(Shutdown::Read);
}

#[cfg(all(test, unix))]
mod tests;
