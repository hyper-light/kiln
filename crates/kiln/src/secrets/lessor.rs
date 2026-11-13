//! A run's credential leases as Hex asks them for the Shards image VMs it boots (Shards' D242; kiln
//! plan 01a1d2b88be57304b972761e2a5c6781 step 6), answered from the run-lease [`Broker`]: kiln's
//! [`Lessors`] are Hex's lease seam (`hex_api::lease`), through which its image class leases a run's
//! credentials (`ImageClass::with_lessors`). They make a lessor for each run as it boots, which
//! leases a credential's value to that run alone, just in time and by reference, gives each lease
//! back as the run lets go of it, and ends the run's leases with the run.
//!
//! - **The run's identity** is the SHA-256 of the run's own key where it binds an agent-control
//!   endpoint (D252: told with each ask, the same for all of them), else a name made for the run as
//!   its lessor is: 256 random bits, the form the broker keeps a key's SHA-256 in, which names no
//!   other run. It is the run's channel, as D242 names a run without a key: it proves nothing a
//!   channel doesn't, and nothing else holds it. An ask under a second key is refused.
//! - **What it answers** is D242's, in Hex's types (`hex_api::lease::Lease`, `LeaseRefused`): the
//!   value, under the store's name for its lease, and how long it lives (none: until its run ends),
//!   or a refusal of D242's kinds, in the broker's words, which never hold a value.
//! - **The run's end** is its lessor let go of. Hex's image class makes it as the run boots and the
//!   run's host holds it for as long as the run can ask (Shards' `RunLessor`), so once no one holds
//!   it, nothing asks through it again: the run is kept ended in the broker's ledger, so nothing more
//!   is leased under its identity, and every lease it holds still is revoked. Its host gives each
//!   lease back as the run ends; the end revokes what a give-back couldn't.
//! - Hex asks on threads of its own, off kiln's runtime, so each call blocks on the broker there
//!   ([`crate::rt::block_on`]); a lessor let go of on one of the runtime's shards, where nothing may
//!   block, ends its run on a task of its own.
//!
//! The server opens its broker over kiln's credential store and gives these lessors to its own runner
//! ([`open`]), whose sandbox hands them to Hex's image runtime and carries each launch's credential
//! grants into its image's, by reference (`sandbox::hexvm`, `hex_node::images::ImageRuntime::leasing`).

use std::sync::Arc;
use std::time::Duration;

use hex_api::BoxId;
use hex_api::lease::{Lease, LeaseRefused, Lessor, Value};
use kiln_seams::Clock;
use kiln_seams::secrets::Refusal;
use parking_lot::Mutex;

use super::broker::{Broker, Refused};

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

/// The lessors of the server's own runner's microVMs, over the run-lease broker [`Broker::open`] opens on kiln's
/// credential store (`KilnSecrets`) with its ledger in the server's store, on the process's clock: what its runner's
/// sandbox leases its VMs' credentials through (`sandbox::Leasing`). Opening it revokes what a restart left leased.
pub async fn open(hub: &Arc<crate::hub::Hub>) -> anyhow::Result<Lessors> {
    let secrets: Arc<dyn kiln_seams::secrets::Secrets> = Arc::new(super::seam::KilnSecrets::new(Arc::clone(hub), crate::rt::clock()));
    let broker = Broker::open(secrets, hub.store.clone(), crate::rt::clock()).await?;
    Ok(Lessors::new(Arc::new(broker), crate::rt::clock()))
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
}

impl hex_api::lease::Lessors for Lessors {
    /// The lessor of box `id`'s run, as the run boots (its id for words alone), with a channel of its
    /// own made now.
    fn lessor(&self, id: &BoxId) -> Result<Arc<dyn Lessor>, hex_api::Refusal> {
        let channel = super::kiln::random::<32>().map_err(|e| hex_api::Refusal::Os { op: "making a run's lease channel", why: format!("{e:#}") })?;
        Ok(Arc::new(RunLessor { broker: Arc::clone(&self.broker), clock: Arc::clone(&self.clock), run: id.as_str().to_owned(), channel: hex::encode(channel), key: Mutex::new(None) }))
    }
}

/// One run's lessor: its leases issued to the run's own key where it has one, else to its channel;
/// its run ended as it is let go of.
struct RunLessor {
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
}

impl Lessor for RunLessor {
    /// Credential `reference`'s value, leased to the run now, or why not, typed.
    fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused> {
        let identity = self.identity(key)?;
        let (broker, reference) = (Arc::clone(&self.broker), reference.to_owned());
        let got = crate::rt::block_on(async move { Ok(broker.lease(&identity, &reference).await) }).map_err(|e| LeaseRefused::Unavailable(format!("run {}: the broker: {e:#}", self.run)))?;
        let leased = got.map_err(refused)?;
        // Its end, from now: none of it left where it has passed, which its run then has lapsed.
        let life = leased.until_ms.map(|end| Duration::from_millis(u64::try_from(end.saturating_sub(self.clock.unix_ms())).unwrap_or(0)));
        // Its value handed on in Hex's holder, which wipes it as it drops, as the broker's own does.
        Ok(Lease { lease: leased.lease, value: Value::new(leased.value.expose().to_vec()), life })
    }

