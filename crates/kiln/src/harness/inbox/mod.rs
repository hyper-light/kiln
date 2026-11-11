//! A session's inbox (CP-T3.2; docs/sends.md). Every message sent to a session carries an id its
//! originator made once (the UI's composer, `agents_send`, the API, a plan run, the goal driver),
//! and every hop and every retry carries that same id. The inbox, on the host the session runs on,
//! takes each id once: the first time, the message goes on to the session; every time after, the
//! sender gets the first one's answer and the session gets nothing. A retried send is never a
//! second prompt, whatever retried it.
//!
//! The inbox is fenced by the session's epoch ([`super::model::State::epoch`], how many times it
//! went on on another machine). A send carries the epoch its server saw. One for an epoch the
//! session has left is refused, naming both epochs; so is one for an epoch this copy hasn't reached
//! (it's a copy left behind), and every send once the session went on elsewhere.
//!
//! It's a pure core over kiln-seams' `Records` (Amphora's, in time): kiln's Turso store on a host,
//! and the simulator in tests. What it keeps of a session:
//! - `inbox-heads/<session>`: its epoch, and whether it went on elsewhere. Written as the session
//!   opens, goes on elsewhere or comes back, never by a take: every take is conditioned on it being
//!   as the inbox last read it, so a take, an epoch and a fence are one order, and a fence's commit
//!   refuses every take after it.
//! - `inbox/<session>/<message>`: each message taken, with its epoch, time and digest. Its place
//!   is the revision its take was committed at: a take is one write.
//!
//! Bounds: a session keeps its last [`KEEP`] messages' ids; past [`KEEP`] and [`TRIM`] more, the
//! oldest [`TRIM`] go at once. A repeat of a message older than that is taken as new. A host keeps
//! the heads of the sessions it has in memory, one each, at most [`HEADS_MAX`].

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

use std::collections::HashMap;
use std::sync::Arc;

use kiln_seams::records::{self, Cond, Consistency, Records, Rev, Scan, Tx, Write};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub mod transfer;
mod accepting;

/// Ids a session keeps: the duplicates of its last this many messages are known.
pub const KEEP: u64 = 4096;
/// Ids that go at once, once a session keeps [`KEEP`] and this many more.
pub const TRIM: u64 = 256;
/// Longest message id.
pub const ID_MAX: usize = 128;
/// Times a take is tried again when its session's head changed under it, before it's refused.
const RETRIES: usize = 3;
/// Heads a host keeps in memory, at most: past it they're forgotten, and read again as needed.
pub const HEADS_MAX: usize = 4096;
/// Ids let go of in one transaction: each is one write, within a transaction's most.
const PAGE: usize = 500;
/// A session's ids read at once, at most (a tidy, a move's carry): twice what it keeps at its most.
/// Past it (its tidies failing), a tidy lets the oldest of those it read go, and a move waits until
/// it's tidied (the twelfth adversarial check's: no read without its bound).
pub const READ: u64 = KEEP.saturating_add(TRIM).saturating_mul(2);
/// Ids brought from another machine taken in one transaction: each is a write and a condition.
const IMPORT_CHUNK: usize = 400;

const HEADS: &str = "inbox-heads";
const MESSAGES: &str = "inbox";
const PENDING: &str = "inbox-pending";

/// Replayable custody is separate from the bounded deduplication history. A terminal record is
/// retained until the server durably acknowledges its disposition, including across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub lease: super::admission::Lease,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<super::model::Command>,
    /// Only bytes actually retained by the old provider writer; an opaque flight never creates
    /// an ordinary Send or automatically replays these uncertain native requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opaque_writes: Vec<String>,
    #[serde(default)]
    pub origin: Origin,
    /// Explicit current-body revision; the accepted original above is never rewritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<Current>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<super::admission::Disposition>,
    #[serde(default)]
    accepted_at: i64,
    #[serde(default)]
    digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accepted_seq: Option<Rev>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin { #[default] Accepted, LegacyRetained, LegacyOpaque }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Current { pub revision: u64, pub command: super::model::Command }

impl Pending {
    pub fn new(lease: super::admission::Lease, command: super::model::Command) -> Self {
        let body_bytes = super::admission::command_bytes(&command).ok();
        Self { lease, command: Some(command), opaque_writes: Vec::new(), origin: Origin::Accepted, current: None, body_bytes, disposition: None, accepted_at: 0, digest: String::new(), accepted_seq: None }
    }
    pub fn legacy(lease: super::admission::Lease, command: super::model::Command) -> Self { Self { origin: Origin::LegacyRetained, ..Self::new(lease, command) } }
    pub fn opaque(lease: super::admission::Lease, opaque_writes: Vec<String>) -> Self { Self { lease, command: None, opaque_writes, origin: Origin::LegacyOpaque, current: None, body_bytes: None, disposition: None, accepted_at: 0, digest: String::new(), accepted_seq: None } }
    pub fn current_command(&self) -> Option<&super::model::Command> { self.current.as_ref().map(|current| &current.command).or(self.command.as_ref()) }
}

pub struct Disposed {
    pub lease: super::admission::Lease,
    pub disposition: super::admission::Disposition,
}

/// What a session's inbox keeps, and reads at once (the defaults: [`KEEP`], [`TRIM`], [`READ`]).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub keep: u64,
    pub trim: u64,
    pub read: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { keep: KEEP, trim: TRIM, read: READ }
    }
}

/// What a sender is told when the inbox takes its message, the first time and every time after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "InboxAnswer"))]
pub struct Answer {
    pub session: String,
    pub message: String,
    /// The session's epoch when it was taken.
    pub epoch: u64,
    /// When it was taken (unix ms, its host's clock).
    pub at: i64,
    /// Its place: the revision its take was committed at here (a later message to the session has a
    /// later one).
    pub seq: u64,
    /// It had been taken before: this is the first time's answer, and the session got nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repeat: bool,
}

/// Why the inbox didn't take a message. Every one names the session, and an epoch where one
/// decided it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "refused", rename_all = "snake_case")]
pub enum Refusal {
    /// Sent for an epoch the session has left: it went on on another machine since. Sent again,
    /// it goes where the session is now (and its id still counts there).
    #[error("this was sent to session {session} at epoch {sent}, and it's at epoch {current} now: it moved since, so send it again")]
    StaleEpoch { session: String, sent: u64, current: u64 },
    /// Sent for an epoch this copy of the session hasn't reached: it's a copy left behind on a
    /// machine the session went on from.
    #[error("this copy of session {session} is at epoch {here}, behind the send's epoch {sent}: the session went on elsewhere")]
    Behind { session: String, here: u64, sent: u64 },
    /// The session went on on another machine (from epoch `epoch`), or is on its way there.
    #[error("session {session} went on on another machine from epoch {epoch}: send it again")]
    Moved { session: String, epoch: u64 },
    /// The id was given to another message to this session before.
    #[error("message id {message} was already used for another message to session {session}")]
    Reused { session: String, message: String },
    /// The session or the message id can't be kept as given.
    #[error("{what} isn't valid: {why}")]
    Invalid { what: String, why: String },
    /// The session's inbox holds its most waiting.
    #[error("session {session}'s inbox holds its most ({limit} waiting)")]
    Full { session: String, limit: usize },
    /// The session isn't here.
    #[error("no session {session} here")]
    NoSession { session: String },
    /// Its records can't be read or written now.
    #[error("session {session}'s inbox can't take it now: {why}")]
    Unavailable { session: String, why: String },
}

impl Refusal {
    /// Its kind, as metrics count it.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::StaleEpoch { .. } => "stale_epoch",
            Self::Behind { .. } => "behind",
            Self::Moved { .. } => "moved",
            Self::Reused { .. } => "reused",
            Self::Invalid { .. } => "invalid",
            Self::Full { .. } => "full",
            Self::NoSession { .. } => "no_session",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

/// What became of one message sent to a session: its answer, or why it was refused. It goes back to
/// whoever sent it (`super::core::Update::Inbox`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Said {
    pub message: String,
    pub answer: Result<Answer, Refusal>,
}

/// A session's head, as kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Head {
    epoch: u64,
    /// It went on on another machine: nothing more is taken for it here.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    moved: bool,
    /// Fenced as it was handed to this machine's next runner process (blue/green), at its epoch:
    /// a copy of it this host goes on with at that epoch (the handover never finished: this
    /// process was started again with it) takes it back. A move to another machine's fence isn't.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    handover: bool,
}

/// A head as this host last read or wrote it (none kept yet: `rev` is none), and how many messages'
/// ids the session keeps here, once counted.
#[derive(Debug, Clone)]
struct Cached {
    head: Head,
    rev: Option<Rev>,
    kept: Option<u64>,
}

/// A message taken, as kept. Its place is the revision of the row, and among ids brought in one
/// commit (which share it), `n`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kept {
    epoch: u64,
    at: i64,
    /// What it is ([`digest`]); empty for one brought from another machine, whose words aren't
    /// known here.
    #[serde(default)]
    digest: String,
    /// Its place among the ids brought in its commit ([`Inbox::import_kept`]), the oldest first: 0
    /// for one taken here, alone in its commit (CP-T3.2, the thirteenth adversarial check's).
    #[serde(default, skip_serializing_if = "is_first")]
    n: u32,
}

fn is_first(n: &u32) -> bool {
    *n == 0
}

/// Where a kept id stands among a session's: its row's revision, then its place among the ids
/// brought in that commit ([`Kept::n`]). The newest is the greatest.
type Place = (Rev, u32);

/// What a message is, for telling a repeat from another message under the same id: its words and
/// the names of the files it carries (what every hop and retry keeps as it was).
pub fn digest(text: &str, files: &[&str]) -> String {
    let mut whole = String::with_capacity(text.len().saturating_add(64));
    whole.push_str(text);
    for f in files {
        whole.push('\u{0}');
        whole.push_str(f);
    }
    let mut h = crate::util::sha256_hex(whole.as_bytes());
    h.truncate(32);
    h
}

/// The inboxes of the sessions on one host.
pub struct Inbox {
    records: Arc<dyn Records>,
    heads: Mutex<HashMap<String, Cached>>,
    limits: Limits,
    /// Ids sessions brought from where they were that aren't taken in yet ([`Inbox::bring`]): each
    /// session's, until they are, it refuses messages, its record keeps them and they go with it.
    /// Each session's newest [`Limits::keep`] at most (no more are taken in); a session's go when
    /// they're in, or when it leaves for good ([`Inbox::let_go`]).
    brought: Mutex<HashMap<String, Vec<(String, String)>>>,
}

/// How a commit under a session's head went, short of success.
enum Retry {
    /// The head changed under it: read it again and decide again.
    Again,
    /// One of the messages it would take was there already.
    Taken,
    No(Refusal),
}

