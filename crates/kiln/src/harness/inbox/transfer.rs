//! The existing inbox journal's handover representation. No body outbox lives here: an export
//! reads one committed row at a time; the caller retains its original journal until verified
//! destination custody. The writer may count/hash into a sink before obtaining storage authority.

use std::io::{self, Write};

use kiln_seams::records::{Cond, Consistency, Scan, Tx, Write as RecordWrite};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::{Inbox, Pending, Refusal, check_session, message_key, session_prefix, unavailable};
use crate::harness::mail::TransferManifest;

const TRANSFERS: &str = "inbox-transfers";
const TRANSFER_ROWS: &str = "inbox-transfer-pending";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role { Source, Destination }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase { Prepared, Offered, Imported, Aborting, Committed, Active, Aborted }

impl Phase {
    fn permits(self, next: Self) -> bool {
        self == next || matches!((self, next),
            (Self::Prepared, Self::Offered | Self::Aborting | Self::Aborted)
            | (Self::Offered, Self::Imported | Self::Aborting | Self::Committed | Self::Aborted)
            | (Self::Imported, Self::Aborting | Self::Committed | Self::Aborted)
            | (Self::Aborting, Self::Committed | Self::Aborted)
            | (Self::Committed, Self::Active))
    }
    fn terminal(self) -> bool { matches!(self, Self::Active | Self::Aborted) }
}

/// Durable recovery metadata at the existing source-journal owner. Bodies stay in their original
/// family or the exact destination staging family. Welcome replays this phase, not an invented
/// epoch, clock-based probe, or in-memory callback remembered after restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeSnapshot {
    pub path: std::path::PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub manifest: TransferManifest,
    pub role: Role,
    pub phase: Phase,
    pub target: String,
    pub instance: Option<String>,
    /// None is the server's explicitly disabled configured floor, not an unknown stat result.
    pub disk_floor: Option<u64>,
    /// A complete private transfer package, synced before Adopt. Kept through uncertain/committed
    /// restart until the active session record is durable; never copied into wire metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_destination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeSnapshot>,
}

impl State {
    pub(crate) fn replay(&self) -> Option<crate::harness::mail::TransferUp> {
        use crate::harness::mail::TransferUp;
        match (self.role, self.phase) {
            (Role::Source, Phase::Aborting) => Some(TransferUp::Abort { manifest: self.manifest.clone() }),
            (Role::Source, Phase::Prepared | Phase::Offered | Phase::Committed) => Some(TransferUp::Offer { manifest: self.manifest.clone(), target: self.target.clone(), instance: self.instance.clone() }),
            (Role::Destination, Phase::Imported | Phase::Committed) => Some(TransferUp::Adopt { manifest: self.manifest.clone() }),
            _ => None,
        }
    }
}

fn prefix(manifest: &TransferManifest) -> io::Result<Vec<u8>> {
    let mut key = Vec::new();
    for value in [&manifest.session, &manifest.attempt] {
        key.extend_from_slice(&u64::try_from(value.len()).map_err(io::Error::other)?.to_le_bytes());
        key.extend_from_slice(value.as_bytes());
    }
    Ok(key)
}

fn validate(manifest: &TransferManifest) -> Result<(), Refusal> {
    check_session(&manifest.session)?;
    super::check_message(&manifest.attempt)?;
    if manifest.source_epoch.checked_add(1) != Some(manifest.destination_epoch)
        || manifest.sha256.len() != sha2::Sha256::output_size().saturating_mul(2)
        || !manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(unavailable(&manifest.session, "mail transfer manifest is invalid"));
    }
    Ok(())
}

/// Exact length-framed row hashing is independent of the spool's record delimiter. The
/// domain/header bind the immutable attempt, recipient and both epochs; the final count binds
/// completion. A partial file can never prove a complete manifest.
struct Digest {
    hash: sha2::Sha256,
    bytes: u64,
    count: u64,
}

