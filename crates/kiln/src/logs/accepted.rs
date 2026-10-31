//! U05: the server's durably accepted, observed job-view prefix. This is deliberately
//! not an attempt identity, a producer sequence, or proof of log-stream closure.
//!
//! One retained operation owns its immutable keys before it writes them. Preparation and
//! Blobs I/O never execute inside the shared SQL writer. Publication is one checked SQL
//! transition; all derived index invalidation commits in that same transition.

use std::sync::Arc;

mod outputs;

use kiln_seams::blobs::{Blobs, Cond, Put, Refusal};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::sql::{Connection, OptionalExtension as _, params};
use crate::store::Store;

pub(crate) const CHUNK: usize = 4 << 20;
pub(crate) const PAGE: usize = 32;
pub(crate) const QUALIFICATION: &str = "historical_snapshot_then_legacy_observed_job_view";
pub(crate) const EMPTY: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Borrow the unchanged legacy postcard shape from its original retained frame. This is
/// observed job-view input only; there is deliberately no inferred attempt/sequence/close.
#[derive(Deserialize)]
pub(crate) struct LegacyChunk<'a> {
    #[serde(borrow)] pub job:&'a str,
    pub idx:u32,
    #[serde(borrow)] pub data:&'a [u8],
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Prefix {
    pub view: String,
    pub revision: i64,
    pub root: String,
}

/// These identities describe the finite historical capture only. They never qualify its
/// past delivery, identify an attempt, or convert an absent file into accepted empty data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct CapturedFile {
    pub step: u32,
    pub bytes: u64,
    pub device: u64,
    pub inode: u64,
    pub modified_ns: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub operation: String,
    pub job: String,
    pub prefix: Prefix,
    pub accepted: bool,
    pub refusal: Option<String>,
}

#[derive(Clone, Debug)]
struct Operation {
    id: String,
    job: String,
    view: String,
    expected_revision: i64,
    expected_root: String,
    staged_revision: i64,
    staged_root: String,
    staged_bytes: u64,
    staged_chunks: i64,
    step: u32,
    input_key: String,
    input_hash: String,
    input_bytes: u64,
    origin: String,
    baseline_host: String,
    baseline_json: String,
    baseline_count: u64,
    baseline_bytes: u64,
    baseline_state: String,
    history_after: Option<i64>,
    file: u64,
    offset: u64,
    input_ready: bool,
    input_staged: bool,
    input_cursor:u64,
    status: String,
    refusal: Option<String>,
    receipt_revision: Option<i64>,
    receipt_root: Option<String>,
}

const OP: &str = "SELECT operation,job,view,expected_revision,expected_root,staged_revision,staged_root,staged_bytes,staged_chunks,step,input_key,input_hash,input_bytes,origin,baseline,baseline_file,baseline_offset,input_ready,input_staged,status,refusal,receipt_revision,receipt_root,baseline_host,baseline_count,baseline_bytes,baseline_state,history_after,input_cursor FROM ci_log_accepted_ops WHERE operation=?1";

fn operation(c: &Connection, id: &str) -> anyhow::Result<Option<Operation>> {
    Ok(c.query_row(OP, [id], |r| {
        Ok(Operation {
            id:r.get(0)?,job:r.get(1)?,view:r.get(2)?,expected_revision:r.get(3)?,expected_root:r.get(4)?,
            staged_revision:r.get(5)?,staged_root:r.get(6)?,staged_bytes:r.get(7)?,staged_chunks:r.get(8)?,
            step:r.get(9)?,input_key:r.get(10)?,input_hash:r.get(11)?,input_bytes:r.get(12)?,origin:r.get(13)?,
            baseline_host:r.get(23)?,baseline_json:r.get(14)?,baseline_count:r.get(24)?,baseline_bytes:r.get(25)?,baseline_state:r.get(26)?,history_after:r.get(27)?,file:r.get(15)?,offset:r.get(16)?,input_ready:r.get(17)?,input_staged:r.get(18)?,input_cursor:r.get(28)?,
            status:r.get(19)?,refusal:r.get(20)?,receipt_revision:r.get(21)?,receipt_root:r.get(22)?,
        })
    }).optional()?)
}

fn prepared(op:Operation)->anyhow::Result<Operation> {
    anyhow::ensure!(op.baseline_json.len()<=1024,"captured directory metadata exceeds its bound");Ok(op)
}

#[derive(Clone)]
pub(super) struct PreparedFile {pub step:u32,pub bytes:u64,pub capture:String}

fn record_files(c:&Connection,id:&str,files:&[PreparedFile],maximum:u64)->anyhow::Result<()> {
    anyhow::ensure!(files.len()<=PAGE,"historical metadata page exceeds its bound");
    let tx=crate::store::tx(c)?;
    let op=operation(&tx,id)?.ok_or_else(||anyhow::anyhow!("original baseline reservation disappeared"))?;
    if op.status!="pending" {return Ok(())}
    anyhow::ensure!(op.baseline_state=="discovering","historical discovery phase changed");
    let (mut count,mut bytes)=(op.baseline_count,op.baseline_bytes);
    for file in files {
        anyhow::ensure!(file.capture.len()<=1024,"historical metadata row exceeds its bound");
        let old:Option<String>=tx.query_row("SELECT capture FROM ci_log_baseline_files WHERE operation=?1 AND step=?2",params![id,file.step],|r|r.get(0)).optional()?;
        if let Some(old)=old {
            if old!=file.capture {refusal_in(&tx,&op,"historical file metadata changed during original capture")?;tx.commit()?;return Ok(())}
            continue;
        }
        let total=bytes.checked_add(file.bytes).and_then(|n|n.checked_add(op.input_bytes));
        if total.is_none_or(|n|n>maximum) {
            refusal_in(&tx,&op,"historical prefix plus new input exceeds the existing job-log byte bound")?;tx.commit()?;return Ok(());
        }
        tx.execute("INSERT INTO ci_log_baseline_files(operation,step,capture,bytes) VALUES(?1,?2,?3,?4)",params![id,file.step,file.capture,file.bytes])?;
        count=count.checked_add(1).ok_or_else(||anyhow::anyhow!("historical catalog count exhausted"))?;
        bytes=bytes.checked_add(file.bytes).ok_or_else(||anyhow::anyhow!("historical byte count exhausted"))?;
    }
    tx.execute("UPDATE ci_log_accepted_ops SET baseline_count=?2,baseline_bytes=?3 WHERE operation=?1",params![id,count,bytes])?;
    tx.commit()?;Ok(())
}

fn history_page(c:&Connection,id:&str,expected:Option<i64>)->anyhow::Result<()> {
    let tx=crate::store::tx(c)?;
    let op=operation(&tx,id)?.ok_or_else(||anyhow::anyhow!("original baseline reservation disappeared"))?;
    if op.status!="pending" || op.baseline_state!="checking" || op.history_after!=expected {return Ok(())}
    let initial="SELECT id,step,line,index_version FROM ci_log_docs WHERE leg=?1 ORDER BY id LIMIT 32";
    let continued="SELECT id,step,line,index_version FROM ci_log_docs WHERE leg=?1 AND id>?2 ORDER BY id LIMIT 32";
    let mut statement=tx.prepare(if op.history_after.is_some(){continued}else{initial})?;
    let map=|r:&crate::sql::Row<'_>|Ok((r.get::<_,i64>(0)?,r.get::<_,u32>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<String>>(3)?));
    let rows=if let Some(after)=op.history_after {
        statement.query_map(params![op.job,after],map)?.collect::<crate::sql::Result<Vec<_>>>()?
    }else{statement.query_map([&op.job],map)?.collect::<crate::sql::Result<Vec<_>>>()?};
    drop(statement);
    for (doc,step,line,version) in &rows {
        if *line<1 || version.as_ref().is_some_and(|version|version.len()>256) {
            refusal_in(&tx,&op,"historical document identity has unsupported bounds")?;tx.commit()?;return Ok(());
        }
        let present:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_baseline_files WHERE operation=?1 AND step=?2)",params![id,step],|r|r.get(0))?;
        if !present {
            refusal_in(&tx,&op,"historical searchable source has no captured file; historical source stays unqualified")?;tx.commit()?;return Ok(());
        }
        tx.execute("INSERT INTO ci_log_baseline_docs(operation,step,line,doc,version) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(operation,step,line,doc) DO NOTHING",params![id,step,line,doc,version])?;
    }
    if let Some((after,_,_,_))=rows.last() {
        tx.execute("UPDATE ci_log_accepted_ops SET history_after=?2 WHERE operation=?1",params![id,after])?;
    }else{
        let old:Option<Option<u64>>=tx.query_row("SELECT bytes FROM ci_log_indexed WHERE leg=?1",[&op.job],|r|r.get(0)).optional()?;
        if op.baseline_bytes==0 && old.is_some_and(|n|n.is_none_or(|n|n>0)) {
            refusal_in(&tx,&op,"historical index records bytes absent from the captured files; no empty accepted baseline")?;
        }else{tx.execute("UPDATE ci_log_accepted_ops SET baseline_state='staging' WHERE operation=?1",[id])?;}
    }
    tx.commit()?;Ok(())
}

async fn baseline(ctx:&Context,mut op:Operation)->anyhow::Result<Operation> {
    if ctx.host!=op.baseline_host {
        refused(ctx,op.id.clone(),"historical capture is owned by another host; no adoption".into()).await;
        return Ok(load(&ctx.store,op.id).await);
    }
    if op.baseline_state=="discovering" {
        let mut capture=match ctx.files.start_capture(&op.job).await {
            Ok(capture)=>capture,
            Err(error)=>{
                refused(ctx,op.id.clone(),format!("historical capture unavailable: {error:#}")).await;
                return Ok(load(&ctx.store,op.id).await);
            }
        };
        let identity=serde_json::to_string(&capture.identity)?;
        if !op.baseline_json.is_empty() && identity!=op.baseline_json {
            refused(ctx,op.id.clone(),"original captured directory was replaced or changed; no historical adoption".into()).await;
            return Ok(load(&ctx.store,op.id).await);
        }
        if op.baseline_json.is_empty() {
            let id=op.id.clone();let check=id.clone();let expected=identity.clone();let observed=identity.clone();
            transition(&ctx.store,move |c|{
                c.execute("UPDATE ci_log_accepted_ops SET baseline=?2 WHERE operation=?1 AND status='pending' AND baseline_state='discovering' AND baseline=''",params![id,expected])?;Ok(())
            },move |c|Ok(operation(c,&check)?.filter(|op|op.baseline_json==observed || op.status!="pending"))).await;
        }
        loop {
            let (next,files,done)=match ctx.files.capture_page(capture).await {
                Ok(page)=>page,
                Err(error)=>{
                    refused(ctx,op.id.clone(),format!("original historical capture changed: {error:#}")).await;
                    return Ok(load(&ctx.store,op.id).await);
                }
            };
            capture=next;
            let id=op.id.clone();let check=id.clone();let page=files.clone();let expected=files.clone();let maximum=ctx.files.max_bytes();
            transition(&ctx.store,move |c|record_files(c,&id,&page,maximum),move |c|{
                let Some(op)=operation(c,&check)? else {return Ok(None)};
                if op.status!="pending" {return Ok(Some(()))}
                for file in &expected {
                    let got:Option<String>=c.query_row("SELECT capture FROM ci_log_baseline_files WHERE operation=?1 AND step=?2",params![check,file.step],|r|r.get(0)).optional()?;
                    if got.as_deref()!=Some(file.capture.as_str()) {return Ok(None)}
                }
                Ok(Some(()))
            }).await;
            op=load(&ctx.store,op.id).await;
            if op.status!="pending" {return Ok(op)}
            if done {break}
        }
        let id=op.id.clone();let check=id.clone();
        transition(&ctx.store,move |c|{c.execute("UPDATE ci_log_accepted_ops SET baseline_state='checking' WHERE operation=?1 AND status='pending' AND baseline_state='discovering'",[&id])?;Ok(())},move |c|Ok(operation(c,&check)?.filter(|op|op.baseline_state!="discovering" || op.status!="pending"))).await;
        op=load(&ctx.store,op.id).await;
    }
    while op.status=="pending" && op.baseline_state=="checking" {
        let id=op.id.clone();let check=id.clone();let after=op.history_after;
        transition(&ctx.store,move |c|history_page(c,&id,after),move |c|Ok(operation(c,&check)?.filter(|op|op.status!="pending" || op.baseline_state!="checking" || op.history_after!=after))).await;
        op=load(&ctx.store,op.id).await;
    }
    Ok(op)
}

async fn next_file(ctx:&Context,op:&Operation)->anyhow::Result<Option<CapturedFile>> {
    let id=op.id.clone();let after=op.file;
    let capture:Option<String>=ctx.store.call(move |c|Ok(c.query_row("SELECT capture FROM ci_log_baseline_files WHERE operation=?1 AND step>=?2 ORDER BY step LIMIT 1",params![id,after],|r|r.get(0)).optional()?)).await?;
    capture.map(|capture|{
        anyhow::ensure!(capture.len()<=1024,"historical metadata row exceeds its bound");
        Ok(serde_json::from_str(&capture)?)
    }).transpose()
}

fn receipt(op: &Operation) -> anyhow::Result<Receipt> {
    anyhow::ensure!(op.status!="pending","pending append has no durable acceptance/refusal receipt");
    Ok(Receipt {
        operation:op.id.clone(),job:op.job.clone(),accepted:op.status=="accepted",refusal:op.refusal.clone(),
        prefix:Prefix {view:op.view.clone(),revision:op.receipt_revision.ok_or_else(||anyhow::anyhow!("append receipt lacks revision"))?,root:op.receipt_root.clone().ok_or_else(||anyhow::anyhow!("append receipt lacks root"))?},
    })
}

