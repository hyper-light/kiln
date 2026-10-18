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

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use kiln_obs::batch::{Builder, SeriesKey};
use kiln_obs::logs::record::Record;

use super::wire::Outcome;

/// How long the first point of a write waits for more.
pub const FLUSH: Duration = Duration::from_millis(200);
/// A write not answered by then is sent again, as it was.
pub const RESEND: Duration = Duration::from_secs(5);
/// How long a series counts as defined: past it, the next write of it defines it again.
const DEFINED_FOR: Duration = Duration::from_secs(3600);
/// Series counted as defined at the most; past it, all are defined again (a definition is some
/// 50 bytes, and this many is 32 MiB of map).
const KNOWN: usize = 1 << 20;
/// What a write's bytes may come to: the group's entry less its command.
const ENTRY_BUDGET: usize = kiln_obs::group::ENTRY_BYTES - kiln_obs::command::WRITE_HEADER;
/// Points waiting, at the most, and log bytes; past them, what comes is dropped and counted.
const WAITING_POINTS: usize = 4 << 20;
const WAITING_LOG_BYTES: usize = 64 << 20;

/// A write out: its place, its entry, when it was sent, and its series' points (to send again
/// with their definitions if the group does not know them).
struct Out {
    seq: u64,
    entry: Vec<u8>,
    sent: Option<Instant>,
    series: Vec<(SeriesKey, Vec<(i64, f64)>)>,
    defines: Vec<u64>,
}

pub struct Batcher {
    sender: u64,
    seq: u64,
    builder: Builder,
    /// Points waiting, by series.
    waiting: HashMap<u64, (SeriesKey, Vec<(i64, f64)>)>,
    waiting_points: usize,
    logs: VecDeque<Record>,
    log_bytes: usize,
    first_waiting: Option<Instant>,
    out: Option<Out>,
    /// Series a write of this sender defined, and when it was applied.
    known: HashMap<u64, Instant>,
    pub dropped: u64,
    pub written: u64,
}

fn record_bytes(r: &Record) -> usize {
    32 + r.service.len() + r.host.len() + r.source.len() + r.message.len() + r.attrs.iter().map(|(k, v)| k.len() + v.len() + 4).sum::<usize>()
}

impl Batcher {
    pub fn new(sender: u64) -> Self {
        Self { sender, seq: 0, builder: Builder::new(), waiting: HashMap::new(), waiting_points: 0, logs: VecDeque::new(), log_bytes: 0, first_waiting: None, out: None, known: HashMap::new(), dropped: 0, written: 0 }
    }

    pub fn sender(&self) -> u64 {
        self.sender
    }

    /// Points of series, waiting for the next write.
    pub fn series(&mut self, series: Vec<(SeriesKey, Vec<(i64, f64)>)>, now: Instant) {
        for (key, points) in series {
            if self.waiting_points + points.len() > WAITING_POINTS {
                self.dropped += points.len() as u64;
                continue;
            }
            self.waiting_points += points.len();
            match self.waiting.get_mut(&key.fp) {
                Some((_, held)) => held.extend(points),
                None => {
                    self.waiting.insert(key.fp, (key, points));
                }
            }
            self.first_waiting.get_or_insert(now);
        }
    }

    pub fn logs(&mut self, records: Vec<Record>, now: Instant) {
        for r in records {
            let n = record_bytes(&r);
            if self.log_bytes + n > WAITING_LOG_BYTES {
                self.dropped += 1;
                continue;
            }
            self.log_bytes += n;
            self.logs.push_back(r);
            self.first_waiting.get_or_insert(now);
        }
    }

    /// Points and records waiting, and whether a write is out.
    pub fn backlog(&self) -> (u64, bool) {
        ((self.waiting_points + self.logs.len()) as u64, self.out.is_some())
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
        let mut series = Vec::new();
        let mut defines = Vec::new();
        // Oldest-defined first is no order to keep: series in fingerprint order, so a write's bytes
        // do not depend on the map's.
        let mut fps: Vec<u64> = self.waiting.keys().copied().collect();
        fps.sort_unstable();
        for fp in fps {
            if self.builder.byte_len() >= ENTRY_BUDGET / 2 {
                break;
            }
            let Some((key, mut points)) = self.waiting.remove(&fp) else { continue };
            // A series' points past what one write should hold go in parts: a sample takes at most
            // 16 bytes in a run (two 64-bit words, Gorilla's worst).
            let room = (ENTRY_BUDGET / 2).saturating_sub(self.builder.byte_len()) / 16;
            let rest = if points.len() > room.max(1) { points.split_off(room.max(1)) } else { Vec::new() };
            self.waiting_points = self.waiting_points.saturating_sub(points.len());
            let defined = self.known.get(&fp).is_some_and(|t| now.duration_since(*t) < DEFINED_FOR);
            if !defined {
                self.builder.define(&key);
                defines.push(fp);
            }
            self.builder.run(fp, points.iter().copied());
            if !rest.is_empty() {
                self.waiting.insert(fp, (key.clone(), rest));
            }
            series.push((key, points));
        }
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
                take.push(r);
            }
        }
        if !take.is_empty() {
            self.builder.logs(&take);
        }
        if self.waiting.is_empty() && self.logs.is_empty() {
            self.first_waiting = None;
            self.waiting_points = 0;
        } else {
            self.first_waiting = Some(now);
        }
        if self.builder.items() == 0 {
            return;
        }
        self.seq += 1;
        let entry = self.builder.finish_write(self.sender, self.seq, now_ms);
        self.out = Some(Out { seq: self.seq, entry, sent: None, series, defines });
    }

    /// What write `seq` came to. A series the group does not know waits again, its points to be
    /// sent with its definition; a write applied (now or before) is done.
    pub fn answered(&mut self, seq: u64, outcome: &Outcome, now: Instant) {
        let Some(out) = self.out.take_if(|o| o.seq == seq) else { return };
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
            let again: Vec<(SeriesKey, Vec<(i64, f64)>)> = out.series.into_iter().filter(|(k, _)| outcome.unknown.contains(&k.fp)).collect();
            self.series(again, now);
        }
        // Refused and past the bound: those points go, counted.
        self.dropped += (outcome.refused.len() + outcome.over_limit.len()) as u64;
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
