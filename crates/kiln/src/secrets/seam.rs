//! kiln's own store behind kiln-seams' `Secrets` (CP-T8.3; Shards plan step 33's step 5): what
//! a run's credential leases are served from until Veil's store (CP-T8.1), and what
//! [`kiln_seams::secrets::SimSecrets`] stands for in tests, so that swapping one for the other
//! changes no other code (the seam's suite runs against both). Versions, stages, deletes and
//! leases are Secrets Manager's, as the simulator keeps them, timed by the clock it is given.
//!
//! Each value is sealed with AES-256-GCM under the secrets service's data keys (`secret_keys`,
//! wrapped by the server's master key: [`super::kiln`]), bound to its secret's name and its
//! version's id, so no ciphertext can be moved to another row. This is a store of its own beside
//! the service's app secrets, which agents never read: nothing of theirs is reachable here.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kiln_seams::Clock;
use kiln_seams::secrets::{Described, LEASE_MAX, LIST_MAX, Lease, Refusal, SecretValue, Secrets, Select, Stage, VERSIONS_MAX, Version, WINDOW_MAX, WINDOW_MIN, check_name, check_value};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::hub::Hub;

/// Secrets in kiln's store.
pub struct KilnSecrets {
    hub: Arc<Hub>,
    clock: Arc<dyn Clock>,
}

/// A step's answer: its refusal as it is, or the store's failure.
type Step<T> = anyhow::Result<Result<T, Refusal>>;

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// What binds a version's ciphertext to its row.
fn aad(name: &str, id: &str) -> Vec<u8> {
    format!("kiln seam secret\0{name}\0{id}").into_bytes()
}

fn letter(stage: Stage) -> &'static str {
    match stage {
        Stage::Current => "C",
        Stage::Previous => "P",
        Stage::Pending => "N",
    }
}

fn stages(letters: &str) -> Vec<Stage> {
    letters
        .chars()
        .filter_map(|c| match c {
            'C' => Some(Stage::Current),
            'P' => Some(Stage::Previous),
            'N' => Some(Stage::Pending),
            _ => None,
        })
        .collect()
}

/// Forgets `name` for good once its delete's window has passed.
fn purge(c: &Connection, name: &str, now: i64) -> anyhow::Result<()> {
    if c.execute("DELETE FROM seam_secrets WHERE name = ?1 AND purge_at_ms IS NOT NULL AND purge_at_ms <= ?2", params![name, now])? > 0 {
        c.execute("DELETE FROM seam_secret_versions WHERE name = ?1", [name])?;
    }
    Ok(())
}

/// A live secret: not there is `NotFound`; scheduled for deletion is `Deleted`.
fn live(c: &Connection, name: &str, now: i64) -> Step<()> {
    purge(c, name, now)?;
    let row: Option<Option<i64>> = c.query_row("SELECT purge_at_ms FROM seam_secrets WHERE name = ?1", [name], |r| r.get(0)).optional()?;
    Ok(match row {
        None => Err(Refusal::NotFound(name.to_owned())),
        Some(Some(purge_at_ms)) => Err(Refusal::Deleted { name: name.to_owned(), purge_at_ms }),
        Some(None) => Ok(()),
    })
}

/// A new version of `name` with `stage`, its value sealed, after its others.
fn insert(hub: &Hub, c: &Connection, name: &str, value: &SecretValue, stage: Stage, now: i64) -> anyhow::Result<Version> {
    let id = crate::util::new_id();
    let (key_id, key) = super::kiln::current_key(hub, c)?;
    let (ciphertext, nonce) = super::kiln::seal(&key, value.expose(), &aad(name, &id))?;
    let seq: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM seam_secret_versions WHERE name = ?1", [name], |r| r.get(0))?;
    c.execute(
        "INSERT INTO seam_secret_versions (name, seq, id, stages, created_ms, ciphertext, nonce, key_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![name, seq, id, letter(stage), now, ciphertext, nonce, key_id],
    )?;
    Ok(Version { id, stages: vec![stage], created_ms: now })
}

