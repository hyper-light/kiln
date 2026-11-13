//! Custody records kept (docs/ci.md W27-R08; the Transfers plan's step 2, kiln plan
//! `01a17856b1b477eda14a574ae312a123`; migration `w27_custody`). Every signed custody record and
//! delegation kiln takes ([`kiln_ci::identity::custody`]) is kept by its statement's digest, its
//! envelope as it was signed, checked as of when kiln took it. Its own hop is checked as it's
//! taken: a receipt that comes before the hand-on it answers waits for it, and is checked when it
//! comes. A break found then, or by walking a thing's chain, is recorded with the records around
//! it; a statement kiln can't take (its signer's identity revoked, as a copy left behind's is once
//! its session went on) is refused and recorded with its envelope. Breaks and refusals go to the
//! audit log too, and to the trail once it's built (the Review plan's step 9).

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use kiln_ci::identity::chain::{Actor, Identity, Revocation};
use kiln_ci::identity::custody::{self, Break, Hop, Kind};
use kiln_ci::identity::statement::{self, Checked, PAYLOAD_CUSTODY, PAYLOAD_DELEGATION};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use crate::store::Store;

pub mod blobs;
pub mod cluster;
pub mod feeds;
pub mod runners;

/// The most records a walk loads: a thing's chain, and its custodians' streams.
pub const WALK_MAX: usize = 10_000;
/// The most breaks or refusals a list gives, newest first.
pub const LIST_MAX: usize = 500;
/// How long an offer (a hand-on) waits for its receipt before it lapses, its giver still holding
/// what it offered (seconds).
pub const LAPSE_AFTER: i64 = 600;

/// The digest algorithm a session's timeline is named by ([`timeline`]).
pub const TIMELINE: &str = "kiln-timeline-v1";

/// Session `session`'s timeline as it leaves epoch `epoch` with `events`: the thing a hand-over of
/// it is about ("session <id> at epoch <n>"), its digest over its id, that epoch and each event's
/// digest (`crate::wire::sums::event`), which both sides work out alike from the events its
/// package carries. A timeline grows while it's held, so each epoch's is a thing of its own.
pub fn timeline(session: &str, epoch: u64, events: &[crate::harness::model::Event]) -> statement::Subject {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(session.as_bytes());
    h.update([0]);
    h.update(epoch.to_be_bytes());
    h.update(u64::try_from(events.len()).unwrap_or(u64::MAX).to_be_bytes());
    for e in events {
        h.update(serde_json::to_value(e).map(|v| crate::wire::sums::event(&v)).unwrap_or_default().to_be_bytes());
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let annotations = BTreeMap::from([("epoch".to_owned(), Value::from(epoch)), ("events".to_owned(), Value::from(events.len()))]);
    statement::Subject { name: format!("session {session} at epoch {epoch}"), digest: BTreeMap::from([(TIMELINE.to_owned(), hex)]), size: None, annotations }
}

/// The agent identity at `epoch` of the session whose identity at some epoch is `uri`, without the
/// runner process holding its key: whom a hand-on goes to, or a receipt comes from.
pub fn at_epoch(uri: &str, epoch: u64) -> Option<String> {
    let (head, _) = uri.split_once("/epoch/")?;
    head.contains("/agent/").then(|| format!("{head}/epoch/{epoch}"))
}

/// kiln's server's identity in organization `org`: the only signer of takes.
pub fn server_uri(org: &str) -> String {
    Identity::new(org, Actor::Server).uri().unwrap_or_default()
}

/// A kind's name, as records say it (`handed-on`).
fn kind_name(k: Kind) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// The digest of a bundle's statement, read without checking it ("" when it can't be read).
pub(crate) fn digest_of(bundle: &Value) -> String {
    let env = bundle.get("dsseEnvelope");
    let ty = env.and_then(|e| e.get("payloadType")).and_then(Value::as_str);
    let payload = env.and_then(|e| e.get("payload")).and_then(Value::as_str).and_then(|p| base64::engine::general_purpose::STANDARD.decode(p).ok());
    match (ty, payload) {
        (Some(t), Some(p)) => statement::digest(t, &p),
        _ => String::new(),
    }
}

/// What became of a statement kiln took.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Taken {
    pub digest: String,
    /// kiln had it already: the same statement, by its digest, is kept once.
    pub again: bool,
    /// Its own hop doesn't hold: the break, recorded.
    pub broke: Option<String>,
}

/// Why kiln didn't take a statement. Each is recorded ([`refused`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// Not a signed statement kiln takes: its signer's chain, signature or form, as of now.
    #[error("{0}")]
    Unsigned(String),
    /// Signed, but not a custody record or a delegation kiln reads.
    #[error("{0}")]
    NotCustody(String),
}

/// A refusal, as kiln records it.
#[derive(Default)]
struct Refusal {
    digest: String,
    act: String,
    signer: String,
    why: String,
    envelope: String,
    came_from: String,
    at: i64,
    /// An agent session's: the epoch it's from (signed at, or, unsigned, the one its sender held
    /// the session at).
    epoch: Option<u64>,
    /// The transfer the session went on with, when that epoch is one it left.
    superseded_by: String,
}

/// Takes the signed custody record or delegation `bundle`, which came over `came_from` (a runner
/// process, an API caller), at `at`: checked as of then against kiln's root `root` and the
/// revocations kiln has; kept by its digest; its own hop checked, and the hops of records that
/// waited for it. Refused, and recorded, when kiln can't take it. One kiln has already (sent again
/// after a reconnect, or carried by another) is answered as such, as it was judged when taken: its
/// signer may have moved on since. What reading and checking it takes is made off the shards
/// (`read`, `check`: goal 9, plan 01a1a87b2cec step 3); the shard only waits for that and for the store.
pub async fn take(store: &Store, root: &[u8], org: &str, bundle: &Value, came_from: &str, at: i64) -> anyhow::Result<Result<Taken, Refused>> {
    take_owned(store, root, org, bundle.clone(), came_from, at).await
}

/// [`take`], `bundle` handed over rather than copied (goal 9, plan 01a1a87b2cec step 5): what each feed's batch takes,
/// a runner's hand-on and kiln's receipt (`custody::feeds`).
///
/// A record new to kiln is one job on the compute pool, one call to the store's thread and, when its hop needs records
/// kiln has, one more job. It was six calls (the revocations read, a look for it, its keeping, itself read back, the
/// records it follows read, its followers looked for) and three jobs (its check, then itself and the records it follows
/// checked again), each a wait the shard parked through and a wake back (rt7's calm lab: 9 to 12 store calls and 6 to 8
/// compute jobs a request, almost all of them a feed's batches' records). It's checked
/// against the revocations the store last read ([`crate::identities::revoked`]), and kept only if they're still all
/// there are, looked at in the same call: else they're read again and it's checked again. Kept, its own hop is checked
/// with itself as it was read (read back, it checks the same: it checked against every revocation then, and is checked
/// again against those before it), and the records it follows as each was taken, as before.
pub async fn take_owned(store: &Store, root: &[u8], org: &str, bundle: Value, came_from: &str, at: i64) -> anyhow::Result<Result<Taken, Refused>> {
    take_read(store, root, org, std::sync::Arc::new(bundle), None, came_from, at).await
}

/// [`take_owned`], `first` the bundle's read already made and the revocations it was checked against ([`sign_and_take`]).
async fn take_read(store: &Store, root: &[u8], org: &str, bundle: std::sync::Arc<Value>, mut first: Option<(Read, std::sync::Arc<crate::identities::Revoked>)>, came_from: &str, at: i64) -> anyhow::Result<Result<Taken, Refused>> {
    let root = std::sync::Arc::<[u8]>::from(root);
    let mut revoked = match &first {
        Some((_, revoked)) => std::sync::Arc::clone(revoked),
        None => crate::identities::revoked(store).await?,
    };
    // Checked against stale revocations: read again and checked again, once; then kept as the check found it, as before
    // they were kept (a revocation added between its read and the record's keeping went unseen then too).
    let mut stale_once = false;
    loop {
        let Read { digest, verdict } = match first.take() {
            Some((made, _)) => made,
            None => read(&bundle, &root, &revoked.list, came_from, at, org).await?,
        };
        let (kind, rows, own) = match verdict {
            Verdict::Custody { kind, rows, own } => (kind, *rows, own),
            Verdict::Unsigned { why, signer, envelope } => {
                if !digest.is_empty() && kept(store, &digest).await? {
                    return Ok(Ok(Taken { digest, again: true, broke: None }));
                }
                // Who signed it, as its certificate says, revocations aside (`read`): a copy left
                // behind signs as the epoch it left.
                let (epoch, superseded_by) = left(store, &signer).await?;
                let refusal = Refusal { digest, act: "a signed statement".to_owned(), signer, why: why.clone(), envelope, came_from: came_from.to_owned(), at, epoch, superseded_by };
                refuse(store, refusal).await?;
                return Ok(Err(Refused::Unsigned(why)));
            }
            Verdict::NotCustody { act, why, custodian, envelope } => {
                if !digest.is_empty() && kept(store, &digest).await? {
                    return Ok(Ok(Taken { digest, again: true, broke: None }));
                }
                refuse(store, Refusal { digest, act: act.to_owned(), signer: custodian, why: why.clone(), envelope, came_from: came_from.to_owned(), at, ..Refusal::default() }).await?;
                return Ok(Err(Refused::NotCustody(why)));
            }
        };
        let stamp = (!stale_once).then_some(revoked.stamp);
        let (wanted, delegation) = (own.wanted().to_vec(), kind.is_none());
        let looked = store.call(move |c| keep_and_look(c, stamp, &digest, rows, &wanted, delegation)).await?;
        let (taken, wanted_rows, waiting) = match looked {
            Looked::Stale => {
                revoked = crate::identities::revoked_now(store).await?;
                stale_once = true;
                continue;
            }
            Looked::Again(digest) => return Ok(Ok(Taken { digest, again: true, broke: None })),
            Looked::New { taken, wanted, waiting } => (taken, wanted, waiting),
        };
        return settle(store, &root, org, &revoked, own, None, taken, wanted_rows, waiting, at).await.map(Ok);
    }
}

/// What follows a record kept new ([`take`]): its own hop, checked as `own` leaves it (or `found`, checked already with the
/// records it follows as read: [`take_batch`]), with `wanted_rows` the rows of the records it needs that kiln has; a break
/// recorded; then the records that waited for it (`waiting`: a custody record's followers, or, a delegation, the records
/// taken for someone under it, which come from other machines in any order), each checked now.
#[allow(clippy::too_many_arguments)]
async fn settle(store: &Store, root: &std::sync::Arc<[u8]>, org: &str, revoked: &crate::identities::Revoked, own: Own, found: Option<Option<Broken>>, taken: String, wanted_rows: Vec<(String, String, i64)>, waiting: Vec<String>, at: i64) -> anyhow::Result<Taken> {
    let broke = match (found, own) {
        (Some(found), _) | (None, Own::Alone(found)) => found,
        (None, Own::Unchecked) => None,
        (None, Own::Waits { checked, wanted }) => {
            let (root, revocations, org, d) = (std::sync::Arc::clone(root), std::sync::Arc::clone(&revoked.list), org.to_owned(), taken.clone());
            crate::rt::compute(move || broken(&d, *checked, checked_rows(wanted_rows, &root, &revocations), &wanted, &org)).await?
        }
    };
    let broke = match broke {
        Some((b, around, relay)) => {
            record_break(store, &taken, &b, &around, &relay, at).await?;
            Some(b.to_string())
        }
        None => None,
    };
    for waiting in waiting {
        check(store, root, org, &waiting, &revoked.list, at).await?;
    }
    Ok(Taken { digest: taken, again: false, broke })
}

/// A statement as [`take`] reads it ([`read`]): its digest ("" when it can't be read), and what kiln makes of it.
struct Read {
    digest: String,
    verdict: Verdict,
}

/// What kiln makes of a statement it's to take; one it refuses with its envelope as kiln keeps it.
enum Verdict {
    /// It doesn't check: why, and who signed it as its certificate says, revocations aside.
    Unsigned { why: String, signer: String, envelope: String },
    /// It checks, but isn't a custody record or a delegation kiln reads: what it was taken for, why, and its signer.
    NotCustody { act: &'static str, why: String, custodian: String, envelope: String },
    /// A custody record (its kind) or a delegation (none), with the rows that keep it and what its own hop needs.
    Custody { kind: Option<Kind>, rows: Box<Rows>, own: Own },
}

/// A record's own hop, as [`read`] leaves it for [`take`] to check once the record is kept ([`check`] for one kept
/// before).
enum Own {
    /// A delegation: its hop isn't checked as it's taken.
    Unchecked,
    /// It follows no record (nor acts under one) kiln must have: its hop, checked where it was read, with only itself
    /// (a break, the records around it and the relay it names, when it doesn't hold).
    Alone(Option<Broken>),
    /// Itself as it was read, and the records its hop needs ([`wanted_by`]).
    Waits { checked: Box<Checked>, wanted: Vec<String> },
}

impl Own {
    /// The records its hop needs kiln to have.
    fn wanted(&self) -> &[String] {
        match self {
            Own::Waits { wanted, .. } => wanted,
            Own::Unchecked | Own::Alone(_) => &[],
        }
    }
}

/// A hop that doesn't hold: the break, the records around it (the broken record first), and the relay it names ("" for none).
type Broken = (Break, Vec<String>, String);

/// `bundle` read for [`take`] on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3): its digest, its
/// envelope written, its check against kiln's root `root` and `revocations` as of `at` (its signature and certificate
/// chain: several public-key verifications, a third of a millisecond together on the lab machine), its body read and
/// its rows made, its own hop made ready (or, needing no other record, checked: step 5), and the statement let go
/// there. In g9p6's calm profile taking records held the shards' threads for 8.6% of their cycles with the check on
/// the pool already: the digest, the envelope, the body, the rows and the hop ([`check`]), between its waits for the
/// store.
async fn read(bundle: &std::sync::Arc<Value>, root: &std::sync::Arc<[u8]>, revocations: &std::sync::Arc<[Revocation]>, came_from: &str, at: i64, org: &str) -> anyhow::Result<Read> {
    let (bundle, root, revocations, came_from, org) = (std::sync::Arc::clone(bundle), std::sync::Arc::clone(root), std::sync::Arc::clone(revocations), came_from.to_owned(), org.to_owned());
    crate::rt::compute(move || with_recent_hop(read_now(&bundle, &root, &revocations, came_from, at, &org), &org)).await
}

/// `read`, its own hop checked now where every record it needs is one of kiln's own kept moments ago ([`recent`]: a
/// mirror's receipt of kiln's hand-on of a batch), with those as they checked when kiln signed them (goal 9, plan
/// 01a1a87b2cec step 5). Its keeping call then reads no row for the hop, and no job of its own reads them back and
/// checks them again: the same hop as those rows give (the same records, checked as kiln took them).
fn with_recent_hop(mut read: Read, org: &str) -> Read {
    let found = match &read.verdict {
        Verdict::Custody { own: Own::Waits { checked, wanted }, .. } => recent::hop(checked, wanted, org),
        _ => None,
    };
    if let (Some(found), Verdict::Custody { own, .. }) = (found, &mut read.verdict) {
        *own = Own::Alone(found);
        recent::served();
    }
    read
}

/// kiln's own records kept moments ago, as they checked when it signed them (its hand-ons of its store's batches to
/// mirrors: [`sign_and_take`]), found by digest (goal 9, plan 01a1a87b2cec step 5): a mirror's receipt of one, taken
/// soon after, has its hop checked with it as it's read ([`with_recent_hop`]). A ring of slots, each swapped whole,
/// no lock; one found in none (older, another process's) is read back from the store, as before.
pub(crate) mod recent {
    use std::collections::BTreeMap;
    #[cfg(test)]
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, OnceLock};

    use arc_swap::ArcSwapOption;
    use kiln_ci::identity::statement::Checked;

    struct Ring {
        slots: Box<[ArcSwapOption<Checked>]>,
        next: AtomicUsize,
    }

    static RING: OnceLock<Ring> = OnceLock::new();
    /// Hops checked with records found here: a test build's stamp, which its tests read (nothing reads it in a server,
    /// so a server doesn't count it: the legacy audit, 2026-11-06).
    #[cfg(test)]
    static SERVED: AtomicU64 = AtomicU64::new(0);

    /// Keeps them for `mirrors` mirrors (`[conversations] mirrors`), once a process: each mirror's feed has at most
    /// `crate::convo::feed::WINDOW` batches sent ahead of its answers, and a batch's receipt can still be on its way as
    /// the window moves on, so twice that a mirror, one at the least.
    pub fn keep_for(mirrors: usize) {
        let slots = 2 * crate::convo::feed::WINDOW * mirrors.max(1);
        let _ = RING.get_or_init(|| Ring { slots: (0..slots).map(|_| ArcSwapOption::empty()).collect(), next: AtomicUsize::new(0) });
    }

    /// `c`, one of kiln's own records just kept, in the place of the oldest.
    pub fn keep(c: Checked) {
        let Some(ring) = RING.get() else { return };
        let at = ring.next.fetch_add(1, Ordering::Relaxed).checked_rem(ring.slots.len());
        if let Some(slot) = at.and_then(|i| ring.slots.get(i)) {
            slot.store(Some(Arc::new(c)));
        }
    }

    /// Record `digest`, if it's here.
    pub fn find(digest: &str) -> Option<Arc<Checked>> {
        RING.get()?.slots.iter().find_map(|s| s.load_full().filter(|c| c.digest == digest))
    }

    /// The hop of `mine` (a record as read), which needs the records `wanted`, checked with them here ([`super::broken`]),
    /// when every one is; else none (read back as before).
    pub fn hop(mine: &Checked, wanted: &[String], org: &str) -> Option<Option<super::Broken>> {
        let map: BTreeMap<String, Checked> = wanted.iter().map(|w| find(w).map(|c| (w.clone(), Checked::clone(&c)))).collect::<Option<_>>()?;
        Some(super::broken(&mine.digest, mine.clone(), map, wanted, org))
    }

    /// A hop was checked with records kept here: counted in a test build ([`served_so_far`]), nothing in a server.
    pub fn served() {
        #[cfg(test)]
        SERVED.fetch_add(1, Ordering::Relaxed);
    }

    /// Hops checked with records kept here, so far.
    #[cfg(test)]
    pub fn served_so_far() -> u64 {
        SERVED.load(Ordering::Relaxed)
    }
}

/// [`read`], on the thread it's called on: a compute job's.
fn read_now(bundle: &Value, root: &[u8], revocations: &[Revocation], came_from: String, at: i64, org: &str) -> Read {
    read_keeping(bundle, root, revocations, came_from, at, org, false).0
}

/// [`read_now`], and with `keep`, the statement as it checked, when it did (what the next record's hop needs of it, when
/// both are taken together: [`take_batch`]).
fn read_keeping(bundle: &Value, root: &[u8], revocations: &[Revocation], came_from: String, at: i64, org: &str, keep: bool) -> (Read, Option<Checked>) {
    let (digest, envelope) = (digest_of(bundle), bundle.to_string());
    let mut checked = None;
    let verdict = match statement::verify(bundle, root, revocations, at) {
        Ok(c) => {
            if keep {
                checked = Some(c.clone());
            }
            verdict_of(c, envelope, came_from, org)
        }
        Err(e) => Verdict::Unsigned { why: e.to_string(), signer: statement::verify(bundle, root, &[], at).map(|c| c.custodian).unwrap_or_default(), envelope },
    };
    (Read { digest, verdict }, checked)
}

/// What [`take`] made of a record ([`take_batch`]'s each).
pub type TakeOutcome = anyhow::Result<Result<Taken, Refused>>;

/// A conversation feed's batch as kiln's intake keeps its two records (goal 9, plan 01a1a87b2cec step 5): the runner's
/// hand-on `given`, as it came over `came_from`, then kiln's receipt of it, `receipt`, signed here; each taken as [`take`]
/// takes it, in that order. Both are read in one job on the compute pool (the receipt signed in it, and its hop checked
/// with the hand-on as it was read, which is what kiln keeps of it, kept now), and kept in one call to the store's thread,
/// one write: two waits for the shard where [`take`] and [`sign_and_take`] made five, and one write where they made two
/// (each write a cluster group entry for every learner). What isn't the usual goes one at a time, as [`take`] has it: either
/// refused, a revocation added since, the receipt's hop needing more than the hand-on, or the hand-on kept before (its
/// copy kiln keeps may be signed apart). Each one's outcome, the receipt's an error when it couldn't be signed (the hand-on
/// taken all the same); an error for both only when the store's revocations couldn't be read.
pub async fn take_batch(store: &Store, pki: &std::sync::Arc<crate::pki::Pki>, given: Value, came_from: &str, receipt: statement::Statement, at: i64) -> anyhow::Result<(TakeOutcome, anyhow::Result<TakeOutcome>)> {
    let revoked = crate::identities::revoked(store).await?;
    let org = pki.organization();
    let given = std::sync::Arc::new(given);
    let (signer, revocations, came, of, hand_on) = (std::sync::Arc::clone(pki), std::sync::Arc::clone(&revoked.list), came_from.to_owned(), org.clone(), std::sync::Arc::clone(&given));
    let (read_given, signed) = crate::rt::compute(move || {
        let (read_given, given_checked) = read_keeping(&hand_on, &signer.ca_der, &revocations, came, at, &of, true);
        let signed = server_signed(&signer, &receipt).map(|b| {
            let signed = std::sync::Arc::new(b);
            let read_receipt = read_now(&signed, &signer.ca_der, &revocations, "kiln".to_owned(), at, &of);
            // The receipt's hop with the hand-on as it checked: what kiln keeps of it once kept now, read back.
            let together = match (&read_receipt.verdict, given_checked) {
                (Verdict::Custody { own: Own::Waits { checked, wanted }, .. }, Some(g)) if wanted.len() == 1 && wanted.first() == Some(&g.digest) => {
                    let map = BTreeMap::from([(g.digest.clone(), g)]);
                    Some(broken(&checked.digest, (**checked).clone(), map, wanted, &of))
                }
                _ => None,
            };
            (signed, read_receipt, together)
        });
        (read_given, signed)
    })
    .await?;
    let root = std::sync::Arc::<[u8]>::from(&*pki.ca_der);
    let (signed, read_receipt, together) = match signed {
        Ok(signed) => signed,
        // Not signed: the hand-on is taken alone, as before kiln signed its receipt in the same job.
        Err(e) => return Ok((take_read(store, &root, &org, given, Some((read_given, revoked)), came_from, at).await, Err(e))),
    };
    let (given_read, receipt_read) = match (read_given, read_receipt) {
        (Read { digest: d1, verdict: Verdict::Custody { kind: k1, rows: r1, own: o1 } }, Read { digest: d2, verdict: Verdict::Custody { kind: k2, rows: r2, own: o2 } }) => ((d1, k1, r1, o1), (d2, k2, r2, o2)),
        // Either refused: each taken alone, as `take` takes it, from what was read.
        (read_given, read_receipt) => {
            let first = take_read(store, &root, &org, given, Some((read_given, std::sync::Arc::clone(&revoked))), came_from, at).await;
            let second = take_read(store, &root, &org, signed, Some((read_receipt, revoked)), "kiln", at).await;
            return Ok((first, Ok(second)));
        }
    };
    let ((given_digest, given_kind, given_rows, given_own), (receipt_digest, receipt_kind, receipt_rows, receipt_own)) = (given_read, receipt_read);
    let first = Keeping { digest: given_digest, rows: *given_rows, wanted: given_own.wanted().to_vec(), delegation: given_kind.is_none() };
    let second = Keeping { digest: receipt_digest, rows: *receipt_rows, wanted: receipt_own.wanted().to_vec(), delegation: receipt_kind.is_none() };
    let stamp = revoked.stamp;
    let both = match store.call(move |c| keep_both(c, stamp, first, second)).await {
        Ok(both) => both,
        Err(e) => {
            let second = anyhow::anyhow!("{e:#}");
            return Ok((Err(e), Ok(Err(second))));
        }
    };
    let Some((given_looked, receipt_looked)) = both else {
        // A revocation was added since they were read: each read again and taken alone, as `take` takes it.
        let first = take_read(store, &root, &org, given, None, came_from, at).await;
        let second = take_read(store, &root, &org, signed, None, "kiln", at).await;
        return Ok((first, Ok(second)));
    };
    // The hand-on kept just now is what the receipt's hop was checked with; one kept before is read back, as `take` reads it.
    let together = together.filter(|_| matches!(given_looked, Looked::New { .. }));
    let first = taken_now(store, &root, &org, &revoked, given_own, None, given_looked, at).await;
    let second = taken_now(store, &root, &org, &revoked, receipt_own, together, receipt_looked, at).await;
    Ok((first, Ok(second)))
}

/// What became of one record [`keep_both`] kept or found ([`take_batch`]): settled when new ([`settle`]).
#[allow(clippy::too_many_arguments)]
async fn taken_now(store: &Store, root: &std::sync::Arc<[u8]>, org: &str, revoked: &crate::identities::Revoked, own: Own, found: Option<Option<Broken>>, looked: Looked, at: i64) -> TakeOutcome {
    match looked {
        Looked::New { taken, wanted, waiting } => settle(store, root, org, revoked, own, found, taken, wanted, waiting, at).await.map(Ok),
        Looked::Again(digest) => Ok(Ok(Taken { digest, again: true, broke: None })),
        // Not one `keep_both` says of a record: it looks at the revocations for both, first.
        Looked::Stale => Err(anyhow::anyhow!("its keeping found the revocations stale")),
    }
}

/// kiln's server's own record `s` (a receipt of a feed's batch, a hand-on of one), signed and read for [`take`] in one
/// job on the compute pool, then kept as [`take`] keeps a record (goal 9, plan 01a1a87b2cec step 5: signed off the shards
/// and then read, it was two jobs, each a park and a wake of the shard that waited). The bundle signed, and what became
/// of it; an error only when it couldn't be signed (or the store's revocations couldn't be read).
pub async fn sign_and_take(store: &Store, pki: &std::sync::Arc<crate::pki::Pki>, s: statement::Statement, came_from: &str, at: i64) -> anyhow::Result<(Value, anyhow::Result<Result<Taken, Refused>>)> {
    let revoked = crate::identities::revoked(store).await?;
    let org = pki.organization();
    let (signer, revocations, came, of) = (std::sync::Arc::clone(pki), std::sync::Arc::clone(&revoked.list), came_from.to_owned(), org.clone());
    let (bundle, read, checked) = crate::rt::compute(move || -> anyhow::Result<(std::sync::Arc<Value>, Read, Option<Checked>)> {
        let bundle = std::sync::Arc::new(server_signed(&signer, &s)?);
        let (read, checked) = read_keeping(&bundle, &signer.ca_der, &revocations, came, at, &of, true);
        Ok((bundle, read, checked))
    })
    .await??;
    let taken = take_read(store, &pki.ca_der, &org, std::sync::Arc::clone(&bundle), Some((read, revoked)), came_from, at).await;
    // Kept (or kept before), as it checked: what a receipt of it, soon after, has its hop checked with (`recent`).
    if let (Ok(Ok(_)), Some(checked)) = (&taken, checked) {
        recent::keep(checked);
    }
    Ok((std::sync::Arc::unwrap_or_clone(bundle), taken))
}

