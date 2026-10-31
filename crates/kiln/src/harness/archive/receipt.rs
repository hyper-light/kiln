//! The existing archive loop's SQL receipts. No controller or database is opened here.
use crate::{convo::retention::{Authority, MaintenanceRefusal}, hub::Hub, sql::{Connection, OptionalExtension as _, params}};

#[derive(Clone,Debug)]
pub(super) struct Capture {
    pub(super) id: String,
    pub(super) session: String,
    pub(super) close_rev: i64,
    pub(super) epoch: i64,
    pub(super) accepted_rev: i64,
    pub(super) history_rev: i64,
    pub(super) authority: Authority,
}
#[derive(Clone,Debug)]
pub(super) struct Source {
    pub(super) source: String,
    pub(super) incarnation: i64,
    pub(super) accepted_rev: i64,
    pub(super) frontier: Option<i64>,
    pub(super) base: Option<String>,
    pub(super) dirty_from: Option<i64>,
    pub(super) complete: bool,
}

impl Capture {
    pub(super) fn check(&self, conn: &Connection<'_>) -> anyhow::Result<()> {
        self.authority.check(conn,true)?;
        let current: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM convo_sessions s
            JOIN convo_retention v ON v.session=s.id JOIN transcript_archives a ON a.session=s.id
            JOIN transcript_manifests m ON m.capture=a.manifest
            WHERE s.id=?1 AND s.closed_at IS NOT NULL AND s.closed_at=v.close_at AND s.epoch=?2 AND v.close_epoch=?2
            AND v.close_rev=?3 AND v.accepted_rev=?4 AND v.history_rev=?8 AND a.close_rev=?3 AND a.accepted_rev=?4
            AND a.manifest=?5 AND a.status='pending' AND a.owner_generation=?6 AND a.policy_version=?7
            AND m.state='pending' AND m.session=s.id AND m.close_rev=?3 AND m.close_epoch=?2 AND m.accepted_rev=?4
            AND m.owner_generation=?6 AND m.policy_version=?7 AND m.history_rev=?8)",
            params![self.session,self.epoch,self.close_rev,self.accepted_rev,self.id,self.authority.generation,self.authority.policy_version,self.history_rev],|r|r.get(0))?;
        if !current { return Err(MaintenanceRefusal::Stale.into()); }
        Ok(())
    }
}

