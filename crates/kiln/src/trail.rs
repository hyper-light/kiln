//! The trail (docs/ci.md W27-R09): every signed statement kiln takes goes into one append-only
//! log, in the transaction that takes it, so the trail's order is the order kiln took things; and
//! kiln's server signs checkpoints of it as `[ci.trail]` says. What holds, and what kiln signs, is
//! `kiln_ci::identity::trail`'s.
//!
//! - **Taken** ([`take`]): a signed statement as kiln keeps it in its own table: a leg's records
//!   and the server's delegations of legs, custody records, W18's attestations.
//! - **kiln's own** ([`own`]): what kiln checks that isn't a statement itself (a pushed commit's
//!   signature, what a host verified, a revocation) goes in as kiln's statement of it, unsigned:
//!   the checkpoints over it vouch for it.
//! - **Once**: a statement taken again (a leg's records sent again after a reconnect) stays where
//!   kiln first took it; a signature of it by another is kept beside it. Nothing is pruned.
//! - **Checkpoints** ([`checkpoint`], [`checkpoints_loop`]): signed by the server once due, each
//!   only after the entries since the one before hold as a chain and hash as kept, and the tree
//!   as kept extends the one before's: kiln signs none that doesn't, saying why.
//! - **Anchors** ([`anchor`], [`anchors`]): each checkpoint logged in the Rekor `[ci.attest]
//!   rekor_url` names, as a hashedrekord entry of its note's text, and the trail held to what Rekor
//!   saw: rewritten and checkpointed anew, it no longer makes the root an anchor names.
//! - **A tiled log** ([`log`]): the newest checkpoint and the tree's tiles and entry bundles (C2SP
//!   tlog-tiles), from which anyone who may read the trail computes any proof. Each tile a leaf
//!   completes is kept as a node of the level above as it's appended, so a tile, a root or a proof
//!   is a few reads however long the trail.

use std::sync::Arc;

use base64::Engine as _;
use kiln_ci::identity::statement::Statement;
use kiln_ci::identity::trail::{self, Checkpoint, Entry, Hash};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::store::Store;

/// How often the server looks for a checkpoint that's due.
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Appends signed statement `bundle` (a Sigstore bundle, as kiln keeps it), taken at `taken_at`,
/// in the transaction `c` that takes it, and keeps its signature beside it: whether the statement
/// is new to the trail. Signed again by another (the leg that built an artifact signing the
/// provenance kiln wrote), it stays where it is and has that signature too.
pub fn take(c: &Connection, bundle: &str, taken_at: i64) -> anyhow::Result<bool> {
    let v: Value = serde_json::from_str(bundle)?;
    let envelope = v.get("dsseEnvelope").ok_or_else(|| anyhow::anyhow!("the statement signs no DSSE envelope"))?;
    let payload_type = envelope.get("payloadType").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| anyhow::anyhow!("the statement's envelope has no payload type"))?;
    let payload = B64.decode(envelope.get("payload").and_then(Value::as_str).unwrap_or_default())?;
    let signature = B64.decode(envelope.pointer("/signatures/0/sig").and_then(Value::as_str).unwrap_or_default())?;
    let (new, digest) = append(c, payload_type, &payload, taken_at)?;
    c.execute("INSERT OR IGNORE INTO trail_signatures (statement, signature, bundle, taken_at) VALUES (?1, ?2, ?3, ?4)", params![digest, crate::util::sha256_hex(&signature), bundle, taken_at])?;
    Ok(new)
}

/// Appends kiln's own `statement` of `payload_type` ([`trail::PAYLOAD_CHECKED`],
/// [`trail::PAYLOAD_REVOCATION`]), taken at `taken_at`, in the transaction `c` that records what
/// it says: whether it's new to the trail.
pub fn own(c: &Connection, payload_type: &str, statement: &Statement, taken_at: i64) -> anyhow::Result<bool> {
    let payload = kiln_ci::identity::statement::payload(statement).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(append(c, payload_type, &payload, taken_at)?.0)
}

