//! kiln's server's end of its cluster group's custody (docs/ci.md W27-R08; the Transfers plan's step
//! 12, path f; docs/p2p.md P2): what it hands each runner's learner of its records, and the records
//! the learner signs back (`crate::store::learner`, `crate::runner::custody::cluster_received`).
//!
//! - **A copy of kiln.db.** kiln hashes each copy it makes for a learner and hands it on, by its
//!   SHA-256, to the runner process that asked for it (`?instance=`, a learner that signs its
//!   receipts): its hand-on, signed and kept, goes with the copy in the [`COPY_HEADER`] response
//!   header, by its digest. The learner hashes the copy as it comes and receives it as handed on,
//!   or refuses it, its receipt of what came the break kiln records, and fetches another.
//! - **The log.** Every [`CHECK_EVERY`] while entries flow, kiln sends each learner that checks
//!   them a checkpoint of the log ([`Msg::Checkpoint`]): its hand-on of the entries since the
//!   learner's period began, by the SHA-256 of their leaves (`crate::store::group::Leaves`). The
//!   first a minute or more into a period closes it (`crate::store::learner::Periods`), and only
//!   that one is kept here: the records a quiet group's own checkpoints write have it send one more
//!   at the most, never one each [`CHECK_EVERY`]. The learner checks each, signs its receipt of
//!   each that closes, sends kiln the hand-on with its receipt of one that doesn't match, and
//!   copies the database again.
//!
//! A learner that doesn't sign its receipts (a runner from before, an unsigned one) asks with no
//! `instance` and says it checks nothing: it's handed nothing signed, and sent no checkpoint.

use std::path::Path;
use std::sync::Arc;

use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{Statement, Subject};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hub::Hub;
use crate::store::learner::{CHECK_EVERY, Due, Msg};

/// The response header a copy of kiln.db goes with when kiln hands it on: [`CopyHanded`], as JSON.
/// Its hand-on by its digest, not whole: a proxy before a server may cap a response's head at 4 KiB.
pub const COPY_HEADER: &str = "kiln-custody";

/// What [`COPY_HEADER`] says: kiln's hand-on of the copy (its digest, which the learner's receipt
/// follows), the copy's SHA-256 as kiln handed it on, and the entry it stands at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyHanded {
    pub hand_on: String,
    pub sha256: String,
    pub at: u64,
}

/// `name`, by its SHA-256 (hex).
fn by_sha256(name: String, sha256: &str) -> Subject {
    Subject { name, digest: std::collections::BTreeMap::from([("sha256".to_owned(), sha256.to_ascii_lowercase())]), ..Subject::default() }
}

/// A copy of kiln.db made at entry `at` of its log, by its SHA-256.
pub fn copy_thing(at: u64, sha256: &str) -> Subject {
    by_sha256(format!("copy of kiln.db at entry {at}"), sha256)
}

/// Entries `from + 1` to `to` of the cluster group's log, by the SHA-256 of their leaves.
pub fn log_thing(from: u64, to: u64, sha256: &str) -> Subject {
    by_sha256(format!("entries {} to {to} of kiln's cluster log", from.saturating_add(1)), sha256)
}

/// kiln's server's (`server`) hand-on of `thing` to runner process `to`, for `why`.
fn hand_on(server: &str, to: &str, thing: Subject, why: &str) -> Statement {
    let body = Body { to: Some(to.to_owned()), why: why.to_owned(), ..Body::new(Kind::HandedOn, server) };
    Statement { subject: vec![thing], ..Statement::new(to_value(&body)) }
}

/// A learner's receipt, as runner process `by`, of `thing` (by what came) from `from` (kiln's
/// server), following kiln's hand-on `follows` (its digest), for `why`.
pub fn receipt(by: &str, from: &str, thing: Subject, follows: &str, why: &str) -> Statement {
    let body = Body { from: Some(from.to_owned()), why: why.to_owned(), ..Body::new(Kind::Received, by) };
    Statement { subject: vec![thing], follows: vec![follows.to_owned()], ..Statement::new(to_value(&body)) }
}

