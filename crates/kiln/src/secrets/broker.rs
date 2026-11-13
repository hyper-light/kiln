//! The run-lease broker (CP-T8.3; Shards plan step 33's step 5, D242): what an orchestrator
//! answers a run's credential asks from. A value is leased to the run's identity (the SHA-256 of
//! its microVM's own key, D238) until its provider says it ends, else until its run ends; it is
//! read through a lease of the store's, which the broker holds for the run and revokes as the run
//! ends or moves. Nothing is leased under a key whose run ended or moved. A sign-in's tokens are
//! kept here, outside the microVM, which is given placeholders; a refresh is answered here, one at
//! a time for each sign-in, since a rotated refresh token retires every other holder's (RFC 6749
//! §6).
//!
//! Its ledger (each run's end, the key a move replaced, each lease held until its revocation went
//! through, each value's end) is kept in kiln's store, so a restart forgets none of it: the run's
//! end is kept before its leases are revoked, and [`Broker::open`] revokes what a restart left.
//! Values go through kiln-seams' [`Secrets`] alone, so `SimSecrets` and
//! [`super::seam::KilnSecrets`] (and Veil's, CP-T8.1) serve it with no other change.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use kiln_seams::Clock;
use kiln_seams::secrets::{LEASE_MAX, Refusal, SecretValue, Secrets, Stage};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::store::Store;

/// Why an ask is refused (R04: callers match these; words are presentation's).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// A run's identity that is not a key's SHA-256 in lowercase hex.
    #[error("{0:?} is not a key's SHA-256 in lowercase hex")]
    Identity(String),
    /// Asked under the key of a run that has ended: nothing more is leased to it.
    #[error("the run of key {0} has ended: nothing more is leased to it")]
    RunEnded(String),
    /// Asked under the key a move replaced, which no lease is served under (D176 part 6,
    /// Shards' `Refused::KeyReplaced`): the key the run has now.
    #[error("{asked} is the key its run had before it moved, which no lease is served under: its key is {current}")]
    KeyReplaced { asked: String, current: String },
    /// A value its provider said ends, past its end: its sign-in refreshes it.
    #[error("the value of {0:?} has ended, as its provider said")]
    Ended(String),
    /// A sign-in nothing was captured for.
    #[error("no sign-in {0:?} was captured")]
    NoSignIn(String),
    /// Its provider refused or failed the refresh, in its words.
    #[error("refreshing sign-in {sign_in:?}: {why}")]
    Upstream { sign_in: String, why: String },
    /// The store refused.
    #[error(transparent)]
    Store(#[from] Refusal),
    /// The ledger could not be read or kept.
    #[error("the run-lease ledger: {0}")]
    Ledger(String),
}

/// A value leased to a run: its lease's id in the store, by which it is given back alone
/// ([`Broker::release_lease`]), the value, and when its lease ends (Unix ms), past which the run's
/// egress holds it no longer and asks again; none where its provider gives it no end, so that
/// it is the run's until the run ends.
#[derive(Debug)]
pub struct Leased {
    pub lease: String,
    pub value: SecretValue,
    pub until_ms: Option<i64>,
}

/// A token response as a provider gives one (RFC 6749 §5.1): the tokens, and the access
/// token's life in seconds where it says.
#[derive(Debug)]
pub struct Tokens {
    pub access: SecretValue,
    pub refresh: Option<SecretValue>,
    pub expires_in_s: Option<u64>,
}

/// A captured token as the microVM is given it: its placeholder, and the reference the value
/// is leased by (D242's grants name it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub placeholder: String,
    pub reference: String,
}

/// What a capture or a refresh gives the microVM: placeholders alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub access: Placed,
    pub refresh: Option<Placed>,
    /// When the access token ends (Unix ms), where its provider says.
    pub expires_ms: Option<i64>,
}

/// A provider's refresh: given the refresh token, the new tokens (RFC 6749 §6).
pub type Upstream = Box<dyn FnOnce(SecretValue) -> BoxFuture<'static, Result<Tokens, String>> + Send>;

/// A run as the ledger keeps it (R04): running, or ended, or moved to another key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Run {
    Running,
    Ended,
    Replaced(String),
}

/// What marks a placeholder, before its random part: visible, as Codex's egress broker marks its
/// own (D242: over 128 bits of randomness after a visible marker).
const MARKER: &str = "kiln-placeholder-";

