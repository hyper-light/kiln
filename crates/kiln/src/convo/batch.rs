//! U05/U17/U19: immutable admitted effects and their selected owner's durable answer.
//! The receipt certifies this batch, not admission or a superseding checkpoint. Publication
//! and harness control remain with the session gate; dropping a wait never cancels a write.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use super::rows::{self, Row};

/// The final SQL request, with space for statements and transaction metadata.
pub const PREPARED_MAX: usize = amphora_api::REQUEST_MAX.saturating_sub(1 << 20);
/// Projection-only flushes leave additional room below the final request bound.
pub const GROUP_MAX: usize = 16 << 20;
/// Pinned Amphora wire.rs COMMAND_MAX is 4096. Leave room for protocol/finalization.
/// Source proof: meta+Commit<=5; retention identities add at most32 row exchanges;
/// a prior staged Record can read512 parts and is conservatively bounded at640.
/// WORK_MAX and BASE_WORK are shared by admission, actor grouping and backend preflight.
pub const WORK_MAX: usize = 4000;
pub const BASE_WORK: usize = 32;
const FIXED_BYTES: usize = 4096;

/// Input is returned intact when construction or admission is refused.
pub struct BatchInput {
    pub session: String,
    pub rows: Vec<Row>,
    pub records: Option<Box<crate::records::BoundRecordsTx>>,
}

/// A sealed effect of one session. No mutable row/transaction access follows admission.
pub struct Batch {
    input: Arc<BatchInput>,
    retained: usize,
    prepared: usize,
    work: usize,
}

impl std::fmt::Debug for BatchInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchInput").field("session", &self.session).field("rows", &self.rows.len()).field("records", &self.records.is_some()).finish()
    }
}
impl std::fmt::Debug for Batch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Batch").field("input", &self.input).field("retained_bytes", &self.retained).field("prepared_bytes", &self.prepared).field("work", &self.work).finish()
    }
}

impl Batch {
    pub fn new(input: BatchInput) -> Result<Self, Rejected<BatchInput>> {
        let checked = || -> Result<(usize, usize, usize), AdmissionRefusal> {
            if input.session.is_empty() || input.rows.iter().any(|row| row.session() != input.session) {
                return Err(AdmissionRefusal::Invalid("a batch must name one nonempty session and contain only its rows".into()));
            }
            if input.rows.is_empty() && input.records.is_none() {
                return Err(AdmissionRefusal::Invalid("an empty projection needs its effect's previous ticket or an explicit barrier".into()));
            }
            if let Some(size) = input.rows.iter().find_map(|row| match row {
                Row::Record(record) if record.line.len() > super::segments::LINE_MAX => Some(record.line.len()),
                _ => None,
            }) {
                return Err(AdmissionRefusal::TooLarge { size, limit: super::segments::LINE_MAX });
            }
            let overflow = || AdmissionRefusal::TooLarge { size: usize::MAX, limit: PREPARED_MAX };
            let mut retained = FIXED_BYTES.checked_add(input.session.capacity()).ok_or_else(overflow)?;
            retained = retained.checked_add(input.rows.capacity().checked_mul(std::mem::size_of::<Row>()).ok_or_else(overflow)?).ok_or_else(overflow)?;
            let mut prepared = FIXED_BYTES;
            let mut work = BASE_WORK;
            for row in &input.rows {
                work = work.checked_add(row.command_cost()).ok_or(AdmissionRefusal::TooLarge { size: usize::MAX, limit: WORK_MAX })?;
                retained = retained.checked_add(row.retained_bytes().ok_or_else(overflow)?).ok_or_else(overflow)?;
                prepared = prepared.checked_add(row.prepared_bytes().ok_or_else(overflow)?).ok_or_else(overflow)?;
            }
            if let Some(records) = &input.records {
                let bytes = records.bytes()?;
                work = work.checked_add(1).ok_or(AdmissionRefusal::TooLarge { size: usize::MAX, limit: WORK_MAX })?;
                retained = retained.checked_add(bytes).ok_or_else(overflow)?;
                prepared = prepared.checked_add(bytes).ok_or_else(overflow)?;
            }
            if work > WORK_MAX { return Err(AdmissionRefusal::TooLarge { size: work, limit: WORK_MAX }); }
            if prepared > PREPARED_MAX {
                return Err(AdmissionRefusal::TooLarge { size: prepared, limit: PREPARED_MAX });
            }
            Ok((retained, prepared, work))
        };
        match checked() {
            Ok((retained, prepared, work)) => Ok(Self { input: Arc::new(input), retained, prepared, work }),
            Err(reason) => Err(Rejected { input, reason }),
        }
    }

