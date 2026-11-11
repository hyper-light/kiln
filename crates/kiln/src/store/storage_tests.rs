//! Bounded real-file fixtures for storage::reserve_rows. No throughput or latency assertion.

use std::io::Write as _;
use std::path::{Path,PathBuf};

use rusqlite::{Connection,TransactionBehavior,params};
use serde::Serialize;

use super::reserve_rows;

#[derive(Debug,Serialize)]
struct Layout { page_size:u64,page_count:u64,trees:u64,columns:u64 }

#[derive(Debug,Serialize)]
struct Growth {
    case:&'static str,
    sqlite:String,
    layout:Layout,
    payload_bound:u64,
    rows:u64,
    reserve:u64,
    db_before:u64,
    db_after_checkpoint:u64,
    pages_after_checkpoint:u64,
    wal_during_transaction:u64,
    wal_after_commit:u64,
    shm_growth:u64,
    observed_growth:u64,
}

struct WalDb {
    connection:Connection,
    path:PathBuf,
    // Close SQLite before removing its private database/WAL/SHM files on success or error.
    _directory:tempfile::TempDir,
}

fn size(path: &Path) -> anyhow::Result<u64> {
    match std::fs::metadata(path) { Ok(value)=>Ok(value.len()),Err(error) if error.kind()==std::io::ErrorKind::NotFound=>Ok(0),Err(error)=>Err(error.into()) }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name=path.as_os_str().to_owned();name.push(suffix);name.into()
}

fn pragma(connection: &Connection, name: &str) -> anyhow::Result<u64> {
    let statement=match name { "page_size"=>"PRAGMA page_size","page_count"=>"PRAGMA page_count",_=>anyhow::bail!("unknown fixture pragma") };
    Ok(u64::try_from(connection.query_row(statement,[],|row|row.get::<_,i64>(0))?)?)
}

