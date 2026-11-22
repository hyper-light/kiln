//! QT.7 (query speed step 7): the batched scan against the row-at-a-time scan, switch on and off,
//! on random tables and predicates: every type, NULLs, affinities, a collation, overflow rows,
//! deleted rows, columns added after rows were written, ascending and descending scans, and
//! batch budgets down to below one row.

use std::cell::Cell;

use crate::io::MemoryIO;
use crate::sync::Arc;
use crate::{Connection, Database, SqliteDialect};

thread_local! {
    static BUDGET: Cell<Option<usize>> = const { Cell::new(None) };
    static DECODED: Cell<usize> = const { Cell::new(0) };
    static FUSED: Cell<usize> = const { Cell::new(0) };
}

/// Rows a fused scan loop ran from a batch (`fused.rs`).
pub(super) fn note_fused(rows: usize) {
    FUSED.with(|fused| fused.set(fused.get() + rows));
}

fn fused() -> usize {
    FUSED.with(Cell::get)
}

/// The batch budget a test set, instead of the measured one.
pub(super) fn budget_override() -> Option<usize> {
    BUDGET.with(Cell::get)
}

pub(super) fn note_decoded(rows: usize) {
    DECODED.with(|decoded| decoded.set(decoded.get() + rows));
}

fn decoded() -> usize {
    DECODED.with(Cell::get)
}

/// A seeded xorshift: the same tables and predicates on every run of a seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn open() -> Arc<Connection> {
    let io = Arc::new(MemoryIO::new());
    let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
    db.connect(crate::ConnectOpts::new()).unwrap()
}

/// A statement's rows, or the error it ended with (`sum` overflowing, say): both must match.
fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    match stmt.run_collect_rows() {
        Ok(rows) => rows.into_iter().map(|row| format!("{row:?}")).collect(),
        Err(e) => vec![format!("error: {e}")],
    }
}

/// A value of any storage class, in SQL.
fn literal(rng: &mut Rng) -> String {
    match rng.below(8) {
        0 => "NULL".into(),
        1 => format!("{}", rng.next() as i64),
        2 => format!("{}", rng.below(50) as i64 - 25),
        3 => format!("{:?}", (rng.below(20_000) as f64 - 10_000.0) / 7.0),
        4 => format!(
            "'{}'",
            ["a", "B", "ab", "Ab", "10", "2.5", "x y", ""][rng.below(8) as usize]
        ),
        5 => format!("X'{:02x}{:02x}'", rng.below(256), rng.below(256)),
        // An overflow row: its record spills past the page.
        6 if rng.below(8) == 0 => format!("X'{}'", "ab".repeat(3_000 + rng.below(3_000) as usize)),
        _ => format!("{}", rng.below(100)),
    }
}

fn build(conn: &Arc<Connection>, rng: &mut Rng) {
    conn.execute("CREATE TABLE t(a INTEGER, b REAL, c TEXT COLLATE NOCASE, d BLOB, e NUMERIC, f)")
        .unwrap();
    let n = 200 + rng.below(1_500);
    for _ in 0..n {
        let values: Vec<String> = (0..6).map(|_| literal(rng)).collect();
        conn.execute(format!("INSERT INTO t VALUES ({})", values.join(",")))
            .unwrap();
    }
    conn.execute(format!(
        "DELETE FROM t WHERE rowid % {} = 0",
        3 + rng.below(5)
    ))
    .unwrap();
    // Rows written before the column was added read its default.
    conn.execute("ALTER TABLE t ADD COLUMN g TEXT DEFAULT 'dflt'")
        .unwrap();
    for _ in 0..rng.below(50) {
        let values: Vec<String> = (0..7).map(|_| literal(rng)).collect();
        conn.execute(format!("INSERT INTO t VALUES ({})", values.join(",")))
            .unwrap();
    }
}

fn predicate(rng: &mut Rng) -> String {
    let column = ["a", "b", "c", "d", "e", "f", "g"][rng.below(7) as usize];
    match rng.below(5) {
        0 => format!("{column} IS NULL"),
        1 => format!("{column} IS NOT NULL"),
        2 => format!("{column} > {}", literal(rng)),
        3 => format!("{column} = {}", literal(rng)),
        _ => format!("{column} <= {} OR a < {}", literal(rng), rng.below(60)),
    }
}

