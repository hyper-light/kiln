//! Buffers: what a connection reads goes through one scratch buffer per thread and is kept only
//! when it holds a message (so an idle connection holds no buffer at all); what it writes gathers
//! in a [`WriteBuf`] — heads and small bodies copied into one run of bytes, large bodies kept as
//! they are — and leaves in as few writes as the transport takes.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures_io::{AsyncRead, AsyncWrite};

/// The scratch buffer's size: one read takes at most this much.
pub(crate) const SCRATCH: usize = 64 << 10;

/// A body this small is copied next to its head rather than written from its own buffer.
const COPY_BELOW: usize = 4 << 10;

/// The most buffers a write gathers.
const MAX_IOVECS: usize = 64;

/// Spare write buffers kept per thread.
const POOL: usize = 64;

thread_local! {
    static SCRATCH_BUF: Cell<Option<Box<[u8]>>> = const { Cell::new(None) };
    static WRITE_POOL: Cell<Vec<Vec<u8>>> = const { Cell::new(Vec::new()) };
}

/// Reads once from `io` into `into`, through the thread's scratch buffer: the byte count (0 at end of
/// stream). `into` grows by exactly what was read.
pub(crate) fn poll_read_into<T: AsyncRead + Unpin>(io: &mut T, cx: &mut Context<'_>, into: &mut BytesMut, max: usize) -> Poll<io::Result<usize>> {
    let mut scratch = SCRATCH_BUF.with(Cell::take).unwrap_or_else(|| vec![0u8; SCRATCH].into_boxed_slice());
    let room = max.clamp(1, SCRATCH);
    let out = match scratch.get_mut(..room) {
        Some(buf) => match Pin::new(&mut *io).poll_read(cx, buf) {
            Poll::Ready(Ok(n)) => {
                into.extend_from_slice(buf.get(..n).unwrap_or_default());
                Poll::Ready(Ok(n))
            }
            other => other,
        },
        None => Poll::Ready(Ok(0)),
    };
    SCRATCH_BUF.with(|c| c.set(Some(scratch)));
    out
}

fn take_vec() -> Vec<u8> {
    WRITE_POOL.with(|p| {
        let mut pool = p.take();
        let v = pool.pop();
        p.set(pool);
        v.unwrap_or_else(|| Vec::with_capacity(4096))
    })
}

fn give_vec(mut v: Vec<u8>) {
    if v.capacity() > SCRATCH {
        return;
    }
    v.clear();
    WRITE_POOL.with(|p| {
        let mut pool = p.take();
        if pool.len() < POOL {
            pool.push(v);
        }
        p.set(pool);
    });
}

enum Seg {
    Owned(Vec<u8>),
    Shared(Bytes),
}

impl Seg {
    fn bytes(&self) -> &[u8] {
        match self {
            Seg::Owned(v) => v,
            Seg::Shared(b) => b,
        }
    }
}

/// Bytes waiting to be written, in order: whole segments, then the run being appended to.
#[derive(Default)]
pub(crate) struct WriteBuf {
    segs: VecDeque<Seg>,
    tail: Vec<u8>,
    /// What of the first piece (the first segment, or the tail when there are none) went out already.
    sent: usize,
}

impl WriteBuf {
    pub(crate) fn is_empty(&self) -> bool {
        self.segs.is_empty() && self.tail.len() <= self.sent
    }

    /// Bytes waiting.
    pub(crate) fn len(&self) -> usize {
        (self.segs.iter().map(|s| s.bytes().len()).sum::<usize>() + self.tail.len()).saturating_sub(self.sent)
    }

    /// The run of bytes to append to.
    pub(crate) fn tail(&mut self) -> &mut Vec<u8> {
        if self.tail.capacity() == 0 {
            self.tail = take_vec();
        }
        &mut self.tail
    }

    pub(crate) fn extend(&mut self, bytes: &[u8]) {
        self.tail().extend_from_slice(bytes);
    }

    /// A body's bytes: copied when small, kept as they are when not.
    pub(crate) fn push(&mut self, bytes: Bytes) {
        if bytes.len() < COPY_BELOW {
            self.extend(&bytes);
            return;
        }
        if !self.tail.is_empty() {
            let run = std::mem::take(&mut self.tail);
            self.segs.push_back(Seg::Owned(run));
        }
        self.segs.push_back(Seg::Shared(bytes));
    }

    fn advance(&mut self, mut n: usize) {
        while n > 0 {
            let Some(front) = self.segs.front() else {
                self.sent += n;
                if self.sent >= self.tail.len() {
                    self.tail.clear();
                    self.sent = 0;
                }
                return;
            };
            let left = front.bytes().len().saturating_sub(self.sent);
            if n < left {
                self.sent += n;
                return;
            }
            n -= left;
            self.sent = 0;
            if let Some(Seg::Owned(v)) = self.segs.pop_front() {
                give_vec(v);
            }
        }
    }

