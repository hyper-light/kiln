//! Buffers: what a connection reads goes through one scratch buffer per thread and is kept only
//! when it holds a message (so an idle connection holds no buffer at all); what it writes gathers
//! in a [`WriteBuf`] — heads and small bodies copied into one run of bytes, large bodies kept as
//! they are — and leaves in as few writes as the transport takes.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread::LocalKey;
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
    /// What its writes are learned to cost on the shard it writes on ([`crate::turn::WRITES`], each shard's own), or a test's.
    costs: &'static LocalKey<crate::turn::Writes>,
    /// It gave its poll's shard up and hasn't come back to write since ([`WriteBuf::cap`]): its next write goes, and
    /// teaches what coming back costs.
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
    pub(crate) fn with_costs(costs: &'static LocalKey<crate::turn::Writes>) -> Self {
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
    /// tasks go between (goal 9, plan 01a1a87b2cec step 3). Each write on a shard teaches that shard what writes cost; off
    /// one, where nothing is held back, none is timed.
    pub(crate) fn poll_flush<T: AsyncWrite + Unpin>(&mut self, io: &mut T, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.is_empty() {
            let room = crate::turn::room();
            let cap = match room.and_then(|room| self.cap(room)) {
                Some(0) => {
                    self.gave_up = true;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                cap => cap.unwrap_or(usize::MAX),
            };
            // Back from giving the shard up: what coming back to write took of this poll, what putting bytes off costs.
            if std::mem::take(&mut self.gave_up)
                && let Some(poll) = hyper_rt::futures::poll_clock()
            {
                self.teach(|costs| costs.resumed(poll.held_ns()));
            }
            let began = room.map(|_| Instant::now());
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
            if let Some(began) = began {
                let took = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
                self.teach(|costs| costs.learn(u64::try_from(n).unwrap_or(u64::MAX), took));
            }
            self.advance(n);
        }
        std::task::ready!(Pin::new(&mut *io).poll_flush(cx))?;
        Poll::Ready(Ok(()))
    }
}

impl WriteBuf {
    /// The most this poll's next write may carry: what fits `room`, what's left of the poll's quantum ([`crate::turn::room`]),
    /// at what writes are learned to cost on this shard ([`crate::turn::Writes::fitting`]), `Some(0)` where nothing more does;
    /// after giving its shard up with nothing written since, what fits a whole quantum, so each poll it gave the shard up for
    /// writes at its next. None, as much as there is: before any write was timed here, and where even a whole quantum fits
    /// none (each write past it teaches it).
    fn cap(&self, room: i64) -> Option<usize> {
        let fitting = |costs: &crate::turn::Writes| match costs.fitting(room)? {
            0 if self.gave_up => crate::turn::quantum().and_then(|q| costs.fitting(crate::turn::signed(q))).filter(|n| *n > 0),
            fit => Some(fit),
        };
        // A shard's own costs are made with its thread and have nothing to drop: they're there for its whole life.
        self.costs.try_with(fitting).ok().flatten()
    }

    /// `learn` taught to what its writes cost on this shard.
    fn teach(&self, learn: impl FnOnce(&crate::turn::Writes)) {
        self.costs.try_with(learn).unwrap_or_default();
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
    /// (goal 9, plan 01a1a87b2cec step 3). Each write's length kept in `sizes`, where it has room for it (none is made).
    struct Slow {
        ns_per_byte: u64,
        sizes: Vec<usize>,
    }

    impl AsyncWrite for Slow {
        fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            let until = Instant::now() + std::time::Duration::from_nanos(self.ns_per_byte.saturating_mul(u64::try_from(buf.len()).unwrap_or(u64::MAX)));
            while Instant::now() < until {
                std::hint::spin_loop();
            }
            let this = self.get_mut();
            if this.sizes.len() < this.sizes.capacity() {
                this.sizes.push(buf.len());
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
        thread_local! {
            static COSTS: crate::turn::Writes = const { crate::turn::Writes::nothing_yet() };
        }
        let (polls, longest_cpu_ns) = crate::testing::block_on(async {
            let mut io = Slow { ns_per_byte: 2, sizes: Vec::new() };
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

    /// The sizes of the writes a flush of 1 MiB made from the start of a poll, its buffer the shard's own
    /// (`crate::turn::WRITES`), each write 2 ns a byte.
    async fn flush_a_mib() -> Result<Vec<usize>, String> {
        let mut io = Slow { ns_per_byte: 2, sizes: Vec::with_capacity(1 << 10) };
        let mut buf = WriteBuf::default();
        buf.push(Bytes::from(vec![2u8; 1 << 20]));
        hyper_rt::futures::yield_now().await;
        std::future::poll_fn(|cx| buf.poll_flush(&mut io, cx)).await.map_err(|e| e.to_string())?;
        Ok(io.sizes)
    }

    /// Goal 9, plan 01a1a87b2cec step 3: each shard learns what its own writes cost, and a write learning and fitted there
    /// allocates nothing. Do: on a fresh runtime's first shard, two windows of 32 KiB flushes at 2 ns a byte, a poll each,
    /// what they allocate counted; then 1 MiB flushed from the start of a poll there, and on its second shard. Expect: the
    /// flushes allocated nothing (each write timed, learned and fitted, the costs halved between them, the shard's own
    /// costs first touched among them); the first shard wrote its MiB in pieces that fit its quantum, the second in one
    /// write, as a fresh process writes its first: it knows nothing of the first's writes. With every shard's writes
    /// learned together (one static), the second wrote in the first's pieces.
    #[test]
    fn g9_s3_a_shards_writes_teach_its_own_costs_alone() -> Result<(), String> {
        let runtime = hyper_rt::Runtime::start(&crate::testing::config(2, 16)).map_err(|e| e.to_string())?;
        let shards: Vec<hyper_rt::SlotHolder> = runtime.shard_ids().iter().filter_map(|id| runtime.holder_of(*id)).collect();
        let (first, second) = (shards.first().copied().ok_or("no first shard")?, shards.get(1).copied().ok_or("no second shard")?);
        let counted = crate::testing::block_on_at(first, async {
            let mut io = Slow { ns_per_byte: 2, sizes: Vec::with_capacity(1 << 10) };
            let mut buf = WriteBuf::default();
            hyper_measure::alloc::begin();
            hyper_measure::alloc::pause();
            for _ in 0..2 * hyper_rt::machine::bench::MIN_SAMPLES {
                buf.push(Bytes::from(vec![1u8; 32 << 10]));
                std::future::poll_fn(|cx| {
                    hyper_measure::alloc::resume();
                    let flushed = buf.poll_flush(&mut io, cx);
                    hyper_measure::alloc::pause();
                    flushed
                })
                .await
                .map_err(|e| e.to_string())?;
                hyper_rt::futures::yield_now().await;
            }
            Ok::<_, String>(hyper_measure::alloc::end())
        })??;
        let (taught, fresh) = (crate::testing::block_on_at(first, flush_a_mib())??, crate::testing::block_on_at(second, flush_a_mib())??);
        drop(runtime);
        assert_eq!((counted.allocations, counted.reallocations), (0, 0), "the flushes' writes, learned and fitted, allocated: {counted:?}");
        assert!(taught.first().is_some_and(|n| *n < 1 << 20), "the shard that learned its writes wrote its MiB whole: {taught:?}");
        assert_eq!(fresh, [1 << 20], "a shard that wrote nothing yet holds its first write to what another shard learned");
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