pub(super) async fn begin(hub: &Hub, authority: Authority, session: &str) -> anyhow::Result<Capture> {
    authority.require_store(&hub.store)?;
    let session=session.to_owned();
    let candidate=crate::util::new_id();
    let capture=hub.store.call(move |conn| {
        authority.check(conn,true)?;
        let (close_rev,epoch,accepted_rev,history_rev): (i64,i64,i64,i64)=conn.query_row("SELECT v.close_rev,v.close_epoch,v.accepted_rev,v.history_rev FROM convo_retention v JOIN convo_sessions s ON s.id=v.session
            WHERE v.session=?1 AND s.closed_at IS NOT NULL AND v.close_rev IS NOT NULL AND v.close_epoch=s.epoch AND v.close_at=s.closed_at",[&session],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let existing: Option<String>=conn.query_row("SELECT a.manifest FROM transcript_archives a JOIN transcript_manifests m ON m.capture=a.manifest
            WHERE a.session=?1 AND a.close_rev=?2 AND a.accepted_rev=?3 AND a.status='pending' AND m.state='pending' AND m.owner_generation=?4 AND m.policy_version=?5 AND m.session=a.session AND m.close_rev=?2 AND m.accepted_rev=?3 AND m.close_epoch=?6 AND m.history_rev=?7",
            params![session,close_rev,accepted_rev,authority.generation,authority.policy_version,epoch,history_rev],|r|r.get(0)).optional()?;
        let id=existing.unwrap_or(candidate);
        conn.execute("INSERT INTO transcript_manifests(capture,session,close_rev,close_epoch,accepted_rev,owner_generation,policy_version,state,created_ms,history_rev) VALUES (?1,?2,?3,?4,?5,?6,?7,'pending',?8,?9) ON CONFLICT(capture) DO NOTHING",
            params![id,session,close_rev,epoch,accepted_rev,authority.generation,authority.policy_version,crate::util::now_ms(),history_rev])?;
        let n=conn.execute("UPDATE transcript_archives SET manifest=?2,status='pending',done_ms=NULL,owner_generation=?3,policy_version=?4 WHERE session=?1 AND close_rev=?5 AND accepted_rev=?6",
            params![session,id,authority.generation,authority.policy_version,close_rev,accepted_rev])?;
        anyhow::ensure!(n==1,"archive obligation changed before capture");
        let capture=Capture {id,session,close_rev,epoch,accepted_rev,history_rev,authority};
        capture.check(conn)?;
        Ok(capture)
    }).await?;
    // Resume an exact partially populated catalog. Each page is a separate,
    // bounded writer turn; the accepted-version fence makes it one snapshot.
    for _ in 0..256 {
        let c=capture.clone();
        let complete=hub.store.call(move |conn| {
            c.check(conn)?;
            let already: bool=conn.query_row("SELECT catalog_complete FROM transcript_manifests WHERE capture=?1",[&c.id],|r|r.get(0))?;
            if already { return Ok(true); }
            let after: Option<String>=conn.query_row("SELECT max(source) FROM transcript_manifest_sources WHERE capture=?1",[&c.id],|r|r.get(0))?;
            let mut q=if after.is_some() {
                conn.prepare("SELECT source,incarnation,accepted_rev,frontier,base_capture,dirty_from FROM convo_record_sources WHERE session=?1 AND source>?2 ORDER BY source LIMIT 128")?
            }else{
                conn.prepare("SELECT source,incarnation,accepted_rev,frontier,base_capture,dirty_from FROM convo_record_sources WHERE session=?1 ORDER BY source LIMIT 128")?
            };
            let rows=if let Some(after)=after {
                q.query_map(params![c.session,after],source_row)?.collect::<crate::sql::Result<Vec<_>>>()?
            }else{
                q.query_map(params![c.session],source_row)?.collect::<crate::sql::Result<Vec<_>>>()?
            };
            for source in rows.iter().cloned() {
                let mut source=source;
                if source.accepted_rev<=c.history_rev {
                    // Expiry revoked only archive history. A keep-forever
                    // canonical policy may still hold rows; they remain the
                    // source of truth for a future accepted snapshot.
                    source.frontier=conn.query_row("SELECT max(n) FROM convo_records WHERE session=?1 AND source=?2",params![c.session,source.source],|r|r.get(0))?;
                    source.base=None;source.dirty_from=Some(0);
                }
                let sealed=conn.query_row("SELECT v.last_complete_capture FROM convo_retention v JOIN transcript_manifests m ON m.capture=v.last_complete_capture AND m.state='complete' AND m.history_rev=v.history_rev
                    JOIN transcript_manifest_sources x ON x.capture=m.capture AND x.source=?2 AND x.incarnation=?3 AND x.accepted_rev=?4 AND x.complete=1 WHERE v.session=?1",params![c.session,source.source,source.incarnation,source.accepted_rev],|r|r.get::<_,String>(0)).optional()?;
                if let Some(sealed)=sealed {
                    source.base=Some(sealed);source.dirty_from=None;
                    // Adopt only an already-positive source version, in this
                    // bounded catalog page. An unfinished capture clears nothing.
                    conn.execute("UPDATE convo_record_sources SET base_capture=?3,dirty_from=NULL WHERE session=?1 AND source=?2 AND incarnation=?4 AND accepted_rev=?5",params![c.session,source.source,source.base,source.incarnation,source.accepted_rev])?;
                }
                conn.execute("INSERT INTO transcript_manifest_sources(capture,source,incarnation,accepted_rev,frontier,rows,base_capture,dirty_from) VALUES (?1,?2,?3,?4,?5,0,?6,?7)",
                    params![c.id,source.source,source.incarnation,source.accepted_rev,source.frontier,source.base,source.dirty_from])?;
            }
            if rows.len()<128 {
                conn.execute("UPDATE transcript_manifests SET catalog_complete=1 WHERE capture=?1 AND state='pending'",[&c.id])?;
                Ok(true)
            }else{Ok(false)}
        }).await?;
        if complete { return Ok(capture); }
    }
    Err(super::More.into())
}

fn source_row(row: &crate::sql::Row<'_>) -> crate::sql::Result<Source> {
    Ok(Source {source:row.get(0)?,incarnation:row.get(1)?,accepted_rev:row.get(2)?,frontier:row.get(3)?,base:row.get(4)?,dirty_from:row.get(5)?,complete:false})
}

pub(super) async fn sources(hub: &Hub, capture: &Capture, after: Option<String>) -> anyhow::Result<Vec<Source>> {
    capture.authority.require_store(&hub.store)?;
    let c=capture.clone();
    hub.store.call(move |conn| {
        c.check(conn)?;
        let mut q=if after.is_some() {
            conn.prepare("SELECT source,incarnation,accepted_rev,frontier,base_capture,dirty_from,complete FROM transcript_manifest_sources WHERE capture=?1 AND source>?2 ORDER BY source LIMIT 128")?
        }else{
            conn.prepare("SELECT source,incarnation,accepted_rev,frontier,base_capture,dirty_from,complete FROM transcript_manifest_sources WHERE capture=?1 ORDER BY source LIMIT 128")?
        };
        let read=|r:&crate::sql::Row<'_>| {let mut s=source_row(r)?;s.complete=r.get(6)?;Ok(s)};
        let rows=if let Some(after)=after {q.query_map(params![c.id,after],read)?.collect::<crate::sql::Result<Vec<_>>>()?}else{q.query_map(params![c.id],read)?.collect::<crate::sql::Result<Vec<_>>>()?};
        Ok(rows)
    }).await
}

pub(super) async fn validate(hub: &Hub, capture: &Capture) -> anyhow::Result<()> {
    capture.authority.require_store(&hub.store)?;
    let c=capture.clone();hub.store.call(move |conn|c.check(conn)).await
}

pub(super) async fn source_done(hub: &Hub, capture: &Capture, source: &Source, rows: u64, last: Option<u64>) -> anyhow::Result<()> {
    capture.authority.require_store(&hub.store)?;
    let (c,s)=(capture.clone(),source.clone());
    hub.store.call(move |conn| {
        c.check(conn)?;
        let current: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM convo_record_sources WHERE session=?1 AND source=?2 AND incarnation=?3 AND accepted_rev=?4)",params![c.session,s.source,s.incarnation,s.accepted_rev],|r|r.get(0))?;
        anyhow::ensure!(current,"archive source incarnation changed");
        let last=last.map(i64::try_from).transpose()?;
        anyhow::ensure!(last==s.frontier,"archive did not reach the captured source frontier");
        let n=conn.execute("UPDATE transcript_manifest_sources SET complete=1 WHERE capture=?1 AND source=?2 AND incarnation=?4 AND accepted_rev=?5 AND rows=?3 AND progress_last IS ?6",params![c.id,s.source,i64::try_from(rows)?,s.incarnation,s.accepted_rev,last])?;
        anyhow::ensure!(n==1,"archive progress did not match its confirmed piece references");
        Ok(())
    }).await
}

