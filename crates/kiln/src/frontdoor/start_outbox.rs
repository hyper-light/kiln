//! W21-R15. Only a failed start forward leaves a source-side payload here. The selected
//! runner still owns admission and records the operation digest alongside its session.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use super::replica::{LocalNode, SessionStart};
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension as _, params};

#[derive(Default)]
struct Peer {
    generation: u64,
    held: bool,
    running: bool,
    writes: u64,
}

trait Delivery: Send + Sync {
    fn send<'a>(&'a self, peer: &'a str, start: SessionStart) -> futures::future::BoxFuture<'a, super::replica::Forward>;
}
impl Delivery for LocalNode {
    fn send<'a>(&'a self, peer: &'a str, start: SessionStart) -> futures::future::BoxFuture<'a, super::replica::Forward> {
        Box::pin(self.forward_held_start(peer, start))
    }
}

#[derive(Default)]
struct Ready {
    local: Weak<LocalNode>,
    admissions: HashMap<String, Weak<Admission>>,
    receipts: HashSet<String>,
    indexed: usize,
    overflow: bool,
    host: Option<Weak<dyn Delivery>>,
    peers: HashMap<String, Peer>,
}

impl Ready {
    fn remember(&mut self, id: &str) {
        if self.overflow || self.receipts.contains(id) {
            return;
        }
        let size = id.len().saturating_add(size_of::<String>());
        if self.indexed.saturating_add(size) > crate::memory::room::share() {
            self.receipts = HashSet::new();
            self.indexed = 0;
            self.overflow = true;
        } else {
            self.receipts.insert(id.into());
            self.indexed += size;
        }
    }
    fn forget(&mut self, id: &str) {
        if self.receipts.remove(id) {
            self.indexed = self.indexed.saturating_sub(id.len() + size_of::<String>());
        }
    }
}

pub(crate) struct Outbox {
    path: PathBuf,
    process: String,
    lease: Mutex<Option<std::fs::File>>,
    db: Mutex<Option<Connection>>,
    ready: Mutex<Ready>,
}

/// Keeps an admission open until all duplicate callers and its SQLite transaction finish.
pub(crate) struct Admission {
    db: Weak<Outbox>,
    key: String,
}
impl Drop for Admission {
    fn drop(&mut self) {
        let Some(db) = self.db.upgrade() else { return };
        let key = self.key.clone();
        crate::rt::spawn(async move {
            let result = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
                db.with_db(|c| {
                    let mut ready = db.ready.lock();
                    if ready.admissions.get(&key).and_then(Weak::upgrade).is_some() {
                        return Ok(());
                    }
                    ready.admissions.remove(&key);
                    drop(ready);
                    if !c.query_row("SELECT EXISTS(SELECT 1 FROM start_attempts WHERE key=?1 AND process=?2)", params![key, db.process], |r| {
                        r.get::<_, bool>(0)
                    })? {
                        return Ok(());
                    }
                    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    tx.execute(
                        "DELETE FROM children WHERE key=?1 AND slot IN (SELECT slot FROM start_attempts WHERE key=?1 AND process=?2 AND target IS NULL)",
                        params![key, db.process],
                    )?;
                    tx.execute("DELETE FROM start_attempts WHERE key=?1 AND process=?2 AND target IS NULL", params![key, db.process])?;
                    tx.execute(
                        "UPDATE start_attempts SET process='' WHERE key=?1 AND process=?2 AND target IS NOT NULL",
                        params![key, db.process],
                    )?;
                    tx.commit()?;
                    Ok(())
                })
            })
            .await
            .and_then(|r| r);
            if let Err(error) = result {
                tracing::warn!(%error,"unable to reclaim unsent start slots");
            }
        });
    }
}
#[derive(Debug)]
pub(crate) struct Held {
    pub slot: String,
    pub target: String,
    pub state: String,
    pub why: String,
    pub result: Option<serde_json::Value>,
}

impl Held {
    pub(crate) fn report(&self, source: &str) -> serde_json::Value {
        use serde_json::json;
        if matches!(self.state.as_str(), "started" | "forwarded")
            && let Some(mut row) = self.result.as_ref().and_then(|v| v.pointer("/0").or_else(|| v.pointer("/agents/0"))).cloned()
            && let Some(o) = row.as_object_mut()
        {
            o.insert("outcome".into(), json!(self.state));
            o.insert("why".into(), json!("the selected node confirmed the start"));
            return row;
        }
        let (code, why) = if self.state == "held" {
            (202, format!("held at {source}, delivering to {}", self.target))
        } else {
            (409, self.why.clone())
        };
        json!({"id":self.slot,"target":self.target,"outcome":self.state,"status":code,"why":why})
    }
}

