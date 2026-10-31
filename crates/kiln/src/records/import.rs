//! U06: bounded legacy exports into the product's fenced import protocol. Source SQL tables
//! remain immutable compatibility input; only Namespace::import_page writes canonical Records.
use std::path::Path;
use amphora::database::{Database,Namespace};
use amphora_api::{Value,import::{ImportPage,ImportState,ImportVersion},sql::OptionalExtension as _};
use serde::{Deserialize,Serialize};
use sha2::{Digest as _,Sha256};
use super::{deadline,legacy::Legacy};

const BYTES:usize=4<<20;
// Product SQL owns its result rows. Eight maximum-size values remain below RESULT_MAX.
const ROWS:usize=8;
#[derive(Clone,Serialize,Deserialize)]
enum Cursor { Versions(Option<(String,Vec<u8>,u64)>), Families(Option<String>), Finished }
#[derive(Clone,Copy)]
struct State {head:u64,horizon:u64,feed:u64}
trait Source {
    fn state(&self)->anyhow::Result<State>;
    fn versions(&self,after:Option<(String,Vec<u8>,u64)>)->anyhow::Result<Vec<ImportVersion>>;
    fn families(&self,after:Option<String>)->anyhow::Result<Vec<(String,u64)>>;
}
struct Host(Legacy);
struct Server(Database);
fn position(v:&ImportVersion)->(String,Vec<u8>,u64) {(v.family.clone(),v.key.clone(),v.rev)}
fn num(n:i64)->anyhow::Result<u64> {Ok(u64::try_from(n)?)}
fn cap(rows:Vec<ImportVersion>)->anyhow::Result<Vec<ImportVersion>> {
    let mut out=Vec::new();let mut bytes=0usize;
    for row in rows {
        let size=row.family.len().saturating_add(row.key.len()).saturating_add(row.value.as_ref().map_or(0,Vec::len)).saturating_add(64);
        anyhow::ensure!(row.value.as_ref().is_none_or(|v|v.len()<=amphora_api::VALUE_MAX),"legacy Records value exceeds product limit");
        if bytes.saturating_add(size)>BYTES && !out.is_empty(){break;}
        bytes=bytes.checked_add(size).ok_or_else(||anyhow::anyhow!("import page overflow"))?;
        out.push(row);
    }
    Ok(out)
}
fn query(table:&str,sequence:&str,filter:&str,limit:usize)->String {
    format!("SELECT family,key,rev,{sequence},value FROM {table}{filter} ORDER BY family,key,rev LIMIT {limit}")
}
impl Source for Host {
    fn state(&self)->anyhow::Result<State> {
        let (mut head,mut horizon,mut feed)=(0,0,0);
        self.0.query("SELECT name,value FROM meta",&[],|r| {
            let name=r.get::<String>(0)?;let value=num(r.get::<i64>(1)?)?;
            match name.as_str(){"head"=>head=value,"horizon"=>horizon=value,"feed"=>feed=value,_=>{}}
            Ok(true)
        })?;
        self.0.query("SELECT max(rev) FROM versions",&[],|r|{if let Ok(value)=r.get::<i64>(0){head=head.max(num(value)?);}Ok(false)})?;
        Ok(State {head,horizon,feed})
    }
    fn versions(&self,after:Option<(String,Vec<u8>,u64)>)->anyhow::Result<Vec<ImportVersion>> {
        use turso_core::Value as V;
        // U06/data3: the retired Turso backend seeks scalar primary-key prefixes. These three
        // disjoint ranges retain the composite order without nullable/OR range predicates.
        let ranges=match after {
            None=>vec![("",vec![])],
            Some((f,k,r))=>vec![
                (" WHERE family=?1 AND key=?2 AND rev>?3",vec![V::from_text(f.clone()),V::from_slice(&k)?,V::from_i64(i64::try_from(r)?)]),
                (" WHERE family=?1 AND key>?2",vec![V::from_text(f.clone()),V::from_slice(&k)?]),
                (" WHERE family>?1",vec![V::from_text(f)]),
            ],
        };
        let mut rows=Vec::new();
        for (filter,params) in ranges {
            let remaining=ROWS.saturating_sub(rows.len());
            if remaining==0 {break;}
            self.0.query(&query("versions","n",filter,remaining),&params,|r| {
                let value=if matches!(r.get_value(4),V::Null){None}else{Some(r.get_value(4).to_blob().ok_or_else(||anyhow::anyhow!("legacy Records value is not a blob"))?.to_vec())};
                rows.push(ImportVersion {family:r.get(0)?,key:r.get_value(1).to_blob().ok_or_else(||anyhow::anyhow!("legacy key is not a blob"))?.to_vec(),rev:num(r.get(2)?)?,seq:num(r.get(3)?)?,value});Ok(true)
            })?;
        }
        cap(rows)
    }
    fn families(&self,after:Option<String>)->anyhow::Result<Vec<(String,u64)>> {
        let filter=if after.is_some(){" WHERE family>?1"}else{""};
        let params=after.map_or_else(Vec::new,|f|vec![turso_core::Value::from_text(f)]);
        let mut out=Vec::new();
        self.0.query(&format!("SELECT family,max(rev) FROM versions{filter} GROUP BY family ORDER BY family LIMIT 128"),&params,|r|{out.push((r.get(0)?,num(r.get(1)?)?));Ok(true)})?;
        Ok(out)
    }
}
impl Source for Server {
    fn state(&self)->anyhow::Result<State> {
        Ok(self.0.call(deadline(),|c|{
            let (head,horizon,feed)=c.query_row("SELECT head,horizon,log_horizon FROM records_meta WHERE id=1",[],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,u64>(1)?,r.get::<_,u64>(2)?)))?;
            Ok(State{head,horizon,feed})
        })?)
    }
    fn versions(&self,after:Option<(String,Vec<u8>,u64)>)->anyhow::Result<Vec<ImportVersion>> {
        // U06: Amphora's complete tuple bound uses the same primary-key order.
        let filter=if after.is_some(){" WHERE (family,key,rev) > (?1,?2,?3)"}else{""};
        let sql=query("records_versions","seq",filter,ROWS);
        let values=match after{Some((f,k,r))=>vec![Value::Text(f),Value::Blob(k),Value::Integer(i64::try_from(r)?)],None=>vec![]};
        cap(self.0.call(deadline(),move |c|{
            let mut q=c.prepare(&sql)?;
            Ok(q.query_map(values,|r|Ok(ImportVersion {family:r.get(0)?,key:r.get(1)?,rev:r.get(2)?,seq:r.get(3)?,value:r.get(4)?}))?.collect::<amphora_api::sql::Result<Vec<_>>>()?)
        })?)
    }
    fn families(&self,after:Option<String>)->anyhow::Result<Vec<(String,u64)>> {
        Ok(self.0.call(deadline(),move |c|{
            let filter=if after.is_some(){" WHERE family>?1"}else{""};
            let values=after.map_or_else(Vec::new,|f|vec![Value::Text(f)]);
            let mut q=c.prepare(&format!("SELECT family,newest FROM records_families{filter} ORDER BY family LIMIT 128"))?;
            Ok(q.query_map(values,|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<amphora_api::sql::Result<Vec<_>>>()?)
        })?)
    }
}

