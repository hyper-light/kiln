//! U19: authenticated source identity is stable across retries and independent of a pinned
//! snapshot fingerprint. Replica hosts can retain different files at the same absolute path.
use super::*;

fn write_identity_source(path:&std::path::Path,identity:&str,session:&str,head:i64)->anyhow::Result<()> {
    if path.exists() {std::fs::remove_file(path)?;}
    let connection=rusqlite::Connection::open(path)?;
    connection.execute_batch(crate::convo::sql::META)?;
    for migration in crate::convo::sql::MIGRATIONS {connection.execute_batch(migration)?;}
    for (key,value) in [("schema",crate::convo::sql::MIGRATIONS.len().to_string()),("id",identity.to_owned()),("rev",head.to_string())] {
        connection.execute("INSERT INTO meta(key,value) VALUES (?1,?2)",rusqlite::params![key,value])?;
    }
    connection.execute("INSERT INTO sessions(rowid,id,info,updated_at,epoch,rev) VALUES (1,?1,'{}',1,1,1)",rusqlite::params![session])?;
    connection.execute("INSERT INTO events(rowid,session,id,seq,at,kind,text,body,rev) VALUES (1,?1,?1,0,1,'user',?1,?2,2)",rusqlite::params![session,format!(r#"{{"source":"{identity}"}}"#)])?;
    Ok(())
}

#[test]
fn u19_authenticated_hosts_separate_same_path_source_identity_and_fence_snapshot_replacement()->anyhow::Result<()> {
    let directory=tempfile::tempdir()?;
    let source_path=directory.path().join("conversations.db");
    let database_path=directory.path().join("amphora.db");
    write_identity_source(&source_path,"source-a","event-a",11)?;
    let canonical=source_path.canonicalize()?;
    let canonical=canonical.to_str().ok_or_else(||anyhow::anyhow!("fixture canonical path is not UTF-8"))?.to_owned();
    let source_a=serde_json::to_string(&(1u8,"authenticated-a",canonical.as_str()))?;
    let source_b=serde_json::to_string(&(1u8,"authenticated-b",canonical.as_str()))?;
    let (host_id,top)={
        let database=Database::open(&database_path,until())?;
        let mut engine=Amphora::from_database(database.clone())?;
        // Empty authenticated identities refuse before opening or registering a source.
        assert!(engine.import_legacy_from("",&directory.path().join("absent.db")).is_err());
        let initial=database.call(until(),|connection|Ok(connection.query_row("SELECT count(*) FROM convo_import",[],|row|row.get::<_,u64>(0))?))?;
        assert_eq!(initial,0);
        let host_id=engine.id();
        engine.import_legacy_from("authenticated-a",&source_path)?;
        engine.finish_imports()?;
        assert_eq!(engine.events("event-a",None,10)?.first().map(|event|event.body.as_str()),Some(r#"{"source":"source-a"}"#));

        // The local test path now stands for another host's independent retained file.
        // Both snapshots intentionally reuse source-local rowids and revisions.
        write_identity_source(&source_path,"source-b","event-b",21)?;
        let replacement_bytes=std::fs::read(&source_path)?;
        engine.import_legacy_from("authenticated-b",&source_path)?;
        engine.finish_imports()?;
        assert_eq!(engine.look(true)?.sessions,Some(2));
        assert_eq!(engine.events("event-a",None,10)?.first().map(|event|event.body.as_str()),Some(r#"{"source":"source-a"}"#));
        assert_eq!(engine.events("event-b",None,10)?.first().map(|event|event.body.as_str()),Some(r#"{"source":"source-b"}"#));
        let sources=database.call(until(),|connection| {
            let mut query=connection.prepare("SELECT source,fingerprint,done FROM convo_import WHERE table_name='__source__' ORDER BY source")?;
            Ok(query.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?)))?.collect::<amphora_api::sql::Result<Vec<_>>>()?)
        })?;
        assert_eq!(sources.len(),2);
        let a=sources.iter().find(|(source,_,_)|source==&source_a).ok_or_else(||anyhow::anyhow!("missing authenticated-a source ledger"))?;
        let b=sources.iter().find(|(source,_,_)|source==&source_b).ok_or_else(||anyhow::anyhow!("missing authenticated-b source ledger"))?;
        assert_ne!(a.1,b.1,"snapshot fingerprints remain independent of the stable source key");
        assert_eq!((a.2,b.2),(1,1));
        for (source,host) in [(&a.0,"authenticated-a"),(&b.0,"authenticated-b")] {
            let identity: (u8,String,String)=serde_json::from_str(source)?;
            assert_eq!(identity,(1,host.to_owned(),canonical.clone()));
        }
        let top=engine.top();
        let error=match engine.import_legacy_from("authenticated-a",&source_path) {Ok(())=>anyhow::bail!("changed pinned source unexpectedly imported"),Err(error)=>error};
        assert!(error.to_string().contains("changed since import began"),"{error:#}");
        assert_eq!(engine.top(),top);
        engine.import_legacy_from("authenticated-b",&source_path)?;
        engine.finish_imports()?;
        assert_eq!(engine.top(),top);
        assert_eq!(engine.id(),host_id);
        assert_eq!(std::fs::read(&source_path)?,replacement_bytes,"imports keep each host's input read-only");
        (host_id,top)
    };
    let database=Database::open(&database_path,until())?;
    let mut engine=Amphora::from_database(database.clone())?;
    engine.import_legacy_from("authenticated-b",&source_path)?;
    engine.finish_imports()?;
    assert_eq!(engine.id(),host_id);
    assert_eq!(engine.top(),top);
    assert_eq!(engine.events("event-a",None,10)?.len(),1);
    assert_eq!(engine.events("event-b",None,10)?.len(),1);
    let ledger=database.call(until(),|connection|Ok(connection.query_row("SELECT count(*) FROM convo_import WHERE table_name='__source__' AND done=1",[],|row|row.get::<_,u64>(0))?))?;
    assert_eq!(ledger,2);
    Ok(())
}
