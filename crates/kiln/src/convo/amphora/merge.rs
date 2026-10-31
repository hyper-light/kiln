//! U06/U15: merge independent legacy stores without treating their local coordinates as
//! canonical rowids or feed revisions. Originals and projection publish with the page cursor.
use super::*;

pub(super) struct PreparedLegacy {
    pub id: i64,
    pub values: Vec<Value>,
    pub archived: Vec<u8>,
    pub markers: Vec<Vec<u8>>,
    pub epoch: u64,
}

/// A source remains intact when its row cannot be merged without changing existing content.
#[derive(Debug, Clone)]
pub struct ImportCollision {
    pub source: String,
    pub table: String,
    pub key: String,
    pub reason: String,
}
impl std::fmt::Display for ImportCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "legacy conversation collision: source={}, table={}, key={}: {}", self.source, self.table, self.key, self.reason)
    }
}
impl std::error::Error for ImportCollision {}

fn collision(source: &str, table: &str, key: &str, reason: impl Into<String>) -> anyhow::Error {
    ImportCollision { source: source.to_owned(), table: table.to_owned(), key: key.to_owned(), reason: reason.into() }.into()
}
fn value(values: &[Value], column: usize) -> anyhow::Result<&Value> {
    values.get(column).ok_or_else(|| anyhow::anyhow!("missing legacy column {column}"))
}
fn get<T: FromSql>(values: &[Value], column: usize) -> anyhow::Result<T> {
    Ok(T::from_sql(value(values, column)?)?)
}
fn optional_integer(values: &[Value], column: usize) -> anyhow::Result<Option<i64>> {
    match value(values, column)? { Value::Null => Ok(None), other => Ok(Some(i64::from_sql(other)?)) }
}
fn optional_text(values: &[Value], column: usize) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(value(values,column)?,Value::Null|Value::Text(_)),"legacy column {column} is not nullable text");
    Ok(())
}
fn required_text(values: &[Value], column: usize) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(value(values,column)?,Value::Text(_)),"legacy column {column} is not text");
    Ok(())
}
fn checked_revision(rev: u64) -> anyhow::Result<Value> { Ok(int(i64::try_from(rev)?)) }

