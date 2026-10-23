use std::collections::BTreeSet;

use super::*;
use crate::batch::{Builder, SeriesKey};
use crate::logs::record::Record;
use crate::testkit::Rng;

fn read_all(s: &Store, sel: &Selector) -> Vec<(String, Vec<(i64, u64)>)> {
    let mut ids = Vec::new();
    s.select(sel, &mut ids);
    let mut out: Vec<(String, Vec<(i64, u64)>)> = ids
        .iter()
        .map(|&id| {
            let r = s.series(id).unwrap();
            let name = format!("{}{:?}{:?}", r.metric(), r.tags().collect::<Vec<_>>(), r.kind());
            let (mut ts, mut vs) = (Vec::new(), Vec::new());
            s.read(id, i64::MIN, i64::MAX, &mut ts, &mut vs).unwrap();
            (name, ts.into_iter().zip(vs.into_iter().map(f64::to_bits)).collect())
        })
        .collect();
    out.sort();
    out
}

fn gauge(metric: &str, host: &str) -> SeriesKey {
    SeriesKey::new(metric, [("host", host)], Kind::Gauge).unwrap()
}

#[test]
fn written_samples_are_selected_and_read_back() {
    let mut s = Store::new(Keyed::with_keys(1, 2));
    let a = SeriesKey::new("system.cpu.user", [("host", "a"), ("env", "prod")], Kind::Gauge).unwrap();
    let b_key = SeriesKey::new("system.cpu.user", [("host", "b"), ("env", "prod")], Kind::Gauge).unwrap();
    let m = SeriesKey::new("system.mem.used", [("host", "a")], Kind::Gauge).unwrap();
    let mut b = Builder::new();
    for k in [&a, &b_key, &m] {
        b.define(k);
    }
    b.run(a.fp, (0..10).map(|i| (i * 1000, i as f64)));
    b.run(b_key.fp, (0..10).map(|i| (i * 1000, 100.0 + i as f64)));
    b.run(m.fp, [(0, 1e9)]);
    let applied = s.apply(&b.finish()).unwrap();
    assert_eq!(applied.counts, Counts { runs: 3, new_series: 3, samples: 21, ..Counts::default() });
    let mut ids = Vec::new();
    s.select(&Selector::metric("system.cpu.user").tag("env", "prod"), &mut ids);
    assert_eq!(ids.len(), 2);
    let b_id = ids.iter().copied().find(|&id| s.series(id).and_then(|r| r.tag("host")) == Some("b")).unwrap();
    let (mut ts, mut vs) = (Vec::new(), Vec::new());
    s.read(b_id, 2000, 4000, &mut ts, &mut vs).unwrap();
    assert_eq!((ts.clone(), vs.clone()), (vec![2000, 3000, 4000], vec![102.0, 103.0, 104.0]));
    let mut b = Builder::new();
    b.run(b_key.fp, [(10_000, 110.0), (500, 0.5)]);
    let applied = s.apply(&b.finish()).unwrap();
    assert_eq!((applied.counts.new_series, applied.counts.late), (0, 1));
    ts.clear();
    vs.clear();
    s.read(b_id, 0, 1000, &mut ts, &mut vs).unwrap();
    assert_eq!(ts, vec![0, 500, 1000]);
    assert_eq!(s.metric_names(), vec!["system.cpu.user", "system.mem.used"]);
    assert_eq!(s.tag_values("host"), vec!["a", "b"]);
}

#[test]
fn a_run_of_an_unknown_series_is_reported_and_a_bad_definition_refused() {
    let mut s = Store::new(Keyed::with_keys(1, 2));
    let a = gauge("m", "a");
    let mut b = Builder::new();
    b.run(a.fp, [(1, 1.0)]);
    let applied = s.apply(&b.finish()).unwrap();
    assert_eq!(applied.unknown, vec![a.fp]);
    assert_eq!(s.series_count(), 0);
    b.define(&a);
    b.run(a.fp, [(1, 1.0)]);
    let applied = s.apply(&b.finish()).unwrap();
    assert!(applied.unknown.is_empty());
    assert_eq!(applied.counts.samples, 1);
    let mut forged = gauge("m", "evil");
    forged.fp = a.fp;
    b.define(&forged);
    let applied = s.apply(&b.finish()).unwrap();
    assert_eq!(applied.refused, vec![a.fp]);
    assert_eq!(s.series_count(), 1);
    // A new series whose fingerprint is not its key's is refused when its samples come.
    let mut wrong = gauge("m", "b");
    wrong.fp ^= 1;
    b.define(&wrong);
    b.run(wrong.fp, [(1, 1.0)]);
    let applied = s.apply(&b.finish()).unwrap();
    assert_eq!(applied.refused, vec![wrong.fp]);
    assert_eq!(s.series_count(), 1);
}

#[test]
fn a_definition_takes_effect_with_its_first_sample_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    let (alone, expired, kept) = (gauge("m", "alone"), gauge("m", "expired"), gauge("m", "kept"));
    s.retain_logical(1_000, 1_000);
    let mut b = Builder::new();
    b.define(&alone);
    b.define(&expired);
    b.run(expired.fp, [(10, 1.0), (999, 2.0)]);
    b.define(&kept);
    b.run(kept.fp, [(500, 1.0), (1_000, 2.0)]);
    let applied = s.apply(&b.finish()).unwrap();
    assert_eq!((applied.counts.new_series, applied.counts.samples, applied.counts.expired), (1, 1, 3));
    assert_eq!(s.tag_values("host"), vec!["kept"]);
    // A run of a series defined alone in an earlier batch: unknown, as on a replica that cut
    // between the two and opened again.
    b.run(alone.fp, [(2_000, 1.0)]);
    assert_eq!(s.apply(&b.finish()).unwrap().unknown, vec![alone.fp]);
    s.cut(Point { index: 1, term: 1 }).unwrap();
    drop(s);
    let s = Store::open(dir.path(), Keyed::with_keys(3, 4)).unwrap();
    assert_eq!(s.tag_values("host"), vec!["kept"]);
    assert_eq!(read_all(&s, &Selector::metric("m")), vec![(format!("m{:?}{:?}", vec![("host", "kept")], Kind::Gauge), vec![(1_000, 2f64.to_bits())])]);
}

#[test]
fn a_write_sent_twice_is_applied_once() {
    let mut s = Store::new(Keyed::with_keys(1, 2));
    let k = gauge("m", "a");
    let mut b = Builder::new();
    b.define(&k);
    b.run(k.fp, [(1, 1.0)]);
    b.logs(&[Record { ts: 5, message: "once".into(), ..Record::default() }]);
    let first = b.finish_write(77, 1, 1_000);
    let applied = |a: Answer| matches!(a, Answer::Write { outcome: Written::Applied(_), .. });
    assert!(applied(s.apply_entry(1, &first).unwrap()));
    // Again, after a leader changed: nothing again.
    assert_eq!(s.apply_entry(2, &first), Ok(Answer::Write { sender: 77, seq: 1, outcome: Written::Duplicate }));
    let all = crate::logs::search::parse("*").unwrap();
    assert_eq!(s.search_logs(&all, i64::MIN, i64::MAX, 10).unwrap().records.len(), 1);
    // The sender's next, and another sender's first, are applied.
    b.run(k.fp, [(2, 2.0)]);
    assert!(applied(s.apply_entry(3, &b.finish_write(77, 2, 1_001)).unwrap()));
    b.run(k.fp, [(3, 3.0)]);
    assert!(applied(s.apply_entry(4, &b.finish_write(78, 1, 1_002)).unwrap()));
    assert_eq!(s.sessions().get(&77), Some(&Session { seq: 2, at: 1_001, index: 3 }));
    // A session goes once its last write is older than the cutoff.
    s.retain_logical(1_002, 1_002);
    assert_eq!(s.sessions().keys().copied().collect::<Vec<_>>(), vec![78]);
    // An entry of a command this build does not know is never skipped.
    assert_eq!(s.apply_entry(5, &[200, 1, 2, 3]), Err(Newer::Command(200)));
    let mut newer = crate::command::write(9, 1, 0, &[]);
    newer.push(crate::batch::VERSION + 1);
    assert_eq!(s.apply_entry(5, &newer), Err(Newer::Batch(crate::batch::VERSION + 1)));
    assert_eq!(s.apply_entry(5, &[crate::command::RETAIN, 1]), Ok(Answer::Malformed(CommandError::Short)));
}

/// Metrics from logs learn how late records come from each write's newest record, whatever items
/// hold its records: a late segment first and fresh records after are one batch, as fresh as its
/// newest (an item alone would say the late segment's twenty seconds); its windows all noted. Spans
/// alike.
#[test]
fn a_write_s_records_are_one_batch_to_metrics_from_logs_whatever_items_hold_them() {
    use crate::logs::metrics::{Touched, lag_bucket};
    let mut s = Store::new(Keyed::with_keys(1, 2));
    let (logs, spans) = (Arc::new(Touched::new()), Arc::new(Touched::new()));
    s.set_touched(Some(logs.clone()));
    s.set_spans_touched(Some(spans.clone()));
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()).unwrap();
    let rec = |ago_ms: i64| Record { ts: now - ago_ms * 1_000_000, message: format!("{ago_ms} ms ago"), ..Record::default() };
    let rows = |ago: &[i64]| {
        let (mut raw, mut prev) = (Vec::new(), 0);
        for &a in ago {
            let r = rec(a);
            crate::logs::record::put_row(&mut raw, &r, prev);
            prev = r.ts;
        }
        (raw, ago.len() as u32)
    };
    let mut b = Builder::new();
    b.logs(&[rec(20_000), rec(21_000)]);
    let (raw, n) = rows(&[300, 200]);
    b.log_rows(&raw, n);
    let (late, n_late) = rows(&[20_000]);
    b.spans(&late, n_late);
    let (fresh, n_fresh) = rows(&[100]);
    b.spans(&fresh, n_fresh);
    let Ok(Answer::Write { outcome: Written::Applied(a), .. }) = s.apply_entry(1, &b.finish_write(5, 1, now / 1_000_000)) else { panic!("not applied") };
    assert_eq!((a.counts.logs, a.counts.spans), (4, 2));
    for (what, t) in [("logs", &logs), ("spans", &spans)] {
        let got = t.take();
        assert_eq!((got.batches, got.backlogs, got.lags.iter().sum::<u32>()), (1, 0, 1), "{what}: one batch");
        let noted = got.lags.iter().position(|&n| n > 0).unwrap();
        assert!(noted <= lag_bucket(1_000), "{what}: as late as its newest, not its first item's: bucket {noted}");
        let window = |ago_ms: i64| crate::logs::metrics::window_of(now - ago_ms * 1_000_000);
        for ago in [20_000, 100] {
            assert!(got.windows.iter().any(|&(a, z)| (a..=z).contains(&window(ago))), "{what}: the window of {ago} ms ago: {:?}", got.windows);
        }
    }
}