pub(crate) fn completed(c:&Connection,id:&str)->anyhow::Result<Option<Receipt>> {
    let Some(op)=operation(c,id)? else {return Ok(None)};
    if op.status=="pending" {return Ok(None)}
    let done=receipt(&op)?;
    if !done.accepted {anyhow::ensure!(done.prefix.revision==op.expected_revision && done.prefix.root==op.expected_root,"original refusal marker conflicts");}
    if done.accepted {
        anyhow::ensure!(done.prefix.revision==op.staged_revision && done.prefix.root==op.staged_root,"original accepted operation marker conflicts");
        let head:Option<(i64,String)>=c.query_row("SELECT revision,root FROM ci_log_accepted_views WHERE view=?1",[&op.view],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let tail:Option<String>=c.query_row("SELECT root FROM ci_log_accepted_chunks WHERE view=?1 AND revision=?2 AND operation=?3",params![op.view,done.prefix.revision,id],|r|r.get(0)).optional()?;
        anyhow::ensure!(tail.as_deref()==Some(done.prefix.root.as_str()) && head.is_some_and(|(revision,root)|revision>done.prefix.revision || (revision==done.prefix.revision && root==done.prefix.root)),"original accepted head/immutable tail is not durably observed");
    }
    Ok(Some(done))
}

pub(crate) fn prefix(c:&Connection,job:&str)->anyhow::Result<Option<Prefix>> {
    Ok(c.query_row("SELECT view,revision,root FROM ci_log_accepted_views WHERE job=?1 AND retired=0 AND revision>0",[job],|r|Ok(Prefix{view:r.get(0)?,revision:r.get(1)?,root:r.get(2)?})).optional()?)
}

#[derive(Clone)]
struct Original {
    id:String,view:String,job:String,step:u32,hash:String,bytes:u64,origin:String,host:String,copy:Option<CopyRequest>,job_order:Option<i64>,
}

enum Reserved { Ready(Box<Operation>), Busy, Refused(String) }

#[derive(Clone)]
struct CopyRequest {
    source:String,step:u32,request_hash:String,legacy:Option<CapturedFile>,directory:String,
    attempt:u32,effect:String,result_hash:String,result_bytes:u64,output_tree:Option<OutputTree>,
}
#[derive(Clone,Debug,Serialize,Deserialize)]
pub(crate) struct OutputTree {pub scope:String,pub digest:String,pub bytes:u64}
pub(crate) struct CopyStep {
    pub job:String,pub step:u32,pub target_attempt:u32,pub effect:String,
    pub source_job:String,pub source_step:u32,pub result:Vec<u8>,pub output_tree:Option<OutputTree>,
}
const RESULT_MAX:usize=16<<20;
const RESULT_PART:usize=64<<10;
const RESULT_PAGE:usize=4;
#[derive(Clone,Debug,Serialize,Deserialize)]
enum CopySource {
    Accepted {job:String,step:u32,prefix:Prefix,ordinal:i64,bytes:u64,root:String},
    Historical {job:String,step:u32,host:String,capture:CapturedFile,directory:String},
    NoBytes {job:String,step:u32,qualification:String},
}
impl CopySource {
    fn bytes(&self)->u64 {match self {Self::Accepted{bytes,..}=>*bytes,Self::Historical{capture,..}=>capture.bytes,Self::NoBytes{..}=>0}}
}
fn copy_source(c:&Connection,id:&str)->anyhow::Result<Option<CopySource>> {
    let raw:Option<String>=c.query_row("SELECT descriptor FROM ci_log_accepted_copy WHERE operation=?1",[id],|r|r.get(0)).optional()?;
    raw.map(|raw|{anyhow::ensure!(raw.len()<=2048,"copy descriptor exceeds its bound");Ok(serde_json::from_str(&raw)?)}).transpose()
}

fn reserve(c:&Connection,o:&Original,max_bytes:u64)->anyhow::Result<Reserved> {
    if let Some(op)=operation(c,&o.id)? {
        if let Some(order)=o.job_order {
            let same:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_views v JOIN jobs j ON j.id=v.job WHERE v.view=?1 AND j.rowid=?2 AND v.job_order=?2 AND v.retired=0 AND j.attempt=?3)",params![op.view,order,o.copy.as_ref().map(|copy|copy.attempt)],|r|r.get(0))?;
            if !same {return Ok(Reserved::Refused("original target row/view/attempt is no longer current".into()))}
        }
        let same=op.job==o.job && op.step==o.step && op.origin==o.origin;
        let same=if let Some(copy)=&o.copy {
            same && c.query_row("SELECT request_hash FROM ci_log_accepted_copy WHERE operation=?1",[&o.id],|r|r.get::<_,String>(0)).optional()?.as_deref()==Some(copy.request_hash.as_str())
        }else{same && op.input_hash==o.hash && op.input_bytes==o.bytes};
        if !same {return Ok(Reserved::Refused("original accepted operation identity conflicts".into()))}
        return Ok(Reserved::Ready(Box::new(op)));
    }
    let tx=crate::store::tx(c)?;
    let job:Option<i64>=tx.query_row("SELECT j.rowid FROM jobs j JOIN runs r ON r.id=j.run_id WHERE j.id=?1 AND r.provider='kiln'",[&o.job],|r|r.get(0)).optional()?;
    let Some(job_order)=job else {return Ok(Reserved::Refused("the observed job is absent or not a kiln job".into()))};
    if o.job_order.is_some_and(|order|order!=job_order) {return Ok(Reserved::Refused("original target incarnation was replaced".into()))}
    if let Some(copy)=&o.copy {
        let same:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1 AND rowid=?2 AND attempt=?3)",params![o.job,job_order,copy.attempt],|r|r.get(0))?;
        if !same {return Ok(Reserved::Refused("original target attempt changed".into()))}
    }
    // Source lookup and lease insertion share this transaction. Retirement sets retired=1
    // before selecting any key; a source already selected by GC cannot acquire a new lease.
    let mut source_view=None;
    let copied=if let Some(copy)=&o.copy {
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs j JOIN runs r ON r.id=j.run_id WHERE j.id=?1 AND r.provider='kiln')",[&copy.source],|r|r.get(0))?;
        if !exists {return Ok(Reserved::Refused("copy source job was removed or is not canonical kiln content".into()))}
        let view:Option<(String,bool,bool)>=tx.query_row("SELECT view,retired,fenced FROM ci_log_accepted_views WHERE job=?1",[&copy.source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((view,retired,fenced))=view {
            if retired || fenced {return Ok(Reserved::Refused("copy source view is retired or fenced".into()))}
            source_view=Some(view);
        }
        let source=if let Some(prefix)=prefix(&tx,&copy.source)? {
            let tail:Option<(i64,u64,String)>=tx.query_row("SELECT step_ordinal,step_bytes,step_root FROM ci_log_accepted_chunks WHERE view=?1 AND step=?2 AND revision<=?3 ORDER BY revision DESC LIMIT 1",params![prefix.view,copy.step,prefix.revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((ordinal,bytes,root))=tail {CopySource::Accepted{job:copy.source.clone(),step:copy.step,prefix,ordinal,bytes,root}}else{CopySource::NoBytes{job:copy.source.clone(),step:copy.step,qualification:"accepted_catalog_has_no_captured_step_bytes_no_delivery_claim".into()}}
        }else{
            if let Some(capture)=copy.legacy.clone() {CopySource::Historical{job:copy.source.clone(),step:copy.step,host:o.host.clone(),capture,directory:copy.directory.clone()}}else{
                let historical:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_docs WHERE leg=?1 AND step=?2) OR EXISTS(SELECT 1 FROM ci_log_indexed WHERE leg=?1 AND (bytes IS NULL OR bytes>0))",params![copy.source,copy.step],|r|r.get(0))?;
                if historical {return Ok(Reserved::Refused("historically positive source is missing; not captured empty".into()))}
                CopySource::NoBytes{job:copy.source.clone(),step:copy.step,qualification:"unqualified_file_capture_has_no_bytes_no_delivery_claim".into()}
            }
        };
        Some(source)
    }else{None};
    let input_bytes=copied.as_ref().map_or(o.bytes,CopySource::bytes);
    tx.execute("INSERT INTO ci_log_accepted_views(job,view,job_order,root,qualification) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(job) DO NOTHING",params![o.job,o.view,job_order,EMPTY,QUALIFICATION])?;
    let (view,order,revision,root,bytes,chunks,operations,retired,fenced):(String,i64,i64,String,u64,i64,i64,bool,bool)=tx.query_row("SELECT view,job_order,revision,root,bytes,chunks,operations,retired,fenced FROM ci_log_accepted_views WHERE job=?1",[&o.job],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?)))?;
    if retired || order!=job_order {return Ok(Reserved::Refused("the original job-view owner was retired or replaced".into()))}
    if fenced {
        let repairing:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_unpublished WHERE view=?1 AND revision=?2 AND root=?3)",params![view,revision,root],|r|r.get(0))?;
        return Ok(if repairing {Reserved::Busy}else{Reserved::Refused("original job-view authority has a nonrecoverable fence".into())});
    }
    let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_ops WHERE view=?1 AND status='pending')",[&view],|r|r.get(0))?;
    if busy {return Ok(Reserved::Busy)}
    if (input_bytes==0 && copied.is_none()) || bytes.checked_add(input_bytes).is_none_or(|n|n>max_bytes) {
        return Ok(Reserved::Refused("the accepted job-view prefix has reached its existing byte bound".into()));
    }
    anyhow::ensure!(operations.checked_add(1).is_some() && chunks.checked_add(1).is_some(),"accepted-prefix SQL counter exhausted");
    let baseline_state=if revision==0 && input_bytes>0 {"discovering"}else{"ready"};
    let input_key=format!("ci-logs/{view}/{}/input",o.id);
    tx.execute("INSERT INTO ci_log_accepted_ops(operation,job,view,expected_revision,expected_root,staged_revision,staged_root,staged_bytes,staged_chunks,step,input_key,input_hash,input_bytes,origin,baseline,baseline_host,baseline_count,baseline_bytes,baseline_state,input_ready) VALUES(?1,?2,?3,?4,?5,?4,?5,?6,?7,?8,?9,?10,?11,?12,'',?13,0,0,?14,?15)",params![o.id,o.job,view,revision,root,bytes,chunks,o.step,input_key,o.hash,input_bytes,o.origin,o.host,baseline_state,copied.is_some()])?;
    tx.execute("UPDATE ci_log_accepted_views SET operations=operations+1 WHERE view=?1",[&view])?;
    if let Some(source)=copied {
        let descriptor=serde_json::to_string(&source)?;anyhow::ensure!(descriptor.len()<=2048,"copy descriptor exceeds its bound");
        let request=o.copy.as_ref().ok_or_else(||anyhow::anyhow!("copy request vanished"))?;
        tx.execute("INSERT INTO ci_log_accepted_copy(operation,request_hash,source_job,source_view,descriptor,target_attempt,effect,result_hash,result_bytes,output_scope,output_digest,output_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![o.id,request.request_hash,request.source,source_view,descriptor,request.attempt,request.effect,request.result_hash,request.result_bytes,request.output_tree.as_ref().map(|tree|tree.scope.as_str()),request.output_tree.as_ref().map(|tree|tree.digest.as_str()),request.output_tree.as_ref().map(|tree|tree.bytes)])?;
    }else{
        tx.execute("INSERT INTO ci_log_accepted_pins(key,view,operation,hash,bytes) VALUES(?1,?2,?3,?4,?5)",params![input_key,view,o.id,o.hash,input_bytes])?;
    }
    let op=operation(&tx,&o.id)?.ok_or_else(||anyhow::anyhow!("new append reservation is absent"))?;
    tx.commit()?;Ok(Reserved::Ready(Box::new(op)))
}

fn hash(bytes:&[u8])->String {hex::encode(Sha256::digest(bytes))}

fn chain(previous:&str,p:&Chunk)->String {
    // Length-delimited strings and fixed little-endian integers: no ambiguous concatenation.
    let mut h=Sha256::new();h.update(b"kiln-u05-observed-prefix-v1\0");
    for text in [previous,p.view.as_str(),p.key.as_str(),p.hash.as_str()] {h.update(u64::try_from(text.len()).unwrap_or(u64::MAX).to_le_bytes());h.update(text.as_bytes());}
    h.update(p.revision.to_le_bytes());h.update(p.step.to_le_bytes());h.update(p.ordinal.to_le_bytes());h.update(p.bytes.to_le_bytes());hex::encode(h.finalize())
}

#[derive(Clone,Debug)]
struct Chunk {
    view:String,revision:i64,operation:String,step:u32,ordinal:i64,step_bytes:u64,key:String,hash:String,bytes:u64,
    previous_root:String,root:String,previous_step_root:String,step_root:String,historical:bool,
}

