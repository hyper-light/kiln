//! What a durable stream keeps so a restart picks it up where it was (docs/wire.md):
//! - its sender keeps its unanswered items, and the number its next one gets;
//! - its receiver keeps through which item it has answered, and the refusals its sender may not
//!   have heard.
//!
//! Everything is kept in `Records`: kiln's store today, Amphora once critical-path T1 lands. So it
//! moves with the rest of kiln's records and never needs a store of its own. A sender keeps an
//! item before sending it (`keep`), and lets it go once it's answered (`forget`). A receiver keeps
//! its state as it answers (`keep_in`), in the same transaction as the work the item did when it
//! can.

use kiln_seams::records::{Cond, Consistency, Records, Refusal, Scan, TX_MAX, Tx, Write};

use super::channel::{Has, Seq};

/// A sender's unanswered items: peer, stream, number → the item's bytes.
pub const OUT: &str = "wire.out";
/// The number a sender's next item gets: peer, stream → the number.
pub const NEXT: &str = "wire.next";
/// A receiver's state: peer, stream → `Has` (JSON).
pub const IN: &str = "wire.in";

fn part<'a>(name: &'a str, what: &'static str) -> Result<&'a [u8], Refusal> {
    if name.is_empty() || name.as_bytes().contains(&0) {
        return Err(Refusal::Invalid { what, why: format!("{name:?}: not empty, and no NUL in it") });
    }
    Ok(name.as_bytes())
}

/// `peer` NUL `stream` NUL: the start of every key of one stream.
pub(crate) fn stream_key(peer: &str, stream: &str) -> Result<Vec<u8>, Refusal> {
    let mut k = part(peer, "a peer's name")?.to_vec();
    k.push(0);
    k.extend_from_slice(part(stream, "a stream's name")?);
    k.push(0);
    Ok(k)
}

pub(crate) fn item_key(peer: &str, stream: &str, seq: Seq) -> Result<Vec<u8>, Refusal> {
    let mut k = stream_key(peer, stream)?;
    k.extend_from_slice(&seq.to_be_bytes());
    Ok(k)
}

/// Keeps item `seq` of `stream` to `peer` before it's sent, with the number the next one gets.
pub async fn keep(r: &dyn Records, peer: &str, stream: &str, seq: Seq, body: &[u8]) -> Result<(), Refusal> {
    let tx = Tx {
        conds: Vec::new(),
        writes: vec![
            Write::Put { family: OUT.into(), key: item_key(peer, stream, seq)?, value: body.to_vec() },
            Write::Put { family: NEXT.into(), key: stream_key(peer, stream)?, value: seq.saturating_add(1).to_be_bytes().to_vec() },
        ],
    };
    r.commit(tx).await.map(|_| ())
}

/// Lets go of answered items.
pub async fn forget(r: &dyn Records, peer: &str, stream: &str, seqs: &[Seq]) -> Result<(), Refusal> {
    for chunk in seqs.chunks(TX_MAX) {
        let writes = chunk.iter().map(|s| item_key(peer, stream, *s).map(|key| Write::Delete { family: OUT.into(), key })).collect::<Result<Vec<_>, _>>()?;
        r.commit(Tx { conds: Vec::new(), writes }).await?;
    }
    Ok(())
}

/// Every row of `family` whose key starts with `prefix`.
async fn all(r: &dyn Records, family: &str, prefix: Vec<u8>) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Refusal> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = r.scan(family, Scan { prefix: prefix.clone(), after: after.clone(), limit: 1000, at: Consistency::Linearizable }).await?;
        after = page.rows.last().map(|row| row.key.clone());
        out.extend(page.rows.into_iter().map(|row| (row.key, row.value)));
        if !page.more || after.is_none() {
            return Ok(out);
        }
    }
}

/// A durable stream's sending side, as kept: its name, the number its next item gets, and its
/// unanswered items in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub stream: String,
    pub next: Seq,
    pub pending: Vec<(Seq, Vec<u8>)>,
}