fn queries(rng: &mut Rng) -> Vec<String> {
    let mut out = vec![
        "SELECT * FROM t".to_string(),
        "SELECT rowid, * FROM t ORDER BY rowid DESC".to_string(),
        "SELECT count(*) FROM t".to_string(),
        "SELECT count(*), count(a), count(g), sum(a), total(b), min(c), max(c), min(d), max(e), avg(e), group_concat(g) FROM t".to_string(),
        "SELECT c, count(*), sum(e) FROM t GROUP BY c ORDER BY c".to_string(),
        "SELECT length(d), typeof(f), g FROM t".to_string(),
    ];
    for _ in 0..12 {
        let p = predicate(rng);
        out.push(format!("SELECT * FROM t WHERE {p}"));
        out.push(format!(
            "SELECT count(*), sum(a), min(b), max(f), total(e) FROM t WHERE {p}"
        ));
        out.push(format!("SELECT a, c FROM t WHERE {p} ORDER BY rowid DESC"));
    }
    out
}

/// One seed: every query's answer with the switch on equals its answer with it off.
fn differential(seed: u64, budget: Option<usize>) -> usize {
    BUDGET.with(|b| b.set(budget));
    DECODED.with(|d| d.set(0));
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let conn = open();
    build(&conn, &mut rng);
    for sql in queries(&mut rng) {
        conn.set_vectorized_scans(false);
        let row_at_a_time = rows(&conn, &sql);
        conn.set_vectorized_scans(true);
        let batched = rows(&conn, &sql);
        assert_eq!(
            batched, row_at_a_time,
            "seed {seed}, budget {budget:?}: {sql}"
        );
    }
    conn.set_vectorized_scans(false);
    BUDGET.with(|b| b.set(None));
    decoded()
}

/// T-QT.7.1: batched against row-at-a-time on random tables and predicates (NULLs, affinities,
/// a collation, overflow pages, deleted rows, added columns): identical results, and the batches
/// were used (non-vacuity).
#[test]
fn qt7_batched_scans_answer_as_row_at_a_time_scans() {
    // A budget of a whole page's rows and more: one window per leaf.
    for seed in 1..=6 {
        let decoded = differential(seed, Some(1 << 20));
        assert!(decoded > 0, "seed {seed}: no row was read from a batch");
    }
    // The budget as it runs: the measured L2 cache and the statement's memory budget. Where the
    // cache size can't be read the scan reads row-at-a-time, with the same answers.
    let decoded = differential(11, None);
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        assert!(decoded > 0, "the measured budget batched no row");
    } else {
        assert_eq!(decoded, 0, "batched without a measured cache size");
    }
}

/// T-QT.7.2: a batch budget smaller than one page's cells: the scan decodes several windows a
/// leaf, and below one row's charge falls back to row-at-a-time, by its stated rule, with
/// identical results.
#[test]
fn qt7_budget_below_a_page_falls_back_with_identical_results() {
    for seed in 7..=10 {
        // A few rows' worth: several windows on every leaf.
        let decoded = differential(seed, Some(600));
        assert!(decoded > 0, "seed {seed}: no row was read from a batch");
        // Under one row's charge: nothing is batched, every row is read as before.
        let decoded = differential(seed, Some(1));
        assert_eq!(decoded, 0, "seed {seed}: a row was batched past its budget");
    }
}

/// T-QT.7.3: a program that writes keeps the row-at-a-time read, and a statement prepared with
/// the switch off is prepared again when it turns on.
#[test]
fn qt7_writes_read_row_at_a_time_and_the_switch_reprepares() {
    BUDGET.with(|b| b.set(Some(1 << 20)));
    let conn = open();
    conn.execute("CREATE TABLE t(a, b)").unwrap();
    for i in 0..500 {
        conn.execute(format!("INSERT INTO t VALUES ({i}, 'v{i}')"))
            .unwrap();
    }
    conn.set_vectorized_scans(true);
    DECODED.with(|d| d.set(0));
    conn.execute("INSERT INTO t SELECT a + 1000, b FROM t")
        .unwrap();
    assert_eq!(decoded(), 0, "a writing program read by batches");
    assert_eq!(
        rows(&conn, "SELECT count(*), sum(a) FROM t"),
        rows(&conn, "SELECT count(a), sum(a) FROM t")
    );
    assert!(decoded() > 0, "the read-only scan didn't use batches");
    BUDGET.with(|b| b.set(None));
}

