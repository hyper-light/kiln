//! U15/B2: bounded read-only legacy input. This module never opens the old Store or a writer.
//! Product recovery runs against the shared compatibility reader's memory overlay. Sources
//! remain byte-for-byte unchanged, fenced by a content identity on every reopen.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use hyper_log::{Config, Waits};
#[cfg(test)]
use hyper_log::format::{self, FrameHeader, Persist, SegmentHeader};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{Data, Manifest, Namespace, RETRIES, load, save};

const GROUP: u128 = 1;
const LOG_ID: u128 = 0x006b_696c_6e2d_6364;
const SEGMENT: usize = 1 << 20;
#[cfg(test)]
const BLOCK: usize = 4096;
const MAX_LOG: u64 = 257 << 20;
const MAX_IMAGE: u64 = 24 << 20;
const PAGE: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Identity {
    directory: PathBuf, image: Option<Fingerprint>, log: Option<Fingerprint>,
    /// U15/B2: member/path fence an unfinished import. After cutover only content receipts
    /// validate any remaining local inputs; canonical state does not depend on their location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    member: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint { bytes: u64, digest: String }
#[derive(Deserialize)]
struct Image { index: u64, term: u64, data: Data }
type Entries = BTreeMap<u64, (u64, Vec<u8>)>;
struct Source {
    identity: Identity, index: u64, data: Data, commit: u64,
    entries: Entries,
    inspection: Option<crate::replication::legacy_read::Source>,
}
struct Inspected { commit: u64, entries: Entries, source: crate::replication::legacy_read::Source }

fn fingerprint(path: &Path, max: u64) -> anyhow::Result<Option<Fingerprint>> {
    let mut file = match File::open(path) { Ok(file) => file, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(e) => return Err(e.into()) };
    let before = file.metadata()?;
    anyhow::ensure!(before.len() <= max, "legacy CD source exceeds its bounded import size");
    let mut hash = Sha256::new(); let mut buf = vec![0; 64 << 10]; let mut bytes = 0_u64;
    loop { let n = file.read(&mut buf)?; if n == 0 { break; } let part = buf.get(..n).context("legacy source read outside buffer")?; hash.update(part); bytes = bytes.saturating_add(n as u64); anyhow::ensure!(bytes <= max, "legacy CD source grew during import"); }
    anyhow::ensure!(bytes == before.len() && file.metadata()?.modified()? == before.modified()?, "legacy CD source changed while being read");
    Ok(Some(Fingerprint { bytes, digest: hex::encode(hash.finalize()) }))
}

fn identity_at(dir: &Path, member: Option<u64>) -> anyhow::Result<Identity> {
    let directory = if dir.exists() { dir.canonicalize()? } else if dir.is_absolute() { dir.to_owned() } else { std::env::current_dir()?.join(dir) };
    Ok(Identity { image: fingerprint(&dir.join("cd.state"), MAX_IMAGE)?, log: fingerprint(&dir.join("cd.log"), MAX_LOG)?, directory, member })
}

#[cfg(test)]
fn identity(dir: &Path) -> anyhow::Result<Identity> { identity_at(dir, None) }

impl Source {
    fn open(dir: &Path, member: Option<u64>) -> anyhow::Result<Self> {
        let source_id = identity_at(dir, member)?;
        let image = if let Some(expected) = &source_id.image {
            // U15/B2: a racing old writer cannot turn the bounded snapshot import into an
            // unbounded allocation between fingerprinting and the actual decode read.
            let mut bytes = Vec::new();
            File::open(dir.join("cd.state"))?.take(MAX_IMAGE + 1).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() as u64 == expected.bytes && crate::util::sha256_hex(&bytes) == expected.digest, "legacy CD snapshot changed between fingerprint and read");
            Some(serde_json::from_slice::<Image>(&bytes)?)
        } else { None };
        let (index, term, data) = image.map_or((0, 0, Data::default()), |i| (i.index, i.term, i.data));
        let (commit, entries, inspection) = if let Some(expected) = &source_id.log {
            let inspected = read_log(&dir.join("cd.log"), expected, index, term)?;
            (inspected.commit, inspected.entries, Some(inspected.source))
        } else { (index, BTreeMap::new(), None) };
        anyhow::ensure!(identity_at(dir, member)? == source_id, "legacy CD source changed during inspection");
        Ok(Self { identity: source_id, index, data, commit, entries, inspection })
    }
}