impl Digest {
    fn new(attempt: &str, session: &str, source_epoch: u64, destination_epoch: u64) -> io::Result<Self> {
        let mut hash = sha2::Sha256::new();
        hash.update(b"kiln-mail-transfer-v1\0");
        for value in [attempt, session] {
            hash.update(u64::try_from(value.len()).map_err(io::Error::other)?.to_le_bytes());
            hash.update(value.as_bytes());
        }
        hash.update(source_epoch.to_le_bytes());
        hash.update(destination_epoch.to_le_bytes());
        Ok(Self { hash, bytes: 0, count: 0 })
    }

    fn row<W: Write>(&mut self, row: &Pending, out: &mut W) -> io::Result<()> {
        let mut count = Count(0);
        serde_json::to_writer(&mut count, row).map_err(io::Error::other)?;
        let after = self.bytes.checked_add(count.0).and_then(|n| n.checked_add(1)).filter(|n| *n <= crate::harness::host::PACKAGE_MAX).ok_or_else(|| io::Error::other("mail transfer exceeds the package byte bound"))?;
        self.hash.update(count.0.to_le_bytes());
        let mut write = Hashing { out, hash: &mut self.hash };
        serde_json::to_writer(&mut write, row).map_err(io::Error::other)?;
        write.out.write_all(b"\n")?;
        self.bytes = after;
        self.count = self.count.checked_add(1).ok_or_else(|| io::Error::other("mail transfer row count overflow"))?;
        Ok(())
    }

    fn finish(mut self) -> (u64, String) {
        self.hash.update(self.count.to_le_bytes());
        (self.count, hex::encode(self.hash.finalize()))
    }
}

#[derive(Default)]
pub struct Count(u64);
impl Count { pub fn bytes(&self) -> u64 { self.0 } }
impl Write for Count {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_add(u64::try_from(data.len()).map_err(io::Error::other)?).filter(|n| *n <= crate::harness::host::PACKAGE_MAX).ok_or_else(|| io::Error::other("mail transfer row exceeds the package byte bound"))?;
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

struct Hashing<'a, W> { out: &'a mut W, hash: &'a mut sha2::Sha256 }
impl<W: Write> Write for Hashing<'_, W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.out.write(data)?;
        let written = data.get(..n).ok_or_else(|| io::Error::other("mail transfer writer returned an invalid length"))?;
        self.hash.update(written);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> { self.out.flush() }
}

impl Inbox {
    pub async fn transfer_observed(&self, session: &str) -> Result<(Option<State>, u64), Refusal> {
        let revision = self.records.head().await.map_err(|e| unavailable(session, &e.to_string()))?;
        Ok((self.transfer(session).await?, revision))
    }

    pub async fn transfer_changed(&self, session: &str, revision: u64) -> Result<(), Refusal> {
        self.records.wait(TRANSFERS, revision).await.map_err(|e| unavailable(session, &e.to_string()))?;
        Ok(())
    }

    pub async fn transfer_next(&self, after: Option<Vec<u8>>) -> Result<Option<(Vec<u8>, State)>, Refusal> {
        let page = self.records.scan(TRANSFERS, Scan { prefix: Vec::new(), after, limit: 1, at: Consistency::Linearizable }).await.map_err(|_| unavailable("", "mail transfer recovery metadata could not be scanned"))?;
        page.rows.into_iter().next().map(|row| serde_json::from_slice(&row.value).map(|state| (row.key, state)).map_err(|_| unavailable("", "mail transfer recovery metadata is unreadable"))).transpose()
    }

    pub async fn transfer(&self, session: &str) -> Result<Option<State>, Refusal> {
        check_session(session)?;
        self.records.get(TRANSFERS, session.as_bytes(), Consistency::Linearizable).await
            .map_err(|e| unavailable(session, &e.to_string()))?
            .map(|row| serde_json::from_slice(&row.value).map_err(|_| unavailable(session, "mail transfer recovery metadata is unreadable")))
            .transpose()
    }

