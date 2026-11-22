//! A store's files (docs/design/store.md, decision 12): a new database's files are written in place
//! on a file system that would copy them (btrfs's no-copy-on-write flag), and each page is
//! checksummed, so a page corrupted on disk is refused rather than read (the guarantee btrfs's own
//! data checksums gave, which the flag turns off).

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::io::{Read, Seek, SeekFrom, Write};

use amphora::db::Db;
use amphora_engine::Value;

/// A directory on the build's own file system (not a tmpfs `/tmp`, which takes no such flag).
fn lab() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// A new database's file and log carry the no-copy-on-write flag wherever the file system takes it
/// (a probe file says whether it does); where it doesn't, nothing changes.
#[cfg(target_os = "linux")]
#[test]
fn a_new_databases_files_are_written_in_place_where_the_file_system_would_copy_them() {
    use rustix::fs::{IFlags, ioctl_getflags, ioctl_setflags};
    let dir = lab();
    let probe = std::fs::File::create(dir.path().join("probe")).unwrap();
    let takes = ioctl_getflags(&probe)
        .and_then(|f| ioctl_setflags(&probe, f | IFlags::NOCOW))
        .is_ok()
        && ioctl_getflags(&probe).is_ok_and(|f| f.contains(IFlags::NOCOW));
    let path = dir.path().join("t.db");
    let db = Db::open(&path).unwrap();
    db.exec("CREATE TABLE t (n INTEGER)").unwrap();
    let mut wal = path.clone().into_os_string();
    wal.push("-wal");
    for file in [path.clone(), wal.into()] {
        let flags = ioctl_getflags(std::fs::File::open(&file).unwrap()).unwrap();
        assert_eq!(
            flags.contains(IFlags::NOCOW),
            takes,
            "{}: the file system takes the flag: {takes}",
            file.display()
        );
    }
}

/// The bytes of the log beside the database at `path`.
fn log_bytes(path: &std::path::Path) -> u64 {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(wal).map_or(0, |m| m.len())
}

/// Do: with a log kept to 64 frames across restarts, commit 500 rows of a page each, their bodies
/// random so that a commit's records (decision 37) don't compress them (more than 500 frames of
/// log), start the log again (every frame copied into the file, no reader reading), commit
/// one row, and start it again. Expect: the first restart cuts the log's file, past its bound, to
/// nothing; the row's commit grows it again to a few frames; the second restart, of a log within its
/// bound, keeps its file's length (decision 13); and every row reads back after opening the database
/// again (docs/design/store.md, decision 31; AC-T1.1.1).
#[test]
fn a_restart_cuts_a_log_past_its_bound_to_nothing_and_keeps_one_within_it() {
    let dir = lab();
    let path = dir.path().join("t.db");
    {
        let mut db = Db::open(&path).unwrap();
        db.keep_log(64);
        db.exec("CREATE TABLE t (n INTEGER PRIMARY KEY, body BLOB NOT NULL)")
            .unwrap();
        db.transaction(|db| {
            for n in 0..500 {
                db.query(
                    "INSERT INTO t (n, body) VALUES (?1, randomblob(4000))",
                    [Value::from_i64(n)],
                    |_| Ok(true),
                )?;
            }
            Ok(())
        })
        .unwrap();
        let high = log_bytes(&path);
        assert!(high > 32 + 500 * 4120, "500 rows took {high} bytes of log");
        assert!(db.restart_log().unwrap());
        assert_eq!(
            log_bytes(&path),
            0,
            "the log's file after a restart, past its bound ({high} bytes before)"
        );
        db.exec("INSERT INTO t (n, body) VALUES (-1, x'00')")
            .unwrap();
        let small = log_bytes(&path);
        assert!(
            small > 32 && small <= 32 + 64 * 4120,
            "one row's commit took {small} bytes of log"
        );
        assert!(db.restart_log().unwrap());
        assert_eq!(
            log_bytes(&path),
            small,
            "a restart within the bound keeps the log's file"
        );
    }
    let db = Db::open(&path).unwrap();
    let mut rows = 0i64;
    db.query("SELECT count(*) FROM t", [], |r| {
        rows = r.get::<i64>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })
    .unwrap();
    assert_eq!(rows, 501);
}

/// A restart's hold on the writer, and the commit and flush after it.
type Times = (std::time::Duration, std::time::Duration);