/// What checked statement `c` is to kiln: a custody record or a delegation, with its rows and its own hop made ready
/// ([`Own`]); or not one kiln reads.
fn verdict_of(c: Checked, envelope: String, came_from: String, org: &str) -> Verdict {
    let (kind, counterpart) = match c.payload_type.as_str() {
        PAYLOAD_CUSTODY => match custody::body(&c) {
            Ok(b) => (Some(b.kind), if b.kind == Kind::Received { b.from } else { b.to }),
            Err(e) => return Verdict::NotCustody { act: "a custody record", why: e.to_string(), custodian: c.custodian, envelope },
        },
        PAYLOAD_DELEGATION => match custody::grant(&c) {
            Ok(g) => (None, Some(g.delegate)),
            Err(why) => return Verdict::NotCustody { act: "a delegation", why, custodian: c.custodian, envelope },
        },
        other => {
            let why = format!("it's a {other} statement, not a custody record or a delegation");
            return Verdict::NotCustody { act: "a signed statement", why, custodian: c.custodian, envelope };
        }
    };
    let made = Box::new(rows(&c, kind, counterpart, envelope, came_from));
    let own = match kind {
        None => Own::Unchecked,
        Some(_) => {
            let wanted = wanted_by(&c);
            if wanted.is_empty() {
                let d = c.digest.clone();
                Own::Alone(broken(&d, c, BTreeMap::new(), &[], org))
            } else {
                Own::Waits { checked: Box::new(c), wanted }
            }
        }
    };
    Verdict::Custody { kind, rows: made, own }
}

/// What the store's thread found as [`take`] kept a record ([`keep_and_look`]).
enum Looked {
    /// A revocation was added since the record was checked: not kept; read them again and check it again.
    Stale,
    /// kiln has it already, by this digest.
    Again(String),
    /// Kept, by its digest: the rows of the records its hop needs that kiln has ([`rows_on`]), and the records that
    /// waited for it (a custody record's followers, a delegation's records taken under it).
    New { taken: String, wanted: Vec<(String, String, i64)>, waiting: Vec<String> },
}

/// On the store's thread, in one call ([`take`]): whether the revocations are still those stamped `stamp` (none: not
/// looked at), whether kiln has the statement by `digest` already, then `rows` kept, the rows of the records in `wanted`
/// read, and the records that waited for it found: a custody record's followers, or a `delegation`'s records under it.
fn keep_and_look(c: &mut rusqlite::Connection, stamp: Option<crate::identities::Stamp>, digest: &str, rows: Rows, wanted: &[String], delegation: bool) -> anyhow::Result<Looked> {
    // One kiln has is answered as such first, whatever the revocations now (as it was judged when taken).
    if has(c, digest)? {
        return Ok(Looked::Again(digest.to_owned()));
    }
    if stale(c, stamp)? {
        return Ok(Looked::Stale);
    }
    keep_looking(c, rows, wanted, delegation)
}

/// Whether kiln has the statement by `digest` ("": one that couldn't be read, which it hasn't).
fn has(c: &rusqlite::Connection, digest: &str) -> rusqlite::Result<bool> {
    Ok(!digest.is_empty() && kept_on(c, digest)?)
}

/// Whether a revocation was added since the revocations stamped `stamp` were read (none: not looked at).
fn stale(c: &rusqlite::Connection, stamp: Option<crate::identities::Stamp>) -> rusqlite::Result<bool> {
    Ok(match stamp {
        Some(s) => crate::identities::revoked_stamp(c)? != s,
        None => false,
    })
}

/// `rows` kept, then what [`Looked::New`] says of them: the rows of `wanted`, and the records that waited for it.
fn keep_looking(c: &rusqlite::Connection, rows: Rows, wanted: &[String], delegation: bool) -> anyhow::Result<Looked> {
    let taken = rows.digest.clone();
    if !keep_on(c, rows)? {
        return Ok(Looked::Again(taken));
    }
    let waiting = if delegation { under_on(c, &taken)? } else { followers_on(c, &taken)? };
    Ok(Looked::New { wanted: rows_on(c, wanted)?, waiting, taken })
}

/// One record as [`keep_both`] keeps it: the digest it's read by, its rows, the records its hop needs, and whether it's a
/// delegation.
struct Keeping {
    digest: String,
    rows: Rows,
    wanted: Vec<String>,
    delegation: bool,
}

/// [`keep_and_look`] for two records in turn, in one call and one write ([`take_batch`]): each one kiln has is answered as
/// such; then, if either is new, none is kept when a revocation was added since `stamp` (none returned); else each new one
/// kept and looked around in turn, the first's followers found before the second is kept.
fn keep_both(c: &mut rusqlite::Connection, stamp: crate::identities::Stamp, first: Keeping, second: Keeping) -> anyhow::Result<Option<(Looked, Looked)>> {
    let (had_first, had_second) = (has(c, &first.digest)?, has(c, &second.digest)?);
    if !(had_first && had_second) && stale(c, Some(stamp))? {
        return Ok(None);
    }
    let keep = |had: bool, k: Keeping| if had { Ok(Looked::Again(k.digest)) } else { keep_looking(c, k.rows, &k.wanted, k.delegation) };
    let first = keep(had_first, first)?;
    let second = keep(had_second, second)?;
    Ok(Some((first, second)))
}

/// The records that act under delegation `digest`.
fn under_on(c: &rusqlite::Connection, digest: &str) -> rusqlite::Result<Vec<String>> {
    let mut q = c.prepare_cached("SELECT digest FROM custody WHERE under = ?1")?;
    q.query_map([digest], |r| r.get(0))?.collect()
}

/// A custody record a runner process sent ([`crate::proto::AgentMsg::Custody`]): taken as having
/// come over that process's connection (`instance`, as it was named when the record came; none, one
/// from before processes said so), and logged either way. Runners' records come here one at a time,
/// in the order they came, off the hub's loop ([`runners::intake_loop`]; goal 9, plan 01a1a87b2cec
/// step 3).
pub async fn on_runner(hub: &crate::hub::Hub, machine: &str, instance: Option<String>, bundle: Value) {
    let instance = instance.unwrap_or_else(|| "legacy".to_owned());
    let came_from = format!("runner {machine} (instance {instance})");
    // Something another runner handed it over a reach link that it refused where it would have
    // used it (W27-R08: a sync's workspace, a run, a run's output not what was handed on, or with
    // no hand-on or receipt of it).
    if let Some(r) = bundle.get("refused_hop") {
        let text = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let why = format!("{} from {}: {}", text("what"), text("from"), text("why"));
        let refusal = Refusal { act: "a hand-over between machines".into(), why: why.clone(), came_from, at: crate::util::now_unix(), ..Refusal::default() };
        if let Err(e) = refuse(&hub.store, refusal).await {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "a refused hand-over between machines couldn't be recorded");
        }
        return tracing::info!(target: "kiln.custody", %machine, %instance, %why, "a runner process refused what another handed it");
    }
    // An act of one of its sessions it didn't do, kiln being out of reach (W27-R08: a machine cut
    // off, as the twin's was).
    if let Some(r) = bundle.get("refused_act") {
        let text = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let op = r.get("op").and_then(Value::as_str).unwrap_or("call");
        let ask = json!({ "op": op, "name": text("name"), "act": r.get("act") });
        if let Err(e) = refused_act(hub, machine, &instance, &text("session"), &ask, &text("why"), crate::util::now_unix()).await {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "an act a runner didn't do couldn't be recorded");
        }
        return tracing::info!(target: "kiln.custody", %machine, %instance, session = %text("session"), why = %text("why"), "a runner process didn't do an act: kiln was out of its reach");
    }
    // A break it found in what a relay passed on to it, and what it refused because of it (W27-R08's
    // relays): kiln's words, a fellow agent's, blue/green's frames, a reach link's.
    if let Some(r) = bundle.get("relay_break") {
        if let Err(e) = crate::relays::reported(hub, machine, &instance, r).await {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "a break a runner found in what a relay passed on couldn't be recorded");
        }
        let said = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let (relay, detail) = (said("relay"), said("detail"));
        return tracing::info!(target: "kiln.custody", %machine, %instance, %relay, %detail, "a runner process found a break in what a relay passed on");
    }
    // A hand-over's word it refused, not being the process it was for (W27-R08; the 14:18 drop).
    if let Some(r) = bundle.get("refused_word") {
        let text = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let word: Option<Value> = r.get("word").and_then(Value::as_str).and_then(|w| serde_json::from_str(w).ok());
        // Who signed it, read on the compute pool (goal 9, plan 01a1a87b2cec step 3).
        let signer = match &word {
            Some(w) => verify_off(w, &hub.pki.ca_der, Vec::new(), crate::util::now_unix()).await.ok().and_then(Result::ok).map(|c| c.custodian).unwrap_or_default(),
            None => String::new(),
        };
        let why = format!("session {}: {}", text("session"), text("why"));
        let refusal = Refusal { digest: word.as_ref().map(digest_of).unwrap_or_default(), act: "a hand-over's word".into(), signer, why: why.clone(), envelope: word.map(|w| w.to_string()).unwrap_or_default(), came_from, at: crate::util::now_unix(), ..Refusal::default() };
        if let Err(e) = refuse(&hub.store, refusal).await {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "a refused hand-over's word couldn't be recorded");
        }
        return tracing::info!(target: "kiln.custody", %machine, %instance, %why, "a runner process refused a hand-over's word not for it");
    }
    match take_owned(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), bundle, &came_from, crate::util::now_unix()).await {
        Ok(Ok(t)) => tracing::info!(target: "kiln.custody", %machine, %instance, digest = %t.digest, again = t.again, broke = t.broke.as_deref().unwrap_or(""), "took a custody record"),
        Ok(Err(why)) => tracing::info!(target: "kiln.custody", %machine, %instance, %why, "refused a custody record"),
        Err(e) => tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "couldn't keep a custody record"),
    }
}

/// `s` signed by kiln's server with its own signing key (`<data>/pki/server-signing.key`, which
/// `identities::own_identities` keeps certified), its body naming it: its bundle. Only kiln's
/// server signs takes.
pub fn server_signed(pki: &crate::pki::Pki, s: &statement::Statement) -> anyhow::Result<Value> {
    server_signed_as(pki, PAYLOAD_CUSTODY, s)
}

/// [`server_signed`] on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3): a
/// signature reads kiln's signing key and certificate and signs, which a shard did for each batch of
/// every conversation feed it took or handed on.
pub async fn server_signed_off(pki: &std::sync::Arc<crate::pki::Pki>, s: statement::Statement) -> anyhow::Result<Value> {
    let pki = std::sync::Arc::clone(pki);
    crate::rt::compute(move || server_signed(&pki, &s)).await?
}

/// [`server_signed`], as `payload_type`.
fn server_signed_as(pki: &crate::pki::Pki, payload_type: &str, s: &statement::Statement) -> anyhow::Result<Value> {
    own_signed_as(pki, "server", &server_uri(&pki.organization()), payload_type, s)
}

/// kiln's CD's identity in organization `org`.
pub fn cd_uri(org: &str) -> String {
    Identity::new(org, Actor::Cd).uri().unwrap_or_default()
}

/// `s` signed by kiln's CD with its own signing key (`<data>/pki/cd-signing.key`, kept certified
/// beside the server's), its body naming it: its bundle. CD signs a promotion's records (docs/ci.md
/// W27-R07, R08: `crate::deploy::promotion`).
pub fn cd_signed(pki: &crate::pki::Pki, s: &statement::Statement) -> anyhow::Result<Value> {
    own_signed_as(pki, "cd", &cd_uri(&pki.organization()), PAYLOAD_CUSTODY, s)
}

/// [`cd_signed`] on the compute pool, off the shards, as [`server_signed_off`] (goal 9, plan 01a1a87b2cec step 3): CD's
/// key and certificate read from their files, and the signature, for each sync's records and each image it holds.
pub async fn cd_signed_off(pki: &std::sync::Arc<crate::pki::Pki>, s: statement::Statement) -> anyhow::Result<Value> {
    let pki = std::sync::Arc::clone(pki);
    crate::rt::compute(move || cd_signed(&pki, &s)).await?
}

/// `s` signed as `payload_type` with the key of kiln's own identity `kind` (`server`, `cd`), whose
/// URI is `by`, its body naming it.
fn own_signed_as(pki: &crate::pki::Pki, kind: &str, by: &str, payload_type: &str, s: &statement::Statement) -> anyhow::Result<Value> {
    use anyhow::Context as _;
    use rcgen::SigningKey as _;
    let read = |f: &str| std::fs::read_to_string(pki.dir().join(f)).with_context(|| format!("kiln's {kind}'s {f}"));
    let key = rcgen::KeyPair::from_pem(&read(&format!("{kind}-signing.key"))?)?;
    let cert = read(&format!("{kind}-signing.pem"))?;
    let mut s = s.clone();
    if let Some(o) = s.body.as_object_mut() {
        o.insert("by".into(), Value::String(by.to_owned()));
    }
    let payload = statement::payload(&s)?;
    let signature = key.sign(&statement::to_sign(payload_type, &payload))?;
    Ok(statement::bundle(payload_type, &payload, &signature, &crate::pki::der_of(&cert)?))
}

/// A hand-over's word (`command`, a session command's JSON) as kiln's server sends it to runner
/// process `instance` of `machine`, which holds session `session` at `epoch` as kiln knows it
/// (docs/ci.md W27-R08; the Transfers plan's step 4): the command with the server's signature over
/// it and whom it's for (`kiln_ci::identity::custody::Word`) beside it, as `word`. A runner from
/// before reads the command alone. What's signed is the command as [`unworded`] writes it.
pub fn word(pki: &crate::pki::Pki, session: &str, command: &str, machine: &str, instance: &str, epoch: u64) -> anyhow::Result<String> {
    word_signed(&|payload_type, s| server_signed_as(pki, payload_type, s), session, command, machine, instance, epoch)
}

/// [`word`] on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3): the server's key and certificate
/// read from their files, and the signature, for each hand-over's word kiln sends a runner process.
pub async fn word_off(pki: &std::sync::Arc<crate::pki::Pki>, session: &str, command: &str, machine: &str, instance: &str, epoch: u64) -> anyhow::Result<String> {
    let (pki, session, command, machine, instance) = (std::sync::Arc::clone(pki), session.to_owned(), command.to_owned(), machine.to_owned(), instance.to_owned());
    crate::rt::compute(move || word(&pki, &session, &command, &machine, &instance, epoch)).await?
}

/// [`word`], signed by `sign` (kiln's server's key: a payload type and a statement to its bundle),
/// as the transfers simulation (`crate::sim::transfers`) signs one with its own server's.
pub fn word_signed(sign: &dyn Fn(&str, &statement::Statement) -> anyhow::Result<Value>, session: &str, command: &str, machine: &str, instance: &str, epoch: u64) -> anyhow::Result<String> {
    use sha2::Digest as _;
    let mut v: Value = serde_json::from_str(command)?;
    let digest: String = sha2::Sha256::digest(unworded(&v).as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let body = serde_json::to_value(custody::Word { v: statement::VERSION, by: String::new(), session: session.to_owned(), machine: machine.to_owned(), instance: instance.to_owned(), epoch })?;
    let subject = statement::Subject { name: format!("a hand-over's word about session {session}"), digest: BTreeMap::from([("sha256".to_owned(), digest)]), ..statement::Subject::default() };
    let bundle = sign(custody::PAYLOAD_WORD, &statement::Statement { subject: vec![subject], ..statement::Statement::new(body) })?;
    if let Some(o) = v.as_object_mut() {
        o.insert("word".into(), Value::String(bundle.to_string()));
    }
    Ok(v.to_string())
}

/// Keeps the word kiln's server signed beside command `json` ([`word`]), so a walk through a relay
/// that passes it on (blue/green's old build: docs/ci.md W27-R08's relays) reaches it.
pub(crate) async fn keep_word(store: &Store, root: &[u8], json: &str) {
    let word = serde_json::from_str::<Value>(json).ok().and_then(|v| v.get("word").and_then(Value::as_str).and_then(|w| serde_json::from_str::<Value>(w).ok()));
    let Some(bundle) = word else { return };
    if let Err(e) = keep_originator(store, root, &bundle, "kiln's server", crate::util::now_unix()).await {
        tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "a hand-over's word kiln signed wasn't kept");
    }
}

/// The command a hand-over's word signs (`v`, as sent): its JSON without the word, as `serde_json`
/// writes it again, which the server and the runner write alike.
pub fn unworded(v: &Value) -> String {
    without(v, "word")
}

/// `v` without its `key` (a signature beside it), as `serde_json` writes it again: what that
/// signature signs, which both sides write alike.
pub fn without(v: &Value, key: &str) -> String {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove(key);
    }
    v.to_string()
}

/// What an agent session's act signs of ask `v`, as sent: the ask without the act beside it, nor
/// the runner's record of passing it on (`passed_on`, added after the act was signed: docs/ci.md
/// W27-R08's relays), as `serde_json` writes it again.
pub fn signed_part(v: &Value) -> String {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("act");
        o.remove("passed_on");
    }
    v.to_string()
}

/// Whether ask `ask`, which runner process `instance` of `machine` sends kiln for agent session
/// `session` (a kiln operation, a page it shows, a run it asks before doing:
/// `crate::mcp::runner_ask`), is the session's act now (docs/ci.md W27-R08; the Transfers plan's
/// step 5, the twin). Once kiln has certified a holder at the session's epoch, it's the session's
/// only if it's signed beside it (`act`) by that holder at that epoch
/// (`kiln_ci::identity::custody::acted`): a copy left behind signs at an epoch whose key kiln
/// revoked as the session went on, and a runner process that signs acts (`signs`: `CAP_CUSTODY`)
/// and that kiln certified for the session can't send one unsigned. One that isn't certified for it
/// yet (it just took it over), or a runner from before, goes by its machine, as before.
/// Why not, each refusal recorded with the act's envelope, the epoch it's from and the transfer
/// that superseded it.
pub async fn act_counts(hub: &crate::hub::Hub, machine: &str, instance: &str, signs: bool, session: &str, ask: &Value, at: i64) -> anyhow::Result<Result<(), String>> {
    // The session as its transfers say (the newest epoch kiln certified it at, and its holder
    // then), not as an Open says: what a process announces doesn't fence out the holder.
    let Some(held) = held_now(&hub.store, session).await? else { return Ok(Ok(())) };
    let came_from = format!("runner {machine} (instance {instance})");
    let record = |signer: String, epoch: Option<u64>, why: String, act: Option<&Value>| Refusal {
        digest: act.map(digest_of).unwrap_or_default(),
        act: format!("an act of session {session}: {}", op_of(ask)),
        signer,
        why,
        envelope: act.map(Value::to_string).unwrap_or_default(),
        came_from: came_from.clone(),
        at,
        superseded_by: if epoch.is_some_and(|e| e < held.epoch) { held.transfer.clone() } else { String::new() },
        epoch,
    };
    let Some(act) = ask.get("act").and_then(Value::as_str).and_then(|a| serde_json::from_str::<Value>(a).ok()) else {
        // Unsigned: from a process that signs acts and that kiln certified for the session, it's
        // refused (at the session's epoch it holds the key; at one the session left, it's a copy
        // left behind); from one kiln hasn't certified for it (it just took it over, its
        // certificate on its way) or a runner from before, it goes by its machine, as before.
        let held_at = if signs { crate::identities::epoch_held(&hub.store, session, machine, instance).await? } else { None };
        let Some(e) = held_at else { return Ok(Ok(())) };
        let why = if e < held.epoch {
            format!("it isn't signed, from a runner process that held session {session} at epoch {e}, which {}: it's from a copy left behind", held.went_on())
        } else {
            format!("it isn't signed, from a runner process that signs its sessions' acts and holds session {session}'s key at epoch {e}")
        };
        refuse(&hub.store, record(String::new(), Some(e), why.clone(), None)).await?;
        return Ok(Err(why));
    };
    let what = signed_part(ask);
    // Checked on the compute pool, against every revocation read now, which is let go of there: a signed act's two
    // signature checks (one again without the revocations when it's refused) and the whole list's free were each of a
    // runner's agents' signed asks' on its shard (goal 9, plan 01a1a87b2cec step 3).
    let revocations = crate::identities::revocations(&hub.store).await?;
    let (pki, holder) = (std::sync::Arc::clone(&hub.pki), held.clone());
    let (checked, act) = crate::rt::compute(move || (act_check(&act, &pki.ca_der, &revocations, &holder, what.as_bytes(), at), act)).await?;
    match checked {
        Ok(()) => Ok(Ok(())),
        Err((signer, why)) => {
            refuse(&hub.store, record(signer.clone(), epoch_of(&signer), why.clone(), Some(&act))).await?;
            Ok(Err(why))
        }
    }
}

/// The server's check of a signed act (`act`, a bundle) of agent session `held.session` (docs/ci.md
/// W27-R08): signed under kiln's root (`root`) by an identity not revoked as of `at`
/// (`revocations`), and the act of its holder now ([`custody::acted`], as of `what`: the ask it
/// was signed beside, without it). [`act_counts`] runs it, and so does the transfers simulation
/// (`crate::sim::transfers`). Who signed it (revocations aside) and why not, when it doesn't count.
pub fn act_check(act: &Value, root: &[u8], revocations: &[Revocation], held: &custody::Held, what: &[u8], at: i64) -> Result<(), (String, String)> {
    let checked = match statement::verify(act, root, revocations, at) {
        Ok(c) => c,
        Err(e) => {
            // Its signer's identity revoked: a copy left behind signs at the epoch it left, which
            // kiln revoked as the session went on. Who signed it, revocations aside, and why the
            // fence says it doesn't count.
            let signed = statement::verify(act, root, &[], at).ok();
            let why = match signed.as_ref().map(|c| custody::acted(c, held, what)) {
                Some(Err(fenced @ custody::Unacted::Fenced(_))) => fenced.to_string(),
                _ => format!("session {}'s act isn't signed as its holder now: {e}", held.session),
            };
            return Err((signed.map(|c| c.custodian).unwrap_or_default(), why));
        }
    };
    custody::acted(&checked, held, what).map(|_| ()).map_err(|e| (checked.custodian.clone(), e.to_string()))
}

/// Act `ask` of agent session `session`, from runner process `instance` of `machine`, refused
/// before it was checked as an act (`why`: the session went on elsewhere, so this is a copy left
/// behind; its runner, out of kiln's reach, didn't do it): recorded with its envelope, who signed it
/// and at which epoch (revocations aside), and the transfer the session went on with if that's an
/// epoch it left (docs/ci.md W27-R08). Why, saying where the session went on.
pub async fn refused_act(hub: &crate::hub::Hub, machine: &str, instance: &str, session: &str, ask: &Value, why: &str, at: i64) -> anyhow::Result<String> {
    refused_act_of(hub, machine, instance, session, ask, why, None, at).await
}

/// [`refused_act`], of work agent session `session` held that changed hands since (a plan's step
/// tried again with another agent, or taken over by a person): recorded as superseded by
/// `transfer`, that work's hand-over (docs/ci.md W27-R08; the Transfers plan's step 6).
#[allow(clippy::too_many_arguments)]
pub async fn refused_work_act(hub: &crate::hub::Hub, machine: &str, instance: &str, session: &str, ask: &Value, why: &str, transfer: &str, at: i64) -> anyhow::Result<String> {
    refused_act_of(hub, machine, instance, session, ask, why, Some(transfer.to_owned()), at).await
}

#[allow(clippy::too_many_arguments)]
async fn refused_act_of(hub: &crate::hub::Hub, machine: &str, instance: &str, session: &str, ask: &Value, why: &str, transfer: Option<String>, at: i64) -> anyhow::Result<String> {
    let act = ask.get("act").and_then(Value::as_str).and_then(|a| serde_json::from_str::<Value>(a).ok());
    // Who signed it, revocations aside, read on the compute pool (goal 9, plan 01a1a87b2cec step 3).
    let signer = match &act {
        Some(a) => verify_off(a, &hub.pki.ca_der, Vec::new(), at).await.ok().and_then(Result::ok).map(|c| c.custodian).unwrap_or_default(),
        None => String::new(),
    };
    // The epoch it's from: the one it's signed at, or, unsigned, the newest kiln certified this
    // process for the session at.
    let epoch = match epoch_of(&signer) {
        Some(e) => Some(e),
        None => crate::identities::epoch_held(&hub.store, session, machine, instance).await?,
    };
    let (why, superseded_by) = match (transfer, held_now(&hub.store, session).await?, epoch) {
        (Some(t), _, _) => (why.to_owned(), t),
        (None, Some(h), Some(e)) if e < h.epoch => (format!("{why}: it's from epoch {e} of session {session}, which {}", h.went_on()), h.transfer),
        _ => (why.to_owned(), String::new()),
    };
    let refusal = Refusal {
        digest: act.as_ref().map(digest_of).unwrap_or_default(),
        act: format!("an act of session {session}: {}", op_of(ask)),
        signer,
        why: why.clone(),
        envelope: act.map(|a| a.to_string()).unwrap_or_default(),
        came_from: format!("runner {machine} (instance {instance})"),
        at,
        epoch,
        superseded_by,
    };
    refuse(&hub.store, refusal).await?;
    Ok(why)
}

/// What ask `ask` is: "call goals.done", "act run", "page".
fn op_of(ask: &Value) -> String {
    match (ask.get("op").and_then(Value::as_str), ask.get("name").and_then(Value::as_str)) {
        (Some(op), Some(name)) => format!("{op} {name}"),
        (Some(op), None) => op.to_owned(),
        _ => "an ask".to_owned(),
    }
}

/// The epoch agent identity `signer` signs at; None for another identity.
fn epoch_of(signer: &str) -> Option<u64> {
    Identity::parse(signer).ok().and_then(|i| match i.actor {
        Actor::Agent { epoch, .. } => Some(epoch),
        _ => None,
    })
}

