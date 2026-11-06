//! TCP on the runtime (docs/runtime.md §5.2), on Linux, macOS and Windows, IPv4 and IPv6. `accept` and
//! `read` await read-readiness through the shard's driver; a write that finds the send buffer full awaits
//! write-readiness, so a stalled peer yields the shard instead of blocking it; `connect` awaits
//! write-readiness for the handshake. The platform calls are [`crate::tcpsys`]'s: `rustix` on Unix,
//! Winsock 2 on Windows, readiness through kqueue, epoll and IOCP's AFD polls (`AFD_POLL_ACCEPT` for a
//! listener, `AFD_POLL_SEND` for a write, `AFD_POLL_CONNECT_FAIL` for a refused connect).
//!
//! **Reads into the caller's buffer.** [`TcpStream::read`] and [`TcpStream::read_vectored`] read straight
//! into slices the caller owns, with no allocation and no copy in the runtime, so a body streams into the
//! caller's own (aligned) buffers.
//!
//! **Every stream is `TCP_NODELAY`**: a small write leaves at once instead of waiting for the peer to
//! acknowledge the previous one (Nagle, RFC 896). A reply written while an earlier one is unacknowledged
//! otherwise waits for the client's delayed ACK, 40 ms on Linux (`TCP_DELACK_MIN`; slates measured 42 ms,
//! docs/bugs/2026-10-05-nagle-delayed-ack.md). A write to a peer that closed is `EPIPE`, never a `SIGPIPE`.
//!
//! **Where offered**, [`TcpStream::set_notsent_lowat`] makes writability mean "the unsent queue is low"
//! (Linux, macOS), keeping a slow peer's backlog in the application's hands where a budget sees it, and
//! [`TcpStream::set_user_timeout`] bounds how long sent data may stay unacknowledged (Linux
//! `TCP_USER_TIMEOUT`, RFC 5482; macOS `TCP_RXT_CONNDROPTIME`; Windows `TCP_MAXRTMS`).
//!
//! **A connection budget.** A listener given a [`ConnectionBudget`] takes a slot of it for every connection
//! it accepts; past the budget it closes the connection at once and counts it (a refusal by close, which a
//! client sees as a reset). The budget is process-wide: every shard's listener may share one, and a slot
//! returns when its stream drops, wherever that is.
//!
//! **Accepting on every shard** is [`serve`]: a listener per shard with `SO_REUSEPORT` on Linux and macOS,
//! and on Windows, which has no equivalent, one acceptor handing each connection to the least-loaded shard.
//!
//! TCP is host-local here: it has no simulated fabric and runs on a real runtime.

mod serve;

pub use serve::{Serving, serve};

use std::io::{IoSlice, IoSliceMut};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::RtError;
use crate::netsys::{Family, Io};
use crate::readiness::{readable, writable};
use crate::sync::cell::{CellRef, claim};
use crate::tcpsys::{self, Stream};

// The address types are `core::net`'s, re-exported so a caller names an address without a backend.
pub use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

pub use crate::tcpsys::Shutdown;

/// The owned OS handle a stream or listener moves as: a file descriptor on Unix, a Winsock socket on
/// Windows.
pub type OwnedStream = tcpsys::OwnedStream;

// ------------------------------------------------------------------------------------------ the budget

/// The most connections a process holds open, shared by every listener given it and counted across
/// shards (docs/runtime.md §5.2). A handle to a process-wide cell (`crate::sync::cell`), not shared
/// ownership: cloning it adds a handle, and the cell is freed with the last handle or slot.
#[derive(Debug)]
pub struct ConnectionBudget {
    cell: CellRef,
    max: u64,
}

/// One connection's place in a [`ConnectionBudget`], and in its shard's count when [`serve`] accepted it;
/// both given back when dropped, wherever the stream went.
#[derive(Debug)]
pub struct ConnectionSlot {
    global: Option<CellRef>,
    local: Option<CellRef>,
}

impl ConnectionBudget {
    /// A budget of `max` connections. Refused `Capacity` at the cell table's bound.
    pub fn new(max: u64) -> Result<Self, RtError> {
        Ok(Self {
            cell: claim(1)?,
            max,
        })
    }

