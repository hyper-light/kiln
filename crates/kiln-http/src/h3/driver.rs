//! The door on a real UDP socket (goal 4 step 5): one task owns the [`Door`], its batched
//! hyper-rt socket ([`Batched`]) and one timer, as hyper-rt-transport's driver does: no locks, no
//! tokio. Each turn takes what arrived, fires the timers due, hands the events to the caller,
//! sends, and parks on the socket's readiness and the soonest timer.

use std::future::{Future as _, poll_fn};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::BytesMut;
use hyper_rt::RtError;
use hyper_rt::futures::{Sleep, sleep};
use hyper_rt::readiness::Ready;
use hyper_rt::udp::{Batched, Io, Sent};

use super::conn::{Admit, Event};
use super::door::{ConnId, Door};

/// Datagrams a system call carries, either way.
const BATCH: usize = 32;
/// Turns before the task yields to the shard's other tasks.
const TURNS: usize = 64;

/// What the driver asks of its owner: CONNECTs to decide, and the events of admitted sessions,
/// with the door to act on them (read, write, open, close).
pub trait Host: Admit {
    fn event(&mut self, door: &mut Door, conn: ConnId, event: Event, now: Instant);
    /// Called once a turn, after the events, for work of the owner's own (timers it keeps, writes
    /// it has waiting); return false to stop the driver.
    fn turn(&mut self, _door: &mut Door, _now: Instant) -> bool {
        true
    }
}

/// The door, its socket and its timer.
pub struct Driver {
    door: Door,
    socket: Batched,
    out: Vec<u8>,
    arrived: Vec<(SocketAddr, BytesMut)>,
    replies: Vec<(SocketAddr, Vec<u8>)>,
    readable: Option<Ready>,
    writable: Option<Ready>,
    timer: Option<(Instant, Sleep)>,
}

#[allow(clippy::disallowed_methods, reason = "the driver is the door's I/O: its clock is the host's")]
fn now() -> Instant {
    Instant::now()
}

impl Driver {
    /// Binds `addr` (its own UDP address: the door shares no port).
    pub fn bind(door: Door, addr: SocketAddr) -> Result<Driver, RtError> {
        let socket = Batched::bind(addr, Io { batch: BATCH }, false)?;
        Ok(Driver { door, socket, out: Vec::with_capacity(1500), arrived: Vec::new(), replies: Vec::new(), readable: None, writable: None, timer: None })
    }

    /// The address bound.
    pub fn local_addr(&self) -> Result<SocketAddr, RtError> {
        self.socket.local_addr()
    }

    pub fn door(&mut self) -> &mut Door {
        &mut self.door
    }

    /// Runs until `host` stops it or the socket fails.
    pub async fn run(mut self, host: &mut impl Host) -> Result<(), RtError> {
        poll_fn(|cx| self.poll_run(cx, host)).await
    }

    fn poll_run(&mut self, cx: &mut Context<'_>, host: &mut impl Host) -> Poll<Result<(), RtError>> {
        for _ in 0..TURNS {
            let now = now();
            let mut got = 0;
            loop {
                let arrived = &mut self.arrived;
                let n = match self.socket.receive(|a, bytes| arrived.push((a.from, BytesMut::from(&*bytes)))) {
                    Ok(n) => n,
                    Err(e) => return Poll::Ready(Err(e)),
                };
                got += n;
                if n == 0 || self.arrived.len() >= BATCH * 4 {
                    break;
                }
            }
            for (from, data) in std::mem::take(&mut self.arrived) {
                if let Some(t) = self.door.handle(now, from, data, &mut self.out, host) {
                    self.replies.push((t.destination, self.out.get(..t.size).unwrap_or_default().to_vec()));
                }
            }
            if self.door.poll_timeout().is_some_and(|d| d <= now) {
                self.door.handle_timeout(now, host);
            }
            while let Some((id, e)) = self.door.poll_event() {
                host.event(&mut self.door, id, e, now);
            }
            if !host.turn(&mut self.door, now) {
                return Poll::Ready(Ok(()));
            }
            for (to, bytes) in std::mem::take(&mut self.replies) {
                self.socket.queue(to, &bytes);
            }
            let mut sent = 0;
            while sent < BATCH {
                let Some(t) = self.door.poll_transmit(now, 1, &mut self.out) else { break };
                self.socket.queue(t.destination, self.out.get(..t.size).unwrap_or_default());
                sent += 1;
            }
            let blocked = self.socket.send() == Sent::Blocked;
            if got > 0 || (sent == BATCH && !blocked) {
                continue;
            }
            // Park: the socket's readiness (and writability while it holds datagrams), the timer.
            let readable = self.readable.get_or_insert_with(|| self.socket.readable());
            match Pin::new(readable).poll(cx) {
                Poll::Ready(r) => {
                    self.readable = None;
                    if let Err(e) = r {
                        return Poll::Ready(Err(e));
                    }
                    continue;
                }
                Poll::Pending => {}
            }
            if self.socket.pending() {
                let writable = self.writable.get_or_insert_with(|| self.socket.writable());
                if let Poll::Ready(r) = Pin::new(writable).poll(cx) {
                    self.writable = None;
                    if let Err(e) = r {
                        return Poll::Ready(Err(e));
                    }
                    continue;
                }
            }
            let Some(due) = self.door.poll_timeout() else { return Poll::Pending };
            if self.timer.as_ref().is_none_or(|(armed, _)| *armed != due) {
                let wait = u64::try_from(due.saturating_duration_since(now).as_nanos()).unwrap_or(u64::MAX);
                self.timer = Some((due, sleep(wait)));
            }
            let Some((_, timer)) = self.timer.as_mut() else { return Poll::Pending };
            match Pin::new(timer).poll(cx) {
                Poll::Ready(r) => {
                    self.timer = None;
                    if let Err(e) = r {
                        return Poll::Ready(Err(e));
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

