//! U05: the existing Spool owns strict producer bytes in the SAME Records/outbox.
//! A local durable commit permits transmission; only an original canonical receipt
//! retires bytes. Physical retirement never substitutes for that receipt.

use std::collections::BTreeMap;
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};

use anyhow::Context;
use kiln_seams::records::{Cond, Consistency, Records, Refusal, Row, Scan, Tx, Write};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::proto::{self, AgentMsg, JobLogBindingV1, JobLogCaptureV1, JobLogChunkV1, JobLogEndV1, JobLogReceiptV1, JobLogReplyV1, JobResult};
use crate::wire::{channel::{Frame, Has}, keep};
use super::{META, Spool};

const QUOTA: &[u8] = b"joblog-v1/quota";
const DESCRIPTORS: &[u8] = b"joblog-v1/b/";
const HOST_BYTES: u64 = 64 << 20;
const STREAM_BYTES: u64 = 8 << 20;
const HOST_CHUNKS: u64 = 2048;
const STREAM_CHUNKS: usize = 256;
const BINDINGS: u64 = 64;
const STEPS: u64 = 4096;
const PAGE: usize = 32;
const END_RESERVE: u64 = 98_304;
const RAW_INPUTS: usize = 64;

#[derive(Debug, thiserror::Error)]
#[error("required producer-log outbox is full; original input remains owned")]
pub(crate) struct Full;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Quota { bytes: u64, chunks: u64, bindings: u64, steps: u64 }

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected { root: [u8;32], bytes: u64, chunk: u64, charge: u64, result_hash: Option<[u8;32]> }

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    version: u8, binding: JobLogBindingV1, input: u64, input_hash: [u8;32], input_item:Option<u64>,
    issued: u64, captured: u64, discarded: u64, root: [u8;32],
    steps: BTreeMap<u32,u64>, pending: BTreeMap<u64,Expected>,
    end: Option<u64>, receipt: Option<JobLogReceiptV1>, uncertainty:Option<JobLogReplyV1>,
}

impl Descriptor {
    fn validate(&self, binding: &JobLogBindingV1) -> anyhow::Result<()> {
        proto::job_log::validate_binding(binding)?;
        anyhow::ensure!(self.version == 1 && &self.binding == binding, "producer descriptor changed original binding");
        anyhow::ensure!(self.pending.len() <= STREAM_CHUNKS + 1 && self.steps.len() <= usize::try_from(STEPS)?, "producer descriptor exceeds its bound");
        anyhow::ensure!(self.captured <= binding.contract.capture_limit && self.steps.values().try_fold(0u64, |a,n| a.checked_add(*n)).is_some_and(|n| n == self.captured), "producer step/total frontier differs");
        Ok(())
    }
}

fn key(binding: &JobLogBindingV1) -> anyhow::Result<Vec<u8>> {
    let mut key=DESCRIPTORS.to_vec(); key.extend_from_slice(hex::encode(proto::job_log::binding_id(binding)?).as_bytes()); Ok(key)
}
fn condition(row: &Option<Row>, family: &str, key: &[u8]) -> Cond {
    row.as_ref().map_or_else(||Cond::Absent{family:family.into(),key:key.to_vec()}, |row|Cond::At{family:family.into(),key:key.to_vec(),rev:row.rev})
}
async fn quota(records: &dyn Records) -> anyhow::Result<(Option<Row>,Quota)> {
    let row=records.get(META,QUOTA,Consistency::Linearizable).await?;
    let value:Quota=row.as_ref().map(|row|serde_json::from_slice(&row.value)).transpose()?.unwrap_or_default();
    anyhow::ensure!(value.bytes<=HOST_BYTES && value.chunks<=HOST_CHUNKS && value.bindings<=BINDINGS && value.steps<=STEPS,"producer quota metadata is inconsistent");
    Ok((row,value))
}
async fn descriptor(records: &dyn Records, binding: &JobLogBindingV1) -> anyhow::Result<(Row,Descriptor)> {
    let row=records.get(META,&key(binding)?,Consistency::Linearizable).await?.context("original producer descriptor is missing")?;
    let value:Descriptor=serde_json::from_slice(&row.value)?; value.validate(binding)?; Ok((row,value))
}
fn bytes<T:Serialize>(value:&T)->anyhow::Result<Vec<u8>> {
    let bytes=serde_json::to_vec(value)?;
    anyhow::ensure!(bytes.len()<=kiln_seams::records::VALUE_MAX,"producer metadata exceeds one bounded Records value"); Ok(bytes)
}

