//! U05: identities for accepted SQL transcript data. These helpers run inside the
//! original writer transaction; they never open/call a second Database owner.
use crate::sql::{Connection, OptionalExtension as _, params};


fn revision(rev: u64) -> anyhow::Result<i64> {
    let rev = i64::try_from(rev)?;
    anyhow::ensure!(rev > 0, "retention identity requires a positive revision");
    Ok(rev)
}

/// A semantic close accepted by the canonical writer, not proof of producer EOF.
pub(super) fn closed(conn: &Connection<'_>, session: &str, rev: u64, origin: &str) -> anyhow::Result<()> {
    let Some((epoch, at)) = conn.query_row(
        "SELECT epoch,closed_at FROM convo_sessions WHERE id=?1 AND closed_at IS NOT NULL",
        [session], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)),
    ).optional()? else { return Ok(()) };
    let rev = revision(rev)?;
    let now = crate::util::now_ms();
    conn.execute("INSERT INTO convo_retention(session,close_rev,close_epoch,close_at,observed_ms,origin) VALUES (?1,?2,?3,?4,?5,?6)
        ON CONFLICT(session) DO UPDATE SET close_rev=excluded.close_rev,close_epoch=excluded.close_epoch,close_at=excluded.close_at,observed_ms=excluded.observed_ms,origin=excluded.origin",
        params![session,rev,epoch,at,now,origin])?;
    pending(conn,session,now)
}

/// Reconcile only the accepted projection's lifecycle. Metadata-only revisions
/// cannot reuse or replace a close identity; a newly adopted lifecycle can.
pub(super) fn projection(conn: &Connection<'_>, session: &str, rev: u64, opened: bool, origin: &str) -> anyhow::Result<()> {
    let Some((epoch, at)) = conn.query_row("SELECT epoch,closed_at FROM convo_sessions WHERE id=?1",[session],
        |r| Ok((r.get::<_,i64>(0)?,r.get::<_,Option<i64>>(1)?))).optional()? else { return Ok(()) };
    if opened || at.is_none() {
        conn.execute("INSERT INTO convo_retention(session,close_epoch) VALUES (?1,?2)
            ON CONFLICT(session) DO UPDATE SET close_rev=NULL,close_epoch=excluded.close_epoch,close_at=NULL,observed_ms=NULL,origin=NULL",
            params![session,epoch])?;
        conn.execute("UPDATE transcript_archives SET status='open',manifest=NULL,done_ms=NULL,close_rev=NULL WHERE session=?1",[session])?;
        return Ok(());
    }
    let same = conn.query_row("SELECT close_rev IS NOT NULL AND close_epoch=?2 AND close_at IS ?3 FROM convo_retention WHERE session=?1",
        params![session,epoch,at], |r| r.get::<_,bool>(0)).optional()?.unwrap_or(false);
    if !same { closed(conn,session,rev,origin)?; }
    Ok(())
}

/// Coalesced within the original atomic apply/import page. A later reset
/// discards the earlier frontier; following lines join its new incarnation.
#[derive(Default)]
pub(super) struct Mutations {
    sources: std::collections::BTreeMap<(String,String), Mutation>,
}
struct Mutation { first: i64, last: i64, reset: Option<i64>, dirty: i64, frontier: Option<i64> }
impl Mutations {
    pub(super) fn record(&mut self, session: &str, source: &str, n: u64, rev: u64, reset: bool) -> anyhow::Result<()> {
        let (rev,n)=(revision(rev)?,i64::try_from(n)?);
        let change=self.sources.entry((session.to_owned(),source.to_owned())).or_insert(Mutation {first:rev,last:rev,reset:None,dirty:n,frontier:None});
        change.last=rev;
        if reset { change.reset=Some(rev);change.dirty=0;change.frontier=None; }
        else {change.dirty=change.dirty.min(n);change.frontier=Some(change.frontier.map_or(n,|v|v.max(n)));}
        Ok(())
    }

