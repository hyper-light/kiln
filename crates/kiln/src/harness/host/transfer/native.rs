//! Recovery belongs to the existing per-session transfer owner. Files retain exact headers;
//! markers only fence synchronous startup and never authorize an owner or native descriptor.

use super::{Host, Phase, Role, State};
use crate::harness::{admission, inbox::transfer::NativeSnapshot, mail::TransferManifest, session};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::path::PathBuf;

pub(super) enum Recovered { Complete, ResourceHeld }

pub(crate) struct NativeImport {
    pub(crate) saved: crate::harness::host::Saved,
    pub(crate) handed: Option<Box<session::Handed>>,
    pub(super) input: Option<admission::Input>,
    pub(super) _guard: Option<futures::lock::OwnedMutexGuard<()>>,
    pub(super) _charge: Option<admission::BufferCharge>,
}
impl NativeImport {
    pub(super) fn plain(saved: crate::harness::host::Saved, handed: Option<Box<session::Handed>>) -> Self { Self { saved, handed, input: None, _guard: None, _charge: None } }
}

#[derive(Serialize)]
struct Header<'a> { manifest: &'a TransferManifest, saved: &'a crate::harness::host::Saved, handed: Option<&'a session::Handed> }
#[derive(Deserialize)]
pub(super) struct Stored { manifest: TransferManifest, pub saved: crate::harness::host::Saved, pub handed: Option<Box<session::Handed>> }
#[derive(Serialize, Deserialize)]
struct Fence { manifest: TransferManifest, role: Role }
#[derive(Serialize, Deserialize)]
struct LegacyFence { session: String, from: u64, to: u64 }

