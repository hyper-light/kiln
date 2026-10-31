//! U17/U19: host-affine desired control, bounded activity and durable resolution intents.
//! The shared controller lease and private instance.id never come from replicated Records.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use amphora::database::{Database, Namespace};
use amphora_api::{Cond, Consistency, Records as _, Tx, Write};
use serde::{Deserialize, Serialize};
use super::{Activity, Controller, Repo, Step};

const FAMILY: &str = "control";
const HEADER: &[u8] = b"import-v1";
const CHUNK: usize = 256 << 10;
const SOURCE_MAX: usize = 8 << 20;
const SNAPSHOT_MAX: usize = 24 << 20;
fn deadline() -> Instant { Instant::now() + Duration::from_secs(30) }
pub(super) fn namespace(instance: &str) -> String {
    let hash = crate::util::sha256_hex(instance.to_ascii_lowercase().as_bytes());
    format!("sync-{}", hash.chars().take(48).collect::<String>())
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Resolution { pub id: String, pub repo: String, pub step: Step, pub text: String }
struct Legacy { state: State, sources: super::super::activity::SourceDigests }
pub(super) struct State {
    pub repos: Vec<Repo>, pub activity: Activity, pub away: HashSet<(String, String)>,
    pub recorded: HashMap<String, u64>, pub resolutions: Vec<Resolution>,
}
pub(super) struct Snapshot<'a> {
    pub repos: &'a [Repo], pub activity: &'a Activity, pub away: &'a HashSet<(String, String)>,
    pub recorded: &'a HashMap<String, u64>, pub resolutions: &'a [Resolution],
}
impl State { pub fn snapshot(&self) -> Snapshot<'_> { Snapshot { repos: &self.repos, activity: &self.activity, away: &self.away, recorded: &self.recorded, resolutions: &self.resolutions } } }
#[derive(Deserialize)]
struct Header {
    version: u32, source_host: String, instance: String, chunks: BTreeMap<String, usize>,
    repos: Vec<Repo>, next: u64, away: HashSet<(String, String)>, recorded: HashMap<String, u64>, resolutions: Vec<Resolution>,
}
#[derive(Serialize)]
struct HeadWrite<'a> {
    version: u32, source_host: &'a str, instance: &'a str, chunks: &'a BTreeMap<String, usize>,
    repos: &'a [Repo], next: u64, away: &'a HashSet<(String, String)>, recorded: &'a HashMap<String, u64>, resolutions: &'a [Resolution],
}
pub(super) struct Metadata {
    records: Namespace, source_host: String, instance: String, rev: Option<u64>,
    chunks: BTreeMap<String, Vec<Vec<u8>>>, header: Vec<u8>, _controller: Arc<Controller>, _owner: Option<super::controller::SyncOwner>,
}
impl Metadata {
    #[cfg(test)]
    pub fn open(database: &Database, dir: &Path, source_host: String) -> anyhow::Result<(Self, State)> {
        let controller = Controller::open(dir)?;
        anyhow::ensure!(controller.active(), "sync controller unavailable: another owner holds the transfer lease");
        Self::from_controller(database, dir, source_host, controller)
    }
    pub fn from_controller(database: &Database, dir: &Path, source_host: String, controller: Arc<Controller>) -> anyhow::Result<(Self, State)> {
        anyhow::ensure!(controller.active() && controller.dir() == dir.canonicalize()?, "sync controller unavailable or transfer directory mismatch");
        let owner = controller.claim_sync()?;
        let mut this = Self::reader(database, dir, source_host, controller, true)?;
        this._owner = Some(owner);
        if let Some(state) = this.load()? { return Ok((this, state)); }
        let read = || -> anyhow::Result<Legacy> {
            let bytes = read_source(&dir.join("repos.json"))?;
            let repos: Vec<Repo> = bytes.as_ref().map(|b| serde_json::from_slice(b)).transpose()?.unwrap_or_default();
            validate(&repos)?;
            // U19: aggregate source sizes checked before allocating any history buffers.
            let mut total = bytes.as_ref().map_or(0, Vec::len);
            for repo in &repos {
                let path = dir.join(format!("{}.activity.json", repo.id));
                if let Ok(m) = std::fs::metadata(&path) {
                    let len = usize::try_from(m.len())?;
                    anyhow::ensure!(len <= SOURCE_MAX, "legacy sync activity exceeds source budget");
                    total = total.checked_add(len).ok_or_else(|| anyhow::anyhow!("sync source size overflow"))?;
                    anyhow::ensure!(total <= SNAPSHOT_MAX, "legacy sync metadata exceeds aggregate budget");
                }
            }
            let (activity, mut sources) = Activity::import_checked(dir, repos.iter().map(|r| r.id.clone()), SNAPSHOT_MAX.saturating_sub(bytes.as_ref().map_or(0, Vec::len)))?;
            sources.insert(0, ("repos.json".into(), bytes.as_ref().map(|b| crate::util::sha256_hex(b))));
            Ok(Legacy { state: State { away: activity.away(), recorded: activity.recorded(), repos, activity, resolutions: Vec::new() }, sources })
        };
        let first = read()?;
        let again = read()?;
        anyhow::ensure!(first.sources == again.sources, "legacy sync metadata changed during import");
        this.save(first.state.snapshot())?;
        Ok((this, first.state))
    }
    /// Standby opens only canonical records and an existing private identity: no import/bootstrap.
    pub fn read_only(database: &Database, dir: &Path, source_host: String, controller: Arc<Controller>) -> anyhow::Result<Option<State>> {
        anyhow::ensure!(!source_host.trim().is_empty(), "sync requires authenticated source-host provenance");
        let identity = dir.parent().ok_or_else(|| anyhow::anyhow!("sync directory has no host directory"))?.join("instance.id");
        if read_limited(&identity, 66)?.is_none() { return Ok(None); }
        Self::reader(database, dir, source_host, controller, false)?.load()
    }
    fn reader(database: &Database, dir: &Path, source_host: String, controller: Arc<Controller>, bootstrap: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(!source_host.trim().is_empty(), "sync requires authenticated source-host provenance");
        let identity = dir.parent().ok_or_else(|| anyhow::anyhow!("sync directory has no host directory"))?.join("instance.id");
        let instance = match read_limited(&identity, 66)? {
            Some(bytes) => String::from_utf8(bytes)?.trim().to_owned(),
            None if bootstrap => {
                let id = hex::encode(rand::random::<[u8; 16]>());
                let mut f = OpenOptions::new().write(true).create_new(true).open(identity)?;
                f.write_all(id.as_bytes())?; f.sync_all()?; id
            }
            None => return Err(anyhow::anyhow!("standby has no local transfer identity; active owner must bootstrap it")),
        };
        anyhow::ensure!((16..=64).contains(&instance.len()) && instance.bytes().all(|b| b.is_ascii_hexdigit()), "invalid private sync instance.id");
        Ok(Self { records: database.namespace(&namespace(&instance))?, source_host, instance, rev: None, chunks: BTreeMap::new(), header: Vec::new(), _controller: controller, _owner: None })
    }
    fn load(&mut self) -> anyhow::Result<Option<State>> {
        let Some(row) = self.records.get(FAMILY, HEADER, Consistency::Linearizable, deadline())? else { return Ok(None) };
        let h: Header = serde_json::from_slice(&row.value)?;
        anyhow::ensure!(h.version == 1 && h.source_host == self.source_host && h.instance == self.instance, "sync metadata belongs to another source host or local transfer instance");
        validate(&h.repos)?;
        anyhow::ensure!(h.chunks.len() <= super::disk::GROUPS && h.chunks.values().all(|n| *n <= SOURCE_MAX / CHUNK), "invalid canonical history bounds");
        let declared = h.chunks.values().try_fold(row.value.len(), |n, chunks| n.checked_add(chunks.saturating_mul(CHUNK))).ok_or_else(|| anyhow::anyhow!("sync snapshot size overflow"))?;
        anyhow::ensure!(declared <= SNAPSHOT_MAX, "canonical sync snapshot exceeds aggregate budget");
        let mut histories = HashMap::new();
        for (id, count) in &h.chunks {
            anyhow::ensure!(h.repos.iter().any(|r| &r.id == id), "history for unknown sync repo");
            let mut chunks = Vec::new();
            for i in 0..*count {
                let chunk = self.records.get(FAMILY, &chunk_key(id, i), Consistency::Snapshot(row.rev), deadline())?.ok_or_else(|| anyhow::anyhow!("incomplete canonical sync history"))?;
                anyhow::ensure!(chunk.value.len() <= CHUNK, "invalid canonical sync history chunk"); chunks.push(chunk.value);
            }
            let ring: VecDeque<super::Event> = serde_json::from_slice(&chunks.concat())?;
            histories.insert(id.clone(), ring); self.chunks.insert(id.clone(), chunks);
        }
        let (mut intent_ids, mut intent_repos) = (HashSet::new(), HashSet::new());
        anyhow::ensure!(h.resolutions.len() <= super::disk::GROUPS && h.resolutions.iter().all(|r| !r.id.is_empty() && intent_ids.insert(&r.id) && intent_repos.insert(&r.repo) && h.repos.iter().any(|repo| repo.id == r.repo) && r.step.member == super::SERVER && r.step.seq > 0 && super::group::entry(&r.step).len() <= super::ENTRY), "invalid canonical resolution intents");
        self.rev = Some(row.rev); self.header = row.value;
        Ok(Some(State { repos: h.repos, activity: Activity::from_saved(histories, h.next)?, away: h.away, recorded: h.recorded, resolutions: h.resolutions }))
    }
    pub fn active(&self) -> bool { self._controller.active() }
    pub fn fence(&self) { self._controller.fence(); }
    pub fn save(&mut self, state: Snapshot<'_>) -> anyhow::Result<()> {
        anyhow::ensure!(self._controller.active(), "sync controller unavailable");
        let mut counts: BTreeMap<String, usize> = self.chunks.iter().map(|(id, c)| (id.clone(), c.len())).collect();
        // U19: count bounded serialization before allocating any changed history payloads.
        for id in state.activity.changed() {
            if let Some(ring) = state.activity.ring(id) { counts.insert(id.clone(), json_size(ring, SOURCE_MAX)?.div_ceil(CHUNK)); }
            else { counts.remove(id); }
        }
        let head = HeadWrite { version: 1, source_host: &self.source_host, instance: &self.instance, chunks: &counts, repos: state.repos, next: state.activity.next(), away: state.away, recorded: state.recorded, resolutions: state.resolutions };
        let header_size = json_size(&head, amphora_api::VALUE_MAX)?;
        let total = counts.values().try_fold(header_size, |n, c| n.checked_add(c.saturating_mul(CHUNK))).ok_or_else(|| anyhow::anyhow!("sync snapshot overflow"))?;
        anyhow::ensure!(total <= SNAPSHOT_MAX && state.resolutions.len() <= super::disk::GROUPS, "sync metadata exceeds aggregate snapshot budget");
        let header = json_bounded(&head, amphora_api::VALUE_MAX)?;
        let mut changed = BTreeMap::new();
        for id in state.activity.changed() {
            let chunks = if let Some(ring) = state.activity.ring(id) { json_bounded(ring, SOURCE_MAX)?.chunks(CHUNK).map(<[u8]>::to_vec).collect() } else { Vec::new() };
            changed.insert(id.clone(), chunks);
        }
        if header == self.header && changed.is_empty() { return Ok(()); }
        let cond = self.rev.map_or_else(|| Cond::Absent { family: FAMILY.into(), key: HEADER.to_vec() }, |rev| Cond::At { family: FAMILY.into(), key: HEADER.to_vec(), rev });
        let mut writes = vec![Write::Put { family: FAMILY.into(), key: HEADER.to_vec(), value: header.clone() }];
        for (id, chunks) in &changed {
            let previous = self.chunks.get(id);
            for (i, chunk) in chunks.iter().enumerate() {
                if previous.and_then(|c| c.get(i)) != Some(chunk) { writes.push(Write::Put { family: FAMILY.into(), key: chunk_key(id, i), value: chunk.clone() }); }
            }
            for i in chunks.len()..previous.map_or(0, Vec::len) { writes.push(Write::Delete { family: FAMILY.into(), key: chunk_key(id, i) }); }
        }
        let rev = self.records.commit(Tx { conds: vec![cond], writes }, deadline())?;
        self.rev = Some(rev); self.header = header;
        for (id, chunks) in changed { if chunks.is_empty() { self.chunks.remove(&id); } else { self.chunks.insert(id, chunks); } }
        Ok(())
    }
}
fn chunk_key(id: &str, i: usize) -> Vec<u8> { format!("activity/{id}/{i:06}").into_bytes() }
pub(in crate::sync) fn read_source(path: &Path) -> anyhow::Result<Option<Vec<u8>>> { read_limited(path, SOURCE_MAX) }
pub(in crate::sync) fn read_source_with_budget(path: &Path, budget: usize) -> anyhow::Result<Option<Vec<u8>>> { read_limited(path, SOURCE_MAX.min(budget)) }
fn read_limited(path: &Path, limit: usize) -> anyhow::Result<Option<Vec<u8>>> {
    let mut file = match File::open(path) { Ok(f) => f, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(e) => return Err(e.into()) };
    let len = usize::try_from(file.metadata()?.len())?;
    anyhow::ensure!(len <= limit, "sync source exceeds allocation budget");
    let mut bytes = Vec::new(); bytes.try_reserve_exact(len)?; bytes.resize(len, 0); file.read_exact(&mut bytes)?;
    let mut extra = [0u8; 1]; anyhow::ensure!(file.read(&mut extra)? == 0, "sync source grew during read"); Ok(Some(bytes))
}
struct SizeOnly { size: usize, limit: usize }
impl std::io::Write for SizeOnly {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.size) { return Err(std::io::Error::other("sync JSON exceeds allocation budget")); }
        self.size += bytes.len(); Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
