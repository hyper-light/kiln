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
fn digest_of(bundle: &Value) -> String {
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
struct Refusal {
    digest: String,
    act: String,
    signer: String,
    why: String,
    envelope: String,
    came_from: String,
    at: i64,
}

/// Takes the signed custody record or delegation `bundle`, which came over `came_from` (a runner
/// process, an API caller), at `at`: checked as of then against kiln's root `root` and the
/// revocations kiln has; kept by its digest; its own hop checked, and the hops of records that
/// waited for it. Refused, and recorded, when kiln can't take it. One kiln has already (sent again
/// after a reconnect, or carried by another) is answered as such, as it was judged when taken: its
/// signer may have moved on since.
pub async fn take(store: &Store, root: &[u8], org: &str, bundle: &Value, came_from: &str, at: i64) -> anyhow::Result<Result<Taken, Refused>> {
    let digest = digest_of(bundle);
    if !digest.is_empty() && kept(store, &digest).await? {
        return Ok(Ok(Taken { digest, again: true, broke: None }));
    }
    let revocations = crate::identities::revocations(store).await?;
    let refusal = |act: &str, signer: String, why: String| Refusal { digest: digest_of(bundle), act: act.to_owned(), signer, why, envelope: bundle.to_string(), came_from: came_from.to_owned(), at };
    let c = match statement::verify(bundle, root, &revocations, at) {
        Ok(c) => c,
        Err(e) => {
            // Who signed it, as its certificate says, revocations aside: a copy left behind signs
            // as the epoch it left.
            let signer = statement::verify(bundle, root, &[], at).map(|c| c.custodian).unwrap_or_default();
            let why = e.to_string();
            refuse(store, refusal("a signed statement", signer, why.clone())).await?;
            return Ok(Err(Refused::Unsigned(why)));
        }
    };
    let (kind, counterpart) = match c.payload_type.as_str() {
        PAYLOAD_CUSTODY => match custody::body(&c) {
            Ok(b) => (Some(b.kind), if b.kind == Kind::Received { b.from } else { b.to }),
            Err(e) => {
                refuse(store, refusal("a custody record", c.custodian.clone(), e.to_string())).await?;
                return Ok(Err(Refused::NotCustody(e.to_string())));
            }
        },
        PAYLOAD_DELEGATION => match custody::grant(&c) {
            Ok(g) => (None, Some(g.delegate)),
            Err(why) => {
                refuse(store, refusal("a delegation", c.custodian.clone(), why.clone())).await?;
                return Ok(Err(Refused::NotCustody(why)));
            }
        },
        other => {
            let why = format!("it's a {other} statement, not a custody record or a delegation");
            refuse(store, refusal("a signed statement", c.custodian.clone(), why.clone())).await?;
            return Ok(Err(Refused::NotCustody(why)));
        }
    };
    let again = !keep(store, &c, kind, counterpart, bundle, came_from).await?;
    let mut broke = None;
    if kind.is_some() && !again {
        broke = check(store, root, org, &c.digest, &revocations, at).await?;
        for waiting in followers(store, &c.digest).await? {
            check(store, root, org, &waiting, &revocations, at).await?;
        }
    }
    Ok(Ok(Taken { digest: c.digest, again, broke }))
}

/// A custody record a runner process sent ([`crate::proto::AgentMsg::Custody`]): taken as having
/// come over that process's connection, and logged either way.
pub async fn on_runner(hub: &crate::hub::Hub, peer: crate::instances::PeerId, machine: &str, json: &str) {
    let instance = hub.instances.lock().get(peer).map(|i| i.label()).unwrap_or_else(|| "legacy".to_owned());
    let Ok(bundle) = serde_json::from_str::<Value>(json) else {
        return tracing::debug!(%machine, "an unreadable custody record");
    };
    let came_from = format!("runner {machine} (instance {instance})");
    match take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), &bundle, &came_from, crate::util::now_unix()).await {
        Ok(Ok(t)) => tracing::info!(target: "kiln.custody", %machine, %instance, digest = %t.digest, again = t.again, broke = t.broke.as_deref().unwrap_or(""), "took a custody record"),
        Ok(Err(why)) => tracing::info!(target: "kiln.custody", %machine, %instance, %why, "refused a custody record"),
        Err(e) => tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "couldn't keep a custody record"),
    }
}

