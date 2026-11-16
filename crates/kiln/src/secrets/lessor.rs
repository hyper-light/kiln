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
//! - **A run's sign-ins** (D242, step 8's third slice): a capture's tokens kept by the broker, its
//!   CLI given placeholders; a refresh made at the sign-in's provider by the broker alone
//!   ([`super::signin`]), its CLI answered placeholders; a revocation in the microVM said, the
//!   sign-in kept.
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
use hex_api::lease::{Lease, LeaseRefused, Lessor, SignInRefused, SignInTokens, Value};
use kiln_seams::Clock;
use kiln_seams::secrets::{Refusal, SecretValue};
use parking_lot::Mutex;

use super::broker::{Broker, Refused, Tokens, Unrefreshed};
use super::signin::{self, Provider};

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

/// A sign-in refusal of the broker's, as D242 kinds it: a run that ended as such; a sign-in nothing
/// was captured for, and a refresh its provider refused, as denied, so its CLI signs in anew; every
/// other as unavailable for now.
fn sign_in_refused(why: Refused) -> SignInRefused {
    match why {
        Refused::RunEnded(_) => SignInRefused::Ended,
        Refused::NoSignIn(_) | Refused::Upstream { why: Unrefreshed::Refused(_), .. } => SignInRefused::Denied(why.to_string()),
        other => SignInRefused::Unavailable(other.to_string()),
    }
}

