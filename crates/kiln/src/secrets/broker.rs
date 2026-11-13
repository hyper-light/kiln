//! The run-lease broker (CP-T8.3; Shards plan step 33's step 5, D242): what an orchestrator
//! answers a run's credential asks from. A value is leased to the run's identity (the SHA-256 of
//! its microVM's own key, D238), read through the lease, held by the run's egress only while the
//! lease lives, and revoked when the run ends or moves. A sign-in's tokens are kept here, outside
//! the microVM, which is given placeholders; a refresh is answered here, one at a time for each
//! sign-in, since a rotated refresh token retires every other holder's (RFC 6749 §6).
//!
//! It works through kiln-seams' [`Secrets`] alone, so `SimSecrets` and [`super::seam::KilnSecrets`]
//! (and Veil's, CP-T8.1) serve it with no other change.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use kiln_seams::Clock;
use kiln_seams::secrets::{LEASE_MAX, Lease, Refusal, SecretValue, Secrets, Stage};
use parking_lot::Mutex;

/// Why an ask is refused (R04: callers match these; words are presentation's).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// A run's identity that is not a key's SHA-256 in lowercase hex.
    #[error("{0:?} is not a key's SHA-256 in lowercase hex")]
    Identity(String),
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
    /// A sign-in captured without a refresh token, which can't be refreshed.
    #[error("sign-in {0:?} has no refresh token")]
    NoRefresh(String),
    /// Its provider refused or failed the refresh, in its words.
    #[error("refreshing sign-in {sign_in:?}: {why}")]
    Upstream { sign_in: String, why: String },
    /// The store refused.
    #[error(transparent)]
    Store(#[from] Refusal),
}

/// A value leased to a run: the value, and when its lease ends (Unix ms), past which the run's
/// egress holds it no longer and asks again.
#[derive(Debug)]
pub struct Leased {
    pub value: SecretValue,
    pub until_ms: i64,
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

/// The lease of a run's, as the broker holds it (R04: held until released, revoked or ended).
#[derive(Debug, Clone)]
struct Held {
    lease: Lease,
    reference: String,
}

#[derive(Default)]
struct Ledger {
    /// Each run's leases, by its key.
    runs: HashMap<String, Vec<Held>>,
    /// Each key a move replaced, and the run's key since.
    replaced: HashMap<String, String>,
    /// Each sign-in's references and the access token's end, as captured or last refreshed.
    sign_ins: HashMap<String, Captured>,
    /// When each reference's value ends (Unix ms), where its provider said.
    ends: HashMap<String, i64>,
}

/// What marks a placeholder, before its random part: visible, as Codex's egress broker marks its
/// own (D242: over 128 bits of randomness after a visible marker).
const MARKER: &str = "kiln-placeholder-";

/// A placeholder's random bytes: 256 bits, the security level of the AES-256-GCM suite every
/// link here holds to (D239), above the 128 D242 asks.
const RANDOM: usize = 32;

pub struct Broker {
    secrets: Arc<dyn Secrets>,
    clock: Arc<dyn Clock>,
    ledger: Mutex<Ledger>,
    /// Each sign-in's refreshes, one at a time.
    refreshing: Mutex<HashMap<String, Arc<Turn>>>,
}

/// A sign-in's refreshes: one under way at a time, and how many were made, read without waiting
/// on the one under way.
#[derive(Default)]
struct Turn {
    one: futures::lock::Mutex<()>,
    made: AtomicU64,
}

/// Whether `key` names a run's identity: a SHA-256 in lowercase hex (D238, Shards' managed metadata).
fn identity(key: &str) -> Result<(), Refused> {
    if key.len() == 64 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Ok(());
    }
    Err(Refused::Identity(key.to_owned()))
}

fn placeholder() -> Result<String, Refused> {
    let bytes = super::kiln::random::<RANDOM>().map_err(|e| Refused::Store(Refusal::Unavailable(format!("{e:#}"))))?;
    Ok(format!("{MARKER}{}", hex::encode(bytes)))
}

impl Broker {
    pub fn new(secrets: Arc<dyn Secrets>, clock: Arc<dyn Clock>) -> Broker {
        Broker { secrets, clock, ledger: Mutex::default(), refreshing: Mutex::default() }
    }

