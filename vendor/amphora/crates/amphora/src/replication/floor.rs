//! The replicated application floor's rows (replication.md, "The application floor, replicated":
//! "Joins", "Attestation", and the upgrade that introduces join rows). Pure SQL over one session's
//! database; the owner and the machine decide when each runs.
//!
//! Every table here is created inside a resolved session (CREATE TABLE IF NOT EXISTS), so it
//! reaches every member as pages: a member running a binary from before the floor applies the
//! same pages and stays byte-identical. The only rows written locally are the join rows a change
//! makes, by `apply_change`, and only once marker J is applied.

use amphora_engine::Value;
use hyper_raft::proto::ConfState;
use hyper_raft::wire::Record;

use crate::db::Db;

const ATTESTATIONS: &str = "CREATE TABLE IF NOT EXISTS __amphora_attestations (member INTEGER NOT NULL, joined INTEGER NOT NULL, application TEXT NOT NULL, highest INTEGER NOT NULL CHECK(highest > 0), seen INTEGER NOT NULL, PRIMARY KEY(member, joined, application))";
const JOINS: &str = "CREATE TABLE IF NOT EXISTS __amphora_joins (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), tracked INTEGER NOT NULL)";
const MEMBERS: &str = "CREATE TABLE IF NOT EXISTS __amphora_members (member INTEGER PRIMARY KEY, joined INTEGER NOT NULL)";
const REQUESTED: &str = "CREATE TABLE IF NOT EXISTS __amphora_attest_requested (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), at INTEGER NOT NULL)";

/// One member's attestation, as a proof reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Attested {
    pub(crate) application: String,
    pub(crate) highest: u32,
}

/// Every member of a configuration: its voters and learners, and in a joint configuration the
/// voters it leaves and the learners it makes, as the floor's proof counts them.
pub(crate) fn members(configuration: &ConfState) -> Vec<u64> {
    let mut members: Vec<u64> = configuration
        .voters
        .iter()
        .chain(&configuration.learners)
        .chain(&configuration.voters_outgoing)
        .chain(&configuration.learners_next)
        .copied()
        .collect();
    members.sort_unstable();
    members.dedup();
    members
}

fn column(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("floor row: {error}")
}

fn integer(n: u64) -> anyhow::Result<Value> {
    Ok(Value::from_i64(i64::try_from(n)?))
}

fn table(db: &Db, name: &'static str) -> anyhow::Result<bool> {
    let mut present = false;
    db.query(
        "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [Value::from_text(name.to_string())],
        |_| {
            present = true;
            Ok(false)
        },
    )?;
    Ok(present)
}

/// The index at which joins began to be tracked (marker J), if they have.
pub(crate) fn tracked(db: &Db) -> anyhow::Result<Option<u64>> {
    if !table(db, "__amphora_joins")? {
        return Ok(None);
    }
    let mut tracked = None;
    db.query(
        "SELECT tracked FROM __amphora_joins WHERE singleton = 1",
        [],
        |row| {
            tracked = Some(u64::try_from(row.get::<i64>(0).map_err(column)?)?);
            Ok(false)
        },
    )?;
    Ok(tracked)
}

/// Whether any member has attested: until one has, the floor has nothing to prove and its
/// keeper writes nothing.
pub(crate) fn any_attested(db: &Db) -> anyhow::Result<bool> {
    if !table(db, "__amphora_attestations")? {
        return Ok(false);
    }
    let mut any = false;
    db.query("SELECT 1 FROM __amphora_attestations LIMIT 1", [], |_| {
        any = true;
        Ok(false)
    })?;
    Ok(any)
}

/// The index after which every member was last asked to attest again, 0 for never.
pub(crate) fn requested(db: &Db) -> anyhow::Result<u64> {
    if !table(db, "__amphora_attest_requested")? {
        return Ok(0);
    }
    let mut at = 0;
    db.query(
        "SELECT at FROM __amphora_attest_requested WHERE singleton = 1",
        [],
        |row| {
            at = u64::try_from(row.get::<i64>(0).map_err(column)?)?;
            Ok(false)
        },
    )?;
    Ok(at)
}