    /// A slot, if fewer than the maximum are open. Wait-free: one add, undone when it overshot.
    pub fn try_take(&self) -> Option<ConnectionSlot> {
        let cell = self.cell.cell()?;
        if cell.state.fetch_add(1, Ordering::AcqRel) >= self.max {
            cell.state.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        self.cell.retain();
        Some(ConnectionSlot {
            global: Some(self.cell),
            local: None,
        })
    }

    /// Connections open now.
    pub fn open(&self) -> u64 {
        self.cell
            .cell()
            .map_or(0, |cell| cell.state.load(Ordering::Acquire).min(self.max))
    }

    /// The maximum.
    pub fn max(&self) -> u64 {
        self.max
    }
}

impl Clone for ConnectionBudget {
    fn clone(&self) -> Self {
        self.cell.retain();
        Self {
            cell: self.cell,
            max: self.max,
        }
    }
}

impl Drop for ConnectionBudget {
    fn drop(&mut self) {
        self.cell.release();
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        if let Some(global) = self.global {
            if let Some(cell) = global.cell() {
                cell.state.fetch_sub(1, Ordering::AcqRel);
            }
            global.release();
        }
        if let Some(local) = self.local {
            serve::returned(local);
        }
    }
}

// ------------------------------------------------------------------------------------------ listener

/// A listening TCP socket whose `accept` awaits the shard's driver for the next connection.
#[derive(Debug)]
pub struct TcpListener {
    stream: Stream,
    /// The kernel's accept queue, as asked: an accept takes at most this many connections before it yields.
    backlog: u32,
    budget: Option<ConnectionBudget>,
    /// Connections closed at once because the budget was full.
    refused: AtomicU64,
}

impl TcpListener {
    /// Binds a listening socket to `addr` (port 0 for an OS-assigned port, then
    /// [`TcpListener::local_addr`]) with a `backlog` of pending connections the kernel queues before the
    /// accept loop takes them — the caller derives it from its connection fan-in; the OS clamps it.
    pub fn bind(addr: impl Into<SocketAddr>, backlog: u32) -> Result<TcpListener, RtError> {
        Self::listening(tcpsys::bound(addr.into(), false)?, backlog)
    }

    /// Binds with `SO_REUSEPORT`, so every shard binds its own listener to one address and the kernel
    /// spreads connections among them (Linux and macOS; Windows has no equivalent, docs/runtime.md §5.2).
    #[cfg(unix)]
    pub fn bind_shared(addr: impl Into<SocketAddr>, backlog: u32) -> Result<TcpListener, RtError> {
        Self::listening(tcpsys::bound(addr.into(), true)?, backlog)
    }

    fn listening(stream: Stream, backlog: u32) -> Result<TcpListener, RtError> {
        tcpsys::listen(&stream, i32::try_from(backlog).unwrap_or(i32::MAX))?;
        Ok(TcpListener {
            stream,
            backlog: backlog.max(1),
            budget: None,
            refused: AtomicU64::new(0),
        })
    }

    /// Accepts within `budget`: a connection past it is closed at once and counted
    /// ([`TcpListener::refused`]).
    #[must_use]
    pub fn with_budget(mut self, budget: ConnectionBudget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Adopts a bound, listening socket a supervisor handed over (its port survives a restart). It is made
    /// non-blocking; the caller states the backlog it listened with.
    pub fn from_owned(owned: OwnedStream, backlog: u32) -> Result<TcpListener, RtError> {
        Ok(TcpListener {
            stream: tcpsys::adopt(owned)?,
            backlog: backlog.max(1),
            budget: None,
            refused: AtomicU64::new(0),
        })
    }

    /// Gives up the socket, for a supervisor that hands it to the process it spawns.
    pub fn into_owned(self) -> OwnedStream {
        tcpsys::into_owned(self.stream)
    }

    /// The local address the listener is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, RtError> {
        tcpsys::local_addr(&self.stream)
    }