fn fingerprint(source:&impl Source,kind:&str)->anyhow::Result<String> {
    let mut hash=Sha256::new();hash.update(kind.as_bytes());
    let state=source.state()?;
    hash.update(postcard::to_allocvec(&(state.head,state.horizon,state.feed))?);
    let mut after=None;
    loop {
        let rows=source.versions(after)?;
        if rows.is_empty(){break;}
        for version in &rows {hash.update(b"version");hash.update(postcard::to_allocvec(version)?);}
        after=rows.last().map(position);
    }
    let mut after=None;
    loop {
        let rows=source.families(after)?;
        if rows.is_empty(){break;}
        for family in &rows {hash.update(b"family");hash.update(postcard::to_allocvec(family)?);}
        after=rows.last().map(|(f,_)|f.clone());
    }
    Ok(format!("kiln-records-v1:{kind}:{}",hex::encode(hash.finalize())))
}
fn run(source:&impl Source,namespace:&Namespace,kind:&str)->anyhow::Result<()> {
    let identity=fingerprint(source,kind)?;
    let progress=namespace.import_state(deadline())?;
    let mut encoded=match progress {
        Some(p)=>{anyhow::ensure!(p.source==identity,"legacy Records snapshot changed since import began; preserve both stores and restore the source before resuming");if p.done{return Ok(());}p.cursor},
        None=>Vec::new(),
    };
    let state=source.state()?;
    loop {
        let cursor=if encoded.is_empty(){Cursor::Versions(None)}else{postcard::from_bytes(&encoded)?};
        let (versions,newest,next,done)=match cursor {
            Cursor::Versions(after)=>{
                let versions=source.versions(after)?;
                let next=match versions.last(){Some(v)=>Cursor::Versions(Some(position(v))),None=>Cursor::Families(None)};
                (versions,Vec::new(),next,false)
            }
            Cursor::Families(after)=>{
                let newest=source.families(after)?;
                let next=match newest.last(){Some((f,_))=>Cursor::Families(Some(f.clone())),None=>Cursor::Finished};
                let done=matches!(next,Cursor::Finished);
                (Vec::new(),newest,next,done)
            }
            Cursor::Finished=>anyhow::bail!("incomplete product import has a terminal cursor"),
        };
        // Server compatibility tables share the host SQL database and are immutable after
        // cutover. Verify their full logical digest again before exposing any imported row.
        // Host input additionally holds one read-only WAL snapshot throughout these passes.
        if done {anyhow::ensure!(fingerprint(source,kind)?==identity,"legacy Records changed during import; destination remains hidden");}
        let next_cursor=postcard::to_allocvec(&next)?;
        let page=ImportPage {source:identity.clone(),expected_cursor:encoded,next_cursor:next_cursor.clone(),versions,state:ImportState {head:state.head,horizon:state.horizon,feed:state.feed,newest},done};
        namespace.import_page(page,deadline())?;
        encoded=next_cursor;
        if done {return Ok(());}
    }
}