fn previous(c:&Connection,op:&Operation,step:u32)->anyhow::Result<(i64,u64,String)> {
    Ok(c.query_row("SELECT step_ordinal,step_bytes,step_root FROM ci_log_accepted_chunks WHERE view=?1 AND step=?2 AND revision<=?3 ORDER BY revision DESC LIMIT 1",params![op.view,step,op.staged_revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.unwrap_or((0,0,EMPTY.into())))
}

fn prepare(op:&Operation,step:u32,key:String,bytes:&[u8],old:(i64,u64,String),historical:bool)->anyhow::Result<Chunk> {
    anyhow::ensure!(bytes.len()<=CHUNK && op.staged_chunks.checked_add(1).is_some(),"accepted chunk bound reached");
    let size=u64::try_from(bytes.len())?;
    let revision=op.staged_revision.checked_add(1).ok_or_else(||anyhow::anyhow!("accepted revision exhausted"))?;
    let ordinal=old.0.checked_add(1).ok_or_else(||anyhow::anyhow!("step ordinal exhausted"))?;
    let step_bytes=old.1.checked_add(size).ok_or_else(||anyhow::anyhow!("accepted step byte count overflow"))?;
    let digest=hash(bytes);
    let mut chunk=Chunk {view:op.view.clone(),revision,operation:op.id.clone(),step,ordinal,step_bytes,key,hash:digest,bytes:size,
        root:String::new(),step_root:String::new(),previous_root:op.staged_root.clone(),previous_step_root:old.2,historical};
    chunk.root=chain(&chunk.previous_root,&chunk);chunk.step_root=chain(&chunk.previous_step_root,&chunk);Ok(chunk)
}

fn pin(c:&Connection,p:&Chunk)->anyhow::Result<()> {
    let op=operation(c,&p.operation)?.ok_or_else(||anyhow::anyhow!("accepted operation disappeared"))?;
    anyhow::ensure!(op.status=="pending" && op.view==p.view && op.staged_revision.checked_add(1)==Some(p.revision) && op.staged_root==p.previous_root,"stale immutable-chunk reservation refused");
    c.execute("INSERT INTO ci_log_accepted_pins(key,view,operation,hash,bytes) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(key) DO NOTHING",params![p.key,p.view,p.operation,p.hash,p.bytes])?;
    let got:(String,String,String,u64)=c.query_row("SELECT view,operation,hash,bytes FROM ci_log_accepted_pins WHERE key=?1",[&p.key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    anyhow::ensure!(got==(p.view.clone(),p.operation.clone(),p.hash.clone(),p.bytes),"immutable-chunk pin conflicts");
    Ok(())
}

fn stage(c:&Connection,p:&Chunk,file:u64,offset:u64,input:bool,input_cursor:u64)->anyhow::Result<()> {
    let tx=crate::store::tx(c)?;
    let saved:Option<(String,String)>=tx.query_row("SELECT operation,root FROM ci_log_accepted_chunks WHERE view=?1 AND revision=?2",params![p.view,p.revision],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some(saved)=saved {anyhow::ensure!(saved==(p.operation.clone(),p.root.clone()),"original staged-chunk receipt conflicts");return Ok(())}
    let op=operation(&tx,&p.operation)?.ok_or_else(||anyhow::anyhow!("accepted operation disappeared"))?;
    anyhow::ensure!(op.status=="pending" && op.input_ready && op.view==p.view && op.staged_revision.checked_add(1)==Some(p.revision) && op.staged_root==p.previous_root,"stale accepted chunk preparation refused");
    let old=previous(&tx,&op,p.step)?;
    anyhow::ensure!(old.0.checked_add(1)==Some(p.ordinal) && old.1.checked_add(p.bytes)==Some(p.step_bytes) && old.2==p.previous_step_root,"accepted step prefix changed");
    let owned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_pins WHERE key=?1 AND view=?2 AND operation=?3 AND hash=?4 AND bytes=?5 AND ready=1)",params![p.key,p.view,p.operation,p.hash,p.bytes],|r|r.get(0))?;
    anyhow::ensure!(owned,"accepted chunk has no original durable pin");
    tx.execute("INSERT INTO ci_log_accepted_chunks(view,revision,operation,step,step_ordinal,step_bytes,key,hash,bytes,previous_root,root,previous_step_root,step_root,historical) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",params![p.view,p.revision,p.operation,p.step,p.ordinal,p.step_bytes,p.key,p.hash,p.bytes,p.previous_root,p.root,p.previous_step_root,p.step_root,p.historical])?;
    tx.execute("INSERT INTO ci_log_accepted_steps(view,step,first_revision) VALUES(?1,?2,?3) ON CONFLICT(view,step) DO NOTHING",params![p.view,p.step,p.revision])?;
    let total=op.staged_bytes.checked_add(p.bytes).ok_or_else(||anyhow::anyhow!("accepted byte count overflow"))?;
    tx.execute("UPDATE ci_log_accepted_ops SET staged_revision=?2,staged_root=?3,staged_bytes=?4,staged_chunks=staged_chunks+1,baseline_file=?5,baseline_offset=?6,input_staged=?7,input_cursor=?8 WHERE operation=?1",params![p.operation,p.revision,p.root,total,i64::try_from(file)?,offset,input,input_cursor])?;
    tx.commit()?;Ok(())
}

pub(crate) fn publish(c:&Connection,id:&str)->anyhow::Result<Receipt> {
    let tx=crate::store::tx(c)?;
    let op=operation(&tx,id)?.ok_or_else(||anyhow::anyhow!("accepted operation disappeared"))?;
    if op.status!="pending" {return receipt(&op)}
    let copied:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_copy WHERE operation=?1)",[id],|r|r.get(0))?;
    if copied && !copy_current(&tx,id)? {
        let refused=refusal_in(&tx,&op,"original copy target row/view/attempt ceased to be current before publication")?;tx.commit()?;return Ok(refused);
    }
    anyhow::ensure!(op.input_ready && op.input_staged && op.baseline_state=="ready","accepted operation still has uncaptured input or history");
    let incomplete:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_copy WHERE operation=?1 AND (result_ready=0 OR objects_complete=0))",[id],|r|r.get(0))?;
    anyhow::ensure!(!incomplete,"copy cannot publish before original result/output pin receipt");
    let changed=tx.execute("UPDATE ci_log_accepted_views SET revision=?2,root=?3,bytes=?4,chunks=?5 WHERE view=?1 AND revision=?6 AND root=?7 AND retired=0 AND fenced=0 AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_unpublished u WHERE u.view=ci_log_accepted_views.view) AND EXISTS(SELECT 1 FROM jobs j JOIN runs r ON r.id=j.run_id WHERE j.id=ci_log_accepted_views.job AND j.rowid=ci_log_accepted_views.job_order AND r.provider='kiln')",params![op.view,op.staged_revision,op.staged_root,op.staged_bytes,op.staged_chunks,op.expected_revision,op.expected_root])?;
    if changed!=1 {
        let done=refusal_in(&tx,&op,"the original accepted view was retired, replaced or advanced")?;
        tx.commit()?;return Ok(done);
    }
    tx.execute("UPDATE ci_log_accepted_ops SET status='accepted',receipt_revision=?2,receipt_root=?3 WHERE operation=?1",params![id,op.staged_revision,op.staged_root])?;
    // This is O(1) dirty intent. Existing indexer pages retire old postings later. Search
    // fences on this new revision immediately, before any old preparation can publish.
    crate::ci::logsearch::stage_finished(&tx,&op.job)?;
    let done=operation(&tx,id)?.ok_or_else(||anyhow::anyhow!("accepted receipt disappeared"))?;
    tx.commit()?;receipt(&done)
}

fn refuse(c:&Connection,id:&str,why:&str)->anyhow::Result<Receipt> {
    let tx=crate::store::tx(c)?;
    let op=operation(&tx,id)?.ok_or_else(||anyhow::anyhow!("original append reservation is absent"))?;
    if op.status!="pending" {return receipt(&op)}
    let done=refusal_in(&tx,&op,why)?;
    tx.commit()?;Ok(done)
}

fn refusal_in(c:&Connection,op:&Operation,why:&str)->anyhow::Result<Receipt> {
    c.execute("UPDATE ci_log_accepted_copy SET active=0 WHERE operation=?1",[&op.id])?;
    // Pins remain owned even on refusal; later bounded retirement is the only releaser.
    c.execute("UPDATE ci_log_accepted_ops SET status='refused',refusal=?2,receipt_revision=?3,receipt_root=?4 WHERE operation=?1",params![op.id,why,op.expected_revision,op.expected_root])?;
    if op.staged_revision>op.expected_revision {
        c.execute("UPDATE ci_log_accepted_views SET fenced=1 WHERE view=?1",[&op.view])?;
        c.execute("INSERT INTO ci_log_accepted_unpublished(view,operation,revision,root,chunk_after,catalog_after) VALUES(?1,?2,?3,?4,?3,?3) ON CONFLICT(view) DO NOTHING",params![op.view,op.id,op.expected_revision,op.expected_root])?;
        let exact:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_unpublished WHERE view=?1 AND operation=?2 AND revision=?3 AND root=?4)",params![op.view,op.id,op.expected_revision,op.expected_root],|r|r.get(0))?;
        anyhow::ensure!(exact,"original unpublished suffix repair identity conflicts");
    }
    let done=operation(c,&op.id)?.ok_or_else(||anyhow::anyhow!("refusal receipt disappeared"))?;
    receipt(&done)
}

/// Only exact original conditional transitions may replay. Existing Database admission,
/// deadline/Unknown and compaction-reset fence semantics remain unchanged. Records/session
/// callers still share Database directly: no blanket product-wide Unknown fence is claimed.
/// This layer never resets a fence or treats a barrier as its own receipt. Each caller supplies an idempotent original descriptor plus a read
/// of its durable marker/result. No retry creates a new append id or changes expected root.
async fn transition<T,W,R>(store:&Store,write:W,read:R)->T
where T:Send+'static,W:Fn(&Connection)->anyhow::Result<()>+Clone+Send+'static,R:Fn(&Connection)->anyhow::Result<Option<T>>+Clone+Send+'static {
    loop {
        let write=write.clone();
        if let Err(error)=store.call(move |c|write(c)).await {
            tracing::warn!(error=%format!("{error:#}"),"original conditional log transition remains unadmitted/unknown; own receipt required");
        }
        let read=read.clone();
        if let Ok(Some(receipt))=store.call(move |c|read(c)).await {return receipt}
        crate::rt::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Read-only reconciliation never promotes a pending operation merely because a later
/// database callback ran. Absent/unavailable original bytes or markers remain incomplete.
async fn reconcile<T,F>(store:&Store,read:F)->T
where T:Send+'static,F:Fn(&Connection)->anyhow::Result<Option<T>>+Clone+Send+'static {
    loop {
        // A fresh READ of the same immutable descriptor is safe; a read failure proves
        // neither presence nor absence. Only transition() replays checked conditional work.
        let reader=read.clone();
        match store.call(move |c|reader(c)).await {
            Ok(Some(value))=>return value,
            Ok(None)=>{},
            Err(error)=>tracing::warn!(error=%format!("{error:#}"),"accepted-prefix reconciliation remains unknown; original operation retained"),
        }
        crate::rt::sleep(std::time::Duration::from_millis(250)).await;
    }
}

async fn load(store:&Store,id:String)->Operation {
    reconcile(store,move |c|operation(c,&id)).await
}

async fn read_key(blobs:&Arc<dyn Blobs>,key:&str,digest:&str,size:u64)->Result<bool,String> {
    match blobs.get(key,None).await {
        Ok((meta,bytes))=>{
            let key=key.to_owned();let digest=digest.to_owned();
            match crate::rt::blocking(move ||meta.key==key && meta.etag==digest && meta.size==size && u64::try_from(bytes.len()).ok()==Some(size) && hash(&bytes)==digest).await {
                Ok(true)=>Ok(true),Ok(false)=>Err("immutable key conflicts with its original operation".into()),Err(_)=>Ok(false),
            }
        }
        Err(_)=>Ok(false), // absence/unavailability is neither acceptance nor refusal.
    }
}

async fn owned_put(ctx:&Context,key:&str,digest:&str,bytes:Vec<u8>)->Result<(),String> {
    let size=u64::try_from(bytes.len()).map_err(|e|e.to_string())?;
    let original=key.to_owned();let expected=digest.to_owned();
    let state=reconcile(&ctx.store,move |c|Ok(c.query_row("SELECT put_started,ready,ambiguous FROM ci_log_accepted_pins WHERE key=?1 AND hash=?2 AND bytes=?3",params![original,expected,size],|r|Ok((r.get::<_,bool>(0)?,r.get::<_,bool>(1)?,r.get::<_,bool>(2)?))).optional()?)).await;
    let mut ambiguous=state.2;
    let original=key.to_owned();let check=original.clone();
    transition(&ctx.store,move |c|{c.execute("UPDATE ci_log_accepted_pins SET put_started=1,ambiguous=1 WHERE key=?1",[&original])?;Ok(())},move |c|Ok(c.query_row("SELECT (put_started=1 AND ambiguous=1) FROM ci_log_accepted_pins WHERE key=?1",[&check],|r|r.get::<_,bool>(0)).optional()?.filter(|started|*started))).await;
    // Retain ORIGINAL raw bytes through every unknown result. Only the same immutable
    // IfNoneMatch key+digest+bytes may replay; no new append or unconditional PUT is made.
    if !state.1 || !read_key(&ctx.blobs,key,digest,size).await? {
        loop {
            match ctx.blobs.put(key,bytes.clone(),Put{cond:Cond::IfNoneMatch,sha256:Some(digest.into())}).await {
                Ok(meta) if meta.key==key && meta.etag==digest && meta.size==size=>break,
                Ok(_)=>return Err("immutable put returned a conflicting own receipt".into()),
                Err(Refusal::PreconditionFailed(_))=>{},
                Err(Refusal::Unavailable(_))=>ambiguous=true,
                Err(error) if !ambiguous=>{
                    let key=key.to_owned();let check=key.clone();
                    transition(&ctx.store,move |c|{c.execute("UPDATE ci_log_accepted_pins SET ambiguous=0 WHERE key=?1",[&key])?;Ok(())},move |c|Ok(c.query_row("SELECT ambiguous FROM ci_log_accepted_pins WHERE key=?1",[&check],|r|r.get::<_,bool>(0)).optional()?.filter(|ambiguous|!*ambiguous))).await;
                    return Err(error.to_string());
                },
                Err(error)=>tracing::warn!(key,error=%error,"earlier immutable write remains unknown; original raw bytes/pins retained"),
            }
            if read_key(&ctx.blobs,key,digest,size).await? {break}
            crate::rt::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    let original=key.to_owned();let expected=digest.to_owned();let check=original.clone();let digest=expected.clone();
    transition(&ctx.store,move |c|{c.execute("UPDATE ci_log_accepted_pins SET ready=1,ambiguous=?4 WHERE key=?1 AND hash=?2 AND bytes=?3 AND put_started=1",params![original,expected,size,ambiguous])?;Ok(())},move |c|Ok(c.query_row("SELECT ready FROM ci_log_accepted_pins WHERE key=?1 AND hash=?2 AND bytes=?3",params![check,digest,size],|r|r.get::<_,bool>(0)).optional()?.filter(|ready|*ready))).await;
    drop(bytes);Ok(())
}

#[derive(Clone)]
pub(super) struct Context {
    pub store:Store,
    pub blobs:Arc<dyn Blobs>,
    pub files:super::Projection,
    pub host:String,
}

async fn refused(ctx:&Context,id:String,why:String)->Receipt {
    let why=crate::util::truncate(&why,4096).to_owned();let check=id.clone();
    transition(&ctx.store,move |c|{refuse(c,&id,&why)?;Ok(())},move |c|completed(c,&check)).await
}

async fn input_ready(ctx:&Context,op:&Operation) {
    let id=op.id.clone();let check=id.clone();
    transition(&ctx.store,move |c|{c.execute("UPDATE ci_log_accepted_ops SET input_ready=1 WHERE operation=?1 AND status='pending'",[&id])?;Ok(())},move |c|Ok(operation(c,&check)?.filter(|op|op.input_ready || op.status!="pending"))).await;
}

pub(super) async fn append(ctx:Context,job:String,step:u32,input:Vec<u8>,origin:String)->anyhow::Result<Receipt> {
    anyhow::ensure!(!input.is_empty() && input.len()<=CHUNK && origin.len()<=256 && ctx.host.len()<=256,"observed append exceeds its admission bound");
    let (input,digest)=crate::rt::blocking(move ||{let digest=hash(&input);(input,digest)}).await?;
    let original=Original{id:crate::util::new_id(),view:crate::util::new_id(),job,step,hash:digest,bytes:u64::try_from(input.len())?,origin,host:ctx.host.clone(),copy:None,job_order:None};
    let mut op=loop {
        let o=original.clone();let max=ctx.files.max_bytes();
        match ctx.store.call(move |c|reserve(c,&o,max)).await {
            Ok(Reserved::Ready(op))=>break *op,
            Ok(Reserved::Busy)=>crate::rt::sleep(std::time::Duration::from_millis(250)).await,
            Ok(Reserved::Refused(why))=>anyhow::bail!("observed append not admitted: {why}; no acceptance was published"),
            Err(error)=>{
                tracing::warn!(operation=original.id,error=%format!("{error:#}"),"append reservation reply is unknown; the original operation is retained");
                crate::rt::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    };
    if op.status!="pending" {return receipt(&op)}
    match owned_put(&ctx,&op.input_key,&op.input_hash,input).await {
        Ok(())=>input_ready(&ctx,&op).await,
        Err(why)=>return Ok(refused(&ctx,op.id,why).await),
    }
    op=load(&ctx.store,op.id).await;
    retained_operation(ctx,op).await
}

/// A target logical effect never includes a mutable source/result in its identity.
fn effect_id(c:&Connection,job:&str,step:u32,attempt:u32,effect:&str)->anyhow::Result<(String,i64)> {
    anyhow::ensure!(effect.len()==64 && effect.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),"memo effect is not a canonical SHA256");
    let row:i64=c.query_row("SELECT j.rowid FROM jobs j JOIN runs r ON r.id=j.run_id WHERE j.id=?1 AND j.attempt=?2 AND r.provider='kiln'",params![job,attempt],|r|r.get(0))?;
    let key=hash(serde_json::to_string(&("kiln-u05-copy-effect-v1",job,row,step,attempt,effect))?.as_bytes());
    Ok((format!("copy-{key}"),row))
}

fn copy_current(c:&Connection,id:&str)->anyhow::Result<bool> {
    let current:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_copy x JOIN ci_log_accepted_ops o ON o.operation=x.operation JOIN ci_log_accepted_views v ON v.view=o.view JOIN jobs j ON j.id=o.job JOIN runs r ON r.id=j.run_id WHERE x.operation=?1 AND o.status IN ('pending','accepted','no_bytes') AND v.retired=0 AND v.fenced=0 AND v.job_order=j.rowid AND j.attempt=x.target_attempt AND r.provider='kiln')",[id],|r|r.get(0))?;
    Ok(current)
}
pub(crate) fn require_copy_object_pin(c:&Connection,id:&str)->anyhow::Result<()> {
    anyhow::ensure!(copy_current(c,id)?,"original copy target is absent, replaced, refused, fenced or retired");Ok(())
}

pub(crate) fn copy_metadata_operation(c:&Connection,job:&str,step:u32,target_attempt:u32,effect:&str)->anyhow::Result<Option<String>> {
    let (id,_)=effect_id(c,job,step,target_attempt,effect)?;require_copy_object_pin(c,&id)?;
    let (ready,applied):(bool,bool)=c.query_row("SELECT (x.result_ready=1 AND x.objects_complete=1 AND o.status IN ('accepted','no_bytes')),x.metadata_applied FROM ci_log_accepted_copy x JOIN ci_log_accepted_ops o ON o.operation=x.operation WHERE x.operation=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    anyhow::ensure!(ready,"original copy has no own committed result/output receipt");Ok((!applied).then_some(id))
}

pub(crate) fn mark_copy_metadata(c:&Connection,id:&str)->anyhow::Result<()> {
    require_copy_object_pin(c,id)?;
    let changed=c.execute("UPDATE ci_log_accepted_copy SET metadata_applied=1,active=0 WHERE operation=?1 AND result_ready=1 AND objects_complete=1 AND EXISTS(SELECT 1 FROM ci_log_accepted_ops o WHERE o.operation=?1 AND o.status IN ('accepted','no_bytes'))",[id])?;
    anyhow::ensure!(changed==1,"original copy metadata cannot commit before its own result/output receipt");Ok(())
}

pub(crate) fn copy_has_object(c:&Connection,job:&str,attempt:u32,scope:&str,digest:&str)->anyhow::Result<bool> {
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_copy_objects p JOIN ci_log_accepted_copy x ON x.operation=p.operation JOIN ci_log_accepted_ops o ON o.operation=x.operation JOIN ci_log_accepted_views v ON v.view=o.view JOIN jobs j ON j.id=o.job JOIN runs r ON r.id=j.run_id JOIN cas_index i ON i.digest=p.digest AND i.size=p.size WHERE o.job=?1 AND j.attempt=?2 AND x.target_attempt=?2 AND x.output_scope=?3 AND p.digest=?4 AND v.job_order=j.rowid AND v.retired=0 AND v.fenced=0 AND o.status IN ('accepted','no_bytes') AND x.result_ready=1 AND x.objects_complete=1 AND r.provider='kiln')",params![job,attempt,scope,digest],|r|r.get(0))?)
}

#[derive(Clone)]
struct CopyState {result_hash:String,result_bytes:u64,result_ready:bool,output:Option<OutputTree>,cursor:u64,objects_complete:bool}
fn copy_state(c:&Connection,id:&str)->anyhow::Result<CopyState> {
    let row=c.query_row("SELECT descriptor,result_hash,result_bytes,result_ready,output_scope,output_digest,output_bytes,object_cursor,objects_complete FROM ci_log_accepted_copy WHERE operation=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?,r.get::<_,bool>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,Option<u64>>(6)?,r.get::<_,u64>(7)?,r.get::<_,bool>(8)?)))?;
    anyhow::ensure!(row.0.len()<=2048 && row.2<=u64::try_from(RESULT_MAX)?,"original copy metadata exceeds its bound");
    let output=match (row.4,row.5,row.6) {(Some(scope),Some(digest),Some(bytes))=>Some(OutputTree{scope,digest,bytes}),(None,None,None)=>None,_=>anyhow::bail!("original output descriptor is partial")};
    let _:CopySource=serde_json::from_str(&row.0)?;
    Ok(CopyState{result_hash:row.1,result_bytes:row.2,result_ready:row.3,output,cursor:row.7,objects_complete:row.8})
}

async fn stage_result(ctx:&Context,id:&str,bytes:Vec<u8>)->anyhow::Result<()> {
    anyhow::ensure!(bytes.len()<=RESULT_MAX,"original SavedHit exceeds16MiB");
    let key=id.to_owned();let state=ctx.store.call(move |c|copy_state(c,&key)).await?;
    if state.result_ready {return Ok(())}
    let (bytes,digest)=crate::rt::blocking(move ||{let digest=hash(&bytes);(bytes,digest)}).await?;
    if digest!=state.result_hash || u64::try_from(bytes.len())?!=state.result_bytes {
        // A newer found() value is not original retry input. Keep original reservation and
        // reconcile only its durable pages; crash-lost original input is explicitly pending.
        tracing::warn!(operation=id,"newly selected memo result differs from original pending effect; original pages/custody retained");
        return Ok(());
    }
    for (page,body) in bytes.chunks(RESULT_PART*RESULT_PAGE).enumerate() {
        let first=page.checked_mul(RESULT_PAGE).ok_or_else(||anyhow::anyhow!("result page cursor exhausted"))?;
        let parts=body.chunks(RESULT_PART).enumerate().map(|(n,b)|(first+n,hash(b),b.to_vec())).collect::<Vec<_>>();
        let id=id.to_owned();let check=id.clone();let expected=parts.clone();
        transition(&ctx.store,move |c|{
            let tx=crate::store::tx(c)?;
            if !copy_current(&tx,&id)? {
                let op=operation(&tx,&id)?.ok_or_else(||anyhow::anyhow!("original copy disappeared"))?;
                if op.status=="pending"{refusal_in(&tx,&op,"original target changed before SavedHit staging completed")?;}
                tx.commit()?;return Ok(());
            }
            for (n,digest,body) in &parts {tx.execute("INSERT INTO ci_log_copy_result_parts(operation,part,hash,body) VALUES(?1,?2,?3,?4) ON CONFLICT(operation,part) DO NOTHING",params![id,u64::try_from(*n)?,digest,body])?;}
            tx.commit()?;Ok(())
        },move |c|{
            if operation(c,&check)?.is_some_and(|op|op.status=="refused"){return Ok(Some(()))}
            for (n,digest,body) in &expected {
                let got:Option<(String,u64)>=c.query_row("SELECT hash,length(body) FROM ci_log_copy_result_parts WHERE operation=?1 AND part=?2",params![check,u64::try_from(*n)?],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                if got!=Some((digest.clone(),u64::try_from(body.len())?)) {return Ok(None)}
            }Ok(Some(()))
        }).await;
    }
    // The descriptor hash and all pages are checked before one O(1) ready publication.
    result_ready(ctx,id).await?;Ok(())
}

async fn result_bytes(ctx:&Context,id:&str)->anyhow::Result<Option<Vec<u8>>> {
    let key=id.to_owned();let state=ctx.store.call(move |c|copy_state(c,&key)).await?;
    let count=usize::try_from(state.result_bytes)?.div_ceil(RESULT_PART);
    let mut bytes=Vec::with_capacity(usize::try_from(state.result_bytes)?);
    for part in 0..count {
        let key=id.to_owned();let row=ctx.store.call(move |c|Ok(c.query_row("SELECT hash,body FROM ci_log_copy_result_parts WHERE operation=?1 AND part=?2",params![key,u64::try_from(part)?],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?))).optional()?)).await?;
        let Some((digest,body))=row else {return Ok(None)};
        anyhow::ensure!(body.len()<=RESULT_PART && hash(&body)==digest && bytes.len().checked_add(body.len()).is_some_and(|n|n<=RESULT_MAX),"original SavedHit page is corrupt/oversized");bytes.extend(body);
    }
    let valid=crate::rt::blocking(move ||{anyhow::ensure!(u64::try_from(bytes.len())?==state.result_bytes && hash(&bytes)==state.result_hash,"original SavedHit root differs from first reserved result");Ok::<_,anyhow::Error>(bytes)}).await??;Ok(Some(valid))
}

async fn result_ready(ctx:&Context,id:&str)->anyhow::Result<()> {
    let key=id.to_owned();let ready=ctx.store.call(move|c|Ok(copy_state(c,&key)?.result_ready)).await?;
    if ready{return Ok(())}
    if result_bytes(ctx,id).await?.is_none(){return Ok(())}
    let id=id.to_owned();let check=id.clone();
    transition(&ctx.store,move|c|{
        let tx=crate::store::tx(c)?;
        if copy_current(&tx,&id)? {tx.execute("UPDATE ci_log_accepted_copy SET result_ready=1 WHERE operation=?1",[&id])?;}else{
            let op=operation(&tx,&id)?.ok_or_else(||anyhow::anyhow!("original copy disappeared"))?;
            if op.status=="pending"{refusal_in(&tx,&op,"original target changed before SavedHit ready publication")?;}
        }
        tx.commit()?;Ok(())
    },move|c|{
        if operation(c,&check)?.is_some_and(|op|op.status=="refused"){return Ok(Some(()))}
        Ok(c.query_row("SELECT result_ready FROM ci_log_accepted_copy WHERE operation=?1",[&check],|r|r.get::<_,bool>(0)).optional()?.filter(|ready|*ready).map(|_|()))
    }).await;Ok(())
}

async fn require_result(ctx:&Context,id:&str)->anyhow::Result<Vec<u8>> {
    loop {
        let key=id.to_owned();let own=ctx.store.call(move|c|Ok((operation(c,&key)?,copy_current(c,&key)?))).await?;
        let op=own.0.ok_or_else(||anyhow::anyhow!("original copy result disappeared"))?;
        anyhow::ensure!(op.status!="refused","original copy has own refusal: {:?}",op.refusal);
        if !own.1 {let done=refused(ctx,id.to_owned(),"original copy target row/view/attempt ceased to be current".into()).await;anyhow::bail!("original copy result unavailable: {:?}",done.refusal);}
        result_ready(ctx,id).await?;
        let key=id.to_owned();let state=ctx.store.call(move |c|copy_state(c,&key)).await?;
        if state.result_ready && let Some(bytes)=result_bytes(ctx,id).await? {return Ok(bytes)}
        tracing::warn!(operation=id,"original SavedHit input not fully durable; effect stays incomplete and cannot reselect latest result");
        crate::rt::sleep(std::time::Duration::from_millis(250)).await;
    }
}

async fn pin_page(ctx:&Context,id:&str,page:&[kiln_ci::work::memo::Digest])->anyhow::Result<()> {
    loop {
        match crate::ci::services::cas::pin_copy_objects(&ctx.store,id,page).await {
            Ok(())=>return Ok(()),
            Err(error)=>tracing::warn!(operation=id,error=%format!("{error:#}"),"original output pin page remains unavailable/unknown; exact page and manifest retained"),
        }
        let operation=id.to_owned();let original=page.to_vec();
        let observed=ctx.store.call(move|c|{
            require_copy_object_pin(c,&operation)?;
            for object in &original {
                let own:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_copy_objects p JOIN cas_index i ON i.digest=p.digest AND i.size=p.size WHERE p.operation=?1 AND p.digest=?2 AND p.size=?3)",params![operation,object.hash,object.size_bytes],|r|r.get(0))?;
                if !own{return Ok(false)}
            }Ok(true)
        }).await;
        // A later healthy actor call reconciles EXACT durable original refs and sizes;
        // callback/barrier success by itself is never a page or copy receipt.
        if matches!(observed,Ok(true)){return Ok(())}
        let key=id.to_owned();
        if matches!(ctx.store.call(move|c|copy_current(c,&key)).await,Ok(false)){anyhow::bail!("original copy target ceased to be current during output pin staging");}
        crate::rt::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn copy_transition(c:&Connection,id:&str,write:impl FnOnce(&Connection)->anyhow::Result<()>)->anyhow::Result<()> {
    let tx=crate::store::tx(c)?;
    let op=operation(&tx,id)?.ok_or_else(||anyhow::anyhow!("original copy transition marker disappeared"))?;
    if op.status=="pending" {
        if copy_current(&tx,id)? {write(&tx)?;}else{
            refusal_in(&tx,&op,"original target row/view/attempt changed during copy transition")?;
        }
    }
    tx.commit()?;Ok(())
}
fn copy_progress(c:&Connection,id:&str,proof:impl FnOnce(&Connection)->anyhow::Result<bool>)->anyhow::Result<Option<Operation>> {
    let op=operation(c,id)?.ok_or_else(||anyhow::anyhow!("original copy progress marker disappeared"))?;
    Ok((op.status!="pending" || proof(c)?).then_some(op))
}
fn pending_copy(op:&Operation)->anyhow::Result<()> {
    anyhow::ensure!(op.status!="refused","original copy has own durable refusal: {:?}",op.refusal);Ok(())
}

async fn pin_outputs(ctx:&Context,id:&str)->anyhow::Result<()> {
    let key=id.to_owned();let state=ctx.store.call(move |c|copy_state(c,&key)).await?;
    if state.objects_complete {return Ok(())}
    if let Some(tree)=state.output {
        let root=kiln_ci::work::memo::Digest{hash:tree.digest,size_bytes:tree.bytes};
        pin_page(ctx,id,std::slice::from_ref(&root)).await?;
        // One bounded read/parse per healthy pass/restart, never one per32-reference page.
        let objects=outputs::manifest(Arc::clone(&ctx.blobs),root).await?;
        let mut cursor=usize::try_from(state.cursor)?;
        anyhow::ensure!(cursor<=objects.len(),"output manifest cursor exceeds original checked tree");
        while cursor<objects.len() {
            let end=cursor.saturating_add(crate::ci::services::cas::COPY_OBJECT_PAGE).min(objects.len());
            let page=objects.get(cursor..end).ok_or_else(||anyhow::anyhow!("output manifest page disappeared"))?;
            pin_page(ctx,id,page).await?;
            let id=id.to_owned();let check=id.clone();let advanced=u64::try_from(end)?;
            let observed=transition(&ctx.store,move |c|copy_transition(c,&id,|c|{c.execute("UPDATE ci_log_accepted_copy SET object_cursor=MAX(object_cursor,?2) WHERE operation=?1",params![id,advanced])?;Ok(())}),move |c|copy_progress(c,&check,|c|Ok(c.query_row("SELECT object_cursor>=?2 FROM ci_log_accepted_copy WHERE operation=?1",params![check,advanced],|r|r.get(0))?))).await;
            pending_copy(&observed)?;
            cursor=end;
        }
    }
    let id=id.to_owned();let check=id.clone();
    let observed=transition(&ctx.store,move |c|copy_transition(c,&id,|c|{c.execute("UPDATE ci_log_accepted_copy SET objects_complete=1 WHERE operation=?1",[&id])?;Ok(())}),move |c|copy_progress(c,&check,|c|Ok(c.query_row("SELECT objects_complete FROM ci_log_accepted_copy WHERE operation=?1",[&check],|r|r.get(0))?))).await;
    pending_copy(&observed)?;Ok(())
}

/// Initial source/result descriptors are bound to one stable target effect before streaming.
async fn copy_append(ctx:Context,request:CopyStep)->anyhow::Result<Vec<u8>> {
    let CopyStep{job,step,target_attempt,effect,source_job,source_step,result,output_tree}=request;
    anyhow::ensure!(result.len()<=RESULT_MAX && source_job.len()<=256 && job.len()<=256,"copy input exceeds its bound");
    if let Some(tree)=&output_tree {anyhow::ensure!(tree.scope.len()<=1024 && tree.bytes<=64<<20 && tree.digest.len()==64,"output tree descriptor exceeds existing bounds");}
    let j=job.clone();let e=effect.clone();
    let (id,order)=ctx.store.call(move |c|effect_id(c,&j,step,target_attempt,&e)).await?;
    let key=id.clone();let source=source_job.clone();
    let needs_file=ctx.store.call(move |c|Ok(operation(c,&key)?.is_none() && prefix(c,&source)?.is_none())).await?;
    let (legacy,directory)=if needs_file {ctx.files.copy_capture(&source_job,source_step).await?}else{(None,String::new())};
    let (result,digest)=crate::rt::blocking(move ||{let digest=hash(&result);(result,digest)}).await?;
    let original=Original{id:id.clone(),view:crate::util::new_id(),job,step,hash:digest.clone(),bytes:0,origin:"server-memo-copy".into(),host:ctx.host.clone(),job_order:Some(order),copy:Some(CopyRequest{source:source_job,step:source_step,request_hash:effect.clone(),legacy,directory,attempt:target_attempt,effect,result_hash:digest,result_bytes:u64::try_from(result.len())?,output_tree})};
    let op=loop {
        let o=original.clone();let max=ctx.files.max_bytes();
        match ctx.store.call(move |c|reserve(c,&o,max)).await {
            Ok(Reserved::Ready(op))=>break *op,
            Ok(Reserved::Refused(why))=>anyhow::bail!("server copy not admitted: {why}"),
            Ok(Reserved::Busy)=>crate::rt::sleep(std::time::Duration::from_millis(250)).await,
            Err(error)=>{tracing::warn!(operation=id,error=%format!("{error:#}"),"original copy reservation remains unknown");crate::rt::sleep(std::time::Duration::from_millis(250)).await;}
        }
    };
    if op.status=="pending" {stage_result(&ctx,&id,result).await?;}
    finish_copy(ctx,op).await
}

async fn finish_copy(ctx:Context,op:Operation)->anyhow::Result<Vec<u8>> {
    anyhow::ensure!(op.status!="refused","original copy has own refusal: {:?}",op.refusal);
    let id=op.id.clone();
    let bytes=require_result(&ctx,&id).await?;
    if op.status=="pending" {let _=retained_operation(ctx.clone(),op).await?;}
    let key=id.clone();let (valid,ready)=ctx.store.call(move |c|{
        require_copy_object_pin(c,&key)?;
        let own=operation(c,&key)?.ok_or_else(||anyhow::anyhow!("original copy disappeared"))?;
        let copy=copy_state(c,&key)?;
        Ok((matches!(own.status.as_str(),"accepted"|"no_bytes"),copy.result_ready && copy.objects_complete))
    }).await?;
    anyhow::ensure!(valid && ready,"original copy remains refused/incomplete; no result200");Ok(bytes)
}

/// Raw immutable copy, never Plain/search normalization. One source descriptor and one
/// CHUNK-sized body per turn. The active source lease makes retirement ineligible.
async fn copied_chunk(ctx:&Context,op:&Operation,source:&CopySource)->anyhow::Result<(Vec<u8>,u64,bool)> {
    match source {
        CopySource::NoBytes{..}=>anyhow::bail!("no-byte copy has no body; it must use its explicit no-byte effect receipt"),
        CopySource::Historical{job,step:_,host,capture,directory}=>{
            ctx.files.verify_directory(job,directory.clone()).await?;
            anyhow::ensure!(&ctx.host==host,"original unqualified file copy belongs to another host; no adoption");
            let bytes=ctx.files.captured_chunk(job,capture,op.input_cursor).await?;
            let next=op.input_cursor.checked_add(u64::try_from(bytes.len())?).ok_or_else(||anyhow::anyhow!("copy offset exhausted"))?;
            Ok((bytes,next,next==capture.bytes))
        }
        CopySource::Accepted{job:_,step,prefix,ordinal,bytes:total,root}=>{
            let view=prefix.view.clone();let revision=prefix.revision;let expected=prefix.root.clone();let step=*step;let after=i64::try_from(op.input_cursor)?;
            let (chunk,previous)=ctx.store.call(move |c|{
                let observed:String=c.query_row("SELECT root FROM ci_log_accepted_chunks WHERE view=?1 AND revision=?2",params![view,revision],|r|r.get(0))?;
                anyhow::ensure!(observed==expected,"source copy prefix root changed");
                let previous=if after==0 {(0,0,EMPTY.to_owned())}else{c.query_row("SELECT step_ordinal,step_bytes,step_root FROM ci_log_accepted_chunks WHERE view=?1 AND step=?2 AND step_ordinal=?3 AND revision<=?4",params![view,step,after,revision],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?)))?};
                let next=after.checked_add(1).ok_or_else(||anyhow::anyhow!("copy ordinal exhausted"))?;
                let chunk=c.query_row("SELECT view,revision,operation,step,step_ordinal,step_bytes,key,hash,bytes,previous_root,root,previous_step_root,step_root,historical FROM ci_log_accepted_chunks WHERE view=?1 AND step=?2 AND step_ordinal=?3 AND revision<=?4",params![view,step,next,revision],|r|Ok(Chunk{view:r.get(0)?,revision:r.get(1)?,operation:r.get(2)?,step:r.get(3)?,ordinal:r.get(4)?,step_bytes:r.get(5)?,key:r.get(6)?,hash:r.get(7)?,bytes:r.get(8)?,previous_root:r.get(9)?,root:r.get(10)?,previous_step_root:r.get(11)?,step_root:r.get(12)?,historical:r.get(13)?}))?;
                Ok((chunk,previous))
            }).await?;
            anyhow::ensure!(chunk.bytes<=u64::try_from(CHUNK)? && chunk.ordinal==previous.0+1 && chunk.previous_step_root==previous.2 && previous.1.checked_add(chunk.bytes)==Some(chunk.step_bytes),"copy source step chain is incomplete");
            let (meta,body)=ctx.blobs.get(&chunk.key,None).await?;
            let (ordinal,total,root)=(*ordinal,*total,root.clone());
            crate::rt::blocking(move ||{
                anyhow::ensure!(meta.key==chunk.key && meta.etag==chunk.hash && meta.size==chunk.bytes && u64::try_from(body.len()).ok()==Some(chunk.bytes) && hash(&body)==chunk.hash,"copy source immutable bytes unavailable or corrupt");
                anyhow::ensure!(chain(&chunk.previous_root,&chunk)==chunk.root && chain(&chunk.previous_step_root,&chunk)==chunk.step_root,"copy source manifest root invalid");
                let done=chunk.ordinal==ordinal;
                anyhow::ensure!(chunk.ordinal<=ordinal && chunk.step_bytes<=total && (!done || (chunk.step_bytes==total && chunk.step_root==root)),"copy source tail differs from original descriptor");
                Ok((body,u64::try_from(chunk.ordinal)?,done))
            }).await?
        }
    }
}

async fn retained_operation(ctx:Context,mut op:Operation)->anyhow::Result<Receipt> {
    loop {
        let preparation=crate::rt::blocking({let op=op.clone();move ||prepared(op)}).await.and_then(|r|r);
        let result=match preparation {Ok(op)=>continue_operation(ctx.clone(),op).await,Err(error)=>Err(error)};
        match result {
            Ok(done)=>return Ok(done),
            Err(error)=>{
                let id=op.id.clone();
                let stale=ctx.store.call(move|c|Ok(copy_source(c,&id)?.is_some() && !copy_current(c,&id)?)).await;
                if matches!(stale,Ok(true)){return Ok(refused(&ctx,op.id,"original copy target ceased to be current while preparing; owned pins retained".into()).await)}
                // Reads/preparation retry under the same operation. Conditional mutations
                // reconcile/replay their exact captured descriptor in transition(), never
                // a new append identity or an inferred producer close.
                tracing::warn!(operation=op.id,error=%format!("{error:#}"),"original accepted-prefix operation remains incomplete; pins and custody retained");
                crate::rt::sleep(std::time::Duration::from_millis(250)).await;
                op=load(&ctx.store,op.id).await;
            }
        }
    }
}

/// Restart never fabricates an accepted empty input. The original retained key and digest
/// must be readable, or the existing operation stays Unknown and consumes its finite slot.
async fn recover_input(ctx:&Context,op:&Operation) {
    loop {
        match read_key(&ctx.blobs,&op.input_key,&op.input_hash,op.input_bytes).await {
            Ok(true)=>{
                let key=op.input_key.clone();let digest=op.input_hash.clone();let size=op.input_bytes;let check=key.clone();
                transition(&ctx.store,move |c|{c.execute("UPDATE ci_log_accepted_pins SET ambiguous=CASE WHEN put_started=1 AND ready=0 THEN 1 ELSE ambiguous END,put_started=1,ready=1 WHERE key=?1 AND hash=?2 AND bytes=?3",params![key,digest,size])?;Ok(())},move |c|Ok(c.query_row("SELECT ready FROM ci_log_accepted_pins WHERE key=?1",[&check],|r|r.get::<_,bool>(0)).optional()?.filter(|ready|*ready))).await;
                input_ready(ctx,op).await;return;
            }
            Err(why)=>{refused(ctx,op.id.clone(),why).await;return}
            Ok(false)=>crate::rt::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
}

/// Sorted legacy document metadata was built in 32-row pages before this phase.
/// Retain ONE normalized step and a forward-only line cursor: healthy verification work
/// is linear in captured step bytes plus compared legacy document bytes, not D times S.
struct HistoryText {step:u32,text:String,line:usize,at:usize}
impl HistoryText {
    fn matches(mut self,body:String,line:i64)->anyhow::Result<(bool,Self)> {
        let line=usize::try_from(line)?;anyhow::ensure!(line>=self.line,"historical document order regressed");
        while self.line<line {
            let Some(next)=self.text.get(self.at..).and_then(|rest|rest.find('\n')) else {return Ok((false,self))};
            self.at=self.at.checked_add(next+1).ok_or_else(||anyhow::anyhow!("historical line offset overflow"))?;self.line+=1;
        }
        let rest=self.text.get(self.at..).ok_or_else(||anyhow::anyhow!("historical line boundary invalid"))?;
        let agrees=rest.split('\n').take(crate::ci::logsearch::SEGMENT_LINES).collect::<Vec<_>>().join("\n")==body;
        Ok((agrees,self))
    }
}
async fn verify_history(ctx:&Context,mut op:Operation)->anyhow::Result<Operation> {
    let mut cached:Option<HistoryText>=None;
    while op.status=="pending" && op.baseline_state=="verifying" {
        let id=op.id.clone();
        let descriptor=ctx.store.call(move |c|Ok(c.query_row("SELECT doc,step,line,version FROM ci_log_baseline_docs WHERE operation=?1 AND verified=0 ORDER BY step,line,doc LIMIT 1",[id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,u32>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<String>>(3)?))).optional()?)).await?;
        if let Some((doc,step,line,version))=descriptor {
            let job=op.job.clone();let expected=version.clone();
            let bytes=ctx.store.call(move |c|Ok(c.query_row("SELECT length(CAST(text AS BLOB)) FROM ci_log_docs WHERE id=?1 AND leg=?2 AND step=?3 AND line=?4 AND index_version IS ?5",params![doc,job,step,line,expected],|r|r.get::<_,u64>(0)).optional()?)).await?;
            if bytes.is_some_and(|bytes|bytes>u64::try_from(crate::ci::logsearch::MAX_INDEXED).unwrap_or(0)) {
                refused(ctx,op.id.clone(),"historical searchable segment exceeds its existing normalized bound; no shadowing".into()).await;return Ok(load(&ctx.store,op.id).await);
            }
            if bytes.is_some() {
                let expected=version.clone();let job=op.job.clone();
                let body=ctx.store.call(move |c|Ok(c.query_row("SELECT text FROM ci_log_docs WHERE id=?1 AND leg=?2 AND step=?3 AND line=?4 AND index_version IS ?5",params![doc,job,step,line,expected],|r|r.get::<_,String>(0)).optional()?)).await?;
                if let Some(body)=body {
                    let history=if cached.as_ref().is_some_and(|history|history.step==step) {cached.take().ok_or_else(||anyhow::anyhow!("historical cache disappeared"))?}else{
                        let prefix=Prefix{view:op.view.clone(),revision:op.staged_revision,root:op.staged_root.clone()};
                        let mut reader=Reader::open(&ctx.store,prefix,step,crate::ci::logsearch::MAX_INDEXED).await?;
                        let text=loop {match reader.advance(&ctx.store,&ctx.blobs).await?{ReadTurn::Pending(next)=>reader=*next,ReadTurn::Ready(text,_)=>break text}};
                        HistoryText{step,text,line:1,at:0}
                    };
                    let (agrees,history)=crate::rt::blocking(move ||history.matches(body,line)).await??;cached=Some(history);
                    if !agrees {refused(ctx,op.id.clone(),"historical searchable text differs from the immutable captured baseline; no shadowing".into()).await;return Ok(load(&ctx.store,op.id).await);}
                }
            }
            let id=op.id.clone();let check=id.clone();let original=version.clone();
            transition(&ctx.store,move |c|{c.execute("UPDATE ci_log_baseline_docs SET verified=1 WHERE operation=?1 AND step=?2 AND line=?3 AND doc=?4 AND version IS ?5",params![id,step,line,doc,version])?;Ok(())},move |c|Ok(c.query_row("SELECT verified FROM ci_log_baseline_docs WHERE operation=?1 AND step=?2 AND line=?3 AND doc=?4 AND version IS ?5",params![check,step,line,doc,original],|r|r.get::<_,bool>(0)).optional()?.filter(|verified|*verified))).await;
        }else{
            let id=op.id.clone();let check=id.clone();
            transition(&ctx.store,move |c|{c.execute("UPDATE ci_log_accepted_ops SET baseline_state='ready' WHERE operation=?1 AND status='pending' AND baseline_state='verifying' AND NOT EXISTS(SELECT 1 FROM ci_log_baseline_docs WHERE operation=?1 AND verified=0)",[&id])?;Ok(())},move |c|Ok(operation(c,&check)?.filter(|op|op.baseline_state=="ready" || op.status!="pending"))).await;
        }
        op=load(&ctx.store,op.id).await;
    }
    Ok(op)
}

async fn continue_operation(ctx:Context,mut op:Operation)->anyhow::Result<Receipt> {
    let id=op.id.clone();let copy=ctx.store.call(move |c|copy_source(c,&id)).await?;
    if op.status!="pending" {return receipt(&op)}
    if copy.is_some() {
        let _=require_result(&ctx,&op.id).await?;pin_outputs(&ctx,&op.id).await?;
        if op.input_bytes==0 {
            let id=op.id.clone();let check=id.clone();
            return Ok(transition(&ctx.store,move |c|{
                copy_transition(c,&id,|c|{
                    c.execute("UPDATE ci_log_accepted_ops SET status='no_bytes',receipt_revision=expected_revision,receipt_root=expected_root WHERE operation=?1 AND status='pending' AND input_bytes=0 AND EXISTS(SELECT 1 FROM ci_log_accepted_copy x WHERE x.operation=?1 AND x.result_ready=1 AND x.objects_complete=1)",[&id])?;Ok(())
                })
            },move |c|completed(c,&check)).await);
        }
    }
    if !op.input_ready {recover_input(&ctx,&op).await;op=prepared(load(&ctx.store,op.id).await)?;}
    loop {
        if op.status!="pending" {return receipt(&op)}
        if op.input_staged {break}
        if matches!(op.baseline_state.as_str(),"discovering"|"checking") {
            op=baseline(&ctx,op).await?;
            if op.status!="pending" {return receipt(&op)}
        }
        let file=if op.baseline_state=="staging" {next_file(&ctx,&op).await?}else{None};
        if op.baseline_state=="staging" && file.is_none() {
            let id=op.id.clone();let check=id.clone();
            transition(&ctx.store,move |c|{c.execute("UPDATE ci_log_accepted_ops SET baseline_state='verifying',history_after=NULL WHERE operation=?1 AND status='pending' AND baseline_state='staging'",[&id])?;Ok(())},move |c|Ok(operation(c,&check)?.filter(|op|op.baseline_state!="staging" || op.status!="pending"))).await;
            op=load(&ctx.store,op.id).await;continue;
        }
        if op.baseline_state=="verifying" {
            op=verify_history(&ctx,op).await?;
            if op.status!="pending" {return receipt(&op)}
        }
        let historical=file.is_some();
        let (step,key,bytes,next_file,next_offset,input,input_cursor,write)=if let Some(file)=file {
            if ctx.host!=op.baseline_host {return Ok(refused(&ctx,op.id,"the original historical capture belongs to a different host; preserved, not adopted".into()).await)}
            let bytes=match ctx.files.captured_chunk(&op.job,&file,op.offset).await {
                Ok(bytes)=>bytes,
                Err(error)=>return Ok(refused(&ctx,op.id,format!("historical capture changed or is unavailable: {error:#}")).await),
            };
            let end=op.offset.checked_add(u64::try_from(bytes.len())?).ok_or_else(||anyhow::anyhow!("historical offset overflow"))?;
            let done=end==file.bytes;
            (file.step,format!("ci-logs/{}/{}/baseline-{}-{}",op.view,op.id,file.step,op.offset),bytes,if done {u64::from(file.step)+1}else{u64::from(file.step)},if done {0}else{end},false,op.input_cursor,true)
        } else if let Some(source)=&copy {
            let (bytes,next,done)=copied_chunk(&ctx,&op,source).await?;
            (op.step,format!("ci-logs/{}/{}/copy-{}",op.view,op.id,op.input_cursor),bytes,op.file,op.offset,done,next,true)
        } else {
            let (meta,bytes)=ctx.blobs.get(&op.input_key,None).await?;
            anyhow::ensure!(meta.key==op.input_key && meta.etag==op.input_hash && meta.size==op.input_bytes && u64::try_from(bytes.len()).ok()==Some(op.input_bytes) && hash(&bytes)==op.input_hash,"original retained input is missing or corrupt; no publication");
            (op.step,op.input_key.clone(),bytes,op.file,0,true,op.input_cursor,false)
        };
        let captured=op.clone();
        let old=ctx.store.call(move |c|previous(c,&captured,step)).await?;
        let captured=op.clone();
        let chunk=crate::rt::blocking(move ||prepare(&captured,step,key,&bytes,old,historical).map(|p|(p,bytes))).await??;
        let (p,bytes)=chunk;
        let proposal=p.clone();let expected=p.clone();
        transition(&ctx.store,move |c|pin(c,&proposal),move |c|{
            let Some(op)=operation(c,&expected.operation)? else {return Ok(None)};
            if op.status!="pending" {return Ok(Some(()))}
            let owned:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_pins WHERE key=?1 AND view=?2 AND operation=?3 AND hash=?4 AND bytes=?5)",params![expected.key,expected.view,expected.operation,expected.hash,expected.bytes],|r|r.get(0))?;
            Ok(owned.then_some(()))
        }).await;
        if write {
            if let Err(why)=owned_put(&ctx,&p.key,&p.hash,bytes).await {return Ok(refused(&ctx,op.id,why).await)}
        }
        let proposal=p.clone();let expected=p.clone();
        transition(&ctx.store,move |c|stage(c,&proposal,next_file,next_offset,input,input_cursor),move |c|{
            let Some(op)=operation(c,&expected.operation)? else {return Ok(None)};
            if op.status!="pending" {return Ok(Some(()))}
            let root:Option<String>=c.query_row("SELECT root FROM ci_log_accepted_chunks WHERE view=?1 AND revision=?2 AND operation=?3",params![expected.view,expected.revision,expected.operation],|r|r.get(0)).optional()?;
            Ok((root.as_deref()==Some(expected.root.as_str())).then_some(()))
        }).await;
        op=prepared(load(&ctx.store,op.id).await)?;
    }
    if op.expected_revision==0 {
        if let Err(error)=ctx.files.verify_directory(&op.job,op.baseline_json.clone()).await {
            return Ok(refused(&ctx,op.id,format!("historical directory changed before publication: {error:#}")).await);
        }
    }
    let id=op.id.clone();let check=id.clone();
    let done=transition(&ctx.store,move |c|{publish(c,&id)?;Ok(())},move |c|completed(c,&check)).await;
    if done.accepted {
        crate::ci::logsearch::kick();
        // Compatibility output follows the OWN durable receipt. Failure here does not
        // erase acceptance, mutate its immutable source, or become delivery closure.
        if let Err(error)=project_input(&ctx,&op,copy.is_some()).await {
            tracing::warn!(operation=op.id,error=%format!("{error:#}"),"accepted bytes are durable; compatibility projection is behind");
        }
    }
    Ok(done)
}

async fn project_input(ctx:&Context,op:&Operation,copied:bool)->anyhow::Result<()> {
    if !copied {
        let (meta,bytes)=ctx.blobs.get(&op.input_key,None).await?;
        anyhow::ensure!(meta.etag==op.input_hash && meta.size==op.input_bytes && hash(&bytes)==op.input_hash,"accepted input projection source invalid");
        let files=ctx.files.clone();let job=op.job.clone();let step=op.step;
        crate::rt::blocking(move ||files.append(&job,step,&bytes)).await?;return Ok(());
    }
    let mut after=op.expected_revision;
    loop {
        let id=op.id.clone();let lower=after;
        let row=ctx.store.call(move |c|Ok(c.query_row("SELECT revision,key,hash,bytes FROM ci_log_accepted_chunks WHERE operation=?1 AND historical=0 AND revision>?2 ORDER BY revision LIMIT 1",params![id,lower],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,u64>(3)?))).optional()?)).await?;
        let Some((revision,key,digest,size))=row else {return Ok(())};
        let (meta,bytes)=ctx.blobs.get(&key,None).await?;
        anyhow::ensure!(size<=u64::try_from(CHUNK)? && meta.key==key && meta.etag==digest && meta.size==size && u64::try_from(bytes.len()).ok()==Some(size) && hash(&bytes)==digest,"accepted copy projection source invalid");
        let files=ctx.files.clone();let job=op.job.clone();let step=op.step;
        crate::rt::blocking(move ||files.append(&job,step,&bytes)).await?;after=revision;
    }
}

/// One transient step reader in the EXISTING indexer. SQL returns one small descriptor;
/// the configured object store and normalizer run outside the shared database writer.
/// The published head is immutable; a later head invalidates this plan at stage/post.
pub(crate) struct Reader {
    prefix:Prefix,step:u32,after:i64,ordinal:i64,bytes:u64,root:String,
    expected_ordinal:i64,expected_bytes:u64,expected_root:String,plain:crate::ci::logsearch::Plain,
}
pub(crate) enum ReadTurn {Pending(Box<Reader>),Ready(String,u64)}
impl Reader {
    pub(crate) async fn open(store:&Store,prefix:Prefix,step:u32,limit:usize)->anyhow::Result<Self> {
        let view=prefix.view.clone();let revision=prefix.revision;let expected=prefix.root.clone();
        let (ordinal,bytes,root)=store.call(move |c|{
            let root:String=c.query_row("SELECT root FROM ci_log_accepted_chunks WHERE view=?1 AND revision=?2",params![view,revision],|r|r.get(0))?;
            anyhow::ensure!(root==expected,"captured accepted head root differs from immutable manifest");
            Ok(c.query_row("SELECT step_ordinal,step_bytes,step_root FROM ci_log_accepted_chunks WHERE view=?1 AND step=?2 AND revision<=?3 ORDER BY revision DESC LIMIT 1",params![view,step,revision],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?)))?)
        }).await?;
        Ok(Self{prefix,step,after:0,ordinal:0,bytes:0,root:EMPTY.into(),expected_ordinal:ordinal,expected_bytes:bytes,expected_root:root,plain:crate::ci::logsearch::Plain::new(limit)})
    }
    pub(crate) async fn advance(mut self,store:&Store,blobs:&Arc<dyn Blobs>)->anyhow::Result<ReadTurn> {
        let (view,step,revision,after)=(self.prefix.view.clone(),self.step,self.prefix.revision,self.after);
        let row=store.call(move |c|Ok(c.query_row("SELECT view,revision,operation,step,step_ordinal,step_bytes,key,hash,bytes,previous_root,root,previous_step_root,step_root,historical FROM ci_log_accepted_chunks WHERE view=?1 AND step=?2 AND revision<=?3 AND revision>?4 ORDER BY revision LIMIT 1",params![view,step,revision,after],|r|Ok(Chunk{view:r.get(0)?,revision:r.get(1)?,operation:r.get(2)?,step:r.get(3)?,ordinal:r.get(4)?,step_bytes:r.get(5)?,key:r.get(6)?,hash:r.get(7)?,bytes:r.get(8)?,previous_root:r.get(9)?,root:r.get(10)?,previous_step_root:r.get(11)?,step_root:r.get(12)?,historical:r.get(13)?})).optional()?)).await?;
        let chunk=row.ok_or_else(||anyhow::anyhow!("accepted step manifest is incomplete; no empty/readiness substitution"))?;
        anyhow::ensure!(chunk.bytes<=u64::try_from(CHUNK)? && chunk.ordinal==self.ordinal+1 && chunk.previous_step_root==self.root && self.bytes.checked_add(chunk.bytes)==Some(chunk.step_bytes),"accepted step manifest chain is inconsistent");
        let (meta,bytes)=blobs.get(&chunk.key,None).await?;
        let ready=crate::rt::blocking(move ||{
            anyhow::ensure!(meta.key==chunk.key && meta.etag==chunk.hash && meta.size==chunk.bytes && u64::try_from(bytes.len()).ok()==Some(chunk.bytes) && hash(&bytes)==chunk.hash,"accepted immutable bytes missing or corrupt; index remains incomplete");
            anyhow::ensure!(chain(&chunk.previous_root,&chunk)==chunk.root && chain(&chunk.previous_step_root,&chunk)==chunk.step_root,"accepted manifest root check failed");
            self.plain.feed(&bytes);self.after=chunk.revision;self.ordinal=chunk.ordinal;self.bytes=chunk.step_bytes;self.root=chunk.step_root;
            let last=self.ordinal==self.expected_ordinal;
            if last {anyhow::ensure!(self.bytes==self.expected_bytes && self.root==self.expected_root,"accepted step tail differs from captured manifest");}
            if last || self.plain.done() {Ok(ReadTurn::Ready(self.plain.finish(),self.expected_bytes))}else{Ok(ReadTurn::Pending(Box::new(self)))}
        }).await??;
        Ok(ready)
    }
}