    /// Lease `lease` given back: the run let go of it, or ended. A give-back the broker couldn't
    /// make is said, never dropped unsaid: the run's end revokes what it holds still.
    fn release(&self, lease: &str) {
        let identity = self.key.lock().clone().unwrap_or_else(|| self.channel.clone());
        let (broker, id) = (Arc::clone(&self.broker), lease.to_owned());
        match crate::rt::block_on(async move { Ok(broker.release_lease(&identity, &id).await) }) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::error!(run = %self.run, lease, "a run's credential lease wasn't given back: {e}"),
            Err(e) => tracing::error!(run = %self.run, lease, "a run's credential lease wasn't given back: the broker: {e:#}"),
        }
    }
}

impl Drop for RunLessor {
    /// The run's end, as nothing asks through its lessor after: kept so in the broker's ledger, then
    /// every lease held for it revoked, and nothing more leased to it (the broker's
    /// [`Broker::release`]), under its channel and its key alike. Off the runtime it ends here; on
    /// one of its shards, where nothing may block, on a task of its own.
    fn drop(&mut self) {
        let mut names = vec![std::mem::take(&mut self.channel)];
        names.extend(self.key.get_mut().take());
        let (broker, run) = (Arc::clone(&self.broker), std::mem::take(&mut self.run));
        let ending = async move {
            let mut revoked = 0usize;
            for name in names {
                revoked = revoked.saturating_add(broker.release(&name).await.map_err(anyhow::Error::msg)?);
            }
            anyhow::Ok(revoked)
        };
        if crate::rt::on_shard() {
            drop(crate::rt::spawn(async move { ended(&run, ending.await) }));
            return;
        }
        ended(&run, crate::rt::block_on(ending));
    }
}

