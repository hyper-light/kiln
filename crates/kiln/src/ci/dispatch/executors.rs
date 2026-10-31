//! U01: ownership journal for executor reservations already placed by Scheduler.
//! It never chooses a host or allocates capacity. Persist before StartExecutor, bind physical
//! identity before assignment, restore the same Scheduler reservation before admitting work.

use serde::{Deserialize, Serialize};
use crate::sql::params;
use crate::hub::Hub;
use kiln_ci::sched::state::ExecutorSnapshot;
use kiln_ci::sched::pools::ExecutorShape;
use kiln_ci::wire::ParentBinding;
use super::{Dispatch, Input, now_ms};

const PREFIX: &str = "u01/executor/";
const LIMIT: usize = 4096;
const RECEIPTS: usize = 16384;

/// Actual authenticated transport provenance supplied by Hub before durable evidence staging.
/// The digest identifies a connection; the raw TLS exporter never enters application records.
#[derive(Debug, Clone)]
pub struct ExecutorOrigin {
    pub(crate) peer: u64,
    pub(crate) connection: String,
    pub(crate) process_nonce: Option<String>,
}

/// Startup intent belongs to this existing grant, not whichever pool configuration is current
/// on replay. The original authenticated peer/process nonce is public identity, never TLS key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecutorStartup {
    pub kind: String,
    pub image: Option<String>,
    pub peer: Option<u64>,
    pub process_nonce: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutorGrant {
    pub executor: String,
    pub generation: String,
    pub allocation: String,
    pub snapshot: ExecutorSnapshot,
    pub physical_execution: Option<String>,
    pub phase: String,
    #[serde(default)]
    pub binding: Option<ParentBinding>,
    #[serde(default)]
    pub pod_identity: Option<crate::k8s::ExecutorIdentity>,
    #[serde(default)]
    pub startup: Option<ExecutorStartup>,
}

/// A successful canonical owner call distinguishes budget refusal BEFORE effects from
/// an outer error/unknown write. The refused input has no durable or external startup permit.
#[derive(Debug)]
pub(super) enum ExecutorGrantAdmission {
    Granted(ExecutorGrant),
    Full(ExecutorGrantFull),
}

#[derive(Debug)]
pub(super) struct ExecutorGrantFull {
    grant: ExecutorGrant,
    grants: usize,
    receipts: usize,
}

/// U11-F05: no boolean/marker/ordinary Pod snapshot can mint the missing canonical
/// authorities. These typed holds remain closed until the original publication owner supplies
/// durable producer closure and a reviewed provider owner supplies whole-UID evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PodRetirementHold {
    OriginalPublicationAndProducerClosureMissing,
    BoundProviderClosureAndSandboxCoverageMissing,
}

impl ExecutorGrant {
    pub(crate) fn stopping(&self) -> bool { matches!(self.phase.as_str(), "stopping" | "retained_stopping") }

    pub(crate) fn pod_retirement_hold(&self) -> Option<PodRetirementHold> {
        if !self.snapshot.runner.is_empty() { return None; }
        Some(if self.pod_identity.is_some() {
            PodRetirementHold::BoundProviderClosureAndSandboxCoverageMissing
        } else {
            PodRetirementHold::OriginalPublicationAndProducerClosureMissing
        })
    }

    /// U11-F03: asynchronous work keeps its original allocation and physical owner. An
    /// enrollment racing an unbound start completion is not authority to adopt its UID.
    pub(crate) fn same_owner(&self, expected: &Self) -> bool {
        self.executor == expected.executor && self.generation == expected.generation && self.allocation == expected.allocation
            && self.snapshot.runner == expected.snapshot.runner && self.physical_execution == expected.physical_execution
            && self.pod_identity == expected.pod_identity
    }

    pub(crate) fn observes(&self, pod: &crate::k8s::ExecutorPod) -> bool {
        self.snapshot.runner.is_empty() && self.pod_identity.as_ref().is_some_and(|owner| pod.observes(owner))
    }

    fn validate(&self, executor: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.executor == executor && self.snapshot.executor == executor && valid(executor) && valid(&self.generation) && valid(&self.allocation)
            && valid(&self.snapshot.pool) && (self.snapshot.runner.is_empty() || valid(&self.snapshot.runner))
            && self.snapshot.reserved.cpu.is_finite() && self.snapshot.reserved.cpu >= 0.0
            && self.physical_execution.as_ref().is_none_or(|p| valid(p))
            && matches!(self.phase.as_str(), "starting" | "ready" | "busy" | "stopping" | "retained" | "retained_stopping" | "stopped"), "U01 invalid executor ownership");
        if let Some(startup) = &self.startup {
            anyhow::ensure!(kiln_ci::sched::pools::KINDS.contains(&startup.kind.as_str()) && startup.image.as_deref().is_none_or(valid), "U01 invalid persisted executor startup");
            if startup.kind == "firecracker" {
                anyhow::ensure!(startup.peer.is_some_and(|peer| peer >= 2) && startup.process_nonce.as_deref().is_some_and(crate::proto::valid_process_nonce)
                    && matches!(&self.snapshot.shape, Some(ExecutorShape::Firecracker(_))), "U01 Firecracker startup lacks its original connection and immutable grant shape");
            } else {
                anyhow::ensure!(startup.peer.is_none() && startup.process_nonce.is_none(), "U01 non-VM startup has a VM process owner");
            }
        }
        if let Some(binding) = &self.binding {
            anyhow::ensure!(binding.executor == self.executor && binding.generation == self.generation && binding.allocation == self.allocation
                && Some(&binding.physical_execution) == self.physical_execution.as_ref() && valid(&binding.job) && binding.attempt > 0
                && self.snapshot.binding.as_ref().is_none_or(|b| b == binding), "U01 executor binding changed its grant");
        }
        if let Some(pod) = &self.pod_identity {
            anyhow::ensure!(self.snapshot.shape.as_ref().is_none_or(|shape| shape.pod().is_some()), "U01 pod grant has another backend shape");
            anyhow::ensure!(self.snapshot.runner.is_empty() && pod.executor == self.executor && pod.generation == self.generation && pod.allocation == self.allocation
                && Some(pod.physical_execution()?) == self.physical_execution, "U01 executor changed immutable pod identity");
        }
        if let Some(shape) = &self.snapshot.shape {
            shape.validate_grant(&self.snapshot.reserved).map_err(anyhow::Error::msg)?;
            if matches!(shape, ExecutorShape::Firecracker(_)) {
                anyhow::ensure!(self.startup.as_ref().is_some_and(|s| s.kind == "firecracker"), "U01 VM shape has no original authenticated startup process");
            }
            match shape {
                ExecutorShape::Kubernetes(_) => anyhow::ensure!(self.snapshot.runner.is_empty() && self.pod_identity.is_some(), "U01 pod capacity has no authenticated server-owned physical grant"),
                ExecutorShape::Firecracker(_) => anyhow::ensure!(!self.snapshot.runner.is_empty() && self.pod_identity.is_none(), "U01 VM shape has no host-owned physical grant"),
            }
        }
        Ok(())
    }
}

fn key(id: &str) -> String { format!("{PREFIX}{id}") }
fn valid(value: &str) -> bool { !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control) }

/// Receipt keys contain only physical ownership identity, never optional diagnostics. They
/// acknowledge an already committed stop and cannot alter any current Scheduler grant.
async fn stopped_received(hub: &Hub, agent: &str, said: &crate::runner::pool::Said) -> anyhow::Result<bool> {
    // Historical pod rows were admitted from status-only observations. They cannot certify
    // original publisher/provider closure or authorize replay as an actionable receipt.
    if agent.is_empty() { return Ok(false); }
    let (Some(generation), Some(allocation), Some(physical)) = (&said.generation, &said.allocation, &said.physical_execution) else { return Ok(false); };
    if ![&said.executor, generation, allocation, physical].iter().all(|s| valid(s)) { return Ok(false); }
    let (executor, generation, allocation, agent, physical) = (said.executor.clone(), generation.clone(), allocation.clone(), agent.to_owned(), physical.clone());
    hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_executor_receipts WHERE executor = ?1 AND generation = ?2 AND allocation = ?3 AND agent = ?4 AND physical_execution = ?5)",
        params![executor, generation, allocation, agent, physical], |r| r.get(0))?)).await
}

pub(crate) async fn executor_grant(hub: &Hub, executor: &str) -> anyhow::Result<Option<ExecutorGrant>> {
    let json = hub.store.kv_get(key(executor)).await?;
    json.map(|s| {
        anyhow::ensure!(s.len() <= 16384, "U01 executor grant exceeds its byte budget");
        let grant: ExecutorGrant = serde_json::from_str(&s)?;
        grant.validate(executor)?;
        Ok(grant)
    }).transpose()
}