impl Inbox {
    pub fn new(records: Arc<dyn Records>, limits: Limits) -> Self {
        Self { records, heads: Mutex::new(HashMap::new()), limits, brought: Mutex::new(HashMap::new()) }
    }

    /// Session `session` is here at `epoch`, with the ids of the messages it took where it was
    /// (`brought`, each with its digest where known): they're taken in first, then its inbox is
    /// opened (`back`: taken back at the epoch it left at, its fence lifted). Ids it can't take in
    /// now wait: it refuses messages (`unavailable`) until they're in, each message trying them again
    /// first, so a retry of one taken where it was is never taken here as new.
    pub async fn arrive(&self, session: &str, epoch: u64, brought: &[(String, String)], back: bool, at: i64) -> Result<(), Refusal> {
        check_session(session)?;
        self.bring(session, brought);
        let taken_in = self.take_in(session, at).await;
        let opened = if back { self.take_back(session, epoch).await } else { self.open(session, epoch).await };
        taken_in.and(opened.map(|_| ()))
    }

    /// Session `session` brought the ids of the messages it took where it was (`brought`, each with
    /// its digest where known), to take in before it takes a message ([`Inbox::arrive`]). Until
    /// they're in they're [`Inbox::waiting`]: its record keeps them, so a restart before they're in
    /// brings them again, and they go with it if it moves on ([`Inbox::carried`]).
    pub fn bring(&self, session: &str, brought: &[(String, String)]) {
        if brought.is_empty() {
            return;
        }
        let keep = usize::try_from(self.limits.keep).unwrap_or(usize::MAX);
        let mut all = self.brought.lock();
        let waiting = all.entry(session.to_owned()).or_default();
        waiting.extend(brought.iter().cloned());
        let past = waiting.len().saturating_sub(keep);
        waiting.drain(..past);
    }

    /// Session `session` left for good (went on elsewhere): what it brought and hadn't taken in went
    /// with it ([`Inbox::carried`]).
    pub fn let_go(&self, session: &str) {
        self.brought.lock().remove(session);
    }

    /// The ids session `session` brought that aren't taken in here yet ([`Inbox::bring`]).
    pub fn waiting(&self, session: &str) -> Vec<(String, String)> {
        self.brought.lock().get(session).cloned().unwrap_or_default()
    }

    /// The ids session `session` brought and couldn't take in yet, taken in now. A session's lane
    /// asks one thing at a time, so none comes between.
    async fn take_in(&self, session: &str, at: i64) -> Result<(), Refusal> {
        let waiting = self.brought.lock().get(session).cloned();
        let Some(ids) = waiting else { return Ok(()) };
        self.import_kept(session, &ids, at).await.map_err(|e| unavailable(session, &format!("the ids of the messages it took where it was aren't taken in here yet: {e}")))?;
        // Those taken in go; any brought meanwhile still wait.
        let taken: std::collections::HashSet<&(String, String)> = ids.iter().collect();
        let mut all = self.brought.lock();
        if let Some(left) = all.get_mut(session) {
            left.retain(|p| !taken.contains(p));
            if left.is_empty() {
                all.remove(session);
            }
        }
        Ok(())
    }

    /// The records it keeps its sessions' inboxes in.
    pub fn records(&self) -> &Arc<dyn Records> {
        &self.records
    }

    /// Session `session` is here at `epoch` (started, started again after a restart, or moved
    /// here): its inbox takes sends for that epoch from now on. An epoch never goes down, and only
    /// a higher one lifts a fence (it came back: a copy restored here at the epoch it left at is a
    /// copy left behind). The epoch it's at.
    pub async fn open(&self, session: &str, epoch: u64) -> Result<u64, Refusal> {
        check_session(session)?;
        for _ in 0..RETRIES {
            let c = self.head(session).await?;
            // Handed to this machine's next runner process at this epoch, and that never finished:
            // this host goes on with it, so it takes messages again.
            let handed_back = c.head.moved && c.head.handover && epoch == c.head.epoch;
            if c.rev.is_some() && epoch <= c.head.epoch && !handed_back {
                return Ok(c.head.epoch);
            }
            let head = Head { epoch: epoch.max(c.head.epoch), moved: false, handover: false };
            match self.commit(session, &c, Vec::new(), Vec::new(), Some(head.clone())).await {
                Ok(_) => return Ok(head.epoch),
                Err(Retry::Again | Retry::Taken) => continue,
                Err(Retry::No(r)) => return Err(r),
            }
        }
        Err(unavailable(session, "its head kept changing"))
    }

    /// Session `session` is taken back by this host at `epoch`, the epoch it left at: it was handed
    /// to another runner process of this machine (blue/green, which keeps its epoch) and that one
    /// never took it over, so this one goes on with it. A fence at `epoch` is lifted; one at a higher
    /// epoch stays (it went on elsewhere since, and this is a copy left behind). The epoch it's at.
    pub async fn take_back(&self, session: &str, epoch: u64) -> Result<u64, Refusal> {
        check_session(session)?;
        for _ in 0..RETRIES {
            let c = self.head(session).await?;
            if c.rev.is_some() && (epoch < c.head.epoch || (epoch == c.head.epoch && !c.head.moved)) {
                return Ok(c.head.epoch);
            }
            let head = Head { epoch: epoch.max(c.head.epoch), moved: false, handover: false };
            match self.commit(session, &c, Vec::new(), Vec::new(), Some(head.clone())).await {
                Ok(_) => return Ok(head.epoch),
                Err(Retry::Again | Retry::Taken) => continue,
                Err(Retry::No(r)) => return Err(r),
            }
        }
        Err(unavailable(session, "its head kept changing"))
    }

    /// Session `session` went on on another machine from `epoch`, the epoch it was at here (or its
    /// copy here was let go of): nothing more is taken for it here, each send refused `Moved`, until
    /// it comes back at a higher epoch. Fenced at the epoch it left at, even where its inbox here
    /// lagged behind it (opening it failed as it came): a copy of it restored here at that epoch
    /// (left behind) can't lift the fence.
    pub async fn fence(&self, session: &str, epoch: u64) -> Result<(), Refusal> {
        self.fence_for(session, epoch, false).await
    }

    /// [`Inbox::fence`]; `handover`: it's handed to this machine's next runner process
    /// (blue/green), and a copy of it this host goes on with at `epoch` takes it back (`open`).
    /// A fence for a move to another machine at the same epoch stays one.
    pub async fn fence_for(&self, session: &str, epoch: u64, handover: bool) -> Result<(), Refusal> {
        check_session(session)?;
        for _ in 0..RETRIES {
            let c = self.head(session).await?;
            if c.head.moved && (c.head.epoch > epoch || (c.head.epoch == epoch && (handover || !c.head.handover))) {
                return Ok(());
            }
            let head = Head { epoch: c.head.epoch.max(epoch), moved: true, handover: handover && c.head.epoch <= epoch };
            match self.commit(session, &c, Vec::new(), Vec::new(), Some(head)).await {
                Ok(_) => return Ok(()),
                Err(Retry::Again | Retry::Taken) => continue,
                Err(Retry::No(r)) => return Err(r),
            }
        }
        Err(unavailable(session, "its head kept changing"))
    }

    /// Takes message `message` for session `session`: its first time, `Ok` with its answer, and
    /// the session is to get it; every time after, the first time's answer marked a repeat, and
    /// the session gets nothing. `digest` ([`digest`]) tells a repeat from another message given the
    /// same id; `epoch` is the one it was sent for (none from a sender that doesn't say); `at` is
    /// the host's time (unix ms). A take is one write.
    pub async fn accept(&self, session: &str, message: &str, digest: &str, epoch: Option<u64>, at: i64) -> Result<Answer, Refusal> {
        self.accept_inner(session, message, digest, epoch, at, None, None).await
    }

    /// The original body and its admission lease commit with the inbox id, before any ACK or
    /// session handoff. Ordinary deduplication cleanup never touches this family.
    pub async fn accept_pending(&self, session: &str, message: &str, digest: &str, epoch: Option<u64>, at: i64, pending: &Pending) -> Result<Answer, Refusal> {
        self.accept_inner(session, message, digest, epoch, at, Some(accepting::PendingWrite::borrowed(pending)), None).await
    }

    /// The owner borrows its immutable original; the counted encoded transaction, rather than
    /// another Pending clone, takes temporary physical authority through the Records worker.
    #[allow(clippy::too_many_arguments)] // Existing inbox identity/epoch/time plus the borrowed immutable body and its physical lease.
    pub async fn accept_input(&self, session: &str, message: &str, digest: &str, epoch: u64, at: i64, lease: &super::admission::Lease, command: &super::model::Command, body_bytes: usize, budget: &super::admission::Input) -> Result<Answer, Refusal> {
        self.accept_inner(session, message, digest, Some(epoch), at, Some(accepting::PendingWrite::original(lease, command, body_bytes)), Some(budget)).await
    }

    #[allow(clippy::too_many_arguments)] // Common legacy/durable acceptance path preserves independently validated protocol fields.
    async fn accept_inner(&self, session: &str, message: &str, digest: &str, epoch: Option<u64>, at: i64, pending: Option<accepting::PendingWrite<'_>>, budget: Option<&super::admission::Input>) -> Result<Answer, Refusal> {
        let storage_failed = |refusal: Refusal| {
            if let Some(input) = budget { let _ = input.controller.lock().unavailable(); }
            refusal
        };
        check_session(session)?;
        // What it brought from where it was first: a retry of one of those is its repeat here.
        self.take_in(session, at).await.map_err(storage_failed)?;
        check_message(message)?;
        for _ in 0..RETRIES {
            let c = self.head(session).await.map_err(storage_failed)?;
            if c.head.moved {
                return Err(Refusal::Moved { session: session.to_owned(), epoch: c.head.epoch });
            }
            match epoch {
                Some(sent) if sent < c.head.epoch => return Err(Refusal::StaleEpoch { session: session.to_owned(), sent, current: c.head.epoch }),
                Some(sent) if sent > c.head.epoch => return Err(Refusal::Behind { session: session.to_owned(), here: c.head.epoch, sent }),
                _ => {}
            }
            // Pending custody outlives bounded deduplication history. It still answers the
            // original message after an ordinary tidy removed its historical id.
            if let Some(row) = self.records.get(PENDING, &message_key(session, message), Consistency::Linearizable).await.map_err(|e| storage_failed(unavailable(session, &e.to_string())))? {
                let original: Pending = serde_json::from_slice(&row.value).map_err(|e| storage_failed(unavailable(session, &format!("its pending communication is unreadable: {e}"))))?;
                if original.digest != digest { return Err(Refusal::Reused { session: session.to_owned(), message: message.to_owned() }); }
                return Ok(Answer { session: session.to_owned(), message: message.to_owned(), epoch: original.lease.epoch, at: original.accepted_at, seq: original.accepted_seq.unwrap_or(row.rev), repeat: true });
            }
            let built = accepting::build(session, message, digest, at, &c, pending, budget)?;
            let result = match built.owner {
                Some(owner) => self.records.commit_owned(built.tx, owner).await,
                None => self.records.commit(built.tx).await,
            };
            if matches!(&result, Err(records::Refusal::Unavailable(_))) && let Some(input) = budget { let _ = input.controller.lock().unavailable(); }
            match self.committed(session, &c, built.head, result) {
                Ok(rev) => {
                    self.counted(session, 1);
                    return Ok(Answer { session: session.to_owned(), message: message.to_owned(), epoch: c.head.epoch, at, seq: rev, repeat: false });
                }
                Err(Retry::Again) => continue,
                Err(Retry::Taken) => {
                    // Taken before: the first time's answer, if it's the same message.
                    let (first, rev) = self.kept(session, message).await?.ok_or_else(|| unavailable(session, "a message taken before can't be read"))?;
                    // One brought from another machine has no digest here: whatever comes under
                    // its id is its repeat.
                    if !first.digest.is_empty() && first.digest != digest {
                        return Err(Refusal::Reused { session: session.to_owned(), message: message.to_owned() });
                    }
                    return Ok(Answer { session: session.to_owned(), message: message.to_owned(), epoch: first.epoch, at: first.at, seq: rev, repeat: true });
                }
                Err(Retry::No(r)) => return Err(r),
            }
        }
        Err(unavailable(session, "its head kept changing"))
    }

