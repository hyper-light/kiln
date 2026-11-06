//! The protocol's end-to-end window (goal 3 of the browser protocol program, plan
//! `01a1a832eb2f711f9c6f51d88effe89d` step 4): the server keeps no more than a window of bytes the
//! page hasn't said it received, so every buffer between them (the server's socket, a front door's
//! pipe, a socat forwarder, a proxy kiln doesn't run, the bottleneck's queue) together holds at most
//! that window (research f36). It is the only lever on macOS and Windows servers and through relays
//! kiln doesn't run, and it works whatever congestion control the kernel has (f27).
//!
//! The window is sized from the path as BBR models it (research f16, f20): the bottleneck's rate (the
//! most delivered in a recent stretch, as a rate) times the least round trip, times a gain, so the pipe
//! stays full and the queue stays short. The acknowledgements come from the page itself
//! (`kiln_bp::server::Event::Acked`: the bytes it had received when it answered a probe the carrier
//! asked for), so they measure the whole path. hyper-quic's own controllers take packet-level
//! samples it builds inside the crate (its `RateSample` can't be made outside it), so this is the
//! same model, kept small and over bytes:
//!
//! - **Startup**: the window grows by what each acknowledgement says was delivered (doubling a round
//!   trip) until the rate stops growing by a quarter for three rounds.
//! - **Steady**: twice the rate times the least round trip, at least the floor, at most the ceiling.
//! - **Probe RTT**: when the least round trip is older than its window, the window drops to the floor
//!   for a short while so the queue drains and the true round trip is seen again.
//!
//! Deterministic: time is passed in (milliseconds), and nothing else is read.

use std::collections::VecDeque;

/// The window's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The window before any acknowledgement, bytes.
    pub initial: u64,
    /// The least window, bytes: two chunks, so a chunk can leave while one is acknowledged.
    pub floor: u64,
    /// The most, bytes: the page's own credit bounds what's sent above this anyway.
    pub ceiling: u64,
    /// The steady window's gain over the path's rate times its round trip, percent.
    pub gain_pct: u64,
    /// Startup's gain, percent: BBR's 2/ln 2, so the rate measured can nearly treble a round.
    pub startup_pct: u64,
    /// How long the least round trip is kept before it's measured again, milliseconds.
    pub min_rtt_ms: u64,
    /// How long the window stays at its floor to measure it, milliseconds.
    pub probe_rtt_ms: u64,
    /// The stretch the rate is the most of: this many least round trips, at least `rate_floor_ms`.
    pub rate_rounds: u64,
    /// The rate's stretch at least, milliseconds.
    pub rate_floor_ms: u64,
}

/// Shape: BBR's: a steady gain of 2 (its cwnd gain), the least round trip kept 10 s and probed for
/// 200 ms, the rate the most of 10 rounds; the start at four 1,200-byte chunks (TCP's initial window
/// is about that, RFC 6928's ten segments being for fast links), the floor two of them.
impl Default for Config {
    fn default() -> Self {
        Self {
            initial: 4 * 1_200,
            floor: 2 * 1_200,
            ceiling: 4 << 20,
            gain_pct: 200,
            startup_pct: 289,
            min_rtt_ms: 10_000,
            probe_rtt_ms: 200,
            rate_rounds: 10,
            rate_floor_ms: 1_000,
        }
    }
}

/// Where the window is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Startup,
    Steady,
    /// Nothing more sent, at least until `until`, and until what was sent before it began (`drain`,
    /// bytes sent) has reached the page; then one acknowledgement asked with nothing ahead of it, whose
    /// round trip is the path's with no queue in it (`drained`: when the pipe was found empty). At
    /// 50 kbit/s even two chunks queued are 400 ms, so the probe goes alone.
    ProbeRtt { until: u64, drain: u64, drained: Option<u64> },
}

/// One acknowledgement as the window keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Acked {
    /// When it came.
    now: u64,
    /// When its probe went.
    sent: u64,
    /// The bytes the page had.
    got: u64,
}

