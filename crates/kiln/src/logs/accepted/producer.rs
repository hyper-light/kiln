//! HELD original-producer ledger in the SAME accepted Logs/Amphora owner.
//! Registration is an authenticated claim transaction, not a frame-side convenience.
//! Producer closure and the captured historical job-view prefix remain distinct proofs.

use super::*;
use crate::proto::job_log;
use kiln_ci::wire::{JobLogBindingV1,JobLogChunkV1,JobLogEndV1,JobLogCaptureV1,
    JobLogPrefixV1,JobLogReceiptV1,JobLogReplyV1,JobLogRefusalV1,JobLogViewV1};

const BODY_MAX:usize=96<<10;
const BINDING_MAX:usize=8192;
const STEPS:usize=4096;

fn number(n:u64)->String {format!("{n:016x}")}
fn unnumber(n:&str)->anyhow::Result<u64> {
    anyhow::ensure!(n.len()==16 && n.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),"noncanonical producer sequence");
    Ok(u64::from_str_radix(n,16)?)
}
fn digest(n:&str)->anyhow::Result<[u8;32]> {
    let mut out=[0;32];hex::decode_to_slice(n,&mut out)?;anyhow::ensure!(hex::encode(out)==n,"noncanonical producer digest");Ok(out)
}
fn whole<T:serde::de::DeserializeOwned>(bytes:&[u8])->anyhow::Result<T> {
    let (value,rest)=postcard::take_from_bytes(bytes)?;
    anyhow::ensure!(rest.is_empty(),"producer metadata/frame has trailing bytes");Ok(value)
}

#[derive(Clone)]
struct Registered {
    id:String,binding:JobLogBindingV1,order:i64,view:String,through:u64,bytes:u64,root:[u8;32],
    state:String,result:Option<Vec<u8>>,result_hash:Option<String>,close:Option<String>,steps:usize,active:bool,received:bool,
}
fn get(c:&Connection,id:&str)->anyhow::Result<Option<Registered>> {
    let row=c.query_row("SELECT descriptor,job_order,view,through,bytes,root,state,result,result_hash,close_operation,steps,active,received FROM ci_log_producers WHERE binding=?1",[id],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get(1)?,r.get(2)?,r.get::<_,String>(3)?,r.get(4)?,r.get::<_,String>(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?,r.get(12)?))).optional()?;
    row.map(|(body,order,view,through,bytes,root,state,result,result_hash,close,steps,active,received)|{
        anyhow::ensure!(body.len()<=BINDING_MAX,"registered binding exceeds its bound");
        let binding:JobLogBindingV1=whole(&body)?;job_log::validate_binding(&binding)?;
        anyhow::ensure!(hex::encode(job_log::binding_id(&binding)?)==id,"registered binding identity conflicts");
        Ok(Registered{id:id.into(),binding,order,view,through:unnumber(&through)?,bytes,root:digest(&root)?,state,result,result_hash,close,steps,active,received})
    }).transpose()
}

/// The original ordinary journal, not the current connection or latest attempt, grants
/// registration. A positive physical disposition is allowed: it does not close logs.
fn current(c:&Connection,r:&Registered)->anyhow::Result<bool> {
    if !r.active {return Ok(false)}
    let owner=&r.binding.owner;
    let live:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM jobs j JOIN runs n ON n.id=j.run_id JOIN ci_log_accepted_views v ON v.job=j.id WHERE j.id=?1 AND j.rowid=?2 AND j.attempt=?3 AND n.provider='kiln' AND v.view=?4 AND v.job_order=?2 AND v.retired=0)",params![owner.job,r.order,owner.attempt,r.view],|r|r.get(0))?;
    if !live {return Ok(false)}
    let Some((original,contract))=crate::ci::dispatch::ordinary_log_registration(c,&owner.job,owner.attempt)? else{return Ok(false)};
    Ok(original==*owner && contract.as_ref()==Some(&r.binding.contract))
}

/// Called INSIDE the original ordinary claim transaction, after its journal INSERT.
/// The caller commits once and observes that own durable callback before exposure.
pub(crate) fn register(c:&Connection,binding:&JobLogBindingV1,job_order:i64)->anyhow::Result<()> {
    job_log::validate_binding(binding)?;let id=hex::encode(job_log::binding_id(binding)?);
    let body=crate::proto::encode(binding)?;anyhow::ensure!(body.len()<=BINDING_MAX,"producer binding exceeds its bound");
    if let Some(old)=get(c,&id)? {
        anyhow::ensure!(old.binding==*binding && old.order==job_order && current(c,&old)?,"original producer registration changed");return Ok(())
    }
    let job=&binding.owner.job;
    let same:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM jobs j JOIN runs r ON r.id=j.run_id WHERE j.id=?1 AND j.rowid=?2 AND j.attempt=?3 AND r.provider='kiln')",params![job,job_order,binding.owner.attempt],|r|r.get(0))?;
    anyhow::ensure!(same,"producer registration lost its original job row/attempt");
    let prior:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producers WHERE job=?1 AND job_order=?2 AND attempt=?3)",params![job,job_order,binding.owner.attempt],|r|r.get(0))?;
    if prior {
        let fenced:Option<bool>=c.query_row("SELECT fenced FROM ci_log_producer_reuse WHERE job=?1 AND job_order=?2 AND attempt=?3",params![job,job_order,binding.owner.attempt],|r|r.get(0)).optional()?;
        anyhow::ensure!(fenced==Some(false),"NoStart producer has received/conflicting or undisposed intent; same-attempt reuse remains fenced");
    }
    let open:usize=c.prepare("SELECT binding FROM ci_log_producers WHERE agent=?1 AND state='open' ORDER BY binding LIMIT 65")?.query_map([&binding.owner.host],|r|r.get::<_,String>(0))?.collect::<crate::sql::Result<Vec<_>>>()?.len();
    anyhow::ensure!(open<64,"original host producer bindings are full before exposure");
    // An unpublished rev0 view is not a Prefix or accepted empty history. It gives all
    // pending descriptors the existing retirement/tombstone owner, even before first input.
    c.execute("INSERT INTO ci_log_accepted_views(job,view,job_order,root,qualification) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(job) DO NOTHING",params![job,crate::util::new_id(),job_order,EMPTY,QUALIFICATION])?;
    let (view,order,retired):(String,i64,bool)=c.query_row("SELECT view,job_order,retired FROM ci_log_accepted_views WHERE job=?1",[job],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    anyhow::ensure!(order==job_order && !retired,"producer registration cannot adopt a retired/replaced view");
    let r=Registered{id:id.clone(),binding:binding.clone(),order,view:view.clone(),through:0,bytes:0,root:job_log::initial_root(&job_log::binding_id(binding)?),state:"open".into(),result:None,result_hash:None,close:None,steps:0,active:true,received:false};
    anyhow::ensure!(current(c,&r)?,"producer registration requires the exact durable ordinary journal owner");
    c.execute("INSERT INTO ci_log_producers(binding,descriptor,job,job_order,attempt,agent,view,capture_limit,through,root) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![id,body,job,order,binding.owner.attempt,binding.owner.host,view,binding.contract.capture_limit,number(0),hex::encode(r.root)])?;Ok(())
}

/// Bounded lookup for the ORIGINAL result staging transaction. No contract means legacy;
/// a malformed/stale registered contract is an error, never a miss or newly stamped owner.
pub(crate) fn registered(c:&Connection,job:&str,attempt:u32,agent:&str)->anyhow::Result<Option<JobLogBindingV1>> {
    let id:Option<String>=c.query_row("SELECT binding FROM ci_log_producers WHERE job=?1 AND attempt=?2 AND agent=?3 AND active=1",params![job,attempt,agent],|r|r.get(0)).optional()?;
    let Some(id)=id else{
        if let Some((owner,contract))=crate::ci::dispatch::ordinary_log_registration(c,job,attempt)? {
            anyhow::ensure!(owner.host==agent,"result reporter differs from the original ordinary owner");
            anyhow::ensure!(contract.is_none() && !requires_contract(c,job,attempt)?,"declared/retained required producer registration is missing");return Ok(None)
        }
        let disposed:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producers WHERE job=?1 AND attempt=?2)",params![job,attempt],|r|r.get(0))?;
        anyhow::ensure!(!disposed,"original required producer was disposed; never a legacy miss");return Ok(None)
    };let r=get(c,&id)?.ok_or_else(||anyhow::anyhow!("registered producer disappeared"))?;
    Ok(Some(r.binding))
}

/// Required mode cannot silently downgrade on a same-incarnation/attempt NoStart reuse.
/// This read is performed by the existing ordinary claim owner before any new effect.
pub(crate) fn requires_contract(c:&Connection,job:&str,attempt:u32)->anyhow::Result<bool> {
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producers p JOIN jobs j ON j.id=p.job AND j.rowid=p.job_order WHERE p.job=?1 AND p.attempt=?2)",params![job,attempt],|r|r.get(0))?)
}