impl Session<'_, '_> {
    fn merge_next_rev(&mut self) -> anyhow::Result<Value> {
        let rev=self.rev.checked_add(1).ok_or_else(||anyhow::anyhow!("conversation revision overflow"))?;
        let value=checked_revision(rev)?;
        self.rev=rev;
        Ok(value)
    }
    fn merge_existing(&self, query: &str, params: Vec<Value>, columns: usize) -> anyhow::Result<Option<Vec<Value>>> {
        match self.conn.query_row(query, params, |row| (0..columns).map(|i| row.get::<_,Value>(i)).collect::<amphora_api::sql::Result<Vec<_>>>()) {
            Ok(values)=>Ok(Some(values)),
            Err(amphora_api::sql::Error::QueryReturnedNoRows)=>Ok(None),
            Err(error)=>Err(error.into()),
        }
    }
    /// The caller's transaction fences source identity and cursor before calling this method.
    /// A collision rolls back this archive, every projection in the page, and its cursor.
    pub(super) fn merge_legacy(&mut self, source: &str, table: &str, row: PreparedLegacy, mutations: &mut super::super::retention::Mutations) -> anyhow::Result<()> {
        let PreparedLegacy {id,values,archived,markers,epoch}=row;
        for marker in markers {segments::validate(self.conn,&marker)?;}
        let (session,item,number,source_rev)=match table {
            "sessions"=>{anyhow::ensure!(matches!(values.len(),7|10),"unsupported legacy sessions schema");(value(&values,0)?.clone(),Value::Null,Value::Null,if values.len()==10 {value(&values,8)?.clone()}else{Value::Null})},
            "events"=>{anyhow::ensure!(matches!(values.len(),12|13),"unsupported legacy events schema");(value(&values,0)?.clone(),value(&values,1)?.clone(),value(&values,2)?.clone(),if values.len()==13 {value(&values,12)?.clone()}else{Value::Null})},
            "records"=>match values.len() {
                3=>(value(&values,0)?.clone(),txt("legacy"),value(&values,1)?.clone(),Value::Null),
                6=>(value(&values,0)?.clone(),value(&values,1)?.clone(),value(&values,2)?.clone(),value(&values,5)?.clone()),
                _=>anyhow::bail!("unsupported legacy records schema"),
            },
            "drops"=>{anyhow::ensure!(values.len()==6,"unsupported legacy drops schema");(value(&values,1)?.clone(),value(&values,2)?.clone(),value(&values,4)?.clone(),value(&values,0)?.clone())},
            "peers"=>{anyhow::ensure!(values.len()==3,"unsupported legacy peers schema");(Value::Null,value(&values,0)?.clone(),Value::Null,value(&values,1)?.clone())},
            "copied"=>{anyhow::ensure!(values.len()==1,"unsupported legacy copied schema");(value(&values,0)?.clone(),Value::Null,Value::Null,Value::Null)},
            "meta"=>{anyhow::ensure!(values.len()==2,"unsupported legacy meta schema");(Value::Null,value(&values,0)?.clone(),Value::Null,Value::Null)},
            _=>anyhow::bail!("unsupported legacy conversation table"),
        };
        if !matches!(source_rev,Value::Null) {let _=u64::from_sql(&source_rev)?;}
        let prior=self.merge_existing("SELECT payload,epoch FROM convo_legacy_rows WHERE source=?1 AND table_name=?2 AND source_rowid=?3",vec![txt(source),txt(table),int(id)],2)?;
        if let Some(prior)=prior {
            anyhow::ensure!(get::<Vec<u8>>(&prior,0)?==archived && get::<u64>(&prior,1)?==epoch,"legacy archived row changed within a fenced source");
        } else {
            self.conn.execute("INSERT INTO convo_legacy_rows(source,table_name,source_rowid,session,item,number,epoch,source_rev,payload) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",vec![txt(source),txt(table),int(id),session,item,number,checked_revision(epoch)?,source_rev,Value::Blob(archived)])?;
        }
        match table {
            "sessions"=>self.merge_session(source,values),
            "events"=>self.merge_event(source,values,epoch),
            "records"=>self.merge_record(source,values,epoch,mutations),
            "drops"=>{
                // Historical resets are source-local evidence, never destructive commands
                // against rows imported from an independent authority or revision space.
                self.conn.execute("INSERT INTO convo_legacy_resets(source,source_rowid,session,what,record_source,n,at,epoch) VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(source,source_rowid) DO NOTHING",vec![txt(source),int(id),value(&values,1)?.clone(),value(&values,2)?.clone(),value(&values,3)?.clone(),value(&values,4)?.clone(),value(&values,5)?.clone(),checked_revision(epoch)?])?;
                Ok(())
            },
            "meta"=>{
                // The host owns its schema ledger and identity. Source counters only raise
                // the host high-water mark, never rewind or replace another source's state.
                if get::<String>(&values,0)?=="rev" {self.rev=self.rev.max(get::<String>(&values,1)?.parse::<u64>()?);let _=checked_revision(self.rev)?;}
                Ok(())
            },
            "peers"|"copied"=>Ok(()),
            _=>anyhow::bail!("unsupported legacy conversation table"),
        }
    }
    fn merge_event(&mut self, source: &str, mut values: Vec<Value>, epoch: u64) -> anyhow::Result<()> {
        values.truncate(12);
        let (session,id)=(get::<String>(&values,0)?,get::<String>(&values,1)?);
        let n=get::<u64>(&values,2)?;
        let at=get::<i64>(&values,3)?;
        for column in [4,5,7,8,9] {optional_text(&values,column)?;}
        for column in [6,10,11] {required_text(&values,column)?;}
        let prior=self.merge_existing("SELECT session,id,seq,at,parent,by,kind,level,tool,status,text,body FROM convo_events WHERE session=?1 AND id=?2",vec![txt(&session),txt(&id)],12)?;
        if let Some(prior)=prior {
            if prior!=values {return Err(collision(source,"events",&format!("{session}/{id}"),"same event ID has different content"));}
            self.check_legacy_duplicate(super::reset::LegacyKey {source,table:"events",session:&session,item:&id,n,at:Some(at),epoch})?;
            return Ok(());
        }
        self.check_legacy_insert(super::reset::LegacyKey {source,table:"events",session:&session,item:&id,n,at:Some(at),epoch})?;
        values.push(self.merge_next_rev()?);
        self.conn.execute("INSERT INTO convo_events(session,id,seq,at,parent,by,kind,level,tool,status,text,body,rev) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",values)?;
        Ok(())
    }
    fn merge_line_identity(&self, session: &str, source: &str, n: u64, value: &Value) -> anyhow::Result<(u64,Vec<u8>)> {
        use sha2::{Digest as _,Sha256};
        if let Value::Blob(marker)=value && marker.first()==Some(&3) {
            segments::validate(self.conn,marker)?;
            let size=marker.get(1..9).and_then(|b|<[u8;8]>::try_from(b).ok()).map(u64::from_le_bytes).ok_or_else(||anyhow::anyhow!("invalid prepared transcript marker"))?;
            let digest=marker.get(9..41).ok_or_else(||anyhow::anyhow!("invalid prepared transcript digest"))?;
            return Ok((size,digest.to_vec()));
        }
        let line=self.line(session,source,n,value)?;
        Ok((u64::try_from(line.len())?,Sha256::digest(line.as_bytes()).to_vec()))
    }
    fn merge_record(&mut self, source: &str, values: Vec<Value>, epoch: u64, mutations: &mut super::super::retention::Mutations) -> anyhow::Result<()> {
        let values=if values.len()==3 {
            vec![value(&values,0)?.clone(),txt("legacy"),value(&values,1)?.clone(),int(0),value(&values,2)?.clone(),int(0)]
        } else {values};
        let (session,record_source,n,end)=(get::<String>(&values,0)?,get::<String>(&values,1)?,get::<u64>(&values,2)?,get::<u64>(&values,3)?);
        let line=value(&values,4)?;
        let prior=self.merge_existing("SELECT line_end,line FROM convo_records WHERE session=?1 AND source=?2 AND n=?3",vec![txt(&session),txt(&record_source),checked_revision(n)?],2)?;
        if let Some(prior)=prior {
            if self.merge_line_identity(&session,&record_source,n,value(&prior,1)?)?!=self.merge_line_identity(&session,&record_source,n,line)? {
                return Err(collision(source,"records",&format!("{session}/{record_source}/{n}"),"same transcript line has different content"));
            }
            self.check_legacy_duplicate(super::reset::LegacyKey {source,table:"records",session:&session,item:&record_source,n,at:None,epoch})?;
            if end>get::<u64>(&prior,0)? {
                let rev=self.merge_next_rev()?;
                let accepted=u64::from_sql(&rev)?;
                self.conn.execute("UPDATE convo_records SET line_end=?4,rev=?5 WHERE session=?1 AND source=?2 AND n=?3",vec![txt(&session),txt(&record_source),checked_revision(n)?,checked_revision(end)?,rev])?;
                mutations.record(&session,&record_source,n,accepted,false)?;
            }
            return Ok(());
        }
        self.check_legacy_insert(super::reset::LegacyKey {source,table:"records",session:&session,item:&record_source,n,at:None,epoch})?;
        let (line,kept)=match line {
            Value::Text(line)=>(line.clone(),None),
            Value::Blob(marker) if marker.first()==Some(&3)=>(String::new(),Some(marker.clone())),
            _=>anyhow::bail!("legacy transcript was not staged"),
        };
        let rec=super::super::rows::RecordRow {session,source:record_source,n,end,line,kept};
        let rev=u64::from_sql(&self.merge_next_rev()?)?;
        anyhow::ensure!(self.write_record(&rec,rev)?,"new legacy transcript did not insert");
        mutations.record(&rec.session,&rec.source,rec.n,rev,false)?;
        Ok(())
    }
    fn merge_session(&mut self, source: &str, mut incoming: Vec<Value>) -> anyhow::Result<()> {
        if incoming.len()==7 {incoming.extend([int(0),int(0),Value::Null]);}
        let id=get::<String>(&incoming,0)?;
        for column in [1,2,3,4,9] {optional_text(&incoming,column)?;}
        let _=get::<i64>(&incoming,5)?;
        let _=optional_integer(&incoming,6)?;
        let incoming_epoch=get::<u64>(&incoming,7)?;
        let prior=self.merge_existing("SELECT id,info,meta,state,changes,updated_at,closed_at,epoch,rev,machine FROM convo_sessions WHERE id=?1",vec![txt(&id)],10)?;
        let merged=if let Some(prior)=prior {
            let current_epoch=get::<u64>(&prior,7)?;
            let mut merged=if incoming_epoch>current_epoch {
                // The new writer's snapshot wins; descriptive identity can survive an
                // incomplete new snapshot, but state and machine never come from the old epoch.
                for column in [1,2] {if matches!(value(&incoming,column)?,Value::Null) {*incoming.get_mut(column).ok_or_else(||anyhow::anyhow!("missing legacy session column"))?=value(&prior,column)?.clone();}}
                incoming
            } else if incoming_epoch<current_epoch {
                let mut merged=prior.clone();
                for column in [1,2] {if matches!(value(&merged,column)?,Value::Null) {*merged.get_mut(column).ok_or_else(||anyhow::anyhow!("missing canonical session column"))?=value(&incoming,column)?.clone();}}
                merged
            } else {
                let mut merged=prior.clone();
                for (column,name) in [(1,"info"),(2,"meta"),(3,"state"),(4,"changes"),(9,"machine")] {
                    let (old,new)=(value(&prior,column)?,value(&incoming,column)?);
                    if !matches!(old,Value::Null) && !matches!(new,Value::Null) && old!=new {return Err(collision(source,"sessions",&id,format!("same epoch has incompatible {name}")));}
                    if matches!(old,Value::Null) {*merged.get_mut(column).ok_or_else(||anyhow::anyhow!("missing canonical session column"))?=new.clone();}
                }
                let (old_at,new_at)=(get::<i64>(&prior,5)?,get::<i64>(&incoming,5)?);
                let (old_close,new_close)=(optional_integer(&prior,6)?,optional_integer(&incoming,6)?);
                let open=match (old_close,new_close) {(None,None)=>Some(old_at.max(new_at)),(None,Some(_))=>Some(old_at),(Some(_),None)=>Some(new_at),(Some(_),Some(_))=>None};
                let close=old_close.into_iter().chain(new_close).max().filter(|close|open.is_none_or(|open|*close>=open));
                *merged.get_mut(5).ok_or_else(||anyhow::anyhow!("missing session updated_at"))?=int(old_at.max(new_at));
                *merged.get_mut(6).ok_or_else(||anyhow::anyhow!("missing session closed_at"))?=close.map_or(Value::Null,int);
                merged
            };
            // Revisions describe the host's projection, not a cross-source ordering.
            *merged.get_mut(8).ok_or_else(||anyhow::anyhow!("missing session revision"))?=value(&prior,8)?.clone();
            if merged==prior {return Ok(());}
            merged
        } else {incoming};
        let mut merged=merged;
        *merged.get_mut(8).ok_or_else(||anyhow::anyhow!("missing session revision"))?=self.merge_next_rev()?;
        self.conn.execute("INSERT INTO convo_sessions(id,info,meta,state,changes,updated_at,closed_at,epoch,rev,machine) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(id) DO UPDATE SET info=excluded.info,meta=excluded.meta,state=excluded.state,changes=excluded.changes,updated_at=excluded.updated_at,closed_at=excluded.closed_at,epoch=excluded.epoch,rev=excluded.rev,machine=excluded.machine",merged)?;
        super::super::retention::projection(self.conn,&id,self.rev,false,"adopted")?;
        Ok(())
    }
}
