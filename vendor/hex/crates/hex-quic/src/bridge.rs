//! A connection as a local stream socket (docs/design/hex-quic.md Q3): the owner gets one end of a socket pair,
//! a byte stream the node agent's client and the guest's protocol read and write as they do vsock's; two threads,
//! one a direction with one frame's buffer, copy between the pair's other end and the connection. Each ends on a
//! fact: the owner's close, the peer's End, or the connection's end.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::Arc;

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

/// `conn` as a local stream socket: the owner's end, its bytes the connection's.
pub fn socket(conn: Conn) -> io::Result<Local> {
    let (owner, ours) = pair()?;
    let conn = Arc::new(conn);
    let up = (Arc::clone(&conn), ours.try_clone()?);
    std::thread::Builder::new()
        .name("hex-quic-up".to_owned())
        .spawn(move || upward(&up.0, up.1))?;
    std::thread::Builder::new()
        .name("hex-quic-down".to_owned())
        .spawn(move || downward(&conn, ours))?;
    Ok(owner)
}

/// What the owner writes, to the peer; the owner's close ends this side's half.
fn upward(conn: &Conn, mut from: Local) {
    let mut buf = vec![0u8; DATA_BOUND];
    loop {
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

/// What the peer sends, to the owner; its End is the owner's end of input.
fn downward(conn: &Conn, mut to: Local) {
    let mut buf = vec![0u8; DATA_BOUND];
    loop {
        match conn.read(&mut buf, None) {
            Ok(0) => {
                let _ = to.shutdown(Shutdown::Write);
                return;
            }
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    conn.reset(Why::Gone);
                    let _ = to.shutdown(Shutdown::Both);
                    return;
                }
            }
            Err(_) => {
                let _ = to.shutdown(Shutdown::Both);
                return;
            }
        }
    }
}
