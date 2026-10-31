//! The existing indexer's durable work, one finite mutation at a time. A cursor and its
//! postings commit together; a timeout never means that the mutation did not happen.

use crate::sql::{Connection, OptionalExtension as _, params};

use super::{MAX_INDEXED, SEGMENT_LINES};

pub(super) const GRAMS_PER_BATCH: usize = 128;
pub(super) const BLOCK_DOCS: i64 = 64;

/// Job rowids are chronology only. The stable leg plus attempt and captured finish tuple
/// owns the content; a retry or an integer-rowid reuse cannot publish an old preparation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Source {
    attempt: i64,
    finished: i64,
    order: i64,
}

fn source(c: &Connection, leg: &str) -> anyhow::Result<Option<Source>> {
    Ok(c.query_row(
        "SELECT j.attempt,j.finished_at,j.rowid FROM jobs j JOIN runs r ON r.id=j.run_id WHERE j.id=?1 AND j.finished_at IS NOT NULL
        AND r.provider='kiln' AND j.status IN ('succeeded','failed','cancelled','skipped','timed_out')",
        [leg],
        |r| {
            Ok(Source {
                attempt: r.get(0)?,
                finished: r.get(1)?,
                order: r.get(2)?,
            })
        },
    )
    .optional()?)
}

/// Called inside the canonical terminal-job transaction. Duplicate exact-source delivery
/// keeps its version/cursor. A changed attempt first retires old documents in bounded pages.
pub(super) fn enqueue(c: &Connection, leg: &str, background: bool) -> anyhow::Result<()> {
    let Some(src) = source(c, leg)? else { return Ok(()) };
    let indexed: Option<(Option<i64>, Option<i64>, Option<i64>)> = c
        .query_row("SELECT source_attempt,source_finished,source_order FROM ci_log_indexed WHERE leg=?1", [leg], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    if indexed == Some((Some(src.attempt), Some(src.finished), Some(src.order))) {
        return Ok(());
    }
    let pending: Option<(Option<i64>, Option<i64>, Option<i64>)> = c
        .query_row("SELECT source_attempt,source_finished,source_order FROM ci_log_index_progress WHERE leg=?1", [leg], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    if pending == Some((Some(src.attempt), Some(src.finished), Some(src.order))) {
        if !background {
            c.execute("UPDATE ci_log_index_progress SET background=0 WHERE leg=?1", [leg])?;
        }
        return Ok(());
    }
    let old: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_docs WHERE leg=?1)", [leg], |r| r.get(0))?;
    c.execute("DELETE FROM ci_log_indexed WHERE leg=?1", [leg])?;
    c.execute(
        "INSERT INTO ci_log_index_progress (leg,version,source_attempt,source_finished,source_order,pruning,restart,background)
        VALUES (?1,?2,?3,?4,?5,?6,?6,?7)
        ON CONFLICT(leg) DO UPDATE SET version=excluded.version,source_attempt=excluded.source_attempt,source_finished=excluded.source_finished,
        source_order=excluded.source_order,pruning=excluded.pruning,restart=excluded.restart,background=excluded.background,
        step=0,segment=0,bytes=0,first_row=NULL,last_row=NULL,next_doc=0",
        params![leg, crate::util::new_id(), src.attempt, src.finished, src.order, old, background],
    )?;
    Ok(())
}

/// A fixed page before any completion anti-join. The cursor and enqueues commit together;
/// once caught up, this never walks historical jobs again to discover fresh completions.
pub(super) fn backfill(c: &Connection) -> anyhow::Result<bool> {
    let tx = crate::store::tx(c)?;
    let (after, after_order, end, end_order, done): (Option<i64>, Option<i64>, Option<i64>, Option<i64>, bool) =
        tx.query_row("SELECT after_finished,after_order,end_finished,end_order,done FROM ci_log_index_backfill WHERE id=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
    if done {
        return Ok(false);
    }
    let (end, end_order) = (
        end.ok_or_else(|| anyhow::anyhow!("log backfill has no high-water finish"))?,
        end_order.ok_or_else(|| anyhow::anyhow!("log backfill has no high-water row"))?,
    );
    let initial = "SELECT finished_at,rowid,id FROM jobs WHERE finished_at IS NOT NULL AND (finished_at,rowid)<=(?1,?2) ORDER BY finished_at,rowid LIMIT 16";
    let continued = "SELECT finished_at,rowid,id FROM jobs WHERE finished_at IS NOT NULL AND (finished_at,rowid)<=(?1,?2) AND (finished_at,rowid)>(?3,?4) ORDER BY finished_at,rowid LIMIT 16";
    let mut st = tx.prepare(if after.is_some() { continued } else { initial })?;
    let map = |r: &crate::sql::Row<'_>| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?));
    let rows: Vec<_> = if let Some(after) = after {
        st.query_map(params![end, end_order, after, after_order], map)?.collect::<crate::sql::Result<_>>()?
    } else {
        st.query_map(params![end, end_order], map)?.collect::<crate::sql::Result<_>>()?
    };
    drop(st);
    for (_, _, leg) in &rows {
        enqueue(&tx, leg, true)?;
    }
    if let Some((finished, order, _)) = rows.last() {
        tx.execute(
            "UPDATE ci_log_index_backfill SET after_finished=?1,after_order=?2,done=?3 WHERE id=1",
            params![finished, order, rows.len() < 16],
        )?;
    } else {
        tx.execute("UPDATE ci_log_index_backfill SET done=1 WHERE id=1", [])?;
    }
    tx.commit()?;
    Ok(!rows.is_empty())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Document {
    pub id: i64,
    pub version: String,
    pub leg: String,
    pub finished: i64,
    pub leg_order: i64,
    pub ordinal: i64,
    pub attempt: i64,
    pub after: Option<i64>,
}

pub(super) fn bitmap(bytes: &[u8]) -> anyhow::Result<u64> {
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| anyhow::anyhow!("invalid log-index bitmap"))?))
}

