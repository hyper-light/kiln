//! The one copy left of a program's stdio (native plan step 8): where a program's stream is a connection of its
//! own (a guest's stdio connection, docs/design/shards-guest.md §3) and its caller gave a pipe of its own for it,
//! the caller's client copies between the two, one thread and one buffer per stream. Never the node: through its
//! socket, the node gives that connection's end to the caller's client with the exec's answer.

use std::io::{self, PipeReader, Read, Write};

use hex_api::Refusal;

/// Copies `from` into `to` on a thread of its own until either ends, then hands `to` to `ended`: a program's
/// output, from its stream's connection into the caller's pipe. Its end is the stream's: the caller sees stdout
/// end, as with a pipe, or the program sees its write fail once the caller's pipe has no reader; there is no one
/// else to tell.
pub(crate) fn copy<R, W>(
    mut from: R,
    mut to: W,
    ended: impl FnOnce(W) + Send + 'static,
) -> Result<(), Refusal>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    std::thread::Builder::new()
        .name("hex-client-stdio".to_owned())
        .spawn(move || {
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
        self.0.read(bytes)
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
    copy(from, to, |to| to.end_sending())
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