/// The version of `name` that `select` picks, with its value opened.
fn version(hub: &Hub, c: &Connection, name: &str, select: &Select) -> Step<(Version, SecretValue)> {
    let pick = |r: &rusqlite::Row<'_>| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Vec<u8>>(3)?, r.get::<_, Vec<u8>>(4)?, r.get::<_, String>(5)?));
    let row = match select {
        Select::Stage(stage) => c
            .query_row(
                "SELECT id, stages, created_ms, ciphertext, nonce, key_id FROM seam_secret_versions WHERE name = ?1 AND instr(stages, ?2) > 0 ORDER BY seq DESC LIMIT 1",
                params![name, letter(*stage)],
                pick,
            )
            .optional()?,
        Select::Version(id) => c.query_row("SELECT id, stages, created_ms, ciphertext, nonce, key_id FROM seam_secret_versions WHERE name = ?1 AND id = ?2", params![name, id], pick).optional()?,
    };
    let Some((id, letters, created_ms, ciphertext, nonce, key_id)) = row else {
        return Ok(Err(Refusal::NoSuchVersion { name: name.to_owned() }));
    };
    let keys = super::kiln::keys(hub, c)?;
    let key = keys.get(&key_id).ok_or_else(|| anyhow::anyhow!("secret {name:?}'s data key {key_id} is gone"))?;
    let opened = super::kiln::open(key, &ciphertext, &nonce, &aad(name, &id))?;
    Ok(Ok((Version { id, stages: stages(&letters), created_ms }, SecretValue::new(opened.bytes().to_vec()))))
}

/// A lease that hasn't expired, or `Expired`.
fn lease_of(c: &Connection, id: &str, now: i64) -> Step<Lease> {
    let row = c.query_row("SELECT name, expires_ms FROM seam_leases WHERE id = ?1 AND ?2 < expires_ms", params![id, now], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).optional()?;
    Ok(row.map(|(name, expires_ms)| Lease { id: id.to_owned(), name, expires_ms }).ok_or(Refusal::Expired))
}

fn ttl_ok(ttl: Duration) -> Result<(), Refusal> {
    if ttl.is_zero() || ttl > LEASE_MAX {
        return Err(Refusal::Invalid { what: "a lease's time", why: format!("{ttl:?} (more than 0, at most a day)") });
    }
    Ok(())
}

impl KilnSecrets {
    pub fn new(hub: Arc<Hub>, clock: Arc<dyn Clock>) -> KilnSecrets {
        KilnSecrets { hub, clock }
    }

    /// Runs `f` in one call of the store, at the clock's now: a refusal of its own as it is, the
    /// store's failure `Unavailable`.
    fn call<'a, T, F>(&'a self, f: F) -> BoxFuture<'a, Result<T, Refusal>>
    where
        T: Send + 'static,
        F: FnOnce(&Hub, &mut Connection, i64) -> Step<T> + Send + 'static,
    {
        let now = self.clock.unix_ms();
        let hub = Arc::clone(&self.hub);
        Box::pin(async move {
            let at = Arc::clone(&hub);
            match hub.store.call(move |c| f(&at, c, now)).await {
                Ok(out) => out,
                Err(e) => Err(Refusal::Unavailable(format!("{e:#}"))),
            }
        })
    }
}

impl Secrets for KilnSecrets {
    fn create<'a>(&'a self, name: &'a str, value: SecretValue) -> BoxFuture<'a, Result<Version, Refusal>> {
        if let Err(e) = check_name(name).and_then(|()| check_value(&value)) {
            return Box::pin(async move { Err(e) });
        }
        let name = name.to_owned();
        self.call(move |hub, c, now| {
            let tx = crate::store::tx(c)?;
            purge(&tx, &name, now)?;
            if tx.query_row("SELECT 1 FROM seam_secrets WHERE name = ?1", [&name], |_| Ok(())).optional()?.is_some() {
                return Ok(Err(Refusal::Exists(name)));
            }
            tx.execute("INSERT INTO seam_secrets (name, purge_at_ms) VALUES (?1, NULL)", [&name])?;
            let v = insert(hub, &tx, &name, &value, Stage::Current, now)?;
            tx.commit()?;
            Ok(Ok(v))
        })
    }

