//! U06/U15: conversation SQL shares the host Amphora database and its commit pipeline.
//! The application retains timeline queries and transcript layout; durability, replication,
//! checkpointing and recovery belong exclusively to Amphora.

use std::time::{Duration, Instant};
use amphora::database::Database;
use amphora_api::sql::{Connection, FromSql};
use amphora_api::Value;
use super::batch::{group_command_cost, ApplyFailure, Batch, Committed, GROUP_MAX, WORK_MAX};
use super::{segments, amphora_sql as sql, rows::{EventRow, Row}, store::{Engine, Kept, Look, Mark, Query, SearchFrom, Unended, Changes, Took, Compacting}};

mod merge;
mod reset;
#[cfg(test)]
mod merge_tests;
#[cfg(test)]
mod reset_tests;
#[cfg(test)]
mod identity_tests;
pub use merge::ImportCollision;
use merge::PreparedLegacy;

struct LegacyPage {
    source:String, fingerprint:String, table:&'static str, expected:i64,
    next:i64, done:bool, rows:Vec<PreparedLegacy>,
}

const LEGACY_TABLES: &[&str] = &["sessions","events","records","drops","peers","copied","meta"];
const SEGMENT: usize = 512 << 10;
const TRANSCRIPT_MAX: usize = segments::LINE_MAX;
// Leave room for SQL, parameters and owned-value overhead below the product's request bound.
const PREPARED_MAX: usize = amphora_api::REQUEST_MAX.saturating_sub(1 << 20);
fn until() -> Instant { Instant::now() + Duration::from_secs(30) }
fn fail(e: impl std::fmt::Display) -> anyhow::Error { anyhow::anyhow!("{e}") }
fn txt(s: &str) -> Value { Value::Text(s.to_owned()) }
fn text(s: Option<&str>) -> Value { s.map_or(Value::Null, txt) }
fn int(n: i64) -> Value { Value::Integer(n) }
fn seq(n: u64) -> Value { Value::Integer(i64::try_from(n).unwrap_or(i64::MAX)) }

pub struct Amphora { host: crate::store::Store, database: Database, id: String, rev: u64, maintenance: Option<super::retention::Authority>, prune_pending: bool }
impl Amphora {
    pub fn from_database(database: Database) -> anyhow::Result<Self> {
        // Never initialize recursively inside the writer callback below.
        let host=crate::store::Store::from_database(database)?;
        let database=host.database();
        let id = crate::util::new_id();
        let (id, rev) = database.call(until(), move |c| {
            // U06: fresh namespaced schema only. Old files are imported separately, never
            // opened as the canonical database or silently treated as empty.
            c.execute_batch(sql::META)?;
            let s = Session { conn: c, rev: 0 };
            let have = s.meta("schema")?.and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            for (i, migration) in sql::MIGRATIONS.iter().enumerate().skip(have) {
                c.execute_batch(migration)?;
                s.set_meta("schema", &(i + 1).to_string())?;
            }

            let id = match s.meta("id")? { Some(id) => id, None => { s.set_meta("id", &id)?; id } };
            let saved=s.meta("rev")?;
            let mut rev = saved.as_deref().map(str::parse::<u64>).transpose()?.unwrap_or(0);
            for q in sql::REV_MAX {rev=rev.max(s.one_int(q,&[])?.map(u64::try_from).transpose()?.unwrap_or(0));}
            let _=i64::try_from(rev)?;
            // Publish recovery before any frontend uses the global allocator.
            let recovered=rev.to_string();
            if saved.as_deref()!=Some(recovered.as_str()) {s.set_meta("rev",&recovered)?;}
            Ok((id, rev))
        })?;
        Ok(Self { host, database, id, rev, maintenance: None, prune_pending: false })
    }
    pub(super) fn host(&self)->&crate::store::Store { &self.host }
    pub(super) fn maintenance(&mut self, authority: Option<super::retention::Authority>)->anyhow::Result<()> {
        if let Some(authority)=&authority { authority.require_store_identity(&self.host)?; }
        self.maintenance=authority;
        Ok(())
    }

    fn call<T: Send + 'static>(&mut self, f: impl FnOnce(&mut Session<'_, '_>) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let (value, rev) = self.database.call(until(), move |c| {
            let mut s = Session { conn: c, rev: 0 };
            s.rev = s.meta("rev")?.ok_or_else(||anyhow::anyhow!("conversation revision allocator is missing"))?.parse()?;
            let before = s.rev;
            let value = f(&mut s)?;
            if s.rev != before { s.set_meta("rev", &s.rev.to_string())?; }
            Ok((value, s.rev))
        })?;
        self.rev = rev; Ok(value)
    }
    /// U05/U17: one Records-bearing envelope is its own transaction. Projection-only
    /// groups retain the existing batching window, with an exact result for each envelope.
    fn apply_envelopes(&mut self, batches: &[&Batch]) -> Result<Vec<Committed>, ApplyFailure> {
        if batches.is_empty() { return Ok(Vec::new()); }
        // The same source-backed budget is checked before staging, including direct callers
        // that bypass the actor. Never split an immutable envelope or exhaust remote commands.
        let work = group_command_cost(batches.iter().copied()).ok_or_else(|| ApplyFailure::Definite(amphora_api::Refusal::TooLarge { what: "conversation wire commands".into(), size: usize::MAX, limit: WORK_MAX }))?;
        if work > WORK_MAX {
            return Err(ApplyFailure::Definite(amphora_api::Refusal::TooLarge { what: "conversation wire commands".into(), size: work, limit: WORK_MAX }));
        }
        let limit = if batches.len() > 1 { GROUP_MAX } else { PREPARED_MAX };
        let estimate = batches.iter().try_fold(0usize, |sum, batch| sum.checked_add(batch.prepared_bytes())).ok_or_else(|| ApplyFailure::Definite(amphora_api::Refusal::TooLarge { what: "prepared conversation group".into(), size: usize::MAX, limit }))?;
        if estimate > limit {
            return Err(ApplyFailure::Definite(amphora_api::Refusal::TooLarge { what: "prepared conversation group".into(), size: estimate, limit }));
        }
        let mut sessions = std::collections::BTreeSet::new();
        for batch in batches {
            if batches.len() > 1 && (batch.records().is_some() || batch.rows().iter().any(|row| matches!(row, Row::Record(_) | Row::RecordReset { .. })) || !sessions.insert(batch.session())) {
                return Err(ApplyFailure::Definite(amphora_api::invalid("conversation group", "groups require distinct sessions, no Records transactions and no transcript records or resets")));
            }
            if batch.rows().iter().any(|row| row.session() != batch.session()) {
                return Err(ApplyFailure::Definite(amphora_api::invalid("conversation batch", "a row belongs to another session")));
            }
        }
        let mut prepared = Vec::with_capacity(batches.len());
        let mut bytes = 0usize;
        for batch in batches {
            let records = if let Some(bound) = batch.records() {
                amphora_api::family(bound.namespace()).map_err(ApplyFailure::Definite)?;
                amphora_api::transaction(bound.transaction()).map_err(ApplyFailure::Definite)?;
                let held = bound.bytes().map_err(|e| ApplyFailure::Definite(amphora_api::invalid("conversation Records transaction", &e.to_string())))?;
                bytes = bytes.checked_add(held).ok_or_else(prepared_overflow)?;
                Some((bound.namespace().to_owned(), bound.api_transaction()))
            } else { None };
            // Staging may make bounded immutable orphan chunks, but never publishes timeline
            // rows or Records. It must run before the outer call, not recursively on its writer.
            let rows = self.prepare_rows(batch.rows()).map_err(preparation_failed)?;
            for row in &rows { bytes = bytes.checked_add(row_size(row).map_err(preparation_failed)?).ok_or_else(prepared_overflow)?; }
            if bytes > limit {
                return Err(ApplyFailure::Definite(amphora_api::Refusal::TooLarge { what: "prepared conversation group".into(), size: bytes, limit }));
            }
            prepared.push((rows, records));
        }
        let records_first = batches.len() == 1 && batches.first().is_some_and(|b| b.records().is_some());
        let committed = self.database.call(until(), move |conn| {
            let mut session = Session { conn, rev: 0 };
            session.rev = session.meta("rev")?.ok_or_else(||anyhow::anyhow!("conversation revision allocator is missing"))?.parse()?;
            let before = session.rev;
            let mut committed = Vec::with_capacity(prepared.len());
            for (rows, records) in prepared {
                // CAS is first: Conflict checks all conditions before writing Records, and
                // before any projection SQL. Never catch this as an inner Result/outer Ok.
                let records_rev = records.map(|(namespace, tx)| conn.records_commit(&namespace, tx)).transpose()?;
                session.apply(&rows)?;
                committed.push(Committed { conversation_rev: session.rev, records_rev });
            }
            if session.rev != before { session.set_meta("rev", &session.rev.to_string())?; }
            Ok(committed)
        }).map_err(|error| final_call_failed(error, records_first))?;
        if let Some(last) = committed.last() { self.rev = last.conversation_rev; }
        Ok(committed)
    }
    fn prepare_record(&self, rec: &super::rows::RecordRow) -> anyhow::Result<super::rows::RecordRow> {
        // U06/U15: never capture a large source line in a Database call. Stage immutable
        // bytes first; publishing the small reference remains atomic with the timeline.
        if let Some(marker)=rec.kept.as_ref().filter(|b|b.first()==Some(&3)) {
            anyhow::ensure!(rec.line.is_empty() && marker.len()==41,"invalid prepared transcript");
            return Ok(rec.clone());
        }
        let decoded;
        let line=match &rec.kept { Some(b)=>{decoded=decoded_value(b)?;decoded.as_str()},None=>rec.line.as_str() };
        anyhow::ensure!(line.len()<=TRANSCRIPT_MAX,"transcript exceeds original 256 MiB line bound; migration refused");
        let (line,kept)=if line.len()>SEGMENT { (String::new(),Some(segments::stage(&self.database,line)?)) } else { (line.to_owned(),None) };
        Ok(super::rows::RecordRow {session:rec.session.clone(),source:rec.source.clone(),n:rec.n,end:rec.end,line,kept})
    }
    fn prepare_rows(&self, rows: &[Row]) -> anyhow::Result<Vec<Row>> {
        let mut out=Vec::with_capacity(rows.len());
        let mut bytes=0usize;
        for row in rows {
            if !matches!(row,Row::Record(_)) {anyhow::ensure!(row_size(row)?<=PREPARED_MAX,"conversation row exceeds bounded SQL request");}
            let prepared=match row { Row::Record(rec)=>Row::Record(Box::new(self.prepare_record(rec)?)),other=>other.clone() };
            bytes=bytes.checked_add(row_size(&prepared)?).ok_or_else(||anyhow::anyhow!("conversation request size overflow"))?;
            anyhow::ensure!(bytes<=PREPARED_MAX,"conversation batch exceeds bounded SQL request; no timeline rows committed");
            out.push(prepared);
        }
        Ok(out)
    }
    fn prepare_import(&self, table: &str, page: &mut [(i64,Vec<Value>)]) -> anyhow::Result<()> {
        let mut bytes=0usize;
        for (_,values) in page {
            if table=="records" {
                let index=match values.len(){3=>2,6=>4,_=>anyhow::bail!("unsupported legacy transcript layout")};
                let value=values.get_mut(index).ok_or_else(||anyhow::anyhow!("legacy transcript has no line"))?;
                // Move the owned legacy value out instead of cloning up to 256 MiB.
                let line=match std::mem::replace(value,Value::Null) { Value::Text(line)=>line,Value::Blob(b)=>decoded_value(&b)?,_=>anyhow::bail!("unknown legacy transcript encoding") };
                anyhow::ensure!(line.len()<=TRANSCRIPT_MAX,"legacy transcript exceeds original 256 MiB line bound; migration refused");
                *value=if line.len()>SEGMENT {Value::Blob(segments::stage(&self.database,&line)?)} else {Value::Text(line)};
            }
            for value in values { bytes=bytes.checked_add(value_size(value)?).ok_or_else(||anyhow::anyhow!("legacy request size overflow"))?; }
            anyhow::ensure!(bytes<=PREPARED_MAX,"legacy {table} page exceeds bounded SQL request; migration refused without advancing cursor");
        }
        Ok(())
    }
    fn prepare_legacy(&self, old:&crate::records::legacy::Legacy, table:&str, mut page:Vec<(i64,Vec<Value>)>)->anyhow::Result<Vec<PreparedLegacy>> {
        self.prepare_import(table,&mut page)?;
        let sessions=old.has("sessions")?;
        let mut out=Vec::with_capacity(page.len());
        let mut bytes=0usize;
        for (id,values) in page {
            let epoch=if table=="sessions" && values.len()==10 {
                u64::from_sql(values.get(7).ok_or_else(||anyhow::anyhow!("missing legacy epoch"))?)?
            } else if sessions && matches!(table,"events"|"records"|"drops") {
                let index=usize::from(table=="drops");
                let session=String::from_sql(values.get(index).ok_or_else(||anyhow::anyhow!("missing legacy session"))?)?;
                let mut epoch=0;
                old.query("SELECT * FROM sessions WHERE id=?1",&[turso_core::Value::from_text(session)],|row| {
                    if row.get_values().count()==10 {epoch=u64::try_from(row.get::<i64>(7)?)?;}
                    Ok(false)
                })?;
                epoch
            } else {0};
            let mut archive=Vec::with_capacity(values.len());
            let mut markers=Vec::new();
            for value in &values {
                let archived=match value {
                    Value::Text(s) if s.len()>SEGMENT=>Value::Blob(segments::stage(&self.database,s)?),
                    other=>other.clone(),
                };
                if let Value::Blob(marker)=&archived && marker.len()==41 && marker.first()==Some(&3) {
                    // U06/U15: archive-only references need the same atomic ready/shape
                    // validation as a live record, even if projection deduplicates the row.
                    bytes=bytes.checked_add(marker.len()).ok_or_else(||anyhow::anyhow!("legacy marker size overflow"))?;
                    markers.push(marker.clone());
                }
                archive.push(archived);
                bytes=bytes.checked_add(value_size(value)?).ok_or_else(||anyhow::anyhow!("legacy page overflow"))?;
            }
            let archived=postcard::to_allocvec(&archive)?;
            bytes=bytes.checked_add(archived.len()).ok_or_else(||anyhow::anyhow!("legacy page overflow"))?;
            anyhow::ensure!(bytes<=PREPARED_MAX,"legacy {table} page and source history exceed bounded SQL request; cursor was not advanced");
            out.push(PreparedLegacy {id,values,archived,markers,epoch});
        }
        Ok(out)
    }
}

