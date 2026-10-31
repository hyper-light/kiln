//! U05: exact receive-grant custody in the existing net owner. No wire format or
//! event-delivery authority lives here. A whole Reservation returns to its original
//! accountant; endpoint reincarnation never creates capacity for retained bytes.

use std::collections::{BTreeSet,HashMap};
use std::sync::Arc;

use hyper_transport::{Lane,Reservation};
use parking_lot::Mutex;

use super::{ApplicationPermit,Class,Connection,Out,PeerId};

// A fixed partition of the existing 16,384-slot handoff, charged before any Agent-Bulk
// frame is completed. It includes Endpoint-queued and application-retained frames.
pub(crate) const BULK_GRANTS:usize=64;
const SLOT_TAG:u8=64;
const URGENT:u8=1;
const COMMON:u8=0;

#[derive(Clone)]
pub(super) struct Owner(Arc<Mutex<Accountant>>);
#[derive(Clone)]
pub(super) struct Budget {owner:Owner,generation:u64}
pub(super) struct Generation {pub budget:Budget}

struct Pool {capacity:u64,used:u64,spare:Vec<Vec<u8>>,spare_limit:usize}
impl Pool {
    fn take(&mut self,bytes:u64,buffer:bool)->Result<Vec<u8>,hyper_transport::Refusal> {
        let next=self.used.checked_add(bytes).filter(|n|*n<=self.capacity).ok_or(hyper_transport::Refusal::Budget)?;
        let wanted=usize::try_from(bytes).map_err(|_|hyper_transport::Refusal::Budget)?;
        let mut value=if buffer {
            let fits=self.spare.iter().enumerate().filter(|(_,v)|v.capacity()>=wanted).min_by_key(|(_,v)|v.capacity()).map(|(n,_)|n);
            fits.map(|at|self.spare.swap_remove(at)).unwrap_or_default()
        }else{Vec::new()};
        value.clear();
        if buffer && value.capacity()<wanted {value.try_reserve_exact(wanted).map_err(|_|hyper_transport::Refusal::Budget)?;}
        self.used=next;Ok(value)
    }
    fn give(&mut self,reservation:Reservation)->bool {
        let Some(used)=self.used.checked_sub(reservation.granted()) else {return false};
        self.used=used;let buffer=reservation.into_buffer();
        if buffer.capacity()>0 && self.spare.len()<self.spare_limit {self.spare.push(buffer)}true
    }
}

#[derive(Default)]
struct Debt {common:u64,urgent:u64,retired:bool}
struct Slot {generation:u64,serial:u64,bytes:u64,capacity:usize,external:Option<(PeerId,u32)>}
struct Accountant {
    common:Pool,urgent:Pool,debts:HashMap<u64,Debt>,slots:Vec<Option<Slot>>,
    generation:u64,serial:u64,fenced:bool,wake:async_channel::WeakSender<Out>,
}

impl Owner {
    pub(super) fn new(wake:async_channel::WeakSender<Out>)->Self {
        // The same original 1 GiB server total, with 64 MiB that Bulk/Window cannot take.
        Self(Arc::new(Mutex::new(Accountant{
            // The original combined 256 cached buffers remains bounded across both pools.
            common:Pool{capacity:(1<<30)-(64<<20),used:0,spare:Vec::new(),spare_limit:240},
            urgent:Pool{capacity:64<<20,used:0,spare:Vec::new(),spare_limit:16},
            debts:HashMap::new(),slots:(0..BULK_GRANTS).map(|_|None).collect(),generation:0,serial:0,fenced:false,wake,
        })))
    }
    pub(super) fn begin(&self)->anyhow::Result<Generation> {
        let mut a=self.0.lock();
        anyhow::ensure!(!a.fenced && !a.debts.values().any(|d|!d.retired),"receive accountant is fenced or an endpoint incarnation remains owned");
        let generation=a.generation.checked_add(1).ok_or_else(||anyhow::anyhow!("receive generation exhausted"))?;
        a.generation=generation;a.debts.insert(generation,Debt::default());
        Ok(Generation{budget:Budget{owner:self.clone(),generation}})
    }
    pub(super) fn holds(&self)->BTreeSet<(PeerId,u32)> {
        self.0.lock().slots.iter().flatten().filter_map(|s|s.external).collect()
    }
}

