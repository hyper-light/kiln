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

use hex_client::Cut;
pub use hex_client::guest::bridge::Local;
use hex_client::guest::bridge::{Bridged, end, pair};
use hex_core::lanes::{DATA_BOUND, Why};

use crate::Conn;

/// `conn` as a local stream socket: the owner's end, its bytes the connection's. Its reads end cleanly whatever ended
/// the connection; [`bridged`]'s tell a cut.
pub fn socket(conn: Conn) -> io::Result<Local> {
    bridged(conn).and_then(Bridged::into_socket)
}

/// `conn` as the owner's end of a bridge that tells a cut. Its two threads share the pair's other end, one reading it
/// and one writing it: no duplicate, which Windows makes inheritable (the review's D-5).
pub(crate) fn bridged(conn: Conn) -> io::Result<Bridged> {
    let (owner, ours) = pair()?;
    let conn = Arc::new(conn);
    let ours = Arc::new(ours);
    let cut = Cut::default();
    let up = (Arc::clone(&conn), Arc::clone(&ours));
    std::thread::Builder::new()
        .name("hex-quic-up".to_owned())
        .spawn(move || upward(&up.0, &up.1))?;
    let recorded = cut.clone();
    std::thread::Builder::new()
        .name("hex-quic-down".to_owned())
        .spawn(move || downward(&conn, &ours, &recorded))?;
    Ok(Bridged::new(owner, cut))
}

/// The `n` bytes a direction copied through `buf`, wiped once copied where `conn` carries the node's wire, whose
/// messages may carry a lease's value or a sign-in's tokens (docs/design/credential-grants.md C7, C8; the review's
/// E-6).
fn wiped(conn: &Conn, buf: &mut [u8], n: usize) {
    if conn.wipes()
        && let Some(copied) = buf.get_mut(..n)
    {
        hex_api::lease::wipe_in_place(copied);
    }
}

/// What the owner writes, to the peer; the owner's close ends this side's half. The connection's end ends it too:
/// [`downward`] then ends its reading.
fn upward(conn: &Conn, mut from: &Local) {
    let mut buf = vec![0u8; DATA_BOUND];
    loop {
        #[cfg(target_os = "macos")]
        if hex_client::wait_readable(std::os::fd::AsFd::as_fd(from)).is_err() {
            conn.reset(Why::Gone);
            return;
        }
        match from.read(&mut buf) {
            Ok(0) => {
                let _ = conn.end(None);
                return;
            }
            Ok(n) => {
                let wrote = conn.write_all(buf.get(..n).unwrap_or_default(), None);
                wiped(conn, &mut buf, n);
                if wrote.is_err() {
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
fn downward(conn: &Conn, mut to: &Local, cut: &Cut) {
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
                let wrote = to.write_all(buf.get(..n).unwrap_or_default());
                wiped(conn, &mut buf, n);
                if wrote.is_err() {
                    conn.reset(Why::Gone);
                    end(to);
                    return;
                }
            }
            Err(e) => {
                cut.record(&e);
                end(to);
                return;
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
