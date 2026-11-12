//! Source/destination ownership phase persistence at the existing Host. Native activation and
//! generic handover success remain behind the exact server Committed event.

use super::Host;
use crate::harness::inbox::transfer::{Phase, Role, State};
use crate::harness::mail::{TransferDown, TransferUp};
use std::io::Seek as _;
use std::sync::Arc;

mod native;
pub(crate) use native::NativeImport;

pub(crate) struct NativeDestination { pub machine: String, pub instance: String, pub mail: bool, pub cancel: crate::rt::Shutdown }

/// The concrete in-process move API uses the same source Records ownership decision as the
/// socket protocol uses the server decision. This capability never serializes or authorizes a
/// token-bound original; it cannot be recreated from an untrusted unbound Saved header.
#[derive(Clone)]
pub(super) struct NativeReceipt(Arc<crate::harness::inbox::Inbox>);
impl std::fmt::Debug for NativeReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("NativeReceipt") }
}
impl NativeReceipt {
    pub(super) fn new(inbox: Arc<crate::harness::inbox::Inbox>) -> Self { Self(inbox) }
    async fn decide(&self, manifest: &crate::harness::mail::TransferManifest, phase: Phase, destination: Option<&Arc<crate::harness::inbox::Inbox>>) -> Result<bool, String> {
        let mut state = self.0.transfer(&manifest.session).await.map_err(|e| e.to_string())?.ok_or_else(|| "the concrete source ownership record is unavailable".to_owned())?;
        if state.manifest != *manifest || state.role != Role::Source { return Err("the concrete source capability names a different native transfer".into()); }
        let destination = destination.map(|inbox| format!("{:p}", Arc::as_ptr(inbox)));
        if state.phase == phase { return Ok(phase != Phase::Committed || state.local_destination == destination); }
        if matches!(state.phase, Phase::Committed | Phase::Active | Phase::Aborted) { return Ok(false); }
        state.phase = phase;
        if phase == Phase::Committed { state.local_destination = destination; }
        self.0.save_transfer(&state).await.map_err(|e| e.to_string())?;
        Ok(true)
    }
}

pub(crate) enum Effect {
    Progress { recover: bool },
    Offered { manifest: crate::harness::mail::TransferManifest, policy: crate::harness::moving::stream::StoragePolicy, role: Role },
}

impl Host {
    async fn native_phase(&self, manifest: &crate::harness::mail::TransferManifest, event: TransferUp, offering: bool, cancel: Option<&crate::rt::Shutdown>) -> Result<State, String> {
        use futures::FutureExt as _;
        let inbox = self.inbox.as_ref().ok_or_else(|| "native handover records are unavailable".to_owned())?;
        let mut offering = offering;
        let mut event = event;
        if matches!(event, TransferUp::Abort { .. }) { self.native_aborting(manifest).await?; }
        let _ = crate::harness::handing::post_transfer(&event).await;
        loop {
            let changed = self.changed.listen();
            let (state, revision) = match inbox.transfer_observed(&manifest.session).await {
                Ok((Some(state), revision)) if state.manifest == *manifest => (state, revision),
                _ => { changed.await; continue; }
            };
            if matches!(state.phase, Phase::Committed | Phase::Aborted | Phase::Active) || (offering && state.phase == Phase::Offered) { return Ok(state); }
            let cancellable = offering;
            let cancellation = async move { match cancel.filter(|_| cancellable) { Some(cancel) => cancel.cancelled().await, None => futures::future::pending().await } };
            futures::select_biased! {
                () = cancellation.fuse() => {
                    self.native_aborting(manifest).await?;
                    offering = false;
                    event = TransferUp::Abort { manifest: manifest.clone() };
                    let _ = crate::harness::handing::post_transfer(&event).await;
                },
                result = inbox.transfer_changed(&manifest.session, revision).fuse() => if result.is_err() { changed.await; },
            }
        }
    }