    /// What to tell the sender of message `message`, sent for `epoch`, that this machine couldn't
    /// read (`crate::wire::payload`), its id read: taken before, it's that message's repeat, the
    /// first time's answer, as [`Inbox::accept`] answers one (its words can't be compared, so it's
    /// taken for the same message); sent for an epoch the session isn't at here, refused as
    /// `accept` refuses it, so it's sent where the session is; never taken, refused for good
    /// (`invalid`: `why`), as nothing took it. Its records unreadable now: `unavailable`.
    pub async fn unreadable(&self, session: &str, message: &str, epoch: Option<u64>, why: &str, at: i64) -> Result<Answer, Refusal> {
        check_session(session)?;
        self.take_in(session, at).await?;
        check_message(message)?;
        let c = self.head(session).await?;
        if c.head.moved {
            return Err(Refusal::Moved { session: session.to_owned(), epoch: c.head.epoch });
        }
        match epoch {
            Some(sent) if sent < c.head.epoch => return Err(Refusal::StaleEpoch { session: session.to_owned(), sent, current: c.head.epoch }),
            Some(sent) if sent > c.head.epoch => return Err(Refusal::Behind { session: session.to_owned(), here: c.head.epoch, sent }),
            _ => {}
        }
        match self.kept(session, message).await? {
            Some((first, rev)) => Ok(Answer { session: session.to_owned(), message: message.to_owned(), epoch: first.epoch, at: first.at, seq: rev, repeat: true }),
            None => Err(Refusal::Invalid { what: "message".to_owned(), why: format!("its runner couldn't read it: {why}") }),
        }
    }

    /// Message `message`, taken, never reached its session after all (it was on its way to another
    /// machine, or ended): it's forgotten, so a retry of it goes where the session is.
    pub async fn unaccept(&self, session: &str, message: &str) -> Result<(), Refusal> {
        check_session(session)?;
        check_message(message)?;
        if self.pending(session, message).await?.is_some() {
            return Err(unavailable(session, "its accepted communication remains in durable custody"));
        }
        let tx = Tx { conds: Vec::new(), writes: vec![Write::Delete { family: MESSAGES.into(), key: message_key(session, message) }] };
        self.records.commit(tx).await.map_err(|e| unavailable(session, &e.to_string()))?;
        self.counted(session, -1);
        Ok(())
    }