    fn put<'a>(&'a self, name: &'a str, value: SecretValue, stage: Stage) -> BoxFuture<'a, Result<Version, Refusal>> {
        if let Err(e) = check_value(&value) {
            return Box::pin(async move { Err(e) });
        }
        if stage == Stage::Previous {
            return Box::pin(async { Err(Refusal::Invalid { what: "a new version's stage", why: "Current or Pending".into() }) });
        }
        let name = name.to_owned();
        self.call(move |hub, c, now| {
            let tx = crate::store::tx(c)?;
            if let Err(e) = live(&tx, &name, now)? {
                return Ok(Err(e));
            }
            if stage == Stage::Current {
                tx.execute("UPDATE seam_secret_versions SET stages = replace(stages, 'P', '') WHERE name = ?1", [&name])?;
                tx.execute("UPDATE seam_secret_versions SET stages = replace(stages, 'C', 'P') WHERE name = ?1", [&name])?;
            } else {
                tx.execute("UPDATE seam_secret_versions SET stages = replace(stages, 'N', '') WHERE name = ?1", [&name])?;
            }
            let v = insert(hub, &tx, &name, &value, stage, now)?;
            // The oldest versions without a stage go first, past what's kept.
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM seam_secret_versions WHERE name = ?1", [&name], |r| r.get(0))?;
            let over = count.saturating_sub(i64::try_from(VERSIONS_MAX).unwrap_or(i64::MAX));
            if over > 0 {
                tx.execute(
                    "DELETE FROM seam_secret_versions WHERE name = ?1 AND seq IN (SELECT seq FROM seam_secret_versions WHERE name = ?1 AND stages = '' ORDER BY seq LIMIT ?2)",
                    params![name, over],
                )?;
            }
            tx.commit()?;
            Ok(Ok(v))
        })
    }

    fn get<'a>(&'a self, name: &'a str, select: Select) -> BoxFuture<'a, Result<(Version, SecretValue), Refusal>> {
        let name = name.to_owned();
        self.call(move |hub, c, now| {
            if let Err(e) = live(c, &name, now)? {
                return Ok(Err(e));
            }
            version(hub, c, &name, &select)
        })
    }