/// A write's samples more than ten minutes past its time, and its log records more than two hours
/// past it, are counted and dropped, judged by the write's own time (its leader's clock): every
/// replica alike. A series whose only sample was ahead is not defined.
#[test]
fn samples_and_records_too_far_ahead_of_their_write_are_counted_and_dropped() {
    let mut s = Store::new(Keyed::with_keys(1, 2));
    let at = 1_760_000_000_000i64;
    let (k, lone) = (gauge("m", "a"), gauge("m", "ahead"));
    let mut b = Builder::new();
    b.define(&k);
    b.run(k.fp, [(at - 60_000, 1.0), (at + 9 * 60_000, 2.0), (at + 11 * 60_000, 3.0)]);
    b.define(&lone);
    b.run(lone.fp, [(at + 3_600_000, 4.0)]);
    let ns = |ms: i64| ms * 1_000_000;
    b.logs(&[
        Record { ts: ns(at), message: "now".into(), ..Record::default() },
        Record { ts: ns(at + 3 * 3_600_000), message: "three hours ahead".into(), ..Record::default() },
        Record { ts: ns(at + 60_000), message: "a minute ahead".into(), ..Record::default() },
    ]);
    let Ok(Answer::Write { outcome: Written::Applied(a), .. }) = s.apply_entry(1, &b.finish_write(5, 1, at)) else { panic!("not applied") };
    assert_eq!((a.counts.samples, a.counts.ahead, a.counts.logs, a.counts.logs_ahead), (2, 2, 2, 1));
    let series = read_all(&s, &Selector::metric("m"));
    assert_eq!(series.len(), 1, "the series whose only sample was ahead is not defined: {series:?}");
    assert_eq!(series[0].1, vec![(at - 60_000, 1f64.to_bits()), (at + 9 * 60_000, 2f64.to_bits())]);
    let all = crate::logs::search::parse("*").unwrap();
    let kept: Vec<String> = s.search_logs(&all, i64::MIN, i64::MAX, 10).unwrap().records.into_iter().map(|r| r.message).collect();
    assert_eq!(kept, vec!["a minute ahead".to_owned(), "now".to_owned()]);
}

#[test]
fn a_bound_on_series_refuses_new_series_past_it() {
    let mut s = Store::new(Keyed::with_keys(1, 2));
    assert_eq!(s.apply_entry(1, &crate::command::limits(2)), Ok(Answer::Limits { max_series: 2 }));
    let mut b = Builder::new();
    for h in ["a", "b", "c"] {
        let k = gauge("m", h);
        b.define(&k);
        b.run(k.fp, [(1, 1.0)]);
    }
    let Ok(Answer::Write { outcome: Written::Applied(a), .. }) = s.apply_entry(2, &b.finish_write(1, 1, 0)) else { panic!() };
    assert_eq!(a.over_limit, vec![gauge("m", "c").fp]);
    assert_eq!(s.series_count(), 2);
    // Known series still take samples.
    b.run(gauge("m", "a").fp, [(2, 2.0)]);
    let Ok(Answer::Write { outcome: Written::Applied(a), .. }) = s.apply_entry(3, &b.finish_write(1, 2, 0)) else { panic!() };
    assert_eq!(a.counts.samples, 1);
}

/// Batches of random series, times and late writes, with the model of what a read must give.
fn random_batches(seed: u64, rounds: usize) -> (Vec<Vec<u8>>, BTreeMap<(String, i64), u64>) {
    let mut rng = Rng::new(seed);
    let mut model = BTreeMap::new();
    let mut batches = Vec::new();
    let mut b = Builder::new();
    for round in 0..rounds {
        for _ in 0..20 {
            let host = format!("h{}", rng.below(15));
            let k = gauge("m", &host);
            b.define(&k);
            let samples: Vec<(i64, f64)> = (0..4)
                .map(|i| {
                    let late = rng.below(8) == 0;
                    let t = if late { rng.below(round as u64 * 1000 + 1) as i64 } else { round as i64 * 1000 + i * 100 + rng.below(50) as i64 };
                    (t, rng.f64())
                })
                .collect();
            // Within a run, the last given at a time wins.
            let mut dedup: BTreeMap<i64, f64> = BTreeMap::new();
            for (t, v) in &samples {
                dedup.insert(*t, *v);
            }
            for (t, v) in dedup {
                model.insert((format!("m{:?}{:?}", vec![("host", host.as_str())], Kind::Gauge), t), v.to_bits());
            }
            b.run(k.fp, samples);
        }
        batches.push(b.finish());
    }
    (batches, model)
}

fn model_view(model: &BTreeMap<(String, i64), u64>) -> Vec<(String, Vec<(i64, u64)>)> {
    let mut out: BTreeMap<String, Vec<(i64, u64)>> = BTreeMap::new();
    for ((name, t), v) in model {
        out.entry(name.clone()).or_default().push((*t, *v));
    }
    out.into_iter().collect()
}

#[test]
fn cuts_compactions_and_a_reopen_keep_exactly_what_was_written() {
    let dir = tempfile::tempdir().unwrap();
    let (batches, model) = random_batches(7, 60);
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    for (i, batch) in batches.iter().enumerate() {
        s.apply(batch).unwrap();
        if i % 7 == 6 {
            s.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
            s.compact().unwrap();
        }
    }
    let sel = Selector::metric("m");
    assert_eq!(read_all(&s, &sel), model_view(&model), "live");
    // Blocks stay logarithmic in what they hold.
    assert!(s.block_count() <= 1 + (batches.len() / 7).ilog2() as usize + 1, "{} blocks", s.block_count());
    let durable = s.durable();
    assert_eq!(durable.index, 56);
    // A restart: blocks reopen, and the log is replayed from the durable point.
    drop(s);
    let mut s = Store::open(dir.path(), Keyed::with_keys(5, 6)).unwrap();
    assert_eq!(s.durable(), durable);
    for batch in batches.iter().skip(durable.index as usize) {
        s.apply(batch).unwrap();
    }
    assert_eq!(read_all(&s, &sel), model_view(&model), "after a restart");
}

#[test]
fn a_crash_between_a_block_and_its_manifest_leaves_no_trace() {
    let dir = tempfile::tempdir().unwrap();
    let (batches, model) = random_batches(9, 20);
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    for batch in batches.iter().take(10) {
        s.apply(batch).unwrap();
    }
    s.cut(Point { index: 10, term: 1 }).unwrap();
    for batch in batches.iter().skip(10) {
        s.apply(batch).unwrap();
    }
    // A block written whole whose manifest never was (the crash after the block's sync): the
    // block is an orphan, deleted at open; its samples come back from the log.
    let orphan = dir.path().join(manifest::block_file(99));
    std::fs::copy(dir.path().join(manifest::block_file(0)), &orphan).unwrap();
    std::fs::write(dir.path().join("MANIFEST.tmp"), b"half a manifest").unwrap();
    drop(s);
    let mut s = Store::open(dir.path(), Keyed::with_keys(3, 4)).unwrap();
    assert!(!orphan.exists() && !dir.path().join("MANIFEST.tmp").exists());
    assert_eq!(s.durable().index, 10);
    for batch in batches.iter().skip(10) {
        s.apply(batch).unwrap();
    }
    assert_eq!(read_all(&s, &Selector::metric("m")), model_view(&model));
}

#[test]
fn a_correction_after_a_cut_wins_over_the_block() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    let k = SeriesKey::new("m", [], Kind::Gauge).unwrap();
    let mut b = Builder::new();
    b.define(&k);
    b.run(k.fp, (0..10).map(|i| (i * 10, i as f64)));
    s.apply(&b.finish()).unwrap();
    s.cut(Point { index: 1, term: 1 }).unwrap();
    b.run(k.fp, [(50, -5.0), (55, 5.5), (200, 20.0)]);
    s.apply(&b.finish()).unwrap();
    let mut ids = Vec::new();
    s.select(&Selector::metric("m"), &mut ids);
    let (mut ts, mut vs) = (Vec::new(), Vec::new());
    s.read(ids[0], 40, 60, &mut ts, &mut vs).unwrap();
    assert_eq!(ts, vec![40, 50, 55, 60]);
    assert_eq!(vs, vec![4.0, -5.0, 5.5, 6.0]);
    // Cut again: two blocks overlap in time, the later wins; compacted into one, the same.
    s.cut(Point { index: 2, term: 1 }).unwrap();
    ts.clear();
    vs.clear();
    s.read(ids[0], 40, 60, &mut ts, &mut vs).unwrap();
    assert_eq!(vs, vec![4.0, -5.0, 5.5, 6.0]);
    s.compact().unwrap();
    assert_eq!(s.block_count(), 1);
    ts.clear();
    vs.clear();
    s.read(ids[0], 40, 60, &mut ts, &mut vs).unwrap();
    assert_eq!(vs, vec![4.0, -5.0, 5.5, 6.0]);
}