pub(crate) enum InputValue { Data {step:u32,data:Vec<u8>}, End {result:Vec<u8>,incomplete:Option<String>} }
/// Original bytes remain solely in this existing owner, never in the wake queue.
pub(crate) struct Input {
    binding:JobLogBindingV1, number:u64, slot:Option<u64>, value:InputValue,
    done:AtomicBool, changed:event_listener::Event,
    published:Mutex<Option<Option<Frame>>>,after:AtomicU64,
}
impl std::fmt::Debug for Input {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result { f.debug_struct("JobLogInput").field("number",&self.number).field("job",&self.binding.owner.job).finish_non_exhaustive() }
}
impl Input {
    pub(crate) fn binding(&self)->&JobLogBindingV1 { &self.binding }
    pub(crate) fn number(&self)->u64 { self.number }
    pub(crate) fn after(&self)->u64 {self.after.load(Ordering::Acquire)}
    #[cfg(test)]
    pub(crate) fn published(&self)->Option<Option<Frame>> { self.published.lock().clone() }
    pub(crate) fn take_published(&self)->Option<Option<Frame>> {self.published.lock().take()}
    fn complete(&self) { self.done.store(true,Ordering::Release);self.changed.notify(usize::MAX); }
    async fn wait(&self) { loop {let changed=self.changed.listen();if self.done.load(Ordering::Acquire){return}changed.await;} }
    fn hash(&self)->anyhow::Result<[u8;32]> {
        let id=proto::job_log::binding_id(&self.binding)?;
        let data=match &self.value {
            InputValue::Data{step,data}=>postcard::to_allocvec(&(id,self.number,0u8,*step,data))?,
            InputValue::End{result,incomplete}=>postcard::to_allocvec(&(id,self.number,1u8,result,incomplete))?,
        };
        Ok(sha2::Sha256::digest(&data).into())
    }
}