/// U11-F03: compare the captured owner inside the existing writer transaction. The boolean
/// reports the first fence only; repeated failures may drive cleanup but never fail startup twice.
async fn stop_exact_executor_grant(hub: &Hub, expected: &ExecutorGrant) -> anyhow::Result<Option<(ExecutorGrant, bool)>> {
    let expected = expected.clone();
    hub.store.call(move |c| {
        use crate::sql::OptionalExtension as _;
        let tx = crate::store::tx(c)?;
        let key = key(&expected.executor);
        let Some(json) = tx.query_row("SELECT v FROM kv WHERE k=?1", [&key], |r| r.get::<_, String>(0)).optional()? else { return Ok(None); };
        anyhow::ensure!(json.len() <= 16384, "U01 executor grant exceeds its byte budget");
        let mut grant: ExecutorGrant = serde_json::from_str(&json)?;
        grant.validate(&expected.executor)?;
        if !grant.same_owner(&expected) || (grant.phase == "stopped" && !grant.snapshot.runner.is_empty()) { return Ok(None); }
        let first = !grant.stopping();
        if first {
            // A pre-fix stopped row may have transferred its sole capacity owner already.
            // Preserve that existing exact retained owner instead of restoring a duplicate.
            let mut retained = grant.phase == "retained";
            if grant.phase == "stopped" && grant.snapshot.runner.is_empty()
                && let Some(binding) = &grant.binding {
                    let allocation: Option<String> = tx.query_row("SELECT allocation FROM ci_retained_ownership WHERE job=?1 AND attempt=?2 AND transferred=1 AND released=0", params![binding.job, binding.attempt], |row| row.get(0)).optional()?;
                    if let Some(json) = allocation {
                        anyhow::ensure!(json.len() <= 16384, "U01 retained allocation exceeds its byte budget");
                        let allocation: kiln_ci::sched::state::Allocation = serde_json::from_str(&json)?;
                        retained = allocation.parent_binding.as_ref() == Some(binding);
                    }
            }
            grant.phase = if retained { "retained_stopping" } else { "stopping" }.into();
            let updated = serde_json::to_string(&grant)?;
            anyhow::ensure!(updated.len() <= 16384, "U01 stopping grant exceeds its byte budget");
            let changed = tx.execute("UPDATE kv SET v=?2 WHERE k=?1 AND v=?3", params![key, updated, json])?;
            anyhow::ensure!(changed == 1, "U11 captured executor fence lost its compare-and-set");
        }
        tx.commit()?;
        Ok(Some((grant, first)))
    }).await
}

/// Fence the existing original pod grant by exact tuple/UID and serialized compare-and-set.
/// An old status-only stopped row is quarantined as stopping, never silently pruned on reopen.
/// This operation retains every byte of physical debt and supplies no producer/provider proof.
pub(crate) async fn hold_pod_retirement(hub: &Hub, expected: &ExecutorGrant) -> anyhow::Result<Option<ExecutorGrant>> {
    anyhow::ensure!(expected.snapshot.runner.is_empty(), "U11 pod retirement hold cannot own a host executor");
    Ok(stop_exact_executor_grant(hub, expected).await?.map(|(grant, _)| grant))
}

/// U01: call inside the enrollment certificate/token transaction. It deliberately does not
/// start or commit a transaction: grant identity and enrollment receipt commit together.
pub(crate) fn bind_pod_enrollment(conn: &crate::sql::Connection<'_>, identity: &crate::k8s::ExecutorIdentity, capacity: &kiln_ci::sched::pools::PodCapacity) -> anyhow::Result<ExecutorGrant> {
    anyhow::ensure!([&identity.namespace, &identity.name, &identity.uid, &identity.node, &identity.executor, &identity.generation, &identity.allocation].iter().all(|s| valid(s)), "U01 invalid pod execution identity");
    capacity.validate().map_err(anyhow::Error::msg)?;
    let key = key(&identity.executor);
    let json: String = conn.query_row("SELECT v FROM kv WHERE k = ?1", [&key], |r| r.get(0))?;
    let mut grant: ExecutorGrant = serde_json::from_str(&json)?;
    grant.validate(&identity.executor)?;
    anyhow::ensure!(grant.executor == identity.executor && grant.generation == identity.generation && grant.allocation == identity.allocation && grant.snapshot.runner.is_empty(), "U01 pod enrollment does not own its existing grant");
    anyhow::ensure!(matches!(grant.phase.as_str(), "starting" | "ready" | "busy"), "U01 pod enrollment has already been fenced");
    let physical = identity.physical_execution()?;
    anyhow::ensure!(grant.physical_execution.as_ref().is_none_or(|old| old == &physical) && grant.pod_identity.as_ref().is_none_or(|old| old == identity), "U01 pod enrollment changed immutable physical ownership");
    anyhow::ensure!(grant.snapshot.shape.as_ref().is_none_or(|old| old.pod().is_some_and(|old| old.same_resources(capacity))), "U01 admitted pod capacity changed under the same physical grant");
    grant.snapshot.shape.get_or_insert_with(|| ExecutorShape::Kubernetes(capacity.clone()));
    grant.physical_execution = Some(physical);
    grant.pod_identity = Some(identity.clone());
    if grant.phase == "starting" { grant.phase = "ready".into(); }
    grant.validate(&identity.executor)?;
    let updated = serde_json::to_string(&grant)?;
    anyhow::ensure!(updated.len() <= 16384, "U01 admitted pod grant exceeds its byte budget");
    conn.execute("UPDATE kv SET v = ?2 WHERE k = ?1", params![key, updated])?;
    Ok(grant)
}

/// Server-owned pod startup uses this before publishing the pod's executor runner. The caller
/// supplies its exact immutable PodUID/node physical identity, never an API absence inference.
/// Runner startup uses the same function after authenticating Said's assigned host.
pub(crate) async fn bind_executor_physical(hub: &Hub, expected: &ExecutorGrant, host: &str, physical_execution: &str, shape: Option<&ExecutorShape>, process_nonce: Option<&str>, peer: Option<u64>) -> anyhow::Result<ExecutorGrant> {
    anyhow::ensure!(valid(physical_execution), "U01 invalid physical execution identity");
    let shape = shape.cloned();
    let process_nonce = process_nonce.map(str::to_owned);
    let (key, executor, generation, allocation, host, physical) = (key(&expected.executor), expected.executor.clone(), expected.generation.clone(), expected.allocation.clone(), host.to_owned(), physical_execution.to_owned());
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let json: String = tx.query_row("SELECT v FROM kv WHERE k = ?1", [&key], |r| r.get(0))?;
        let mut grant: ExecutorGrant = serde_json::from_str(&json)?;
        anyhow::ensure!(grant.executor == executor && grant.generation == generation && grant.allocation == allocation && grant.snapshot.runner == host, "U01 executor startup does not own this grant");
        anyhow::ensure!(matches!(grant.phase.as_str(), "starting" | "ready" | "busy"), "U01 executor startup has already been fenced");
        grant.validate(&executor)?;
        anyhow::ensure!(grant.snapshot.shape == shape, "U01 Ready changed or omitted its persisted backend shape");
        if let Some(startup) = grant.startup.as_ref().filter(|s| s.kind == "firecracker") {
            anyhow::ensure!(startup.process_nonce == process_nonce && startup.peer == peer, "U01 Ready belongs to another or missing authenticated runner process");
        }
        anyhow::ensure!(grant.physical_execution.as_deref().is_none_or(|old| old == physical), "U01 executor changed physical owner");
        grant.physical_execution = Some(physical);
        if grant.phase == "starting" { grant.phase = "ready".into(); }
        tx.execute("UPDATE kv SET v = ?2 WHERE k = ?1", params![key, serde_json::to_string(&grant)?])?;
        tx.commit()?;
        Ok(grant)
    }).await
}

/// A kernel/PodUID stop proof is durable before any allocation is released or ACK sent.
/// The transport adapter authenticates `host`; internal pod callers use the server host "".
pub(crate) async fn confirm_executor_stopped(hub: &Hub, executor: &str, generation: &str, allocation: &str, host: &str, physical_execution: &str) -> anyhow::Result<ExecutorGrant> {
    anyhow::ensure!(valid(physical_execution), "U01 stopped proof lacks physical execution identity");
    let (key, generation, allocation, host, physical) = (key(executor), generation.to_owned(), allocation.to_owned(), host.to_owned(), physical_execution.to_owned());
    hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let json: String = tx.query_row("SELECT v FROM kv WHERE k = ?1", [&key], |r| r.get(0))?;
        let mut grant: ExecutorGrant = serde_json::from_str(&json)?;
        grant.validate(&grant.executor)?;
        anyhow::ensure!(grant.generation == generation && grant.allocation == allocation && grant.snapshot.runner == host, "U01 stopped proof does not own this executor");
        // The final canonical writer gate covers direct callbacks, historical rows and replay,
        // not only the reaper. Latest statuses, DELETE acceptance and 404 are not retirement.
        anyhow::ensure!(grant.pod_retirement_hold().is_none(), "U11 pod retirement has no authoritative original publication/producer closure and complete provider proof: {:?}", grant.pod_retirement_hold());
        anyhow::ensure!(grant.physical_execution.as_deref().is_none_or(|old| old == physical), "U01 stopped proof changed physical owner");
        let accepted: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_executor_receipts WHERE executor = ?1 AND generation = ?2 AND allocation = ?3 AND agent = ?4 AND physical_execution = ?5)",
            params![grant.executor, generation, allocation, host, physical], |r| r.get(0))?;
        if !accepted {
            let count: usize = tx.query_row("SELECT count(*) FROM ci_executor_receipts", [], |r| r.get(0))?;
            anyhow::ensure!(count < RECEIPTS, "U01 executor receipt budget exhausted");
            tx.execute("INSERT INTO ci_executor_receipts (executor,generation,allocation,agent,physical_execution) VALUES (?1,?2,?3,?4,?5)", params![grant.executor, generation, allocation, host, physical])?;
        }
        grant.physical_execution = Some(physical);
        grant.phase = "stopped".into();
        tx.execute("UPDATE kv SET v = ?2 WHERE k = ?1", params![key, serde_json::to_string(&grant)?])?;
        tx.commit()?;
        Ok(grant)
    }).await
}

