//! A run's credential leases as Hex asks them for the Shards image VMs it boots (Shards' D242; kiln
//! plan 01a1d2b88be57304b972761e2a5c6781 step 6), answered from the run-lease [`Broker`]: [`Lessors`]
//! makes a [`RunLessor`] for each run, which leases a credential's value to that run alone, just in
//! time and by reference, gives each lease back as the run lets go of it, and ends the run's leases
//! with the run.
//!
//! - **The run's identity** is the SHA-256 of the run's own key where it binds an agent-control
//!   endpoint (D252: told with each ask, the same for all of them), else a name made for the run as
//!   its lessor is: 256 random bits, the form the broker keeps a key's SHA-256 in, which names no
//!   other run. It is the run's channel, as D242 names a run without a key: it proves nothing a
//!   channel doesn't, and nothing else holds it. An ask under a second key is refused.
//! - **What it answers** is D242's: the value, under the store's name for its lease, and how long it
//!   lives (none: until its run ends), or a refusal of D242's kinds, in the broker's words, which
//!   never hold a value.
//! - Hex asks on threads of its own, off kiln's runtime, so each call blocks on the broker there
//!   ([`crate::rt::block_on`]).
//!
//! These are what implement Hex's lease seam once kiln vendors the Hex that has it, its `Lease` and
//! `LeaseRefused` in place of the ones here; that wiring, and the sandbox that gives Hex these
//! lessors as it boots a harness's microVM, are owed (docs/research/s33-veil-creds/STEP6.md in
//! Shards).

use std::sync::Arc;
use std::time::Duration;

use kiln_seams::Clock;
use kiln_seams::secrets::{Refusal, SecretValue};
use parking_lot::Mutex;

use super::broker::{Broker, Refused};

/// A credential's value leased to a run (D242): the store's name for the lease, by which the run
/// gives it back ([`RunLessor::release`]), the value, and how long it lives from now; none where its
/// provider gives it no end, so that it lives until its run ends.
#[derive(Debug)]
pub struct Lease {
    pub lease: String,
    pub value: SecretValue,
    pub life: Option<Duration>,
}

/// Why a lease is refused, by D242's kinds (Shards' `LeaseRefusal`): callers match these, and each
/// carries the broker's words, which never hold a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseRefused {
    /// The broker leases the run none of it: a credential its store doesn't hold, or can't name.
    #[error("{0}")]
    Denied(String),
    /// Asked under the key the run had before a move replaced it (D176 part 6): that key, and the
    /// run's key now.
    #[error("{asked} is the key its run had before it moved, which no lease is served under: its key is {}", current.as_deref().unwrap_or("none"))]
    KeyReplaced { asked: String, current: Option<String> },
    /// The broker couldn't be asked, or failed, in its words: a later ask may be leased.
    #[error("{0}")]
    Unavailable(String),
    /// The run has ended: nothing is leased to it after.
    #[error("the run has ended: nothing is leased to it after")]
    Ended,
}

/// The broker's refusal as D242 kinds it: a run ended or moved as such, a credential the store
/// holds none of (or can't name) as denied, and every other, a value past its own end among them
/// (its sign-in refreshes it), as unavailable for now.
fn refused(why: Refused) -> LeaseRefused {
    match why {
        Refused::RunEnded(_) => LeaseRefused::Ended,
        Refused::KeyReplaced { asked, current } => LeaseRefused::KeyReplaced { asked, current: Some(current) },
        Refused::Store(e @ (Refusal::NotFound(_) | Refusal::Invalid { .. })) => LeaseRefused::Denied(e.to_string()),
        other => LeaseRefused::Unavailable(other.to_string()),
    }
}

/// The lessors of a machine's runs: one for each run, over the broker.
pub struct Lessors {
    broker: Arc<Broker>,
    clock: Arc<dyn Clock>,
}

impl Lessors {
    /// Over `broker`, with the clock it reads its values' ends on.
    pub fn new(broker: Arc<Broker>, clock: Arc<dyn Clock>) -> Lessors {
        Lessors { broker, clock }
    }

    /// The lessor of one run, `run` as Hex names it (its box's id, for words alone), with a channel
    /// of its own made now.
    pub fn lessor(&self, run: &str) -> anyhow::Result<RunLessor> {
        let channel = hex::encode(super::kiln::random::<32>()?);
        Ok(RunLessor { broker: Arc::clone(&self.broker), clock: Arc::clone(&self.clock), run: run.to_owned(), channel, key: Mutex::new(None) })
    }
}

