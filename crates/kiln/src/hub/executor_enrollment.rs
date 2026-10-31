//! U03: durable enrollment of the physical pod under an existing StartExecutor grant.
//! Token hashes and certificate receipts are authority; the Hub's pod map is only an index.

use std::time::Duration;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use crate::sql::{OptionalExtension as _, params};
use crate::k8s::ExecutorIdentity;
use kiln_ci::sched::pools::PodCapacity;
use super::Hub;

const TOKENS: &str = "u03/executor-token/";
const PODS: &str = "u03/executor-pod/";
const LIMIT: usize = 4096;
const BYTES: usize = 16384;

#[derive(Clone, Serialize, Deserialize)]
struct Ticket {
    executor: String,
    generation: String,
    allocation: String,
    pod: String,
    expires: i64,
    issued: Option<Issued>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Issued {
    identity: ExecutorIdentity,
    csr: String,
    certificate: String,
    certificate_sha256: String,
}

fn token_key(token: &str) -> String { format!("{TOKENS}{}", crate::util::sha256_hex(token.as_bytes())) }
fn pod_key(pod: &str) -> String { format!("{PODS}{}", crate::util::sha256_hex(pod.as_bytes())) }
fn decode<T: for<'de> Deserialize<'de>>(json: &str) -> anyhow::Result<T> {
    anyhow::ensure!(json.len() <= BYTES, "U03 executor enrollment receipt exceeds its bound");
    Ok(serde_json::from_str(json)?)
}
fn encode<T: Serialize>(value: &T) -> anyhow::Result<String> {
    let json = serde_json::to_string(value)?;
    anyhow::ensure!(json.len() <= BYTES, "U03 executor enrollment receipt exceeds its bound");
    Ok(json)
}
fn same_ticket(ticket: &Ticket, identity: &ExecutorIdentity) -> bool {
    ticket.executor == identity.executor && ticket.generation == identity.generation
        && ticket.allocation == identity.allocation && ticket.pod == identity.name
}

async fn commit_enrollment(store: &crate::store::Store, key: String, expected: String, ticket: Ticket, issued: Issued, capacity: PodCapacity) -> anyhow::Result<()> {
    anyhow::ensure!(same_ticket(&ticket, &issued.identity) && ticket.issued.as_ref() == Some(&issued), "U03 certificate receipt does not match its issued token");
    anyhow::ensure!(ticket.expires > crate::util::now_unix(), "U03 executor enrollment token expired before commit");
    let updated = encode(&ticket)?;
    let enrollment = encode(&issued)?;
    let pod_key = pod_key(&issued.identity.name);
    store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let current: String = tx.query_row("SELECT v FROM kv WHERE k=?1", [&key], |r| r.get(0))?;
        anyhow::ensure!(current == expected, "U03 executor token was concurrently consumed");
        crate::ci::dispatch::bind_pod_enrollment(&tx, &issued.identity, &capacity)?;
        let old: Option<String> = tx.query_row("SELECT v FROM kv WHERE k=?1", [&pod_key], |r| r.get(0)).optional()?;
        if let Some(old) = old {
            let old: Issued = decode(&old)?;
            let same = old.identity == issued.identity && old.certificate_sha256 == issued.certificate_sha256;
            let successor = old.identity.executor == issued.identity.executor
                && old.identity.generation != issued.identity.generation && old.identity.allocation != issued.identity.allocation;
            anyhow::ensure!(same || successor, "U03 pod name already has another physical enrollment");
        } else {
            let count: usize = tx.query_row("SELECT count(*) FROM kv WHERE substr(k,1,length(?1))=?1", [PODS], |r| r.get(0))?;
            anyhow::ensure!(count < LIMIT, "U03 executor certificate table is full");
        }
        tx.execute("UPDATE kv SET v=?2 WHERE k=?1", params![key, updated])?;
        tx.execute("INSERT INTO kv(k,v) VALUES (?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v", params![pod_key, enrollment])?;
        tx.commit()?;
        Ok(())
    }).await
}

