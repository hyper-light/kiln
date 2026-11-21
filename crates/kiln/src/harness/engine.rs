//! A runner's engine: how it runs new agent sessions (kiln plan 01a1dd2e…, P2.3 and P2.4; the design's
//! part A; docs/isolation.md, "A runner's engine"). `host`: each a process of its machine's, with no
//! microVM. `microvm`: each in a Shards microVM of its own, through Hex (`crate::sandbox::hexvm`). Its
//! engine is, the first of these there is:
//!
//! 1. what its owner set on the server (Runners; `PUT /api/runners/:name/engine`, MCP and code mode's
//!    `runners.engine.set`): kept in the server's `kv` table (`runner_engine/<runner>`), sent to its
//!    runner as that names [`crate::proto::feature::RUNNER_ENGINE`] and whenever it changes
//!    (`ServerMsg::RunnerEngine`), and kept by the runner in its state folder for its restarts;
//! 2. `host`: the microVM engine is an option its owner enables, never a default (the person's ruling, 2026-11-21:
//!    "Like the microVM engine, it's designed to be an option we can enable"; it was `microvm` wherever the runner
//!    could hold microVMs, the person's decision 3 of 2026-11-14), saying why it can't hold them where it can't.
//!
//! `container` (docs/agentic.md W9): each in an OCI container of its own on its workspace's Slates volume, through the
//! machine's container engine (`crate::sandbox::container`). Only ever its owner's choice, never a default: it's refused
//! where the runner's facts say its agents can't run in containers ([`containers`]), and one set where they later can't
//! is held as `host`, saying why.
//!
//! `microvm` is refused, saying why, where the runner can't hold them; one set where it later can't is
//! held as `host`, saying why. The engine is written into the runner's facts ([`overlay`]:
//! `isolation.engine`, and `virt.microvm` only on a `microvm` one) by the runner, for its own admission
//! as an owner probes it, and by the server over what each runner says, for placement and the runners
//! API alike. So a session that needs a microVM is placed only on a `microvm` runner, and the start of
//! one that asks for none only on a `host` one ([`super::lineage`]). A switch takes nothing running away:
//! the sessions of the engine it had finish there ([`view`]'s `draining`, each listed), and new sessions take
//! the new. While they drain (ROOT's decision 8 of the design): "End them now" ends each ([`end_draining`]),
//! and the switch can be cancelled ([`cancel`]), its engine set back to what its owner had set before, so
//! they're on its engine again. What it had is kept with the setting ([`Kept::switched`]).

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
    /// Nothing set: `host`, the microVM engine being an option its owner enables (the person's ruling, 2026-11-21).
    Default,
}

/// Whether a runner can hold agents in microVMs now, and why not: the one place kiln reads that from.
///
/// From its facts as its runner says them: its kiln carries Shards, and Shards' own check of its host says it
/// runs microVMs now (`shards_api::host_check`'s `{backend, usable, why}` as `isolation`'s `hypervisor`,
/// `available` and `why_not`: `crate::sandbox::facts`).
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

/// Why a runner whose facts say nothing of containers (a runner from before them) runs its agents in none.
pub const CONTAINERS_NOT_SAID: &str = "its runner doesn't say whether its agents can run in containers: its kiln is from before them";

