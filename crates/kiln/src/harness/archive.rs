//! CP-T10.1/U05: accepted transcript snapshots in the existing object store.
//! The canonical writer records close and content identities with their effects.
//! The archive loop captures those identities, uploads immutable hashed pieces,
//! and publishes an exact receipt before canonical retention may delete rows.
//! A late row invalidates that receipt; rebuilding merges verified archived
//! prefixes with the current canonical rows outside the SQL writer. A reset has
//! a fresh source incarnation. Neither a quiet interval nor inventory absence
//! proves a producer's EOF.
//!
//! One registered host policy/owner generation governs archive and retention
//! SQL. Secondary clients cannot prune. Metadata adoption is bounded and durably
//! resumable; schema/read floors do not prove physical old-writer drain. Object
//! collection claims unique keys before I/O and retains unresolved claims.

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

use kiln_seams::blobs::{Cond, Put, Refusal};
use crate::sql::{OptionalExtension as _, params};
use serde::Serialize;

use crate::hub::Hub;
use crate::convo::retention::{Authority, MaintenanceRefusal};
mod receipt;

/// How long after a close a conversation's record is archived.
pub const AFTER: Duration = Duration::from_secs(300);
/// How often what's due is looked for.
const EVERY: Duration = Duration::from_secs(60);
/// How long an archive that failed waits to be tried again.
const RETRY_MS: i64 = 3_600_000;
/// Archives one look takes, at most.
const BATCH: i64 = 8;
/// Lines read from the conversation store at a time.
const PAGE: usize = 2000;
/// Pages one archive of a source reads, at most: a record longer goes on from its last piece at
/// the next try.
const PAGES_MAX: usize = 50_000;
/// The most lines, and bytes of them, a piece holds.
pub const PIECE_LINES: u64 = 50_000;
pub const PIECE_BYTES: u64 = 32 << 20;
/// The most a piece is unpacked to when it's read back: its lines, and a last one however long.
pub const UNPACKED_MAX: u64 = 1 << 30;
/// brotli's quality, as the conversation store keeps lines (`crate::convo`).
const QUALITY: u32 = 5;
/// The codec a piece is written in.
const CODEC: &str = "jsonl+br";

/// How large a piece may grow: its lines, and bytes of them.
#[derive(Debug, Clone, Copy)]
struct Limits {
    lines: u64,
    bytes: u64,
}

const LIMITS: Limits = Limits { lines: PIECE_LINES, bytes: PIECE_BYTES };
/// How long past the conversation store's own keeping a record not archived yet is held: past it,
/// its archive is given up, warned, and the store lets the record go as it would have.
pub const HOLD_EXTRA: Duration = Duration::from_secs(30 * 86_400);
/// Archives let go of, or given up, at most a look.
const LET_GO_BATCH: i64 = 100;
/// Pages of ended sessions a backfill reads, at most (of `convo::store::ENDED_MAX`).
const BACKFILL_PAGES: usize = 1000;

/// A name as it goes in a key: itself when it's a plain name, else its SHA-256.
fn plain(name: &str) -> String {
    if !name.is_empty() && name.len() <= 128 && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) && name != "." && name != ".." {
        name.to_owned()
    } else {
        crate::util::sha256_hex(name.as_bytes())
    }
}

/// Where lines `first` to `last` of a session's record from `source` are kept.
pub fn key(session: &str, source: &str, first: u64, last: u64) -> String {
    format!("transcripts/{}/{}/{first}-{last}.jsonl.br", plain(session), plain(source))
}

/// An archived piece of a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Piece {
    pub source: String,
    pub first: u64,
    pub last: u64,
    #[serde(skip)]
    pub key: String,
    pub lines: u64,
    /// As kept: compressed.
    pub bytes: u64,
    /// Unpacked: its lines and their newlines.
    pub raw: u64,
    /// A reset starts a new immutable source history. Legacy archives have no proven identity.
    pub incarnation: Option<i64>,
    #[serde(skip)]
    codec: String,
    #[serde(skip)]
    sha256: Option<String>,
}

/// Scheduling hint only. The accepted close and transcript mutations already
/// committed their exact obligation. Inventory absence never creates a close.
pub fn closed(hub: &Hub, _session: &str) {
    if let Some(convo)=&hub.harness.convo { convo.wake_maintenance(); }
}

fn piece_row(r: &crate::sql::Row<'_>) -> crate::sql::Result<Piece> {
    Ok(Piece {source:r.get(0)?,first:r.get(1)?,last:r.get(2)?,key:r.get(3)?,lines:r.get(4)?,bytes:r.get(5)?,raw:r.get(6)?,incarnation:r.get(7)?,codec:r.get(8)?,sha256:r.get(9)?})
}

