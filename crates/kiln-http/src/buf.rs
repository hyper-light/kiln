//! Buffers: what a connection reads goes through one scratch buffer per thread and is kept only
//! when it holds a message (so an idle connection holds no buffer at all); what it writes gathers
//! in a [`WriteBuf`] — heads and small bodies copied into one run of bytes, large bodies kept as
//! they are — and leaves in as few writes as the transport takes.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use futures_io::{AsyncRead, AsyncWrite};

/// The scratch buffer's size: one read takes at most this much.
pub(crate) const SCRATCH: usize = 64 << 10;

/// A body this small is copied next to its head rather than written from its own buffer.
const COPY_BELOW: usize = 4 << 10;

/// The most buffers a write gathers.
const MAX_IOVECS: usize = 64;

/// Spare write buffers kept per thread.
pub(crate) const POOL: usize = 64;

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
pub(crate) struct WriteBuf {
    segs: VecDeque<Seg>,
    tail: Vec<u8>,
    /// What of the first piece (the first segment, or the tail when there are none) went out already.
    sent: usize,
    /// What its writes are learned to cost ([`crate::turn::Writes`]): every connection's, but a test's own.
    costs: &'static crate::turn::Writes,
    /// It gave its poll's shard up with nothing written since ([`WriteBuf::cap`]): its next write goes.
    gave_up: bool,
}

impl Default for WriteBuf {
    fn default() -> Self {
        Self { segs: VecDeque::new(), tail: Vec::new(), sent: 0, costs: &crate::turn::WRITES, gave_up: false }
    }
}

impl WriteBuf {
    /// One whose writes teach, and are held to, `costs` alone (a test's).
    #[cfg(test)]
    pub(crate) fn with_costs(costs: &'static crate::turn::Writes) -> Self {
        let mut buf = Self::default();
        buf.costs = costs;
        buf
    }

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