fn prepared_overflow() -> ApplyFailure {
    ApplyFailure::Definite(amphora_api::Refusal::TooLarge { what: "prepared conversation group".into(), size: usize::MAX, limit: PREPARED_MAX })
}
fn preparation_failed(error: anyhow::Error) -> ApplyFailure {
    let refusal = error.downcast_ref::<amphora_api::Refusal>().cloned().or_else(|| match error.downcast_ref::<amphora_api::sql::Error>() {
        Some(amphora_api::sql::Error::Refusal(refusal)) => Some(refusal.clone()),
        _ => None,
    });
    match refusal {
        Some(error @ (amphora_api::Refusal::UnknownOutcome | amphora_api::Refusal::Timeout | amphora_api::Refusal::Unavailable(_) | amphora_api::Refusal::Fenced(_))) => ApplyFailure::Unknown(error),
        Some(error) => ApplyFailure::Definite(error),
        None => ApplyFailure::Definite(amphora_api::invalid("conversation preparation", &format!("{error:#}"))),
    }
}
fn final_call_failed(error: amphora_api::Refusal, records_first: bool) -> ApplyFailure {
    // Only this source-proven refusal happened before both Records and projection mutations.
    // Other SDK errors can include failed rollback or a lost durable response: be conservative.
    if records_first && matches!(error, amphora_api::Refusal::Conflict { .. }) { ApplyFailure::Definite(error) }
    else { ApplyFailure::Unknown(error) }
}

fn value_size(value:&Value)->anyhow::Result<usize> {
    64usize.checked_add(match value {Value::Text(s)=>s.len(),Value::Blob(b)=>b.len(),_=>8}).ok_or_else(||anyhow::anyhow!("value size overflow"))
}
fn row_size(row:&Row)->anyhow::Result<usize> {
    let mut strings=vec![row.session()];
    let extra=match row {
        Row::Session {info,meta,state,changes,..}=>{strings.extend([info,meta,state,changes].into_iter().filter_map(|s|s.as_deref()));0},
        Row::Event(e)=>{strings.extend([e.id.as_str(),e.kind.as_str(),e.text.as_str(),e.body.as_str()]);strings.extend([&e.parent,&e.by,&e.level,&e.tool,&e.status].into_iter().filter_map(|s|s.as_deref()));0},
        Row::Record(r)=>{strings.extend([r.source.as_str(),r.line.as_str()]);r.kept.as_ref().map_or(0,Vec::len)},
        Row::RecordReset {source,..}=>{strings.push(source);0},
        Row::Machine {machine,..}=>{strings.push(machine);0},
        Row::Reset {..}|Row::Closed {..}=>0,
    };
    let mut bytes=512usize.checked_add(extra).ok_or_else(||anyhow::anyhow!("row size overflow"))?;
    for string in strings {bytes=bytes.checked_add(string.len()).ok_or_else(||anyhow::anyhow!("row size overflow"))?;}
    Ok(bytes)
}