fn has_source(dir: &Path) -> anyhow::Result<bool> {
    Ok(dir.join("cd.state").try_exists()? || dir.join("cd.log").try_exists()?)
}

fn verify_completed(receipt: &Identity, dir: &Path) -> anyhow::Result<()> {
    let image = fingerprint(&dir.join("cd.state"), MAX_IMAGE)?;
    let log = fingerprint(&dir.join("cd.log"), MAX_LOG)?;
    // U15/B2/U19: absent inputs are retired, not another source election. Present files must
    // match the committed import receipt on every member; paths never establish authority.
    for (name, current, expected) in [("snapshot", image.as_ref(), receipt.image.as_ref()), ("log", log.as_ref(), receipt.log.as_ref())] {
        anyhow::ensure!(current.is_none_or(|current| Some(current) == expected), "legacy CD {name} differs from its completed import receipt; retain both stores and obtain explicit domain reconciliation before importing divergent data");
    }
    Ok(())
}

pub(super) fn import_at(namespace: &Namespace, dir: &Path, member: Option<u64>, source_node: Option<u64>) -> anyhow::Result<()> {
    anyhow::ensure!(source_node != Some(0), "CD migration requires a nonzero declared source node");
    if let Some((_, manifest, _)) = load(namespace)? {
        if !manifest.importing { return verify_completed(&manifest.source, dir); }
        anyhow::ensure!(manifest.source.member == member, "CD import is unfinished on source member {:?}; another replica cannot resume it from local legacy files", manifest.source.member);
        let current = identity_at(dir, member)?;
        anyhow::ensure!(manifest.source == current, "legacy CD source changed during its unfinished import; preserve both stores and restore the pinned source before resuming");
    } else if let Some(member) = member {
        if let Some(source) = source_node {
            anyhow::ensure!(member == source, "CD initialization awaits its declared legacy source member {source}; local member {member} cannot publish an empty replacement");
        } else {
            anyhow::ensure!(!has_source(dir)?, "replicated CD legacy import requires an explicit cluster.replica.bootstrap.source_node; source files retained");
        }
    }
    let source = Source::open(dir, member)?;
    import_source(namespace, &source, || identity_at(dir, member).map(|now| now == source.identity))
}

#[cfg(test)]
fn import(namespace: &Namespace, dir: &Path) -> anyhow::Result<()> { import_at(namespace, dir, None, None) }

fn import_source(namespace: &Namespace, source: &Source, unchanged: impl Fn() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    for _ in 0..RETRIES {
        if load(namespace)?.is_none() {
            let manifest = Manifest { format: 1, bytes: 0, pieces: 0, digest: String::new(), changes: Vec::new(), source: source.identity.clone(), cursor: source.index, importing: true };
            match save(namespace, 0, &source.data, manifest) { Ok(_) => {}, Err(amphora_api::Refusal::Conflict { .. }) => continue, Err(e) => return Err(e.into()) }
        }
        loop {
            let (rev, mut manifest, mut data) = load(namespace)?.context("CD import state vanished")?;
            if !manifest.importing { return verify_completed(&manifest.source, &source.identity.directory); }
            anyhow::ensure!(manifest.source == source.identity && manifest.cursor >= source.index && manifest.cursor <= source.commit, "CD import source/cursor mismatch");
            let end = manifest.cursor.saturating_add(PAGE as u64).min(source.commit);
            for index in manifest.cursor.saturating_add(1)..=end {
                let (term, bytes) = source.entries.get(&index).context("legacy CD committed entry is missing; cutover refused")?;
                let entry = hyper_durable::decode_entry(index, *term, bytes).context("legacy CD committed entry is unreadable")?;
                // Membership changes have no domain command. Legacy state application also
                // advances rejected commands without changing Data; preserve that result.
                if entry.kind == hyper_raft::proto::EntryType::EntryNormal && !entry.data.is_empty()
                    && let Ok(command) = serde_json::from_slice(entry.data)
                { let _ = data.apply(command, &mut Vec::new()); }
            }
            manifest.cursor = end;
            manifest.importing = end != source.commit;
            if !manifest.importing {
                if let Some(inspection) = &source.inspection { inspection.verify()?; }
                anyhow::ensure!(unchanged()?, "legacy CD source changed before cutover; destination remains fenced");
            }
            match save(namespace, rev, &data, manifest.clone()) {
                Ok(_) if !manifest.importing => return Ok(()),
                Ok(_) => {},
                Err(amphora_api::Refusal::Conflict { .. }) => break,
                Err(e) => return Err(e.into()),
            }
        }
    }
    anyhow::bail!("CD import cursor kept changing; another importer may be active")
}

