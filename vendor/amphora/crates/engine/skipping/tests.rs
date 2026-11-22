//! T-G5.4.1: writes between scans, each prototype under a random differential against a plain
//! scan of a model table: no row missed, none seen stale.

use super::*;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::collections::BTreeMap;

const SEEDS: u64 = 64;
const OPS: usize = 600;

/// The model table: rowid -> the filtered column (`None`: not an integer).
type Table = BTreeMap<i64, Option<i64>>;

fn value_for(rng: &mut ChaCha8Rng, rowid: i64) -> Option<i64> {
    if rng.random_bool(0.05) {
        None
    } else {
        // Mostly following the rowid (an append-ordered column, which zone maps can skip), with
        // noise and some values from anywhere.
        if rng.random_bool(0.9) {
            Some(rowid / 4 + rng.random_range(-8..=8))
        } else {
            Some(rng.random_range(-1_000..=1_000))
        }
    }
}

fn initial_table(rng: &mut ChaCha8Rng) -> Table {
    let mut table = Table::new();
    let mut rowid = 0;
    for _ in 0..rng.random_range(0..400) {
        rowid += rng.random_range(1..4);
        let value = value_for(rng, rowid);
        table.insert(rowid, value);
    }
    table
}

/// The plain scan: every row whose integer value the filter's bounds hold.
fn plain(table: &Table, lo: i64, hi: i64) -> Vec<i64> {
    table
        .iter()
        .filter(|(_, value)| value.is_some_and(|v| (lo..=hi).contains(&v)))
        .map(|(rowid, _)| *rowid)
        .collect()
}

/// A random write, applied to the model and returned for the hook.
fn random_write(rng: &mut ChaCha8Rng, table: &mut Table) -> RowWrite {
    let last = table.keys().next_back().copied().unwrap_or(0);
    let pick = rng.random_range(0..10);
    if table.is_empty() || pick < 4 {
        // Insert: mostly appended, sometimes into a gap or below every rowid.
        let rowid = match rng.random_range(0..4) {
            0 => {
                let r = rng.random_range(-50..=last + 1);
                if table.contains_key(&r) {
                    last + 1
                } else {
                    r
                }
            }
            _ => last + rng.random_range(1..4),
        };
        let value = value_for(rng, rowid);
        table.insert(rowid, value);
        return RowWrite::Insert { rowid, value };
    }
    let nth = rng.random_range(0..table.len());
    let (&rowid, &old) = table.iter().nth(nth).unwrap();
    if pick < 7 {
        table.remove(&rowid);
        RowWrite::Delete { rowid, old }
    } else {
        let new = value_for(rng, rowid);
        table.insert(rowid, new);
        RowWrite::Update { rowid, old, new }
    }
}

fn random_filter(rng: &mut ChaCha8Rng, table: &Table, repeats: &mut Vec<(i64, i64)>) -> (i64, i64) {
    // Filters repeat (what cracking feeds on), and the bounds reach both ends of i64.
    if !repeats.is_empty() && rng.random_bool(0.5) {
        return repeats[rng.random_range(0..repeats.len())];
    }
    let top = table.keys().next_back().copied().unwrap_or(0) / 4 + 10;
    let filter = match rng.random_range(0..10) {
        0 => (i64::MIN, rng.random_range(-20..top)),
        1 => (rng.random_range(-20..top), i64::MAX),
        2 => (5, 4),
        _ => {
            let lo = rng.random_range(-20..top);
            (lo, lo + rng.random_range(0..40))
        }
    };
    repeats.push(filter);
    filter
}

