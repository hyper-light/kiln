//! A runner's dials to its server and the waits between them, learned from what its own dials did, with no figure of
//! their own (research 01a1b3bbac36, f6, f7, f10, f11; Ada, 2026-11-06). Two DDSketch-style sketches, kept on the
//! machine across its runner's restarts: how long dials that answered took, and how long the server stayed unreachable
//! (from losing it to the start of the dial that answered). A bucket holds the durations of one bit length in
//! nanoseconds, its top twice its bottom: the coarsest log scale, so there's no precision to pick.
//!
//! Patience: a dial is given up once every dial that has answered had answered. Past the handshakes' top bucket the
//! learned hazard of an answer is none, below any rate a redial's cost implies. Once the server has been unreachable
//! longer than it ever was, a dial given up suggests a slower path than any learned, and the next is given the next
//! bucket up, so such a path is still reached (and its time learned). Before any dial has answered, a dial is the
//! transport's to end: its idle timeout.
//!
//! Redials: once the server is lost at outage age `s`, the next dial is due by the first `t` at which the time it has
//! likely been back unnoticed, E[(t - D)+ | D > s], adds up to what that dial costs, its patience. `D` is spread evenly
//! within each outage bucket, with one more outage's share below the shortest seen (the next is as likely to be shorter
//! than all `n` seen as to fall in any of the other `n` places among them); past the longest seen it's scale-free,
//! P(D > u | D > s) = s/u. The dial is drawn uniformly up to then (AWS's full jitter), so runners don't redial in step.
//!
//! What the server says while the link is up overrides that for the next dial (docs/resilience.md R5): over its
//! admission limit, it gives this runner its turn (`ServerMsg::Later`), and the next dial is at a point drawn within it;
//! going down, it says when it expects to answer again and this runner's turn after that (`ServerMsg::Back`), this
//! outage's expected length in place of what this runner learned of others. Either way the jitter is the turn's width,
//! one admission's time on the server, which places each runner in its own turn: the server's, not a figure here.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context as _;

const BUCKETS: usize = u64::BITS as usize + 1;

/// Durations by bit length in nanoseconds: bucket `i` holds `[2^(i-1), 2^i)`, bucket 0 zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sketch([u32; BUCKETS]);

impl Default for Sketch {
    fn default() -> Self {
        Self([0; BUCKETS])
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn bucket(d: Duration) -> usize {
    (u64::BITS - nanos(d).leading_zeros()) as usize
}

/// Bucket `i`'s top.
fn top(i: usize) -> Duration {
    Duration::from_nanos(u32::try_from(i).ok().and_then(|i| 1u64.checked_shl(i)).unwrap_or(u64::MAX))
}

/// The top of the bucket `d` falls in.
fn next_up(d: Duration) -> Duration {
    top(bucket(d))
}

fn ns(d: Duration) -> f64 {
    d.as_nanos() as f64
}

impl Sketch {
    fn add(&mut self, d: Duration) {
        if let Some(n) = self.0.get_mut(bucket(d)) {
            *n = n.saturating_add(1);
        }
    }

    fn count(&self) -> u64 {
        self.0.iter().map(|&n| u64::from(n)).sum()
    }

    /// The top of the highest bucket that holds any.
    fn top(&self) -> Option<Duration> {
        self.0.iter().rposition(|&n| n > 0).map(top)
    }

    /// The CDF this predicts for the next one, in nanoseconds: each bucket's count spread evenly over it, and one
    /// more count's share spread below the lowest that holds any. Given at each bucket's top; linear between; 1 past.
    fn knots(&self) -> impl Iterator<Item = (f64, f64)> + Clone + '_ {
        let n = self.count() as f64 + 1.0;
        let first = self.0.iter().position(|&c| c > 0).unwrap_or(BUCKETS);
        let below = first.checked_sub(1).map_or(Duration::ZERO, top);
        let counts = self.0.iter().enumerate().skip(first);
        std::iter::once((0.0, 0.0)).chain(std::iter::once((ns(below), 1.0 / n))).chain(counts.scan(1.0, move |seen, (i, &c)| {
            *seen += f64::from(c);
            Some((ns(top(i)), *seen / n))
        }))
    }

    fn text(&self) -> String {
        use std::fmt::Write as _;
        self.0.iter().enumerate().filter(|(_, n)| **n > 0).fold(String::new(), |mut out, (i, n)| {
            let _ = write!(out, " {i}:{n}");
            out
        })
    }

    fn read<'a>(words: impl Iterator<Item = &'a str>) -> anyhow::Result<Self> {
        let mut s = Self::default();
        for w in words {
            let (i, n) = w.split_once(':').ok_or_else(|| anyhow::anyhow!("{w:?} isn't <bucket>:<count>"))?;
            let at = s.0.get_mut(i.parse::<usize>().with_context(|| format!("{w:?}"))?).ok_or_else(|| anyhow::anyhow!("no bucket {i}"))?;
            *at = n.parse().with_context(|| format!("{w:?}"))?;
        }
        Ok(s)
    }
}

