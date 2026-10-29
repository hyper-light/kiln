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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// What a sender of log records waits on to know they are written: done once the write holding
/// the last of them is answered (applied by the group, or refused for good), so a reader of a
/// source it can read again (a file) moves its place only past what is kept, and a crash repeats
/// what was in flight rather than losing it.
#[derive(Debug, Clone, Default)]
pub struct Ack(Arc<AtomicBool>);

impl Ack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn done(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn set(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A write out: its place, its entry, when it was sent, and what it holds of each series (its
/// points, a range of `points`, to send again with their definitions if the group does not know
/// them), and the acks its answer gives. Kept, buffers and all, for the next write once answered.
#[derive(Default)]
struct Out {
    seq: u64,
    entry: Vec<u8>,
    sent: Option<Instant>,
    series: Vec<(SeriesKey, std::ops::Range<usize>)>,
    points: Vec<(i64, f64)>,
    defines: Vec<u64>,
    acks: Vec<Ack>,
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
    /// Log records waiting, as rows, oldest first: one queue, so they are cut in the order their
    /// acks were counted in.
    logs: VecDeque<LogSegment>,
    /// Buffers of segments cut, for the next records'.
    log_spare: Vec<Vec<u8>>,
    /// The segments a write takes, joined (kept from write to write).
    log_joined: Vec<u8>,
    /// Spans as their requests' decoding wrote them: segments of rows, each with its rows. Their
    /// bytes count with the records' against the same bound.
    spans: VecDeque<(Vec<u8>, u32)>,
    /// Segments written into a write, their buffers for the owner to hand back.
    spent: Vec<Vec<u8>>,
    /// Which of records and spans goes first in a write, by turns (neither waits behind the
    /// other); and whether one too big to go beside the points goes before them in the next write
    /// (never two writes running: the points wait behind neither).
    spans_turn: bool,
    records_first: bool,
    last_records_first: bool,
    log_bytes: usize,
    /// Records ever queued and ever cut into a write; the acks waiting, each with the count of
    /// records queued when it came (it is given once a write takes the count's last).
    logs_in: u64,
    logs_cut: u64,
    acks: VecDeque<(u64, Ack)>,
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

/// Log records offered together, in the order they were read: runs of records, and segments of
/// records a collector made rows where it read them.
#[derive(Debug, Default)]
pub struct Logs {
    pub parts: Vec<LogPart>,
    /// Each run's part of the store, by its place (`super::indexes::route_logs`); one not said,
    /// the main part's.
    pub into: Vec<u16>,
}

pub use kiln_obs::logs::record::LogPart;

impl From<Vec<Record>> for Logs {
    fn from(records: Vec<Record>) -> Self {
        Self { parts: if records.is_empty() { Vec::new() } else { vec![LogPart::Records(records)] }, into: Vec::new() }
    }
}

impl Logs {
    /// kiln's events, of their own part of the store whatever the indexes say.
    pub fn events(records: Vec<Record>) -> Self {
        Self { parts: if records.is_empty() { Vec::new() } else { vec![LogPart::Records(records)] }, into: vec![kiln_obs::logs::store::EVENTS] }
    }
}

impl From<Vec<LogPart>> for Logs {
    fn from(parts: Vec<LogPart>) -> Self {
        Self { parts, into: Vec::new() }
    }
}

impl Logs {
    /// Records, whichever form.
    pub fn len(&self) -> u64 {
        self.parts.iter().map(LogPart::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Their weight against [`WAITING_LOG_BYTES`]: a record's [`record_bytes`], rows their bytes.
    pub fn bytes(&self) -> u64 {
        self.parts
            .iter()
            .map(|p| match p {
                LogPart::Records(r) => r.iter().map(|r| record_bytes(r) as u64).sum::<u64>(),
                LogPart::Rows { raw, .. } => raw.len() as u64,
            })
            .sum()
    }

    /// Its records not yet rows, in order.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.parts.iter().flat_map(LogPart::records)
    }

    pub fn records_mut(&mut self) -> impl Iterator<Item = &mut Record> {
        self.parts.iter_mut().flat_map(LogPart::records_mut)
    }

    /// Its runs of records, each to be processed whole (the pipelines take a run at a time).
    pub fn runs_mut(&mut self) -> impl Iterator<Item = &mut Vec<Record>> {
        self.parts.iter_mut().filter_map(|p| match p {
            LogPart::Records(r) => Some(r),
            LogPart::Rows { .. } => None,
        })
    }

    /// Its records alone, for a sink that takes no rows: what a records' offer comes back as (it
    /// holds no rows, none dropped).
    pub fn into_records(self) -> Vec<Record> {
        debug_assert!(self.parts.iter().all(|p| matches!(p, LogPart::Records(_))), "rows where only records go");
        self.parts
            .into_iter()
            .flat_map(|p| match p {
                LogPart::Records(r) => r,
                LogPart::Rows { .. } => Vec::new(),
            })
            .collect()
    }
}

/// Log records waiting: a segment of rows (records written as rows as they came, or rows a
/// collector wrote), how many, their weight against [`WAITING_LOG_BYTES`] (what admission took for
/// them, and gives back as they are cut), and the last one's time (the next segment's first is
/// chained from it, joined in a write).
#[derive(Debug)]
struct LogSegment {
    raw: Vec<u8>,
    rows: u32,
    weight: usize,
    last_ts: i64,
    /// Its part of the store.
    part: u16,
}

/// A segment of rows the batcher writes records into: a log block's text (a write, a megabyte,
/// holds four), past which a new one begins.
const SEGMENT_BYTES: usize = kiln_obs::logs::block::BLOCK_TEXT;
/// Segments' buffers kept for the next records: a megabyte's.
const SPARE_SEGMENTS: usize = 4;

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
            log_spare: Vec::new(),
            log_joined: Vec::new(),
            spans: VecDeque::new(),
            spent: Vec::new(),
            spans_turn: false,
            records_first: false,
            last_records_first: false,
            log_bytes: 0,
            logs_in: 0,
            logs_cut: 0,
            acks: VecDeque::new(),
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
        let logs = if self.logs.is_empty() && self.spans.is_empty() { None } else { self.first_waiting };
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

    /// Log records, and rows a collector wrote, waiting for the next write, in their order.
    pub fn logs(&mut self, logs: impl Into<Logs>, now: Instant) {
        let Logs { parts, into } = logs.into();
        for (i, part) in parts.into_iter().enumerate() {
            let into = into.get(i).copied().unwrap_or(kiln_obs::logs::store::MAIN);
            match part {
                LogPart::Records(records) => self.records(records, into, now),
                LogPart::Rows { raw, rows, last_ts } => self.rows(raw, rows, last_ts, into, now),
            }
        }
    }

    /// Records written as rows as they come, into segments of `into`'s (a record past the bound
    /// dropped).
    fn records(&mut self, records: Vec<Record>, into: u16, now: Instant) {
        let (mut seg, mut prev): (Option<LogSegment>, i64) = (None, 0);
        for r in records {
            let n = record_bytes(&r);
            if self.log_bytes + n > WAITING_LOG_BYTES {
                self.dropped += 1;
                self.left_log_bytes += n as u64;
                continue;
            }
            if seg.is_none() {
                seg = Some(LogSegment { raw: self.log_spare.pop().unwrap_or_default(), rows: 0, weight: 0, last_ts: 0, part: into });
                prev = 0;
            }
            let Some(s) = seg.as_mut() else { continue };
            kiln_obs::logs::record::put_row(&mut s.raw, &r, prev);
            prev = r.ts;
            s.last_ts = r.ts;
            s.rows += 1;
            s.weight += n;
            self.log_bytes += n;
            self.logs_in += 1;
            self.first_waiting.get_or_insert(now);
            if s.raw.len() >= SEGMENT_BYTES {
                self.logs.extend(seg.take());
            }
        }
        self.logs.extend(seg);
    }

    /// A segment of rows a collector wrote, whole, of `into`'s (dropped past the bound).
    fn rows(&mut self, raw: Vec<u8>, n: u32, last_ts: i64, into: u16, now: Instant) {
        let bytes = raw.len();
        if n == 0 {
            self.spare_segment(raw);
            return;
        }
        if self.log_bytes + bytes > WAITING_LOG_BYTES {
            self.dropped += u64::from(n);
            self.left_log_bytes += bytes as u64;
            self.spare_segment(raw);
            return;
        }
        self.log_bytes += bytes;
        self.logs_in += u64::from(n);
        self.logs.push_back(LogSegment { raw, rows: n, weight: bytes, last_ts, part: into });
        self.first_waiting.get_or_insert(now);
    }

    /// A segment's buffer, cut or dropped: kept for the next records', a few of them; the rest to
    /// the owner, for requests' decoding.
    fn spare_segment(&mut self, mut raw: Vec<u8>) {
        if self.log_spare.len() < SPARE_SEGMENTS && raw.capacity() <= 2 * SEGMENT_BYTES {
            raw.clear();
            self.log_spare.push(raw);
        } else {
            self.spent.push(raw);
        }
    }

    /// Spans' segments, waiting for the next write as records do.
    pub fn spans(&mut self, segments: Vec<(Vec<u8>, u32)>, now: Instant) {
        for (rows, n) in segments {
            let bytes = rows.len();
            if n == 0 {
                self.left_log_bytes += bytes as u64;
                self.spent.push(rows);
                continue;
            }
            if self.log_bytes + bytes > WAITING_LOG_BYTES {
                self.dropped += u64::from(n);
                self.left_log_bytes += bytes as u64;
                self.spent.push(rows);
                continue;
            }
            self.log_bytes += bytes;
            self.spans.push_back((rows, n));
            self.first_waiting.get_or_insert(now);
        }
    }

    /// Buffers of spans' segments that went into writes (or were dropped), for the next requests.
    pub fn spent(&mut self) -> std::vec::Drain<'_, Vec<u8>> {
        self.spent.drain(..)
    }

    /// Records with an ack given once the write holding the last of them is answered: with every
    /// record queued before them answered too (writes go one at a time, in order); at once when
    /// nothing is waiting or out.
    pub fn logs_acked(&mut self, logs: impl Into<Logs>, ack: Ack, now: Instant) {
        self.logs(logs, now);
        if self.logs_cut >= self.logs_in && self.out.is_none() && self.acks.is_empty() {
            ack.set();
        } else {
            self.acks.push_back((self.logs_in, ack));
        }
    }

    /// Points and log bytes that have left what waits: see `left_points`.
    pub fn left(&self) -> (u64, u64) {
        (self.left_points, self.left_log_bytes)
    }

    /// Points, records and spans waiting, and whether a write is out.
    pub fn backlog(&self) -> (u64, bool) {
        (self.waiting_points as u64 + self.logs.iter().map(|s| u64::from(s.rows)).sum::<u64>() + self.spans.iter().map(|s| u64::from(s.1)).sum::<u64>(), self.out.is_some())
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
        // A record or a segment that did not fit beside the last write's points goes first.
        let records_first = std::mem::take(&mut self.records_first) && !self.last_records_first;
        self.last_records_first = records_first;
        if records_first {
            self.take_records();
        }
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
        if !records_first {
            self.take_records();
        }
        if self.points.is_empty() && self.logs.is_empty() && self.spans.is_empty() {
            self.first_waiting = None;
            self.waiting_points = 0;
        } else {
            self.first_waiting = Some(now);
        }
        if self.builder.items() == 0 {
            self.spare = out;
            return;
        }
        // The acks whose records this write holds the last of go with it.
        out.acks.clear();
        while self.acks.front().is_some_and(|(at, _)| *at <= self.logs_cut) {
            if let Some((_, a)) = self.acks.pop_front() {
                out.acks.push(a);
            }
        }
        self.seq += 1;
        out.seq = self.seq;
        out.entry = self.builder.finish_write(self.sender, self.seq, now_ms);
        out.sent = None;
        self.out = Some(out);
    }

    /// Log records and spans' segments into the write being built, which first by turns.
    fn take_records(&mut self) {
        self.spans_turn = !self.spans_turn;
        if self.spans_turn {
            self.take_spans();
            self.take_logs();
        } else {
            self.take_logs();
            self.take_spans();
        }
    }

    /// Log records' segments, whole, oldest first, as many as fit, joined into one item (compressed
    /// whole: a segment alone compresses the less well the fewer its rows), each one's first time
    /// chained again from the last before it: rows of the room left, raw, as they are (LZ4's worst
    /// case is a little over its input, the block format's bound n + n/255 + 16, so they fit
    /// whatever they compress to; and what a write holds, applied, is never more than an entry's
    /// worth, however well it compresses). The first of a write of nothing else goes however big (a
    /// segment is at most a block's text and a row); one that does not fit beside what the write
    /// holds goes first in the next.
    fn take_logs(&mut self) {
        let room = ENTRY_BUDGET.saturating_sub(self.builder.byte_len()) * 255 / 256;
        let alone = self.builder.items() == 0;
        // Each part's segments joined (most writes: the main part's alone, in the buffer kept).
        let mut joined: Vec<(u16, Vec<u8>, u32, i64)> = Vec::new();
        let (mut first, mut taken) = (true, 0usize);
        while let Some(s) = self.logs.front() {
            // A first time chained again is a byte longer at the most; an item's head, 64.
            let new_part = !joined.iter().any(|(p, ..)| *p == s.part);
            if !(alone && first) && taken + s.raw.len() + 1 + 64 * (1 + usize::from(new_part)) > room {
                if first {
                    self.records_first = true;
                }
                break;
            }
            let Some(s) = self.logs.pop_front() else { break };
            first = false;
            let at = match joined.iter().position(|(p, ..)| *p == s.part) {
                Some(at) => at,
                None => {
                    let mut buf = if joined.is_empty() { std::mem::take(&mut self.log_joined) } else { self.log_spare.pop().unwrap_or_default() };
                    buf.clear();
                    joined.push((s.part, buf, 0, 0));
                    taken += 64;
                    joined.len() - 1
                }
            };
            if let Some((_, buf, rows, last)) = joined.get_mut(at) {
                let before = buf.len();
                if kiln_obs::logs::record::put_rows_after(buf, &s.raw, *last) {
                    *rows += s.rows;
                    *last = s.last_ts;
                } else {
                    // Never so of rows written here; were it, they go, counted.
                    self.dropped += u64::from(s.rows);
                }
                taken += buf.len() - before;
            }
            self.logs_cut += u64::from(s.rows);
            self.log_bytes = self.log_bytes.saturating_sub(s.weight);
            self.left_log_bytes += s.weight as u64;
            self.spare_segment(s.raw);
        }
        for (i, (part, buf, rows, _)) in joined.into_iter().enumerate() {
            if rows > 0 {
                self.builder.log_rows_in(part, &buf, rows);
            }
            if i == 0 {
                self.log_joined = buf;
            } else {
                self.spare_segment(buf);
            }
        }
    }

    /// Spans' segments, whole, as many as fit (one always fits a write of nothing else: a segment
    /// is at most half an entry, `kiln_obs::otlp::traces::MAX_ROW_BYTES`); one that does not fit
    /// beside what the write holds goes first in the next.
    fn take_spans(&mut self) {
        let mut took = false;
        while let Some((rows, _)) = self.spans.front() {
            let room = ENTRY_BUDGET.saturating_sub(self.builder.byte_len()) * 255 / 256;
            if self.builder.items() > 0 && rows.len() + 64 > room {
                if !took {
                    self.records_first = true;
                }
                break;
            }
            let Some((rows, n)) = self.spans.pop_front() else { break };
            self.builder.spans(&rows, n);
            self.log_bytes = self.log_bytes.saturating_sub(rows.len());
            self.left_log_bytes += rows.len() as u64;
            self.spent.push(rows);
            took = true;
        }
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
        // Its records' answer is final either way: kept, or refused for good (sending them again
        // would be refused again).
        for a in out.acks.drain(..) {
            a.set();
        }
        // An ack that came while this write was out, of records it or an earlier one held.
        while self.acks.front().is_some_and(|(at, _)| *at <= self.logs_cut) {
            if let Some((_, a)) = self.acks.pop_front() {
                a.set();
            }
        }
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
    fn an_ack_is_given_once_the_write_holding_its_last_record_is_answered_and_not_before() -> anyhow::Result<()> {
        let mut b = Batcher::new(7);
        let t0 = Instant::now();
        let rec = |i: usize, size: usize| Record { ts: i as i64, message: format!("{i} {}", "x".repeat(size)), ..Record::default() };
        // Three batches; the second some two and a half writes' worth.
        let acks = [Ack::new(), Ack::new(), Ack::new()];
        let ends = [10u64, 60, 61];
        b.logs_acked((0..10).map(|i| rec(i, 10)).collect::<Vec<_>>(), acks[0].clone(), t0);
        b.logs_acked((10..60).map(|i| rec(i, ENTRY_BUDGET / 20)).collect::<Vec<_>>(), acks[1].clone(), t0);
        b.logs_acked((60..61).map(|i| rec(i, 10)).collect::<Vec<_>>(), acks[2].clone(), t0);
        let mut store = Store::new(Keyed::new());
        let (mut index, mut t) = (0, t0);
        // Each ack's index and the records stored when it was given.
        let mut given: Vec<(usize, u64)> = Vec::new();
        while let Some((seq, entry)) = b.poll(t + FLUSH, 0) {
            index += 1;
            let o = applied(&mut store, index, &entry)?;
            let before: Vec<bool> = acks.iter().map(Ack::done).collect();
            anyhow::ensure!(before.iter().zip(&ends).all(|(&done, &end)| !done || store.totals().logs >= end), "an ack before its records were stored");
            b.answered(seq, &o, t);
            for (i, a) in acks.iter().enumerate() {
                if a.done() && !before.get(i).copied().unwrap_or(true) {
                    given.push((i, store.totals().logs));
                }
            }
            t += FLUSH;
        }
        anyhow::ensure!(index >= 3, "the records went in several writes: {index}");
        anyhow::ensure!(given.iter().map(|g| g.0).collect::<Vec<_>>() == vec![0, 1, 2], "each given, in order: {given:?}");
        anyhow::ensure!(given.iter().all(|&(i, stored)| ends.get(i).is_some_and(|&end| stored >= end)), "each once its records were stored: {given:?}");
        // Nothing waiting or out: given at once.
        let now = Ack::new();
        b.logs_acked(Vec::<Record>::new(), now.clone(), t);
        anyhow::ensure!(now.done());
        // A write out: an ack of nothing waits for it.
        let (held, after) = (Ack::new(), Ack::new());
        b.logs_acked(vec![rec(61, 10)], held.clone(), t);
        let (seq, entry) = b.poll(t + FLUSH, 0).ok_or_else(|| anyhow::anyhow!("a write"))?;
        b.logs_acked(Vec::<Record>::new(), after.clone(), t);
        anyhow::ensure!(!held.done() && !after.done(), "given before the write was answered");
        let o = applied(&mut store, index + 1, &entry)?;
        b.answered(seq, &o, t);
        anyhow::ensure!(held.done() && after.done());
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
        b.logs((0..20_000).map(|i| Record { ts: i, message: format!("line {i} {}", "x".repeat(i as usize % 300)), ..Record::default() }).collect::<Vec<_>>(), t0);
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

    /// Records and spans both flooding, with points: each write holds some of each kind waiting
    /// (none waits behind another), none passes the entry; a record too big to go beside the points
    /// goes first in the next write, and the points go the write after.
    #[test]
    fn records_and_spans_take_turns_and_a_big_record_goes_whole() -> anyhow::Result<()> {
        let mut b = Batcher::new(9);
        let t0 = Instant::now();
        // Points enough for half an entry a write, for a while.
        b.series((0..4000).map(|h| -> anyhow::Result<_> { Ok((key(&format!("h{h}"))?, (0..20).map(|i| (i, i as f64)).collect())) }).collect::<anyhow::Result<Vec<_>>>()?, t0);
        b.logs((0..4000).map(|i| Record { ts: i, message: format!("line {i} {}", "y".repeat(400)), ..Record::default() }).collect::<Vec<_>>(), t0);
        let mut raw = Vec::new();
        let mut prev = 0;
        // A segment as the decoder cuts them: under a segment's bytes, its last row past them.
        for i in 0..500i64 {
            let r = Record { ts: 1_700_000_000_000_000_000 + i, message: "GET".into(), attrs: vec![("p".into(), "z".repeat(400))], trace_id: Some([1; 16]), span_id: Some((i + 1).to_le_bytes()), ..Record::default() };
            kiln_obs::logs::record::put_row(&mut raw, &r, prev);
            prev = r.ts;
        }
        let segment = raw.len();
        b.spans((0..8).map(|_| (raw.clone(), 500u32)).collect(), t0);
        // A record of 700 KB: past what is left beside points.
        b.logs(vec![Record { ts: 9_999, message: "B".repeat(700_000), ..Record::default() }], t0);
        let mut store = Store::new(Keyed::new());
        let (mut index, mut now) = (0, t0);
        let mut kinds: Vec<(bool, bool, bool)> = Vec::new();
        while b.backlog().0 > 0 || b.backlog().1 {
            now += FLUSH;
            let Some((seq, entry)) = b.poll(now, 1_700_000_000_000) else { continue };
            anyhow::ensure!(entry.len() <= kiln_obs::group::ENTRY_BYTES, "{} bytes", entry.len());
            let Ok(Command::Write { batch, .. }) = command::decode(&entry) else { anyhow::bail!("not a write") };
            let mut r = kiln_obs::batch::Reader::new(batch)?;
            let (mut points, mut logs, mut spans) = (false, false, false);
            while let Some(item) = r.next_item()? {
                match item {
                    kiln_obs::batch::Item::Run { .. } => points = true,
                    kiln_obs::batch::Item::Logs { .. } => logs = true,
                    kiln_obs::batch::Item::Spans { .. } => spans = true,
                    kiln_obs::batch::Item::Def { .. } => {}
                }
            }
            kinds.push((points, logs, spans));
            index += 1;
            let o = applied(&mut store, index, &entry)?;
            b.answered(seq, &o, now);
            anyhow::ensure!(index < 1_000, "it never drained");
        }
        anyhow::ensure!(store.totals().logs == 4_001 && store.totals().spans == 4_000 && store.totals().samples == 80_000, "{:?}", store.totals());
        // While spans waited, a write never went without them for two writes running: they took turns.
        let spans_until = kinds.iter().rposition(|k| k.2).unwrap_or(0);
        anyhow::ensure!(kinds.get(..spans_until).unwrap_or_default().windows(3).all(|w| w.iter().any(|k| k.2)), "spans waited behind records: {kinds:?}");
        anyhow::ensure!(segment < kiln_obs::otlp::traces::MAX_ROW_BYTES * 2);
        Ok(())
    }

    /// Records and the rows a collector wrote, offered together, are written in the order they
    /// were read: runs and segments alike, each row as it was written, whatever their times.
    #[test]
    fn records_and_rows_are_written_in_the_order_they_were_read() -> anyhow::Result<()> {
        let rec = |i: i64| Record { ts: 1_700_000_000_000_000_000 + i, message: format!("line {i}"), ..Record::default() };
        let rows = |of: &[i64]| {
            let (mut raw, mut prev) = (Vec::new(), 0);
            for &i in of {
                let r = rec(i);
                kiln_obs::logs::record::put_row(&mut raw, &r, prev);
                prev = r.ts;
            }
            LogPart::Rows { raw, rows: of.len() as u32, last_ts: prev }
        };
        // Two JSON lines, two text lines, a JSON line, a text line.
        let parts = vec![LogPart::Records(vec![rec(5), rec(1)]), rows(&[4, 2]), LogPart::Records(vec![rec(6)]), rows(&[3])];
        let mut b = Batcher::new(9);
        let t0 = Instant::now();
        b.logs(Logs::from(parts), t0);
        let Some((_, entry)) = b.poll(t0 + FLUSH, 1_700_000_000_000) else { anyhow::bail!("no write") };
        let Ok(Command::Write { batch, .. }) = command::decode(&entry) else { anyhow::bail!("not a write") };
        let mut items = kiln_obs::batch::Reader::new(batch)?;
        let (mut read, mut logs_items) = (Vec::new(), 0);
        while let Some(item) = items.next_item()? {
            if let kiln_obs::batch::Item::Logs { rows, raw_len, packed, .. } = item {
                logs_items += 1;
                let mut raw = Vec::new();
                kiln_obs::lz4::decompress(packed, &mut raw, raw_len)?;
                let mut it = kiln_obs::logs::record::Rows::new(&raw, rows as usize);
                while let Some(r) = it.next_row()? {
                    read.push(r.to_owned().message);
                }
            }
        }
        anyhow::ensure!(read == ["line 5", "line 1", "line 4", "line 2", "line 6", "line 3"], "{read:?}");
        // Joined: one item, compressed whole.
        anyhow::ensure!(logs_items == 1, "{logs_items} items");
        anyhow::ensure!(b.backlog() == (0, true), "{:?}", b.backlog());
        Ok(())
    }

    /// Spans as a request's decoding wrote them: segments packed whole beside points and records,
    /// none past the entry, all stored once, their buffers handed back.
    #[test]
    fn spans_segments_ride_writes_whole_and_their_buffers_come_back() -> anyhow::Result<()> {
        use kiln_obs::otlp::pb::Writer;
        // Requests of 2,000 spans each, with payloads that do not compress (a write holds few).
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut noise = |n: usize| -> String {
            (0..n)
                .map(|_| {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    char::from(b'!' + (rng % 90) as u8)
                })
                .collect()
        };
        let mut b = Batcher::new(3);
        let t0 = Instant::now();
        let mut decoder = kiln_obs::otlp::traces::Decoder::new();
        let mut sent = 0u64;
        for req in 0..6u64 {
            let mut w = Writer::default();
            w.message(1, |rs| {
                rs.message(2, |ss| {
                    for i in 0..2000u64 {
                        ss.message(2, |s| {
                            s.bytes(1, &(u128::from(req * 10_000 + i / 10) + 1).to_le_bytes());
                            s.bytes(2, &(req * 10_000 + i + 1).to_le_bytes());
                            s.str(5, "GET /x");
                            s.fixed64(7, 1_700_000_000_000_000_000 + i);
                            s.fixed64(8, 1_700_000_000_000_000_100 + i);
                            s.message(9, |kv| {
                                kv.str(1, "payload");
                                kv.message(2, |v| v.str(1, &noise(300)));
                            });
                        });
                    }
                });
            });
            let mut spans = kiln_obs::otlp::traces::Spans::default();
            decoder.decode(&w.buf, 0, &mut spans)?;
            sent += spans.spans;
            b.spans(spans.segments, t0);
        }
        b.logs((0..500).map(|i| Record { ts: i, message: format!("line {i}"), ..Record::default() }).collect::<Vec<_>>(), t0);
        let mut store = Store::new(Keyed::new());
        let (mut index, mut now, mut spent) = (0, t0, 0);
        while b.backlog().0 > 0 || b.backlog().1 {
            now += FLUSH;
            // Written at the spans' time: none is ahead of its write.
            let Some((seq, entry)) = b.poll(now, 1_700_000_000_000) else { continue };
            anyhow::ensure!(entry.len() <= kiln_obs::group::ENTRY_BYTES, "{} bytes", entry.len());
            index += 1;
            let o = applied(&mut store, index, &entry)?;
            b.answered(seq, &o, now);
            spent += b.spent().count();
        }
        anyhow::ensure!(index > 3, "it took several writes: {index}");
        anyhow::ensure!(store.totals().spans == sent && sent == 12_000 && store.totals().spans_ahead == 0, "{} of {sent}", store.totals().spans);
        anyhow::ensure!(store.totals().logs == 500);
        anyhow::ensure!(b.dropped == 0 && spent > 6, "every segment's buffer back: {spent}");
        let all = kiln_obs::logs::search::Search::all();
        anyhow::ensure!(store.search_spans(&all, 0, i64::MAX, usize::MAX)?.records.len() == 12_000);
        Ok(())
    }
}
