//! The storage layer.
//!
//! This module contains the storage layer for Limbo. The storage layer is
//! responsible for managing access to the database and its pages. The main
//! interface to the storage layer is the `Pager` struct, which is
//! responsible for managing the database file and the pages it contains.
//!
//! Pages in a database are stored in one of the following to data structures:
//! `DatabaseStorage` or `Wal`. The `DatabaseStorage` trait is responsible
//! for reading and writing pages to the database file, either local or
//! remote. The `Wal` struct is responsible for managing the write-ahead log
//! for the database, also either local or remote.
pub(crate) mod btree;
pub(crate) mod buffer_pool;
pub(crate) mod changes;
pub(crate) mod checksum;
pub mod database;
pub(crate) mod encryption;
pub(crate) mod journal_mode;
pub(crate) mod packed;
#[cfg(feature = "aristo-instr")]
pub mod page_cache;
#[cfg(not(feature = "aristo-instr"))]
pub(crate) mod page_cache;
pub(crate) mod page_transform;
#[allow(clippy::arc_with_non_send_sync)]
pub(crate) mod pager;
pub mod shared_pages;
#[cfg(host_shared_wal)]
#[allow(dead_code)]
pub(crate) mod shared_wal_coordination;
#[allow(dead_code)]
pub(super) mod slot_bitmap;
pub mod sqlite3_ondisk;
mod state_machines;
pub(crate) mod subjournal;
pub mod versioned;
#[allow(clippy::arc_with_non_send_sync)]
pub(crate) mod wal;

/// Amphora: a new database file in WAL mode, its first page written and its log empty, made by the
/// engine itself, as Turso's tests made it with SQLite through rusqlite (which compiles C; Amphora's
/// build compiles none).
#[cfg(test)]
pub(crate) fn new_wal_database(path: &std::path::Path) {
    use std::sync::Arc;
    let io: Arc<dyn crate::IO> = Arc::new(crate::PlatformIO::new().unwrap());
    let db = crate::Database::open_file(io, path.to_str().unwrap(), Arc::new(crate::SqliteDialect))
        .unwrap();
    let conn = db.connect(crate::ConnectOpts::new()).unwrap();
    conn.execute("PRAGMA journal_mode = 'wal'").unwrap();
    // A write of the header, as SQLite's change of journal mode made.
    conn.execute("PRAGMA user_version = 0").unwrap();
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    conn.close().unwrap();
}

#[macro_export]
macro_rules! return_corrupt {
    ($($arg:tt)*) => {
        return Err(LimboError::Corrupt(format!($($arg)*)).into());
    };
}
