//! Identities kiln's CA issued, and their revocations, kept (docs/ci.md W27-R01; migration
//! `w27_identities`). Each certificate is recorded as it's issued, by kind and subject (a machine's
//! name, else its kiln:// URI) with its serial and the SHA-256 of its key. A revocation is a row
//! with the time it took effect, why and by whom: the TLS directory refuses revoked keys from
//! then on (loaded at start, so a restart keeps them, and a machine enrolling again under the same
//! name gets a new key the revocation doesn't touch), and [`revocations`] feeds the chain checker
//! (`kiln_ci::identity::chain`).

use kiln_ci::identity::chain::{Actor, Identity, Revocation};
use rusqlite::{OptionalExtension as _, params};

use crate::hub::Hub;
use crate::instances::PeerId;
use crate::proto::{IdentityAsk, IdentityGiven, ServerMsg};
use crate::store::Store;

/// How long a runner process's certificate lasts; it asks again on each connection.
pub const RUNNER_HOURS: i64 = 7 * 24;
/// How long an agent session's certificate lasts, for one epoch; asked again on each connection.
pub const AGENT_HOURS: i64 = 7 * 24;

/// What a revocation names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// One certificate, by its serial (hex).
    Serial(String),
    /// Every certificate for a key, by its SPKI's SHA-256 (hex): a key lost.
    Key(String),
    /// Every certificate recorded for a subject: a machine's name, or a kiln:// URI.
    Subject(String),
    /// A subject's certificates, by serial only: their key goes on (a runner process given a
    /// newer certificate for the same key, or gone with its key).
    Certificates(String),
}

/// Records a certificate kiln just issued.
pub async fn record(store: &Store, kind: &str, subject: &str, cert_pem: &str) -> anyhow::Result<()> {
    let f = crate::pki::fingerprints(cert_pem)?;
    let (kind, subject, pem, now) = (kind.to_owned(), subject.to_owned(), cert_pem.to_owned(), crate::util::now_unix());
    store
        .call(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO identities (serial, kind, subject, key_sha256, not_before, not_after, issued_at, cert_pem) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![f.serial, kind, subject, f.key, f.not_before, f.not_after, now, pem],
            )?;
            Ok(())
        })
        .await
}

/// A person's name as an identity's path segment: `user:ada` and `pat:7 of user:ada` are `ada`;
/// the owner (`admin`) is `admin`; anything else is made safe the way machine names are.
pub fn person_of(caller: &str) -> String {
    let name = caller.rsplit_once("user:").map_or(caller, |(_, n)| n);
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    let safe = safe.trim_matches('-');
    if safe.is_empty() || safe == "." || safe == ".." { "admin".to_owned() } else { crate::util::truncate(safe, 128).to_owned() }
}

/// Records who enrolled `machine` (its last enrollment's).
pub async fn enrolled(store: &Store, machine: &str, by: &str) -> anyhow::Result<()> {
    let (machine, by, now) = (machine.to_owned(), person_of(by), crate::util::now_unix());
    store
        .call(move |c| {
            c.execute("INSERT INTO enrollments (machine, enrolled_by, at) VALUES (?1, ?2, ?3) ON CONFLICT (machine) DO UPDATE SET enrolled_by = excluded.enrolled_by, at = excluded.at", params![machine, by, now])?;
            Ok(())
        })
        .await
}

/// Who enrolled `machine`: `admin` for one enrolled before kiln recorded it.
pub async fn enrolled_by(store: &Store, machine: &str) -> anyhow::Result<String> {
    let machine = machine.to_owned();
    let by: Option<String> = store.call(move |c| Ok(c.query_row("SELECT enrolled_by FROM enrollments WHERE machine = ?1", [machine], |r| r.get(0)).optional()?)).await?;
    Ok(by.unwrap_or_else(|| "admin".to_owned()))
}

/// A runner process's identity: its machine and instance, who enrolled the machine, kiln's
/// organization.
pub async fn runner_identity(hub: &Hub, machine: &str, instance: &str) -> anyhow::Result<Identity> {
    let enrolled_by = enrolled_by(&hub.store, machine).await?;
    Ok(Identity::new(hub.pki.organization(), Actor::Runner { enrolled_by, machine: machine.to_owned(), instance: instance.to_owned() }))
}

