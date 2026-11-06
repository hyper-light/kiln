//! kiln's server's ends of the conversation feeds (docs/ci.md W27-R08; the Transfers plan's step
//! 12; `crate::convo::feed::Custodian`): its receipt of each batch a runner's store sends it, kept
//! with the runner's hand-on of it; and its hand-on of each batch of its own store it sends a
//! mirror, to the runner process keeping the copy there now, which signs its receipt as it takes
//! it. A runner process that takes no custody (`CAP_CUSTODY`) is handed nothing signed.

use std::sync::{Arc, Weak};

use futures::future::BoxFuture;
use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{Statement, Subject};
use serde_json::Value;

use crate::convo::feed::{Ack, Custodian};
use crate::hub::Hub;

/// The bytes of receipts waiting for the task that keeps them, at the most (goal 11, plan 01a1a87b2e56 step
/// 3): past it, kiln is that far behind, and a batch's receipt is let go (said so), its hand-on unanswered.
/// As many as the intake's queue of feed messages holds (`crate::convo::feed::HEARD_BYTES`), each receipt's
/// hand-on having come in one of them, counted there. It was 1,024 receipts of any size, their places made
/// up front: 0.35 MB of an idle server's heap (research 01a1a87b2e57, f10).
pub const RECEIPTS_BYTES: usize = crate::convo::feed::HEARD_BYTES;

/// Gives the server's intake and its mirrors their custodians, once its own identity can sign.
/// Called on the runtime: the intake's receipts are kept by a task of their own, so its thread
/// never waits on them.
pub fn install(hub: &Arc<Hub>) {
    if let Some(intake) = &hub.harness.feed {
        let (tx, rx) = crate::memory::queue::bounded::<Receipt>(&crate::memory::owners::CUSTODY_RECEIPTS, RECEIPTS_BYTES);
        let w = Arc::downgrade(hub);
        // On the next shard round-robin, not the one the server starts on (goal 9, plan 01a1a87b2cec step 3).
        crate::rt::spawn_spread(async move {
            while let Ok(Receipt { given, receipt, came_from, ack }) = rx.recv().await {
                let Some(hub) = w.upgrade() else { return };
                // The runner's hand-on as the batch carried it, then kiln's receipt of what came, signed
                // off the shards (goal 9, plan 01a1a87b2cec step 3): both read in one job, kept in one
                // call (step 5).
                match super::take_batch(&hub.store, &hub.pki, given, &came_from, receipt, crate::util::now_unix()).await {
                    Ok((given, receipt)) => {
                        said(given, &came_from);
                        match receipt {
                            Ok(taken) => said(taken, "kiln"),
                            Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "kiln's receipt of a conversation feed's batch couldn't be signed"),
                        }
                    }
                    Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "a record of a conversation feed's batch couldn't be kept"),
                }
                // The batch's Ack, once its records are kept (or said not kept): its runner's next batch at its store's tip
                // waited for it (consts-audit R4).
                if let Some(ack) = ack {
                    ack.said().await;
                }
            }
        });
        intake.sign_with(Arc::new(Taker { hub: Arc::downgrade(hub), tx }));
    }
    if let Some(mirrors) = &hub.harness.copies {
        // kiln's hand-ons to its mirrors kept beside the store, for their receipts' hops (goal 9, plan 01a1a87b2cec step 5).
        super::recent::keep_for(hub.cfg.conversations.mirrors);
        let w = Arc::downgrade(hub);
        mirrors.sign_with(Arc::new(move |machine: &str, peer: crate::convo::mirror::Peer| Arc::new(ToMirror { hub: w.clone(), machine: machine.to_owned(), peer }) as Arc<dyn Custodian>));
    }
}

/// What became of a record of a conversation feed's batch, as it came over `came_from`, said where kiln couldn't take it.
fn said(taken: anyhow::Result<Result<super::Taken, super::Refused>>, came_from: &str) {
    match taken {
        Ok(Ok(t)) if t.broke.is_some() => tracing::warn!(target: "kiln.custody", digest = %t.digest, broke = t.broke.as_deref().unwrap_or(""), %came_from, "a conversation feed's batch broke its chain"),
        Ok(Ok(_)) => {}
        Ok(Err(why)) => tracing::info!(target: "kiln.custody", %why, %came_from, "refused a record of a conversation feed's batch"),
        Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "a record of a conversation feed's batch couldn't be kept"),
    }
}

