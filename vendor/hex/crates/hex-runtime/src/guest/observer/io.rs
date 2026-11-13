//! One caller drains the actual guest Program's two asked sockets. Both EOFs
//! are required; no copier, reader worker, periodic probe or console scan exists.

use hex_api::Refusal;

use super::{exhausted, invalid};
use crate::guest::managed::Budget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum End {
    Open,
    Eof,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Captured {
    bytes: Vec<u8>,
    used: usize,
    limit: usize,
    pub end: End,
}

impl Captured {
    pub fn new(limit: usize) -> Result<Self, Refusal> {
        let room = limit.checked_add(1).ok_or_else(|| exhausted(limit))?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(room).map_err(|_| exhausted(room))?;
        bytes.resize(room, 0);
        Ok(Self {
            bytes,
            used: 0,
            limit,
            end: End::Open,
        })
    }

    pub fn bytes(&self) -> Result<&[u8], Refusal> {
        self.bytes
            .get(..self.used)
            .ok_or_else(|| exhausted(self.limit))
    }

    pub fn within_bound(&self) -> bool {
        self.used <= self.limit
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn encoded_room(&self) -> Result<usize, Refusal> {
        self.limit
            .checked_add(1)
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or_else(|| exhausted(self.limit))
    }

    pub fn render(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        write!(
            out,
            "{} {} ",
            if self.end == End::Eof { "eof" } else { "open" },
            self.limit
        )?;
        if self.used == 0 {
            return write!(out, "-");
        }
        for byte in self.bytes().map_err(|_| std::fmt::Error)? {
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }

    pub fn parse(state: &str, limit: &str, frame: &str) -> Result<Self, Refusal> {
        let limit = usize::try_from(super::number(limit)?).map_err(|_| exhausted(usize::MAX))?;
        if limit > hex_core::record::MAX_RECORD_BYTES {
            return Err(exhausted(limit));
        }
        let mut captured = Self::new(limit)?;
        captured.end = match state {
            "open" => End::Open,
            "eof" => End::Eof,
            _ => return Err(invalid("retained observer stream has an unknown EOF state")),
        };
        if frame == "-" {
            return Ok(captured);
        }
        let (pairs, tail) = frame.as_bytes().as_chunks::<2>();
        if pairs.is_empty()
            || pairs.len() > captured.bytes.len()
            || !tail.is_empty()
            || !frame.bytes().all(super::lower_hex)
        {
            return Err(invalid(
                "retained observer stream has invalid or excessive bytes",
            ));
        }
        for (output, pair) in captured.bytes.iter_mut().zip(pairs) {
            *output = super::hex_byte(pair)?;
        }
        captured.used = pairs.len();
        Ok(captured)
    }

    /// Keep the first bound+1 bytes on overrun, including its non-vacuous
    /// excess-byte witness. The caller persists this partial failed attempt.
    fn receive(&mut self, source: &mut impl std::io::Read) -> Result<(), Refusal> {
        if self.end == End::Eof {
            return Ok(());
        }
        let output = self
            .bytes
            .get_mut(self.used..)
            .filter(|bytes| !bytes.is_empty())
            .ok_or_else(|| exhausted(self.limit))?;
        match source.read(output) {
            Ok(0) => self.end = End::Eof,
            Ok(count) => {
                self.used = self
                    .used
                    .checked_add(count)
                    .filter(|used| *used <= self.bytes.len())
                    .ok_or_else(|| exhausted(self.limit))?;
                if self.used > self.limit {
                    return Err(exhausted(self.limit));
                }
            }
            Err(why)
                if matches!(
                    why.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(why) => return Err(Refusal::os("reading actual guest observer output", why)),
        }
        Ok(())
    }
}

use socket2::Socket;

fn socket(end: hex_api::End) -> Result<Socket, Refusal> {
    #[cfg(unix)]
    {
        let socket = Socket::from(end);
        socket
            .set_nonblocking(true)
            .map_err(|why| Refusal::os("making guest observer output nonblocking", why))?;
        Ok(socket)
    }
    #[cfg(windows)]
    {
        let hex_api::End::Socket(socket) = end else {
            return Err(invalid(
                "a guest observer output must be its actual relay socket",
            ));
        };
        // Safe ownership transfer of the SAME existing guest relay socket; no
        // socket open/duplication, receive timeout, pending IO or another owner.
        let socket = Socket::from(socket);
        socket
            .set_nonblocking(true)
            .map_err(|why| Refusal::os("making guest observer output nonblocking", why))?;
        Ok(socket)
    }
}

pub(super) struct Streams {
    stdout: Option<Socket>,
    stderr: Option<Socket>,
}

impl Streams {
    pub fn new(mut given: hex_api::Given) -> Result<Self, Refusal> {
        if given.stdin.is_some() {
            return Err(invalid("guest observer received unasked input authority"));
        }
        let stdout = given
            .stdout
            .take()
            .ok_or_else(|| invalid("guest observer stdout is missing"))?;
        let stderr = given
            .stderr
            .take()
            .ok_or_else(|| invalid("guest observer stderr is missing"))?;
        Ok(Self {
            stdout: Some(socket(stdout)?),
            stderr: Some(socket(stderr)?),
        })
    }

    pub fn drain(
        &mut self,
        stdout: &mut Captured,
        stderr: &mut Captured,
        budget: Budget,
    ) -> Result<(), Refusal> {
        loop {
            budget.remaining()?;
            if stdout.end == End::Eof && stderr.end == End::Eof {
                return Ok(());
            }
            let [out, err] = wait(self.stdout.as_ref(), self.stderr.as_ref(), budget)?;
            if out && let Some(source) = &mut self.stdout {
                stdout.receive(source)?;
            }
            if err && let Some(source) = &mut self.stderr {
                stderr.receive(source)?;
            }
            // A completed EOF is removed from the NEXT native wait. Repeatedly
            // waiting on its persistent HUP would spin beside a quiet sibling.
            if stdout.end == End::Eof {
                self.stdout.take();
            }
            if stderr.end == End::Eof {
                self.stderr.take();
            }
        }
    }
}

#[cfg(unix)]
fn wait(
    stdout: Option<&Socket>,
    stderr: Option<&Socket>,
    budget: Budget,
) -> Result<[bool; 2], Refusal> {
    use rustix::event::{PollFd, PollFlags};
    loop {
        let left = budget.remaining()?;
        // macOS poll's signed millisecond timeout is the smallest supported
        // Unix ABI ceiling; only that ABI can split a larger finite wait.
        let ceiling =
            std::time::Duration::from_millis(u64::try_from(i32::MAX).map_err(|_| exhausted(2))?);
        let timeout = rustix::event::Timespec::try_from(left.min(ceiling)).map_err(|_| {
            invalid("guest observer native wait cannot represent its remaining budget")
        })?;
        let result = match (stdout, stderr) {
            (Some(stdout), Some(stderr)) => {
                let mut sources = [
                    PollFd::new(stdout, PollFlags::IN),
                    PollFd::new(stderr, PollFlags::IN),
                ];
                rustix::event::poll(&mut sources, Some(&timeout))
            }
            (Some(source), None) | (None, Some(source)) => {
                let mut sources = [PollFd::new(source, PollFlags::IN)];
                rustix::event::poll(&mut sources, Some(&timeout))
            }
            (None, None) => return Err(invalid("guest observer has no open output to wait for")),
        };
        match result {
            // Nonblocking reads of BOTH active streams handle readiness/EOF or
            // typed descriptor errors without interpreting unknown event bits.
            Ok(count) if count > 0 => return Ok([stdout.is_some(), stderr.is_some()]),
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(why) => return Err(Refusal::os("waiting for guest observer output", why)),
        }
    }
}

#[cfg(windows)]
fn wait(
    stdout: Option<&Socket>,
    stderr: Option<&Socket>,
    budget: Budget,
) -> Result<[bool; 2], Refusal> {
    use std::os::windows::io::AsSocket as _;
    crate::windows::sys::readable_pair(
        [stdout.map(Socket::as_socket), stderr.map(Socket::as_socket)],
        budget.remaining()?,
    )
    .map_err(|why| Refusal::os("waiting for guest observer output", why))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T7.4.1: an overflow retains the exact prefix plus its excess witness;
    /// a zero-length read alone changes this stream to positive EOF.
    #[test]
    fn bounded_observer_capture_keeps_overrun_and_requires_actual_eof() {
        let mut capture = Captured::new(2).unwrap();
        let mut source = std::io::Cursor::new(b"abc");
        assert!(capture.receive(&mut source).is_err());
        assert_eq!(capture.bytes().unwrap(), b"abc");
        assert_eq!(capture.end, End::Open);
        let mut capture = Captured::new(2).unwrap();
        let mut source = std::io::Cursor::new(b"ab");
        capture.receive(&mut source).unwrap();
        assert_eq!(capture.end, End::Open);
        capture.receive(&mut source).unwrap();
        assert_eq!(capture.end, End::Eof);
        assert_eq!(capture.bytes().unwrap(), b"ab");
    }

    /// AC-T7.4.1: actual asked-socket data and independent EOFs drain without a
    /// copier/extra reader owner. This is a local pipe control, not GPU proof.
    #[cfg(unix)]
    #[test]
    fn two_actual_guest_style_sockets_require_both_stream_ends() {
        use std::io::Write as _;
        use std::os::fd::OwnedFd;
        let (out, mut out_writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (err, mut err_writer) = std::os::unix::net::UnixStream::pair().unwrap();
        out_writer.write_all(b"out").unwrap();
        err_writer.write_all(b"err").unwrap();
        drop((out_writer, err_writer));
        let mut streams = Streams::new(hex_api::Given {
            stdin: None,
            stdout: Some(OwnedFd::from(out)),
            stderr: Some(OwnedFd::from(err)),
        })
        .unwrap();
        let mut stdout = Captured::new(3).unwrap();
        let mut stderr = Captured::new(3).unwrap();
        // The drain spends its observer attempt's budget, as the node gives one (`first_dial_bound`):
        // twice the guest's recorded boot to ready.
        let attempt = crate::guest::recorded_boot()
            .unwrap()
            .saturating_mul(hex_core::patience::TIMES_SLOWEST);
        streams
            .drain(&mut stdout, &mut stderr, Budget::new(attempt).unwrap())
            .unwrap();
        assert_eq!(stdout.bytes().unwrap(), b"out");
        assert_eq!(stderr.bytes().unwrap(), b"err");
        assert_eq!((stdout.end, stderr.end), (End::Eof, End::Eof));
        assert!(streams.stdout.is_none() && streams.stderr.is_none());
    }
}
