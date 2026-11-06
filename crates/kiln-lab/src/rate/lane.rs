//! One generator thread's share of a fixed rate, open loop, with no coordinated omission (Gil Tene's term; wrk2's
//! model). The schedule says when each request falls due; a request goes out on whichever of the thread's
//! connections has room first, and its latency runs from when it fell due, not from when it went out. So a
//! server that stalls is charged for every request the stall held back, each the time it waited, as the people
//! behind those requests would have waited; a closed loop, or an open one timed from each send, would see the
//! stall once and miss the queue behind it.
//!
//! This is the bookkeeping alone, with time passed in, so it is tested with a server simulated to the
//! nanosecond (below); `super::load` drives it with the sockets.

use std::collections::VecDeque;

use super::hist::Hist;

/// Nanoseconds since the run's start, when its first request fell due.
pub type Ns = u64;

const NS_PER_S: u128 = 1_000_000_000;

/// Which requests of a rate are a thread's: every request falls due `1/rate` seconds after the last, request `k`
/// at `k / rate`, and of `lanes` threads, thread `lane` sends those whose `k` is `lane` past a multiple of
/// `lanes`. Together the threads send exactly the rate, evenly spaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    pub rate: u64,
    pub lanes: u64,
    pub lane: u64,
}

impl Schedule {
    /// The `j`th request of this thread: its place `k` among every thread's.
    pub fn k(&self, j: u64) -> u64 {
        self.lane.saturating_add(j.saturating_mul(self.lanes.max(1)))
    }

    /// When request `k` falls due.
    pub fn due(&self, k: u64) -> Ns {
        u64::try_from(u128::from(k) * NS_PER_S / u128::from(self.rate.max(1))).unwrap_or(u64::MAX)
    }

    /// This thread's first request (its `j`) that falls due at `t` or after.
    pub fn first_at(&self, t: Ns) -> u64 {
        // due(k) >= t exactly when k >= ceil(t * rate / 1e9).
        let k = (u128::from(t) * u128::from(self.rate.max(1))).div_ceil(NS_PER_S);
        let lanes = u128::from(self.lanes.max(1));
        let j = k.saturating_sub(u128::from(self.lane)).div_ceil(lanes);
        u64::try_from(j).unwrap_or(u64::MAX)
    }

    /// How many of this thread's requests fall due in `[from, to)`.
    pub fn count(&self, from: Ns, to: Ns) -> u64 {
        self.first_at(to).saturating_sub(self.first_at(from))
    }
}

/// A request on its way: its place in the schedule (its `${id}` is `k + 1`), when it fell due and when it went
/// out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Req {
    pub k: u64,
    pub due: Ns,
    pub sent: Ns,
}

/// How a request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Answered as the workload says an answer is good.
    Ok,
    /// Answered, but not so (an HTTP status the workload doesn't take, an RPC that failed).
    Bad,
    /// No answer within the timeout.
    Timeout,
    /// Its connection ended before its answer came.
    Closed,
}

/// What a thread saw of the requests that fell due in the window (and of what it sent and was answered during
/// the window, whatever fell due when).
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// Good answers' latencies, from when each request fell due.
    pub latency: Hist,
    /// The same answers from when each went out: what a measurement with coordinated omission would say.
    pub service: Hist,
    /// The thread's own lateness: from when a request fell due, or when a connection came free if none was, to
    /// when the thread sent it. A thread that kept up keeps this to microseconds.
    pub lag: Hist,
    /// The requests that waited for a connection (every one was busy as they fell due), and how long.
    pub waited: u64,
    pub wait: Hist,
    pub ok: u64,
    pub bad: u64,
    pub timeouts: u64,
    pub closed: u64,
    /// The window's requests sent.
    pub sent: u64,
    /// Requests sent, and answers (good or bad) that came, while the window lasted, whatever fell due when.
    pub sent_in_window: u64,
    pub answered_in_window: u64,
    /// The good answers' bytes (their bodies, or their messages).
    pub bytes: u64,
}

impl Seen {
    pub fn merge(&mut self, o: &Seen) {
        self.latency.merge(&o.latency);
        self.service.merge(&o.service);
        self.lag.merge(&o.lag);
        self.wait.merge(&o.wait);
        self.waited += o.waited;
        self.ok += o.ok;
        self.bad += o.bad;
        self.timeouts += o.timeouts;
        self.closed += o.closed;
        self.sent += o.sent;
        self.sent_in_window += o.sent_in_window;
        self.answered_in_window += o.answered_in_window;
        self.bytes += o.bytes;
    }
}

