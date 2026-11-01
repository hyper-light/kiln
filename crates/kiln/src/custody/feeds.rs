//! kiln's server's ends of the conversation feeds (docs/ci.md W27-R08; the Transfers plan's step
//! 12; `crate::convo::feed::Custodian`): its receipt of each batch a runner's store sends it, kept
//! with the runner's hand-on of it; and its hand-on of each batch of its own store it sends a
//! mirror, to the runner process keeping the copy there now, which signs its receipt as it takes
//! it. A runner process that takes no custody (`CAP_CUSTODY`) is handed nothing signed.

use std::sync::{Arc, Mutex, PoisonError, Weak};

use futures::future::BoxFuture;
use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{Statement, Subject};
use serde_json::Value;

use crate::convo::feed::Custodian;
use crate::hub::Hub;

/// Gives the server's intake and its mirrors their custodians, once its own identity can sign.
pub fn install(hub: &Arc<Hub>) {
    if let Some(intake) = &hub.harness.feed {
        intake.sign_with(Arc::new(Taker(Arc::downgrade(hub))));
    }
    if let Some(mirrors) = &hub.harness.copies {
        let w = Arc::downgrade(hub);
        mirrors.sign_with(Arc::new(move |machine: &str, peer: Arc<Mutex<Option<u64>>>| Arc::new(ToMirror { hub: w.clone(), machine: machine.to_owned(), peer }) as Arc<dyn Custodian>));
    }
}

/// Keeps `bundle`, as it came over `came_from`, saying so where kiln can't.
async fn kept(hub: &Hub, bundle: &Value, came_from: &str) {
    match super::take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), bundle, came_from, crate::util::now_unix()).await {
        Ok(Ok(t)) if t.broke.is_some() => tracing::warn!(target: "kiln.custody", digest = %t.digest, broke = t.broke.as_deref().unwrap_or(""), %came_from, "a conversation feed's batch broke its chain"),
        Ok(Ok(_)) => {}
        Ok(Err(why)) => tracing::info!(target: "kiln.custody", %why, %came_from, "refused a record of a conversation feed's batch"),
        Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "a record of a conversation feed's batch couldn't be kept"),
    }
}

/// The server's intake of runners' stores (and of mirrors' copies sent back to restore its own).
struct Taker(Weak<Hub>);

impl Custodian for Taker {
    fn hand_on(&self, _thing: Subject) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }

    fn received(&self, machine: &str, hand_on: &str, thing: Subject) {
        let (Some(hub), Ok(given)) = (self.0.upgrade(), serde_json::from_str::<Value>(hand_on)) else { return };
        let Some(giver) = crate::runner::custody::giver_of(&given) else { return };
        let server = super::server_uri(&hub.pki.organization());
        let body = Body { from: Some(giver), why: "a batch of a runner's conversation store, taken into kiln's".into(), ..Body::new(Kind::Received, &server) };
        let receipt = Statement { subject: vec![thing], follows: vec![crate::runner::custody::digest_of(&given)], ..Statement::new(to_value(&body)) };
        let came_from = format!("runner {machine}");
        crate::rt::spawn(async move {
            // The runner's hand-on as the batch carried it, then kiln's receipt of what came.
            kept(&hub, &given, &came_from).await;
            match super::server_signed(&hub.pki, &receipt) {
                Ok(b) => kept(&hub, &b, "kiln").await,
                Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "kiln's receipt of a conversation feed's batch couldn't be signed"),
            }
        });
    }
}

/// The server's feed to the mirror on `machine`: its hand-on of each batch, to the runner process
/// of it that keeps the copy now (`peer`).
struct ToMirror {
    hub: Weak<Hub>,
    machine: String,
    peer: Arc<Mutex<Option<u64>>>,
}

impl Custodian for ToMirror {
    fn hand_on(&self, thing: Subject) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let hub = self.hub.upgrade()?;
            let peer = (*self.peer.lock().unwrap_or_else(PoisonError::into_inner))?;
            let instance = {
                let instances = hub.instances.lock();
                let i = instances.get(peer)?;
                ((u64::from(i.caps) | i.wide) & crate::proto::CAP_CUSTODY != 0).then(|| i.label())?
            };
            let to = crate::identities::runner_identity(&hub, &self.machine, &instance).await.ok()?.uri().ok()?;
            let server = super::server_uri(&hub.pki.organization());
            let body = Body { to: Some(to), why: "a batch of kiln's conversation store, for the copy this runner keeps".into(), ..Body::new(Kind::HandedOn, &server) };
            let bundle = match super::server_signed(&hub.pki, &Statement { subject: vec![thing], ..Statement::new(to_value(&body)) }) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(target: "kiln.custody", machine = %self.machine, error = %format!("{e:#}"), "kiln's hand-on of a batch to a mirror couldn't be signed");
                    return None;
                }
            };
            kept(&hub, &bundle, "kiln").await;
            Some(bundle.to_string())
        })
    }

    fn received(&self, _machine: &str, _hand_on: &str, _thing: Subject) {}
}