/// Exact positive NoStart is read from the SAME original journal after its update in
/// the caller's transaction. All prior bytes, pins, receipts and conflicts survive.
/// A later grant may reuse this attempt only if the disposed descriptor is truly empty.
pub(crate) fn no_start(c:&Connection,owner:&kiln_ci::wire::OrdinaryOwnerV1)->anyhow::Result<()> {
    anyhow::ensure!(crate::ci::dispatch::ordinary_log_no_start(c,owner)?,"producer NoStart requires the exact positive original ordinary disposition");
    let id:Option<String>=c.query_row("SELECT binding FROM ci_log_producers WHERE job=?1 AND attempt=?2 AND active=1",params![owner.job,owner.attempt],|r|r.get(0)).optional()?;
    let Some(id)=id else{return Ok(())};let r=get(c,&id)?.ok_or_else(||anyhow::anyhow!("original NoStart producer disappeared"))?;
    anyhow::ensure!(r.binding.owner==*owner,"NoStart cannot dispose a different original producer");
    let received:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producer_items WHERE binding=?1)",[&id],|r|r.get(0))?;
    let fenced=received || r.received || r.result.is_some() || r.through!=0;
    c.execute("INSERT INTO ci_log_producer_reuse(job,job_order,attempt,view,fenced) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(job,job_order,attempt) DO UPDATE SET fenced=CASE WHEN ci_log_producer_reuse.fenced=1 THEN 1 ELSE excluded.fenced END",params![owner.job,r.order,owner.attempt,r.view,fenced])?;
    c.execute("UPDATE ci_log_producers SET active=0,end_ready=0,disposition='no_start',state=CASE WHEN ?2=0 THEN 'disposed' ELSE state END WHERE binding=?1 AND active=1",params![id,fenced])?;Ok(())
}

#[derive(Debug,Clone,PartialEq,Eq)]
pub(crate) enum Close {Pending,Refused(String),Closed(JobLogReceiptV1)}

/// First exact serialized JobResult belongs to the existing completion staging tx.
/// No effect or positive End receipt is inferred from this separate control message.
pub(crate) fn note_result(c:&Connection,binding:&JobLogBindingV1,result:&[u8])->anyhow::Result<Close> {
    let id=hex::encode(job_log::binding_id(binding)?);let r=get(c,&id)?.ok_or_else(||anyhow::anyhow!("required producer was not registered"))?;
    anyhow::ensure!(r.binding==*binding,"original result binding changed");
    let digest=hex::encode(job_log::result_hash(result)?);let value:crate::proto::JobResult=whole(result)?;
    anyhow::ensure!(value.job==binding.owner.job,"producer result belongs to another job");
    if let Some(old)=&r.result {if old!=result {return Ok(Close::Refused("first original JobResult bytes conflict".into()))}}
    else {
        anyhow::ensure!(current(c,&r)?,"new original result requires its still-current ordinary owner");
        c.execute("UPDATE ci_log_producers SET result=?2,result_hash=?3 WHERE binding=?1 AND result IS NULL",params![id,result,digest])?;
    }
    ready_end(c,&id)?;
    close(c,binding,result)
}
pub(crate) fn close(c:&Connection,binding:&JobLogBindingV1,result:&[u8])->anyhow::Result<Close> {
    let id=hex::encode(job_log::binding_id(binding)?);let r=get(c,&id)?.ok_or_else(||anyhow::anyhow!("required producer registration is missing"))?;
    anyhow::ensure!(r.binding==*binding,"required close changed its immutable original registration");
    let hash=hex::encode(job_log::result_hash(result)?);
    if r.result.as_deref()!=Some(result) || r.result_hash.as_deref()!=Some(hash.as_str()) {return Ok(Close::Refused("first result bytes are missing or conflict".into()))}
    let Some(operation)=r.close else{return Ok(Close::Pending)};
    let receipt=own_receipt(c,&operation)?.ok_or_else(||anyhow::anyhow!("closed producer lacks its own close receipt"))?;
    anyhow::ensure!(r.state=="closed" && receipt.result_hash==Some(digest(&hash)?),"producer close marker conflicts");
    match receipt.view {JobLogViewV1::Captured(_)=>Ok(Close::Closed(receipt)),JobLogViewV1::ProducerOnly=>Ok(Close::Pending)}
}

/// Exact original bytes, never a reconstruction of the completion report's JSON.
pub(crate) fn stored_result(c:&Connection,binding:&JobLogBindingV1)->anyhow::Result<Option<Vec<u8>>> {
    let id=hex::encode(job_log::binding_id(binding)?);let r=get(c,&id)?.ok_or_else(||anyhow::anyhow!("required producer registration is missing"))?;
    anyhow::ensure!(r.binding==*binding,"stored result changed its immutable original registration");
    if let Some(bytes)=&r.result {
        anyhow::ensure!(r.result_hash.as_deref()==Some(hex::encode(job_log::result_hash(bytes)?).as_str()),"stored original result hash conflicts");
    }Ok(r.result)
}

#[derive(Clone)]
pub(super) struct Original {binding:String,item:String}

#[derive(Clone)]
enum Item {Chunk(JobLogChunkV1),End(JobLogEndV1)}
impl Item {
    fn binding(&self)->&JobLogBindingV1 {match self {Self::Chunk(v)=>&v.binding,Self::End(v)=>&v.binding}}
    fn seq(&self)->anyhow::Result<u64> {match self {Self::Chunk(v)=>Ok(v.seq),Self::End(v)=>job_log::end_item_seq(v)}}
    fn bytes(&self)->&[u8] {match self {Self::Chunk(v)=>&v.data,Self::End(_)=>&[]}}
    fn step(&self)->u32 {match self {Self::Chunk(v)=>v.step,Self::End(_)=>0}}
}

/// Publication needs only bounded ledger metadata. It never fabricates raw chunk bytes
/// or allocates a placeholder input under the shared database writer.
#[derive(Clone)]
enum LedgerItem {
    Chunk {seq:u64,step:u32,offset:u64,bytes:u64,previous_root:[u8;32],root:[u8;32]},
    End(Box<JobLogEndV1>),
}
impl LedgerItem {
    fn from_item(item:&Item)->anyhow::Result<Self> {Ok(match item {
        Item::Chunk(v)=>Self::Chunk{seq:v.seq,step:v.step,offset:v.offset,bytes:u64::try_from(v.data.len())?,previous_root:v.previous_root,root:job_log::chunk_root(v)?},
        Item::End(v)=>Self::End(Box::new(v.clone())),
    })}
    fn seq(&self)->anyhow::Result<u64> {match self{Self::Chunk{seq,..}=>Ok(*seq),Self::End(v)=>job_log::end_item_seq(v)}}
}
fn decode(binding:&JobLogBindingV1,body:&[u8])->anyhow::Result<Item> {
    anyhow::ensure!(!body.is_empty() && body.len()<=BODY_MAX,"producer Item body exceeds its bound");
    let message:crate::proto::AgentMsg=whole(body)?;
    anyhow::ensure!(crate::proto::encode(&message)?==body,"producer message is not its canonical original encoding");
    let item=match message {
        crate::proto::AgentMsg::JobLogChunkV1(v)=>{job_log::validate_chunk(&v)?;Item::Chunk(v)},
        crate::proto::AgentMsg::JobLogEndV1(v)=>{job_log::validate_end(&v)?;Item::End(v)},
        _=>anyhow::bail!("negotiated job-log Item has another message kind"),
    };
    anyhow::ensure!(item.binding()==binding,"producer body changed its registered full binding");Ok(item)
}
fn refused(binding:[u8;32],seq:u64,code:JobLogRefusalV1,why:&str)->JobLogReplyV1 {
    JobLogReplyV1::Refused{binding_id:binding,item_seq:seq,code,why:crate::util::truncate(why,4096).into()}
}
fn unknown(binding:[u8;32],seq:u64,why:&str)->JobLogReplyV1 {
    JobLogReplyV1::Unknown{binding_id:binding,item_seq:seq,operation:job_log::stable_operation(&binding,seq).ok(),why:crate::util::truncate(why,4096).into()}
}