/// A runner process asked for an identity ([`crate::proto::AgentMsg::Identity`]): the certificate,
/// recorded, or why not, back to it. Its earlier certificates for the same subject are revoked by
/// serial (the key goes on in the same process).
pub async fn on_ask(hub: &Hub, peer: PeerId, machine: &str, json: &str) {
    let ask: IdentityAsk = match serde_json::from_str(json) {
        Ok(a) => a,
        Err(e) => return tracing::debug!(agent = %machine, error = %e, "an unreadable identity request"),
    };
    let mut given = IdentityGiven { kind: ask.kind.clone(), of: ask.of.clone(), epoch: ask.epoch, ..IdentityGiven::default() };
    match issue(hub, peer, machine, &ask).await {
        Ok((uri, pem)) => {
            given.uri = uri;
            given.cert_pem = Some(pem);
        }
        Err(e) => {
            tracing::warn!(agent = %machine, kind = %ask.kind, error = %format!("{e:#}"), "no identity issued");
            given.error = Some(format!("{e:#}"));
        }
    }
    if let Ok(json) = serde_json::to_string(&given) {
        hub.send_peer(peer, &ServerMsg::Identity { json }).await;
    }
}

async fn issue(hub: &Hub, peer: PeerId, machine: &str, ask: &IdentityAsk) -> anyhow::Result<(String, String)> {
    let (identity, hours) = match ask.kind.as_str() {
        "runner" => {
            let instance = hub.instances.lock().get(peer).map(|i| i.label()).unwrap_or_else(|| "legacy".to_owned());
            (runner_identity(hub, machine, &instance).await?, RUNNER_HOURS)
        }
        "agent" => {
            let session = ask.of.as_deref().ok_or_else(|| anyhow::anyhow!("an agent's identity names its session"))?;
            let epoch = ask.epoch.ok_or_else(|| anyhow::anyhow!("an agent's identity names its epoch"))?;
            (agent_identity(hub, machine, session, epoch).await?, AGENT_HOURS)
        }
        other => anyhow::bail!("kiln doesn't issue {other:?} identities"),
    };
    let uri = identity.uri().map_err(|e| anyhow::anyhow!("{e}"))?;
    let pem = hub.pki.issue_identity(&ask.csr, &identity, &[], time::Duration::hours(hours))?;
    let now = crate::util::now_unix();
    revoke(&hub.store, Target::Certificates(uri.clone()), "superseded by a newer certificate", "kiln", now).await?;
    if let Actor::Agent { epoch, .. } = &identity.actor
        && let Some(session) = ask.of.as_deref()
    {
        // What the session signed at an epoch it has left is refused from the hand-over on.
        for old in epochs_of(&hub.store, session).await?.into_iter().filter(|(_, e)| e < epoch) {
            revoke(&hub.store, Target::Subject(old.0), &format!("moved on to epoch {epoch}"), "kiln", now).await?;
        }
    }
    record(&hub.store, &ask.kind, &uri, &pem).await?;
    tracing::info!(agent = %machine, identity = %identity, "issued an identity");
    Ok((uri, pem))
}

/// An agent session's identity at `epoch`, for the runner process of `machine` holding it: the
/// session must run there, at that epoch or a later one than kiln last heard (a copy left behind
/// at an older one gets none), and kiln must know whom it acts for.
async fn agent_identity(hub: &Hub, machine: &str, session: &str, epoch: u64) -> anyhow::Result<Identity> {
    let info = hub.harness.info(session).ok_or_else(|| anyhow::anyhow!("kiln knows no agent session {session} yet"))?;
    anyhow::ensure!(info.target == format!("agent:{machine}"), "agent session {session} runs on {}, not {machine}", info.target);
    let known = hub.harness.epoch_of(session);
    anyhow::ensure!(epoch >= known, "agent session {session} is at epoch {known}; epoch {epoch} is a copy left behind");
    let person = match crate::mcp::whose(hub, session).await {
        Some(p) if p == "owner" => "admin".to_owned(),
        Some(p) => person_of(&p),
        None => anyhow::bail!("kiln can't say whom agent session {session} acts for"),
    };
    Ok(Identity::new(hub.pki.organization(), Actor::Agent { person, agent: session.to_owned(), epoch }))
}

