//! U15/B2: CD domain commands on the host's sole Amphora Database. No CD Raft/log owner.
//! `kiln_cd::state::Data::apply` decides; this adapter CAS-commits its result before ACK.

use std::path::Path;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use std::time::{Duration, Instant};

use amphora::database::{Database, Namespace};
use amphora_api::{Cond, Consistency, Records as _, Refusal, Rev, Tx, Write};
use anyhow::Context as _;
use futures::channel::oneshot;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use kiln_cd::state::{Change, Command, Data};

mod legacy;
mod control;

const FAMILY: &str = "cd-state";
const EVENTS: &str = "cd-changes";
const HEAD: &[u8] = b"head";
const PART: usize = 512 << 10;
const MAX_STATE: usize = 24 << 20;
const RETRIES: usize = 16;
fn deadline() -> Instant { Instant::now() + Duration::from_secs(30) }
fn part_key(n: usize) -> Vec<u8> { format!("part/{n:08}").into_bytes() }

#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    bytes: usize,
    pieces: usize,
    digest: String,
    changes: Vec<Change>,
    /// Pinned source/cursor fence unfinished imports. Completed content receipts survive source
    /// retirement and validate remaining local inputs without making their paths authoritative.
    source: legacy::Identity,
    cursor: u64,
    importing: bool,
}

#[derive(Serialize, Deserialize)]
enum Event { Change(Change), Resnapshot }

pub struct Changes { events: crate::bus::Receiver<Event> }
impl Changes {
    pub async fn recv(&mut self) -> Result<Change, crate::bus::RecvError> {
        match self.events.recv().await? { Event::Change(change) => Ok(change), Event::Resnapshot => Err(crate::bus::RecvError::Overflowed(1)) }
    }
}

struct View { rev: Rev, data: Data }
struct Inner {
    namespace: Namespace,
    controller: ControllerScope,
    view: RwLock<View>,
    serial: async_lock::Mutex<()>,
    changes: crate::bus::Broadcast<Event>,
    delivered: AtomicU64,
}

pub struct Store { inner: Arc<Inner>, stop: crate::rt::Shutdown }

/// U04/U15: pure Engine operation ids restart at one. This instance's identity is captured
/// before either live status or immutable Records outcomes are accepted; replay keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct ControllerScope(String);
impl ControllerScope {
    fn new() -> Self { Self(crate::util::new_id()) }
    pub(crate) fn as_str(&self) -> &str { &self.0 }
}
impl TryFrom<String> for ControllerScope {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 32 || !value.bytes().all(|b| b.is_ascii_hexdigit()) { return Err("invalid CD controller incarnation"); }
        Ok(Self(value))
    }
}
impl From<ControllerScope> for String { fn from(scope: ControllerScope) -> Self { scope.0 } }

/// U15/B2: deployment configuration defines source authority. An attached remote Database
/// can lack a local replica handle while still belonging to the configured replicated group.
#[derive(Clone, Copy, Debug)]
pub enum SourceConfig { Local, Replica { member: u64, source: Option<u64> } }

impl Drop for Store {
    fn drop(&mut self) { self.stop.cancel(); }
}

impl Store {
    /// Standalone/local admission. Configured replicas use `open_from` with their declared ids.
    pub async fn open(database: Database, legacy_dir: &Path, bus: Arc<dyn kiln_seams::Bus>, shutdown: crate::rt::Shutdown) -> anyhow::Result<Self> {
        Self::open_from(database, legacy_dir, bus, shutdown, SourceConfig::Local).await
    }