impl Hub {
    /// U03: persist token/grant ownership before the pod may be created. Never reserve here.
    pub(crate) async fn mint_executor_token(&self, grant: &crate::ci::dispatch::ExecutorGrant, pod: &str, ttl: Duration) -> anyhow::Result<String> {
        let executor = grant.executor.as_str();
        anyhow::ensure!(grant.phase == "starting" && grant.snapshot.runner.is_empty(), "U03 pod token requires an existing server-owned starting grant");
        anyhow::ensure!(pod == crate::k8s::executor_pod(executor), "U03 executor token pod name changed");
        let expires = crate::util::now_unix().checked_add(i64::try_from(ttl.as_secs())?).context("U03 executor token expiry overflow")?;
        let now = crate::util::now_unix();
        let rows = self.store.kv_prefix(TOKENS.into()).await?;
        anyhow::ensure!(rows.len() <= LIMIT, "U03 executor token table exceeds its bound");
        let mut expired = Vec::new();
        for (key, json) in rows {
            if decode::<Ticket>(&json)?.expires <= now { expired.push((key, json)); }
        }
        let token = crate::util::random_token();
        let key = token_key(&token);
        let value = encode(&Ticket { executor: executor.into(), generation: grant.generation.clone(), allocation: grant.allocation.clone(), pod: pod.into(), expires, issued: None })?;
        let grant_key = format!("u01/executor/{executor}");
        let expected = serde_json::to_string(&grant)?;
        self.store.call(move |c| {
            let tx = crate::store::tx(c)?;
            let current: String = tx.query_row("SELECT v FROM kv WHERE k=?1", [&grant_key], |r| r.get(0))?;
            anyhow::ensure!(current == expected, "U03 executor grant changed before token publication");
            for (expired_key, expired_json) in expired {
                tx.execute("DELETE FROM kv WHERE k=?1 AND v=?2", params![expired_key, expired_json])?;
            }
            let count: usize = tx.query_row("SELECT count(*) FROM kv WHERE substr(k,1,length(?1))=?1", [TOKENS], |r| r.get(0))?;
            anyhow::ensure!(count < LIMIT, "U03 executor enrollment token table is full");
            tx.execute("INSERT INTO kv(k,v) VALUES (?1,?2)", params![key, value])?;
            tx.commit()?;
            Ok(())
        }).await?;
        Ok(token)
    }

    /// U03: no token consumption, identity publication or grant update is acknowledged early.
    /// A lost HTTP response can repeat the same CSR and receive the same committed certificate.
    pub(crate) async fn enroll_executor(&self, token: &str, csr: &str) -> anyhow::Result<Option<(ExecutorIdentity, String)>> {
        anyhow::ensure!(token.len() <= 1024, "U03 invalid executor token size");
        let key = token_key(token);
        let Some(json) = self.store.kv_get(key.clone()).await? else { return Ok(None); };
        let mut ticket: Ticket = decode(&json)?;
        anyhow::ensure!(ticket.expires > crate::util::now_unix(), "U03 executor enrollment token expired");
        let k = self.k8s.get().and_then(Option::as_ref).context("U03 executor enrollment requires its Kubernetes API")?;
        let observed = k.executor_bootstrap(&ticket.executor, &ticket.generation, &ticket.allocation).await?.context("U03 executor pod has no verified bootstrap identity")?;
        let identity = observed.identity;
        anyhow::ensure!(same_ticket(&ticket, &identity), "U03 executor enrollment changed pod identity");
        let csr_hash = crate::util::sha256_hex(csr.as_bytes());
        if let Some(issued) = &ticket.issued {
            anyhow::ensure!(issued.identity == identity && issued.csr == csr_hash, "U03 consumed executor token changed identity or CSR");
            self.check_executor_grant(&identity, observed.capacity).await?;
            return Ok(Some((identity, issued.certificate.clone())));
        }
        let grant = crate::ci::dispatch::executor_grant(self, &ticket.executor).await?.context("U03 executor enrollment grant is missing")?;
        let physical = identity.physical_execution()?;
        anyhow::ensure!(grant.generation == identity.generation && grant.allocation == identity.allocation
            && grant.snapshot.runner.is_empty() && matches!(grant.phase.as_str(), "starting" | "ready")
            && grant.physical_execution.as_deref().is_none_or(|old| old == physical), "U03 executor enrollment does not own the existing grant");
        let certificate = self.pki.sign_csr(csr, &identity.name, crate::pki::EPHEMERAL_DAYS)?;
        let issued = Issued { identity: identity.clone(), csr: csr_hash, certificate_sha256: crate::pki::cert_info(&certificate)?.sha256, certificate: certificate.clone() };
        ticket.issued = Some(issued.clone());
        commit_enrollment(&self.store, key, json, ticket, issued, observed.capacity).await?;
        self.pod_executors.lock().insert(identity.name.clone(), identity.executor.clone());
        Ok(Some((identity, certificate)))
    }

