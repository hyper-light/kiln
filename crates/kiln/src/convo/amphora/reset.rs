//! U06/U15: conservative, per-key reset checks during multi-source legacy adoption.
//! Resets are retained as evidence, never replayed as commands against another source. A
//! conflicting key refuses projection/completion so both legacy inputs remain available.

use super::*;
use amphora_api::sql::{OptionalExtension as _, params};

pub(super) struct LegacyKey<'a> {
    pub(super) source: &'a str,
    pub(super) table: &'a str,
    pub(super) session: &'a str,
    pub(super) item: &'a str,
    pub(super) n: u64,
    pub(super) at: Option<i64>,
    pub(super) epoch: u64,
}

fn reset_key(table: &str, session: &str, item: &str, n: u64) -> String {
    if table == "records" {
        format!("{session:?}/{item:?}/{n}")
    } else {
        format!("{session:?}/{item:?}")
    }
}

fn reset_collision(source: &str, table: &str, session: &str, item: &str, n: u64, reason: String) -> anyhow::Error {
    ImportCollision {
        source: source.to_owned(),
        table: table.to_owned(),
        key: reset_key(table, session, item, n),
        reason,
    }.into()
}

impl Session<'_, '_> {
    /// Check only a previously absent canonical event/record, after archiving its source row
    /// and before projecting it. Same-source current rows may have been written after a reset;
    /// foreign resets and live drops cannot safely be dismissed that way.
    pub(super) fn check_legacy_insert(&self, key: LegacyKey<'_>) -> anyhow::Result<()> {
        self.check_legacy_reset_key(key,true)
    }

    /// U19: an equal canonical row may predate source provenance (single-source backfill).
    /// Its newly revealed foreign origin must still respect archived reset evidence. A reset
    /// owner's current copy of the same key permits the duplicate; live drops do not override
    /// an already-present live row. Call after content equality, before metadata enrichment.
    pub(super) fn check_legacy_duplicate(&self, key: LegacyKey<'_>) -> anyhow::Result<()> {
        self.check_legacy_reset_key(key,false)
    }