pub(super) async fn finish(hub: &Hub, capture: &Capture) -> anyhow::Result<()> {
    capture.authority.require_store(&hub.store)?;
    let c=capture.clone();
    hub.store.call(move |conn| {
        c.check(conn)?;
        let catalog: bool=conn.query_row("SELECT catalog_complete FROM transcript_manifests WHERE capture=?1",[&c.id],|r|r.get(0))?;
        let incomplete: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM transcript_manifest_sources WHERE capture=?1 AND complete=0)",[&c.id],|r|r.get(0))?;
        // Every reference was admitted Ready in the same transaction that
        // advanced its prefix. The active capture and then complete history
        // protect it from collection in that same SQL owner. No full piece scan
        // or source-catalog rewrite is needed at final publication.
        anyhow::ensure!(catalog && !incomplete,"archive capture is incomplete");
        let now=crate::util::now_ms();
        conn.execute("UPDATE transcript_manifests SET state='complete',completed_ms=?2 WHERE capture=?1 AND state='pending'",params![c.id,now])?;
        let n=conn.execute("UPDATE transcript_archives SET status='archived',done_ms=?2 WHERE session=?1 AND manifest=?3 AND close_rev=?4 AND accepted_rev=?5 AND owner_generation=?6 AND policy_version=?7 AND status='pending'",params![c.session,now,c.id,c.close_rev,c.accepted_rev,c.authority.generation,c.authority.policy_version])?;
        anyhow::ensure!(n==1,"archive completion lost its exact obligation");
        // One publication. Per-source positive-base adoption is lazy and
        // bounded; finishing a huge source catalog never rewrites all its rows.
        let n=conn.execute("UPDATE convo_retention SET last_complete_capture=?2 WHERE session=?1 AND close_rev=?3 AND accepted_rev=?4",params![c.session,c.id,c.close_rev,c.accepted_rev])?;
        anyhow::ensure!(n==1,"archive publication lost its accepted identity");
        conn.execute("UPDATE conversation_maintenance SET prune_again=1 WHERE id=1",[])?;
        Ok(())
    }).await
}


/// Sixteen maximal row-index vectors stay well below RESULT_MAX=32 MiB.
pub(super) const BASE_PAGE:usize=16;
#[derive(Clone,Debug)]
pub(super) struct StoredPiece {
    pub(super) piece: super::Piece,
    pub(super) indices: Vec<u64>,
}

pub(super) async fn base_page(hub: &Hub, capture: &Capture, source: &Source, after: Option<i64>) -> anyhow::Result<Vec<StoredPiece>> {
    capture.authority.require_store(&hub.store)?;
    let (c,s)=(capture.clone(),source.clone());
    let Some(base)=s.base else {return Ok(Vec::new())};
    hub.store.call(move |conn| {
        c.check(conn)?;
        let positive: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM transcript_manifests m JOIN transcript_manifest_sources x ON x.capture=m.capture WHERE m.capture=?1 AND m.state='complete' AND m.session=?2 AND x.source=?3 AND x.incarnation=?4 AND x.complete=1 AND m.history_rev=?5)",params![base,c.session,s.source,s.incarnation,c.history_rev],|r|r.get(0))?;
        anyhow::ensure!(positive,"archive source prefix has no positive immutable manifest");
        let mut q=if after.is_some() {
            conn.prepare("SELECT p.source,p.first,p.last,p.key,p.lines,p.bytes,p.raw,p.incarnation,p.codec,p.sha256,p.indices,p.state FROM transcript_manifest_pieces r JOIN transcript_segments p ON p.key=r.key WHERE r.capture=?1 AND r.source=?2 AND r.first>?3 ORDER BY r.first LIMIT ?4")?
        }else{
            conn.prepare("SELECT p.source,p.first,p.last,p.key,p.lines,p.bytes,p.raw,p.incarnation,p.codec,p.sha256,p.indices,p.state FROM transcript_manifest_pieces r JOIN transcript_segments p ON p.key=r.key WHERE r.capture=?1 AND r.source=?2 ORDER BY r.first LIMIT ?3")?
        };
        let read=|r:&crate::sql::Row<'_>|Ok((super::piece_row(r)?,r.get::<_,Vec<u8>>(10)?,r.get::<_,String>(11)?));
        let rows=if let Some(after)=after {q.query_map(params![base,s.source,after,BASE_PAGE],read)?.collect::<crate::sql::Result<Vec<_>>>()?}else{q.query_map(params![base,s.source,BASE_PAGE],read)?.collect::<crate::sql::Result<Vec<_>>>()?};
        rows.into_iter().map(|(piece,indices,state)| {
            anyhow::ensure!(state=="ready" && piece.incarnation==Some(s.incarnation),"archive prefix is collecting or has another incarnation");
            let indices=postcard::from_bytes::<Vec<u64>>(&indices)?;
            anyhow::ensure!(u64::try_from(indices.len())?==piece.lines && indices.first()==Some(&piece.first) && indices.last()==Some(&piece.last),"archive prefix index differs from its immutable range");
            Ok(StoredPiece {piece,indices})
        }).collect()
    }).await
}

fn add_reference(conn: &Connection<'_>, capture: &Capture, piece: &super::Piece) -> anyhow::Result<()> {
    capture.check(conn)?;
    let ready: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM transcript_segments p JOIN transcript_manifests origin ON origin.capture=p.capture WHERE p.key=?1 AND p.state='ready' AND p.session=?2 AND p.source=?3 AND p.incarnation=?4 AND p.first=?5 AND p.last=?6 AND p.lines=?7 AND p.bytes=?8 AND p.raw=?9 AND p.sha256=?10 AND origin.history_rev=?11)",
        params![piece.key,capture.session,piece.source,piece.incarnation,i64::try_from(piece.first)?,i64::try_from(piece.last)?,i64::try_from(piece.lines)?,i64::try_from(piece.bytes)?,i64::try_from(piece.raw)?,piece.sha256,capture.history_rev],|r|r.get(0))?;
    anyhow::ensure!(ready,"immutable archive piece has no ready receipt");
    let prior: Option<String>=conn.query_row("SELECT key FROM transcript_manifest_pieces WHERE capture=?1 AND source=?2 AND first=?3",params![capture.id,piece.source,i64::try_from(piece.first)?],|r|r.get(0)).optional()?;
    if let Some(prior)=prior {
        anyhow::ensure!(prior==piece.key,"archive range already has different immutable content");
        return Ok(());
    }
    let (rows,last):(i64,Option<i64>)=conn.query_row("SELECT rows,progress_last FROM transcript_manifest_sources WHERE capture=?1 AND source=?2 AND complete=0",params![capture.id,piece.source],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let first=i64::try_from(piece.first)?;
    anyhow::ensure!(last.is_none_or(|last|first>last),"immutable archive ranges are not ascending");
    let rows=rows.checked_add(i64::try_from(piece.lines)?).ok_or_else(||anyhow::anyhow!("archive source row count overflow"))?;
    // Reference admission and Ready/Collecting are one writer transaction.
    conn.execute("INSERT INTO transcript_manifest_pieces(capture,key,source,first) VALUES (?1,?2,?3,?4)",params![capture.id,piece.key,piece.source,first])?;
    conn.execute("UPDATE transcript_manifest_sources SET rows=?3,progress_last=?4 WHERE capture=?1 AND source=?2 AND complete=0",params![capture.id,piece.source,rows,i64::try_from(piece.last)?])?;
    Ok(())
}