    /// One indexed source write per distinct source, then one state/obligation
    /// write per session. Retention deletion deliberately never calls this.
    pub(super) fn flush(self, conn: &Connection<'_>) -> anyhow::Result<()> {
        let mut sessions=std::collections::BTreeMap::<String,i64>::new();
        for ((session,source),change) in self.sources {
            let incarnation=change.reset.unwrap_or(change.first);
            let expired: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM convo_record_sources s JOIN convo_retention v ON v.session=s.session WHERE s.session=?1 AND s.source=?2 AND s.accepted_rev<=v.history_rev)",params![session,source],|r|r.get(0))?;
            let sealed=if change.reset.is_none() && !expired {
                conn.query_row("SELECT v.last_complete_capture FROM convo_record_sources s JOIN convo_retention v ON v.session=s.session
                    JOIN transcript_manifests m ON m.capture=v.last_complete_capture AND m.state='complete' AND m.history_rev=v.history_rev
                    JOIN transcript_manifest_sources x ON x.capture=m.capture AND x.source=s.source AND x.incarnation=s.incarnation AND x.accepted_rev=s.accepted_rev AND x.complete=1
                    WHERE s.session=?1 AND s.source=?2",params![session,source],|r|r.get::<_,String>(0)).optional()?
            }else{None};
            let (dirty,frontier)=if expired && change.reset.is_none() {
                (0,conn.query_row("SELECT max(n) FROM convo_records WHERE session=?1 AND source=?2",params![session,source],|r|r.get::<_,Option<i64>>(0))?)
            }else{(change.dirty,change.frontier)};
            conn.execute("INSERT INTO convo_record_sources(session,source,incarnation,accepted_rev,dirty_from,frontier) VALUES (?1,?2,?3,?4,?5,?6)
                ON CONFLICT(session,source) DO UPDATE SET incarnation=CASE WHEN ?7 IS NOT NULL THEN excluded.incarnation ELSE convo_record_sources.incarnation END,accepted_rev=excluded.accepted_rev,
                base_capture=CASE WHEN ?7 IS NOT NULL OR ?9 THEN NULL WHEN ?8 IS NOT NULL THEN ?8 ELSE convo_record_sources.base_capture END,
                dirty_from=CASE WHEN ?7 IS NOT NULL THEN 0 WHEN ?9 THEN excluded.dirty_from WHEN ?8 IS NOT NULL THEN excluded.dirty_from WHEN convo_record_sources.dirty_from IS NULL THEN excluded.dirty_from ELSE min(convo_record_sources.dirty_from,excluded.dirty_from) END,
                frontier=CASE WHEN ?7 IS NOT NULL OR ?9 THEN excluded.frontier WHEN convo_record_sources.frontier IS NULL THEN excluded.frontier WHEN excluded.frontier IS NULL THEN convo_record_sources.frontier ELSE max(convo_record_sources.frontier,excluded.frontier) END",
                params![session,source,incarnation,change.last,dirty,frontier,change.reset,sealed,expired])?;
            sessions.entry(session).and_modify(|last|*last=(*last).max(change.last)).or_insert(change.last);
        }
        let now=crate::util::now_ms();
        for (session,rev) in sessions {
            conn.execute("INSERT INTO convo_retention(session,accepted_rev) VALUES (?1,?2) ON CONFLICT(session) DO UPDATE SET accepted_rev=excluded.accepted_rev",params![session,rev])?;
            pending(conn,&session,now)?;
        }
        Ok(())
    }
}

fn pending(conn: &Connection<'_>, session: &str, now: i64) -> anyhow::Result<()> {
    // The obligation commits with its effect, even before any maintenance owner
    // is registered. An absent asynchronous archive.closed hint grants nothing.
    conn.execute("INSERT INTO transcript_archives(session,closed_ms,due_ms,done_ms,close_rev,accepted_rev,manifest,status)
        SELECT session,observed_ms,?2,NULL,close_rev,accepted_rev,NULL,'pending' FROM convo_retention WHERE session=?1 AND close_rev IS NOT NULL
        ON CONFLICT(session) DO UPDATE SET closed_ms=excluded.closed_ms,due_ms=excluded.due_ms,done_ms=NULL,close_rev=excluded.close_rev,accepted_rev=excluded.accepted_rev,manifest=NULL,status='pending',failure_ms=NULL,owner_generation=NULL,policy_version=NULL",
        params![session,now.saturating_add(i64::try_from(crate::harness::archive::AFTER.as_millis())?)])?;
    Ok(())
}