/// The server's intake of runners' stores (and of mirrors' copies sent back to restore its own):
/// its receipts go to the task [`install`] started.
struct Taker {
    hub: Weak<Hub>,
    tx: crate::memory::queue::Sender<Receipt>,
}

/// A batch's records for that task: its runner's hand-on, kiln's receipt to sign, what it came over, and the batch's Ack,
/// said once they're kept.
struct Receipt {
    given: Value,
    receipt: Statement,
    came_from: String,
    ack: Option<Ack>,
}

/// Counts what's written to it, keeping none of it.
struct Count(usize);

impl std::io::Write for Count {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(b.len());
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl crate::memory::queue::Weigh for Receipt {
    /// Its place, the hand-on's tree (`crate::harness::weigh::value`), the receipt as its JSON's bytes
    /// (counted as written, nothing kept), where it came from, and its Ack.
    fn weight(&self) -> usize {
        let mut receipt = Count(0);
        let _ = serde_json::to_writer(&mut receipt, &self.receipt);
        size_of::<Receipt>().saturating_add(crate::harness::weigh::value(&self.given)).saturating_add(receipt.0).saturating_add(self.came_from.len()).saturating_add(self.ack.as_ref().map_or(0, Ack::bytes))
    }
}

impl Custodian for Taker {
    fn hand_on(&self, _thing: Subject) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }

    /// Its receipt to the task that keeps receipts, with the batch's Ack, which that task says once it's kept (consts-audit
    /// R4): the runner's next batch at its store's tip waits for it, so the receipts waiting never outnumber the batches on
    /// their way. One it can't read is answered at once, as one let go is.
    fn received(&self, machine: &str, hand_on: &str, thing: Subject, ack: Option<Ack>) -> Option<Ack> {
        let (Some(hub), Ok(given)) = (self.hub.upgrade(), serde_json::from_str::<Value>(hand_on)) else { return ack };
        let Some(giver) = crate::runner::custody::giver_of(&given) else { return ack };
        let server = super::server_uri(&hub.pki.organization());
        let body = Body { from: Some(giver), why: "a batch of a runner's conversation store, taken into kiln's".into(), ..Body::new(Kind::Received, &server) };
        let receipt = Statement { subject: vec![thing], follows: vec![crate::runner::custody::digest_of(&given)], ..Statement::new(to_value(&body)) };
        match self.tx.try_send(Receipt { given, receipt, came_from: format!("runner {machine}"), ack }) {
            Ok(()) => None,
            Err(async_channel::TrySendError::Full(r) | async_channel::TrySendError::Closed(r)) => {
                tracing::warn!(target: "kiln.custody", %machine, "kiln is behind keeping its receipts of conversation feeds' batches: this one is let go, its hand-on unanswered");
                r.ack
            }
        }
    }
}

/// The server's feed to the mirror on `machine`: its hand-on of each batch, to the runner process
/// of it that keeps the copy now (`peer`).
struct ToMirror {
    hub: Weak<Hub>,
    machine: String,
    peer: crate::convo::mirror::Peer,
}