#[test]
fn retention_drops_series_at_its_entry_and_files_once_it_is_durable() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    let (old, live) = (gauge("m", "old"), gauge("m", "live"));
    let mut b = Builder::new();
    b.define(&old);
    b.define(&live);
    b.run(old.fp, (0..10).map(|i| (i * 10, 1.0)));
    b.run(live.fp, (0..10).map(|i| (i * 10, 2.0)));
    s.apply(&b.finish()).unwrap();
    s.cut(Point { index: 1, term: 1 }).unwrap();
    b.run(live.fp, (100..110).map(|i| (i * 10, 3.0)));
    s.apply(&b.finish()).unwrap();
    s.cut(Point { index: 2, term: 1 }).unwrap();
    // The entry: the series goes at once, and nothing older is read; the files stay.
    assert_eq!(s.retain_logical(1000, 1000), 1);
    assert_eq!(s.tag_values("host"), vec!["live"]);
    let mut ids = Vec::new();
    s.select(&Selector::metric("m"), &mut ids);
    let (mut ts, mut vs) = (Vec::new(), Vec::new());
    s.read(ids[0], i64::MIN, i64::MAX, &mut ts, &mut vs).unwrap();
    assert_eq!(ts.first(), Some(&1000));
    assert_eq!(s.retain_files().unwrap().blocks_removed, 0, "the cutoff is not durable yet");
    // Once a cut made it durable, the block wholly older goes.
    s.cut(Point { index: 3, term: 1 }).unwrap();
    assert_eq!(s.retain_files().unwrap().blocks_removed, 1);
    drop(s);
    let s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    assert_eq!(s.series_count(), 1);
    assert_eq!(s.cutoff(), 1000);
}

#[test]
fn logs_ride_batches_through_cuts_a_reopen_and_retention() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    let rec = |i: i64| Record { ts: i * 1_000_000_000, severity: if i % 7 == 0 { 17 } else { 9 }, service: "api".into(), host: "h".into(), source: "kiln".into(), message: format!("event {i} {}", if i % 7 == 0 { "boom" } else { "fine" }), ..Record::default() };
    let mut b = Builder::new();
    for batch in 0..20 {
        let recs: Vec<Record> = (batch * 1000..(batch + 1) * 1000).map(rec).collect();
        b.logs(&recs);
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!(applied.counts.logs, 1000);
        if batch % 6 == 5 {
            s.cut(Point { index: batch as u64 + 1, term: 1 }).unwrap();
        }
    }
    let q = crate::logs::search::parse("status:error boom").unwrap();
    let newest = |s: &Store| s.search_logs(&q, 0, i64::MAX, 3).unwrap().records.iter().map(|r| r.ts / 1_000_000_000).collect::<Vec<_>>();
    assert_eq!(newest(&s), vec![19_999, 19_992, 19_985]);
    // After a restart: the files reopen; the batches past the durable point come again.
    let durable = s.durable();
    drop(s);
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    for batch in (durable.index as i64)..20 {
        let recs: Vec<Record> = (batch * 1000..(batch + 1) * 1000).map(rec).collect();
        b.logs(&recs);
        s.apply(&b.finish()).unwrap();
    }
    assert_eq!(newest(&s), vec![19_999, 19_992, 19_985]);
    assert!(s.log_bytes() > 0);
    // Retention: nothing before the cutoff is found at once; files wholly before it (0–5,999 and
    // 6,000–11,999 s) go once it is durable, and the one holding 12,000–17,999 stays whole.
    s.retain_logical(13_000_000, 13_000_000);
    let all = crate::logs::search::parse("*").unwrap();
    assert!(s.search_logs(&all, 0, 12_999_999_999_999, 1).unwrap().records.is_empty());
    assert_eq!(s.search_logs(&all, 0, 13_000_000_000_000, 1).unwrap().records.len(), 1);
    s.cut(Point { index: 20, term: 1 }).unwrap();
    let before = s.log_bytes();
    s.retain_files().unwrap();
    assert!(s.log_bytes() < before);
    assert_eq!(s.search_logs(&all, 0, 13_000_000_000_000, 1).unwrap().records.len(), 1);
}

/// Parts of the logs (an index's; those not kept for search): each part's rows to files of its
/// own at a cut, searched as the parts kept for search are (those not kept, apart), every part
/// read for metrics from logs; a part's retention its own and never back, its files gone by it
/// alone; a reopen keeps them all.
#[test]
fn parts_of_the_logs_are_kept_read_and_retained_apart() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    let rec = |i: i64, what: &str| Record { ts: i * 1_000_000_000, severity: 9, service: "api".into(), host: "h".into(), source: "app".into(), message: format!("{what} {i}"), ..Record::default() };
    let put = |b: &mut Builder, part: u16, recs: &[Record]| {
        let mut raw = Vec::new();
        let mut prev = 0;
        for r in recs {
            crate::logs::record::put_row(&mut raw, r, prev);
            prev = r.ts;
        }
        b.log_rows_in(part, &raw, recs.len() as u32);
    };
    let mut b = Builder::new();
    for i in 0..10 {
        put(&mut b, crate::logs::store::MAIN, &[rec(i, "main")]);
        put(&mut b, INDEX, &[rec(i, "audit")]);
        put(&mut b, crate::logs::store::TRANSIT, &[rec(i, "dropped")]);
        assert_eq!(s.apply(&b.finish()).unwrap().counts.logs, 3);
        if i == 4 {
            s.cut(Point { index: 1, term: 1 }).unwrap();
        }
    }
    let all = crate::logs::search::parse("*").unwrap();
    let count = |at: &LogsAt, what: &str| {
        let mut n = 0;
        at.each(&all, i64::MIN, i64::MAX, |r| n += usize::from(r.message.starts_with(what))).unwrap();
        n
    };
    assert_eq!((count(&s.logs_at(), "main"), count(&s.logs_at(), "audit"), count(&s.logs_at(), "dropped")), (10, 10, 0), "searched: those kept for search");
    assert_eq!(count(&s.logs_every(), "dropped"), 10, "every part, for metrics from logs");
    let newest: Vec<i64> = s.search_logs(&all, i64::MIN, i64::MAX, 3).unwrap().records.iter().map(|r| r.ts / 1_000_000_000).collect();
    assert_eq!(newest, [9, 9, 8], "newest first across parts");
    s.cut(Point { index: 2, term: 1 }).unwrap();
    let mut parts: Vec<u16> = s.logs.files().iter().map(|f| f.part).collect();
    parts.sort_unstable();
    parts.dedup();
    assert_eq!(parts, [crate::logs::store::MAIN, INDEX, crate::logs::store::TRANSIT], "a file a part");
    // The index's retention its own: its records before 6 s go, the others' stay; never back.
    assert_eq!(s.apply_entry(3, &crate::command::retain_part(INDEX, 6_000, 0)), Ok(Answer::RetainPart { part: INDEX, logs_cutoff: 6_000 }));
    s.apply_entry(4, &crate::command::retain_part(INDEX, 2_000, 0)).unwrap();
    assert_eq!(s.part_cutoff(INDEX), 6_000, "never back");
    assert_eq!((count(&s.logs_at(), "main"), count(&s.logs_at(), "audit")), (10, 4));
    // Its files go by its cutoff alone once durable: its first (0–4 s), none of the others'.
    s.cut(Point { index: 4, term: 1 }).unwrap();
    let before = s.logs.files().len();
    s.retain_files().unwrap();
    assert_eq!(s.logs.files().len(), before - 1);
    assert!(s.logs.files().iter().all(|f| f.part != INDEX || f.log.max_ts >= 6_000_000_000));
    drop(s);
    let s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    assert_eq!(s.part_cutoffs().get(&INDEX), Some(&6_000), "reopened");
    assert_eq!((count(&s.logs_at(), "main"), count(&s.logs_at(), "audit"), count(&s.logs_every(), "dropped")), (10, 4, 10));
}

/// Everything a replica answers, by the series' own keys: every series' samples, the names and
/// values autocomplete gives, the replicated state, every log record in search order, and the
/// answers to queries, to the bit.
#[derive(Debug, PartialEq)]
struct View {
    series: Vec<(String, Vec<(i64, u64)>)>,
    names: Vec<String>,
    hosts: Vec<String>,
    cutoff: i64,
    logs_cutoff: i64,
    max_series: u64,
    sessions: BTreeMap<u64, Session>,
    logs: Vec<Record>,
    /// Each part's cutoff, an index's records, and those not kept for search.
    part_cutoffs: BTreeMap<u16, i64>,
    index_logs: Vec<Record>,
    transit: Vec<Record>,
    spans_cutoff: i64,
    spans: Vec<Record>,
    /// One trace's spans, found by its id (a block of spans ruled out by its filter).
    trace: Vec<Record>,
    queries: Vec<Vec<(Vec<(String, String)>, Vec<u64>)>>,
}

/// The trace the stream's spans come back to most.
const TRACE: [u8; 16] = [7; 16];

const QUERIES: [&str; 4] = ["avg:m0{*} by {host}", "sum:m1{*}", "p90:m2{*} by {host}", "sum:flip{*}.as_count()"];

