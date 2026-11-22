//! G5 step 4's harness (AC-G5.4.1: both prototypes' scan and write costs): zone maps against
//! cracking against a plain scan, through the engine, on filter_no_index-like filters with writes
//! interleaved. The coordinator runs it at the suite's size and records it in bench/; the run here
//! is a smoke instance (it asserts only that the three count the same rows).
//!
//!   AMPHORA_SKIP_ROWS=10000000 AMPHORA_SKIP_SCANS=50 AMPHORA_SKIP_WRITES=100 \
//!     cargo test -p amphora-engine --release --test skipping_bench -- --ignored --nocapture
//!
//! The table is an Amphora database file (under the target directory) with `items`' columns the
//! filters read (bench/src/suite.rs): `qty` uniform over 0..100 and unrelated to the rowid,
//! `price` over 0..1000, `created` time-ordered (it follows the rowid), and a text body. Two
//! classes: `qty = ? AND price < ?` (filter_no_index itself) and `created BETWEEN ? AND ? AND
//! price < ?`. Each scan is preceded by AMPHORA_SKIP_WRITES writes through SQL, each handed to the
//! summary's hook (a third append, a third update the column, a third delete).
//!
//! Zone maps are built from the table's own leaves (`zone_map_of_table`): a zone is a leaf, so
//! its size is the file's, never a picked run size. A zone-map scan runs the filter over each
//! candidate rowid range (`id BETWEEN ? AND ?`); a cracking scan runs it on each candidate rowid.
//!
//! Printed per class and prototype: the summary's build time and size, candidates per scan, and
//! nanoseconds per scan (the summary's answer and the engine's reads) and per write (the hook
//! alone; the SQL write is the same for all three). Times are wall clock on whatever host runs it:
//! read them as the coordinator's recorded run, never as a test's assertion.

use amphora_engine::skipping::{zone_map_of_table, Candidates, CrackerColumn, RowWrite, Skipping};
use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Statement, Value,
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::num::NonZero;
use std::sync::Arc;
use std::time::Instant;

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn open(path: &str) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, path, options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Numeric(Numeric::Integer(i)) => Some(*i),
        _ => None,
    }
}

/// Runs a prepared statement with `params`; its rows.
fn run(stmt: &mut Statement, params: &[i64]) -> Vec<Vec<Value>> {
    stmt.reset().unwrap();
    for (i, p) in params.iter().enumerate() {
        stmt.bind_at(NonZero::new(i + 1).unwrap(), Value::from_i64(*p))
            .unwrap();
    }
    stmt.run_collect_rows().unwrap()
}

fn count(stmt: &mut Statement, params: &[i64]) -> i64 {
    run(stmt, params)
        .first()
        .and_then(|row| row.first())
        .and_then(int)
        .unwrap()
}

struct Class {
    name: &'static str,
    column: &'static str,
    /// The filter's bounds on the summarized column, given the largest rowid so far.
    bounds: fn(&mut ChaCha8Rng, i64) -> (i64, i64),
}

fn load(conn: &Arc<Connection>, rows: usize) {
    conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER NOT NULL, price REAL NOT NULL, created INTEGER NOT NULL, body TEXT NOT NULL)")
        .unwrap();
    conn.execute(format!(
        "INSERT INTO items (qty, price, created, body) SELECT abs(random()) % 100, abs(random()) % 1000, \
         value * 10 + abs(random()) % 10, hex(randomblob(40 + abs(random()) % 60)) FROM generate_series(1, {rows})"
    ))
    .unwrap();
}

fn build(conn: &Arc<Connection>, class: &Class, prototype: &str) -> (Skipping, usize) {
    match prototype {
        "zone_maps" => {
            let zones = zone_map_of_table(conn, "items", class.column).unwrap();
            let size = zones.zone_count();
            (Skipping::ZoneMaps(zones), size)
        }
        "cracking" => {
            let mut all = conn
                .prepare(format!("SELECT id, {} FROM items", class.column))
                .unwrap();
            let rows: Vec<(i64, Option<i64>)> = run(&mut all, &[])
                .iter()
                .map(|row| (int(&row[0]).unwrap(), int(&row[1])))
                .collect();
            let size = rows.len();
            (Skipping::Cracking(CrackerColumn::new(rows)), size)
        }
        _ => (Skipping::Off, 0),
    }
}