    /// One session has one unsettled ownership operation. A different attempt cannot replace
    /// uncertainty. Transitions are conditional on the exact record; conflict is retained work.
    pub async fn save_transfer(&self, state: &State) -> Result<(), Refusal> {
        let session = &state.manifest.session;
        validate(&state.manifest)?;
        let key = session.as_bytes().to_vec();
        let old = self.records.get(TRANSFERS, &key, Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))?;
        let condition = match old {
            Some(row) => {
                let before: State = serde_json::from_slice(&row.value).map_err(|_| unavailable(session, "mail transfer recovery metadata is unreadable"))?;
                if before.manifest == state.manifest && before.role == state.role {
                    if before.target != state.target || before.instance != state.instance || !before.phase.permits(state.phase) {
                        return Err(unavailable(session, "mail transfer identity or phase cannot change"));
                    }
                    if before.local_destination != state.local_destination
                        && !(before.local_destination.is_none() && state.local_destination.is_some() && state.role == Role::Source && before.phase == Phase::Offered && state.phase == Phase::Committed)
                    {
                        return Err(unavailable(session, "native receipt destination cannot change"));
                    }
                    if before.native.is_some() && before.native != state.native {
                        return Err(unavailable(session, "the immutable native recovery snapshot cannot change"));
                    }
                    if before == *state { return Ok(()); }
                } else if !before.phase.terminal() || state.phase != Phase::Prepared {
                    return Err(unavailable(session, "another mail transfer still owns this session"));
                }
                Cond::At { family: TRANSFERS.into(), key: key.clone(), rev: row.rev }
            }
            None if state.phase == Phase::Prepared => Cond::Absent { family: TRANSFERS.into(), key: key.clone() },
            None => return Err(unavailable(session, "mail transfer was not durably prepared")),
        };
        self.records.commit(Tx { conds: vec![condition], writes: vec![RecordWrite::Put { family: TRANSFERS.into(), key, value: super::json(session, state)? }] }).await.map_err(|e| unavailable(session, &e.to_string()))?;
        Ok(())
    }

    /// Stage one original, preserving all source receipt/edit/native handoff fields. No active
    /// PENDING key is changed before the server's exact Committed transition. Retrying an
    /// interrupted import is idempotent only for the same manifest and exact source record.
    pub async fn import_pending(&self, manifest: &TransferManifest, pending: &Pending) -> Result<(), Refusal> {
        validate(manifest)?;
        let session = &manifest.session;
        let ownership = self.records.get(TRANSFERS, session.as_bytes(), Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))?.ok_or_else(|| unavailable(session, "mail import has no durable ownership operation"))?;
        let state: State = serde_json::from_slice(&ownership.value).map_err(|_| unavailable(session, "mail import ownership metadata is unreadable"))?;
        if state.manifest != *manifest || state.role != Role::Destination || !matches!(state.phase, Phase::Offered | Phase::Imported) {
            return Err(unavailable(session, "mail import does not match the offered destination"));
        }
        let original = match &pending.command {
            Some(command) => crate::harness::host::message_of(command).is_some_and(|identity| identity.0 == pending.lease.message && (identity.1 == pending.digest || (pending.origin == super::Origin::LegacyRetained && pending.lease.token.is_none() && pending.digest.is_empty()))),
            None => pending.origin == super::Origin::LegacyOpaque && pending.current.is_none() && pending.lease.token.is_none() && pending.lease.stage == crate::harness::admission::Stage::ProviderFlight,
        };
        if !original || pending.lease.epoch != manifest.source_epoch || pending.accepted_seq.is_none() {
            return Err(unavailable(session, "mail import original identity or source receipt is invalid"));
        }
        let mut key = prefix(manifest).map_err(|e| unavailable(session, &e.to_string()))?;
        key.extend_from_slice(pending.lease.message.as_bytes());
        let value = super::json(session, pending)?;
        if let Some(row) = self.records.get(TRANSFER_ROWS, &key, Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))? {
            return if row.value == value { Ok(()) } else { Err(unavailable(session, "mail import attempt reused a message for different source state")) };
        }
        self.records.commit(Tx { conds: vec![Cond::At { family: TRANSFERS.into(), key: session.as_bytes().to_vec(), rev: ownership.rev }, Cond::Absent { family: TRANSFER_ROWS.into(), key: key.clone() }], writes: vec![RecordWrite::Put { family: TRANSFER_ROWS.into(), key, value }] }).await.map_err(|e| unavailable(session, &e.to_string()))?;
        Ok(())
    }

    pub async fn transfer_pending_next(&self, manifest: &TransferManifest, after: Option<Vec<u8>>) -> Result<Option<(Vec<u8>, Pending)>, Refusal> {
        validate(manifest)?;
        let session = &manifest.session;
        let prefix = prefix(manifest).map_err(|e| unavailable(session, &e.to_string()))?;
        let page = self.records.scan(TRANSFER_ROWS, Scan { prefix, after, limit: 1, at: Consistency::Linearizable }).await.map_err(|e| unavailable(session, &e.to_string()))?;
        page.rows.into_iter().next().map(|row| {
            serde_json::from_slice(&row.value).map(|pending| (row.key, pending)).map_err(|_| unavailable(session, "mail import staged original is unreadable"))
        }).transpose()
    }

    /// Verify completion from persisted rows before granting the destination any execution.
    pub async fn verify_import(&self, manifest: &TransferManifest) -> Result<(), Refusal> {
        validate(manifest)?;
        let session = &manifest.session;
        let mut digest = Digest::new(&manifest.attempt, session, manifest.source_epoch, manifest.destination_epoch).map_err(|e| unavailable(session, &e.to_string()))?;
        let mut after = None;
        while let Some((key, pending)) = self.transfer_pending_next(manifest, after).await? {
            digest.row(&pending, &mut io::sink()).map_err(|e| unavailable(session, &e.to_string()))?;
            after = Some(key);
        }
        let (count, sha256) = digest.finish();
        if count != manifest.count || sha256 != manifest.sha256 { return Err(unavailable(session, "mail import is incomplete or does not match its source manifest")); }
        Ok(())
    }

    /// Install exactly a committed manifest. Terminal rows keep their disposition; live rows
    /// retain the same token and native-flight stage at the verified new epoch. The caller
    /// still owns its Restoring/debt gate and starts no actor until this complete scan succeeds.
    pub async fn activate_transfer(&self, manifest: &TransferManifest) -> Result<(), Refusal> {
        let session = &manifest.session;
        let ownership = self.records.get(TRANSFERS, session.as_bytes(), Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))?.ok_or_else(|| unavailable(session, "mail transfer has no committed ownership operation"))?;
        let state: State = serde_json::from_slice(&ownership.value).map_err(|_| unavailable(session, "mail transfer ownership metadata is unreadable"))?;
        if state.manifest != *manifest || state.role != Role::Destination || state.phase != Phase::Committed {
            return Err(unavailable(session, "mail transfer is not verified committed"));
        }
        self.verify_import(manifest).await?;
        let mut after = None;
        while let Some((cursor, mut pending)) = self.transfer_pending_next(manifest, after).await? {
            pending.lease.epoch = manifest.destination_epoch;
            let key = message_key(session, &pending.lease.message);
            let value = super::json(session, &pending)?;
            let old = self.records.get(super::PENDING, &key, Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))?;
            match old {
                Some(row) if row.value != value => return Err(unavailable(session, "mail destination already has a different original or receipt")),
                Some(_) => {},
                None => {
                    self.records.commit(Tx { conds: vec![Cond::At { family: TRANSFERS.into(), key: session.as_bytes().to_vec(), rev: ownership.rev }, Cond::Absent { family: super::PENDING.into(), key: key.clone() }], writes: vec![RecordWrite::Put { family: super::PENDING.into(), key, value }] }).await.map_err(|e| unavailable(session, &e.to_string()))?;
                }
            }
            after = Some(cursor);
        }
        Ok(())
    }

    /// Export the exact journal under the source's ownership fence, one row at a time. The
    /// source caller must quiesce native effects too; the inbox head alone cannot stop a native
    /// consumption callback. A second pass must match the first manifest before it is sent.
    pub async fn export_pending<W: Write>(&self, session: &str, source_epoch: u64, destination_epoch: u64, attempt: &str, out: &mut W) -> Result<TransferManifest, Refusal> {
        check_session(session)?;
        super::check_message(attempt)?;
        if source_epoch.checked_add(1) != Some(destination_epoch) {
            return Err(unavailable(session, "mail transfer must name the next epoch"));
        }
        let head = self.head(session).await?;
        if head.head.epoch != source_epoch || !head.head.moved {
            return Err(unavailable(session, "mail transfer source inbox is not fenced at its exact epoch"));
        }
        let mut digest = Digest::new(attempt, session, source_epoch, destination_epoch).map_err(|e| unavailable(session, &e.to_string()))?;
        let mut after = None;
        loop {
            let page = self.records.scan(super::PENDING, Scan { prefix: session_prefix(session), after, limit: 1, at: Consistency::Linearizable }).await.map_err(|e| unavailable(session, &e.to_string()))?;
            let Some(row) = page.rows.into_iter().next() else { break };
            let mut pending: Pending = serde_json::from_slice(&row.value).map_err(|_| unavailable(session, "a transfer's original journal row is unreadable"))?;
            if row.key != message_key(session, &pending.lease.message) || pending.lease.epoch != source_epoch {
                return Err(unavailable(session, "a transfer's journal row does not match its recipient/epoch"));
            }
            pending.accepted_seq = Some(pending.accepted_seq.unwrap_or(row.rev));
            digest.row(&pending, out).map_err(|e| unavailable(session, &e.to_string()))?;
            after = Some(row.key);
        }
        out.flush().map_err(|e| unavailable(session, &e.to_string()))?;
        let (count, sha256) = digest.finish();
        Ok(TransferManifest { attempt: attempt.to_owned(), session: session.to_owned(), source_epoch, destination_epoch, count, sha256 })
    }
}