/// What a sender kept of its streams to a peer (Sender::restore picks each up).
/// These are opaque original bytes, not a codec migration. A caller must establish the
/// writer's protocol layout before replay; unknown experimental layouts must be refused,
/// preserving their bytes. Current runner completion journals are process-local, and the
/// retained-event spool stores JSON receipts which are encoded afresh when sent.
pub async fn load_out(r: &dyn Records, peer: &str) -> Result<Vec<Kept>, Refusal> {
    let mut prefix = part(peer, "a peer's name")?.to_vec();
    prefix.push(0);
    let mut kept: std::collections::BTreeMap<String, Kept> = std::collections::BTreeMap::new();
    for (key, value) in all(r, NEXT, prefix.clone()).await? {
        let Some(stream) = key.strip_prefix(prefix.as_slice()).and_then(|k| k.strip_suffix(&[0])).and_then(|s| std::str::from_utf8(s).ok()) else { continue };
        let next = <[u8; 8]>::try_from(value.as_slice()).map(u64::from_be_bytes).unwrap_or(1);
        kept.insert(stream.to_owned(), Kept { stream: stream.to_owned(), next, pending: Vec::new() });
    }
    for (key, value) in all(r, OUT, prefix.clone()).await? {
        let Some(rest) = key.strip_prefix(prefix.as_slice()) else { continue };
        let Some(nul) = rest.iter().position(|b| *b == 0) else { continue };
        let Some((name, seq)) = rest.split_at_checked(nul) else { continue };
        let Some(seq) = seq.strip_prefix(&[0]).and_then(|s| <[u8; 8]>::try_from(s).ok()).map(u64::from_be_bytes) else { continue };
        let Ok(stream) = std::str::from_utf8(name) else { continue };
        let k = kept.entry(stream.to_owned()).or_insert_with(|| Kept { stream: stream.to_owned(), next: seq.saturating_add(1), pending: Vec::new() });
        k.pending.push((seq, value));
    }
    Ok(kept.into_values().collect())
}

/// Keeps a receiver's state of `stream` from `peer`.
pub async fn keep_in(r: &dyn Records, peer: &str, has: &Has) -> Result<(), Refusal> {
    let value = serde_json::to_vec(has).map_err(|e| Refusal::Invalid { what: "a stream's state", why: e.to_string() })?;
    let key = stream_key(peer, &has.stream)?;
    r.commit(Tx { conds: Vec::new(), writes: vec![Write::Put { family: IN.into(), key, value }] }).await.map(|_| ())
}

