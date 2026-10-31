//! U06/U15: full reservations survive abandoned staging and quota exhaustion. Admission
//! refuses new immutable content without reclaiming pending, acknowledged or archived bytes.
use super::*;
use crate::convo::{amphora::Amphora, rows::{RecordRow, Row}, store::Engine as _};

fn marker(content:Manifest)->anyhow::Result<Vec<u8>> {
    let mut marker=vec![CODEC];
    marker.extend_from_slice(&u64::try_from(content.size)?.to_le_bytes());
    marker.extend_from_slice(&content.digest);
    Ok(marker)
}
fn pool(database:&Database)->anyhow::Result<(u64,u64,u64,u64)> {
    let ((manifests,bytes),(parts,stored),counters)=database.call(until()?,|connection|Ok((
        connection.query_row("SELECT count(*),coalesce(sum(size),0) FROM convo_chunk_manifests",[],|row|Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?)))?,
        connection.query_row("SELECT count(*),coalesce(sum(length(value)),0) FROM convo_chunks",[],|row|Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?)))?,
        connection.query_row("SELECT bytes,manifests FROM convo_chunk_pool WHERE id=1",[],|row|Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?)))?,
    )))?;
    assert_eq!(counters,(bytes,manifests),"durable counters must match full immutable reservations");
    Ok((manifests,bytes,parts,stored))
}

fn quota_refusal(error:&anyhow::Error)->anyhow::Result<&amphora_api::Refusal> {
    error.downcast_ref::<amphora_api::Refusal>().ok_or_else(||anyhow::anyhow!("expected typed product quota refusal: {error:#}"))
}
fn publication(marker:&[u8],end:u64)->Vec<Row> {
    vec![
        Row::Session {session:"s".to_owned(),info:Some("{}".to_owned()),meta:None,state:None,changes:None,epoch:Some(1),open:true,at:1},
        Row::Record(Box::new(RecordRow {session:"s".to_owned(),source:"rollout".to_owned(),n:0,end,line:String::new(),kept:Some(marker.to_vec())})),
    ]
}

#[test]
fn u06_u15_reserved_partial_content_resumes_at_capacity_without_losing_published_or_archive_references()->anyhow::Result<()> {
    let database=Database::in_memory(until()?)?;
    let mut engine=Amphora::from_database(database.clone())?;
    // Two parts, with a UTF-8 scalar crossing the byte boundary. Full reservation is much
    // larger than the first committed part, so partial physical size cannot admit new bytes.
    let line="界".repeat(PART_BYTES/3+1);
    let archived_line="archive-only 界";
    let content=Manifest {size:line.len(),parts:line.len().div_ceil(PART_BYTES),digest:Sha256::digest(line.as_bytes()).into()};
    let bytes=u64::try_from(line.len())?.checked_add(u64::try_from(archived_line.len())?).ok_or_else(||anyhow::anyhow!("fixture reservation overflow"))?;
    let quota=Quota {bytes,manifests:2};
    let first=line.as_bytes().get(..PART_BYTES).ok_or_else(||anyhow::anyhow!("fixture has no complete first part"))?.to_vec();
    database.call(until()?,move |connection| {
        reserve(connection,content,quota)?;
        connection.execute("INSERT INTO convo_chunks(digest,part,value) VALUES (?1,0,?2)",params![content.digest,first])?;
        Ok(())
    })?;
    let pending=marker(content)?;
    let archive_marker=stage_with_quota(&database,archived_line,quota)?;
    let reserved=pool(&database)?;
    assert_eq!(reserved.0,2);
    assert_eq!(reserved.1,bytes);
    assert!(reserved.3<bytes,"pending content reserves bytes that have not been staged yet");

    // Model damaged/premature readiness: publication must check the part shape atomically,
    // rather than trusting ready=1 while a required part is absent.
    database.call(until()?,move |connection| {
        connection.execute("UPDATE convo_chunk_manifests SET ready=1 WHERE digest=?1",params![content.digest])?;
        Ok(())
    })?;
    let incomplete=pending.clone();
    assert!(database.call(until()?,move |connection|validate(connection,&incomplete)).is_err());
    let rows=publication(&pending,u64::try_from(line.len())?);
    assert!(engine.apply(&rows).is_err());
    assert_eq!(engine.top(),0);
    let unpublished=database.call(until()?,|connection|Ok((
        connection.query_row("SELECT count(*) FROM convo_sessions",[],|row|row.get::<_,u64>(0))?,
        connection.query_row("SELECT count(*) FROM convo_records",[],|row|row.get::<_,u64>(0))?,
    )))?;
    assert_eq!(unpublished,(0,0),"failed reference validation rolls back earlier writes in the batch");

    assert_eq!(stage_with_quota(&database,&line,quota)?,pending);
    engine.apply(&rows)?;
    let archive=postcard::to_allocvec(&vec![amphora_api::Value::Blob(archive_marker.clone())])?;
    database.call(until()?,move |connection| {
        connection.execute("INSERT INTO convo_legacy_rows(source,table_name,source_rowid,session,item,number,epoch,source_rev,payload) VALUES ('archive-source','records',1,'archived','rollout',0,0,1,?1)",params![archive])?;
        Ok(())
    })?;
    let complete=pool(&database)?;
    assert_eq!(complete,(2,bytes,3,bytes));
    let byte_error=stage_with_quota(&database,"new distinct content",Quota {bytes,manifests:3}).err().ok_or_else(||anyhow::anyhow!("new content exceeded the full reservation quota"))?;
    assert!(matches!(quota_refusal(&byte_error)?,amphora_api::Refusal::TooLarge {..}));
    let count_error=stage_with_quota(&database,"",Quota {bytes:bytes.checked_add(1024).ok_or_else(||anyhow::anyhow!("fixture quota overflow"))?,manifests:2}).err().ok_or_else(||anyhow::anyhow!("zero-byte content exceeded the manifest quota"))?;
    assert!(matches!(quota_refusal(&count_error)?,amphora_api::Refusal::Full {limit:2}));
    assert_eq!(pool(&database)?,complete,"refusals leave every manifest, part and reservation intact");

    // A reduced budget is an admission policy, not permission to strand old content.
    assert_eq!(stage_with_quota(&database,&line,Quota {bytes:0,manifests:0})?,pending);
    assert_eq!(stage_with_quota(&database,archived_line,Quota {bytes:0,manifests:0})?,archive_marker);
    assert_eq!(pool(&database)?,complete,"retries never consume another reservation");
    assert_eq!(engine.record("s","rollout",0,10)?,vec![(0,line)]);
    let archive_only=database.call(until()?,|connection| {
        let payload=connection.query_row("SELECT payload FROM convo_legacy_rows WHERE source='archive-source'",[],|row|row.get::<_,Vec<u8>>(0))?;
        let values:Vec<amphora_api::Value>=postcard::from_bytes(&payload)?;
        let Some(amphora_api::Value::Blob(marker))=values.first() else {anyhow::bail!("archive lost its embedded marker")};
        let live=connection.query_row("SELECT count(*) FROM convo_records WHERE line=?1",params![marker],|row|row.get::<_,u64>(0))?;
        anyhow::ensure!(live==0,"archive-only content unexpectedly has a live transcript reference");
        read(connection,marker)
    })?;
    assert_eq!(archive_only,archived_line);
    Ok(())
}