    pub async fn pending(&self, session: &str, message: &str) -> Result<Option<Pending>, Refusal> {
        check_session(session)?;
        check_message(message)?;
        let row = self.records.get(PENDING, &message_key(session, message), Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))?;
        row.map(|row| serde_json::from_slice(&row.value).map_err(|e| unavailable(session, &format!("its pending communication is unreadable: {e}")))).transpose()
    }

    /// Upgrade reconciliation of a complete retained legacy queue body. A prior journal wins;
    /// old dedup IDs cannot suppress insertion of the missing replayable body. No mail token or
    /// original-publication attestation is invented for this recovered current queue value.
    pub async fn adopt_pending(&self, session: &str, mut pending: Pending, at: i64) -> Result<Pending, Refusal> {
        check_session(session)?; check_message(&pending.lease.message)?;
        let message = pending.lease.message.clone();
        if let Some(existing) = self.pending(session, &message).await? { return Ok(existing); }
        let cached = self.head(session).await?;
        if cached.head.moved { return Err(Refusal::Moved { session: session.to_owned(), epoch: cached.head.epoch }); }
        if cached.head.epoch != pending.lease.epoch { return Err(Refusal::StaleEpoch { session: session.to_owned(), sent: pending.lease.epoch, current: cached.head.epoch }); }
        let kept = self.kept(session, &message).await?;
        if pending.origin != Origin::LegacyOpaque { pending.origin = Origin::LegacyRetained; }
        pending.lease.token = None;
        match kept {
            Some((kept, revision)) => { pending.accepted_at = kept.at; pending.digest = kept.digest; pending.accepted_seq = Some(revision); }
            None => {
                pending.accepted_at = at;
                pending.digest = match &pending.command {
                    Some(super::model::Command::Send { text, attachments, .. }) => digest(text, &attachments.iter().map(|attachment| attachment.name.as_str()).collect::<Vec<_>>()),
                    None if pending.origin == Origin::LegacyOpaque => String::new(),
                    _ => return Err(unavailable(session, "legacy queue custody requires its complete retained communication")),
                };
            }
        }
        let key = message_key(session, &message);
        let head = cached.rev.is_none().then(|| cached.head.clone());
        match self.commit(session, &cached, vec![Cond::Absent { family: PENDING.into(), key: key.clone() }], vec![Write::Put { family: PENDING.into(), key, value: json(session, &pending)? }], head).await {
            Ok(revision) => { pending.accepted_seq = Some(pending.accepted_seq.unwrap_or(revision)); Ok(pending) }
            Err(Retry::Again | Retry::Taken) => self.pending(session, &message).await?.ok_or_else(|| unavailable(session, "legacy queue custody changed during reconciliation")),
            Err(Retry::No(error)) => Err(error),
        }
    }

    /// Current body is durable before the owner mutates an accepted queue item. The accepted
    /// original, receipt digest/token and order remain unchanged. CAS failure retains the old row.
    pub async fn edit_pending(&self, session: &str, message: &str, lease: super::admission::Lease, command: super::model::Command) -> Result<u64, Refusal> {
        check_session(session)?; check_message(message)?;
        if !matches!(&command, super::model::Command::Send { id, .. } if id == message) || lease.message != message { return Err(unavailable(session, "an edit cannot replace communication identity")); }
        let key = message_key(session, message);
        let row = self.records.get(PENDING, &key, Consistency::Linearizable).await.map_err(|error| unavailable(session, &error.to_string()))?.ok_or_else(|| unavailable(session, "the edited communication has no durable original"))?;
        let mut pending: Pending = serde_json::from_slice(&row.value).map_err(|_| unavailable(session, "the edited communication is unreadable"))?;
        if pending.lease.epoch != lease.epoch || pending.lease.token != lease.token || pending.disposition.is_some() { return Err(unavailable(session, "the edited communication changed custody")); }
        let revision = pending.current.as_ref().map_or(0, |current| current.revision).checked_add(1).ok_or_else(|| unavailable(session, "the edited communication revision overflowed"))?;
        pending.body_bytes = Some(super::admission::command_bytes(&command).map_err(|_| unavailable(session, "the edited communication's resident size is unknown"))?);
        pending.current = Some(Current { revision, command });
        pending.lease = lease;
        pending.accepted_seq = Some(pending.accepted_seq.unwrap_or(row.rev));
        self.records.commit(Tx { conds: vec![Cond::At { family: PENDING.into(), key: key.clone(), rev: row.rev }], writes: vec![Write::Put { family: PENDING.into(), key, value: json(session, &pending)? }] }).await.map_err(|error| unavailable(session, &error.to_string()))?;
        Ok(revision)
    }

    /// Metadata registration leaves queue/body/lease custody intact. A real native tool result
    /// must later prove this exact handoff before any member is consumed or removed.
    pub async fn register_handoff(&self, session: &str, leases: &[super::admission::Lease]) -> Result<(), Refusal> {
        check_session(session)?;
        if leases.len() > records::TX_MAX { return Err(unavailable(session, "handoff exceeds transaction capacity")); }
        let mut tx = Tx::default();
        for lease in leases {
            check_message(&lease.message)?;
            let key = message_key(session, &lease.message);
            let row = self.records.get(PENDING, &key, Consistency::Linearizable).await.map_err(|_| unavailable(session, "handoff custody is unavailable"))?.ok_or_else(|| unavailable(session, "handoff has no replayable original"))?;
            let mut pending: Pending = serde_json::from_slice(&row.value).map_err(|_| unavailable(session, "handoff custody is unreadable"))?;
            if pending.disposition.is_some() || pending.lease.epoch != lease.epoch || pending.lease.token != lease.token || pending.lease.handoff.as_ref().is_some_and(|old| Some(old) != lease.handoff.as_ref()) { return Err(unavailable(session, "handoff custody changed")); }
            pending.lease = lease.clone();
            pending.accepted_seq = Some(pending.accepted_seq.unwrap_or(row.rev));
            tx.conds.push(Cond::At { family: PENDING.into(), key: key.clone(), rev: row.rev });
            tx.writes.push(Write::Put { family: PENDING.into(), key, value: json(session, &pending)? });
        }
        if !tx.writes.is_empty() { self.records.commit(tx).await.map_err(|_| unavailable(session, "handoff metadata could not be committed"))?; }
        Ok(())
    }

    /// Provider-flight intent is committed before the session flush writes that input. Thus an
    /// accepted record without this stage is safely replayable after a crash; flight is unknown.
    pub async fn stage_pending(&self, session: &str, message: &str, epoch: u64, stage: super::admission::Stage) -> Result<bool, Refusal> {
        check_session(session)?;
        check_message(message)?;
        let key = message_key(session, message);
        let Some(row) = self.records.get(PENDING, &key, Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))? else { return Ok(false) };
        let mut pending: Pending = serde_json::from_slice(&row.value).map_err(|e| unavailable(session, &format!("its pending communication is unreadable: {e}")))?;
        if pending.lease.epoch != epoch { return Err(Refusal::StaleEpoch { session: session.to_owned(), sent: epoch, current: pending.lease.epoch }); }
        if pending.disposition.is_some() || pending.lease.stage == stage { return Ok(true); }
        pending.accepted_seq = Some(pending.accepted_seq.unwrap_or(row.rev));
        pending.lease.stage = stage;
        self.records.commit(Tx { conds: vec![Cond::At { family: PENDING.into(), key: key.clone(), rev: row.rev }], writes: vec![Write::Put { family: PENDING.into(), key, value: json(session, &pending)? }] }).await.map_err(|e| unavailable(session, &e.to_string()))?;
        Ok(true)
    }

    /// Restore/replay one atomic communication at a time; the journal has no deduplication cap.
    pub async fn pending_next(&self, session: &str, after: Option<Vec<u8>>) -> Result<Option<(Vec<u8>, Pending)>, Refusal> {
        check_session(session)?;
        let page = self.records.scan(PENDING, Scan { prefix: session_prefix(session), after, limit: 1, at: Consistency::Linearizable }).await.map_err(|e| unavailable(session, &e.to_string()))?;
        let Some(row) = page.rows.into_iter().next() else { return Ok(None) };
        let pending = serde_json::from_slice(&row.value).map_err(|e| unavailable(session, &format!("its pending communication is unreadable: {e}")))?;
        Ok(Some((row.key, pending)))
    }

    /// Persist the exact terminal cause before publishing its receipt. Failure leaves the
    /// original body and lease pending, rather than releasing credit on uncertain persistence.
    pub async fn dispose_pending(&self, session: &str, message: &str, epoch: u64, disposition: super::admission::Disposition) -> Result<Option<Pending>, Refusal> {
        check_session(session)?;
        check_message(message)?;
        let key = message_key(session, message);
        let Some(row) = self.records.get(PENDING, &key, Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))? else { return Ok(None) };
        let mut pending: Pending = serde_json::from_slice(&row.value).map_err(|e| unavailable(session, &format!("its pending communication is unreadable: {e}")))?;
        if pending.lease.epoch != epoch { return Err(Refusal::StaleEpoch { session: session.to_owned(), sent: epoch, current: pending.lease.epoch }); }
        if pending.disposition.is_none() {
            pending.disposition = Some(disposition);
            pending.accepted_seq = Some(pending.accepted_seq.unwrap_or(row.rev));
            self.records.commit(Tx { conds: vec![Cond::At { family: PENDING.into(), key: key.clone(), rev: row.rev }], writes: vec![Write::Put { family: PENDING.into(), key, value: json(session, &pending)? }] }).await.map_err(|e| unavailable(session, &e.to_string()))?;
        }
        Ok(Some(pending))
    }

    /// A real handoff batch has one durable transition before any member returns credit or
    /// publishes a receipt. The Records transaction limit is protocol authority, not a chosen
    /// queue depth; an unsupported larger batch remains retained and visibly unavailable.
    pub async fn dispose_pending_batch(&self, session: &str, messages: &[String], epoch: u64, disposition: &super::admission::Disposition) -> Result<Vec<Disposed>, Refusal> {
        check_session(session)?;
        if messages.len() > records::TX_MAX { return Err(unavailable(session, "one observed handoff batch exceeds the records transaction capacity")); }
        let mut tx = Tx::default();
        let mut disposed = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for message in messages {
            check_message(message)?;
            if !seen.insert(message.as_str()) { continue; }
            let key = message_key(session, message);
            let row = self.records.get(PENDING, &key, Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))?.ok_or_else(|| unavailable(session, "an observed handoff has no replayable custody record"))?;
            let mut pending: Pending = serde_json::from_slice(&row.value).map_err(|e| unavailable(session, &format!("its pending communication is unreadable: {e}")))?;
            if pending.lease.epoch != epoch { return Err(Refusal::StaleEpoch { session: session.to_owned(), sent: epoch, current: pending.lease.epoch }); }
            if pending.disposition.is_none() {
                pending.disposition = Some(disposition.clone());
                pending.accepted_seq = Some(pending.accepted_seq.unwrap_or(row.rev));
                tx.conds.push(Cond::At { family: PENDING.into(), key: key.clone(), rev: row.rev });
                tx.writes.push(Write::Put { family: PENDING.into(), key, value: json(session, &pending)? });
            }
            let actual = pending.disposition.ok_or_else(|| unavailable(session, "its handoff disposition could not be retained"))?;
            disposed.push(Disposed { lease: pending.lease, disposition: actual });
        }
        if !tx.writes.is_empty() { self.records.commit(tx).await.map_err(|e| unavailable(session, &e.to_string()))?; }
        Ok(disposed)
    }

    /// Only a durable server acknowledgement may discard a terminal custody record. A stale
    /// publication/attempt cannot delete a newer record under the same ordinary message id.
    pub async fn acknowledge_pending(&self, session: &str, message: &str, epoch: u64, token: &super::mail::Token, disposition: Option<&super::admission::Disposition>) -> Result<bool, Refusal> {
        check_session(session)?;
        check_message(message)?;
        let Some(disposition) = disposition else { return Ok(false) };
        let key = message_key(session, message);
        let Some(row) = self.records.get(PENDING, &key, Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))? else { return Ok(false) };
        let pending: Pending = serde_json::from_slice(&row.value).map_err(|e| unavailable(session, &format!("its pending communication is unreadable: {e}")))?;
        if pending.lease.epoch != epoch || pending.lease.token.as_ref() != Some(token) || pending.disposition.as_ref() != Some(disposition) { return Ok(false); }
        self.records.commit(Tx { conds: vec![Cond::At { family: PENDING.into(), key: key.clone(), rev: row.rev }], writes: vec![Write::Delete { family: PENDING.into(), key }] }).await.map_err(|e| unavailable(session, &e.to_string()))?;
        Ok(true)
    }

    /// Ids session `session` had before it came here (the messages in its queue and its timeline,
    /// from the machine it moved from), taken as its own: a repeat of one is answered, never
    /// delivered. At most [`Limits::keep`], the last of them. How many were new here.
    pub async fn import(&self, session: &str, ids: &[String], at: i64) -> Result<usize, Refusal> {
        let kept: Vec<(String, String)> = ids.iter().map(|id| (id.clone(), String::new())).collect();
        self.import_kept(session, &kept, at).await
    }

    /// [`Inbox::import`], each id with its digest where the inbox it came from kept one
    /// ([`Inbox::carried`]): another message under it is refused `reused` here, as there. Empty
    /// where its words aren't known (an id from its timeline): whatever comes under it is its
    /// repeat.
    pub async fn import_kept(&self, session: &str, ids: &[(String, String)], at: i64) -> Result<usize, Refusal> {
        check_session(session)?;
        let keep = usize::try_from(self.limits.keep).unwrap_or(usize::MAX);
        // Each id once, at its newest place in the list (the newest last), and the newest `keep`
        // of them, the oldest of those first: an id listed twice never takes another's room.
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut newest: Vec<&(String, String)> = Vec::new();
        for pair in ids.iter().rev() {
            if newest.len() >= keep {
                break;
            }
            if check_message(&pair.0).is_ok() && seen.insert(pair.0.as_str()) {
                newest.push(pair);
            }
        }
        newest.reverse();
        let ids = newest;
        let mut new = 0usize;
        // A transaction at a time, each within its most writes.
        for chunk in ids.chunks(IMPORT_CHUNK) {
            for _ in 0..RETRIES {
                let c = self.head(session).await?;
                let mut writes = Vec::new();
                let mut conds = Vec::new();
                for (j, (id, digest)) in chunk.iter().map(|p| (p.0.as_str(), p.1.as_str())).enumerate() {
                    if self.kept(session, id).await?.is_some() {
                        continue;
                    }
                    // Its place among those brought with it, the oldest first: they share a revision.
                    let kept = Kept { epoch: c.head.epoch, at, digest: digest.to_owned(), n: u32::try_from(j).unwrap_or(u32::MAX) };
                    conds.push(Cond::Absent { family: MESSAGES.into(), key: message_key(session, id) });
                    writes.push(Write::Put { family: MESSAGES.into(), key: message_key(session, id), value: json(session, &kept)? });
                }
                if writes.is_empty() {
                    break;
                }
                let added = writes.len();
                let head = c.rev.is_none().then(|| c.head.clone());
                match self.commit(session, &c, conds, writes, head).await {
                    Ok(_) => {
                        new = new.saturating_add(added);
                        self.counted(session, i64::try_from(added).unwrap_or(i64::MAX));
                        break;
                    }
                    // A take came between: what's left is looked at again.
                    Err(Retry::Again | Retry::Taken) => continue,
                    Err(Retry::No(r)) => return Err(r),
                }
            }
        }
        Ok(new)
    }

    /// Session `session` ended for good: what its inbox kept goes, and its head.
    pub async fn forget(&self, session: &str) -> Result<(), Refusal> {
        check_session(session)?;
        // Bounded: each round takes away a page, and a session keeps at most [`Limits::keep`] and
        // [`Limits::trim`] ids.
        let rounds = self.limits.keep.saturating_add(self.limits.trim).checked_div(u64::try_from(PAGE).unwrap_or(1)).unwrap_or(0).saturating_add(2);
        for _ in 0..rounds {
            let page = self.page(session, None, PAGE).await?;
            if page.is_empty() {
                break;
            }
            let writes = page.into_iter().map(|(key, _, _, _)| Write::Delete { family: MESSAGES.into(), key }).collect();
            self.records.commit(Tx { conds: Vec::new(), writes }).await.map_err(|e| unavailable(session, &e.to_string()))?;
        }
        self.records.commit(Tx { conds: Vec::new(), writes: vec![Write::Delete { family: HEADS.into(), key: session.as_bytes().to_vec() }] }).await.map_err(|e| unavailable(session, &e.to_string()))?;
        self.heads.lock().remove(session);
        self.brought.lock().remove(session);
        Ok(())
    }

    /// The newest `limit` ids session `session` took, the oldest of them first: what goes with it
    /// when it moves (the likeliest to be sent again), and what reached it for kiln-seams'
    /// HarnessPort suite.
    pub async fn taken(&self, session: &str, limit: usize) -> Result<Vec<String>, Refusal> {
        check_session(session)?;
        Ok(self.carried(session, limit).await?.into_iter().map(|(m, _)| m).collect())
    }

    /// [`Inbox::taken`], each id with its digest (empty where its words aren't known here): what
    /// goes with a session that moves, so another message under one of its ids is refused
    /// `reused` where it goes, as here. Those it brought and hasn't taken in yet
    /// ([`Inbox::waiting`]) go too, as its newest (it takes nothing here while they wait).
    pub async fn carried(&self, session: &str, limit: usize) -> Result<Vec<(String, String)>, Refusal> {
        check_session(session)?;
        let (mut all, whole) = self.read(session).await?;
        // Its newest may be among those not read: it goes once a tidy brings it back under.
        if !whole {
            return Err(unavailable(session, &format!("it keeps more ids than the {} read at once (its tidies failing): it goes once they're tidied", self.limits.read)));
        }
        all.sort_by_key(|(_, place, _, _)| *place);
        let ids = all.into_iter().map(|(_, _, m, d)| (m, d)).chain(self.waiting(session));
        // Each id once, at its newest place (one taken in part-way may be in both).
        let mut seen = std::collections::HashSet::new();
        let mut newest: Vec<(String, String)> = ids.rev().filter(|(m, _)| seen.insert(m.clone())).take(limit).collect();
        newest.reverse();
        Ok(newest)
    }

    /// The epoch session `session`'s inbox is at, and whether it went on elsewhere: none when it
    /// has no head here.
    pub async fn epoch(&self, session: &str) -> Result<Option<(u64, bool)>, Refusal> {
        check_session(session)?;
        let c = self.head(session).await?;
        Ok(c.rev.map(|_| (c.head.epoch, c.head.moved)))
    }

    /// `by` more (or fewer) ids kept for session `session`; once it keeps [`Limits::keep`] and
    /// [`Limits::trim`] more, the oldest go. A failure to let them go leaves them for later.
    fn counted(&self, session: &str, by: i64) {
        let mut heads = self.heads.lock();
        if let Some(k) = heads.get_mut(session).and_then(|c| c.kept.as_mut()) {
            *k = k.saturating_add_signed(by);
        }
    }

    /// Session `session`'s inbox past its bound ([`Limits::keep`] and [`Limits::trim`] more): its
    /// oldest ids go, down to its newest [`Limits::keep`], in commits of their own. Not on a take's
    /// way: the host tidies once the take's answer has gone. One that fails is tried again after
    /// the next take. How many went.
    pub async fn tidy(&self, session: &str) -> usize {
        let bound = self.limits.keep.saturating_add(self.limits.trim);
        // Not in memory now: its next take reads its head again, and tidies after.
        let Some(kept) = self.heads.lock().get(session).map(|c| c.kept) else { return 0 };
        let over = match kept {
            Some(k) => k >= bound,
            // Not counted yet in this process (its head read since it started): counted now, once.
            // More than are read at once is past its bound.
            None => match self.read(session).await {
                Ok((all, whole)) => {
                    let n = u64::try_from(all.len()).unwrap_or(u64::MAX);
                    if whole
                        && let Some(c) = self.heads.lock().get_mut(session)
                        && c.kept.is_none()
                    {
                        c.kept = Some(n);
                    }
                    !whole || n >= bound
                }
                Err(_) => return 0,
            },
        };
        if !over {
            return 0;
        }
        self.trim(session).await.unwrap_or_else(|e| {
            tracing::warn!(target: "kiln.sends", event = "trim_failed", %session, error = %e, "an inbox's oldest ids couldn't go; they go after a later message");
            0
        })
    }

    /// Lets the oldest ids of session `session` go, down to the newest [`Limits::keep`] of those
    /// read ([`Inbox::read`]): each that goes has that many newer than it among them, so it's never
    /// among the session's newest, whatever wasn't read (past what's read at once, its tidies having
    /// failed, the rest go at its next tidies). How many went.
    async fn trim(&self, session: &str) -> Result<usize, Refusal> {
        let (read, whole) = self.read(session).await?;
        let mut every: Vec<(Vec<u8>, Place)> = read.into_iter().map(|(key, place, _, _)| (key, place)).collect();
        every.sort_by_key(|(_, place)| *place);
        let keep = usize::try_from(self.limits.keep).unwrap_or(usize::MAX);
        let past = every.len().saturating_sub(keep);
        let mut gone = 0usize;
        for chunk in every.get(..past).unwrap_or_default().chunks(PAGE) {
            let writes: Vec<Write> = chunk.iter().map(|(key, _)| Write::Delete { family: MESSAGES.into(), key: key.clone() }).collect();
            self.records.commit(Tx { conds: Vec::new(), writes }).await.map_err(|e| unavailable(session, &e.to_string()))?;
            gone = gone.saturating_add(chunk.len());
        }
        if let Some(c) = self.heads.lock().get_mut(session) {
            // Counted when every one was read; else counted again at its next tidy.
            c.kept = whole.then(|| u64::try_from(every.len().saturating_sub(gone)).unwrap_or(u64::MAX));
        }
        Ok(gone)
    }

    /// The messages session `session` keeps, [`Limits::read`] at most: each one's key, the revision
    /// it was taken at, its id and digest, a page at a time in key order; and whether that's every
    /// one (past its bound, its tidies failing, it may keep more than [`Limits::keep`] and
    /// [`Limits::trim`], and its newest must be among what's read for what goes with it when it
    /// moves, whatever their keys' order).
    async fn read(&self, session: &str) -> Result<(Vec<(Vec<u8>, Place, String, String)>, bool), Refusal> {
        let most = usize::try_from(self.limits.read).unwrap_or(usize::MAX).max(1);
        let mut out: Vec<(Vec<u8>, Place, String, String)> = Vec::new();
        let mut after = None;
        // A page reads one at least: `most` pages at the most.
        for _ in 0..most {
            let want = most.saturating_sub(out.len()).min(records::SCAN_MAX);
            let page = self.page(session, after.take(), want).await?;
            let short = page.len() < want;
            after = page.last().map(|(k, _, _, _)| k.clone());
            out.extend(page);
            if short {
                return Ok((out, true));
            }
            if out.len() >= most {
                // As many as are read at once: every one only if none follows.
                let next = self.page(session, after, 1).await?;
                return Ok((out, next.is_empty()));
            }
        }
        Ok((out, false))
    }

    /// A page of session `session`'s messages in key order, after `after`: each key, place
    /// ([`Place`]), id and digest (empty where its words aren't known here).
    async fn page(&self, session: &str, after: Option<Vec<u8>>, limit: usize) -> Result<Vec<(Vec<u8>, Place, String, String)>, Refusal> {
        let prefix = session_prefix(session);
        let skip = prefix.len();
        let scan = Scan { prefix, after, limit: limit.clamp(1, records::SCAN_MAX), at: Consistency::Linearizable };
        let page = self.records.scan(MESSAGES, scan).await.map_err(|e| unavailable(session, &e.to_string()))?;
        Ok(page
            .rows
            .into_iter()
            .map(|r| {
                let (digest, n) = serde_json::from_slice::<Kept>(&r.value).map(|k| (k.digest, k.n)).unwrap_or_default();
                (r.key.clone(), (r.rev, n), String::from_utf8_lossy(r.key.get(skip..).unwrap_or_default()).into_owned(), digest)
            })
            .collect())
    }

    /// Message `message` as kept for session `session`, and the revision it was taken at.
    async fn kept(&self, session: &str, message: &str) -> Result<Option<(Kept, Rev)>, Refusal> {
        let row = self.records.get(MESSAGES, &message_key(session, message), Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))?;
        row.map(|r| serde_json::from_slice::<Kept>(&r.value).map(|k| (k, r.rev)).map_err(|e| unavailable(session, &format!("a kept message is unreadable: {e}")))).transpose()
    }

    /// Session `session`'s head: as this host has it, else as kept, else a new one.
    async fn head(&self, session: &str) -> Result<Cached, Refusal> {
        if let Some(c) = self.heads.lock().get(session) {
            return Ok(c.clone());
        }
        let row = self.records.get(HEADS, session.as_bytes(), Consistency::Linearizable).await.map_err(|e| unavailable(session, &e.to_string()))?;
        let (head, rev) = match row {
            Some(r) => (serde_json::from_slice(&r.value).map_err(|e| unavailable(session, &format!("its head is unreadable: {e}")))?, Some(r.rev)),
            None => (Head::default(), None),
        };
        // How many ids it keeps isn't counted here, on a take's way: its tidy counts them, once
        // its answer has gone (CP-T3.2).
        let c = Cached { head, rev, kept: None };
        self.cache(session, c.clone());
        Ok(c)
    }

    fn cache(&self, session: &str, c: Cached) {
        let mut heads = self.heads.lock();
        // Full: one goes (any: read again as needed), not all at once.
        if heads.len() >= HEADS_MAX
            && !heads.contains_key(session)
            && let Some(other) = heads.keys().next().cloned()
        {
            heads.remove(&other);
        }
        heads.insert(session.to_owned(), c);
    }

    /// Commits `writes` under `conds` for session `session` if its head is still `was`, writing
    /// `head` with them when it's given: its revision; `Again` when the head changed (it's read
    /// again), `Taken` when a message it puts was there.
    async fn commit(&self, session: &str, was: &Cached, mut conds: Vec<Cond>, mut writes: Vec<Write>, head: Option<Head>) -> Result<Rev, Retry> {
        let key = session.as_bytes().to_vec();
        conds.insert(0, match was.rev {
            Some(rev) => Cond::At { family: HEADS.into(), key: key.clone(), rev },
            None => Cond::Absent { family: HEADS.into(), key: key.clone() },
        });
        if let Some(h) = &head {
            writes.push(Write::Put { family: HEADS.into(), key, value: json(session, h).map_err(Retry::No)? });
        }
        let result = self.records.commit(Tx { conds, writes }).await;
        self.committed(session, was, head, result)
    }

    fn committed(&self, session: &str, was: &Cached, head: Option<Head>, result: Result<Rev, records::Refusal>) -> Result<Rev, Retry> {
        match result {
            Ok(rev) => {
                if let Some(h) = head {
                    self.cache(session, Cached { head: h, rev: Some(rev), kept: was.kept });
                }
                Ok(rev)
            }
            Err(records::Refusal::Conflict { family, .. }) if family == HEADS => {
                self.heads.lock().remove(session);
                Err(Retry::Again)
            }
            Err(records::Refusal::Conflict { family, .. }) if family == MESSAGES => Err(Retry::Taken),
            Err(e) => Err(Retry::No(unavailable(session, &e.to_string()))),
        }
    }
}