#[derive(Default)]
struct Encoding { count: crate::harness::inbox::transfer::Count, hash: sha2::Sha256 }
impl std::io::Write for Encoding {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.count.write_all(bytes)?; self.hash.update(bytes); Ok(bytes.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn verified(snapshot: &NativeSnapshot) -> Result<std::fs::File, String> {
    if snapshot.bytes > crate::harness::host::PACKAGE_MAX { return Err("native recovery header exceeds its existing package bound".into()); }
    let metadata = std::fs::symlink_metadata(&snapshot.path).map_err(|_| "native recovery header is unavailable".to_owned())?;
    if !metadata.is_file() || metadata.len() != snapshot.bytes { return Err("native recovery header size or file type changed".into()); }
    let mut file = std::fs::File::open(&snapshot.path).map_err(|_| "native recovery header cannot be opened".to_owned())?;
    let mut encoding = Encoding::default();
    std::io::copy(&mut file, &mut encoding).map_err(|_| "native recovery header cannot be hashed".to_owned())?;
    if encoding.count.bytes() != snapshot.bytes || hex::encode(encoding.hash.finalize()) != snapshot.sha256 { return Err("native recovery header hash changed".into()); }
    use std::io::Seek as _;
    file.rewind().map_err(|_| "native recovery header cannot be rewound".to_owned())?;
    Ok(file)
}

impl Host {
    pub(crate) fn native_budget_counts(&self) -> (usize, usize) { self.input_budgets.counts() }
    pub(crate) async fn native_budget_changed(&self, counts: (usize, usize)) { self.input_budgets.changed_since(counts).await; }

    fn native_directory(&self) -> PathBuf {
        match self.file.as_ref().and_then(|path| path.parent()) {
            Some(directory) => directory.join("mail-transfers"),
            None => {
                // A fileless Host has no cold-start identity. Keep its concrete Inbox capability
                // separate from another such Host; no pointer becomes wire ownership authority.
                let scope = self.inbox.as_ref().map(|inbox| format!("{:p}", std::sync::Arc::as_ptr(inbox))).unwrap_or_default();
                self.files_dir.join("mail-transfers").join(crate::util::sha256_hex(scope.as_bytes()))
            }
        }
    }

    fn native_fence_path(&self, session: &str) -> PathBuf { self.native_directory().join(format!("native-{}.fence", crate::util::sha256_hex(session.as_bytes()))) }

    fn legacy_fence_path(&self, session: &str) -> PathBuf { self.native_directory().join(format!("legacy-{}.fence", crate::util::sha256_hex(session.as_bytes()))) }

    fn read_legacy_fence(&self, path: &std::path::Path) -> Result<LegacyFence, String> {
        let metadata = std::fs::symlink_metadata(path).map_err(|_| "legacy promotion fence is unavailable".to_owned())?;
        if !metadata.is_file() || metadata.len() > crate::harness::state_view::FRAME as u64 { return Err("legacy promotion fence has an invalid type or size".into()); }
        let file = std::fs::File::open(path).map_err(|_| "legacy promotion fence cannot be opened".to_owned())?;
        let fence: LegacyFence = serde_json::from_reader(std::io::BufReader::new(file)).map_err(|_| "legacy promotion fence is unreadable".to_owned())?;
        if fence.from.checked_add(1) != Some(fence.to) || self.legacy_fence_path(&fence.session) != path { return Err("legacy promotion fence identity or epoch is invalid".into()); }
        Ok(fence)
    }

    pub(in crate::harness::host) fn legacy_fence_epochs(&self, session: &str) -> Result<Option<(u64, u64)>, String> {
        let path = self.legacy_fence_path(session);
        if !path.try_exists().map_err(|_| "legacy promotion fence cannot be inspected".to_owned())? { return Ok(None); }
        self.read_legacy_fence(&path).map(|fence| Some((fence.from, fence.to)))
    }

    pub(in crate::harness::host) fn persist_legacy_fence(&self, session: &str, from: u64, to: u64) -> Result<(), String> {
        if from.checked_add(1) != Some(to) { return Err("legacy promotion requires the next epoch".into()); }
        let path = self.legacy_fence_path(session);
        if path.try_exists().map_err(|_| "legacy promotion fence cannot be inspected".to_owned())? {
            let old = self.read_legacy_fence(&path)?;
            return if old.session == session && old.from == from && old.to == to { Ok(()) } else { Err("another legacy promotion still fences this session".into()) };
        }
        std::fs::create_dir_all(self.native_directory()).map_err(|_| "legacy promotion directory cannot be created".to_owned())?;
        let bytes = crate::harness::state_view::encode(&LegacyFence { session: session.to_owned(), from, to }, crate::harness::state_view::FRAME).map_err(|_| "legacy promotion fence exceeds its metadata frame".to_owned())?;
        crate::util::write_secret(&path, &bytes, 0o600).and_then(|()| path.parent().map_or(Ok(()), crate::blobs::chunks::sync_dir)).map_err(|_| "legacy promotion fence could not be synced".to_owned())
    }

    pub(in crate::harness::host) fn clear_legacy_fence(&self, session: &str, from: u64, to: u64) -> Result<(), String> {
        let path = self.legacy_fence_path(session);
        if !path.try_exists().map_err(|_| "legacy promotion fence cannot be inspected".to_owned())? { return Ok(()); }
        let fence = self.read_legacy_fence(&path)?;
        if fence.session != session || fence.from != from || fence.to != to { return Err("legacy promotion fence does not match its completed owner".into()); }
        std::fs::remove_file(&path).and_then(|()| path.parent().map_or(Ok(()), crate::blobs::chunks::sync_dir)).map_err(|_| "legacy promotion fence could not be retired".to_owned())
    }

    fn read_native_fence(&self, path: &std::path::Path) -> Result<Fence, String> {
        let file = std::fs::File::open(path).map_err(|_| "a native ownership fence cannot be opened".to_owned())?;
        if file.metadata().map_err(|_| "a native ownership fence cannot be inspected".to_owned())?.len() > crate::harness::state_view::FRAME as u64 { return Err("a native ownership fence exceeds its metadata frame".into()); }
        let fence: Fence = serde_json::from_reader(std::io::BufReader::new(file)).map_err(|_| "a native ownership fence is unreadable".to_owned())?;
        if self.native_fence_path(&fence.manifest.session) != path { return Err("a native ownership fence names a different session".into()); }
        Ok(fence)
    }

    pub(in crate::harness::host) fn native_restore_fences(&self) -> Result<std::collections::HashSet<String>, String> {
        let entries = match std::fs::read_dir(self.native_directory()) { Ok(entries) => entries, Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()), Err(_) => return Err("native ownership fence directory is unavailable".into()) };
        let mut ids = std::collections::HashSet::new();
        for entry in entries {
            let path = entry.map_err(|_| "native ownership fences cannot be enumerated".to_owned())?.path();
            if path.extension().is_some_and(|extension| extension == "fence") {
                if path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("legacy-")) { ids.insert(self.read_legacy_fence(&path)?.session); }
                else { ids.insert(self.read_native_fence(&path)?.manifest.session); }
            }
        }
        Ok(ids)
    }