/// One thread's schedule and connections' room: what falls due, which connection takes it, and what the window
/// saw.
pub struct Lane {
    pub sched: Schedule,
    /// The window: requests falling due in `[warm, end)` are measured (those before warm the server up).
    pub warm: Ns,
    pub end: Ns,
    /// Requests the window left waiting for a connection are still sent until then (the run's drain), and
    /// counted unsent after.
    pub backlog_until: Ns,
    next_j: u64,
    /// Room on the connections: one entry per request a connection can take now, with when it came free, the
    /// longest free first, so the work goes round them all.
    free: VecDeque<(usize, Ns)>,
    /// Requests taken and not yet ended.
    flying: u64,
    pub seen: Seen,
}

impl Lane {
    pub fn new(sched: Schedule, warm: Ns, end: Ns, backlog_until: Ns) -> Self {
        Self { sched, warm, end, backlog_until: backlog_until.max(end), next_j: 0, free: VecDeque::new(), flying: 0, seen: Seen::default() }
    }

    /// Requests sent and not yet ended (answered, timed out, or their connection gone).
    pub fn flying(&self) -> u64 {
        self.flying
    }

    fn in_window(&self, t: Ns) -> bool {
        t >= self.warm && t < self.end
    }

    /// Connection `conn` opened at `now`, with room for `depth` requests at once.
    pub fn add_conn(&mut self, conn: usize, depth: u32, now: Ns) {
        for _ in 0..depth.max(1) {
            self.free.push_back((conn, now));
        }
    }

    /// Connection `conn` is gone: its room with it (what it had in flight is ended by the caller).
    pub fn drop_conn(&mut self, conn: usize) {
        self.free.retain(|(c, _)| *c != conn);
    }

    pub fn has_room(&self) -> bool {
        !self.free.is_empty()
    }

    /// When the next request falls due, while there's one still to send.
    pub fn next_due(&self, now: Ns) -> Option<Ns> {
        let due = self.sched.due(self.sched.k(self.next_j));
        (due < self.end && (now < self.end || now < self.backlog_until)).then_some(due)
    }

    /// Whether requests the window had fall due remain to send.
    pub fn backlog(&self, now: Ns) -> bool {
        self.next_due(now).is_some_and(|d| d <= now)
    }

    /// The next request, if one has fallen due by `now` and a connection has room: that connection and the
    /// request, sent at `now`.
    pub fn take(&mut self, now: Ns) -> Option<(usize, Req)> {
        let due = self.next_due(now)?;
        if due > now {
            return None;
        }
        let (conn, since) = self.free.pop_front()?;
        let k = self.sched.k(self.next_j);
        self.next_j += 1;
        self.flying += 1;
        if self.in_window(due) {
            self.seen.sent += 1;
            // It could have gone at `started`: when it fell due, or when a connection came free after.
            let started = due.max(since);
            if started > due {
                self.seen.waited += 1;
                self.seen.wait.add(started - due);
            }
            self.seen.lag.add(now.saturating_sub(started));
        }
        if self.in_window(now) {
            self.seen.sent_in_window += 1;
        }
        Some((conn, Req { k, due, sent: now }))
    }

    /// `req` on `conn` ended at `now` as `outcome`, with `bytes` of answer; its room goes back to the connection
    /// when `room` (not when the connection is ending).
    pub fn done(&mut self, conn: usize, req: Req, now: Ns, outcome: Outcome, bytes: u64, room: bool) {
        if room {
            self.free.push_back((conn, now));
        }
        self.flying = self.flying.saturating_sub(1);
        if self.in_window(now) && matches!(outcome, Outcome::Ok | Outcome::Bad) {
            self.seen.answered_in_window += 1;
        }
        if !self.in_window(req.due) {
            return;
        }
        match outcome {
            Outcome::Ok => {
                self.seen.ok += 1;
                self.seen.latency.add(now.saturating_sub(req.due));
                self.seen.service.add(now.saturating_sub(req.sent));
                self.seen.bytes += bytes;
            }
            Outcome::Bad => self.seen.bad += 1,
            Outcome::Timeout => self.seen.timeouts += 1,
            Outcome::Closed => self.seen.closed += 1,
        }
    }