/// D's CDF at `u`, given at `knots` (ascending, linear between, 1 past the last).
fn cdf(knots: impl Iterator<Item = (f64, f64)>, u: f64) -> f64 {
    let mut before = (0.0, 0.0);
    for (x, f) in knots {
        if x >= u {
            let (a, fa) = before;
            return if x > a { fa + (f - fa) * (u - a) / (x - a) } else { f };
        }
        before = (x, f);
    }
    1.0
}

/// The first `t >= s` by which E[(t - D)+ | D > s] reaches `cost` (nanoseconds), D's CDF given at `knots`; none when
/// D > s can't be.
fn due(knots: impl Iterator<Item = (f64, f64)> + Clone, s: f64, cost: f64) -> Option<f64> {
    let at = cdf(knots.clone(), s);
    let left = 1.0 - at;
    if left <= 0.0 {
        return None;
    }
    // Area under F(u) - F(s) still to cover, the conditional's denominator multiplied out.
    let mut need = cost * left;
    let (mut a, mut ga) = (s, 0.0);
    for (b, f) in knots.filter(|&(x, _)| x > s) {
        let gb = f - at;
        let area = (ga + gb) / 2.0 * (b - a);
        if area >= need {
            // ga y + k y^2 = need over this piece, solved without cancellation.
            let k = (gb - ga) / (2.0 * (b - a));
            return Some(a + 2.0 * need / (ga + (ga * ga + 4.0 * k * need).sqrt()));
        }
        need -= area;
        (a, ga) = (b, gb);
    }
    Some(a + need / left)
}

/// D's CDF given D > `s`, scale-free: P(D > u | D > s) = s/u, at the bucket tops past `s`.
fn scale_free(s: f64) -> impl Iterator<Item = (f64, f64)> + Clone {
    std::iter::once((s, 0.0)).chain((0..BUCKETS).map(|i| ns(top(i))).filter(move |&x| x > s).map(move |x| (x, 1.0 - s / x)))
}

/// Uniform over `[0, d)`, by `draw` (a random `u64`).
fn drawn(d: Duration, draw: u64) -> Duration {
    Duration::from_nanos(u64::try_from((u128::from(nanos(d)) * u128::from(draw)) >> u64::BITS).unwrap_or(u64::MAX))
}

/// What a runner has learned of its dials to its server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Learned {
    handshakes: Sketch,
    outages: Sketch,
}

impl Learned {
    /// What `handshakes` and `outages` teach, for a test.
    #[cfg(test)]
    pub(crate) fn of(handshakes: &[Duration], outages: &[Duration]) -> Self {
        let mut l = Self::default();
        for &d in handshakes {
            l.handshakes.add(d);
        }
        for &d in outages {
            l.outages.add(d);
        }
        l
    }

    /// What a run before kept at `path`; nothing learned when there's none.
    pub(crate) fn kept(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let mut learned = Self::default();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            match words.next() {
                Some("handshakes") => learned.handshakes = Sketch::read(words)?,
                Some("outages") => learned.outages = Sketch::read(words)?,
                _ => {}
            }
        }
        Ok(learned)
    }

    fn text(&self) -> String {
        format!("handshakes{}\noutages{}\n", self.handshakes.text(), self.outages.text())
    }

    /// How long a dial is given to answer.
    pub(crate) fn patience(&self) -> Duration {
        self.handshakes.top().unwrap_or_else(|| crate::net::limits(false).idle_timeout)
    }

    /// With the server unreachable for `s`, how long until a dial that costs `cost` is due at the latest.
    pub(crate) fn wait(&self, s: Duration, cost: Duration) -> Duration {
        let (s, cost) = (ns(s), ns(cost));
        let learned = (self.outages.count() > 0).then(|| due(self.outages.knots(), s, cost)).flatten();
        let t = learned.or_else(|| due(scale_free(s), s, cost)).unwrap_or(s);
        Duration::from_nanos((t - s).max(0.0) as u64)
    }
}