/// The refusal receipt and every immutable/CAS pin remain owned. Only metadata that was
/// never named by the original checked head is unlinked, one fixed page per indexer turn.
#[derive(Clone,PartialEq,Eq)]
struct Unpublished {view:String,id:String,revision:i64,root:String,phase:i64,chunks:i64,catalog:i64}
fn unpublished(c:&Connection,view:&str)->anyhow::Result<Option<Unpublished>> {
    Ok(c.query_row("SELECT view,operation,revision,root,phase,chunk_after,catalog_after FROM ci_log_accepted_unpublished WHERE view=?1",[view],|r|Ok(Unpublished{view:r.get(0)?,id:r.get(1)?,revision:r.get(2)?,root:r.get(3)?,phase:r.get(4)?,chunks:r.get(5)?,catalog:r.get(6)?})).optional()?)
}
#[derive(Clone)]
enum UnpublishedPage {Chunks(Vec<i64>),Catalog(Vec<(u32,i64)>)}

pub(crate) async fn repair_unpublished(store:&Store,cursor:&mut Option<String>)->anyhow::Result<bool> {
    let after=cursor.clone();
    let selected=store.call(move |c|{
        let next=if let Some(after)=after {c.query_row("SELECT view FROM ci_log_accepted_unpublished WHERE view>?1 ORDER BY view LIMIT 1",[after],|r|r.get::<_,String>(0)).optional()?}else{None};
        let next=if next.is_some(){next}else{c.query_row("SELECT view FROM ci_log_accepted_unpublished ORDER BY view LIMIT 1",[],|r|r.get(0)).optional()?};
        let Some(view)=next else{return Ok(None)};
        let original=unpublished(c,&view)?.ok_or_else(||anyhow::anyhow!("original suffix intent disappeared during selection"))?;
        Ok(Some(original))
    }).await?;
    let Some(original)=selected else{*cursor=None;return Ok(false)};
    // Advance before page preparation/admission: a conflicting original cannot monopolize
    // later repair intents, and this cursor confers no publication/cleanup receipt.
    *cursor=Some(original.view.clone());
    let expected=original.clone();
    let page=store.call(move |c|{
        if unpublished(c,&expected.view)?.as_ref()!=Some(&expected){return Ok(None)}
        if expected.phase==0 {
            let rows=c.prepare("SELECT revision,operation FROM ci_log_accepted_chunks WHERE view=?1 AND revision>?2 ORDER BY revision LIMIT 32")?.query_map(params![expected.view,expected.chunks],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<crate::sql::Result<Vec<_>>>()?;
            anyhow::ensure!(rows.iter().all(|(revision,id)|*revision>expected.revision && id==&expected.id),"unpublished suffix has another owner; exact original fence retained");
            Ok(Some(UnpublishedPage::Chunks(rows.into_iter().map(|(revision,_)|revision).collect())))
        }else{
            let rows=c.prepare("SELECT step,first_revision FROM ci_log_accepted_steps WHERE view=?1 AND first_revision>?2 ORDER BY first_revision LIMIT 32")?.query_map(params![expected.view,expected.catalog],|r|Ok((r.get::<_,u32>(0)?,r.get::<_,i64>(1)?)))?.collect::<crate::sql::Result<Vec<_>>>()?;
            Ok(Some(UnpublishedPage::Catalog(rows)))
        }
    }).await?;
    let Some(page)=page else{return Ok(false)};
    // Unknown may leave this exact page callback in flight. A later turn reconciles the
    // persisted cursor; replay with the old descriptor is an idempotent no-op, never a
    // fresh delete-next-page operation. All metadata work is bounded to32 captured rows.
    store.call(move |c|{
        let tx=crate::store::tx(c)?;
        if unpublished(&tx,&original.view)?.as_ref()!=Some(&original){return Ok(false)}
        let op=operation(&tx,&original.id)?.ok_or_else(||anyhow::anyhow!("original suffix refusal receipt disappeared"))?;
        anyhow::ensure!(op.status=="refused" && op.view==original.view && op.expected_revision==original.revision && op.expected_root==original.root,"suffix repair lacks exact original refusal");
        let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_views v WHERE v.view=?1 AND v.revision=?2 AND v.root=?3 AND v.fenced=1 AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_ops o WHERE o.view=v.view AND o.status='pending'))",params![original.view,original.revision,original.root],|r|r.get(0))?;
        anyhow::ensure!(exact,"published head changed or another owner is pending; original suffix fence retained");
        match page {
            UnpublishedPage::Chunks(rows)=>{
                if let Some(after)=rows.last() {
                    for revision in &rows {
                        let removed=tx.execute("DELETE FROM ci_log_accepted_chunks WHERE view=?1 AND revision=?2 AND operation=?3 AND revision>?4",params![original.view,revision,original.id,original.revision])?;
                        anyhow::ensure!(removed==1,"original unpublished chunk page changed");
                    }
                    tx.execute("UPDATE ci_log_accepted_unpublished SET chunk_after=?2 WHERE view=?1",params![original.view,after])?;
                }else{tx.execute("UPDATE ci_log_accepted_unpublished SET phase=1 WHERE view=?1",[&original.view])?;}
            }
            UnpublishedPage::Catalog(rows)=>{
                if let Some((_,after))=rows.last() {
                    for (step,revision) in &rows {
                        anyhow::ensure!(*revision>original.revision && *revision<=op.staged_revision,"catalog row lies outside the original refused suffix");
                        let removed=tx.execute("DELETE FROM ci_log_accepted_steps WHERE view=?1 AND step=?2 AND first_revision=?3 AND first_revision>?4",params![original.view,step,revision,original.revision])?;
                        anyhow::ensure!(removed==1,"original unpublished catalog page changed");
                    }
                    tx.execute("UPDATE ci_log_accepted_unpublished SET catalog_after=?2 WHERE view=?1",params![original.view,after])?;
                }else{
                    let remains:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_chunks WHERE view=?1 AND revision>?2) OR EXISTS(SELECT 1 FROM ci_log_accepted_steps WHERE view=?1 AND first_revision>?2)",params![original.view,original.revision],|r|r.get(0))?;
                    anyhow::ensure!(!remains,"original unpublished suffix still has rows; fence retained");
                    tx.execute("UPDATE ci_log_accepted_views SET fenced=0 WHERE view=?1 AND retired=0 AND revision=?2 AND root=?3",params![original.view,original.revision,original.root])?;
                    tx.execute("DELETE FROM ci_log_accepted_unpublished WHERE view=?1 AND operation=?2 AND revision=?3 AND root=?4",params![original.view,original.id,original.revision,original.root])?;
                }
            }
        }
        tx.commit()?;Ok(true)
    }).await
}