/// The latest complete view of every immutable source incarnation. Unknown
/// legacy histories remain readable, but are never positive pruning evidence.
pub async fn archived(hub: &Hub, session: &str) -> anyhow::Result<Vec<Piece>> {
    let session=session.to_owned();
    hub.store.call(move |conn| {
        let mut q=conn.prepare("SELECT p.source,p.first,p.last,p.key,p.lines,p.bytes,p.raw,p.incarnation,p.codec,p.sha256
            FROM transcript_manifests m JOIN convo_retention v ON v.session=m.session AND v.history_rev=m.history_rev JOIN transcript_manifest_sources x ON x.capture=m.capture
            JOIN transcript_manifest_pieces r ON r.capture=x.capture AND r.source=x.source
            JOIN transcript_segments p ON p.key=r.key
            WHERE m.session=?1 AND m.state='complete' AND x.complete=1 AND p.state='ready' AND p.incarnation=x.incarnation
            AND NOT EXISTS(SELECT 1 FROM transcript_manifests newer JOIN transcript_manifest_sources nx ON nx.capture=newer.capture
                WHERE newer.session=m.session AND newer.state='complete' AND nx.complete=1 AND nx.source=x.source AND nx.incarnation=x.incarnation
                AND (newer.accepted_rev,newer.close_rev,newer.owner_generation,newer.capture)>(m.accepted_rev,m.close_rev,m.owner_generation,m.capture))")?;
        let mut out=q.query_map([&session],piece_row)?.collect::<crate::sql::Result<Vec<_>>>()?;
        let mut q=conn.prepare("SELECT source,first,last,key,lines,bytes,raw,incarnation,codec,sha256 FROM transcript_segments WHERE session=?1 AND state='legacy' AND NOT EXISTS(SELECT 1 FROM convo_retention v WHERE v.session=transcript_segments.session AND v.history_rev>0)")?;
        out.extend(q.query_map([&session],piece_row)?.collect::<crate::sql::Result<Vec<_>>>()?);
        out.sort_by(|a,b|(&a.source,a.incarnation,a.first).cmp(&(&b.source,b.incarnation,b.first)));
        Ok(out)
    }).await
}

/// Metadata adoption has explicit durable progress. It runs on the existing
/// archive cadence and never certifies the producer's EOF or a per-close archive.
async fn backfill(hub: &Hub, convo: &crate::convo::store::Store) -> anyhow::Result<bool> {
    let authority=convo.maintenance().ok_or(MaintenanceRefusal::Secondary)?;
    authority.require_store(&hub.store)?;
    for _ in 0..BACKFILL_PAGES {
        let authority=authority.clone();
        if hub.store.call(move |conn|crate::convo::retention::backfill_step(conn,&authority)).await? {
            convo.wake_maintenance();
            return Ok(true);
        }
    }
    Ok(false)
}

async fn give_up(hub: &Hub, convo: &crate::convo::store::Store, authority: &Authority) -> anyhow::Result<()> {
    authority.require_store(&hub.store)?;
    let Some(keep)=authority.policy.keep_closed_ms else { return Ok(()) };
    let before=crate::util::now_ms().saturating_sub(keep.saturating_add(authority.policy.hold_extra_ms));
    let authority=authority.clone();
    let given=hub.store.call(move |conn| {
        authority.check(conn,true)?;
        let mut rows=Vec::new();
        for session in receipt::give_up_candidates(conn,before)? {
            let row=conn.query_row("SELECT a.session,a.close_rev,a.accepted_rev FROM transcript_archives a JOIN convo_retention v ON v.session=a.session JOIN convo_sessions s ON s.id=a.session
                WHERE a.session=?1 AND a.status='pending' AND a.failure_ms IS NOT NULL AND a.failure_ms<=?2 AND v.observed_ms<?3
                AND a.close_rev=v.close_rev AND a.accepted_rev=v.accepted_rev AND s.closed_at=v.close_at AND s.epoch=v.close_epoch
                AND a.owner_generation=?4 AND a.policy_version=?5",
                params![session,crate::util::now_ms(),before,authority.generation,authority.policy_version],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?))).optional()?;
            if let Some(row)=row {rows.push(row);}
        }
        for (session,close,accepted) in &rows {
            let n=conn.execute("UPDATE transcript_archives SET status='given-up',done_ms=0,manifest=NULL WHERE session=?1 AND close_rev=?2 AND accepted_rev=?3 AND status='pending' AND owner_generation=?4 AND policy_version=?5",params![session,close,accepted,authority.generation,authority.policy_version])?;
            anyhow::ensure!(n==1,"given-up disposition lost its exact obligation");
        }
        if !rows.is_empty() {conn.execute("UPDATE conversation_maintenance SET prune_again=1 WHERE id=1",[])?;}
        Ok(rows)
    }).await?;
    for (session,_,_) in &given {
        crate::metrics::blobs().archives_given_up.inc();
        tracing::warn!(target:"kiln.blobs",event="given_up",session=%session,"the exact closed transcript snapshot exceeded its archive failure holding policy");
    }
    if !given.is_empty() { convo.wake_maintenance(); }
    Ok(())
}

/// The same archive owner resumes bounded metadata initialization and maintenance.
/// No secondary client starts a competing archive/retention controller.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let Some(convo)=hub.harness.convo.clone() else {return Ok(())};
    let Some(authority)=convo.maintenance() else {return Ok(())};
    if let Err(error)=authority.require_store(&hub.store) {
        if error.downcast_ref::<MaintenanceRefusal>().is_some_and(|e|matches!(e,MaintenanceRefusal::UnsupportedWriterBoundary)) {
            tracing::warn!(target:"kiln.blobs",event="writer_boundary","conversation maintenance requires an original standalone Store writer boundary; this owner mode remains unsupported");
            return Ok(());
        }
        return Err(error);
    }
    loop {
        let owner=authority.clone();
        let state=hub.store.call(move |conn|owner.state(conn)).await?;
        if matches!(state,crate::convo::retention::AuthorityState::Stale) {return Ok(());}
        match backfill(&hub,&convo).await {
            Ok(true)=>{
                let owner=authority.clone();
                if matches!(hub.store.call(move |conn|owner.state(conn)).await?,crate::convo::retention::AuthorityState::UnsupportedWriterBoundary) {
                    tracing::warn!(target:"kiln.blobs",event="writer_boundary","conversation destructive maintenance remains unready: original Store writer boundary is unsupported");
                    return Ok(());
                }
                pass(&hub,&convo).await?;
            },
            Ok(false)=>{},
            Err(error) if error.downcast_ref::<MaintenanceRefusal>().is_some_and(|e|matches!(e,MaintenanceRefusal::Stale))=>return Ok(()),
            Err(error)=>tracing::warn!(target:"kiln.blobs",event="adopt",error=%format!("{error:#}"),"archive coverage remains unready; exact progress retained"),
        }
        crate::rt::sleep(EVERY).await;
    }
}

fn definitive_archive_failure(error: &anyhow::Error) -> bool {
    // A timed-out SQL/remote blob outcome is not evidence of a definite failed
    // archive. It clears any earlier failure disposition rather than enabling loss.
    error.downcast_ref::<UnresolvedPut>().is_none() && !error.chain().any(|cause|cause.downcast_ref::<More>().is_some() || cause.downcast_ref::<amphora_api::Refusal>().is_some()
        || cause.downcast_ref::<MaintenanceRefusal>().is_some()
        || cause.downcast_ref::<Refusal>().is_some_and(|e|matches!(e,Refusal::Unavailable(_))))
}