/// The longest a runner that has learned nothing takes to dial its server again once it's lost: a dial left to the
/// transport, then the wait before the next.
pub(crate) fn unlearned_bound() -> Duration {
    let l = Learned::default();
    let p = l.patience();
    p.saturating_add(l.wait(Duration::ZERO, p))
}

/// What the server said, while the link was up, of when to dial it again: from now plus `after`, at a point drawn
/// within `width`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Told {
    /// Over its admission limit, at this turn: it answered, so this is no outage.
    Later(Instant),
    /// Going down, back at about this time (its drain's notice): the outage starts as the link drops.
    Back(Instant),
}

impl Told {
    fn at(self) -> Instant {
        match self {
            Told::Later(at) | Told::Back(at) => at,
        }
    }
}

/// Where the link to the server stands. `down`: since when the server has been unreachable (none when it isn't known
/// to be: the runner just started, or left a link for another path, or the server gave it a turn); `patience`: the
/// dial's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Up { told: Option<Told> },
    Waiting { at: Instant, down: Option<Instant>, patience: Duration },
    Dialing { since: Instant, down: Option<Instant>, patience: Duration },
    /// Given up: the dial's end is on its way from the transport.
    GivingUp { since: Instant, down: Option<Instant>, patience: Duration },
}

/// What the link's deadline asks for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Resolve where to dial, then dial ([`Redials::dialing`]).
    Dial,
    /// End the dial in flight: its end comes back as a failure ([`Redials::failed`]).
    GiveUp,
    Wait,
}

/// A runner's link to its server: when to dial, how long to give a dial, and what that teaches.
pub(crate) struct Redials {
    learned: Learned,
    link: Link,
    /// Where `learned` is kept (none for an ephemeral runner).
    kept: Option<PathBuf>,
}

impl Redials {
    /// Dialing at once.
    pub(crate) fn new(learned: Learned, kept: Option<PathBuf>, now: Instant) -> Self {
        let patience = learned.patience();
        Self { learned, link: Link::Waiting { at: now, down: None, patience }, kept }
    }