/// `s` signed by kiln's server with its own signing key (`<data>/pki/server-signing.key`, which
/// `identities::own_identities` keeps certified), its body naming it: its bundle. Only kiln's
/// server signs takes.
pub fn server_signed(pki: &crate::pki::Pki, s: &statement::Statement) -> anyhow::Result<Value> {
    use anyhow::Context as _;
    use rcgen::SigningKey as _;
    let read = |f: &str| std::fs::read_to_string(pki.dir().join(f)).with_context(|| format!("kiln's server's {f}"));
    let key = rcgen::KeyPair::from_pem(&read("server-signing.key")?)?;
    let cert = read("server-signing.pem")?;
    let mut s = s.clone();
    if let Some(o) = s.body.as_object_mut() {
        o.insert("by".into(), Value::String(server_uri(&pki.organization())));
    }
    let payload = statement::payload(&s)?;
    let signature = key.sign(&statement::to_sign(PAYLOAD_CUSTODY, &payload))?;
    Ok(statement::bundle(PAYLOAD_CUSTODY, &payload, &signature, &crate::pki::der_of(&cert)?))
}

/// kiln's take of agent session `session` as it leaves epoch `epoch` (its timeline `events`), for
/// the process that held it there, which can't hand it on (`why`: its machine is away, and since
/// when): to the session at the next epoch, whichever process takes it, following that process's
/// receipt of it if kiln has one. Kept here at once, and carried in the session's package: its
/// receipt follows it as a hand-on's does (docs/ci.md W27-R08). None if kiln never certified the
/// session at that epoch: it goes on unsigned, as before.
pub async fn take_for(store: &Store, pki: &crate::pki::Pki, session: &str, epoch: u64, events: &[crate::harness::model::Event], why: &str) -> anyhow::Result<Option<Value>> {
    let Some(holder) = crate::identities::holder_at(store, session, epoch).await? else { return Ok(None) };
    let org = pki.organization();
    let to = at_epoch(&holder, epoch.saturating_add(1)).ok_or_else(|| anyhow::anyhow!("{holder} isn't an agent session's identity"))?;
    let body = custody::Body { taken_for: Some(holder.clone()), to: Some(to), why: why.to_owned(), epoch: Some(epoch.saturating_add(1)), ..custody::Body::new(Kind::Taken, &server_uri(&org)) };
    let follows = receipt_of(store, &holder).await?.into_iter().collect();
    let s = statement::Statement { subject: vec![timeline(session, epoch, events)], follows, ..statement::Statement::new(custody::to_value(&body)) };
    let bundle = server_signed(pki, &s)?;
    match take(store, &pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
        Ok(_) => Ok(Some(bundle)),
        Err(why) => anyhow::bail!("kiln refused its own take: {why}"),
    }
}

/// The newest receipt custodian `custodian` signed: how it came to hold what it holds.
async fn receipt_of(store: &Store, custodian: &str) -> anyhow::Result<Option<String>> {
    let c = custodian.to_owned();
    store.call(move |conn| Ok(conn.query_row("SELECT digest FROM custody WHERE custodian = ?1 AND kind = 'received' ORDER BY taken_at DESC, rowid DESC LIMIT 1", [&c], |r| r.get(0)).optional()?)).await
}