impl hyper_transport::Budget<Class> for Budget {
    fn reserve(&mut self,bytes:u64,lane:Lane<Class>)->Result<Reservation,hyper_transport::Refusal> {
        let mut a=self.owner.0.lock();
        if a.fenced || a.debts.get(&self.generation).is_none_or(|d|d.retired) {return Err(hyper_transport::Refusal::Budget)}
        let is_bulk=matches!(lane,Lane::Class(Class::BulkFromAgent));
        let slot=if is_bulk {
            Some(a.slots.iter().position(Option::is_none).ok_or(hyper_transport::Refusal::Budget)?)
        }else{None};
        let buffer=!matches!(lane,Lane::Window);
        let urgent=matches!(lane,Lane::Class(Class::Interactive));
        let (tag,value)=if urgent {
            match a.urgent.take(bytes,buffer) {
                Ok(value)=>(URGENT,value),
                Err(_)=>(COMMON,a.common.take(bytes,buffer)?),
            }
        }else{(COMMON,a.common.take(bytes,buffer)?)};
        let tag=if let Some(slot)=slot {
            let Some(serial)=a.serial.checked_add(1) else {a.fenced=true;return Err(hyper_transport::Refusal::Budget)};
            a.serial=serial;
            *a.slots.get_mut(slot).ok_or(hyper_transport::Refusal::Budget)?=Some(Slot{generation:self.generation,serial,bytes,capacity:value.capacity(),external:None});
            SLOT_TAG+u8::try_from(slot).map_err(|_|hyper_transport::Refusal::Budget)?
        }else{tag};
        let Some(debt)=a.debts.get_mut(&self.generation) else {a.fenced=true;return Err(hyper_transport::Refusal::Budget)};
        let target=if tag==URGENT {&mut debt.urgent}else{&mut debt.common};
        let Some(total)=target.checked_add(bytes) else {a.fenced=true;return Err(hyper_transport::Refusal::Budget)};
        *target=total;Ok(Reservation::tagged(bytes,value,tag))
    }
    fn release(&mut self,reservation:Reservation) {self.release_exact(reservation,None)}
}

impl Budget {
    pub(super) fn holds(&self)->BTreeSet<(PeerId,u32)> {self.owner.holds()}
    fn release_exact(&self,reservation:Reservation,external:Option<u64>) {
        let tag=reservation.tag();let bytes=reservation.granted();let mut a=self.owner.0.lock();
        let slot=tag.checked_sub(SLOT_TAG).map(usize::from);
        let valid=if let Some(at)=slot {
            a.slots.get(at).and_then(Option::as_ref).is_some_and(|s|s.generation==self.generation && s.bytes==bytes && match external {Some(serial)=>s.serial==serial && s.external.is_some(),None=>s.external.is_none()})
        }else{external.is_none() && (tag==COMMON || tag==URGENT)};
        if !valid {a.fenced=true;tracing::error!(generation=self.generation,tag,"receive grant identity mismatch; no capacity refunded");return}
        let Some(debt)=a.debts.get_mut(&self.generation) else {a.fenced=true;return};
        let amount=if tag==URGENT {&mut debt.urgent}else{&mut debt.common};
        let Some(left)=amount.checked_sub(bytes) else {a.fenced=true;return};*amount=left;
        if let Some(at)=slot {if let Some(slot)=a.slots.get_mut(at){*slot=None}}
        let good=if tag==URGENT {a.urgent.give(reservation)}else{a.common.give(reservation)};
        if !good {a.fenced=true;tracing::error!("receive accountant underflow; further grants fenced")}
        if a.debts.get(&self.generation).is_some_and(|d|d.retired && d.common==0 && d.urgent==0) && !a.slots.iter().flatten().any(|slot|slot.generation==self.generation) {a.debts.remove(&self.generation);}
        if let Some(wake)=a.wake.upgrade() {let _=wake.try_send(Out::ReceiveReleased);}
    }
    pub(super) fn retain(&self,frame:Reservation,peer:PeerId,lane:u32,connection:Option<Connection>,admission:ApplicationPermit)->FrameBytes {
        let mut a=self.owner.0.lock();let at=frame.tag().checked_sub(SLOT_TAG).map(usize::from);
        let serial=at.and_then(|at|a.slots.get_mut(at)).and_then(Option::as_mut).filter(|s|s.generation==self.generation && s.bytes==frame.granted() && s.external.is_none()).map(|s|{s.external=Some((peer,lane));(s.serial,s.capacity)});
        if serial.is_none() {a.fenced=true;tracing::error!("received frame has no exact original grant; retained and fenced")}
        // Even an invariant failure retains the whole frame; it never fabricates a release.
        FrameBytes(Bytes::Received(Arc::new(Received{frame:Some(frame),budget:self.clone(),serial:serial.map(|v|v.0),capacity:serial.map(|v|v.1).unwrap_or(usize::MAX),peer,lane,connection,admission})))
    }
}