/// One write through SQL, as the hook takes it; None when the rowid picked holds no row.
fn write(
    conn: &Arc<Connection>,
    rng: &mut ChaCha8Rng,
    class: &Class,
    next: &mut i64,
) -> Option<RowWrite> {
    let c = class.column;
    let fresh = |rng: &mut ChaCha8Rng, rowid: i64| match c {
        "qty" => rng.random_range(0..100),
        _ => rowid * 10 + rng.random_range(0..10),
    };
    let pick = rng.random_range(0..3);
    if pick == 0 {
        *next += 1;
        let rowid = *next;
        let (qty, price, created) = (
            rng.random_range(0..100),
            rng.random_range(0..1000),
            rowid * 10 + rng.random_range(0..10),
        );
        conn.execute(format!("INSERT INTO items (id, qty, price, created, body) VALUES ({rowid}, {qty}, {price}, {created}, hex(randomblob(50)))"))
            .unwrap();
        let value = if c == "qty" { qty } else { created };
        return Some(RowWrite::Insert {
            rowid,
            value: Some(value),
        });
    }
    let rowid = rng.random_range(1..=*next);
    let mut stmt = conn
        .prepare(format!("SELECT {c} FROM items WHERE id = ?1"))
        .unwrap();
    let old = run(&mut stmt, &[rowid]).first().map(|row| int(&row[0]))?;
    if pick == 1 {
        let new = fresh(rng, rowid);
        conn.execute(format!("UPDATE items SET {c} = {new} WHERE id = {rowid}"))
            .unwrap();
        Some(RowWrite::Update {
            rowid,
            old,
            new: Some(new),
        })
    } else {
        conn.execute(format!("DELETE FROM items WHERE id = {rowid}"))
            .unwrap();
        Some(RowWrite::Delete { rowid, old })
    }
}

fn run_class(class: &Class, prototype: &str, rows: usize, scans: usize, writes: usize) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let conn = open(dir.path().join("items.db").to_str().unwrap());
    load(&conn, rows);
    let mut rng = ChaCha8Rng::seed_from_u64(42);
    let started = Instant::now();
    let (mut skipping, size) = build(&conn, class, prototype);
    let build_ns = started.elapsed().as_nanos();
    let filter = format!("{} BETWEEN ?1 AND ?2 AND price < ?3", class.column);
    let mut plain = conn
        .prepare(format!("SELECT COUNT(*) FROM items WHERE {filter}"))
        .unwrap();
    let mut ranged = conn
        .prepare(format!(
            "SELECT COUNT(*) FROM items WHERE id BETWEEN ?4 AND ?5 AND {filter}"
        ))
        .unwrap();
    let mut one = conn
        .prepare(format!(
            "SELECT COUNT(*) FROM items WHERE id = ?4 AND {filter}"
        ))
        .unwrap();
    let mut next = rows as i64;
    let (mut write_ns, mut writes_made, mut scan_ns) = (0u128, 0u128, 0u128);
    let (mut candidates, mut matched) = (0usize, 0i64);
    for _ in 0..scans {
        for _ in 0..writes {
            let Some(w) = write(&conn, &mut rng, class, &mut next) else {
                continue;
            };
            let t = Instant::now();
            skipping.on_write(w).unwrap();
            write_ns += t.elapsed().as_nanos();
            writes_made += 1;
        }
        let (lo, hi) = (class.bounds)(&mut rng, next);
        let price = rng.random_range(0..1000);
        let t = Instant::now();
        let found: i64 = match skipping.candidates(lo, hi).unwrap() {
            Candidates::All => count(&mut plain, &[lo, hi, price]),
            Candidates::Ranges(ranges) => {
                candidates += ranges.len();
                ranges
                    .iter()
                    .map(|r| count(&mut ranged, &[lo, hi, price, r.first, r.last]))
                    .sum()
            }
            Candidates::Rowids(rowids) => {
                candidates += rowids.len();
                rowids
                    .iter()
                    .map(|r| count(&mut one, &[lo, hi, price, *r]))
                    .sum()
            }
        };
        scan_ns += t.elapsed().as_nanos();
        assert_eq!(
            found,
            count(&mut plain, &[lo, hi, price]),
            "{} {prototype}",
            class.name
        );
        matched += found;
    }
    let scans_n = scans.max(1);
    println!(
        "{:<14} {:<10} rows {:>9} size {:>9} build_ns {:>12} candidates/scan {:>9} matched/scan {:>8} ns/scan {:>12} ns/write {:>8}",
        class.name,
        prototype,
        rows,
        size,
        build_ns,
        candidates / scans_n,
        matched / scans_n as i64,
        scan_ns / scans_n as u128,
        write_ns / writes_made.max(1),
    );
}

/// Do: run each class under plain scans, zone maps and cracking with writes between scans. Expect:
/// every scan's count equals the plain scan's (the costs are printed for the record).
#[test]
#[ignore = "G5 step 4's harness: the coordinator runs it (see the module's comment)"]
fn g5_s4_skipping_harness() {
    let rows = env("AMPHORA_SKIP_ROWS", 20_000);
    let scans = env("AMPHORA_SKIP_SCANS", 10);
    let writes = env("AMPHORA_SKIP_WRITES", 10);
    let classes = [
        Class {
            name: "qty_eq",
            column: "qty",
            bounds: |rng, _| {
                let q = rng.random_range(0..100);
                (q, q)
            },
        },
        Class {
            name: "created_range",
            column: "created",
            bounds: |rng, next| {
                let lo = rng.random_range(0..next * 10);
                (lo, lo + next / 10)
            },
        },
    ];
    for class in &classes {
        for prototype in ["plain", "zone_maps", "cracking"] {
            run_class(class, prototype, rows, scans, writes);
        }
    }
}
