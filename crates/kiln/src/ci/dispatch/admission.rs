//! U01: reach is externally executed work in the CI Scheduler, never a second capacity ledger.
//!
//! The durable records below authenticate ownership; only Scheduler assigns resources. An
//! unclaimed offer can expire. A claimed execution cannot: only its host's stopped proof frees
//! its assignment. Restart changes the epoch, retaining old assignments until their host fences
//! them and acknowledges termination.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Command, Dispatch, Input, LegIn, Outcome, now_ms};
use kiln_ci::sched::needs::Needs;
use kiln_ci::sched::routing::LegRoute;
use kiln_ci::sched::state::Placement;

const KEY: &str = "u01/admission";
const LIMIT: usize = 4096;
const OFFER_MS: i64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Grant {
    pub id: String,
    pub token: String,
    pub epoch: String,
    pub owner: String,
    pub host: String,
    pub needs: Needs,
    /// U01: older ownership snapshots reserved the whole host despite requested quantities.
    /// Missing means whole, so an upgrade cannot shrink a live reservation before host proof.
    #[serde(default)]
    pub packed: bool,
    pub claimed: bool,
    pub expires: i64,
    pub instance: Option<String>,
    #[serde(default)]
    pub containment: Option<String>,
}

impl Grant {
    fn leg(&self) -> LegIn {
        LegIn {
            id: self.id.clone(),
            run: self.id.clone(),
            repo: format!("reach/{}", self.owner),
            actor: self.owner.clone(),
            lane: format!("reach/{}", self.owner),
            external: true,
            priority: 60,
            // U01: CPU/memory/disk/GPU quantities pack through Scheduler's shared physical
            // accounting. An explicit exclusive flag or an empty request takes the host whole.
            exclusive: !self.packed || self.needs.exclusive,
            request: if self.packed { self.needs.request } else { Default::default() },
            route: LegRoute { requires: self.needs.requires.clone(), ..LegRoute::default() },
            placement: Placement { runner: Some(self.host.clone()), ..Placement::default() },
            ..LegIn::default()
        }
    }
}

#[derive(Debug)]
pub(super) struct State {
    pub grants: BTreeMap<String, Grant>,
    pub retained: std::collections::BTreeSet<String>,
    pub epoch: String,
    pub ready: bool,
}

