//! A store in groups (CP-T1.1; AC-T1.1.1: no update dropped unsaid): rows go to their key's group
//! and are read back from it, each group's file holds only its keys' rows, and a batch with rows in
//! more than one group is refused, nothing of it written.

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

use std::collections::HashSet;
use std::time::{Duration, Instant};

use amphora::db::Db;
use amphora::groups::Groups;
use amphora::store::{Checkpointed, Config, Engine, Flush, Opener, Refused};
use amphora_engine::Value;

struct Events {
    db: Db,
}

struct Row {
    key: String,
    seq: i64,
}

const INSERT: &str = "INSERT INTO events (key, seq) VALUES (?1, ?2)";

impl Engine for Events {
    type Row = Row;
    type Reader = Db;

    fn key(row: &Row) -> &str {
        &row.key
    }

    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<usize> {
        self.db.transaction(|db| {
            for r in rows {
                db.query(
                    INSERT,
                    [Value::from_text(r.key.clone()), Value::from_i64(r.seq)],
                    |_| Ok(true),
                )?;
            }
            Ok(rows.len())
        })
    }

    fn flusher(&self) -> Option<Flush> {
        Some(self.db.flusher())
    }

    fn opener(&self) -> Option<Opener<Db>> {
        Some(self.db.opener())
    }

    fn checkpoint(reader: &mut Db) -> anyhow::Result<Checkpointed> {
        reader.checkpoint()
    }

    fn restart_log(&mut self) -> anyhow::Result<bool> {
        self.db.restart_log()
    }
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn row(key: &str, seq: i64) -> Row {
    Row {
        key: key.to_owned(),
        seq,
    }
}

const GROUPS: usize = 4;

fn lab(dir: &std::path::Path) -> Groups<Events> {
    Groups::start(GROUPS, Config::default(), |i| {
        let db = Db::open(&dir.join(format!("events-{i}.db")))?;
        db.exec(
            "CREATE TABLE events (key TEXT, seq INTEGER); CREATE INDEX by_key ON events (key)",
        )?;
        Ok(Events { db })
    })
    .unwrap()
}

const COUNT: &str = "SELECT count(*) FROM events WHERE key = ?1";
const KEYS: &str = "SELECT DISTINCT key FROM events";

fn count(groups: &Groups<Events>, key: &str) -> i64 {
    let (k, w) = (key.to_owned(), key.to_owned());
    let count_on = |db: &mut Db, key: &str| {
        let mut n = 0;
        db.query(COUNT, [Value::from_text(key.to_owned())], |r| {
            n = r.get::<i64>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(true)
        })?;
        Ok(n)
    };
    groups
        .read(
            key,
            soon(),
            move |db| count_on(db, &k),
            move |e| count_on(&mut e.db, &w),
        )
        .unwrap()
}

/// Rows go to their key's group and are read back from it, and each group's file holds only the
/// keys whose group it is.
#[test]
fn rows_go_to_their_keys_group_and_read_back_from_it() {
    let dir = tempfile::tempdir().unwrap();
    let groups = lab(dir.path());
    let keys: Vec<String> = (0..20).map(|i| format!("k{i}")).collect();
    let mut tickets = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        let rows = (0..=i)
            .map(|seq| row(key, i64::try_from(seq).unwrap()))
            .collect();
        tickets.push(groups.put(rows).unwrap());
    }
    for t in &tickets {
        groups.durable(t, soon()).unwrap();
    }
    for (i, key) in keys.iter().enumerate() {
        assert_eq!(count(&groups, key), i64::try_from(i).unwrap() + 1);
    }
    let held = groups
        .read_each(
            soon(),
            false,
            |db| {
                let mut keys = Vec::new();
                db.query(KEYS, [], |r| {
                    keys.push(r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?);
                    Ok(true)
                })?;
                Ok(keys)
            },
            |_| Ok(Vec::new()),
        )
        .unwrap();
    let mut seen = HashSet::new();
    for (group, keys) in held.iter().enumerate() {
        for key in keys {
            assert_eq!(groups.group(key), group, "{key} is in group {group}'s file");
            assert!(seen.insert(key.clone()));
        }
    }
    assert_eq!(seen.len(), keys.len());
    // Twenty keys over four groups: more than one group holds some (the spread isn't vacuous).
    assert!(held.iter().filter(|k| !k.is_empty()).count() > 1);
}

/// A batch with rows in more than one group is refused, typed, and nothing of it is written.
#[test]
fn a_batch_spanning_groups_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let groups = lab(dir.path());
    let a = "a".to_owned();
    let b = (0..)
        .map(|i| format!("b{i}"))
        .find(|k| groups.group(k) != groups.group(&a))
        .unwrap();
    assert_eq!(
        groups.put(vec![row(&a, 1), row(&b, 1)]).err(),
        Some(Refused::Groups)
    );
    groups.sync(soon()).unwrap();
    assert_eq!(count(&groups, &a), 0);
    assert_eq!(count(&groups, &b), 0);
}
