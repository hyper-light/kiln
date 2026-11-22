//! G3 step 3 (AC-G3.3.2, `docs/design/store.md` decision 42): allocations per point lookup on each
//! page path, counted by a counting allocator. The test asserts only that the plain path's misses
//! allocate less than today's; the coordinator's measurement of allocations per `point_pk` lookup
//! prints the counts (`--nocapture`).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PagePath, PlatformIO,
    SqliteDialect, Value,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::num::NonZero;
use std::sync::Arc;

/// These tests exercise the pager's page paths (allocations per lookup on each), which the
/// versioned storage doesn't have: with its switch on (`AMPHORA_STORAGE=versioned`, G7 step 2,
/// `AC-NS.2.1`) they say so and pass over, and aren't ported. The versioned storage's own pool is
/// tested in `storage::versioned`.
fn pager_only() -> bool {
    let on = amphora_engine::storage::versioned::switch::asked();
    if on {
        eprintln!("pager-only: this test exercises the pager's page paths (allocations per lookup on each); the versioned storage has none");
    }
    on
}

/// Counts the allocations made on a thread while it has counting on.
struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|on| {
            if on.get() {
                let _ = COUNT.try_with(|n| n.set(n.get() + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn counted(work: impl FnOnce()) -> u64 {
    COUNT.with(|n| n.set(0));
    COUNTING.with(|on| on.set(true));
    work();
    COUNTING.with(|on| on.set(false));
    COUNT.with(|n| n.get())
}

fn open(dir: &std::path::Path, share: bool) -> Arc<Database> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("allocs.db").to_str().unwrap(), options).unwrap();
    if share {
        db.share_pages(4096).unwrap();
    }
    db
}

fn connect(db: &Arc<Database>) -> Arc<Connection> {
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
    conn
}

/// `rows` rows of about 200 bytes, checkpointed into the file.
fn table(db: &Arc<Database>, rows: i64) {
    let conn = connect(db);
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
        .unwrap();
    conn.execute("BEGIN").unwrap();
    for first in (1..=rows).step_by(200) {
        let values: Vec<String> = (first..(first + 200).min(rows + 1))
            .map(|id| format!("({id}, 'v{id}-{}')", "x".repeat(190)))
            .collect();
        conn.execute(format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
}

/// Allocations per lookup of `lookups` point reads by a prepared statement at keys spread by a
/// fixed stride over the table, on a connection on `path` caching `cache` pages.
fn per_lookup(db: &Arc<Database>, path: PagePath, cache: i64, rows: i64, lookups: i64) -> f64 {
    let conn = connect(db);
    conn.execute(format!("PRAGMA cache_size = {cache}"))
        .unwrap();
    conn.get_pager().set_page_path(path);
    let mut stmt = conn.prepare("SELECT v FROM t WHERE id = ?").unwrap();
    let mut lookup = |id: i64| {
        stmt.reset().unwrap();
        stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(id))
            .unwrap();
        let got = stmt.run_collect_rows().unwrap();
        assert_eq!(got.len(), 1, "row {id}");
    };
    // A stride coprime with the row count visits every row once per pass, far apart.
    let key = |i: i64| 1 + (i * 7919) % rows;
    for i in 0..lookups {
        lookup(key(i));
    }
    let n = counted(|| {
        for i in lookups..lookups * 2 {
            lookup(key(i));
        }
    });
    n as f64 / lookups as f64
}

/// Do: point reads of a table four times a 200-page cache (most miss), and of one the cache holds
/// (every read hits), on each path, with and without shared pages. Expect: the plain path's misses
/// allocate less than today's; its hits no more.
#[test]
fn g3s3_allocations_per_point_lookup_on_each_page_path() {
    if pager_only() {
        return;
    }
    let rows = 16_000;
    for share in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path(), share);
        table(&db, rows);
        for (cache, what) in [(200, "misses"), (4000, "hits")] {
            let today = per_lookup(&db, PagePath::Today, cache, rows, 2000);
            let plain = per_lookup(&db, PagePath::Plain, cache, rows, 2000);
            println!(
                "shared {share}, {what}: allocations per lookup today {today:.2}, plain {plain:.2}"
            );
            if what == "misses" && !share {
                assert!(plain < today, "{what}: plain {plain}, today {today}");
            } else {
                assert!(plain <= today, "{what}: plain {plain}, today {today}");
            }
        }
    }
}