/// One run's lessor: its leases issued to the run's own key where it has one, else to its channel.
pub struct RunLessor {
    broker: Arc<Broker>,
    clock: Arc<dyn Clock>,
    run: String,
    channel: String,
    /// The SHA-256 of the run's key, once an ask has told it (D252), in the broker's form.
    key: Mutex<Option<String>>,
}

impl RunLessor {
    /// The identity the run's leases are issued to: its key where `key` tells one (the first ask's,
    /// kept, and every later ask's the same), else its channel.
    fn identity(&self, key: Option<[u8; 32]>) -> Result<String, LeaseRefused> {
        let Some(key) = key else {
            return Ok(self.key.lock().clone().unwrap_or_else(|| self.channel.clone()));
        };
        let told = hex::encode(key);
        let mut kept = self.key.lock();
        match kept.as_deref() {
            None => {
                *kept = Some(told.clone());
                Ok(told)
            }
            Some(k) if k == told => Ok(told),
            Some(k) => Err(LeaseRefused::Unavailable(format!("run {}: asked under key {told}, where it was told key {k}: a run binds one key (D252)", self.run))),
        }
    }

    /// Credential `reference`'s value, leased to the run now, or why not, typed.
    pub fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused> {
        let identity = self.identity(key)?;
        let (broker, reference) = (Arc::clone(&self.broker), reference.to_owned());
        let got = crate::rt::block_on(async move { Ok(broker.lease(&identity, &reference).await) }).map_err(|e| LeaseRefused::Unavailable(format!("run {}: the broker: {e:#}", self.run)))?;
        let leased = got.map_err(refused)?;
        // Its end, from now: none of it left where it has passed, which its run then has lapsed.
        let life = leased.until_ms.map(|end| Duration::from_millis(u64::try_from(end.saturating_sub(self.clock.unix_ms())).unwrap_or(0)));
        Ok(Lease { lease: leased.lease, value: leased.value, life })
    }