pub(super) fn doc_id(block: i64, bit: u32) -> anyhow::Result<i64> {
    block
        .checked_mul(BLOCK_DOCS)
        .and_then(|b| b.checked_add(i64::from(bit)))
        .ok_or_else(|| anyhow::anyhow!("log-index document identity overflow"))
}

fn position(doc: i64) -> (i64, u64) {
    (doc.div_euclid(BLOCK_DOCS), 1u64 << u32::try_from(doc.rem_euclid(BLOCK_DOCS)).unwrap_or(0))
}

/// Unknown historical content cannot acquire a current source by observation. Ineligible
/// (running, remote or deleted) sources retire only; a later valid finish is the sole restart.
fn retire_unknown(c: &Connection, leg: &str) -> anyhow::Result<()> {
    if source(c, leg)?.is_some() {
        return enqueue(c, leg, true);
    }
    c.execute(
        "INSERT INTO ci_log_index_progress (leg,version,pruning,restart,background) VALUES (?1,?2,1,0,1)
        ON CONFLICT(leg) DO UPDATE SET version=excluded.version,pruning=1,restart=0
        WHERE ci_log_index_progress.pruning=0 OR ci_log_index_progress.restart!=0",
        params![leg, crate::util::new_id()],
    )?;
    Ok(())
}