struct Slot { input:Option<u64>,retiring:bool }
struct Live { input:u64,pending:BTreeMap<u64,Arc<Input>>,slots:BTreeMap<u64,Slot>,next_slot:u64,incomplete:Option<String>,closed:bool,writers:usize }
/// SAME Spool's input handle. It retains binding/input bytes, never Execution/CPU custody.
pub(crate) struct Producer { binding:JobLogBindingV1,live:Mutex<Live>,changed:event_listener::Event }
/// An original capture flow reserves RAM credit BEFORE it can run/read anything.
/// Ordinary Leg clones carry None; strict background flows must fork before self.step.
pub(crate) struct Writer { producer:Arc<Producer>,slot:u64 }
impl Writer {
    pub(crate) fn binding(&self)->&JobLogBindingV1 {self.producer.binding()}
    pub(crate) fn incomplete(&self,why:&str){self.producer.incomplete(why);}
    pub(crate) fn capture(&self)->anyhow::Result<Capture<'_>> {self.producer.capture()}
    pub(crate) async fn fork(&self)->anyhow::Result<Arc<Writer>> {self.producer.reserve_writer().await}
    pub(crate) async fn log(&self,up:&crate::exec::Uplink,step:u32,data:Vec<u8>)->anyhow::Result<()> {
        anyhow::ensure!(!data.is_empty()&&data.len()<=proto::LOG_CHUNK,"producer input exceeds one captured chunk");
        let data=data.into_boxed_slice().into_vec();
        self.producer.submit(up,Some(self.slot),InputValue::Data{step,data}).await
    }
}
impl Drop for Writer {
    fn drop(&mut self){
        let mut live=self.producer.live.lock();
        if let Some(slot)=live.slots.get_mut(&self.slot){slot.retiring=true;if slot.input.is_none(){live.slots.remove(&self.slot);}}
        drop(live);self.producer.changed.notify(usize::MAX);
    }
}
impl Producer {
    fn new(binding:JobLogBindingV1)->Self {Self{binding,live:Mutex::new(Live{input:0,pending:BTreeMap::new(),slots:BTreeMap::from([(0,Slot{input:None,retiring:false})]),next_slot:1,incomplete:None,closed:false,writers:0}),changed:event_listener::Event::new()}}
    pub(crate) fn foreground(self:&Arc<Self>)->Arc<Writer> {Arc::new(Writer{producer:Arc::clone(self),slot:0})}
    pub(crate) fn binding(&self)->&JobLogBindingV1 {&self.binding}
    pub(crate) fn pending(&self)->Option<Arc<Input>> {self.live.lock().pending.first_key_value().map(|(_,input)|Arc::clone(input))}
    pub(crate) fn incomplete(&self,why:&str) {self.live.lock().incomplete.get_or_insert_with(||crate::util::truncate(why,proto::job_log::WHY_MAX).to_owned());}
    fn capture(&self)->anyhow::Result<Capture<'_>> {
        let mut live=self.live.lock();anyhow::ensure!(!live.closed,"original producer is closed");
        live.writers=live.writers.checked_add(1).context("producer writer count overflow")?;
        Ok(Capture{producer:self,settled:false})
    }
    async fn reserve_writer(self:&Arc<Self>)->anyhow::Result<Arc<Writer>> {
        loop{
            let changed=self.changed.listen();
            {
                let mut live=self.live.lock();anyhow::ensure!(!live.closed,"original producer is closed");
                if live.slots.len()<RAW_INPUTS{
                    let slot=live.next_slot;live.next_slot=slot.checked_add(1).context("producer capture slot identity exhausted")?;
                    live.slots.insert(slot,Slot{input:None,retiring:false});
                    return Ok(Arc::new(Writer{producer:Arc::clone(self),slot}));
                }
            }
            // No original pipe bytes/result have been read into this prospective flow.
            // Canceling this wait consumes neither input nor a reserved slot.
            changed.await;
        }
    }
    pub(crate) fn complete(&self,input:&Input)->anyhow::Result<()> {
        let mut live=self.live.lock();
        anyhow::ensure!(live.pending.get(&input.number).is_some_and(|original|std::ptr::eq(&**original,input)),"completed producer input changed original custody");
        if let Some(slot_id)=input.slot{
            let slot=live.slots.get_mut(&slot_id).context("original raw capture slot is missing")?;
            anyhow::ensure!(slot.input==Some(input.number),"original raw capture slot changed input");
            slot.input=None;if slot.retiring{live.slots.remove(&slot_id);}
        }
        live.pending.remove(&input.number);
        drop(live);input.complete();self.changed.notify(usize::MAX);Ok(())
    }
    async fn submit(&self,up:&crate::exec::Uplink,slot:Option<u64>,value:InputValue)->anyhow::Result<()> {
        let mut writer=self.capture()?;
        let input={
            let mut live=self.live.lock();
            anyhow::ensure!(!live.closed,"original producer cannot reopen after End");
            let value=match value {
                InputValue::End{result,..}=>{
                    anyhow::ensure!(slot.is_none()&&live.writers==1,"original producer still has unjoined writers; no End invented");
                    live.closed=true;InputValue::End{result,incomplete:live.incomplete.clone()}
                },
                value=>{
                    let slot=live.slots.get(&slot.context("raw input lacks its pre-reserved capture flow")?).context("raw capture flow was retired")?;
                    anyhow::ensure!(!slot.retiring&&slot.input.is_none(),"capture flow attempted concurrent reads without its original RAM credit");
                    value
                },
            };
            let number=live.input.checked_add(1).context("producer local input sequence exhausted")?;
            let input=Arc::new(Input{binding:self.binding.clone(),number,slot,value,done:AtomicBool::new(false),changed:event_listener::Event::new(),published:Mutex::new(None),after:AtomicU64::new(0)});
            if let Some(slot)=slot{live.slots.get_mut(&slot).context("raw capture flow disappeared")?.input=Some(number);}
            live.input=number;live.pending.insert(number,Arc::clone(&input));input
        };
        // The SAME original data now lives outside every cancelable send/wait.
        up.0.send(crate::exec::Up::JobLog(proto::job_log::binding_id(&self.binding)?)).await.map_err(|_|anyhow::anyhow!("producer uplink closed; original input remains pending"))?;
        input.wait().await;writer.settled=true;Ok(())
    }
    pub(crate) async fn end(&self,up:&crate::exec::Uplink,result:&JobResult)->anyhow::Result<()> {
        let result=proto::encode(result)?;proto::job_log::result_hash(&result)?;
        let result=result.into_boxed_slice().into_vec();
        self.submit(up,None,InputValue::End{result,incomplete:None}).await
    }
}
pub(crate) struct Capture<'a>{producer:&'a Producer,settled:bool}
impl Capture<'_>{pub(crate) fn finish(mut self){self.settled=true;}}
impl Drop for Capture<'_>{fn drop(&mut self){let mut live=self.producer.live.lock();live.writers=live.writers.saturating_sub(1);if !self.settled{live.incomplete.get_or_insert_with(||"an original producer writer did not finish its durable handoff".into());}}}