/// Same existing indexer cursor: blocked views still yield a turn to later work.
pub(crate) async fn retirement_view(store:&Store,after:Option<String>)->anyhow::Result<Option<String>> {
    store.call(move |c|{
        let next=if let Some(after)=after {c.query_row("SELECT view FROM ci_log_accepted_retired WHERE view>?1 ORDER BY view LIMIT 1",[after],|r|r.get::<_,String>(0)).optional()?}else{None};
        Ok(if next.is_some(){next}else{c.query_row("SELECT view FROM ci_log_accepted_retired ORDER BY view LIMIT 1",[],|r|r.get(0)).optional()?})
    }).await
}

/// One owned key or metadata page. Ambiguous PUT lifetimes retain their key+pin and all
/// view provenance; positive GET is a publication proof, not physical request completion.
pub(crate) async fn retire_turn(logs:&super::Logs,store:&Store,blobs:&Arc<dyn Blobs>,view:String)->anyhow::Result<bool> {
    let original=view.clone();
    let job=store.call(move |c|Ok(c.query_row("SELECT job FROM ci_log_accepted_views v WHERE view=?1 AND retired=1 AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.id=v.job)",[original],|r|r.get::<_,String>(0)).optional()?)).await?;
    let Some(job)=job else {return Ok(false)};
    // No await under the owner lock. Current IDs cannot be recreated after retirement.
    // New requests register before lookup; late Resume performs its own checked lookup.
    if logs.io.lock().busy(&job) {return Ok(false)}
    let check=view.clone();
    let pending=store.call(move |c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_ops WHERE view=?1 AND status='pending' UNION ALL SELECT 1 FROM ci_log_accepted_copy WHERE source_view=?1 AND active=1 UNION ALL SELECT 1 FROM ci_log_accepted_unpublished WHERE view=?1)",[check],|r|r.get::<_,bool>(0))?)).await?;
    anyhow::ensure!(!pending,"retired view {view} retains a pending original operation; no cleanup verdict");
    let key_view=view.clone();
    let pin=store.call(move |c|{
        let after:Option<String>=c.query_row("SELECT after_key FROM ci_log_accepted_retired WHERE view=?1",[&key_view],|r|r.get(0)).optional()?;
        let Some(after)=after else {return Ok(None)};
        let map=|r:&crate::sql::Row<'_>|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?));
        let next=c.query_row("SELECT key,hash,bytes FROM ci_log_accepted_pins WHERE view=?1 AND ambiguous=0 AND key>?2 ORDER BY key LIMIT 1",params![key_view,after],map).optional()?;
        Ok(if next.is_some(){next}else{c.query_row("SELECT key,hash,bytes FROM ci_log_accepted_pins WHERE view=?1 AND ambiguous=0 ORDER BY key LIMIT 1",[key_view],map).optional()?})
    }).await?;
    if let Some((key,digest,size))=pin {
        // Advance BEFORE I/O: a permanently conflicting/unavailable key cannot monopolize
        // later eligible keys. This cursor conveys no deletion or acceptance authority.
        let cursor=view.clone();let attempted=key.clone();
        store.call(move |c|{c.execute("UPDATE ci_log_accepted_retired SET after_key=?2 WHERE view=?1",params![cursor,attempted])?;Ok(())}).await?;
        match blobs.head(&key).await {
            Ok(meta)=>{
                anyhow::ensure!(meta.key==key && meta.etag==digest && meta.size==size,"retired immutable key conflicts; pin retained");
                match blobs.delete(&key).await {Ok(())|Err(Refusal::NotFound(_))=>{},Err(error)=>return Err(error.into())}
            }
            Err(Refusal::NotFound(_))=>{},Err(error)=>return Err(error.into()),
        }
        store.call(move |c|{c.execute("DELETE FROM ci_log_accepted_pins WHERE key=?1 AND view=?2 AND hash=?3 AND bytes=?4 AND ambiguous=0 AND EXISTS(SELECT 1 FROM ci_log_accepted_views v WHERE v.view=?2 AND v.retired=1) AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_ops o WHERE o.view=?2 AND o.status='pending') AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_copy x WHERE x.source_view=?2 AND x.active=1)",params![key,view,digest,size])?;Ok(())}).await?;
        return Ok(true);
    }
    store.call(move |c|{
        let tx=crate::store::tx(c)?;
        let held:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_pins WHERE view=?1 AND ambiguous=1)",[&view],|r|r.get(0))?;
        anyhow::ensure!(!held,"retired view {view} retains ambiguous PUT lifetimes, immutable keys and SQL pins; cleanup incomplete");
        let eligible:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_views v WHERE view=?1 AND retired=1 AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_ops o WHERE o.view=v.view AND o.status='pending') AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_pins p WHERE p.view=v.view) AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_unpublished u WHERE u.view=v.view) AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_copy x WHERE x.source_view=v.view AND x.active=1))",[&view],|r|r.get(0))?;
        if !eligible {return Ok(false)}
        let chunks=tx.execute("DELETE FROM ci_log_accepted_chunks WHERE view=?1 AND revision IN (SELECT revision FROM ci_log_accepted_chunks WHERE view=?1 ORDER BY revision LIMIT 32)",[&view])?;
        if chunks==0 {
            let operation:Option<String>=tx.query_row("SELECT operation FROM ci_log_accepted_ops WHERE view=?1 ORDER BY operation LIMIT 1",[&view],|r|r.get(0)).optional()?;
            if let Some(operation)=operation {
                let files=tx.execute("DELETE FROM ci_log_baseline_files WHERE operation=?1 AND step IN (SELECT step FROM ci_log_baseline_files WHERE operation=?1 ORDER BY step LIMIT 32)",[&operation])?;
                if files==0 {
                    let docs=tx.execute("DELETE FROM ci_log_baseline_docs WHERE operation=?1 AND (step,line,doc) IN (SELECT step,line,doc FROM ci_log_baseline_docs WHERE operation=?1 ORDER BY step,line,doc LIMIT 32)",[&operation])?;
                    if docs==0 {
                        // Target is durably retired before refs can be removed; late CAS pin
                        // callbacks use require_copy_object_pin and cannot recreate them.
                        let refs=tx.execute("DELETE FROM ci_log_copy_objects WHERE operation=?1 AND digest IN (SELECT digest FROM ci_log_copy_objects WHERE operation=?1 ORDER BY digest LIMIT 32)",[&operation])?;
                        if refs==0 {
                            let parts=tx.execute("DELETE FROM ci_log_copy_result_parts WHERE operation=?1 AND part IN (SELECT part FROM ci_log_copy_result_parts WHERE operation=?1 ORDER BY part LIMIT 32)",[&operation])?;
                            if parts==0 {
                                tx.execute("UPDATE ci_log_accepted_copy SET active=0 WHERE operation=?1",[&operation])?;
                                tx.execute("DELETE FROM ci_log_accepted_copy WHERE operation=?1 AND active=0",[&operation])?;
                                tx.execute("DELETE FROM ci_log_accepted_ops WHERE operation=?1 AND status!='pending'",[operation])?;
                            }
                        }
                    }
                }
            }else{
                let steps=tx.execute("DELETE FROM ci_log_accepted_steps WHERE view=?1 AND step IN (SELECT step FROM ci_log_accepted_steps WHERE view=?1 ORDER BY step LIMIT 32)",[&view])?;
                if steps==0 {tx.execute("DELETE FROM ci_log_accepted_retired WHERE view=?1",[&view])?;}
            }
        }
        tx.commit()?;Ok(true)
    }).await
}

