//! U05/U06/U15: listener metadata belongs to the host's existing Amphora Records owner.
//! The retired JSON file is a bounded, read-only import source, never a serving fallback.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use kiln_seams::records::{Cond, Consistency, Records, Refusal, Rev, Row, Scan, Tx, Write, VALUE_MAX};
use serde::{Deserialize, Serialize};

use super::{LISTENERS, Listener};

const FAMILY: &str = "observe.statsd";
const META: &str = "observe.metadata";
const KEY: &[u8] = b"statsd";
// U06: bounded keyset scans and import pages; the complete encoded product mutation, including
// conditions and progress metadata, must fit 4 MiB (below the product request bound).
const PAGE: usize = 4;
const PAGE_BYTES: usize = 4 << 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] Refusal),
    #[error("DogStatsD metadata import refused: {0}; legacy files remain unchanged")]
    Import(String),
    #[error("DogStatsD metadata deadline elapsed; a submitted mutation may still commit; reconcile canonical state before retrying")]
    Unknown,
}

fn unavailable(why: impl Into<String>) -> Error { Refusal::Unavailable(why.into()).into() }
fn imported(why: impl std::fmt::Display) -> Error { Error::Import(why.to_string()) }
fn encoded<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let value = serde_json::to_vec(value).map_err(|e| unavailable(e.to_string()))?;
    if value.len() > VALUE_MAX { return Err(Error::Invalid("DogStatsD metadata exceeds the existing Records value bound".into())); }
    Ok(value)
}
fn name_valid(name: &str) -> Result<(), Error> {
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
        return Err(Error::Invalid("a listener's name is 1 to 64 letters, digits, '-', '_' and '.'".into()));
    }
    Ok(())
}
async fn within<T>(until: Instant, future: impl std::future::Future<Output = Result<T, Error>>) -> Result<T, Error> {
    crate::rt::timeout(until.saturating_duration_since(Instant::now()), future).await.map_err(|_| Error::Unknown)?
}

/// One committed collection snapshot. Its revision fences every derived local/remote view.
#[derive(Debug, Clone)]
pub struct Configuration {
    pub revision: Rev,
    pub listeners: Vec<(String, Listener)>,
}