/// The member's current join: its row once joins are tracked, else 0 (every member before).
pub(crate) fn joined(db: &Db, member: u64) -> anyhow::Result<u64> {
    if !table(db, "__amphora_members")? {
        return Ok(0);
    }
    let mut joined = 0;
    db.query(
        "SELECT joined FROM __amphora_members WHERE member = ?1",
        [integer(member)?],
        |row| {
            joined = u64::try_from(row.get::<i64>(0).map_err(column)?)?;
            Ok(false)
        },
    )?;
    Ok(joined)
}

/// The applied index this session reads at its base.
fn applied(db: &Db) -> anyhow::Result<u64> {
    let mut applied = None;
    db.query(
        "SELECT applied FROM __amphora_replica WHERE id = 1",
        [],
        |row| {
            applied = Some(row.get::<String>(0).map_err(column)?.parse::<u64>()?);
            Ok(false)
        },
    )?;
    applied.ok_or_else(|| anyhow::anyhow!("the replica has no applied marker"))
}

/// The member's own attestation, written in its session: under its current join, with the
/// applied index it saw.
pub(crate) fn attest(
    db: &mut Db,
    member: u64,
    application: &str,
    highest: u32,
) -> anyhow::Result<()> {
    db.exec(ATTESTATIONS)?;
    let joined = joined(db, member)?;
    let seen = applied(db)?;
    db.query(
        "INSERT INTO __amphora_attestations(member, joined, application, highest, seen) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(member, joined, application) DO UPDATE SET highest = excluded.highest, seen = excluded.seen",
        [
            integer(member)?,
            integer(joined)?,
            Value::from_text(application.to_string()),
            Value::from_i64(i64::from(highest)),
            integer(seen)?,
        ],
        |_| Ok(true),
    )?;
    Ok(())
}

/// The attestation of `application` that counts for `member` now: under its current join, and,
/// before joins are tracked, with `seen` at or above both `last_change` (the last configuration
/// change the prover applied, or more) and the last request to attest again.
pub(crate) fn current(
    db: &Db,
    member: u64,
    application: &str,
    last_change: u64,
) -> anyhow::Result<Option<Attested>> {
    if !table(db, "__amphora_attestations")? {
        return Ok(None);
    }
    let joined = joined(db, member)?;
    let since = if tracked(db)?.is_some() {
        0
    } else {
        last_change.max(requested(db)?)
    };
    let mut attested = None;
    db.query(
        "SELECT application, highest FROM __amphora_attestations WHERE member = ?1 AND joined = ?2 AND seen >= ?3 AND application = ?4 LIMIT 1",
        [
            integer(member)?,
            integer(joined)?,
            integer(since)?,
            Value::from_text(application.to_string()),
        ],
        |row| {
            attested = Some(Attested {
                application: row.get::<String>(0).map_err(column)?,
                highest: u32::try_from(row.get::<i64>(1).map_err(column)?)?,
            });
            Ok(false)
        },
    )?;
    Ok(attested)
}

/// The configuration this session's base applied, as the machine's marker records it.
pub(crate) fn configuration(db: &Db) -> anyhow::Result<ConfState> {
    let mut configuration = None;
    db.query(
        "SELECT configuration FROM __amphora_replica WHERE id = 1",
        [],
        |row| {
            let bytes = row
                .get_value(0)
                .to_blob()
                .ok_or_else(|| anyhow::anyhow!("membership is not a blob"))?;
            configuration = Some(ConfState::decode(bytes)?);
            Ok(false)
        },
    )?;
    configuration.ok_or_else(|| anyhow::anyhow!("the replica has no applied marker"))
}

