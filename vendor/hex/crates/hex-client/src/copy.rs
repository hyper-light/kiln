//! The one copy left of a program's stdio (native plan step 8): where a program's stream is a connection of its
//! own (a guest's stdio connection, docs/design/shards-guest.md §3) and its caller gave a pipe of its own for it,
//! the caller's client copies between the two, one thread and one buffer per stream. Never the node: through its
//! socket, the node gives that connection's end to the caller's client with the exec's answer.

use std::io::{self, PipeReader, Read, Write};
use std::sync::{Arc, OnceLock};

use hex_api::Refusal;

/// What cut a stream short, once something has: the error its reading ended with before its end, as a reset or its
/// link's end makes it (docs/design/hex-quic.md Q11). Shared by whoever reads the stream (a carried stream's copy, a
/// bridge) and the program it is of, whose end then says so.
#[derive(Clone, Debug, Default)]
pub struct Cut(Arc<OnceLock<(io::ErrorKind, String)>>);

impl Cut {
    /// Records `e` as what cut the stream, the first such alone.
    pub fn record(&self, e: &io::Error) {
        let _ = self.0.set((e.kind(), e.to_string()));
    }

    /// What cut the stream, once something has.
    pub fn error(&self) -> Option<io::Error> {
        self.0
            .get()
            .map(|(kind, why)| io::Error::new(*kind, why.clone()))
    }
}

/// A program's output streams whose cut its end tells, each by its name.
pub type Cuts = Vec<(&'static str, Cut)>;

/// A stream read for a copy, its read's error recorded as its cut.
struct Watched<R> {
    from: R,
    cut: Cut,
}

impl<R: Read> Read for Watched<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let read = self.from.read(bytes);
        if let Err(e) = &read
            && e.kind() != io::ErrorKind::Interrupted
        {
            self.cut.record(e);
        }
        read
    }
}

/// Copies `from` into `to` on a thread of its own until either ends, then hands `to` to `ended`: a program's
/// output, from its stream's connection into the caller's pipe. Its end is the stream's: the caller sees stdout
/// end, as with a pipe, or the program sees its write fail once the caller's pipe has no reader. A read that fails
/// cut the stream short: `cut` records it before the caller's pipe ends, and the program's end says so.
pub(crate) fn copy<R, W>(
    from: R,
    mut to: W,
    cut: Cut,
    ended: impl FnOnce(W) + Send + 'static,
) -> Result<(), Refusal>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    std::thread::Builder::new()
        .name("hex-client-stdio".to_owned())
        .spawn(move || {
            let mut from = Watched { from, cut };
            let _ = io::copy(&mut from, &mut to);
            ended(to);
        })
        .map(drop)
        .map_err(|e| Refusal::os("starting a stdio copier", e))
}

/// The connection a stdin copier feeds: written to, waited on for its end, and its sending half ended.
pub(crate) trait Feed: Write + Send + 'static {
    /// Its descriptor, waited on for its end.
    #[cfg(unix)]
    fn waited(&self) -> std::os::fd::BorrowedFd<'_>;
    /// Ends its sending half, so the program sees its stdin end even while another holds the connection.
    fn end_sending(&self);
}

/// A stream's end the caller's client holds: a guest's connection, which its node gave with the exec's answer.
impl Feed for hex_api::OwnedStream {
    #[cfg(unix)]
    fn waited(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(self)
    }

    fn end_sending(&self) {
        #[cfg(unix)]
        let _ = rustix::net::shutdown(self, rustix::net::Shutdown::Write);
        #[cfg(windows)]
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

/// Moves the same unique facade into the existing generic copier. UniqueArc itself has no
/// Read/Write implementation; this adapter adds neither allocation nor native ownership.
pub(crate) struct DuplexIo(pub(crate) crate::guest::DuplexOwner);

impl Read for DuplexIo {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        Waiting(&mut *self.0).read(bytes)
    }
}

/// A guest connection read as every read of one must wait: on macOS in poll(2) first ([`wait_readable`]), then the
/// read; elsewhere the read itself.
pub(crate) struct Waiting<'a>(pub(crate) &'a mut dyn crate::guest::Duplex);