pub(super) async fn reuse(hub: &Hub, capture: &Capture, piece: &StoredPiece) -> anyhow::Result<()> {
    capture.authority.require_store(&hub.store)?;
    let (c,p)=(capture.clone(),piece.piece.clone());
    hub.store.call(move |conn|add_reference(conn,&c,&p)).await
}

/// Persist the exact external object operation before issuing it. Unknown puts
/// remain Putting, cannot authorize deletion, and are not timed out into absence.
pub(super) enum PieceAdmission { New, Ready, Unresolved }
pub(super) async fn prepare_piece(hub: &Hub, capture: &Capture, piece: &StoredPiece) -> anyhow::Result<PieceAdmission> {
    capture.authority.require_store(&hub.store)?;
    let (c,p)=(capture.clone(),piece.clone());
    hub.store.call(move |conn| {
        c.check(conn)?;
        let b=&p.piece;
        let indices=postcard::to_allocvec(&p.indices)?;
        let inserted=conn.execute("INSERT INTO transcript_segments(key,session,source,first,last,codec,lines,bytes,raw,written_ms,capture,incarnation,sha256,indices,state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'putting') ON CONFLICT(key) DO NOTHING",
            params![b.key,c.session,b.source,i64::try_from(b.first)?,i64::try_from(b.last)?,b.codec,i64::try_from(b.lines)?,i64::try_from(b.bytes)?,i64::try_from(b.raw)?,crate::util::now_ms(),c.id,b.incarnation,b.sha256,indices])?;
        let (state,same):(String,bool)=conn.query_row("SELECT state,session=?2 AND source=?3 AND first=?4 AND last=?5 AND codec=?6 AND lines=?7 AND bytes=?8 AND raw=?9 AND capture=?10 AND incarnation=?11 AND sha256=?12 AND indices=?13 FROM transcript_segments WHERE key=?1",
            params![b.key,c.session,b.source,i64::try_from(b.first)?,i64::try_from(b.last)?,b.codec,i64::try_from(b.lines)?,i64::try_from(b.bytes)?,i64::try_from(b.raw)?,c.id,b.incarnation,b.sha256,indices],|r|Ok((r.get(0)?,r.get(1)?)))?;
        anyhow::ensure!(same && matches!(state.as_str(),"putting"|"ready"),"archive object identity is collecting or differs from its planned bytes");
        Ok(if state=="ready" {PieceAdmission::Ready}else if inserted==1 {PieceAdmission::New}else{PieceAdmission::Unresolved})
    }).await
}

pub(super) async fn piece_ready(hub: &Hub, capture: &Capture, piece: &StoredPiece) -> anyhow::Result<()> {
    capture.authority.require_store(&hub.store)?;
    let (c,p)=(capture.clone(),piece.piece.clone());
    hub.store.call(move |conn| {
        c.check(conn)?;
        let n=conn.execute("UPDATE transcript_segments SET state='ready' WHERE key=?1 AND capture=?2 AND state IN ('putting','ready') AND sha256=?3",params![p.key,c.id,p.sha256])?;
        anyhow::ensure!(n==1,"archive object receipt lost its original capture");
        add_reference(conn,&c,&p)
    }).await
}


/// Release only the active logical pointer. The old capture retains its exact
/// unresolved external operation; no HEAD, timeout or new owner clears it.
pub(super) async fn abandon(hub:&Hub,capture:&Capture)->anyhow::Result<()> {
    capture.authority.require_store(&hub.store)?;
    let c=capture.clone();
    hub.store.call(move |conn| {
        c.authority.check(conn,true)?;
        conn.execute("UPDATE transcript_archives SET manifest=NULL WHERE session=?1 AND manifest=?2 AND status='pending' AND close_rev=?3 AND accepted_rev=?4 AND owner_generation=?5 AND policy_version=?6",
            params![c.session,c.id,c.close_rev,c.accepted_rev,c.authority.generation,c.authority.policy_version])?;
        Ok(())
    }).await
}

pub(super) async fn progress(hub: &Hub, capture: &Capture, source: &Source) -> anyhow::Result<(u64,Option<u64>)> {
    capture.authority.require_store(&hub.store)?;
    let (c,s)=(capture.clone(),source.clone());
    hub.store.call(move |conn| {
        c.check(conn)?;
        let (rows,last):(i64,Option<i64>)=conn.query_row("SELECT rows,progress_last FROM transcript_manifest_sources WHERE capture=?1 AND source=?2",params![c.id,s.source],|r|Ok((r.get(0)?,r.get(1)?)))?;
        Ok((u64::try_from(rows)?,last.map(u64::try_from).transpose()?))
    }).await
}


// The limit applies to indexed candidates, before any joined disposition or
// canonical-row test. Rejected candidates still advance a durable cursor. Each
// finite horizon is exhausted before a later sweep admits arrivals behind it.
type Position = (i64,String);
#[derive(Default,serde::Serialize,serde::Deserialize)]
struct Sweep<K> { after: Option<K>, horizon: Option<K> }
#[derive(Default,serde::Serialize,serde::Deserialize)]
struct Selections {
    expire: Sweep<Position>,
    give_up: Sweep<Position>,
    due: Sweep<Position>,
    generation: Sweep<String>,
    #[serde(default)]
    generation_done: bool,
}
impl Selections {
    fn read(conn:&Connection<'_>)->anyhow::Result<Self> {
        let value:Option<String>=conn.query_row("SELECT archive_scan FROM conversation_maintenance WHERE id=1",[],|r|r.get(0))?;
        Ok(value.map(|v|serde_json::from_str(&v)).transpose()?.unwrap_or_default())
    }
    fn save(&self,conn:&Connection<'_>)->anyhow::Result<()> {
        conn.execute("UPDATE conversation_maintenance SET archive_scan=?1 WHERE id=1",[serde_json::to_string(self)?])?;
        Ok(())
    }
}

