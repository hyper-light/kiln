//! A node's writes into the group (`docs/observe.md` §8): what kiln itself, its runners' hosts and
//! OpenTelemetry give this node, gathered by series and sent as one write at a time, each sent again
//! until it is answered.
//!
//! One write out at a time, as Ongaro's client sessions assume (thesis §6.3): its sender (this
//! process, a random id each start) and its place make a write sent twice one write, so a write is
//! sent again as it was until an answer comes, and the next is built only then. Points that come
//! meanwhile wait by series, which also merges their many small writes into one; a series is
//! defined in a write until a write of this sender that defined it is applied, and again a while
//! after, so that a series retention took is defined anew before its next point. A series the
//! group says it does not know has its points of that write sent again, with its definition.
//!
//! Gathering follows the OpenTelemetry Collector's batch processor: a write goes once its first
//! point has waited [`FLUSH`], or once it is full (opentelemetry-collector
//! `processor/batchprocessor`, `timeout` 200ms; full here is the group's entry, not a count).
//!
//! Series go into writes oldest first (the series whose first waiting point came first): when more
//! waits than a write holds, no series waits behind others for longer than the queue's delay. That
//! delay is what admission controls, as CoDel controls a queue (Nichols and Jacobson, "Controlling
//! Queue Delay", 2012; RFC 8289): while the oldest point's wait stays above [`TARGET`] for an
//! interval, exporters are told to come back (503) rather than queued behind it.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use kiln_obs::batch::{Builder, SeriesKey};
use kiln_obs::logs::record::Record;

use super::wire::Outcome;

/// How long the first point of a write waits for more.
pub const FLUSH: Duration = Duration::from_millis(200);
/// The wait a queue may keep: five flushes. Under it a point is in a write within a second of
/// coming; past it for a whole [`INTERVAL`], exporters' points are refused until it drains (they
/// hold them better: an exporter retries a refused batch after seconds anyway).
pub const TARGET: Duration = Duration::from_secs(1);
/// What a queue's wait is judged over: its least within this long (CoDel's interval), so a burst
/// that drains in time is no reason to refuse.
pub const INTERVAL: Duration = Duration::from_secs(1);
/// A write not answered by then is sent again, as it was.
pub const RESEND: Duration = Duration::from_secs(5);
/// How long a series counts as defined: past it, the next write of it defines it again.
const DEFINED_FOR: Duration = Duration::from_secs(3600);
/// Series counted as defined at the most; past it, all are defined again (a definition is some
/// 50 bytes, and this many is 32 MiB of map).
const KNOWN: usize = 1 << 20;
/// What a write's bytes may come to: the group's entry less its command.
const ENTRY_BUDGET: usize = kiln_obs::group::ENTRY_BYTES - kiln_obs::command::WRITE_HEADER;
/// Points waiting, at the most, and log bytes. The node takes no more than this to write
/// (`super::owner::Observe::offer`, which answers 503 past three quarters of it, the rest kept for
/// kiln's own figures): what is taken is never dropped for room. Past it a write would be dropped
/// and counted, which admission keeps from happening.
pub const WAITING_POINTS: usize = 4 << 20;
pub const WAITING_LOG_BYTES: usize = 64 << 20;

/// A write out: its place, its entry, when it was sent, and what it holds of each series (its
/// points, a range of `points`, to send again with their definitions if the group does not know
/// them). Kept, buffers and all, for the next write once answered.
#[derive(Default)]
struct Out {
    seq: u64,
    entry: Vec<u8>,
    sent: Option<Instant>,
    series: Vec<(SeriesKey, std::ops::Range<usize>)>,
    points: Vec<(i64, f64)>,
    defines: Vec<u64>,
}

/// A buffer kept for reuse past what it holds, halved when it holds a quarter of its room: a burst
/// passes and its memory with it.
fn settle<T>(v: &mut Vec<T>) {
    if v.capacity() > 1 << 16 && v.len() < v.capacity() / 4 {
        v.shrink_to(v.capacity() / 2);
    }
}