impl Dispatch {
    async fn cleanup_historical_executor(&self, agent: &str, said: &crate::runner::pool::Said, origin: Option<&ExecutorOrigin>) -> anyhow::Result<()> {
        if said.stop_proof.is_some() { return Ok(()); }
        let Some(origin) = origin else { return Ok(()); };
        let Some(nonce) = said.process_nonce.as_deref() else { return Ok(()); };
        anyhow::ensure!(crate::proto::valid_process_nonce(nonce) && origin.process_nonce.as_deref() == Some(nonce) && valid(&origin.connection),
            "U03 historical cleanup hint has no authenticated current process identity");
        let target = self.hub.capture_original_runner(agent, origin.peer, nonce, u64::from(crate::proto::CAP_POOLS) | crate::proto::CAP_FIRECRACKER_SHAPE | crate::proto::CAP_WIRE)
            .map_err(|error| anyhow::anyhow!("U03 closed historical owner is not currently reachable: {error:?}"))?;
        let ask = serde_json::json!({ "op": "stop", "kind": "firecracker", "executor": said.executor,
            "generation": said.generation, "allocation": said.allocation, "process_nonce": nonce });
        // This exact tuple was already durably stopped. The current closed-map hint may
        // request only its ordered cleanup barrier; it cannot recreate a grant or VM.
        self.hub.send_captured_runner(&target, &crate::proto::ServerMsg::Executor { json: ask.to_string() }).await?.log_when_answered(crate::hub::LOG_WITHIN);
        Ok(())
    }

    fn executor_failure_commands(&mut self, grant: &ExecutorGrant, error: String) -> Vec<super::Command> {
        let mut commands = self.sched.handle(Input::ExecutorFailed { executor: grant.executor.clone(), error }, now_ms());
        // Scheduler may already have transferred capacity to the retained allocation. The
        // same durable grant still owns physical cleanup and must order its Stop barrier.
        if !commands.iter().any(|command| matches!(command, super::Command::StopExecutor { executor, .. } if executor == &grant.executor)) {
            commands.push(super::Command::StopExecutor { executor: grant.executor.clone(), runner: grant.snapshot.runner.clone() });
        }
        commands
    }

    fn observe_executor_command(&self, grant: &ExecutorGrant, operation: &str, pending: crate::hub::Pending) {
        let (hub, executor, generation, allocation, operation) = (std::sync::Arc::clone(&self.hub), grant.executor.clone(), grant.generation.clone(), grant.allocation.clone(), operation.to_owned());
        crate::rt::spawn(async move {
            let answer = pending.answer(crate::hub::LOG_WITHIN).await;
            // Taken means synchronous queue acceptance only. Native Ready and physical stop
            // still require their independently staged evidence; never infer them from ACK.
            if !answer.taken() { hub.ci.send(super::Msg::ExecutorCommand { executor, generation, allocation, operation, answer }); }
        });
    }

    pub(super) async fn executor_command_answer(&mut self, executor: &str, generation: &str, allocation: &str, operation: &str, answer: crate::wire::commands::Answer) -> anyhow::Result<()> {
        let Some(grant) = executor_grant(&self.hub, executor).await? else { return Ok(()); };
        if grant.generation != generation || grant.allocation != allocation { return Ok(()); }
        tracing::warn!(executor, operation, answer = %answer.words(), "U01 executor command was not accepted; physical ownership remains fenced");
        if operation == "start" && matches!(grant.phase.as_str(), "starting" | "ready") && grant.binding.is_none() {
            let Some((grant, first)) = stop_exact_executor_grant(&self.hub, &grant).await? else { return Ok(()); };
            if !first { return Ok(()); }
            let commands = self.executor_failure_commands(&grant, format!("startup command {}", answer.words()));
            if let Some(commands) = self.after(commands) { Box::pin(self.commands(commands)).await; }
        } else if grant.stopping() {
            use crate::wire::{commands::Answer, channel::Refusal};
            // A queue-capacity refusal/unknown delivery can recover after actual progress.
            // Malformed/unsupported/stale identities require corrective action, not a loop.
            if matches!(answer, Answer::Unreachable { .. } | Answer::Unanswered { .. } | Answer::Refused { code: Refusal::Refused, .. }) {
                let (hub, executor, generation, allocation) = (std::sync::Arc::clone(&self.hub), executor.to_owned(), generation.to_owned(), allocation.to_owned());
                crate::rt::spawn(async move {
                    let delay = std::time::Duration::from_millis(kiln_ci::sched::pools::FIRST_BACKOFF_MS.unsigned_abs());
                    if hub.shutdown.sleep(delay).await { hub.ci.send(super::Msg::ExecutorRetry { executor, generation, allocation }); }
                });
            }
        }
        Ok(())
    }

    pub(super) async fn retry_executor_intent(&mut self, executor: &str, generation: &str, allocation: &str) -> anyhow::Result<()> {
        let Some(grant) = executor_grant(&self.hub, executor).await? else { return Ok(()); };
        if grant.generation != generation || grant.allocation != allocation { return Ok(()); }
        if grant.stopping() {
            self.stop_host_executor(executor, &grant.snapshot.runner).await?;
        }
        Ok(())
    }

    /// Start/replay one existing host grant. Selection is connection-bound, while the durable
    /// owner is the original certificate peer plus its once-per-process authenticated nonce.
    pub(super) async fn start_host_executor(&mut self, pool: &str, executor: &str, runner: &str, kind: &str, image: Option<&str>) -> anyhow::Result<()> {
        let existing = executor_grant(&self.hub, executor).await?;
        let startup = existing.as_ref().and_then(|g| g.startup.clone());
        let vm = startup.as_ref().is_some_and(|s| s.kind == "firecracker")
            || existing.as_ref().is_some_and(|g| matches!(&g.snapshot.shape, Some(ExecutorShape::Firecracker(_)))) || kind == "firecracker";
        let captured = if vm {
            let target = match (&existing, &startup) {
                (Some(grant), Some(startup)) => {
                    anyhow::ensure!(grant.snapshot.runner == runner && startup.kind == "firecracker", "U01 replay changed its existing backend/host");
                    let peer = startup.peer.ok_or_else(|| anyhow::anyhow!("U01 startup has no original authenticated runner peer; retain its grant until stop proof"))?;
                    let nonce = startup.process_nonce.as_deref().ok_or_else(|| anyhow::anyhow!("U01 startup has no original process generation; retain its grant until stop proof"))?;
                    self.hub.capture_original_runner(runner, peer, nonce, u64::from(crate::proto::CAP_POOLS) | crate::proto::CAP_FIRECRACKER_SHAPE | crate::proto::CAP_WIRE)
                }
                (Some(_), None) => anyhow::bail!("U01 legacy VM cannot replay without original process ownership and shape; stop/reconcile its existing grant before replacement"),
                (None, _) => self.hub.capture_newest_runner(runner, u64::from(crate::proto::CAP_POOLS) | crate::proto::CAP_FIRECRACKER_SHAPE | crate::proto::CAP_WIRE),
            };
            match target {
                Ok(target) => Some(target),
                Err(error) => {
                    if existing.is_none() {
                        // No startup has been admitted or sent: Scheduler still owns a logical
                        // unprotected reservation, which its existing failure path can release.
                        self.next.extend(self.sched.handle(Input::ExecutorFailed { executor: executor.into(), error: format!("shape-aware process unavailable before startup: {error:?}") }, now_ms()));
                    }
                    anyhow::bail!("U01 original shape-aware runner unavailable; existing physical grants stay owned: {error:?}");
                }
            }
        } else { None };
        let startup = startup.unwrap_or_else(|| ExecutorStartup { kind: kind.into(), image: image.map(str::to_owned),
            peer: captured.as_ref().map(|target| target.peer()), process_nonce: captured.as_ref().map(|target| target.process_nonce().to_owned()) });
        let grant = match self.start_executor_grant(executor, startup.clone()).await {
            Ok(ExecutorGrantAdmission::Granted(grant)) => grant,
            Ok(ExecutorGrantAdmission::Full(refused)) => return self.refuse_executor_grant(refused),
            Err(error) => { self.admission.ready = false; return Err(error.context("U01 executor startup ownership could not commit; resource admission is fenced")); }
        };
        anyhow::ensure!(grant.snapshot.pool == pool && grant.snapshot.runner == runner, "U01 startup changed its existing pool/host");
        let mut ask = super::super::pools::start_ask(&self.hub, &grant.snapshot.pool, executor, &startup.kind, startup.image.as_deref());
        let fields = ask.as_object_mut().ok_or_else(|| anyhow::anyhow!("U01 startup envelope is not an object"))?;
        fields.insert("generation".into(), serde_json::json!(grant.generation));
        fields.insert("allocation".into(), serde_json::json!(grant.allocation));
        fields.insert("shape".into(), serde_json::to_value(&grant.snapshot.shape)?);
        fields.insert("process_nonce".into(), serde_json::json!(startup.process_nonce));
        let msg = crate::proto::ServerMsg::Executor { json: ask.to_string() };
        if let Some(target) = captured {
            let pending = self.hub.send_captured_runner(&target, &msg).await?;
            self.observe_executor_command(&grant, "start", pending);
        } else {
            self.hub.send_to_agent(runner, crate::proto::LANE_CONTROL, &msg).await;
        }
        Ok(())
    }

    pub(super) async fn stop_host_executor(&mut self, executor: &str, runner: &str) -> anyhow::Result<()> {
        let grant = match self.stop_executor_grant(executor).await {
            Ok(grant) => grant,
            Err(error) => { self.admission.ready = false; return Err(error.context("U01 executor stop could not persist; admission is fenced")); }
        };
        let startup = grant.as_ref().and_then(|g| g.startup.as_ref()).filter(|s| s.kind == "firecracker");
        let mut ask = serde_json::json!({ "op": "stop", "executor": executor, "generation": grant.as_ref().map(|g| &g.generation), "allocation": grant.as_ref().map(|g| &g.allocation) });
        if let Some(startup) = startup {
            let fields = ask.as_object_mut().ok_or_else(|| anyhow::anyhow!("U01 stop envelope is not an object"))?;
            fields.insert("kind".into(), serde_json::json!(startup.kind));
            fields.insert("process_nonce".into(), serde_json::json!(startup.process_nonce));
        }
        let msg = crate::proto::ServerMsg::Executor { json: ask.to_string() };
        if let Some(startup) = startup {
            let peer = startup.peer.ok_or_else(|| anyhow::anyhow!("U01 stop has no original authenticated runner peer"))?;
            let nonce = startup.process_nonce.as_deref().ok_or_else(|| anyhow::anyhow!("U01 stop has no original runner process generation"))?;
            let target = self.hub.capture_original_runner(runner, peer, nonce, u64::from(crate::proto::CAP_POOLS) | crate::proto::CAP_FIRECRACKER_SHAPE | crate::proto::CAP_WIRE)
                .map_err(|error| anyhow::anyhow!("U01 original process is absent; stop intent and reservation remain fenced: {error:?}"))?;
            let pending = self.hub.send_captured_runner(&target, &msg).await?;
            let grant = grant.as_ref().ok_or_else(|| anyhow::anyhow!("U01 captured stop lost its durable grant"))?;
            self.observe_executor_command(grant, "stop", pending);
        } else {
            // Legacy records have no inventible process generation. This is only a stop request;
            // their reservation still waits for the exact original physical proof.
            self.hub.send_to_agent(runner, crate::proto::LANE_CONTROL, &msg).await;
        }
        Ok(())
    }