fn unavailable(session: &str, why: &str) -> Refusal {
    Refusal::Unavailable { session: session.to_owned(), why: why.to_owned() }
}

fn json<T: Serialize>(session: &str, v: &T) -> Result<Vec<u8>, Refusal> {
    serde_json::to_vec(v).map_err(|e| unavailable(session, &e.to_string()))
}

/// A session id as hosts take them: 1 to 64 of A-Z, a-z, 0-9, `-` and `_`.
fn check_session(session: &str) -> Result<(), Refusal> {
    let ok = !session.is_empty() && session.len() <= 64 && session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok { Ok(()) } else { Err(Refusal::Invalid { what: "a session id".into(), why: format!("{session:?}: 1 to 64 of A-Z, a-z, 0-9, - and _") }) }
}

/// A message id: 1 to [`ID_MAX`] bytes, none of them a control character.
fn check_message(message: &str) -> Result<(), Refusal> {
    if message.is_empty() || message.len() > ID_MAX || message.chars().any(char::is_control) {
        return Err(Refusal::Invalid { what: "a message id".into(), why: format!("{message:?}: 1 to {ID_MAX} bytes, no control characters") });
    }
    Ok(())
}

fn session_prefix(session: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(session.len().saturating_add(1));
    k.extend_from_slice(session.as_bytes());
    k.push(b'/');
    k
}

