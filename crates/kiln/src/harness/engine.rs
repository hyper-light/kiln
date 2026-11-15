//! A runner's engine: how it runs new agent sessions (kiln plan 01a1dd2e…, P2.3 and P2.4; the design's
//! part A; docs/isolation.md, "A runner's engine"). `host`: each a process of its machine's, with no
//! microVM. `microvm`: each in a Shards microVM of its own, through Hex (`crate::sandbox::hexvm`). Its
//! engine is, the first of these there is:
//!
//! 1. what its owner set on the server (Runners; `PUT /api/runners/:name/engine`, MCP and code mode's
//!    `runners.engine.set`): kept in the server's `kv` table (`runner_engine/<runner>`), sent to its
//!    runner as that names [`crate::proto::feature::RUNNER_ENGINE`] and whenever it changes
//!    (`ServerMsg::RunnerEngine`), and kept by the runner in its state folder for its restarts;
//! 2. `microvm` where the runner can hold agents in microVMs now ([`capable`]: the person's decision 3,
//!    2026-11-14), else `host`, saying why.
//!
//! `microvm` is refused, saying why, where the runner can't hold them; one set where it later can't is
//! held as `host`, saying why. The engine is written into the runner's facts ([`overlay`]:
//! `isolation.engine`, and `virt.microvm` only on a `microvm` one) by the runner, for its own admission
//! as an owner probes it, and by the server over what each runner says, for placement and the runners
//! API alike. So a session that needs a microVM is placed only on a `microvm` runner, and the start of
//! one that asks for none only on a `host` one ([`super::lineage`]). A switch takes nothing running away:
//! the sessions of the engine it had finish there ([`view`]'s `draining`), and new sessions take the new.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use http::StatusCode;
use kiln_ci::facts::RunnerFacts;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use kiln_ci::facts::Engine;

use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::instances::PeerId;
use crate::proto::ServerMsg;

/// What its owner set on the server (`ServerMsg::RunnerEngine`, `PUT /api/runners/:name/engine`):
/// none, and what the runner can do decides ([`held`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asked {
    #[serde(default)]
    pub engine: Option<Engine>,
    /// How many times it was set: the one before's plus one, at every set (none set yet: 0). A start
    /// carries the version its boundary was decided under (`Spec::engine_version`), and its runner
    /// refuses one decided under another ([`stale`]): placed just before a switch, it never starts on
    /// the engine the runner had.
    #[serde(default)]
    pub version: u64,
}

/// Why a runner holding `here` refuses a start whose boundary was decided under its engine setting's
/// version `placed`: decided under another, it may be on the engine the runner had (kiln plan
/// 01a1dd2e…, P2.3). None: it starts. A start that names none (from a server before versions, or one
/// the server itself starts) isn't held to it.
pub fn stale(placed: Option<u64>, here: Asked) -> Option<String> {
    let placed = placed?;
    (placed != here.version).then(|| {
        let now = here.engine.map_or_else(|| "what it can do".to_owned(), |e| e.to_string());
        format!("this runner's engine was set again (to {now}, setting {}) after this start was placed under setting {placed}: start it again, and it's placed on the engine it has now", here.version)
    })
}

/// Where a runner's engine comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Its owner set it on the server.
    Server,
    /// What the runner can do now: `microvm` where it can hold agents in microVMs, else `host`.
    Default,
}

/// Whether a runner can hold agents in microVMs now, and why not: the one place kiln reads that from.
///
/// From its facts as its runner says them: its kiln carries Shards and its machine has a hypervisor it
/// can use (`isolation.available`, `why_not`: `crate::sandbox::facts`). Once kiln vendors a Shards with
/// `shards_api::host_check()` (d993d83ee or later), the runner's facts say its answer
/// (`{backend, usable, why}`) in `isolation`, and this reads it there, unchanged for every caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capable {
    pub usable: bool,
    pub why: Option<String>,
}

/// Why a runner whose facts say nothing of microVMs (a runner from before them) holds none.
pub const NOT_SAID: &str = "its runner doesn't say whether it starts microVMs: its kiln is from before them";