fn query_limit(params:&[Value],index:usize)->anyhow::Result<usize> {
    let n=i64::from_sql(params.get(index).ok_or_else(||anyhow::anyhow!("missing query limit"))?)?;
    Ok(if n<0 {usize::MAX}else{usize::try_from(n)?})
}
struct Statement { sql: String, changed: u64 }
impl Statement { fn n_change(&self) -> u64 { self.changed } }
struct TRow(Vec<Value>);
impl TRow {
    fn get<T: FromSql>(&self, index: usize) -> amphora_api::sql::Result<T> {
        T::from_sql(self.0.get(index).ok_or(amphora_api::sql::Error::InvalidColumnIndex(index))?)
    }
    fn get_value(&self, index: usize) -> &Value { self.0.get(index).unwrap_or(&Value::Null) }
}
struct Session<'c, 'db> { conn: &'c Connection<'db>, rev: u64 }
impl Session<'_, '_> {
    fn prepare(&self, sql: &str) -> anyhow::Result<Statement> { Ok(Statement { sql: sql.to_owned(), changed: 0 }) }
    fn run(&self, stmt: &mut Statement, params: &[Value], mut each: impl FnMut(&TRow) -> anyhow::Result<()>) -> anyhow::Result<()> { self.run_while(stmt, params, |r| each(r).map(|()| true)) }
    fn run_while(&self, stmt: &mut Statement, params: &[Value], each: impl FnMut(&TRow) -> anyhow::Result<bool>) -> anyhow::Result<()> {
        self.run_from(stmt,params,None,None,each)
    }
    fn run_from(&self, stmt: &mut Statement, params: &[Value], start: Option<Vec<Value>>, budget: Option<usize>, mut each: impl FnMut(&TRow) -> anyhow::Result<bool>) -> anyhow::Result<()> {
        let mut query = self.conn.prepare(&stmt.sql)?;
        let columns = query.column_count();
        if columns == 0 { stmt.changed = u64::try_from(query.execute(params)?)?; return Ok(()); }
        if let Some(plan)=sql::event_page(&stmt.sql) {
            let mut remaining=match plan.limit {
                Some(i)=>query_limit(params,i)?,
                None=>budget.unwrap_or(usize::MAX),
            };
            if remaining==0 {return Ok(());}
            let mut first=self.conn.prepare(&plan.first)?;
            let mut equal=self.conn.prepare(&plan.equal)?;
            let mut lower=self.conn.prepare(&plan.lower)?;
            let mut payload=self.conn.prepare(plan.payload)?;
            let mut cursor=match start.as_deref() {
                Some([key,rowid])=>Some((i64::from_sql(key)?,i64::from_sql(rowid)?)),
                Some(_)=>anyhow::bail!("invalid search continuation"),
                None=>None,
            };
            loop {
                let capacity=remaining.min(sql::KEY_PAGE);
                let mut keys=Vec::with_capacity(capacity);
                if let Some((key,rowid))=cursor {
                    let mut bound=if plan.session {vec![params.first().cloned().ok_or_else(||anyhow::anyhow!("missing session"))?]}else{vec![]};
                    bound.extend([int(key),int(rowid),int(i64::try_from(capacity)?)]);
                    let mut rows=equal.query(bound)?;
                    while let Some(r)=rows.next()? {keys.push((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?));}
                    if keys.len()<capacity {
                        let mut bound=if plan.session {vec![params.first().cloned().ok_or_else(||anyhow::anyhow!("missing session"))?]}else{vec![]};
                        bound.extend([int(key),int(i64::try_from(capacity.saturating_sub(keys.len()))?)]);
                        let mut rows=lower.query(bound)?;
                        while let Some(r)=rows.next()? {keys.push((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?));}
                    }
                } else {
                    let mut bound=params.to_vec();
                    bound.push(int(i64::try_from(capacity)?));
                    let mut rows=first.query(bound)?;
                    while let Some(r)=rows.next()? {keys.push((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?));}
                }
                let count=keys.len();
                for (rowid,key) in keys {
                    cursor=Some((key,rowid));
                    let mut rows=payload.query(vec![int(rowid)])?;
                    if let Some(r)=rows.next()? {
                        let values=(0..columns).map(|i|r.get::<_,Value>(i)).collect::<amphora_api::sql::Result<Vec<_>>>()?;
                        remaining=remaining.saturating_sub(1);
                        if !each(&TRow(values))? || remaining==0 {return Ok(());}
                    }
                }
                if count<capacity {break;}
            }
            stmt.changed=self.conn.changes();
            return Ok(());
        }
        if let Some(plan)=sql::inventory(&stmt.sql) {
            let requested=match plan.limit {Some(i)=>query_limit(params,i)?,None=>usize::MAX};
            if requested==0 {return Ok(());}
            let mut keys:Vec<(i64,i64,String)>=Vec::new();
            let mut query=self.conn.prepare(&plan.scan)?;
            let first_sql=plan.scan.replace(&format!(" AND rowid>?{}",params.len()+1),"");
            let mut first=self.conn.prepare(&first_sql)?;
            let mut after=None;
            loop {
                let mut bound=params.to_vec();
                let mut rows=if let Some(rowid)=after {bound.push(int(rowid));query.query(bound)?}else{first.query(bound)?};
                if let Some(r)=rows.next()? {
                    let rowid=r.get::<_,i64>(0)?;
                    after=Some(rowid);
                    keys.push((rowid,r.get(1)?,r.get(2)?));
                } else {break;}
            }
            keys.sort_by(|a,b|a.1.cmp(&b.1).then(a.2.cmp(&b.2)));
            keys.truncate(requested);
            let mut payload=self.conn.prepare(plan.payload)?;
            for (rowid,_,_) in keys {
                let mut rows=payload.query(vec![int(rowid)])?;
                if let Some(r)=rows.next()? {
                    let values=(0..columns).map(|i|r.get::<_,Value>(i)).collect::<amphora_api::sql::Result<Vec<_>>>()?;
                    if !each(&TRow(values))? {return Ok(());}
                }
            }
            stmt.changed=self.conn.changes();
            return Ok(());
        }
        // U05/U06/U15: Rows is materialized, not a streaming product cursor. Fixed
        // query plans seek past the last key within this SAME Database.call snapshot.
        // One event can approach RESULT_MAX; transcripts are <=512 KiB or manifests.
        if let Some(plan)=sql::iteration(&stmt.sql) {
            let mut remaining=match plan.limit {
                Some(index)=> {
                    let n=i64::from_sql(params.get(index).ok_or_else(||anyhow::anyhow!("missing query limit"))?)?;
                    if n<0 {usize::MAX}else{usize::try_from(n)?}
                },
                None=>usize::MAX,
            };
            if remaining==0 {return Ok(());}
            let mut first=self.conn.prepare(&plan.first)?;
            let mut next=self.conn.prepare(&plan.next)?;
            let mut cursor=start;
            loop {
                let mut bound=params.to_vec();
                if let Some(keys)=&cursor {bound.extend(keys.iter().cloned());}
                let mut rows=if cursor.is_none() {first.query(bound)?}else{next.query(bound)?};
                let mut count=0usize;
                while let Some(r)=rows.next()? {
                    let keys=plan.keys.iter().map(|&i|r.get::<_,Value>(i)).collect::<amphora_api::sql::Result<Vec<_>>>()?;
                    let values=(0..columns).map(|i|r.get::<_,Value>(i)).collect::<amphora_api::sql::Result<Vec<_>>>()?;
                    cursor=Some(keys);
                    count=count.saturating_add(1);
                    remaining=remaining.saturating_sub(1);
                    if !each(&TRow(values))? || remaining==0 {return Ok(());}
                }
                if count<plan.page {break;}
            }
        } else {
            // Singleton reads and DML execute once. Unrecognized SQL is passed through
            // byte-for-byte, never wrapped or heuristically rewritten.
            let mut rows=query.query(params)?;
            while let Some(r)=rows.next()? {
                let values=(0..columns).map(|i|r.get::<_,Value>(i)).collect::<amphora_api::sql::Result<Vec<_>>>()?;
                if !each(&TRow(values))? {return Ok(());}
            }
        }

        stmt.changed = self.conn.changes(); Ok(())
    }
    fn transaction<T>(&mut self, f: impl FnOnce(&mut Self) -> anyhow::Result<T>) -> anyhow::Result<T> { f(self) }
    fn touch(&self, _: &str) {}
    fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.prepare(sql::META_GET)?;
        let mut out = None;
        self.run(&mut stmt, &[Value::Text(key.to_owned())], |r| {
            out = r.get::<String>(0).ok();
            Ok(())
        })?;
        Ok(out)
    }

    fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut stmt = self.prepare(sql::META_SET)?;
        self.run(&mut stmt, &[Value::Text(key.to_owned()), Value::Text(value.to_owned())], |_| Ok(()))
    }

    /// The first column of the first row a query answers, as a number (None: no row, or NULL).
    fn one_int(&self, sql: &str, params: &[Value]) -> anyhow::Result<Option<i64>> {
        let mut stmt = self.prepare(sql)?;
        let mut out = None;
        self.run(&mut stmt, params, |r| {
            if out.is_none() {
                out = r.get::<i64>(0).ok();
            }
            Ok(())
        })?;
        Ok(out)
    }

    fn event_rows(&self, sql: &str, params: &[Value]) -> anyhow::Result<Vec<EventRow>> {
        let mut stmt = self.prepare(sql)?;
        let mut out = Vec::new();
        self.run(&mut stmt, params, |r| {
            out.push(event_of(r)?);
            Ok(())
        })?;
        Ok(out)
    }
}

/// An event row as `sql::EVENTS_*` select it.
fn event_of(r: &TRow) -> anyhow::Result<EventRow> {
    let s = |i: usize| r.get::<String>(i).ok();
    Ok(EventRow {
        session: s(0).unwrap_or_default(),
        id: s(1).unwrap_or_default(),
        seq: r.get::<i64>(2).ok().and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
        at: r.get::<i64>(3).unwrap_or(0),
        parent: s(4),
        by: s(5),
        kind: s(6).unwrap_or_default(),
        level: s(7),
        tool: s(8),
        status: s(9),
        text: s(10).unwrap_or_default(),
        body: s(11).ok_or_else(|| anyhow::anyhow!("an event without its body"))?,
    })
}

/// The statements a transaction writes with, prepared once for it.
struct Writes {
    session: Statement,
    machine: Statement,
    open: Statement,
    close: Statement,
    event: Statement,
    reset: Statement,
    dropped: Statement,
    records_reset: Statement,
}


impl Session<'_, '_> {
    fn writes(&self) -> anyhow::Result<Writes> {
        Ok(Writes {
            session: self.prepare(sql::SESSION_UPSERT)?,
            machine: self.prepare(sql::SESSION_MACHINE)?,
            open: self.prepare(sql::SESSION_OPEN)?,
            close: self.prepare(sql::SESSION_CLOSE)?,
            event: self.prepare(sql::EVENT_UPSERT)?,
            reset: self.prepare(sql::EVENTS_RESET)?,
            dropped: self.prepare(sql::DROP_ADD)?,
            records_reset: self.prepare(sql::RECORDS_RESET)?,
        })
    }