/// Load one canonical document only with its captured source. Legacy unknown-source
/// content must pass the same bounded prune/raw rebuild; it is never relabeled current.
pub(super) fn load(c: &Connection, id: i64) -> anyhow::Result<Option<(Document, String)>> {
    let tx = crate::store::tx(c)?;
    let identity: Option<(String, bool)> = tx
        .query_row(
            "SELECT leg,(index_version IS NOT NULL AND source_attempt IS NOT NULL AND order_finished IS NOT NULL AND order_leg IS NOT NULL AND order_doc IS NOT NULL) FROM ci_log_docs WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((leg, bound)) = identity else {
        tx.execute("DELETE FROM ci_log_search_pending WHERE doc=?1", [id])?;
        tx.commit()?;
        return Ok(None);
    };
    if !bound {
        retire_unknown(&tx, &leg)?;
        tx.commit()?;
        return Ok(None);
    }
    let bytes: i64 = tx.query_row("SELECT length(CAST(text AS BLOB)) FROM ci_log_docs WHERE id=?1", [id], |r| r.get(0))?;
    anyhow::ensure!(
        bytes >= 0 && usize::try_from(bytes)? <= MAX_INDEXED,
        "canonical log document {id} exceeds the 8 MiB indexing atom; left unindexed"
    );
    let row: Option<(String, Document)> = tx
        .query_row(
            "SELECT d.text,d.index_version,d.leg,d.order_finished,d.order_leg,d.order_doc,d.source_attempt,p.after_gram
        FROM ci_log_docs d JOIN ci_log_search_pending p ON p.doc=d.id JOIN jobs j ON j.id=d.leg JOIN runs r ON r.id=j.run_id
        WHERE d.id=?1 AND r.provider='kiln' AND j.status IN ('succeeded','failed','cancelled','skipped','timed_out')
          AND d.source_attempt=j.attempt AND d.order_finished=j.finished_at AND d.order_leg=j.rowid
          AND NOT EXISTS(SELECT 1 FROM ci_log_index_progress x WHERE x.leg=d.leg AND x.pruning=1)
          AND NOT EXISTS(SELECT 1 FROM ci_log_search_ready r WHERE r.doc=d.id AND r.version=d.index_version)",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    Document {
                        id,
                        version: r.get(1)?,
                        leg: r.get(2)?,
                        finished: r.get(3)?,
                        leg_order: r.get(4)?,
                        ordinal: r.get(5)?,
                        attempt: r.get(6)?,
                        after: r.get(7)?,
                    },
                ))
            },
        )
        .optional()?;
    let Some((body, doc)) = row else { return Ok(None) };
    anyhow::ensure!(body.len() <= MAX_INDEXED, "canonical log document {id} exceeds the 8 MiB indexing atom; left unindexed");
    tx.execute(
        "INSERT OR IGNORE INTO ci_log_doc_order (leg,ordinal,doc,version) VALUES (?1,?2,?3,?4)",
        params![doc.leg, doc.ordinal, doc.id, doc.version],
    )?;
    tx.execute("UPDATE ci_log_search_pending SET bytes=?2 WHERE doc=?1 AND bytes IS NULL", params![id, bytes])?;
    tx.commit()?;
    Ok(Some((doc, body)))
}

fn current(c: &Connection, d: &Document) -> anyhow::Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM ci_log_docs d JOIN ci_log_search_pending p ON p.doc=d.id JOIN jobs j ON j.id=d.leg JOIN runs r ON r.id=j.run_id
         WHERE d.id=?1 AND d.index_version=?2 AND d.leg=?3 AND d.order_finished=?4 AND d.order_leg=?5 AND d.order_doc=?6
           AND p.after_gram IS ?7 AND d.source_attempt=?8 AND j.attempt=d.source_attempt AND j.finished_at=d.order_finished AND j.rowid=d.order_leg
           AND r.provider='kiln' AND j.status IN ('succeeded','failed','cancelled','skipped','timed_out')
           AND NOT EXISTS (SELECT 1 FROM ci_log_index_progress x WHERE x.leg=d.leg AND x.pruning=1))",
        params![d.id, d.version, d.leg, d.finished, d.leg_order, d.ordinal, d.after, d.attempt],
        |r| r.get(0),
    )?)
}

pub(super) fn refresh(c: &Connection, d: &Document) -> anyhow::Result<Option<Document>> {
    let after: Option<Option<i64>> = c
        .query_row(
            "SELECT p.after_gram FROM ci_log_search_pending p JOIN ci_log_docs s ON s.id=p.doc JOIN jobs j ON j.id=s.leg JOIN runs r ON r.id=j.run_id
        WHERE s.id=?1 AND s.index_version=?2 AND s.leg=?3 AND s.order_finished=?4 AND s.order_leg=?5 AND s.order_doc=?6 AND r.provider='kiln' AND s.source_attempt=?7 AND j.attempt=s.source_attempt AND j.finished_at=s.order_finished AND j.rowid=s.order_leg
          AND j.status IN ('succeeded','failed','cancelled','skipped','timed_out')
          AND NOT EXISTS(SELECT 1 FROM ci_log_index_progress x WHERE x.leg=s.leg AND x.pruning=1)",
            params![d.id, d.version, d.leg, d.finished, d.leg_order, d.ordinal, d.attempt],
            |r| r.get(0),
        )
        .optional()?;
    Ok(after.map(|after| Document { after, ..d.clone() }))
}