    /// The value of `reference`, leased to the run whose key is `key`, just in time: for as long as
    /// the value lives where its provider says, else as long as a lease may (the store's
    /// `LEASE_MAX`), never past either. Asked under a key a move replaced, refused with the key the
    /// run has now.
    pub async fn lease(&self, key: &str, reference: &str) -> Result<Leased, Refused> {
        identity(key)?;
        let now = self.clock.unix_ms();
        let ends = {
            let l = self.ledger.lock();
            if let Some(current) = l.replaced.get(key) {
                return Err(Refused::KeyReplaced { asked: key.to_owned(), current: current.clone() });
            }
            l.ends.get(reference).copied()
        };
        let ttl = match ends {
            Some(end) if end <= now => return Err(Refused::Ended(reference.to_owned())),
            Some(end) => Duration::from_millis(u64::try_from(end.saturating_sub(now)).unwrap_or(0)).min(LEASE_MAX),
            None => LEASE_MAX,
        };
        let lease = self.secrets.lease(reference, ttl).await?;
        let value = match self.secrets.read_leased(&lease.id).await {
            Ok(v) => v,
            Err(e) => {
                let _ = self.secrets.revoke(&lease.id).await;
                return Err(e.into());
            }
        };
        let until_ms = lease.expires_ms;
        // A move between the ask and now leases nothing to the key it replaced.
        let moved = {
            let mut l = self.ledger.lock();
            match l.replaced.get(key).cloned() {
                Some(current) => Some(current),
                None => {
                    l.runs.entry(key.to_owned()).or_default().push(Held { lease: lease.clone(), reference: reference.to_owned() });
                    None
                }
            }
        };
        if let Some(current) = moved {
            let _ = self.secrets.revoke(&lease.id).await;
            return Err(Refused::KeyReplaced { asked: key.to_owned(), current });
        }
        Ok(Leased { value, until_ms })
    }

    /// Revokes every lease of the run whose key is `key`, as it ends on any path; how many.
    pub async fn release(&self, key: &str) -> usize {
        let held = self.ledger.lock().runs.remove(key).unwrap_or_default();
        self.revoke_all(&held).await
    }

    /// A run moved (D176 part 6): its key `replaced` gives way to `current`. Every lease bound to
    /// the replaced key is revoked, and an ask under it is refused from now on; the run's first
    /// use under its new key is leased anew. How many were revoked.
    pub async fn moved(&self, replaced: &str, current: &str) -> Result<usize, Refused> {
        identity(replaced)?;
        identity(current)?;
        let held = {
            let mut l = self.ledger.lock();
            l.replaced.insert(replaced.to_owned(), current.to_owned());
            l.runs.remove(replaced).unwrap_or_default()
        };
        Ok(self.revoke_all(&held).await)
    }