fn own_receipt(c:&Connection,id:&str)->anyhow::Result<Option<JobLogReceiptV1>> {
    let row:Option<(String,String,String,String,Vec<u8>)>=c.query_row("SELECT binding,item,kind,root,receipt FROM ci_log_producer_items WHERE operation=?1 AND receipt IS NOT NULL",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((binding,item,kind,item_root,body))=row else{return Ok(None)};
    anyhow::ensure!(body.len()<=8192,"producer receipt exceeds its bound");let receipt:JobLogReceiptV1=whole(&body)?;
    anyhow::ensure!(receipt.operation==id && hex::encode(receipt.binding_id)==binding,"own producer receipt identity conflicts");
    anyhow::ensure!(number(receipt.item_seq)==item && job_log::stable_operation(&receipt.binding_id,receipt.item_seq)?==id && hex::encode(receipt.root)==item_root,"own producer item/operation/root receipt conflicts");
    let registration=get(c,&binding)?.ok_or_else(||anyhow::anyhow!("own producer receipt lost its immutable original registration"))?;
    if kind=="end" {
        anyhow::ensure!(registration.state=="closed" && registration.close.as_deref()==Some(id) && registration.through==receipt.chunk_seq && registration.bytes==receipt.captured_bytes && registration.root==receipt.root && receipt.result_hash.is_some() && receipt.item_seq==receipt.chunk_seq.checked_add(1).ok_or_else(||anyhow::anyhow!("End receipt sequence exhausted"))?,"own End receipt lacks its exact durable closed cursor");
    }else{
        anyhow::ensure!(receipt.item_seq==receipt.chunk_seq && receipt.result_hash.is_none() && registration.through>=receipt.chunk_seq && registration.bytes>=receipt.captured_bytes,"own chunk receipt lacks its exact durable cursor");
    }
    let captured=match &receipt.view {JobLogViewV1::Captured(p)=>p,JobLogViewV1::ProducerOnly=>anyhow::bail!("positive full-view receipt has no captured prefix")};
    checked_prefix(c,&Prefix{view:captured.view.clone(),revision:captured.revision,root:captured.root.clone()})?;
    if let Some(op)=operation(c,id)? {
        let accepted=completed(c,id)?.ok_or_else(||anyhow::anyhow!("producer append lacks its own receipt"))?;
        anyhow::ensure!(accepted.accepted && op.view==captured.view && accepted.prefix.revision==captured.revision && accepted.prefix.root==captured.root,"producer and append own receipts conflict");
    }else{anyhow::ensure!(kind=="end" && receipt.chunk_seq>0,"original chunk/End0 append receipt disappeared");}
    Ok(Some(receipt))
}

/// The existing accepted-operation refusal is already a durable original disposition,
/// not Unknown. Validate its exact refusal prefix and original producer identity before
/// replay. All terminal accepted-prefix refusals use Incomplete consistently, including
/// the first live reply; no typed reason is guessed from mutable status/error text.
fn terminal_reply(c:&Connection,id:&str)->anyhow::Result<Option<JobLogReplyV1>> {
    if let Some(receipt)=own_receipt(c,id)? {return Ok(Some(JobLogReplyV1::Committed(receipt)))}
    let Some(done)=completed(c,id)? else{return Ok(None)};
    anyhow::ensure!(!done.accepted,"positive append lacks its own producer receipt");
    let row:Option<(String,String)>=c.query_row("SELECT binding,item FROM ci_log_producer_items WHERE operation=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((binding,item))=row else{return Ok(None)};
    let Some(original)=get(c,&binding)? else{return Ok(None)};
    let binding_id=digest(&binding)?;let seq=unnumber(&item)?;
    anyhow::ensure!(job_log::stable_operation(&binding_id,seq)?==id && done.operation==id && done.job==original.binding.owner.job && done.prefix.view==original.view,"original negative producer receipt identity conflicts");
    Ok(Some(refused(binding_id,seq,JobLogRefusalV1::Incomplete,done.refusal.as_deref().unwrap_or("original accepted-prefix operation refused"))))
}
async fn terminal_reply_owned(ctx:&Context,id:String)->JobLogReplyV1 {reconcile(&ctx.store,move |c|terminal_reply(c,&id)).await}

enum Admission {Ready(Box<super::Original>),Receipt(JobLogReceiptV1),Pending,Refused(JobLogRefusalV1,String)}
fn decided(tx:crate::store::Tx<'_, '_>,decision:Admission)->anyhow::Result<Admission> {tx.commit()?;Ok(decision)}
#[derive(Clone)]
struct Prepared {item:Item,ledger:LedgerItem,body:Vec<u8>,body_hash:String,input_hash:String,capture:Option<Vec<u8>>}
fn prepare_item(binding:&JobLogBindingV1,body:Vec<u8>)->anyhow::Result<Prepared> {
    let item=decode(binding,&body)?;let ledger=LedgerItem::from_item(&item)?;
    let capture=if let Item::End(end)=&item {Some(crate::proto::encode(&end.capture)?)}else{None};
    Ok(Prepared{body_hash:super::hash(&body),input_hash:super::hash(item.bytes()),item,ledger,body,capture})
}
fn dependency(c:&Connection,r:&Registered,item:&LedgerItem)->anyhow::Result<Option<String>> {
    match item {
        LedgerItem::Chunk{seq,step,offset,bytes,previous_root,..}=>{
            if r.state!="open" {return Ok(Some("producer is already closed".into()))}
            if *seq!=r.through.checked_add(1).ok_or_else(||anyhow::anyhow!("producer cursor exhausted"))? {return Ok(Some("predecessor chunk has no own receipt".into()))}
            let expected:u64=c.query_row("SELECT bytes FROM ci_log_producer_steps WHERE binding=?1 AND step=?2",params![r.id,step],|r|r.get(0)).optional()?.unwrap_or(0);
            if *previous_root!=r.root || *offset!=expected {return Ok(Some("original producer root/step offset conflicts".into()))}
            anyhow::ensure!(r.bytes.checked_add(*bytes).is_some_and(|n|n<=r.binding.contract.capture_limit),"producer exceeds its negotiated capture limit");
        },
        LedgerItem::End(v)=>{
            if v.final_seq!=r.through || v.captured_bytes!=r.bytes || v.root!=r.root {return Ok(Some("producer End awaits its exact predecessor cursor".into()))}
            if r.result.as_deref()!=Some(v.result.as_slice()) || r.result_hash.as_deref()!=Some(hex::encode(v.result_hash).as_str()) {return Ok(Some("producer End awaits the first exact control result".into()))}
            if let JobLogCaptureV1::Incomplete{why,..}=&v.capture {return Ok(Some(format!("producer explicitly incomplete: {why}")))}
        },
    }Ok(None)
}

/// Constant one-End lookup. The indexed eligibility bit is only a scheduling hint;
/// replay rechecks original authority, exact predecessor/result and its own receipt.
fn ready_end(c:&Connection,binding:&str)->anyhow::Result<()> {
    let id:Option<String>=c.query_row("SELECT operation FROM ci_log_producer_items WHERE binding=?1 AND kind='end' AND receipt IS NULL AND refusal IS NULL",[binding],|r|r.get(0)).optional()?;
    let ready=if let Some(id)=id && let Some((r,item))=item_for(c,&id)? {
        r.active && r.state=="open" && dependency(c,&r,&item)?.is_none()
    }else{false};
    c.execute("UPDATE ci_log_producers SET end_ready=?2 WHERE binding=?1",params![binding,ready])?;Ok(())
}

fn admit(c:&Connection,binding:&JobLogBindingV1,prepared:&Prepared,host:&str)->anyhow::Result<Admission> {
    let (item,body)=(&prepared.item,prepared.body.as_slice());
    let binding_id=job_log::binding_id(binding)?;let id=hex::encode(binding_id);let seq=item.seq()?;let key=number(seq);let operation=job_log::stable_operation(&binding_id,seq)?;
    let tx=crate::store::tx(c)?;let Some(r)=get(&tx,&id)? else{return Ok(Admission::Refused(JobLogRefusalV1::StaleOwner,"producer was not registered by its original claim".into()))};
    if r.binding!=*binding {return Ok(Admission::Refused(JobLogRefusalV1::Conflict,"producer changed its original registration".into()))}
    let old:Option<(String,u64,Option<Vec<u8>>)>=tx.query_row("SELECT body_hash,body_bytes,end_body FROM ci_log_producer_items WHERE binding=?1 AND item=?2",params![id,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let is_new=old.is_none();
    if let Some((hash,bytes,end))=old {
        if hash!=prepared.body_hash || bytes!=u64::try_from(body.len())? || end.as_ref().is_some_and(|end|end!=body) {return Ok(Admission::Refused(JobLogRefusalV1::Conflict,"same original producer item has conflicting bytes/kind".into()))}
        if let Some(receipt)=own_receipt(&tx,&operation)? {return Ok(Admission::Receipt(receipt))}
        if let Some(JobLogReplyV1::Refused{code,why,..})=terminal_reply(&tx,&operation)? {return Ok(Admission::Refused(code,why))}
        let why:Option<String>=tx.query_row("SELECT refusal FROM ci_log_producer_items WHERE operation=?1",[&operation],|r|r.get(0))?;
        if let Some(why)=why {return Ok(Admission::Refused(JobLogRefusalV1::StaleOwner,why))}
    }
    // Immutable exact replay above survives ordinary-journal retirement. Every NEW
    // mutation below must still own the original current incarnation/attempt/journal.
    if !current(&tx,&r)? {return Ok(Admission::Refused(JobLogRefusalV1::StaleOwner,"original producer row/attempt/journal is no longer current".into()))}
    // Even a canonical gap/conflict is received intent for NoStart reuse. One constant
    // flag is durable without staging an unbounded gap-row catalog or claiming bytes.
    tx.execute("UPDATE ci_log_producers SET received=1 WHERE binding=?1 AND active=1",[&id])?;
    if is_new {
        if r.state!="open" || matches!(item,Item::Chunk(v) if v.seq<=r.through) {return decided(tx,Admission::Refused(JobLogRefusalV1::Conflict,"producer cursor has no matching original item".into()))}
        if let Item::Chunk(v)=item {
            if r.through.checked_add(1)!=Some(v.seq) {return decided(tx,Admission::Pending)}
            if v.previous_root!=r.root {return decided(tx,Admission::Refused(JobLogRefusalV1::Conflict,"producer previous root conflicts".into()))}
            let offset:u64=tx.query_row("SELECT bytes FROM ci_log_producer_steps WHERE binding=?1 AND step=?2",params![id,v.step],|r|r.get(0)).optional()?.unwrap_or(0);
            if v.offset!=offset {return decided(tx,Admission::Refused(JobLogRefusalV1::Conflict,"producer step offset conflicts".into()))}
            if offset==0 && r.steps>=STEPS {return decided(tx,Admission::Refused(JobLogRefusalV1::Full,"producer step metadata bound reached".into()))}
            if r.bytes.checked_add(u64::try_from(v.data.len())?).is_none_or(|n|n>binding.contract.capture_limit) {return decided(tx,Admission::Refused(JobLogRefusalV1::Full,"producer exceeds the original negotiated capture limit".into()))}
        }
        if let Item::End(v)=item
            && v.final_seq<=r.through && (v.final_seq!=r.through || v.captured_bytes!=r.bytes || v.root!=r.root) {return decided(tx,Admission::Refused(JobLogRefusalV1::Conflict,"producer End contradicts the original accepted cursor".into()))}
        let (kind,offset,previous,root,end)=match item {
            Item::Chunk(v)=>{
                let root=match &prepared.ledger {LedgerItem::Chunk{root,..}=>*root,_=>anyhow::bail!("prepared producer kind changed")};
                ("chunk",v.offset,v.previous_root,root,None)
            },
            Item::End(v)=>{
                let prior:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producer_items WHERE binding=?1 AND kind='end')",[&id],|r|r.get(0))?;
                if prior {return decided(tx,Admission::Refused(JobLogRefusalV1::Conflict,"producer already has a different first End".into()))}
                ("end",0,v.root,v.root,Some(body.to_vec()))
            },
        };
        let (final_seq,captured_bytes,result,result_hash)=match item {
            Item::End(end)=>(Some(number(end.final_seq)),Some(end.captured_bytes),Some(end.result.as_slice()),Some(hex::encode(end.result_hash))),
            Item::Chunk(_)=>(None,None,None,None),
        };
        tx.execute("INSERT INTO ci_log_producer_items(binding,item,operation,kind,body_hash,body_bytes,step,offset,previous_root,root,input_hash,input_bytes,end_body,final_seq,captured_bytes,result,result_hash,capture) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",params![id,key,operation,kind,prepared.body_hash,u64::try_from(body.len())?,item.step(),offset,hex::encode(previous),hex::encode(root),prepared.input_hash,u64::try_from(item.bytes().len())?,end,final_seq,captured_bytes,result,result_hash,prepared.capture])?;
    }
    if let Some(why)=dependency(&tx,&r,&prepared.ledger)? {
        ready_end(&tx,&id)?;
        let refused=matches!(item,Item::End(v) if v.final_seq<r.through || matches!(v.capture,JobLogCaptureV1::Incomplete{..}) || r.result.as_ref().is_some_and(|old|old!=&v.result));
        tx.commit()?;return Ok(if refused {Admission::Refused(JobLogRefusalV1::Incomplete,why)}else{Admission::Pending});
    }
    let original=super::Original{id:operation,view:r.view,job:binding.owner.job.clone(),step:item.step(),hash:prepared.input_hash.clone(),bytes:u64::try_from(item.bytes().len())?,origin:if matches!(item,Item::End(_)){"producer-end0"}else{"producer-chunk"}.into(),host:host.into(),copy:None,job_order:Some(r.order),attempt:Some(binding.owner.attempt),producer:Some(Original{binding:id,item:key})};
    ready_end(&tx,&original.producer.as_ref().ok_or_else(||anyhow::anyhow!("producer descriptor disappeared"))?.binding)?;
    tx.commit()?;Ok(Admission::Ready(Box::new(original)))
}

fn item_for(c:&Connection,id:&str)->anyhow::Result<Option<(Registered,LedgerItem)>> {
    let row:Option<(String,String,u64,u32,u64,String,String)>=c.query_row("SELECT binding,kind,input_bytes,step,offset,previous_root,root FROM ci_log_producer_items WHERE operation=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
    row.map(|(binding,kind,bytes,step,offset,previous,root)|{
        let r=get(c,&binding)?.ok_or_else(||anyhow::anyhow!("original producer binding disappeared"))?;
        let item=if kind=="end" {
            let (seq,bytes,result,hash,capture):(String,u64,Vec<u8>,String,Vec<u8>)=c.query_row("SELECT final_seq,captured_bytes,result,result_hash,capture FROM ci_log_producer_items WHERE operation=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
            anyhow::ensure!(result.len()<=65536 && capture.len()<=8192,"original End metadata exceeds its bound");
            LedgerItem::End(Box::new(JobLogEndV1{binding:r.binding.clone(),final_seq:unnumber(&seq)?,captured_bytes:bytes,root:digest(&root)?,result,result_hash:digest(&hash)?,capture:whole(&capture)?}))
        }else{
            let seq:String=c.query_row("SELECT item FROM ci_log_producer_items WHERE operation=?1",[id],|r|r.get(0))?;
            anyhow::ensure!(bytes<=32768,"producer chunk exceeds bound");
            LedgerItem::Chunk{seq:unnumber(&seq)?,step,offset,bytes,previous_root:digest(&previous)?,root:digest(&root)?}
        };Ok((r,item))
    }).transpose()
}

/// Rechecked within the existing reservation transaction, before any new immutable key.
pub(super) fn reserve_current(c:&Connection,o:&Original)->anyhow::Result<bool> {
    let r=get(c,&o.binding)?.ok_or_else(||anyhow::anyhow!("original producer registration disappeared"))?;
    let id:Option<String>=c.query_row("SELECT operation FROM ci_log_producer_items WHERE binding=?1 AND item=?2 AND refusal IS NULL",params![o.binding,o.item],|r|r.get(0)).optional()?;
    Ok(current(c,&r)? && id.is_some())
}
pub(super) fn is_end0(c:&Connection,id:&str)->anyhow::Result<bool> {
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producer_items WHERE operation=?1 AND kind='end' AND input_bytes=0)",[id],|r|r.get(0))?)
}
pub(super) fn publication_current(c:&Connection,id:&str)->anyhow::Result<bool> {
    let Some((r,item))=item_for(c,id)? else{
        let Some(op)=operation(c,id)? else{return Ok(true)};
        if !op.origin.starts_with("legacy-job-view/net/") {return Ok(true)}
        let required:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producers p JOIN jobs j ON j.id=p.job WHERE p.job=?1 AND p.job_order=j.rowid AND p.attempt=j.attempt)",[op.job],|r|r.get(0))?;
        return Ok(!required)
    };
    let op=operation(c,id)?.ok_or_else(||anyhow::anyhow!("original producer append operation disappeared"))?;
    let (input_hash,input_bytes):(String,u64)=c.query_row("SELECT input_hash,input_bytes FROM ci_log_producer_items WHERE operation=?1",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    Ok(current(c,&r)? && dependency(c,&r,&item)?.is_none() && op.job==r.binding.owner.job && op.view==r.view && op.input_hash==input_hash && op.input_bytes==input_bytes)
}
fn receipt_in(c:&Connection,r:&Registered,item:&LedgerItem,id:&str,prefix:Prefix)->anyhow::Result<JobLogReceiptV1> {
    let (chunk_seq,bytes,root,result_hash)=match item {
        LedgerItem::Chunk{seq,bytes,root,..}=>{
            (*seq,r.bytes.checked_add(*bytes).ok_or_else(||anyhow::anyhow!("producer byte overflow"))?,*root,None)
        },
        LedgerItem::End(v)=>(v.final_seq,v.captured_bytes,v.root,Some(v.result_hash)),
    };
    let receipt=JobLogReceiptV1{binding_id:job_log::binding_id(&r.binding)?,item_seq:item.seq()?,chunk_seq,captured_bytes:bytes,root,operation:id.into(),view:JobLogViewV1::Captured(JobLogPrefixV1{view:prefix.view,revision:prefix.revision,root:prefix.root}),result_hash};
    let body=crate::proto::encode(&receipt)?;anyhow::ensure!(body.len()<=8192,"producer receipt exceeds its bound");
    let changed=c.execute("UPDATE ci_log_producer_items SET receipt=?2 WHERE operation=?1 AND receipt IS NULL",params![id,body])?;
    anyhow::ensure!(changed==1,"original producer receipt CAS changed");
    match item {
        LedgerItem::Chunk{seq,step,offset,bytes:input_bytes,..}=>{
            let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producer_steps WHERE binding=?1 AND step=?2)",params![r.id,step],|r|r.get(0))?;
            let steps=r.steps.checked_add(usize::from(!exists)).ok_or_else(||anyhow::anyhow!("producer step counter exhausted"))?;
            anyhow::ensure!(steps<=STEPS,"producer step metadata bound reached");
            let changed=c.execute("UPDATE ci_log_producers SET through=?2,bytes=?3,root=?4,steps=?7 WHERE binding=?1 AND through=?5 AND root=?6 AND state='open'",params![r.id,number(*seq),bytes,hex::encode(root),number(r.through),hex::encode(r.root),steps])?;
            anyhow::ensure!(changed==1,"original producer cursor CAS changed");
            c.execute("INSERT INTO ci_log_producer_steps(binding,step,bytes) VALUES(?1,?2,?3) ON CONFLICT(binding,step) DO UPDATE SET bytes=excluded.bytes",params![r.id,step,offset.checked_add(*input_bytes).ok_or_else(||anyhow::anyhow!("step offset overflow"))?])?;
        },
        LedgerItem::End(_)=>{let changed=c.execute("UPDATE ci_log_producers SET state='closed',close_operation=?2 WHERE binding=?1 AND state='open'",params![r.id,id])?;anyhow::ensure!(changed==1,"original End close CAS changed");},
    }ready_end(c,&r.id)?;Ok(receipt)
}
/// SAME transaction as checked accepted-head publication, never a later inferred ACK.
pub(super) fn published(c:&Connection,id:&str,prefix:Prefix)->anyhow::Result<()> {
    if let Some((r,item))=item_for(c,id)? {
        anyhow::ensure!(current(c,&r)? && dependency(c,&r,&item)?.is_none(),"producer authority changed before own publication");
        receipt_in(c,&r,&item,id,prefix)?;
    }Ok(())
}

fn seal(c:&Connection,id:&str)->anyhow::Result<Option<JobLogReceiptV1>> {
    let tx=crate::store::tx(c)?;if let Some(receipt)=own_receipt(&tx,id)? {return Ok(Some(receipt))}
    let refused:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producer_items WHERE operation=?1 AND refusal IS NOT NULL)",[id],|r|r.get(0))?;
    anyhow::ensure!(!refused,"original producer End already has its own refusal");
    let (r,item)=item_for(&tx,id)?.ok_or_else(||anyhow::anyhow!("original End operation disappeared"))?;
    anyhow::ensure!(current(&tx,&r)?,"original End owner ceased to be current");
    if dependency(&tx,&r,&item)?.is_some() {return Ok(None)}
    let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_ops WHERE view=?1 AND status='pending')",[&r.view],|r|r.get(0))?;
    if busy {return Ok(None)}
    let Some(prefix)=prefix(&tx,&r.binding.owner.job)? else{return Ok(None)};
    let receipt=receipt_in(&tx,&r,&item,id,prefix)?;tx.commit()?;Ok(Some(receipt))
}

/// Reuse the existing exact conditional reconciliation helper after a reservation's
/// unknown reply. The original I/O slot stays occupied: an unknown submitted reserve
/// cannot leave an unbounded set of pending accepted operations behind parked frames.
fn reservation_turn(c:&Connection,o:&super::Original,maximum:u64)->anyhow::Result<()> {
    if let Reserved::Refused(why)=reserve(c,o,maximum)? {
        let why=crate::util::truncate(&why,4096).to_owned();
        // A positive own refusal marker forecloses only this SAME reserved item, and
        // only when no original accepted operation exists. A late exact replay cannot
        // reopen it; reserve_current reads this marker before any new key is reserved.
        c.execute("UPDATE ci_log_producer_items SET refusal=?2 WHERE operation=?1 AND receipt IS NULL AND refusal IS NULL AND NOT EXISTS(SELECT 1 FROM ci_log_accepted_ops WHERE operation=?1)",params![o.id,why])?;
    }Ok(())
}
fn reservation_receipt(c:&Connection,id:&str)->anyhow::Result<Option<Reserved>> {
    if let Some(op)=operation(c,id)? {return Ok(Some(Reserved::Ready(Box::new(op))))}
    let why:Option<String>=c.query_row("SELECT refusal FROM ci_log_producer_items WHERE operation=?1",[id],|r|r.get(0)).optional()?.flatten();
    Ok(why.map(Reserved::Refused))
}
fn close_turn(c:&Connection,id:&str)->anyhow::Result<()> {
    if own_receipt(c,id)?.is_some() {return Ok(())}
    let (r,_)=item_for(c,id)?.ok_or_else(||anyhow::anyhow!("original End descriptor disappeared"))?;
    if !current(c,&r)? {
        c.execute("UPDATE ci_log_producer_items SET refusal='original End owner ceased to be current' WHERE operation=?1 AND receipt IS NULL AND refusal IS NULL",[id])?;ready_end(c,&r.id)?;return Ok(())
    }
    let _=seal(c,id)?;Ok(())
}
fn close_receipt(c:&Connection,id:&str)->anyhow::Result<Option<JobLogReplyV1>> {
    if let Some(receipt)=own_receipt(c,id)? {return Ok(Some(JobLogReplyV1::Committed(receipt)))}
    let row:Option<(String,String,Option<String>)>=c.query_row("SELECT binding,item,refusal FROM ci_log_producer_items WHERE operation=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((binding,item,Some(why)))=row else{return Ok(None)};
    Ok(Some(refused(digest(&binding)?,unnumber(&item)?,JobLogRefusalV1::StaleOwner,&why)))
}

/// A single producer dependency turn. Gaps/Unknown return to the SAME bounded owner;
/// no new sleep, task, retry worker or synthetic input is introduced here.
pub(super) async fn turn(ctx:Context,binding:JobLogBindingV1,body:Vec<u8>,expected:Option<String>)->JobLogReplyV1 {
    let binding_id=match job_log::binding_id(&binding) {Ok(id)=>id,Err(error)=>return refused([0;32],0,JobLogRefusalV1::Malformed,&error.to_string())};
    let b=binding.clone();let prepared=match crate::rt::blocking(move ||prepare_item(&b,body)).await {
        Ok(Ok(prepared))=>prepared,
        Ok(Err(error))|Err(error)=>return refused(binding_id,0,JobLogRefusalV1::Malformed,&error.to_string()),
    };
    let item=prepared.item.clone();
    let seq=match item.seq(){Ok(seq)=>seq,Err(error)=>return refused(binding_id,0,JobLogRefusalV1::Malformed,&error.to_string())};
    let id=match job_log::stable_operation(&binding_id,seq){Ok(id)=>id,Err(error)=>return refused(binding_id,seq,JobLogRefusalV1::Malformed,&error.to_string())};
    if expected.as_ref().is_some_and(|original|original!=&id) {return refused(binding_id,seq,JobLogRefusalV1::Malformed,"body does not match its original outer item sequence")}
    let original=binding.clone();let host=ctx.host.clone();
    let admission=ctx.store.call(move |c|admit(c,&original,&prepared,&host)).await;
    let original=match admission {
        Ok(Admission::Pending)=>return unknown(binding_id,seq,"original predecessor/result receipt is pending; exact input retained"),
        Ok(Admission::Refused(code,why))=>return refused(binding_id,seq,code,&why),
        // The immutable original canonical-body SHA/length/kind, stable operation and
        // own checked head/cursor receipt identify this exact replay. A later object GET
        // outage cannot undo that past acceptance; raw complete reads validate bytes anew.
        Ok(Admission::Receipt(receipt))=>return JobLogReplyV1::Committed(receipt),
        Ok(Admission::Ready(original))=>*original,
        Err(error)=>return unknown(binding_id,seq,&format!("original producer reservation is unknown: {error:#}")),
    };
    if matches!(&item,Item::End(end) if end.final_seq>0) {
        let own=id.clone();return match ctx.store.call(move |c|seal(c,&own)).await {
            Ok(Some(receipt))=>{ctx.files.accepted(&binding.owner.job);JobLogReplyV1::Committed(receipt)},
            Ok(None)=>unknown(binding_id,seq,"captured predecessor/head is pending"),
            Err(error)=>{
                tracing::warn!(operation=id,error=%format!("{error:#}"),"End close unknown; original I/O retained until exact own disposition");
                let write=id.clone();let read=id.clone();
                let reply=transition(&ctx.store,move |c|close_turn(c,&write),move |c|close_receipt(c,&read)).await;
                if matches!(reply,JobLogReplyV1::Committed(_)) {ctx.files.accepted(&binding.owner.job)}
                reply
            },
        }
    }
    let o=original.clone();let max=ctx.files.max_bytes();
    let reserved=match ctx.store.call(move |c|reserve(c,&o,max)).await {
        Ok(reserved)=>reserved,
        Err(error)=>{
            tracing::warn!(operation=id,error=%format!("{error:#}"),"producer reservation unknown; same original I/O and operation retained");
            let o=original.clone();let own=id.clone();
            transition(&ctx.store,move |c|reservation_turn(c,&o,max),move |c|reservation_receipt(c,&own)).await
        },
    };
    let op=match reserved {
        Reserved::Ready(op)=>*op,
        Reserved::Busy=>return unknown(binding_id,seq,"same accepted view has a retained predecessor or suffix repair"),
        Reserved::Refused(why)=>return refused(binding_id,seq,JobLogRefusalV1::StaleOwner,&why),
    };
    if op.status=="pending" {
        match owned_put(&ctx,&op.input_key,&op.input_hash,item.bytes().to_vec()).await {
            Ok(())=>input_ready(&ctx,&op).await,
            Err(why)=>{let _=refused_append(&ctx,&id,why).await;return terminal_reply_owned(&ctx,id).await},
        }
        match retained_operation(ctx.clone(),op).await {
            Ok(done) if done.accepted=>{},
            Ok(_)=>return terminal_reply_owned(&ctx,id).await,
            Err(error)=>return unknown(binding_id,seq,&format!("original accepted operation remains unknown: {error:#}")),
        }
    }
    match ctx.store.call(move |c|terminal_reply(c,&id)).await {
        Ok(Some(reply))=>reply,
        Ok(None)=>unknown(binding_id,seq,"own producer receipt is not yet observed"),
        Err(error)=>unknown(binding_id,seq,&format!("own producer receipt remains unknown: {error:#}")),
    }
}
async fn refused_append(ctx:&Context,id:&str,why:String)->Receipt {super::refused(ctx,id.into(),why).await}

/// An accounting read of the SAME original reservation. Unknown/missing metadata keeps
/// this I/O future and its credit. Only a checked original terminal receipt clears debt;
/// a pending read is never promoted into an append/producer success.
pub(super) async fn remaining_debt(ctx:&Context,original:Operation)->Option<Operation> {
    reconcile(&ctx.store,move |c|{
        let Some(op)=operation(c,&original.id)? else{return Ok(None)};
        anyhow::ensure!(op.job==original.job && op.view==original.view && op.input_hash==original.input_hash && op.input_bytes==original.input_bytes && op.expected_revision==original.expected_revision && op.expected_root==original.expected_root,"original pending reservation identity changed");
        if completed(c,&original.id)?.is_some() {Ok(Some(None))}else{Ok(Some(Some(op)))}
    }).await
}

/// A cold missing pre-PUT input becomes a bounded debt in the SAME owner. This releases
/// the job's turn for its exact replay, but does not refund its eight-slot I/O credit.
pub(super) async fn recover(ctx:Context,op:Operation)->anyhow::Result<Option<Receipt>> {
    let own=op.id.clone();
    let current=reconcile(&ctx.store,move |c|Ok(Some(publication_current(c,&own)?))).await;
    if !current {return Ok(Some(refused_append(&ctx,&op.id,"original producer is no longer current; retained pins preserved".into()).await))}
    if !op.input_ready && !matches!(read_key(&ctx.blobs,&op.input_key,&op.input_hash,op.input_bytes).await,Ok(true)) {return Ok(None)}
    retained_operation(ctx,op).await.map(Some)
}

pub(super) async fn recovered_completion(ctx:&Context,id:String,done:Receipt)->Completion {
    reconcile(&ctx.store,move |c|{
        let binding:Option<String>=c.query_row("SELECT binding FROM ci_log_producer_items WHERE operation=?1",[&id],|r|r.get(0)).optional()?;
        let Some(binding)=binding else{return Ok(None)};
        let Some(r)=get(c,&binding)? else{return Ok(None)};
        let Some(checked)=completed(c,&id)? else{return Ok(None)};
        anyhow::ensure!(checked==done,"recovered original append disposition changed");
        let Some(reply)=terminal_reply(c,&id)? else{return Ok(None)};
        Ok(Some(Completion::RecoveredProducer{binding:r.binding,reply}))
    }).await
}

#[derive(Clone)]
pub(super) struct EndCandidate {pub binding:String,pub operation:String,pub job:String}

pub(super) fn end_page(c:&Connection,after:Option<&str>)->anyhow::Result<Vec<EndCandidate>> {
    let first="SELECT p.binding,i.operation,p.job FROM ci_log_producers p JOIN ci_log_producer_items i ON i.binding=p.binding AND i.kind='end' WHERE p.end_ready=1 ORDER BY p.binding LIMIT 8";
    let next="SELECT p.binding,i.operation,p.job FROM ci_log_producers p JOIN ci_log_producer_items i ON i.binding=p.binding AND i.kind='end' WHERE p.end_ready=1 AND p.binding>?1 ORDER BY p.binding LIMIT 8";
    let map=|r:&crate::sql::Row<'_>|Ok(EndCandidate{binding:r.get(0)?,operation:r.get(1)?,job:r.get(2)?});
    let mut statement=c.prepare(if after.is_some(){next}else{first})?;
    let rows=if let Some(after)=after {statement.query_map([after],map)?.collect::<crate::sql::Result<Vec<_>>>()?}else{statement.query_map([],map)?.collect::<crate::sql::Result<Vec<_>>>()?};
    if rows.is_empty() && after.is_some() {drop(statement);return end_page(c,None)}
    Ok(rows)
}

/// No route is fabricated on restart. This replays only the originally persisted End
/// through the SAME publisher; a later actual frame replay obtains its bound ACK.
pub(super) async fn recover_end(ctx:Context,candidate:EndCandidate)->anyhow::Result<(Option<Completion>,Option<Operation>)> {
    let id=candidate.operation.clone();let binding=candidate.binding.clone();
    let stored=reconcile(&ctx.store,move |c|{
        let Some(r)=get(c,&binding)? else{return Ok(Some(None))};
        let body:Option<Vec<u8>>=c.query_row("SELECT end_body FROM ci_log_producer_items WHERE operation=?1 AND binding=?2 AND kind='end'",params![id,binding],|r|r.get(0)).optional()?.flatten();
        Ok(Some(body.map(|body|(r.binding,body))))
    }).await;
    let Some((binding,body))=stored else{return Ok((None,None))};
    let reply=turn(ctx.clone(),binding.clone(),body,Some(candidate.operation.clone())).await;
    let id=candidate.operation.clone();
    let debt=reconcile(&ctx.store,move |c|{
        let Some(op)=operation(c,&id)? else{return Ok(Some(None))};
        if completed(c,&id)?.is_some() {Ok(Some(None))}else{Ok(Some(Some(op)))}
    }).await;
    if debt.is_some() {return Ok((None,debt))}
    if matches!(reply,JobLogReplyV1::Refused{..}) {
        // This is only a scheduling hint. Failure to clear it claims no acceptance;
        // another actual progress opportunity may recheck the same original body.
        let id=candidate.binding;let _=ctx.store.call(move |c|{c.execute("UPDATE ci_log_producers SET end_ready=0 WHERE binding=?1",[id])?;Ok(())}).await;
    }
    Ok((Some(Completion::RecoveredProducer{binding,reply}),None))
}

/// Existing retirement owner removes one fixed producer metadata page only after job
/// absence, same-view retirement, no pins and no live receipt owner were already checked.
pub(super) fn retire_page(c:&Connection,view:&str)->anyhow::Result<bool> {
    let id:Option<String>=c.query_row("SELECT binding FROM ci_log_producers WHERE view=?1 ORDER BY binding LIMIT 1",[view],|r|r.get(0)).optional()?;
    let Some(id)=id else {
        return Ok(c.execute("DELETE FROM ci_log_producer_reuse WHERE view=?1 AND attempt IN (SELECT attempt FROM ci_log_producer_reuse WHERE view=?1 ORDER BY attempt LIMIT 32)",[view])?>0)
    };
    let n=c.execute("DELETE FROM ci_log_producer_items WHERE binding=?1 AND item IN (SELECT item FROM ci_log_producer_items WHERE binding=?1 ORDER BY item LIMIT 32)",[&id])?;
    if n==0 {
        let n=c.execute("DELETE FROM ci_log_producer_steps WHERE binding=?1 AND step IN (SELECT step FROM ci_log_producer_steps WHERE binding=?1 ORDER BY step LIMIT 32)",[&id])?;
        if n==0 {c.execute("DELETE FROM ci_log_producers WHERE binding=?1",[id])?;}
    }Ok(true)
}

/// A raw step reader for API consumers of a real captured Prefix. No Plain normalizer,
/// search cap or mutable-file fallback; each turn returns at most one immutable CHUNK.
pub(crate) struct RawReader {reader:Reader}
pub(crate) struct RawTurn {pub bytes:Vec<u8>,pub reader:RawReader,pub done:bool}
impl RawReader {
    pub(crate) async fn open(store:&Store,prefix:Prefix,step:u32)->anyhow::Result<Option<Self>> {
        let p=prefix.clone();let exists=store.call(move |c|{
            checked_prefix(c,&p)?;
            Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_steps WHERE view=?1 AND step=?2 AND first_revision<=?3)",params![p.view,step,p.revision],|r|r.get::<_,bool>(0))?)
        }).await?;
        // None is a validated catalog absence under a REAL captured prefix, not a file
        // error or invented empty prefix. Strict complete callers also require End proof.
        if !exists {return Ok(None)}
        Ok(Some(Self{reader:Reader::open(store,prefix,step,0).await?}))
    }
    pub(crate) fn done(&self)->bool {self.reader.ordinal==self.reader.expected_ordinal}
    pub(crate) fn len(&self)->u64 {self.reader.expected_bytes}
    pub(crate) async fn extend(mut self,store:&Store,prefix:Prefix)->anyhow::Result<Self> {
        anyhow::ensure!(self.reader.prefix.view==prefix.view && prefix.revision>=self.reader.prefix.revision,"raw reader cannot change its original view or move backwards");
        let original=self.reader.prefix.clone();let next=prefix.clone();let step=self.reader.step;
        let (ordinal,bytes,root)=store.call(move |c|{
            checked_prefix(c,&original)?;checked_prefix(c,&next)?;
            if next.revision==original.revision {anyhow::ensure!(next.root==original.root,"same raw prefix revision changed root");}
            Ok(c.query_row("SELECT step_ordinal,step_bytes,step_root FROM ci_log_accepted_chunks WHERE view=?1 AND step=?2 AND revision<=?3 ORDER BY revision DESC LIMIT 1",params![next.view,step,next.revision],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?)))?)
        }).await?;
        anyhow::ensure!(ordinal>=self.reader.expected_ordinal && bytes>=self.reader.expected_bytes,"raw reader tail moved backwards");
        if ordinal==self.reader.expected_ordinal {anyhow::ensure!(bytes==self.reader.expected_bytes && root==self.reader.expected_root,"raw reader immutable tail changed");}
        self.reader.prefix=prefix;self.reader.expected_ordinal=ordinal;self.reader.expected_bytes=bytes;self.reader.expected_root=root;Ok(self)
    }
    pub(crate) async fn advance(self,store:&Store,blobs:&Arc<dyn Blobs>)->anyhow::Result<RawTurn> {
        if self.done() {return Ok(RawTurn{bytes:Vec::new(),reader:self,done:true})}
        let mut reader=self.reader;let (view,step,revision,after)=(reader.prefix.view.clone(),reader.step,reader.prefix.revision,reader.after);
        let chunk=store.call(move |c|Ok(c.query_row("SELECT view,revision,operation,step,step_ordinal,step_bytes,key,hash,bytes,previous_root,root,previous_step_root,step_root,historical FROM ci_log_accepted_chunks WHERE view=?1 AND step=?2 AND revision<=?3 AND revision>?4 ORDER BY revision LIMIT 1",params![view,step,revision,after],|r|Ok(Chunk{view:r.get(0)?,revision:r.get(1)?,operation:r.get(2)?,step:r.get(3)?,ordinal:r.get(4)?,step_bytes:r.get(5)?,key:r.get(6)?,hash:r.get(7)?,bytes:r.get(8)?,previous_root:r.get(9)?,root:r.get(10)?,previous_step_root:r.get(11)?,step_root:r.get(12)?,historical:r.get(13)?})).optional()?)).await?.ok_or_else(||anyhow::anyhow!("raw accepted manifest is incomplete"))?;
        anyhow::ensure!(chunk.bytes<=u64::try_from(CHUNK)? && reader.ordinal.checked_add(1)==Some(chunk.ordinal) && chunk.previous_step_root==reader.root && reader.bytes.checked_add(chunk.bytes)==Some(chunk.step_bytes),"raw accepted manifest chain conflicts");
        let (meta,bytes)=blobs.get(&chunk.key,None).await?;
        crate::rt::blocking(move ||{
            anyhow::ensure!(meta.key==chunk.key && meta.etag==chunk.hash && meta.size==chunk.bytes && u64::try_from(bytes.len()).ok()==Some(chunk.bytes) && hash(&bytes)==chunk.hash,"raw accepted immutable input is missing or corrupt");
            anyhow::ensure!(chain(&chunk.previous_root,&chunk)==chunk.root && chain(&chunk.previous_step_root,&chunk)==chunk.step_root,"raw accepted immutable roots conflict");
            reader.after=chunk.revision;reader.ordinal=chunk.ordinal;reader.bytes=chunk.step_bytes;reader.root=chunk.step_root;
            let done=reader.ordinal==reader.expected_ordinal;
            anyhow::ensure!(reader.ordinal<=reader.expected_ordinal && (!done || (reader.bytes==reader.expected_bytes && reader.root==reader.expected_root)),"raw accepted tail differs from captured prefix");
            Ok(RawTurn{bytes,reader:Self{reader},done})
        }).await?
    }
    pub(crate) async fn steps(store:&Store,prefix:Prefix,after:Option<i64>)->anyhow::Result<Vec<(i64,u32)>> {
        store.call(move |c|{
            checked_prefix(c,&prefix)?;
            Ok(c.prepare("SELECT first_revision,step FROM ci_log_accepted_steps WHERE view=?1 AND first_revision<=?2 AND first_revision>?3 ORDER BY first_revision LIMIT 32")?.query_map(params![prefix.view,prefix.revision,after.unwrap_or(0)],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<crate::sql::Result<Vec<_>>>()?)
        }).await
    }
}