impl Read for Waiting<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        #[cfg(target_os = "macos")]
        wait_readable(self.0.handle())?;
        self.0.read(bytes)
    }
}

/// Waits in poll(2) until `conn` has bytes or its end to read. On macOS a read waiting in recv(2) on a Unix stream
/// socket can sleep through its peer's half-close: XNU's `uipc_rcvd` lets go of the reader's socket to take its
/// peer's lock first, and a shutdown landing then wakes no one (Shards M236: 360 of 50,000 readers asleep), where
/// poll(2) sees it (0 of 50,000). So no read of one waits in the read (Shards D175).
#[cfg(target_os = "macos")]
pub fn wait_readable(conn: crate::guest::Handle<'_>) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags};
    loop {
        let mut ready = [PollFd::from_borrowed_fd(conn, PollFlags::IN)];
        match rustix::event::poll(&mut ready, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

impl Write for DuplexIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// A guest's stdio connection the client dialed itself, in process.
impl Feed for DuplexIo {
    #[cfg(unix)]
    fn waited(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.handle()
    }

    fn end_sending(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Write);
    }
}

/// Copies the caller's own stdin pipe `from` into its stream's connection `to` on a thread of its own, until the
/// pipe ends (then the connection's sending half ends, so the program sees its stdin end) or the connection does
/// (its program let its stdin go, as when it ended): either way the copier lets the caller's pipe go, so it has no
/// reader once the program has ended, as a local program's own stdin pipe has none. Each read takes at most the
/// connection's send buffer (`SO_SNDBUF`, read from the connection itself), what one write can queue there.
#[cfg(unix)]
pub(crate) fn copy_in(from: PipeReader, to: impl Feed) -> Result<(), Refusal> {
    let unit = rustix::net::sockopt::socket_send_buffer_size(to.waited())
        .map_err(|e| Refusal::os("reading a stdin connection's send buffer", e))?;
    if unit == 0 {
        return Err(Refusal::Broken(
            "a stdin connection's send buffer reads 0 bytes".to_owned(),
        ));
    }
    std::thread::Builder::new()
        .name("hex-client-stdio".to_owned())
        .spawn(move || feed(from, to, unit))
        .map(drop)
        .map_err(|e| Refusal::os("starting a stdio copier", e))
}

/// Where neither end can be waited on beside the other (Windows' pipes and sockets), the copy ends with the
/// caller's pipe or a write the connection refuses; ending it with the connection is the Windows agent's, by its
/// port.
#[cfg(not(unix))]
pub(crate) fn copy_in(from: PipeReader, to: impl Feed) -> Result<(), Refusal> {
    // The caller's own pipe is read here, no stream of a program: its cut is no one's.
    copy(from, to, Cut::default(), |to| to.end_sending())
}

/// The stdin copier's loop: waits on the caller's pipe and the connection together, copying what the pipe gives,
/// until either ends.
#[cfg(unix)]
fn feed(mut from: PipeReader, mut to: impl Feed, unit: usize) {
    use rustix::event::{PollFd, PollFlags};
    let mut buf = vec![0u8; unit];
    loop {
        let (pipe, conn) = {
            let waited = to.waited();
            let mut ready = [
                PollFd::new(&from, PollFlags::IN),
                PollFd::new(&waited, PollFlags::IN),
            ];
            match rustix::event::poll(&mut ready, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return,
            }
            let [pipe, conn] = &ready;
            (pipe.revents(), conn.revents())
        };
        // Readable, hung up or failed, the connection has ended: nothing more reaches its program.
        if !conn.is_empty() {
            return;
        }
        if pipe.is_empty() {
            continue;
        }
        match from.read(&mut buf) {
            Ok(0) => {
                to.end_sending();
                return;
            }
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
