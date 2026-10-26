//! Identities kiln's CA issued, and their revocations, kept (docs/ci.md W27-R01; migration
//! `w27_identities`). Each certificate is recorded as it's issued, by kind and subject (a machine's
//! name, else its kiln:// URI) with its serial and the SHA-256 of its key. A revocation is a row
//! with the time it took effect, why and by whom: the TLS directory refuses revoked keys from
//! then on (loaded at start, so a restart keeps them, and a machine enrolling again under the same
//! name gets a new key the revocation doesn't touch), and [`revocations`] feeds the chain checker
//! (`kiln_ci::identity::chain`).

use kiln_ci::identity::chain::Revocation;
use rusqlite::{OptionalExtension as _, params};

use crate::store::Store;

/// What a revocation names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// One certificate, by its serial (hex).
    Serial(String),
    /// Every certificate for a key, by its SPKI's SHA-256 (hex): a key lost.
    Key(String),
    /// Every certificate recorded for a subject: a machine's name, or a kiln:// URI.
    Subject(String),
}

/// Records a certificate kiln just issued.
pub async fn record(store: &Store, kind: &str, subject: &str, cert_pem: &str) -> anyhow::Result<()> {
    let f = crate::pki::fingerprints(cert_pem)?;
    let (kind, subject, now) = (kind.to_owned(), subject.to_owned(), crate::util::now_unix());
    store
        .call(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO identities (serial, kind, subject, key_sha256, not_before, not_after, issued_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![f.serial, kind, subject, f.key, f.not_before, f.not_after, now],
            )?;
            Ok(())
        })
        .await
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