    /// One row, as write number `rev`: whether it wrote anything. An event or a record line that
    /// comes again as it is kept writes nothing, and the number goes unused.
    fn write(&self, w: &mut Writes, row: &Row, rev: u64) -> anyhow::Result<bool> {
        let r = seq(rev);
        let none = |_: &TRow| Ok(());
        match row {
            Row::Session { session, info, meta, state, changes, epoch, open, at } => {
                self.run(
                    &mut w.session,
                    &[txt(session), text(info.as_deref()), text(meta.as_deref()), text(state.as_deref()), text(changes.as_deref()), int(*at), seq(epoch.unwrap_or(0)), r],
                    none,
                )?;
                if *open {
                    self.run(&mut w.open, &[txt(session)], none)?;
                    self.touch(session);
                }
                if *open || epoch.is_some() {super::retention::projection(self.conn, session, rev, *open, "native")?;}
            }
            Row::Event(e) => {
                self.run(
                    &mut w.event,
                    &[
                        txt(&e.session),
                        txt(&e.id),
                        seq(e.seq),
                        int(e.at),
                        text(e.parent.as_deref()),
                        text(e.by.as_deref()),
                        txt(&e.kind),
                        text(e.level.as_deref()),
                        text(e.tool.as_deref()),
                        text(e.status.as_deref()),
                        txt(&e.text),
                        txt(&e.body),
                        r,
                    ],
                    none,
                )?;
                return Ok(w.event.n_change() > 0);
            }
            Row::Reset { session, from, at } => {
                self.run(&mut w.reset, &[txt(session), seq(from.unwrap_or(0))], none)?;
                self.run(&mut w.dropped, &[r, txt(session), txt("events"), Value::Null, from.map_or(Value::Null, seq), int(*at)], none)?;
            }
            Row::Closed { session, at } => {
                self.run(&mut w.close, &[txt(session), int(*at), r], none)?;
                super::retention::closed(self.conn, session, rev, "native")?;
            }
            Row::Record(rec) => { return self.write_record(rec, rev); }
            Row::RecordReset { session, source, at } => {
                self.run(&mut w.records_reset, &[txt(session), txt(source)], none)?;
                self.conn.execute("DELETE FROM convo_segments WHERE session=?1 AND source=?2", vec![txt(session),txt(source)])?;
                self.run(&mut w.dropped, &[r, txt(session), txt("record"), txt(source), Value::Null, int(*at)], none)?;
            }
            // Where it runs isn't a write of its own: it goes with the session's row.
            Row::Machine { session, machine } => {
                self.run(&mut w.machine, &[txt(session), txt(machine)], none)?;
                self.touch(session);
            }
        }
        Ok(true)
    }

    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.transaction(|t| {
            let mut w = t.writes()?;
            let mut mutations=super::retention::Mutations::default();
            for row in rows {
                t.rev = t.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision overflow"))?;
                if !t.write(&mut w, row, t.rev)? {
                    t.rev = t.rev.saturating_sub(1);
                }else{
                    match row {
                        Row::Record(rec)=>mutations.record(&rec.session,&rec.source,rec.n,t.rev,false)?,
                        Row::RecordReset {session,source,..}=>mutations.record(session,source,0,t.rev,true)?,
                        _=>{},
                    }
                }
            }
            mutations.flush(t.conn)
        })
    }

    fn unended(&mut self, since: i64) -> anyhow::Result<Vec<Unended>> {
        let mut stmt = self.prepare(sql::SESSIONS_UNENDED)?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[int(since)], |r| {
            if let (Ok(id), Ok(info)) = (r.get::<String>(0), r.get::<String>(1)) {
                out.push(Unended { id, info, meta: r.get::<String>(2).ok(), state: r.get::<String>(3).ok(), machine: r.get::<String>(4).ok() });
            }
            Ok(())
        })?;
        Ok(out)
    }

    fn ended(&mut self, after: (i64, &str), limit: usize) -> anyhow::Result<Vec<(String, i64)>> {
        let mut stmt = self.prepare(sql::SESSIONS_ENDED_AFTER)?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[int(after.0), txt(after.1), int(i64::try_from(limit).unwrap_or(i64::MAX))], |r| {
            if let (Ok(id), Ok(at)) = (r.get::<String>(0), r.get::<i64>(1)) {
                out.push((id, at));
            }
            Ok(())
        })?;
        Ok(out)
    }

    fn kept(&mut self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        let mut get = self.prepare(sql::SESSION_GET)?;
        let mut kept: Option<Kept> = None;
        self.run(&mut get, &[Value::Text(session.to_owned())], |r| {
            let s = |i: usize| r.get::<String>(i).ok();
            kept = Some(Kept { info: s(0), meta: s(1), state: s(2), changes: s(3), updated_at: r.get::<i64>(4).unwrap_or(0), closed_at: r.get::<i64>(5).ok(), events: Vec::new() });
            Ok(())
        })?;
        let mut events = self.event_rows(sql::EVENTS_LAST, &[Value::Text(session.to_owned()), int(i64::try_from(last).unwrap_or(i64::MAX))])?;
        if kept.is_none() && events.is_empty() {
            return Ok(None);
        }
        events.reverse();
        let mut k = kept.unwrap_or_default();
        k.events = events;
        Ok(Some(k))
    }

    fn events(&mut self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        let limit = int(i64::try_from(limit).unwrap_or(i64::MAX));
        let mut rows = match before {
            Some(b) => self.event_rows(sql::EVENTS_BEFORE, &[Value::Text(session.to_owned()), seq(b), limit])?,
            None => self.event_rows(sql::EVENTS_LAST, &[Value::Text(session.to_owned()), limit])?,
        };
        rows.reverse();
        Ok(rows)
    }

    fn search_some(&mut self, q: &Query, from: Option<SearchFrom>, rows: usize, want: usize) -> anyhow::Result<(Vec<EventRow>, Option<SearchFrom>)> {
        // Newest first by an index, so nothing is sorted and it stops once it has `want`; the words
        // matched here, as LIKE would (any case of an ASCII letter; `%` and `_` as they are).
        let words: Vec<String> = q.q.split_whitespace().map(str::to_ascii_lowercase).collect();
        let (sql, params) = match &q.session {
            Some(s) => (sql::SEARCH_SESSION, vec![txt(s), int(from.map_or(i64::MAX, |f| f.key))]),
            None => (sql::SEARCH_ALL, vec![int(from.map_or_else(|| q.until.map_or(i64::MAX, |u| u.saturating_sub(1)), |f| f.key))]),
        };
        let mut stmt = self.prepare(sql)?;
        let (mut hits, mut looked, mut last, mut more) = (Vec::new(), 0usize, None, false);
        self.run_from(&mut stmt, &params, from.map(|f|vec![int(f.key),int(f.rowid)]), Some(rows.max(1)), |r| {
            let (rowid, key) = (r.get::<i64>(0).map_err(fail)?, r.get::<i64>(1).map_err(fail)?);
            // Looked at already (the same key as where it goes on from, a rowid not before it).
            if from.is_some_and(|f| key == f.key && rowid >= f.rowid) {
                return Ok(true);
            }
            let at = r.get::<i64>(2).unwrap_or(0);
            // Across agents it goes back in time: older than it looks back to, nothing more fits.
            if q.session.is_none() && q.since.is_some_and(|s| at < s) {
                return Ok(false);
            }
            looked += 1;
            last = Some(SearchFrom { key, rowid });
            let column = |i: usize| r.get::<String>(i).ok();
            let fits = q.since.is_none_or(|s| at >= s)
                && q.until.is_none_or(|u| at < u)
                && [(&q.kind, 3), (&q.tool, 4), (&q.status, 5)].into_iter().all(|(w, i)| w.as_deref().is_none_or(|w| column(i).as_deref() == Some(w)))
                && (words.is_empty() || {
                    let text = column(6).unwrap_or_default().to_ascii_lowercase();
                    words.iter().all(|w| text.contains(w.as_str()))
                });
            if fits {
                hits.push(rowid);
            }
            more = hits.len() >= want || looked >= rows;
            Ok(!more)
        })?;
        let mut whole = self.prepare(sql::EVENT_BY_ROWID)?;
        let mut found = Vec::with_capacity(hits.len());
        for rowid in hits {
            self.run(&mut whole, &[int(rowid)], |r| {
                found.push(event_of(r)?);
                Ok(())
            })?;
        }
        Ok((found, if more { last } else { None }))
    }
}

fn decoded_value(b: &[u8]) -> anyhow::Result<String> {
    use std::io::Read as _;
    anyhow::ensure!(b.first()==Some(&1), "unknown transcript encoding");
    let mut bytes=Vec::new();
    brotli::Decompressor::new(b.get(1..).unwrap_or_default(),4096).take(u64::try_from(TRANSCRIPT_MAX)?.saturating_add(1)).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len()<=TRANSCRIPT_MAX,"compressed transcript exceeds original 256 MiB line bound; migration refused");
    Ok(String::from_utf8(bytes)?)
}
fn decoded(value: &Value) -> anyhow::Result<String> {
    match value {
        Value::Text(s)=>{anyhow::ensure!(s.len()<=TRANSCRIPT_MAX,"transcript exceeds original 256 MiB line bound");Ok(s.clone())},
        Value::Blob(b)=>decoded_value(b),
        _=>anyhow::bail!("unknown transcript encoding"),
    }
}

impl Session<'_, '_> {
    fn line(&self, session: &str, source: &str, n: u64, value: &Value) -> anyhow::Result<String> {
        let Value::Blob(marker) = value else { return decoded(value) };
        if marker.first()==Some(&3) {return segments::read(self.conn,marker);}
        if marker.first() != Some(&2) { return decoded(value); }
        // Codec 2 is retained for already committed segment manifests. New writes stage codec 3.
        let size = marker.get(1..9).and_then(|b| <[u8; 8]>::try_from(b).ok()).map(u64::from_le_bytes).ok_or_else(|| anyhow::anyhow!("invalid transcript segment marker"))?;
        anyhow::ensure!(marker.len() == 9 && size <= u64::try_from(TRANSCRIPT_MAX)?, "invalid transcript segment size");
        let mut bytes = Vec::new();
        let count=usize::try_from(size)?.div_ceil(SEGMENT);
        for expected in 0..count {
            let part=self.conn.query_row("SELECT length(value),substr(value,1,?5) FROM convo_segments WHERE session=?1 AND source=?2 AND n=?3 AND part=?4",vec![txt(session),txt(source),seq(n),seq(u64::try_from(expected)?),seq(u64::try_from(SEGMENT)?)],|r|Ok((r.get::<_,usize>(0)?,r.get::<_,Vec<u8>>(1)?)));
            let (part_size,part)=part?;
            let left=usize::try_from(size)?.checked_sub(bytes.len()).ok_or_else(||anyhow::anyhow!("invalid transcript segment size"))?;
            anyhow::ensure!(part_size==part.len() && part_size==left.min(SEGMENT), "transcript segment is incomplete or malformed");
            bytes.extend_from_slice(&part);
        }
        let extra=self.conn.query_row("SELECT part FROM convo_segments WHERE session=?1 AND source=?2 AND n=?3 AND (part<0 OR part>=?4) LIMIT 1",vec![txt(session),txt(source),seq(n),seq(u64::try_from(count)?)],|r|r.get::<_,i64>(0));
        anyhow::ensure!(matches!(extra,Err(amphora_api::sql::Error::QueryReturnedNoRows)),"transcript has unexpected extra segments");
        anyhow::ensure!(u64::try_from(bytes.len())? == size, "transcript segments are incomplete");
        Ok(String::from_utf8(bytes)?)
    }
    fn write_record(&self, rec: &super::rows::RecordRow, rev: u64) -> anyhow::Result<bool> {
        let value=match &rec.kept {
            Some(marker) if marker.first()==Some(&3)=>{segments::validate(self.conn,marker)?;Value::Blob(marker.clone())},
            Some(_)=>anyhow::bail!("transcript must be prepared before SQL submission"),
            None=>{anyhow::ensure!(rec.line.len()<=SEGMENT,"transcript must be staged before SQL submission");Value::Text(rec.line.clone())},
        };
        let mut old = self.prepare(sql::RECORD_LINE)?;
        let mut same = false;
        self.run(&mut old, &[txt(&rec.session),txt(&rec.source),seq(rec.n)], |row| {
            if u(row,0)!=rec.end {return Ok(());}
            let prior=row.get_value(1);
            same=prior==&value;
            if !same {
                let prior=self.line(&rec.session,&rec.source,rec.n,prior)?;
                same=match &value {
                    Value::Text(line)=>prior==*line,
                    Value::Blob(marker)=>{
                        use sha2::{Digest as _,Sha256};
                        let size=marker.get(1..9).and_then(|b|<[u8;8]>::try_from(b).ok()).map(u64::from_le_bytes);
                        size==u64::try_from(prior.len()).ok() && marker.get(9..)==Some(Sha256::digest(prior.as_bytes()).as_slice())
                    },
                    _=>false,
                };
            }
            Ok(())
        })?;
        if same { return Ok(false); }
        self.conn.execute("DELETE FROM convo_segments WHERE session=?1 AND source=?2 AND n=?3", vec![txt(&rec.session),txt(&rec.source),seq(rec.n)])?;
        self.conn.execute(sql::RECORD_UPSERT, vec![txt(&rec.session),txt(&rec.source),seq(rec.n),seq(rec.end),value,seq(rev)])?;
        Ok(true)
    }
    fn marks(&mut self, session: &str) -> anyhow::Result<Vec<Mark>> {
        let mut sources = self.prepare(sql::RECORD_SOURCES)?;
        let mut last = Vec::new();
        self.run(&mut sources, &[txt(session)], |r| { last.push((r.get::<String>(0)?,u(r,1))); Ok(()) })?;
        let mut out = Vec::new();
        for (source,n) in last {
            let mut stmt = self.prepare(sql::RECORD_LINE)?;
            self.run(&mut stmt, &[txt(session),txt(&source),seq(n)], |r| {
                out.push(Mark { source: source.clone(), n, end:u(r,0), line:self.line(session,&source,n,r.get_value(1))? }); Ok(())
            })?;
        }
        Ok(out)
    }
    fn record(&mut self, session: &str, source: &str, from: u64, limit: usize) -> anyhow::Result<Vec<(u64,String)>> {
        let mut stmt = self.prepare(sql::RECORD_LINES)?;
        let mut out = Vec::new();
        let mut bytes=0usize;
        self.run_while(&mut stmt, &[txt(session),txt(source),seq(from),seq(u64::try_from(limit.min(4096))?)], |r| {
            let n = u(r,0); let value=r.get_value(1);
            let known=match value {Value::Text(s)=>Some(s.len()),Value::Blob(b) if matches!(b.first(),Some(2|3))=>b.get(1..9).and_then(|v|<[u8;8]>::try_from(v).ok()).map(u64::from_le_bytes).map(usize::try_from).transpose()?,_=>None};
            // U06: byte-bounded pages permit one line up to the original 256 MiB bound.
            // Callers advance by the returned line number until an empty page, never by count.
            if !out.is_empty() && known.is_some_and(|n|bytes.saturating_add(n)>4<<20) {return Ok(false);}
            let line=self.line(session,source,n,value)?;
            if !out.is_empty() && bytes.saturating_add(line.len())>4<<20 {return Ok(false);}
            bytes=bytes.checked_add(line.len()).ok_or_else(||anyhow::anyhow!("transcript page overflow"))?;
            out.push((n,line)); Ok(bytes<4<<20)
        })?;
        Ok(out)
    }
}
fn u(row: &TRow, column: usize) -> u64 { row.get::<i64>(column).ok().and_then(|n| u64::try_from(n).ok()).unwrap_or(0) }

