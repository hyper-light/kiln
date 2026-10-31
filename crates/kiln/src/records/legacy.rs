//! U06/U15: read-only, snapshot-pinned input for restartable imports. This is never a Records
//! implementation and cannot acknowledge a write. Retired files remain available for rollback.
use std::num::NonZero;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use amphora_api::Value;
use turso_core::{ConnectOpts, Connection, Database, IO, OpenFlags, OpenOptions, PlatformIO, SqliteDialect, StepResult, Value as OldValue, Numeric};

pub(crate) struct Legacy { io: Arc<dyn IO>, _database: Arc<Database>, connection: Arc<Connection> }
impl Legacy {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(path.is_file(), "legacy input is missing: {}", path.display());
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new()?);
        let name = path.to_str().ok_or_else(|| anyhow::anyhow!("legacy path is not UTF-8"))?;
        let database = Database::open(io.clone(), name, OpenOptions::new(Arc::new(SqliteDialect)).flags(OpenFlags::ReadOnly))?;
        let connection = database.connect(ConnectOpts::new())?;
        let source = Self { io, _database:database, connection };
        source.query("BEGIN", &[], |_| Ok(true))?;
        // Establish the snapshot before returning to the importer.
        source.query("SELECT name FROM sqlite_schema LIMIT 1", &[], |_| Ok(false))?;
        Ok(source)
    }
    pub fn query(&self, sql: &str, params: &[OldValue], mut each: impl FnMut(&turso_core::Row) -> anyhow::Result<bool>) -> anyhow::Result<()> {
        let mut stmt=self.connection.prepare(sql)?;
        for (i,value) in params.iter().enumerate() {
            stmt.bind_at(NonZero::new(i.saturating_add(1)).ok_or_else(|| anyhow::anyhow!("invalid parameter"))?,value.clone())?;
        }
        let until=Instant::now()+Duration::from_secs(30);
        loop {
            anyhow::ensure!(Instant::now()<until,"legacy import read timed out");
            match stmt.step()? {
                StepResult::Row => if let Some(row)=stmt.row() && !each(row)? { stmt.reset()?; return Ok(()); },
                StepResult::Done => return Ok(()),
                StepResult::IO | StepResult::Yield => self.io.step()?,
                StepResult::Sleep{duration} => { std::thread::sleep(duration.min(Duration::from_millis(10))); self.io.step()?; },
                StepResult::Busy => std::thread::sleep(Duration::from_millis(10)),
                StepResult::Interrupt => anyhow::bail!("legacy import read interrupted"),
            }
        }
    }
    pub fn has(&self, table: &str) -> anyhow::Result<bool> {
        let mut found=false;
        self.query("SELECT name FROM sqlite_schema WHERE type='table' AND name=?1", &[OldValue::from_text(table.to_owned())], |_| { found=true; Ok(false) })?;
        Ok(found)
    }
    /// Logical snapshot identity includes all rows and table definitions, including WAL-only
    /// data. File size/mtime alone cannot fence a replacement with the same metadata.
    pub fn fingerprint(&self, tables: &[&str]) -> anyhow::Result<String> {
        use sha2::{Digest as _,Sha256};
        let mut hash=Sha256::new();
        for table in tables {
            hash.update(postcard::to_allocvec(table)?);
            if !self.has(table)? { hash.update(b"absent"); continue; }
            self.query("SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1", &[OldValue::from_text((*table).to_owned())], |r| {
                hash.update(postcard::to_allocvec(&r.get::<String>(0)?)?); Ok(false)
            })?;
            let mut cursor=i64::MIN;
            loop {
                let rows=self.page(table,cursor)?;
                if rows.is_empty(){break;}
                for row in &rows { hash.update(postcard::to_allocvec(row)?); }
                cursor=rows.last().map_or(cursor,|(id,_)|*id);
            }
        }
        Ok(hex::encode(hash.finalize()))
    }
    pub fn page(&self, table: &str, after: i64) -> anyhow::Result<Vec<(i64,Vec<Value>)>> {
        // Identifiers are only the fixed legacy tables, never external SQL.
        anyhow::ensure!(["meta","sessions","events","records","drops","peers","copied","versions","records_meta","records_versions","records_families"].contains(&table),"unknown legacy table");
        if after==i64::MIN {
            // This value is the persisted initial-cursor sentinel. Refuse a source row at it
            // explicitly instead of silently skipping it or looping on a singleton large row.
            self.query(&format!("SELECT rowid FROM {table} WHERE rowid=?1"),&[OldValue::from_i64(i64::MIN)],|_|anyhow::bail!("legacy {table} uses the minimum rowid; migration cursor cannot represent it"))?;
        }
        let mut rows=Vec::new(); let mut bytes=0usize;
        self.query(&format!("SELECT rowid,* FROM {table} WHERE rowid>?1 ORDER BY rowid LIMIT 128"), &[OldValue::from_i64(after)], |row| {
            let id=row.get::<i64>(0)?;
            let values=row.get_values().skip(1).map(owned).collect::<Vec<_>>();
            let mut size=0usize;
            for value in &values { size=size.checked_add(match value { Value::Text(s)=>s.len(),Value::Blob(b)=>b.len(),_=>8 }).ok_or_else(||anyhow::anyhow!("legacy row size overflow"))?; }
            // U06: a transcript retains the recorder's original 256 MiB line bound. A large
            // singleton is decoded/staged before entering a bounded product SQL call.
            anyhow::ensure!(size<=257<<20,"legacy import row exceeds 256 MiB plus metadata; migration refused");
            if !rows.is_empty() && bytes.saturating_add(size)>4<<20 { return Ok(false); }
            bytes=bytes.checked_add(size).ok_or_else(||anyhow::anyhow!("legacy page size overflow"))?;
            rows.push((id,values)); Ok(bytes<4<<20)
        })?;
        Ok(rows)
    }
}
fn owned(value:&OldValue)->Value {
    match value { OldValue::Null=>Value::Null,OldValue::Numeric(Numeric::Integer(n))=>Value::Integer(*n),OldValue::Numeric(Numeric::Float(n))=>Value::Real(f64::from(*n)),OldValue::Text(s)=>Value::Text(s.as_str().to_owned()),OldValue::Blob(b)=>Value::Blob(b.to_vec()) }
}