/// Count exact encoded bytes without allocating a package-sized buffer. Real package and
/// blue/green encoders reserve physical authority for this count before materializing it.
pub fn encoded_len(value: &impl Serialize) -> io::Result<u64> {
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).map_err(io::Error::other)?;
    Ok(count.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::admission::{Lease, Stage};
    use crate::harness::inbox::{Limits, digest};
    use crate::harness::mail::Token;
    use crate::harness::model::Command;
    use std::sync::Arc;

    #[test]
    fn w20_r09_transfer_manifest_binds_original_edit_terminal_token_and_epochs() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let inbox = Inbox::new(Arc::new(kiln_seams::records::SimRecords::new()), Limits::default());
            inbox.open("s", 2).await?;
            let command = Command::Send { id: "m".into(), text: "accepted original".into(), mode: Default::default(), attachments: Vec::new(), overrides: None, reply_to: None, task: None };
            let pending = Pending::new(Lease { message: "m".into(), peer: "sender".into(), epoch: 2, bytes: 10, stage: Stage::InboxAccepted, token: Some(Token { revision: 8, generation: 3 }), handoff: None }, command.clone());
            inbox.accept_pending("s", "m", &digest("accepted original", &[]), Some(2), 10, &pending).await?;
            inbox.fence("s", 2).await?;
            let mut bytes = Vec::new();
            let first = inbox.export_pending("s", 2, 3, "a", &mut bytes).await?;
            assert_eq!(first.count, 1);
            let read: Pending = serde_json::from_slice(&bytes)?;
            assert_eq!(read.lease.token, pending.lease.token);
            assert_eq!(serde_json::to_value(&read.command)?, serde_json::to_value(command)?);
            assert!(read.accepted_seq.is_some());
            assert_eq!(first, inbox.export_pending("s", 2, 3, "a", &mut io::sink()).await?);
            assert_ne!(first.sha256, inbox.export_pending("s", 2, 3, "b", &mut io::sink()).await?.sha256);
            let mut changed = read.current_command().cloned().ok_or_else(|| anyhow::anyhow!("original command is absent"))?;
            if let Command::Send { text, .. } = &mut changed { *text = "explicit edit".into(); }
            inbox.edit_pending("s", "m", read.lease, changed).await?;
            assert_ne!(first.sha256, inbox.export_pending("s", 2, 3, "a", &mut io::sink()).await?.sha256);
            Ok(())
        })
    }

    #[test]
    fn w20_r09_native_abort_intent_survives_reload_and_cannot_replay_as_offer() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let inbox = Inbox::new(std::sync::Arc::new(kiln_seams::records::SimRecords::new()), Limits::default());
            let manifest = TransferManifest { attempt: "abort-attempt".into(), session: "s".into(), source_epoch: 2, destination_epoch: 3, count: 0, sha256: "00".repeat(32) };
            let mut state = State { manifest, role: Role::Source, phase: Phase::Prepared, target: "same-machine".into(), instance: Some("green".into()), disk_floor: None, package: None, local_destination: None, native: None };
            inbox.save_transfer(&state).await?;
            state.phase = Phase::Offered;
            inbox.save_transfer(&state).await?;
            state.phase = Phase::Aborting;
            inbox.save_transfer(&state).await?;
            let loaded = inbox.transfer("s").await?.ok_or_else(|| anyhow::anyhow!("no durable abort intent"))?;
            assert!(matches!(loaded.replay(), Some(crate::harness::mail::TransferUp::Abort { manifest }) if manifest == state.manifest));
            assert!(inbox.save_transfer(&State { phase: Phase::Offered, ..loaded.clone() }).await.is_err());
            state.phase = Phase::Committed;
            inbox.save_transfer(&state).await?;
            assert!(inbox.save_transfer(&State { phase: Phase::Aborted, ..state }).await.is_err(), "a late abort cannot authorize a second owner");
            Ok(())
        })
    }

    #[test]
    fn w20_r09_partial_transfer_never_enters_active_pending_and_commit_keeps_original_token() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let source = Inbox::new(Arc::new(kiln_seams::records::SimRecords::new()), Limits::default());
            source.open("s", 2).await?;
            for message in ["a", "b"] {
                let command = Command::Send { id: message.into(), text: message.into(), mode: Default::default(), attachments: Vec::new(), overrides: None, reply_to: None, task: None };
                let pending = Pending::new(Lease { message: message.into(), peer: "sender".into(), epoch: 2, bytes: 10, stage: Stage::ProviderFlight, token: Some(Token { revision: if message == "a" { 8 } else { 9 }, generation: 3 }), handoff: None }, command);
                source.accept_pending("s", message, &digest(message, &[]), Some(2), 10, &pending).await?;
            }
            source.fence("s", 2).await?;
            let mut exported = Vec::new();
            let manifest = source.export_pending("s", 2, 3, "attempt", &mut exported).await?;
            let rows = serde_json::Deserializer::from_slice(&exported).into_iter::<Pending>().collect::<Result<Vec<_>, _>>()?;
            let destination = Inbox::new(Arc::new(kiln_seams::records::SimRecords::new()), Limits::default());
            let mut state = State { manifest: manifest.clone(), role: Role::Destination, phase: Phase::Prepared, target: "there".into(), instance: None, disk_floor: None, package: None, local_destination: None, native: None };
            destination.save_transfer(&state).await?;
            state.phase = Phase::Offered;
            destination.save_transfer(&state).await?;
            let first = rows.first().ok_or_else(|| anyhow::anyhow!("no first record"))?;
            destination.import_pending(&manifest, first).await?;
            assert!(destination.verify_import(&manifest).await.is_err());
            assert!(destination.pending("s", &first.lease.message).await?.is_none());
            assert!(destination.activate_transfer(&manifest).await.is_err());
            for row in &rows { destination.import_pending(&manifest, row).await?; }
            destination.verify_import(&manifest).await?;
            state.phase = Phase::Imported;
            destination.save_transfer(&state).await?;
            assert!(destination.activate_transfer(&manifest).await.is_err());
            state.phase = Phase::Committed;
            destination.save_transfer(&state).await?;
            destination.activate_transfer(&manifest).await?;
            destination.activate_transfer(&manifest).await?;
            let installed = destination.pending("s", &first.lease.message).await?.ok_or_else(|| anyhow::anyhow!("original not installed"))?;
            assert_eq!(installed.lease.epoch, 3);
            assert_eq!(installed.lease.token, first.lease.token);
            assert_eq!(installed.lease.stage, Stage::ProviderFlight);
            assert_eq!(serde_json::to_value(installed.command)?, serde_json::to_value(&first.command)?);
            assert!(source.pending("s", &first.lease.message).await?.is_some(), "source custody was not erased by an import");
            assert!(destination.save_transfer(&State { phase: Phase::Offered, ..state }).await.is_err(), "a delayed offered event cannot regress committed ownership");
            Ok(())
        })
    }
}