fn view(s: &Store) -> View {
    let mut series = Vec::new();
    for name in s.metric_names() {
        series.extend(read_all(s, &Selector::metric(name)));
    }
    series.sort();
    let all = crate::logs::search::parse("*").unwrap();
    let queries = QUERIES
        .iter()
        .map(|q| {
            let a = crate::query::eval::eval(s, &crate::query::eval::Request::new(q, 0, 400_000)).unwrap();
            a.series.into_iter().map(|x| (x.tags, x.values.into_iter().map(f64::to_bits).collect())).collect()
        })
        .collect();
    View {
        series,
        names: s.metric_names().into_iter().map(str::to_owned).collect(),
        hosts: s.tag_values("host").into_iter().map(str::to_owned).collect(),
        cutoff: s.cutoff(),
        logs_cutoff: s.logs_cutoff(),
        max_series: s.max_series(),
        sessions: s.sessions().clone(),
        logs: s.search_logs(&all, i64::MIN, i64::MAX, usize::MAX).unwrap().records,
        part_cutoffs: s.part_cutoffs().clone(),
        index_logs: s.logs_scoped(&|p| p == INDEX).search(&all, i64::MIN, i64::MAX, usize::MAX).unwrap().records,
        transit: s.logs_scoped(&|p| p == crate::logs::store::TRANSIT).search(&all, i64::MIN, i64::MAX, usize::MAX).unwrap().records,
        spans_cutoff: s.spans_cutoff(),
        spans: s.search_spans(&all, i64::MIN, i64::MAX, usize::MAX).unwrap().records,
        trace: s.search_spans(&crate::logs::search::Search::trace(TRACE), i64::MIN, i64::MAX, usize::MAX).unwrap().records,
        queries,
    }
}

/// The part of the logs a stream with parts writes an index's records to.
const INDEX: u16 = 3;

/// [`stream_with`], its logs of one part, as a store's were before parts.
fn stream(seed: u64, n: usize) -> Vec<Vec<u8>> {
    stream_with(seed, n, false)
}

/// A stream of entries: writes from three senders (some sent twice, as by a sender whose answer
/// was lost), late samples and corrections, log records several to a time, empty entries,
/// retention moving forward, a bound on series, and a series that expires and comes back of
/// another kind. With `parts`, log records of an index's part and of those not kept for search
/// too, and each of those parts' retention moving forward.
fn stream_with(seed: u64, n: usize, parts: bool) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    let mut seqs = [0u64; 3];
    let mut sent: [Option<Vec<u8>>; 3] = [None, None, None];
    let mut b = Builder::new();
    for round in 0..n {
        let now = round as i64 * 1000;
        // A bound on series for a while, lifted before the flipped series comes back.
        if round == n / 2 {
            out.push(crate::command::limits(45));
        }
        if round == n * 3 / 4 - 10 {
            out.push(crate::command::limits(0));
        }
        match rng.below(25) {
            0 => out.push(crate::command::retain(now - 60_000 + rng.below(10_000) as i64, now - 40_000 + rng.below(10_000) as i64, now)),
            3 => out.push(crate::command::retain_spans(now - 50_000 + rng.below(10_000) as i64, now)),
            4 if parts => out.push(crate::command::retain_part(INDEX, now - 30_000 + rng.below(10_000) as i64, now)),
            5 if parts => out.push(crate::command::retain_part(crate::logs::store::TRANSIT, now - 20_000 + rng.below(10_000) as i64, now)),
            1 => {
                let i = rng.below(3) as usize;
                if let Some(e) = &sent[i] {
                    out.push(e.clone());
                }
            }
            2 => out.push(Vec::new()),
            _ => {
                let sender = rng.below(3) as usize;
                for _ in 0..1 + rng.below(6) {
                    let k = gauge(&format!("m{}", rng.below(3)), &format!("h{}", rng.below(20)));
                    if rng.below(5) != 0 {
                        b.define(&k);
                    }
                    let samples: Vec<(i64, f64)> = (0..1 + rng.below(5) as i64)
                        .map(|i| {
                            let t = match rng.below(10) {
                                // Late, some past the cutoff.
                                0 => now - rng.below(90_000) as i64,
                                // A correction of a recent time.
                                1 => now - (rng.below(10) as i64) * 1000,
                                _ => now + i * 100,
                            };
                            (t, (rng.below(1000) as f64) / 8.0)
                        })
                        .collect();
                    b.run(k.fp, samples);
                }
                // A series written early, then never until retention takes it, then again as a
                // count: the store must hold it as the count, however its blocks fell.
                if round < 20 || round > n * 3 / 4 {
                    let kind = if round < 20 { Kind::Gauge } else { Kind::Count };
                    let k = SeriesKey::new("flip", [("host", "x")], kind).unwrap();
                    b.define(&k);
                    b.run(k.fp, [(now, 1.0)]);
                }
                if rng.below(3) == 0 {
                    let recs: Vec<Record> = (0..1 + rng.below(30))
                        .map(|i| Record {
                            // Several records to a time: the search's order among them is the store's.
                            ts: (now + (i as i64 / 3)) * 1_000_000,
                            severity: [9, 13, 17][rng.below(3) as usize],
                            service: ["api", "web"][rng.below(2) as usize].into(),
                            host: format!("h{}", rng.below(4)),
                            source: "kiln".into(),
                            message: format!("event {} {}", rng.below(50), ["ok", "failed", "slow"][rng.below(3) as usize]),
                            ..Record::default()
                        })
                        .collect();
                    if parts {
                        let part = match rng.below(20) {
                            0..=11 => crate::logs::store::MAIN,
                            12..=16 => INDEX,
                            _ => crate::logs::store::TRANSIT,
                        };
                        let mut raw = Vec::new();
                        let mut prev = 0;
                        for r in &recs {
                            crate::logs::record::put_row(&mut raw, r, prev);
                            prev = r.ts;
                        }
                        b.log_rows_in(part, &raw, recs.len() as u32);
                    } else {
                        b.logs(&recs);
                    }
                }
                if rng.below(3) == 0 {
                    // Spans, several to a time, of a few traces, one of them most.
                    let mut raw = Vec::new();
                    let mut prev = 0;
                    let n = 1 + rng.below(20) as u32;
                    for i in 0..n {
                        let trace = if rng.below(4) == 0 { TRACE } else { [rng.below(50) as u8 + 8; 16] };
                        let r = Record {
                            ts: (now + i64::from(i / 4)) * 1_000_000,
                            severity: [9, 17][rng.below(2) as usize],
                            service: ["checkout", "cart"][rng.below(2) as usize].into(),
                            host: format!("h{}", rng.below(4)),
                            source: "otel".into(),
                            message: ["GET /cart", "SELECT carts"][rng.below(2) as usize].into(),
                            attrs: vec![("span.kind".into(), "server".into()), ("duration".into(), rng.below(1_000_000).to_string())],
                            trace_id: Some(trace),
                            span_id: Some((rng.next_u64() | 1).to_le_bytes()),
                        };
                        crate::logs::record::put_row(&mut raw, &r, prev);
                        prev = r.ts;
                    }
                    b.spans(&raw, n);
                }
                seqs[sender] += 1;
                let e = b.finish_write(0x5e4d_0000 + sender as u64, seqs[sender], now);
                sent[sender] = Some(e.clone());
                out.push(e);
            }
        }
    }
    out
}

/// Applies entries `from..to`; the series their bound refused.
fn apply_at(s: &mut Store, entries: &[Vec<u8>], from: usize, to: usize) -> usize {
    let mut over = 0;
    for (i, e) in entries.iter().enumerate().take(to).skip(from) {
        if let Answer::Write { outcome: Written::Applied(a), .. } = s.apply_entry(i as u64 + 1, e).unwrap() {
            over += a.over_limit.len();
        }
    }
    over
}