    /// The next deadline: the next dial's, or the dial in flight's patience.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self.link {
            Link::Waiting { at, .. } => Some(at),
            Link::Dialing { since, patience, .. } => Some(kiln_seams::clock::later(since, patience)),
            Link::Up { .. } | Link::GivingUp { .. } => None,
        }
    }

    pub(crate) fn on_deadline(&mut self, now: Instant) -> Step {
        match self.link {
            Link::Waiting { at, .. } if at <= now => Step::Dial,
            Link::Dialing { since, down, patience } if kiln_seams::clock::later(since, patience) <= now => {
                self.link = Link::GivingUp { since, down, patience };
                Step::GiveUp
            }
            _ => Step::Wait,
        }
    }

    /// A dial is on its way.
    pub(crate) fn dialing(&mut self, now: Instant) {
        if let Link::Waiting { down, patience, .. } = self.link {
            self.link = Link::Dialing { since: now, down, patience };
        }
    }

    /// The dial answered: what it took is learned, and kept.
    pub(crate) fn connected(&mut self, now: Instant) -> anyhow::Result<()> {
        let (Link::Dialing { since, down, .. } | Link::GivingUp { since, down, .. }) = std::mem::replace(&mut self.link, Link::Up { told: None }) else {
            return Ok(());
        };
        self.learned.handshakes.add(now.saturating_duration_since(since));
        if let Some(down) = down {
            self.learned.outages.add(since.saturating_duration_since(down));
        }
        match &self.kept {
            Some(path) => crate::util::write_secret_unsynced(path, self.learned.text().as_bytes(), 0o644).with_context(|| format!("keep {}", path.display())),
            None => Ok(()),
        }
    }

    /// The server, over its admission limit, gave this runner its turn `after` from now, `width` wide: the next dial
    /// is at a point drawn within it by `draw` (docs/resilience.md R5).
    pub(crate) fn later(&mut self, now: Instant, after: Duration, width: Duration, draw: u64) {
        self.told(Told::Later(kiln_seams::clock::later(now, after.saturating_add(drawn(width, draw)))));
    }

    /// The server is going down and expects to answer again `after` from now, this runner's turn `width` wide: the next
    /// dial is at a point drawn within it by `draw`, and the outage runs from the link's drop.
    pub(crate) fn back(&mut self, now: Instant, after: Duration, width: Duration, draw: u64) {
        self.told(Told::Back(kiln_seams::clock::later(now, after.saturating_add(drawn(width, draw)))));
    }

    fn told(&mut self, told: Told) {
        if let Link::Up { .. } = self.link {
            self.link = Link::Up { told: Some(told) };
        }
    }

    /// The link went down: the dial in flight failed, or the connection dropped. The next dial is drawn by `draw`, or
    /// is when the server said.
    pub(crate) fn failed(&mut self, now: Instant, draw: u64) {
        let (down, patience) = match self.link {
            Link::Waiting { .. } => return,
            Link::Up { told: Some(Told::Later(at)) } => {
                self.link = Link::Waiting { at, down: None, patience: self.learned.patience() };
                return;
            }
            Link::Up { told: Some(Told::Back(at)) } => {
                self.link = Link::Waiting { at, down: Some(now), patience: self.learned.patience() };
                return;
            }
            Link::Up { told: None } => (now, self.learned.patience()),
            Link::Dialing { since, down, patience } => (down.unwrap_or(since), patience),
            Link::GivingUp { since, down, patience } => {
                let down = down.unwrap_or(since);
                let beyond = self.learned.outages.top().is_none_or(|longest| now.saturating_duration_since(down) >= longest);
                (down, if beyond { next_up(patience) } else { patience })
            }
        };
        let wait = self.learned.wait(now.saturating_duration_since(down), patience);
        self.link = Link::Waiting { at: kiln_seams::clock::later(now, drawn(wait, draw)), down: Some(down), patience };
    }

    /// Dial at once, another way: the link that was up is left, or the dial in flight forgotten; but not before the
    /// server said.
    pub(crate) fn switch(&mut self, now: Instant) {
        self.link = match self.link {
            Link::Up { told } => Link::Waiting { at: told.map_or(now, Told::at), down: None, patience: self.learned.patience() },
            Link::Waiting { down, patience, .. } | Link::Dialing { down, patience, .. } | Link::GivingUp { down, patience, .. } => Link::Waiting { at: now, down, patience },
        };
    }

    /// No dial before `until`.
    pub(crate) fn hold(&mut self, until: Instant) {
        if let Link::Waiting { at, .. } = &mut self.link {
            *at = (*at).max(until);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const S: Duration = Duration::from_secs(1);

    fn sketch(of: &[Duration]) -> Sketch {
        let mut s = Sketch::default();
        for &d in of {
            s.add(d);
        }
        s
    }

    fn learned(handshakes: &[Duration], outages: &[Duration]) -> Learned {
        Learned { handshakes: sketch(handshakes), outages: sketch(outages) }
    }

    /// Equal but for the float's last place and the cast to whole nanoseconds.
    fn close(a: Duration, b: Duration) -> bool {
        a.abs_diff(b) <= Duration::from_nanos(2)
    }

    /// Research 01a1b3bbac36 f6: a dial's patience is what this runner's dials that answered took, not a figure: none
    /// learned, the transport's own end of a dial; then the top of the bucket of the slowest that answered. A dial is
    /// given up at its patience and not before; given up past every outage learned, the next dial gets the next bucket,
    /// and within them (the server down explains it) the same.
    #[test]
    fn redial_patience_follows_measured_handshakes() {
        assert_eq!(Learned::default().patience(), crate::net::limits(false).idle_timeout, "nothing learned: the transport's");
        let lan = learned(&[3 * MS, 2 * MS, 3 * MS], &[]);
        assert_eq!(lan.patience(), Duration::from_nanos(1 << 22), "3 ms answers: 4.19 ms");
        let wan = learned(&[3 * MS, 2 * MS, 300 * MS], &[]);
        assert_eq!(wan.patience(), Duration::from_nanos(1 << 29), "one at 300 ms: 537 ms");

        let t0 = Instant::now();
        let mut r = Redials::new(lan, None, t0);
        assert_eq!(r.on_deadline(t0), Step::Dial);
        r.dialing(t0);
        let patience = Duration::from_nanos(1 << 22);
        assert_eq!(r.deadline(), Some(t0 + patience));
        assert_eq!(r.on_deadline(t0 + patience - Duration::from_nanos(1)), Step::Wait, "not before its patience");
        assert_eq!(r.on_deadline(t0 + patience), Step::GiveUp);
        assert_eq!(r.deadline(), None, "given up: waiting for the dial's end");
        // No outage learned: past every one, so the next dial is given the next bucket.
        r.failed(t0 + patience, 0);
        assert_eq!(r.link, Link::Waiting { at: t0 + patience, down: Some(t0), patience: 2 * patience });

        // Outages of 30 s learned: a dial given up 1 s in is the server being down, and the next has the same patience.
        let mut r = Redials::new(learned(&[3 * MS], &[30 * S]), None, t0);
        r.dialing(t0);
        r.failed(t0, 0);
        let at = t0 + S;
        r.switch(at);
        r.dialing(at);
        assert_eq!(r.on_deadline(at + patience), Step::GiveUp);
        r.failed(at + patience, 0);
        assert!(matches!(r.link, Link::Waiting { patience: p, .. } if p == patience), "{:?}", r.link);
        // A dial that answers after its patience teaches the slower time.
        r.dialing(at + S);
        assert_eq!(r.on_deadline(at + S + patience), Step::GiveUp);
        assert!(r.connected(at + S + 20 * MS).is_ok());
        assert_eq!(r.learned.patience(), Duration::from_nanos(1 << 25), "a 20 ms answer: 33.6 ms");
        assert_eq!(r.learned.outages, sketch(&[30 * S, 2 * S]), "the outage, from the first failed dial to the one that answered");
    }

    /// Research 01a1b3bbac36 f7, f11: the waits come from how long this server stayed unreachable. Early on, the one
    /// share below the shortest outage seen is all that's due, so the first dial is due by sqrt(2 (n+1) l c) (l that
    /// bucket's bottom, c a dial's cost); within the outages seen, dials come much closer; the same counts an order
    /// of magnitude shorter bring the first dial in; past the longest seen, waits grow as the root of the age.
    #[test]
    fn redial_waits_follow_the_learned_outages() {
        let cost = Duration::from_nanos(1 << 22);
        let restarts = learned(&[3 * MS], &[30 * S; 20]);
        let below = 2f64.powi(34);
        let first = Duration::from_nanos((2.0 * 21.0 * below * ns(cost)).sqrt() as u64);
        assert!(close(restarts.wait(Duration::ZERO, cost), first), "{:?} vs {first:?}", restarts.wait(Duration::ZERO, cost));
        let inside = restarts.wait(25 * S, cost);
        assert!(inside < first / 4, "within the outages seen: {inside:?}");
        let quick = learned(&[3 * MS], &[3 * S; 20]);
        assert!(quick.wait(Duration::ZERO, cost) < first / 2, "{:?}", quick.wait(Duration::ZERO, cost));
        assert!(quick.wait(Duration::ZERO, cost) > Duration::ZERO);
        // Past the longest: scale-free, sublinear in the age.
        let (a, b) = (restarts.wait(100 * S, cost), restarts.wait(1600 * S, cost));
        assert!(b > a && b < 16 * a && b > 2 * a, "{a:?} then {b:?}");
        assert!(b < 1600 * S / 100, "{b:?}");
        // Nothing learned: as scale-free from the start: the first dial due within one dial's cost.
        assert!(close(Learned::default().wait(Duration::ZERO, cost), cost));
    }

    /// AWS's full jitter (research f7): each dial is drawn uniformly up to its due time, so runners that lost the
    /// server together don't dial it together.
    #[test]
    fn redial_keeps_full_jitter() {
        let wait = 10 * S;
        assert_eq!(drawn(wait, 0), Duration::ZERO);
        assert_eq!(drawn(wait, 1 << 63), wait / 2);
        assert_eq!(drawn(wait, u64::MAX), wait - Duration::from_nanos(1));
        let t0 = Instant::now();
        let restarts = learned(&[3 * MS], &[30 * S; 20]);
        let due = restarts.wait(Duration::ZERO, restarts.patience());
        let ats: Vec<Instant> = [0, u64::MAX / 3, u64::MAX / 3 * 2, u64::MAX]
            .iter()
            .map(|&draw| {
                let mut r = Redials::new(restarts, None, t0);
                r.dialing(t0);
                assert!(r.connected(t0 + 3 * MS).is_ok());
                r.failed(t0 + S, draw);
                r.deadline().unwrap_or(t0)
            })
            .collect();
        assert!(ats.windows(2).all(|w| matches!(w, [a, b] if a < b)), "spread, not in step: {ats:?}");
        assert!(ats.iter().all(|&at| at >= t0 + S && at < t0 + S + due), "{ats:?} within {due:?}");
    }

    /// No figure of the schedule's own: the same history twice as slow (every handshake and outage a bucket up) gives
    /// exactly twice the patience and twice every wait. A first wait, a cap or a step of its own would break that.
    /// The server's grace before it has learned anything follows from the same code (`crate::comeback::prior`).
    #[test]
    fn redial_no_picked_constant_left() {
        let (h, o) = ([2 * MS, 3 * MS, 40 * MS], [S / 2, 20 * S, 30 * S, 31 * S, 400 * S]);
        let base = learned(&h, &o);
        for k in 1..8u32 {
            let scaled = learned(&h.map(|d| d * (1 << k)), &o.map(|d| d * (1 << k)));
            assert_eq!(scaled.patience(), base.patience() * (1 << k));
            for s in [Duration::ZERO, S, 10 * S, 25 * S, 100 * S, 3600 * S] {
                let (a, b) = (base.wait(s, base.patience()), scaled.wait(s * (1 << k), scaled.patience()));
                assert!(b.abs_diff(a * (1 << k)) < Duration::from_nanos(1 << k), "k {k}, s {s:?}: {a:?} then {b:?}");
            }
        }
        assert_eq!(crate::comeback::prior(), unlearned_bound());
        assert_eq!(unlearned_bound(), Learned::default().patience() + Learned::default().wait(Duration::ZERO, Learned::default().patience()));
    }

    /// The coordinator's bar for redials: a dial's outcome is all the work, and it allocates nothing (the sketches are
    /// fixed arrays, read in place); only a connection, which keeps what it taught, writes.
    #[test]
    fn redial_a_dials_outcome_allocates_nothing() {
        let t0 = Instant::now();
        let mut r = Redials::new(learned(&[3 * MS, 40 * MS], &[S / 2, 20 * S, 30 * S]), None, t0);
        let before = crate::os::counted::made();
        let mut now = t0;
        for draw in [0, u64::MAX / 2, u64::MAX] {
            now = r.deadline().unwrap_or(now).max(now);
            assert_eq!(r.on_deadline(now), Step::Dial);
            r.dialing(now);
            now = r.deadline().unwrap_or(now);
            assert_eq!(r.on_deadline(now), Step::GiveUp);
            r.failed(now, draw);
        }
        r.switch(now);
        r.hold(now + S);
        assert_eq!(crate::os::counted::made(), before, "three dials given up, a switch and a hold");
    }

    /// R5: the server over its admission limit gives this runner its turn: the next dial is at a point drawn within
    /// it (a turn's width at most), whatever the learned redials say, and it's no outage (the server answered): the
    /// dial at its turn teaches a handshake and no outage. A switch of path waits for the turn too.
    #[test]
    fn r5_a_turn_is_the_next_dial_drawn_within_it() {
        let t0 = Instant::now();
        let restarts = learned(&[3 * MS], &[30 * S; 20]);
        let (after, width) = (700 * MS, 4 * MS);
        let ats: Vec<Instant> = [0, 1 << 63, u64::MAX]
            .iter()
            .map(|&draw| {
                let mut r = Redials::new(restarts, None, t0);
                r.dialing(t0);
                assert!(r.connected(t0 + 3 * MS).is_ok());
                r.later(t0 + 4 * MS, after, width, draw);
                assert_eq!(r.deadline(), None, "still up until the link drops");
                r.failed(t0 + 5 * MS, 0);
                assert_eq!(r.on_deadline(t0 + 4 * MS + after - Duration::from_nanos(1)), Step::Wait, "not before its turn");
                r.deadline().unwrap_or(t0)
            })
            .collect();
        let turn = t0 + 4 * MS + after;
        assert_eq!(ats.first(), Some(&turn));
        assert_eq!(ats.get(1), Some(&(turn + width / 2)));
        assert!(ats.iter().all(|&at| at >= turn && at < turn + width), "{ats:?} within its turn");
        let mut r = Redials::new(restarts, None, t0);
        r.dialing(t0);
        assert!(r.connected(t0 + 3 * MS).is_ok());
        r.later(t0 + 4 * MS, after, width, 0);
        r.failed(t0 + 5 * MS, 0);
        assert_eq!(r.on_deadline(turn), Step::Dial);
        r.dialing(turn);
        assert!(r.connected(turn + 2 * MS).is_ok());
        assert_eq!(r.learned.outages, restarts.outages, "a turn isn't an outage");
        let mut s = Redials::new(restarts, None, t0);
        s.dialing(t0);
        assert!(s.connected(t0 + 3 * MS).is_ok());
        s.later(t0 + 4 * MS, after, width, 0);
        s.switch(t0 + 5 * MS);
        assert_eq!(s.deadline(), Some(turn), "another path, but at its turn");
    }

    /// R5, docs/resilience.md C3 step 5: a drained server's notice moves this runner's first dial to the server's
    /// expected restart (plus its turn), where its own redials, learned from outages of 30 s, would have dialed within
    /// a second or two of the drop; the outage runs from the drop, so a dial at the notice that answers teaches it.
    #[test]
    fn r5_a_drained_servers_notice_moves_the_first_dial_to_its_restart() {
        let t0 = Instant::now();
        let restarts = learned(&[3 * MS], &[30 * S; 20]);
        let own = restarts.wait(Duration::ZERO, restarts.patience());
        assert!(own < 2 * S, "its own first dial is due by {own:?}");
        let (restart, turn) = (12 * S, 3 * MS);
        let mut r = Redials::new(restarts, None, t0);
        r.dialing(t0);
        assert!(r.connected(t0 + 3 * MS).is_ok());
        r.back(t0 + S, restart + 5 * turn, turn, 1 << 63);
        r.failed(t0 + S + MS, 0);
        let first = t0 + S + restart + 5 * turn + turn / 2;
        assert_eq!(r.deadline(), Some(first), "the first dial at the restart, its turn after it");
        assert_eq!(r.on_deadline(first - Duration::from_nanos(1)), Step::Wait);
        assert_eq!(r.on_deadline(first), Step::Dial);
        r.dialing(first);
        assert!(r.connected(first + 3 * MS).is_ok());
        assert_eq!(r.learned.outages, sketch(&[30 * S; 20].into_iter().chain([first - (t0 + S + MS)]).collect::<Vec<_>>()), "the outage from the drop");
        // Its dial at the notice finding the server still down: its own redials from there, the outage that old.
        let mut late = Redials::new(restarts, None, t0);
        late.dialing(t0);
        assert!(late.connected(t0 + 3 * MS).is_ok());
        late.back(t0 + S, restart, Duration::ZERO, 0);
        late.failed(t0 + S, 0);
        let at = t0 + S + restart;
        assert_eq!(late.on_deadline(at), Step::Dial);
        late.dialing(at);
        late.failed(at + MS, u64::MAX);
        let next = late.deadline().unwrap_or(at);
        assert!(next > at + MS && next <= at + MS + restarts.wait(restart + MS, restarts.patience()), "{next:?}");
    }

    /// What's learned is kept as text a restart reads back; a text that doesn't read is an error, never a guess.
    #[test]
    fn redial_what_is_learned_is_kept() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("redials");
        assert_eq!(Learned::kept(&path)?, Learned::default(), "none kept: nothing learned");
        let t0 = Instant::now();
        let mut r = Redials::new(Learned::default(), Some(path.clone()), t0);
        r.dialing(t0);
        r.failed(t0 + S, 0);
        r.switch(t0 + 2 * S);
        r.dialing(t0 + 2 * S);
        r.connected(t0 + 2 * S + 3 * MS)?;
        assert_eq!(Learned::kept(&path)?, r.learned);
        assert_eq!(r.learned, learned(&[3 * MS], &[2 * S]));
        std::fs::write(&path, "handshakes 22:x\n")?;
        anyhow::ensure!(Learned::kept(&path).is_err(), "a bad count is an error");
        std::fs::write(&path, "handshakes 99:1\n")?;
        anyhow::ensure!(Learned::kept(&path).is_err(), "no such bucket");
        Ok(())
    }
}
