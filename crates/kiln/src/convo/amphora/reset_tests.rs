//! U19: reset ambiguity is refused for a concrete key in either source order. Historical
//! resets stay as evidence; adoption never replays them as destructive controller commands.

use super::*;
use amphora_api::sql::OptionalExtension as _;
use crate::convo::sql as legacy_sql;

fn reset_fixture(path: &std::path::Path, has_reset: bool) -> anyhow::Result<()> {
    let connection = rusqlite::Connection::open(path)?;
    connection.execute_batch(legacy_sql::META)?;
    for migration in legacy_sql::MIGRATIONS {
        connection.execute_batch(migration)?;
    }
    let (identity, head, session_rev, event_rev) = if has_reset {
        ("reset-store", 91i64, 44i64, 45i64)
    } else {
        ("old-store", 17i64, 2i64, 3i64)
    };
    connection.execute("INSERT INTO meta VALUES ('schema',?1)", [legacy_sql::MIGRATIONS.len().to_string()])?;
    connection.execute("INSERT INTO meta VALUES ('id',?1)", [identity])?;
    connection.execute("INSERT INTO meta VALUES ('rev',?1)", [head.to_string()])?;
    connection.execute("INSERT INTO sessions(id,info,updated_at,epoch,rev) VALUES ('s','{}',1,1,?1)", [session_rev])?;
    let (id, number, at, body) = if has_reset { ("new",1i64,25i64,"new") } else { ("old",0i64,5i64,"old") };
    connection.execute(
        "INSERT INTO events(session,id,seq,at,kind,text,body,rev) VALUES ('s',?1,?2,?3,'user',?4,?4,?5)",
        rusqlite::params![id,number,at,body,event_rev],
    )?;
    if has_reset {
        connection.execute("INSERT INTO drops(rev,session,what,source,n,at) VALUES (90,'s','events',NULL,0,20)", [])?;
    }
    Ok(())
}

#[test]
fn u19_reset_overlap_refuses_per_key_in_either_source_order_without_deleting_history() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let old_path = directory.path().join("old-conversations.db");
    let reset_path = directory.path().join("reset-conversations.db");
    reset_fixture(&old_path,false)?;
    reset_fixture(&reset_path,true)?;
    let old_bytes = std::fs::read(&old_path)?;
    let reset_bytes = std::fs::read(&reset_path)?;
    let old_source = old_path.canonicalize()?.to_string_lossy().into_owned();
    let reset_source = reset_path.canonicalize()?.to_string_lossy().into_owned();

    for reset_first in [false,true] {
        let database = Database::in_memory(until())?;
        let mut engine = Amphora::from_database(database.clone())?;
        let (first,second,failed_source) = if reset_first {
            (&reset_path,&old_path,&old_source)
        } else {
            (&old_path,&reset_path,&reset_source)
        };
        engine.import_legacy(first)?;
        engine.finish_imports()?;
        let error = engine.import_legacy(second).err().ok_or_else(||anyhow::anyhow!("overlapping reset unexpectedly merged"))?;
        let collision = error.downcast_ref::<ImportCollision>().ok_or_else(||anyhow::anyhow!("expected a typed per-key collision, got {error:#}"))?;
        assert_eq!(collision.source, *failed_source);
        assert_eq!(collision.table,"events");
        assert!(collision.key.contains("old"),"{}",collision.key);
        assert!(collision.reason.contains("reset ambiguity"),"{}",collision.reason);
        assert!(engine.finish_imports().is_err(),"an ambiguous source must not publish as complete");
        let events = engine.events("s",None,10)?;
        let ids: Vec<&str> = events.iter().map(|event|event.id.as_str()).collect();
        if reset_first {
            assert_eq!(ids,vec!["new"]);
            assert_eq!(events.first().map(|event|event.body.as_str()),Some("new"));
        } else {
            assert_eq!(ids,vec!["old","new"],"reset evidence must never delete an imported event");
            assert_eq!(events.first().map(|event|event.body.as_str()),Some("old"));
        }
        let (source,old_source,reset_source) = (failed_source.clone(),old_source.clone(),reset_source.clone());
        let (done,old_history,reset_history,event_cursor,live_drops) = database.call(until(),move |connection| {
            Ok((
                connection.query_row("SELECT done FROM convo_import WHERE source=?1 AND table_name='__source__'",vec![txt(&source)],|row|row.get::<_,i64>(0))?,
                connection.query_row("SELECT source_rev FROM convo_legacy_rows WHERE source=?1 AND table_name='events' AND item='old'",vec![txt(&old_source)],|row|row.get::<_,i64>(0)).optional()?,
                connection.query_row("SELECT source_rev FROM convo_legacy_rows WHERE source=?1 AND table_name='events' AND item='new'",vec![txt(&reset_source)],|row|row.get::<_,i64>(0)).optional()?,
                connection.query_row("SELECT cursor,done FROM convo_import WHERE source=?1 AND table_name='events'",vec![txt(&source)],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?))).optional()?,
                connection.query_row("SELECT count(*) FROM convo_drops",[],|row|row.get::<_,usize>(0))?,
            ))
        })?;
        assert_eq!(done,0);
        assert_eq!(reset_history,Some(45),"reset source history retains its independent revision");
        assert_eq!(live_drops,0,"historical reset evidence must not become a live drop command");
        if reset_first {
            assert_eq!(old_history,None,"the refused event page must roll back its source history");
            assert_eq!(event_cursor,None,"the refused event page must not advance its cursor");
        } else {
            assert_eq!(old_history,Some(3));
            assert!(event_cursor.is_some_and(|(_,done)|done==1),"all reset-source rows export before reverse-order validation refuses publication");
        }
        assert_eq!(std::fs::read(&old_path)?,old_bytes,"old legacy source remains read-only");
        assert_eq!(std::fs::read(&reset_path)?,reset_bytes,"reset legacy source remains read-only");
    }
    Ok(())
}