/// Who holds agent session `session` now, as its transfers say: the newest epoch kiln certified it
/// at, the runner process it certified then and when, and kiln's record of the transfer that
/// process took it with (its receipt of the session's timeline, [`transfer_receipt`]; never a
/// receipt of work it took up since). None before kiln certified any (W27-R08). Each signed act
/// asks it, and its reads are beside the writer (the DB query audit, 2026-11-06): a transfer's
/// receipt and certificate are kept before its process is given the certificate, so before it
/// signs anything, as the revocations its act is checked against are read (`identities::revoked_current`).
async fn held_now(store: &Store, session: &str) -> anyhow::Result<Option<custody::Held>> {
    let Some((epoch, holder)) = crate::identities::newest_holder(store, session).await? else { return Ok(None) };
    let by = Identity::parse(&holder).ok().and_then(|i| match i.actor {
        Actor::Agent { held_by, .. } => held_by,
        _ => None,
    });
    Ok(Some(custody::Held { session: session.to_owned(), epoch, by, since: issued_at(store, &holder).await?.unwrap_or_default(), transfer: transfer_receipt(store, &holder).await?.unwrap_or_default() }))
}

/// The epoch agent identity `signer` signs at, and the transfer its session went on with if that's
/// an epoch the session left (a copy left behind's): what a refusal of what it signed records.
async fn left(store: &Store, signer: &str) -> anyhow::Result<(Option<u64>, String)> {
    let Some((session, epoch)) = Identity::parse(signer).ok().and_then(|i| match i.actor {
        Actor::Agent { agent, epoch, .. } => Some((agent, epoch)),
        _ => None,
    }) else {
        return Ok((None, String::new()));
    };
    let held = held_now(store, &session).await?;
    Ok((Some(epoch), held.filter(|h| epoch < h.epoch).map(|h| h.transfer).unwrap_or_default()))
}

/// When kiln issued its newest certificate for identity `subject`.
async fn issued_at(store: &Store, subject: &str) -> anyhow::Result<Option<i64>> {
    let s = subject.to_owned();
    store.read(move |c| Ok(c.prepare_cached("SELECT MAX(issued_at) FROM identities WHERE subject = ?1")?.query_row([&s], |r| r.get::<_, Option<i64>>(0)).optional()?.flatten())).await
}

/// kiln's take of agent session `session` as it leaves epoch `epoch` (its timeline `events`), for
/// the process that held it there, which can't hand it on (`why`: its machine is away, and since
/// when): to the session at the next epoch, whichever process takes it, following that process's
/// receipt of it if kiln has one ([`transfer_receipt`]). Kept here at once, and carried in the session's package: its
/// receipt follows it as a hand-on's does (docs/ci.md W27-R08). None if kiln never certified the
/// session at that epoch: it goes on unsigned, as before. `timeline` is the session's timeline as it leaves that epoch
/// ([`timeline`]), worked out where its package is made, off the shards: it's every event's digest (goal 9, plan
/// 01a1a87b2cec step 3).
pub async fn take_for(store: &Store, pki: &std::sync::Arc<crate::pki::Pki>, session: &str, epoch: u64, timeline: statement::Subject, why: &str) -> anyhow::Result<Option<Value>> {
    let Some(holder) = crate::identities::holder_at(store, session, epoch).await? else { return Ok(None) };
    let org = pki.organization();
    let to = at_epoch(&holder, epoch.saturating_add(1)).ok_or_else(|| anyhow::anyhow!("{holder} isn't an agent session's identity"))?;
    let body = custody::Body { taken_for: Some(holder.clone()), to: Some(to), why: why.to_owned(), epoch: Some(epoch.saturating_add(1)), ..custody::Body::new(Kind::Taken, &server_uri(&org)) };
    let follows = transfer_receipt(store, &holder).await?.into_iter().collect();
    let s = statement::Statement { subject: vec![timeline], follows, ..statement::Statement::new(custody::to_value(&body)) };
    // Signed on the compute pool: the server's key and certificate read from their files, and the signature, were the
    // owner's shard's as a session's machine stayed away (goal 9, plan 01a1a87b2cec step 3).
    let bundle = server_signed_off(pki, s).await?;
    match take(store, &pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
        Ok(_) => Ok(Some(bundle)),
        Err(why) => anyhow::bail!("kiln refused its own take: {why}"),
    }
}

/// What a hand-over of work is about, by its digest (docs/ci.md W27-R08; the Transfers plan's step
/// 6): a plan's step, a subagent's task, what a person handed off.
pub const WORK: &str = "kiln-work-v1";

/// Work named `name` ("plan run <run> step <n>", "the task of agent <id>"), its digest over what it
/// is (`parts`: a step's run, number and goal; a task's words), which its giver and its taker work
/// out alike: the same work across every hand it goes through.
pub fn work(name: &str, parts: &[&str]) -> statement::Subject {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(u64::try_from(p.len()).unwrap_or(u64::MAX).to_be_bytes());
        h.update(p.as_bytes());
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    statement::Subject { name: name.to_owned(), digest: BTreeMap::from([(WORK.to_owned(), hex)]), ..statement::Subject::default() }
}

/// Person `name`'s identity in organization `org`.
pub fn person_uri(org: &str, name: &str) -> String {
    Identity::new(org, Actor::Person { name: name.to_owned() }).uri().unwrap_or_default()
}

/// How kiln's server hands work on: as itself (a plan run's driver), acting for those it names,
/// nearest first; or as a take, for a giver who can't sign (a retried step's earlier agent, a
/// step's agent as a person takes it over, a person with no key on the device that asked).
pub enum Giver {
    Kiln { acts_for: Vec<String> },
    For(String),
}

/// kiln's server hands `thing` on to `to` (an identity's URI: an agent session's at its epoch,
/// without the process holding its key, or a person's), for `why`, as `giver` says; a take follows
/// how its giver came to hold it, when kiln has that. Kept at once: its digest and bundle.
pub async fn hand_work(hub: &crate::hub::Hub, thing: statement::Subject, to: &str, why: &str, giver: Giver) -> anyhow::Result<(String, Value)> {
    let org = hub.pki.organization();
    let (kind, acts_for, taken_for) = match giver {
        Giver::Kiln { acts_for } => (Kind::HandedOn, acts_for, None),
        Giver::For(g) => (Kind::Taken, Vec::new(), Some(g)),
    };
    let follows = match &taken_for {
        Some(g) => held_with(&hub.store, &org, g, &thing).await?.into_iter().collect(),
        None => Vec::new(),
    };
    let body = custody::Body { to: Some(to.to_owned()), why: why.to_owned(), acts_for, taken_for, ..custody::Body::new(kind, &server_uri(&org)) };
    let s = statement::Statement { subject: vec![thing], follows, ..statement::Statement::new(custody::to_value(&body)) };
    // Signed on the compute pool, off the shards: the server's key and certificate read from their files, and the
    // signature (goal 9, plan 01a1a87b2cec step 3).
    let bundle = server_signed_off(&hub.pki, s).await?;
    match take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
        Ok(t) => Ok((t.digest, bundle)),
        Err(why) => anyhow::bail!("kiln refused its own hand-on: {why}"),
    }
}

/// kiln's server takes `thing` for `person` (their identity's URI), who has no key on the device
/// that asked: its receipt for them of hand-on `hand_on` (from `giver`), on the strength of their
/// signed-in session, saying their own signature is missing and why. A walk reads the link as
/// kiln's for them, never as theirs. Kept at once: its digest.
pub async fn receive_for(hub: &crate::hub::Hub, person: &str, thing: statement::Subject, hand_on: &str, giver: &str, why: &str) -> anyhow::Result<String> {
    receive_for_because(hub, person, thing, hand_on, giver, why, "no key on the device that asked").await
}

/// [`receive_for`], their own signature missing for the reason `missing` (a browser that can't
/// sign: "this page isn't secure (https), so this browser has no key").
pub async fn receive_for_because(hub: &crate::hub::Hub, person: &str, thing: statement::Subject, hand_on: &str, giver: &str, why: &str, missing: &str) -> anyhow::Result<String> {
    let org = hub.pki.organization();
    let why = format!("{why}: kiln's receipt for them, on their signed-in session; their own signature is missing: {missing}");
    let body = custody::Body { from: Some(giver.to_owned()), taken_for: Some(person.to_owned()), why, ..custody::Body::new(Kind::Received, &server_uri(&org)) };
    let s = statement::Statement { subject: vec![thing], follows: vec![hand_on.to_owned()], ..statement::Statement::new(custody::to_value(&body)) };
    // Signed on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3).
    let bundle = server_signed_off(&hub.pki, s).await?;
    match take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
        Ok(t) => Ok(t.digest),
        Err(why) => anyhow::bail!("kiln refused its own receipt for {person}: {why}"),
    }
}

/// Person `person`'s own receipt (`bundle`, signed on their device with the key `kiln login` made
/// them) of hand-on `hand_on`, which came over `came_from`: kept if it's theirs, and a receipt of
/// that hand-on; why not, if it isn't.
pub async fn person_receipt(hub: &crate::hub::Hub, person: &str, hand_on: &str, bundle: &Value, came_from: &str) -> anyhow::Result<Result<String, String>> {
    let at = crate::util::now_unix();
    let revocations = crate::identities::revocations(&hub.store).await?;
    // Checked on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3).
    let c = match verify_off(bundle, &hub.pki.ca_der, revocations, at).await? {
        Ok(c) => c,
        Err(e) => return Ok(Err(format!("the receipt doesn't verify: {e}"))),
    };
    if !custody::names(person, &c.custodian) {
        return Ok(Err(format!("the receipt is signed by {}, not by {person}", c.custodian)));
    }
    let is_receipt = custody::body(&c).is_ok_and(|b| b.kind == Kind::Received);
    if !is_receipt || !c.statement.follows.iter().any(|f| f == hand_on) {
        return Ok(Err(format!("it isn't a receipt of {hand_on}")));
    }
    Ok(take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), bundle, came_from, at).await?.map(|t| t.digest).map_err(|r| r.to_string()))
}

/// Person `person`'s own hand-on (`bundle`, signed on their device with the key `kiln login` made
/// them) of `thing` to `to`, which came over `came_from`: kept if it's theirs, of that work, to
/// that taker; why not, if it isn't.
pub async fn person_hand_on(hub: &crate::hub::Hub, person: &str, to: &str, thing: &statement::Subject, bundle: &Value, came_from: &str) -> anyhow::Result<Result<String, String>> {
    let at = crate::util::now_unix();
    let revocations = crate::identities::revocations(&hub.store).await?;
    // Checked on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3).
    let c = match verify_off(bundle, &hub.pki.ca_der, revocations, at).await? {
        Ok(c) => c,
        Err(e) => return Ok(Err(format!("the hand-on doesn't verify: {e}"))),
    };
    if !custody::names(person, &c.custodian) {
        return Ok(Err(format!("the hand-on is signed by {}, not by {person}", c.custodian)));
    }
    let of_it = custody::body(&c).is_ok_and(|b| b.kind == Kind::HandedOn && b.to.as_deref().is_some_and(|t| custody::names(t, to) || custody::names(to, t)));
    if !of_it || !c.statement.subject.iter().any(|t| t.name == thing.name && t.digest == thing.digest) {
        return Ok(Err(format!("it isn't a hand-on of {} to {to}", thing.name)));
    }
    Ok(take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), bundle, came_from, at).await?.map(|t| t.digest).map_err(|r| r.to_string()))
}