    /// Gives the tail's buffer back to the thread while nothing waits: an idle connection holds none.
    pub(crate) fn release(&mut self) {
        if self.is_empty() && self.tail.capacity() > 0 {
            self.segs.clear();
            self.sent = 0;
            give_vec(std::mem::take(&mut self.tail));
        }
    }

    /// Writes until everything went out (`Ready(Ok)`), the transport is full (`Pending`) or fails.
    pub(crate) fn poll_flush<T: AsyncWrite + Unpin>(&mut self, io: &mut T, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.is_empty() {
            let n = if self.segs.is_empty() {
                let rest = self.tail.get(self.sent..).unwrap_or_default();
                std::task::ready!(Pin::new(&mut *io).poll_write(cx, rest))?
            } else {
                let mut slices: [IoSlice<'_>; MAX_IOVECS] = [IoSlice::new(&[]); MAX_IOVECS];
                let mut count = 0;
                let pieces = self.segs.iter().map(Seg::bytes).chain(std::iter::once(self.tail.as_slice()));
                for (i, bytes) in pieces.take(MAX_IOVECS).enumerate() {
                    let bytes = if i == 0 { bytes.get(self.sent..).unwrap_or_default() } else { bytes };
                    if let Some(slot) = slices.get_mut(i) {
                        *slot = IoSlice::new(bytes);
                        count = i + 1;
                    }
                }
                std::task::ready!(Pin::new(&mut *io).poll_write_vectored(cx, slices.get(..count).unwrap_or_default()))?
            };
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.advance(n);
        }
        std::task::ready!(Pin::new(&mut *io).poll_flush(cx))?;
        Poll::Ready(Ok(()))
    }
}

impl Drop for WriteBuf {
    fn drop(&mut self) {
        for seg in self.segs.drain(..) {
            if let Seg::Owned(v) = seg {
                give_vec(v);
            }
        }
        if self.tail.capacity() > 0 {
            give_vec(std::mem::take(&mut self.tail));
        }
    }
}

/// Appends `n` in decimal.
pub(crate) fn put_dec(out: &mut Vec<u8>, n: u64) {
    if n < 10 {
        out.push(b'0' + n as u8);
        return;
    }
    if n < 100 {
        out.push(b'0' + (n / 10) as u8);
        out.push(b'0' + (n % 10) as u8);
        return;
    }
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut n = n;
    loop {
        i -= 1;
        if let Some(d) = digits.get_mut(i) {
            *d = b'0' + (n % 10) as u8;
        }
        n /= 10;
        if n == 0 || i == 0 {
            break;
        }
    }
    out.extend_from_slice(digits.get(i..).unwrap_or_default());
}

/// Appends `n` in hexadecimal (a chunk's size).
pub(crate) fn put_hex(out: &mut Vec<u8>, mut n: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digits = [0u8; 16];
    let mut i = digits.len();
    loop {
        i -= 1;
        if let (Some(d), Some(h)) = (digits.get_mut(i), HEX.get((n & 0xf) as usize)) {
            *d = *h;
        }
        n >>= 4;
        if n == 0 || i == 0 {
            break;
        }
    }
    out.extend_from_slice(digits.get(i..).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_written_out() {
        for (n, dec, hex) in [(0u64, "0", "0"), (9, "9", "9"), (10, "10", "a"), (255, "255", "ff"), (u64::MAX, "18446744073709551615", "ffffffffffffffff")] {
            let (mut d, mut h) = (Vec::new(), Vec::new());
            put_dec(&mut d, n);
            put_hex(&mut h, n);
            assert_eq!((d.as_slice(), h.as_slice()), (dec.as_bytes(), hex.as_bytes()));
        }
    }

    /// A transport that takes at most `limit` bytes a write and keeps them.
    struct Trickle {
        out: Vec<u8>,
        limit: usize,
    }

    impl AsyncWrite for Trickle {
        fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let n = buf.len().min(this.limit);
            this.out.extend_from_slice(buf.get(..n).unwrap_or_default());
            Poll::Ready(Ok(n))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn written_bytes_keep_their_order_through_short_writes() {
        let mut w = WriteBuf::default();
        w.extend(b"head ");
        w.push(Bytes::from(vec![b'x'; COPY_BELOW]));
        w.extend(b" tail");
        assert_eq!(w.len(), 5 + COPY_BELOW + 5);
        let mut t = Trickle { out: Vec::new(), limit: 7 };
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(w.poll_flush(&mut t, &mut cx), Poll::Ready(Ok(()))));
        assert!(w.is_empty());
        let mut want = b"head ".to_vec();
        want.extend(vec![b'x'; COPY_BELOW]);
        want.extend(b" tail");
        assert_eq!(t.out, want);
    }
}