/// Only the existing trusted Registry/archive startup constructs this policy.
/// No transport/path/first-opener observation promotes a secondary client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Policy {
    pub(crate) keep_closed_ms: Option<i64>,
    pub(crate) keep_archived_ms: Option<i64>,
    pub(crate) hold_extra_ms: i64,
}
impl Policy {
    pub(crate) fn archive(config: &crate::config::Conversations) -> anyhow::Result<Self> {
        let millis = |days: u32| i64::from(days).checked_mul(86_400_000).ok_or_else(|| anyhow::anyhow!("retention duration overflow"));
        Ok(Self {
            keep_closed_ms: (config.keep_closed_days > 0).then(|| millis(config.keep_closed_days)).transpose()?,
            keep_archived_ms: (config.keep_archived_days > 0).then(|| millis(config.keep_archived_days)).transpose()?,
            hold_extra_ms: i64::try_from(crate::harness::archive::HOLD_EXTRA.as_millis())?,
        })
    }
}

/// Internal SQL capability. It fences maintenance SQL, not physical old-writer
/// drain, producer EOF or another process's outstanding external blob operation.
#[derive(Clone, Debug)]
pub(crate) struct Authority {
    pub(crate) store: String,
    pub(crate) owner: String,
    pub(crate) generation: i64,
    pub(crate) policy_version: i64,
    pub(crate) policy: Policy,
    host: crate::store::Store,
    writer_boundary: Option<std::sync::Arc<crate::store::StoreWriterBoundary>>,
}

#[derive(Debug)]
pub(crate) enum MaintenanceRefusal { Secondary, Stale, Initializing, UnsupportedWriterBoundary }
impl std::fmt::Display for MaintenanceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self { Self::Secondary => "secondary conversation client has no maintenance authority", Self::Stale => "conversation maintenance capability is stale", Self::Initializing => "conversation maintenance coverage is not ready", Self::UnsupportedWriterBoundary => "conversation maintenance requires the original Store writer boundary; legacy live-writer cutover is unsupported" })
    }
}
impl std::error::Error for MaintenanceRefusal {}

#[derive(Clone,Copy)]
pub(crate) enum AuthorityState { Ready, Initializing, Stale, UnsupportedWriterBoundary }
impl Authority {
    /// This immutable identity check precedes each maintenance call. The
    /// serialized SDK writer checks the durable floor again before its closure.
    pub(crate) fn require_store_identity(&self,store:&crate::store::Store)->anyhow::Result<()> {
        self.host.require_same_database(store)?;
        if let Some(boundary)=&self.writer_boundary { boundary.require_store(store)?; }
        Ok(())
    }
    pub(crate) fn require_store(&self,store:&crate::store::Store)->anyhow::Result<()> {
        self.require_store_identity(store)?;
        let boundary=self.writer_boundary.as_ref().ok_or(MaintenanceRefusal::UnsupportedWriterBoundary)?;
        anyhow::ensure!(boundary.version()>=6,"conversation writer boundary is older than contract 6");
        Ok(())
    }
    pub(crate) fn state(&self, conn: &Connection<'_>) -> anyhow::Result<AuthorityState> {
        let store: String = conn.query_row("SELECT value FROM convo_meta WHERE key='id'",[],|r|r.get(0))?;
        if store!=self.store {return Ok(AuthorityState::Stale);}
        let state=conn.query_row("SELECT owner,generation,policy_version,keep_closed_ms,keep_archived_ms,hold_extra_ms,phase FROM conversation_maintenance WHERE id=1",[],|r| {
            Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,Option<i64>>(4)?,r.get::<_,i64>(5)?,r.get::<_,String>(6)?))
        }).optional()?;
        let Some((owner,generation,version,closed,archived,extra,phase))=state else {return Ok(AuthorityState::Stale)};
        if owner!=self.owner || generation!=self.generation || version!=self.policy_version || closed!=self.policy.keep_closed_ms || archived!=self.policy.keep_archived_ms || extra!=self.policy.hold_extra_ms {return Ok(AuthorityState::Stale);}
        if self.writer_boundary.is_none() {return Ok(AuthorityState::UnsupportedWriterBoundary);}
        self.require_store(&self.host)?;
        match phase.as_str() {
            "ready"=>Ok(AuthorityState::Ready),
            "initializing"=>Ok(AuthorityState::Initializing),
            _=>anyhow::bail!("conversation maintenance phase is invalid"),
        }
    }
    pub(crate) fn check(&self, conn: &Connection<'_>, ready: bool) -> anyhow::Result<()> {
        match self.state(conn)? {
            AuthorityState::Stale=>Err(MaintenanceRefusal::Stale.into()),
            AuthorityState::Initializing if ready=>Err(MaintenanceRefusal::Initializing.into()),
            AuthorityState::UnsupportedWriterBoundary=>Err(MaintenanceRefusal::UnsupportedWriterBoundary.into()),
            _=>Ok(()),
        }
    }
}