pub(crate) enum Clean { Finished, More, Waiting }

impl Spool {
    pub(crate) async fn register_job_log(&self,binding:JobLogBindingV1,execution:&crate::runner::processes::Execution)->anyhow::Result<Arc<Producer>> {
        proto::job_log::validate_binding(&binding)?;
        execution.require_native_attempt(&binding.owner.job,binding.owner.attempt)?;
        anyhow::ensure!(execution.identity()==binding.owner.execution,"producer admission changed original Execution");
        let key=key(&binding)?;
        let id=proto::job_log::binding_id(&binding)?;
        for _ in 0..3 {
            let existing=self.records.get(META,&key,Consistency::Linearizable).await?;
            if let Some(existing)=existing {
                let value:Descriptor=serde_json::from_slice(&existing.value)?;value.validate(&binding)?;
                anyhow::ensure!(value.input==0 && value.end.is_none(),"original producer already emitted input; no new writer may replace it");
                return Ok(Arc::new(Producer::new(binding)));
            }
            let (row,mut q)=quota(&*self.records).await?;
            anyhow::ensure!(q.bindings<BINDINGS&&q.bytes.checked_add(END_RESERVE).is_some_and(|n|n<=HOST_BYTES),Full);
            q.bindings+=1;q.bytes+=END_RESERVE;
            let value=Descriptor{version:1,binding:binding.clone(),input:0,input_hash:[0;32],input_item:None,issued:0,captured:0,discarded:0,root:proto::job_log::initial_root(&id),steps:BTreeMap::new(),pending:BTreeMap::new(),end:None,receipt:None,uncertainty:None};
            match self.records.commit(Tx{conds:vec![Cond::Absent{family:META.into(),key:key.clone()},condition(&row,META,QUOTA)],writes:vec![Write::Put{family:META.into(),key:key.clone(),value:bytes(&value)?},Write::Put{family:META.into(),key:QUOTA.to_vec(),value:bytes(&q)?}]}).await {
                Ok(_)=>return Ok(Arc::new(Producer::new(binding))),
                Err(Refusal::Conflict{..})=>continue,
                Err(error)=>return Err(error.into()),
            }
        }
        anyhow::bail!("producer registration changed concurrently; original admission remains held")
    }