/// Appends the statement `payload` of `payload_type` unless the trail has it: whether it's new,
/// and its digest.
fn append(c: &Connection, payload_type: &str, payload: &[u8], taken_at: i64) -> anyhow::Result<(bool, String)> {
    let (digest, kind, subjects) = trail::about(payload_type, payload);
    if c.query_row("SELECT 1 FROM trail WHERE statement = ?1", [&digest], |_| Ok(())).optional()?.is_some() {
        return Ok((false, digest));
    }
    let last = c.query_row(&format!("SELECT {COLUMNS} FROM trail ORDER BY idx DESC LIMIT 1"), [], raw).optional()?.map(entry).transpose()?;
    let e = trail::next(last.as_ref().map(|(e, _)| e), &digest, &kind, subjects, taken_at).map_err(|b| anyhow::anyhow!("{b}"))?;
    let idx = i64::try_from(e.index)?;
    c.execute(
        "INSERT INTO trail (idx, prev, statement, kind, subjects, taken_at, hash, payload_type, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![idx, hex::encode(e.prev), e.statement, e.kind, serde_json::to_string(&e.subjects)?, e.taken_at, hex::encode(e.hash()), payload_type, payload],
    )?;
    for s in &e.subjects {
        c.execute("INSERT OR IGNORE INTO trail_subjects (subject, idx) VALUES (?1, ?2)", params![s, idx])?;
    }
    // A tile it completes is a node of the level above, and so on up while each completes.
    let (mut count, mut level) = (e.index + 1, 0u8);
    while count % trail::TILE == 0 {
        let t = trail::Tile { level, index: count / trail::TILE - 1, width: trail::TILE };
        let hash = trail::root(&tile_hashes(c, t)?);
        c.execute("INSERT OR IGNORE INTO trail_nodes (level, idx, hash) VALUES (?1, ?2, ?3)", params![i64::from(level) + 1, i64::try_from(t.index)?, hex::encode(hash)])?;
        count /= trail::TILE;
        level += 1;
    }
    Ok((true, digest))
}