/// At most 128 unique grams, one reverse page, and the exact progress/publication receipt.
/// The caller computes the grams outside Store::call. A stale page is a no-op, not a retry
/// from an assumed rollback. The product's final page/byte envelope remains authoritative.
pub(super) fn post(c: &Connection, d: &Document, grams: &[i64], done: bool) -> anyhow::Result<bool> {
    anyhow::ensure!(
        grams.len() <= GRAMS_PER_BATCH && grams.windows(2).all(|p| p.first() < p.get(1)) && grams.first().is_none_or(|g| d.after.is_none_or(|a| *g > a)),
        "invalid log-index posting page"
    );
    anyhow::ensure!(done || !grams.is_empty(), "empty nonterminal log-index page");
    let tx = crate::store::tx(c)?;
    if !current(&tx, d)? {
        return Ok(false);
    }
    let (block, mask) = position(d.ordinal);
    let mut packed = Vec::with_capacity(grams.len() * 8);
    for gram in grams {
        let old: Option<Vec<u8>> = tx
            .query_row(
                "SELECT bits FROM ci_log_grams WHERE gram=?1 AND finished=?2 AND leg_order=?3 AND leg=?4 AND block=?5",
                params![gram, d.finished, d.leg_order, d.leg, block],
                |r| r.get(0),
            )
            .optional()?;
        let bits = old.as_deref().map(bitmap).transpose()?.unwrap_or(0);
        if bits & mask == 0 {
            tx.execute(
                "INSERT INTO ci_log_grams (gram,finished,leg_order,leg,block,bits) VALUES (?1,?2,?3,?4,?5,?6)
                ON CONFLICT(gram,finished,leg_order,leg,block) DO UPDATE SET bits=excluded.bits",
                params![gram, d.finished, d.leg_order, d.leg, block, (bits | mask).to_le_bytes().as_slice()],
            )?;
            tx.execute("INSERT INTO ci_log_terms (gram,documents) VALUES (?1,1) ON CONFLICT(gram) DO UPDATE SET documents=documents+1", [gram])?;
        }
        packed.extend_from_slice(&gram.to_le_bytes());
    }
    if let Some(last) = grams.last() {
        tx.execute(
            "INSERT INTO ci_log_doc_gram_pages (doc,version,part,grams) VALUES (?1,?2,?3,?4)",
            params![d.id, d.version, last, packed],
        )?;
        tx.execute("UPDATE ci_log_search_pending SET after_gram=?2 WHERE doc=?1", params![d.id, last])?;
    }
    if done {
        tx.execute("INSERT INTO ci_log_search_ready (doc,version) VALUES (?1,?2)", params![d.id, d.version])?;
        tx.execute("DELETE FROM ci_log_search_pending WHERE doc=?1", [d.id])?;
    }
    tx.commit()?;
    Ok(true)
}

#[derive(Clone, Debug)]
pub(super) struct LegPlan {
    pub leg: String,
    pub version: String,
    pub step: u32,
    pub segment: usize,
    pub actual_step: u32,
    pub finished: i64,
    pub leg_order: i64,
    pub attempt: i64,
}
pub(super) enum Work {
    Post(i64),
    Read(LegPlan),
    Complete,
    Idle,
}