/// Called outside any Database callback, after the canonical conversation schema
/// and complete declared imports exist. A different owner remains a secondary;
/// transferring to it requires an explicit trusted handoff, not absence/expiry.
pub(crate) fn register(host: &crate::store::Store, owner: &str, policy: Policy) -> anyhow::Result<Option<Authority>> {
    anyhow::ensure!(!owner.is_empty() && owner.len()<=1024,"archive owner identity is invalid");
    let boundary=match host.writer_boundary(6) {
        Ok(boundary)=>Some(std::sync::Arc::new(boundary)),
        Err(error) if error.downcast_ref::<crate::store::UnsupportedStoreWriterBoundary>().is_some()=>None,
        Err(error)=>return Err(error),
    };
    let (host,owner)=(host.clone(),owner.to_owned());
    host.database().call(crate::store::deadline(),move |conn| {
        let store: String=conn.query_row("SELECT value FROM convo_meta WHERE key='id'",[],|r|r.get(0))?;
        let horizon=backfill_horizon(conn)?;
        let horizon=serde_json::to_string(&horizon)?;
        let prior=conn.query_row("SELECT owner,generation,policy_version,keep_closed_ms,keep_archived_ms,hold_extra_ms FROM conversation_maintenance WHERE id=1",[],|r| {
            Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,Option<i64>>(4)?,r.get::<_,i64>(5)?))
        }).optional()?;
        let (generation,version)=if let Some((old,generation,version,closed,archived,extra))=prior {
            if old!=owner { return Ok(None); }
            let next=generation.checked_add(1).ok_or_else(||anyhow::anyhow!("maintenance generation overflow"))?;
            let changed=closed!=policy.keep_closed_ms || archived!=policy.keep_archived_ms || extra!=policy.hold_extra_ms;
            let next_version=if changed {version.checked_add(1).ok_or_else(||anyhow::anyhow!("retention policy version overflow"))?}else{version};
            let n=conn.execute("UPDATE conversation_maintenance SET generation=?2,policy_version=?3,keep_closed_ms=?4,keep_archived_ms=?5,hold_extra_ms=?6,prune_again=1,archive_scan=NULL WHERE id=1 AND owner=?1 AND generation=?7 AND policy_version=?8",
                params![owner,next,next_version,policy.keep_closed_ms,policy.keep_archived_ms,policy.hold_extra_ms,generation,version])?;
            anyhow::ensure!(n==1,"maintenance owner CAS refused");
            (next,next_version)
        }else{
            conn.execute("INSERT INTO conversation_maintenance(id,owner,generation,policy_version,keep_closed_ms,keep_archived_ms,hold_extra_ms,phase,backfill_horizon) VALUES (1,?1,1,1,?2,?3,?4,'initializing',?5)",
                params![owner,policy.keep_closed_ms,policy.keep_archived_ms,policy.hold_extra_ms,horizon])?;
            (1,1)
        };
        Ok(Some(Authority {store,owner,generation,policy_version:version,policy,host,writer_boundary:boundary}))
    }).map_err(Into::into)
}