    /// U15/B2: a replicated legacy migration has an explicitly declared source member.
    /// The fresh/local constructor remains usable without introducing another election.
    pub async fn open_from(database: Database, legacy_dir: &Path, bus: Arc<dyn kiln_seams::Bus>, shutdown: crate::rt::Shutdown, source: SourceConfig) -> anyhow::Result<Self> {
        let (member, source_node) = match source {
            SourceConfig::Local => (None, None),
            SourceConfig::Replica { member, source } => {
                anyhow::ensure!(member != 0 && source != Some(0), "CD source admission requires nonzero configured member and source ids");
                (Some(member), source)
            }
        };
        let namespace = database.namespace("cd-engine")?;
        let (ns, dir) = (namespace.clone(), legacy_dir.to_path_buf());
        let view = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || {
            legacy::import_at(&ns, &dir, member, source_node)?;
            load(&ns)?.map(|(rev, _, data)| View { rev, data }).context("CD import did not commit its canonical snapshot")
        }).await??;
        let delivered = AtomicU64::new(view.rev);
        let inner = Arc::new(Inner { namespace, controller: ControllerScope::new(), view: RwLock::new(view), serial: async_lock::Mutex::new(()), changes: crate::bus::Broadcast::on(bus, 1024)?, delivered });
        let store = Self { inner, stop: crate::rt::Shutdown::new() };
        store.follow(shutdown);
        Ok(store)
    }

    pub fn read<T>(&self, f: impl FnOnce(&Data) -> T) -> T { f(&self.inner.view.read().data) }

    pub(crate) fn controller_scope(&self) -> &ControllerScope { &self.inner.controller }

    /// Same public ACK shape as the retired store; Amphora alone decides durable success.
    pub fn submit(&self, command: Command) -> oneshot::Receiver<Result<(), String>> {
        self.submit_with_app(command, None)
    }

    /// A Git/rendered app may be runtime-owned rather than present in Data.apps. Capture that
    /// accepted definition with its deployment, never by looking it up during later delivery.
    pub(crate) fn submit_with_app(&self, command: Command, app: Option<kiln_cd::apps::AppDef>) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        let inner = Arc::clone(&self.inner);
        crate::rt::spawn(async move {
            let _serial = inner.serial.lock().await;
            let namespace = inner.namespace.clone();
            let controller = inner.controller.clone();
            let result = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || apply_with_app(&namespace, &controller, command, app)).await;
            let result = match result {
                Ok(Ok((rev, data, _))) => { install(&inner, rev, data); Ok(()) }
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(e) => Err(format!("CD storage task failed: {e:#}")),
            };
            let _ = tx.send(result);
        });
        rx
    }

    pub fn changes(&self) -> Changes { Changes { events: self.inner.changes.subscribe() } }

    /// The existing CD worker owns recovery. Quiver is a wakeup; these rows remain until the
    /// canonical SQL control outbox has durably accepted their immutable descriptors.
    pub(crate) async fn drain_controls(&self, store: &crate::store::Store) -> anyhow::Result<bool> {
        control::drain(&self.inner.namespace, store).await
    }

    fn follow(&self, shutdown: crate::rt::Shutdown) {
        let weak = Arc::downgrade(&self.inner);
        let stop = self.stop.clone();
        crate::rt::spawn(async move {
            use futures::FutureExt as _;
            loop {
                if stop.is_cancelled() || shutdown.is_cancelled() { return; }
                let Some(inner) = weak.upgrade() else { return };
                let after = inner.delivered.load(Ordering::Acquire);
                let waiting = match inner.namespace.watch(EVENTS, after) {
                    Ok(wait) => wait,
                    Err(e) => { tracing::warn!(error = %e, "U15 CD watch could not register; retrying"); drop(inner); if !shutdown.sleep(Duration::from_secs(1)).await { return; } continue; }
                };
                drop(inner);
                futures::select_biased! {
                    () = stop.cancelled().fuse() => return,
                    () = shutdown.cancelled().fuse() => return,
                    result = waiting.fuse() => {
                        if let Err(e) = result { tracing::warn!(error = %e, "U15 CD watch requires resnapshot"); }
                    }
                }
                let Some(inner) = weak.upgrade() else { return };
                let _serial = inner.serial.lock().await;
                let ns = inner.namespace.clone();
                let result = crate::rt::blocking_in(crate::rt::SHARE_STORE, move || -> anyhow::Result<_> {
                    let events = match ns.changes(EVENTS, after, 128, deadline()) {
                        Ok(events) => Some(events),
                        Err(Refusal::Compacted { .. }) => None,
                        Err(e) => return Err(e.into()),
                    };
                    let snapshot = load(&ns)?.context("canonical CD state vanished")?;
                    Ok((events, snapshot))
                }).await;
                match result {
                    Ok(Ok((events, (rev, _, data)))) => {
                        install(&inner, rev, data);
                        match events {
                            Some(events) => {
                                for row in events.changes {
                                    let changes = match row.value.as_deref().map(serde_json::from_slice::<Vec<Change>>).transpose() {
                                        Ok(Some(changes)) => changes,
                                        _ => { let _ = inner.changes.send(Event::Resnapshot); inner.delivered.store(rev, Ordering::Release); break; }
                                    };
                                    for change in changes { let _ = inner.changes.send(Event::Change(change)); }
                                    inner.delivered.store(row.rev, Ordering::Release);
                                }
                            }
                            None => { let _ = inner.changes.send(Event::Resnapshot); inner.delivered.store(rev, Ordering::Release); }
                        }
                    }
                    other => {
                        let why = match other { Ok(Err(e)) => format!("{e:#}"), Err(e) => format!("{e:#}"), _ => String::new() };
                        tracing::error!(error = %why, "U15 CD follower snapshot unavailable; refusing to advance cached state");
                        if !shutdown.sleep(Duration::from_secs(1)).await { return; }
                    }
                }
            }
        });
    }
}