fn observed_page(conn:&Connection<'_>,sweep:&mut Sweep<Position>,before:i64)->anyhow::Result<Vec<String>> {
    let horizon=if let Some(horizon)=sweep.horizon.clone() {Some(horizon)}else{
        conn.query_row("SELECT observed_ms,session FROM convo_retention WHERE observed_ms<?1 ORDER BY observed_ms DESC,session DESC LIMIT 1",[before],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).optional()?
    };
    let Some(horizon)=horizon else {sweep.after=None;sweep.horizon=None;return Ok(Vec::new())};
    let read=|r:&crate::sql::Row<'_>|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?));
    let rows=if let Some(after)=&sweep.after {
        let mut q=conn.prepare("SELECT observed_ms,session FROM convo_retention WHERE (observed_ms,session)>(?1,?2) AND (observed_ms,session)<=(?3,?4) AND observed_ms<?5 ORDER BY observed_ms,session LIMIT ?6")?;
        q.query_map(params![after.0,after.1,horizon.0,horizon.1,before,super::LET_GO_BATCH],read)?.collect::<crate::sql::Result<Vec<_>>>()?
    }else{
        let mut q=conn.prepare("SELECT observed_ms,session FROM convo_retention WHERE (observed_ms,session)<=(?1,?2) AND observed_ms<?3 ORDER BY observed_ms,session LIMIT ?4")?;
        q.query_map(params![horizon.0,horizon.1,before,super::LET_GO_BATCH],read)?.collect::<crate::sql::Result<Vec<_>>>()?
    };
    if rows.len()<usize::try_from(super::LET_GO_BATCH)? || rows.last()==Some(&horizon) {sweep.after=None;sweep.horizon=None;}else if let Some(last)=rows.last() {sweep.after=Some(last.clone());sweep.horizon=Some(horizon);}
    Ok(rows.into_iter().map(|(_,session)|session).collect())
}

pub(super) fn give_up_candidates(conn:&Connection<'_>,before:i64)->anyhow::Result<Vec<String>> {
    let mut selections=Selections::read(conn)?;
    let rows=observed_page(conn,&mut selections.give_up,before)?;
    selections.save(conn)?;
    Ok(rows)
}

fn generation_page(conn:&Connection<'_>,sweep:&mut Sweep<String>)->anyhow::Result<Vec<String>> {
    let horizon=if let Some(horizon)=sweep.horizon.clone() {Some(horizon)}else{
        conn.query_row("SELECT max(session) FROM transcript_archives",[],|r|r.get::<_,Option<String>>(0))?
    };
    let Some(horizon)=horizon else {sweep.after=None;sweep.horizon=None;return Ok(Vec::new())};
    let rows=if let Some(after)=&sweep.after {
        let mut q=conn.prepare("SELECT session FROM transcript_archives WHERE session>?1 AND session<=?2 ORDER BY session LIMIT 32")?;
        q.query_map(params![after,horizon],|r|r.get::<_,String>(0))?.collect::<crate::sql::Result<Vec<_>>>()?
    }else{
        let mut q=conn.prepare("SELECT session FROM transcript_archives WHERE session<=?1 ORDER BY session LIMIT 32")?;
        q.query_map([&horizon],|r|r.get::<_,String>(0))?.collect::<crate::sql::Result<Vec<_>>>()?
    };
    if rows.len()<32 || rows.last()==Some(&horizon) {sweep.after=None;sweep.horizon=None;}else if let Some(last)=rows.last() {sweep.after=Some(last.clone());sweep.horizon=Some(horizon);}
    Ok(rows)
}

fn pending_page(conn:&Connection<'_>,sweep:&mut Sweep<Position>,now:i64)->anyhow::Result<Vec<String>> {
    // status is the equality prefix of transcript_archives_pending_due. Include
    // it in every range/order tuple so the whole leading index key can seek.
    let horizon=if let Some(horizon)=sweep.horizon.clone() {Some(horizon)}else{
        conn.query_row("SELECT due_ms,session FROM transcript_archives WHERE status='pending' AND due_ms<=?1 ORDER BY status DESC,due_ms DESC,session DESC LIMIT 1",[now],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).optional()?
    };
    let Some(horizon)=horizon else {sweep.after=None;sweep.horizon=None;return Ok(Vec::new())};
    let read=|r:&crate::sql::Row<'_>|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?));
    let rows=if let Some(after)=&sweep.after {
        let mut q=conn.prepare("SELECT due_ms,session FROM transcript_archives WHERE status='pending' AND (status,due_ms,session)>(?1,?2,?3) AND (status,due_ms,session)<=(?1,?4,?5) AND due_ms<=?6 ORDER BY status,due_ms,session LIMIT ?7")?;
        q.query_map(params!["pending",after.0,after.1,horizon.0,horizon.1,now,super::BATCH],read)?.collect::<crate::sql::Result<Vec<_>>>()?
    }else{
        let mut q=conn.prepare("SELECT due_ms,session FROM transcript_archives WHERE status='pending' AND (status,due_ms,session)<=(?1,?2,?3) AND due_ms<=?4 ORDER BY status,due_ms,session LIMIT ?5")?;
        q.query_map(params!["pending",horizon.0,horizon.1,now,super::BATCH],read)?.collect::<crate::sql::Result<Vec<_>>>()?
    };
    if rows.len()<usize::try_from(super::BATCH)? || rows.last()==Some(&horizon) {sweep.after=None;sweep.horizon=None;}else if let Some(last)=rows.last() {sweep.after=Some(last.clone());sweep.horizon=Some(horizon);}
    Ok(rows.into_iter().map(|(_,session)|session).collect())
}