/// The sizes sysfs writes for a cache parse to bytes; anything else is refused.
#[test]
fn qt7_cache_sizes_parse() {
    assert_eq!(super::cache::parse_size("2048K\n"), Some(2048 * 1024));
    assert_eq!(super::cache::parse_size("1M"), Some(1024 * 1024));
    assert_eq!(super::cache::parse_size("524288"), Some(524_288));
    assert_eq!(super::cache::parse_size("0K"), None);
    assert_eq!(super::cache::parse_size("big"), None);
}

/// A filter of comparisons, ranges, IN lists, IS NULL, AND and OR over any column.
fn filter(rng: &mut Rng, depth: u32) -> String {
    let column = ["a", "b", "c", "d", "e", "f", "g"][rng.below(7) as usize];
    let choices = if depth == 0 { 6 } else { 8 };
    match rng.below(choices) {
        0 => format!("{column} IS NULL"),
        1 => format!("{column} IS NOT NULL"),
        2 => {
            let op = ["=", "<>", "<", "<=", ">", ">="][rng.below(6) as usize];
            format!("{column} {op} {}", literal(rng))
        }
        3 => format!("{column} BETWEEN {} AND {}", literal(rng), literal(rng)),
        4 => format!(
            "{column} IN ({}, {}, {})",
            literal(rng),
            literal(rng),
            literal(rng)
        ),
        5 => format!(
            "{column} > {} AND {column} < {}",
            rng.below(60),
            rng.below(100)
        ),
        6 => format!(
            "({}) AND ({})",
            filter(rng, depth - 1),
            filter(rng, depth - 1)
        ),
        _ => format!(
            "({}) OR ({})",
            filter(rng, depth - 1),
            filter(rng, depth - 1)
        ),
    }
}

/// One seed of filtered aggregates: identical answers (an error included: SUM's integer
/// overflow) switch on and off. Returns the rows the fused loops ran.
fn fused_differential(seed: u64, budget: Option<usize>) -> usize {
    BUDGET.with(|b| b.set(budget));
    FUSED.with(|f| f.set(0));
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let conn = open();
    build(&conn, &mut rng);
    let mut queries = vec![
        "SELECT count(*), count(a), sum(a), total(b), avg(e), min(c), max(c), min(d), max(f) FROM t".to_string(),
        "SELECT sum(e), avg(b), min(g), max(g), count(g) FROM t".to_string(),
    ];
    for _ in 0..16 {
        let p = filter(&mut rng, 2);
        queries.push(format!(
            "SELECT count(*), count(a), sum(a), total(b), avg(e), min(c), max(c), min(d), max(f), count(g) FROM t WHERE {p}"
        ));
        queries.push(format!(
            "SELECT count(*), sum(e), avg(b), min(a), max(b) FROM t WHERE {p}"
        ));
        queries.push(format!(
            "SELECT c, count(*), sum(a), max(b) FROM t WHERE {p} GROUP BY c ORDER BY c"
        ));
    }
    for sql in queries {
        conn.set_vectorized_scans(false);
        let row_at_a_time = rows(&conn, &sql);
        conn.set_vectorized_scans(true);
        let batched = rows(&conn, &sql);
        assert_eq!(
            batched, row_at_a_time,
            "seed {seed}, budget {budget:?}: {sql}"
        );
    }
    conn.set_vectorized_scans(false);
    BUDGET.with(|b| b.set(None));
    fused()
}

/// T-QT.7.4: filters (comparisons, ranges, IN, IS NULL, AND, OR) and aggregates (COUNT, SUM
/// with its overflow error, TOTAL, AVG, MIN, MAX) run over whole batches by selection, against
/// row-at-a-time on random tables of every storage class, NULLs, a collation, overflow rows and
/// added columns: identical answers, and the fused loops ran rows (non-vacuity).
#[test]
fn qt7_batch_filters_and_aggregates_answer_as_row_at_a_time() {
    for seed in 21..=26 {
        let ran = fused_differential(seed, Some(1 << 20));
        assert!(ran > 0, "seed {seed}: no row ran in a fused loop");
    }
    // Windows of a few rows: a fused run ends at each window's edge.
    let ran = fused_differential(27, Some(600));
    assert!(ran > 0, "a small budget ran no row in a fused loop");
    // Under one row's charge nothing is batched, so nothing is fused.
    assert_eq!(fused_differential(28, Some(1)), 0, "fused past the budget");
}