fn install(inner: &Inner, rev: Rev, data: Data) {
    let mut view = inner.view.write();
    if rev > view.rev { *view = View { rev, data }; }
}

fn load(namespace: &Namespace) -> anyhow::Result<Option<(Rev, Manifest, Data)>> {
    let Some(head) = namespace.get(FAMILY, HEAD, Consistency::Linearizable, deadline())? else { return Ok(None) };
    let manifest: Manifest = serde_json::from_slice(&head.value)?;
    anyhow::ensure!(manifest.format == 1 && manifest.bytes <= MAX_STATE && manifest.pieces == manifest.bytes.div_ceil(PART), "invalid canonical CD state manifest");
    let mut bytes = Vec::with_capacity(manifest.bytes);
    for n in 0..manifest.pieces {
        let row = namespace.get(FAMILY, &part_key(n), Consistency::Snapshot(head.rev), deadline())?.context("canonical CD state piece missing")?;
        anyhow::ensure!(row.rev == head.rev && row.value.len() <= PART, "canonical CD state piece belongs to another commit");
        bytes.extend_from_slice(&row.value);
    }
    anyhow::ensure!(bytes.len() == manifest.bytes && crate::util::sha256_hex(&bytes) == manifest.digest, "canonical CD state digest mismatch");
    Ok(Some((head.rev, manifest, serde_json::from_slice(&bytes)?)))
}

fn save(namespace: &Namespace, expected: Rev, data: &Data, mut manifest: Manifest) -> Result<Rev, Refusal> {
    save_with(namespace, expected, data, &mut manifest, Tx::default())
}

fn save_with(namespace: &Namespace, expected: Rev, data: &Data, manifest: &mut Manifest, mut tx: Tx) -> Result<Rev, Refusal> {
    let bytes = serde_json::to_vec(data).map_err(|e| Refusal::Invalid { what: "CD state".into(), why: e.to_string() })?;
    if bytes.len() > MAX_STATE { return Err(Refusal::TooLarge { what: "CD state".into(), size: bytes.len(), limit: MAX_STATE }); }
    manifest.bytes = bytes.len(); manifest.pieces = bytes.len().div_ceil(PART); manifest.digest = crate::util::sha256_hex(&bytes);
    let mut writes: Vec<_> = bytes.chunks(PART).enumerate().map(|(n, bytes)| Write::Put { family: FAMILY.into(), key: part_key(n), value: bytes.to_vec() }).collect();
    writes.push(Write::Put { family: FAMILY.into(), key: HEAD.to_vec(), value: serde_json::to_vec(manifest).map_err(|e| Refusal::Invalid { what: "CD manifest".into(), why: e.to_string() })? });
    // A separate one-row-per-command feed preserves every Change even when a follower skips
    // state snapshots or a large chunked commit crosses a change-page boundary.
    writes.push(Write::Put { family: EVENTS.into(), key: HEAD.to_vec(), value: serde_json::to_vec(&manifest.changes).map_err(|e| Refusal::Invalid { what: "CD changes".into(), why: e.to_string() })? });
    let cond = if expected == 0 { Cond::Absent { family: FAMILY.into(), key: HEAD.to_vec() } } else { Cond::At { family: FAMILY.into(), key: HEAD.to_vec(), rev: expected } };
    tx.conds.push(cond);
    tx.writes.extend(writes);
    namespace.commit(tx, deadline())
}