/// Keeps `bundle`, kiln's own hand-on, saying so where it can't.
async fn kept(hub: &Hub, bundle: &Value) {
    match super::take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), bundle, "kiln", crate::util::now_unix()).await {
        Ok(Ok(_)) => {}
        Ok(Err(why)) => tracing::warn!(target: "kiln.custody", %why, "kiln refused its own hand-on to a learner of its cluster group"),
        Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "kiln's hand-on to a learner of its cluster group couldn't be kept"),
    }
}

/// The URI of runner process `instance` of `machine`, if it's connected and takes custody records.
async fn learner_uri(hub: &Hub, machine: &str, instance: &str) -> Option<String> {
    let takes = {
        let inst = hub.instances.load();
        inst.of(machine).into_iter().filter_map(|p| inst.get(p)).any(|i| i.label() == instance && (u64::from(i.caps) | i.wide) & crate::proto::CAP_CUSTODY != 0)
    };
    if !takes {
        return None;
    }
    crate::identities::runner_identity(hub, machine, instance).await.ok()?.uri().ok()
}

/// The entry a copy of kiln.db at `path` stands at, and its SHA-256 (hex).
fn copy_digest(path: &Path) -> anyhow::Result<(u64, String)> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let at: i64 = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?.query_row(&format!("SELECT applied FROM {}", crate::store::group::STATE), [], |r| r.get(0))?;
    let mut f = std::fs::File::open(path)?;
    let (mut h, mut buf) = (sha2::Sha256::new(), vec![0u8; 1 << 20]);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(buf.get(..n).unwrap_or_default());
    }
    Ok((u64::try_from(at).unwrap_or(0), hex::encode(h.finalize())))
}

/// A copy of kiln.db at `path`, made for `machine`'s learner: the learner's period of checkpoints
/// starts at its entry; and, for runner process `instance` (one that signs its receipts: none from a
/// learner from before), kiln's hand-on of it by its SHA-256, signed and kept, as [`COPY_HEADER`]
/// says it. None when kiln hands it on unsigned.
pub async fn copy_hand_on(hub: &Hub, machine: &str, instance: Option<&str>, path: &Path) -> Option<String> {
    let p = path.to_path_buf();
    let (at, sha256) = match crate::rt::blocking(move || copy_digest(&p)).await.and_then(|d| d) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "a copy of kiln.db couldn't be hashed: it goes unsigned");
            return None;
        }
    };
    hub.cluster.copied(machine, at);
    let to = learner_uri(hub, machine, instance?).await?;
    let server = super::server_uri(&hub.pki.organization());
    // Signed on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3).
    let bundle = match super::server_signed_off(&hub.pki, hand_on(&server, &to, copy_thing(at, &sha256), "a copy of kiln.db, for the replica this runner keeps")).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "kiln's hand-on of a copy of kiln.db couldn't be signed: it goes unsigned");
            return None;
        }
    };
    kept(hub, &bundle).await;
    serde_json::to_string(&CopyHanded { hand_on: super::digest_of(&bundle), sha256, at }).ok()
}

/// Until the server stops: every [`CHECK_EVERY`], each checkpoint of the log due to a learner that
/// checks them ([`crate::store::learner::Periods`]), signed and sent; one that closes a period kept.
/// None with learners off (`[cluster] learners = false`: every runner lets its replica go).
#[allow(clippy::disallowed_methods)] // (e) checkpoints looked at every CHECK_EVERY; the log's writes could tell
pub async fn checkpoints_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    if !hub.cfg.cluster.learners {
        return Ok(());
    }
    while hub.shutdown.sleep(CHECK_EVERY).await {
        let Some(group) = hub.store.group() else { return Ok(()) };
        for d in hub.cluster.due(group.standing().1) {
            checkpoint(&hub, d).await;
        }
    }
    Ok(())
}

