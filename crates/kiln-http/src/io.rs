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
                // The try would have blocked: its charge took nothing, and the wait armed next is the one charged
                // (`hyper_rt::coop::refund`).
                hyper_rt::coop::refund();
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

/// Whether a read's or write's next step is a native wait polled under its task's own waker, which charges the
/// task's turn itself (hyper-rt's readiness wait, `hyper_rt::coop`): the read or write then charges nothing more.
/// One charge an operation: charged here as well, a read spent its poll's first charge, which always goes on, on
/// no operation, and its wait's charge was refused at every poll once the poll's own work before it outlasted the
/// share (futures' `read_to_end` zeroing its buffer's spare room again: kiln's proxy test spun a shard so,
/// 2026-11-07).
fn charged_at_wait(wait: &Option<Wait>, native: bool) -> bool {
    native && matches!(wait, Some(Wait::Native(_)))
}

impl AsyncRead for TcpIo {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let native = is_native(cx);
        if std::mem::take(&mut this.drained) && this.read_wait.is_none() && native {
            this.read_wait = Some(Wait::Native(this.stream.readable()));
        }
        // A read is a charge of the connection task's turn on its shard (hyper_rt::coop, kiln's goal 6): a
        // socket that always has bytes yields past the task's share, as one that waits does at its wait.
        if !charged_at_wait(&this.read_wait, native) && hyper_rt::coop::poll_proceed(cx).is_pending() {
            return Poll::Pending;
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
        // A write is a charge of the connection task's turn (hyper_rt::coop): a socket that always has room
        // yields past the task's share.
        if !charged_at_wait(&this.write_wait, is_native(cx)) && hyper_rt::coop::poll_proceed(cx).is_pending() {
            return Poll::Pending;
        }
        poll_io(&mut this.write_wait, &this.stream, true, cx, |s| s.try_write(buf))
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[IoSlice<'_>]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !charged_at_wait(&this.write_wait, is_native(cx)) && hyper_rt::coop::poll_proceed(cx).is_pending() {
            return Poll::Pending;
        }
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

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    use futures_io::AsyncRead;
    use hyper_rt::tcp::{TcpListener, TcpStream};

    use super::TcpIo;

    /// Goal 6 (`hyper_rt::coop`): a read whose poll's own work before it outlasts the task's share, its readiness wait
    /// armed, takes what came at its next poll, beside a task that wants the shard. Do: arm a reader's wait with nothing
    /// to read, send it 4 KiB, then poll it until it reads, each poll spinning twice the shard's share before the read
    /// (as futures' `read_to_end` zeroes its buffer's spare room again first), beside a task whose timer falls due every
    /// quarter share. Expect: it reads at the first poll after its wait fired, a poll or two. A read that charged the
    /// task's turn itself, ahead of its wait's charge, spent its poll's first charge, which always goes on, on no
    /// operation, and its wait's was refused at every poll: it never read, its shard spinning (kiln's proxy test,
    /// 2026-11-07).
    #[test]
    fn g6_s1_a_read_after_its_polls_own_work_takes_what_came() -> Result<(), String> {
        let (polls, read) = crate::testing::block_on(async {
            let share = hyper_rt::futures::step_budget_ns().ok_or("off a shard")?;
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), 8).map_err(|e| e.to_string())?;
            let client = TcpStream::connect(listener.local_addr().map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
            let server = listener.accept().await.map_err(|e| e.to_string())?;
            let done = Arc::new(AtomicBool::new(false));
            let competing = Arc::clone(&done);
            crate::testing::spawn(async move {
                while !competing.load(Ordering::Acquire) {
                    let _ = hyper_rt::futures::sleep(share / 4).await;
                }
            });
            let mut reader = TcpIo::new(client);
            let mut buf = [0u8; 8192];
            let armed = std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut reader).poll_read(cx, &mut buf).is_pending())).await;
            if !armed {
                return Err("read before anything was sent".to_owned());
            }
            server.try_write(&[7u8; 4096]).map_err(|e| e.to_string())?.ok_or("no room for 4 KiB")?;
            let mut polls = 0u32;
            let read = std::future::poll_fn(|cx| {
                polls = polls.saturating_add(1);
                if polls > 64 {
                    return Poll::Ready(Err("no read in 64 polls".to_owned()));
                }
                while hyper_rt::futures::poll_clock().is_some_and(|p| p.held_ns() < share.saturating_mul(2)) {
                    std::hint::spin_loop();
                }
                Pin::new(&mut reader).poll_read(cx, &mut buf).map_err(|e| e.to_string())
            })
            .await;
            done.store(true, Ordering::Release);
            Ok::<_, String>((polls, read?))
        })??;
        assert!(read > 0, "read nothing");
        // Shape: the poll the wait fired for, one before it, and a wake of the task's own.
        assert!(polls <= 3, "{polls} polls to read what came");
        Ok(())
    }
}