/// Whether kiln keeps record `digest`.
async fn kept(store: &Store, digest: &str) -> anyhow::Result<bool> {
    let d = digest.to_owned();
    store.call(move |c| Ok(c.query_row("SELECT 1 FROM custody WHERE digest = ?1", [&d], |_| Ok(())).optional()?.is_some())).await
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
    // Kept, so checked when kiln took it: read again, revocations aside.
    let b = statement::verify(&given, root, &[], at).map_err(|e| anyhow::anyhow!("{e}")).and_then(|c| custody::body(&c).map_err(|e| anyhow::anyhow!("{e}")))?;
    if !b.kind.gives() || b.epoch != Some(epoch) || !b.to.as_deref().is_some_and(|to| custody::names(to, issued)) {
        let to = b.to.as_deref().unwrap_or("no one");
        return Ok(Err(format!("its hand-on ({}) is to {to} at epoch {}, not to {issued}", kind_name(b.kind), b.epoch.map_or_else(|| "none".to_owned(), |e| e.to_string()))));
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
    let follows = statement::verify(&bundle, root, &[], at).map(|c| c.statement.follows).unwrap_or_default();
    if !follows.contains(&handed) {
        return Ok(Err(format!("its receipt doesn't follow its hand-on {handed}")));
    }
    Ok(Ok(got.digest))
}

/// Keeps `c`: whether it's new (false: kiln had it).
async fn keep(store: &Store, c: &Checked, kind: Option<Kind>, counterpart: Option<String>, bundle: &Value, came_from: &str) -> anyhow::Result<bool> {
    let (digest, payload_type, kind, custodian) = (c.digest.clone(), c.payload_type.clone(), kind.map(kind_name), c.custodian.clone());
    let (stream, seq, under, envelope, came_from, taken_at) = (c.statement.stream.clone(), c.statement.seq.and_then(|s| i64::try_from(s).ok()), c.statement.under.clone(), bundle.to_string(), came_from.to_owned(), c.at);
    let follows = c.statement.follows.clone();
    let things: Vec<(String, String, String)> = c.statement.subject.iter().flat_map(|s| s.digest.iter().map(|(alg, hex)| (s.name.clone(), alg.clone(), hex.clone()))).collect();
    store
        .call(move |conn| {
            let tx = crate::store::tx(conn)?;
            let n = tx.execute(
                "INSERT OR IGNORE INTO custody (digest, payload_type, kind, custodian, counterpart, stream, seq, under, envelope, came_from, taken_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![digest, payload_type, kind, custodian, counterpart, stream, seq, under, envelope, came_from, taken_at],
            )?;
            if n == 0 {
                return Ok(false);
            }
            for f in &follows {
                tx.execute("INSERT OR IGNORE INTO custody_follows (record, follows) VALUES (?1, ?2)", params![digest, f])?;
            }
            for (name, alg, hex) in &things {
                tx.execute("INSERT INTO custody_things (record, name, algorithm, hex) VALUES (?1, ?2, ?3, ?4)", params![digest, name, alg, hex])?;
            }
            tx.commit()?;
            Ok(true)
        })
        .await
}

/// The records that follow record `digest`.
async fn followers(store: &Store, digest: &str) -> anyhow::Result<Vec<String>> {
    let d = digest.to_owned();
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT record FROM custody_follows WHERE follows = ?1")?;
            let rows = q.query_map([d], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
}

/// The records kiln has among `digests`, each checked as of when kiln took it (a revocation
/// since doesn't undo it).
async fn load(store: &Store, root: &[u8], digests: &[String], revocations: &[Revocation]) -> anyhow::Result<BTreeMap<String, Checked>> {
    let wanted = digests.to_vec();
    let rows: Vec<(String, String, i64)> = store
        .call(move |c| {
            let mut q = c.prepare("SELECT digest, envelope, taken_at FROM custody WHERE digest = ?1")?;
            let mut out = Vec::new();
            for d in &wanted {
                if let Some(row) = q.query_row([d], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()? {
                    out.push(row);
                }
            }
            Ok(out)
        })
        .await?;
    let mut map = BTreeMap::new();
    for (d, envelope, at) in rows {
        let Ok(bundle) = serde_json::from_str::<Value>(&envelope) else { continue };
        // Only what was revoked before kiln took it: kiln checked it then, and a revocation since
        // (one in the same second included) doesn't undo it.
        let before: Vec<Revocation> = revocations.iter().filter(|r| r.at < at).cloned().collect();
        match statement::verify(&bundle, root, &before, at) {
            Ok(c) => {
                map.insert(d, c);
            }
            Err(e) => tracing::warn!(record = %d, error = %e, "a custody record kiln kept no longer checks as of when it took it"),
        }
    }
    Ok(map)
}

/// Checks record `digest`'s own hop, at `at`: a break, recorded, if it doesn't hold; none if it
/// holds or waits for a record it follows that kiln doesn't have yet.
async fn check(store: &Store, root: &[u8], org: &str, digest: &str, revocations: &[Revocation], at: i64) -> anyhow::Result<Option<String>> {
    let mine = load(store, root, &[digest.to_owned()], revocations).await?;
    let Some(r) = mine.get(digest) else { return Ok(None) };
    let follows = r.statement.follows.clone();
    let mut map = load(store, root, &follows, revocations).await?;
    if follows.iter().any(|f| !map.contains_key(f)) {
        return Ok(None);
    }
    map.extend(mine);
    match custody::hop(digest, &map, &server_uri(org)) {
        Ok(_) => Ok(None),
        Err(b) => {
            let mut around = follows;
            around.insert(0, digest.to_owned());
            record_break(store, digest, &b, &around, at).await?;
            Ok(Some(b.to_string()))
        }
    }
}

/// Records break `b` at record `record`, with the records around it, once.
async fn record_break(store: &Store, record: &str, b: &Break, around: &[String], at: i64) -> anyhow::Result<()> {
    let (r, kind, detail, around_json) = (record.to_owned(), b.code().to_owned(), b.to_string(), serde_json::to_string(around)?);
    let (rr, dd) = (r.clone(), detail.clone());
    let new = store
        .call(move |c| {
            let seen: Option<i64> = c.query_row("SELECT id FROM custody_breaks WHERE record = ?1 AND kind = ?2 AND detail = ?3", params![r, kind, detail], |x| x.get(0)).optional()?;
            if seen.is_some() {
                return Ok(false);
            }
            c.execute("INSERT INTO custody_breaks (record, kind, detail, around, at) VALUES (?1, ?2, ?3, ?4, ?5)", params![r, kind, detail, around_json, at])?;
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
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO transfer_refusals (digest, act, signer, why, envelope, came_from, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![r.digest, r.act, r.signer, r.why, r.envelope, r.came_from, r.at],
            )?;
            Ok(())
        })
        .await?;
    crate::audit::write(store, entry).await;
    Ok(())
}

/// Records that kiln refused act `act` (a statement's digest `digest`, signed by `signer`) for
/// `why`: what the fence and the act checks call (W27-R08).
pub async fn refused(store: &Store, digest: &str, act: &str, signer: &str, why: &str, came_from: &str, at: i64) -> anyhow::Result<()> {
    refuse(store, Refusal { digest: digest.to_owned(), act: act.to_owned(), signer: signer.to_owned(), why: why.to_owned(), envelope: String::new(), came_from: came_from.to_owned(), at }).await
}

/// The record a walk starts from: `digest` if it's a record kiln has; else, as a thing's digest
/// (`sha256:…`, `gitCommit:…`), the head of that thing's chain: a record about it that no other
/// record about it follows (records arrive in any order: a receipt may come before its hand-on),
/// the newest such when its chain forks.
async fn start(store: &Store, digest: &str) -> anyhow::Result<Option<String>> {
    let d = digest.to_owned();
    store
        .call(move |c| {
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

/// The head of the chain of the newest thing named `name` ("session 01a1751c at epoch 3"; a name
/// ending in `*` matches every name it begins: "session 01a1751c at epoch *"): the record a walk of
/// it starts from, none if kiln has no record of it.
pub async fn head_named(store: &Store, name: &str) -> anyhow::Result<Option<String>> {
    let escape = |s: &str| s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let pattern = name.strip_suffix('*').map_or_else(|| escape(name), |p| format!("{}%", escape(p)));
    store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT t.record FROM custody_things t JOIN custody c ON c.digest = t.record
                 WHERE t.name LIKE ?1 ESCAPE '\\' AND c.kind IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM custody_follows f WHERE f.follows = t.record)
                 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                [pattern],
                |x| x.get(0),
            )
            .optional()?)
        })
        .await
}

/// The digests of every record kiln has on custodian `custodian`'s stream `stream`.
async fn on_stream(store: &Store, custodian: &str, stream: &str) -> anyhow::Result<Vec<String>> {
    let (who, s) = (custodian.to_owned(), stream.to_owned());
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT digest FROM custody WHERE custodian = ?1 AND stream = ?2 ORDER BY seq LIMIT ?3")?;
            let rows = q.query_map(params![who, s, i64::try_from(WALK_MAX).unwrap_or(i64::MAX)], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
}

/// A thing's chain of custody, walked now from `digest` (a record's, or a thing's): each hop with
/// both sides and who signed it, or the first break, recorded. None when kiln has no record of it.
pub async fn chain(store: &Store, root: &[u8], org: &str, digest: &str, at: i64) -> anyhow::Result<Option<Value>> {
    let Some(from) = start(store, digest).await? else { return Ok(None) };
    let revocations = crate::identities::revocations(store).await?;
    let mut map: BTreeMap<String, Checked> = BTreeMap::new();
    let mut frontier = vec![from.clone()];
    while !frontier.is_empty() && map.len() < WALK_MAX {
        let got = load(store, root, &frontier, &revocations).await?;
        let mut next: BTreeSet<String> = BTreeSet::new();
        for c in got.values() {
            next.extend(c.statement.follows.iter().filter(|f| !map.contains_key(*f) && !got.contains_key(*f)).cloned());
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
            "signer": c.map(|c| c.signer.to_string()),
            "to": b.as_ref().and_then(|b| b.to.clone()),
            "from": b.as_ref().and_then(|b| b.from.clone()),
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
            held_by = Some(c.custodian.clone());
        } else if followers(store, &from).await?.is_empty() {
            open = true;
            lapsed = at - c.at > LAPSE_AFTER;
            held_by = Some(if b.kind == Kind::Taken { b.taken_for.clone().unwrap_or_default() } else { c.custodian.clone() });
        }
    }
    Ok(Some(match custody::walk(&from, &map, &server_uri(org)) {
        Ok(w) => json!({ "from": from, "unbroken": true, "open": open, "lapsed": lapsed, "held_by": held_by, "hops": w.hops.iter().map(view).collect::<Vec<_>>(), "began": w.began }),
        Err(b) => {
            let at_record = break_record(&b).unwrap_or(&from).to_owned();
            record_break(store, &at_record, &b, std::slice::from_ref(&from), at).await?;
            json!({ "from": from, "unbroken": false, "break": { "kind": b.code(), "record": at_record, "detail": b.to_string() } })
        }
    }))
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
        | Break::TakenNotByServer { record, .. } => record,
        Break::Gap(_) => return None,
    })
}