impl Custodian for ToMirror {
    fn hand_on(&self, thing: Subject) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let hub = self.hub.upgrade()?;
            let peer = self.peer.load().as_deref().copied()?;
            let instance = {
                let instances = hub.instances.load();
                let i = instances.get(peer)?;
                ((u64::from(i.caps) | i.wide) & crate::proto::CAP_CUSTODY != 0).then(|| i.label())?
            };
            let to = crate::identities::runner_identity(&hub, &self.machine, &instance).await.ok()?.uri().ok()?;
            let server = super::server_uri(&hub.pki.organization());
            let body = Body { to: Some(to), why: "a batch of kiln's conversation store, for the copy this runner keeps".into(), ..Body::new(Kind::HandedOn, &server) };
            // Signed off the shards (goal 9, plan 01a1a87b2cec step 3), in the job that reads it to keep it (step 5).
            let (bundle, taken) = match super::sign_and_take(&hub.store, &hub.pki, Statement { subject: vec![thing], ..Statement::new(to_value(&body)) }, "kiln", crate::util::now_unix()).await {
                Ok(signed) => signed,
                Err(e) => {
                    tracing::warn!(target: "kiln.custody", machine = %self.machine, error = %format!("{e:#}"), "kiln's hand-on of a batch to a mirror couldn't be signed");
                    return None;
                }
            };
            said(taken, "kiln");
            Some(bundle.to_string())
        })
    }

    fn received(&self, _machine: &str, _hand_on: &str, _thing: Subject, ack: Option<Ack>) -> Option<Ack> {
        ack
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::queue::Weigh as _;

    /// Consts-audit R4 (goal 9, plan 01a1a87b2cec step 5): kiln's receipt of a runner's batch goes to the task that keeps
    /// receipts with the batch's Ack, which that task says once the receipt is kept: the runner's next batch at its store's
    /// tip waits for it, so the receipts waiting never outnumber the batches on their way and a slow kiln.db grows the
    /// batches. A receipt let go for want of room, and a hand-on it can't read, have their Ack handed back, said at once. Do:
    /// a taker with room for one receipt; three batches received, the third's hand-on not one. Expect: the first's Ack waits
    /// with its receipt, none handed back; the second's and the third's handed back.
    #[test]
    fn r4_a_receipt_carries_its_batchs_ack_to_be_said_once_its_kept() -> anyhow::Result<()> {
        use base64::Engine as _;
        use kiln_ci::identity::statement::{PAYLOAD_CUSTODY, payload};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (tx, rx) = crate::memory::queue::places::<Receipt>(&crate::memory::owners::CUSTODY_RECEIPTS, 1);
            let taker = Taker { hub: Arc::downgrade(&hub), tx };
            let (to, _answers) = crate::memory::queue::places::<crate::convo::feed::Out>(&crate::memory::owners::CHANNELS, 4);
            let ack = |rev: u64| Some(Ack::of(crate::convo::feed::Msg::Ack { store: "s1".into(), rev }, 7, to.clone()));
            let thing = |n: u64| crate::convo::feed::batch_thing("s1", n, n + 1, &crate::util::sha256_hex(&n.to_be_bytes()));
            // A runner's hand-on, as a bundle's JSON (unsigned: what's read of it here is who gave it).
            let st = Statement { subject: vec![thing(0)], ..Statement::new(serde_json::json!({ "v": 1, "kind": "handed-on", "by": "kiln://hyper-light/person/ada/machine/box/instance/i", "to": "kiln://hyper-light/server" })) };
            let encoded = base64::engine::general_purpose::STANDARD.encode(payload(&st).unwrap_or_default());
            let hand_on = serde_json::json!({ "dsseEnvelope": { "payloadType": PAYLOAD_CUSTODY, "payload": encoded, "signatures": [] } }).to_string();
            let given: Value = serde_json::from_str(&hand_on)?;
            anyhow::ensure!(crate::runner::custody::giver_of(&given).is_some(), "its giver read: {hand_on}");
            anyhow::ensure!(taker.received("box", &hand_on, thing(1), ack(1)).is_none(), "the first's Ack waits with its receipt");
            anyhow::ensure!(taker.received("box", &hand_on, thing(2), ack(2)).is_some(), "the second, let go for want of room: its Ack handed back");
            anyhow::ensure!(taker.received("box", "not a hand-on", thing(3), ack(3)).is_some(), "a hand-on it can't read: its Ack handed back");
            let waiting = rx.try_recv().map_err(|_| anyhow::anyhow!("no receipt waiting"))?;
            anyhow::ensure!(waiting.ack.is_some() && waiting.came_from == "runner box", "the receipt waiting carries its batch's Ack");
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a receipt waiting for the task weighs its hand-on, its receipt
    /// and where it came from, so its queue is bounded in bytes whatever the bundles' sizes.
    #[test]
    fn g11_s3_a_receipt_weighs_its_hand_on() {
        let receipt = |n: usize| Receipt { given: serde_json::json!({ "bundle": "b".repeat(n) }), receipt: Statement::new(serde_json::json!({ "why": "a test" })), came_from: "runner m1".into(), ack: None };
        assert_eq!(receipt(5000).weight() - receipt(0).weight(), 5000);
        assert!(receipt(0).weight() > size_of::<Receipt>() + "runner m1".len(), "the receipt's own JSON counts");
        assert_eq!(RECEIPTS_BYTES, crate::convo::feed::HEARD_BYTES);
    }
}
