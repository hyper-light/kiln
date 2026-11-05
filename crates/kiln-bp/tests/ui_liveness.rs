//! bp-g9 step 2 (goal 9, plan 01a1a832ec1b): the seeded link traces of the UI's
//! `ui/tests/liveness-traces.ts`, the same generator line for line, through this crate's rule. The
//! TypeScript rule must reach the same verdicts: both fold them into `DIGEST`. The model is the
//! TypeScript file's to say (50 kbit/s, a 6 s FIFO queue, 5% loss each way, 1 MB in flight, TCP's
//! head-of-line hold and retransmissions, a page shown, hidden or frozen).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use kiln_bp::liveness::{Liveness, Verdict};

/// The TypeScript test's `DIGEST`: seeds 0..64 live and 1,000,000.. blackholed, shown pages.
const DIGEST: u32 = 2_748_312_533;

const MTU: i64 = 1200;
const QUEUE_MS: i64 = 6000;
const LOSS_PCT: u32 = 5;
const END_MS: i64 = 600_000;
const BULK: i64 = 1 << 20;

fn ser(bytes: i64) -> i64 {
    (bytes * 4 + 24) / 25
}

/// mulberry32, as the TypeScript file computes it.
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(0x6d2b_79f5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        t ^ (t >> 14)
    }
    fn below(&mut self, n: i64) -> i64 {
        if n <= 1 { 0 } else { i64::from(self.next() % u32::try_from(n).unwrap_or(u32::MAX)) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Shown,
    Hidden,
    Frozen,
}

struct Pkt {
    seq: i64,
    bytes: i64,
    answers: Option<i64>,
    sent_at: i64,
    tries: u32,
    again: bool,
}

#[derive(Clone, Copy)]
enum Ev {
    Arrive(usize),
    Rto(usize, u32),
    Resend(usize),
    ProbeAtServer(i64),
    Check,
    Server,
}

#[derive(Default)]
struct Outcome {
    false_dead: u32,
    found_at: Option<i64>,
    bound: Option<i64>,
    digest: u32,
}

fn letter(v: Verdict) -> u32 {
    match v {
        Verdict::Live => 108,
        Verdict::Suspect => 115,
        Verdict::Dead => 100,
    }
}

struct Sim {
    g: Rng,
    q: BinaryHeap<Reverse<(i64, u64, usize)>>,
    evs: Vec<Ev>,
    n: u64,
    link_free: i64,
    last_arrive: i64,
    last_write: i64,
    seq: i64,
    srtt: i64,
    rttvar: i64,
    pkts: Vec<Pkt>,
    lost: Vec<usize>,
    owd: i64,
    jitter: i64,
    blackhole_at: Option<i64>,
}

impl Sim {
    fn push(&mut self, t: i64, e: Ev) {
        self.evs.push(e);
        self.q.push(Reverse((t, self.n, self.evs.len() - 1)));
        self.n += 1;
    }
    fn gone(&self, t: i64) -> bool {
        self.blackhole_at.is_some_and(|b| t >= b)
    }
    fn write(&mut self, t: i64, bytes: i64, answers: Option<i64>) {
        self.pkts.push(Pkt { seq: self.seq, bytes, answers, sent_at: 0, tries: 0, again: false });
        self.seq += 1;
        self.transmit(t, self.pkts.len() - 1);
        self.last_write = t;
    }
    fn transmit(&mut self, t: i64, i: usize) {
        let bytes = self.pkts.get(i).map_or(0, |p| p.bytes);
        let start = t.max(self.link_free);
        self.link_free = start + ser(bytes);
        let at = (self.link_free + self.owd + self.g.below(self.jitter)).max(self.last_arrive);
        self.last_arrive = at;
        let lose = self.gone(t) || self.g.below(100) < i64::from(LOSS_PCT);
        let rto0 = 200.max(self.srtt + 4 * self.rttvar);
        let link_free = self.link_free;
        let Some(p) = self.pkts.get_mut(i) else { return };
        p.sent_at = start;
        if lose {
            p.again = false;
            let tries = p.tries;
            self.lost.push(i);
            self.push(link_free + rto0 * (1i64 << tries.min(10)), Ev::Rto(i, tries));
        } else {
            self.push(at, Ev::Arrive(i));
        }
    }
    fn resend(&mut self, t: i64, i: usize) {
        if !self.lost.contains(&i) {
            return;
        }
        let Some(p) = self.pkts.get_mut(i) else { return };
        if p.again {
            return;
        }
        p.again = true;
        self.push(t, Ev::Resend(i));
    }
}

fn trace(seed: u32, blackhole: bool, page: Page) -> Outcome {
    let mut g = Rng(seed);
    let idle = 1000 + g.below(29_000);
    let owd = 30 + g.below(400);
    let jitter = 1 + g.below(120);
    let blackhole_at = blackhole.then(|| 20_000 + g.below(500_000));
    let frozen_from = (page == Page::Frozen).then(|| 30_000 + g.below(300_000));
    let frozen_to = frozen_from.map(|f| f + 10_000 + g.below(240_000));
    let mut bulk_left = BULK;
    let mut next_burst = 1000 + g.below(idle * 3);
    let srtt = 2 * owd + ser(MTU);
    let mut s = Sim { g, q: BinaryHeap::new(), evs: Vec::new(), n: 0, link_free: 0, last_arrive: 0, last_write: 0, seq: 0, srtt, rttvar: srtt / 2, pkts: Vec::new(), lost: Vec::new(), owd, jitter, blackhole_at };

    let fresh = |now: i64| {
        let mut l = Liveness::new(idle as u64, now as u64);
        l.round_trip((2 * owd + ser(200)) as u64);
        l
    };
    let mut live = fresh(0);
    let mut delivered = 0i64;
    let mut held: HashMap<i64, usize> = HashMap::new();
    let mut probe_at: Option<i64> = None;
    let mut probe_n = 0i64;
    let mut out = Outcome { digest: 0x811c_9dc5, ..Default::default() };
    let mut check_at = -1i64;
    let mut pending: Vec<(i64, Option<i64>)> = Vec::new();
    let mut resumed = false;
    let running = |t: i64| match (frozen_from, frozen_to) {
        (Some(f), Some(to)) => t < f || t >= to,
        _ => true,
    };
    let runs = |want: i64| {
        let mut t = want;
        if page == Page::Hidden {
            t = (t + 59_999) / 60_000 * 60_000;
        }
        if !running(t)
            && let Some(to) = frozen_to
        {
            t = to;
        }
        t
    };
    macro_rules! schedule {
        ($now:expr) => {{
            let now: i64 = $now;
            let want = runs((now + 1).max(live.next_change(now as u64) as i64));
            if want != check_at {
                check_at = want;
                s.push(want, Ev::Check);
            }
        }};
    }
    macro_rules! arrive {
        ($t:expr, $answered:expr) => {{
            let t: i64 = $t;
            if let (Some(_), Some(p)) = ($answered, probe_at) {
                live.round_trip((t - p) as u64);
            }
            live.arrived(t as u64);
            probe_at = None;
            if blackhole_at.is_some() {
                out.bound = Some(live.dead_at() as i64);
            }
            schedule!(t);
        }};
    }

    s.push(0, Ev::Server);
    schedule!(0);
    let end = if blackhole_at.is_none() { END_MS } else { 2 * END_MS };
    while out.found_at.is_none() {
        let Some(Reverse((t, _, at))) = s.q.pop() else { break };
        if t > end {
            break;
        }
        let Some(&e) = s.evs.get(at) else { break };
        match e {
            Ev::Server => {
                if bulk_left > 0 && s.link_free - t <= QUEUE_MS {
                    let b = MTU.min(bulk_left);
                    bulk_left -= b;
                    s.write(t, b, None);
                } else if bulk_left == 0 && t >= next_burst {
                    let n = 1 + s.g.below(12);
                    for i in 0..n {
                        let bytes = if i + 1 == n { 100 + s.g.below(MTU - 100) } else { MTU };
                        s.write(t, bytes, None);
                    }
                    next_burst = t + 1 + s.g.below(idle * 3);
                } else if t - s.last_write >= idle {
                    s.write(t, 24, None);
                }
                let next = if bulk_left > 0 { (t + 1).max(s.link_free - QUEUE_MS) } else { next_burst.min(s.last_write + idle) };
                s.push((t + 1).max(next), Ev::Server);
            }
            Ev::ProbeAtServer(n) => {
                if !s.gone(t) {
                    s.write(t, 24, Some(n));
                }
            }
            Ev::Rto(i, tries) => {
                if s.lost.contains(&i) && s.pkts.get(i).is_some_and(|p| p.tries == tries) {
                    s.resend(t, i);
                }
            }
            Ev::Resend(i) => {
                if s.lost.contains(&i) {
                    s.lost.retain(|&m| m != i);
                    if let Some(p) = s.pkts.get_mut(i) {
                        p.tries += 1;
                    }
                    s.transmit(t, i);
                }
            }
            Ev::Arrive(i) => {
                let (seq, tries, sent_at) = s.pkts.get(i).map_or((0, 0, 0), |p| (p.seq, p.tries, p.sent_at));
                let back = t + s.owd + s.srtt / 4;
                let behind: Vec<usize> = s.lost.iter().copied().filter(|&m| s.pkts.get(m).is_some_and(|p| p.seq < seq)).collect();
                for m in behind {
                    s.resend(back, m);
                }
                if tries == 0 {
                    let sample = t + s.owd - sent_at;
                    s.rttvar = (3 * s.rttvar + (sample - s.srtt).abs()) / 4;
                    s.srtt = (7 * s.srtt + sample) / 8;
                }
                held.insert(seq, i);
                let page_t = if running(t) { t } else { frozen_to.unwrap_or(t) };
                let mut got = false;
                let mut answered = None;
                while let Some(d) = held.remove(&delivered) {
                    if let Some(a) = s.pkts.get(d).and_then(|p| p.answers)
                        && a == probe_n
                    {
                        answered = Some(a);
                    }
                    delivered += 1;
                    got = true;
                }
                if got {
                    if page_t == t {
                        arrive!(t, answered);
                    } else {
                        s.push(page_t, Ev::Check);
                        pending.push((page_t, answered));
                    }
                }
            }
            Ev::Check => {
                if t != check_at && pending.is_empty() {
                    continue;
                }
                // A page that was frozen says it ran again before it reads what came meanwhile
                // (goal 2's `Liveness::resumed`): its own gap isn't the link's.
                if let Some(to) = frozen_to
                    && t >= to
                    && !resumed
                {
                    resumed = true;
                    live.resumed(to as u64);
                    probe_at = None;
                }
                for (at, ans) in std::mem::take(&mut pending) {
                    arrive!(at, ans);
                }
                let now = t;
                let v = live.verdict(now as u64);
                out.digest = (out.digest ^ letter(v)).wrapping_mul(0x0100_0193);
                if v == Verdict::Suspect && !live.probing() {
                    live.probe_sent(now as u64);
                    probe_at = Some(now);
                    probe_n += 1;
                    let n = probe_n;
                    let mut at = now;
                    let mut rto = 200.max(live.rtt_bound() as i64);
                    while s.g.below(100) < i64::from(LOSS_PCT) || s.gone(at) {
                        at += rto;
                        rto *= 2;
                        if at > END_MS * 2 {
                            break;
                        }
                    }
                    if at <= END_MS * 2 {
                        let j = s.g.below(jitter);
                        s.push(at + owd + j, Ev::ProbeAtServer(n));
                    }
                }
                if v == Verdict::Dead {
                    if blackhole_at.is_some_and(|b| now >= b) {
                        out.found_at.get_or_insert(now);
                        continue;
                    }
                    out.false_dead += 1;
                    live = fresh(now);
                    probe_at = None;
                }
                schedule!(now);
            }
        }
    }
    out
}

#[test]
fn bp_g9_s2_the_rust_rule_reaches_the_typescript_rules_verdicts_on_the_same_traces() {
    let mut d: u32 = 0x811c_9dc5;
    for (from, blackhole) in [(0u32, false), (1_000_000, true)] {
        for i in 0..64 {
            d = (d ^ trace(from + i, blackhole, Page::Shown).digest).wrapping_mul(0x0100_0193);
        }
    }
    assert_eq!(d, DIGEST, "the Rust rule's verdicts differ from the TypeScript rule's on the same traces");
}

#[test]
fn bp_g9_s2_a_blackholed_link_is_found_dead_never_later_than_the_rules_bound() {
    for page in [Page::Shown, Page::Hidden] {
        for i in 0..200 {
            let o = trace(1_000_000 + i, true, page);
            assert!(o.found_at.is_some(), "found dead");
            if page == Page::Shown {
                assert!(o.found_at <= o.bound, "found at {:?}, bound {:?}", o.found_at, o.bound);
            }
        }
    }
}

/// Goal 2's rule (a silent link given `2^BACKOFF - 1` round trips; a resumed page judging from
/// its resume): no live link judged dead, shown, frozen or hidden, and every blackhole found, a
/// frozen page's too. `LIVENESS_TRACES` sets how many of each (5,000 for the bar).
#[test]
fn bp_g9_s2_a_live_link_is_never_judged_dead() {
    let n: u32 = std::env::var("LIVENESS_TRACES").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
    let mut dead = 0;
    let mut lost = 0;
    for page in [Page::Shown, Page::Frozen, Page::Hidden] {
        dead += (0..n).map(|i| trace(i, false, page).false_dead).sum::<u32>();
        lost += (0..n).filter(|i| trace(1_000_000 + i, true, page).found_at.is_none()).count();
    }
    println!("{} live and {} blackholed traces: {dead} false dead verdicts, {lost} blackholes not found", 3 * n, 3 * n);
    assert_eq!((dead, lost), (0, 0));
}