#[test]
fn u19_reset_validation_resumes_after_128_canonical_keys_and_only_empty_page_is_done() -> anyhow::Result<()> {
    let database = Database::in_memory(until())?;
    let mut engine = Amphora::from_database(database.clone())?;
    database.call(until(),|connection| {
        // Keep the no-reset fast path disabled while every candidate is unrelated to the
        // reset. Validation must still consider only one bounded canonical metadata page.
        connection.execute("INSERT INTO convo_legacy_resets(source,source_rowid,session,what,n,at,epoch) VALUES ('reset-source',1,'another-session','events',0,20,1)",[])?;
        for number in 0..129i64 {
            connection.execute("INSERT INTO convo_events(session,id,seq,at,kind,text,body,rev) VALUES ('s',?1,?2,5,'user','text','{}',0)",vec![txt(&format!("event-{number}")),int(number)])?;
        }
        Ok(())
    })?;
    let first = engine.call(|session|session.validate_legacy_resets("reset-source","events",i64::MIN))?;
    assert_eq!(first,(128,false));
    let second = engine.call(move |session|session.validate_legacy_resets("reset-source","events",first.0))?;
    assert_eq!(second,(129,false));
    let done = engine.call(move |session|session.validate_legacy_resets("reset-source","events",second.0))?;
    assert_eq!(done,(129,true));
    Ok(())
}

#[test]
fn u19_backfilled_originless_row_checks_completed_reset_evidence() -> anyhow::Result<()> {
    let directory=tempfile::tempdir()?;
    let old_path=directory.path().join("old.db");
    let reset_path=directory.path().join("reset.db");
    reset_fixture(&old_path,false)?;
    reset_fixture(&reset_path,true)?;
    let old_source=old_path.canonicalize()?.to_string_lossy().into_owned();
    let old=crate::records::legacy::Legacy::open(&old_path)?;
    let fingerprint=old.fingerprint(LEGACY_TABLES)?;
    let database=Database::in_memory(until())?;
    let mut engine=Amphora::from_database(database.clone())?;
    // Model a canonical key adopted by the earlier single-source adapter: the source is
    // marked complete but its provenance has not yet been backfilled by schema 6.
    engine.apply(&[
        Row::Session {session:"s".into(),info:Some("{}".into()),meta:None,state:None,changes:None,epoch:Some(1),open:false,at:1},
        Row::Event(Box::new(EventRow {session:"s".into(),id:"old".into(),seq:0,at:5,parent:None,by:None,kind:"user".into(),level:None,tool:None,status:None,text:"old".into(),body:"old".into()})),
    ])?;
    let source=old_source.clone();
    database.call(until(),move |c| {
        c.execute("INSERT INTO convo_import(source,table_name,cursor,done,fingerprint) VALUES (?1,'__source__',0,1,?2)",vec![txt(&source),txt(&fingerprint)])?;
        Ok(())
    })?;
    engine.import_legacy(&reset_path)?;
    drop(old);
    let error=engine.import_legacy(&old_path).err().ok_or_else(||anyhow::anyhow!("backfill concealed a reset overlap"))?;
    let collision=error.downcast_ref::<ImportCollision>().ok_or_else(||anyhow::anyhow!("expected typed backfill collision: {error}"))?;
    assert_eq!(collision.source,old_source);
    assert_eq!(collision.table,"events");
    assert!(collision.key.contains("old"));
    assert!(collision.reason.contains("reset ambiguity"));
    assert_eq!(engine.events("s",None,10)?.iter().map(|e|e.id.as_str()).collect::<Vec<_>>(),vec!["old","new"]);
    assert!(engine.finish_imports().is_err());
    Ok(())
}