pub struct Batcher {
    sender: u64,
    seq: u64,
    builder: Builder,
    /// The series points wait for, oldest first: each with how many wait, and since when the first
    /// of them; by fingerprint, their places here.
    slots: Vec<(SeriesKey, u32, Instant)>,
    by_fp: HashMap<u64, u32>,
    /// Points waiting, each with its series' place, as they came: one buffer, kept.
    points: Vec<(u32, i64, f64)>,
    waiting_points: usize,
    logs: VecDeque<Record>,
    log_bytes: usize,
    first_waiting: Option<Instant>,
    out: Option<Out>,
    /// The last write answered: its buffers, for the next.
    spare: Out,
    /// Reused to cut a write: where each series' points start, and next go; the points sorted by
    /// series.
    starts: Vec<u32>,
    sorted: Vec<(i64, f64)>,
    /// Reused to place a request's series.
    places: Vec<u32>,
    /// The queue's least wait in this interval, and when the interval began; the last interval's.
    least: Option<Duration>,
    interval_from: Option<Instant>,
    judged: Duration,
    /// Series a write of this sender defined, and when it was applied.
    known: HashMap<u64, Instant>,
    pub dropped: u64,
    pub written: u64,
    /// Samples and records the group dropped as too far ahead of their write's time.
    pub ahead: u64,
    /// Points and log bytes that have left what waits (cut into a write, or dropped), less those
    /// that came back to wait again: what admission's bound is released by.
    left_points: u64,
    left_log_bytes: u64,
}

/// A record's weight against [`WAITING_LOG_BYTES`].
pub fn record_bytes(r: &Record) -> usize {
    32 + r.service.len() + r.host.len() + r.source.len() + r.message.len() + r.attrs.iter().map(|(k, v)| k.len() + v.len() + 4).sum::<usize>()
}

impl Batcher {
    pub fn new(sender: u64) -> Self {
        Self {
            sender,
            seq: 0,
            builder: Builder::new(),
            slots: Vec::new(),
            by_fp: HashMap::new(),
            points: Vec::new(),
            waiting_points: 0,
            logs: VecDeque::new(),
            log_bytes: 0,
            first_waiting: None,
            out: None,
            spare: Out::default(),
            starts: Vec::new(),
            sorted: Vec::new(),
            places: Vec::new(),
            least: None,
            interval_from: None,
            judged: Duration::ZERO,
            known: HashMap::new(),
            dropped: 0,
            written: 0,
            ahead: 0,
            left_points: 0,
            left_log_bytes: 0,
        }
    }

    pub fn sender(&self) -> u64 {
        self.sender
    }

    /// The place of `key`'s series in `slots`, made (last: the youngest) if new.
    fn slot(&mut self, key: SeriesKey, now: Instant) -> u32 {
        if let Some(&i) = self.by_fp.get(&key.fp) {
            return i;
        }
        let i = u32::try_from(self.slots.len()).unwrap_or(u32::MAX);
        self.by_fp.insert(key.fp, i);
        self.slots.push((key, 0, now));
        i
    }

    /// How long the oldest waiting point has waited.
    pub fn delay(&self, now: Instant) -> Duration {
        let series = self.slots.first().map(|s| s.2);
        let logs = if self.logs.is_empty() { None } else { self.first_waiting };
        series.into_iter().chain(logs).min().map_or(Duration::ZERO, |t| now.saturating_duration_since(t))
    }

    /// Whether the queue has kept more than [`TARGET`] of wait for a whole [`INTERVAL`]: its least
    /// wait over the last interval, judged as time goes (each call one look).
    pub fn congested(&mut self, now: Instant) -> bool {
        let d = self.delay(now);
        self.least = Some(self.least.map_or(d, |l| l.min(d)));
        let from = *self.interval_from.get_or_insert(now);
        if now.saturating_duration_since(from) >= INTERVAL {
            self.judged = self.least.unwrap_or_default();
            self.least = None;
            self.interval_from = Some(now);
        }
        self.judged > TARGET
    }

    /// Points of series, waiting for the next write.
    pub fn series(&mut self, series: Vec<(SeriesKey, Vec<(i64, f64)>)>, now: Instant) {
        for (key, points) in series {
            if points.is_empty() {
                continue;
            }
            if self.waiting_points + points.len() > WAITING_POINTS {
                self.dropped += points.len() as u64;
                self.left_points += points.len() as u64;
                continue;
            }
            let slot = self.slot(key, now);
            self.take(slot, points.iter().copied());
            self.first_waiting.get_or_insert(now);
        }
    }

