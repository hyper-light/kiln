//! Bounded Records GC through the existing writer; no background owner or caller-owned cursor.
use super::*;
use amphora_engine::LimboError;

// Worst-case postcard envelope (including the optional bootstrap declaration) and 4096-byte
// page entry. See docs/design/records-compaction.md: 8178 pages fit REQUEST_MAX.
const ENVELOPE: usize = 81;
const PAGE_ENTRY: usize = 4103; // 5-byte ID + 2-byte length + 4096-byte page.
const MIN_ROOM: usize = 4184; // Envelope and one page.

pub(crate) fn page_limit(room: usize) -> Result<usize, Refusal> {
    let room = room.min(api::REQUEST_MAX);
    let pages = room
        .saturating_sub(ENVELOPE)
        .checked_div(PAGE_ENTRY)
        .ok_or_else(|| api::invalid("compaction page envelope", "zero page-entry size"))?;
    if pages == 0 {
        return Err(Refusal::TooLarge {
            what: "compaction page envelope".into(),
            size: MIN_ROOM,
            limit: room,
        });
    }
    Ok(pages)
}

/// A row count bounds work, not bytes. Only the pager's exact admission error permits halving;
/// every attempt must have rolled back and reset before this closure returns that error.
pub(crate) fn retry<R>(mut attempt: impl FnMut(usize) -> anyhow::Result<R>) -> Result<R, Refusal> {
    let mut visits = api::TX_MAX;
    loop {
        match attempt(visits) {
            Ok(value) => return Ok(value),
            Err(error) => match error.downcast_ref::<LimboError>() {
                Some(LimboError::TransactionPageLimit { limit, attempted }) if visits == 1 => {
                    return Err(Refusal::TooLarge {
                        what: "compaction atom dirty pages".into(),
                        size: *attempted,
                        limit: *limit,
                    });
                }
                Some(LimboError::TransactionPageLimit { .. }) => {
                    visits = visits.checked_div(2).unwrap_or(1).max(1)
                }
                _ => return Err(refused(&error)),
            },
        }
    }
}

pub(super) fn local(
    db: &mut Db,
    namespace: &str,
    before: Rev,
    until: Instant,
    restoring: bool,
) -> Result<api::CompactionProgress, Refusal> {
    let pages = page_limit(api::REQUEST_MAX)?;
    retry(|visits| {
        let result = db
            .compaction_attempt(pages, false, |db| {
                let _deadline = db.deadline(until);
                ready(db, namespace, until, restoring)?;
                step(db, namespace, before, visits)
            })
            .map(|(progress, _)| progress);
        deadline_result(result, until)
    })
}

pub(crate) fn deadline_result<R>(result: anyhow::Result<R>, until: Instant) -> anyhow::Result<R> {
    // Cleanup fencing takes priority over deadline reporting and may never permit a retry.
    match result {
        Err(error)
            if Instant::now() >= until
                && !matches!(error.downcast_ref::<Refusal>(), Some(Refusal::Fenced(_))) =>
        {
            Err(Refusal::Timeout.into())
        }
        other => other,
    }
}

pub(crate) fn ready(
    db: &Db,
    namespace: &str,
    until: Instant,
    restoring: bool,
) -> anyhow::Result<()> {
    if Instant::now() >= until {
        return Err(Refusal::Timeout.into());
    }
    if !restoring && restore::incomplete(db)? {
        return Err(api::invalid("database", "restore is incomplete").into());
    }
    if import::incomplete(db, namespace)? {
        return Err(api::invalid("namespace", "import is incomplete").into());
    }
    Ok(())
}

type Cursor = (String, Vec<u8>, Rev);

fn next_query(
    db: &Db,
    query: &'static str,
    params: impl IntoIterator<Item = EValue>,
) -> anyhow::Result<Option<(Cursor, bool)>> {
    let mut next = None;
    db.query(query, params, |row| {
        next = Some((
            (
                row.get::<String>(0).map_err(unavailable)?,
                bytes(row, 1)?.ok_or_else(|| anyhow::anyhow!("null compaction key"))?,
                rev(row, 2)?,
            ),
            row.get::<i64>(3).map_err(unavailable)? != 0,
        ));
        Ok(false)
    })?;
    Ok(next)
}

fn next(
    db: &Db,
    ns: &str,
    target: Rev,
    cursor: Option<&Cursor>,
) -> anyhow::Result<Option<(Cursor, bool)>> {
    // Separate prefix/range seeks avoid an OR cursor predicate that could repeatedly scan the
    // namespace's prefix. No candidate set is materialized ahead of the working transaction.
    if let Some((family, key, revision)) = cursor {
        if let Some(next) = next_query(
            db,
            "SELECT family,key,rev,value IS NULL FROM __amphora_versions WHERE namespace=?1 AND family=?2 AND key=?3 AND rev>?4 AND rev<=?5 ORDER BY rev LIMIT 1",
            [
                text(ns),
                text(family),
                blob(key)?,
                integer(*revision)?,
                integer(target)?,
            ],
        )? {
            return Ok(Some(next));
        }
        if let Some(next) = next_query(
            db,
            "SELECT family,key,rev,value IS NULL FROM __amphora_versions WHERE namespace=?1 AND family=?2 AND key>?3 AND rev<=?4 ORDER BY key,rev LIMIT 1",
            [text(ns), text(family), blob(key)?, integer(target)?],
        )? {
            return Ok(Some(next));
        }
        next_query(
            db,
            "SELECT family,key,rev,value IS NULL FROM __amphora_versions WHERE namespace=?1 AND family>?2 AND rev<=?3 ORDER BY family,key,rev LIMIT 1",
            [text(ns), text(family), integer(target)?],
        )
    } else {
        next_query(
            db,
            "SELECT family,key,rev,value IS NULL FROM __amphora_versions WHERE namespace=?1 AND rev<=?2 ORDER BY family,key,rev LIMIT 1",
            [text(ns), integer(target)?],
        )
    }
}