/// Whether a runner's agents can run in containers now, and why not, from its facts as its runner says them
/// (`containers`: `crate::workspace::container::facts`).
pub fn containers(facts: &RunnerFacts) -> Capable {
    match &facts.containers {
        Some(c) if c.available => Capable { usable: true, why: None },
        Some(c) => Capable { usable: false, why: Some(c.why_not.clone().unwrap_or_else(|| "its agents can't run in containers here".to_owned())) },
        None => Capable { usable: false, why: Some(CONTAINERS_NOT_SAID.to_owned()) },
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

/// What a runner holds: what its owner set, else `host` (the person's ruling, 2026-11-21: the microVM engine is an
/// option its owner enables), saying why it can't hold microVMs where it can't (`can`). `microvm` set where it can't any more is held as `host`, saying why, and so is `container` set where its agents
/// can't run in containers any more (`boxed`).
pub fn held(asked: Asked, can: &Capable, boxed: &Capable) -> Held {
    match (asked.engine, can.usable) {
        (Some(Engine::Microvm), false) => Held { engine: Engine::Host, from: Source::Default, why: Some(format!("its owner set microvm, but it can't hold microVMs now: {}", can.why.as_deref().unwrap_or(crate::sandbox::NO_MICROVM))) },
        (Some(Engine::Container), _) if !boxed.usable => {
            Held { engine: Engine::Host, from: Source::Default, why: Some(format!("its owner set container, but its agents can't run in containers now: {}", boxed.why.as_deref().unwrap_or(CONTAINERS_NOT_SAID))) }
        }
        (Some(engine), _) => Held { engine, from: Source::Server, why: None },
        (None, _) => Held { engine: Engine::Host, from: Source::Default, why: can.why.clone() },
    }
}

/// `facts` as a runner whose owner set `asked` holds them: its engine in `isolation.engine`, and
/// `virt.microvm` only where that's `microvm`, so placement (on the server, and by its owner probing
/// its admission) never sends it a session that needs a microVM while it runs sessions on its host.
pub fn overlay(facts: &mut RunnerFacts, asked: Asked) -> Held {
    let can = capable(facts);
    let h = held(asked, &can, &containers(facts));
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
/// facts say (`isolation.image_class`, which its runner reads from its node: `crate::sandbox::facts_of`).
/// A `microvm` runner whose node lists none is refused a session that needs a microVM, in Hex's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeImages {
    /// Its node lists the class.
    Listed,
    /// Its node lists no such class, `why` in Hex's words.
    NotListed { why: String },
    /// Its facts don't say: a runner holding no microVMs through Hex, or one from before the class,
    /// placed by its engine alone.
    Unknown,
}

/// What `facts` say of their node's image class ([`NodeImages`]).
pub fn node_images(facts: &RunnerFacts) -> NodeImages {
    match facts.isolation.as_ref().and_then(|i| i.image_class.as_ref()) {
        Some(kiln_ci::facts::NodeImageClass::Listed) => NodeImages::Listed,
        Some(kiln_ci::facts::NodeImageClass::Unlisted { why }) => NodeImages::NotListed { why: why.clone() },
        None => NodeImages::Unknown,
    }
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

/// A change of what its owner set for a runner's engine (decision 8: a switch drains, and can be
/// cancelled while it does): what was set before (`from`; none: what the runner can do), what it was
/// set to (`to`), and when (Unix ms). Kept on the server alone, with the setting ([`Kept`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Switch {
    #[serde(default)]
    pub from: Option<Engine>,
    #[serde(default)]
    pub to: Option<Engine>,
    pub at: i64,
}

/// What the server keeps for a runner's engine (`kv`, [`KEY_PREFIX`]): the setting its runner is sent
/// ([`Asked`]) and the latest switch of it. Read as an `Asked` alone ([`Engines::load`]), the switch is
/// passed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    #[serde(flatten)]
    pub asked: Asked,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switched: Option<Switch>,
}

/// What the server keeps for `runner`'s engine: none where nothing was set, or it can't be read.
async fn kept_for(hub: &Hub, runner: &str) -> Option<Kept> {
    serde_json::from_str(&hub.store.kv_get(format!("{KEY_PREFIX}{runner}")).await.ok()??).ok()
}

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

/// The engine a session runs on: a microVM where it's isolated, a container where it's in one, else its host.
pub fn session_engine(i: &crate::harness::session::Info) -> Engine {
    if i.isolation.is_some() {
        Engine::Microvm
    } else if i.container.is_some() {
        Engine::Container
    } else {
        Engine::Host
    }
}

/// Of `sessions` (a runner's), those whose harness still runs (not exited or crashed) on `engine`, and
/// those on another, which drain: they finish there.
fn split(sessions: &[crate::harness::session::Info], engine: Engine) -> (Vec<&crate::harness::session::Info>, Vec<&crate::harness::session::Info>) {
    use crate::harness::model::Status;
    sessions.iter().filter(|i| !matches!(i.status, Status::Exited | Status::Crashed)).partition(|i| session_engine(i) == engine)
}

/// The engine runner `name` holds now, as the server has its facts: none before they've come.
fn held_now(hub: &Hub, name: &str) -> Option<Held> {
    let facts = hub.agents.load().get(name).and_then(|a| a.facts.as_deref().cloned())?;
    Some(held(hub.engines.asked(name), &capable(&facts), &containers(&facts)))
}

/// The sessions of runner `name` that drain (they run on the engine it doesn't hold: [`split`]),
/// each before the agent that started it where both drain, so ending them in order ends none before
/// what it started.
fn draining_of(hub: &Hub, name: &str) -> Vec<crate::harness::session::Info> {
    let Some(h) = held_now(hub, name) else { return Vec::new() };
    let target = format!("agent:{name}");
    let sessions: Vec<_> = hub.harness.list().into_iter().filter(|i| i.target == target).collect();
    let (_, draining) = split(&sessions, h.engine);
    let mut out: Vec<crate::harness::session::Info> = Vec::with_capacity(draining.len());
    let all = hub.harness.list();
    for i in draining {
        // The agents it started that drain too come before it (the deepest first).
        let mut kids = super::lineage::descendants(&all, &i.id);
        kids.reverse();
        for k in kids {
            if !out.iter().any(|o| o.id == k.id) && k.target == target && !matches!(k.status, crate::harness::model::Status::Exited | crate::harness::model::Status::Crashed) && session_engine(&k) != h.engine {
                out.push(k);
            }
        }
        if !out.iter().any(|o| o.id == i.id) {
            out.push(i.clone());
        }
    }
    out
}

/// A draining session as the view lists it: its id and name, its harness, how it stands (as
/// `agents.list`'s `stand`), the engine it runs on and when it was made (Unix ms).
fn drained(i: &crate::harness::session::Info) -> Value {
    json!({"id": i.id, "name": i.name, "kind": i.kind, "state": super::wait::stand(i), "engine": session_engine(i), "created_at": i.created_at})
}

/// `GET /api/runners/:name/engine` (and what `PUT` answers): the engine it holds, where that comes
/// from and why, what its owner set, whether it can hold microVMs now and why not, whether its agents can run in
/// containers now and why not (`container`, with its container facts, `containers`), its isolation facts,
/// and its sessions on each engine (`draining`: how many of the other engine, which finish there, and
/// `draining_sessions`, each of them); with the switch they drain for (`switch`: what was set before,
/// what it was set to, and when), while any drains.
pub async fn view(hub: &Hub, name: &str) -> Value {
    let asked = hub.engines.asked(name);
    let facts = hub.agents.load().get(name).and_then(|a| a.facts.as_deref().cloned());
    let (held, can, boxed) = match &facts {
        Some(f) => {
            let (can, boxed) = (capable(f), containers(f));
            (Some(held(asked, &can, &boxed)), Some(can), Some(boxed))
        }
        None => (None, None, None),
    };
    let target = format!("agent:{name}");
    let sessions: Vec<_> = hub.harness.list().into_iter().filter(|i| i.target == target).collect();
    let (on, draining) = held.as_ref().map_or((0, 0), |h| {
        let (on, other) = split(&sessions, h.engine);
        (on.len(), other.len())
    });
    let listed: Vec<Value> = if draining == 0 { Vec::new() } else { draining_of(hub, name).iter().map(drained).collect() };
    let switch = if draining == 0 { None } else { kept_for(hub, name).await.and_then(|k| k.switched).filter(|s| s.to == asked.engine) };
    json!({
        "runner": name,
        "engine": held.as_ref().map(|h| h.engine),
        "from": held.as_ref().map(|h| h.from),
        "why": held.as_ref().and_then(|h| h.why.clone()),
        "asked": asked.engine,
        "version": asked.version,
        "microvm": can.as_ref().map(|c| json!({"usable": c.usable, "why": c.why})),
        "container": boxed.as_ref().map(|c| json!({"usable": c.usable, "why": c.why})),
        "containers": facts.as_ref().and_then(|f| f.containers.clone()),
        "isolation": facts.as_ref().and_then(|f| f.isolation.clone()),
        "virt": facts.as_ref().map(|f| json!({"microvm": f.virt.microvm, "kvm": f.virt.kvm, "hvf": f.virt.hvf})),
        "sessions": on,
        "draining": draining,
        "draining_sessions": listed,
        "switch": switch,
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

/// What a `PUT` asks for: `{"engine": "host" | "microvm" | "container"}`, or `{"engine": null}` for what the runner
/// can do.
pub fn asked_of(body: &Value) -> Result<Asked, String> {
    const SHAPE: &str = "expected {\"engine\": \"host\"}, {\"engine\": \"microvm\"} or {\"engine\": \"container\"}, or {\"engine\": null} for what the runner can do";
    let o = body.as_object().ok_or(SHAPE)?;
    if let Some(k) = o.keys().find(|k| *k != "engine") {
        return Err(format!("{k:?} isn't one: {SHAPE}"));
    }
    match o.get("engine").ok_or(SHAPE)? {
        Value::Null => Ok(Asked::default()),
        Value::String(s) => match s.trim() {
            "host" => Ok(Asked { engine: Some(Engine::Host), version: 0 }),
            "microvm" => Ok(Asked { engine: Some(Engine::Microvm), version: 0 }),
            "container" => Ok(Asked { engine: Some(Engine::Container), version: 0 }),
            other => Err(format!("a runner's engine is \"host\", \"microvm\" or \"container\", not {other:?}")),
        },
        v => Err(format!("a runner's engine is \"host\", \"microvm\" or \"container\", not {v}")),
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
    // `container` only where its runner says its agents can run in containers now (docs/agentic.md W9).
    if asked.engine == Some(Engine::Container) {
        let facts = hub.agents.load().get(name).and_then(|a| a.facts.as_deref().cloned());
        match facts.as_ref().map(containers) {
            Some(Capable { usable: true, .. }) => {}
            Some(Capable { why, .. }) => return Err(fail(StatusCode::CONFLICT, format!("{} can't run its sessions in containers: {}", hub.shown(name), why.as_deref().unwrap_or(CONTAINERS_NOT_SAID)))),
            None => return Err(fail(StatusCode::CONFLICT, format!("{} hasn't said yet whether its agents can run in containers: set its engine once it has", hub.shown(name)))),
        }
    }
    // The one before's plus one: a start decided under the setting before is refused by the runner.
    let before = hub.engines.asked(name);
    let version = before.version.checked_add(1).ok_or_else(|| fail(StatusCode::CONFLICT, format!("{name}'s engine was set as many times as a setting's version counts")))?;
    let asked = Asked { version, ..asked };
    // A change of it is a switch, kept for its drain to be cancelled; the same set again keeps the one before.
    let switched = if asked.engine == before.engine { kept_for(hub, name).await.and_then(|k| k.switched) } else { Some(Switch { from: before.engine, to: asked.engine, at: crate::util::now_ms() }) };
    let json = serde_json::to_string(&Kept { asked, switched }).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    hub.store.kv_set(format!("{KEY_PREFIX}{name}"), json).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.engines.inner.update(|g| {
        g.insert(name.to_owned(), asked);
    });
    let said = asked.engine.map_or_else(|| "nothing set here: what it can do".to_owned(), |e| e.to_string());
    hub.store.audit(by.to_owned(), "runner_engine.update".into(), format!("{name}: {said}")).await;
    push(hub, name).await;
    Ok(view(hub, name).await)
}

/// "End them now" (decision 8): runner `name`'s sessions that drain ([`draining_of`]) ended now, each
/// after the agents it started, rather than waiting for each to finish on the engine it had, as `by`
/// asks. The view after it, with those it ended (`ended`): each its runner closed, or kiln forgot as its
/// runner couldn't say (it drops them as it comes back). One its runner keeps (its archive or restore
/// under way) stays among those draining.
pub async fn end_draining(hub: &Hub, name: &str, by: &str) -> Result<Value, (StatusCode, String)> {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    let mut ended: Vec<String> = Vec::new();
    for s in draining_of(hub, name) {
        super::about::ended(&s.id);
        if super::ops::end(hub, &s.id, by).await || hub.harness.info(&s.id).is_none() {
            ended.push(s.id);
        }
    }
    hub.store.audit(by.to_owned(), "runner_engine.end_draining".into(), format!("{name}: {} ended ({})", ended.len(), ended.join(", "))).await;
    let mut v = view(hub, name).await;
    if let Some(o) = v.as_object_mut() {
        o.insert("ended".into(), json!(ended));
    }
    Ok(v)
}

/// A switch of runner `name`'s engine cancelled while what it drains still runs (decision 8): its engine
/// set back to what its owner had set before the switch, as `by` asks, so those sessions are on its
/// engine again (a setting of its own, its version the next). Refused where it wasn't switched, or
/// nothing drains (the switch is done: set its engine instead). The view after it.
pub async fn cancel(hub: &Hub, name: &str, by: &str) -> Result<Value, (StatusCode, String)> {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    let asked = hub.engines.asked(name);
    let Some(s) = kept_for(hub, name).await.and_then(|k| k.switched).filter(|s| s.to == asked.engine) else {
        return Err(fail(StatusCode::CONFLICT, format!("{}'s engine wasn't switched: set it instead", hub.shown(name))));
    };
    if draining_of(hub, name).is_empty() {
        let to = s.to.map_or_else(|| "what it can do".to_owned(), |e| e.to_string());
        return Err(fail(StatusCode::CONFLICT, format!("nothing of {} drains: its switch to {to} is done, so set its engine instead", hub.shown(name))));
    }
    set(hub, name, &json!({"engine": s.from}), by).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::facts::Isolation;

    fn facts(isolation: Option<Isolation>) -> RunnerFacts {
        RunnerFacts { isolation, ..RunnerFacts::default() }
    }

    fn able() -> Isolation {
        Isolation { available: true, hypervisor: Some("Hypervisor.framework".into()), why_not: None, engine: None, image_class: None }
    }

    fn unable(why: &str) -> Isolation {
        Isolation { available: false, hypervisor: None, why_not: Some(why.into()), engine: None, image_class: None }
    }

    /// P2.3 AC2, as the person ruled on 2026-11-21 (the microVM engine is an option its owner enables): a runner
    /// that can hold microVMs holds `host` until its owner sets `microvm`; one that can't holds `host`, its facts' why
    /// beside it. What its owner set wins, but `microvm` set where it can't any more is held as `host`, saying why.
    #[test]
    fn p2_3_ac2_the_default_engine_is_host_and_microvm_is_its_owners_to_set() {
        let set = |e| Asked { engine: Some(e), version: 1 };
        let yes = capable(&facts(Some(able())));
        let no = capable(&facts(Some(unable("no hypervisor"))));
        let boxed = containers(&facts(None));
        assert_eq!(held(Asked::default(), &yes, &boxed), Held { engine: Engine::Host, from: Source::Default, why: None }, "microVMs are opt-in");
        assert_eq!(held(set(Engine::Microvm), &yes, &boxed), Held { engine: Engine::Microvm, from: Source::Server, why: None });
        assert_eq!(held(Asked::default(), &no, &boxed), Held { engine: Engine::Host, from: Source::Default, why: Some("no hypervisor".into()) });
        assert_eq!(held(set(Engine::Host), &yes, &boxed), Held { engine: Engine::Host, from: Source::Server, why: None });
        assert_eq!(held(set(Engine::Host), &no, &boxed).engine, Engine::Host);
        let lost = held(set(Engine::Microvm), &no, &boxed);
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
        assert_eq!((f.virt.microvm, of(&f)), (false, Some(Engine::Host)), "microVMs are opt-in");
        overlay(&mut f, Asked { engine: Some(Engine::Microvm), version: 0 });
        assert_eq!((f.virt.microvm, of(&f)), (true, Some(Engine::Microvm)));
        overlay(&mut f, Asked { engine: Some(Engine::Host), version: 0 });
        assert_eq!((f.virt.microvm, of(&f)), (false, Some(Engine::Host)));
        overlay(&mut f, Asked { engine: Some(Engine::Host), version: 0 });
        assert_eq!((f.virt.microvm, of(&f)), (false, Some(Engine::Host)), "said again, the same");
        overlay(&mut f, Asked { engine: Some(Engine::Microvm), version: 0 });
        assert!(f.virt.microvm, "set back to microvm");
        let mut g = facts(Some(unable("no hypervisor")));
        g.virt.microvm = true;
        overlay(&mut g, Asked { engine: Some(Engine::Microvm), version: 0 });
        assert_eq!((g.virt.microvm, of(&g)), (false, Some(Engine::Host)));
        assert_eq!(of(&facts(None)), Some(Engine::Host), "a runner from before microVMs runs its sessions on its host");
        assert_eq!(node_images(&f), NodeImages::Unknown, "facts that say nothing of the node's image class");
    }

    /// P2.4 (on Hex's P2.2): placement reads whether a runner's Hex node lists the class image VMs run as from what its
    /// runner said of its node (`isolation.image_class`). Do read facts saying it's listed, that it isn't and why, that
    /// say nothing of it, and none at all. Expect it listed; not listed, in Hex's words; and unknown for the last two,
    /// whose runners are placed by their engine alone.
    #[test]
    fn p2_4_node_images_is_what_the_runner_said_of_its_hex_node() {
        use kiln_ci::facts::NodeImageClass;
        let said = |class: Option<NodeImageClass>| facts(Some(Isolation { image_class: class, ..able() }));
        assert_eq!(node_images(&said(Some(NodeImageClass::Listed))), NodeImages::Listed);
        let why = "Shards image VMs on linux: cannot open /dev/kvm: permission denied";
        assert_eq!(node_images(&said(Some(NodeImageClass::Unlisted { why: why.into() }))), NodeImages::NotListed { why: why.into() });
        assert_eq!(node_images(&said(None)), NodeImages::Unknown);
        assert_eq!(node_images(&facts(None)), NodeImages::Unknown, "a runner from before microVMs");
    }

    /// W9: `container` is only ever its owner's choice. Given a runner whose agents can run in containers and one whose
    /// can't (and one from before containers), each able to hold microVMs or not. Expect none to hold `container` unasked;
    /// `container` set held where its agents can, and held as `host`, saying why, where they can't; and the facts placement
    /// reads saying so, with no microVM offered on a `container` runner.
    #[test]
    fn w9_a_runner_holds_container_only_where_its_owner_set_it_and_its_agents_can_run_in_containers() {
        use kiln_ci::facts::{ContainerFacts, ContainerMissing};
        let set = Asked { engine: Some(Engine::Container), version: 1 };
        let boxes = |available: bool| RunnerFacts {
            containers: Some(ContainerFacts {
                available,
                why_not: (!available).then(|| "no container engine answers here".to_owned()),
                missing: (!available).then_some(ContainerMissing::Engine),
                ..ContainerFacts::default()
            }),
            ..facts(Some(able()))
        };
        let (can, cant) = (boxes(true), boxes(false));
        assert_eq!(containers(&can), Capable { usable: true, why: None });
        assert_eq!(containers(&facts(None)), Capable { usable: false, why: Some(CONTAINERS_NOT_SAID.into()) });
        for f in [&can, &cant] {
            assert_eq!(held(Asked::default(), &capable(f), &containers(f)).engine, Engine::Host, "never container unasked");
        }
        assert_eq!(held(set, &capable(&can), &containers(&can)), Held { engine: Engine::Container, from: Source::Server, why: None });
        let lost = held(set, &capable(&cant), &containers(&cant));
        assert_eq!((lost.engine, lost.from), (Engine::Host, Source::Default));
        assert!(lost.why.as_deref().is_some_and(|w| w.contains("its owner set container") && w.contains("no container engine answers here")), "{lost:?}");
        let mut f = can.clone();
        overlay(&mut f, set);
        assert_eq!((f.virt.microvm, of(&f)), (false, Some(Engine::Container)), "a container runner offers no microVM");
        let mut g = cant.clone();
        overlay(&mut g, set);
        assert_eq!(of(&g), Some(Engine::Host));
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

    /// A `PUT` sets `host`, `microvm`, `container` or `null`, and nothing else.
    #[test]
    fn a_put_sets_host_microvm_container_or_null_and_nothing_else() {
        assert_eq!(asked_of(&json!({"engine": "host"})), Ok(Asked { engine: Some(Engine::Host), version: 0 }));
        assert_eq!(asked_of(&json!({"engine": "microvm"})), Ok(Asked { engine: Some(Engine::Microvm), version: 0 }));
        assert_eq!(asked_of(&json!({"engine": "container"})), Ok(Asked { engine: Some(Engine::Container), version: 0 }));
        assert_eq!(asked_of(&json!({"engine": null})), Ok(Asked::default()));
        for bad in [json!({}), json!({"engin": "host"}), json!({"engine": "docker"}), json!({"engine": 1}), json!({"engine": "host", "x": 1}), json!(["host"])] {
            assert!(asked_of(&bad).is_err(), "{bad} was taken");
        }
    }

    /// Decision 8 of the design (ROOT's rulings: "Draining waits for each session to end, with an explicit End them
    /// now press"), through the operations as REST, MCP and code mode call them, on a runner that holds microVMs
    /// (`linux`) whose owner set `microvm`, running two sessions in them, one started by the other: nothing drains, so `cancel` is refused and
    /// "End them now" ends nothing. Switched to `host`, both drain, each listed (`draining_sessions`, the child first)
    /// with the switch they drain for; cancelled, its engine is what its owner had set before (`microvm`), a setting of
    /// its own, and nothing drains. Switched again, "End them now" ends both, the child before its parent, and as
    /// their runner says each closed, nothing drains and no session is left. A runner kiln doesn't know is 404 to
    /// both. Taking the switch out of what `set` keeps refuses the cancel; ending them parent first fails the order.
    #[test]
    fn d8_a_switch_lists_what_drains_ends_it_on_ask_and_is_cancelled_while_it_drains() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::registry::Location;
        use crate::harness::session::{Info, Isolation as Boundary};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            crate::hub::tests::join_runner(&hub, 2, "linux", crate::proto::CAP_WIRE);
            let mut f = facts(Some(able()));
            overlay(&mut f, Asked::default());
            hub.put_agent(crate::hub::AgentLive { facts: Some(Box::new(f)), ..crate::hub::tests::live_runner("linux", "linux", "x86_64") });
            let there = Location::Machine("linux".into());
            for (id, parent) in [("vm-lead", None), ("vm-child", Some("vm-lead"))] {
                let info = Info { id: id.into(), kind: "claude".into(), name: id.into(), target: "agent:linux".into(), status: crate::harness::model::Status::Idle, isolation: Some(Boundary::microvm()), parent: parent.map(str::to_owned), ..Default::default() };
                hub.harness.ingest(&there, id, Update::Open { info: Box::new(info), since: None }).await;
            }
            let op = |name: &'static str, input: Value| crate::ops::call(crate::ops::Cx { hub: std::sync::Arc::clone(&hub), caller: crate::authz::Caller::owner() }, name, input);
            let ids = |v: &Value, k: &str| -> Vec<String> { v.get(k).and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.get("id").or(Some(s)).and_then(Value::as_str)).map(str::to_owned).collect() };
            let linux = json!({"runner": "linux"});
            // Never switched: nothing to cancel.
            let refused = op("runners.engine.cancel", linux.clone()).await.err().ok_or_else(|| anyhow::anyhow!("a cancel with no switch went"))?;
            anyhow::ensure!(refused.0 == StatusCode::CONFLICT && refused.1.contains("wasn't switched"), "{refused:?}");
            // Its owner's `microvm` (the default is `host`: the person's ruling, 2026-11-21).
            op("runners.engine.set", json!({"runner": "linux", "engine": "microvm"})).await.map_err(|(c, m)| anyhow::anyhow!("{c} {m}"))?;

            // Nothing drains: the owner's switch to `microvm` is done, so there's nothing to cancel or end.
            let v = op("runners.engine", linux.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c} {m}"))?;
            anyhow::ensure!(v.get("engine") == Some(&json!("microvm")) && v.get("sessions") == Some(&json!(2)) && v.get("draining") == Some(&json!(0)) && v.get("draining_sessions") == Some(&json!([])) && v.get("switch") == Some(&Value::Null), "{v}");
            let refused = op("runners.engine.cancel", linux.clone()).await.err().ok_or_else(|| anyhow::anyhow!("a cancel with nothing draining went"))?;
            anyhow::ensure!(refused.0 == StatusCode::CONFLICT && refused.1.contains("nothing of"), "{refused:?}");
            let v = op("runners.engine.end_draining", linux.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c} {m}"))?;
            anyhow::ensure!(v.get("ended") == Some(&json!([])) && v.get("sessions") == Some(&json!(2)), "nothing ended: {v}");

            // Switched to host: both drain, listed, the agent it started before the one that started it.
            let v = op("runners.engine.set", json!({"runner": "linux", "engine": "host"})).await.map_err(|(c, m)| anyhow::anyhow!("{c} {m}"))?;
            anyhow::ensure!(v.get("engine") == Some(&json!("host")) && v.get("draining") == Some(&json!(2)) && ids(&v, "draining_sessions") == ["vm-child", "vm-lead"], "{v}");
            anyhow::ensure!(v.pointer("/draining_sessions/0/engine") == Some(&json!("microvm")) && v.pointer("/switch/from") == Some(&json!("microvm")) && v.pointer("/switch/to") == Some(&json!("host")) && v.pointer("/switch/at").is_some_and(Value::is_i64), "{v}");

            // Cancelled: its engine as it was, a setting of its own, and nothing drains.
            let v = op("runners.engine.cancel", linux.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c} {m}"))?;
            anyhow::ensure!(v.get("engine") == Some(&json!("microvm")) && v.get("from") == Some(&json!("server")) && v.get("asked") == Some(&json!("microvm")) && v.get("version") == Some(&json!(3)), "{v}");
            anyhow::ensure!(v.get("draining") == Some(&json!(0)) && v.get("sessions") == Some(&json!(2)) && v.get("switch") == Some(&Value::Null), "{v}");
            let refused = op("runners.engine.cancel", linux.clone()).await.err().ok_or_else(|| anyhow::anyhow!("a second cancel went"))?;
            anyhow::ensure!(refused.0 == StatusCode::CONFLICT && refused.1.contains("nothing of"), "{refused:?}");

            // Switched again, then "End them now": both ended, the child first; as their runner says each closed,
            // nothing drains and none is left.
            op("runners.engine.set", json!({"runner": "linux", "engine": "host"})).await.map_err(|(c, m)| anyhow::anyhow!("{c} {m}"))?;
            let v = op("runners.engine.end_draining", linux.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c} {m}"))?;
            anyhow::ensure!(v.get("ended") == Some(&json!(["vm-child", "vm-lead"])), "{v}");
            for id in ["vm-child", "vm-lead"] {
                hub.harness.ingest(&there, id, Update::Closed).await;
            }
            let v = op("runners.engine", linux.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c} {m}"))?;
            anyhow::ensure!(v.get("draining") == Some(&json!(0)) && v.get("sessions") == Some(&json!(0)) && v.get("draining_sessions") == Some(&json!([])), "{v}");

            for name in ["runners.engine.end_draining", "runners.engine.cancel"] {
                let refused = op(name, json!({"runner": "nobody"})).await.err().ok_or_else(|| anyhow::anyhow!("{name} on no runner went"))?;
                anyhow::ensure!(refused.0 == StatusCode::NOT_FOUND, "{name}: {refused:?}");
            }
            anyhow::Ok(())
        })
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