/// The same declared legacy-source law used by the host's CD import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportAuthority {
    member: Option<u64>,
    source_node: Option<u64>,
}
impl ImportAuthority {
    pub(crate) fn server(replica: Option<&crate::replication::Config>) -> Self {
        Self { member: replica.map(|r| r.id), source_node: replica.and_then(|r| r.bootstrap.as_ref().map(|b| b.source_node)) }
    }
    fn permits(&self, present: bool) -> Result<(), Error> {
        if let Some(member) = self.member {
            match self.source_node {
                Some(source) if source != member => return Err(imported(format!("initialization awaits declared source member {source}, not local member {member}"))),
                None if present => return Err(imported("replicated legacy metadata requires cluster.replica.bootstrap.source_node")),
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    authority: ImportAuthority,
    path: PathBuf,
    digest: Option<String>,
    bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Metadata {
    format: u32,
    source: Identity,
    cursor: Option<String>,
    done: bool,
    count: usize,
    bytes: usize,
}
impl Metadata {
    fn checked(&self) -> Result<(), Error> {
        if self.format != 1 || self.count > LISTENERS || self.bytes > LISTENERS * VALUE_MAX {
            return Err(unavailable("canonical DogStatsD metadata format/count/byte accounting is invalid"));
        }
        Ok(())
    }
}
struct Snapshot {
    row: Row,
    metadata: Metadata,
    all: BTreeMap<String, Listener>,
    rows: BTreeMap<String, (Rev, usize)>,
}

/// No live JSON path, process-local persistence map, database or independent writer.
pub struct Listeners {
    records: Arc<dyn Records>,
    wake: Option<crate::bus::Broadcast<()>>,
}
impl Listeners {
    pub(crate) async fn open(records: Arc<dyn Records>, dir: &Path, authority: ImportAuthority, bus: Option<Arc<dyn kiln_seams::Bus>>) -> Result<Self, Error> {
        let mut store = Self { records, wake: None };
        let until = crate::store::deadline();
        within(until, store.initialize(dir.join("statsd.json"), authority)).await?;
        within(until, store.snapshot()).await?;
        // Existing shared Quiver fanout owns the ephemeral topic, offsets and refusal handling.
        // A refused/missed wake never weakens canonical storage or periodic reconciliation.
        store.wake = bus.and_then(|bus| match crate::bus::Broadcast::on(bus, 1) {
            Ok(wake) => Some(wake),
            Err(error) => { tracing::warn!(%error, "Observe metadata wake topic unavailable; periodic canonical reconciliation remains active"); None }
        });
        Ok(store)
    }

    pub(crate) fn changed(&self) {
        if let Some(wake) = &self.wake { let _ = wake.send(()); }
    }
    pub(crate) fn subscribe(&self) -> Option<crate::bus::Receiver<()>> {
        self.wake.as_ref().map(crate::bus::Broadcast::subscribe)
    }
    /// The small collection row is the revision probe; unchanged collections never reload/hash
    /// listener values or the retired JSON file. A remote report never supplies this authority.
    pub(crate) async fn refresh(&self, known: Option<Rev>) -> Result<Option<Configuration>, Error> {
        within(crate::store::deadline(), async {
            let (row, metadata) = self.metadata(Consistency::Linearizable).await?.ok_or_else(|| unavailable("canonical listener metadata vanished"))?;
            if !metadata.done { return Err(unavailable("canonical listener import is incomplete")); }
            if known == Some(row.rev) { return Ok(None); }
            if known.is_some_and(|previous| row.rev < previous) { return Err(unavailable("canonical listener revision regressed; explicit reconciliation required")); }
            let snapshot = self.snapshot().await?;
            Ok(Some(Configuration { revision: snapshot.row.rev, listeners: snapshot.all.into_iter().collect() }))
        }).await
    }

    async fn metadata(&self, at: Consistency) -> Result<Option<(Row, Metadata)>, Error> {
        let Some(row) = self.records.get(META, KEY, at).await? else { return Ok(None) };
        let metadata: Metadata = serde_json::from_slice(&row.value).map_err(|e| unavailable(format!("canonical DogStatsD metadata is unreadable: {e}")))?;
        metadata.checked()?;
        Ok(Some((row, metadata)))
    }

    async fn snapshot(&self) -> Result<Snapshot, Error> {
        self.snapshot_at(self.records.head().await?, true).await
    }

    async fn snapshot_at(&self, head: Rev, require_done: bool) -> Result<Snapshot, Error> {
        let (row, metadata) = self.metadata(Consistency::Snapshot(head)).await?.ok_or_else(|| unavailable("DogStatsD metadata has not been initialized"))?;
        if require_done && !metadata.done { return Err(unavailable("DogStatsD metadata import is incomplete")); }
        let (mut all, mut rows, mut after, mut bytes) = (BTreeMap::new(), BTreeMap::new(), None, 0_usize);
        loop {
            let page = self.records.scan(FAMILY, Scan { prefix: Vec::new(), after, limit: PAGE, at: Consistency::Snapshot(head) }).await?;
            for row in &page.rows {
                if all.len() >= LISTENERS || row.value.len() > VALUE_MAX { return Err(unavailable("canonical DogStatsD listeners exceed their existing bounds")); }
                let name = std::str::from_utf8(&row.key).map_err(|e| unavailable(e.to_string()))?.to_owned();
                name_valid(&name).map_err(|e| unavailable(e.to_string()))?;
                let listener: Listener = serde_json::from_slice(&row.value).map_err(|e| unavailable(format!("canonical listener is unreadable: {e}")))?;
                listener.check().map_err(unavailable)?;
                bytes = bytes.checked_add(row.value.len()).ok_or_else(|| unavailable("listener byte accounting overflow"))?;
                rows.insert(name.clone(), (row.rev, row.value.len()));
                all.insert(name, listener);
            }
            if !page.more { break; }
            after = Some(page.rows.last().ok_or_else(|| unavailable("empty canonical listener page claims continuation"))?.key.clone());
        }
        if metadata.count != all.len() || metadata.bytes != bytes { return Err(unavailable("canonical listener accounting does not match its snapshot")); }
        transport_bound(&all).map_err(|e| unavailable(e.to_string()))?;
        Ok(Snapshot { row, metadata, all, rows })
    }

    pub async fn list(&self) -> Result<Configuration, Error> {
        within(crate::store::deadline(), async {
            let snapshot = self.snapshot().await?;
            Ok(Configuration { revision: snapshot.row.rev, listeners: snapshot.all.into_iter().collect() })
        }).await
    }

    pub async fn put(&self, name: &str, listener: Listener) -> Result<Configuration, Error> {
        let result = within(crate::store::deadline(), async {
            name_valid(name)?;
            listener.check().map_err(Error::Invalid)?;
            let value = encoded(&listener)?;
            let mut snapshot = self.snapshot().await?;
            if snapshot.all.get(name) == Some(&listener) {
                return Ok(Configuration { revision: snapshot.row.rev, listeners: snapshot.all.into_iter().collect() });
            }
            if !snapshot.all.contains_key(name) && snapshot.all.len() >= LISTENERS { return Err(Error::Invalid(format!("{LISTENERS} listeners at the most"))); }
            let old = snapshot.rows.get(name).copied();
            snapshot.metadata.bytes = snapshot.metadata.bytes.checked_sub(old.map_or(0, |(_, size)| size)).and_then(|n| n.checked_add(value.len())).ok_or_else(|| unavailable("listener byte accounting overflow"))?;
            snapshot.all.insert(name.to_owned(), listener);
            snapshot.metadata.count = snapshot.all.len();
            transport_bound(&snapshot.all)?;
            let object = old.map_or_else(|| Cond::Absent { family: FAMILY.into(), key: name.as_bytes().to_vec() }, |(rev, _)| Cond::At { family: FAMILY.into(), key: name.as_bytes().to_vec(), rev });
            let revision = self.records.commit(Tx {
                conds: vec![Cond::At { family: META.into(), key: KEY.to_vec(), rev: snapshot.row.rev }, object],
                writes: vec![Write::Put { family: FAMILY.into(), key: name.as_bytes().to_vec(), value }, Write::Put { family: META.into(), key: KEY.to_vec(), value: encoded(&snapshot.metadata)? }],
            }).await?;
            Ok(Configuration { revision, listeners: snapshot.all.into_iter().collect() })
        }).await;
        self.changed(); // Unknown outcomes are reconciled by canonical reads, never guessed.
        result
    }

    pub async fn delete(&self, name: &str) -> Result<Option<Configuration>, Error> {
        let result = within(crate::store::deadline(), async {
            name_valid(name)?;
            let mut snapshot = self.snapshot().await?;
            let Some((rev, size)) = snapshot.rows.get(name).copied() else { return Ok(None) };
            snapshot.all.remove(name);
            snapshot.metadata.count = snapshot.all.len();
            snapshot.metadata.bytes = snapshot.metadata.bytes.checked_sub(size).ok_or_else(|| unavailable("listener byte accounting underflow"))?;
            let revision = self.records.commit(Tx {
                conds: vec![Cond::At { family: META.into(), key: KEY.to_vec(), rev: snapshot.row.rev }, Cond::At { family: FAMILY.into(), key: name.as_bytes().to_vec(), rev }],
                writes: vec![Write::Delete { family: FAMILY.into(), key: name.as_bytes().to_vec() }, Write::Put { family: META.into(), key: KEY.to_vec(), value: encoded(&snapshot.metadata)? }],
            }).await?;
            Ok(Some(Configuration { revision, listeners: snapshot.all.into_iter().collect() }))
        }).await;
        self.changed(); // Unknown outcomes are reconciled by canonical reads, never guessed.
        result
    }

    async fn initialize(&self, path: PathBuf, authority: ImportAuthority) -> Result<(), Error> {
        // Canonical Done wins even if the old file is changed, removed or unreadable afterwards.
        if self.metadata(Consistency::Linearizable).await?.is_some_and(|(_, state)| state.done) { return Ok(()); }
        let source_path = path.clone();
        let source = crate::rt::blocking(move || read_source(source_path, authority)).await.map_err(imported)??;
        let mut current = match self.metadata(Consistency::Linearizable).await? {
            Some(current) => current,
            None => {
                let head = self.records.head().await?;
                if !self.records.scan(FAMILY, Scan { prefix: Vec::new(), after: None, limit: 1, at: Consistency::Snapshot(head) }).await?.rows.is_empty() {
                    return Err(imported("canonical listeners already exist without matching import provenance"));
                }
                let metadata = Metadata { format: 1, source: source.identity.clone(), cursor: None, done: false, count: 0, bytes: 0 };
                self.records.commit(Tx { conds: vec![Cond::Absent { family: META.into(), key: KEY.to_vec() }], writes: vec![Write::Put { family: META.into(), key: KEY.to_vec(), value: encoded(&metadata)? }] }).await?;
                self.metadata(Consistency::Linearizable).await?.ok_or_else(|| unavailable("committed listener import state is missing"))?
            }
        };
        loop {
            let (row, mut metadata) = current;
            if metadata.done { return Ok(()); }
            if metadata.source != source.identity { return Err(imported("legacy source or declared authority changed during incomplete import")); }
            let expected_cursor = metadata.count.checked_sub(1).and_then(|at| source.all.keys().nth(at));
            if metadata.count > source.all.len() || expected_cursor != metadata.cursor.as_ref() {
                return Err(imported("canonical import cursor/count does not match its pinned source"));
            }
            // A resumed page must agree with the pinned source, not merely its own counters.
            // This also refuses stray/conflicting rows before marking the import complete.
            let progress = self.snapshot_at(row.rev, false).await?;
            if progress.all.iter().any(|(name, listener)| source.all.get(name) != Some(listener) || metadata.cursor.as_ref().is_none_or(|last| name > last)) {
                return Err(imported("canonical import rows conflict with the pinned source/cursor"));
            }
            let mut tx = Tx { conds: vec![Cond::At { family: META.into(), key: KEY.to_vec(), rev: row.rev }], writes: Vec::new() };
            let after = metadata.cursor.clone();
            for (name, listener) in source.all.iter().filter(|(name, _)| after.as_ref().is_none_or(|after| *name > after)).take(PAGE) {
                let value = encoded(listener)?;
                let mut next = metadata.clone();
                next.count = next.count.checked_add(1).ok_or_else(|| imported("listener import count overflow"))?;
                next.bytes = next.bytes.checked_add(value.len()).ok_or_else(|| imported("listener import byte overflow"))?;
                next.cursor = Some(name.clone());
                next.done = next.count == source.all.len();
                next.checked()?;
                tx.conds.push(Cond::Absent { family: FAMILY.into(), key: name.as_bytes().to_vec() });
                tx.writes.push(Write::Put { family: FAMILY.into(), key: name.as_bytes().to_vec(), value });
                tx.writes.push(Write::Put { family: META.into(), key: KEY.to_vec(), value: encoded(&next)? });
                if import_bytes(&tx)? > PAGE_BYTES {
                    tx.writes.pop();
                    tx.writes.pop();
                    tx.conds.pop();
                    if tx.writes.is_empty() { return Err(imported("one listener plus import progress exceeds the bounded product mutation page")); }
                    break;
                }
                tx.writes.pop(); // Progress is written once, atomically with the accepted page.
                metadata = next;
            }
            metadata.done = metadata.count == source.all.len();
            if metadata.done {
                let (path, authority) = (source.identity.path.clone(), source.identity.authority.clone());
                let identity = crate::rt::blocking(move || identity_at(path, authority).map(|(id, _)| id)).await.map_err(imported)??;
                if identity != source.identity { return Err(imported("legacy source changed before canonical cutover")); }
            }
            tx.writes.push(Write::Put { family: META.into(), key: KEY.to_vec(), value: encoded(&metadata)? });
            if import_bytes(&tx)? > PAGE_BYTES { return Err(imported("listener import progress exceeds the bounded product mutation page")); }
            self.records.commit(tx).await?;
            if metadata.done { return Ok(()); }
            current = self.metadata(Consistency::Linearizable).await?.ok_or_else(|| unavailable("listener import state vanished"))?;
        }
    }
}

// Match the existing seam adapter's public product conversion, including namespace/condition
// overhead. This bounds import work; it does not claim a replicated dirty-page bound.
fn import_bytes(tx: &Tx) -> Result<usize, Error> {
    use amphora_api as api;
    let tx = api::Tx {
        conds: tx.conds.iter().map(|c| match c {
            Cond::Absent { family, key } => api::Cond::Absent { family: family.clone(), key: key.clone() },
            Cond::At { family, key, rev } => api::Cond::At { family: family.clone(), key: key.clone(), rev: *rev },
        }).collect(),
        writes: tx.writes.iter().map(|w| match w {
            Write::Put { family, key, value } => api::Write::Put { family: family.clone(), key: key.clone(), value: value.clone() },
            Write::Delete { family, key } => api::Write::Delete { family: family.clone(), key: key.clone() },
        }).collect(),
    };
    postcard::to_allocvec(&api::Mutation::Records { namespace: "kiln".into(), tx }).map(|bytes| bytes.len()).map_err(imported)
}

/// Each actual destination gets wildcard listeners plus its own selector. There is no invented
/// global one-frame quota when different listeners belong to different machines.
fn transport_bound(all: &BTreeMap<String, Listener>) -> Result<(), Error> {
    if all.len() > LISTENERS { return Err(Error::Invalid(format!("{LISTENERS} listeners at the most"))); }
    let targets: BTreeSet<&str> = std::iter::once(super::SERVER).chain(all.values().map(|l| l.on.as_str()).filter(|on| *on != "*")).collect();
    for target in targets {
        let listeners = all.iter().filter(|(_, l)| l.read_by(target)).map(|(name, l)| (name.clone(), l.clone())).collect();
        let json = serde_json::to_string(&super::super::wire::Msg::Statsd { listeners, revision: Some(u64::MAX) }).map_err(|e| Error::Invalid(e.to_string()))?;
        let bytes = crate::proto::encode(&crate::proto::ServerMsg::Observe { json, bytes: Vec::new() }).map_err(|e| Error::Invalid(e.to_string()))?;
        let limit = <crate::net::Kiln as hyper_transport::Classes>::frame_bound(crate::net::Class::Interactive);
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(Error::Invalid(format!("DogStatsD configuration for {target:?} exceeds the existing {limit}-byte transport frame bound")));
        }
    }
    Ok(())
}

struct Source { identity: Identity, all: BTreeMap<String, Listener> }
fn identity_at(path: PathBuf, authority: ImportAuthority) -> Result<(Identity, Option<Vec<u8>>), Error> {
    let path = std::path::absolute(path).map_err(imported)?;
    let file = match std::fs::File::open(&path) {
        Ok(file) => Some(file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(imported(e)),
    };
    authority.permits(file.is_some())?;
    let Some(file) = file else { return Ok((Identity { authority, path, digest: None, bytes: 0 }, None)) };
    let before = file.metadata().map_err(imported)?;
    let cap = amphora_api::REQUEST_MAX;
    if !before.is_file() || before.len() > cap as u64 { return Err(imported("legacy listener JSON exceeds the existing bounded request input size")); }
    let modified: SystemTime = before.modified().map_err(imported)?;
    let mut bytes = Vec::new();
    (&file).take(cap as u64 + 1).read_to_end(&mut bytes).map_err(imported)?;
    let after = file.metadata().map_err(imported)?;
    if bytes.len() > cap || bytes.len() as u64 != before.len() || after.len() != before.len() || after.modified().map_err(imported)? != modified {
        return Err(imported("legacy listener JSON changed or grew while being read"));
    }
    let identity = Identity { authority, path, digest: Some(crate::util::sha256_hex(&bytes)), bytes: bytes.len() as u64 };
    Ok((identity, Some(bytes)))
}

/// Borrow raw values first so entry/count/fragment bounds precede domain allocations.
struct RawMap<'a>(BTreeMap<String, &'a serde_json::value::RawValue>);
impl<'de> Deserialize<'de> for RawMap<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = RawMap<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("at most sixteen named DogStatsD listeners") }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
                let mut all = BTreeMap::new();
                while let Some(name) = input.next_key::<String>()? {
                    if all.len() >= LISTENERS { return Err(serde::de::Error::custom("legacy listener count exceeds sixteen")); }
                    name_valid(&name).map_err(serde::de::Error::custom)?;
                    let value: &'de serde_json::value::RawValue = input.next_value()?;
                    if value.get().len() > VALUE_MAX { return Err(serde::de::Error::custom("legacy listener JSON fragment exceeds the bounded Records value import size")); }
                    if all.insert(name, value).is_some() { return Err(serde::de::Error::custom("duplicate legacy listener name")); }
                }
                Ok(RawMap(all))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}
fn read_source(path: PathBuf, authority: ImportAuthority) -> Result<Source, Error> {
    let (identity, bytes) = identity_at(path, authority)?;
    let mut all = BTreeMap::new();
    if let Some(bytes) = bytes {
        let raw: RawMap<'_> = serde_json::from_slice(&bytes).map_err(imported)?;
        for (name, value) in raw.0 {
            let listener: Listener = serde_json::from_str(value.get()).map_err(imported)?;
            listener.check().map_err(imported)?;
            encoded(&listener).map_err(imported)?;
            all.insert(name, listener);
        }
    }
    transport_bound(&all).map_err(imported)?;
    Ok(Source { identity, all })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listener(on: &str) -> Listener {
        Listener { listen: vec!["udp://127.0.0.1:8125".into()], tags: vec![], histogram_aggregates: None, histogram_percentiles: None, on: on.into() }
    }
    fn local() -> ImportAuthority { ImportAuthority { member: None, source_node: None } }

    /// U05/U06: the injected host Records owner is authoritative after a paged, read-only
    /// import; deleting/updating a listener survives reopening without resurrecting legacy JSON.
    #[test]
    fn u06_listeners_import_read_only_and_reopen_from_host_records() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("kiln.db");
            let legacy = dir.path().join("statsd.json");
            let all: BTreeMap<_, _> = (0..5).map(|n| (format!("listener{n}"), listener("server"))).collect();
            let original = serde_json::to_vec_pretty(&all)?;
            std::fs::write(&legacy, &original)?;
            let revision = {
                let store = crate::store::Store::open(&path)?;
                let records = Arc::new(crate::records::StoreRecords::open(store)?);
                let kept = Listeners::open(records, dir.path(), local(), None).await?;
                let imported = kept.list().await?;
                anyhow::ensure!(imported.listeners == all.into_iter().collect::<Vec<_>>());
                let changed = kept.put("new", listener("worker")).await?;
                anyhow::ensure!(changed.revision > imported.revision);
                let deleted = kept.delete("listener0").await?.ok_or_else(|| anyhow::anyhow!("imported listener missing"))?;
                anyhow::ensure!(deleted.revision > changed.revision && kept.delete("absent").await?.is_none());
                anyhow::ensure!(std::fs::read(&legacy)? == original, "canonical writes changed the read-only source");
                deleted.revision
            };
            // Completed canonical state wins before any read of a now-corrupt local source or
            // a different local source declaration. It is never merged back into the collection.
            std::fs::write(&legacy, b"not json")?;
            let records = Arc::new(crate::records::StoreRecords::open(crate::store::Store::open(&path)?)?);
            let kept = Listeners::open(records, dir.path(), ImportAuthority { member: Some(2), source_node: Some(1) }, None).await?;
            let reopened = kept.list().await?;
            anyhow::ensure!(reopened.revision == revision && reopened.listeners.len() == 5);
            anyhow::ensure!(reopened.listeners.iter().any(|(name, l)| name == "new" && l.on == "worker"));
            anyhow::ensure!(reopened.listeners.iter().all(|(name, _)| name != "listener0"));
            anyhow::ensure!(std::fs::read(&legacy)? == b"not json");
            Ok(())
        })
    }

    /// U06: failed writes/imports never publish invented empty state. Frame admission measures
    /// JSON escaping and the full outer message for each actual destination independently.
    #[test]
    fn u06_listeners_refuse_bad_sources_and_unfittable_destination_frames() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            use kiln_seams::records::{SimLimits, SimRecords};
            let dir = tempfile::tempdir()?;
            let records = Arc::new(SimRecords::new());
            let foreign = ImportAuthority { member: Some(2), source_node: Some(1) };
            anyhow::ensure!(matches!(Listeners::open(records.clone(), dir.path(), foreign, None).await, Err(Error::Import(_))));
            anyhow::ensure!(records.head().await? == 0, "a follower published an empty replacement");
            std::fs::write(dir.path().join("statsd.json"), br#"{"same":{"listen":["udp://127.0.0.1:8125"]},"same":{"listen":["udp://127.0.0.1:8126"]}}"#)?;
            anyhow::ensure!(matches!(Listeners::open(records.clone(), dir.path(), local(), None).await, Err(Error::Import(_))));
            anyhow::ensure!(records.head().await? == 0, "invalid legacy data mutated canonical storage");
            std::fs::remove_file(dir.path().join("statsd.json"))?;
            // Real partial progress: one unrelated row exhausts room only after the first
            // import page committed. Changed source refuses; releasing that fixture row lets
            // the same public Records owner resume without rewriting the committed prefix.
            let partial = Arc::new(SimRecords::with_limits(SimLimits { rows: 6, ..SimLimits::default() }));
            partial.commit(Tx { conds: vec![], writes: vec![Write::Put { family: "fixture".into(), key: b"room".to_vec(), value: vec![] }] }).await?;
            let source: BTreeMap<_, _> = (0..5).map(|n| (format!("part{n}"), listener("server"))).collect();
            let bytes = serde_json::to_vec(&source)?;
            std::fs::write(dir.path().join("statsd.json"), &bytes)?;
            anyhow::ensure!(matches!(Listeners::open(partial.clone(), dir.path(), local(), None).await, Err(Error::Store(Refusal::Full { .. }))));
            let prefix = partial.scan(FAMILY, Scan { prefix: vec![], after: None, limit: LISTENERS, at: Consistency::Linearizable }).await?;
            anyhow::ensure!(!prefix.rows.is_empty() && prefix.rows.len() < source.len(), "capacity failure did not retain a real partial import");
            let head = partial.head().await?;
            std::fs::write(dir.path().join("statsd.json"), b"{}")?;
            anyhow::ensure!(matches!(Listeners::open(partial.clone(), dir.path(), local(), None).await, Err(Error::Import(_))));
            anyhow::ensure!(partial.head().await? == head, "changed source modified incomplete canonical state");
            std::fs::write(dir.path().join("statsd.json"), &bytes)?;
            partial.commit(Tx { conds: vec![], writes: vec![Write::Delete { family: "fixture".into(), key: b"room".to_vec() }] }).await?;
            let resumed = Listeners::open(partial, dir.path(), local(), None).await?.list().await?;
            anyhow::ensure!(resumed.listeners == source.into_iter().collect::<Vec<_>>());
            std::fs::remove_file(dir.path().join("statsd.json"))?;

            let full = Arc::new(SimRecords::with_limits(SimLimits { rows: 1, ..SimLimits::default() }));
            let kept = Listeners::open(full, dir.path(), local(), None).await?;
            let before = kept.list().await?;
            anyhow::ensure!(matches!(kept.put("refused", listener("server")).await, Err(Error::Store(Refusal::Full { .. }))));
            let after = kept.list().await?;
            anyhow::ensure!(after.listeners.is_empty() && after.revision == before.revision, "failed commit changed served configuration");
            anyhow::ensure!(!dir.path().join("statsd.json").exists(), "canonical initialization created a live JSON writer");

            let mut first = listener("worker-a");
            first.tags = vec![format!("payload:{}", "\\\"".repeat(160 << 10))];
            let mut second = first.clone();
            second.on = "worker-b".into();
            anyhow::ensure!(encoded(&first)?.len() <= VALUE_MAX);
            let mut all = BTreeMap::from([("a".into(), first), ("b".into(), second)]);
            transport_bound(&all)?; // Both fit their own frames despite exceeding one globally.
            if let Some(second) = all.get_mut("b") { second.on = "worker-a".into(); }
            anyhow::ensure!(matches!(transport_bound(&all), Err(Error::Invalid(_))));
            if let Some(second) = all.get_mut("b") { second.on = "*".into(); }
            anyhow::ensure!(matches!(transport_bound(&all), Err(Error::Invalid(_))));

            let old: crate::observe::wire::Msg = serde_json::from_str(r#"{"kind":"statsd","listeners":[]}"#)?;
            anyhow::ensure!(matches!(old, crate::observe::wire::Msg::Statsd { revision: None, .. }));
            // An already-installed listener cannot be stopped by an older configuration or an
            // unversioned old-peer update. No socket/timing fixture is needed for that owner rule.
            let stop = crate::rt::Shutdown::new();
            let running = super::super::Running { listener: listener("server"), stop: stop.clone(), state: Arc::new(std::sync::Mutex::new(super::super::State::default())) };
            let runtime = super::super::Statsd {
                machine: "server".into(), host: "host".into(),
                running: std::sync::Mutex::new(std::collections::HashMap::from([("installed".into(), running)])),
                revision: std::sync::Mutex::new(Some(9)), stop: crate::rt::Shutdown::new(),
            };
            runtime.set(&[], Some(8));
            runtime.set(&[], None);
            anyhow::ensure!(runtime.states().len() == 1 && !stop.is_cancelled());
            runtime.set(&[], Some(10));
            anyhow::ensure!(runtime.states().is_empty() && stop.is_cancelled());
            Ok(())
        })
    }
}
