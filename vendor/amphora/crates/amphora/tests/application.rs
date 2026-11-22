//! U05, CP-T1.8 (docs/design/replication.md, "The application floor, replicated", "Today"): the
//! standalone application floor. A handle binds its application's claim; the floor is raised in the
//! original writer and never falls; every application transaction below it is refused.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::time::{Duration, Instant};

use amphora::database::Database;
use amphora_api::Refusal;

fn until() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

fn insert(database: &Database, n: i64) -> Result<(), Refusal> {
    database.call(until(), move |c| {
        c.execute("INSERT INTO items VALUES (?1)", [n])?;
        Ok(())
    })
}

fn count(database: &Database) -> i64 {
    database
        .call(until(), |c| {
            Ok(c.query_row("SELECT count(*) FROM items", [], |r| r.get(0usize))?)
        })
        .unwrap()
}

/// U05 on the standalone owner. Do: read the floor's status; write unclaimed; raise the floor to 7
/// through a handle
/// bound to version 7; write through it, through a handle of version 6, through an unclaimed
/// handle and through another application's; raise to 5. Expect: the unclaimed write before
/// the floor applies; after it, only the version-7 write applies and each other is refused
/// Fenced inside its transaction; the raise to 5 answers the floor still at 7, and version 6
/// is still refused; floor_status reads no floor before and 7 for kiln.host-store after, through a
/// handle below it too, naming no members.
#[test]
fn a_write_below_the_standalone_floor_is_refused_and_the_floor_never_falls() {
    let dir = tempfile::tempdir().unwrap();
    let database = Database::open(&dir.path().join("floor.db"), until()).unwrap();
    database
        .call(until(), |c| {
            c.execute_batch("CREATE TABLE items(n INTEGER)")?;
            Ok(())
        })
        .unwrap();
    insert(&database, 1).unwrap();

    let none = database.floor_status(until()).unwrap();
    assert_eq!((none.application, none.version), (None, 0));

    let current = database.for_application("kiln.host-store", 7).unwrap();
    let boundary = current.writer_floor("kiln.host-store", 7, until()).unwrap();
    // The floor, read the same way a replica's is, by any handle, a stale one included.
    let status = database
        .for_application("kiln.host-store", 6)
        .unwrap()
        .floor_status(until())
        .unwrap();
    assert_eq!(
        (
            status.application.as_deref(),
            status.version,
            status.members.len()
        ),
        (Some("kiln.host-store"), 7, 0)
    );
    assert_eq!(boundary.version(), 7);
    boundary.require(&current).unwrap();
    insert(&current, 2).unwrap();

    let older = database.for_application("kiln.host-store", 6).unwrap();
    let other = database.for_application("another.app", 9).unwrap();
    for refused in [&older, &database, &other] {
        assert!(
            matches!(insert(refused, 3), Err(Refusal::Fenced(_))),
            "a write below the floor applied"
        );
    }

    let lowered = current.writer_floor("kiln.host-store", 5, until()).unwrap();
    assert_eq!(lowered.version(), 7, "the floor fell");
    assert!(matches!(insert(&older, 4), Err(Refusal::Fenced(_))));
    assert_eq!(count(&current), 2);
}

/// U05, CP-T1.8 (CLAUDE.md section 8; amphora-seam-owner's review of step 4, nits): a read is
/// checked against the floor as a write is, standalone as replicated. Do: raise the floor to 7;
/// read through the version-7 handle and through a version-6 one. Expect: the version-7 read
/// answers; the version-6 read is refused Fenced, as a replicated one is.
#[test]
fn a_read_below_the_standalone_floor_is_refused_as_a_replicated_one_is() {
    let dir = tempfile::tempdir().unwrap();
    let database = Database::open(&dir.path().join("floor.db"), until()).unwrap();
    database
        .call(until(), |c| {
            c.execute_batch("CREATE TABLE items(n INTEGER)")?;
            Ok(())
        })
        .unwrap();
    let current = database.for_application("kiln.host-store", 7).unwrap();
    current.writer_floor("kiln.host-store", 7, until()).unwrap();
    let read = |database: &Database| {
        database.query(until(), |c| {
            Ok(c.query_row("SELECT count(*) FROM items", [], |r| {
                r.get::<_, i64>(0usize)
            })?)
        })
    };
    assert_eq!(read(&current), Ok(0));
    let older = database.for_application("kiln.host-store", 6).unwrap();
    assert!(
        matches!(read(&older), Err(Refusal::Fenced(_))),
        "a read below the floor answered"
    );
}

/// U05: a claim and a raise are checked before anything is written. Do: bind a version 0 and a
/// malformed name; raise through an unbound handle, past the handle's own version, and for an
/// application other than the handle's. Expect: Invalid for each, and no floor installed: an
/// unclaimed write still applies.
#[test]
fn malformed_claims_and_raises_are_refused_before_any_floor_is_installed() {
    let dir = tempfile::tempdir().unwrap();
    let database = Database::open(&dir.path().join("claims.db"), until()).unwrap();
    database
        .call(until(), |c| {
            c.execute_batch("CREATE TABLE items(n INTEGER)")?;
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        database.for_application("kiln.host-store", 0),
        Err(Refusal::Invalid { .. })
    ));
    assert!(matches!(
        database.for_application("", 1),
        Err(Refusal::Invalid { .. })
    ));
    let bound = database.for_application("kiln.host-store", 3).unwrap();
    for refused in [
        database.writer_floor("kiln.host-store", 1, until()),
        bound.writer_floor("kiln.host-store", 4, until()),
        bound.writer_floor("another.app", 1, until()),
        bound.writer_floor("kiln.host-store", 0, until()),
    ] {
        assert!(matches!(refused, Err(Refusal::Invalid { .. })));
    }
    insert(&database, 1).unwrap();
    assert_eq!(count(&database), 1);
}