/// Tile `t`'s hashes as kept: the leaves' at level 0, the whole subtrees' (`trail_nodes`) above.
fn tile_hashes(c: &Connection, t: trail::Tile) -> anyhow::Result<Vec<Hash>> {
    let first = t.index.checked_mul(trail::TILE).ok_or_else(|| anyhow::anyhow!("tile {} is past any trail", t.index))?;
    let (from, to) = (i64::try_from(first)?, i64::try_from(first.saturating_add(t.width))?);
    let rows: Vec<String> = if t.level == 0 {
        c.prepare("SELECT hash FROM trail WHERE idx >= ?1 AND idx < ?2 ORDER BY idx")?.query_map([from, to], |r| r.get(0))?.collect::<Result<_, _>>()?
    } else {
        c.prepare("SELECT hash FROM trail_nodes WHERE level = ?1 AND idx >= ?2 AND idx < ?3 ORDER BY idx")?.query_map(params![i64::from(t.level), from, to], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    anyhow::ensure!(rows.len() as u64 == t.width, "the trail keeps {} of tile {}'s {} hashes at level {}", rows.len(), t.index, t.width, t.level);
    rows.iter().map(|h| unhex(h)).collect()
}

/// What `GET /api/trail/log/<path>` answers (C2SP tlog-tiles, that its prefix): the newest
/// checkpoint (`checkpoint`), a tile or an entry bundle; its bytes, content type and whether it
/// never changes. None for a path that isn't one, or a tile whose every hash the trail hasn't.
pub async fn log(store: &Store, path: &str) -> anyhow::Result<Option<(Vec<u8>, &'static str, bool)>> {
    if path == "checkpoint" {
        return Ok(checkpoints(store, 1).await?.into_iter().next().map(|k| (k.note.into_bytes(), "text/plain; charset=utf-8", false)));
    }
    let Some((t, entries)) = trail::Tile::parse(path) else { return Ok(None) };
    store
        .call(move |c| {
            match t.span() {
                Some((_, end)) if end <= size_of(c)? => {}
                _ => return Ok(None),
            }
            let bytes = if entries {
                let (from, to) = t.span().map_or((0, 0), |(a, b)| (i64::try_from(a).unwrap_or(i64::MAX), i64::try_from(b).unwrap_or(i64::MAX)));
                let rows: Vec<Raw> = c.prepare(&format!("SELECT {COLUMNS} FROM trail WHERE idx >= ?1 AND idx < ?2 ORDER BY idx"))?.query_map([from, to], raw)?.collect::<Result<_, _>>()?;
                let data: Vec<Vec<u8>> = rows.into_iter().map(|r| entry(r).map(|(e, _)| e.encoding())).collect::<anyhow::Result<_>>()?;
                trail::bundle(&data).ok_or_else(|| anyhow::anyhow!("an entry of tile {} is too long for a bundle", t.index))?
            } else {
                tile_hashes(c, t)?.concat()
            };
            Ok(Some((bytes, "application/octet-stream", true)))
        })
        .await
}

/// An entry's columns, as [`raw`] reads them.
const COLUMNS: &str = "idx, prev, statement, kind, subjects, taken_at, hash";

type Raw = (i64, String, String, String, String, i64, String);

fn raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
}

/// An entry as kept, and its hash as kept.
fn entry((idx, prev, statement, kind, subjects, taken_at, hash): Raw) -> anyhow::Result<(Entry, String)> {
    Ok((Entry { index: u64::try_from(idx)?, prev: unhex(&prev)?, statement, kind, subjects: serde_json::from_str(&subjects)?, taken_at }, hash))
}

fn unhex(s: &str) -> anyhow::Result<Hash> {
    hex::decode(s)?.try_into().map_err(|_| anyhow::anyhow!("{s:?} isn't a SHA-256"))
}

/// How many entries the trail has.
fn size_of(c: &Connection) -> anyhow::Result<u64> {
    let n: i64 = c.query_row("SELECT COALESCE(MAX(idx) + 1, 0) FROM trail", [], |r| r.get(0))?;
    Ok(u64::try_from(n)?)
}

/// A checkpoint as kiln keeps it.
pub struct Kept {
    pub size: u64,
    pub root: Hash,
    /// The signed note.
    pub note: String,
    /// The certificate it's signed under (DER).
    pub certificate: Vec<u8>,
    pub signed_at: i64,
}

/// kiln's checkpoints, the newest first, at most `limit`.
pub async fn checkpoints(store: &Store, limit: u64) -> anyhow::Result<Vec<Kept>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows: Vec<(i64, String, String, String, i64)> = store
        .call(move |c| {
            let mut q = c.prepare("SELECT size, root, note, certificate, signed_at FROM trail_checkpoints ORDER BY size DESC LIMIT ?1")?;
            let rows = q.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    rows.into_iter().map(|(size, root, note, certificate, signed_at)| Ok(Kept { size: u64::try_from(size)?, root: unhex(&root)?, note, certificate: B64.decode(certificate)?, signed_at })).collect()
}

/// Signs a checkpoint of the trail as kiln's server when one is due ([`trail::due`]; `now`: as
/// soon as any entry came after the last one): the size it signed at, if it signed. The entries
/// since the last checkpoint must hold as a chain and hash as kept, and the tree over the leaves
/// kept must extend the last checkpoint's: else it isn't signed, saying why.
pub async fn checkpoint(hub: &Hub, now: bool) -> anyhow::Result<Option<u64>> {
    let (every, minutes) = (hub.cfg.ci.trail.checkpoint_every, hub.cfg.ci.trail.checkpoint_minutes);
    let at = crate::util::now_unix();
    let last = checkpoints(&hub.store, 1).await?.into_iter().next();
    let signed = last.as_ref().map_or(0, |k| k.size);
    let from = i64::try_from(signed)?;
    let (size, oldest) = hub
        .store
        .call(move |c| {
            let oldest: Option<i64> = c.query_row("SELECT taken_at FROM trail WHERE idx = ?1", [from], |r| r.get(0)).optional()?;
            Ok((size_of(c)?, oldest))
        })
        .await?;
    let due = if now { size > signed } else { trail::due(size, signed, oldest, at, every, minutes) };
    if !due {
        return Ok(None);
    }
    // The tree's root, and its consistency proof from the last checkpoint, hashed up from its
    // tiles as kept; the entries since the last checkpoint whole, with the one before them.
    let (to, before) = (i64::try_from(size)?, from.saturating_sub(1));
    let (root, proof, recent) = hub
        .store
        .call(move |c| {
            let mut tile = |t: trail::Tile| tile_hashes(c, t).ok();
            let root = trail::mth_from(size, 0, size, &mut tile).ok_or_else(|| anyhow::anyhow!("the trail's tiles up to {size} aren't all kept"))?;
            let proof = trail::consistency_by(size, signed, &mut |lo, hi| trail::mth_from(size, lo, hi, &mut tile)).ok_or_else(|| anyhow::anyhow!("the trail's tiles up to {size} aren't all kept"))?;
            let recent: Vec<Raw> = c.prepare(&format!("SELECT {COLUMNS} FROM trail WHERE idx >= ?1 AND idx < ?2 ORDER BY idx"))?.query_map([before, to], raw)?.collect::<Result<_, _>>()?;
            Ok((root, proof, recent))
        })
        .await?;
    let recent: Vec<(Entry, String)> = recent.into_iter().map(entry).collect::<anyhow::Result<_>>()?;
    let (head, run) = match recent.split_first() {
        Some((h, rest)) if signed > 0 => (Some(&h.0), rest),
        _ => (None, recent.as_slice()),
    };
    if let Some((e, _)) = run.iter().find(|(e, kept)| hex::encode(e.hash()) != *kept) {
        anyhow::bail!("entry {} doesn't hash to the hash kept beside it", e.index);
    }
    let run: Vec<Entry> = run.iter().map(|(e, _)| e.clone()).collect();
    trail::check(head, &run)?;
    let origin = trail::origin(&hub.pki.organization());
    let cp = Checkpoint { origin: origin.clone(), size, root };
    let previous = last.as_ref().map(|k| Checkpoint { origin, size: k.size, root: k.root });
    let pki = hub.pki.clone();
    let mut under = Vec::new();
    let note = trail::signed(&cp, previous.as_ref().map(|p| (p, proof.as_slice())), |m| {
        let (signature, certificate) = crate::identities::sign_own(&pki, "server", m).map_err(|e| format!("{e:#}"))?;
        under.clone_from(&certificate);
        Ok((signature, certificate))
    })?;
    let (root, certificate) = (hex::encode(cp.root), B64.encode(&under));
    let kept = hub
        .store
        .call(move |c| Ok(c.execute("INSERT OR IGNORE INTO trail_checkpoints (size, root, note, certificate, signed_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![to, root, note, certificate, at])?))
        .await?;
    if kept > 0 {
        tracing::info!(target: "kiln.trail", size, "the trail's checkpoint signed");
    }
    Ok((kept > 0).then_some(size))
}

/// Signs the trail's checkpoints as they come due, and anchors each in the Rekor kiln is
/// configured with, until the server stops (docs/ci.md W27-R09); what can't be done is said once,
/// until it's done or why changes.
pub async fn checkpoints_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    let mut said = String::new();
    while hub.shutdown.sleep(LOOK_EVERY).await {
        let done = match checkpoint(&hub, false).await {
            Ok(_) => anchor(&hub).await.map(|_| ()).map_err(|e| e.context("the trail's checkpoint isn't anchored")),
            Err(e) => Err(e.context("the trail's checkpoint isn't signed")),
        };
        match done {
            Ok(()) => said.clear(),
            Err(e) => {
                let why = format!("{e:#}");
                if why != said {
                    tracing::warn!(target: "kiln.trail", error = %why, "the trail's checkpoints fall behind");
                    said = why;
                }
            }
        }
    }
    Ok(())
}

/// The Rekor `[ci.attest] rekor_url` names, the trail's checkpoints anchored there: its URL and a
/// client of it.
fn rekor(hub: &Hub) -> anyhow::Result<Option<(String, crate::ci::signing::Remote)>> {
    let Some(url) = hub.cfg.ci.attest.rekor_url.as_deref().map(|u| u.trim().trim_end_matches('/')).filter(|u| !u.is_empty()) else { return Ok(None) };
    let fulcio = hub.cfg.ci.attest.fulcio_url.as_deref().unwrap_or_default();
    Ok(Some((url.to_owned(), crate::ci::signing::Remote::new(fulcio, url)?)))
}

/// Anchors each checkpoint the configured Rekor hasn't yet (by its note: one signed anew at a size
/// anchored before is another): a hashedrekord entry of its note's
/// text (its SHA-256, and the note's signature with the certificate it's signed under), Rekor's
/// answer kept apart from the checkpoints. The sizes anchored now.
pub async fn anchor(hub: &Hub) -> anyhow::Result<Vec<u64>> {
    use kiln_seams::Sigstore as _;
    let Some((log, remote)) = rekor(hub)? else { return Ok(Vec::new()) };
    let l = log.clone();
    let pending: Vec<(i64, String, String)> = hub
        .store
        .call(move |c| Ok(c.prepare("SELECT size, note, certificate FROM trail_checkpoints WHERE note NOT IN (SELECT note FROM trail_anchors WHERE log = ?1) ORDER BY size")?.query_map([&l], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?))
        .await?;
    let mut anchored = Vec::new();
    for (size, note, certificate) in pending {
        let entry = {
            let (text, signatures) = trail::open(&note)?;
            let signature = signatures.first().map(|s| B64.encode(&s.signature)).ok_or_else(|| anyhow::anyhow!("checkpoint {size} has no signature"))?;
            let pem = kiln_seams::sigstore::pem("CERTIFICATE", &B64.decode(&certificate)?);
            json!({ "apiVersion": "0.0.1", "kind": "hashedrekord", "spec": {
                "data": { "hash": { "algorithm": "sha256", "value": crate::util::sha256_hex(text.as_bytes()) } },
                "signature": { "content": signature, "publicKey": { "content": B64.encode(pem) } },
            } })
        };
        let logged = match remote.log(entry).await {
            Ok(l) => l,
            // Logged before (kiln stopped before keeping Rekor's answer): read where it is.
            Err(kiln_seams::sigstore::Refusal::Conflict { index }) => remote.entry(index).await.map_err(|e| anyhow::anyhow!("checkpoint {size}'s entry {index} in {log}: {e}"))?,
            Err(e) => anyhow::bail!("checkpoint {size} isn't anchored in {log}: {e}"),
        };
        let (l, idx, at, answer) = (log.clone(), i64::try_from(logged.index)?, crate::util::now_unix(), logged.answer.to_string());
        let uuid = logged.uuid;
        hub.store
            .call(move |c| {
                Ok(c.execute("INSERT OR IGNORE INTO trail_anchors (size, log, idx, uuid, note, certificate, answer, anchored_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![size, l, idx, uuid, note, certificate, answer, at])?)
            })
            .await?;
        tracing::info!(target: "kiln.trail", size, %log, index = idx, "the trail's checkpoint anchored");
        anchored.push(u64::try_from(size)?);
    }
    Ok(anchored)
}

/// `GET /api/trail/anchors`: each of the trail's anchors as it stands now. One holds when Rekor
/// still has its entry, naming the SHA-256 of the anchored note's text (its promise and proof
/// Rekor's, checked against Sigstore's trusted root where kiln has it); the note is kiln's
/// server's; and the trail as kept makes the root the note names at its size. Else, why not.
pub async fn anchors(hub: &Hub) -> anyhow::Result<Value> {
    let rows: Vec<(i64, String, i64, String, String, i64)> = hub
        .store
        .call(|c| Ok(c.prepare("SELECT size, log, idx, note, certificate, anchored_at FROM trail_anchors ORDER BY size, idx")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<Result<_, _>>()?))
        .await?;
    let remote = rekor(hub)?;
    let trust = crate::ci::sigstore::trusted_root(&hub.cfg).await;
    let revocations = crate::identities::revocations(&hub.store).await?;
    let mut list = Vec::new();
    for (size, log, idx, note, certificate, at) in rows {
        let held = held(hub, remote.as_ref().filter(|(url, _)| *url == log), &trust, &revocations, idx, &note, &certificate, at).await;
        let rekor = match &trust {
            Ok(_) => "verified".to_owned(),
            Err(e) => format!("Rekor's promise and proof unchecked: no trusted root ({e:#})"),
        };
        list.push(json!({ "size": size, "log": log, "index": idx, "holds": held.is_ok(), "why": held.err().map(|e| format!("{e:#}")), "rekor": rekor }));
    }
    Ok(json!({ "anchors": list }))
}

/// Whether anchor `idx` (checkpoint `note`, signed under `certificate`, anchored at `at`) holds.
#[allow(clippy::too_many_arguments)]
async fn held(hub: &Hub, remote: Option<&(String, crate::ci::signing::Remote)>, trust: &anyhow::Result<crate::ci::sigstore::TrustedRoot>, revocations: &[kiln_ci::identity::chain::Revocation], idx: i64, note: &str, certificate: &str, at: i64) -> anyhow::Result<()> {
    use kiln_seams::Sigstore as _;
    let cert = B64.decode(certificate)?;
    let (cp, _) = trail::verify_note(note, &hub.pki.organization(), &[&cert], &hub.pki.ca_der, revocations, at)?;
    let (text, _) = trail::open(note)?;
    let (log, client) = remote.ok_or_else(|| anyhow::anyhow!("kiln isn't configured with the Rekor that anchored checkpoint {}", cp.size))?;
    let logged = client.entry(u64::try_from(idx)?).await.map_err(|e| anyhow::anyhow!("entry {idx} of {log}: {e}"))?;
    let body: Value = match trust {
        Ok(root) => crate::ci::sigstore::logged(&logged.answer, root)?,
        Err(_) => serde_json::from_slice(&B64.decode(&logged.body)?)?,
    };
    anyhow::ensure!(body.pointer("/spec/data/hash/value").and_then(Value::as_str) == Some(crate::util::sha256_hex(text.as_bytes()).as_str()), "entry {idx} of {log} names another hash than checkpoint {}'s", cp.size);
    let size = cp.size;
    let kept = hub
        .store
        .call(move |c| {
            let now = size_of(c)?;
            anyhow::ensure!(now >= size, "the trail has {now} entries, fewer than the {size} Rekor saw");
            Ok(trail::mth_from(now, 0, size, &mut |t| tile_hashes(c, t).ok()))
        })
        .await?;
    anyhow::ensure!(kept == Some(cp.root), "the trail's first {size} entries don't make the root Rekor saw at entry {idx} of {log}");
    Ok(())
}

/// `GET /api/trail`: the trail's size, and its entries from index `from`, at most `limit` (100;
/// 1,000 at most): each with its hash as kept.
pub async fn page(store: &Store, from: u64, limit: u64) -> anyhow::Result<Value> {
    let (from, limit) = (i64::try_from(from).unwrap_or(i64::MAX), i64::try_from(limit.clamp(1, 1000)).unwrap_or(1000));
    let (size, rows) = store
        .call(move |c| {
            let rows: Vec<Raw> = c.prepare(&format!("SELECT {COLUMNS} FROM trail WHERE idx >= ?1 ORDER BY idx LIMIT ?2"))?.query_map([from, limit], raw)?.collect::<Result<_, _>>()?;
            Ok((size_of(c)?, rows))
        })
        .await?;
    let entries = rows
        .into_iter()
        .map(|r| {
            let (e, hash) = entry(r)?;
            Ok(json!({ "index": e.index, "prev": hex::encode(e.prev), "statement": e.statement, "kind": e.kind, "subjects": e.subjects, "taken_at": e.taken_at, "hash": hash }))
        })
        .collect::<anyhow::Result<Vec<Value>>>()?;
    Ok(json!({ "size": size, "entries": entries }))
}

/// `GET /api/trail/checkpoints`: kiln's checkpoints, the newest first, at most `limit` (10): each
/// its size, root (hex), signed note, the certificate it's signed under (DER, base64) and when.
pub async fn checkpoints_page(store: &Store, limit: u64) -> anyhow::Result<Value> {
    let kept = checkpoints(store, limit.clamp(1, 1000)).await?;
    let list: Vec<Value> = kept.iter().map(|k| json!({ "size": k.size, "root": hex::encode(k.root), "note": k.note, "certificate": B64.encode(&k.certificate), "signed_at": k.signed_at })).collect();
    Ok(json!({ "checkpoints": list }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trail as kept, from its start.
    async fn kept(store: &Store) -> anyhow::Result<Vec<(Entry, String)>> {
        let rows: Vec<Raw> = store.call(|c| Ok(c.prepare(&format!("SELECT {COLUMNS} FROM trail ORDER BY idx"))?.query_map([], raw)?.collect::<Result<_, _>>()?)).await?;
        rows.into_iter().map(entry).collect()
    }

    #[test]
    fn w27_r09_appends_at_once_chain_and_a_statement_goes_in_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = Store::in_memory()?;
            let said = |i: u64| trail::revocation(Some(&format!("{i:08x}")), None, "fedora-2", 1_790_000_000, "a test", "ada");
            let calls = (0..32u64).map(|i| {
                let (store, s) = (store.clone(), said(i));
                async move { store.call(move |c| own(c, trail::PAYLOAD_REVOCATION, &s, 1_790_000_000)).await }
            });
            for appended in futures::future::join_all(calls).await {
                assert!(appended?, "each new to the trail");
            }
            let again = said(7);
            assert!(!store.call(move |c| own(c, trail::PAYLOAD_REVOCATION, &again, 1_790_000_100)).await?, "taken again, it stays where it was");
            let t = kept(&store).await?;
            assert_eq!(t.len(), 32);
            assert!(t.iter().all(|(e, hash)| hex::encode(e.hash()) == *hash), "each hashes as kept");
            trail::check(None, &t.into_iter().map(|(e, _)| e).collect::<Vec<_>>())?;
            let v = page(&store, 30, 10).await?;
            assert_eq!((v.get("size").and_then(Value::as_u64), v.get("entries").and_then(Value::as_array).map(Vec::len)), (Some(32), Some(2)));
            Ok(())
        })
    }

    #[test]
    fn w27_r09_the_trail_reads_as_tiles_as_kept() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = Store::in_memory()?;
            for i in 0..600u64 {
                let s = trail::revocation(Some(&format!("{i:08x}")), None, "fedora-2", 1_790_000_000, "a test", "ada");
                store.call(move |c| own(c, trail::PAYLOAD_REVOCATION, &s, 1_790_000_000)).await?;
            }
            let leaves: Vec<Hash> = kept(&store).await?.iter().map(|(e, _)| e.hash()).collect();
            let part = |lo: usize, hi: usize| leaves.get(lo..hi).unwrap_or_default();
            let tile = |path: &'static str| {
                let store = store.clone();
                async move { log(&store, path).await }
            };
            // Level 1: the two whole blocks of 256, each kept as its last leaf came.
            let (bytes, _, immutable) = tile("tile/1/000.p/2").await?.ok_or_else(|| anyhow::anyhow!("tile 1/0 of 2"))?;
            assert_eq!(bytes, [trail::root(part(0, 256)), trail::root(part(256, 512))].concat());
            assert!(immutable);
            assert!(tile("tile/1/000.p/3").await?.is_none() && tile("tile/1/000").await?.is_none(), "a block not whole yet");
            // Level 0: the leaves, the last tile partial; an entry bundle's entries hash to them.
            let (bytes, ..) = tile("tile/0/002.p/88").await?.ok_or_else(|| anyhow::anyhow!("tile 0/2 of 88"))?;
            assert_eq!(bytes, part(512, 600).concat());
            let (bytes, ..) = tile("tile/entries/000").await?.ok_or_else(|| anyhow::anyhow!("entries 0"))?;
            let data = trail::unbundle(&bytes).ok_or_else(|| anyhow::anyhow!("a bundle"))?;
            assert_eq!(data.iter().map(|d| trail::leaf(d)).collect::<Vec<_>>(), part(0, 256));
            // The root, hashed up from the tiles as kept, is the leaves'.
            let root = store.call(|c| Ok(trail::mth_from(600, 0, 600, &mut |t| tile_hashes(c, t).ok()))).await?;
            assert_eq!(root, Some(trail::root(&leaves)));
            assert!(tile("checkpoint").await?.is_none(), "none signed yet");
            Ok(())
        })
    }
}
