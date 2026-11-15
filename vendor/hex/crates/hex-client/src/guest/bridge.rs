//! A connection that holds no OS handle as a local stream socket: the owner gets one end of a socket pair, a byte
//! stream the node agent's client and the guest's protocol read and write as they do vsock's, an event loop waiting on
//! its handle; what carries the connection's bytes copies between the pair's other end and the connection, as
//! `hex_quic::bridge` does for a QUIC stream (docs/design/hex-quic.md Q3) and [`super::lanes`] for a Shards socket
//! lane. A socket pair carries no reset, so a connection cut short ends the owner's socket as cleanly as its end in
//! order does; the copier records the cut ([`Cut`]), and the owner's end, [`Bridged`], gives reads it after the bytes
//! that came before it (Q11).

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::Arc;
use std::time::Duration;

use super::{Duplex, DuplexOwner, Handle, own_duplex};
use crate::Cut;

/// The owner's end: a Unix socket pair's on Unix; a loopback TCP pair's on Windows, whose std has no socket pair.
#[cfg(unix)]
pub type Local = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type Local = std::net::TcpStream;

/// A socket pair: the owner's end, then its copier's.
#[cfg(unix)]
pub fn pair() -> io::Result<(Local, Local)> {
    Local::pair()
}

/// A loopback TCP pair: a listener on an ephemeral port, connected to and accepted from; only the connection whose
/// address is the connecting socket's own is taken, so another local process that connects first is closed. The
/// kernel queues at most the listener's backlog before ours, std's own bound on `listen`.
#[cfg(windows)]
pub fn pair() -> io::Result<(Local, Local)> {
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

/// The owner's end of a bridged connection, read as the connection is: the bytes that came, then its End, or the
/// error that cut it short (a reset, the link's end: what its connection's read ended with before the peer's End,
/// recorded as the bridge ends the owner's socket), which the socket pair alone would read as an End. Its clones share
/// its one socket ([`Duplex::try_clone`]): no duplicate, which Windows makes inheritable (the review's D-5).
pub struct Bridged {
    sock: Arc<Local>,
    cut: Cut,
}

impl Bridged {
    /// The owner's end `sock` of a bridge whose copier records what cut its connection in `cut`.
    pub fn new(sock: Local, cut: Cut) -> Self {
        Self {
            sock: Arc::new(sock),
            cut,
        }
    }

    /// The owner's end as the local socket alone, its cut no longer told; refused while a clone shares it.
    pub fn into_socket(self) -> io::Result<Local> {
        Arc::try_unwrap(self.sock)
            .map_err(|_| io::Error::other("a clone of the bridged connection shares its socket"))
    }
}

impl Read for Bridged {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut sock: &Local = &self.sock;
        let n = sock.read(buf)?;
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
    /// On Windows a cut connection's writes fail here, with the cut: its bridge never shuts the pair's receiving, whose
    /// reset would drop what the owner hasn't read ([`end`]). On Unix the pair's other end stops reading.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        #[cfg(windows)]
        if let Some(cut) = self.cut.error() {
            return Err(cut);
        }
        let mut sock: &Local = &self.sock;
        sock.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut sock: &Local = &self.sock;
        sock.flush()
    }
}

impl Duplex for Bridged {
    /// Another handle on the same socket, shared, never a duplicate.
    fn try_clone(&self) -> io::Result<DuplexOwner> {
        own_duplex(Bridged {
            sock: Arc::clone(&self.sock),
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
        Duplex::handle(&*self.sock)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        Local::set_nonblocking(&self.sock, on)
    }

    /// The cut the bridge records: its owner's caller reads the socket itself, whose pair can't report it.
    fn cut(&self) -> Option<Cut> {
        Some(self.cut.clone())
    }
}

/// Ends the pair's other end so the owner reads its end, after every byte that came before it. On Unix one way at a
/// time, writing first, so the owner reads its end whichever side half-closed first: macOS refuses `SHUT_RDWR` on a
/// socket whose reading has ended (ENOTCONN) and shuts nothing (Shards M236, D175); its reading shut, the owner's
/// writes fail. On Windows its writing alone: Winsock resets a TCP connection whose receiving is shut while bytes wait
/// or come (`shutdown`'s `SD_RECEIVE`), and the reset drops what the owner hasn't read yet (the review's D-4), so a cut
/// connection's writes fail at [`Bridged`]'s write instead.
pub fn end(to: &Local) {
    let _ = to.shutdown(Shutdown::Write);
    #[cfg(unix)]
    let _ = to.shutdown(Shutdown::Read);
}