fn checked_prefix(c:&Connection,p:&Prefix)->anyhow::Result<()> {
    let root:Option<String>=c.query_row("SELECT x.root FROM ci_log_accepted_chunks x JOIN ci_log_accepted_views v ON v.view=x.view WHERE x.view=?1 AND x.revision=?2 AND v.revision>=?2",params![p.view,p.revision],|r|r.get(0)).optional()?;
    anyhow::ensure!(p.revision>0 && root.as_deref()==Some(p.root.as_str()),"captured immutable head is missing or corrupt");Ok(())
}

#[derive(Debug,thiserror::Error)]
pub(crate) enum ReadError {
    #[error("required producer logs are pending their own captured close receipt")]
    Pending,
    #[error("required producer logs are unavailable: {0}")]
    Unavailable(String),
}
#[derive(Clone,Debug)]
pub(crate) struct ProducerSnapshot {
    pub complete:bool,pub prefix:Option<Prefix>,pub qualification:&'static str,
    pub capture:Option<JobLogCaptureV1>,pub max_bytes:u64,
    captured_bytes:u64,
    // Metadata only: an empty complete read must still GET/verify this original
    // End0 marker. This is captured with the SAME binding/close snapshot, not a
    // later lookup that could select a newer attempt's registration.
    empty_marker:Option<(String,String,u64)>,
}
#[derive(Clone,Debug)]
pub(crate) struct ProducerRead {
    pub bytes:Vec<u8>,pub complete:bool,pub prefix:Option<Prefix>,
    pub qualification:&'static str,pub capture:Option<JobLogCaptureV1>,
}