/// A placeholder's random bytes: 256 bits, the security level of the AES-256-GCM suite every
/// link here holds to (D239), above the 128 D242 asks.
const RANDOM: usize = 32;

/// A sign-in's refreshes: one under way at a time, and how many were made, read without waiting
/// on the one under way.
#[derive(Default)]
struct Turn {
    one: futures::lock::Mutex<()>,
    made: AtomicU64,
}

pub struct Broker {
    secrets: Arc<dyn Secrets>,
    ledger: Store,
    clock: Arc<dyn Clock>,
    /// The last capture of each sign-in, for a refresh that waited on another's to take.
    captured: Mutex<HashMap<String, Captured>>,
    refreshing: Mutex<HashMap<String, Arc<Turn>>>,
}

/// Whether `key` names a run's identity: a SHA-256 in lowercase hex (D238, Shards' managed metadata).
fn identity(key: &str) -> Result<(), Refused> {
    if key.len() == 64 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Ok(());
    }
    Err(Refused::Identity(key.to_owned()))
}

fn placeholder() -> Result<String, Refused> {
    let bytes = super::kiln::random::<RANDOM>().map_err(|e| Refused::Ledger(format!("{e:#}")))?;
    Ok(format!("{MARKER}{}", hex::encode(bytes)))
}