/// The window of one connection.
#[derive(Debug, Clone)]
pub struct Window {
    cfg: Config,
    phase: Phase,
    w: u64,
    /// The bytes sent, all of them.
    sent: u64,
    /// The last acknowledgement.
    last: Option<Acked>,
    /// The recent ones, oldest first: a rate is taken over at least a least round trip of them.
    history: VecDeque<Acked>,
    /// The least round trip and when it was seen.
    min_rtt: Option<(u64, u64)>,
    /// The least round trip seen while probing it.
    probe_min: Option<u64>,
    /// Rate samples: when, and bytes a second.
    rates: VecDeque<(u64, u64)>,
    /// Startup's: the rate it last grew to, and the rounds since without a quarter more.
    full_rate: u64,
    flat_rounds: u8,
    /// The bytes sent when the current round began: it ends when the page has them.
    round_end: u64,
}

/// Shape: the most rate samples kept (a stretch's worth at an acknowledgement a quarter-window).
const RATES_KEPT: usize = 256;

/// Shape: the most acknowledgements kept for taking rates over: a round trip's worth at four a window.
const HISTORY_KEPT: usize = 64;

impl Window {
    pub fn new(cfg: Config) -> Self {
        let w = cfg.initial.clamp(cfg.floor, cfg.ceiling.max(cfg.floor));
        Self { cfg, phase: Phase::Startup, w, sent: 0, last: None, history: VecDeque::new(), min_rtt: None, probe_min: None, rates: VecDeque::new(), full_rate: 0, flat_rounds: 0, round_end: 0 }
    }

    /// The least window from now on: the carrier's two chunks, as its chunk changes with the path (a
    /// window smaller than the largest message would never let one go).
    pub fn set_floor(&mut self, floor: u64) {
        self.cfg.floor = floor;
        self.cfg.ceiling = self.cfg.ceiling.max(floor);
        self.w = self.w.max(floor);
    }

    /// `bytes` more were sent.
    pub fn on_sent(&mut self, bytes: u64) {
        self.sent = self.sent.saturating_add(bytes);
    }