/// What `facts` say the runner can do now ([`Capable`]).
pub fn capable(facts: &RunnerFacts) -> Capable {
    match &facts.isolation {
        Some(i) if i.available => Capable { usable: true, why: None },
        Some(i) => Capable { usable: false, why: Some(i.why_not.clone().unwrap_or_else(|| crate::sandbox::NO_MICROVM.to_owned())) },
        None => Capable { usable: false, why: Some(NOT_SAID.to_owned()) },
    }
}

/// The engine a runner holds, where it comes from, and why it's `host` where it can't be `microvm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub engine: Engine,
    pub from: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// What a runner holds: what its owner set, else `microvm` where it can hold microVMs, else `host`.
/// `microvm` set where it can't any more is held as `host`, saying why.
pub fn held(asked: Asked, can: &Capable) -> Held {
    match (asked.engine, can.usable) {
        (Some(Engine::Microvm), false) => Held { engine: Engine::Host, from: Source::Default, why: Some(format!("its owner set microvm, but it can't hold microVMs now: {}", can.why.as_deref().unwrap_or(crate::sandbox::NO_MICROVM))) },
        (Some(engine), _) => Held { engine, from: Source::Server, why: None },
        (None, true) => Held { engine: Engine::Microvm, from: Source::Default, why: None },
        (None, false) => Held { engine: Engine::Host, from: Source::Default, why: can.why.clone() },
    }
}

/// `facts` as a runner whose owner set `asked` holds them: its engine in `isolation.engine`, and
/// `virt.microvm` only where that's `microvm`, so placement (on the server, and by its owner probing
/// its admission) never sends it a session that needs a microVM while it runs sessions on its host.
pub fn overlay(facts: &mut RunnerFacts, asked: Asked) -> Held {
    let can = capable(facts);
    let h = held(asked, &can);
    facts.virt.microvm = can.usable && h.engine == Engine::Microvm;
    if let Some(i) = facts.isolation.as_mut() {
        i.engine = Some(h.engine);
    }
    h
}

/// The engine a runner's facts say it holds, as the server has them ([`overlay`]): none from a runner
/// whose facts haven't come yet.
pub fn of(facts: &RunnerFacts) -> Option<Engine> {
    facts.isolation.as_ref().and_then(|i| i.engine).or_else(|| facts.isolation.is_none().then_some(Engine::Host))
}

/// Whether a runner's Hex node lists the class image VMs run as (Hex's `shards-image`, P2.2), as its
/// facts say. The kiln Hex vendors today has no such class (an image box is `Class::Shards`), so every
/// node that can hold microVMs runs them: this answers `Unknown` and placement goes by the engine
/// alone. Once kiln vendors that Hex, the runner's facts carry its node's classes and this reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeImages {
    /// Its node lists the class.
    Listed,
    /// Its node lists the classes, and not that one.
    NotListed,
    /// Its kiln's Hex has no such class to list.
    Unknown,
}

/// What `facts` say of their node's image class ([`NodeImages`]).
pub fn node_images(_facts: &RunnerFacts) -> NodeImages {
    NodeImages::Unknown
}

// ------------------------------------------------------------------------------ on the runner

/// Where a machine keeps what its owner set on the server: its state folder, which each of its runner
/// processes shares.
fn kept_path(state_dir: &Path) -> PathBuf {
    state_dir.join("runner-engine.json")
}