impl Outbox {
    pub(crate) fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            process: crate::util::new_id(),
            lease: Mutex::default(),
            db: Mutex::default(),
            ready: Mutex::default(),
        })
    }

    pub(crate) fn admission(self: &Arc<Self>, key: &str) -> Arc<Admission> {
        let mut ready = self.ready.lock();
        if let Some(active) = ready.admissions.get(key).and_then(Weak::upgrade) {
            return active;
        }
        let active = Arc::new(Admission {
            db: Arc::downgrade(self),
            key: key.into(),
        });
        ready.admissions.insert(key.into(), Arc::downgrade(&active));
        active
    }

    fn lease_path(&self, owner: &str) -> PathBuf {
        self.path.with_extension(format!("{owner}.using"))
    }

    fn live_reserver(&self) -> Result<()> {
        let mut lease = self.lease.lock();
        if lease.is_none() {
            let file = crate::os::with_mode(std::fs::OpenOptions::new().read(true).write(true).create_new(true), 0o600).open(self.lease_path(&self.process))?;
            file.try_lock()?;
            *lease = Some(file);
        }
        Ok(())
    }

    fn reap_unsent(&self, tx: &Connection) -> Result<()> {
        let mut q = tx.prepare("SELECT DISTINCT process FROM start_attempts WHERE target IS NULL")?;
        let owners = q.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        for owner in owners {
            if owner == self.process {
                continue;
            }
            anyhow::ensure!(crate::util::is_id(&owner), "invalid start admission owner");
            let path = self.lease_path(&owner);
            let abandoned = match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => match file.try_lock() {
                    Ok(()) => true,
                    Err(std::fs::TryLockError::WouldBlock) => false,
                    Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => return Err(e.into()),
            };
            if abandoned {
                tx.execute(
                    "DELETE FROM children WHERE (key,slot) IN (SELECT key,slot FROM start_attempts WHERE process=?1 AND target IS NULL)",
                    [&owner],
                )?;
                tx.execute("DELETE FROM start_attempts WHERE process=?1 AND target IS NULL", [&owner])?;
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }

    fn with_db<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.db.lock();
        if guard.is_none() {
            let parent = self.path.parent().context("missing start outbox directory")?;
            std::fs::create_dir_all(parent)?;
            let mut file = std::fs::OpenOptions::new();
            file.read(true).write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                file.mode(0o600);
            }
            drop(file.open(&self.path)?);
            let mut db = Connection::open(&self.path)?;
            db.pragma_update(None, "synchronous", "FULL")?;
            db.execute_batch(include_str!("start_outbox_001.sql"))?;
            db.execute_batch(include_str!("start_outbox_002.sql"))?;
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            self.reap_unsent(&tx)?;
            tx.commit()?;
            crate::blobs::chunks::sync_dir(parent)?;
            *guard = Some(db);
        }
        f(guard.as_mut().context("start outbox unavailable")?)
    }

    fn existing(c: &Connection, key: &str, slot: &str, digest: &str) -> Result<Option<Held>> {
        let row = c
            .query_row("SELECT digest,target,state,why,result FROM starts WHERE key=?1 AND slot=?2", params![key, slot], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .optional()?;
        row.map(|(prior, target, state, why, result)| {
            anyhow::ensure!(prior == digest, "operation key already names different start content");
            Ok(Held {
                slot: slot.into(),
                target,
                state,
                why,
                result: result.map(|r| serde_json::from_str(&r)).transpose()?,
            })
        })
        .transpose()
    }

    pub(crate) async fn bind(self: &Arc<Self>, key: String, digest: String, target: String, actor: String) -> Result<bool> {
        let me = self.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
            me.with_db(|db| {
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let fresh = tx.execute(
                    "INSERT INTO operations(key,digest,target,actor,requested_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(key) DO NOTHING",
                    params![key, digest, target, actor, crate::util::now_ms()],
                )?;
                let same: bool = tx.query_row(
                    "SELECT digest=?2 AND target=?3 AND actor=?4 FROM operations WHERE key=?1",
                    params![key, digest, target, actor],
                    |r| r.get(0),
                )?;
                anyhow::ensure!(same, "operation key already belongs to a different start");
                tx.commit()?;
                Ok(fresh == 1)
            })
        })
        .await?
    }

    /// Keep admission serial across concurrent requests here. Consult the committed session
    /// state only when the actor would otherwise exceed their budget. In-flight and held
    /// slots stay charged until there is evidence they finished or were refused.
    pub(crate) async fn reserve(
        self: &Arc<Self>,
        actor: String,
        key: String,
        digest: String,
        ids: Vec<String>,
        limit: u32,
        store: Option<crate::convo::store::Store>,
    ) -> Result<HashSet<String>> {
        let this = self.clone();
        let active = self.ready.lock().admissions.get(&key).and_then(Weak::upgrade);
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
            let _active = active;
            this.with_db(|db| {
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                this.live_reserver()?;
                let used = |tx: &Connection| {
                    anyhow::Ok(usize::try_from(
                        tx.query_row("SELECT count(*) FROM children WHERE actor=?1", [&actor], |r| r.get::<_, i64>(0))?,
                    )?)
                };
                let mut active = used(&tx)?;
                if active.saturating_add(ids.len()) > limit as usize {
                    this.reap_unsent(&tx)?;
                    active = used(&tx)?;
                }
                let mut fresh = Vec::new();
                let mut conflicts = HashSet::new();
                for id in &ids {
                    let prior = tx
                        .query_row("SELECT digest FROM children WHERE actor=?1 AND slot=?2", params![actor, id], |r| r.get::<_, String>(0))
                        .optional()?;
                    match prior {
                        None => fresh.push(id),
                        Some(prior) if prior != digest => {
                            conflicts.insert(id.clone());
                        }
                        Some(_) => {}
                    }
                }
                if active.saturating_add(fresh.len()) > limit as usize
                    && let Some(store) = store
                {
                    let rows = tx
                        .prepare("SELECT slot FROM children WHERE actor=?1")?
                        .query_map([&actor], |r| r.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    let requested: std::collections::HashSet<&String> = ids.iter().collect();
                    for id in rows {
                        if requested.contains(&id) {
                            continue;
                        }
                        if let Some(kept) = store.kept_committed(&id, 0)?
                            && let Some(raw) = kept.info
                            && !serde_json::from_str::<crate::harness::session::Info>(&raw)?.at_work()
                        {
                            active = active.saturating_sub(tx.execute("DELETE FROM children WHERE actor=?1 AND slot=?2", params![actor, id])?);
                        }
                    }
                }
                let remaining = (limit as usize).saturating_sub(active);
                anyhow::ensure!(fresh.len() <= remaining, "the caller has {remaining} remaining starts under max_children {limit}");
                for id in fresh {
                    tx.execute("INSERT INTO children(actor,slot,key,digest) VALUES(?1,?2,?3,?4)", params![actor, id, key, digest])?;
                    tx.execute(
                        "INSERT INTO start_attempts(key,slot,actor,digest,process) VALUES(?1,?2,?3,?4,?5)",
                        params![key, id, actor, digest, this.process],
                    )?;
                }
                tx.commit()?;
                Ok(conflicts)
            })
        })
        .await?
    }

    pub(crate) async fn dispatch(self: &Arc<Self>, key: String, ids: Vec<String>, target: String, receipt: String, agents: bool) -> Result<()> {
        let db = self.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
            db.with_db(|c| {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                for id in &ids {
                    let marked = tx.execute(
                        "INSERT INTO start_attempts(key,slot,actor,digest,process,target,receipt,agents)
                    SELECT key,slot,actor,digest,?3,?4,?5,?6 FROM children WHERE key=?1 AND slot=?2 AND NOT EXISTS(SELECT 1 FROM starts WHERE key=?1 AND slot=?2)
                    ON CONFLICT(key,slot) DO UPDATE SET target=excluded.target,receipt=excluded.receipt,agents=excluded.agents
                    WHERE start_attempts.target IS NULL OR (start_attempts.target=excluded.target AND start_attempts.receipt=excluded.receipt)",
                        params![key, id, db.process, target, receipt, agents],
                    )?;
                    anyhow::ensure!(
                        marked != 0 || tx.query_row("SELECT EXISTS(SELECT 1 FROM starts WHERE key=?1 AND slot=?2)", params![key, id], |r| r.get::<_, bool>(0))?,
                        "session {id} lost its admission or already has a different pending delivery; retry the original start"
                    );
                }
                tx.commit()?;
                for id in &ids {
                    db.ready.lock().remember(id);
                    db.check_known(id);
                }
                Ok(())
            })
        })
        .await??;
        Ok(())
    }

    pub(crate) async fn answered(self: &Arc<Self>, key: String, infos: Vec<serde_json::Value>) -> Result<()> {
        if infos.is_empty() {
            return Ok(());
        }
        let db = self.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE,move||db.with_db(|c|{
            let tx=c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            for info in &infos {
                let id=info.get("id").and_then(|v|v.as_str()).context("start answer lacks its session id")?;
                if let Some(agents)=tx.query_row("SELECT agents FROM start_attempts WHERE key=?1 AND slot=?2 AND process<>?3 AND target IS NOT NULL",params![key,id,db.process],|r|r.get::<_,bool>(0)).optional()? {
                    let state=info.get("outcome").and_then(|v|v.as_str()).unwrap_or("forwarded");
                    let result=if agents{serde_json::json!({"agents":[info]})}else{serde_json::json!([info])};
                    tx.execute("INSERT OR IGNORE INTO starts(key,slot,target,digest,state,why,result) SELECT key,slot,target,digest,?3,'the destination confirmed the start',?4 FROM start_attempts WHERE key=?1 AND slot=?2",params![key,id,state,serde_json::to_string(&result)?])?;
                }
                tx.execute("DELETE FROM start_attempts WHERE key=?1 AND slot=?2",params![key,id])?;
            }
            tx.commit()?;
            for id in infos.iter().filter_map(|i|i.get("id").and_then(|v|v.as_str())){db.forget_if_done(c,id)?;}
            Ok(())
        })).await?
    }

    pub(crate) async fn release(self: &Arc<Self>, actor: String, digest: String, slots: Vec<String>) -> Result<()> {
        if slots.is_empty() {
            return Ok(());
        }
        let this = self.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
            this.with_db(|db| {
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                for slot in &slots {
                    tx.execute("DELETE FROM start_attempts WHERE slot=?1 AND actor=?2 AND digest=?3", params![slot, actor, digest])?;
                    tx.execute("DELETE FROM children WHERE actor=?1 AND slot=?2 AND digest=?3", params![actor, slot, digest])?;
                }
                tx.commit()?;
                for slot in &slots {
                    this.forget_if_done(db, slot)?;
                }
                Ok(())
            })
        })
        .await?
    }

    pub(crate) async fn visible(self: &Arc<Self>, key: String) -> Result<Vec<serde_json::Value>> {
        let me = self.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
            if !me.path.try_exists()? {
                return Ok(Vec::new());
            }
            me.with_db(|c| {
                let mut q = c.prepare("SELECT slot,target,state,why FROM (SELECT slot,target,state,why,rowid AS seq FROM starts WHERE key=?1 UNION ALL SELECT a.slot,a.target,'unconfirmed','dispatch began; awaiting the destination receipt',NULL FROM start_attempts a WHERE a.key=?1 AND a.target IS NOT NULL AND NOT EXISTS(SELECT 1 FROM starts s WHERE s.key=a.key AND s.slot=a.slot)) ORDER BY seq")?;
                Ok(q.query_map([key], |r| {
                    Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"target":r.get::<_,String>(1)?,"outcome":r.get::<_,String>(2)?,"why":r.get::<_,String>(3)?}))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
            })
        })
        .await?
    }

    pub(crate) async fn status(self: &Arc<Self>, key: String, slot: String, digest: String) -> Result<Option<Held>> {
        let me = self.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
            if !me.path.try_exists()? {
                return Ok(None);
            }
            me.with_db(|db| Self::existing(db, &key, &slot, &digest))
        })
        .await?
    }

    pub(crate) fn generation(&self, peer: &str) -> u64 {
        self.ready.lock().peers.get(peer).map_or(0, |p| p.generation)
    }

    fn associated(key: &str, slot: &str, target: &str, digest: &str) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&("kiln deferred start body v1", key, slot, target, digest))?)
    }

    fn seal_start(&self, db: &Connection, start: &SessionStart, key: &str, slot: &str, target: &str) -> Result<Vec<u8>> {
        let master = self.path.with_extension("key");
        let new = !master.try_exists()?;
        if new {
            let pending: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM starts WHERE state='held' AND typeof(payload)='blob')", [], |r| r.get(0))?;
            anyhow::ensure!(!pending, "the source's start outbox key is missing");
        }
        let aad = Self::associated(key, slot, target, &start.request)?;
        let (sealed, nonce) = crate::ci::secrets::seal_with(&master, &serde_json::to_vec(start)?, &aad)?;
        if new {
            crate::blobs::chunks::sync_dir(master.parent().context("missing start key directory")?)?;
        }
        Ok(postcard::to_allocvec(&(nonce, sealed))?)
    }

    fn open_start(&self, key: &str, slot: &str, target: &str, digest: &str, body: rusqlite::types::Value) -> Result<SessionStart> {
        let start: SessionStart = match body {
            rusqlite::types::Value::Blob(body) => {
                let master = self.path.with_extension("key");
                anyhow::ensure!(master.try_exists()?, "the source's start outbox key is missing");
                let (nonce, sealed): (&[u8], &[u8]) = postcard::from_bytes(&body)?;
                let plain = crate::ci::secrets::open_with(&master, sealed, nonce, &Self::associated(key, slot, target, digest)?)?;
                serde_json::from_slice(&plain)?
            }
            rusqlite::types::Value::Text(legacy) => serde_json::from_str(&legacy)?,
            _ => anyhow::bail!("invalid held start payload"),
        };
        anyhow::ensure!(start.matches_hold(slot, target, digest), "held session identity does not match its index");
        Ok(start)
    }

    pub(super) async fn hold(self: &Arc<Self>, key: String, slot: String, target: String, start: SessionStart, why: String, before: u64) -> Result<Held> {
        start.safe_to_hold()?;
        anyhow::ensure!(start.matches_hold(&slot, &target, &start.request), "the held slot does not name its destination");
        let saved = self.clone();
        let held = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
            saved.with_db(|db| {
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                if let Some(prior) = Self::existing(&tx, &key, &slot, &start.request)? {
                    return Ok(prior);
                }
                let encoded = saved.seal_start(&tx, &start, &key, &slot, &target)?;
                tx.execute(
                    "INSERT INTO starts(key,slot,target,digest,payload,state,why) VALUES(?1,?2,?3,?4,?5,'held',?6) ON CONFLICT(key,slot) DO NOTHING",
                    params![key, slot, target, start.request, encoded, why],
                )?;
                tx.execute("DELETE FROM start_attempts WHERE key=?1 AND slot=?2", params![key, slot])?;
                let result = Self::existing(&tx, &key, &slot, &start.request)?.context("start was not retained")?;
                tx.commit()?;
                saved.ready.lock().remember(&slot);
                Ok(result)
            })
        })
        .await??;
        if held.state == "held" {
            let run = {
                let mut ready = self.ready.lock();
                let p = ready.peers.entry(held.target.clone()).or_default();
                p.held = true;
                p.writes = p.writes.wrapping_add(1);
                p.generation != before
            };
            if run {
                self.schedule(&held.target);
            }
            self.check_known(&held.slot);
        }
        Ok(held)
    }

    /// On publication (including a cold restart) index only the destinations with pending work.
    /// Existing live links may already have raised their generation before the index was read.
    pub(crate) fn attach(self: &Arc<Self>, host: &Arc<LocalNode>) {
        self.ready.lock().local = Arc::downgrade(host);
        if let Some(manager) = host.manager() {
            manager.observe_start_holds(self);
        }
        self.attach_delivery(host.clone());
    }

    fn attach_delivery(self: &Arc<Self>, host: Arc<dyn Delivery>) {
        {
            let mut state = self.ready.lock();
            let initialized = state.host.as_ref().and_then(Weak::upgrade);
            state.host = Some(Arc::downgrade(&host));
            if initialized.is_some() {
                return;
            }
        }
        let me = self.clone();
        crate::rt::spawn(async move {
            let db = me.clone();
            let peers = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
                if !db.path.try_exists()? {
                    return Ok(Vec::new());
                }
                db.with_db(|c| {
                    let mut q = c.prepare("SELECT target,slot FROM starts WHERE state='held' UNION ALL SELECT NULL,slot FROM start_attempts WHERE target IS NOT NULL")?;
                    let mut peers = HashSet::new();
                    for row in q.query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)))? {
                        let (peer, slot) = row?;
                        if let Some(peer) = peer {
                            peers.insert(peer);
                        }
                        db.ready.lock().remember(&slot);
                        db.check_known(&slot);
                    }
                    Ok(peers.into_iter().collect::<Vec<_>>())
                })
            })
            .await
            .and_then(|r| r);
            match peers {
                Ok(peers) => {
                    for name in peers {
                        let linked = {
                            let mut state = me.ready.lock();
                            let lane = state.peers.entry(name.clone()).or_default();
                            lane.held = true;
                            lane.generation != 0
                        };
                        if linked {
                            me.schedule(&name);
                        }
                    }
                }
                Err(e) => tracing::error!(error=%e,"could not recover held session starts"),
            }
        });
    }

    fn check_known(self: &Arc<Self>, id: &str) {
        let local = self.ready.lock().local.upgrade();
        if local.and_then(|n| n.manager()).is_some_and(|mgr| mgr.with_status(id, |s| s.founded) == Some(true)) {
            self.founded(id);
        }
    }

    /// A sealed creation is also a receipt when both transport answers were lost.
    /// Look up only this slot; an idle node does no database work on publication.
    pub(crate) fn founded(self: &Arc<Self>, id: &str) {
        let local = {
            let ready = self.ready.lock();
            if !ready.overflow && !ready.receipts.contains(id) {
                return;
            }
            let Some(local) = ready.local.upgrade() else { return };
            local
        };
        let (me, id) = (self.clone(), id.to_owned());
        crate::rt::spawn(async move {
            let db = me.clone();
            let slot = id.clone();
            let rows = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
                db.with_db(|c| {
                    let mut q = c.prepare("SELECT rowid,key,target,digest,payload FROM starts WHERE slot=?1 AND state='held'")?;
                    let mut rows = Vec::new();
                    for r in q.query_map([&slot], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, rusqlite::types::Value>(4)?,
                        ))
                    })? {
                        let (row, key, target, digest, payload) = r?;
                        match db.open_start(&key, &slot, &target, &digest, payload) {
                            Ok(start) => rows.push((row, start)),
                            Err(error) => tracing::error!(%slot,%error,"could not verify a saved start against its replicated receipt"),
                        }
                    }
                    let mut pending = c.prepare("SELECT key,target,receipt,agents FROM start_attempts WHERE slot=?1 AND target IS NOT NULL")?;
                    let attempts = pending
                        .query_map([&slot], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, bool>(3)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok((rows, attempts))
                })
            })
            .await
            .and_then(|r| r);
            let result = async {
                let (rows, attempts) = rows?;
                for (key, target, receipt, agents) in attempts {
                    let (state, why, result) = match local.replicated_outcome(&id, &receipt, agents).await? {
                        Some(super::replica::Forward::Started(info)) => (
                            if target == "local" || target == local.machine { "started" } else { "forwarded" },
                            "founding receipt replicated".to_owned(),
                            Some(serde_json::to_string(&info)?),
                        ),
                        Some(super::replica::Forward::Refused(_, why)) => ("refused", why, None),
                        _ => continue,
                    };
                    me.settle_ambiguous(key, id.clone(), state, why, result).await?;
                }
                for (row, start) in rows {
                    let (status, why, answer) = match local.held_receipt(&id, &start).await? {
                        Some(super::replica::Forward::Started(receipt)) => ("forwarded", "founding receipt replicated".to_owned(), Some(serde_json::to_string(&receipt)?)),
                        Some(super::replica::Forward::Refused(_, why)) => ("refused", why, None),
                        _ => continue,
                    };
                    me.finish(row, id.clone(), status, why, answer).await?;
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(%id,%error,"held start receipt remains unconfirmed");
            }
        });
    }
    async fn settle_ambiguous(self: &Arc<Self>, key: String, slot: String, state: &'static str, why: String, result: Option<String>) -> Result<()> {
        let me = self.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE,move||me.with_db(|c|{
            let tx=c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute("INSERT OR IGNORE INTO starts(key,slot,target,digest,state,why,result) SELECT key,slot,target,digest,?3,?4,?5 FROM start_attempts WHERE key=?1 AND slot=?2 AND target IS NOT NULL",params![key,slot,state,why,result])?;
            if state=="refused"{tx.execute("DELETE FROM children WHERE key=?1 AND slot=?2 AND EXISTS(SELECT 1 FROM start_attempts WHERE key=?1 AND slot=?2 AND target IS NOT NULL)",params![key,slot])?;}
            tx.execute("DELETE FROM start_attempts WHERE key=?1 AND slot=?2",params![key,slot])?;
            tx.commit()?;
            me.forget_if_done(c,&slot)
        })).await?
    }
    fn forget_if_done(&self, c: &Connection, slot: &str) -> Result<()> {
        if !c.query_row(
            "SELECT EXISTS(SELECT 1 FROM starts WHERE slot=?1 AND state='held' UNION ALL SELECT 1 FROM start_attempts WHERE slot=?1 AND target IS NOT NULL)",
            [slot],
            |r| r.get::<_, bool>(0),
        )? {
            let overflow = {
                let mut ready = self.ready.lock();
                ready.forget(slot);
                ready.overflow
            };
            if overflow
                && !c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM starts WHERE state='held' UNION ALL SELECT 1 FROM start_attempts WHERE target IS NOT NULL)",
                    [],
                    |r| r.get::<_, bool>(0),
                )?
            {
                self.ready.lock().overflow = false;
            }
        }
        Ok(())
    }

    pub(crate) fn linked(self: &Arc<Self>, peer: &str) {
        {
            let mut ready = self.ready.lock();
            let p = ready.peers.entry(peer.to_owned()).or_default();
            p.generation = p.generation.wrapping_add(1);
        }
        self.schedule(peer);
    }

    fn schedule(self: &Arc<Self>, peer: &str) {
        let mut state = self.ready.lock();
        let Some(host) = state.host.as_ref().and_then(Weak::upgrade) else {
            return;
        };
        let Some(lane) = state.peers.get_mut(peer) else {
            return;
        };
        if !lane.held || lane.running {
            return;
        }
        lane.running = true;
        let (generation, writes) = (lane.generation, lane.writes);
        let (me, peer) = (self.clone(), peer.to_owned());
        crate::rt::spawn(async move {
            let result = me.drain_with(&peer, |start| host.send(&peer, start)).await;
            let again = {
                let mut state = me.ready.lock();
                let lane = state.peers.entry(peer.clone()).or_default();
                lane.running = false;
                let finished = matches!(result, Ok(true));
                match result {
                    Ok(empty) if lane.generation == generation && lane.writes == writes => lane.held = !empty,
                    Err(ref e) => tracing::warn!(%peer,error=%e,"held starts remain visible"),
                    _ => {}
                }
                lane.held && (lane.generation != generation || finished && lane.writes != writes)
            };
            if again {
                me.schedule(&peer);
            }
        });
    }

    async fn drain_with<Fut>(self: &Arc<Self>, peer: &str, mut deliver: impl FnMut(SessionStart) -> Fut) -> Result<bool>
    where
        Fut: std::future::Future<Output = super::replica::Forward>,
    {
        let mut after = 0;
        let mut unsettled = false;
        loop {
            let (db, target) = (self.clone(), peer.to_owned());
            let row = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
                db.with_db(|c| {
                    let row = c
                        .query_row(
                            "SELECT rowid,slot,key,digest,payload FROM starts WHERE target=?1 AND state='held' AND rowid>?2 ORDER BY rowid LIMIT 1",
                            params![target, after],
                            |r| {
                                Ok((
                                    r.get::<_, i64>(0)?,
                                    r.get::<_, String>(1)?,
                                    r.get::<_, String>(2)?,
                                    r.get::<_, String>(3)?,
                                    r.get::<_, rusqlite::types::Value>(4)?,
                                ))
                            },
                        )
                        .optional()?;
                    Ok(row.map(|(row, slot, key, digest, raw)| {
                        let start = db.open_start(&key, &slot, &target, &digest, raw);
                        (row, slot, start)
                    }))
                })
            })
            .await??;
            let Some((id, slot, decoded)) = row else {
                return Ok(!unsettled);
            };
            after = id;
            let outcome = match decoded {
                Ok(start) => Some(deliver(start).await),
                Err(e) => {
                    tracing::error!(%peer,%slot,error=%e,"could not read a held start");
                    None
                }
            };
            let (status, why, answer) = match outcome {
                Some(super::replica::Forward::Started(value)) => {
                    let confirmed = value.pointer("/0/id").or_else(|| value.pointer("/agents/0/id")).and_then(|v| v.as_str());
                    if confirmed != Some(slot.as_str()) {
                        unsettled = true;
                        tracing::warn!(%peer,%slot,"destination returned a different held session id");
                        continue;
                    }
                    ("forwarded", "the selected node confirmed the start".to_owned(), Some(serde_json::to_string(&value)?))
                }
                Some(super::replica::Forward::Refused(_, why)) => ("refused", why, None),
                Some(super::replica::Forward::Unavailable(_)) => return Ok(false),
                Some(super::replica::Forward::Unconfirmed(_)) => {
                    unsettled = true;
                    continue;
                }
                None => {
                    unsettled = true;
                    ("held", "saved start data cannot be read; check the destination session before retrying".to_owned(), None)
                }
            };
            self.finish(id, slot, status, why, answer).await?;
        }
    }

    async fn finish(self: &Arc<Self>, row: i64, slot: String, status: &'static str, why: String, answer: Option<String>) -> Result<()> {
        let db = self.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
            db.with_db(|c| {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let changed = tx.execute(
                    "UPDATE starts SET state=?1,why=?2,result=?3,payload=CASE WHEN ?1='held' THEN payload ELSE NULL END WHERE rowid=?4 AND state='held'",
                    params![status, why, answer, row],
                )?;
                if changed != 0 && status == "refused" {
                    tx.execute(
                        "DELETE FROM children WHERE slot=?1 AND digest=(SELECT digest FROM starts WHERE rowid=?2)",
                        params![slot, row],
                    )?;
                }
                tx.commit()?;
                db.forget_if_done(c, &slot)
            })
        })
        .await?
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        if self.lease.get_mut().take().is_some() {
            let _ = std::fs::remove_file(self.lease_path(&self.process));
        }
    }
}