fn message_key(session: &str, message: &str) -> Vec<u8> {
    let mut k = session_prefix(session);
    k.extend_from_slice(message.as_bytes());
    k
}

#[cfg(test)]
mod sim;

#[cfg(test)]
mod faults;

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::records::SimRecords;

    fn block<T>(f: impl std::future::Future<Output = T>) -> T {
        futures::executor::block_on(f)
    }

    fn inbox() -> Inbox {
        Inbox::new(Arc::new(SimRecords::new()), Limits::default())
    }

    fn pending_message(message: &str, text: &str) -> Pending {
        use crate::harness::{admission::{Lease, Stage}, mail::Token, model::{Command, SendMode}};
        Pending::new(Lease { message: message.into(), peer: "peer".into(), epoch: 0, bytes: text.len(), stage: Stage::InboxAccepted, token: Some(Token { revision: 1, generation: 1 }), handoff: None }, Command::Send { id: message.into(), text: text.into(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None })
    }

    #[test]
    fn an_ack_has_atomic_body_custody_and_unaccept_cannot_erase_it() -> Result<(), Refusal> {
        let i = inbox();
        let pending = pending_message("journalled", "retain these words");
        let answer = block(i.accept_pending("s1", "journalled", &digest("retain these words", &[]), Some(0), 10, &pending))?;
        assert!(!answer.repeat);
        let saved = block(i.pending("s1", "journalled"))?.ok_or_else(|| unavailable("s1", "test journal missing"))?;
        assert!(matches!(saved.command, Some(crate::harness::model::Command::Send { text, .. }) if text == "retain these words"));
        assert!(block(i.unaccept("s1", "journalled")).is_err());
        assert!(block(i.pending("s1", "journalled"))?.is_some());
        Ok(())
    }

    #[test]
    fn pending_custody_outlives_dedup_tidy_and_repeats_original_receipt() -> Result<(), Refusal> {
        let i = Inbox::new(Arc::new(SimRecords::new()), Limits { keep: 1, trim: 1, read: 4 });
        let first = block(i.accept_pending("s1", "old", &digest("original", &[]), Some(0), 10, &pending_message("old", "original")))?;
        block(i.accept("s1", "new", &digest("new", &[]), Some(0), 20))?;
        let _ = block(i.tidy("s1"));
        let repeat = block(i.accept_pending("s1", "old", &digest("original", &[]), Some(0), 30, &pending_message("old", "original")))?;
        assert_eq!(repeat, Answer { repeat: true, ..first });
        assert!(block(i.pending("s1", "old"))?.is_some());
        Ok(())
    }

    #[test]
    fn terminal_custody_stays_until_exact_server_ack() -> Result<(), Refusal> {
        use crate::harness::{admission::{Consumption, Disposition}, mail::Token};
        let i = inbox();
        let first = block(i.accept_pending("s1", "m", &digest("words", &[]), Some(0), 10, &pending_message("m", "words")))?;
        block(i.dispose_pending("s1", "m", 0, Disposition::Consumed { boundary: Consumption::ProviderAccepted }))?;
        let repeat = block(i.accept("s1", "m", &digest("words", &[]), Some(0), 99))?;
        assert_eq!(repeat, Answer { repeat: true, ..first });
        assert!(!block(i.acknowledge_pending("s1", "m", 0, &Token { revision: 1, generation: 2 }, Some(&Disposition::Consumed { boundary: Consumption::ProviderAccepted })))?);
        assert!(!block(i.acknowledge_pending("s1", "m", 1, &Token { revision: 1, generation: 1 }, Some(&Disposition::Consumed { boundary: Consumption::ProviderAccepted })))?);
        assert!(!block(i.acknowledge_pending("s1", "m", 0, &Token { revision: 1, generation: 1 }, None))?, "a delayed nonterminal ACK must not clear a later terminal receipt");
        assert!(!block(i.acknowledge_pending("s1", "m", 0, &Token { revision: 1, generation: 1 }, Some(&Disposition::Consumed { boundary: Consumption::WaitHandoff })))?, "an ACK must bind the exact observed terminal boundary");
        assert!(block(i.pending("s1", "m"))?.is_some());
        assert!(block(i.acknowledge_pending("s1", "m", 0, &Token { revision: 1, generation: 1 }, Some(&Disposition::Consumed { boundary: Consumption::ProviderAccepted })))?);
        assert!(block(i.pending("s1", "m"))?.is_none());
        Ok(())
    }

    #[test]
    fn a_handoff_batch_retains_every_original_before_any_terminal_receipt() -> Result<(), Refusal> {
        use crate::harness::admission::{Consumption, Disposition};
        let i = inbox();
        for message in ["first", "second"] {
            block(i.accept_pending("s1", message, &digest(message, &[]), Some(0), 10, &pending_message(message, message)))?;
        }
        let disposition = Disposition::Consumed { boundary: Consumption::WaitHandoff };
        assert!(block(i.dispose_pending_batch("s1", &["first".into(), "missing".into()], 0, &disposition)).is_err());
        let original = block(i.pending("s1", "first"))?.ok_or_else(|| unavailable("s1", "fixture original missing"))?;
        assert!(original.disposition.is_none(), "a failed batch must not partially return credit");
        assert!(matches!(original.command, Some(crate::harness::model::Command::Send { text, .. }) if text == "first"));
        let disposed = block(i.dispose_pending_batch("s1", &["first".into(), "second".into(), "first".into()], 0, &disposition))?;
        assert_eq!(disposed.len(), 2, "the actual batch quantum counts distinct source communications");
        for message in ["first", "second"] {
            let saved = block(i.pending("s1", message))?.ok_or_else(|| unavailable("s1", "fixture terminal custody missing"))?;
            assert_eq!(saved.disposition, Some(disposition.clone()));
            assert!(matches!(saved.command, Some(crate::harness::model::Command::Send { text, .. }) if text == message));
        }
        Ok(())
    }

    #[test]
    fn an_edit_versions_current_words_without_rewriting_the_accepted_original_or_receipt() -> Result<(), Refusal> {
        let i = inbox();
        let original = pending_message("edited", "accepted original");
        let answer = block(i.accept_pending("s1", "edited", &digest("accepted original", &[]), Some(0), 10, &original))?;
        let mut changed = original.current_command().cloned().ok_or_else(|| unavailable("s1", "fixture command missing"))?;
        if let crate::harness::model::Command::Send { text, .. } = &mut changed { *text = "current explicit edit".into(); }
        assert_eq!(block(i.edit_pending("s1", "edited", original.lease.clone(), changed))?, 1);
        let saved = block(i.pending("s1", "edited"))?.ok_or_else(|| unavailable("s1", "fixture current body missing"))?;
        assert!(matches!(saved.command, Some(crate::harness::model::Command::Send { ref text, .. }) if text == "accepted original"));
        assert!(matches!(saved.current_command(), Some(crate::harness::model::Command::Send { text, .. }) if text == "current explicit edit"));
        let repeated = block(i.accept_pending("s1", "edited", &digest("accepted original", &[]), Some(0), 99, &original))?;
        assert_eq!(repeated, Answer { repeat: true, ..answer }, "immutable publication retry still identifies the accepted original");
        Ok(())
    }

    #[test]
    fn upgrade_adopts_old_dedup_body_and_never_overwrites_a_prior_original() -> Result<(), Refusal> {
        let i = inbox();
        block(i.open("s1", 0))?;
        block(i.accept("s1", "legacy", &digest("retained", &[]), Some(0), 1))?;
        let recovered = block(i.adopt_pending("s1", pending_message("legacy", "retained"), 2))?;
        assert_eq!(recovered.origin, Origin::LegacyRetained);
        assert!(recovered.lease.token.is_none());
        assert!(matches!(recovered.current_command(), Some(crate::harness::model::Command::Send { text, .. }) if text == "retained"));
        let prior = block(i.adopt_pending("s1", pending_message("legacy", "later preview"), 3))?;
        assert!(matches!(prior.current_command(), Some(crate::harness::model::Command::Send { text, .. }) if text == "retained"));
        let mut lease = pending_message("native", "unused").lease;
        lease.stage = crate::harness::admission::Stage::ProviderFlight;
        let opaque = block(i.adopt_pending("s1", Pending::opaque(lease, vec![r#"{"event":"user"}"#.into()]), 4))?;
        assert_eq!(opaque.origin, Origin::LegacyOpaque);
        assert!(opaque.current_command().is_none(), "unknown native input is never fabricated as Send");
        assert_eq!(opaque.opaque_writes.len(), 1);
        Ok(())
    }

    /// CP-T3.2, AC-T3.2.1: a message is taken once. Sent again under its id, the sender gets the
    /// first time's answer, marked a repeat, and the session is to get nothing.
    #[test]
    fn a_message_is_taken_once_and_a_repeat_gets_the_first_answer() -> Result<(), Refusal> {
        let i = inbox();
        block(i.open("s1", 0))?;
        let d = digest("deploy it", &[]);
        let first = block(i.accept("s1", "m1", &d, Some(0), 1_000))?;
        assert_eq!(first, Answer { session: "s1".into(), message: "m1".into(), epoch: 0, at: 1_000, seq: first.seq, repeat: false });
        let again = block(i.accept("s1", "m1", &d, Some(0), 9_000))?;
        assert_eq!(again, Answer { repeat: true, ..first.clone() }, "the first time's answer");
        // Another message goes on, after it.
        assert!(block(i.accept("s1", "m2", &digest("and test it", &[]), Some(0), 9_500))?.seq > first.seq);
        assert_eq!(block(i.taken("s1", 10))?, ["m1", "m2"]);
        Ok(())
    }

    /// CP-T3.2, AC-T3.2.4: a send for an epoch the session has left is refused, typed, naming the
    /// epoch it was sent for and the one the session is at; so is one for an epoch this copy hasn't
    /// reached.
    #[test]
    fn a_send_from_a_stale_epoch_is_refused_naming_the_epochs() -> Result<(), Refusal> {
        let i = inbox();
        block(i.open("s1", 2))?;
        let d = digest("hello", &[]);
        let stale = block(i.accept("s1", "m1", &d, Some(1), 1_000));
        assert_eq!(stale, Err(Refusal::StaleEpoch { session: "s1".into(), sent: 1, current: 2 }));
        assert_eq!(stale.map_err(|e| e.to_string()), Err("this was sent to session s1 at epoch 1, and it's at epoch 2 now: it moved since, so send it again".into()));
        assert_eq!(block(i.accept("s1", "m1", &d, Some(3), 1_000)), Err(Refusal::Behind { session: "s1".into(), here: 2, sent: 3 }));
        // Nothing was taken: sent for its epoch, it goes through.
        assert!(!block(i.accept("s1", "m1", &d, Some(2), 1_000))?.repeat);
        // A sender that doesn't say its epoch isn't fenced by it.
        assert!(!block(i.accept("s1", "m2", &d, None, 1_000))?.repeat);
        Ok(())
    }

    /// CP-T3.2: once the session went on elsewhere its inbox here takes nothing, saying so; back
    /// here at a higher epoch, it takes sends again, and what it took before still counts.
    #[test]
    fn a_fenced_inbox_refuses_until_its_session_comes_back_higher() -> Result<(), Refusal> {
        let i = inbox();
        block(i.open("s1", 1))?;
        let d = digest("x", &[]);
        block(i.accept("s1", "m1", &d, Some(1), 1))?;
        block(i.fence("s1", 1))?;
        assert_eq!(block(i.accept("s1", "m2", &d, Some(1), 2)), Err(Refusal::Moved { session: "s1".into(), epoch: 1 }));
        assert_eq!(block(i.epoch("s1"))?, Some((1, true)));
        // Opened again at its old epoch: still fenced (a stale copy can't lift it).
        assert_eq!(block(i.open("s1", 0))?, 1);
        assert!(matches!(block(i.accept("s1", "m2", &d, None, 2)), Err(Refusal::Moved { .. })));
        assert_eq!(block(i.open("s1", 3))?, 3);
        assert!(block(i.accept("s1", "m1", &d, Some(3), 3))?.repeat, "what it took before still counts");
        assert!(!block(i.accept("s1", "m2", &d, Some(3), 3))?.repeat);
        Ok(())
    }

    /// CP-T3.2: a session handed to another runner process of this machine and taken back (the new
    /// build never took it over) takes messages again at the epoch it left at; a copy left behind
    /// (it went on elsewhere, at a higher epoch) isn't taken back, and opening a session again at
    /// its epoch never lifts a fence.
    #[test]
    fn a_session_taken_back_at_its_epoch_takes_messages_again_and_a_copy_left_behind_does_not() -> Result<(), Refusal> {
        let i = inbox();
        let d = digest("x", &[]);
        block(i.open("s1", 2))?;
        block(i.accept("s1", "m1", &d, Some(2), 1))?;
        block(i.fence("s1", 2))?;
        assert_eq!(block(i.open("s1", 2))?, 2);
        assert!(matches!(block(i.accept("s1", "m2", &d, Some(2), 2)), Err(Refusal::Moved { .. })), "opening again at its epoch leaves the fence");
        assert_eq!(block(i.take_back("s1", 1))?, 2);
        assert!(matches!(block(i.accept("s1", "m2", &d, Some(2), 2)), Err(Refusal::Moved { .. })), "a copy from a lower epoch isn't taken back");
        assert_eq!(block(i.take_back("s1", 2))?, 2);
        assert_eq!(block(i.epoch("s1"))?, Some((2, false)));
        assert!(block(i.accept("s1", "m1", &d, Some(2), 3))?.repeat, "what it took before still counts");
        assert!(!block(i.accept("s1", "m2", &d, Some(2), 3))?.repeat);
        // Taken back by a host that never had it: as if opened.
        assert_eq!(block(i.take_back("s2", 4))?, 4);
        assert!(!block(i.accept("s2", "m1", &d, Some(4), 3))?.repeat);
        Ok(())
    }

    /// CP-T3.2: what goes with a session that moves is its newest ids (the likeliest to be sent
    /// again), however many it keeps; on the host it goes to they're repeats.
    #[test]
    fn a_carry_holds_the_newest_ids() -> Result<(), Refusal> {
        let here = inbox();
        block(here.open("s1", 0))?;
        let d = digest("x", &[]);
        let most = KEEP.saturating_add(TRIM).saturating_sub(1);
        for n in 0..most {
            block(here.accept("s1", &format!("m{n}"), &d, Some(0), 1))?;
        }
        let carried = block(here.taken("s1", usize::try_from(KEEP).unwrap_or(usize::MAX)))?;
        assert_eq!(carried.len(), usize::try_from(KEEP).unwrap_or(usize::MAX));
        let newest = format!("m{}", most.saturating_sub(1));
        assert_eq!(carried.last(), Some(&newest), "the newest last");
        assert_eq!(carried.first().map(String::as_str), Some(format!("m{}", most.saturating_sub(KEEP)).as_str()), "the oldest of them first");
        let there = inbox();
        block(there.open("s1", 1))?;
        block(there.import("s1", &carried, 5))?;
        assert!(block(there.accept("s1", &newest, &d, Some(1), 9))?.repeat, "{newest}, taken before the move, is a repeat after it");
        Ok(())
    }

    /// CP-T3.2: a session that came here while its inbox couldn't be opened (it stayed at an older
    /// epoch), and then went on elsewhere, is fenced at the epoch it left at: a copy of it restored
    /// here at that epoch (left behind) can't lift the fence, by opening its inbox or by a message.
    /// Back higher, it takes messages. (The sends simulation found the copy taking one, seed 67657.)
    #[test]
    fn a_copy_left_behind_cannot_lift_a_fence_its_inbox_lagged_behind() -> Result<(), Refusal> {
        let i = inbox();
        let d = digest("x", &[]);
        // Came at epoch 3, its inbox left at 0 (the ids it brought taken, opening it failed); then
        // went on elsewhere from 3.
        block(i.import("s1", &["m0".into()], 1))?;
        block(i.fence("s1", 3))?;
        assert_eq!(block(i.epoch("s1"))?, Some((3, true)));
        assert_eq!(block(i.open("s1", 3))?, 3);
        assert!(matches!(block(i.accept("s1", "m1", &d, Some(3), 2)), Err(Refusal::Moved { epoch: 3, .. })), "a copy left behind takes nothing");
        assert_eq!(block(i.open("s1", 4))?, 4);
        assert!(!block(i.accept("s1", "m1", &d, Some(4), 3))?.repeat);
        assert!(block(i.accept("s1", "m0", &d, Some(4), 3))?.repeat, "what it brought still counts");
        Ok(())
    }

    /// CP-T3.2: an id given to another message is refused, typed, never taken as a repeat.
    #[test]
    fn an_id_reused_for_another_message_is_refused() -> Result<(), Refusal> {
        let i = inbox();
        block(i.accept("s1", "m1", &digest("one thing", &[]), None, 1))?;
        assert_eq!(block(i.accept("s1", "m1", &digest("another thing", &[]), None, 2)), Err(Refusal::Reused { session: "s1".into(), message: "m1".into() }));
        // Files are part of what a message is.
        assert_ne!(digest("look", &["a.png"]), digest("look", &["b.png"]));
        assert_ne!(digest("look", &[]), digest("look", &[""]));
        Ok(())
    }

    /// CP-T3.2: what an inbox keeps is bounded: past its last `keep` ids and `trim` more, the oldest
    /// `trim` go, and a repeat of one that went is taken as new (the stated bound).
    #[test]
    fn an_inbox_keeps_its_last_ids_and_the_oldest_go() -> Result<(), Refusal> {
        let i = Inbox::new(Arc::new(SimRecords::new()), Limits { keep: 4, trim: 2, ..Limits::default() });
        let d = digest("x", &[]);
        for n in 1..=6 {
            block(i.accept("s1", &format!("m{n}"), &d, None, n))?;
            // As the host does once a take's answer has gone.
            block(i.tidy("s1"));
        }
        // 6 = 4 kept and 2 more: m1 and m2 went.
        assert_eq!(block(i.taken("s1", 100))?, ["m3", "m4", "m5", "m6"]);
        assert!(block(i.accept("s1", "m3", &d, None, 7))?.repeat);
        assert!(!block(i.accept("s1", "m1", &d, None, 7))?.repeat, "past the bound, taken as new");
        Ok(())
    }

    /// CP-T3.2 (the twelfth adversarial check's): no read of a session's ids is without its bound.
    /// One past what's read at once (its tidies failed while it took 20, 8 read at once) doesn't
    /// move: what goes with it is refused, typed, until it's tidied. Each tidy reads 8 at most and
    /// lets go the oldest 4 of them (each has 4 newer than it among them, so it's never among the
    /// session's newest), and they converge on its newest 4, which go with it then.
    #[test]
    fn a_session_past_what_is_read_at_once_is_tidied_a_read_at_a_time() -> Result<(), Refusal> {
        let i = Inbox::new(Arc::new(SimRecords::new()), Limits { keep: 4, trim: 2, read: 8 });
        let d = digest("x", &[]);
        for n in 1..=20 {
            block(i.accept("s1", &format!("m{n}"), &d, None, n))?;
        }
        let carried = block(i.carried("s1", 100));
        assert!(matches!(&carried, Err(Refusal::Unavailable { why, .. }) if why.contains("8 read at once")), "{carried:?}");
        let gone: Vec<usize> = (0..5).map(|_| block(i.tidy("s1"))).collect();
        assert_eq!(gone, [4, 4, 4, 4, 0], "4 a tidy (8 read, 4 kept), down to its newest 4");
        assert_eq!(block(i.taken("s1", 100))?, ["m17", "m18", "m19", "m20"]);
        assert!(block(i.accept("s1", "m17", &d, None, 21))?.repeat);
        assert!(!block(i.accept("s1", "m16", &d, None, 22))?.repeat, "past the bound, taken as new");
        Ok(())
    }

    /// CP-T3.2 (the thirteenth adversarial check's): ids brought in one commit share its revision;
    /// their order as brought (the oldest first) orders them among themselves, so what goes past
    /// the bound is the oldest of them, whatever their ids' order, and what goes with the session
    /// when it moves is its newest.
    #[test]
    fn ids_brought_together_are_let_go_oldest_first() -> Result<(), Refusal> {
        let i = Inbox::new(Arc::new(SimRecords::new()), Limits { keep: 4, trim: 2, ..Limits::default() });
        block(i.open("s1", 1))?;
        // Brought oldest first, their ids sorting the other way.
        let brought: Vec<String> = ["d", "c", "b", "a"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(block(i.import("s1", &brought, 5))?, 4);
        let d = digest("x", &[]);
        for n in 1..=3 {
            block(i.accept("s1", &format!("m{n}"), &d, Some(1), 10 + n))?;
        }
        // 7, past 4 and 2 more: down to the newest 4.
        assert_eq!(block(i.tidy("s1")), 3);
        assert_eq!(block(i.taken("s1", 100))?, ["a", "m1", "m2", "m3"]);
        assert!(block(i.accept("s1", "a", &d, Some(1), 20))?.repeat, "the newest brought stays");
        assert!(!block(i.accept("s1", "d", &d, Some(1), 21))?.repeat, "the oldest brought went");
        Ok(())
    }

    /// CP-T3.2: the ids a session brings from the machine it moved from count as taken here.
    #[test]
    fn ids_brought_from_another_machine_count_as_taken() -> Result<(), Refusal> {
        let i = inbox();
        block(i.open("s1", 2))?;
        assert_eq!(block(i.import("s1", &["a".into(), "b".into(), "a".into(), "\u{7}".into()], 5))?, 2, "each new id once; one that isn't an id is left out");
        let repeat = block(i.accept("s1", "a", &digest("whatever it said", &[]), Some(2), 9))?;
        assert!(repeat.repeat && repeat.seq > 0 && repeat.epoch == 2 && repeat.at == 5, "{repeat:?}");
        assert_eq!(block(i.import("s1", &["b".into(), "c".into()], 6))?, 1);
        Ok(())
    }

    /// CP-T3.2: ids brought twice (a session's timeline shows what its inbox took too) never take
    /// another id's room: past the bound, the newest distinct ids are kept, the oldest of them first.
    /// CP-T3.2 (the fourth adversarial check's): an id carried with its digest from where its
    /// session was is that message's here too: another message under it is refused `reused`, the
    /// same one is its repeat. One brought without a digest (from its timeline, whose words may
    /// differ from how they came) takes whatever comes under it as its repeat.
    #[test]
    fn another_message_under_a_carried_id_is_refused_reused() -> Result<(), Refusal> {
        let i = inbox();
        block(i.open("s1", 1))?;
        let d = digest("ship it", &[]);
        block(i.import_kept("s1", &[("m1".into(), d.clone()), ("m2".into(), String::new())], 1))?;
        let other = block(i.accept("s1", "m1", &digest("something else entirely", &[]), Some(1), 2));
        assert!(matches!(other, Err(Refusal::Reused { .. })), "another message under a carried id: {other:?}");
        assert!(block(i.accept("s1", "m1", &d, Some(1), 3))?.repeat, "the same one: its repeat");
        assert!(block(i.accept("s1", "m2", &digest("anything", &[]), Some(1), 4))?.repeat, "its words unknown here: its repeat");
        // What goes on with it carries the digests it kept.
        assert_eq!(block(i.carried("s1", 10))?, [("m1".to_owned(), d), ("m2".to_owned(), String::new())]);
        Ok(())
    }

    #[test]
    fn an_import_keeps_the_newest_distinct_ids_however_often_they_are_listed() -> Result<(), Refusal> {
        let limits = Limits { keep: 8, trim: 2, ..Limits::default() };
        let d = digest("x", &[]);
        let inbox: Vec<String> = (0..8).map(|n| format!("m{n}")).collect();
        // The timeline's ids (its newest 5) first, the inbox's own after them.
        let i = Inbox::new(Arc::new(SimRecords::new()), limits);
        block(i.open("s1", 1))?;
        let mut brought: Vec<String> = inbox.iter().skip(3).cloned().collect();
        brought.extend(inbox.iter().cloned());
        assert_eq!(block(i.import("s1", &brought, 5))?, 8, "each of the 8 once");
        assert!(block(i.accept("s1", "m0", &d, Some(1), 9))?.repeat, "the oldest one carried still counts");
        // Listed the other way round: the same 8.
        let j = Inbox::new(Arc::new(SimRecords::new()), limits);
        block(j.open("s1", 1))?;
        let mut after: Vec<String> = inbox.clone();
        after.extend(inbox.iter().skip(3).cloned());
        assert_eq!(block(j.import("s1", &after, 5))?, 8);
        // More than it keeps: the newest 8 distinct.
        let k = Inbox::new(Arc::new(SimRecords::new()), limits);
        block(k.open("s1", 1))?;
        let ten: Vec<String> = (0..10).map(|n| format!("m{n}")).collect();
        assert_eq!(block(k.import("s1", &ten, 5))?, 8);
        assert_eq!(block(k.taken("s1", 100))?, ten.get(2..).unwrap_or_default());
        Ok(())
    }

    /// CP-T3.2: a message taken that never reached its session (it was on its way elsewhere) is
    /// forgotten, so its retry goes through.
    #[test]
    fn a_message_taken_back_is_taken_again_on_its_retry() -> Result<(), Refusal> {
        let i = inbox();
        let d = digest("x", &[]);
        block(i.accept("s1", "m1", &d, None, 1))?;
        block(i.unaccept("s1", "m1"))?;
        assert!(!block(i.accept("s1", "m1", &d, None, 2))?.repeat);
        Ok(())
    }

    /// CP-T3.2: a session that ended takes its inbox with it.
    #[test]
    fn a_forgotten_session_keeps_nothing() -> Result<(), Refusal> {
        let r = Arc::new(SimRecords::new());
        let i = Inbox::new(r.clone(), Limits { keep: 600, trim: 10, ..Limits::default() });
        for n in 0..700u32 {
            block(i.accept("s1", &format!("m{n}"), "d", None, 1))?;
        }
        block(i.accept("s2", "m1", "d", None, 1))?;
        block(i.forget("s1"))?;
        assert!(block(i.taken("s1", 1000))?.is_empty());
        assert_eq!(block(i.epoch("s1"))?, None);
        let left = |family: &str| block(r.scan(family, Scan { prefix: Vec::new(), after: None, limit: 1000, at: Consistency::Linearizable })).map(|p| p.rows.len());
        assert_eq!((left(MESSAGES).ok(), left(HEADS).ok()), (Some(1), Some(1)), "only s2's");
        Ok(())
    }

    /// docs/sends.md says what the code does: every refusal, by its reason; every bound, as the code
    /// has it; and every sends metric, by its name, and no other.
    #[test]
    fn docs_sends_says_what_the_code_does() {
        let doc = include_str!("../../../../../docs/sends.md");
        let s = || "s".to_owned();
        let refusals = [
            Refusal::StaleEpoch { session: s(), sent: 0, current: 1 },
            Refusal::Behind { session: s(), here: 0, sent: 1 },
            Refusal::Moved { session: s(), epoch: 0 },
            Refusal::Reused { session: s(), message: s() },
            Refusal::Invalid { what: s(), why: s() },
            Refusal::Full { session: s(), limit: 1 },
            Refusal::NoSession { session: s() },
            Refusal::Unavailable { session: s(), why: s() },
        ];
        for r in &refusals {
            assert!(doc.contains(&format!("| `{}` |", r.reason())), "the doc's table of refusals lacks {}", r.reason());
        }
        // A count as the doc says it: 256, 4,096, 10,000.
        let count = |n: u64| {
            let d = n.to_string();
            let mut out = String::new();
            for (i, c) in d.chars().enumerate() {
                if i > 0 && (d.len() - i).is_multiple_of(3) {
                    out.push(',');
                }
                out.push(c);
            }
            out
        };
        let bounds = [
            format!("last {} messages", count(KEEP)),
            format!("past {} and {} more, its", count(KEEP), count(TRIM)),
            format!("down to its newest {}", count(KEEP)),
            format!("A lane holds {}", count(super::super::host::LANE as u64)),
            format!("holds {} waiting", count(super::super::host::LANE as u64)),
            format!("waits {} s for an answer", super::super::registry::ANSWER_WAIT.as_secs()),
            format!("for {} messages at once", count(super::super::registry::WAITING_MAX as u64)),
            format!("over {} bytes", ID_MAX),
            format!("keeps {} sessions' heads", count(HEADS_MAX as u64)),
            format!("each hand-over it took (the newest {},", count(super::super::host::HANDOVERS_MAX as u64)),
            format!("still knows, its newest {} too)", count(super::super::host::HANDOVERS_MAX as u64)),
            format!("reads at most {} of a session's ids at once", count(READ)),
            format!("a stack keeps its newest {} asked under a key", crate::goals::ASKED_MAX),
        ];
        // As read, however its lines are wrapped.
        let flat = doc.split_whitespace().collect::<Vec<_>>().join(" ");
        for b in &bounds {
            assert!(flat.contains(b.as_str()), "the doc doesn't say {b:?}");
        }
        // Its metrics are exactly those rendered.
        let rendered = crate::metrics::render();
        let named: std::collections::BTreeSet<&str> = rendered.lines().filter_map(|l| l.strip_prefix("# TYPE ")).filter_map(|l| l.split_whitespace().next()).filter(|n| n.starts_with("kiln_sends_")).collect();
        let said: std::collections::BTreeSet<&str> = doc.lines().filter_map(|l| l.strip_prefix("| `")).filter_map(|l| l.split('`').next()).map(|n| n.split('{').next().unwrap_or(n)).filter(|n| n.starts_with("kiln_sends_")).collect();
        assert_eq!(said, named, "the doc's metrics and the code's");
    }

    /// What one transaction of the inbox writes stays within a transaction's most.
    #[test]
    fn its_transactions_fit() {
        const { assert!(PAGE <= records::TX_MAX && IMPORT_CHUNK < records::TX_MAX) };
    }

    /// CP-T3.2: what's refused before the records are touched: ids that can't be keys.
    #[test]
    fn bad_sessions_and_ids_are_refused_typed() {
        let i = inbox();
        assert!(matches!(block(i.accept("", "m1", "d", None, 1)), Err(Refusal::Invalid { .. })));
        assert!(matches!(block(i.accept("s/1", "m1", "d", None, 1)), Err(Refusal::Invalid { .. })));
        assert!(matches!(block(i.accept("s1", "", "d", None, 1)), Err(Refusal::Invalid { .. })));
        assert!(matches!(block(i.accept("s1", &"m".repeat(ID_MAX + 1), "d", None, 1)), Err(Refusal::Invalid { .. })));
        assert!(matches!(block(i.accept("s1", "m\n1", "d", None, 1)), Err(Refusal::Invalid { .. })));
    }

    /// CP-T3.2: the inbox on kiln's Turso store: what it took is still taken after the host starts
    /// again (the store opened anew, the inbox made anew).
    #[test]
    fn what_was_taken_is_still_taken_after_a_restart() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("records.db");
        let d = digest("ship it", &[]);
        let first = {
            let i = Inbox::new(crate::records::open(&path)?, Limits::default());
            block(i.open("s1", 4))?;
            block(i.accept("s1", "m1", &d, Some(4), 10))?
        };
        let i = Inbox::new(crate::records::open(&path)?, Limits::default());
        assert_eq!(block(i.accept("s1", "m1", &d, Some(4), 99))?, Answer { repeat: true, ..first });
        assert_eq!(block(i.accept("s1", "m2", &d, Some(3), 99)), Err(Refusal::StaleEpoch { session: "s1".into(), sent: 3, current: 4 }));
        Ok(())
    }
}