    /// Samples as a request decoded them (`kiln_obs::otlp::metrics::Decoded`): each with its
    /// series' place in `keys` (taken, the list's room left), waiting for the next write.
    pub fn samples(&mut self, keys: &mut Vec<SeriesKey>, samples: &[(u32, i64, f64)], now: Instant) {
        if samples.is_empty() {
            return;
        }
        if self.waiting_points + samples.len() > WAITING_POINTS {
            self.dropped += samples.len() as u64;
            self.left_points += samples.len() as u64;
            return;
        }
        // Each key's place here, by its place there.
        let mut places = std::mem::take(&mut self.places);
        places.clear();
        places.extend(keys.drain(..).map(|k| self.slot(k, now)));
        for &(i, ts, v) in samples {
            let Some(&slot) = places.get(i as usize) else { continue };
            self.push(slot, ts, v);
        }
        self.places = places;
        self.first_waiting.get_or_insert(now);
    }

    fn take(&mut self, slot: u32, points: impl Iterator<Item = (i64, f64)>) {
        for (ts, v) in points {
            self.push(slot, ts, v);
        }
    }

    fn push(&mut self, slot: u32, ts: i64, v: f64) {
        if let Some(s) = self.slots.get_mut(slot as usize) {
            s.1 += 1;
            self.points.push((slot, ts, v));
            self.waiting_points += 1;
        }
    }

    pub fn logs(&mut self, records: Vec<Record>, now: Instant) {
        for r in records {
            let n = record_bytes(&r);
            if self.log_bytes + n > WAITING_LOG_BYTES {
                self.dropped += 1;
                self.left_log_bytes += n as u64;
                continue;
            }
            self.log_bytes += n;
            self.logs.push_back(r);
            self.first_waiting.get_or_insert(now);
        }
    }

    /// Points and log bytes that have left what waits: see `left_points`.
    pub fn left(&self) -> (u64, u64) {
        (self.left_points, self.left_log_bytes)
    }

    /// Points and records waiting, and whether a write is out.
    pub fn backlog(&self) -> (u64, bool) {
        ((self.waiting_points + self.logs.len()) as u64, self.out.is_some())
    }

    /// When [`Batcher::poll`] next has a write to give: the write out's resend, or the flush of
    /// what waits (at once when it fills a write); none when nothing waits.
    pub fn next_due(&self) -> Option<Instant> {
        if let Some(out) = &self.out {
            return Some(out.sent.map_or_else(Instant::now, |t| t + RESEND));
        }
        let first = self.first_waiting?;
        let full = self.waiting_points * 16 + self.log_bytes >= ENTRY_BUDGET;
        Some(if full { first } else { first + FLUSH })
    }

    /// The write to send now, if any: the one out again once [`RESEND`] passed since it went, or
    /// the next once the last was answered and points have waited [`FLUSH`] (or fill a write).
    pub fn poll(&mut self, now: Instant, now_ms: i64) -> Option<(u64, Vec<u8>)> {
        if let Some(out) = &mut self.out {
            if out.sent.is_some_and(|t| now.duration_since(t) < RESEND) {
                return None;
            }
            out.sent = Some(now);
            return Some((out.seq, out.entry.clone()));
        }
        let first = self.first_waiting?;
        let full = self.waiting_points * 16 + self.log_bytes >= ENTRY_BUDGET;
        if now.duration_since(first) < FLUSH && !full {
            return None;
        }
        self.build(now, now_ms);
        let out = self.out.as_mut()?;
        out.sent = Some(now);
        Some((out.seq, out.entry.clone()))
    }