pub(super) fn host(database:&Database,path:&Path,namespace:&str)->anyhow::Result<()> {
    let namespace=database.namespace(namespace)?;
    if !path.exists(){
        anyhow::ensure!(namespace.import_state(deadline())?.is_none_or(|p|p.done),"legacy records.db is missing while its namespace import is incomplete");
        return Ok(());
    }
    run(&Host(Legacy::open(path)?),&namespace,"host")
}
pub(super) fn server(database:&Database)->anyhow::Result<()> {
    let exists=database.call(deadline(),|c|Ok(c.query_row("SELECT name FROM sqlite_schema WHERE type='table' AND name='records_meta'",[],|r|r.get::<_,String>(0)).optional()?.is_some()))?;
    if !exists{return Ok(());}
    run(&Server(database.clone()),&database.namespace("kiln")?,"server")
}

#[cfg(test)]
mod tests {
    use super::*;
    use amphora_api::{Records as _,Consistency,Cond,Tx,Write,Refusal,sql::params};
    #[test]
    fn u06_both_legacy_layouts_import_resume_cas_reopen_and_compact()->anyhow::Result<()> {
        // U06/data3: real source pages cross a same-key revision boundary after row8 and a
        // family boundary after row16. Binary keys stay blobs, including NUL/0xff prefixes.
        let mut expected=Vec::new();
        for (seq,key) in [vec![0],vec![0,0]].into_iter().enumerate() {
            for rev in 1..=5 {expected.push(ImportVersion {family:"binary".into(),key:key.clone(),rev,seq:u64::try_from(seq)?,value:Some(vec![0,255])});}
        }
        for (n,key) in [vec![0,0,0],vec![0,0,255],vec![0,255],vec![127],vec![255],vec![255,0]].into_iter().enumerate() {
            expected.push(ImportVersion {family:"binary".into(),key,rev:5,seq:u64::try_from(n+2)?,value:Some(vec![255,0])});
        }
        for (key,rev,seq,value) in [(b"a".as_slice(),1,0,Some(b"old".as_slice())),(b"a".as_slice(),3,0,Some(b"new".as_slice())),(b"b".as_slice(),2,0,Some(b"before delete".as_slice())),(b"b".as_slice(),3,1,None)] {
            expected.push(ImportVersion {family:"items".into(),key:key.to_vec(),rev,seq,value:value.map(<[u8]>::to_vec)});
        }
        expected.push(ImportVersion {family:"zeta".into(),key:vec![0],rev:5,seq:0,value:Some(vec![0,255])});
        fn check_pages(source:&impl Source,expected:&[ImportVersion])->anyhow::Result<()> {
            let mut after=None;
            for page in expected.chunks(ROWS) {
                let actual=source.versions(after)?;
                assert_eq!(actual,page,"both source adapters retain the exact composite cursor");
                after=actual.last().map(position);
            }
            assert!(source.versions(after)?.is_empty());
            Ok(())
        }
        let dir=tempfile::tempdir()?;
        let old_path=dir.path().join("records.db");
        {
            let old=rusqlite::Connection::open(&old_path)?;
            old.execute_batch("CREATE TABLE meta(name TEXT PRIMARY KEY,value INTEGER NOT NULL); CREATE TABLE versions(family TEXT NOT NULL,key BLOB NOT NULL,rev INTEGER NOT NULL,n INTEGER NOT NULL,value BLOB,PRIMARY KEY(family,key,rev)); CREATE INDEX versions_by_rev ON versions(family,rev,n); INSERT INTO meta VALUES ('head',5),('horizon',2),('feed',1),('live',10);")?;
            for v in expected.iter().rev() {
                old.execute("INSERT INTO versions VALUES (?1,?2,?3,?4,?5)",rusqlite::params![v.family,v.key,i64::try_from(v.rev)?,i64::try_from(v.seq)?,v.value])?;
            }
        }
        let original=std::fs::read(&old_path)?;
        let path=dir.path().join("amphora.db");
        {
            let db=Database::open(&path,deadline())?;
            let source=Host(Legacy::open(&old_path)?);
            check_pages(&source,&expected)?;
            assert_eq!(source.families(None)?,vec![("binary".into(),5),("items".into(),3),("zeta".into(),5)]);
            assert_eq!(source.families(Some("binary".into()))?,vec![("items".into(),3),("zeta".into(),5)]);
            assert!(source.families(Some("zeta".into()))?.is_empty());
            let state=source.state()?;
            let version=source.versions(None)?.into_iter().next().ok_or_else(||anyhow::anyhow!("missing fixture version"))?;
            let next=postcard::to_allocvec(&Cursor::Versions(Some(position(&version))))?;
            db.namespace("host")?.import_page(ImportPage {source:fingerprint(&source,"host")?,expected_cursor:vec![],next_cursor:next,versions:vec![version],state:ImportState {head:state.head,horizon:state.horizon,feed:state.feed,newest:vec![]},done:false},deadline())?;
        }
        {
            let db=Database::open(&path,deadline())?;
            host(&db,&old_path,"host")?;
            let ns=db.namespace("host")?;
            assert_eq!(ns.head(deadline())?,5);
            assert_eq!(ns.get("binary",&[0,0],Consistency::Snapshot(4),deadline())?.map(|r|(r.rev,r.value)),Some((4,vec![0,255])));
            assert_eq!(ns.get("binary",&[255,0],Consistency::Linearizable,deadline())?.map(|r|(r.rev,r.value)),Some((5,vec![255,0])));
            assert_eq!(ns.get("items",b"a",Consistency::Snapshot(2),deadline())?.map(|r|(r.rev,r.value)),Some((1,b"old".to_vec())));
            assert!(ns.get("items",b"b",Consistency::Linearizable,deadline())?.is_none());
            let rev=ns.commit(Tx {conds:vec![Cond::At {family:"items".into(),key:b"a".to_vec(),rev:3}],writes:vec![Write::Put {family:"items".into(),key:b"a".to_vec(),value:b"after import".to_vec()}]},deadline())?;
            assert_eq!(rev,6);
            host(&db,&old_path,"host")?;
            assert_eq!(ns.head(deadline())?,6);
            // Bluegreen instances have independent old revision spaces. Import into a distinct
            // namespace; forcing this source into the first namespace must refuse, never merge.
            let second_path=dir.path().join("instance-records.db");
            std::fs::copy(&old_path,&second_path)?;
            {
                let old=rusqlite::Connection::open(&second_path)?;
                old.execute("UPDATE versions SET value=?1 WHERE family='items' AND key=?2 AND rev=3",rusqlite::params![b"other instance".as_slice(),b"a".as_slice()])?;
            }
            host(&db,&second_path,"host.instance2")?;
            assert_eq!(db.namespace("host.instance2")?.get("items",b"a",Consistency::Linearizable,deadline())?.map(|r|(r.rev,r.value)),Some((3,b"other instance".to_vec())));
            assert_eq!(ns.head(deadline())?,6);
            assert!(host(&db,&second_path,"host").is_err());
            // Main SQL compatibility rows share the SAME database, retaining their old independent
            // revision space in another Namespace; no extra Records file is opened.
            let server_versions=expected.clone();
            db.call(deadline(),move |c|{
                c.execute_batch(include_str!("../store/migrations/20261023-1300_p6_records.sql"))?;
                c.execute_batch("UPDATE records_meta SET head=5,horizon=2,log_horizon=1 WHERE id=1; INSERT INTO records_families VALUES ('binary',5),('items',3),('zeta',5),('compacted',4);")?;
                for v in server_versions.iter().rev() {
                    c.execute("INSERT INTO records_versions VALUES (?1,?2,?3,?4,?5)",params![v.family,v.key,v.rev,v.seq,v.value])?;
                }
                Ok(())
            })?;
            let server_source=Server(db.clone());
            check_pages(&server_source,&expected)?;
            assert_eq!(server_source.families(None)?,vec![("binary".into(),5),("compacted".into(),4),("items".into(),3),("zeta".into(),5)]);
            assert_eq!(server_source.families(Some("binary".into()))?,vec![("compacted".into(),4),("items".into(),3),("zeta".into(),5)]);
            assert!(server_source.families(Some("zeta".into()))?.is_empty());
            server(&db)?;
            assert_eq!(db.namespace("kiln")?.head(deadline())?,5);
            assert_eq!(db.namespace("kiln")?.get("binary",&[0,0],Consistency::Snapshot(4),deadline())?.map(|r|(r.rev,r.value)),Some((4,vec![0,255])));
            assert_eq!(db.namespace("kiln")?.try_wait("compacted",3)?,Some(4));
            ns.compact(6,deadline())?;
        }
        let db=Database::open(&path,deadline())?;
        let ns=db.namespace("host")?;
        assert_eq!(ns.head(deadline())?,6);
        assert!(matches!(ns.get("items",b"a",Consistency::Snapshot(2),deadline()),Err(Refusal::Compacted{..})));
        assert_eq!(ns.get("items",b"a",Consistency::Linearizable,deadline())?.map(|r|(r.rev,r.value)),Some((6,b"after import".to_vec())));
        assert_eq!(std::fs::read(&old_path)?,original);
        Ok(())
    }
}