fn layout(connection: &Connection, tables: &[&str]) -> anyhow::Result<Layout> {
    let (mut trees,mut columns)=(0u64,0u64);
    for table in tables {
        let roots=u64::try_from(connection.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE rootpage>0 AND tbl_name=?1",[table],|row|row.get::<_,i64>(0))?)?;
        let width=u64::try_from(connection.query_row("SELECT COUNT(*) FROM pragma_table_info(?1)",[table],|row|row.get::<_,i64>(0))?)?;
        anyhow::ensure!(roots>0&&width>0,"fixture has no actual affected Btrees/columns");
        trees=trees.checked_add(roots).ok_or_else(||anyhow::anyhow!("fixture tree count overflow"))?;columns=columns.max(width);
    }
    let sequence=u64::try_from(connection.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE rootpage>0 AND name='sqlite_sequence'",[],|row|row.get::<_,i64>(0))?)?;
    trees=trees.checked_add(sequence).ok_or_else(||anyhow::anyhow!("fixture sequence tree overflow"))?;
    Ok(Layout { page_size:pragma(connection,"page_size")?,page_count:pragma(connection,"page_count")?,trees,columns })
}

impl WalDb {
    fn new(page_size: u64, schema: &str) -> anyhow::Result<Self> {
        let directory=tempfile::tempdir()?;let path=directory.path().join("storage.sqlite");let connection=Connection::open(&path)?;
        connection.pragma_update(None,"page_size",i64::try_from(page_size)?)?;
        let mode:String=connection.query_row("PRAGMA journal_mode=WAL",[],|row|row.get(0))?;
        anyhow::ensure!(mode=="wal","fixture did not enter real WAL mode");
        connection.execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA synchronous=FULL; PRAGMA cache_size=8; PRAGMA cache_spill=ON; PRAGMA secure_delete=ON;")?;
        connection.execute_batch(schema)?;
        Ok(Self { connection,path,_directory:directory })
    }

    fn checkpoint(&self) -> anyhow::Result<()> {
        let (busy,_,_):(i64,i64,i64)=self.connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        anyhow::ensure!(busy==0,"private fixture checkpoint was blocked");Ok(())
    }

    fn measure(&mut self, case: &'static str, tables: &[&str], payload_bound:u64, rows:u64, require_spill:bool, mutate:impl FnOnce(&Connection)->anyhow::Result<()>) -> anyhow::Result<Growth> {
        self.checkpoint()?;
        let wal=sidecar(&self.path,"-wal");let shm=sidecar(&self.path,"-shm");
        let db_before=size(&self.path)?;let wal_before=size(&wal)?;let shm_before=size(&shm)?;
        assert_eq!(wal_before,0,"measurement begins after truncating baseline WAL");
        let layout=layout(&self.connection,tables)?;
        assert_eq!(db_before,layout.page_count.checked_mul(layout.page_size).ok_or_else(||anyhow::anyhow!("fixture baseline size overflow"))?);
        let reserve=reserve_rows(layout.page_size,layout.page_count,layout.trees,layout.columns,payload_bound,rows)?;
        let transaction=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        mutate(&transaction)?;
        let wal_during_transaction=size(&wal)?;
        if require_spill { assert!(wal_during_transaction>wal_before,"large transaction must spill actual frames before commit"); }
        transaction.commit()?;
        let wal_after_commit=size(&wal)?;let shm_growth=size(&shm)?.saturating_sub(shm_before);
        self.checkpoint()?;
        let db_after_checkpoint=size(&self.path)?;let pages_after_checkpoint=pragma(&self.connection,"page_count")?;
        assert_eq!(db_after_checkpoint,pages_after_checkpoint.checked_mul(layout.page_size).ok_or_else(||anyhow::anyhow!("fixture final size overflow"))?);
        let observed_growth=wal_after_commit.saturating_sub(wal_before).checked_add(db_after_checkpoint.saturating_sub(db_before)).and_then(|sum|sum.checked_add(shm_growth)).ok_or_else(||anyhow::anyhow!("fixture growth overflow"))?;
        let growth=Growth { case,sqlite:self.connection.query_row("SELECT sqlite_version()",[],|row|row.get(0))?,layout,payload_bound,rows,reserve,db_before,db_after_checkpoint,pages_after_checkpoint,wal_during_transaction,wal_after_commit,shm_growth,observed_growth };
        if std::env::var_os("KILN_STORAGE_FIXTURE_JSON").is_some() { let _=writeln!(std::io::stdout().lock(),"{}",serde_json::to_string(&growth)?); }
        assert!(growth.observed_growth<=growth.reserve,"actual WAL + checkpoint database growth + new SHM exceeds reserve: {growth:?}");
        Ok(growth)
    }
}

#[test]
fn overflow_payload_and_spilled_wal_fit_reserved_scratch() -> anyhow::Result<()> {
    let mut db=WalDb::new(512,"CREATE TABLE messages(id INTEGER PRIMARY KEY,body BLOB NOT NULL); CREATE TABLE baseline(id INTEGER PRIMARY KEY,body BLOB NOT NULL);")?;
    db.connection.execute("INSERT INTO baseline VALUES(1,zeroblob(?1))",[2*1024*1024])?;
    let body=vec![0x5au8;8*1024*1024];
    let growth=db.measure("overflow-512-spill",&["messages"],u64::try_from(body.len())?,1,true,|c| { c.execute("INSERT INTO messages(id,body) VALUES(1,?1)",[&body])?;Ok(()) })?;
    assert_eq!(growth.layout.page_size,512);assert_eq!(growth.layout.trees,1);assert_eq!(growth.layout.columns,2);
    assert!(growth.pages_after_checkpoint>growth.layout.page_count);
    assert!(growth.observed_growth>growth.payload_bound,"WAL and checkpoint growth coexist beyond just the body bytes");
    Ok(())
}

#[test]
fn indexed_large_keys_and_many_inserts_fit_actual_schema_reserve() -> anyhow::Result<()> {
    let mut db=WalDb::new(4096,"CREATE TABLE messages(id TEXT PRIMARY KEY,body BLOB NOT NULL,sender TEXT NOT NULL); CREATE INDEX messages_sender ON messages(sender); CREATE INDEX messages_body ON messages(body); CREATE TABLE baseline(id INTEGER PRIMARY KEY AUTOINCREMENT,body BLOB NOT NULL);")?;
    db.connection.execute("INSERT INTO baseline(body) VALUES(zeroblob(?1))",[2*1024*1024])?;
    let body=vec![0x43u8;64*1024];let mut originals=Vec::new();
    for row in 0..32 { originals.push((format!("{:04}-{}",(row*17)%32,"k".repeat(8*1024)),format!("sender-{row}-{}","s".repeat(4*1024)))); }
    let mut payload=0u64;
    for (key,sender) in &originals {
        let row=key.len().checked_add(sender.len()).and_then(|len|len.checked_add(body.len())).ok_or_else(||anyhow::anyhow!("fixture row size overflow"))?;
        payload=payload.checked_add(u64::try_from(row)?).ok_or_else(||anyhow::anyhow!("fixture total payload overflow"))?;
    }
    let rows=u64::try_from(originals.len())?;
    let growth=db.measure("indexed-large-keys-4096",&["messages"],payload,rows,true,|c| {
        for (key,sender) in &originals { c.execute("INSERT INTO messages(id,body,sender) VALUES(?1,?2,?3)",params![key,body,sender])?; }Ok(())
    })?;
    assert_eq!(growth.layout.page_size,4096);assert_eq!(growth.layout.trees,5,"table + primary-key index + two explicit indexes + sqlite_sequence");assert_eq!(growth.layout.columns,3);
    assert_eq!(db.connection.query_row("SELECT COUNT(*) FROM messages",[],|row|row.get::<_,i64>(0))?,i64::try_from(rows)?);
    for (key,sender) in originals { assert_eq!(db.connection.query_row("SELECT length(body),sender FROM messages WHERE id=?1",[key],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?,(i64::try_from(body.len())?,sender)); }
    Ok(())
}

#[test]
fn shrinking_old_overflow_body_and_keys_with_secure_delete_fit_prior_bound() -> anyhow::Result<()> {
    let mut db=WalDb::new(4096,"CREATE TABLE messages(id TEXT PRIMARY KEY,body BLOB NOT NULL,sender TEXT NOT NULL); CREATE INDEX messages_sender ON messages(sender);")?;
    let old_body=vec![0x71u8;1024*1024];let old_key="k".repeat(128*1024);let old_sender="s".repeat(128*1024);
    db.connection.execute("INSERT INTO messages(id,body,sender) VALUES(?1,?2,?3)",params![old_key,old_body,old_sender])?;
    let prior=u64::try_from(old_body.len().checked_add(old_key.len()).and_then(|len|len.checked_add(old_sender.len())).ok_or_else(||anyhow::anyhow!("fixture prior payload overflow"))?)?;
    let growth=db.measure("shrink-old-overflow-secure-delete",&["messages"],prior,1,true,|c| { c.execute("UPDATE messages SET id='small',body=x'01',sender='short' WHERE id=?1",[&old_key])?;Ok(()) })?;
    assert_eq!(growth.layout.trees,3);assert_eq!(growth.layout.columns,3);
    assert!(growth.wal_after_commit>1,"shrinking a body still writes old overflow/free pages");
    assert_eq!(db.connection.query_row("SELECT id,body,sender FROM messages",[],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Vec<u8>>(1)?,row.get::<_,String>(2)?)))?,("small".into(),vec![1],"short".into()));
    Ok(())
}