/// Checkpoint `d`: signed, kept if it closes its period, and sent to the runner process holding the
/// learner's replica, if it still does and takes custody records.
async fn checkpoint(hub: &Arc<Hub>, d: Due) {
    if hub.store.group().is_none() {
        return;
    }
    let instance = {
        let inst = hub.instances.load();
        inst.newest_with(&d.machine, crate::proto::CAP_CLUSTER).filter(|p| *p == d.peer).and_then(|p| inst.get(p)).map(crate::instances::Instance::label)
    };
    let to = match instance {
        Some(i) => learner_uri(hub, &d.machine, &i).await,
        None => None,
    };
    let Some(to) = to else {
        hub.cluster.away(d.member);
        return;
    };
    // Worked out on the compute pool (goal 9, plan 01a1a87b2cec step 3): a SHA-256 over the leaves of the learner's
    // period so far, up to 65,536 of them (2 MiB), on the shard that ran this loop every 10 s for each learner, up to
    // 0.93 ms on the CPU in rt9's rounds.
    let (h, first, last) = (Arc::clone(hub), d.from, d.to);
    let digest = match crate::rt::compute(move || h.store.group().map(|g| g.digest(first, last))).await {
        Ok(Some(Ok(digest))) => digest,
        // kiln no longer holds the leaves its period starts after (it started since, or the
        // learner's period outlasted what it keeps): one starts where the group stands.
        Ok(Some(Err(_))) => {
            hub.cluster.restart(d.member, d.to);
            return;
        }
        // The group gone meanwhile (the server stopping), or a job that unwound: this one isn't sent, and its period's
        // next comes in its time.
        Ok(None) | Err(_) => return,
    };
    let server = super::server_uri(&hub.pki.organization());
    let why = if d.closes { "entries of kiln's cluster log, for the replica this runner keeps: the last of a period" } else { "entries of kiln's cluster log, for the replica this runner keeps" };
    // Signed on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3): a public-key signature,
    // for each learner each period, on the shard that ran this loop.
    let bundle = match super::server_signed_off(&hub.pki, hand_on(&server, &to, log_thing(d.from, d.to, &digest), why)).await {
        Ok(b) => b,
        Err(e) => return tracing::warn!(target: "kiln.custody", machine = %d.machine, error = %format!("{e:#}"), "a checkpoint of the cluster log couldn't be signed"),
    };
    // Kept once a period: the learner sends the others' with its receipt when they don't match.
    if d.closes {
        kept(hub, &bundle).await;
    }
    let msg = Msg::Checkpoint { from: d.from, to: d.to, closes: d.closes, hand_on: bundle.to_string() };
    let Ok(frame) = crate::proto::encode(&crate::proto::ServerMsg::Cluster { json: crate::store::learner::encode(&msg), raft: Vec::new() }) else { return };
    if hub.net.send(d.peer, crate::proto::LANE_SYNC, crate::net::Kind::Control, frame).await {
        hub.cluster.sent(&d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::identity::statement::{PAYLOAD_CUSTODY, bundle, payload, to_sign};
    use rcgen::SigningKey as _;

    const ORG: &str = "hyper-light";
    /// The runner process holding a replica, in organization [`ORG`].
    const RUNNER: &str = "kiln://hyper-light/person/ada/machine/box-a/instance/main";

    /// A runner process with a key kiln certified as `uri`: its receipts, signed as the runner
    /// signs them (`crate::runner::custody::cluster_received`).
    struct Learner {
        uri: String,
        key: rcgen::KeyPair,
        der: Vec<u8>,
    }

    impl Learner {
        fn new(pki: &crate::pki::Pki, uri: &str) -> anyhow::Result<Self> {
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::make_csr_with_key("kiln-identity", &key)?;
            let pem = pki.issue_identity(&csr, &kiln_ci::identity::chain::Identity::parse(uri)?, &[], time::Duration::hours(1))?;
            Ok(Self { uri: uri.to_owned(), key: rcgen::KeyPair::from_pem(&key)?, der: crate::pki::der_of(&pem)? })
        }

        fn sign(&self, s: &Statement) -> anyhow::Result<Value> {
            let mut s = s.clone();
            if let Some(o) = s.body.as_object_mut() {
                o.insert("by".into(), Value::String(self.uri.clone()));
            }
            let p = payload(&s)?;
            Ok(bundle(PAYLOAD_CUSTODY, &p, &self.key.sign(&to_sign(PAYLOAD_CUSTODY, &p))?, &self.der))
        }
    }

    /// W27-R08 (the Transfers plan's step 12, path f): kiln's hand-on of a copy of kiln.db and of a
    /// stretch of its cluster log, each to the runner process holding a replica, and that process's
    /// receipt of each by the same SHA-256, walk unbroken from the receipt, both sides signed. A copy
    /// or a stretch that came other than handed on breaks at the learner's receipt, as `changed`.
    #[test]
    fn w27_r08_a_learners_copy_and_its_log_walk_from_kilns_hand_on_to_its_receipt() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = crate::store::Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            crate::identities::own_identities(&store, &pki).await?;
            let (root, org) = (pki.ca_der.clone(), pki.organization());
            let (server, at) = (super::super::server_uri(&org), crate::util::now_unix());
            let runner = RUNNER.replace(ORG, &org);
            let learner = Learner::new(&pki, &runner)?;
            let things = [copy_thing(12, &"c".repeat(64)), log_thing(12, 40, &"e".repeat(64))];
            for (n, thing) in things.into_iter().enumerate() {
                let given = super::super::server_signed(&pki, &hand_on(&server, &runner, thing.clone(), "a test"))?;
                let digest = super::super::digest_of(&given);
                anyhow::ensure!(super::super::take(&store, &root, &org, &given, "kiln", at).await?.is_ok(), "kiln keeps its hand-on");
                // As handed on: the chain walks from the learner's receipt to kiln's hand-on.
                let r = learner.sign(&receipt(&runner, &server, thing.clone(), &digest, "a test"))?;
                let t = super::super::take(&store, &root, &org, &r, "runner box-a", at).await?.map_err(|e| anyhow::anyhow!("{e}"))?;
                assert_eq!(t.broke, None, "{} as handed on", thing.name);
                let v = super::super::chain(&store, &root, &org, &t.digest, at).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
                assert_eq!(v.get("unbroken"), Some(&Value::Bool(true)), "{v}");
                let said = |h: &Value, k: &str| h.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                let hops: Vec<(String, String)> = v.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| (said(h, "kind"), said(h, "custodian"))).collect();
                assert_eq!(hops, [("received".to_owned(), runner.clone()), ("handed-on".to_owned(), server.clone())]);
                // Other than handed on: the break, at the learner's receipt of what came.
                let changed = by_sha256(thing.name.clone(), &n.to_string().repeat(64));
                let r = learner.sign(&receipt(&runner, &server, changed, &digest, "a test: not as handed on"))?;
                let t = super::super::take(&store, &root, &org, &r, "runner box-a", at).await?.map_err(|e| anyhow::anyhow!("{e}"))?;
                assert!(t.broke.as_deref().is_some_and(|b| b.contains(&thing.name) && b.contains("changed")), "{:?}", t.broke);
                let v = super::super::chain(&store, &root, &org, &t.digest, at).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
                assert_eq!((v.get("unbroken"), v.pointer("/break/kind")), (Some(&Value::Bool(false)), Some(&Value::String("changed".into()))), "{v}");
            }
            Ok(())
        })
    }

    /// What a copy's header says comes back as it went, and a header from a later kiln with more
    /// in it still reads.
    #[test]
    fn w27_r08_a_copys_header_reads_back() -> anyhow::Result<()> {
        let h = CopyHanded { hand_on: "sha256:ab".into(), sha256: "cd".into(), at: 7 };
        assert_eq!(serde_json::from_str::<CopyHanded>(&serde_json::to_string(&h)?)?, h);
        let later: CopyHanded = serde_json::from_str(r#"{"hand_on":"sha256:ab","sha256":"cd","at":7,"more":1}"#)?;
        assert_eq!(later, h);
        Ok(())
    }
}