pub(super) fn leg(c: &Connection, leg: &str) -> anyhow::Result<Work> {
    let tx = crate::store::tx(c)?;
    let Some(src) = source(&tx, leg)? else { return Ok(Work::Idle) };
    let progress: Option<(String, u32, usize, bool, Option<i64>, Option<i64>, Option<i64>)> = tx
        .query_row(
            "SELECT version,step,segment,pruning,source_attempt,source_finished,source_order FROM ci_log_index_progress WHERE leg=?1",
            [leg],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .optional()?;
    let Some((version, step, segment, pruning, attempt, finished, order)) = progress else {
        return Ok(Work::Idle);
    };
    if pruning {
        return Ok(Work::Idle);
    }
    if (attempt, finished, order) != (Some(src.attempt), Some(src.finished), Some(src.order)) {
        enqueue(&tx, leg, false)?;
        tx.commit()?;
        return Ok(Work::Idle);
    }
    let (finished, leg_order) = (src.finished, src.order);
    let pending: Option<i64> = tx
        .query_row(
            "SELECT d.id FROM ci_log_docs d JOIN ci_log_search_pending p ON p.doc=d.id WHERE d.leg=?1 ORDER BY d.id LIMIT 1",
            [leg],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(doc) = pending {
        tx.commit()?;
        return Ok(Work::Post(doc));
    }
    let next: Option<u32> = tx
        .query_row("SELECT idx FROM steps WHERE job_id=?1 AND idx>=?2 ORDER BY idx LIMIT 1", params![leg, step], |r| r.get(0))
        .optional()?;
    let Some(actual_step) = next else {
        tx.execute(
            "INSERT OR REPLACE INTO ci_log_indexed (leg,at,bytes,first_row,last_row,source_attempt,source_finished,source_order) SELECT leg,?2,bytes,first_row,last_row,source_attempt,source_finished,source_order FROM ci_log_index_progress WHERE leg=?1",
            params![leg, crate::util::now_unix()],
        )?;
        tx.execute("DELETE FROM ci_log_index_progress WHERE leg=?1", [leg])?;
        tx.commit()?;
        return Ok(Work::Complete);
    };
    tx.commit()?;
    Ok(Work::Read(LegPlan {
        leg: leg.into(),
        version,
        step,
        segment: if step == actual_step { segment } else { 0 },
        actual_step,
        finished,
        leg_order,
        attempt: src.attempt,
    }))
}

/// One canonical payload (at most the existing 8 MiB normalized step prefix) plus its
/// cursor. Preparation and line splitting happen outside the writer.
pub(super) fn stage(c: &Connection, p: &LegPlan, body: &str, last_segment: bool, raw_bytes: u64) -> anyhow::Result<bool> {
    anyhow::ensure!(body.len() <= MAX_INDEXED, "log-index payload exceeds 8 MiB");
    let tx = crate::store::tx(c)?;
    let valid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ci_log_index_progress x JOIN jobs j ON j.id=x.leg
        WHERE x.leg=?1 AND x.version=?2 AND x.step=?3 AND x.segment=?4 AND x.pruning=0 AND j.finished_at=?5 AND j.rowid=?6 AND j.attempt=?7
          AND x.source_attempt=j.attempt AND x.source_finished=j.finished_at AND x.source_order=j.rowid
          AND j.status IN ('succeeded','failed','cancelled','skipped','timed_out'))",
        params![p.leg, p.version, p.step, p.segment, p.finished, p.leg_order, p.attempt],
        |r| r.get(0),
    )?;
    if !valid {
        return Ok(false);
    }
    let doc = if body.trim().is_empty() {
        None
    } else {
        let line = p
            .segment
            .checked_mul(SEGMENT_LINES)
            .and_then(|n| n.checked_add(1))
            .and_then(|n| i64::try_from(n).ok())
            .ok_or_else(|| anyhow::anyhow!("log-index line overflow"))?;
        let ordinal: i64 = tx.query_row("SELECT next_doc FROM ci_log_index_progress WHERE leg=?1", [&p.leg], |r| r.get(0))?;
        anyhow::ensure!(ordinal.checked_add(1).is_some(), "log-index document ordinal exhausted");
        tx.execute(
            "INSERT INTO ci_log_docs (text,leg,step,line,index_version,order_finished,order_leg,order_doc,source_attempt) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![body, p.leg, p.actual_step, line, crate::util::new_id(), p.finished, p.leg_order, ordinal, p.attempt],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO ci_log_doc_order (leg,ordinal,doc,version) SELECT leg,order_doc,id,index_version FROM ci_log_docs WHERE id=?1",
            [id],
        )?;
        tx.execute("INSERT INTO ci_log_search_pending (doc,bytes) VALUES (?1,?2)", params![id, i64::try_from(body.len())?])?;
        Some(id)
    };
    let (step, segment, bytes) = if last_segment {
        (p.actual_step.checked_add(1).ok_or_else(|| anyhow::anyhow!("log-index step overflow"))?, 0, i64::try_from(raw_bytes)?)
    } else {
        (p.actual_step, p.segment.checked_add(1).ok_or_else(|| anyhow::anyhow!("log-index segment overflow"))?, 0)
    };
    tx.execute(
        "UPDATE ci_log_index_progress SET step=?2,segment=?3,bytes=bytes+?4,first_row=COALESCE(first_row,?5),last_row=COALESCE(?5,last_row),next_doc=next_doc+CASE WHEN ?5 IS NULL THEN 0 ELSE 1 END WHERE leg=?1",
        params![p.leg, step, segment, bytes, doc],
    )?;
    tx.commit()?;
    Ok(true)
}