fn snapshot(c:&Connection,job:&str,require_complete:bool,max_bytes:u64)->anyhow::Result<Option<ProducerSnapshot>> {
    let attempt:Option<u32>=c.query_row("SELECT attempt FROM jobs WHERE id=?1",[job],|r|r.get(0)).optional()?;
    if let Some(attempt)=attempt && let Some((owner,contract))=crate::ci::dispatch::ordinary_log_registration(c,job,attempt)? {
        if contract.is_none() {
            anyhow::ensure!(!requires_contract(c,job,attempt)?,ReadError::Unavailable("retained required mode cannot become a legacy read".into()));return Ok(None)
        }
        let registered:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producers WHERE job=?1 AND attempt=?2 AND agent=?3 AND active=1)",params![job,attempt,owner.host],|r|r.get(0))?;
        anyhow::ensure!(registered,ReadError::Unavailable("declared required registration is missing; no legacy fallback".into()));
    }
    let id:Option<String>=c.query_row("SELECT p.binding FROM ci_log_producers p JOIN jobs j ON j.id=p.job AND j.rowid=p.job_order AND j.attempt=p.attempt WHERE p.job=?1 AND p.active=1",[job],|r|r.get(0)).optional()?;
    let Some(id)=id else {
        // Required mode is per ORIGINAL row/attempt, not a stamp on a later legacy
        // attempt. A missing current job with retained authority is still unavailable.
        let prior=if let Some(attempt)=attempt {requires_contract(c,job,attempt)?}else{c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producers WHERE job=?1)",[job],|r|r.get::<_,bool>(0))?};
        anyhow::ensure!(!prior,ReadError::Unavailable("registered original row/attempt is absent; no legacy fallback".into()));return Ok(None)
    };
    let r=get(c,&id)?.ok_or_else(||anyhow::anyhow!(ReadError::Unavailable("original binding disappeared".into())))?;
    let mut complete=false;let mut capture=None;let mut empty_marker=None;
    if let Some(result)=&r.result && let Close::Closed(receipt)=close(c,&r.binding,result)? {
        complete=true;
        if let JobLogViewV1::Captured(p)=receipt.view {checked_prefix(c,&Prefix{view:p.view,revision:p.revision,root:p.root})?;}
        if let Some(id)=r.close && let Some((_,LedgerItem::End(end)))=item_for(c,&id)? {
            if end.final_seq==0 {
                let op=operation(c,&id)?.ok_or_else(||anyhow::anyhow!(ReadError::Unavailable("original End0 marker disappeared".into())))?;
                anyhow::ensure!(op.input_bytes==0 && op.input_hash==hash(&[]),ReadError::Unavailable("original End0 marker identity conflicts".into()));
                empty_marker=Some((op.input_key,op.input_hash,op.input_bytes));
            }
            capture=Some(end.capture)
        }
    }
    if require_complete && !complete {return Err(ReadError::Pending.into())}
    // The close prefix proves original delivery; later explicit server-authored accepted
    // operations remain visible through the latest SAME-view head, never a projection file.
    let prefix=prefix(c,job)?;let mut captured_bytes=0;
    if let Some(p)=&prefix {
        anyhow::ensure!(p.view==r.view,ReadError::Unavailable("registered captured view changed".into()));checked_prefix(c,p)?;
        captured_bytes=c.query_row("SELECT bytes FROM ci_log_accepted_views WHERE view=?1",[&p.view],|r|r.get(0))?;
        anyhow::ensure!(captured_bytes<=max_bytes,ReadError::Unavailable("captured view exceeds current configured read bound".into()));
    }
    if complete && prefix.is_none() {return Err(ReadError::Unavailable("closed producer lacks a captured historical view".into()).into())}
    Ok(Some(ProducerSnapshot{complete,prefix,qualification:"registered_producer_and_captured_observed_job_view_v1",capture,max_bytes,captured_bytes,empty_marker}))
}