    async fn native_aborting(&self, manifest: &crate::harness::mail::TransferManifest) -> Result<(), String> {
        let inbox = self.inbox.as_ref().ok_or_else(|| "native source ownership storage is unavailable".to_owned())?;
        let mut state = inbox.transfer(&manifest.session).await.map_err(|e| e.to_string())?.ok_or_else(|| "native source ownership metadata is unavailable".to_owned())?;
        if state.manifest != *manifest || state.role != Role::Source { return Err("native abort names a different source operation".into()); }
        if !matches!(state.phase, Phase::Committed | Phase::Active | Phase::Aborted | Phase::Aborting) {
            state.phase = Phase::Aborting;
            inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub(crate) async fn abort_native_records(&self, saved: &[super::Saved]) -> Result<bool, String> {
        for saved in saved {
            let Some(manifest) = saved.mail.as_ref() else { continue };
            if saved.mail_bound {
                let state = self.native_phase(manifest, TransferUp::Abort { manifest: manifest.clone() }, false, None).await?;
                if state.phase != Phase::Aborted { return Ok(false); }
            } else {
                let receipt = saved.local_mail.as_ref().ok_or_else(|| "native rollback is missing its concrete source capability".to_owned())?;
                if !receipt.decide(manifest, Phase::Aborted, None).await? { return Ok(false); }
            }
        }
        Ok(true)
    }

    pub(crate) async fn restore_native_source(&self, mut saved: super::Saved, handed: Option<Box<crate::harness::session::Handed>>) -> anyhow::Result<()> {
        if !self.abort_native_records(std::slice::from_ref(&saved)).await.map_err(anyhow::Error::msg)? { anyhow::bail!("the native destination committed custody; the previous owner cannot resume"); }
        if let (Some(inbox), Some(manifest)) = (&self.inbox, &saved.mail) {
            let state = inbox.transfer(&saved.id).await?.ok_or_else(|| anyhow::anyhow!("native rollback has no durable ownership operation"))?;
            if state.role != Role::Source || state.manifest != *manifest || state.phase != Phase::Aborted { anyhow::bail!("native rollback is not authoritatively aborted; the current owner remains fenced"); }
            inbox.take_back(&saved.id, manifest.source_epoch).await?;
        }
        let manifest = saved.mail.clone();
        saved.mail = None;
        saved.journal = None;
        saved.mail_bound = false;
        saved.local_mail = None;
        self.move_in_plain(saved, handed, None)?;
        self.save_checked()?;
        if let Some(manifest) = &manifest { self.clear_native_fence(manifest, Role::Source).map_err(anyhow::Error::msg)?; }
        Ok(())
    }

    pub(crate) async fn export_native_journal(&self, session: &Arc<crate::harness::session::Session>, handed: Option<&crate::harness::session::Handed>, destination: Option<&NativeDestination>) -> Result<Option<(crate::harness::mail::TransferManifest, Arc<std::fs::File>, bool)>, String> {
        let Some(inbox) = &self.inbox else { return Ok(None) };
        let epoch = session.epoch();
        let next_epoch = epoch.checked_add(1).ok_or_else(|| "native handover epoch exhausted".to_owned())?;
        inbox.fence_for(&session.id, epoch, true).await.map_err(|e| e.to_string())?;
        let attempt = crate::util::new_id();
        let mut bytes = crate::harness::inbox::transfer::Count::default();
        let manifest = inbox.export_pending(&session.id, epoch, next_epoch, &attempt, &mut bytes).await.map_err(|e| e.to_string())?;
        if manifest.count == 0 { return Ok(None); }
        let mut cursor = None;
        let mut tokens = false;
        while let Some((key, row)) = inbox.pending_next(&session.id, cursor).await.map_err(|e| e.to_string())? { tokens |= row.lease.token.is_some(); cursor = Some(key); }
        if tokens && destination.is_none() { return Err("authenticated server ownership is required for this native handover; the source is retained".into()); }
        if destination.is_some_and(|d| !d.mail) { return Err("the new runner cannot import this source journal; the source is retained".into()); }
        let mut state = State { manifest: manifest.clone(), role: Role::Source, phase: Phase::Prepared, target: destination.map_or_else(String::new, |d| d.machine.clone()), instance: destination.map(|d| d.instance.clone()), disk_floor: None, package: None, local_destination: None, native: None };
        inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
        let mut saved = super::record(session);
        saved.mail = Some(manifest.clone());
        saved.mail_bound = destination.is_some();
        if let Err(why) = self.persist_native_fence(&state) {
            state.phase = Phase::Aborted;
            inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
            return Err(why);
        }
        if destination.is_some_and(|destination| destination.cancel.is_cancelled()) {
            state.phase = Phase::Aborted;
            inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
            self.clear_native_fence(&manifest, Role::Source)?;
            return Err("native handover was cancelled before its offer was sent".into());
        }
        if destination.is_some() {
            state = self.native_phase(&manifest, TransferUp::Offer { manifest: manifest.clone(), target: state.target.clone(), instance: state.instance.clone() }, true, destination.map(|destination| &destination.cancel)).await?;
            if state.phase == Phase::Aborted { self.clear_native_fence(&manifest, Role::Source)?; return Err("native handover was authoritatively aborted".into()); }
        } else {
            state.phase = Phase::Offered;
            inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
        }
        let directory = self.file.as_ref().and_then(|path| path.parent()).unwrap_or(&self.files_dir).join("mail-transfers");
        let produced = async {
            self.persist_native_snapshot(&mut state, &saved, handed).await?;
            let floor = crate::harness::moving::stream::StoragePolicy::offered(state.disk_floor).floor().map_err(|error| error.to_string())?;
            let mut spool = crate::harness::moving::stream::Spool::create(&directory, super::PACKAGE_MAX, floor).map_err(|e| e.to_string())?;
            spool.begin(bytes.bytes()).map_err(|e| e.to_string())?;
            let exact = inbox.export_pending(&session.id, epoch, next_epoch, &attempt, &mut spool).await.map_err(|e| e.to_string())?;
            if exact != manifest { return Err("native source journal changed after its paused manifest was offered".into()); }
            spool.complete(true).map_err(|e| e.to_string())?;
            let file = Arc::new(spool.reader().map_err(|e| e.to_string())?);
            state.package = Some(spool.path().to_path_buf());
            inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
            spool.retain().map_err(|e| e.to_string())?;
            Ok::<_, String>(file)
        }.await;
        match produced {
            Ok(file) => Ok(Some((manifest, file, destination.is_some()))),
            Err(why) => {
                if destination.is_some() {
                    let outcome = self.native_phase(&manifest, TransferUp::Abort { manifest: manifest.clone() }, false, None).await?;
                    if outcome.phase != Phase::Aborted {
                        // A verified destination now owns the attempt. Returning an ordinary
                        // failure would let the caller resume a second native writer.
                        return futures::future::pending().await;
                    }
                } else { state.phase = Phase::Aborted; inbox.save_transfer(&state).await.map_err(|e| e.to_string())?; }
                self.clear_native_fence(&manifest, Role::Source)?;
                Err(why)
            }
        }
    }

    pub(crate) async fn prepare_native_import(&self, saved: super::Saved, handed: Option<Box<crate::harness::session::Handed>>) -> Result<NativeImport, String> {
        let Some(manifest) = saved.mail.clone() else { return Ok(NativeImport::plain(saved, handed)) };
        let guard = self.archive_lock(&saved.id).lock_owned().await;
        let native = handed.is_some();
        if !saved.mail_bound && saved.local_mail.is_none() { return Err("an unbound native journal has no concrete source receipt capability".into()); }
        let inbox = self.inbox.as_ref().ok_or_else(|| "native journal destination storage is unavailable".to_owned())?;
        let mut state = match inbox.transfer(&manifest.session).await.map_err(|e| e.to_string())? {
            Some(state) if state.manifest == manifest => state,
            _ if !saved.mail_bound => {
                let state = State { manifest: manifest.clone(), role: Role::Destination, phase: Phase::Prepared, target: String::new(), instance: None, disk_floor: None, package: None, local_destination: None, native: None };
                inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
                state
            }
            _ => return Err("native journal has no authenticated destination offer".into()),
        };
        if state.role != Role::Destination { return Err("native import reached the source ownership record".into()); }
        if state.phase == Phase::Active && self.get(&saved.id).is_some() { return Ok(NativeImport::plain(saved, handed)); }
        if state.phase == Phase::Prepared { state.phase = Phase::Offered; inbox.save_transfer(&state).await.map_err(|e| e.to_string())?; }
        if matches!(state.phase, Phase::Aborting | Phase::Aborted) { return Err("native ownership is aborting or aborted; this receiver cannot activate".into()); }
        let previous = state.native.is_some();
        self.persist_native_snapshot(&mut state, &saved, handed.as_deref()).await?;
        let (saved, handed, header_charge) = if previous {
            let (mut stored, charge) = self.read_native_snapshot(&state)?.ok_or_else(|| "native retry is waiting for physical header copy authority".to_owned())?;
            if stored.handed.as_ref().map(|native| (native.kind.as_str(), native.process.pid)) != handed.as_ref().map(|native| (native.kind.as_str(), native.process.pid)) { return Err("the native retry names a different retained process".into()); }
            stored.saved.journal = saved.journal;
            stored.saved.local_mail = saved.local_mail;
            (stored.saved, stored.handed, Some(charge))
        } else { (saved, handed, None) };
        let mut file = saved.journal.as_ref().ok_or_else(|| "native journal descriptor did not accompany its manifest".to_owned())?.try_clone().map_err(|e| e.to_string())?;
        file.rewind().map_err(|e| e.to_string())?;
        if file.metadata().map_err(|e| e.to_string())?.len() > super::PACKAGE_MAX { return Err("native journal exceeds the existing package bound".into()); }
        let input = crate::harness::admission::Input::new(manifest.destination_epoch, self.input_budgets.budget(), Arc::clone(inbox));
        input.controller.lock().restoring().map_err(|_| "native destination cannot enter restoring".to_owned())?;
        if matches!(state.phase, Phase::Offered | Phase::Imported) {
            for row in serde_json::Deserializer::from_reader(std::io::BufReader::new(file)).into_iter::<crate::harness::inbox::Pending>() {
                let row = row.map_err(|_| "native journal contains an unreadable original".to_owned())?;
                if row.current_command().is_none() && row.disposition.is_none() && !native { return Err("opaque native flight requires its retained native process".into()); }
                if !saved.mail_bound && row.lease.token.is_some() { return Err("a token-bound native original requires verified server ownership".into()); }
                if let Some(command @ crate::harness::model::Command::Send { attachments, .. }) = row.current_command()
                    && !attachments.is_empty()
                {
                    // This is a same-machine transfer. Materialization verifies/copies immutable
                    // actual source files; no body/path/attachment is removed from the journal.
                    let (copy, mut charge) = input.clone_command(command).map_err(|_| "native file verification byte accounting failed".to_owned())?.ok_or_else(|| "native file verification has no physical copy authority".to_owned())?;
                    let materialized = crate::harness::session::prepare(&self.files_for(&saved.id), copy).await.map_err(|_| "native handover files are not completely materialized".to_owned())?;
                    let bytes = crate::harness::admission::command_bytes(&materialized).map_err(|_| "native materialized body byte accounting failed".to_owned())?;
                    if !charge.try_resize(bytes).map_err(|_| "native materialized copy byte accounting failed".to_owned())? { return Err("native materialized copy exceeds physical authority".into()); }
                }
                inbox.import_pending(&manifest, &row).await.map_err(|e| e.to_string())?;
            }
            inbox.verify_import(&manifest).await.map_err(|e| e.to_string())?;
        }
        let mut cursor = None;
        while let Some((key, row)) = inbox.transfer_pending_next(&manifest, cursor).await.map_err(|e| e.to_string())? {
            if row.disposition.is_none() { let mut lease = row.lease; lease.epoch = manifest.destination_epoch; input.controller.lock().restore(lease).map_err(|_| "native accepted debt could not be restored".to_owned())?; }
            cursor = Some(key);
        }
        if state.phase != Phase::Committed && state.phase != Phase::Active {
            state.phase = Phase::Imported;
            inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
            if saved.mail_bound {
                state = self.native_phase(&manifest, TransferUp::Adopt { manifest: manifest.clone() }, false, None).await?;
                if state.phase != Phase::Committed { return Err("native ownership was not committed; the source retains its handles".into()); }
            } else {
                let receipt = saved.local_mail.as_ref().ok_or_else(|| "the concrete source receipt capability is unavailable".to_owned())?;
                if !receipt.decide(&manifest, Phase::Committed, Some(inbox)).await? { return Err("the concrete source handover was aborted or belongs to another destination; this destination cannot activate".into()); }
                state.phase = Phase::Committed;
                inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
            }
        }
        inbox.activate_transfer(&manifest).await.map_err(|e| e.to_string())?;
        // Lane::Open performs current-body/file reconciliation before clearing Restoring.
        Ok(NativeImport { saved, handed, input: Some(input), _guard: Some(guard), _charge: header_charge })
    }

    pub(crate) async fn move_in_prepared(&self, prepared: NativeImport) -> anyhow::Result<()> {
        let NativeImport { mut saved, mut handed, input, _guard, _charge } = prepared;
        let manifest = saved.mail.clone();
        if let Some(manifest) = &manifest {
            saved.epoch = manifest.destination_epoch;
            if let Some(handed) = &mut handed { handed.core.transfer_epoch(manifest.source_epoch, manifest.destination_epoch).map_err(anyhow::Error::msg)?; }
        }
        let id = saved.id.clone();
        self.move_in_plain(saved, handed, input)?;
        self.save_checked()?;
        if let (Some(inbox), Some(manifest)) = (&self.inbox, manifest) {
            let mut state = inbox.transfer(&id).await?.ok_or_else(|| anyhow::anyhow!("native handover recovery metadata disappeared"))?;
            if state.manifest != manifest { anyhow::bail!("native handover recovery changed during activation"); }
            state.phase = Phase::Active;
            inbox.save_transfer(&state).await?;
            self.clear_native_fence(&manifest, Role::Destination).map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
    /// Reach's existing handover arrives in an owned bounded file. The legacy header and v4
    /// journal share one reader; no whole incoming package or collection of journal bodies is
    /// allocated. A committed failure is uncertainty owned here, never a refusal authorizing
    /// the previous process to resume.
    pub(crate) async fn adopt_spool(&self, from: &str, spool: crate::harness::moving::stream::Spool) -> Result<(), String> {
        use crate::harness::{admission, inbox::transfer, moving::stream::Incoming};
        let file = spool.reader().map_err(|_| format!("{}the private handover file is unavailable", crate::reach::node::NOT_TAKEN))?;
        let mut incoming = Incoming::read(std::io::BufReader::new(file)).map_err(|why| format!("{}{why}", crate::reach::node::NOT_TAKEN))?;
        let Some(manifest) = incoming.package.mail.clone() else {
            incoming.next().map_err(|why| format!("{}{why}", crate::reach::node::NOT_TAKEN))?;
            return self.take_over_package(from, incoming.package, false).map_err(|error| match error {
                super::Refused::NotTaken(why) => format!("{}{why}", crate::reach::node::NOT_TAKEN),
                super::Refused::Elsewhere(why) => format!("{}{why}", super::TAKEN_ELSEWHERE),
                super::Refused::Unsure(why) => why,
            });
        };
        let inbox = self.inbox.as_ref().ok_or_else(|| "source journal import is unavailable; ownership stays pending".to_owned())?;
        let mut state = inbox.transfer(&manifest.session).await.map_err(|e| e.to_string())?.ok_or_else(|| "source journal import has no authenticated offer".to_owned())?;
        if state.manifest != manifest || state.role != Role::Destination || !matches!(state.phase, Phase::Offered | Phase::Imported | Phase::Committed | Phase::Active) {
            return Err("source journal import does not match the authenticated destination operation".into());
        }
        if state.phase == Phase::Active && self.get(&manifest.session).is_some_and(|s| s.epoch() >= manifest.destination_epoch) { return Ok(()); }
        let kind = crate::harness::kinds::Kind::get(&incoming.package.spec.kind).ok_or_else(|| "the offered native harness is unknown; the previous owner retains custody".to_owned())?;
        if !incoming.package.spec.program.as_deref().map_or_else(|| kind.available(), |program| crate::os::which(program).is_some()) {
            return Err("the offered native harness is unavailable on this destination; ownership stays pending".into());
        }
        if self.get(&manifest.session).is_some() { return Err("a different native session is already present on this destination; ownership stays pending".into()); }
        if let Some(previous) = &state.package
            && !crate::harness::moving::stream::same_package(previous, &spool).map_err(|_| "the immutable staged package cannot be verified; ownership stays pending".to_owned())?
        {
            return Err("the same handover attempt carried a different package; its original remains retained".into());
        }
        let input = admission::Input::new(manifest.destination_epoch, self.input_budgets.budget(), std::sync::Arc::clone(inbox));
        input.controller.lock().restoring().map_err(|_| "destination input could not enter restoring".to_owned())?;
        if matches!(state.phase, Phase::Offered | Phase::Imported) {
            if state.package.is_none() {
                state.package = Some(spool.path().to_path_buf());
                inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
                let _retained = spool.retain().map_err(|_| "the complete private handover package could not be synced".to_owned())?;
            }
            while let Some(pending) = incoming.next()? {
                // File proof/materialization is a prerequisite, never an excuse to accept only
                // the words. The source retains responsibility if legacy bytes are unavailable.
                if let Some(command) = pending.current_command() {
                    let attachments = match command { crate::harness::model::Command::Send { attachments, .. } => attachments.as_slice(), _ => &[] };
                    if !attachments.is_empty() { return Err("file-bearing handover requires complete verified destination file evidence; ownership stays pending".into()); }
                } else if pending.disposition.is_none() {
                    return Err("opaque native-flight handover requires its retained native process; ownership stays pending".into());
                }
                inbox.import_pending(&manifest, &pending).await.map_err(|e| e.to_string())?;
            }
            inbox.verify_import(&manifest).await.map_err(|e| e.to_string())?;
        }
        let mut after = None;
        while let Some((key, pending)) = inbox.transfer_pending_next(&manifest, after).await.map_err(|e| e.to_string())? {
            if pending.disposition.is_none() {
                let mut lease = pending.lease;
                lease.epoch = manifest.destination_epoch;
                input.controller.lock().restore(lease).map_err(|_| "destination accepted debt could not be restored".to_owned())?;
            }
            after = Some(key);
        }
        if state.phase != Phase::Committed && state.phase != Phase::Active {
            state.phase = transfer::Phase::Imported;
            inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
            match crate::harness::handing::transfer(&TransferUp::Adopt { manifest: manifest.clone() }).await? {
                TransferDown::Committed { manifest: actual } if actual == manifest => {},
                TransferDown::Aborted { .. } => return Err(format!("{}the source handover was authoritatively aborted", crate::reach::node::NOT_TAKEN)),
                _ => return Err("destination custody has not been verified committed; ownership remains pending".into()),
            }
        }
        inbox.activate_transfer(&manifest).await.map_err(|e| e.to_string())?;
        self.take_over_package(from, incoming.package, false).map_err(|_| "verified destination custody is retained, but native activation remains pending".to_owned())?;
        if let Some(session) = self.get(&manifest.session) { self.replay_session_mail(&session).await; }
        self.save_checked().map_err(|_| "verified destination custody is retained, but its active record is not durable yet".to_owned())?;
        state = inbox.transfer(&manifest.session).await.map_err(|e| e.to_string())?.ok_or_else(|| "committed handover recovery metadata disappeared".to_owned())?;
        state.phase = Phase::Active;
        inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Persist the authenticated server's authoritative phase before waking its existing
    /// handover waiter. An answer that was lost before waiter registration is recovered by
    /// replaying the durable operation on Welcome/feature negotiation.
    pub(crate) async fn mail_transfer_event(&self, answer: TransferDown) -> Result<Effect, String> {
        let inbox = self.inbox.as_ref().ok_or_else(|| "mail transfer records are unavailable; the original owner retains custody".to_owned())?;
        let mut effect = Effect::Progress { recover: false };
        match &answer {
            TransferDown::Offered { manifest, disk_floor } => {
                let mut state = match inbox.transfer(&manifest.session).await.map_err(|e| e.to_string())? {
                    Some(state) if state.manifest == *manifest => state,
                    Some(state) if !matches!(state.phase, Phase::Active | Phase::Aborted) => return Err("a different mail ownership operation is still pending".into()),
                    _ => {
                        let prepared = State { manifest: manifest.clone(), role: Role::Destination, phase: Phase::Prepared, target: String::new(), instance: None, disk_floor: *disk_floor, package: None, local_destination: None, native: None };
                        inbox.save_transfer(&prepared).await.map_err(|e| e.to_string())?;
                        prepared
                    }
                };
                // A delayed Offered can explain policy but cannot undo a committed activation.
                if matches!(state.phase, Phase::Prepared | Phase::Offered) {
                    state.phase = Phase::Offered;
                    state.disk_floor = *disk_floor;
                    inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
                }
                effect = Effect::Offered { manifest: manifest.clone(), policy: crate::harness::moving::stream::StoragePolicy::offered(*disk_floor), role: state.role };
            }
            TransferDown::Committed { manifest } | TransferDown::Aborted { manifest } => {
                let mut state = inbox.transfer(&manifest.session).await.map_err(|e| e.to_string())?.ok_or_else(|| "mail ownership answer has no durable local operation".to_owned())?;
                if state.manifest != *manifest { return Err("mail ownership answer names a different immutable manifest".into()); }
                let next = if matches!(answer, TransferDown::Committed { .. }) { Phase::Committed } else { Phase::Aborted };
                if !(state.phase == next || (state.phase == Phase::Active && next == Phase::Committed)) {
                    state.phase = next;
                    inbox.save_transfer(&state).await.map_err(|e| e.to_string())?;
                    effect = Effect::Progress { recover: true };
                }
            }
            // Refusal is not proof that an earlier offer/adopt could not have committed. The
            // exact phase remains durable and fenced until authoritative Abort/Commit.
            TransferDown::Refused { .. } => {},
        }
        crate::harness::handing::transfer_answer(answer);
        Ok(effect)
    }

    /// Actual connection/feature events replay metadata only, one row at a time. No timer,
    /// assembled body collection, or per-publication recovery task is introduced.
    pub(crate) async fn replay_mail_transfers(&self) -> bool {
        self.changed.notify();
        let Some(inbox) = &self.inbox else { return false };
        let mut resource_held = false;
        let mut after = None;
        loop {
            let next = inbox.transfer_next(after).await;
            let (key, state) = match next {
                Ok(Some(next)) => next,
                Ok(None) => return resource_held,
                Err(_) => { tracing::warn!("mail transfer recovery metadata is unavailable; ownership stays pending"); return resource_held; }
            };
            after = Some(key);
            let event = state.replay();
            if let Some(event) = event && crate::harness::handing::post_transfer(&event).await.is_err() { return resource_held; }
            match self.recover_native_snapshot(&state).await {
                Ok(native::Recovered::ResourceHeld) => resource_held = true,
                Ok(native::Recovered::Complete) => {},
                Err(why) => tracing::warn!(session = %state.manifest.session, %why, "native recovery remains fenced with its complete original retained"),
            }
        }
    }
}