    /// Writes until everything went out (`Ready(Ok)`), the transport is full (`Pending`) or fails. On a shard, a poll writes
    /// what fits what's left of its quantum, at what writes are learned to cost a byte ([`WriteBuf::cap`]), and gives its
    /// shard up (`Pending`, its task woken at once) where nothing more fits, to write on at its next poll: the shard's other
    /// tasks go between (goal 9, plan 01a1a87b2cec step 3). Each write teaches what writes cost.
    pub(crate) fn poll_flush<T: AsyncWrite + Unpin>(&mut self, io: &mut T, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.is_empty() {
            let cap = match self.cap() {
                Some(0) => {
                    self.gave_up = true;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                cap => cap.unwrap_or(usize::MAX),
            };
            let began = Instant::now();
            let n = if self.segs.is_empty() {
                let rest = self.tail.get(self.sent..).unwrap_or_default();
                let rest = rest.get(..cap.min(rest.len())).unwrap_or(rest);
                std::task::ready!(Pin::new(&mut *io).poll_write(cx, rest))?
            } else {
                let mut slices: [IoSlice<'_>; MAX_IOVECS] = [IoSlice::new(&[]); MAX_IOVECS];
                let (mut count, mut left) = (0, cap);
                let pieces = self.segs.iter().map(Seg::bytes).chain(std::iter::once(self.tail.as_slice()));
                for (i, bytes) in pieces.take(MAX_IOVECS).enumerate() {
                    if left == 0 {
                        break;
                    }
                    let bytes = if i == 0 { bytes.get(self.sent..).unwrap_or_default() } else { bytes };
                    let bytes = bytes.get(..left.min(bytes.len())).unwrap_or(bytes);
                    left -= bytes.len();
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
            self.gave_up = false;
            self.costs.learn(u64::try_from(n).unwrap_or(u64::MAX), u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX));
            self.advance(n);
        }
        std::task::ready!(Pin::new(&mut *io).poll_flush(cx))?;
        Poll::Ready(Ok(()))
    }
}

impl WriteBuf {
    /// The most this poll's next write may carry: what fits what's left of the poll's quantum ([`crate::turn::room`]) at
    /// what writes are learned to cost ([`crate::turn::Writes::fitting`]), `Some(0)` where nothing more does; after giving its
    /// shard up with nothing written since, what fits a whole quantum, so each poll it gave the shard up for writes at its
    /// next. None, as much as there is: off a shard, before any write was timed, and where even a whole quantum fits none
    /// (each write past it teaches it).
    fn cap(&self) -> Option<usize> {
        match self.costs.fitting(crate::turn::room()?)? {
            0 if self.gave_up => crate::turn::quantum().and_then(|q| self.costs.fitting(q)).filter(|n| *n > 0),
            fit => Some(fit),
        }
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

    /// A transport each write to which takes its time on the thread, `ns_per_byte` a byte: what a flush on a shard is held to
    /// (goal 9, plan 01a1a87b2cec step 3).
    struct Slow {
        ns_per_byte: u64,
    }

    impl AsyncWrite for Slow {
        fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            let until = Instant::now() + std::time::Duration::from_nanos(self.ns_per_byte.saturating_mul(u64::try_from(buf.len()).unwrap_or(u64::MAX)));
            while Instant::now() < until {
                std::hint::spin_loop();
            }
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what writes are learned to cost. Nothing learned holds no write back; then a write
    /// is expected to take its bytes at the writes' rate, and what fits a room is that room's worth at that rate, less four
    /// times how far writes fall from it (RFC 6298's K).
    #[test]
    fn g9_s3_writes_are_learned_by_their_bytes() {
        let w = crate::turn::Writes::nothing_yet();
        assert_eq!(w.fitting(250_000), None, "nothing learned holds nothing back");
        // 64 KiB in 128 µs, again and again: 2 ns a byte, and no write falls from it.
        for _ in 0..32 {
            w.learn(64 << 10, 131_072);
        }
        assert_eq!(w.fitting(250_000), Some(125_000), "250 µs at 2 ns a byte");
        assert_eq!(w.fitting(0), Some(0), "no room fits nothing");
        // Writes that fall far from the rate leave less room for the next.
        for _ in 0..8 {
            w.learn(64 << 10, 400_000);
            w.learn(64 << 10, 50_000);
        }
        assert!(w.fitting(250_000).is_some_and(|n| n < 125_000), "{:?}", w.fitting(250_000));
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a flush on a shard writes what fits what's left of its poll's quantum and gives its
    /// shard up for the rest, rather than writing all of it in a poll that spent most of its quantum already (as an answer's
    /// handler does before its answer is written). Do: teach a buffer's own costs with writes at 2 ns a byte, then flush 1
    /// MiB (2 ms of writes) in a poll that has held its shard 200 µs of the test runtime's 250 µs. Expect: it takes more than
    /// one poll, and none of them is 1 ms on the CPU. The flush before wrote it all in that one poll (2.2 ms).
    #[test]
    fn g9_s3_a_flush_writes_within_its_polls_quantum() -> Result<(), String> {
        static COSTS: crate::turn::Writes = crate::turn::Writes::nothing_yet();
        let (polls, longest_cpu_ns) = crate::testing::block_on(async {
            let mut io = Slow { ns_per_byte: 2 };
            let mut buf = WriteBuf::with_costs(&COSTS);
            for _ in 0..8 {
                buf.push(Bytes::from(vec![1u8; 32 << 10]));
                std::future::poll_fn(|cx| buf.poll_flush(&mut io, cx)).await.map_err(|e| e.to_string())?;
            }
            buf.push(Bytes::from(vec![2u8; 1 << 20]));
            let (mut polls, mut longest) = (0u32, 0u64);
            std::future::poll_fn(|cx| {
                polls += 1;
                let (flushed, spent) = hyper_rt::futures::spent(|| {
                    // The answer's handler: most of the poll.
                    while polls == 1 && hyper_rt::futures::poll_clock().is_some_and(|p| p.held_ns() < 200_000) {
                        std::hint::spin_loop();
                    }
                    buf.poll_flush(&mut io, cx)
                });
                longest = longest.max(spent.cpu_ns.unwrap_or(spent.wall_ns));
                flushed
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok::<_, String>((polls, longest))
        })??;
        assert!(polls > 1, "the flush wrote 1 MiB in the poll that had held its shard 200 µs already");
        assert!(longest_cpu_ns < 1_000_000, "a poll of the flush held its shard {longest_cpu_ns} ns on the CPU");
        Ok(())
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