    fn promote<'a>(&'a self, name: &'a str, id: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let (name, id) = (name.to_owned(), id.to_owned());
        self.call(move |_, c, now| {
            let tx = crate::store::tx(c)?;
            if let Err(e) = live(&tx, &name, now)? {
                return Ok(Err(e));
            }
            let Some(letters) = tx.query_row("SELECT stages FROM seam_secret_versions WHERE name = ?1 AND id = ?2", params![name, id], |r| r.get::<_, String>(0)).optional()? else {
                return Ok(Err(Refusal::NoSuchVersion { name }));
            };
            if letters.contains('C') {
                return Ok(Ok(()));
            }
            tx.execute("UPDATE seam_secret_versions SET stages = replace(stages, 'P', '') WHERE name = ?1", [&name])?;
            tx.execute("UPDATE seam_secret_versions SET stages = replace(stages, 'C', 'P') WHERE name = ?1 AND id != ?2", params![name, id])?;
            tx.execute("UPDATE seam_secret_versions SET stages = replace(stages, 'N', '') || 'C' WHERE name = ?1 AND id = ?2", params![name, id])?;
            tx.commit()?;
            Ok(Ok(()))
        })
    }

    fn describe<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Described, Refusal>> {
        let name = name.to_owned();
        self.call(move |_, c, now| {
            purge(c, &name, now)?;
            let Some(purge_at_ms) = c.query_row("SELECT purge_at_ms FROM seam_secrets WHERE name = ?1", [&name], |r| r.get::<_, Option<i64>>(0)).optional()? else {
                return Ok(Err(Refusal::NotFound(name)));
            };
            let mut q = c.prepare("SELECT id, stages, created_ms FROM seam_secret_versions WHERE name = ?1 ORDER BY seq")?;
            let versions = q
                .query_map([&name], |r| Ok(Version { id: r.get(0)?, stages: stages(&r.get::<_, String>(1)?), created_ms: r.get(2)? }))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Ok(Described { name, versions, purge_at_ms }))
        })
    }

    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Vec<String>, Refusal>> {
        if limit == 0 {
            return Box::pin(async { Err(Refusal::Invalid { what: "a listing's limit", why: "it's 0".into() }) });
        }
        let (prefix, after) = (prefix.to_owned(), after.map(str::to_owned));
        let take = i64::try_from(limit.min(LIST_MAX)).unwrap_or(i64::MAX);
        self.call(move |_, c, now| {
            // Names in byte order, as a BTreeMap's: those with the prefix lie together.
            let (from, past) = match after {
                Some(a) if a >= prefix => (a, true),
                _ => (prefix.clone(), false),
            };
            let mut q = c.prepare(
                "SELECT name FROM seam_secrets WHERE (name > ?1 OR (name = ?1 AND NOT ?2)) AND substr(name, 1, length(?3)) = ?3
                 AND (purge_at_ms IS NULL OR ?4 < purge_at_ms) ORDER BY name LIMIT ?5",
            )?;
            let names = q.query_map(params![from, past, prefix, now, take], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(Ok(names))
        })
    }

    fn delete<'a>(&'a self, name: &'a str, window: Duration) -> BoxFuture<'a, Result<i64, Refusal>> {
        if window < WINDOW_MIN || window > WINDOW_MAX {
            return Box::pin(async move { Err(Refusal::Invalid { what: "a delete's window", why: format!("{window:?} (7 to 30 days)") }) });
        }
        let name = name.to_owned();
        self.call(move |_, c, now| {
            if let Err(e) = live(c, &name, now)? {
                return Ok(Err(e));
            }
            let purge_at_ms = now.saturating_add(ms(window));
            c.execute("UPDATE seam_secrets SET purge_at_ms = ?2 WHERE name = ?1", params![name, purge_at_ms])?;
            Ok(Ok(purge_at_ms))
        })
    }

    fn restore<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let name = name.to_owned();
        self.call(move |_, c, now| {
            purge(c, &name, now)?;
            if c.execute("UPDATE seam_secrets SET purge_at_ms = NULL WHERE name = ?1", [&name])? == 0 {
                return Ok(Err(Refusal::NotFound(name)));
            }
            Ok(Ok(()))
        })
    }

    fn lease<'a>(&'a self, name: &'a str, ttl: Duration) -> BoxFuture<'a, Result<Lease, Refusal>> {
        if let Err(e) = ttl_ok(ttl) {
            return Box::pin(async move { Err(e) });
        }
        let name = name.to_owned();
        self.call(move |_, c, now| {
            c.execute("DELETE FROM seam_leases WHERE expires_ms <= ?1", [now])?;
            if let Err(e) = live(c, &name, now)? {
                return Ok(Err(e));
            }
            let lease = Lease { id: crate::util::new_id(), name, expires_ms: now.saturating_add(ms(ttl)) };
            c.execute("INSERT INTO seam_leases (id, name, expires_ms) VALUES (?1, ?2, ?3)", params![lease.id, lease.name, lease.expires_ms])?;
            Ok(Ok(lease))
        })
    }

    fn read_leased<'a>(&'a self, lease: &'a str) -> BoxFuture<'a, Result<SecretValue, Refusal>> {
        let lease = lease.to_owned();
        self.call(move |hub, c, now| {
            let l = match lease_of(c, &lease, now)? {
                Ok(l) => l,
                Err(e) => return Ok(Err(e)),
            };
            if let Err(e) = live(c, &l.name, now)? {
                return Ok(Err(e));
            }
            Ok(version(hub, c, &l.name, &Select::Stage(Stage::Current))?.map(|(_, value)| value))
        })
    }

    fn renew<'a>(&'a self, lease: &'a str, ttl: Duration) -> BoxFuture<'a, Result<Lease, Refusal>> {
        if let Err(e) = ttl_ok(ttl) {
            return Box::pin(async move { Err(e) });
        }
        let lease = lease.to_owned();
        self.call(move |_, c, now| {
            let mut l = match lease_of(c, &lease, now)? {
                Ok(l) => l,
                Err(e) => return Ok(Err(e)),
            };
            l.expires_ms = now.saturating_add(ms(ttl));
            c.execute("UPDATE seam_leases SET expires_ms = ?2 WHERE id = ?1", params![l.id, l.expires_ms])?;
            Ok(Ok(l))
        })
    }

    fn revoke<'a>(&'a self, lease: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let lease = lease.to_owned();
        self.call(move |_, c, _| {
            c.execute("DELETE FROM seam_leases WHERE id = ?1", [&lease])?;
            Ok(Ok(()))
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::VecDeque;

    use kiln_seams::clock::SimClock;
    use kiln_seams::secrets::{SecretsFixture, cases};

    use super::*;

    pub(crate) struct Fixture {
        pub(crate) secrets: KilnSecrets,
        pub(crate) clock: SimClock,
    }

    impl Fixture {
        /// A store of its own, on a clock tests move.
        pub(crate) async fn new() -> anyhow::Result<Fixture> {
            Ok(Fixture::on(crate::hub::tests::scratch_hub().await?))
        }

        fn on(hub: Arc<Hub>) -> Fixture {
            let clock = SimClock::new();
            Fixture { secrets: KilnSecrets::new(hub, Arc::new(clock.clone())), clock }
        }
    }

    impl SecretsFixture for Fixture {
        fn secrets(&self) -> &dyn Secrets {
            &self.secrets
        }
        fn pass(&self, d: Duration) -> BoxFuture<'_, ()> {
            self.clock.advance(d);
            Box::pin(async {})
        }
    }

    /// kiln's store passes the `Secrets` suite, every clause, as `SimSecrets` does: swapping one
    /// for the other changes no other code (CP-T8.3; Shards plan step 33, step 5).
    #[test]
    fn cp_t8_3_kiln_s_store_passes_the_secrets_suite() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let cases = cases::<Fixture>();
            // A store of its own for each case, opened first.
            let mut hubs = VecDeque::new();
            for _ in 0..cases.len() {
                hubs.push_back(crate::hub::tests::scratch_hub().await?);
            }
            let last = hubs.back().cloned().ok_or_else(|| anyhow::anyhow!("no cases"))?;
            let make = || {
                let hub = hubs.pop_front().unwrap_or_else(|| Arc::clone(&last));
                async move { Fixture::on(hub) }
            };
            let clock = crate::rt::clock();
            let n = kiln_seams::conformance::run_within("Secrets (kiln's store)", &cases, make, || clock.sleep(Duration::from_secs(30))).await.into_result().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(n == cases.len(), "{n} of {} clauses held", cases.len());
            Ok(())
        })
    }

    /// A value is sealed in the store: no row holds its bytes, and its ciphertext opens only as
    /// the row it was written to.
    #[test]
    fn cp_t8_3_values_are_sealed_at_rest() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = Fixture::new().await?;
            let s = &f.secrets;
            s.create("runs/claude", SecretValue::new(b"sk-ant-oat01-plaintext".to_vec())).await.map_err(anyhow::Error::msg)?;
            let rows: Vec<Vec<u8>> = s
                .hub
                .store
                .call(|c| {
                    let mut q = c.prepare("SELECT ciphertext FROM seam_secret_versions")?;
                    let rows = q.query_map([], |r| r.get::<_, Vec<u8>>(0))?.collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                })
                .await?;
            anyhow::ensure!(rows.len() == 1, "{} rows", rows.len());
            anyhow::ensure!(!rows.iter().any(|r| r.windows(9).any(|w| w == b"plaintext")), "a value is kept as it was given");
            // Moved to another name, its row's ciphertext no longer opens.
            s.hub.store.call(|c| Ok(c.execute("UPDATE seam_secret_versions SET name = 'runs/other'", [])?)).await?;
            s.hub.store.call(|c| Ok(c.execute("INSERT INTO seam_secrets (name) VALUES ('runs/other')", [])?)).await?;
            let moved = s.get("runs/other", Select::Stage(Stage::Current)).await;
            anyhow::ensure!(matches!(moved, Err(Refusal::Unavailable(_))), "a moved ciphertext opened: {moved:?}");
            Ok(())
        })
    }
}