fn retired<T>() -> anyhow::Result<T> { anyhow::bail!("U15: conversation feed/mirror recovery is retired; use Amphora replication") }
impl Engine for Amphora {
    fn id(&self) -> String { self.id.clone() }
    fn apply_batch(&mut self, batch: &Batch) -> Result<Committed, ApplyFailure> {
        self.apply_envelopes(&[batch])?.pop().ok_or_else(|| ApplyFailure::Unknown(amphora_api::Refusal::Unavailable("the conversation writer returned no batch result".into())))
    }
    fn apply_group(&mut self, batches: &[&Batch]) -> Result<Vec<Committed>, ApplyFailure> { self.apply_envelopes(batches) }
    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<()> { let rows = self.prepare_rows(rows)?; self.call(move |s| s.apply(&rows)) }
    fn top(&mut self) -> u64 { self.rev }
    fn kept(&mut self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> { let session=session.to_owned(); self.call(move |s| s.kept(&session,last)) }
    fn events(&mut self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> { let session=session.to_owned(); self.call(move |s| s.events(&session,before,limit)) }
    fn search_some(&mut self, q: &Query, from: Option<SearchFrom>, rows: usize, want: usize) -> anyhow::Result<(Vec<EventRow>,Option<SearchFrom>)> { let q=q.clone(); self.call(move |s| s.search_some(&q,from,rows,want)) }
    fn marks(&mut self, session: &str) -> anyhow::Result<Vec<Mark>> { let session=session.to_owned(); self.call(move |s| s.marks(&session)) }
    fn record(&mut self, session: &str, source: &str, from: u64, limit: usize) -> anyhow::Result<Vec<(u64,String)>> { let session=session.to_owned(); let source=source.to_owned(); self.call(move |s| s.record(&session,&source,from,limit)) }
    fn unended(&mut self, since: i64) -> anyhow::Result<Vec<Unended>> { self.call(move |s| s.unended(since)) }
    fn ended(&mut self, after: (i64,&str), limit: usize) -> anyhow::Result<Vec<(String,i64)>> { let (at,id)=(after.0,after.1.to_owned()); self.call(move |s| s.ended((at,&id),limit)) }
    fn is_empty(&mut self) -> anyhow::Result<bool> { self.call(|s| Ok(s.one_int(sql::ANY_SESSION,&[])?.is_none() && s.one_int(sql::ANY_EVENT,&[])?.is_none())) }
    fn changes(&mut self, _: u64, _: usize, _: bool) -> anyhow::Result<Changes> { retired() }
    fn take(&mut self, _: &str, _: Option<&str>, _: u64, _: &Changes) -> anyhow::Result<Took> { retired() }
    fn take_copy(&mut self, _: &str, _: u64, _: &Changes) -> anyhow::Result<Took> { retired() }
    fn end_copy(&mut self) -> anyhow::Result<usize> { retired() }
    fn peer(&mut self, _: &str) -> anyhow::Result<u64> { retired() }
    fn peers(&mut self) -> anyhow::Result<Vec<(String,u64,i64)>> { retired() }
    fn squeeze_some(&mut self, _: usize) -> anyhow::Result<bool> { Ok(false) }
    fn maintenance_pending(&self)->bool { self.prune_pending }
    fn prune_some(&mut self, before: i64, limit: usize) -> anyhow::Result<usize> {
        self.prune_pending=false;
        if limit==0 {return Ok(0);}
        let authority=self.maintenance.clone().ok_or(super::retention::MaintenanceRefusal::Secondary)?;
        authority.require_store(&self.host)?;
        let outcome=self.call(move |s| {
            match authority.state(s.conn)? {
                super::retention::AuthorityState::Stale=>return Ok((Err(super::retention::MaintenanceRefusal::Stale),false)),
                super::retention::AuthorityState::Initializing=>return Ok((Err(super::retention::MaintenanceRefusal::Initializing),false)),
                super::retention::AuthorityState::Ready=>{},
                super::retention::AuthorityState::UnsupportedWriterBoundary=>return Ok((Err(super::retention::MaintenanceRefusal::UnsupportedWriterBoundary),false)),
            }
            let Some(keep)=authority.policy.keep_closed_ms else { return Ok((Ok(0),false)) };
            let before=before.min(crate::util::now_ms().saturating_sub(keep));
            // Bound both examined sessions and deleted rows. A fixed indexed
            // horizon prevents new arrivals from starving older obligations;
            // empty/ineligible histories cannot cause a whole-record-table scan.
            let (after,horizon,again):(Option<String>,Option<String>,bool)=s.conn.query_row("SELECT prune_after,prune_horizon,prune_again FROM conversation_maintenance WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            let after=after.map(|v|serde_json::from_str::<(i64,String)>(&v)).transpose()?;
            let mut horizon=horizon.map(|v|serde_json::from_str::<(i64,String)>(&v)).transpose()?;
            if horizon.is_none() {
                horizon=match s.conn.query_row("SELECT observed_ms,session FROM convo_retention WHERE observed_ms<?1 ORDER BY observed_ms DESC,session DESC LIMIT 1",vec![int(before)],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))) {
                    Ok(row)=>Some(row),Err(amphora_api::sql::Error::QueryReturnedNoRows)=>None,Err(error)=>return Err(error.into()),
                };
            }
            let Some(end)=horizon else {
                s.conn.execute("UPDATE conversation_maintenance SET prune_again=0 WHERE id=1",[])?;
                return Ok((Ok(0),false));
            };
            let mut q=if after.is_some() {
                s.conn.prepare("SELECT observed_ms,session FROM convo_retention WHERE (observed_ms,session)>(?1,?2) AND (observed_ms,session)<=(?3,?4) AND observed_ms<?5 ORDER BY observed_ms,session LIMIT 32")?
            }else{
                s.conn.prepare("SELECT observed_ms,session FROM convo_retention WHERE (observed_ms,session)<=(?1,?2) AND observed_ms<?3 ORDER BY observed_ms,session LIMIT 32")?
            };
            let read=|r:&crate::sql::Row<'_>|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?));
            let sessions=if let Some((at,id))=&after {
                q.query_map(vec![int(*at),txt(id),int(end.0),txt(&end.1),int(before)],read)?.collect::<amphora_api::sql::Result<Vec<_>>>()?
            }else{q.query_map(vec![int(end.0),txt(&end.1),int(before)],read)?.collect::<amphora_api::sql::Result<Vec<_>>>()?};
            let (mut cursor,mut gone)=(after,0usize);
            // 1800*2 deletes plus at most32 eligibility/record reads and cursor
            // work leave protocol/finalization room below COMMAND_MAX=4096.
            let limit=limit.min(1800);
            for (at,session) in &sessions {
                let positive:bool=s.conn.query_row("SELECT EXISTS(SELECT 1 FROM convo_sessions s JOIN convo_retention v ON v.session=s.id JOIN transcript_archives a ON a.session=s.id LEFT JOIN transcript_manifests m ON m.capture=a.manifest
                    WHERE s.id=?1 AND s.closed_at IS NOT NULL AND v.close_rev IS NOT NULL AND v.close_epoch=s.epoch AND v.close_at=s.closed_at AND v.observed_ms<?2
                    AND a.close_rev=v.close_rev AND a.accepted_rev=v.accepted_rev AND a.owner_generation=?3 AND a.policy_version=?4
                    AND (a.status='given-up' OR (a.status='archived' AND m.state='complete' AND m.catalog_complete=1 AND m.session=s.id AND m.close_rev=v.close_rev AND m.close_epoch=v.close_epoch AND m.accepted_rev=v.accepted_rev AND m.owner_generation=?3 AND m.policy_version=?4 AND m.history_rev=v.history_rev)))",
                    vec![txt(session),int(before),int(authority.generation),int(authority.policy_version)],|r|r.get(0))?;
                if positive {
                    let left=limit.checked_sub(gone).ok_or_else(||anyhow::anyhow!("prune budget underflow"))?;
                    if left==0 {break;}
                    let mut records=s.conn.prepare("SELECT source,n FROM convo_records WHERE session=?1 ORDER BY source,n LIMIT ?2")?;
                    let rows=records.query_map(vec![txt(session),seq(u64::try_from(left)?)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?.collect::<amphora_api::sql::Result<Vec<_>>>()?;
                    for (source,n) in &rows {
                        let params=vec![txt(session),txt(source),int(*n)];
                        s.conn.execute("DELETE FROM convo_segments WHERE session=?1 AND source=?2 AND n=?3",params.clone())?;
                        s.conn.execute("DELETE FROM convo_records WHERE session=?1 AND source=?2 AND n=?3",params)?;
                    }
                    gone=gone.checked_add(rows.len()).ok_or_else(||anyhow::anyhow!("prune count overflow"))?;
                    if rows.len()==left {break;}
                }
                cursor=Some((*at,session.clone()));
            }
            let (cursor,horizon)=if sessions.is_empty() {(None,None)}else{(cursor,Some(end))};
            let encode=|v:Option<(i64,String)>|v.map(|v|serde_json::to_string(&v)).transpose();
            s.conn.execute("UPDATE conversation_maintenance SET prune_after=?1,prune_horizon=?2,prune_again=CASE WHEN ?3 THEN 0 ELSE prune_again END WHERE id=1",vec![text(encode(cursor)?.as_deref()),text(encode(horizon)?.as_deref()),int(i64::from(sessions.is_empty()))])?;
            // A positive receipt arriving behind this finite cursor schedules
            // one further pass; it neither resets ongoing progress nor grants
            // pruning authority. Every deletion rechecks the witness above.
            Ok((Ok(gone),!sessions.is_empty() || again))
        })?;
        self.prune_pending=outcome.1;
        outcome.0.map_err(Into::into)
    }
    // U15: application code never checkpoints, copies or swaps the shared host database.
    fn compact_some(self: Box<Self>, _: u64, _: usize) -> anyhow::Result<(Box<dyn Engine>,Compacting)> { Ok((self,Compacting::Not)) }
    fn look(&mut self, count: bool) -> anyhow::Result<Look> {
        self.call(move |s| Ok(Look { top:s.rev, sessions:if count { s.one_int(sql::COUNT_SESSIONS,&[])?.map(u64::try_from).transpose()? } else { None }, ..Look::default() }))
    }
}

impl Amphora {
    fn merge_page(&mut self, page:LegacyPage)->anyhow::Result<()> {
        let LegacyPage {source,fingerprint,table,expected,next,done,rows}=page;
        let collision=self.call(move |s| {
            // The savepoint makes a typed collision an ordinary callback answer only AFTER
            // every projection/history write and cursor in this page has rolled back.
            let transaction=s.conn.unchecked_transaction()?;
            let (result,rev)={
                let mut merge=Session {conn:&transaction,rev:s.rev};
                let result=(||->anyhow::Result<()> {
                    merge.import_fence(&source,&fingerprint,table,expected)?;
                    let mut mutations=super::retention::Mutations::default();
                    for row in rows {merge.merge_legacy(&source,table,row,&mut mutations)?;}
                    mutations.flush(merge.conn)?;
                    merge.conn.execute("INSERT INTO convo_import(source,table_name,cursor,done,fingerprint) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(source,table_name) DO UPDATE SET cursor=excluded.cursor,done=excluded.done",vec![txt(&source),txt(table),int(next),int(i64::from(done)),txt(&fingerprint)])?;
                    Ok(())
                })();
                (result,merge.rev)
            };
            match result {
                Ok(())=>{transaction.commit()?;s.rev=rev;Ok(None)},
                Err(error)=>{
                    transaction.rollback()?;
                    match error.downcast::<ImportCollision>() {Ok(collision)=>Ok(Some(collision)),Err(error)=>Err(error)}
                },
            }
        })?;
        if let Some(collision)=collision {return Err(collision.into());}
        Ok(())
    }
    fn validate_source(&mut self,source:&str,fingerprint:&str)->anyhow::Result<()> {
        for table in ["events","records"] {
            let ledger=format!("__validate_{table}__");
            loop {
                let (source,fingerprint,ledger)=(source.to_owned(),fingerprint.to_owned(),ledger.clone());
                let result=self.call(move |s| {
                    let previous=s.conn.query_row("SELECT cursor,done FROM convo_import WHERE source=?1 AND table_name=?2",vec![txt(&source),txt(&ledger)],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)));
                    let (after,done)=match previous {Ok(v)=>v,Err(amphora_api::sql::Error::QueryReturnedNoRows)=>(i64::MIN,0),Err(e)=>return Err(e.into())};
                    if done==1 {return Ok(Ok(true));}
                    s.import_fence(&source,&fingerprint,&ledger,after)?;
                    let (next,done)=match s.validate_legacy_resets(&source,table,after) {
                        Ok(value)=>value,
                        Err(error)=>return match error.downcast::<ImportCollision>() {Ok(collision)=>Ok(Err(collision)),Err(error)=>Err(error)},
                    };
                    s.conn.execute("INSERT INTO convo_import(source,table_name,cursor,done,fingerprint) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(source,table_name) DO UPDATE SET cursor=excluded.cursor,done=excluded.done",vec![txt(&source),txt(&ledger),int(next),int(i64::from(done)),txt(&fingerprint)])?;
                    Ok(Ok(done))
                })?;
                if result.map_err(anyhow::Error::new)? {break;}
            }
        }
        Ok(())
    }
    /// U06/U15/U19: adopt independent snapshots before exposing the writer. Source rowids,
    /// revisions, metadata and retired feed state remain source-qualified history; canonical
    /// rowids/revisions belong to this host. Equal event/transcript content deduplicates, and
    /// disjoint keys insert. Session epochs fence mutable snapshots; ambiguous content or
    /// reset overlaps return a typed per-key ImportCollision instead of overwriting history.
    /// Each bounded page and its expected cursor commit together. Sources stay read-only.
    pub fn import_legacy(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        self.adopt_legacy(None,path)
    }
    /// U19: production source identity is the authenticated origin plus its canonical path.
    /// The pinned fingerprint remains separate so replacement at the same identity refuses.
    pub fn import_legacy_from(&mut self, host:&str, path:&std::path::Path)->anyhow::Result<()> {
        anyhow::ensure!(!host.is_empty() && host.len()<=1024,"legacy source requires an authenticated host name of 1 to 1024 bytes");
        self.adopt_legacy(Some(host),path)
    }
    fn adopt_legacy(&mut self, host:Option<&str>, path:&std::path::Path)->anyhow::Result<()> {
        if !path.exists() {
            let incomplete=self.database.call(until(),|c|match c.query_row("SELECT source FROM convo_import WHERE table_name='__source__' AND done<>1 LIMIT 1",[],|r|r.get::<_,String>(0)) {
                Ok(source)=>Ok(Some(source)),Err(amphora_api::sql::Error::QueryReturnedNoRows)=>Ok(None),Err(e)=>Err(e.into()),
            })?;
            if let Some(source)=incomplete {anyhow::bail!("legacy conversation source is missing at {}; shared timeline import is incomplete: {source}",path.display());}
            return Ok(());
        }
        use crate::records::legacy::Legacy;
        let canonical_path=path.canonicalize()?;
        let canonical=canonical_path.to_str().ok_or_else(||anyhow::anyhow!("canonical legacy conversation path is not UTF-8"))?;
        anyhow::ensure!(canonical.len()<=32768,"legacy conversation path exceeds bounded source identity size");
        let source=match host {
            Some(host)=>serde_json::to_string(&(1u8,host,canonical))?,
            // Unbound receipts from the singleton adapter keep their explicit old identity.
            // Replicated callers never guess an owner or reuse this cursor for a host key.
            None=>canonical.to_owned(),
        };
        anyhow::ensure!(source.len()<=256<<10,"legacy conversation source identity exceeds bound");
        // Resolve the identity before opening its snapshot: a retargeted caller symlink
        // must not bind an already-open input to a different canonical path.
        let old=Legacy::open(&canonical_path)?;
        let fingerprint=old.fingerprint(LEGACY_TABLES)?;
        let source0=source.clone();
        let fingerprint0=fingerprint.clone();
        let done=self.database.call(until(),move |c| {
            let known=c.query_row("SELECT fingerprint,done FROM convo_import WHERE source=?1 AND table_name='__source__'",vec![txt(&source0)],|r| Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)));
            let done=match known {
                Ok((was,done))=> {
                    anyhow::ensure!(was==fingerprint0,"legacy conversation source changed since import began");
                    done==1
                },
                Err(amphora_api::sql::Error::QueryReturnedNoRows)=> {
                    c.execute("INSERT INTO convo_import(source,table_name,cursor,done,fingerprint) VALUES (?1,'__source__',?2,0,?3)",vec![txt(&source0),int(i64::MIN),txt(&fingerprint0)])?;
                    false
                }
                Err(e)=>return Err(e.into()),
            };
            let history=c.query_row("SELECT done FROM convo_import WHERE source=?1 AND table_name='__history_v1__'",vec![txt(&source0)],|r|r.get::<_,i64>(0));
            match history {
                Ok(1)=>Ok(done),
                Ok(_)=>anyhow::bail!("legacy history format marker is invalid"),
                Err(amphora_api::sql::Error::QueryReturnedNoRows)=>{
                    // An import completed by the earlier single-source adapter needs its
                    // provenance backfilled. Re-read the SAME fenced snapshot; projection
                    // equality makes this restart safe and never removes canonical rows.
                    c.execute("DELETE FROM convo_import WHERE source=?1 AND table_name<>'__source__'",vec![txt(&source0)])?;
                    c.execute("UPDATE convo_import SET done=0 WHERE source=?1 AND table_name='__source__'",vec![txt(&source0)])?;
                    c.execute("INSERT INTO convo_import(source,table_name,cursor,done,fingerprint) VALUES (?1,'__history_v1__',0,1,?2)",vec![txt(&source0),txt(&fingerprint0)])?;
                    Ok(false)
                },
                Err(e)=>Err(e.into()),
            }
        })?;
        if done { return Ok(()); }
        for &table in LEGACY_TABLES {
            if !old.has(table)? { continue; }
            let (source0,table0)=(source.clone(),table.to_owned());
            let (mut cursor,done)=self.database.call(until(),move |c| {
                match c.query_row("SELECT cursor,done FROM convo_import WHERE source=?1 AND table_name=?2",vec![txt(&source0),txt(&table0)],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))) {
                    Ok((cursor,done))=>Ok((cursor,done!=0)),
                    Err(amphora_api::sql::Error::QueryReturnedNoRows)=>Ok((i64::MIN,false)),
                    Err(e)=>Err(e.into()),
                }
            })?;
            if done { continue; }
            loop {
                let page=self.prepare_legacy(&old,table,old.page(table,cursor)?)?;
                let empty=page.is_empty();
                let next=page.last().map_or(cursor,|row|row.id);
                self.merge_page(LegacyPage {source:source.clone(),fingerprint:fingerprint.clone(),table,expected:cursor,next,done:empty,rows:page})?;
                cursor=next;
                if empty { break; }
            }
        }
        self.validate_source(&source,&fingerprint)?;
        let (source0,fingerprint0)=(source,fingerprint);
        self.call(move |s| {
            let identity=s.conn.query_row("SELECT fingerprint FROM convo_import WHERE source=?1 AND table_name='__source__'",vec![txt(&source0)],|r|r.get::<_,String>(0))?;
            anyhow::ensure!(identity==fingerprint0,"conversation import source identity changed before publication");
            let incomplete=s.conn.query_row("SELECT count(*) FROM convo_import WHERE source=?1 AND done<>1 AND table_name<>'__source__'",vec![txt(&source0)],|r|r.get::<_,usize>(0))?;
            anyhow::ensure!(incomplete==0,"conversation import has incomplete table cursors");
            for q in sql::REV_MAX { s.rev=s.rev.max(s.one_int(q,&[])?.and_then(|n|u64::try_from(n).ok()).unwrap_or(0)); }
            s.conn.execute("UPDATE convo_import SET done=1 WHERE source=?1 AND table_name='__source__'",vec![txt(&source0)])?;
            Ok(())
        })?;
        Ok(())
    }
    /// U19: a writer is exposed only after every registered source has completed, including
    /// restarted imports that were registered by an earlier constructor in this host.
    pub fn finish_imports(&mut self)->anyhow::Result<()> {
        self.call(|s|match s.conn.query_row("SELECT source FROM convo_import WHERE table_name='__source__' AND done<>1 LIMIT 1",[],|r|r.get::<_,String>(0)) {
            Ok(source)=>anyhow::bail!("legacy conversation import is incomplete: {source}"),
            Err(amphora_api::sql::Error::QueryReturnedNoRows)=>Ok(()),
            Err(error)=>Err(error.into()),
        })
    }
}
impl Session<'_, '_> {
    fn import_fence(&self,source:&str,fingerprint:&str,table:&str,expected:i64)->anyhow::Result<()> {
        let (identity,done)=self.conn.query_row("SELECT fingerprint,done FROM convo_import WHERE source=?1 AND table_name='__source__'",vec![txt(source)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?;
        anyhow::ensure!(identity==fingerprint && done==0,"conversation import source changed or was already published; retry initialization");
        let position=self.conn.query_row("SELECT cursor,done,fingerprint FROM convo_import WHERE source=?1 AND table_name=?2",vec![txt(source),txt(table)],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?)));
        match position {
            Ok((cursor,done,identity))=>anyhow::ensure!(cursor==expected && done==0 && identity==fingerprint,"conversation import cursor advanced concurrently; retry initialization"),
            Err(amphora_api::sql::Error::QueryReturnedNoRows)=>anyhow::ensure!(expected==i64::MIN,"conversation import cursor disappeared"),
            Err(e)=>return Err(e.into()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::rows::RecordRow;
    fn transcript(n:u64,line:String)->Row { Row::Record(Box::new(RecordRow {session:"s".into(),source:"rollout".into(),n,end:n.saturating_add(100),line,kept:None})) }
    #[test]
    fn u05_u06_u15_keysets_preserve_ties_limits_and_early_stop() -> anyhow::Result<()> {
        let database=Database::in_memory(until())?;
        let mut engine=Amphora::from_database(database)?;
        engine.call(|s| {
            for n in 0..19 {
                s.conn.execute("INSERT INTO convo_events(session,id,seq,at,kind,text,body) VALUES ('s',?1,?2,?2,'user','match','{}')",vec![txt(&format!("e{n}")),int(n/3)])?;
                s.conn.execute("INSERT INTO convo_records(session,source,n,line_end,line,rev) VALUES ('s','a',?1,0,'line',0)",vec![int(n)])?;
            }
            for id in ["z","a","q"] {
                s.conn.execute("INSERT INTO convo_sessions(id,info,updated_at,closed_at) VALUES (?1,'{}',7,NULL)",vec![txt(id)])?;
            }
            let cases=[
                (sql::EVENTS_LAST,vec![txt("s"),int(17)]),
                (sql::EVENTS_LAST,vec![txt("s"),int(0)]),
                (sql::EVENTS_LAST,vec![txt("s"),int(-1)]),
                (sql::EVENTS_LAST,vec![txt("s"),int(32769)]),
                (sql::EVENTS_BEFORE,vec![txt("s"),int(5),int(8)]),
                (sql::SEARCH_ALL,vec![int(9)]),
                (sql::SEARCH_SESSION,vec![txt("s"),int(5)]),
                (sql::RECORD_LINES,vec![txt("s"),txt("a"),int(2),int(17)]),
                (sql::RECORD_SOURCES,vec![txt("s")]),
                (sql::SESSIONS_UNENDED,vec![int(0)]),
            ];
            for (query,params) in cases {
                let oracle=if query==sql::SESSIONS_UNENDED {format!("{query}, id")}else{query.to_owned()};
                let mut original=s.conn.prepare(&oracle)?;
                let columns=original.column_count();
                let mut rows=original.query(params.clone())?;
                let mut expected=Vec::new();
                while let Some(row)=rows.next()? {
                    expected.push((0..columns).map(|i|row.get::<_,Value>(i)).collect::<amphora_api::sql::Result<Vec<_>>>()?);
                }
                let mut statement=s.prepare(query)?;
                let mut actual=Vec::new();
                s.run(&mut statement,&params,|r|{actual.push(r.0.clone());Ok(())})?;
                assert_eq!(actual,expected,"{query}");
                let mut prefix=Vec::new();
                s.run_while(&mut statement,&params,|r|{prefix.push(r.0.clone());Ok(prefix.len()<9)})?;
                assert_eq!(prefix,expected.into_iter().take(9).collect::<Vec<_>>(),"{query}");
            }
            // Resume inside equal keys: the external search cursor seeks immediately.
            let mut search=s.prepare(sql::SEARCH_SESSION)?;
            let mut resumed=Vec::new();
            s.run_from(&mut search,&[txt("s"),int(5)],Some(vec![int(5),int(17)]),Some(3),|r| {
                resumed.push(r.get::<i64>(0)?);Ok(resumed.len()<3)
            })?;
            assert_eq!(resumed,vec![16,15,14]);
            s.conn.execute("UPDATE convo_sessions SET closed_at=7",[])?;
            let mut ended=s.prepare(sql::SESSIONS_ENDED_AFTER)?;
            let mut ids=Vec::new();
            s.run(&mut ended,&[int(0),txt(""),int(2)],|r|{ids.push(r.get::<String>(0)?);Ok(())})?;
            assert_eq!(ids,vec!["a","q"]);
            Ok(())
        })
    }
    #[test]
    fn u05_u06_u15_native_equal_key_probe_seeks_full_physical_suffix() -> anyhow::Result<()> {
        let database=Database::in_memory(until())?;
        let mut engine=Amphora::from_database(database)?;
        engine.call(|s| {
            for (query,params,width) in [
                (sql::SEARCH_SESSION,vec![txt("s"),int(7),int(300),int(256)],3usize),
                (sql::SEARCH_ALL,vec![int(7),int(300),int(256)],2usize),
            ] {
                let plan=sql::event_page(query).ok_or_else(||anyhow::anyhow!("missing event plan"))?;
                let mut explain=s.conn.prepare(&format!("EXPLAIN {}",plan.equal))?;
                let mut rows=explain.query(params)?;
                let (mut full_seek,mut backwards)=(false,false);
                while let Some(r)=rows.next()? {
                    let opcode=r.get::<_,String>(1)?;
                    if opcode=="Prev" {backwards=true;}
                    assert!(!opcode.starts_with("Sorter"),"key probe must remain covered and ordered");
                    if opcode=="SeekLT" {
                        let comment=r.get::<_,String>(7)?;
                        if let Some(range)=comment.strip_prefix("key=[").and_then(|s|s.strip_suffix(']'))
                            && let Some((start,end))=range.split_once("..") {
                            let (start,end)=(start.parse::<usize>()?,end.parse::<usize>()?);
                            full_seek|=end.checked_sub(start).and_then(|n|n.checked_add(1))==Some(width);
                        }
                    }
                }
                assert!(full_seek && backwards,"native implicit rowid suffix optimization required: {}",plan.equal);
            }
            Ok(())
        })
    }
    #[test]
    fn u05_u06_u15_key_pages_cross_duplicate_groups_and_stop_payloads() -> anyhow::Result<()> {
        let database=Database::in_memory(until())?;
        let mut engine=Amphora::from_database(database)?;
        engine.call(|s| {
            // More than two key pages in one tie group, plus a lower-key tail.
            for n in 0..610 {
                s.conn.execute("INSERT INTO convo_events(session,id,seq,at,kind,text,body) VALUES ('s',?1,?2,?2,'user','match','{}')",vec![txt(&format!("e{n}")),int(if n<10 {6}else{7})])?;
            }
            for query in [sql::SEARCH_SESSION,sql::SEARCH_ALL] {
                let params=if query==sql::SEARCH_SESSION {vec![txt("s"),int(7)]}else{vec![int(7)]};
                let mut stmt=s.prepare(query)?;
                let mut all=Vec::new();
                s.run(&mut stmt,&params,|r|{all.push(r.get::<i64>(0)?);Ok(())})?;
                assert_eq!(all,(1..=610).rev().collect::<Vec<i64>>());
                let mut resumed=Vec::new();
                s.run_from(&mut stmt,&params,Some(vec![int(7),int(500)]),Some(500),|r| {
                    resumed.push(r.get::<i64>(0)?);Ok(resumed.len()<257)
                })?;
                assert_eq!(resumed,(243..500).rev().collect::<Vec<i64>>());
            }
            Ok(())
        })
    }
    #[test]
    fn u05_u06_u15_native_inventory_and_payload_use_scalar_rowid_search() -> anyhow::Result<()> {
        let database=Database::in_memory(until())?;
        let mut engine=Amphora::from_database(database)?;
        engine.call(|s| {
            for (query,params) in [
                (sql::SESSIONS_UNENDED,vec![int(0)]),
                (sql::SESSIONS_ENDED_AFTER,vec![int(0),txt(""),int(2)]),
            ] {
                let plan=sql::inventory(query).ok_or_else(||anyhow::anyhow!("missing inventory plan"))?;
                let mut bound=params;
                bound.push(int(0));
                for (sql,params) in [(plan.scan,bound),(plan.payload.to_owned(),vec![int(1)])] {
                    let mut explain=s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let mut rows=explain.query(params)?;
                    let mut details=Vec::new();
                    while let Some(r)=rows.next()? {details.push(r.get::<_,String>(3)?);}
                    let detail=details.join(" ").to_ascii_lowercase();
                    assert!(detail.contains("search") && (detail.contains("rowid") || detail.contains("integer primary key")),"{sql}: {details:?}");
                    assert!(!detail.contains("temp b-tree"),"{sql}: {details:?}");
                }
            }
            Ok(())
        })
    }
    #[test]
    fn u05_u06_u15_event_iteration_bounds_owned_large_payloads() -> anyhow::Result<()> {
        let database=Database::in_memory(until())?;
        let mut engine=Amphora::from_database(database)?;
        // Two individually valid payloads exceed the 32 MiB materialized result cap.
        // The application must retain both without querying them in one owned Rows.
        let body="x".repeat(17<<20);
        for n in 0..2 {
            engine.apply(&[Row::Event(Box::new(EventRow {session:"s".into(),id:format!("e{n}"),seq:n,at:1,parent:None,by:None,kind:"user".into(),level:None,tool:None,status:None,text:String::new(),body:body.clone()}))])?;
        }
        let events=engine.events("s",None,2)?;
        assert_eq!(events.len(),2);
        for event in events {assert_eq!(event.body,body);}
        Ok(())
    }
    #[test]
    fn u06_u15_canonical_sql_preserves_literals_comments_and_quoted_identifiers() -> anyhow::Result<()> {
        let database=Database::in_memory(until())?;
        database.call(until(),|c| {
            c.execute_batch("CREATE TABLE events(id TEXT PRIMARY KEY); INSERT INTO events VALUES ('main-event'); CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT); INSERT INTO meta VALUES ('owner','main-store');")?;
            Ok(())
        })?;
        let mut engine=Amphora::from_database(database.clone())?;
        engine.apply(&[Row::Session {session:"s".into(),info:Some("{}".into()),meta:Some("sessions events records".into()),state:None,changes:None,epoch:Some(1),open:true,at:1}])?;
        // Exercise the qualified meta column and excluded.meta in the upsert too.
        engine.apply(&[Row::Session {session:"s".into(),info:None,meta:None,state:Some("{}".into()),changes:None,epoch:Some(1),open:false,at:2}])?;
        let event=EventRow {session:"s".into(),id:"e".into(),seq:0,at:2,parent:None,by:None,kind:"user".into(),level:None,tool:None,status:None,text:"records".into(),body:"{}".into()};
        engine.apply(&[Row::Event(Box::new(event.clone()))])?;
        assert_eq!(engine.events("s",None,10)?,vec![event]);
        assert_eq!(engine.look(true)?.sessions,Some(1));
        const QUERY:&str=r#"SELECT 'events', 'it''s records', 'FROM meta ', "sessions""suffix"."meta" AS [records old]
            FROM "convo_sessions" AS "sessions""suffix" -- sessions events
            /* FROM meta records */ WHERE "sessions""suffix"."id"=?1"#;
        let values=engine.call(|s| {
            let mut statement=s.prepare(QUERY)?;
            assert_eq!(statement.sql,QUERY,"fixed SQL reaches the product parser byte-for-byte");
            let mut values=Vec::new();
            s.run(&mut statement,&[txt("s")],|row| {
                for column in 0..4 {values.push(row.get::<String>(column)?);}
                Ok(())
            })?;
            Ok(values)
        })?;
        assert_eq!(values,vec!["events","it's records","FROM meta ","sessions events records"]);
        let main=database.call(until(),|c|Ok((c.query_row("SELECT id FROM events",[],|r|r.get::<_,String>(0))?,c.query_row("SELECT value FROM meta WHERE key='owner'",[],|r|r.get::<_,String>(0))?)))?;
        assert_eq!(main,("main-event".to_owned(),"main-store".to_owned()));
        Ok(())
    }
    #[test]
    fn u06_u15_segmented_transcript_reopens_with_exact_utf8_and_shared_records() -> anyhow::Result<()> {
        use amphora_api::Records as _;
        let dir=tempfile::tempdir()?;
        let path=dir.path().join("host.amphora");
        // A UTF-8 scalar straddles segment boundaries; segmentation is byte-oriented and joins
        // before decoding. This exceeds the product's per-record 1 MiB bound.
        let line="界x".repeat((1<<20)/2);
        {
            let database=Database::open(&path,until())?;
            let mut engine=Amphora::from_database(database.clone())?;
            let ns=database.namespace("host")?;
            ns.commit(amphora_api::Tx {conds:vec![],writes:vec![amphora_api::Write::Put {family:"inbox".into(),key:b"k".to_vec(),value:b"v".to_vec()}]},until())?;
            engine.apply(&[transcript(0,line.clone())])?;
            let rev=engine.top();
            engine.apply(&[transcript(0,line.clone())])?;
            assert_eq!(engine.top(),rev);
            assert_eq!(engine.record("s","rollout",0,10)?,vec![(0,line.clone())]);
            let largest=database.call(until(),|c| Ok(c.query_row("SELECT max(length(value)) FROM convo_chunks",[],|r|r.get::<_,i64>(0))?))?;
            assert!(largest<=i64::try_from(SEGMENT)?);
        }
        let database=Database::open(&path,until())?;
        let mut engine=Amphora::from_database(database.clone())?;
        assert_eq!(engine.marks("s")?.first().map(|m|m.line.as_str()),Some(line.as_str()));
        assert_eq!(database.namespace("host")?.get("inbox",b"k",amphora_api::Consistency::Linearizable,until())?.map(|r|r.value),Some(b"v".to_vec()));
        assert!(engine.changes(0,1024,false).is_err());
        Ok(())
    }
    #[test]
    fn u06_u15_transcript_above_64m_stages_before_atomic_publish_and_reopens() -> anyhow::Result<()> {
        let dir=tempfile::tempdir()?;
        let path=dir.path().join("amphora.db");
        // Above both the SQL request limit and the former 64 MiB reconstruction cap. A
        // three-byte scalar crosses 512 KiB part boundaries, with exact bytes retained.
        let line="界".repeat((64<<20)/3+1);
        {
            let database=Database::open(&path,until())?;
            let mut engine=Amphora::from_database(database.clone())?;
            let rows=engine.prepare_rows(&[transcript(0,line.clone()),transcript(1,"after large line".to_owned())])?;
            assert!(rows.iter().try_fold(0usize,|n,r|n.checked_add(row_size(r)?).ok_or_else(||anyhow::anyhow!("fixture size overflow")))?<4096);
            assert_eq!(engine.top(),0);
            assert!(engine.record("s","rollout",0,10)?.is_empty(),"staging exposes no transcript");
            let aborted=rows.clone();
            let failed:anyhow::Result<()>=engine.call(move |s|{s.apply(&aborted)?;anyhow::bail!("modeled interruption before manifest commit")});
            assert!(failed.is_err());
            assert!(engine.record("s","rollout",0,10)?.is_empty());
            assert_eq!(engine.top(),0);
            engine.call(move |s|s.apply(&rows))?;
            assert_eq!(engine.top(),2);
            let ready=database.call(until(),|c|Ok(c.query_row("SELECT count(*) FROM convo_chunk_manifests WHERE ready=1",[],|r|r.get::<_,usize>(0))?))?;
            assert_eq!(ready,1);
        }
        let database=Database::open(&path,until())?;
        let mut engine=Amphora::from_database(database)?;
        // Short nonempty pages are byte bounds, not EOF; the following line is retained.
        let page=engine.record("s","rollout",0,10)?;
        assert_eq!(page.len(),1);
        assert_eq!(page.first().map(|(_,s)|s.as_str()),Some(line.as_str()));
        assert_eq!(engine.record("s","rollout",1,10)?,vec![(1,"after large line".to_owned())]);
        assert!(engine.record("s","rollout",2,10)?.is_empty());
        Ok(())
    }
    #[test]
    fn u06_u15_conversation_import_resumes_atomic_cursor_and_preserves_revisions() -> anyhow::Result<()> {
        use crate::records::legacy::Legacy;
        let dir=tempfile::tempdir()?;
        let legacy=dir.path().join("conversations.db");
        let line="a".repeat((32<<20)+17);
        let attachment_event=EventRow { session:"s".into(),id:"e".into(),seq:0,at:123,parent:None,by:None,kind:"user".into(),level:None,tool:None,status:None,text:"attached image".into(),body:r#"{"type":"user","attachments":[{"name":"image.png","data":"YWJj"}]}"#.into() };
        let top={
            let mut old=super::super::turso::Turso::open(&legacy)?;
            old.apply(&[transcript(0,line.clone()),transcript(1,"next".into()),Row::Event(Box::new(attachment_event.clone()))])?;
            old.take("old-runner",None,0,&Changes {rows:vec![],upto:7})?;
            old.top()
        };
        let old=Legacy::open(&legacy)?;
        let source=legacy.canonicalize()?.to_string_lossy().into_owned();
        let identity=old.fingerprint(LEGACY_TABLES)?;
        let first=old.page("records",i64::MIN)?.into_iter().next().ok_or_else(||anyhow::anyhow!("fixture has no transcript"))?;
        let path=dir.path().join("host.amphora");
        {
            let database=Database::open(&path,until())?;
            let mut engine=Amphora::from_database(database)?;
            // Model a stop immediately after one page and its cursor committed.
            let page=engine.prepare_legacy(&old,"records",vec![first])?;
            let (stale_source,stale_identity)=(source.clone(),identity.clone());
            engine.call(move |s| {
                let row=page.into_iter().next().ok_or_else(||anyhow::anyhow!("missing prepared page"))?;
                let id=row.id;
                let mut mutations=super::super::retention::Mutations::default();
                s.merge_legacy(&source,"records",row,&mut mutations)?;
                mutations.flush(s.conn)?;
                for (table,cursor) in [("__source__",i64::MIN),("records",id)] {
                    s.conn.execute("INSERT INTO convo_import(source,table_name,cursor,done,fingerprint) VALUES (?1,?2,?3,0,?4)",vec![txt(&source),txt(table),int(cursor),txt(&identity)])?;
                }
                Ok(())
            })?;
            // A second initializer still holding the initial cursor must fail atomically;
            // it cannot overwrite that committed page or move its cursor backwards.
            assert!(engine.call(move |s|s.import_fence(&stale_source,&stale_identity,"records",i64::MIN)).is_err());
        }
        drop(old);
        let database=Database::open(&path,until())?;
        let mut engine=Amphora::from_database(database.clone())?;
        let displaced=dir.path().join("retained-conversations.db");
        std::fs::rename(&legacy,&displaced)?;
        assert!(engine.import_legacy(&legacy).is_err(),"missing interrupted source must not publish a partial import");
        assert!(super::super::from_database(database.clone(),&legacy,super::super::store::Upkeep::default()).is_err(),"startup must not expose a writer over the incomplete import");
        std::fs::rename(&displaced,&legacy)?;
        engine.import_legacy(&legacy)?;
        assert!(engine.top()>=top);
        let imported_top=engine.top();
        assert_eq!(engine.events("s",None,10)?,vec![attachment_event]);
        assert_eq!(database.call(until(),|c|Ok(c.query_row("SELECT source_rev FROM convo_legacy_rows WHERE table_name='peers' AND item='old-runner'",[],|r|r.get::<_,i64>(0))?))?,7);
        assert_eq!(engine.record("s","rollout",0,10)?,vec![(0,line)]);
        assert_eq!(engine.record("s","rollout",1,10)?,vec![(1,"next".to_owned())]);
        engine.import_legacy(&legacy)?;
        assert_eq!(engine.top(),imported_top);
        engine.apply(&[transcript(2,"after import".into())])?;
        assert_eq!(engine.top(),imported_top.saturating_add(1));
        Ok(())
    }
}