/// T-QT.7.5: a fused loop leaves the cursor where row-at-a-time would: rows past a batch, a
/// row spilling to overflow pages amid the batch, and the scan's end all read as before.
#[test]
fn qt7_fused_loop_resumes_row_at_a_time_where_the_batch_ends() {
    BUDGET.with(|b| b.set(Some(1 << 20)));
    FUSED.with(|f| f.set(0));
    let conn = open();
    conn.execute("CREATE TABLE t(a, b)").unwrap();
    for i in 0..2_000 {
        let b = if i % 97 == 0 {
            format!("X'{}'", "cd".repeat(5_000))
        } else {
            format!("{i}")
        };
        conn.execute(format!("INSERT INTO t VALUES ({i}, {b})"))
            .unwrap();
    }
    let sql = "SELECT count(*), sum(a), min(a), max(a), count(b) FROM t WHERE a % 1 = 0 OR a >= 0";
    let sql2 = "SELECT count(*), sum(a), min(a), max(a), count(b) FROM t WHERE a >= 0";
    conn.set_vectorized_scans(true);
    assert_eq!(rows(&conn, sql2), vec!["[Numeric(Integer(2000)), Numeric(Integer(1999000)), Numeric(Integer(0)), Numeric(Integer(1999)), Numeric(Integer(2000))]".to_string()]);
    assert!(fused() > 0, "the fused loop didn't run");
    conn.set_vectorized_scans(false);
    let off = rows(&conn, sql);
    conn.set_vectorized_scans(true);
    assert_eq!(rows(&conn, sql), off);
    conn.set_vectorized_scans(false);
    BUDGET.with(|b| b.set(None));
}

thread_local! {
    static CUT: Cell<usize> = const { Cell::new(0) };
    static SAME_GROUP: Cell<usize> = const { Cell::new(0) };
    static FOUND: Cell<usize> = const { Cell::new(0) };
}

/// A fused group step ran: `cut` when a full group table refused a row past the run's first
/// (`fused.rs`).
pub(super) fn note_cut(cut: bool) {
    CUT.with(|c| c.set(c.get() + usize::from(cut)));
}

/// A row of a fused group step: of the group stepped just before it (its find and the save
/// before it skipped), or found.
pub(super) fn note_same_group(same: bool) {
    if same {
        SAME_GROUP.with(|s| s.set(s.get() + 1));
    } else {
        FOUND.with(|f| f.set(f.get() + 1));
    }
}

/// The counters of fused group steps: (rows found, rows of the group before them, runs cut
/// short by a full table).
fn group_counters() -> (usize, usize, usize) {
    (
        FOUND.with(Cell::get),
        SAME_GROUP.with(Cell::get),
        CUT.with(Cell::get),
    )
}

fn reset_group_counters() {
    FOUND.with(|f| f.set(0));
    SAME_GROUP.with(|s| s.set(0));
    CUT.with(|c| c.set(0));
}

/// Every query's answer with the switch on equals its answer with it off.
fn same_answers(conn: &Arc<Connection>, queries: &[String], what: &str) {
    for sql in queries {
        conn.set_vectorized_scans(false);
        let row_at_a_time = rows(conn, sql);
        conn.set_vectorized_scans(true);
        let batched = rows(conn, sql);
        assert_eq!(batched, row_at_a_time, "{what}: {sql}");
    }
    conn.set_vectorized_scans(false);
}

/// Bounds the group table of a new database's statements so it fills: the statement's memory
/// budget is the connection's page cache, here its fewest pages (`CacheSize::MIN`) of the
/// smallest page size, as a hash table's share of it.
fn small_budget(conn: &Arc<Connection>) {
    conn.execute("PRAGMA page_size = 512").unwrap();
    conn.execute(format!(
        "PRAGMA cache_size = {}",
        crate::storage::page_cache::MINIMUM_PAGE_CACHE_SIZE_IN_PAGES
    ))
    .unwrap();
}

