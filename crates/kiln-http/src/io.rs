//! A hyper-rt TCP stream as `futures-io` (`AsyncRead + AsyncWrite`), what every connection here
//! speaks over: the plain socket, or a TLS stream over one.
//!
//! hyper-rt sockets are readiness-based: `try_read` / `try_write` never wait, and `readable()` /
//! `writable()` are one-shot waits kept between polls (dropping one withdraws its registration).
//! Such a wait accepts only the polling task's own waker; under a foreign waker it runs in a
//! [`Bridge`] helper task instead.

use std::future::Future;
use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_io::{AsyncRead, AsyncWrite};
use hyper_rt::RtError;
use hyper_rt::readiness::Ready;
use hyper_rt::tcp::{Shutdown, TcpStream};

use crate::rt::{Bridge, is_native, to_io};

/// One direction's outstanding wait.
enum Wait {
    /// Registered for the polling task itself (the fast path).
    Native(Ready),
    /// Registered by a helper task that wakes a foreign waker.
    Bridged(Bridge<Result<(), RtError>>),
}

/// Tries the non-blocking call; when it would block, arms readiness (natively or through a bridge)
/// and returns `Pending`; once readiness fires, tries again.
fn poll_io<T>(wait: &mut Option<Wait>, stream: &TcpStream, writable: bool, cx: &mut Context<'_>, mut attempt: impl FnMut(&TcpStream) -> Result<Option<T>, RtError>) -> Poll<io::Result<T>> {
    let native = is_native(cx);
    loop {
        match wait.take() {
            None => {}
            Some(Wait::Native(mut ready)) => {
                if native {
                    match Pin::new(&mut ready).poll(cx) {
                        Poll::Pending => {
                            *wait = Some(Wait::Native(ready));
                            return Poll::Pending;
                        }
                        Poll::Ready(outcome) => outcome.map_err(to_io)?,
                    }
                }
                // A native wait polled now under a foreign waker is withdrawn and armed again below.
            }
            Some(Wait::Bridged(bridge)) => match bridge.poll(cx) {
                Poll::Pending => {
                    *wait = Some(Wait::Bridged(bridge));
                    return Poll::Pending;
                }
                Poll::Ready(outcome) => outcome.map_err(to_io)?,
            },
        }
        match attempt(stream) {
            Ok(Some(value)) => return Poll::Ready(Ok(value)),
            Err(error) => return Poll::Ready(Err(to_io(error))),
            Ok(None) => {
                let ready = if writable { stream.writable() } else { stream.readable() };
                if native {
                    *wait = Some(Wait::Native(ready));
                } else {
                    *wait = Some(Wait::Bridged(Bridge::spawn(ready, cx.waker()).map_err(to_io)?));
                    return Poll::Pending;
                }
            }
        }
    }
}

/// One vectored write that never waits: hyper-rt offers it only as a future, which, polled once under a
/// waker that is no task's, writes or else is refused at its readiness wait without registering one
/// (`NotOnShardThread`): that refusal is "the send buffer is full".
fn try_write_vectored(stream: &TcpStream, bufs: &[IoSlice<'_>]) -> Result<Option<usize>, RtError> {
    let mut write = std::pin::pin!(stream.write_vectored(bufs));
    match write.as_mut().poll(&mut Context::from_waker(std::task::Waker::noop())) {
        Poll::Ready(Ok(n)) => Ok(Some(n)),
        Poll::Ready(Err(RtError::NotOnShardThread)) | Poll::Pending => Ok(None),
        Poll::Ready(Err(e)) => Err(e),
    }
}

/// A hyper-rt TCP stream as `futures-io`.
///
/// A read that took less than it had room for emptied the socket, so the next read waits for
/// readiness before it reads, rather than asking the kernel first and being told to wait: one system
/// call fewer for every request a connection waits for.
pub struct TcpIo {
    // The waits drop (withdraw their registrations) before the stream closes.
    read_wait: Option<Wait>,
    write_wait: Option<Wait>,
    stream: TcpStream,
    /// The last read emptied the socket.
    drained: bool,
}

impl TcpIo {
    pub fn new(stream: TcpStream) -> TcpIo {
        TcpIo { read_wait: None, write_wait: None, stream, drained: false }
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.stream.peer_addr().ok()
    }

    pub fn get_ref(&self) -> &TcpStream {
        &self.stream
    }

    /// The stream back, its waits withdrawn.
    pub fn into_inner(self) -> TcpStream {
        let TcpIo { read_wait, write_wait, stream, .. } = self;
        drop(read_wait);
        drop(write_wait);
        stream
    }
}

impl AsyncRead for TcpIo {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if std::mem::take(&mut this.drained) && this.read_wait.is_none() && is_native(cx) {
            let mut ready = this.stream.readable();
            match Pin::new(&mut ready).poll(cx) {
                Poll::Pending => {
                    this.read_wait = Some(Wait::Native(ready));
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(to_io(e))),
                Poll::Ready(Ok(())) => {}
            }
        }
        let want = buf.len();
        let read = poll_io(&mut this.read_wait, &this.stream, false, cx, |s| s.try_read(buf));
        if let Poll::Ready(Ok(n)) = read {
            this.drained = n < want;
        }
        read
    }
}

impl AsyncWrite for TcpIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        poll_io(&mut this.write_wait, &this.stream, true, cx, |s| s.try_write(buf))
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[IoSlice<'_>]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match bufs {
            [] => Poll::Ready(Ok(0)),
            [one] => poll_io(&mut this.write_wait, &this.stream, true, cx, |s| s.try_write(one)),
            _ => poll_io(&mut this.write_wait, &this.stream, true, cx, |s| try_write_vectored(s, bufs)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Nothing is buffered here: every write went to the kernel.
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.get_mut().stream.shutdown(Shutdown::Write).map_err(to_io))
    }
}