#[derive(serde::Serialize,serde::Deserialize)]
struct Horizon { sessions: Option<String>, records: Option<(String,String,i64)> }
#[derive(Default,serde::Serialize,serde::Deserialize)]
struct Cursor { sessions: Option<String>, sessions_done: bool, records: Option<(String,String,i64)> }
fn backfill_horizon(conn: &Connection<'_>) -> anyhow::Result<Horizon> {
    Ok(Horizon {
        sessions:conn.query_row("SELECT max(id) FROM convo_sessions",[],|r|r.get(0))?,
        records:conn.query_row("SELECT session,source,n FROM convo_records ORDER BY session DESC,source DESC,n DESC LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?,
    })
}

/// One bounded metadata adoption transaction on the original archive cadence.
/// An exact cursor is committed with the identities it covers. A page count,
/// wall-clock window or missing pending aggregate is never a readiness proof.
pub(crate) fn backfill_step(conn: &Connection<'_>, authority: &Authority) -> anyhow::Result<bool> {
    authority.check(conn,false)?;
    let (phase,cursor,horizon): (String,Option<String>,String)=conn.query_row("SELECT phase,backfill_after,backfill_horizon FROM conversation_maintenance WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    if phase=="ready" { return Ok(true); }
    let mut cursor=cursor.map(|s|serde_json::from_str::<Cursor>(&s)).transpose()?.unwrap_or_default();
    let horizon: Horizon=serde_json::from_str(&horizon)?;
    let mut rev: u64=conn.query_row("SELECT value FROM convo_meta WHERE key='rev'",[],|r|r.get::<_,String>(0))?.parse()?;
    if !cursor.sessions_done {
        let rows=if let Some(horizon)=horizon.sessions {
            if let Some(after)=&cursor.sessions {
                let mut q=conn.prepare("SELECT id FROM convo_sessions WHERE id>?1 AND id<=?2 ORDER BY id LIMIT 64")?;
                q.query_map(params![after,horizon],|r|r.get::<_,String>(0))?.collect::<crate::sql::Result<Vec<_>>>()?
            }else{
                let mut q=conn.prepare("SELECT id FROM convo_sessions WHERE id<=?1 ORDER BY id LIMIT 64")?;
                q.query_map([horizon],|r|r.get::<_,String>(0))?.collect::<crate::sql::Result<Vec<_>>>()?
            }
        }else{Vec::new()};
        for session in &rows {
            rev=rev.checked_add(1).ok_or_else(||anyhow::anyhow!("conversation revision overflow"))?;
            let _=revision(rev)?;
            projection(conn,session,rev,false,"adopted")?;
        }
        cursor.sessions=rows.last().cloned().or(cursor.sessions);
        cursor.sessions_done=rows.len()<64;
    }else{
        let rows=if let Some((hs,hsrc,hn))=horizon.records {
            if let Some(after)=&cursor.records {
                let mut q=conn.prepare("SELECT session,source,n FROM convo_records WHERE (session,source,n)>(?1,?2,?3) AND (session,source,n)<=(?4,?5,?6) ORDER BY session,source,n LIMIT 128")?;
                q.query_map(params![after.0,after.1,after.2,hs,hsrc,hn],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?.collect::<crate::sql::Result<Vec<_>>>()?
            }else{
                let mut q=conn.prepare("SELECT session,source,n FROM convo_records WHERE (session,source,n)<=(?1,?2,?3) ORDER BY session,source,n LIMIT 128")?;
                q.query_map(params![hs,hsrc,hn],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?.collect::<crate::sql::Result<Vec<_>>>()?
            }
        }else{Vec::new()};
        let mut mutations=Mutations::default();
        for (session,source,n) in &rows {
            rev=rev.checked_add(1).ok_or_else(||anyhow::anyhow!("conversation revision overflow"))?;
            mutations.record(session,source,u64::try_from(*n)?,rev,false)?;
        }
        mutations.flush(conn)?;
        cursor.records=rows.last().cloned().or(cursor.records);
        if rows.len()<128 {
            conn.execute("UPDATE conversation_maintenance SET phase='ready' WHERE id=1 AND owner=?1 AND generation=?2 AND policy_version=?3",params![authority.owner,authority.generation,authority.policy_version])?;
        }
    }
    conn.execute("INSERT INTO convo_meta(key,value) VALUES ('rev',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[rev.to_string()])?;
    conn.execute("UPDATE conversation_maintenance SET backfill_after=?1 WHERE id=1 AND owner=?2 AND generation=?3 AND policy_version=?4",params![serde_json::to_string(&cursor)?,authority.owner,authority.generation,authority.policy_version])?;
    Ok(false)
}