/// Existing product fixture drives this exact SQL/Blobs publisher leaf. It deliberately
/// does not manufacture a transport Reservation or claim an end-to-end network test.
#[cfg(test)]
pub(crate) async fn fixture_input(logs:&crate::logs::Logs,binding:JobLogBindingV1,body:Vec<u8>)->anyhow::Result<JobLogReplyV1> {
    Ok(turn(logs.accepted_context()?,binding,body,None).await)
}

#[cfg(test)]
pub(crate) async fn fixture_missing_input(logs:&crate::logs::Logs,binding:JobLogBindingV1,body:Vec<u8>)->anyhow::Result<String> {
    let ctx=logs.accepted_context()?;let b=binding.clone();
    let prepared=crate::rt::blocking(move ||prepare_item(&b,body)).await??;
    let host=ctx.host.clone();let maximum=ctx.files.max_bytes();
    let id=ctx.store.call(move |c|{
        let Admission::Ready(original)=admit(c,&binding,&prepared,&host)? else{anyhow::bail!("missing-input fixture did not reserve its original item")};
        let Reserved::Ready(op)=reserve(c,&original,maximum)? else{anyhow::bail!("missing-input fixture did not reserve its accepted operation")};
        anyhow::ensure!(op.status=="pending" && !op.input_ready,"fixture must stop BEFORE the original input PUT");Ok(op.id)
    }).await?;
    Ok(id)
}