/// Do: build zone maps over random leaf runs, then interleave random inserts, deletes and updates
/// with filters. Expect: reading only the candidate ranges finds exactly the plain scan's rows,
/// every live row stays inside its zone's summary, and some zones were skipped (non-vacuity).
#[test]
fn t_g5_4_1_zone_maps_writes_between_scans_match_a_plain_scan() {
    let mut skipped = 0;
    let mut splits = 0;
    for seed in 0..SEEDS {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut table = initial_table(&mut rng);
        let mut builder = ZoneMapBuilder::new();
        for (&rowid, &value) in &table {
            builder.row(rowid, value).unwrap();
            if rng.random_bool(0.04) {
                builder.end_run();
            }
        }
        let mut skipping = Skipping::ZoneMaps(builder.finish());
        let mut repeats = Vec::new();
        for _ in 0..OPS {
            let write = random_write(&mut rng, &mut table);
            skipping.on_write(write).unwrap();
            if rng.random_bool(0.3) {
                let (lo, hi) = random_filter(&mut rng, &table, &mut repeats);
                let Candidates::Ranges(ranges) = skipping.candidates(lo, hi).unwrap() else {
                    panic!("zone maps name ranges");
                };
                let mut found = Vec::new();
                for range in &ranges {
                    found.extend(plain_range(&table, range.first, range.last, lo, hi));
                }
                assert_eq!(
                    found,
                    plain(&table, lo, hi),
                    "seed {seed}, filter {lo}..={hi}"
                );
            }
            let Skipping::ZoneMaps(zones) = &skipping else {
                unreachable!()
            };
            for (&rowid, &value) in &table {
                assert!(
                    zones.covers(rowid, value),
                    "seed {seed}: row {rowid} = {value:?} outside its zone"
                );
            }
        }
        let Skipping::ZoneMaps(zones) = &skipping else {
            unreachable!()
        };
        skipped += zones.stats().zones_skipped;
        splits += zones.stats().zones_split;
    }
    assert!(skipped > 0, "no zone was ever skipped");
    assert!(splits > 0, "no appended tail ever started a zone");
}

fn plain_range(table: &Table, first: i64, last: i64, lo: i64, hi: i64) -> Vec<i64> {
    table
        .range(first..=last)
        .filter(|(_, value)| value.is_some_and(|v| (lo..=hi).contains(&v)))
        .map(|(rowid, _)| *rowid)
        .collect()
}

/// Do: crack a random table's column with repeated filters, interleaving random inserts, deletes
/// and updates. Expect: the candidates, filtered, are the plain scan's rows; each matching entry the
/// cracker holds carries the row's current value (none stale); and repeated filters found their
/// bounds cracked (non-vacuity: fewer partitions than queries asked).
#[test]
fn t_g5_4_1_cracking_writes_between_scans_match_a_plain_scan() {
    let mut cracks = 0;
    let mut queries = 0;
    let mut moved = 0;
    for seed in 0..SEEDS {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut table = initial_table(&mut rng);
        let mut skipping =
            Skipping::Cracking(CrackerColumn::new(table.iter().map(|(r, v)| (*r, *v))));
        let mut repeats = Vec::new();
        for _ in 0..OPS {
            let write = random_write(&mut rng, &mut table);
            skipping.on_write(write).unwrap();
            if rng.random_bool(0.3) {
                let (lo, hi) = random_filter(&mut rng, &table, &mut repeats);
                let Candidates::Rowids(rowids) = skipping.candidates(lo, hi).unwrap() else {
                    panic!("cracking names rowids");
                };
                let found: Vec<i64> = rowids
                    .iter()
                    .filter(|rowid| table[*rowid].is_some_and(|v| (lo..=hi).contains(&v)))
                    .copied()
                    .collect();
                let expected = plain(&table, lo, hi);
                assert_eq!(found, expected, "seed {seed}, filter {lo}..={hi}");
                let Skipping::Cracking(cracker) = &mut skipping else {
                    unreachable!()
                };
                let mut entries = cracker.matching_entries(lo, hi).unwrap();
                for (value, rowid) in &entries {
                    assert_eq!(
                        table.get(rowid),
                        Some(&Some(*value)),
                        "seed {seed}: stale entry for row {rowid}"
                    );
                }
                entries.sort_by_key(|(_, rowid)| *rowid);
                let rowids: Vec<i64> = entries.iter().map(|(_, rowid)| *rowid).collect();
                assert_eq!(rowids, expected, "seed {seed}: entries for {lo}..={hi}");
            }
        }
        let Skipping::Cracking(cracker) = &mut skipping else {
            unreachable!()
        };
        // Every row the table holds is in the column once: integers as entries, others apart.
        let mut held: Vec<i64> = cracker
            .matching_entries(i64::MIN, i64::MAX)
            .unwrap()
            .iter()
            .map(|(_, r)| *r)
            .collect();
        held.extend(cracker.others());
        held.sort_unstable();
        assert_eq!(
            held,
            table.keys().copied().collect::<Vec<_>>(),
            "seed {seed}"
        );
        cracks += cracker.stats().cracks;
        queries += cracker.stats().queries;
        moved += cracker.stats().entries_moved;
    }
    assert!(
        cracks > 0 && cracks < 2 * queries,
        "cracks {cracks} for {queries} queries"
    );
    assert!(moved > 0, "no write ever rippled");
}

