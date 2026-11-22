//! Amphora: a table's statement read by the path the planner chooses, the column copy or the rows
//! (G11 step 5, CS.5; the choice is `translate::optimizer::column_path`'s, by cost).
//!
//! [`read`] asks the copy for its view at the statement's snapshot ([`Columns::plan_view`]), lets
//! the planner choose, and reads by the path chosen. The copy's own scan keeps its guarantee
//! whatever the choice: it reads a generation only where one answers the snapshot exactly, and
//! reads rows otherwise, so a copy behind its snapshot is never read past it, even if it fell
//! behind between the view and the scan. A copy scan's time is charged back to the copy
//! ([`Columns::measured`]), which is how its cost per row is learned.
//!
//! An aggregate ([`aggregate`]) runs on the copy through one narrow hook, [`CopyScan`]: today
//! G11 step 4's vectorized scan as main has it ([`Vectorized`]: granules with summaries, [`Plan`]),
//! and whatever scan step 4 lands next drops in by implementing the same trait, with no change to
//! the choice. Its time is charged per row of the table ([`Columns::measured_scan`]), so the
//! granules its summaries skip already lower the copy's learned cost per row; on the rows it runs
//! row by row ([`evaluate_rows`]), with the same answer.
//!
//! Behind the planner's switch (`column_path::asked`): no SQL table keeps a copy yet.
#![cfg_attr(not(test), allow(dead_code))]

use super::columns::scan::{evaluate_rows, Cache, Cell, Counts, Decode, Plan, Query, Sources};
use super::columns::{Block, Columns, Rows, Rowset, ScanCounts};
use super::version::Ts;
use crate::translate::optimizer::column_path::{self, Choice, CopyView, Path, RowRates, Shape};
use crate::translate::optimizer::learned_costs::LearnedCosts;
use crate::Result;
use std::ops::Bound;

/// What a statement asks of the table.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Ask<'a> {
    /// The row with this key.
    Point(&'a [u8]),
    /// Every row, in key order (a whole-table aggregate's input).
    Whole,
}