/// Says what a run's end revoked that its host hadn't given back, or why it couldn't: a lease the
/// store couldn't revoke is revoked as the broker next opens, where the run's end was kept.
fn ended(run: &str, revoked: anyhow::Result<usize>) {
    match revoked {
        Ok(0) => {}
        Ok(revoked) => tracing::info!(run, revoked, "a run's credential leases its host hadn't given back, revoked at its end"),
        Err(e) => tracing::error!(run, "a run's credential leases weren't revoked at its end: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use hex_api::BoxId;
    use hex_api::lease::conformance::{self, Fixture};
    use hex_api::lease::{LeaseRefused, Lessor};
    use kiln_seams::secrets::SecretValue;

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

    /// Box `name`'s run's lessor, as Hex's image class asks kiln's lessors for one as the run boots
    /// (`ImageClass::run_lessor`): through Hex's seam alone.
    fn lessor(lessors: &Lessors, name: &str) -> anyhow::Result<Arc<dyn Lessor>> {
        let id = BoxId::new(name).map_err(anyhow::Error::msg)?;
        hex_api::lease::Lessors::lessor(lessors, &id).map_err(anyhow::Error::msg)
    }

    /// Credential `name` holds `value` in the store, with no end of its own.
    fn hold(u: &Under, name: &str, value: &[u8]) -> anyhow::Result<()> {
        let (secrets, name, value) = (Arc::clone(&u.secrets), name.to_owned(), SecretValue::new(value.to_vec()));
        crate::rt::block_on(async move { secrets.create(&name, value).await.map(|_| ()).map_err(anyhow::Error::msg) })
    }

    /// Whether the store reads `lease` still.
    fn reads(u: &Under, lease: &str) -> anyhow::Result<bool> {
        let (secrets, lease) = (Arc::clone(&u.secrets), lease.to_owned());
        crate::rt::block_on(async move { Ok(secrets.read_leased(&lease).await.is_ok()) })
    }

    /// How many leases the broker's ledger holds for runs and hasn't revoked, every run's together.
    fn unrevoked(u: &Under) -> anyhow::Result<i64> {
        let ledger = u.ledger.clone();
        crate::rt::block_on(async move { ledger.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM broker_leases WHERE revoked_ms IS NULL", [], |r| r.get::<_, i64>(0))?)).await })
    }

    /// Hex's lease contract's fixture: kiln's lessors, and the store they lease from.
    struct Contract<'a> {
        under: &'a Under,
        lessors: &'a Lessors,
    }

    impl Fixture for Contract<'_> {
        fn lessors(&self) -> &dyn hex_api::lease::Lessors {
            self.lessors
        }
        fn hold(&self, reference: &str, value: &[u8]) -> Result<(), String> {
            hold(self.under, reference, value).map_err(|e| format!("{e:#}"))
        }
        fn holds(&self, lease: &str) -> Result<bool, String> {
            reads(self.under, lease).map_err(|e| format!("{e:#}"))
        }
    }

    /// CP-T8.3 (D242; Hex's docs/design/credential-grants.md C3): kiln's lessors over its run-lease
    /// broker pass Hex's lease contract (HEX-LEASE-1 to 4) through its seam alone, over the
    /// simulator's store and kiln's own alike, and leave none of its cases' leases held in the
    /// broker's ledger.
    #[test]
    fn cp_t8_3_kilns_lessors_pass_hexs_lease_contract() -> anyhow::Result<()> {
        for (u, _, lessors) in stores()? {
            let failures = conformance::run(&Contract { under: &u, lessors: &lessors });
            anyhow::ensure!(failures.is_empty(), "{}: {failures:?}", u.name);
            anyhow::ensure!(unrevoked(&u)? == 0, "{}: a case left a lease held", u.name);
        }
        Ok(())
    }

    /// CP-T8.3 (D242): through Hex's seam, a run's lessor leases its credential's value from the
    /// broker by reference, living until the run ends where its provider gives it no end, its value
    /// never shown; each lease it gives back is revoked alone, the run's others and another run's
    /// held on; and one another run's lessor gives back is held on.
    #[test]
    fn cp_t8_3_a_runs_lessor_leases_by_reference_through_hexs_seam_and_gives_each_lease_back() -> anyhow::Result<()> {
        for (u, _, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/anthropic", b"sk-ant-api03-real")?;
            let (run, other) = (lessor(&lessors, "box-a")?, lessor(&lessors, "box-b")?);
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
        }
        Ok(())
    }

    /// CP-T8.3 (D242): a run's end, as Hex and the run's host let go of its lessor, revokes every
    /// lease it holds still, issued to its key or to its channel, and nothing more is leased under
    /// its key; another run's leases are held on, and it leases on.
    #[test]
    fn cp_t8_3_a_runs_end_revokes_every_lease_it_holds() -> anyhow::Result<()> {
        const A: [u8; 32] = [0xa1; 32];
        for (u, _, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/anthropic", b"sk-ant-api03-real")?;
            hold(&u, "providers/openai", b"sk-proj-real")?;
            let (keyed, bare, other) = (lessor(&lessors, "box-k")?, lessor(&lessors, "box-b")?, lessor(&lessors, "box-o")?);
            let mut held = Vec::new();
            for (run, key) in [(&keyed, Some(A)), (&bare, None)] {
                for reference in ["providers/anthropic", "providers/openai"] {
                    held.push(run.lease(reference, key).map_err(anyhow::Error::msg)?.lease);
                }
            }
            let theirs = other.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            drop((keyed, bare));
            for lease in &held {
                anyhow::ensure!(!reads(&u, lease)?, "{store}: a run's lease outlived its end");
            }
            anyhow::ensure!(reads(&u, &theirs.lease)?, "{store}: one run's end revoked another's lease");
            let after = lessor(&lessors, "box-k")?.lease("providers/anthropic", Some(A)).err();
            anyhow::ensure!(after == Some(LeaseRefused::Ended), "{store}: an ask under the key of a run that ended: {after:?}");
            anyhow::ensure!(other.lease("providers/openai", None).is_ok(), "{store}: one run's end ended another's leases");
        }
        Ok(())
    }

    /// CP-T8.3 (D242): a run let go of on one of the runtime's shards, which can't block, ends on a
    /// task of its own, its leases revoked all the same: waited for by what its end leaves in the
    /// store, bounded only against a hang.
    #[test]
    fn cp_t8_3_a_run_let_go_of_on_the_runtime_ends_all_the_same() -> anyhow::Result<()> {
        for (u, _, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/anthropic", b"sk-ant-api03-real")?;
            let run = lessor(&lessors, "box-a")?;
            let one = run.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            crate::rt::block_on(async move {
                drop(run);
                Ok(())
            })?;
            let until = Instant::now().checked_add(Duration::from_secs(60)).ok_or_else(|| anyhow::anyhow!("{store}: no bound past now"))?;
            while reads(&u, &one.lease)? {
                anyhow::ensure!(Instant::now() < until, "{store}: a run let go of on a shard kept its lease");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }

    /// CP-T8.3 (D242): through Hex's seam, each refusal is D242's kind, in the broker's words: a
    /// credential the store holds none of, or can't name, denied; a run's second key unavailable;
    /// an ask under a key a move replaced, that key and the run's key now; a value past its own end
    /// unavailable until its sign-in refreshes it, and one with an end lives until then.
    #[test]
    fn cp_t8_3_a_lessor_refuses_by_d242s_kinds() -> anyhow::Result<()> {
        const A: [u8; 32] = [0xa1; 32];
        const B: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
        for (u, broker, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/openai", b"sk-proj-real")?;
            let run = lessor(&lessors, "box-a")?;
            anyhow::ensure!(matches!(run.lease("providers/none", None), Err(LeaseRefused::Denied(_))), "{store}: a credential the store holds none of");
            anyhow::ensure!(matches!(run.lease("not a name", None), Err(LeaseRefused::Denied(_))), "{store}: a reference no store names");
            let keyed = lessor(&lessors, "box-k")?;
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