    /// Connections closed at once because the budget was full.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Accepts the next connection, awaiting readability through the driver when none is pending. The
    /// accepted stream is non-blocking and `TCP_NODELAY`. At most the backlog's worth of connections is
    /// taken (refused or aborted ones included) before the task yields to the driver, which reports the
    /// listener ready at once if more wait.
    pub async fn accept(&self) -> Result<TcpStream, RtError> {
        // An accept is a charge of its task's turn (`crate::coop`).
        crate::coop::consume_budget().await;
        loop {
            for _ in 0..self.backlog {
                // A system call on the shard (kiln's goal 9, plan 01a1a87b2cec step 5).
                crate::registry::count_here(|live| &live.socket_accepts);
                let accepted = match tcpsys::accept(&self.stream)? {
                    Io::Ready(accepted) => accepted,
                    Io::WouldBlock => break,
                    Io::Interrupted | Io::Astray => continue,
                };
                let slot = match &self.budget {
                    None => None,
                    Some(budget) => match budget.try_take() {
                        Some(slot) => Some(slot),
                        None => {
                            // Closed by the drop: the refusal a client sees as a reset.
                            drop(accepted);
                            self.refused.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    },
                };
                // A peer that reset the connection already can refuse its options (macOS, BSD): that
                // connection is dropped, and the listener goes on (mantle's review, finding 5).
                if tcpsys::set_options(&accepted).is_err() {
                    continue;
                }
                return Ok(TcpStream {
                    stream: accepted,
                    slot,
                });
            }
            readable(self.stream.raw_id()).await?;
        }
    }
}

// ------------------------------------------------------------------------------------------ stream

/// A connected TCP stream whose reads and writes await the shard's driver.
#[derive(Debug)]
pub struct TcpStream {
    stream: Stream,
    /// Its place in the listener's budget, if it was accepted under one.
    slot: Option<ConnectionSlot>,
}

impl TcpStream {
    /// Connects to `addr`, awaiting the driver until the handshake completes; a refused or unreachable
    /// peer surfaces then, as a typed refusal.
    pub async fn connect(addr: impl Into<SocketAddr>) -> Result<TcpStream, RtError> {
        let addr = addr.into();
        let stream = tcpsys::stream_socket(Family::of(addr))?;
        if let Io::WouldBlock | Io::Interrupted | Io::Astray = tcpsys::connect(&stream, addr)? {
            writable(stream.raw_id()).await?;
            if let Some(code) = tcpsys::take_error(&stream)? {
                return Err(RtError::DriverRefused {
                    call: "connect",
                    code: Some(code),
                });
            }
        }
        tcpsys::set_options(&stream)?;
        Ok(TcpStream { stream, slot: None })
    }

    /// Gives up the stream's socket, to move the connection to another shard. No readiness is armed
    /// between awaits (every registration is one-shot), so nothing on this shard waits on it once its task
    /// stops awaiting. The budget slot, if any, goes with [`TcpStream::into_parts`].
    pub fn into_owned(self) -> OwnedStream {
        self.into_parts().0
    }

    /// The stream's socket and its budget slot, to move both to another shard.
    pub fn into_parts(self) -> (OwnedStream, Option<ConnectionSlot>) {
        (tcpsys::into_owned(self.stream), self.slot)
    }

    /// Adopts a connected stream's socket on the current shard: non-blocking and `TCP_NODELAY`, as an
    /// accepted one.
    pub fn from_owned(owned: OwnedStream) -> Result<TcpStream, RtError> {
        Self::from_parts(owned, None)
    }

    /// A connected local (`AF_UNIX`) stream: [`crate::local`] reads and writes through this type, with no
    /// `TCP_NODELAY` (a local socket has no Nagle) and no `SIGPIPE`.
    pub(crate) fn local(stream: Stream) -> Result<TcpStream, RtError> {
        tcpsys::set_no_sigpipe(&stream)?;
        Ok(TcpStream { stream, slot: None })
    }

    /// The handle [`crate::readiness::readable`] and [`crate::readiness::writable`] wait on (an fd on Unix,
    /// a socket's low 32 bits on Windows): for a wait kept apart from the stream's own calls.
    pub fn readiness_handle(&self) -> i32 {
        self.stream.raw_id()
    }

    /// The stream's seam handle.
    pub(crate) fn seam(&self) -> &Stream {
        &self.stream
    }

    /// Counts the stream in its shard's `local` cell ([`serve`]), given back with its slot.
    pub(crate) fn count_in(&mut self, local: CellRef) {
        serve::counted(local);
        match &mut self.slot {
            Some(slot) => slot.local = Some(local),
            None => {
                self.slot = Some(ConnectionSlot {
                    global: None,
                    local: Some(local),
                });
            }
        }
    }

    /// Adopts a socket and the budget slot it moved with.
    pub fn from_parts(
        owned: OwnedStream,
        slot: Option<ConnectionSlot>,
    ) -> Result<TcpStream, RtError> {
        let stream = tcpsys::adopt(owned)?;
        tcpsys::set_options(&stream)?;
        Ok(TcpStream { stream, slot })
    }

    /// The local address.
    pub fn local_addr(&self) -> Result<SocketAddr, RtError> {
        tcpsys::local_addr(&self.stream)
    }

