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
use std::task::{Context, Poll, ready};

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
///
/// Each try is one charge of the task's turn on its shard (hyper_rt::coop, kiln's goal 6), made where it goes on:
/// a native wait that fired was charged by hyper-rt as it fired, any other way on is charged here, and a wait
/// that still waits is no charge. So a refusal only ever stands in for a try, and a poll's first charge, which
/// always goes on, goes to one. Charged as they were polled, a read or write that waited took that charge, and a
/// read refused at its wait once the poll's own work outlasted the share was woken at once, poll after poll,
/// never reading (the front door's pipe test, Quiver's report of 11-07).
fn poll_io<T>(wait: &mut Option<Wait>, stream: &TcpStream, writable: bool, cx: &mut Context<'_>, mut attempt: impl FnMut(&TcpStream) -> Result<Option<T>, RtError>) -> Poll<io::Result<T>> {
    let native = is_native(cx);
    loop {
        match wait.take() {
            Some(Wait::Native(mut ready)) if native => match Pin::new(&mut ready).poll(cx) {
                Poll::Pending => {
                    *wait = Some(Wait::Native(ready));
                    return Poll::Pending;
                }
                Poll::Ready(outcome) => outcome.map_err(to_io)?,
            },
            Some(Wait::Bridged(bridge)) => match bridge.poll(cx) {
                Poll::Pending => {
                    *wait = Some(Wait::Bridged(bridge));
                    return Poll::Pending;
                }
                Poll::Ready(outcome) => {
                    outcome.map_err(to_io)?;
                    ready!(hyper_rt::coop::poll_proceed(cx));
                }
            },
            // No wait, or a native one polled now under a foreign waker, withdrawn here and armed again below.
            None | Some(Wait::Native(_)) => ready!(hyper_rt::coop::poll_proceed(cx)),
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
            this.read_wait = Some(Wait::Native(this.stream.readable()));
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

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};

    use futures_io::AsyncRead;
    use hyper_rt::futures::{sleep, spawn, yield_now};
    use hyper_rt::machine::clock::monotonic_ns;
    use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
    use hyper_rt::tcp::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};

    use super::TcpIo;

    /// Shape: the share under test, a fixed step budget (no wake tracking), as hyper-rt's own coop tests have it: long
    /// against a loaded host's scheduling noise, short enough for many shares in well under a second.
    const QUANTUM_NS: u64 = 2_000_000;
    /// Shape: the polls past which a read counts as spun, far past the three these reads take.
    const SPUN: u32 = 64;

    fn config() -> RuntimeConfig {
        RuntimeConfig {
            shards: 1,
            tasks_per_shard: 16,
            timers_per_shard: 16,
            interests_per_shard: 64,
            ring_entries: 64,
            step_budget_ns: QUANTUM_NS,
            // Shape: a twentieth of the share, so a timer falls due within one.
            timer_tick_ns: QUANTUM_NS / 20,
            batch: 16,
            pin: false,
            cores: Vec::new(),
            page_bytes: 4096,
            spin_ns: 0,
            wake_tracking: None,
            latency_objective_ns: None,
        }
    }

    /// A connected pair on the loopback: ours, and its peer.
    async fn pair() -> Result<(TcpIo, TcpStream), String> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), 8).map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let ours = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        let theirs = listener.accept().await.map_err(|e| e.to_string())?;
        Ok((TcpIo::new(ours), theirs))
    }

    /// Reads `io` to its end the way futures' `read_to_end` does on a loaded shard: every poll first works twice its
    /// share (`read_to_end` zeroes its buffer's spare room again at every poll), then polls `before` (a leaf of the
    /// same task polled ahead of the read), then reads until a read waits, beside a timer falling due within every
    /// share, so that the shard is wanted whenever a poll outlasts its share. `waited` is set once a read has waited.
    /// What it read and its polls' trail: a poll's reads, each `x` (bytes), `e` (the end) or `w` (waited), the polls
    /// apart by `|`.
    async fn read_to_end_beside(io: &mut TcpIo, mut before: impl FnMut(&mut Context<'_>), waited: &AtomicBool) -> Result<(Vec<u8>, String), String> {
        let done = Arc::new(AtomicBool::new(false));
        let until = Arc::clone(&done);
        spawn(async move {
            while !until.load(Ordering::Acquire) && sleep(QUANTUM_NS / 4).await.is_ok() {}
        })
        .map_err(|e| e.to_string())?;
        // The reads begin a poll of their own, not the one that made their sockets.
        yield_now().await;
        let (mut got, mut polls, mut trail, mut buf) = (Vec::new(), 0u32, String::new(), [0u8; 64]);
        let read = poll_fn(|cx| {
            polls += 1;
            if polls > SPUN {
                return Poll::Ready(Err(format!("spun: {trail}")));
            }
            if polls > 1 {
                trail.push('|');
            }
            let worked = monotonic_ns().saturating_add(2 * QUANTUM_NS);
            while monotonic_ns() < worked {
                std::hint::spin_loop();
            }
            before(cx);
            loop {
                match Pin::new(&mut *io).poll_read(cx, &mut buf) {
                    Poll::Ready(Ok(0)) => {
                        trail.push('e');
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Ready(Ok(n)) => {
                        trail.push('x');
                        got.extend_from_slice(buf.get(..n).unwrap_or_default());
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(format!("read: {e} after {trail}"))),
                    Poll::Pending => {
                        trail.push('w');
                        waited.store(true, Ordering::Release);
                        return Poll::Pending;
                    }
                }
            }
        })
        .await;
        done.store(true, Ordering::Release);
        read.map(|()| (got, trail))
    }

    /// Kiln's goal 6, plan 01a1a59c2abe step 1: the front door's pipe test spun a shard for hours beside kiln's other
    /// tests (Quiver's report of 11-07). Its reader, futures' `read_to_end`, outlasted its share before each read, and
    /// `TcpIo` charged the read as it began (the poll's first charge, which always goes on) and hyper-rt's readiness
    /// wait charged it again as it registered: refused there, the read woke its task at once with no wait registered,
    /// and the next poll was refused at the same place, for ever, the bytes that came meanwhile unread. Do: a read
    /// whose polls each work twice their share; its peer sends a byte once it has waited, then ends. Expect: the byte
    /// and the end read, in three polls: one that waits, one that reads the byte (and finds the socket drained, so
    /// waits again), one that reads the end.
    #[test]
    fn g6_s1_a_read_on_a_wanted_shard_waits_for_its_bytes_and_reads_them() -> Result<(), String> {
        let mut rt = LocalRuntime::new(&config()).map_err(|e| e.to_string())?;
        let (got, trail) = rt
            .block_on(async {
                let (mut io, peer) = pair().await?;
                let waited = Arc::new(AtomicBool::new(false));
                let seen = Arc::clone(&waited);
                spawn(async move {
                    while !seen.load(Ordering::Acquire) {
                        yield_now().await;
                    }
                    // A byte, then the end as the peer goes with its task (a write refused shows as the byte missing).
                    let _ = peer.write_all(b"x").await;
                })
                .map_err(|e| e.to_string())?;
                read_to_end_beside(&mut io, |_| {}, &waited).await
            })
            .map_err(|e| e.to_string())??;
        assert_eq!(got, b"x");
        assert!(trail.matches('|').count() < 3, "three polls at the most to wait, read the byte and read the end: {trail}");
        Ok(())
    }

    /// Kiln's goal 6, plan 01a1a59c2abe step 1: the same test's other shape, a leaf that waits polled ahead of the read
    /// in every poll (there, the write half, waiting for room the read would have made). `TcpIo` charged that leaf at
    /// every poll, so it took each poll's first charge, which always goes on, and the read after the poll's own work
    /// was refused at every poll. Do: a read on a socket whose peer sends nothing, polled ahead of a read with a byte and
    /// its end to take, every poll working twice its share. Expect: the byte and the end read in three polls at the
    /// most, the quiet read waiting all along.
    #[test]
    fn g6_s1_a_read_that_waits_leaves_the_polls_charge_to_the_read_beside_it() -> Result<(), String> {
        let mut rt = LocalRuntime::new(&config()).map_err(|e| e.to_string())?;
        let (got, trail, woke) = rt
            .block_on(async {
                let (mut quiet, _silent) = pair().await?;
                let (mut io, peer) = pair().await?;
                peer.write_all(b"x").await.map_err(|e| e.to_string())?;
                peer.shutdown(Shutdown::Write).map_err(|e| e.to_string())?;
                let (mut woke, mut nothing) = (false, [0u8; 1]);
                let (got, trail) = read_to_end_beside(&mut io, |cx| woke |= Pin::new(&mut quiet).poll_read(cx, &mut nothing).is_ready(), &AtomicBool::new(false)).await?;
                Ok::<_, String>((got, trail, woke))
            })
            .map_err(|e| e.to_string())??;
        assert!(!woke, "the quiet socket's peer sent nothing, yet its read went on");
        assert_eq!(got, b"x");
        assert!(trail.matches('|').count() < 3, "three polls at the most to read a byte and the end beside the waiting read: {trail}");
        Ok(())
    }
}