impl Drop for Generation {
    fn drop(&mut self) {
        // REQUIRED lexical barrier: thread_main declares Generation BEFORE LocalRuntime,
        // and explicitly drops LocalRuntime before this guard on every return/unwind path.
        // Driver owns Endpoint; LocalRuntime/Shard joins/drops the root and every child.
        // Thus every Reservation not in an exact external slot has already been dropped.
        let mut a=self.budget.owner.0.lock();let generation=self.budget.generation;
        if a.fenced {return}
        let external=a.slots.iter().flatten().filter(|s|s.generation==generation && s.external.is_some()).try_fold(0u64,|sum,s|sum.checked_add(s.bytes));
        let Some(external)=external else {a.fenced=true;return};
        let Some(debt)=a.debts.get(&generation) else {a.fenced=true;return};
        if debt.retired {a.fenced=true;return}
        let (Some(internal),urgent)=(debt.common.checked_sub(external),debt.urgent) else {a.fenced=true;return};
        let (Some(common_left),Some(urgent_left))=(a.common.used.checked_sub(internal),a.urgent.used.checked_sub(urgent)) else {a.fenced=true;return};
        a.common.used=common_left;a.urgent.used=urgent_left;
        for slot in &mut a.slots {if slot.as_ref().is_some_and(|s|s.generation==generation && s.external.is_none()){*slot=None}}
        if let Some(debt)=a.debts.get_mut(&generation){debt.common=external;debt.urgent=0;debt.retired=true;}
        // Zero-byte legal frames still own exact slots and incarnation identity.
        if external==0 && !a.slots.iter().flatten().any(|slot|slot.generation==generation) {a.debts.remove(&generation);}
    }
}

struct Received {
    frame:Option<Reservation>,budget:Budget,serial:Option<u64>,capacity:usize,peer:PeerId,lane:u32,
    // Frozen original authentication/connection identity survives every Unknown operation.
    connection:Option<Connection>,admission:ApplicationPermit,
}
impl Drop for Received {
    fn drop(&mut self) {
        if let (Some(frame),Some(serial))=(self.frame.take(),self.serial){self.budget.release_exact(frame,Some(serial));}
    }
}

enum Bytes {Owned(Vec<u8>),Received(Arc<Received>)}
#[derive(Clone)]
pub struct FrameBytes(Bytes);
impl Clone for Bytes {
    fn clone(&self)->Self {match self{Self::Owned(v)=>Self::Owned(v.clone()),Self::Received(r)=>Self::Received(Arc::clone(r))}}
}
impl From<Vec<u8>> for FrameBytes {fn from(value:Vec<u8>)->Self{Self(Bytes::Owned(value))}}
impl std::ops::Deref for FrameBytes {
    type Target=[u8];
    fn deref(&self)->&[u8] {match &self.0{Bytes::Owned(v)=>v,Bytes::Received(r)=>r.frame.as_ref().map(Reservation::bytes).unwrap_or_default()}}
}
impl AsRef<[u8]> for FrameBytes {fn as_ref(&self)->&[u8]{self}}
impl<T:AsRef<[u8]>+?Sized> PartialEq<T> for FrameBytes {fn eq(&self,other:&T)->bool{self.as_ref()==other.as_ref()}}
impl Eq for FrameBytes {}
impl std::fmt::Debug for FrameBytes {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {
        f.debug_struct("FrameBytes").field("bytes",&self.len()).field("allocated",&self.capacity()).field("retained",&self.retained()).finish()
    }
}
impl FrameBytes {
    // The agent downlink remains its original owned Vec path; retained server job-log
    // frames never use this conversion or release their credit before durable completion.
    pub(crate) fn into_vec(self)->Vec<u8> {match self.0{Bytes::Owned(value)=>value,Bytes::Received(frame)=>frame.frame.as_ref().map(|r|r.bytes().to_vec()).unwrap_or_default()}}
    pub(crate) fn capacity(&self)->usize {match &self.0{Bytes::Owned(v)=>v.capacity(),Bytes::Received(r)=>r.capacity}}
    pub(crate) fn retained(&self)->bool {matches!(self.0,Bytes::Received(_))}
    pub(crate) fn origin(&self)->String {
        match &self.0 {
            Bytes::Owned(_)=>"unretained-local-input".into(),
            Bytes::Received(r)=>{
                let identity=format!("{}|{:?}|{}|{}|{:?}|{}|{}|{}",r.budget.generation,r.serial,r.peer,r.lane,r.connection.map(|c|c.epoch()),r.connection.map(|c|c.connection_id()).unwrap_or_default(),r.admission.initialized,r.admission.key.as_deref().unwrap_or(""));
                format!("legacy-job-view/net/{}",crate::util::sha256_hex(identity.as_bytes()))
            }
        }
    }
}