    /// The peer's address.
    pub fn peer_addr(&self) -> Result<SocketAddr, RtError> {
        tcpsys::peer_addr(&self.stream)
    }

    /// Whether `TCP_NODELAY` is set (it always is; read back from the OS).
    pub fn nodelay(&self) -> Result<bool, RtError> {
        tcpsys::nodelay(&self.stream)
    }

    /// Makes the stream writable only while fewer than `bytes` sent bytes wait unsent
    /// (`TCP_NOTSENT_LOWAT`). Whether the platform offers it (Linux and macOS do, Windows does not).
    pub fn set_notsent_lowat(&self, bytes: u32) -> Result<bool, RtError> {
        tcpsys::set_notsent_lowat(&self.stream, bytes)
    }

    /// `TCP_NOTSENT_LOWAT` read back, where offered.
    pub fn notsent_lowat(&self) -> Result<Option<u32>, RtError> {
        tcpsys::notsent_lowat(&self.stream)
    }

    /// Drops the connection when sent data stays unacknowledged for `ms` milliseconds (macOS rounds up to
    /// whole seconds). Whether the platform offers it.
    pub fn set_user_timeout(&self, ms: u32) -> Result<bool, RtError> {
        tcpsys::set_user_timeout(&self.stream, ms)
    }

    /// The deadline [`TcpStream::set_user_timeout`] set, in milliseconds, where offered.
    pub fn user_timeout_ms(&self) -> Result<Option<u32>, RtError> {
        tcpsys::user_timeout_ms(&self.stream)
    }

    /// Awaits read readiness once (or a spurious wake).
    pub fn readable(&self) -> crate::readiness::Ready {
        crate::readiness::ready(crate::readiness::Target::Os(self.stream.raw_id()), false)
    }

    /// Awaits write readiness once (or a spurious wake).
    pub fn writable(&self) -> crate::readiness::Ready {
        crate::readiness::ready(crate::readiness::Target::Os(self.stream.raw_id()), true)
    }

    /// Reads into `buf` without waiting: the byte count (zero at end of stream), or `None` when nothing is
    /// ready now.
    pub fn try_read(&self, buf: &mut [u8]) -> Result<Option<usize>, RtError> {
        let read = tcpsys::read(&self.stream, buf)?;
        counted_read(&read);
        Ok(match read {
            Io::Ready(n) => Some(n),
            Io::WouldBlock | Io::Interrupted | Io::Astray => None,
        })
    }

    /// Reads into `buf`, awaiting readability when nothing is ready: the byte count, or zero at end of
    /// stream (the peer closed its write half).
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize, RtError> {
        // A read is a charge of its task's turn (`crate::coop`): a stream that always has bytes ready yields
        // past its share, as one that waits does at its wait.
        crate::coop::consume_budget().await;
        loop {
            if let Some(n) = self.try_read(buf)? {
                return Ok(n);
            }
            readable(self.stream.raw_id()).await?;
        }
    }

    /// Reads into `bufs` in order, in one call, awaiting readability when nothing is ready.
    pub async fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> Result<usize, RtError> {
        crate::coop::consume_budget().await;
        loop {
            let read = tcpsys::read_vectored(&self.stream, bufs)?;
            counted_read(&read);
            match read {
                Io::Ready(n) => return Ok(n),
                Io::WouldBlock | Io::Interrupted | Io::Astray => {
                    readable(self.stream.raw_id()).await?;
                }
            }
        }
    }

    /// Writes from `buf` without waiting: the bytes the kernel took, or `None` when the send buffer is full.
    pub fn try_write(&self, buf: &[u8]) -> Result<Option<usize>, RtError> {
        let wrote = tcpsys::write(&self.stream, buf)?;
        counted_write(&wrote);
        Ok(match wrote {
            Io::Ready(n) => Some(n),
            Io::WouldBlock | Io::Interrupted | Io::Astray => None,
        })
    }

    /// Writes from `buf`, awaiting writability while the send buffer is full: the bytes the kernel took.
    pub async fn write(&self, buf: &[u8]) -> Result<usize, RtError> {
        // A write is a charge of its task's turn (`crate::coop`).
        crate::coop::consume_budget().await;
        loop {
            if let Some(n) = self.try_write(buf)? {
                return Ok(n);
            }
            writable(self.stream.raw_id()).await?;
        }
    }