#[test]
fn replicas_that_cut_compact_retain_and_restart_differently_answer_alike() {
    // `KILN_OBS_SEEDS=n` runs n seeds (a soak); three by default.
    let seeds: u64 = std::env::var("KILN_OBS_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let mut refused_in_all = 0;
    for seed in 1..=seeds {
        let entries = stream_with(seed, 400, true);
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // C: in memory, never cut. A: cut every 7 entries, compacted and retained each time.
        // B: cut, compacted, retained and restarted at random.
        let mut c = Store::new(Keyed::with_keys(1, 1));
        let mut a = Store::open(da.path(), Keyed::with_keys(2, 2)).unwrap();
        let mut b = Store::open(db.path(), Keyed::with_keys(3, 3)).unwrap();
        let mut rng = Rng::new(seed ^ 0xb);
        let (mut restarts, mut over) = (0, 0);
        for i in 0..entries.len() {
            over += apply_at(&mut c, &entries, i, i + 1);
            apply_at(&mut a, &entries, i, i + 1);
            apply_at(&mut b, &entries, i, i + 1);
            let at = Point { index: i as u64 + 1, term: 1 };
            if i % 7 == 6 {
                a.cut(at).unwrap();
                a.compact().unwrap();
                a.retain_files().unwrap();
            }
            if rng.below(10) == 0 {
                b.cut(at).unwrap();
                if rng.below(3) == 0 {
                    b.compact().unwrap();
                }
                if rng.below(3) == 0 {
                    b.retain_files().unwrap();
                }
            }
            if rng.below(20) == 0 {
                // B's process starts again: it opens what is durable and the log brings the rest.
                drop(b);
                b = Store::open(db.path(), Keyed::with_keys(rng.next_u64(), 7)).unwrap();
                let durable = b.durable().index as usize;
                apply_at(&mut b, &entries, durable, i + 1);
                restarts += 1;
            }
            if i % 25 == 24 || i + 1 == entries.len() {
                let want = view(&c);
                assert_eq!(view(&a), want, "seed {seed}: A after entry {}", i + 1);
                assert_eq!(view(&b), want, "seed {seed}: B after entry {}", i + 1);
            }
        }
        for full in [false, true] {
            let want = c.digest(full).unwrap();
            assert_eq!(a.digest(full).unwrap(), want, "seed {seed}: A's digest, full {full}");
            assert_eq!(b.digest(full).unwrap(), want, "seed {seed}: B's digest, full {full}");
        }
        let v = view(&c);
        assert!(restarts > 5 && v.cutoff > 0 && !v.logs.is_empty(), "seed {seed}: the stream exercised too little ({restarts} restarts)");
        assert!(v.spans_cutoff > 0 && !v.spans.is_empty() && !v.trace.is_empty() && v.trace.len() < v.spans.len(), "seed {seed}: the spans exercised too little");
        assert!(v.trace.iter().all(|r| r.trace_id == Some(TRACE)));
        refused_in_all += over;
        assert!(v.series.iter().any(|(name, _)| name.starts_with("flip") && name.ends_with("Count")), "seed {seed}: the flipped series is the count");
    }
    assert!(refused_in_all > 0, "the bound on series never refused one");
}

#[test]
fn an_image_installed_elsewhere_answers_as_its_source() {
    let entries = stream_with(11, 300, true);
    let (ds, dt, staging, snaps) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut source = Store::open(ds.path(), Keyed::with_keys(1, 2)).unwrap();
    let mut target = Store::open(dt.path(), Keyed::with_keys(3, 4)).unwrap();
    // The target holds state of its own, which the image replaces.
    apply_at(&mut target, &entries, 0, 40);
    target.cut(Point { index: 40, term: 1 }).unwrap();
    for i in 0..200 {
        apply_at(&mut source, &entries, i, i + 1);
        if i % 30 == 29 {
            source.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
            source.compact().unwrap();
        }
    }
    source.cut(Point { index: 200, term: 1 }).unwrap();
    let image = source.image();
    let snap = source.link_snapshot(snaps.path()).unwrap();
    // The source compacts and retains after imaging: the snapshot keeps the files it names.
    apply_at(&mut source, &entries, 200, 260);
    source.cut(Point { index: 260, term: 1 }).unwrap();
    source.compact().unwrap();
    source.retain_files().unwrap();
    let mut bytes = Vec::new();
    image.encode(&mut bytes);
    let image = Image::decode(&bytes).unwrap();
    // What a member fetching the image does: each file to the staging folder, checked whole.
    for f in &image.files {
        let to = staging.path().join(&f.name);
        std::fs::copy(snap.join(&f.name), &to).unwrap();
        match f.kind {
            FileKind::Block => Block::open(&to).unwrap().verify().unwrap(),
            FileKind::Logs | FileKind::Spans => LogFile::open(&to).unwrap().verify().unwrap(),
        }
    }
    // A file missing or not the image's is refused, and the target is as it was.
    let first = image.files.first().unwrap();
    std::fs::rename(staging.path().join(&first.name), staging.path().join("aside")).unwrap();
    let conf = Membership { voters: vec![1], learners: vec![2], ..Membership::default() };
    assert!(matches!(target.install(staging.path(), &image, Point { index: 200, term: 1 }, conf.clone()), Err(StoreError::Staged(_))));
    assert_eq!(target.durable().index, 40);
    std::fs::rename(staging.path().join("aside"), staging.path().join(&first.name)).unwrap();
    target.install(staging.path(), &image, Point { index: 200, term: 1 }, conf.clone()).unwrap();
    assert_eq!((target.durable(), target.durable_membership()), (Point { index: 200, term: 1 }, &conf));
    // The reference: every entry through 200, then on to the end, alike.
    let mut reference = Store::new(Keyed::with_keys(9, 9));
    apply_at(&mut reference, &entries, 0, 200);
    assert_eq!(view(&target), view(&reference), "as installed");
    apply_at(&mut target, &entries, 200, entries.len());
    apply_at(&mut reference, &entries, 200, entries.len());
    apply_at(&mut source, &entries, 260, entries.len());
    assert_eq!(view(&target), view(&reference), "after the rest of the log");
    assert_eq!(view(&source), view(&reference), "the source too");
    // And after the target restarts.
    target.cut(Point { index: entries.len() as u64, term: 1 }).unwrap();
    drop(target);
    let target = Store::open(dt.path(), Keyed::with_keys(5, 5)).unwrap();
    assert_eq!(view(&target), view(&reference), "reopened");
    // Snapshots are pruned to the newest two.
    for i in 0..3 {
        source.cut(Point { index: 1000 + i, term: 2 }).unwrap();
        source.link_snapshot(snaps.path()).unwrap();
    }
    assert_eq!(std::fs::read_dir(snaps.path()).unwrap().count(), KEEP_SNAPSHOTS);
}

#[test]
fn rewrites_built_apart_while_the_store_goes_on_answer_alike() {
    let seeds: u64 = std::env::var("KILN_OBS_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    for seed in 1..=seeds {
        let entries = stream_with(seed + 100, 400, true);
        let d = tempfile::tempdir().unwrap();
        let mut c = Store::new(Keyed::with_keys(1, 1));
        let mut s = Store::open(d.path(), Keyed::with_keys(2, 2)).unwrap();
        let mut rng = Rng::new(seed);
        // Rewrites planned and not yet built: the store goes on applying and cutting meanwhile.
        let mut planned: Vec<Rewrite> = Vec::new();
        let mut taken = 0;
        for i in 0..entries.len() {
            apply_at(&mut c, &entries, i, i + 1);
            apply_at(&mut s, &entries, i, i + 1);
            if rng.below(6) == 0 {
                s.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
                s.drop_expired().unwrap();
            }
            if rng.below(4) == 0
                && let Some(p) = if rng.below(2) == 0 { s.plan_merge() } else { s.plan_retention() }
            {
                planned.push(p);
            }
            if !planned.is_empty() && rng.below(3) == 0 {
                // A plan another rewrite overtook cannot be built: it is dropped, as maintenance
                // drops it.
                let p = planned.remove(rng.below(planned.len() as u64) as usize);
                if let Ok(built) = p.build() {
                    taken += s.commit(built).unwrap().blocks_removed;
                }
            }
            if i % 50 == 49 {
                assert_eq!(view(&s), view(&c), "seed {seed} after entry {}", i + 1);
            }
        }
        for p in planned.drain(..) {
            if let Ok(built) = p.build() {
                s.commit(built).unwrap();
            }
        }
        assert_eq!(view(&s), view(&c), "seed {seed} at the end");
        assert!(taken > 0, "seed {seed}: some rewrite was taken in");
        // Nothing left behind: every block file is the manifest's.
        let files: BTreeSet<String> = std::fs::read_dir(d.path()).unwrap().filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".block")).collect();
        assert_eq!(files.len(), s.block_count(), "seed {seed}: orphan blocks {files:?}");
        drop(s);
        let s = Store::open(d.path(), Keyed::with_keys(3, 3)).unwrap();
        let mut r = Store::new(Keyed::with_keys(4, 4));
        apply_at(&mut r, &entries, 0, s.durable().index as usize);
        assert_eq!(view(&s), view(&r), "seed {seed} reopened");
    }
}

#[test]
fn a_digest_tells_apart_stores_that_differ_in_any_sample_or_record() {
    let entries = stream(5, 200);
    let mut a = Store::new(Keyed::with_keys(1, 1));
    let mut b = Store::new(Keyed::with_keys(2, 2));
    apply_at(&mut a, &entries, 0, entries.len());
    apply_at(&mut b, &entries, 0, entries.len());
    assert_eq!(a.digest(true).unwrap(), b.digest(true).unwrap());
    // One sample's value corrected on one of them: the full digest differs, the index's does not.
    let mut bld = Builder::new();
    let mut ids = Vec::new();
    b.select(&Selector::metric("m0"), &mut ids);
    let r = b.series(ids[0]).unwrap();
    let k = SeriesKey::new(r.metric(), r.tags().collect::<Vec<_>>(), r.kind()).unwrap();
    let (mut ts, mut vs) = (Vec::new(), Vec::new());
    b.read(ids[0], i64::MIN, i64::MAX, &mut ts, &mut vs).unwrap();
    bld.run(k.fp, [(ts[0], vs[0] + 1.0)]);
    assert_eq!(b.apply(&bld.finish()).unwrap().counts.late, 1);
    assert_eq!(a.digest(false).unwrap(), b.digest(false).unwrap());
    assert_ne!(a.digest(true).unwrap(), b.digest(true).unwrap());
    // One log record more, past the logs' cutoff: the full digest differs. One older than the
    // cutoff is never seen, and counts for nothing.
    let mut c = Store::new(Keyed::with_keys(3, 3));
    apply_at(&mut c, &entries, 0, entries.len());
    bld.logs(&[Record { ts: 1, message: "too old".into(), ..Record::default() }]);
    c.apply(&bld.finish()).unwrap();
    assert_eq!(a.digest(true).unwrap(), c.digest(true).unwrap());
    bld.logs(&[Record { ts: (c.logs_cutoff() + 1) * 1_000_000, message: "one more".into(), ..Record::default() }]);
    c.apply(&bld.finish()).unwrap();
    assert_ne!(a.digest(true).unwrap(), c.digest(true).unwrap());
}