pub(super) fn request_prune(c: &Connection, leg: &str) -> anyhow::Result<()> {
    // Only the successful deletion transaction (or its idempotent replay after absence)
    // supplies this intent. A newer same-ID job is never an authority to delete its index.
    c.execute(
        "INSERT INTO ci_log_index_progress (leg,version,pruning,restart) SELECT ?1,?2,1,0 WHERE NOT EXISTS(SELECT 1 FROM jobs WHERE id=?1)
        ON CONFLICT(leg) DO UPDATE SET version=excluded.version,pruning=1,restart=0",
        params![leg, crate::util::new_id()],
    )?;
    Ok(())
}

/// One reverse page or one document/leg receipt. The request's version fences a stale
/// prune turn; document versions fence integer-ID reuse and late posting plans.
pub(super) fn prune(c: &Connection, leg: &str, version: &str) -> anyhow::Result<bool> {
    let tx = crate::store::tx(c)?;
    let current: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ci_log_index_progress WHERE leg=?1 AND version=?2 AND pruning=1)",
        params![leg, version],
        |r| r.get(0),
    )?;
    if !current {
        return Ok(false);
    }
    let row: Option<(i64, Option<String>, Option<i64>, Option<i64>, Option<i64>)> = tx
        .query_row(
            "SELECT id,index_version,order_finished,order_leg,order_doc FROM ci_log_docs WHERE leg=?1 ORDER BY id LIMIT 1",
            [leg],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((doc, doc_version, finished, leg_order, ordinal)) = row else {
        tx.execute("DELETE FROM ci_log_indexed WHERE leg=?1", [leg])?;
        let restart: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ci_log_index_progress x JOIN jobs j ON j.id=x.leg
            WHERE x.leg=?1 AND x.version=?2 AND x.restart=1 AND x.source_attempt=j.attempt AND x.source_finished=j.finished_at AND x.source_order=j.rowid
              AND j.status IN ('succeeded','failed','cancelled','skipped','timed_out'))",
            params![leg, version],
            |r| r.get(0),
        )?;
        if restart {
            tx.execute(
                "UPDATE ci_log_index_progress SET version=?3,pruning=0,restart=0,step=0,segment=0,bytes=0,first_row=NULL,last_row=NULL,next_doc=0 WHERE leg=?1 AND version=?2",
                params![leg, version, crate::util::new_id()],
            )?;
        } else {
            tx.execute("DELETE FROM ci_log_index_progress WHERE leg=?1 AND version=?2", params![leg, version])?;
        }
        tx.commit()?;
        return Ok(true);
    };
    let page: Option<(i64, Vec<u8>)> = if let Some(v) = &doc_version {
        tx.query_row("SELECT part,grams FROM ci_log_doc_gram_pages WHERE doc=?1 AND version=?2 ORDER BY part LIMIT 1", params![doc, v], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?
    } else {
        None
    };
    if let Some((part, grams)) = page {
        anyhow::ensure!(grams.len() <= GRAMS_PER_BATCH * 8 && grams.len().is_multiple_of(8), "invalid log-index reverse page");
        let finished = finished.ok_or_else(|| anyhow::anyhow!("reverse page has no finished order"))?;
        let leg_order = leg_order.ok_or_else(|| anyhow::anyhow!("reverse page has no leg order"))?;
        let ordinal = ordinal.ok_or_else(|| anyhow::anyhow!("reverse page has no document ordinal"))?;
        let (block, mask) = position(ordinal);
        for bytes in grams.as_chunks::<8>().0 {
            let gram = i64::from_le_bytes(*bytes);
            let old: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT bits FROM ci_log_grams WHERE gram=?1 AND finished=?2 AND leg_order=?3 AND leg=?4 AND block=?5",
                    params![gram, finished, leg_order, leg, block],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(bits) = old.as_deref().map(bitmap).transpose()? else { continue };
            if bits & mask == 0 {
                continue;
            }
            if bits & !mask == 0 {
                tx.execute(
                    "DELETE FROM ci_log_grams WHERE gram=?1 AND finished=?2 AND leg_order=?3 AND leg=?4 AND block=?5",
                    params![gram, finished, leg_order, leg, block],
                )?;
            } else {
                tx.execute(
                    "UPDATE ci_log_grams SET bits=?6 WHERE gram=?1 AND finished=?2 AND leg_order=?3 AND leg=?4 AND block=?5",
                    params![gram, finished, leg_order, leg, block, (bits & !mask).to_le_bytes().as_slice()],
                )?;
            }
            tx.execute("UPDATE ci_log_terms SET documents=documents-1 WHERE gram=?1", [gram])?;
            tx.execute("DELETE FROM ci_log_terms WHERE gram=?1 AND documents<=0", [gram])?;
        }
        tx.execute("DELETE FROM ci_log_doc_gram_pages WHERE doc=?1 AND version=?2 AND part=?3", params![doc, doc_version, part])?;
    } else {
        tx.execute(
            "DELETE FROM ci_log_doc_order WHERE leg=?1 AND ordinal IS ?2 AND doc=?3 AND version IS ?4",
            params![leg, ordinal, doc, doc_version],
        )?;
        tx.execute("DELETE FROM ci_log_search_ready WHERE doc=?1", [doc])?;
        tx.execute("DELETE FROM ci_log_search_pending WHERE doc=?1", [doc])?;
        tx.execute("DELETE FROM ci_log_docs WHERE id=?1 AND index_version IS ?2", params![doc, doc_version])?;
    }
    tx.commit()?;
    Ok(true)
}