/// The breaks kiln found, newest first.
pub async fn breaks(store: &Store, limit: usize) -> anyhow::Result<Vec<Value>> {
    let n = i64::try_from(limit).unwrap_or(i64::MAX);
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT record, kind, detail, around, refused, at FROM custody_breaks ORDER BY at DESC, id DESC LIMIT ?1")?;
            let rows = q
                .query_map([n], |r| {
                    let around: String = r.get(3)?;
                    Ok(json!({ "record": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "detail": r.get::<_, String>(2)?, "around": serde_json::from_str::<Value>(&around).unwrap_or(Value::Null), "refused": r.get::<_, String>(4)?, "at": r.get::<_, i64>(5)? }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// What kiln refused, newest first.
pub async fn refusals(store: &Store, limit: usize) -> anyhow::Result<Vec<Value>> {
    let n = i64::try_from(limit).unwrap_or(i64::MAX);
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT digest, act, signer, why, came_from, at FROM transfer_refusals ORDER BY at DESC, id DESC LIMIT ?1")?;
            let rows = q
                .query_map([n], |r| {
                    Ok(json!({ "digest": r.get::<_, String>(0)?, "act": r.get::<_, String>(1)?, "signer": r.get::<_, String>(2)?, "why": r.get::<_, String>(3)?, "came_from": r.get::<_, String>(4)?, "at": r.get::<_, i64>(5)? }))
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
}

/// `kiln transfers`: what kiln refused because its signer no longer held what it acted on.
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
}