    /// The next write from what waits, as much as one entry holds; the rest waits on.
    fn build(&mut self, now: Instant, now_ms: i64) {
        let mut out = std::mem::take(&mut self.spare);
        out.series.clear();
        out.points.clear();
        out.defines.clear();
        // Series oldest first (their places are in that order), each one's points in the order
        // they came: a counting sort.
        self.starts.clear();
        let mut at = 0u32;
        for s in &self.slots {
            self.starts.push(at);
            at += s.1;
        }
        self.sorted.clear();
        self.sorted.resize(self.points.len(), (0, 0.0));
        for &(slot, ts, v) in &self.points {
            if let Some(st) = self.starts.get_mut(slot as usize) {
                if let Some(p) = self.sorted.get_mut(*st as usize) {
                    *p = (ts, v);
                }
                *st += 1;
            }
        }
        // Series while the entry has room; one too big for what is left goes in part. A sample
        // takes at most 16 bytes in a run (two 64-bit words, Gorilla's worst).
        let mut start = 0usize;
        let mut taken_to = 0usize;
        for (key, n, _) in &self.slots {
            let n = *n as usize;
            if n == 0 {
                continue;
            }
            if self.builder.byte_len() >= ENTRY_BUDGET / 2 {
                break;
            }
            let room = ((ENTRY_BUDGET / 2).saturating_sub(self.builder.byte_len()) / 16).max(1);
            let take = n.min(room);
            let points = self.sorted.get(start..start + take).unwrap_or_default();
            let defined = self.known.get(&key.fp).is_some_and(|t| now.duration_since(*t) < DEFINED_FOR);
            if !defined {
                self.builder.define(key);
                out.defines.push(key.fp);
            }
            self.builder.run(key.fp, points.iter().copied());
            out.series.push((key.clone(), out.points.len()..out.points.len() + take));
            out.points.extend_from_slice(points);
            taken_to = start + take;
            start += n;
            if take < n {
                break;
            }
        }
        let taken = out.points.len();
        self.waiting_points -= taken;
        self.left_points += taken as u64;
        // What was not taken waits on, oldest first still: the rest of a series cut in part, then
        // every series after it, each since when it was. Places start again with them.
        self.points.clear();
        let mut kept: Vec<(SeriesKey, u32, Instant)> = Vec::new();
        if taken_to < self.sorted.len() {
            let mut at = 0usize;
            for (key, n, since) in &self.slots {
                let (from, to) = (at, at + *n as usize);
                at = to;
                let from = from.max(taken_to);
                if from >= to {
                    continue;
                }
                let place = kept.len() as u32;
                kept.push((key.clone(), (to - from) as u32, *since));
                for &(ts, v) in self.sorted.get(from..to).unwrap_or_default() {
                    self.points.push((place, ts, v));
                }
            }
        }
        self.slots.clear();
        self.by_fp.clear();
        for (key, n, since) in kept {
            self.by_fp.insert(key.fp, self.slots.len() as u32);
            self.slots.push((key, n, since));
        }
        settle(&mut self.points);
        settle(&mut self.sorted);
        settle(&mut self.slots);
        // Log records, as many as fit: LZ4's worst case is a little over its input (the block
        // format's bound, n + n/255 + 16), so rows of the room left fit whatever they compress to.
        let room = ENTRY_BUDGET.saturating_sub(self.builder.byte_len()) * 255 / 256;
        let mut take = Vec::new();
        let mut bytes = 0usize;
        while let Some(r) = self.logs.front() {
            let n = record_bytes(r);
            if !take.is_empty() && bytes + n > room.saturating_sub(64) {
                break;
            }
            bytes += n;
            if let Some(r) = self.logs.pop_front() {
                self.log_bytes = self.log_bytes.saturating_sub(n);
                self.left_log_bytes += n as u64;
                take.push(r);
            }
        }
        if !take.is_empty() {
            self.builder.logs(&take);
        }
        if self.points.is_empty() && self.logs.is_empty() {
            self.first_waiting = None;
            self.waiting_points = 0;
        } else {
            self.first_waiting = Some(now);
        }
        if self.builder.items() == 0 {
            self.spare = out;
            return;
        }
        self.seq += 1;
        out.seq = self.seq;
        out.entry = self.builder.finish_write(self.sender, self.seq, now_ms);
        out.sent = None;
        self.out = Some(out);
    }