/// What a person signs on their device for custody statement `s` (a browser, with its own key:
/// `ui/src/lib/signing.ts`), said as a verdict's is (`ci::verdicts::to_sign`): its payload type, the
/// statement, its bytes and the bytes signed (DSSE's PAE), those two base64; never required.
pub fn to_sign(s: &statement::Statement) -> anyhow::Result<Value> {
    let payload = statement::payload(s).map_err(|e| anyhow::anyhow!("{e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(json!({ "payload_type": PAYLOAD_CUSTODY, "statement": s, "payload": b64.encode(&payload), "pae": b64.encode(statement::to_sign(PAYLOAD_CUSTODY, &payload)), "required": false }))
}

/// Whom a hand-on (`bundle`, read without checking it) says it hands its things to.
pub fn hand_on_to(bundle: &Value) -> Option<String> {
    body_in(&bundle.to_string())?.to
}

/// kiln's server passes `things`, handed on to it by `from` as `hand_on` (its digest), on unchanged
/// to `to`, for `why`: a relay's record, its server's (docs/ci.md W27-R08). Kept at once: its
/// digest and bundle, which `to`'s receipt follows.
pub async fn pass_on(hub: &crate::hub::Hub, things: Vec<statement::Subject>, hand_on: &str, from: &str, to: &str, why: &str) -> anyhow::Result<(String, Value)> {
    let org = hub.pki.organization();
    let body = custody::Body { from: Some(from.to_owned()), to: Some(to.to_owned()), why: why.to_owned(), ..custody::Body::new(Kind::PassedOn, &server_uri(&org)) };
    let s = statement::Statement { subject: things, follows: vec![hand_on.to_owned()], ..statement::Statement::new(custody::to_value(&body)) };
    // Signed on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3).
    let bundle = server_signed_off(&hub.pki, s).await?;
    match take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
        Ok(t) => Ok((t.digest, bundle)),
        Err(why) => anyhow::bail!("kiln refused its own pass of {hand_on}: {why}"),
    }
}

/// Whom a hand-on (`bundle`, read without checking: kiln checked it as it took it) says its things
/// come from: the giver a take hands on for, else its signer.
pub fn giver_of(bundle: &Value) -> Option<String> {
    let b: custody::Body = serde_json::from_value(statement_in(&bundle.to_string())?.body).ok()?;
    Some(match b.kind {
        Kind::Taken => b.taken_for?,
        _ => b.by,
    })
}

/// The receipt by which `holder` came to hold `thing`, its newest of it: one it signed, or kiln's
/// for it (a person with no key on the device that asked). None if kiln has none.
pub(crate) async fn held_with(store: &Store, org: &str, holder: &str, thing: &statement::Subject) -> anyhow::Result<Option<String>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    let rows: Vec<(String, String, String)> = store
        .read(move |c| {
            let mut q = c.prepare("SELECT c.digest, c.custodian, c.envelope FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind = 'received' AND t.algorithm = ?1 AND t.hex = ?2 ORDER BY c.taken_at DESC, c.rowid DESC")?;
            let rows = q.query_map(params![algorithm, hex], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let server = server_uri(org);
    Ok(rows
        .into_iter()
        .find(|(_, custodian, envelope)| {
            let for_whom = if *custodian == server { body_in(envelope).and_then(|b| b.taken_for) } else { None };
            let whose = for_whom.unwrap_or_else(|| custodian.clone());
            custody::names(holder, &whose) || custody::names(&whose, holder)
        })
        .map(|(d, _, _)| d))
}

/// The newest hand-on (or take) of `thing` to `to` kiln has: its digest.
pub async fn handed_to(store: &Store, to: &str, thing: &statement::Subject) -> anyhow::Result<Option<String>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    let to = to.to_owned();
    store
        .read(move |c| {
            Ok(c.query_row(
                "SELECT c.digest FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind IN ('handed-on', 'taken') AND c.counterpart = ?1 AND t.algorithm = ?2 AND t.hex = ?3 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                params![to, algorithm, hex],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await
}

/// A kept record's statement, read from its envelope as kiln took it.
pub(crate) fn statement_in(envelope: &str) -> Option<statement::Statement> {
    let bundle: Value = serde_json::from_str(envelope).ok()?;
    let payload = bundle.get("dsseEnvelope")?.get("payload")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A kept record's body, read from its envelope as kiln took it.
fn body_in(envelope: &str) -> Option<custody::Body> {
    serde_json::from_value(statement_in(envelope)?.body).ok()
}

/// The newest hand-on (or take) of `thing` to `to` that no receipt answers yet: its digest and
/// bundle (docs/ci.md W27-R08; the Transfers plan's step 7: a change given, waiting to be taken).
/// A receipt is looked for among the records that follow each hand-on (`custody_followed`), each read
/// by its digest: the CROSS JOIN keeps SQLite, which has no statistics here, from walking every receipt
/// on `custody_kind_taken` for each (the DB query audit, 2026-11-05: 601 ms at production's size, 167 us).
pub async fn waiting_for(store: &Store, to: &str, thing: &statement::Subject) -> anyhow::Result<Option<(String, Value)>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    let to = to.to_owned();
    let row: Option<(String, String)> = store
        .read(move |c| {
            Ok(c.query_row(
                "SELECT c.digest, c.envelope FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind IN ('handed-on', 'taken') AND c.counterpart = ?1 AND t.algorithm = ?2 AND t.hex = ?3
                 AND NOT EXISTS (SELECT 1 FROM custody_follows f CROSS JOIN custody r ON r.digest = f.record WHERE f.follows = c.digest AND r.kind = 'received')
                 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                params![to, algorithm, hex],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await?;
    Ok(row.and_then(|(d, e)| serde_json::from_str(&e).ok().map(|b| (d, b))))
}

/// The newest hand-on (or take) of `thing` away from `giver` (its signer, or whom kiln's server
/// took it for): how `giver` came to no longer hold it. Its digest (docs/ci.md W27-R08: a change's
/// former author).
pub async fn handed_from(store: &Store, org: &str, giver: &str, thing: &statement::Subject) -> anyhow::Result<Option<String>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    let rows: Vec<(String, String, String)> = store
        .read(move |c| {
            let mut q = c.prepare("SELECT c.digest, c.custodian, c.envelope FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind IN ('handed-on', 'taken') AND t.algorithm = ?1 AND t.hex = ?2 ORDER BY c.taken_at DESC, c.rowid DESC")?;
            let rows = q.query_map(params![algorithm, hex], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let server = server_uri(org);
    Ok(rows
        .into_iter()
        .find(|(_, custodian, envelope)| {
            let for_whom = if *custodian == server { body_in(envelope).and_then(|b| b.taken_for) } else { None };
            let whose = for_whom.unwrap_or_else(|| custodian.clone());
            custody::names(giver, &whose) || custody::names(&whose, giver)
        })
        .map(|(d, _, _)| d))
}

/// The newest hand-on (or take) of `thing` kiln has, to whomever: its digest.
pub async fn newest_hand_on(store: &Store, thing: &statement::Subject) -> anyhow::Result<Option<String>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    store
        .read(move |c| {
            Ok(c.query_row(
                "SELECT c.digest FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind IN ('handed-on', 'taken') AND t.algorithm = ?1 AND t.hex = ?2 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                params![algorithm, hex],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await
}

/// kiln's newest take of work ([`WORK`], `?2`) for agent session `?1` (at any epoch): its digest, the work's name and
/// whom it went to (the take's counterpart). Sought among kiln's takes by the session each is for
/// (`custody_taken_for`, migration `dbq_custody`, kept with every take), each one's work by its record
/// (`custody_things_record`). The newest 256 takes of work were read, and each one's envelope decoded to match its
/// giver as text, so a session's take older than those was missed (the DB query audit, 2026-11-06: 7.6 to 10 us at
/// production's size and at ten times its takes).
const WORK_TAKEN: &str = "SELECT c.digest, w.name, c.counterpart FROM custody_taken_for f CROSS JOIN custody_things w CROSS JOIN custody c
                          WHERE f.session = ?1 AND w.record = f.record AND w.algorithm = ?2 AND c.digest = f.record ORDER BY f.taken_at DESC LIMIT 1";

/// The newest work kiln took for agent session `session` (at any epoch) and handed to another: a
/// plan's step tried again with another agent, or taken over by a person. The take's digest, the
/// work's name and whom it went to ([`WORK_TAKEN`]). Each call of an agent session's asks it, read
/// beside the writer, whose turn it took before.
pub async fn work_taken_from(store: &Store, session: &str) -> anyhow::Result<Option<(String, String, String)>> {
    let s = session.to_owned();
    store
        .read(move |c| {
            let taken = c.prepare_cached(WORK_TAKEN)?.query_row(params![s, WORK], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default()))).optional()?;
            Ok(taken)
        })
        .await
}

/// Asks the runner process holding agent session `session` for its receipt of work `hand_on`,
/// which it signs as the session takes the work up, at its next turn (`ServerMsg::Custody`): the
/// process kiln counts as its holder, else its machine's newest. Only one that signs custody
/// (`CAP_CUSTODY`) is asked: another's hand-on stays unanswered, as a walk shows it.
pub async fn ask_receipt(hub: &crate::hub::Hub, session: &str, hand_on: &Value) {
    ask_receipt_of(hub, session, hand_on, false).await;
}

/// [`ask_receipt`] for the session's first work: kiln started it with the work (a subagent's
/// task) or gave it the work as its first goal (a plan step's own agent). Its first turn takes the
/// work up, and may begin before the ask reaches its runner, which then signs the receipt at once.
pub async fn ask_first_receipt(hub: &crate::hub::Hub, session: &str, hand_on: &Value) {
    ask_receipt_of(hub, session, hand_on, true).await;
}

async fn ask_receipt_of(hub: &crate::hub::Hub, session: &str, hand_on: &Value, first: bool) {
    let Some(crate::harness::registry::Location::Machine(machine)) = hub.harness.location(session) else { return };
    let ask = if first { json!({ "session": session, "hand_on": hand_on.to_string(), "first": true }) } else { json!({ "session": session, "hand_on": hand_on.to_string() }) };
    let msg = crate::proto::ServerMsg::Custody { json: ask.to_string() };
    let (owner, signs) = {
        let inst = hub.instances.load();
        let owner = inst.owner(&crate::instances::Owned::Agent(session.to_owned()));
        let signs = owner.and_then(|p| inst.get(p)).is_some_and(|i| (u64::from(i.caps) | i.wide) & crate::proto::CAP_CUSTODY != 0);
        (owner, signs)
    };
    match owner {
        Some(p) if signs => hub.send_peer(p, &msg).await,
        Some(_) => {}
        None if hub.runner_wide_caps(&machine) & crate::proto::CAP_CUSTODY != 0 => {
            hub.send_to_agent(&machine, crate::proto::LANE_CONTROL, &msg).await;
        }
        None => {}
    }
}

/// Agent session `parent` (over `came_from`, its runner) started subagents `children` for a task,
/// handing it on signed with its own key at its epoch (`hand_on`, beside its `agents.start` ask) to
/// kiln's server, which starts them: kiln keeps the hand-on, passes the task on to each child
/// unchanged (a relay's record, its server's), and asks each for its receipt as it takes the task
/// up, at its first turn (docs/ci.md W27-R08; the Transfers plan's step 6).
pub async fn subagents_handed(hub: &crate::hub::Hub, came_from: &str, hand_on: &Value, children: &[String]) {
    let (org, at) = (hub.pki.organization(), crate::util::now_unix());
    let taken = match take(&hub.store, &hub.pki.ca_der, &org, hand_on, came_from, at).await {
        Ok(Ok(t)) => t,
        Ok(Err(why)) => return tracing::warn!(target: "kiln.custody", %came_from, %why, "a subagent's task: its hand-on refused"),
        Err(e) => return tracing::warn!(target: "kiln.custody", %came_from, error = %format!("{e:#}"), "a subagent's task: its hand-on wasn't kept"),
    };
    let Some(s) = statement_in(&hand_on.to_string()) else { return };
    let parent = s.body.get("by").and_then(Value::as_str).unwrap_or_default().to_owned();
    for child in children {
        let to = match crate::identities::agent_at(hub, child, 0).await {
            Ok(to) => to,
            Err(e) => {
                tracing::warn!(target: "kiln.custody", %child, error = %format!("{e:#}"), "a subagent's task passed on unsigned: kiln can't name the subagent");
                continue;
            }
        };
        match pass_on(hub, s.subject.clone(), &taken.digest, &parent, &to, "a subagent's task, passed on unchanged as kiln started it").await {
            Ok((_, bundle)) => ask_first_receipt(hub, child, &bundle).await,
            Err(e) => tracing::warn!(target: "kiln.custody", %child, error = %format!("{e:#}"), "a subagent's task passed on wasn't kept"),
        }
    }
}

/// Runner process `peer` says it holds `sessions` (its inventory): each one's work kiln handed it
/// that no receipt answers yet is asked for again (its ask may have gone while the runner was
/// away).
pub async fn ask_again(hub: &crate::hub::Hub, peer: crate::instances::PeerId, sessions: &[String]) {
    let signs = hub.instances.load().get(peer).is_some_and(|i| (u64::from(i.caps) | i.wide) & crate::proto::CAP_CUSTODY != 0);
    if !signs {
        return;
    }
    for session in sessions {
        for envelope in unreceived(&hub.store, session).await.unwrap_or_default() {
            let msg = crate::proto::ServerMsg::Custody { json: json!({ "session": session, "hand_on": envelope }).to_string() };
            hub.send_peer(peer, &msg).await;
        }
    }
}

/// Agent session `session` opened on its runner: the work kiln handed it before (a plan's step
/// given as it started) that no receipt answers yet is asked for ([`ask_receipt`]).
pub async fn ask_open(hub: &crate::hub::Hub, session: &str) {
    for envelope in unreceived(&hub.store, session).await.unwrap_or_default() {
        if let Ok(bundle) = serde_json::from_str::<Value>(&envelope) {
            ask_receipt(hub, session, &bundle).await;
        }
    }
}

/// The envelopes of the hand-ons (and takes, and passes) of work ([`WORK`], `?2`) to agent session `?1` (at any
/// epoch, with or without the runner process holding its key) that no receipt answers, oldest first: found among the
/// records each agent session takes (`custody_takers`, migration `dbq_custody`, kept with every record, the session
/// as `Identity::parse` reads it); each one's work by its record (`custody_things_record`), looked for first (an IN, which
/// SQLite tests before the NOT EXISTS, where an EXISTS came after it), and its receipt among the records that follow it,
/// as [`waiting_for`] looks; each envelope read once the hand-on is found unanswered (the DB query audit, 2026-11-06:
/// 37 to 47 us at production's size and at ten times its agents' records).
const UNRECEIVED: &str = "SELECT (SELECT envelope FROM custody WHERE digest = t.record) FROM custody_takers t WHERE t.session = ?1 AND t.kind IN ('handed-on', 'taken', 'passed-on')
                          AND t.record IN (SELECT w.record FROM custody_things w WHERE w.record = t.record AND w.algorithm = ?2)
                          AND NOT EXISTS (SELECT 1 FROM custody_follows f CROSS JOIN custody r ON r.digest = f.record WHERE f.follows = t.record AND r.kind = 'received')
                          ORDER BY t.taken_at";

/// Whether the runner process `machine`/`instance`, holding agent session `session` at epoch `epoch`, signed a
/// hand-on of that session (to a holder of it after it): its own word that it let the session go (docs/ci.md W27-R08).
/// Read from the records each session takes (`custody_takers_session`), each one's signer matched whole.
/// 2026-11-12: adacachy's runner started again, its old process signed each session's hand-on to its next epoch, and
/// the new process held them on at the epoch it had; the mail hand-over that had bound them to the old process
/// fenced them off for good (`harness::mail_transfer::speaks_for`).
pub(crate) async fn handed_on_by(store: &Store, session: &str, epoch: u64, machine: &str, instance: &str) -> anyhow::Result<bool> {
    let (s, signer) = (session.to_owned(), format!("/agent/{session}/epoch/{epoch}/machine/{machine}/instance/{instance}"));
    store
        .read(move |c| {
            Ok(c.prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM custody_takers t JOIN custody c ON c.digest = t.record WHERE t.session = ?1 AND t.kind = 'handed-on'
                 AND length(c.custodian) > length(?2) AND substr(c.custodian, length(c.custodian) - length(?2) + 1) = ?2)",
            )?
            .query_row(params![s, signer], |r| r.get(0))?)
        })
        .await
}

/// The work kiln handed agent session `session` (to it at any epoch) that no receipt answers yet:
/// the hand-ons' envelopes, oldest first ([`UNRECEIVED`]). Looked for each time an agent session
/// opens, and for each session of every runner's inventory: read beside the writer, as no caller
/// looks for a hand-on of its own still on its way to it.
async fn unreceived(store: &Store, session: &str) -> anyhow::Result<Vec<String>> {
    let s = session.to_owned();
    store.read(move |c| Ok(c.prepare_cached(UNRECEIVED)?.query_map(params![s, WORK], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?)).await
}

/// The receipt agent identity `?2` (of session `?1`) signed of its session's timeline ([`TIMELINE`], `?3`), the newest
/// should it have signed more than one: read from the records each agent session takes (`custody_takers`, migration
/// `dbq_custody`), the timeline by its record (`custody_things_record`). Its newest receipt of anything was read, a
/// receipt of work its session took up since as well, by walking every receipt kiln keeps from the newest (the DB
/// query audit, 2026-11-06: 236 ms at production's size, 41 to 102 us).
const TRANSFER_RECEIPT: &str = "SELECT t.record FROM custody_takers t WHERE t.session = ?1 AND t.kind = 'received' AND t.taker = ?2
                                AND EXISTS (SELECT 1 FROM custody_things x WHERE x.record = t.record AND x.algorithm = ?3) ORDER BY t.taken_at DESC LIMIT 1";

/// The transfer agent session holder `holder` (an agent's identity, at its epoch) took its session with: its receipt
/// of the session's timeline ([`TRANSFER_RECEIPT`]). None for one that names no agent session, or took it with no
/// receipt kiln keeps (a hand-over from before, one that went unsigned).
async fn transfer_receipt(store: &Store, holder: &str) -> anyhow::Result<Option<String>> {
    let Ok(Identity { actor: Actor::Agent { agent, .. }, .. }) = Identity::parse(holder) else { return Ok(None) };
    let h = holder.to_owned();
    store.read(move |conn| Ok(conn.prepare_cached(TRANSFER_RECEIPT)?.query_row(params![agent, h, TIMELINE], |r| r.get(0)).optional()?)).await
}

/// Agent session `session`'s hand-over to epoch `epoch` went unsigned (docs/ci.md W27-R08): from
/// `gave` to `took`, as kiln knows them, because `why`. Kept once per session and epoch, and
/// audited; whether it's new.
pub async fn unsigned(store: &Store, session: &str, epoch: u64, gave: &str, took: &str, why: &str, at: i64) -> anyhow::Result<bool> {
    let (s, g, t, w) = (session.to_owned(), gave.to_owned(), took.to_owned(), why.to_owned());
    let e = i64::try_from(epoch).unwrap_or(i64::MAX);
    let new = store
        .call(move |c| Ok(c.execute("INSERT OR IGNORE INTO transfers_unsigned (session, epoch, gave, took, why, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![s, e, g, t, w, at])? > 0))
        .await?;
    if new {
        tracing::info!(target: "kiln.custody", %session, epoch, %gave, %took, %why, "an agent session's hand-over went unsigned");
        store.audit("kiln".into(), "custody.unsigned".into(), format!("session {session} to epoch {epoch}, from {} to {}: {why}", or_unknown(gave), or_unknown(took))).await;
    }
    Ok(new)
}

fn or_unknown(s: &str) -> &str {
    if s.is_empty() { "a process kiln never certified" } else { s }
}

/// The hand-overs that went unsigned, newest first, at most `limit` (`LIMIT +?1`, as [`BREAKS`]').
pub async fn unsigned_list(store: &Store, limit: usize) -> anyhow::Result<Vec<Value>> {
    let n = i64::try_from(limit).unwrap_or(i64::MAX);
    store
        .read(move |c| {
            let mut q = c.prepare("SELECT session, epoch, gave, took, why, at FROM transfers_unsigned ORDER BY id DESC LIMIT +?1")?;
            let rows = q
                .query_map([n], |r| {
                    Ok(json!({ "session": r.get::<_, String>(0)?, "epoch": r.get::<_, i64>(1)?, "gave": r.get::<_, String>(2)?, "took": r.get::<_, String>(3)?, "why": r.get::<_, String>(4)?, "at": r.get::<_, i64>(5)? }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// Whether kiln keeps record `digest`. On the writer, in its turn: a statement [`take`] refuses
/// must be found kept when a take of it came before (a record sent again as a runner's connection
/// came back), so it's answered as kiln's rather than refused and recorded so.
pub(crate) async fn kept(store: &Store, digest: &str) -> anyhow::Result<bool> {
    let d = digest.to_owned();
    store.call(move |c| Ok(kept_on(c, &d)?)).await
}

/// [`kept`], on the store's thread.
fn kept_on(c: &rusqlite::Connection, digest: &str) -> rusqlite::Result<bool> {
    Ok(c.prepare_cached("SELECT 1 FROM custody WHERE digest = ?1")?.query_row([digest], |_| Ok(())).optional()?.is_some())
}

/// Whether hand-on `b` (a custody record's body) hands an agent session on to `issued`, the identity
/// kiln certifies it at at `epoch` ([`transfer`]; the transfers simulation, `crate::sim::transfers`,
/// checks one so too). Why not, when it doesn't.
pub fn hands_on_to(b: &custody::Body, issued: &str, epoch: u64) -> Result<(), String> {
    if !b.kind.gives() || b.epoch != Some(epoch) || !b.to.as_deref().is_some_and(|to| custody::names(to, issued)) {
        let to = b.to.as_deref().unwrap_or("no one");
        return Err(format!("its hand-on ({}) is to {to} at epoch {}, not to {issued}", kind_name(b.kind), b.epoch.map_or_else(|| "none".to_owned(), |e| e.to_string())));
    }
    Ok(())
}

/// A hand-over's records, taken as the session's process at its new epoch `epoch` asks for its
/// certificate (docs/ci.md W27-R08; the Transfers plan's step 3), at `at`: the hand-on its
/// package carried (`hand_on`, a bundle as JSON), then its receipt, signed with the key being
/// certified and bundled here with the certificate `pem` kiln issued it for identity `issued`.
/// Both are kept before the epoch rises: the caller revokes the epoch left, and records the
/// certificate, only then. The receipt's digest, or why not (each refusal recorded as [`take`]
/// records them): the hand-on refused (a copy left behind's, its epoch revoked), or not to
/// `issued` at `epoch`; the receipt missing, refused, or not following it. Then kiln doesn't
/// certify the key. A receipt of other than what was handed on is kept, its break recorded.
#[allow(clippy::too_many_arguments)]
pub async fn transfer(store: &Store, root: &[u8], org: &str, came_from: &str, issued: &str, epoch: u64, hand_on: &str, receipt: Option<&crate::proto::Unbundled>, pem: &str, at: i64) -> anyhow::Result<Result<String, String>> {
    let Ok(given) = serde_json::from_str::<Value>(hand_on) else { return Ok(Err("its hand-on can't be read".into())) };
    let handed = match take(store, root, org, &given, came_from, at).await? {
        Ok(t) => t.digest,
        Err(why) => return Ok(Err(format!("its hand-on was refused: {why}"))),
    };
    // Kept, so checked when kiln took it: read again, revocations aside, on the compute pool (goal 9, plan 01a1a87b2cec
    // step 3: a hand-over's two checks were the hub's loop's, as it answered the runner's ask for its identity).
    let b = verify_off(&given, root, Vec::new(), at).await?.map_err(|e| anyhow::anyhow!("{e}")).and_then(|c| custody::body(&c).map_err(|e| anyhow::anyhow!("{e}")))?;
    if let Err(why) = hands_on_to(&b, issued, epoch) {
        return Ok(Err(why));
    }
    let Some(r) = receipt else { return Ok(Err("no receipt came with its hand-on".into())) };
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(payload), Ok(signature)) = (b64.decode(&r.payload), b64.decode(&r.signature)) else { return Ok(Err("its receipt can't be read".into())) };
    let bundle = statement::bundle(&r.payload_type, &payload, &signature, &crate::pki::der_of(pem)?);
    let got = match take(store, root, org, &bundle, came_from, at).await? {
        Ok(t) => t,
        Err(why) => return Ok(Err(format!("its receipt was refused: {why}"))),
    };
    // A receipt of other than what was handed on breaks the chain there, recorded (`take`): the
    // session still goes on where it's held, as the chain says.
    if let Some(broke) = &got.broke {
        tracing::warn!(target: "kiln.custody", record = %got.digest, %broke, "a hand-over's receipt breaks its chain");
    }
    let follows = verify_off(&bundle, root, Vec::new(), at).await?.map(|c| c.statement.follows).unwrap_or_default();
    if !follows.contains(&handed) {
        return Ok(Err(format!("its receipt doesn't follow its hand-on {handed}")));
    }
    Ok(Ok(got.digest))
}

/// A record's rows as [`keep`] writes them: made where it was checked, off the shards for what kiln takes ([`read`]).
struct Rows {
    digest: String,
    payload_type: String,
    kind: Option<String>,
    custodian: String,
    counterpart: Option<String>,
    stream: Option<String>,
    seq: Option<i64>,
    under: Option<String>,
    envelope: String,
    came_from: String,
    taken_at: i64,
    follows: Vec<String>,
    /// Each thing it names: its name, an algorithm and its digest by it.
    things: Vec<(String, String, String)>,
}

/// `c`'s rows, of kind `kind` with `counterpart`, its envelope `envelope`, come over `came_from`.
fn rows(c: &Checked, kind: Option<Kind>, counterpart: Option<String>, envelope: String, came_from: String) -> Rows {
    Rows {
        digest: c.digest.clone(),
        payload_type: c.payload_type.clone(),
        kind: kind.map(kind_name),
        custodian: c.custodian.clone(),
        counterpart,
        stream: c.statement.stream.clone(),
        seq: c.statement.seq.and_then(|s| i64::try_from(s).ok()),
        under: c.statement.under.clone(),
        envelope,
        came_from,
        taken_at: c.at,
        follows: c.statement.follows.clone(),
        things: c.statement.subject.iter().flat_map(|s| s.digest.iter().map(|(alg, hex)| (s.name.clone(), alg.clone(), hex.clone()))).collect(),
    }
}

/// Keeps a record by its rows: whether it's new (false: kiln had it).
async fn keep(store: &Store, rows: Rows) -> anyhow::Result<bool> {
    store.call(move |conn| keep_on(conn, rows)).await
}

/// [`keep`], on the store's thread. Its statements are the store's cached ones: each record kiln takes runs them, on
/// the one thread every write waits for (the DB query audit, 2026-11-05: prepared anew they were half their time).
fn keep_on(conn: &rusqlite::Connection, rows: Rows) -> anyhow::Result<bool> {
    let Rows { digest, payload_type, kind, custodian, counterpart, stream, seq, under, envelope, came_from, taken_at, follows, things } = rows;
    let tx = crate::store::tx(conn)?;
    let n = tx
        .prepare_cached("INSERT OR IGNORE INTO custody (digest, payload_type, kind, custodian, counterpart, stream, seq, under, envelope, came_from, taken_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)")?
        .execute(params![digest, payload_type, kind, custodian, counterpart, stream, seq, under, envelope, came_from, taken_at])?;
    if n == 0 {
        return Ok(false);
    }
    if !follows.is_empty() {
        let mut q = tx.prepare_cached("INSERT OR IGNORE INTO custody_follows (record, follows) VALUES (?1, ?2)")?;
        for f in &follows {
            q.execute(params![digest, f])?;
        }
    }
    if !things.is_empty() {
        let mut q = tx.prepare_cached("INSERT INTO custody_things (record, name, algorithm, hex) VALUES (?1, ?2, ?3, ?4)")?;
        for (name, alg, hex) in &things {
            q.execute(params![digest, name, alg, hex])?;
        }
    }
    crate::trail::take(&tx, &envelope, taken_at)?;
    tx.commit()?;
    Ok(true)
}

/// The records that follow record `digest`.
async fn followers(store: &Store, digest: &str) -> anyhow::Result<Vec<String>> {
    let d = digest.to_owned();
    store.read(move |c| Ok(followers_on(c, &d)?)).await
}

/// [`followers`], on the store's thread.
fn followers_on(c: &rusqlite::Connection, digest: &str) -> rusqlite::Result<Vec<String>> {
    let mut q = c.prepare_cached("SELECT record FROM custody_follows WHERE follows = ?1")?;
    q.query_map([digest], |r| r.get(0))?.collect()
}

/// The records kiln has among `digests`, each checked as of when kiln took it (a revocation
/// since doesn't undo it).
async fn load(store: &Store, root: &std::sync::Arc<[u8]>, digests: &[String], revocations: &std::sync::Arc<[Revocation]>) -> anyhow::Result<BTreeMap<String, Checked>> {
    load_with(store, root, digests, revocations, |records| records).await
}

/// [`load`], with `then` made of the records in the same job on the compute pool, off the shards, and the records let
/// go there (goal 9, plan 01a1a87b2cec step 3): what a check reads of them and the hop it walks ([`check`]). The root and
/// the revocations go to it shared, not copied: each of a waiting record's two loads copied every revocation on its
/// shard (goal 9, plan 01a1a87b2cec step 3).
async fn load_with<T: Send + 'static>(store: &Store, root: &std::sync::Arc<[u8]>, digests: &[String], revocations: &std::sync::Arc<[Revocation]>, then: impl FnOnce(BTreeMap<String, Checked>) -> T + Send + 'static) -> anyhow::Result<T> {
    let wanted = digests.to_vec();
    let rows = store.read(move |c| Ok(rows_on(c, &wanted)?)).await?;
    // Each checked again as of when kiln took it, on the compute pool, off the shards (`read`).
    let (root, revocations) = (std::sync::Arc::clone(root), std::sync::Arc::clone(revocations));
    crate::rt::compute(move || then(checked_rows(rows, &root, &revocations))).await
}

/// The rows kiln keeps of the records among `digests` it has, on the store's thread: each one's digest, envelope and
/// when kiln took it.
fn rows_on(c: &rusqlite::Connection, digests: &[String]) -> rusqlite::Result<Vec<(String, String, i64)>> {
    let mut q = c.prepare_cached("SELECT digest, envelope, taken_at FROM custody WHERE digest = ?1")?;
    let mut out = Vec::with_capacity(digests.len());
    for d in digests {
        if let Some(row) = q.query_row([d], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()? {
            out.push(row);
        }
    }
    Ok(out)
}

/// `rows` ([`rows_on`]) checked again, each as of when kiln took it, against kiln's root `root` and the revocations
/// before then: off the shards, on the compute pool ([`load_with`], [`take`]).
fn checked_rows(rows: Vec<(String, String, i64)>, root: &[u8], revocations: &[Revocation]) -> BTreeMap<String, Checked> {
    let mut map = BTreeMap::new();
    for (d, envelope, at) in rows {
        let Ok(bundle) = serde_json::from_str::<Value>(&envelope) else { continue };
        // Only what was revoked before kiln took it: kiln checked it then, and a revocation since
        // (one in the same second included) doesn't undo it.
        let before: Vec<Revocation> = revocations.iter().filter(|r| r.at < at).cloned().collect();
        match statement::verify(&bundle, root, &before, at) {
            // Kept by its statement's digest: an envelope of another statement there (changed
            // where kiln keeps it) isn't the record.
            Ok(c) if c.digest == d => {
                map.insert(d, c);
            }
            Ok(c) => tracing::warn!(record = %d, kept = %c.digest, "a custody record kiln kept is another record now"),
            Err(e) => tracing::warn!(record = %d, error = %e, "a custody record kiln kept no longer checks as of when it took it"),
        }
    }
    map
}

/// What record `r`'s own hop needs kiln to have: the records it follows, and, taken for someone (`held_for`), the
/// delegation it acts under.
fn wanted_by(r: &Checked) -> Vec<String> {
    let held_for = custody::body(r).is_ok_and(|b| b.kind.takes() && b.held_for.as_deref().is_some_and(|w| !w.is_empty()));
    r.statement.follows.iter().cloned().chain(r.statement.under.clone().filter(|_| held_for)).collect()
}

/// Record `d`'s own hop, `mine` itself as checked and `map` the records of `needed` ([`wanted_by`]) kiln has, each as
/// checked: none while one of them isn't there yet (it waits for it) or when the hop holds; else the break, the records
/// around it and the relay it names ([`Broken`]).
fn broken(d: &str, mine: Checked, mut map: BTreeMap<String, Checked>, needed: &[String], org: &str) -> Option<Broken> {
    if needed.iter().any(|f| !map.contains_key(f)) {
        return None;
    }
    let follows = mine.statement.follows.clone();
    map.insert(d.to_owned(), mine);
    let b = custody::hop(d, &map, &server_uri(org)).err()?;
    let mut around = follows;
    around.insert(0, d.to_owned());
    let relay = relay_named(&b, d, &map);
    Some((b, around, relay))
}

/// Checks record `digest`'s own hop, at `at`: a break, recorded, if it doesn't hold; none if it
/// holds or waits for a record it follows that kiln doesn't have yet, or, taken for someone, the
/// delegation it acts under (W27-R08: another machine's, which comes in any order). Its records are
/// read and its hop walked on the compute pool, with their loads ([`load_with`]).
async fn check(store: &Store, root: &std::sync::Arc<[u8]>, org: &str, digest: &str, revocations: &std::sync::Arc<[Revocation]>, at: i64) -> anyhow::Result<Option<String>> {
    let d = digest.to_owned();
    let wants = load_with(store, root, std::slice::from_ref(&d), revocations, {
        let d = d.clone();
        move |mut mine| {
            let r = mine.remove(&d)?;
            let wanted = wanted_by(&r);
            Some((r, wanted))
        }
    })
    .await?;
    let Some((mine, wanted)) = wants else { return Ok(None) };
    let (needed, org) = (wanted.clone(), org.to_owned());
    let found = load_with(store, root, &wanted, revocations, move |map| broken(&d, mine, map, &needed, &org)).await?;
    let Some((b, around, relay)) = found else { return Ok(None) };
    record_break(store, digest, &b, &around, &relay, at).await?;
    Ok(Some(b.to_string()))
}

/// Records break `b` at record `record`, with the records around it, once; naming `relay` when
/// it's in what a relay passed on ([`relay_named`]).
async fn record_break(store: &Store, record: &str, b: &Break, around: &[String], relay: &str, at: i64) -> anyhow::Result<()> {
    insert_break(store, record, b.code(), &b.to_string(), around, "", relay, at).await
}

/// The relay break `b` at record `record` names, when it's in what a relay passed on (docs/ci.md
/// W27-R08's relays): the broken record's signer when it's a relay's record, else the signer of the
/// relay's record it follows; "" for none.
fn relay_named(b: &Break, record: &str, map: &BTreeMap<String, Checked>) -> String {
    let passed = |d: &str| map.get(d).filter(|c| custody::body(c).is_ok_and(|b| b.kind == Kind::PassedOn)).map(|c| c.custodian.clone());
    let from = match b {
        Break::Changed { from, .. }
        | Break::NoCommonDigest { from, .. }
        | Break::NotHandedOn { from, .. }
        | Break::NotAll { from, .. }
        | Break::NotTheTaker { from, .. }
        | Break::NotTheGiver { from, .. }
        | Break::NotTheHolder { from, .. } => Some(from.as_str()),
        _ => None,
    };
    passed(record).or_else(|| from.and_then(passed)).unwrap_or_default()
}

/// Records a break found in what relay `relay` (its identity's URI) passed on (docs/ci.md
/// W27-R08's relays): at record `record`, the relay's record by its digest; its kind (`code`, as
/// `kiln_ci::identity::relay::Unrelayed::code` names it) and words; the records around it; and
/// what was refused because of it, if anything. Once.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn relay_break(store: &Store, record: &str, code: &str, detail: &str, around: &[String], refused: &str, relay: &str, at: i64) -> anyhow::Result<()> {
    insert_break(store, record, code, detail, around, refused, relay, at).await
}

#[allow(clippy::too_many_arguments)]
async fn insert_break(store: &Store, record: &str, kind: &str, detail: &str, around: &[String], refused: &str, relay: &str, at: i64) -> anyhow::Result<()> {
    let (r, kind, detail, around_json, refused, relay) = (record.to_owned(), kind.to_owned(), detail.to_owned(), serde_json::to_string(around)?, refused.to_owned(), relay.to_owned());
    let (rr, dd) = (r.clone(), detail.clone());
    let new = store
        .call(move |c| {
            let seen: Option<i64> = c.query_row("SELECT id FROM custody_breaks WHERE record = ?1 AND kind = ?2 AND detail = ?3", params![r, kind, detail], |x| x.get(0)).optional()?;
            if seen.is_some() {
                return Ok(false);
            }
            c.execute("INSERT INTO custody_breaks (record, kind, detail, around, refused, relay, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![r, kind, detail, around_json, refused, relay, at])?;
            Ok(true)
        })
        .await?;
    if new {
        tracing::warn!(target: "kiln.custody", record = %rr, detail = %dd, "a chain of custody doesn't hold");
        let entry = crate::audit::Entry { principal: "kiln".into(), action: "custody.break".into(), target: Some(rr), result: "broken".into(), detail: dd, ..crate::audit::Entry::default() };
        crate::audit::write(store, entry).await;
    }
    Ok(())
}

/// Records that kiln, or a runner process, refused `what`, which relay `relay` passed on, for
/// `why` (docs/ci.md W27-R08's relays): the relay's record by its digest and envelope, and what it
/// came over.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn refused_relayed(store: &Store, digest: &str, what: &str, relay: &str, why: &str, envelope: &str, came_from: &str, at: i64) -> anyhow::Result<()> {
    let r = Refusal { digest: digest.to_owned(), act: what.to_owned(), signer: relay.to_owned(), why: why.to_owned(), envelope: envelope.to_owned(), came_from: came_from.to_owned(), at, ..Refusal::default() };
    refuse(store, r).await
}

/// Keeps `bundle`, an originator's own signed statement that a relay passed on (an agent session's
/// act: docs/ci.md W27-R08's relays), by its digest, so a walk from a relay's record finds it: not
/// a custody record, so of no kind; checked as of `at`, revocations aside (whether the act counts
/// is its own check's). Whether it's new.
pub(crate) async fn keep_originator(store: &Store, root: &[u8], bundle: &Value, came_from: &str, at: i64) -> anyhow::Result<bool> {
    let c = verify_off(bundle, root, Vec::new(), at).await?.map_err(|e| anyhow::anyhow!("{e}"))?;
    keep(store, rows(&c, None, None, bundle.to_string(), came_from.to_owned())).await
}

/// [`keep_originator`], `c` its check made already (a relayed act's, `crate::relays::call`): not made again.
pub(crate) async fn keep_originator_checked(store: &Store, c: &Checked, bundle: &Value, came_from: &str) -> anyhow::Result<bool> {
    keep(store, rows(c, None, None, bundle.to_string(), came_from.to_owned())).await
}

/// [`statement::verify`] on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3): a statement's
/// signature and its certificate chain, public-key checks that took a third of a millisecond together on the
/// lab machine, on the shard of whatever task asked.
pub(crate) async fn verify_off(bundle: &Value, root: &[u8], revocations: Vec<Revocation>, at: i64) -> anyhow::Result<Result<Checked, statement::Refusal>> {
    let (bundle, root) = (bundle.clone(), root.to_vec());
    crate::rt::compute(move || statement::verify(&bundle, &root, &revocations, at)).await
}

/// Records refusal `r`, and audits it.
async fn refuse(store: &Store, r: Refusal) -> anyhow::Result<()> {
    let entry = crate::audit::Entry {
        principal: if r.signer.is_empty() { "unknown".into() } else { r.signer.clone() },
        action: "custody.refused".into(),
        target: Some(if r.digest.is_empty() { r.act.clone() } else { r.digest.clone() }),
        result: "refused".into(),
        detail: r.why.clone(),
        ..crate::audit::Entry::default()
    };
    tracing::info!(target: "kiln.custody", digest = %r.digest, signer = %r.signer, why = %r.why, "refused {}", r.act);
    // What an API caller sent is audited as their request already, one row for it (W19-AC04);
    // what a runner sent, which no request audits, is audited here.
    let audited = !r.came_from.starts_with("api:");
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO transfer_refusals (digest, act, signer, why, envelope, came_from, at, epoch, superseded_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![r.digest, r.act, r.signer, r.why, r.envelope, r.came_from, r.at, r.epoch.map(|e| i64::try_from(e).unwrap_or(i64::MAX)), r.superseded_by],
            )?;
            Ok(())
        })
        .await?;
    if audited {
        crate::audit::write(store, entry).await;
    }
    Ok(())
}

/// Records that kiln refused `act` by `signer`, who no longer holds what it acts on: superseded by
/// `transfer`, the hand-over that moved it (docs/ci.md W27-R08: a change's former author).
pub async fn refused_former(store: &Store, act: &str, signer: &str, why: &str, transfer: &str, came_from: &str, at: i64) -> anyhow::Result<()> {
    refuse(store, Refusal { act: act.to_owned(), signer: signer.to_owned(), why: why.to_owned(), superseded_by: transfer.to_owned(), came_from: came_from.to_owned(), at, ..Refusal::default() }).await
}

/// Records that kiln refused act `act` (a statement's digest `digest`, signed by `signer`) for
/// `why`: what the fence and the act checks call (W27-R08).
pub async fn refused(store: &Store, digest: &str, act: &str, signer: &str, why: &str, came_from: &str, at: i64) -> anyhow::Result<()> {
    refuse(store, Refusal { digest: digest.to_owned(), act: act.to_owned(), signer: signer.to_owned(), why: why.to_owned(), came_from: came_from.to_owned(), at, ..Refusal::default() }).await
}

/// The record a walk starts from: `digest` if it's a record kiln has; else, as a thing's digest
/// (`sha256:…`, `gitCommit:…`), the head of that thing's chain: a record about it that no other
/// record about it follows (records arrive in any order: a receipt may come before its hand-on),
/// the newest such when its chain forks.
async fn start(store: &Store, digest: &str) -> anyhow::Result<Option<String>> {
    let d = digest.to_owned();
    store
        .read(move |c| {
            if let Some(r) = c.query_row("SELECT digest FROM custody WHERE digest = ?1", [&d], |x| x.get::<_, String>(0)).optional()? {
                return Ok(Some(r));
            }
            let Some((alg, hex)) = d.split_once(':') else { return Ok(None) };
            Ok(c.query_row(
                "SELECT t.record FROM custody_things t JOIN custody c ON c.digest = t.record
                 WHERE t.algorithm = ?1 AND t.hex = ?2 AND c.kind IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM custody_follows f JOIN custody_things n ON n.record = f.record WHERE f.follows = t.record AND n.algorithm = t.algorithm AND n.hex = t.hex)
                 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                params![alg, hex.to_ascii_lowercase()],
                |x| x.get(0),
            )
            .optional()?)
        })
        .await
}

/// The head of the chain of the newest thing named `?1`: its newest record no other record follows, sought by its name
/// on `custody_things_name` (migration `dbq_custody`), NOCASE as the name's letters' case is set aside, where a
/// pattern was bound and the statement prepared again each call (the DB query audit, 2026-11-06: 22.7 us, 14.4).
const HEAD_NAMED: &str = "SELECT t.record FROM custody_things t JOIN custody c ON c.digest = t.record WHERE t.name = ?1 COLLATE NOCASE AND c.kind IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM custody_follows f WHERE f.follows = t.record) ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1";

/// [`HEAD_NAMED`] among the names from `?1` up to `?2`: every name a prefix begins (36 us, 24).
const HEAD_BEGUN: &str = "SELECT t.record FROM custody_things t JOIN custody c ON c.digest = t.record WHERE t.name >= ?1 COLLATE NOCASE AND t.name < ?2 COLLATE NOCASE
     AND c.kind IS NOT NULL AND NOT EXISTS (SELECT 1 FROM custody_follows f WHERE f.follows = t.record) ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1";

/// The least text after every one `prefix` begins, as NOCASE orders them (ASCII's letters folded to lower case, then
/// by their bytes): the folded prefix with its last character the next one there can be, carried back past one that
/// has none. A folded text holds no upper-case letter, so after `@` comes `[`. None when no text comes after them all
/// (an empty prefix, one of U+10FFFF alone).
fn after_prefix(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().map(|c| c.to_ascii_lowercase()).collect();
    while let Some(last) = chars.pop() {
        let next = match last {
            '@' => Some('['),
            '\u{D7FF}' => Some('\u{E000}'),
            c => char::from_u32(u32::from(c) + 1),
        };
        if let Some(n) = next {
            chars.push(n);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

/// The head of the chain of the newest thing named `name` ("session 01a1751c at epoch 3"), the
/// letters' case aside (ASCII's); a name ending in `*` is every name it begins ("session 01a1751c at
/// epoch *"). The record a walk of it starts from; none if kiln has no record of it.
pub async fn head_named(store: &Store, name: &str) -> anyhow::Result<Option<String>> {
    let name = name.to_owned();
    store
        .read(move |c| {
            let head = match name.strip_suffix('*') {
                None => c.prepare_cached(HEAD_NAMED)?.query_row([&name], |r| r.get(0)).optional()?,
                Some(prefix) => {
                    // A blob sorts after every text: the range's end when no text comes after the prefix's.
                    let end = after_prefix(prefix).map_or(rusqlite::types::Value::Blob(Vec::new()), rusqlite::types::Value::Text);
                    c.prepare_cached(HEAD_BEGUN)?.query_row(params![prefix, end], |r| r.get(0)).optional()?
                }
            };
            Ok(head)
        })
        .await
}

/// One side of an approval carried across a rebase-only version ([`carry`]): the version, its
/// commit and the base it's on.
pub struct Side<'a> {
    pub n: u32,
    pub commit: &'a str,
    pub base: &'a str,
}

/// An approval carried across a rebase-only version, as a transfer (docs/ci.md W27-R08, with
/// W27-R04; the Transfers plan's step 7): verdict `verdict` (its statement's digest), given on
/// version `from` of `diff` (`acme/review D1`), holds on `to`, a later one with the same verbatim
/// patch-id `patch_id`. kiln's server hands it on for the version it was given on and receives it
/// for the one it reached, each record naming its version's commit and base (`component`), and
/// why: "carried from v2: rebase only, patch-id 9f3c41a…, base a1b2c3d to c3d4e5f". Both are kept;
/// what comes back is the receipt's digest and that why. Its bodies are what the versions say, no
/// time of their own, so carrying it again (the verdicts read again) gives the same records, kept
/// once.
pub async fn carry(hub: &crate::hub::Hub, diff: &str, verdict: &str, from: &Side<'_>, to: &Side<'_>, patch_id: &str) -> anyhow::Result<Result<(String, String), String>> {
    let short = |s: &str| s.get(..7).unwrap_or(s).to_owned();
    let why = format!("carried from v{}: rebase only, patch-id {}…, base {} to {}", from.n, short(patch_id), short(from.base), short(to.base));
    let hex = verdict.strip_prefix("sha256:").unwrap_or(verdict);
    let things = vec![
        statement::Subject { name: format!("verdict {} on {diff}", short(hex)), digest: BTreeMap::from([("sha256".to_owned(), hex.to_owned())]), ..statement::Subject::default() },
        statement::Subject { name: format!("{diff}'s patch"), digest: BTreeMap::from([(kiln_ci::identity::verdict::PATCH_ID.to_owned(), patch_id.to_owned())]), ..statement::Subject::default() },
    ];
    let (org, at) = (hub.pki.organization(), crate::util::now_unix());
    let (server, came_from) = (server_uri(&org), format!("kiln's review of {diff}"));
    let version = |v: &Side<'_>| format!("v{} at {} on base {}", v.n, short(v.commit), short(v.base));
    let b = custody::Body { to: Some(server.clone()), component: Some(version(from)), why: why.clone(), ..custody::Body::new(Kind::HandedOn, "") };
    // Each record signed on the compute pool, off the shards, in turn (goal 9, plan 01a1a87b2cec step 3).
    let hand_on = server_signed_off(&hub.pki, statement::Statement { subject: things.clone(), ..statement::Statement::new(custody::to_value(&b)) }).await?;
    let handed = match take(&hub.store, &hub.pki.ca_der, &org, &hand_on, &came_from, at).await? {
        Ok(t) => t.digest,
        Err(why) => return Ok(Err(format!("its hand-on wasn't kept: {why}"))),
    };
    let b = custody::Body { from: Some(server), component: Some(version(to)), why: why.clone(), ..custody::Body::new(Kind::Received, "") };
    let receipt = server_signed_off(&hub.pki, statement::Statement { subject: things, follows: vec![handed], ..statement::Statement::new(custody::to_value(&b)) }).await?;
    Ok(match take(&hub.store, &hub.pki.ca_der, &org, &receipt, &came_from, at).await? {
        Ok(t) => Ok((t.digest, why)),
        Err(why) => Err(format!("its receipt wasn't kept: {why}")),
    })
}

/// What a runner's blue/green upgrade on `machine`, under way since `since` (unix seconds), handed
/// on, as a replant's record of it lists them (docs/ci.md W27-R08; the Transfers plan's step 8):
/// the machine's runner role, from the process it replaced to its new build, and each of
/// `sessions` (the agent sessions it held as the upgrade began), each a transfer kiln took since,
/// signed by both sides and walking unbroken; and what moved without one.
pub async fn replanted(store: &Store, root: &[u8], org: &str, machine: &str, since: i64, sessions: &[String]) -> anyhow::Result<Value> {
    let (mut transfers, mut without) = (Vec::new(), Vec::new());
    let things = std::iter::once((format!("runner role of {machine}"), format!("runner role of {machine}"))).chain(sessions.iter().map(|s| (format!("session {s}"), format!("session {s} at epoch *"))));
    for (what, name) in things {
        match transfer_since(store, root, org, &name, since).await? {
            Some(mut t) => {
                if let Some(o) = t.as_object_mut() {
                    o.insert("what".into(), json!(what));
                }
                transfers.push(t);
            }
            None => without.push(what),
        }
    }
    Ok(json!({ "transfers": transfers, "moved_without_a_transfer": without }))
}

/// The newest transfer of the thing named `name` (a trailing `*` matching any end) that kiln took
/// since `since`: its receipt and the hand-on or take it follows, both signed, its chain walking
/// unbroken. None otherwise.
async fn transfer_since(store: &Store, root: &[u8], org: &str, name: &str, since: i64) -> anyhow::Result<Option<Value>> {
    let Some(head) = head_named(store, name).await? else { return Ok(None) };
    let Some(v) = chain(store, root, org, &head, crate::util::now_unix()).await? else { return Ok(None) };
    if v.get("unbroken") != Some(&json!(true)) {
        return Ok(None);
    }
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let s = |h: &Value, k: &str| h.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    Ok(match (hops.first(), hops.get(1)) {
        (Some(got), Some(gave))
            if s(got, "kind") == "received"
                && matches!(s(gave, "kind").as_str(), "handed-on" | "taken")
                && !s(got, "signer").is_empty()
                && !s(gave, "signer").is_empty()
                && got.get("at").and_then(Value::as_i64).is_some_and(|at| at >= since) =>
        {
            Some(json!({ "record": s(got, "record"), "from": s(gave, "custodian"), "to": s(got, "custodian"), "at": got.get("at") }))
        }
        _ => None,
    })
}

/// The digests of every record kiln has on custodian `custodian`'s stream `stream` (`LIMIT +?3`, as
/// [`BREAKS`]').
async fn on_stream(store: &Store, custodian: &str, stream: &str) -> anyhow::Result<Vec<String>> {
    let (who, s) = (custodian.to_owned(), stream.to_owned());
    store
        .read(move |c| {
            let mut q = c.prepare("SELECT digest FROM custody WHERE custodian = ?1 AND stream = ?2 ORDER BY seq LIMIT +?3")?;
            let rows = q.query_map(params![who, s, i64::try_from(WALK_MAX).unwrap_or(i64::MAX)], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
}

/// A thing's chain of custody, walked now from `digest` (a record's, or a thing's): each hop with
/// both sides and who signed it, or the first break, recorded. None when kiln has no record of it.
pub async fn chain(store: &Store, root: &[u8], org: &str, digest: &str, at: i64) -> anyhow::Result<Option<Value>> {
    Ok(walked(store, root, org, digest, at).await?.map(|(v, _)| v))
}

/// [`chain`], and the digests of the records it read.
async fn walked(store: &Store, root: &[u8], org: &str, digest: &str, at: i64) -> anyhow::Result<Option<(Value, Vec<String>)>> {
    let Some(from) = start(store, digest).await? else { return Ok(None) };
    let (root, revocations): (std::sync::Arc<[u8]>, std::sync::Arc<[Revocation]>) = (root.into(), crate::identities::revocations(store).await?.into());
    let root = &root;
    let mut map: BTreeMap<String, Checked> = BTreeMap::new();
    let mut frontier = vec![from.clone()];
    while !frontier.is_empty() && map.len() < WALK_MAX {
        let got = load(store, root, &frontier, &revocations).await?;
        let mut next: BTreeSet<String> = BTreeSet::new();
        for c in got.values() {
            // What it follows, and the delegation it acts under (a record taken for someone is
            // judged by it: W27-R08).
            next.extend(c.statement.follows.iter().chain(c.statement.under.iter()).filter(|f| !map.contains_key(*f) && !got.contains_key(*f)).cloned());
        }
        map.extend(got);
        frontier = next.into_iter().filter(|d| !map.contains_key(d)).collect();
    }
    let streams: BTreeSet<(String, String)> = map.values().filter_map(|c| c.statement.stream.clone().map(|s| (c.custodian.clone(), s))).collect();
    for (who, s) in streams {
        let more: Vec<String> = on_stream(store, &who, &s).await?.into_iter().filter(|d| !map.contains_key(d)).collect();
        let got = load(store, root, &more, &revocations).await?;
        map.extend(got);
    }
    let view = |h: &Hop| {
        let c = map.get(&h.record);
        let b = c.and_then(|c| custody::body(c).ok());
        json!({
            "record": h.record,
            "kind": kind_name(h.kind),
            "custodian": h.custodian,
            // kiln's take, or its receipt, for someone who couldn't sign: "kiln for sam".
            "taken_for": h.taken_for,
            "signer": c.map(|c| c.signer.to_string()),
            "to": b.as_ref().and_then(|b| b.to.clone()),
            "from": b.as_ref().and_then(|b| b.from.clone()),
            // The part of its custodian that holds them: a promotion's environment.
            "component": b.as_ref().and_then(|b| b.component.clone()),
            // Whom it holds them for, and the delegation it takes them under (W27-R08).
            "for": b.as_ref().and_then(|b| b.held_for.clone()),
            "under": c.and_then(|c| c.statement.under.clone()),
            "why": b.as_ref().map(|b| b.why.clone()),
            "at": h.at,
            "subject": h.subject,
            "made_from": h.made_from,
            "began": h.began,
        })
    };
    // Who holds it as of that record: its taker; or, for an offer no record answers yet, its giver,
    // until it's taken (and after it lapses).
    let (mut open, mut lapsed, mut held_by) = (false, false, None);
    if let Some((c, b)) = map.get(&from).and_then(|c| custody::body(c).ok().map(|b| (c, b))) {
        if b.kind == Kind::Received {
            // kiln's receipt for a person with no key on the device that asked: theirs to hold.
            held_by = Some(b.taken_for.clone().filter(|_| c.custodian == server_uri(org)).unwrap_or_else(|| c.custodian.clone()));
        } else if followers(store, &from).await?.is_empty() {
            open = true;
            lapsed = at - c.at > LAPSE_AFTER;
            held_by = Some(if b.kind == Kind::Taken { b.taken_for.clone().unwrap_or_default() } else { c.custodian.clone() });
        }
    }
    let read: Vec<String> = map.keys().cloned().collect();
    let v = match custody::walk(&from, &map, &server_uri(org)) {
        Ok(w) => json!({ "from": from, "unbroken": true, "open": open, "lapsed": lapsed, "held_by": held_by, "hops": w.hops.iter().map(view).collect::<Vec<_>>(), "began": w.began }),
        Err(b) => {
            let b = unchecked(store, root, &revocations, b).await?;
            let at_record = break_record(&b).unwrap_or(&from).to_owned();
            // A gap in a relay's stream names the relay: records it passed on never came.
            let relay = match &b {
                Break::Gap(statement::Gap::Missing { custodian, stream, .. } | statement::Gap::Twice { custodian, stream, .. })
                    if map.values().any(|c| c.custodian == *custodian && c.statement.stream.as_deref() == Some(stream) && custody::body(c).is_ok_and(|b| b.kind == Kind::PassedOn)) =>
                {
                    custodian.clone()
                }
                _ => relay_named(&b, &at_record, &map),
            };
            record_break(store, &at_record, &b, std::slice::from_ref(&from), &relay, at).await?;
            json!({ "from": from, "unbroken": false, "break": { "kind": b.code(), "record": at_record, "detail": b.to_string(), "relay": relay } })
        }
    };
    Ok(Some((v, read)))
}

/// A walk's break at a record kiln doesn't have, when kiln keeps it but it no longer checks as kiln
/// took it (changed where it's kept): a break at that record, saying so, not a record missing.
async fn unchecked(store: &Store, root: &std::sync::Arc<[u8]>, revocations: &std::sync::Arc<[Revocation]>, b: Break) -> anyhow::Result<Break> {
    let Break::NoRecord { record, .. } = &b else { return Ok(b) };
    let d = record.clone();
    let kept: Option<(String, i64)> = store.read(move |c| Ok(c.query_row("SELECT envelope, taken_at FROM custody WHERE digest = ?1", [&d], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await?;
    let Some((envelope, at)) = kept else { return Ok(b) };
    // Checked again on the compute pool, off the shards, with the walk's root and revocations shared, not copied (goal 9,
    // plan 01a1a87b2cec step 3).
    let (root, revocations) = (std::sync::Arc::clone(root), std::sync::Arc::clone(revocations));
    let checked = crate::rt::compute(move || checked_as_taken(&envelope, &root, &revocations, at)).await?;
    let why = match checked {
        Ok(c) if c.digest == *record => return Ok(b),
        Ok(c) => format!("what kiln keeps as it is another record, {}", c.digest),
        Err(e) => format!("what kiln keeps as it no longer checks as kiln took it: {e}"),
    };
    Ok(Break::Unreadable { record: record.clone(), why })
}

/// A record as kiln keeps it (`envelope`, taken at `at`) checked again as of then, against kiln's root `root` and the
/// revocations before then ([`unchecked`]): on the thread it's called on, a compute job's.
fn checked_as_taken(envelope: &str, root: &[u8], revocations: &[Revocation], at: i64) -> Result<Checked, String> {
    let before: Vec<Revocation> = revocations.iter().filter(|r| r.at < at).cloned().collect();
    serde_json::from_str::<Value>(envelope).map_err(|e| e.to_string()).and_then(|bundle| statement::verify(&bundle, root, &before, at).map_err(|e| e.to_string()))
}

/// A thing's chain of custody walked for whoever audits it (docs/ci.md W27-R08; the Transfers
/// plan's step 15): [`chain`]'s walk from `digest` (a record's, or a thing's, from where it's used
/// back to where it was made), each hop also in words, or its first break; with `export`, every
/// record the walk read as kiln took it (its Sigstore bundle), kiln's CA certificate and the
/// revocations each was checked against, which check each signature and walk it again without
/// kiln. None when kiln has no record of it.
pub async fn walk(store: &Store, pki: &crate::pki::Pki, digest: &str, at: i64, export: bool) -> anyhow::Result<Option<Value>> {
    let Some((mut v, mut read)) = walked(store, &pki.ca_der, &pki.organization(), digest, at).await? else { return Ok(None) };
    // Its words name a runner as people see it, kiln knowing it by its machine id (docs/machines.md).
    let labels = store.agent_labels().await.unwrap_or_else(|e| {
        tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "the runners' labels couldn't be read: a walk names them by their ids");
        Default::default()
    });
    let shown = |machine: &str| labels.get(machine).cloned().unwrap_or_else(|| machine.to_owned());
    if let Some(hops) = v.get_mut("hops").and_then(Value::as_array_mut) {
        for h in hops.iter_mut() {
            let words = hop_words(h, &shown);
            if let Some(o) = h.as_object_mut() {
                o.insert("words".into(), Value::String(words));
            }
        }
    }
    if !export {
        return Ok(Some(v));
    }
    // The record a break is at, as kiln keeps it, too: one that no longer checks among them.
    if let Some(b) = v.pointer("/break/record").and_then(Value::as_str)
        && !read.iter().any(|d| d == b)
    {
        read.push(b.to_owned());
    }
    let rows: Vec<(String, String, i64, String)> = store
        .read(move |c| {
            let mut q = c.prepare("SELECT digest, envelope, taken_at, came_from FROM custody WHERE digest = ?1")?;
            let mut out = Vec::new();
            for d in &read {
                if let Some(row) = q.query_row([d], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()? {
                    out.push(row);
                }
            }
            Ok(out)
        })
        .await?;
    let records: Vec<Value> = rows
        .into_iter()
        .map(|(digest, envelope, taken_at, came_from)| json!({ "digest": digest, "taken_at": taken_at, "came_from": came_from, "bundle": serde_json::from_str::<Value>(&envelope).unwrap_or(Value::String(envelope)) }))
        .collect();
    let revocations: Vec<Value> = crate::identities::revocations(store).await?.into_iter().map(|r| json!({ "serial": r.serial, "key": r.key, "at": r.at, "reason": r.reason, "by": r.by })).collect();
    if let Some(o) = v.as_object_mut() {
        o.insert("records".into(), Value::Array(records));
        o.insert("root".into(), Value::String(pki.ca_pem.clone()));
        o.insert("revocations".into(), Value::Array(revocations));
    }
    Ok(Some(v))
}

/// Custodian `uri` in words: "kiln", "kiln's CD", "ada", "the runner on fedora-2", "run 812's leg
/// build on fedora-2", "agent session 01a1751c at epoch 3 (on adacachy)", "kiln's store of bytes
/// (s3)", "node m02 (cluster e2e)", "pod shop/web-1 (cluster e2e)"; as it's written when it's none
/// of those. A machine is named as `shown` gives it.
fn custodian_words(uri: &str, shown: &dyn Fn(&str) -> String) -> String {
    if let Ok(id) = Identity::parse(uri) {
        return match id.actor {
            Actor::Person { name } => name,
            Actor::Agent { agent, epoch, held_by, .. } => format!("agent session {} at epoch {epoch}{}", agent.get(..8).unwrap_or(&agent), held_by.map(|h| format!(" (on {})", shown(&h.machine))).unwrap_or_default()),
            Actor::Runner { machine, .. } => format!("the runner on {}", shown(&machine)),
            Actor::Leg { machine, run, leg, .. } => format!("run {run}'s leg {leg} on {}", shown(&machine)),
            Actor::Server => "kiln".into(),
            Actor::Cd => "kiln's CD".into(),
        };
    }
    if let Some((_, kind)) = uri.rsplit_once("/blobs/") {
        return format!("kiln's store of bytes ({kind})");
    }
    if let Some(runner) = uri.strip_prefix("reach-v1:") {
        return format!("runner {}, from before custody", shown(runner));
    }
    // A deploy's cluster, its nodes and its pods (kiln_ci::identity::deployed).
    if let Some(rest) = uri.strip_prefix("kubernetes://") {
        return match rest.split('/').collect::<Vec<_>>().as_slice() {
            [cluster] => format!("cluster {cluster}"),
            [cluster, "node", node] => format!("node {node} (cluster {cluster})"),
            [cluster, namespace, "pod", pod] => format!("pod {namespace}/{pod} (cluster {cluster})"),
            _ => uri.to_owned(),
        };
    }
    uri.to_owned()
}

/// A walk's hop in words: "the runner on box-b received binaries/kiln-x from kiln", a machine
/// named as `shown` gives it.
fn hop_words(h: &Value, shown: &dyn Fn(&str) -> String) -> String {
    let s = |k: &str| h.get(k).and_then(Value::as_str).unwrap_or_default();
    let names: Vec<String> = h
        .get("subject")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|t| match t.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
            Some(n) => n.to_owned(),
            None => t.get("digest").and_then(Value::as_object).and_then(|d| d.iter().next()).map_or_else(|| "something unnamed".to_owned(), |(alg, hex)| format!("{alg}:{}", hex.as_str().and_then(|x| x.get(..12)).unwrap_or_default())),
        })
        .collect();
    let things = match names.as_slice() {
        [] => "nothing named".to_owned(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    };
    // kiln's take, or its receipt, for someone who couldn't sign: "kiln for sam".
    let by = h.get("taken_for").and_then(Value::as_str).map_or_else(|| custodian_words(s("custodian"), shown), |who| format!("kiln for {}", custodian_words(who, shown)));
    let to = |before: &str| match s("to") {
        "" => String::new(),
        to => format!("{before}to {}", custodian_words(to, shown)),
    };
    let made = h.get("made_from").and_then(Value::as_array).is_some_and(|m| !m.is_empty());
    match s("kind") {
        "received" => format!("{by} received {things} from {}", custodian_words(s("from"), shown)),
        "passed-on" => format!("{by} passed {things} on{}", to(" ")),
        "taken" => format!("{by} took {things}"),
        _ if made => format!("{by} handed on {things}, made from what it received{}", to(", ")),
        _ if h.get("began") == Some(&Value::Bool(true)) => format!("{by} handed on {things}{}, where the chain begins", to(" ")),
        _ => format!("{by} handed {things} on{}", to(" ")),
    }
}

/// The record a break is at.
fn break_record(b: &Break) -> Option<&str> {
    Some(match b {
        Break::NoRecord { followed_by: Some(r), .. } => r,
        Break::NoRecord { record, .. }
        | Break::NotCustody { record, .. }
        | Break::Unreadable { record, .. }
        | Break::Changed { record, .. }
        | Break::NoCommonDigest { record, .. }
        | Break::NotHandedOn { record, .. }
        | Break::NotAll { record, .. }
        | Break::NotTheTaker { record, .. }
        | Break::NotTheGiver { record, .. }
        | Break::NotTheHolder { record, .. }
        | Break::NoHandOn { record, .. }
        | Break::OutOfOrder { record, .. }
        | Break::TakenNotByServer { record, .. }
        | Break::NotEntitled { record, .. } => record,
        Break::Gap(_) => return None,
    })
}

/// The newest `?1` breaks, read in their order off `custody_breaks_at` (migration `dbq_custody`) rather
/// than every break read and sorted (the DB query audit, 2026-11-06: the custody page, 2.3 ms at
/// production's size, 0.4 ms). `LIMIT +?1`: SQLite plans by a bare `?1` there, preparing the statement
/// again each time it's bound, and not by `+?1`.
const BREAKS: &str = "SELECT record, kind, detail, around, refused, at, relay FROM custody_breaks ORDER BY at DESC, id DESC LIMIT +?1";

/// The breaks kiln found, newest first.
pub async fn breaks(store: &Store, limit: usize) -> anyhow::Result<Vec<Value>> {
    let n = i64::try_from(limit).unwrap_or(i64::MAX);
    store
        .read(move |c| {
            let mut q = c.prepare(BREAKS)?;
            let rows = q
                .query_map([n], |r| {
                    let around: String = r.get(3)?;
                    // The relay, when it's in what one passed on (docs/ci.md W27-R08's relays).
                    let relay: String = r.get(6)?;
                    Ok(json!({ "record": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "detail": r.get::<_, String>(2)?, "around": serde_json::from_str::<Value>(&around).unwrap_or(Value::Null), "refused": r.get::<_, String>(4)?, "at": r.get::<_, i64>(5)?, "relay": relay }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// The newest `?1` refusals, read in their order off `transfer_refusals_at` (migration `dbq_custody`) rather than
/// every refusal read and sorted (the DB query audit, 2026-11-06: `kiln transfers refused`, 2.7 ms at production's size
/// and 30 ms at ten times its refusals; 0.44 and 0.72 ms). `LIMIT +?1`, as [`BREAKS`]'.
const REFUSALS: &str = "SELECT digest, act, signer, why, came_from, at, epoch, superseded_by FROM transfer_refusals ORDER BY at DESC, id DESC LIMIT +?1";

/// What kiln refused, newest first.
pub async fn refusals(store: &Store, limit: usize) -> anyhow::Result<Vec<Value>> {
    let n = i64::try_from(limit).unwrap_or(i64::MAX);
    store
        .read(move |c| {
            let mut q = c.prepare(REFUSALS)?;
            let rows = q
                .query_map([n], |r| {
                    Ok(json!({ "digest": r.get::<_, String>(0)?, "act": r.get::<_, String>(1)?, "signer": r.get::<_, String>(2)?, "why": r.get::<_, String>(3)?, "came_from": r.get::<_, String>(4)?, "at": r.get::<_, i64>(5)?, "epoch": r.get::<_, Option<i64>>(6)?, "superseded_by": r.get::<_, String>(7)? }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// `kiln custody`: a thing's chain of custody, and the breaks found, over the API (W27-R08).
#[derive(clap::Args)]
pub struct CustodyArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The admin token, or a personal access token with `audit.read`.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: CustodyCmd,
}

#[derive(clap::Subcommand)]
enum CustodyCmd {
    /// A thing's chain of custody, walked now: from a record's digest, or a thing's
    /// (`sha256:…`, `gitCommit:…`), each hop with who signed it, or where it breaks.
    Show { digest: String },
    /// The breaks kiln found in chains of custody, newest first.
    Breaks,
    /// A thing's chain walked from where it's used back to where it was made (a record's digest,
    /// or a thing's), each hop in words, or its first break.
    Walk {
        digest: String,
        /// Also write the walk to this file as JSON, with every record it read as signed, kiln's
        /// CA certificate and its revocations: what an auditor checks it with, without kiln.
        #[arg(long)]
        export: Option<std::path::PathBuf>,
    },
}

/// `kiln transfers`: what kiln refused because its signer no longer held what it acted on, and the
/// hand-overs that went unsigned.
#[derive(clap::Args)]
pub struct TransfersArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The admin token, or a personal access token with `audit.read`.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: TransfersCmd,
}

#[derive(clap::Subcommand)]
enum TransfersCmd {
    /// What kiln refused, newest first: when, what, who signed it, and why.
    Refused,
    /// Agent sessions' hand-overs that went unsigned, newest first: when, the session and the
    /// epoch it went on at, from whom to whom, and why.
    Unsigned,
}

/// `kiln custody`.
pub async fn cli(args: CustodyArgs, cfg: &crate::config::Config) -> anyhow::Result<()> {
    use crate::ci::cli::{Api, enc, when};
    use crate::http::client::Method;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        CustodyCmd::Show { digest } => {
            let v = api.call_api(Method::Get, &format!("/api/custody?digest={}", enc(&digest)), None).await?;
            if v.get("unbroken").and_then(Value::as_bool) == Some(true) {
                let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
                println!("unbroken: {} hop(s) from {}", hops.len(), s(&v, "from"));
                for h in hops {
                    let whom = if s(&h, "kind") == "received" { format!("from {}", s(&h, "from")) } else { format!("to {}", s(&h, "to")) };
                    println!("{}  {:<10} {}  {whom}  {}", when(h.get("at").and_then(Value::as_i64)), s(&h, "kind"), s(&h, "signer"), s(&h, "record"));
                }
            } else {
                let b = v.get("break").cloned().unwrap_or(Value::Null);
                println!("broken at {}: {}", s(&b, "record"), s(&b, "detail"));
            }
        }
        CustodyCmd::Breaks => {
            let v = api.call_api(Method::Get, "/api/custody/breaks", None).await?;
            for b in v.get("breaks").and_then(Value::as_array).cloned().unwrap_or_default() {
                println!("{}  {:<16} {}  {}", when(b.get("at").and_then(Value::as_i64)), s(&b, "kind"), s(&b, "record"), s(&b, "detail"));
            }
        }
        CustodyCmd::Walk { digest, export } => {
            let v = api.call_api(Method::Get, &format!("/api/custody/walk?digest={}{}", enc(&digest), if export.is_some() { "&export=true" } else { "" }), None).await?;
            if v.get("unbroken").and_then(Value::as_bool) == Some(true) {
                let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
                println!("{digest}: unbroken, each of its {} hop(s) signed", hops.len());
                for h in hops {
                    println!("  {}  {}", when(h.get("at").and_then(Value::as_i64)), s(&h, "words"));
                }
            } else {
                let b = v.get("break").cloned().unwrap_or(Value::Null);
                println!("{digest}: broken at {} ({}): {}", s(&b, "record"), s(&b, "kind"), s(&b, "detail"));
            }
            if let Some(path) = export {
                std::fs::write(&path, serde_json::to_vec_pretty(&v)?)?;
                let n = v.get("records").and_then(Value::as_array).map_or(0, Vec::len);
                println!("exported to {}: {n} record(s) as signed, kiln's CA certificate and its revocations", path.display());
            }
        }
    }
    Ok(())
}

/// `kiln transfers`.
pub async fn transfers_cli(args: TransfersArgs, cfg: &crate::config::Config) -> anyhow::Result<()> {
    use crate::ci::cli::{Api, when};
    use crate::http::client::Method;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        TransfersCmd::Refused => {
            let v = api.call_api(Method::Get, "/api/transfers/refused", None).await?;
            for r in v.get("refused").and_then(Value::as_array).cloned().unwrap_or_default() {
                let signer = s(&r, "signer");
                println!("{}  {}  {}  {}", when(r.get("at").and_then(Value::as_i64)), s(&r, "act"), if signer.is_empty() { "unknown signer".to_owned() } else { signer }, s(&r, "why"));
            }
        }
        TransfersCmd::Unsigned => {
            let v = api.call_api(Method::Get, "/api/transfers/unsigned", None).await?;
            for u in v.get("unsigned").and_then(Value::as_array).cloned().unwrap_or_default() {
                let epoch = u.get("epoch").and_then(Value::as_i64).unwrap_or_default();
                println!("{}  session {} to epoch {epoch}  from {} to {}  {}", when(u.get("at").and_then(Value::as_i64)), s(&u, "session"), or_unknown(&s(&u, "gave")), or_unknown(&s(&u, "took")), s(&u, "why"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::identity::custody::{Body, to_value};
    use kiln_ci::identity::statement::{Statement, Subject, bundle, payload, to_sign};
    use rcgen::SigningKey as _;

    const ORG: &str = "hyper-light";

    /// Goal 9, plan 01a1a87b2cec step 3: what custody checks or signs as runners' agents act, refuse and hand over, and as
    /// kiln takes a session it resumes, is done off the shards: no signature is checked or made in those functions' own
    /// code, only in the compute or blocking pool's jobs they hand it to (`verify_off`, `server_signed_off`, `act_check`
    /// in a job). What each finds is the W27 tests'.
    #[test]
    fn g9_s3_custodys_signatures_are_checked_and_made_off_the_shards() {
        let code: String = include_str!("custody.rs").split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default().lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
        let body = |head: &str| code.split(head).nth(1).and_then(|rest| rest.split("\n}\n").next()).unwrap_or_default().to_owned();
        for f in ["pub async fn act_counts(", "async fn refused_act_of(", "pub async fn take_for(", "pub async fn transfer(", "pub async fn on_runner("] {
            let b = body(f);
            assert!(!b.is_empty(), "{f} not found");
            assert!(!b.contains("statement::verify(") && !b.contains("server_signed(pki"), "{f} checks or signs on its shard: {b}");
        }
        assert!(body("pub async fn act_counts(").contains("crate::rt::compute(move || (act_check("), "act_counts checks in a job");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a record that waited for this one is checked with kiln's root and every
    /// revocation handed to its loads' compute jobs as they're shared, not copied on its shard for each of its two
    /// loads (a list that grows by one with each certificate kiln supersedes). What the check finds is the W27 tests'.
    #[test]
    fn g9_s3_a_checks_loads_share_the_root_and_revocations() {
        let code: String = include_str!("custody.rs").split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default().lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
        let load_with = code.split("async fn load_with<").nth(1).and_then(|rest| rest.split("\nfn rows_on(").next()).unwrap_or_default();
        assert!(load_with.contains("Arc::clone(root)") && load_with.contains("Arc::clone(revocations)"), "{load_with}");
        assert!(!load_with.contains("root.to_vec()") && !load_with.contains("revocations.to_vec()"), "{load_with}");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the rarer paths that sign or check a statement as they go (kiln's own hand-ons,
    /// receipts, passes and carried approvals; a person's own receipt or hand-on; a walk's break at a record kiln keeps;
    /// a review's signed verdicts; a leg's inputs; KilnCD's records; the store of bytes'; a commit kiln makes) hand it
    /// to the compute or blocking pool: none signs or checks in its own code. What each finds is the W27 tests'.
    #[test]
    fn g9_s3_the_rare_signing_paths_sign_and_check_off_the_shards() {
        // A file's code outside its tests (all of it, with none), each line without its comment.
        let code = |file: &str| file.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default().lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
        let (custody, verdicts, signed, cd, blobs, revision) = (code(include_str!("custody.rs")), code(include_str!("ci/verdicts.rs")), code(include_str!("ci/signed.rs")), code(include_str!("cd/custody.rs")), code(include_str!("custody/blobs.rs")), code(include_str!("ci/revision.rs")));
        let promotion = code(include_str!("deploy/promotion.rs"));
        let (vsa, lands, handovers) = (code(include_str!("verify.rs")), code(include_str!("ci/lands.rs")), code(include_str!("handovers.rs")));
        // Where a function's body ends: a free function's brace, or a method's in its `impl`.
        let (free, method) = ("\n}\n", "\n    }\n");
        let moved = [
            (custody.as_str(), "pub async fn hand_work(", free, "server_signed_off(&hub.pki, "),
            (custody.as_str(), "pub async fn receive_for_because(", free, "server_signed_off(&hub.pki, "),
            (custody.as_str(), "pub async fn pass_on(", free, "server_signed_off(&hub.pki, "),
            (custody.as_str(), "pub async fn carry(", free, "server_signed_off(&hub.pki, "),
            (custody.as_str(), "pub async fn person_receipt(", free, "verify_off(bundle, "),
            (custody.as_str(), "pub async fn person_hand_on(", free, "verify_off(bundle, "),
            (custody.as_str(), "async fn unchecked(", free, "crate::rt::compute(move || checked_as_taken("),
            (verdicts.as_str(), "pub async fn take(", free, "crate::custody::verify_off(bundle, "),
            (verdicts.as_str(), "pub async fn holding(", free, "crate::rt::compute_map(rows, "),
            (signed.as_str(), "async fn hand_to_runner(", free, "crate::custody::server_signed_off(&hub.pki, "),
            (cd.as_str(), "async fn as_cd(", free, "crate::custody::cd_signed_off(&hub.pki, "),
            (cd.as_str(), "async fn as_server(", free, "crate::custody::server_signed_off(&hub.pki, "),
            (blobs.as_str(), "async fn keep(", method, "super::server_signed_off(&self.pki, "),
            (revision.as_str(), "pub(crate) async fn sign_as_kiln(", free, "crate::rt::blocking(move || signed_by_kiln("),
            (signed.as_str(), "async fn take(", free, "a.check(&pki.ca_der, b, at)"),
            (signed.as_str(), "pub async fn judged(", free, "a.check(&pki.ca_der, &b, r.taken_at)"),
            (signed.as_str(), "async fn receipt(", free, "crate::rt::blocking(move || crate::identities::sign_as(&pki, "),
            (promotion.as_str(), "pub async fn hand_on(", free, "crate::custody::cd_signed_off(&d.hub.pki, "),
            (promotion.as_str(), "pub async fn receipt(", free, "crate::custody::cd_signed_off(&d.hub.pki, "),
            (vsa.as_str(), "pub async fn vsa(", free, "crate::identities::sign_as_off(&hub.pki, "),
            (lands.as_str(), "async fn keep(", free, "crate::identities::sign_as_off(&hub.pki, "),
            (handovers.as_str(), "pub(crate) async fn worded(", method, "crate::custody::word_off(&self.pki, "),
        ];
        for (src, head, end, off) in moved {
            let b = src.split(head).nth(1).and_then(|rest| rest.split(end).next()).unwrap_or_default();
            assert!(!b.is_empty(), "{head} not found");
            for on_shard in ["statement::verify(", "server_signed(&", "cd_signed(&", "sign_own(", "against.check(hub", "sign_as(&hub.pki", "custody::word(&self.pki"] {
                assert!(!b.contains(on_shard), "{head} calls {on_shard} on its shard: {b}");
            }
            assert!(b.contains(off), "{head} doesn't hand its signature or check to a pool ({off}): {b}");
        }
    }

    const FEDORA: &str = "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed";
    const JUPITER: &str = "kiln://hyper-light/person/ada/machine/jupiter/instance/01a18b00";
    const LEFT: &str = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/0/machine/fedora/instance/01a14c4f";

    /// A custodian with a key, certified by `pki` as `uri`.
    struct Signer {
        uri: String,
        key: rcgen::KeyPair,
        der: Vec<u8>,
        /// Its certificate, as kiln records what it issues.
        pem: String,
    }

    impl Signer {
        fn new(pki: &crate::pki::Pki, uri: &str) -> anyhow::Result<Self> {
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::make_csr_with_key("kiln-identity", &key)?;
            let pem = pki.issue_identity(&csr, &Identity::parse(uri)?, &[], time::Duration::hours(1))?;
            Ok(Self { uri: uri.to_owned(), key: rcgen::KeyPair::from_pem(&key)?, der: crate::pki::der_of(&pem)?, pem })
        }

        /// `s`'s payload as this custodian signs it: its body naming it.
        fn payload(&self, s: &Statement) -> anyhow::Result<Vec<u8>> {
            let mut s = s.clone();
            if let Some(o) = s.body.as_object_mut() {
                o.insert("by".into(), Value::String(self.uri.clone()));
            }
            Ok(payload(&s)?)
        }

        /// The digest `s` has once this custodian signs it.
        fn digest(&self, s: &Statement) -> anyhow::Result<String> {
            Ok(statement::digest(PAYLOAD_CUSTODY, &self.payload(s)?))
        }

        fn sign(&self, s: &Statement) -> anyhow::Result<Value> {
            let p = self.payload(s)?;
            Ok(bundle(PAYLOAD_CUSTODY, &p, &self.key.sign(&to_sign(PAYLOAD_CUSTODY, &p))?, &self.der))
        }
    }

    fn thing(name: &str, c: char) -> Subject {
        Subject { name: name.into(), digest: BTreeMap::from([("sha256".to_owned(), c.to_string().repeat(64))]), ..Subject::default() }
    }

    fn record(kind: Kind, what: &Subject, to: Option<&str>, from: Option<&str>, follows: &[&str], stream: &str, seq: u64) -> Statement {
        let b = Body { to: to.map(str::to_owned), from: from.map(str::to_owned), why: "a test".into(), ..Body::new(kind, "") };
        Statement { subject: vec![what.clone()], follows: follows.iter().map(|d| (*d).to_owned()).collect(), stream: Some(stream.into()), seq: Some(seq), ..Statement::new(to_value(&b)) }
    }

    async fn count(store: &Store, table: &'static str) -> anyhow::Result<i64> {
        store.call(move |c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)).await
    }

    /// W27-R08 (the Transfers plan's step 2): records kept once by digest, each hop checked as
    /// it's taken (a receipt before its hand-on waits for it), a chain walked by a record's digest
    /// or a thing's, a break recorded with the records around it, and a statement signed at an
    /// epoch its session left refused and recorded with its signer and envelope.
    #[test]
    fn w27_r08_records_kept_breaks_and_refusals_recorded() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let (root, org) = (pki.ca_der.clone(), pki.organization());
            let at = crate::util::now_unix();
            let server = server_uri(&org);
            let (kiln, fedora, jupiter) = (Signer::new(&pki, &server)?, Signer::new(&pki, &FEDORA.replace(ORG, &org))?, Signer::new(&pki, &JUPITER.replace(ORG, &org))?);
            let (fedora_uri, jupiter_uri) = (FEDORA.replace(ORG, &org), JUPITER.replace(ORG, &org));
            let commit = thing("commit 4c1e2a", 'c');
            // The receipt comes first: it waits for the hand-on, and nothing breaks.
            let hand_on = record(Kind::HandedOn, &commit, Some(&fedora_uri), None, &[], "land", 1);
            let hand_on_digest = kiln.digest(&hand_on)?;
            let receipt = kiln_or(take(&store, &root, &org, &fedora.sign(&record(Kind::Received, &commit, None, Some(&server), &[&hand_on_digest], "legs", 1))?, "runner fedora-2", at).await?)?;
            assert_eq!(receipt.broke, None);
            let given = kiln_or(take(&store, &root, &org, &kiln.sign(&hand_on)?, "kiln", at).await?)?;
            assert_eq!((given.digest.as_str(), given.broke.as_deref(), given.again), (hand_on_digest.as_str(), None, false));
            assert_eq!(count(&store, "custody_breaks").await?, 0);
            // Taken again, it's kept once.
            assert!(kiln_or(take(&store, &root, &org, &kiln.sign(&hand_on)?, "kiln", at).await?)?.again);
            assert_eq!(count(&store, "custody").await?, 2);
            // The chain, walked from the receipt or from the commit's digest.
            for from in [receipt.digest.clone(), format!("sha256:{}", "c".repeat(64))] {
                let v = chain(&store, &root, &org, &from, at).await?.ok_or_else(|| anyhow::anyhow!("no chain from {from}"))?;
                assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
                let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
                assert_eq!(hops.iter().map(|h| h.get("custodian").cloned().unwrap_or_default()).collect::<Vec<_>>(), vec![json!(fedora_uri), json!(server)]);
                assert!(hops.iter().all(|h| h.get("signer").and_then(Value::as_str).is_some_and(|s| !s.is_empty())), "each hop names who signed it");
            }
            assert_eq!(chain(&store, &root, &org, "sha256:feed", at).await?, None);
            // An offer no one takes: open, the giver holding it; lapsed once its time has passed.
            let offered = kiln_or(take(&store, &root, &org, &fedora.sign(&record(Kind::HandedOn, &thing("image kiln:4c1e2a", 'e'), Some(&jupiter_uri), None, &[&receipt.digest], "legs", 2))?, "runner fedora-2", at).await?)?;
            let v = chain(&store, &root, &org, &offered.digest, at).await?.unwrap_or_default();
            assert_eq!((v.get("open"), v.get("lapsed"), v.get("held_by")), (Some(&json!(true)), Some(&json!(false)), Some(&json!(fedora_uri))), "{v}");
            let v = chain(&store, &root, &org, &offered.digest, at + LAPSE_AFTER + 1).await?.unwrap_or_default();
            assert_eq!((v.get("lapsed"), v.get("held_by")), (Some(&json!(true)), Some(&json!(fedora_uri))), "{v}");
            // jupiter takes what was handed to fedora-2: kept, the break recorded with the records
            // around it, and audited.
            let wrong = kiln_or(take(&store, &root, &org, &jupiter.sign(&record(Kind::Received, &commit, None, Some(&server), &[&hand_on_digest], "legs", 1))?, "runner jupiter", at).await?)?;
            assert!(wrong.broke.as_deref().is_some_and(|b| b.contains("not to") && b.contains(&jupiter_uri)), "{wrong:?}");
            let listed = breaks(&store, LIST_MAX).await?;
            assert_eq!(listed.len(), 1);
            assert_eq!(listed.first().and_then(|b| b.get("kind")), Some(&json!("not-the-taker")));
            assert_eq!(listed.first().and_then(|b| b.get("around")), Some(&json!([wrong.digest, hand_on_digest])));
            let v = chain(&store, &root, &org, &wrong.digest, at).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
            assert_eq!(v.get("unbroken"), Some(&json!(false)));
            assert_eq!(breaks(&store, LIST_MAX).await?.len(), 1, "the same break is recorded once");
            // The session's epoch 0 left (its certificate revoked as it went on): what its copy
            // signs after is refused, and recorded naming it.
            let left_uri = LEFT.replace(ORG, &org);
            let copy = Signer::new(&pki, &left_uri)?;
            crate::identities::record(&store, "agent", &left_uri, &copy.pem).await?;
            crate::identities::revoke(&store, crate::identities::Target::Subject(left_uri.clone()), "epoch 0 left: moved on to epoch 1, on runner adacachy (instance 01a14794)", "kiln", at - 10).await?;
            let stale = take(&store, &root, &org, &copy.sign(&record(Kind::HandedOn, &thing("goal met", 'd'), Some(&server), None, &[], "acts", 1))?, "runner fedora", at).await?;
            assert!(matches!(&stale, Err(Refused::Unsigned(why)) if why.contains("epoch 0 left")), "{stale:?}");
            let r = refusals(&store, LIST_MAX).await?;
            assert_eq!(r.len(), 1);
            let first = r.first().cloned().unwrap_or_default();
            assert_eq!((first.get("signer"), first.get("came_from")), (Some(&json!(left_uri)), Some(&json!("runner fedora"))));
            assert!(first.get("digest").and_then(Value::as_str).is_some_and(|d| d.starts_with("sha256:")));
            assert_eq!(count(&store, "custody").await?, 4, "a refused statement isn't kept as custody");
            let audited: i64 = store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM audit2 WHERE action IN ('custody.break', 'custody.refused')", [], |r| r.get(0))?)).await?;
            assert_eq!(audited, 2);
            // fedora-2's certificate revoked in the second kiln took its receipt: what kiln took
            // before stays taken, and its chain still holds.
            crate::identities::record(&store, "runner", &fedora_uri, &fedora.pem).await?;
            crate::identities::revoke(&store, crate::identities::Target::Subject(fedora_uri.clone()), "runner forgotten", "ada", at).await?;
            let v = chain(&store, &root, &org, &receipt.digest, at + 1).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
            assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a record new to kiln is one call to the store's thread once its revocations
    /// were read, whether it follows nothing or a record kiln has (its hop checked with it); one kiln has already is one
    /// call; and a revocation added after they were read is seen before what it refuses is kept: the store's thread
    /// finds them stale, they're read again, and the record is checked again and refused.
    #[test]
    fn g9_s5_a_new_record_is_one_call_to_the_stores_thread() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let (root, org) = (pki.ca_der.clone(), pki.organization());
            let at = crate::util::now_unix();
            let server = server_uri(&org);
            let fedora_uri = FEDORA.replace(ORG, &org);
            let (kiln, fedora) = (Signer::new(&pki, &server)?, Signer::new(&pki, &fedora_uri)?);
            crate::identities::record(&store, "runner", &fedora_uri, &fedora.pem).await?;
            crate::identities::revoked(&store).await?;
            let commit = thing("commit 9c5e1a", 'a');
            let hand_on = record(Kind::HandedOn, &commit, Some(&fedora_uri), None, &[], "land", 1);
            let before = store.asked();
            let given = kiln_or(take(&store, &root, &org, &kiln.sign(&hand_on)?, "kiln", at).await?)?;
            assert_eq!((store.asked() - before, given.again, given.broke.as_deref()), (1, false, None), "a record that follows nothing: one call");
            let before = store.asked();
            let receipt = fedora.sign(&record(Kind::Received, &commit, None, Some(&server), &[&given.digest], "legs", 1))?;
            let took = kiln_or(take(&store, &root, &org, &receipt, "runner fedora-2", at).await?)?;
            assert_eq!((store.asked() - before, took.again, took.broke.as_deref()), (1, false, None), "one that follows a record kiln has: one call, and its hop holds");
            let before = store.asked();
            assert!(kiln_or(take(&store, &root, &org, &receipt, "runner fedora-2", at).await?)?.again);
            assert_eq!(store.asked() - before, 1, "one kiln has: one call");
            // fedora-2's certificate revoked after the revocations were read: its next record isn't kept.
            crate::identities::revoke(&store, crate::identities::Target::Subject(fedora_uri.clone()), "runner forgotten", "ada", at - 1).await?;
            let late = take(&store, &root, &org, &fedora.sign(&record(Kind::Received, &thing("commit 9c5e1b", 'b'), None, Some(&server), &[], "legs", 2))?, "runner fedora-2", at).await?;
            assert!(matches!(&late, Err(Refused::Unsigned(why)) if why.contains("runner forgotten")), "{late:?}");
            assert_eq!(count(&store, "custody").await?, 2, "the revoked signer's record isn't kept");
            assert_eq!(store.revoked_kept().map(|r| r.list.len()), Some(1), "the revocations were read again");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a mirror's receipt of kiln's hand-on, taken while kiln keeps the hand-on beside the
    /// store as it checked ([`recent`]), has its hop checked with it as the receipt is read, and comes out as taking it with
    /// the hand-on read back from the store does: the same digests, the same break (none for a hop that holds; the same
    /// one for a hop that doesn't), the same rows.
    #[test]
    fn g9_s5_a_receipts_hop_with_kilns_hand_on_kept_beside_it_is_as_read_back() -> anyhow::Result<()> {
        // Room enough that other tests' hand-ons kept meanwhile don't push this one's out.
        recent::keep_for(16);
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let (root, org) = (pki.ca_der.clone(), pki.organization());
            let at = crate::util::now_unix();
            let server = server_uri(&org);
            let (fedora_uri, jupiter_uri) = (FEDORA.replace(ORG, &org), JUPITER.replace(ORG, &org));
            let (kiln, fedora) = (Signer::new(&pki, &server)?, Signer::new(&pki, &fedora_uri)?);
            // A hand-on to fedora (its receipt's hop holds) and one to jupiter (fedora's receipt of it breaks).
            for (n, to) in [(1, &fedora_uri), (2, &jupiter_uri)] {
                let batch = thing(&format!("conversation store s1 batch {n}"), 'e');
                let hand_on = kiln.sign(&record(Kind::HandedOn, &batch, Some(to.as_str()), None, &[], "feed", n))?;
                let receipt = fedora.sign(&record(Kind::Received, &batch, None, Some(&server), &[&digest_of(&hand_on)], "feed", n))?;
                let mut outcomes = Vec::new();
                for beside in [false, true] {
                    let store = Store::in_memory()?;
                    crate::identities::record(&store, "runner", &fedora_uri, &fedora.pem).await?;
                    crate::identities::revoked(&store).await?;
                    let given = kiln_or(take(&store, &root, &org, &hand_on, "kiln", at).await?)?;
                    if beside {
                        recent::keep(statement::verify(&hand_on, &root, &[], at).map_err(|e| anyhow::anyhow!("{e}"))?);
                    }
                    let served = recent::served_so_far();
                    let took = kiln_or(take(&store, &root, &org, &receipt, "runner fedora-2", at).await?)?;
                    anyhow::ensure!(!beside || recent::served_so_far() > served, "{n}: its hop checked with the hand-on kept beside the store");
                    outcomes.push((given.digest, took.digest, took.broke, count(&store, "custody").await?, count(&store, "custody_breaks").await?));
                }
                anyhow::ensure!(outcomes.first() == outcomes.get(1), "{n}: read back, then beside the store: {outcomes:?}");
                let broke = outcomes.first().and_then(|o| o.2.clone());
                anyhow::ensure!(broke.is_some() == (n == 2), "{n}: {broke:?}");
            }
            // One not kept beside the store: none found, its hop read back as before.
            anyhow::ensure!(recent::find("f".repeat(64).as_str()).is_none());
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: kiln's own record (a receipt or a hand-on of a feed's batch) is signed and read
    /// in one job and kept in one call to the store's thread: the bundle it signed is the one kept, and it checks.
    #[test]
    fn g9_s5_kilns_own_record_is_signed_and_kept_in_one_call() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = std::sync::Arc::new(crate::pki::Pki::open_scratch(&dir.path().join("pki"))?);
            crate::identities::own_identities(&store, &pki).await?;
            let fedora_uri = FEDORA.replace(ORG, &pki.organization());
            crate::identities::revoked(&store).await?;
            let hand_on = record(Kind::HandedOn, &thing("commit 9c5e1c", 'c'), Some(&fedora_uri), None, &[], "land", 1);
            let before = store.asked();
            let (bundle, taken) = sign_and_take(&store, &pki, hand_on, "kiln", crate::util::now_unix()).await?;
            let taken = kiln_or(taken?)?;
            assert_eq!((store.asked() - before, taken.again, taken.broke.as_deref()), (1, false, None));
            assert_eq!(digest_of(&bundle), taken.digest, "the bundle it signed is the one kept");
            assert!(kept(&store, &taken.digest).await?);
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a feed's batch's two records (the runner's hand-on, kiln's receipt of it) are read
    /// in one job and kept in one call to the store's thread, and come out as taking them one at a time does: the same
    /// digests and breaks (the receipt's hop checked with the hand-on as it was read), the same rows; taken again, both again.
    #[test]
    fn g9_s5_a_batchs_two_records_are_one_call_and_come_out_as_one_at_a_time() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = std::sync::Arc::new(crate::pki::Pki::open_scratch(&dir.path().join("pki"))?);
            let org = pki.organization();
            let (server, fedora_uri, jupiter_uri) = (server_uri(&org), FEDORA.replace(ORG, &org), JUPITER.replace(ORG, &org));
            let fedora = Signer::new(&pki, &fedora_uri)?;
            let at = crate::util::now_unix();
            // A batch handed on to kiln (its receipt's hop holds), and one handed on to jupiter that kiln took (it breaks).
            for (to, breaks) in [(&server, false), (&jupiter_uri, true)] {
                let batch = thing("conversation store s1 after 0 through 9", 'f');
                let given = fedora.sign(&record(Kind::HandedOn, &batch, Some(to.as_str()), None, &[], "feed", 1))?;
                let receipt = || {
                    let b = Body { from: Some(fedora_uri.clone()), why: "a test".into(), ..Body::new(Kind::Received, &server) };
                    Statement { subject: vec![batch.clone()], follows: vec![digest_of(&given)], ..Statement::new(to_value(&b)) }
                };
                let (one, two) = (Store::in_memory()?, Store::in_memory()?);
                for s in [&one, &two] {
                    crate::identities::own_identities(s, &pki).await?;
                    crate::identities::revoked(s).await?;
                }
                let before = one.asked();
                let (g, r) = take_batch(&one, &pki, given.clone(), "runner fedora-2", receipt(), at).await?;
                let (g, r) = (kiln_or(g?)?, kiln_or(r??)?);
                if !breaks {
                    assert_eq!(one.asked() - before, 1, "both kept in one call");
                }
                let g2 = kiln_or(take(&two, &pki.ca_der, &org, &given, "runner fedora-2", at).await?)?;
                let r2 = kiln_or(sign_and_take(&two, &pki, receipt(), "kiln", at).await?.1?)?;
                assert_eq!((&g, &r), (&g2, &r2), "as taking them one at a time");
                assert_eq!((g.broke.is_some(), r.broke.is_some()), (false, breaks), "{r:?}");
                for s in [&one, &two] {
                    assert_eq!((count(s, "custody").await?, count(s, "custody_follows").await?, count(s, "custody_breaks").await?), (2, 1, i64::from(breaks)));
                }
                // The same batch again: both again, nothing more kept.
                let (g, r) = take_batch(&one, &pki, given.clone(), "runner fedora-2", receipt(), at).await?;
                assert!(kiln_or(g?)?.again && kiln_or(r??)?.again);
                assert_eq!(count(&one, "custody").await?, 2);
            }
            Ok(())
        })
    }

    /// W27-R08: both sides of a hand-over name a session's timeline alike from the same events,
    /// and differently once an event differs or the epoch does.
    #[test]
    fn w27_r08_a_timeline_is_named_alike_by_both_sides() -> anyhow::Result<()> {
        let events: Vec<crate::harness::model::Event> = serde_json::from_value(json!([]))?;
        let a = timeline("s1", 3, &events);
        assert_eq!(a, timeline("s1", 3, &events));
        assert_ne!(a.digest, timeline("s1", 4, &events).digest);
        assert_ne!(a.digest, timeline("s2", 3, &events).digest);
        assert_eq!(a.digest.get(TIMELINE).map(String::len), Some(64));
        assert_eq!(at_epoch("kiln://o/person/p/agent/s1/epoch/3/machine/m/instance/i", 4).as_deref(), Some("kiln://o/person/p/agent/s1/epoch/4"));
        assert_eq!(at_epoch("kiln://o/person/p/machine/m/instance/i", 4), None);
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 4, the 14:18 drop): a hand-over's word as kiln's server
    /// sends it is applied only by the process it's for, at the epoch it holds the session at: not
    /// by another instance (a new build it was passed on to), not at an epoch the process left,
    /// not changed on the way, and not unsigned from a server that signs words. From a server that
    /// doesn't, it goes as before; and a command that isn't a hand-over's word isn't checked.
    #[test]
    fn w27_r08_a_hand_overs_word_goes_only_to_its_process() -> anyhow::Result<()> {
        use crate::harness::model::Command;
        use crate::runner::custody::word_for_me;
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            crate::identities::own_identities(&store, &pki).await?;
            let root = pki.ca_der.clone();
            let command = serde_json::to_string(&Command::HandedOver { error: None, unsure: false, by_newest: false, none_newer: false })?;
            let worded = word(&pki, "s1", &command, "fedora", "main", 3)?;
            let check = |json: &str, instance: &str, epoch: Option<u64>| word_for_me("s1", json, true, &root, "fedora", instance, epoch);
            assert_eq!(check(&worded, "main", Some(3)), Ok(()), "for this process at its epoch");
            assert_eq!(check(&worded, "main", None), Ok(()), "for this process, which let go of it");
            let passed = check(&worded, "01a17502", Some(4));
            assert!(passed.as_ref().is_err_and(|w| w.contains("for fedora (instance main) at epoch 3, not this process, fedora (instance 01a17502) at epoch 4")), "{passed:?}");
            assert!(check(&worded, "main", Some(4)).is_err(), "an epoch it left");
            let mut changed: Value = serde_json::from_str(&worded)?;
            if let Some(o) = changed.as_object_mut() {
                o.insert("error".into(), json!("let go of it"));
            }
            assert!(check(&changed.to_string(), "main", Some(3)).is_err_and(|w| w.contains("changed")));
            assert!(check(&command, "main", Some(3)).is_err_and(|w| w.contains("isn't signed")), "unsigned, from a server that signs words");
            assert_eq!(word_for_me("s1", &command, false, &root, "fedora", "main", Some(3)), Ok(()), "unsigned, from a server from before");
            assert_eq!(check(&serde_json::to_string(&Command::Interrupt)?, "01a17502", None), Ok(()), "not a hand-over's word");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a hand-over's word and a statement kiln signs as its server, made on the pools
    /// (`word_off`, `identities::sign_as_off`), are what the caller's thread made: the same command and word for the same
    /// process at the same epoch, a statement over the same payload, each verifying to kiln's root as its server.
    #[test]
    fn g9_s3_words_and_statements_signed_off_the_shards_are_as_before() -> anyhow::Result<()> {
        use crate::harness::model::Command;
        use crate::runner::custody::word_for_me;
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = std::sync::Arc::new(crate::pki::Pki::open_scratch(&dir.path().join("pki"))?);
            crate::identities::own_identities(&store, &pki).await?;
            let root = pki.ca_der.clone();
            let command = serde_json::to_string(&Command::HandedOver { error: None, unsure: false, by_newest: false, none_newer: false })?;
            let (here, off) = (word(&pki, "s1", &command, "fedora", "main", 3)?, word_off(&pki, "s1", &command, "fedora", "main", 3).await?);
            let (here_v, off_v): (Value, Value) = (serde_json::from_str(&here)?, serde_json::from_str(&off)?);
            anyhow::ensure!(unworded(&here_v) == unworded(&off_v), "the same command");
            anyhow::ensure!(word_for_me("s1", &off, true, &root, "fedora", "main", Some(3)) == Ok(()), "for that process at its epoch");
            let said = |v: &Value| -> anyhow::Result<(String, statement::Statement, String)> {
                let bundle: Value = serde_json::from_str(v.get("word").and_then(Value::as_str).unwrap_or_default())?;
                let c = statement::verify(&bundle, &root, &[], crate::util::now_unix()).map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok((c.digest, c.statement, c.custodian))
            };
            anyhow::ensure!(said(&here_v)? == said(&off_v)?, "the same word, by kiln's server");
            let payload = statement::payload(&statement::Statement::new(json!({"why": "a land of kiln's"}))).map_err(|e| anyhow::anyhow!("{e}"))?;
            let signed = crate::identities::sign_as_off(&pki, "server", PAYLOAD_CUSTODY, payload.clone()).await?;
            let c = statement::verify(&signed, &root, &[], crate::util::now_unix()).map_err(|e| anyhow::anyhow!("{e}"))?;
            let inline = crate::identities::sign_as(&pki, "server", PAYLOAD_CUSTODY, &payload)?;
            let d = statement::verify(&inline, &root, &[], crate::util::now_unix()).map_err(|e| anyhow::anyhow!("{e}"))?;
            anyhow::ensure!(c.digest == d.digest && c.custodian == d.custodian && c.payload_type == d.payload_type, "the same statement, by kiln's server");
            Ok(())
        })
    }

    /// W27-R08 (the Transfers plan's step 5, the twin): once kiln has certified a session's holder
    /// at its epoch, an act counts only from that holder, signed at that epoch, whatever it is (a
    /// goal met, a message to an agent, a CI run, a page's answer, a run on another machine asked
    /// before it's done). Unsigned, it's refused from a runner process that signs and that kiln
    /// certified for the session (its holder, or a copy left behind), and goes by its machine from
    /// one kiln hasn't certified for it (it just took it over) or a runner from before; signed by
    /// the holder, it counts; signed by the copy left behind at the epoch it left, or by a former
    /// holder of the current epoch, or beside another act than its own, it's refused, saying why.
    /// Each refusal is recorded with who signed it, the epoch it's from and the transfer that
    /// superseded that epoch: its holder's receipt of the session's timeline, never a newer one of
    /// work it signed since (the DB query audit, 2026-11-06).
    #[test]
    fn w27_r08_an_act_counts_only_from_its_holder_now() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (org, at) = (hub.pki.organization(), crate::util::now_unix());
            let uri = |epoch: u64, machine: &str, instance: &str| format!("kiln://{org}/person/ada/agent/s1/epoch/{epoch}/machine/{machine}/instance/{instance}");
            let (left_uri, former_uri, holder_uri) = (uri(0, "fedora", "main"), uri(1, "jupiter", "01a14700"), uri(1, "adacachy", "01a14794"));
            let (left, former, holder) = (Signer::new(&hub.pki, &left_uri)?, Signer::new(&hub.pki, &former_uri)?, Signer::new(&hub.pki, &holder_uri)?);
            // Certified in turn: the copy left behind at epoch 0, then at epoch 1 a process that
            // held it before its holder now.
            for (u, s) in [(&left_uri, &left), (&former_uri, &former), (&holder_uri, &holder)] {
                crate::identities::record(&hub.store, "agent", u, &s.pem).await?;
            }
            crate::identities::revoke(&hub.store, crate::identities::Target::Subject(left_uri.clone()), "epoch 0 left: moved on to epoch 1, on runner adacachy (instance 01a14794)", "kiln", at - 10).await?;
            // kiln's record of the receipt its holder took it with, of the session's timeline; and of
            // its receipt of work it took up since, newer, which is no transfer of the session.
            let h = holder_uri.clone();
            hub.store
                .call(move |c| {
                    for (digest, name, algorithm, taken) in [("sha256:5e55", "session s1 at epoch 0", TIMELINE, at), ("sha256:3a3a", "plan run r1 step 1", WORK, at + 1)] {
                        c.execute("INSERT INTO custody (digest, payload_type, kind, custodian, envelope, taken_at) VALUES (?1, ?2, 'received', ?3, '{}', ?4)", params![digest, PAYLOAD_CUSTODY, h, taken])?;
                        c.execute("INSERT INTO custody_things (record, name, algorithm, hex) VALUES (?1, ?2, ?3, 'e0')", params![digest, name, algorithm])?;
                    }
                    Ok(())
                })
                .await?;
            // An Open that says epoch 7 doesn't move the epoch acts are held to.
            hub.harness.raise_epoch("s1", 7);
            let asks = [
                json!({"op": "call", "session": "s1", "name": "goals.done", "input": {"id": "g1", "evidence": "built"}}),
                json!({"op": "call", "session": "s1", "name": "agents.send", "input": {"agent": "a2", "text": "the build is green"}}),
                json!({"op": "call", "session": "s1", "name": "ci.runs.trigger", "input": {"repo": "kiln"}}),
                json!({"op": "page", "session": "s1", "page": "p1", "do": "approve"}),
                json!({"op": "act", "session": "s1", "name": "run on jupiter: cargo build"}),
            ];
            let signed = |by: &Signer, epoch: u64, ask: &Value| -> anyhow::Result<Value> {
                use sha2::Digest as _;
                let hex: String = sha2::Sha256::digest(without(ask, "act").as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
                let body = serde_json::to_value(custody::Act { v: statement::VERSION, by: String::new(), session: "s1".into(), epoch, op: op_of(ask) })?;
                let subject = Subject { name: "an act of session s1".into(), digest: BTreeMap::from([("sha256".to_owned(), hex)]), ..Subject::default() };
                let p = by.payload(&Statement { subject: vec![subject], ..Statement::new(body) })?;
                let act = bundle(custody::PAYLOAD_ACT, &p, &by.key.sign(&to_sign(custody::PAYLOAD_ACT, &p))?, &by.der);
                let mut a = ask.clone();
                if let Some(o) = a.as_object_mut() {
                    o.insert("act".into(), Value::String(act.to_string()));
                }
                Ok(a)
            };
            let counts = |machine: &'static str, instance: &'static str, signs: bool, a: Value| {
                let hub = std::sync::Arc::clone(&hub);
                async move { act_counts(&hub, machine, instance, signs, "s1", &a, at).await }
            };
            for ask in &asks {
                let op = op_of(ask);
                assert!(counts("adacachy", "01a14794", true, ask.clone()).await?.is_err_and(|w| w.contains("isn't signed")), "{op}: unsigned, from the process that holds the key");
                assert_eq!(counts("adacachy", "z9", true, ask.clone()).await?, Ok(()), "{op}: unsigned, from a process kiln hasn't certified for it");
                assert_eq!(counts("adacachy", "01a14794", false, ask.clone()).await?, Ok(()), "{op}: unsigned, from a runner from before");
                assert_eq!(counts("adacachy", "01a14794", true, signed(&holder, 1, ask)?).await?, Ok(()), "{op}: signed by its holder at its epoch");
                let stale = counts("fedora", "main", true, signed(&left, 0, ask)?).await?;
                assert!(stale.as_ref().is_err_and(|w| w.contains("signed at epoch 0 of session s1, which went on at epoch 1, on runner adacachy (instance 01a14794)") && w.contains("(transfer sha256:5e55): it's from a copy left behind")), "{op}: {stale:?}");
                let unsigned_left = counts("fedora", "main", true, ask.clone()).await?;
                assert!(unsigned_left.as_ref().is_err_and(|w| w.contains("held session s1 at epoch 0, which went on at epoch 1") && w.ends_with("it's from a copy left behind")), "{op}: {unsigned_left:?}");
                let former_now = counts("jupiter", "01a14700", true, signed(&former, 1, ask)?).await?;
                assert!(former_now.as_ref().is_err_and(|w| w.contains("on runner jupiter (instance 01a14700), but the session is held, on runner adacachy (instance 01a14794)")), "{op}: {former_now:?}");
            }
            // Its holder's signature beside another act (its goal changed): refused.
            let mut changed = signed(&holder, 1, &json!({"op": "call", "session": "s1", "name": "goals.done", "input": {"id": "g1", "evidence": "built"}}))?;
            if let Some(id) = changed.pointer_mut("/input/id") {
                *id = json!("g2");
            }
            assert!(counts("adacachy", "01a14794", true, changed).await?.is_err_and(|w| w.contains("the act itself changed")));
            let r = refusals(&hub.store, LIST_MAX).await?;
            assert_eq!(r.len(), 4 * asks.len() + 1, "{r:?}");
            let from = |who: &str| -> Vec<&Value> { r.iter().filter(|x| x.get("signer") == Some(&json!(who))).collect() };
            let (stale, former_now) = (from(&left_uri), from(&former_uri));
            assert!(stale.len() == asks.len() && stale.iter().all(|x| x.get("epoch") == Some(&json!(0)) && x.get("superseded_by") == Some(&json!("sha256:5e55")) && x.get("came_from") == Some(&json!("runner fedora (instance main)"))), "{stale:?}");
            assert!(former_now.len() == asks.len() && former_now.iter().all(|x| x.get("epoch") == Some(&json!(1)) && x.get("superseded_by") == Some(&json!(""))), "{former_now:?}");
            let unsigned_left: Vec<&Value> = r.iter().filter(|x| x.get("signer") == Some(&json!("")) && x.get("epoch") == Some(&json!(0))).collect();
            assert!(unsigned_left.len() == asks.len() && unsigned_left.iter().all(|x| x.get("superseded_by") == Some(&json!("sha256:5e55"))), "{unsigned_left:?}");
            Ok(())
        })
    }

    /// W27-R08 (the Transfers plan's step 6): work changes hands with both sides signed. kiln, a
    /// plan run's driver, hands a step to its agent, acting for the person; the agent signs its
    /// receipt. Tried again, kiln takes it for the earlier agent (following that one's receipt) and
    /// hands it to the next, which signs; kiln knows the earlier agent no longer holds it. Taken over
    /// by a person with no key on the device that asked, kiln signs the receipt for them, and the
    /// walk reads that link as kiln's for them; with a key, the receipt is theirs, and one signed by
    /// another is refused.
    #[test]
    fn w27_r08_work_changes_hands_signed_by_both_sides() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            // The server's own signing key, as a server's start certifies it.
            crate::identities::own_identities(&hub.store, &hub.pki).await?;
            let (org, at) = (hub.pki.organization(), crate::util::now_unix());
            let (root, server) = (hub.pki.ca_der.clone(), server_uri(&org));
            let (ada, sam) = (person_uri(&org, "ada"), person_uri(&org, "sam"));
            let agent = |id: &str| format!("kiln://{org}/person/ada/agent/{id}/epoch/0");
            let held = |id: &str| format!("{}/machine/adacachy/instance/01a14794", agent(id));
            let (first, second) = (Signer::new(&hub.pki, &held("a1"))?, Signer::new(&hub.pki, &held("a2"))?);
            let step = work("plan run r1 step 2", &["r1", "2", "the build is green"]);
            let receipt = |by: &Signer, from: &str, hand_on: &str| -> anyhow::Result<Value> {
                let b = Body { from: Some(from.to_owned()), why: "step 2 of plan run r1".into(), ..Body::new(Kind::Received, "") };
                by.sign(&Statement { subject: vec![step.clone()], follows: vec![hand_on.to_owned()], ..Statement::new(to_value(&b)) })
            };
            let walked = |digest: String| {
                let (store, root, org) = (hub.store.clone(), root.clone(), org.clone());
                async move { chain(&store, &root, &org, &digest, at).await?.ok_or_else(|| anyhow::anyhow!("no chain")) }
            };
            // The first try: kiln's, for ada; the agent's receipt.
            let (handed, _) = hand_work(&hub, step.clone(), &agent("a1"), "step 2 of plan run r1", Giver::Kiln { acts_for: vec![ada.clone()] }).await?;
            let got = kiln_or(take(&hub.store, &root, &org, &receipt(&first, &server, &handed)?, "runner adacachy", at).await?)?;
            assert_eq!(walked(got.digest.clone()).await?.get("unbroken"), Some(&json!(true)));
            // Tried again: kiln's take for the first agent, following its receipt; the next's receipt.
            let (taken, _) = hand_work(&hub, step.clone(), &agent("a2"), "step 2 of plan run r1, tried again", Giver::For(held("a1"))).await?;
            let again = kiln_or(take(&hub.store, &root, &org, &receipt(&second, &held("a1"), &taken)?, "runner adacachy", at).await?)?;
            let v = walked(again.digest.clone()).await?;
            assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
            let custodians: Vec<&str> = v.get("hops").and_then(Value::as_array).into_iter().flatten().filter_map(|h| h.get("custodian").and_then(Value::as_str)).collect();
            assert_eq!(custodians, vec![held("a2").as_str(), server.as_str(), held("a1").as_str(), server.as_str()], "{v}");
            assert_eq!(work_taken_from(&hub.store, "a1").await?, Some((taken.clone(), step.name.clone(), agent("a2"))));
            assert_eq!(work_taken_from(&hub.store, "a2").await?, None);
            // Taken over by ada, with no key on the device that asked: kiln's receipt for her.
            let (to_ada, _) = hand_work(&hub, step.clone(), &ada, "step 2 of plan run r1, taken over", Giver::For(held("a2"))).await?;
            let for_ada = receive_for(&hub, &ada, step.clone(), &to_ada, &held("a2"), "step 2 of plan run r1, taken over").await?;
            let v = walked(for_ada).await?;
            assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
            let first_hop = v.get("hops").and_then(Value::as_array).and_then(|h| h.first()).cloned().unwrap_or_default();
            assert_eq!((first_hop.get("custodian"), first_hop.get("taken_for")), (Some(&json!(server)), Some(&json!(ada))), "kiln for ada, never ada: {first_hop}");
            assert!(first_hop.get("why").and_then(Value::as_str).is_some_and(|w| w.contains("no key on the device that asked")), "{first_hop}");
            // Taken over again, by ada with a key: her own receipt; one signed by sam isn't hers.
            let (to_ada_again, _) = hand_work(&hub, step.clone(), &ada, "step 2 of plan run r1, taken over again", Giver::For(held("a2"))).await?;
            let (ada_key, sam_key) = (Signer::new(&hub.pki, &ada)?, Signer::new(&hub.pki, &sam)?);
            let not_hers = person_receipt(&hub, &ada, &to_ada_again, &receipt(&sam_key, &held("a2"), &to_ada_again)?, "api:sam").await?;
            assert!(not_hers.as_ref().is_err_and(|w| w.contains("not by")), "{not_hers:?}");
            let hers = person_receipt(&hub, &ada, &to_ada_again, &receipt(&ada_key, &held("a2"), &to_ada_again)?, "api:ada").await?.map_err(|w| anyhow::anyhow!("{w}"))?;
            assert_eq!(walked(hers).await?.get("unbroken"), Some(&json!(true)));
            assert_eq!(handed_to(&hub.store, &ada, &step).await?, Some(to_ada_again));
            Ok(())
        })
    }

    /// A taken statement, or the refusal as an error.
    fn kiln_or(r: Result<Taken, Refused>) -> anyhow::Result<Taken> {
        r.map_err(|e| anyhow::anyhow!("refused: {e}"))
    }

    /// W27-R08 (the Transfers plan's step 3): a session's hand-over, as its process at the next
    /// epoch asks for its certificate. Its hand-on and its receipt, signed with the key being
    /// certified before kiln certified it, are kept, and walk as one transfer; asked again, the
    /// same. Not certified: a hand-on to another epoch, one without its receipt, and a copy left
    /// behind's (its epoch revoked), which is refused and recorded. A receipt of other than what
    /// was handed on is kept, its break recorded: the session goes on where it's held.
    #[test]
    fn w27_r08_a_transfer_is_kept_before_its_epoch_rises() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let (root, org) = (pki.ca_der.clone(), pki.organization());
            let at = crate::util::now_unix();
            let left_uri = LEFT.replace(ORG, &org);
            let left = Signer::new(&pki, &left_uri)?;
            let (next, issued) = (format!("kiln://{org}/person/ada/agent/01a1751c/epoch/1"), format!("kiln://{org}/person/ada/agent/01a1751c/epoch/1/machine/adacachy/instance/01a14794"));
            // The receiver's key, its receipt signed with it before kiln certifies it.
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::make_csr_with_key("kiln-identity", &key)?;
            let pem = pki.issue_identity(&csr, &Identity::parse(&issued)?, &[], time::Duration::hours(1))?;
            let signing = rcgen::KeyPair::from_pem(&key)?;
            let timeline = thing("session 01a1751c at epoch 0", 'e');
            let hand_on = |to: &str, epoch: u64, what: &Subject| -> anyhow::Result<Value> {
                let b = Body { to: Some(to.to_owned()), why: "taken over".into(), epoch: Some(epoch), ..Body::new(Kind::HandedOn, "") };
                left.sign(&Statement { subject: vec![what.clone()], ..Statement::new(to_value(&b)) })
            };
            let receipt = |follows: &str, what: &Subject| -> anyhow::Result<crate::proto::Unbundled> {
                let b = Body { from: Some(left_uri.clone()), why: "taken over".into(), epoch: Some(1), ..Body::new(Kind::Received, &next) };
                let p = payload(&Statement { subject: vec![what.clone()], follows: vec![follows.to_owned()], ..Statement::new(to_value(&b)) })?;
                let b64 = base64::engine::general_purpose::STANDARD;
                Ok(crate::proto::Unbundled { payload_type: PAYLOAD_CUSTODY.into(), payload: b64.encode(&p), signature: b64.encode(signing.sign(&to_sign(PAYLOAD_CUSTODY, &p))?) })
            };
            let transfer = |h: Value, r: Option<crate::proto::Unbundled>| {
                let (store, root, org, issued, pem) = (store.clone(), root.clone(), org.clone(), issued.clone(), pem.clone());
                async move { super::transfer(&store, &root, &org, "runner adacachy (instance 01a14794)", &issued, 1, &h.to_string(), r.as_ref(), &pem, at).await }
            };
            let handed = hand_on(&next, 1, &timeline)?;
            let handed_digest = digest_of(&handed);
            let got = transfer(handed.clone(), Some(receipt(&handed_digest, &timeline)?)).await?.map_err(|why| anyhow::anyhow!("{why}"))?;
            let v = chain(&store, &root, &org, &got, at).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
            assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
            let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
            let custodians: Vec<&str> = hops.iter().filter_map(|h| h.get("custodian").and_then(Value::as_str)).collect();
            assert_eq!(custodians, vec![issued.as_str(), left_uri.as_str()], "{v}");
            // Asked again (its answer lost): the same transfer, kept once.
            assert_eq!(transfer(handed.clone(), Some(receipt(&handed_digest, &timeline)?)).await?, Ok(got.clone()));
            assert_eq!(count(&store, "custody").await?, 2);
            // A hand-on to another epoch, or without its receipt: no certificate.
            let other = hand_on(&format!("kiln://{org}/person/ada/agent/01a1751c/epoch/2"), 2, &thing("session 01a1751c at epoch 0", 'f'))?;
            assert!(transfer(other, None).await?.is_err_and(|why| why.contains("not to")));
            let lone = hand_on(&next, 1, &thing("session 01a1751c at epoch 0", '7'))?;
            assert!(transfer(lone, None).await?.is_err_and(|why| why.contains("no receipt")));
            // A receipt of other than what was handed on: kept, its break recorded.
            let more = hand_on(&next, 1, &thing("session 01a1751c at epoch 0", '8'))?;
            let more_digest = digest_of(&more);
            assert!(transfer(more, Some(receipt(&more_digest, &thing("session 01a1751c at epoch 0", '9'))?)).await?.is_ok());
            assert_eq!(breaks(&store, LIST_MAX).await?.len(), 1);
            // The copy left behind, its epoch revoked as it moved on, hands it on again: refused,
            // recorded, never certified.
            crate::identities::record(&store, "agent", &left_uri, &left.pem).await?;
            crate::identities::revoke(&store, crate::identities::Target::Subject(left_uri.clone()), "epoch 0 left: moved on to epoch 1, on runner adacachy (instance 01a14794)", "kiln", at - 10).await?;
            let stale = hand_on(&next, 1, &thing("session 01a1751c at epoch 0", 'a'))?;
            let stale_digest = digest_of(&stale);
            assert!(transfer(stale, Some(receipt(&stale_digest, &thing("session 01a1751c at epoch 0", 'a'))?)).await?.is_err_and(|why| why.contains("refused") && why.contains("epoch 0 left")));
            let r = refusals(&store, LIST_MAX).await?;
            assert!(r.iter().any(|x| x.get("signer") == Some(&json!(left_uri))), "{r:?}");
            Ok(())
        })
    }

    /// W27-R08 (the Transfers plan's step 15): a walk from where a thing is used back to where it
    /// was made says each hop in words; exported, it carries every record it read as signed, kiln's
    /// CA certificate and its revocations, which check each record without kiln; and with one hop's
    /// record removed, or changed where kiln keeps it, it names the break at that hop.
    #[test]
    fn w27_r08_a_walk_says_each_hop_and_exports_what_it_read() -> anyhow::Result<()> {
        use base64::Engine as _;
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let (root, org) = (pki.ca_der.clone(), pki.organization());
            let at = crate::util::now_unix();
            let (server, fedora_uri) = (server_uri(&org), FEDORA.replace(ORG, &org));
            let (kiln, fedora) = (Signer::new(&pki, &server)?, Signer::new(&pki, &fedora_uri)?);
            // kiln hands commit 4c1e2a to fedora-2's runner, which makes image kiln:4c1e2a from it
            // and hands that to kiln.
            let (commit, image) = (thing("commit 4c1e2a", 'c'), thing("image kiln:4c1e2a", 'e'));
            let handed = kiln.sign(&record(Kind::HandedOn, &commit, Some(&fedora_uri), None, &[], "land", 1))?;
            let h = kiln_or(take(&store, &root, &org, &handed, "kiln", at).await?)?;
            let received = fedora.sign(&record(Kind::Received, &commit, None, Some(&server), &[&h.digest], "legs", 1))?;
            let r = kiln_or(take(&store, &root, &org, &received, "runner fedora-2", at).await?)?;
            let made = kiln_or(take(&store, &root, &org, &fedora.sign(&record(Kind::HandedOn, &image, Some(&server), None, &[&r.digest], "legs", 2))?, "runner fedora-2", at).await?)?;
            let got = kiln_or(take(&store, &root, &org, &kiln.sign(&record(Kind::Received, &image, None, Some(&fedora_uri), &[&made.digest], "land", 2))?, "kiln", at).await?)?;
            assert_eq!([&h.broke, &r.broke, &made.broke, &got.broke], [&None, &None, &None, &None]);
            // Walked from the image's digest: kiln's receipt of it, back to kiln's hand-on of the
            // commit it was made from.
            let image_digest = format!("sha256:{}", "e".repeat(64));
            let v = walk(&store, &pki, &image_digest, at, true).await?.ok_or_else(|| anyhow::anyhow!("no walk from {image_digest}"))?;
            assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
            let words: Vec<&str> = v.get("hops").and_then(Value::as_array).into_iter().flatten().filter_map(|h| h.get("words").and_then(Value::as_str)).collect();
            assert_eq!(
                words,
                [
                    "kiln received image kiln:4c1e2a from the runner on fedora-2",
                    "the runner on fedora-2 handed on image kiln:4c1e2a, made from what it received, to kiln",
                    "the runner on fedora-2 received commit 4c1e2a from kiln",
                    "kiln handed on commit 4c1e2a to the runner on fedora-2, where the chain begins",
                ]
            );
            // Exported: each record as signed, each checking against kiln's CA certificate alone.
            let ca = crate::pki::der_of(v.get("root").and_then(Value::as_str).unwrap_or_default())?;
            let records = v.get("records").and_then(Value::as_array).cloned().unwrap_or_default();
            assert_eq!(records.len(), 4, "{v}");
            for x in &records {
                let c = statement::verify(x.get("bundle").unwrap_or(&Value::Null), &ca, &[], at)?;
                assert_eq!(Some(c.digest.as_str()), x.get("digest").and_then(Value::as_str));
            }
            assert_eq!(v.get("revocations"), Some(&json!([])));
            let plain = walk(&store, &pki, &image_digest, at, false).await?.unwrap_or_default();
            assert_eq!((plain.get("records"), plain.get("root")), (None, None), "exported only when asked");
            let set = |envelope: Option<String>| {
                let (d, store) = (r.digest.clone(), store.clone());
                async move {
                    match envelope {
                        Some(e) => store.call(move |c| Ok(c.execute("UPDATE custody SET envelope = ?2 WHERE digest = ?1", params![d, e])?)).await,
                        None => store.call(move |c| Ok(c.execute("DELETE FROM custody WHERE digest = ?1", [&d])?)).await,
                    }
                }
            };
            // fedora-2's receipt of the commit removed: the walk breaks where the image's hand-on
            // follows it, naming it.
            set(None).await?;
            let v = walk(&store, &pki, &image_digest, at, false).await?.unwrap_or_default();
            assert_eq!((v.pointer("/break/kind"), v.pointer("/break/record")), (Some(&json!("no-record")), Some(&json!(made.digest))), "{v}");
            assert!(v.pointer("/break/detail").and_then(Value::as_str).is_some_and(|d| d.contains(&r.digest)), "{v}");
            kiln_or(take(&store, &root, &org, &received, "runner fedora-2", at).await?)?;
            assert_eq!(walk(&store, &pki, &image_digest, at, false).await?.unwrap_or_default().get("unbroken"), Some(&json!(true)), "kept again, it walks");
            // Its signed payload changed where kiln keeps it: the walk breaks at it, saying it no
            // longer checks, and the export carries it as kiln keeps it.
            let mut changed = received.clone();
            if let Some(p) = changed.pointer_mut("/dsseEnvelope/payload") {
                let was = base64::engine::general_purpose::STANDARD.decode(p.as_str().unwrap_or_default())?;
                *p = Value::String(base64::engine::general_purpose::STANDARD.encode(String::from_utf8_lossy(&was).replace("a test", "a lie")));
            }
            set(Some(changed.to_string())).await?;
            let v = walk(&store, &pki, &image_digest, at, true).await?.unwrap_or_default();
            assert_eq!((v.pointer("/break/kind"), v.pointer("/break/record")), (Some(&json!("unreadable")), Some(&json!(r.digest))), "{v}");
            assert!(v.pointer("/break/detail").and_then(Value::as_str).is_some_and(|d| d.contains("no longer checks")), "{v}");
            assert!(v.get("records").and_then(Value::as_array).is_some_and(|x| x.iter().any(|x| x.get("digest") == Some(&json!(r.digest)))), "{v}");
            // Another record's envelope kept in its place: a break at it, naming the other.
            set(Some(handed.to_string())).await?;
            let v = walk(&store, &pki, &image_digest, at, false).await?.unwrap_or_default();
            assert_eq!((v.pointer("/break/kind"), v.pointer("/break/record")), (Some(&json!("unreadable")), Some(&json!(r.digest))), "{v}");
            assert!(v.pointer("/break/detail").and_then(Value::as_str).is_some_and(|d| d.contains(&h.digest)), "{v}");
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): a session's work taken, its unanswered work, a holder's transfer and the newest
    /// breaks and refusals are each read off the index made for them, never by walking every take, hand-on, receipt,
    /// break or refusal. A record's taker is the agent session `Identity::parse` reads in the side's identity that
    /// takes its things (whom it hands them to; a receipt's signer), none in any other text. A session's unanswered
    /// work is its hand-ons, takes and passes of work, to it at any epoch, that no receipt follows (another record
    /// following one doesn't answer it), oldest first; and a holder's transfer is its own receipt of its session's
    /// timeline, never a receipt of work it signed since.
    #[test]
    fn dbq_custodys_reads_are_planned_on_their_indexes() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = Store::in_memory()?;
            let plan = |sql: &'static str, args: Vec<rusqlite::types::Value>| {
                let store = store.clone();
                async move { store.call(move |c| Ok(c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?.query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))?.collect::<Result<Vec<_>, _>>()?)).await }
            };
            let text = |s: &str| rusqlite::types::Value::Text(s.to_owned());
            let int = rusqlite::types::Value::Integer;
            let work = plan(WORK_TAKEN, vec![text("s1"), text(WORK)]).await?;
            assert!(work.iter().any(|p| p.contains("custody_taken_for_session")) && work.iter().any(|p| p.contains("custody_things_record")) && !work.iter().any(|p| p.contains("TEMP B-TREE")), "{work:?}");
            let open = plan(UNRECEIVED, vec![text("s1"), text(WORK)]).await?;
            assert!(open.iter().any(|p| p.contains("custody_takers_session")) && open.iter().any(|p| p.contains("custody_things_record")) && !open.iter().any(|p| p.contains("custody_kind_taken")), "{open:?}");
            let receipt = plan(TRANSFER_RECEIPT, vec![text("s1"), text("kiln://acme/person/ada/agent/s1/epoch/2/machine/m/instance/i"), text(TIMELINE)]).await?;
            assert!(receipt.iter().any(|p| p.contains("custody_takers_session")) && receipt.iter().any(|p| p.contains("custody_things_record")) && !receipt.iter().any(|p| p.contains("TEMP B-TREE")), "{receipt:?}");
            for (sql, index) in [(BREAKS, "custody_breaks_at"), (REFUSALS, "transfer_refusals_at")] {
                let newest = plan(sql, vec![int(500)]).await?;
                assert!(newest.iter().any(|p| p.contains(index)) && !newest.iter().any(|p| p.contains("TEMP B-TREE")), "{newest:?}");
            }
            // A record's taker, as Identity::parse reads the side that takes its things.
            let agent = |s: &str, e: u64| format!("kiln://acme/person/ada/agent/{s}/epoch/{e}");
            let held = |s: &str, e: u64| format!("{}/machine/m/instance/i", agent(s, e));
            let named = [
                agent("s1", 0),
                held("01a1ae8f", 12),
                "kiln://agent/person/epoch/agent/s1/epoch/1".to_owned(),
                "kiln://acme/person/agent/agent/epoch/epoch/0".to_owned(),
                "kiln://a.b/person/c_d/agent/e-f/epoch/9/machine/g.h/instance/i_j".to_owned(),
                agent("s1", 1).replace("/agent/", "/AGENT/"),
                agent("s1", 1).replace("/epoch/1", "/epoch/01"),
                agent("s1", 1).replace("/ada/", "//"),
                agent("..", 1),
                agent(&"s".repeat(129), 1),
                agent("s 1", 1),
                format!("{}/machine/m", agent("s1", 1)),
                format!("{}/more", held("s1", 1)),
                format!("the agent at {}", agent("s1", 1)),
                format!("{} and {}", agent("s1", 1), agent("s2", 0)),
                "kiln://acme/person/ada/machine/m/instance/i".to_owned(),
                "kiln://acme/person/ada".to_owned(),
                "kiln://acme/server".to_owned(),
                "docker://kiln@sha256:00".to_owned(),
                String::new(),
            ];
            let takers: Vec<(Option<String>, Option<String>)> = store
                .call(move |c| {
                    let mut out = Vec::new();
                    for (i, uri) in named.iter().enumerate() {
                        let (given, got) = (format!("g{i}"), format!("t{i}"));
                        c.execute("INSERT INTO custody (digest, payload_type, kind, custodian, counterpart, envelope, taken_at) VALUES (?1, 'x', 'handed-on', 'kiln://acme/server', ?2, ?1, 1)", params![given, uri])?;
                        c.execute("INSERT INTO custody (digest, payload_type, kind, custodian, counterpart, envelope, taken_at) VALUES (?1, 'x', 'received', ?2, 'kiln://acme/server', ?1, 1)", params![got, uri])?;
                        let read = |d: &str| c.query_row("SELECT session FROM custody_takers WHERE record = ?1", [d], |r| r.get::<_, String>(0)).optional();
                        let parsed = Identity::parse(uri).ok().and_then(|i| match i.actor {
                            Actor::Agent { agent, .. } => Some(agent),
                            _ => None,
                        });
                        anyhow::ensure!(read(&given)? == parsed && read(&got)? == parsed, "{uri:?}: the taker is {:?} given to it, {:?} received by it; Identity::parse reads {parsed:?}", read(&given)?, read(&got)?);
                        out.push((parsed, read(&given)?));
                    }
                    c.execute_batch("DELETE FROM custody")?;
                    Ok(out)
                })
                .await?;
            assert_eq!(takers.iter().filter(|(p, _)| p.is_some()).count(), 5, "{takers:?}");
            // Session s1's hand-ons, takes and passes of work, and what isn't: answered by a receipt, about other than
            // work, to another session (S1 isn't s1), a receipt itself, to no identity.
            let records = [
                ("h1", "handed-on", Some(agent("s1", 0)), WORK, 10),
                ("h2", "taken", Some(agent("s1", 1)), WORK, 11),
                ("h3", "passed-on", Some(agent("s1", 1)), WORK, 12),
                ("h4", "handed-on", Some(agent("s1", 2)), WORK, 8),
                ("h5", "handed-on", Some(agent("s1", 2)), TIMELINE, 8),
                ("h6", "handed-on", Some(agent("s2", 0)), WORK, 8),
                ("h7", "handed-on", Some(agent("S1", 3)), WORK, 9),
                ("h8", "received", Some(agent("s1", 3)), WORK, 8),
                ("a1", "handed-on", Some("kiln://agent/person/epoch/agent/s1/epoch/1".to_owned()), WORK, 20),
                ("a2", "handed-on", Some("kiln://acme/person/agent/agent/epoch/epoch/0".to_owned()), WORK, 21),
                ("a3", "handed-on", Some(format!("the agent at {}", agent("s1", 1))), WORK, 22),
                ("a4", "taken", Some(held("s1", 4)), WORK, 14),
                ("a5", "handed-on", Some(agent("s1", 3).replace("/agent/", "/AGENT/")), WORK, 23),
                ("a6", "handed-on", None, WORK, 24),
                ("x1", "handed-on", Some(agent("s10", 0)), WORK, 15),
            ];
            store
                .call(move |c| {
                    for (digest, kind, counterpart, algorithm, at) in records {
                        c.execute("INSERT INTO custody (digest, payload_type, kind, custodian, counterpart, envelope, taken_at) VALUES (?1, ?2, ?3, 'kiln://acme/server', ?4, ?1, ?5)", params![digest, PAYLOAD_CUSTODY, kind, counterpart, at])?;
                        c.execute("INSERT INTO custody_things (record, name, algorithm, hex) VALUES (?1, 'plan run r1 step 1', ?2, ?1)", params![digest, algorithm])?;
                    }
                    // Receipts: holder 2's of its session's timeline (r6), then of work (r4 answers h4; r5), newer;
                    // holder 3's of the timeline; kiln's own.
                    let holder = |e: u64| format!("kiln://acme/person/ada/agent/s1/epoch/{e}/machine/m/instance/i");
                    for (digest, custodian, algorithm, at) in [("r4", holder(2), WORK, 13), ("r5", holder(2), WORK, 15), ("r6", holder(2), TIMELINE, 9), ("r7", holder(3), TIMELINE, 16), ("r8", "kiln://acme/server".to_owned(), TIMELINE, 17)] {
                        c.execute("INSERT INTO custody (digest, payload_type, kind, custodian, envelope, taken_at) VALUES (?1, 'x', 'received', ?2, ?1, ?3)", params![digest, custodian, at])?;
                        c.execute("INSERT INTO custody_things (record, name, algorithm, hex) VALUES (?1, 'session s1 at epoch 1', ?2, ?1)", params![digest, algorithm])?;
                    }
                    c.execute_batch(
                        "INSERT INTO custody (digest, payload_type, kind, custodian, envelope, taken_at) VALUES ('n3', 'x', 'handed-on', 'kiln://acme/server', 'n3', 13);
                         INSERT INTO custody_follows (record, follows) VALUES ('r4', 'h4'), ('n3', 'h3');",
                    )?;
                    Ok(())
                })
                .await?;
            assert_eq!(unreceived(&store, "s1").await?, vec!["h1", "h2", "h3", "a4", "a1"]);
            assert_eq!(unreceived(&store, "S1").await?, vec!["h7"], "a session is its id, its letters' case and all");
            assert_eq!(unreceived(&store, "s2").await?, vec!["h6"]);
            assert_eq!(unreceived(&store, "s10").await?, vec!["x1"], "s10's isn't s1's, nor s1's s10's");
            assert_eq!(unreceived(&store, "epoch").await?, vec!["a2"], "a person called agent hands to a session called epoch");
            for nobody in ["s3", "s", "person", "%", "_", ""] {
                assert!(unreceived(&store, nobody).await?.is_empty(), "{nobody:?}");
            }
            let holder = |e: u64| format!("kiln://acme/person/ada/agent/s1/epoch/{e}/machine/m/instance/i");
            assert_eq!(transfer_receipt(&store, &holder(2)).await?.as_deref(), Some("r6"), "its receipt of the timeline, not of the work it took up since");
            assert_eq!(transfer_receipt(&store, &holder(3)).await?.as_deref(), Some("r7"), "its own, not its session's other holder's");
            assert_eq!(transfer_receipt(&store, "kiln://acme/person/ada/agent/s9/epoch/0").await?, None);
            assert_eq!(transfer_receipt(&store, "kiln://acme/server").await?, None, "kiln's server is no agent session's holder");
            // Each is prepared once, whatever is bound next: none binds a value SQLite plans by.
            let prepared_again = |sql: &'static str, binds: Vec<Vec<rusqlite::types::Value>>| {
                let store = store.clone();
                async move {
                    store
                        .call(move |c| {
                            let mut q = c.prepare(sql)?;
                            for args in binds {
                                let mut rows = q.query(rusqlite::params_from_iter(args))?;
                                while rows.next()?.is_some() {}
                            }
                            Ok(q.get_status(rusqlite::StatementStatus::RePrepare))
                        })
                        .await
                }
            };
            assert_eq!(prepared_again(UNRECEIVED, vec![vec![text("s1"), text(WORK)], vec![text("s2"), text(WORK)]]).await?, 0, "unreceived");
            assert_eq!(prepared_again(TRANSFER_RECEIPT, vec![vec![text("s1"), text(&holder(2)), text(TIMELINE)], vec![text("s2"), text(&holder(3)), text(TIMELINE)]]).await?, 0, "transfer_receipt");
            assert_eq!(prepared_again(WORK_TAKEN, vec![vec![text("s1"), text(WORK)], vec![text("s2"), text(WORK)]]).await?, 0, "work_taken_from");
            assert_eq!(prepared_again(BREAKS, vec![vec![int(500)], vec![int(10)]]).await?, 0, "breaks");
            assert_eq!(prepared_again(REFUSALS, vec![vec![int(500)], vec![int(10)]]).await?, 0, "refusals");
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): kiln's take of a session's work is found however many takes of work came after
    /// it (here the 300th newest, past the 256 read before), by the session it's taken for as `Identity::parse` reads
    /// its giver (a1's isn't a10's), the newest of a session's takes; never a take of the session's timeline, which
    /// isn't work. With it, the work's name and whom it went to.
    #[test]
    fn dbq_a_sessions_work_taken_is_found_however_old() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            crate::identities::own_identities(&hub.store, &hub.pki).await?;
            let (org, at) = (hub.pki.organization(), crate::util::now_unix());
            let agent = |id: &str, epoch: u64| format!("kiln://{org}/person/ada/agent/{id}/epoch/{epoch}");
            let held = |id: &str| format!("{}/machine/adacachy/instance/01a14794", agent(id, 0));
            // kiln's take of `thing` for `from`, handed to `to`, kept at `when`: its digest.
            let taken = |thing: Subject, from: String, to: String, when: i64| {
                let (hub, org) = (hub.clone(), org.clone());
                async move {
                    let body = Body { to: Some(to), taken_for: Some(from), why: "tried again".into(), ..Body::new(Kind::Taken, &server_uri(&org)) };
                    let bundle = server_signed(&hub.pki, &Statement { subject: vec![thing], ..Statement::new(to_value(&body)) })?;
                    anyhow::Ok(kiln_or(take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", when).await?)?.digest)
                }
            };
            let step = |n: i64| work(&format!("plan run r1 step {n}"), &["r1", &n.to_string()]);
            let first = taken(step(0), held("a1"), agent("a2", 0), at).await?;
            let mut b3 = String::new();
            for n in 1..299 {
                let d = taken(step(n), held(&format!("b{}", n % 7)), agent("b9", 0), at + n).await?;
                if n % 7 == 3 {
                    b3 = d;
                }
            }
            let other = taken(step(299), held("a10"), agent("a11", 0), at + 299).await?;
            taken(timeline("a1", 0, &[]), held("a1"), agent("a1", 1), at + 400).await?;
            let newer: i64 = hub
                .store
                .call(move |c| Ok(c.query_row("SELECT count(*) FROM custody c WHERE c.kind = 'taken' AND c.taken_at > ?1 AND EXISTS (SELECT 1 FROM custody_things t WHERE t.record = c.digest AND t.algorithm = ?2)", params![at, WORK], |r| r.get(0))?))
                .await?;
            assert_eq!(newer, 299, "a1's take of work is the 300th newest");
            assert_eq!(work_taken_from(&hub.store, "a1").await?, Some((first, "plan run r1 step 0".to_owned(), agent("a2", 0))));
            assert_eq!(work_taken_from(&hub.store, "a10").await?, Some((other, "plan run r1 step 299".to_owned(), agent("a11", 0))));
            assert_eq!(work_taken_from(&hub.store, "b3").await?, Some((b3, "plan run r1 step 297".to_owned(), agent("b9", 0))), "its newest");
            for none in ["a2", "a", "b9", "%"] {
                assert_eq!(work_taken_from(&hub.store, none).await?, None, "{none:?}: no work was taken for it");
            }
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): a thing's head is found by its name, the letters' case aside (ASCII's), or by a
    /// name ending in `*` among every name it begins, sought on `custody_things_name` where a pattern was matched. A
    /// name is itself (`%`, `_` and `\` are no wildcards), and a prefix's range ends where the names it begins do: `x@`
    /// begins `x@y`, not `x[y`, which NOCASE sorts just after it.
    #[test]
    fn dbq_a_things_head_is_sought_by_its_name() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = Store::in_memory()?;
            let named = [
                ("n1", "session 01a1 at epoch 3", 10),
                ("n2", "session 01a1 at epoch 4", 20),
                ("n3", "Session 01A1 at epoch 5", 15),
                ("n4", "x@y", 30),
                ("n5", "x[y", 40),
                ("n6", "50% of a_b\\c", 50),
                ("n7", "505 of axb", 60),
                ("n8", "session 01a2 at epoch 0", 70),
            ];
            store
                .call(move |c| {
                    for (digest, name, at) in named {
                        c.execute("INSERT INTO custody (digest, payload_type, kind, custodian, envelope, taken_at) VALUES (?1, 'x', 'handed-on', 'kiln://acme/server', ?1, ?2)", params![digest, at])?;
                        c.execute("INSERT INTO custody_things (record, name, algorithm, hex) VALUES (?1, ?2, 'sha256', ?1)", params![digest, name])?;
                    }
                    Ok(())
                })
                .await?;
            for (name, head) in [
                ("session 01a1 at epoch 4", Some("n2")),
                ("SESSION 01A1 AT EPOCH 3", Some("n1")),
                ("session 01a1 at epoch *", Some("n2")),
                ("session 01a1 at *", Some("n2")),
                ("x@*", Some("n4")),
                ("x[*", Some("n5")),
                ("50% of a_b\\c", Some("n6")),
                ("50% of a_b*", Some("n6")),
                ("505 of a_b", None),
                ("*", Some("n8")),
                ("session 01a3*", None),
                ("nothing", None),
            ] {
                assert_eq!(head_named(&store, name).await?.as_deref(), head, "{name:?}");
            }
            assert_eq!(after_prefix("x@").as_deref(), Some("x["));
            assert_eq!(after_prefix("aB").as_deref(), Some("ac"), "folded, then the next");
            assert_eq!(after_prefix("a\u{10FFFF}").as_deref(), Some("b"), "carried past a last character with none after it");
            assert_eq!(after_prefix("\u{D7FF}").as_deref(), Some("\u{E000}"), "past the surrogates");
            assert_eq!((after_prefix(""), after_prefix("\u{10FFFF}")), (None, None));
            let plans = store
                .call(|c| {
                    let plan = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<Vec<String>> { c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?.query_map(args, |r| r.get(3))?.collect() };
                    let again = |sql: &str, binds: [[&dyn rusqlite::ToSql; 2]; 2]| -> rusqlite::Result<i32> {
                        let mut q = c.prepare(sql)?;
                        for args in binds {
                            let n = q.parameter_count();
                            let mut rows = q.query(args.get(..n).unwrap_or(&args))?;
                            while rows.next()?.is_some() {}
                        }
                        Ok(q.get_status(rusqlite::StatementStatus::RePrepare))
                    };
                    Ok((plan(HEAD_NAMED, &[&"a"])?, plan(HEAD_BEGUN, &[&"a", &"b"])?, again(HEAD_NAMED, [[&"a", &""], [&"b", &""]])?, again(HEAD_BEGUN, [[&"a", &"b"], [&"x@", &"x["]])?))
                })
                .await?;
            for plan in [&plans.0, &plans.1] {
                assert!(plan.iter().any(|p| p.contains("custody_things_name")) && !plan.iter().any(|p| p.contains("SCAN t")), "{plan:?}");
            }
            assert_eq!((plans.2, plans.3), (0, 0), "prepared once, whatever name is bound next");
            Ok(())
        })
    }
}