    async fn check_executor_grant(&self, identity: &ExecutorIdentity, capacity: PodCapacity) -> anyhow::Result<()> {
        let grant = crate::ci::dispatch::executor_grant(self, &identity.executor).await?.context("U03 executor enrollment grant is no longer present")?;
        anyhow::ensure!(grant.generation == identity.generation && grant.allocation == identity.allocation
            && grant.snapshot.runner.is_empty() && matches!(grant.phase.as_str(), "ready" | "busy")
            && grant.pod_identity.as_ref() == Some(identity)
            && grant.physical_execution.as_deref() == Some(identity.physical_execution()?.as_str()), "U03 executor certificate no longer owns its grant");
        // Reconnect can backfill an old snapshot only from this same authenticated UID.
        // The transaction rechecks ownership/fencing and refuses any captured shape change.
        let identity = identity.clone();
        self.store.call(move |c| {
            let tx = crate::store::tx(c)?;
            crate::ci::dispatch::bind_pod_enrollment(&tx, &identity, &capacity)?;
            tx.commit()?;
            Ok(())
        }).await
    }

    /// U11-F03: Drain is addressed to the certificate of the enrolled UID, never a reused
    /// pod name or whichever runner process happens to own that name when the send completes.
    pub(crate) async fn drain_executor(&self, identity: &ExecutorIdentity) -> anyhow::Result<()> {
        let Some(json) = self.store.kv_get(pod_key(&identity.name)).await? else { return Ok(()); };
        let issued: Issued = decode(&json)?;
        if issued.identity != *identity { return Ok(()); }
        let peer = u64::from_str_radix(issued.certificate_sha256.get(..16).context("U03 invalid certificate receipt")?, 16)?.max(2);
        self.command_peers(&identity.name, vec![peer], &crate::proto::ServerMsg::Drain).await.log_when_answered(super::LOG_WITHIN);
        Ok(())
    }

    // U11-F05: keep the original certificate/index while whole-UID retirement authority
    // is unavailable. The former private status-only retirement helper had no valid caller;
    // a future canonical provider-closure owner must reintroduce exact authenticated cleanup.