    /// The window now, bytes.
    pub fn window(&self) -> u64 {
        match self.phase {
            Phase::ProbeRtt { .. } => 0,
            _ => self.w,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Bytes sent the page hasn't said it has.
    pub fn in_flight(&self) -> u64 {
        self.sent.saturating_sub(self.last.map_or(0, |a| a.got))
    }

    /// How many more bytes may go now.
    pub fn room(&self) -> u64 {
        self.window().saturating_sub(self.in_flight())
    }

    /// The path's rate, bytes a second: the most of the recent stretch's samples.
    pub fn rate(&self) -> Option<u64> {
        self.rates.iter().map(|(_, r)| *r).max()
    }

    /// The least round trip, milliseconds.
    pub fn min_rtt(&self) -> Option<u64> {
        self.min_rtt.map(|(r, _)| r)
    }

    /// The bytes sent, all of them.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// How far apart acknowledgements are asked for: a quarter window, at least `chunk`.
    pub fn ack_every(&self, chunk: u64) -> u64 {
        (self.window() / 4).max(chunk)
    }

    /// An acknowledgement at `now`: the page had `got` bytes, its probe went at `sent` and took `rtt`.
    /// `app_limited`: the carrier had nothing to send for part of what it covers, so its rate says the
    /// path is at least that fast, no more.
    pub fn on_ack(&mut self, now: u64, got: u64, rtt: u64, sent: u64, app_limited: bool) {
        if self.last.is_some_and(|p| got < p.got) {
            return;
        }
        let this = Acked { now, sent, got };
        self.last = Some(this);
        self.take_rtt(now, rtt, got, sent);
        // The rate over at least a least round trip: from the newest acknowledgement that long before
        // this one (else the oldest kept), over the longer of the send and the acknowledgement
        // spacing (delivery-rate estimation's rule), so acknowledgements answered together say
        // nothing they can't.
        let span = self.min_rtt().unwrap_or(0);
        let from = self.history.iter().rev().find(|h| now.saturating_sub(h.now) >= span).or(self.history.front()).copied();
        if let Some(h) = from {
            let delivered = got.saturating_sub(h.got);
            let interval = now.saturating_sub(h.now).max(sent.saturating_sub(h.sent));
            if delivered > 0 && interval > 0 && interval >= span / 2 {
                let rate = delivered.saturating_mul(1_000) / interval;
                if !app_limited || self.rate().is_none_or(|r| rate > r) {
                    self.rates.push_back((now, rate));
                }
            }
        }
        let delivered = self.history.back().map_or(got, |h| got.saturating_sub(h.got));
        self.history.push_back(this);
        while self.history.len() > HISTORY_KEPT {
            self.history.pop_front();
        }
        let horizon = span.saturating_mul(self.cfg.rate_rounds).max(self.cfg.rate_floor_ms);
        while self.rates.len() > 1 && (self.rates.front().is_some_and(|(at, _)| now.saturating_sub(*at) > horizon) || self.rates.len() > RATES_KEPT) {
            // The newest is kept, so a quiet path still has a rate.
            self.rates.pop_front();
        }
        self.size(now, delivered, app_limited);
    }

    fn take_rtt(&mut self, now: u64, rtt: u64, got: u64, sent: u64) {
        if let Phase::ProbeRtt { drain, drained, until } = self.phase {
            match drained {
                // A probe asked once the pipe was empty: its round trip is the path's own.
                Some(at) if sent >= at => self.probe_min = Some(self.probe_min.map_or(rtt, |m| m.min(rtt))),
                Some(_) => {}
                None if got >= drain => self.phase = Phase::ProbeRtt { until, drain, drained: Some(now) },
                None => {}
            }
            return;
        }
        let expired = self.min_rtt.is_none_or(|(_, at)| now.saturating_sub(at) > self.cfg.min_rtt_ms);
        if self.min_rtt.is_none_or(|(m, _)| rtt <= m) {
            self.min_rtt = Some((rtt.max(1), now));
        } else if expired && self.phase != Phase::Startup {
            self.phase = Phase::ProbeRtt { until: now.saturating_add(self.cfg.probe_rtt_ms), drain: self.sent, drained: None };
            self.probe_min = None;
        }
    }

    /// Whether the carrier should ask for an acknowledgement now beyond its cadence: the window is
    /// full and none is on its way, so nothing would free it.
    /// While the round trip is probed, that's the probe asked alone once the pipe is empty.
    pub fn stalled(&self, chunk: u64, acks_out: usize) -> bool {
        self.room() < chunk && acks_out == 0 && (self.in_flight() > 0 || matches!(self.phase, Phase::ProbeRtt { .. }))
    }

    /// The steady window: the gain times the path's rate times its least round trip.
    fn steady(&self) -> u64 {
        self.gained(self.cfg.gain_pct)
    }

    /// `pct` percent of the path's rate times its least round trip, within the floor and ceiling.
    fn gained(&self, pct: u64) -> u64 {
        let (Some(rate), Some(rtt)) = (self.rate(), self.min_rtt()) else { return self.w };
        let bdp = rate.saturating_mul(rtt) / 1_000;
        (bdp.saturating_mul(pct) / 100).clamp(self.cfg.floor, self.cfg.ceiling.max(self.cfg.floor))
    }

    fn size(&mut self, now: u64, delivered: u64, app_limited: bool) {
        let round_over = self.last.is_some_and(|a| a.got >= self.round_end);
        if round_over {
            self.round_end = self.sent;
        }
        match self.phase {
            Phase::Startup => {
                // Startup's gain over the path as measured so far: each round the rate measured can
                // grow by as much, until the path's own rate stops it.
                if !app_limited {
                    self.w = self.gained(self.cfg.startup_pct).max(self.cfg.initial).max(self.w.min(delivered.saturating_mul(2)));
                }
                if round_over && !app_limited {
                    let rate = self.rate().unwrap_or(0);
                    if rate >= self.full_rate.saturating_add(self.full_rate / 4) {
                        self.full_rate = rate;
                        self.flat_rounds = 0;
                    } else {
                        self.flat_rounds = self.flat_rounds.saturating_add(1);
                    }
                    if self.flat_rounds >= 3 {
                        self.phase = Phase::Steady;
                        self.w = self.steady();
                    }
                }
            }
            Phase::Steady => self.w = self.steady(),
            Phase::ProbeRtt { until, .. } => {
                if now >= until
                    && let Some(m) = self.probe_min.take()
                {
                    self.min_rtt = Some((m.max(1), now));
                    self.phase = Phase::Steady;
                    self.w = self.steady();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crate::rng::Rng;

    use super::*;

    /// A path for the window's simulation: a bottleneck of a rate (bytes a millisecond, changing on
    /// a schedule) with a deep queue, a fixed one-way delay each way, and loss as a retransmission's
    /// extra round trip for a share of what's sent; the page answers each probe as it arrives.
    struct Path {
        /// (from ms, bytes a second, one-way delay ms)
        schedule: Vec<(u64, u64, u64)>,
        loss_pm: u64,
        rng: Rng,
        /// What's in the bottleneck's queue: bytes left, whether that segment is lost (and sent again a
        /// round trip later), and a probe (its send time) behind them.
        queue: VecDeque<(u64, bool, Option<u64>)>,
        /// Bytes past the bottleneck, arriving at the page: (at, bytes, probe).
        flight: VecDeque<(u64, u64, Option<u64>)>,
        /// Answers on their way back: (at, got, sent).
        answers: VecDeque<(u64, u64, u64)>,
        got: u64,
        credit: f64,
    }

    impl Path {
        fn at(&self, now: u64) -> (u64, u64) {
            self.schedule.iter().rev().find(|(from, _, _)| now >= *from).map_or((6_250, 150), |(_, r, d)| (*r, *d))
        }

        fn queued(&self) -> u64 {
            self.queue.iter().map(|(b, _, _)| *b).sum()
        }

        /// A segment of `bytes` (a probe: 0), lost at the path's rate.
        fn send(&mut self, bytes: u64, probe: Option<u64>) {
            let lost = bytes > 0 && self.rng.below(1_000) < self.loss_pm;
            self.queue.push_back((bytes, lost, probe));
        }

        /// One millisecond of the path; the answers that reached the server now.
        fn tick(&mut self, now: u64) -> Vec<(u64, u64, u64)> {
            let (rate, delay) = self.at(now);
            self.credit += rate as f64 / 1_000.0;
            while let Some((bytes, lost, probe)) = self.queue.front_mut() {
                if *bytes == 0 {
                    let p = *probe;
                    self.queue.pop_front();
                    let arrive = (now + delay).max(self.flight.back().map_or(0, |f| f.0));
                    self.flight.push_back((arrive, 0, p));
                    continue;
                }
                let take = (*bytes as f64).min(self.credit.floor()) as u64;
                if take == 0 {
                    break;
                }
                *bytes -= take;
                self.credit -= take as f64;
                // A lost segment arrives a round trip (and a timer's worth) later, and what follows it
                // waits for it at the page: TCP delivers in order.
                let arrive = now + delay + if *lost { 2 * delay + 200 } else { 0 };
                let arrive = arrive.max(self.flight.back().map_or(0, |f| f.0));
                self.flight.push_back((arrive, take, None));
                if *bytes == 0 {
                    self.queue.pop_front();
                }
            }
            if self.queue.is_empty() {
                self.credit = self.credit.min(1.0);
            }
            while self.flight.front().is_some_and(|f| f.0 <= now) {
                if let Some((_, b, probe)) = self.flight.pop_front() {
                    self.got += b;
                    if let Some(sent) = probe {
                        self.answers.push_back((now + delay, self.got, sent));
                    }
                }
            }
            let mut out = Vec::new();
            while self.answers.front().is_some_and(|a| a.0 <= now) {
                if let Some(a) = self.answers.pop_front() {
                    out.push(a);
                }
            }
            out
        }
    }

    /// What a run did: the window each second, and the most in flight over the window.
    #[derive(Debug, PartialEq)]
    struct Trace {
        windows: Vec<u64>,
        excess: i64,
        queued_max: u64,
        window_max: u64,
    }

    /// A bulk flood through `path` for `ms` milliseconds, a chunk at a time while the window has room,
    /// an acknowledgement asked for every quarter window.
    fn flood(mut path: Path, ms: u64) -> (Trace, Window) {
        const CHUNK: u64 = 1_200;
        /// Shape: when startup's overshoot has drained, on the slowest path the tests give.
        const WARM_MS: u64 = 20_000;
        let mut w = Window::new(Config::default());
        let (mut since_ask, mut windows, mut excess, mut queued_max, mut window_max) = (0u64, Vec::new(), i64::MIN, 0u64, 0u64);
        let mut out = 0u64;
        for now in 0..ms {
            for (at, got, sent) in path.tick(now) {
                w.on_ack(at, got, at - sent, sent, false);
                out -= 1;
            }
            while w.room() >= CHUNK {
                w.on_sent(CHUNK);
                since_ask += CHUNK;
                path.send(CHUNK, None);
                if since_ask >= w.ack_every(CHUNK) {
                    path.send(0, Some(now));
                    out += 1;
                    since_ask = 0;
                }
                // What's in flight once a chunk went, against the window it went under.
                excess = excess.max(w.in_flight() as i64 - w.window() as i64 - CHUNK as i64);
            }
            // A full window with no acknowledgement on its way asks for one, or nothing would free it.
            if w.stalled(CHUNK, usize::try_from(out).unwrap_or(1)) {
                path.send(0, Some(now));
                out += 1;
                since_ask = 0;
            }
            // The queue once startup's first overshoot has drained.
            if now >= WARM_MS {
                queued_max = queued_max.max(path.queued());
                window_max = window_max.max(w.window());
            }
            if now % 1_000 == 999 {
                windows.push(w.window());
            }
        }
        (Trace { windows, excess, queued_max, window_max }, w)
    }

    fn path(schedule: Vec<(u64, u64, u64)>, loss_pm: u64, seed: u64) -> Path {
        Path { schedule, loss_pm, rng: Rng::new(seed), queue: VecDeque::new(), flight: VecDeque::new(), answers: VecDeque::new(), got: 0, credit: 0.0 }
    }

    /// bp-g3 step 4: the window over a path whose rate steps between 50 kbit/s and 10 Mbit/s, with
    /// round trips of 50 to 800 ms, 0 to 5% loss and a deep queue. The window follows the rate within
    /// a few round trips each way (once what was in flight at a step down has drained: a full window of
    /// a fast path is that window's time at the slow one, for any controller), what's in flight never
    /// passes it (by more than the chunk that was let go when it had room), and the same seed gives the
    /// same trace.
    #[test]
    fn bp_g3_s4_the_window_follows_the_path() {
        // 50 kbit/s = 6,250 B/s; 10 Mbit/s = 1,250,000 B/s.
        let schedule = vec![(0, 6_250, 150), (40_000, 1_250_000, 25), (80_000, 6_250, 400), (120_000, 1_250_000, 100), (160_000, 12_500, 300)];
        const END: u64 = 260_000;
        for loss in [0, 10, 50] {
            let (trace, w) = flood(path(schedule.clone(), loss, 7), END);
            assert!(trace.excess <= 0, "loss {loss}‰: in flight past the window by {}", trace.excess);
            // Each stretch's last window against its path's rate times its round trip (and the gain):
            // within a quarter and four times of the 2 x BDP it aims at, once a few round trips passed.
            for (i, (from, rate, delay)) in schedule.iter().enumerate() {
                let end = usize::try_from(schedule.get(i + 1).map_or(END, |s| s.0) / 1_000).unwrap_or(0);
                let mut last: Vec<u64> = trace.windows.get(end.saturating_sub(10)..end).unwrap_or_default().to_vec();
                last.sort_unstable();
                let median = last.get(last.len() / 2).copied().unwrap_or(0);
                let aim = (2 * rate * 2 * delay / 1_000).max(Config::default().floor);
                // Loss holds what follows a lost segment back at the page (TCP delivers in order), and
                // the page's acknowledgements can't see past that as the kernel's selective ones do: a
                // lossy fast path gets a smaller window than its rate times its round trip.
                let low = if loss == 0 { aim / 4 } else { aim / 8 };
                assert!(median >= low && median <= aim * 4, "loss {loss}‰, stretch from {from} ms ({rate} B/s, {delay} ms each way): median window {median}, aiming at {aim}; {:?}", trace.windows);
            }
            assert!(w.min_rtt().is_some() && w.rate().is_some());
            // A deep queue is never filled: at most a few windows of the slow path wait in it.
            assert!(trace.queued_max < 2 << 20, "loss {loss}‰: the queue held {} B", trace.queued_max);
        }
        // Deterministic: the same seed, the same trace.
        assert_eq!(flood(path(schedule.clone(), 50, 7), 60_000).0, flood(path(schedule, 50, 7), 60_000).0);
    }

    /// bp-g3 step 4: on a slow path with a deep queue, the window holds the queue to about its own
    /// size: an urgent frame queued behind a flood waits about the window's time, not the queue's.
    #[test]
    fn bp_g3_s4_a_deep_queue_holds_about_a_window() {
        let (trace, _) = flood(path(vec![(0, 6_250, 300)], 0, 1), 60_000);
        let window = trace.window_max;
        assert!(window <= 4 * 6_250 * 600 / 1_000, "window {window} at 50 kbit/s, 600 ms round trip: {:?}", trace.windows);
        assert!(trace.queued_max <= window + 1_200, "queued {} with window {window}", trace.queued_max);
    }
}
