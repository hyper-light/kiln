//! Legacy blue/green ownership metadata and bounded, restartable debt rebinding.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionPhase { AwaitingPromotion, AwaitingCertificate, Rebinding, Active }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Promotion {
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub phase: PromotionPhase,
    pub proof: crate::runner::custody::LegacyProof,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromotionError {
    #[error("the legacy promotion epoch is stale")]
    Stale,
    #[error("invalid legacy promotion: {why}")]
    Invalid { why: String },
    #[error("legacy promotion storage is unavailable: {why}")]
    Storage { why: String },
    #[error("legacy promotion is held by physical byte authority")]
    ResourceHeld,
    #[error("legacy promotion is waiting for its current certificate: {why}")]
    WaitingForCertificate { why: String },
    #[error("legacy promotion observed a concurrent journal change at revision {revision}")]
    Conflict { revision: u64 },
}

impl From<super::Refusal> for PromotionError {
    fn from(error: super::Refusal) -> Self { Self::Storage { why: error.to_string() } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebindStep { Row, Complete }

use std::sync::Arc;
use kiln_seams::records::{self, Cond, Consistency, Scan, Tx, Write};
use crate::harness::admission::{Input, PhysicalBudget};
use super::{Inbox, Cached, Head, Origin, Pending, Retry, HEADS, PENDING, accepting, check_session, session_prefix};

fn storage(error: impl std::fmt::Display) -> PromotionError { PromotionError::Storage { why: error.to_string() } }
fn invalid(why: &str) -> PromotionError { PromotionError::Invalid { why: why.to_owned() } }

impl Inbox {
    pub async fn promotion(&self, session: &str) -> Result<Option<Promotion>, PromotionError> {
        self.promotion_snapshot(session).await.map(|(_, promotion)| promotion)
    }

    pub async fn promotion_snapshot(&self, session: &str) -> Result<(u64, Option<Promotion>), PromotionError> {
        check_session(session)?;
        let row = self.records.get(HEADS, session.as_bytes(), Consistency::Linearizable).await.map_err(storage)?;
        let Some(row) = row else { self.heads.lock().remove(session); return Ok((0, None)) };
        let head: Head = serde_json::from_slice(&row.value).map_err(storage)?;
        let promotion = head.promotion.clone();
        let kept = self.heads.lock().get(session).and_then(|cached| cached.kept);
        self.cache(session, Cached { head, rev: Some(row.rev), kept });
        Ok((row.rev, promotion))
    }

    pub async fn wait_promotion(&self, after: u64) -> Result<u64, PromotionError> { self.records.wait(HEADS, after).await.map_err(storage) }

    pub async fn promotion_head_revision(&self, session: &str) -> Result<u64, PromotionError> {
        Ok(self.records.get(HEADS, session.as_bytes(), Consistency::Linearizable).await.map_err(storage)?.map_or(0, |row| row.rev))
    }

    pub async fn promotion_next(&self, mut after: Option<String>) -> Result<Option<(String, Promotion)>, PromotionError> {
        loop {
            let page = self.records.scan(HEADS, Scan { prefix: Vec::new(), after: after.map(String::into_bytes), limit: 1, at: Consistency::Linearizable }).await.map_err(storage)?;
            let Some(row) = page.rows.into_iter().next() else { return Ok(None) };
            let session = String::from_utf8(row.key).map_err(|_| invalid("inbox head session is not UTF-8"))?;
            let head: Head = serde_json::from_slice(&row.value).map_err(storage)?;
            if let Some(promotion) = head.promotion { return Ok(Some((session, promotion))); }
            after = Some(session);
        }
    }

    async fn write_promotion_head(&self, session: &str, cached: &Cached, head: Head, pool: &Arc<PhysicalBudget>, input: Option<&Input>) -> Result<(), PromotionError> {
        let len = accepting::count(session, &head)?;
        let bytes = len.checked_mul(2).and_then(|n| n.checked_add(size_of::<Cond>())).and_then(|n| n.checked_add(size_of::<Write>())).and_then(|n| n.checked_add(HEADS.len().checked_add(session.len())?.checked_mul(3)?)).ok_or_else(|| invalid("promotion metadata size overflowed"))?;
        let charge = match input { Some(input) => input.reserve_buffer(bytes), None => pool.reserve_buffer(bytes) }.map_err(|_| invalid("promotion metadata reservation overflowed"))?.ok_or(PromotionError::ResourceHeld)?;
        let key = session.as_bytes().to_vec();
        let condition = match cached.rev { Some(rev) => Cond::At { family: HEADS.into(), key: key.clone(), rev }, None => Cond::Absent { family: HEADS.into(), key: key.clone() } };
        let tx = Tx { conds: vec![condition], writes: vec![Write::Put { family: HEADS.into(), key, value: accepting::encode(session, &head, len)? }] };
        let result = self.records.commit_owned(tx, Box::new(charge)).await;
        let revision = result.as_ref().ok().copied();
        if let (Some(input), Some(revision)) = (input, revision) { input.promotion_cursor.fetch_max(revision, std::sync::atomic::Ordering::AcqRel); input.promotion_head.store(revision, std::sync::atomic::Ordering::Release); }
        self.committed(session, cached, Some(head), result).map(|_| ()).map_err(|error| match error { Retry::No(error) => error.into(), _ => storage("promotion head changed concurrently") })
    }

    pub async fn prepare_promotion(&self, session: &str, from: u64, to: u64, proof: crate::runner::custody::LegacyProof, pool: &Arc<PhysicalBudget>, input: Option<&Input>) -> Result<(), PromotionError> {
        check_session(session)?;
        proof.validate(session, from, to).map_err(|why| PromotionError::Invalid { why })?;
        if from.checked_add(1) != Some(to) { return Err(PromotionError::Stale); }
        let cached = self.head(session).await?;
        if let Some(old) = &cached.head.promotion {
            return if old.from_epoch == from && old.to_epoch == to && old.proof == proof { Ok(()) } else { Err(PromotionError::Stale) };
        }
        if cached.rev.is_some() && cached.head.epoch != from { return Err(PromotionError::Stale); }
        let promotion = Promotion { from_epoch: from, to_epoch: to, phase: PromotionPhase::AwaitingPromotion, proof, after: None };
        self.write_promotion_head(session, &cached, Head { epoch: from, moved: true, handover: false, promotion: Some(promotion) }, pool, input).await
    }

    pub async fn advance_promotion(&self, session: &str, to: u64, phase: PromotionPhase, pool: &Arc<PhysicalBudget>, input: Option<&Input>) -> Result<(), PromotionError> {
        let cached = self.head(session).await?;
        let mut head = cached.head.clone();
        let promotion = head.promotion.as_mut().ok_or(PromotionError::Stale)?;
        if promotion.to_epoch != to { return Err(PromotionError::Stale); }
        if promotion.phase == phase { return Ok(()); }
        if !matches!((promotion.phase, phase), (PromotionPhase::AwaitingPromotion, PromotionPhase::AwaitingCertificate) | (PromotionPhase::AwaitingCertificate, PromotionPhase::Rebinding) | (PromotionPhase::Rebinding, PromotionPhase::Active)) { return Err(invalid("promotion phase cannot advance")); }
        if phase == PromotionPhase::Active {
            let next = self.records.scan(PENDING, Scan { prefix: session_prefix(session), after: promotion.after.clone(), limit: 1, at: Consistency::Linearizable }).await.map_err(storage)?;
            if !next.rows.is_empty() { return Err(invalid("promotion journal is not completely rebound")); }
            head.epoch = to; head.moved = false; head.handover = false; promotion.after = None;
        }
        promotion.phase = phase;
        self.write_promotion_head(session, &cached, head, pool, input).await
    }

    pub async fn rebind_promotion_step(&self, session: &str, input: &Input) -> Result<RebindStep, PromotionError> {
        let cached = self.head(session).await?;
        let mut head = cached.head.clone();
        let promotion = head.promotion.as_mut().ok_or(PromotionError::Stale)?;
        if promotion.phase == PromotionPhase::Active { return Ok(RebindStep::Complete); }
        if promotion.phase != PromotionPhase::Rebinding { return Err(PromotionError::WaitingForCertificate { why: "promotion is not certified".into() }); }
        let page = self.records.scan(PENDING, Scan { prefix: session_prefix(session), after: promotion.after.clone(), limit: 1, at: Consistency::Linearizable }).await.map_err(storage)?;
        let Some(row) = page.rows.into_iter().next() else { return Ok(RebindStep::Complete) };
        let mut pending: Pending = serde_json::from_slice(&row.value).map_err(storage)?;
        if pending.lease.token.is_some() || !matches!(pending.origin, Origin::LegacyRetained | Origin::LegacyOpaque) { return Err(invalid("token-bearing or nonlegacy custody requires a verified transfer")); }
        if pending.lease.epoch != promotion.from_epoch && pending.lease.epoch != promotion.to_epoch { return Err(PromotionError::Stale); }
        // An absent sequence means the original row revision, not a later migration write.
        pending.accepted_seq = Some(pending.accepted_seq.unwrap_or(row.rev));
        pending.lease.epoch = promotion.to_epoch;
        promotion.after = Some(row.key.clone());
        let row_len = accepting::count(session, &pending)?;
        let head_len = accepting::count(session, &head)?;
        let driver = row_len.checked_add(PENDING.len()).and_then(|n| n.checked_add(row.key.len())).ok_or_else(|| invalid("promotion row binding overflowed"))?.max(head_len.saturating_add(HEADS.len()).saturating_add(session.len()));
        let bytes = row_len.checked_add(head_len).and_then(|n| n.checked_add(driver)).and_then(|n| n.checked_add(size_of::<Cond>().checked_mul(2)?)).and_then(|n| n.checked_add(size_of::<Write>().checked_mul(2)?)).and_then(|n| n.checked_add(PENDING.len().checked_add(row.key.len())?.checked_mul(2)?)).and_then(|n| n.checked_add(HEADS.len().checked_add(session.len())?.checked_mul(2)?)).ok_or_else(|| invalid("promotion transaction size overflowed"))?;
        let charge = input.reserve_buffer(bytes).map_err(|_| invalid("promotion row reservation overflowed"))?.ok_or(PromotionError::ResourceHeld)?;
        let head_key = session.as_bytes().to_vec();
        let head_rev = cached.rev.ok_or(PromotionError::Stale)?;
        let tx = Tx { conds: vec![Cond::At { family: HEADS.into(), key: head_key.clone(), rev: head_rev }, Cond::At { family: PENDING.into(), key: row.key.clone(), rev: row.rev }], writes: vec![Write::Put { family: PENDING.into(), key: row.key, value: accepting::encode(session, &pending, row_len)? }, Write::Put { family: HEADS.into(), key: head_key, value: accepting::encode(session, &head, head_len)? }] };
        let result = self.records.commit_owned(tx, Box::new(charge)).await;
        if let Ok(revision) = &result { input.promotion_cursor.fetch_max(*revision, std::sync::atomic::Ordering::AcqRel); input.promotion_head.store(*revision, std::sync::atomic::Ordering::Release); }
        if let Err(records::Refusal::Conflict { at: Some(revision), .. }) = &result {
            self.heads.lock().remove(session);
            return Err(PromotionError::Conflict { revision: *revision });
        }
        self.committed(session, &cached, Some(head), result).map(|_| RebindStep::Row).map_err(|error| match error { Retry::No(error) => error.into(), _ => storage("promotion row or head changed concurrently") })
    }

    pub async fn retire_promotion(&self, session: &str, to: u64, pool: &Arc<PhysicalBudget>, input: Option<&Input>) -> Result<(), PromotionError> {
        let cached = self.head(session).await?;
        let mut head = cached.head.clone();
        let Some(promotion) = &head.promotion else { return Ok(()) };
        if promotion.phase != PromotionPhase::Active || promotion.to_epoch != to || head.epoch < to { return Err(PromotionError::Stale); }
        head.promotion = None;
        self.write_promotion_head(session, &cached, head, pool, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};
    use futures::{FutureExt as _, future::{BoxFuture, Either, select}};
    use kiln_seams::records::{Changes, Page, Records, Refusal, Rev, Row};
    use parking_lot::Mutex;
    use crate::harness::{admission::{Consumption, Disposition, Input, Lease, PhysicalBudget, Stage}, inbox::{Inbox, Limits, Pending}, mail::Token, model::{Command, SendMode}};

    struct ReadGate {
        entered: futures::channel::oneshot::Sender<()>,
        release: futures::channel::oneshot::Receiver<()>,
    }

    /// Real file-backed records, with one precisely selected failure/cancellation boundary.
    struct FaultRecords {
        inner: Arc<dyn Records>,
        commit_fault: AtomicU8,
        read_gate: Mutex<Option<ReadGate>>,
    }

    impl FaultRecords {
        fn new(inner: Arc<dyn Records>) -> Self { Self { inner, commit_fault: AtomicU8::new(0), read_gate: Mutex::new(None) } }
    }

    impl Records for FaultRecords {
        fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>> {
            let gate = if family == HEADS && key == b"s" { self.read_gate.lock().take() } else { None };
            Box::pin(async move {
                if let Some(gate) = gate { let _ = gate.entered.send(()); let _ = gate.release.await; }
                self.inner.get(family, key, at).await
            })
        }
        fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Refusal>> { self.inner.scan(family, scan) }
        fn commit<'a>(&'a self, tx: Tx) -> BoxFuture<'a, Result<Rev, Refusal>> { self.inner.commit(tx) }
        fn commit_owned<'a>(&'a self, tx: Tx, owner: Box<dyn Send + Sync>) -> BoxFuture<'a, Result<Rev, Refusal>> {
            let fault = self.commit_fault.swap(0, Ordering::AcqRel);
            Box::pin(async move {
                if fault == 1 { return Err(Refusal::Unavailable("before commit (injected)".into())); }
                let result = self.inner.commit_owned(tx, owner).await;
                if fault == 2 && result.is_ok() { return Err(Refusal::Unavailable("after durable commit (injected)".into())); }
                result
            })
        }
        fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Refusal>> { self.inner.changes(family, after, limit) }
        fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Refusal>> { self.inner.wait(family, after) }
        fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>> { self.inner.head() }
    }