/// Do: hand the hook writes with the switch off, and ask for candidates. Expect: nothing kept,
/// every row a candidate.
#[test]
fn t_g5_4_1_switch_off_keeps_nothing() {
    let mut skipping = Skipping::Off;
    skipping
        .on_write(RowWrite::Insert {
            rowid: 1,
            value: Some(1),
        })
        .unwrap();
    assert_eq!(skipping.candidates(0, 0).unwrap(), Candidates::All);
}

/// Do: remove a row neither summary holds; build from rowids out of order. Expect: typed refusals.
#[test]
fn t_g5_4_1_disagreements_are_refused() {
    let mut zones = ZoneMapBuilder::new().finish();
    assert_eq!(
        zones.on_write(RowWrite::Delete {
            rowid: 3,
            old: Some(1)
        }),
        Err(SkipError::MissingRow {
            rowid: 3,
            value: Some(1)
        })
    );
    let mut cracker = CrackerColumn::new([(1, Some(5))]);
    assert_eq!(
        cracker.on_write(RowWrite::Delete {
            rowid: 1,
            old: Some(6)
        }),
        Err(SkipError::MissingRow {
            rowid: 1,
            value: Some(6)
        })
    );
    assert_eq!(
        cracker.on_write(RowWrite::Delete {
            rowid: 2,
            old: None
        }),
        Err(SkipError::MissingRow {
            rowid: 2,
            value: None
        })
    );
    let mut builder = ZoneMapBuilder::new();
    builder.row(5, Some(1)).unwrap();
    assert_eq!(
        builder.row(5, Some(2)),
        Err(SkipError::RowidOrder {
            previous: 5,
            rowid: 5
        })
    );
}

/// A real table's leaves (G5 step 4: run sizes from measured data): an in-memory database whose
/// rows span many leaves.
mod leaves {
    use super::super::{zone_map_of_table, LeafBuildError, RowWrite, ZoneMap};
    use crate::io::MemoryIO;
    use crate::sync::Arc;
    use crate::{Connection, Database, SqliteDialect, Value};
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;

    fn open() -> Arc<Connection> {
        let io = Arc::new(MemoryIO::new());
        let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
        db.connect(crate::ConnectOpts::new()).unwrap()
    }

