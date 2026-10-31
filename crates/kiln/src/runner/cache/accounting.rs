//! U07 / W08-R09: canonical physical inventory, not a second object/authorization store.
//! All entry/history bytes are cache bytes. Only OS-owned operation guards protect residency.
//! Calls require the shared mutation fence; DB jobs retain it even after caller cancellation.
use std::{collections::{BTreeMap, HashSet}, fmt, io, path::{Path, PathBuf}, sync::Arc, time::{SystemTime, UNIX_EPOCH}};
use anyhow::Context as _;
use std::io::Read as _;
use crate::sql::{params, OptionalExtension as _};
use super::guards::Mutation;

const DEFAULT_MAX: u64 = 10 << 30;
const PAGE: usize = 256;
/// Operator attestation, written only while every pre-U07 writer is stopped. Never an age lease.
const QUIESCENT: &str = "U07: all pre-format cache writers are quiescent\n";

#[derive(Debug)]
pub(super) struct Capacity { pub max: u64, pub used: u64, pub reserved: u64, pub requested: u64 }
impl fmt::Display for Capacity { fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result { write!(f,"U07 cache capacity: limit {}, used {}, reserved {}, requested {} bytes",self.max,self.used,self.reserved,self.requested) } }
impl std::error::Error for Capacity {}
#[derive(Debug)]
pub(super) struct MaintenancePending;
impl fmt::Display for MaintenancePending { fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result { f.write_str("U07 cache needs another bounded maintenance pass") } }
impl std::error::Error for MaintenancePending {}

#[derive(Clone)]
pub(super) struct Accounting { store: crate::store::Store, owner: String }
#[derive(Clone, Debug)]
pub(super) struct Reservation { pub id: String, pub want: Vec<(String,u64)> }

fn digest_valid(d:&str)->bool { d.len()==64 && d.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)) }
fn valid(key:&str)->bool {digest_valid(key.strip_prefix("manifest:").unwrap_or(key))}
fn protected_key(protected:&HashSet<String>,key:&str)->bool {protected.contains(key)||protected.contains(key.strip_prefix("manifest:").unwrap_or(key))}
fn manifest_root(root:&Path)->anyhow::Result<PathBuf> {Ok(root.parent().context("U07 cache root has no work parent")?.join("_cas/objects"))}
pub(super) fn physical_path(root:&Path,key:&str)->anyhow::Result<PathBuf> {
    anyhow::ensure!(valid(key),"invalid U07 physical inventory key");
    let manifest=key.strip_prefix("manifest:");
    let d=manifest.unwrap_or(key);
    let base=if manifest.is_some() {manifest_root(root)?} else {root.to_path_buf()};
    let file=base.join(d.get(..2).context("digest prefix")?).join(d.get(2..4).context("digest prefix")?).join(d);
    Ok(if manifest.is_some() {file.with_extension("chunks")} else {file})
}
fn path(root:&Path,key:&str)->anyhow::Result<PathBuf> {physical_path(root,key)}
fn nonempty(path:&Path,ignored:&[&str])->io::Result<bool> {
    match std::fs::read_dir(path) {
        Ok(entries)=>for entry in entries {let entry=entry?;if !ignored.iter().any(|name|entry.file_name()==*name) {return Ok(true);}},
        Err(e) if e.kind()==io::ErrorKind::NotFound=>{},Err(e)=>return Err(e),
    }
    Ok(false)
}
fn small_text(path:&Path)->anyhow::Result<String> {
    let mut text=String::new();
    std::fs::File::open(path)?.take(129).read_to_string(&mut text)?;
    anyhow::ensure!(text.len()<=128,"U07 format/quiescence marker exceeds 128 bytes");
    Ok(text)
}
fn number(n:u64)->anyhow::Result<i64> { Ok(i64::try_from(n)?) }
fn used_at(m:&std::fs::Metadata)->i64 { m.modified().ok().and_then(|t|t.duration_since(UNIX_EPOCH).ok()).and_then(|d|i64::try_from(d.as_millis()).ok()).unwrap_or(0) }
fn now()->i64 { SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|d|i64::try_from(d.as_millis()).ok()).unwrap_or(0) }