/// A failure settles only the exact current capture disposition. Unknown
/// outcomes clear an earlier definite failure; they never authorize giving up.
async fn retry(hub:&Hub,authority:&Authority,session:&str,close:i64,accepted:i64,error:&anyhow::Error)->anyhow::Result<()> {
    authority.require_store(&hub.store)?;
    let (owner,session,definite)=(authority.clone(),session.to_owned(),definitive_archive_failure(error));
    hub.store.call(move |conn| {
        owner.check(conn,true)?;
        let now=crate::util::now_ms();
        conn.execute("UPDATE transcript_archives SET due_ms=?4,failure_ms=?5 WHERE session=?1 AND close_rev=?2 AND accepted_rev=?3 AND status='pending' AND owner_generation=?6 AND policy_version=?7",
            params![session,close,accepted,now.saturating_add(RETRY_MS),definite.then_some(now),owner.generation,owner.policy_version])?;
        Ok(())
    }).await
}

async fn pass(hub: &Hub, convo: &crate::convo::store::Store) -> anyhow::Result<()> {
    let authority=convo.maintenance().ok_or(MaintenanceRefusal::Secondary)?;
    authority.require_store(&hub.store)?;
    receipt::collect(hub,&authority).await?;
    give_up(hub,convo,&authority).await?;
    let owner=authority.clone();
    let due=hub.store.call(move |conn| {
        owner.check(conn,true)?;
        receipt::due(conn,&owner,crate::util::now_ms())
    }).await?;
    for (session,close,accepted) in due {
        match archive(hub,convo,&session).await {
            Ok(_)=>convo.wake_maintenance(),
            Err(error)=>{
                tracing::warn!(target:"kiln.blobs",event="put",session=%session,error=%format!("{error:#}"),"archive did not confirm this snapshot; current bytes retained");
                retry(hub,&authority,&session,close,accepted,&error).await?;
            }
        }
    }
    Ok(())
}

/// Archives the accepted closed snapshot, reusing verified unchanged pieces.
/// Returns the number of newly written immutable pieces.
pub async fn archive(hub: &Hub, convo: &crate::convo::store::Store, session: &str) -> anyhow::Result<usize> {
    archive_in(hub, convo, session, LIMITS).await
}

async fn archive_in(hub: &Hub, convo: &crate::convo::store::Store, session: &str, limits: Limits) -> anyhow::Result<usize> {
    let authority=convo.maintenance().ok_or(MaintenanceRefusal::Secondary)?;
    authority.require_store(&hub.store)?;
    let capture=receipt::begin(hub,authority,session).await?;
    let (mut after,mut written)=(None,0usize);
    loop {
        let sources=receipt::sources(hub,&capture,after.clone()).await?;
        if sources.is_empty() {break;}
        for source in &sources {
            if !source.complete {
                match pieces(hub,convo,&capture,source,limits).await {
                    Ok(n)=>written=written.checked_add(n).ok_or_else(||anyhow::anyhow!("archive piece count overflow"))?,
                    Err(error)=>{
                        if error.downcast_ref::<UnresolvedPut>().is_some() && let Err(refusal)=receipt::abandon(hub,&capture).await {
                            return Err(error.context(format!("unresolved capture pointer could not be released: {refusal:#}")));
                        }
                        return Err(error);
                    },
                }
            }
        }
        after=sources.last().map(|s|s.source.clone());
    }
    receipt::finish(hub,&capture).await?;
    Ok(written)
}

const FRAME_CODEC: &str="rows+br-v1";
#[derive(Debug)]
struct More;
impl std::fmt::Display for More {fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("archive page budget reached; exact confirmed prefix retained")}}
impl std::error::Error for More {}

#[derive(Debug)]
struct UnresolvedPut;
impl std::fmt::Display for UnresolvedPut {fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("original immutable archive PUT has unresolved custody; its key is retained")}}
impl std::error::Error for UnresolvedPut {}

/// At most one existing piece plus one canonical read page is decoded at once.
/// Compression, digest work and old-piece decoding run outside the SQL writer.
struct BaseReader {
    capture: receipt::Capture,
    source: receipt::Source,
    after: Option<i64>,
    pending: std::collections::VecDeque<receipt::StoredPiece>,
    rows: std::collections::VecDeque<(u64,String)>,
    done: bool,
    from: u64,
    pages: usize,
    expected_rows: u64,
    seen_rows: u64,
}
impl BaseReader {
    fn new(capture:&receipt::Capture,source:&receipt::Source,from:u64,expected_rows:u64)->Self {
        Self {capture:capture.clone(),source:source.clone(),after:None,pending:Default::default(),rows:Default::default(),done:false,from,pages:0,expected_rows,seen_rows:0}
    }
    async fn piece(&mut self,hub:&Hub)->anyhow::Result<Option<receipt::StoredPiece>> {
        loop {
            if let Some(piece)=self.pending.pop_front() {
                self.seen_rows=self.seen_rows.checked_add(piece.piece.lines).ok_or_else(||anyhow::anyhow!("archive prefix row count overflow"))?;
                if piece.piece.last>=self.from {return Ok(Some(piece));}
                continue;
            }
            if self.done {anyhow::ensure!(self.seen_rows==self.expected_rows,"archive prefix omitted confirmed rows");return Ok(None);}
            self.pages=self.pages.checked_add(1).ok_or_else(||anyhow::anyhow!("archive metadata page overflow"))?;
            if self.pages>PAGES_MAX {return Err(More.into());}
            let page=receipt::base_page(hub,&self.capture,&self.source,self.after).await?;
            self.after=page.last().map(|p|i64::try_from(p.piece.first)).transpose()?;
            self.done=page.len()<receipt::BASE_PAGE;
            self.pending=page.into();
        }
    }
    async fn row(&mut self,hub:&Hub)->anyhow::Result<Option<(u64,String)>> {
        loop {
            if let Some(row)=self.rows.pop_front() {if row.0>=self.from {return Ok(Some(row));}else{continue;}}
            let Some(piece)=self.piece(hub).await? else {return Ok(None)};
            self.rows=read_piece(hub,&piece).await?.into();
            receipt::validate(hub,&self.capture).await?;
        }
    }
}