    /// Reconnect repeats only phase-authorized FC intent to its original authenticated process.
    /// A replacement process is never authorized by an empty local Startup map.
    pub(super) async fn replay_vm_intents(&mut self, runner: &str) -> anyhow::Result<()> {
        let rows = self.hub.store.kv_prefix(PREFIX.into()).await?;
        anyhow::ensure!(rows.len() <= LIMIT, "U01 executor ownership table is full");
        for (saved_key, json) in rows {
            anyhow::ensure!(json.len() <= 16384, "U01 executor replay metadata exceeds its byte budget");
            let grant: ExecutorGrant = serde_json::from_str(&json)?;
            grant.validate(&grant.executor)?;
            anyhow::ensure!(saved_key == key(&grant.executor), "U01 executor replay key changed");
            if grant.snapshot.runner != runner { continue; }
            let Some(startup) = grant.startup.as_ref().filter(|s| s.kind == "firecracker") else { continue; };
            let result = if grant.stopping() { self.stop_host_executor(&grant.executor, runner).await }
                else if matches!(grant.phase.as_str(), "starting" | "ready") && grant.binding.is_none() {
                    self.start_host_executor(&grant.snapshot.pool, &grant.executor, runner, &startup.kind, startup.image.as_deref()).await
                } else { continue; };
            if let Err(error) = result {
                tracing::warn!(executor = %grant.executor, error = %format!("{error:#}"), "U01 original VM intent remains owned and fenced");
            }
        }
        Ok(())
    }

    pub(super) async fn restore_executors(&mut self) -> anyhow::Result<()> {
        let rows = self.hub.store.kv_prefix(PREFIX.into()).await?;
        anyhow::ensure!(rows.len() <= LIMIT, "U01 executor ownership table is full");
        // Read raw ownership declarations too: an invalid legacy FC configuration must not
        // make its old fixed-size executor look like an unrelated usable host backend.
        let vm_pools: std::collections::HashSet<String> = super::super::pools::specs(&self.hub).await.into_iter()
            .filter(|p| p.executor == "firecracker").map(|p| p.name).collect();
        for (key, json) in rows {
            anyhow::ensure!(json.len() <= 16384, "U01 executor ownership exceeds its byte budget");
            let mut grant: ExecutorGrant = serde_json::from_str(&json)?;
            grant.validate(&grant.executor)?;
            anyhow::ensure!(key == self::key(&grant.executor), "U01 executor ownership key changed");
            if grant.snapshot.binding.is_none() { grant.snapshot.binding = grant.binding.clone(); }
            if grant.phase == "stopped" && grant.snapshot.runner.is_empty() {
                // Older status-only pod receipts are not whole-UID closure. Reuse the exact
                // grant CAS and restore its original reservation instead of failing all startup
                // or deleting physical debt. Already-pruned history cannot be adopted by name.
                let Some(held) = hold_pod_retirement(&self.hub, &grant).await? else { continue; };
                grant = held;
                if grant.snapshot.binding.is_none() { grant.snapshot.binding = grant.binding.clone(); }
            }
            if grant.phase == "stopped" {
                let physical = grant.physical_execution.as_deref().ok_or_else(|| anyhow::anyhow!("U01 stopped executor lacks physical proof identity"))?;
                confirm_executor_stopped(&self.hub, &grant.executor, &grant.generation, &grant.allocation, &grant.snapshot.runner, physical).await?;
                if let Some(binding) = &grant.binding { self.retained_parent_stopped(binding).await?; }
                self.hub.store.kv_delete(self::key(&grant.executor)).await?;
                continue;
            }
            if grant.snapshot.runner.is_empty() {
                // Server-owned pod cleanup keeps its existing controller after recovery.
                self.pods.insert(grant.executor.clone());
            }
            if matches!(grant.phase.as_str(), "retained" | "retained_stopping") {
                // Retained transfer is already the sole capacity owner. Preserve an ordered
                // cleanup intent without reconstructing an executor reservation beside it.
                if grant.stopping() { self.next.push(super::Command::StopExecutor { executor: grant.executor.clone(), runner: grant.snapshot.runner.clone() }); }
                continue;
            }
            if grant.stopping() && grant.snapshot.runner.is_empty() {
                self.next.push(super::Command::StopExecutor { executor: grant.executor.clone(), runner: String::new() });
            }
            let legacy_vm = grant.snapshot.shape.is_none() && vm_pools.contains(&grant.snapshot.pool);
            if legacy_vm && grant.binding.is_none() {
                // No new defaults or resize: unused legacy VMs retire under the original grant.
                // Busy/bound work keeps its existing owner until the normal completion boundary.
                grant.phase = "stopping".into();
                self.hub.store.kv_set(self::key(&grant.executor), serde_json::to_string(&grant)?).await?;
                self.next.push(super::Command::StopExecutor { executor: grant.executor.clone(), runner: grant.snapshot.runner.clone() });
            }
            let stopping = grant.stopping();
            self.sched.restore_executor(grant.snapshot, stopping, now_ms());
        }
        Ok(())
    }