/// The migration only renamed old row indexes. Retire a finite page, and enqueue a
/// finite page of old ready documents, without blocking current legs behind a rebuild.
pub(super) fn retire_legacy(c: &Connection) -> anyhow::Result<bool> {
    let tx = crate::store::tx(c)?;
    let mut more = false;
    for table in ["ci_log_grams_legacy", "ci_log_terms_legacy", "ci_log_search_ready_legacy"] {
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?;
        if !exists {
            continue;
        }
        let query = match table {
            "ci_log_grams_legacy" => "SELECT rowid FROM ci_log_grams_legacy ORDER BY rowid LIMIT 128",
            "ci_log_terms_legacy" => "SELECT rowid FROM ci_log_terms_legacy ORDER BY rowid LIMIT 128",
            _ => "SELECT doc FROM ci_log_search_ready_legacy ORDER BY doc LIMIT 8",
        };
        let ids: Vec<i64> = tx.prepare(query)?.query_map([], |r| r.get(0))?.collect::<crate::sql::Result<_>>()?;
        if ids.is_empty() {
            tx.execute_batch(&format!("DROP TABLE {table}"))?;
            continue;
        }
        more = true;
        for id in ids {
            if table == "ci_log_search_ready_legacy" {
                tx.execute("INSERT OR IGNORE INTO ci_log_search_pending (doc) SELECT id FROM ci_log_docs d WHERE id=?1 AND NOT EXISTS (SELECT 1 FROM ci_log_search_ready r WHERE r.doc=d.id AND r.version=d.index_version)",[id])?;
            }
            let delete = match table {
                "ci_log_grams_legacy" => "DELETE FROM ci_log_grams_legacy WHERE rowid=?1",
                "ci_log_terms_legacy" => "DELETE FROM ci_log_terms_legacy WHERE rowid=?1",
                _ => "DELETE FROM ci_log_search_ready_legacy WHERE doc=?1",
            };
            tx.execute(delete, [id])?;
        }
    }
    tx.commit()?;
    Ok(more)
}

