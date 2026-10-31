//! U19: independent legacy stores share IDs and local coordinates, not ownership of the
//! canonical rowid or revision space. These tests exercise the real paged import boundary.
use super::*;

struct LegacyEvent<'a> { rowid:i64,session:&'a str,id:&'a str,n:i64,rev:i64,text:&'a str,body:&'a str }
struct Archived { source:String,table:String,rowid:i64,rev:Option<u64>,payload:Vec<Value> }

fn source(path:&std::path::Path,id:&str,head:i64)->anyhow::Result<rusqlite::Connection> {
    let connection=rusqlite::Connection::open(path)?;
    connection.execute_batch(crate::convo::sql::META)?;
    for migration in crate::convo::sql::MIGRATIONS {connection.execute_batch(migration)?;}
    for (key,value) in [("schema",crate::convo::sql::MIGRATIONS.len().to_string()),("id",id.to_owned()),("rev",head.to_string()),("squeeze_below",head.checked_add(1).ok_or_else(||anyhow::anyhow!("fixture revision overflow"))?.to_string())] {
        connection.execute("INSERT INTO meta(key,value) VALUES (?1,?2)",rusqlite::params![key,value])?;
    }
    Ok(connection)
}
fn session(connection:&rusqlite::Connection,rowid:i64,id:&str,meta:Option<&str>,machine:Option<&str>,rev:i64,at:i64)->anyhow::Result<()> {
    connection.execute("INSERT INTO sessions(rowid,id,info,meta,state,changes,updated_at,closed_at,epoch,rev,machine) VALUES (?1,?2,?3,?4,?5,NULL,?6,NULL,?7,?8,?9)",rusqlite::params![rowid,id,format!(r#"{{"agent":"{id}"}}"#),meta,r#"{"phase":"ready"}"#,at,if id=="shared" {4}else{3},rev,machine])?;
    Ok(())
}
fn event(connection:&rusqlite::Connection,event:LegacyEvent<'_>)->anyhow::Result<()> {
    connection.execute("INSERT INTO events(rowid,session,id,seq,at,parent,by,kind,level,tool,status,text,body,rev) VALUES (?1,?2,?3,?4,20,NULL,NULL,'user',NULL,NULL,NULL,?5,?6,?7)",rusqlite::params![event.rowid,event.session,event.id,event.n,event.text,event.body,event.rev])?;
    Ok(())
}
fn record(connection:&rusqlite::Connection,rowid:i64,session:&str,end:i64,rev:i64,line:&str)->anyhow::Result<()> {
    connection.execute("INSERT INTO records(rowid,session,source,n,line_end,line,rev) VALUES (?1,?2,?3,0,?4,?5,?6)",rusqlite::params![rowid,session,if session=="shared" {"shared-rollout"}else{"rollout"},end,line,rev])?;
    Ok(())
}
fn archives(database:&Database)->anyhow::Result<Vec<Archived>> {
    let rows=database.call(until(),|connection| {
        let mut query=connection.prepare("SELECT source,table_name,source_rowid,source_rev,payload FROM convo_legacy_rows ORDER BY source,table_name,source_rowid")?;
        Ok(query.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,Option<u64>>(3)?,row.get::<_,Vec<u8>>(4)?)))?.collect::<amphora_api::sql::Result<Vec<_>>>()?)
    })?;
    rows.into_iter().map(|(source,table,rowid,rev,payload)|Ok(Archived {source,table,rowid,rev,payload:postcard::from_bytes(&payload)?})).collect()
}
fn archived_meta<'a>(rows:&'a [Archived],source:&str,key:&str)->Option<&'a Value> {
    rows.iter().filter(|row|row.source==source && row.table=="meta").find_map(|row| {
        if row.payload.first()==Some(&txt(key)) {row.payload.get(1)} else {None}
    })
}