/// The run of `key` as the ledger keeps it; one it has no row of has not ended.
fn run_of(c: &Connection, key: &str) -> rusqlite::Result<Run> {
    let row: Option<(Option<i64>, Option<String>)> = c.query_row("SELECT ended_ms, replaced_by FROM broker_runs WHERE key = ?1", [key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    Ok(match row {
        Some((_, Some(current))) => Run::Replaced(current),
        Some((Some(_), None)) => Run::Ended,
        _ => Run::Running,
    })
}

fn refused(key: &str, run: Run) -> Option<Refused> {
    match run {
        Run::Running => None,
        Run::Ended => Some(Refused::RunEnded(key.to_owned())),
        Run::Replaced(current) => Some(Refused::KeyReplaced { asked: key.to_owned(), current }),
    }
}

impl Broker {
    /// The broker over `secrets`, its ledger in `ledger`: as it opens, every lease held for a run
    /// that ended or moved and not yet revoked is revoked, so a restart between a run's end and
    /// its leases' revocation leaves none live.
    pub async fn open(secrets: Arc<dyn Secrets>, ledger: Store, clock: Arc<dyn Clock>) -> anyhow::Result<Broker> {
        let broker = Broker { secrets, ledger, clock, captured: Mutex::default(), refreshing: Mutex::default() };
        let left: Vec<String> = broker
            .ledger
            .call(|c| {
                let mut q = c.prepare(
                    "SELECT l.lease_id FROM broker_leases l JOIN broker_runs r ON r.key = l.key
                     WHERE l.revoked_ms IS NULL AND (r.ended_ms IS NOT NULL OR r.replaced_by IS NOT NULL)",
                )?;
                let ids = q.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
                Ok(ids)
            })
            .await?;
        let revoked = broker.revoke(left).await;
        if revoked > 0 {
            tracing::info!(revoked, "run-lease broker: leases of runs that ended before it opened, revoked");
        }
        Ok(broker)
    }

    /// One call of the ledger, its failure `Ledger`.
    async fn ledger<T: Send + 'static>(&self, f: impl FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static) -> Result<T, Refused> {
        self.ledger.call(f).await.map_err(|e| Refused::Ledger(format!("{e:#}")))
    }

    /// The value of `reference`, leased to the run whose key is `key`, just in time: until its
    /// provider says it ends, else until its run ends. Refused under a key whose run ended or
    /// moved, and for a value past its end.
    pub async fn lease(&self, key: &str, reference: &str) -> Result<Leased, Refused> {
        identity(key)?;
        let now = self.clock.unix_ms();
        let (k, r) = (key.to_owned(), reference.to_owned());
        let (run, end) = self.ledger(move |c| Ok((run_of(c, &k)?, c.query_row("SELECT end_ms FROM broker_ends WHERE reference = ?1", [&r], |row| row.get::<_, i64>(0)).optional()?))).await?;
        if let Some(no) = refused(key, run) {
            return Err(no);
        }
        // The store's lease is how the value is read and what is revoked at the run's end: as
        // long as the value lives, where the store lets a lease last so long (its LEASE_MAX).
        let (until_ms, ttl) = match end {
            Some(end) if end <= now => return Err(Refused::Ended(reference.to_owned())),
            Some(end) => (Some(end), Duration::from_millis(u64::try_from(end.saturating_sub(now)).unwrap_or(0)).min(LEASE_MAX)),
            None => (None, LEASE_MAX),
        };
        let lease = self.secrets.lease(reference, ttl).await?;
        // Kept before the value is read, and the run looked at again in the same call: a run that
        // ended meanwhile has this lease revoked now, never left live.
        let (k, r, id) = (key.to_owned(), reference.to_owned(), lease.id.clone());
        let since = self
            .ledger(move |c| {
                c.execute("INSERT INTO broker_leases (lease_id, key, reference) VALUES (?1, ?2, ?3)", params![id, k, r])?;
                Ok(run_of(c, &k)?)
            })
            .await;
        let since = match since {
            Ok(run) => run,
            Err(e) => {
                let _ = self.secrets.revoke(&lease.id).await;
                return Err(e);
            }
        };
        if let Some(no) = refused(key, since) {
            self.revoke(vec![lease.id]).await;
            return Err(no);
        }
        match self.secrets.read_leased(&lease.id).await {
            Ok(value) => Ok(Leased { lease: lease.id, value, until_ms }),
            Err(e) => {
                self.revoke(vec![lease.id]).await;
                Err(e.into())
            }
        }
    }

    /// The run whose key is `key` has ended, on whatever path: kept first, so nothing more is
    /// leased under its key, then every lease held for it revoked. How many.
    pub async fn release(&self, key: &str) -> Result<usize, Refused> {
        identity(key)?;
        let now = self.clock.unix_ms();
        let k = key.to_owned();
        let held = self
            .ledger(move |c| {
                c.execute("INSERT INTO broker_runs (key, ended_ms) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET ended_ms = COALESCE(ended_ms, ?2)", params![k, now])?;
                held_by(c, &k)
            })
            .await?;
        Ok(self.revoke(held).await)
    }

    /// One lease of the run whose key is `key` given back before the run's end (D242: one its run
    /// let go of, lapsed before it was passed on, or given after the run released the rest): revoked
    /// where the ledger holds it for that run and it is not yet revoked, the revocation kept. Whether
    /// it was revoked now: a lease the ledger holds for another run, or not at all, is not.
    pub async fn release_lease(&self, key: &str, lease: &str) -> Result<bool, Refused> {
        identity(key)?;
        let (k, id) = (key.to_owned(), lease.to_owned());
        let held = self
            .ledger(move |c| Ok(c.query_row("SELECT 1 FROM broker_leases WHERE lease_id = ?1 AND key = ?2 AND revoked_ms IS NULL", params![id, k], |_| Ok(())).optional()?.is_some()))
            .await?;
        Ok(held && self.revoke(vec![lease.to_owned()]).await == 1)
    }

    /// A run moved (D176 part 6): its key `replaced` gives way to `current`. Kept first, so an ask
    /// under the replaced key is refused from now on, with the run's new key; then every lease
    /// bound to it is revoked. Its first use under the new key is leased anew. How many revoked.
    pub async fn moved(&self, replaced: &str, current: &str) -> Result<usize, Refused> {
        identity(replaced)?;
        identity(current)?;
        let (k, by) = (replaced.to_owned(), current.to_owned());
        let held = self
            .ledger(move |c| {
                c.execute("INSERT INTO broker_runs (key, replaced_by) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET replaced_by = ?2", params![k, by])?;
                held_by(c, &k)
            })
            .await?;
        Ok(self.revoke(held).await)
    }

    /// Revokes each lease and keeps that it went through; one the store couldn't revoke stays to
    /// be revoked as the broker next opens. How many went through.
    async fn revoke(&self, leases: Vec<String>) -> usize {
        let mut revoked = 0;
        for id in leases {
            if self.secrets.revoke(&id).await.is_err() {
                continue;
            }
            let now = self.clock.unix_ms();
            let kept = self.ledger(move |c| Ok(c.execute("UPDATE broker_leases SET revoked_ms = ?2 WHERE lease_id = ?1", params![id, now])?)).await;
            if kept.is_ok() {
                revoked += 1;
            }
        }
        revoked
    }

    /// Keeps a sign-in's tokens (its harness's, captured outside the microVM) under references of
    /// its own, `sign-ins/<sign_in>/access` and `/refresh`, and gives back placeholders for them.
    pub async fn capture(&self, sign_in: &str, tokens: Tokens) -> Result<Captured, Refused> {
        let now = self.clock.unix_ms();
        let access = format!("sign-ins/{sign_in}/access");
        let refresh = format!("sign-ins/{sign_in}/refresh");
        self.keep(&access, tokens.access).await?;
        let has_refresh = tokens.refresh.is_some();
        if let Some(r) = tokens.refresh {
            self.keep(&refresh, r).await?;
        }
        let expires_ms = tokens.expires_in_s.map(|s| now.saturating_add(i64::try_from(s.saturating_mul(1000)).unwrap_or(i64::MAX)));
        let a = access.clone();
        self.ledger(move |c| {
            match expires_ms {
                Some(end) => c.execute("INSERT INTO broker_ends (reference, end_ms) VALUES (?1, ?2) ON CONFLICT (reference) DO UPDATE SET end_ms = ?2", params![a, end])?,
                None => c.execute("DELETE FROM broker_ends WHERE reference = ?1", [&a])?,
            };
            Ok(())
        })
        .await?;
        let captured = Captured {
            access: Placed { placeholder: placeholder()?, reference: access },
            refresh: if has_refresh { Some(Placed { placeholder: placeholder()?, reference: refresh }) } else { None },
            expires_ms,
        };
        self.captured.lock().insert(sign_in.to_owned(), captured.clone());
        Ok(captured)
    }

    /// A value made current under `name`, made if new.
    async fn keep(&self, name: &str, value: SecretValue) -> Result<(), Refused> {
        match self.secrets.put(name, value.clone(), Stage::Current).await {
            Ok(_) => Ok(()),
            Err(Refusal::NotFound(_)) => self.secrets.create(name, value).await.map(|_| ()).map_err(Refused::from),
            Err(e) => Err(e.into()),
        }
    }

    /// The current value under `name`, read through a lease of its own, revoked at once.
    async fn read(&self, name: &str) -> Result<SecretValue, Refusal> {
        let lease = self.secrets.lease(name, LEASE_MAX).await?;
        let value = self.secrets.read_leased(&lease.id).await;
        let _ = self.secrets.revoke(&lease.id).await;
        value
    }

    /// Answers a sign-in's refresh outside the microVM: one at a time for each sign-in, and a
    /// refresh asked while another was under way takes that one's tokens, never a second call to
    /// its provider, whose rotation would retire the tokens the first just kept (RFC 6749 §6).
    pub async fn refresh(&self, sign_in: &str, upstream: Upstream) -> Result<Captured, Refused> {
        let turn = Arc::clone(self.refreshing.lock().entry(sign_in.to_owned()).or_default());
        let seen = turn.made.load(Ordering::SeqCst);
        let _one = turn.one.lock().await;
        if turn.made.load(Ordering::SeqCst) != seen {
            return self.captured.lock().get(sign_in).cloned().ok_or_else(|| Refused::NoSignIn(sign_in.to_owned()));
        }
        let reference = format!("sign-ins/{sign_in}/refresh");
        let token = match self.read(&reference).await {
            Ok(t) => t,
            Err(Refusal::NotFound(_)) => return Err(Refused::NoSignIn(sign_in.to_owned())),
            Err(e) => return Err(e.into()),
        };
        let kept = token.clone();
        let mut tokens = upstream(token).await.map_err(|why| Refused::Upstream { sign_in: sign_in.to_owned(), why })?;
        // A provider that doesn't rotate keeps the refresh token it had (RFC 6749 §6).
        if tokens.refresh.is_none() {
            tokens.refresh = Some(kept);
        }
        let captured = self.capture(sign_in, tokens).await?;
        turn.made.fetch_add(1, Ordering::SeqCst);
        Ok(captured)
    }
}

/// The leases held for the run of `key` and not yet revoked.
fn held_by(c: &Connection, key: &str) -> anyhow::Result<Vec<String>> {
    let mut q = c.prepare("SELECT lease_id FROM broker_leases WHERE key = ?1 AND revoked_ms IS NULL")?;
    let ids = q.query_map([key], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::atomic::AtomicUsize;

    use kiln_seams::clock::SimClock;
    use kiln_seams::secrets::SimSecrets;

    use super::*;

    const A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
    const B: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

    fn v(s: &str) -> SecretValue {
        SecretValue::new(s.as_bytes().to_vec())
    }

    /// A store of values and a ledger, once with the simulator and once with kiln's store (step
    /// 5's acceptance: swapping SimSecrets for kiln's store changes no other code), and a clock.
    pub(in crate::secrets) struct Under {
        pub(in crate::secrets) name: &'static str,
        pub(in crate::secrets) secrets: Arc<dyn Secrets>,
        pub(in crate::secrets) ledger: Store,
        pub(in crate::secrets) clock: SimClock,
    }

    impl Under {
        pub(in crate::secrets) async fn broker(&self) -> anyhow::Result<Broker> {
            Broker::open(Arc::clone(&self.secrets), self.ledger.clone(), Arc::new(self.clock.clone())).await
        }

        /// The ledger's leases held for the run of `key`, revoked or not.
        async fn leases(&self, key: &'static str) -> anyhow::Result<Vec<String>> {
            self.ledger
                .call(move |c| {
                    let mut q = c.prepare("SELECT lease_id FROM broker_leases WHERE key = ?1")?;
                    let ids = q.query_map([key], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
                    Ok(ids)
                })
                .await
        }
    }

    pub(in crate::secrets) async fn unders() -> anyhow::Result<Vec<Under>> {
        let sim = SimClock::new();
        let kiln = super::super::seam::tests::Fixture::new().await?;
        Ok(vec![
            Under { name: "SimSecrets", secrets: Arc::new(SimSecrets::new(Arc::new(sim.clone()))), ledger: Store::in_memory()?, clock: sim },
            Under { name: "KilnSecrets", ledger: kiln.hub.store.clone(), clock: kiln.clock.clone(), secrets: Arc::new(kiln.secrets) },
        ])
    }

    /// A value leased to a run reads until the run ends: one its provider gives no end is the
    /// run's until then (no other bound), one it does no longer than that. Released, its lease
    /// reads no more, and nothing more is leased under its key.
    #[test]
    fn cp_t8_3_a_runs_leases_end_with_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for u in unders().await? {
                let (store, b) = (u.name, u.broker().await?);
                u.secrets.create("providers/anthropic", v("sk-ant-api03-real")).await.map_err(anyhow::Error::msg)?;
                let got = b.lease(A, "providers/anthropic").await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(got.value.expose() == b"sk-ant-api03-real", "{store}: the value");
                anyhow::ensure!(got.until_ms.is_none(), "{store}: a value with no end of its own ends before its run: {:?}", got.until_ms);
                let held = u.leases(A).await?;
                anyhow::ensure!(held.len() == 1, "{store}: {} leases held", held.len());
                anyhow::ensure!(b.release(A).await.map_err(anyhow::Error::msg)? == 1, "{store}: not one lease revoked");
                for id in &held {
                    let read = u.secrets.read_leased(id).await;
                    anyhow::ensure!(read == Err(Refusal::Expired), "{store}: a released lease still reads: {read:?}");
                }
                let after = b.lease(A, "providers/anthropic").await;
                anyhow::ensure!(after.as_ref().is_err_and(|e| *e == Refused::RunEnded(A.into())), "{store}: an ask after the run ended: {after:?}");
                // A token captured with its life is leased until then.
                b.capture("claude/ada", Tokens { access: v("oat-real"), refresh: None, expires_in_s: Some(600) }).await.map_err(anyhow::Error::msg)?;
                let short = b.lease(B, "sign-ins/claude/ada/access").await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(short.until_ms == Some(u.clock.unix_ms() + 600_000), "{store}: its end {:?}", short.until_ms);
                u.clock.advance(Duration::from_secs(601));
                anyhow::ensure!(b.lease(B, "sign-ins/claude/ada/access").await.is_err_and(|e| e == Refused::Ended("sign-ins/claude/ada/access".into())), "{store}: a value past its end leased");
                anyhow::ensure!(b.lease("not-a-key", "providers/anthropic").await.is_err_and(|e| matches!(e, Refused::Identity(_))), "{store}: a malformed identity leased");
            }
            Ok(())
        })
    }

    /// A lease given back alone (D242: lapsed, or given after its run released the rest) is revoked
    /// and kept so, the run's other leases and another run's held on; one the ledger holds for
    /// another run, or none, is given back by no one but its own run.
    #[test]
    fn cp_t8_3_a_lease_given_back_alone_is_revoked_alone() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for u in unders().await? {
                let (store, b) = (u.name, u.broker().await?);
                u.secrets.create("providers/anthropic", v("sk-ant-api03-real")).await.map_err(anyhow::Error::msg)?;
                let (one, two) = (b.lease(A, "providers/anthropic").await.map_err(anyhow::Error::msg)?, b.lease(A, "providers/anthropic").await.map_err(anyhow::Error::msg)?);
                let theirs = b.lease(B, "providers/anthropic").await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(!b.release_lease(B, &one.lease).await.map_err(anyhow::Error::msg)?, "{store}: another run gave back A's lease");
                anyhow::ensure!(!b.release_lease(A, "no-such-lease").await.map_err(anyhow::Error::msg)?, "{store}: a lease no run holds was given back");
                anyhow::ensure!(b.release_lease(A, &one.lease).await.map_err(anyhow::Error::msg)?, "{store}: A's own lease wasn't given back");
                anyhow::ensure!(u.secrets.read_leased(&one.lease).await == Err(Refusal::Expired), "{store}: a lease given back still reads");
                anyhow::ensure!(!b.release_lease(A, &one.lease).await.map_err(anyhow::Error::msg)?, "{store}: a lease given back twice");
                for held in [&two, &theirs] {
                    anyhow::ensure!(u.secrets.read_leased(&held.lease).await.is_ok(), "{store}: a lease not given back stopped reading");
                }
                anyhow::ensure!(b.lease(A, "providers/anthropic").await.is_ok(), "{store}: the run leases on after giving one back");
                anyhow::ensure!(b.release(A).await.map_err(anyhow::Error::msg)? == 2, "{store}: the run's end didn't revoke the two it held");
            }
            Ok(())
        })
    }

    /// A restart forgets nothing of the ledger. Restarted mid-run, the run's end revokes the lease
    /// taken before, and its key is refused after; a run that ended without its leases revoked
    /// (the broker gone between) has them revoked as the broker opens; a move kept before a
    /// restart is refused after it.
    #[test]
    fn cp_t8_3_a_restart_keeps_the_ledger() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for u in unders().await? {
                let store = u.name;
                u.secrets.create("providers/openai", v("sk-proj-real")).await.map_err(anyhow::Error::msg)?;
                let first = u.broker().await?;
                first.lease(A, "providers/openai").await.map_err(anyhow::Error::msg)?;
                first.lease(B, "providers/openai").await.map_err(anyhow::Error::msg)?;
                drop(first);
                let second = u.broker().await?;
                anyhow::ensure!(second.release(A).await.map_err(anyhow::Error::msg)? == 1, "{store}: the lease taken before the restart wasn't revoked");
                for id in u.leases(A).await? {
                    anyhow::ensure!(u.secrets.read_leased(&id).await == Err(Refusal::Expired), "{store}: a lease from before the restart still reads");
                }
                anyhow::ensure!(second.lease(A, "providers/openai").await.is_err_and(|e| e == Refused::RunEnded(A.into())), "{store}: an ask under an ended run's key after a restart");
                drop(second);
                // B's run ends with the broker gone before its leases were revoked.
                u.ledger.call(|c| Ok(c.execute("INSERT INTO broker_runs (key, ended_ms) VALUES (?1, 1)", [B])?)).await?;
                let held = u.leases(B).await?;
                anyhow::ensure!(held.len() == 1, "{store}: {} leases of B", held.len());
                let third = u.broker().await?;
                for id in &held {
                    anyhow::ensure!(u.secrets.read_leased(id).await == Err(Refusal::Expired), "{store}: an ended run's lease lived through the broker's opening");
                }
                third.moved("c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3", B).await.map_err(anyhow::Error::msg)?;
                drop(third);
                let fourth = u.broker().await?;
                let refused = fourth.lease("c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3", "providers/openai").await;
                anyhow::ensure!(matches!(&refused, Err(Refused::KeyReplaced { current, .. }) if current == B), "{store}: a move forgotten by a restart: {refused:?}");
            }
            Ok(())
        })
    }

    /// A move revokes every lease of the key it replaced, refuses an ask under that key with the
    /// run's new one, and leases anew under the new key.
    #[test]
    fn cp_t8_3_a_move_revokes_the_old_keys_leases_and_refuses_it_typed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for u in unders().await? {
                let (store, b) = (u.name, u.broker().await?);
                u.secrets.create("providers/openai", v("sk-proj-real")).await.map_err(anyhow::Error::msg)?;
                b.lease(A, "providers/openai").await.map_err(anyhow::Error::msg)?;
                let held = u.leases(A).await?;
                anyhow::ensure!(b.moved(A, B).await.map_err(anyhow::Error::msg)? == 1, "{store}: the old key's lease wasn't revoked");
                for id in &held {
                    anyhow::ensure!(u.secrets.read_leased(id).await == Err(Refusal::Expired), "{store}: the old key's lease still reads");
                }
                let refused = b.lease(A, "providers/openai").await;
                anyhow::ensure!(
                    matches!(&refused, Err(Refused::KeyReplaced { asked, current }) if asked == A && current == B),
                    "{store}: an ask under the replaced key: {refused:?}"
                );
                anyhow::ensure!(b.lease(B, "providers/openai").await.is_ok(), "{store}: the new key was refused");
            }
            Ok(())
        })
    }

    /// A captured sign-in's tokens are kept and placeholders given back: neither placeholder holds
    /// a token, each is marked and random, and the references read the real tokens.
    #[test]
    fn cp_t8_3_a_captured_sign_in_is_kept_and_placeholders_given() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for u in unders().await? {
                let (store, b) = (u.name, u.broker().await?);
                let c = b.capture("codex/ada", Tokens { access: v("eyJ-access-real"), refresh: Some(v("rt-real")), expires_in_s: Some(3600) }).await.map_err(anyhow::Error::msg)?;
                let refresh = c.refresh.clone().ok_or_else(|| anyhow::anyhow!("{store}: no refresh placeholder"))?;
                for p in [&c.access.placeholder, &refresh.placeholder] {
                    anyhow::ensure!(p.starts_with(MARKER) && p.len() == MARKER.len() + 2 * RANDOM, "{store}: {p}");
                    anyhow::ensure!(!p.contains("real"), "{store}: a placeholder holds a token");
                }
                anyhow::ensure!(c.access.placeholder != refresh.placeholder, "{store}: one placeholder for both");
                let access = b.lease(A, &c.access.reference).await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(access.value.expose() == b"eyJ-access-real", "{store}: the access token kept");
                let rt = b.lease(A, &refresh.reference).await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(rt.value.expose() == b"rt-real", "{store}: the refresh token kept");
            }
            Ok(())
        })
    }

    /// Two refreshes asked at once make one call to the provider; the second takes the first's
    /// tokens. The rotated refresh token is the one kept.
    #[test]
    fn cp_t8_3_a_refresh_is_single_flight() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for u in unders().await? {
                let (store, b) = (u.name, Arc::new(u.broker().await?));
                b.capture("claude/ada", Tokens { access: v("at-1"), refresh: Some(v("rt-1")), expires_in_s: Some(60) }).await.map_err(anyhow::Error::msg)?;
                let calls = Arc::new(AtomicUsize::new(0));
                let up = |calls: Arc<AtomicUsize>| -> Upstream {
                    Box::new(move |rt: SecretValue| -> BoxFuture<'static, Result<Tokens, String>> {
                        Box::pin(async move {
                            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                            // Long enough that the other refresh is asked meanwhile.
                            crate::rt::sleep(Duration::from_millis(50)).await;
                            if rt.expose() != format!("rt-{n}").as_bytes() {
                                return Err(format!("refresh token {} retired", String::from_utf8_lossy(rt.expose())));
                            }
                            Ok(Tokens { access: v(&format!("at-{}", n + 1)), refresh: Some(v(&format!("rt-{}", n + 1))), expires_in_s: Some(60) })
                        })
                    })
                };
                let (one, two) = futures::join!(b.refresh("claude/ada", up(Arc::clone(&calls))), b.refresh("claude/ada", up(Arc::clone(&calls))));
                let (one, two) = (one.map_err(anyhow::Error::msg)?, two.map_err(anyhow::Error::msg)?);
                anyhow::ensure!(calls.load(Ordering::SeqCst) == 1, "{store}: {} calls to the provider", calls.load(Ordering::SeqCst));
                anyhow::ensure!(one == two, "{store}: the second refresh didn't take the first's tokens");
                let rt = b.lease(A, "sign-ins/claude/ada/refresh").await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(rt.value.expose() == b"rt-2", "{store}: the rotated refresh token wasn't kept");
                // One asked after both gets one call of its own, with the rotated token.
                b.refresh("claude/ada", up(Arc::clone(&calls))).await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(calls.load(Ordering::SeqCst) == 2, "{store}: a later refresh made no call");
            }
            Ok(())
        })
    }
}