/// The full digest as it was before records were visited one at a time (gathered by a search,
/// newest first): what a replica of an older version says; and the spans', which none held.
fn digest_before(st: &Store) -> u64 {
    let mix = |x: u64| {
        let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    let mut d = mix(st.cutoff as u64).wrapping_add(mix(st.logs_cutoff as u64 ^ 0x1)).wrapping_add(mix(st.max_series ^ 0x2));
    for (sender, s) in &st.sessions {
        d = d.wrapping_add(mix(mix(*sender) ^ s.seq));
    }
    let mut scratch = Scratch::default();
    let (mut ts, mut vs) = (Vec::new(), Vec::new());
    for id in 0..st.index.len() as u32 {
        let Some(fp) = st.index.fp(id) else { continue };
        let last = st.last.get(id as usize).copied().unwrap_or(i64::MIN);
        let mut term = mix(fp ^ mix(last as u64));
        ts.clear();
        vs.clear();
        st.read_with(id, i64::MIN, i64::MAX, &mut scratch, &mut ts, &mut vs).unwrap();
        for (&t, &v) in ts.iter().zip(&vs) {
            term = mix(term ^ mix(t as u64) ^ v.to_bits());
        }
        d = d.wrapping_add(term);
    }
    let all = crate::logs::search::Search::all();
    for r in st.search_logs(&all, i64::MIN, i64::MAX, usize::MAX).unwrap().records {
        let mut row = Vec::new();
        crate::logs::record::put_row(&mut row, &r, 0);
        d = d.wrapping_add(mix(crate::crc32c::checksum(&row) as u64 ^ mix(r.ts as u64)));
    }
    if st.spans_cutoff != i64::MIN {
        d = d.wrapping_add(mix(st.spans_cutoff as u64 ^ 0x3));
    }
    for r in st.search_spans(&all, i64::MIN, i64::MAX, usize::MAX).unwrap().records {
        let mut row = Vec::new();
        crate::logs::record::put_row(&mut row, &r, 0);
        d = d.wrapping_add(mix(mix(crate::crc32c::checksum(&row) as u64 ^ mix(r.ts as u64)) ^ 0x5));
    }
    d
}

/// A full digest visits each record once and keeps none (a store of millions of records took a
/// search of all of them, sorted as it went, before: hours); its sum is the one of before, so
/// replicas of two versions agree.
#[test]
fn a_full_digest_visits_each_record_and_sums_as_before() {
    for seed in 0..20 {
        let entries = stream(seed, 150);
        let mut a = Store::new(Keyed::with_keys(1, 1));
        apply_at(&mut a, &entries, 0, entries.len());
        let mut rng = Rng::new(seed);
        let mut bld = Builder::new();
        let records: Vec<Record> = (0..500)
            .map(|i| Record {
                // Some before the cutoff, some at one time, attributes and ids now and then.
                ts: (a.logs_cutoff() + rng.below(1_000) as i64 - 100) * 1_000_000 + (i % 7),
                severity: (rng.below(24) + 1) as u8,
                service: format!("svc{}", rng.below(4)),
                host: "h".into(),
                message: format!("record {i} {}", rng.below(1_000_000)),
                attrs: (0..rng.below(4)).map(|k| (format!("k{k}"), format!("v{}", rng.below(9)))).collect(),
                trace_id: (i % 11 == 0).then_some([7; 16]),
                ..Record::default()
            })
            .collect();
        bld.logs(&records);
        let batch = bld.finish();
        a.apply(&batch).unwrap();
        assert_eq!(a.digest(true).unwrap(), digest_before(&a), "seed {seed}");
        // A store that never held spans: the older version's sum exactly, for both its digests.
        let mut b = Store::new(Keyed::with_keys(2, 2));
        b.apply(&batch).unwrap();
        assert!(b.spans_cutoff() == i64::MIN && b.span_bytes() == 0);
        assert_eq!(b.digest(true).unwrap(), digest_before(&b), "seed {seed}: no spans");
    }
}

#[test]
fn a_rewrite_planned_before_an_install_is_not_taken_in_after() {
    let entries = stream(7, 200);
    let (da, db, staging, snaps) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = Store::open(da.path(), Keyed::with_keys(1, 1)).unwrap();
    for i in 0..120 {
        apply_at(&mut a, &entries, i, i + 1);
        if i % 20 == 19 {
            a.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
        }
    }
    let mut b = Store::open(db.path(), Keyed::with_keys(2, 2)).unwrap();
    for i in 0..60 {
        apply_at(&mut b, &entries, i, i + 1);
        if i % 10 == 9 {
            b.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
        }
    }
    let plan = b.plan_merge().expect("B has blocks to merge");
    // B installs A's image meanwhile.
    let image = a.image();
    let snap = a.link_snapshot(snaps.path()).unwrap();
    for f in &image.files {
        std::fs::copy(snap.join(&f.name), staging.path().join(&f.name)).unwrap();
    }
    b.install(staging.path(), &image, a.durable(), Membership::default()).unwrap();
    let before = view(&b);
    // The plan's inputs are gone (or of another store): its build fails or is not taken in.
    if let Ok(built) = plan.build() {
        assert_eq!(b.commit(built).unwrap(), Maintained::default());
    }
    assert_eq!(view(&b), before);
    let blocks = std::fs::read_dir(db.path()).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().ends_with(".block")).count();
    assert_eq!(blocks, b.block_count(), "the rewrite's file was deleted");
}

#[test]
fn a_maintenance_thread_rewrites_while_the_store_applies_and_cuts() {
    use std::sync::{Arc, RwLock};
    let entries = stream(21, 600);
    let d = tempfile::tempdir().unwrap();
    let shared = Arc::new(RwLock::new(Store::open(d.path(), Keyed::with_keys(1, 1)).unwrap()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let (shared, stop) = (shared.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut taken = 0u64;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                // Planned under the lock, built without it, taken in under it.
                let plan = {
                    let mut s = shared.write().unwrap();
                    s.drop_expired().unwrap();
                    s.plan_merge().or_else(|| s.plan_retention())
                };
                match plan {
                    Some(p) => {
                        let built = p.build().unwrap();
                        taken += shared.write().unwrap().commit(built).unwrap().blocks_removed;
                    }
                    None => std::thread::yield_now(),
                }
            }
            taken
        })
    };
    let mut c = Store::new(Keyed::with_keys(2, 2));
    for i in 0..entries.len() {
        apply_at(&mut c, &entries, i, i + 1);
        let mut s = shared.write().unwrap();
        apply_at(&mut s, &entries, i, i + 1);
        if i % 5 == 4 {
            s.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
        }
        drop(s);
        if i % 100 == 99 {
            // A reader meanwhile, as a query.
            assert_eq!(view(&shared.read().unwrap()), view(&c), "after entry {}", i + 1);
        }
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let taken = worker.join().unwrap();
    assert!(taken > 0, "the thread took rewrites in");
    assert_eq!(view(&shared.read().unwrap()), view(&c));
}

/// The files changed beside queries (`cut_shared`, `plan_shared`, `commit_shared`,
/// `drop_expired_shared`): one thread applies and cuts as the group's does, another merges,
/// rewrites and expires as maintenance does, two read throughout as queries do. Every view is the
/// reference store's, and so is the store opened again from its files with the entries after its
/// durable point applied.
#[test]
fn files_changed_beside_queries_answer_as_one_store_and_open_again_alike() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, RwLock};
    let entries = stream_with(23, 900, true);
    let d = tempfile::tempdir().unwrap();
    let shared = Arc::new(RwLock::new(Store::open(d.path(), Keyed::with_keys(1, 1)).unwrap()));
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let maintenance = {
        let (shared, stop) = (shared.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut taken = 0u64;
            while !stop.load(Ordering::SeqCst) {
                crate::store::drop_expired_shared(&shared).unwrap();
                match crate::store::plan_shared(&shared) {
                    Some(p) => {
                        if let Ok(built) = p.build() {
                            taken += crate::store::commit_shared(&shared, built).unwrap().blocks_removed;
                        }
                    }
                    None => std::thread::yield_now(),
                }
            }
            taken
        })
    };
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let (shared, stop, reads) = (shared.clone(), stop.clone(), reads.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let s = shared.read().unwrap();
                    crate::query::eval::eval(&s, &crate::query::eval::Request::new("sum:m0{*} by {host}", 0, 1_000_000)).unwrap();
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let mut c = Store::new(Keyed::with_keys(2, 2));
    for i in 0..entries.len() {
        apply_at(&mut c, &entries, i, i + 1);
        apply_at(&mut shared.write().unwrap(), &entries, i, i + 1);
        if i % 5 == 4 {
            crate::store::cut_shared(&shared, Point { index: i as u64 + 1, term: 1 }).unwrap();
        }
        if i % 150 == 149 {
            assert_eq!(view(&shared.read().unwrap()), view(&c), "after entry {}", i + 1);
        }
    }
    stop.store(true, Ordering::SeqCst);
    let taken = maintenance.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
    assert!(taken > 0 && reads.load(Ordering::Relaxed) > 0, "rewrites taken in ({taken}) and reads made");
    assert_eq!(view(&shared.read().unwrap()), view(&c));
    // Opened again from its files: the durable point's state, then the entries after it.
    let durable = shared.read().unwrap().durable().index;
    drop(shared);
    let mut again = Store::open(d.path(), Keyed::with_keys(3, 3)).unwrap();
    apply_at(&mut again, &entries, durable as usize, entries.len());
    assert_eq!(view(&again), view(&c), "opened again at {durable}");
}

/// Cuts as a busy group makes them, a big one and then a small one (a compaction's, then a
/// snapshot's), and the logarithmic method's merges after each: the blocks stay a handful, each
/// more than twice the next newer, however long it goes on. (Merging only the newest pair stalled
/// here: a small cut always sat at the newest end, so equal blocks behind it never merged.)
#[test]
fn alternating_big_and_small_cuts_still_merge_into_a_handful_of_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::new()).unwrap();
    let keys: Vec<SeriesKey> = (0..500).map(|i| SeriesKey::new("load.v", [("s", &i.to_string()[..])], Kind::Gauge).unwrap()).collect();
    let (mut index, mut t) = (0u64, 0i64);
    let mut most = 0;
    for cut in 0..64 {
        // A big cut is 20 batches, a small one 2.
        for _ in 0..(if cut % 2 == 0 { 20 } else { 2 }) {
            let mut b = Builder::new();
            for k in &keys {
                if index == 0 {
                    b.define(k);
                }
                b.run(k.fp, (0..20).map(|j| (t + j, (t + j) as f64 * 0.5)));
            }
            t += 20;
            index += 1;
            s.apply(&b.finish()).unwrap();
        }
        s.cut(Point { index, term: 1 }).unwrap();
        s.compact().unwrap();
        most = most.max(s.block_count());
        let sizes: Vec<u64> = s.block_list().iter().map(|b| b.2).collect();
        // Every pair but the newest end's keeps the invariant (the newest may be a small cut).
        for w in sizes.windows(2) {
            assert!(w[1] * 2 < w[0], "cut {cut}: blocks {sizes:?}");
        }
    }
    // 64 cuts of 704 batches in all: log2 of it, give or take.
    assert!(most <= 12, "{most} blocks at the most");
    let mut ids = Vec::new();
    s.select(&crate::index::Selector::metric("load.v"), &mut ids);
    assert_eq!(ids.len(), 500);
    assert_eq!(s.totals().samples, index * 500 * 20);
}