    /// What write `seq` came to. A series the group does not know waits again, its points to be
    /// sent with its definition; a write applied (now or before) is done.
    pub fn answered(&mut self, seq: u64, outcome: &Outcome, now: Instant) {
        let Some(mut out) = self.out.take_if(|o| o.seq == seq) else { return };
        if outcome.applied {
            self.written += 1;
            if self.known.len() + out.defines.len() > KNOWN {
                self.known.clear();
            }
            for fp in &out.defines {
                if !outcome.unknown.contains(fp) && !outcome.refused.contains(fp) && !outcome.over_limit.contains(fp) {
                    self.known.insert(*fp, now);
                }
            }
        }
        if !outcome.unknown.is_empty() {
            for fp in &outcome.unknown {
                self.known.remove(fp);
            }
            // They wait again: their room is taken again.
            let mut back = 0u64;
            for (key, range) in &out.series {
                if outcome.unknown.contains(&key.fp) {
                    let points = out.points.get(range.clone()).unwrap_or_default();
                    back += points.len() as u64;
                    let slot = self.slot(key.clone(), now);
                    self.take(slot, points.iter().copied());
                    self.first_waiting.get_or_insert(now);
                }
            }
            self.left_points = self.left_points.saturating_sub(back);
        }
        // Refused and past the bound: those points go, counted; so do those too far ahead.
        self.dropped += (outcome.refused.len() + outcome.over_limit.len()) as u64;
        self.ahead += outcome.ahead;
        // Its buffers for the next write (its entry went to whoever sent it).
        out.entry = Vec::new();
        settle(&mut out.points);
        self.spare = out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_obs::batch::Kind;
    use kiln_obs::command::{self, Command};
    use kiln_obs::hash::Keyed;
    use kiln_obs::store::{Answer, Store};

    fn key(h: &str) -> anyhow::Result<SeriesKey> {
        SeriesKey::new("m", [("host", h)], Kind::Gauge).map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn applied(store: &mut Store, index: u64, entry: &[u8]) -> anyhow::Result<Outcome> {
        match store.apply_entry(index, entry) {
            Ok(Answer::Write { outcome, .. }) => Ok(Outcome::of(&outcome)),
            other => anyhow::bail!("not a write's answer: {other:?}"),
        }
    }

    #[test]
    fn points_wait_for_the_flush_then_go_as_one_write_sent_again_until_answered() -> anyhow::Result<()> {
        let mut b = Batcher::new(7);
        let t0 = Instant::now();
        b.series(vec![(key("a")?, vec![(1, 1.0), (2, 2.0)])], t0);
        b.series(vec![(key("a")?, vec![(3, 3.0)]), (key("b")?, vec![(1, 9.0)])], t0);
        anyhow::ensure!(b.poll(t0, 0).is_none(), "the first point waits for more");
        let (seq, entry) = b.poll(t0 + FLUSH, 0).ok_or_else(|| anyhow::anyhow!("a write after the flush"))?;
        anyhow::ensure!(seq == 1);
        anyhow::ensure!(b.poll(t0 + FLUSH, 0).is_none(), "one write out at a time");
        let (again, same) = b.poll(t0 + FLUSH + RESEND, 0).ok_or_else(|| anyhow::anyhow!("sent again"))?;
        anyhow::ensure!(again == 1 && same == entry);
        let mut store = Store::new(Keyed::new());
        let o = applied(&mut store, 1, &entry)?;
        anyhow::ensure!(store.series_count() == 2);
        // Sent twice, applied once: the second is a duplicate.
        anyhow::ensure!(applied(&mut store, 2, &same)?.duplicate);
        b.answered(1, &o, t0);
        anyhow::ensure!(b.backlog() == (0, false));
        // The next write of the same series defines it no more.
        b.series(vec![(key("a")?, vec![(4, 4.0)])], t0);
        let (_, entry) = b.poll(t0 + FLUSH * 2, 0).ok_or_else(|| anyhow::anyhow!("the next"))?;
        let Ok(Command::Write { batch, .. }) = command::decode(&entry) else { anyhow::bail!("not a write") };
        let mut r = kiln_obs::batch::Reader::new(batch)?;
        let mut defs = 0;
        while let Some(item) = r.next_item()? {
            defs += usize::from(matches!(item, kiln_obs::batch::Item::Def { .. }));
        }
        anyhow::ensure!(defs == 0, "{defs} definitions");
        Ok(())
    }

    #[test]
    fn a_series_the_group_does_not_know_is_sent_again_with_its_definition() -> anyhow::Result<()> {
        let mut b = Batcher::new(7);
        let t0 = Instant::now();
        b.series(vec![(key("a")?, vec![(1, 1.0)])], t0);
        let (_, first) = b.poll(t0 + FLUSH, 0).ok_or_else(|| anyhow::anyhow!("a write"))?;
        let mut store = Store::new(Keyed::new());
        let o = applied(&mut store, 1, &first)?;
        b.answered(1, &o, t0);
        // The group lost the series (another group, or retention took it): its points come back.
        let mut fresh = Store::new(Keyed::new());
        b.series(vec![(key("a")?, vec![(2, 2.0)])], t0);
        let (seq, second) = b.poll(t0 + FLUSH * 2, 0).ok_or_else(|| anyhow::anyhow!("a write"))?;
        let o = applied(&mut fresh, 1, &second)?;
        anyhow::ensure!(o.unknown == vec![key("a")?.fp]);
        b.answered(seq, &o, t0);
        let (_, third) = b.poll(t0 + FLUSH * 3, 0).ok_or_else(|| anyhow::anyhow!("the points again"))?;
        let o = applied(&mut fresh, 2, &third)?;
        anyhow::ensure!(o.applied && o.unknown.is_empty());
        let mut ids = Vec::new();
        fresh.select(&kiln_obs::index::Selector::metric("m"), &mut ids);
        let id = *ids.first().ok_or_else(|| anyhow::anyhow!("the series"))?;
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        fresh.read(id, i64::MIN, i64::MAX, &mut ts, &mut vs)?;
        anyhow::ensure!(ts == vec![2], "{ts:?}");
        Ok(())
    }

    /// Every write the batcher makes of `b`'s waiting points, flushed and answered as applied.
    fn writes(b: &mut Batcher, t0: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut now = t0;
        while b.backlog().0 > 0 {
            now += FLUSH;
            if let Some((seq, entry)) = b.poll(now, 0) {
                b.answered(seq, &Outcome { applied: true, ..Outcome::default() }, now);
                out.push(entry);
            }
        }
        out
    }

    /// What a store holds once it applied `writes`: its digest of every sample.
    fn applied_all(writes: &[Vec<u8>]) -> anyhow::Result<u64> {
        let mut store = Store::new(Keyed::new());
        for (i, w) in writes.iter().enumerate() {
            applied(&mut store, i as u64 + 1, w)?;
        }
        Ok(store.digest(true)?)
    }

    #[test]
    fn the_same_points_are_stored_alike_whatever_order_they_came_in_and_however_they_came() -> anyhow::Result<()> {
        // 3,000 series of 40 points of values that do not compress: more than one write holds, so
        // series are cut in part too.
        let val = |k: u32, t: i64| (u64::from(k).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (t as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)) as f64;
        let keys: Vec<SeriesKey> = (0..3000).map(|h| key(&format!("h{h}"))).collect::<anyhow::Result<_>>()?;
        let all: Vec<(u32, i64, f64)> = (0..3000u32).flat_map(|k| (0..40i64).map(move |t| (k, t * 10, val(k, t)))).collect();
        let t0 = Instant::now();
        let mut grouped = Batcher::new(9);
        grouped.series(keys.iter().enumerate().map(|(k, key)| (key.clone(), (0..40i64).map(|t| (t * 10, val(k as u32, t))).collect())).collect(), t0);
        let first = writes(&mut grouped, t0);
        anyhow::ensure!(first.len() > 1, "{} writes", first.len());
        let want = applied_all(&first)?;
        // The same points as a request decodes them, in two other orders, each series' own order kept.
        for seed in [1u64, 2] {
            let mut x = seed;
            let mut shuffled = all.clone();
            for i in (1..shuffled.len()).rev() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                shuffled.swap(i, (x % (i as u64 + 1)) as usize);
            }
            let mut next = vec![0i64; 3000];
            for p in &mut shuffled {
                let Some(n) = next.get_mut(p.0 as usize) else { continue };
                *p = (p.0, *n * 10, val(p.0, *n));
                *n += 1;
            }
            let mut flat = Batcher::new(9);
            flat.samples(&mut keys.clone(), &shuffled, t0);
            anyhow::ensure!(applied_all(&writes(&mut flat, t0))? == want, "order {seed} stored otherwise");
        }
        Ok(())
    }

    /// More comes each flush than a write holds: every series' points still go within a few
    /// writes of coming (oldest first), none waiting behind the newer ones for good; and the queue's
    /// wait, kept past the target for an interval, says it is congested until it drains.
    #[test]
    fn under_overload_no_series_waits_behind_newer_ones_and_a_kept_wait_is_congestion() -> anyhow::Result<()> {
        let val = |k: u64, t: i64| (k.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (t as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)) as f64;
        let mut b = Batcher::new(3);
        let t0 = Instant::now();
        let mut now = t0;
        // Each step, 4,000 new series of 30 points: some two and a half times what a write holds.
        let mut first_seen: HashMap<u64, usize> = HashMap::new();
        let mut written_at: HashMap<u64, usize> = HashMap::new();
        let mut congested_once = false;
        for step in 0..30usize {
            let batch: Vec<(SeriesKey, Vec<(i64, f64)>)> = (0..4000u64)
                .map(|j| {
                    let k = key(&format!("s{step}-{j}"))?;
                    first_seen.insert(k.fp, step);
                    Ok((k, (0..30i64).map(|t| (t, val(j, t))).collect()))
                })
                .collect::<anyhow::Result<_>>()?;
            b.series(batch, now);
            now += FLUSH;
            congested_once |= b.congested(now);
            if let Some((seq, entry)) = b.poll(now, 0) {
                let Ok(Command::Write { batch, .. }) = command::decode(&entry) else { anyhow::bail!("not a write") };
                let mut r = kiln_obs::batch::Reader::new(batch)?;
                while let Some(item) = r.next_item()? {
                    if let kiln_obs::batch::Item::Def { fp, .. } = item {
                        written_at.entry(fp).or_insert(step);
                    }
                }
                b.answered(seq, &Outcome { applied: true, ..Outcome::default() }, now);
            }
        }
        anyhow::ensure!(congested_once, "a queue kept growing for 6 s is congestion: its wait {:?}", b.delay(now));
        // Oldest first: every series written was written in the order it came, no later than any
        // series that came after it, and the oldest still waiting came after every one written.
        let mut order: Vec<(usize, usize)> = written_at.iter().filter_map(|(fp, w)| Some((*first_seen.get(fp)?, *w))).collect();
        order.sort_unstable();
        anyhow::ensure!(order.windows(2).all(|w| matches!(w, [a, b] if a.1 <= b.1)), "a series written before an older one");
        let waits = b.delay(now);
        anyhow::ensure!(waits > TARGET, "the queue's wait grows: {waits:?}");
        // Drained, with nothing new: the wait goes, and so does the congestion, an interval on.
        while b.backlog().0 > 0 {
            now += FLUSH;
            if let Some((seq, _)) = b.poll(now, 0) {
                b.answered(seq, &Outcome { applied: true, ..Outcome::default() }, now);
            }
        }
        b.congested(now);
        now += INTERVAL;
        b.congested(now);
        now += INTERVAL;
        anyhow::ensure!(!b.congested(now) && b.delay(now).is_zero(), "drained, it is not congested");
        Ok(())
    }

    #[test]
    fn a_write_never_passes_the_entry_and_what_does_not_fit_waits() -> anyhow::Result<()> {
        let mut b = Batcher::new(1);
        let t0 = Instant::now();
        let mut many = Vec::new();
        for h in 0..2000i64 {
            many.push((key(&format!("h{h}"))?, (0..200).map(|i| (i, (i * h) as f64)).collect()));
        }
        b.series(many, t0);
        b.logs((0..20_000).map(|i| Record { ts: i, message: format!("line {i} {}", "x".repeat(i as usize % 300)), ..Record::default() }).collect(), t0);
        let mut store = Store::new(Keyed::new());
        let mut index = 0;
        let mut now = t0;
        while b.backlog().0 > 0 || b.backlog().1 {
            now += FLUSH;
            let Some((seq, entry)) = b.poll(now, 0) else { continue };
            anyhow::ensure!(entry.len() <= kiln_obs::group::ENTRY_BYTES, "{} bytes", entry.len());
            index += 1;
            let o = applied(&mut store, index, &entry)?;
            b.answered(seq, &o, now);
        }
        anyhow::ensure!(index > 2, "it took several writes: {index}");
        anyhow::ensure!(store.series_count() == 2000);
        anyhow::ensure!(store.totals().samples == 400_000, "{}", store.totals().samples);
        anyhow::ensure!(store.totals().logs == 20_000);
        anyhow::ensure!(b.dropped == 0);
        Ok(())
    }
}