/// Called only inside one page-fenced transaction. Each visit takes the next version from the
/// working tree, so a newly obsolete tombstone is examined before the cursor passes its key.
pub(crate) fn step(
    db: &Db,
    ns: &str,
    before: Rev,
    visits: usize,
) -> anyhow::Result<api::CompactionProgress> {
    // Lazy DDL is part of the same prepared/committed command, never startup-only replica state.
    db.exec("CREATE TABLE IF NOT EXISTS __amphora_compaction(namespace TEXT PRIMARY KEY, target INTEGER, completed INTEGER NOT NULL, family TEXT, key BLOB, rev INTEGER)")?;
    db.query("INSERT OR IGNORE INTO __amphora_namespaces(name,head,horizon,feed,live) VALUES (?1,0,0,0,0)",[text(ns)], |_| Ok(true))?;
    db.query(
        "INSERT OR IGNORE INTO __amphora_compaction(namespace,completed) VALUES (?1,0)",
        [text(ns)],
        |_| Ok(true),
    )?;
    let mut state = (0, 0, 0);
    db.query(
        "SELECT head,horizon,feed FROM __amphora_namespaces WHERE name=?1",
        [text(ns)],
        |row| {
            state = (rev(row, 0)?, rev(row, 1)?, rev(row, 2)?);
            Ok(false)
        },
    )?;
    let (head, old_horizon, old_feed) = state;
    let horizon = before.min(head).max(old_horizon);
    let feed = old_feed.max(horizon.saturating_sub(1));
    if horizon != old_horizon || feed != old_feed {
        db.query(
            "UPDATE __amphora_namespaces SET horizon=?2,feed=?3 WHERE name=?1",
            [text(ns), integer(horizon)?, integer(feed)?],
            |_| Ok(true),
        )?;
    }
    let mut target = None;
    let mut completed = 0;
    let mut cursor = None;
    db.query(
        "SELECT target,completed,family,key,rev FROM __amphora_compaction WHERE namespace=?1",
        [text(ns)],
        |row| {
            target = match row.get_value(0) {
                EValue::Null => None,
                _ => Some(rev(row, 0)?),
            };
            completed = rev(row, 1)?;
            cursor = match row.get_value(2) {
                EValue::Null => None,
                _ => Some((
                    row.get::<String>(2).map_err(unavailable)?,
                    bytes(row, 3)?.ok_or_else(|| anyhow::anyhow!("null compaction cursor key"))?,
                    rev(row, 4)?,
                )),
            };
            Ok(false)
        },
    )?;
    if target.is_none() && completed < horizon {
        target = Some(horizon);
        cursor = None;
    }
    let mut visited = 0usize;
    let mut deleted = 0usize;
    if let Some(cutoff) = target {
        while visited < visits {
            let Some(((family, key, revision), tombstone)) = next(db, ns, cutoff, cursor.as_ref())?
            else {
                break;
            };
            let mut latest = 0;
            db.query("SELECT MAX(rev) FROM __amphora_versions WHERE namespace=?1 AND family=?2 AND key=?3 AND rev<=?4",[text(ns),text(&family),blob(&key)?,integer(cutoff)?], |row| { latest=rev(row,0)?; Ok(false) })?;
            let mut obsolete = revision < latest;
            if !obsolete && tombstone && revision < cutoff {
                let mut older = false;
                db.query("SELECT 1 FROM __amphora_versions WHERE namespace=?1 AND family=?2 AND key=?3 AND rev<?4 LIMIT 1",[text(ns),text(&family),blob(&key)?,integer(revision)?], |_| { older=true; Ok(false) })?;
                obsolete = !older;
            }
            if obsolete {
                db.query("DELETE FROM __amphora_versions WHERE namespace=?1 AND family=?2 AND key=?3 AND rev=?4",[text(ns),text(&family),blob(&key)?,integer(revision)?], |_| Ok(true))?;
                deleted = deleted
                    .checked_add(1)
                    .ok_or_else(|| api::invalid("compaction count", "deleted count overflow"))?;
            }
            cursor = Some((family, key, revision));
            visited = visited
                .checked_add(1)
                .ok_or_else(|| api::invalid("compaction count", "visited count overflow"))?;
        }
        if next(db, ns, cutoff, cursor.as_ref())?.is_none() {
            completed = cutoff;
            target = if completed < horizon {
                Some(horizon)
            } else {
                None
            };
            cursor = None;
        }
    }
    let (family, key, revision) = match cursor {
        Some((family, key, revision)) => (text(&family), blob(&key)?, integer(revision)?),
        None => (EValue::Null, EValue::Null, EValue::Null),
    };
    db.query("UPDATE __amphora_compaction SET target=?2,completed=?3,family=?4,key=?5,rev=?6 WHERE namespace=?1",[text(ns),match target {Some(value)=>integer(value)?,None=>EValue::Null},integer(completed)?,family,key,revision], |_| Ok(true))?;
    Ok(api::CompactionProgress {
        head,
        horizon,
        feed,
        completed,
        target,
        visited,
        deleted,
        more: completed < horizon,
    })
}