#[cfg(test)]
fn apply(namespace: &Namespace, command: Command) -> anyhow::Result<(Rev, Data, Vec<Change>)> {
    apply_with_app(namespace, &ControllerScope::new(), command, None)
}

fn apply_with_app(namespace: &Namespace, controller: &ControllerScope, command: Command, app: Option<kiln_cd::apps::AppDef>) -> anyhow::Result<(Rev, Data, Vec<Change>)> {
    for _ in 0..RETRIES {
        let (rev, mut manifest, mut data) = load(namespace)?.context("canonical CD state is missing")?;
        anyhow::ensure!(!manifest.importing, "CD legacy import is incomplete; commands are fenced until cutover");
        let accepted_app = control::before(&data, &command, app.as_ref())?;
        let mut changes = Vec::new();
        data.apply(command.clone(), &mut changes).map_err(anyhow::Error::msg)?;
        manifest.changes.clone_from(&changes);
        let pending = control::admit(namespace, controller, rev, &data, &changes, accepted_app)?;
        match save_with(namespace, rev, &data, &mut manifest, pending) {
            Ok(rev) => return Ok((rev, data, changes)),
            Err(Refusal::Conflict { .. }) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("CD state kept changing; retry the domain command against its current version")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_cd::state::Deployment;
    use serde_json::json;

    #[test]
    fn u15_b2_cd_configured_member_fences_source_admission_without_a_local_replica_handle() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let database = Database::in_memory(deadline())?;
            assert!(database.replica().is_none(), "the fixture deliberately supplies no local replica handle");
            let cache = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache"); std::fs::create_dir_all(&cache)?;
            let dir = tempfile::tempdir_in(cache)?; let legacy = dir.path().join("legacy");
            let bus: Arc<dyn kiln_seams::Bus> = Arc::new(crate::bus::QuiverBus::open(&Default::default(), crate::rt::clock())?);
            let shutdown = crate::rt::Shutdown::new();
            let follower = SourceConfig::Replica { member: 2, source: Some(1) };
            assert!(Store::open_from(database.clone(), &legacy, Arc::clone(&bus), shutdown.clone(), follower).await.is_err(), "missing local handle cannot turn a configured follower into a standalone import owner");
            let namespace = database.namespace("cd-engine")?;
            assert!(load(&namespace)?.is_none(), "refused follower did not publish empty state");
            std::fs::create_dir_all(&legacy)?;
            std::fs::write(legacy.join("cd.state"), b"legacy bytes requiring source selection")?;
            assert!(Store::open_from(database.clone(), &legacy, Arc::clone(&bus), shutdown.clone(), SourceConfig::Replica { member: 2, source: None }).await.is_err(), "configured replicated legacy input requires a declaration despite the missing backend handle");
            assert!(load(&namespace)?.is_none());
            std::fs::remove_file(legacy.join("cd.state"))?;
            let store = Store::open_from(database, &legacy, bus, shutdown.clone(), SourceConfig::Replica { member: 1, source: Some(1) }).await?;
            store.submit(Command::PutSetting("configured-owner".into(), json!(true))).await?.map_err(anyhow::Error::msg)?;
            assert_eq!(store.read(|data| data.settings.get("configured-owner").cloned()), Some(json!(true)));
            shutdown.cancel();
            Ok(())
        })
    }

    #[test]
    fn u15_b2_cd_acknowledged_commands_survive_adapter_owner_failover_and_database_reopen() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let cache = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache"); std::fs::create_dir_all(&cache)?;
            let dir = tempfile::tempdir_in(cache)?; let path = dir.path().join("host.db"); let legacy = dir.path().join("legacy");
            let bus: Arc<dyn kiln_seams::Bus> = Arc::new(crate::bus::QuiverBus::open(&Default::default(), crate::rt::clock())?);
            let shutdown = crate::rt::Shutdown::new();
            {
                let database = Database::open(&path, deadline())?;
                let first = Store::open(database.clone(), &legacy, Arc::clone(&bus), shutdown.clone()).await?;
                let second = Store::open(database, &legacy, Arc::clone(&bus), shutdown.clone()).await?;
                let mut changes = second.changes();
                first.submit(Command::PutSetting("rule".into(), json!("first"))).await?.map_err(anyhow::Error::msg)?;
                let change = crate::rt::timeout(Duration::from_secs(5), changes.recv()).await??;
                assert_eq!(change, Change::Setting("rule".into()));
                assert_eq!(second.read(|d| d.settings.get("rule").cloned()), Some(json!("first")));
                drop(first);
                second.submit(Command::Deployed { deployment:Box::new(Deployment { app:"app".into(), op:7, ..Default::default() }), limit:10 }).await?.map_err(anyhow::Error::msg)?;
                second.submit(Command::PutSetting("rule".into(), json!("successor"))).await?.map_err(anyhow::Error::msg)?;
            }
            // U15/B2: successful public inspection holds the product's exclusive lease. A
            // still-live owner must drain before this can prove recovery will read disk.
            let owner_path = path.clone();
            crate::rt::blocking_in(crate::rt::SHARE_STORE, move || -> anyhow::Result<()> {
                let until = deadline();
                loop {
                    match amphora::replication::bootstrap::is_replica(&owner_path) {
                        Ok(replica) => { anyhow::ensure!(!replica, "standalone CD fixture unexpectedly became replicated"); return Ok(()); }
                        Err(error) => anyhow::ensure!(Instant::now() < until, "CD adapter did not release the product lease: {error}"),
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }).await??;
            let reopened = Store::open(Database::open(&path, deadline())?, &legacy, bus, shutdown.clone()).await?;
            assert_eq!(reopened.read(|d| d.settings.get("rule").cloned()), Some(json!("successor")));
            assert_eq!(reopened.read(|d| d.history.get("app").and_then(|h| h.back()).map(|d| (d.id, d.op))), Some((1, 7)));
            shutdown.cancel();
            Ok(())
        })
    }

    #[test]
    fn u15_b2_cd_competing_cas_commands_recompute_without_lost_deployment_history() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let database = Database::in_memory(deadline())?;
            let cache = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache"); std::fs::create_dir_all(&cache)?;
            let dir = tempfile::tempdir_in(cache)?;
            let bus: Arc<dyn kiln_seams::Bus> = Arc::new(crate::bus::QuiverBus::open(&Default::default(), crate::rt::clock())?);
            let shutdown = crate::rt::Shutdown::new();
            let first = Store::open(database.clone(), dir.path(), Arc::clone(&bus), shutdown.clone()).await?;
            let second = Store::open(database.clone(), dir.path(), bus, shutdown.clone()).await?;
            let command = |op| Command::Deployed { deployment:Box::new(Deployment { app:"app".into(), op, ..Default::default() }), limit:10 };
            let mut changes = second.changes();
            let (a, b) = futures::join!(first.submit(command(1)), second.submit(command(2)));
            a?.map_err(anyhow::Error::msg)?; b?.map_err(anyhow::Error::msg)?;
            for _ in 0..2 {
                assert_eq!(crate::rt::timeout(Duration::from_secs(5), changes.recv()).await??, Change::Deployed("app".into()), "each committed domain change reaches Quiver");
            }
            let namespace = database.namespace("cd-engine")?;
            let (_, _, data) = load(&namespace)?.context("canonical data")?;
            assert_eq!(data.history.get("app").map(|h| (h.len(), h.back().map(|d| d.id))), Some((2, Some(2))));
            let before = data;
            assert!(apply(&namespace, Command::DeleteCluster("missing".into())).is_err());
            assert_eq!(serde_json::to_value(load(&namespace)?.context("after refusal")?.2)?, serde_json::to_value(before)?);
            shutdown.cancel();
            Ok(())
        })
    }
}