/// The members of the configuration this session's base applied whose attestation, under their
/// current join, is not of `application` at `version` or above: a raise to `version` is proven
/// only when there are none (replication.md, "Raising the floor"). Joins must be tracked.
pub(crate) fn short_of(db: &Db, application: &str, version: u32) -> anyhow::Result<Vec<u64>> {
    let mut short = Vec::new();
    for member in members(&configuration(db)?) {
        let proven = current(db, member, application, 0)?
            .is_some_and(|attested| attested.highest >= version);
        if !proven {
            short.push(member);
        }
    }
    Ok(short)
}

/// Marker J, written in the leader's session only when every member of the configuration at its
/// base has an attestation of `application` that counts at `last_change`. `seen_configuration` is
/// the configuration the leader read `last_change` with: if the base applied another since, the
/// session writes nothing. Answers whether it was written; the stale-base rule makes it hold at
/// apply. The members present now get join rows at 0, so their attestations stay valid.
pub(crate) fn track_joins(
    db: &mut Db,
    seen_configuration: &ConfState,
    application: &str,
    last_change: u64,
) -> anyhow::Result<bool> {
    if tracked(db)?.is_some() {
        return Ok(false);
    }
    let configuration = configuration(db)?;
    if configuration != *seen_configuration {
        return Ok(false);
    }
    for member in members(&configuration) {
        if current(db, member, application, last_change)?.is_none() {
            return Ok(false);
        }
    }
    let at = applied(db)?;
    db.exec(MEMBERS)?;
    for member in members(&configuration) {
        db.query(
            "INSERT INTO __amphora_members(member, joined) VALUES (?1, 0)",
            [integer(member)?],
            |_| Ok(true),
        )?;
    }
    db.exec(JOINS)?;
    db.query(
        "INSERT INTO __amphora_joins(singleton, tracked) VALUES (1, ?1)",
        [integer(at)?],
        |_| Ok(true),
    )?;
    Ok(true)
}

/// Asks every member to attest again after `at`: what the leader writes when its own knowledge
/// of the last change (after a restart, the applied point) is above the members' attestations.
pub(crate) fn request(db: &mut Db, at: u64) -> anyhow::Result<()> {
    db.exec(REQUESTED)?;
    db.query(
        "INSERT INTO __amphora_attest_requested(singleton, at) VALUES (1, ?1) ON CONFLICT(singleton) DO UPDATE SET at = MAX(at, excluded.at)",
        [integer(at)?],
        |_| Ok(true),
    )?;
    Ok(())
}

/// The join rows a configuration change makes, once joins are tracked: a member `after` names
/// that has no row joins at `at`; a row for a member `after` leaves out goes; a member with a row
/// that `after` names (promoted, demoted, or neither) keeps it. The rows, not this member's own
/// configuration before the change, say who was a member: a member added opens with the
/// configuration it is to have, so its own "before" already names it. Runs in `apply_change`'s
/// own transaction, so it is applied with the change or not at all.
pub(crate) fn apply_joins(db: &mut Db, at: u64, after: &ConfState) -> anyhow::Result<()> {
    if tracked(db)?.is_none() {
        return Ok(());
    }
    let after = members(after);
    let mut held = Vec::new();
    db.query("SELECT member FROM __amphora_members", [], |row| {
        held.push(u64::try_from(row.get::<i64>(0).map_err(column)?)?);
        Ok(true)
    })?;
    for member in &after {
        if !held.contains(member) {
            db.query(
                "INSERT INTO __amphora_members(member, joined) VALUES (?1, ?2)",
                [integer(*member)?, integer(at)?],
                |_| Ok(true),
            )?;
        }
    }
    for member in &held {
        if after.binary_search(member).is_err() {
            db.query(
                "DELETE FROM __amphora_members WHERE member = ?1",
                [integer(*member)?],
                |_| Ok(true),
            )?;
        }
    }
    Ok(())
}