    async fn within<T>(future: impl std::future::Future<Output = T>) -> anyhow::Result<T> {
        let deadline = crate::rt::clock().sleep(crate::harness::registry::ANSWER_WAIT);
        futures::pin_mut!(future, deadline);
        match select(future, deadline).await { Either::Left((value, _)) => Ok(value), Either::Right(_) => anyhow::bail!("promotion fixture protocol deadline") }
    }

    fn pending(id: &str) -> Pending {
        Pending::legacy(Lease { message: id.into(), peer: String::new(), epoch: 0, bytes: 80, stage: Stage::ProviderFlight, token: None, handoff: None }, Command::Send { id: id.into(), text: format!("original {id}"), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None })
    }

    async fn prepare(inbox: &Inbox, pool: &Arc<PhysicalBudget>) -> anyhow::Result<()> {
        inbox.open("s", 0).await?;
        let proof = crate::runner::custody::LegacyProof::of("s", 0, &[], None).map_err(anyhow::Error::msg)?;
        inbox.prepare_promotion("s", 0, 1, proof, pool, None).await?;
        Ok(())
    }

    async fn certified(inbox: &Inbox, input: &Input, pool: &Arc<PhysicalBudget>) -> anyhow::Result<()> {
        input.controller.lock().begin_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        inbox.advance_promotion("s", 1, PromotionPhase::AwaitingCertificate, pool, Some(input)).await?;
        inbox.advance_promotion("s", 1, PromotionPhase::Rebinding, pool, Some(input)).await?;
        Ok(())
    }