#[test]
fn u06_u15_manifest_quota_bounds_empty_and_tiny_content_while_existing_empty_content_retries_free()->anyhow::Result<()> {
    let directory=tempfile::tempdir()?;
    let path=directory.path().join("amphora.db");
    let partial=Manifest {size:5,parts:1,digest:Sha256::digest(b"later").into()};
    let empty={
        let database=Database::open(&path,until()?)?;
        let _engine=Amphora::from_database(database.clone())?;
        let empty=stage_with_quota(&database,"",Quota {bytes:0,manifests:1})?;
        assert_eq!(pool(&database)?,(1,0,0,0));
        assert_eq!(stage_with_quota(&database,"",Quota {bytes:0,manifests:0})?,empty);
        let error=stage_with_quota(&database,"x",Quota {bytes:1024,manifests:1}).err().ok_or_else(||anyhow::anyhow!("tiny new digest exceeded the manifest quota"))?;
        assert!(matches!(quota_refusal(&error)?,amphora_api::Refusal::Full {limit:1}));
        assert_eq!(pool(&database)?,(1,0,0,0));
        database.call(until()?,move |connection|reserve(connection,partial,Quota {bytes:5,manifests:2}))?;
        assert_eq!(pool(&database)?,(2,5,0,0));
        // Model an existing pre-quota schema: only immutable manifest metadata survives.
        // The next canonical initialization must seed its counters exactly once.
        database.call(until()?,|connection| {
            connection.execute_batch("DROP TABLE convo_chunk_pool; UPDATE convo_meta SET value='6' WHERE key='schema';")?;
            Ok(())
        })?;
        empty
    };
    {
        let database=Database::open(&path,until()?)?;
        let _engine=Amphora::from_database(database.clone())?;
        assert_eq!(pool(&database)?,(2,5,0,0),"migration seeds full declared bytes of partial content, not just stored parts");
        stage_with_quota(&database,"x",Quota {bytes:6,manifests:3})?;
        assert_eq!(pool(&database)?,(3,6,1,1));
    }
    let database=Database::open(&path,until()?)?;
    let _engine=Amphora::from_database(database.clone())?;
    assert_eq!(pool(&database)?,(3,6,1,1),"later startup retains reservations admitted after migration");
    assert_eq!(stage_with_quota(&database,"",Quota {bytes:0,manifests:0})?,empty);
    assert_eq!(stage_with_quota(&database,"later",Quota {bytes:0,manifests:0})?,marker(partial)?);
    let error=stage_with_quota(&database,"y",Quota {bytes:1024,manifests:3}).err().ok_or_else(||anyhow::anyhow!("reopen reset the durable manifest quota"))?;
    assert!(matches!(quota_refusal(&error)?,amphora_api::Refusal::Full {limit:3}));
    assert_eq!(pool(&database)?,(3,6,2,6));
    assert_eq!(database.call(until()?,move |connection|read(connection,&empty))?,"");
    Ok(())
}