    /// U03: a persisted certificate is bound to one API-observed UID/node, not reusable pod name.
    pub(crate) async fn verify_executor_peer(&self, name: &str, peer: hyper_transport::PeerId) -> anyhow::Result<()> {
        let Some(json) = self.store.kv_get(pod_key(name)).await? else {
            anyhow::ensure!(!name.starts_with("kiln-x-"), "U03 executor pod has no durable enrollment");
            return Ok(());
        };
        let issued: Issued = decode(&json)?;
        let expected = u64::from_str_radix(issued.certificate_sha256.get(..16).context("U03 invalid certificate receipt")?, 16)?.max(2);
        anyhow::ensure!(issued.identity.name == name && expected == peer, "U03 executor certificate does not match its physical enrollment");
        let k = self.k8s.get().and_then(Option::as_ref).context("U03 executor reconnect requires its Kubernetes API")?;
        let observed = k.executor_bootstrap(&issued.identity.executor, &issued.identity.generation, &issued.identity.allocation).await?.context("U03 executor pod has no verified bootstrap identity")?;
        anyhow::ensure!(observed.identity == issued.identity, "U03 executor pod UID/node changed since enrollment");
        self.check_executor_grant(&issued.identity, observed.capacity).await?;
        self.pod_executors.lock().insert(name.into(), issued.identity.executor);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capacity() -> PodCapacity {
        PodCapacity { resource_version: "api-42".into(), cpu_request: Some(0.5), memory_request: Some(64), ..PodCapacity::default() }
    }

    async fn fixture() -> anyhow::Result<(crate::store::Store, String, String, Ticket, Issued)> {
        let store = crate::store::Store::in_memory()?;
        let identity = ExecutorIdentity {
            namespace: "jobs".into(), name: "kiln-x-one".into(), uid: "pod-uid".into(), node: "node".into(),
            executor: "one".into(), generation: "generation".into(), allocation: "existing-allocation".into(),
        };
        let grant = serde_json::json!({
            "executor": identity.executor, "generation": identity.generation, "allocation": identity.allocation,
            "snapshot": { "executor": identity.executor, "pool": "pool", "runner": "", "reserved": kiln_ci::sched::state::Resources::default(), "leg": null },
            "physical_execution": null, "phase": "starting", "binding": null, "pod_identity": null,
        });
        store.kv_set("u01/executor/one".into(), grant.to_string()).await?;
        let issued = Issued { identity: identity.clone(), csr: "csr-hash".into(), certificate: "fixture-certificate".into(), certificate_sha256: "a".repeat(64) };
        let mut ticket = Ticket { executor: identity.executor, generation: identity.generation, allocation: identity.allocation, pod: identity.name, expires: i64::MAX, issued: None };
        let key = token_key("fixture-token-not-a-secret");
        let original = encode(&ticket)?;
        store.kv_set(key.clone(), original.clone()).await?;
        ticket.issued = Some(issued.clone());
        Ok((store, key, original, ticket, issued))
    }

    #[test]
    fn u03_executor_enrollment_certificate_and_existing_grant_commit_together() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (store, key, original, ticket, issued) = fixture().await?;
            commit_enrollment(&store, key.clone(), original.clone(), ticket.clone(), issued.clone(), capacity()).await?;
            let stored = store.kv_get(key.clone()).await?.context("issued token missing")?;
            assert_eq!(decode::<Ticket>(&stored)?.issued, Some(issued.clone()));
            let grant: serde_json::Value = serde_json::from_str(&store.kv_get("u01/executor/one".into()).await?.context("grant missing")?)?;
            assert_eq!(grant.get("pod_identity"), Some(&serde_json::to_value(&issued.identity)?));
            assert_eq!(grant.get("phase").and_then(serde_json::Value::as_str), Some("ready"));
            assert_eq!(grant.pointer("/snapshot/shape"), Some(&serde_json::to_value(kiln_ci::sched::pools::ExecutorShape::Kubernetes(capacity()))?), "certificate and admitted capacity commit together");
            assert!(commit_enrollment(&store, key.clone(), original, ticket, issued, capacity()).await.is_err());
            assert_eq!(store.kv_get(key).await?, Some(stored));
            Ok(())
        })
    }

    #[test]
    fn u03_conflicting_pod_receipt_rolls_back_grant_and_token_consumption() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (store, key, original, ticket, issued) = fixture().await?;
            let mut conflicting = issued.clone();
            conflicting.identity.uid = "another-pod-uid".into();
            let prior = encode(&conflicting)?;
            store.kv_set(pod_key(&issued.identity.name), prior.clone()).await?;
            assert!(commit_enrollment(&store, key.clone(), original.clone(), ticket, issued.clone(), capacity()).await.is_err());
            assert_eq!(store.kv_get(key).await?, Some(original));
            assert_eq!(store.kv_get(pod_key(&issued.identity.name)).await?, Some(prior));
            let grant: serde_json::Value = serde_json::from_str(&store.kv_get("u01/executor/one".into()).await?.context("grant missing")?)?;
            assert_eq!(grant.get("phase").and_then(serde_json::Value::as_str), Some("starting"));
            assert_eq!(grant.get("physical_execution"), Some(&serde_json::Value::Null));
            assert!(grant.pointer("/snapshot/shape").is_none(), "conflicting enrollment cannot publish capacity either");
            Ok(())
        })
    }
}