struct CanonicalReader {
    store: crate::convo::store::Store,
    capture: receipt::Capture,
    source: String,
    from: u64,
    rows: std::collections::VecDeque<(u64,String)>,
    done: bool,
    pages: usize,
}
impl CanonicalReader {
    async fn row(&mut self,hub:&Hub)->anyhow::Result<Option<(u64,String)>> {
        loop {
            if let Some(row)=self.rows.pop_front() {return Ok(Some(row));}
            if self.done {return Ok(None);}
            if self.from>i64::MAX as u64 {self.done=true;receipt::validate(hub,&self.capture).await?;return Ok(None);}
            self.pages=self.pages.checked_add(1).ok_or_else(||anyhow::anyhow!("archive record page overflow"))?;
            if self.pages>PAGES_MAX {return Err(More.into());}
            let (store,session,source,from)=(self.store.clone(),self.capture.session.clone(),self.source.clone(),self.from);
            let page=crate::rt::blocking(move ||store.record(&session,&source,from,PAGE)).await??;
            receipt::validate(hub,&self.capture).await?;
            let mut prior=None;
            for (n,_) in &page {
                anyhow::ensure!(*n>=self.from && prior.is_none_or(|p|*n>p),"canonical archive page did not advance");
                prior=Some(*n);
            }
            if let Some(last)=prior {self.from=last.checked_add(1).ok_or_else(||anyhow::anyhow!("archive line number overflow"))?;}else{self.done=true;}
            self.rows=page.into();
        }
    }
}

struct Making {rows:Vec<(u64,String)>,raw:u64}
impl Making {
    fn new()->Self {Self {rows:Vec::new(),raw:0}}
    fn full(&self,limits:Limits)->bool {u64::try_from(self.rows.len()).map_or(true,|n|n>=limits.lines) || self.raw>=limits.bytes}
    fn push(&mut self,row:(u64,String))->anyhow::Result<()> {
        self.raw=self.raw.checked_add(u64::try_from(row.1.len())?).and_then(|n|n.checked_add(1)).ok_or_else(||anyhow::anyhow!("archive size overflow"))?;
        self.rows.push(row);Ok(())
    }
}

async fn pieces(hub:&Hub,convo:&crate::convo::store::Store,capture:&receipt::Capture,source:&receipt::Source,limits:Limits)->anyhow::Result<usize> {
    anyhow::ensure!(limits.lines>0 && limits.bytes>0,"archive piece limits must be positive");
    let (mut count,mut last)=receipt::progress(hub,capture,source).await?;
    let mut from=last.map(|n|n.checked_add(1).ok_or_else(||anyhow::anyhow!("archive line number overflow"))).transpose()?.unwrap_or(0);
    let expected=receipt::base_rows(hub,capture,source).await?;
    let mut base=BaseReader::new(capture,source,from,expected);
    // Reuse only a wholly unaffected positive prefix. A changed old n causes
    // its old piece/tail to be decoded and merged, including pruned older rows.
    let mut overlap=None;
    while let Some(piece)=base.piece(hub).await? {
        let unaffected=source.dirty_from.is_none_or(|n|piece.piece.last<u64::try_from(n).unwrap_or(0));
        if unaffected && piece.piece.first>=from {
            receipt::reuse(hub,capture,&piece).await?;
            count=count.checked_add(piece.piece.lines).ok_or_else(||anyhow::anyhow!("archive row count overflow"))?;
            last=Some(piece.piece.last);
            from=piece.piece.last.checked_add(1).ok_or_else(||anyhow::anyhow!("archive line number overflow"))?;
            base.from=from;
        }else{overlap=Some(piece);break;}
    }
    if let Some(piece)=overlap {base.rows=read_piece(hub,&piece).await?.into();receipt::validate(hub,capture).await?;}
    base.from=from;
    let mut current=CanonicalReader {store:convo.clone(),capture:capture.clone(),source:source.source.clone(),from,rows:Default::default(),done:false,pages:0};
    let (mut old,mut new)=(base.row(hub).await?,current.row(hub).await?);
    let (mut making,mut written)=(Making::new(),0usize);
    loop {
        let row=match (old.as_ref(),new.as_ref()) {
            (None,None)=>break,
            (Some(a),Some(b)) if b.0<=a.0=>{
                if b.0==a.0 {old=base.row(hub).await?;}
                let row=new.take().ok_or_else(||anyhow::anyhow!("archive merge lost its canonical row"))?;
                new=current.row(hub).await?;row
            },
            (_,Some(_)) if old.is_none()=>{
                let row=new.take().ok_or_else(||anyhow::anyhow!("archive merge lost its canonical row"))?;
                new=current.row(hub).await?;row
            },
            _=>{let row=old.take().ok_or_else(||anyhow::anyhow!("archive merge lost its prefix row"))?;old=base.row(hub).await?;row},
        };
        if making.full(limits) {
            write_piece(hub,capture,source,making).await?;
            written=written.checked_add(1).ok_or_else(||anyhow::anyhow!("archive piece count overflow"))?;
            making=Making::new();
        }
        anyhow::ensure!(last.is_none_or(|n|row.0>n),"archive merged rows are not ascending");
        last=Some(row.0);count=count.checked_add(1).ok_or_else(||anyhow::anyhow!("archive row count overflow"))?;
        making.push(row)?;
    }
    if !making.rows.is_empty() {write_piece(hub,capture,source,making).await?;written=written.checked_add(1).ok_or_else(||anyhow::anyhow!("archive piece count overflow"))?;}
    receipt::source_done(hub,capture,source,count,last).await?;
    Ok(written)
}