/// Live tail: the log records entries apply reach whoever follows, each once: the entries a store
/// opened again applies again (those after its last cut) are not published twice, and a store that
/// installs an image keeps its feed.
#[test]
fn applied_log_records_reach_a_follower_once_through_a_reopen_and_an_image() {
    use crate::logs::tail::{Feed, Follower, Sample};
    let (dir, other, snaps, staging) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let rec = |i: i64| Record { ts: i * 1_000_000_000, severity: 9, service: "api".into(), host: "h".into(), source: "kiln".into(), message: format!("event {i}"), ..Record::default() };
    let entry = |seq: u64| {
        let mut b = Builder::new();
        let from = (seq as i64 - 1) * 2;
        b.logs(&(from..from + 2).map(rec).collect::<Vec<_>>());
        crate::command::write(7, seq, 0, &b.finish())
    };
    let all = crate::logs::search::parse("*").unwrap();
    let read = |f: &mut Follower| {
        let mut sample = Sample::new(100, 1);
        f.read(&all, &mut sample).unwrap();
        sample.records().map(|r| r.message.clone()).collect::<Vec<_>>()
    };
    let feed = Feed::new();
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    s.set_feed(Some(feed.clone()));
    // Applied while no one follows: nothing kept for anyone.
    s.apply_entry(1, &entry(1)).unwrap();
    assert_eq!(feed.held_bytes(), 0);
    let mut f = feed.follow().unwrap();
    s.apply_entry(2, &entry(2)).unwrap();
    s.cut(Point { index: 2, term: 1 }).unwrap();
    s.apply_entry(3, &entry(3)).unwrap();
    assert_eq!(read(&mut f), ["event 2", "event 3", "event 4", "event 5"]);
    // A crash: opened again, the store applies entry 3 again (after its cut), the follower sees
    // nothing new; entry 4 is.
    drop(s);
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    s.set_feed(Some(feed.clone()));
    s.apply_entry(3, &entry(3)).unwrap();
    assert!(read(&mut f).is_empty());
    s.apply_entry(4, &entry(4)).unwrap();
    assert_eq!(read(&mut f), ["event 6", "event 7"]);
    // The entry applied again was the store's all the same.
    assert_eq!(s.search_logs(&all, 0, i64::MAX, 100).unwrap().records.len(), 8);
    // Another store installs this one's image: its feed stays, and what it applies next is
    // published.
    s.cut(Point { index: 4, term: 1 }).unwrap();
    let image = s.image();
    let snap = s.link_snapshot(snaps.path()).unwrap();
    for file in &image.files {
        std::fs::copy(snap.join(&file.name), staging.path().join(&file.name)).unwrap();
    }
    drop(s);
    let mut t = Store::open(other.path(), Keyed::with_keys(1, 2)).unwrap();
    t.set_feed(Some(feed.clone()));
    t.install(staging.path(), &image, Point { index: 4, term: 1 }, Membership::default()).unwrap();
    t.apply_entry(5, &entry(5)).unwrap();
    assert_eq!(read(&mut f), ["event 8", "event 9"]);
}

/// Each series' newest sample by a tag, as reading it whole says: from the head where it holds it
/// in order, a late write beside it or a block holding it read as a query reads it; a series of no
/// such tag under `""`; one whose newest is before the bound left out.
#[test]
fn the_newest_samples_are_read_by_a_tag() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    let keys = [gauge("m", "a"), gauge("m", "b"), gauge("m", "c"), SeriesKey::new("m", [("env", "x")], Kind::Gauge).unwrap(), gauge("other", "a")];
    let mut b = Builder::new();
    for k in &keys {
        b.define(k);
    }
    b.run(keys[0].fp, [(100_000, 1.0), (200_000, 2.0)]);
    b.run(keys[1].fp, [(100_000, 10.0), (200_000, 20.0)]);
    b.run(keys[2].fp, [(1_000, 7.0)]);
    b.run(keys[3].fp, [(150_000, 3.5)]);
    b.run(keys[4].fp, [(200_000, 99.0)]);
    s.apply(&b.finish()).unwrap();
    // A late write beside b's newest: read as a query reads it.
    b.run(keys[1].fp, [(150_000, 15.0)]);
    s.apply(&b.finish()).unwrap();
    let newest = |s: &Store, since: i64| {
        let mut out: Vec<(String, f64)> = Vec::new();
        s.newest_by("m", "host", since, &mut Scratch::default(), &mut |h, v| out.push((h.to_owned(), v)));
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };
    let want = vec![("".to_owned(), 3.5), ("a".to_owned(), 2.0), ("b".to_owned(), 20.0)];
    assert_eq!(newest(&s, 50_000), want, "c's newest is before the bound");
    assert_eq!(newest(&s, 0).len(), 4);
    // Every series' newest as reading it whole says it.
    for (h, v) in newest(&s, 0) {
        let sel = if h.is_empty() { Selector::metric("m").tag("env", "x") } else { Selector::metric("m").tag("host", &h) };
        let read = read_all(&s, &sel);
        assert_eq!(read.first().and_then(|(_, pts)| pts.last()).map(|p| f64::from_bits(p.1)), Some(v), "{h}");
    }
    // Cut into a block: read from it, the same.
    s.cut(Point { index: 1, term: 1 }).unwrap();
    assert_eq!(newest(&s, 50_000), want);
    let mut none = Vec::new();
    s.newest_by("nonesuch", "host", 0, &mut Scratch::default(), &mut |h, v| none.push((h.to_owned(), v)));
    assert!(none.is_empty());
}

/// Each host as the index says it: its series, its newest sample, its metrics' namespaces, and the
/// tags asked of its first series; a host whose newest sample is before the bound is left out, and
/// one gone with retention is gone.
#[test]
fn hosts_are_seen_by_their_series() {
    let mut s = Store::new(Keyed::with_keys(1, 2));
    let mut b = Builder::new();
    let keys = [
        SeriesKey::new("system.cpu.idle", [("host", "web-1"), ("os.type", "linux")], Kind::Gauge).unwrap(),
        SeriesKey::new("system.load.15", [("host", "web-1"), ("os.type", "linux")], Kind::Gauge).unwrap(),
        SeriesKey::new("nginx.net.conn_opened_per_s", [("host", "web-1"), ("port", "80")], Kind::Gauge).unwrap(),
        SeriesKey::new("system.cpu.idle", [("host", "db-1")], Kind::Gauge).unwrap(),
        SeriesKey::new("postgresql.connections", [("host", "db-1"), ("db", "shop")], Kind::Gauge).unwrap(),
        SeriesKey::new("system.cpu.idle", [("host", "old-1")], Kind::Gauge).unwrap(),
        SeriesKey::new("http.requests", [("service", "api")], Kind::Count).unwrap(),
        SeriesKey::new("kiln_jobs_queued", [("host", "db-1")], Kind::Gauge).unwrap(),
    ];
    for k in &keys {
        b.define(k);
    }
    for (i, k) in keys.iter().enumerate() {
        let at = if i == 5 { 1_000 } else { 100_000 + i as i64 * 1000 };
        b.run(k.fp, [(at, 1.0)]);
    }
    s.apply(&b.finish()).unwrap();
    let seen = s.tag_seen("host", 50_000, &["os.type", "cloud.provider"]);
    let names: Vec<(&str, usize, i64)> = seen.iter().map(|h| (h.value.as_str(), h.series, h.last_ms)).collect();
    assert_eq!(names, vec![("db-1", 3, 107_000), ("web-1", 3, 102_000)], "old-1 is before the bound");
    let db = seen.iter().find(|h| h.value == "db-1").unwrap();
    assert_eq!(db.namespaces, vec!["kiln".to_owned(), "postgresql".to_owned(), "system".to_owned()], "a name of no dot, to its first underscore");
    let web = seen.iter().find(|h| h.value == "web-1").unwrap();
    assert_eq!(web.namespaces, vec!["nginx".to_owned(), "system".to_owned()]);
    assert_eq!(web.tags, vec![("os.type".to_owned(), "linux".to_owned())]);
    assert_eq!(s.tag_seen("host", i64::MIN, &[]).len(), 3);
    assert!(s.tag_seen("nonesuch", i64::MIN, &[]).is_empty());
    assert!(s.retain_logical(50_000, i64::MIN) > 0);
    assert_eq!(s.tag_seen("host", i64::MIN, &[]).iter().map(|h| h.value.as_str()).collect::<Vec<_>>(), vec!["db-1", "web-1"]);
}