/// W19-R04: settle only the existing discovery probe before this fixture seeds its
/// first producer. No live input, original delivery or receipt is discarded or reset.
#[cfg(test)]
pub(crate) async fn fixture_drain_before_producers(logs:&crate::logs::Logs)->anyhow::Result<Option<Completion>> {
    let ctx=logs.accepted_context()?;
    ctx.store.call(|c|{
        anyhow::ensure!(!c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_producers)",[],|r|r.get::<_,bool>(0))?,"fixture discovery drain must precede producer ledger seeding");Ok(())
    }).await?;
    let only_probe=|owner:&Owner|owner.ready && owner.recovery.is_none() && owner.jobs.is_empty() && owner.waiting.is_empty() && owner.parked.is_empty() && owner.deliveries.is_empty() && owner.end_waiting.is_empty() && owner.debts.is_empty() && owner.ends.is_empty() && owner.server==0 && owner.io.len()<=1;
    futures::future::poll_fn(|cx|{
        let mut owner=logs.io.lock();
        if !only_probe(&owner) {return std::task::Poll::Ready(Err(anyhow::anyhow!("fixture discovery drain refuses live work or original delivery custody")))}
        // deliver=false keeps every Hub delivery in the SAME owner. If an unexpected
        // append completion appears, return it for the fixture to inspect, never hide it.
        if let std::task::Poll::Ready(result)=owner.poll_inner(cx,false) {
            return std::task::Poll::Ready(result.map(Some))
        }
        if !only_probe(&owner) {return std::task::Poll::Ready(Err(anyhow::anyhow!("fixture discovery drain encountered live work; original owner retained")))}
        if owner.io.is_empty() && !owner.end_probe {return std::task::Poll::Ready(Ok(None))}
        std::task::Poll::Pending
    }).await
}