    /// Lease `lease` given back: the run let go of it, or ended. A give-back the broker couldn't
    /// make is said, never dropped unsaid: the run's end revokes what it holds still ([`Self::ended`]).
    pub fn release(&self, lease: &str) {
        let identity = self.key.lock().clone().unwrap_or_else(|| self.channel.clone());
        let (broker, id) = (Arc::clone(&self.broker), lease.to_owned());
        match crate::rt::block_on(async move { Ok(broker.release_lease(&identity, &id).await) }) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::error!(run = %self.run, lease, "a run's credential lease wasn't given back: {e}"),
            Err(e) => tracing::error!(run = %self.run, lease, "a run's credential lease wasn't given back: the broker: {e:#}"),
        }
    }

    /// The run has ended, as its launch saw it end: kept so in the broker's ledger, then every lease
    /// held for it revoked, and nothing more leased to it (the broker's [`Broker::release`]), under
    /// its channel and its key alike. How many leases that revoked.
    pub fn ended(&self) -> anyhow::Result<usize> {
        let mut names = vec![self.channel.clone()];
        names.extend(self.key.lock().clone());
        let broker = Arc::clone(&self.broker);
        crate::rt::block_on(async move {
            let mut revoked = 0usize;
            for name in names {
                revoked = revoked.saturating_add(broker.release(&name).await.map_err(anyhow::Error::msg)?);
            }
            Ok(revoked)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::broker::tests::{Under, unders};
    use super::*;

    fn v(s: &str) -> SecretValue {
        SecretValue::new(s.as_bytes().to_vec())
    }

    /// The stores under test, each with its broker and its lessors, made on the runtime; the test
    /// asks them from its own thread, as Hex does from its.
    fn stores() -> anyhow::Result<Vec<(Under, Arc<Broker>, Lessors)>> {
        crate::rt::block_on(async {
            let mut made = Vec::new();
            for u in unders().await? {
                let broker = Arc::new(u.broker().await?);
                let lessors = Lessors::new(Arc::clone(&broker), Arc::new(u.clock.clone()));
                made.push((u, broker, lessors));
            }
            Ok(made)
        })
    }

    fn hold(u: &Under, name: &'static str, value: &'static str) -> anyhow::Result<()> {
        let secrets = Arc::clone(&u.secrets);
        crate::rt::block_on(async move { secrets.create(name, v(value)).await.map(|_| ()).map_err(anyhow::Error::msg) })
    }

    /// Whether the store reads `lease` still.
    fn reads(u: &Under, lease: &str) -> anyhow::Result<bool> {
        let (secrets, lease) = (Arc::clone(&u.secrets), lease.to_owned());
        crate::rt::block_on(async move { Ok(secrets.read_leased(&lease).await.is_ok()) })
    }

    /// CP-T8.3 (D242): a run's lessor leases its credential's value from the broker by reference,
    /// living until the run ends where its provider gives it no end; each lease it gives back is
    /// revoked alone, the run's others and another run's held on; and the run's end revokes the rest
    /// and leases nothing more to it, though another run leases on.
    #[test]
    fn cp_t8_3_a_runs_lessor_leases_by_reference_and_gives_each_lease_back() -> anyhow::Result<()> {
        for (u, _, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/anthropic", "sk-ant-api03-real")?;
            let (run, other) = (lessors.lessor("box-a")?, lessors.lessor("box-b")?);
            let one = run.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(one.value.expose() == b"sk-ant-api03-real", "{store}: the value");
            anyhow::ensure!(one.life.is_none(), "{store}: a value with no end of its own lives until its run ends: {:?}", one.life);
            anyhow::ensure!(!format!("{one:?}").contains("real"), "{store}: a lease shows its value");
            let two = run.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            let theirs = other.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            run.release(&one.lease);
            anyhow::ensure!(!reads(&u, &one.lease)?, "{store}: a lease given back still reads");
            anyhow::ensure!(reads(&u, &two.lease)? && reads(&u, &theirs.lease)?, "{store}: giving one lease back ended another");
            other.release(&two.lease);
            anyhow::ensure!(reads(&u, &two.lease)?, "{store}: another run gave this run's lease back");
            anyhow::ensure!(run.ended()? == 1, "{store}: the run's end didn't revoke the lease it held");
            anyhow::ensure!(!reads(&u, &two.lease)?, "{store}: the run's lease outlived its end");
            anyhow::ensure!(run.lease("providers/anthropic", None).is_err_and(|e| e == LeaseRefused::Ended), "{store}: a lease after the run's end");
            anyhow::ensure!(other.lease("providers/anthropic", None).is_ok(), "{store}: one run's end ended another's leases");
        }
        Ok(())
    }

    /// CP-T8.3 (D242): each refusal is D242's kind, in the broker's words: a credential the store
    /// holds none of, or can't name, denied; an ask under a key a move replaced, the run's key now;
    /// a run's second key unavailable; a value past its own end unavailable until its sign-in
    /// refreshes it, and one with an end lives until then.
    #[test]
    fn cp_t8_3_a_lessor_refuses_by_d242s_kinds() -> anyhow::Result<()> {
        const A: [u8; 32] = [0xa1; 32];
        const B: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
        for (u, broker, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/openai", "sk-proj-real")?;
            let run = lessors.lessor("box-a")?;
            anyhow::ensure!(matches!(run.lease("providers/none", None), Err(LeaseRefused::Denied(_))), "{store}: a credential the store holds none of");
            anyhow::ensure!(matches!(run.lease("not a name", None), Err(LeaseRefused::Denied(_))), "{store}: a reference no store names");
            let keyed = lessors.lessor("box-k")?;
            keyed.lease("providers/openai", Some(A)).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(matches!(keyed.lease("providers/openai", Some([0xc3; 32])), Err(LeaseRefused::Unavailable(_))), "{store}: a second key");
            let moved = Arc::clone(&broker);
            crate::rt::block_on(async move { moved.moved(&hex::encode(A), B).await.map_err(anyhow::Error::msg) })?;
            let after = keyed.lease("providers/openai", Some(A)).err();
            anyhow::ensure!(after == Some(LeaseRefused::KeyReplaced { asked: hex::encode(A), current: Some(B.to_owned()) }), "{store}: an ask under a replaced key: {after:?}");
            let b = Arc::clone(&broker);
            let captured = crate::rt::block_on(async move { b.capture("claude/ada", super::super::broker::Tokens { access: v("oat-real"), refresh: None, expires_in_s: Some(600) }).await.map_err(anyhow::Error::msg) })?;
            let short = run.lease(&captured.access.reference, None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(short.life == Some(Duration::from_secs(600)), "{store}: its life {:?}", short.life);
            u.clock.advance(Duration::from_secs(601));
            anyhow::ensure!(matches!(run.lease(&captured.access.reference, None), Err(LeaseRefused::Unavailable(_))), "{store}: a value past its own end");
        }
        Ok(())
    }
}