    async fn revoke_all(&self, held: &[Held]) -> usize {
        let mut revoked = 0;
        for h in held {
            if self.secrets.revoke(&h.lease.id).await.is_ok() {
                revoked += 1;
            }
            tracing::debug!(reference = %h.reference, lease = %h.lease.id, "credential lease revoked");
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
        let captured = Captured {
            access: Placed { placeholder: placeholder()?, reference: access.clone() },
            refresh: if has_refresh { Some(Placed { placeholder: placeholder()?, reference: refresh }) } else { None },
            expires_ms,
        };
        let mut l = self.ledger.lock();
        match expires_ms {
            Some(end) => l.ends.insert(access, end),
            None => l.ends.remove(&access),
        };
        l.sign_ins.insert(sign_in.to_owned(), captured.clone());
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

    /// Answers a sign-in's refresh outside the microVM: one at a time for each sign-in, and a
    /// refresh asked while another was under way takes that one's tokens, never a second call to
    /// its provider, whose rotation would retire the tokens the first just kept (RFC 6749 §6).
    pub async fn refresh(&self, sign_in: &str, upstream: Upstream) -> Result<Captured, Refused> {
        let turn = Arc::clone(self.refreshing.lock().entry(sign_in.to_owned()).or_default());
        let seen = turn.made.load(Ordering::SeqCst);
        let _one = turn.one.lock().await;
        if turn.made.load(Ordering::SeqCst) != seen {
            return self.ledger.lock().sign_ins.get(sign_in).cloned().ok_or_else(|| Refused::NoSignIn(sign_in.to_owned()));
        }
        let had = self.ledger.lock().sign_ins.get(sign_in).cloned().ok_or_else(|| Refused::NoSignIn(sign_in.to_owned()))?;
        let refresh = had.refresh.as_ref().ok_or_else(|| Refused::NoRefresh(sign_in.to_owned()))?;
        let lease = self.secrets.lease(&refresh.reference, LEASE_MAX).await?;
        let token = self.secrets.read_leased(&lease.id).await;
        let _ = self.secrets.revoke(&lease.id).await;
        let mut tokens = upstream(token?).await.map_err(|why| Refused::Upstream { sign_in: sign_in.to_owned(), why })?;
        // A provider that doesn't rotate keeps the refresh token it had (RFC 6749 §6).
        if tokens.refresh.is_none() {
            let lease = self.secrets.lease(&refresh.reference, LEASE_MAX).await?;
            tokens.refresh = self.secrets.read_leased(&lease.id).await.ok();
            let _ = self.secrets.revoke(&lease.id).await;
        }
        let captured = self.capture(sign_in, tokens).await?;
        turn.made.fetch_add(1, Ordering::SeqCst);
        Ok(captured)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use kiln_seams::clock::SimClock;
    use kiln_seams::secrets::SimSecrets;

    use super::*;

    const A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
    const B: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

    fn v(s: &str) -> SecretValue {
        SecretValue::new(s.as_bytes().to_vec())
    }

    /// Each test, once over the simulator and once over kiln's store: the same code (step 5's
    /// acceptance: swapping SimSecrets for kiln's store changes no other code).
    async fn stores() -> anyhow::Result<Vec<(&'static str, Arc<dyn Secrets>, SimClock)>> {
        let sim = SimClock::new();
        let kiln = super::super::seam::tests::Fixture::new().await?;
        let clock = kiln.clock.clone();
        Ok(vec![("SimSecrets", Arc::new(SimSecrets::new(Arc::new(sim.clone()))), sim), ("KilnSecrets", Arc::new(kiln.secrets), clock)])
    }

    /// A value leased to a run reads until the run ends; released, its lease reads no more; a
    /// value whose provider says when it ends is leased no longer than that.
    #[test]
    fn cp_t8_3_a_runs_leases_end_with_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for (store, secrets, clock) in stores().await? {
                let b = Broker::new(Arc::clone(&secrets), Arc::new(clock.clone()));
                secrets.create("providers/anthropic", v("sk-ant-api03-real")).await.map_err(anyhow::Error::msg)?;
                let got = b.lease(A, "providers/anthropic").await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(got.value.expose() == b"sk-ant-api03-real", "{store}: the value");
                anyhow::ensure!(got.until_ms <= clock.unix_ms() + i64::try_from(LEASE_MAX.as_millis())?, "{store}: past LEASE_MAX");
                let held = b.ledger.lock().runs.get(A).cloned().unwrap_or_default();
                anyhow::ensure!(held.len() == 1, "{store}: {} leases held", held.len());
                anyhow::ensure!(b.release(A).await == 1, "{store}: not one lease revoked");
                for h in &held {
                    let read = secrets.read_leased(&h.lease.id).await;
                    anyhow::ensure!(read == Err(Refusal::Expired), "{store}: a released lease still reads: {read:?}");
                }
                // A token captured with its life is leased no longer than it lives.
                b.capture("claude/ada", Tokens { access: v("oat-real"), refresh: None, expires_in_s: Some(600) }).await.map_err(anyhow::Error::msg)?;
                let short = b.lease(A, "sign-ins/claude/ada/access").await.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(short.until_ms <= clock.unix_ms() + 600_000, "{store}: leased past the token's life");
                anyhow::ensure!(b.lease("not-a-key", "providers/anthropic").await.is_err_and(|e| matches!(e, Refused::Identity(_))), "{store}: a malformed identity leased");
            }
            Ok(())
        })
    }

    /// A move revokes every lease of the key it replaced, refuses an ask under that key with the
    /// run's new one, and leases anew under the new key.
    #[test]
    fn cp_t8_3_a_move_revokes_the_old_keys_leases_and_refuses_it_typed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for (store, secrets, clock) in stores().await? {
                let b = Broker::new(Arc::clone(&secrets), Arc::new(clock.clone()));
                secrets.create("providers/openai", v("sk-proj-real")).await.map_err(anyhow::Error::msg)?;
                b.lease(A, "providers/openai").await.map_err(anyhow::Error::msg)?;
                let held = b.ledger.lock().runs.get(A).cloned().unwrap_or_default();
                anyhow::ensure!(b.moved(A, B).await.map_err(anyhow::Error::msg)? == 1, "{store}: the old key's lease wasn't revoked");
                for h in &held {
                    anyhow::ensure!(secrets.read_leased(&h.lease.id).await == Err(Refusal::Expired), "{store}: the old key's lease still reads");
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
            for (store, secrets, clock) in stores().await? {
                let b = Broker::new(Arc::clone(&secrets), Arc::new(clock.clone()));
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
            for (store, secrets, clock) in stores().await? {
                let b = Arc::new(Broker::new(Arc::clone(&secrets), Arc::new(clock.clone())));
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