/// Rebuild only at a quiescent owner boundary, retaining the real Store/Blobs and their
/// original durable operations. This does not manufacture a Net grant or test transport.
#[cfg(test)]
pub(crate) async fn fixture_restart(logs:&crate::logs::Logs)->anyhow::Result<(usize,Vec<Completion>)> {
    let ctx=logs.accepted_context()?;
    {
        let mut owner=logs.io.lock();
        anyhow::ensure!(owner.io.is_empty() && owner.jobs.is_empty() && owner.waiting.is_empty() && owner.parked.is_empty() && owner.deliveries.is_empty() && owner.end_waiting.is_empty() && owner.server==0,"fixture restart requires a quiescent owner, not abandoned physical I/O");
        let mut next=Owner::default();next.bind(ctx);*owner=next;
    }
    let mut completions=Vec::new();
    let debts=futures::future::poll_fn(|cx|{
        let mut owner=logs.io.lock();
        if let std::task::Poll::Ready(result)=owner.poll(cx) {
            match result {Ok(completion)=>completions.push(completion),Err(error)=>return std::task::Poll::Ready(Err(error))}
        }
        if completions.len()>IO_SLOTS {return std::task::Poll::Ready(Err(anyhow::anyhow!("fixture recovery output exceeds its fixed page")))}
        if owner.ready && owner.io.is_empty() && owner.end_waiting.is_empty() && !owner.end_probe {
            return std::task::Poll::Ready(Ok(owner.debts.len()))
        }
        std::task::Poll::Pending
    }).await?;
    Ok((debts,completions))
}

#[cfg(test)]
pub(crate) async fn fixture_owned_replay(logs:&crate::logs::Logs,binding:JobLogBindingV1,body:Vec<u8>,operation:String)->anyhow::Result<JobLogReplyV1> {
    let (answer,reply)=futures::channel::oneshot::channel();let job=binding.owner.job.clone();
    logs.io.lock().server(Observed{job,step:0,input:Input::ProducerReplay(Box::new((binding,body,operation))),origin:"producer-fixture-replay".into(),answer:Some(Answer::Producer(answer))})?;
    logs.own_reply(reply).await
}

impl crate::logs::Logs {
    pub(crate) async fn producer_snapshot(&self,job:&str,require_complete:bool)->anyhow::Result<Option<ProducerSnapshot>> {
        let ctx=self.accepted_context()?;let job=job.to_owned();
        let maximum=ctx.files.max_bytes();
        // Logical pending is returned as data through a successful canonical read so
        // Amphora's outer Refusal wrapping cannot erase the caller's typed distinction.
        let result=ctx.store.call(move |c|match snapshot(c,&job,require_complete,maximum) {
            Ok(value)=>Ok(Ok(value)),
            Err(error)=>match error.downcast::<ReadError>() {Ok(reason)=>Ok(Err(reason)),Err(error)=>Err(error)},
        }).await.map_err(|e|ReadError::Unavailable(format!("{e:#}")))?;
        result.map_err(anyhow::Error::from)
    }
    pub(crate) async fn read_producer(&self,job:&str,step:u32,require_complete:bool)->anyhow::Result<Option<ProducerRead>> {
        self.read_producer_selected(job,Some(step),require_complete).await
    }
    /// U05/W20: one captured view, catalogued in first-observed step order. SQL step
    /// projections cannot decide whether a registered producer has bytes or is complete.
    pub(crate) async fn read_producer_all(&self,job:&str,require_complete:bool)->anyhow::Result<Option<ProducerRead>> {
        self.read_producer_selected(job,None,require_complete).await
    }
    async fn read_producer_selected(&self,job:&str,selected:Option<u32>,require_complete:bool)->anyhow::Result<Option<ProducerRead>> {
        let Some(snapshot)=self.producer_snapshot(job,require_complete).await? else{return Ok(None)};
        let ctx=self.accepted_context()?;let mut bytes=Vec::new();
        if let Some(prefix)=snapshot.prefix.clone() {
            let mut after=None;let mut steps=0i64;let mut remaining=prefix.revision;
            loop {
                let page=if let Some(step)=selected {vec![(0,step)]}else{RawReader::steps(&ctx.store,prefix.clone(),after).await.map_err(|e|ReadError::Unavailable(format!("{e:#}")))?};
                anyhow::ensure!(page.len()<=32,ReadError::Unavailable("raw accepted catalog page exceeds its bound".into()));
                if page.is_empty(){break}
                for (order,step) in page {
                    if selected.is_none() {
                        steps=steps.checked_add(1).ok_or_else(||anyhow::anyhow!(ReadError::Unavailable("raw accepted catalog count overflow".into())))?;
                        anyhow::ensure!(order>after.unwrap_or(0) && order<=prefix.revision && steps<=prefix.revision,ReadError::Unavailable("raw accepted catalog does not advance within its captured bound".into()));
                        after=Some(order);
                    }
                    let reader=RawReader::open(&ctx.store,prefix.clone(),step).await.map_err(|e|ReadError::Unavailable(format!("{e:#}")))?;
                    let Some(mut reader)=reader else {
                        anyhow::ensure!(selected.is_some(),ReadError::Unavailable("captured aggregate step disappeared".into()));continue
                    };
                    loop {
                        // One shared aggregate turn budget, not a fresh allowance per step.
                        anyhow::ensure!(remaining>0,ReadError::Unavailable("raw accepted read exceeds captured chunk count".into()));remaining-=1;
                        let turn=reader.advance(&ctx.store,&ctx.blobs).await.map_err(|e|ReadError::Unavailable(format!("{e:#}")))?;
                        let size=u64::try_from(bytes.len())?.checked_add(u64::try_from(turn.bytes.len())?).ok_or_else(||anyhow::anyhow!("raw read byte count overflow"))?;
                        anyhow::ensure!(size<=snapshot.max_bytes,ReadError::Unavailable("raw accepted read exceeds configured job bound".into()));
                        bytes.extend_from_slice(&turn.bytes);if turn.done {break}reader=turn.reader;
                    }
                }
                if selected.is_some(){break}
            }
        }
        if selected.is_none() {
            anyhow::ensure!(u64::try_from(bytes.len())?==snapshot.captured_bytes,ReadError::Unavailable("captured aggregate catalog does not cover its exact byte count".into()));
        }
        if snapshot.complete && bytes.is_empty() && let Some((key,digest,size))=&snapshot.empty_marker {
            match read_key(&ctx.blobs,key,digest,*size).await {
                Ok(true)=>{},
                Ok(false)=>return Err(ReadError::Unavailable("original End0 marker is missing; no complete empty substitution".into()).into()),
                Err(why)=>return Err(ReadError::Unavailable(why).into()),
            }
        }
        Ok(Some(ProducerRead{bytes,complete:snapshot.complete,prefix:snapshot.prefix,qualification:snapshot.qualification,capture:snapshot.capture}))
    }
    pub(crate) async fn producer_step_page(&self,prefix:Prefix,after:Option<i64>)->anyhow::Result<Vec<(i64,u32)>> {
        let ctx=self.accepted_context()?;RawReader::steps(&ctx.store,prefix,after).await
    }
}