    pub(super) fn clear_native_fence(&self, manifest: &TransferManifest, role: Role) -> Result<(), String> {
        let path = self.native_fence_path(&manifest.session);
        if !path.try_exists().map_err(|_| "the native ownership fence cannot be inspected".to_owned())? { return Ok(()); }
        let fence = self.read_native_fence(&path)?;
        if fence.manifest != *manifest || fence.role != role { return Err("a different native operation still fences this session".into()); }
        std::fs::remove_file(&path).and_then(|()| path.parent().map_or(Ok(()), crate::blobs::chunks::sync_dir)).map_err(|_| "the completed native ownership fence could not be retired".to_owned())
    }

    pub(super) async fn persist_native_snapshot(&self, state: &mut State, saved: &crate::harness::host::Saved, handed: Option<&session::Handed>) -> Result<(), String> {
        if !matches!(state.phase, Phase::Offered | Phase::Imported | Phase::Committed | Phase::Active) { return Err("the native snapshot storage policy has not been offered; only its fence may be persisted".into()); }
        let floor = crate::harness::moving::stream::StoragePolicy::offered(state.disk_floor).floor().map_err(|error| error.to_string())?;
        if let Some(snapshot) = &state.native { verified(snapshot)?; }
        else {
        let header = Header { manifest: &state.manifest, saved, handed };
        let mut encoded = Encoding::default();
        serde_json::to_writer(&mut encoded, &header).map_err(|_| "native recovery header cannot be encoded within the existing package bound".to_owned())?;
        let mut spool = crate::harness::moving::stream::Spool::create(&self.native_directory(), crate::harness::host::PACKAGE_MAX, floor).map_err(|_| "native recovery storage has no authority".to_owned())?;
        spool.begin(encoded.count.bytes()).map_err(|_| "native recovery storage has no room".to_owned())?;
        serde_json::to_writer(&mut spool, &header).map_err(|_| "native recovery header could not be streamed".to_owned())?;
        spool.complete(true).map_err(|_| "native recovery header could not be synced".to_owned())?;
        let snapshot = NativeSnapshot { path: spool.path().to_path_buf(), bytes: encoded.count.bytes(), sha256: hex::encode(encoded.hash.finalize()) };
        verified(&snapshot)?;
        let retained = spool.retain().map_err(|_| "native recovery header could not be retained".to_owned())?;
        state.native = Some(snapshot);
        let inbox = self.inbox.as_ref().ok_or_else(|| "native recovery Records are unavailable".to_owned())?;
        if let Err(error) = inbox.save_transfer(state).await { let _ = std::fs::remove_file(retained); return Err(error.to_string()); }
        }
        self.persist_native_fence(state)
    }

    pub(super) fn persist_native_fence(&self, state: &State) -> Result<(), String> {
        let fence = Fence { manifest: state.manifest.clone(), role: state.role };
        let bytes = crate::harness::state_view::encode(&fence, crate::harness::state_view::FRAME).map_err(|_| "native fence metadata exceeds its existing frame".to_owned())?;
        let path = self.native_fence_path(&state.manifest.session);
        crate::util::write_secret(&path, &bytes, 0o600).and_then(|()| path.parent().map_or(Ok(()), crate::blobs::chunks::sync_dir)).map_err(|_| "native ownership fence could not be synced before the ownership request".to_owned())
    }

    pub(super) fn read_native_snapshot(&self, state: &State) -> Result<Option<(Stored, admission::BufferCharge)>, String> {
        let snapshot = state.native.as_ref().ok_or_else(|| "native recovery metadata has no complete header".to_owned())?;
        let file = verified(snapshot)?;
        let bytes = usize::try_from(snapshot.bytes).map_err(|_| "native recovery header size exceeds addressable memory".to_owned())?;
        let Some(charge) = self.reserve_input_buffer(bytes).map_err(|_| "native recovery copy byte accounting failed".to_owned())? else { return Ok(None) };
        let stored: Stored = serde_json::from_reader(std::io::BufReader::new(file)).map_err(|_| "native recovery header is unreadable".to_owned())?;
        if stored.manifest != state.manifest || stored.saved.id != state.manifest.session || stored.saved.mail.as_ref() != Some(&state.manifest) { return Err("native recovery header does not match its exact ownership manifest".into()); }
        Ok(Some((stored, charge)))
    }