/// A database whose log's file was grown to 1 GiB, the bound past which a restart cuts it by
/// default (rows of a page of random bytes, which a commit's records don't compress, in
/// transactions of 64 MiB), all of it copied into the file and the log
/// started again with its file kept; then three times a row committed and the log restarted
/// keeping its file, and once a row committed and the log restarted cutting it, each restart timed
/// and then a commit of 4 MiB and a flush of the log after it: the times kept, and the time cut.
fn restarts_kept_then_cut() -> (Vec<Times>, Times) {
    let dir = lab();
    let path = dir.path().join("t.db");
    let mut db = Db::open(&path).unwrap();
    db.exec("CREATE TABLE t (n INTEGER PRIMARY KEY, body BLOB NOT NULL)")
        .unwrap();
    let mut next = 0i64;
    let mut insert = |db: &mut Db, rows: i64| {
        db.transaction(|db| {
            for _ in 0..rows {
                db.query(
                    "INSERT INTO t (n, body) VALUES (?1, randomblob(4000))",
                    [Value::from_i64(next)],
                    |_| Ok(true),
                )?;
                next += 1;
            }
            Ok(())
        })
        .unwrap();
    };
    for _ in 0..16 {
        insert(&mut db, 16_384);
    }
    db.keep_log(u64::MAX);
    assert!(db.restart_log().unwrap());
    let high = log_bytes(&path);
    let mut timed = |db: &mut Db, cut: bool| {
        insert(db, 1);
        let began = std::time::Instant::now();
        assert!(db.restart_log().unwrap());
        let restart = began.elapsed();
        assert_eq!(log_bytes(&path) == 0, cut, "the log cut: {cut}");
        let began = std::time::Instant::now();
        insert(db, 1024);
        db.flusher()().unwrap();
        (restart, began.elapsed())
    };
    let kept = (0..3).map(|_| timed(&mut db, false)).collect();
    assert_eq!(log_bytes(&path), high, "restarts keeping the file");
    db.keep_log(0);
    (kept, timed(&mut db, true))
}

/// Do: on two databases whose logs grew to 1 GiB, time three restarts keeping the log's file and
/// then one cutting it, and the commit of 4 MiB and the flush after each. Expect: each restarts;
/// what cutting the file adds to the writer's hold, and to the commit after it, printed (best of 6
/// kept, best of 2 cut; docs/design/store.md, decision 31's cost).
#[test]
#[ignore = "a timing check: run on its own, in release"]
fn what_cutting_the_log_costs_its_writer() {
    let (mut kept, mut cut) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        let (k, c) = restarts_kept_then_cut();
        kept.extend(k);
        cut.push(c);
    }
    let best = |times: &[Times]| {
        (
            times.iter().map(|t| t.0).min().unwrap(),
            times.iter().map(|t| t.1).min().unwrap(),
        )
    };
    let (kept, cut) = (best(&kept), best(&cut));
    eprintln!(
        "a log of 1 GiB: a restart keeping its file held the writer {:?} and the commit and flush \
         after it took {:?} (best of 6); cutting it, {:?} and {:?} (best of 2)",
        kept.0, kept.1, cut.0, cut.1
    );
}

/// A page of the database file corrupted on disk is refused with an error when read, never handed
/// back as data.
#[test]
fn a_page_corrupted_on_disk_is_refused_not_read() {
    let dir = lab();
    let path = dir.path().join("t.db");
    {
        let mut db = Db::open(&path).unwrap();
        db.exec("CREATE TABLE t (n INTEGER, text TEXT)").unwrap();
        db.transaction(|db| {
            for n in 0..500 {
                db.query(
                    "INSERT INTO t (n, text) VALUES (?1, ?2)",
                    [
                        Value::from_i64(n),
                        Value::from_text(format!("row {n} of five hundred")),
                    ],
                    |_| Ok(true),
                )?;
            }
            Ok(())
        })
        .unwrap();
        // Everything copied from the log into the file, the file flushed, the log started again.
        assert!(db.restart_log().unwrap());
    }
    // The log goes (its frames, still valid on disk until the next write, would be read in place
    // of the file's pages): the file holds every page.
    let mut wal = path.clone().into_os_string();
    wal.push("-wal");
    std::fs::remove_file(&wal).unwrap();
    // The database keeps room for each page's checksum: the header's reserved-space byte (offset
    // 20 of SQLite's format) says 8.
    let header = std::fs::read(&path).unwrap();
    assert_eq!(header[20], 8, "the pages keep no room for a checksum");
    // A byte in the middle of the table's root page (page 2, the first table made) flipped.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(
        header.len() > 2 * 4096,
        "the table took more than its root page"
    );
    let at = 4096 + 2048;
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::Start(at)).unwrap();
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(at)).unwrap();
    file.write_all(&[byte[0] ^ 0xff]).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let db = Db::open(&path).unwrap();
    let mut sum = 0i64;
    let read = db.query("SELECT n FROM t", [], |r| {
        sum += r.get::<i64>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    });
    let err = read.expect_err("a corrupted page was read as data");
    assert!(
        format!("{err:#}").to_lowercase().contains("checksum"),
        "refused, but not for its checksum: {err:#}"
    );
}