fn json_size(value: &impl Serialize, limit: usize) -> anyhow::Result<usize> { let mut out = SizeOnly { size: 0, limit }; serde_json::to_writer(&mut out, value)?; Ok(out.size) }
struct Bounded { bytes: Vec<u8>, limit: usize }
impl std::io::Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) { return Err(std::io::Error::other("sync JSON exceeds allocation budget")); }
        self.bytes.extend_from_slice(bytes); Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
fn json_bounded(value: &impl Serialize, limit: usize) -> anyhow::Result<Vec<u8>> { let mut out = Bounded { bytes: Vec::new(), limit }; serde_json::to_writer(&mut out, value)?; Ok(out.bytes) }
fn validate(repos: &[Repo]) -> anyhow::Result<()> {
    anyhow::ensure!(repos.len() <= super::disk::GROUPS, "too many synced repositories");
    let (mut ids, mut groups, mut names) = (HashSet::new(), HashSet::new(), HashSet::new());
    for repo in repos {
        anyhow::ensure!(!repo.id.is_empty() && repo.id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') && ids.insert(&repo.id) && groups.insert(repo.group) && repo.group > 0 && repo.next_member > super::SERVER && !repo.name.trim().is_empty() && names.insert(&repo.name), "invalid or duplicate sync repository identity");
        let mut members = HashSet::new();
        let mut locations = HashSet::new();
        for copy in &repo.copies {
            anyhow::ensure!(copy.member > super::SERVER && copy.member < repo.next_member && members.insert(copy.member) && !copy.machine.trim().is_empty() && locations.insert((&copy.machine, &copy.path)), "invalid sync copy identity/member allocation");
            anyhow::ensure!(copy.path.starts_with('/') || matches!(copy.path.get(1..3), Some(":\\" | ":/")), "invalid sync copy path");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::activity::{Event, Kind, Tone, KEEP};

    fn repo() -> Repo {
        Repo { id: "repo-1".into(), name: "kiln".into(), group: 1, created: 42, next_member: 3,
            copies: vec![super::super::CopyRef { member: 2, machine: "runner".into(), path: "/src/kiln".into(), added: 7, adopt: Some(23) }],
            exclude: vec!["build".into()], mode: crate::sync::Mode::FilesAndGit, transient: Some("moving".into()) }
    }

    fn event() -> Event {
        Event { seq: 81, repo: "repo-1".into(), at: 123, kind: Kind::State, machine: "runner".into(), text: "offline".into(), tone: Tone::Warn, step: None, state: Some("offline".into()) }
    }

    #[test]
    fn u19_source_and_serialization_budgets_refuse_before_payload_allocation() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let file = temp.path().join("source.json");
        std::fs::write(&file, b"12345")?;
        assert!(read_limited(&file, 4).is_err());
        assert_eq!(read_limited(&file, 5)?, Some(b"12345".to_vec()));
        assert!(json_size(&"12345", 4).is_err());
        assert!(json_bounded(&"12345", 4).is_err());
        Ok(())
    }

    #[test]
    fn u17_import_restart_keeps_control_history_and_private_transfer_artifacts() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("sync");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(temp.path().join("instance.id"), "0123456789abcdef0123456789abcdef")?;
        let repos = serde_json::to_vec(&vec![repo()])?;
        let history = serde_json::to_vec(&vec![event()])?;
        std::fs::write(dir.join("repos.json"), &repos)?;
        std::fs::write(dir.join("repo-1.activity.json"), &history)?;
        std::fs::write(dir.join("repo-1.tree.json"), b"essential compacted checkpoint")?;
        std::fs::write(dir.join("sync.log"), b"transfer log")?;
        let db_path = temp.path().join("amphora");
        {
            let db = Database::open(&db_path, deadline())?;
            let (mut metadata, mut state) = Metadata::open(&db, &dir, "authenticated-host".into())?;
            assert_eq!(state.repos, vec![repo()]);
            assert_eq!(state.activity.since(0, None, 10), vec![event()]);
            assert_eq!(state.activity.next(), 82);
            assert!(state.away.contains(&("repo-1".into(), "runner".into())));
            // Independent dedup/away watermarks outlive eviction of their history events.
            state.recorded.insert("repo-1".into(), 456);
            for _ in 0..KEEP + 1 { state.activity.push(Event { kind: Kind::Copy, state: None, ..event() }); }
            state.repos.iter_mut().flat_map(|r| &mut r.copies).for_each(|c| c.adopt = None);
            metadata.save(state.snapshot())?;
            assert_eq!(std::fs::read(dir.join("repos.json"))?, repos);
            assert_eq!(std::fs::read(dir.join("repo-1.activity.json"))?, history);
        }
        // Completed imports ignore even subsequently corrupt legacy files; no second writer.
        std::fs::write(dir.join("repos.json"), b"obsolete")?;
        std::fs::write(dir.join("repo-1.activity.json"), b"obsolete")?;
        let db = Database::open(&db_path, deadline())?;
        let (metadata, state) = Metadata::open(&db, &dir, "authenticated-host".into())?;
        assert_eq!(state.repos.first().and_then(|r| r.copies.first()).and_then(|c| c.adopt), None);
        assert_eq!(state.activity.next(), 82 + KEEP as u64 + 1);
        assert_eq!(state.activity.since(0, None, usize::MAX).len(), KEEP);
        assert_eq!(state.recorded.get("repo-1"), Some(&456));
        assert!(state.away.contains(&("repo-1".into(), "runner".into())));
        assert_eq!(std::fs::read(dir.join("repo-1.tree.json"))?, b"essential compacted checkpoint");
        assert_eq!(std::fs::read(dir.join("sync.log"))?, b"transfer log");
        assert!(Metadata::open(&db, &dir, "authenticated-host".into()).is_err(), "same-log second controller fenced");
        drop(metadata);
        assert!(Metadata::open(&db, &dir, "different-host".into()).is_err(), "copied local identity cannot activate on another host");
        assert_eq!(std::fs::read_to_string(temp.path().join("instance.id"))?, "0123456789abcdef0123456789abcdef");
        Ok(())
    }

    #[test]
    fn u17_corrupt_import_has_no_marker_and_refused_commit_keeps_canonical_state() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let dir = temp.path().join("sync");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("repos.json"), serde_json::to_vec(&vec![repo()])?)?;
        std::fs::write(dir.join("repo-1.activity.json"), b"broken")?;
        let db = Database::in_memory(deadline())?;
        assert!(Metadata::open(&db, &dir, "host".into()).is_err());
        let id = std::fs::read_to_string(temp.path().join("instance.id"))?;
        let records = db.namespace(&namespace(&id))?;
        assert!(records.get(FAMILY, HEADER, Consistency::Linearizable, deadline())?.is_none());
        std::fs::write(dir.join("repo-1.activity.json"), serde_json::to_vec(&vec![event()])?)?;
        let (mut metadata, mut state) = Metadata::open(&db, &dir, "host".into())?;
        // Competing committed authority changes the header revision: stale writes must fail CAS.
        let header = records.get(FAMILY, HEADER, Consistency::Linearizable, deadline())?.ok_or_else(|| anyhow::anyhow!("missing header"))?;
        records.commit(Tx { conds: vec![], writes: vec![Write::Put { family: FAMILY.into(), key: HEADER.to_vec(), value: header.value }] }, deadline())?;
        state.repos.clear();
        assert!(metadata.save(state.snapshot()).is_err());
        drop(metadata);
        let (_, state) = Metadata::open(&db, &dir, "host".into())?;
        assert_eq!(state.repos, vec![repo()]);
        Ok(())
    }
}