/// U15/B2: recover through the product's compatibility overlay, never an original-file writer.
/// Keep the recovered bytes bounded and retain its generation fence through canonical cutover.
fn read_log(path: &Path, expected_source: &Fingerprint, index: u64, term: u64) -> anyhow::Result<Inspected> {
    let config = Config {
        segment_bytes: SEGMENT as u64, max_segments: 256, max_groups: 8,
        group_entries: 1 << 18, group_bytes: 1 << 27, group_cache: 1 << 23,
        queue_submissions: 32, waits: Waits::Never,
    };
    let log = crate::replication::legacy_read::LegacyLog::open(path, config, LOG_ID, GROUP)?.context("legacy CD log vanished during inspection")?;
    let (commit, after) = if let Some(view) = log.view() {
        let commit = view.hard_state.map_or(view.start.index, |state| state.commit.max(view.start.index)).max(view.released);
        anyhow::ensure!(index >= view.start.index, "legacy CD log was compacted past its available snapshot");
        anyhow::ensure!(index <= commit, "legacy CD snapshot is ahead of the committed log");
        if index == view.start.index {
            anyhow::ensure!(view.start.term == term, "legacy CD snapshot term disagrees with compacted log");
            (commit, index)
        } else {
            // The exact shared API exposes view/replay. Replay the snapshot boundary once
            // to verify its retained term, then apply only entries strictly after it.
            (commit, index.checked_sub(1).context("legacy CD snapshot boundary underflow")?)
        }
    } else {
        anyhow::ensure!(index == 0, "legacy CD snapshot has no corresponding log group");
        (0, 0)
    };
    let mut entries = BTreeMap::new();
    let mut bytes = 0_usize;
    let mut boundary_checked = after == index;
    let inspection = log.replay(after, |at, held_term, entry| {
        if at == index {
            anyhow::ensure!(held_term == term, "legacy CD snapshot term disagrees with its log");
            boundary_checked = true;
            return Ok(());
        }
        anyhow::ensure!(at > index, "legacy CD replay precedes its snapshot boundary");
        bytes = bytes.checked_add(entry.len()).context("legacy CD committed suffix size overflow")?;
        anyhow::ensure!(bytes <= 1 << 27 && entries.len() < 1 << 18, "legacy CD committed suffix exceeds its bounded import size");
        anyhow::ensure!(entries.insert(at, (held_term, entry.to_vec())).is_none(), "legacy CD replay repeated an index");
        Ok(())
    })?;
    anyhow::ensure!(boundary_checked, "legacy CD snapshot boundary is missing from its log");
    // Validate against the identity computed before opening recovery, before publishing even
    // a fenced partial import. A later restored source cannot bless bytes from another source.
    anyhow::ensure!(inspection.digest() == expected_source.digest, "legacy CD log changed between fingerprint and decode");
    inspection.verify()?;
    Ok(Inspected { commit, entries, source: inspection })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_cd::state::{Command, Deployment};
    use serde_json::json;

    fn scratch() -> anyhow::Result<tempfile::TempDir> {
        let cache = PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache");
        std::fs::create_dir_all(&cache)?;
        Ok(tempfile::tempdir_in(cache)?)
    }

    /// A real format-3 source, produced from the public wire format without ever starting an
    /// old CD Store, Raft member or log writer. The final entry is durable but uncommitted.
    fn fixture(dir: &Path, commands: &[Command], committed: usize, snapshot: Data) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let image = json!({ "index":1, "term":1, "configuration":{"voters":[1],"learners":[],"voters_outgoing":[],"learners_next":[],"auto_leave":false}, "data":snapshot });
        std::fs::write(dir.join("cd.state"), serde_json::to_vec(&image)?)?;
        let mut encoded = vec![hyper_durable::encode_entry(&hyper_raft::proto::Entry { index:1, term:1, ..Default::default() }).context("fixture no-op")?];
        for (n, command) in commands.iter().enumerate() {
            encoded.push(hyper_durable::encode_entry(&hyper_raft::proto::Entry { index:n as u64 + 2, term:1, data:serde_json::to_vec(command)?, ..Default::default() }).context("fixture entry")?);
        }
        let entries = encoded.iter().map(|e| (1, e.as_slice())).collect::<Vec<_>>();
        let state = format::HardState { term:1, vote:1, commit:committed as u64 + 1, ..Default::default() };
        let mut payload = hyper_log::codec::Writer::with_capacity(4096);
        format::put(&mut payload, &format::Record::Entries { group:GROUP, first:1, entries:&entries }, 0).context("fixture entries")?;
        format::put(&mut payload, &format::Record::HardState { group:GROUP, state }, 0).context("fixture hard state")?;
        let frame = FrameHeader::frame(LOG_ID, 1, 7, 1, 1, 2, &payload.into_vec()).context("fixture frame")?;
        let segment = SegmentHeader { log:LOG_ID, incarnation:1, nonce:7, segment_bytes:SEGMENT as u64, key:None }.encode();
        let persisted = Persist { log:LOG_ID, sequence:1, confirms:1, ..Default::default() }.encode().context("fixture persist")?;
        let mut bytes = vec![0; SEGMENT * 2];
        bytes.get_mut(BLOCK..BLOCK + persisted.len()).context("persist bounds")?.copy_from_slice(&persisted);
        bytes.get_mut(SEGMENT..SEGMENT + segment.len()).context("segment bounds")?.copy_from_slice(&segment);
        bytes.get_mut(SEGMENT + BLOCK..SEGMENT + BLOCK + frame.len()).context("frame bounds")?.copy_from_slice(&frame);
        std::fs::write(dir.join("cd.log"), bytes)?;
        Ok(())
    }

    #[test]
    fn u15_b2_cd_readonly_import_keeps_snapshot_and_committed_suffix_refuses_changed_source() -> anyhow::Result<()> {
        let dir = scratch()?;
        let source = dir.path().join("legacy");
        let mut snapshot = Data::default(); snapshot.settings.insert("snapshot".into(), json!("kept"));
        fixture(&source, &[Command::PutSetting("committed".into(), json!(2)), Command::PutSetting("uncommitted".into(), json!(3))], 1, snapshot)?;
        let before = identity(&source)?;
        let db = amphora::database::Database::open(&dir.path().join("host.db"), super::super::deadline())?;
        let namespace = db.namespace("cd-engine")?;
        import(&namespace, &source)?;
        let (_, manifest, data) = load(&namespace)?.context("import state")?;
        assert!(!manifest.importing);
        assert_eq!(manifest.cursor, 2);
        assert_eq!(data.settings.get("snapshot"), Some(&json!("kept")));
        assert_eq!(data.settings.get("committed"), Some(&json!(2)));
        assert!(!data.settings.contains_key("uncommitted"));
        assert!(super::super::control::page(&namespace)?.is_empty(), "legacy import must not fabricate historical control transitions");
        assert_eq!(identity(&source)?, before, "source was never repaired or rewritten");
        import(&namespace, &source)?;
        std::fs::write(source.join("cd.state"), b"changed source")?;
        assert!(import(&namespace, &source).is_err(), "a stale source cannot replace canonical data on reopen");
        assert_eq!(load(&namespace)?.context("canonical state")?.2.settings.get("committed"), Some(&json!(2)));
        Ok(())
    }

    #[test]
    fn u15_b2_cd_import_resumes_atomic_cursor_after_interruption_without_duplicating_history() -> anyhow::Result<()> {
        let dir = scratch()?; let source_dir = dir.path().join("legacy"); let path = dir.path().join("host.db");
        let mut snapshot = Data::default();
        snapshot.history.insert("app".into(), [Deployment { app:"app".into(), id:1, ..Default::default() }].into());
        let commands = (0..66).map(|n| Command::Deployed { deployment:Box::new(Deployment { app:"app".into(), op:n, ..Default::default() }), limit:100 }).collect::<Vec<_>>();
        fixture(&source_dir, &commands, commands.len(), snapshot)?;
        {
            let db = amphora::database::Database::open(&path, super::super::deadline())?; let namespace = db.namespace("cd-engine")?;
            let source = Source::open(&source_dir, None)?;
            assert!(import_source(&namespace, &source, || anyhow::bail!("injected interruption before final cutover")).is_err());
            let (_, state, data) = load(&namespace)?.context("partial import")?;
            assert!(state.importing); assert_eq!(state.cursor, 65);
            assert!(import_at(&namespace, &dir.path().join("other-replica"), Some(2), None).is_err(), "another source cannot resume a partial import");
            assert_eq!(data.history.get("app").map(|h| h.len()), Some(65));
            assert!(super::super::apply(&namespace, Command::PutSetting("must-wait".into(), json!(true))).is_err());
        }
        let db = amphora::database::Database::open(&path, super::super::deadline())?; let namespace = db.namespace("cd-engine")?;
        import(&namespace, &source_dir)?;
        let (_, state, data) = load(&namespace)?.context("completed import")?;
        assert!(!state.importing); assert_eq!(state.cursor, 67);
        assert_eq!(data.history.get("app").map(|h| (h.len(), h.back().map(|d| d.id))), Some((67, Some(67))));
        Ok(())
    }

    #[test]
    fn u15_b2_cd_import_refuses_committed_frame_damage_without_touching_source() -> anyhow::Result<()> {
        let dir = scratch()?; let source = dir.path().join("legacy");
        fixture(&source, &[Command::PutSetting("committed".into(), json!(1))], 1, Data::default())?;
        let path = source.join("cd.log"); let mut bytes = std::fs::read(&path)?;
        let byte = bytes.get_mut(SEGMENT + BLOCK + format::FRAME_HEADER_LEN).context("fixture payload")?; *byte ^= 1;
        std::fs::write(&path, &bytes)?;
        let db = amphora::database::Database::in_memory(super::super::deadline())?;
        assert!(import(&db.namespace("cd-engine")?, &source).is_err());
        assert_eq!(std::fs::read(&path)?, bytes, "refusal did not repair a legacy frame");
        Ok(())
    }

    #[test]
    fn u15_b2_cd_snapshot_term_mismatch_refuses_cutover_without_touching_either_store() -> anyhow::Result<()> {
        let dir = scratch()?; let source = dir.path().join("legacy");
        fixture(&source, &[Command::PutSetting("committed".into(), json!(1))], 1, Data::default())?;
        let path = source.join("cd.state");
        let mut image: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        image.as_object_mut().context("fixture image")?.insert("term".into(), json!(2));
        std::fs::write(&path, serde_json::to_vec(&image)?)?;
        let before = identity(&source)?;
        let db = amphora::database::Database::in_memory(super::super::deadline())?;
        let namespace = db.namespace("cd-engine")?;
        let error = import(&namespace, &source).err().context("mismatched snapshot was accepted")?;
        assert!(error.to_string().contains("snapshot term disagrees"));
        assert!(load(&namespace)?.is_none(), "invalid source must not publish even a partial canonical image");
        assert_eq!(identity(&source)?, before, "recovery leaves both original sources unchanged");
        Ok(())
    }

    #[test]
    fn u15_b2_cd_completed_receipts_allow_absent_paths_and_identical_mirrors_but_refuse_divergence() -> anyhow::Result<()> {
        let dir = scratch()?; let source = dir.path().join("node-1-legacy");
        fixture(&source, &[Command::PutSetting("imported".into(), json!(1))], 1, Data::default())?;
        let db = amphora::database::Database::in_memory(super::super::deadline())?;
        let namespace = db.namespace("cd-engine")?;
        import_at(&namespace, &source, Some(1), Some(1))?;
        super::super::apply(&namespace, Command::PutSetting("canonical".into(), json!(2)))?;
        let before = serde_json::to_value(load(&namespace)?.context("canonical state")?.2)?;
        let follower = dir.path().join("node-2-legacy");
        let other = dir.path().join("node-3-legacy");
        import_at(&namespace, &follower, Some(2), Some(1))?;
        import_at(&namespace, &other, Some(3), Some(1))?;
        // Identical mirrors are accepted by content receipt, independently of node/path.
        std::fs::create_dir_all(&follower)?;
        for file in ["cd.state", "cd.log"] { std::fs::copy(source.join(file), follower.join(file))?; }
        import_at(&namespace, &follower, Some(2), Some(1))?;
        std::fs::write(source.join("cd.state"), b"changed original source")?;
        assert!(import_at(&namespace, &source, Some(1), Some(1)).is_err(), "remaining source input must still match its receipt");
        // The canonical import remains authoritative after the original source is retired.
        std::fs::remove_file(source.join("cd.state"))?;
        std::fs::remove_file(source.join("cd.log"))?;
        import_at(&namespace, &source, Some(1), Some(1))?;
        import_at(&namespace, &follower, Some(2), Some(1))?;
        std::fs::write(follower.join("cd.state"), b"unrelated divergent legacy state")?;
        assert!(import_at(&namespace, &follower, Some(2), Some(1)).is_err(), "foreign legacy input cannot bypass receipt validation");
        import_at(&namespace, &other, Some(3), Some(1))?;
        assert_eq!(serde_json::to_value(load(&namespace)?.context("canonical state")?.2)?, before, "reopen validation never replaces canonical domain state");
        Ok(())
    }

    #[test]
    fn u15_b2_cd_declared_source_admission_prevents_empty_replica_from_omitting_legacy_data() -> anyhow::Result<()> {
        let dir = scratch()?; let source = dir.path().join("source"); let follower = dir.path().join("follower");
        fixture(&source, &[Command::PutSetting("legacy".into(), json!(1))], 1, Data::default())?;
        let db = amphora::database::Database::in_memory(super::super::deadline())?;
        let namespace = db.namespace("cd-engine")?;
        assert!(import_at(&namespace, &follower, Some(2), Some(1)).is_err(), "an empty follower cannot initialize ahead of the declared source");
        assert!(load(&namespace)?.is_none());
        assert!(import_at(&namespace, &source, Some(1), None).is_err(), "replicated legacy data requires an explicitly selected source");
        assert!(load(&namespace)?.is_none());
        import_at(&namespace, &source, Some(1), Some(1))?;
        import_at(&namespace, &follower, Some(2), Some(1))?;
        assert_eq!(load(&namespace)?.context("imported state")?.2.settings.get("legacy"), Some(&json!(1)));
        let fresh = db.namespace("cd-fresh")?;
        import_at(&fresh, &follower, Some(2), None)?;
        assert!(import_at(&fresh, &source, Some(1), None).is_err(), "an existing empty canonical store cannot silently hide a newly discovered legacy source");
        assert!(load(&fresh)?.context("fresh state")?.2.settings.is_empty());
        Ok(())
    }
}