    fn query(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<Value>> {
        let mut stmt = conn.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        stmt.run_collect_rows()
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn rowids(conn: &Arc<Connection>, sql: &str) -> Vec<i64> {
        query(conn, sql)
            .into_iter()
            .map(|row| match row.first() {
                Some(Value::Numeric(crate::Numeric::Integer(rowid))) => *rowid,
                other => panic!("{sql}: rowid {other:?}"),
            })
            .collect()
    }

    fn build(conn: &Arc<Connection>, table: &str) -> ZoneMap {
        zone_map_of_table(conn, table, "a").unwrap()
    }

    /// A row's column `a` as the hook takes it (`None`: not an integer).
    fn value_of(conn: &Arc<Connection>, rowid: i64) -> Option<i64> {
        match query(conn, &format!("SELECT a FROM t WHERE rowid = {rowid}"))
            .first()
            .and_then(|row| row.first())
        {
            Some(Value::Numeric(crate::Numeric::Integer(a))) => Some(*a),
            _ => None,
        }
    }

    /// Do: build zone maps over two tables whose rows differ only in size. Expect: one zone per
    /// leaf, so the table with larger rows has fewer rows a zone; every row is in some zone.
    #[test]
    fn t_g5_4_1_zones_follow_the_tables_leaves() {
        let conn = open();
        for (table, pad) in [("small", 40), ("large", 400)] {
            conn.execute(format!("CREATE TABLE {table}(a INTEGER, pad BLOB)"))
                .unwrap();
            conn.execute(format!(
                "INSERT INTO {table} SELECT value, randomblob({pad}) FROM generate_series(1, 4000)"
            ))
            .unwrap();
        }
        let small = build(&conn, "small");
        let large = build(&conn, "large");
        assert_eq!(small.rows().unwrap(), 4000);
        assert_eq!(large.rows().unwrap(), 4000);
        assert!(small.zone_count() > 1, "small rows fill several leaves");
        assert!(
            large.zone_count() > small.zone_count(),
            "larger rows fill more leaves: {} against {}",
            large.zone_count(),
            small.zone_count()
        );
    }

    /// Do: ask for zone maps of what holds no stored column. Expect: typed refusals.
    #[test]
    fn t_g5_4_1_no_table_or_column_is_refused() {
        let conn = open();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a)")
            .unwrap();
        conn.execute("CREATE VIEW v AS SELECT a FROM t").unwrap();
        assert!(matches!(
            zone_map_of_table(&conn, "v", "a"),
            Err(LeafBuildError::NoTable { .. })
        ));
        assert!(matches!(
            zone_map_of_table(&conn, "t", "b"),
            Err(LeafBuildError::NoColumn { .. })
        ));
        assert!(matches!(
            zone_map_of_table(&conn, "t", "id"),
            Err(LeafBuildError::NoColumn { .. })
        ));
        assert_eq!(
            zone_map_of_table(&conn, "t", "a").unwrap().rows().unwrap(),
            0
        );
    }

    /// Do: zone maps built from a real table's leaves, SQL writes between scans fed to the hook,
    /// each filter read only from the candidate ranges. Expect: the same rows as a plain scan.
    #[test]
    fn t_g5_4_1_real_leaves_writes_between_scans_match_a_plain_scan() {
        let mut rng = ChaCha8Rng::seed_from_u64(54);
        let conn = open();
        conn.execute("CREATE TABLE t(a, pad BLOB)").unwrap();
        // A time-ordered column (it follows the rowid), some values not integers.
        conn.execute(
            "INSERT INTO t SELECT CASE WHEN value % 97 = 0 THEN 'x' ELSE value / 3 END, \
             randomblob(120) FROM generate_series(1, 3000)",
        )
        .unwrap();
        let mut zones = build(&conn, "t");
        let mut next = 3000i64;
        let mut skipped_any = false;
        for _ in 0..60 {
            for _ in 0..10 {
                let write = match rng.random_range(0..3) {
                    0 => {
                        next += 1;
                        let value = next / 3 + rng.random_range(-5..=5);
                        conn.execute(format!("INSERT INTO t VALUES ({value}, randomblob(120))"))
                            .unwrap();
                        RowWrite::Insert {
                            rowid: next,
                            value: Some(value),
                        }
                    }
                    pick => {
                        let rowid = rng.random_range(1..=next);
                        if query(&conn, &format!("SELECT 1 FROM t WHERE rowid = {rowid}"))
                            .is_empty()
                        {
                            continue;
                        }
                        let old = value_of(&conn, rowid);
                        if pick == 1 {
                            conn.execute(format!("DELETE FROM t WHERE rowid = {rowid}"))
                                .unwrap();
                            RowWrite::Delete { rowid, old }
                        } else {
                            let new = rng.random_range(-100..=1_200);
                            conn.execute(format!("UPDATE t SET a = {new} WHERE rowid = {rowid}"))
                                .unwrap();
                            RowWrite::Update {
                                rowid,
                                old,
                                new: Some(new),
                            }
                        }
                    }
                };
                zones.on_write(write).unwrap();
            }
            let lo = rng.random_range(-50..=1_100);
            let hi = lo + rng.random_range(0..60);
            let filter = format!("typeof(a) = 'integer' AND a BETWEEN {lo} AND {hi}");
            let plain = rowids(
                &conn,
                &format!("SELECT rowid FROM t WHERE {filter} ORDER BY rowid"),
            );
            let ranges = zones.candidate_ranges(lo, hi);
            let mut found = Vec::new();
            for range in &ranges {
                found.extend(rowids(
                    &conn,
                    &format!(
                        "SELECT rowid FROM t WHERE rowid BETWEEN {} AND {} AND {filter} ORDER BY rowid",
                        range.first, range.last
                    ),
                ));
            }
            assert_eq!(found, plain, "filter {lo}..={hi}, ranges {ranges:?}");
            skipped_any |= zones.stats().zones_skipped > 0;
        }
        assert!(skipped_any, "the zones skipped some leaves");
    }
}