// U07: absence observed after a prior failed sync is not durable credit. Re-sync the
// nearest existing known physical ancestor on every unlink retry, even when this attempt
// found no target. Syncing that directory makes the missing child chain durable.
#[cfg(test)]
std::thread_local! {
    static FAIL_PARENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PARENT_SYNC_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn sync_existing_parent(root:&Path,file:&Path)->anyhow::Result<()> {
    let boundary=root.parent().context("U07 cache root has no work parent")?;
    let parent=file.parent().context("U07 physical file has no parent")?;
    let manifest=manifest_root(root)?;
    anyhow::ensure!(parent.starts_with(root)||parent.starts_with(&manifest),"U07 sync outside physical ancestry");
    let mut parent=parent;
    for _ in 0..5 {
        match std::fs::symlink_metadata(parent) {
            Ok(metadata)=>{
                anyhow::ensure!(metadata.is_dir(),"U07 physical ancestor is not a directory");
                return sync_directory(parent);
            },
            Err(error) if error.kind()==io::ErrorKind::NotFound=>{
                anyhow::ensure!(parent!=boundary,"U07 work parent is missing");
                parent=parent.parent().context("U07 physical ancestry exhausted")?;
                anyhow::ensure!(parent.starts_with(boundary),"U07 physical ancestry escaped work root");
            },
            Err(error)=>return Err(error.into()),
        }
    }
    anyhow::bail!("U07 physical ancestry exceeds bounded depth")
}
fn sync_directory(parent:&Path)->anyhow::Result<()> {
    #[cfg(test)]
    {
        PARENT_SYNC_ATTEMPTS.with(|count|count.set(count.get().saturating_add(1)));
        if FAIL_PARENT_SYNC.with(|fail|fail.replace(false)) {return Err(io::Error::other("injected U07 parent sync failure").into());}
    }
    crate::blobs::chunks::sync_dir(parent)?;
    Ok(())
}
pub(super) fn native_staging_path(root:&Path,id:&str)->anyhow::Result<PathBuf> {
    anyhow::ensure!(super::guards::intent_valid(id),"invalid U07 native intent identity");
    Ok(root.join(".staging").join(format!("{id}.native")))
}
pub(super) fn remove_native_staging(root:&Path,id:&str)->anyhow::Result<()> {
    remove_synced(root,&native_staging_path(root,id)?)
}
pub(super) fn remove_cdc_staging(root:&Path,key:&str,id:&str)->anyhow::Result<()> {
    anyhow::ensure!(super::guards::intent_valid(id),"invalid U07 publication intent identity");
    remove_synced(root,&path(root,key)?.with_extension(format!("{id}.tmp")))
}
fn remove_synced(root:&Path,file:&Path)->anyhow::Result<()> {
    match std::fs::remove_file(file) {Ok(())=>{},Err(error) if error.kind()==io::ErrorKind::NotFound=>{},Err(error)=>return Err(error.into())}
    sync_existing_parent(root,file)
}

impl Accounting {
    /// U07 cutover is offline: create `.u07-quiescent` with QUIESCENT's exact line only after
    /// proving every old writer stopped. No code here restarts a runner or guesses from pin age.
    pub(super) fn open(root:&Path,store:crate::store::Store,max:Option<u64>,fence:Arc<Mutation>)->anyhow::Result<Self> {
        std::fs::create_dir_all(root)?;
        let owner=root.canonicalize()?.to_string_lossy().into_owned();
        let this=Self {store,owner};
        let (key,held)=(this.owner.clone(),fence.clone());
        let known:Option<(String,bool)>=this.db(held,move|c| { Ok(c.query_row("SELECT identity,ready FROM runner_cache_tiers WHERE root=?1",[key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?) })?;
        let marker=root.join(".u07-format");
        anyhow::ensure!(known.is_some() || !marker.exists(),"U07 physical root is bound to a different metadata database");
        if marker.exists() {
            let identity=known.as_ref().map(|(identity,_)|identity).context("U07 root binding unavailable")?;
            anyhow::ensure!(small_text(&marker)?==*identity,"U07 physical root belongs to another metadata database");
        }
        if let Some((identity,true))=&known {
            anyhow::ensure!(small_text(&marker)?==*identity,"U07 physical root belongs to another metadata database");
        } else {
            // Any legacy file/pin, including an orphan publication, requires the same proof.
            let legacy_tmp=root.parent().context("cache work parent")?.join("_cas/tmp");
            let nonempty=nonempty(root,&[".mutation.lock",".u07-quiescent"])? || nonempty(&manifest_root(root)?,&[])? || nonempty(&legacy_tmp,&[])? || nonempty(&root.parent().context("cache work parent")?.join("_cas/corrupt"),&[])? || nonempty(&root.parent().context("cache work parent")?.join("_cas/peers"),&[])?;
            anyhow::ensure!(known.is_some() || !nonempty || small_text(&root.join(".u07-quiescent")).is_ok_and(|s|s==QUIESCENT),"U07 offline migration requires verified old-writer quiescence; write .u07-quiescent with exact line: {QUIESCENT}");
            if known.is_none() {
                let (key,identity,limit,held)=(this.owner.clone(),crate::util::new_id(),number(max.unwrap_or(DEFAULT_MAX))?,fence.clone());
                this.db(held,move|c| { c.execute("INSERT INTO runner_cache_tiers(root,identity,max_bytes) VALUES(?1,?2,?3)",params![key,identity,limit])?; Ok(()) })?;
            }
            this.cleanup_legacy_pins(root)?;
            this.cleanup_legacy_tmp(root)?;
            this.cleanup_legacy_corrupt(root)?;
            this.import(root,fence.clone())?;
            let (key,held)=(this.owner.clone(),fence.clone());
            let identity:String=this.db(held,move|c| { Ok(c.query_row("SELECT identity FROM runner_cache_tiers WHERE root=?1",[key],|r|r.get(0))?) })?;
            std::fs::write(&marker,identity)?;
            std::fs::File::options().read(true).write(true).open(&marker)?.sync_all()?;
            crate::blobs::chunks::sync_dir(root)?;
            let (key,held)=(this.owner.clone(),fence.clone());
            this.db(held,move|c| { c.execute("UPDATE runner_cache_tiers SET ready=1 WHERE root=?1",[key])?; Ok(()) })?;
        }
        if let Some(max)=max {this.set_max(max,fence)?;}
        Ok(this)
    }

    pub(super) fn set_max(&self,max:u64,fence:Arc<Mutation>)->anyhow::Result<()> {
        let (key,max)=(self.owner.clone(),number(max)?);
        self.db(fence,move|c| {c.execute("UPDATE runner_cache_tiers SET max_bytes=?2 WHERE root=?1",params![key,max])?;Ok(())})
    }

    /// Permanent pre-format pins never represented active leases. The attested offline
    /// cutover retires their files once; no runtime path creates or scans these markers.
    fn cleanup_legacy_pins(&self,root:&Path)->anyhow::Result<()> {
        let dir=root.join("pins");
        match std::fs::read_dir(&dir) {
            Ok(entries)=>for entry in entries {
                let entry=entry?;let name=entry.file_name().to_string_lossy().into_owned();
                anyhow::ensure!(digest_valid(&name) && entry.file_type()?.is_file(),"unknown legacy pin entry; offline cleanup requires explicit identification before activation");
                std::fs::remove_file(entry.path())?;
            },
            Err(e) if e.kind()==io::ErrorKind::NotFound=>return Ok(()),Err(e)=>return Err(e.into()),
        }
        crate::blobs::chunks::sync_dir(&dir)?;
        Ok(())
    }
    /// Old write_list and receive staging used the same 32-hex names. Only the offline
    /// quiescent import may remove these; runtime input sources are never swept by age.
    fn cleanup_legacy_tmp(&self,root:&Path)->anyhow::Result<()> {
        let dir=root.parent().context("cache work parent")?.join("_cas/tmp");
        match std::fs::read_dir(&dir) {
            Ok(entries)=>for entry in entries {
                let entry=entry?;let name=entry.file_name().to_string_lossy().into_owned();
                anyhow::ensure!(name.len()==32 && name.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)) && entry.file_type()?.is_file(),"unknown legacy _cas/tmp entry; offline cleanup requires explicit identification before activation");
                std::fs::remove_file(entry.path())?;
            },
            Err(e) if e.kind()==io::ErrorKind::NotFound=>return Ok(()),Err(e)=>return Err(e.into()),
        }
        crate::blobs::chunks::sync_dir(&dir)?;
        Ok(())
    }
    fn cleanup_legacy_corrupt(&self,root:&Path)->anyhow::Result<()> {
        // Old peer windows were optional whole-object copies outside the shared tier.
        // Their writer is removed by this cutover; retire them once alongside quarantine.
        for dir in [root.join("corrupt"),root.parent().context("cache work parent")?.join("_cas/corrupt"),root.parent().context("cache work parent")?.join("_cas/peers")] {
            match std::fs::read_dir(&dir) {
                Ok(entries)=>for entry in entries {
                    let entry=entry?;let name=entry.file_name().to_string_lossy().into_owned();
                    anyhow::ensure!(digest_valid(&name) && entry.file_type()?.is_file(),"unknown legacy corrupt/peer-cache entry; offline cleanup requires explicit identification before activation");
                    std::fs::remove_file(entry.path())?;
                    tracing::warn!(target:"kiln.work",digest=%name,path=%dir.display(),"U07 offline cutover discarded old quarantined/peer cache bytes");
                },
                Err(e) if e.kind()==io::ErrorKind::NotFound=>continue,Err(e)=>return Err(e.into()),
            }
            crate::blobs::chunks::sync_dir(&dir)?;
        }
        Ok(())
    }
    fn import(&self,root:&Path,fence:Arc<Mutation>)->anyhow::Result<()> {
        let (key,held)=(self.owner.clone(),fence.clone());
        let start:u32=self.db(held,move|c| {Ok(c.query_row("SELECT import_prefix FROM runner_cache_tiers WHERE root=?1",[key],|r|r.get(0))?)})?;
        // Two bounded prefix spaces: chunks [0,65536), derived manifests [65536,131072).
        // Only existing directories are visited; one scalar cursor resumes either phase.
        let mut prefixes=Vec::new();
        for (base,offset,manifest) in [(root.to_path_buf(),0u32,false),(manifest_root(root)?,65536,true)] {
            let entries=match std::fs::read_dir(&base) {Ok(entries)=>entries,Err(e) if e.kind()==io::ErrorKind::NotFound=>continue,Err(e)=>return Err(e.into())};
            for first in entries {
                let first=first?;let name=first.file_name().to_string_lossy().into_owned();
                let kind=first.file_type()?;
                // U07: only these physical-owner entries may sit beside chunk prefixes.
                // Manifest roots have no reserved metadata names. Unknowns are never deleted.
                if !manifest {
                    let reserved=match name.as_str() {
                        ".mutation.lock"|".u07-quiescent"|".u07-format"=>Some(kind.is_file()),
                        ".activity"|"pins"|"corrupt"=>Some(kind.is_dir()),
                        _=>None,
                    };
                    if let Some(valid)=reserved {anyhow::ensure!(valid,"invalid U07 reserved physical entry type");continue;}
                }
                anyhow::ensure!(name.len()==2 && name.bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase()) && kind.is_dir(),"unknown U07 first-prefix entry; offline identification required");
                for second in std::fs::read_dir(first.path())? {
                    let second=second?;let tail=second.file_name().to_string_lossy().into_owned();
                    anyhow::ensure!(tail.len()==2 && tail.bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase()) && second.file_type()?.is_dir(),"unknown U07 second-prefix entry; offline identification required");
                    let prefix=u32::from_str_radix(&format!("{name}{tail}"),16)?+offset;
                    if prefix>=start {prefixes.push((prefix,second.path(),manifest));}
                }
            }
        }
        prefixes.sort_by_key(|(prefix,_,_)|*prefix);
        for (position,(prefix,dir,manifest)) in prefixes.into_iter().enumerate() {
            let mut batch=Vec::new();
            for entry in std::fs::read_dir(dir)? {
                let entry=entry?;let name=entry.file_name().to_string_lossy().into_owned();
                // Whole legacy objects remain read-only migration sources until their verified
                // shared publication succeeds. They are not a second live cache writer.
                if manifest && digest_valid(&name) {continue;}
                let digest=if manifest {name.strip_suffix(".chunks").filter(|d|digest_valid(d))} else {digest_valid(&name).then_some(name.as_str())};
                let Some(digest)=digest else {
                    let owned_temp=name.strip_suffix(".tmp").and_then(|stem|stem.split_once('.')).is_some_and(|(digest,id)|digest_valid(digest)&&id.len()==32&&id.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)));
                    anyhow::ensure!(owned_temp && entry.file_type()?.is_file(),"unknown file in U07 legacy physical directory");
                    std::fs::remove_file(entry.path())?;
                    crate::blobs::chunks::sync_dir(entry.path().parent().context("legacy physical parent")?)?;
                    continue;
                };
                let inventory_key=if manifest {format!("manifest:{digest}")} else {digest.to_owned()};
                anyhow::ensure!(path(root,&inventory_key)?==entry.path(),"legacy U07 bytes have an inconsistent physical prefix");
                let m=entry.metadata()?;anyhow::ensure!(m.is_file(),"U07 legacy physical bytes are not a regular file");
                batch.push((inventory_key,m.len(),used_at(&m)));
                if batch.len()==PAGE {self.import_batch(std::mem::take(&mut batch),fence.clone())?;}
            }
            if !batch.is_empty() {self.import_batch(batch,fence.clone())?;}
            if position % PAGE == PAGE-1 {
                let (key,held)=(self.owner.clone(),fence.clone());
                self.db(held,move|c| {c.execute("UPDATE runner_cache_tiers SET import_prefix=?2 WHERE root=?1",params![key,i64::from(prefix)+1])?;Ok(())})?;
            }
        }
        let key=self.owner.clone();
        self.db(fence,move|c| {c.execute("UPDATE runner_cache_tiers SET import_prefix=131072 WHERE root=?1",[key])?;Ok(())})
    }
    fn import_batch(&self,rows:Vec<(String,u64,i64)>,fence:Arc<Mutation>)->anyhow::Result<()> {
        let key=self.owner.clone();
        self.db(fence,move|c| {let tx=crate::store::tx(c)?;
            for (d,n,t) in rows { if tx.execute("INSERT INTO runner_cache_chunks(root,digest,size,last_used) VALUES(?1,?2,?3,?4) ON CONFLICT(root,digest) DO NOTHING",params![key,d,number(n)?,t])?==1 {tx.execute("UPDATE runner_cache_tiers SET used_bytes=used_bytes+?2 WHERE root=?1",params![key,number(n)?])?;} }
            tx.commit()?;Ok(())
        })
    }

    #[cfg(test)]
    pub(super) fn prepare(&self,root:&Path,want:&[(String,u64)],repair:&HashSet<String>,protected:&HashSet<String>,fence:Arc<Mutation>)->anyhow::Result<Reservation> {
        self.prepare_plan(root,want,repair,protected,None,fence)
    }

    pub(super) fn prepare_live(&self,root:&Path,want:&[(String,u64)],repair:&HashSet<String>,protected:&HashSet<String>,live:(String,super::CacheGuard),fence:Arc<Mutation>)->anyhow::Result<Reservation> {
        let (id,guard)=live;
        anyhow::ensure!(guard.owns_intent(root,&id),"U07 CDC admission lost its exact intent lease");
        self.prepare_plan(root,want,repair,protected,Some((id,guard)),fence)
    }

    fn prepare_plan(&self,root:&Path,want:&[(String,u64)],repair:&HashSet<String>,protected:&HashSet<String>,live:Option<(String,super::CacheGuard)>,fence:Arc<Mutation>)->anyhow::Result<Reservation> {
        anyhow::ensure!(want.len()<=65537,"U07 publication exceeds 65536 chunks plus one manifest plan limit");
        let mut unique=BTreeMap::new();
        for (d,n) in want { anyhow::ensure!(valid(d),"invalid U07 chunk digest"); if let Some(old)=unique.insert(d.clone(),*n) {anyhow::ensure!(old==*n,"conflicting sizes for U07 chunk");} }
        anyhow::ensure!(repair.iter().all(|digest|!protected_key(protected,digest)),"U07 corrupt content is still protected; release its readers before replacement");
        let mut keep=protected.clone();
        keep.extend(unique.keys().map(|key|key.strip_prefix("manifest:").unwrap_or(key).to_owned()));
        // Repair compares OTHER readers; recovery/eviction retains our exact slot names,
        // so manifest:O also protects native/whole O, including incomplete deletion.
        self.recover(root,&keep,fence.clone())?;
        let mut reserve=0u64;
        // Caller verifies content before calling prepare. Existing files are charged independently
        // from the full replacement staging reservation, so corrupt repair cannot exceed the peak.
        for (d,n) in &unique {
            let physical=match path(root,d)?.metadata() {Ok(m) if m.is_file()=>Some(m.len()),Ok(_)=>anyhow::bail!("U07 chunk is not a regular file"),Err(e) if e.kind()==io::ErrorKind::NotFound=>None,Err(e)=>return Err(e.into())};
            if physical!=Some(*n)||repair.contains(d) {reserve=reserve.checked_add(*n).context("cache reservation overflow")?;}
        }
        let footprint=unique.values().try_fold(0u64,|a,n|a.checked_add(*n).context("cache footprint overflow"))?;
        let (limit,used,reserved)=self.totals(fence.clone())?;
        if footprint>limit && reserve>0 {return Err(Capacity{max:limit,used,reserved,requested:footprint}.into());}
        self.refresh_many(root,&unique.keys().cloned().collect::<Vec<_>>(),fence.clone())?;
        if reserve>0 {self.make_room(root,reserve,&keep,fence.clone())?;}
        let (id,guard,version)=match live {Some((id,guard))=>(id,Some(guard),3),None=>(crate::util::new_id(),None,1)};
        let reservation=Reservation{id,want:unique.into_iter().collect()};
        let (key,id,chunks,n)=(self.owner.clone(),reservation.id.clone(),serde_json::to_string(&reservation.want)?,number(reserve)?);
        self.db_owned(fence,guard,move|c| {let tx=crate::store::tx(c)?;
            let (limit,used,reserved):(i64,i64,i64)=tx.query_row("SELECT max_bytes,used_bytes,reserved_bytes FROM runner_cache_tiers WHERE root=?1",[&key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            anyhow::ensure!(n==0 || used.checked_add(reserved).and_then(|v|v.checked_add(n)).is_some_and(|v|v<=limit),"U07 admission changed under physical fence");
            tx.execute("INSERT INTO runner_cache_intents(root,id,reserved_bytes,chunks,plan_version) VALUES(?1,?2,?3,?4,?5)",params![key,id,n,chunks,version])?;
            tx.execute("UPDATE runner_cache_tiers SET reserved_bytes=reserved_bytes+?2 WHERE root=?1",params![key,n])?;tx.commit()?;Ok(())
        })?;
        Ok(reservation)
    }

    /// U07 host-only native writes reserve their entire finite staging extent. Native
    /// bytes and chunks share the same digest inventory; no second cache or LRU exists.
    pub(super) fn prepare_native(&self,root:&Path,plan:&super::NativePlan,id:String,protected:&HashSet<String>,fence:Arc<Mutation>,guard:super::CacheGuard)->anyhow::Result<Reservation> {
        plan.validate()?;
        anyhow::ensure!(guard.owns_intent(root,&id),"U07 native admission lost its exact intent lease");
        anyhow::ensure!(super::guards::intent_valid(&id),"invalid U07 native intent identity");
        self.recover(root,protected,fence.clone())?;
        self.refresh_many(root,&[plan.digest().to_owned()],fence.clone())?;
        let mut keep=protected.clone(); keep.insert(plan.digest().to_owned());
        self.make_room(root,plan.len(),&keep,fence.clone())?;
        let reservation=Reservation{id,want:vec![(plan.digest().to_owned(),plan.len())]};
        let (key,id,json,n)=(self.owner.clone(),reservation.id.clone(),serde_json::to_string(plan)?,number(plan.len())?);
        self.db_owned(fence,Some(guard),move|c| {let tx=crate::store::tx(c)?;
            let (limit,used,reserved):(i64,i64,i64)=tx.query_row("SELECT max_bytes,used_bytes,reserved_bytes FROM runner_cache_tiers WHERE root=?1",[&key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            anyhow::ensure!(used.checked_add(reserved).and_then(|v|v.checked_add(n)).is_some_and(|v|v<=limit),"U07 native admission changed under physical fence");
            tx.execute("INSERT INTO runner_cache_intents(root,id,reserved_bytes,chunks,plan_version) VALUES(?1,?2,?3,?4,2)",params![key,id,n,json])?;
            tx.execute("UPDATE runner_cache_tiers SET reserved_bytes=reserved_bytes+?2 WHERE root=?1",params![key,n])?;
            tx.commit()?;Ok(())
        })?;
        Ok(reservation)
    }

    pub(super) fn settle(&self,root:&Path,reservation:&Reservation,fence:Arc<Mutation>)->anyhow::Result<()> {
        self.settle_owned(root,reservation,fence,None)
    }
    pub(super) fn settle_live(&self,root:&Path,reservation:&Reservation,fence:Arc<Mutation>,guard:super::CacheGuard)->anyhow::Result<()> {
        anyhow::ensure!(guard.owns_intent(root,&reservation.id),"U07 settlement lost its exact intent lease");
        self.settle_owned(root,reservation,fence,Some(guard))
    }
    fn settle_owned(&self,root:&Path,reservation:&Reservation,fence:Arc<Mutation>,guard:Option<super::CacheGuard>)->anyhow::Result<()> {
        let rows=Self::snapshot(root,&reservation.want.iter().map(|(d,_)|d.clone()).collect::<Vec<_>>())?;
        let (key,id)=(self.owner.clone(),reservation.id.clone());
        self.db_owned(fence,guard,move|c| {let tx=crate::store::tx(c)?;
            Self::apply_snapshot(&tx,&key,rows)?;
            let n:Option<i64>=tx.query_row("SELECT reserved_bytes FROM runner_cache_intents WHERE root=?1 AND id=?2",params![key,id],|r|r.get(0)).optional()?;
            if let Some(n)=n {tx.execute("DELETE FROM runner_cache_intents WHERE root=?1 AND id=?2",params![key,id])?;tx.execute("UPDATE runner_cache_tiers SET reserved_bytes=reserved_bytes-?2 WHERE root=?1",params![key,n])?;}
            tx.commit()?;Ok(())
        })
    }
    /// Legacy v1 retains Mutation through writes; native v2 and CDC v3 retain an exact
    /// intent in the same OS slots. Mutation alone cannot reclaim a live producer.
    /// Unknown producer/plan types fail closed.
    pub(super) fn recover(&self,root:&Path,protected:&HashSet<String>,fence:Arc<Mutation>)->anyhow::Result<()> {
        let activity=super::guards::activity(root,&fence,None)?;
        let key=self.owner.clone();
        let pending:Vec<(String,String,i64)>=self.db(fence.clone(),move|c| {
            let mut q=c.prepare("SELECT id,chunks,plan_version FROM runner_cache_intents WHERE root=?1 ORDER BY id LIMIT 257")?;
            Ok(q.query_map([key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<crate::sql::Result<_>>()?)
        })?;
        let more=pending.len()>PAGE;
        // The same 64 guard slots are fewer than this 256-row page. Even a full active
        // native cohort cannot hide all abandoned intents; no new durable cursor is needed.
        anyhow::ensure!(activity.intents.len()<PAGE,"U07 active intent count exceeds bounded recovery progress");
        for (id,json,version) in pending.into_iter().take(PAGE) {
            match version {
                1|3=>{
                    let chunks:Vec<(String,u64)>=serde_json::from_str(&json)?;
                    anyhow::ensure!(super::guards::intent_valid(&id),"invalid U07 publication intent identity");
                    if version==3 && activity.intents.contains(&id) {continue;}
                    for (d,_) in &chunks {remove_cdc_staging(root,d,&id)?;}
                    self.settle(root,&Reservation{id,want:chunks},fence.clone())?;
                },
                2=>{
                    let plan:super::NativePlan=serde_json::from_str(&json)?;
                    plan.validate()?;
                    if activity.intents.contains(&id) {continue;}
                    remove_native_staging(root,&id)?;
                    self.settle(root,&Reservation{id,want:vec![(plan.digest().to_owned(),plan.len())]},fence.clone())?;
                },
                _=>anyhow::bail!("unknown U07 cache intent plan version {version}; reservation remains fenced"),
            }
        }
        // Do not retire resident inventory before every remaining producer plan was
        // recognized. An unknown use plan beyond this page must still fence deletion.
        if more {return Err(MaintenancePending.into());}
        let key=self.owner.clone();
        let deleting:Vec<String>=self.db(fence.clone(),move|c| {let mut q=c.prepare("SELECT digest FROM runner_cache_chunks WHERE root=?1 AND deleting=1 LIMIT 256")?;Ok(q.query_map([key],|r|r.get(0))?.collect::<crate::sql::Result<_>>()?)})?;
        for d in deleting {if !protected_key(protected,&d) {self.delete(root,&d,fence.clone())?;}}
        Ok(())
    }
    pub(super) fn trim(&self,root:&Path,protected:&HashSet<String>,fence:Arc<Mutation>)->anyhow::Result<()> {self.recover(root,protected,fence.clone())?; self.make_room(root,0,protected,fence)}
    fn totals(&self,fence:Arc<Mutation>)->anyhow::Result<(u64,u64,u64)> {
        let key=self.owner.clone();self.db(fence,move|c|Ok(c.query_row("SELECT max_bytes,used_bytes,reserved_bytes FROM runner_cache_tiers WHERE root=?1",[key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?))
    }
    fn refresh(&self,root:&Path,d:&str,fence:Arc<Mutation>)->anyhow::Result<()> {self.refresh_many(root,&[d.to_owned()],fence)}
    fn snapshot(root:&Path,digests:&[String])->anyhow::Result<Vec<(String,i64,i64)>> {
        let mut rows=Vec::with_capacity(digests.len());
        for d in digests {
            let file=path(root,d)?;
            let (n,t)=match file.metadata() {Ok(m) if m.is_file()=>(m.len(),used_at(&m)),Ok(_)=>anyhow::bail!("U07 chunk is not a regular file"),Err(e) if e.kind()==io::ErrorKind::NotFound=>(0,now()),Err(e)=>return Err(e.into())};
            rows.push((d.clone(),number(n)?,t));
        }
        Ok(rows)
    }
    fn apply_snapshot(c:&crate::sql::Connection<'_>,key:&str,rows:Vec<(String,i64,i64)>)->anyhow::Result<()> {
        for (d,n,t) in rows {
            let old:i64=c.query_row("SELECT size FROM runner_cache_chunks WHERE root=?1 AND digest=?2",params![key,d],|r|r.get(0)).optional()?.unwrap_or(0);
            c.execute("UPDATE runner_cache_tiers SET used_bytes=used_bytes+?2 WHERE root=?1",params![key,n-old])?;
            if n==0 {c.execute("DELETE FROM runner_cache_chunks WHERE root=?1 AND digest=?2",params![key,d])?;} else {c.execute("INSERT INTO runner_cache_chunks(root,digest,size,last_used,deleting) VALUES(?1,?2,?3,?4,0) ON CONFLICT(root,digest) DO UPDATE SET size=excluded.size,last_used=excluded.last_used,deleting=0",params![key,d,n,t])?;}
        }
        Ok(())
    }
    fn refresh_many(&self,root:&Path,digests:&[String],fence:Arc<Mutation>)->anyhow::Result<()> {
        let rows=Self::snapshot(root,digests)?;
        let key=self.owner.clone();
        self.db(fence,move|c| {let tx=crate::store::tx(c)?;
            Self::apply_snapshot(&tx,&key,rows)?;
            tx.commit()?;Ok(())
        })
    }
    fn make_room(&self,root:&Path,need:u64,protected:&HashSet<String>,fence:Arc<Mutation>)->anyhow::Result<()> {
        let (max,used,reserved)=self.totals(fence.clone())?;
        if used.checked_add(reserved).and_then(|n|n.checked_add(need)).is_some_and(|n|n<=max) {return Ok(());}
        let key=self.owner.clone();
        let (candidates,continued):(Vec<(String,i64)>,bool)=self.db(fence.clone(),move|c| {
            let tx=crate::store::tx(c)?;
            let cursor:(Option<i64>,Option<String>)=tx.query_row("SELECT evict_time,evict_digest FROM runner_cache_tiers WHERE root=?1",[&key],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let continued=cursor.0.is_some();
            let (time,digest)=(cursor.0.unwrap_or(i64::MIN),cursor.1.unwrap_or_default());
            let rows:Vec<(String,i64)>={let mut q=tx.prepare("SELECT digest,last_used FROM runner_cache_chunks WHERE root=?1 AND deleting=0 AND (last_used,digest)>(?2,?3) ORDER BY last_used,digest LIMIT 256")?;q.query_map(params![key,time,digest],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<crate::sql::Result<_>>()?};
            if rows.len()==PAGE {
                let (d,t)=rows.last().context("full LRU page")?;
                tx.execute("UPDATE runner_cache_tiers SET evict_time=?2,evict_digest=?3 WHERE root=?1",params![key,t,d])?;
            } else {tx.execute("UPDATE runner_cache_tiers SET evict_time=NULL,evict_digest=NULL WHERE root=?1",[&key])?;}
            tx.commit()?;Ok((rows,continued))
        })?;
        let full=candidates.len()==PAGE;
        let complete=!full && !continued;
        let mut refreshed=false;
        let mut eligible=Vec::with_capacity(candidates.len());
        for (d,old_time) in candidates {
            if protected_key(protected,&d) {continue;}
            let time=match path(root,&d)?.metadata() {
                Ok(m)=>used_at(&m),Err(e) if e.kind()==io::ErrorKind::NotFound=>old_time,Err(e)=>return Err(e.into()),
            };
            if time>old_time {
                self.refresh(root,&d,fence.clone())?;
                // U07: a complete short page can re-order touched candidates and reclaim
                // in this SAME bounded pass. Partial pages must yield to unseen older rows.
                if !complete {refreshed=true;continue;}
            }
            eligible.push((d,time));
        }
        eligible.sort_by(|(a,at),(b,bt)|at.cmp(bt).then_with(||a.cmp(b)));
        for (d,_) in eligible {
            self.delete(root,&d,fence.clone())?;
            let (max,used,reserved)=self.totals(fence.clone())?;
            if used.checked_add(reserved).and_then(|n|n.checked_add(need)).is_some_and(|n|n<=max) {return Ok(());}
        }
        if full || continued || refreshed {return Err(MaintenancePending.into());}
        let (max,used,reserved)=self.totals(fence)?;
        Err(Capacity{max,used,reserved,requested:need}.into())
    }
    fn delete(&self,root:&Path,d:&str,fence:Arc<Mutation>)->anyhow::Result<()> {
        let (key,digest)=(self.owner.clone(),d.to_owned());
        self.db(fence.clone(),move|c| {c.execute("UPDATE runner_cache_chunks SET deleting=1 WHERE root=?1 AND digest=?2",params![key,digest])?;Ok(())})?;
        remove_synced(root,&path(root,d)?)?;
        self.refresh(root,d,fence)
    }
    /// The caller proved corruption under this same fence. Delete, never accumulate an
    /// unaccounted permanent quarantine; credit follows durable physical removal.
    pub(super) fn discard(&self,root:&Path,key:&str,fence:Arc<Mutation>)->anyhow::Result<()> {
        // Unknown future producer/use plans must also fence direct corruption retirement.
        let protected=super::guards::protected(root,&fence)?;
        self.recover(root,&protected,fence.clone())?;
        self.delete(root,key,fence)
    }
    /// Reconcile a caller's already completed quarantine/removal. Never release credit before
    /// examining the actual path under the physical fence.
    pub(super) fn forget(&self,root:&Path,digest:&str,fence:Arc<Mutation>)->anyhow::Result<()> {
        let protected=super::guards::protected(root,&fence)?;
        self.recover(root,&protected,fence.clone())?;
        let file=path(root,digest)?;
        match file.metadata() {Ok(_)=>{},Err(error) if error.kind()==io::ErrorKind::NotFound=>sync_existing_parent(root,&file)?,Err(error)=>return Err(error.into())}
        self.refresh(root,digest,fence)
    }

    // Return the fence *with* the SQL outcome, keeping it owned through durable acknowledgement.
    // Every multi-statement mutation above uses its own transaction, including on closure error.
    fn db<R:Send+'static>(&self,fence:Arc<Mutation>,f:impl FnOnce(&mut crate::sql::Connection<'_>)->anyhow::Result<R>+Send+'static)->anyhow::Result<R> {
        self.db_owned(fence,None,f)
    }
    fn db_owned<R:Send+'static>(&self,fence:Arc<Mutation>,guard:Option<super::CacheGuard>,f:impl FnOnce(&mut crate::sql::Connection<'_>)->anyhow::Result<R>+Send+'static)->anyhow::Result<R> {
        // Return both owners even on an inner error. UnknownOutcome may precede the real
        // callback: a queued closure/result, not the caller, owns their final release.
        let (outcome,_fence,_guard)=self.store.now(move|c|Ok((f(c),fence,guard)))?;
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn u07_unique_footprint_validates_names_and_sizes()->anyhow::Result<()> {
        assert!(!valid("../outside"));
        assert!(!valid(&format!("other:{}","a".repeat(64))));
        assert!(valid(&format!("manifest:{}","a".repeat(64))));
        let d="a".repeat(64);
        let root=Path::new("cache");
        assert_eq!(path(root,&d)?,root.join("aa").join("aa").join(&d));
        assert!(number(u64::MAX).is_err());
        assert!(super::super::NativePlan::new(d, u64::MAX).is_err());
        for producer in ["external", "generated"] {
            assert!(serde_json::from_value::<super::super::NativePlan>(serde_json::json!({"kind":"native_file","producer":producer,"digest":"a".repeat(64),"bytes":1})).is_err());
        }
        Ok(())
    }
    struct Fixture {root:PathBuf, accounting:Accounting, fence:Arc<Mutation>}
    impl Fixture {
        fn new(max:u64)->anyhow::Result<Self> {
            let root=std::env::var_os("HOME").or_else(||std::env::var_os("USERPROFILE")).map(PathBuf::from).context("HOME for U07 test cache")?.join(".cache/kiln-tests/u07").join(crate::util::new_id()).join("_cache");
            std::fs::create_dir_all(&root)?;
            let fence=Arc::new(Mutation::acquire(&root)?);
            let accounting=Accounting::open(&root,crate::store::Store::in_memory()?,Some(max),fence.clone())?;
            Ok(Self{root,accounting,fence})
        }
        fn write(&self,d:&str,data:&[u8])->anyhow::Result<()> {let p=path(&self.root,d)?;std::fs::create_dir_all(p.parent().context("chunk parent")?)?;std::fs::write(p,data)?;Ok(())}
    }
    impl Drop for Fixture {fn drop(&mut self) {if let Some(work)=self.root.parent() {let _=std::fs::remove_dir_all(work);}}}
    #[test]
    fn u07_admission_deduplicates_and_idle_current_bytes_remain_evictable()->anyhow::Result<()> {
        let f=Fixture::new(4)?;
        let a="a".repeat(64);let b="b".repeat(64);let empty=HashSet::new();
        let reservation=f.accounting.prepare(&f.root,&[(a.clone(),4),(a.clone(),4)],&empty,&empty,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,4));
        f.write(&a,b"data")?;f.accounting.settle(&f.root,&reservation,f.fence.clone())?;
        let protected=HashSet::from([a.clone()]);
        let error=f.accounting.prepare(&f.root,&[(b.clone(),1)],&empty,&protected,f.fence.clone()).err().context("active bytes must refuse growth")?;
        assert!(error.downcast_ref::<Capacity>().is_some());
        let dedup=f.accounting.prepare(&f.root,&[(a.clone(),4)],&empty,&protected,f.fence.clone())?;
        f.accounting.settle(&f.root,&dedup,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,4,0));
        // U07: lazy hit touches must not force a spurious extra pass for a complete
        // short inventory. This is the public admission failure observed in r50.
        std::fs::File::options().write(true).open(path(&f.root,&a)?)?.set_modified(SystemTime::now()+std::time::Duration::from_secs(1))?;
        let next=f.accounting.prepare(&f.root,&[(b.clone(),1)],&empty,&empty,f.fence.clone())?;
        assert!(!path(&f.root,&a)?.exists());
        f.write(&b,b"b")?;f.accounting.settle(&f.root,&next,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,1,0));
        Ok(())
    }
    #[test]
    fn u07_repair_peak_and_oversize_refuse_before_writes_but_overage_dedup_works()->anyhow::Result<()> {
        let f=Fixture::new(4)?;let d="c".repeat(64);let empty=HashSet::new();
        let reservation=f.accounting.prepare(&f.root,&[(d.clone(),4)],&empty,&empty,f.fence.clone())?;
        f.write(&d,b"data")?;f.accounting.settle(&f.root,&reservation,f.fence.clone())?;
        let repair=HashSet::from([d.clone()]);
        let error=f.accounting.prepare(&f.root,&[(d.clone(),4)],&repair,&empty,f.fence.clone()).err().context("repair staging peak must be charged")?;
        assert!(error.downcast_ref::<Capacity>().is_some());
        f.accounting.set_max(2,f.fence.clone())?;
        let dedup=f.accounting.prepare(&f.root,&[(d.clone(),4)],&empty,&empty,f.fence.clone())?;
        f.accounting.settle(&f.root,&dedup,f.fence.clone())?;
        let error=f.accounting.prepare(&f.root,&[("d".repeat(64),3)],&empty,&empty,f.fence.clone()).err().context("oversized object must refuse")?;
        assert!(error.downcast_ref::<Capacity>().is_some());
        Ok(())
    }
    #[test]
    fn u07_recovery_removes_known_staging_before_returning_reservation_credit()->anyhow::Result<()> {
        let f=Fixture::new(4)?;let d="e".repeat(64);let empty=HashSet::new();
        let reservation=f.accounting.prepare(&f.root,&[(d.clone(),4)],&empty,&empty,f.fence.clone())?;
        let file=path(&f.root,&d)?;std::fs::create_dir_all(file.parent().context("chunk parent")?)?;
        let temporary=file.with_extension(format!("{}.tmp",reservation.id));std::fs::write(&temporary,b"part")?;
        f.accounting.recover(&f.root,&empty,f.fence.clone())?;
        assert!(!temporary.exists());assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,0));
        let id=crate::util::new_id();
        let plan=super::super::NativePlan::new(d,4)?;
        let active=super::super::guards::protect_intent(&f.root,&f.fence,plan.digest(),&id)?;
        let clone=active.clone();
        f.accounting.prepare_native(&f.root,&plan,id.clone(),&empty,f.fence.clone(),active.clone())?;
        let staged=native_staging_path(&f.root,&id)?;
        std::fs::create_dir_all(staged.parent().context("native staging parent")?)?;
        std::fs::write(&staged,b"part")?;
        drop(active);
        f.accounting.recover(&f.root,&empty,f.fence.clone())?;
        assert!(staged.exists());assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,4));
        drop(clone);
        FAIL_PARENT_SYNC.with(|fail|fail.set(true));
        assert!(f.accounting.recover(&f.root,&empty,f.fence.clone()).is_err());
        assert!(!staged.exists());assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,4));
        f.accounting.recover(&f.root,&empty,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,0));
        // ctl-10: live multi-part CDC intent survives Mutation acquisition and caller drop.
        let id=crate::util::new_id();
        let chunks=vec![("1".repeat(64),2),("2".repeat(64),2)];
        let digests:Vec<_>=chunks.iter().map(|(d,_)|d.clone()).collect();
        let active=super::super::guards::protect_cdc(&f.root,&f.fence,&digests,&id)?;
        let clone=active.clone();
        let admitted=f.accounting.prepare_live(&f.root,&chunks,&empty,&empty,(id,active.clone()),f.fence.clone())?;
        for (d,_) in &chunks {let tmp=path(&f.root,d)?.with_extension(format!("{}.tmp",admitted.id));std::fs::create_dir_all(tmp.parent().context("CDC staging parent")?)?;std::fs::write(tmp,b"io")?;}
        drop(active);
        f.accounting.recover(&f.root,&empty,f.fence.clone())?;
        for (d,_) in &chunks {assert!(path(&f.root,d)?.with_extension(format!("{}.tmp",admitted.id)).exists());}
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,4));
        drop(clone);
        FAIL_PARENT_SYNC.with(|fail|fail.set(true));
        assert!(f.accounting.recover(&f.root,&empty,f.fence.clone()).is_err());
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,4));
        f.accounting.recover(&f.root,&empty,f.fence.clone())?;
        for (d,_) in &chunks {assert!(!path(&f.root,d)?.with_extension(format!("{}.tmp",admitted.id)).exists());}
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,0));
        let key=f.accounting.owner.clone();
        f.accounting.db(f.fence.clone(),move|c| {
            c.execute("INSERT INTO runner_cache_intents(root,id,reserved_bytes,chunks,plan_version) VALUES(?1,?2,0,'{}',99)",params![key,crate::util::new_id()])?;Ok(())
        })?;
        assert!(f.accounting.recover(&f.root,&empty,f.fence.clone()).is_err(),"unknown native plans must remain fenced");
        let kept="f".repeat(64);f.write(&kept,b"kept")?;
        assert!(f.accounting.discard(&f.root,&kept,f.fence.clone()).is_err());
        assert_eq!(std::fs::read(path(&f.root,&kept)?)?,b"kept","direct retirement cannot bypass an unknown producer plan");
        Ok(())
    }

    #[test]
    fn u07_sync_failure_keeps_credit_until_missing_target_retry_is_synced()->anyhow::Result<()> {
        let f=Fixture::new(4)?;
        let digest="7".repeat(64);
        let empty=HashSet::new();
        let admitted=f.accounting.prepare(&f.root,&[(digest.clone(),4)],&empty,&empty,f.fence.clone())?;
        f.write(&digest,b"data")?;
        f.accounting.settle(&f.root,&admitted,f.fence.clone())?;
        let before=PARENT_SYNC_ATTEMPTS.with(std::cell::Cell::get);
        FAIL_PARENT_SYNC.with(|fail|fail.set(true));
        assert!(f.accounting.discard(&f.root,&digest,f.fence.clone()).is_err());
        assert!(!path(&f.root,&digest)?.exists());
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,4,0));
        // The target is already missing. A retry must still try (and can fail) directory sync.
        FAIL_PARENT_SYNC.with(|fail|fail.set(true));
        assert!(f.accounting.recover(&f.root,&empty,f.fence.clone()).is_err());
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,4,0));
        assert_eq!(PARENT_SYNC_ATTEMPTS.with(std::cell::Cell::get),before+2);
        f.accounting.recover(&f.root,&empty,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,0));
        assert_eq!(PARENT_SYNC_ATTEMPTS.with(std::cell::Cell::get),before+3);
        // The same retry rule applies to known publication staging before reservation release.
        let staged=f.accounting.prepare(&f.root,&[(digest.clone(),4)],&empty,&empty,f.fence.clone())?;
        let temporary=path(&f.root,&digest)?.with_extension(format!("{}.tmp",staged.id));
        std::fs::write(&temporary,b"part")?;
        FAIL_PARENT_SYNC.with(|fail|fail.set(true));
        assert!(f.accounting.recover(&f.root,&empty,f.fence.clone()).is_err());
        assert!(!temporary.exists());
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,4));
        FAIL_PARENT_SYNC.with(|fail|fail.set(true));
        assert!(f.accounting.recover(&f.root,&empty,f.fence.clone()).is_err());
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,4));
        f.accounting.recover(&f.root,&empty,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,0));
        // Crash before creating any chunk parent: ancestor sync must release the reservation.
        let other="8".repeat(64);
        let _pending=f.accounting.prepare(&f.root,&[(other,1)],&empty,&empty,f.fence.clone())?;
        assert!(!path(&f.root,&"8".repeat(64))?.parent().context("test parent")?.exists());
        f.accounting.recover(&f.root,&empty,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(4,0,0));
        Ok(())
    }

    #[test]
    fn u07_legacy_orphan_pins_require_quiescence_and_do_not_create_retention()->anyhow::Result<()> {
        let parent=Fixture::new(1)?;let root=parent.root.join("legacy");
        let d="f".repeat(64);let file=path(&root,&d)?;
        std::fs::create_dir_all(file.parent().context("legacy parent")?)?;
        std::fs::write(&file,b"old")?;
        std::fs::create_dir_all(root.join("pins"))?;
        std::fs::write(root.join("pins").join(&d),b"")?;
        std::fs::write(root.join("pins").join("0".repeat(64)),b"")?;
        let fence=Arc::new(Mutation::acquire(&root)?);let store=crate::store::Store::in_memory()?;
        assert!(Accounting::open(&root,store.clone(),Some(0),fence.clone()).is_err());
        std::fs::write(root.join(".u07-quiescent"),QUIESCENT)?;
        let accounting=Accounting::open(&root,store,Some(0),fence.clone())?;
        assert!(!root.join("pins").join(&d).exists());
        assert!(!root.join("pins").join("0".repeat(64)).exists());
        assert_eq!(accounting.totals(fence.clone())?,(0,3,0));
        accounting.trim(&root,&HashSet::new(),fence.clone())?;
        assert!(!file.exists());
        assert!(Accounting::open(&root,crate::store::Store::in_memory()?,None,fence).is_err());
        Ok(())
    }

    #[test]
    fn u07_bounded_lru_cursor_progresses_past_300_active_oldest_chunks()->anyhow::Result<()> {
        let f=Fixture::new(301)?;let empty=HashSet::new();
        let want:Vec<_>=(0u32..301).map(|i|(format!("{i:064x}"),1u64)).collect();
        let initial=f.accounting.prepare(&f.root,&want,&empty,&empty,f.fence.clone())?;
        for (position,(d,_)) in want.iter().enumerate() {
            f.write(d,b"x")?;
            std::fs::File::options().write(true).open(path(&f.root,d)?)?.set_modified(UNIX_EPOCH+std::time::Duration::from_secs(u64::try_from(position)?+1))?;
        }
        f.accounting.settle(&f.root,&initial,f.fence.clone())?;
        let protected:HashSet<_>=want.iter().take(300).map(|(d,_)|d.clone()).collect();
        let next=vec![(format!("{:064x}",301u32),1)];
        let first=f.accounting.prepare(&f.root,&next,&empty,&protected,f.fence.clone()).err().context("first bounded page contains only active bytes")?;
        assert!(first.downcast_ref::<MaintenancePending>().is_some());
        let admitted=f.accounting.prepare(&f.root,&next,&empty,&protected,f.fence.clone())?;
        assert!(!path(&f.root,&format!("{:064x}",300u32))?.exists());
        assert!(path(&f.root,&format!("{:064x}",0u32))?.exists());
        f.write(&format!("{:064x}",301u32),b"y")?;
        f.accounting.settle(&f.root,&admitted,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(301,301,0));
        Ok(())
    }

    #[test]
    fn u07_manifest_growth_protection_and_failed_write_staging_share_budget()->anyhow::Result<()> {
        let f=Fixture::new(12)?;let empty=HashSet::new();
        let chunk="a".repeat(64);let object="b".repeat(64);let manifest=format!("manifest:{object}");
        let first=f.accounting.prepare(&f.root,&[(chunk.clone(),4),(manifest.clone(),2)],&empty,&empty,f.fence.clone())?;
        f.write(&chunk,b"data")?;f.write(&manifest,b"[]")?;
        f.accounting.settle(&f.root,&first,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(12,6,0));
        let next_manifest=format!("manifest:{}","c".repeat(64));
        let active=HashSet::from([chunk.clone(),object]);
        let error=f.accounting.prepare(&f.root,&[(next_manifest.clone(),7)],&empty,&active,f.fence.clone()).err().context("active manifest must consume protected capacity")?;
        assert!(error.downcast_ref::<Capacity>().is_some());
        assert!(physical_path(&f.root,&manifest)?.exists());
        let protected_chunk=HashSet::from([chunk.clone()]);
        let next=f.accounting.prepare(&f.root,&[(next_manifest.clone(),7)],&empty,&protected_chunk,f.fence.clone())?;
        assert!(!physical_path(&f.root,&manifest)?.exists());
        assert!(physical_path(&f.root,&chunk)?.exists());
        f.write(&next_manifest,b"[1,2,3]")?;
        f.accounting.settle(&f.root,&next,f.fence.clone())?;
        assert_eq!(f.accounting.totals(f.fence.clone())?,(12,11,0));
        let failed_manifest=format!("manifest:{}","d".repeat(64));
        let pending=f.accounting.prepare(&f.root,&[(failed_manifest.clone(),2)],&empty,&protected_chunk,f.fence.clone())?;
        let temporary=physical_path(&f.root,&failed_manifest)?.with_extension(format!("{}.tmp",pending.id));
        std::fs::create_dir_all(temporary.parent().context("manifest parent")?)?;
        std::fs::write(&temporary,b"[]")?;
        f.accounting.recover(&f.root,&protected_chunk,f.fence.clone())?;
        assert!(!temporary.exists());
        assert_eq!(f.accounting.totals(f.fence.clone())?,(12,4,0));
        // ctl-10: a manifest-only plan owns raw O too, exactly like its normalized slot.
        // Exercise both a pending deletion and LRU pressure with no OTHER reader guard.
        for (deleting,limit) in [(true,6),(false,5)] {
            let same=Fixture::new(limit)?;
            let object="e".repeat(64);let manifest=format!("manifest:{object}");
            let initial=same.accounting.prepare(&same.root,&[(object.clone(),4)],&empty,&empty,same.fence.clone())?;
            same.write(&object,b"body")?;
            same.accounting.settle(&same.root,&initial,same.fence.clone())?;
            if deleting {
                let (root,digest)=(same.accounting.owner.clone(),object.clone());
                same.accounting.db(same.fence.clone(),move|c| {c.execute("UPDATE runner_cache_chunks SET deleting=1 WHERE root=?1 AND digest=?2",params![root,digest])?;Ok(())})?;
            }
            let id=crate::util::new_id();
            let own=super::super::guards::protect_cdc(&same.root,&same.fence,std::slice::from_ref(&object),&id)?;
            let result=same.accounting.prepare_live(&same.root,&[(manifest.clone(),2)],&empty,&empty,(id,own.clone()),same.fence.clone());
            assert_eq!(std::fs::read(path(&same.root,&object)?)?,b"body","same-slot protection was lost: deleting={deleting}");
            if deleting {
                let admitted=result?;
                assert_eq!(same.accounting.totals(same.fence.clone())?,(6,4,2));
                same.write(&manifest,b"[]")?;
                same.accounting.settle_live(&same.root,&admitted,same.fence.clone(),own.clone())?;
                drop(own);
                same.accounting.recover(&same.root,&empty,same.fence.clone())?;
                assert!(!path(&same.root,&object)?.exists(),"deletion resumes only after the actual own lease is gone");
                assert_eq!(same.accounting.totals(same.fence.clone())?,(6,2,0));
            } else {
                let error=result.err().context("LRU evicted the manifest's own raw/native digest")?;
                assert!(error.downcast_ref::<Capacity>().is_some(),"{error:#}");
                assert_eq!(same.accounting.totals(same.fence.clone())?,(5,4,0));
            }
        }
        Ok(())
    }
    #[test]
    fn u07_manifest_only_legacy_root_requires_quiescence_and_cleans_known_orphans()->anyhow::Result<()> {
        let parent=Fixture::new(1)?;let work=parent.root.join("legacy-work");let root=work.join("_cache");
        std::fs::create_dir_all(&root)?;
        let object="e".repeat(64);let key=format!("manifest:{object}");let file=physical_path(&root,&key)?;
        std::fs::create_dir_all(file.parent().context("manifest parent")?)?;std::fs::write(&file,b"[12]")?;
        let old_source=file.with_extension("");std::fs::write(&old_source,b"legacy source")?;
        let tmp=work.join("_cas/tmp");std::fs::create_dir_all(&tmp)?;let orphan=tmp.join("a".repeat(32));std::fs::write(&orphan,b"failed manifest")?;
        let corrupt=work.join("_cas/corrupt");std::fs::create_dir_all(&corrupt)?;let bad=corrupt.join("b".repeat(64));std::fs::write(&bad,b"old corrupt bytes")?;
        let peers=work.join("_cas/peers");std::fs::create_dir_all(&peers)?;let peer=peers.join("c".repeat(64));std::fs::write(&peer,b"old peer window")?;
        let fence=Arc::new(Mutation::acquire(&root)?);let store=crate::store::Store::in_memory()?;
        assert!(Accounting::open(&root,store.clone(),Some(0),fence.clone()).is_err());
        assert!(orphan.exists());assert!(bad.exists());assert!(peer.exists());
        std::fs::write(root.join(".u07-quiescent"),QUIESCENT)?;
        let accounting=Accounting::open(&root,store,Some(0),fence.clone())?;
        assert!(!orphan.exists());assert!(!bad.exists());assert!(!peer.exists());assert!(old_source.exists());
        assert_eq!(accounting.totals(fence.clone())?,(0,4,0));
        let active=HashSet::from([object]);
        let error=accounting.trim(&root,&active,fence.clone()).err().context("object operation protects its assembly")?;
        assert!(error.downcast_ref::<Capacity>().is_some());
        assert!(file.exists());
        accounting.trim(&root,&HashSet::new(),fence)?;
        assert!(!file.exists());assert!(old_source.exists());
        Ok(())
    }

    #[test]
    fn u07_unknown_legacy_pin_refuses_readiness_and_recovers_after_identification()->anyhow::Result<()> {
        let parent=Fixture::new(1)?;let root=parent.root.join("pin-work/_cache");let pins=root.join("pins");
        std::fs::create_dir_all(&pins)?;let unknown=pins.join("not-a-pin");std::fs::write(&unknown,b"unknown")?;
        std::fs::write(root.join(".u07-quiescent"),QUIESCENT)?;
        let fence=Arc::new(Mutation::acquire(&root)?);let store=crate::store::Store::in_memory()?;
        assert!(Accounting::open(&root,store.clone(),Some(0),fence.clone()).is_err());
        assert!(unknown.exists());assert!(!root.join(".u07-format").exists());
        std::fs::remove_file(unknown)?;
        let known=pins.join("f".repeat(64));std::fs::write(&known,b"")?;
        // Root and second-prefix unknowns in BOTH physical spaces must refuse without deletion.
        let manifests=manifest_root(&root)?;
        for base in [&root,&manifests] {
            for relative in ["unknown.bin","aa/nothex","ab","ac/bd"] {
                let unknown=base.join(relative);
                std::fs::create_dir_all(unknown.parent().context("unknown test parent")?)?;
                std::fs::write(&unknown,b"unknown")?;
                assert!(Accounting::open(&root,store.clone(),None,fence.clone()).is_err());
                assert!(unknown.exists());assert!(!root.join(".u07-format").exists());
                std::fs::remove_file(unknown)?;
            }
        }
        // Correctly typed owner metadata remains accepted alongside empty prefix directories.
        std::fs::create_dir_all(root.join(".activity"))?;
        let accounting=Accounting::open(&root,store,None,fence.clone())?;
        assert!(!known.exists());assert_eq!(accounting.totals(fence)?,(0,0,0));
        Ok(())
    }

}