    pub fn projection(session: String, rows: Vec<Row>) -> Result<Self, Rejected<BatchInput>> {
        Self::new(BatchInput { session, rows, records: None })
    }

    /// Conversion is fallible; the borrowed harness update stays with its owner on error.
    /// None means this protocol frame has no conversation projection, not a durable success.
    pub fn from_update(session: &str, update: &crate::harness::core::Update, now: i64) -> Result<Option<Self>, AdmissionRefusal> {
        let rows = rows::try_of(session, update, now).map_err(|e| AdmissionRefusal::Invalid(format!("conversation update could not be encoded: {e}")))?;
        if rows.is_empty() { return Ok(None); }
        Self::projection(session.to_owned(), rows).map(Some).map_err(|e| e.reason)
    }

    // Admission keeps a shared immutable owner until channel handoff has succeeded, so a
    // SyncSender refusal returns the original input without an impossible-variant panic.
    pub(crate) fn shared(&self) -> Self { Self { input: self.input.clone(), retained: self.retained, prepared: self.prepared, work: self.work } }
    pub fn session(&self) -> &str { &self.input.session }
    pub fn rows(&self) -> &[Row] { &self.input.rows }
    pub fn records(&self) -> Option<&crate::records::BoundRecordsTx> { self.input.records.as_deref() }
    /// Retained input capacity plus fixed envelope/ticket accounting. This is not process RSS.
    pub fn retained_bytes(&self) -> usize { self.retained }
    /// A conservative final-request bound; a long transcript stages to a small immutable ref.
    pub fn prepared_bytes(&self) -> usize { self.prepared }
    pub fn command_cost(&self) -> usize { self.work }
}

/// One final selected-owner call pays BASE_WORK once. An empty group submits no call.
/// Each sealed batch keeps its own checked cost; only the repeated fixed allowance is removed.
pub fn group_command_cost<'a>(batches: impl IntoIterator<Item = &'a Batch>) -> Option<usize> {
    let mut total = 0usize;
    let mut started = false;
    for batch in batches {
        if !started { total = BASE_WORK; started = true; }
        total = total.checked_add(batch.command_cost().checked_sub(BASE_WORK)?)?;
    }
    Some(total)
}

pub struct Rejected<T> {
    pub input: T,
    pub reason: AdmissionRefusal,
}

impl<T> std::fmt::Debug for Rejected<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rejected").field("reason", &self.reason).finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionRefusal {
    #[error("conversation admission is full ({limit_batches} batches or {limit_bytes} retained bytes)")]
    Full { limit_batches: usize, limit_bytes: usize },
    #[error("conversation batch size {size} exceeds limit {limit}")]
    TooLarge { size: usize, limit: usize },
    #[error("invalid conversation batch: {0}")]
    Invalid(String),
    #[error("the conversation writer is closed")]
    Closed,
    #[error("the conversation writer is fenced: {0}")]
    Fenced(Arc<ApplyFailure>),
    #[error("the admission deadline elapsed; the input remains with its caller")]
    Deadline,
}

/// The exact final transaction's revisions, returned only after durable owner completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    pub conversation_rev: u64,
    pub records_rev: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApplyFailure {
    #[error("the batch did not commit: {0}")]
    Definite(amphora_api::Refusal),
    #[error("the batch's outcome is unknown; reconcile it before retrying: {0}")]
    Unknown(amphora_api::Refusal),
    #[error("the batch was not submitted because the writer was fenced: {0}")]
    Blocked(Arc<ApplyFailure>),
}

type Answer = Result<Committed, Arc<ApplyFailure>>;