/// What a receiver kept of its streams from `peer` (`Receiver::restore` picks each up).
pub async fn load_in(r: &dyn Records, peer: &str) -> Result<Vec<Has>, Refusal> {
    let mut prefix = part(peer, "a peer's name")?.to_vec();
    prefix.push(0);
    let mut out = Vec::new();
    for (_, value) in all(r, IN, prefix).await? {
        if let Ok(has) = serde_json::from_slice::<Has>(&value) {
            out.push(has);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use kiln_seams::records::SimRecords;

    use super::super::channel::{Frame, Nack, Refusal as Why, Sender};
    use super::*;

    /// What a sender kept outlives it: a new one picks its streams up and sends them again.
    async fn a_restart_sends_what_was_kept(r: &dyn Records) -> anyhow::Result<()> {
        let mut s = Sender::new(1 << 20, Duration::from_secs(10));
        for n in 0..3u8 {
            let seq = s.push("cmd", vec![n]).map_err(|_| anyhow::anyhow!("full"))?;
            keep(r, "runner-a", "cmd", seq, &[n]).await?;
        }
        // Item 1 was answered; 2 and 3 weren't when it stopped.
        forget(r, "runner-a", "cmd", &[1]).await?;
        let kept = load_out(r, "runner-a").await?;
        assert_eq!(kept, vec![Kept { stream: "cmd".into(), next: 4, pending: vec![(2, vec![1]), (3, vec![2])] }]);
        let mut again = Sender::new(1 << 20, Duration::from_secs(10));
        for k in kept {
            again.restore(&k.stream, k.next, k.pending);
        }
        again.on_frame(Frame::Have(vec![Has { stream: "cmd".into(), upto: 1, nacks: Vec::new() }]));
        let seqs: Vec<Seq> = again.poll(Instant::now()).into_iter().filter_map(|f| if let Frame::Item { seq, .. } = f { Some(seq) } else { None }).collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(again.push("cmd", vec![9]), Ok(4));
        // Another peer's streams are its own.
        assert!(load_out(r, "runner-b").await?.is_empty());
        // A receiver's state too, refusals and all.
        let has = Has { stream: "cmd".into(), upto: 7, nacks: vec![Nack { seq: 6, code: Why::Unknown, why: "no such approval".into() }] };
        keep_in(r, "server", &has).await?;
        assert_eq!(load_in(r, "server").await?, vec![has]);
        // U01/U03: full authoritative state includes separate admission and wire outcomes.
        let nonce="a".repeat(43);let origin=crate::net::Connection::new(1,1,[1;32]);let stream="l:7.1";
        assert!(!ordinary_incoming_present(r).await?);
        let loaded=load_ordinary_incoming(r,&nonce).await?;
        let registered=register_ordinary_incoming(r,&nonce,origin,loaded.as_ref()).await?;
        assert!(ordinary_incoming_present(r).await?);
        let (has,admitted)=registered.for_stream(origin,&nonce,stream)?;
        let mut receiver=super::super::channel::Receiver::new();receiver.bind_ordinary(origin,&nonce,has,admitted)?;
        receiver.stage_ordinary(origin,&nonce,stream,1,0,b"refused".to_vec())?;
        let ticket=receiver.ordinary_head(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("incoming oracle ticket missing"))?;
        let accepted=accept_ordinary_incoming_state(r,&nonce,origin,stream,1).await?;
        let (unanswered,admitted)=accepted.for_stream(origin,&nonce,stream)?;
        assert_eq!((unanswered.upto,admitted),(0,1),"admission is not Taken");
        let mut fresh=super::super::channel::Receiver::new();fresh.bind_ordinary(origin,&nonce,unanswered,admitted)?;
        assert!(matches!(fresh.stage_ordinary(origin,&nonce,stream,1,100,b"must not run".to_vec())?,super::super::channel::OrdinaryArrival::AdmittedPending));
        assert!(close_ordinary_incoming(r,&nonce,&origin.channel_id(),stream,3).await.is_err());
        receiver.ordinary_admitted(&ticket)?;
        receiver.ordinary_effect_done(&ticket,Some((Why::Unknown,"original refusal".into())))?;
        let closure=receiver.ordinary_ready(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("incoming oracle closure missing"))?;
        let saved=commit_ordinary_incoming(r,&closure).await?;let _lost=receiver.ordinary_committed(&closure)?;
        receiver.stage_ordinary(origin,&nonce,stream,2,0,b"valid original Start".to_vec())?;
        let next=receiver.ordinary_head(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("refusal stranded next Start"))?;
        accept_ordinary_incoming_state(r,&nonce,origin,stream,2).await?;receiver.ordinary_admitted(&next)?;
        receiver.ordinary_effect_done(&next,None)?;
        let next_closure=receiver.ordinary_ready(origin,&nonce,stream)?.ok_or_else(||anyhow::anyhow!("next Start result missing"))?;
        commit_ordinary_incoming(r,&next_closure).await?;receiver.ordinary_committed(&next_closure)?;
        close_ordinary_incoming(r,&nonce,&origin.channel_id(),stream,3).await?;
        let durable=load_ordinary_incoming(r,&nonce).await?.ok_or_else(||anyhow::anyhow!("incoming full state missing"))?;
        let (has,admitted)=durable.for_stream(origin,&nonce,stream)?;
        assert_eq!(has.nacks.len(),1,"positive Gate closure preserves refusal outcome");assert_eq!((has.upto,admitted),(2,3));
        let mut restored=super::super::channel::Receiver::new();restored.bind_ordinary(origin,&nonce,has.clone(),admitted)?;
        assert_eq!(restored.stage_ordinary(origin,&nonce,stream,1,0,b"refused".to_vec())?,super::super::channel::OrdinaryArrival::Answered(super::super::channel::Frame::Ack(has)));
        close_ordinary_incoming(r,&nonce,&origin.channel_id(),"l:7.2",1).await?;
        let newer=load_ordinary_incoming(r,&nonce).await?.ok_or_else(||anyhow::anyhow!("new generation missing"))?;
        assert!(newer.for_stream(origin,&nonce,stream).is_err());
        assert!(newer.for_stream(origin,&nonce,"l:6.1").is_err());
        assert!(close_ordinary_incoming(r,&nonce,&origin.channel_id(),"l:6.1",1).await.is_err());
        let mut unproven=super::super::channel::Receiver::new();
        assert!(matches!(unproven.on_item_strict("l:8.1",3,100,b"missing original".to_vec()),super::super::channel::Arrival::Gap(_)));
        let replacement=crate::net::Connection::new(1,2,[2;32]);
        register_ordinary_incoming(r,&nonce,replacement,Some(&newer)).await?;
        assert!(commit_ordinary_incoming(r,&closure).await.is_err(),"late old route cannot overwrite current domain");
        assert!(accept_ordinary_incoming_state(r,&nonce,origin,stream,2).await.is_err());
        assert!(register_ordinary_incoming(r,&nonce,origin,Some(&saved)).await.is_err(),"stale expected revision cannot rebind old origin");
        close_ordinary_incoming(r,&nonce,&origin.channel_id(),stream,2).await?;
        let current=load_ordinary_incoming(r,&nonce).await?.ok_or_else(||anyhow::anyhow!("replacement state missing"))?;
        assert!(current.for_stream(origin,&nonce,stream).is_err());
        assert_eq!(current.for_stream(replacement,&nonce,"l:8.1")?.1,0);
        Ok(())
    }

    #[test]
    fn kept_in_the_simulator() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let r = SimRecords::new();
            a_restart_sends_what_was_kept(&r).await
        })
    }

    #[test]
    fn kept_in_kilns_store() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let r = crate::records::StoreRecords::open(crate::store::Store::in_memory()?)?;
            a_restart_sends_what_was_kept(&r).await
        })
    }

    #[test]
    fn a_name_with_a_nul_is_refused() {
        assert!(stream_key("a\0b", "s").is_err());
        assert!(stream_key("a", "").is_err());
    }
}