    pub(crate) async fn publish_job_log(&self,input:&Input)->anyhow::Result<()> {
        input.after.store(self.records.head().await?,Ordering::Release);
        let binding=&input.binding;
        let input_hash=input.hash()?;
        let stream=proto::job_log::stream(binding)?;
        let descriptor_key=key(binding)?;
        for _ in 0..3 {
            let (row,mut d)=descriptor(&*self.records,binding).await?;
            if d.input==input.number {
                anyhow::ensure!(d.input_hash==input_hash,"producer original local input conflicts");
                let frame=if let Some(seq)=d.input_item.filter(|seq|d.pending.contains_key(seq)) {
                    // A discarded-only input has no new wire item. Match this exact
                    // item's saved body before treating it as this input's publication.
                    let out=self.records.get(keep::OUT,&keep::item_key("server",&stream,seq)?,Consistency::Linearizable).await?.context("original producer publication marker has no retained body")?;
                    Some(Frame::Item{stream:stream.clone(),seq,low:0,body:out.value})
                }else{None};
                *input.published.lock()=Some(frame);return Ok(());
            }
            anyhow::ensure!(d.input.checked_add(1)==Some(input.number)&&d.end.is_none(),"producer input is not the next original open slot");
            let (quota_row,mut q)=quota(&*self.records).await?;
            let mut frame=None;
            let mut added_step=false;
            match &input.value {
                InputValue::Data{step,data}=>{
                    anyhow::ensure!(!data.is_empty()&&data.len()<=proto::LOG_CHUNK,"producer data exceeds its input bound");
                    let count=u64::try_from(data.len())?;
                    let take=count.min(binding.contract.capture_limit.checked_sub(d.captured).context("producer cap regression")?);
                    d.discarded=d.discarded.checked_add(count-take).context("producer discarded-byte counter exhausted")?;
                    if take>0 {
                        let step_offset=d.steps.get(step).copied().unwrap_or(0);
                        added_step=!d.steps.contains_key(step);
                        anyhow::ensure!(!added_step||q.steps<STEPS,Full);
                        let seq=d.issued.checked_add(1).context("producer wire sequence exhausted")?;
                        let data=data.get(..usize::try_from(take)?).context("producer captured prefix unavailable")?.to_vec();
                        let chunk=JobLogChunkV1{binding:binding.clone(),seq,step:*step,offset:step_offset,previous_root:d.root,data};
                        let root=proto::job_log::chunk_root(&chunk)?;
                        let body=proto::encode(&AgentMsg::JobLogChunkV1(chunk))?;
                        let charge=u64::try_from(body.len())?;
                        let retained=d.pending.values().filter(|item|item.result_hash.is_none()).try_fold(END_RESERVE,|a,item|a.checked_add(item.charge)).context("producer byte charge overflow")?;
                        anyhow::ensure!(d.pending.len()<STREAM_CHUNKS&&q.chunks<HOST_CHUNKS&&retained.checked_add(charge).is_some_and(|n|n<=STREAM_BYTES)&&q.bytes.checked_add(charge).is_some_and(|n|n<=HOST_BYTES),Full);
                        d.captured=d.captured.checked_add(take).context("producer captured-byte counter exhausted")?;
                        d.steps.insert(*step,step_offset.checked_add(take).context("producer step offset exhausted")?);
                        d.root=root;d.issued=seq;
                        d.pending.insert(seq,Expected{root,bytes:d.captured,chunk:seq,charge,result_hash:None});
                        q.bytes+=charge;q.chunks+=1;
                        frame=Some(Frame::Item{stream:stream.clone(),seq,low:0,body});
                    }
                },
                InputValue::End{result,incomplete}=>{
                    let capture=match incomplete {
                        Some(why)=>JobLogCaptureV1::Incomplete{code:proto::JobLogRefusalV1::Incomplete,why:why.clone()},
                        None if d.discarded>0=>JobLogCaptureV1::Truncated{limit:binding.contract.capture_limit,discarded_bytes:d.discarded},
                        None=>JobLogCaptureV1::Complete,
                    };
                    let end=JobLogEndV1{binding:binding.clone(),final_seq:d.issued,captured_bytes:d.captured,root:d.root,result:result.clone(),result_hash:proto::job_log::result_hash(result)?,capture};
                    proto::job_log::validate_end(&end)?;
                    let seq=proto::job_log::end_item_seq(&end)?;
                    let hash=end.result_hash;
                    let body=proto::encode(&AgentMsg::JobLogEndV1(end))?;
                    anyhow::ensure!(u64::try_from(body.len())?<=END_RESERVE,"producer End exceeded its preowned reserved slot");
                    d.end=Some(seq);d.issued=seq;
                    d.pending.insert(seq,Expected{root:d.root,bytes:d.captured,chunk:seq-1,charge:0,result_hash:Some(hash)});
                    frame=Some(Frame::Item{stream:stream.clone(),seq,low:0,body});
                },
            }
            if added_step{q.steps+=1;}
            d.input=input.number;d.input_hash=input_hash;d.input_item=frame.as_ref().and_then(|frame|if let Frame::Item{seq,..}=frame{Some(*seq)}else{None});
            let mut conds=vec![Cond::At{family:META.into(),key:descriptor_key.clone(),rev:row.rev},condition(&quota_row,META,QUOTA)];
            let mut writes=vec![Write::Put{family:META.into(),key:descriptor_key.clone(),value:bytes(&d)?},Write::Put{family:META.into(),key:QUOTA.to_vec(),value:bytes(&q)?}];
            if let Some(Frame::Item{seq,body,..})=&frame {
                let out=keep::item_key("server",&stream,*seq)?;
                conds.push(Cond::Absent{family:keep::OUT.into(),key:out.clone()});
                writes.push(Write::Put{family:keep::OUT.into(),key:out,value:body.clone()});
                writes.push(Write::Put{family:keep::NEXT.into(),key:keep::stream_key("server",&stream)?,value:seq.checked_add(1).context("producer next sequence exhausted")?.to_be_bytes().to_vec()});
            }
            match self.records.commit(Tx{conds,writes}).await {
                Ok(_)=>{*input.published.lock()=Some(frame);return Ok(())},
                Err(Refusal::Conflict{..})=>continue,
                Err(error)=>return Err(error.into()),
            }
        }
        anyhow::bail!("producer metadata changed concurrently; same original input remains pending")
    }