async fn write_piece(hub:&Hub,capture:&receipt::Capture,source:&receipt::Source,making:Making)->anyhow::Result<()> {
    let (bytes,indices,raw,digest)=crate::rt::blocking(move ||->anyhow::Result<_> {
        let Making {rows,raw}=making;
        anyhow::ensure!(!rows.is_empty(),"an empty archive piece has no range");
        let mut enc=brotli::CompressorWriter::new(Vec::new(),1<<16,QUALITY,22);
        let mut indices=Vec::with_capacity(rows.len());
        let mut framed=0u64;
        for (n,line) in rows {
            let length=u64::try_from(line.len())?;
            framed=framed.checked_add(16).and_then(|v|v.checked_add(length)).ok_or_else(||anyhow::anyhow!("archive frame overflow"))?;
            anyhow::ensure!(framed<=UNPACKED_MAX,"archive piece exceeds its bounded decode budget");
            enc.write_all(&n.to_le_bytes())?;enc.write_all(&length.to_le_bytes())?;enc.write_all(line.as_bytes())?;
            indices.push(n);
        }
        let bytes=enc.into_inner();let digest=crate::util::sha256_hex(&bytes);
        Ok((bytes,indices,raw,digest))
    }).await??;
    let first=*indices.first().ok_or_else(||anyhow::anyhow!("archive piece has no first row"))?;
    let last=*indices.last().ok_or_else(||anyhow::anyhow!("archive piece has no last row"))?;
    let key=format!("transcripts/v2/{}/{}/{}/{}/{}-{}-{}.rows.br",plain(&capture.session),plain(&source.source),source.incarnation,plain(&capture.id),first,last,digest);
    let piece=receipt::StoredPiece {piece:Piece {source:source.source.clone(),first,last,key:key.clone(),lines:u64::try_from(indices.len())?,bytes:u64::try_from(bytes.len())?,raw,incarnation:Some(source.incarnation),codec:FRAME_CODEC.to_owned(),sha256:Some(digest.clone())},indices};
    let already=receipt::prepare_piece(hub,capture,&piece).await?;
    let meta=match already {
        receipt::PieceAdmission::Unresolved=>return Err(UnresolvedPut.into()),
        receipt::PieceAdmission::Ready=>hub.blobs.blobs.head(&key).await?,
        receipt::PieceAdmission::New=>{
        match hub.blobs.blobs.put(&key,bytes,Put {cond:Cond::IfNoneMatch,sha256:Some(digest.clone())}).await {
            Ok(meta)=>meta,
            Err(Refusal::PreconditionFailed(_))=>hub.blobs.blobs.head(&key).await?,
            Err(error@Refusal::Unavailable(_))=>return Err(anyhow::Error::from(error).context(UnresolvedPut)),
            Err(error)=>return Err(error.into()),
        }
        },
    };
    anyhow::ensure!(meta.key==key && meta.size==piece.piece.bytes && meta.etag.eq_ignore_ascii_case(&digest),"archive object did not confirm its exact immutable bytes");
    receipt::piece_ready(hub,capture,&piece).await?;
    Ok(())
}

async fn read_piece(hub:&Hub,piece:&receipt::StoredPiece)->anyhow::Result<Vec<(u64,String)>> {
    let p=piece.piece.clone();
    anyhow::ensure!(p.codec==FRAME_CODEC,"unverified legacy range cannot certify a retained prefix");
    let (meta,bytes)=crate::blobs::io::get_all(hub.blobs.blobs.as_ref(),&p.key,p.bytes).await?;
    let expected=piece.indices.clone();
    crate::rt::blocking(move ||->anyhow::Result<_> {
        let digest=crate::util::sha256_hex(&bytes);
        anyhow::ensure!(p.sha256.as_ref()==Some(&digest) && meta.etag.eq_ignore_ascii_case(&digest) && meta.size==p.bytes,"archive prefix digest differs");
        let rows=decode_frames(&bytes)?;
        anyhow::ensure!(rows.iter().map(|(n,_)|*n).eq(expected.into_iter()),"archive prefix row identities differ");
        let raw=rows.iter().try_fold(0u64,|n,(_,line)|n.checked_add(u64::try_from(line.len()).ok()?).and_then(|n|n.checked_add(1))).ok_or_else(||anyhow::anyhow!("archive decoded size overflow"))?;
        anyhow::ensure!(raw==p.raw && u64::try_from(rows.len())?==p.lines,"archive prefix size differs");
        Ok(rows)
    }).await?
}

fn decode_frames(bytes:&[u8])->anyhow::Result<Vec<(u64,String)>> {
    let mut raw=Vec::new();
    brotli::Decompressor::new(bytes,1<<16).take(UNPACKED_MAX.checked_add(1).ok_or_else(||anyhow::anyhow!("archive decode bound overflow"))?).read_to_end(&mut raw)?;
    anyhow::ensure!(u64::try_from(raw.len())?<=UNPACKED_MAX,"archive decoded beyond its budget");
    let (mut rows,mut at,mut prior)=(Vec::new(),0usize,None);
    while at<raw.len() {
        let header_end=at.checked_add(16).ok_or_else(||anyhow::anyhow!("archive frame overflow"))?;
        let header=raw.get(at..header_end).ok_or_else(||anyhow::anyhow!("archive frame is truncated"))?;
        let n=u64::from_le_bytes(header.get(..8).ok_or_else(||anyhow::anyhow!("archive row number is missing"))?.try_into()?);
        let len=u64::from_le_bytes(header.get(8..).ok_or_else(||anyhow::anyhow!("archive row length is missing"))?.try_into()?);
        anyhow::ensure!(len<=u64::try_from(crate::convo::TRANSCRIPT_LINE_MAX)?,"archive row exceeds its budget");
        anyhow::ensure!(prior.is_none_or(|p|n>p),"archive frame row order differs");
        let end=header_end.checked_add(usize::try_from(len)?).ok_or_else(||anyhow::anyhow!("archive row size overflow"))?;
        let line=std::str::from_utf8(raw.get(header_end..end).ok_or_else(||anyhow::anyhow!("archive row is truncated"))?)?.to_owned();
        anyhow::ensure!(u64::try_from(rows.len())?<PIECE_LINES,"archive frame row count exceeds its budget");
        rows.push((n,line));prior=Some(n);at=end;
    }
    Ok(rows)
}