    /// The window's requests never sent: still waiting for a connection when the drain ended.
    pub fn unsent(&self) -> u64 {
        self.sched.count(self.warm, self.end).saturating_sub(self.seen.sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Ns = 1_000_000;
    const US: Ns = 1_000;

    /// A server simulated to the nanosecond: it answers a request `service` after it comes, but holds whatever
    /// comes during `stall` until the stall ends.
    struct Server {
        service: Ns,
        stall: (Ns, Ns),
    }

    impl Server {
        fn answers(&self, sent: Ns) -> Ns {
            if sent >= self.stall.0 && sent < self.stall.1 { self.stall.1 + self.service } else { sent + self.service }
        }
    }

    /// Runs `lane` against `server` until nothing is left, sending each request the moment it can: the requests
    /// as they went (each with when its answer came).
    fn drive(lane: &mut Lane, server: &Server) -> Vec<(Req, Ns)> {
        let mut flying: Vec<(Ns, usize, Req)> = Vec::new();
        let mut log = Vec::new();
        let mut now = 0;
        loop {
            while let Some((c, r)) = lane.take(now) {
                flying.push((server.answers(now), c, r));
            }
            let answer = flying.iter().map(|f| f.0).min();
            let due = if lane.has_room() { lane.next_due(now) } else { None };
            let Some(next) = [answer, due].into_iter().flatten().min() else { break };
            now = next.max(now);
            let (done, left): (Vec<_>, Vec<_>) = flying.into_iter().partition(|f| f.0 <= now);
            flying = left;
            for (at, c, r) in done {
                lane.done(c, r, at, Outcome::Ok, 10, true);
                log.push((r, at));
            }
        }
        log
    }

    /// Goal 10 (Ada's fixed-rate benchmark): threads share a rate evenly, each request `1/rate` after the last,
    /// and a thread knows how many of its requests fall due in a span.
    #[test]
    fn rate_threads_share_the_schedule_evenly() {
        let lanes: Vec<Schedule> = (0..3).map(|lane| Schedule { rate: 3000, lanes: 3, lane }).collect();
        let mut all: Vec<Ns> = lanes.iter().flat_map(|s| (0..1000).map(move |j| s.due(s.k(j)))).collect();
        all.sort_unstable();
        for (i, pair) in all.windows(2).enumerate() {
            let gap = pair.get(1).copied().unwrap_or(0) - pair.first().copied().unwrap_or(0);
            assert!(gap == 333_333 || gap == 333_334, "gap {i}: {gap} ns");
        }
        let one = lanes.get(1).copied().unwrap_or(Schedule { rate: 1, lanes: 1, lane: 0 });
        assert_eq!(one.due(one.k(0)), 333_333);
        assert_eq!(one.first_at(0), 0);
        assert_eq!(one.first_at(333_333), 0);
        assert_eq!(one.first_at(333_334), 1);
        // A second at 3,000 a second, a thousand for each of three threads.
        assert_eq!(lanes.iter().map(|s| s.count(0, 1_000 * MS)).collect::<Vec<_>>(), vec![1000, 1000, 1000]);
        assert_eq!(lanes.iter().map(|s| s.count(5 * MS, 15 * MS)).sum::<u64>(), 30);
    }

    /// Goal 10 (Ada's fixed-rate benchmark): no coordinated omission. At 1,000 a second on two connections, a
    /// server that holds every request for the 100 ms from 500 ms is charged for each request the stall held back,
    /// from when it fell due: about a hundred slow answers, their latencies spread from the stall's length down,
    /// their mean half the stall's. Timed from each send (coordinated omission) it looks like two slow answers.
    #[test]
    fn rate_no_coordinated_omission_a_stall_is_charged_to_every_request_it_held_back() {
        let sched = Schedule { rate: 1000, lanes: 1, lane: 0 };
        let mut lane = Lane::new(sched, 0, 1_000 * MS, 1_000 * MS);
        lane.add_conn(0, 1, 0);
        lane.add_conn(1, 1, 0);
        let log = drive(&mut lane, &Server { service: 100 * US, stall: (500 * MS, 600 * MS) });
        let s = &lane.seen;
        assert_eq!((s.ok, s.sent, lane.unsent(), lane.flying()), (1000, 1000, 0, 0), "every request of the window answered");
        // From when each fell due: the stall's queue is there.
        let held = log.iter().filter(|(r, at)| at - r.due >= 10 * MS).count();
        assert!((88..=100).contains(&held), "{held} requests waited 10 ms or more");
        assert!(s.latency.quantile(0.99).unwrap_or(0) >= 80 * MS, "p99 {:?}", s.latency.quantile(0.99));
        assert!(s.latency.max().unwrap_or(0) >= 100 * MS, "the first held waited the whole stall: {:?}", s.latency.max());
        // 100 held requests waiting 50 ms on average, and the backlog's drain, over 1,000: about 5.4 ms.
        let mean = s.latency.mean().unwrap_or(0.0);
        assert!((4.5e6..=6.0e6).contains(&mean), "the stall's queue averages half its length over the window's requests: {mean} ns");
        assert!(s.waited >= 95, "{} waited for a connection", s.waited);
        // From each send: two slow answers in a thousand, and the p99 a service time (to the histogram's 0.2%).
        let p99 = s.service.quantile(0.99).unwrap_or(0);
        assert!((100 * US..=100 * US + 100 * US / 500).contains(&p99), "timed from each send, the stall vanishes at p99: {p99}");
        assert!(s.service.mean().unwrap_or(0.0) < 0.4e6, "and its mean is a fraction of a millisecond");
        // The thread itself was never late: whatever waited, waited for a connection.
        assert_eq!(s.lag.max(), Some(0));
    }

    /// Goal 10 (Ada's fixed-rate benchmark): a thread's lateness is its own only while a connection had room; a
    /// request that waited for a connection is charged that wait apart.
    #[test]
    fn rate_the_threads_lateness_and_a_wait_for_a_connection_are_told_apart() {
        let sched = Schedule { rate: 1000, lanes: 1, lane: 0 };
        let mut lane = Lane::new(sched, 0, 10 * MS, 10 * MS);
        lane.add_conn(0, 1, 0);
        // Due at 0, sent 50 µs late with the connection free: the thread's lag.
        let (c, r) = lane.take(50 * US).unwrap_or((9, Req { k: 9, due: 9, sent: 9 }));
        assert_eq!((c, r), (0, Req { k: 0, due: 0, sent: 50 * US }));
        assert!(lane.take(2 * MS).is_none(), "no room until the first is answered");
        // Answered at 3 ms; the next (due at 1 ms) goes at 3.01 ms: a 2 ms wait, 10 µs of lag.
        lane.done(c, r, 3 * MS, Outcome::Ok, 0, true);
        let (_, r2) = lane.take(3 * MS + 10 * US).unwrap_or((9, Req { k: 9, due: 9, sent: 9 }));
        assert_eq!(r2.due, MS);
        assert_eq!((lane.seen.waited, lane.seen.wait.max(), lane.seen.lag.max()), (1, Some(2 * MS), Some(50 * US)));
        assert_eq!(lane.seen.lag.quantile(0.0), Some(10 * US));
    }

    /// Goal 10 (Ada's fixed-rate benchmark): only requests due in the window are measured; the warm-up's aren't,
    /// and what the window left waiting is sent through the drain, then counted unsent.
    #[test]
    fn rate_the_window_measures_its_own_and_counts_what_it_never_sent() {
        let sched = Schedule { rate: 1000, lanes: 1, lane: 0 };
        // Warm-up 0-5 ms, window 5-15 ms, the drain sends the window's backlog until 20 ms.
        let mut lane = Lane::new(sched, 5 * MS, 15 * MS, 20 * MS);
        let log = drive(&mut lane, &Server { service: 10 * US, stall: (0, 0) });
        assert_eq!(log.len(), 0, "no connection, nothing sent");
        assert_eq!(lane.unsent(), 10, "the window's ten requests");
        let mut lane = Lane::new(sched, 5 * MS, 15 * MS, 20 * MS);
        lane.add_conn(0, 1, 0);
        let log = drive(&mut lane, &Server { service: 10 * US, stall: (0, 0) });
        assert_eq!(log.len(), 15, "the warm-up's five and the window's ten");
        assert_eq!((lane.seen.ok, lane.seen.latency.n(), lane.unsent()), (10, 10, 0));
        // A server that holds everything from 8 ms to 30 ms: the window's backlog is sent until 20 ms, so none
        // of it; the two in flight are answered at 30 ms.
        let mut lane = Lane::new(sched, 5 * MS, 15 * MS, 20 * MS);
        lane.add_conn(0, 1, 0);
        drive(&mut lane, &Server { service: 10 * US, stall: (8 * MS, 30 * MS) });
        assert_eq!((lane.seen.ok, lane.unsent()), (4, 6), "5, 6, 7 and the one held; 9 to 14 never sent");
        assert_eq!(lane.seen.latency.max(), Some(30 * MS + 10 * US - 8 * MS));
    }
}