/// One seed of hash GROUP BYs, filtered and not, one key and two, over every storage class,
/// NULLs, a collation, overflow rows and added columns; `small` bounds the group table so it
/// fills mid-scan. Returns the group counters.
fn group_differential(seed: u64, small: bool) -> (usize, usize, usize) {
    BUDGET.with(|b| b.set(Some(1 << 20)));
    reset_group_counters();
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let conn = open();
    if small {
        small_budget(&conn);
    }
    build(&conn, &mut rng);
    let mut queries = vec![
        "SELECT c, count(*), sum(a), max(b) FROM t GROUP BY c".to_string(),
        "SELECT a, b, count(*), avg(e), total(b) FROM t GROUP BY a, b".to_string(),
        "SELECT e, f, min(c), max(d), count(g), sum(a) FROM t GROUP BY e, f".to_string(),
        "SELECT g, b, count(*), min(a) FROM t GROUP BY g, b".to_string(),
        "SELECT a, c, count(*) FROM t GROUP BY a, c ORDER BY 3 DESC, 1, 2".to_string(),
    ];
    for _ in 0..8 {
        let p = filter(&mut rng, 2);
        queries.push(format!(
            "SELECT c, count(*), sum(a), max(b), avg(e) FROM t WHERE {p} GROUP BY c"
        ));
        queries.push(format!(
            "SELECT a, b, count(*), total(e), min(f) FROM t WHERE {p} GROUP BY a, b"
        ));
    }
    same_answers(
        &conn,
        &queries,
        &format!("seed {seed}, small budget {small}"),
    );
    BUDGET.with(|b| b.set(None));
    group_counters()
}

/// T-QT.7.6: a hash GROUP BY's step runs over whole batches (the opcodes' own GroupFind and
/// GroupSave for each selected row in order) against row-at-a-time, on random tables and
/// filters: identical answers with the group table whole and with it filling mid-batch, and
/// the batch group step ran (non-vacuity).
#[test]
fn qt7_batch_group_by_answers_as_row_at_a_time() {
    for seed in 31..=34 {
        let (found, _, cut) = group_differential(seed, false);
        assert!(found > 0, "seed {seed}: no row ran in a batch group step");
        assert_eq!(cut, 0, "seed {seed}: the whole table refused a row");
    }
    let mut cut_runs = 0;
    for seed in 35..=38 {
        let (found, _, cut) = group_differential(seed, true);
        assert!(found > 0, "seed {seed}: no row ran in a batch group step");
        cut_runs += cut;
    }
    assert!(cut_runs > 0, "no group table filled mid-batch");
}

/// T-QT.7.7: rows of one group in a row skip the save and the next find; a group table that
/// fills mid-batch ends the run before the row it refused, which goes on row-at-a-time to the
/// sorter: the answers are the row-at-a-time ones either way.
#[test]
fn qt7_batch_group_by_skips_runs_of_a_group_and_ends_where_the_table_fills() {
    BUDGET.with(|b| b.set(Some(1 << 20)));
    let queries = [
        "SELECT k, count(*), sum(v), min(v), max(v) FROM u GROUP BY k".to_string(),
        "SELECT k, count(*), avg(v) FROM u WHERE v <> 4 AND v < 11 OR v = 12 GROUP BY k"
            .to_string(),
    ];
    // Runs of seven rows a group, each key a hundred and more bytes.
    let table = |small: bool| {
        let conn = open();
        if small {
            small_budget(&conn);
        }
        conn.execute("CREATE TABLE u(k, v)").unwrap();
        for i in 0..4_200 {
            conn.execute(format!(
                "INSERT INTO u VALUES ('{}-{}', {})",
                i / 7,
                "k".repeat(100),
                i % 13
            ))
            .unwrap();
        }
        conn
    };
    reset_group_counters();
    same_answers(&table(false), &queries, "whole table");
    let (found, same, cut) = group_counters();
    assert!(found > 0, "no row ran in a batch group step");
    assert!(same > found, "runs of one group took a find a row");
    assert_eq!(cut, 0, "the whole table refused a row");
    reset_group_counters();
    same_answers(&table(true), &queries, "full table");
    let (found, _, cut) = group_counters();
    assert!(found > 0, "no row ran in a batch group step");
    assert!(cut > 0, "the table didn't fill mid-batch");
    BUDGET.with(|b| b.set(None));
}
