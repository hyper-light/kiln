//! When what the lab reads arrived at its socket, by the kernel's clock (goal 10, plan 01a1a87b2dae step 5): Linux
//! stamps each segment as it's queued for the socket (`SO_TIMESTAMPNS`, socket(7)), and a read says the stamp of the
//! last it took. A loopback segment is queued by the sender's own write, so the stamp is when the server wrote it, and
//! the rest of a read's time is the lab's own: its thread's wake onto its CPU and its read. The lab's one other
//! OS interface beside `crate::rate::os`, and with it the lab's only `unsafe`.
//!
//! Linux alone: macOS and Windows stamp no TCP segments, and there a read says no arrival.
//!
//! Every read of the lab's clients comes here, so here is where an interrupted one is read again ([`read`]), within the
//! time its caller gave it, and counted ([`interrupted`]).
#![allow(unsafe_code)]

use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The lab's reads an interruption cut short (goal 1, plan 01a1a59c2a68 step 3; found in goal 9's lab g9p21, round 3
/// calm): a signal, or the lab's processes stopped or frozen and then let go on. Linux fails a socket read that has a
/// timeout with EINTR then, even with no handler (signal(7)): fedora's installed runner paces every unit
/// `systemd-run --user` runs, the lab's among them, with `cgroup.freeze` (headroom's `escapes`), and a lab binary that
/// took EINTR for an error failed its round, or hung it. Each is read again ([`read`]) and counted here, and a round
/// says how many its window had: a window whose clients were frozen measured the freezes with the server.
static INTERRUPTED: AtomicU64 = AtomicU64::new(0);

/// How many of the lab's reads were interrupted so far ([`INTERRUPTED`]).
pub fn interrupted() -> u64 {
    INTERRUPTED.load(Ordering::Relaxed)
}

/// What an interrupted read changed while it's read again: the socket's own timeout, which what's left of the time to
/// `until` stands in for, so an interruption never makes a wait longer than its caller allowed. (The kernel starts a
/// read's timeout over at each call, so a read simply called again after each interruption, as this file's did, waited
/// on without end while something kept interrupting it.)
struct Again<'a> {
    s: &'a TcpStream,
    until: Instant,
    own: Option<Option<Duration>>,
}

impl Again<'_> {
    /// Ready for one more try after an interruption: what's left of the time to `until` as the socket's timeout, or
    /// `TimedOut` once none is left.
    fn arm(&mut self) -> io::Result<()> {
        INTERRUPTED.fetch_add(1, Ordering::Relaxed);
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "interrupted with no time left to read"));
        }
        if self.own.is_none() {
            self.own = Some(self.s.read_timeout()?);
        }
        self.s.set_read_timeout(Some(left))
    }

    /// What the read came to, with the socket's own timeout back where a try changed it.
    fn done<T>(self, got: io::Result<T>) -> io::Result<T> {
        match self.own {
            Some(own) => self.s.set_read_timeout(own).and(got),
            None => got,
        }
    }
}

/// Reads into `buf` as [`std::io::Read::read`] does: how many bytes came, and when the last of them arrived at `s`, in
/// microseconds since the epoch (the kernel's `CLOCK_REALTIME`, the clock the lab's stand-in stamps its events by), where
/// the socket stamps arrivals ([`stamp_arrivals`]). One that timed out is `WouldBlock`, as `read`'s. An interrupted read
/// ([`INTERRUPTED`]) is read again for what's left of the time to `until`, and is `TimedOut` once that's gone.
pub fn read(s: &TcpStream, buf: &mut [u8], until: Instant) -> io::Result<(usize, Option<u64>)> {
    let mut again = Again { s, until, own: None };
    let got = loop {
        match read_once(s, buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                if let Err(e) = again.arm() {
                    break Err(e);
                }
            }
            got => break got,
        }
    };
    again.done(got)
}

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