    pub(in crate::harness::host) fn native_active_record(&self, id: &str) -> Result<crate::harness::host::Saved, String> {
        struct Find<'a>(&'a str);
        impl<'de> serde::de::DeserializeSeed<'de> for Find<'_> {
            type Value = Option<crate::harness::host::Saved>;
            fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> { deserializer.deserialize_seq(self) }
        }
        impl<'de> serde::de::Visitor<'de> for Find<'_> {
            type Value = Option<crate::harness::host::Saved>;
            fn expecting(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { out.write_str("the ordinary saved sessions") }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
                let mut found = None;
                while let Some(saved) = sequence.next_element::<crate::harness::host::Saved>()? { if saved.id == self.0 { found = Some(saved); } }
                Ok(found)
            }
        }
        use serde::de::DeserializeSeed as _;
        let path = self.file.as_ref().ok_or_else(|| "the durable active native record is unavailable".to_owned())?;
        let file = std::fs::File::open(path).map_err(|_| "the durable active native record cannot be opened".to_owned())?;
        let mut decoder = serde_json::Deserializer::from_reader(std::io::BufReader::new(file));
        Find(id).deserialize(&mut decoder).map_err(|_| "the durable active native record is unreadable".to_owned())?.ok_or_else(|| "the native session was absent from its newer durable active record; it is not resurrected".to_owned())
    }

    pub(super) async fn recover_native_snapshot(&self, state: &State) -> Result<Recovered, String> {
        let allowed = matches!((state.role, state.phase), (Role::Destination, Phase::Committed | Phase::Active) | (Role::Source, Phase::Aborted));
        if !allowed { return Ok(Recovered::Complete); }
        if state.native.is_none() { return Err("the handover stopped before its full native header was retained; the fenced source journal requires manual recovery".into()); }
        let Some(_guard) = self.archive_lock(&state.manifest.session).try_lock_owned() else { return Ok(Recovered::Complete) };
        let inbox = self.inbox.as_ref().ok_or_else(|| "native recovery Inbox is unavailable".to_owned())?;
        if inbox.transfer(&state.manifest.session).await.map_err(|e| e.to_string())?.as_ref() != Some(state) { return Ok(Recovered::Complete); }
        let epoch = if state.role == Role::Destination { state.manifest.destination_epoch } else { state.manifest.source_epoch };
        if let Some(session) = self.get(&state.manifest.session) {
            if session.epoch() != epoch { return Err("a different live epoch already occupies the native recovery session".into()); }
            self.save_checked().map_err(|_| "the recovered native actor is still waiting for a durable active record".to_owned())?;
            if state.role == Role::Destination && state.phase == Phase::Committed { let mut active = state.clone(); active.phase = Phase::Active; inbox.save_transfer(&active).await.map_err(|e| e.to_string())?; }
            self.clear_native_fence(&state.manifest, state.role)?;
            return Ok(Recovered::Complete);
        }
        // Active implies that its ordinary record was durable. A leftover fence must never
        // resurrect an earlier precommit queue over that newer record; retire it and let the
        // ordinary restore path read its current snapshot.
        if state.phase == Phase::Active {
            let saved = self.native_active_record(&state.manifest.session)?;
            self.move_in_plain(saved, None, None).map_err(|_| "the current durable active native record could not be restored".to_owned())?;
            self.clear_native_fence(&state.manifest, state.role)?;
            return Ok(Recovered::Complete);
        }
        let Some((mut stored, _charge)) = self.read_native_snapshot(state)? else { return Ok(Recovered::ResourceHeld) };
        if state.role == Role::Destination { inbox.verify_import(&state.manifest).await.map_err(|e| e.to_string())?; inbox.activate_transfer(&state.manifest).await.map_err(|e| e.to_string())?; }
        else { inbox.take_back(&state.manifest.session, epoch).await.map_err(|e| e.to_string())?; }
        let input = admission::Input::new(epoch, self.input_budgets.budget(), std::sync::Arc::clone(inbox));
        input.controller.lock().restoring().map_err(|_| "native recovery cannot enter Restoring".to_owned())?;
        let mut after = None;
        loop {
            let next = if state.role == Role::Destination { inbox.transfer_pending_next(&state.manifest, after).await } else { inbox.pending_next(&state.manifest.session, after).await }.map_err(|e| e.to_string())?;
            let Some((key, row)) = next else { break };
            if row.disposition.is_none() { let mut lease = row.lease; lease.epoch = epoch; input.controller.lock().restore(lease).map_err(|_| "native recovery debt could not be restored".to_owned())?; }
            after = Some(key);
        }
        // Stored raw descriptor integers never cross a process restart. The complete frozen
        // native state remains in the immutable file; no provider receipt is invented here.
        let seed = stored.handed.take().map(|handed| handed.core.restart_after_transfer(state.manifest.source_epoch, epoch)).transpose()?;
        stored.saved.epoch = epoch;
        stored.saved.manual_resume = true;
        stored.saved.cut = false;
        stored.saved.spec.prompt = None;
        stored.saved.mail = None;
        stored.saved.mail_bound = false;
        let survived = crate::harness::survivors::alive(&stored.saved.harness.iter().cloned().collect::<Vec<_>>());
        crate::harness::survivors::stop(&survived.iter().map(|process| process.pid).collect::<Vec<_>>());
        self.open_record_with_input(stored.saved, seed, Vec::new(), u64::try_from(self.clock.unix_s()).unwrap_or(0), true, Some(input)).map_err(|_| "native recovery is retained but its actor could not start".to_owned())?;
        if let Some(session) = self.get(&state.manifest.session) {
            let mut core = session.core.lock();
            core.upsert(&format!("native-recovery:{}", state.manifest.attempt), None, crate::harness::model::Body::Notice { level: crate::harness::model::Level::Warn, text: "The runner restarted after native handover. The complete original and native snapshot are retained. The interrupted native result is uncertain; review it before resuming.".into() });
        }
        self.save_checked().map_err(|_| "native recovery actor exists but its active record is not durable".to_owned())?;
        if state.role == Role::Destination { let mut active = state.clone(); active.phase = Phase::Active; inbox.save_transfer(&active).await.map_err(|e| e.to_string())?; }
        self.clear_native_fence(&state.manifest, state.role)?;
        Ok(Recovered::Complete)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use super::*;
    use std::sync::Arc;

    #[test]
    fn w20_r09_durable_native_header_fences_restore_before_activation_and_rejects_changed_bytes() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let directory = tempfile::tempdir()?;
            let file = directory.path().join("harness.json");
            let (sink, _heard) = async_channel::unbounded();
            let host = Host::with_records(sink, Some(file.clone()), crate::harness::host::Limits::default(), Arc::new(kiln_seams::clock::SimClock::new()), Some(Arc::new(kiln_seams::records::SimRecords::new())));
            host.set_input_budget(1 << 20);
            let manifest = TransferManifest { attempt: "native-attempt".into(), session: "s".into(), source_epoch: 2, destination_epoch: 3, count: 0, sha256: "00".repeat(32) };
            let mut saved: crate::harness::host::Saved = serde_json::from_value(serde_json::json!({"id":"s","epoch":2,"spec":{"kind":"claude","program":directory.path().join("absent-provider")}}))?;
            saved.mail = Some(manifest.clone());
            let mut state = State { manifest: manifest.clone(), role: Role::Source, phase: Phase::Prepared, target: "machine".into(), instance: Some("green".into()), disk_floor: None, package: None, local_destination: None, native: None };
            let inbox = host.inbox().ok_or_else(|| anyhow::anyhow!("no test inbox"))?;
            inbox.save_transfer(&state).await?;
            host.persist_native_fence(&state).map_err(anyhow::Error::msg)?;
            assert!(state.native.is_none());
            assert!(host.persist_native_snapshot(&mut state, &saved, None).await.is_err(), "unknown policy cannot be treated as an offered Off policy");
            state.phase = Phase::Offered;
            inbox.save_transfer(&state).await?;
            host.persist_native_snapshot(&mut state, &saved, None).await.map_err(anyhow::Error::msg)?;
            assert_eq!(inbox.transfer("s").await?.map(|state| state.phase), Some(Phase::Offered), "saving a snapshot cannot authorize committed ownership");
            let (loaded, charge) = host.read_native_snapshot(&state).map_err(anyhow::Error::msg)?.ok_or_else(|| anyhow::anyhow!("the fixture header had no copy authority"))?;
            assert_eq!(loaded.saved.id, "s");
            assert_eq!(loaded.saved.mail, Some(manifest));
            drop(charge);
            crate::util::write_secret(&file, &serde_json::to_vec(&vec![saved])?, 0o600)?;
            host.restore();
            assert!(host.get("s").is_none(), "a fenced record cannot start an actor before owner reconciliation");
            assert!(host.native_restore_fences().map_err(anyhow::Error::msg)?.contains("s"));
            let snapshot = state.native.as_ref().ok_or_else(|| anyhow::anyhow!("no durable header"))?;
            let mut changed = std::fs::OpenOptions::new().write(true).open(&snapshot.path)?;
            changed.write_all(b"!")?;
            changed.sync_all()?;
            assert!(host.read_native_snapshot(&state).is_err(), "a header with changed bytes cannot authorize recovery");
            assert!(host.native_restore_fences().map_err(anyhow::Error::msg)?.contains("s"), "corruption must retain the ownership fence");
            Ok(())
        })
    }
}