    pub(super) async fn start_executor_grant(&mut self, executor: &str, startup: ExecutorStartup) -> anyhow::Result<ExecutorGrantAdmission> {
        if let Some(grant) = executor_grant(&self.hub, executor).await? {
            anyhow::ensure!(matches!(grant.phase.as_str(), "starting" | "ready") && grant.binding.is_none(), "U01 executor startup was already assigned or fenced");
            anyhow::ensure!(self.sched.protect_executor(executor), "U01 replay has no restored Scheduler reservation");
            return Ok(ExecutorGrantAdmission::Granted(grant));
        }
        let snapshot = self.sched.executor_snapshot(executor).ok_or_else(|| anyhow::anyhow!("U01 StartExecutor has no Scheduler reservation"))?;
        let grant = ExecutorGrant { executor: executor.into(), generation: crate::util::new_id(), allocation: format!("executor-{}", crate::util::new_id()), snapshot,
            physical_execution: None, phase: "starting".into(), binding: None, pod_identity: None, startup: Some(startup) };
        grant.validate(executor)?;
        let (key, json) = (key(executor), serde_json::to_string(&grant)?);
        anyhow::ensure!(json.len() <= 16384, "U01 executor grant exceeds its byte budget");
        let admission = self.hub.store.call(move |c| {
            // Before any mutating SQL/start permit, recheck this exact key inside the same
            // atomic owner session. The earlier async read alone is not an absence proof.
            let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM kv WHERE k=?1)", [&key], |row| row.get(0))?;
            anyhow::ensure!(!exists, "U11 executor owner appeared during startup preflight; keep admission fenced");
            let count: usize = c.query_row("SELECT count(*) FROM kv WHERE substr(k,1,length(?1)) = ?1", [PREFIX], |r| r.get(0))?;
            let receipts: usize = c.query_row("SELECT count(*) FROM ci_executor_receipts", [], |r| r.get(0))?;
            // A successful outer call alone may return this owned definite refusal. SQL,
            // INSERT, commit and outer/unknown failures never become safe no-start evidence.
            if count >= LIMIT || count.saturating_add(receipts) >= RECEIPTS {
                return Ok(ExecutorGrantAdmission::Full(ExecutorGrantFull { grant, grants: count, receipts }));
            }
            let tx = crate::store::tx(c)?;
            tx.execute("INSERT INTO kv(k,v) VALUES (?1,?2)", params![key,json])?;
            tx.commit()?;
            Ok(ExecutorGrantAdmission::Granted(grant))
        }).await?;
        if let ExecutorGrantAdmission::Granted(_) = &admission {
            anyhow::ensure!(self.sched.protect_executor(executor), "U01 executor reservation disappeared before startup");
        }
        Ok(admission)
    }

    pub(super) fn refuse_executor_grant(&mut self, refused: ExecutorGrantFull) -> anyhow::Result<()> {
        let error = format!("executor ownership budget is full ({} grants, {} receipts); new startup waits", refused.grants, refused.receipts);
        let Some(commands) = self.sched.refuse_executor_start(&refused.grant.snapshot, error.clone(), now_ms()) else {
            self.admission.ready = false;
            anyhow::bail!("U11 definite startup refusal no longer owns an unprotected reservation; admission is fenced");
        };
        // Existing pool backoff/queued reasons retry only this new, never-sent logical start.
        // There is no stored grant, stopped receipt, NoStart or physical refund here.
        self.next.extend(commands);
        tracing::warn!(executor = %refused.grant.executor, pool = %refused.grant.snapshot.pool, %error, "new executor startup refused before ownership admission");
        Ok(())
    }

    pub(super) async fn stop_executor_grant(&mut self, executor: &str) -> anyhow::Result<Option<ExecutorGrant>> {
        let Some(expected) = executor_grant(&self.hub, executor).await? else { return Ok(None); };
        if expected.phase == "stopped" { return Ok(Some(expected)); }
        let stopped = stop_exact_executor_grant(&self.hub, &expected).await?
            .ok_or_else(|| anyhow::anyhow!("U11 executor stop no longer owns its captured grant"))?;
        Ok(Some(stopped.0))
    }

    pub(super) async fn executor_parent(&mut self, executor: &str, job: &str, attempt: u32) -> anyhow::Result<ParentBinding> {
        let mut grant = executor_grant(&self.hub, executor).await?.ok_or_else(|| anyhow::anyhow!("U01 executor assignment has no durable grant"))?;
        anyhow::ensure!(matches!(grant.phase.as_str(), "ready" | "busy"), "U01 executor is not ready under its existing grant");
        let physical_execution = grant.physical_execution.clone().ok_or_else(|| anyhow::anyhow!("U01 executor has no authenticated physical owner"))?;
        let binding = ParentBinding { executor: grant.executor.clone(), generation: grant.generation.clone(), allocation: grant.allocation.clone(), physical_execution, job: job.into(), attempt };
        anyhow::ensure!(grant.binding.as_ref().is_none_or(|old| old == &binding), "U01 executor belongs to another job attempt");
        anyhow::ensure!(self.sched.bind_parent(binding.clone()), "U01 physical owner does not match Scheduler assignment");
        grant.binding = Some(binding.clone());
        grant.snapshot.binding = Some(binding.clone());
        grant.phase = "busy".into();
        self.hub.store.kv_set(key(executor), serde_json::to_string(&grant)?).await?;
        Ok(binding)
    }

    pub(super) async fn executor_event(&mut self, agent: &str, said: crate::runner::pool::Said) -> anyhow::Result<()> {
        self.executor_event_from(agent, said, None).await
    }

    pub(super) async fn executor_event_from(&mut self, agent: &str, said: crate::runner::pool::Said, connection: Option<&ExecutorOrigin>) -> anyhow::Result<()> {
        let historical = said.state == "stopped" && stopped_received(&self.hub, agent, &said).await?;
        let Some(grant) = executor_grant(&self.hub, &said.executor).await? else {
            anyhow::ensure!(historical, "U01 unknown executor event or stopped ownership receipt");
            self.cleanup_historical_executor(agent, &said, connection).await?;
            return Ok(());
        };
        if historical && (grant.snapshot.runner != agent || said.generation.as_deref() != Some(grant.generation.as_str())
            || said.allocation.as_deref() != Some(grant.allocation.as_str()) || said.physical_execution != grant.physical_execution) {
            // An old exact receipt may share an executor's reused name. Acknowledge only
            // its history; the current generation's allocation is completely untouched.
            self.cleanup_historical_executor(agent, &said, connection).await?;
            return Ok(());
        }
        // U11-F03: delayed local pod completions are observations of their original tuple.
        // Never restamp one with the current grant or let a name-reused UID fail its successor.
        if agent.is_empty() && grant.snapshot.runner.is_empty()
            && (said.generation.as_deref() != Some(grant.generation.as_str()) || said.allocation.as_deref() != Some(grant.allocation.as_str())
                || said.physical_execution != grant.physical_execution) { return Ok(()); }
        anyhow::ensure!(grant.snapshot.runner == agent && said.generation.as_deref() == Some(grant.generation.as_str()) && said.allocation.as_deref() == Some(grant.allocation.as_str()), "U01 executor event does not own this grant");
        let commands = match said.state.as_str() {
            "ready" => {
                if matches!(&grant.snapshot.shape, Some(ExecutorShape::Firecracker(_))) {
                    let origin = connection.ok_or_else(|| anyhow::anyhow!("U01 VM Ready has no authenticated frame-origin connection"))?;
                    anyhow::ensure!(valid(&origin.connection) && origin.process_nonce == said.process_nonce && grant.startup.as_ref().is_some_and(|s| s.peer == Some(origin.peer)),
                        "U01 VM Ready belongs to another authenticated runner peer");
                } else if grant.snapshot.shape.is_none() {
                    anyhow::ensure!(!super::super::pools::specs(&self.hub).await.iter().any(|p| p.name == grant.snapshot.pool && p.executor == "firecracker"),
                        "U01 legacy VM Ready lacks exact shape/process proof; reconcile its existing grant before replacement");
                }
                let physical = said.physical_execution.as_deref().ok_or_else(|| anyhow::anyhow!("U01 Ready lacks physical execution identity"))?;
                bind_executor_physical(&self.hub, &grant, agent, physical, said.shape.as_ref(), said.process_nonce.as_deref(), connection.map(|origin| origin.peer)).await?;
                self.sched.handle(Input::ExecutorReady { executor: said.executor }, now_ms())
            }
            "stopping" => {
                anyhow::ensure!(grant.stopping() && said.stop_proof.is_none()
                    && said.physical_execution.as_deref().is_some_and(valid)
                    && grant.physical_execution.as_ref().is_none_or(|physical| Some(physical) == said.physical_execution.as_ref()),
                    "U01 pending stop changed its physical identity or stopping phase");
                if grant.startup.as_ref().is_some_and(|startup| startup.kind == "firecracker")
                    || matches!(&grant.snapshot.shape, Some(ExecutorShape::Firecracker(_))) {
                    let origin = connection.ok_or_else(|| anyhow::anyhow!("U01 pending VM stop has no authenticated frame origin"))?;
                    anyhow::ensure!(valid(&origin.connection) && grant.startup.as_ref().is_some_and(|startup| startup.peer == Some(origin.peer)
                        && startup.process_nonce == origin.process_nonce && startup.process_nonce == said.process_nonce),
                        "U01 pending VM stop does not own its original process");
                } else if let Some(nonce) = &said.process_nonce {
                    // Docker keeps its existing authenticated-machine grant contract. New
                    // process hints still prove their actual connection; no pending hint
                    // changes capacity or supplies a physical-release proof.
                    anyhow::ensure!(connection.is_some_and(|origin| valid(&origin.connection) && origin.process_nonce.as_ref() == Some(nonce)),
                        "U01 pending stop has no matching authenticated process declaration");
                }
                // Retained debug/daemon groups can keep the durable Gate pending after stop().
                // Keep the grant and use the existing delayed Stop retry, never an immediate
                // ExecutorFailed loop or a fictitious stopped receipt.
                self.executor_command_answer(&said.executor, &grant.generation, &grant.allocation, "stop",
                    crate::wire::commands::Answer::Refused { code: crate::wire::channel::Refusal::Refused,
                        why: said.error.unwrap_or_else(|| "physical retirement is still pending retained cleanup".into()) }).await?;
                Vec::new()
            }
            "stopped" => {
                let physical = said.physical_execution.as_deref().ok_or_else(|| anyhow::anyhow!("U01 stopped proof lacks physical identity"))?;
                let vm = grant.startup.as_ref().is_some_and(|s| s.kind == "firecracker")
                    || matches!(&grant.snapshot.shape, Some(ExecutorShape::Firecracker(_)));
                if vm {
                    anyhow::ensure!(grant.physical_execution.as_deref().is_none_or(|expected| expected == physical), "U01 stopped evidence changed physical ownership");
                    let original_process = connection.is_some_and(|origin| valid(&origin.connection)
                        && grant.startup.as_ref().is_some_and(|s| s.peer == Some(origin.peer) && s.process_nonce == origin.process_nonce));
                    match said.stop_proof {
                        Some(crate::runner::pool::StopProof::OrderedBarrier) => {
                            let origin = connection.ok_or_else(|| anyhow::anyhow!("U01 ordered stop has no authenticated frame origin"))?;
                            anyhow::ensure!(valid(&origin.connection) && original_process && grant.stopping()
                                && grant.startup.as_ref().is_some_and(|s| s.peer == Some(origin.peer) && s.process_nonce == said.process_nonce),
                                "U01 stop barrier does not own the original process or phase");
                        }
                        Some(crate::runner::pool::StopProof::PhysicalRecovery) if !original_process => {
                            anyhow::ensure!(connection.is_some_and(|origin| valid(&origin.connection) && origin.process_nonce.as_deref().is_some_and(crate::proto::valid_process_nonce)),
                                "U01 replacement physical recovery has no authenticated current process identity");
                            // Gate proof independently releases this physical execution. It does
                            // not retire the old producer; its closed-map hint can later request
                            // only cleanup using the same existing historical receipt authority.
                        }
                        proof => {
                            anyhow::ensure!(original_process && (proof == Some(crate::runner::pool::StopProof::PhysicalRecovery)
                                || grant.startup.as_ref().is_some_and(|s| s.process_nonce == said.process_nonce)),
                                "U01 spontaneous stop hint belongs to another or unknown process");
                            // A spontaneous native failure can precede already-sent distinct
                            // Start items. Fence durably, then order Stop behind every such item.
                            // The closed Startup stays owned until the FIFO barrier and existing
                            // canonical outbox commit prove it safe to reclaim.
                            self.stop_executor_grant(&said.executor).await?;
                            let commands = if let Some(error) = said.error {
                                self.executor_failure_commands(&grant, error)
                            } else { vec![super::Command::StopExecutor { executor: said.executor.clone(), runner: grant.snapshot.runner.clone() }] };
                            if let Some(commands) = self.after(commands) { Box::pin(self.commands(commands)).await; }
                            return Ok(());
                        }
                    }
                } else if grant.snapshot.shape.is_none() {
                    let legacy_vm = super::super::pools::specs(&self.hub).await.iter().any(|p| p.name == grant.snapshot.pool && p.executor == "firecracker");
                    anyhow::ensure!(!legacy_vm || matches!(said.stop_proof, Some(crate::runner::pool::StopProof::PhysicalRecovery)),
                        "U01 legacy VM stop has no durable physical recovery proof; keep its original reservation fenced");
                }
                let proof = confirm_executor_stopped(&self.hub, &said.executor, &grant.generation, &grant.allocation, agent, physical).await?;
                if let Some(binding) = &proof.binding { self.retained_parent_stopped(binding).await?; }
                let mut commands = if let Some(error) = said.error {
                    self.sched.handle(Input::ExecutorFailed { executor: said.executor.clone(), error }, now_ms())
                } else { Vec::new() };
                // The backend already proved this executor stopped; preserve failure backoff
                // without asking it to stop again after its journal is retired.
                commands.retain(|c| !matches!(c, super::Command::StopExecutor { executor, .. } if executor == &said.executor));
                self.hub.store.kv_delete(key(&said.executor)).await?;
                commands.extend(self.sched.executor_stopped(&said.executor, now_ms()));
                commands
            }
            _ => {
                let Some((grant, first)) = stop_exact_executor_grant(&self.hub, &grant).await? else { return Ok(()); };
                if !first { return Ok(()); }
                self.executor_failure_commands(&grant, said.error.unwrap_or_else(|| "executor startup failed".into()))
            }
        };
        if let Some(commands) = self.after(commands) {
            Box::pin(self.commands(commands)).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::sched::state::Resources;

    fn grant(host: &str) -> ExecutorGrant {
        ExecutorGrant { executor: "executor-test".into(), generation: "generation".into(), allocation: "existing-grant".into(),
            snapshot: ExecutorSnapshot { executor: "executor-test".into(), pool: "pool".into(), runner: host.into(), reserved: if host.is_empty() { Resources::default() } else { Resources { cpu: 4.0, mem: 40, ..Resources::default() } }, leg: None, binding: None, shape: None },
            physical_execution: None, phase: "starting".into(), binding: None, pod_identity: None, startup: None }
    }

    #[test]
    fn u01_executor_ready_and_stop_proof_authenticate_same_persisted_grant() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let g = grant("host");
            hub.store.kv_set(key(&g.executor), serde_json::to_string(&g)?).await?;
            assert!(bind_executor_physical(&hub, &ExecutorGrant { generation: "other-generation".into(), ..g.clone() }, "host", "physical", None, None, None).await.is_err());
            assert!(bind_executor_physical(&hub, &g, "other-host", "physical", None, None, None).await.is_err());
            let ready = bind_executor_physical(&hub, &g, "host", "physical", None, None, None).await?;
            assert_eq!(ready.phase, "ready");
            assert!(confirm_executor_stopped(&hub, &g.executor, &g.generation, &g.allocation, "host", "another-physical").await.is_err());
            assert_eq!(executor_grant(&hub, &g.executor).await?.map(|g| g.phase), Some("ready".into()));
            confirm_executor_stopped(&hub, &g.executor, &g.generation, &g.allocation, "host", "physical").await?;
            assert_eq!(executor_grant(&hub, &g.executor).await?.map(|g| g.phase), Some("stopped".into()), "physical proof commits before capacity release");
            let unready = ExecutorGrant { executor: "failed-before-ready".into(), snapshot: ExecutorSnapshot { executor: "failed-before-ready".into(), ..g.snapshot }, ..g };
            hub.store.kv_set(key(&unready.executor), serde_json::to_string(&unready)?).await?;
            let stopped = confirm_executor_stopped(&hub, &unready.executor, &unready.generation, &unready.allocation, "host", "actual-failed-startup").await?;
            assert_eq!(stopped.physical_execution.as_deref(), Some("actual-failed-startup"), "first authenticated stopped proof may bind a startup which never sent Ready");
            let outer = Resources { cpu: 4.5, mem: 2560 << 20, disk: 34 << 30, gpu: 0 };
            let shape = kiln_ci::sched::vm::FirecrackerShape::derive(outer, Resources { cpu: 0.25, mem: 512 << 20, disk: 2 << 30, gpu: 0 }).map_err(anyhow::Error::msg)?;
            let shaped = ExecutorShape::Firecracker(shape);
            let vm = ExecutorGrant { executor: "vm-executor".into(), generation: "vm-generation".into(), allocation: "vm-grant".into(),
                snapshot: ExecutorSnapshot { executor: "vm-executor".into(), pool: "vm-pool".into(), runner: "host".into(), reserved: outer, leg: None, binding: None, shape: Some(shaped.clone()) },
                physical_execution: None, phase: "starting".into(), binding: None, pod_identity: None,
                startup: Some(ExecutorStartup { kind: "firecracker".into(), image: Some("alpine".into()), peer: Some(2), process_nonce: Some("a".repeat(43)) }) };
            hub.store.kv_set(key(&vm.executor), serde_json::to_string(&vm)?).await?;
            assert!(bind_executor_physical(&hub, &vm, "host", "vm-physical", None, Some(&"a".repeat(43)), Some(2)).await.is_err(), "old/missing Ready cannot consume a shaped grant");
            let other = ExecutorShape::Firecracker(kiln_ci::sched::vm::FirecrackerShape::derive(Resources { cpu: 3.5, ..outer }, Resources { cpu: 0.25, mem: 512 << 20, disk: 2 << 30, gpu: 0 }).map_err(anyhow::Error::msg)?);
            assert!(bind_executor_physical(&hub, &vm, "host", "vm-physical", Some(&other), Some(&"a".repeat(43)), Some(2)).await.is_err());
            assert!(executor_grant(&hub, &vm.executor).await?.is_some_and(|g| g.phase == "starting" && g.physical_execution.is_none()), "shape refusal commits neither Ready nor an invented physical owner");
            assert!(bind_executor_physical(&hub, &vm, "host", "vm-physical", Some(&shaped), Some(&"b".repeat(43)), Some(2)).await.is_err(), "same host/shape cannot replace the original runner process");
            assert!(bind_executor_physical(&hub, &vm, "host", "vm-physical", Some(&shaped), Some(&"a".repeat(43)), Some(3)).await.is_err(), "a copied nonce cannot move the physical startup to another certificate peer");
            let ready = bind_executor_physical(&hub, &vm, "host", "vm-physical", Some(&shaped), Some(&"a".repeat(43)), Some(2)).await?;
            assert_eq!(ready.snapshot.reserved, outer);
            assert_eq!(ready.snapshot.shape, Some(shaped));
            assert_eq!(ready.startup, vm.startup, "Ready cannot rewrite the original startup owner or image");
            let mut d = Dispatch { hub: std::sync::Arc::clone(&hub), sched: kiln_ci::sched::Scheduler::new(Default::default()), routing: Default::default(), held: Default::default(), runs: Default::default(), legs: Default::default(),
                timers: Default::default(), next: Vec::new(), in_commands: true, pods: Default::default(), absent_after: None, decisions: Default::default(), warm: Default::default(), warm_seq: 0, admission: Default::default(), pending_losses: Default::default(), pipeline_retries:Default::default() };
            d.sched.restore_executor(ready.snapshot.clone(), false, now_ms());
            let origin = ExecutorOrigin { peer: 2, connection: "authenticated-original-connection".into(), process_nonce: Some("a".repeat(43)) };
            let recovery = crate::runner::pool::Said { executor: vm.executor.clone(), state: "stopped".into(), generation: Some(vm.generation.clone()), allocation: Some(vm.allocation.clone()), physical_execution: Some("vm-physical".into()),
                stop_proof: Some(crate::runner::pool::StopProof::PhysicalRecovery), ..Default::default() };
            d.executor_event_from("host", recovery.clone(), Some(&origin)).await?;
            assert!(executor_grant(&hub, &vm.executor).await?.is_some_and(|grant| grant.stopping()), "kernel-empty proof from the original live producer does not retire its queued Starts");
            assert!(d.sched.executor_snapshot(&vm.executor).is_some(), "the original grant remains reserved until its ordered Stop barrier");
            assert!(d.next.iter().any(|command| matches!(command, super::super::Command::StopExecutor { executor, .. } if executor == &vm.executor)), "physical recovery queues Stop behind previously accepted Starts");
            let barrier = crate::runner::pool::Said { process_nonce: origin.process_nonce.clone(), stop_proof: Some(crate::runner::pool::StopProof::OrderedBarrier), ..recovery.clone() };
            let foreign = ExecutorOrigin { process_nonce: Some("b".repeat(43)), ..origin.clone() };
            assert!(d.executor_event_from("host", barrier.clone(), Some(&foreign)).await.is_err());
            assert!(executor_grant(&hub, &vm.executor).await?.is_some(), "a replacement process cannot claim the original producer's ordered barrier");
            d.executor_event_from("host", barrier, Some(&origin)).await?;
            assert!(executor_grant(&hub, &vm.executor).await?.is_none());
            assert!(d.sched.executor_snapshot(&vm.executor).is_none(), "the exact ordered physical barrier releases the original grant");
            let replacement = ExecutorGrant { executor: "vm-recovery".into(), snapshot: ExecutorSnapshot { executor: "vm-recovery".into(), ..ready.snapshot }, ..ready };
            hub.store.kv_set(key(&replacement.executor), serde_json::to_string(&replacement)?).await?;
            d.sched.restore_executor(replacement.snapshot.clone(), false, now_ms());
            let recovery = crate::runner::pool::Said { executor: replacement.executor.clone(), ..recovery };
            d.executor_event_from("host", recovery.clone(), Some(&foreign)).await?;
            assert!(executor_grant(&hub, &replacement.executor).await?.is_none(), "replacement Gate proof releases actual physical capacity without claiming old producer retirement");
            assert!(stopped_received(&hub, "host", &recovery).await?, "the same existing physical receipt authorizes later cleanup only");
            Ok(())
        })
    }

    #[test]
    fn u01_pod_enrollment_identity_and_grant_commit_in_same_transaction() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let g = grant("");
            hub.store.kv_set(key(&g.executor), serde_json::to_string(&g)?).await?;
            let identity = crate::k8s::ExecutorIdentity { namespace: "jobs".into(), name: "pod".into(), uid: "uid".into(), node: "node".into(), executor: g.executor.clone(), generation: g.generation.clone(), allocation: g.allocation.clone() };
            let capacity = kiln_ci::sched::pools::PodCapacity { resource_version: "42".into(), cpu_request: Some(0.5), memory_request: Some(64), ..Default::default() };
            let physical = identity.physical_execution()?;
            let proposed = identity.clone();
            let observed = capacity.clone();
            hub.store.call(move |c| { let tx = crate::store::tx(c)?; bind_pod_enrollment(&tx, &proposed, &observed)?; Ok(()) }).await?;
            assert_eq!(executor_grant(&hub, &g.executor).await?.map(|g| (g.phase, g.pod_identity)), Some(("starting".into(), None)), "aborted enrollment cannot publish physical ownership");
            assert!(executor_grant(&hub, &g.executor).await?.is_some_and(|g| g.snapshot.shape.is_none()), "old/aborted snapshot remains unknown");
            let proposed = identity.clone();
            let observed = capacity.clone();
            hub.store.call(move |c| { let tx = crate::store::tx(c)?; bind_pod_enrollment(&tx, &proposed, &observed)?; tx.commit()?; Ok(()) }).await?;
            let ready = executor_grant(&hub, &g.executor).await?.ok_or_else(|| anyhow::anyhow!("grant missing"))?;
            assert_eq!(ready.phase, "ready");
            assert_eq!(ready.pod_identity, Some(identity.clone()));
            assert_eq!(ready.snapshot.shape, Some(ExecutorShape::Kubernetes(capacity.clone())));
            assert_eq!(ready.physical_execution.as_deref(), Some(physical.as_str()), "shape metadata never changes the old physical hash");
            let proposed = identity.clone();
            let observed = kiln_ci::sched::pools::PodCapacity { resource_version: "43".into(), ..capacity.clone() };
            hub.store.call(move |c| { let tx = crate::store::tx(c)?; bind_pod_enrollment(&tx, &proposed, &observed)?; tx.commit()?; Ok(()) }).await?;
            assert_eq!(executor_grant(&hub, &g.executor).await?.and_then(|g| g.snapshot.shape), Some(ExecutorShape::Kubernetes(capacity.clone())), "status-only resourceVersion churn keeps the original observation");
            let proposed = identity.clone();
            let resized = kiln_ci::sched::pools::PodCapacity { cpu_request: Some(2.0), ..capacity.clone() };
            assert!(hub.store.call(move |c| { let tx = crate::store::tx(c)?; bind_pod_enrollment(&tx, &proposed, &resized)?; tx.commit()?; Ok(()) }).await.is_err(), "same UID cannot silently resize an existing grant");
            let changed = crate::k8s::ExecutorIdentity { uid: "reused-name-new-uid".into(), ..identity.clone() };
            assert!(hub.store.call(move |c| { let tx = crate::store::tx(c)?; bind_pod_enrollment(&tx, &changed, &capacity)?; tx.commit()?; Ok(()) }).await.is_err());

            // U11-F03: an async start captured before enrollment cannot adopt the new UID.
            assert!(stop_exact_executor_grant(&hub, &g).await?.is_none());
            let observation = crate::k8s::ExecutorPod {
                namespace: identity.namespace.clone(), name: identity.name.clone(), uid: identity.uid.clone(), resource_version: "43".into(),
                executor: identity.executor.clone(), generation: identity.generation.clone(), allocation: identity.allocation.clone(),
                phase: "Pending".into(), stuck: Some("bad image".into()), deleting: false,
            };
            assert!(ready.observes(&observation), "status-version churn retains the same enrolled owner");
            assert!(!ready.observes(&crate::k8s::ExecutorPod { uid: "previous-uid-same-name".into(), ..observation.clone() }));
            assert!(!ready.observes(&crate::k8s::ExecutorPod { generation: "previous-generation".into(), ..observation.clone() }));

            let next_identity = crate::k8s::ExecutorIdentity { uid: "successor-uid".into(), generation: "successor-generation".into(), allocation: "successor-allocation".into(), ..identity.clone() };
            let next = ExecutorGrant { generation: next_identity.generation.clone(), allocation: next_identity.allocation.clone(),
                physical_execution: Some(next_identity.physical_execution()?), pod_identity: Some(next_identity), ..ready.clone() };
            hub.store.kv_set(key(&next.executor), serde_json::to_string(&next)?).await?;
            assert!(stop_exact_executor_grant(&hub, &ready).await?.is_none(), "late stop cannot fence a successor allocation");
            let mut d = Dispatch { hub: std::sync::Arc::clone(&hub), sched: kiln_ci::sched::Scheduler::new(Default::default()), routing: Default::default(), held: Default::default(), runs: Default::default(), legs: Default::default(),
                timers: Default::default(), next: Vec::new(), in_commands: true, pods: Default::default(), absent_after: None, decisions: Default::default(), warm: Default::default(), warm_seq: 0, admission: Default::default(), pending_losses: Default::default(), pipeline_retries:Default::default() };
            d.sched.restore_executor(next.snapshot.clone(), false, now_ms());
            let failed = crate::runner::pool::Said { executor: ready.executor.clone(), state: "failed".into(), generation: Some(ready.generation.clone()),
                allocation: Some(ready.allocation.clone()), physical_execution: ready.physical_execution.clone(), error: Some("late startup error".into()), ..Default::default() };
            d.executor_event("", failed.clone()).await?;
            let old_uid = crate::runner::pool::Said { generation: Some(next.generation.clone()), allocation: Some(next.allocation.clone()), ..failed.clone() };
            d.executor_event("", old_uid).await?;
            assert!(d.next.is_empty(), "an old tuple or same-name old UID queues no successor Stop");
            assert_eq!(executor_grant(&hub, &next.executor).await?.map(|grant| grant.phase), Some("ready".into()));
            let original = identity.clone();
            let observed = kiln_ci::sched::pools::PodCapacity { resource_version: "44".into(), ..Default::default() };
            assert!(hub.store.call(move |c| { let tx = crate::store::tx(c)?; bind_pod_enrollment(&tx, &original, &observed)?; tx.commit()?; Ok(()) }).await.is_err(), "old enrollment cannot consume the successor");

            let current = crate::runner::pool::Said { generation: Some(next.generation.clone()), allocation: Some(next.allocation.clone()), physical_execution: next.physical_execution.clone(), ..failed };
            d.executor_event("", current.clone()).await?;
            let commands = d.next.len();
            assert_eq!(d.next.iter().filter(|command| matches!(command, super::super::Command::StopExecutor { executor, .. } if executor == &next.executor)).count(), 1);
            d.executor_event("", current).await?;
            assert_eq!(d.next.len(), commands, "a later reap drives pending cleanup without another startup failure/Stop task");
            assert!(executor_grant(&hub, &next.executor).await?.is_some_and(|grant| grant.stopping() && grant.same_owner(&next)));
            assert_eq!(d.sched.executor_snapshot(&next.executor).map(|snapshot| snapshot.reserved), Some(next.snapshot.reserved), "failure retains the original allocation until physical proof");
            let receipts: usize = hub.store.call(|c| Ok(c.query_row("SELECT count(*) FROM ci_executor_receipts", [], |r| r.get(0))?)).await?;
            assert_eq!(receipts, 0, "a stale or failed observation never fabricates stopped evidence");

            // U11-F05: even current complete terminal statuses or a deleting/absent API object
            // cannot pass the final writer gate without original producer/provider authority.
            assert_eq!(next.pod_retirement_hold(), Some(PodRetirementHold::BoundProviderClosureAndSandboxCoverageMissing));
            assert!(confirm_executor_stopped(&hub, &next.executor, &next.generation, &next.allocation, "", next.physical_execution.as_deref().ok_or_else(|| anyhow::anyhow!("fixture physical owner missing"))?).await.is_err());
            assert!(hold_pod_retirement(&hub, &ready).await?.is_none(), "an old UID/tuple cannot quarantine a successor");
            let status_hint = crate::runner::pool::Said { executor: next.executor.clone(), state: "stopped".into(), generation: Some(next.generation.clone()),
                allocation: Some(next.allocation.clone()), physical_execution: next.physical_execution.clone(), ..Default::default() };
            assert!(d.executor_event("", status_hint).await.is_err(), "a current callback cannot bypass the canonical provider-authority gate");
            assert!(executor_grant(&hub, &next.executor).await?.is_some_and(|grant| grant.stopping() && grant.same_owner(&next)));
            let binding = ParentBinding { executor: next.executor.clone(), generation: next.generation.clone(), allocation: next.allocation.clone(),
                physical_execution: next.physical_execution.clone().ok_or_else(|| anyhow::anyhow!("fixture physical owner missing"))?, job: "retained-pod-job".into(), attempt: 1 };
            let mut old_status_only = next.clone();
            old_status_only.phase = "stopped".into();
            old_status_only.binding = Some(binding.clone());
            old_status_only.snapshot.binding = None;
            hub.store.kv_set(key(&next.executor), serde_json::to_string(&old_status_only)?).await?;
            d.sched = kiln_ci::sched::Scheduler::new(Default::default());
            d.next.clear();
            d.restore_executors().await?;
            assert!(executor_grant(&hub, &next.executor).await?.is_some_and(|grant| grant.stopping() && grant.same_owner(&next)), "reopen keeps status-only stopped debt fenced under its original UID");
            assert_eq!(d.sched.executor_snapshot(&next.executor).map(|snapshot| snapshot.reserved), Some(next.snapshot.reserved), "recovery never releases the original physical reservation");
            assert_eq!(d.sched.executor_snapshot(&next.executor).and_then(|snapshot| snapshot.binding), Some(binding.clone()), "legacy binding backfill survives the exact hold reload");
            let mut transferred = old_status_only.clone();
            transferred.binding = Some(binding.clone());
            transferred.snapshot.binding = Some(binding.clone());
            let allocation = kiln_ci::sched::state::Allocation { id: "retained-pod-owner".into(), runner: "pod-runner".into(), request: Default::default(),
                mode: kiln_ci::sched::state::ReservationMode::Ordinary, repo: "fixture".into(), actor: String::new(), lane: String::new(), parent_binding: Some(binding) };
            hub.store.call(move |conn| {
                conn.execute("INSERT INTO ci_retained_ownership(job,attempt,agent,until_at,allocation,transferred,released,received_at) VALUES ('retained-pod-job',1,'pod-runner',0,?1,1,0,0)", [serde_json::to_string(&allocation)?])?;
                Ok(())
            }).await?;
            hub.store.kv_set(key(&transferred.executor), serde_json::to_string(&transferred)?).await?;
            d.sched = kiln_ci::sched::Scheduler::new(Default::default());
            d.next.clear();
            d.restore_executors().await?;
            assert!(executor_grant(&hub, &transferred.executor).await?.is_some_and(|grant| grant.phase == "retained_stopping" && grant.same_owner(&transferred)));
            assert!(d.sched.executor_snapshot(&transferred.executor).is_none(), "the existing exact retained owner is not duplicated by recovery");
            let mut unbound = grant("");
            unbound.executor = "unbound-publication".into();
            unbound.snapshot.executor = unbound.executor.clone();
            hub.store.kv_set(key(&unbound.executor), serde_json::to_string(&unbound)?).await?;
            assert_eq!(unbound.pod_retirement_hold(), Some(PodRetirementHold::OriginalPublicationAndProducerClosureMissing));
            let held = hold_pod_retirement(&hub, &unbound).await?.ok_or_else(|| anyhow::anyhow!("fixture original unbound grant missing"))?;
            assert!(held.stopping() && held.physical_execution.is_none() && held.pod_identity.is_none(), "unbound hold fabricates no captured UID, node or NoStart identity");
            assert!(confirm_executor_stopped(&hub, &unbound.executor, &unbound.generation, &unbound.allocation, "", "invented-never-bound").await.is_err());
            let receipts: usize = hub.store.call(|c| Ok(c.query_row("SELECT count(*) FROM ci_executor_receipts", [], |r| r.get(0))?)).await?;
            assert_eq!(receipts, 0, "withheld provider authority never emits a physical stopped receipt");

            // U11: retained pod debt can fill the shared budget. Exercise both actual start
            // callers at the real bound: known pre-write Full queues only new logical starts.
            let count: usize = hub.store.call(|conn| Ok(conn.query_row("SELECT count(*) FROM kv WHERE substr(k,1,length(?1))=?1", [PREFIX], |row| row.get(0))?)).await?;
            let mut fills = Vec::new();
            for n in count..LIMIT {
                let mut debt = grant("");
                debt.executor = format!("budget-debt-{n}");
                debt.snapshot.executor = debt.executor.clone();
                debt.phase = "stopping".into();
                fills.push((key(&debt.executor), serde_json::to_string(&debt)?));
            }
            // Bounded SQL packets reuse the same Store; no alternate database/test framework.
            for chunk in fills.chunks(128) {
                let chunk = chunk.to_vec();
                hub.store.call(move |conn| {
                    let tx = crate::store::tx(conn)?;
                    for (key, json) in chunk { tx.execute("INSERT INTO kv(k,v) VALUES (?1,?2)", params![key, json])?; }
                    tx.commit()?;
                    Ok(())
                }).await?;
            }
            use kiln_ci::sched::{pools::PoolSpec, routing::{LegRoute, RunnerView}, state::{Config, LegIn, RunnerIn, Scheduler}};
            let host_pool = PoolSpec { name: "full-host".into(), executor: "docker".into(), hosts: Some("executors contains docker".into()), image: Some("kiln/runner-ubuntu-24.04".into()),
                labels: vec!["pool-host".into()], min_idle: 1, max: Some(1), ..PoolSpec::default() }.pool().map_err(anyhow::Error::msg)?;
            let pod_pool = PoolSpec { name: "full-pod".into(), executor: "k8s".into(), labels: vec!["pool-pod".into()], min_idle: 1, max: Some(1), ..PoolSpec::default() }.pool().map_err(anyhow::Error::msg)?;
            d.sched = Scheduler::new(Config { pools: vec![host_pool, pod_pool], ..Config::default() });
            assert!(d.sched.restore_executor(held.snapshot.clone(), true, now_ms()));
            assert!(d.sched.refuse_executor_start(&held.snapshot, "must keep stored debt".into(), now_ms()).is_none(), "owned physical debt cannot enter definite logical refusal");
            let mut runner = RunnerIn { id: "budget-host".into(), slots: 2, view: RunnerView { name: "budget-host".into(), labels: vec!["ordinary".into()], caps: u64::from(crate::proto::CAP_POOLS), ..RunnerView::default() }, ..RunnerIn::default() };
            runner.view.facts.executors.insert("docker".into(), kiln_ci::facts::Executor::default());
            let mut commands = d.sched.handle(Input::RunnerUp(Box::new(runner)), now_ms());
            commands.extend(d.sched.handle(Input::Tick, now_ms()));
            let starts: Vec<_> = commands.into_iter().filter(|command| matches!(command, super::super::Command::StartExecutor { .. })).collect();
            assert_eq!(starts.len(), 2, "fixture must attempt both host and server-owned startup paths");
            let attempted: Vec<_> = starts.iter().filter_map(|command| match command { super::super::Command::StartExecutor { executor, .. } => Some(executor.clone()), _ => None }).collect();
            d.in_commands = false;
            d.next.clear();
            d.admission.ready = true;
            d.commands(starts).await;
            assert!(d.admission.ready, "a definite before-effects Full must not globally fence ordinary assignment");
            for executor in attempted {
                assert!(executor_grant(&hub, &executor).await?.is_none() && d.sched.executor_snapshot(&executor).is_none() && !d.pods.contains(&executor), "Full admits no durable grant/provider Start and backs off only the new logical reservation");
            }
            assert_eq!(d.sched.executor_snapshot(&held.executor), Some(held.snapshot.clone()), "Full never refunds stored physical debt");
            let ordinary = d.sched.handle(Input::LegQueued(Box::new(LegIn { id: "ordinary-after-full".into(), run: "fixture".into(), repo: "fixture".into(), route: LegRoute { labels: vec!["ordinary".into()], ..LegRoute::default() }, ..LegIn::default() })), now_ms());
            assert!(ordinary.iter().any(|command| matches!(command, super::super::Command::Assign { leg, runner, .. } if leg == "ordinary-after-full" && runner == "budget-host")), "ordinary existing-host assignment continues while new executor admission is Full");
            d.commands(ordinary).await;
            let (grants, receipts): (usize, usize) = hub.store.call(|conn| Ok((conn.query_row("SELECT count(*) FROM kv WHERE substr(k,1,length(?1))=?1", [PREFIX], |row| row.get(0))?, conn.query_row("SELECT count(*) FROM ci_executor_receipts", [], |row| row.get(0))?))).await?;
            assert_eq!((grants, receipts), (LIMIT, 0), "known Full changes no durable debt and fabricates no stopped/NoStart receipt");
            Ok(())
        })
    }
    /// U01: a lost stopped receipt can be replayed after the capacity tuple is pruned, even
    /// if its executor name now belongs to another generation. History never changes capacity.
    #[test]
    fn u01_executor_stopped_receipt_survives_prune_without_touching_new_generation() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let g = grant("host");
            hub.store.kv_set(key(&g.executor), serde_json::to_string(&g)?).await?;
            confirm_executor_stopped(&hub, &g.executor, &g.generation, &g.allocation, "host", "physical").await?;
            let said = crate::runner::pool::Said { executor: g.executor.clone(), state: "stopped".into(), generation: Some(g.generation.clone()), allocation: Some(g.allocation.clone()), physical_execution: Some("physical".into()), error: None, shape: None, process_nonce: None, stop_proof: None };
            hub.store.kv_delete(key(&g.executor)).await?;
            assert!(stopped_received(&hub, "host", &said).await?);
            assert!(!stopped_received(&hub, "other-host", &said).await?);
            assert!(!stopped_received(&hub, "host", &crate::runner::pool::Said { physical_execution: Some("different".into()), ..said.clone() }).await?);
            let mut d = Dispatch { hub: std::sync::Arc::clone(&hub), sched: kiln_ci::sched::Scheduler::new(Default::default()), routing: Default::default(), held: Default::default(), runs: Default::default(), legs: Default::default(),
                timers: Default::default(), next: Vec::new(), in_commands: false, pods: Default::default(), absent_after: None, decisions: Default::default(), warm: Default::default(), warm_seq: 0, admission: Default::default(), pending_losses: Default::default(), pipeline_retries:Default::default() };
            d.restore_admission().await?;
            d.executor_event("host", said.clone()).await?;
            assert!(executor_grant(&hub, &g.executor).await?.is_none(), "receipt replay never reconstructs a grant");
            let next = ExecutorGrant { generation: "new-generation".into(), allocation: "new-allocation".into(), physical_execution: Some("new-physical".into()), phase: "ready".into(), ..g };
            hub.store.kv_set(key(&next.executor), serde_json::to_string(&next)?).await?;
            d.sched.restore_executor(next.snapshot.clone(), false, now_ms());
            d.executor_event("host", said.clone()).await?;
            assert_eq!(d.sched.executor_snapshot(&next.executor), Some(next.snapshot.clone()), "old receipt cannot free the reused name's current allocation");
            assert_eq!(executor_grant(&hub, &next.executor).await?.map(|g| g.generation), Some(next.generation));
            assert!(d.executor_event("other-host", said).await.is_err());
            assert!(d.executor_event("host", crate::runner::pool::Said { executor: "unknown".into(), state: "stopped".into(), ..Default::default() }).await.is_err());
            Ok(())
        })
    }

}