/// Only test setup drives a document to completion synchronously; production always returns
/// to the same async indexer between finite Store calls.
#[cfg(test)]
pub(super) fn index_step(c: &Connection, leg: &str, step: u32, text: &str) -> anyhow::Result<Option<(i64, i64)>> {
    let mut first = None;
    let mut last = None;
    for (n, chunk) in crate::util::truncate(text, MAX_INDEXED).split('\n').collect::<Vec<_>>().chunks(SEGMENT_LINES).enumerate() {
        let body = chunk.join("\n");
        if body.trim().is_empty() {
            continue;
        }
        let version = crate::util::new_id();
        let ordinal: i64 = c.query_row("SELECT COALESCE(MAX(order_doc)+1,0) FROM ci_log_docs WHERE leg=?1", [leg], |r| r.get(0))?;
        let src = source(c, leg)?.ok_or_else(|| anyhow::anyhow!("fixture has no current terminal source"))?;
        c.execute(
            "INSERT INTO ci_log_docs (text,leg,step,line,index_version,order_doc,source_attempt,order_finished,order_leg) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![body, leg, step, i64::try_from(n * SEGMENT_LINES + 1)?, version, ordinal, src.attempt, src.finished, src.order],
        )?;
        let id = c.last_insert_rowid();
        c.execute("INSERT INTO ci_log_search_pending (doc) VALUES (?1)", [id])?;
        loop {
            let Some((d, body)) = load(c, id)? else { break };
            let all = kiln_ci::logsearch::trigrams(&body);
            let page: Vec<_> = all.iter().copied().filter(|g| d.after.is_none_or(|a| *g > a)).take(GRAMS_PER_BATCH + 1).collect();
            post(c, &d, page.get(..page.len().min(GRAMS_PER_BATCH)).unwrap_or_default(), page.len() <= GRAMS_PER_BATCH)?;
        }
        first.get_or_insert(id);
        last = Some(id);
    }
    Ok(first.zip(last))
}

/// Streaming form of plain(): only the normalized prefix and a rewritten line are retained.
/// ANSI and UTF-8 state cross read boundaries. A trailing CR keeps the last displayed line;
/// a later non-CR character replaces it. No giant raw line or full raw log is allocated.
pub(crate) fn plain_reader(mut input: impl std::io::Read, limit: usize) -> std::io::Result<String> {
    struct Plain {
        out: String,
        line: String,
        limit: usize,
        full: bool,
        cr: bool,
        esc: bool,
        csi: bool,
        done: bool,
    }
    impl Plain {
        fn put(&mut self, c: char) {
            if self.done {
                return;
            }
            if self.csi {
                if ('@'..='~').contains(&c) {
                    self.csi = false
                }
                return;
            }
            if self.esc {
                self.esc = false;
                if c == '[' {
                    self.csi = true;
                    return;
                }
            }
            if c == '\x1b' {
                self.esc = true;
                return;
            }
            match c {
                '\r' => self.cr = true,
                '\n' => {
                    self.out.push_str(&self.line);
                    self.line.clear();
                    self.cr = false;
                    if self.full || self.out.len() >= self.limit {
                        self.done = true;
                        return;
                    }
                    self.out.push('\n');
                    if self.out.len() >= self.limit {
                        self.done = true
                    }
                }
                _ => {
                    if self.cr {
                        self.line.clear();
                        self.full = false;
                        self.cr = false
                    }
                    if !self.full {
                        if self.out.len() + self.line.len() + c.len_utf8() <= self.limit {
                            self.line.push(c)
                        } else {
                            self.full = true
                        }
                    }
                }
            }
        }
    }
    let mut plain = Plain {
        out: String::new(),
        line: String::new(),
        limit,
        full: false,
        cr: false,
        esc: false,
        csi: false,
        done: limit == 0,
    };
    let mut carry = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    while !plain.done {
        let n = input.read(&mut chunk)?;
        if n == 0 {
            if !carry.is_empty() {
                plain.put('\u{fffd}')
            }
            break;
        }
        carry.extend_from_slice(chunk.get(..n).unwrap_or_default());
        let mut at = 0;
        loop {
            let rest = carry.get(at..).unwrap_or_default();
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    for c in text.chars() {
                        plain.put(c)
                    }
                    at = carry.len();
                    break;
                }
                Err(e) => {
                    let good = e.valid_up_to();
                    if let Ok(text) = std::str::from_utf8(rest.get(..good).unwrap_or_default()) {
                        for c in text.chars() {
                            plain.put(c)
                        }
                    }
                    at += good;
                    match e.error_len() {
                        Some(bad) => {
                            plain.put('\u{fffd}');
                            at += bad
                        }
                        None => break,
                    }
                }
            }
            if plain.done {
                break;
            }
        }
        carry.drain(..at);
    }
    if !plain.done {
        plain.out.push_str(&plain.line)
    }
    Ok(plain.out)
}