/// A source's archived lines, in order, as JSON lines: each piece read and unpacked in turn.
pub fn lines(hub: &Arc<Hub>, pieces: Vec<Piece>) -> async_channel::Receiver<bytes::Bytes> {
    let (tx, rx) = async_channel::bounded(4);
    let hub = Arc::clone(hub);
    crate::rt::spawn(async move {
        for p in pieces {
            let packed = match crate::blobs::io::get_all(hub.blobs.blobs.as_ref(), &p.key, p.bytes).await {
                Ok((_, b)) => b,
                Err(e) => {
                    tracing::warn!(target: "kiln.blobs", event = "get", key = %p.key, error = %e, "an archived record ends short");
                    return;
                }
            };
            let decoding=p.clone();
            let text = crate::rt::blocking(move || -> anyhow::Result<Vec<u8>> {
                if decoding.codec==FRAME_CODEC {
                    let digest=crate::util::sha256_hex(&packed);
                    anyhow::ensure!(decoding.sha256.as_ref()==Some(&digest),"archive readback digest differs");
                    let rows=decode_frames(&packed)?;
                    anyhow::ensure!(u64::try_from(rows.len())?==decoding.lines,"archive readback row count differs");
                    let mut out=Vec::new();
                    for (_,line) in rows {out.extend_from_slice(line.as_bytes());out.push(b'\n');}
                    anyhow::ensure!(u64::try_from(out.len())?==decoding.raw,"archive readback size differs");
                    Ok(out)
                }else{
                    anyhow::ensure!(decoding.codec==CODEC,"archive codec is unknown");
                    let mut out=Vec::new();
                    brotli::Decompressor::new(packed.as_slice(),1<<16).take(UNPACKED_MAX.checked_add(1).ok_or_else(||anyhow::anyhow!("archive decode bound overflow"))?).read_to_end(&mut out)?;
                    anyhow::ensure!(u64::try_from(out.len())?<=UNPACKED_MAX && u64::try_from(out.len())?==decoding.raw,"legacy archive readback size differs");
                    Ok(out)
                }
            }).await;
            let Ok(Ok(text)) = text else {
                tracing::warn!(target: "kiln.blobs", event = "get", key = %p.key, "an archived piece couldn't be unpacked whole; the record ends short");
                return;
            };
            if tx.send(bytes::Bytes::from(text)).await.is_err() {
                return;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(session: &str, lines: std::ops::Range<u64>) -> Vec<crate::convo::rows::Row> {
        lines.map(|n| crate::convo::rows::Row::Record(Box::new(crate::convo::rows::RecordRow { session: session.into(), source: "cc".into(), n, end: n + 1, line: format!("{{\"n\":{n},\"text\":\"{}\"}}", "word ".repeat(20)), kept: None }))).collect()
    }

    async fn read_back(hub: &Arc<Hub>, session: &str) -> anyhow::Result<Vec<String>> {
        let rx = lines(hub, archived(hub, session).await?);
        let mut text = Vec::new();
        while let Ok(b) = rx.recv().await {
            text.extend_from_slice(&b);
        }
        Ok(String::from_utf8(text)?.lines().map(str::to_owned).collect())
    }

    fn texts(session: &str, lines: std::ops::Range<u64>) -> Vec<String> {
        record(session, lines).into_iter().filter_map(|r| if let crate::convo::rows::Row::Record(r) = r { Some(r.line) } else { None }).collect()
    }

    fn closed_rows(session:&str,lines:std::ops::Range<u64>,at:i64)->Vec<crate::convo::rows::Row> {
        let mut rows=vec![crate::convo::rows::Row::Session {session:session.into(),info:Some("{}".into()),meta:None,state:None,changes:None,epoch:None,open:true,at}];
        rows.extend(record(session,lines));
        rows.push(crate::convo::rows::Row::Closed {session:session.into(),at});
        rows
    }

    async fn accepted(convo:&crate::convo::store::Store,session:&str,rows:Vec<crate::convo::rows::Row>)->anyhow::Result<()> {
        let mut rows=rows.into_iter().peekable();
        while rows.peek().is_some() {
            // Respect the real staged-read/SQL work allowance, including the
            // receipt for each chunk. No admission-only wait is called durable.
            let batch=crate::convo::batch::Batch::projection(session.into(),rows.by_ref().take(6).collect()).map_err(|e|anyhow::anyhow!("{:?}",e.reason))?;
            let ticket=convo.try_admit(batch).map_err(|e|anyhow::anyhow!("{:?}",e.reason))?;
            ticket.wait().await.map_err(|e|anyhow::anyhow!("{e:?}"))?;
        }
        Ok(())
    }

    async fn ready(hub:&Hub,convo:&crate::convo::store::Store)->anyhow::Result<()> {
        let authority=convo.maintenance().ok_or(MaintenanceRefusal::Secondary)?;
        authority.require_store(&hub.store)?;
        anyhow::ensure!(backfill(hub,convo).await?,"scratch metadata did not finish adoption");
        Ok(())
    }

    async fn age(hub:&Hub,session:&str,old:i64)->anyhow::Result<()> {
        let session=session.to_owned();
        hub.store.call(move |c| {
            c.execute("UPDATE convo_retention SET observed_ms=?2 WHERE session=?1",params![session,old])?;
            c.execute("UPDATE transcript_archives SET closed_ms=?2,due_ms=0 WHERE session=?1",params![session,old])?;
            Ok(())
        }).await
    }

    async fn pruned(convo:&crate::convo::store::Store,session:&str)->anyhow::Result<()> {
        let (c,session)=(convo.clone(),session.to_owned());
        crate::rt::blocking(move || ->anyhow::Result<()> {
            let until=std::time::Instant::now()+Duration::from_secs(30);
            // Woken as the archive's owner wakes it, and asked more seldom than the store's writer
            // waits, idle, before it looks again (2 s): its upkeep runs only while nothing is asked.
            loop {
                c.wake_maintenance();
                if c.marks(&session)?.is_empty() { return Ok(()); }
                anyhow::ensure!(std::time::Instant::now()<until,"owner retention did not prune its positively archived snapshot");
                std::thread::sleep(Duration::from_millis(2_200));
            }
        }).await?
    }

    /// CP-T10.1/U05: immutable pieces retain every accepted line, respect both
    /// bounds, and do not adopt an unrelated object at the old range key.
    #[test]
    fn a_closed_conversations_record_is_archived_in_pieces_never_replaced()->anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub=crate::hub::tests::scratch_hub().await?;
            let convo=hub.harness.convo.clone().ok_or_else(||anyhow::anyhow!("no conversation store"))?;
            ready(&hub,&convo).await?;
            let now=crate::util::now_ms();
            let small=Limits {lines:100,bytes:1<<20};
            accepted(&convo,"s1",closed_rows("s1",0..2_250,now)).await?;
            assert_eq!(archive_in(&hub,&convo,"s1",small).await?,23,"22 full pieces, one the rest");
            let pieces=archived(&hub,"s1").await?;
            assert_eq!(pieces.first().map(|p|(p.first,p.last,p.lines)),Some((0,99,100)));
            assert_eq!(pieces.last().map(|p|(p.first,p.last,p.lines)),Some((2_200,2_249,50)));
            let packed:u64=pieces.iter().map(|p|p.bytes).sum();
            assert!(packed>0 && packed<2_250*120/4,"{packed} bytes packed");
            assert_eq!(read_back(&hub,"s1").await?,texts("s1",0..2_250));
            assert_eq!(archive_in(&hub,&convo,"s1",small).await?,0);
            accepted(&convo,"s1",closed_rows("s1",2_250..2_260,now)).await?;
            assert_eq!(archive_in(&hub,&convo,"s1",small).await?,1);
            assert_eq!(read_back(&hub,"s1").await?,texts("s1",0..2_260));
            accepted(&convo,"s3",closed_rows("s3",0..50,now)).await?;
            assert_eq!(archive_in(&hub,&convo,"s3",Limits {lines:1_000,bytes:1_000}).await?,6);
            assert!(archived(&hub,"s3").await?.iter().all(|p|p.lines<=9));
            let old_key=key("s2","cc",0,2);
            hub.blobs.blobs.put(&old_key,b"kept before".to_vec(),Put::default()).await?;
            accepted(&convo,"s2",closed_rows("s2",0..3,now)).await?;
            assert_eq!(archive(&hub,&convo,"s2").await?,1);
            assert_eq!(hub.blobs.blobs.get(&old_key,None).await?.1,b"kept before".to_vec());
            assert_eq!(read_back(&hub,"s2").await?,texts("s2",0..3));
            Ok(())
        })
    }

    /// CP-T10.1/U05: secondary maintenance cannot delete a current snapshot.
    /// A late replacement after pruning is merged with the archived prefix; a
    /// reset starts a different history even when its line numbers are reused.
    #[test]
    fn the_conversation_store_holds_records_until_they_are_archived()->anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub=crate::hub::tests::scratch_hub().await?;
            let convo=hub.harness.convo.clone().ok_or_else(||anyhow::anyhow!("no conversation store"))?;
            ready(&hub,&convo).await?;
            let old=crate::util::now_ms().saturating_sub(31*86_400_000);
            accepted(&convo,"runner-held",closed_rows("runner-held",0..3,old)).await?;
            age(&hub,"runner-held",old).await?;
            let database=hub.store.database();
            let runner=crate::rt::blocking(move ||crate::records::SessionStorage::from_host_database(database,"runner-fixture",&[],crate::agent::runner_conversation_upkeep()).map(|s|s.conversations())).await??;
            let r=runner.clone();
            crate::rt::blocking(move ||->anyhow::Result<()> {
                let until=std::time::Instant::now()+Duration::from_secs(15);
                let mut first_look=0;
                loop {
                    let file=r.report().file;
                    if first_look!=0 && file.at!=0 && file.at!=first_look && matches!(file.upkeep,crate::convo::telemetry::Upkeeping::Idle) {break;}
                    if first_look==0 {first_look=file.at;}
                    anyhow::ensure!(std::time::Instant::now()<until,"runner maintenance did not run");
                    std::thread::sleep(Duration::from_millis(50));
                }
                assert_eq!(r.record("runner-held","cc",0,10)?.len(),3,"secondary maintenance deleted unarchived canonical bytes");
                Ok(())
            }).await??;
            pass(&hub,&convo).await?;
            pruned(&convo,"runner-held").await?;
            assert_eq!(read_back(&hub,"runner-held").await?,texts("runner-held",0..3));
            let changed="{\"replacement\":true}".to_owned();
            accepted(&convo,"runner-held",vec![crate::convo::rows::Row::Record(Box::new(crate::convo::rows::RecordRow {session:"runner-held".into(),source:"cc".into(),n:1,end:2,line:changed.clone(),kept:None}))]).await?;
            assert_eq!(convo.record("runner-held","cc",0,10)?.len(),1);
            archive(&hub,&convo,"runner-held").await?;
            let mut expected=texts("runner-held",0..3);
            let middle=expected.get_mut(1).ok_or_else(||anyhow::anyhow!("missing fixture middle"))?;*middle=changed;
            assert_eq!(read_back(&hub,"runner-held").await?,expected,"replaced row keeps the pruned prefix and suffix");
            pruned(&convo,"runner-held").await?;
            let mut reset=vec![crate::convo::rows::Row::RecordReset {session:"runner-held".into(),source:"cc".into(),at:old}];
            reset.extend(record("runner-held",0..1));
            accepted(&convo,"runner-held",reset).await?;
            archive(&hub,&convo,"runner-held").await?;
            expected.extend(texts("runner-held",0..1));
            assert_eq!(read_back(&hub,"runner-held").await?,expected,"reset preserves the old history and archives a fresh incarnation");
            Ok(())
        })
    }

    /// CP-T10.1/U05: expiry revokes only its captured history before collection;
    /// a later close is not deleted. An unknown result cannot turn into give-up.
    #[test]
    fn archives_go_past_their_keeping_and_are_given_up_past_their_hold()->anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub=crate::hub::tests::scratch_hub().await?;
            let convo=hub.harness.convo.clone().ok_or_else(||anyhow::anyhow!("no conversation store"))?;
            ready(&hub,&convo).await?;
            let owner=convo.maintenance().ok_or(MaintenanceRefusal::Secondary)?;
            let now=crate::util::now_ms();
            let old=now.saturating_sub(366*86_400_000);
            accepted(&convo,"s-old",closed_rows("s-old",0..3,old)).await?;
            age(&hub,"s-old",old).await?;
            archive(&hub,&convo,"s-old").await?;
            let key=archived(&hub,"s-old").await?.first().map(|p|p.key.clone()).ok_or_else(||anyhow::anyhow!("not archived"))?;
            pruned(&convo,"s-old").await?;
            // Old pending obligations are not expiry evidence. A bounded look
            // skips them, then resumes to the eligible history on its next turn.
            for n in 0..100 {
                let session=format!("pending-{n}");
                accepted(&convo,&session,closed_rows(&session,0..0,old)).await?;
                age(&hub,&session,old.saturating_sub(1)).await?;
            }
            receipt::expire(&hub,&owner).await?;
            assert_eq!(read_back(&hub,"s-old").await?,texts("s-old",0..3),"one bounded turn did not pass the rejected candidate population");
            receipt::expire(&hub,&owner).await?;
            assert!(archived(&hub,"s-old").await?.is_empty(),"the next bounded turn did not retire its eligible history");
            accepted(&convo,"s-old",closed_rows("s-old",3..6,now)).await?;
            archive(&hub,&convo,"s-old").await?;
            receipt::collect(&hub,&owner).await?;
            assert!(matches!(hub.blobs.blobs.head(&key).await,Err(Refusal::NotFound(_))));
            assert_eq!(read_back(&hub,"s-old").await?,texts("s-old",3..6),"old collection did not delete the new close");
            let held=now.saturating_sub(61*86_400_000);
            accepted(&convo,"s-held",closed_rows("s-held",0..1,held)).await?;
            age(&hub,"s-held",held).await?;
            let capture=receipt::begin(&hub,owner.clone(),"s-held").await?;
            let unknown:anyhow::Error=amphora_api::Refusal::UnknownOutcome.into();
            retry(&hub,&owner,"s-held",capture.close_rev,capture.accepted_rev,&unknown).await?;
            give_up(&hub,&convo,&owner).await?;
            let done:Option<i64>=hub.store.call(|c|Ok(c.query_row("SELECT done_ms FROM transcript_archives WHERE session='s-held'",[],|r|r.get(0))?)).await?;
            assert_eq!(done,None,"unknown outcome must retain the accepted snapshot");
            let refused:anyhow::Error=Refusal::Forbidden("fixture definite refusal".into()).into();
            retry(&hub,&owner,"s-held",capture.close_rev,capture.accepted_rev,&refused).await?;
            give_up(&hub,&convo,&owner).await?;
            let done:Option<i64>=hub.store.call(|c|Ok(c.query_row("SELECT done_ms FROM transcript_archives WHERE session='s-held'",[],|r|r.get(0))?)).await?;
            assert_eq!(done,Some(0),"definite refusal settles only the exact policy-aged obligation");
            Ok(())
        })
    }

    /// CP-T10.1/U05: a completion for an older close cannot settle a reopened
    /// session. The next capture includes every line accepted for the new close.
    #[test]
    fn a_conversation_closed_again_long_after_has_its_new_lines_archived()->anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub=crate::hub::tests::scratch_hub().await?;
            let convo=hub.harness.convo.clone().ok_or_else(||anyhow::anyhow!("no conversation store"))?;
            ready(&hub,&convo).await?;
            let now=crate::util::now_ms();
            accepted(&convo,"s1",closed_rows("s1",0..3,now)).await?;
            archive(&hub,&convo,"s1").await?;
            accepted(&convo,"s1",closed_rows("s1",3..6,now)).await?;
            let owner=convo.maintenance().ok_or(MaintenanceRefusal::Secondary)?;
            let stale=receipt::begin(&hub,owner,"s1").await?;
            for source in receipt::sources(&hub,&stale,None).await? {
                pieces(&hub,&convo,&stale,&source,LIMITS).await?;
            }
            accepted(&convo,"s1",closed_rows("s1",6..9,now)).await?;
            assert!(receipt::finish(&hub,&stale).await.is_err(),"stale completion authorized a different close");
            archive(&hub,&convo,"s1").await?;
            assert_eq!(read_back(&hub,"s1").await?,texts("s1",0..9));
            Ok(())
        })
    }

    /// CP-T10.1/U05: accepted closes have durable obligations without an async
    /// scheduling hint, including a producer clock far behind the host's clock.
    #[test]
    fn conversations_that_ended_before_are_noted_once_and_archived()->anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub=crate::hub::tests::scratch_hub().await?;
            let convo=hub.harness.convo.clone().ok_or_else(||anyhow::anyhow!("no conversation store"))?;
            let old=crate::util::now_ms().saturating_sub(200*86_400_000);
            accepted(&convo,"a",closed_rows("a",0..2,old)).await?;
            accepted(&convo,"b",closed_rows("b",0..3,old)).await?;
            ready(&hub,&convo).await?;
            assert!(backfill(&hub,&convo).await?,"durable readiness is idempotent");
            hub.store.call(|c|Ok(c.execute("UPDATE transcript_archives SET due_ms=0",[])?)).await?;
            pass(&hub,&convo).await?;
            assert_eq!(read_back(&hub,"a").await?,texts("a",0..2));
            assert_eq!(read_back(&hub,"b").await?,texts("b",0..3));
            Ok(())
        })
    }

    /// CP-T10.1: names that aren't plain go into keys as their SHA-256.
    #[test]
    fn keys_hold_plain_names_or_their_hash() {
        assert_eq!(key("s-1", "cc", 0, 9), "transcripts/s-1/cc/0-9.jsonl.br");
        assert_eq!(key("a/b", "..", 3, 4), format!("transcripts/{}/{}/3-4.jsonl.br", crate::util::sha256_hex(b"a/b"), crate::util::sha256_hex(b"..")));
    }
}