#[test]
fn u19_legacy_sources_merge_disjoint_and_identical_rows_without_coordinate_collision()->anyhow::Result<()> {
    let directory=tempfile::tempdir()?;
    let a=directory.path().join("server-conversations.db");
    let b=directory.path().join("runner-conversations.db");
    const BODY:&str=r#"{"type":"user","attachments":[{"name":"a.png","data":"YWJj"}]}"#;
    for (path,identity,head,own,common_rowid,common_rev,machine) in [(&a,"source-a",100,"left",2,11,None),(&b,"source-b",200,"right",9,99,Some("node-shared"))] {
        let old=source(path,identity,head)?;
        session(&old,1,own,None,None,1,10)?;
        session(&old,common_rowid,"shared",machine.map(|_|r#"{"title":"shared"}"#),machine,common_rev,20)?;
        event(&old,LegacyEvent {rowid:1,session:own,id:own,n:0,rev:1,text:own,body:r#"{"type":"user"}"#})?;
        event(&old,LegacyEvent {rowid:common_rowid,session:"shared",id:"common-event",n:4,rev:common_rev,text:"attachment",body:BODY})?;
        record(&old,1,own,10,1,own)?;
        record(&old,common_rowid,"shared",if own=="left" {101}else{202},common_rev,"shared 界")?;
        // Both reset keys use the same source-local revision, but neither range covers
        // an existing event or line. Import retains them without replaying a deletion.
        old.execute("INSERT INTO drops(rev,session,what,source,n,at) VALUES (73,'shared',?1,?2,99,99)",rusqlite::params![if own=="left" {"events"}else{"record"},if own=="left" {None}else{Some("shared-rollout")}])?;
        old.execute("INSERT INTO peers(peer,rev,at) VALUES ('old-peer',?1,?2)",rusqlite::params![if own=="left" {41}else{42},head])?;
        old.execute("INSERT INTO copied(session) VALUES ('shared')",[])?;
    }
    let original_a=std::fs::read(&a)?;
    let original_b=std::fs::read(&b)?;
    let source_a=a.canonicalize()?.to_string_lossy().into_owned();
    let source_b=b.canonicalize()?.to_string_lossy().into_owned();
    let path=directory.path().join("amphora.db");
    let (host_id,top,archive_count)={
        let database=Database::open(&path,until())?;
        let mut engine=Amphora::from_database(database.clone())?;
        let host_id=engine.id();
        engine.import_legacy(&a)?;
        engine.import_legacy(&b)?;
        assert_eq!(engine.look(true)?.sessions,Some(3));
        assert_eq!(engine.events("left",None,10)?.len(),1);
        assert_eq!(engine.events("right",None,10)?.len(),1);
        let common=engine.events("shared",None,10)?;
        assert_eq!(common.len(),1);
        assert_eq!(common.first().map(|event|event.body.as_str()),Some(BODY));
        assert_eq!(engine.record("left","rollout",0,10)?,vec![(0,"left".to_owned())]);
        assert_eq!(engine.record("right","rollout",0,10)?,vec![(0,"right".to_owned())]);
        assert_eq!(engine.record("shared","shared-rollout",0,10)?,vec![(0,"shared 界".to_owned())]);
        let state=database.call(until(),|connection|Ok((
            connection.query_row("SELECT value FROM convo_meta WHERE key='id'",[],|row|row.get::<_,String>(0))?,
            connection.query_row("SELECT value FROM convo_meta WHERE key='schema'",[],|row|row.get::<_,String>(0))?,
            connection.query_row("SELECT meta,machine FROM convo_sessions WHERE id='shared'",[],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?,
            connection.query_row("SELECT line_end FROM convo_records WHERE session='shared' AND source='shared-rollout' AND n=0",[],|row|row.get::<_,u64>(0))?,
            connection.query_row("SELECT count(*) FROM convo_legacy_resets",[],|row|row.get::<_,u64>(0))?,
            connection.query_row("SELECT count(*) FROM convo_peers",[],|row|row.get::<_,u64>(0))?,
            connection.query_row("SELECT count(*) FROM convo_drops",[],|row|row.get::<_,u64>(0))?,
            connection.query_row("SELECT count(*) FROM convo_copied",[],|row|row.get::<_,u64>(0))?,
        )))?;
        assert_eq!(state.0,host_id);
        assert_eq!(state.1,sql::MIGRATIONS.len().to_string());
        assert_eq!(state.2,(r#"{"title":"shared"}"#.to_owned(),"node-shared".to_owned()));
        assert_eq!(state.3,202);
        assert_eq!((state.4,state.5,state.6,state.7),(2,0,0,0));
        let rows=archives(&database)?;
        for origin in [&source_a,&source_b] {
            assert_eq!(rows.iter().filter(|row|row.source==*origin && row.rowid==1 && row.rev==Some(1) && matches!(row.table.as_str(),"sessions"|"events"|"records")).count(),3);
            for table in ["sessions","events","records"] {assert_eq!(rows.iter().filter(|row|row.source==*origin && row.table==table).count(),2);}
            for table in ["peers","drops","copied"] {assert_eq!(rows.iter().filter(|row|row.source==*origin && row.table==table).count(),1);}
        }
        assert_eq!(archived_meta(&rows,&source_a,"id"),Some(&txt("source-a")));
        assert_eq!(archived_meta(&rows,&source_b,"id"),Some(&txt("source-b")));
        assert_eq!(archived_meta(&rows,&source_a,"rev"),Some(&txt("100")));
        assert_eq!(archived_meta(&rows,&source_b,"rev"),Some(&txt("200")));
        assert_eq!(rows.iter().find(|row|row.source==source_a && row.table=="peers").and_then(|row|row.rev),Some(41));
        assert_eq!(rows.iter().find(|row|row.source==source_b && row.table=="peers").and_then(|row|row.rev),Some(42));
        assert_eq!(rows.iter().filter(|row|row.table=="drops" && row.rowid==73 && row.rev==Some(73)).count(),2);
        assert!(engine.top()>=200);
        (host_id,engine.top(),rows.len())
    };
    let database=Database::open(&path,until())?;
    let mut engine=Amphora::from_database(database.clone())?;
    engine.import_legacy(&a)?;
    engine.import_legacy(&b)?;
    assert_eq!(engine.id(),host_id);
    assert_eq!(engine.top(),top);
    assert_eq!(archives(&database)?.len(),archive_count);
    engine.apply(&[Row::Event(Box::new(EventRow {session:"left".into(),id:"after-import".into(),seq:1,at:201,parent:None,by:None,kind:"user".into(),level:None,tool:None,status:None,text:"new".into(),body:"{}".into()}))])?;
    assert!(engine.top()>top && engine.top()>200);
    assert_eq!(std::fs::read(&a)?,original_a);
    assert_eq!(std::fs::read(&b)?,original_b);
    Ok(())
}

#[test]
fn u19_divergent_legacy_row_rolls_back_page_and_cursor_with_typed_collision()->anyhow::Result<()> {
    let directory=tempfile::tempdir()?;
    let a=directory.path().join("a.db");
    let c=directory.path().join("c.db");
    {
        let old=source(&a,"source-a",100)?;
        session(&old,1,"s",None,None,1,10)?;
        event(&old,LegacyEvent {rowid:1,session:"s",id:"e",n:0,rev:1,text:"existing",body:r#"{"original":true}"#})?;
    }
    {
        let old=source(&c,"source-c",200)?;
        session(&old,1,"s",None,None,1,10)?;
        event(&old,LegacyEvent {rowid:1,session:"s",id:"first-in-page",n:1,rev:1,text:"new",body:"{}"})?;
        event(&old,LegacyEvent {rowid:2,session:"s",id:"e",n:0,rev:2,text:"existing",body:r#"{"original":false}"#})?;
    }
    let original_a=std::fs::read(&a)?;
    let original_c=std::fs::read(&c)?;
    let source_c=c.canonicalize()?.to_string_lossy().into_owned();
    let database=Database::in_memory(until())?;
    let mut engine=Amphora::from_database(database.clone())?;
    engine.import_legacy(&a)?;
    let top=engine.top();
    for _ in 0..2 {
        let error=match engine.import_legacy(&c) {Ok(())=>anyhow::bail!("divergent source unexpectedly imported"),Err(error)=>error};
        let collision=error.downcast_ref::<ImportCollision>().ok_or_else(||anyhow::anyhow!("expected typed ImportCollision: {error}"))?;
        assert_eq!(collision.source,source_c);
        assert_eq!(collision.table,"events");
        assert_eq!(collision.key,"s/e");
        assert!(collision.reason.contains("different content"));
        assert_eq!(engine.top(),top);
        let events=engine.events("s",None,10)?;
        assert_eq!(events.len(),1);
        assert_eq!(events.first().map(|event|event.body.as_str()),Some(r#"{"original":true}"#));
        let source_c=source_c.clone();
        let progress=database.call(until(),move |connection|Ok((
            connection.query_row("SELECT count(*) FROM convo_legacy_rows WHERE source=?1 AND table_name='events'",vec![txt(&source_c)],|row|row.get::<_,u64>(0))?,
            connection.query_row("SELECT count(*) FROM convo_import WHERE source=?1 AND table_name='events'",vec![txt(&source_c)],|row|row.get::<_,u64>(0))?,
            connection.query_row("SELECT done FROM convo_import WHERE source=?1 AND table_name='__source__'",vec![txt(&source_c)],|row|row.get::<_,u64>(0))?,
            connection.query_row("SELECT count(*) FROM convo_legacy_rows WHERE source=?1 AND table_name='sessions'",vec![txt(&source_c)],|row|row.get::<_,u64>(0))?,
        )))?;
        assert_eq!(progress,(0,0,0,1),"the prior table may commit; the failed event page must not");
    }
    assert_eq!(std::fs::read(&a)?,original_a);
    assert_eq!(std::fs::read(&c)?,original_c);
    Ok(())
}