/// U01/U03: admission exclusion is NOT a completed wire disposition. One bounded
/// SAME incoming row owns maxdomain/admitted prefix and completed Has/NACK separately.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OrdinaryIncomingRefusal {
    #[error("ordinary incoming domain changed without its captured registration")] DomainChanged,
    #[error("ordinary incoming acceptance has an unresolved earlier command")] MissingPrefix,
    #[error("ordinary incoming admission metadata is full; authoritative drain is required")] Full,
    #[error("ordinary incoming acceptance metadata is corrupt")] Corrupt,
}
#[derive(Clone,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrdinaryIncomingState {
    version:u8,process_nonce:String,connection:String,born:u64,generation:u64,
    through:u64,has:Has,
    #[serde(skip)] rev:u64,
}
impl OrdinaryIncomingState {
    /// Pure derivation from the complete authoritative load/known commit, never a read
    /// per frame. A new authenticated generation is empty; old/foreign domains refuse.
    pub(crate) fn for_stream(&self,origin:crate::net::Connection,nonce:&str,stream:&str)->anyhow::Result<(Has,u64)> {
        check_incoming(self,nonce)?;
        anyhow::ensure!(self.connection==origin.channel_id(),OrdinaryIncomingRefusal::DomainChanged);
        let (born,generation)=ordinary_name(stream)?;
        if self.generation==0 {return Ok((empty_has(stream),0));}
        anyhow::ensure!(self.born==born && generation>=self.generation,OrdinaryIncomingRefusal::DomainChanged);
        if generation>self.generation {return Ok((empty_has(stream),0));}
        Ok((self.has.clone(),self.through))
    }
}
const ORDINARY_COUNT:&[u8]=b"ordinary/live-count";
fn empty_has(stream:&str)->Has {Has{stream:stream.into(),upto:0,nacks:Vec::new()}}
fn ordinary_key(nonce:&str)->anyhow::Result<Vec<u8>> {
    anyhow::ensure!(crate::proto::valid_process_nonce(nonce),OrdinaryIncomingRefusal::Corrupt);
    Ok(format!("ordinary/live/{nonce}").into_bytes())
}
fn ordinary_name(stream:&str)->anyhow::Result<(u64,u64)> {
    let (born,generation)=stream.strip_prefix("l:").and_then(|s|s.split_once('.')).ok_or(OrdinaryIncomingRefusal::Corrupt)?;
    let (born,generation)=(born.parse::<u64>()?,generation.parse::<u64>()?);
    anyhow::ensure!(generation>0 && generation<u64::MAX,OrdinaryIncomingRefusal::Corrupt);
    Ok((born,generation))
}
fn check_incoming(row:&OrdinaryIncomingState,nonce:&str)->anyhow::Result<()> {
    anyhow::ensure!(row.version==2 && row.process_nonce==nonce && crate::proto::valid_process_nonce(nonce)
        && row.connection.len()==64 && row.connection.bytes().all(|b|b.is_ascii_hexdigit())
        && row.through<u64::MAX && row.has.upto<=row.through && row.through-row.has.upto<=4096,OrdinaryIncomingRefusal::Corrupt);
    if row.generation==0 {
        anyhow::ensure!(row.born==0 && row.through==0 && row.has==empty_has(""),OrdinaryIncomingRefusal::Corrupt);
    } else {
        anyhow::ensure!(row.generation<u64::MAX && row.has.stream==format!("l:{}.{}",row.born,row.generation),OrdinaryIncomingRefusal::Corrupt);
        super::channel::ordinary_has_valid(&row.has).map_err(|_|OrdinaryIncomingRefusal::Corrupt)?;
    }
    Ok(())
}
async fn incoming_at(r:&dyn Records,nonce:&str,at:Consistency)->anyhow::Result<Option<OrdinaryIncomingState>> {
    let Some(row)=r.get(IN,&ordinary_key(nonce)?,at).await? else {return Ok(None)};
    anyhow::ensure!(row.value.len()<=kiln_seams::records::VALUE_MAX,OrdinaryIncomingRefusal::Corrupt);
    let mut value:OrdinaryIncomingState=serde_json::from_slice(&row.value).map_err(|_|OrdinaryIncomingRefusal::Corrupt)?;
    value.rev=row.rev;check_incoming(&value,nonce)?;Ok(Some(value))
}
/// Prior incoming metadata remains epoch evidence after every Gate/publication compacts.
pub(crate) async fn ordinary_incoming_present(r:&dyn Records)->anyhow::Result<bool> {
    let at=Consistency::Snapshot(r.head().await?);
    let counter=r.get(IN,ORDINARY_COUNT,at).await?;
    let row=r.scan(IN,Scan{prefix:b"ordinary/live/".to_vec(),after:None,limit:1,at}).await?;
    Ok(counter.is_some() || !row.rows.is_empty())
}
/// Full maxdomain + admission + completed outcomes load ONCE before strict readiness.
/// Missing is returned explicitly; errors/corruption are never an empty fresh state.
pub(crate) async fn load_ordinary_incoming(r:&dyn Records,nonce:&str)->anyhow::Result<Option<OrdinaryIncomingState>> {
    incoming_at(r,nonce,Consistency::Linearizable).await
}
async fn save_incoming(r:&dyn Records,nonce:&str,old:Option<&OrdinaryIncomingState>,mut value:OrdinaryIncomingState,at:Consistency)->anyhow::Result<OrdinaryIncomingState> {
    anyhow::ensure!(value.has.upto<=value.through,OrdinaryIncomingRefusal::Corrupt);
    anyhow::ensure!(value.through-value.has.upto<=4096,OrdinaryIncomingRefusal::Full);
    check_incoming(&value,nonce)?;
    let encoded=serde_json::to_vec(&value)?;
    anyhow::ensure!(encoded.len()<=kiln_seams::records::VALUE_MAX,OrdinaryIncomingRefusal::Full);
    let key=ordinary_key(nonce)?;
    let count=r.get(IN,ORDINARY_COUNT,at).await?;
    if count.is_none() {anyhow::ensure!(r.scan(IN,Scan{prefix:b"ordinary/live/".to_vec(),after:None,limit:1,at}).await?.rows.is_empty(),OrdinaryIncomingRefusal::Corrupt);}
    let n:usize=count.as_ref().map(|row|serde_json::from_slice(&row.value)).transpose()?.unwrap_or(0);
    anyhow::ensure!(n<=4096 && (old.is_none() || n>0),OrdinaryIncomingRefusal::Corrupt);
    anyhow::ensure!(old.is_some() || n<4096,OrdinaryIncomingRefusal::Full);
    let mut conds=vec![old.map_or(Cond::Absent{family:IN.into(),key:key.clone()},|old|Cond::At{family:IN.into(),key:key.clone(),rev:old.rev})];
    let mut writes=vec![Write::Put{family:IN.into(),key:key.clone(),value:encoded}];
    if old.is_none() {
        conds.push(count.as_ref().map_or(Cond::Absent{family:IN.into(),key:ORDINARY_COUNT.to_vec()},|row|Cond::At{family:IN.into(),key:ORDINARY_COUNT.to_vec(),rev:row.rev}));
        writes.push(Write::Put{family:IN.into(),key:ORDINARY_COUNT.to_vec(),value:serde_json::to_vec(&(n+1))?});
    }
    value.rev=r.commit(Tx{conds,writes}).await?;Ok(value)
}
/// Actual connection registration fences late old-origin writers BEFORE strict capability.
/// Only the exact loaded revision may change binding; no read-newest-and-retarget retry.
pub(crate) async fn register_ordinary_incoming(r:&dyn Records,nonce:&str,origin:crate::net::Connection,expected:Option<&OrdinaryIncomingState>)->anyhow::Result<OrdinaryIncomingState> {
    let at=Consistency::Snapshot(r.head().await?);let old=incoming_at(r,nonce,at).await?;
    if let Some(old)=&old && old.connection==origin.channel_id() {return Ok(old.clone());}
    if let Some(expected)=expected {check_incoming(expected,nonce)?;}
    anyhow::ensure!(old.as_ref().map(|s|s.rev)==expected.map(|s|s.rev),OrdinaryIncomingRefusal::DomainChanged);
    let value=OrdinaryIncomingState{version:2,process_nonce:nonce.into(),connection:origin.channel_id(),born:0,generation:0,through:0,has:empty_has(""),rev:0};
    save_incoming(r,nonce,old.as_ref(),value,at).await
}
fn select_stream(value:&mut OrdinaryIncomingState,stream:&str)->anyhow::Result<()> {
    let (born,generation)=ordinary_name(stream)?;
    if value.generation>0 {anyhow::ensure!(value.born==born && generation>=value.generation,OrdinaryIncomingRefusal::DomainChanged);}
    if generation!=value.generation {
        value.born=born;value.generation=generation;value.through=0;value.has=empty_has(stream);
    }
    Ok(())
}
/// Admission-only known result for the exact retained Start. Never returns a wire ACK.
pub(crate) async fn accept_ordinary_incoming_state(r:&dyn Records,nonce:&str,origin:crate::net::Connection,stream:&str,seq:u64)->anyhow::Result<OrdinaryIncomingState> {
    anyhow::ensure!(seq>0 && seq<u64::MAX,OrdinaryIncomingRefusal::Corrupt);
    for _ in 0..3 {
        let at=Consistency::Snapshot(r.head().await?);
        let old=incoming_at(r,nonce,at).await?.ok_or(OrdinaryIncomingRefusal::DomainChanged)?;
        anyhow::ensure!(old.connection==origin.channel_id(),OrdinaryIncomingRefusal::DomainChanged);
        let mut value=old.clone();select_stream(&mut value,stream)?;
        if seq<=value.through {return Ok(value);}
        anyhow::ensure!(seq==value.through.saturating_add(1),OrdinaryIncomingRefusal::MissingPrefix);value.through=seq;
        match save_incoming(r,nonce,Some(&old),value,at).await {Ok(value)=>return Ok(value),Err(error) if matches!(error.downcast_ref::<Refusal>(),Some(Refusal::Conflict{..}))=>continue,Err(error)=>return Err(error)}
    }
    anyhow::bail!("ordinary incoming admission contended; retain exact operation")
}
/// Commit ONLY the opaque SAME Receiver's contiguous completed disposition snapshot.
/// Recorded NACKs cannot silently disappear or become Taken after reply loss.
pub(crate) async fn commit_ordinary_incoming(r:&dyn Records,closure:&super::channel::OrdinaryClosure)->anyhow::Result<OrdinaryIncomingState> {
    let nonce=closure.nonce();let has=closure.has();super::channel::ordinary_has_valid(has)?;
    for _ in 0..3 {
        let at=Consistency::Snapshot(r.head().await?);
        let old=incoming_at(r,nonce,at).await?.ok_or(OrdinaryIncomingRefusal::DomainChanged)?;
        anyhow::ensure!(old.connection==closure.origin().channel_id(),OrdinaryIncomingRefusal::DomainChanged);
        let mut value=old.clone();select_stream(&mut value,&has.stream)?;
        anyhow::ensure!(has.upto>=value.has.upto && has.upto-value.has.upto<=64 && closure.low()<=value.has.upto,OrdinaryIncomingRefusal::MissingPrefix);
        for nack in value.has.nacks.iter().filter(|n|n.seq>closure.low()) {
            anyhow::ensure!(has.nacks.iter().any(|n|n==nack),OrdinaryIncomingRefusal::Corrupt);
        }
        value.through=value.through.max(has.upto);value.has=has.clone();
        if value.through==old.through && value.has==old.has && value.generation==old.generation {return Ok(old);}
        match save_incoming(r,nonce,Some(&old),value,at).await {Ok(value)=>return Ok(value),Err(error) if matches!(error.downcast_ref::<Refusal>(),Some(Refusal::Conflict{..}))=>continue,Err(error)=>return Err(error)}
    }
    anyhow::bail!("ordinary incoming completed outcome contended; retain exact closure")
}
/// An exclusive positive stopped/no-start Gate can close admission, preserving ALL
/// completed outcomes. Different persisted binding/newer generation excludes the old route,
/// but neither exclusion nor this admission-only prefix is physical proof by itself.
pub(crate) async fn close_ordinary_incoming(r:&dyn Records,nonce:&str,connection:&str,stream:&str,seq:u64)->anyhow::Result<()> {
    anyhow::ensure!(seq>0 && seq<u64::MAX && connection.len()==64 && connection.bytes().all(|b|b.is_ascii_hexdigit()),OrdinaryIncomingRefusal::Corrupt);
    for _ in 0..3 {
        let at=Consistency::Snapshot(r.head().await?);let old=incoming_at(r,nonce,at).await?;
        if let Some(old)=&old {
            if old.connection!=connection {return Ok(());}
            let (born,generation)=ordinary_name(stream)?;
            anyhow::ensure!(old.generation==0 || old.born==born,OrdinaryIncomingRefusal::DomainChanged);
            if generation<old.generation {return Ok(());}
        }
        let mut value=old.clone().unwrap_or(OrdinaryIncomingState{version:2,process_nonce:nonce.into(),connection:connection.into(),born:0,generation:0,through:0,has:empty_has(""),rev:0});
        select_stream(&mut value,stream)?;
        if seq<=value.through {return Ok(());}
        anyhow::ensure!(seq==value.through.saturating_add(1),OrdinaryIncomingRefusal::MissingPrefix);value.through=seq;
        match save_incoming(r,nonce,old.as_ref(),value,at).await {Ok(_)=>return Ok(()),Err(error) if matches!(error.downcast_ref::<Refusal>(),Some(Refusal::Conflict{..}))=>continue,Err(error)=>return Err(error)}
    }
    anyhow::bail!("ordinary incoming Gate closure contended; retain SAME proof")
}