/// A metric's summary: its series by kind, each tag's values and the series carrying it (the most
/// values first, each tag's values of most series first), the samples applied and its newest time;
/// the metrics of most series first; counts carried over when retention numbers the symbols anew.
#[test]
fn metrics_are_summarized_by_series_tags_and_samples() {
    let mut s = Store::new(Keyed::with_keys(1, 2));
    let mut b = Builder::new();
    let mut keys = Vec::new();
    for (host, env) in [("a", "prod"), ("b", "prod"), ("c", "dev")] {
        keys.push(SeriesKey::new("http.requests", [("host", host), ("env", env)], Kind::Count).unwrap());
    }
    let old = SeriesKey::new("batch.done", [("job", "nightly")], Kind::Gauge).unwrap();
    let late = SeriesKey::new("http.requests", [("host", "d"), ("env", "prod")], Kind::Gauge).unwrap();
    for k in keys.iter().chain([&old, &late]) {
        b.define(k);
    }
    for k in &keys {
        b.run(k.fp, (0..10).map(|i| (100_000 + i * 1000, 1.0)));
    }
    b.run(late.fp, [(130_000, 2.0)]);
    b.run(old.fp, (0..3).map(|i| (1_000 + i * 1000, 1.0)));
    s.apply(&b.finish()).unwrap();
    let (matched, all) = s.metric_summaries(&|_| true, 10, 2);
    assert_eq!(matched, 2);
    let names: Vec<(&str, usize, u64)> = all.iter().map(|m| (m.name.as_str(), m.series, m.samples)).collect();
    assert_eq!(names, vec![("http.requests", 4, 31), ("batch.done", 1, 3)]);
    let h = all.first().unwrap();
    assert_eq!(h.kinds, vec![(Kind::Count, 3), (Kind::Gauge, 1)], "a metric written as two kinds says so");
    assert_eq!(h.last_ms, Some(130_000));
    let tag = |m: &MetricSummary, k: &str| m.tags.iter().find(|t| t.key == k).cloned().unwrap();
    assert_eq!((tag(h, "host").values, tag(h, "host").series), (4, 4));
    assert_eq!(tag(h, "env").top, vec![("prod".to_owned(), 3), ("dev".to_owned(), 1)]);
    assert_eq!(h.tags.first().map(|t| t.key.as_str()), Some("host"), "the tag of most values first");
    assert_eq!(s.metric_summary("nope", 1), None);
    // Retention past the old series: its metric goes, the other's counts stay theirs.
    assert!(s.retain_logical(50_000, i64::MIN) > 0);
    let (matched, all) = s.metric_summaries(&|n| n.starts_with("http"), 10, 0);
    assert_eq!(matched, 1);
    assert_eq!(all.first().map(|m| (m.series, m.samples)), Some((4, 31)));
    assert!(s.metric_summary("batch.done", 0).is_none());
}

#[test]
fn spans_are_a_store_of_their_own_and_a_trace_is_found_by_its_blocks_filters() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
    let sec = 1_000_000_000i64;
    // 20 batches of 1,000 spans, traces of 10 spans each, and 100 log records each.
    let span = |i: i64| Record {
        ts: i * sec / 100,
        severity: if i % 50 == 0 { 17 } else { 9 },
        service: ["checkout", "cart"][(i % 2) as usize].into(),
        host: "h".into(),
        source: "otel".into(),
        message: "GET /cart".into(),
        attrs: vec![("span.kind".into(), "server".into()), ("duration".into(), (i * 1000).to_string())],
        trace_id: Some((i / 10).to_le_bytes().repeat(2).try_into().unwrap()),
        span_id: Some((i + 1).to_le_bytes()),
    };
    let log = |i: i64| Record { ts: i * sec, severity: 9, service: "checkout".into(), host: "h".into(), source: "app".into(), message: format!("line {i}"), ..Record::default() };
    let mut b = Builder::new();
    for batch in 0..20 {
        let mut raw = Vec::new();
        let mut prev = 0;
        for i in batch * 1000..(batch + 1) * 1000 {
            let r = span(i);
            crate::logs::record::put_row(&mut raw, &r, prev);
            prev = r.ts;
        }
        b.spans(&raw, 1000);
        b.logs(&(batch * 100..(batch + 1) * 100).map(log).collect::<Vec<_>>());
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!((applied.counts.spans, applied.counts.logs), (1000, 100));
        if batch % 6 == 5 {
            s.cut(Point { index: batch as u64 + 1, term: 1 }).unwrap();
        }
    }
    s.cut(Point { index: 20, term: 1 }).unwrap();
    // Apart: a search of logs finds no span, one of spans no log.
    let all = crate::logs::search::parse("*").unwrap();
    assert_eq!(s.search_logs(&all, 0, i64::MAX, usize::MAX).unwrap().records.len(), 2_000);
    assert_eq!(s.search_spans(&all, 0, i64::MAX, usize::MAX).unwrap().records.len(), 20_000);
    assert!(s.span_bytes() > 0 && s.log_bytes() > 0);
    // A trace by its id: its ten spans, and every block without it ruled out by its filter.
    let id: [u8; 16] = 1234i64.to_le_bytes().repeat(2).try_into().unwrap();
    let found = s.search_spans(&crate::logs::search::Search::trace(id), 0, i64::MAX, 100).unwrap();
    assert_eq!(found.records.iter().map(|r| r.span_id.map(i64::from_le_bytes).unwrap() - 1).rev().collect::<Vec<_>>(), (12_340..12_350).collect::<Vec<_>>());
    assert!(found.blocks_pruned + 2 >= found.blocks_total && found.blocks_total > 4, "{} of {} pruned", found.blocks_pruned, found.blocks_total);
    // Searched as the trace explorer reads it.
    // Errors are every 50th span, of checkout (even); 12 ms or more from the 12,000th on.
    let q = crate::logs::search::parse_spans("service:checkout resource_name:\"GET /cart\" status:error @duration:>=12ms").unwrap();
    let errors = s.search_spans(&q, 0, i64::MAX, usize::MAX).unwrap().records;
    assert_eq!(errors.len(), (12_000..20_000).filter(|i| i % 50 == 0).count());
    assert!(errors.iter().all(|r| r.service == "checkout" && r.severity == 17 && r.attrs[1].1.parse::<i64>().unwrap() >= 12_000_000));
    assert!(s.search_spans(&crate::logs::search::parse_spans("resource:\"GET /carts\"").unwrap(), 0, i64::MAX, 1).unwrap().records.is_empty());
    // Analytics by the span's name.
    let by = s.aggregate_spans(&all, 0, i64::MAX, &[crate::logs::store::Facet::Name], &crate::logs::store::Measure::Count).unwrap();
    assert_eq!(by.groups, vec![(vec!["GET /cart".to_owned()], 20_000.0)]);
    // A restart: the same answers.
    let digest = s.digest(true).unwrap();
    drop(s);
    let mut s = Store::open(dir.path(), Keyed::with_keys(3, 4)).unwrap();
    assert_eq!(s.digest(true).unwrap(), digest);
    assert_eq!(s.search_spans(&crate::logs::search::Search::trace(id), 0, i64::MAX, 100).unwrap().records.len(), 10);
    // Spans' retention, apart from the logs': spans older than 100 s go, the logs stay.
    match s.apply_entry(21, &crate::command::retain_spans(100_000, 0)).unwrap() {
        Answer::RetainSpans { spans_cutoff } => assert_eq!(spans_cutoff, 100_000),
        other => panic!("{other:?}"),
    }
    assert!(s.search_spans(&all, 0, 99 * sec, 1).unwrap().records.is_empty());
    assert_eq!(s.search_logs(&all, 0, 99 * sec, usize::MAX).unwrap().records.len(), 100);
    s.cut(Point { index: 21, term: 1 }).unwrap();
    let (spans_before, logs_before) = (s.span_bytes(), s.log_bytes());
    s.retain_files().unwrap();
    assert!(s.span_bytes() < spans_before, "the spans' first file is wholly before the cutoff");
    assert_eq!(s.log_bytes(), logs_before);
    // Never back.
    s.apply_entry(22, &crate::command::retain_spans(50_000, 0)).unwrap();
    assert_eq!(s.spans_cutoff(), 100_000);
    // The manifest says spans now; a reopen holds the cutoff.
    assert!(std::fs::read_to_string(dir.path().join(crate::manifest::NAME)).unwrap().starts_with("kiln-obs manifest 3"));
    s.cut(Point { index: 22, term: 1 }).unwrap();
    drop(s);
    let s = Store::open(dir.path(), Keyed::with_keys(5, 6)).unwrap();
    assert_eq!(s.spans_cutoff(), 100_000);
    assert_eq!(s.search_spans(&all, 0, i64::MAX, usize::MAX).unwrap().records.len(), 10_000);
}

/// A batch whose row holds text that is not UTF-8 (in an attribute's value, its key, or its
/// message: a writer gone wrong) is refused whole as it is applied, past the horizon or not: the
/// head never takes a row its seals and cuts could not read. The same batch whole is taken.
#[test]
fn a_batch_with_a_row_that_is_not_utf_8_is_refused_as_it_is_applied() {
    use crate::logs::record::put_row;
    let records: Vec<Record> = (0..3)
        .map(|i| Record {
            ts: 1_700_000_000_000_000_000 + i * 1_000_000,
            severity: 9,
            service: "api".into(),
            host: "h1".into(),
            source: "kiln".into(),
            message: format!("request {i} took {} ms", i * 7),
            attrs: vec![("http.status_code".into(), "200".into()), ("route".into(), "/api/runs".into())],
            trace_id: None,
            span_id: None,
        })
        .collect();
    let mut raw = Vec::new();
    let mut prev = 0;
    for r in &records {
        put_row(&mut raw, r, prev);
        prev = r.ts;
    }
    let (kept, n, ahead) = within_logs(raw.clone(), 3, i64::MAX, None).unwrap();
    assert_eq!((kept.as_slice(), n, ahead), (raw.as_slice(), 3, 0));
    let last = records.last().unwrap();
    for (what, needle) in [("a value", last.attrs[1].1.as_bytes()), ("a key", last.attrs[0].0.as_bytes()), ("the message", last.message.as_bytes())] {
        let at = raw.windows(needle.len()).rposition(|w| w == needle).unwrap();
        let mut bad = raw.clone();
        bad[at] = 0xff;
        assert!(within_logs(bad.clone(), 3, i64::MAX, None).is_err(), "{what}: taken");
        // A row before it past the horizon (the rows written again without it): refused too.
        assert!(within_logs(bad, 3, records[0].ts, None).is_err(), "{what}: taken past the horizon");
    }
}