/// A sign-in whose name names no provider the broker refreshes at ([`Provider::of`]): denied.
fn unprovided(sign_in: &str) -> SignInRefused {
    SignInRefused::Denied(format!("sign-in {sign_in:?} names no provider kiln refreshes at (claude/… or codex/…)"))
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

    /// Sign-in `sign_in`'s tokens, as the run's injector took them from its CLI's token response,
    /// kept by the broker (D242, step 8's third slice): the reference its access token is leased by
    /// from now, and the placeholders its CLI holds in their places. Its ID token, inert, is not
    /// kept. A sign-in whose name names no provider is refused, nothing kept.
    fn capture(&self, sign_in: &str, tokens: SignInTokens, key: Option<[u8; 32]>) -> Result<hex_api::Captured, SignInRefused> {
        self.identity(key).map_err(|why| SignInRefused::Unavailable(why.to_string()))?;
        if Provider::of(sign_in).is_none() {
            return Err(unprovided(sign_in));
        }
        let SignInTokens { access, refresh, expires_in, scope, .. } = tokens;
        let tokens = Tokens { access: SecretValue::new(access.into_bytes()), refresh: refresh.map(|r| SecretValue::new(r.into_bytes())), expires_in_s: expires_in, scope };
        let (broker, name) = (Arc::clone(&self.broker), sign_in.to_owned());
        let kept = crate::rt::block_on(async move { Ok(broker.capture(&name, tokens).await) }).map_err(|e| SignInRefused::Unavailable(format!("run {}: the broker: {e:#}", self.run)))?;
        let captured = kept.map_err(sign_in_refused)?;
        Ok(hex_api::Captured { access: captured.access.reference, access_placeholder: captured.access.placeholder, refresh_placeholder: captured.refresh.map(|r| r.placeholder) })
    }

    /// Sign-in `sign_in` refreshed at its provider by the broker, which alone calls it, one refresh
    /// of a sign-in at a time across every run (D242, step 8's third slice): the token response its
    /// CLI is to have, its tokens placeholders ([`signin::answer`]).
    fn refresh(&self, sign_in: &str, key: Option<[u8; 32]>) -> Result<Value, SignInRefused> {
        self.identity(key).map_err(|why| SignInRefused::Unavailable(why.to_string()))?;
        let provider = Provider::of(sign_in).ok_or_else(|| unprovided(sign_in))?;
        let (broker, name) = (Arc::clone(&self.broker), sign_in.to_owned());
        let got = crate::rt::block_on(async move { Ok(broker.refresh(&name, provider.upstream()).await) }).map_err(|e| SignInRefused::Unavailable(format!("run {}: the broker: {e:#}", self.run)))?;
        let captured = got.map_err(sign_in_refused)?;
        let answer = signin::answer(&captured, self.clock.unix_ms()).map_err(|e| SignInRefused::Unavailable(format!("run {}: its answer: {e}", self.run)))?;
        Ok(Value::new(answer))
    }

    /// The run's microVM revoked sign-in `sign_in` (its CLI's logout, or a sign-in anew), answered
    /// there: said, and its tokens kept, as the sign-in outlives the run (D242, step 8's question 6).
    fn revoked(&self, sign_in: &str, _key: Option<[u8; 32]>) {
        tracing::info!(run = %self.run, sign_in, "a run's microVM revoked its sign-in, answered there: its tokens kept");
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
            let captured = crate::rt::block_on(async move { b.capture("claude/ada", super::super::broker::Tokens { access: v("oat-real"), refresh: None, expires_in_s: Some(600), scope: None }).await.map_err(anyhow::Error::msg) })?;
            let short = run.lease(&captured.access.reference, None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(short.life == Some(Duration::from_secs(600)), "{store}: its life {:?}", short.life);
            u.clock.advance(Duration::from_secs(601));
            anyhow::ensure!(matches!(run.lease(&captured.access.reference, None), Err(LeaseRefused::Unavailable(_))), "{store}: a value past its own end");
        }
        Ok(())
    }

    /// Claude Code's and Codex's token endpoints stood in for, once for every test here (each
    /// test's sign-ins its own refresh tokens): each refresh asked of it kept, by its path and
    /// body, and answered as its refresh token says: `rt-refused` refused (RFC 6749 §5.2's 400),
    /// `rt-broken` unavailable (503), any other given its next tokens, Codex's with an ID token.
    struct StandIn {
        asked: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    }

    impl StandIn {
        /// The refreshes asked of it with refresh token `rt`, by their paths and bodies.
        fn asked(&self, rt: &str) -> Vec<(String, serde_json::Value)> {
            self.asked.lock().iter().filter(|(_, body)| body.get("refresh_token").and_then(serde_json::Value::as_str) == Some(rt)).cloned().collect()
        }
    }

    fn stand_in() -> anyhow::Result<&'static StandIn> {
        use crate::http::server::{self as srv, Request, Response};

        struct Fake {
            asked: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        }
        impl srv::Handler for Fake {
            async fn handle(self: Arc<Self>, req: Request) -> Response {
                let path = req.uri().path().to_owned();
                let body = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
                let asked: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let rt = asked.get("refresh_token").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
                self.asked.lock().push((path.clone(), asked));
                match rt.as_str() {
                    "rt-refused" => srv::json(http::StatusCode::BAD_REQUEST, &serde_json::json!({ "error": "invalid_grant", "error_description": "Refresh token revoked" })),
                    "rt-broken" => srv::json(http::StatusCode::SERVICE_UNAVAILABLE, &serde_json::json!({ "error": "temporarily_unavailable" })),
                    _ if path == "/oauth/token" => srv::json(http::StatusCode::OK, &serde_json::json!({ "id_token": format!("eyJh.eyJp.sig-{rt}"), "access_token": format!("at-{rt}"), "refresh_token": format!("{rt}-2") })),
                    _ => srv::json(http::StatusCode::OK, &serde_json::json!({ "access_token": format!("at-{rt}"), "refresh_token": format!("{rt}-2"), "expires_in": 3600, "token_type": "Bearer" })),
                }
            }
        }

        /// Bound, served and made both providers' endpoint before any test asks it.
        fn start() -> anyhow::Result<StandIn> {
            let asked = Arc::new(Mutex::new(Vec::new()));
            let fake = Arc::new(Fake { asked: Arc::clone(&asked) });
            let addr = crate::rt::block_on(async move {
                let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
                let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
                crate::rt::spawn(async move {
                    while let Ok(s) = tcp.accept().await {
                        let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
                        crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
                    }
                });
                anyhow::Ok(addr)
            })?;
            crate::ci::integrations::override_endpoint("claude:token", &format!("http://{addr}"));
            crate::ci::integrations::override_endpoint("codex:token", &format!("http://{addr}"));
            Ok(StandIn { asked })
        }
        static S: std::sync::LazyLock<Result<StandIn, String>> = std::sync::LazyLock::new(|| start().map_err(|e| format!("{e:#}")));
        S.as_ref().map_err(|e| anyhow::anyhow!("the stand-in token endpoint: {e}"))
    }

    fn sign_in_tokens(access: &str, refresh: &str, expires_in: Option<u64>, id_token: Option<&str>, scope: Option<&str>) -> SignInTokens {
        let value = |s: &str| Value::new(s.as_bytes().to_vec());
        SignInTokens { access: value(access), refresh: Some(value(refresh)), expires_in, id_token: id_token.map(value), scope: scope.map(str::to_owned) }
    }

    /// A refresh's answer as its CLI reads it: its fields, and its words whole.
    fn read(answer: &Value) -> anyhow::Result<(serde_json::Map<String, serde_json::Value>, String)> {
        let words = String::from_utf8(answer.expose().to_vec())?;
        match serde_json::from_str(&words)? {
            serde_json::Value::Object(fields) => Ok((fields, words)),
            other => anyhow::bail!("an answer that is no object: {other}"),
        }
    }

    fn placeholder(fields: &serde_json::Map<String, serde_json::Value>, name: &str) -> bool {
        fields.get(name).and_then(serde_json::Value::as_str).is_some_and(|p| p.starts_with("kiln-placeholder-"))
    }

    /// Plan step 33's step 8 (D242, its third slice; CP-T8.3): through Hex's seam, a run's sign-in
    /// is captured into the broker, its CLI given placeholders and its access token leased by
    /// reference; its refresh is made at its provider by the broker alone, as its CLI's own would
    /// be (Claude Code's scope asked again, Codex's none), the rotated tokens kept under the same
    /// references and its CLI answered placeholders, never a token, Codex's ID token left out; and a
    /// revocation in its microVM keeps the sign-in.
    #[test]
    fn cp_t8_3_a_runs_sign_ins_are_captured_refreshed_and_revoked_outside_through_hexs_seam() -> anyhow::Result<()> {
        let provider = stand_in()?;
        for (u, _, lessors) in stores()? {
            let store = u.name;
            let run = lessor(&lessors, &format!("box-s-{store}"))?;
            let claude = format!("claude/ada-{store}");
            let scope = "user:inference user:profile";
            let c = run.capture(&claude, sign_in_tokens(&format!("at-claude-{store}"), &format!("rt-claude-{store}"), Some(28800), None, Some(scope)), None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(c.access == format!("sign-ins/{claude}/access"), "{store}: its reference {}", c.access);
            let refresh = c.refresh_placeholder.clone().unwrap_or_default();
            anyhow::ensure!(c.access_placeholder.starts_with("kiln-placeholder-") && refresh.starts_with("kiln-placeholder-") && c.access_placeholder != refresh, "{store}: {c:?}");
            let held = run.lease(&c.access, None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(held.value.expose() == format!("at-claude-{store}").as_bytes(), "{store}: the captured access token isn't the one leased");

            let (fields, words) = read(&run.refresh(&claude, None).map_err(anyhow::Error::msg)?)?;
            let asked = provider.asked(&format!("rt-claude-{store}"));
            let expect = serde_json::json!({ "grant_type": "refresh_token", "refresh_token": format!("rt-claude-{store}"), "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e", "scope": scope });
            anyhow::ensure!(asked == [("/v1/oauth/token".to_owned(), expect)], "{store}: Claude Code's provider was asked {asked:?}");
            anyhow::ensure!(!words.contains(&format!("claude-{store}")), "{store}: a token in the answer");
            anyhow::ensure!(placeholder(&fields, "access_token") && placeholder(&fields, "refresh_token"), "{store}: {fields:?}");
            let said = |name: &str| fields.get(name).cloned();
            anyhow::ensure!((said("token_type"), said("expires_in"), said("scope")) == (Some("Bearer".into()), Some(3600u64.into()), Some(scope.into())), "{store}: {fields:?}");
            anyhow::ensure!(fields.len() == 5, "{store}: its provider's other fields in the answer: {fields:?}");
            let rotated = run.lease(&c.access, None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(rotated.value.expose() == format!("at-rt-claude-{store}").as_bytes(), "{store}: the rotated access token isn't kept under its reference");
            run.revoked(&claude, None);
            let after = run.lease(&c.access, None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(after.value.expose() == format!("at-rt-claude-{store}").as_bytes(), "{store}: a revocation in the microVM ended the sign-in");

            let codex = format!("codex/ada-{store}");
            run.capture(&codex, sign_in_tokens(&format!("at-codex-{store}"), &format!("rt-codex-{store}"), None, Some("eyJh.eyJp.shards-placeholder"), None), None).map_err(anyhow::Error::msg)?;
            let (fields, words) = read(&run.refresh(&codex, None).map_err(anyhow::Error::msg)?)?;
            let asked = provider.asked(&format!("rt-codex-{store}"));
            let expect = serde_json::json!({ "client_id": "app_EMoamEEZ73f0CkXaXp7hrann", "grant_type": "refresh_token", "refresh_token": format!("rt-codex-{store}") });
            anyhow::ensure!(asked == [("/oauth/token".to_owned(), expect)], "{store}: Codex's provider was asked {asked:?}");
            anyhow::ensure!(!words.contains(&format!("codex-{store}")) && !words.contains("eyJ"), "{store}: a token in the answer: {fields:?}");
            anyhow::ensure!(placeholder(&fields, "access_token") && placeholder(&fields, "refresh_token") && fields.len() == 3, "{store}: {fields:?}");
        }
        Ok(())
    }

    /// Plan step 33's step 8 (D242, its third slice): through Hex's seam, each sign-in refusal is
    /// D242's kind: a sign-in whose name names no provider, captured by no one and nothing kept; a
    /// refresh of one never captured, and one its provider refuses, denied, so its CLI signs in
    /// anew; one its provider can't answer unavailable, its words holding no token.
    #[test]
    fn cp_t8_3_a_sign_ins_refusals_are_d242s_kinds() -> anyhow::Result<()> {
        stand_in()?;
        for (u, _, lessors) in stores()? {
            let store = u.name;
            let run = lessor(&lessors, &format!("box-r-{store}"))?;
            let devin = format!("devin/ada-{store}");
            let refused = run.capture(&devin, sign_in_tokens("at-devin", "rt-devin", None, None, None), None).err();
            anyhow::ensure!(matches!(refused, Some(SignInRefused::Denied(_))), "{store}: {refused:?}");
            let kept = run.lease(&format!("sign-ins/{devin}/access"), None).err();
            anyhow::ensure!(matches!(kept, Some(LeaseRefused::Denied(_))), "{store}: a sign-in naming no provider was kept: {kept:?}");
            anyhow::ensure!(matches!(run.refresh(&devin, None), Err(SignInRefused::Denied(_))), "{store}: a refresh naming no provider");
            anyhow::ensure!(matches!(run.refresh(&format!("claude/none-{store}"), None), Err(SignInRefused::Denied(_))), "{store}: a refresh of a sign-in never captured");
            for (name, rt) in [(format!("claude/refused-{store}"), "rt-refused"), (format!("codex/broken-{store}"), "rt-broken")] {
                run.capture(&name, sign_in_tokens("at-x", rt, None, None, None), None).map_err(anyhow::Error::msg)?;
                match (rt, run.refresh(&name, None)) {
                    ("rt-refused", Err(SignInRefused::Denied(_))) => {}
                    ("rt-broken", Err(SignInRefused::Unavailable(words))) => anyhow::ensure!(!words.contains(rt), "{store}: {words}"),
                    (_, got) => anyhow::bail!("{store}: {name}: {:?}", got.map(|_| "answered")),
                }
            }
        }
        Ok(())
    }

    /// Plan step 33's step 8 (D242, its third slice): two refreshes of one sign-in at once, as two
    /// runs' lessors ask them of the one broker, make one call to its provider, the second taking
    /// the first's tokens, as a rotated refresh token retires every other holder's (RFC 6749 §6).
    #[test]
    fn cp_t8_3_two_refreshes_of_one_sign_in_at_once_make_one_call_to_its_provider() -> anyhow::Result<()> {
        let provider = stand_in()?;
        for (u, broker, _) in stores()? {
            let store = u.name;
            let name = format!("claude/once-{store}");
            let rt = format!("rt-once-{store}");
            let b = Arc::clone(&broker);
            let (n, r) = (name.clone(), rt.clone());
            let (one, two) = crate::rt::block_on(async move {
                b.capture(&n, Tokens { access: v("at-once"), refresh: Some(v(&r)), expires_in_s: None, scope: None }).await.map_err(anyhow::Error::msg)?;
                anyhow::Ok(futures::join!(b.refresh(&n, Provider::ClaudeCode.upstream()), b.refresh(&n, Provider::ClaudeCode.upstream())))
            })?;
            let (one, two) = (one.map_err(anyhow::Error::msg)?, two.map_err(anyhow::Error::msg)?);
            anyhow::ensure!(one == two, "{store}: the second refresh didn't take the first's tokens");
            let calls = provider.asked(&rt).len();
            anyhow::ensure!(calls == 1, "{store}: {calls} calls to the provider");
        }
        Ok(())
    }
}