/// Every subject an agent session's certificates were issued for, with its epoch.
async fn epochs_of(store: &Store, session: &str) -> anyhow::Result<Vec<(String, u64)>> {
    let pattern = format!("%/agent/{session}/epoch/%");
    let subjects: Vec<String> = store
        .call(move |c| {
            let mut q = c.prepare("SELECT DISTINCT subject FROM identities WHERE kind = 'agent' AND subject LIKE ?1")?;
            let rows = q.query_map([pattern], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await?;
    Ok(subjects
        .into_iter()
        .filter_map(|s| match Identity::parse(&s).ok()?.actor {
            Actor::Agent { agent, epoch, .. } if agent == session => Some((s, epoch)),
            _ => None,
        })
        .collect())
}

/// Agent session `session` ended: every certificate of every epoch it had is revoked from now.
pub async fn agent_ended(store: &Store, session: &str) -> anyhow::Result<()> {
    let now = crate::util::now_unix();
    for (subject, _) in epochs_of(store, session).await? {
        revoke(store, Target::Subject(subject), "agent ended", "kiln", now).await?;
    }
    Ok(())
}

/// A runner process of `machine` went while another goes on (retired after a blue/green
/// upgrade): its certificates are revoked from now (its key went with it).
pub async fn runner_retired(hub: &Hub, machine: &str, instance: &str) -> anyhow::Result<()> {
    let uri = runner_identity(hub, machine, instance).await?.uri().map_err(|e| anyhow::anyhow!("{e}"))?;
    revoke(&hub.store, Target::Certificates(uri), "runner process retired", "kiln", crate::util::now_unix()).await?;
    Ok(())
}

/// One certificate kiln issued, as `GET /api/identities` lists it: checked now, against every
/// revocation, by `kiln_ci::identity::chain`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Listed {
    pub serial: String,
    pub kind: String,
    pub subject: String,
    pub key: String,
    pub not_before: i64,
    pub not_after: i64,
    pub issued_at: i64,
    /// Who it verifies as (`agent matrix, epoch 3, for sam, of hyper-light`), else why it doesn't.
    pub verified: Option<String>,
    pub refused: Option<String>,
}

/// The certificates kiln issued of `kind` (all, when `None`), newest first, each checked now.
pub async fn list(hub: &Hub, kind: Option<&str>) -> anyhow::Result<Vec<Listed>> {
    let kind = kind.map(str::to_owned);
    let rows: Vec<(String, String, String, String, i64, i64, i64, String)> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT serial, kind, subject, key_sha256, not_before, not_after, issued_at, cert_pem FROM identities WHERE (?1 IS NULL OR kind = ?1) ORDER BY issued_at DESC, serial")?;
            let rows = q.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let revs = revocations(&hub.store).await?;
    let now = crate::util::now_unix();
    Ok(rows
        .into_iter()
        .map(|(serial, kind, subject, key, not_before, not_after, issued_at, pem)| {
            let checked = crate::pki::der_of(&pem).map_err(|e| e.to_string()).and_then(|der| kiln_ci::identity::chain::verify(&[&der], &hub.pki.ca_der, &revs, now).map_err(|e| e.to_string()));
            let (verified, refused) = match checked {
                Ok(v) => (Some(v.to_string()), None),
                // A machine's TLS certificate names no kiln identity: say so only when it's revoked.
                Err(e) if kind == "machine" && !e.contains("revoked") => (None, None),
                Err(e) => (None, Some(e)),
            };
            Listed { serial, kind, subject, key, not_before, not_after, issued_at, verified, refused }
        })
        .collect())
}

/// Revokes what `target` names from `at` (unix seconds) on; returns the keys it revoked (for the
/// TLS directory). A subject with no recorded certificate revokes nothing.
pub async fn revoke(store: &Store, target: Target, reason: &str, by: &str, at: i64) -> anyhow::Result<Vec<String>> {
    let (reason, by) = (reason.to_owned(), by.to_owned());
    store
        .call(move |c| {
            // A savepoint: the store may already run this inside its own transaction (the cluster group).
            let tx = c.savepoint()?;
            // (serial, key, subject) of each row to write.
            let rows: Vec<(Option<String>, Option<String>, String)> = match &target {
                Target::Certificates(s) => {
                    let mut q = tx.prepare("SELECT serial FROM identities WHERE subject = ?1 AND serial NOT IN (SELECT serial FROM revocations WHERE serial IS NOT NULL)")?;
                    q.query_map([s], |r| Ok((Some(r.get::<_, String>(0)?), None, s.clone())))?.collect::<Result<Vec<_>, _>>()?
                }
                Target::Subject(s) => {
                    let mut q = tx.prepare("SELECT serial, key_sha256 FROM identities WHERE subject = ?1")?;
                    q.query_map([s], |r| Ok((Some(r.get::<_, String>(0)?), Some(r.get::<_, String>(1)?), s.clone())))?.collect::<Result<Vec<_>, _>>()?
                }
                Target::Serial(serial) => {
                    let serial = serial.to_ascii_lowercase();
                    let known: Option<(String, String)> =
                        tx.query_row("SELECT key_sha256, subject FROM identities WHERE serial = ?1", [&serial], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    let (key, subject) = known.map_or((None, serial.clone()), |(k, s)| (Some(k), s));
                    vec![(Some(serial), key, subject)]
                }
                Target::Key(key) => {
                    let key = key.to_ascii_lowercase();
                    let subject: Option<String> = tx.query_row("SELECT subject FROM identities WHERE key_sha256 = ?1 LIMIT 1", [&key], |r| r.get(0)).optional()?;
                    vec![(None, Some(key.clone()), subject.unwrap_or(key))]
                }
            };
            for (serial, key, subject) in &rows {
                tx.execute(
                    "INSERT INTO revocations (serial, key_sha256, subject, at, reason, by) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![serial, key, subject, at, reason, by],
                )?;
            }
            tx.commit()?;
            let mut keys: Vec<String> = rows.into_iter().filter_map(|(_, k, _)| k).collect();
            keys.sort();
            keys.dedup();
            Ok(keys)
        })
        .await
}

/// Every revocation, for the chain checker.
pub async fn revocations(store: &Store) -> anyhow::Result<Vec<Revocation>> {
    store
        .call(|c| {
            let mut q = c.prepare("SELECT serial, key_sha256, at, reason, by FROM revocations ORDER BY at, id")?;
            let rows = q
                .query_map([], |r| Ok(Revocation { serial: r.get(0)?, key: r.get(1)?, at: r.get(2)?, reason: r.get(3)?, by: r.get(4)? }))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// Every revoked key, for the TLS directory.
pub async fn revoked_keys(store: &Store) -> anyhow::Result<Vec<String>> {
    store
        .call(|c| {
            let mut q = c.prepare(
                "SELECT key_sha256 FROM revocations WHERE key_sha256 IS NOT NULL \
                 UNION SELECT i.key_sha256 FROM revocations r JOIN identities i ON i.serial = r.serial",
            )?;
            let rows = q.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(pki: &crate::pki::Pki, name: &str) -> anyhow::Result<String> {
        let (csr, _key) = crate::pki::make_csr(name)?;
        pki.sign_csr(&csr, name, 30)
    }

    #[test]
    fn w27_r01_the_person_an_enrollment_names() {
        assert_eq!(person_of("user:ada"), "ada");
        assert_eq!(person_of("pat:7 of user:ada"), "ada");
        assert_eq!(person_of("admin"), "admin");
        assert_eq!(person_of("user:sam o'neil"), "sam-o-neil");
        assert_eq!(person_of("user:"), "admin");
        assert_eq!(person_of("user:.."), "admin");
    }

    #[test]
    fn w27_r01_revocations_recorded_with_when_why_and_who() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let (a1, a2, b) = (cert(&pki, "fedora-2")?, cert(&pki, "fedora-2")?, cert(&pki, "mac-1")?);
            for c in [&a1, &a2] {
                record(&store, "machine", "fedora-2", c).await?;
            }
            record(&store, "machine", "mac-1", &b).await?;
            let keys = revoke(&store, Target::Subject("fedora-2".into()), "runner forgotten", "ada", 1_790_000_000).await?;
            let (fa1, fa2, fb) = (crate::pki::fingerprints(&a1)?, crate::pki::fingerprints(&a2)?, crate::pki::fingerprints(&b)?);
            let mut want = vec![fa1.key.clone(), fa2.key.clone()];
            want.sort();
            assert_eq!(keys, want);
            let mut stored = revoked_keys(&store).await?;
            stored.sort();
            assert_eq!(stored, want, "mac-1's key isn't revoked");
            let revs = revocations(&store).await?;
            assert_eq!(revs.len(), 2);
            assert!(revs.iter().all(|r| r.at == 1_790_000_000 && r.reason == "runner forgotten" && r.by == "ada"));
            // By serial: its key comes from the record.
            revoke(&store, Target::Serial(fb.serial.to_uppercase()), "key lost", "sam", 1_790_000_100).await?;
            assert!(revoked_keys(&store).await?.contains(&fb.key));
            // A subject with nothing recorded revokes nothing.
            assert!(revoke(&store, Target::Subject("nobody".into()), "x", "ada", 0).await?.is_empty());
            Ok(())
        })
    }
}