#[derive(Default)]
struct State {
    answer: Mutex<Option<Answer>>,
    ready: event_listener::Event,
    failed: Mutex<Option<(Batch, Option<super::store::Permit>)>>,
}

/// A waitable answer for one admitted batch, with no global receipt registry.
#[derive(Clone, Default)]
pub struct Ticket(Arc<State>);

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let answer = self.try_result();
        let state = match answer.as_ref() {
            None => "pending",
            Some(Ok(_)) => "durable",
            Some(Err(error)) => match error.as_ref() {
                ApplyFailure::Definite(_) => "definite refusal",
                ApplyFailure::Unknown(_) => "unknown outcome",
                ApplyFailure::Blocked(_) => "blocked before submit",
            },
        };
        f.debug_struct("Ticket").field("state", &state).finish()
    }
}

impl Ticket {
    pub(crate) fn pending() -> (Self, Completion) {
        let ticket = Self::default();
        let completion = Completion(ticket.clone());
        (ticket, completion)
    }

    pub fn try_result(&self) -> Option<Answer> {
        self.0.answer.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Returns a definitely refused input once. Ownership transfers back to the caller and
    /// releases its store quota. An unknown outcome stays with the fenced writer instead.
    pub fn take_failed(&self) -> Option<Batch> {
        if !matches!(self.try_result(), Some(Err(failure)) if matches!(failure.as_ref(), ApplyFailure::Definite(_))) { return None; }
        let (batch, permit) = self.0.failed.lock().unwrap_or_else(PoisonError::into_inner).take()?;
        drop(permit);
        Some(batch)
    }

    pub async fn wait(&self) -> Answer {
        loop {
            let ready = self.0.ready.listen();
            if let Some(answer) = self.try_result() { return answer; }
            ready.await;
        }
    }

    /// A deadline expires only this wait. The ticket remains pending and its input stays owned.
    pub async fn wait_until(&self, until: Instant) -> Result<Answer, crate::rt::Elapsed> {
        crate::rt::timeout(until.saturating_duration_since(Instant::now()), self.wait()).await
    }
}

pub(crate) struct Completion(Ticket);

impl Completion {
    pub(crate) fn refused(&self, batch: Batch, permit: super::store::Permit, failure: Arc<ApplyFailure>) {
        *self.0.0.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some((batch, Some(permit)));
        self.finish(Err(failure));
    }

