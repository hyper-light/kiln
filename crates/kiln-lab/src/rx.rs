//! When what the lab reads arrived at its socket, by the kernel's clock (goal 10, plan 01a1a87b2dae step 5): Linux
//! stamps each segment as it's queued for the socket (`SO_TIMESTAMPNS`, socket(7)), and a read says the stamp of the
//! last it took. A loopback segment is queued by the sender's own write, so the stamp is when the server wrote it, and
//! the rest of a read's time is the lab's own: its thread's wake onto its CPU and its read. The lab's one other
//! OS interface beside `crate::rate::os`, and with it the lab's only `unsafe`.
//!
//! Linux alone: macOS and Windows stamp no TCP segments, and there a read says no arrival.
#![allow(unsafe_code)]

use std::io;
use std::net::TcpStream;

/// Asks the kernel to stamp the segments that arrive on `s` with their arrival: whether it will.
#[cfg(target_os = "linux")]
pub fn stamp_arrivals(s: &TcpStream) -> bool {
    use std::os::fd::AsRawFd as _;
    let on: libc::c_int = 1;
    let Ok(len) = libc::socklen_t::try_from(size_of::<libc::c_int>()) else { return false };
    // SAFETY: `s`'s descriptor is open while `s` is borrowed; the option's value is the local `on`, `len` bytes, which
    // the call only reads.
    let done = unsafe { libc::setsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_TIMESTAMPNS, (&raw const on).cast(), len) };
    done == 0
}

/// Asks the kernel to stamp the segments that arrive on `s`: none does but Linux's.
#[cfg(not(target_os = "linux"))]
pub fn stamp_arrivals(_s: &TcpStream) -> bool {
    false
}

/// The room for a read's control messages: one `SCM_TIMESTAMPNS`, a `timespec` (`CMSG_SPACE`), in words, so the room
/// is aligned as a `cmsghdr` must be.
#[cfg(target_os = "linux")]
const CONTROL_WORDS: usize = {
    // SAFETY: `CMSG_SPACE` only adds and aligns lengths: no pointer is read.
    let bytes = unsafe { libc::CMSG_SPACE(size_of::<libc::timespec>() as libc::c_uint) } as usize;
    bytes.div_ceil(size_of::<u64>())
};

/// Reads into `buf` as [`std::io::Read::read`] does: how many bytes came, and when the last of them arrived at `s`, in
/// microseconds since the epoch (the kernel's `CLOCK_REALTIME`, the clock the lab's stand-in stamps its events by),
/// where the socket stamps arrivals ([`stamp_arrivals`]). An interrupted read is read again; one that timed out is
/// `WouldBlock`, as `read`'s.
#[cfg(target_os = "linux")]
pub fn read(s: &mut TcpStream, buf: &mut [u8]) -> io::Result<(usize, Option<u64>)> {
    use std::os::fd::AsRawFd as _;
    let mut control = [0u64; CONTROL_WORDS];
    loop {
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        // SAFETY: a `msghdr` of zeros is valid (no name, no data, no control); its fields are set below.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        // A `size_t` in glibc's `msghdr`, a `socklen_t` in musl's: the room is a few words either way.
        msg.msg_controllen = size_of_val(&control) as _;
        // SAFETY: `s`'s descriptor is open while `s` is borrowed; `msg` names `buf` (its length) and `control` (its
        // length), both live and writable for the call, which writes at most those lengths of each.
        let n = unsafe { libc::recvmsg(s.as_raw_fd(), &raw mut msg, 0) };
        let Ok(n) = usize::try_from(n) else {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        };
        return Ok((n, arrival(&msg)));
    }
}

/// The arrival a read's control messages say (`SCM_TIMESTAMPNS`), in microseconds since the epoch.
#[cfg(target_os = "linux")]
fn arrival(msg: &libc::msghdr) -> Option<u64> {
    // SAFETY: `msg` is the `msghdr` `recvmsg` filled: its control room and the length it said were written there, and
    // `CMSG_FIRSTHDR` and `CMSG_NXTHDR` stay within that length, each header read whole before its data. (`cmsg_len` is a
    // `size_t` in glibc's headers and a `socklen_t` in musl's: the length it's held to is cast to its type.)
    unsafe {
        let mut at = libc::CMSG_FIRSTHDR(msg);
        while let Some(header) = at.as_ref() {
            if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_TIMESTAMPNS && header.cmsg_len >= libc::CMSG_LEN(size_of::<libc::timespec>() as libc::c_uint) as _ {
                let ts = std::ptr::read_unaligned(libc::CMSG_DATA(at).cast::<libc::timespec>());
                let secs = u64::try_from(ts.tv_sec).ok()?;
                let nanos = u64::try_from(ts.tv_nsec).ok()?;
                return secs.checked_mul(1_000_000)?.checked_add(nanos / 1000);
            }
            at = libc::CMSG_NXTHDR(msg, at);
        }
    }
    None
}

/// Reads into `buf` as [`std::io::Read::read`] does: no arrival is said off Linux.
#[cfg(not(target_os = "linux"))]
pub fn read(s: &mut TcpStream, buf: &mut [u8]) -> io::Result<(usize, Option<u64>)> {
    use std::io::Read as _;
    s.read(buf).map(|n| (n, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Goal 10, plan 01a1a87b2dae step 5: on Linux what arrives on a stamped socket says when it arrived, by the clock
    /// the lab's stand-in stamps by, at or after the write that sent it and at or before the read that took it; the
    /// bytes are what was sent. Elsewhere it reads as `read` does and says no arrival.
    #[test]
    fn g10_s5_a_read_says_when_its_bytes_arrived() -> anyhow::Result<()> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let mut client = TcpStream::connect(listener.local_addr()?)?;
        let (mut server, _) = listener.accept()?;
        let stamped = stamp_arrivals(&client);
        anyhow::ensure!(stamped == cfg!(target_os = "linux"), "stamped: {stamped}");
        let before = crate::standin::now_us();
        server.write_all(b"the retry loop")?;
        let mut buf = [0u8; 64];
        let (n, arrived) = read(&mut client, &mut buf)?;
        let after = crate::standin::now_us();
        anyhow::ensure!(buf.get(..n) == Some(&b"the retry loop"[..]), "{:?}", buf.get(..n));
        if cfg!(target_os = "linux") {
            let at = arrived.ok_or_else(|| anyhow::anyhow!("no arrival said"))?;
            // The kernel's clock and the lab's are one, read to the microsecond: a microsecond either side for the rounding.
            anyhow::ensure!(before.saturating_sub(1) <= at && at <= after.saturating_add(1), "arrived at {at}, written at {before}, read by {after}");
        } else {
            anyhow::ensure!(arrived.is_none(), "{arrived:?}");
        }
        // A socket not stamped says none.
        let mut plain = TcpStream::connect(listener.local_addr()?)?;
        let (mut other, _) = listener.accept()?;
        other.write_all(b"x")?;
        let (n, arrived) = read(&mut plain, &mut buf)?;
        anyhow::ensure!(n == 1 && arrived.is_none(), "{n}, {arrived:?}");
        Ok(())
    }
}