    #[test]
    fn partial_legacy_epoch_rebind_reopens_without_changing_originals_or_debt() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("records.db");
        crate::rt::block_on(async {
            let records = Arc::new(FaultRecords::new(crate::records::open(&path)?));
            let pool = PhysicalBudget::new(kiln_seams::records::VALUE_MAX * 2);
            let inbox = Arc::new(Inbox::new(records.clone(), Limits::default()));
            prepare(&inbox, &pool).await?;
            inbox.adopt_pending("s", pending("a"), 11).await?;
            inbox.adopt_pending("s", Pending::opaque(pending("b").lease, vec!["exact retained native bytes".into()]), 12).await?;
            let first = inbox.pending("s", "a").await?.unwrap();
            let second = inbox.pending("s", "b").await?.unwrap();
            let first_revision = records.get(PENDING, &super::super::message_key("s", "a"), Consistency::Linearizable).await?.unwrap().rev;
            let second_revision = records.get(PENDING, &super::super::message_key("s", "b"), Consistency::Linearizable).await?.unwrap().rev;
            let input = Input::new(0, pool.clone(), inbox.clone());
            input.controller.lock().restore(first.lease.clone()).map_err(|error| anyhow::anyhow!("{error:?}"))?;
            input.controller.lock().restore(second.lease.clone()).map_err(|error| anyhow::anyhow!("{error:?}"))?;
            let credit = input.controller.lock().capacity().retry_permission();
            certified(&inbox, &input, &pool).await?;
            assert_eq!(inbox.rebind_promotion_step("s", &input).await?, RebindStep::Row);
            let key = super::super::message_key("s", "b");
            let before = records.get(PENDING, &key, Consistency::Linearizable).await?.unwrap();
            let head = inbox.promotion_snapshot("s").await?;
            records.commit_fault.store(1, Ordering::Release);
            assert!(matches!(inbox.rebind_promotion_step("s", &input).await, Err(PromotionError::Storage { .. })));
            let after = records.get(PENDING, &key, Consistency::Linearizable).await?.unwrap();
            assert_eq!((after.rev, after.value), (before.rev, before.value), "failed CAS changed the accepted original");
            assert_eq!(inbox.promotion_snapshot("s").await?, head, "failed CAS advanced its resume cursor");
            pool.resize(0);
            assert_eq!(inbox.rebind_promotion_step("s", &input).await, Err(PromotionError::ResourceHeld));
            drop(input); drop(inbox); drop(records);
            assert_eq!(pool.counts().0, 0);

            let records = crate::records::open(&path)?;
            let inbox = Arc::new(Inbox::new(records, Limits::default()));
            assert!(inbox.open("s", 1).await.is_err());
            pool.resize(kiln_seams::records::VALUE_MAX * 2);
            let input = Input::new(0, pool.clone(), inbox.clone());
            input.controller.lock().begin_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
            for id in ["a", "b"] {
                let retained = inbox.pending("s", id).await?.unwrap();
                input.controller.lock().restore_legacy_debt(retained.lease, 0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
            }
            while inbox.rebind_promotion_step("s", &input).await? != RebindStep::Complete {}
            for (original, revision) in [(first, first_revision), (second, second_revision)] {
                let mut expected = original.clone(); expected.lease.epoch = 1; expected.accepted_seq = Some(original.accepted_seq.unwrap_or(revision));
                let actual = inbox.pending("s", &original.lease.message).await?.unwrap();
                assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(expected)?);
            }
            input.controller.lock().rebind_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
            assert_eq!(pool.counts().0, 160);
            assert_eq!(input.controller.lock().capacity().retry_permission(), credit);
            inbox.advance_promotion("s", 1, PromotionPhase::Active, &pool, Some(&input)).await?;
            assert_eq!(inbox.epoch("s").await?, Some((1, false)));
            assert_eq!(inbox.advance_promotion("s", 2, PromotionPhase::Active, &pool, Some(&input)).await, Err(PromotionError::Stale));
            inbox.retire_promotion("s", 1, &pool, Some(&input)).await?;
            input.controller.lock().complete_legacy_reconciliation().map_err(|error| anyhow::anyhow!("{error:?}"))?;
            input.controller.lock().activate_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
            let terminal = Disposition::Consumed { boundary: Consumption::ProviderAccepted };
            inbox.dispose_pending("s", "a", 1, terminal.clone()).await?;
            assert!(input.controller.lock().finish("a", terminal, 1, false).map_err(|error| anyhow::anyhow!("{error:?}"))?);
            assert_eq!(pool.counts().0, 80, "completion must release only its own lease");
            assert!(inbox.pending("s", "b").await?.unwrap().disposition.is_none());
            let mut ordinary = pending("new"); ordinary.origin = Origin::Accepted; ordinary.lease.epoch = 1; ordinary.lease.token = Some(Token { revision: 4, generation: 1 });
            inbox.accept_pending("s", "new", &super::super::digest("original new", &[]), Some(1), 20, &ordinary).await?;
            assert!(inbox.promotion("s").await?.is_none());
            assert!(!input.controller.lock().promoting());
            assert_eq!(input.controller.lock().capacity().epoch, 1);
            Ok(())
        })
    }

    #[test]
    fn applied_then_error_and_cancelled_head_read_keep_the_promotion_wake() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        crate::rt::block_on(async {
            let records = Arc::new(FaultRecords::new(crate::records::open(&dir.path().join("records.db"))?));
            let pool = PhysicalBudget::new(kiln_seams::records::VALUE_MAX * 2);
            let inbox = Arc::new(Inbox::new(records.clone(), Limits::default()));
            prepare(&inbox, &pool).await?;
            let input = Input::new(0, pool.clone(), inbox.clone());
            certified(&inbox, &input, &pool).await?;
            assert_eq!(inbox.rebind_promotion_step("s", &input).await?, RebindStep::Complete);
            input.controller.lock().rebind_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
            inbox.advance_promotion("s", 1, PromotionPhase::Active, &pool, Some(&input)).await?;
            let (revision, _) = inbox.promotion_snapshot("s").await?;
            assert_eq!(input.promotion_cursor.load(Ordering::Acquire), revision);
            records.commit_fault.store(2, Ordering::Release);
            assert!(matches!(inbox.retire_promotion("s", 1, &pool, Some(&input)).await, Err(PromotionError::Storage { .. })));
            assert!(input.controller.lock().promoting(), "an unknown commit outcome cannot open the owner");
            let (entered, entry) = futures::channel::oneshot::channel();
            let (release, released) = futures::channel::oneshot::channel();
            *records.read_gate.lock() = Some(ReadGate { entered, release: released });
            let mut listener = Box::pin(input.promotion_journal_changed("s"));
            within(async {
                futures::select! {
                    result = entry.fuse() => result?,
                    _ = listener.as_mut().fuse() => anyhow::bail!("listener completed before its paused Head read"),
                }
                Ok::<_, anyhow::Error>(())
            }).await??;
            drop(listener);
            assert_eq!(input.promotion_cursor.load(Ordering::Acquire), revision, "cancelled read consumed the only real wake");
            let _ = release.send(());
            within(input.promotion_journal_changed("s")).await?;
            let (retired_revision, promotion) = inbox.promotion_snapshot("s").await?;
            assert!(retired_revision > revision && promotion.is_none());
            assert_eq!(inbox.epoch("s").await?, Some((1, false)));
            inbox.retire_promotion("s", 1, &pool, Some(&input)).await?;
            input.controller.lock().complete_legacy_reconciliation().map_err(|error| anyhow::anyhow!("{error:?}"))?;
            input.controller.lock().activate_legacy_epoch(0, 1).map_err(|error| anyhow::anyhow!("{error:?}"))?;
            assert!(!input.controller.lock().promoting());
            assert_eq!(input.controller.lock().capacity().epoch, 1);
            Ok(())
        })
    }

    #[test]
    fn legacy_epoch_rebind_refuses_token_or_stale_rows_without_mutation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        crate::rt::block_on(async {
            let records = crate::records::open(&dir.path().join("records.db"))?;
            let pool = PhysicalBudget::new(kiln_seams::records::VALUE_MAX * 2);
            let inbox = Arc::new(Inbox::new(records.clone(), Limits::default()));
            prepare(&inbox, &pool).await?;
            inbox.adopt_pending("s", pending("a"), 11).await?;
            let original = inbox.pending("s", "a").await?.unwrap();
            let input = Input::new(0, pool.clone(), inbox.clone());
            input.controller.lock().restore(original.lease.clone()).map_err(|error| anyhow::anyhow!("{error:?}"))?;
            certified(&inbox, &input, &pool).await?;
            let key = super::super::message_key("s", "a");
            let saved = records.get(PENDING, &key, Consistency::Linearizable).await?.unwrap();
            let mut token_row = original.clone(); token_row.lease.token = Some(Token { revision: 1, generation: 1 });
            let token_revision = records.commit(Tx { conds: vec![Cond::At { family: PENDING.into(), key: key.clone(), rev: saved.rev }], writes: vec![Write::Put { family: PENDING.into(), key: key.clone(), value: serde_json::to_vec(&token_row)? }] }).await?;
            assert!(matches!(inbox.rebind_promotion_step("s", &input).await, Err(PromotionError::Invalid { .. })));
            assert_eq!(serde_json::to_value(inbox.pending("s", "a").await?.unwrap())?, serde_json::to_value(&token_row)?);
            let mut stale = original; stale.lease.epoch = 2;
            records.commit(Tx { conds: vec![Cond::At { family: PENDING.into(), key: key.clone(), rev: token_revision }], writes: vec![Write::Put { family: PENDING.into(), key, value: serde_json::to_vec(&stale)? }] }).await?;
            assert_eq!(inbox.rebind_promotion_step("s", &input).await, Err(PromotionError::Stale));
            assert_eq!(serde_json::to_value(inbox.pending("s", "a").await?.unwrap())?, serde_json::to_value(stale)?);
            assert_eq!(pool.counts().0, 80);
            assert_eq!(input.controller.lock().capacity().epoch, 0);
            assert!(input.controller.lock().promoting());
            Ok(())
        })
    }
}