/// A read's answer and where it came from.
#[derive(Debug)]
pub(crate) struct Read {
    pub(crate) rows: Rowset,
    pub(crate) choice: Choice,
    /// The copy's scan counts: all zero when the rows were chosen.
    pub(crate) copy: ScanCounts,
    /// Rows read from the row B-tree: by the row path, or by the copy's path for queued keys and
    /// fallbacks.
    pub(crate) row_reads: u64,
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Reads `ask` at snapshot `s` as a statement does: by the planner's switch ([`column_path::asked`])
/// and the database's learned rates. Before both a row's visit and a seek were timed, the rows.
pub(crate) fn read_learned<B: Block>(
    copy: &Columns<B>,
    rows: &dyn Rows,
    s: Ts,
    ask: Ask<'_>,
    costs: &LearnedCosts,
    clock: &dyn Fn() -> u128,
) -> Result<Read> {
    match RowRates::learned(costs) {
        Some(rates) => read(copy, rows, s, ask, rates, column_path::asked(), clock),
        // Unknown rates price nothing: the rows, which the switch being off also gives.
        None => read(
            copy,
            rows,
            s,
            ask,
            RowRates {
                row: 0.0,
                seek: 0.0,
            },
            false,
            clock,
        ),
    }
}

/// Reads `ask` at snapshot `s`, `rows` being the row B-tree at `s`, by the path the planner chooses
/// with the row path's learned `rates`; `on` is the planner's switch, `clock` the caller's
/// nanoseconds, which time the copy's scan.
pub(crate) fn read<B: Block>(
    copy: &Columns<B>,
    rows: &dyn Rows,
    s: Ts,
    ask: Ask<'_>,
    rates: RowRates,
    on: bool,
    clock: &dyn Fn() -> u128,
) -> Result<Read> {
    let view = copy.plan_view(s);
    let table_rows = view.rows.saturating_add(view.queued) as f64;
    let shape = match ask {
        Ask::Point(_) => Shape::Point,
        Ask::Whole => Shape::Whole,
    };
    let cv = CopyView {
        exact: view.exact,
        reads: view.rows as f64,
        queued: view.queued as f64,
        nanos_per_row: view.nanos_per_row,
    };
    let choice = column_path::choose(on, shape, table_rows, rates, cv);
    match choice.path {
        Path::Rows => {
            let out = match ask {
                Ask::Point(key) => rows
                    .get(key)?
                    .map(|v| vec![(key.to_vec(), v)])
                    .unwrap_or_default(),
                Ask::Whole => rows.all()?,
            };
            let row_reads = match ask {
                Ask::Point(_) => 1,
                Ask::Whole => count(out.len()),
            };
            Ok(Read {
                rows: out,
                choice,
                copy: ScanCounts::default(),
                row_reads,
            })
        }
        Path::Copy => {
            let start = clock();
            let (all, counts) = copy.scan(s, rows)?;
            copy.measured(clock().saturating_sub(start), &counts);
            let row_reads = if counts.fallbacks > 0 {
                count(all.len())
            } else {
                counts.queued
            };
            let out = match ask {
                Ask::Point(key) => all.into_iter().filter(|(k, _)| k == key).collect(),
                Ask::Whole => all,
            };
            Ok(Read {
                rows: out,
                choice,
                copy: counts,
                row_reads,
            })
        }
    }
}

/// What the copy's scan gives back for an aggregate.
#[derive(Debug)]
pub(crate) struct Scanned {
    /// Its rows: the groups' keys and aggregates.
    pub(crate) answer: Vec<Vec<Cell>>,
    pub(crate) counts: Counts,
    pub(crate) sources: Sources,
    /// Rows its granules hold: on a fallback ([`Sources::fallbacks`]), every row, read from the
    /// rows.
    pub(crate) built: usize,
}

/// The copy's scan for an aggregate: the one seam a vectorized scan plugs into (G11 step 4; CS.5's
/// hook). It answers `q` at snapshot `s` exactly, reading `rows` (the row B-tree at `s`) for what
/// the copy hasn't applied, and only `rows` when no generation answers `s`; each row's columns
/// decoded by `dec`. The choice of path, the timing and the charge stay with [`aggregate`], so a
/// new scan changes neither.
pub(crate) trait CopyScan<B: Block> {
    fn scan(
        &self,
        copy: &Columns<B>,
        s: Ts,
        rows: &dyn Rows,
        dec: &dyn Decode,
        q: &Query,
    ) -> Result<Scanned>;
}

/// Step 4's scan as main has it: the copy's granules, with their summaries, over the statement's
/// key range ([`Columns::granules`]), run on the caller's thread by [`Plan`].
pub(crate) struct Vectorized<'a> {
    /// The decoded blocks kept between scans.
    pub(crate) cache: &'a Cache,
}

impl<B: Block> CopyScan<B> for Vectorized<'_> {
    fn scan(
        &self,
        copy: &Columns<B>,
        s: Ts,
        rows: &dyn Rows,
        dec: &dyn Decode,
        q: &Query,
    ) -> Result<Scanned> {
        let (granules, sources) = copy.granules(s, rows, dec, self.cache, &q.keys)?;
        let built = granules
            .iter()
            .fold(0usize, |n, p| n.saturating_add(p.granule.rows));
        let (answer, counts) = Plan { granules, query: q }.run_all()?;
        Ok(Scanned {
            answer,
            counts,
            sources,
            built,
        })
    }
}

/// An aggregate's answer and where it came from.
#[derive(Debug)]
pub(crate) struct Aggregated {
    /// Its rows: the groups' keys and aggregates.
    pub(crate) answer: Vec<Vec<Cell>>,
    pub(crate) choice: Choice,
    /// The vectorized scan's counts and its granules' sources: all zero when the rows were chosen.
    pub(crate) counts: Counts,
    pub(crate) sources: Sources,
    /// Rows read from the row B-tree: by the row path, or by the copy's for queued keys and
    /// fallbacks.
    pub(crate) row_reads: u64,
}