    fn check_legacy_reset_key(&self, key: LegacyKey<'_>, absent: bool) -> anyhow::Result<()> {
        let what = match key.table {
            "events" => {
                anyhow::ensure!(key.at.is_some(), "legacy event has no timestamp for reset validation");
                "events"
            }
            "records" => "record",
            _ => anyhow::bail!("reset validation requires an event or record key"),
        };
        let n = i64::try_from(key.n)?;
        let epoch = i64::try_from(key.epoch)?;
        let reset = self.conn.query_row(
            "SELECT r.source,r.source_rowid FROM convo_legacy_resets r WHERE r.source<>?1 AND r.session=?2 AND r.what=?3 AND COALESCE(r.n,0)<=?4 AND r.epoch>=?5 AND (?3<>'record' OR r.record_source=?6) AND (?3<>'events' OR r.at>=?7) AND (?8 OR NOT EXISTS (SELECT 1 FROM convo_legacy_rows own WHERE own.source=r.source AND own.table_name=?9 AND own.session=?2 AND own.item=?6 AND (?9<>'records' OR own.number=?4))) LIMIT 1",
            params![key.source,key.session,what,n,epoch,key.item,key.at,absent,key.table],
            |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)),
        ).optional()?;
        if let Some((source, rowid)) = reset {
            let evidence = if absent { "an absent canonical key" } else { "a newly attributed canonical key absent from the reset owner's current rows" };
            return Err(reset_collision(key.source,key.table,key.session,key.item,key.n,format!("reset ambiguity: source {source:?} reset row {rowid} covers {evidence} at this epoch")));
        }
        if !absent { return Ok(()); }
        // Current drop rows have no source epoch. Their range is evidence that an absent key
        // was deliberately removed; an import must not resurrect it on a guessed authority.
        let dropped = self.conn.query_row(
            "SELECT rev FROM convo_drops WHERE session=?1 AND what=?2 AND COALESCE(n,0)<=?3 AND (?2<>'record' OR source=?4) AND (?2<>'events' OR at>=?5) LIMIT 1",
            params![key.session,what,n,key.item,key.at],
            |row| row.get::<_,i64>(0),
        ).optional()?;
        if let Some(rev) = dropped {
            return Err(reset_collision(key.source,key.table,key.session,key.item,key.n,format!("reset ambiguity: live drop revision {rev} covers an absent canonical key")));
        }
        Ok(())
    }

    /// Check the reverse import order in bounded pages before publishing this source.
    /// The caller commits this validation cursor alongside its source ledger. Own current
    /// keys remain valid: they may have been written after their own reset.
    pub(super) fn validate_legacy_resets(&self, source: &str, table: &str, after: i64) -> anyhow::Result<(i64,bool)> {
        let (query,own_query,reset_query) = match table {
            "events" => (
                "SELECT rowid,session,id,seq,at FROM convo_events WHERE rowid>?1 ORDER BY rowid LIMIT 128",
                "SELECT 1 FROM convo_legacy_rows WHERE source=?1 AND table_name='events' AND session=?2 AND item=?3 LIMIT 1",
                "SELECT r.source_rowid FROM convo_legacy_resets r WHERE r.source=?1 AND r.session=?2 AND r.what='events' AND COALESCE(r.n,0)<=?4 AND r.at>=?5 AND EXISTS (SELECT 1 FROM convo_legacy_rows foreign_row WHERE foreign_row.source<>r.source AND foreign_row.table_name='events' AND foreign_row.session=?2 AND foreign_row.item=?3 AND foreign_row.epoch<=r.epoch) LIMIT 1",
            ),
            "records" => (
                "SELECT rowid,session,source,n,NULL FROM convo_records WHERE rowid>?1 ORDER BY rowid LIMIT 128",
                "SELECT 1 FROM convo_legacy_rows WHERE source=?1 AND table_name='records' AND session=?2 AND item=?3 AND number=?4 LIMIT 1",
                "SELECT r.source_rowid FROM convo_legacy_resets r WHERE r.source=?1 AND r.session=?2 AND r.what='record' AND r.record_source=?3 AND COALESCE(r.n,0)<=?4 AND EXISTS (SELECT 1 FROM convo_legacy_rows foreign_row WHERE foreign_row.source<>r.source AND foreign_row.table_name='records' AND foreign_row.session=?2 AND foreign_row.item=?3 AND foreign_row.number=?4 AND foreign_row.epoch<=r.epoch) LIMIT 1",
            ),
            _ => anyhow::bail!("reset validation requires the events or records table"),
        };
        let has_resets = self.conn.query_row(
            "SELECT 1 FROM convo_legacy_resets WHERE source=?1 LIMIT 1",
            params![source],
            |row| row.get::<_,i64>(0),
        ).optional()?.is_some();
        if !has_resets { return Ok((after,true)); }
        let mut query = self.conn.prepare(query)?;
        let rows = query.query_map(params![after], |row| Ok((
            row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,
            row.get::<_,u64>(3)?,row.get::<_,Option<i64>>(4)?,
        )))?;
        let mut cursor = after;
        let mut empty = true;
        // At most 128 canonical keys are considered in one Database call. Every evidence
        // query returns at most one metadata row; no transcript or event body is loaded.
        for row in rows {
            let (rowid,session,item,n,at) = row?;
            cursor = rowid;
            empty = false;
            let number = i64::try_from(n)?;
            let own = if table == "events" {
                self.conn.query_row(own_query,params![source,&session,&item],|row|row.get::<_,i64>(0)).optional()?.is_some()
            } else {
                self.conn.query_row(own_query,params![source,&session,&item,number],|row|row.get::<_,i64>(0)).optional()?.is_some()
            };
            if own { continue; }
            let reset = if table == "events" {
                self.conn.query_row(reset_query,params![source,&session,&item,number,at],|row|row.get::<_,i64>(0)).optional()?
            } else {
                self.conn.query_row(reset_query,params![source,&session,&item,number],|row|row.get::<_,i64>(0)).optional()?
            };
            if let Some(reset) = reset {
                return Err(reset_collision(source,table,&session,&item,n,format!("reset ambiguity: this source's reset row {reset} covers a foreign canonical key absent from this source")));
            }
        }
        Ok((cursor,empty))
    }
}