    /// Writes from `bufs` in order, in one call, awaiting writability while the send buffer is full.
    pub async fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> Result<usize, RtError> {
        crate::coop::consume_budget().await;
        loop {
            let wrote = tcpsys::write_vectored(&self.stream, bufs)?;
            counted_write(&wrote);
            match wrote {
                Io::Ready(n) => return Ok(n),
                Io::WouldBlock | Io::Interrupted | Io::Astray => {
                    writable(self.stream.raw_id()).await?;
                }
            }
        }
    }

    /// Writes all of `buf`, awaiting writability whenever the send buffer is full, so a stalled peer yields
    /// the shard. A write of a non-empty slice that takes nothing without blocking is a broken pipe.
    pub async fn write_all(&self, buf: &[u8]) -> Result<(), RtError> {
        let mut sent = 0;
        while let Some(rest) = buf.get(sent..).filter(|rest| !rest.is_empty()) {
            match self.write(rest).await? {
                0 => {
                    return Err(RtError::DriverRefused {
                        call: "write",
                        code: None,
                    });
                }
                n => sent = sent.saturating_add(n),
            }
        }
        Ok(())
    }

    /// Shuts one half, or both.
    pub fn shutdown(&self, how: Shutdown) -> Result<(), RtError> {
        tcpsys::shutdown(&self.stream, how)
    }
}

/// Counts a read the calling shard's task made, and whether it brought nothing (kiln's goal 9, plan
/// 01a1a87b2cec step 5: each a system call on the shard, and one that brings nothing is followed by a wait).
fn counted_read<T>(read: &Io<T>) {
    crate::registry::count_here(|live| &live.socket_reads);
    if !matches!(read, Io::Ready(_)) {
        crate::registry::count_here(|live| &live.socket_reads_empty);
    }
}

/// Counts a write the calling shard's task made, and whether the send buffer was full (as [`counted_read`]).
fn counted_write<T>(wrote: &Io<T>) {
    crate::registry::count_here(|live| &live.socket_writes);
    if !matches!(wrote, Io::Ready(_)) {
        crate::registry::count_here(|live| &live.socket_writes_full);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kiln's goal 9, plan 01a1a87b2cec step 5: each accept, read and write a shard's tasks make of a TCP socket
    /// is counted on the shard, the read that found nothing ready too, and the readiness wait armed after it; a
    /// write the send buffer took whole is no full one.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn g9_s5_a_shards_socket_calls_and_arms_are_counted() {
        let config = crate::runtime::RuntimeConfig {
            shards: 1,
            tasks_per_shard: 8,
            timers_per_shard: 8,
            interests_per_shard: 8,
            ring_entries: 16,
            step_budget_ns: 1_000_000,
            timer_tick_ns: 100_000,
            batch: 16,
            pin: false,
            cores: Vec::new(),
            page_bytes: 4096,
            spin_ns: 0,
            wake_tracking: None,
            latency_objective_ns: None,
        };
        let mut rt = crate::runtime::LocalRuntime::new(&config).unwrap();
        let before = rt.counters();
        let read = rt
            .block_on(async {
                let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)), 4)?;
                let client = TcpStream::connect(listener.local_addr()?).await?;
                let server = listener.accept().await?;
                let mut buf = [0u8; 4];
                // The read goes first and finds nothing, so it waits; the write after it brings the bytes.
                let (read, wrote) = crate::combine::join2(server.read(&mut buf), async {
                    crate::futures::yield_now().await;
                    client.write_all(b"ping").await
                })
                .await;
                wrote?;
                read.map(|n| buf.get(..n).map(<[u8]>::to_vec))
            })
            .unwrap()
            .unwrap();
        assert_eq!(read.as_deref(), Some(&b"ping"[..]));
        let after = rt.counters();
        let moved = |of: fn(&crate::shard_loop::Counters) -> u64| of(&after) - of(&before);
        assert!(moved(|c| c.socket_accepts) >= 1, "the accept");
        assert!(moved(|c| c.socket_reads) >= 2, "the read that found nothing, and the one that brought the bytes");
        assert!(moved(|c| c.socket_reads_empty) >= 1, "the read that went first found nothing");
        assert!(moved(|c| c.socket_reads_empty) < moved(|c| c.socket_reads), "a read brought the bytes");
        assert_eq!(moved(|c| c.socket_writes), 1, "four bytes, one write");
        assert_eq!(moved(|c| c.socket_writes_full), 0, "the send buffer took them");
        assert!(moved(|c| c.arms) >= 1, "the empty read's wait was armed");
    }
}