impl Default for State {
    fn default() -> Self {
        Self { grants: BTreeMap::new(), retained: Default::default(), epoch: crate::util::new_id(), ready: false }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Code {
    Unavailable, Limit, Invalid, Offline, Unsupported, Capacity, Busy, Owner, Host,
    Epoch, Claimed, Instance, Proof, Containment, Storage, Timeout, Unknown,
}

#[derive(Debug, Serialize)]
struct Refusal {
    code: Code,
    message: String,
}

pub(crate) fn refused(code: Code, message: impl Into<String>) -> Value {
    let message = message.into().chars().take(512).collect::<String>();
    json!({ "ok": false, "refusal": Refusal { code, message } })
}

impl Dispatch {
    async fn save_admission(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string(&self.admission.grants)?;
        anyhow::ensure!(json.len() <= LIMIT * 16384, "U01 durable ownership exceeds its byte budget");
        self.hub.store.kv_set(KEY.to_owned(), json).await
    }

    /// Must run before restoring CI and before any RunnerUp can place new work.
    pub(super) async fn restore_admission(&mut self) -> anyhow::Result<()> {
        let grants = match self.hub.store.kv_get(KEY.to_owned()).await? {
            Some(s) => {
                anyhow::ensure!(s.len() <= LIMIT * 16384, "U01 durable ownership exceeds its byte budget");
                serde_json::from_str::<BTreeMap<String, Grant>>(&s)?
            },
            None => BTreeMap::new(),
        };
        anyhow::ensure!(grants.len() <= LIMIT, "U01 admission record limit exceeded");
        for (id, g) in &grants {
            anyhow::ensure!(id == &g.id && id.starts_with("reach-") && id.len() <= 128
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "U01 invalid durable permit id");
            anyhow::ensure!(!g.host.is_empty() && g.host.len() <= 256 && !g.owner.is_empty() && g.owner.len() <= 256
                && !g.token.is_empty() && g.token.len() <= 256 && !g.epoch.is_empty() && g.epoch.len() <= 128,
                "U01 invalid durable permit ownership");
            anyhow::ensure!(!g.claimed || (g.instance.as_ref().is_some_and(|i| !i.is_empty() && i.len() <= 128)
                && g.containment.as_ref().is_some_and(|i| !i.is_empty() && i.len() <= 4096)),
                "U01 claimed ownership lacks its receiver or containment identity");
        }
        self.admission.grants = grants;
        // Old unclaimed tokens cannot claim in this epoch. No process could have started with
        // them: claim is durable before its answer. Claimed tokens stay adopted until host end.
        self.admission.grants.retain(|_, g| g.claimed);
        self.save_admission().await?;
        for g in self.admission.grants.values() {
            let cmds = self.sched.handle(Input::LegRunning { leg: Box::new(g.leg()), runner: g.host.clone(), attempt: 1 }, now_ms());
            self.next.extend(cmds);
        }
        self.admission.ready = true;
        Ok(())
    }

    pub(super) async fn admission_ask(&mut self, agent: &str, ask: Value) -> Value {
        if !self.admission.ready {
            return refused(Code::Unavailable, "resource admission has not recovered its durable ownership");
        }
        let result = self.admission_request(agent, &ask).await;
        match result {
            Ok(v) => v,
            Err(e) => {
                self.admission.ready = false;
                tracing::error!(error = %format!("{e:#}"), "U01 admission stopped after a persistence failure");
                refused(Code::Storage, "resource ownership could not be committed; admission is fenced")
            }
        }
    }

    async fn admission_request(&mut self, agent: &str, ask: &Value) -> anyhow::Result<Value> {
        let text = |key: &str| ask.get(key).and_then(Value::as_str).unwrap_or_default();
        match text("action") {
            "retained-inventory" => self.retained_inventory(agent).await,
            "recover" => {
                if text("instance").is_empty() {
                    return Ok(refused(Code::Instance, "receiver identity is required"));
                }
                let grants: Vec<Value> = self.admission.grants.values()
                    .filter(|g| g.host == agent && g.claimed && g.instance.as_deref() != Some(text("instance")))
                    .map(|g| json!({ "id": g.id, "token": g.token, "epoch": g.epoch, "host": g.host, "instance": g.instance, "containment": g.containment }))
                    .collect();
                Ok(json!({ "ok": true, "grants": grants }))
            }
            "acquire" => {
                if self.admission.grants.len() >= LIMIT {
                    return Ok(refused(Code::Limit, "the bounded admission table is full"));
                }
                let host = text("host");
                if agent.is_empty() || agent.len() > 256 || host.is_empty() || host.len() > 256 {
                    return Ok(refused(Code::Invalid, "invalid runner identity"));
                }
                let needs: Needs = match serde_json::from_value(ask.get("needs").cloned().unwrap_or_else(|| json!({}))) {
                    Ok(n) => n,
                    Err(_) => return Ok(refused(Code::Invalid, "invalid resource needs")),
                };
                let q = needs.request;
                if !q.cpu.is_finite() || q.cpu < 0.0 || needs.requires.len() > 128 || needs.requires.iter().map(String::len).sum::<usize>() > 8192 || needs.constraints().is_err() {
                    return Ok(refused(Code::Invalid, "invalid resource needs"));
                }
                // U01: supported hosts advertise the new reach inner protocol, and a strict
                // containment backend. The host still verifies containment before claiming.
                let supported = self.hub.reach.lock().announced(host).is_some_and(|a| a.version >= 2 && a.accept);
                let runner = self.hub.agents.lock().get(host).cloned();
                let Some(runner) = runner.filter(|r| r.connected && !r.draining && !r.ephemeral) else {
                    return Ok(refused(Code::Offline, "the chosen runner is unavailable or draining"));
                };
                if !supported {
                    return Ok(refused(Code::Unsupported, "the runner needs leased reach protocol version 2"));
                }
                let f = runner.facts.as_deref();
                let fits = f.is_some_and(|f| {
                    let cpu = f.cpu.threads.max(f.cpu.cores);
                    q.cpu <= f64::from(cpu) && q.mem <= f.mem.total && q.disk <= f.disk.free && u64::from(q.gpu) <= f.gpus.len() as u64
                });
                if (q.cpu > 0.0 || q.mem > 0 || q.disk > 0 || q.gpu > 0) && !fits {
                    return Ok(refused(Code::Capacity, "the runner cannot satisfy the requested resources"));
                }
                let id = format!("reach-{}", crate::util::new_id());
                let g = Grant { id: id.clone(), token: format!("{}{}", crate::util::new_id(), crate::util::new_id()), epoch: self.admission.epoch.clone(), owner: agent.to_owned(), host: host.to_owned(), needs, packed: true, claimed: false, expires: now_ms().saturating_add(OFFER_MS), instance: None, containment: None };
                self.admission.grants.insert(id.clone(), g.clone());
                let cmds = self.sched.handle(Input::LegQueued(Box::new(g.leg())), now_ms());
                let assigned = cmds.iter().any(|c| matches!(c, Command::Assign { leg, runner, .. } if leg == &id && runner == host));
                // Assignment is already atomic in the pure Scheduler. Its external commands
                // carry no CI runtime effects and are intercepted in the command loop.
                if let Some(cmds) = self.after(cmds) {
                    Box::pin(self.commands(cmds)).await;
                }
                if !assigned {
                    let cmds = self.sched.handle(Input::LegGone { leg: id.clone() }, now_ms());
                    if let Some(cmds) = self.after(cmds) {
                        Box::pin(self.commands(cmds)).await;
                    }
                    self.admission.grants.remove(&id);
                    return Ok(refused(Code::Busy, "the common scheduler has no free capacity on this runner"));
                }
                self.save_admission().await?;
                self.timers.insert(g.expires);
                Ok(json!({ "ok": true, "id": g.id, "token": g.token, "epoch": g.epoch, "host": g.host, "expires": g.expires }))
            }
            "claim" | "renew" | "end" | "abandon" | "decline" | "inspect" => {
                let id = text("id");
                let Some(g) = self.admission.grants.get(id).cloned() else {
                    // Idempotent stopped acknowledgement: unknown ownership cannot free
                    // anything, including when a successful end answer was lost.
                    return Ok(if text("action") == "end" { json!({ "ok": true }) } else { refused(Code::Unknown, "the resource permit is no longer held") });
                };
                if g.token != text("token") {
                    return Ok(refused(Code::Owner, "the resource permit belongs to another caller"));
                }
                let action = text("action");
                if action == "abandon" {
                    if g.owner != agent || g.claimed {
                        return Ok(refused(Code::Owner, "only the offer owner can abandon an unclaimed permit"));
                    }
                } else if g.host != agent {
                    return Ok(refused(Code::Host, "only the assigned execution host may claim or end this permit"));
                }
                if action == "decline" && g.claimed {
                    return Ok(refused(Code::Proof, "a claimed execution must prove termination before release"));
                }
                if action == "inspect" {
                    return Ok(json!({ "ok": true, "claimed": g.claimed, "instance": g.instance, "containment": g.containment }));
                }
                if action == "claim" || action == "renew" {
                    if g.epoch != self.admission.epoch || text("epoch") != self.admission.epoch {
                        return Ok(refused(Code::Epoch, "the admission server restarted; fence this execution before ending its permit"));
                    }
                    if action == "claim" && (g.claimed || g.expires <= now_ms()) {
                        return Ok(refused(Code::Claimed, "the offer was already claimed or expired"));
                    }
                    if action == "claim" && g.owner != text("owner") {
                        return Ok(refused(Code::Owner, "the encrypted peer is not the permit owner"));
                    }
                    if text("instance").is_empty() || text("instance").len() > 128 || (action == "renew" && (!g.claimed || g.instance.as_deref() != Some(text("instance")))) {
                        return Ok(refused(Code::Instance, "this execution belongs to another receiver instance"));
                    }
                    if action == "renew" {
                        // U01: renewal verifies the current epoch and receiver ownership; it
                        // never changes capacity or rewrites the durable ownership snapshot.
                        return Ok(json!({ "ok": true }));
                    }
                    if action == "claim" && (text("containment").is_empty() || text("containment").len() > 4096) {
                        return Ok(refused(Code::Containment, "the receiver must retain a bounded restart-safe containment identity"));
                    }
                    if let Some(held) = self.admission.grants.get_mut(id) {
                        if action == "claim" { held.containment = Some(text("containment").to_owned()); }
                        held.claimed = true;
                        held.instance = Some(text("instance").to_owned());
                    }
                    // Once claimed, there is deliberately no capacity-releasing server TTL.
                    self.save_admission().await?;
                    return Ok(json!({ "ok": true }));
                }
                if action == "end" && (!g.claimed || g.instance.as_deref() != Some(text("instance")) || ask.get("stopped") != Some(&Value::Bool(true))) {
                    return Ok(refused(Code::Proof, "the claiming receiver must confirm its containment is empty"));
                }
                self.admission.grants.remove(id);
                // Commit removal before freeing Scheduler capacity. A failed commit keeps the
                // assignment held and fences subsequent admission.
                self.save_admission().await?;
                let cmds = self.sched.handle(Input::AttemptFinished { leg: id.to_owned(), attempt: 1, outcome: Outcome::Success }, now_ms());
                if let Some(cmds) = self.after(cmds) {
                    Box::pin(self.commands(cmds)).await;
                }
                Ok(json!({ "ok": true }))
            }
            _ => Ok(refused(Code::Invalid, "unknown admission action")),
        }
    }

    pub(super) async fn expire_admission(&mut self) {
        if !self.admission.ready {
            return;
        }
        let expired: Vec<String> = self.admission.grants.values().filter(|g| !g.claimed && g.expires <= now_ms()).map(|g| g.id.clone()).collect();
        if expired.is_empty() { return; }
        for id in &expired {
            self.admission.grants.remove(id);
        }
        if let Err(e) = self.save_admission().await {
            self.admission.ready = false;
            tracing::error!(error = %e, "U01 unclaimed offers could not expire durably");
            return;
        }
        for id in expired {
            let cmds = self.sched.handle(Input::AttemptFinished { leg: id, attempt: 1, outcome: Outcome::Cancelled }, now_ms());
            if let Some(cmds) = self.after(cmds) {
                Box::pin(self.commands(cmds)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap, HashSet};
    use std::sync::Arc;
    use kiln_ci::sched::state::{Config, RunnerIn, Scheduler};
    use kiln_ci::sched::routing::RunnerView;

    fn dispatcher(hub: Arc<crate::hub::Hub>) -> Dispatch {
        Dispatch {
            hub, sched: Scheduler::new(Config::default()), routing: Default::default(),
            held: HashMap::new(), runs: HashMap::new(), legs: HashMap::new(), timers: BTreeSet::new(),
            next: Vec::new(), in_commands: false, pods: HashSet::new(), absent_after: None,
            decisions: HashMap::new(), warm: HashMap::new(), warm_seq: 0, admission: State::default(), pending_losses: HashMap::new(), pipeline_retries:Default::default(),
        }
    }

    fn grant() -> Grant {
        Grant {
            id: "reach-fixture".into(), token: "secret-fixture".into(), epoch: "old-epoch".into(),
            owner: "caller".into(), host: "host".into(), needs: Needs::default(), packed: true, claimed: true,
            expires: 0, instance: Some("receiver".into()), containment: Some("/sys/fs/cgroup/fixture/reach-fixture".into()),
        }
    }

    /// U01: expiry and restart retain occupied capacity; only its claiming host can free it.
    #[test]
    fn u01_claimed_expiry_restart_and_cross_caller_end_never_free_capacity() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let mut d = dispatcher(Arc::clone(&hub));
            let g = grant();
            d.admission.grants.insert(g.id.clone(), g.clone());
            d.save_admission().await?;
            d.restore_admission().await?;
            d.sched.handle(Input::RunnerUp(Box::new(RunnerIn {
                id: "host".into(), slots: 1,
                view: RunnerView { name: "host".into(), ..RunnerView::default() }, ..RunnerIn::default()
            })), now_ms());
            d.sched.handle(Input::LegQueued(Box::new(LegIn { id: "ci".into(), repo: "repo".into(), ..LegIn::default() })), now_ms());
            assert!(d.sched.reason("ci").is_some());
            d.expire_admission().await;
            assert_eq!(d.admission.grants.len(), 1, "claimed expiry cannot release a live host");

            let renew = json!({ "action": "renew", "id": g.id, "token": g.token, "epoch": g.epoch, "instance": "receiver" });
            let answer = d.admission_ask("host", renew).await;
            assert_eq!(answer.pointer("/refusal/code"), Some(&json!("epoch")));

            let end = json!({ "action": "end", "id": g.id, "token": g.token, "instance": "receiver", "stopped": true });
            let denied = d.admission_ask("caller", end.clone()).await;
            assert_eq!(denied.pointer("/refusal/code"), Some(&json!("host")));
            let mut no_proof = end.clone();
            no_proof.as_object_mut().ok_or_else(|| anyhow::anyhow!("U01 end fixture is not an object"))?.insert("stopped".into(), json!(false));
            assert_eq!(d.admission_ask("host", no_proof).await.pointer("/refusal/code"), Some(&json!("proof")));
            assert_eq!(d.admission.grants.len(), 1);
            assert!(d.sched.reason("ci").is_some());

            assert_eq!(d.admission_ask("host", end.clone()).await.get("ok"), Some(&json!(true)));
            assert!(d.admission.grants.is_empty());
            assert!(d.sched.reason("ci").is_none(), "host proof releases CI through the same Scheduler");
            assert_eq!(d.admission_ask("host", end).await.get("ok"), Some(&json!(true)), "lost end acknowledgement is idempotent");

            let mut after = dispatcher(hub);
            after.restore_admission().await?;
            assert!(after.admission.grants.is_empty(), "durable removal survives restart");
            Ok(())
        })
    }

    #[test]
    fn u01_unclaimed_offer_expires_and_cannot_be_claimed_twice() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let mut d = dispatcher(hub);
            d.restore_admission().await?;
            let mut g = grant();
            g.epoch = d.admission.epoch.clone();
            g.claimed = false;
            g.instance = None;
            g.expires = now_ms() + OFFER_MS;
            d.admission.grants.insert(g.id.clone(), g.clone());
            d.sched.handle(Input::LegRunning { leg: Box::new(g.leg()), runner: g.host.clone(), attempt: 1 }, now_ms());
            d.save_admission().await?;
            let claim = json!({ "action": "claim", "id": g.id, "token": g.token, "epoch": g.epoch, "owner": "caller", "instance": "receiver", "containment": "/sys/fs/cgroup/fixture/reach-fixture" });
            assert_eq!(d.admission_ask("host", claim.clone()).await.get("ok"), Some(&json!(true)));
            assert_eq!(d.admission_ask("host", claim).await.pointer("/refusal/code"), Some(&json!("claimed")));
            let mut expired = g;
            expired.id = "reach-expired".into();
            expired.expires = 0;
            d.admission.grants.insert(expired.id.clone(), expired.clone());
            d.sched.handle(Input::LegRunning { leg: Box::new(expired.leg()), runner: expired.host.clone(), attempt: 1 }, now_ms());
            d.save_admission().await?;
            d.expire_admission().await;
            assert_eq!(d.admission.grants.len(), 1);
            assert!(d.admission.grants.values().all(|g| g.claimed));
            Ok(())
        })
    }

    /// U01: persisted whole-host ownership remains whole across the packing upgrade; only
    /// newly admitted grants explicitly opt into quantities. No live claim silently shrinks.
    #[test]
    fn u01_old_persisted_whole_host_claim_cannot_shrink_during_packing_upgrade() -> anyhow::Result<()> {
        let mut g = grant();
        g.needs = kiln_ci::sched::needs::parse("cpus=2,mem=20").map_err(anyhow::Error::msg)?;
        let mut old = serde_json::to_value(&g)?;
        if let Some(object) = old.as_object_mut() { object.remove("packed"); }
        let old: Grant = serde_json::from_value(old)?;
        for resources in [false, true] {
            for grant in [&old, &g] {
                let mut sched = Scheduler::new(Config { resources, ..Config::default() });
                sched.handle(Input::LegRunning { leg: Box::new(grant.leg()), runner: "host".into(), attempt: 1 }, 0);
                sched.handle(Input::RunnerUp(Box::new(RunnerIn {
                    id: "host".into(), slots: 4,
                    capacity: Some(kiln_ci::sched::state::Resources { cpu: 8.0, mem: 80, disk: 100, gpu: 0 }),
                    view: RunnerView { name: "host".into(), ..RunnerView::default() }, ..RunnerIn::default()
                })), 1);
                let cmds = sched.handle(Input::LegQueued(Box::new(LegIn {
                    id: "ci".into(), repo: "repo".into(), request: kiln_ci::sched::state::Resources { cpu: 6.0, mem: 60, ..Default::default() }, ..LegIn::default()
                })), 2);
                let assigned = cmds.iter().any(|c| matches!(c, Command::Assign { leg, .. } if leg == "ci"));
                assert_eq!(assigned, grant.packed, "restore must retain the original reservation shape");
            }
        }
        Ok(())
    }

}