/// One existing indexer file-retention turn. Active source SQL leases, pending own I/O
/// and exact directory/host custody fail closed; a blocked item yields to later intents.
pub(crate) async fn retire_file_turn(logs:&super::Logs,cursor:&mut Option<String>)->anyhow::Result<bool> {
    let ctx=logs.accepted_context()?;let after=cursor.clone();
    let next=ctx.store.call(move |c|{
        // Select ONE intent by its indexed cursor before eligibility checks. Filtering
        // all held intents to find a match would be an unbounded shared-writer scan.
        let row=if let Some(after)=after {c.query_row("SELECT job FROM ci_log_deferred_delete WHERE job>?1 ORDER BY job LIMIT 1",[after],|r|r.get::<_,String>(0)).optional()?}else{None};
        Ok(if row.is_some(){row}else{c.query_row("SELECT job FROM ci_log_deferred_delete ORDER BY job LIMIT 1",[],|r|r.get(0)).optional()?})
    }).await?;
    let Some(job)=next else{*cursor=None;return Ok(false)};
    *cursor=Some(job.clone());
    let check=job.clone();
    let eligible=ctx.store.call(move |c|Ok(c.query_row("SELECT NOT EXISTS(SELECT 1 FROM jobs WHERE id=?1) AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_copy WHERE source_job=?1 AND active=1) AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_ops WHERE job=?1 AND status='pending')",[check],|r|r.get::<_,bool>(0))?)).await?;
    if !eligible || logs.io.lock().busy(&job) {return Ok(false)}
    let capture=ctx.files.delete_capture(&job).await?;
    let id=job.clone();let original=capture.clone();let host=ctx.host.clone();let check=id.clone();let own_host=host.clone();
    transition(&ctx.store,move |c|{
        c.execute("UPDATE ci_log_deferred_delete SET host=?2,capture=?3 WHERE job=?1 AND host IS NULL AND capture IS NULL AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.id=?1) AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_copy x WHERE x.source_job=?1 AND x.active=1)",params![id,host,original])?;Ok(())
    },move |c|{
        let got:Option<(Option<String>,Option<String>)>=c.query_row("SELECT host,capture FROM ci_log_deferred_delete WHERE job=?1",[&check],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        Ok(got.and_then(|(host,capture)|host.zip(capture)).map(|(host,capture)|(host==own_host,capture)))
    }).await;
    let check=job.clone();let host=ctx.host.clone();
    let original:Option<String>=ctx.store.call(move |c|Ok(c.query_row("SELECT capture FROM ci_log_deferred_delete WHERE job=?1 AND host=?2 AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.id=?1) AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_copy x WHERE x.source_job=?1 AND x.active=1)",params![check,host],|r|r.get(0)).optional()?)).await?;
    let Some(original)=original else {return Ok(false)};
    // INSERT fence prevents same-ID replacement while this durable intent exists. Current
    // accepted producer futures are still checked; tombstones do not prove old-writer drain.
    if {let owner=logs.io.lock();owner.busy(&job)} {return Ok(false)}
    ctx.files.delete_captured(&job,original.clone()).await?;
    let done=job.clone();let host=ctx.host.clone();
    ctx.store.call(move |c|{c.execute("DELETE FROM ci_log_deferred_delete WHERE job=?1 AND host=?2 AND capture=?3 AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.id=?1) AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_copy x WHERE x.source_job=?1 AND x.active=1)",params![done,host,original])?;Ok(())}).await?;
    Ok(true)
}

const IO_SLOTS:usize=8;
const SERVER_SLOTS:usize=8;

pub(crate) struct Observed {
    pub job:String,pub step:u32,pub input:Input,pub origin:String,pub answer:Option<Answer>,
}
pub(crate) enum Answer {
    Append(futures::channel::oneshot::Sender<Result<Receipt,String>>),
    Copy(futures::channel::oneshot::Sender<Result<Vec<u8>,String>>),
}
pub(crate) enum Input {
    Frame(crate::net::FrameBytes,std::ops::Range<usize>),
    Server(Vec<u8>),
    Copy(Box<CopyStep>),
    Resume(String),
}
impl Input {
    fn bytes(&self)->anyhow::Result<&[u8]> {
        match self {
            Self::Frame(frame,range)=>frame.get(range.clone()).ok_or_else(||anyhow::anyhow!("captured log body range is invalid")),
            Self::Server(bytes)=>Ok(bytes),
            Self::Copy(_)|Self::Resume(_)=>anyhow::bail!("copy input is a durable descriptor, not a frame body"),
        }
    }
    fn server(&self)->bool {!matches!(self,Self::Frame(..))}
}
enum Outcome {Append(Receipt),Copy(Vec<u8>)}
struct Done {job:String,server:bool,result:anyhow::Result<Outcome>,answer:Option<Answer>}
type Io=futures::future::BoxFuture<'static,Done>;
type Recovery=futures::future::BoxFuture<'static,anyhow::Result<Vec<Operation>>>;

/// One existing retained Logs owner. At most8 I/O futures,8 admitted server effects and64
/// exact network grants. The global NetEvent FIFO and per-job ordering remain unchanged.
/// Futures own their original request across caller/Hub cancellation, not a spawned task.
#[derive(Default)]
pub(super) struct Owner {
    context:Option<Context>,io:futures::stream::FuturesUnordered<Io>,
    jobs:std::collections::HashSet<String>,waiting:std::collections::VecDeque<Observed>,
    // At most one End marker per already-owned/queued job; no terminal backlog is admitted.
    ends:std::collections::HashSet<String>,
    server:usize,recovery:Option<Recovery>,ready:bool,wake:Option<std::task::Waker>,
}
impl Owner {
    pub(super) fn bind(&mut self,context:Context) {if self.context.is_none(){self.context=Some(context)}}
    pub(super) fn bound(&self)->bool {self.context.is_some()}
    fn occupied(&self,job:&str)->bool {self.jobs.contains(job) || self.waiting.iter().any(|o|o.job==job)}
    fn busy(&self,job:&str)->bool {self.bound() && (!self.ready || self.occupied(job))}
    pub(super) fn defer_finish(&mut self,job:&str)->bool {
        if !self.occupied(job) {return false}
        self.ends.insert(job.to_owned());true
    }
    fn queue(&mut self,observed:Observed) {
        self.waiting.push_back(observed);if let Some(wake)=self.wake.take(){wake.wake()}
    }
    fn server(&mut self,observed:Observed)->anyhow::Result<()> {
        anyhow::ensure!(self.context.is_some() && self.server<SERVER_SLOTS,"server log effect refused before bounded admission; no acceptance/projection");
        self.server+=1;self.queue(observed);Ok(())
    }
    fn start_ready(&mut self) {
        let Some(ctx)=self.context.clone() else{return};
        while self.io.len()<IO_SLOTS {
            let Some(at)=self.waiting.iter().position(|o|!self.jobs.contains(&o.job)) else{break};
            let Some(observed)=self.waiting.remove(at) else{break};
            self.jobs.insert(observed.job.clone());let ctx=ctx.clone();
            self.io.push(Box::pin(async move {
                let Observed{job,step,input,origin,answer}=observed;let server=input.server();
                let result=match input {
                    Input::Copy(request)=>copy_append(ctx,*request).await.map(Outcome::Copy),
                    Input::Resume(id)=>{
                        // A prequeue slot lookup is not receipt custody. Recheck after
                        // registration; GC/revocation means unavailable, never a new miss.
                        let checked=ctx.store.call(move |c|{
                            let op=operation(c,&id)?.ok_or_else(||anyhow::anyhow!("original copy disappeared before retained resume admission"))?;
                            anyhow::ensure!(copy_current(c,&id)?,"original copy target is retired, replaced or revoked");Ok(op)
                        }).await;
                        match checked {Ok(op)=>finish_copy(ctx,op).await.map(Outcome::Copy),Err(error)=>Err(error)}
                    },
                    input=>{
                        // Only active frame/server append inputs allocate one CHUNK vector.
                        // Waiting frames retain their original whole charged Reservation.
                        let prepared=crate::rt::blocking(move ||{let bytes=input.bytes()?.to_vec();Ok::<_,anyhow::Error>((bytes,input))}).await;
                        match prepared {
                            Ok(Ok((bytes,input)))=>{let result=append(ctx,job.clone(),step,bytes,origin).await.map(Outcome::Append);drop(input);result},
                            Ok(Err(error))|Err(error)=>Err(error),
                        }
                    }
                };
                Done{job,server,result,answer}
            }));
        }
    }
    fn poll(&mut self,cx:&mut std::task::Context<'_>)->std::task::Poll<anyhow::Result<Receipt>> {
        use futures::{FutureExt as _,StreamExt as _};
        self.wake=Some(cx.waker().clone());let Some(ctx)=self.context.clone()else{return std::task::Poll::Pending};
        if !self.ready {
            if self.recovery.is_none() {
                let store=ctx.store.clone();self.recovery=Some(Box::pin(async move{
                    crate::rt::sleep(std::time::Duration::from_millis(250)).await;
                    store.call(|c|{
                        let ids=c.prepare("SELECT operation FROM ci_log_accepted_ops WHERE status='pending' ORDER BY operation LIMIT 9")?.query_map([],|r|r.get::<_,String>(0))?.collect::<crate::sql::Result<Vec<_>>>()?;
                        anyhow::ensure!(ids.len()<=IO_SLOTS,"pending accepted-prefix authority exceeds finite owner capacity; log admission fenced");
                        ids.iter().map(|id|operation(c,id)?.ok_or_else(||anyhow::anyhow!("pending original operation disappeared"))).collect()
                    }).await
                }));
            }
            let Some(recovery)=self.recovery.as_mut()else{return std::task::Poll::Pending};
            match recovery.poll_unpin(cx) {
                std::task::Poll::Pending=>return std::task::Poll::Pending,
                std::task::Poll::Ready(Err(error))=>{self.recovery=None;return std::task::Poll::Ready(Err(error))},
                std::task::Poll::Ready(Ok(operations))=>{
                    self.recovery=None;self.ready=true;
                    for op in operations {
                        self.jobs.insert(op.job.clone());let ctx=ctx.clone();
                        self.io.push(Box::pin(async move{let job=op.job.clone();let result=retained_operation(ctx,op).await.map(Outcome::Append);Done{job,server:false,result,answer:None}}));
                    }
                }
            }
        }
        self.start_ready();
        match self.io.poll_next_unpin(cx) {
            std::task::Poll::Ready(Some(done))=>{
                if matches!(&done.result,Ok(Outcome::Append(receipt)) if !receipt.accepted) || done.result.is_err() {crate::ci::logsearch::kick();}
                self.jobs.remove(&done.job);if done.server{self.server=self.server.saturating_sub(1)}self.start_ready();
                if !self.occupied(&done.job) && self.ends.remove(&done.job) {ctx.files.finish(&done.job)}
                match (done.result,done.answer) {
                    (Ok(Outcome::Append(receipt)),Some(Answer::Append(answer)))=>{let _=answer.send(Ok(receipt.clone()));std::task::Poll::Ready(Ok(receipt))},
                    (Ok(Outcome::Append(receipt)),None)=>std::task::Poll::Ready(Ok(receipt)),
                    (Ok(Outcome::Copy(bytes)),Some(Answer::Copy(answer)))=>{let _=answer.send(Ok(bytes));cx.waker().wake_by_ref();std::task::Poll::Pending},
                    (Err(error),Some(Answer::Append(answer)))=>{let _=answer.send(Err(format!("{error:#}")));std::task::Poll::Ready(Err(error))},
                    (Err(error),Some(Answer::Copy(answer)))=>{let _=answer.send(Err(format!("{error:#}")));std::task::Poll::Ready(Err(error))},
                    (Err(error),None)=>std::task::Poll::Ready(Err(error)),
                    _=>std::task::Poll::Ready(Err(anyhow::anyhow!("retained log result/reply type mismatch"))),
                }
            }
            _=>std::task::Poll::Pending,
        }
    }
}

impl super::Logs {
    pub(super) fn accepted_context(&self)->anyhow::Result<Context> {self.io.lock().context.clone().ok_or_else(||anyhow::anyhow!("accepted Logs owner is unbound"))}
    pub(crate) fn observe_frame(&self,observed:Observed) {self.io.lock().queue(observed)}
    pub(crate) fn observe_server(&self,job:String,step:u32,input:Vec<u8>)->anyhow::Result<()> {
        if input.is_empty(){return Ok(())}anyhow::ensure!(input.len()<=CHUNK,"server log observation exceeds its bound");
        self.io.lock().server(Observed{job,step,input:Input::Server(input),origin:"server-observed".into(),answer:None})
    }
    async fn own_reply<T>(&self,mut reply:futures::channel::oneshot::Receiver<Result<T,String>>)->anyhow::Result<T> {
        use futures::FutureExt as _;
        futures::future::poll_fn(|cx|{
            if let std::task::Poll::Ready(result)=reply.poll_unpin(cx){return std::task::Poll::Ready(result.map_err(anyhow::Error::from).and_then(|r|r.map_err(anyhow::Error::msg)))}
            // Drive the SAME retained owner, including when the original Hub poll was cancelled.
            if let std::task::Poll::Ready(result)=self.io.lock().poll(cx){
                if let Err(error)=result {tracing::warn!(error=%format!("{error:#}"),"retained Logs completion remains qualified");}
                // FuturesUnordered may already have another ready item without a fresh
                // wake after returning this unrelated successful append completion.
                cx.waker().wake_by_ref();
            }
            match reply.poll_unpin(cx){std::task::Poll::Ready(result)=>std::task::Poll::Ready(result.map_err(anyhow::Error::from).and_then(|r|r.map_err(anyhow::Error::msg))),std::task::Poll::Pending=>std::task::Poll::Pending}
        }).await
    }
    pub(crate) async fn append_server(&self,job:String,step:u32,input:Vec<u8>)->anyhow::Result<Receipt> {
        anyhow::ensure!(!input.is_empty() && input.len()<=CHUNK,"server append exceeds its bounded nonempty admission");
        let (answer,reply)=futures::channel::oneshot::channel();
        self.io.lock().server(Observed{job,step,input:Input::Server(input),origin:"server-observed".into(),answer:Some(Answer::Append(answer))})?;
        let receipt=self.own_reply(reply).await?;anyhow::ensure!(receipt.accepted,"server append has an own refusal: {:?}",receipt.refusal);Ok(receipt)
    }
    pub(crate) async fn copy_step(&self,request:CopyStep)->anyhow::Result<Vec<u8>> {
        anyhow::ensure!(request.result.len()<=RESULT_MAX,"SavedHit exceeds16MiB before copy admission");
        let (answer,reply)=futures::channel::oneshot::channel();let job=request.job.clone();let step=request.step;
        self.io.lock().server(Observed{job,step,input:Input::Copy(Box::new(request)),origin:"server-memo-copy".into(),answer:Some(Answer::Copy(answer))})?;
        self.own_reply(reply).await
    }
    pub(crate) async fn resume_copy(&self,job:&str,step:u32,target_attempt:u32,effect:&str)->anyhow::Result<Option<Vec<u8>>> {
        let ctx=self.io.lock().context.clone().ok_or_else(||anyhow::anyhow!("accepted Logs owner is unbound"))?;
        let j=job.to_owned();let e=effect.to_owned();
        let id=ctx.store.call(move |c|{let (id,_)=effect_id(c,&j,step,target_attempt,&e)?;Ok(operation(c,&id)?.map(|_|id))}).await?;
        let Some(id)=id else{return Ok(None)};
        let (answer,reply)=futures::channel::oneshot::channel();
        self.io.lock().server(Observed{job:job.to_owned(),step,input:Input::Resume(id),origin:"server-memo-copy".into(),answer:Some(Answer::Copy(answer))})?;
        Ok(Some(self.own_reply(reply).await?))
    }
    pub(crate) async fn accepted_completion(&self)->anyhow::Result<Receipt> {futures::future::poll_fn(|cx|self.io.lock().poll(cx)).await}
}