/// Runs aggregate `q` at snapshot `s` by the path the planner chooses: on the copy by `scan`, or
/// row by row over `rows` (the row B-tree at `s`), each row's columns decoded by `dec`. A whole-table statement is priced as one; a key range by
/// the rows the copy holds, the most it can read (no estimate of the range's rows is made here).
#[allow(clippy::too_many_arguments)]
pub(crate) fn aggregate<B: Block>(
    copy: &Columns<B>,
    rows: &dyn Rows,
    s: Ts,
    q: &Query,
    dec: &dyn Decode,
    scan: &dyn CopyScan<B>,
    rates: RowRates,
    on: bool,
    clock: &dyn Fn() -> u128,
) -> Result<Aggregated> {
    let view = copy.plan_view(s);
    let table = view.rows.saturating_add(view.queued);
    let table_rows = table as f64;
    let whole = matches!(
        (&q.keys.lo, &q.keys.hi),
        (Bound::Unbounded, Bound::Unbounded)
    );
    let shape = if whole {
        Shape::Whole
    } else {
        Shape::Range { rows: table_rows }
    };
    let cv = CopyView {
        exact: view.exact,
        reads: view.rows as f64,
        queued: view.queued as f64,
        nanos_per_row: view.nanos_per_row,
    };
    let choice = column_path::choose(on, shape, table_rows, rates, cv);
    match choice.path {
        Path::Rows => {
            let all = rows.all()?;
            let mut decoded = Vec::with_capacity(all.len());
            for (k, v) in all {
                let mut cells = Vec::new();
                dec.decode(&k, &v, &mut cells)?;
                decoded.push((k, cells));
            }
            Ok(Aggregated {
                answer: evaluate_rows(q, &decoded)?,
                choice,
                counts: Counts::default(),
                sources: Sources::default(),
                row_reads: count(decoded.len()),
            })
        }
        Path::Copy => {
            let start = clock();
            let Scanned {
                answer,
                counts,
                sources,
                built,
            } = scan.scan(copy, s, rows, dec, q)?;
            // A fallback's granules were built from every row; else the queued keys were looked up.
            let row_reads = if sources.fallbacks > 0 {
                count(built)
            } else {
                copy.measured_scan(clock().saturating_sub(start), table);
                count(view.queued)
            };
            Ok(Aggregated {
                answer,
                choice,
                counts,
                sources,
                row_reads,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::versioned::blink::{Comparator, Descender};
    use crate::storage::versioned::columns::{Feed, Packed, Plain};
    use crate::storage::versioned::disk::Simulated;
    use crate::storage::versioned::durable::Database;
    use crate::storage::versioned::version::Snapshot;
    use crate::sync::Arc;

    const N: u16 = 600;

    fn cmp() -> Comparator {
        Box::new(|a: &[u8], b: &[u8]| a.cmp(b))
    }

    struct At<'a> {
        db: &'a Database,
        snap: &'a Snapshot,
    }

    impl Rows for At<'_> {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            self.db.get(&mut Descender::new(), self.snap, 0, key)
        }
        fn all(&self) -> Result<Rowset> {
            self.db
                .scan(&mut Descender::new(), self.snap, 0, None, usize::MAX)
        }
    }

    fn open(seed: u64, bound: usize) -> (Database, Arc<Feed>) {
        let db = Database::open(Arc::new(Simulated::new(seed, 1).unwrap()), 4_000, 1, cmp).unwrap();
        let feed = Arc::new(Feed::new(0, bound, Box::new(|| 0)));
        db.keep_columns(feed.clone()).unwrap();
        (db, feed)
    }

    fn commit(db: &Database, keys: impl Iterator<Item = u16>, tag: u8, remove_every: u16) {
        let mut d = Descender::new();
        let mut t = db.begin().unwrap();
        for k in keys {
            if remove_every > 0 && k % remove_every == 0 {
                db.remove(&mut d, &mut t, 0, &k.to_be_bytes()).unwrap();
            } else {
                db.put(&mut d, &mut t, 0, &k.to_be_bytes(), &[tag; 8])
                    .unwrap();
            }
        }
        db.commit(t, &mut d, db.lane(), false).unwrap();
    }

    fn rebuild<B: Block>(db: &Database, copy: &Columns<B>) {
        let snap = db.snapshot().unwrap();
        let rows = At { db, snap: &snap }.all().unwrap();
        copy.rebuild(snap.start, &rows, 0).unwrap();
        db.close_snapshot(snap);
    }

    /// A clock that moves `step` nanoseconds a reading: a copy scan (two readings) is measured at
    /// `step` for the rows it read, so the test sets the copy's learned cost per row.
    fn clock(step: u128) -> impl Fn() -> u128 {
        let t = std::cell::Cell::new(0u128);
        move || {
            let now = t.get().saturating_add(step);
            t.set(now);
            now
        }
    }

    // The row path's rates, as a database would have learned them.
    const RATES: RowRates = RowRates {
        row: 1_000.0,
        seek: 2_000.0,
    };

    /// A copy with rows in blocks, applied deltas, and queued keys not applied yet.
    fn kept<B: Block>(seed: u64) -> (Database, Columns<B>) {
        let (db, feed) = open(seed, usize::MAX);
        let copy = Columns::<B>::new(feed);
        commit(&db, 0..N, 1, 0);
        rebuild(&db, &copy);
        commit(&db, (0..N).step_by(7), 2, 3);
        copy.catch_up(0).unwrap();
        commit(&db, (0..N + 40).step_by(11), 3, 4);
        (db, copy)
    }

    fn answers_identical<B: Block>(seed: u64) {
        let (db, copy) = kept::<B>(seed);
        let snap = db.snapshot().unwrap();
        let at = At {
            db: &db,
            snap: &snap,
        };
        let truth = at.all().unwrap();
        let tick = clock(1);
        // Every read by both switch settings; the paths each took, by shape.
        let check = |rates: RowRates| {
            let mut paths = Vec::new();
            for on in [false, true] {
                let whole = read(&copy, &at, snap.start, Ask::Whole, rates, on, &tick).unwrap();
                assert_eq!(whole.rows, truth, "{:?}", whole.choice);
                paths.push((Shape::Whole, whole.choice.path));
                for k in [0u16, 1, 7, 11, 44, N - 1, N + 5, N + 39] {
                    let key = k.to_be_bytes();
                    let want: Rowset = truth.iter().filter(|(kk, _)| kk == &key).cloned().collect();
                    let p =
                        read(&copy, &at, snap.start, Ask::Point(&key), rates, on, &tick).unwrap();
                    assert_eq!(p.rows, want, "key {k} {:?}", p.choice);
                    paths.push((Shape::Point, p.choice.path));
                }
            }
            paths
        };
        // Keys queued and not applied: the whole table by the copy, merged with them, and by the
        // rows; single rows by the rows (a queued key costs a seek of its own).
        let paths = check(RATES);
        assert!(paths.contains(&(Shape::Whole, Path::Copy)));
        assert!(paths.contains(&(Shape::Whole, Path::Rows)));
        assert!(!paths.contains(&(Shape::Point, Path::Copy)));
        // Applied, and a seek dearer than every row's scan: single rows by the copy too.
        copy.catch_up(0).unwrap();
        assert_eq!(copy.plan_view(snap.start).queued, 0);
        let seek_dear = RowRates {
            row: RATES.row,
            seek: 1e12,
        };
        let paths = check(seek_dear);
        assert!(paths.contains(&(Shape::Point, Path::Copy)));
        assert!(paths.contains(&(Shape::Point, Path::Rows)));
        db.close_snapshot(snap);
    }

    fn record(k: u16) -> Vec<u8> {
        use crate::numeric::Numeric;
        use crate::types::{ImmutableRecord, Value};
        let values = [
            Value::Numeric(Numeric::Integer(i64::from(k))),
            Value::Numeric(Numeric::Integer(i64::from(k % 7))),
        ];
        ImmutableRecord::from_values(values.iter(), values.len())
            .unwrap()
            .get_payload()
            .to_vec()
    }

    fn commit_records(db: &Database, keys: impl Iterator<Item = u16>, remove_every: u16) {
        let mut d = Descender::new();
        let mut t = db.begin().unwrap();
        for k in keys {
            if remove_every > 0 && k % remove_every == 0 {
                db.remove(&mut d, &mut t, 0, &k.to_be_bytes()).unwrap();
            } else {
                db.put(
                    &mut d,
                    &mut t,
                    0,
                    &k.to_be_bytes(),
                    &record(k.wrapping_mul(3)),
                )
                .unwrap();
            }
        }
        db.commit(t, &mut d, db.lane(), false).unwrap();
    }

    fn queries() -> Vec<Query> {
        use crate::storage::versioned::columns::scan::{Agg, Filter, KeyRange, Op};
        let aggs = vec![Agg::Count, Agg::Sum(0), Agg::Min(0), Agg::Max(1)];
        vec![
            Query {
                filters: vec![],
                group: vec![],
                aggs: aggs.clone(),
                budget: usize::MAX,
                keys: KeyRange::default(),
            },
            Query {
                filters: vec![Filter::Cmp {
                    col: 0,
                    op: Op::Gt,
                    lit: Cell::Int(900),
                }],
                group: vec![1],
                aggs: aggs.clone(),
                budget: usize::MAX,
                keys: KeyRange::default(),
            },
            Query {
                filters: vec![],
                group: vec![],
                aggs,
                budget: usize::MAX,
                keys: KeyRange {
                    lo: Bound::Included(100u16.to_be_bytes().to_vec()),
                    hi: Bound::Excluded(400u16.to_be_bytes().to_vec()),
                },
            },
        ]
    }

    /// The truth: the aggregate row by row over the rows at the snapshot.
    fn truth(at: &At<'_>, q: &Query) -> Vec<Vec<Cell>> {
        use crate::storage::versioned::columns::scan::Records;
        let mut decoded = Vec::new();
        for (k, v) in at.all().unwrap() {
            let mut cells = Vec::new();
            Records.decode(&k, &v, &mut cells).unwrap();
            decoded.push((k, cells));
        }
        evaluate_rows(q, &decoded).unwrap()
    }

    /// Do: run whole-table, grouped and key-range aggregates on a copy with deltas and queued
    /// keys, by each path, then on a copy behind its snapshot. Expect: every answer equals the
    /// rows' at the snapshot; with the switch on the copy is chosen and read by the vectorized
    /// scan (granules read, only the queued keys looked up in the rows); off, every row is read
    /// from the rows; behind, the rows are chosen and no granule is read (CS.5 on CS.4's scan).
    #[test]
    fn cs5_vectorized_aggregates_identical_and_chosen_by_cost() {
        use crate::storage::versioned::columns::scan::Records;
        let (db, feed) = open(16, usize::MAX);
        let copy = Columns::<Plain>::new(feed);
        commit_records(&db, 0..N, 0);
        rebuild(&db, &copy);
        commit_records(&db, (0..N).step_by(7), 3);
        copy.catch_up(0).unwrap();
        commit_records(&db, (0..N).step_by(13), 4);
        let snap = db.snapshot().unwrap();
        let at = At {
            db: &db,
            snap: &snap,
        };
        let cache = Cache::new(usize::MAX);
        let tick = clock(1);
        let queued = copy.plan_view(snap.start).queued;
        assert!(queued > 0);
        for q in queries() {
            let want = truth(&at, &q);
            let off = aggregate(
                &copy,
                &at,
                snap.start,
                &q,
                &Records,
                &Vectorized { cache: &cache },
                RATES,
                false,
                &tick,
            )
            .unwrap();
            assert_eq!(off.answer, want);
            assert_eq!(off.choice.path, Path::Rows);
            assert_eq!(off.counts, Counts::default());
            assert_eq!(off.row_reads, count(at.all().unwrap().len()));
            let on = aggregate(
                &copy,
                &at,
                snap.start,
                &q,
                &Records,
                &Vectorized { cache: &cache },
                RATES,
                true,
                &tick,
            )
            .unwrap();
            assert_eq!(on.answer, want, "{:?}", on.choice);
            assert_eq!(on.choice.path, Path::Copy, "{:?}", on.choice);
            assert_eq!(on.sources.fallbacks, 0);
            assert!(on.counts.granules > 0);
            assert_eq!(on.row_reads, count(queued));
        }
        db.close_snapshot(snap);

        // Behind: a queue that holds nothing drops the commits after the rebuild.
        let (db, feed) = open(17, 0);
        let copy = Columns::<Packed>::new(feed);
        commit_records(&db, 0..N, 0);
        rebuild(&db, &copy);
        commit_records(&db, (0..N).step_by(5), 2);
        let snap = db.snapshot().unwrap();
        let at = At {
            db: &db,
            snap: &snap,
        };
        for q in queries() {
            let r = aggregate(
                &copy,
                &at,
                snap.start,
                &q,
                &Records,
                &Vectorized { cache: &cache },
                RATES,
                true,
                &tick,
            )
            .unwrap();
            assert_eq!((r.choice.path, r.choice.copy_cost), (Path::Rows, None));
            assert_eq!(
                (r.counts, r.sources),
                (Counts::default(), Sources::default())
            );
            assert_eq!(r.answer, truth(&at, &q));
        }
        db.close_snapshot(snap);
    }

    /// A scan dropped in through the hook: step 4's as main has it, counting its calls.
    struct Counted<'a> {
        inner: Vectorized<'a>,
        calls: std::cell::Cell<u32>,
    }

    impl<B: Block> CopyScan<B> for Counted<'_> {
        fn scan(
            &self,
            copy: &Columns<B>,
            s: Ts,
            rows: &dyn Rows,
            dec: &dyn Decode,
            q: &Query,
        ) -> Result<Scanned> {
            self.calls.set(self.calls.get().saturating_add(1));
            self.inner.scan(copy, s, rows, dec, q)
        }
    }

    /// Do: run the aggregates through a scan given to the hook, with the switch on and off, then
    /// on a copy behind its snapshot. Expect: the hook's scan runs once for each aggregate the copy
    /// is chosen for and answers it, never with the switch off, never on a stale copy; every
    /// answer equals the rows' (CS.5's hook for step 4's scan).
    #[test]
    fn cs5_scan_hook_runs_only_when_the_copy_is_chosen() {
        use crate::storage::versioned::columns::scan::Records;
        let cache = Cache::new(usize::MAX);
        let tick = clock(1);
        let hooked = Counted {
            inner: Vectorized { cache: &cache },
            calls: std::cell::Cell::new(0),
        };
        let (db, feed) = open(18, usize::MAX);
        let copy = Columns::<Plain>::new(feed);
        commit_records(&db, 0..N, 0);
        rebuild(&db, &copy);
        commit_records(&db, (0..N).step_by(9), 4);
        let snap = db.snapshot().unwrap();
        let at = At {
            db: &db,
            snap: &snap,
        };
        let qs = queries();
        for q in &qs {
            let off = aggregate(
                &copy, &at, snap.start, q, &Records, &hooked, RATES, false, &tick,
            )
            .unwrap();
            assert_eq!(off.choice.path, Path::Rows);
            assert_eq!(off.answer, truth(&at, q));
        }
        assert_eq!(hooked.calls.get(), 0);
        for q in &qs {
            let on = aggregate(
                &copy, &at, snap.start, q, &Records, &hooked, RATES, true, &tick,
            )
            .unwrap();
            assert_eq!(on.choice.path, Path::Copy, "{:?}", on.choice);
            assert_eq!(on.answer, truth(&at, q));
        }
        assert_eq!(hooked.calls.get(), u32::try_from(qs.len()).unwrap());
        db.close_snapshot(snap);

        let (db, feed) = open(19, 0);
        let copy = Columns::<Packed>::new(feed);
        commit_records(&db, 0..N, 0);
        rebuild(&db, &copy);
        commit_records(&db, (0..N).step_by(5), 2);
        let snap = db.snapshot().unwrap();
        let at = At {
            db: &db,
            snap: &snap,
        };
        let before = hooked.calls.get();
        for q in &qs {
            let r = aggregate(
                &copy, &at, snap.start, q, &Records, &hooked, RATES, true, &tick,
            )
            .unwrap();
            assert_eq!(r.choice.path, Path::Rows);
            assert_eq!(r.answer, truth(&at, q));
        }
        assert_eq!(hooked.calls.get(), before);
        db.close_snapshot(snap);
    }

    /// Do: read through the statement's entry, with this process's learned rates and the switch
    /// as the environment sets it. Expect: whatever path they choose, the answers equal the rows.
    #[test]
    fn cs5_learned_rates_answer_like_the_rows() {
        let (db, copy) = kept::<Packed>(15);
        let snap = db.snapshot().unwrap();
        let at = At {
            db: &db,
            snap: &snap,
        };
        let truth = at.all().unwrap();
        let costs = LearnedCosts::new();
        let tick = clock(1);
        for _ in 0..2 {
            let whole = read_learned(&copy, &at, snap.start, Ask::Whole, &costs, &tick).unwrap();
            assert_eq!(whole.rows, truth, "{:?}", whole.choice);
            let key = 44u16.to_be_bytes();
            let want: Rowset = truth.iter().filter(|(k, _)| k == &key).cloned().collect();
            let p = read_learned(&copy, &at, snap.start, Ask::Point(&key), &costs, &tick).unwrap();
            assert_eq!(p.rows, want, "{:?}", p.choice);
        }
        assert_eq!(
            column_path::asked(),
            std::env::var(column_path::ENV).is_ok_and(|v| v == "on")
        );
        db.close_snapshot(snap);
    }

    /// Do: read every row and single rows, by each path, from a copy with deltas and queued keys.
    /// Expect: every answer equals the rows at the snapshot (CS.5, answers identical).
    #[test]
    fn cs5_answers_identical_whichever_path_plain() {
        answers_identical::<Plain>(11);
    }

    #[test]
    fn cs5_answers_identical_whichever_path_packed() {
        answers_identical::<Packed>(12);
    }

    /// Do: with the copy measured cheaper per row than a row's visit, read the whole table, then
    /// one row. Expect, by counts: the whole table from the copy (every row from its blocks and
    /// deltas, only the queued keys from the rows), the one row from the rows (one row read, the
    /// copy untouched) (CS.5).
    #[test]
    fn cs5_wide_aggregate_reads_the_copy_point_read_the_rows() {
        let (db, copy) = kept::<Plain>(13);
        let snap = db.snapshot().unwrap();
        let at = At {
            db: &db,
            snap: &snap,
        };
        let truth = at.all().unwrap();
        // The copy's first scan at its block size is tried, and measured: one reading step for
        // its rows, a tenth of a row's visit each.
        let step = (truth.len() as u128).saturating_mul(100);
        let tick = clock(step);
        let first = read(&copy, &at, snap.start, Ask::Whole, RATES, true, &tick).unwrap();
        assert_eq!(first.choice.path, Path::Copy);
        assert!(copy.plan_view(snap.start).nanos_per_row.is_some());
        let whole = read(&copy, &at, snap.start, Ask::Whole, RATES, true, &tick).unwrap();
        assert_eq!(whole.choice.path, Path::Copy, "{:?}", whole.choice);
        assert_eq!(whole.copy.fallbacks, 0);
        let queued = copy.plan_view(snap.start).queued;
        assert!(queued > 0);
        assert_eq!(whole.row_reads, whole.copy.queued);
        assert!(whole.copy.queued <= count(queued));
        assert_eq!(
            whole.copy.copy.saturating_add(whole.copy.queued),
            count(truth.len())
        );
        assert_eq!(whole.rows, truth);

        let key = 44u16.to_be_bytes();
        let point = read(&copy, &at, snap.start, Ask::Point(&key), RATES, true, &tick).unwrap();
        assert_eq!(point.choice.path, Path::Rows, "{:?}", point.choice);
        assert_eq!(point.copy, ScanCounts::default());
        assert_eq!(point.row_reads, 1);
        db.close_snapshot(snap);
    }

    /// Do: read at snapshots no generation answers exactly: after commits a full queue dropped, and
    /// before the copy's only generation was made. Expect: the rows are chosen, the copy is not
    /// read, and the answers equal the rows at each snapshot (CS.5, never read past its snapshot).
    #[test]
    fn cs5_stale_copy_never_read_past_its_snapshot() {
        // A queue that holds nothing: every commit after the rebuild is dropped, the copy behind.
        let (db, feed) = open(14, 0);
        let copy = Columns::<Plain>::new(feed);
        commit(&db, 0..N, 1, 0);
        let early = db.snapshot().unwrap();
        commit(&db, (0..N).step_by(3), 2, 5);
        rebuild(&db, &copy);
        commit(&db, (0..N).step_by(2), 3, 7);
        copy.catch_up(0).unwrap();
        let late = db.snapshot().unwrap();
        let tick = clock(1);
        for snap in [&early, &late] {
            let at = At { db: &db, snap };
            assert!(!copy.plan_view(snap.start).exact);
            let truth = at.all().unwrap();
            let r = read(&copy, &at, snap.start, Ask::Whole, RATES, true, &tick).unwrap();
            assert_eq!((r.choice.path, r.choice.copy_cost), (Path::Rows, None));
            assert_eq!(r.copy, ScanCounts::default());
            assert_eq!(r.rows, truth);
            assert_eq!(r.row_reads, count(truth.len()));
        }
        db.close_snapshot(late);
        db.close_snapshot(early);
    }
}