    pub(crate) fn finish(&self, answer: Answer) {
        let mut current = self.0.0.answer.lock().unwrap_or_else(PoisonError::into_inner);
        if current.is_none() {
            *current = Some(answer);
            drop(current);
            self.0.0.ready.notify(usize::MAX);
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        // Unexpected writer teardown is never a success or cancellation of accepted work.
        self.finish(Err(Arc::new(ApplyFailure::Unknown(amphora_api::Refusal::Unavailable("the conversation writer stopped before a durable answer".into())))));
    }
}

impl Drop for State {
    fn drop(&mut self) {
        // The worker retains Completion through the attempt. Only a terminal definite
        // result can own failed input here; abandoning it is explicit in the counters.
        if let Some((_, Some(permit))) = self.failed.get_mut().unwrap_or_else(PoisonError::into_inner).as_ref() {
            permit.abandoned();
        }
    }
}

/// The caller retains either the original input or its admitted ticket across cancellation.
#[derive(Debug)]
pub struct PendingSubmission {
    pub(crate) batch: Option<Batch>,
    pub(crate) ticket: Option<Ticket>,
}

impl PendingSubmission {
    pub fn new(batch: Batch) -> Self { Self { batch: Some(batch), ticket: None } }
    pub fn ticket(&self) -> Option<&Ticket> { self.ticket.as_ref() }
    pub fn input(&self) -> Option<&Batch> { self.batch.as_ref() }
    pub fn into_input(self) -> Option<Batch> { self.batch }
}


/// Shared cancellation-safe room wait for the production actor and deterministic strategy.
/// The input/ticket slot is always restored before suspension; no await follows transfer.
pub(crate) async fn wait_for_admission(
    pending: &mut PendingSubmission,
    until: Instant,
    room: &event_listener::Event,
    mut admit: impl FnMut(Batch) -> Result<Ticket, Rejected<Batch>>,
) -> Result<(), AdmissionRefusal> {
    loop {
        if pending.ticket.is_some() { return Ok(()); }
        if Instant::now() >= until { return Err(AdmissionRefusal::Deadline); }
        let changed = room.listen();
        let batch = pending.batch.take().ok_or_else(|| AdmissionRefusal::Invalid("the submission has no prepared input".into()))?;
        match admit(batch) {
            Ok(ticket) => { pending.ticket = Some(ticket); return Ok(()); }
            Err(Rejected { input, reason }) => {
                pending.batch = Some(input);
                if !matches!(reason, AdmissionRefusal::Full { .. }) { return Err(reason); }
            }
        }
        if crate::rt::timeout(until.saturating_duration_since(Instant::now()), changed).await.is_err() {
            return Err(AdmissionRefusal::Deadline);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(session: &str, text: String) -> Row {
        Row::Session { session: session.into(), info: None, meta: None, state: Some(text), changes: None, epoch: None, open: false, at: 1 }
    }

    #[test]
    fn u05_retained_capacity_is_charged_and_payload_is_absent_from_diagnostics() -> anyhow::Result<()> {
        let secret = "private-transcript-sentinel";
        let mut text = String::with_capacity(32 << 10);
        text.push_str(secret);
        let text_capacity = text.capacity();
        let mut rows = Vec::with_capacity(128);
        rows.push(state("s", text));
        let row_capacity = rows.capacity().checked_mul(std::mem::size_of::<Row>()).ok_or_else(|| anyhow::anyhow!("test capacity overflow"))?;
        let batch = Batch::projection("s".into(), rows).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        anyhow::ensure!(batch.retained_bytes() >= text_capacity + row_capacity, "unused allocated capacity is still retained input");
        let refused = Rejected { input: batch, reason: AdmissionRefusal::Full { limit_batches: 1, limit_bytes: 1 } };
        anyhow::ensure!(!format!("{:?}", refused.input).contains(secret) && !format!("{refused:?}").contains(secret), "generic diagnostics disclosed the payload");
        anyhow::ensure!(matches!(refused.input.rows(), [Row::Session { state: Some(value), .. }] if value == secret), "diagnostic redaction changed input ownership");
        Ok(())
    }

    #[test]
    fn u05_remote_work_refusal_returns_the_whole_oversized_effect_before_transfer() -> anyhow::Result<()> {
        let rows = (0..2_000).map(|_| Row::Closed { session: "s".into(), at: 1 }).collect();
        let rejected = Batch::projection("s".into(), rows).err().ok_or_else(|| anyhow::anyhow!("oversized remote effect was admitted"))?;
        anyhow::ensure!(matches!(rejected.reason, AdmissionRefusal::TooLarge { limit: WORK_MAX, .. }) && rejected.input.rows.len() == 2_000, "work refusal split or consumed its input");
        let a = Batch::projection("a".into(), (0..62).map(|_| Row::Closed { session: "a".into(), at: 1 }).collect()).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        let b = Batch::projection("b".into(), (0..62).map(|_| Row::Closed { session: "b".into(), at: 1 }).collect()).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        anyhow::ensure!(group_command_cost([&a, &b]) == Some(WORK_MAX), "group charged a fixed call allowance per envelope instead of once");
        anyhow::ensure!(group_command_cost(std::iter::empty()) == Some(0), "an empty group consumed a call allowance");
        Ok(())
    }

    #[test]
    fn u05_no_projection_is_not_success_and_cross_session_input_is_returned() -> anyhow::Result<()> {
        let stream = crate::harness::core::Update::Stream { id: "m".into(), text: "partial".into() };
        anyhow::ensure!(Batch::from_update("s", &stream, 1)?.is_none(), "an ephemeral projection manufactured a durable effect");
        let rejected = Batch::projection("s".into(), vec![state("s", "one".into()), state("other", "two".into())]).err().ok_or_else(|| anyhow::anyhow!("cross-session batch was admitted"))?;
        anyhow::ensure!(matches!(rejected.reason, AdmissionRefusal::Invalid(_)) && rejected.input.rows.len() == 2, "mixed-session input was not returned intact");
        Ok(())
    }
}