/// The existing archive pass inspects 32 generation candidates and at most eight
/// indexed pending candidates. No OR over archived history precedes the limit.
/// Caller checks the retained owner and commits these cursors in this same TX.
pub(super) fn due(conn:&Connection<'_>,owner:&Authority,now:i64)->anyhow::Result<Vec<(String,i64,i64)>> {
    let mut selections=Selections::read(conn)?;
    let generation=if selections.generation_done {Vec::new()}else{
        let rows=generation_page(conn,&mut selections.generation)?;
        if selections.generation.after.is_none() && selections.generation.horizon.is_none() {selections.generation_done=true;}
        rows
    };
    for session in generation {
        // Revalidation merely revokes a stale disposition and queues its exact
        // obligation. It grants no new receipt, failure proof or archive attempt.
        conn.execute("UPDATE transcript_archives SET status='pending',done_ms=NULL,manifest=NULL,failure_ms=NULL
            WHERE session=?1 AND status IN ('archived','given-up') AND (owner_generation IS NOT ?2 OR policy_version IS NOT ?3)
            AND EXISTS(SELECT 1 FROM convo_retention v JOIN convo_sessions s ON s.id=v.session WHERE v.session=?1
                AND transcript_archives.close_rev=v.close_rev AND transcript_archives.accepted_rev=v.accepted_rev AND v.close_at=s.closed_at AND v.close_epoch=s.epoch)",
            params![session,owner.generation,owner.policy_version])?;
    }
    let mut rows=Vec::new();
    for session in pending_page(conn,&mut selections.due,now)? {
        let row=conn.query_row("SELECT a.session,a.close_rev,a.accepted_rev FROM transcript_archives a JOIN convo_retention v ON v.session=a.session JOIN convo_sessions s ON s.id=a.session
            WHERE a.session=?1 AND a.status='pending' AND a.due_ms<=?2 AND a.close_rev=v.close_rev AND a.accepted_rev=v.accepted_rev AND v.close_at=s.closed_at AND v.close_epoch=s.epoch",
            params![session,now],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?))).optional()?;
        if let Some(row)=row {rows.push(row);}
    }
    selections.save(conn)?;
    Ok(rows)
}

/// Policy retirement is a logical history mutation, not a producer reset. It
/// revokes receipts before external deletion and never holds the writer across I/O.
pub(super) async fn expire(hub:&Hub,authority:&Authority)->anyhow::Result<()> {
    authority.require_store(&hub.store)?;
    let Some(keep)=authority.policy.keep_archived_ms else {return Ok(())};
    let owner=authority.clone();
    hub.store.call(move |conn| {
        owner.check(conn,true)?;
        let before=crate::util::now_ms().saturating_sub(keep);
        let mut selections=Selections::read(conn)?;
        let candidates=observed_page(conn,&mut selections.expire,before)?;
        selections.save(conn)?;
        let mut rows=Vec::new();
        for session in candidates {
            let row=conn.query_row("SELECT a.session,a.close_rev,a.accepted_rev FROM transcript_archives a JOIN convo_retention v ON v.session=a.session JOIN convo_sessions s ON s.id=a.session
                WHERE a.session=?1 AND a.status IN ('archived','given-up') AND v.observed_ms<?2 AND a.close_rev=v.close_rev AND a.accepted_rev=v.accepted_rev
                AND v.close_at=s.closed_at AND v.close_epoch=s.epoch
                AND (?3 OR NOT EXISTS(SELECT 1 FROM convo_records r WHERE r.session=s.id))",
                params![session,before,owner.policy.keep_closed_ms.is_none()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?))).optional()?;
            if let Some(row)=row {rows.push(row);}
        }
        if rows.is_empty() {return Ok(());}
        let mut rev:u64=conn.query_row("SELECT value FROM convo_meta WHERE key='rev'",[],|r|r.get::<_,String>(0))?.parse()?;
        for (session,close,accepted) in rows {
            rev=rev.checked_add(1).ok_or_else(||anyhow::anyhow!("conversation revision overflow"))?;
            let next=i64::try_from(rev)?;
            let n=conn.execute("UPDATE convo_retention SET accepted_rev=?4,history_rev=?4,last_complete_capture=NULL WHERE session=?1 AND close_rev=?2 AND accepted_rev=?3",params![session,close,accepted,next])?;
            anyhow::ensure!(n==1,"archive expiry lost its exact closed state");
            conn.execute("UPDATE transcript_archives SET status='expired',accepted_rev=?4,manifest=NULL,failure_ms=NULL,owner_generation=?5,policy_version=?6 WHERE session=?1 AND close_rev=?2 AND accepted_rev=?3",params![session,close,accepted,next,owner.generation,owner.policy_version])?;
        }
        conn.execute("UPDATE convo_meta SET value=?1 WHERE key='rev'",[rev.to_string()])?;
        Ok(())
    }).await
}

/// Current complete histories and exact active captures protect a shared object.
/// The key-index lookup and reference admission use the same serialized SQL owner.
fn protected(conn:&Connection<'_>,key:&str)->anyhow::Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM transcript_manifest_pieces r JOIN transcript_manifests m ON m.capture=r.capture JOIN convo_retention v ON v.session=m.session
        LEFT JOIN transcript_archives a ON a.session=m.session
        WHERE r.key=?1 AND ((m.state='complete' AND m.history_rev=v.history_rev)
            OR (m.state='pending' AND a.status='pending' AND a.manifest=m.capture AND a.close_rev=v.close_rev AND a.accepted_rev=v.accepted_rev AND m.history_rev=v.history_rev)))",[key],|r|r.get(0))?)
}