/// What was set for this machine on the server, as it kept it: none when nothing was, or it can't be
/// read. Blocking.
pub fn kept(state_dir: &Path) -> Asked {
    std::fs::read(kept_path(state_dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// `asked` kept for this machine's restarts, its version with it (none set is kept too: its version
/// says which setting it is). Blocking.
pub fn keep(state_dir: &Path, asked: Asked) -> std::io::Result<()> {
    crate::util::write_secret(&kept_path(state_dir), &serde_json::to_vec(&asked)?, 0o644)
}

/// What this process's runner was set on the server, which its facts are written with ([`overlay`],
/// `runner::probe::Probing::read`). One runner a process.
static HERE: Mutex<Asked> = Mutex::new(Asked { engine: None, version: 0 });

/// This process's runner holds `asked` from now on (as its next facts say).
pub fn hold(asked: Asked) {
    *HERE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = asked;
}

/// What this process's runner was set on the server.
pub fn here() -> Asked {
    *HERE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ------------------------------------------------------------------------------ on the server

pub(crate) const KEY_PREFIX: &str = "runner_engine/";

pub type Answer = Result<Response, (StatusCode, String)>;

fn fail(code: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (code, msg.into())
}

/// What's set on the server for each runner: a copy read without a lock (each start's placement, the
/// runners API) and changed by swapping a new one in whole (`crate::rcu`).
#[derive(Default)]
pub struct Engines {
    inner: crate::rcu::Rcu<HashMap<String, Asked>>,
}

impl Engines {
    /// What owners set, read as the server starts.
    pub async fn load(&self, store: &crate::store::Store) -> anyhow::Result<()> {
        let rows = store.kv_prefix(KEY_PREFIX.to_owned()).await?;
        let mut read: Vec<(String, Asked)> = Vec::new();
        for (k, v) in rows {
            let Some(runner) = k.strip_prefix(KEY_PREFIX) else { continue };
            match serde_json::from_str::<Asked>(&v) {
                Ok(asked) => read.push((runner.to_owned(), asked)),
                Err(e) => tracing::warn!(runner, error = %e, "runner engine: what was set is unreadable; what the runner can do decides"),
            }
        }
        self.inner.update(|g| g.extend(read.iter().cloned()));
        Ok(())
    }

    /// What's set on the server for `runner`.
    pub fn asked(&self, runner: &str) -> Asked {
        self.inner.load().get(runner).copied().unwrap_or_default()
    }

    /// Every runner's setting now, for a start to carry the versions it was decided under.
    pub fn now(&self) -> std::sync::Arc<HashMap<String, Asked>> {
        self.inner.get()
    }

    /// A runner known by another name from now on (docs/machines.md): what's set for it goes with it
    /// (its row moves with the store's, `Store::move_runner`).
    pub fn moved(&self, from: &str, to: &str) {
        self.inner.update(|g| {
            if let Some(e) = g.remove(from) {
                g.entry(to.to_owned()).or_insert(e);
            }
        });
    }

    /// A runner forgotten: what was set for it goes.
    pub async fn forget(&self, store: &crate::store::Store, runner: &str) {
        self.inner.update(|g| g.remove(runner));
        if let Err(e) = store.kv_delete(format!("{KEY_PREFIX}{runner}")).await {
            tracing::warn!(runner, error = %format!("{e:#}"), "runner engine: what was set wasn't deleted");
        }
    }
}

/// The version of `target`'s runner's engine setting now, for a start whose boundary is decided now to
/// carry (`Spec::engine_version`): none for the server itself (`local`).
pub fn version_at(hub: &Hub, target: &str) -> Option<u64> {
    target.strip_prefix("agent:").map(|m| hub.engines.asked(m).version)
}

/// The runner processes of `runner` that take its engine.
fn takers(hub: &Hub, runner: &str) -> Vec<PeerId> {
    let inst = hub.instances.load();
    inst.of(runner).into_iter().filter(|p| inst.get(*p).is_some_and(|i| crate::proto::feature::speaks(&i.features, crate::proto::feature::RUNNER_ENGINE))).collect()
}

/// Sends what's set for `runner` to its processes `peers` (a state: the newest goes).
async fn send(hub: &Hub, runner: &str, peers: Vec<PeerId>) {
    let Ok(json) = serde_json::to_string(&hub.engines.asked(runner)) else { return };
    let pending = hub.command_peers(runner, peers, &ServerMsg::RunnerEngine { json }).await;
    pending.log_when_answered(crate::hub::LOG_WITHIN);
}

/// What's set for `runner`, held from now on: in the facts the server has of it at once, and sent to
/// its runner processes that take it (one away gets it as it next names the feature).
pub async fn push(hub: &Hub, runner: &str) {
    let asked = hub.engines.asked(runner);
    hub.change_agent(runner, |a| {
        if let Some(f) = a.facts.as_mut() {
            overlay(f, asked);
        }
    });
    let peers = takers(hub, runner);
    if !peers.is_empty() {
        send(hub, runner, peers).await;
    }
}

/// A runner process named the features it speaks (`AgentMsg::Features`): one that takes its engine is
/// sent what's set for its runner, none set too (what it kept from before goes).
pub async fn on_features(hub: &Hub, runner: &str, peer: PeerId, names: &[String]) {
    if crate::proto::feature::speaks(names, crate::proto::feature::RUNNER_ENGINE) {
        send(hub, runner, vec![peer]).await;
    }
}

/// A runner known to the server (enrolled, or connected now).
async fn known(hub: &Hub, name: &str) -> bool {
    hub.agents.load().contains_key(name) || hub.store.agents().await.is_ok_and(|rows| rows.iter().any(|a| a.name == name))
}

/// How many of `sessions` (a runner's) run on `engine`, and how many on the other: those whose harness
/// still runs (not exited or crashed).
fn counted(sessions: &[crate::harness::session::Info], engine: Engine) -> (usize, usize) {
    use crate::harness::model::Status;
    let running = sessions.iter().filter(|i| !matches!(i.status, Status::Exited | Status::Crashed));
    let (on, other): (Vec<_>, Vec<_>) = running.partition(|i| i.isolation.is_some() == (engine == Engine::Microvm));
    (on.len(), other.len())
}

/// `GET /api/runners/:name/engine` (and what `PUT` answers): the engine it holds, where that comes
/// from and why, what its owner set, whether it can hold microVMs now and why not, its isolation facts,
/// and its sessions on each engine (`draining`: those of the other engine, which finish there).
pub async fn view(hub: &Hub, name: &str) -> Value {
    let asked = hub.engines.asked(name);
    let facts = hub.agents.load().get(name).and_then(|a| a.facts.as_deref().cloned());
    let (held, can) = match &facts {
        Some(f) => {
            let can = capable(f);
            (Some(held(asked, &can)), Some(can))
        }
        None => (None, None),
    };
    let target = format!("agent:{name}");
    let sessions: Vec<_> = hub.harness.list().into_iter().filter(|i| i.target == target).collect();
    let (on, draining) = held.as_ref().map_or((0, 0), |h| counted(&sessions, h.engine));
    json!({
        "runner": name,
        "engine": held.as_ref().map(|h| h.engine),
        "from": held.as_ref().map(|h| h.from),
        "why": held.as_ref().and_then(|h| h.why.clone()),
        "asked": asked.engine,
        "version": asked.version,
        "microvm": can.as_ref().map(|c| json!({"usable": c.usable, "why": c.why})),
        "isolation": facts.as_ref().and_then(|f| f.isolation.clone()),
        "virt": facts.as_ref().map(|f| json!({"microvm": f.virt.microvm, "kvm": f.virt.kvm, "hvf": f.virt.hvf})),
        "sessions": on,
        "draining": draining,
    })
}

/// [`view`] of a runner the server knows, for every surface (REST, MCP, code mode).
pub async fn read(hub: &Hub, name: &str) -> Result<Value, (StatusCode, String)> {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    Ok(view(hub, name).await)
}

pub async fn get(hub: &Hub, name: &str) -> Answer {
    read(hub, name).await.map(|v| srv::json(StatusCode::OK, &v))
}

/// What a `PUT` asks for: `{"engine": "host" | "microvm"}`, or `{"engine": null}` for what the runner
/// can do.
pub fn asked_of(body: &Value) -> Result<Asked, String> {
    const SHAPE: &str = "expected {\"engine\": \"host\"} or {\"engine\": \"microvm\"}, or {\"engine\": null} for what the runner can do";
    let o = body.as_object().ok_or(SHAPE)?;
    if let Some(k) = o.keys().find(|k| *k != "engine") {
        return Err(format!("{k:?} isn't one: {SHAPE}"));
    }
    match o.get("engine").ok_or(SHAPE)? {
        Value::Null => Ok(Asked::default()),
        Value::String(s) => match s.trim() {
            "host" => Ok(Asked { engine: Some(Engine::Host), version: 0 }),
            "microvm" => Ok(Asked { engine: Some(Engine::Microvm), version: 0 }),
            other => Err(format!("a runner's engine is \"host\" or \"microvm\", not {other:?}")),
        },
        v => Err(format!("a runner's engine is \"host\" or \"microvm\", not {v}")),
    }
}

/// `PUT /api/runners/:name/engine`: sets it (`microvm` refused, saying why, where the runner can't
/// hold microVMs now), or takes what was set away; stored, held in the server's facts of it at once
/// and sent to the runner. What runs on the other engine finishes there.
pub async fn put(hub: &Hub, name: &str, req: Request, by: &str) -> Answer {
    let body: Value = srv::body_json(req.into_body(), 4096).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    set(hub, name, &body, by).await.map(|v| srv::json(StatusCode::OK, &v))
}

/// [`put`]'s work, for every surface (REST, MCP, code mode): the view after it.
pub async fn set(hub: &Hub, name: &str, body: &Value, by: &str) -> Result<Value, (StatusCode, String)> {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    let asked = asked_of(body).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    if asked.engine == Some(Engine::Microvm) {
        let facts = hub.agents.load().get(name).and_then(|a| a.facts.as_deref().cloned());
        let can = facts.as_ref().map(capable);
        match can {
            Some(Capable { usable: true, .. }) => {}
            Some(Capable { why, .. }) => return Err(fail(StatusCode::CONFLICT, format!("{} can't run its sessions in microVMs: {}", hub.shown(name), why.as_deref().unwrap_or(crate::sandbox::NO_MICROVM)))),
            None => return Err(fail(StatusCode::CONFLICT, format!("{} hasn't said yet whether it can run microVMs: set its engine once it has", hub.shown(name)))),
        }
    }
    // The one before's plus one: a start decided under the setting before is refused by the runner.
    let version = hub.engines.asked(name).version.checked_add(1).ok_or_else(|| fail(StatusCode::CONFLICT, format!("{name}'s engine was set as many times as a setting's version counts")))?;
    let asked = Asked { version, ..asked };
    let json = serde_json::to_string(&asked).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    hub.store.kv_set(format!("{KEY_PREFIX}{name}"), json).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.engines.inner.update(|g| {
        g.insert(name.to_owned(), asked);
    });
    let said = asked.engine.map_or_else(|| "nothing set here: what it can do".to_owned(), |e| e.to_string());
    hub.store.audit(by.to_owned(), "runner_engine.update".into(), format!("{name}: {said}")).await;
    push(hub, name).await;
    Ok(view(hub, name).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::facts::Isolation;

    fn facts(isolation: Option<Isolation>) -> RunnerFacts {
        RunnerFacts { isolation, ..RunnerFacts::default() }
    }

    fn able() -> Isolation {
        Isolation { available: true, hypervisor: Some("Hypervisor.framework".into()), why_not: None, engine: None }
    }

    fn unable(why: &str) -> Isolation {
        Isolation { available: false, hypervisor: None, why_not: Some(why.into()), engine: None }
    }

    /// P2.3 AC2 (the person's decision 3): a runner that can hold microVMs defaults to `microvm`; one
    /// that can't holds `host`, its facts' why beside it. What its owner set wins, but `microvm` set
    /// where it can't any more is held as `host`, saying why.
    #[test]
    fn p2_3_ac2_a_capable_runner_defaults_to_microvm_and_one_that_isnt_says_why() {
        let set = |e| Asked { engine: Some(e), version: 1 };
        let yes = capable(&facts(Some(able())));
        let no = capable(&facts(Some(unable("no hypervisor"))));
        assert_eq!(held(Asked::default(), &yes), Held { engine: Engine::Microvm, from: Source::Default, why: None });
        assert_eq!(held(Asked::default(), &no), Held { engine: Engine::Host, from: Source::Default, why: Some("no hypervisor".into()) });
        assert_eq!(held(set(Engine::Host), &yes), Held { engine: Engine::Host, from: Source::Server, why: None });
        assert_eq!(held(set(Engine::Host), &no).engine, Engine::Host);
        let lost = held(set(Engine::Microvm), &no);
        assert_eq!((lost.engine, lost.from), (Engine::Host, Source::Default));
        assert!(lost.why.is_some_and(|w| w.contains("no hypervisor")));
        assert_eq!(capable(&facts(None)), Capable { usable: false, why: Some(NOT_SAID.into()) }, "a runner from before microVMs");
    }

    /// The engine goes into the facts placement reads: `virt.microvm` only on a `microvm` runner that
    /// can hold them, `isolation.engine` its engine. Written over facts that say it already, it says
    /// the same (the server writes it over the runner's own).
    #[test]
    fn p2_4_a_host_runner_offers_no_microvm_in_its_facts() {
        let mut f = facts(Some(able()));
        overlay(&mut f, Asked::default());
        assert_eq!((f.virt.microvm, of(&f)), (true, Some(Engine::Microvm)));
        overlay(&mut f, Asked { engine: Some(Engine::Host), version: 0 });
        assert_eq!((f.virt.microvm, of(&f)), (false, Some(Engine::Host)));
        overlay(&mut f, Asked { engine: Some(Engine::Host), version: 0 });
        assert_eq!((f.virt.microvm, of(&f)), (false, Some(Engine::Host)), "said again, the same");
        overlay(&mut f, Asked::default());
        assert!(f.virt.microvm, "set back to what it can do");
        let mut g = facts(Some(unable("no hypervisor")));
        g.virt.microvm = true;
        overlay(&mut g, Asked { engine: Some(Engine::Microvm), version: 0 });
        assert_eq!((g.virt.microvm, of(&g)), (false, Some(Engine::Host)));
        assert_eq!(of(&facts(None)), Some(Engine::Host), "a runner from before microVMs runs its sessions on its host");
        assert_eq!(node_images(&f), NodeImages::Unknown, "no shards-image class in the Hex kiln vendors");
    }

    /// A start placed under one setting of its runner's engine is refused by the runner once another
    /// is set, saying what it is now; one under the setting it holds, or one that names none, starts.
    #[test]
    fn a_start_placed_before_a_switch_is_refused_by_the_runner() {
        let now = Asked { engine: Some(Engine::Host), version: 3 };
        let why = stale(Some(2), now).unwrap_or_default();
        assert!(why.contains("set again (to host, setting 3)") && why.contains("placed under setting 2"), "{why}");
        assert_eq!(stale(Some(3), now), None);
        assert_eq!(stale(None, now), None, "a start that names none");
        assert!(stale(Some(0), Asked { engine: None, version: 1 }).is_some_and(|w| w.contains("what it can do")));
    }

    /// A `PUT` sets `host`, `microvm` or `null`, and nothing else.
    #[test]
    fn a_put_sets_host_microvm_or_null_and_nothing_else() {
        assert_eq!(asked_of(&json!({"engine": "host"})), Ok(Asked { engine: Some(Engine::Host), version: 0 }));
        assert_eq!(asked_of(&json!({"engine": "microvm"})), Ok(Asked { engine: Some(Engine::Microvm), version: 0 }));
        assert_eq!(asked_of(&json!({"engine": null})), Ok(Asked::default()));
        for bad in [json!({}), json!({"engin": "host"}), json!({"engine": "docker"}), json!({"engine": 1}), json!({"engine": "host", "x": 1}), json!(["host"])] {
            assert!(asked_of(&bad).is_err(), "{bad} was taken");
        }
    }

    /// What was set on the server is kept on the machine for its restarts, and goes once nothing is.
    #[test]
    fn a_runner_keeps_its_engine_for_its_restarts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(kept(dir.path()), Asked::default());
        keep(dir.path(), Asked { engine: Some(Engine::Host), version: 1 })?;
        assert_eq!(kept(dir.path()), Asked { engine: Some(Engine::Host), version: 1 });
        keep(dir.path(), Asked { engine: None, version: 2 })?;
        assert_eq!(kept(dir.path()), Asked { engine: None, version: 2 }, "nothing set, its version kept");
        std::fs::write(kept_path(dir.path()), "not json")?;
        assert_eq!(kept(dir.path()), Asked::default());
        Ok(())
    }
}