pub(crate) fn path(dir: &Path, machine: &str, root: &[u8]) -> PathBuf {
    let scope = crate::util::sha256_hex(&[machine.as_bytes(), &[0], root].concat());
    dir.join(format!("session-starts-{scope}.db"))
}

#[cfg(test)]
mod tests {
    use super::super::replica::Forward;
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn start(id: &str) -> Result<SessionStart> {
        SessionStart::parse(false, &serde_json::to_vec(&json!({"target":"agent:east","ids":[id],"spec":{"kind":"codex"}}))?, "op").map_err(|(_, why)| anyhow::anyhow!(why))
    }
    async fn put(db: &Arc<Outbox>, id: &str) -> Result<()> {
        let start = start(id)?;
        db.bind("op".into(), start.request.clone(), "east".into(), "admin".into()).await?;
        db.hold("op".into(), id.into(), "east".into(), start, "no link".into(), db.generation("east")).await?;
        Ok(())
    }

    #[test]
    fn a_held_with_power_loss() -> Result<()> {
        use std::io::{BufRead as _, Read as _, Write as _};
        const WRITER: &str = "KILN_TEST_HELD_START_CRASH_WRITER";
        const SLOT: &str = "KILN_TEST_HELD_START_CRASH_SLOT";
        const COMMITTED: &str = "kiln test start hold committed";
        if let Some(path) = std::env::var_os(WRITER) {
            let source = Outbox::new(path.into());
            let id = std::env::var(SLOT)?;
            let writer = source.clone();
            crate::rt::block_on(async move {
                put(&writer, &id).await?;
                let sent = crate::util::derive_id(&["attempted", &id]);
                let unused = crate::util::derive_id(&["unsent", &id]);
                writer
                    .reserve("budget".into(), "attempt".into(), "digest".into(), vec![sent.clone(), unused], 2, None)
                    .await?;
                writer.dispatch("attempt".into(), vec![sent], "east".into(), "digest".into(), false).await?;
                Ok(())
            })?;
            let mut output = std::io::stdout().lock();
            writeln!(output, "{COMMITTED}")?;
            output.flush()?;
            // The parent cuts the process here, with its database connection still open.
            let mut stdin = std::io::stdin().lock();
            std::io::copy(&mut stdin.by_ref(), &mut std::io::sink())?;
            anyhow::bail!("writer was expected to be terminated");
        }
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("holding.db");
        let id = crate::util::new_id();
        let mut writer = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "frontdoor::start_outbox::tests::a_held_with_power_loss", "--nocapture"])
            .env(WRITER, &path)
            .env(SLOT, &id)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let output = writer.stdout.take().context("writer stdout")?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let committed = std::io::BufReader::new(output)
                .lines()
                .map_while(|line| line.ok())
                .any(|line| line.trim_end().ends_with(COMMITTED));
            let _ = tx.send(committed);
        });
        let committed = rx.recv_timeout(crate::hub::COMMAND_WITHIN);
        if committed.as_ref().is_ok_and(|ready| *ready) {
            let contender = Outbox::new(path.clone());
            crate::rt::block_on(async move {
                anyhow::ensure!(
                    contender
                        .reserve("budget".into(), "concurrent".into(), "concurrent".into(), vec![crate::util::new_id()], 2, None)
                        .await
                        .is_err(),
                    "a live process lost its reservation"
                );
                Ok(())
            })?;
        }
        let killed = writer.kill();
        let _ = writer.wait()?;
        anyhow::ensure!(committed?, "outbox writer stopped before committing");
        killed?;
        crate::rt::block_on(async move {
            let restarted = Outbox::new(path);
            let sent = crate::util::derive_id(&["attempted", &id]);
            let ambiguous = restarted.visible("attempt".into()).await?;
            anyhow::ensure!(
                ambiguous.len() == 1
                    && ambiguous
                        .first()
                        .is_some_and(|v| v.get("id") == Some(&json!(sent)) && v.get("outcome") == Some(&json!("unconfirmed")))
            );
            restarted
                .reserve("budget".into(), "replacement".into(), "replacement".into(), vec![crate::util::new_id()], 2, None)
                .await?;
            anyhow::ensure!(
                restarted
                    .reserve("budget".into(), "one-too-many".into(), "too-many".into(), vec![crate::util::new_id()], 2, None)
                    .await
                    .is_err(),
                "dispatched uncertainty released its capacity"
            );
            anyhow::ensure!(restarted.visible("op".into()).await?.first().and_then(|s| s.get("outcome")) == Some(&json!("held")));
            let answer = json!([{"id":id}]);
            anyhow::ensure!(restarted.drain_with("east", |_| std::future::ready(Forward::Started(answer.clone()))).await?);
            let state = restarted.status("op".into(), id.clone(), start(&id)?.request).await?.context("missing receipt")?;
            anyhow::ensure!(state.state == "forwarded" && state.result == Some(answer));
            Ok(())
        })
    }

    #[test]
    fn w21_r15_raw_launch_values_are_encrypted_and_bound_to_the_held_slot() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("outbox.db");
        let db = Outbox::new(path.clone());
        let id = crate::util::new_id();
        crate::rt::block_on(async move {
            let start = SessionStart::parse(
                false,
                &serde_json::to_vec(&json!({"target":"east","ids":[id],"spec":{"kind":"codex","env":[["SETTING","sensitive-fixture-value"]]}}))?,
                "op",
            )
            .map_err(|(_, why)| anyhow::anyhow!(why))?;
            db.hold("op".into(), id.clone(), "east".into(), start, "no link".into(), 0).await?;
            let (kind, leak): (String, i64) = Connection::open(path)?.query_row("SELECT typeof(payload),instr(payload,'sensitive-fixture-value') FROM starts", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            anyhow::ensure!(kind == "blob" && leak == 0);
            let done = db
                .drain_with("east", |start| {
                    let restored = serde_json::to_value(start)
                        .ok()
                        .is_some_and(|v| v.pointer("/spec/env/0/1") == Some(&json!("sensitive-fixture-value")));
                    std::future::ready(if restored {
                        Forward::Started(json!([{"id":id}]))
                    } else {
                        Forward::Unavailable("missing value".into())
                    })
                })
                .await?;
            anyhow::ensure!(done);
            Ok(())
        })
    }

    #[test]
    fn w21_r15_one_unconfirmed_session_does_not_hold_up_other_starts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Outbox::new(dir.path().join("holds.db"));
        crate::rt::block_on(async move {
            let first = crate::util::new_id();
            let second = crate::util::new_id();
            for id in [&first, &second] {
                db.hold(id.clone(), id.clone(), "east".into(), start(id)?, "link down".into(), 0).await?;
            }
            let mut sent = Vec::new();
            let first_request = start(&first)?.request;
            let idle = db
                .drain_with("east", |value| {
                    let id = if value.request == first_request { first.clone() } else { second.clone() };
                    sent.push(id.clone());
                    std::future::ready(if id == first {
                        Forward::Unconfirmed("waiting for its founding proof".into())
                    } else {
                        Forward::Started(json!([{"id":id}]))
                    })
                })
                .await?;
            anyhow::ensure!(!idle && sent == [first.clone(), second.clone()]);
            anyhow::ensure!(db.visible(first).await?.first().and_then(|v| v.get("outcome")) == Some(&json!("held")));
            anyhow::ensure!(db.visible(second).await?.first().and_then(|v| v.get("outcome")) == Some(&json!("forwarded")));
            Ok(())
        })
    }

    #[test]
    fn target_unreachable_then_back() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = Outbox::new(temp.path().join("holds.db"));
        let id = crate::util::new_id();
        crate::rt::block_on(async move {
            put(&source, &id).await?;
            let attempts = AtomicUsize::new(0);
            let done = source
                .drain_with("east", |_| {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    std::future::ready(Forward::Unavailable("link down".into()))
                })
                .await?;
            anyhow::ensure!(!done);
            source.linked("east");
            let done = source
                .drain_with("east", |_| {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    std::future::ready(Forward::Started(json!([{"id":id}])))
                })
                .await?;
            anyhow::ensure!(done && attempts.load(Ordering::Relaxed) == 2);
            anyhow::ensure!(
                source
                    .drain_with("east", |_| {
                        attempts.fetch_add(1, Ordering::Relaxed);
                        std::future::ready(Forward::Unavailable("duplicate".into()))
                    })
                    .await?
            );
            anyhow::ensure!(attempts.load(Ordering::Relaxed) == 2);
            Ok(())
        })
    }

    #[test]
    fn forward_succeeded_but_answer_lost() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = Outbox::new(temp.path().join("holds.db"));
        let id = crate::util::new_id();
        crate::rt::block_on(async move {
            let original = start(&id)?;
            let accepted = std::collections::HashSet::from([original.request.clone()]);
            source.hold("op".into(), id.clone(), "east".into(), original, "answer lost".into(), 0).await?;
            let answer = json!([{"id":id}]);
            let mut accepted = accepted;
            let done = source
                .drain_with("east", |retry| {
                    let duplicate = !accepted.insert(retry.request);
                    std::future::ready(if duplicate {
                        Forward::Started(answer.clone())
                    } else {
                        Forward::Refused(http::StatusCode::CONFLICT, "created twice".into())
                    })
                })
                .await?;
            anyhow::ensure!(done && accepted.len() == 1 && source.visible("op".into()).await?.first().and_then(|v| v.get("outcome")) == Some(&json!("forwarded")));
            Ok(())
        })
    }

    struct Counted {
        count: AtomicUsize,
        up: std::sync::atomic::AtomicBool,
        slot: String,
        sent: async_channel::Sender<()>,
    }
    impl Delivery for Counted {
        fn send<'a>(&'a self, _: &'a str, _: SessionStart) -> futures::future::BoxFuture<'a, Forward> {
            self.count.fetch_add(1, Ordering::Relaxed);
            let _ = self.sent.try_send(());
            let answer = if self.up.load(Ordering::Relaxed) {
                Forward::Started(json!([{"id":self.slot}]))
            } else {
                Forward::Unavailable("peer offline".into())
            };
            Box::pin(std::future::ready(answer))
        }
    }
    fn counted(source: &Arc<Outbox>, id: &str, up: bool) -> (Arc<Counted>, async_channel::Receiver<()>) {
        let (tx, rx) = async_channel::unbounded();
        let host = Arc::new(Counted {
            count: AtomicUsize::new(0),
            up: std::sync::atomic::AtomicBool::new(up),
            slot: id.into(),
            sent: tx,
        });
        source.attach_delivery(host.clone());
        (host, rx)
    }

    #[test]
    fn a_target_that_never_returns() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = Outbox::new(temp.path().join("holds.db"));
        let id = crate::util::new_id();
        crate::rt::block_on(async move {
            let (host, events) = counted(&source, &id, false);
            put(&source, &id).await?;
            for _ in 0..10 {
                let slots = source.visible("op".into()).await?;
                anyhow::ensure!(slots.first().and_then(|s| s.get("outcome")) == Some(&json!("held")));
            }
            anyhow::ensure!(host.count.load(Ordering::Relaxed) == 0);
            source.linked("elsewhere");
            anyhow::ensure!(events.try_recv().is_err());
            source.linked("east");
            crate::rt::timeout(std::time::Duration::from_secs(2), events.recv()).await??;
            for _ in 0..10 {
                source.visible("op".into()).await?;
            }
            anyhow::ensure!(host.count.load(Ordering::Relaxed) == 1);
            Ok(())
        })
    }

    #[test]
    fn the_target_comes_back_while_the_hold_is_being_written() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = Outbox::new(temp.path().join("holds.db"));
        let id = crate::util::new_id();
        crate::rt::block_on(async move {
            let (host, events) = counted(&source, &id, true);
            let before = source.generation("east");
            // Link-up races between the failed forward and the durable transaction commit.
            source.linked("east");
            source.hold("op".into(), id.clone(), "east".into(), start(&id)?, "old connection".into(), before).await?;
            crate::rt::timeout(std::time::Duration::from_secs(2), events.recv()).await??;
            anyhow::ensure!(host.count.load(Ordering::Relaxed) == 1);
            Ok(())
        })
    }

    #[test]
    fn concurrent_child_reservations_use_remaining_capacity_and_allow_replay() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = Outbox::new(temp.path().join("holds.db"));
        crate::rt::block_on(async move {
            let ids = (0..24).map(|_| crate::util::new_id()).collect::<Vec<_>>();
            let reserve = |digest: &str, ids: Vec<String>| source.reserve("operator".into(), digest.into(), digest.into(), ids, 24, None);
            reserve("first", ids.clone()).await?;
            reserve("first", ids.clone()).await?;
            anyhow::ensure!(reserve("extra", vec![crate::util::new_id()]).await.is_err());
            source.release("operator".into(), "first".into(), ids.into_iter().take(1).collect()).await?;
            let (one, two) = futures::join!(reserve("one", vec![crate::util::new_id()]), reserve("two", vec![crate::util::new_id()]));
            anyhow::ensure!(one.is_ok() != two.is_ok());
            for machine in ["east", "west"] {
                let caller = crate::authz::Caller::peer(&format!("session-node:{machine}"), "agents.run");
                let key = crate::harness::ops::start_key(&caller, "choose");
                let spec = crate::harness::session::Spec {
                    start_key: Some(key.clone()),
                    ..Default::default()
                };
                let actor = crate::harness::ops::start_budget_owner(&spec);
                anyhow::ensure!(actor == caller.name);
                source.reserve(actor.into(), key, machine.into(), vec![crate::util::new_id()], 1, None).await?;
            }
            Ok(())
        })
    }
}