    /// The original canonical receipt is persisted before either Sender or quota can
    /// forget bytes. Generic channel Have/Ack never calls this function.
    pub(crate) async fn receive_job_log(&self,binding:&JobLogBindingV1,reply:&JobLogReplyV1)->anyhow::Result<Has> {
        let binding_id=proto::job_log::binding_id(binding)?;
        let receipt=match reply {
            JobLogReplyV1::Committed(receipt)=>receipt,
            JobLogReplyV1::Refused{binding_id:original,item_seq,why,..}|JobLogReplyV1::Unknown{binding_id:original,item_seq,why,..}=>{
                anyhow::ensure!(*original==binding_id&&!why.is_empty()&&why.len()<=proto::job_log::WHY_MAX,"producer uncertainty changed its original binding or exceeds its diagnostic bound");
                if let JobLogReplyV1::Unknown{operation:Some(operation),..}=reply{anyhow::ensure!(*operation==proto::job_log::stable_operation(&binding_id,*item_seq)?,"producer uncertainty changed its original operation");}
                for _ in 0..3 {
                    let(row,mut d)=descriptor(&*self.records,binding).await?;
                    anyhow::ensure!(d.pending.contains_key(item_seq),"producer uncertainty exceeds retained original input");
                    if d.uncertainty.as_ref()==Some(reply){break}
                    d.uncertainty=Some(reply.clone());
                    match self.records.commit(Tx{conds:vec![Cond::At{family:META.into(),key:row.key.clone(),rev:row.rev}],writes:vec![Write::Put{family:META.into(),key:row.key,value:bytes(&d)?}]}).await{
                        Ok(_)=>break,
                        Err(Refusal::Conflict{..})=>continue,
                        Err(error)=>return Err(error.into()),
                    }
                }
                anyhow::bail!("producer has typed noncommitted own outcome; bytes and End remain held")
            },
        };
        anyhow::ensure!(receipt.binding_id==binding_id&&receipt.operation==proto::job_log::stable_operation(&binding_id,receipt.item_seq)?,"producer receipt changed original operation");
        for _ in 0..3 {
            let (row,mut d)=descriptor(&*self.records,binding).await?;
            if let Some(old)=&d.receipt {
                if old.item_seq>receipt.item_seq{return Ok(Has{stream:proto::job_log::stream(binding)?,upto:old.item_seq,nacks:Vec::new()})}
                if old.item_seq==receipt.item_seq{anyhow::ensure!(old==receipt,"original producer receipt changed");return Ok(Has{stream:proto::job_log::stream(binding)?,upto:receipt.item_seq,nacks:Vec::new()})}
            }
            let expected=d.pending.get(&receipt.item_seq).context("receipt exceeds original retained producer frontier")?;
            anyhow::ensure!((receipt.root,receipt.captured_bytes,receipt.chunk_seq,receipt.result_hash)==(expected.root,expected.bytes,expected.chunk,expected.result_hash),"canonical receipt differs from original producer bytes/result");
            match &receipt.view {
                proto::JobLogViewV1::ProducerOnly=>anyhow::ensure!(expected.chunk==0&&expected.bytes==0&&expected.result_hash.is_some(),"producer-only receipt cannot retire nonempty original bytes"),
                proto::JobLogViewV1::Captured(prefix)=>anyhow::ensure!(!prefix.view.is_empty()&&prefix.view.len()<=256&&prefix.revision>0&&prefix.root.len()==64&&prefix.root.bytes().all(|byte|byte.is_ascii_hexdigit()),"producer receipt has no bounded captured view"),
            }
            d.receipt=Some(receipt.clone());d.uncertainty=None;
            match self.records.commit(Tx{conds:vec![Cond::At{family:META.into(),key:row.key.clone(),rev:row.rev}],writes:vec![Write::Put{family:META.into(),key:row.key,value:bytes(&d)?}]}).await {
                Ok(_)=>return Ok(Has{stream:proto::job_log::stream(binding)?,upto:receipt.item_seq,nacks:Vec::new()}),
                Err(Refusal::Conflict{..})=>continue,
                Err(error)=>return Err(error.into()),
            }
        }
        anyhow::bail!("producer receipt changed concurrently; nothing retired")
    }