/// One read into `buf`, as [`read`] says, an interrupted one `Interrupted`.
#[cfg(target_os = "linux")]
fn read_once(s: &TcpStream, buf: &mut [u8]) -> io::Result<(usize, Option<u64>)> {
    use std::os::fd::AsRawFd as _;
    let mut control = [0u64; CONTROL_WORDS];
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
    let Ok(n) = usize::try_from(n) else { return Err(io::Error::last_os_error()) };
    Ok((n, arrival(&msg)))
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

/// One read into `buf`, as [`read`] says, an interrupted one `Interrupted`: no arrival is said off Linux.
#[cfg(not(target_os = "linux"))]
fn read_once(s: &TcpStream, buf: &mut [u8]) -> io::Result<(usize, Option<u64>)> {
    use std::io::Read as _;
    let mut r: &TcpStream = s;
    r.read(buf).map(|n| (n, None))
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
        let client = TcpStream::connect(listener.local_addr()?)?;
        let (mut server, _) = listener.accept()?;
        let stamped = stamp_arrivals(&client);
        anyhow::ensure!(stamped == cfg!(target_os = "linux"), "stamped: {stamped}");
        let before = crate::standin::now_us();
        server.write_all(b"the retry loop")?;
        let mut buf = [0u8; 64];
        let (n, arrived) = read(&client, &mut buf, Instant::now() + Duration::from_secs(5))?;
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
        let plain = TcpStream::connect(listener.local_addr()?)?;
        let (mut other, _) = listener.accept()?;
        other.write_all(b"x")?;
        let (n, arrived) = read(&plain, &mut buf, Instant::now() + Duration::from_secs(5))?;
        anyhow::ensure!(n == 1 && arrived.is_none(), "{n}, {arrived:?}");
        Ok(())
    }

    /// A handler that does nothing: a thread it's sent to has its blocked read interrupted (EINTR), as the lab's are when
    /// its processes are stopped or frozen and let go on.
    #[cfg(target_os = "linux")]
    extern "C" fn nothing(_: libc::c_int) {}

    /// SIGUSR2 caught by [`nothing`], without SA_RESTART (a read with a timeout isn't restarted with it either).
    #[cfg(target_os = "linux")]
    fn catch_usr2() -> anyhow::Result<()> {
        let handler: extern "C" fn(libc::c_int) = nothing;
        // SAFETY: a zeroed `sigaction` is valid: no flags, and its mask is emptied by `sigemptyset` before it's used; the
        // handler does nothing, so it's safe wherever a signal lands.
        let done = unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler as libc::sighandler_t;
            libc::sigemptyset(&raw mut sa.sa_mask);
            libc::sigaction(libc::SIGUSR2, &raw const sa, std::ptr::null_mut())
        };
        anyhow::ensure!(done == 0, "sigaction: {}", io::Error::last_os_error());
        Ok(())
    }

    /// Interrupts `thread`'s blocked call with SIGUSR2 ([`catch_usr2`]).
    #[cfg(target_os = "linux")]
    fn interrupt(thread: libc::pthread_t) {
        // SAFETY: `thread` is a thread of this test's whose handle isn't joined yet, so its id is still its own; SIGUSR2's
        // handler does nothing.
        unsafe { libc::pthread_kill(thread, libc::SIGUSR2) };
    }

    /// Goal 1, plan 01a1a59c2a68 step 3 (goal 9's lab g9p21: its round 3 calm hung, and three rounds after it failed, on
    /// "Interrupted system call"): a read that has a timeout and is interrupted (Linux: a signal, or the lab stopped or
    /// frozen and let go on, as fedora's runner's headroom freezes the lab's unit) is read again and counted, and its
    /// bytes come; and interruptions never make a read wait longer than it was given (the kernel starts a read's timeout
    /// over at each call: one read again with its whole timeout each time waited without end while they kept coming),
    /// the socket's own timeout put back after either.
    #[cfg(target_os = "linux")]
    #[test]
    fn g1_s3_an_interrupted_read_is_read_again_within_its_time() -> anyhow::Result<()> {
        use std::os::unix::thread::JoinHandleExt as _;
        catch_usr2()?;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let own = Duration::from_secs(30);

        // Interrupted, then answered: the answer is read.
        let client = TcpStream::connect(listener.local_addr()?)?;
        let (mut server, _) = listener.accept()?;
        client.set_read_timeout(Some(own))?;
        let before = interrupted();
        let reading = client.try_clone()?;
        let reader = std::thread::spawn(move || -> io::Result<Vec<u8>> {
            let mut buf = [0u8; 64];
            let (n, _) = read(&reading, &mut buf, Instant::now() + own)?;
            Ok(buf.get(..n).unwrap_or_default().to_vec())
        });
        let gave_up = Instant::now() + own;
        while interrupted() == before {
            anyhow::ensure!(Instant::now() < gave_up && !reader.is_finished(), "the read was never interrupted");
            interrupt(reader.as_pthread_t());
            std::thread::sleep(Duration::from_millis(2));
        }
        server.write_all(b"after the freeze")?;
        let got = reader.join().map_err(|_| anyhow::anyhow!("the reader panicked"))??;
        anyhow::ensure!(got == b"after the freeze", "{got:?}");
        anyhow::ensure!(client.read_timeout()? == Some(own), "the socket's own timeout is back: {:?}", client.read_timeout()?);

        // Interrupted again and again, never answered: it ends when its time does, not its socket's.
        let client = TcpStream::connect(listener.local_addr()?)?;
        let (_server, _) = listener.accept()?;
        client.set_read_timeout(Some(own))?;
        let given = Duration::from_millis(300);
        let reading = client.try_clone()?;
        let started = Instant::now();
        let reader = std::thread::spawn(move || -> io::Result<usize> {
            let mut buf = [0u8; 64];
            read(&reading, &mut buf, Instant::now() + given).map(|(n, _)| n)
        });
        while !reader.is_finished() && started.elapsed() < own {
            interrupt(reader.as_pthread_t());
            std::thread::sleep(Duration::from_millis(2));
        }
        let took = started.elapsed();
        let got = reader.join().map_err(|_| anyhow::anyhow!("the reader panicked"))?;
        anyhow::ensure!(matches!(&got, Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)), "{got:?}");
        anyhow::ensure!(took < own / 2, "interrupted reads waited {took:?}, given {given:?}");
        anyhow::ensure!(client.read_timeout()? == Some(own), "the socket's own timeout is back: {:?}", client.read_timeout()?);
        Ok(())
    }
}