/// Inspect64 manifests, then at most100 referenced keys and100 legacy keys.
/// Each writer turn is bounded. A lost delete reply
/// retains Collecting. Keys are never reused, including after owner transfer.
pub(super) async fn collect(hub:&Hub,authority:&Authority)->anyhow::Result<()> {
    authority.require_store(&hub.store)?;
    expire(hub,authority).await?;
    let (owner,proposal)=(authority.clone(),crate::util::new_id());
    let claim=hub.store.call(move |conn| {
        owner.check(conn,true)?;
        let (after,horizon):(Option<String>,Option<String>)=conn.query_row("SELECT collect_after,collect_horizon FROM conversation_maintenance WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let eligible="SELECT m.capture,m.collection FROM transcript_manifests m JOIN convo_retention v ON v.session=m.session
            WHERE ((m.state='collecting' AND (EXISTS(SELECT 1 FROM transcript_manifest_pieces r WHERE r.capture=m.capture)
                    OR NOT EXISTS(SELECT 1 FROM transcript_segments p WHERE p.capture=m.capture AND p.state IN ('putting','collecting'))))
                OR (m.state='complete' AND m.history_rev<v.history_rev)
                OR (m.state='pending' AND NOT EXISTS(SELECT 1 FROM transcript_archives a WHERE a.session=m.session AND a.status='pending' AND a.manifest=m.capture AND a.close_rev=v.close_rev AND a.accepted_rev=v.accepted_rev)))";
        let horizon=if let Some(horizon)=horizon {Some(horizon)}else{
            conn.query_row("SELECT max(capture) FROM transcript_manifests",[],|r|r.get::<_,Option<String>>(0))?
        };
        let Some(horizon)=horizon else{return Ok(None)};
        let mut page=if after.is_some() {
            conn.prepare("SELECT capture FROM transcript_manifests WHERE capture>?1 AND capture<=?2 ORDER BY capture LIMIT 64")?
        }else{conn.prepare("SELECT capture FROM transcript_manifests WHERE capture<=?1 ORDER BY capture LIMIT 64")?};
        let rows=if let Some(after)=after {
            page.query_map(params![after,horizon],|r|r.get::<_,String>(0))?.collect::<crate::sql::Result<Vec<_>>>()?
        }else{page.query_map([&horizon],|r|r.get::<_,String>(0))?.collect::<crate::sql::Result<Vec<_>>>()?};
        if rows.is_empty() {
            conn.execute("UPDATE conversation_maintenance SET collect_after=NULL,collect_horizon=NULL WHERE id=1",[])?;
            return Ok(None);
        }
        let mut candidate=None;
        let mut cursor=None;
        let mut eligible=conn.prepare(&format!("{eligible} AND m.capture=?1"))?;
        for capture in rows {
            cursor=Some(capture.clone());
            candidate=eligible.query_row([capture],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?))).optional()?;
            if candidate.is_some() {break;}
        }
        conn.execute("UPDATE conversation_maintenance SET collect_after=?1,collect_horizon=?2 WHERE id=1",params![cursor,horizon])?;
        let Some((capture,collection))=candidate else{return Ok(None)};
        let collection=collection.unwrap_or(proposal);
        conn.execute("UPDATE transcript_manifests SET state='collecting',collection=?2 WHERE capture=?1 AND state IN ('pending','complete','collecting')",params![capture,collection])?;
        Ok(Some((capture,collection)))
    }).await?;
    if let Some((capture,collection))=claim {
        let (owner,capture_id)=(authority.clone(),capture.clone());
        let keys=hub.store.call(move |conn| {
            owner.check(conn,true)?;
            let after:Option<(String,i64)>=conn.query_row("SELECT collection_after_source,collection_after_first FROM transcript_manifests WHERE capture=?1",[&capture_id],|r|Ok(r.get::<_,Option<String>>(0)?.zip(r.get::<_,Option<i64>>(1)?)))?;
            let mut q=if after.is_some() {
                conn.prepare("SELECT key,source,first FROM transcript_manifest_pieces WHERE capture=?1 AND (source,first)>(?2,?3) ORDER BY source,first LIMIT ?4")?
            }else{
                conn.prepare("SELECT key,source,first FROM transcript_manifest_pieces WHERE capture=?1 ORDER BY source,first LIMIT ?4")?
            };
            let read=|r:&crate::sql::Row<'_>|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?));
            let cursor=after.unwrap_or_default();
            let mut rows=q.query_map(params![capture_id,cursor.0,cursor.1,super::LET_GO_BATCH],read)?.collect::<crate::sql::Result<Vec<_>>>()?;
            if rows.is_empty() {
                let mut first=conn.prepare("SELECT key,source,first FROM transcript_manifest_pieces WHERE capture=?1 ORDER BY source,first LIMIT ?2")?;
                rows=first.query_map(params![capture_id,super::LET_GO_BATCH],read)?.collect::<crate::sql::Result<Vec<_>>>()?;
            }
            let next=rows.last().map(|(_,s,n)|(s.clone(),*n));
            conn.execute("UPDATE transcript_manifests SET collection_after_source=?2,collection_after_first=?3 WHERE capture=?1",params![capture_id,next.as_ref().map(|(s,_)|s),next.as_ref().map(|(_,n)|*n)])?;
            let rows=rows.into_iter().map(|(key,_,_)|key).collect::<Vec<_>>();
            Ok(rows)
        }).await?;
        for key in keys {
            let (owner,capture_id,collection_id,key_id)=(authority.clone(),capture.clone(),collection.clone(),key.clone());
            let token=hub.store.call(move |conn| {
                owner.check(conn,true)?;
                let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM transcript_manifests WHERE capture=?1 AND state='collecting' AND collection=?2)",params![capture_id,collection_id],|r|r.get(0))?;
                anyhow::ensure!(valid,"archive collection claim changed");
                let (state,prior):(String,Option<String>)=conn.query_row("SELECT state,collection FROM transcript_segments WHERE key=?1",[&key_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                if state=="deleted" || protected(conn,&key_id)? {
                    conn.execute("DELETE FROM transcript_manifest_pieces WHERE capture=?1 AND key=?2",params![capture_id,key_id])?;
                    return Ok(None);
                }
                if state=="collecting" {return Ok(Some(prior.ok_or_else(||anyhow::anyhow!("archive collecting key has no original token"))?));}
                anyhow::ensure!(state=="ready","unresolved archive object cannot be released");
                let n=conn.execute("UPDATE transcript_segments SET state='collecting',collection=?2 WHERE key=?1 AND state='ready'",params![key_id,collection_id])?;
                anyhow::ensure!(n==1,"archive object changed before collection");
                Ok(Some(collection_id))
            }).await?;
            let Some(token)=token else{continue};
            match hub.blobs.blobs.delete(&key).await {
                Ok(())|Err(kiln_seams::blobs::Refusal::NotFound(_))=>{
                    let (owner,key_id,capture_id)=(authority.clone(),key.clone(),capture.clone());
                    hub.store.call(move |conn| {
                        owner.check(conn,true)?;
                        let n=conn.execute("UPDATE transcript_segments SET state='deleted' WHERE key=?1 AND state='collecting' AND collection=?2",params![key_id,token])?;
                        anyhow::ensure!(n==1,"archive delete receipt differs from its exact object claim");
                        conn.execute("DELETE FROM transcript_manifest_pieces WHERE capture=?1 AND key=?2",params![capture_id,key_id])?;
                        Ok(())
                    }).await?;
                    crate::metrics::blobs().collected.add(&["transcripts"],1);
                },
                Err(error)=>tracing::warn!(target:"kiln.blobs",event="collect",key=%key,error=%error,"exact archive delete remains collecting; retry on the existing cadence"),
            }
        }
        let (owner,capture_id,collection_id)=(authority.clone(),capture,collection);
        hub.store.call(move |conn| {
            owner.check(conn,true)?;
            // Putting has no positive original completion. It remains owned and
            // unresolved even if a head observation happens to find an object.
            conn.execute("UPDATE transcript_manifests SET state='deleted' WHERE capture=?1 AND state='collecting' AND collection=?2
                AND NOT EXISTS(SELECT 1 FROM transcript_manifest_pieces WHERE capture=?1)
                AND NOT EXISTS(SELECT 1 FROM transcript_segments WHERE capture=?1 AND state IN ('putting','collecting'))",params![capture_id,collection_id])?;
            Ok(())
        }).await?;
    }
    // Read-only legacy pieces have no capture receipt. Only an already committed
    // history retirement permits their exact old keys to be collected.
    let (owner,proposal)=(authority.clone(),crate::util::new_id());
    let legacy=hub.store.call(move |conn| {
        owner.check(conn,true)?;
        let after:Option<String>=conn.query_row("SELECT legacy_after FROM conversation_maintenance WHERE id=1",[],|r|r.get(0))?;
        let mut q=if after.is_some() {
            conn.prepare("SELECT key,collection FROM transcript_segments WHERE capture IS NULL AND key>?2 ORDER BY key LIMIT ?1")?
        }else{
            conn.prepare("SELECT key,collection FROM transcript_segments WHERE capture IS NULL ORDER BY key LIMIT ?1")?
        };
        let read=|r:&crate::sql::Row<'_>|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?));
        let mut rows=if let Some(after)=after {q.query_map(params![super::LET_GO_BATCH,after],read)?.collect::<crate::sql::Result<Vec<_>>>()?}else{q.query_map([super::LET_GO_BATCH],read)?.collect::<crate::sql::Result<Vec<_>>>()?};
        if rows.is_empty() {
            let mut first=conn.prepare("SELECT key,collection FROM transcript_segments WHERE capture IS NULL ORDER BY key LIMIT ?1")?;
            rows=first.query_map([super::LET_GO_BATCH],read)?.collect::<crate::sql::Result<Vec<_>>>()?;
        }
        conn.execute("UPDATE conversation_maintenance SET legacy_after=?1 WHERE id=1",[rows.last().map(|(key,_)|key)])?;
        let mut claims=Vec::with_capacity(rows.len());
        for (key,prior) in rows {
            let state:Option<String>=conn.query_row("SELECT p.state FROM transcript_segments p JOIN convo_retention v ON v.session=p.session WHERE p.key=?1 AND p.capture IS NULL AND v.history_rev>0 AND p.state IN ('legacy','collecting')",[&key],|r|r.get(0)).optional()?;
            let Some(state)=state else{continue};
            let token=if state=="collecting" {prior.ok_or_else(||anyhow::anyhow!("legacy collection has no retained claim"))?}else{proposal.clone()};
            let n=conn.execute("UPDATE transcript_segments SET state='collecting',collection=?2 WHERE key=?1 AND capture IS NULL AND state IN ('legacy','collecting')",params![key,token])?;
            anyhow::ensure!(n==1,"legacy collection lost its exact object");
            claims.push((key,token));
        }
        Ok(claims)
    }).await?;
    for (key,token) in legacy {
        match hub.blobs.blobs.delete(&key).await {
            Ok(())|Err(kiln_seams::blobs::Refusal::NotFound(_))=>{
                let owner=authority.clone();
                hub.store.call(move |conn| {
                    owner.check(conn,true)?;
                    let n=conn.execute("UPDATE transcript_segments SET state='deleted' WHERE key=?1 AND capture IS NULL AND state='collecting' AND collection=?2",params![key,token])?;
                    anyhow::ensure!(n==1,"legacy archive delete receipt differs from its claim");
                    Ok(())
                }).await?;
                crate::metrics::blobs().collected.add(&["transcripts"],1);
            },
            Err(error)=>tracing::warn!(target:"kiln.blobs",event="collect",key=%key,error=%error,"legacy delete retains its exact claim"),
        }
    }
    Ok(())
}


pub(super) async fn base_rows(hub:&Hub,capture:&Capture,source:&Source)->anyhow::Result<u64> {
    capture.authority.require_store(&hub.store)?;
    let Some(base)=source.base.clone() else{return Ok(0)};
    let (c,s)=(capture.clone(),source.clone());
    hub.store.call(move |conn| {
        c.check(conn)?;
        let rows:i64=conn.query_row("SELECT x.rows FROM transcript_manifest_sources x JOIN transcript_manifests m ON m.capture=x.capture WHERE m.capture=?1 AND m.session=?2 AND m.state='complete' AND m.history_rev=?3 AND x.source=?4 AND x.incarnation=?5 AND x.complete=1",params![base,c.session,c.history_rev,s.source,s.incarnation],|r|r.get(0))?;
        Ok(u64::try_from(rows)?)
    }).await
}