    /// One bounded page, on the SAME existing recovery/sample turns. Every remaining
    /// pending descriptor, including a foreign old process nonce, prevents clean retire.
    pub(crate) async fn clean_job_log_page(&self,binding:&JobLogBindingV1)->anyhow::Result<Clean> {
        let descriptor_key=key(binding)?;
        let Some(row)=self.records.get(META,&descriptor_key,Consistency::Linearizable).await? else{return Ok(Clean::Finished)};
        let mut d:Descriptor=serde_json::from_slice(&row.value)?;d.validate(binding)?;
        let Some(receipt)=&d.receipt else{return Ok(Clean::Waiting)};
        let upto=receipt.item_seq;
        let items:Vec<_>=d.pending.range(..=upto).take(PAGE).map(|(seq,item)|(*seq,item.clone())).collect();
        if items.is_empty() && d.end.is_none_or(|end|end>upto){return Ok(Clean::Waiting)}
        let (quota_row,mut q)=quota(&*self.records).await?;
        let stream=proto::job_log::stream(binding)?;
        let mut writes=Vec::new();
        for (seq,item) in items {
            d.pending.remove(&seq);
            q.bytes=q.bytes.checked_sub(item.charge).context("producer byte quota underflow")?;
            if item.result_hash.is_none(){q.chunks=q.chunks.checked_sub(1).context("producer chunk quota underflow")?;}
            writes.push(Write::Delete{family:keep::OUT.into(),key:keep::item_key("server",&stream,seq)?});
        }
        let finished=d.end.is_some_and(|end|end<=upto)&&d.pending.is_empty();
        if finished {
            q.bytes=q.bytes.checked_sub(END_RESERVE).context("producer End reservation underflow")?;
            q.bindings=q.bindings.checked_sub(1).context("producer binding quota underflow")?;
            q.steps=q.steps.checked_sub(u64::try_from(d.steps.len())?).context("producer step quota underflow")?;
            writes.push(Write::Delete{family:META.into(),key:descriptor_key.clone()});
            writes.push(Write::Delete{family:keep::NEXT.into(),key:keep::stream_key("server",&stream)?});
        }else{writes.push(Write::Put{family:META.into(),key:descriptor_key.clone(),value:bytes(&d)?});}
        writes.push(Write::Put{family:META.into(),key:QUOTA.to_vec(),value:bytes(&q)?});
        self.records.commit(Tx{conds:vec![Cond::At{family:META.into(),key:descriptor_key,rev:row.rev},condition(&quota_row,META,QUOTA)],writes}).await?;
        Ok(if finished{Clean::Finished}else if d.pending.range(..=upto).next().is_some(){Clean::More}else{Clean::Waiting})
    }

    /// A successfully closed original NoStart Gate may release only its never-used
    /// local End reservation. It cannot manufacture producer End or log completeness.
    pub(crate) async fn no_start_job_log(&self,binding:&JobLogBindingV1)->anyhow::Result<()> {
        let descriptor_key=key(binding)?;
        let Some(row)=self.records.get(META,&descriptor_key,Consistency::Linearizable).await? else{return Ok(())};
        let d:Descriptor=serde_json::from_slice(&row.value)?;d.validate(binding)?;
        anyhow::ensure!(d.input==0&&d.issued==0&&d.pending.is_empty()&&d.steps.is_empty()&&d.end.is_none(),"NoStart cannot retire original captured input");
        let (quota_row,mut q)=quota(&*self.records).await?;
        q.bytes=q.bytes.checked_sub(END_RESERVE).context("producer empty reservation underflow")?;
        q.bindings=q.bindings.checked_sub(1).context("producer empty binding underflow")?;
        self.records.commit(Tx{conds:vec![Cond::At{family:META.into(),key:descriptor_key.clone(),rev:row.rev},condition(&quota_row,META,QUOTA)],writes:vec![Write::Delete{family:META.into(),key:descriptor_key},Write::Put{family:META.into(),key:QUOTA.to_vec(),value:bytes(&q)?}]}).await?;
        Ok(())
    }

    pub(crate) async fn job_log_debt(&self)->anyhow::Result<bool> {
        let (_,q)=quota(&*self.records).await?;
        let any=!self.records.scan(META,Scan{prefix:DESCRIPTORS.to_vec(),after:None,limit:1,at:Consistency::Linearizable}).await?.rows.is_empty();
        anyhow::ensure!(any==(q.bindings>0),"producer descriptor/quota presence differs; retirement refused");
        Ok(any)
    }
}
